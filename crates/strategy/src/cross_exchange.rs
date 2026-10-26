//! Cross-exchange spot: buy the cheap venue, sell the rich venue.
//! Assumes pre-funded inventory on every venue (no transfers in the loop).

use crate::{
    fresh, imbalance::both_sided_agrees, size_for_notional, MarketView, Strategy, StrategyParams,
};
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

/// S2 depth-adaptive sizing grid (Karpathy-style: try small first, prove the
/// hump). Fixed $1000 leaves money in deep books and pays impact in thin ones.
pub const ADAPTIVE_NOTIONALS: [i64; 4] = [100, 500, 1000, 2500];

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
    /// S2: scan the notional grid via VWAP + net edge, keep the max net edge.
    /// Skips notionals that exceed book depth (VWAP error). Returns None when
    /// no notional clears `min_edge_bps`.
    fn best_sized_intent(&self, view: &MarketView, sym: &Symbol) -> Option<Intent> {
        let mut best: Option<(Venue, Venue, Decimal, Decimal, Decimal, Decimal)> = None;
        // (buy_v, sell_v, qty, buy_px, sell_px, edge)
        for n in ADAPTIVE_NOTIONALS {
            let notional = Decimal::new(n, 0);
            let mut best_buy: Option<(Venue, Decimal, Decimal)> = None;
            let mut best_sell: Option<(Venue, Decimal, Decimal)> = None;
            for v in &self.venues {
                let Some(b) = view.book(*v, sym) else {
                    continue;
                };
                if !fresh(b, view.now_ms, self.params.max_book_age_ms) {
                    continue;
                }
                let Some(touch) = b.best_ask() else { continue };
                let qty = size_for_notional(notional, touch.price);
                if qty <= Decimal::ZERO {
                    continue;
                }
                if let Ok(vwap) = b.vwap(Side::Buy, qty) {
                    if best_buy.map(|(_, p, _)| vwap < p).unwrap_or(true) {
                        best_buy = Some((*v, vwap, qty));
                    }
                }
                if let Ok(vwap) = b.vwap(Side::Sell, qty) {
                    if best_sell.map(|(_, p, _)| vwap > p).unwrap_or(true) {
                        best_sell = Some((*v, vwap, qty));
                    }
                }
            }
            let (Some((buy_v, buy_px, qty)), Some((sell_v, sell_px, _))) = (best_buy, best_sell)
            else {
                continue;
            };
            if buy_v == sell_v {
                continue;
            }
            // S6 gate: require both-sided imbalance agreement when enabled.
            if let Some(t) = self.imbalance_threshold {
                let (Some(bb), Some(sb)) = (view.book(buy_v, sym), view.book(sell_v, sym)) else {
                    continue;
                };
                let (Some(bq), Some(sq)) = (bb.quote(), sb.quote()) else {
                    continue;
                };
                if !both_sided_agrees(&bq, &sq, t) {
                    continue;
                }
            }
            let edge = net_edge_bps(
                buy_px,
                sell_px,
                buy_v.default_taker_bps(),
                sell_v.default_taker_bps(),
                self.params.slip_buffer_bps,
            );
            if edge < self.params.min_edge_bps {
                continue;
            }
            let replace = best.map(|(_, _, _, _, _, e)| edge > e).unwrap_or(true);
            if replace {
                best = Some((buy_v, sell_v, qty, buy_px, sell_px, edge));
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
    fn adaptive_skips_infeasible_large_size() {
        // Thin books: only the small notionals fit, so the adaptive scan must
        // still fire instead of dying on the 2500 grid point.
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
