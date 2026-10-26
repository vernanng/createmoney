//! S6 microprice/imbalance gate: take cross spreads only when both books
//! agree with convergence. Karpathy-style: one filter, top-of-book only.
use arb_core::Quote;
use rust_decimal::Decimal;

/// Top-of-book imbalance in [-1, 1]: +1 = all bid pressure.
pub fn imbalance(bid_qty: Decimal, ask_qty: Decimal) -> Decimal {
    let sum = bid_qty + ask_qty;
    if sum <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        (bid_qty - ask_qty) / sum
    }
}

pub fn quote_imbalance(q: &Quote) -> Decimal {
    imbalance(q.bid_qty, q.ask_qty)
}

/// Both-sided gate: cheap side bid-heavy (about to rise) AND rich side
/// ask-heavy (about to fall). Threshold e.g. 0.2.
pub fn both_sided_agrees(buy_q: &Quote, sell_q: &Quote, threshold: Decimal) -> bool {
    quote_imbalance(buy_q) > threshold && quote_imbalance(sell_q) < -threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{Symbol, Venue};
    use rust_decimal_macros::dec;

    fn q(v: Venue, bid_q: Decimal, ask_q: Decimal) -> Quote {
        Quote {
            venue: v,
            symbol: Symbol::new("BTC", "USDT"),
            bid: dec!(100),
            ask: dec!(100.1),
            bid_qty: bid_q,
            ask_qty: ask_q,
            ts_ms: 1_000,
        }
    }

    #[test]
    fn imbalance_range_and_sign() {
        assert_eq!(imbalance(dec!(3), dec!(1)), dec!(0.5));
        assert_eq!(imbalance(dec!(1), dec!(3)), dec!(-0.5));
        assert_eq!(imbalance(dec!(0), dec!(0)), dec!(0));
    }

    #[test]
    fn both_sided_gate_2x2() {
        let t = dec!(0.2);
        let buy_ok = q(Venue::Binance, dec!(8), dec!(2)); // +0.6
        let sell_ok = q(Venue::Bybit, dec!(2), dec!(8)); // -0.6
        assert!(both_sided_agrees(&buy_ok, &sell_ok, t));
        // One-sided or none must block.
        let flat = q(Venue::Binance, dec!(5), dec!(5));
        assert!(!both_sided_agrees(&flat, &sell_ok, t));
        assert!(!both_sided_agrees(&buy_ok, &flat, t));
        assert!(!both_sided_agrees(&flat, &flat, t));
    }

    #[test]
    fn label_shuffle_destroys_gain() {
        // Shuffled labels (swap bid/ask qty) must flip the signal.
        let buy_ok = q(Venue::Binance, dec!(8), dec!(2));
        let sell_ok = q(Venue::Bybit, dec!(2), dec!(8));
        let t = dec!(0.2);
        assert!(both_sided_agrees(&buy_ok, &sell_ok, t));
        let buy_shuf = q(Venue::Binance, dec!(2), dec!(8));
        let sell_shuf = q(Venue::Bybit, dec!(8), dec!(2));
        assert!(!both_sided_agrees(&buy_shuf, &sell_shuf, t));
    }
}
