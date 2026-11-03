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
    /// Open positions by pair, with the filled spot quantity an exit must sell.
    open: BTreeMap<(Venue, String), (Symbol, Decimal)>,
    /// Pairs closed this tick; drained by the engine via `take_closed`
    /// so venue-cap exposure is released.
    closed: Vec<(Venue, Symbol)>,
    /// S4 exit timer: last-N annualized prints per pair for the flip EMA.
    history: BTreeMap<(Venue, String), Vec<Decimal>>,
    pub exit_window: usize,
    pub exit_threshold_bps: Decimal,
    /// Horizon over which entry + exit taker/slip costs must be recovered.
    pub planned_hold_hours: Decimal,
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
            planned_hold_hours: Decimal::new(720, 0),
            params,
            min_annualized_bps,
            pairs,
            open: BTreeMap::new(),
            closed: Vec::new(),
            history: BTreeMap::new(),
        }
    }

    pub fn with_exit(mut self, window: usize, threshold_bps: Decimal) -> Self {
        self.exit_window = window.max(1);
        self.exit_threshold_bps = threshold_bps;
        self
    }

    pub fn with_planned_hold_hours(mut self, hours: Decimal) -> Self {
        self.planned_hold_hours = hours.max(Decimal::ONE);
        self
    }

    /// Seed positions already opened by earlier engine runs (rebuilt from
    /// stored fills) so a restart or redeploy does not open duplicates.
    /// Pairs outside the configured set are ignored.
    pub fn with_open_pairs(mut self, pairs: Vec<(Venue, Symbol, Decimal)>) -> Self {
        for (venue, symbol, qty) in pairs {
            self.note_opened(venue, &symbol, qty);
        }
        self
    }

    pub fn mark_closed(&mut self, venue: Venue, symbol: &Symbol) {
        let was_open = self.open.remove(&(venue, symbol.to_string())).is_some();
        if was_open && !self.closed.iter().any(|(v, s)| *v == venue && s == symbol) {
            self.closed.push((venue, symbol.clone()));
        }
    }

    pub fn drain_closed(&mut self) -> Vec<(Venue, Symbol)> {
        std::mem::take(&mut self.closed)
    }

    pub fn open_pairs(&self) -> Vec<(Venue, Symbol, Decimal)> {
        self.open
            .iter()
            .map(|((venue, _), (symbol, qty))| (*venue, symbol.clone(), *qty))
            .collect()
    }

    fn open_qty(&self, venue: Venue, symbol: &Symbol) -> Option<Decimal> {
        self.open
            .get(&(venue, symbol.to_string()))
            .map(|(_, qty)| *qty)
            .filter(|qty| *qty > Decimal::ZERO)
    }

    /// Record a filled entry. Unknown pairs are ignored; duplicate fills
    /// for one position do not double its quantity.
    pub fn note_opened(&mut self, venue: Venue, symbol: &Symbol, qty: Decimal) {
        if qty <= Decimal::ZERO || !self.pairs.contains(&(venue, symbol.clone())) {
            return;
        }
        self.open
            .entry((venue, symbol.to_string()))
            .or_insert_with(|| (symbol.clone(), qty));
    }

    /// S4: record one annualized print, keeping the last `exit_window`.
    /// `on_tick` calls this every tick with the latest print, so repeats
    /// of the same print are dropped — the window spans distinct prints,
    /// not the last few hundred milliseconds of one print.
    pub fn record(&mut self, venue: Venue, symbol: &Symbol, ann_bps: Decimal) {
        let h = self.history.entry((venue, symbol.to_string())).or_default();
        if h.last() == Some(&ann_bps) {
            return;
        }
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
        self.open_pairs()
            .into_iter()
            .map(|(venue, symbol, _qty)| (venue, symbol))
            .collect()
    }

    fn note_filled(&mut self, intent: &Intent) {
        let Some(spot) = intent
            .legs
            .iter()
            .find(|leg| leg.market == MarketKind::Spot)
        else {
            return;
        };
        match spot.side {
            Side::Buy => self.note_opened(spot.venue, &spot.symbol, spot.qty_base),
            Side::Sell => self.mark_closed(spot.venue, &spot.symbol),
        }
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        // Clone pairs so we can inspect open state while iterating.
        for (venue, sym) in self.pairs.clone() {
            let (venue, sym) = (&venue, &sym);
            let open_qty = self.open_qty(*venue, sym);
            let fresh_book = view
                .book(*venue, sym)
                .filter(|b| fresh(b, view.now_ms, self.params.max_book_age_ms));
            let print = view
                .funding
                .iter()
                .find(|f| f.venue == *venue && f.symbol == *sym);
            if let Some(fr) = print {
                self.record(*venue, sym, fr.annualized_bps());
            }
            if let Some(qty) = open_qty {
                // S4 exit is a real closing intent, emitted until a fill
                // is noted back. Without a fresh book there is no honest
                // exit price, so the position stays open.
                if self.should_exit(*venue, sym) {
                    if let Some(book) = fresh_book {
                        if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                            out.push(Intent {
                                strategy: self.name().into(),
                                expected_edge_bps: Decimal::ZERO,
                                ttl_ms: self.params.ttl_ms,
                                created_ms: view.now_ms,
                                roundtrip: false,
                                legs: vec![
                                    TradeLeg {
                                        venue: *venue,
                                        symbol: sym.clone(),
                                        side: Side::Sell,
                                        price: bid.price,
                                        qty_base: qty,
                                        market: MarketKind::Spot,
                                    },
                                    TradeLeg {
                                        venue: *venue,
                                        symbol: sym.clone(),
                                        side: Side::Buy,
                                        price: ask.price,
                                        qty_base: qty,
                                        market: MarketKind::Perp,
                                    },
                                ],
                            });
                        }
                    }
                }
                continue;
            }
            let Some(fr) = print else { continue };
            let ann = fr.annualized_bps();
            // Reserve both crossings. Annualizing the round-trip cost over
            // the planned hold makes it comparable with annualized funding.
            let entry_cost_bps =
                venue.default_taker_bps() * Decimal::TWO + self.params.slip_buffer_bps;
            let round_trip_cost_bps = entry_cost_bps * Decimal::TWO;
            let expected_edge_bps = ann
                - round_trip_cost_bps * Decimal::new(8_760, 0)
                    / self.planned_hold_hours.max(Decimal::ONE);
            if ann < self.min_annualized_bps || expected_edge_bps <= Decimal::ZERO {
                continue;
            }
            let Some(book) = fresh_book else { continue };
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
            // The share cap shrinks with the venue's remaining quote, so
            // successive entries would drain it in geometric dust slices
            // (500, 375, 281, ...) that can never repay their round trip.
            // Below the shared min-notional floor, don't enter at all.
            if notional < self.params.min_notional_quote {
                continue;
            }
            let qty = size_for_notional(notional, ask.price);
            if qty <= Decimal::ZERO {
                continue;
            }
            out.push(Intent {
                strategy: self.name().into(),
                expected_edge_bps,
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
        st.note_opened(Venue::Bybit, &s, hits[0].legs[0].qty_base);
        assert_eq!(st.open_pairs().len(), 1);
        st.note_opened(Venue::Bybit, &s, hits[0].legs[0].qty_base);
        assert_eq!(st.open_pairs().len(), 1, "retries must not double-open");
        // Regression: must not re-fire every tick while the position is open.
        assert!(st.on_tick(&v).is_empty(), "open pair must not churn");
        assert_eq!(st.open_pairs().len(), 1);
        st.mark_closed(Venue::Bybit, &s);
        assert_eq!(st.on_tick(&v).len(), 1, "closed pair may re-enter");
    }

    #[test]
    fn entry_reserves_round_trip_fees_over_planned_hold() {
        // 547.5 bps/yr clears the old annualized gate, but over a 30-day
        // planned hold it accrues only ~45 bps, less than the ~50 bps
        // entry + exit taker/slip round trip on a 10bp venue.
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
            symbol: s,
            rate_per_interval: dec!(0.00005),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        });
        assert!(st.on_tick(&v).is_empty());
    }

    #[test]
    fn share_capped_entry_below_min_notional_is_skipped() {
        // Venue holds 1200 quote; the 25% share cap shrinks the entry to
        // 300, under the 500 min-notional floor. Emitting it would drain
        // the venue in shrinking dust slices (500, 375, 281, ...).
        let s = Symbol::new("BTC", "USDT");
        let params = StrategyParams {
            target_notional_quote: dec!(500),
            min_notional_quote: dec!(500),
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
        v.venue_quote.insert(Venue::Bybit, dec!(1200));
        v.funding.push(FundingRate {
            venue: Venue::Bybit,
            symbol: s.clone(),
            rate_per_interval: dec!(0.0005),
            interval_hours: dec!(8),
            next_funding_ms: 0,
        });
        assert!(st.on_tick(&v).is_empty(), "dust entry must not fire");
        // A funded venue still fires at full size.
        v.venue_quote.insert(Venue::Bybit, dec!(10000));
        assert_eq!(st.on_tick(&v).len(), 1);
    }

    #[test]
    fn record_dedupes_repeated_prints() {
        // on_tick records the latest print every tick (250ms); without
        // dedupe the exit window is the last ~1s of identical prints.
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        );
        for _ in 0..5 {
            st.record(Venue::Bybit, &s, dec!(1000));
        }
        st.record(Venue::Bybit, &s, dec!(0));
        assert_eq!(st.ema(Venue::Bybit, &s), Some(dec!(500)));
    }

    #[test]
    fn exit_emits_sell_spot_and_buy_perp_legs() {
        let s = Symbol::new("BTC", "USDT");
        let mut st = FundingStrategy::new(
            StrategyParams::default(),
            dec!(500),
            vec![(Venue::Bybit, s.clone())],
        )
        .with_exit(1, dec!(500));
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
        let entry = st.on_tick(&v);
        assert_eq!(entry.len(), 1);
        let qty = entry[0].legs[0].qty_base;
        st.note_opened(Venue::Bybit, &s, qty);

        v.funding[0].rate_per_interval = dec!(0.00001);
        let exit = st.on_tick(&v);
        assert_eq!(exit.len(), 1, "exit must be a real closing intent");
        assert_eq!(exit[0].legs[0].market, MarketKind::Spot);
        assert_eq!(exit[0].legs[0].side, Side::Sell);
        assert_eq!(exit[0].legs[0].qty_base, qty);
        assert_eq!(exit[0].legs[1].market, MarketKind::Perp);
        assert_eq!(exit[0].legs[1].side, Side::Buy);
        assert_eq!(exit[0].legs[1].qty_base, qty);
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
            (Venue::Bybit, s.clone(), dec!(1)),
            (Venue::Bybit, s.clone(), dec!(1)), // duplicate is deduped
            (Venue::Okx, Symbol::new("ETH", "USDT"), dec!(1)), // unconfigured: ignored
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
    fn open_pair_without_fresh_book_stays_open() {
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
        st.note_opened(Venue::Bybit, &s, dec!(1));
        // History says exit, but with no executable book the position
        // cannot honestly close, so it remains open for a later tick.
        let v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        assert!(st.on_tick(&v).is_empty());
        assert_eq!(st.open_pairs(), vec![(Venue::Bybit, s, dec!(1))]);
        assert!(st.drain_closed().is_empty());
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
