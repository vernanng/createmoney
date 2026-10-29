//! Tick engine: strategies, paper fills, funding, status. Moved out of main.rs.

use super::*;

pub(crate) struct Engine {
    pub(crate) cfg: config::Config,
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) strategies: Vec<Box<dyn Strategy>>,
    pub(crate) paper: PaperExecutor,
    pub(crate) store: MemoryStore,
    pub(crate) shared: Arc<Shared>,
    pub(crate) opportunities: u64,
    pub(crate) per_strategy: BTreeMap<String, StrategyMetrics>,
    /// Tops at the last fill per opportunity key. Paper fills never consume
    /// book depth, so without this the same print refires every tick.
    pub(crate) last_fill_print: BTreeMap<String, String>,
    pub(crate) live: LiveExecutor,
    pub(crate) session_id: String,
    /// Wall-clock start of this run, surfaced via status for viewer uptime.
    pub(crate) started_ms: i64,
    /// Last tick's USDT/IDR rate decision, surfaced via status/metrics.
    pub(crate) last_fx: RateDecision,
    /// Funding interval already credited per open pair. A print's
    /// `next_funding_ms` pays once, when it passes; stale prints pay never.
    pub(crate) funding_credited_through: BTreeMap<(Venue, String), i64>,
}

impl Engine {
    pub(crate) fn new(cfg: config::Config) -> anyhow::Result<Self> {
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
            transfer_fees: cfg.paper.transfer_fees.clone(),
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
            // The only pretend inventory in the book: Binance holds one full
            // fill of each basket asset, so both legs can fire from tick one.
            // Everything else starts at zero, so the seeded portfolio fits
            // the configured starting capital instead of ~$372k of base.
            const SEED_BASE_BASKET: [&str; 3] = ["BTC", "ETH", "SOL"];
            for v in venues {
                for a in &assets {
                    let amt = if a == "USDT" || a == "USDC" {
                        cfg.paper.quote_for(v)
                    } else if v == Venue::Binance && SEED_BASE_BASKET.contains(&a.as_str()) {
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
                        cfg.params.target_notional_quote / ref_px
                    } else {
                        continue;
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
            last_fill_print: BTreeMap::new(),
            live,
            session_id: new_session_id(),
            started_ms,
            last_fx: RateDecision::default(),
            funding_credited_through: BTreeMap::new(),
        })
    }

    /// Stamp the JSONL history with this run's boundary record.
    pub(crate) fn record_session_start(&mut self) {
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
    pub(crate) fn fx_decision(
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

    pub(crate) fn build_view(&mut self) -> MarketView {
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
        // Venue ledgers backing the share cap, so single-venue strategies
        // size to what the engine's risk gate would accept anyway.
        view.venue_quote = self
            .cfg
            .venues
            .iter()
            .copied()
            .map(|v| {
                let q = self.paper.balance(v, "USDT") + self.paper.balance(v, "USDC");
                (v, q)
            })
            .collect();
        view
    }

    pub(crate) fn tick(&mut self) {
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
            // Repeat-fill guard: paper fills never consume book depth, so an
            // unchanged top of book would refill every tick. Drop the intent
            // when every leg's top matches the last fill's.
            // shortcut: same-price refills are dropped, not sized down;
            // upgrade to paper-local depth consumption when partial fills matter.
            let key = fill_key(&intent);
            let print = fill_print(&intent, &view.books);
            if let Some(p) = &print {
                if self.last_fill_print.get(&key) == Some(p) {
                    self.per_strategy
                        .entry(intent.strategy.clone())
                        .or_default()
                        .dup_skipped += 1;
                    tracing::debug!(strategy = %intent.strategy, "unchanged tops since last fill; skipped");
                    continue;
                }
            }
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
                Ok(result) => {
                    if let Some(p) = print.clone() {
                        self.last_fill_print.insert(key.clone(), p);
                    }
                    self.on_paper_fill(intent, result, now)
                }
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
                    // Transfer friction is a realized trading cost, not free
                    // agility: debit it from the strategy's and the engine's
                    // realized PnL so the edge math stays honest. (The ledger
                    // burn already happened inside the paper executor.)
                    let transfer_cost: Decimal = moved.iter().map(|t| t.fee_quote).sum();
                    self.debit_transfer_fees(&intent.strategy, transfer_cost);
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
                        Ok(result) => {
                            if let Some(p) = print.clone() {
                                self.last_fill_print.insert(key.clone(), p);
                            }
                            self.on_paper_fill(intent, result, now)
                        }
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
        self.settle_funding(now, &view);
        self.release_closed();
        self.publish();
    }

    /// Debit realized transfer friction to one strategy and the engine total.
    /// `total_fee_quote` is the quote-denominated sum the paper executor
    /// stamped on the transfers that enabled this intent.
    pub(crate) fn debit_transfer_fees(&mut self, strategy: &str, total_fee_quote: Decimal) {
        if total_fee_quote == Decimal::ZERO {
            return;
        }
        self.paper.realized_pnl -= total_fee_quote;
        self.per_strategy
            .entry(strategy.to_string())
            .or_default()
            .realized_pnl_quote -= total_fee_quote;
    }

    /// Paper accounting for one filled intent: metrics, JSONL, and (live
    /// mode only) best-effort testnet sends that can only log, never mutate.
    pub(crate) fn on_paper_fill(&mut self, intent: Intent, result: PaperFill, now: i64) {
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
        // Opens belong to fills, not intents: a strategy tracks a pair
        // only once the executor actually filled it, so failed fills
        // cannot strand phantoms that never close.
        if let Some(leg) = intent.legs.first() {
            for st in self.strategies.iter_mut() {
                if st.name() == intent.strategy {
                    st.note_filled(leg.venue, &leg.symbol);
                }
            }
        }
        // Funding entries earn per-interval credits on their spot notional.
        if intent.strategy == "funding_rate" {
            if let Some(f) = result
                .fills
                .iter()
                .find(|f| f.market == MarketKind::Spot && f.side == Side::Buy)
            {
                self.paper.note_funding_fill(
                    f.venue,
                    &f.symbol.to_string(),
                    &f.symbol.quote,
                    f.price * f.qty_base,
                );
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

    /// Credit elapsed funding intervals on open pairs. A print's
    /// `next_funding_ms` pays exactly once, when it passes; a stale or
    /// missing print pays nothing, and unknown pairs are ignored.
    pub(crate) fn settle_funding(&mut self, now: i64, view: &MarketView) {
        let open: Vec<(Venue, String)> = self.paper.funding_open.keys().cloned().collect();
        for (venue, sym_str) in open {
            let Some(fr) = view
                .funding
                .iter()
                .find(|f| f.venue == venue && f.symbol.to_string() == sym_str)
            else {
                continue;
            };
            if fr.next_funding_ms > now {
                continue;
            }
            if self.funding_credited_through.get(&(venue, sym_str.clone()))
                == Some(&fr.next_funding_ms)
            {
                continue;
            }
            let got = self
                .paper
                .credit_funding(venue, &sym_str, fr.rate_per_interval);
            self.funding_credited_through
                .insert((venue, sym_str.clone()), fr.next_funding_ms);
            tracing::info!(venue = %venue, symbol = %sym_str, amount = %got, "funding credited");
        }
    }

    /// Drain reported funding exits so the closed set cannot grow without
    /// bound. Re-entry is owned by the funding strategy's open set.
    pub(crate) fn release_closed(&mut self) {
        for st in self.strategies.iter_mut() {
            for _ in st.take_closed() {}
        }
    }

    /// Push a fresh snapshot to the API task.
    pub(crate) fn publish(&self) {
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
            *f = render_flows_envelope(&items, self.store.trade_count() as usize);
        }
    }

    pub(crate) fn status(&self) -> EngineStatus {
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
        // Entry size from the latest matching stored trade; live mark from
        // the same marks used for equity (None = stale, shown as stale).
        // shortcut: missing history means unknown size, not zero-size.
        let open_positions: Vec<OpenPosition> = self
            .strategies
            .iter()
            .flat_map(|st| {
                let name = st.name().to_string();
                st.open_positions()
                    .into_iter()
                    .map(|(venue, symbol)| (name.clone(), venue, symbol))
                    .collect::<Vec<_>>()
            })
            .map(|(name, venue, symbol)| {
                let sym = symbol.to_string();
                let base = sym.split('/').next().unwrap_or("").to_string();
                let found: Option<(Decimal, Decimal)> = self
                    .store
                    .trades()
                    .iter()
                    .rev()
                    .find(|t| {
                        t.intent.strategy == name
                            && t.intent
                                .legs
                                .iter()
                                .any(|l| l.venue == venue && l.symbol.to_string() == sym)
                    })
                    .and_then(|t| {
                        t.intent
                            .legs
                            .iter()
                            .find(|l| l.venue == venue && l.symbol.to_string() == sym)
                            .map(|l| (l.qty_base, l.price))
                    });
                let (qty_base, entry_price_quote) = match found {
                    Some((q, p)) if q > Decimal::ZERO => (q, Some(p)),
                    _ => (Decimal::ZERO, None),
                };
                let (mark_price_quote, market_value_quote) =
                    match (marks.get(&base), qty_base > Decimal::ZERO) {
                        (Some(m), true) => (Some(*m), Some(qty_base * *m)),
                        _ => (None, None),
                    };
                OpenPosition {
                    strategy: name,
                    venue,
                    symbol: sym,
                    qty_base,
                    entry_price_quote,
                    mark_price_quote,
                    market_value_quote,
                }
            })
            .collect();
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
            open_positions,
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
