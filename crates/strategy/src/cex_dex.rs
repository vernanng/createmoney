//! CEX <-> DEX spread. DEX side comes in as a firm-ish quote (pool or
//! aggregator) already net of pool fees; gas/priority fee is charged here.

use crate::{fresh, size_for_notional, MarketView, Strategy, StrategyParams};
use arb_core::{net_edge_bps, Intent, MarketKind, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DexQuote {
    pub venue: Venue, // Uniswap | Pancakeswap | Jupiter
    pub symbol: Symbol,
    /// Effective price to BUY base on the DEX (quote per base, incl. pool fee + impact).
    pub buy_px: Decimal,
    /// Effective price to SELL base on the DEX.
    pub sell_px: Decimal,
    /// Estimated gas / priority fee in quote terms for one swap.
    pub gas_quote: Decimal,
    pub ts_ms: i64,
}

pub struct CexDex {
    pub params: StrategyParams,
    pub cex_venues: Vec<Venue>,
    pub symbols: Vec<Symbol>,
}

impl CexDex {
    pub fn new(params: StrategyParams, cex_venues: Vec<Venue>, symbols: Vec<Symbol>) -> Self {
        Self {
            params,
            cex_venues,
            symbols,
        }
    }

    /// S5 multi-size sweep: try 0.1/0.5/1.0x of target, keep the best net
    /// edge including per-size gas. Small sizes win where V2 impact < spread.
    fn try_pair(&self, view: &MarketView, sym: &Symbol, dq: &DexQuote) -> Option<Intent> {
        if view.now_ms - dq.ts_ms > self.params.dex_max_quote_age_ms {
            return None;
        }
        let mut best: Option<(Decimal, Intent)> = None; // (edge, intent)
        let mut consider = |edge: Decimal, legs: [TradeLeg; 2]| {
            if edge >= self.params.min_edge_bps
                && best.as_ref().map(|(e, _)| edge > *e).unwrap_or(true)
            {
                best = Some((edge, intent(self, view, sym, legs, edge)));
            }
        };
        for cv in &self.cex_venues {
            // A missing/empty book on one venue must skip that venue only —
            // never abort the whole scan (`?` here was a live bug).
            let Some(book) = view.book(*cv, sym) else {
                continue;
            };
            if !fresh(book, view.now_ms, self.params.max_book_age_ms) {
                continue;
            }
            let (Some(_bid), Some(_ask)) = (book.best_bid(), book.best_ask()) else {
                continue;
            };
            for frac in [Decimal::new(1, 1), Decimal::new(5, 1), Decimal::ONE] {
                let sweep_notional = self.params.target_notional_quote * frac;
                if sweep_notional <= Decimal::ZERO {
                    continue;
                }
                // Direction 1: buy CEX ask, sell DEX.
                if let Some(ask) = book.best_ask() {
                    let qty = size_for_notional(sweep_notional, ask.price);
                    if qty > Decimal::ZERO {
                        if let Ok(buy_vwap) = book.vwap(Side::Buy, qty) {
                            let gas_bps = gas_as_bps(dq.gas_quote, sweep_notional);
                            let edge = net_edge_bps(
                                buy_vwap,
                                dq.sell_px,
                                cv.default_taker_bps(),
                                Decimal::ZERO,
                                self.params.slip_buffer_bps + gas_bps,
                            );
                            consider(
                                edge,
                                [
                                    TradeLeg {
                                        venue: *cv,
                                        symbol: sym.clone(),
                                        side: Side::Buy,
                                        price: buy_vwap,
                                        qty_base: qty,
                                        market: MarketKind::Spot,
                                    },
                                    TradeLeg {
                                        venue: dq.venue,
                                        symbol: sym.clone(),
                                        side: Side::Sell,
                                        price: dq.sell_px,
                                        qty_base: qty,
                                        market: MarketKind::Spot,
                                    },
                                ],
                            );
                        }
                    }
                }
                // Direction 2: buy DEX, sell CEX bid.
                {
                    let qty = size_for_notional(sweep_notional, dq.buy_px);
                    if qty > Decimal::ZERO {
                        if let Ok(sell_vwap) = book.vwap(Side::Sell, qty) {
                            let gas_bps = gas_as_bps(dq.gas_quote, sweep_notional);
                            let edge = net_edge_bps(
                                dq.buy_px,
                                sell_vwap,
                                Decimal::ZERO,
                                cv.default_taker_bps(),
                                self.params.slip_buffer_bps + gas_bps,
                            );
                            consider(
                                edge,
                                [
                                    TradeLeg {
                                        venue: dq.venue,
                                        symbol: sym.clone(),
                                        side: Side::Buy,
                                        price: dq.buy_px,
                                        qty_base: qty,
                                        market: MarketKind::Spot,
                                    },
                                    TradeLeg {
                                        venue: *cv,
                                        symbol: sym.clone(),
                                        side: Side::Sell,
                                        price: sell_vwap,
                                        qty_base: qty,
                                        market: MarketKind::Spot,
                                    },
                                ],
                            );
                        }
                    }
                }
            }
        }
        best.map(|(_, i)| i)
    }
}

fn gas_as_bps(gas_quote: Decimal, notional: Decimal) -> Decimal {
    if notional <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        gas_quote / notional * Decimal::new(10_000, 0)
    }
}

