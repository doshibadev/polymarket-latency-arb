#[derive(Debug, Clone)]
pub struct MarketData {
    pub symbol: String, // "BTC", "ETH", "SOL"
    pub condition_id: String,
    pub question: String,
    pub up_price: f64,
    pub down_price: f64,
    pub up_token_id: String,
    pub down_token_id: String,
    /// Window end timestamp (unix seconds) for the active 5-minute market window
    pub window_end_ts: u64,
}

fn gamma_client() -> Option<&'static reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()
        })
        .as_ref()
}

pub async fn fetch_current_market(symbol: &str) -> Option<MarketData> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let window_ts = now - (now % 300);
    fetch_market_for_window(symbol, window_ts).await
}

pub async fn fetch_market_for_window(symbol: &str, window_ts: u64) -> Option<MarketData> {
    let symbol_slug = symbol.to_lowercase();
    let slug = format!("{}-updown-5m-{}", symbol_slug, window_ts);

    let url = format!(
        "https://gamma-api.polymarket.com/markets?slug={}&active=true",
        slug
    );

    tracing::info!("Fetching market for {}: {}", symbol, url);

    let client = gamma_client()?;
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
                .next_back()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let window_end_ts = window_start_ts + 300; // 5 minutes

            return Some(MarketData {
                symbol: symbol.to_string(),
                condition_id: market["condition_id"].as_str().unwrap_or("").to_string(),
                question: market["question"].as_str().unwrap_or("").to_string(),
                up_price,
                down_price,
                up_token_id: token_ids[0].clone(),
                down_token_id: token_ids[1].clone(),
                window_end_ts,
            });
        }
    }

    None
}
