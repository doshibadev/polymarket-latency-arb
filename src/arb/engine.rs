use crate::config::AppConfig;
use crate::error::Result;
use crate::execution::PaperWallet;
use crate::execution::LiveWallet;
use crate::polymarket::{MarketData, SharePriceUpdate};
use crate::rtds::PriceUpdate;
use std::collections::{HashMap, VecDeque};
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
    broadcast_tx: broadcast::Sender<String>,
    cmd_rx: mpsc::Receiver<serde_json::Value>,
    pub wallet: PaperWallet,
    pub live_wallet: Option<LiveWallet>,
    symbol_states: HashMap<String, SymbolState>,
    signals: Vec<Value>,
    running: bool,
    last_clob_sync: Option<Instant>,
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
            last_clob_sync: None,
        }
    }

    fn current_balance(&self) -> f64 {
        self.live_wallet.as_ref().map(|lw| lw.balance).unwrap_or(self.wallet.balance)
    }

    fn current_net_market_value(&self) -> f64 {
        let positions = self.live_wallet.as_ref()
            .map(|lw| &lw.open_positions)
            .unwrap_or(&self.wallet.open_positions);
        
        let is_live = self.live_wallet.is_some();
        
        positions.iter().map(|pos| {
            let (b, a) = if is_live {
                if let Some(lw) = &self.live_wallet {
                    lw.get_bid_ask(&pos.symbol, &pos.direction)
                } else {
                    (0.0, 0.0)
                }
            } else {
                self.wallet.get_bid_ask(&pos.symbol, &pos.direction)
            };
            let price = if b > 0.0 && a > 0.0 { (b + a) / 2.0 } else { pos.entry_price };
            pos.shares * price
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

    /// Helper to get current unix timestamp in seconds
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn broadcast_state(&self) {
        let (balance, open_positions_ref, trade_history_ref, wins, losses, fees, volume, starting_balance, is_live) =
            if let Some(lw) = &self.live_wallet {
                (&lw.balance, &lw.open_positions, &lw.trade_history, lw.wins, lw.losses, lw.total_fees_paid, lw.total_volume, lw.starting_balance, true)
            } else {
                (&self.wallet.balance, &self.wallet.open_positions, &self.wallet.trade_history, self.wallet.wins, self.wallet.losses, self.wallet.total_fees_paid, self.wallet.total_volume, self.wallet.starting_balance, false)
            };

        let mut unrealized_pnl = 0.0;
        let mut _net_market_value = 0.0;
        let mut positions = Vec::new();

        for pos in open_positions_ref {
            // Use live wallet's price cache for live positions, paper wallet's cache for paper
            let (up_p, down_p) = if is_live {
                if let Some(lw) = &self.live_wallet {
                    lw.get_bid_ask(&pos.symbol, &pos.direction)
                } else {
                    (0.0, 0.0)
                }
            } else {
                self.wallet.get_bid_ask(&pos.symbol, &pos.direction)
            };
            
            let current_price = if up_p > 0.0 && down_p > 0.0 { (up_p + down_p) / 2.0 } else { pos.entry_price };
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
            "history": if is_live {
                // Live mode: build chart from trade history (only updates on closed trades)
                let mut chart_points = Vec::new();
                chart_points.push(json!({ "t": "start", "v": starting_balance }));
                let mut running_value = starting_balance;
                for trade in trade_history_ref.iter().filter(|t| t.r#type == "exit") {
                    if let Some(pnl) = trade.pnl {
                        running_value += pnl;
                        chart_points.push(json!({
                            "t": &trade.timestamp[11..19], // extract HH:MM:SS
                            "v": running_value
                        }));
                    }
                }
                json!(chart_points)
            } else {
                json!(self.wallet.history)
            },
            "signals": self.signals,
            "markets": markets,
            "wallet_address": wallet_address,
            "running": self.running,
            "is_live": self.live_wallet.is_some(),
            "config": {
                "threshold_bps": self.config.threshold_bps,
                "portfolio_pct": self.config.portfolio_pct,
                "max_entry_price": self.config.max_entry_price,
                "min_entry_price": self.config.min_entry_price,
                "trend_reversal_pct": self.config.trend_reversal_pct,
                "spike_faded_pct": self.config.spike_faded_pct,
                "spike_faded_ms": self.config.spike_faded_ms,
                "min_hold_ms": self.config.min_hold_ms,
                "execution_delay_ms": self.config.execution_delay_ms,
                "max_orders_per_minute": self.config.max_orders_per_minute,
                "max_daily_loss": self.config.max_daily_loss,
                "max_exposure_per_market": self.config.max_exposure_per_market,
                "max_drawdown_pct": self.config.max_drawdown_pct,
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
                            // Approvals are permanent on-chain (set during LiveWallet::new).
                            // Don't check here — it blocks the event loop for seconds.
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
                            self.signals.clear(); // Clear signal terminal
                            info!("Paper wallet reset");
                        }
                        Some("close_position") => {
                            if let Some(idx) = cmd.get("index").and_then(|v| v.as_u64()) {
                                let idx = idx as usize;

                                if self.live_wallet.is_some() {
                                    // LIVE MODE: operate directly on live wallet
                                    // Dashboard shows lw.open_positions, so the index maps to live wallet
                                    let lw = self.live_wallet.as_mut().unwrap();
                                    if idx < lw.open_positions.len() {
                                        let sym = lw.open_positions[idx].symbol.clone();
                                        let dir = lw.open_positions[idx].direction.clone();
                                        let price = lw.get_share_price(&sym, &dir);
                                        lw.close_position_at(idx, price, "manual").await;

                                        // Also remove matching paper position if it exists
                                        if let Some(paper_idx) = self.wallet.open_positions.iter()
                                            .position(|p| p.symbol == sym && p.direction == dir)
                                        {
                                            let pos = self.wallet.open_positions.remove(paper_idx);
                                            let paper_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);
                                            let sell_fee = self.wallet.calculate_fee(pos.shares, paper_price);
                                            let net_revenue = (pos.shares * paper_price) - sell_fee;
                                            let pnl = net_revenue - (pos.position_size + pos.buy_fee);
                                            self.wallet.balance += net_revenue;
                                            self.wallet.trade_count += 1;
                                            if pnl > 0.0 { self.wallet.wins += 1; } else { self.wallet.losses += 1; }
                                            self.wallet.total_fees_paid += pos.buy_fee + sell_fee;
                                            self.wallet.total_volume += pos.position_size + (pos.shares * paper_price);
                                            self.wallet.trade_history.push(crate::execution::paper::TradeRecord {
                                                symbol: pos.symbol.clone(), r#type: "exit".to_string(),
                                                question: String::new(), direction: pos.direction.clone(),
                                                entry_price: Some(pos.entry_price), exit_price: Some(paper_price),
                                                shares: pos.shares, cost: pos.position_size, pnl: Some(pnl),
                                                cumulative_pnl: None, balance_after: None,
                                                timestamp: chrono::Local::now().to_rfc3339(),
                                                close_reason: Some("manual".to_string()),
                                            });
                                        }
                                        info!(symbol=%sym, direction=%dir, "Position manually closed (live mode)");
                                    } else {
                                        warn!(idx=idx, live_len=lw.open_positions.len(), "Close button: index out of bounds for live wallet");
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
                                            timestamp: chrono::Local::now().to_rfc3339(),
                                            close_reason: Some("manual".to_string()),
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
                    if let Some(lw) = &mut self.live_wallet {
                        let should_sync = self.last_clob_sync.map_or(true, |t| t.elapsed().as_secs() >= 5);
                        if should_sync {
                            lw.sync_from_clob().await;
                            self.last_clob_sync = Some(Instant::now());
                        }
                    }

                    // Retry orphaned live positions — positions that exist in live wallet but not in paper wallet
                    // This happens when paper closes successfully but the live sell fails and pushes the position back
                    // Throttled to every 5 seconds to avoid hammering the API on repeated failures
                    if let Some(lw) = &mut self.live_wallet {
                        let orphaned: Vec<usize> = lw.open_positions.iter()
                            .enumerate()
                            .filter(|(_, lp)| {
                                // Orphan = no matching paper position AND no matching pending entry
                                // AND position is older than 5 seconds (avoid racing with fresh entries)
                                !self.wallet.open_positions.iter().any(|pp| pp.symbol == lp.symbol && pp.direction == lp.direction)
                                && !self.wallet.pending_entries.iter().any(|pe| pe.symbol == lp.symbol && pe.direction == lp.direction)
                                && lp.entry_time.elapsed().as_secs() >= 5
                            })
                            .map(|(idx, _)| idx)
                            .collect();
                        // Only retry if we're on the 5-second sync boundary (same cadence as sync_from_clob)
                        if !orphaned.is_empty() && self.last_clob_sync.map_or(true, |t| t.elapsed().as_millis() < 500) {
                            for idx in orphaned.into_iter().rev() {
                                let sym = lw.open_positions[idx].symbol.clone();
                                let dir = lw.open_positions[idx].direction.clone();
                                let price = lw.get_share_price(&sym, &dir);
                                info!(symbol=%sym, direction=%dir, "Retrying sell for orphaned live position");
                                lw.close_position_at(idx, price, "orphan_retry").await;
                            }
                        }
                    }

                    // Auto-redeem resolved markets in live mode
                    if self.live_wallet.is_some() {
                        let now = Self::now_secs();
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
                    // Early exit for losing positions 30 seconds before market end
                    // This gives us time to exit gracefully instead of being forced out
                    let now = Self::now_secs();
                    let early_exit_needed = self.symbol_states.iter()
                        .filter(|(_, s)| s.market_end_ts.map_or(false, |end| now >= end.saturating_sub(30)))
                        .map(|(k, _)| k.clone())
                        .collect::<Vec<_>>();

                    for symbol in &early_exit_needed {
                        let positions_to_close: Vec<(usize, f64, String)> = self.wallet.open_positions.iter()
                            .enumerate()
                            .filter(|(_, pos)| pos.symbol == *symbol)
                            .filter_map(|(idx, pos)| {
                                let current_price = self.wallet.get_share_price(&pos.symbol, &pos.direction);

                                // DON'T exit if in HOLD mode (share price > threshold)
                                // HOLD mode positions are managed by try_close_position with BTC margin checks
                                if current_price > self.config.hold_min_share_price {
                                    return None;
                                }

                                // Only exit if losing by more than threshold from entry
                                // This prevents exiting positions that are just slightly down
                                let loss_pct = (pos.entry_price - current_price) / pos.entry_price;
                                let is_losing_significantly = loss_pct > self.config.early_exit_loss_pct;

                                if is_losing_significantly {
                                    Some((idx, current_price, pos.symbol.clone()))
                                } else {
                                    None
                                }
                            })
                            .collect();

                        for (idx, current_price, _sym) in positions_to_close.iter().rev() {
                            let pos = self.wallet.open_positions.remove(*idx);

                            // Mirror to live wallet
                            if let Some(lw) = &mut self.live_wallet {
                                if let Some(live_idx) = lw.open_positions.iter().position(|p| p.symbol == pos.symbol && p.direction == pos.direction) {
                                    lw.close_position_at(live_idx, *current_price, "early_exit_losing").await;
                                }
                            }

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
                                timestamp: chrono::Local::now().to_rfc3339(),
                                close_reason: Some("early_exit_losing".to_string()),
                            });
                            info!(symbol=%pos.symbol, pnl=pnl, "Early exit for losing position before market end");
                        }

                        // Independent early exit for live wallet positions that have no paper counterpart
                        if let Some(lw) = &mut self.live_wallet {
                            let live_to_close: Vec<usize> = lw.open_positions.iter()
                                .enumerate()
                                .filter(|(_, pos)| pos.symbol == *symbol)
                                .filter_map(|(idx, pos)| {
                                    let current_price = lw.get_share_price(&pos.symbol, &pos.direction);
                                    if current_price > self.config.hold_min_share_price { return None; }
                                    let loss_pct = (pos.entry_price - current_price) / pos.entry_price;
                                    if loss_pct > self.config.early_exit_loss_pct {
                                        Some(idx)
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            for idx in live_to_close.into_iter().rev() {
                                let price = lw.get_share_price(&lw.open_positions[idx].symbol, &lw.open_positions[idx].direction);
                                lw.close_position_at(idx, price, "early_exit_losing").await;
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
                    // Independent force-close for live wallet positions (catches orphaned positions)
                    if market_ending {
                        if let Some(lw) = &mut self.live_wallet {
                            let live_to_close: Vec<usize> = lw.open_positions.iter()
                                .enumerate()
                                .filter(|(_, pos)| ending_symbols.contains(&pos.symbol))
                                .map(|(idx, _)| idx)
                                .collect();
                            for idx in live_to_close.into_iter().rev() {
                                let price = lw.get_share_price(&lw.open_positions[idx].symbol, &lw.open_positions[idx].direction);
                                lw.close_position_at(idx, price, "market_end").await;
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
                _ = spike_poll.tick() => {
                    if self.running {
                        let symbols: Vec<String> = self.symbol_states.keys().cloned().collect();
                        for symbol in symbols {
                            self.check_for_spike(&symbol).await;
                        }
                    }
                    self.wallet.flush_pending();
                    // Clean up stale pending orders in live wallet
                    if let Some(lw) = &mut self.live_wallet {
                        lw.cleanup_pending_orders();
                    }
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
                state.btc_history.retain(|(_, t)| t.elapsed().as_millis() < 6000);
                
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

                // Find price from ~5s ago for trend detection
                let cutoff_5s = std::time::Duration::from_millis(5000);
                state.baseline_5s = state.btc_history.iter()
                    .find(|(_, t)| now.duration_since(*t) >= cutoff_5s)
                    .map(|(p, _)| *p);

                // Decimated slow buffer: 1 sample per ~500ms, 30s window (for longer trend)
                let should_sample = state.btc_history_slow.back()
                    .map(|(_, t)| now.duration_since(*t) >= Duration::from_millis(500))
                    .unwrap_or(true);
                if should_sample {
                    state.btc_history_slow.push_back((update.price, now));
                    while state.btc_history_slow.front()
                        .map(|(_, t)| now.duration_since(*t) > Duration::from_secs(31))
                        .unwrap_or(false)
                    {
                        state.btc_history_slow.pop_front();
                    }
                }
                state.baseline_30s = state.btc_history_slow.front()
                    .filter(|(_, t)| now.duration_since(*t) >= Duration::from_secs(28))
                    .map(|(p, _)| *p);

                // Compute trend magnitudes
                state.trend_5s_usd = state.baseline_5s
                    .map(|base| update.price - base)
                    .unwrap_or(0.0);
                state.trend_30s_usd = state.baseline_30s
                    .map(|base| update.price - base)
                    .unwrap_or(0.0);
            } else {
                state.last_chainlink = Some((update.price, update.timestamp));
                // Use first Chainlink tick of new window as price to beat (fallback)
                if !state.price_to_beat_set {
                    state.price_to_beat = Some(update.price);
                    state.price_to_beat_set = true;
                    info!(symbol=%update.symbol, price_to_beat=update.price, "Price to beat set from first Chainlink tick");
                    // Update wallet with market metadata for hold mode
                    self.wallet.set_market_metadata(&update.symbol, state.price_to_beat, state.market_end_ts);
                }
            }
        }

        if update.source == "binance" {
            let symbol = update.symbol.clone();

            // Push BTC price to wallet for long-baseline spike calculation (every tick)
            self.wallet.push_btc_price(&symbol, update.price);
            
            // Update BTC trailing stop tracking for open positions
            self.wallet.update_btc_trailing(&symbol, update.price);
            if let Some(lw) = &mut self.live_wallet {
                lw.update_btc_trailing(&symbol, update.price);
            }

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
        if closed {
            self.mirror_closes_to_live().await;
        }

        closed
    }

    async fn handle_clob_update(&mut self, update: SharePriceUpdate) -> bool {
        self.wallet.update_share_price(&update.symbol, &update.direction, update.best_bid, update.best_ask);
        // Update live wallet price cache too (matches paper.rs)
        if let Some(lw) = &mut self.live_wallet {
            lw.update_share_price(&update.symbol, &update.direction, update.best_bid, update.best_ask);
        }
        self.wallet.flush_pending();
        let closed = self.wallet.try_close_position().await;

        // Mirror closes to live wallet (same as handle_price_update)
        if closed {
            self.mirror_closes_to_live().await;
        }

        closed
    }

    /// Mirror paper wallet exits to live wallet.
    /// Scans paper trade_history for recent exits (last 500ms) and closes
    /// the matching live wallet position with the same reason.
    async fn mirror_closes_to_live(&mut self) {
        if self.live_wallet.is_none() { return; }

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
                if let Some(idx) = lw.open_positions.iter().position(|p| p.symbol == sym && p.direction == dir) {
                    let reason_str = reason.as_deref().unwrap_or("exit");
                    let r: &'static str = match reason_str {
                        "trailing_stop" => "trailing_stop",
                        "max_price" => "max_price",
                        "trend_reversed" => "trend_reversed",
                        "spike_faded" => "spike_faded",
                        "stop_loss" => "stop_loss",
                        "near_end" => "near_end",
                        "market_end" => "market_end",
                        "manual" => "manual",
                        "hold_safety_exit" => "hold_safety_exit",
                        _ => "exit",
                    };
                    lw.close_position_at(idx, price, r).await;
                }
            }
        }
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
        if let Some(lw) = &mut self.live_wallet {
            lw.clear_tokens(&market.symbol);
            lw.clear_price_cache(&market.symbol);
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
            let is_counter_trend = spike_direction != 0.0
                && trend_dir != 0.0
                && spike_direction != trend_dir;

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
                    let should_log = state.last_rejection.as_ref()
                        .is_none_or(|(r, t)| r != "COUNTER_TREND" || t.elapsed().as_secs() >= 3);
                    if should_log {
                        state.last_rejection = Some(("COUNTER_TREND".to_string(), Instant::now()));
                        self.add_signal(symbol, direction, adjusted_spike, "REJECTED",
                            Some(format!("COUNTER_TREND(trend={:.0},need={:.0})", trend_magnitude, required_spike)));
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
                let is_counter_ptb = (trade_is_up && ptb_distance < -self.config.ptb_neutral_zone_usd)
                    || (!trade_is_up && ptb_distance > self.config.ptb_neutral_zone_usd);

                if is_counter_ptb {
                    let ptb_abs = ptb_distance.abs();

                    // Hard reject: too far from ptb for this direction to win
                    if ptb_abs > self.config.ptb_max_counter_distance_usd {
                        let state = self.symbol_states.get_mut(symbol).unwrap();
                        let should_log = state.last_rejection.as_ref()
                            .is_none_or(|(r, t)| r != "PTB_TOO_FAR" || t.elapsed().as_secs() >= 5);
                        if should_log {
                            state.last_rejection = Some(("PTB_TOO_FAR".to_string(), Instant::now()));
                            self.add_signal(symbol, direction, adjusted_spike, "REJECTED",
                                Some(format!("PTB_TOO_FAR(dist={:.0},max={:.0})", ptb_distance, self.config.ptb_max_counter_distance_usd)));
                        }
                        return;
                    }

                    // Soft penalty: scale threshold up based on distance from ptb
                    let ptb_t = ((ptb_abs - self.config.ptb_neutral_zone_usd)
                        / (self.config.ptb_max_counter_distance_usd - self.config.ptb_neutral_zone_usd))
                        .clamp(0.0, 1.0);
                    let ptb_multiplier = 1.0 + ptb_t; // 1.0 to 2.0
                    let required_spike = threshold_usd * ptb_multiplier;

                    if abs_spike < required_spike {
                        let state = self.symbol_states.get_mut(symbol).unwrap();
                        let should_log = state.last_rejection.as_ref()
                            .is_none_or(|(r, t)| r != "PTB_COUNTER" || t.elapsed().as_secs() >= 3);
                        if should_log {
                            state.last_rejection = Some(("PTB_COUNTER".to_string(), Instant::now()));
                            self.add_signal(symbol, direction, adjusted_spike, "REJECTED",
                                Some(format!("PTB_COUNTER(dist={:.0},need={:.0})", ptb_distance, required_spike)));
                        }
                        return;
                    }
                }
            }
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
            let now = Self::now_secs();
            if now >= end_ts.saturating_sub(2) {
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

        // Check cooldown - don't re-enter too quickly after a close
        let cooldown_ms = 3000; // 3 second cooldown after a position closes
        let cooldown_ok = state.last_close_time.map_or(true, |t| t.elapsed().as_millis() >= cooldown_ms);
        
        // New spike or significantly larger spike for scaling in
        if abs_spike > state.last_spike_usd * 1.1 && cooldown_ok {
            // Get entry price BEFORE open_position (pending entry hasn't been promoted yet)
            let entry_price = self.wallet.get_share_price(symbol, direction);
            
            // Get current BTC price to set entry_btc immediately
            let current_btc = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);
            
            // Check if we're in HOLD mode (share price > threshold, < 30s remaining)
            let now_secs = Self::now_secs();
            let time_remaining = state.market_end_ts.and_then(|end| {
                if end > now_secs { Some(end - now_secs) } else { None }
            });
            let in_hold_mode = entry_price > self.config.hold_min_share_price 
                && time_remaining.map_or(false, |t| t <= 30);
            
            match self.wallet.open_position(symbol, direction, adjusted_spike, threshold_usd, in_hold_mode, current_btc) {
                Ok(level) => {
                    state.last_spike_usd = abs_spike;

                    if self.live_wallet.is_some() {
                        let sym = symbol.to_string();
                        let dir = direction.to_string();
                        let spk = adjusted_spike;
                        let thresh = threshold_usd;
                        let btc = current_btc;
                        if let Some(lw) = &mut self.live_wallet {
                            match lw.open_position(&sym, &dir, spk, thresh, in_hold_mode, btc).await {
                                Ok(live_level) => {
                                    // Sync paper wallet's pending entry to match live fill exactly.
                                    // This ensures paper and live positions are identical (same price,
                                    // shares, cost) so exit decisions from paper correctly apply to live.
                                    if let Some(live_pos) = lw.open_positions.last() {
                                        if live_pos.symbol == sym && live_pos.direction == dir {
                                            self.wallet.sync_pending_to_live_fill(
                                                &sym, &dir,
                                                live_pos.entry_price,
                                                live_pos.shares,
                                                live_pos.position_size,
                                                live_pos.buy_fee,
                                            );
                                        }
                                    }
                                    self.add_signal(&sym, &dir, spk, "EXECUTED", Some(format!("LIVE_LEVEL_{}", live_level)));
                                }
                                Err(e) => {
                                    // Live wallet failed — roll back the paper wallet entry
                                    // Otherwise paper wallet has an orphaned position that blocks
                                    // all future entries with MAX_SCALE_LEVEL
                                    self.wallet.rollback_pending_entry(&sym, &dir);
                                    // Reset spike gate so next spike can trigger a fresh entry
                                    let state = self.symbol_states.get_mut(symbol).unwrap();
                                    state.last_spike_usd = 0.0;
                                    // Dedup live rejections — same 3-second cooldown as paper rejections
                                    let live_reason = format!("LIVE:{}", e);
                                    let should_log = state.last_rejection.as_ref()
                                        .map_or(true, |(r, t)| r != &live_reason || t.elapsed().as_secs() >= 3);
                                    if should_log {
                                        state.last_rejection = Some((live_reason.clone(), Instant::now()));
                                        self.add_signal(&sym, &dir, spk, "REJECTED", Some(live_reason));
                                    }
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
