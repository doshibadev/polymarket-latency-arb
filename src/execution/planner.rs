use std::time::Instant;

use crate::config::AppConfig;
use crate::execution::model::PendingEntry;
use crate::polymarket::BookLevel;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PtbTier {
    Unknown,
    Unfavorable,
    Neutral,
    Favorable,
    Strong,
    Extreme,
}

impl PtbTier {
    pub fn from_margin(margin: Option<f64>) -> Self {
        match margin {
            Some(m) if m >= 150.0 => Self::Extreme,
            Some(m) if m >= 100.0 => Self::Strong,
            Some(m) if m >= 30.0 => Self::Favorable,
            Some(m) if m >= -20.0 => Self::Neutral,
            Some(_) => Self::Unfavorable,
            None => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unfavorable => "unfavorable",
            Self::Neutral => "neutral",
            Self::Favorable => "favorable",
            Self::Strong => "strong",
            Self::Extreme => "extreme",
        }
    }

    fn is_favorable(self) -> bool {
        matches!(self, Self::Favorable | Self::Strong | Self::Extreme)
    }

    pub fn is_ptb_hold(self) -> bool {
        matches!(self, Self::Strong | Self::Extreme)
    }
}

pub struct EntryPlanInput<'a> {
    pub symbol: &'a str,
    pub direction: &'a str,
    pub spike: f64,
    pub threshold_usd: f64,
    pub allow_scaling: bool,
    pub current_btc: f64,
    pub balance: f64,
    pub portfolio_pct: f64,
    pub config: &'a AppConfig,
    pub existing_same_direction: usize,
    pub pending_same_direction: usize,
    pub bid: f64,
    pub ask: f64,
    pub bids: &'a [BookLevel],
    pub asks: &'a [BookLevel],
    pub price_to_beat: Option<f64>,
    pub market_end_ts: Option<u64>,
    pub submitted_at: Instant,
}

struct EntryContext {
    score: f64,
    round_trip_loss_pct: f64,
    spread: f64,
    ptb_margin: Option<f64>,
    seconds_to_expiry: Option<u64>,
    ptb_tier: PtbTier,
    entry_mode: &'static str,
}

pub fn calculate_fee(shares: f64, price: f64, crypto_fee_rate: f64) -> f64 {
    let fee = shares * crypto_fee_rate * price * (1.0 - price);
    if fee < 0.00001 {
        0.0
    } else {
        (fee * 100000.0).round() / 100000.0
    }
}

pub fn estimate_fak_buy(
    asks: &[BookLevel],
    usdc_budget: f64,
    limit_price: f64,
) -> Option<(f64, f64, f64)> {
    if asks.is_empty() {
        return None;
    }
    let mut remaining = usdc_budget;
    let mut shares = 0.0;
    let mut spent = 0.0;
    for level in asks {
        if remaining <= 0.0 || level.price > limit_price {
            break;
        }
        let spend = remaining.min(level.price * level.size);
        if spend <= 0.0 {
            continue;
        }
        spent += spend;
        shares += spend / level.price;
        remaining -= spend;
    }
    if shares > 0.0 && spent > 0.0 {
        Some((spent / shares, shares, spent))
    } else {
        None
    }
}

pub fn estimate_fak_sell(
    bids: &[BookLevel],
    shares_to_sell: f64,
    limit_price: f64,
) -> Option<(f64, f64, f64)> {
    if bids.is_empty() {
        return None;
    }
    let mut remaining = shares_to_sell;
    let mut sold = 0.0;
    let mut gross = 0.0;
    for level in bids {
        if remaining <= 0.0 || level.price < limit_price {
            break;
        }
        let fill = remaining.min(level.size);
        if fill <= 0.0 {
            continue;
        }
        sold += fill;
        gross += fill * level.price;
        remaining -= fill;
    }
    if sold > 0.0 && gross > 0.0 {
        Some((gross / sold, sold, gross))
    } else {
        None
    }
}

pub fn build_entry_plan(input: EntryPlanInput<'_>) -> Result<PendingEntry, String> {
    let scale_level = (input.existing_same_direction + input.pending_same_direction) as u32 + 1;

    if !input.allow_scaling && scale_level > 1 {
        return Err("MAX_SCALE_LEVEL".to_string());
    }

    let entry_price = if input.bid > 0.0 && input.ask > 0.0 {
        (input.bid + input.ask) / 2.0
    } else {
        0.0
    };
    if entry_price <= 0.0 {
        return Err("NO_PRICE_DATA".to_string());
    }
    if entry_price > input.config.max_entry_price {
        return Err("PRICE_TOO_HIGH".to_string());
    }
    if entry_price < input.config.min_entry_price {
        return Err("PRICE_TOO_LOW".to_string());
    }

    let portfolio_pct = if entry_price < 0.20 {
        0.10
    } else if entry_price < 0.50 {
        0.15
    } else if entry_price < 0.75 {
        0.20
    } else {
        input.portfolio_pct
    };
    let position_size = (input.balance * portfolio_pct * (1.0 / scale_level as f64)).min(20.0);
    let shares = position_size / entry_price;
    let buy_fee = calculate_fee(shares, entry_price, input.config.crypto_fee_rate);

    if (position_size + buy_fee) > input.balance {
        return Err("INSUFFICIENT_BALANCE".to_string());
    }
    if position_size < 1.0 {
        return Err("BELOW_MIN_ORDER_SIZE".to_string());
    }

    let entry_context = entry_context_score(&input, position_size)?;

    Ok(PendingEntry {
        symbol: input.symbol.to_string(),
        direction: input.direction.to_string(),
        spike: input.spike,
        entry_price,
        scale_level,
        position_size,
        shares,
        buy_fee,
        submitted_at: input.submitted_at,
        entry_btc: input.current_btc,
        live_synced: false,
        price_to_beat_at_entry: input.price_to_beat,
        ptb_margin_at_entry: entry_context.ptb_margin,
        seconds_to_expiry_at_entry: entry_context.seconds_to_expiry,
        spread_at_entry: Some(entry_context.spread),
        round_trip_loss_pct_at_entry: Some(entry_context.round_trip_loss_pct),
        signal_score: Some(entry_context.score),
        ptb_tier_at_entry: Some(entry_context.ptb_tier.as_str().to_string()),
        entry_mode: Some(entry_context.entry_mode.to_string()),
    })
}

