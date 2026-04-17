# CLAUDE.md

This file provides guidance to Claude Code when working with this repository.

## Project Overview

Lattice Terminal is a Rust Polymarket trading bot with a React/TypeScript terminal dashboard. It targets BTC Up/Down 5-minute markets and monitors Binance, Polymarket RTDS/Chainlink, Gamma market data, and Polymarket CLOB liquidity. It supports paper trading through a local simulated wallet and live trading through `polymarket-client-sdk`.

## Build & Run Commands

```bash
cargo build
cargo build --release
cargo run
cargo run --release
cargo fmt
cargo clippy
cargo test
```

Frontend:

```bash
cd web
npm install
npm run dev
npm run build
```

The production dashboard is served from `web/dist` by Axum. Run `npm run build` before expecting the Rust server to serve the React terminal.

## Running Modes

- Paper trading: `PAPER_TRADING=true` in `.env`. Uses `PaperWallet`, simulated execution delay, order-book-aware fill estimation, and local SQLite state in `lattice.db`.
- Live trading: `PAPER_TRADING=false`. Requires `POLYMARKET_PRIVATE_KEY`, `POLYGON_RPC_URL`, funded Polygon wallet, and live CLOB access.
- Multiple instances: use different env files or ports, for example `.env.paper` with `DASHBOARD_PORT=3001`.

## Architecture

```text
Binance direct WS -----+
                       +-- price_tx -> ArbEngine -> broadcast_tx -> Axum /ws -> React terminal
Polymarket RTDS WS ----+       ^              |
                               |              v
Polymarket CLOB WS - clob_tx --+    PaperWallet / LiveWallet
                               ^
Gamma market refresh - market_tx

React terminal - POST /command, /settings -> cmd_rx
```

## Module Responsibilities

- `src/main.rs` wires startup, channels, market fetch, streams, server, and live/paper wallet mode.
- `src/arb/engine.rs` is the core event loop, spike detector, market state holder, command handler, and dashboard snapshot broadcaster.
- `src/execution/paper.rs` implements paper state, execution delay, fee accounting, liquidity checks, entries, exits, and persistence.
- `src/execution/live.rs` implements live balance sync, approvals, CLOB order placement, retry/close behavior, and live position tracking.
- `src/polymarket/market_data.rs` fetches active 5-minute BTC markets from Gamma.
- `src/polymarket/clob_client.rs` subscribes to CLOB market data and maintains order-book depth.
- `src/rtds/stream.rs` manages Binance and Polymarket RTDS price feeds.
- `src/server/mod.rs` serves React assets and exposes `/ws`, `/config`, `/settings`, and `/command`.
- `web/src/App.tsx` renders the terminal.
- `web/src/types.ts` defines the frontend/backend contract.

## Dashboard/API Contract

Routes:

- `GET /config`
- `POST /settings`
- `POST /command`
- `GET /ws`

Commands:

- `{ "_type": "start" }`
- `{ "_type": "stop" }`
- `{ "_type": "reset" }`
- `{ "_type": "close_position", "index": number }`

Websocket snapshots include portfolio metrics, positions, trades, equity history, signals, markets, wallet/mode/running state, runtime, and config.

## Key Strategy Concepts

- `THRESHOLD_BPS` is hundredths of USD: `2000` means a `$20` BTC spike threshold.
- `PORTFOLIO_PCT`, `MAX_DRAWDOWN_PCT`, and `EARLY_EXIT_LOSS_PCT` are backend fractions.
- The frontend displays those fractional values as percentages and serializes them back to fractions.
- Entry checks include spike magnitude, sustained spike timing, price bounds, CLOB depth, price-to-beat context, trend filter, exposure, balance, drawdown, and rate limits.
- Exit checks include trend reversal, spike fade, stop-loss, trailing stop, hold-to-resolution, early loss exit near expiry, market-end force-close, and manual close.

## Security

Never commit `.env`, private keys, RPC secrets, generated `web/dist`, `target`, or local SQLite files (`lattice.db`, `lattice.db-shm`, `lattice.db-wal`). Treat live-trading changes as high risk and explicitly document validation steps.
