use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{debug, info, warn};

const FRONTEND_DIST_DIR: &str = "web/dist";
const FRONTEND_ENTRYPOINT: &str = "web/dist/index.html";
const FRONTEND_SETUP_PAGE: &str = r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Lattice Terminal</title>
    <style>
      :root {
        color-scheme: dark;
        font-family: "IBM Plex Sans", "Segoe UI", sans-serif;
        background:
          radial-gradient(circle at top, rgba(203, 237, 117, 0.14), transparent 28%),
          linear-gradient(180deg, #0f1311 0%, #090b0a 100%);
        color: #e8ecd9;
      }
      * { box-sizing: border-box; }
      body {
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        padding: 24px;
      }
      main {
        width: min(760px, 100%);
        padding: 32px;
        border: 1px solid rgba(232, 236, 217, 0.12);
        border-radius: 28px;
        background: rgba(15, 19, 17, 0.78);
        box-shadow: 0 24px 80px rgba(0, 0, 0, 0.3);
      }
      p {
        color: rgba(232, 236, 217, 0.72);
      }
      code {
        font-family: "IBM Plex Mono", "SFMono-Regular", monospace;
      }
      pre {
        margin: 18px 0 0;
        padding: 16px;
        border-radius: 18px;
        overflow-x: auto;
        background: #0b0f0c;
      }
    </style>
  </head>
  <body>
    <main>
      <p>Lattice Terminal</p>
      <h1>Frontend is currently removed.</h1>
      <p>
        The backend server is running, but no frontend is installed right now.
        Add a new frontend later and place its production build in
        <code>web/dist</code> if you want Axum to serve it.
      </p>
    </main>
  </body>
</html>
"#;

const NUMERIC_CONFIG_FIELDS: &[(&str, &str)] = &[
    ("THRESHOLD_BPS", "threshold_bps"),
    ("PORTFOLIO_PCT", "portfolio_pct"),
    ("CRYPTO_FEE_RATE", "crypto_fee_rate"),
    ("MAX_ENTRY_PRICE", "max_entry_price"),
    ("MIN_ENTRY_PRICE", "min_entry_price"),
    ("TREND_REVERSAL_PCT", "trend_reversal_pct"),
    ("TREND_REVERSAL_THRESHOLD", "trend_reversal_threshold"),
    ("SPIKE_FADED_PCT", "spike_faded_pct"),
    ("SPIKE_FADED_MS", "spike_faded_ms"),
    ("MIN_HOLD_MS", "min_hold_ms"),
    ("TRAILING_STOP_PCT", "trailing_stop_pct"),
    ("TRAILING_STOP_ACTIVATION", "trailing_stop_activation"),
    ("STOP_LOSS_PCT", "stop_loss_pct"),
    ("HOLD_MIN_SHARE_PRICE", "hold_min_share_price"),
    ("HOLD_SAFETY_MARGIN", "hold_safety_margin"),
    ("EARLY_EXIT_LOSS_PCT", "early_exit_loss_pct"),
    ("HOLD_MARGIN_PER_SECOND", "hold_margin_per_second"),
    ("HOLD_MAX_SECONDS", "hold_max_seconds"),
    ("HOLD_MAX_CROSSINGS", "hold_max_crossings"),
    ("SPIKE_SUSTAIN_MS", "spike_sustain_ms"),
    ("EXECUTION_DELAY_MS", "execution_delay_ms"),
    ("MIN_PRICE_DISTANCE", "min_price_distance"),
    ("MAX_ORDERS_PER_MINUTE", "max_orders_per_minute"),
    ("MAX_DAILY_LOSS", "max_daily_loss"),
    ("MAX_EXPOSURE_PER_MARKET", "max_exposure_per_market"),
    ("MAX_DRAWDOWN_PCT", "max_drawdown_pct"),
    ("TREND_MIN_MAGNITUDE_USD", "trend_min_magnitude_usd"),
    ("COUNTER_TREND_MULTIPLIER", "counter_trend_multiplier"),
    ("TREND_MAX_MAGNITUDE_USD", "trend_max_magnitude_usd"),
    ("PTB_NEUTRAL_ZONE_USD", "ptb_neutral_zone_usd"),
    (
        "PTB_MAX_COUNTER_DISTANCE_USD",
        "ptb_max_counter_distance_usd",
    ),
];

const BOOL_CONFIG_FIELDS: &[(&str, &str)] = &[("TREND_FILTER_ENABLED", "trend_filter_enabled")];

pub struct ServerState {
    pub tx: broadcast::Sender<String>,
    pub cmd_tx: mpsc::Sender<serde_json::Value>,
}

pub async fn run_server(tx: broadcast::Sender<String>, cmd_tx: mpsc::Sender<serde_json::Value>) {
    let state = Arc::new(ServerState { tx, cmd_tx });

    let app = if std::path::Path::new(FRONTEND_ENTRYPOINT).exists() {
        info!("Serving frontend assets from {}", FRONTEND_DIST_DIR);

        Router::new()
            .route("/ws", get(ws_handler))
            .route("/config", get(get_config))
            .route("/settings", post(update_settings))
            .route("/command", post(send_command))
            .fallback_service(
                ServeDir::new(FRONTEND_DIST_DIR).fallback(ServeFile::new(FRONTEND_ENTRYPOINT)),
            )
            .layer(CorsLayer::permissive())
            .with_state(state)
    } else {
        warn!(
            "Frontend build not found at {:?}. Serving setup page instead.",
            std::path::Path::new(FRONTEND_ENTRYPOINT)
        );

        Router::new()
            .route("/", get(frontend_setup_page))
            .route("/ws", get(ws_handler))
            .route("/config", get(get_config))
            .route("/settings", post(update_settings))
            .route("/command", post(send_command))
            .fallback(get(frontend_setup_page))
            .layer(CorsLayer::permissive())
            .with_state(state)
    };

    // Read port from environment variable, default to 3000
    let port = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3000);

    let addr = format!("0.0.0.0:{}", port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            panic!(
                "Failed to bind to {}: {}. Port might already be in use.",
                addr, e
            );
        }
    };
    info!("Dashboard server running on http://localhost:{}", port);
    info!("Working directory: {:?}", std::env::current_dir());
    axum::serve(listener, app).await.unwrap();
}

