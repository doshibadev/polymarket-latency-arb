use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::Query,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tower_http::services::{ServeDir, ServeFile};
use tracing::{debug, info, warn};

use crate::config::AppConfig;

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
    ("ETH_THRESHOLD_BPS", "eth_threshold_bps"),
    ("PORTFOLIO_PCT", "portfolio_pct"),
    ("CRYPTO_FEE_RATE", "crypto_fee_rate"),
    ("MAX_ENTRY_PRICE", "max_entry_price"),
    ("MIN_ENTRY_PRICE", "min_entry_price"),
    ("TREND_REVERSAL_PCT", "trend_reversal_pct"),
    ("TREND_REVERSAL_THRESHOLD", "trend_reversal_threshold"),
    (
        "ETH_TREND_REVERSAL_THRESHOLD",
        "eth_trend_reversal_threshold",
    ),
    ("SPIKE_FADED_PCT", "spike_faded_pct"),
    ("SPIKE_FADED_MS", "spike_faded_ms"),
    ("MIN_HOLD_MS", "min_hold_ms"),
    ("TRAILING_STOP_PCT", "trailing_stop_pct"),
    ("TRAILING_STOP_ACTIVATION", "trailing_stop_activation"),
    ("STOP_LOSS_PCT", "stop_loss_pct"),
    ("HOLD_MIN_SHARE_PRICE", "hold_min_share_price"),
    ("HOLD_SAFETY_MARGIN", "hold_safety_margin"),
    ("ETH_HOLD_SAFETY_MARGIN", "eth_hold_safety_margin"),
    ("EARLY_EXIT_LOSS_PCT", "early_exit_loss_pct"),
    ("HOLD_MARGIN_PER_SECOND", "hold_margin_per_second"),
    ("ETH_HOLD_MARGIN_PER_SECOND", "eth_hold_margin_per_second"),
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
    ("ETH_TREND_MIN_MAGNITUDE_USD", "eth_trend_min_magnitude_usd"),
    ("COUNTER_TREND_MULTIPLIER", "counter_trend_multiplier"),
    ("TREND_MAX_MAGNITUDE_USD", "trend_max_magnitude_usd"),
    ("ETH_TREND_MAX_MAGNITUDE_USD", "eth_trend_max_magnitude_usd"),
    ("PTB_NEUTRAL_ZONE_USD", "ptb_neutral_zone_usd"),
    ("ETH_PTB_NEUTRAL_ZONE_USD", "eth_ptb_neutral_zone_usd"),
    (
        "PTB_MAX_COUNTER_DISTANCE_USD",
        "ptb_max_counter_distance_usd",
    ),
    (
        "ETH_PTB_MAX_COUNTER_DISTANCE_USD",
        "eth_ptb_max_counter_distance_usd",
    ),
    ("LIVE_MAX_ORDER_USDC", "live_max_order_usdc"),
    ("LIVE_MAX_SESSION_LOSS_USDC", "live_max_session_loss_usdc"),
    ("LIVE_MAX_OPEN_POSITIONS", "live_max_open_positions"),
    ("LIVE_MAX_SLIPPAGE_CENTS", "live_max_slippage_cents"),
    ("LIVE_MAX_QUOTE_AGE_MS", "live_max_quote_age_ms"),
    ("LIVE_MIN_GAS_POL", "live_min_gas_pol"),
    ("LIVE_MIN_USDC_BALANCE", "live_min_usdc_balance"),
    ("LIVE_MAX_FEED_AGE_MS", "live_max_feed_age_ms"),
    ("LIVE_MAX_CLOCK_SKEW_MS", "live_max_clock_skew_ms"),
];

const BOOL_CONFIG_FIELDS: &[(&str, &str)] = &[
    ("TREND_FILTER_ENABLED", "trend_filter_enabled"),
    ("LIVE_DRY_RUN_ORDERS", "live_dry_run_orders"),
];

const STARTUP_ONLY_CONFIG_FIELDS: &[&str] = &[
    "dashboard_bind_addr",
    "dashboard_auth_token",
    "paper_trading",
    "starting_balance",
    "symbols",
    "paper_symbols",
    "live_symbols",
    "paper_db_path",
    "live_db_path",
];

