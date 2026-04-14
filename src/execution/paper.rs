use tracing::info;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::time::Instant;
use crate::config::AppConfig;

/// An open scalp position
#[derive(Clone, Serialize)]
pub struct OpenPosition {
    pub symbol: String,
    pub direction: String,
    pub entry_price: f64,
    pub avg_entry_price: f64,  // Weighted average when scaling in
    pub shares: f64,
    pub position_size: f64,
    pub buy_fee: f64,
    pub entry_spike: f64,
    #[serde(skip)]
    pub entry_time: std::time::Instant,
    pub highest_price: f64,
    pub scale_level: u32,
    pub hold_to_resolution: bool,
    pub peak_spike: f64,           // highest spike seen since entry
    // BTC-based exit fields
    pub entry_btc: f64,            // BTC price at entry
    pub peak_btc: f64,             // highest BTC since entry (for UP positions)
    pub trough_btc: f64,           // lowest BTC since entry (for DOWN positions)
    #[serde(skip)]
    pub spike_faded_since: Option<Instant>, // when spike_faded reversal first detected
    #[serde(skip)]
    pub trend_reversed_since: Option<Instant>, // when trend_reversed first detected
}

#[derive(Serialize, Deserialize, Clone)]
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
    pub cumulative_pnl: Option<f64>,
    pub balance_after: Option<f64>,
    pub timestamp: String,
    pub close_reason: Option<String>,
}

