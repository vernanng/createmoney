mod config;

use arb_api::{
    render_flows_envelope, render_metrics, route, route_viewer, ConnectorHealth, Control,
    EngineStatus, StrategyMetrics, UsdtIdrHealth,
};
use arb_core::{now_ms, Symbol, Venue};
use arb_dex_evm::{to_dex_quote, DexKind, V2Pool};
use arb_exec::{keys_from_env, ExecMode, LiveExecutor};
use arb_feed::{
    implied_usdt_idr, pick_fresh, resolve_usdt_idr, FxCrossCheck, LiveBooks, LiveConnectors,
    RateDecision, SyntheticFeed, UsdtIdrRate,
};
use arb_paper::PaperExecutor;
use arb_risk::RiskEngine;
use arb_store::{MemoryStore, SessionStart, Store, StoredTrade};
use arb_strategy::{
    CexDex, CrossExchange, FundingRate, FundingStrategy, FxTriangle, LagChase, MarketView,
    Strategy, Triangular,
};
use clap::Parser;
use rust_decimal::Decimal;
use std::collections::BTreeMap;
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
    /// Live venue books (empty unless `feed = "live"` tasks are spawned).
    live_books: LiveBooks,
    /// Socket-level state written by the live venue tasks.
    live_connectors: LiveConnectors,
    /// Latest independent USDT/IDR sample (Upbit Indonesia cross-check).
    fx_cross: FxCrossCheck,
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

/// Funding snapshot used only by the synthetic feed. Live mode must never
/// invent funding: until real perp feeds land, `funding_rate` simply has
/// no data there and cannot fire.
fn synthetic_funding(feed: &str, symbols: &[Symbol], now: i64) -> Vec<FundingRate> {
    if feed == "live" {
        return Vec::new();
    }
    let mut out = Vec::new();
    for v in [Venue::Binance, Venue::Bybit, Venue::Okx] {
        for sym in symbols {
            out.push(FundingRate {
                venue: v,
                symbol: sym.clone(),
                rate_per_interval: Decimal::new(1, 4),
                interval_hours: Decimal::new(8, 0),
                next_funding_ms: now + 3_600_000,
            });
        }
    }
    out
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

/// Reference price levels for the synthetic feed and paper seeding.
/// Real feeds replace these with live books; keep them plausible.
fn anchors() -> BTreeMap<String, Decimal> {
    BTreeMap::from([
        ("BTC/USDT".to_string(), Decimal::new(100_000, 0)),
        ("ETH/USDT".to_string(), Decimal::new(3_000, 0)),
        ("ETH/BTC".to_string(), Decimal::new(3, 2)), // 0.03 — triangular needs the cross
        ("SOL/USDT".to_string(), Decimal::new(150, 0)),
        ("SOL/USDC".to_string(), Decimal::new(150, 0)),
        ("XRP/USDT".to_string(), Decimal::new(6, 1)),
        ("DOGE/USDT".to_string(), Decimal::new(2, 1)),
    ])
}

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
    per_strategy: BTreeMap<String, StrategyMetrics>,
    live: LiveExecutor,
    session_id: String,
    /// Last tick's USDT/IDR rate decision, surfaced via status/metrics.
    last_fx: RateDecision,
}

impl Engine {
    fn new(cfg: config::Config) -> anyhow::Result<Self> {
        let symbols = cfg.symbols()?;
        let params = cfg.params.to_strategy_params();
        let cex: Vec<Venue> = cfg.venues.iter().copied().filter(|v| v.is_cex()).collect();

        // Open the store first: reloaded history seeds funding pairs that
        // are still open, so a restart cannot reopen them.
        let store = match &cfg.engine.jsonl_path {
            Some(p) => MemoryStore::with_jsonl(p.clone())?,
            None => MemoryStore::new(),
        };
        let restored_pairs = restored_funding_pairs(store.trades());

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
            strategies.push(Box::new(
                FundingStrategy::new(params.clone(), cfg.params.min_funding_annualized_bps, pairs)
                    .with_open_pairs(restored_pairs),
            ));
        }
        if cfg.strategies.cex_dex {
            strategies.push(Box::new(CexDex::new(
                params.clone(),
                cex.clone(),
                symbols.clone(),
            )));
        }
        // S1 + S3: lead-lag and FX-triangle ride on the cross-exchange flag.
        // Conservative jump threshold so synthetic paper stays quiet until a
        // real dislocation prints; only wired when both venues exist.
        if cfg.strategies.cross_exchange
            && cex.contains(&Venue::Binance)
            && cex.contains(&Venue::Indodax)
        {
            strategies.push(Box::new(LagChase::new(
                params.clone(),
                Venue::Binance,
                Venue::Indodax,
                symbols.clone(),
                Decimal::new(30, 0),
            )));
            strategies.push(Box::new(FxTriangle::new(
                params.clone(),
                Venue::Binance,
                Venue::Indodax,
                symbols.clone(),
                cfg.indodax.usd_idr,
            )));
        }

