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
