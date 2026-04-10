use crate::config::AppConfig;
use crate::error::Result;
use crate::execution::PaperWallet;
use crate::polymarket::{MarketData, SharePriceUpdate};
use crate::rtds::PriceUpdate;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, broadcast};
use tracing::{debug, info, warn};
use serde_json::{json, Value};

#[derive(Default)]
struct SymbolState {
    pub last_binance: Option<(f64, Instant)>,
    pub last_chainlink: Option<(f64, Instant)>,
    pub ema_offset: f64,
    pub has_ema: bool,
    pub ema_ticks: u32,
    pub last_spike_usd: f64,
    pub market_end_ts: Option<u64>,
    pub question: String,
    pub price_to_beat: Option<f64>,
    pub price_to_beat_set: bool, // true once we've captured the opening Chainlink price
    pub btc_history: Vec<(f64, Instant)>, // (price, time) for 500ms momentum
    pub last_rejection: Option<(String, Instant)>,
    pub spike_confirmed_since: Option<(f64, Instant)>, // (direction_sign, time spike first exceeded threshold)
}

pub struct ArbEngine {
    config: AppConfig,
    price_rx: mpsc::Receiver<PriceUpdate>,
    clob_rx: mpsc::Receiver<SharePriceUpdate>,
    market_rx: mpsc::Receiver<MarketData>,
    broadcast_tx: broadcast::Sender<String>,
    cmd_rx: mpsc::Receiver<serde_json::Value>,
    pub wallet: PaperWallet,
    last_trade_time: Option<Instant>,
    symbol_states: HashMap<String, SymbolState>,
    last_clob_update_ts: Option<Instant>,
    history: Vec<Value>,
    signals: Vec<Value>,
    running: bool,
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

