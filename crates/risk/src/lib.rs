//! Pre-trade risk. Every intent passes here before the executor sees it.

use arb_core::{Intent, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_notional_quote_per_trade: Decimal,
    pub max_notional_quote_per_venue: Decimal,
    pub min_edge_bps: Decimal,
    pub max_book_age_ms: i64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_notional_quote_per_trade: Decimal::new(5_000, 0),
            max_notional_quote_per_venue: Decimal::new(25_000, 0),
            min_edge_bps: Decimal::new(15, 0),
            max_book_age_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Reject {
    #[error("kill-switch engaged")]
    Killed,
    #[error("venue paused: {0}")]
    VenuePaused(Venue),
    #[error("edge {edge}bps below minimum {min}bps")]
    EdgeTooSmall { edge: Decimal, min: Decimal },
    #[error("notional {notional} exceeds per-trade cap {cap}")]
    NotionalTooBig { notional: Decimal, cap: Decimal },
    #[error("venue {venue} exposure {exposure} exceeds cap {cap}")]
    VenueExposure {
        venue: Venue,
        exposure: Decimal,
        cap: Decimal,
    },
    #[error("intent expired (age {age_ms}ms > ttl {ttl_ms}ms)")]
    Expired { age_ms: i64, ttl_ms: i64 },
    #[error("empty intent")]
    Empty,
}

pub struct RiskEngine {
    pub limits: RiskLimits,
    killed: Arc<AtomicBool>,
    paused: Vec<Venue>,
    venue_exposure: BTreeMap<Venue, Decimal>,
}

impl RiskEngine {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            killed: Arc::new(AtomicBool::new(false)),
            paused: Vec::new(),
            venue_exposure: BTreeMap::new(),
        }
    }

    /// Shared handle so an API/signal task can trip the switch from another thread.
    pub fn kill_handle(&self) -> Arc<AtomicBool> {
        self.killed.clone()
    }
    pub fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
    }
    pub fn revive(&self) {
        self.killed.store(false, Ordering::SeqCst);
    }
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    pub fn pause_venue(&mut self, v: Venue) {
        if !self.paused.contains(&v) {
            self.paused.push(v);
        }
    }
    pub fn resume_venue(&mut self, v: Venue) {
        self.paused.retain(|x| *x != v);
    }

    pub fn check(&self, intent: &Intent, now_ms: i64) -> Result<(), Reject> {
        if self.is_killed() {
            return Err(Reject::Killed);
        }
        if intent.legs.is_empty() {
            return Err(Reject::Empty);
        }
        let age = now_ms - intent.created_ms;
        if age > intent.ttl_ms {
            return Err(Reject::Expired {
                age_ms: age,
                ttl_ms: intent.ttl_ms,
            });
        }
        if intent.expected_edge_bps < self.limits.min_edge_bps {
            return Err(Reject::EdgeTooSmall {
                edge: intent.expected_edge_bps,
                min: self.limits.min_edge_bps,
            });
        }
        let mut per_venue: BTreeMap<Venue, Decimal> = BTreeMap::new();
        for leg in &intent.legs {
            if self.paused.contains(&leg.venue) {
                return Err(Reject::VenuePaused(leg.venue));
            }
            let n = leg.notional_quote();
            if n > self.limits.max_notional_quote_per_trade {
                return Err(Reject::NotionalTooBig {
                    notional: n,
                    cap: self.limits.max_notional_quote_per_trade,
                });
            }
            *per_venue.entry(leg.venue).or_default() += n;
        }
        for (venue, add) in per_venue {
            let exposure = self.venue_exposure.get(&venue).copied().unwrap_or_default() + add;
            if exposure > self.limits.max_notional_quote_per_venue {
                return Err(Reject::VenueExposure {
                    venue,
                    exposure,
                    cap: self.limits.max_notional_quote_per_venue,
                });
            }
        }
        Ok(())
    }

    /// Settle an executed intent into venue exposure. Round-trip intents close
    /// immediately, so they hold no exposure once filled; position-opening
    /// intents (funding entries) accumulate until released on unwind.
    /// The engine MUST call this after every successful fill — `check` alone
    /// cannot enforce per-venue caps.
    pub fn settle(&mut self, intent: &Intent) {
        if !intent.roundtrip {
            self.record(intent);
        }
    }

    /// Current open exposure on a venue (quote notional).
    pub fn exposure(&self, venue: Venue) -> Decimal {
        self.venue_exposure.get(&venue).copied().unwrap_or_default()
    }

    /// Record executed notional so venue caps accumulate. Call on unwind with negatives via `release`.
    pub fn record(&mut self, intent: &Intent) {
        for leg in &intent.legs {
            *self.venue_exposure.entry(leg.venue).or_default() += leg.notional_quote();
        }
    }

    pub fn release(&mut self, venue: Venue, notional: Decimal) {
        let e = self.venue_exposure.entry(venue).or_default();
        *e = (*e - notional).max(Decimal::ZERO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{MarketKind, Side, Symbol, TradeLeg};
    use rust_decimal_macros::dec;

    fn intent(notional_px: Decimal, qty: Decimal, edge: Decimal, created: i64) -> Intent {
        Intent {
            strategy: "test".into(),
            expected_edge_bps: edge,
            ttl_ms: 1_000,
            created_ms: created,
            roundtrip: true,
            legs: vec![TradeLeg {
                venue: Venue::Binance,
                symbol: Symbol::new("BTC", "USDT"),
                side: Side::Buy,
                price: notional_px,
                qty_base: qty,
                market: MarketKind::Spot,
            }],
        }
    }

    #[test]
    fn accepts_good_and_rejects_bad() {
        let mut r = RiskEngine::new(RiskLimits::default());
        assert!(r
            .check(&intent(dec!(100), dec!(1), dec!(30), 0), 100)
            .is_ok());
        assert!(matches!(
            r.check(&intent(dec!(100), dec!(1), dec!(5), 0), 100),
            Err(Reject::EdgeTooSmall { .. })
        ));
        assert!(matches!(
            r.check(&intent(dec!(100000), dec!(1), dec!(30), 0), 100),
            Err(Reject::NotionalTooBig { .. })
        ));
        assert!(matches!(
            r.check(&intent(dec!(100), dec!(1), dec!(30), 0), 5_000),
            Err(Reject::Expired { .. })
        ));
        r.pause_venue(Venue::Binance);
        assert!(matches!(
            r.check(&intent(dec!(100), dec!(1), dec!(30), 0), 100),
            Err(Reject::VenuePaused(_))
        ));
        r.resume_venue(Venue::Binance);
        r.kill();
        assert!(matches!(
            r.check(&intent(dec!(100), dec!(1), dec!(30), 0), 100),
            Err(Reject::Killed)
        ));
    }

    #[test]
    fn venue_exposure_accumulates() {
        let limits = RiskLimits {
            max_notional_quote_per_venue: dec!(150),
            ..Default::default()
        };
        let mut r = RiskEngine::new(limits);
        let i = intent(dec!(100), dec!(1), dec!(30), 0);
        r.record(&i);
        assert!(matches!(r.check(&i, 10), Err(Reject::VenueExposure { .. })));
        r.release(Venue::Binance, dec!(100));
        assert!(r.check(&i, 10).is_ok());
    }

    #[test]
    fn settle_only_holds_open_positions() {
        // Regression: the engine never settled fills, so caps never bound.
        // settle() is the engine's contract: round-trips free their capital
        // at fill time; open positions keep consuming the venue cap.
        let mut r = RiskEngine::new(RiskLimits::default());
        let rt = intent(dec!(100), dec!(1), dec!(30), 0);
        r.settle(&rt);
        assert_eq!(r.exposure(Venue::Binance), dec!(0));
        let mut open = intent(dec!(100), dec!(1), dec!(30), 0);
        open.roundtrip = false;
        r.settle(&open);
        assert_eq!(r.exposure(Venue::Binance), dec!(100));
        r.settle(&open);
        assert_eq!(r.exposure(Venue::Binance), dec!(200));
    }
}
