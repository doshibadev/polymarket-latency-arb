use crate::polymarket::{fetch_current_market, MarketData};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{error, info};

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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ClobEnvelope {
    Batch(Vec<ClobMessage>),
    Single(ClobMessage),
}

#[derive(Debug, Deserialize)]
struct ClobMessage {
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    bids: Vec<RawBookLevel>,
    #[serde(default)]
    asks: Vec<RawBookLevel>,
    #[serde(default)]
    price_changes: Vec<RawPriceChange>,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBookLevel {
    price: String,
    size: String,
}

#[derive(Debug, Deserialize)]
struct RawPriceChange {
    #[serde(default)]
    asset_id: Option<String>,
    #[serde(default)]
    price: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    best_bid: Option<String>,
    #[serde(default)]
    best_ask: Option<String>,
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
            token_to_symbol.insert(
                m.down_token_id.clone(),
                (m.symbol.clone(), "DOWN".to_string()),
            );
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
        let (mut ws_stream, _) = connect_async_tls_with_config(&self.ws_url, None, true, None).await?;
        info!("CLOB WebSocket connected");

        self.subscribe_all(&mut ws_stream).await?;
        LOGGED.store(false, Ordering::Relaxed);

        let (mut write, mut read) = ws_stream.split();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        let mut refresh_check = tokio::time::interval(Duration::from_secs(1));
        let (prefetch_tx, mut prefetch_rx) = mpsc::channel::<(String, Option<MarketData>)>(16);
        let mut prefetched_markets = HashMap::<String, MarketData>::new();
        let mut inflight_prefetches = HashSet::<String>::new();

