//! Paper executor: fills intents against live books.
//! Fills walk the book (VWAP) and pay taker fees, so paper PnL is honest
//! about depth. A leg that cannot fill aborts the whole intent — in live
//! trading that maps to the hedge/rollback path in `arb-exec`.

use arb_core::{fee_amount, Balances, Fill, Intent, MarketKind, OrderBook, Side, Symbol, Venue};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct PaperConfig {
    pub fee_bps_override: Option<Decimal>,
    pub extra_slip_bps: Decimal,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            fee_bps_override: None,
            extra_slip_bps: Decimal::new(2, 0),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaperFill {
    pub fills: Vec<Fill>,
    /// Realized PnL in quote terms for closed round-trip intents, else None.
    pub realized_pnl_quote: Option<Decimal>,
}

pub struct PaperExecutor {
    pub cfg: PaperConfig,
    pub realized_pnl: Decimal,
    pub trades: u64,
    /// Simulated free balances per venue/asset. Spot fills debit/credit these;
    /// a leg the balance cannot fund aborts the intent (no partial mutation).
    pub balances: Balances,
    /// Mark-to-market equity right after seeding. Unrealized P/L is measured
    /// against this: equity now minus baseline minus realized.
    pub starting_equity: Decimal,
}

impl Default for PaperExecutor {
    fn default() -> Self {
        Self::new(PaperConfig::default())
    }
}

impl PaperExecutor {
    pub fn new(cfg: PaperConfig) -> Self {
        Self {
            cfg,
            realized_pnl: Decimal::ZERO,
            trades: 0,
            balances: Balances::new(),
            starting_equity: Decimal::ZERO,
        }
    }

    /// Whole ledger, for the portfolio view. Clone is fine: called once a tick.
    pub fn balances_snapshot(&self) -> Balances {
        self.balances.clone()
    }

    /// Mark-to-market equity in quote terms. Assets with no mark are skipped
    /// (stale venue, unknown token) so a missing feed never invents value.
    pub fn equity_quote(&self, marks: &BTreeMap<String, Decimal>) -> Decimal {
        self.balances
            .values()
            .flat_map(|m| m.iter())
            .map(|(asset, amt)| marks.get(asset).copied().unwrap_or(Decimal::ZERO) * *amt)
            .sum()
    }

    /// Open-position P/L: what the ledger gained or lost beyond closed trades.
    pub fn unrealized_pnl_quote(&self, marks: &BTreeMap<String, Decimal>) -> Decimal {
        self.equity_quote(marks) - self.starting_equity - self.realized_pnl
    }

    /// Credit a simulated balance (engine seeds these from config at startup).
    pub fn seed(&mut self, venue: Venue, asset: &str, amount: Decimal) {
        *self
            .balances
            .entry(venue)
            .or_default()
            .entry(asset.to_uppercase())
            .or_default() += amount;
    }

    pub fn balance(&self, venue: Venue, asset: &str) -> Decimal {
        self.balances
            .get(&venue)
            .and_then(|m| m.get(&asset.to_uppercase()))
            .copied()
            .unwrap_or_default()
    }

    fn fee_bps(&self, v: Venue) -> Decimal {
        self.cfg
            .fee_bps_override
            .unwrap_or_else(|| v.default_taker_bps())
    }

    /// Execute all legs against `books` keyed by (venue, symbol-as-string).
    pub fn execute(
        &mut self,
        intent: &Intent,
        books: &BTreeMap<(Venue, String), OrderBook>,
        ts_ms: i64,
    ) -> Result<PaperFill, String> {
        // Work on a cloned ledger: any failed leg aborts with balances untouched.
        let mut ledger = self.balances.clone();
        let mut fills = Vec::new();
        for leg in &intent.legs {
            let px = match leg.market {
                // Perp fills at mark (no book to walk) with the same slip
                // haircut as spot, so funding entries do not understate cost.
                MarketKind::Perp => {
                    let slip = leg.price * self.cfg.extra_slip_bps / Decimal::new(10_000, 0);
                    match leg.side {
                        Side::Buy => leg.price + slip,
                        Side::Sell => leg.price - slip,
                    }
                }
                MarketKind::Spot => {
                    // DEX legs fill at their quote price: AMM/aggregator
                    // quotes are already size-aware executable prices, and
                    // DEX venues never have an order book to walk.
                    let mut px = if leg.venue.is_dex() {
                        leg.price
                    } else {
                        let book = books
                            .get(&(leg.venue, leg.symbol.to_string()))
                            .ok_or_else(|| format!("no book for {} {}", leg.venue, leg.symbol))?;
                        book.vwap(leg.side, leg.qty_base)
                            .map_err(|e| e.to_string())?
                    };
                    // Extra slippage haircut: buys pay up, sells receive less.
                    let slip = px * self.cfg.extra_slip_bps / Decimal::new(10_000, 0);
                    px = match leg.side {
                        Side::Buy => px + slip,
                        Side::Sell => px - slip,
                    };
                    px
                }
            };
            let fee = fee_amount(px * leg.qty_base, self.fee_bps(leg.venue));
            apply_to_ledger(&mut ledger, leg, px, fee)?;
            fills.push(Fill {
                venue: leg.venue,
                symbol: leg.symbol.clone(),
                side: leg.side,
                price: px,
                qty_base: leg.qty_base,
                fee_quote: fee,
                ts_ms,
                simulated: true,
                market: leg.market,
            });
        }
        self.balances = ledger;
        // Position-opening intents (e.g. funding entries) accrue PnL over time;
        // never mark them as an instant round-trip.
        let pnl = if intent.roundtrip {
            roundtrip_pnl(&fills)
        } else {
            None
        };
        if let Some(p) = pnl {
            self.realized_pnl += p;
        }
        self.trades += 1;
        Ok(PaperFill {
            fills,
            realized_pnl_quote: pnl,
        })
    }
}

fn bal(ledger: &Balances, venue: Venue, asset: &str) -> Decimal {
    ledger
        .get(&venue)
        .and_then(|m| m.get(asset))
        .copied()
        .unwrap_or_default()
}

fn adjust(ledger: &mut Balances, venue: Venue, asset: &str, delta: Decimal) {
    *ledger
        .entry(venue)
        .or_default()
        .entry(asset.into())
        .or_default() += delta;
}

/// Debit/credit one leg. Spot moves both assets; perp only pays the fee from
/// quote margin (position tracking itself lives with the strategy/exec layer).
fn apply_to_ledger(
    ledger: &mut Balances,
    leg: &arb_core::TradeLeg,
    px: Decimal,
    fee: Decimal,
) -> Result<(), String> {
    let notional = px * leg.qty_base;
    let (base, quote) = (leg.symbol.base.clone(), leg.symbol.quote.clone());
    match leg.market {
        MarketKind::Perp => {
            if bal(ledger, leg.venue, &quote) < fee {
                return Err(format!(
                    "insufficient {quote} margin on {} for perp fee {fee}",
                    leg.venue
                ));
            }
            adjust(ledger, leg.venue, &quote, -fee);
        }
        MarketKind::Spot => match leg.side {
            Side::Buy => {
                let need = notional + fee;
                if bal(ledger, leg.venue, &quote) < need {
                    return Err(format!(
                        "insufficient {quote} on {}: need {need}, have {}",
                        leg.venue,
                        bal(ledger, leg.venue, &quote)
                    ));
                }
                adjust(ledger, leg.venue, &quote, -need);
                adjust(ledger, leg.venue, &base, leg.qty_base);
            }
            Side::Sell => {
                if bal(ledger, leg.venue, &base) < leg.qty_base {
                    return Err(format!(
                        "insufficient {base} on {}: need {}, have {}",
                        leg.venue,
                        leg.qty_base,
                        bal(ledger, leg.venue, &base)
                    ));
                }
                adjust(ledger, leg.venue, &base, -leg.qty_base);
                adjust(ledger, leg.venue, &quote, notional - fee);
            }
        },
    }
    Ok(())
}

/// PnL for closed cycles, tracked per asset (both legs of every pair).
/// A buy adds base and spends quote (+fee); a sell does the reverse.
/// When every intermediate asset nets to ~zero and exactly one asset (the
/// numeraire, e.g. USDT) remains, that remainder is realized PnL. This
/// handles cross-exchange (2 legs, same base) and triangular cycles
/// (BTC and ETH flatten, USDT keeps the profit). Open intents return None.
pub fn roundtrip_pnl(fills: &[Fill]) -> Option<Decimal> {
    if fills.is_empty() {
        return None;
    }
    let mut net: BTreeMap<String, Decimal> = BTreeMap::new();
    for f in fills {
        if f.market == MarketKind::Perp {
            // Perp legs move no spot assets; only their fee hits the numeraire.
            *net.entry(f.symbol.quote.clone()).or_default() -= f.fee_quote;
            continue;
        }
        let notional = f.price * f.qty_base;
        match f.side {
            Side::Buy => {
                *net.entry(f.symbol.base.clone()).or_default() += f.qty_base;
                *net.entry(f.symbol.quote.clone()).or_default() -= notional + f.fee_quote;
            }
            Side::Sell => {
                *net.entry(f.symbol.base.clone()).or_default() -= f.qty_base;
                *net.entry(f.symbol.quote.clone()).or_default() += notional - f.fee_quote;
            }
        }
    }
    let eps = Decimal::new(1, 8);
    let nonzero: Vec<Decimal> = net.values().copied().filter(|v| v.abs() >= eps).collect();
    match nonzero.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

pub fn book_key(v: Venue, s: &Symbol) -> (Venue, String) {
    (v, s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, TradeLeg};
    use rust_decimal_macros::dec;

    fn books() -> BTreeMap<(Venue, String), OrderBook> {
        let s = Symbol::new("BTC", "USDT");
        let mut cheap = OrderBook::new(Venue::Binance, s.clone());
        cheap.apply_snapshot(
            vec![BookLevel {
                price: dec!(99),
                qty: dec!(5),
            }],
            vec![BookLevel {
                price: dec!(100),
                qty: dec!(5),
            }],
            1,
            0,
        );
        let mut rich = OrderBook::new(Venue::Bybit, s.clone());
        rich.apply_snapshot(
            vec![BookLevel {
                price: dec!(102),
                qty: dec!(5),
            }],
            vec![BookLevel {
                price: dec!(103),
                qty: dec!(5),
            }],
            1,
            0,
        );
        BTreeMap::from([
            (book_key(Venue::Binance, &s), cheap),
            (book_key(Venue::Bybit, &s), rich),
        ])
    }

    #[test]
    fn equity_and_unrealized_measure_against_baseline() {
        let mut p = PaperExecutor::default();
        p.seed(Venue::Binance, "USDT", dec!(1000));
        p.seed(Venue::Binance, "BTC", dec!(1));
        let marks = BTreeMap::from([
            ("USDT".to_string(), dec!(1)),
            ("BTC".to_string(), dec!(100)),
        ]);
        p.starting_equity = p.equity_quote(&marks);
        assert_eq!(p.starting_equity, dec!(1100));
        // BTC doubles with no closed trades: all gain is unrealized.
        let up = BTreeMap::from([
            ("USDT".to_string(), dec!(1)),
            ("BTC".to_string(), dec!(200)),
        ]);
        assert_eq!(p.equity_quote(&up), dec!(1200));
        assert_eq!(p.unrealized_pnl_quote(&up), dec!(100));
        // Unknown assets mark at zero, never invented.
        p.seed(Venue::Binance, "MYST", dec!(9));
        assert_eq!(p.equity_quote(&up), dec!(1200));
    }

    #[test]
    fn paper_roundtrip_pnl_positive_net_of_fees() {
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "x".into(),
            expected_edge_bps: dec!(100),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(102),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
            ],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Bybit, "BTC", dec!(10));
        let r = ex.execute(&intent, &books(), 10).unwrap();
        // Gross +2. Fees: 0.1% of 100 + 0.1% of 102 = 0.202. Net = 1.798.
        assert_eq!(r.realized_pnl_quote, Some(dec!(1.798)));
        assert_eq!(ex.realized_pnl, dec!(1.798));
    }

    #[test]
    fn triangular_cycle_pnl_nets_intermediates() {
        // USDT -> 1 BTC @100 -> 20 ETH @0.05 BTC -> sell ETH @5.30 USDT.
        // BTC and ETH must flatten; USDT remainder = 106 - 100 = 6 (no fees).
        let f = |base: &str, quote: &str, side: Side, px: Decimal, qty: Decimal| Fill {
            venue: Venue::Binance,
            symbol: Symbol::new(base, quote),
            side,
            price: px,
            qty_base: qty,
            fee_quote: dec!(0),
            ts_ms: 0,
            simulated: true,
            market: MarketKind::Spot,
        };
        let fills = vec![
            f("BTC", "USDT", Side::Buy, dec!(100), dec!(1)),
            f("ETH", "BTC", Side::Buy, dec!(0.05), dec!(20)),
            f("ETH", "USDT", Side::Sell, dec!(5.30), dec!(20)),
        ];
        assert_eq!(roundtrip_pnl(&fills), Some(dec!(6)));
        // Dropping the last leg leaves ETH open -> no realized PnL.
        assert_eq!(roundtrip_pnl(&fills[..2]), None);
    }

    #[test]
    fn open_position_intent_has_no_instant_pnl() {
        // Regression: funding entries (buy spot + sell perp on one book) were
        // booked as instant round-trips, fabricating a spread+fees loss.
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "funding_rate".into(),
            expected_edge_bps: dec!(500),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: false,
            legs: vec![
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
            ],
        };
        let mut ex = PaperExecutor::default();
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        let r = ex.execute(&intent, &books(), 10).unwrap();
        assert_eq!(r.realized_pnl_quote, None);
        assert_eq!(ex.realized_pnl, dec!(0));
    }

    #[test]
    fn insufficient_balance_aborts_atomically() {
        // Selling an asset you do not hold must fail, leaving balances intact.
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "x".into(),
            expected_edge_bps: dec!(100),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(102),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
            ],
        };
        let mut ex = PaperExecutor::default();
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        // Deliberately no BTC on Bybit to sell.
        let err = ex.execute(&intent, &books(), 10).unwrap_err();
        assert!(err.contains("insufficient BTC"), "got: {err}");
        // First leg must not have debited anything (clone-commit ledger).
        assert_eq!(ex.balance(Venue::Binance, "USDT"), dec!(10000));
        assert_eq!(ex.balance(Venue::Binance, "BTC"), dec!(0));
        assert_eq!(ex.trades, 0);
    }

    #[test]
    fn perp_leg_fills_without_spot_book_and_pays_fee_from_margin() {
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "funding_rate".into(),
            expected_edge_bps: dec!(500),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: false,
            legs: vec![TradeLeg {
                venue: Venue::Bybit,
                symbol: s,
                side: Side::Sell,
                price: dec!(100),
                qty_base: dec!(2),
                market: MarketKind::Perp,
            }],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
        });
        ex.seed(Venue::Bybit, "USDT", dec!(1000));
        // Empty book map: perp leg must not need one.
        let r = ex.execute(&intent, &BTreeMap::new(), 10).unwrap();
        assert_eq!(r.fills[0].market, MarketKind::Perp);
        assert_eq!(r.fills[0].price, dec!(100));
        // Fee = 0.1% of 200 = 0.2 debited from margin; no BTC created or owed.
        assert_eq!(ex.balance(Venue::Bybit, "USDT"), dec!(999.8));
        assert_eq!(ex.balance(Venue::Bybit, "BTC"), dec!(0));
    }

    #[test]
    fn perp_leg_pays_slip_haircut() {
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "funding_rate".into(),
            expected_edge_bps: dec!(500),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: false,
            legs: vec![TradeLeg {
                venue: Venue::Bybit,
                symbol: s,
                side: Side::Sell,
                price: dec!(100),
                qty_base: dec!(2),
                market: MarketKind::Perp,
            }],
        };
        // Default 2bps slip: short fills 2bps below mark.
        let mut ex = PaperExecutor::default();
        ex.seed(Venue::Bybit, "USDT", dec!(1000));
        let r = ex.execute(&intent, &BTreeMap::new(), 10).unwrap();
        assert_eq!(r.fills[0].price, dec!(99.98));
    }

    #[test]
    fn missing_book_aborts() {
        let s = Symbol::new("ETH", "USDT");
        let intent = Intent {
            strategy: "x".into(),
            expected_edge_bps: dec!(100),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![TradeLeg {
                venue: Venue::Binance,
                symbol: s,
                side: Side::Buy,
                price: dec!(1),
                qty_base: dec!(1),
                market: MarketKind::Spot,
            }],
        };
        let mut ex = PaperExecutor::default();
        assert!(ex.execute(&intent, &books(), 0).is_err());
    }

    #[test]
    fn dex_spot_leg_fills_at_quote_price_without_book() {
        // Regression: cex_dex intents always fill-failed with "no book"
        // because DEX venues never have an OrderBook. DEX legs must fill
        // at their (already size-aware) quote price.
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "cex_dex".into(),
            expected_edge_bps: dec!(100),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Jupiter,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(102),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
            ],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Jupiter, "BTC", dec!(10));
        // books() holds no Jupiter book, by construction.
        let r = ex.execute(&intent, &books(), 10).unwrap();
        assert_eq!(r.fills[1].venue, Venue::Jupiter);
        assert_eq!(r.fills[1].price, dec!(102));
        // Gross +2. Fees: 0.1% of 100 + 0.1% of 102 = 0.202. Net = 1.798.
        assert_eq!(r.realized_pnl_quote, Some(dec!(1.798)));
        assert_eq!(ex.balance(Venue::Jupiter, "BTC"), dec!(9));
        assert_eq!(ex.balance(Venue::Jupiter, "USDT"), dec!(101.898));
        assert_eq!(ex.trades, 1);
    }
}
