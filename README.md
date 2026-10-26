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

## Layout

```
crates/core      types, L2 book (snapshot/diff/VWAP), decimal money math
crates/feed      Binance/Bybit/OKX/Indodax parsers, WS URLs, IDR->USD books, synthetic feed
crates/strategy  Strategy trait + cross-ex / triangular / funding / cex-dex
crates/risk      pre-trade checks, venue caps, kill-switch
crates/paper     book-walking fill simulator (fees + slippage haircut)
crates/exec      live send gate (armed check), env key loading, HMAC signing
crates/dex-evm   Uniswap + PancakeSwap quoting (V2 exact math)
crates/dex-sol   Jupiter quote building/parsing
crates/store     Store trait + in-memory/JSONL backend + SQL schema for the DB backend
crates/api       control state, status/metrics rendering, route table
bins/arb-engine  wiring, config, tick loop, localhost control API
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
