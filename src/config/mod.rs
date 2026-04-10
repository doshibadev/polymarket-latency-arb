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
    pub max_spread_bps: u64,
    pub spike_scaling_factor: f64,
    pub ema_alpha: f64,
    pub execution_delay_ms: u64,
    pub hold_margin_per_second: f64, // required $ margin per second remaining to hold to resolution
    pub hold_max_seconds: u64,       // only activate hold mode when <= this many seconds remain
    pub hold_max_crossings: usize,   // max price-to-beat crossings in history before refusing to hold
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
            max_entry_price: env::var("MAX_ENTRY_PRICE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.70),
            profit_target_pct: env::var("PROFIT_TARGET_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0),
            trailing_stop_pct: env::var("TRAILING_STOP_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(10.0),
            spike_faded_pct: env::var("SPIKE_FADED_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.25),
            max_spread_bps: env::var("MAX_SPREAD_BPS").ok().and_then(|v| v.parse().ok()).unwrap_or(100),
            spike_scaling_factor: env::var("SPIKE_SCALING_FACTOR").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0001),
            ema_alpha: env::var("EMA_ALPHA").ok().and_then(|v| v.parse().ok()).unwrap_or(0.1),
            execution_delay_ms: env::var("EXECUTION_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(300),
            hold_margin_per_second: env::var("HOLD_MARGIN_PER_SECOND").ok().and_then(|v| v.parse().ok()).unwrap_or(1.5),
            hold_max_seconds: env::var("HOLD_MAX_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(90),
            hold_max_crossings: env::var("HOLD_MAX_CROSSINGS").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
        })
    }

    pub fn update_from_json(&mut self, json: serde_json::Value) -> Result<()> {
        if let Some(v) = json.get("threshold_bps").and_then(|v| v.as_u64()) { self.threshold_bps = v; }
        if let Some(v) = json.get("portfolio_pct").and_then(|v| v.as_f64()) { self.portfolio_pct = v; }
        if let Some(v) = json.get("profit_target_pct").and_then(|v| v.as_f64()) { self.profit_target_pct = v; }
        if let Some(v) = json.get("trailing_stop_pct").and_then(|v| v.as_f64()) { self.trailing_stop_pct = v; }
        if let Some(v) = json.get("spike_faded_pct").and_then(|v| v.as_f64()) { self.spike_faded_pct = v; }
        if let Some(v) = json.get("max_spread_bps").and_then(|v| v.as_u64()) { self.max_spread_bps = v; }
        if let Some(v) = json.get("max_entry_price").and_then(|v| v.as_f64()) { self.max_entry_price = v; }
        if let Some(v) = json.get("spike_scaling_factor").and_then(|v| v.as_f64()) { self.spike_scaling_factor = v; }
        if let Some(v) = json.get("ema_alpha").and_then(|v| v.as_f64()) { self.ema_alpha = v; }
        if let Some(v) = json.get("execution_delay_ms").and_then(|v| v.as_u64()) { self.execution_delay_ms = v; }
        Ok(())
    }
}
