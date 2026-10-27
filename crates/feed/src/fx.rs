//! Live USDT/IDR rate selection for normalizing Indodax IDR books.
//!
//! The divisor must be a USDT/IDR rate, not cash USD/IDR: the engine
//! normalizes IDR books into `/USDT` terms, and USDT trades at its own
//! premium/discount to cash dollars. Indodax's own USDT/IDR book is the
//! primary (executable, same venue). Independent USDT/IDR samples — the
//! Upbit Indonesia book and crosses implied by `X/IDR` vs `X/USDT` books —
//! form a consensus median used only as a sanity veto: if the primary
//! deviates too far, Indodax books are withheld rather than converted at
//! a blended rate that exists on no venue. Independent samples are never
//! used as the divisor themselves; an implied cross used as its own
//! divisor would erase exactly the dislocation the engine trades on.

use arb_core::OrderBook;
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// A validated USDT/IDR quote from one book.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsdtIdrRate {
    pub bid: Decimal,
    pub ask: Decimal,
    pub mid: Decimal,
    pub spread_bps: Decimal,
    pub ts_ms: i64,
}

impl UsdtIdrRate {
    /// Validate a USDT/IDR book into a rate. Rejects one-sided, crossed,
    /// non-positive, stale, or excessively wide books.
    pub fn from_book(
        book: &OrderBook,
        now_ms: i64,
        max_age_ms: i64,
        max_spread_bps: Decimal,
    ) -> Option<Self> {
        let (bid, ask) = (book.best_bid()?.price, book.best_ask()?.price);
        Self::from_bid_ask(bid, ask, book.ts_ms, now_ms, max_age_ms, max_spread_bps)
    }

    pub fn from_bid_ask(
        bid: Decimal,
        ask: Decimal,
        ts_ms: i64,
        now_ms: i64,
        max_age_ms: i64,
        max_spread_bps: Decimal,
    ) -> Option<Self> {
        if bid <= Decimal::ZERO || ask <= bid || now_ms - ts_ms > max_age_ms {
            return None;
        }
        let mid = (bid + ask) / Decimal::new(2, 0);
        let spread_bps = (ask - bid) / mid * Decimal::new(10_000, 0);
        if spread_bps > max_spread_bps {
            return None;
        }
        Some(Self {
            bid,
            ask,
            mid,
            spread_bps,
            ts_ms,
        })
    }
}

/// One unvalidated bid/ask sample from an independent USDT/IDR source.
#[derive(Debug, Clone, Copy)]
pub struct FxSample {
    pub bid: Decimal,
    pub ask: Decimal,
    pub ts_ms: i64,
}

/// Latest independent USDT/IDR sample (Upbit Indonesia), written by its
/// stream task and read by the engine tick loop.
pub type FxCrossCheck = Arc<Mutex<Option<FxSample>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateState {
    /// Primary accepted, cross-checked against at least one independent sample.
    Ok,
    /// Primary accepted but no independent sample was fresh: single-source.
    Unchecked,
    /// No usable primary book (missing, stale, crossed, or too wide).
    MissingPrimary,
    /// Primary deviates from the independent consensus beyond the limit.
    DeviationTooWide,
}

impl RateState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unchecked => "unchecked",
            Self::MissingPrimary => "missing_primary",
            Self::DeviationTooWide => "deviation_too_wide",
        }
    }
}

/// Outcome of one rate selection. `rate` is `None` whenever Indodax books
/// must be withheld; there is deliberately no fallback divisor.
#[derive(Debug, Clone, Copy)]
pub struct RateDecision {
    pub rate: Option<UsdtIdrRate>,
    pub consensus_mid: Option<Decimal>,
    pub deviation_bps: Option<Decimal>,
    pub independent_sources: usize,
    pub state: RateState,
}

impl Default for RateDecision {
    fn default() -> Self {
        Self {
            rate: None,
            consensus_mid: None,
            deviation_bps: None,
            independent_sources: 0,
            state: RateState::MissingPrimary,
        }
    }
}

impl RateDecision {
    pub fn degraded(&self) -> bool {
        self.state != RateState::Ok
    }
}

/// Median of a set of samples (`None` when empty).
pub fn median_decimals(mut xs: Vec<Decimal>) -> Option<Decimal> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_unstable();
    let n = xs.len();
    if n % 2 == 1 {
        Some(xs[n / 2])
    } else {
        Some((xs[n / 2 - 1] + xs[n / 2]) / Decimal::new(2, 0))
    }
}

