# createmoney — Rust arbitrage engine

Multi-venue, multi-strategy arbitrage engine in Rust (edition 2021, tokio,
`rust_decimal` — no floats for money). **Paper-first**: real strategy logic
on real order books, simulated book-walking fills. Live sending stays locked
behind `mode = "live"` + env keys + the armed check in `arb-exec`, and even
then only Binance/Bybit testnet senders exist (see [Going live](#going-live-read-before-flipping-anything)).

One binary, `arb-engine`, runs a tick loop (default 250 ms):
live WS books → `MarketView` → strategies emit `Intent`s → paper fill
(VWAP + fees) → store (memory + JSONL) → control API on `:8787`.
Strategies never place orders; executors own everything after the intent.
There is no pre-trade gate — the paper fill enforces inventory, and
`POST /kill` stops emitting until revived.

## Quick start

```bash
cargo run -p arb-engine -- --config config.toml --ticks 40   # paper smoke run
cargo run -p arb-engine -- --config config.toml              # run until Ctrl-C
curl http://127.0.0.1:8787/health
curl http://127.0.0.1:8787/status
curl -X POST http://127.0.0.1:8787/kill                      # trip kill-switch
```

Paper trades append to `paper-trades.jsonl`. Logs via `RUST_LOG=debug`.
CLI flags: `--config <path>` (default `config.toml`), `--ticks <n>`
(default `0` = until Ctrl-C).

Quality gates (same as CI):

```bash
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo xtask ablate    # S8 ablation matrix, then the full suite
```

## Venues

| Venue | Kind | Role in the engine | Default taker fee |
|---|---|---|---|
| Binance | CEX | spot books, perp funding | 10 bps (0.10%) |
| Bybit | CEX | spot books, perp funding | 10 bps |
| OKX | CEX | spot books, perp funding | 10 bps |
| Indodax | CEX (IDR) | IDR books normalized with the executable sides of the live, cross-checked USDT/IDR rate | 51 bps (~0.51% incl. tax) |
| Kraken | CEX | spot books (no sender yet) | 80 bps (0.80% tier-1) |
| Uniswap | DEX (EVM) | V2 pools quoted locally; V3 quoter types/addresses | 30 bps V2 (0.30%) |
| PancakeSwap | DEX (EVM) | V2 pools quoted locally; V3 quoter types/addresses | 25 bps V2 (0.25%) |
| Jupiter | DEX (Solana) | Swap API quote URL building + parsing | 0 bps (inside the route) |

Fees are `Venue::default_taker_bps` in `crates/core` — defaults to verify
against your account, not account truth. The binary spawns one reconnecting
WebSocket task per CEX venue (`crates/feed`); stale venues are skipped,
never backfilled. Indodax books convert only when the live USDT/IDR divisor
is fresh (≤2 s), tight, and corroborated by independent consensus — a
single-source unchecked divisor never converts a book, and cash USD/IDR is
never used.

## Strategies

Four strategies implement the `Strategy` trait and are engine-wired:

| # | Strategy (`name()`) | File | Idea in one line | Legs | Round-trip? | Config toggle |
|---|---|---|---|---|---|---|
| 1 | `cross_exchange` | `cross_exchange.rs` | Buy cheap venue, sell rich venue (S2 adaptive sizing) | 2 spot | yes | `[strategies] cross_exchange` |
| 2 | `triangular` | `triangular.rs` | One-venue A→B→C→A cycle ends with more than it started | 3 spot | yes | `[strategies] triangular` |
| 3 | `funding_rate` | `funding.rs` | Long spot + short perp to collect positive funding (S4 exit) | 1 spot + 1 perp | **no** (opens a position) | `[strategies] funding_rate` |
| 4 | `cex_dex` | `cex_dex.rs` | CEX book vs DEX quote, net of gas (S5 size sweep) | 1 CEX spot + 1 DEX | yes | `[strategies] cex_dex` |

How they behave, briefly:

- **`cross_exchange`** walks both books level by level and stops at the
  first marginal chunk that fails the net-edge bar, so a passing average
  never drags in loss-making tail depth. Assumes pre-funded inventory (no
  transfers in the loop); at most one intent per symbol per tick; a filled
  spread cannot refill until it has been seen closed and reopens.
- **`triangular`** prices the whole A→B→C→A chain off the books (each step
  at VWAP minus taker fee) and fires only on the end/start ratio. Not wired
  on Indodax or Kraken, where three taker legs cannot pay.
- **`funding_rate`** enters long spot + short perp (Binance/Bybit/OKX) only
  when annualized funding clears the entry bar *and* repays entry + exit
  round-trip costs over `funding_planned_hold_hours` (default 720 h). It
  opens a position — PnL accrues over time, never at fill. S4 emits real
  exit legs when recent funding decays; filled entries reload from the
  JSONL at startup so restarts never duplicate a position.
- **`cex_dex`** trades CEX vs DEX in both directions. The DEX side arrives
  net of pool fee/impact plus a gas estimate; gas is charged in bps of
  notional, so $50 of L1 gas correctly kills a small spread. S5 sweeps
  0.1×/0.5×/1.0× of target size and keeps the best net edge. Engine passes
  only SOL, ETH, BNB (the bases with live quote sources: Jupiter HTTP, and
  Uniswap/PancakeSwap V2 pools via `eth_call`).
- **S6 `imbalance`** is a filter on `cross_exchange`, not a strategy: take
  only spreads both books agree will converge. Off unless
  `[params] imbalance_threshold` is set. **S7 `fee_table`** (per-route
  learned thresholds) is a test-only harness, not engine-wired. **S8
  `ablate`** (`cargo xtask ablate`) proves each cost model matters by
  stripping it and requiring PnL to inflate.
- Retired: the IDR-FX strategy was deleted and `lag_chase` unwired — both
  duplicated `cross_exchange`, so one dislocation could fill twice.

Pause any strategy at runtime: `POST /pause/<name>` / `POST /resume/<name>`.

### Shared contract

All strategies share `StrategyParams` (from `[params]` in config). Net edge
everywhere is `gross_bps − buy_fee − sell_fee − slip_buffer` at **VWAP over
the actual quantity**, never the touch. A missing, stale, or crossed book
skips that venue only — never the whole scan.

| Param | Default | Meaning |
|---|---|---|
| `min_edge_bps` | 15 | Minimum **net** edge (after fees + slippage buffer) to emit |
| `slip_buffer_bps` | 5 | Extra bps subtracted from every edge before comparing |
| `target_notional_quote` | 500 | Base order size in quote (USD/USDT) terms |
| `min_notional_quote` | 500 | `cross_exchange` + funding-entry hard floor; smaller prints cannot cover fixed transfer friction, and share-capped funding entries would drain a venue in dust slices |
| `rebalance_cost_bps` | 20 | Eventual inventory-transfer friction amortised into the `cross_exchange` edge gate |
| `max_venue_quote_share_bps` | 2500 | One intent may spend at most this share (bps) of a venue's USDT+USDC ledger; 0 = off |
| `dex_max_quote_age_ms` | 2000 | Polled DEX quotes older than this never reach `cex_dex` |
| `max_book_age_ms` | 2000 | Books older than this, or crossed, never trade |
| `ttl_ms` | 1500 | Intent lifetime |

Per-gate rejection counters (`edge`, `imbalance`, `min_notional`, `stale`,
`share`) are exposed on `/strategies` and `/metrics`, so an `emitted = 0`
result can be attributed to the gate that blocked it.

## Paper execution and the live gate

- **Paper fills** (`crates/paper`) walk the book (VWAP), pay real taker
  fees plus a 2 bps extra-slippage haircut, against balances seeded from
  `[paper]`. Inventory is enforced: an unfunded buy or a sell of an unheld
  asset aborts the **whole** intent with no partial mutation. Open perp
  positions are marked to market in portfolio equity (a hedged spot/perp
  pair reports its basis, not the spot leg's drift) and cash-settled into
  realized PnL when a fill closes them. Realized PnL
  is computed only for round-trip intents (net of transfer fees); funding
  entries accrue over time.
- **Rebalancing** stays out of the strategy loop by design, but it is not
  free in the model: virtual transfers burn the sender's per-venue fee,
  are sized from simulated ledger deficits, and an intent whose expected
  profit cannot cover transfer fees is left unfunded instead of funded at
  a guaranteed loss.
- **Live gate** (`crates/exec`): `LiveExecutor::guard` refuses to arm
  unless mode is `live`, venue keys are in env, and the executor was
  constructed armed. Binance spot and Bybit V5 spot testnet senders are
  wired + unit-tested (fail-closed to testnet hosts); other venues have
  no senders yet.
- **Kill-switch**: `POST /kill` stops the tick loop emitting new intents;
  `POST /revive` clears it.

## Control API

Localhost HTTP on `api_addr` (default `127.0.0.1:8787`). Reads are open;
if `API_TOKEN` is set, POSTs need `?token=<value>` and return 401 without it.

| Route | Method | Effect |
|---|---|---|
| `/health` | GET | `{"ok":true}` |
| `/status` | GET | mode, killed, uptime, trades, realized PnL, opportunities, per-venue connector health, paused strategies, portfolio, open positions with live marks |
| `/metrics` | GET | text metrics: per-strategy funnel counters + per-venue book freshness |
| `/strategies` | GET | per-strategy funnel counters as JSON |
| `/flows` | GET | JSON snapshot of the last 200 trades |
| `/` and `/dashboard` | GET | strategy-flow page: 2D venue flow map + ops panels, zero dependencies |
| `/kill` / `/revive` | POST | trip / clear the kill-switch |
| `/pause/<strategy>` / `/resume/<strategy>` | POST | pause / resume one strategy by `name()` |

## Config

A working paper `config.toml` is committed; `config.example.toml` documents
every key. Secrets only via env (see `.env.example`, auto-loaded from
`./.env` at startup; real env wins) — never in config. The Indodax feed
needs `INDODAX_WS_TOKEN` (public static market-data token); without it that
venue is disabled at startup. Top-level `venues` / `symbols` must stay
above the first `[table]` header.

| Key | Default (committed config) | Meaning |
|---|---|---|
| `venues` | binance, bybit, okx, indodax, kraken | CEX venues to run |
| `symbols` | 20 `…/USDT` pairs (BTC, ETH, SOL, XRP, DOGE, BNB, …) | traded pairs; `ETH/BTC` books are always built for triangular |
| `[engine] mode` | `paper` | `paper` \| `live` (live still needs env keys + armed check) |
| `[engine] tick_ms` | 250 | tick-loop period |
| `[engine] api_addr` | `127.0.0.1:8787` | control-API bind; omit to disable. `$PORT`, when set, overrides to `0.0.0.0:$PORT` |
| `[engine] jsonl_path` | `paper-trades.jsonl` | paper-trade log; history reloads at startup |
| `[strategies]` | all `true` | toggles for the four active strategies |
| `[params]` | see [Shared contract](#shared-contract) | edge/sizing/freshness parameters |
| `[params] min_funding_annualized_bps` | 500 | funding-rate annualized entry bar |
| `[params] funding_planned_hold_hours` | 720 | horizon over which funding must repay entry + exit costs |
| `[params] imbalance_threshold` | unset (off) | S6 gate: both-sided imbalance bar, e.g. 0.2 |
| `[indodax] rate_max_age_ms` | 2000 | max age of the live USDT/IDR book used as divisor |
| `[indodax] rate_max_spread_bps` | 50 | max USDT/IDR top-of-book spread (bps of mid) |
| `[indodax] rate_max_deviation_bps` | 300 | max deviation vs independent consensus before Indodax is withheld |
| `[paper] start_quote_per_venue` | 200 | seeded USDT per venue; `[paper.capital] <venue>` overrides one venue. The shipped config spreads $10k across the five CEX venues so the 25% share guard admits target-size prints |

Before going live, set `[paper]` amounts to your real planned capital per
venue so the paper soak behaves like the live book.

## Layout

```
crates/core      types (Venue/Symbol/Intent), L2 book (snapshot/diff/VWAP/depth), decimal money math
crates/feed      Binance/Bybit/OKX/Indodax/Kraken parsers, WS URLs/frames, live WS tasks, IDR->USD books
crates/strategy  Strategy trait + MarketView; four active strategies + imbalance filter (S6); fee_table (S7) / ablate (S8) test harnesses
crates/paper     book-walking fill simulator (fees + slippage haircut, inventory-enforced)
crates/exec      live send gate (armed check), env key loading, HMAC-SHA256 signing, Binance + Bybit testnet senders
crates/dex-evm   Uniswap + PancakeSwap quoting (exact V2 math; V3 quoter types + addresses)
crates/dex-sol   Jupiter quote URL building/parsing (mints, prices, DexQuote conversion)
crates/store     Store trait + in-memory/JSONL backend + SQL schema for the DB backend
crates/api       control state, status/metrics/flows rendering, route table, index HTML (flow map + ops panels)
bins/arb-engine  wiring, config, tick loop, localhost control API
xtask            dev tooling: doc-check | install-hooks | ablate | autoloop (cargo xtask <cmd>); post-commit hook is this binary
```

Conventions: submodules declared with `pub mod` in each crate's `lib.rs`,
shared items re-exported with `pub use`, unit tests inline in
`#[cfg(test)] mod tests`, one branch per change (never commit to `main`).
After commits, run `cargo xtask doc-check` and update the docs it names.

## Container image (CD)

The newest green `main` build publishes `ghcr.io/vernanng/createmoney:latest`
(plus a SHA tag) via the `publish` workflow, gated on `ci` and a `/health`
boot check. Paper soak from the image:

```bash
docker pull ghcr.io/vernanng/createmoney:latest
docker run --rm -p 127.0.0.1:8787:8787 \
  -v ./config.toml:/etc/arb-engine/config.toml:ro \
  ghcr.io/vernanng/createmoney:latest
```

Or build locally with `docker compose up --build` (`docker-compose.yml`).
Live trading is never auto-deployed: promote by hand, keep caps tiny.

### Autoloop (local CI/CD, paper only)

`cargo xtask autoloop` drives the self-tuning soak on this machine —
no GitHub Actions in the gate, no live mode, no keys:

- `check` — one-line verdict from `GET /status`: `SOAKING` (<24 h
  since engine start), `PROFITABLE` / `LOSING` (realized + unrealized
  vs 0 after 24 h; exit code 3 when losing).
- `ci` — the local gates: `cargo fmt --check`, `clippy -D warnings`,
  `cargo test --workspace`.
- `ship "<title>" [body]` — refuses a dirty tree or `main`, runs
  `ci`, pushes the branch, opens the PR and merges it. The owner has
  waived the review step for loop branches only; the merge gate is
  the green local CI.
- `deploy` — builds `ghcr.io/vernanng/createmoney:latest` from this
  checkout (run it at the merged commit), rotates the previous
  `cd-data/paper-trades.jsonl` to a `.bak-<ts>` file so the iteration
  starts clean, and restarts the CD stack. Watchtower is paused
  during the swap and resumed before `deploy` returns, so GHCR CD
  stays on afterwards.

The loop judges iterations on the 24 h bar above; the 7-day green
gate below still governs any live promotion — an autoloop "profit"
is not a live-trading signal.

### Local continuous deployment (paper)

On the dev Mac, Colima (`brew services start colima`) runs the latest
published image as a continuous paper soak; Watchtower polls GHCR every
5 minutes and restarts the engine on each new CI-green `main` image.
Paper trades land in `cd-data/paper-trades.jsonl`. No API keys are passed
to this stack, so it cannot arm live trading.

```bash
gh auth refresh -h github.com -s read:packages   # one-time: pull the private image
gh auth token | docker login ghcr.io -u <user> --password-stdin
docker compose -f docker-compose.cd.yml up -d    # start engine + watchtower
docker compose -f docker-compose.cd.yml ps       # status
docker compose -f docker-compose.cd.yml logs -f  # logs
docker compose -f docker-compose.cd.yml down     # stop continuous deployment
curl http://127.0.0.1:8787/health
```

## Going live (read before flipping anything)

1. Paper soak first — fills always walk real books. Green gate: 7 days,
   net positive after fees, zero stale-book fills, plus a clean kill-switch
   and pause drill. Confirm it in the JSONL (`transfer` events show each
   rebalance), and run `cargo xtask ablate` so you trust the cost models
   behind that PnL.
2. Live WS books and funding prints (Binance/Bybit/OKX) are wired; DEX
   quotes come from Jupiter HTTP (SOL pairs) and Uniswap/PancakeSwap V2
   pools via `eth_call`. Still missing: V3 quoter polling, Jupiter `/swap`
   build + Solana send, and per-venue HTTP senders beyond Binance/Bybit
   testnet. Start on testnet/demo where offered.
3. Set keys in env, set `mode = "live"`, keep caps tiny. The executor
   refuses to arm without keys; the kill-switch (`POST /kill`) blocks all
   intents. Verify Indodax fees against your account, and confirm `/status`
   shows the USDT/IDR divisor healthy before trusting Indodax books with
   real money.

Safety defaults: no floats for money; edges computed **net** of taker fees
+ slippage buffer (+ gas on DEX) at VWAP for the actual size; stale or
crossed books never trade; intents expire; per-trade and per-venue caps
accumulate; paper inventory is enforced (an intent that cannot fully fill
aborts whole); Indodax's ~51 bps fee is charged in full on every edge that
touches it.

## Current limitations (honest list)

- Live mode: real books in, paper fills out, plus Binance/Bybit testnet
  sender mirrors. OKX, Indodax, Kraken, and all DEX venues have no order
  senders yet — `mode = "live"` alone sends nothing anywhere else.
- Cross-exchange assumes pre-funded balances on both venues; real
  withdrawal/deposit rebalancing (fees, delays, minimums) is modelled in
  paper but not executed.
- S6 imbalance gate is off unless `[params] imbalance_threshold` is set;
  S7 fee table is a test-only harness, not engine-wired.
- `cex_dex` covers only SOL (Jupiter), ETH and BNB (V2 pools); V3 quoting
  is types/addresses only.

## Sources (official docs used for connector facts)

- Binance WS: https://raw.githubusercontent.com/binance/binance-spot-api-docs/master/web-socket-streams.md
- Bybit V5: https://bybit-exchange.github.io/docs/v5/ws/connect
- OKX v5: https://www.okx.com/docs-v5/en/
- Indodax: https://raw.githubusercontent.com/btcid/indodax-official-api-docs/master/Marketdata-websocket.md
- Kraken WS v2: https://docs.kraken.com/api/docs/websocket-v2 · book channel: https://docs.kraken.com/api/docs/websocket-v2/book
- Withdrawal fees (transfer schedule, checked 2026-10-29): Binance/Bybit/OKX 1 USDT on TRC20 per their official fee schedules; Indodax publishes no static figure (https://help.indodax.com/hc/en-us/articles/4416646599705-Indodax-Detailed-Fees shows fees in-app only), operator-set to 1 USDT; Kraken TRC20 is 1 USDT (min 6) per its official schedule, other networks differ. Re-verify on the withdrawal screen.
- Jupiter: https://developers.jup.ag/docs/swap-api
- Rust crates: https://docs.rs/tokio/latest/tokio/ · https://docs.rs/rust_decimal/latest/rust_decimal/ · https://alloy.rs/