fn intent(
    s: &CexDex,
    view: &MarketView,
    _sym: &Symbol,
    legs: [TradeLeg; 2],
    edge: Decimal,
) -> Intent {
    Intent {
        strategy: "cex_dex".into(),
        legs: legs.into(),
        expected_edge_bps: edge,
        // shortcut: 4x TTL kept stale DEX quotes alive; DEX quotes carry
        // their own ts so the normal TTL plus max_book_age_ms is enough.
        ttl_ms: s.params.ttl_ms,
        created_ms: view.now_ms,
        roundtrip: true,
    }
}

impl Strategy for CexDex {
    fn name(&self) -> &'static str {
        "cex_dex"
    }
    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        for sym in &self.symbols {
            for dq in view.dex_quotes.iter().filter(|q| &q.symbol == sym) {
                if let Some(i) = self.try_pair(view, sym, dq) {
                    out.push(i);
                }
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
    fn gas_kills_thin_edge() {
        let s = Symbol::new("ETH", "USDT");
        let mut st = CexDex::new(
            StrategyParams::default(),
            vec![Venue::Binance],
            vec![s.clone()],
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Binance, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(3000),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(3001),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(b);
        // DEX buys at 3030 (+~100bps vs CEX ask) but gas is $0.50 on $10 notional = 500bps -> dead.
        v.dex_quotes.push(DexQuote {
            venue: Venue::Uniswap,
            symbol: s.clone(),
            buy_px: dec!(3035),
            sell_px: dec!(3030),
            gas_quote: dec!(0.5),
            ts_ms: 1_000,
        });
        assert!(st.on_tick(&v).is_empty());
        // Same prices, L2-style gas $0.005 = 5bps -> fires.
        v.dex_quotes[0].gas_quote = dec!(0.005);
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].legs[1].venue, Venue::Uniswap);
    }

    #[test]
    fn missing_first_venue_book_does_not_hide_second_venue() {
        // Regression: `view.book(...)?` aborted the scan when the FIRST venue
        // had no book, hiding opportunities on every later venue.
        let s = Symbol::new("ETH", "USDT");
        let mut st = CexDex::new(
            StrategyParams::default(),
            vec![Venue::Binance, Venue::Bybit], // Binance book deliberately absent
            vec![s.clone()],
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Bybit, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(3000),
                qty: dec!(10),
            }],
            vec![BookLevel {
                price: dec!(3001),
                qty: dec!(10),
            }],
            1,
            1_000,
        );
        v.put(b);
        v.dex_quotes.push(DexQuote {
            venue: Venue::Uniswap,
            symbol: s,
            buy_px: dec!(3035),
            sell_px: dec!(3030),
            gas_quote: dec!(0.005),
            ts_ms: 1_000,
        });
        let hits = st.on_tick(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].legs[0].venue, Venue::Bybit);
        assert_eq!(hits[0].ttl_ms, StrategyParams::default().ttl_ms);
    }

    #[test]
    fn sweep_prefers_small_size_in_thin_book() {
        // Thin CEX asks: 0.0003 @ 3001 then deep 3100. Full $10 walks into
        // 3100 (edge dies); 0.1x stays near touch (edge lives). Sweep must
        // pick the small size, proving the edge-size curve crosses zero.
        let s = Symbol::new("ETH", "USDT");
        let mut st = CexDex::new(
            StrategyParams::default(),
            vec![Venue::Binance],
            vec![s.clone()],
        );
        let mut v = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        let mut b = OrderBook::new(Venue::Binance, s.clone());
        b.apply_snapshot(
            vec![BookLevel {
                price: dec!(3000),
                qty: dec!(0.1),
            }],
            vec![
                BookLevel {
                    price: dec!(3001),
                    qty: dec!(0.0003),
                },
                BookLevel {
                    price: dec!(3100),
                    qty: dec!(0.1),
                },
            ],
            1,
            1_000,
        );
        v.put(b);
        v.dex_quotes.push(DexQuote {
            venue: Venue::Uniswap,
            symbol: s,
            buy_px: dec!(3060),
            sell_px: dec!(3050),
            gas_quote: dec!(0.005),
            ts_ms: 1_000,
        });
        let hits = st.on_tick(&v);
        assert_eq!(
            hits.len(),
            1,
            "small size must survive where full size dies"
        );
        let qty = hits[0].legs[0].qty_base;
        let full = crate::size_for_notional(dec!(10), dec!(3001));
        assert!(
            qty < full,
            "sweep must pick smaller than full, got {qty} vs {full}"
        );
    }
}
