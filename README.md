<p align="center">
  <img src="assets/lattice-mark.svg" alt="Lattice Terminal mark" width="88" height="88" />
</p>

<h1 align="center">Lattice Terminal</h1>

<p align="center">
  <strong>A Rust-powered Polymarket trading terminal for latency-sensitive BTC Up/Down markets.</strong>
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/Rust-2021-f97316?style=flat-square&logo=rust&logoColor=white"></a>
  <a href="https://tokio.rs/"><img alt="Tokio" src="https://img.shields.io/badge/Runtime-Tokio-2563eb?style=flat-square"></a>
  <a href="https://react.dev/"><img alt="React" src="https://img.shields.io/badge/Frontend-React%20%2B%20TypeScript-38bdf8?style=flat-square&logo=react&logoColor=white"></a>
  <a href="https://vite.dev/"><img alt="Vite" src="https://img.shields.io/badge/Build-Vite-646cff?style=flat-square&logo=vite&logoColor=white"></a>
  <img alt="Mode" src="https://img.shields.io/badge/Paper%20Trading-supported-10b981?style=flat-square">
  <img alt="Live Trading" src="https://img.shields.io/badge/Live%20Trading-high%20risk-f43f5e?style=flat-square">
</p>

---

## What It Is

Lattice Terminal is a single-binary Rust trading bot with a professional React dashboard. It watches BTC price movement, active Polymarket 5-minute Up/Down markets, CLOB liquidity, and Chainlink reference data, then manages paper or live positions through a dense terminal-style dashboard.

The strategy is built around latency-sensitive market movement:

1. Binance BTC updates arrive quickly through a direct websocket.
2. Polymarket/Chainlink market context updates on its own cadence.
3. The engine detects short-horizon BTC spikes against fast baselines.
4. The bot evaluates CLOB liquidity, share price, price-to-beat context, trend filters, and risk limits.
5. It enters UP or DOWN exposure in paper or live mode, then exits through layered risk logic.

This is trading infrastructure, not a SaaS demo. The dashboard is meant to stay open while monitoring automated execution.

## Current Capabilities

- Direct Binance BTC websocket feed for low-latency price movement.
- Polymarket RTDS websocket feed for Chainlink BTC reference prices.
- Polymarket Gamma market discovery for active 5-minute BTC Up/Down markets.
- Polymarket CLOB websocket subscription with bid/ask and order-book depth cache.
- Paper wallet with persistent local state, simulated execution delay, fee model, and order-book-aware fill estimation.
- Live wallet path using `polymarket-client-sdk`, Polygon approvals, balance sync, order placement, and close/retry logic.
- Shared entry planner and shared position model so paper and live use the same strategy decisions.
- Axum dashboard server with `/ws`, `/config`, `/settings`, and `/command`.
- React + TypeScript terminal frontend served from `web/dist`.
- Runtime settings editor that persists supported config fields into `.env`.
- Start, stop, reset, and manual close controls.
- Equity chart, positions table, execution blotter, signal terminal, active market panel, market scanner, and PnL metrics.

## Safety Notice

Live trading can lose real money. The live path places real Polymarket CLOB orders and requires private-key access. Use paper mode first, verify behavior under live market conditions, and keep position sizes small.

Important operational constraints:

- `PAPER_TRADING=true` is the default and should be used for development.
- `PAPER_TRADING=false` enables real execution.
- `POLYMARKET_PRIVATE_KEY` must never be committed.
- Paper state is stored locally in `lattice.db` using SQLite.
- Live state is stored separately in `lattice-live.db` using SQLite.
- Dashboard settings write to `.env`.
- The bot currently targets BTC 5-minute Up/Down markets.

## Architecture

```text
Direct Binance WS ------+
                        +-- price updates --+
Polymarket RTDS WS -----+                   |
                                           v
Polymarket Gamma API -- market windows -> ArbEngine -- snapshots -> Axum /ws -> React terminal
                                           ^
Polymarket CLOB WS --- bid/ask + depth ----+
                                           |
                                           +-- PaperWallet
                                           +-- LiveWallet

React terminal -- POST /command -- start | stop | reset | close_position
React terminal -- POST /settings - persisted .env updates
```

## Repository Layout

