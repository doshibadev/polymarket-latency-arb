# Live V1 Go-Live TODO

Scope: backend, live execution, safety, ops, tests, and release process only. Frontend implementation is intentionally excluded because it is handled separately. Current date: 2026-04-17.

Go-live means `PAPER_TRADING=false` with real USDC on Polymarket. Do not trade real money until all `BLOCKER` items are done and validated.

## 1. Fix Live Fill Mirroring Before Any Real Money

- [x] `BLOCKER` Prevent live mode from promoting paper pending entries until a real live fill result is received.
  - Current risk: `PaperWallet::flush_pending()` uses zero delay in live mode, so the paper strategy mirror can create a simulated open position before `LiveEvent::OpenPositionResult` confirms the real CLOB fill.
  - Required behavior: paper mode may simulate after `EXECUTION_DELAY_MS`; live mode must keep a pending mirror only, then promote exactly from the live fill data or rollback on live reject.
  - Files: `src/execution/paper.rs`, `src/arb/engine.rs`, `src/execution/actor.rs`.

- [x] Add tests for the live pending lifecycle.
  - Test live open success: pending entry remains pending until live fill, then becomes an open mirror with live fill price/shares/cost.
  - Test live open reject: pending entry is removed and no phantom open position remains.
  - Test delayed actor response: no simulated position appears during the delay.

- [x] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 2. Add Stable Position Identity Everywhere

- [x] `BLOCKER` Add a unique `position_id` to `OpenPosition`, `PendingEntry`, close intents, live commands, live events, DB rows, and dashboard position snapshots.
  - Current risk: several paths identify positions by `(symbol, direction)` only. That can close/sync/remove the wrong position if scaling, duplicate direction entries, retries, or orphan recovery produce multiple same-side positions.
  - Required behavior: every entry, close, mirror sync, pending close, manual close, and DB row targets one exact position.
  - Files: `src/execution/model.rs`, `src/execution/planner.rs`, `src/execution/paper.rs`, `src/execution/live.rs`, `src/execution/actor.rs`, `src/arb/engine.rs`, `src/server/mod.rs`.

- [x] Replace all close/sync keys that use only `(symbol, direction)`.
  - `pending_live_closes` must key by `position_id`.
  - `planned_exit_intents()` must emit `position_id`.
  - `LiveCommand::ClosePosition` must target `position_id`, not just symbol/direction.
  - `LiveWallet::close_position_at()` should close by exact ID.
  - `sync_paper_to_live_snapshot()` must match by `position_id`.

- [x] Update live DB schema with safe migration.
  - Add `position_id TEXT NOT NULL` or compatible generated migration path for existing rows.
  - Add unique/index constraints that prevent duplicate live rows for the same position.
  - Keep live DB separate from paper DB.

- [x] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 3. Lock Down Dashboard/API Before Live

- [x] `BLOCKER` Add backend authentication for `/settings`, `/command`, and websocket routes.
  - Current risk: server binds `0.0.0.0`, uses permissive CORS, and accepts unauthenticated commands that can start/stop the bot, close live positions, toggle markets, or mutate `.env`.
  - Required behavior: live mode must require an admin token or local-only binding before accepting command/config routes.
  - Files: `src/server/mod.rs`, `src/config/mod.rs`, `.env.example`, `.env.paper`.
  - Delivered: protected `/settings`, `/command`, `/ws`, `/ws/fast`, and `/ws/slow` with `DASHBOARD_AUTH_TOKEN`; frontend now forwards token on HTTP + websocket requests.

- [x] Add `DASHBOARD_BIND_ADDR` and default safely for live.
  - Recommended default: `127.0.0.1`.
  - If binding to `0.0.0.0` in live mode, require explicit `DASHBOARD_AUTH_TOKEN`.

- [x] Reject or restrict risky commands in live mode.
  - `reset` already only resets paper state in engine, but backend should still reject live reset commands.
  - Settings updates in live mode should either be disabled while running or allow only a validated safe subset.
  - `close_position` should target `position_id`, not index.

- [x] Add server tests.
  - Unauthenticated live command returns unauthorized.
  - Authenticated command passes.
  - Live mode cannot reset state.
  - Invalid settings values are rejected.

- [x] Commit + push after this block.
  - Validation run: `cargo fmt`, `cargo test`, `cd web && npm run build`.
  - Follow-up: `cargo clippy -- -D warnings` still fails on pre-existing repo-wide lints outside this block; `cargo build --release` was started but not waited to completion because fat LTO link/build was slow.

