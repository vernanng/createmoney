//! Strategy trait + registry. Strategies emit intents; they never place orders.

use arb_core::{Intent, OrderBook, Symbol, Venue};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// S8 cost-model falsifier: test harness only, never built into the binary.
#[cfg(test)]
mod ablate;
pub mod cex_dex;
pub mod cross_exchange;
/// S7 fee-threshold optimizer: test harness only, never built into the binary.
#[cfg(test)]
mod fee_table;
pub mod funding;
pub mod imbalance;
pub mod lag_chase;
pub mod triangular;

pub use cex_dex::{CexDex, DexQuote};
pub use cross_exchange::CrossExchange;
pub use funding::{accrued, FundingRate, FundingStrategy};
pub use imbalance::{both_sided_agrees, imbalance, quote_imbalance};
pub use lag_chase::LagChase;
pub use triangular::Triangular;

/// Read-only market view handed to strategies each tick.
#[derive(Debug, Default)]
pub struct MarketView {
    pub books: BTreeMap<(Venue, String), OrderBook>,
    pub dex_quotes: Vec<DexQuote>,
    pub funding: Vec<FundingRate>,
    pub now_ms: i64,
    /// Per-venue USDT+USDC ledger totals this tick, backing the venue-share
    /// cap. Empty in unit tests, where sizing falls back to full notional.
    pub venue_quote: BTreeMap<Venue, Decimal>,
}

impl MarketView {
    pub fn book(&self, v: Venue, s: &Symbol) -> Option<&OrderBook> {
        self.books.get(&(v, s.to_string()))
    }
    pub fn put(&mut self, b: OrderBook) {
        self.books.insert((b.venue, b.symbol.to_string()), b);
    }
}

#[derive(Debug, Clone)]
pub struct StrategyParams {
    pub min_edge_bps: Decimal,
    pub slip_buffer_bps: Decimal,
    pub target_notional_quote: Decimal,
    /// Hard floor for cross-exchange intents: below it, fixed transfer
    /// friction dwarfs any spread the edge gate can clear. 0 = off.
    pub min_notional_quote: Decimal,
    /// Eventual inventory-rebalance friction, amortised into the edge
    /// gate. 0 = assume rebalancing is free (it is not).
    pub rebalance_cost_bps: Decimal,
    pub max_book_age_ms: i64,
    pub ttl_ms: i64,
    /// Max age of a polled DEX quote. Pollers run every ~10 s, far slower
    /// than CEX books, so DEX quotes get their own (longer) gate.
    pub dex_max_quote_age_ms: i64,
    /// Venue-share cap, in bps of that venue's USDT+USDC ledger. Strategies
    /// that size single-venue entries shrink to fit it instead of emitting
    /// intents the engine's risk gate would only reject. 0 = off.
    pub max_venue_quote_share_bps: Decimal,
}

impl Default for StrategyParams {
    fn default() -> Self {
        Self {
            min_edge_bps: Decimal::new(15, 0),
            slip_buffer_bps: Decimal::new(5, 0),
            target_notional_quote: Decimal::new(10, 0),
            min_notional_quote: Decimal::ZERO,
            rebalance_cost_bps: Decimal::ZERO,
            max_book_age_ms: 2_000,
            ttl_ms: 1_500,
            dex_max_quote_age_ms: 30_000,
            max_venue_quote_share_bps: Decimal::ZERO,
        }
    }
}

/// Cumulative pair-evaluation rejections inside a strategy, so an
/// emitted=0 result can be attributed to the gate that actually blocked it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GateRejections {
    pub edge: u64,
    pub imbalance: u64,
    pub min_notional: u64,
    pub stale_or_missing: u64,
}

pub trait Strategy: Send {
    fn name(&self) -> &'static str;
    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent>;
    fn gate_rejections(&self) -> GateRejections {
        GateRejections::default()
    }
    /// Pairs the strategy closed this tick (funding exits). The engine
    /// releases their venue-cap exposure. Default: nothing ever closes.
    fn take_closed(&mut self) -> Vec<(Venue, Symbol)> {
        Vec::new()
    }
    /// Pairs the strategy currently holds open (funding entries). The
    /// engine surfaces them as pending orders. Default: nothing open.
    fn open_positions(&self) -> Vec<(Venue, Symbol)> {
        Vec::new()
    }
    /// A fill the engine executed for one of this strategy's intents.
    /// Strategies that track open pairs record them here — never on
    /// intent creation, where a later fill failure would leave phantoms.
    /// Default: nothing to track.
    fn note_filled(&mut self, _intent: &Intent) {}
}

/// Size (base qty) for a target quote notional at a reference price.
pub fn size_for_notional(notional_quote: Decimal, ref_px: Decimal) -> Decimal {
    if ref_px <= Decimal::ZERO {
        Decimal::ZERO
    } else {
        notional_quote / ref_px
    }
}

pub fn fresh(book: &OrderBook, now_ms: i64, max_age_ms: i64) -> bool {
    now_ms - book.ts_ms <= max_age_ms && !book.is_crossed()
}
