use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

static POSITION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn next_position_id(symbol: &str, direction: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = POSITION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{symbol}-{direction}-{ts}-{seq}")
}

/// Position model shared by paper simulation and live execution.
#[derive(Clone, Serialize)]
pub struct OpenPosition {
    pub position_id: String,
    pub symbol: String,
    pub direction: String,
    pub entry_price: f64,
    pub avg_entry_price: f64,
    pub shares: f64,
    pub position_size: f64,
    pub buy_fee: f64,
    pub entry_spike: f64,
    #[serde(skip)]
    pub entry_time: Instant,
    pub highest_price: f64,
    pub scale_level: u32,
    pub hold_to_resolution: bool,
    pub peak_spike: f64,
    pub entry_btc: f64,
    pub peak_btc: f64,
    pub trough_btc: f64,
    #[serde(skip)]
    pub spike_faded_since: Option<Instant>,
    #[serde(skip)]
    pub trend_reversed_since: Option<Instant>,
    pub trailing_stop_activated: bool,
    pub on_chain_shares: Option<f64>,
    pub ptb_tier_at_entry: Option<String>,
    pub entry_mode: Option<String>,
    pub exit_suppressed_count: u32,
    #[serde(skip)]
    pub last_suppressed_exit_signal: Option<(String, Instant)>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TradeRecord {
    #[serde(default)]
    pub position_id: Option<String>,
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
    #[serde(default)]
    pub btc_at_entry: Option<f64>,
    #[serde(default)]
    pub price_to_beat_at_entry: Option<f64>,
    #[serde(default)]
    pub ptb_margin_at_entry: Option<f64>,
    #[serde(default)]
    pub seconds_to_expiry_at_entry: Option<u64>,
    #[serde(default)]
    pub spread_at_entry: Option<f64>,
    #[serde(default)]
    pub round_trip_loss_pct_at_entry: Option<f64>,
    #[serde(default)]
    pub signal_score: Option<f64>,
    #[serde(default)]
    pub ptb_margin_at_exit: Option<f64>,
    #[serde(default)]
    pub exit_mode: Option<String>,
    #[serde(default)]
    pub favorable_ptb_at_exit: Option<bool>,
    #[serde(default)]
    pub ptb_tier_at_entry: Option<String>,
    #[serde(default)]
    pub ptb_tier_at_exit: Option<String>,
    #[serde(default)]
    pub entry_mode: Option<String>,
    #[serde(default)]
    pub exit_suppressed_count: Option<u32>,
}

/// Entry plan produced once by shared strategy logic and consumed by either executor.
#[derive(Clone)]
pub struct PendingEntry {
    pub position_id: String,
    pub symbol: String,
    pub direction: String,
    pub spike: f64,
    pub entry_price: f64,
    pub scale_level: u32,
    pub position_size: f64,
    pub shares: f64,
    pub buy_fee: f64,
    pub submitted_at: Instant,
    pub entry_btc: f64,
    pub live_synced: bool,
    pub price_to_beat_at_entry: Option<f64>,
    pub ptb_margin_at_entry: Option<f64>,
    pub seconds_to_expiry_at_entry: Option<u64>,
    pub spread_at_entry: Option<f64>,
    pub round_trip_loss_pct_at_entry: Option<f64>,
    pub signal_score: Option<f64>,
    pub ptb_tier_at_entry: Option<String>,
    pub entry_mode: Option<String>,
}
