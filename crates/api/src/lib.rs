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
    /// Socket-level state, independent of whether books are fresh.
    pub connected: bool,
    /// Last WebSocket frame of any kind received, if ever.
    pub last_msg_ms: Option<i64>,
    /// Newest stored book timestamp for this venue, if any.
    pub last_book_ms: Option<i64>,
    /// Books currently held for this venue.
    pub books: usize,
    /// Held books inside the freshness window and not crossed.
    pub fresh_books: usize,
    pub last_error: Option<String>,
}

/// Engine-boundary funnel counters for one strategy: how many intents it
/// emitted, and where they stopped. Decisions inside a strategy (for
/// example stale-book skips before emitting) are not counted here.
#[derive(Debug, Clone, Default)]
pub struct StrategyMetrics {
    pub scans: u64,
    pub emitted: u64,
    pub risk_rejected: u64,
    pub fill_failed: u64,
    pub filled: u64,
    pub realized_pnl_quote: Decimal,
}

/// Health of the live USDT/IDR divisor used to normalize Indodax books.
/// `fresh` means a rate was accepted for conversion; `degraded` means the
/// cross-checks could not confirm it (or no rate at all).
#[derive(Debug, Clone, Serialize)]
pub struct UsdtIdrHealth {
    pub mid: Option<Decimal>,
    pub spread_bps: Option<Decimal>,
    pub book_ts_ms: Option<i64>,
    pub age_ms: Option<i64>,
    pub fresh: bool,
    pub degraded: bool,
    pub consensus_mid: Option<Decimal>,
    pub deviation_bps: Option<Decimal>,
    pub independent_sources: usize,
    pub state: String,
}

impl Default for UsdtIdrHealth {
    fn default() -> Self {
        Self {
            mid: None,
            spread_bps: None,
            book_ts_ms: None,
            age_ms: None,
            fresh: false,
            degraded: true,
            consensus_mid: None,
            deviation_bps: None,
            independent_sources: 0,
            state: "missing_primary".into(),
        }
    }
}

/// One ledger line: free balance of one asset on one venue.
#[derive(Debug, Clone, Serialize)]
pub struct VenueBalance {
    pub venue: Venue,
    pub asset: String,
    pub amount: Decimal,
}

/// Mark-to-market portfolio. `unrealized` is equity now minus the seeded
/// baseline minus closed-trade (realized) P/L; assets with no live mark are
/// excluded from equity rather than guessed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PortfolioStatus {
    pub per_venue: Vec<VenueBalance>,
    pub equity_quote: Decimal,
    pub baseline_quote: Decimal,
    pub realized_pnl_quote: Decimal,
    pub unrealized_pnl_quote: Decimal,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub mode: String,
    pub killed: bool,
    /// Wall-clock start of this engine run; the viewer renders uptime from it.
    pub started_ms: i64,
    pub trades: u64,
    pub realized_pnl_quote: Decimal,
    pub opportunities_seen: u64,
    pub connectors: Vec<ConnectorHealth>,
    pub paused_strategies: Vec<String>,
    pub usdt_idr: UsdtIdrHealth,
    pub portfolio: PortfolioStatus,
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
    per_strategy: &BTreeMap<String, StrategyMetrics>,
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
    s.push_str(
        "# HELP arb_equity_quote Mark-to-market equity in quote\n# TYPE arb_equity_quote gauge\n",
    );
    s.push_str(&format!(
        "arb_equity_quote {}\n",
        status.portfolio.equity_quote
    ));
    s.push_str("# HELP arb_unrealized_pnl_quote Open-position PnL in quote\n# TYPE arb_unrealized_pnl_quote gauge\n");
    s.push_str(&format!(
        "arb_unrealized_pnl_quote {}\n",
        status.portfolio.unrealized_pnl_quote
    ));
    s.push_str("# HELP arb_killed Kill-switch state\n# TYPE arb_killed gauge\n");
    s.push_str(&format!(
        "arb_killed {}\n",
        if status.killed { 1 } else { 0 }
    ));
    for (k, m) in per_strategy {
        s.push_str(&format!(
            "arb_strategy_scans_total{{strategy=\"{k}\"}} {}\n",
            m.scans
        ));
        s.push_str(&format!(
            "arb_strategy_intents_total{{strategy=\"{k}\"}} {}\n",
            m.emitted
        ));
        s.push_str(&format!(
            "arb_strategy_risk_rejected_total{{strategy=\"{k}\"}} {}\n",
            m.risk_rejected
        ));
        s.push_str(&format!(
            "arb_strategy_fill_failed_total{{strategy=\"{k}\"}} {}\n",
            m.fill_failed
        ));
        s.push_str(&format!(
            "arb_strategy_trades_total{{strategy=\"{k}\"}} {}\n",
            m.filled
        ));
        s.push_str(&format!(
            "arb_strategy_realized_pnl_quote{{strategy=\"{k}\"}} {}\n",
            m.realized_pnl_quote
        ));
    }
    for c in &status.connectors {
        s.push_str(&format!(
            "arb_connector_up{{venue=\"{}\"}} {}\n",
            c.venue,
            if c.connected { 1 } else { 0 }
        ));
        s.push_str(&format!(
            "arb_connector_books{{venue=\"{}\"}} {}\n",
            c.venue, c.books
        ));
        s.push_str(&format!(
            "arb_connector_fresh_books{{venue=\"{}\"}} {}\n",
            c.venue, c.fresh_books
        ));
    }
    let fx = &status.usdt_idr;
    s.push_str("# HELP arb_usdt_idr_fresh Live USDT/IDR divisor accepted\n# TYPE arb_usdt_idr_fresh gauge\n");
    s.push_str(&format!(
        "arb_usdt_idr_fresh {}\n",
        if fx.fresh { 1 } else { 0 }
    ));
    s.push_str(&format!(
        "arb_usdt_idr_degraded {}\n",
        if fx.degraded { 1 } else { 0 }
    ));
    s.push_str(&format!(
        "arb_usdt_idr_independent_sources {}\n",
        fx.independent_sources
    ));
    if let Some(mid) = fx.mid {
        s.push_str(&format!("arb_usdt_idr_mid {mid}\n"));
    }
    if let Some(spread) = fx.spread_bps {
        s.push_str(&format!("arb_usdt_idr_spread_bps {spread}\n"));
    }
    if let Some(age) = fx.age_ms {
        s.push_str(&format!("arb_usdt_idr_age_ms {age}\n"));
    }
    if let Some(dev) = fx.deviation_bps {
        s.push_str(&format!("arb_usdt_idr_deviation_bps {dev}\n"));
    }
    s
}

