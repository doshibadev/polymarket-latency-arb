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
    pub last_spike_usd: f64,
    pub market_end_ts: Option<u64>,
    pub question: String,
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
                "level": pos.scale_level
            }));
        }

        let total_val = self.wallet.balance + net_market_value;
        let total_pnl = total_val - self.wallet.starting_balance;

        let mut markets = HashMap::new();
        for (symbol, state) in &self.symbol_states {
            let (up_b, up_a) = self.wallet.get_bid_ask(symbol, "UP");
            let (dn_b, dn_a) = self.wallet.get_bid_ask(symbol, "DOWN");

            let binance = state.last_binance.map(|(p, _)| p).unwrap_or(0.0);
            let chainlink = state.last_chainlink.map(|(p, _)| p).unwrap_or(0.0);

            markets.insert(symbol.clone(), json!({
                "question": state.question,
                "up_price": (up_b + up_a) / 2.0,
                "down_price": (dn_b + dn_a) / 2.0,
                "binance": binance,
                "chainlink": chainlink,
                "spike": binance - chainlink - state.ema_offset,
                "end_ts": state.market_end_ts
            }));
        }

        let state = json!({
            "balance": self.wallet.balance,
            "starting_balance": self.wallet.starting_balance,
            "total_portfolio_value": total_val,
            "unrealized_pnl": unrealized_pnl,
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
                "min_hold_ms": self.config.min_hold_ms,
                "profit_target_pct": self.config.profit_target_pct,
                "trailing_stop_pct": self.config.trailing_stop_pct,
                "max_spread_bps": self.config.max_spread_bps,
                "timeout_ms": self.config.timeout_ms,
                "max_entry_price": self.config.max_entry_price,
                "spike_scaling_factor": self.config.spike_scaling_factor,
                "ema_alpha": self.config.ema_alpha,
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
                _ = broadcast_timer.tick() => { self.broadcast_state(); }
                Some(update) = self.price_rx.recv() => {
                    if self.handle_price_update(update).await {
                        // Push new history point after a position closed
                        let mut net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        self.update_history(self.wallet.balance + net_market_value);
                    }
                }
                Some(clob_update) = self.clob_rx.recv() => {
                    if self.handle_clob_update(clob_update).await {
                        // Push new history point after a position closed
                        let mut net_market_value = 0.0;
                        for pos in &self.wallet.open_positions {
                            let (b, a) = self.wallet.get_bid_ask(&pos.symbol, &pos.direction);
                            net_market_value += pos.shares * ((b + a) / 2.0);
                        }
                        self.update_history(self.wallet.balance + net_market_value);
                    }
                }
                Some(market) = self.market_rx.recv() => { self.handle_market_update(market).await; }
                else => break,
            }
        }
        Ok(())
    }

    async fn handle_price_update(&mut self, update: PriceUpdate) -> bool {
        let state = self.symbol_states.entry(update.symbol.clone()).or_default();
        if update.source == "binance" {
            state.last_binance = Some((update.price, update.timestamp));
        } else {
            state.last_chainlink = Some((update.price, update.timestamp));
            if let Some((b_price, _)) = state.last_binance {
                let current_offset = update.price - b_price;
                if !state.has_ema { state.ema_offset = current_offset; state.has_ema = true; }
                else { state.ema_offset = self.config.ema_alpha * current_offset + (1.0 - self.config.ema_alpha) * state.ema_offset; }
            }
        }

        if let (Some((b, _)), Some((c, _))) = (state.last_binance, state.last_chainlink) {
            self.wallet.update_btc_prices(&update.symbol, b, c);
            if self.running { self.check_for_spike(&update.symbol).await; }
        }
        self.wallet.try_close_position().await
    }

    async fn handle_clob_update(&mut self, update: SharePriceUpdate) -> bool {
        self.last_clob_update_ts = Some(Instant::now());
        self.wallet.update_share_price(&update.symbol, &update.direction, update.best_bid, update.best_ask);
        self.wallet.try_close_position().await;
        if self.running { self.check_for_spike(&update.symbol).await; }
        self.wallet.try_close_position().await
    }

    pub async fn handle_market_update(&mut self, market: MarketData) {
        let state = self.symbol_states.entry(market.symbol.clone()).or_default();
        state.market_end_ts = Some(market.window_end_ts);
        state.question = market.question.clone();
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
        
        let adjusted_spike = (binance - chainlink) - state.ema_offset;
        let abs_spike = adjusted_spike.abs();
        let threshold_usd = self.config.threshold_bps as f64 / 100.0;

        if abs_spike < threshold_usd { 
            state.last_spike_usd = 0.0; 
            return; 
        }

        let direction = if adjusted_spike > 0.0 { "UP" } else { "DOWN" };
        let (bid, ask) = self.wallet.get_bid_ask(symbol, direction);
        
        if bid <= 0.0 || ask <= 0.0 { 
            self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some("NO_POL_LIQUIDITY".to_string()));
            return; 
        }
        
        if let Some(end_ts) = state.market_end_ts {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            if now >= end_ts.saturating_sub(45) { 
                self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some("MARKET_ENDING".to_string()));
                return; 
            }
        }

        // New spike or significantly larger spike for scaling in
        if abs_spike > state.last_spike_usd * 1.1 {
            let spread_bps = (((ask - bid) / ((bid + ask) / 2.0)) * 10000.0) as u64;
            match self.wallet.open_position(symbol, direction, binance, chainlink, adjusted_spike, spread_bps, threshold_usd).await {
                Ok(level) => {
                    let level_str = format!("LEVEL_{}", level);
                    state.last_spike_usd = abs_spike;
                    self.add_signal(symbol, direction, adjusted_spike, "EXECUTED", Some(level_str));
                }
                Err(reason) => {
                    self.add_signal(symbol, direction, adjusted_spike, "REJECTED", Some(reason));
                }
            }
        }
    }
}
