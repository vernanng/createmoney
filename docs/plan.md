# Build plan (executed)

Decisions locked before coding:

- Markets: CEX↔CEX (Binance, Bybit, OKX, Indodax — no Coinbase), DEX on-chain
  (Uniswap, PancakeSwap on EVM; Jupiter on Solana), CEX↔DEX hybrid.
- Strategies: cross-exchange, triangular, funding-rate, CEX↔DEX.
- Mode: paper + live-ready (live gated by config + env keys + armed check).
- Stack: Rust workspace, tokio, rust_decimal, single `arb-engine` binary.

Build order: core → feed parsers → risk/paper → strategies → DEX quoting →
store/exec/api → binary + config + Docker/CI. See README for go-live steps.
