mod config;

use arb_api::{
    render_flows_envelope, render_metrics, route, route_viewer, ConnectorHealth, Control,
    EngineStatus, OpenPosition, PortfolioStatus, StrategyMetrics, UsdtIdrHealth, VenueBalance,
};
use arb_core::Intent;
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

struct Engine {
    cfg: config::Config,
    symbols: Vec<Symbol>,
    strategies: Vec<Box<dyn Strategy>>,
    paper: PaperExecutor,
    store: MemoryStore,
    shared: Arc<Shared>,
    opportunities: u64,
    per_strategy: BTreeMap<String, StrategyMetrics>,
    live: LiveExecutor,
    session_id: String,
    /// Wall-clock start of this run, surfaced via status for viewer uptime.
    started_ms: i64,
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
            let mut cross = CrossExchange::new(params.clone(), cex.clone(), symbols.clone());
            // S6: off unless configured; a set threshold gates every pair.
            if let Some(t) = cfg.params.imbalance_threshold {
                cross = cross.with_imbalance(t);
            }
            strategies.push(Box::new(cross));
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
        // Conservative jump threshold so paper stays quiet until a real
        // dislocation prints; only wired when both venues exist.
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

        // One clock for the whole run: the viewer renders uptime from it.
        let started_ms = now_ms();
        let shared = Arc::new(Shared {
            // The kill flag lives here now that the risk gate is gone: when
            // set, the tick loop emits no intents until revived.
            control: Mutex::new(Control::new(Arc::new(AtomicBool::new(false)))),
            status: Mutex::new(EngineStatus {
                mode: cfg.engine.mode.clone(),
                killed: false,
                started_ms,
                trades: 0,
                realized_pnl_quote: Decimal::ZERO,
                opportunities_seen: 0,
                connectors: vec![],
                paused_strategies: vec![],
                imbalance_threshold: cfg.params.imbalance_threshold,
                open_positions: vec![],
                usdt_idr: UsdtIdrHealth::default(),
                portfolio: PortfolioStatus::default(),
                transfers: 0,
            }),
            per_strategy: Mutex::new(BTreeMap::new()),
            flows: Mutex::new(render_flows_envelope(&[], 0)),
            live_books: LiveBooks::default(),
            live_connectors: LiveConnectors::default(),
            fx_cross: FxCrossCheck::default(),
            live_dex: LiveDexQuotes::default(),
            live_funding: LiveFunding::default(),
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
        let mut paper = PaperExecutor::new(PaperConfig {
            transfer_fee_bps: cfg.paper.transfer_fee_bps,
            transfer_delay_ms: cfg.paper.transfer_delay_ms,
            ..Default::default()
        });
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
                        cfg.paper.quote_for(v)
                    } else {
                        // Seed enough base to sell ~5x target notional at the
                        // reference price, so dynamic S2 sizes to ~$50 fill.
                        // Exact USDT pair first: a prefix match would catch the
                        // ETH/BTC triangle cross (0.03) and overseed ETH ~666x.
                        let ref_px = anchor_tbl
                            .get(&format!("{a}/USDT"))
                            .copied()
                            .or_else(|| {
                                anchor_tbl
                                    .iter()
                                    .find(|(k, _)| k.split('/').next() == Some(a.as_str()))
                                    .map(|(_, v)| *v)
                            })
                            .unwrap_or(Decimal::new(100, 0));
                        Decimal::new(5, 0) * cfg.params.target_notional_quote / ref_px
                    };
                    paper.seed(v, a, amt);
                }
            }
        }
        // Baseline for unrealized P/L: mark the seeded ledger at the reference
        // prices, the same marks live mids fall back to.
        paper.starting_equity = paper.equity_quote(&anchor_marks());
        Ok(Self {
            cfg,
            symbols,
            strategies,
            paper,
            store,
            shared,
            opportunities: 0,
            per_strategy: BTreeMap::new(),
            live,
            session_id: new_session_id(),
            started_ms,
            last_fx: RateDecision::default(),
        })
    }

    /// Stamp the JSONL history with this run's boundary record.
    fn record_session_start(&mut self) {
        let rec = SessionStart::new(
            self.session_id.clone(),
            ENGINE_VERSION.into(),
            self.cfg.engine.mode.clone(),
            "live".into(),
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
        // Live books only: fresh ones win, stale venues are skipped (never
        // backfilled) so paper only sees real spreads.
        let live_map = self
            .shared
            .live_books
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        // One snapshot, one divisor: every IDR book this tick converts at
        // the same selected rate (or not at all, when the decision has none).
        let fx = self.fx_decision(&live_map, &needed, now);
        self.last_fx = fx;
        for sym in &needed {
            let venues = self.cfg.venues.clone();
            for v in &venues {
                if !v.is_cex() {
                    continue;
                }
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
            }
        }
        // Real polled DEX quotes (Jupiter + EVM V2). Staleness is gated by
        // `cex_dex` itself via `dex_max_quote_age_ms`; a stalled poller
        // degrades to no quotes, never to stale fills.
        if let Ok(dex) = self.shared.live_dex.lock() {
            view.dex_quotes.extend(dex.values().cloned());
        }
        // Real perp funding prints. Anything older than `max_book_age_ms`
        // stays out of the view, so `funding_rate` only sees live prints.
        if let Ok(funding) = self.shared.live_funding.lock() {
            view.funding.extend(
                funding
                    .values()
                    .filter(|(_, ts)| now - ts <= self.cfg.params.max_book_age_ms)
                    .map(|(r, _)| r.clone()),
            );
        }
        // Publish the divisor for provenance (FxTriangle records it).
        view.usdt_idr_mid = fx.rate.map(|r| r.mid);
        view.usdt_idr_ts_ms = fx.rate.map(|r| r.ts_ms);
        view
    }

    fn tick(&mut self) {
        let view = self.build_view();
        let now = view.now_ms;
        // Settle due virtual transfers before pricing new intents, so a
        // delayed rebalance becomes spendable exactly when its delay elapses.
        self.paper.settle_due(now);
        // No pre-trade risk gate: every emitted intent goes to the paper
        // executor, which still enforces inventory. The kill-switch stays:
        // when set, no intents are generated until revived.
        let (killed, paused): (bool, Vec<String>) = self
            .shared
            .control
            .lock()
            .map(|c| (c.is_killed(), c.paused()))
            .unwrap_or_default();
        let mut intents = Vec::new();
        if !killed {
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
        }
        for intent in intents {
            self.opportunities += 1;
            self.per_strategy
                .entry(intent.strategy.clone())
                .or_default()
                .emitted += 1;
            // Live gate: even in live mode, sends must pass the armed check.
            if self.live.mode == ExecMode::Live {
                if let Err(e) = self.live.guard(&intent) {
                    tracing::warn!(%e, "live send blocked; falling back to paper fill for accounting");
                }
            }
            // Venue exposure: skip prints that would spend more quote on
            // one venue than the configured share of its ledger. No hard
            // notional cap — large prints pass while the venue funds them.
            if venue_quote_exceeded(
                &intent,
                &self.paper,
                self.cfg.params.max_venue_quote_share_bps,
            ) {
                self.per_strategy
                    .entry(intent.strategy.clone())
                    .or_default()
                    .risk_rejected += 1;
                tracing::debug!(strategy = %intent.strategy, "intent exceeds venue quote share; skipped");
                continue;
            }
            match self.paper.execute(&intent, &view.books, now) {
                Ok(result) => self.on_paper_fill(intent, result, now),
                Err(e) => {
                    // Self-rebalance (paper-only, never a live movement):
                    // cover inventory shortfalls with virtual venue-to-venue
                    // transfers, then retry the intent once at these prices.
                    let moved = self.paper.rebalance_for(&intent, &view.books, now);
                    if moved.is_empty() {
                        self.per_strategy
                            .entry(intent.strategy.clone())
                            .or_default()
                            .fill_failed += 1;
                        tracing::debug!(%e, "paper fill failed");
                        continue;
                    }
                    for t in moved {
                        tracing::info!(from = %t.from, to = %t.to, asset = %t.asset,
                            amount = %t.amount, fee = %t.fee, "paper rebalance");
                        let stored = StoredTransfer {
                            event: "transfer".into(),
                            from: t.from,
                            to: t.to,
                            asset: t.asset,
                            amount: t.amount,
                            fee: t.fee,
                            ts_ms: t.ts_ms,
                            available_at_ms: t.available_at_ms,
                            session_id: self.session_id.clone(),
                        };
                        if let Err(e) = self.store.record_transfer(stored) {
                            tracing::error!(%e, "store write failed");
                        }
                    }
                    match self.paper.execute(&intent, &view.books, now) {
                        Ok(result) => self.on_paper_fill(intent, result, now),
                        Err(e) => {
                            self.per_strategy
                                .entry(intent.strategy.clone())
                                .or_default()
                                .fill_failed += 1;
                            tracing::debug!(%e, "paper fill failed after rebalance");
                        }
                    }
                }
            }
        }
        self.release_closed();
        self.publish();
    }

    /// Paper accounting for one filled intent: metrics, JSONL, and (live
    /// mode only) best-effort testnet sends that can only log, never mutate.
    fn on_paper_fill(&mut self, intent: Intent, result: PaperFill, now: i64) {
        // Live Binance/Bybit legs (testnet): collect while `intent`
        // is still borrowed; sends spawn after the paper accounting
        // below, so a send failure can only log — never lose paper.
        let live_legs = if self.live.mode == ExecMode::Live && self.live.guard(&intent).is_ok() {
            intent
                .legs
                .iter()
                .filter(|l| l.venue == Venue::Binance || l.venue == Venue::Bybit)
                .filter_map(|leg| self.live.keys_for(leg.venue).map(|k| (leg.clone(), k)))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
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
        for (leg, keys) in live_legs {
            tokio::spawn(async move {
                let now = arb_core::now_ms();
                let res = if leg.venue == Venue::Bybit {
                    let base = arb_exec::bybit_api_base();
                    arb_exec::send_bybit_limit(&leg, &keys, &base, now).await
                } else {
                    let base = arb_exec::binance_api_base();
                    arb_exec::send_binance_limit(&leg, &keys, &base, now).await
                };
                if let Err(e) = res {
                    tracing::error!(%e, "live send failed; paper fill kept");
                }
            });
        }
    }

    /// Drain reported funding exits so the closed set cannot grow without
    /// bound. Re-entry is owned by the funding strategy's open set.
    fn release_closed(&mut self) {
        for st in self.strategies.iter_mut() {
            for _ in st.take_closed() {}
        }
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
        // Portfolio marks: fresh live-book mids win; anchors fill the rest.
        // Non-USDT pairs never mark.
        let mut marks = anchor_marks();
        for ((_, sym), book) in &live_map {
            let mut parts = sym.split('/');
            if let (Some(base), Some("USDT")) = (parts.next(), parts.next()) {
                if now - book.ts_ms <= self.cfg.params.max_book_age_ms && !book.is_crossed() {
                    if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
                        marks.insert(
                            base.to_string(),
                            (bid.price + ask.price) / Decimal::new(2, 0),
                        );
                    }
                }
            }
        }
        let equity = self.paper.equity_quote(&marks);
        let portfolio = PortfolioStatus {
            per_venue: self
                .paper
                .balances_snapshot()
                .iter()
                .flat_map(|(v, m)| {
                    m.iter().map(|(a, amt)| VenueBalance {
                        venue: *v,
                        asset: a.clone(),
                        amount: *amt,
                    })
                })
                .collect(),
            equity_quote: equity,
            baseline_quote: self.paper.starting_equity,
            realized_pnl_quote: self.paper.realized_pnl,
            unrealized_pnl_quote: self.paper.unrealized_pnl_quote(&marks),
        };
        EngineStatus {
            mode: self.cfg.engine.mode.clone(),
            killed,
            started_ms: self.started_ms,
            trades: self.paper.trades,
            realized_pnl_quote: self.paper.realized_pnl,
            opportunities_seen: self.opportunities,
            transfers: self.paper.transfers,
            portfolio,
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
                    ConnectorHealth {
                        venue: *v,
                        connected: conn.map(|c| c.connected).unwrap_or(false),
                        last_msg_ms: conn.and_then(|c| c.last_msg_ms),
                        last_book_ms: venue_books.iter().map(|b| b.ts_ms).max(),
                        books: venue_books.len(),
                        fresh_books: venue_books
                            .iter()
                            .filter(|b| {
                                now - b.ts_ms <= self.cfg.params.max_book_age_ms && !b.is_crossed()
                            })
                            .count(),
                        last_error: conn.and_then(|c| c.last_error.clone()),
                    }
                })
                .collect(),
            paused_strategies,
            imbalance_threshold: self.cfg.params.imbalance_threshold,
            open_positions: self
                .strategies
                .iter()
                .flat_map(|st| {
                    let name = st.name().to_string();
                    st.open_positions()
                        .into_iter()
                        .map(move |(venue, symbol)| OpenPosition {
                            strategy: name.clone(),
                            venue,
                            symbol: symbol.to_string(),
                        })
                })
                .collect(),
            usdt_idr: {
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
            },
        }
    }
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
    tracing::info!(addr, "control API listening (GET /health /status /metrics /flows /viewer, POST /kill /revive /pause/<s> /resume/<s>)");
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
                if let Some(hit) = route_viewer(method, path, &flows) {
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
    let api_addr = cfg.engine.api_addr.clone();
    let tick_ms = cfg.engine.tick_ms;
    let mut engine = Engine::new(cfg)?;
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
        engine.symbols.clone(),
        indodax_token,
    );
    if engine.cfg.venues.contains(&Venue::Indodax) {
        // Independent USDT/IDR cross-check for the live divisor.
        arb_feed::spawn_upbit_usdt_idr_task(&engine.shared.fx_cross);
    }
    // Real DEX quotes in (Jupiter HTTP + one EVM V2 pool), paper execution
    // unchanged. Each source is fail-closed: unset means no quotes, never
    // invented ones.
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
    let dex_evm = if engine.cfg.dex.evm_rpc_url.trim().is_empty()
        || engine.cfg.dex.evm_pool.trim().is_empty()
    {
        None
    } else {
        Some(arb_feed::EvmCfg {
            symbol: Symbol::new("ETH", "USDT"),
            rpc_url: engine.cfg.dex.evm_rpc_url.clone(),
            pool: engine.cfg.dex.evm_pool.clone(),
            token0_is_base: engine.cfg.dex.evm_token0_is_base,
            base_decimals: engine.cfg.dex.evm_base_decimals,
            quote_decimals: engine.cfg.dex.evm_quote_decimals,
            gas_quote: engine.cfg.dex.evm_gas_quote,
            target_notional_quote: engine.cfg.params.target_notional_quote,
            poll_secs: engine.cfg.dex.evm_poll_secs,
        })
    };
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
        let engine = Engine::new(cfg).unwrap();
        // Dynamic rule: 5x target notional at the USDT anchor, no fixed floor.
        // BTC: 5000/80000 = 0.0625 exact. ETH must be 5000/2300, not the
        // 66666.67 the old prefix match produced from the ETH/BTC cross.
        assert_eq!(
            engine.paper.balance(Venue::Binance, "BTC"),
            Decimal::new(625, 4)
        );
        let eth = engine.paper.balance(Venue::Binance, "ETH");
        assert!(
            eth > Decimal::ONE && eth < Decimal::new(3, 0),
            "ETH seed: {eth}"
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
        let mut engine = Engine::new(cfg).unwrap();
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
        let mut engine = Engine::new(cfg).unwrap();
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
