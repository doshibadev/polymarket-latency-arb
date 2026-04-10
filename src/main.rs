pub mod arb;
pub mod config;
pub mod error;
pub mod execution;
pub mod polymarket;
pub mod rtds;
pub mod server;

use arb::ArbEngine;
use config::AppConfig;
use execution::LiveWallet;
use polymarket::{ClobClient, fetch_current_market, MarketData};
use tokio::sync::{mpsc, broadcast};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    tracing::info!("Polymarket Latency Arbitrage Bot V2 starting");

    let config = AppConfig::load()?;

    // Fetch initial markets for all symbols
    let symbols = vec!["BTC"];
    let mut initial_markets = Vec::new();
    for symbol in &symbols {
        if let Some(market) = fetch_current_market(symbol).await {
            initial_markets.push(market);
        } else {
            tracing::error!("Failed to fetch initial market for {}", symbol);
        }
    }

    if initial_markets.is_empty() {
        return Err("No active markets found".into());
    }

    // Channels
    let (price_tx, price_rx) = mpsc::channel(2000);
    let (clob_tx, clob_rx) = mpsc::channel(2000);
    let (market_tx, market_rx) = mpsc::channel(10);
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(100);
    let (cmd_tx, cmd_rx) = mpsc::channel(10);

    // Dashboard Server
    let server_tx = broadcast_tx.clone();
    tokio::spawn(async move {
        server::run_server(server_tx, cmd_tx).await;
    });

    // CLOB WebSocket
    let clob = ClobClient::new(initial_markets.clone(), clob_tx, Some(market_tx));
    tokio::spawn(async move {
        if let Err(e) = clob.run().await { tracing::error!(error = %e, "CLOB stream error"); }
    });

    // RTDS Stream
    tokio::spawn(async move {
        let mut stream = crate::rtds::RtdsStream::new();
        if let Err(e) = stream.run(price_tx).await { tracing::error!(error = %e, "RTDS stream error"); }
    });

    // Arb Engine
    let mut engine = ArbEngine::new(config.clone(), price_rx, clob_rx, market_rx, broadcast_tx, cmd_rx);

    // Live trading mode
    if !config.paper_trading {
        tracing::info!("LIVE TRADING MODE — initializing live wallet...");
        match execution::LiveWallet::new(config).await {
            Ok(lw) => {
                engine.live_wallet = Some(lw);
                tracing::info!("Live wallet ready");
            }
            Err(e) => {
                return Err(format!("Failed to initialize live wallet: {}", e).into());
            }
        }
    } else {
        tracing::info!("Paper trading mode");
    }
    
    // Initialize engine with current markets
    for m in initial_markets {
        engine.handle_market_update(m).await;
    }

    engine.run().await?;
    Ok(())
}
