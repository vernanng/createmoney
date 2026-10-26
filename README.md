# createmoney — Rust arbitrage engine

Multi-venue, multi-strategy arbitrage engine. **Paper-first**: real strategy
logic, simulated fills against live-shaped books, live order sending locked
behind `mode = "live"` + env keys + an armed check in `arb-exec`.

## Venues & strategies

| Area | Included |
|---|---|
| CEX | Binance, Bybit, OKX, Indodax (feed parsers + subscribe frames + signing) |
| DEX (EVM) | **Uniswap** V2/V3, **PancakeSwap** V2/V3 (local V2 AMM math, V3 quoter types + addresses) |
| DEX (Solana) | Jupiter Swap API (quote URL building + parsing) |
| Strategies | `cross_exchange`, `triangular`, `funding_rate`, `cex_dex` — each toggleable in config |

## Quick start

```bash
cargo test --workspace
cargo run -p arb-engine -- --config config.toml --ticks 40   # paper smoke run
cargo run -p arb-engine -- --config config.toml              # run until Ctrl-C
curl http://127.0.0.1:8787/health
curl -X POST http://127.0.0.1:8787/kill                      # trip kill-switch
```

Paper trades append to `paper-trades.jsonl`. Logs via `RUST_LOG=debug`.

## Container image (CD)

Every green `main` build publishes `ghcr.io/vernanng/createmoney:latest`
(plus a SHA tag) via the `publish` workflow, which only runs after `ci`
passes. Paper soak from the image:

```bash
docker pull ghcr.io/vernanng/createmoney:latest
docker run --rm -p 127.0.0.1:8787:8787 \
  -v ./config.toml:/etc/arb-engine/config.toml:ro \
  ghcr.io/vernanng/createmoney:latest
```

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

## Layout

```
crates/core      types, L2 book (snapshot/diff/VWAP), decimal money math
crates/feed      Binance/Bybit/OKX/Indodax parsers, WS URLs, IDR->USD books, synthetic feed
crates/strategy  Strategy trait + cross-ex / triangular / funding / cex-dex + lag-chase / fx-triangle / imbalance / ablation / fee-table
crates/risk      pre-trade checks, venue caps, kill-switch
crates/paper     book-walking fill simulator (fees + slippage haircut)
crates/exec      live send gate (armed check), env key loading, HMAC signing
crates/dex-evm   Uniswap + PancakeSwap quoting (V2 exact math)
crates/dex-sol   Jupiter quote building/parsing
crates/store     Store trait + in-memory/JSONL backend + SQL schema for the DB backend
crates/api       control state, status/metrics rendering, route table
bins/arb-engine  wiring, config, tick loop, localhost control API
xtask            dev tooling: doc-check, install-hooks, ablate (cargo xtask <cmd>); post-commit hook is this binary
```

## Config

Copy `config.example.toml` to `config.toml`. Key knobs: `min_edge_bps`,
`target_notional_quote`, per-trade/per-venue caps, DEX gas estimates,
Indodax `usd_idr` reference rate. Secrets only via env (see `.env.example`).

## Going live (read before flipping anything)

1. Paper soak first. Confirm zero stale-book trades and sane PnL in the JSONL.
2. Wire live WS tasks: `arb_feed::{ws_url, subscribe_frame}` + the parsers are
   ready; each venue task maintains an `OrderBook` and publishes into the view.
3. Wire venue HTTP senders behind `LiveExecutor::guard` (signing is done:
   `arb_exec::sign_query`). Start on testnet/demo where offered.
4. Set keys in env, set `mode = "live"`, keep caps tiny. The executor refuses
   to arm without keys; the kill-switch (`POST /kill`) blocks all intents.
5. DEX live: add an alloy provider + signer in `dex-evm` (V3 via Quoter
   `eth_call`), and Jupiter `/swap` build + Solana send in `dex-sol`.

## Safety defaults

- No floats for money (`rust_decimal` everywhere).
- Edges are computed **net** of taker fees + slippage buffer (+ gas on DEX).
- Stale or crossed books never trade. Intents expire. Venue exposure caps
  accumulate. Indodax sizes default small (thin IDR books).

## Sources (official docs used for connector facts)

- Binance WS: https://raw.githubusercontent.com/binance/binance-spot-api-docs/master/web-socket-streams.md
- Bybit V5: https://bybit-exchange.github.io/docs/v5/ws/connect
- OKX v5: https://www.okx.com/docs-v5/en/
- Indodax: https://raw.githubusercontent.com/btcid/indodax-official-api-docs/master/Marketdata-websocket.md
- Jupiter: https://developers.jup.ag/docs/swap-api
- Rust crates: https://docs.rs/tokio/latest/tokio/ · https://docs.rs/rust_decimal/latest/rust_decimal/ · https://alloy.rs/
