use crate::config::AppConfig;
use crate::error::Result;
use crate::execution::PaperWallet;
use crate::execution::LiveWallet;
use crate::polymarket::{MarketData, SharePriceUpdate};
use crate::rtds::PriceUpdate;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, broadcast};
use tracing::{info, warn};
use serde_json::{json, Value};

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
    pub btc_history: Vec<(f64, Instant)>,
    pub last_rejection: Option<(String, Instant)>,
    pub spike_confirmed_since: Option<(f64, Instant)>,
    // Pre-computed baselines for faster spike detection
    pub baseline_200ms: Option<f64>,
    pub baseline_1s: Option<f64>,
}

pub struct ArbEngine {
    config: AppConfig,
    price_rx: mpsc::Receiver<PriceUpdate>,
    clob_rx: mpsc::Receiver<SharePriceUpdate>,
    market_rx: mpsc::Receiver<MarketData>,
    broadcast_tx: broadcast::Sender<String>,
    cmd_rx: mpsc::Receiver<serde_json::Value>,
    pub wallet: PaperWallet,
    pub live_wallet: Option<LiveWallet>,
    symbol_states: HashMap<String, SymbolState>,
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
            live_wallet: None,
            symbol_states: HashMap::new(),
            signals: Vec::new(),
            running: false, // starts paused — user must press START
        }
    }

    fn current_balance(&self) -> f64 {
        self.live_wallet.as_ref().map(|lw| lw.balance).unwrap_or(self.wallet.balance)
    }

    fn current_net_market_value(&self) -> f64 {
        let positions = self.live_wallet.as_ref()
            .map(|lw| &lw.open_positions)
            .unwrap_or(&self.wallet.open_positions);
        positions.iter().map(|pos| {
            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
            pos.shares * ((b + a) / 2.0)
        }).sum()
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
        self.wallet.push_history(total_val);
    }

    fn broadcast_state(&self) {
        let (balance, open_positions_ref, trade_history_ref, wins, losses, fees, volume, starting_balance) =
            if let Some(lw) = &self.live_wallet {
                (&lw.balance, &lw.open_positions, &lw.trade_history, lw.wins, lw.losses, lw.total_fees_paid, lw.total_volume, lw.starting_balance)
            } else {
                (&self.wallet.balance, &self.wallet.open_positions, &self.wallet.trade_history, self.wallet.wins, self.wallet.losses, self.wallet.total_fees_paid, self.wallet.total_volume, self.wallet.starting_balance)
            };

        let mut unrealized_pnl = 0.0;
        let mut _net_market_value = 0.0;
        let mut positions = Vec::new();

        for pos in open_positions_ref {
            let (up_p, down_p) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
            let current_price = (up_p + down_p) / 2.0;
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
        let realized_pnl: f64 = trade_history_ref.iter()
            .filter(|t| t.r#type == "exit")
            .filter_map(|t| t.pnl)
            .sum();

        let wallet_address = self.live_wallet.as_ref()
            .map(|_| std::env::var("POLYMARKET_PRIVATE_KEY").ok()
                .and_then(|pk| {
                    use std::str::FromStr;
                    polymarket_client_sdk::auth::LocalSigner::<k256::ecdsa::SigningKey>::from_str(&pk).ok()
                        .map(|s| format!("{:?}", polymarket_client_sdk::auth::Signer::address(&s)))
                })
                .unwrap_or_default()
            );

        let mut markets = HashMap::new();
        for (symbol, state) in &self.symbol_states {
            let (up_b, up_a) = self.wallet.get_bid_ask(symbol, "UP");
            let (dn_b, dn_a) = self.wallet.get_bid_ask(symbol, "DOWN");

            let binance = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);
            let chainlink = state.last_chainlink.map(|(p, _)| p).unwrap_or(0.0);

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
            "balance": balance,
            "starting_balance": starting_balance,
            "total_portfolio_value": total_val,
            "unrealized_pnl": unrealized_pnl,
            "realized_pnl": realized_pnl,
            "total_pnl": total_pnl,
            "cumulative_pnl": self.live_wallet.as_ref().map(|lw| lw.cumulative_pnl).unwrap_or(0.0),
            "daily_pnl": self.live_wallet.as_ref().map(|lw| lw.daily_pnl).unwrap_or(0.0),
            "wins": wins,
            "losses": losses,
            "total_fees": fees,
            "total_volume": volume,
            "positions": positions,
            "trades": trade_history_ref,
            "history": self.wallet.history,
            "signals": self.signals,
            "markets": markets,
            "wallet_address": wallet_address,
            "running": self.running,
            "is_live": self.live_wallet.is_some(),
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

        // Load saved paper wallet state
        self.wallet.load_state();

        // Push initial portfolio value (just cash)
        self.update_history(self.current_balance());

        let mut broadcast_timer = tokio::time::interval(Duration::from_millis(200)); // faster UI updates
        let mut spike_poll = tokio::time::interval(Duration::from_millis(50)); // faster spike detection
        let mut save_timer = tokio::time::interval(Duration::from_secs(30)); // save state every 30s

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
                            if let Some(lw) = &self.live_wallet {
                                info!("Running on-chain approval check before starting live trading...");
                                match lw.ensure_approvals().await {
                                    Ok(_) => info!("Approvals confirmed"),
                                    Err(e) => warn!("Approval check failed (continuing anyway): {}", e),
                                }
                            }
                            self.running = true;
                            info!("Bot started");
                        }
                        Some("stop") => {
                            self.running = false;
                            self.wallet.save_state();
                            info!("Bot stopped");
                        }
                        Some("reset") => {
                            // Only reset paper wallet, never live
                            self.wallet.reset();
                            info!("Paper wallet reset");
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
                                        cumulative_pnl: None,
                                        balance_after: None,
                                        timestamp: chrono::Local::now().to_rfc3339(),
                                        close_reason: Some("manual".to_string()),
                                    });
                                    info!(symbol=%pos.symbol, pnl=pnl, "Position manually closed");
                                    // Mirror to live wallet
                                    if let Some(lw) = &mut self.live_wallet {
                                        if let Some(live_idx) = lw.open_positions.iter().position(|p| p.symbol == pos.symbol && p.direction == pos.direction) {
                                            lw.close_position_at(live_idx, current_price, "manual").await;
                                        }
                                    }
                                    // Update chart history
                                    let mut _net_market_value = 0.0;
                                    for p in &self.wallet.open_positions {
                                        let (b, a) = self.wallet.get_bid_ask(&p.symbol, &p.direction);
                                        _net_market_value += p.shares * ((b + a) / 2.0);
                                    }
                                    let _bal = self.current_balance(); let _nmv = self.current_net_market_value(); self.update_history(_bal + _nmv);
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
                    // Sync live wallet state from Polymarket (ground truth)
                    if let Some(lw) = &mut self.live_wallet {
                        lw.sync_from_clob().await;
                    }

                    // Auto-redeem resolved markets in live mode
                    if self.live_wallet.is_some() {
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                        let resolved: Vec<String> = self.symbol_states.values()
                            .filter(|s| s.market_end_ts.map_or(false, |end| now > end + 30)) // 30s after end
                            .filter_map(|s| s.condition_id.clone())
                            .collect();
                        for condition_id in resolved {
                            if let Some(lw) = &self.live_wallet {
                                info!(condition_id=%condition_id, "Auto-redeeming resolved market");
                                lw.redeem_resolved_positions(&condition_id).await;
                            }
                        }
                    }
                    // Force-close all positions if any market is within 5 seconds of ending
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    let market_ending = self.symbol_states.values()
                        .any(|s| s.market_end_ts.map_or(false, |end| now >= end.saturating_sub(5)));
                    if market_ending && !self.wallet.open_positions.is_empty() {
                        let indices: Vec<usize> = (0..self.wallet.open_positions.len()).rev().collect();
                        for idx in indices {
                            let pos = self.wallet.open_positions.remove(idx);
                            let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);

                            // Mirror to live wallet
                            if let Some(lw) = &mut self.live_wallet {
                                if let Some(live_idx) = lw.open_positions.iter().position(|p| p.symbol == pos.symbol && p.direction == pos.direction) {
                                    lw.close_position_at(live_idx, current_price, "market_end").await;
                                }
                            }
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
                                timestamp: chrono::Local::now().to_rfc3339(),
                                close_reason: Some("market_end".to_string()),
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
                        // Reset spike gate so next spike triggers a fresh entry
                        for state in self.symbol_states.values_mut() { state.last_spike_usd = 0.0; }
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
                _ = save_timer.tick() => {
                    self.wallet.save_state();
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
                let now = Instant::now();
                state.btc_history.push((update.price, now));
                state.btc_history.retain(|(_, t)| t.elapsed().as_millis() < 2000);
                
                // Update pre-computed baselines for faster spike detection
                // Find price from ~200ms ago for fast spike
                let cutoff_200ms = std::time::Duration::from_millis(200);
                state.baseline_200ms = state.btc_history.iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_200ms)
                    .map(|(p, _)| *p);
                
                // Find price from ~1s ago for slow spike confirmation
                let cutoff_1s = std::time::Duration::from_millis(1000);
                state.baseline_1s = state.btc_history.iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_1s)
                    .map(|(p, _)| *p);
            } else {
                state.last_chainlink = Some((update.price, update.timestamp));
                // Use first Chainlink tick of new window as price to beat (fallback)
                if !state.price_to_beat_set {
                    state.price_to_beat = Some(update.price);
                    state.price_to_beat_set = true;
                    info!(symbol=%update.symbol, price_to_beat=update.price, "Price to beat set from first Chainlink tick");
                }
            }
        }

        if update.source == "binance" {
            let symbol = update.symbol.clone();

            // Push BTC price to wallet for long-baseline spike calculation (every tick)
            self.wallet.push_btc_price(&symbol, update.price);

            // Compute fast spike for entry detection (using pre-computed baseline)
            let fast_spike = {
                let s = self.symbol_states.get(&symbol).unwrap();
                s.baseline_200ms.map(|b| update.price - b)
            };
            if let Some(momentum) = fast_spike {
                self.wallet.push_spike_momentum(&symbol, momentum);
            }

            let (b, c) = {
                let s = self.symbol_states.get(&symbol).unwrap();
                (s.last_binance.map(|(p,_)| p).unwrap_or(0.0),
                 s.last_chainlink.map(|(p,_)| p).unwrap_or(0.0))
            };
            if b > 0.0 && c > 0.0 {
                self.wallet.update_btc_prices(&symbol, b, c);
                // IMMEDIATE spike check on every Binance price update (no polling delay)
                if self.running { self.check_for_spike(&symbol).await; }
            }

            let (ptb, end_ts, btc_hist, chainlink) = {
                let s = self.symbol_states.get(&symbol);
                (
                    s.and_then(|s| s.price_to_beat),
                    s.and_then(|s| s.market_end_ts),
                    s.map(|s| s.btc_history.iter().map(|(p,_)| *p).collect::<Vec<_>>()).unwrap_or_default(),
                    s.and_then(|s| s.last_chainlink).map(|(p,_)| p).unwrap_or(0.0),
                )
            };
            self.wallet.update_hold_status(
                &symbol, &btc_hist, chainlink, ptb, end_ts,
                self.config.hold_margin_per_second,
                self.config.hold_max_seconds,
                self.config.hold_max_crossings,
            );
        }

        self.wallet.flush_pending();
        let closed = self.wallet.try_close_position().await;

        // Mirror closes to live wallet
        if closed && self.live_wallet.is_some() {
            // Find positions that were just closed (in trade_history but not in open_positions)
            let recent_exits: Vec<_> = self.wallet.trade_history.iter().rev()
                .take_while(|t| t.r#type == "exit")
                .filter(|t| {
                    // Only exits from last 500ms
                    chrono::Local::now().timestamp_millis() -
                        chrono::DateTime::parse_from_rfc3339(&t.timestamp)
                            .map(|d| d.timestamp_millis()).unwrap_or(0) < 500
                })
                .map(|t| (t.symbol.clone(), t.direction.clone(), t.exit_price.unwrap_or(0.0), t.close_reason.clone()))
                .collect();

            for (sym, dir, price, reason) in recent_exits {
                if let Some(lw) = &mut self.live_wallet {
                    // Find matching position index in live wallet
                    if let Some(idx) = lw.open_positions.iter().position(|p| p.symbol == sym && p.direction == dir) {
                        let reason_str = reason.as_deref().unwrap_or("exit");
                        // Convert to &'static str for the interface
                        let r: &'static str = match reason_str {
                            "trailing_stop" => "trailing_stop",
                            "trend_reversed" => "trend_reversed",
                            "spike_faded" => "spike_faded",
                            "near_end" => "near_end",
                            "market_end" => "market_end",
                            "manual" => "manual",
                            _ => "exit",
                        };
                        lw.close_position_at(idx, price, r).await;
                    }
                }
            }
        }

        closed
    }

    async fn handle_clob_update(&mut self, update: SharePriceUpdate) -> bool {
        self.wallet.update_share_price(&update.symbol, &update.direction, update.best_bid, update.best_ask);
        self.wallet.flush_pending();
        self.wallet.try_close_position().await
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
        }
        // Register token IDs in live wallet so it can place orders
        if let Some(lw) = &mut self.live_wallet {
            lw.register_tokens(&market.up_token_id, &market.down_token_id, &market.symbol);
        }
        self.wallet.set_market_info(&market.symbol, market.question);
        self.wallet.reset_prices(&market.symbol);
    }

    async fn check_for_spike(&mut self, symbol: &str) {
        let (binance, _chainlink) = {
            let state = self.symbol_states.get(symbol).unwrap();
            match (state.last_binance, state.last_chainlink) {
                (Some(b), Some(c)) => (b.0, c.0),
                _ => return,
            }
        };

        // Need to re-fetch state to modify it
        let state = self.symbol_states.get_mut(symbol).unwrap();

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
        let confirmed = slow_spike.signum() == fast_spike.signum() && slow_spike.abs() >= threshold_usd * 0.5;

        if abs_spike < threshold_usd { 
            state.last_spike_usd = 0.0;
            state.spike_confirmed_since = None;
            return; 
        }

        let direction = if adjusted_spike > 0.0 { "UP" } else { "DOWN" };
        let direction_sign = if adjusted_spike > 0.0 { 1.0f64 } else { -1.0f64 };

        // Reduced sustain requirement: configurable via SPIKE_SUSTAIN_MS (default 50ms)
        // Fast enough to catch spikes early, slow enough to avoid flash reversals
        let spike_sustained = match state.spike_confirmed_since {
            Some((sign, t)) if sign == direction_sign => t.elapsed().as_millis() >= self.config.spike_sustain_ms as u128,
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
            // Get entry price BEFORE open_position (pending entry hasn't been promoted yet)
            let entry_price = self.wallet.get_share_price(symbol, direction);
            match self.wallet.open_position(symbol, direction, adjusted_spike, threshold_usd) {
                Ok(level) => {
                    state.last_spike_usd = abs_spike;

                    if self.live_wallet.is_some() {
                        let sym = symbol.to_string();
                        let dir = direction.to_string();
                        let spk = adjusted_spike;
                        let end_ts = state.market_end_ts;
                        if let Some(lw) = &mut self.live_wallet {
                            match lw.open_position(&sym, &dir, spk, entry_price, end_ts).await {
                                Ok(live_level) => {
                                    self.add_signal(&sym, &dir, spk, "EXECUTED", Some(format!("LIVE_LEVEL_{}", live_level)));
                                }
                                Err(e) => {
                                    self.add_signal(&sym, &dir, spk, "REJECTED", Some(format!("LIVE:{}", e)));
                                }
                            }
                        }
                    } else {
                        self.add_signal(symbol, direction, adjusted_spike, "EXECUTED", Some(format!("LEVEL_{}", level)));
                    }
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
