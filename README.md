# Polymarket Latency Arbitrage Bot

High-frequency trading bot that exploits the 1.7-second delay between Binance price updates and Polymarket chart visibility.

## How It Works

1. **Binance moves** → Bot detects spike instantly
2. **1.7 seconds later** → Polymarket chart updates
3. **Traders react** → Polymarket price catches up
4. **Profit** → Exit before edge expires

## Features

- Real-time Binance & Chainlink price feeds
- 200ms spike detection (fast baseline)
- Paper trading mode with realistic execution delays
- Live trading on Polymarket
- Web dashboard with performance charts
- Position management with trailing stops

## Setup

1. Install Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
2. Copy `.env.example` to `.env`
3. Configure settings in `.env`
4. Run: `cargo run --release`
5. Open dashboard: `http://localhost:3030`

## Configuration

Key settings in `.env`:

```bash
PAPER_TRADING=true              # false for live trading
STARTING_BALANCE=27.0           # Initial balance
THRESHOLD_BPS=2000              # $20 spike threshold
PORTFOLIO_PCT=0.10              # 10% per trade
EXECUTION_DELAY_MS=100          # Simulated latency
SPIKE_SUSTAIN_MS=0              # Instant entry
TRAILING_STOP_PCT=15            # 15% trailing stop
```

## Dashboard

- Portfolio performance chart
- Open positions with live P&L
- Trade execution log
- Signal terminal
- Recent resolved markets
- Real-time metrics

## Strategy

- **Entry**: Binance spike > $20 in 200ms
- **Hold**: 1-2 seconds (let Polymarket catch up)
- **Exit**: Trailing stop, trend reversal, or spike fade
- **Edge**: 1.7s chart delay = information advantage

## Live Trading

1. Set `PAPER_TRADING=false` in `.env`
2. Add `POLYMARKET_PRIVATE_KEY` to `.env`
3. Ensure USDC balance on Polygon
4. Bot auto-approves CTF Exchange contract
5. Start trading

## Risk Management

- Max entry price: 0.70 (avoid high-fee zones)
- Trailing stop: 15% from peak
- Position timeout: 295 seconds
- Auto-close before market end

## License

MIT