fn entry_context_score(
    input: &EntryPlanInput<'_>,
    position_size: f64,
) -> Result<EntryContext, String> {
    if input.bid <= 0.0 || input.ask <= 0.0 {
        return Err("NO_POL_LIQUIDITY".to_string());
    }
    let buy_limit = (input.ask + 0.04).min(0.99);
    let (buy_price, buy_shares, spent_usdc) =
        estimate_fak_buy(input.asks, position_size, buy_limit)
            .ok_or_else(|| "ENTRY_NO_FAK_LIQUIDITY".to_string())?;
    let sell_limit = if input.bid > 0.04 {
        (input.bid - 0.04).max(0.01)
    } else {
        0.01
    };
    let (sell_price, sell_shares, gross_revenue) =
        estimate_fak_sell(input.bids, buy_shares, sell_limit)
            .ok_or_else(|| "EXIT_NO_FAK_LIQUIDITY".to_string())?;
    let coverage = (sell_shares / buy_shares).clamp(0.0, 1.0);
    let buy_fee = calculate_fee(buy_shares, buy_price, input.config.crypto_fee_rate);
    let sell_fee = calculate_fee(sell_shares, sell_price, input.config.crypto_fee_rate);
    let round_trip_loss =
        ((spent_usdc + buy_fee) - (gross_revenue - sell_fee)) / (spent_usdc + buy_fee).max(0.01);
    if coverage < 0.80 {
        return Err(format!("EXIT_DEPTH_TOO_THIN({:.0}%)", coverage * 100.0));
    }
    if round_trip_loss > 0.16 {
        return Err(format!(
            "ROUND_TRIP_TOO_EXPENSIVE({:.1}%)",
            round_trip_loss * 100.0
        ));
    }

    let ptb_margin = input.price_to_beat.map(|ptb| {
        if input.direction == "UP" {
            input.current_btc - ptb
        } else {
            ptb - input.current_btc
        }
    });
    let seconds_to_expiry = input.market_end_ts.and_then(|end| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        (end > now).then_some(end - now)
    });
    let ptb_tier = PtbTier::from_margin(ptb_margin);
    let spike_multiple = input.spike.abs() / input.threshold_usd.max(0.01);

    if seconds_to_expiry.is_some_and(|s| s < 45)
        && !(ptb_tier == PtbTier::Extreme && buy_price >= 0.70)
    {
        return Err(format!(
            "ENTRY_REJECT_TIMING(late,secs={},tier={})",
            seconds_to_expiry.unwrap_or(0),
            ptb_tier.as_str()
        ));
    }
    if seconds_to_expiry.is_some_and(|s| s >= 240)
        && (!ptb_tier.is_favorable() || spike_multiple < 1.35)
    {
        return Err(format!(
            "ENTRY_REJECT_TIMING(early,secs={},tier={},spike={:.2}x)",
            seconds_to_expiry.unwrap_or(0),
            ptb_tier.as_str(),
            spike_multiple
        ));
    }
    if ptb_tier == PtbTier::Unfavorable && spike_multiple < 1.80 {
        return Err(format!(
            "ENTRY_REJECT_PTB_TIER({},spike={:.2}x)",
            ptb_tier.as_str(),
            spike_multiple
        ));
    }

    let ptb_score = ptb_margin
        .map(|m| {
            if m >= 20.0 {
                0.9
            } else if m >= -20.0 {
                0.35
            } else {
                (-0.9f64).max(m / 120.0)
            }
        })
        .unwrap_or(0.0);
    let expiry_score = seconds_to_expiry
        .map(|s| {
            if s < 20 {
                -0.8
            } else if s <= 90 {
                0.25
            } else {
                0.0
            }
        })
        .unwrap_or(0.0);
    let spread = input.ask - input.bid;
    let spread_penalty = (spread / 0.08).clamp(0.0, 1.0) * 0.7;
    let spike_score = (input.spike.abs() / input.threshold_usd.max(0.01)).min(3.0);
    let liquidity_score = coverage - (round_trip_loss * 2.5);
    let score = spike_score + ptb_score + expiry_score + liquidity_score - spread_penalty;
    let entry_mode = if ptb_tier.is_ptb_hold() {
        "ptb_hold"
    } else if ptb_tier == PtbTier::Favorable {
        "breakout"
    } else {
        "scalp"
    };
    Ok(EntryContext {
        score,
        round_trip_loss_pct: round_trip_loss,
        spread,
        ptb_margin,
        seconds_to_expiry,
        ptb_tier,
        entry_mode,
    })
}
