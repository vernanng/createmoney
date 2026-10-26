mod config;

use arb_api::{
    render_flows_envelope, render_metrics, route, route_viewer, ConnectorHealth, Control,
    EngineStatus,
};
use arb_core::{now_ms, Symbol, Venue};
use arb_dex_evm::{to_dex_quote, DexKind, V2Pool};
use arb_exec::{keys_from_env, ExecMode, LiveExecutor};
use arb_feed::SyntheticFeed;
use arb_paper::PaperExecutor;
use arb_risk::RiskEngine;
use arb_store::{MemoryStore, Store, StoredTrade};
use arb_strategy::{
    CexDex, CrossExchange, FundingRate, FundingStrategy, MarketView, Strategy, Triangular,
};
use clap::Parser;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
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
    per_strategy: Mutex<BTreeMap<String, u64>>,
    /// Pre-rendered `/flows` snapshot (last `FLOWS_SNAPSHOT` trades).
    flows: Mutex<String>,
}

/// Trades kept in the `/flows` snapshot served to the 3D viewer.
const FLOWS_SNAPSHOT: usize = 200;

struct Engine {
    cfg: config::Config,
    symbols: Vec<Symbol>,
    strategies: Vec<Box<dyn Strategy>>,
    risk: RiskEngine,
    paper: PaperExecutor,
    store: MemoryStore,
    shared: Arc<Shared>,
    feed: SyntheticFeed,
    opportunities: u64,
    per_strategy: BTreeMap<String, u64>,
    live: LiveExecutor,
}

impl Engine {
    fn new(cfg: config::Config) -> anyhow::Result<Self> {
        let symbols = cfg.symbols()?;
        let params = cfg.params.to_strategy_params();
        let cex: Vec<Venue> = cfg.venues.iter().copied().filter(|v| v.is_cex()).collect();

        let mut strategies: Vec<Box<dyn Strategy>> = Vec::new();
        if cfg.strategies.cross_exchange {
            strategies.push(Box::new(CrossExchange::new(
                params.clone(),
                cex.clone(),
                symbols.clone(),
            )));
        }
        if cfg.strategies.triangular {
            strategies.push(Box::new(Triangular::new(
                params.clone(),
                cex.clone(),
                Triangular::default_cycles(),
            )));
        }
        if cfg.strategies.funding_rate {
            let pairs: Vec<(Venue, Symbol)> = cex
                .iter()
                .filter(|v| matches!(v, Venue::Binance | Venue::Bybit | Venue::Okx))
                .flat_map(|v| symbols.iter().map(move |s| (*v, s.clone())))
                .collect();
            strategies.push(Box::new(FundingStrategy::new(
                params.clone(),
                cfg.params.min_funding_annualized_bps,
                pairs,
            )));
        }
        if cfg.strategies.cex_dex {
            strategies.push(Box::new(CexDex::new(
                params.clone(),
                cex.clone(),
                symbols.clone(),
            )));
        }

        let risk = RiskEngine::new(
            cfg.risk
                .to_limits(cfg.params.min_edge_bps, cfg.params.max_book_age_ms),
        );
        let shared = Arc::new(Shared {
            control: Mutex::new(Control::new(risk.kill_handle())),
            status: Mutex::new(EngineStatus {
                mode: cfg.engine.mode.clone(),
                killed: false,
                trades: 0,
                realized_pnl_quote: Decimal::ZERO,
                opportunities_seen: 0,
                connectors: vec![],
                paused_strategies: vec![],
            }),
            per_strategy: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(render_flows_envelope(&[], 0)),
        });
        let mode = ExecMode::parse(&cfg.engine.mode).unwrap_or(ExecMode::Paper);
        let keys = keys_from_env();
        if mode == ExecMode::Live {
            tracing::warn!(venues_with_keys = keys.len(), "LIVE mode requested; paper executor still handles fills until venue HTTP senders are wired — see arb-exec guard");
        }
        let live = LiveExecutor::new(mode, keys);
        let store = match &cfg.engine.jsonl_path {
            Some(p) => MemoryStore::with_jsonl(p.clone()),
            None => MemoryStore::new(),
        };
        // Seed paper inventory: quote cash + base assets on every venue that
        // can receive a leg (CEXs plus DEX wallets). Sells of unheld assets
        // and unfunded buys abort in the paper executor.
        let mut paper = PaperExecutor::default();
        {
            let mut venues: Vec<Venue> = cfg.venues.clone();
            for dv in [Venue::Uniswap, Venue::Pancakeswap, Venue::Jupiter] {
                if !venues.contains(&dv) {
                    venues.push(dv);
                }
            }
            let mut assets: Vec<String> = vec!["USDT".into(), "USDC".into()];
            for s in &symbols {
                if !assets.contains(&s.base) {
                    assets.push(s.base.clone());
                }
                if !assets.contains(&s.quote) {
                    assets.push(s.quote.clone());
                }
            }
            for extra in ["BTC", "ETH"] {
                if !assets.iter().any(|a| a == extra) {
                    assets.push(extra.into());
                }
            }
            for v in venues {
                for a in &assets {
                    let amt = if a == "USDT" || a == "USDC" {
                        cfg.paper.start_quote_per_venue
                    } else {
                        cfg.paper.start_base_per_asset
                    };
                    paper.seed(v, a, amt);
                }
            }
        }
        Ok(Self {
            cfg,
            symbols,
            strategies,
            risk,
            paper,
            store,
            shared,
            feed: SyntheticFeed::new(0xC0FFEE, Decimal::new(100_000, 0)),
            opportunities: 0,
            per_strategy: BTreeMap::new(),
            live,
        })
    }