pub struct ServerState {
    pub tx: broadcast::Sender<String>,
    pub cmd_tx: mpsc::Sender<serde_json::Value>,
    pub startup_preflight: Arc<RwLock<serde_json::Value>>,
    pub latest_fast: Arc<RwLock<Option<serde_json::Value>>>,
    pub config: AppConfig,
}

#[derive(Clone, Default, serde::Deserialize)]
struct AuthQuery {
    token: Option<String>,
}

#[derive(Debug)]
struct RequestAuth {
    token: Option<String>,
}

impl RequestAuth {
    fn from_parts(headers: &HeaderMap, query: AuthQuery) -> Self {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let token = query
            .token
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(bearer);
        Self { token }
    }
}

pub async fn run_server(
    tx: broadcast::Sender<String>,
    cmd_tx: mpsc::Sender<serde_json::Value>,
    startup_preflight: Arc<RwLock<serde_json::Value>>,
) {
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(err) => panic!("Failed to load dashboard config: {err}"),
    };
    let latest_fast = Arc::new(RwLock::new(None));
    let state = Arc::new(ServerState {
        tx: tx.clone(),
        cmd_tx,
        startup_preflight,
        latest_fast: latest_fast.clone(),
        config: config.clone(),
    });
    let mut snapshot_rx = tx.subscribe();
    tokio::spawn(async move {
        while let Ok(msg) = snapshot_rx.recv().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg) else {
                continue;
            };
            if value.get("_kind").and_then(|v| v.as_str()) == Some("fast") {
                *latest_fast.write().await = Some(value);
            }
        }
    });

    let app = if std::path::Path::new(FRONTEND_ENTRYPOINT).exists() {
        info!("Serving frontend assets from {}", FRONTEND_DIST_DIR);

        Router::new()
            .route("/ws", get(ws_handler))
            .route("/ws/fast", get(ws_fast_handler))
            .route("/ws/slow", get(ws_slow_handler))
            .route("/config", get(get_config))
            .route("/preflight", get(get_preflight))
            .route("/settings", post(update_settings))
            .route("/command", post(send_command))
            .fallback_service(
                ServeDir::new(FRONTEND_DIST_DIR).fallback(ServeFile::new(FRONTEND_ENTRYPOINT)),
            )
            .with_state(state)
    } else {
        warn!(
            "Frontend build not found at {:?}. Serving setup page instead.",
            std::path::Path::new(FRONTEND_ENTRYPOINT)
        );

        Router::new()
            .route("/", get(frontend_setup_page))
            .route("/ws", get(ws_handler))
            .route("/ws/fast", get(ws_fast_handler))
            .route("/ws/slow", get(ws_slow_handler))
            .route("/config", get(get_config))
            .route("/preflight", get(get_preflight))
            .route("/settings", post(update_settings))
            .route("/command", post(send_command))
            .fallback(get(frontend_setup_page))
            .with_state(state)
    };

    // Read port from environment variable, default to 3000
    let port = std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(3000);

    let addr = format!("{}:{}", config.dashboard_bind_addr, port);
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

async fn get_preflight(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let startup = state.startup_preflight.read().await.clone();
    let latest_fast = state.latest_fast.read().await.clone();
    Json(build_preflight_response(startup, latest_fast))
}

