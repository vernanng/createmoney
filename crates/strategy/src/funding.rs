//! Funding-rate capture: long spot + short perp when perps pay longs' counterparties.
//! Positive funding => longs pay shorts => short perp / long spot collects it.

use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

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
}

impl FundingStrategy {
    pub fn new(
        params: StrategyParams,
        min_annualized_bps: Decimal,
        pairs: Vec<(Venue, Symbol)>,
    ) -> Self {
        Self {
            params,
            min_annualized_bps,
            pairs,
            open: Vec::new(),
        }
    }

    pub fn mark_closed(&mut self, venue: Venue, symbol: &Symbol) {
        self.open.retain(|(v, s)| !(*v == venue && s == symbol));
    }

    pub fn open_pairs(&self) -> &[(Venue, Symbol)] {
        &self.open
    }
}

impl Strategy for FundingStrategy {
    fn name(&self) -> &'static str {
        "funding_rate"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        // Clone pairs so we can mutate `open` while iterating.
        for (venue, sym) in self.pairs.clone() {
            let (venue, sym) = (&venue, &sym);
            if self.open.iter().any(|(v, s)| v == venue && s == sym) {
                continue; // already positioned; funding is held, not churned
            }
            let Some(fr) = view
                .funding
                .iter()
                .find(|f| f.venue == *venue && f.symbol == *sym)
            else {
                continue;
            };
            let ann = fr.annualized_bps();
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
            let qty = size_for_notional(self.params.target_notional_quote, ask.price);
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
            self.open.push((*venue, sym.clone()));
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
        // Regression: must not re-fire every tick while the position is open.
        assert!(st.on_tick(&v).is_empty(), "open pair must not churn");
        assert_eq!(st.open_pairs().len(), 1);
        st.mark_closed(Venue::Bybit, &s);
        assert_eq!(st.on_tick(&v).len(), 1, "closed pair may re-enter");
    }
}
