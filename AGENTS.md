# Repository Guidelines

## Project Structure & Module Organization

This is a single Rust binary crate for a Polymarket latency arbitrage bot. Source code lives in `src/`: `main.rs` wires the async runtime, `arb/` contains the trading engine, `execution/` contains paper and live wallet implementations, `polymarket/` handles market and CLOB data, `rtds/` handles price streams, `server/` serves the dashboard/API, and `config/` loads environment settings. `dashboard.html` is the single-file web dashboard served from the repo root. Runtime state such as `paper_wallet_state.json` should be treated as local data, not source. Build output is in `target/`.

## Build, Test, and Development Commands

- `cargo build` builds a fast development binary.
- `cargo build --release` builds the optimized trading binary.
- `cargo run` starts the bot using `.env` settings and serves the dashboard.
- `cargo run --release` is the normal performance-oriented local run.
- `cargo fmt` formats Rust code with standard rustfmt defaults.
- `cargo clippy` runs default Rust lints.
- `cargo test` runs unit and integration tests when present.

Run commands from the repository root so `dashboard.html` and local state paths resolve correctly.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and standard formatting: four-space indentation, `snake_case` for functions/modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer `Result<T>` from `src/error.rs` and add variants to `ArbError` instead of returning loosely typed strings. Keep async work on Tokio primitives and follow the existing channel-based design (`mpsc`, `broadcast`) for cross-task communication.

## Testing Guidelines

There are currently no dedicated test files; paper trading mode is the practical functional harness. Add unit tests beside the code under `#[cfg(test)]` for isolated calculations and integration tests under `tests/` for behavior spanning modules. Name tests after the behavior, for example `exits_when_stop_loss_crossed`. Before submitting changes, run `cargo fmt`, `cargo clippy`, and `cargo test`.

## Commit & Pull Request Guidelines

Recent history uses short imperative or descriptive messages such as `Remove dead config surface`, `Align dashboard config surface`, and `bug fixes`. Prefer concise, specific commit subjects that describe the changed behavior. Pull requests should include a summary, risk notes for live trading paths, relevant configuration changes, and validation commands run. Include screenshots only when `dashboard.html` changes.

## Security & Configuration Tips

Configuration is environment-driven via `.env`. Keep private keys, RPC URLs, and live-trading credentials out of git. Use `PAPER_TRADING=true` for development. Live trading requires `PAPER_TRADING=false`, `POLYMARKET_PRIVATE_KEY`, and Polygon funding, so document any change touching order placement, approvals, or wallet state persistence.