/// USDT/IDR implied by one asset quoted in both IDR and USDT
/// (`X/IDR` mid / `X/USDT` mid). Validation input only — see module docs.
pub fn implied_usdt_idr(idr_book: &OrderBook, usdt_book: &OrderBook) -> Option<Decimal> {
    let mid = |b: &OrderBook| {
        let (bid, ask) = (b.best_bid()?.price, b.best_ask()?.price);
        if bid <= Decimal::ZERO || ask <= bid {
            return None;
        }
        Some((bid + ask) / Decimal::new(2, 0))
    };
    let usdt_mid = mid(usdt_book)?;
    Some(mid(idr_book)? / usdt_mid)
}

/// Select the divisor for one tick from the primary Indodax book plus
/// already-fresh independent USDT/IDR mids (Upbit book, implied crosses).
#[allow(clippy::too_many_arguments)]
pub fn resolve_usdt_idr(
    primary: Option<&OrderBook>,
    independent_mids: &[Decimal],
    now_ms: i64,
    max_age_ms: i64,
    max_spread_bps: Decimal,
    max_deviation_bps: Decimal,
) -> RateDecision {
    let consensus_mid = median_decimals(independent_mids.to_vec());
    let base = RateDecision {
        consensus_mid,
        independent_sources: independent_mids.len(),
        ..Default::default()
    };
    let Some(rate) =
        primary.and_then(|b| UsdtIdrRate::from_book(b, now_ms, max_age_ms, max_spread_bps))
    else {
        return RateDecision {
            state: RateState::MissingPrimary,
            ..base
        };
    };
    let Some(consensus) = consensus_mid else {
        return RateDecision {
            rate: Some(rate),
            state: RateState::Unchecked,
            ..base
        };
    };
    let deviation_bps = (rate.mid - consensus).abs() / consensus * Decimal::new(10_000, 0);
    if deviation_bps > max_deviation_bps {
        return RateDecision {
            deviation_bps: Some(deviation_bps),
            state: RateState::DeviationTooWide,
            ..base
        };
    }
    RateDecision {
        rate: Some(rate),
        deviation_bps: Some(deviation_bps),
        state: RateState::Ok,
        ..base
    }
}

// ---------- Upbit Indonesia (independent USDT/IDR source) ----------

pub const UPBIT_ID_WS_URL: &str = "wss://id-api.upbit.com/websocket/v1";

pub fn upbit_subscribe_frame() -> Value {
    serde_json::json!([
        {"ticket": "arb-fx-crosscheck"},
        {"type": "orderbook", "codes": ["IDR-USDT"]}
    ])
}