fn build_preflight_response(
    startup: serde_json::Value,
    latest_fast: Option<serde_json::Value>,
) -> serde_json::Value {
    let runtime_gate = latest_fast
        .as_ref()
        .and_then(|value| value.get("snapshot"))
        .and_then(|snapshot| snapshot.get("live_start_gate"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let running = latest_fast
        .as_ref()
        .and_then(|value| value.get("snapshot"))
        .and_then(|snapshot| snapshot.get("running"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    serde_json::json!({
        "startup": startup,
        "runtime_gate": runtime_gate,
        "running": running,
    })
}

async fn update_settings(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
    Json(payload): Json<serde_json::Value>,
) -> axum::response::Response {
    if let Err(status) =
        authorize_dashboard_request(&state, &RequestAuth::from_parts(&headers, query))
    {
        return status.into_response();
    }

    info!(settings = ?payload, "Received settings update from dashboard");

    let config = match validate_settings_update(&state, &payload).await {
        Ok(config) => config,
        Err((status, message)) => {
            return (status, Json(serde_json::json!({ "error": message }))).into_response();
        }
    };

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
    Json(serde_json::json!({
        "status": "ok",
        "paper_trading": config.paper_trading
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        authorize_dashboard_request, build_preflight_response, validate_command,
        validate_settings_update, AuthQuery, RequestAuth, ServerState, BOOL_CONFIG_FIELDS,
        NUMERIC_CONFIG_FIELDS,
    };
    use crate::config::AppConfig;
    use axum::http::StatusCode;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, RwLock};

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

    #[test]
    fn preflight_response_includes_startup_and_runtime_gate() {
        let startup = json!({
            "mode": "live",
            "status": "passed"
        });
        let latest_fast = Some(json!({
            "_kind": "fast",
            "snapshot": {
                "running": false,
                "live_start_gate": {
                    "ready": true,
                    "errors": []
                }
            }
        }));

        let response = build_preflight_response(startup.clone(), latest_fast);

        assert_eq!(response["startup"], startup);
        assert_eq!(response["runtime_gate"]["ready"], json!(true));
        assert_eq!(response["running"], json!(false));
    }

    fn test_state(
        config: AppConfig,
        startup: serde_json::Value,
        latest_fast: Option<serde_json::Value>,
    ) -> Arc<ServerState> {
        let (tx, _) = broadcast::channel(4);
        let (cmd_tx, _) = mpsc::channel(4);
        Arc::new(ServerState {
            tx,
            cmd_tx,
            startup_preflight: Arc::new(RwLock::new(startup)),
            latest_fast: Arc::new(RwLock::new(latest_fast)),
            config,
        })
    }

    #[test]
    fn auth_rejects_missing_token_when_configured() {
        let mut config = AppConfig::load().expect("config should load");
        config.dashboard_auth_token = Some("secret".to_string());
        let state = test_state(config, json!({"mode":"paper"}), None);

        let result = authorize_dashboard_request(
            &state,
            &RequestAuth::from_parts(&axum::http::HeaderMap::new(), AuthQuery::default()),
        );

        assert_eq!(result, Err(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn live_reset_command_is_rejected() {
        let config = AppConfig::load().expect("config should load");
        let state = test_state(
            config,
            json!({"mode":"live"}),
            Some(json!({
                "_kind": "fast",
                "snapshot": {
                    "is_live": true,
                    "running": false
                }
            })),
        );

        let result = validate_command(&state, &json!({ "_type": "reset" })).await;

        assert_eq!(
            result,
            Err((
                StatusCode::FORBIDDEN,
                "reset is disabled in live mode".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn invalid_settings_values_are_rejected() {
        let config = AppConfig::load().expect("config should load");
        let state = test_state(config, json!({"mode":"paper"}), None);

        let result = validate_settings_update(
            &state,
            &json!({
                "portfolio_pct": 1.5
            }),
        )
        .await;

        assert!(matches!(result, Err((StatusCode::BAD_REQUEST, _))));
    }

    #[tokio::test]
    async fn startup_only_settings_are_rejected() {
        let config = AppConfig::load().expect("config should load");
        let state = test_state(config, json!({"mode":"paper"}), None);

        let result = validate_settings_update(
            &state,
            &json!({
                "paper_db_path": "other.db"
            }),
        )
        .await;

        assert!(matches!(
            result,
            Err((StatusCode::BAD_REQUEST, ref message))
                if message == "`paper_db_path` is startup-only and cannot be changed via /settings"
        ));
    }
}

async fn send_command(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
    Json(payload): Json<serde_json::Value>,
) -> axum::response::Response {
    if let Err(status) =
        authorize_dashboard_request(&state, &RequestAuth::from_parts(&headers, query))
    {
        return status.into_response();
    }

    if let Err((status, message)) = validate_command(&state, &payload).await {
        return (status, Json(serde_json::json!({ "error": message }))).into_response();
    }

    info!(cmd = ?payload, "Received command from dashboard");
    let _ = state.cmd_tx.send(payload).await;
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize_dashboard_request(&state, &RequestAuth::from_parts(&headers, query))?;
    Ok(ws.on_upgrade(|socket| handle_socket(socket, state, None)))
}

async fn ws_fast_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize_dashboard_request(&state, &RequestAuth::from_parts(&headers, query))?;
    Ok(ws.on_upgrade(|socket| handle_socket(socket, state, Some("fast"))))
}

async fn ws_slow_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    authorize_dashboard_request(&state, &RequestAuth::from_parts(&headers, query))?;
    Ok(ws.on_upgrade(|socket| handle_socket(socket, state, Some("slow"))))
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<ServerState>,
    kind_filter: Option<&'static str>,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    debug!("New dashboard connection established");

    loop {
        tokio::select! {
            // Forward broadcast messages from the engine to the dashboard
            broadcast_msg = rx.recv() => {
                match broadcast_msg {
                    Ok(msg) => {
                        if let Some(kind) = kind_filter {
                            let expected = format!("\"_kind\":\"{kind}\"");
                            if !msg.contains(&expected) {
                                continue;
                            }
                        }
                        debug!("Sending state to dashboard ({} bytes)", msg.len());
                        if sender.send(Message::Text(msg.into())).await.is_err() {
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

fn authorize_dashboard_request(state: &ServerState, auth: &RequestAuth) -> Result<(), StatusCode> {
    match &state.config.dashboard_auth_token {
        Some(expected) if auth.token.as_deref() != Some(expected.as_str()) => {
            Err(StatusCode::UNAUTHORIZED)
        }
        _ => Ok(()),
    }
}

async fn validate_command(
    state: &ServerState,
    payload: &serde_json::Value,
) -> std::result::Result<(), (StatusCode, String)> {
    let is_live = is_live_mode(state).await;
    match payload.get("_type").and_then(|value| value.as_str()) {
        Some("reset") if is_live => Err((
            StatusCode::FORBIDDEN,
            "reset is disabled in live mode".to_string(),
        )),
        Some("close_position")
            if payload
                .get("position_id")
                .and_then(|v| v.as_str())
                .is_none() =>
        {
            Err((
                StatusCode::BAD_REQUEST,
                "close_position requires position_id".to_string(),
            ))
        }
        _ => Ok(()),
    }
}

async fn validate_settings_update(
    state: &ServerState,
    payload: &serde_json::Value,
) -> std::result::Result<AppConfig, (StatusCode, String)> {
    if let Some(key) = payload.as_object().and_then(|map| {
        map.keys()
            .find(|key| STARTUP_ONLY_CONFIG_FIELDS.contains(&key.as_str()))
    }) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("`{key}` is startup-only and cannot be changed via /settings"),
        ));
    }

    let mut config = AppConfig::load().map_err(|err| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load config: {err}"),
        )
    })?;
    config
        .update_from_json(payload.clone())
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    config
        .validate_runtime_settings()
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

    if is_live_mode(state).await && is_running(state).await {
        return Err((
            StatusCode::FORBIDDEN,
            "settings updates are disabled while live bot is running".to_string(),
        ));
    }

    Ok(config)
}

async fn is_live_mode(state: &ServerState) -> bool {
    if state
        .startup_preflight
        .read()
        .await
        .get("mode")
        .and_then(|value| value.as_str())
        == Some("live")
    {
        return true;
    }

    state
        .latest_fast
        .read()
        .await
        .as_ref()
        .and_then(|value| value.get("snapshot"))
        .and_then(|snapshot| snapshot.get("is_live"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

async fn is_running(state: &ServerState) -> bool {
    state
        .latest_fast
        .read()
        .await
        .as_ref()
        .and_then(|value| value.get("snapshot"))
        .and_then(|snapshot| snapshot.get("running"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}