    fn build_view(&mut self) -> MarketView {
        let now = now_ms();
        let mut view = MarketView {
            now_ms: now,
            ..Default::default()
        };
        let anchors: BTreeMap<String, Decimal> = BTreeMap::from([
            ("BTC/USDT".to_string(), Decimal::new(100_000, 0)),
            ("ETH/USDT".to_string(), Decimal::new(3_000, 0)),
            ("ETH/BTC".to_string(), Decimal::new(3, 2)), // 0.03 — triangular needs the cross
            ("SOL/USDT".to_string(), Decimal::new(150, 0)),
            ("SOL/USDC".to_string(), Decimal::new(150, 0)),
        ]);
        let mut needed: Vec<Symbol> = self.symbols.clone();
        for cyc_sym in [
            Symbol::new("BTC", "USDT"),
            Symbol::new("ETH", "BTC"),
            Symbol::new("ETH", "USDT"),
        ] {
            if !needed.contains(&cyc_sym) {
                needed.push(cyc_sym);
            }
        }
        for sym in &needed {
            let anchor = anchors
                .get(&sym.to_string())
                .copied()
                .unwrap_or(Decimal::new(100, 0));
            // One shared synthetic stream: books evolve per call, venues differ.
            let venues = self.cfg.venues.clone();
            for v in &venues {
                if !v.is_cex() {
                    continue;
                }
                if *v == Venue::Indodax && sym.quote == "USDT" {
                    // Indodax quotes in IDR: build an IDR book, normalize to USD.
                    let idr_sym = Symbol::new(&sym.base, "IDR");
                    let idr_anchor = anchor * self.cfg.indodax.usd_idr;
                    let idr_book = self.feed.book_at(*v, &idr_sym, idr_anchor, now);
                    view.put(arb_feed::idr_book_to_usd(
                        &idr_book,
                        self.cfg.indodax.usd_idr,
                    ));
                    continue;
                }
                let book = self.feed.book_at(*v, sym, anchor, now);
                view.put(book);
            }
        }
        // DEX quotes from local V2 pool math (paper reserves track the anchor).
        for sym in &self.symbols {
            let anchor = anchors
                .get(&sym.to_string())
                .copied()
                .unwrap_or(Decimal::new(100, 0));
            let qty = self.cfg.params.target_notional_quote / anchor;
            if self.cfg.dex.uniswap {
                let pool = V2Pool {
                    kind: DexKind::UniswapV2,
                    symbol: sym.clone(),
                    reserve_base: Decimal::new(500, 0),
                    reserve_quote: anchor * Decimal::new(500, 0),
                    fee_bps: DexKind::UniswapV2.default_fee_bps(),
                };
                if let Some(q) = to_dex_quote(&pool, qty, self.cfg.dex.evm_gas_quote, now) {
                    view.dex_quotes.push(q);
                }
            }
            if self.cfg.dex.pancakeswap {
                let pool = V2Pool {
                    kind: DexKind::PancakeV2,
                    symbol: sym.clone(),
                    reserve_base: Decimal::new(400, 0),
                    reserve_quote: anchor * Decimal::new(400, 0) * Decimal::new(1001, 3),
                    fee_bps: DexKind::PancakeV2.default_fee_bps(),
                };
                if let Some(q) = to_dex_quote(&pool, qty, self.cfg.dex.evm_gas_quote, now) {
                    view.dex_quotes.push(q);
                }
            }
            if self.cfg.dex.jupiter {
                // Paper Jupiter quote: anchor ± small dislocation. Live mode
                // replaces this with arb_dex_sol::quote_url + HTTP GET.
                if arb_dex_sol::quote_url(sym, qty, true, 50).is_some() {
                    view.dex_quotes.push(arb_strategy::DexQuote {
                        venue: Venue::Jupiter,
                        symbol: sym.clone(),
                        buy_px: anchor * Decimal::new(10015, 4),
                        sell_px: anchor * Decimal::new(9985, 4),
                        gas_quote: self.cfg.dex.solana_fee_quote,
                        ts_ms: now,
                    });
                }
            }
        }
        // Synthetic funding snapshot (live mode replaces with perp feed values).
        for v in [Venue::Binance, Venue::Bybit, Venue::Okx] {
            for sym in &self.symbols {
                view.funding.push(FundingRate {
                    venue: v,
                    symbol: sym.clone(),
                    rate_per_interval: Decimal::new(1, 4),
                    interval_hours: Decimal::new(8, 0),
                    next_funding_ms: now + 3_600_000,
                });
            }
        }
        view
    }