```text
.
+-- src/
|   +-- main.rs                  # Tokio wiring, channels, startup, live/paper selection
|   +-- arb/engine.rs            # Core event loop, signal logic, state broadcast, orchestration
|   +-- config/mod.rs            # Env config loading and dashboard config updates
|   +-- execution/model.rs       # Shared position, trade, and pending-entry domain types
|   +-- execution/planner.rs     # Shared entry sizing, liquidity, PTB, and risk gating
|   +-- execution/exit.rs        # Shared hold/exit strategy planner for paper and live
|   +-- execution/shared.rs      # Shared quote/price/trailing helpers
|   +-- execution/paper.rs       # Paper wallet, simulated fills, exits, persistence
|   +-- execution/live.rs        # Live CLOB wallet, approvals, order placement, sync, closes
|   +-- polymarket/
|   |   +-- market_data.rs       # Gamma active market discovery
|   |   +-- clob_client.rs       # CLOB websocket, token mapping, book cache
|   +-- rtds/stream.rs           # Binance + Polymarket RTDS price feeds
|   +-- server/mod.rs            # Axum static server, websocket, REST commands/settings
|   +-- error.rs                 # Shared error type
+-- web/
|   +-- src/App.tsx              # Terminal dashboard UI
|   +-- src/types.ts             # Backend contract types
|   +-- src/api.ts               # REST client and config serialization
|   +-- src/useTerminalFeed.ts   # Websocket client with reconnect
|   +-- src/styles.css           # Trading-terminal visual system
+-- assets/lattice-mark.svg      # README/dashboard-style brand mark
+-- .env.example                 # Full config reference
+-- .env.paper                   # Paper-mode starter config
+-- Cargo.toml                   # Rust crate: lattice-terminal
+-- README.md
```

## Prerequisites

- Rust stable toolchain.
- Node.js 18+ and npm for frontend development/builds.
- Network access to Binance, Polymarket RTDS, Polymarket Gamma, and Polymarket CLOB.
- For live mode only: Polygon wallet funding, USDC, Polymarket account/private key, and RPC access.

## Quick Start: Paper Trading

1. Install Rust if needed.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

2. Install frontend dependencies.

```bash
cd web
npm install
npm run build
cd ..
```

3. Create local config.

```bash
cp .env.example .env
```

4. Confirm paper mode in `.env`.

```bash
PAPER_TRADING=true
DASHBOARD_PORT=3000
```

5. Start the bot.

```bash
cargo run --release
```

6. Open the terminal.

```text
http://localhost:3000
```

The bot starts paused. Use the dashboard `START` control after confirming the market feed, mode, wallet, risk settings, and paper balance.

## Frontend Development

The production dashboard is served by Axum from `web/dist`. For frontend work, run the Rust server and Vite dev server separately:

```bash
cargo run
```

```bash
cd web
npm install
npm run dev
```

Vite runs on `http://localhost:5173` and proxies:

- `/ws` to `ws://127.0.0.1:3000`
- `/config` to `http://127.0.0.1:3000`
- `/settings` to `http://127.0.0.1:3000`
- `/command` to `http://127.0.0.1:3000`

Build the frontend before using the Rust server as the only web server:

```bash
cd web
npm run build
```

If `web/dist/index.html` is missing, the Rust server serves a setup page instead of the terminal.

## Live Trading

Live mode uses real funds and real Polymarket orders.

1. Build and test in paper mode first.
2. Add secrets to `.env`.

```bash
PAPER_TRADING=false
POLYMARKET_PRIVATE_KEY=...
WALLET_TYPE=gnosis
POLYGON_RPC_URL=https://rpc.ankr.com/polygon
```

3. Make sure the wallet has Polygon gas and USDC.
4. Start with small limits and conservative risk settings.
5. Run the release binary.

```bash
cargo run --release
```

The live wallet initializes CLOB clients, checks balance, ensures approvals, places orders, syncs live state, and mirrors paper exit decisions into live close actions. Treat live mode as high risk until observed over multiple sessions.

## Dashboard

The terminal is intentionally dense and dark. It focuses on operational trading state:

- Feed, runtime, mode, wallet, exposure, open positions, and last update.
- Portfolio value, wallet balance, total/daily/cumulative/realized/unrealized PnL.
- Equity/history chart with line and candle modes.
- Active market detail with Binance, Chainlink, price-to-beat, UP/DOWN mids, spike delta, and decision context.
- Market scanner for every backend market in the snapshot.
- Open positions with side, shares, entry/current price, PnL, held time, flags, and close action.
- Trade stats, fee drag, execution log, recent settled trades.
- Signal terminal for executed/rejected decisions and reasons.
- Settings modal grouped by entry, risk, hold behavior, trend filter, and execution controls.

## API Contract

