//! Deterministic synthetic books for paper runs without network access.
//! Prices follow a seeded walk around an anchor; venues get small offsets so
//! cross-venue opportunities actually appear (and disappear).

use arb_core::{BookLevel, OrderBook, Symbol, Venue};
use rust_decimal::Decimal;

pub struct SyntheticFeed {
    seed: u64,
    tick: u64,
    anchor: Decimal,
}

impl SyntheticFeed {
    pub fn new(seed: u64, anchor: Decimal) -> Self {
        Self {
            seed,
            tick: 0,
            anchor,
        }
    }

    fn rnd(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.seed ^ self.tick.wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.tick += 1;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Signed drift in bps in [-range, +range].
    fn drift_bps(&mut self, range_bps: i64) -> Decimal {
        let span = (2 * range_bps + 1) as u64;
        let v = (self.rnd() % span) as i64 - range_bps;
        Decimal::new(v, 0)
    }

    pub fn book(&mut self, venue: Venue, symbol: &Symbol, ts_ms: i64) -> OrderBook {
        let anchor = self.anchor;
        self.book_at(venue, symbol, anchor, ts_ms)
    }

    /// Book around an explicit anchor. Shares the RNG stream, so consecutive
    /// calls (across venues, symbols, ticks) produce different books.
    pub fn book_at(
        &mut self,
        venue: Venue,
        symbol: &Symbol,
        anchor: Decimal,
        ts_ms: i64,
    ) -> OrderBook {
        let drift = self.drift_bps(35); // venue mid wanders ±35bps around anchor
        let mid = anchor * (Decimal::ONE + drift / Decimal::new(10_000, 0));
        let half_spread_bps = match venue {
            Venue::Indodax => Decimal::new(25, 0),
            Venue::Uniswap => Decimal::new(18, 0),
            Venue::Pancakeswap => Decimal::new(15, 0),
            _ => Decimal::new(4, 0),
        };
        let half = mid * half_spread_bps / Decimal::new(10_000, 0);
        let (bid, ask) = (mid - half, mid + half);
        let step = mid * Decimal::new(5, 0) / Decimal::new(10_000, 0);
        let mut levels = |start: Decimal, dir: Decimal| -> Vec<BookLevel> {
            (0..5)
                .map(|i| {
                    let qty = Decimal::new(5 + ((self.rnd() % 20) as i64), 1); // 0.5 .. 2.4
                    BookLevel {
                        price: start + dir * step * Decimal::new(i, 0),
                        qty,
                    }
                })
                .collect()
        };
        let bids = levels(bid, Decimal::NEGATIVE_ONE);
        let asks = levels(ask, Decimal::ONE);
        let mut b = OrderBook::new(venue, symbol.clone());
        b.apply_snapshot(bids, asks, self.tick, ts_ms);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn consecutive_books_evolve_and_differ_by_venue() {
        // Regression: re-seeding per book made every venue/tick identical,
        // which hid all cross-venue edge. One shared stream must evolve.
        let mut f = SyntheticFeed::new(7, dec!(100));
        let s = Symbol::new("BTC", "USDT");
        let a1 = f.book_at(Venue::Binance, &s, dec!(100), 1);
        let b1 = f.book_at(Venue::Bybit, &s, dec!(100), 1);
        let a2 = f.book_at(Venue::Binance, &s, dec!(100), 2);
        let same_a = a1.best_bid().unwrap().price == a2.best_bid().unwrap().price
            && a1.best_ask().unwrap().price == a2.best_ask().unwrap().price;
        let same_venue = a1.best_bid().unwrap().price == b1.best_bid().unwrap().price
            && a1.best_ask().unwrap().price == b1.best_ask().unwrap().price;
        assert!(!same_a || !same_venue, "books must not be frozen identical");
    }

    #[test]
    fn synthetic_books_are_valid_and_deterministic() {
        let mut a = SyntheticFeed::new(42, dec!(100000));
        let mut b = SyntheticFeed::new(42, dec!(100000));
        let s = Symbol::new("BTC", "USDT");
        let (ba, bb) = (a.book(Venue::Binance, &s, 1), b.book(Venue::Binance, &s, 1));
        assert_eq!(ba.best_bid().unwrap().price, bb.best_bid().unwrap().price);
        assert!(!ba.is_crossed());
        assert_eq!(ba.bids_vec().len(), 5);
    }
}
