use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::{get, post, any_service},
    Router, Json,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{info, warn, debug};

pub struct ServerState {
    pub tx: broadcast::Sender<String>,
    pub cmd_tx: mpsc::Sender<serde_json::Value>,
}

pub async fn run_server(tx: broadcast::Sender<String>, cmd_tx: mpsc::Sender<serde_json::Value>) {
    let state = Arc::new(ServerState { tx, cmd_tx });

    let app = Router::new()
        .route("/", any_service(ServeFile::new("dashboard.html")))
        .route("/ws", get(ws_handler))
        .route("/config", get(get_config))
        .route("/settings", post(update_settings))
        .route("/command", post(send_command))
        .fallback_service(ServeDir::new("."))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Dashboard server running on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn get_config() -> impl IntoResponse {
    // Re-read .env fresh every time settings modal is opened
    let _ = dotenvy::dotenv_override();
    let pairs = [
        ("THRESHOLD_BPS", "threshold_bps"),
        ("PORTFOLIO_PCT", "portfolio_pct"),
        ("MAX_ENTRY_PRICE", "max_entry_price"),
        ("PROFIT_TARGET_PCT", "profit_target_pct"),
        ("TRAILING_STOP_PCT", "trailing_stop_pct"),
        ("SPIKE_FADED_PCT", "spike_faded_pct"),
        ("MAX_SPREAD_BPS", "max_spread_bps"),
        ("SPIKE_SCALING_FACTOR", "spike_scaling_factor"),
        ("EMA_ALPHA", "ema_alpha"),
        ("CRYPTO_FEE_RATE", "crypto_fee_rate"),
    ];
    let mut map = serde_json::Map::new();
    for (env_key, json_key) in &pairs {
        if let Ok(val) = std::env::var(env_key) {
            if let Ok(n) = val.parse::<f64>() {
                map.insert(json_key.to_string(), serde_json::json!(n));
            }
        }
    }
    Json(serde_json::Value::Object(map))
}

async fn update_settings(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!(settings = ?payload, "Received settings update from dashboard");

    // Persist to .env
    if let Ok(content) = std::fs::read_to_string(".env") {
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

        let mappings: &[(&str, &str)] = &[
            ("threshold_bps", "THRESHOLD_BPS"),
            ("portfolio_pct", "PORTFOLIO_PCT"),
            ("profit_target_pct", "PROFIT_TARGET_PCT"),
            ("trailing_stop_pct", "TRAILING_STOP_PCT"),
            ("spike_faded_pct", "SPIKE_FADED_PCT"),
            ("max_spread_bps", "MAX_SPREAD_BPS"),
            ("max_entry_price", "MAX_ENTRY_PRICE"),
            ("spike_scaling_factor", "SPIKE_SCALING_FACTOR"),
            ("ema_alpha", "EMA_ALPHA"),
        ("execution_delay_ms", "EXECUTION_DELAY_MS"),
            ("execution_delay_ms", "EXECUTION_DELAY_MS"),
        ];

        for (json_key, env_key) in mappings {
            if let Some(val) = payload.get(json_key) {
                let val_str = val.to_string();
                let mut found = false;
                for line in &mut lines {
                    if line.starts_with(&format!("{}=", env_key)) {
                        *line = format!("{}={}", env_key, val_str);
                        found = true;
                        break;
                    }
                }
                if !found {
                    lines.push(format!("{}={}", env_key, val_str));
                }
            }
        }

        let _ = std::fs::write(".env", lines.join("\n") + "\n");
    }

    let mut cmd = payload;
    cmd["_type"] = serde_json::json!("settings");
    let _ = state.cmd_tx.send(cmd).await;
    Json(serde_json::json!({ "status": "ok" }))
}

async fn send_command(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!(cmd = ?payload, "Received command from dashboard");
    let _ = state.cmd_tx.send(payload).await;
    Json(serde_json::json!({ "status": "ok" }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<ServerState>) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    debug!("New dashboard connection established");

    loop {
        tokio::select! {
            // Forward broadcast messages from the engine to the dashboard
            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(msg) => {
                        debug!("Sending state to dashboard ({} bytes)", msg.len());
                        if let Err(_) = sender.send(Message::Text(msg)).await {
                            // Standard disconnect, no need to log as ERROR
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Dashboard socket lagged by {} messages", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Listen for client messages or connection closure
            client_msg = receiver.next() => {
                if client_msg.is_none() {
                    // Client closed connection
                    break;
                }
                // We ignore incoming messages from the dashboard for now
            }
        }
    }

    debug!("Dashboard client disconnected");
}
