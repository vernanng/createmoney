//! Paper executor: fills intents against live books.
//! Fills walk the book (VWAP) and pay taker fees, so paper PnL is honest
//! about depth. A leg that cannot fill aborts the whole intent — in live
//! trading that maps to the hedge/rollback path in `arb-exec`.

use arb_core::{fee_amount, Balances, Fill, Intent, MarketKind, OrderBook, Side, Symbol, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct PaperConfig {
    pub fee_bps_override: Option<Decimal>,
    pub extra_slip_bps: Decimal,
    /// Withdrawal-style friction on virtual rebalances: this fee is burned
    /// from the sender, and the receiver cannot spend the funds until
    /// `transfer_delay_ms` has passed.
    pub transfer_fee_bps: Decimal,
    /// Flat per-transfer fee in quote terms (USDT), keyed by SENDING venue.
    /// Overrides the documented schedule below; venues absent here fall back
    /// to it, so an empty map means the official figures apply.
    pub transfer_fees: BTreeMap<Venue, Decimal>,
    pub transfer_delay_ms: i64,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            fee_bps_override: None,
            extra_slip_bps: Decimal::new(2, 0),
            transfer_fee_bps: Decimal::ZERO,
            transfer_fees: BTreeMap::new(),
            transfer_delay_ms: 0,
        }
    }
}

impl PaperConfig {
    /// Documented per-venue flat withdrawal proxy (USDT per transfer).
    /// Binance, Bybit and OKX publish 1 USDT (TRC20 schedules as of
    /// 2026-10-29); Indodax publishes no static figure (fees shown in-app),
    /// so it is set to the same 1 USDT by operator decision. Kraken varies
    /// its USDT fee by network and changes it without notice, so it is also
    /// operator-set to 1 USDT until verified. Re-verify on the withdrawal
    /// screen — exchanges change these without notice.
    pub fn documented_flat_fee(venue: Venue) -> Decimal {
        match venue {
            Venue::Binance | Venue::Bybit | Venue::Okx | Venue::Indodax | Venue::Kraken => {
                Decimal::new(1, 0)
            }
            _ => Decimal::ZERO,
        }
    }

    /// Friction for moving `short` units of `asset` off `venue`, denominated
    /// in `asset`: the flat USDT proxy converted at `px` (quote per base
    /// unit) for non-stable moves, 1:1 for stablecoin moves, plus the
    /// proportional haircut. `px` is the leg's executable price; a
    /// non-positive price degrades to the bps leg only, never a divide-by-zero.
    fn transfer_cost(&self, venue: Venue, asset: &str, short: Decimal, px: Decimal) -> Decimal {
        let flat_quote = self
            .transfer_fees
            .get(&venue)
            .copied()
            .unwrap_or_else(|| Self::documented_flat_fee(venue));
        let flat = if flat_quote == Decimal::ZERO {
            Decimal::ZERO
        } else {
            let a = asset.to_uppercase();
            if a == "USDT" || a == "USDC" {
                flat_quote
            } else if px > Decimal::ZERO {
                flat_quote / px
            } else {
                Decimal::ZERO
            }
        };
        flat + fee_amount(short, self.transfer_fee_bps)
    }
}

#[derive(Debug, Clone)]
pub struct PaperFill {
    pub fills: Vec<Fill>,
    /// Realized PnL in quote terms for closed round-trip intents, else None.
    pub realized_pnl_quote: Option<Decimal>,
}

/// Restate an asset-denominated transfer fee in quote terms (USDT).
/// Stablecoin moves are 1:1; base moves convert at the leg price `px`.
/// A non-positive price degrades to the unconverted fee, never a multiply
/// by zero that would silently erase the cost.
fn fee_quote_for(asset: &str, fee_asset: Decimal, px: Decimal) -> Decimal {
    let a = asset.to_uppercase();
    if a == "USDT" || a == "USDC" || px <= Decimal::ZERO {
        fee_asset
    } else {
        fee_asset * px
    }
}

