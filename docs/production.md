# Production Checklist

Use this bot close to exchange endpoints and keep the paper/live realism knobs intact.

## Runtime

- Build with `cargo build --release`
- Native CPU tuning is enabled through `.cargo/config.toml`
- Optional allocator experiment:
  - default allocator: `cargo build --release`
  - mimalloc: `cargo build --release --features mimalloc`

## Deployment

- Run from a host geographically close to:
  - Polymarket CLOB / Polygon RPC endpoints
  - Binance websocket edge
- Prefer a stable low-jitter VPS over consumer internet
- Keep process clock synchronized:
  - macOS: system time sync enabled
  - Linux: `chronyd` or `systemd-timesyncd`
- Keep websocket/RPC egress stable:
  - avoid VPN hops
  - avoid overloaded shared hosts
  - pin reliable Polygon RPC

## Safety

- Keep realism knobs enabled unless intentionally testing:
  - `EXECUTION_DELAY_MS`
  - `MIN_HOLD_MS`
  - hold/safety/trailing-stop settings
- Validate paper/live config separately before changing live funds
- Monitor latency summaries in logs after deploy changes
