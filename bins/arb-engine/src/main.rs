mod config;
mod engine;

use arb_api::{
    render_flows_envelope, render_metrics, render_strategies, route, route_dashboard, route_viewer,
    ConnectorHealth, Control, EngineStatus, OpenPosition, PortfolioStatus, StrategyMetrics,
    UsdtIdrHealth, VenueBalance,
};
use arb_core::Intent;
use arb_core::MarketKind;
use arb_core::Side;
use arb_core::{now_ms, Symbol, Venue};
use arb_exec::{keys_from_env, ExecMode, LiveExecutor};
use arb_feed::{
    implied_usdt_idr, pick_fresh, resolve_usdt_idr, FxCrossCheck, LiveBooks, LiveConnectors,
    LiveDexQuotes, LiveFunding, RateDecision, UsdtIdrRate,
};
use arb_paper::{PaperConfig, PaperExecutor, PaperFill};
use arb_store::{MemoryStore, SessionStart, Store, StoredTrade, StoredTransfer};
use arb_strategy::{
    CexDex, CrossExchange, FundingStrategy, FxTriangle, LagChase, MarketView, Strategy, Triangular,
};
use clap::Parser;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(
    name = "arb-engine",
    about = "Multi-venue, multi-strategy arbitrage engine (paper-first)"
)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: String,
    /// Stop after N ticks (0 = run until Ctrl-C). Handy for smoke tests.
    #[arg(long, default_value_t = 0)]
    ticks: u64,
}

/// State shared between the tick loop and the control API task.
struct Shared {
    control: Mutex<Control>,
    status: Mutex<EngineStatus>,
    per_strategy: Mutex<BTreeMap<String, StrategyMetrics>>,
    /// Pre-rendered `/flows` snapshot (last `FLOWS_SNAPSHOT` trades).
    flows: Mutex<String>,
    /// Live venue books written by the WS tasks (empty before first frames).
    live_books: LiveBooks,
    /// Socket-level state written by the live venue tasks.
    live_connectors: LiveConnectors,
    /// Latest independent USDT/IDR sample (Upbit Indonesia cross-check).
    fx_cross: FxCrossCheck,
    /// Latest polled DEX quotes, written by the DEX tasks. `cex_dex` drops
    /// any quote older than `dex_max_quote_age_ms` itself.
    live_dex: LiveDexQuotes,
    /// Latest perp funding prints with receipt time. Entries older than
    /// `max_book_age_ms` never reach the strategy.
    live_funding: LiveFunding,
}

const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// One engine run = one session. The id is stamped on the session-start
/// record and every trade so restarts are visible in the JSONL history.
fn new_session_id() -> String {
    format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        SESSION_SEQ.fetch_add(1, Ordering::SeqCst)
    )
}

