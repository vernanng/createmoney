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
- S6 imbalance gate: implemented and tested, not yet on in the engine
  binary. S7 fee table and S8 ablation harness are test-only harnesses
  (`cargo xtask ablate` runs them).
- Live WS venue tasks, control API (`/flows`, `/viewer`, pause/resume), and
  the GHCR + Watchtower paper CD. Venue order senders remain go-live work.