/// Parse one Upbit orderbook frame into a top-of-book sample. The stream
/// also carries connection notices, which return `None`.
pub fn parse_upbit_orderbook(raw: &str) -> Option<FxSample> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v["type"].as_str()? != "orderbook" || v["code"].as_str()? != "IDR-USDT" {
        return None;
    }
    let unit = &v["orderbook_units"][0];
    let num = |x: &Value| x.to_string().parse::<Decimal>().ok();
    Some(FxSample {
        bid: num(&unit["bid_price"])?,
        ask: num(&unit["ask_price"])?,
        ts_ms: v["timestamp"].as_i64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, Symbol, Venue};
    use rust_decimal_macros::dec;

    const NOW: i64 = 100_000;
    const MAX_AGE: i64 = 30_000;

    fn book(bid: Decimal, ask: Decimal, ts: i64) -> OrderBook {
        let mut b = OrderBook::new(Venue::Indodax, Symbol::new("USDT", "IDR"));
        b.apply_snapshot(
            vec![BookLevel {
                price: bid,
                qty: dec!(1000),
            }],
            vec![BookLevel {
                price: ask,
                qty: dec!(1000),
            }],
            1,
            ts,
        );
        b
    }

    fn resolve(primary: Option<&OrderBook>, mids: &[Decimal]) -> RateDecision {
        resolve_usdt_idr(primary, mids, NOW, MAX_AGE, dec!(50), dec!(300))
    }

    #[test]
    fn valid_book_yields_midpoint_rate() {
        let b = book(dec!(18111), dec!(18112), NOW - 1_000);
        let r = UsdtIdrRate::from_book(&b, NOW, MAX_AGE, dec!(50)).unwrap();
        assert_eq!(r.mid, dec!(18111.5));
        assert!(r.spread_bps < dec!(1), "spread {}", r.spread_bps);
    }

    #[test]
    fn crossed_stale_and_wide_books_are_rejected() {
        let crossed = book(dec!(18112), dec!(18111), NOW);
        assert!(UsdtIdrRate::from_book(&crossed, NOW, MAX_AGE, dec!(50)).is_none());
        let stale = book(dec!(18111), dec!(18112), NOW - MAX_AGE - 1);
        assert!(UsdtIdrRate::from_book(&stale, NOW, MAX_AGE, dec!(50)).is_none());
        // 200 IDR wide on ~18k is ~110 bps, over the 50 bps cap.
        let wide = book(dec!(18000), dec!(18200), NOW);
        assert!(UsdtIdrRate::from_book(&wide, NOW, MAX_AGE, dec!(50)).is_none());
        let mut one_sided = OrderBook::new(Venue::Indodax, Symbol::new("USDT", "IDR"));
        one_sided.apply_snapshot(
            vec![BookLevel {
                price: dec!(18111),
                qty: dec!(1),
            }],
            vec![],
            1,
            NOW,
        );
        assert!(UsdtIdrRate::from_book(&one_sided, NOW, MAX_AGE, dec!(50)).is_none());
    }

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median_decimals(vec![]), None);
        assert_eq!(
            median_decimals(vec![dec!(3), dec!(1), dec!(2)]),
            Some(dec!(2))
        );
        assert_eq!(
            median_decimals(vec![dec!(1), dec!(2), dec!(3), dec!(4)]),
            Some(dec!(2.5))
        );
    }

    #[test]
    fn primary_within_consensus_is_accepted() {
        let b = book(dec!(18111), dec!(18112), NOW);
        // Upbit + two implied crosses, all within a few bps of the primary.
        let d = resolve(Some(&b), &[dec!(18096), dec!(18120), dec!(18105)]);
        assert_eq!(d.state, RateState::Ok);
        assert_eq!(d.rate.unwrap().mid, dec!(18111.5));
        assert_eq!(d.independent_sources, 3);
        assert!(d.deviation_bps.unwrap() < dec!(10));
    }

    #[test]
    fn primary_far_from_consensus_is_vetoed() {
        // A primary at the old static 19000 against an ~18100 consensus is
        // ~490 bps out: exactly the phantom-edge failure this replaces.
        let b = book(dec!(18999), dec!(19001), NOW);
        let d = resolve(Some(&b), &[dec!(18096), dec!(18105)]);
        assert_eq!(d.state, RateState::DeviationTooWide);
        assert!(d.rate.is_none());
        assert!(d.deviation_bps.unwrap() > dec!(300));
    }

    #[test]
    fn missing_primary_never_falls_back_to_consensus() {
        // Independent samples validate; they are never the divisor.
        let d = resolve(None, &[dec!(18096), dec!(18105)]);
        assert_eq!(d.state, RateState::MissingPrimary);
        assert!(d.rate.is_none());
        assert_eq!(d.consensus_mid, Some(dec!(18100.5)));
    }

    #[test]
    fn primary_without_crosscheck_is_flagged_unchecked() {
        let b = book(dec!(18111), dec!(18112), NOW);
        let d = resolve(Some(&b), &[]);
        assert_eq!(d.state, RateState::Unchecked);
        assert!(d.rate.is_some());
        assert!(d.degraded());
    }

    #[test]
    fn implied_cross_divides_idr_mid_by_usdt_mid() {
        let mut idr = OrderBook::new(Venue::Indodax, Symbol::new("BTC", "IDR"));
        idr.apply_snapshot(
            vec![BookLevel {
                price: dec!(1811000000),
                qty: dec!(1),
            }],
            vec![BookLevel {
                price: dec!(1811200000),
                qty: dec!(1),
            }],
            1,
            NOW,
        );
        let mut usdt = OrderBook::new(Venue::Binance, Symbol::new("BTC", "USDT"));
        usdt.apply_snapshot(
            vec![BookLevel {
                price: dec!(99990),
                qty: dec!(1),
            }],
            vec![BookLevel {
                price: dec!(100010),
                qty: dec!(1),
            }],
            1,
            NOW,
        );
        // 1,811,100,000 IDR mid / 100,000 USDT mid = 18,111.
        assert_eq!(implied_usdt_idr(&idr, &usdt), Some(dec!(18111)));
    }

    #[test]
    fn upbit_frame_parses_and_notices_are_skipped() {
        let s = parse_upbit_orderbook(
            r#"{"type":"orderbook","code":"IDR-USDT","timestamp":1793081771766,"total_ask_size":62666,"total_bid_size":60628,"orderbook_units":[{"ask_price":18101,"bid_price":18090,"ask_size":116,"bid_size":63}]}"#,
        )
        .unwrap();
        assert_eq!(s.bid, dec!(18090));
        assert_eq!(s.ask, dec!(18101));
        assert_eq!(s.ts_ms, 1793081771766);
        assert!(parse_upbit_orderbook(r#"{"status":"CONNECTED"}"#).is_none());
        assert!(upbit_subscribe_frame().to_string().contains("IDR-USDT"));
    }
}
