//! Local L2 order book. Bids sorted high->low, asks low->high on read.

use crate::{CoreError, Quote, Side, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub qty: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub venue: Venue,
    pub symbol: Symbol,
    // price -> qty
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    pub last_update_id: u64,
    pub ts_ms: i64,
}

impl OrderBook {
    pub fn new(venue: Venue, symbol: Symbol) -> Self {
        Self {
            venue,
            symbol,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            ts_ms: 0,
        }
    }

    /// Replace both sides (REST snapshot). Zero-qty levels are dropped.
    pub fn apply_snapshot(
        &mut self,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        update_id: u64,
        ts_ms: i64,
    ) {
        self.bids = bids
            .into_iter()
            .filter(|l| l.qty > Decimal::ZERO)
            .map(|l| (l.price, l.qty))
            .collect();
        self.asks = asks
            .into_iter()
            .filter(|l| l.qty > Decimal::ZERO)
            .map(|l| (l.price, l.qty))
            .collect();
        self.last_update_id = update_id;
        self.ts_ms = ts_ms;
    }

    /// Apply diff levels. Returns false on sequence gap (caller must resync from snapshot).
    /// `first_id`/`final_id` follow the Binance-style diff contract; other venues map onto it.
    pub fn apply_diff(
        &mut self,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        first_id: u64,
        final_id: u64,
        ts_ms: i64,
    ) -> bool {
        if self.last_update_id != 0 && first_id > self.last_update_id + 1 {
            return false; // gap: missed updates
        }
        // shortcut: stale or duplicate diffs must not rewind the sequence.
        if self.last_update_id != 0 && final_id <= self.last_update_id {
            return false;
        }
        for l in bids {
            Self::upsert(&mut self.bids, l);
        }
        for l in asks {
            Self::upsert(&mut self.asks, l);
        }
        self.last_update_id = final_id;
        self.ts_ms = ts_ms;
        true
    }

