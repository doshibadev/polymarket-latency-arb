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
        .route("/settings", post(update_settings))
        .fallback_service(ServeDir::new("."))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    info!("Dashboard server running on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn update_settings(
    axum::extract::State(state): axum::extract::State<Arc<ServerState>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!(settings = ?payload, "Received settings update from dashboard");
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
