//! Cross-exchange spot: buy the cheap venue, sell the rich venue.
//! Assumes pre-funded inventory on every venue (no transfers in the loop).

use crate::{fresh, imbalance::both_sided_agrees, MarketView, Strategy, StrategyParams};
use arb_core::{net_edge_bps, Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;

pub struct CrossExchange {
    pub params: StrategyParams,
    pub venues: Vec<Venue>,
    pub symbols: Vec<Symbol>,
    /// S6: None = off (default, preserves legacy behavior). Some(t) requires
    /// both-sided imbalance agreement at threshold t before emitting.
    pub imbalance_threshold: Option<Decimal>,
}

impl CrossExchange {
    pub fn new(params: StrategyParams, venues: Vec<Venue>, symbols: Vec<Symbol>) -> Self {
        Self {
            params,
            venues,
            symbols,
            imbalance_threshold: None,
        }
    }

    pub fn with_imbalance(mut self, threshold: Decimal) -> Self {
        self.imbalance_threshold = Some(threshold);
        self
    }
}

/// S2 dynamic sizing: no fixed grid. For each venue pair, walk the two books
/// level by level and keep the largest quantity whose combined VWAP still
/// clears `min_edge_bps` net of fees. Size follows the opportunity.
fn max_profitable_qty(
    asks: &[arb_core::BookLevel],
    bids: &[arb_core::BookLevel],
    buy_fee_bps: Decimal,
    sell_fee_bps: Decimal,
    slip_buffer_bps: Decimal,
    min_edge_bps: Decimal,
) -> Option<(Decimal, Decimal, Decimal)> {
    // (qty_base, buy_vwap, sell_vwap)
    let (mut ai, mut bi) = (0usize, 0usize);
    let (mut aq, mut bq) = (
        asks.first().map(|l| l.qty).unwrap_or(Decimal::ZERO),
        bids.first().map(|l| l.qty).unwrap_or(Decimal::ZERO),
    );
    let (mut cost, mut proceeds, mut total) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    let mut best: Option<(Decimal, Decimal, Decimal)> = None;
    while ai < asks.len() && bi < bids.len() {
        let q = aq.min(bq);
        if q <= Decimal::ZERO {
            break;
        }
        cost += q * asks[ai].price;
        proceeds += q * bids[bi].price;
        total += q;
        aq -= q;
        bq -= q;
        let edge = net_edge_bps(
            cost / total,
            proceeds / total,
            buy_fee_bps,
            sell_fee_bps,
            slip_buffer_bps,
        );
        if edge >= min_edge_bps {
            best = Some((total, cost / total, proceeds / total));
        }
        if aq <= Decimal::ZERO {
            ai += 1;
            aq = asks.get(ai).map(|l| l.qty).unwrap_or(Decimal::ZERO);
        }
        if bq <= Decimal::ZERO {
            bi += 1;
            bq = bids.get(bi).map(|l| l.qty).unwrap_or(Decimal::ZERO);
        }
    }
    best
}

impl Strategy for CrossExchange {
    fn name(&self) -> &'static str {
        "cross_exchange"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        for sym in &self.symbols {
            if let Some(intent) = self.best_sized_intent(view, sym) {
                out.push(intent);
            }
        }
        out
    }
}

