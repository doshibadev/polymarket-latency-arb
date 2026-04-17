use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::config::AppConfig;
use crate::execution::model::OpenPosition;
use crate::execution::planner::PtbTier;

pub struct PositionExitContext {
    pub current_price: f64,
    pub current_btc: f64,
    pub last_price_to_beat: Option<f64>,
    pub last_market_end_ts: Option<u64>,
}

pub struct HoldEvaluationInput<'a> {
    pub pos: &'a OpenPosition,
    pub btc_history: &'a VecDeque<(f64, Instant)>,
    pub current_btc: f64,
    pub price_to_beat: f64,
    pub time_remaining: u64,
    pub hold_margin_per_second: f64,
    pub hold_max_seconds: u64,
    pub hold_max_crossings: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedExit {
    pub position_id: String,
    pub symbol: String,
    pub direction: String,
    pub price: f64,
    pub reason: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionExitDecision {
    pub reason: Option<&'static str>,
    pub suppressed_reason: Option<&'static str>,
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn ptb_margin(direction: &str, current_btc: f64, price_to_beat: Option<f64>) -> Option<f64> {
    price_to_beat.map(|ptb| {
        if direction == "UP" {
            current_btc - ptb
        } else {
            ptb - current_btc
        }
    })
}

fn ptb_factor(margin: Option<f64>) -> f64 {
    let margin = margin.unwrap_or(0.0);
    if margin > 50.0 {
        1.5
    } else if margin > 30.0 {
        1.0 + (margin - 30.0) / 20.0 * 0.5
    } else if margin < -50.0 {
        0.6
    } else if margin < -30.0 {
        1.0 - ((-margin) - 30.0) / 20.0 * 0.4
    } else {
        1.0
    }
}

fn trend_reversed_hit(
    pos: &OpenPosition,
    current_btc: f64,
    config: &AppConfig,
    factor: f64,
) -> bool {
    if pos.entry_btc <= 0.0 || current_btc <= 0.0 {
        return false;
    }

    let reversal_threshold = (pos.entry_spike.abs() * (config.trend_reversal_pct / 100.0))
        .max(config.trend_reversal_threshold)
        * factor;
    if pos.direction == "UP" {
        current_btc < (pos.entry_btc - reversal_threshold)
    } else {
        current_btc > (pos.entry_btc + reversal_threshold)
    }
}

fn spike_faded_hit(pos: &OpenPosition, current_btc: f64, config: &AppConfig, factor: f64) -> bool {
    if pos.entry_btc <= 0.0 || current_btc <= 0.0 || pos.entry_spike.abs() <= 0.0 {
        return false;
    }

    let favorable_move = if pos.direction == "UP" {
        pos.peak_btc - pos.entry_btc
    } else {
        pos.entry_btc - pos.trough_btc
    };
    let reference_move = favorable_move.max(pos.entry_spike.abs());
    let threshold_dollars = reference_move * (config.spike_faded_pct / 100.0) * factor;
    if pos.direction == "UP" {
        let peak = pos.peak_btc;
        peak > 0.0 && peak - current_btc >= threshold_dollars
    } else {
        let trough = pos.trough_btc;
        trough > 0.0 && current_btc - trough >= threshold_dollars
    }
}

pub fn evaluate_position_exit(
    pos: &OpenPosition,
    ctx: &PositionExitContext,
    config: &AppConfig,
    now_secs: u64,
) -> PositionExitDecision {
    if ctx.current_price >= 0.995 {
        return PositionExitDecision {
            reason: Some("max_price"),
            suppressed_reason: None,
        };
    }

    let held_ms = pos.entry_time.elapsed().as_millis() as u64;
    let min_hold_passed = held_ms >= config.min_hold_ms;
    let time_remaining = ctx
        .last_market_end_ts
        .and_then(|end| (end > now_secs).then_some(end - now_secs));
    let margin = ptb_margin(&pos.direction, ctx.current_btc, ctx.last_price_to_beat);
    let factor = ptb_factor(margin);

    let trend_reversed_raw = trend_reversed_hit(pos, ctx.current_btc, config, factor);
    let trend_confirmation_ms =
        if ctx.current_price < pos.entry_price && margin.is_some_and(|m| m > 30.0) {
            700
        } else {
            200
        };
    let trend_reversed_confirmed = trend_reversed_raw
        && pos
            .trend_reversed_since
            .is_some_and(|t| t.elapsed().as_millis() >= trend_confirmation_ms);
    let trend_reversed = trend_reversed_confirmed && min_hold_passed;

    let spike_faded_raw = spike_faded_hit(pos, ctx.current_btc, config, factor);
    let spike_faded_confirmed = spike_faded_raw
        && pos
            .spike_faded_since
            .is_some_and(|t| t.elapsed().as_millis() >= config.spike_faded_ms as u128);

    let stop_loss_hit = config.stop_loss_pct > 0.0
        && min_hold_passed
        && ctx.current_price <= pos.entry_price * (1.0 - config.stop_loss_pct / 100.0);
    let trailing_stop_hit = pos.trailing_stop_activated
        && config.trailing_stop_pct > 0.0
        && min_hold_passed
        && ctx.current_price <= pos.highest_price * (1.0 - config.trailing_stop_pct / 100.0);
    let near_end = held_ms > 295_000;

    let ptb_tier = PtbTier::from_margin(margin);
    let ptb_hold_active = ptb_tier.is_ptb_hold()
        || pos.entry_mode.as_deref() == Some("ptb_hold")
        || (pos.hold_to_resolution && margin.is_some_and(|m| m > config.hold_safety_margin));

    if ptb_hold_active {
        let margin = margin.unwrap_or(0.0);
        if margin <= config.hold_safety_margin {
            return PositionExitDecision {
                reason: Some("hold_safety_exit"),
                suppressed_reason: None,
            };
        }
        if time_remaining.is_some_and(|t| t <= 2) {
            return PositionExitDecision {
                reason: Some("market_end"),
                suppressed_reason: None,
            };
        }
        if trend_reversed && margin < 30.0 {
            return PositionExitDecision {
                reason: Some("trend_reversed"),
                suppressed_reason: None,
            };
        }

        let suppressed_reason = if trend_reversed {
            Some("trend_reversed")
        } else if trailing_stop_hit {
            Some("trailing_stop")
        } else if stop_loss_hit {
            Some("stop_loss")
        } else if spike_faded_confirmed {
            Some("spike_faded")
        } else {
            None
        };
        return PositionExitDecision {
            reason: None,
            suppressed_reason,
        };
    }

    if trend_reversed && ctx.current_price < pos.entry_price && margin.is_some_and(|m| m > 30.0) {
        return PositionExitDecision {
            reason: None,
            suppressed_reason: Some("trend_reversed"),
        };
    }

    let reason = if trend_reversed {
        Some("trend_reversed")
    } else if trailing_stop_hit {
        Some("trailing_stop")
    } else if stop_loss_hit {
        Some("stop_loss")
    } else if min_hold_passed && spike_faded_confirmed {
        Some("spike_faded")
    } else if min_hold_passed && near_end {
        Some("near_end")
    } else {
        None
    };

    if pos.hold_to_resolution
        && reason.is_some()
        && !matches!(reason, Some("trend_reversed" | "max_price"))
    {
        return PositionExitDecision {
            reason: None,
            suppressed_reason: None,
        };
    }

    PositionExitDecision {
        reason,
        suppressed_reason: None,
    }
}

pub fn update_exit_persistence(pos: &mut OpenPosition, current_btc: f64, config: &AppConfig) {
    if spike_faded_hit(pos, current_btc, config, 1.0) {
        if pos.spike_faded_since.is_none() {
            pos.spike_faded_since = Some(Instant::now());
        }
    } else {
        pos.spike_faded_since = None;
    }

    if trend_reversed_hit(pos, current_btc, config, 1.0) {
        if pos.trend_reversed_since.is_none() {
            pos.trend_reversed_since = Some(Instant::now());
        }
    } else {
        pos.trend_reversed_since = None;
    }
}

pub fn evaluate_hold_to_resolution(input: HoldEvaluationInput<'_>) -> Option<bool> {
    let margin = if input.pos.direction == "UP" {
        input.current_btc - input.price_to_beat
    } else {
        input.price_to_beat - input.current_btc
    };

    if margin <= 0.0 {
        return Some(false);
    }
    if input.time_remaining > input.hold_max_seconds {
        return Some(false);
    }

    let mut crossings = 0usize;
    let mut iter = input.btc_history.iter().map(|(price, _)| *price);
    if let Some(mut previous) = iter.next() {
        for current in iter {
            let was_above = previous > input.price_to_beat;
            let is_above = current > input.price_to_beat;
            if was_above != is_above {
                crossings += 1;
            }
            previous = current;
        }
    }

    if crossings > input.hold_max_crossings {
        return Some(false);
    }

    let trend_ok = if input.btc_history.len() >= 10 {
        let recent: Vec<f64> = input
            .btc_history
            .iter()
            .rev()
            .take(10)
            .map(|(price, _)| *price)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let all_correct_side = if input.pos.direction == "UP" {
            recent.iter().all(|p| *p > input.price_to_beat)
        } else {
            recent.iter().all(|p| *p < input.price_to_beat)
        };
        let slope = recent.last().unwrap() - recent.first().unwrap();
        let moving_away = if input.pos.direction == "UP" {
            slope > 0.0
        } else {
            slope < 0.0
        };
        all_correct_side && moving_away
    } else {
        false
    };

    let required_margin = input.hold_margin_per_second * input.time_remaining as f64;
    Some(margin >= required_margin && trend_ok && crossings == 0)
}

pub fn exit_mode_for(
    reason: &str,
    ptb_margin_at_exit: Option<f64>,
    remaining_secs: Option<u64>,
    config: &AppConfig,
) -> &'static str {
    if matches!(reason, "market_end" | "near_end" | "manual") {
        return "forced";
    }

    let required_exit_margin = remaining_secs
        .map(|t| {
            config
                .hold_safety_margin
                .max(config.hold_margin_per_second * t as f64)
        })
        .unwrap_or(config.hold_safety_margin);

    if ptb_margin_at_exit.is_some_and(|m| m >= 100.0 || m >= required_exit_margin) {
        "ptb_conviction"
    } else {
        "scalp"
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::config::AppConfig;

    use super::*;

    fn position(direction: &str) -> OpenPosition {
        OpenPosition {
            position_id: format!("test-{direction}"),
            symbol: "BTC".to_string(),
            direction: direction.to_string(),
            entry_price: 0.5,
            avg_entry_price: 0.5,
            shares: 10.0,
            position_size: 5.0,
            buy_fee: 0.0,
            entry_spike: 20.0,
            entry_time: Instant::now() - Duration::from_secs(10),
            highest_price: 0.8,
            scale_level: 1,
            hold_to_resolution: false,
            peak_spike: 20.0,
            entry_btc: 100_000.0,
            peak_btc: 100_050.0,
            trough_btc: 99_950.0,
            spike_faded_since: None,
            trend_reversed_since: Some(Instant::now() - Duration::from_millis(250)),
            trailing_stop_activated: false,
            on_chain_shares: None,
            ptb_tier_at_entry: None,
            entry_mode: None,
            exit_suppressed_count: 0,
            last_suppressed_exit_signal: None,
        }
    }

    #[test]
    fn exit_decision_is_directional_for_trend_reversal() {
        let mut config = AppConfig::load().expect("config");
        config.min_hold_ms = 0;
        config.trend_reversal_pct = 50.0;
        config.trend_reversal_threshold = 10.0;

        let up = position("UP");
        let up_decision = evaluate_position_exit(
            &up,
            &PositionExitContext {
                current_price: 0.4,
                current_btc: 99_980.0,
                last_price_to_beat: None,
                last_market_end_ts: None,
            },
            &config,
            now_unix_secs(),
        );
        assert_eq!(up_decision.reason, Some("trend_reversed"));

        let down = position("DOWN");
        let down_decision = evaluate_position_exit(
            &down,
            &PositionExitContext {
                current_price: 0.4,
                current_btc: 100_020.0,
                last_price_to_beat: None,
                last_market_end_ts: None,
            },
            &config,
            now_unix_secs(),
        );
        assert_eq!(down_decision.reason, Some("trend_reversed"));
    }
}
