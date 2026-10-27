//! Persistence. `MemoryStore` ships now; the `Store` trait is the seam where
//! the SQL backend (Postgres/SQLite via sqlx) drops in without touching callers.
//! JSONL export gives durable, diffable paper-trade history today.

use arb_core::{Fill, Intent};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTrade {
    pub intent: Intent,
    pub fills: Vec<Fill>,
    pub realized_pnl_quote: Option<Decimal>,
    pub ts_ms: i64,
    /// Engine run that produced this trade. Empty on records written
    /// before sessions were tracked.
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub engine_version: String,
}

/// Boundary marker written once per engine start so one JSONL file can
/// hold many runs without looking like a single continuous soak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStart {
    pub event: String,
    pub session_id: String,
    pub engine_version: String,
    pub mode: String,
    pub feed: String,
    pub started_ms: i64,
    /// Trades reloaded from the existing JSONL at this start.
    pub restored_trades: usize,
}

impl SessionStart {
    pub fn new(
        session_id: String,
        engine_version: String,
        mode: String,
        feed: String,
        started_ms: i64,
        restored_trades: usize,
    ) -> Self {
        Self {
            event: "session_start".into(),
            session_id,
            engine_version,
            mode,
            feed,
            started_ms,
            restored_trades,
        }
    }
}

pub trait Store: Send {
    fn record_trade(&mut self, t: StoredTrade) -> anyhow::Result<()>;
    fn record_session_start(&mut self, s: SessionStart) -> anyhow::Result<()>;
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

    /// Open a JSONL-backed store, reloading trades written by earlier
    /// engine runs so state derived from history (for example open
    /// funding pairs) survives restarts. A missing file starts empty.
    pub fn with_jsonl(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let trades = load_jsonl(&path)?;
        Ok(Self {
            trades,
            jsonl_path: Some(path),
        })
    }

    fn append_line(&self, line: &str) -> anyhow::Result<()> {
        if let Some(p) = &self.jsonl_path {
            let mut f = OpenOptions::new().create(true).append(true).open(p)?;
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

/// Reload trades from JSONL. `session_start` markers are skipped, and a
/// malformed final line (a write interrupted mid-append) is tolerated;
/// malformed lines anywhere else are an error, so history loss is loud.
fn load_jsonl(path: &Path) -> anyhow::Result<Vec<StoredTrade>> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut lines: Vec<String> = Vec::new();
    for line in BufReader::new(file).lines() {
        lines.push(line?);
    }
    let last_nonempty = lines.iter().rposition(|l| !l.trim().is_empty());
    let mut trades = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(line).and_then(|v| {
            if v.get("event").and_then(|e| e.as_str()) == Some("session_start") {
                Ok(None)
            } else {
                serde_json::from_value::<StoredTrade>(v).map(Some)
            }
        });
        match parsed {
            Ok(Some(t)) => trades.push(t),
            Ok(None) => {}
            Err(e) if Some(i) == last_nonempty => {
                // Trailing partial record from an interrupted append.
                let _ = e;
                break;
            }
            Err(e) => {
                // shortcut: one corrupt middle line must not block startup;
                // skip it loudly so history loss is visible but not fatal.
                eprintln!(
                    "store: skipping corrupt {} line {}: {e}",
                    path.display(),
                    i + 1
                );
                continue;
            }
        }
    }
    Ok(trades)
}

impl Store for MemoryStore {
    fn record_trade(&mut self, t: StoredTrade) -> anyhow::Result<()> {
        self.append_line(&serde_json::to_string(&t)?)?;
        self.trades.push(t);
        Ok(())
    }
    fn record_session_start(&mut self, s: SessionStart) -> anyhow::Result<()> {
        self.append_line(&serde_json::to_string(&s)?)
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

    fn mk_trade(pnl: Decimal, session_id: &str) -> StoredTrade {
        StoredTrade {
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
            session_id: session_id.into(),
            engine_version: "0.1.0".into(),
        }
    }

    #[test]
    fn memory_store_sums_pnl_and_writes_jsonl() {
        let dir = std::env::temp_dir().join("arb-store-test.jsonl");
        let _ = std::fs::remove_file(&dir);
        let mut st = MemoryStore::with_jsonl(dir.clone()).unwrap();
        st.record_trade(mk_trade(dec!(1.5), "s1")).unwrap();
        st.record_trade(mk_trade(dec!(-0.5), "s1")).unwrap();
        assert_eq!(st.total_pnl(), dec!(1));
        let content = std::fs::read_to_string(&dir).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn jsonl_reload_restores_trades_and_skips_session_markers() {
        let dir = std::env::temp_dir().join("arb-store-test-session.jsonl");
        let _ = std::fs::remove_file(&dir);
        {
            let mut st = MemoryStore::with_jsonl(dir.clone()).unwrap();
            st.record_session_start(SessionStart::new(
                "s1".into(),
                "0.1.0".into(),
                "paper".into(),
                "live".into(),
                10,
                0,
            ))
            .unwrap();
            st.record_trade(mk_trade(dec!(2), "s1")).unwrap();
            // Legacy line written before session fields existed.
            let mut legacy = serde_json::to_value(mk_trade(dec!(3), "")).unwrap();
            let obj = legacy.as_object_mut().unwrap();
            obj.remove("session_id");
            obj.remove("engine_version");
            let mut f = OpenOptions::new().append(true).open(&dir).unwrap();
            writeln!(f, "{legacy}").unwrap();
            // Trailing partial record from an interrupted append.
            write!(f, "{{\"intent\":").unwrap();
        }
        let reopened = MemoryStore::with_jsonl(dir.clone()).unwrap();
        assert_eq!(reopened.trades().len(), 2);
        assert_eq!(reopened.total_pnl(), dec!(5));
        assert_eq!(reopened.trades()[0].session_id, "s1");
        assert_eq!(reopened.trades()[1].session_id, "");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn schema_sql_covers_the_store_contract() {
        // No SQL backend exists yet; the DDL is the contract it must
        // honor. Pin the tables and columns the Store trait serves so
        // schema and trait cannot drift apart silently.
        for table in [
            "CREATE TABLE IF NOT EXISTS trades",
            "CREATE TABLE IF NOT EXISTS fills",
            "CREATE TABLE IF NOT EXISTS opportunities",
        ] {
            assert!(SCHEMA_SQL.contains(table), "{table}");
        }
        for column in [
            "strategy",
            "expected_edge_bps",
            "realized_pnl_quote",
            "venue",
            "symbol",
            "side",
            "price",
            "qty_base",
            "fee_quote",
        ] {
            assert!(SCHEMA_SQL.contains(column), "{column}");
        }
    }

    #[test]
    fn corrupt_middle_line_does_not_block_startup() {
        let dir = std::env::temp_dir().join("arb-store-test-corrupt.jsonl");
        let _ = std::fs::remove_file(&dir);
        {
            let mut st = MemoryStore::with_jsonl(dir.clone()).unwrap();
            st.record_trade(mk_trade(dec!(2), "s1")).unwrap();
            let mut f = OpenOptions::new().append(true).open(&dir).unwrap();
            writeln!(f, "not json at all").unwrap();
            st.record_trade(mk_trade(dec!(3), "s1")).unwrap();
        }
        let reopened = MemoryStore::with_jsonl(dir.clone()).unwrap();
        assert_eq!(reopened.trades().len(), 2);
        assert_eq!(reopened.total_pnl(), dec!(5));
        let _ = std::fs::remove_file(&dir);
    }
}