        let mut risk = RiskEngine::new(
            cfg.risk
                .to_limits(cfg.params.min_edge_bps, cfg.params.max_book_age_ms),
        );
        // Restored open (non-roundtrip) positions still consume venue caps.
        for t in store.trades() {
            if !t.intent.roundtrip {
                risk.settle(&t.intent);
            }
        }
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
                usdt_idr: UsdtIdrHealth::default(),
            }),
            per_strategy: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(render_flows_envelope(&[], 0)),
            live_books: LiveBooks::default(),
            live_connectors: LiveConnectors::default(),
            fx_cross: FxCrossCheck::default(),
        });
        let mode = ExecMode::parse(&cfg.engine.mode).unwrap_or(ExecMode::Paper);
        let keys = keys_from_env();
        if mode == ExecMode::Live {
            tracing::warn!(venues_with_keys = keys.len(), "LIVE mode requested; paper executor still handles fills until venue HTTP senders are wired — see arb-exec guard");
        }
        let live = LiveExecutor::new(mode, keys);
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
            let anchor_tbl = anchors();
            for v in venues {
                for a in &assets {
                    let amt = if a == "USDT" || a == "USDC" {
                        cfg.paper.start_quote_per_venue
                    } else {
                        // Seed enough base to sell ~2x target notional at the
                        // reference price, so cheap assets can actually trade.
                        let ref_px = anchor_tbl
                            .iter()
                            .find(|(k, _)| k.split('/').next() == Some(a.as_str()))
                            .map(|(_, v)| *v)
                            .unwrap_or(Decimal::new(100, 0));
                        let need = Decimal::new(2, 0) * cfg.params.target_notional_quote / ref_px;
                        cfg.paper.start_base_per_asset.max(need)
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
            session_id: new_session_id(),
            last_fx: RateDecision::default(),
        })
    }

    /// Stamp the JSONL history with this run's boundary record.
    fn record_session_start(&mut self) {
        let rec = SessionStart::new(
            self.session_id.clone(),
            ENGINE_VERSION.into(),
            self.cfg.engine.mode.clone(),
            self.cfg.engine.feed.clone(),
            now_ms(),
            self.store.trades().len(),
        );
        if let Err(e) = self.store.record_session_start(rec) {
            tracing::error!(%e, "session start write failed");
        }
    }

    /// Select this tick's USDT/IDR divisor from one live snapshot:
    /// Indodax's own USDT/IDR book as primary, cross-checked against the
    /// Upbit Indonesia sample and the crosses implied by fresh Binance
    /// `X/USDT` vs Indodax `X/IDR` books. No static fallback, ever.
    fn fx_decision(
        &self,
        live_map: &std::collections::BTreeMap<(Venue, String), arb_core::OrderBook>,
        needed: &[Symbol],
        now: i64,
    ) -> RateDecision {
        let cfg = &self.cfg.indodax;
        let mut independent: Vec<Decimal> = Vec::new();
        if let Some(sample) = self.shared.fx_cross.lock().map(|c| *c).unwrap_or_default() {
            if let Some(r) = UsdtIdrRate::from_bid_ask(
                sample.bid,
                sample.ask,
                sample.ts_ms,
                now,
                cfg.rate_max_age_ms,
                cfg.rate_max_spread_bps,
            ) {
                independent.push(r.mid);
            }
        }
        for sym in needed {
            if sym.quote != "USDT" || sym.base == "USDT" {
                continue;
            }
            let (Some(usdt_book), Some(idr_book)) = (
                pick_fresh(
                    live_map,
                    Venue::Binance,
                    &sym.to_string(),
                    now,
                    self.cfg.params.max_book_age_ms,
                ),
                pick_fresh(
                    live_map,
                    Venue::Indodax,
                    &format!("{}/IDR", sym.base),
                    now,
                    self.cfg.params.max_book_age_ms,
                ),
            ) else {
                continue;
            };
            if let Some(mid) = implied_usdt_idr(&idr_book, &usdt_book) {
                independent.push(mid);
            }
        }
        resolve_usdt_idr(
            live_map.get(&(Venue::Indodax, "USDT/IDR".to_string())),
            &independent,
            now,
            cfg.rate_max_age_ms,
            cfg.rate_max_spread_bps,
            cfg.rate_max_deviation_bps,
        )
    }

    fn build_view(&mut self) -> MarketView {
        let now = now_ms();
        let mut view = MarketView {
            now_ms: now,
            ..Default::default()
        };
        let anchors = anchors();
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
        // Live books, when the WS tasks run: fresh ones win, stale venues
        // are skipped (never backfilled) so paper only sees real spreads.
        let live_mode = self.cfg.engine.feed == "live";
        let live_map = self
            .shared
            .live_books
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // One snapshot, one divisor: every IDR book this tick converts at
        // the same selected rate (or not at all, when the decision has none).
        let fx = if live_mode {
            self.fx_decision(&live_map, &needed, now)
        } else {
            RateDecision::default()
        };
        if live_mode {
            self.last_fx = fx;
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
                if live_mode {
                    if *v == Venue::Indodax {
                        // Raw IDR books convert only under an accepted live
                        // rate. Without one the venue is invisible this tick:
                        // no static-rate fallback, no phantom edge.
                        if sym.quote == "USDT" {
                            if let (Some(rate), Some(raw)) = (
                                fx.rate,
                                pick_fresh(
                                    &live_map,
                                    *v,
                                    &format!("{}/IDR", sym.base),
                                    now,
                                    self.cfg.params.max_book_age_ms,
                                ),
                            ) {
                                view.put(arb_feed::idr_book_to_usd(&raw, rate.mid));
                            }
                        }
                        continue;
                    }
                    if let Some(b) = pick_fresh(
                        &live_map,
                        *v,
                        &sym.to_string(),
                        now,
                        self.cfg.params.max_book_age_ms,
                    ) {
                        view.put(b);
                    }
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
        // Synthetic funding exists only for the synthetic feed. In live
        // mode there is deliberately no funding data until real perp
        // feeds land, so the soak cannot accumulate invented entries.
        view.funding
            .extend(synthetic_funding(&self.cfg.engine.feed, &self.symbols, now));
        // Publish the divisor for provenance (FxTriangle records it).
        if live_mode {
            view.usdt_idr_mid = fx.rate.map(|r| r.mid);
            view.usdt_idr_ts_ms = fx.rate.map(|r| r.ts_ms);
        } else {
            view.usdt_idr_mid = Some(self.cfg.indodax.usd_idr);
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
            self.per_strategy
                .entry(st.name().to_string())
                .or_default()
                .scans += 1;
            intents.extend(st.on_tick(&view));
        }
        for intent in intents {
            self.opportunities += 1;
            self.per_strategy
                .entry(intent.strategy.clone())
                .or_default()
                .emitted += 1;
            if let Err(e) = self.risk.check(&intent, now) {
                self.per_strategy
                    .entry(intent.strategy.clone())
                    .or_default()
                    .risk_rejected += 1;
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
                    {
                        let m = self
                            .per_strategy
                            .entry(intent.strategy.clone())
                            .or_default();
                        m.filled += 1;
                        if let Some(p) = result.realized_pnl_quote {
                            m.realized_pnl_quote += p;
                        }
                    }
                    tracing::info!(strategy = %intent.strategy, edge_bps = %intent.expected_edge_bps,
                        pnl = ?result.realized_pnl_quote, legs = intent.legs.len(), "paper trade");
                    let stored = StoredTrade {
                        intent,
                        fills: result.fills,
                        realized_pnl_quote: result.realized_pnl_quote,
                        ts_ms: now,
                        session_id: self.session_id.clone(),
                        engine_version: ENGINE_VERSION.into(),
                    };
                    if let Err(e) = self.store.record_trade(stored) {
                        tracing::error!(%e, "store write failed");
                    }
                }
                Err(e) => {
                    self.per_strategy
                        .entry(intent.strategy.clone())
                        .or_default()
                        .fill_failed += 1;
                    tracing::debug!(%e, "paper fill failed");
                }
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
        let live_mode = self.cfg.engine.feed == "live";
        let now = now_ms();
        let live_map = self
            .shared
            .live_books
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let conn_map = self
            .shared
            .live_connectors
            .lock()
            .map(|m| m.clone())
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
                .map(|v| {
                    // Socket state and book freshness are separate signals:
                    // `connected` says the WS task holds a socket, while
                    // `fresh_books` says its data is currently usable.
                    let venue_books: Vec<&arb_core::OrderBook> = live_map
                        .iter()
                        .filter(|((w, _), _)| w == v)
                        .map(|(_, b)| b)
                        .collect();
                    let conn = conn_map.get(v);
                    if live_mode {
                        ConnectorHealth {
                            venue: *v,
                            connected: conn.map(|c| c.connected).unwrap_or(false),
                            last_msg_ms: conn.and_then(|c| c.last_msg_ms),
                            last_book_ms: venue_books.iter().map(|b| b.ts_ms).max(),
                            books: venue_books.len(),
                            fresh_books: venue_books
                                .iter()
                                .filter(|b| {
                                    now - b.ts_ms <= self.cfg.params.max_book_age_ms
                                        && !b.is_crossed()
                                })
                                .count(),
                            last_error: conn.and_then(|c| c.last_error.clone()),
                        }
                    } else {
                        ConnectorHealth {
                            venue: *v,
                            connected: true,
                            last_msg_ms: Some(now),
                            last_book_ms: Some(now),
                            books: self.symbols.len(),
                            fresh_books: self.symbols.len(),
                            last_error: None,
                        }
                    }
                })
                .collect(),
            paused_strategies,
            usdt_idr: if live_mode {
                let d = &self.last_fx;
                UsdtIdrHealth {
                    mid: d.rate.map(|r| r.mid),
                    spread_bps: d.rate.map(|r| r.spread_bps),
                    book_ts_ms: d.rate.map(|r| r.ts_ms),
                    age_ms: d.rate.map(|r| now - r.ts_ms),
                    fresh: d.rate.is_some(),
                    degraded: d.degraded(),
                    consensus_mid: d.consensus_mid,
                    deviation_bps: d.deviation_bps,
                    independent_sources: d.independent_sources,
                    state: d.state.as_str().into(),
                }
            } else {
                UsdtIdrHealth {
                    mid: Some(self.cfg.indodax.usd_idr),
                    fresh: true,
                    degraded: false,
                    state: "synthetic".into(),
                    ..UsdtIdrHealth::default()
                }
            },
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
                        usdt_idr: UsdtIdrHealth::default(),
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
    engine.record_session_start();

    tracing::info!(session = %engine.session_id, mode = %engine.status().mode, feed = %feed_kind, venues = ?engine.cfg.venues,
        symbols = ?engine.cfg.symbols, strategies = engine.strategies.len(), "arb-engine starting");
    if feed_kind == "live" {
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
            engine.symbols.clone(),
            indodax_token,
        );
        if engine.cfg.venues.contains(&Venue::Indodax) {
            // Independent USDT/IDR cross-check for the live divisor.
            arb_feed::spawn_upbit_usdt_idr_task(&engine.shared.fx_cross);
        }
        tracing::info!("live WS feeds starting (paper execution)");
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

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{Intent, MarketKind, Side, TradeLeg};

    #[test]
    fn live_feed_never_invents_funding() {
        // Regression: the paper soak accumulated 90 synthetic funding
        // entries against live books. Live mode must see no funding data
        // until real perp feeds exist.
        let syms = vec![Symbol::new("BTC", "USDT"), Symbol::new("ETH", "USDT")];
        assert!(synthetic_funding("live", &syms, 1_000).is_empty());
        assert_eq!(synthetic_funding("synthetic", &syms, 1_000).len(), 6);
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
}
