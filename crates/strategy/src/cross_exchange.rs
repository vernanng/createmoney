//! Cross-exchange spot: buy the cheap venue, sell the rich venue.
//! Assumes pre-funded inventory on every venue (no transfers in the loop).

use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{net_edge_bps, Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;

pub struct CrossExchange {
    pub params: StrategyParams,
    pub venues: Vec<Venue>,
    pub symbols: Vec<Symbol>,
}

impl CrossExchange {
    pub fn new(params: StrategyParams, venues: Vec<Venue>, symbols: Vec<Symbol>) -> Self {
        Self {
            params,
            venues,
            symbols,
        }
    }
}

impl Strategy for CrossExchange {
    fn name(&self) -> &'static str {
        "cross_exchange"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        for sym in &self.symbols {
            // Find cheapest ask and richest bid across fresh books.
            let mut best_buy: Option<(Venue, Decimal, Decimal)> = None; // venue, vwap, qty
            let mut best_sell: Option<(Venue, Decimal, Decimal)> = None;
            for v in &self.venues {
                let Some(b) = view.book(*v, sym) else {
                    continue;
                };
                if !fresh(b, view.now_ms, self.params.max_book_age_ms) {
                    continue;
                }
                let Some(ask) = b.best_ask() else { continue };
                let qty = size_for_notional(self.params.target_notional_quote, ask.price);
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
            out.push(Intent {
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
            });
        }
        out
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
}
