use std::time::Instant;

#[derive(Debug, Clone)]
pub struct MarketData {
    pub symbol: String, // "BTC", "ETH", "SOL"
    pub condition_id: String,
    pub question: String,
    pub slug: String,
    pub up_price: f64,
    pub down_price: f64,
    pub up_token_id: String,
    pub down_token_id: String,
    pub active: bool,
    pub fetched_at: Instant,
    /// Window start timestamp (unix seconds) from slug
    pub window_start_ts: u64,
    /// Window end timestamp (unix seconds) = window_start_ts + 300
    pub window_end_ts: u64,
}

/// Fetch the Chainlink BTC/USD price at a specific timestamp via on-chain round data
/// Returns None if RPC is unavailable or no round found near the timestamp
pub async fn fetch_chainlink_price_at(timestamp_secs: u64) -> Option<f64> {
    let rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://rpc.ankr.com/polygon".to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    // Get latest round to find the round ID range
    let latest_resp = client.post(&rpc)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": "0xc907E116054Ad103354f2D350FD2514433D57F6f", "data": "0xfeaf968c"}, "latest"],
            "id": 1
        }))
        .send().await.ok()?
        .json::<serde_json::Value>().await.ok()?;

    let result = latest_resp["result"].as_str()?;
    let data = hex::decode(&result[2..]).ok()?;
    if data.len() < 160 { return None; }

    let latest_round_id = u128::from_be_bytes(data[16..32].try_into().ok()?);
    let latest_updated_at = u64::from_be_bytes(data[96..104].try_into().ok()?);

    // Binary search backwards — Chainlink rounds are ~1s apart for BTC/USD data streams
    // Start from latest and walk back to find the round at window_start_ts
    let mut round_id = latest_round_id;
    let target = timestamp_secs;

    // Estimate how many rounds back we need (~1 round/sec, max search 600 rounds = 10 min)
    if latest_updated_at > target {
        let diff = (latest_updated_at - target).min(600) as u128;
        round_id = round_id.saturating_sub(diff);
    }

    // Fetch that round
    let round_hex = format!("{:064x}", round_id);
    let data_hex = format!("0x9a6fc8f5{}", round_hex); // getRoundData(uint80)

    let round_resp = client.post(&rpc)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{"to": "0xc907E116054Ad103354f2D350FD2514433D57F6f", "data": data_hex}, "latest"],
            "id": 2
        }))
        .send().await.ok()?
        .json::<serde_json::Value>().await.ok()?;

    let result = round_resp["result"].as_str()?;
    let data = hex::decode(&result[2..]).ok()?;
    if data.len() < 160 { return None; }

    let answer = i128::from_be_bytes(data[32..48].try_into().ok()?);
    let price = answer as f64 / 1e8;

    tracing::info!(timestamp=timestamp_secs, price=price, "Chainlink price at window start fetched");
    Some(price)
}

pub async fn fetch_current_market(symbol: &str) -> Option<MarketData> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let window_ts = now - (now % 300);
    let symbol_slug = symbol.to_lowercase();
    let slug = format!("{}-updown-5m-{}", symbol_slug, window_ts);

    let url = format!(
        "https://gamma-api.polymarket.com/markets?slug={}&active=true",
        slug
    );

    tracing::info!("Fetching market for {}: {}", symbol, url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        tracing::error!("API returned status: {}", resp.status());
        return None;
    }

    let body = resp.text().await.ok()?;
    let markets: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
    if markets.is_empty() {
        tracing::warn!("No markets found for slug: {}", slug);
        return None;
    }

    for market in markets {
        let market_slug = market["slug"].as_str().unwrap_or("");
        if market_slug.contains(&format!("{}-updown-5m", symbol_slug)) {
            let outcome_prices_str = market["outcomePrices"].as_str().unwrap_or("[]");
            let prices_raw: Vec<String> = serde_json::from_str(outcome_prices_str).ok()?;
            if prices_raw.len() < 2 {
                continue;
            }
            let up_price: f64 = prices_raw[0].parse().ok()?;
            let down_price: f64 = prices_raw[1].parse().ok()?;

            let token_ids_str = market["clobTokenIds"].as_str().unwrap_or("[]");
            let token_ids: Vec<String> = serde_json::from_str(token_ids_str).ok()?;
            if token_ids.len() < 2 {
                continue;
            }

            // Extract window_start from slug: {symbol}-updown-5m-{timestamp}
            let window_start_ts = market_slug
                .split('-')
                .last()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let window_end_ts = window_start_ts + 300; // 5 minutes

            return Some(MarketData {
                symbol: symbol.to_string(),
                condition_id: market["condition_id"].as_str().unwrap_or("").to_string(),
                question: market["question"].as_str().unwrap_or("").to_string(),
                slug: market_slug.to_string(),
                up_price,
                down_price,
                up_token_id: token_ids[0].clone(),
                down_token_id: token_ids[1].clone(),
                active: market["active"].as_bool().unwrap_or(false),
                fetched_at: Instant::now(),
                window_start_ts,
                window_end_ts,
            });
        }
    }

    None
}
