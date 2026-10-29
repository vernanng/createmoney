//! Funding-rate capture: long spot + short perp when perps pay longs' counterparties.
//! Positive funding => longs pay shorts => short perp / long spot collects it.

use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingRate {
    pub venue: Venue,
    pub symbol: Symbol, // perp symbol, same BASE/QUOTE spelling as spot here
    /// Funding per interval as a fraction (0.0001 = 1bp per interval).
    pub rate_per_interval: Decimal,
    pub interval_hours: Decimal,
    pub next_funding_ms: i64,
}

impl FundingRate {
    /// Annualized funding in bps (simple, non-compounded).
    pub fn annualized_bps(&self) -> Decimal {
        if self.interval_hours <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let intervals_per_year = Decimal::new(8760, 0) / self.interval_hours;
        self.rate_per_interval * intervals_per_year * Decimal::new(10_000, 0)
    }
}

pub struct FundingStrategy {
    pub params: StrategyParams,
    pub min_annualized_bps: Decimal,
    /// venue -> spot symbols to pair against perps
    pub pairs: Vec<(Venue, Symbol)>,
    /// Pairs with an open position; each fires once until closed via `mark_closed`.
    open: Vec<(Venue, Symbol)>,
    /// Pairs closed this tick; drained by the engine via `take_closed`
    /// so venue-cap exposure is released.
    closed: Vec<(Venue, Symbol)>,
    /// S4 exit timer: last-N annualized prints per pair for the flip EMA.
    history: BTreeMap<(Venue, String), Vec<Decimal>>,
    pub exit_window: usize,
    pub exit_threshold_bps: Decimal,
}

impl FundingStrategy {
    pub fn new(
        params: StrategyParams,
        min_annualized_bps: Decimal,
        pairs: Vec<(Venue, Symbol)>,
    ) -> Self {
        Self {
            exit_window: 5,
            exit_threshold_bps: min_annualized_bps,
            params,
            min_annualized_bps,
            pairs,
            open: Vec::new(),
            closed: Vec::new(),
            history: BTreeMap::new(),
        }
    }

    pub fn with_exit(mut self, window: usize, threshold_bps: Decimal) -> Self {
        self.exit_window = window.max(1);
        self.exit_threshold_bps = threshold_bps;
        self
    }

    /// Seed pairs already opened by earlier engine runs (rebuilt from
    /// stored fills) so a restart or redeploy does not open duplicates.
    /// Pairs outside the configured set are ignored.
    pub fn with_open_pairs(mut self, pairs: Vec<(Venue, Symbol)>) -> Self {
        for p in pairs {
            if self.pairs.contains(&p) && !self.open.contains(&p) {
                self.open.push(p);
            }
        }
        self
    }

    pub fn mark_closed(&mut self, venue: Venue, symbol: &Symbol) {
        let was_open = self.open.iter().any(|(v, s)| *v == venue && s == symbol);
        self.open.retain(|(v, s)| !(*v == venue && s == symbol));
        if was_open && !self.closed.iter().any(|(v, s)| *v == venue && s == symbol) {
            self.closed.push((venue, symbol.clone()));
        }
    }

    pub fn drain_closed(&mut self) -> Vec<(Venue, Symbol)> {
        std::mem::take(&mut self.closed)
    }

    pub fn open_pairs(&self) -> &[(Venue, Symbol)] {
        &self.open
    }

    /// Record a filled entry. Unknown or duplicate pairs are ignored so
    /// restarts and retries cannot double-open.
    pub fn note_opened(&mut self, venue: Venue, symbol: &Symbol) {
        if self.pairs.contains(&(venue, symbol.clone()))
            && !self.open.contains(&(venue, symbol.clone()))
        {
            self.open.push((venue, symbol.clone()));
        }
    }

    /// S4: record one annualized print, keeping the last `exit_window`.
    pub fn record(&mut self, venue: Venue, symbol: &Symbol, ann_bps: Decimal) {
        let h = self.history.entry((venue, symbol.to_string())).or_default();
        h.push(ann_bps);
        while h.len() > self.exit_window {
            h.remove(0);
        }
    }

    /// Simple mean of the window (EMA-equivalent for the nano-prototype;
    /// monotonic in the same direction as an EMA for the falsifier).
    pub fn ema(&self, venue: Venue, symbol: &Symbol) -> Option<Decimal> {
        let h = self.history.get(&(venue, symbol.to_string()))?;
        if h.is_empty() {
            return None;
        }
        Some(h.iter().copied().sum::<Decimal>() / Decimal::new(h.len() as i64, 0))
    }

