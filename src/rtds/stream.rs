use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Deserialize)]
struct RtdsMessage {
    topic: String,
    payload: RtdsPayload,
}

#[derive(Debug, Clone, Deserialize)]
struct RtdsPayload {
    symbol: String,
    value: f64,
}

#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: String, // "BTC", "ETH", "SOL"
    pub price: f64,
    pub timestamp: Instant,
    pub source: String, // "binance" or "chainlink"
}

/// Direct Binance WebSocket for low-latency BTC price (~10ms updates vs RTDS relay ~50ms)
async fn run_binance_direct(tx: mpsc::Sender<PriceUpdate>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = "wss://stream.binance.com:9443/ws/btcusdt@aggTrade";
    loop {
        match connect_async(url).await {
            Ok((ws_stream, _)) => {
                info!("Direct Binance WebSocket connected");
                let (mut write, mut read) = ws_stream.split();
                let mut ping_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                        if let Some(price_str) = val.get("p").and_then(|v| v.as_str()) {
                                            if let Ok(price) = price_str.parse::<f64>() {
                                                let _ = tx.send(PriceUpdate {
                                                    symbol: "BTC".to_string(),
                                                    price,
                                                    timestamp: Instant::now(),
                                                    source: "binance".to_string(),
                                                }).await;
                                            }
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(payload))) => {
                                    let _ = write.send(Message::Pong(payload)).await;
                                }
                                Some(Ok(Message::Close(_))) | None => break,
                                Some(Err(e)) => { error!("Direct Binance error: {}", e); break; }
                                _ => {}
                            }
                        }
                        _ = ping_interval.tick() => {
                            if write.send(Message::Ping(vec![].into())).await.is_err() { break; }
                        }
                    }
                }
                error!("Direct Binance WebSocket disconnected, reconnecting in 1s...");
            }
            Err(e) => {
                error!("Direct Binance connect failed: {}, retrying in 3s", e);
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

pub struct RtdsStream {}

impl RtdsStream {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn run(
        &mut self,
        tx: mpsc::Sender<PriceUpdate>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = "wss://ws-live-data.polymarket.com";
        info!("Connecting to Polymarket RTDS: {}", url);

        // Spawn direct Binance stream once — it manages its own reconnects
        let tx_binance = tx.clone();
        tokio::spawn(async move {
            let _ = run_binance_direct(tx_binance).await;
        });

        loop {
            if let Err(e) = self.connect_and_stream(&url, &tx).await {
                error!(error = %e, "RTDS WebSocket error, reconnecting in 3s");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
    }

    async fn connect_and_stream(
        &mut self,
        url: &str,
        tx: &mpsc::Sender<PriceUpdate>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut ws_stream, _) = connect_async(url).await?;

        // Binance subscriptions
        let binance_sub = serde_json::json!({
            "action": "subscribe",
            "subscriptions": [
                { "topic": "crypto_prices", "type": "update", "filters": "{\"symbol\":\"btcusdt\"}" }
            ]
        });
        ws_stream.send(Message::Text(binance_sub.to_string().into())).await?;

        // Chainlink subscriptions
        let chainlink_sub = serde_json::json!({
            "action": "subscribe",
            "subscriptions": [
                { "topic": "crypto_prices_chainlink", "type": "*", "filters": "{\"symbol\":\"btc/usd\"}" }
            ]
        });
        ws_stream.send(Message::Text(chainlink_sub.to_string().into())).await?;

        info!("RTDS connected, subscribed to BTC (Binance & Chainlink)");
        let (mut write, mut read) = ws_stream.split();
        let mut heartbeat = tokio::time::interval(tokio::time::Duration::from_secs(5));

        loop {
            tokio::select! {
                message = read.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(msg) = serde_json::from_str::<RtdsMessage>(&text) {
                                let symbol = match msg.payload.symbol.to_lowercase().as_str() {
                                    "btcusdt" | "btc/usd" => "BTC",
                                    _ => continue,
                                };

                                let source = if msg.topic == "crypto_prices" { "binance" } else { "chainlink" };

                                let _ = tx.send(PriceUpdate {
                                    symbol: symbol.to_string(),
                                    price: msg.payload.value,
                                    timestamp: Instant::now(),
                                    source: source.to_string(),
                                }).await;
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Err(e)) => return Err(Box::new(e)),
                        _ => {}
                    }
                }
                _ = heartbeat.tick() => {
                    let _ = write.send(Message::Text("PING".into())).await;
                }
            }
        }

        Ok(())
    }
}
