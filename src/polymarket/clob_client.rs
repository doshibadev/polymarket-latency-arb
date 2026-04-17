use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info};
use crate::polymarket::{MarketData, fetch_current_market};

static LOGGED: AtomicBool = AtomicBool::new(false);

/// Message types from CLOB WebSocket
#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct SharePriceUpdate {
    pub symbol: String,
    pub direction: String, // "UP" or "DOWN"
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid_price: f64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Default)]
struct OrderBookCache {
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
}

/// CLOB WebSocket client for multiple concurrent markets
pub struct ClobClient {
    ws_url: String,
    symbols: Vec<String>,
    token_to_symbol: HashMap<String, (String, String)>, // token_id -> (symbol, direction)
    price_sender: mpsc::Sender<SharePriceUpdate>,
    market_sender: Option<mpsc::Sender<MarketData>>,
    market_ends: HashMap<String, u64>, // symbol -> window_end_ts
    books: HashMap<String, OrderBookCache>, // token_id -> orderbook depth
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
            books: HashMap::new(),
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

    async fn handle_message(&mut self, text: &str) {
        if let Ok(arr) = serde_json::from_str::<Vec<Value>>(text) {
            for item in arr { self.process_item(&item).await; }
        } else if let Ok(obj) = serde_json::from_str::<Value>(text) {
            self.process_item(&obj).await;
        }
    }

    fn parse_levels(value: Option<&Value>, ascending: bool) -> Vec<BookLevel> {
        let mut levels = value.and_then(|v| v.as_array()).map(|arr| {
            arr.iter().filter_map(|level| {
                let price = level.get("price").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok())?;
                let size = level.get("size").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok())?;
                (price > 0.0 && size > 0.0).then_some(BookLevel { price, size })
            }).collect::<Vec<_>>()
        }).unwrap_or_default();
        if ascending {
            levels.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            levels.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        }
        levels
    }

    fn upsert_level(levels: &mut Vec<BookLevel>, price: f64, size: f64, ascending: bool) {
        if let Some(existing) = levels.iter_mut().find(|level| (level.price - price).abs() < f64::EPSILON) {
            existing.size = size;
        } else {
            levels.push(BookLevel { price, size });
        }
        levels.retain(|level| level.size > 0.0);
        if ascending {
            levels.sort_by(|a, b| a.price.partial_cmp(&b.price).unwrap_or(std::cmp::Ordering::Equal));
        } else {
            levels.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    async fn send_book_update(&self, asset_id: &str, best_bid: f64, best_ask: f64) {
        if let Some((symbol, direction)) = self.token_to_symbol.get(asset_id) {
            let book = self.books.get(asset_id).cloned().unwrap_or_default();
            let bid = if best_bid > 0.0 { best_bid } else { book.bids.first().map(|l| l.price).unwrap_or(0.0) };
            let ask = if best_ask > 0.0 { best_ask } else { book.asks.first().map(|l| l.price).unwrap_or(0.0) };
            if bid > 0.0 || ask > 0.0 {
                let mid_price = if bid > 0.0 && ask > 0.0 { (bid + ask) / 2.0 } else { 0.0 };
                let _ = self.price_sender.send(SharePriceUpdate { symbol: symbol.clone(), direction: direction.clone(), best_bid: bid, best_ask: ask, mid_price, bids: book.bids, asks: book.asks, timestamp: Instant::now() }).await;
            }
        }
    }

    async fn process_item(&mut self, item: &Value) {
        let event_type = item.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
        if event_type == "book" {
            if let Some(asset_id) = item.get("asset_id").and_then(|v| v.as_str()) {
                self.books.insert(asset_id.to_string(), OrderBookCache { bids: Self::parse_levels(item.get("bids"), false), asks: Self::parse_levels(item.get("asks"), true) });
                self.send_book_update(asset_id, 0.0, 0.0).await;
            }
            return;
        }
        if event_type == "price_change" {
            if let Some(changes) = item.get("price_changes").and_then(|v| v.as_array()) {
                for change in changes {
                    let Some(asset_id) = change.get("asset_id").and_then(|v| v.as_str()).or_else(|| item.get("asset_id").and_then(|v| v.as_str())) else { continue; };
                    let price = change.get("price").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok());
                    let size = change.get("size").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok());
                    if let (Some(price), Some(size)) = (price, size) {
                        let book = self.books.entry(asset_id.to_string()).or_default();
                        match change.get("side").and_then(|v| v.as_str()).unwrap_or("").to_ascii_uppercase().as_str() {
                            "BUY" | "BID" | "BIDS" => Self::upsert_level(&mut book.bids, price, size, false),
                            "SELL" | "ASK" | "ASKS" => Self::upsert_level(&mut book.asks, price, size, true),
                            _ => {}
                        }
                    }
                    let best_bid = change.get("best_bid").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).or_else(|| item.get("best_bid").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok())).unwrap_or(0.0);
                    let best_ask = change.get("best_ask").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).or_else(|| item.get("best_ask").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok())).unwrap_or(0.0);
                    self.send_book_update(asset_id, best_bid, best_ask).await;
                }
            } else if let Some(asset_id) = item.get("asset_id").and_then(|v| v.as_str()) {
                let best_bid = item.get("best_bid").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                let best_ask = item.get("best_ask").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                self.send_book_update(asset_id, best_bid, best_ask).await;
            }
            return;
        }
        if event_type == "best_bid_ask" {
            if let Some(asset_id) = item.get("asset_id").and_then(|v| v.as_str()) {
                let best_bid = item.get("best_bid").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                let best_ask = item.get("best_ask").and_then(|v| v.as_str()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
                self.send_book_update(asset_id, best_bid, best_ask).await;
            }
        }
    }
}