    /// Exit when the window average falls below the exit threshold.
    pub fn should_exit(&self, venue: Venue, symbol: &Symbol) -> bool {
        self.ema(venue, symbol)
            .map(|e| e < self.exit_threshold_bps)
            .unwrap_or(false)
    }
}

impl Strategy for FundingStrategy {
    fn name(&self) -> &'static str {
        "funding_rate"
    }

    fn take_closed(&mut self) -> Vec<(Venue, Symbol)> {
        self.drain_closed()
    }

    fn open_positions(&self) -> Vec<(Venue, Symbol)> {
        self.open.clone()
    }

    fn note_filled(&mut self, venue: Venue, symbol: &Symbol) {
        self.note_opened(venue, symbol);
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        // Clone pairs so we can mutate `open` while iterating.
        for (venue, sym) in self.pairs.clone() {
            let (venue, sym) = (&venue, &sym);
            let is_open = self.open.iter().any(|(v, s)| v == venue && s == sym);
            let Some(fr) = view
                .funding
                .iter()
                .find(|f| f.venue == *venue && f.symbol == *sym)
            else {
                // No print this tick: an open pair can still exit on its
                // stored history instead of persisting on a dead feed.
                if is_open && self.should_exit(*venue, sym) {
                    self.mark_closed(*venue, sym);
                }
                continue;
            };
            let ann = fr.annualized_bps();
            self.record(*venue, sym, ann);
            if is_open {
                // S4: time the exit — flip below threshold closes, no churn.
                if self.should_exit(*venue, sym) {
                    self.mark_closed(*venue, sym);
                }
                continue;
            }
            // Entry cost: two taker fees + slip, amortized must stay well below funding.
            let entry_cost_bps =
                venue.default_taker_bps() * Decimal::TWO + self.params.slip_buffer_bps;
            if ann < self.min_annualized_bps + entry_cost_bps {
                continue;
            }
            let Some(book) = view.book(*venue, sym) else {
                continue;
            };
            if !fresh(book, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            let Some(ask) = book.best_ask() else { continue };
            // Single-venue entry: shrink to the venue-share cap instead of
            // emitting an intent the engine's risk gate would only reject.
            // A 500-quote print never fit a 400-quote venue at 25%.
            let mut notional = self.params.target_notional_quote;
            if self.params.max_venue_quote_share_bps > Decimal::ZERO {
                if let Some(funds) = view.venue_quote.get(venue) {
                    let cap =
                        *funds * self.params.max_venue_quote_share_bps / Decimal::new(10_000, 0);
                    if cap <= Decimal::ZERO {
                        continue;
                    }
                    notional = notional.min(cap);
                }
            }
            let qty = size_for_notional(notional, ask.price);
            if qty <= Decimal::ZERO {
                continue;
            }
            // Legs: Spot buy + Perp sell on the same venue. Edge reported is
            // annualized funding net of entry costs.
            out.push(Intent {
                strategy: self.name().into(),
                expected_edge_bps: ann - entry_cost_bps,
                ttl_ms: self.params.ttl_ms,
                created_ms: view.now_ms,
                roundtrip: false,
                legs: vec![
                    TradeLeg {
                        venue: *venue,
                        symbol: sym.clone(),
                        side: Side::Buy,
                        price: ask.price,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    },
                    TradeLeg {
                        venue: *venue,
                        symbol: sym.clone(),
                        side: Side::Sell,
                        price: ask.price,
                        qty_base: qty,
                        market: MarketKind::Perp,
                    },
                ],
            });
            // Opens are recorded by `note_filled` after the engine fills
            // the intent. Pushing here would strand phantoms on fill failure.
        }
        out
    }
}

