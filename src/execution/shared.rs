use crate::execution::model::OpenPosition;

pub fn update_directional_bid_ask(
    up_bid: &mut f64,
    up_ask: &mut f64,
    down_bid: &mut f64,
    down_ask: &mut f64,
    direction: &str,
    bid: f64,
    ask: f64,
) {
    if direction == "UP" {
        *up_bid = bid;
        *up_ask = ask;
    } else {
        *down_bid = bid;
        *down_ask = ask;
    }
}

pub fn directional_bid_ask(
    up_bid: f64,
    up_ask: f64,
    down_bid: f64,
    down_ask: f64,
    direction: &str,
) -> (f64, f64) {
    if direction == "UP" {
        (up_bid, up_ask)
    } else {
        (down_bid, down_ask)
    }
}

pub fn share_mid_price(bid: f64, ask: f64) -> f64 {
    if bid > 0.0 && ask > 0.0 {
        (bid + ask) / 2.0
    } else {
        0.0
    }
}

pub fn sync_position_price_state(
    open_positions: &mut [OpenPosition],
    symbol: &str,
    direction: &str,
    mid_price: f64,
    trailing_stop_activation_pct: f64,
) {
    if mid_price <= 0.0 {
        return;
    }

    for pos in open_positions {
        if pos.symbol != symbol || pos.direction != direction {
            continue;
        }

        if mid_price > pos.highest_price {
            pos.highest_price = mid_price;
        }

        if !pos.trailing_stop_activated && trailing_stop_activation_pct > 0.0 {
            let threshold = pos.entry_price * (1.0 + trailing_stop_activation_pct / 100.0);
            if pos.highest_price >= threshold {
                pos.trailing_stop_activated = true;
            }
        }
    }
}

pub fn update_btc_extrema(open_positions: &mut [OpenPosition], symbol: &str, current_btc: f64) {
    for pos in open_positions {
        if pos.symbol != symbol {
            continue;
        }

        if current_btc > pos.peak_btc {
            pos.peak_btc = current_btc;
        }
        if current_btc < pos.trough_btc {
            pos.trough_btc = current_btc;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::execution::model::OpenPosition;

    use super::{
        directional_bid_ask, share_mid_price, sync_position_price_state, update_btc_extrema,
        update_directional_bid_ask,
    };

    fn position(symbol: &str, direction: &str) -> OpenPosition {
        OpenPosition {
            position_id: format!("{symbol}-{direction}-test"),
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price: 0.50,
            avg_entry_price: 0.50,
            shares: 10.0,
            position_size: 5.0,
            buy_fee: 0.0,
            entry_spike: 20.0,
            entry_time: Instant::now(),
            highest_price: 0.50,
            scale_level: 1,
            hold_to_resolution: false,
            peak_spike: 20.0,
            entry_btc: 100000.0,
            peak_btc: 100000.0,
            trough_btc: 100000.0,
            spike_faded_since: None,
            trend_reversed_since: None,
            trailing_stop_activated: false,
            on_chain_shares: None,
            ptb_tier_at_entry: None,
            entry_mode: Some("scalp".to_string()),
            exit_suppressed_count: 0,
            last_suppressed_exit_signal: None,
        }
    }

    #[test]
    fn quote_helpers_update_and_read_consistently() {
        let (mut up_bid, mut up_ask, mut down_bid, mut down_ask) = (0.0, 0.0, 0.0, 0.0);
        update_directional_bid_ask(
            &mut up_bid,
            &mut up_ask,
            &mut down_bid,
            &mut down_ask,
            "UP",
            0.44,
            0.46,
        );
        update_directional_bid_ask(
            &mut up_bid,
            &mut up_ask,
            &mut down_bid,
            &mut down_ask,
            "DOWN",
            0.54,
            0.56,
        );

        assert_eq!(
            directional_bid_ask(up_bid, up_ask, down_bid, down_ask, "UP"),
            (0.44, 0.46)
        );
        assert_eq!(
            directional_bid_ask(up_bid, up_ask, down_bid, down_ask, "DOWN"),
            (0.54, 0.56)
        );
        assert_eq!(share_mid_price(0.44, 0.46), 0.45);
        assert_eq!(share_mid_price(0.0, 0.46), 0.0);
    }

    #[test]
    fn position_helpers_track_trailing_and_btc_extrema() {
        let mut positions = vec![position("BTC-1", "UP"), position("BTC-1", "DOWN")];

        sync_position_price_state(&mut positions, "BTC-1", "UP", 0.62, 20.0);
        assert_eq!(positions[0].highest_price, 0.62);
        assert!(positions[0].trailing_stop_activated);
        assert_eq!(positions[1].highest_price, 0.50);
        assert!(!positions[1].trailing_stop_activated);

        update_btc_extrema(&mut positions, "BTC-1", 100050.0);
        update_btc_extrema(&mut positions, "BTC-1", 99920.0);
        assert_eq!(positions[0].peak_btc, 100050.0);
        assert_eq!(positions[0].trough_btc, 99920.0);
        assert_eq!(positions[1].peak_btc, 100050.0);
        assert_eq!(positions[1].trough_btc, 99920.0);
    }
}
