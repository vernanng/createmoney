# AGENTS.md

## Commands

- `cargo test --locked --workspace` — run the full test suite
- `cargo fmt --check` — run the format task
- `cargo clippy --locked --workspace --all-targets -- -D warnings` — run the lint task
- `cargo xtask ablate` — run the S8 ablation matrix, then the full suite

## Code Map

- `bins` — application source
- `crates` — crates
- `xtask` — dev tooling (`cargo xtask doc-check|install-hooks|ablate`; the post-commit hook is the `xtask` binary, no shell)
- `docs` — project documentation
- `.github` — project configuration

## Conventions

- Declare submodules with `pub mod <name>;` in each crate's `lib.rs`.
- Import shared types through workspace path crates (e.g. `use arb_core::{...};`).
- Write Rust against edition 2021.
- Place unit tests inline in `#[cfg(test)] mod tests` inside the source file.
- Re-export shared items with `pub use` in each crate's `lib.rs`.

## Branches

- Never work directly on `main`. Use one branch per change: `feat/<name>` (feature), `fix/<name>` (bug fix), `chore/<name>` (tooling/deps/CI), `docs/<name>` (docs only), `refactor/<name>` (no behavior change), `hotfix/<name>` (urgent fix toward `main`).
- Merge to `main` only after review and green CI (`cargo test --locked --workspace`, `cargo fmt --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`).

## Verification

- Verify through the interface the user touches (tests for engine logic, Ego browser for the viewer).
- If a request admits two structures, ask one question with the two options before building.

## Viewer

- `crates/api/src/viewer.html` is baked into the binary via `include_str!`. A viewer change is done only after rebuild + engine restart + Ego reload.
- After every viewer edit, verify in Ego and quote three numbers: sideways overflow (`#money` scrollWidth − clientWidth, must be 0), panel height vs viewport (must fit or scroll), all blocks reachable.

## Docs

- After every commit, review and update the affected `.md` docs (`README.md`, `docs/plan.md`) so commands, layout, and behavior stay accurate.
- Run `cargo xtask doc-check` to list which docs a commit affects; install the reminder hook once with `cargo xtask install-hooks`.
