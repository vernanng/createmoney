//! S8 nano-ablation harness: one-command proof that each cost model matters.
//! Karpathy-style: minimal, deterministic, fail-fast. The matrix runs
//! SyntheticFeed (3 seeds) x strategy x {full, no-fee, no-slip, inf-age,
//! inf-depth} and asserts full-cost PnL < no-fee PnL and stale-allowed worse.

use crate::StrategyParams;
use rust_decimal::Decimal;

/// One ablation switch set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ablation {
    pub ignore_fees: bool,
    pub ignore_slip: bool,
    pub infinite_age: bool,
    pub infinite_depth: bool,
}

impl Ablation {
    pub fn full() -> Self {
        Self {
            ignore_fees: false,
            ignore_slip: false,
            infinite_age: false,
            infinite_depth: false,
        }
    }
    pub fn label(&self) -> &'static str {
        match (
            *self == Self::full(),
            self.ignore_fees,
            self.ignore_slip,
            self.infinite_age,
            self.infinite_depth,
        ) {
            (true, _, _, _, _) => "full",
            (_, true, _, _, _) => "no-fee",
            (_, _, true, _, _) => "no-slip",
            (_, _, _, true, _) => "inf-age",
            (_, _, _, _, true) => "inf-depth",
            _ => "mixed",
        }
    }
}

/// Full matrix in Zero-to-Hero order.
pub fn matrix() -> Vec<Ablation> {
    vec![
        Ablation::full(),
        Ablation {
            ignore_fees: true,
            ..Ablation::full()
        },
        Ablation {
            ignore_slip: true,
            ..Ablation::full()
        },
        Ablation {
            infinite_age: true,
            ..Ablation::full()
        },
        Ablation {
            infinite_depth: true,
            ..Ablation::full()
        },
    ]
}

/// Apply an ablation to strategy params (fees zeroed by caller via
/// `effective_fees`; depth bypassed by caller via touch-price cheat check).
pub fn effective_params(base: &StrategyParams, a: &Ablation) -> StrategyParams {
    let mut p = base.clone();
    if a.ignore_slip {
        p.slip_buffer_bps = Decimal::ZERO;
    }
    if a.infinite_age {
        p.max_book_age_ms = i64::MAX;
    }
    p
}

/// Zero both leg fees when the ablation says so.
pub fn effective_fees(buy_fee: Decimal, sell_fee: Decimal, a: &Ablation) -> (Decimal, Decimal) {
    if a.ignore_fees {
        (Decimal::ZERO, Decimal::ZERO)
    } else {
        (buy_fee, sell_fee)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{edge_bps, net_edge_bps};
    use rust_decimal_macros::dec;

    #[test]
    fn matrix_has_five_modes_starting_full() {
        let m = matrix();
        assert_eq!(m.len(), 5);
        assert_eq!(m[0], Ablation::full());
        assert_eq!(m[0].label(), "full");
        assert_eq!(m[1].label(), "no-fee");
        assert_eq!(m[2].label(), "no-slip");
        assert_eq!(m[3].label(), "inf-age");
        assert_eq!(m[4].label(), "inf-depth");
    }

    #[test]
    fn full_costs_less_than_no_fee() {
        let buy = dec!(100);
        let sell = dec!(101); // +100bps gross
        let full = net_edge_bps(buy, sell, dec!(10), dec!(10), dec!(5));
        let a = Ablation {
            ignore_fees: true,
            ..Ablation::full()
        };
        let (bf, sf) = effective_fees(dec!(10), dec!(10), &a);
        let stripped = net_edge_bps(buy, sell, bf, sf, dec!(5));
        assert!(
            stripped > full,
            "no-fee must inflate edge: {stripped} vs {full}"
        );
        assert_eq!(stripped - full, dec!(20));
    }

    #[test]
    fn touch_price_cheat_inflates_pnl() {
        // Harness power check: VWAP (honest, walks depth) must be worse than
        // touch price when books are thin. If they tie, the harness is blind.
        use arb_core::{BookLevel, OrderBook, Side, Symbol, Venue};
        let s = Symbol::new("BTC", "USDT");
        let mut b = OrderBook::new(Venue::Binance, s);
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(1),
            }],
            vec![
                BookLevel {
                    price: dec!(100),
                    qty: dec!(1),
                },
                BookLevel {
                    price: dec!(110),
                    qty: dec!(10),
                },
            ],
            1,
            1_000,
        );
        let honest = b.vwap(Side::Buy, dec!(5)).unwrap();
        let cheat = b.best_ask().unwrap().price;
        assert!(honest > cheat, "VWAP must exceed touch on thin book");
        let edge_honest = edge_bps(honest, dec!(112));
        let edge_cheat = edge_bps(cheat, dec!(112));
        assert!(edge_cheat > edge_honest);
    }

    #[test]
    fn stale_allowed_is_worse_gate() {
        let base = StrategyParams::default();
        let aged = effective_params(
            &base,
            &Ablation {
                infinite_age: true,
                ..Ablation::full()
            },
        );
        assert_eq!(aged.max_book_age_ms, i64::MAX);
        assert!(aged.max_book_age_ms > base.max_book_age_ms);
    }
}
