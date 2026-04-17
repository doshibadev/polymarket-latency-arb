# Repository Guidelines

## Project Structure & Module Organization

This is a single Rust binary crate with a React dashboard. Rust source lives in `src/`: `main.rs` wires the async runtime, `arb/` contains the trading engine, `execution/` contains paper and live wallet implementations, `polymarket/` handles market discovery and CLOB data, `rtds/` handles price streams, `server/` serves the dashboard/API, and `config/` loads environment settings. The frontend lives in `web/` and builds into `web/dist`, which Axum serves in production. Runtime state such as `paper_wallet_state.json` is local data, not source.

## Build, Test, and Development Commands

- `cargo build` builds a fast development binary.
- `cargo build --release` builds the optimized trading binary.
- `cargo run` starts the bot using `.env` settings and serves the dashboard/API.
- `cargo run --release` is the normal performance-oriented local run.
- `cargo fmt` formats Rust code.
- `cargo clippy` runs Rust lints.
- `cargo test` runs Rust tests.
- `cd web && npm install` installs frontend dependencies.
- `cd web && npm run dev` starts Vite on port `5173` with backend proxies.
- `cd web && npm run build` type-checks and builds the production dashboard into `web/dist`.

Run backend commands from the repository root so `.env`, `web/dist`, and local state paths resolve correctly.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and standard formatting: four-space indentation, `snake_case` for functions/modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer `Result<T>` from `src/error.rs` where practical and add variants to `ArbError` instead of returning loosely typed strings for shared errors. Keep async work on Tokio primitives and follow the existing channel-based design (`mpsc`, `broadcast`) for cross-task communication.

Frontend code is React + TypeScript. Keep the terminal visual language dense, dark, and operational. Avoid SaaS/dashboard decoration. Backend contract types live in `web/src/types.ts`; update them when the websocket snapshot or config surface changes.

## Testing Guidelines

Current automated coverage is small and focused on config/server invariants. Add Rust unit tests beside code under `#[cfg(test)]` for isolated calculations and integration tests under `tests/` for behavior spanning modules. Name tests after behavior, for example `exits_when_stop_loss_crossed`. Before submitting changes, run `cargo fmt`, `cargo test`, and `cd web && npm run build`. Run `cargo clippy` when touching backend logic.

## Commit & Pull Request Guidelines

Use concise, specific commit subjects that describe changed behavior. Pull requests should include a summary, live-trading risk notes if relevant, configuration changes, and validation commands run. Include screenshots when changing the React terminal UI.

## Security & Configuration Tips

Configuration is environment-driven via `.env`. Keep private keys, RPC URLs, and live-trading credentials out of git. Use `PAPER_TRADING=true` for development. Live trading requires `PAPER_TRADING=false`, `POLYMARKET_PRIVATE_KEY`, Polygon funding, and correctly configured approvals. Document any change touching order placement, approvals, close/retry behavior, or wallet state persistence.
