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

/// Funnel counters for one strategy: pair evaluations rejected by each
/// strategy gate, plus how many emitted intents stopped at the engine.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StrategyMetrics {
    pub scans: u64,
    pub emitted: u64,
    pub edge_rejected: u64,
    pub imbalance_rejected: u64,
    pub min_notional_rejected: u64,
    pub stale_rejected: u64,
    pub share_rejected: u64,
    pub risk_rejected: u64,
    pub fill_failed: u64,
    pub filled: u64,
    /// Intents dropped because every leg's top of book was byte-identical
    /// to the last fill's (paper never consumes depth, so without this the
    /// same print refires every tick).
    pub dup_skipped: u64,
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

/// One open position held by a strategy, shown as a pending order.
/// `qty_base` + `entry_price_quote` come from the opening trade in history;
/// `mark_price_quote` + `market_value_quote` use the same live marks as
/// equity (None when no fresh mark exists — shown as stale, never guessed).
#[derive(Debug, Clone, Serialize)]
pub struct OpenPosition {
    pub strategy: String,
    pub venue: Venue,
    pub symbol: String,
    pub qty_base: Decimal,
    pub entry_price_quote: Option<Decimal>,
    pub mark_price_quote: Option<Decimal>,
    pub market_value_quote: Option<Decimal>,
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
    /// S6 gate threshold on `cross_exchange`; None = off.
    pub imbalance_threshold: Option<Decimal>,
    pub open_positions: Vec<OpenPosition>,
    pub usdt_idr: UsdtIdrHealth,
    pub portfolio: PortfolioStatus,
    /// Completed paper-only venue rebalances (live never auto-moves).
    pub transfers: u64,
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
            "arb_strategy_gate_rejected_total{{strategy=\"{k}\",gate=\"edge\"}} {}\n",
            m.edge_rejected
        ));
        s.push_str(&format!(
            "arb_strategy_gate_rejected_total{{strategy=\"{k}\",gate=\"imbalance\"}} {}\n",
            m.imbalance_rejected
        ));
        s.push_str(&format!(
            "arb_strategy_gate_rejected_total{{strategy=\"{k}\",gate=\"min_notional\"}} {}\n",
            m.min_notional_rejected
        ));
        s.push_str(&format!(
            "arb_strategy_gate_rejected_total{{strategy=\"{k}\",gate=\"stale\"}} {}\n",
            m.stale_rejected
        ));
        s.push_str(&format!(
            "arb_strategy_gate_rejected_total{{strategy=\"{k}\",gate=\"share\"}} {}\n",
            m.share_rejected
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
            "arb_strategy_dup_skipped_total{{strategy=\"{k}\"}} {}\n",
            m.dup_skipped
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

/// The strategy-flow page (3D scene + ops drawer), served as `GET /` by the
/// engine binary.
pub const INDEX_HTML: &str = include_str!("index.html");
/// The ops dashboard. Same bytes as the root page: one page, two paths.
pub const DASHBOARD_HTML: &str = INDEX_HTML;

/// Per-strategy funnel counters as JSON (read-open, like `/status`).
pub fn render_strategies(per_strategy: &BTreeMap<String, StrategyMetrics>) -> String {
    serde_json::to_string(per_strategy).unwrap_or_else(|_| "{}".into())
}

/// Page + data routes. Returns (status_code, body, content_type) when matched.
/// `flows_body` is the snapshot rendered by [`render_flows_envelope`];
/// `strategies_body` by [`render_strategies`]. `/viewer` is retired.
pub fn route_pages(
    method: &str,
    path: &str,
    flows_body: &str,
    strategies_body: &str,
) -> Option<(u16, String, &'static str)> {
    match (method, path) {
        ("GET", "/") => Some((200, INDEX_HTML.to_string(), "text/html")),
        ("GET", "/dashboard") => Some((200, DASHBOARD_HTML.to_string(), "text/html")),
        ("GET", "/flows") => Some((200, flows_body.to_string(), "application/json")),
        ("GET", "/strategies") => Some((200, strategies_body.to_string(), "application/json")),
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
            imbalance_threshold: Some(dec!(0.2)),
            open_positions: vec![OpenPosition {
                strategy: "funding_rate".into(),
                venue: Venue::Bybit,
                symbol: "BTC/USDT".into(),
                qty_base: dec!(0.5),
                entry_price_quote: Some(dec!(100)),
                mark_price_quote: Some(dec!(101)),
                market_value_quote: Some(dec!(50.5)),
            }],
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
            transfers: 0,
        };
        let m = render_metrics(
            &st,
            &BTreeMap::from([(
                "cross_exchange".to_string(),
                StrategyMetrics {
                    scans: 10,
                    emitted: 5,
                    edge_rejected: 7,
                    imbalance_rejected: 3,
                    min_notional_rejected: 2,
                    stale_rejected: 4,
                    share_rejected: 1,
                    risk_rejected: 1,
                    fill_failed: 1,
                    filled: 3,
                    dup_skipped: 2,
                    realized_pnl_quote: dec!(1.5),
                },
            )]),
        );
        assert!(m.contains("arb_trades_total 3"));
        assert!(m.contains("arb_strategy_intents_total{strategy=\"cross_exchange\"} 5"));
        assert!(m.contains("arb_strategy_risk_rejected_total{strategy=\"cross_exchange\"} 1"));
        assert!(m.contains(
            "arb_strategy_gate_rejected_total{strategy=\"cross_exchange\",gate=\"edge\"} 7"
        ));
        assert!(m.contains(
            "arb_strategy_gate_rejected_total{strategy=\"cross_exchange\",gate=\"imbalance\"} 3"
        ));
        assert!(m.contains(
            "arb_strategy_gate_rejected_total{strategy=\"cross_exchange\",gate=\"share\"} 1"
        ));
        assert!(m.contains("arb_strategy_dup_skipped_total{strategy=\"cross_exchange\"} 2"));
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
        assert!(
            body.contains("\"imbalance_threshold\""),
            "status body: {body}"
        );
        assert!(body.contains("\"state\":\"ok\""), "status body: {body}");
        assert!(body.contains("\"triangular\""), "status body: {body}");
        assert!(body.contains("\"killed\":true"), "status body: {body}");
        assert!(body.contains("\"started_ms\":1000"), "status body: {body}");
        assert!(body.contains("\"portfolio\""), "status body: {body}");
        assert!(
            body.contains("\"equity_quote\":\"100\""),
            "status body: {body}"
        );
        assert!(
            body.contains("\"open_positions\":[{\"strategy\":\"funding_rate\",\"venue\":\"bybit\",\"symbol\":\"BTC/USDT\",\"qty_base\":\"0.5\""),
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
            imbalance_threshold: None,
            open_positions: vec![],
            usdt_idr: UsdtIdrHealth::default(),
            portfolio: PortfolioStatus {
                per_venue: vec![],
                equity_quote: dec!(0),
                baseline_quote: dec!(0),
                realized_pnl_quote: dec!(0),
                unrealized_pnl_quote: dec!(0),
            },
            transfers: 0,
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
    fn pages_serve_root_flows_and_strategies() {
        let items = vec![serde_json::json!({"intent": {"strategy": "triangular"}})];
        let flows = render_flows_envelope(&items, 42);
        assert!(flows.contains("\"count\":42"), "flows body: {flows}");
        let per = BTreeMap::from([(
            "cross_exchange".to_string(),
            StrategyMetrics {
                scans: 10,
                emitted: 5,
                risk_rejected: 1,
                fill_failed: 1,
                filled: 3,
                dup_skipped: 2,
                realized_pnl_quote: dec!(1.5),
                ..Default::default()
            },
        )]);
        let strat = render_strategies(&per);
        assert!(strat.contains("cross_exchange"), "strategies: {strat}");
        assert!(strat.contains("\"filled\":3"), "strategies: {strat}");
        let (code, out, ctype) = route_pages("GET", "/flows", &flows, &strat).unwrap();
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");
        assert!(out.contains("triangular"), "flows out: {out}");
        let (code, out, ctype) = route_pages("GET", "/strategies", &flows, &strat).unwrap();
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");
        assert!(out.contains("cross_exchange"), "strategies out: {out}");
        // Root page and /dashboard serve the same bytes.
        let (code, root, ctype) = route_pages("GET", "/", &flows, &strat).unwrap();
        assert_eq!(code, 200);
        assert_eq!(ctype, "text/html");
        let (code, dash, _) = route_pages("GET", "/dashboard", &flows, &strat).unwrap();
        assert_eq!(code, 200);
        assert_eq!(dash, root);
        assert!(root.contains("strategy-flow"), "page heading");
        // All active engine strategies must have a scene series; otherwise
        // their flows fetch but never draw.
        for s in ["cross_exchange", "triangular", "funding_rate", "cex_dex"] {
            assert!(root.contains(s), "page hides {s}");
        }
        // Token-gated controls ship on the page.
        assert!(root.contains("kill"), "page hides controls");
        // /viewer is retired; POSTs and unknown paths miss.
        assert!(route_pages("GET", "/viewer", &flows, &strat).is_none());
        assert!(route_pages("GET", "/nope", &flows, &strat).is_none());
        assert!(route_pages("POST", "/flows", &flows, &strat).is_none());
        assert!(route_pages("POST", "/", &flows, &strat).is_none());
    }

    #[test]
    fn viewer_zoom_anchors_at_cursor() {
        // Zoom keeps the point under the cursor fixed: applyView() maps
        // screen = (world - center) * s + center + offset, so the offsets
        // must unwind the canvas-center term, not the raw origin.
        assert!(
            INDEX_HTML.contains("mx - w / 2 - (mx - w / 2 - view.ox) * k"),
            "zoom x offset must anchor at the cursor around canvas center"
        );
        assert!(
            INDEX_HTML.contains("my - h / 2 - (my - h / 2 - view.oy) * k"),
            "zoom y offset must anchor at the cursor around canvas center"
        );
        assert!(
            !INDEX_HTML.contains("view.ox = mx - (mx - view.ox) * k"),
            "old origin-anchored zoom math must be gone: it flings the scene on wheel zoom"
        );
    }
}
