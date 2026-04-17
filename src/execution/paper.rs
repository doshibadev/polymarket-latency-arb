use crate::config::AppConfig;
use crate::polymarket::BookLevel;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteRow, SqliteSynchronous,
};
use sqlx::{Pool, Row, Sqlite};
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// An open scalp position
#[derive(Clone, Serialize)]
pub struct OpenPosition {
    pub symbol: String,
    pub direction: String,
    pub entry_price: f64,
    pub avg_entry_price: f64, // Weighted average when scaling in
    pub shares: f64,
    pub position_size: f64,
    pub buy_fee: f64,
    pub entry_spike: f64,
    #[serde(skip)]
    pub entry_time: std::time::Instant,
    pub highest_price: f64,
    pub scale_level: u32,
    pub hold_to_resolution: bool,
    pub peak_spike: f64, // highest spike seen since entry
    // BTC-based exit fields
    pub entry_btc: f64,  // BTC price at entry
    pub peak_btc: f64,   // highest BTC since entry (for UP positions)
    pub trough_btc: f64, // lowest BTC since entry (for DOWN positions)
    #[serde(skip)]
    pub spike_faded_since: Option<Instant>, // when spike_faded reversal first detected
    #[serde(skip)]
    pub trend_reversed_since: Option<Instant>, // when trend_reversed first detected
    pub trailing_stop_activated: bool, // true once share price gains activation_pct% from entry
    pub on_chain_shares: Option<f64>, // actual on-chain balance from sync_from_clob (live only)
    pub ptb_tier_at_entry: Option<String>,
    pub entry_mode: Option<String>,
    pub exit_suppressed_count: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TradeRecord {
    pub symbol: String,
    pub r#type: String,
    pub question: String,
    pub direction: String,
    pub entry_price: Option<f64>,
    pub exit_price: Option<f64>,
    pub shares: f64,
    pub cost: f64,
    pub pnl: Option<f64>,
    pub cumulative_pnl: Option<f64>,
    pub balance_after: Option<f64>,
    pub timestamp: String,
    pub close_reason: Option<String>,
    #[serde(default)]
    pub btc_at_entry: Option<f64>,
    #[serde(default)]
    pub price_to_beat_at_entry: Option<f64>,
    #[serde(default)]
    pub ptb_margin_at_entry: Option<f64>,
    #[serde(default)]
    pub seconds_to_expiry_at_entry: Option<u64>,
    #[serde(default)]
    pub spread_at_entry: Option<f64>,
    #[serde(default)]
    pub round_trip_loss_pct_at_entry: Option<f64>,
    #[serde(default)]
    pub signal_score: Option<f64>,
    #[serde(default)]
    pub ptb_margin_at_exit: Option<f64>,
    #[serde(default)]
    pub exit_mode: Option<String>,
    #[serde(default)]
    pub favorable_ptb_at_exit: Option<bool>,
    #[serde(default)]
    pub ptb_tier_at_entry: Option<String>,
    #[serde(default)]
    pub ptb_tier_at_exit: Option<String>,
    #[serde(default)]
    pub entry_mode: Option<String>,
    #[serde(default)]
    pub exit_suppressed_count: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HistoryPoint {
    pub t: String, // timestamp
    pub v: f64,    // portfolio value
}

#[derive(Clone)]
struct SymbolMarketState {
    pub up_bid: f64,
    pub up_ask: f64,
    pub down_bid: f64,
    pub down_ask: f64,
    pub up_bids: Vec<BookLevel>,
    pub up_asks: Vec<BookLevel>,
    pub down_bids: Vec<BookLevel>,
    pub down_asks: Vec<BookLevel>,
    pub question: String,
    pub last_binance: f64,
    pub last_chainlink: f64,
    pub spike_history: [f64; 16], // fixed-size ring buffer for smoothed spike
    pub spike_history_len: usize, // how many valid entries
    pub spike_history_idx: usize, // next write position
    pub btc_price_history: [(f64, Instant); 64], // (price, time) for long-baseline spike calc
    pub btc_history_len: usize,
    pub btc_history_idx: usize,
    pub last_price_to_beat: Option<f64>, // price_to_beat for hold mode
    pub last_market_end_ts: Option<u64>, // market end timestamp for hold mode
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PtbTier {
    Unknown,
    Unfavorable,
    Neutral,
    Favorable,
    Strong,
    Extreme,
}

impl PtbTier {
    fn from_margin(margin: Option<f64>) -> Self {
        match margin {
            Some(m) if m >= 150.0 => Self::Extreme,
            Some(m) if m >= 100.0 => Self::Strong,
            Some(m) if m >= 30.0 => Self::Favorable,
            Some(m) if m >= -20.0 => Self::Neutral,
            Some(_) => Self::Unfavorable,
            None => Self::Unknown,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unfavorable => "unfavorable",
            Self::Neutral => "neutral",
            Self::Favorable => "favorable",
            Self::Strong => "strong",
            Self::Extreme => "extreme",
        }
    }

    fn is_favorable(self) -> bool {
        matches!(self, Self::Favorable | Self::Strong | Self::Extreme)
    }

    fn is_ptb_hold(self) -> bool {
        matches!(self, Self::Strong | Self::Extreme)
    }
}

struct EntryContext {
    score: f64,
    round_trip_loss_pct: f64,
    spread: f64,
    ptb_margin: Option<f64>,
    seconds_to_expiry: Option<u64>,
    ptb_tier: PtbTier,
    entry_mode: &'static str,
}

impl Default for SymbolMarketState {
    fn default() -> Self {
        Self {
            up_bid: 0.0,
            up_ask: 0.0,
            down_bid: 0.0,
            down_ask: 0.0,
            up_bids: Vec::new(),
            up_asks: Vec::new(),
            down_bids: Vec::new(),
            down_asks: Vec::new(),
            question: String::new(),
            last_binance: 0.0,
            last_chainlink: 0.0,
            spike_history: [0.0; 16],
            spike_history_len: 0,
            spike_history_idx: 0,
            btc_price_history: [(0.0, Instant::now()); 64],
            btc_history_len: 0,
            btc_history_idx: 0,
            last_price_to_beat: None,
            last_market_end_ts: None,
        }
    }
}

/// A pending close waiting for execution delay to elapse
#[derive(Clone)]
pub struct PendingClose {
    pub idx_at_submit: usize,
    pub symbol: String,
    pub direction: String,
    pub close_price: f64,
    pub reason: &'static str,
    pub submitted_at: Instant,
}

/// A pending entry waiting for execution delay to elapse
#[derive(Clone)]
pub struct PendingEntry {
    pub symbol: String,
    pub direction: String,
    pub spike: f64,
    pub entry_price: f64,
    pub scale_level: u32,
    pub position_size: f64,
    pub shares: f64,
    pub buy_fee: f64,
    pub submitted_at: Instant,
    pub entry_btc: f64,    // BTC price at entry to avoid race condition
    pub live_synced: bool, // true if synced from live fill — skip recalculation in flush_pending
    pub price_to_beat_at_entry: Option<f64>,
    pub ptb_margin_at_entry: Option<f64>,
    pub seconds_to_expiry_at_entry: Option<u64>,
    pub spread_at_entry: Option<f64>,
    pub round_trip_loss_pct_at_entry: Option<f64>,
    pub signal_score: Option<f64>,
    pub ptb_tier_at_entry: Option<String>,
    pub entry_mode: Option<String>,
}

#[derive(Clone)]
pub struct WalletEvent {
    pub symbol: String,
    pub direction: String,
    pub spike: f64,
    pub status: String,
    pub reason: String,
}

pub struct PaperWallet {
    pub balance: f64,
    pub starting_balance: f64,
    pub trade_count: u64,
    pub wins: u64,
    pub losses: u64,
    pub total_fees_paid: f64,
    pub total_volume: f64,
    pub portfolio_pct: f64,
    pub open_positions: Vec<OpenPosition>,
    pub pending_entries: Vec<PendingEntry>,
    pub pending_closes: Vec<PendingClose>,
    pub trade_history: Vec<TradeRecord>,
    pub history: VecDeque<HistoryPoint>, // Chart performance data
    pub events: Vec<WalletEvent>,
    symbol_states: HashMap<String, SymbolMarketState>,
    config: AppConfig,
    db: Option<Pool<Sqlite>>,
    db_writer: Option<mpsc::Sender<DbWriteCommand>>,
    saved_trade_count: usize,
    saved_history_count: usize,
}

#[derive(Clone)]
struct WalletStateSnapshot {
    balance: f64,
    starting_balance: f64,
    trade_count: u64,
    wins: u64,
    losses: u64,
    total_fees_paid: f64,
    total_volume: f64,
}

enum DbWriteCommand {
    Save {
        wallet: WalletStateSnapshot,
        trades: Vec<TradeRecord>,
        history: Vec<HistoryPoint>,
    },
    Reset,
}

impl PaperWallet {
    const DB_URL: &'static str = "sqlite://lattice.db";

    pub fn new(config: AppConfig) -> Self {
        Self {
            balance: config.starting_balance,
            starting_balance: config.starting_balance,
            trade_count: 0,
            wins: 0,
            losses: 0,
            total_fees_paid: 0.0,
            total_volume: 0.0,
            portfolio_pct: config.portfolio_pct,
            open_positions: Vec::new(),
            pending_entries: Vec::new(),
            pending_closes: Vec::new(),
            trade_history: Vec::new(),
            history: VecDeque::new(),
            events: Vec::new(),
            symbol_states: HashMap::new(),
            config,
            db: None,
            db_writer: None,
            saved_trade_count: 0,
            saved_history_count: 0,
        }
    }

    async fn db(&mut self) -> std::result::Result<Pool<Sqlite>, sqlx::Error> {
        if let Some(db) = &self.db {
            return Ok(db.clone());
        }

        let options = SqliteConnectOptions::from_str(Self::DB_URL)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let db = SqlitePool::connect_with(options).await?;
        Self::migrate(&db).await?;
        self.db = Some(db.clone());
        self.ensure_db_writer(&db);
        Ok(db)
    }

    fn ensure_db_writer(&mut self, db: &Pool<Sqlite>) {
        if self.db_writer.is_some() {
            return;
        }

        let (tx, mut rx) = mpsc::channel(32);
        let db = db.clone();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    DbWriteCommand::Save {
                        wallet,
                        trades,
                        history,
                    } => {
                        let mut tx = match db.begin().await {
                            Ok(tx) => tx,
                            Err(err) => {
                                warn!("Failed to start paper wallet save transaction: {err}");
                                continue;
                            }
                        };

                        let save_result = async {
                            sqlx::query(
                                r#"
                                INSERT INTO paper_wallet_state (
                                    id, balance, starting_balance, trade_count, wins, losses,
                                    total_fees_paid, total_volume, updated_at
                                )
                                VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?)
                                ON CONFLICT(id) DO UPDATE SET
                                    balance = excluded.balance,
                                    starting_balance = excluded.starting_balance,
                                    trade_count = excluded.trade_count,
                                    wins = excluded.wins,
                                    losses = excluded.losses,
                                    total_fees_paid = excluded.total_fees_paid,
                                    total_volume = excluded.total_volume,
                                    updated_at = excluded.updated_at
                                "#,
                            )
                            .bind(wallet.balance)
                            .bind(wallet.starting_balance)
                            .bind(wallet.trade_count as i64)
                            .bind(wallet.wins as i64)
                            .bind(wallet.losses as i64)
                            .bind(wallet.total_fees_paid)
                            .bind(wallet.total_volume)
                            .bind(chrono::Local::now().to_rfc3339())
                            .execute(&mut *tx)
                            .await?;

                            for trade in &trades {
                                sqlx::query(
                                    r#"
                                    INSERT INTO paper_trades (
                                        symbol, type, question, direction, entry_price, exit_price, shares, cost,
                                        pnl, cumulative_pnl, balance_after, timestamp, close_reason, btc_at_entry,
                                        price_to_beat_at_entry, ptb_margin_at_entry, seconds_to_expiry_at_entry,
                                        spread_at_entry, round_trip_loss_pct_at_entry, signal_score,
                                        ptb_margin_at_exit, exit_mode, favorable_ptb_at_exit, ptb_tier_at_entry,
                                        ptb_tier_at_exit, entry_mode, exit_suppressed_count
                                    )
                                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                                    "#,
                                )
                                .bind(&trade.symbol)
                                .bind(&trade.r#type)
                                .bind(&trade.question)
                                .bind(&trade.direction)
                                .bind(trade.entry_price)
                                .bind(trade.exit_price)
                                .bind(trade.shares)
                                .bind(trade.cost)
                                .bind(trade.pnl)
                                .bind(trade.cumulative_pnl)
                                .bind(trade.balance_after)
                                .bind(&trade.timestamp)
                                .bind(&trade.close_reason)
                                .bind(trade.btc_at_entry)
                                .bind(trade.price_to_beat_at_entry)
                                .bind(trade.ptb_margin_at_entry)
                                .bind(trade.seconds_to_expiry_at_entry.map(|value| value as i64))
                                .bind(trade.spread_at_entry)
                                .bind(trade.round_trip_loss_pct_at_entry)
                                .bind(trade.signal_score)
                                .bind(trade.ptb_margin_at_exit)
                                .bind(&trade.exit_mode)
                                .bind(trade.favorable_ptb_at_exit.map(i64::from))
                                .bind(&trade.ptb_tier_at_entry)
                                .bind(&trade.ptb_tier_at_exit)
                                .bind(&trade.entry_mode)
                                .bind(trade.exit_suppressed_count.map(|value| value as i64))
                                .execute(&mut *tx)
                                .await?;
                            }

                            for point in &history {
                                sqlx::query("INSERT INTO paper_equity_history (t, v) VALUES (?, ?)")
                                    .bind(&point.t)
                                    .bind(point.v)
                                    .execute(&mut *tx)
                                    .await?;
                            }

                            std::result::Result::<(), sqlx::Error>::Ok(())
                        }
                        .await;

                        match save_result {
                            Ok(()) => {
                                if let Err(err) = tx.commit().await {
                                    warn!("Failed to commit paper wallet state: {err}");
                                }
                            }
                            Err(err) => {
                                warn!("Failed to save paper wallet state: {err}");
                                let _ = tx.rollback().await;
                            }
                        }
                    }
                    DbWriteCommand::Reset => {
                        let result = async {
                            sqlx::query("DELETE FROM paper_trades").execute(&db).await?;
                            sqlx::query("DELETE FROM paper_equity_history")
                                .execute(&db)
                                .await?;
                            sqlx::query("DELETE FROM paper_wallet_state")
                                .execute(&db)
                                .await?;
                            std::result::Result::<(), sqlx::Error>::Ok(())
                        }
                        .await;

                        if let Err(err) = result {
                            warn!("Failed to clear paper wallet database: {err}");
                        }
                    }
                }
            }
        });
        self.db_writer = Some(tx);
    }

    async fn migrate(db: &Pool<Sqlite>) -> std::result::Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS paper_wallet_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                balance REAL NOT NULL,
                starting_balance REAL NOT NULL,
                trade_count INTEGER NOT NULL,
                wins INTEGER NOT NULL,
                losses INTEGER NOT NULL,
                total_fees_paid REAL NOT NULL,
                total_volume REAL NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(db)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS paper_trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol TEXT NOT NULL,
                type TEXT NOT NULL,
                question TEXT NOT NULL,
                direction TEXT NOT NULL,
                entry_price REAL,
                exit_price REAL,
                shares REAL NOT NULL,
                cost REAL NOT NULL,
                pnl REAL,
                cumulative_pnl REAL,
                balance_after REAL,
                timestamp TEXT NOT NULL,
                close_reason TEXT,
                btc_at_entry REAL,
                price_to_beat_at_entry REAL,
                ptb_margin_at_entry REAL,
                seconds_to_expiry_at_entry INTEGER,
                spread_at_entry REAL,
                round_trip_loss_pct_at_entry REAL,
                signal_score REAL,
                ptb_margin_at_exit REAL,
                exit_mode TEXT,
                favorable_ptb_at_exit INTEGER,
                ptb_tier_at_entry TEXT,
                ptb_tier_at_exit TEXT,
                entry_mode TEXT,
                exit_suppressed_count INTEGER
            )
            "#,
        )
        .execute(db)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS paper_equity_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                t TEXT NOT NULL,
                v REAL NOT NULL
            )
            "#,
        )
        .execute(db)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_paper_trades_timestamp ON paper_trades(timestamp)",
        )
        .execute(db)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_paper_equity_history_id ON paper_equity_history(id)",
        )
        .execute(db)
        .await?;

        Ok(())
    }

    fn trade_from_row(row: &SqliteRow) -> TradeRecord {
        TradeRecord {
            symbol: row.get("symbol"),
            r#type: row.get("type"),
            question: row.get("question"),
            direction: row.get("direction"),
            entry_price: row.get("entry_price"),
            exit_price: row.get("exit_price"),
            shares: row.get("shares"),
            cost: row.get("cost"),
            pnl: row.get("pnl"),
            cumulative_pnl: row.get("cumulative_pnl"),
            balance_after: row.get("balance_after"),
            timestamp: row.get("timestamp"),
            close_reason: row.get("close_reason"),
            btc_at_entry: row.get("btc_at_entry"),
            price_to_beat_at_entry: row.get("price_to_beat_at_entry"),
            ptb_margin_at_entry: row.get("ptb_margin_at_entry"),
            seconds_to_expiry_at_entry: row
                .get::<Option<i64>, _>("seconds_to_expiry_at_entry")
                .map(|value| value as u64),
            spread_at_entry: row.get("spread_at_entry"),
            round_trip_loss_pct_at_entry: row.get("round_trip_loss_pct_at_entry"),
            signal_score: row.get("signal_score"),
            ptb_margin_at_exit: row.get("ptb_margin_at_exit"),
            exit_mode: row.get("exit_mode"),
            favorable_ptb_at_exit: row
                .get::<Option<i64>, _>("favorable_ptb_at_exit")
                .map(|value| value != 0),
            ptb_tier_at_entry: row.get("ptb_tier_at_entry"),
            ptb_tier_at_exit: row.get("ptb_tier_at_exit"),
            entry_mode: row.get("entry_mode"),
            exit_suppressed_count: row
                .get::<Option<i64>, _>("exit_suppressed_count")
                .map(|value| value as u32),
        }
    }

    /// Load saved state from SQLite (only for paper trading)
    pub async fn load_state(&mut self) {
        if !self.config.paper_trading {
            return; // Never load state for live trading
        }

        let db = match self.db().await {
            Ok(db) => db,
            Err(err) => {
                warn!("Failed to open paper wallet database: {err}");
                return;
            }
        };

        let wallet_row = match sqlx::query(
            r#"
            SELECT balance, trade_count, wins, losses, total_fees_paid, total_volume
            FROM paper_wallet_state
            WHERE id = 1
            "#,
        )
        .fetch_optional(&db)
        .await
        {
            Ok(row) => row,
            Err(err) => {
                warn!("Failed to load paper wallet state: {err}");
                return;
            }
        };

        let Some(wallet_row) = wallet_row else {
            return;
        };

        self.balance = wallet_row.get("balance");
        // Always use current .env STARTING_BALANCE, not saved value.
        self.starting_balance = self.config.starting_balance;
        self.trade_count = wallet_row.get::<i64, _>("trade_count") as u64;
        self.wins = wallet_row.get::<i64, _>("wins") as u64;
        self.losses = wallet_row.get::<i64, _>("losses") as u64;
        self.total_fees_paid = wallet_row.get("total_fees_paid");
        self.total_volume = wallet_row.get("total_volume");

        match sqlx::query("SELECT * FROM paper_trades ORDER BY id ASC")
            .fetch_all(&db)
            .await
        {
            Ok(rows) => {
                self.trade_history = rows.iter().map(Self::trade_from_row).collect();
                self.saved_trade_count = self.trade_history.len();
            }
            Err(err) => warn!("Failed to load paper trade history: {err}"),
        }

        match sqlx::query(
            "SELECT t, v FROM paper_equity_history ORDER BY id DESC LIMIT 1000",
        )
            .fetch_all(&db)
            .await
        {
            Ok(rows) => {
                self.history = rows
                    .iter()
                    .rev()
                    .map(|row| HistoryPoint {
                        t: row.get("t"),
                        v: row.get("v"),
                    })
                    .collect();
                self.saved_history_count = self.history.len();
            }
            Err(err) => warn!("Failed to load paper equity history: {err}"),
        }

        info!(
            "Loaded paper wallet state from SQLite ({} trades, {} history points)",
            self.trade_history.len(),
            self.history.len()
        );
    }

    /// Save state to SQLite (only for paper trading)
    pub async fn save_state(&mut self) {
        if !self.config.paper_trading {
            return; // Never save state for live trading
        }

        match self.db().await {
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to open paper wallet database for save: {err}");
                return;
            }
        }

        let wallet = WalletStateSnapshot {
            balance: self.balance,
            starting_balance: self.starting_balance,
            trade_count: self.trade_count,
            wins: self.wins,
            losses: self.losses,
            total_fees_paid: self.total_fees_paid,
            total_volume: self.total_volume,
        };
        let new_trades = self.trade_history[self.saved_trade_count..].to_vec();
        let new_history = self.history.iter().skip(self.saved_history_count).cloned().collect();

        if let Some(writer) = &self.db_writer {
            match writer.try_send(DbWriteCommand::Save {
                wallet,
                trades: new_trades.clone(),
                history: new_history,
            }) {
                Ok(()) => {
                    self.saved_trade_count = self.trade_history.len();
                    self.saved_history_count = self.history.len();
                }
                Err(mpsc::error::TrySendError::Full(_)) if new_trades.is_empty() => {
                    self.saved_history_count = self.history.len();
                    warn!("Paper DB writer backed up, dropped nonessential equity history points");
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("Paper DB writer backed up, retaining unsaved trades for retry");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("Failed to queue paper wallet save: DB writer closed");
                }
            }
        }
    }

    /// Reset paper wallet to initial state
    pub async fn reset(&mut self) {
        self.balance = self.config.starting_balance;
        self.starting_balance = self.config.starting_balance;
        self.trade_count = 0;
        self.wins = 0;
        self.losses = 0;
        self.total_fees_paid = 0.0;
        self.total_volume = 0.0;
        self.open_positions.clear();
        self.pending_entries.clear();
        self.pending_closes.clear();
        self.trade_history.clear();
        self.history.clear(); // Clear chart data
        self.events.clear();
        self.symbol_states.clear();
        self.saved_trade_count = 0;
        self.saved_history_count = 0;

        if self.config.paper_trading {
            match self.db().await {
                Ok(_) => {
                    if let Some(writer) = &self.db_writer {
                        if let Err(err) = writer.send(DbWriteCommand::Reset).await {
                            warn!("Failed to queue paper wallet reset: {err}");
                        }
                    }
                }
                Err(err) => warn!("Failed to open paper wallet database for reset: {err}"),
            }
        }

        info!("Paper wallet reset to initial state");
    }

    pub fn drain_events(&mut self) -> Vec<WalletEvent> {
        self.events.drain(..).collect()
    }

    pub fn update_config(&mut self, config: AppConfig) {
        self.config = config.clone();
        self.portfolio_pct = config.portfolio_pct;
    }

    /// Add a point to the performance history chart
    pub fn push_history(&mut self, value: f64) {
        let now = chrono::Local::now().format("%H:%M:%S").to_string();
        self.history.push_back(HistoryPoint { t: now, v: value });
        // Keep last 1000 points (about 3 minutes at 200ms intervals)
        if self.history.len() > 1000 {
            self.history.pop_front();
        }
    }

    pub fn set_market_info(&mut self, symbol: &str, question: String) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.question = question;
    }

    pub fn set_market_metadata(
        &mut self,
        symbol: &str,
        price_to_beat: Option<f64>,
        market_end_ts: Option<u64>,
    ) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.last_price_to_beat = price_to_beat;
        state.last_market_end_ts = market_end_ts;
    }

    pub fn reset_prices(&mut self, symbol: &str) {
        if let Some(state) = self.symbol_states.get_mut(symbol) {
            state.up_bid = 0.0;
            state.up_ask = 0.0;
            state.down_bid = 0.0;
            state.down_ask = 0.0;
            state.up_bids.clear();
            state.up_asks.clear();
            state.down_bids.clear();
            state.down_asks.clear();
        }
    }

    pub fn update_share_price(
        &mut self,
        symbol: &str,
        direction: &str,
        bid: f64,
        ask: f64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
    ) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        let mid_price = (bid + ask) / 2.0;
        if direction == "UP" {
            state.up_bid = bid;
            state.up_ask = ask;
            if !bids.is_empty() {
                state.up_bids = bids;
            }
            if !asks.is_empty() {
                state.up_asks = asks;
            }
        } else {
            state.down_bid = bid;
            state.down_ask = ask;
            if !bids.is_empty() {
                state.down_bids = bids;
            }
            if !asks.is_empty() {
                state.down_asks = asks;
            }
        }

        // Update best price for all positions of this symbol/direction
        for pos in &mut self.open_positions {
            if pos.symbol == symbol && pos.direction == direction {
                if mid_price > pos.highest_price {
                    pos.highest_price = mid_price;
                }
                // We are long shares for both UP and DOWN, so profit is a higher share price.
                if !pos.trailing_stop_activated && self.config.trailing_stop_activation > 0.0 {
                    let threshold =
                        pos.entry_price * (1.0 + self.config.trailing_stop_activation / 100.0);
                    if pos.highest_price >= threshold {
                        pos.trailing_stop_activated = true;
                        info!(symbol=%symbol, direction=%direction, entry=pos.entry_price, peak=pos.highest_price,
                              "Trailing stop ACTIVATED (share price gained {}%+)", self.config.trailing_stop_activation);
                    }
                }
            }
        }
    }

    pub fn update_btc_prices(&mut self, symbol: &str, binance: f64, chainlink: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.last_binance = binance;
        state.last_chainlink = chainlink;
        // spike_history is updated by engine via update_spike_momentum
    }

    /// Push BTC price for long-baseline spike calculation (called every tick)
    pub fn push_btc_price(&mut self, symbol: &str, btc_price: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.btc_price_history[state.btc_history_idx] = (btc_price, Instant::now());
        state.btc_history_idx = (state.btc_history_idx + 1) % 64;
        if state.btc_history_len < 64 {
            state.btc_history_len += 1;
        }
    }

    /// Update BTC trailing stop tracking for all open positions of a symbol
    /// Called on every BTC price update from the engine
    pub fn update_btc_trailing(&mut self, symbol: &str, current_btc: f64) {
        for pos in &mut self.open_positions {
            if pos.symbol != symbol {
                continue;
            }

            // entry_btc is now set immediately when position is created
            // Just update peak/trough here
            if current_btc > pos.peak_btc {
                pos.peak_btc = current_btc;
            }
            if current_btc < pos.trough_btc {
                pos.trough_btc = current_btc;
            }
        }
    }

    /// Push the current momentum value into ring buffer for exit smoothing
    pub fn push_spike_momentum(&mut self, symbol: &str, momentum: f64) {
        let state = self.symbol_states.entry(symbol.to_string()).or_default();
        state.spike_history[state.spike_history_idx] = momentum;
        state.spike_history_idx = (state.spike_history_idx + 1) % 16;
        if state.spike_history_len < 16 {
            state.spike_history_len += 1;
        }
    }

    /// Called by engine on each Binance tick to evaluate hold-to-resolution for open positions
    pub fn update_hold_status(
        &mut self,
        symbol: &str,
        btc_history: &VecDeque<(f64, Instant)>,
        current_chainlink: f64,
        price_to_beat: Option<f64>,
        end_ts: Option<u64>,
        hold_margin_per_second: f64,
        hold_max_seconds: u64,
        hold_max_crossings: usize,
    ) {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let time_remaining = match end_ts {
            Some(e) if e > now_secs => e - now_secs,
            _ => return,
        };

        let ptb = match price_to_beat {
            Some(p) => p,
            None => return,
        };
        // Use Chainlink price for margin — market resolves against Chainlink
        let current_btc = if current_chainlink > 0.0 {
            current_chainlink
        } else {
            match btc_history.back() {
                Some((p, _)) => *p,
                None => return,
            }
        };

        for pos in &mut self.open_positions {
            if pos.symbol != symbol {
                continue;
            }

            let margin = if pos.direction == "UP" {
                current_btc - ptb
            } else {
                ptb - current_btc
            };

            // If BTC has crossed price_to_beat → close immediately, don't hold
            if margin <= 0.0 {
                pos.hold_to_resolution = false;
                continue;
            }

            // Only consider holding in the last hold_max_seconds
            if time_remaining > hold_max_seconds {
                pos.hold_to_resolution = false;
                continue;
            }

            // Count crossings in btc_history
            let mut crossings = 0usize;
            let mut iter = btc_history.iter().map(|(price, _)| *price);
            if let Some(mut previous) = iter.next() {
                for current in iter {
                    let was_above = previous > ptb;
                    let is_above = current > ptb;
                    if was_above != is_above {
                        crossings += 1;
                    }
                    previous = current;
                }
            }

            if crossings > hold_max_crossings {
                pos.hold_to_resolution = false;
                continue;
            }

            // Check trend: BTC must be consistently on correct side AND accelerating away from ptb
            let trend_ok = if btc_history.len() >= 10 {
                let recent: Vec<f64> = btc_history
                    .iter()
                    .rev()
                    .take(10)
                    .map(|(price, _)| *price)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                // All recent ticks must be on the correct side of price-to-beat
                let all_correct_side = if pos.direction == "UP" {
                    recent.iter().all(|p| *p > ptb)
                } else {
                    recent.iter().all(|p| *p < ptb)
                };
                // Trend must be moving away from price-to-beat (not just sideways)
                let slope = recent.last().unwrap() - recent.first().unwrap();
                let moving_away = if pos.direction == "UP" {
                    slope > 0.0
                } else {
                    slope < 0.0
                };
                all_correct_side && moving_away
            } else {
                false
            }; // not enough history = don't hold

            // Required margin check — stricter: 2x the normal requirement for high confidence
            let required_margin = hold_margin_per_second * time_remaining as f64;
            pos.hold_to_resolution = margin >= required_margin && trend_ok && crossings == 0;
        }
    }

    pub fn calculate_fee(&self, shares: f64, price: f64) -> f64 {
        let fee = shares * self.config.crypto_fee_rate * price * (1.0 - price);
        if fee < 0.00001 {
            0.0
        } else {
            (fee * 100000.0).round() / 100000.0
        }
    }

    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(state) = self.symbol_states.get(symbol) {
            if direction == "UP" {
                (state.up_bid, state.up_ask)
            } else {
                (state.down_bid, state.down_ask)
            }
        } else {
            (0.0, 0.0)
        }
    }

    fn get_book(&self, symbol: &str, direction: &str) -> Option<(&[BookLevel], &[BookLevel])> {
        self.symbol_states
            .get(symbol)
            .map(|state| {
                if direction == "UP" {
                    (state.up_bids.as_slice(), state.up_asks.as_slice())
                } else {
                    (state.down_bids.as_slice(), state.down_asks.as_slice())
                }
            })
    }

    pub fn estimate_fak_buy(
        &self,
        symbol: &str,
        direction: &str,
        usdc_budget: f64,
        limit_price: f64,
    ) -> Option<(f64, f64, f64)> {
        let asks = match self.get_book(symbol, direction) {
            Some((_bids, asks)) if !asks.is_empty() => asks,
            _ => return None,
        };
        if asks.is_empty() {
            return None;
        }
        let mut remaining = usdc_budget;
        let mut shares = 0.0;
        let mut spent = 0.0;
        for level in asks {
            if remaining <= 0.0 || level.price > limit_price {
                break;
            }
            let spend = remaining.min(level.price * level.size);
            if spend <= 0.0 {
                continue;
            }
            spent += spend;
            shares += spend / level.price;
            remaining -= spend;
        }
        if shares > 0.0 && spent > 0.0 {
            Some((spent / shares, shares, spent))
        } else {
            None
        }
    }

    pub fn estimate_fak_sell(
        &self,
        symbol: &str,
        direction: &str,
        shares_to_sell: f64,
        limit_price: f64,
    ) -> Option<(f64, f64, f64)> {
        let bids = match self.get_book(symbol, direction) {
            Some((bids, _asks)) if !bids.is_empty() => bids,
            _ => return None,
        };
        if bids.is_empty() {
            return None;
        }
        let mut remaining = shares_to_sell;
        let mut sold = 0.0;
        let mut gross = 0.0;
        for level in bids {
            if remaining <= 0.0 || level.price < limit_price {
                break;
            }
            let fill = remaining.min(level.size);
            if fill <= 0.0 {
                continue;
            }
            sold += fill;
            gross += fill * level.price;
            remaining -= fill;
        }
        if sold > 0.0 && gross > 0.0 {
            Some((gross / sold, sold, gross))
        } else {
            None
        }
    }

    pub fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        if bid > 0.0 && ask > 0.0 {
            (bid + ask) / 2.0
        } else {
            0.0
        }
    }

    fn entry_context_score(
        &self,
        symbol: &str,
        direction: &str,
        spike: f64,
        threshold_usd: f64,
        current_btc: f64,
        position_size: f64,
    ) -> std::result::Result<EntryContext, String> {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        if bid <= 0.0 || ask <= 0.0 {
            return Err("NO_POL_LIQUIDITY".to_string());
        }
        let buy_limit = (ask + 0.04).min(0.99);
        let (buy_price, buy_shares, spent_usdc) = self
            .estimate_fak_buy(symbol, direction, position_size, buy_limit)
            .ok_or_else(|| "ENTRY_NO_FAK_LIQUIDITY".to_string())?;
        let sell_limit = if bid > 0.04 {
            (bid - 0.04).max(0.01)
        } else {
            0.01
        };
        let (sell_price, sell_shares, gross_revenue) = self
            .estimate_fak_sell(symbol, direction, buy_shares, sell_limit)
            .ok_or_else(|| "EXIT_NO_FAK_LIQUIDITY".to_string())?;
        let coverage = (sell_shares / buy_shares).clamp(0.0, 1.0);
        let buy_fee = self.calculate_fee(buy_shares, buy_price);
        let sell_fee = self.calculate_fee(sell_shares, sell_price);
        let round_trip_loss = ((spent_usdc + buy_fee) - (gross_revenue - sell_fee))
            / (spent_usdc + buy_fee).max(0.01);
        if coverage < 0.80 {
            return Err(format!("EXIT_DEPTH_TOO_THIN({:.0}%)", coverage * 100.0));
        }
        if round_trip_loss > 0.16 {
            return Err(format!(
                "ROUND_TRIP_TOO_EXPENSIVE({:.1}%)",
                round_trip_loss * 100.0
            ));
        }

        let state = self.symbol_states.get(symbol).cloned().unwrap_or_default();
        let ptb_margin = state.last_price_to_beat.map(|ptb| {
            if direction == "UP" {
                current_btc - ptb
            } else {
                ptb - current_btc
            }
        });
        let seconds_to_expiry = state.last_market_end_ts.and_then(|end| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            (end > now).then_some(end - now)
        });
        let ptb_tier = PtbTier::from_margin(ptb_margin);
        let spike_multiple = spike.abs() / threshold_usd.max(0.01);

        if seconds_to_expiry.is_some_and(|s| s < 45)
            && !(ptb_tier == PtbTier::Extreme && buy_price >= 0.70)
        {
            return Err(format!(
                "ENTRY_REJECT_TIMING(late,secs={},tier={})",
                seconds_to_expiry.unwrap_or(0),
                ptb_tier.as_str()
            ));
        }
        if seconds_to_expiry.is_some_and(|s| s >= 240)
            && (!ptb_tier.is_favorable() || spike_multiple < 1.35)
        {
            return Err(format!(
                "ENTRY_REJECT_TIMING(early,secs={},tier={},spike={:.2}x)",
                seconds_to_expiry.unwrap_or(0),
                ptb_tier.as_str(),
                spike_multiple
            ));
        }
        if ptb_tier == PtbTier::Unfavorable && spike_multiple < 1.80 {
            return Err(format!(
                "ENTRY_REJECT_PTB_TIER({},spike={:.2}x)",
                ptb_tier.as_str(),
                spike_multiple
            ));
        }

        let ptb_score = ptb_margin
            .map(|m| {
                if m >= 20.0 {
                    0.9
                } else if m >= -20.0 {
                    0.35
                } else {
                    (-0.9f64).max(m / 120.0)
                }
            })
            .unwrap_or(0.0);
        let expiry_score = seconds_to_expiry
            .map(|s| {
                if s < 20 {
                    -0.8
                } else if s <= 90 {
                    0.25
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let spread = ask - bid;
        let spread_penalty = (spread / 0.08).clamp(0.0, 1.0) * 0.7;
        let spike_score = (spike.abs() / threshold_usd.max(0.01)).min(3.0);
        let liquidity_score = coverage - (round_trip_loss * 2.5);
        let score = spike_score + ptb_score + expiry_score + liquidity_score - spread_penalty;
        let entry_mode = if ptb_tier.is_ptb_hold() {
            "ptb_hold"
        } else if ptb_tier == PtbTier::Favorable {
            "breakout"
        } else {
            "scalp"
        };
        Ok(EntryContext {
            score,
            round_trip_loss_pct: round_trip_loss,
            spread,
            ptb_margin,
            seconds_to_expiry,
            ptb_tier,
            entry_mode,
        })
    }

    pub async fn try_close_position(&mut self) -> bool {
        let mut to_close = Vec::new();
        let mut suppressed_exits: Vec<(usize, String, String, f64, &'static str)> = Vec::new();

        // Calculate current time for hold mode
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // First pass: collect all data needed for decisions
        for (idx, pos) in self.open_positions.iter().enumerate() {
            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            if current_price <= 0.0 {
                continue;
            }

            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };

            let current_btc = state.last_binance;
            let held_ms = pos.entry_time.elapsed().as_millis() as u64;

            // Calculate time remaining for hold mode
            let time_remaining = state.last_market_end_ts.and_then(|end| {
                if end > now_secs {
                    Some(end - now_secs)
                } else {
                    None
                }
            });
            let ptb_margin = state.last_price_to_beat.map(|ptb| {
                if pos.direction == "UP" {
                    current_btc - ptb
                } else {
                    ptb - current_btc
                }
            });
            let ptb_tier = PtbTier::from_margin(ptb_margin);

            // Max price exit ALWAYS fires, even in hold mode — take the guaranteed profit
            if current_price >= 0.995 {
                to_close.push((idx, "max_price"));
                continue;
            }

            // Minimum hold time check - don't exit too early
            // Give Polymarket time to update their chart
            let min_hold_passed = held_ms >= self.config.min_hold_ms;

            // PTB-dynamic exit thresholds: adjust based on distance from price_to_beat
            // Positive margin = winning side, negative = losing side
            // Dead zone: -$30 to +$30 → no adjustment (normal exits)
            // Far winning (>$50): hold longer (wider thresholds, up to 1.5x)
            // Far losing (<-$30): exit faster (tighter thresholds, down to 0.6x)
            let ptb_factor = {
                let ptb_margin = if let Some(ptb) = state.last_price_to_beat {
                    if current_btc > 0.0 {
                        if pos.direction == "UP" {
                            current_btc - ptb
                        } else {
                            ptb - current_btc
                        }
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };

                if ptb_margin > 50.0 {
                    1.5 // Far winning: 50% wider → ride it out
                } else if ptb_margin > 30.0 {
                    1.0 + (ptb_margin - 30.0) / 20.0 * 0.5 // Linear 1.0→1.5
                } else if ptb_margin < -50.0 {
                    0.6 // Far losing: 40% tighter → exit fast
                } else if ptb_margin < -30.0 {
                    1.0 - ((-ptb_margin) - 30.0) / 20.0 * 0.4 // Linear 1.0→0.6
                } else {
                    1.0 // Dead zone: no adjustment
                }
            };

            // Trend reversed - exit if BTC reverses by a percentage of the entry spike from entry
            // Uses trend_reversal_pct (e.g., 50% of spike), floored at trend_reversal_threshold ($10)
            // PTB factor widens threshold when winning, tightens when losing
            let trend_reversed_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 {
                let reversal_threshold = (pos.entry_spike.abs()
                    * (self.config.trend_reversal_pct / 100.0))
                    .max(self.config.trend_reversal_threshold)
                    * ptb_factor;
                if pos.direction == "UP" {
                    // For UP: exit if BTC drops below threshold
                    current_btc < (pos.entry_btc - reversal_threshold)
                } else {
                    // For DOWN: exit if BTC rises above threshold
                    current_btc > (pos.entry_btc + reversal_threshold)
                }
            } else {
                false
            };

            // Trend reversed requires persistence; loss-making favorable-PTB trades need more proof.
            let trend_confirmation_ms =
                if current_price < pos.entry_price && ptb_margin.is_some_and(|m| m > 30.0) {
                    700
                } else {
                    200
                };
            // This filters out single-tick flash spikes that bounce right back
            let trend_reversed_confirmed = trend_reversed_hit
                && pos
                    .trend_reversed_since
                    .is_some_and(|t| t.elapsed().as_millis() >= trend_confirmation_ms);
            let trend_reversed = trend_reversed_confirmed && min_hold_passed;

            // Spike faded: exit if BTC reverses by X% of the peak favorable move from peak/trough
            // Uses the GREATER of entry_spike or total favorable move (peak_btc - entry_btc for UP)
            // This lets big winning runs breathe — if BTC ran $80, need $40 reversal (50%), not $10
            // PTB factor widens threshold when winning, tightens when losing
            let spike_faded_hit =
                if pos.entry_btc > 0.0 && current_btc > 0.0 && pos.entry_spike.abs() > 0.0 {
                    // Calculate total favorable move from entry
                    let favorable_move = if pos.direction == "UP" {
                        pos.peak_btc - pos.entry_btc
                    } else {
                        pos.entry_btc - pos.trough_btc
                    };
                    let reference_move = favorable_move.max(pos.entry_spike.abs());
                    let threshold_dollars =
                        reference_move * (self.config.spike_faded_pct / 100.0) * ptb_factor;
                    if pos.direction == "UP" {
                        // For UP: check if BTC dropped from peak
                        let peak = pos.peak_btc;
                        if peak > 0.0 {
                            let drop_from_peak = peak - current_btc;
                            drop_from_peak >= threshold_dollars
                        } else {
                            false
                        }
                    } else {
                        // For DOWN: check if BTC rose from trough
                        let trough = pos.trough_btc;
                        if trough > 0.0 {
                            let rise_from_trough = current_btc - trough;
                            rise_from_trough >= threshold_dollars
                        } else {
                            false
                        }
                    }
                } else {
                    false
                };

            // Spike faded requires persistence
            let spike_faded_confirmed = spike_faded_hit
                && pos
                    .spike_faded_since
                    .is_some_and(|t| t.elapsed().as_millis() >= self.config.spike_faded_ms as u128);

            // Stop-loss: exit if share price drops by X% from entry
            // Works the same for both UP and DOWN - we're long shares, price drop = loss
            let stop_loss_hit = if self.config.stop_loss_pct > 0.0 && min_hold_passed {
                let stop_price = pos.entry_price * (1.0 - self.config.stop_loss_pct / 100.0);
                current_price <= stop_price
            } else {
                false
            };

            // Trailing stop: protect profits by trailing the best share price
            // Only fires after trailing_stop_activated (share price gained activation_pct% from entry)
            let trailing_stop_hit = if pos.trailing_stop_activated
                && self.config.trailing_stop_pct > 0.0
                && min_hold_passed
            {
                let stop_level = pos.highest_price * (1.0 - self.config.trailing_stop_pct / 100.0);
                current_price <= stop_level
            } else {
                false
            };

            // Max price exit: if share price hits 0.995, take the free money
            let max_price_hit = current_price >= 0.995;

            let near_end = held_ms > 295000;

            let ptb_hold_active = ptb_tier.is_ptb_hold()
                || pos.entry_mode.as_deref() == Some("ptb_hold")
                || (pos.hold_to_resolution
                    && ptb_margin.is_some_and(|m| m > self.config.hold_safety_margin));

            if ptb_hold_active {
                let margin = ptb_margin.unwrap_or(0.0);
                if margin <= self.config.hold_safety_margin {
                    to_close.push((idx, "hold_safety_exit"));
                    continue;
                }
                if time_remaining.is_some_and(|t| t <= 2) {
                    to_close.push((idx, "market_end"));
                    continue;
                }

                let emergency_reversal = trend_reversed && margin < 30.0;
                if emergency_reversal {
                    to_close.push((idx, "trend_reversed"));
                    continue;
                }

                let suppressed_reason = if trend_reversed {
                    Some("trend_reversed")
                } else if trailing_stop_hit {
                    Some("trailing_stop")
                } else if stop_loss_hit {
                    Some("stop_loss")
                } else if spike_faded_confirmed {
                    Some("spike_faded")
                } else {
                    None
                };
                if let Some(reason) = suppressed_reason {
                    suppressed_exits.push((
                        idx,
                        pos.symbol.clone(),
                        pos.direction.clone(),
                        pos.entry_spike,
                        reason,
                    ));
                }
                continue;
            }

            let suppress_loss_trend_for_ptb = trend_reversed
                && current_price < pos.entry_price
                && ptb_margin.is_some_and(|m| m > 30.0);
            if suppress_loss_trend_for_ptb {
                suppressed_exits.push((
                    idx,
                    pos.symbol.clone(),
                    pos.direction.clone(),
                    pos.entry_spike,
                    "trend_reversed",
                ));
                continue;
            }

            // Determine exit reason (priority order)
            let reason = if max_price_hit {
                Some("max_price")
            } else if trend_reversed {
                Some("trend_reversed")
            } else if trailing_stop_hit {
                Some("trailing_stop")
            } else if min_hold_passed && stop_loss_hit {
                Some("stop_loss")
            } else if min_hold_passed && spike_faded_confirmed {
                Some("spike_faded")
            } else if min_hold_passed && near_end {
                Some("near_end")
            } else {
                None
            };

            if let Some(r) = reason {
                if pos.hold_to_resolution && r != "trend_reversed" && r != "max_price" {
                    continue;
                }
                to_close.push((idx, r));
            }
        }

        for (idx, symbol, direction, spike, reason) in suppressed_exits {
            if let Some(pos) = self.open_positions.get_mut(idx) {
                pos.exit_suppressed_count += 1;
            }
            self.events.push(WalletEvent {
                symbol,
                direction,
                spike,
                status: "HOLD".to_string(),
                reason: format!("EXIT_SUPPRESSED_PTB_HOLD({})", reason),
            });
        }

        // Update spike_faded_since for all positions
        for pos in self.open_positions.iter_mut() {
            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };

            let current_btc = state.last_binance;

            // Track spike_faded_since (uses same threshold as exit check)
            let spike_faded_hit =
                if pos.entry_btc > 0.0 && current_btc > 0.0 && pos.entry_spike.abs() > 0.0 {
                    let favorable_move = if pos.direction == "UP" {
                        pos.peak_btc - pos.entry_btc
                    } else {
                        pos.entry_btc - pos.trough_btc
                    };
                    let reference_move = favorable_move.max(pos.entry_spike.abs());
                    let threshold_dollars = reference_move * (self.config.spike_faded_pct / 100.0);
                    if pos.direction == "UP" {
                        let peak = pos.peak_btc;
                        if peak > 0.0 {
                            let drop_from_peak = peak - current_btc;
                            drop_from_peak >= threshold_dollars
                        } else {
                            false
                        }
                    } else {
                        let trough = pos.trough_btc;
                        if trough > 0.0 {
                            let rise_from_trough = current_btc - trough;
                            rise_from_trough >= threshold_dollars
                        } else {
                            false
                        }
                    }
                } else {
                    false
                };

            if spike_faded_hit {
                if pos.spike_faded_since.is_none() {
                    pos.spike_faded_since = Some(Instant::now());
                }
            } else {
                pos.spike_faded_since = None;
            }

            // Track trend_reversed_since (uses same scaled threshold as exit check)
            let trend_reversed_hit = if pos.entry_btc > 0.0 && current_btc > 0.0 {
                let reversal_threshold = (pos.entry_spike.abs()
                    * (self.config.trend_reversal_pct / 100.0))
                    .max(self.config.trend_reversal_threshold);
                if pos.direction == "UP" {
                    current_btc < (pos.entry_btc - reversal_threshold)
                } else {
                    current_btc > (pos.entry_btc + reversal_threshold)
                }
            } else {
                false
            };

            if trend_reversed_hit {
                if pos.trend_reversed_since.is_none() {
                    pos.trend_reversed_since = Some(Instant::now());
                }
            } else {
                pos.trend_reversed_since = None;
            }
        }

        if to_close.is_empty() {
            return false;
        }

        // Process closes
        to_close.sort_by_key(|k: &(usize, &str)| std::cmp::Reverse(k.0));
        let mut closed = false;
        for (idx, reason) in to_close {
            let pos = self.open_positions.remove(idx);
            let (bid, _ask) = self.get_bid_ask(&pos.symbol, &pos.direction);

            if bid <= 0.0 {
                self.events.push(WalletEvent {
                    symbol: pos.symbol.clone(),
                    direction: pos.direction.clone(),
                    spike: pos.entry_spike,
                    status: "REJECTED".to_string(),
                    reason: format!("EXIT_NO_BID_{}", reason),
                });
                self.open_positions.push(pos);
                continue;
            }

            let limit_price = if bid > 0.04 {
                (bid - 0.04).max(0.01)
            } else {
                0.01
            };
            let Some((fill_price, sold_shares, gross_revenue)) =
                self.estimate_fak_sell(&pos.symbol, &pos.direction, pos.shares, limit_price)
            else {
                self.events.push(WalletEvent {
                    symbol: pos.symbol.clone(),
                    direction: pos.direction.clone(),
                    spike: pos.entry_spike,
                    status: "REJECTED".to_string(),
                    reason: format!("EXIT_NO_FAK_LIQUIDITY_{}", reason),
                });
                self.open_positions.push(pos);
                continue;
            };

            let fill_ratio = (sold_shares / pos.shares).clamp(0.0, 1.0);
            let closed_cost = pos.position_size * fill_ratio;
            let closed_buy_fee = pos.buy_fee * fill_ratio;
            let sell_fee = self.calculate_fee(sold_shares, fill_price);
            let net_revenue = gross_revenue - sell_fee;
            let pnl = net_revenue - (closed_cost + closed_buy_fee);
            let state = self
                .symbol_states
                .get(&pos.symbol)
                .cloned()
                .unwrap_or_default();
            let ptb_margin_at_exit = state.last_price_to_beat.map(|ptb| {
                if pos.direction == "UP" {
                    state.last_binance - ptb
                } else {
                    ptb - state.last_binance
                }
            });
            let remaining_secs = state.last_market_end_ts.and_then(|end| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs();
                (end > now).then_some(end - now)
            });
            let required_exit_margin = remaining_secs
                .map(|t| {
                    self.config
                        .hold_safety_margin
                        .max(self.config.hold_margin_per_second * t as f64)
                })
                .unwrap_or(self.config.hold_safety_margin);
            let exit_mode = if matches!(reason, "market_end" | "near_end" | "manual") {
                "forced"
            } else if ptb_margin_at_exit.is_some_and(|m| m >= 100.0 || m >= required_exit_margin) {
                "ptb_conviction"
            } else {
                "scalp"
            };
            let ptb_tier_at_exit = PtbTier::from_margin(ptb_margin_at_exit);

            if sold_shares + 0.000001 < pos.shares {
                let mut remaining = pos.clone();
                remaining.shares = pos.shares - sold_shares;
                remaining.position_size = pos.position_size - closed_cost;
                remaining.buy_fee = pos.buy_fee - closed_buy_fee;
                self.open_positions.push(remaining);
                self.events.push(WalletEvent {
                    symbol: pos.symbol.clone(),
                    direction: pos.direction.clone(),
                    spike: pos.entry_spike,
                    status: "EXIT".to_string(),
                    reason: format!(
                        "{}_PARTIAL_FILL({:.2}/{:.2})",
                        reason, sold_shares, pos.shares
                    ),
                });
            } else {
                self.events.push(WalletEvent {
                    symbol: pos.symbol.clone(),
                    direction: pos.direction.clone(),
                    spike: pos.entry_spike,
                    status: "EXIT".to_string(),
                    reason: reason.to_string(),
                });
            }

            self.balance += net_revenue;
            self.trade_count += 1;
            if pnl > 0.0 {
                self.wins += 1;
            } else {
                self.losses += 1;
            }
            self.total_fees_paid += closed_buy_fee + sell_fee;
            self.total_volume += closed_cost + gross_revenue;
            self.trade_history.push(TradeRecord {
                symbol: pos.symbol.clone(),
                r#type: "exit".to_string(),
                question: state.question,
                direction: pos.direction.clone(),
                entry_price: Some(pos.entry_price),
                exit_price: Some(fill_price),
                shares: sold_shares,
                cost: closed_cost,
                pnl: Some(pnl),
                cumulative_pnl: None,
                balance_after: None,
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: Some(reason.to_string()),
                btc_at_entry: Some(pos.entry_btc),
                price_to_beat_at_entry: state.last_price_to_beat,
                ptb_margin_at_entry: state.last_price_to_beat.map(|ptb| {
                    if pos.direction == "UP" {
                        pos.entry_btc - ptb
                    } else {
                        ptb - pos.entry_btc
                    }
                }),
                seconds_to_expiry_at_entry: None,
                spread_at_entry: None,
                round_trip_loss_pct_at_entry: None,
                signal_score: None,
                ptb_margin_at_exit,
                exit_mode: Some(exit_mode.to_string()),
                favorable_ptb_at_exit: ptb_margin_at_exit.map(|m| m > 0.0),
                ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                ptb_tier_at_exit: Some(ptb_tier_at_exit.as_str().to_string()),
                entry_mode: pos.entry_mode.clone(),
                exit_suppressed_count: Some(pos.exit_suppressed_count),
            });
            info!(symbol=%pos.symbol, pnl=format!("${:.4}", pnl), reason=%reason, fill_price=fill_price, sold_shares=sold_shares, "Position closed with FAK bid-depth simulation");
            closed = true;
        }
        closed
    }

    pub fn open_position(
        &mut self,
        symbol: &str,
        direction: &str,
        spike: f64,
        _threshold_usd: f64,
        allow_scaling: bool,
        current_btc: f64,
    ) -> std::result::Result<u32, String> {
        let existing: Vec<_> = self
            .open_positions
            .iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let pending_same = self
            .pending_entries
            .iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();
        let scale_level = (existing.len() + pending_same) as u32 + 1;

        // In HOLD mode (allow_scaling=true), ignore MAX_SCALE_LEVEL to add to winning positions
        if !allow_scaling && scale_level > 1 {
            return Err("MAX_SCALE_LEVEL".to_string());
        }

        let entry_price = self.get_share_price(symbol, direction);
        if entry_price <= 0.0 {
            return Err("NO_PRICE_DATA".to_string());
        }
        if entry_price > self.config.max_entry_price {
            return Err("PRICE_TOO_HIGH".to_string());
        }
        if entry_price < self.config.min_entry_price {
            return Err("PRICE_TOO_LOW".to_string());
        }

        // Position sizing based on share price tiers
        // Cheaper shares = smaller position (higher risk), expensive shares = larger position
        let portfolio_pct = if entry_price < 0.20 {
            0.10 // 10% for very cheap shares
        } else if entry_price < 0.50 {
            0.15 // 15% for mid-range shares
        } else if entry_price < 0.75 {
            0.20 // 20% for higher-priced shares
        } else {
            self.portfolio_pct // config default for 75c+
        };
        let position_size = self.balance * portfolio_pct * (1.0 / scale_level as f64);

        // Cap position size at $20
        let position_size = position_size.min(20.0);

        let shares = position_size / entry_price;
        let buy_fee = self.calculate_fee(shares, entry_price);
        if (position_size + buy_fee) > self.balance {
            return Err("INSUFFICIENT_BALANCE".to_string());
        }
        if position_size < 1.0 {
            return Err("BELOW_MIN_ORDER_SIZE".to_string());
        }
        let entry_context = self.entry_context_score(
            symbol,
            direction,
            spike,
            _threshold_usd,
            current_btc,
            position_size,
        )?;
        let price_to_beat_at_entry = self
            .symbol_states
            .get(symbol)
            .and_then(|s| s.last_price_to_beat);

        // Reserve balance immediately so concurrent entries don't over-allocate
        self.balance -= position_size + buy_fee;

        self.pending_entries.push(PendingEntry {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            spike,
            entry_price,
            scale_level,
            position_size,
            shares,
            buy_fee,
            submitted_at: Instant::now(),
            entry_btc: current_btc, // Set BTC price immediately to avoid race condition
            live_synced: false,
            price_to_beat_at_entry,
            ptb_margin_at_entry: entry_context.ptb_margin,
            seconds_to_expiry_at_entry: entry_context.seconds_to_expiry,
            spread_at_entry: Some(entry_context.spread),
            round_trip_loss_pct_at_entry: Some(entry_context.round_trip_loss_pct),
            signal_score: Some(entry_context.score),
            ptb_tier_at_entry: Some(entry_context.ptb_tier.as_str().to_string()),
            entry_mode: Some(entry_context.entry_mode.to_string()),
        });

        Ok(scale_level)
    }

    /// Roll back a pending entry that failed to execute on the live wallet.
    /// Removes the pending entry and restores the reserved balance.
    pub fn rollback_pending_entry(&mut self, symbol: &str, direction: &str) {
        if let Some(idx) = self
            .pending_entries
            .iter()
            .position(|p| p.symbol == symbol && p.direction == direction)
        {
            let entry = self.pending_entries.remove(idx);
            self.balance += entry.position_size + entry.buy_fee;
            info!(symbol=%symbol, direction=%direction, restored=entry.position_size + entry.buy_fee, "Rolled back paper entry (live wallet failed)");
        }
    }

    /// Sync a pending paper entry to match the live wallet's actual fill.
    /// This ensures paper and live positions are identical so exit decisions apply correctly.
    /// Called after live wallet fills an order — adjusts the pending entry's price, shares,
    /// position_size, and fees to match the real fill.
    pub fn sync_pending_to_live_fill(
        &mut self,
        symbol: &str,
        direction: &str,
        live_entry_price: f64,
        live_shares: f64,
        live_position_size: f64,
        live_buy_fee: f64,
    ) {
        if let Some(entry) = self
            .pending_entries
            .iter_mut()
            .find(|p| p.symbol == symbol && p.direction == direction)
        {
            let old_reserved = entry.position_size + entry.buy_fee;
            let new_reserved = live_position_size + live_buy_fee;

            // Adjust balance: refund old reservation, apply new one
            self.balance += old_reserved;
            self.balance -= new_reserved;

            info!(symbol=%symbol, direction=%direction,
                  paper_price=entry.entry_price, live_price=live_entry_price,
                  paper_shares=entry.shares, live_shares=live_shares,
                  paper_cost=entry.position_size, live_cost=live_position_size,
                  "Paper entry synced to live fill");

            entry.entry_price = live_entry_price;
            entry.shares = live_shares;
            entry.position_size = live_position_size;
            entry.buy_fee = live_buy_fee;
            entry.live_synced = true;
        }
    }

    /// Call on every tick — promotes pending entries to open positions after execution delay
    pub fn flush_pending(&mut self) {
        let delay = if self.config.paper_trading {
            std::time::Duration::from_millis(self.config.execution_delay_ms)
        } else {
            std::time::Duration::ZERO
        };
        let mut promoted = Vec::new();
        self.pending_entries.retain(|p| {
            if p.submitted_at.elapsed() >= delay {
                promoted.push(p.clone());
                false
            } else {
                true
            }
        });

        for p in promoted {
            let state = self
                .symbol_states
                .get(&p.symbol)
                .cloned()
                .unwrap_or_default();

            // If this entry was synced from a live fill, use exact values — no recalculation
            let (fill_price, actual_shares, actual_fee, actual_position_size) = if p.live_synced {
                (p.entry_price, p.shares, p.buy_fee, p.position_size)
            } else {
                let (_bid, ask) = self.get_bid_ask(&p.symbol, &p.direction);
                if ask <= 0.0 {
                    self.balance += p.position_size + p.buy_fee;
                    self.events.push(WalletEvent {
                        symbol: p.symbol.clone(),
                        direction: p.direction.clone(),
                        spike: p.spike,
                        status: "REJECTED".to_string(),
                        reason: "ENTRY_NO_ASK_LIQUIDITY".to_string(),
                    });
                    info!(symbol=%p.symbol, direction=%p.direction, "Entry killed at fill time — no ask liquidity");
                    continue;
                }
                let limit_price = (ask + 0.04).min(0.99);
                let Some((fp, shares, spent_usdc)) =
                    self.estimate_fak_buy(&p.symbol, &p.direction, p.position_size, limit_price)
                else {
                    self.balance += p.position_size + p.buy_fee;
                    self.events.push(WalletEvent {
                        symbol: p.symbol.clone(),
                        direction: p.direction.clone(),
                        spike: p.spike,
                        status: "REJECTED".to_string(),
                        reason: "ENTRY_NO_FAK_LIQUIDITY".to_string(),
                    });
                    info!(symbol=%p.symbol, direction=%p.direction, ask=ask, limit=limit_price, "Entry killed at fill time — no orderbook liquidity within FAK limit");
                    continue;
                };

                // Re-check price bounds at fill time — price may have moved during execution delay
                // If fill price is now outside min/max, cancel the entry and refund balance
                if fp < self.config.min_entry_price || fp > self.config.max_entry_price {
                    self.balance += p.position_size + p.buy_fee;
                    self.events.push(WalletEvent {
                        symbol: p.symbol.clone(),
                        direction: p.direction.clone(),
                        spike: p.spike,
                        status: "REJECTED".to_string(),
                        reason: "ENTRY_PRICE_MOVED_OUTSIDE_BOUNDS".to_string(),
                    });
                    info!(symbol=%p.symbol, direction=%p.direction, fill_price=fp,
                        min=self.config.min_entry_price, max=self.config.max_entry_price,
                        "Entry cancelled at fill time — price moved outside bounds");
                    continue;
                }

                let fee = self.calculate_fee(shares, fp);
                let reserved = p.position_size + p.buy_fee;
                let actual_total = spent_usdc + fee;
                if reserved >= actual_total {
                    self.balance += reserved - actual_total;
                } else {
                    let extra = actual_total - reserved;
                    if self.balance >= extra {
                        self.balance -= extra;
                    } else {
                        self.balance += reserved;
                        self.events.push(WalletEvent {
                            symbol: p.symbol.clone(),
                            direction: p.direction.clone(),
                            spike: p.spike,
                            status: "REJECTED".to_string(),
                            reason: "ENTRY_INSUFFICIENT_FEE_BALANCE".to_string(),
                        });
                        info!(symbol=%p.symbol, direction=%p.direction, extra=extra, "Entry killed at fill time — insufficient balance for FAK fee");
                        continue;
                    }
                }

                (fp, shares, fee, spent_usdc)
            };

            self.trade_history.push(TradeRecord {
                symbol: p.symbol.clone(),
                r#type: "entry".to_string(),
                question: state.question.clone(),
                direction: p.direction.clone(),
                entry_price: Some(fill_price),
                exit_price: None,
                shares: actual_shares,
                cost: actual_position_size,
                pnl: None,
                cumulative_pnl: None,
                balance_after: None,
                timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: None,
                btc_at_entry: Some(p.entry_btc),
                price_to_beat_at_entry: p.price_to_beat_at_entry,
                ptb_margin_at_entry: p.ptb_margin_at_entry,
                seconds_to_expiry_at_entry: p.seconds_to_expiry_at_entry,
                spread_at_entry: p.spread_at_entry,
                round_trip_loss_pct_at_entry: p.round_trip_loss_pct_at_entry,
                signal_score: p.signal_score,
                ptb_margin_at_exit: None,
                exit_mode: None,
                favorable_ptb_at_exit: None,
                ptb_tier_at_entry: p.ptb_tier_at_entry.clone(),
                ptb_tier_at_exit: None,
                entry_mode: p.entry_mode.clone(),
                exit_suppressed_count: Some(0),
            });
            let sym = p.symbol.clone();
            let dir = p.direction.clone();
            let level = p.scale_level;
            let slippage = fill_price - p.entry_price;

            // Use BTC price at FILL time (now), not signal time (300ms ago)
            // This makes BTC-based exits (trend_reversed, spike_faded) more accurate
            let entry_btc = if state.last_binance > 0.0 {
                state.last_binance
            } else {
                p.entry_btc
            };

            self.open_positions.push(OpenPosition {
                symbol: p.symbol,
                direction: p.direction,
                entry_price: fill_price,
                avg_entry_price: fill_price,
                shares: actual_shares,
                position_size: actual_position_size,
                buy_fee: actual_fee,
                entry_spike: p.spike,
                entry_time: Instant::now(),
                highest_price: fill_price,
                scale_level: p.scale_level,
                hold_to_resolution: false,
                peak_spike: p.spike.abs(),
                // BTC price at fill time for accurate exit calibration
                entry_btc,
                peak_btc: entry_btc,
                trough_btc: entry_btc,
                spike_faded_since: None,
                trend_reversed_since: None,
                trailing_stop_activated: false,
                on_chain_shares: None,
                ptb_tier_at_entry: p.ptb_tier_at_entry,
                entry_mode: p.entry_mode,
                exit_suppressed_count: 0,
            });
            info!(symbol=%sym, direction=%dir, requested_price=p.entry_price, fill_price=fill_price, spread_slippage=slippage, shares=actual_shares, level=level, "Position opened with FAK orderbook simulation");
        }
    }
}
