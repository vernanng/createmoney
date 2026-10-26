//! S7 venue-fee routing optimizer: per-pair thresholds from paper PnL.
//! Global 15bps is wrong for 10bps vs 51bps venues. EWMA of realized
//! round-trip PnL moves the threshold up after losers, never below global.
use arb_core::{Symbol, Venue};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

pub struct FeeTable {
    pub global_min_bps: Decimal,
    alpha: Decimal,
    table: BTreeMap<(Venue, Venue, String), Decimal>,
}

impl FeeTable {
    pub fn new(global_min_bps: Decimal) -> Self {
        Self {
            global_min_bps,
            alpha: Decimal::new(3, 1), // 0.3
            table: BTreeMap::new(),
        }
    }

    pub fn threshold_for(&self, buy: Venue, sell: Venue, sym: &Symbol) -> Decimal {
        self.table
            .get(&(buy, sell, sym.to_string()))
            .copied()
            .unwrap_or(self.global_min_bps)
    }

    /// Update from one realized round-trip: losers raise the bar, winners
    /// decay back toward the global floor.
    pub fn record(
        &mut self,
        buy: Venue,
        sell: Venue,
        sym: &Symbol,
        pnl_quote: Decimal,
        notional_quote: Decimal,
    ) {
        if notional_quote <= Decimal::ZERO {
            return;
        }
        let pnl_bps = pnl_quote / notional_quote * Decimal::new(10_000, 0);
        let target = self.global_min_bps - pnl_bps;
        let floor = self.global_min_bps;
        let target = target.max(floor);
        let old = self.threshold_for(buy, sell, sym);
        let next = old + (target - old) * self.alpha;
        self.table.insert((buy, sell, sym.to_string()), next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn loser_raises_winner_stays_at_floor() {
        let s = Symbol::new("BTC", "USDT");
        let mut t = FeeTable::new(dec!(15));
        assert_eq!(
            t.threshold_for(Venue::Binance, Venue::Indodax, &s),
            dec!(15)
        );
        // Loser: -$20 on $1000 = -200bps -> target 215, EWMA 15 + 0.3*200 = 75.
        t.record(Venue::Binance, Venue::Indodax, &s, dec!(-20), dec!(1000));
        assert!(t.threshold_for(Venue::Binance, Venue::Indodax, &s) > dec!(15));
        // Winner on another route stays at floor.
        t.record(Venue::Binance, Venue::Bybit, &s, dec!(5), dec!(1000));
        assert_eq!(t.threshold_for(Venue::Binance, Venue::Bybit, &s), dec!(15));
    }

    #[test]
    fn equal_thresholds_reproduce_baseline() {
        // Sanity: forcing equal thresholds must reproduce the global baseline.
        let s = Symbol::new("BTC", "USDT");
        let t = FeeTable::new(dec!(15));
        for v in [Venue::Binance, Venue::Bybit, Venue::Indodax] {
            assert_eq!(t.threshold_for(Venue::Binance, v, &s), dec!(15));
        }
    }
}
