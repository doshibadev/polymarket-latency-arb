#![recursion_limit = "256"]

pub mod arb;
pub mod config;
pub mod error;
pub mod execution;
pub mod polymarket;
pub mod rtds;
pub mod server;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use arb::ArbEngine;
use config::AppConfig;
use execution::spawn_live_execution;
use polymarket::{fetch_current_market, ClobClient};
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    tracing::info!("Polymarket Latency Arbitrage Bot V2 starting");

    let config = AppConfig::load()?;

    // Fetch initial markets for all symbols
    let symbols = vec!["BTC", "ETH"];
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
    let startup_preflight = std::sync::Arc::new(RwLock::new(serde_json::json!({
        "mode": if config.paper_trading { "paper" } else { "live" },
        "status": "pending",
    })));

    // Dashboard Server
    let server_tx = broadcast_tx.clone();
    let server_preflight = startup_preflight.clone();
    tokio::spawn(async move {
        server::run_server(server_tx, cmd_tx, server_preflight).await;
    });

    // CLOB WebSocket
    let clob = ClobClient::new(initial_markets.clone(), clob_tx, Some(market_tx));
    tokio::spawn(async move {
        if let Err(e) = clob.run().await {
            tracing::error!(error = %e, "CLOB stream error");
        }
    });

    // RTDS Stream
    tokio::spawn(async move {
        let mut stream = crate::rtds::RtdsStream::new(vec!["BTC", "ETH"]);
        if let Err(e) = stream.run(price_tx).await {
            tracing::error!(error = %e, "RTDS stream error");
        }
    });

    // Arb Engine
    let mut engine = ArbEngine::new(
        config.clone(),
        price_rx,
        clob_rx,
        market_rx,
        broadcast_tx,
        cmd_rx,
    );

    // Live trading mode
    if !config.paper_trading {
        tracing::info!("LIVE TRADING MODE — initializing live wallet...");
        match execution::LiveWallet::new(config).await {
            Ok(mut lw) => {
                lw.recover_startup_state(&initial_markets)
                    .await
                    .map_err(|err| format!("Failed live startup recovery: {err}"))?;
                let report = lw
                    .run_startup_preflight(&initial_markets)
                    .await
                    .map_err(|err| format!("Failed live startup preflight: {err}"))?;
                *startup_preflight.write().await = serde_json::json!({
                    "mode": "live",
                    "status": "passed",
                    "report": report.to_json(),
                });
                let (handle, live_event_rx) = spawn_live_execution(lw);
                engine.live_execution = Some(handle);
                engine.live_event_rx = Some(live_event_rx);
                tracing::info!("Live wallet ready after startup preflight");
            }
            Err(e) => {
                return Err(format!("Failed to initialize live wallet: {}", e).into());
            }
        }
    } else {
        *startup_preflight.write().await = serde_json::json!({
            "mode": "paper",
            "status": "skipped",
        });
        tracing::info!("Paper trading mode");
    }

    // Initialize engine with current markets
    for m in initial_markets {
        engine.handle_market_update(m).await;
    }

    engine.run().await?;
    Ok(())
}