/// Funding collected over `hours_held` on `notional` at a constant rate.
pub fn accrued(notional: Decimal, rate: &FundingRate, hours_held: Decimal) -> Decimal {
    if rate.interval_hours <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    notional * rate.rate_per_interval * (hours_held / rate.interval_hours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, OrderBook};
    use rust_decimal_macros::dec;

    #[test]
    fn annualized_and_accrual() {
        let fr = FundingRate {
            venue: Venue::Bybit,
            symbol: Symbol::new("BTC", "USDT"),
            rate_per_interval: dec!(0.0001),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        };
        // 1bp per 8h -> 1095 intervals/yr -> 1095bps/yr.
        assert_eq!(fr.annualized_bps(), dec!(1095));
        // 24h on 10k = 3 intervals * 1bp * 10k = 3.
        assert_eq!(accrued(dec!(10000), &fr, dec!(24)), dec!(3));
    }

    #[test]
    fn fires_only_above_threshold() {
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.1),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(b);
        v.funding.push(FundingRate {
            venue: Venue::Bybit,
            symbol: s.clone(),
            rate_per_interval: dec!(0.00001),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        });
        assert!(st.on_tick(&v).is_empty(), "thin funding must not fire");
        v.funding[0].rate_per_interval = dec!(0.0005); // 5bps/8h ≈ 5475bps/yr
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        // Spot long + Perp short, explicitly typed (never guessed from symbol).
        assert_eq!(hits[0].legs[0].market, MarketKind::Spot);
        assert_eq!(hits[0].legs[0].side, Side::Buy);
        assert_eq!(hits[0].legs[1].market, MarketKind::Perp);
        assert_eq!(hits[0].legs[1].side, Side::Sell);
        // Regression: the intent alone must not open the pair — only a
        // fill may, or failed fills strand phantoms that never close.
        assert!(st.open_pairs().is_empty());
        st.note_opened(Venue::Bybit, &s);
        assert_eq!(st.open_pairs().len(), 1);
        st.note_opened(Venue::Bybit, &s);
        assert_eq!(st.open_pairs().len(), 1, "retries must not double-open");
        // Regression: must not re-fire every tick while the position is open.
        assert!(st.on_tick(&v).is_empty(), "open pair must not churn");
        assert_eq!(st.open_pairs().len(), 1);
        st.mark_closed(Venue::Bybit, &s);
        assert_eq!(st.on_tick(&v).len(), 1, "closed pair may re-enter");
    }

    #[test]
    fn sizes_to_venue_share_cap() {
        let s = Symbol::new("BTC", "USDT");
        let params = StrategyParams {
            target_notional_quote: dec!(500),
            max_venue_quote_share_bps: dec!(2500),
            ..Default::default()
        };
        let mut st = FundingStrategy::new(params, dec!(500), vec![(Venue::Bybit, s.clone())]);
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.1),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(b);
        v.funding.push(FundingRate {
            venue: Venue::Bybit,
            symbol: s.clone(),
            rate_per_interval: dec!(0.0005),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        });
        v.venue_quote.insert(Venue::Bybit, dec!(400));
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        // 25% of a 400-quote venue is 100, not the 500 target.
        assert_eq!(
            hits[0].legs[0].qty_base,
            size_for_notional(dec!(100), dec!(100.1))
        );
        // Unknown venue falls back to the full notional.
        v.venue_quote.clear();
        st.mark_closed(Venue::Bybit, &s);
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].legs[0].qty_base,
            size_for_notional(dec!(500), dec!(100.1))
        );
    }

    #[test]
    fn restored_open_pair_does_not_refire() {
        // Regression: a redeploy must not reopen positions that a previous
        // engine run already filled and stored.
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        )
        .with_open_pairs(vec![
            (Venue::Bybit, s.clone()),
            (Venue::Bybit, s.clone()), // duplicate is deduped
            (Venue::Okx, Symbol::new("ETH", "USDT")), // unconfigured: ignored
        ]);
        assert_eq!(st.open_pairs().len(), 1);
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.1),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(b);
        v.funding.push(FundingRate {
            venue: Venue::Bybit,
            symbol: s.clone(),
            rate_per_interval: dec!(0.0005),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        });
        assert!(
            st.on_tick(&v).is_empty(),
            "restored open pair must not re-fire"
        );
        st.mark_closed(Venue::Bybit, &s);
        assert_eq!(st.on_tick(&v).len(), 1, "closed pair may re-enter");
    }

    #[test]
    fn open_pair_exits_without_fresh_print() {
        use crate::Strategy;
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        )
        .with_exit(2, dec!(500));
        st.record(Venue::Bybit, &s, dec!(100));
        st.record(Venue::Bybit, &s, dec!(100));
        st.open.push((Venue::Bybit, s.clone()));
        // No funding print in view: exit must still fire from history.
        let v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        assert!(st.on_tick(&v).is_empty());
        assert!(st.open_pairs().is_empty());
        assert_eq!(st.drain_closed(), vec![(Venue::Bybit, s)]);
    }

    #[test]
    fn flip_below_threshold_auto_exits() {
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        )
        .with_exit(3, dec!(500));
        // One hot print then a cold flip: mean drops below threshold.
        st.record(Venue::Bybit, &s, dec!(600));
        st.record(Venue::Bybit, &s, dec!(100));
        st.record(Venue::Bybit, &s, dec!(100));
        assert!(st.should_exit(Venue::Bybit, &s), "cold flip must trip exit");
        // Shuffled timestamps (steady hot) must not exit.
        let mut hot = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        )
        .with_exit(3, dec!(500));
        hot.record(Venue::Bybit, &s, dec!(5000));
        hot.record(Venue::Bybit, &s, dec!(5000));
        hot.record(Venue::Bybit, &s, dec!(5000));
        assert!(!hot.should_exit(Venue::Bybit, &s));
    }
}
