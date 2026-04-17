use crate::config::AppConfig;
use crate::error::Result;
use crate::execution::{
    paper::OpenPosition, LatencyTrace, LiveCommand, LiveEvent, LiveExecutionHandle,
    LiveWalletSnapshot, PaperWallet,
};
use crate::polymarket::{MarketData, SharePriceUpdate};
use crate::rtds::{PriceSource, PriceUpdate};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

struct LatencySample {
    total_ms: u64,
    decision_ms: u64,
    queue_ms: u64,
    submit_ms: u64,
    apply_ms: u64,
}

#[derive(Clone, Copy)]
enum LatencyKind {
    Open,
    Close,
}

#[derive(Default)]
struct LatencyStats {
    open_order_samples: VecDeque<LatencySample>,
    close_order_samples: VecDeque<LatencySample>,
}

impl LatencyStats {
    fn record(&mut self, kind: LatencyKind, sample: LatencySample) {
        let samples = match kind {
            LatencyKind::Open => &mut self.open_order_samples,
            LatencyKind::Close => &mut self.close_order_samples,
        };

        samples.push_back(sample);
        if samples.len() > 128 {
            samples.pop_front();
        }

        if samples.len() % 10 == 0 {
            let total_p50 = Self::percentile(samples, |sample| sample.total_ms, 50.0);
            let total_p95 = Self::percentile(samples, |sample| sample.total_ms, 95.0);
            let submit_p95 = Self::percentile(samples, |sample| sample.submit_ms, 95.0);
            let label = match kind {
                LatencyKind::Open => "open",
                LatencyKind::Close => "close",
            };
            info!(
                operation = label,
                samples = samples.len(),
                total_p50_ms = total_p50,
                total_p95_ms = total_p95,
                submit_p95_ms = submit_p95,
                "Latency summary: decision -> live result applied"
            );
        }
    }

    fn percentile<F>(samples: &VecDeque<LatencySample>, map: F, percentile: f64) -> u64
    where
        F: Fn(&LatencySample) -> u64,
    {
        if samples.is_empty() {
            return 0;
        }

        let mut values: Vec<u64> = samples.iter().map(map).collect();
        values.sort_unstable();
        let idx = (((values.len() - 1) as f64) * (percentile / 100.0)).round() as usize;
        values[idx]
    }
}

#[derive(Default)]
struct SlowSnapshotCache {
    trades: Value,
    history: Value,
    config: Value,
    updated_at: Option<Instant>,
}

enum DashboardMessage {
    Fast(Value),
    Slow(Value),
}

struct LiveCloseIntent {
    symbol: String,
    direction: String,
    price: f64,
    reason: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum StrategyDirection {
    Up,
    Down,
}

impl StrategyDirection {
    fn from_spike(spike: f64) -> Self {
        if spike > 0.0 {
            Self::Up
        } else {
            Self::Down
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Up => "UP",
            Self::Down => "DOWN",
        }
    }
}

#[derive(Clone, Copy)]
enum SignalStatus {
    Executed,
    Rejected,
    Exit,
    Hold,
}

impl From<StrategyDirection> for &'static str {
    fn from(value: StrategyDirection) -> Self {
        value.as_str()
    }
}

impl SignalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Executed => "EXECUTED",
            Self::Rejected => "REJECTED",
            Self::Exit => "EXIT",
            Self::Hold => "HOLD",
        }
    }
}

#[derive(Default)]
struct SymbolState {
    pub last_binance: Option<(f64, Instant)>,
    pub last_chainlink: Option<(f64, Instant)>,
    pub last_spike_usd: f64,
    pub market_end_ts: Option<u64>,
    pub condition_id: Option<String>,
    pub question: String,
    pub price_to_beat: Option<f64>,
    pub price_to_beat_set: bool,
    pub btc_history: VecDeque<(f64, Instant)>,
    pub last_rejection: Option<(String, Instant)>,
    pub spike_confirmed_since: Option<(f64, Instant)>,
    // Pre-computed baselines for faster spike detection
    pub baseline_200ms: Option<f64>,
    pub baseline_1s: Option<f64>,
    pub baseline_5s: Option<f64>,
    // Decimated price history: 1 sample per ~500ms, 30s window (for trend detection)
    pub btc_history_slow: VecDeque<(f64, Instant)>,
    pub baseline_30s: Option<f64>,
    // Trend magnitudes (binance - baseline) in USD
    pub trend_5s_usd: f64,
    pub trend_30s_usd: f64,
    // Cooldown after position closes before allowing re-entry
    pub last_close_time: Option<Instant>,
    // Cooldown after market transition to prevent buying stale tokens
    pub last_market_update: Option<Instant>,
}

pub struct ArbEngine {
    config: AppConfig,
    price_rx: mpsc::Receiver<PriceUpdate>,
    clob_rx: mpsc::Receiver<SharePriceUpdate>,
    market_rx: mpsc::Receiver<MarketData>,
    cmd_rx: mpsc::Receiver<serde_json::Value>,
    pub wallet: PaperWallet,
    pub live_execution: Option<LiveExecutionHandle>,
    pub live_event_rx: Option<mpsc::Receiver<LiveEvent>>,
    live_state: Option<LiveWalletSnapshot>,
    symbol_states: HashMap<String, SymbolState>,
    signals: VecDeque<Value>,
    latency_stats: LatencyStats,
    slow_snapshot_cache: SlowSnapshotCache,
    dashboard_tx: mpsc::Sender<DashboardMessage>,
    losing_direction_counts: HashMap<(String, StrategyDirection), u32>,
    processed_trade_count: usize,
    running: bool,
    started_at: Option<Instant>,
    last_clob_sync: Option<Instant>,
    pending_live_closes: HashSet<(String, String)>,
}

impl ArbEngine {
    pub fn new(
        config: AppConfig,
        price_rx: mpsc::Receiver<PriceUpdate>,
        clob_rx: mpsc::Receiver<SharePriceUpdate>,
        market_rx: mpsc::Receiver<MarketData>,
        broadcast_tx: broadcast::Sender<String>,
        cmd_rx: mpsc::Receiver<serde_json::Value>,
    ) -> Self {
        let wallet = PaperWallet::new(config.clone());
        let (dashboard_tx, mut dashboard_rx) = mpsc::channel(64);
        let ui_broadcast_tx = broadcast_tx.clone();
        tokio::spawn(async move {
            while let Some(message) = dashboard_rx.recv().await {
                let payload = match message {
                    DashboardMessage::Fast(snapshot) => json!({
                        "_kind": "fast",
                        "snapshot": snapshot,
                    }),
                    DashboardMessage::Slow(snapshot) => json!({
                        "_kind": "slow",
                        "snapshot": snapshot,
                    }),
                };

                if let Ok(json_str) = serde_json::to_string(&payload) {
                    let _ = ui_broadcast_tx.send(json_str);
                }
            }
        });

        Self {
            config,
            price_rx,
            clob_rx,
            market_rx,
            cmd_rx,
            wallet,
            live_execution: None,
            live_event_rx: None,
            live_state: None,
            symbol_states: HashMap::new(),
            signals: VecDeque::new(),
            latency_stats: LatencyStats::default(),
            slow_snapshot_cache: SlowSnapshotCache::default(),
            dashboard_tx,
            losing_direction_counts: HashMap::new(),
            processed_trade_count: 0,
            running: false, // starts paused — user must press START
            started_at: None,
            last_clob_sync: None,
            pending_live_closes: HashSet::new(),
        }
    }

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn now_hms() -> String {
        Self::now_rfc3339()[11..19].to_string()
    }

    fn current_balance(&self) -> f64 {
        self.live_state
            .as_ref()
            .map(|lw| lw.balance)
            .unwrap_or(self.wallet.balance)
    }

    fn is_live_mode(&self) -> bool {
        self.live_execution.is_some()
    }

    fn current_net_market_value(&self) -> f64 {
        let positions = self
            .live_state
            .as_ref()
            .map(|lw| &lw.open_positions)
            .unwrap_or(&self.wallet.open_positions);

        positions
            .iter()
            .map(|pos| {
                let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                let price = if b > 0.0 && a > 0.0 {
                    (b + a) / 2.0
                } else {
                    pos.entry_price
                };
                pos.shares * price
            })
            .sum()
    }