/// One virtual rebalance between venue ledgers. Paper-only: live mode never
/// auto-moves funds. The sender is debited immediately (amount + fee, burned);
/// the receiver is credited at `available_at_ms` when a delay is configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub from: Venue,
    pub to: Venue,
    pub asset: String,
    pub amount: Decimal,
    pub fee: Decimal,
    /// Same friction in quote terms (USDT), for realized-PnL accounting.
    /// Stablecoin moves are 1:1; base moves convert at the leg price.
    #[serde(default)]
    pub fee_quote: Decimal,
    pub ts_ms: i64,
    pub available_at_ms: i64,
}

#[derive(Debug, Clone)]
struct PendingTransfer {
    to: Venue,
    asset: String,
    amount: Decimal,
    available_at_ms: i64,
}

pub struct PaperExecutor {
    pub cfg: PaperConfig,
    pub realized_pnl: Decimal,
    pub trades: u64,
    /// Completed virtual rebalances (paper-only self-rebalance).
    pub transfers: u64,
    /// Simulated free balances per venue/asset. Spot fills debit/credit these;
    /// a leg the balance cannot fund aborts the intent (no partial mutation).
    pub balances: Balances,
    /// Transfers deducted from the sender but not yet credited (delay model).
    pending: Vec<PendingTransfer>,
    /// Mark-to-market equity right after seeding. Unrealized P/L is measured
    /// against this: equity now minus baseline minus realized.
    pub starting_equity: Decimal,
    /// Quote notional held per open funding pair, keyed (venue, "BASE/QUOTE")
    /// with its quote asset. Recorded on fills; credited per interval.
    pub funding_open: BTreeMap<(Venue, String), (Decimal, String)>,
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
            transfers: 0,
            balances: Balances::new(),
            pending: Vec::new(),
            starting_equity: Decimal::ZERO,
            funding_open: BTreeMap::new(),
        }
    }

    /// Record a filled funding entry's quote notional for interval credits.
    /// Notionals accumulate: re-entries on one pair top the position up.
    pub fn note_funding_fill(
        &mut self,
        venue: Venue,
        symbol: &str,
        quote: &str,
        notional_quote: Decimal,
    ) {
        let e = self
            .funding_open
            .entry((venue, symbol.to_string()))
            .or_insert((Decimal::ZERO, quote.to_string()));
        e.0 += notional_quote;
    }

    /// Credit one funding interval on an open pair. Positive rates pay our
    /// short perp; negative rates charge it. Unknown pairs credit nothing.
    /// The payout lands straight in the quote balance (and realized P/L),
    /// like a venue credit: no ledger leg exists to move it from.
    pub fn credit_funding(
        &mut self,
        venue: Venue,
        symbol: &str,
        rate_per_interval: Decimal,
    ) -> Decimal {
        let Some((notional, quote)) = self.funding_open.get(&(venue, symbol.to_string())).cloned()
        else {
            return Decimal::ZERO;
        };
        let amount = notional * rate_per_interval;
        adjust(&mut self.balances, venue, &quote, amount);
        self.realized_pnl += amount;
        amount
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
            let px = self.fill_price(leg, books)?;
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

    /// Executable price for one leg, with the same slip haircut as `execute`.
    fn fill_price(
        &self,
        leg: &arb_core::TradeLeg,
        books: &BTreeMap<(Venue, String), OrderBook>,
    ) -> Result<Decimal, String> {
        match leg.market {
            // Perp fills at mark (no book to walk).
            MarketKind::Perp => {
                let slip = leg.price * self.cfg.extra_slip_bps / Decimal::new(10_000, 0);
                Ok(match leg.side {
                    Side::Buy => leg.price + slip,
                    Side::Sell => leg.price - slip,
                })
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
                Ok(px)
            }
        }
    }

    /// Credit transfers whose settlement delay has passed. Call once per tick.
    pub fn settle_due(&mut self, now_ms: i64) -> usize {
        let mut n = 0;
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].available_at_ms <= now_ms {
                let p = self.pending.remove(i);
                adjust(&mut self.balances, p.to, &p.asset, p.amount);
                n += 1;
            } else {
                i += 1;
            }
        }
        n
    }

    /// In-flight credits to a venue/asset (deducted from the sender, not yet
    /// settled). Shortfall math must count these or a delayed transfer repeats.
    fn pending_to(&self, venue: Venue, asset: &str) -> Decimal {
        self.pending
            .iter()
            .filter(|p| p.to == venue && p.asset == asset)
            .map(|p| p.amount)
            .sum()
    }

    /// What one leg needs that its venue ledger lacks, if anything.
    /// Returns (asset, shortfall) in asset units. Counts in-flight delayed
    /// credits alongside settled balances.
    fn shortfall(
        &self,
        ledger: &Balances,
        leg: &arb_core::TradeLeg,
        px: Decimal,
        fee: Decimal,
    ) -> Option<(String, Decimal)> {
        let (base, quote) = (leg.symbol.base.clone(), leg.symbol.quote.clone());
        let (asset, need) = match leg.market {
            MarketKind::Perp => (quote, fee),
            MarketKind::Spot => match leg.side {
                Side::Buy => (quote, px * leg.qty_base + fee),
                Side::Sell => (base, leg.qty_base),
            },
        };
        let have = bal(ledger, leg.venue, &asset) + self.pending_to(leg.venue, &asset);
        if have < need {
            Some((asset, need - have))
        } else {
            None
        }
    }

    /// Cover an intent's inventory shortfalls with virtual venue-to-venue
    /// transfers (paper-only). Loops until every leg is fundable or no donor
    /// covers the next shortfall. Each transfer burns the sending venue's
    /// own friction (flat withdrawal proxy + `transfer_fee_bps`) from the
    /// sender and honors `transfer_delay_ms` before crediting.
    pub fn rebalance_for(
        &mut self,
        intent: &Intent,
        books: &BTreeMap<(Venue, String), OrderBook>,
        ts_ms: i64,
    ) -> Vec<Transfer> {
        let mut out = Vec::new();
        // Price every leg once; a leg whose book is missing aborts the whole
        // rebalance — moving funds cannot fix a missing market.
        let mut priced = Vec::new();
        for leg in &intent.legs {
            let Ok(px) = self.fill_price(leg, books) else {
                return Vec::new();
            };
            priced.push((
                leg,
                px,
                fee_amount(px * leg.qty_base, self.fee_bps(leg.venue)),
            ));
        }
        loop {
            // (venue, asset, short, leg price for fee conversion)
            let mut next: Option<(Venue, String, Decimal, Decimal)> = None;
            for (leg, px, fee) in &priced {
                if let Some((asset, short)) = self.shortfall(&self.balances, leg, *px, *fee) {
                    next = Some((leg.venue, asset, short, *px));
                    break;
                }
            }
            let Some((venue, asset, short, px)) = next else {
                break;
            };
            // Richest other venue that covers shortfall + ITS OWN fee wins:
            // withdrawal friction differs per sender.
            let mut donor: Option<(Venue, Decimal, Decimal)> = None;
            let mut best = Decimal::ZERO;
            for (v, m) in &self.balances {
                if *v == venue {
                    continue;
                }
                let have = m.get(&asset).copied().unwrap_or_default();
                let tfee = self.cfg.transfer_cost(*v, &asset, short, px);
                if have >= short + tfee && have > best {
                    best = have;
                    donor = Some((*v, tfee, fee_quote_for(&asset, tfee, px)));
                }
            }
            let Some((from, tfee, tfee_quote)) = donor else {
                break;
            };
            adjust(&mut self.balances, from, &asset, -(short + tfee));
            let available_at_ms = ts_ms + self.cfg.transfer_delay_ms.max(0);
            if available_at_ms <= ts_ms {
                adjust(&mut self.balances, venue, &asset, short);
            } else {
                self.pending.push(PendingTransfer {
                    to: venue,
                    asset: asset.clone(),
                    amount: short,
                    available_at_ms,
                });
            }
            self.transfers += 1;
            out.push(Transfer {
                from,
                to: venue,
                asset,
                amount: short,
                fee: tfee,
                fee_quote: tfee_quote,
                ts_ms,
                available_at_ms,
            });
        }
        out
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
            ..Default::default()
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
            ..Default::default()
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
    fn funding_credit_pays_open_pairs_only() {
        let mut ex = PaperExecutor::default();
        ex.seed(Venue::Bybit, "USDT", dec!(1000));
        // Unknown pair credits nothing and mutates nothing.
        assert_eq!(
            ex.credit_funding(Venue::Bybit, "BTC/USDT", dec!(0.0001)),
            dec!(0)
        );
        assert_eq!(ex.realized_pnl, dec!(0));
        ex.note_funding_fill(Venue::Bybit, "BTC/USDT", "USDT", dec!(10000));
        // One 1bp interval on 10k pays 1; negative rates charge the short.
        assert_eq!(
            ex.credit_funding(Venue::Bybit, "BTC/USDT", dec!(0.0001)),
            dec!(1)
        );
        assert_eq!(
            ex.credit_funding(Venue::Bybit, "BTC/USDT", dec!(-0.0001)),
            dec!(-1)
        );
        assert_eq!(ex.realized_pnl, dec!(0));
        assert_eq!(ex.balance(Venue::Bybit, "USDT"), dec!(1000));
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
            ..Default::default()
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

    fn cross_intent() -> Intent {
        let s = Symbol::new("BTC", "USDT");
        Intent {
            strategy: "cross_exchange".into(),
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
        }
    }

    #[test]
    fn rebalance_covers_shortfall_then_fill_succeeds() {
        let intent = cross_intent();
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            // No flat schedule here: this test covers coverage, not fees.
            transfer_fees: BTreeMap::from([(Venue::Binance, Decimal::ZERO)]),
            ..Default::default()
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Binance, "BTC", dec!(5));
        // Bybit holds nothing to sell: fill must abort with balances untouched.
        assert!(ex.execute(&intent, &books(), 10).is_err());
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1);
        assert_eq!((moved[0].from, moved[0].to), (Venue::Binance, Venue::Bybit));
        assert_eq!(moved[0].asset, "BTC");
        assert_eq!(moved[0].amount, dec!(1));
        assert_eq!(ex.transfers, 1);
        // Donor debited, receiver credited, retry fills.
        assert_eq!(ex.balance(Venue::Binance, "BTC"), dec!(4));
        ex.execute(&intent, &books(), 10).unwrap();
        assert_eq!(ex.trades, 1);
    }

    #[test]
    fn rebalance_burns_fee_and_honors_delay() {
        let intent = cross_intent();
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            transfer_fee_bps: dec!(100), // 1% burned from the sender
            // No flat schedule here: this test covers the bps leg + delay.
            transfer_fees: BTreeMap::from([(Venue::Binance, Decimal::ZERO)]),
            transfer_delay_ms: 1000,
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Binance, "BTC", dec!(5));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].fee, dec!(0.01));
        assert_eq!(moved[0].available_at_ms, 1010);
        // Sender debited amount + fee; receiver locked until settle.
        assert_eq!(ex.balance(Venue::Binance, "BTC"), dec!(3.99));
        assert_eq!(ex.balance(Venue::Bybit, "BTC"), dec!(0));
        assert_eq!(ex.settle_due(500), 0);
        assert_eq!(ex.settle_due(1010), 1);
        assert_eq!(ex.balance(Venue::Bybit, "BTC"), dec!(1));
        ex.execute(&intent, &books(), 1010).unwrap();
    }

    #[test]
    fn rebalance_stops_when_no_donor_covers() {
        let intent = cross_intent();
        let mut ex = PaperExecutor::default();
        ex.seed(Venue::Binance, "USDT", dec!(1)); // neither leg fundable
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert!(moved.is_empty());
        assert_eq!(ex.transfers, 0);
    }

    #[test]
    fn documented_schedule_mirrors_official_withdrawal_fees() {
        // Empty map = official USDT figures (TRC20, checked 2026-10-29).
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Binance), dec!(1));
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Bybit), dec!(1));
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Okx), dec!(1));
        // Indodax publishes no static figure: operator-set to 1 like the rest.
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Indodax), dec!(1));
        // Kraken varies its USDT fee by network: operator-set to 1 until verified.
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Kraken), dec!(1));
        let cfg = PaperConfig::default();
        // Stablecoin move: flat applies 1:1 on top of the bps leg.
        assert_eq!(
            cfg.transfer_cost(Venue::Binance, "USDT", dec!(100), dec!(50)),
            dec!(1)
        );
        // Base move: flat USDT converts at the leg price (1/100 = 0.01 BTC).
        assert_eq!(
            cfg.transfer_cost(Venue::Binance, "BTC", dec!(1), dec!(100)),
            dec!(1) / dec!(100)
        );
        // Explicit zero opts a venue out of the schedule.
        let zero = PaperConfig {
            transfer_fees: BTreeMap::from([(Venue::Binance, Decimal::ZERO)]),
            ..Default::default()
        };
        assert_eq!(
            zero.transfer_cost(Venue::Binance, "USDT", dec!(100), dec!(50)),
            dec!(0)
        );
    }

    #[test]
    fn rebalance_burns_per_venue_flat_fee_in_quote_terms() {
        // Binance buy leg needs 100.1 USDT; Binance holds 50. Okx is richer
        // but prices itself out with a 500 USDT flat fee, so Bybit — on the
        // documented 1 USDT schedule — covers.
        let intent = cross_intent();
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            transfer_fees: BTreeMap::from([(Venue::Okx, dec!(500))]),
            ..Default::default()
        });
        ex.seed(Venue::Binance, "USDT", dec!(50));
        ex.seed(Venue::Okx, "USDT", dec!(500));
        ex.seed(Venue::Bybit, "USDT", dec!(200));
        ex.seed(Venue::Bybit, "BTC", dec!(5));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1);
        assert_eq!((moved[0].from, moved[0].to), (Venue::Bybit, Venue::Binance));
        assert_eq!(moved[0].asset, "USDT");
        assert_eq!(moved[0].amount, dec!(50.1));
        assert_eq!(moved[0].fee, dec!(1));
        assert_eq!(moved[0].fee_quote, dec!(1));
        // Donor debited shortfall + its own flat fee.
        assert_eq!(ex.balance(Venue::Bybit, "USDT"), dec!(200) - dec!(51.1));
    }

    #[test]
    fn rebalance_converts_flat_fee_to_base_at_leg_price() {
        // Bybit must sell 1 BTC it lacks; Binance covers. The 1 USDT flat
        // converts at the sell leg's fill price: 1/102 BTC, quoted back.
        let intent = cross_intent();
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            ..Default::default() // documented schedule: Binance flat 1 USDT
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Binance, "BTC", dec!(5));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].asset, "BTC");
        // Same operations, same order: identical rounding to production.
        assert_eq!(moved[0].fee, dec!(1) / dec!(102));
        assert_eq!(moved[0].fee_quote, dec!(1) / dec!(102) * dec!(102));
        assert_eq!(
            ex.balance(Venue::Binance, "BTC"),
            dec!(5) - dec!(1) - dec!(1) / dec!(102)
        );
    }
}
