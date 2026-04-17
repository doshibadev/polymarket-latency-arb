use crate::error::Result;
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub threshold_bps: u64,
    pub eth_threshold_bps: u64,
    pub starting_balance: f64,
    pub portfolio_pct: f64,
    pub paper_trading: bool,
    pub crypto_fee_rate: f64,
    pub max_entry_price: f64,
    pub trailing_stop_pct: f64,
    pub spike_faded_pct: f64,
    pub spike_faded_ms: u64, // ms the reversal must persist before exiting
    pub min_hold_ms: u64,    // minimum hold time before any exit (except extreme reversal)
    pub execution_delay_ms: u64,
    pub spike_sustain_ms: u64, // ms to wait before entering on spike detection
    pub hold_margin_per_second: f64, // required $ margin per second remaining to hold to resolution
    pub eth_hold_margin_per_second: f64,
    pub hold_max_seconds: u64, // only activate hold mode when <= this many seconds remain
    pub hold_max_crossings: usize,
    pub min_price_distance: f64,
    pub min_entry_price: f64,
    pub max_orders_per_minute: u32,
    pub max_daily_loss: f64,
    pub max_exposure_per_market: f64,
    pub max_drawdown_pct: f64,
    pub trend_reversal_pct: f64, // % of spike that must reverse to trigger trend_reversed exit
    pub stop_loss_pct: f64,      // % drop from entry price to trigger stop-loss exit (0 = disabled)
    pub hold_safety_margin: f64, // $ margin from price_to_beat to exit in hold mode (e.g., 20 = exit if within $20)
    pub eth_hold_safety_margin: f64,
    pub hold_min_share_price: f64, // minimum share price to enter hold mode (e.g., 0.80 = 80 cents)
    pub early_exit_loss_pct: f64, // % loss threshold for early exit before market end (e.g., 0.20 = 20%)
    pub trend_reversal_threshold: f64, // $ minimum BTC reversal to trigger trend_reversed (e.g., 10.0 = $10)
    pub eth_trend_reversal_threshold: f64,
    // Trend filter: prevent counter-trend entries
    pub trend_filter_enabled: bool,   // master switch for trend filter
    pub trend_min_magnitude_usd: f64, // minimum trend size (USD) to trigger counter-trend penalty
    pub eth_trend_min_magnitude_usd: f64,
    pub counter_trend_multiplier: f64, // max multiplier for counter-trend threshold (e.g., 2.5x)
    pub trend_max_magnitude_usd: f64,  // trend size (USD) at which multiplier maxes out
    pub eth_trend_max_magnitude_usd: f64,
    pub ptb_neutral_zone_usd: f64, // within this distance of ptb, no directional penalty
    pub eth_ptb_neutral_zone_usd: f64,
    pub ptb_max_counter_distance_usd: f64, // hard reject counter-ptb trades beyond this distance
    pub eth_ptb_max_counter_distance_usd: f64,
    pub trailing_stop_activation: f64, // % gain above entry to activate trailing stop (20 = 20%)
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();

        Ok(Self {
            threshold_bps: env::var("THRESHOLD_BPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000),
            eth_threshold_bps: env::var("ETH_THRESHOLD_BPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
            starting_balance: env::var("STARTING_BALANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(27.0),
            portfolio_pct: env::var("PORTFOLIO_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.20),
            paper_trading: env::var("PAPER_TRADING")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            crypto_fee_rate: env::var("CRYPTO_FEE_RATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.072),
            max_entry_price: env::var("MAX_ENTRY_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.80),
            trailing_stop_pct: env::var("TRAILING_STOP_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10.0),
            spike_faded_pct: env::var("SPIKE_FADED_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50.0),
            spike_faded_ms: env::var("SPIKE_FADED_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200),
            min_hold_ms: env::var("MIN_HOLD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1500),
            execution_delay_ms: env::var("EXECUTION_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            spike_sustain_ms: env::var("SPIKE_SUSTAIN_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
            hold_margin_per_second: env::var("HOLD_MARGIN_PER_SECOND")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.5),
            eth_hold_margin_per_second: env::var("ETH_HOLD_MARGIN_PER_SECOND")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.05),
            hold_max_seconds: env::var("HOLD_MAX_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(90),
            hold_max_crossings: env::var("HOLD_MAX_CROSSINGS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
            min_price_distance: env::var("MIN_PRICE_DISTANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.05),
            min_entry_price: env::var("MIN_ENTRY_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.05),
            max_orders_per_minute: env::var("MAX_ORDERS_PER_MINUTE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20),
            max_daily_loss: env::var("MAX_DAILY_LOSS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10.0),
            max_exposure_per_market: env::var("MAX_EXPOSURE_PER_MARKET")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50.0),
            max_drawdown_pct: env::var("MAX_DRAWDOWN_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.30),
            trend_reversal_pct: env::var("TREND_REVERSAL_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50.0),
            stop_loss_pct: env::var("STOP_LOSS_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30.0),
            hold_safety_margin: env::var("HOLD_SAFETY_MARGIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20.0),
            eth_hold_safety_margin: env::var("ETH_HOLD_SAFETY_MARGIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.75),
            hold_min_share_price: env::var("HOLD_MIN_SHARE_PRICE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.80),
            early_exit_loss_pct: env::var("EARLY_EXIT_LOSS_PCT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.20),
            trend_reversal_threshold: env::var("TREND_REVERSAL_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10.0),
            eth_trend_reversal_threshold: env::var("ETH_TREND_REVERSAL_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.5),
            trend_filter_enabled: env::var("TREND_FILTER_ENABLED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            trend_min_magnitude_usd: env::var("TREND_MIN_MAGNITUDE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30.0),
            eth_trend_min_magnitude_usd: env::var("ETH_TREND_MIN_MAGNITUDE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.5),
            counter_trend_multiplier: env::var("COUNTER_TREND_MULTIPLIER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.5),
            trend_max_magnitude_usd: env::var("TREND_MAX_MAGNITUDE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(150.0),
            eth_trend_max_magnitude_usd: env::var("ETH_TREND_MAX_MAGNITUDE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(7.5),
            ptb_neutral_zone_usd: env::var("PTB_NEUTRAL_ZONE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20.0),
            eth_ptb_neutral_zone_usd: env::var("ETH_PTB_NEUTRAL_ZONE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0),
            ptb_max_counter_distance_usd: env::var("PTB_MAX_COUNTER_DISTANCE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200.0),
            eth_ptb_max_counter_distance_usd: env::var("ETH_PTB_MAX_COUNTER_DISTANCE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10.0),
            trailing_stop_activation: env::var("TRAILING_STOP_ACTIVATION")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20.0),
        })
    }

    pub fn update_from_json(&mut self, json: serde_json::Value) -> Result<()> {
        if let Some(v) = json.get("threshold_bps").and_then(|v| v.as_u64()) {
            self.threshold_bps = v;
        }
        if let Some(v) = json.get("eth_threshold_bps").and_then(|v| v.as_u64()) {
            self.eth_threshold_bps = v;
        }
        if let Some(v) = json.get("portfolio_pct").and_then(|v| v.as_f64()) {
            self.portfolio_pct = v;
        }
        if let Some(v) = json.get("crypto_fee_rate").and_then(|v| v.as_f64()) {
            self.crypto_fee_rate = v;
        }
        if let Some(v) = json.get("max_entry_price").and_then(|v| v.as_f64()) {
            self.max_entry_price = v;
        }
        if let Some(v) = json.get("min_entry_price").and_then(|v| v.as_f64()) {
            self.min_entry_price = v;
        }
        if let Some(v) = json.get("trend_reversal_pct").and_then(|v| v.as_f64()) {
            self.trend_reversal_pct = v;
        }
        if let Some(v) = json
            .get("trend_reversal_threshold")
            .and_then(|v| v.as_f64())
        {
            self.trend_reversal_threshold = v;
        }
        if let Some(v) = json
            .get("eth_trend_reversal_threshold")
            .and_then(|v| v.as_f64())
        {
            self.eth_trend_reversal_threshold = v;
        }
        if let Some(v) = json.get("spike_faded_pct").and_then(|v| v.as_f64()) {
            self.spike_faded_pct = v;
        }
        if let Some(v) = json.get("spike_faded_ms").and_then(|v| v.as_u64()) {
            self.spike_faded_ms = v;
        }
        if let Some(v) = json.get("min_hold_ms").and_then(|v| v.as_u64()) {
            self.min_hold_ms = v;
        }
        if let Some(v) = json.get("trailing_stop_pct").and_then(|v| v.as_f64()) {
            self.trailing_stop_pct = v;
        }
        if let Some(v) = json
            .get("trailing_stop_activation")
            .and_then(|v| v.as_f64())
        {
            self.trailing_stop_activation = v;
        }
        if let Some(v) = json.get("stop_loss_pct").and_then(|v| v.as_f64()) {
            self.stop_loss_pct = v;
        }
        if let Some(v) = json.get("hold_safety_margin").and_then(|v| v.as_f64()) {
            self.hold_safety_margin = v;
        }
        if let Some(v) = json.get("eth_hold_safety_margin").and_then(|v| v.as_f64()) {
            self.eth_hold_safety_margin = v;
        }
        if let Some(v) = json.get("hold_min_share_price").and_then(|v| v.as_f64()) {
            self.hold_min_share_price = v;
        }
        if let Some(v) = json.get("early_exit_loss_pct").and_then(|v| v.as_f64()) {
            self.early_exit_loss_pct = v;
        }
        if let Some(v) = json.get("hold_margin_per_second").and_then(|v| v.as_f64()) {
            self.hold_margin_per_second = v;
        }
        if let Some(v) = json
            .get("eth_hold_margin_per_second")
            .and_then(|v| v.as_f64())
        {
            self.eth_hold_margin_per_second = v;
        }
        if let Some(v) = json.get("hold_max_seconds").and_then(|v| v.as_u64()) {
            self.hold_max_seconds = v;
        }
        if let Some(v) = json.get("hold_max_crossings").and_then(|v| v.as_u64()) {
            self.hold_max_crossings = v as usize;
        }
        if let Some(v) = json.get("spike_sustain_ms").and_then(|v| v.as_u64()) {
            self.spike_sustain_ms = v;
        }
        if let Some(v) = json.get("execution_delay_ms").and_then(|v| v.as_u64()) {
            self.execution_delay_ms = v;
        }
        if let Some(v) = json.get("min_price_distance").and_then(|v| v.as_f64()) {
            self.min_price_distance = v;
        }
        if let Some(v) = json.get("max_orders_per_minute").and_then(|v| v.as_u64()) {
            self.max_orders_per_minute = v as u32;
        }
        if let Some(v) = json.get("max_daily_loss").and_then(|v| v.as_f64()) {
            self.max_daily_loss = v;
        }
        if let Some(v) = json.get("max_exposure_per_market").and_then(|v| v.as_f64()) {
            self.max_exposure_per_market = v;
        }
        if let Some(v) = json.get("max_drawdown_pct").and_then(|v| v.as_f64()) {
            self.max_drawdown_pct = v;
        }
        if let Some(v) = json.get("trend_filter_enabled").and_then(|v| v.as_bool()) {
            self.trend_filter_enabled = v;
        }
        if let Some(v) = json.get("trend_min_magnitude_usd").and_then(|v| v.as_f64()) {
            self.trend_min_magnitude_usd = v;
        }
        if let Some(v) = json
            .get("eth_trend_min_magnitude_usd")
            .and_then(|v| v.as_f64())
        {
            self.eth_trend_min_magnitude_usd = v;
        }
        if let Some(v) = json
            .get("counter_trend_multiplier")
            .and_then(|v| v.as_f64())
        {
            self.counter_trend_multiplier = v;
        }
        if let Some(v) = json.get("trend_max_magnitude_usd").and_then(|v| v.as_f64()) {
            self.trend_max_magnitude_usd = v;
        }
        if let Some(v) = json
            .get("eth_trend_max_magnitude_usd")
            .and_then(|v| v.as_f64())
        {
            self.eth_trend_max_magnitude_usd = v;
        }
        if let Some(v) = json.get("ptb_neutral_zone_usd").and_then(|v| v.as_f64()) {
            self.ptb_neutral_zone_usd = v;
        }
        if let Some(v) = json
            .get("eth_ptb_neutral_zone_usd")
            .and_then(|v| v.as_f64())
        {
            self.eth_ptb_neutral_zone_usd = v;
        }
        if let Some(v) = json
            .get("ptb_max_counter_distance_usd")
            .and_then(|v| v.as_f64())
        {
            self.ptb_max_counter_distance_usd = v;
        }
        if let Some(v) = json
            .get("eth_ptb_max_counter_distance_usd")
            .and_then(|v| v.as_f64())
        {
            self.eth_ptb_max_counter_distance_usd = v;
        }
        Ok(())
    }

    pub fn threshold_bps_for(&self, symbol: &str) -> u64 {
        match symbol {
            "ETH" => self.eth_threshold_bps,
            _ => self.threshold_bps,
        }
    }

    pub fn threshold_usd_for(&self, symbol: &str) -> f64 {
        self.threshold_bps_for(symbol) as f64 / 100.0
    }

    pub fn trend_reversal_threshold_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_trend_reversal_threshold,
            _ => self.trend_reversal_threshold,
        }
    }

    pub fn hold_safety_margin_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_hold_safety_margin,
            _ => self.hold_safety_margin,
        }
    }

    pub fn hold_margin_per_second_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_hold_margin_per_second,
            _ => self.hold_margin_per_second,
        }
    }

    pub fn trend_min_magnitude_usd_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_trend_min_magnitude_usd,
            _ => self.trend_min_magnitude_usd,
        }
    }

    pub fn trend_max_magnitude_usd_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_trend_max_magnitude_usd,
            _ => self.trend_max_magnitude_usd,
        }
    }

    pub fn ptb_neutral_zone_usd_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_ptb_neutral_zone_usd,
            _ => self.ptb_neutral_zone_usd,
        }
    }

    pub fn ptb_max_counter_distance_usd_for(&self, symbol: &str) -> f64 {
        match symbol {
            "ETH" => self.eth_ptb_max_counter_distance_usd,
            _ => self.ptb_max_counter_distance_usd,
        }
    }

    pub fn ptb_favorable_margin_for(&self, symbol: &str) -> f64 {
        self.ptb_neutral_zone_usd_for(symbol) * 1.5
    }

    pub fn ptb_strong_margin_for(&self, symbol: &str) -> f64 {
        self.ptb_neutral_zone_usd_for(symbol) * 5.0
    }

    pub fn ptb_extreme_margin_for(&self, symbol: &str) -> f64 {
        self.ptb_neutral_zone_usd_for(symbol) * 7.5
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;
    use serde_json::json;

    #[test]
    fn update_from_json_updates_extended_config_surface() {
        let mut config = AppConfig::load().expect("config should load");
        config
            .update_from_json(json!({
                "crypto_fee_rate": 0.08,
                "eth_threshold_bps": 125,
                "trend_reversal_threshold": 12.0,
                "eth_trend_reversal_threshold": 0.6,
                "hold_max_crossings": 4,
                "spike_sustain_ms": 75,
                "trend_filter_enabled": false,
                "eth_trend_min_magnitude_usd": 1.8,
                "trend_max_magnitude_usd": 220.0,
                "eth_trend_max_magnitude_usd": 8.5,
                "ptb_neutral_zone_usd": 33.0,
                "eth_ptb_neutral_zone_usd": 1.2,
                "trailing_stop_activation": 18.0
            }))
            .expect("json update should succeed");

        assert_eq!(config.crypto_fee_rate, 0.08);
        assert_eq!(config.eth_threshold_bps, 125);
        assert_eq!(config.trend_reversal_threshold, 12.0);
        assert_eq!(config.eth_trend_reversal_threshold, 0.6);
        assert_eq!(config.hold_max_crossings, 4);
        assert_eq!(config.spike_sustain_ms, 75);
        assert!(!config.trend_filter_enabled);
        assert_eq!(config.eth_trend_min_magnitude_usd, 1.8);
        assert_eq!(config.trend_max_magnitude_usd, 220.0);
        assert_eq!(config.eth_trend_max_magnitude_usd, 8.5);
        assert_eq!(config.ptb_neutral_zone_usd, 33.0);
        assert_eq!(config.eth_ptb_neutral_zone_usd, 1.2);
        assert_eq!(config.trailing_stop_activation, 18.0);
    }
}