The React app talks to the Rust server through:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/config` | Read current persisted settings from `.env`. |
| `POST` | `/settings` | Persist config updates and send them to the running engine. |
| `POST` | `/command` | Send `start`, `stop`, `reset`, or `close_position`. |
| `GET` | `/ws` | Realtime JSON snapshot stream for the dashboard. |

Commands:

```json
{ "_type": "start" }
{ "_type": "stop" }
{ "_type": "reset" }
{ "_type": "close_position", "index": 0 }
```

Snapshot groups:

- Portfolio: `balance`, `starting_balance`, `total_portfolio_value`, `unrealized_pnl`, `realized_pnl`, `total_pnl`, `cumulative_pnl`, `daily_pnl`.
- Trading stats: `wins`, `losses`, `total_fees`, `total_volume`.
- State: `running`, `runtime_ms`, `is_live`, `wallet_address`.
- Collections: `positions`, `trades`, `history`, `signals`, `markets`.
- Config: all dashboard-editable strategy/risk settings.

## Configuration Reference

Full examples live in `.env.example` and `.env.paper`. The most important settings are:

| Variable | Meaning |
|---|---|
| `DASHBOARD_PORT` | Axum dashboard/API port. Defaults to `3000`. |
| `PAPER_TRADING` | `true` for simulation, `false` for real live trading. |
| `STARTING_BALANCE` | Paper wallet starting balance in USDC. |
| `THRESHOLD_BPS` | Spike threshold in hundredths of USD. `2000` means `$20`. |
| `PORTFOLIO_PCT` | Fraction of balance per entry. `0.20` means 20%. |
| `MAX_ENTRY_PRICE` / `MIN_ENTRY_PRICE` | Share price entry bounds. |
| `SPIKE_SUSTAIN_MS` | Required spike persistence before entry. |
| `EXECUTION_DELAY_MS` | Paper-mode simulated entry delay. |
| `MIN_HOLD_MS` | Minimum hold time before most exits. |
| `TRAILING_STOP_PCT` | Share-price trailing stop distance after activation. |
| `TRAILING_STOP_ACTIVATION` | Gain required before trailing stop activates. |
| `STOP_LOSS_PCT` | Entry-price loss percentage that triggers stop-loss. |
| `HOLD_MIN_SHARE_PRICE` | Share price threshold for hold-to-resolution behavior. |
| `HOLD_SAFETY_MARGIN` | BTC price-to-beat safety margin for hold exits. |
| `EARLY_EXIT_LOSS_PCT` | Loss threshold for pre-expiry early exits. |
| `MAX_ORDERS_PER_MINUTE` | Rate limit guard. |
| `MAX_DAILY_LOSS` | Daily loss cap. |
| `MAX_EXPOSURE_PER_MARKET` | Market exposure cap. |
| `MAX_DRAWDOWN_PCT` | Max drawdown from starting balance. |
| `TREND_FILTER_ENABLED` | Enables counter-trend filtering. |
| `PTB_NEUTRAL_ZONE_USD` | Price-to-beat neutral zone. |
| `PTB_MAX_COUNTER_DISTANCE_USD` | Hard reject distance for counter-PTB trades. |

Percentage semantics matter:

- Backend `PORTFOLIO_PCT`, `MAX_DRAWDOWN_PCT`, and `EARLY_EXIT_LOSS_PCT` are stored as fractions.
- The dashboard displays those values in trader-friendly percentage form and converts them back before saving.

## Strategy Notes

Entry checks include:

- Binance spike magnitude versus configured threshold.
- Sustained spike timing.
- Share price min/max bounds.
- Distance from 50c fee-heavy zone.
- CLOB liquidity and order-book-aware fill estimates.
- Existing position scale level and pending entries.
- Trend filter and price-to-beat directional context.
- Daily loss, drawdown, exposure, balance, and order-rate limits.

Exit checks include:

- Trend reversal.
- Spike fade with persistence.
- Stop-loss.
- Trailing stop after activation.
- Hold-to-resolution BTC margin logic.
- Early exit near market end for losing positions.
- Forced close near market expiry.
- Manual close from the dashboard.

## Validation

Use these before pushing changes:

```bash
cargo fmt
cargo test
```

```bash
cd web
npm run build
```

Optional:

```bash
cargo clippy
```

Current test coverage is small and focused on config/server invariants. Paper trading is still the main functional harness for end-to-end behavior.

## Operational Files

- `.env` contains secrets and runtime config. Do not commit it.
- `lattice.db`, `lattice.db-shm`, and `lattice.db-wal` are local paper state. Do not commit them.
- `lattice-live.db`, `lattice-live.db-shm`, and `lattice-live.db-wal` are local live state. Do not commit them.
- `web/dist` is generated frontend output. Do not commit it.
- `target` is Rust build output. Do not commit it.

## Troubleshooting

- If the terminal shows the setup page, run `cd web && npm run build`.
- If Vite cannot reach the backend, make sure `cargo run` is serving on `DASHBOARD_PORT=3000`.
- If no market appears, the current 5-minute BTC market may not be available from Gamma yet; wait for the next window or check network access.
- If live approvals fail, verify Polygon gas, `POLYGON_RPC_URL`, wallet type, and private key.
- If paper state looks stale, stop the bot and remove `lattice.db`, `lattice.db-shm`, and `lattice.db-wal`.

## License

No license file is currently included. Add one before public redistribution.