        loop {
            while let Ok((symbol, prefetched)) = prefetch_rx.try_recv() {
                inflight_prefetches.remove(&symbol);
                if let Some(prefetched) = prefetched {
                    prefetched_markets.insert(prefetched.symbol.clone(), prefetched);
                }
            }

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
                            if now >= end_ts.saturating_sub(10)
                                && now < end_ts.saturating_sub(1)
                                && !prefetched_markets
                                    .get(symbol)
                                    .is_some_and(|market| market.window_end_ts > end_ts)
                                && inflight_prefetches.insert(symbol.clone())
                            {
                                let symbol_clone = symbol.clone();
                                let prefetch_tx = prefetch_tx.clone();
                                tokio::spawn(async move {
                                    let next_market = fetch_current_market(&symbol_clone).await;
                                    let _ = prefetch_tx.send((symbol_clone, next_market)).await;
                                });
                            }

                            if now >= end_ts.saturating_sub(1) {
                                if let Some(new_m) = prefetched_markets.remove(symbol) {
                                    if new_m.window_end_ts > end_ts {
                                        info!(symbol = %symbol, "Applying prefetched market refresh");
                                        let old_tokens: Vec<String> = self.token_to_symbol.keys().cloned().collect();
                                        if !old_tokens.is_empty() {
                                            let unsub_msg = serde_json::json!({
                                                "assets_ids": old_tokens,
                                                "operation": "unsubscribe",
                                            });
                                            let _ = write.send(Message::Text(unsub_msg.to_string().into())).await;
                                        }

                                        self.token_to_symbol.retain(|_, (sym, _)| sym != symbol);
                                        self.token_to_symbol.insert(new_m.up_token_id.clone(), (new_m.symbol.clone(), "UP".to_string()));
                                        self.token_to_symbol.insert(new_m.down_token_id.clone(), (new_m.symbol.clone(), "DOWN".to_string()));
                                        self.market_ends.insert(symbol.clone(), new_m.window_end_ts);

                                        if let Some(market_tx) = &self.market_sender {
                                            let _ = market_tx.try_send(new_m);
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

    async fn subscribe_all<W>(
        &self,
        ws_stream: &mut W,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
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

        ws_stream
            .send(Message::Text(sub_msg.to_string().into()))
            .await?;
        info!("CLOB subscribed to all active markets: {:?}", self.symbols);
        Ok(())
    }

    async fn handle_message(&mut self, text: &str) {
        if let Ok(message) = serde_json::from_str::<ClobEnvelope>(text) {
            match message {
                ClobEnvelope::Batch(messages) => {
                    for item in messages {
                        self.process_item(item).await;
                    }
                }
                ClobEnvelope::Single(message) => {
                    self.process_item(message).await;
                }
            }
        }
    }

    fn parse_levels(levels: &[RawBookLevel], ascending: bool) -> Vec<BookLevel> {
        let mut levels = levels
            .iter()
            .filter_map(|level| {
                let price = level.price.parse::<f64>().ok()?;
                let size = level.size.parse::<f64>().ok()?;
                (price > 0.0 && size > 0.0).then_some(BookLevel { price, size })
            })
            .collect::<Vec<_>>();
        if ascending {
            levels.sort_by(|a, b| {
                a.price
                    .partial_cmp(&b.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            levels.sort_by(|a, b| {
                b.price
                    .partial_cmp(&a.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        levels
    }

    fn upsert_level(levels: &mut Vec<BookLevel>, price: f64, size: f64, ascending: bool) {
        let insert_at = levels.binary_search_by(|level| {
            if ascending {
                level
                    .price
                    .partial_cmp(&price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                price
                    .partial_cmp(&level.price)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }
        });

        match insert_at {
            Ok(idx) => {
                if size > 0.0 {
                    levels[idx].size = size;
                } else {
                    levels.remove(idx);
                }
            }
            Err(idx) if size > 0.0 => {
                levels.insert(idx, BookLevel { price, size });
            }
            Err(_) => {}
        }
    }

    fn parse_num(value: Option<&str>) -> f64 {
        value.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
    }

    fn send_book_update(&self, asset_id: &str, best_bid: f64, best_ask: f64, include_depth: bool) {
        if let Some((symbol, direction)) = self.token_to_symbol.get(asset_id) {
            let book = self.books.get(asset_id);
            let bid = if best_bid > 0.0 {
                best_bid
            } else {
                book.and_then(|book| book.bids.first().map(|l| l.price))
                    .unwrap_or(0.0)
            };
            let ask = if best_ask > 0.0 {
                best_ask
            } else {
                book.and_then(|book| book.asks.first().map(|l| l.price))
                    .unwrap_or(0.0)
            };
            if bid > 0.0 || ask > 0.0 {
                let mid_price = if bid > 0.0 && ask > 0.0 {
                    (bid + ask) / 2.0
                } else {
                    0.0
                };
                let _ = self
                    .price_sender
                    .try_send(SharePriceUpdate {
                        symbol: symbol.clone(),
                        direction: direction.clone(),
                        best_bid: bid,
                        best_ask: ask,
                        mid_price,
                        bids: if include_depth {
                            book.map(|book| book.bids.clone()).unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                        asks: if include_depth {
                            book.map(|book| book.asks.clone()).unwrap_or_default()
                        } else {
                            Vec::new()
                        },
                        timestamp: Instant::now(),
                    });
            }
        }
    }

    async fn process_item(&mut self, item: ClobMessage) {
        let event_type = item.event_type.as_str();
        if event_type == "book" {
            if let Some(asset_id) = item.asset_id.as_deref() {
                self.books.insert(
                    asset_id.to_string(),
                    OrderBookCache {
                        bids: Self::parse_levels(&item.bids, false),
                        asks: Self::parse_levels(&item.asks, true),
                    },
                );
                self.send_book_update(asset_id, 0.0, 0.0, true);
            }
            return;
        }
        if event_type == "price_change" {
            if !item.price_changes.is_empty() {
                for change in item.price_changes {
                    let Some(asset_id) = change.asset_id.as_deref().or(item.asset_id.as_deref()) else {
                        continue;
                    };
                    let price = change.price.as_deref().and_then(|value| value.parse::<f64>().ok());
                    let size = change.size.as_deref().and_then(|value| value.parse::<f64>().ok());
                    if let (Some(price), Some(size)) = (price, size) {
                        let book = self.books.entry(asset_id.to_string()).or_default();
                        let side = change.side.as_deref().unwrap_or("");
                        match side.to_ascii_uppercase().as_str() {
                            "BUY" | "BID" | "BIDS" => {
                                Self::upsert_level(&mut book.bids, price, size, false)
                            }
                            "SELL" | "ASK" | "ASKS" => {
                                Self::upsert_level(&mut book.asks, price, size, true)
                            }
                            _ => {}
                        }
                    }
                    let best_bid =
                        Self::parse_num(change.best_bid.as_deref().or(item.best_bid.as_deref()));
                    let best_ask =
                        Self::parse_num(change.best_ask.as_deref().or(item.best_ask.as_deref()));
                    self.send_book_update(asset_id, best_bid, best_ask, false);
                }
            } else if let Some(asset_id) = item.asset_id.as_deref() {
                let best_bid = Self::parse_num(item.best_bid.as_deref());
                let best_ask = Self::parse_num(item.best_ask.as_deref());
                self.send_book_update(asset_id, best_bid, best_ask, false);
            }
            return;
        }
        if event_type == "best_bid_ask" {
            if let Some(asset_id) = item.asset_id.as_deref() {
                let best_bid = Self::parse_num(item.best_bid.as_deref());
                let best_ask = Self::parse_num(item.best_ask.as_deref());
                self.send_book_update(asset_id, best_bid, best_ask, false);
            }
        }
    }
}
