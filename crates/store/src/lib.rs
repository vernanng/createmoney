//! Persistence. `MemoryStore` ships now; the `Store` trait is the seam where
//! the SQL backend (Postgres/SQLite via sqlx) drops in without touching callers.
//! JSONL export gives durable, diffable paper-trade history today.

use arb_core::{Fill, Intent};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTrade {
    pub intent: Intent,
    pub fills: Vec<Fill>,
    pub realized_pnl_quote: Option<Decimal>,
    pub ts_ms: i64,
}

pub trait Store: Send {
    fn record_trade(&mut self, t: StoredTrade) -> anyhow::Result<()>;
    fn trades(&self) -> &[StoredTrade];
    fn total_pnl(&self) -> Decimal {
        self.trades()
            .iter()
            .filter_map(|t| t.realized_pnl_quote)
            .sum()
    }
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    trades: Vec<StoredTrade>,
    jsonl_path: Option<PathBuf>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_jsonl(path: impl Into<PathBuf>) -> Self {
        Self {
            trades: Vec::new(),
            jsonl_path: Some(path.into()),
        }
    }
}

impl Store for MemoryStore {
    fn record_trade(&mut self, t: StoredTrade) -> anyhow::Result<()> {
        if let Some(p) = &self.jsonl_path {
            let mut f = OpenOptions::new().create(true).append(true).open(p)?;
            writeln!(f, "{}", serde_json::to_string(&t)?)?;
        }
        self.trades.push(t);
        Ok(())
    }
    fn trades(&self) -> &[StoredTrade] {
        &self.trades
    }
}

/// DDL for the SQL backend. Kept next to the trait so both stay in sync.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS trades (
  id BIGSERIAL PRIMARY KEY,
  strategy TEXT NOT NULL,
  expected_edge_bps NUMERIC NOT NULL,
  realized_pnl_quote NUMERIC,
  ts_ms BIGINT NOT NULL,
  simulated BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE TABLE IF NOT EXISTS fills (
  id BIGSERIAL PRIMARY KEY,
  trade_id BIGINT NOT NULL REFERENCES trades(id),
  venue TEXT NOT NULL, symbol TEXT NOT NULL, side TEXT NOT NULL,
  price NUMERIC NOT NULL, qty_base NUMERIC NOT NULL, fee_quote NUMERIC NOT NULL,
  ts_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS fills_trade_idx ON fills(trade_id);
CREATE TABLE IF NOT EXISTS opportunities (
  id BIGSERIAL PRIMARY KEY,
  strategy TEXT NOT NULL, detail JSONB NOT NULL, ts_ms BIGINT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{MarketKind, Side, Symbol, TradeLeg, Venue};
    use rust_decimal_macros::dec;

    #[test]
    fn memory_store_sums_pnl_and_writes_jsonl() {
        let dir = std::env::temp_dir().join("arb-store-test.jsonl");
        let _ = std::fs::remove_file(&dir);
        let mut st = MemoryStore::with_jsonl(dir.clone());
        let mk = |pnl: Decimal| StoredTrade {
            intent: Intent {
                strategy: "t".into(),
                expected_edge_bps: dec!(20),
                ttl_ms: 100,
                created_ms: 0,
                roundtrip: true,
                legs: vec![TradeLeg {
                    venue: Venue::Binance,
                    symbol: Symbol::new("BTC", "USDT"),
                    side: Side::Buy,
                    price: dec!(100),
                    qty_base: dec!(1),
                    market: MarketKind::Spot,
                }],
            },
            fills: vec![],
            realized_pnl_quote: Some(pnl),
            ts_ms: 1,
        };
        st.record_trade(mk(dec!(1.5))).unwrap();
        st.record_trade(mk(dec!(-0.5))).unwrap();
        assert_eq!(st.total_pnl(), dec!(1));
        let content = std::fs::read_to_string(&dir).unwrap();
        assert_eq!(content.lines().count(), 2);
    }
}
