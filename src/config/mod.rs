use crate::error::Result;
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub threshold_bps: u64,
    pub min_hold_ms: u64,
    pub starting_balance: f64,
    pub portfolio_pct: f64,
    pub paper_trading: bool,
    pub crypto_fee_rate: f64,
    pub max_entry_price: f64,
    pub profit_target_pct: f64,
    pub timeout_ms: u64,
    pub trailing_stop_pct: f64,
    pub max_spread_bps: u64,
    pub spike_scaling_factor: f64,
    pub ema_alpha: f64,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let threshold_bps: u64 = env::var("THRESHOLD_BPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000); // $20 spike minimum

        let min_hold_ms: u64 = env::var("MIN_HOLD_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1500);

        let starting_balance: f64 = env::var("STARTING_BALANCE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(27.0);

        let portfolio_pct: f64 = env::var("PORTFOLIO_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.10);

        let paper_trading: bool = env::var("PAPER_TRADING")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(true);

        let crypto_fee_rate: f64 = env::var("CRYPTO_FEE_RATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.072);

        let max_entry_price: f64 = env::var("MAX_ENTRY_PRICE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.70);

        let profit_target_pct: f64 = env::var("PROFIT_TARGET_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0); // 0% profit target (trade by spike)

        let timeout_ms: u64 = env::var("TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10000);

        let trailing_stop_pct: f64 = env::var("TRAILING_STOP_PCT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5.0); // 5% trailing stop from highest

        let max_spread_bps: u64 = env::var("MAX_SPREAD_BPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100); // 1.0% max spread

        let spike_scaling_factor: f64 = env::var("SPIKE_SCALING_FACTOR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0001); // Scale profit target by 0.01% per $1 spike

        let ema_alpha: f64 = env::var("EMA_ALPHA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.1); // EMA alpha for baseline

        Ok(Self {
            threshold_bps,
            min_hold_ms,
            starting_balance,
            portfolio_pct,
            paper_trading,
            crypto_fee_rate,
            max_entry_price,
            profit_target_pct,
            timeout_ms,
            trailing_stop_pct,
            max_spread_bps,
            spike_scaling_factor,
            ema_alpha,
        })
    }

    pub fn update_from_json(&mut self, json: serde_json::Value) -> Result<()> {
        if let Some(v) = json.get("threshold_bps").and_then(|v| v.as_u64()) {
            self.threshold_bps = v;
        }
        if let Some(v) = json.get("portfolio_pct").and_then(|v| v.as_f64()) {
            self.portfolio_pct = v;
        }
        if let Some(v) = json.get("min_hold_ms").and_then(|v| v.as_u64()) {
            self.min_hold_ms = v;
        }
        if let Some(v) = json.get("profit_target_pct").and_then(|v| v.as_f64()) {
            self.profit_target_pct = v;
        }
        if let Some(v) = json.get("trailing_stop_pct").and_then(|v| v.as_f64()) {
            self.trailing_stop_pct = v;
        }
        if let Some(v) = json.get("max_spread_bps").and_then(|v| v.as_u64()) {
            self.max_spread_bps = v;
        }
        if let Some(v) = json.get("timeout_ms").and_then(|v| v.as_u64()) {
            self.timeout_ms = v;
        }
        Ok(())
    }
}
