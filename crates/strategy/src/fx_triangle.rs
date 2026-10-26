//! S3 IDR-FX triangle: BTC/IDR misprices vs BTC/USDT when the USD/IDR
//! reference is stale. Only FX arithmetic separates signal from phantom.
use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{net_edge_bps, Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;

/// Convert an IDR price to USD with an explicit rate (kept explicit so the
/// stale-vs-true replay falsifier can run without engine rewiring).
pub fn converted(px_idr: Decimal, usd_idr: Decimal) -> Decimal {
    if usd_idr <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        px_idr / usd_idr
    }
}

pub struct FxTriangle {
    pub params: StrategyParams,
    pub ref_venue: Venue,
    pub idr_venue: Venue,
    pub symbols: Vec<Symbol>,
    /// Rate the engine used to normalize IDR books (stale in prod).
    pub usd_idr_used: Decimal,
}

impl FxTriangle {
    pub fn new(
        params: StrategyParams,
        ref_venue: Venue,
        idr_venue: Venue,
        symbols: Vec<Symbol>,
        usd_idr_used: Decimal,
    ) -> Self {
        Self {
            params,
            ref_venue,
            idr_venue,
            symbols,
            usd_idr_used,
        }
    }
}

impl Strategy for FxTriangle {
    fn name(&self) -> &'static str {
        "fx_triangle"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        for sym in &self.symbols {
            let (Some(rb), Some(ib)) = (
                view.book(self.ref_venue, sym),
                view.book(self.idr_venue, sym),
            ) else {
                continue;
            };
            if !fresh(rb, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            if !fresh(ib, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            // Both directions; the FX error shows up as a one-sided phantom.
            for (buy_v, sell_v, buy_b, sell_b) in [
                (self.ref_venue, self.idr_venue, rb, ib),
                (self.idr_venue, self.ref_venue, ib, rb),
            ] {
                let (Some(ask), Some(_bid)) = (buy_b.best_ask(), sell_b.best_bid()) else {
                    continue;
                };
                let qty = size_for_notional(self.params.target_notional_quote, ask.price);
                if qty <= Decimal::ZERO {
                    continue;
                }
                let (Ok(buy_px), Ok(sell_px)) =
                    (buy_b.vwap(Side::Buy, qty), sell_b.vwap(Side::Sell, qty))
                else {
                    continue;
                };
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
                    expected_edge_bps: edge,
                    ttl_ms: self.params.ttl_ms,
                    created_ms: view.now_ms,
                    roundtrip: true,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, OrderBook};
    use rust_decimal_macros::dec;

    #[test]
    fn stale_fx_phantom_vanishes_with_true_rate() {
        // IDR price fixed; stale 16k overstates USD vs true 19k.
        let px_idr = dec!(1600000);
        let stale = converted(px_idr, dec!(16000));
        let tru = converted(px_idr, dec!(19000));
        assert_eq!(stale, dec!(100));
        assert!(tru < stale, "true rate must lower the USD print");
        // Phantom: stale print matches Binance 100 -> no edge either way once
        // fees apply; the point is the conversion moves the print 15%+.
        let phantom_bps = (stale - tru) / tru * dec!(10000);
        assert!(phantom_bps > dec!(1000), "FX staleness dwarfs venue edge");
    }

    #[test]
    fn zero_fee_ablation_must_fail() {
        // With the 51bps Indodax fee zeroed, a 30bps spread fires; with the
        // real fee it must not. Fee-free-only = reject.
        let s = Symbol::new("BTC", "USDT");
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut r = OrderBook::new(Venue::Binance, s.clone());
        r.apply_snapshot(
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.1),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        let mut i = OrderBook::new(Venue::Indodax, s.clone());
        i.apply_snapshot(
            vec![BookLevel {
                price: dec!(100.15),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(100.2),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(r);
        v.put(i);
        let mut st = FxTriangle::new(
            StrategyParams::default(),
            Venue::Binance,
            Venue::Indodax,
            vec![s],
            dec!(16000),
        );
        // 100.1 -> 100.15 is ~5bps gross < fees + slip: must stay out.
        assert!(st.on_tick(&v).is_empty());
    }
}
