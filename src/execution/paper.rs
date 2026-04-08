use tracing::{debug, info};
use serde::Serialize;
use std::collections::HashMap;
use crate::config::AppConfig;

/// An open scalp position
#[derive(Clone, Serialize)]
pub struct OpenPosition {
    pub symbol: String,
    pub direction: String,
    pub entry_price: f64,
    pub shares: f64,
    pub position_size: f64,
    pub buy_fee: f64,
    pub entry_binance: f64,
    pub entry_chainlink: f64,
    pub entry_spike: f64,
    #[serde(skip)]
    pub entry_time: std::time::Instant,
    pub highest_price: f64,
    pub profit_target: f64,
    pub scale_level: u32, // 1 for first entry, 2 for second, etc.
}

#[derive(Serialize, Clone)]
pub struct TradeRecord {
    pub symbol: String,
    pub r#type: String,
    pub question: String,
    pub direction: String,
    pub entry_price: Option<f64>,
    pub exit_price: Option<f64>,
    pub shares: f64,
    pub cost: f64,
    pub pnl: Option<f64>,
    pub timestamp: String,
    pub close_reason: Option<String>,
}

#[derive(Default, Clone)]
struct SymbolMarketState {
    pub up_bid: f64,
    pub up_ask: f64,
    pub down_bid: f64,
    pub down_ask: f64,
    pub question: String,
    pub last_binance: f64,
    pub last_chainlink: f64,
}

pub struct PaperWallet {
    pub balance: f64,
    pub starting_balance: f64,
    pub trade_count: u64,
    pub wins: u64,
    pub losses: u64,
    pub total_fees_paid: f64,
    pub total_volume: f64,
    pub portfolio_pct: f64,
    pub open_positions: Vec<OpenPosition>,
    pub trade_history: Vec<TradeRecord>,
    symbol_states: HashMap<String, SymbolMarketState>,
    config: AppConfig,
}

impl PaperWallet {
    pub fn new(config: AppConfig) -> Self {
        Self {
            balance: config.starting_balance,
            starting_balance: config.starting_balance,
            trade_count: 0,
            wins: 0,
            losses: 0,
            total_fees_paid: 0.0,
            total_volume: 0.0,
            portfolio_pct: config.portfolio_pct,
            open_positions: Vec::new(),
            trade_history: Vec::new(),
            symbol_states: HashMap::new(),
            config,
        }
    }

    pub fn update_config(&mut self, config: AppConfig) {
        self.config = config.clone();
        self.portfolio_pct = config.portfolio_pct;
    }

