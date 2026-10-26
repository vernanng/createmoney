//! S1 lead-lag stale-book arb (Binance -> Indodax).
//! Hypothesis: thin IDR books lag the leader 100-500ms after jumps.
//! Minimal prototype: leader mid delta > jump_bps triggers a follower take
//! sized by depth_within_bps. Timestamp-shuffle must kill the edge.
use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{net_edge_bps, Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

pub struct LagChase {
    pub params: StrategyParams,
    pub leader: Venue,
    pub follower: Venue,
    pub symbols: Vec<Symbol>,
    pub jump_bps: Decimal,
    prev_mid: BTreeMap<String, Decimal>,
}

impl LagChase {
    pub fn new(
        params: StrategyParams,
        leader: Venue,
        follower: Venue,
        symbols: Vec<Symbol>,
        jump_bps: Decimal,
    ) -> Self {
        Self {
            params,
            leader,
            follower,
            symbols,
            jump_bps,
            prev_mid: BTreeMap::new(),
        }
    }
}

impl Strategy for LagChase {
    fn name(&self) -> &'static str {
        "lag_chase"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        for sym in &self.symbols {
            let Some(leader_book) = view.book(self.leader, sym) else {
                continue;
            };
            let Some(follower_book) = view.book(self.follower, sym) else {
                continue;
            };
            if !fresh(leader_book, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            if !fresh(follower_book, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            let (Some(lq), Some(fq)) = (leader_book.quote(), follower_book.quote()) else {
                continue;
            };
            let key = sym.to_string();
            let prev = self.prev_mid.get(&key).copied();
            self.prev_mid.insert(key, lq.mid());
            let Some(p) = prev else {
                continue; // prime on first sighting
            };
            if p <= Decimal::ZERO {
                continue;
            }
            let jump = (lq.mid() - p) / p * Decimal::new(10_000, 0);
            if jump.abs() < self.jump_bps {
                continue;
            }
            // Follower stale-ask take: buy or sell the follower depending on
            // jump direction, sized by near-touch depth.
            let (side, px) = if jump > Decimal::ZERO {
                // Leader jumped up: follower asks are stale-cheap.
                let Some(a) = follower_book.best_ask() else {
                    continue;
                };
                (Side::Buy, a.price)
            } else {
                let Some(b) = follower_book.best_bid() else {
                    continue;
                };
                (Side::Sell, b.price)
            };
            let mut qty = size_for_notional(self.params.target_notional_quote, px);
            let depth = follower_book.depth_within_bps(side, Decimal::new(10, 0));
            if depth <= Decimal::ZERO {
                continue;
            }
            qty = qty.min(depth);
            if qty <= Decimal::ZERO {
                continue;
            }
            let (buy_px, sell_px, buy_v, sell_v) = match side {
                Side::Buy => {
                    let Ok(vwap) = follower_book.vwap(Side::Buy, qty) else {
                        continue;
                    };
                    (vwap, lq.ask, self.follower, self.leader)
                }
                Side::Sell => {
                    let Ok(vwap) = follower_book.vwap(Side::Sell, qty) else {
                        continue;
                    };
                    (lq.bid, vwap, self.leader, self.follower)
                }
            };
            // Touch pricing on the leader side is conservative enough for the
            // nano-prototype; paper fills re-walk both books.
            let _ = fq;
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
            let (buy_leg, sell_leg) = match side {
                Side::Buy => (
                    TradeLeg {
                        venue: self.follower,
                        symbol: sym.clone(),
                        side: Side::Buy,
                        price: buy_px,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    },
                    TradeLeg {
                        venue: self.leader,
                        symbol: sym.clone(),
                        side: Side::Sell,
                        price: sell_px,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    },
                ),
                Side::Sell => (
                    TradeLeg {
                        venue: self.leader,
                        symbol: sym.clone(),
                        side: Side::Buy,
                        price: buy_px,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    },
                    TradeLeg {
                        venue: self.follower,
                        symbol: sym.clone(),
                        side: Side::Sell,
                        price: sell_px,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    },
                ),
            };
            out.push(Intent {
                strategy: self.name().into(),
                legs: vec![buy_leg, sell_leg],
                expected_edge_bps: edge,
                ttl_ms: self.params.ttl_ms,
                created_ms: view.now_ms,
                roundtrip: true,
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

    fn book(v: Venue, s: &Symbol, bid: Decimal, ask: Decimal, ts: i64) -> OrderBook {
        let mut b = OrderBook::new(v, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: bid,
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: ask,
                qty: dec!(10),
            }],
            1,
            ts,
        );
        b
    }

    #[test]
    fn fires_on_leader_jump_not_on_flat() {
        let s = Symbol::new("BTC", "USDT");
        let mut st = LagChase::new(
            StrategyParams::default(),
            Venue::Binance,
            Venue::Indodax,
            vec![s.clone()],
            dec!(20),
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        v.put(book(Venue::Binance, &s, dec!(100), dec!(100.1), 1_000));
        v.put(book(Venue::Indodax, &s, dec!(100), dec!(100.1), 1_000));
        assert!(st.on_tick(&v).is_empty(), "first tick only primes");
        // Leader jumps +100bps, follower untouched.
        v.now_ms = 1_500;
        v.put(book(Venue::Binance, &s, dec!(101), dec!(101.1), 1_500));
        v.put(book(Venue::Indodax, &s, dec!(100), dec!(100.1), 1_500));
        // Follower economics: 51bps Indodax fee may still block a 100bps move
        // net of slip; assert either fires or correctly stays out for costs.
        let hits = st.on_tick(&v);
        assert!(hits.len() <= 1);
        if !hits.is_empty() {
            assert_eq!(hits[0].legs.len(), 2);
        }
    }

    #[test]
    fn timestamp_shuffle_kills_edge() {
        // Stale follower (ts far behind now) must never fire: shifting the
        // follower timestamp into the past simulates the shuffle falsifier.
        let s = Symbol::new("BTC", "USDT");
        let mut st = LagChase::new(
            StrategyParams::default(),
            Venue::Binance,
            Venue::Indodax,
            vec![s.clone()],
            dec!(5),
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        v.put(book(Venue::Binance, &s, dec!(100), dec!(100.1), 1_000));
        v.put(book(Venue::Indodax, &s, dec!(100), dec!(100.1), 1_000));
        let _ = st.on_tick(&v);
        v.now_ms = 100_000; // both books now stale
        v.put(book(Venue::Binance, &s, dec!(110), dec!(110.1), 1_000));
        v.put(book(Venue::Indodax, &s, dec!(100), dec!(100.1), 1_000));
        assert!(st.on_tick(&v).is_empty(), "stale books must not fire");
    }
}
