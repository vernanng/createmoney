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
    /// so it is set to the same 1 USDT by operator decision. Kraken TRC20
    /// is 1 USDT (min 6; official schedule, checked 2026-10-29) — other
    /// networks differ, so TRC20 is the modeled rail. Re-verify on the
    /// withdrawal screen — exchanges change these without notice.
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

    /// Cover an intent's inventory shortfalls with virtual venue-to-venue
    /// transfers (paper-only). Loops until every leg is fundable or no donor
    /// covers the next shortfall. Each transfer burns the sending venue's
    /// own friction (flat withdrawal proxy + `transfer_fee_bps`) from the
    /// sender and honors `transfer_delay_ms` before crediting. Moves
    /// overshoot to at least 100x the fee, and total fees for one intent
    /// are capped below its expected profit — past that, the intent is
    /// left unfunded rather than funded at a guaranteed loss.
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
        // Funding this intent may never cost more than it expects to make:
        // cumulative transfer fees are capped at the expected profit
        // (edge on the first leg's notional, quote terms). Open intents
        // (funding) quote that edge annualized, so scale it to a 7-day
        // hold — the soak-gate horizon — before comparing.
        let mut profit = priced.first().map_or(Decimal::ZERO, |p| {
            fee_amount(p.1 * p.0.qty_base, intent.expected_edge_bps)
        });
        if !intent.roundtrip {
            profit = profit * Decimal::new(7, 0) / Decimal::new(365, 0);
        }
        let mut spent_quote = Decimal::ZERO;
        loop {
            // Simulate the legs in intent order against current balances:
            // a leg may spend what an earlier leg produced (triangular
            // chains), so the true shortfall per (venue, asset) is the
            // deepest deficit along the sequence. Same-ledger debits
            // (spot buy + perp fee) still merge into one transfer.
            // (venue, asset) -> (short, leg px, leg quote) at deepest dip
            let mut deficits: BTreeMap<(Venue, String), (Decimal, Decimal, String)> =
                BTreeMap::new();
            let mut running: BTreeMap<(Venue, String), Decimal> = BTreeMap::new();
            for (leg, px, fee) in &priced {
                let (debit_asset, debit) = leg_need(leg, *px, *fee);
                let credit = match leg.market {
                    MarketKind::Perp => None,
                    MarketKind::Spot => match leg.side {
                        Side::Buy => Some((leg.symbol.base.clone(), leg.qty_base)),
                        Side::Sell => Some((leg.symbol.quote.clone(), *px * leg.qty_base - *fee)),
                    },
                };
                for (asset, delta) in [(debit_asset, -debit)].into_iter().chain(credit) {
                    let key = (leg.venue, asset);
                    let start =
                        bal(&self.balances, leg.venue, &key.1) + self.pending_to(leg.venue, &key.1);
                    let bal_now = running.entry(key.clone()).or_insert(start);
                    *bal_now += delta;
                    if *bal_now < Decimal::ZERO {
                        let short = -*bal_now;
                        let e = deficits.entry(key).or_insert((
                            Decimal::ZERO,
                            *px,
                            leg.symbol.quote.clone(),
                        ));
                        if short > e.0 {
                            *e = (short, *px, leg.symbol.quote.clone());
                        }
                    }
                }
            }
            // (venue, asset, short, dollar price for fee conversion)
            let next =
                deficits
                    .iter()
                    .next()
                    .map(|((venue, asset), (short, leg_px, leg_quote))| {
                        (
                            *venue,
                            asset.clone(),
                            *short,
                            usdt_px(books, asset, *leg_px, leg_quote),
                        )
                    });
            let Some((venue, asset, short, px)) = next else {
                break;
            };
            // Richest other venue that covers the move + ITS OWN fee wins:
            // withdrawal friction differs per sender. The move overshoots
            // the shortfall to at least 100x the fee, so a flat withdrawal
            // fee is paid per chunk of inventory, not per dust top-up.
            let mut donor: Option<(Venue, Decimal, Decimal, Decimal)> = None;
            let mut best = Decimal::ZERO;
            for (v, m) in &self.balances {
                if *v == venue {
                    continue;
                }
                let have = m.get(&asset).copied().unwrap_or_default();
                let fee_on_short = self.cfg.transfer_cost(*v, &asset, short, px);
                let chunk = short.max(fee_on_short * Decimal::new(100, 0));
                let chunk_fee = self.cfg.transfer_cost(*v, &asset, chunk, px);
                // Overshoot when the donor can carry the chunk; a donor
                // that only covers the shortfall itself still funds it.
                let (amount, tfee) = if have >= chunk + chunk_fee {
                    (chunk, chunk_fee)
                } else if have >= short + fee_on_short {
                    (short, fee_on_short)
                } else {
                    continue;
                };
                if have > best {
                    best = have;
                    donor = Some((*v, amount, tfee, fee_quote_for(&asset, tfee, px)));
                }
            }
            let Some((from, amount, tfee, tfee_quote)) = donor else {
                break;
            };
            if tfee_quote > Decimal::ZERO && spent_quote + tfee_quote >= profit {
                break;
            }
            spent_quote += tfee_quote;
            adjust(&mut self.balances, from, &asset, -(amount + tfee));
            let available_at_ms = ts_ms + self.cfg.transfer_delay_ms.max(0);
            if available_at_ms <= ts_ms {
                adjust(&mut self.balances, venue, &asset, amount);
            } else {
                self.pending.push(PendingTransfer {
                    to: venue,
                    asset: asset.clone(),
                    amount,
                    available_at_ms,
                });
            }
            self.transfers += 1;
            out.push(Transfer {
                from,
                to: venue,
                asset,
                amount,
                fee: tfee,
                fee_quote: tfee_quote,
                ts_ms,
                available_at_ms,
            });
        }
        out
    }
}

