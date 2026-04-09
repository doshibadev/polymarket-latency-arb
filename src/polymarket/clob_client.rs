use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info};
use crate::polymarket::{MarketData, fetch_current_market};

static LOGGED: AtomicBool = AtomicBool::new(false);

/// Message types from CLOB WebSocket
#[derive(Debug, Clone)]
pub struct SharePriceUpdate {
    pub symbol: String,
    pub direction: String, // "UP" or "DOWN"
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid_price: f64,
    pub timestamp: Instant,
}

/// CLOB WebSocket client for multiple concurrent markets
pub struct ClobClient {
    ws_url: String,
    symbols: Vec<String>,
    token_to_symbol: HashMap<String, (String, String)>, // token_id -> (symbol, direction)
    price_sender: mpsc::Sender<SharePriceUpdate>,
    market_sender: Option<mpsc::Sender<MarketData>>,
    market_ends: HashMap<String, u64>, // symbol -> window_end_ts
}

impl ClobClient {
    pub fn new(
        initial_markets: Vec<MarketData>,
        price_sender: mpsc::Sender<SharePriceUpdate>,
        market_sender: Option<mpsc::Sender<MarketData>>,
    ) -> Self {
        let mut token_to_symbol = HashMap::new();
        let mut market_ends = HashMap::new();
        let mut symbols = Vec::new();

        for m in initial_markets {
            token_to_symbol.insert(m.up_token_id.clone(), (m.symbol.clone(), "UP".to_string()));
            token_to_symbol.insert(m.down_token_id.clone(), (m.symbol.clone(), "DOWN".to_string()));
            market_ends.insert(m.symbol.clone(), m.window_end_ts);
            symbols.push(m.symbol);
        }

        Self {
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            symbols,
            token_to_symbol,
            price_sender,
            market_sender,
            market_ends,
        }
    }

    pub async fn run(mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("CLOB client starting for markets: {:?}", self.symbols);

        let mut backoff = Duration::from_millis(500);
        loop {
            match self.connect_and_stream().await {
                Ok(()) => {
                    info!("CLOB connection reset for market refresh, reconnecting...");
                    backoff = Duration::from_millis(100);
                }
                Err(e) => {
                    error!(error = %e, "CLOB WebSocket error, reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = std::cmp::min(backoff * 2, Duration::from_secs(30));
                }
            }
        }
    }

    async fn connect_and_stream(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut ws_stream, _) = connect_async(&self.ws_url).await?;
        info!("CLOB WebSocket connected");

        self.subscribe_all(&mut ws_stream).await?;
        LOGGED.store(false, Ordering::Relaxed);

        let (mut write, mut read) = ws_stream.split();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        let mut refresh_check = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                message = read.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_message(&text).await;
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = write.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Err(e)) => return Err(Box::new(e)),
                        _ => {}
                    }
                }
                _ = heartbeat.tick() => {
                    let _ = write.send(Message::Text("PING".into())).await;
                }
                _ = refresh_check.tick() => {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                    let mut needs_refresh = false;

                    for symbol in &self.symbols {
                        if let Some(&end_ts) = self.market_ends.get(symbol) {
                            if now >= end_ts.saturating_sub(1) {
                                info!(symbol = %symbol, "Market window ending, refreshing...");
                                if let Some(new_m) = fetch_current_market(symbol).await {
                                    if new_m.window_end_ts > end_ts {
                                        // Unsubscribe old tokens
                                        let old_tokens: Vec<String> = self.token_to_symbol.keys().cloned().collect();
                                        if !old_tokens.is_empty() {
                                            let unsub_msg = serde_json::json!({
                                                "assets_ids": old_tokens,
                                                "operation": "unsubscribe",
                                            });
                                            let _ = write.send(Message::Text(unsub_msg.to_string().into())).await;
                                        }

                                        // Remove old token mappings for this symbol
                                        self.token_to_symbol.retain(|_, (sym, _)| sym != symbol);

                                        // Insert new tokens
                                        self.token_to_symbol.insert(new_m.up_token_id.clone(), (new_m.symbol.clone(), "UP".to_string()));
                                        self.token_to_symbol.insert(new_m.down_token_id.clone(), (new_m.symbol.clone(), "DOWN".to_string()));
                                        self.market_ends.insert(symbol.clone(), new_m.window_end_ts);

                                        if let Some(market_tx) = &self.market_sender {
                                            let _ = market_tx.send(new_m).await;
                                        }
                                        needs_refresh = true;
                                    }
                                }
                            }
                        }
                    }

                    if needs_refresh {
                        return Ok(()); // Reconnect with new subscriptions
                    }
                }
            }
        }

        Ok(())
    }

    async fn subscribe_all<W>(&self, ws_stream: &mut W) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        W: SinkExt<Message> + Unpin,
        W::Error: std::error::Error + Send + Sync + 'static,
    {
        let asset_ids: Vec<String> = self.token_to_symbol.keys().cloned().collect();
        let sub_msg = serde_json::json!({
            "type": "subscribe",
            "assets_ids": asset_ids,
            "custom_feature_enabled": true
        });

        ws_stream.send(Message::Text(sub_msg.to_string().into())).await?;
        info!("CLOB subscribed to all active markets: {:?}", self.symbols);
        Ok(())
    }

    async fn handle_message(&self, text: &str) {
        if let Ok(arr) = serde_json::from_str::<Vec<Value>>(text) {
            for item in arr { self.process_item(&item).await; }
        } else if let Ok(obj) = serde_json::from_str::<Value>(text) {
            self.process_item(&obj).await;
        }
    }

    async fn process_item(&self, item: &Value) {
        let event_type = item.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
        if event_type == "best_bid_ask" || event_type == "price_change" {
            if let Some(asset_id) = item.get("asset_id").and_then(|v| v.as_str()) {
                if let Some((symbol, direction)) = self.token_to_symbol.get(asset_id) {
                    if let (Some(bid_s), Some(ask_s)) = (item.get("best_bid").and_then(|v| v.as_str()), item.get("best_ask").and_then(|v| v.as_str())) {
                        if let (Ok(bid), Ok(ask)) = (bid_s.parse::<f64>(), ask_s.parse::<f64>()) {
                            let _ = self.price_sender.send(SharePriceUpdate {
                                symbol: symbol.clone(),
                                direction: direction.clone(),
                                best_bid: bid,
                                best_ask: ask,
                                mid_price: (bid + ask) / 2.0,
                                timestamp: Instant::now(),
                            }).await;
                        }
                    }
                }
            }
        }
    }
}
