use thiserror::Error;

#[derive(Error, Debug)]
pub enum ArbError {
    #[error("WebSocket error: {0}")]
    WebSocket(#[from] tungstenite::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Price data error: {0}")]
    PriceData(String),
}

pub type Result<T> = std::result::Result<T, ArbError>;