        Self {
            config,
            price_rx,
            clob_rx,
            market_rx,
            broadcast_tx,
            cmd_rx,
            wallet,
            last_trade_time: None,
            symbol_states: HashMap::new(),
            last_clob_update_ts: None,
            history: Vec::new(),
            signals: Vec::new(),
            running: true,
        }
    }

    fn add_signal(&mut self, symbol: &str, direction: &str, spike: f64, status: &str, reason: Option<String>) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        self.signals.push(json!({
            "t": now,
            "s": symbol,
            "d": direction,
            "v": spike,
            "st": status,
            "r": reason
        }));
        if self.signals.len() > 50 { self.signals.remove(0); }
    }

    fn update_history(&mut self, total_val: f64) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        self.history.push(json!({ "t": now, "v": total_val }));
        if self.history.len() > 1000 { self.history.remove(0); }
    }

    fn broadcast_state(&self) {
        tracing::debug!("Broadcasting state: {} markets, balance: {}", self.symbol_states.len(), self.wallet.balance);
        let mut unrealized_pnl = 0.0;
        let mut net_market_value = 0.0;
        let mut positions = Vec::new();

        for pos in &self.wallet.open_positions {
            let (up_p, down_p) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
            let current_price = (up_p + down_p) / 2.0;
            let current_value = pos.shares * current_price;
            let pnl = current_value - pos.position_size - pos.buy_fee;
            
            unrealized_pnl += pnl;
            net_market_value += current_value;

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

        let total_val = self.wallet.balance + net_market_value;
        let total_pnl = total_val - self.wallet.starting_balance;
        let realized_pnl: f64 = self.wallet.trade_history.iter()
            .filter(|t| t.r#type == "exit")
            .filter_map(|t| t.pnl)
            .sum();

        let mut markets = HashMap::new();
        for (symbol, state) in &self.symbol_states {
            let (up_b, up_a) = self.wallet.get_bid_ask(symbol, "UP");
            let (dn_b, dn_a) = self.wallet.get_bid_ask(symbol, "DOWN");

            let binance = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);
            let chainlink = state.last_chainlink.map(|(p, _)| p).unwrap_or(0.0);

            // Spike delta = Binance momentum over last 500ms
            let spike = {
                let cutoff = std::time::Duration::from_millis(500);
                let baseline = state.btc_history.iter()
                    .find(|(_, t)| t.elapsed() >= cutoff)
                    .map(|(p, _)| *p);
                match baseline { Some(b) => binance - b, None => 0.0 }
            };

            markets.insert(symbol.clone(), json!({
                "question": state.question,
                "up_price": (up_b + up_a) / 2.0,
                "down_price": (dn_b + dn_a) / 2.0,
                "binance": binance,
                "chainlink": chainlink,
                "spike": spike,
                "ema_offset": 0.0,
                "price_to_beat": state.price_to_beat,
                "end_ts": state.market_end_ts
            }));
        }

        let state = json!({
            "balance": self.wallet.balance,
            "starting_balance": self.wallet.starting_balance,
            "total_portfolio_value": total_val,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
            "total_pnl": total_pnl,
            "wins": self.wallet.wins,
            "losses": self.wallet.losses,
            "total_fees": self.wallet.total_fees_paid,
            "total_volume": self.wallet.total_volume,
            "positions": positions,
            "trades": self.wallet.trade_history,
            "history": self.history,
            "signals": self.signals,
            "markets": markets,
            "running": self.running,
            "config": {
                "threshold_bps": self.config.threshold_bps,
                "portfolio_pct": self.config.portfolio_pct,
                "profit_target_pct": self.config.profit_target_pct,
                "trailing_stop_pct": self.config.trailing_stop_pct,
                "spike_faded_pct": self.config.spike_faded_pct,
                "max_spread_bps": self.config.max_spread_bps,
                "max_entry_price": self.config.max_entry_price,
                "spike_scaling_factor": self.config.spike_scaling_factor,
                "ema_alpha": self.config.ema_alpha,
                "execution_delay_ms": self.config.execution_delay_ms,
            }
        });

        if let Ok(json_str) = serde_json::to_string(&state) {
            let _ = self.broadcast_tx.send(json_str);
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        info!("Arb Engine V2 running");

        // Push initial portfolio value (just cash)
        self.update_history(self.wallet.balance);

        let mut broadcast_timer = tokio::time::interval(Duration::from_millis(500));
        let mut spike_poll = tokio::time::interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    match cmd.get("_type").and_then(|v| v.as_str()) {
                        Some("settings") => {
                            let _ = self.config.update_from_json(cmd);
                            self.wallet.update_config(self.config.clone());
                            info!("Settings updated");
                        }
                        Some("start") => {
                            self.running = true;
                            info!("Bot started");
                        }
                        Some("stop") => {
                            self.running = false;
                            info!("Bot stopped");
                        }
                        Some("close_position") => {
                            if let Some(idx) = cmd.get("index").and_then(|v| v.as_u64()) {
                                let idx = idx as usize;
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
                                        timestamp: chrono::Local::now().to_rfc3339(),
                                        close_reason: Some("manual".to_string()),
                                    });
                                    info!(symbol=%pos.symbol, pnl=pnl, "Position manually closed");
                                    // Update chart history
                                    let mut net_market_value = 0.0;
                                    for p in &self.wallet.open_positions {
                                        let (b, a) = self.wallet.get_bid_ask(&p.symbol, &p.direction);
                                        net_market_value += p.shares * ((b + a) / 2.0);
                                    }
                                    self.update_history(self.wallet.balance + net_market_value);
                                    self.broadcast_state();
                                }
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
                    // Force-close all positions if any market is within 5 seconds of ending
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    let market_ending = self.symbol_states.values()
                        .any(|s| s.market_end_ts.map_or(false, |end| now >= end.saturating_sub(5)));
                    if market_ending && !self.wallet.open_positions.is_empty() {
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
                                timestamp: chrono::Local::now().to_rfc3339(),
                                close_reason: Some("market_end".to_string()),
                            });
                            info!(symbol=%pos.symbol, pnl=pnl, "Position force-closed at market end");
                        }
                        let mut net_market_value = 0.0;
                        for p in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&p.symbol, &p.direction);
                            net_market_value += p.shares * ((b + a) / 2.0);
                        }
                        self.update_history(self.wallet.balance + net_market_value);
                    }
                    self.broadcast_state();
                }
                Some(update) = self.price_rx.recv() => {
                    if self.handle_price_update(update).await {
                        let mut net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        self.update_history(self.wallet.balance + net_market_value);
                        // Reset spike gate so next spike triggers a fresh entry
                        for state in self.symbol_states.values_mut() { state.last_spike_usd = 0.0; }
                    }
                }
                Some(clob_update) = self.clob_rx.recv() => {
                    if self.handle_clob_update(clob_update).await {
                        let mut net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        self.update_history(self.wallet.balance + net_market_value);
                        // Reset spike gate so next spike triggers a fresh entry
                        for state in self.symbol_states.values_mut() { state.last_spike_usd = 0.0; }
                    }
                }
                Some(market) = self.market_rx.recv() => { self.handle_market_update(market).await; }
                _ = spike_poll.tick() => {
                    if self.running {
                        let symbols: Vec<String> = self.symbol_states.keys().cloned().collect();
                        for symbol in symbols {
                            self.check_for_spike(&symbol).await;
                        }
                    }
                    self.wallet.flush_pending();
                }
                else => break,
            }
        }
        Ok(())
    }

    async fn handle_price_update(&mut self, update: PriceUpdate) -> bool {
        {
            let state = self.symbol_states.entry(update.symbol.clone()).or_default();
            if update.source == "binance" {
                state.last_binance = Some((update.price, update.timestamp));
                state.btc_history.push((update.price, Instant::now()));
                // Keep only last 2 seconds of history
                state.btc_history.retain(|(_, t)| t.elapsed().as_millis() < 2000);
            } else {
                state.last_chainlink = Some((update.price, update.timestamp));
                state.ema_ticks += 1;
                if !state.price_to_beat_set {
                    state.price_to_beat = Some(update.price);
                    state.price_to_beat_set = true;
                    info!(symbol=%update.symbol, price_to_beat=update.price, "Price to beat set from first Chainlink tick");
                }
            }
        }

        let state = self.symbol_states.get(&update.symbol).unwrap();
        if let (Some((b, _)), Some((c, _))) = (state.last_binance, state.last_chainlink) {
            self.wallet.update_btc_prices(&update.symbol, b, c);
            if self.running { self.check_for_spike(&update.symbol).await; }
        }

        if update.source == "binance" {
            let symbol = update.symbol.clone();
            let (ptb, end_ts, btc_hist) = {
                let s = self.symbol_states.get(&symbol);
                (
                    s.and_then(|s| s.price_to_beat),
                    s.and_then(|s| s.market_end_ts),
                    s.map(|s| s.btc_history.iter().map(|(p,_)| *p).collect::<Vec<_>>()).unwrap_or_default(),
                )
            };
            self.wallet.update_hold_status(
                &symbol, &btc_hist, ptb, end_ts,
                self.config.hold_margin_per_second,
                self.config.hold_max_seconds,
                self.config.hold_max_crossings,
            );
        }

        self.wallet.flush_pending();
        self.wallet.try_close_position().await
    }

    async fn handle_clob_update(&mut self, update: SharePriceUpdate) -> bool {
        self.last_clob_update_ts = Some(Instant::now());
        self.wallet.update_share_price(&update.symbol, &update.direction, update.best_bid, update.best_ask);
        self.wallet.flush_pending();
        let closed = self.wallet.try_close_position().await;
        if self.running { self.check_for_spike(&update.symbol).await; }
        closed
    }

    pub async fn handle_market_update(&mut self, market: MarketData) {
        let state = self.symbol_states.entry(market.symbol.clone()).or_default();
        state.market_end_ts = Some(market.window_end_ts);
        state.question = market.question.clone();
        // Parse price to beat from question e.g. "Will BTC be above $83,450.00 at 14:35?"
        state.price_to_beat = None; // will be set from first Chainlink tick of new window
        state.price_to_beat_set = false;
        state.btc_history.clear();
        self.wallet.set_market_info(&market.symbol, market.question);
        self.wallet.reset_prices(&market.symbol);
    }

    async fn check_for_spike(&mut self, symbol: &str) {
        let (binance, chainlink) = {
            let state = self.symbol_states.get(symbol).unwrap();
            match (state.last_binance, state.last_chainlink) {
                (Some(b), Some(c)) => (b.0, c.0),
                _ => return,
            }
        };

        // Need to re-fetch state to modify it
        let state = self.symbol_states.get_mut(symbol).unwrap();

        // Dual-window spike detection:
        // Fast (200ms): detects spike early, starts sustain timer
        // Slow (1000ms): confirms spike is real and not a flash reversal
        let fast_spike = {
            let cutoff = std::time::Duration::from_millis(200);
            let baseline = state.btc_history.iter()
                .find(|(_, t)| t.elapsed() >= cutoff)
                .map(|(p, _)| *p);
            match baseline { Some(b) => binance - b, None => return }
        };

        let slow_spike = {
            let cutoff = std::time::Duration::from_millis(1000);
            state.btc_history.iter()
                .find(|(_, t)| t.elapsed() >= cutoff)
                .map(|(p, _)| binance - p)
                .unwrap_or(fast_spike) // fall back to fast if not enough history
        };

        // Use fast spike for direction/magnitude, slow spike for confirmation
        let adjusted_spike = fast_spike;
        let abs_spike = adjusted_spike.abs();
        let threshold_usd = self.config.threshold_bps as f64 / 100.0;
        let confirmed = slow_spike.signum() == fast_spike.signum() && slow_spike.abs() >= threshold_usd * 0.5;

        if abs_spike < threshold_usd { 
            state.last_spike_usd = 0.0;
            state.spike_confirmed_since = None;
            return; 
        }

        let direction = if adjusted_spike > 0.0 { "UP" } else { "DOWN" };
        let direction_sign = if adjusted_spike > 0.0 { 1.0f64 } else { -1.0f64 };

        // Require spike to be sustained for 200ms in the same direction before entering
        let spike_sustained = match state.spike_confirmed_since {
            Some((sign, t)) if sign == direction_sign => t.elapsed().as_millis() >= 300,
            _ => {
                state.spike_confirmed_since = Some((direction_sign, Instant::now()));
                false
            }
        };

        if !spike_sustained || !confirmed {
            return;
        }
        let (bid, ask) = self.wallet.get_bid_ask(symbol, direction);
        
        if bid <= 0.0 || ask <= 0.0 {
            let state = self.symbol_states.get_mut(symbol).unwrap();
            let should_log = state.last_rejection.as_ref()
                .map_or(true, |(r, t)| r != "NO_POL_LIQUIDITY" || t.elapsed().as_secs() >= 5);
            if should_log {
                state.last_rejection = Some(("NO_POL_LIQUIDITY".to_string(), Instant::now()));
                self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some("NO_POL_LIQUIDITY".to_string()));
            }
            return;
        }
        
        if let Some(end_ts) = state.market_end_ts {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            if now >= end_ts.saturating_sub(5) {
                let state = self.symbol_states.get_mut(symbol).unwrap();
                let should_log = state.last_rejection.as_ref()
                    .map_or(true, |(r, t)| r != "MARKET_ENDING" || t.elapsed().as_secs() >= 5);
                if should_log {
                    state.last_rejection = Some(("MARKET_ENDING".to_string(), Instant::now()));
                    self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some("MARKET_ENDING".to_string()));
                }
                return;
            }
        }

        // New spike or significantly larger spike for scaling in
        if abs_spike > state.last_spike_usd * 1.1 {
            let spread_bps = (((ask - bid) / ((bid + ask) / 2.0)) * 10000.0) as u64;
            let ema_offset = 0.0;
            match self.wallet.open_position(symbol, direction, binance, chainlink, adjusted_spike, ema_offset, spread_bps, threshold_usd) {
                Ok(level) => {
                    let level_str = format!("LEVEL_{}", level);
                    state.last_spike_usd = abs_spike;
                    self.add_signal(symbol, direction, adjusted_spike, "EXECUTED", Some(level_str));
                }
                Err(reason) => {
                    let state = self.symbol_states.get_mut(symbol).unwrap();
                    let should_log = state.last_rejection.as_ref()
                        .map_or(true, |(r, t)| r != &reason || t.elapsed().as_secs() >= 3);
                    if should_log {
                        state.last_rejection = Some((reason.clone(), Instant::now()));
                        self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some(reason));
                    }
                }
            }
        }
    }
}