## 4. Add Submit-Time Live Execution Guards

- [x] `BLOCKER` Revalidate live quote and risk immediately before CLOB order submission.
  - Current behavior: shared planner decides using cached bid/ask/depth, then live executor uses current ask/bid plus a fixed 4c buffer.
  - Required behavior: live executor must reject if quote is stale, spread widened, ask/bid moved beyond configured slippage, book depth vanished, or order notional exceeds live caps.
  - Files: `src/execution/live.rs`, `src/execution/planner.rs`, `src/config/mod.rs`.

- [x] Add live-specific safety config.
  - `LIVE_MAX_ORDER_USDC`.
  - `LIVE_MAX_SESSION_LOSS_USDC`.
  - `LIVE_MAX_OPEN_POSITIONS`.
  - `LIVE_MAX_SLIPPAGE_CENTS`.
  - `LIVE_MAX_QUOTE_AGE_MS`.
  - `LIVE_DRY_RUN_ORDERS=false` optional final staging flag if useful.

- [x] Make the 4c live buy/sell buffer configurable and bounded.
  - Do not hardcode `ask + 0.04` / `bid - 0.04` for real money without env-controlled limits.

- [x] Add tests with mocked quote/order data.
  - Reject stale quotes.
  - Reject widened spreads.
  - Reject notional over live caps.
  - Accept valid live plan without changing shared strategy math.

- [x] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 5. Harden Live DB Persistence And Recovery

- [x] `BLOCKER` Make live DB writes transactionally durable around real order effects.
  - Current risk: live order can fill, then process can crash before async DB writer persists the new open position. Startup recovery currently loads local DB first and may not discover every external on-chain token balance if no local row exists.
  - Required behavior: after a real fill, persist critical state before reporting success to strategy, or reconcile from CLOB/on-chain balances independent of local DB on startup.
  - Files: `src/execution/live.rs`, `src/execution/actor.rs`.

- [x] Startup recovery must discover external live positions even if local DB is missing/stale.
  - Query current market token balances/open orders for registered markets.
  - Reconstruct or quarantine unmatched positions.
  - Never silently ignore nonzero conditional-token balances.

- [x] Add a live reconciliation report at startup.
  - Show local DB positions, CLOB balances, open orders, USDC balance, approvals, and mismatches.
  - In live mode, block start if mismatches are unsafe until user resolves them.

- [x] Add integration tests around restart recovery.
  - Filled order before DB save.
  - DB row exists but token balance is zero.
  - Token balance exists but DB row missing.
  - Pending close before crash.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 6. Finish Live Startup Preflight

- [x] Expand preflight into one explicit live gate before the bot can start.
  - Validate private key, wallet type, wallet address, CLOB auth, RPC connectivity, chain ID, gas balance, USDC balance, approvals, active markets, token IDs, websocket freshness, and clock skew.
  - Current preflight exists, but it should become the single authoritative live readiness gate with clear fail reasons.

- [x] Make gas threshold and USDC minimum configurable.
  - Current code checks a small POL amount but error text asks for more. Make this exact and documented.

- [x] Require fresh market data before `start`.
  - Do not allow live start until CLOB prices, Binance price, Chainlink price, and market metadata are fresh for every enabled live symbol.

- [x] Add preflight command/report endpoint for ops.
  - Backend only; frontend can wire later.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 7. Add Live Pause, Kill Switch, And Safe Shutdown

- [ ] Add a backend kill switch that immediately stops new entries.
  - This must be independent of frontend availability.
  - Suggested controls: env file flag reload, local CLI signal, or protected command route.

- [ ] Add graceful shutdown handling.
  - On `SIGINT`/`SIGTERM`, stop new entries, flush live DB, cancel/reconcile pending orders, and print open position summary.

- [ ] Decide and implement emergency flatten behavior.
  - Separate `STOP_NEW_ENTRIES` from `FLATTEN_ALL`.
  - Flatten should use exact `position_id` and live quote guards.

- [ ] Add automatic circuit breakers.
  - Pause on max session loss, stale feed, repeated CLOB errors, repeated DB write failures, quote lag, or live/paper mirror mismatch.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 8. Validate Paper/Live Parity With Tests

