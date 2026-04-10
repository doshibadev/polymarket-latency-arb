use tracing::{debug, info};
use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;
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
    pub entry_spike: f64,
    #[serde(skip)]
    pub entry_time: std::time::Instant,
    pub highest_price: f64,
    pub scale_level: u32,
    pub hold_to_resolution: bool,
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
    pub spike_history: Vec<f64>, // rolling window for smoothed spike
}

/// A pending entry waiting for execution delay to elapse
#[derive(Clone)]
pub struct PendingEntry {
    pub symbol: String,
    pub direction: String,
    pub spike: f64,
    pub entry_price: f64,
    pub scale_level: u32,
    pub position_size: f64,
    pub shares: f64,
    pub buy_fee: f64,
    pub submitted_at: Instant,
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
    pub pending_entries: Vec<PendingEntry>,
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
            pending_entries: Vec::new(),
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

        // Update best price for all positions of this symbol/direction
        for pos in &mut self.open_positions {
            if pos.symbol == symbol && pos.direction == direction {
                if direction == "UP" {
                    // Track highest price reached
                    if mid_price > pos.highest_price { pos.highest_price = mid_price; }
                } else {
                    // For DOWN positions, track lowest price reached (shares gain value as price drops)
                    if mid_price < pos.highest_price { pos.highest_price = mid_price; }
                }
            }
        }
    }

    pub fn update_btc_prices(&mut self, symbol: &str, binance: f64, chainlink: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.last_binance = binance;
        state.last_chainlink = chainlink;
        // spike_history is updated by engine via update_spike_momentum
    }

    /// Push the current 200ms momentum value into spike_history for exit smoothing
    pub fn push_spike_momentum(&mut self, symbol: &str, momentum: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.spike_history.push(momentum);
        if state.spike_history.len() > 5 { state.spike_history.remove(0); }
    }

    /// Called by engine on each Binance tick to evaluate hold-to-resolution for open positions
    pub fn update_hold_status(
        &mut self,
        symbol: &str,
        btc_history: &[f64],
        current_chainlink: f64,
        price_to_beat: Option<f64>,
        end_ts: Option<u64>,
        hold_margin_per_second: f64,
        hold_max_seconds: u64,
        hold_max_crossings: usize,
    ) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let time_remaining = match end_ts {
            Some(e) if e > now_secs => e - now_secs,
            _ => return,
        };

        let ptb = match price_to_beat { Some(p) => p, None => return };
        // Use Chainlink price for margin — market resolves against Chainlink
        let current_btc = if current_chainlink > 0.0 { current_chainlink } else {
            match btc_history.last() { Some(&p) => p, None => return }
        };

        for pos in &mut self.open_positions {
            if pos.symbol != symbol { continue; }

            let margin = if pos.direction == "UP" {
                current_btc - ptb
            } else {
                ptb - current_btc
            };

            // If BTC has crossed price_to_beat → close immediately, don't hold
            if margin <= 0.0 {
                pos.hold_to_resolution = false;
                continue;
            }

            // Only consider holding in the last hold_max_seconds
            if time_remaining > hold_max_seconds {
                pos.hold_to_resolution = false;
                continue;
            }

            // Count crossings in btc_history
            let crossings = btc_history.windows(2).filter(|w| {
                let was_above = w[0] > ptb;
                let is_above = w[1] > ptb;
                was_above != is_above
            }).count();

            if crossings > hold_max_crossings {
                pos.hold_to_resolution = false;
                continue;
            }

            // Check trend: BTC must be consistently on correct side AND accelerating away from ptb
            let trend_ok = if btc_history.len() >= 10 {
                let recent = &btc_history[btc_history.len()-10..];
                // All recent ticks must be on the correct side of price-to-beat
                let all_correct_side = if pos.direction == "UP" {
                    recent.iter().all(|&p| p > ptb)
                } else {
                    recent.iter().all(|&p| p < ptb)
                };
                // Trend must be moving away from price-to-beat (not just sideways)
                let slope = recent.last().unwrap() - recent.first().unwrap();
                let moving_away = if pos.direction == "UP" { slope > 0.0 } else { slope < 0.0 };
                all_correct_side && moving_away
            } else { false }; // not enough history = don't hold

            // Required margin check — stricter: 2x the normal requirement for high confidence
            let required_margin = hold_margin_per_second * time_remaining as f64;
            pos.hold_to_resolution = margin >= required_margin && trend_ok && crossings == 0;
        }
    }

    pub fn calculate_fee(&self, shares: f64, price: f64) -> f64 {
        let fee = shares * self.config.crypto_fee_rate * price * (1.0 - price);
        if fee < 0.00001 { 0.0 } else { (fee * 100000.0).round() / 100000.0 }
    }

    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(state) = self.symbol_states.get(symbol) {
            if direction == "UP" { (state.up_bid, state.up_ask) } else { (state.down_bid, state.down_ask) }
        } else { (0.0, 0.0) }
    }

    pub fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        if bid > 0.0 && ask > 0.0 { (bid + ask) / 2.0 } else { 0.0 }
    }

    pub async fn try_close_position(&mut self) -> bool {
        let mut to_close = Vec::new();

        for (idx, pos) in self.open_positions.iter().enumerate() {
            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            if current_price <= 0.0 { continue; }

            let state = self.symbol_states.get(&pos.symbol).cloned().unwrap_or_default();
            // Use smoothed Binance momentum for exit decisions
            let adjusted_spike = if state.spike_history.is_empty() {
                0.0
            } else {
                state.spike_history.iter().sum::<f64>() / state.spike_history.len() as f64
            };

            // Trailing stop: 5% drop from highest (UP) or 5% rise from lowest (DOWN)
            let trailing_stop_hit = if pos.direction == "UP" {
                current_price < pos.highest_price * (1.0 - self.config.trailing_stop_pct / 100.0)
            } else {
                // For DOWN, highest_price tracks the lowest price seen (best value)
                current_price > pos.highest_price * (1.0 + self.config.trailing_stop_pct / 100.0)
            };

            // Spike reversed: for UP close when spike goes negative, for DOWN close when spike goes positive
            // Use entry_spike sign to determine what "reversed" means
            let trend_reversed = if pos.direction == "UP" {
                adjusted_spike < -(pos.entry_spike.abs() * 0.1) // spike went 10% negative
            } else {
                adjusted_spike > (pos.entry_spike.abs() * 0.1) // spike went 10% positive
            };

            // Spike faded to <spike_faded_pct of entry spike
            let spike_faded = adjusted_spike.abs() < pos.entry_spike.abs() * self.config.spike_faded_pct;

            let held_ms = pos.entry_time.elapsed().as_millis();
            let near_end = held_ms > 295000;

            if pos.hold_to_resolution {
                // In hold mode: only close if price-to-beat is breached (trailing stop still active)
                if trailing_stop_hit {
                    to_close.push((idx, "trailing_stop"));
                }
                // near_end handled by engine's force-close at market end
            } else if trailing_stop_hit || trend_reversed || spike_faded || near_end {
                let reason = if trailing_stop_hit { "trailing_stop" }
                            else if trend_reversed { "trend_reversed" }
                            else if spike_faded { "spike_faded" }
                            else { "near_end" };
                to_close.push((idx, reason));
            }        }

        if to_close.is_empty() {
            return false;
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

        true
    }

    pub fn open_position(&mut self, symbol: &str, direction: &str, spike: f64, threshold_usd: f64) -> std::result::Result<u32, String> {
        let existing: Vec<_> = self.open_positions.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let pending_same = self.pending_entries.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();
        let scale_level = (existing.len() + pending_same) as u32 + 1;

        if scale_level > 1 { return Err("MAX_SCALE_LEVEL".to_string()); }

        let entry_price = self.get_share_price(symbol, direction);
        if entry_price <= 0.0 { return Err("NO_PRICE_DATA".to_string()); }
        if entry_price > self.config.max_entry_price { return Err("PRICE_TOO_HIGH".to_string()); }
        if entry_price < self.config.min_entry_price { return Err("PRICE_TOO_LOW".to_string()); }
        if (entry_price - 0.5).abs() < self.config.min_price_distance {
            return Err("PRICE_TOO_CLOSE_TO_HALF".to_string());
        }

        let position_size = self.balance * self.portfolio_pct * (1.0 / scale_level as f64);
        let shares = position_size / entry_price;
        let buy_fee = self.calculate_fee(shares, entry_price);
        if (position_size + buy_fee) > self.balance { return Err("INSUFFICIENT_BALANCE".to_string()); }
        if position_size < 1.0 { return Err("BELOW_MIN_ORDER_SIZE".to_string()); }

        // Reserve balance immediately so concurrent entries don't over-allocate
        self.balance -= position_size + buy_fee;

        let spike_bonus = (spike.abs() - threshold_usd) * self.config.spike_scaling_factor;
        let profit_target = entry_price * (1.0 + self.config.profit_target_pct + spike_bonus);

        self.pending_entries.push(PendingEntry {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            spike,
            entry_price,
            scale_level,
            position_size,
            shares,
            buy_fee,
            submitted_at: Instant::now(),
        });

        Ok(scale_level)
    }

    /// Call on every tick — promotes pending entries to open positions after execution delay
    pub fn flush_pending(&mut self) {
        let delay = if self.config.paper_trading {
            std::time::Duration::from_millis(self.config.execution_delay_ms)
        } else {
            std::time::Duration::ZERO
        };
        let mut promoted = Vec::new();
        self.pending_entries.retain(|p| {
            if p.submitted_at.elapsed() >= delay {
                promoted.push(p.clone());
                false
            } else {
                true
            }
        });

        for p in promoted {
            let state = self.symbol_states.get(&p.symbol).cloned().unwrap_or_default();
            self.trade_history.push(TradeRecord {
                symbol: p.symbol.clone(),
                r#type: "entry".to_string(),
                question: state.question,
                direction: p.direction.clone(),
                entry_price: Some(p.entry_price),
                exit_price: None,
                shares: p.shares,
                cost: p.position_size,
                pnl: None,
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: None,
            });
            let sym = p.symbol.clone();
            let dir = p.direction.clone();
            let price = p.entry_price;
            let level = p.scale_level;
            self.open_positions.push(OpenPosition {
                symbol: p.symbol,
                direction: p.direction,
                entry_price: p.entry_price,
                shares: p.shares,
                position_size: p.position_size,
                buy_fee: p.buy_fee,
                entry_spike: p.spike,
                entry_time: Instant::now(),
                highest_price: p.entry_price,
                scale_level: p.scale_level,
                hold_to_resolution: false,
            });
            info!(symbol=%sym, direction=%dir, price=price, level=level, "Position opened after delay");
        }
    }
}