/// Recent-trade feed for the 3D strategy-flow viewer.
/// `items` are pre-serialized trades, oldest first; `total` is the full count.
pub fn render_flows_envelope(items: &[serde_json::Value], total: usize) -> String {
    serde_json::json!({"flows": items, "count": total}).to_string()
}

/// The viewer page, served as `GET /viewer` by the engine binary.
pub const VIEWER_HTML: &str = include_str!("viewer.html");

/// Viewer routes. Returns (status_code, body, content_type) when matched.
/// `flows_body` is the snapshot rendered by [`render_flows_envelope`].
pub fn route_viewer(
    method: &str,
    path: &str,
    flows_body: &str,
) -> Option<(u16, String, &'static str)> {
    match (method, path) {
        ("GET", "/flows") => Some((200, flows_body.to_string(), "application/json")),
        ("GET", "/viewer") => Some((200, VIEWER_HTML.to_string(), "text/html")),
        _ => None,
    }
}

/// Route a control request path. Returns (status_code, body).
/// POST semantics are modeled as path commands to keep the transport trivial:
///   GET  /health /status /metrics
///   POST /kill /revive /pause/<strategy> /resume/<strategy>
/// When `expected_token` is Some, POST control paths require
/// `?token=<value>` and return 401 without it. Reads stay open.
pub fn route(
    method: &str,
    path: &str,
    control: &mut Control,
    status: &EngineStatus,
    metrics: &str,
    expected_token: Option<&str>,
) -> (u16, String) {
    let (path, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };
    let is_control = path == "/kill"
        || path == "/revive"
        || path.starts_with("/pause/")
        || path.starts_with("/resume/");
    if method == "POST" && is_control {
        let ok = match expected_token {
            None => true,
            Some(t) => query
                .split('&')
                .any(|kv| kv.strip_prefix("token=") == Some(t)),
        };
        if !ok {
            return (401, "{\"error\":\"unauthorized\"}".into());
        }
    }
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
            started_ms: 1_000,
            trades: 3,
            realized_pnl_quote: dec!(1.5),
            opportunities_seen: 9,
            connectors: vec![ConnectorHealth {
                venue: Venue::Binance,
                connected: true,
                last_msg_ms: Some(1),
                last_book_ms: Some(1),
                books: 2,
                fresh_books: 1,
                last_error: None,
            }],
            paused_strategies: vec![],
            usdt_idr: UsdtIdrHealth {
                mid: Some(dec!(18111.5)),
                spread_bps: Some(dec!(0.55)),
                book_ts_ms: Some(1_000),
                age_ms: Some(250),
                fresh: true,
                degraded: false,
                consensus_mid: Some(dec!(18105)),
                deviation_bps: Some(dec!(3.6)),
                independent_sources: 3,
                state: "ok".into(),
            },
            portfolio: PortfolioStatus {
                per_venue: vec![VenueBalance {
                    venue: Venue::Binance,
                    asset: "USDT".into(),
                    amount: dec!(100),
                }],
                equity_quote: dec!(100),
                baseline_quote: dec!(100),
                realized_pnl_quote: dec!(1.5),
                unrealized_pnl_quote: dec!(-1.5),
            },
        };
        let m = render_metrics(
            &st,
            &BTreeMap::from([(
                "cross_exchange".to_string(),
                StrategyMetrics {
                    scans: 10,
                    emitted: 5,
                    risk_rejected: 1,
                    fill_failed: 1,
                    filled: 3,
                    realized_pnl_quote: dec!(1.5),
                },
            )]),
        );
        assert!(m.contains("arb_trades_total 3"));
        assert!(m.contains("arb_strategy_intents_total{strategy=\"cross_exchange\"} 5"));
        assert!(m.contains("arb_strategy_risk_rejected_total{strategy=\"cross_exchange\"} 1"));
        assert!(
            m.contains("arb_connector_fresh_books{venue=\"binance\"} 1"),
            "{m}"
        );
        assert!(m.contains("arb_usdt_idr_mid 18111.5"), "{m}");
        assert!(m.contains("arb_usdt_idr_fresh 1"), "{m}");
        assert!(m.contains("arb_usdt_idr_independent_sources 3"), "{m}");
        assert!(m.contains("arb_equity_quote 100"), "{m}");
        assert!(m.contains("arb_unrealized_pnl_quote -1.5"), "{m}");
        assert_eq!(route("GET", "/health", &mut c, &st, &m, None).0, 200);
        assert_eq!(route("POST", "/kill", &mut c, &st, &m, None).0, 200);
        assert!(flag.load(Ordering::SeqCst));
        route("POST", "/pause/triangular", &mut c, &st, &m, None);
        assert!(c.is_paused("triangular"));
        // Regression: /status must reflect the pause immediately, even though
        // the caller's snapshot still has paused_strategies=[].
        let (_, body) = route("GET", "/status", &mut c, &st, &m, None);
        assert!(body.contains("\"usdt_idr\""), "status body: {body}");
        assert!(body.contains("\"state\":\"ok\""), "status body: {body}");
        assert!(body.contains("\"triangular\""), "status body: {body}");
        assert!(body.contains("\"killed\":true"), "status body: {body}");
        assert!(body.contains("\"started_ms\":1000"), "status body: {body}");
        assert!(body.contains("\"portfolio\""), "status body: {body}");
        assert!(
            body.contains("\"equity_quote\":\"100\""),
            "status body: {body}"
        );
        assert_eq!(route("GET", "/nope", &mut c, &st, &m, None).0, 404);
    }

    #[test]
    fn control_posts_need_token_when_set() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut c = Control::new(flag.clone());
        let st = EngineStatus {
            mode: "paper".into(),
            killed: false,
            started_ms: 1_000,
            trades: 0,
            realized_pnl_quote: dec!(0),
            opportunities_seen: 0,
            connectors: vec![],
            paused_strategies: vec![],
            usdt_idr: UsdtIdrHealth::default(),
            portfolio: PortfolioStatus {
                per_venue: vec![],
                equity_quote: dec!(0),
                baseline_quote: dec!(0),
                realized_pnl_quote: dec!(0),
                unrealized_pnl_quote: dec!(0),
            },
        };
        // Reads stay open; control posts are rejected without the token.
        assert_eq!(
            route("GET", "/health", &mut c, &st, "", Some("s3cr3t")).0,
            200
        );
        assert_eq!(
            route("POST", "/kill", &mut c, &st, "", Some("s3cr3t")).0,
            401
        );
        assert!(!flag.load(Ordering::SeqCst));
        assert_eq!(
            route(
                "POST",
                "/kill?token=s3cr3t",
                &mut c,
                &st,
                "",
                Some("s3cr3t")
            )
            .0,
            200
        );
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn viewer_flows_and_page() {
        let items = vec![serde_json::json!({"intent": {"strategy": "triangular"}})];
        let body = render_flows_envelope(&items, 42);
        assert!(body.contains("\"count\":42"), "flows body: {body}");
        let (code, out, ctype) = route_viewer("GET", "/flows", &body).unwrap();
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");
        assert!(out.contains("triangular"), "flows out: {out}");
        let (code, page, ctype) = route_viewer("GET", "/viewer", &body).unwrap();
        assert_eq!(code, 200);
        assert_eq!(ctype, "text/html");
        assert!(page.contains("strategy-flow"), "viewer page");
        assert!(route_viewer("GET", "/nope", &body).is_none());
        assert!(route_viewer("POST", "/flows", &body).is_none());
    }
}
