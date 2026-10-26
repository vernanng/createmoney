//! Strategy trait + registry. Strategies emit intents; they never place orders.

use arb_core::{Intent, OrderBook, Symbol, Venue};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

pub mod ablate;
pub mod cex_dex;
pub mod cross_exchange;
pub mod fee_table;
pub mod funding;
pub mod fx_triangle;
pub mod imbalance;
pub mod lag_chase;
pub mod triangular;

pub use ablate::{effective_fees, effective_params, matrix, Ablation};
pub use cex_dex::{CexDex, DexQuote};
pub use cross_exchange::{CrossExchange, ADAPTIVE_NOTIONALS};
pub use fee_table::FeeTable;
pub use funding::{accrued, FundingRate, FundingStrategy};
pub use fx_triangle::{converted, FxTriangle};
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
    pub max_book_age_ms: i64,
    pub ttl_ms: i64,
}

impl Default for StrategyParams {
    fn default() -> Self {
        Self {
            min_edge_bps: Decimal::new(15, 0),
            slip_buffer_bps: Decimal::new(5, 0),
            target_notional_quote: Decimal::new(1_000, 0),
            max_book_age_ms: 2_000,
            ttl_ms: 1_500,
        }
    }
}

pub trait Strategy: Send {
    fn name(&self) -> &'static str;
    fn on_tick(&mut self, view: &MarketView) -> Vec<Intent>;
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