/// Pairs with a filled, still-open funding entry in stored history.
/// There are no closing fills yet, so every stored non-roundtrip
/// `funding_rate` trade counts as open.
fn restored_funding_pairs(trades: &[StoredTrade]) -> Vec<(Venue, Symbol)> {
    let mut out: Vec<(Venue, Symbol)> = Vec::new();
    for t in trades {
        if t.intent.strategy != "funding_rate" || t.intent.roundtrip {
            continue;
        }
        if let Some(leg) = t.intent.legs.first() {
            let pair = (leg.venue, leg.symbol.clone());
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    out
}

/// Trades kept in the `/flows` snapshot served to the 3D viewer.
const FLOWS_SNAPSHOT: usize = 200;

/// Reference marks (asset -> USDT price) from the anchor table for the
/// portfolio baseline. Stablecoins count at one; the ETH/BTC cross and any
/// non-USDT pair never appear here.
fn anchor_marks() -> BTreeMap<String, Decimal> {
    let mut marks = BTreeMap::from([
        ("USDT".to_string(), Decimal::ONE),
        ("USDC".to_string(), Decimal::ONE),
    ]);
    for (pair, px) in anchors() {
        let mut parts = pair.split('/');
        if let (Some(base), Some("USDT")) = (parts.next(), parts.next()) {
            marks.insert(base.to_string(), px);
        }
    }
    marks
}

fn anchors() -> BTreeMap<String, Decimal> {
    BTreeMap::from([
        ("BTC/USDT".to_string(), Decimal::new(80_000, 0)),
        ("ETH/USDT".to_string(), Decimal::new(2_300, 0)),
        ("ETH/BTC".to_string(), Decimal::new(3, 2)), // 0.03 — triangular needs the cross
        ("SOL/USDT".to_string(), Decimal::new(100, 0)),
        ("SOL/USDC".to_string(), Decimal::new(100, 0)),
        ("BNB/USDT".to_string(), Decimal::new(740, 0)),
        ("ADA/USDT".to_string(), Decimal::new(2, 1)),
        ("LINK/USDT".to_string(), Decimal::new(113, 1)),
        ("TRX/USDT".to_string(), Decimal::new(33, 2)),
        ("LTC/USDT".to_string(), Decimal::new(56, 0)),
        ("DOT/USDT".to_string(), Decimal::new(1, 0)),
        ("AVAX/USDT".to_string(), Decimal::new(88, 1)),
        ("UNI/USDT".to_string(), Decimal::new(71, 1)),
        ("ATOM/USDT".to_string(), Decimal::new(16, 1)),
        ("NEAR/USDT".to_string(), Decimal::new(45, 1)),
        ("XLM/USDT".to_string(), Decimal::new(17, 2)),
        ("ETC/USDT".to_string(), Decimal::new(75, 1)),
        ("ALGO/USDT".to_string(), Decimal::new(1, 1)),
        ("FIL/USDT".to_string(), Decimal::new(9, 1)),
        ("SUI/USDT".to_string(), Decimal::new(9, 1)),
        ("XRP/USDT".to_string(), Decimal::new(13, 1)),
        ("DOGE/USDT".to_string(), Decimal::new(78, 3)),
    ])
}

/// True when one intent spends more quote on a single venue than
/// `share_bps` of that venue's USDT+USDC ledger. Only Buy legs spend quote;
/// sells don't. `share_bps <= 0` disables the check. Caps ruin-per-print
/// while letting large prints through wherever the venue funds them.
fn venue_quote_exceeded(intent: &Intent, paper: &PaperExecutor, share_bps: Decimal) -> bool {
    if share_bps <= Decimal::ZERO {
        return false;
    }
    let mut spend: BTreeMap<Venue, Decimal> = BTreeMap::new();
    for leg in &intent.legs {
        if leg.side == Side::Buy {
            *spend.entry(leg.venue).or_default() += leg.price * leg.qty_base;
        }
    }
    spend.into_iter().any(|(v, s)| {
        let ledger = paper.balance(v, "USDT") + paper.balance(v, "USDC");
        ledger <= Decimal::ZERO || s > ledger * share_bps / Decimal::new(10_000, 0)
    })
}

/// Key identifying one repeatable opportunity: same strategy, same legs.
fn fill_key(intent: &Intent) -> String {
    let mut k = intent.strategy.clone();
    for leg in &intent.legs {
        use std::fmt::Write as _;
        let _ = write!(k, "|{:?}:{}/{:?}", leg.venue, leg.symbol, leg.side);
    }
    k
}

/// Fingerprint of the tops an intent would fill against, read from the same
/// books `execute` uses. Two prints match only when every leg's top of book
/// is identical — same prices, same sizes. Top levels (not timestamps: the
/// parsers stamp arrival time, so `ts_ms` moves even when prices don't).
fn fill_print(
    intent: &Intent,
    books: &BTreeMap<(Venue, String), arb_core::OrderBook>,
) -> Option<String> {
    let mut p = String::new();
    for leg in &intent.legs {
        let b = books.get(&(leg.venue, leg.symbol.to_string()))?;
        let (ask, bid) = (b.best_ask()?, b.best_bid()?);
        use std::fmt::Write as _;
        let _ = write!(p, "{}/{}/{}/{}|", ask.price, ask.qty, bid.price, bid.qty);
    }
    Some(p)
}

async fn serve_api(addr: String, token: Option<String>, shared: Arc<Shared>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%e, addr, "api bind failed");
            return;
        }
    };
    if token.is_none() && !addr.starts_with("127.") && !addr.starts_with("localhost") {
        // shortcut: remote binds without a token leave POST /kill open;
        // set API_TOKEN. Reads stay open either way.
        tracing::warn!(addr, "control API on a non-loopback bind without API_TOKEN; POST /kill /revive /pause /resume need ?token=");
    }
    tracing::info!(addr, "control API listening (GET /health /status /metrics /flows /viewer /dashboard /strategies, POST /kill /revive /pause/<s> /resume/<s>)");
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let (shared, token) = (shared.clone(), token.clone());
        tokio::spawn(async move {
            // Read until the header block ends (or 64KB); one 2048-byte
            // read truncated long requests.
            let mut raw: Vec<u8> = Vec::with_capacity(2048);
            let mut buf = [0u8; 1024];
            loop {
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 65536 {
                    break;
                }
                let head = String::from_utf8_lossy(&raw);
                if head.contains("\r\n\r\n") || head.contains("\n\n") {
                    break;
                }
            }
            if raw.is_empty() {
                return;
            }
            let req = String::from_utf8_lossy(&raw).into_owned();
            let mut parts = req.split_whitespace();
            let (method, path) = (parts.next().unwrap_or("GET"), parts.next().unwrap_or("/"));
            // Route through the tested table in arb-api; no parallel handlers.
            let (code, body, ctype) = {
                let status = shared
                    .status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or(EngineStatus {
                        mode: "unknown".into(),
                        killed: true,
                        started_ms: 0,
                        trades: 0,
                        realized_pnl_quote: Decimal::ZERO,
                        opportunities_seen: 0,
                        connectors: vec![],
                        paused_strategies: vec![],
                        imbalance_threshold: None,
                        open_positions: vec![],
                        usdt_idr: UsdtIdrHealth::default(),
                        portfolio: PortfolioStatus::default(),
                        transfers: 0,
                    });
                let per = shared
                    .per_strategy
                    .lock()
                    .map(|p| p.clone())
                    .unwrap_or_default();
                let metrics = render_metrics(&status, &per);
                let flows = shared
                    .flows
                    .lock()
                    .map(|f| f.clone())
                    .unwrap_or_else(|_| render_flows_envelope(&[], 0));
                let strategies = render_strategies(&per);
                if let Some(hit) = route_viewer(method, path, &flows) {
                    hit
                } else if let Some(hit) = route_dashboard(method, path, &strategies) {
                    hit
                } else {
                    match shared.control.lock() {
                        Ok(mut ctl) => {
                            let (code, body) =
                                route(method, path, &mut ctl, &status, &metrics, token.as_deref());
                            let ctype = if path == "/metrics" {
                                "text/plain"
                            } else {
                                "application/json"
                            };
                            (code, body, ctype)
                        }
                        Err(_) => (
                            500,
                            "{\"error\":\"lock poisoned\"}".into(),
                            "application/json",
                        ),
                    }
                }
            };
            let _ = sock
                .write_all(
                    format!(
                        "HTTP/1.1 {code} OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
    }
}

/// Split one `.env` line into (key, value). Blanks, comments, and lines
/// without `=` return None. One layer of matching quotes is stripped.
fn parse_dotenv_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let body = line.strip_prefix("export ").unwrap_or(line).trim();
    if body.is_empty() || body.starts_with('#') {
        return None;
    }
    let (key, mut value) = body.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    value = value.trim();
    if value.len() >= 2
        && value.starts_with(['\'', '"'])
        && value.ends_with(value.as_bytes()[0] as char)
    {
        value = &value[1..value.len() - 1];
    }
    Some((key, value))
}

/// Load `./.env` into the process env (real env wins). Lets `cargo run`
/// pick up keys without a `set -a` wrapper; missing file is fine.
fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        if let Some((key, value)) = parse_dotenv_line(line) {
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Address for the control API. Railway (and similar hosts) assigns the
/// port at runtime via `$PORT`: when set, listen on all interfaces there.
/// Otherwise use the configured address (local runs are unchanged).
/// Feed subscriptions: configured symbols plus the ETH/BTC cross while
/// the triangular strategy needs it. The view already expects the cross;
/// without the subscription its books never arrive and the strategy
/// fires nothing. Strategy symbol lists are untouched.
fn feed_symbols(symbols: &[Symbol], triangular: bool) -> Vec<Symbol> {
    let mut out = symbols.to_vec();
    if triangular {
        let cross = Symbol::new("ETH", "BTC");
        if !out.contains(&cross) {
            out.push(cross);
        }
    }
    out
}

fn resolve_api_addr(configured: Option<String>) -> Option<String> {
    std::env::var("PORT")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .map(|p| format!("0.0.0.0:{p}"))
        .or(configured)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    load_dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    let api_addr = resolve_api_addr(cfg.engine.api_addr.clone());
    let tick_ms = cfg.engine.tick_ms;
    let mut engine = engine::Engine::new(cfg)?;
    engine.record_session_start();

    tracing::info!(session = %engine.session_id, mode = %engine.status().mode, venues = ?engine.cfg.venues,
        symbols = ?engine.cfg.symbols, strategies = engine.strategies.len(), "arb-engine starting");
    // Live venue books in, paper execution unchanged: fills stay simulated
    // and the live-send gate is untouched. Stale venues are skipped.
    // Indodax additionally needs its published static market-data token
    // from the environment; it is not a trading credential.
    let indodax_token = std::env::var("INDODAX_WS_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    arb_feed::spawn_venue_tasks(
        &engine.shared.live_books,
        &engine.shared.live_connectors,
        engine.cfg.venues.clone(),
        feed_symbols(&engine.symbols, engine.cfg.strategies.triangular),
        indodax_token,
    );
    if engine.cfg.venues.contains(&Venue::Indodax) {
        // Independent USDT/IDR cross-check for the live divisor.
        arb_feed::spawn_upbit_usdt_idr_task(&engine.shared.fx_cross);
    }
    // Real DEX quotes in (Jupiter HTTP + Uniswap V2 + PancakeSwap V2),
    // paper execution unchanged. Each source is fail-closed: unset means
    // no quotes, never invented ones.
    let dex_jupiter = engine.cfg.dex.jupiter_enabled.then(|| {
        let symbols = engine
            .symbols
            .iter()
            .filter(|s| arb_dex_sol::mint_for(&s.base).is_some())
            .cloned()
            .collect::<Vec<_>>();
        arb_feed::JupiterCfg {
            symbols,
            target_notional_quote: engine.cfg.params.target_notional_quote,
            gas_quote: engine.cfg.dex.jupiter_gas_quote,
            poll_secs: engine.cfg.dex.jupiter_poll_secs,
        }
    });
    let mut dex_evm = Vec::new();
    if !engine.cfg.dex.evm_rpc_url.trim().is_empty() && !engine.cfg.dex.evm_pool.trim().is_empty() {
        dex_evm.push(arb_feed::EvmCfg {
            kind: arb_feed::DexKind::UniswapV2,
            symbol: Symbol::new("ETH", "USDT"),
            rpc_url: engine.cfg.dex.evm_rpc_url.clone(),
            pool: engine.cfg.dex.evm_pool.clone(),
            token0_is_base: engine.cfg.dex.evm_token0_is_base,
            base_decimals: engine.cfg.dex.evm_base_decimals,
            quote_decimals: engine.cfg.dex.evm_quote_decimals,
            gas_quote: engine.cfg.dex.evm_gas_quote,
            target_notional_quote: engine.cfg.params.target_notional_quote,
            poll_secs: engine.cfg.dex.evm_poll_secs,
        });
    }
    if !engine.cfg.dex.bsc_rpc_url.trim().is_empty() && !engine.cfg.dex.bsc_pool.trim().is_empty() {
        dex_evm.push(arb_feed::EvmCfg {
            kind: arb_feed::DexKind::PancakeV2,
            symbol: Symbol::new("BNB", "USDT"),
            rpc_url: engine.cfg.dex.bsc_rpc_url.clone(),
            pool: engine.cfg.dex.bsc_pool.clone(),
            token0_is_base: engine.cfg.dex.bsc_token0_is_base,
            base_decimals: engine.cfg.dex.bsc_base_decimals,
            quote_decimals: engine.cfg.dex.bsc_quote_decimals,
            // BSC gas is cheaper; the shared EVM figure overestimates cost,
            // which only suppresses marginal edges (fail-safe direction).
            gas_quote: engine.cfg.dex.evm_gas_quote,
            target_notional_quote: engine.cfg.params.target_notional_quote,
            poll_secs: engine.cfg.dex.evm_poll_secs,
        });
    }
    arb_feed::spawn_dex_tasks(&engine.shared.live_dex, dex_jupiter, dex_evm);
    // Perp funding prints in (Binance/Bybit/OKX). Indodax has no perps.
    // A stalled stream degrades to no entries, never to stale ones.
    let perp_venues: Vec<Venue> = engine
        .cfg
        .venues
        .iter()
        .copied()
        .filter(|v| matches!(v, Venue::Binance | Venue::Bybit | Venue::Okx))
        .collect();
    arb_feed::spawn_funding_tasks(
        &engine.shared.live_funding,
        perp_venues,
        engine.symbols.clone(),
    );
    tracing::info!("live WS feeds starting (paper execution)");

    if let Some(addr) = api_addr {
        // Secret from env only, like venue keys — never from the config file.
        let api_token = std::env::var("API_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        tokio::spawn(serve_api(addr, api_token, engine.shared.clone()));
    }

    let mut count: u64 = 0;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Ctrl-C — shutting down");
                break;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(tick_ms)) => {
                engine.tick();
                count += 1;
                if args.ticks > 0 && count >= args.ticks { break; }
            }
        }
    }
    let st = engine.status();
    tracing::info!(ticks = count, trades = st.trades, opportunities = st.opportunities_seen,
        pnl = %st.realized_pnl_quote, stored_trades = engine.store.trade_count(), "arb-engine stopped");
    println!(
        "summary: ticks={} opportunities={} paper_trades={} realized_pnl={}",
        count, st.opportunities_seen, st.trades, st.realized_pnl_quote
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{BookLevel, Intent, MarketKind, OrderBook, Side, TradeLeg};

    #[test]
    fn feed_symbols_add_cross_for_triangular_only() {
        // The triangular strategy needs the ETH/BTC cross, but no venue
        // would ever subscribe to it, so it fired zero intents in 180k scans.
        let syms = vec![Symbol::new("BTC", "USDT")];
        let out = feed_symbols(&syms, true);
        assert!(out.contains(&Symbol::new("ETH", "BTC")));
        assert_eq!(out.len(), 2);
        let out = feed_symbols(&syms, false);
        assert_eq!(out, syms);
        // Already configured: no duplicate subscription.
        let both = vec![Symbol::new("BTC", "USDT"), Symbol::new("ETH", "BTC")];
        assert_eq!(feed_symbols(&both, true).len(), 2);
    }

    #[test]
    fn dotenv_line_parses_env_format() {
        assert_eq!(parse_dotenv_line("FOO=bar"), Some(("FOO", "bar")));
        assert_eq!(parse_dotenv_line("  FOO =  bar  "), Some(("FOO", "bar")));
        assert_eq!(parse_dotenv_line("FOO=\"a=b\""), Some(("FOO", "a=b")));
        assert_eq!(parse_dotenv_line("FOO='sp ace'"), Some(("FOO", "sp ace")));
        assert_eq!(parse_dotenv_line("export FOO=bar"), Some(("FOO", "bar")));
        assert_eq!(parse_dotenv_line("# comment"), None);
        assert_eq!(parse_dotenv_line(""), None);
        assert_eq!(parse_dotenv_line("NOEQUALS"), None);
        assert_eq!(parse_dotenv_line("=v"), None);
        assert_eq!(parse_dotenv_line("NOT A KEY=v"), None);
    }

    #[test]
    fn port_env_overrides_configured_addr() {
        let prev = std::env::var("PORT").ok();
        std::env::set_var("PORT", "4321");
        assert_eq!(
            resolve_api_addr(Some("127.0.0.1:8787".into())),
            Some("0.0.0.0:4321".into())
        );
        std::env::remove_var("PORT");
        assert_eq!(
            resolve_api_addr(Some("127.0.0.1:8787".into())),
            Some("127.0.0.1:8787".into())
        );
        assert_eq!(resolve_api_addr(None), None);
        if let Some(v) = prev {
            std::env::set_var("PORT", v);
        }
    }

    #[test]
    fn eth_seeding_uses_usdt_anchor_not_triangle_cross() {
        // Regression: the prefix lookup matched "ETH/BTC" (0.03) before
        // "ETH/USDT" and seeded 66666.67 ETH per venue, inflating the
        // portfolio baseline ~17x.
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance"]
symbols = ["BTC/USDT", "ETH/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let engine = engine::Engine::new(cfg).unwrap();
        // Budget rule: one fill (1x target) at the USDT anchor, no fixed
        // floor. BTC: 1000/80000 = 0.0125 exact. ETH must be 1000/2300, not
        // the 66666.67 the old prefix match produced from the ETH/BTC cross.
        assert_eq!(
            engine.paper.balance(Venue::Binance, "BTC"),
            Decimal::new(125, 4)
        );
        let eth = engine.paper.balance(Venue::Binance, "ETH");
        assert!(
            eth > Decimal::new(43, 2) && eth < Decimal::new(44, 2),
            "ETH seed: {eth}"
        );
    }

    #[test]
    fn seeding_stays_within_starting_capital() {
        // The paper portfolio must be holdable with the configured starting
        // capital: base is seeded only on Binance for the majors basket, one
        // full fill each. Everything else starts at zero, so equity and
        // unrealized stay meaningful instead of ~$372k of pretend inventory.
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance", "bybit"]
symbols = ["BTC/USDT", "ETH/USDT", "DOGE/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000

[paper]
start_quote_per_venue = 200

[paper.capital]
binance = 10000
"#,
        )
        .unwrap();
        let engine = engine::Engine::new(cfg).unwrap();
        // Quote side honors the capital map (USDT and USDC each).
        assert_eq!(
            engine.paper.balance(Venue::Binance, "USDT"),
            Decimal::new(10_000, 0)
        );
        assert_eq!(
            engine.paper.balance(Venue::Bybit, "USDT"),
            Decimal::new(200, 0)
        );
        // Basket base on Binance only, one fill each at the USDT anchor.
        assert_eq!(
            engine.paper.balance(Venue::Binance, "BTC"),
            Decimal::new(125, 4) // 1000/80000
        );
        let eth = engine.paper.balance(Venue::Binance, "ETH");
        assert!(
            eth > Decimal::new(43, 2) && eth < Decimal::new(44, 2),
            "ETH seed: {eth}"
        );
        // Non-basket base is zero everywhere; basket base is zero off Binance.
        assert_eq!(engine.paper.balance(Venue::Binance, "DOGE"), Decimal::ZERO);
        assert_eq!(engine.paper.balance(Venue::Bybit, "BTC"), Decimal::ZERO);
        assert_eq!(engine.paper.balance(Venue::Bybit, "DOGE"), Decimal::ZERO);
        // Whole portfolio fits the starting capital: quote on both venues
        // plus three basket fills, with headroom for anchor rounding.
        assert!(
            engine.paper.starting_equity < Decimal::new(25_000, 0),
            "baseline: {}",
            engine.paper.starting_equity
        );
    }

    #[test]
    fn no_live_books_means_no_quotes_or_funding() {
        // Regression: the paper soak accumulated 90 synthetic funding
        // entries against live books. With no live books connected, the
        // view must carry no funding and no DEX quotes — the engine
        // cannot invent entries, so those strategies stay quiet.
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let mut engine = engine::Engine::new(cfg).unwrap();
        let view = engine.build_view();
        assert!(view.funding.is_empty());
        assert!(view.dex_quotes.is_empty());
        assert!(view.books.is_empty());
    }

    fn stored(strategy: &str, roundtrip: bool, venue: Venue, base: &str) -> StoredTrade {
        StoredTrade {
            intent: Intent {
                strategy: strategy.into(),
                expected_edge_bps: Decimal::new(20, 0),
                ttl_ms: 100,
                created_ms: 0,
                roundtrip,
                legs: vec![TradeLeg {
                    venue,
                    symbol: Symbol::new(base, "USDT"),
                    side: Side::Buy,
                    price: Decimal::new(100, 0),
                    qty_base: Decimal::new(1, 0),
                    market: MarketKind::Spot,
                }],
            },
            fills: vec![],
            realized_pnl_quote: None,
            ts_ms: 1,
            session_id: "old".into(),
            engine_version: "0.1.0".into(),
        }
    }

    #[test]
    fn restored_funding_pairs_dedupes_and_skips_roundtrips() {
        let trades = vec![
            stored("funding_rate", false, Venue::Bybit, "BTC"),
            stored("funding_rate", false, Venue::Bybit, "BTC"), // duplicate restart entry
            stored("funding_rate", false, Venue::Binance, "ETH"),
            stored("cross_exchange", true, Venue::Bybit, "BTC"),
            stored("funding_rate", true, Venue::Okx, "SOL"), // not a position
        ];
        assert_eq!(
            restored_funding_pairs(&trades),
            vec![
                (Venue::Bybit, Symbol::new("BTC", "USDT")),
                (Venue::Binance, Symbol::new("ETH", "USDT")),
            ]
        );
    }

    #[test]
    fn status_enriches_open_positions_with_entry_and_live_mark() {
        // A restored funding entry must surface qty + entry $ from history
        // and a live mark + market value from the anchor marks (no books).
        let path = std::env::temp_dir().join(format!("dash-open-pos-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let trade = StoredTrade {
            intent: Intent {
                strategy: "funding_rate".into(),
                expected_edge_bps: Decimal::new(20, 0),
                ttl_ms: 100,
                created_ms: 0,
                roundtrip: false,
                legs: vec![TradeLeg {
                    venue: Venue::Binance,
                    symbol: Symbol::new("BTC", "USDT"),
                    side: Side::Buy,
                    price: Decimal::new(80_000, 0),
                    qty_base: Decimal::new(5, 1),
                    market: MarketKind::Spot,
                }],
            },
            fills: vec![],
            realized_pnl_quote: None,
            ts_ms: 1,
            session_id: "old".into(),
            engine_version: "0.1.0".into(),
        };
        MemoryStore::with_jsonl(&path)
            .unwrap()
            .record_trade(trade)
            .unwrap();
        let cfg: config::Config = toml::from_str(&format!(
            r#"
venues = ["binance"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"
jsonl_path = "{}"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#,
            path.display()
        ))
        .unwrap();
        let engine = engine::Engine::new(cfg).unwrap();
        let st = engine.status();
        assert_eq!(st.open_positions.len(), 1);
        let o = &st.open_positions[0];
        assert_eq!(o.qty_base, Decimal::new(5, 1));
        assert_eq!(o.entry_price_quote, Some(Decimal::new(80_000, 0)));
        // Anchor mark (no live books): BTC/USDT 80000, value 0.5 * 80000.
        assert_eq!(o.mark_price_quote, Some(Decimal::new(80_000, 0)));
        assert_eq!(o.market_value_quote, Some(Decimal::new(40_000, 0)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn transfer_friction_debits_strategy_and_engine_realized() {
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let mut engine = engine::Engine::new(cfg).unwrap();
        engine.debit_transfer_fees("cross_exchange", Decimal::new(250, 2));
        assert_eq!(engine.paper.realized_pnl, Decimal::new(-250, 2));
        assert_eq!(
            engine.per_strategy["cross_exchange"].realized_pnl_quote,
            Decimal::new(-250, 2)
        );
        // Zero is a no-op (and creates no per-strategy entry).
        engine.debit_transfer_fees("triangular", Decimal::ZERO);
        assert!(!engine.per_strategy.contains_key("triangular"));
        assert_eq!(engine.paper.realized_pnl, Decimal::new(-250, 2));
    }

    fn buy_intent(notional: Decimal) -> Intent {
        let s = Symbol::new("BTC", "USDT");
        Intent {
            strategy: "t".into(),
            expected_edge_bps: Decimal::new(20, 0),
            ttl_ms: 1000,
            created_ms: 0,
            roundtrip: true,
            legs: vec![
                TradeLeg {
                    venue: Venue::Binance,
                    symbol: s.clone(),
                    side: Side::Buy,
                    price: Decimal::new(100, 0),
                    qty_base: notional / Decimal::new(100, 0),
                    market: MarketKind::Spot,
                },
                TradeLeg {
                    venue: Venue::Bybit,
                    symbol: s,
                    side: Side::Sell,
                    price: Decimal::new(102, 0),
                    qty_base: notional / Decimal::new(100, 0),
                    market: MarketKind::Spot,
                },
            ],
        }
    }

    #[test]
    fn venue_quote_share_skips_only_ruinous_prints() {
        let mut paper = PaperExecutor::new(PaperConfig::default());
        paper.seed(Venue::Binance, "USDT", Decimal::new(10_000, 0));
        paper.seed(Venue::Bybit, "USDT", Decimal::new(200, 0));
        let share = Decimal::new(2500, 0); // 25%
                                           // $500 buy on $10k (5%) passes; the Bybit sell spends no quote.
        assert!(!venue_quote_exceeded(
            &buy_intent(Decimal::new(500, 0)),
            &paper,
            share
        ));
        // $3000 on $10k (30%) trips the 25% share.
        assert!(venue_quote_exceeded(
            &buy_intent(Decimal::new(3000, 0)),
            &paper,
            share
        ));
        // Zero share disables the check entirely.
        assert!(!venue_quote_exceeded(
            &buy_intent(Decimal::new(3000, 0)),
            &paper,
            Decimal::ZERO
        ));
    }

    fn book(
        venue: Venue,
        symbol: Symbol,
        bid: Decimal,
        ask: Decimal,
        qty: Decimal,
        ts_ms: i64,
    ) -> OrderBook {
        let mut b = OrderBook::new(venue, symbol);
        // Thin books (≈$40 notional): S2 sizes inside the 25%-of-ledger venue
        // share, so the fill path — not the risk gate — is what this drill
        // exercises.
        b.apply_snapshot(
            vec![BookLevel { price: bid, qty }],
            vec![BookLevel { price: ask, qty }],
            1,
            ts_ms,
        );
        b
    }

    /// Hedge-failure drill (Q7): stale books and a mid-run venue outage must
    /// never partially fill. Every emitted intent ends filled, failed, or
    /// risk-rejected, and every ledger balance stays non-negative.
    #[test]
    fn outage_and_stale_books_never_partially_fill() {
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance", "bybit"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 50
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let mut engine = engine::Engine::new(cfg).unwrap();
        let now = now_ms();
        let btc = Symbol::new("BTC", "USDT");
        // Depth fits the honestly seeded base: 0.0005 BTC ≈ $40, inside the
        // $50 of BTC seeded on Binance (and inside the venue-share guard).
        let qty = Decimal::new(5, 4);
        // Stale rich book: Bybit's cheap ancient ask must not fire.
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Binance, "BTC/USDT".into()),
            book(
                Venue::Binance,
                btc.clone(),
                Decimal::new(80400, 0),
                Decimal::new(80480, 0),
                qty,
                now,
            ),
        );
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Bybit, "BTC/USDT".into()),
            book(
                Venue::Bybit,
                btc.clone(),
                Decimal::new(79850, 0),
                Decimal::new(79950, 0),
                qty,
                0,
            ),
        );
        engine.tick();
        assert_eq!(engine.paper.trades, 0, "stale books must never fire");
        // Both fresh with a real spread: buys Bybit, sells Binance (where the
        // seeded base lives) — fills exactly once.
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Bybit, "BTC/USDT".into()),
            book(
                Venue::Bybit,
                btc.clone(),
                Decimal::new(79850, 0),
                Decimal::new(79950, 0),
                qty,
                now_ms(),
            ),
        );
        engine.tick();
        assert_eq!(engine.paper.trades, 1);
        // Venue outage mid-run: the tick completes, nothing partially fills.
        engine
            .shared
            .live_books
            .lock()
            .unwrap()
            .remove(&(Venue::Bybit, "BTC/USDT".into()));
        engine.tick();
        assert_eq!(engine.paper.trades, 1, "outage must not fill");
        for (venue, assets) in engine.paper.balances_snapshot() {
            for (asset, amt) in assets {
                assert!(
                    amt >= Decimal::ZERO,
                    "negative ledger {venue} {asset}: {amt}"
                );
            }
        }
        // Every emitted intent is accounted for: filled, failed, or rejected.
        let m = &engine.per_strategy["cross_exchange"];
        assert_eq!(
            m.emitted,
            m.filled + m.fill_failed + m.risk_rejected,
            "every intent accounted: {m:?}"
        );
    }

    /// Repeat-fill guard: paper never consumes book depth, so an unchanged
    /// top of book must not refill every tick. Same tops -> skipped; moved
    /// tops with the edge intact -> fills again.
    #[test]
    fn unchanged_tops_do_not_refill() {
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance", "bybit"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 50
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let mut engine = engine::Engine::new(cfg).unwrap();
        let now = now_ms();
        let btc = Symbol::new("BTC", "USDT");
        let qty = Decimal::new(5, 4);
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Binance, "BTC/USDT".into()),
            book(
                Venue::Binance,
                btc.clone(),
                Decimal::new(80400, 0),
                Decimal::new(80480, 0),
                qty,
                now,
            ),
        );
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Bybit, "BTC/USDT".into()),
            book(
                Venue::Bybit,
                btc.clone(),
                Decimal::new(79850, 0),
                Decimal::new(79950, 0),
                qty,
                now,
            ),
        );
        engine.tick();
        assert_eq!(engine.paper.trades, 1);
        // Identical books next tick: the same print must not refill.
        engine.tick();
        assert_eq!(engine.paper.trades, 1, "unchanged tops must not refill");
        // Moved Bybit ask, edge intact: fires again.
        engine.shared.live_books.lock().unwrap().insert(
            (Venue::Bybit, "BTC/USDT".into()),
            book(
                Venue::Bybit,
                btc.clone(),
                Decimal::new(79850, 0),
                Decimal::new(79900, 0),
                qty,
                now_ms(),
            ),
        );
        engine.tick();
        assert_eq!(engine.paper.trades, 2, "moved tops must fill");
    }

    /// Stub strategy that reports one closed funding pair, so the engine
    /// drain path runs without a full tick.
    struct Closer {
        pairs: Vec<(Venue, Symbol)>,
        drained: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Strategy for Closer {
        fn name(&self) -> &'static str {
            "test_closer"
        }
        fn on_tick(&mut self, _view: &MarketView) -> Vec<Intent> {
            vec![]
        }
        fn take_closed(&mut self) -> Vec<(Venue, Symbol)> {
            self.drained
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::mem::take(&mut self.pairs)
        }
    }

    #[test]
    fn closed_funding_pairs_drain() {
        let cfg: config::Config = toml::from_str(
            r#"
venues = ["binance"]
symbols = ["BTC/USDT"]

[engine]
mode = "paper"

[params]
min_edge_bps = 15
slip_buffer_bps = 5
target_notional_quote = 1000
max_book_age_ms = 2000
"#,
        )
        .unwrap();
        let mut engine = engine::Engine::new(cfg).unwrap();
        let drained = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        engine.strategies = vec![Box::new(Closer {
            pairs: vec![(Venue::Binance, Symbol::new("BTC", "USDT"))],
            drained: drained.clone(),
        })];
        // Draining must consume the reported pair so the closed set cannot
        // grow without bound.
        engine.release_closed();
        assert!(
            drained.load(std::sync::atomic::Ordering::SeqCst),
            "release_closed must drain take_closed"
        );
    }
}
