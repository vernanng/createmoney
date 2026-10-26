//! Exact decimal helpers. 1 bp = 0.01% = 0.0001.

use rust_decimal::Decimal;

/// bps fraction of an amount: bps_of(10_000, 25) = 25.
pub fn bps_of(amount: Decimal, bps: Decimal) -> Decimal {
    amount * bps / Decimal::new(10_000, 0)
}

pub fn fee_amount(notional: Decimal, fee_bps: Decimal) -> Decimal {
    bps_of(notional, fee_bps)
}

/// Gross edge in bps of buying at `buy_px` and selling at `sell_px`.
/// Returns 0 when buy price is non-positive (caller must reject empty books first).
pub fn edge_bps(buy_px: Decimal, sell_px: Decimal) -> Decimal {
    if buy_px <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (sell_px - buy_px) / buy_px * Decimal::new(10_000, 0)
}

/// Net edge after taker fees on both legs and a slippage buffer.
pub fn net_edge_bps(
    buy_px: Decimal,
    sell_px: Decimal,
    buy_fee_bps: Decimal,
    sell_fee_bps: Decimal,
    slip_buffer_bps: Decimal,
) -> Decimal {
    edge_bps(buy_px, sell_px) - buy_fee_bps - sell_fee_bps - slip_buffer_bps
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn bps_math() {
        assert_eq!(bps_of(dec!(10000), dec!(25)), dec!(25));
        assert_eq!(edge_bps(dec!(100), dec!(101)), dec!(100));
        assert_eq!(edge_bps(dec!(0), dec!(101)), dec!(0));
    }

    #[test]
    fn net_edge_subtracts_costs() {
        // 100 -> 100.50 is +50bps gross; 10+10 fees + 5 slip = +25 net.
        assert_eq!(
            net_edge_bps(dec!(100), dec!(100.50), dec!(10), dec!(10), dec!(5)),
            dec!(25)
        );
        // Fees can flip a trade negative.
        assert!(net_edge_bps(dec!(100), dec!(100.10), dec!(10), dec!(10), dec!(5)) < dec!(0));
    }
}
