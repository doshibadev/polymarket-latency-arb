# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Polymarket latency arbitrage trading bot in Rust. Exploits the ~1.5-2 second delay between Binance BTC spot price movements and Chainlink oracle updates that Polymarket uses to resolve "BTC Up/Down 5-minute" binary options markets. Monitors dual price feeds, detects BTC price spikes, and buys UP/DOWN shares before the market catches up. Supports paper trading (simulated) and live trading (real CLOB orders on Polygon).

## Build & Run Commands

```bash
cargo build              # Dev build (no debug symbols for fast linking)
cargo build --release    # Optimized: opt-level=3, thin LTO, codegen-units=1
cargo run                # Run with .env config
cargo clippy             # Lint (no config file, use defaults)
cargo fmt                # Format (no config file, use defaults)
```

No tests exist in the codebase. Paper trading mode serves as the functional test harness.

## Running Modes

- **Paper trading**: `PAPER_TRADING=true` in `.env` (default). Simulates orders with configurable execution delay and slippage.
- **Live trading**: `PAPER_TRADING=false`. Requires `POLYMARKET_PRIVATE_KEY` and `POLYGON_RPC_URL` in `.env`.
- **Multiple instances**: Use different `.env` files with different `DASHBOARD_PORT` values (e.g., `.env.paper` uses port 3001).

## Architecture

Single async Tokio binary with four concurrent tasks wired together via `mpsc` and `broadcast` channels:

```
Binance WS (aggTrade, ~10ms) ──┐
                                ├── price_tx ──> ArbEngine ──> broadcast_tx ──> Dashboard WS
Polymarket RTDS (Chainlink) ───┘       ^              |
                                       |              v
CLOB WS (bid/ask) ─── clob_tx ────────┘    PaperWallet / LiveWallet
                                                     |
Market refresh ─── market_tx ──────────────>         |
                                              cmd_rx <── Dashboard (start/stop/reset)
```

### Module Responsibilities

- **`rtds/stream.rs`** - Dual price feed: spawns direct Binance WebSocket (`wss://stream.binance.com`) and Polymarket RTDS WebSocket for Chainlink oracle prices. Both send `PriceUpdate` structs on the same channel.
- **`arb/engine.rs`** (~840 lines, the core) - Main `tokio::select!` event loop. Maintains per-symbol state (`SymbolState`), detects spikes via baseline comparison, manages positions, and handles exit strategies: trend reversal, spike faded, stop-loss, hold-to-resolution, and market-end force-close. Also broadcasts state JSON to the dashboard.
- **`execution/paper.rs`** (~850 lines) - Paper wallet with simulated execution delay, slippage, fee calculation, and persistent state in `paper_wallet_state.json`.
- **`execution/live.rs`** (~800 lines) - Real trading via `polymarket-client-sdk`. Handles CLOB order placement, ERC20/ERC1155 approvals via `alloy`, and order status polling.
- **`polymarket/market_data.rs`** - Fetches active 5-minute BTC markets from Polymarket Gamma API, extracts Chainlink price info.
- **`polymarket/clob_client.rs`** - Subscribes to Polymarket CLOB WebSocket for real-time best bid/ask on UP/DOWN tokens. Auto-refreshes when market windows rotate.
- **`server/mod.rs`** - Axum HTTP server: serves `dashboard.html` at `/`, WebSocket at `/ws` for real-time state, REST at `/config`, `/settings`, `/command`.
- **`config/mod.rs`** - `AppConfig` loaded from env vars via `dotenvy`. Supports runtime updates from dashboard via `update_from_json()`.
- **`error.rs`** - `ArbError` enum with `thiserror` derive for WebSocket, HTTP, JSON, Config, Execution, and PriceData errors. Defines `Result<T>` alias.

### Key Data Flow Concepts

- **Spike detection**: Compares current Binance price against pre-computed 200ms and 1s baselines. Spike must exceed `THRESHOLD_BPS` (in hundredths of USD, e.g., 2000 = $20) and sustain for `SPIKE_SUSTAIN_MS`.
- **Market windows**: 5-minute binary options that resolve via Chainlink. The bot fetches new market data when windows rotate (`market_tx` channel).
- **Exit strategies are layered**: min hold time must pass first, then trend reversal, spike faded (with persistence timer), stop-loss, and hold-to-resolution (activates when share price is high near market end).

## Configuration

All config is via environment variables loaded from `.env` (see `.env.example` for full reference). Key env vars and their units:

| Variable | Unit | Default | Note |
|---|---|---|---|
| `THRESHOLD_BPS` | hundredths of USD | 2000 | 2000 = $20 spike threshold |
| `PORTFOLIO_PCT` | fraction | 0.20 | 20% of balance per trade |
| `SPIKE_FADED_MS` | milliseconds | 200 | Reversal persistence before exit |
| `MIN_HOLD_MS` | milliseconds | 1500 | Minimum position hold time |
| `STOP_LOSS_PCT` | percentage | 30 | 30% drop from entry triggers exit |
| `HOLD_SAFETY_MARGIN` | USD | 20 | Exit hold if BTC within $20 of price_to_beat |

## Dashboard

`dashboard.html` is a single-file web app (HTML/CSS/JS with Chart.js) served by the Axum server. It connects via WebSocket to display real-time prices, positions, and signals. The server must be run from the repo root so `dashboard.html` is found via relative path.

## Key Dependencies

- **`polymarket-client-sdk`** - Polymarket CLOB client (order placement, signing)
- **`alloy`** - Ethereum/Polygon interaction (approvals, contract calls)
- **`k256`** - secp256k1 signing for order authentication
- **`tokio-tungstenite`** - Async WebSocket connections to Binance and Polymarket
- **`axum` + `tower-http`** - Dashboard web server with WebSocket, CORS, static file serving