    fn is_live_close_pending(&self, symbol: &str, direction: &str) -> bool {
        self.pending_live_closes
            .contains(&(symbol.to_string(), direction.to_string()))
    }

    fn trade_reason_to_live_reason(reason: Option<&str>) -> &'static str {
        match reason.unwrap_or("exit") {
            "trailing_stop" => "trailing_stop",
            "max_price" => "max_price",
            "trend_reversed" => "trend_reversed",
            "spike_faded" => "spike_faded",
            "stop_loss" => "stop_loss",
            "near_end" => "near_end",
            "market_end" => "market_end",
            "manual" => "manual",
            "hold_safety_exit" => "hold_safety_exit",
            "early_exit_losing" => "early_exit_losing",
            _ => "exit",
        }
    }

    async fn request_live_close(
        &mut self,
        symbol: String,
        direction: String,
        price: f64,
        reason: &'static str,
    ) -> bool {
        if !self.is_live_mode() {
            return false;
        }

        let key = (symbol.clone(), direction.clone());
        if !self.pending_live_closes.insert(key.clone()) {
            return false;
        }

        let latency_trace = self.close_latency_trace(&symbol, &direction);
        let sent = self
            .send_live_command(LiveCommand::ClosePosition {
                symbol,
                direction,
                price,
                reason,
                latency_trace,
            })
            .await;
        if !sent {
            self.pending_live_closes.remove(&key);
        }
        sent
    }

    async fn evaluate_live_exit_intents(&mut self) -> Vec<LiveCloseIntent> {
        let original_open_positions: Vec<OpenPosition> = self.wallet.open_positions.clone();
        let original_balance = self.wallet.balance;
        let original_trade_count = self.wallet.trade_count;
        let original_wins = self.wallet.wins;
        let original_losses = self.wallet.losses;
        let original_total_fees_paid = self.wallet.total_fees_paid;
        let original_total_volume = self.wallet.total_volume;
        let original_trade_history_len = self.wallet.trade_history.len();
        let pending_live_closes = self.pending_live_closes.clone();

        self.wallet.open_positions.retain(|pos| {
            !pending_live_closes.contains(&(pos.symbol.clone(), pos.direction.clone()))
        });

        let _ = self.wallet.try_close_position().await;

        let intents = self.wallet.trade_history[original_trade_history_len..]
            .iter()
            .filter(|trade| trade.r#type == "exit")
            .map(|trade| LiveCloseIntent {
                symbol: trade.symbol.clone(),
                direction: trade.direction.clone(),
                price: trade.exit_price.unwrap_or(0.0),
                reason: Self::trade_reason_to_live_reason(trade.close_reason.as_deref()),
            })
            .collect();

        self.wallet.open_positions = original_open_positions;
        self.wallet.balance = original_balance;
        self.wallet.trade_count = original_trade_count;
        self.wallet.wins = original_wins;
        self.wallet.losses = original_losses;
        self.wallet.total_fees_paid = original_total_fees_paid;
        self.wallet.total_volume = original_total_volume;
        self.wallet
            .trade_history
            .truncate(original_trade_history_len);

        intents
    }

    fn add_signal(
        &mut self,
        symbol: &str,
        direction: StrategyDirection,
        spike: f64,
        status: SignalStatus,
        reason: Option<String>,
    ) {
        self.signals.push_back(json!({
            "t": Self::now_hms(),
            "s": symbol,
            "d": direction.as_str(),
            "v": spike,
            "st": status.as_str(),
            "r": reason
        }));
        if self.signals.len() > 50 {
            self.signals.pop_front();
        }
    }

    fn update_history(&mut self, total_val: f64) {
        self.wallet.push_history(total_val);
    }

    fn drain_wallet_events_to_signals(&mut self) {
        for event in self.wallet.drain_events() {
            self.add_signal(
                &event.symbol,
                StrategyDirection::from_spike(if event.direction == "UP" { 1.0 } else { -1.0 }),
                event.spike,
                match event.status.as_str() {
                    "EXECUTED" => SignalStatus::Executed,
                    "EXIT" => SignalStatus::Exit,
                    "HOLD" => SignalStatus::Hold,
                    _ => SignalStatus::Rejected,
                },
                Some(event.reason),
            );
        }
    }

    /// Helper to get current unix timestamp in seconds
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn broadcast_state(&mut self) {
        let slow_cache_updated = self.refresh_slow_snapshot_cache();

        let (
            balance,
            open_positions_ref,
            trade_history_ref,
            wins,
            losses,
            fees,
            volume,
            starting_balance,
            _is_live,
        ) = if let Some(lw) = &self.live_state {
            (
                &lw.balance,
                &lw.open_positions,
                &lw.trade_history,
                lw.wins,
                lw.losses,
                lw.total_fees_paid,
                lw.total_volume,
                lw.starting_balance,
                true,
            )
        } else {
            (
                &self.wallet.balance,
                &self.wallet.open_positions,
                &self.wallet.trade_history,
                self.wallet.wins,
                self.wallet.losses,
                self.wallet.total_fees_paid,
                self.wallet.total_volume,
                self.wallet.starting_balance,
                false,
            )
        };

        let mut unrealized_pnl = 0.0;
        let mut _net_market_value = 0.0;
        let mut positions = Vec::new();

        for pos in open_positions_ref {
            // Use live wallet's price cache for live positions, paper wallet's cache for paper
            let (up_p, down_p) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);

            let current_price = if up_p > 0.0 && down_p > 0.0 {
                (up_p + down_p) / 2.0
            } else {
                pos.entry_price
            };
            let current_value = pos.shares * current_price;
            let pnl = current_value - pos.position_size - pos.buy_fee;

            unrealized_pnl += pnl;
            _net_market_value += current_value;

            positions.push(json!({
                "symbol": pos.symbol,
                "cost": pos.position_size + pos.buy_fee,
                "entry_price": pos.entry_price,
                "current_price": current_price,
                "shares": pos.shares,
                "side": pos.direction,
                "pnl": pnl,
                "level": pos.scale_level,
                "held_ms": pos.entry_time.elapsed().as_millis() as u64,
                "hold_to_resolution": pos.hold_to_resolution
            }));
        }

        let total_val = balance + _net_market_value;
        let total_pnl = total_val - starting_balance;
        let realized_pnl: f64 = trade_history_ref
            .iter()
            .filter(|t| t.r#type == "exit")
            .filter_map(|t| t.pnl)
            .sum();

        let wallet_address = self.live_state.as_ref().map(|lw| lw.wallet_address.clone());

        let mut markets = HashMap::new();
        for (symbol, state) in &self.symbol_states {
            let (up_b, up_a) = self.wallet.get_bid_ask(symbol, "UP");
            let (dn_b, dn_a) = self.wallet.get_bid_ask(symbol, "DOWN");

            let binance = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);
            let chainlink = state.last_chainlink.map(|(p, _)| p).unwrap_or(0.0);

            let spike = {
                let cutoff = std::time::Duration::from_millis(500);
                let baseline = state
                    .btc_history
                    .iter()
                    .find(|(_, t)| t.elapsed() >= cutoff)
                    .map(|(p, _)| *p);
                match baseline {
                    Some(b) => binance - b,
                    None => 0.0,
                }
            };

            markets.insert(
                symbol.clone(),
                json!({
                    "question": state.question,
                    "up_price": (up_b + up_a) / 2.0,
                    "down_price": (dn_b + dn_a) / 2.0,
                    "binance": binance,
                    "chainlink": chainlink,
                    "spike": spike,
                    "ema_offset": 0.0,
                    "price_to_beat": state.price_to_beat,
                    "end_ts": state.market_end_ts
                }),
            );
        }

        let fast_state = json!({
            "balance": balance,
            "starting_balance": starting_balance,
            "total_portfolio_value": total_val,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
            "total_pnl": total_pnl,
            "cumulative_pnl": self.live_state.as_ref().map(|lw| lw.cumulative_pnl).unwrap_or(0.0),
            "daily_pnl": self.live_state.as_ref().map(|lw| lw.daily_pnl).unwrap_or(0.0),
            "wins": wins,
            "losses": losses,
            "total_fees": fees,
            "total_volume": volume,
            "positions": positions,
            "signals": self.signals,
            "latency": self.latest_latency_json(),
            "markets": markets,
            "running": self.running,
            "runtime_ms": self.started_at.map(|started_at| started_at.elapsed().as_millis() as u64).unwrap_or(0),
            "is_live": self.live_execution.is_some()
        });

        let _ = self
            .dashboard_tx
            .try_send(DashboardMessage::Fast(fast_state));

        if slow_cache_updated {
            let slow_state = json!({
                "trades": self.slow_snapshot_cache.trades.clone(),
                "history": self.slow_snapshot_cache.history.clone(),
                "config": self.slow_snapshot_cache.config.clone(),
                "wallet_address": wallet_address,
            });
            let _ = self
                .dashboard_tx
                .try_send(DashboardMessage::Slow(slow_state));
        }
    }

    fn latest_latency_json(&self) -> Value {
        json!({
            "open": self.latency_stats.open_order_samples.back().map(|last| json!({
                "total_ms": last.total_ms,
                "decision_ms": last.decision_ms,
                "queue_ms": last.queue_ms,
                "submit_ms": last.submit_ms,
                "apply_ms": last.apply_ms
            })).unwrap_or(Value::Null),
            "close": self.latency_stats.close_order_samples.back().map(|last| json!({
                "total_ms": last.total_ms,
                "decision_ms": last.decision_ms,
                "queue_ms": last.queue_ms,
                "submit_ms": last.submit_ms,
                "apply_ms": last.apply_ms
            })).unwrap_or(Value::Null)
        })
    }

    fn refresh_slow_snapshot_cache(&mut self) -> bool {
        let should_refresh = self
            .slow_snapshot_cache
            .updated_at
            .map(|updated_at| updated_at.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true);

        if !should_refresh {
            return false;
        }

        let (trade_history_ref, starting_balance, is_live) = if let Some(lw) = &self.live_state {
            (&lw.trade_history, lw.starting_balance, true)
        } else {
            (
                &self.wallet.trade_history,
                self.wallet.starting_balance,
                false,
            )
        };

        self.slow_snapshot_cache.trades = json!(trade_history_ref);
        self.slow_snapshot_cache.history = if is_live {
            let mut chart_points = Vec::new();
            chart_points.push(json!({ "t": "start", "v": starting_balance }));
            let mut running_value = starting_balance;
            for trade in trade_history_ref.iter().filter(|t| t.r#type == "exit") {
                if let Some(pnl) = trade.pnl {
                    running_value += pnl;
                    chart_points.push(json!({
                        "t": &trade.timestamp[11..19],
                        "v": running_value
                    }));
                }
            }
            json!(chart_points)
        } else {
            json!(self.wallet.history)
        };
        self.slow_snapshot_cache.config = json!({
            "threshold_bps": self.config.threshold_bps,
            "portfolio_pct": self.config.portfolio_pct,
            "crypto_fee_rate": self.config.crypto_fee_rate,
            "max_entry_price": self.config.max_entry_price,
            "min_entry_price": self.config.min_entry_price,
            "trend_reversal_pct": self.config.trend_reversal_pct,
            "trend_reversal_threshold": self.config.trend_reversal_threshold,
            "spike_faded_pct": self.config.spike_faded_pct,
            "spike_faded_ms": self.config.spike_faded_ms,
            "min_hold_ms": self.config.min_hold_ms,
            "trailing_stop_pct": self.config.trailing_stop_pct,
            "trailing_stop_activation": self.config.trailing_stop_activation,
            "stop_loss_pct": self.config.stop_loss_pct,
            "hold_min_share_price": self.config.hold_min_share_price,
            "hold_safety_margin": self.config.hold_safety_margin,
            "early_exit_loss_pct": self.config.early_exit_loss_pct,
            "hold_margin_per_second": self.config.hold_margin_per_second,
            "hold_max_seconds": self.config.hold_max_seconds,
            "hold_max_crossings": self.config.hold_max_crossings,
            "spike_sustain_ms": self.config.spike_sustain_ms,
            "execution_delay_ms": self.config.execution_delay_ms,
            "min_price_distance": self.config.min_price_distance,
            "max_orders_per_minute": self.config.max_orders_per_minute,
            "max_daily_loss": self.config.max_daily_loss,
            "max_exposure_per_market": self.config.max_exposure_per_market,
            "max_drawdown_pct": self.config.max_drawdown_pct,
            "trend_filter_enabled": self.config.trend_filter_enabled,
            "trend_min_magnitude_usd": self.config.trend_min_magnitude_usd,
            "counter_trend_multiplier": self.config.counter_trend_multiplier,
            "trend_max_magnitude_usd": self.config.trend_max_magnitude_usd,
            "ptb_neutral_zone_usd": self.config.ptb_neutral_zone_usd,
            "ptb_max_counter_distance_usd": self.config.ptb_max_counter_distance_usd,
        });
        self.slow_snapshot_cache.updated_at = Some(Instant::now());
        true
    }

    fn sync_loss_counters(&mut self) {
        let trade_history = self
            .live_state
            .as_ref()
            .map(|state| &state.trade_history)
            .unwrap_or(&self.wallet.trade_history);
        let start = self.processed_trade_count.min(trade_history.len());
        let new_trades = &trade_history[start..];
        for trade in new_trades {
            if trade.r#type != "exit" || trade.pnl.unwrap_or(0.0) > 0.0 {
                continue;
            }
            let direction = match trade.direction.as_str() {
                "UP" => StrategyDirection::Up,
                "DOWN" => StrategyDirection::Down,
                _ => continue,
            };
            let key = (trade.question.clone(), direction);
            *self.losing_direction_counts.entry(key).or_insert(0) += 1;
        }
        self.processed_trade_count = trade_history.len();
    }

    fn close_latency_trace(&self, symbol: &str, direction: &str) -> LatencyTrace {
        let now = Instant::now();
        LatencyTrace {
            operation: "close",
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            feed_received_at: now,
            decision_at: now,
            intent_sent_at: now,
            actor_received_at: None,
            submit_started_at: None,
            submit_finished_at: None,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Arb Engine V2 running");

        // Load saved paper wallet state
        self.wallet.load_state().await;

        // Push initial portfolio value (just cash)
        self.update_history(self.current_balance());

        let mut broadcast_timer = tokio::time::interval(Duration::from_millis(200)); // faster UI updates
        let mut maintenance_timer = tokio::time::interval(Duration::from_millis(50));
        let mut save_timer = tokio::time::interval(Duration::from_secs(30)); // save state every 30s

        loop {
            let live_event_rx = self.live_event_rx.as_mut();
            tokio::select! {
                Some(event) = async {
                    match live_event_rx {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    self.handle_live_event(event);
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd.get("_type").and_then(|v| v.as_str()) {
                        Some("settings") => {
                            let _ = self.config.update_from_json(cmd);
                            self.wallet.update_config(self.config.clone());
                            info!("Settings updated");
                        }
                        Some("start") => {
                            // Approvals are permanent on-chain (set during LiveWallet::new).
                            // Don't check here — it blocks the event loop for seconds.
                            if !self.running {
                                self.started_at = Some(Instant::now());
                            }
                            self.running = true;
                            info!("Bot started");
                        }
                        Some("stop") => {
                            self.running = false;
                            self.started_at = None;
                            self.wallet.save_state().await;
                            info!("Bot stopped");
                        }
                        Some("reset") => {
                            // Only reset paper wallet, never live
                            self.wallet.reset().await;
                            self.signals.clear(); // Clear signal terminal
                            info!("Paper wallet reset");
                        }
                        Some("close_position") => {
                            if let Some(idx) = cmd.get("index").and_then(|v| v.as_u64()) {
                                let idx = idx as usize;

                                if self.is_live_mode() {
                                    // LIVE MODE: operate directly on live wallet
                                    let live_positions = self
                                        .live_state
                                        .as_ref()
                                        .map(|lw| lw.open_positions.clone())
                                        .unwrap_or_default();
                                    if idx < live_positions.len() {
                                        let sym = live_positions[idx].symbol.clone();
                                        let dir = live_positions[idx].direction.clone();
                                        let price = self.wallet.get_share_price(&sym, &dir);
                                        if self
                                            .request_live_close(sym.clone(), dir.clone(), price, "manual")
                                            .await
                                        {
                                            info!(symbol=%sym, direction=%dir, "Manual live close requested");
                                        }
                                    } else {
                                        warn!(idx=idx, live_len=live_positions.len(), "Close button: index out of bounds for live wallet");
                                    }
                                } else {
                                    // PAPER MODE: existing logic
                                    if idx < self.wallet.open_positions.len() {
                                        let pos = self.wallet.open_positions.remove(idx);
                                        let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);
                                        let sell_fee = self.wallet.calculate_fee(pos.shares, current_price);
                                        let net_revenue = (pos.shares * current_price) - sell_fee;
                                        let pnl = net_revenue - (pos.position_size + pos.buy_fee);
                                        self.wallet.balance += net_revenue;
                                        self.wallet.trade_count += 1;
                                        if pnl > 0.0 { self.wallet.wins += 1; } else { self.wallet.losses += 1; }
                                        self.wallet.total_fees_paid += pos.buy_fee + sell_fee;
                                        self.wallet.total_volume += pos.position_size + (pos.shares * current_price);
                                        self.wallet.trade_history.push(crate::execution::paper::TradeRecord {
                                            symbol: pos.symbol.clone(),
                                            r#type: "exit".to_string(),
                                            question: String::new(),
                                            direction: pos.direction.clone(),
                                            entry_price: Some(pos.entry_price),
                                            exit_price: Some(current_price),
                                            shares: pos.shares,
                                            cost: pos.position_size,
                                            pnl: Some(pnl),
                                            cumulative_pnl: None,
                                            balance_after: None,
                                            timestamp: Self::now_rfc3339(),
                                            close_reason: Some("manual".to_string()),
                                            btc_at_entry: Some(pos.entry_btc),
                                            price_to_beat_at_entry: None,
                                            ptb_margin_at_entry: None,
                                            seconds_to_expiry_at_entry: None,
                                            spread_at_entry: None,
                                            round_trip_loss_pct_at_entry: None,
                                            signal_score: None,
                                            ptb_margin_at_exit: None,
                                            exit_mode: Some("manual".to_string()),
                                            favorable_ptb_at_exit: None,
                                            ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                                            ptb_tier_at_exit: None,
                                            entry_mode: pos.entry_mode.clone(),
                                            exit_suppressed_count: Some(pos.exit_suppressed_count),
                                        });
                                        info!(symbol=%pos.symbol, pnl=pnl, "Position manually closed");
                                    }
                                }
                                // Update chart history
                                let _bal = self.current_balance(); let _nmv = self.current_net_market_value(); self.update_history(_bal + _nmv);
                                self.broadcast_state();
                            }
                        }
                        _ => {
                            // Legacy settings format (no _type)
                            let _ = self.config.update_from_json(cmd);
                            self.wallet.update_config(self.config.clone());
                        }
                    }
                }
                _ = broadcast_timer.tick() => {
                    // Sync live wallet state from Polymarket (throttled to every 5s to avoid blocking event loop)
                    if self.is_live_mode() {
                        let should_sync = self.last_clob_sync.map_or(true, |t| t.elapsed().as_secs() >= 5);
                        if should_sync {
                            self.send_live_command(LiveCommand::SyncFromClob).await;
                            self.last_clob_sync = Some(Instant::now());
                        }
                    }

                    // Retry orphaned live positions — positions that exist in live wallet but not in paper wallet
                    // This happens when paper closes successfully but the live sell fails and pushes the position back
                    // Throttled to every 5 seconds to avoid hammering the API on repeated failures
                    if let Some(lw) = &self.live_state {
                        let orphaned: Vec<(String, String, f64)> = lw.open_positions.iter()
                            .filter(|lp| {
                                // Orphan = no matching paper position AND no matching pending entry
                                // AND position is older than 5 seconds (avoid racing with fresh entries)
                                !self.wallet.open_positions.iter().any(|pp| pp.symbol == lp.symbol && pp.direction == lp.direction)
                                && !self.wallet.pending_entries.iter().any(|pe| pe.symbol == lp.symbol && pe.direction == lp.direction)
                                && lp.entry_time.elapsed().as_secs() >= 5
                            })
                            .map(|lp| {
                                (
                                    lp.symbol.clone(),
                                    lp.direction.clone(),
                                    self.wallet.get_share_price(&lp.symbol, &lp.direction),
                                )
                            })
                            .collect();
                        // Only retry if we're on the 5-second sync boundary (same cadence as sync_from_clob)
                        if !orphaned.is_empty() && self.last_clob_sync.map_or(true, |t| t.elapsed().as_millis() < 500) {
                            for (sym, dir, price) in orphaned.into_iter().rev() {
                                info!(symbol=%sym, direction=%dir, "Retrying sell for orphaned live position");
                                self.request_live_close(sym, dir, price, "orphan_retry").await;
                            }
                        }
                    }

                    // Auto-redeem resolved markets in live mode
                    if self.is_live_mode() {
                        let now = Self::now_secs();
                        let resolved: Vec<String> = self.symbol_states.values()
                            .filter(|s| s.market_end_ts.map_or(false, |end| now > end + 30)) // 30s after end
                            .filter_map(|s| s.condition_id.clone())
                            .collect();
                        for condition_id in resolved {
                            info!(condition_id=%condition_id, "Auto-redeeming resolved market");
                            self.send_live_command(LiveCommand::RedeemResolved { condition_id }).await;
                        }
                    }
                    // Early exit for losing positions 30 seconds before market end
                    // This gives us time to exit gracefully instead of being forced out
                    let now = Self::now_secs();
                    let early_exit_needed = self.symbol_states.iter()
                        .filter(|(_, s)| s.market_end_ts.map_or(false, |end| now >= end.saturating_sub(30)))
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>();

                    for symbol in &early_exit_needed {
                        if self.is_live_mode() {
                            if let Some(lw) = &self.live_state {
                                let live_to_close: Vec<(String, String, f64)> = lw.open_positions.iter()
                                    .filter(|pos| pos.symbol == *symbol)
                                    .filter(|pos| !self.is_live_close_pending(&pos.symbol, &pos.direction))
                                    .filter_map(|pos| {
                                        let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);
                                        if current_price > self.config.hold_min_share_price {
                                            return None;
                                        }
                                        let loss_pct = (pos.entry_price - current_price) / pos.entry_price;
                                        (loss_pct > self.config.early_exit_loss_pct)
                                            .then_some((pos.symbol.clone(), pos.direction.clone(), current_price))
                                    })
                                    .collect();
                                for (symbol, direction, price) in live_to_close.into_iter().rev() {
                                    self.request_live_close(symbol, direction, price, "early_exit_losing").await;
                                }
                            }
                        } else {
                            let positions_to_close: Vec<(usize, f64)> = self.wallet.open_positions.iter()
                                .enumerate()
                                .filter(|(_, pos)| pos.symbol == *symbol)
                                .filter_map(|(idx, pos)| {
                                    let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);
                                    if current_price > self.config.hold_min_share_price {
                                        return None;
                                    }
                                    let loss_pct = (pos.entry_price - current_price) / pos.entry_price;
                                    (loss_pct > self.config.early_exit_loss_pct).then_some((idx, current_price))
                                })
                                .collect();

                            for (idx, current_price) in positions_to_close.iter().rev() {
                                let pos = self.wallet.open_positions.remove(*idx);
                                let sell_fee = self.wallet.calculate_fee(pos.shares, *current_price);
                                let net_revenue = (pos.shares * current_price) - sell_fee;
                                let pnl = net_revenue - (pos.position_size + pos.buy_fee);
                                self.wallet.balance += net_revenue;
                                self.wallet.trade_count += 1;
                                if pnl > 0.0 { self.wallet.wins += 1; } else { self.wallet.losses += 1; }
                                self.wallet.total_fees_paid += pos.buy_fee + sell_fee;
                                self.wallet.total_volume += pos.position_size + (pos.shares * current_price);
                                self.wallet.trade_history.push(crate::execution::paper::TradeRecord {
                                    symbol: pos.symbol.clone(),
                                    r#type: "exit".to_string(),
                                    question: String::new(),
                                    direction: pos.direction.clone(),
                                    entry_price: Some(pos.entry_price),
                                    exit_price: Some(*current_price),
                                    shares: pos.shares,
                                    cost: pos.position_size,
                                    pnl: Some(pnl),
                                    cumulative_pnl: None,
                                    balance_after: None,
                                    timestamp: Self::now_rfc3339(),
                                    close_reason: Some("early_exit_losing".to_string()),
                                    btc_at_entry: Some(pos.entry_btc),
                                    price_to_beat_at_entry: None,
                                    ptb_margin_at_entry: None,
                                    seconds_to_expiry_at_entry: None,
                                    spread_at_entry: None,
                                    round_trip_loss_pct_at_entry: None,
                                    signal_score: None,
                                    ptb_margin_at_exit: None,
                                    exit_mode: Some("forced".to_string()),
                                    favorable_ptb_at_exit: None,
                                    ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                                    ptb_tier_at_exit: None,
                                    entry_mode: pos.entry_mode.clone(),
                                    exit_suppressed_count: Some(pos.exit_suppressed_count),
                                });
                                info!(symbol=%pos.symbol, pnl=pnl, "Early exit for losing position before market end");
                            }
                        }
                    }

                    // Force-close all positions if any market is within 2 seconds of ending
                    let now = Self::now_secs();
                    let ending_symbols: Vec<String> = self.symbol_states.iter()
                        .filter(|(_, s)| s.market_end_ts.map_or(false, |end| now >= end.saturating_sub(2)))
                        .map(|(k, _)| k.clone())
                        .collect();
                    let market_ending = !ending_symbols.is_empty();
                    if !self.is_live_mode() && market_ending && !self.wallet.open_positions.is_empty() {
                        let indices: Vec<usize> = (0..self.wallet.open_positions.len()).rev().collect();
                        for idx in indices {
                            let pos = self.wallet.open_positions.remove(idx);
                            let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);
                            let sell_fee = self.wallet.calculate_fee(pos.shares, current_price);
                            let net_revenue = (pos.shares * current_price) - sell_fee;
                            let pnl = net_revenue - (pos.position_size + pos.buy_fee);
                            self.wallet.balance += net_revenue;
                            self.wallet.trade_count += 1;
                            if pnl > 0.0 { self.wallet.wins += 1; } else { self.wallet.losses += 1; }
                            self.wallet.total_fees_paid += pos.buy_fee + sell_fee;
                            self.wallet.total_volume += pos.position_size + (pos.shares * current_price);
                            self.wallet.trade_history.push(crate::execution::paper::TradeRecord {
                                symbol: pos.symbol.clone(),
                                r#type: "exit".to_string(),
                                question: String::new(),
                                direction: pos.direction.clone(),
                                entry_price: Some(pos.entry_price),
                                exit_price: Some(current_price),
                                shares: pos.shares,
                                cost: pos.position_size,
                                pnl: Some(pnl),
                                cumulative_pnl: None,
                                balance_after: None,
                                timestamp: Self::now_rfc3339(),
                                close_reason: Some("market_end".to_string()),
                                btc_at_entry: Some(pos.entry_btc),
                                price_to_beat_at_entry: None,
                                ptb_margin_at_entry: None,
                                seconds_to_expiry_at_entry: None,
                                spread_at_entry: None,
                                round_trip_loss_pct_at_entry: None,
                                signal_score: None,
                                ptb_margin_at_exit: None,
                                exit_mode: Some("forced".to_string()),
                                favorable_ptb_at_exit: None,
                                ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                                ptb_tier_at_exit: None,
                                entry_mode: pos.entry_mode.clone(),
                                exit_suppressed_count: Some(pos.exit_suppressed_count),
                            });
                            info!(symbol=%pos.symbol, pnl=pnl, "Position force-closed at market end");
                        }
                        let mut _net_market_value = 0.0;
                        for p in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&p.symbol, &p.direction);
                            _net_market_value += p.shares * ((b + a) / 2.0);
                        }
                        let _bal = self.current_balance(); let _nmv = self.current_net_market_value(); self.update_history(_bal + _nmv);
                    }
                    // Independent force-close for live wallet positions (catches orphaned positions)
                    if market_ending {
                        if let Some(lw) = &self.live_state {
                            let live_to_close: Vec<(String, String, f64)> = lw.open_positions.iter()
                                .filter(|pos| ending_symbols.contains(&pos.symbol))
                                .filter(|pos| !self.is_live_close_pending(&pos.symbol, &pos.direction))
                                .map(|pos| {
                                    (
                                        pos.symbol.clone(),
                                        pos.direction.clone(),
                                        self.wallet.get_share_price(&pos.symbol, &pos.direction),
                                    )
                                })
                                .collect();
                            for (symbol, direction, price) in live_to_close.into_iter().rev() {
                                self.request_live_close(symbol, direction, price, "market_end").await;
                            }
                        }
                    }
                    self.broadcast_state();
                }
                Some(update) = self.price_rx.recv() => {
                    if self.handle_price_update(update).await {
                        let mut _net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            _net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        let _bal = self.current_balance(); let _nmv = self.current_net_market_value(); self.update_history(_bal + _nmv);
                        // Reset spike gate and set cooldown so next spike triggers a fresh entry
                        for state in self.symbol_states.values_mut() {
                            state.last_spike_usd = 0.0;
                            state.last_close_time = Some(Instant::now());
                        }
                    }
                }
                Some(clob_update) = self.clob_rx.recv() => {
                    if self.handle_clob_update(clob_update).await {
                        let mut _net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            _net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        let _bal = self.current_balance(); let _nmv = self.current_net_market_value(); self.update_history(_bal + _nmv);
                        // Reset spike gate and set cooldown so next spike triggers a fresh entry
                        for state in self.symbol_states.values_mut() {
                            state.last_spike_usd = 0.0;
                            state.last_close_time = Some(Instant::now());
                        }
                    }
                }
                Some(market) = self.market_rx.recv() => { self.handle_market_update(market).await; }
                _ = maintenance_timer.tick() => {
                    self.wallet.flush_pending();
                    self.drain_wallet_events_to_signals();
                    // Clean up stale pending orders in live wallet
                    if let Some(handle) = &self.live_execution {
                        let _ = handle.try_send(LiveCommand::CleanupPendingOrders);
                    }
                }
                _ = save_timer.tick() => {
                    self.wallet.save_state().await;
                }
                else => break,
            }
        }
        Ok(())
    }

    async fn handle_price_update(&mut self, update: PriceUpdate) -> bool {
        {
            let state = self
                .symbol_states
                .entry(update.symbol.to_string())
                .or_default();
            if update.source == PriceSource::Binance {
                state.last_binance = Some((update.price, update.timestamp));
                let now = Instant::now();
                state.btc_history.push_back((update.price, now));
                state
                    .btc_history
                    .retain(|(_, t)| t.elapsed().as_millis() < 6000);

                // Update pre-computed baselines for faster spike detection
                // Find price from ~200ms ago for fast spike
                let cutoff_200ms = std::time::Duration::from_millis(200);
                state.baseline_200ms = state
                    .btc_history
                    .iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_200ms)
                    .map(|(p, _)| *p);

                // Find price from ~1s ago for slow spike confirmation
                let cutoff_1s = std::time::Duration::from_millis(1000);
                state.baseline_1s = state
                    .btc_history
                    .iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_1s)
                    .map(|(p, _)| *p);

                // Find price from ~5s ago for trend detection
                let cutoff_5s = std::time::Duration::from_millis(5000);
                state.baseline_5s = state
                    .btc_history
                    .iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_5s)
                    .map(|(p, _)| *p);

                // Decimated slow buffer: 1 sample per ~500ms, 30s window (for longer trend)
                let should_sample = state
                    .btc_history_slow
                    .back()
                    .map(|(_, t)| now.duration_since(*t) >= Duration::from_millis(500))
                    .unwrap_or(true);
                if should_sample {
                    state.btc_history_slow.push_back((update.price, now));
                    while state
                        .btc_history_slow
                        .front()
                        .map(|(_, t)| now.duration_since(*t) > Duration::from_secs(31))
                        .unwrap_or(false)
                    {
                        state.btc_history_slow.pop_front();
                    }
                }
                state.baseline_30s = state
                    .btc_history_slow
                    .front()
                    .filter(|(_, t)| now.duration_since(*t) >= Duration::from_secs(28))
                    .map(|(p, _)| *p);

                // Compute trend magnitudes
                state.trend_5s_usd = state
                    .baseline_5s
                    .map(|base| update.price - base)
                    .unwrap_or(0.0);
                state.trend_30s_usd = state
                    .baseline_30s
                    .map(|base| update.price - base)
                    .unwrap_or(0.0);
            } else {
                state.last_chainlink = Some((update.price, update.timestamp));
                // Use first Chainlink tick of new window as price to beat (fallback)
                if !state.price_to_beat_set {
                    state.price_to_beat = Some(update.price);
                    state.price_to_beat_set = true;
                    info!(
                        symbol = update.symbol,
                        price_to_beat = update.price,
                        "Price to beat set from first Chainlink tick"
                    );
                    // Update wallet with market metadata for hold mode
                    self.wallet.set_market_metadata(
                        update.symbol,
                        state.price_to_beat,
                        state.market_end_ts,
                    );
                }
            }
        }

        if update.source == PriceSource::Binance {
            let symbol = update.symbol;

            // Push BTC price to wallet for long-baseline spike calculation (every tick)
            self.wallet.push_btc_price(&symbol, update.price);

            // Update BTC trailing stop tracking for open positions
            self.wallet.update_btc_trailing(&symbol, update.price);
            if let Some(handle) = &self.live_execution {
                let _ = handle.try_send(LiveCommand::UpdateBtcTrailing {
                    symbol: symbol.to_string(),
                    price: update.price,
                });
            }

            // Compute fast spike for entry detection (using pre-computed baseline)
            let fast_spike = {
                let s = self.symbol_states.get(symbol).unwrap();
                s.baseline_200ms.map(|b| update.price - b)
            };
            if let Some(momentum) = fast_spike {
                self.wallet.push_spike_momentum(&symbol, momentum);
            }

            let (b, c) = {
                let s = self.symbol_states.get(symbol).unwrap();
                (
                    s.last_binance.map(|(p, _)| p).unwrap_or(0.0),
                    s.last_chainlink.map(|(p, _)| p).unwrap_or(0.0),
                )
            };
            if b > 0.0 && c > 0.0 {
                self.wallet.update_btc_prices(&symbol, b, c);
                // IMMEDIATE spike check on every Binance price update (no polling delay)
                if self.running {
                    self.check_for_spike(&symbol).await;
                }
            }

            let (ptb, end_ts, chainlink) = {
                let s = self.symbol_states.get(symbol);
                (
                    s.and_then(|s| s.price_to_beat),
                    s.and_then(|s| s.market_end_ts),
                    s.and_then(|s| s.last_chainlink)
                        .map(|(p, _)| p)
                        .unwrap_or(0.0),
                )
            };
            let btc_hist = self
                .symbol_states
                .get(symbol)
                .map(|s| &s.btc_history)
                .expect("symbol state missing for hold update");
            self.wallet.update_hold_status(
                symbol,
                btc_hist,
                chainlink,
                ptb,
                end_ts,
                self.config.hold_margin_per_second,
                self.config.hold_max_seconds,
                self.config.hold_max_crossings,
            );
        }

        self.wallet.flush_pending();
        self.drain_wallet_events_to_signals();
        let closed = if self.is_live_mode() {
            let intents = self.evaluate_live_exit_intents().await;
            let mut issued = false;
            for intent in intents {
                issued |= self
                    .request_live_close(
                        intent.symbol,
                        intent.direction,
                        intent.price,
                        intent.reason,
                    )
                    .await;
            }
            issued
        } else {
            self.wallet.try_close_position().await
        };
        self.drain_wallet_events_to_signals();

        closed
    }

    async fn handle_clob_update(&mut self, update: SharePriceUpdate) -> bool {
        self.wallet.update_share_price(
            &update.symbol,
            &update.direction,
            update.best_bid,
            update.best_ask,
            update.bids,
            update.asks,
        );
        // Update live wallet price cache too (matches paper.rs)
        if let Some(handle) = &self.live_execution {
            let _ = handle.try_send(LiveCommand::UpdateSharePrice {
                symbol: update.symbol.clone(),
                direction: update.direction.clone(),
                best_bid: update.best_bid,
                best_ask: update.best_ask,
            });
        }
        self.wallet.flush_pending();
        self.drain_wallet_events_to_signals();
        let closed = if self.is_live_mode() {
            let intents = self.evaluate_live_exit_intents().await;
            let mut issued = false;
            for intent in intents {
                issued |= self
                    .request_live_close(
                        intent.symbol,
                        intent.direction,
                        intent.price,
                        intent.reason,
                    )
                    .await;
            }
            issued
        } else {
            self.wallet.try_close_position().await
        };
        self.drain_wallet_events_to_signals();

        closed
    }

    pub async fn handle_market_update(&mut self, market: MarketData) {
        let _symbol_clone = market.symbol.clone();

        {
            let state = self.symbol_states.entry(market.symbol.clone()).or_default();
            state.market_end_ts = Some(market.window_end_ts);
            state.condition_id = Some(market.condition_id.clone());
            state.question = market.question.clone();
            state.price_to_beat = None;
            state.price_to_beat_set = false;
            state.btc_history.clear();
            state.baseline_200ms = None;
            state.baseline_1s = None;
            state.baseline_5s = None;
            state.btc_history_slow.clear();
            state.baseline_30s = None;
            state.trend_5s_usd = 0.0;
            state.trend_30s_usd = 0.0;
            state.spike_confirmed_since = None;
            state.last_spike_usd = 0.0;
            // Block spike detection for 3 seconds after market transition
            state.last_market_update = Some(Instant::now());
        }
        // Clear old tokens FIRST, then register new ones for live wallet
        if self.live_execution.is_some() {
            self.send_live_command(LiveCommand::ClearTokens {
                symbol: market.symbol.clone(),
            })
            .await;
            self.send_live_command(LiveCommand::ClearPriceCache {
                symbol: market.symbol.clone(),
            })
            .await;
            self.send_live_command(LiveCommand::RegisterTokens {
                up_token_id: market.up_token_id.clone(),
                down_token_id: market.down_token_id.clone(),
                symbol: market.symbol.clone(),
            })
            .await;
        }
        self.wallet.set_market_info(&market.symbol, market.question);
        self.wallet.reset_prices(&market.symbol);
    }

    async fn check_for_spike(&mut self, symbol: &str) {
        self.sync_loss_counters();

        let (binance, _chainlink) = {
            let state = self.symbol_states.get(symbol).unwrap();
            match (state.last_binance, state.last_chainlink) {
                (Some(b), Some(c)) => (b.0, c.0),
                _ => return,
            }
        };

        // Need to re-fetch state to modify it
        let state = self.symbol_states.get_mut(symbol).unwrap();

        // Block spike detection for 3 seconds after market transition
        // Prevents buying stale tokens from the old market window
        if let Some(t) = state.last_market_update {
            if t.elapsed().as_millis() < 3000 {
                return;
            }
        }

        // Fast spike detection using pre-computed baseline (no iteration)
        let fast_spike = match state.baseline_200ms {
            Some(baseline) => binance - baseline,
            None => return, // Not enough history yet
        };

        // Slow spike for confirmation (pre-computed baseline)
        let slow_spike = match state.baseline_1s {
            Some(baseline) => binance - baseline,
            None => fast_spike, // Fall back to fast if not enough history
        };

        // Use fast spike for direction/magnitude, slow spike for confirmation
        let adjusted_spike = fast_spike;
        let abs_spike = adjusted_spike.abs();
        let threshold_usd = self.config.threshold_bps as f64 / 100.0;
        let confirmed =
            slow_spike.signum() == fast_spike.signum() && slow_spike.abs() >= threshold_usd * 0.5;

        if abs_spike < threshold_usd {
            state.last_spike_usd = 0.0;
            state.spike_confirmed_since = None;
            return;
        }

        let direction = StrategyDirection::from_spike(adjusted_spike);
        let direction_sign = if adjusted_spike > 0.0 {
            1.0f64
        } else {
            -1.0f64
        };

        // Reduced sustain requirement: configurable via SPIKE_SUSTAIN_MS (default 50ms)
        // Fast enough to catch spikes early, slow enough to avoid flash reversals
        let spike_sustained = match state.spike_confirmed_since {
            Some((sign, t)) if sign == direction_sign => {
                t.elapsed().as_millis() >= self.config.spike_sustain_ms as u128
            }
            _ => {
                state.spike_confirmed_since = Some((direction_sign, Instant::now()));
                false
            }
        };

        if !spike_sustained || !confirmed {
            return;
        }

        // Extract trend values from state before any other borrows
        // These are Copy types so they don't extend the borrow
        let trend_5s = state.trend_5s_usd;
        let trend_30s = state.trend_30s_usd;
        let ptb = state.price_to_beat;

        // --- TREND-AWARE ENTRY FILTER ---
        if self.config.trend_filter_enabled {
            let spike_direction = adjusted_spike.signum(); // +1.0 = UP spike, -1.0 = DOWN spike

            // Use the stronger trend signal (5s or 30s)
            let (trend_magnitude, trend_dir) = if trend_30s.abs() > trend_5s.abs() {
                (trend_30s, trend_30s.signum())
            } else {
                (trend_5s, trend_5s.signum())
            };
            let trend_abs = trend_magnitude.abs();
            let is_counter_trend =
                spike_direction != 0.0 && trend_dir != 0.0 && spike_direction != trend_dir;

            // Gate 1: Trend-based dynamic threshold
            // If entering against the trend, require a bigger spike proportional to trend strength
            if is_counter_trend && trend_abs >= self.config.trend_min_magnitude_usd {
                let t = ((trend_abs - self.config.trend_min_magnitude_usd)
                    / (self.config.trend_max_magnitude_usd - self.config.trend_min_magnitude_usd))
                    .clamp(0.0, 1.0);
                let multiplier = 1.0 + t * (self.config.counter_trend_multiplier - 1.0);
                let required_spike = threshold_usd * multiplier;

                if abs_spike < required_spike {
                    let state = self.symbol_states.get_mut(symbol).unwrap();
                    let should_log = state
                        .last_rejection
                        .as_ref()
                        .is_none_or(|(r, t)| r != "COUNTER_TREND" || t.elapsed().as_secs() >= 3);
                    if should_log {
                        state.last_rejection = Some(("COUNTER_TREND".to_string(), Instant::now()));
                        self.add_signal(
                            symbol,
                            direction,
                            adjusted_spike,
                            SignalStatus::Rejected,
                            Some(format!(
                                "COUNTER_TREND(trend={:.0},need={:.0})",
                                trend_magnitude, required_spike
                            )),
                        );
                    }
                    return;
                }
            }

            // Gate 2: Price-to-beat distance filter
            // If BTC is far from price_to_beat in the wrong direction, reject or penalize
            if let Some(price_to_beat) = ptb {
                let ptb_distance = binance - price_to_beat; // positive = BTC above ptb
                let trade_is_up = spike_direction > 0.0;

                // Counter-PTB: buying UP when BTC is well below ptb, or DOWN when well above
                let is_counter_ptb = (trade_is_up
                    && ptb_distance < -self.config.ptb_neutral_zone_usd)
                    || (!trade_is_up && ptb_distance > self.config.ptb_neutral_zone_usd);

                if is_counter_ptb {
                    let ptb_abs = ptb_distance.abs();

                    // Hard reject: too far from ptb for this direction to win
                    if ptb_abs > self.config.ptb_max_counter_distance_usd {
                        let state = self.symbol_states.get_mut(symbol).unwrap();
                        let should_log = state
                            .last_rejection
                            .as_ref()
                            .is_none_or(|(r, t)| r != "PTB_TOO_FAR" || t.elapsed().as_secs() >= 5);
                        if should_log {
                            state.last_rejection =
                                Some(("PTB_TOO_FAR".to_string(), Instant::now()));
                            self.add_signal(
                                symbol,
                                direction,
                                adjusted_spike,
                                SignalStatus::Rejected,
                                Some(format!(
                                    "PTB_TOO_FAR(dist={:.0},max={:.0})",
                                    ptb_distance, self.config.ptb_max_counter_distance_usd
                                )),
                            );
                        }
                        return;
                    }

                    // Soft penalty: scale threshold up based on distance from ptb
                    let ptb_t = ((ptb_abs - self.config.ptb_neutral_zone_usd)
                        / (self.config.ptb_max_counter_distance_usd
                            - self.config.ptb_neutral_zone_usd))
                        .clamp(0.0, 1.0);
                    let ptb_multiplier = 1.0 + ptb_t; // 1.0 to 2.0
                    let required_spike = threshold_usd * ptb_multiplier;

                    if abs_spike < required_spike {
                        let state = self.symbol_states.get_mut(symbol).unwrap();
                        let should_log = state
                            .last_rejection
                            .as_ref()
                            .is_none_or(|(r, t)| r != "PTB_COUNTER" || t.elapsed().as_secs() >= 3);
                        if should_log {
                            state.last_rejection =
                                Some(("PTB_COUNTER".to_string(), Instant::now()));
                            self.add_signal(
                                symbol,
                                direction,
                                adjusted_spike,
                                SignalStatus::Rejected,
                                Some(format!(
                                    "PTB_COUNTER(dist={:.0},need={:.0})",
                                    ptb_distance, required_spike
                                )),
                            );
                        }
                        return;
                    }
                }
            }
        }

        let (bid, ask) = self.wallet.get_bid_ask(symbol, direction.as_str());

        if bid <= 0.0 || ask <= 0.0 {
            let state = self.symbol_states.get_mut(symbol).unwrap();
            let should_log = state.last_rejection.as_ref().map_or(true, |(r, t)| {
                r != "NO_POL_LIQUIDITY" || t.elapsed().as_secs() >= 5
            });
            if should_log {
                state.last_rejection = Some(("NO_POL_LIQUIDITY".to_string(), Instant::now()));
                self.add_signal(
                    symbol,
                    direction,
                    adjusted_spike,
                    SignalStatus::Rejected,
                    Some("NO_POL_LIQUIDITY".to_string()),
                );
            }
            return;
        }

        if let Some(end_ts) = state.market_end_ts {
            let now = Self::now_secs();
            if now >= end_ts.saturating_sub(2) {
                let state = self.symbol_states.get_mut(symbol).unwrap();
                let should_log = state.last_rejection.as_ref().map_or(true, |(r, t)| {
                    r != "MARKET_ENDING" || t.elapsed().as_secs() >= 5
                });
                if should_log {
                    state.last_rejection = Some(("MARKET_ENDING".to_string(), Instant::now()));
                    self.add_signal(
                        symbol,
                        direction,
                        adjusted_spike,
                        SignalStatus::Rejected,
                        Some("MARKET_ENDING".to_string()),
                    );
                }
                return;
            }
        }

        // Check cooldown - don't re-enter too quickly after a close
        let cooldown_ms = 3000; // 3 second cooldown after a position closes
        let cooldown_ok = state
            .last_close_time
            .map_or(true, |t| t.elapsed().as_millis() >= cooldown_ms);

        // Max 1 position at a time — don't open opposite direction while one is open
        // Prevents opening UP while DOWN is held (or vice versa), which guarantees a loss on one side
        let total_positions = self.wallet.open_positions.len() + self.wallet.pending_entries.len();
        if total_positions > 0 {
            return;
        }

        // New spike or significantly larger spike for scaling in
        if abs_spike > state.last_spike_usd * 1.1 && cooldown_ok {
            let same_market_direction_losses = self
                .losing_direction_counts
                .get(&(state.question.clone(), direction))
                .copied()
                .unwrap_or(0);
            if same_market_direction_losses >= 2 && abs_spike < threshold_usd * 1.5 {
                let state = self.symbol_states.get_mut(symbol).unwrap();
                let reason = "MARKET_DIRECTION_COOLDOWN".to_string();
                let should_log = state
                    .last_rejection
                    .as_ref()
                    .map_or(true, |(r, t)| r != &reason || t.elapsed().as_secs() >= 5);
                if should_log {
                    state.last_rejection = Some((reason.clone(), Instant::now()));
                    self.add_signal(
                        symbol,
                        direction,
                        adjusted_spike,
                        SignalStatus::Rejected,
                        Some(reason),
                    );
                }
                return;
            }

            // Get entry price BEFORE open_position (pending entry hasn't been promoted yet)
            let entry_price = self.wallet.get_share_price(symbol, direction.as_str());

            // Get current BTC price to set entry_btc immediately
            let current_btc = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);

            // Check if we're in HOLD mode (share price > threshold, < 30s remaining)
            let now_secs = Self::now_secs();
            let time_remaining = state.market_end_ts.and_then(|end| {
                if end > now_secs {
                    Some(end - now_secs)
                } else {
                    None
                }
            });
            let in_hold_mode = entry_price > self.config.hold_min_share_price
                && time_remaining.map_or(false, |t| t <= 30);

            match self.wallet.open_position(
                symbol,
                direction.as_str(),
                adjusted_spike,
                threshold_usd,
                in_hold_mode,
                current_btc,
            ) {
                Ok(level) => {
                    state.last_spike_usd = abs_spike;

                    if self.live_execution.is_some() {
                        let sym = symbol.to_string();
                        let dir = direction.as_str().to_string();
                        let spk = adjusted_spike;
                        let thresh = threshold_usd;
                        let btc = current_btc;
                        let latency_trace = LatencyTrace {
                            operation: "open",
                            symbol: sym.clone(),
                            direction: dir.clone(),
                            feed_received_at: state
                                .last_binance
                                .map(|(_, ts)| ts)
                                .unwrap_or_else(Instant::now),
                            decision_at: Instant::now(),
                            intent_sent_at: Instant::now(),
                            actor_received_at: None,
                            submit_started_at: None,
                            submit_finished_at: None,
                        };
                        self.send_live_command(LiveCommand::OpenPosition {
                            symbol: sym,
                            direction: dir,
                            spike: spk,
                            threshold_usd: thresh,
                            in_hold_mode,
                            current_btc: btc,
                            latency_trace,
                        })
                        .await;
                    } else {
                        self.add_signal(
                            symbol,
                            direction,
                            adjusted_spike,
                            SignalStatus::Executed,
                            Some(format!("LEVEL_{}", level)),
                        );
                    }
                }
                Err(reason) => {
                    let state = self.symbol_states.get_mut(symbol).unwrap();
                    let should_log = state
                        .last_rejection
                        .as_ref()
                        .map_or(true, |(r, t)| r != &reason || t.elapsed().as_secs() >= 3);
                    if should_log {
                        state.last_rejection = Some((reason.clone(), Instant::now()));
                        self.add_signal(
                            symbol,
                            direction,
                            adjusted_spike,
                            SignalStatus::Rejected,
                            Some(reason),
                        );
                    }
                }
            }
        }
    }

    async fn send_live_command(&self, mut command: LiveCommand) -> bool {
        if let LiveCommand::OpenPosition { latency_trace, .. }
        | LiveCommand::ClosePosition { latency_trace, .. } = &mut command
        {
            latency_trace.intent_sent_at = Instant::now();
        }
        if let Some(handle) = &self.live_execution {
            if let Err(err) = handle.send(command).await {
                warn!(error = %err, "Live execution command dropped");
                return false;
            }
        }
        true
    }

    fn handle_live_event(&mut self, event: LiveEvent) {
        match event {
            LiveEvent::Snapshot(snapshot) => {
                self.live_state = Some(snapshot);
                self.sync_paper_to_live_snapshot();
            }
            LiveEvent::OpenPositionResult {
                symbol,
                direction,
                spike,
                result,
                snapshot,
                latency_trace,
            } => {
                self.live_state = Some(snapshot);
                self.record_latency(LatencyKind::Open, &latency_trace, "open");
                match result {
                    Ok(fill) => {
                        self.wallet.sync_pending_to_live_fill(
                            &symbol,
                            &direction,
                            fill.position.entry_price,
                            fill.position.shares,
                            fill.position.position_size,
                            fill.position.buy_fee,
                        );
                        self.add_signal(
                            &symbol,
                            match direction.as_str() {
                                "UP" => StrategyDirection::Up,
                                "DOWN" => StrategyDirection::Down,
                                _ => StrategyDirection::Down,
                            },
                            spike,
                            SignalStatus::Executed,
                            Some(format!("LIVE_LEVEL_{}", fill.level)),
                        );
                    }
                    Err(err) => {
                        self.wallet.rollback_pending_entry(&symbol, &direction);
                        if let Some(state) = self.symbol_states.get_mut(&symbol) {
                            let live_reason = format!("LIVE:{}", err);
                            let should_log =
                                state.last_rejection.as_ref().map_or(true, |(r, t)| {
                                    r != &live_reason || t.elapsed().as_secs() >= 3
                                });
                            if should_log {
                                state.last_rejection = Some((live_reason.clone(), Instant::now()));
                                self.add_signal(
                                    &symbol,
                                    match direction.as_str() {
                                        "UP" => StrategyDirection::Up,
                                        "DOWN" => StrategyDirection::Down,
                                        _ => StrategyDirection::Down,
                                    },
                                    spike,
                                    SignalStatus::Rejected,
                                    Some(live_reason),
                                );
                            }
                        }
                    }
                }
                self.sync_paper_to_live_snapshot();
            }
            LiveEvent::ClosePositionResult {
                symbol,
                direction,
                reason,
                snapshot,
                latency_trace,
            } => {
                self.pending_live_closes
                    .remove(&(symbol.clone(), direction.clone()));
                self.live_state = Some(snapshot);
                self.record_latency(LatencyKind::Close, &latency_trace, reason);
                self.sync_paper_to_live_snapshot();
                info!(symbol = %symbol, direction = %direction, reason, "Live close result applied");
            }
        }
    }

    fn sync_paper_to_live_snapshot(&mut self) {
        let Some(live_state) = &self.live_state else {
            return;
        };

        let live_positions = &live_state.open_positions;
        let live_position_keys: HashSet<(String, String)> = live_positions
            .iter()
            .map(|pos| (pos.symbol.clone(), pos.direction.clone()))
            .collect();
        self.pending_live_closes
            .retain(|key| live_position_keys.contains(key));

        self.wallet.open_positions.retain(|paper_pos| {
            let exists_live = live_positions
                .iter()
                .any(|live_pos| live_pos.symbol == paper_pos.symbol && live_pos.direction == paper_pos.direction);
            if !exists_live {
                warn!(symbol=%paper_pos.symbol, direction=%paper_pos.direction, "Removing stale paper mirror position absent from live wallet");
            }
            exists_live
        });

        for paper_pos in &mut self.wallet.open_positions {
            if let Some(live_pos) = live_positions.iter().find(|live_pos| {
                live_pos.symbol == paper_pos.symbol && live_pos.direction == paper_pos.direction
            }) {
                paper_pos.shares = live_pos.shares;
                paper_pos.on_chain_shares = live_pos.on_chain_shares;
            }
        }
    }

    fn record_latency(&mut self, kind: LatencyKind, trace: &LatencyTrace, reason: &str) {
        let applied_at = Instant::now();
        let Some(actor_received_at) = trace.actor_received_at else {
            return;
        };
        let Some(submit_started_at) = trace.submit_started_at else {
            return;
        };
        let Some(submit_finished_at) = trace.submit_finished_at else {
            return;
        };

        let sample = LatencySample {
            total_ms: applied_at
                .duration_since(trace.feed_received_at)
                .as_millis() as u64,
            decision_ms: trace
                .decision_at
                .duration_since(trace.feed_received_at)
                .as_millis() as u64,
            queue_ms: actor_received_at
                .duration_since(trace.intent_sent_at)
                .as_millis() as u64,
            submit_ms: submit_finished_at
                .duration_since(submit_started_at)
                .as_millis() as u64,
            apply_ms: applied_at.duration_since(submit_finished_at).as_millis() as u64,
        };

        let operation = match kind {
            LatencyKind::Open => "open",
            LatencyKind::Close => "close",
        };

        info!(
            operation,
            reason,
            symbol = %trace.symbol,
            direction = %trace.direction,
            total_ms = sample.total_ms,
            decision_ms = sample.decision_ms,
            queue_ms = sample.queue_ms,
            submit_ms = sample.submit_ms,
            apply_ms = sample.apply_ms,
            "Latency trace: decision -> live result applied"
        );
        self.latency_stats.record(kind, sample);
    }
}