    fn tick(&mut self) {
        let view = self.build_view();
        let now = view.now_ms;
        let paused: Vec<String> = self
            .shared
            .control
            .lock()
            .map(|c| c.paused())
            .unwrap_or_default();
        let mut intents = Vec::new();
        for st in self.strategies.iter_mut() {
            if paused.iter().any(|p| p == st.name()) {
                continue;
            }
            intents.extend(st.on_tick(&view));
        }
        for intent in intents {
            self.opportunities += 1;
            if let Err(e) = self.risk.check(&intent, now) {
                tracing::debug!(strategy = %intent.strategy, reject = %e, "intent rejected");
                continue;
            }
            // Live gate: even in live mode, sends must pass the armed check.
            if self.live.mode == ExecMode::Live {
                if let Err(e) = self.live.guard(&intent) {
                    tracing::warn!(%e, "live send blocked; falling back to paper fill for accounting");
                }
            }
            match self.paper.execute(&intent, &view.books, now) {
                Ok(result) => {
                    // Settle exposure or per-venue caps can never bind.
                    self.risk.settle(&intent);
                    *self
                        .per_strategy
                        .entry(intent.strategy.clone())
                        .or_default() += 1;
                    tracing::info!(strategy = %intent.strategy, edge_bps = %intent.expected_edge_bps,
                        pnl = ?result.realized_pnl_quote, legs = intent.legs.len(), "paper trade");
                    let stored = StoredTrade {
                        intent,
                        fills: result.fills,
                        realized_pnl_quote: result.realized_pnl_quote,
                        ts_ms: now,
                    };
                    if let Err(e) = self.store.record_trade(stored) {
                        tracing::error!(%e, "store write failed");
                    }
                }
                Err(e) => tracing::debug!(%e, "paper fill failed"),
            }
        }
        self.publish();
    }

    /// Push a fresh snapshot to the API task.
    fn publish(&self) {
        if let Ok(mut s) = self.shared.status.lock() {
            *s = self.status();
        }
        if let Ok(mut p) = self.shared.per_strategy.lock() {
            *p = self.per_strategy.clone();
        }
        if let Ok(mut f) = self.shared.flows.lock() {
            let trades = self.store.trades();
            let from = trades.len().saturating_sub(FLOWS_SNAPSHOT);
            let items: Vec<serde_json::Value> = trades[from..]
                .iter()
                .filter_map(|t| serde_json::to_value(t).ok())
                .collect();
            *f = render_flows_envelope(&items, trades.len());
        }
    }

    fn status(&self) -> EngineStatus {
        let (killed, paused_strategies) = self
            .shared
            .control
            .lock()
            .map(|c| (c.is_killed(), c.paused()))
            .unwrap_or_default();
        EngineStatus {
            mode: self.cfg.engine.mode.clone(),
            killed,
            trades: self.paper.trades,
            realized_pnl_quote: self.paper.realized_pnl,
            opportunities_seen: self.opportunities,
            connectors: self
                .cfg
                .venues
                .iter()
                .map(|v| ConnectorHealth {
                    venue: *v,
                    connected: true,
                    last_msg_ms: now_ms(),
                    books: self.symbols.len(),
                })
                .collect(),
            paused_strategies,
        }
    }
}

async fn serve_api(addr: String, shared: Arc<Shared>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%e, addr, "api bind failed");
            return;
        }
    };
    tracing::info!(addr, "control API listening (GET /health /status /metrics /flows /viewer, POST /kill /revive /pause/<s> /resume/<s>)");
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            let req = String::from_utf8_lossy(&buf[..n]);
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
                        trades: 0,
                        realized_pnl_quote: Decimal::ZERO,
                        opportunities_seen: 0,
                        connectors: vec![],
                        paused_strategies: vec![],
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
                if let Some(hit) = route_viewer(method, path, &flows) {
                    hit
                } else {
                    match shared.control.lock() {
                        Ok(mut ctl) => {
                            let (code, body) = route(method, path, &mut ctl, &status, &metrics);
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    let api_addr = cfg.engine.api_addr.clone();
    let tick_ms = cfg.engine.tick_ms;
    let feed_kind = cfg.engine.feed.clone();
    let mut engine = Engine::new(cfg)?;

    tracing::info!(mode = %engine.status().mode, feed = %feed_kind, venues = ?engine.cfg.venues,
        symbols = ?engine.cfg.symbols, strategies = engine.strategies.len(), "arb-engine starting");
    if feed_kind == "live" {
        tracing::warn!("feed=live requested: wire WS tasks into arb-feed parsers (ws_url/subscribe_frame are ready); running synthetic books until then");
    }

    if let Some(addr) = api_addr {
        tokio::spawn(serve_api(addr, engine.shared.clone()));
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
        pnl = %st.realized_pnl_quote, stored_trades = engine.store.trades().len(), "arb-engine stopped");
    println!(
        "summary: ticks={} opportunities={} paper_trades={} realized_pnl={}",
        count, st.opportunities_seen, st.trades, st.realized_pnl_quote
    );
    Ok(())
}
