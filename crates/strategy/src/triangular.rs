//! Triangular arb on one venue: A -> B -> C -> A cycles.
//! Classic form: start in USDT, buy BTC (BTC/USDT), sell BTC for ETH via
//! ETH/BTC inverted, sell ETH (ETH/USDT). We model each cycle as a chain of
//! (symbol, side) conversions and price the whole chain off the books.

use crate::{fresh, MarketView, Strategy, StrategyParams};
use arb_core::{Intent, MarketKind, OrderBook, Side, Symbol, TradeLeg, Venue};
use rust_decimal::Decimal;

/// One conversion step: trade `symbol` on `side`.
#[derive(Debug, Clone)]
pub struct Step {
    pub symbol: Symbol,
    pub side: Side,
}

/// A closed cycle starting/ending in `start_asset` (usually the quote, USDT).
#[derive(Debug, Clone)]
pub struct Cycle {
    pub steps: Vec<Step>,
}

pub struct Triangular {
    pub params: StrategyParams,
    pub venues: Vec<Venue>,
    pub cycles: Vec<Cycle>,
}

impl Triangular {
    pub fn new(params: StrategyParams, venues: Vec<Venue>, cycles: Vec<Cycle>) -> Self {
        Self {
            params,
            venues,
            cycles,
        }
    }

    /// BTC/ETH triangle in both directions.
    pub fn default_cycles() -> Vec<Cycle> {
        let btc_usdt = Symbol::new("BTC", "USDT");
        let eth_btc = Symbol::new("ETH", "BTC");
        let eth_usdt = Symbol::new("ETH", "USDT");
        vec![
            // USDT -> BTC -> ETH -> USDT
            Cycle {
                steps: vec![
                    Step {
                        symbol: btc_usdt.clone(),
                        side: Side::Buy,
                    },
                    Step {
                        symbol: eth_btc.clone(),
                        side: Side::Buy,
                    },
                    Step {
                        symbol: eth_usdt.clone(),
                        side: Side::Sell,
                    },
                ],
            },
            // USDT -> ETH -> BTC -> USDT
            Cycle {
                steps: vec![
                    Step {
                        symbol: eth_usdt,
                        side: Side::Buy,
                    },
                    Step {
                        symbol: eth_btc,
                        side: Side::Sell,
                    },
                    Step {
                        symbol: btc_usdt,
                        side: Side::Sell,
                    },
                ],
            },
        ]
    }

    /// Price the cycle: returns (end/start ratio, legs) for a start amount.
    /// Each step converts the running asset amount using book VWAP-less touch
    /// pricing scaled to the running qty, with taker fee deducted per step.
    fn price_cycle(
        &self,
        venue: Venue,
        cycle: &Cycle,
        view: &MarketView,
        start_quote: Decimal,
    ) -> Option<(Decimal, Vec<TradeLeg>)> {
        let fee_frac = venue.default_taker_bps() / Decimal::new(10_000, 0);
        let mut amount = start_quote; // in current asset terms (quote of next step, or base after conversion)
        let mut legs = Vec::new();
        for step in &cycle.steps {
            let book = view.book(venue, &step.symbol)?;
            if !fresh(book, view.now_ms, self.params.max_book_age_ms) {
                return None;
            }
            match step.side {
                Side::Buy => {
                    // amount is in quote; base received = amount / vwap_ask(qty) approx via touch then verify depth.
                    let ask = book.best_ask()?;
                    let qty = amount / ask.price;
                    let px = vwap_or(book, Side::Buy, qty)?;
                    legs.push(TradeLeg {
                        venue,
                        symbol: step.symbol.clone(),
                        side: Side::Buy,
                        price: px,
                        qty_base: qty,
                        market: MarketKind::Spot,
                    });
                    amount = qty * (Decimal::ONE - fee_frac);
                }
                Side::Sell => {
                    // amount is in base; quote received = amount * vwap_bid.
                    let px = vwap_or(book, Side::Sell, amount)?;
                    legs.push(TradeLeg {
                        venue,
                        symbol: step.symbol.clone(),
                        side: Side::Sell,
                        price: px,
                        qty_base: amount,
                        market: MarketKind::Spot,
                    });
                    amount = amount * px * (Decimal::ONE - fee_frac);
                }
            }
        }
        if start_quote <= Decimal::ZERO {
            return None;
        }
        Some((amount / start_quote, legs))
    }
}

fn vwap_or(book: &OrderBook, side: Side, qty: Decimal) -> Option<Decimal> {
    book.vwap(side, qty).ok()
}

impl Strategy for Triangular {
    fn name(&self) -> &'static str {
        "triangular"
    }

    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent> {
        let mut out = Vec::new();
        let min_ratio = Decimal::ONE
            + (self.params.min_edge_bps + self.params.slip_buffer_bps) / Decimal::new(10_000, 0);
        for venue in &self.venues {
            for cycle in &self.cycles {
                let Some((ratio, legs)) =
                    self.price_cycle(*venue, cycle, view, self.params.target_notional_quote)
                else {
                    continue;
                };
                if ratio < min_ratio {
                    continue;
                }
                let edge_bps = (ratio - Decimal::ONE) * Decimal::new(10_000, 0);
                out.push(Intent {
                    strategy: self.name().into(),
                    legs,
                    expected_edge_bps: edge_bps,
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
    use arb_core::BookLevel;
    use rust_decimal_macros::dec;

    fn book(v: Venue, s: Symbol, bid: Decimal, ask: Decimal) -> OrderBook {
        let mut b = OrderBook::new(v, s);
        b.apply_snapshot(
            vec![BookLevel {
                price: bid,
                qty: dec!(100000),
            }],
            vec![BookLevel {
                price: ask,
                qty: dec!(100000),
            }],
            1,
            1_000,
        );
        b
    }

    #[test]
    fn profitable_cycle_fires_and_fair_cycle_does_not() {
        let btc = Symbol::new("BTC", "USDT");
        let ethbtc = Symbol::new("ETH", "BTC");
        let eth = Symbol::new("ETH", "USDT");
        let params = StrategyParams {
            min_edge_bps: dec!(10),
            ..Default::default()
        };
        let mut st = Triangular::new(params, vec![Venue::Binance], Triangular::default_cycles());

        // Fair: BTC=100, ETH/BTC=0.05 => ETH fair = 5. Quoting ETH at 5 -> no edge.
        let mut fair = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        fair.put(book(Venue::Binance, btc.clone(), dec!(99.99), dec!(100.01)));
        fair.put(book(
            Venue::Binance,
            ethbtc.clone(),
            dec!(0.0499),
            dec!(0.0501),
        ));
        fair.put(book(Venue::Binance, eth.clone(), dec!(4.999), dec!(5.001)));
        assert!(st.on_tick(&fair).is_empty(), "fair triangle must not fire");

        // Mispriced: ETH/USDT bid lifted to 5.30 (+6%) -> USDT->BTC->ETH->USDT prints.
        let mut hot = MarketView {
            now_ms: 1_000,
            ..Default::default()
        };
        hot.put(book(Venue::Binance, btc, dec!(99.99), dec!(100.01)));
        hot.put(book(Venue::Binance, ethbtc, dec!(0.0499), dec!(0.0501)));
        hot.put(book(Venue::Binance, eth, dec!(5.30), dec!(5.31)));
        let hits = st.on_tick(&hot);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].legs.len(), 3);
        assert!(hits[0].expected_edge_bps > dec!(100));
    }
}