- [ ] Add parity tests proving strategy logic is shared.
  - Feed identical synthetic market/price/book events.
  - Assert paper and live produce the same entry/exit decisions before execution adapter differences.
  - Assert live executor only changes interaction with Polymarket, not strategy decisions.

- [ ] Add compile-time or test guard against strategy drift in live executor.
  - Live must consume `PendingEntry` and shared exit intents only.
  - No duplicate spike/entry/exit strategy conditions in `live.rs`.

- [ ] Add golden tests for existing optimized behavior.
  - Spike sustain timing.
  - 200ms/1s/5s/30s baseline logic.
  - Trend filter and PTB gates.
  - Hold-to-resolution logic.
  - Trailing stop and stop loss.
  - Order-book-aware FAK estimation.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 9. Clean Config And Symbol Control

- [ ] Move hardcoded live symbols out of `main.rs`.
  - Current code starts BTC and ETH unconditionally.
  - Add `SYMBOLS=BTC,ETH` or separate `PAPER_SYMBOLS` / `LIVE_SYMBOLS`.
  - In live mode, require every enabled symbol to pass market, RTDS, CLOB, risk, and preflight checks.

- [ ] Add strict config validation on startup and settings update.
  - Reject invalid ranges such as negative fees, impossible entry price bounds, drawdown over 100%, zero/negative order limits, and dangerous live defaults.
  - Document all new env vars in `.env.example` and `.env.paper` if relevant.

- [ ] Split mutable runtime settings from startup-only settings.
  - Startup-only: mode, private key, wallet type, RPC URL, DB path, bind address, auth token, symbol list.
  - Runtime-mutable: conservative strategy/risk fields only, and maybe only while stopped in live mode.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 10. Improve Observability And Audit Logs

- [ ] Add structured live audit log file.
  - Log every live decision, reject, submit, fill, partial fill, close, retry, reconciliation mismatch, DB write, and manual command.
  - Include `position_id`, order ID, token ID, symbol, direction, quote age, planned price, submitted limit, filled price, shares, USDC, fees, latency, and reason.

- [ ] Add metrics/summary output for live operations.
  - Order submit latency p50/p95 already exists; expand with reject reasons, queue depth, DB writer lag, websocket freshness, and reconciliation status.

- [ ] Add command/event correlation IDs.
  - Every dashboard/ops command should be traceable through engine -> actor -> live wallet -> DB -> event.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 11. Reduce Live Operational Footguns

- [ ] Add separate configurable DB paths.
  - Current default split is `lattice.db` and `lattice-live.db`; keep it.
  - Add explicit `PAPER_DB_PATH` and `LIVE_DB_PATH` so live and paper cannot accidentally share state.

- [ ] Add DB backup/export command before live session.
  - Backup live DB before each real-money run.
  - Include schema version in backup metadata.

- [ ] Add dependency/security checks to release process.
  - Run `cargo audit` if installed.
  - Run `npm audit --omit=dev` only if frontend dependencies are in scope for release.

- [ ] Add production runbook updates.
  - Live startup checklist.
  - Recovery checklist.
  - How to stop new entries.
  - How to flatten.
  - How to inspect `lattice-live.db`.
  - How to recover from mismatched local DB vs CLOB balances.

- [ ] Commit + push after this block.
  - Validation before commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`.

## 12. Final Paper Soak And Tiny Live Staging

- [ ] Run extended paper soak with the exact live config except `PAPER_TRADING=true`.
  - Use same symbols, risk limits, dashboard bind/auth, and release binary.
  - Record fills/rejects/exits/recovery behavior.

- [ ] Run live preflight with bot stopped.
  - Confirm no start until all checks pass.
  - Confirm auth is required.
  - Confirm DB backup created.

- [ ] Run tiny live staging only after blockers are complete.
  - Use minimal USDC/order caps.
  - Start with one enabled symbol.
  - Watch logs, DB rows, live Polymarket account, and dashboard mirror.
  - Stop after first full entry/exit cycle and reconcile manually.

- [ ] Commit + push final release checkpoint.
  - Validation before final commit: `cargo fmt`, `cargo test`, `cargo clippy`, `cargo build --release`, and `cd web && npm run build` if backend contract/types changed.

## Not In This TODO

- Frontend layout, styling, components, and UX polish.
- ETH frontend display work.
- New strategy improvements that intentionally change engine behavior. V1 go-live work should preserve the current strategy unless a safety bug requires a change.