async fn get_config() -> impl IntoResponse {
    // Re-read .env fresh every time settings modal is opened
    let _ = dotenvy::dotenv_override();
    let mut map = serde_json::Map::new();
    for (env_key, json_key) in NUMERIC_CONFIG_FIELDS {
        if let Ok(val) = std::env::var(env_key) {
            if let Ok(n) = val.parse::<f64>() {
                map.insert(json_key.to_string(), serde_json::json!(n));
            }
        }
    }
    for (env_key, json_key) in BOOL_CONFIG_FIELDS {
        if let Ok(val) = std::env::var(env_key) {
            if let Ok(b) = val.parse::<bool>() {
                map.insert(json_key.to_string(), serde_json::json!(b));
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

    // Persist to .env, falling back to .env.example on first run.
    let content = std::fs::read_to_string(".env")
        .or_else(|_| std::fs::read_to_string(".env.example"))
        .unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    for (env_key, json_key) in NUMERIC_CONFIG_FIELDS {
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

    for (env_key, json_key) in BOOL_CONFIG_FIELDS {
        if let Some(val) = payload.get(json_key).and_then(|v| v.as_bool()) {
            let val_str = if val { "true" } else { "false" };
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

    let mut cmd = payload;
    cmd["_type"] = serde_json::json!("settings");
    let _ = state.cmd_tx.send(cmd).await;
    Json(serde_json::json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use super::{BOOL_CONFIG_FIELDS, NUMERIC_CONFIG_FIELDS};

    #[test]
    fn config_field_keys_are_unique() {
        let mut env_keys = std::collections::HashSet::new();
        let mut json_keys = std::collections::HashSet::new();

        for (env_key, json_key) in NUMERIC_CONFIG_FIELDS
            .iter()
            .chain(BOOL_CONFIG_FIELDS.iter())
        {
            assert!(env_keys.insert(*env_key), "duplicate env key: {}", env_key);
            assert!(
                json_keys.insert(*json_key),
                "duplicate json key: {}",
                json_key
            );
        }
    }
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
                        if let Err(_) = sender.send(Message::Text(msg.into())).await {
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

async fn frontend_setup_page() -> Html<&'static str> {
    Html(FRONTEND_SETUP_PAGE)
}
