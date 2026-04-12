use crate::error::Result;
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub threshold_bps: u64,
    pub starting_balance: f64,
    pub portfolio_pct: f64,
    pub paper_trading: bool,
    pub crypto_fee_rate: f64,
    pub max_entry_price: f64,
    pub profit_target_pct: f64,
    pub trailing_stop_pct: f64,
    pub spike_faded_pct: f64,
    pub spike_faded_ms: u64,       // ms the reversal must persist before exiting
    pub min_hold_ms: u64,          // minimum hold time before any exit (except extreme reversal)
    pub max_spread_bps: u64,
    pub spike_scaling_factor: f64,
    pub ema_alpha: f64,
    pub execution_delay_ms: u64,
    pub spike_sustain_ms: u64,       // ms to wait before entering on spike detection
    pub hold_margin_per_second: f64, // required $ margin per second remaining to hold to resolution
    pub hold_max_seconds: u64,       // only activate hold mode when <= this many seconds remain
    pub hold_max_crossings: usize,
    pub min_price_distance: f64,
    pub min_entry_price: f64,
    pub max_orders_per_minute: u32,
    pub max_daily_loss: f64,
    pub max_exposure_per_market: f64,
    pub max_drawdown_pct: f64,
    pub slippage_bps: u64,           // simulated slippage in basis points for paper trading
    pub trend_reversal_pct: f64,      // % of spike that must reverse to trigger trend_reversed exit
    pub stop_loss_pct: f64,           // % drop from entry price to trigger stop-loss exit (0 = disabled)
    pub hold_safety_margin: f64,      // $ margin from price_to_beat to exit in hold mode (e.g., 20 = exit if within $20)
    pub hold_min_share_price: f64,    // minimum share price to enter hold mode (e.g., 0.80 = 80 cents)
    pub early_exit_loss_pct: f64,     // % loss threshold for early exit before market end (e.g., 0.20 = 20%)
    pub trend_reversal_threshold: f64, // $ minimum BTC reversal to trigger trend_reversed (e.g., 10.0 = $10)
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();

        Ok(Self {
            threshold_bps: env::var("THRESHOLD_BPS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000),
            starting_balance: env::var("STARTING_BALANCE").ok().and_then(|v| v.parse().ok()).unwrap_or(27.0),
            portfolio_pct: env::var("PORTFOLIO_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.20),
            paper_trading: env::var("PAPER_TRADING").ok().and_then(|v| v.parse().ok()).unwrap_or(true),
            crypto_fee_rate: env::var("CRYPTO_FEE_RATE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.072),
            max_entry_price: env::var("MAX_ENTRY_PRICE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.80),
            profit_target_pct: env::var("PROFIT_TARGET_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            trailing_stop_pct: env::var("TRAILING_STOP_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0),
            spike_faded_pct: env::var("SPIKE_FADED_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0),
            spike_faded_ms: env::var("SPIKE_FADED_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(200),
            min_hold_ms: env::var("MIN_HOLD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500),
            max_spread_bps: env::var("MAX_SPREAD_BPS").ok().and_then(|v| v.parse().ok()).unwrap_or(100),
            spike_scaling_factor: env::var("SPIKE_SCALING_FACTOR").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0001),
            ema_alpha: env::var("EMA_ALPHA").ok().and_then(|v| v.parse().ok()).unwrap_or(0.1),
            execution_delay_ms: env::var("EXECUTION_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(300),
            spike_sustain_ms: env::var("SPIKE_SUSTAIN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(50),
            hold_margin_per_second: env::var("HOLD_MARGIN_PER_SECOND").ok().and_then(|v| v.parse().ok()).unwrap_or(1.5),
            hold_max_seconds: env::var("HOLD_MAX_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(90),
            hold_max_crossings: env::var("HOLD_MAX_CROSSINGS").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
            min_price_distance: env::var("MIN_PRICE_DISTANCE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.05),
            min_entry_price: env::var("MIN_ENTRY_PRICE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.05),
            max_orders_per_minute: env::var("MAX_ORDERS_PER_MINUTE").ok().and_then(|v| v.parse().ok()).unwrap_or(20),
            max_daily_loss: env::var("MAX_DAILY_LOSS").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0),
            max_exposure_per_market: env::var("MAX_EXPOSURE_PER_MARKET").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0),
            max_drawdown_pct: env::var("MAX_DRAWDOWN_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.30),
            slippage_bps: env::var("SLIPPAGE_BPS").ok().and_then(|v| v.parse().ok()).unwrap_or(50),
            trend_reversal_pct: env::var("TREND_REVERSAL_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(50.0),
            stop_loss_pct: env::var("STOP_LOSS_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(30.0),
            hold_safety_margin: env::var("HOLD_SAFETY_MARGIN").ok().and_then(|v| v.parse().ok()).unwrap_or(20.0),
            hold_min_share_price: env::var("HOLD_MIN_SHARE_PRICE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.80),
            early_exit_loss_pct: env::var("EARLY_EXIT_LOSS_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.20),
            trend_reversal_threshold: env::var("TREND_REVERSAL_THRESHOLD").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0),
        })
    }

    pub fn update_from_json(&mut self, json: serde_json::Value) -> Result<()> {
        if let Some(v) = json.get("threshold_bps").and_then(|v| v.as_u64()) { self.threshold_bps = v; }
        if let Some(v) = json.get("portfolio_pct").and_then(|v| v.as_f64()) { self.portfolio_pct = v; }
        if let Some(v) = json.get("max_entry_price").and_then(|v| v.as_f64()) { self.max_entry_price = v; }
        if let Some(v) = json.get("min_entry_price").and_then(|v| v.as_f64()) { self.min_entry_price = v; }
        if let Some(v) = json.get("trend_reversal_pct").and_then(|v| v.as_f64()) { self.trend_reversal_pct = v; }
        if let Some(v) = json.get("spike_faded_pct").and_then(|v| v.as_f64()) { self.spike_faded_pct = v; }
        if let Some(v) = json.get("spike_faded_ms").and_then(|v| v.as_u64()) { self.spike_faded_ms = v; }
        if let Some(v) = json.get("min_hold_ms").and_then(|v| v.as_u64()) { self.min_hold_ms = v; }
        if let Some(v) = json.get("execution_delay_ms").and_then(|v| v.as_u64()) { self.execution_delay_ms = v; }
        if let Some(v) = json.get("max_orders_per_minute").and_then(|v| v.as_u64()) { self.max_orders_per_minute = v as u32; }
        if let Some(v) = json.get("max_daily_loss").and_then(|v| v.as_f64()) { self.max_daily_loss = v; }
        if let Some(v) = json.get("max_exposure_per_market").and_then(|v| v.as_f64()) { self.max_exposure_per_market = v; }
        if let Some(v) = json.get("max_drawdown_pct").and_then(|v| v.as_f64()) { self.max_drawdown_pct = v; }
        if let Some(v) = json.get("stop_loss_pct").and_then(|v| v.as_f64()) { self.stop_loss_pct = v; }
        if let Some(v) = json.get("hold_safety_margin").and_then(|v| v.as_f64()) { self.hold_safety_margin = v; }
        if let Some(v) = json.get("hold_min_share_price").and_then(|v| v.as_f64()) { self.hold_min_share_price = v; }
        if let Some(v) = json.get("early_exit_loss_pct").and_then(|v| v.as_f64()) { self.early_exit_loss_pct = v; }
        if let Some(v) = json.get("trend_reversal_threshold").and_then(|v| v.as_f64()) { self.trend_reversal_threshold = v; }
        Ok(())
    }
}
