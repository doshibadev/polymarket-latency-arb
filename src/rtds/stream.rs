use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{error, info};

#[derive(Clone, Copy)]
struct FeedSymbol {
    app_symbol: &'static str,
    binance_symbol: &'static str,
    chainlink_symbol: &'static str,
}

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

#[derive(Debug, Clone, Deserialize)]
struct BinanceTradeMessage {
    #[serde(rename = "p")]
    price: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BinanceBookTickerMessage {
    #[serde(rename = "b")]
    best_bid: String,
    #[serde(rename = "a")]
    best_ask: String,
}

#[derive(Debug, Clone, Deserialize)]
struct BinanceCombinedStreamMessage {
    stream: String,
    data: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    Binance,
    Chainlink,
}

#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub symbol: &'static str, // "BTC", "ETH", "SOL"
    pub price: f64,
    pub timestamp: Instant,
    pub source: PriceSource,
}

fn emit_price(
    tx: &mpsc::Sender<PriceUpdate>,
    symbol: &'static str,
    price: f64,
    source: PriceSource,
) {
    let _ = tx.try_send(PriceUpdate {
        symbol,
        price,
        timestamp: Instant::now(),
        source,
    });
}

fn feed_symbol(symbol: &'static str) -> Option<FeedSymbol> {
    match symbol {
        "BTC" => Some(FeedSymbol {
            app_symbol: "BTC",
            binance_symbol: "btcusdt",
            chainlink_symbol: "btc/usd",
        }),
        "ETH" => Some(FeedSymbol {
            app_symbol: "ETH",
            binance_symbol: "ethusdt",
            chainlink_symbol: "eth/usd",
        }),
        "SOL" => Some(FeedSymbol {
            app_symbol: "SOL",
            binance_symbol: "solusdt",
            chainlink_symbol: "sol/usd",
        }),
        "XRP" => Some(FeedSymbol {
            app_symbol: "XRP",
            binance_symbol: "xrpusdt",
            chainlink_symbol: "xrp/usd",
        }),
        _ => None,
    }
}

/// Direct Binance WebSocket for low-latency BTC price (~10ms updates vs RTDS relay ~50ms)
async fn run_binance_direct(
    symbol: FeedSymbol,
    tx: mpsc::Sender<PriceUpdate>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "wss://stream.binance.com:9443/stream?streams={}@trade/{}@bookTicker",
        symbol.binance_symbol, symbol.binance_symbol
    );
    loop {
        match connect_async_tls_with_config(&url, None, true, None).await {
            Ok((ws_stream, _)) => {
                info!(
                    symbol = symbol.app_symbol,
                    "Direct Binance WebSocket connected"
                );
                let (mut write, mut read) = ws_stream.split();
                let mut ping_interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(msg) = serde_json::from_str::<BinanceCombinedStreamMessage>(&text) {
                                        if msg.stream.ends_with("@trade") {
                                            if let Ok(trade) = serde_json::from_value::<BinanceTradeMessage>(msg.data) {
                                                if let Ok(price) = trade.price.parse::<f64>() {
                                                    emit_price(&tx, symbol.app_symbol, price, PriceSource::Binance);
                                                }
                                            }
                                        } else if msg.stream.ends_with("@bookTicker") {
                                            if let Ok(book) = serde_json::from_value::<BinanceBookTickerMessage>(msg.data) {
                                                if let (Ok(best_bid), Ok(best_ask)) = (
                                                    book.best_bid.parse::<f64>(),
                                                    book.best_ask.parse::<f64>(),
                                                ) {
                                                    emit_price(
                                                        &tx,
                                                        symbol.app_symbol,
                                                        (best_bid + best_ask) / 2.0,
                                                        PriceSource::Binance,
                                                    );
                                                }
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
                error!(
                    symbol = symbol.app_symbol,
                    "Direct Binance WebSocket disconnected, reconnecting in 1s..."
                );
            }
            Err(e) => {
                error!(symbol = symbol.app_symbol, error = %e, "Direct Binance connect failed, retrying in 3s");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

pub struct RtdsStream {
    symbols: Vec<FeedSymbol>,
}

impl RtdsStream {
    pub fn new(symbols: Vec<String>) -> Self {
        let symbols = symbols
            .into_iter()
            .filter_map(|symbol| match symbol.as_str() {
                "BTC" => feed_symbol("BTC"),
                "ETH" => feed_symbol("ETH"),
                "SOL" => feed_symbol("SOL"),
                "XRP" => feed_symbol("XRP"),
                _ => None,
            })
            .collect();
        Self { symbols }
    }

    pub async fn run(
        &mut self,
        tx: mpsc::Sender<PriceUpdate>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for symbol in self.symbols.clone() {
            let tx_binance = tx.clone();
            tokio::spawn(async move {
                let _ = run_binance_direct(symbol, tx_binance).await;
            });

            let tx_rtds = tx.clone();
            tokio::spawn(async move {
                let url = "wss://ws-live-data.polymarket.com";
                info!(
                    symbol = symbol.app_symbol,
                    url, "Connecting to Polymarket RTDS"
                );
                let mut backoff_secs = 3u64;
                loop {
                    match Self::connect_and_stream(url, symbol, &tx_rtds).await {
                        Ok(_) => {
                            backoff_secs = 3;
                        }
                        Err(e) => {
                            error!(
                                symbol = symbol.app_symbol,
                                error = %e,
                                backoff = backoff_secs,
                                "RTDS WebSocket error, reconnecting..."
                            );
                            tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs))
                                .await;
                            backoff_secs = (backoff_secs * 2).min(60);
                        }
                    }
                }
            });
        }

        futures_util::future::pending::<()>().await;
        #[allow(unreachable_code)]
        Ok(())
    }

    async fn connect_and_stream(
        url: &str,
        symbol: FeedSymbol,
        tx: &mpsc::Sender<PriceUpdate>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (mut ws_stream, _) = connect_async_tls_with_config(url, None, true, None).await?;

        let binance_sub = serde_json::json!({
            "action": "subscribe",
            "subscriptions": [
                {
                    "topic": "crypto_prices",
                    "type": "update",
                    "filters": format!("{{\"symbol\":\"{}\"}}", symbol.binance_symbol)
                }
            ]
        });
        ws_stream
            .send(Message::Text(binance_sub.to_string().into()))
            .await?;

        let chainlink_sub = serde_json::json!({
            "action": "subscribe",
            "subscriptions": [
                {
                    "topic": "crypto_prices_chainlink",
                    "type": "*",
                    "filters": format!("{{\"symbol\":\"{}\"}}", symbol.chainlink_symbol)
                }
            ]
        });
        ws_stream
            .send(Message::Text(chainlink_sub.to_string().into()))
            .await?;

        info!(
            symbol = symbol.app_symbol,
            binance_symbol = symbol.binance_symbol,
            chainlink_symbol = symbol.chainlink_symbol,
            "RTDS connected"
        );
        let (mut write, mut read) = ws_stream.split();
        let mut heartbeat = tokio::time::interval(tokio::time::Duration::from_secs(5));

        loop {
            tokio::select! {
                message = read.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(msg) = serde_json::from_str::<RtdsMessage>(&text) {
                                if msg.payload.symbol.to_lowercase() != symbol.binance_symbol
                                    && msg.payload.symbol.to_lowercase() != symbol.chainlink_symbol
                                {
                                    continue;
                                }

                                let source = if msg.topic == "crypto_prices" {
                                    PriceSource::Binance
                                } else {
                                    PriceSource::Chainlink
                                };

                                emit_price(tx, symbol.app_symbol, msg.payload.value, source);
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