    fn upsert(side: &mut BTreeMap<Decimal, Decimal>, l: BookLevel) {
        if l.qty <= Decimal::ZERO {
            side.remove(&l.price);
        } else {
            side.insert(l.price, l.qty);
        }
    }

    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids
            .iter()
            .next_back()
            .map(|(p, q)| BookLevel { price: *p, qty: *q })
    }
    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks
            .iter()
            .next()
            .map(|(p, q)| BookLevel { price: *p, qty: *q })
    }

    pub fn quote(&self) -> Option<Quote> {
        let (b, a) = (self.best_bid()?, self.best_ask()?);
        Some(Quote {
            venue: self.venue,
            symbol: self.symbol.clone(),
            bid: b.price,
            ask: a.price,
            bid_qty: b.qty,
            ask_qty: a.qty,
            ts_ms: self.ts_ms,
        })
    }

    /// Volume-weighted average price to trade `qty_base` against this book.
    /// Buy walks asks, sell walks bids. Errors when depth is insufficient.
    pub fn vwap(&self, side: Side, qty_base: Decimal) -> Result<Decimal, CoreError> {
        if qty_base <= Decimal::ZERO {
            return Ok(Decimal::ZERO);
        }
        let mut remaining = qty_base;
        let mut cost = Decimal::ZERO;
        match side {
            Side::Buy => {
                for (px, q) in self.asks.iter() {
                    let take = remaining.min(*q);
                    cost += px * take;
                    remaining -= take;
                    if remaining <= Decimal::ZERO {
                        break;
                    }
                }
            }
            Side::Sell => {
                for (px, q) in self.bids.iter().rev() {
                    let take = remaining.min(*q);
                    cost += px * take;
                    remaining -= take;
                    if remaining <= Decimal::ZERO {
                        break;
                    }
                }
            }
        }
        if remaining > Decimal::ZERO {
            return Err(CoreError::InsufficientDepth {
                need: qty_base,
                have: qty_base - remaining,
            });
        }
        Ok(cost / qty_base)
    }

    /// Max base qty executable within `max_slip_bps` of the touch price.
    pub fn depth_within_bps(&self, side: Side, max_slip_bps: Decimal) -> Decimal {
        let factor = max_slip_bps / Decimal::new(10_000, 0);
        match side {
            Side::Buy => {
                let Some(best) = self.best_ask() else {
                    return Decimal::ZERO;
                };
                let limit = best.price * (Decimal::ONE + factor);
                self.asks
                    .iter()
                    .take_while(|(p, _)| **p <= limit)
                    .map(|(_, q)| *q)
                    .sum()
            }
            Side::Sell => {
                let Some(best) = self.best_bid() else {
                    return Decimal::ZERO;
                };
                let limit = best.price * (Decimal::ONE - factor);
                self.bids
                    .iter()
                    .rev()
                    .take_while(|(p, _)| **p >= limit)
                    .map(|(_, q)| *q)
                    .sum()
            }
        }
    }

    /// All bid levels, best (highest) first.
    pub fn bids_vec(&self) -> Vec<BookLevel> {
        self.bids
            .iter()
            .rev()
            .map(|(p, q)| BookLevel { price: *p, qty: *q })
            .collect()
    }
    /// All ask levels, best (lowest) first.
    pub fn asks_vec(&self) -> Vec<BookLevel> {
        self.asks
            .iter()
            .map(|(p, q)| BookLevel { price: *p, qty: *q })
            .collect()
    }

    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b.price >= a.price,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn book() -> OrderBook {
        let mut b = OrderBook::new(Venue::Binance, Symbol::new("BTC", "USDT"));
        b.apply_snapshot(
            vec![
                BookLevel {
                    price: dec!(99),
                    qty: dec!(2),
                },
                BookLevel {
                    price: dec!(98),
                    qty: dec!(3),
                },
            ],
            vec![
                BookLevel {
                    price: dec!(100),
                    qty: dec!(1),
                },
                BookLevel {
                    price: dec!(101),
                    qty: dec!(4),
                },
            ],
            10,
            1_000,
        );
        b
    }

    #[test]
    fn best_and_vwap() {
        let b = book();
        assert_eq!(b.best_bid().unwrap().price, dec!(99));
        assert_eq!(b.best_ask().unwrap().price, dec!(100));
        // Buy 2: 1@100 + 1@101 = 100.5
        assert_eq!(b.vwap(Side::Buy, dec!(2)).unwrap(), dec!(100.5));
        // Sell 3: 2@99 + 1@98 = 98.666...
        let v = b.vwap(Side::Sell, dec!(3)).unwrap();
        assert!(v > dec!(98.6) && v < dec!(98.7));
        assert!(b.vwap(Side::Buy, dec!(99)).is_err());
    }

    #[test]
    fn diff_gap_and_delete() {
        let mut b = book();
        assert!(!b.apply_diff(vec![], vec![], 20, 21, 2_000)); // gap after id 10
        assert!(b.apply_diff(
            vec![BookLevel {
                price: dec!(99),
                qty: dec!(0)
            }],
            vec![],
            11,
            11,
            2_000
        ));
        assert_eq!(b.best_bid().unwrap().price, dec!(98)); // 99 level removed
    }

    #[test]
    fn stale_diff_does_not_rewind() {
        let mut b = book();
        assert!(!b.apply_diff(vec![], vec![], 5, 9, 2_000));
        assert_eq!(b.last_update_id, 10);
        assert!(!b.apply_diff(vec![], vec![], 11, 10, 2_000));
        assert_eq!(b.last_update_id, 10);
    }

    #[test]
    fn depth_within_bps_works() {
        let b = book();
        // 0 bps: only touch level (1 @ 100). 200bps: up to 102 -> 1+4=5.
        assert_eq!(b.depth_within_bps(Side::Buy, dec!(0)), dec!(1));
        assert_eq!(b.depth_within_bps(Side::Buy, dec!(200)), dec!(5));
    }
}
