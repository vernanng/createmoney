# Build plan (executed)

Decisions locked before coding:

- Markets: CEX↔CEX (Binance, Bybit, OKX, Indodax — no Coinbase), DEX on-chain
  (Uniswap, PancakeSwap on EVM; Jupiter on Solana), CEX↔DEX hybrid.
- Strategies: cross-exchange, triangular, funding-rate, CEX↔DEX.
- Mode: paper + live-ready (live gated by config + env keys + armed check).
- Stack: Rust workspace, tokio, rust_decimal, single `arb-engine` binary.

Build order: core → feed parsers → paper → strategies → DEX quoting →
store/exec/api → binary + config + Docker/CI. See README for go-live steps.

## Shipped since this plan (status addendum)

The README is now the source of truth for behavior; this file stays as the
decision record. Beyond the original four strategies, the S-series shipped:

- `lag_chase` (S1 lead-lag Binance→Indodax) and `fx_triangle` (S3 IDR-FX),
  auto-wired when Binance + Indodax are both configured.
- S2 depth-adaptive sizing inside `cross_exchange`, S4 timed exit inside
  `funding_rate`, S5 multi-size sweep inside `cex_dex`.
- S6 imbalance gate: wired behind `[params] imbalance_threshold`
  (off unless set). S7 fee table and S8 ablation harness are test-only
  harnesses (`cargo xtask ablate` runs them).
- Live WS venue tasks, control API (`/flows`, `/viewer`, pause/resume), and
  the GHCR + Watchtower paper CD. Venue order senders remain go-live work.
- Kraken books-only venue (spot `book` over WS v2, no sender yet): 80 bps
  tier-1 taker, `KRAKEN_WS_URL` blank = mainnet. Verified live 2026-10-29.

## Final: local Docker build speedup (accepted `2026-10-28`)

Settled: local builds are for iteration speed; CI on `main` stays the only
real image builder (full suite green → release build → `ghcr.io`).
Settled: keep `linux/amd64,linux/arm64` — the Watchtower paper soak runs
natively on Apple Silicon; `amd64`-only would force emulation there.
Settled: no new cache vendor — `publish` already uses GHA cache
(`type=gha`); chef layers would reuse it with no extra setup.
Settled: CD needs no change — it pulls the image, never builds.

Settled (Q1): cargo-chef — faster local and CI rebuilds; accepted
`2026-10-28`.

Settled (Q2): ignore `target/`, `.git/`, `*.jsonl`, `cd-data/`; keep
`.cargo/`, `xtask/`, `docs/`; accepted `2026-10-28`.

Settled (Q3): yes, minimal boot + `/health` gate in publish; accepted
`2026-10-28`.

Settled (Q5): keep `linux/amd64,linux/arm64` — soak stays
Watchtower-updated; accepted `2026-10-28`.

Settled (Q4): defer sccache + mold until chef speedup is measured;
accepted `2026-10-28`.

Settled contract: goal is faster local rebuilds and faster CI image
builds with no behavior change. Non-goals: new cache vendors, paid
builders, `amd64`-only, SQLite migration. Decisions: Q1 cargo-chef, Q2
ignore `target/ .git/ *.jsonl cd-data/`, Q3 boot + `/health` smoke gate
in publish, Q4 defer sccache/mold, Q5 keep multi-arch. Constraints: CI
on `main` stays the only real image builder; runtime image stays
~150 MB slim. Risks: first chef cold build is slower; `Cargo.*`
changes invalidate the cook layer. Validation: cold vs touched-file
rebuild timing, full suite green, smoke gate passes. Unresolved: none —
flip to Final on explicit accept, then `go` executes.

## Final: $10k concentrated capital + self-rebalance + real feeds (accepted)

Status: Final — accepted by user.
Goal: paper as real as live — same fees, latency, depth impact, transfer friction, and failure modes — starting $10k on Binance with self-rebalance, real DEX/funding feeds, larger size when depth allows.
Fidelity bar: live books with tick delay (exists via WS + freshness gate); VWAP fills + taker fees + 2 bps haircut (exists); gas as bps + quote staleness gate (exists); transfer fee/delay model (open, Q2.2); tick/min-notional quantization (open); hedge-leg failure modeling (open, Q7); funding accrual timing (exists).
Scope: docs/plan.md decision record only. No code, no config edits, no later stages approved.
Settled Q1: $10k Binance + small settlement balances elsewhere (~$200 quote plus small base, current default). Reason: counterpart legs fill from tick one; rebalance still proves itself by refilling.
Settled Q2: hybrid — (1) paper-only virtual transfers toward target weights now, logged in JSONL; (2) transfer intents with fee/delay cost model next so paper learns real friction before any real moves. Live auto-withdrawal stays out. Reason: going real eventually, so prove rebalancing in paper first, then add cost model; real fund movement needs its own gate.
Settled Q3: target 500, no hard max_notional cap; size follows S2/S5 profitable depth, bounded only by inventory and venue exposure check. Reason: trade larger when depth allows; cap would leave deep-book edge on the table.
Settled Q4: both from day one — Jupiter HTTP poller plus one EVM V2 reserve poller together. Reason: full `cex_dex` coverage immediately; accept more failure modes at once.
Settled Q5: venues Binance, Bybit, Indodax on configured symbols. Funding shorts only where perps exist (Binance/Bybit); Indodax participates via spot legs (cross/lag/FX). Reason: user call; keeps Indodax edge while funding stays on perp venues.
Settled Q6: paper soak green plus kill-switch/pause drill, then testnet sends before any mainnet key. Reason: prove the system before funds are reachable.
Scope contract (accepted): artifact boundary is this decision record only — docs/plan.md. Out of scope: code, config edits, PRs, live sends. Later build stages are deferred proposals; accepting this record approves no implementation stage. `go` later authorizes only this boundary.
Risk mitigations: R1 fake agility — fee/delay transfer model before any real move, live never auto-moves, deficits in `/status`, every virtual move in JSONL. R2 size impact/stale fills — S2/S5 depth walk with net-edge gate (fees + slip + 2 bps paper haircut), inventory abort, venue exposure check, freshness + crossed-book rejection. R3 DEX staleness/gas — quote timestamps gated by `max_book_age_ms`, gas as bps in edge, TTL, withhold when stale. R4 funding drag — fire-once + restored opens, S4 window-mean exit, entry cost of 2x taker + slip vs annualized threshold. R5 live exposure — soak green + kill/pause drill, armed guard, testnet-only senders, per-strategy pause.
Settled Q7: defer non-atomic fills; hedge-failure drill is the path after feeds and sizing soak. Reason: keeps fills clean now, still proves the stranded path later.
Settled Q8: one EVM pool plus SOL on Jupiter — ETH/USDT Uniswap V2 plus SOL/USDT and SOL/USDC on Jupiter. Reason: smallest set proving both paths on traded symbols.
Settled Q9: green soak is 7 days, net positive after fees, zero stale-book fills, plus clean kill and pause drill. Reason: long enough to see weekends and funding prints, short enough to ship.
Unresolved: none — accepted.
Execution (all stages approved): owner approved all six implementation stages. One branch per stage; no commits without an explicit ask.
Shipped: per-venue `[paper.capital]` ($10k Binance); paper self-rebalance with fee/delay + JSONL `transfer` events + `/status` transfers; target 500 with 25% venue-share guard; Jupiter + EVM-V2 DEX pollers with `dex_max_quote_age_ms`; Binance/Bybit/OKX funding streams; `outage_and_stale_books_never_partially_fill` drill; soak gate (7d, net positive, zero stale fills, kill+pause drill) in README going-live.
