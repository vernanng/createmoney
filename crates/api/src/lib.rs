//! Control-plane state + metrics rendering (transport-agnostic).
//! The binary serves these over a tiny localhost HTTP listener.

use arb_core::Venue;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorHealth {
    pub venue: Venue,
    pub connected: bool,
    pub last_msg_ms: i64,
    pub books: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub mode: String,
    pub killed: bool,
    pub trades: u64,
    pub realized_pnl_quote: Decimal,
    pub opportunities_seen: u64,
    pub connectors: Vec<ConnectorHealth>,
    pub paused_strategies: Vec<String>,
}

pub struct Control {
    kill: Arc<AtomicBool>,
    paused: Vec<String>,
}

impl Control {
    pub fn new(kill: Arc<AtomicBool>) -> Self {
        Self {
            kill,
            paused: Vec::new(),
        }
    }
    pub fn kill(&self) {
        self.kill.store(true, Ordering::SeqCst);
    }
    pub fn revive(&self) {
        self.kill.store(false, Ordering::SeqCst);
    }
    pub fn is_killed(&self) -> bool {
        self.kill.load(Ordering::SeqCst)
    }
    pub fn pause(&mut self, strategy: &str) {
        if !self.paused.iter().any(|s| s == strategy) {
            self.paused.push(strategy.into());
        }
    }
    pub fn resume(&mut self, strategy: &str) {
        self.paused.retain(|s| s != strategy);
    }
    pub fn is_paused(&self, strategy: &str) -> bool {
        self.paused.iter().any(|s| s == strategy)
    }
    pub fn paused(&self) -> Vec<String> {
        self.paused.clone()
    }
}

/// Prometheus text exposition for the handful of series that matter.
pub fn render_metrics(
    status: &EngineStatus,
    per_strategy_trades: &BTreeMap<String, u64>,
) -> String {
    let mut s = String::new();
    s.push_str("# HELP arb_trades_total Executed intents\n# TYPE arb_trades_total counter\n");
    s.push_str(&format!("arb_trades_total {}\n", status.trades));
    s.push_str("# HELP arb_opportunities_total Detected opportunities\n# TYPE arb_opportunities_total counter\n");
    s.push_str(&format!(
        "arb_opportunities_total {}\n",
        status.opportunities_seen
    ));
    s.push_str("# HELP arb_realized_pnl_quote Realized PnL in quote\n# TYPE arb_realized_pnl_quote gauge\n");
    s.push_str(&format!(
        "arb_realized_pnl_quote {}\n",
        status.realized_pnl_quote
    ));
    s.push_str("# HELP arb_killed Kill-switch state\n# TYPE arb_killed gauge\n");
    s.push_str(&format!(
        "arb_killed {}\n",
        if status.killed { 1 } else { 0 }
    ));
    for (k, v) in per_strategy_trades {
        s.push_str(&format!(
            "arb_strategy_trades_total{{strategy=\"{k}\"}} {v}\n"
        ));
    }
    for c in &status.connectors {
        s.push_str(&format!(
            "arb_connector_up{{venue=\"{}\"}} {}\n",
            c.venue,
            if c.connected { 1 } else { 0 }
        ));
    }
    s
}

/// Route a control request path. Returns (status_code, body).
/// POST semantics are modeled as path commands to keep the transport trivial:
///   GET  /health /status /metrics
///   POST /kill /revive /pause/<strategy> /resume/<strategy>
pub fn route(
    method: &str,
    path: &str,
    control: &mut Control,
    status: &EngineStatus,
    metrics: &str,
) -> (u16, String) {
    match (method, path) {
        ("GET", "/health") => (200, "{\"ok\":true}".into()),
        ("GET", "/status") => {
            // Overlay live control state: the engine publishes snapshots per
            // tick, so paused/killed from the snapshot can lag a just-issued
            // POST. Control is the source of truth for those two fields.
            let mut live = status.clone();
            live.killed = control.is_killed();
            live.paused_strategies = control.paused();
            (200, serde_json::to_string(&live).unwrap_or_default())
        }
        ("GET", "/metrics") => (200, metrics.into()),
        ("POST", "/kill") => {
            control.kill();
            (200, "{\"killed\":true}".into())
        }
        ("POST", "/revive") => {
            control.revive();
            (200, "{\"killed\":false}".into())
        }
        ("POST", p) if p.starts_with("/pause/") => {
            control.pause(&p["/pause/".len()..]);
            (200, "{\"paused\":true}".into())
        }
        ("POST", p) if p.starts_with("/resume/") => {
            control.resume(&p["/resume/".len()..]);
            (200, "{\"paused\":false}".into())
        }
        _ => (404, "{\"error\":\"not found\"}".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn routes_and_metrics() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut c = Control::new(flag.clone());
        let st = EngineStatus {
            mode: "paper".into(),
            killed: false,
            trades: 3,
            realized_pnl_quote: dec!(1.5),
            opportunities_seen: 9,
            connectors: vec![ConnectorHealth {
                venue: Venue::Binance,
                connected: true,
                last_msg_ms: 1,
                books: 2,
            }],
            paused_strategies: vec![],
        };
        let m = render_metrics(&st, &BTreeMap::from([("cross_exchange".to_string(), 3)]));
        assert!(m.contains("arb_trades_total 3"));
        assert_eq!(route("GET", "/health", &mut c, &st, &m).0, 200);
        assert_eq!(route("POST", "/kill", &mut c, &st, &m).0, 200);
        assert!(flag.load(Ordering::SeqCst));
        route("POST", "/pause/triangular", &mut c, &st, &m);
        assert!(c.is_paused("triangular"));
        // Regression: /status must reflect the pause immediately, even though
        // the caller's snapshot still has paused_strategies=[].
        let (_, body) = route("GET", "/status", &mut c, &st, &m);
        assert!(body.contains("\"triangular\""), "status body: {body}");
        assert!(body.contains("\"killed\":true"), "status body: {body}");
        assert_eq!(route("GET", "/nope", &mut c, &st, &m).0, 404);
    }
}
