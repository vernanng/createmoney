# createmoney — Rust arbitrage engine

Multi-venue, multi-strategy arbitrage engine in Rust (edition 2021, tokio,
`rust_decimal` — no floats for money). **Paper-first**: real strategy logic
and real order books, simulated book-walking fills. Live order
sending stays locked behind `mode = "live"` + env keys + the armed check in
`arb-exec`; even in live mode the paper executor still handles fills until
venue HTTP senders land (see [Going live](#going-live-read-before-flipping-anything)).

One binary, `arb-engine`, runs a tick loop (default 250 ms):

```
feed (live WS books; no DEX quotes or funding until real feeds land) ──▶ MarketView
  ──▶ strategies emit Intents ──▶ paper fill (VWAP + fees)
  ──▶ store (memory + JSONL) ──▶ control API (:8787)
```

Strategies never place orders. Each one reads a read-only `MarketView` and
emits `Intent`s (typed multi-leg trades with `expected_edge_bps`, `ttl_ms`,
`created_ms`, and a `roundtrip` flag); the executors own everything after
that. No pre-trade gate: every emitted intent goes to the paper fill, which
still enforces inventory; `POST /kill` stops the loop emitting until revived.

## Quick start

```bash
cargo test --locked --workspace
cargo run -p arb-engine -- --config config.toml --ticks 40   # paper smoke run
cargo run -p arb-engine -- --config config.toml              # run until Ctrl-C
curl http://127.0.0.1:8787/health
curl http://127.0.0.1:8787/status
curl -X POST http://127.0.0.1:8787/kill                      # trip kill-switch
```

Paper trades append to `paper-trades.jsonl` (configurable). Logs via
`RUST_LOG=debug`. CLI flags: `--config <path>` (default `config.toml`) and
`--ticks <n>` (default `0` = run until Ctrl-C).

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
| Binance | CEX | spot books, perp funding, lag leader | 10 bps (0.10%) |
| Bybit | CEX | spot books, perp funding | 10 bps |
| OKX | CEX | spot books, perp funding | 10 bps |
| Indodax | CEX (IDR) | IDR books normalized via live USDT/IDR rate (own book, cross-checked; `[indodax] usd_idr` is only a fallback reference for the strategy's own math) | 51 bps (~0.51% incl. tax) |
| Uniswap | DEX (EVM) | V2 pools quoted locally; V3 quoter types/addresses | 30 bps V2 (0.30%) |
| PancakeSwap | DEX (EVM) | V2 pools quoted locally; V3 quoter types/addresses | 25 bps V2 (0.25%) |
| Jupiter | DEX (Solana) | Swap API quote URL building + parsing | 0 bps (inside the route) |

Fees are `Venue::default_taker_bps` in `crates/core` — defaults to verify
against your account/pair, not account truth. Indodax's 51 bps is why most
thin IDR spreads correctly never fire.

Default symbols (`config.toml`): `BTC/USDT`, `ETH/USDT`, `SOL/USDT`,
`XRP/USDT`, `DOGE/USDT`. The engine always also builds `ETH/BTC` books so
the triangular cycles can price.

**Feeds** (`crates/feed`): parsers for Binance depth/partial/bookTicker,
Bybit orderbook, OKX books, and Indodax depth, plus `ws_url` /
`subscribe_frame` helpers. The binary spawns one
reconnecting WebSocket task per CEX venue (`spawn_venue_tasks`, 1 s → 30 s
backoff); fresh live books win and stale venues are skipped — never
backfilled. Indodax IDR books are converted
with `idr_book_to_usd` at the streamed live rate.

## Strategies

Six strategies implement the `Strategy` trait and run in the engine. Three
more modules (`imbalance` S6, `fee_table` S7, `ablate` S8) are a filter, a
threshold optimizer, and the cost-model falsifier — documented below the
strategies because they modify or validate them rather than emitting
intents of their own.

| # | Strategy (`name()`) | File | Idea in one line | Legs | Round-trip? | Engine wiring |
|---|---|---|---|---|---|---|
| 1 | `cross_exchange` | `cross_exchange.rs` | Buy cheap venue, sell rich venue (S2 adaptive sizing) | 2 spot | yes | `[strategies] cross_exchange` |
| 2 | `triangular` | `triangular.rs` | One-venue A→B→C→A cycle ends with more than it started | 3 spot | yes | `[strategies] triangular` |
| 3 | `funding_rate` | `funding.rs` | Long spot + short perp to collect positive funding (S4 exit) | 1 spot + 1 perp | **no** (opens a position) | `[strategies] funding_rate` |
| 4 | `cex_dex` | `cex_dex.rs` | CEX book vs DEX quote, net of gas (S5 size sweep) | 1 CEX spot + 1 DEX | yes | `[strategies] cex_dex` |
| 5 | `lag_chase` (S1) | `lag_chase.rs` | Leader jumps, thin follower hasn't moved yet — take the stale side | 2 spot | yes | auto: needs Binance + Indodax (see below) |
| 6 | `fx_triangle` (S3) | `fx_triangle.rs` | IDR book mispriced vs USDT book around the USD/IDR reference | 2 spot | yes | auto: needs Binance + Indodax (see below) |

### Shared contract

All strategies share `StrategyParams` (from `[params]` in config):

| Param | Default | Meaning |
|---|---|---|
| `min_edge_bps` | 15 | Minimum **net** edge (after fees + slippage buffer) to emit |
| `slip_buffer_bps` | 5 | Extra bps subtracted from every edge before comparing |
| `target_notional_quote` | 500 | Base order size in quote (USD/USDT) terms |
| `max_venue_quote_share_bps` | 2500 | One intent may spend at most this share (bps) of a venue's USDT+USDC ledger; 0 = off |
| `dex_max_quote_age_ms` | 30000 | Polled DEX quotes older than this never reach `cex_dex` (pollers run every ~10 s) |
| `max_book_age_ms` | 2000 | Books older than this, or crossed, never trade (`fresh()`) |
| `ttl_ms` | 1500 | Intent lifetime |

Net edge everywhere is `net_edge_bps` from `crates/core`:
`gross_bps − buy_fee − sell_fee − slip_buffer`. Prices come from book
**VWAP over the actual quantity**, not the touch, so a spread that only
exists for dust does not fire. A venue with a missing, empty, stale, or
crossed book is skipped — it never aborts the whole scan.

Pause any strategy at runtime without a restart:
`POST /pause/<name>` / `POST /resume/<name>` using the `name()` values above
(e.g. `POST /pause/triangular`).

### 1. `cross_exchange` — buy cheap, sell rich (includes S2)

Classic spot arbitrage across CEXs: buy on the venue with the lowest VWAP,
sell on the venue with the highest VWAP, same symbol, same quantity.
Assumes pre-funded inventory on every venue — no transfers inside the loop.

- **S2 dynamic sizing**: no fixed size or grid. For each venue pair it
  walks both books level by level and keeps the largest quantity whose
  combined VWAP still clears `min_edge_bps` net of fees — size follows the
  opportunity. Deep books fill big, thin books fill small; anything the
  book cannot fill never emits. This replaces `target_notional_quote`
  for this strategy.
- Buy and sell venues must differ. Edge is net of both venues' taker fees
  plus `slip_buffer_bps`.
- Optional **S6 imbalance gate**: also require the
  buy-side book to be bid-heavy and the sell-side book ask-heavy (see
  [S6](#s6-imbalance--a-gate-on-cross_exchange-not-a-strategy)). Off unless
  `[params] imbalance_threshold` is set.
- Emits at most one intent per symbol per tick. `roundtrip = true`.

### 2. `triangular` — one-venue cycles

On a single venue, run A→B→C→A and keep the difference. Default cycles
(`Triangular::default_cycles`, both directions over BTC/ETH/USDT):

- USDT → BTC → ETH → USDT: buy `BTC/USDT`, buy `ETH/BTC`, sell `ETH/USDT`
- USDT → ETH → BTC → USDT: buy `ETH/USDT`, sell `ETH/BTC`, sell `BTC/USDT`

The whole chain is priced off the books before emitting: each step converts
the running amount at that step's VWAP and deducts the venue taker fee, so
the signal is the end/start ratio. Fires only when
`ratio ≥ 1 + (min_edge_bps + slip_buffer_bps) / 10_000`; reported edge is
`(ratio − 1) × 10_000` bps. Sizes from `target_notional_quote` as the
starting USDT amount. Any stale/crossed/short book in the chain kills that
cycle only. `roundtrip = true`. Cycles are data (`Cycle { steps }`), so
adding a triangle is a config-shaped change, not new logic.

### 3. `funding_rate` — collect perp funding (includes S4)

When perpetual funding is positive, longs pay shorts. Entry: **long spot +
short perp on the same venue** (Binance/Bybit/OKX only), delta-neutral,
collecting funding each interval. Legs carry explicit `MarketKind::Spot` /
`MarketKind::Perp` — executors never guess market type from the symbol.

- Fires when annualized funding (`rate × intervals/year × 10_000` bps)
  clears `min_funding_annualized_bps` (default 500) **plus** entry cost
  (2 × taker fee + slippage buffer). Reported edge is annualized funding
  net of entry cost. Sized from `target_notional_quote` at the spot ask.
- `roundtrip = false`: this opens a position. Paper PnL is *not* booked at
  fill; funding accrues over time (`accrued(notional, rate, hours)`).
- **Fire-once**: an open (venue, symbol) pair never re-fires until closed
  (`mark_closed`), so the strategy cannot churn every tick. This holds
  across restarts and redeploys: filled entries reload from the JSONL
  store at startup and re-seed the open set.
- **S4 exit timing** (`with_exit(window, threshold)`, defaults: last 5
  prints vs `min_funding_annualized_bps`): the strategy records annualized
  prints per pair; when the window mean drops below the exit threshold
  (funding flipped cold), the pair is marked closed and may re-enter later.
- Feeds are live: Binance (`markPrice@1s`), Bybit linear `tickers`, and
  OKX `funding-rate` stream into the tick view (`crates/feed/src/funding.rs`).
  Prints older than `max_book_age_ms` never reach the strategy, so a stalled
  stream reads as quiet, not stale.

### 4. `cex_dex` — CEX book vs DEX quote (includes S5)

Trades the spread between a CEX order book and a DEX quote in both
directions: buy CEX / sell DEX, or buy DEX / sell CEX. The DEX side arrives
as a `DexQuote` (effective buy/sell price already net of pool fee and price
impact, plus a gas estimate in quote terms); gas is charged here, converted
to bps of the attempted notional — so $50 of L1 gas on a $1000 trade (500
bps) correctly kills a 100 bps spread, while $0.50 of L2 gas (5 bps) lets
it fire.

- **S5 multi-size sweep**: tries 0.1× / 0.5× / 1.0× of
  `target_notional_quote` and keeps the best net edge including per-size
  gas. Small sizes win where AMM/book impact would eat the spread at full
  size; gas bps grows as size shrinks, so the sweep finds the hump.
- Only the CEX leg pays a taker fee in the edge math (the DEX fee is inside
  the quoted price). Stale DEX quotes and stale/crossed CEX books are
  skipped per venue — a missing book on one venue never hides the others.
- TTL is 4× the shared `ttl_ms` (DEX settlement is slower than a CEX fill).
  `roundtrip = true`.
- Quote sources polled live: Jupiter HTTP for SOL/USDT and SOL/USDC
  (both directions, `crates/dex-sol`), and one Uniswap V2 pool (ETH/USDT)
  via `eth_call getReserves` (`crates/dex-evm`) — off until `[dex]
  evm_rpc_url` and `evm_pool` are set. V2 quotes price the full
  `target_notional_quote` so smaller sweeps see a conservative price.
  Quotes older than `dex_max_quote_age_ms` never reach the strategy.

### 5. `lag_chase` — S1 lead-lag stale-book take (Binance → Indodax)

Hypothesis: thin IDR books lag the global leader by 100–500 ms after jumps.
When the Binance mid moves more than `jump_bps` in one tick and Indodax has
not followed, take the stale side on Indodax (buy its cheap asks after an
up-jump, sell into its rich bids after a down-jump) hedged with the opposite
leg on Binance.

- First sighting of a symbol only primes the previous-mid tracker; it never
  trades blind. Size is `target_notional_quote` capped by the follower's
  near-touch depth (`depth_within_bps(side, 10)`), then VWAP-checked, then
  net-edge-checked with both venues' real fees — Indodax's 51 bps blocks
  most marginal chases, by design.
- Engine wiring: no separate config flag. It is added automatically when
  `cross_exchange = true` and both Binance and Indodax are in `venues`,
  with a conservative 30 bps jump threshold so paper stays quiet
  until a real dislocation prints. `roundtrip = true`.
- Falsifier (in tests): shuffling timestamps / staleness must kill the
  edge — stale books never fire.

### 6. `fx_triangle` — S3 IDR-FX triangle (Binance ↔ Indodax)

Exploits mispricing between Indodax's IDR books (normalized to USDT at
the live USDT/IDR rate) and Binance's USDT books. Checks both directions
(buy Binance / sell Indodax and the reverse) at VWAP for
`target_notional_quote`, net of the real 10 bps vs 51 bps fee asymmetry.

- The catch this strategy exists to respect: a stale USD/IDR reference
  manufactures phantom spreads far larger than any real venue edge (a
  16000 vs 19000 rate error moves the USD print by >1000 bps; the old
  static 19000 did exactly this against an ~18100 market). In live mode
  the divisor is therefore streamed, never configured: Indodax's own
  USDT/IDR order-book midpoint, validated per tick against independent
  USDT/IDR sources — the Upbit Indonesia book plus the crosses implied
  by fresh Binance `X/USDT` vs Indodax `X/IDR` books (`crates/feed/src/fx.rs`).
  If the primary book is stale/crossed/wide, or deviates from the
  independent consensus by more than `[indodax] rate_max_deviation_bps`
  (default 300), Indodax books are withheld entirely — there is no
  fallback rate. Cash USD/IDR (Google, ECB reference) is never used: it
  prices bank dollars, not USDT. `[indodax] usd_idr` is only a fallback
  reference for the strategy's own math. Rate state is visible at `/status` (`usdt_idr`)
  and as `arb_usdt_idr_*` metrics, and the strategy records the divisor
  it actually saw (`usd_idr_used`).
- Engine wiring: same auto-enable rule as `lag_chase` (`cross_exchange`
  on + Binance and Indodax present). `roundtrip = true`.

### S6 `imbalance` — a gate on `cross_exchange`, not a strategy

Top-of-book imbalance filter (`crates/strategy/src/imbalance.rs`, no
intents of its own): `imbalance = (bid_qty − ask_qty) / (bid_qty +
ask_qty)` in [−1, 1]. `both_sided_agrees(buy, sell, t)` requires the cheap
side to be bid-heavy (`> t`, about to rise) **and** the rich side ask-heavy
(`< −t`, about to fall) — only take spreads both books agree will converge.
Set `[params] imbalance_threshold` (e.g. 0.2) to enable it in the
engine; unset means off (legacy behavior). The strategy-level switch stays
`CrossExchange::with_imbalance(threshold)`.

### S7 `fee_table` — per-route thresholds (library, not engine-wired)

`FeeTable` learns a per-(buy venue, sell venue, symbol) minimum-edge
threshold from realized paper PnL, because one global 15 bps bar is wrong
for both 10 bps and 51 bps venues. A losing round-trip raises that route's
bar (EWMA, α = 0.3, toward `global_min − pnl_bps`); winners decay back to
the global floor, never below it. Test-only harness in
`crates/strategy/src/fee_table.rs` (not compiled into the binary);
wire `threshold_for` / `record` into the tick loop before relying on it.

### S8 `ablate` — proving the cost models matter

Deterministic ablation matrix (`Ablation::matrix()`): `full`, `no-fee`,
`no-slip`, `inf-age` (stale books allowed), `inf-depth` (touch-price
cheat). The claim under test: stripping any cost model inflates PnL —
full-cost results must be worse than cost-free ones, and allowing stale
books must hurt. Run it with `cargo xtask ablate` (runs the `arb-strategy`
suite verbosely, then the full workspace suite). If an ablation ever stops
mattering, the corresponding cost model — or the test — is broken.

## Paper execution and the live gate

- **No pre-trade gate**: every emitted intent goes straight to the paper
  executor. Strategies still self-gate on net edge (`min_edge_bps`); the
  paper fill enforces inventory (unfunded buys and sells of unheld assets
  abort the whole intent).
- **Paper fills** (`crates/paper`): fills walk the book (VWAP) and pay real
  taker fees plus a 2 bps extra-slippage haircut by default, against
  simulated balances seeded from `[paper]`. Inventory is enforced: an
  unfunded buy or a sell of an unheld asset aborts the **whole** intent
  with no partial mutation. Realized PnL is computed only for round-trip
  intents; funding entries accrue over time instead.
- **Live gate** (`crates/exec`): `LiveExecutor::guard` refuses to arm
  unless mode is `live`, venue keys are present in env, and the executor
  was constructed armed. Binance spot and Bybit V5 spot testnet senders
  are wired + unit-tested (fail-closed to testnet hosts); other venues
  are the remaining go-live work.
- **Kill-switch**: `POST /kill` stops the tick loop emitting new intents;
  `POST /revive` clears it.

## Control API

Localhost HTTP on `api_addr` (default `127.0.0.1:8787`), served by the
binary from the tested route table in `crates/api`. Reads are open;
if `API_TOKEN` is set in env, the POST rows below need `?token=<value>`
(e.g. `POST /kill?token=$API_TOKEN`) and return 401 without it:

| Route | Method | Effect |
|---|---|---|
| `/health` | GET | `{"ok":true}` |
| `/status` | GET | mode, killed, engine start (`started_ms`, viewer shows uptime), trades, realized PnL, opportunities, per-venue connector health (socket state reported separately from fresh-book counts), paused strategies, portfolio (per-venue balances, equity, realized/unrealized PnL) |
| `/metrics` | GET | text metrics incl. per-strategy funnel counters (scans, intents, fill failures, `dup_skipped` same-top refills, trades, PnL; `risk_rejected` stays 0 — the pre-trade gate is removed) and per-venue book freshness |
| `/flows` | GET | JSON snapshot of the last 200 trades (for the viewer) |
| `/viewer` | GET | built-in HTML trade-flow viewer |
| `/kill` | POST | trip the kill-switch |
| `/revive` | POST | clear the kill-switch |
| `/pause/<strategy>` | POST | pause one strategy by `name()` |
| `/resume/<strategy>` | POST | resume it |

## Config

Copy `config.example.toml` to `config.toml` (a working paper default is
committed). Secrets only via env (see `.env.example`) — never in config.
`arb-engine` auto-loads `./.env` at startup (real env wins), so plain
`cargo run -p arb-engine` picks up keys with no wrapper.
The live Indodax feed additionally needs `INDODAX_WS_TOKEN`, the public
static market-data token from Indodax's WebSocket docs; without it that
venue is disabled at startup with one error instead of reconnecting.
Top-level `venues` / `symbols` must stay above the first `[table]` header.

| Key | Default (committed config) | Meaning |
|---|---|---|
| `venues` | binance, bybit, okx, indodax | CEX venues to run |
| `symbols` | BTC, ETH, SOL, XRP, DOGE, BNB, ADA, LINK, TRX, LTC, DOT, AVAX, UNI, ATOM, NEAR, XLM, ETC, ALGO, FIL, SUI `/USDT` | traded pairs |
| `[engine] mode` | `paper` | `paper` \| `live` (live still needs env keys + armed check) |
| `[engine] tick_ms` | 250 | tick-loop period |
| `[engine] api_addr` | `127.0.0.1:8787` | control-API bind; omit to disable |
| `[engine] jsonl_path` | `paper-trades.jsonl` | paper-trade log; each start appends a `session_start` record, trades carry `session_id` + `engine_version`, and history reloads at startup |
| `[strategies]` | all `true` | toggles for `cross_exchange`, `triangular`, `funding_rate`, `cex_dex` (`lag_chase`/`fx_triangle` ride on `cross_exchange`) |
| `[params]` | see [Shared contract](#shared-contract) | `min_edge_bps`, `slip_buffer_bps`, `target_notional_quote`, `max_book_age_ms`, `ttl_ms` |
| `[params] min_funding_annualized_bps` | 500 | funding-rate entry bar |
| `[params] imbalance_threshold` | unset (off) | S6 gate: both-sided imbalance bar, e.g. 0.2 |
| `[indodax] usd_idr` | 19000 | fallback reference for the strategy's own math; book conversion always uses the streamed live rate |
| `[indodax] rate_max_age_ms` | 30000 | max age of the live USDT/IDR book used as divisor |
| `[indodax] rate_max_spread_bps` | 50 | max USDT/IDR top-of-book spread (bps of mid) |
| `[indodax] rate_max_deviation_bps` | 300 | max deviation vs independent consensus before Indodax is withheld |
| `[paper] start_quote_per_venue` | 200 | seeded USDT/USDC per venue incl. DEX wallets. Base is seeded only on Binance (BTC/ETH/SOL, one fill each at the reference price) so the portfolio fits the starting capital. `[paper.capital] <venue>` overrides one venue, e.g. `binance = 10000` |

Before going live, set `[paper]` amounts to your real planned capital per
venue so the paper soak behaves like the live book.

## Layout

```
crates/core      types (Venue/Symbol/Intent), L2 book (snapshot/diff/VWAP/depth), decimal money math
crates/feed      Binance/Bybit/OKX/Indodax parsers, WS URLs/frames, live WS tasks, IDR->USD books (+ deterministic SyntheticFeed kept for the S8 harness)
crates/strategy  Strategy trait + MarketView; all six strategies + imbalance filter (S6); fee_table (S7) / ablate (S8) test harnesses
crates/paper     book-walking fill simulator (fees + slippage haircut, inventory-enforced)
crates/exec      live send gate (armed check), env key loading, HMAC-SHA256 signing, Binance + Bybit testnet senders
crates/dex-evm   Uniswap + PancakeSwap quoting (exact V2 math; V3 quoter types + addresses)
crates/dex-sol   Jupiter quote URL building/parsing (mints, prices, DexQuote conversion)
crates/store     Store trait + in-memory/JSONL backend + SQL schema for the DB backend
crates/api       control state, status/metrics/flows rendering, route table, viewer HTML
bins/arb-engine  wiring, config, tick loop, localhost control API
xtask            dev tooling: doc-check | install-hooks | ablate (cargo xtask <cmd>); post-commit hook is this binary
```

Conventions: submodules declared with `pub mod` in each crate's `lib.rs`,
shared items re-exported with `pub use`, unit tests inline in
`#[cfg(test)] mod tests`, one branch per change (never commit to `main`).
After commits, run `cargo xtask doc-check` and update the docs it names.

## Container image (CD)

The newest green `main` build publishes `ghcr.io/vernanng/createmoney:latest`
(plus a SHA tag) via the `publish` workflow, which only runs after `ci`
passes. Publish runs share one concurrency group, so a superseded in-progress
build is cancelled instead of letting an older commit move `latest` backward.
Before pushing, publish boots the `amd64` image and gates on `/health`.
Paper soak from the image:

```bash
docker pull ghcr.io/vernanng/createmoney:latest
docker run --rm -p 127.0.0.1:8787:8787 \
  -v ./config.toml:/etc/arb-engine/config.toml:ro \
  ghcr.io/vernanng/createmoney:latest
```

Or build locally with `docker compose up --build` (`docker-compose.yml`).
Live trading is never auto-deployed: promote by hand, keep caps tiny.

### Local continuous deployment (paper)

On the dev Mac, Colima (auto-started at login via `brew services start
colima`) runs the latest published image as a continuous paper soak.
Watchtower polls GHCR every 5 minutes and restarts the engine on each new
CI-green `main` image. Paper trades land in `cd-data/paper-trades.jsonl`.

```bash
gh auth refresh -h github.com -s read:packages   # one-time: pull the private image
gh auth token | docker login ghcr.io -u <user> --password-stdin
docker compose -f docker-compose.cd.yml up -d    # start engine + watchtower
docker compose -f docker-compose.cd.yml ps       # status
docker compose -f docker-compose.cd.yml logs -f  # logs
docker compose -f docker-compose.cd.yml down     # stop continuous deployment
curl http://127.0.0.1:8787/health
```

No API keys are passed to this stack, so it cannot arm live trading; it
exists to keep the paper soak current with `main`. The publish workflow
builds `linux/amd64` and `linux/arm64` — Watchtower needs a host-arch
manifest to auto-update on Apple Silicon.

## Going live (read before flipping anything)

1. Paper soak first — fills always walk real books. Green gate: 7 days,
   net positive after fees, zero stale-book fills, plus a clean kill-switch
   and pause drill. Confirm it in the JSONL (`transfer` events show each
   rebalance), and run `cargo xtask ablate` so you trust the cost models
   behind that PnL.
2. Live WS books are already wired (`arb_feed::spawn_venue_tasks`);
   Binance spot and Bybit V5 spot testnet senders sit behind
   `LiveExecutor::guard`. Remaining venues still need per-venue HTTP
   senders. Start on testnet/demo where offered.
3. Live quote sources: funding prints stream for `funding_rate`
   (Binance/Bybit/OKX), Jupiter HTTP quotes cover SOL pairs, and one
   Uniswap V2 pool (ETH/USDT) polls via `eth_call getReserves` once
   `[dex] evm_rpc_url` and `evm_pool` are set. Still to go: V3 quoter,
   Jupiter `/swap` build + Solana send, and the remaining venue senders.
   Chain reads/sends use HTTP RPC endpoints
   (`EVM_RPC_URL`/`SOLANA_RPC_URL`).
4. Set keys in env, set `mode = "live"`, keep caps tiny. The executor
   refuses to arm without keys; the kill-switch (`POST /kill`) blocks all
   intents. Verify Indodax fees, and confirm `/status` shows
   `usdt_idr.state = "ok"`, before enabling `lag_chase` / `fx_triangle`
   with real money.

## Safety defaults

- No floats for money (`rust_decimal` everywhere).
- Edges are computed **net** of taker fees + slippage buffer (+ gas on DEX),
  at VWAP for the actual size — never at the touch.
- Stale or crossed books never trade. Intents expire. Per-trade and
  per-venue caps accumulate; funding positions hold exposure until unwound.
- Paper inventory is enforced; an intent that cannot fully fill aborts whole.
- Indodax sizes default small (thin IDR books) and its ~51 bps fee is
  charged in full in every edge that touches it.

## Current limitations (honest list)

- Live mode: real books in, paper fills out + Binance/Bybit testnet mirrors — other venues have no order senders yet.
- `funding_rate` has no live data source yet: it receives no
  funding prints and cannot fire.
- `cex_dex` has no live quote source yet: the engine carries no DEX
  quotes, so it cannot fire either.
- S6 imbalance gate is wired behind `[params] imbalance_threshold`
  (off unless set); S7 fee table is a test-only harness, not engine-wired.
- Cross-exchange assumes pre-funded balances on both venues; rebalancing
  and transfer costs are out of loop and unmodeled.

## Sources (official docs used for connector facts)

- Binance WS: https://raw.githubusercontent.com/binance/binance-spot-api-docs/master/web-socket-streams.md
- Bybit V5: https://bybit-exchange.github.io/docs/v5/ws/connect
- OKX v5: https://www.okx.com/docs-v5/en/
- Indodax: https://raw.githubusercontent.com/btcid/indodax-official-api-docs/master/Marketdata-websocket.md
- Jupiter: https://developers.jup.ag/docs/swap-api
- Rust crates: https://docs.rs/tokio/latest/tokio/ · https://docs.rs/rust_decimal/latest/rust_decimal/ · https://alloy.rs/