impl CrossExchange {
    /// S2: size each venue pair to its profitable depth, keep the best edge.
    /// Returns None when no pair clears `min_edge_bps` at any quantity.
    fn best_sized_intent(&self, view: &MarketView, sym: &Symbol) -> Option<Intent> {
        // (buy_v, sell_v, qty, buy_px, sell_px, edge)
        let mut best: Option<(Venue, Venue, Decimal, Decimal, Decimal, Decimal)> = None;
        for buy_v in &self.venues {
            for sell_v in &self.venues {
                if buy_v == sell_v {
                    continue;
                }
                let (Some(buy_b), Some(sell_b)) = (view.book(*buy_v, sym), view.book(*sell_v, sym))
                else {
                    continue;
                };
                if !fresh(buy_b, view.now_ms, self.params.max_book_age_ms)
                    || !fresh(sell_b, view.now_ms, self.params.max_book_age_ms)
                {
                    continue;
                }
                // S6 gate: require both-sided imbalance agreement when enabled.
                if let Some(t) = self.imbalance_threshold {
                    let (Some(bq), Some(sq)) = (buy_b.quote(), sell_b.quote()) else {
                        continue;
                    };
                    if !both_sided_agrees(&bq, &sq, t) {
                        continue;
                    }
                }
                let Some((qty, buy_px, sell_px)) = max_profitable_qty(
                    &buy_b.asks_vec(),
                    &sell_b.bids_vec(),
                    buy_v.default_taker_bps(),
                    sell_v.default_taker_bps(),
                    self.params.slip_buffer_bps,
                    self.params.min_edge_bps,
                ) else {
                    continue;
                };
                let edge = net_edge_bps(
                    buy_px,
                    sell_px,
                    buy_v.default_taker_bps(),
                    sell_v.default_taker_bps(),
                    self.params.slip_buffer_bps,
                );
                let replace = best
                    .map(|(_, _, q, _, _, e)| edge > e || (edge == e && qty > q))
                    .unwrap_or(true);
                if replace {
                    best = Some((*buy_v, *sell_v, qty, buy_px, sell_px, edge));
                }
            }
        }
        let (buy_v, sell_v, qty, buy_px, sell_px, edge) = best?;
        Some(Intent {
            strategy: self.name().into(),
            expected_edge_bps: edge,
            ttl_ms: self.params.ttl_ms,
            created_ms: view.now_ms,
            roundtrip: true,
            legs: vec![
                TradeLeg {
                    venue: buy_v,
                    symbol: sym.clone(),
                    side: Side::Buy,
                    price: buy_px,
                    qty_base: qty,
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: sell_v,
                    symbol: sym.clone(),
                    side: Side::Sell,
                    price: sell_px,
                    qty_base: qty,
                    market: MarketKind::Spot,
                },
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, OrderBook};
    use rust_decimal_macros::dec;

    fn view(spread: Decimal) -> MarketView {
        let s = Symbol::new("BTC", "USDT");
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut a = OrderBook::new(Venue::Binance, s.clone());
        a.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.05),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        let rich = dec!(100) + spread;
        b.apply_snapshot(
            vec![BookLevel {
                price: rich,
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: rich + dec!(0.05),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(a);
        v.put(b);
        v
    }

    #[test]
    fn fires_only_when_net_edge_clears_fees() {
        let mut st = CrossExchange::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit],
            vec![Symbol::new("BTC", "USDT")],
        );
        assert_eq!(
            st.on_tick(&view(dec!(0.10))).len(),
            0,
            "10c on 100 = 10bps gross < fees"
        );
        let hits = st.on_tick(&view(dec!(1.00)));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].legs[0].venue, Venue::Binance);
        assert_eq!(hits[0].legs[1].venue, Venue::Bybit);
    }

    #[test]
    fn stale_books_ignored() {
        let mut st = CrossExchange::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit],
            vec![Symbol::new("BTC", "USDT")],
        );
        let mut v = view(dec!(1.00));
        v.now_ms = 100_000; // books timestamped at 1000 -> stale
        assert!(st.on_tick(&v).is_empty());
    }

    #[test]
    fn dynamic_sizes_to_profitable_depth() {
        // Thin books cap the walk at 1.5; deep books below let it run past
        // any fixed grid point, so size follows the opportunity.
        use arb_core::{BookLevel, OrderBook};
        let s = Symbol::new("BTC", "USDT");
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut a = OrderBook::new(Venue::Binance, s.clone());
        a.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(1.5),
            }],
            vec![BookLevel {
                price: dec!(100.05),
                qty: dec!(1.5),
            }],
            1,
            1_000,
        );
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(101),
                qty: dec!(1.5),
            }],
            vec![BookLevel {
                price: dec!(101.05),
                qty: dec!(1.5),
            }],
            1,
            1_000,
        );
        v.put(a);
        v.put(b);
        let mut st = CrossExchange::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit],
            vec![s],
        );
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1, "adaptive must pick a feasible small size");
        let qty = hits[0].legs[0].qty_base;
        assert!(
            qty <= dec!(1.5),
            "picked qty must fit thin depth, got {qty}"
        );
    }

    #[test]
    fn deep_books_size_past_old_grid() {
        // $2500 at px 100 was 25 base; profitable depth of 100 must be taken
        // whole now that no fixed grid caps the walk.
        use arb_core::{BookLevel, OrderBook};
        let s = Symbol::new("BTC", "USDT");
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        for (venue, bid, ask) in [
            (Venue::Binance, dec!(100), dec!(100.05)),
            (Venue::Bybit, dec!(101), dec!(101.05)),
        ] {
            let mut b = OrderBook::new(venue, s.clone());
            b.apply_snapshot(
                vec![BookLevel {
                    price: bid,
                    qty: dec!(100),
                }],
                vec![BookLevel {
                    price: ask,
                    qty: dec!(100),
                }],
                1,
                1_000,
            );
            v.put(b);
        }
        let mut st = CrossExchange::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit],
            vec![s],
        );
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].legs[0].qty_base, dec!(100));
    }

    #[test]
    fn imbalance_gate_blocks_flat_toxic_spread() {
        use rust_decimal::Decimal;
        // view(1.00) has flat 10/10 qty on both sides -> imbalance 0.
        let mut gated = CrossExchange::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit],
            vec![Symbol::new("BTC", "USDT")],
        )
        .with_imbalance(Decimal::new(2, 1));
        assert!(
            gated.on_tick(&view(dec!(1.00))).is_empty(),
            "flat books must not pass the both-sided gate"
        );
    }
}
