# AGENTS.md

## Commands

- `cargo test --workspace` — run the full test suite
- `cargo fmt --check` — run the format task
- `cargo clippy --workspace --all-targets -- -D warnings` — run the lint task

## Code Map

- `bins` — application source
- `crates` — crates
- `docs` — project documentation
- `.github` — project configuration

## Conventions

- Declare submodules with `pub mod <name>;` in each crate's `lib.rs`.
- Import shared types through workspace path crates (e.g. `use arb_core::{...};`).
- Write Rust against edition 2021.
- Place unit tests inline in `#[cfg(test)] mod tests` inside the source file.

## Docs

- After every commit, review and update the affected `.md` docs (`README.md`, `docs/plan.md`) so commands, layout, and behavior stay accurate.
- Run `scripts/doc-check.sh` to list which docs a commit affects; install the reminder hook once with `scripts/install-hooks.sh`.