/// Dollar price of `asset` for fee conversion: the leg price when the
/// leg is stablecoin-quoted, else the mid of any live ASSET/USDT book.
/// Zero degrades `transfer_cost` to its bps leg — better than converting
/// dollars at a BTC-quoted price (0.03) into garbage units.
fn usdt_px(
    books: &BTreeMap<(Venue, String), OrderBook>,
    asset: &str,
    leg_px: Decimal,
    leg_quote: &str,
) -> Decimal {
    let stable = |s: &str| s.eq_ignore_ascii_case("USDT") || s.eq_ignore_ascii_case("USDC");
    if stable(leg_quote) {
        return leg_px;
    }
    for q in ["USDT", "USDC"] {
        let key = format!("{asset}/{q}");
        for ((_, k), b) in books {
            if *k == key {
                if let (Some(bid), Some(ask)) = (b.best_bid(), b.best_ask()) {
                    return (bid.price + ask.price) / Decimal::new(2, 0);
                }
            }
        }
    }
    Decimal::ZERO
}

/// Gross amount of one asset a leg debits at execution.
fn leg_need(leg: &arb_core::TradeLeg, px: Decimal, fee: Decimal) -> (String, Decimal) {
    match leg.market {
        MarketKind::Perp => (leg.symbol.quote.clone(), fee),
        MarketKind::Spot => match leg.side {
            Side::Buy => (leg.symbol.quote.clone(), px * leg.qty_base + fee),
            Side::Sell => (leg.symbol.base.clone(), leg.qty_base),
        },
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
        // 500 bps edge: the 1% transfer burn ($1.02) must stay under the
        // $5 expected profit or the fee gate rightly refuses to fund it.
        let mut intent = cross_intent();
        intent.expected_edge_bps = dec!(500);
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
    fn rebalance_skips_when_fee_eats_the_edge() {
        // Dust intent: 0.01 BTC ≈ $1 notional, 100 bps edge = $0.01 of
        // expected profit. Paying the documented $1 flat transfer fee to
        // fund it loses money by construction: no transfer may fire.
        let mut intent = cross_intent();
        for leg in &mut intent.legs {
            leg.qty_base = dec!(0.01);
        }
        let mut ex = PaperExecutor::default(); // documented $1 flat schedule
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        ex.seed(Venue::Binance, "BTC", dec!(5));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert!(moved.is_empty());
        assert_eq!(ex.transfers, 0);
        assert_eq!(ex.balance(Venue::Binance, "BTC"), dec!(5));
    }

    #[test]
    fn rebalance_aggregates_same_venue_asset_shortfalls() {
        // Funding-shaped intent: spot buy and perp short on the SAME venue
        // both debit USDT (103.103 + 0.102 fee). Funding them leg by leg
        // pays the $1 flat fee twice — a $100 min-chunk for the $0.102
        // margin shortfall, then the real move. One intent, one venue,
        // one asset must mean one transfer.
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "funding_rate".into(),
            // 20000 bps annualized: scaled to a 7-day hold the expected
            // profit is ~$3.95, still over the $1 gate.
            expected_edge_bps: dec!(20000),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: false,
            legs: vec![
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(103),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(102),
                    qty_base: dec!(1),
                    market: MarketKind::Perp,
                },
            ],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            ..Default::default() // documented $1 flat schedule
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1, "one transfer per (venue, asset)");
        assert!(moved[0].amount >= dec!(103.205));
        assert_eq!(moved[0].fee, dec!(1));
    }

    #[test]
    fn rebalance_funding_gate_scales_annualized_edge() {
        // Funding edge is per YEAR: 1000 bps on ~$103 is $10.30/yr, about
        // $0.20 over a 7-day hold. The gate must not OK a $1 transfer fee
        // against the unscaled yearly figure.
        let s = Symbol::new("BTC", "USDT");
        let intent = Intent {
            strategy: "funding_rate".into(),
            expected_edge_bps: dec!(1000),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: false,
            legs: vec![
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: dec!(103),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s,
                    side: Side::Sell,
                    price: dec!(102),
                    qty_base: dec!(1),
                    market: MarketKind::Perp,
                },
            ],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            ..Default::default()
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert!(moved.is_empty(), "fee dwarfs 7-day funding expectation");
        assert_eq!(ex.transfers, 0);
    }

    #[test]
    fn rebalance_falls_back_to_shortfall_when_chunk_does_not_fit() {
        // Donor holds 60 USDT: enough for the 50.1 shortfall + $1 fee,
        // not enough for the 100x-fee chunk (101). The chunk is an
        // overshoot preference, not a funding requirement.
        let mut intent = cross_intent();
        intent.expected_edge_bps = dec!(500);
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            ..Default::default()
        });
        ex.seed(Venue::Binance, "USDT", dec!(50)); // buy leg needs 100.1
        ex.seed(Venue::Bybit, "USDT", dec!(60));
        ex.seed(Venue::Bybit, "BTC", dec!(5)); // sell leg funded
        let moved = ex.rebalance_for(&intent, &books(), 10);
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].amount, dec!(50.1));
        assert_eq!(moved[0].fee, dec!(1));
    }

    #[test]
    fn rebalance_triangular_chain_only_funds_the_missing_asset() {
        // One-venue cycle USDT -> BTC -> ETH -> USDT: legs 2 and 3 spend
        // what earlier legs produce, so only the opening USDT is truly
        // missing. Funding gross per-asset needs instead would chase BTC
        // and ETH transfers (priced off the 0.03 ETH/BTC leg) and never
        // fund the trade.
        let mk = |sym: Symbol, bid: Decimal, ask: Decimal| {
            let mut b = OrderBook::new(Venue::Bybit, sym.clone());
            b.apply_snapshot(
                vec![BookLevel {
                    price: bid,
                    qty: dec!(100),
                }],
                vec![BookLevel {
                    price: ask,
                    qty: dec!(100),
                }],
                1,
                0,
            );
            (book_key(Venue::Bybit, &sym), b)
        };
        let btc_usdt = Symbol::new("BTC", "USDT");
        let eth_btc = Symbol::new("ETH", "BTC");
        let eth_usdt = Symbol::new("ETH", "USDT");
        let tri_books = BTreeMap::from([
            mk(btc_usdt.clone(), dec!(99), dec!(100)),
            mk(eth_btc.clone(), dec!(0.029), dec!(0.03)),
            mk(eth_usdt.clone(), dec!(3.3), dec!(3.4)),
        ]);
        let leg = |symbol: Symbol, side: Side, price: Decimal, qty_base: Decimal| TradeLeg {
            venue: Venue::Bybit,
            symbol,
            side,
            price,
            qty_base,
            market: MarketKind::Spot,
        };
        let intent = Intent {
            strategy: "triangular".into(),
            expected_edge_bps: dec!(500),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![
                leg(btc_usdt, Side::Buy, dec!(100), dec!(1)),
                leg(eth_btc, Side::Buy, dec!(0.03), dec!(30)),
                leg(eth_usdt, Side::Sell, dec!(3.3), dec!(30)),
            ],
        };
        let mut ex = PaperExecutor::new(PaperConfig {
            fee_bps_override: Some(dec!(10)),
            extra_slip_bps: dec!(0),
            ..Default::default()
        });
        ex.seed(Venue::Binance, "USDT", dec!(10000));
        let moved = ex.rebalance_for(&intent, &tri_books, 10);
        assert_eq!(moved.len(), 1, "only the opening USDT is missing");
        assert_eq!(moved[0].asset, "USDT");
        assert_eq!(moved[0].fee, dec!(1));
        ex.execute(&intent, &tri_books, 10).unwrap();
    }

    #[test]
    fn documented_schedule_mirrors_official_withdrawal_fees() {
        // Empty map = official USDT figures (TRC20, checked 2026-10-29).
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Binance), dec!(1));
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Bybit), dec!(1));
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Okx), dec!(1));
        // Indodax publishes no static figure: operator-set to 1 like the rest.
        assert_eq!(PaperConfig::documented_flat_fee(Venue::Indodax), dec!(1));
        // Kraken TRC20 is 1 USDT per the official schedule (other networks differ).
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
        // documented 1 USDT schedule — covers. 500 bps edge ($5 profit)
        // keeps the $1 fee under the gate.
        let mut intent = cross_intent();
        intent.expected_edge_bps = dec!(500);
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
        // Shortfall is 50.1, but the move overshoots to 100x the $1 fee.
        assert_eq!(moved[0].amount, dec!(100));
        assert_eq!(moved[0].fee, dec!(1));
        assert_eq!(moved[0].fee_quote, dec!(1));
        // Donor debited the chunk + its own flat fee.
        assert_eq!(ex.balance(Venue::Bybit, "USDT"), dec!(200) - dec!(101));
    }

    #[test]
    fn rebalance_converts_flat_fee_to_base_at_leg_price() {
        // Bybit must sell 1 BTC it lacks; Binance covers. The 1 USDT flat
        // converts at the sell leg's fill price: 1/102 BTC, quoted back.
        // 500 bps edge ($5 profit) keeps the $1 fee under the gate; the
        // 1 BTC shortfall already exceeds 100x the fee, so no overshoot.
        let mut intent = cross_intent();
        intent.expected_edge_bps = dec!(500);
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
