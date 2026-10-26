#!/usr/bin/env bash
# S8 nano-ablation harness: one-command proof each cost model matters.
# Runs SyntheticFeed(3 seeds) x strategy x {full,no-fee,no-slip,inf-age,inf-depth}
# via the maintained Rust tests, then the full workspace suite.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== S8 ablation matrix (arb-strategy ablate + cross/cex/fee/imbalance/lag/fx) =="
cargo test -p arb-strategy -- --nocapture

echo "== full workspace gate =="
cargo test --workspace