/// Serializable state for paper trading persistence
#[derive(Serialize, Deserialize)]
pub struct PaperWalletState {
    pub balance: f64,
    pub starting_balance: f64,
    pub trade_count: u64,
    pub wins: u64,
    pub losses: u64,
    pub total_fees_paid: f64,
    pub total_volume: f64,
    pub trade_history: Vec<TradeRecord>,
    pub history: Vec<HistoryPoint>, // Chart performance data
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HistoryPoint {
    pub t: String,  // timestamp
    pub v: f64,     // portfolio value
}

#[derive(Clone)]
struct SymbolMarketState {
    pub up_bid: f64,
    pub up_ask: f64,
    pub down_bid: f64,
    pub down_ask: f64,
    pub question: String,
    pub last_binance: f64,
    pub last_chainlink: f64,
    pub spike_history: [f64; 16], // fixed-size ring buffer for smoothed spike
    pub spike_history_len: usize, // how many valid entries
    pub spike_history_idx: usize, // next write position
    pub btc_price_history: [(f64, Instant); 64], // (price, time) for long-baseline spike calc
    pub btc_history_len: usize,
    pub btc_history_idx: usize,
    pub last_price_to_beat: Option<f64>,  // price_to_beat for hold mode
    pub last_market_end_ts: Option<u64>,  // market end timestamp for hold mode
}

impl Default for SymbolMarketState {
    fn default() -> Self {
        Self {
            up_bid: 0.0,
            up_ask: 0.0,
            down_bid: 0.0,
            down_ask: 0.0,
            question: String::new(),
            last_binance: 0.0,
            last_chainlink: 0.0,
            spike_history: [0.0; 16],
            spike_history_len: 0,
            spike_history_idx: 0,
            btc_price_history: [(0.0, Instant::now()); 64],
            btc_history_len: 0,
            btc_history_idx: 0,
            last_price_to_beat: None,
            last_market_end_ts: None,
        }
    }
}

/// A pending close waiting for execution delay to elapse
#[derive(Clone)]
pub struct PendingClose {
    pub idx_at_submit: usize,
    pub symbol: String,
    pub direction: String,
    pub close_price: f64,
    pub reason: &'static str,
    pub submitted_at: Instant,
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
    pub entry_btc: f64,  // BTC price at entry to avoid race condition
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
    pub pending_closes: Vec<PendingClose>,
    pub trade_history: Vec<TradeRecord>,
    pub history: Vec<HistoryPoint>, // Chart performance data
    symbol_states: HashMap<String, SymbolMarketState>,
    config: AppConfig,
}

impl PaperWallet {
    const STATE_FILE: &'static str = "paper_wallet_state.json";
    
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
            pending_closes: Vec::new(),
            trade_history: Vec::new(),
            history: Vec::new(),
            symbol_states: HashMap::new(),
            config,
        }
    }
    
    /// Load saved state from disk (only for paper trading)
    pub fn load_state(&mut self) {
        if !self.config.paper_trading {
            return; // Never load state for live trading
        }
        
        if let Ok(data) = std::fs::read_to_string(Self::STATE_FILE) {
            if let Ok(state) = serde_json::from_str::<PaperWalletState>(&data) {
                self.balance = state.balance;
                // Always use current .env STARTING_BALANCE, not saved value
                self.starting_balance = self.config.starting_balance;
                self.trade_count = state.trade_count;
                self.wins = state.wins;
                self.losses = state.losses;
                self.total_fees_paid = state.total_fees_paid;
                self.total_volume = state.total_volume;
                self.trade_history = state.trade_history;
                self.history = state.history;
                tracing::info!("Loaded paper wallet state from disk ({} history points)", self.history.len());
            }
        }
    }
    
    /// Save state to disk (only for paper trading)
    pub fn save_state(&self) {
        if !self.config.paper_trading {
            return; // Never save state for live trading
        }
        
        let state = PaperWalletState {
            balance: self.balance,
            starting_balance: self.starting_balance,
            trade_count: self.trade_count,
            wins: self.wins,
            losses: self.losses,
            total_fees_paid: self.total_fees_paid,
            total_volume: self.total_volume,
            trade_history: self.trade_history.clone(),
            history: self.history.clone(),
        };
        
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(Self::STATE_FILE, json);
        }
    }
    
    /// Reset paper wallet to initial state
    pub fn reset(&mut self) {
        self.balance = self.config.starting_balance;
        self.starting_balance = self.config.starting_balance;
        self.trade_count = 0;
        self.wins = 0;
        self.losses = 0;
        self.total_fees_paid = 0.0;
        self.total_volume = 0.0;
        self.open_positions.clear();
        self.pending_entries.clear();
        self.pending_closes.clear();
        self.trade_history.clear();
        self.history.clear(); // Clear chart data
        self.symbol_states.clear();
        
        // Delete saved state file
        let _ = std::fs::remove_file(Self::STATE_FILE);
        tracing::info!("Paper wallet reset to initial state");
    }

    pub fn update_config(&mut self, config: AppConfig) {
        self.config = config.clone();
        self.portfolio_pct = config.portfolio_pct;
    }

    /// Add a point to the performance history chart
    pub fn push_history(&mut self, value: f64) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        self.history.push(HistoryPoint { t: now, v: value });
        // Keep last 1000 points (about 3 minutes at 200ms intervals)
        if self.history.len() > 1000 {
            self.history.remove(0);
        }
    }

    pub fn set_market_info(&mut self, symbol: &str, question: String) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.question = question;
    }

    pub fn set_market_metadata(&mut self, symbol: &str, price_to_beat: Option<f64>, market_end_ts: Option<u64>) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.last_price_to_beat = price_to_beat;
        state.last_market_end_ts = market_end_ts;
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

    /// Push BTC price for long-baseline spike calculation (called every tick)
    pub fn push_btc_price(&mut self, symbol: &str, btc_price: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.btc_price_history[state.btc_history_idx] = (btc_price, Instant::now());
        state.btc_history_idx = (state.btc_history_idx + 1) % 64;
        if state.btc_history_len < 64 { state.btc_history_len += 1; }
    }
    
    /// Update BTC trailing stop tracking for all open positions of a symbol
    /// Called on every BTC price update from the engine
    pub fn update_btc_trailing(&mut self, symbol: &str, current_btc: f64) {
        for pos in &mut self.open_positions {
            if pos.symbol != symbol { continue; }
            
            // entry_btc is now set immediately when position is created
            // Just update peak/trough here
            if current_btc > pos.peak_btc {
                pos.peak_btc = current_btc;
            }
            if current_btc < pos.trough_btc {
                pos.trough_btc = current_btc;
            }
        }
    }
    
    /// Push the current momentum value into ring buffer for exit smoothing
    pub fn push_spike_momentum(&mut self, symbol: &str, momentum: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.spike_history[state.spike_history_idx] = momentum;
        state.spike_history_idx = (state.spike_history_idx + 1) % 16;
        if state.spike_history_len < 16 { state.spike_history_len += 1; }
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

        // Calculate current time for hold mode
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // First pass: collect all data needed for decisions
        for (idx, pos) in self.open_positions.iter().enumerate() {
            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            if current_price <= 0.0 { continue; }

            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };
            
            let current_btc = state.last_binance;
            let held_ms = pos.entry_time.elapsed().as_millis() as u64;
            
            // Calculate time remaining for hold mode
            let time_remaining = state.last_market_end_ts.and_then(|end| {
                if end > now_secs { Some(end - now_secs) } else { None }
            });
            
            // HOLD MODE: share price > threshold AND < 30s remaining
            // In hold mode, we're likely to win but need to watch BTC closely
            let in_hold_mode = current_price > self.config.hold_min_share_price 
                && time_remaining.map_or(false, |t| t <= 30);
            
            if in_hold_mode {
                // In hold mode, only exit if BTC gets too close to price_to_beat
                // This is our edge: we see BTC moving 1.5-2s before Chainlink updates
                if let Some(ptb) = state.last_price_to_beat {
                    if current_btc > 0.0 {
                        let margin = if pos.direction == "UP" {
                            current_btc - ptb  // For UP: we win if BTC >= ptb
                        } else {
                            ptb - current_btc  // For DOWN: we win if BTC < ptb
                        };
                        
                        // Exit if margin drops below safety threshold
                        if margin < self.config.hold_safety_margin {
                            to_close.push((idx, "hold_safety_exit"));
                            continue;
                        } else {
                            // Safe margin, skip all other exits
                            continue;
                        }
                    }
                }
                // If we can't check margin, skip other exits anyway (hold mode)
                continue;
            }
            
            // Minimum hold time check - don't exit too early
            // Give Polymarket time to update their chart
            let min_hold_passed = held_ms >= self.config.min_hold_ms;

            // PTB-dynamic exit thresholds: adjust based on distance from price_to_beat
            // Positive margin = winning side, negative = losing side
            // Dead zone: -$30 to +$30 → no adjustment (normal exits)
            // Far winning (>$50): hold longer (wider thresholds, up to 1.5x)
            // Far losing (<-$30): exit faster (tighter thresholds, down to 0.6x)
            let ptb_factor = {
                let ptb_margin = if let Some(ptb) = state.last_price_to_beat {
                    if current_btc > 0.0 {
                        if pos.direction == "UP" { current_btc - ptb } else { ptb - current_btc }
                    } else { 0.0 }
                } else { 0.0 };

                if ptb_margin > 50.0 {
                    1.5  // Far winning: 50% wider → ride it out
                } else if ptb_margin > 30.0 {
                    1.0 + (ptb_margin - 30.0) / 20.0 * 0.5  // Linear 1.0→1.5
                } else if ptb_margin < -50.0 {
                    0.6  // Far losing: 40% tighter → exit fast
                } else if ptb_margin < -30.0 {
                    1.0 - ((-ptb_margin) - 30.0) / 20.0 * 0.4  // Linear 1.0→0.6
                } else {
                    1.0  // Dead zone: no adjustment
                }
            };

            // Trend reversed - exit if BTC reverses by a percentage of the entry spike from entry
            // Uses trend_reversal_pct (e.g., 50% of spike), floored at trend_reversal_threshold ($10)
            // PTB factor widens threshold when winning, tightens when losing
            let trend_reversed_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 {
                let reversal_threshold = (pos.entry_spike.abs() * (self.config.trend_reversal_pct / 100.0))
                    .max(self.config.trend_reversal_threshold) * ptb_factor;
                if pos.direction == "UP" {
                    // For UP: exit if BTC drops below threshold
                    current_btc < (pos.entry_btc - reversal_threshold)
                } else {
                    // For DOWN: exit if BTC rises above threshold
                    current_btc > (pos.entry_btc + reversal_threshold)
                }
            } else {
                false
            };
            
            // Trend reversed requires BOTH min_hold_ms AND 200ms persistence
            // This filters out single-tick flash spikes that bounce right back
            let trend_reversed_confirmed = trend_reversed_hit && pos.trend_reversed_since.map_or(false, |t| {
                t.elapsed().as_millis() >= 200
            });
            let trend_reversed = trend_reversed_confirmed && min_hold_passed;

            // Spike faded: exit if BTC reverses by X% of the peak favorable move from peak/trough
            // Uses the GREATER of entry_spike or total favorable move (peak_btc - entry_btc for UP)
            // This lets big winning runs breathe — if BTC ran $80, need $40 reversal (50%), not $10
            // PTB factor widens threshold when winning, tightens when losing
            let spike_faded_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 && pos.entry_spike.abs() > 0.0 {
                // Calculate total favorable move from entry
                let favorable_move = if pos.direction == "UP" {
                    pos.peak_btc - pos.entry_btc
                } else {
                    pos.entry_btc - pos.trough_btc
                };
                let reference_move = favorable_move.max(pos.entry_spike.abs());
                let threshold_dollars = reference_move * (self.config.spike_faded_pct / 100.0) * ptb_factor;
                if pos.direction == "UP" {
                    // For UP: check if BTC dropped from peak
                    let peak = pos.peak_btc;
                    if peak > 0.0 {
                        let drop_from_peak = peak - current_btc;
                        drop_from_peak >= threshold_dollars
                    } else {
                        false
                    }
                } else {
                    // For DOWN: check if BTC rose from trough
                    let trough = pos.trough_btc;
                    if trough > 0.0 {
                        let rise_from_trough = current_btc - trough;
                        rise_from_trough >= threshold_dollars
                    } else {
                        false
                    }
                }
            } else {
                false
            };

            // Spike faded requires persistence
            let spike_faded_confirmed = spike_faded_hit && pos.spike_faded_since.map_or(false, |t| {
                t.elapsed().as_millis() >= self.config.spike_faded_ms as u128
            });

            // Stop-loss: exit if share price drops by X% from entry
            // Works the same for both UP and DOWN - we're long shares, price drop = loss
            let stop_loss_hit = if self.config.stop_loss_pct > 0.0 && min_hold_passed {
                let stop_price = pos.entry_price * (1.0 - self.config.stop_loss_pct / 100.0);
                current_price <= stop_price
            } else {
                false
            };

            let near_end = held_ms > 295000;

            // Determine exit reason (priority order)
            let reason = if trend_reversed { 
                Some("trend_reversed") 
            } else if min_hold_passed && stop_loss_hit { 
                Some("stop_loss") 
            } else if min_hold_passed && spike_faded_confirmed { 
                Some("spike_faded") 
            } else if min_hold_passed && near_end { 
                Some("near_end") 
            } else { 
                None 
            };

            if let Some(r) = reason {
                if pos.hold_to_resolution && r != "trend_reversed" {
                    continue;
                }
                to_close.push((idx, r));
            }
        }

        // Update spike_faded_since for all positions
        for pos in self.open_positions.iter_mut() {
            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };
            
            let current_btc = state.last_binance;
            
            // Track spike_faded_since (uses same threshold as exit check)
            let spike_faded_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 && pos.entry_spike.abs() > 0.0 {
                let favorable_move = if pos.direction == "UP" {
                    pos.peak_btc - pos.entry_btc
                } else {
                    pos.entry_btc - pos.trough_btc
                };
                let reference_move = favorable_move.max(pos.entry_spike.abs());
                let threshold_dollars = reference_move * (self.config.spike_faded_pct / 100.0);
                if pos.direction == "UP" {
                    let peak = pos.peak_btc;
                    if peak > 0.0 {
                        let drop_from_peak = peak - current_btc;
                        drop_from_peak >= threshold_dollars
                    } else {
                        false
                    }
                } else {
                    let trough = pos.trough_btc;
                    if trough > 0.0 {
                        let rise_from_trough = current_btc - trough;
                        rise_from_trough >= threshold_dollars
                    } else {
                        false
                    }
                }
            } else {
                false
            };
            
            if spike_faded_hit {
                if pos.spike_faded_since.is_none() {
                    pos.spike_faded_since = Some(Instant::now());
                }
            } else {
                pos.spike_faded_since = None;
            }
            
            // Track trend_reversed_since (uses same scaled threshold as exit check)
            let trend_reversed_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 {
                let reversal_threshold = (pos.entry_spike.abs() * (self.config.trend_reversal_pct / 100.0))
                    .max(self.config.trend_reversal_threshold);
                if pos.direction == "UP" {
                    current_btc < (pos.entry_btc - reversal_threshold)
                } else {
                    current_btc > (pos.entry_btc + reversal_threshold)
                }
            } else {
                false
            };
            
            if trend_reversed_hit {
                if pos.trend_reversed_since.is_none() {
                    pos.trend_reversed_since = Some(Instant::now());
                }
            } else {
                pos.trend_reversed_since = None;
            }
        }

        if to_close.is_empty() {
            return false;
        }

        // Process closes
        to_close.sort_by_key(|k: &(usize, &str)| std::cmp::Reverse(k.0));
        let mut closed = false;
        for (idx, reason) in to_close {
            let pos = self.open_positions.remove(idx);
            
            // USE ACTUAL SPREAD FROM ORDERBOOK (not simulated)
            // For market SELL orders: we receive the BID price, not the mid
            let (bid, _ask) = self.get_bid_ask(&pos.symbol, &pos.direction);
            
            // If we have real orderbook data, use the bid price for sells
            // Otherwise fall back to mid price
            let fill_price = if bid > 0.0 {
                bid // Market sell fills at bid
            } else {
                self.get_share_price(&pos.symbol, &pos.direction) // Fall back to mid
            };
            
            let sell_fee = self.calculate_fee(pos.shares, fill_price);
            let net_revenue = (pos.shares * fill_price) - sell_fee;
            let pnl = net_revenue - (pos.position_size + pos.buy_fee);
            let state = self.symbol_states.get(&pos.symbol).cloned().unwrap_or_default();

            self.balance += net_revenue;
            self.trade_count += 1;
            if pnl > 0.0 { self.wins += 1; } else { self.losses += 1; }
            self.total_fees_paid += pos.buy_fee + sell_fee;
            self.total_volume += pos.position_size + (pos.shares * fill_price);
            self.trade_history.push(TradeRecord {
                symbol: pos.symbol.clone(),
                r#type: "exit".to_string(),
                question: state.question,
                direction: pos.direction.clone(),
                entry_price: Some(pos.entry_price),
                exit_price: Some(fill_price),
                shares: pos.shares,
                cost: pos.position_size,
                pnl: Some(pnl),
                cumulative_pnl: None,
                balance_after: None,
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: Some(reason.to_string()),
            });
            info!(symbol=%pos.symbol, pnl=format!("${:.4}", pnl), reason=%reason, fill_price=fill_price, "Position closed at bid price");
            closed = true;
        }
        closed
    }

    pub fn open_position(&mut self, symbol: &str, direction: &str, spike: f64, threshold_usd: f64, allow_scaling: bool, current_btc: f64) -> std::result::Result<u32, String> {
        let existing: Vec<_> = self.open_positions.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let pending_same = self.pending_entries.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();
        let scale_level = (existing.len() + pending_same) as u32 + 1;

        // In HOLD mode (allow_scaling=true), ignore MAX_SCALE_LEVEL to add to winning positions
        if !allow_scaling && scale_level > 1 { return Err("MAX_SCALE_LEVEL".to_string()); }

        let entry_price = self.get_share_price(symbol, direction);
        if entry_price <= 0.0 { return Err("NO_PRICE_DATA".to_string()); }
        if entry_price > self.config.max_entry_price { return Err("PRICE_TOO_HIGH".to_string()); }
        if entry_price < self.config.min_entry_price { return Err("PRICE_TOO_LOW".to_string()); }

        let position_size = self.balance * self.portfolio_pct * (1.0 / scale_level as f64);
        
        // Position sizing based on share price - scale down for cheaper shares
        // Cheap shares (< 20 cents) are higher risk, use smaller position size
        let position_size = if entry_price < 0.20 {
            position_size * 0.25  // 25% of normal for very cheap shares
        } else if entry_price < 0.50 {
            position_size * 0.50  // 50% of normal for cheap shares
        } else {
            position_size  // Full size for 50+ cent shares
        };

        // Cap position size at $20 — enter with max instead of rejecting
        let position_size = position_size.min(20.0);

        let shares = position_size / entry_price;
        let buy_fee = self.calculate_fee(shares, entry_price);
        if (position_size + buy_fee) > self.balance { return Err("INSUFFICIENT_BALANCE".to_string()); }
        if position_size < 1.0 { return Err("BELOW_MIN_ORDER_SIZE".to_string()); }

        // Reserve balance immediately so concurrent entries don't over-allocate
        self.balance -= position_size + buy_fee;

        let spike_bonus = (spike.abs() - threshold_usd) * self.config.spike_scaling_factor;
        let _profit_target = entry_price * (1.0 + self.config.profit_target_pct + spike_bonus);

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
            entry_btc: current_btc,  // Set BTC price immediately to avoid race condition
        });

        Ok(scale_level)
    }

    /// Roll back a pending entry that failed to execute on the live wallet.
    /// Removes the pending entry and restores the reserved balance.
    pub fn rollback_pending_entry(&mut self, symbol: &str, direction: &str) {
        if let Some(idx) = self.pending_entries.iter().position(|p| p.symbol == symbol && p.direction == direction) {
            let entry = self.pending_entries.remove(idx);
            self.balance += entry.position_size + entry.buy_fee;
            info!(symbol=%symbol, direction=%direction, restored=entry.position_size + entry.buy_fee, "Rolled back paper entry (live wallet failed)");
        }
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
            
            // USE ACTUAL SPREAD FROM ORDERBOOK (not simulated)
            // For market BUY orders: we pay the ASK price, not the mid
            // This is real slippage from the actual orderbook
            let (_bid, ask) = self.get_bid_ask(&p.symbol, &p.direction);
            
            // If we have real orderbook data, use the ask price for buys
            // Otherwise fall back to mid price
            let fill_price = if ask > 0.0 {
                ask // Market buy fills at ask
            } else {
                p.entry_price // Fall back to mid price
            };
            
            // Recalculate shares at actual fill price
            let actual_shares = p.position_size / fill_price;
            let actual_fee = self.calculate_fee(actual_shares, fill_price);
            
            // Adjust balance if fee changed
            let fee_diff = actual_fee - p.buy_fee;
            if fee_diff > 0.0 && self.balance >= fee_diff {
                self.balance -= fee_diff;
            } else if fee_diff < 0.0 {
                self.balance -= fee_diff; // Add back savings
            }
            
            self.trade_history.push(TradeRecord {
                symbol: p.symbol.clone(),
                r#type: "entry".to_string(),
                question: state.question.clone(),
                direction: p.direction.clone(),
                entry_price: Some(fill_price),
                exit_price: None,
                shares: actual_shares,
                cost: p.position_size,
                pnl: None,
                cumulative_pnl: None,
                balance_after: None,
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: None,
            });
            let sym = p.symbol.clone();
            let dir = p.direction.clone();
            let level = p.scale_level;
            let slippage = fill_price - p.entry_price;

            // Use BTC price at FILL time (now), not signal time (300ms ago)
            // This makes BTC-based exits (trend_reversed, spike_faded) more accurate
            let entry_btc = if state.last_binance > 0.0 { state.last_binance } else { p.entry_btc };

            self.open_positions.push(OpenPosition {
                symbol: p.symbol,
                direction: p.direction,
                entry_price: fill_price,
                avg_entry_price: fill_price,
                shares: actual_shares,
                position_size: p.position_size,
                buy_fee: actual_fee,
                entry_spike: p.spike,
                entry_time: Instant::now(),
                highest_price: fill_price,
                scale_level: p.scale_level,
                hold_to_resolution: false,
                peak_spike: p.spike.abs(),
                // BTC price at fill time for accurate exit calibration
                entry_btc,
                peak_btc: entry_btc,
                trough_btc: entry_btc,
                spike_faded_since: None,
                trend_reversed_since: None,
            });
            info!(symbol=%sym, direction=%dir, requested_price=p.entry_price, fill_price=fill_price, spread_slippage=slippage, shares=actual_shares, level=level, "Position opened at ask price");
        }
    }
}
