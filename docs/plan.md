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
