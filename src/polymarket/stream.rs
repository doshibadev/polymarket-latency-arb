use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Deserialize)]
struct PolymarketTrade {
    #[allow(dead_code)]
    asset_id: String,
    price: String,
    #[allow(dead_code)]
    size: String,
    side: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PolymarketMessage {
    #[allow(dead_code)]
    trades: Option<Vec<PolymarketTrade>>,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    msg_type: Option<String>,
    #[allow(dead_code)]
    data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct GammaMarket {
    condition_id: String,
    #[serde(rename = "clob_token_ids")]
    token_ids: Vec<String>,
    question: String,
    slug: String,
}

pub struct PolymarketStream {}

impl PolymarketStream {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn run(&mut self, tx: mpsc::Sender<(f64, Instant)>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
        info!("Connecting to Polymarket CLOB WebSocket: {}", url);

        loop {
            if let Err(e) = self.connect_and_stream(&url, &tx).await {
                error!(error = %e, "Polymarket WebSocket error, reconnecting in 3s");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
    }

    async fn connect_and_stream(&mut self, url: &str, tx: &mpsc::Sender<(f64, Instant)>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut ws_stream, _) = connect_async(url).await?;

        // Subscribe to market channel with custom features enabled
        let subscribe_msg = serde_json::json!({
            "type": "market",
            "custom_feature_enabled": true
        });
        ws_stream.send(Message::Text(subscribe_msg.to_string().into())).await?;

        info!("Polymarket WebSocket connected, subscribed to market channel");

        let (mut write, mut read) = ws_stream.split();

        // Heartbeat every 10 seconds as required by Polymarket
        let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(10));

        // Market refresh every 30 seconds to check for new BTC 5m markets
        let mut market_refresh = tokio::time::interval(tokio::time::Duration::from_secs(30));
        market_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut subscribed_assets: Vec<String> = Vec::new();

        // Do initial market discovery immediately
        info!("Performing initial BTC market discovery...");
        if let Ok(new_assets) = self.discover_btc_markets().await {
            if !new_assets.is_empty() {
                info!(?new_assets, "Initial market discovery successful, subscribing");
                let sub_msg = serde_json::json!({
                    "assets_ids": new_assets,
                    "operation": "subscribe",
                    "custom_feature_enabled": true
                });
                if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
                    error!(error = %e, "Failed to subscribe to initial assets");
                } else {
                    subscribed_assets.extend(new_assets);
                }
            } else {
                warn!("No BTC 5m markets found in initial discovery");
            }
        }

        loop {
            tokio::select! {
                message = read.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(msg) = serde_json::from_str::<PolymarketMessage>(&text) {
                                if let Some(trades) = msg.trades {
                                    for trade in trades {
                                        if let Ok(price) = trade.price.parse::<f64>() {
                                            let now = Instant::now();
                                            debug!(price, side = %trade.side, asset_id = %trade.asset_id, "Polymarket trade");

                                            if tx.send((price, now)).await.is_err() {
                                                warn!("Receiver dropped, stopping Polymarket stream");
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            debug!("Polymarket pong received");
                        }
                        Some(Ok(Message::Close(frame))) => {
                            info!(reason = ?frame, "Polymarket WebSocket closed");
                            break;
                        }
                        Some(Err(e)) => {
                            error!(error = %e, "Polymarket WebSocket read error");
                            return Err(Box::new(e));
                        }
                        _ => {}
                    }
                }
                _ = heartbeat_interval.tick() => {
                    // Send PING as plain text (not JSON) per Polymarket docs
                    let _ = write.send(Message::Text("PING".into())).await;
                    debug!("Sent PING heartbeat to Polymarket");
                }
                _ = market_refresh.tick() => {
                    // Discover and subscribe to current BTC 5m markets
                    if let Ok(new_assets) = self.discover_btc_markets().await {
                        let assets_to_subscribe: Vec<String> = new_assets.iter()
                            .filter(|a| !subscribed_assets.contains(a))
                            .cloned()
                            .collect();

                        if !assets_to_subscribe.is_empty() {
                            info!(?assets_to_subscribe, "Subscribing to new BTC market assets");
                            let sub_msg = serde_json::json!({
                                "assets_ids": assets_to_subscribe,
                                "operation": "subscribe",
                                "custom_feature_enabled": true
                            });
                            if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
                                error!(error = %e, "Failed to subscribe to new assets");
                            } else {
                                subscribed_assets.extend(assets_to_subscribe);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn discover_btc_markets(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let window_ts = now - (now % 300);
        let slug = format!("btc-updown-5m-{}", window_ts);

        let url = format!(
            "https://gamma-api.polymarket.com/events?slug={}",
            slug
        );

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            debug!(status = %resp.status(), "Gamma API returned non-success");
            return Ok(Vec::new());
        }

        let events: Vec<serde_json::Value> = resp.json().await?;
        if events.is_empty() {
            debug!(slug, "No BTC 5m market found for current window");
            return Ok(Vec::new());
        }

        let mut asset_ids = Vec::new();
        for event in events {
            if let Some(markets) = event["markets"].as_array() {
                for market in markets {
                    // Try both camelCase and snake_case
                    let token_ids = market["clobTokenIds"]
                        .as_array()
                        .or_else(|| market["clob_token_ids"].as_array());
                    
                    if let Some(ids) = token_ids {
                        for token_id in ids {
                            if let Some(id) = token_id.as_str() {
                                asset_ids.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }

        if !asset_ids.is_empty() {
            info!(count = asset_ids.len(), ?asset_ids, "Discovered BTC 5m market assets");
        }

        Ok(asset_ids)
    }
}