    pub fn set_market_info(&mut self, symbol: &str, question: String) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.question = question;
    }

    pub fn reset_prices(&mut self, symbol: &str) {
        if let Some(state) = self.symbol_states.get_mut(symbol) {
            state.up_bid = 0.0; state.up_ask = 0.0;
            state.down_bid = 0.0; state.down_ask = 0.0;
        }
    }

    pub fn update_share_price(&mut self, symbol: &str, direction: &str, bid: f64, ask: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        let mid_price = (bid + ask) / 2.0;
        if direction == "UP" {
            state.up_bid = bid; state.up_ask = ask;
        } else {
            state.down_bid = bid; state.down_ask = ask;
        }

        // Update highest price for all positions of this symbol/direction
        for pos in &mut self.open_positions {
            if pos.symbol == symbol && pos.direction == direction {
                if mid_price > pos.highest_price { pos.highest_price = mid_price; }
            }
        }
    }

    pub fn update_btc_prices(&mut self, symbol: &str, binance: f64, chainlink: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.last_binance = binance;
        state.last_chainlink = chainlink;
    }

    fn calculate_fee(&self, shares: f64, price: f64) -> f64 {
        let fee = shares * self.config.crypto_fee_rate * price * (1.0 - price);
        if fee < 0.00001 { 0.0 } else { (fee * 100000.0).round() / 100000.0 }
    }

    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(state) = self.symbol_states.get(symbol) {
            if direction == "UP" { (state.up_bid, state.up_ask) } else { (state.down_bid, state.down_ask) }
        } else { (0.0, 0.0) }
    }

    fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        if bid > 0.0 && ask > 0.0 { (bid + ask) / 2.0 } else { 0.0 }
    }

    pub async fn try_close_position(&mut self) {
        let mut to_close = Vec::new();

        for (idx, pos) in self.open_positions.iter().enumerate() {
            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            if current_price <= 0.0 { continue; }

            let sell_fee = self.calculate_fee(pos.shares, current_price);
            let net_revenue = (pos.shares * current_price) - sell_fee;
            let pnl = net_revenue - (pos.position_size + pos.buy_fee);

            let state = self.symbol_states.get(&pos.symbol).cloned().unwrap_or_default();
            let current_spike = state.last_binance - state.last_chainlink;
            
            // Close if next tick is going in opposite direction
            let mut trend_reversed = false;
            if pos.direction == "UP" {
                if current_spike < 0.0 { trend_reversed = true; }
            } else {
                if current_spike > 0.0 { trend_reversed = true; }
            }

            // Spike Convergence: Close if spike has significantly faded
            let mut spike_faded = false;
            if current_spike.abs() < pos.entry_spike.abs() * 0.1 {
                spike_faded = true;
            }

            let held_ms = pos.entry_time.elapsed().as_millis();
            
            // Trailing stop (optional safety)
            let mut trailing_stop_hit = false;
            if self.config.trailing_stop_pct > 0.0 && current_price < pos.highest_price * (1.0 - self.config.trailing_stop_pct) {
                trailing_stop_hit = true;
            }

            // Emergency Exit: near end of window
            let near_end = held_ms > 240000; // 4 minutes

            if trend_reversed || spike_faded || trailing_stop_hit || near_end {
                let reason = if trend_reversed { "trend_reversed" } 
                            else if spike_faded { "spike_faded" }
                            else if trailing_stop_hit { "trailing_stop" }
                            else { "near_end" };
                to_close.push((idx, reason));
            }
        }

        // Close in reverse order to keep indices valid
        to_close.sort_by_key(|k| std::cmp::Reverse(k.0));
        for (idx, reason) in to_close {
            let pos = self.open_positions.remove(idx);
            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            let sell_fee = self.calculate_fee(pos.shares, current_price);
            let net_revenue = (pos.shares * current_price) - sell_fee;
            let pnl = net_revenue - (pos.position_size + pos.buy_fee);

            self.balance += net_revenue;
            self.trade_count += 1;
            if pnl > 0.0 { self.wins += 1; } else { self.losses += 1; }
            self.total_fees_paid += pos.buy_fee + sell_fee;
            self.total_volume += pos.position_size + (pos.shares * current_price);

            let state = self.symbol_states.get(&pos.symbol).cloned().unwrap_or_default();
            self.trade_history.push(TradeRecord {
                symbol: pos.symbol.clone(),
                r#type: "exit".to_string(),
                question: state.question,
                direction: pos.direction.clone(),
                entry_price: Some(pos.entry_price),
                exit_price: Some(current_price),
                shares: pos.shares,
                cost: pos.position_size,
                pnl: Some(pnl),
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: Some(reason.to_string()),
            });

            info!(symbol=%pos.symbol, pnl=format!("${:.4}", pnl), reason=%reason, "Position closed");
        }
    }

    pub async fn open_position(&mut self, symbol: &str, direction: &str, binance: f64, chainlink: f64, spike: f64, spread_bps: u64, threshold_usd: f64) {
        let current_symbol_positions: Vec<_> = self.open_positions.iter().filter(|p| p.symbol == symbol && p.direction == direction).collect();
        let scale_level = current_symbol_positions.len() as u32 + 1;

        if scale_level > 3 { return; } // Max 3 entries per coin/direction

        if scale_level > 1 {
            let last_entry_spike = current_symbol_positions.last().unwrap().entry_spike.abs();
            if spike.abs() < last_entry_spike * 1.5 { return; } // Only scale in if spike grows 50%
        }

        let entry_price = self.get_share_price(symbol, direction);
        if entry_price <= 0.0 || entry_price > self.config.max_entry_price { return; }
        if spread_bps > self.config.max_spread_bps { return; }

        let position_size = self.balance * self.portfolio_pct * (1.0 / scale_level as f64);
        let shares = position_size / entry_price;
        let buy_fee = self.calculate_fee(shares, entry_price);
        if (position_size + buy_fee) > self.balance { return; }

        self.balance -= position_size + buy_fee;

        let spike_bonus = (spike.abs() - threshold_usd) * self.config.spike_scaling_factor;
        let profit_target = entry_price * (1.0 + self.config.profit_target_pct + spike_bonus);

        let state = self.symbol_states.get(symbol).cloned().unwrap_or_default();
        self.trade_history.push(TradeRecord {
            symbol: symbol.to_string(),
            r#type: "entry".to_string(),
            question: state.question,
            direction: direction.to_string(),
            entry_price: Some(entry_price),
            exit_price: None,
            shares,
            cost: position_size,
            pnl: None,
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: None,
        });

        self.open_positions.push(OpenPosition {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price,
            shares,
            position_size,
            buy_fee,
            entry_binance: binance,
            entry_chainlink: chainlink,
            entry_spike: spike,
            entry_time: std::time::Instant::now(),
            highest_price: entry_price,
            profit_target,
            scale_level,
        });

        info!(symbol=%symbol, level=scale_level, "Position opened");
    }
}
