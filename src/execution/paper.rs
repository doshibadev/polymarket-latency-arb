use crate::config::AppConfig;
use crate::execution::exit::{
    evaluate_hold_to_resolution, evaluate_position_exit, exit_mode_for, now_unix_secs, ptb_margin,
    update_exit_persistence, HoldEvaluationInput, PlannedExit, PositionExitContext,
};
use crate::execution::model::{OpenPosition, PendingEntry, TradeRecord};
use crate::execution::planner::{
    build_entry_plan, calculate_fee, estimate_fak_buy, estimate_fak_sell, EntryPlanInput, PtbTier,
};
use crate::execution::shared::{
    directional_bid_ask, share_mid_price, sync_position_price_state, update_btc_extrema,
    update_directional_bid_ask,
};
use crate::polymarket::BookLevel;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteRow, SqliteSynchronous,
};
use sqlx::{Pool, Row, Sqlite};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

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
    const SUPPRESSED_EXIT_SIGNAL_INTERVAL: Duration = Duration::from_secs(5);

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

        match sqlx::query("SELECT t, v FROM paper_equity_history ORDER BY id DESC LIMIT 1000")
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
        let new_history = self
            .history
            .iter()
            .skip(self.saved_history_count)
            .cloned()
            .collect();

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
        let mid_price = share_mid_price(bid, ask);
        update_directional_bid_ask(
            &mut state.up_bid,
            &mut state.up_ask,
            &mut state.down_bid,
            &mut state.down_ask,
            direction,
            bid,
            ask,
        );
        if direction == "UP" {
            if !bids.is_empty() {
                state.up_bids = bids;
            }
            if !asks.is_empty() {
                state.up_asks = asks;
            }
        } else {
            if !bids.is_empty() {
                state.down_bids = bids;
            }
            if !asks.is_empty() {
                state.down_asks = asks;
            }
        }

        let trailing_before = self
            .open_positions
            .iter()
            .filter(|pos| pos.symbol == symbol && pos.direction == direction)
            .filter(|pos| !pos.trailing_stop_activated)
            .count();
        sync_position_price_state(
            &mut self.open_positions,
            symbol,
            direction,
            mid_price,
            self.config.trailing_stop_activation,
        );
        let trailing_after = self
            .open_positions
            .iter()
            .filter(|pos| pos.symbol == symbol && pos.direction == direction)
            .filter(|pos| !pos.trailing_stop_activated)
            .count();
        if trailing_after < trailing_before {
            info!(
                symbol = %symbol,
                direction = %direction,
                "Trailing stop ACTIVATED (share price gained {}%+)",
                self.config.trailing_stop_activation
            );
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
        update_btc_extrema(&mut self.open_positions, symbol, current_btc);
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

            if let Some(should_hold) = evaluate_hold_to_resolution(HoldEvaluationInput {
                pos,
                btc_history,
                current_btc,
                price_to_beat: ptb,
                time_remaining,
                hold_margin_per_second,
                hold_max_seconds,
                hold_max_crossings,
            }) {
                pos.hold_to_resolution = should_hold;
            }
        }
    }

    pub fn calculate_fee(&self, shares: f64, price: f64) -> f64 {
        calculate_fee(shares, price, self.config.crypto_fee_rate)
    }

    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(state) = self.symbol_states.get(symbol) {
            directional_bid_ask(
                state.up_bid,
                state.up_ask,
                state.down_bid,
                state.down_ask,
                direction,
            )
        } else {
            (0.0, 0.0)
        }
    }

    fn get_book(&self, symbol: &str, direction: &str) -> Option<(&[BookLevel], &[BookLevel])> {
        self.symbol_states.get(symbol).map(|state| {
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
        estimate_fak_buy(asks, usdc_budget, limit_price)
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
        estimate_fak_sell(bids, shares_to_sell, limit_price)
    }

    pub fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        share_mid_price(bid, ask)
    }

    pub fn close_position_at_price(
        &mut self,
        idx: usize,
        current_price: f64,
        reason: &str,
    ) -> Option<TradeRecord> {
        if idx >= self.open_positions.len() {
            return None;
        }

        let pos = self.open_positions.remove(idx);
        let sell_fee = self.calculate_fee(pos.shares, current_price);
        let gross_revenue = pos.shares * current_price;
        let net_revenue = gross_revenue - sell_fee;
        let pnl = net_revenue - (pos.position_size + pos.buy_fee);

        self.balance += net_revenue;
        self.trade_count += 1;
        if pnl > 0.0 {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
        self.total_fees_paid += pos.buy_fee + sell_fee;
        self.total_volume += pos.position_size + gross_revenue;

        let exit_mode = if reason == "manual" {
            "manual"
        } else {
            "forced"
        };
        let trade = TradeRecord {
            symbol: pos.symbol.clone(),
            r#type: "exit".to_string(),
            question: String::new(),
            direction: pos.direction.clone(),
            entry_price: Some(pos.entry_price),
            exit_price: Some(current_price),
            shares: pos.shares,
            cost: pos.position_size,
            pnl: Some(pnl),
            cumulative_pnl: None,
            balance_after: None,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            close_reason: Some(reason.to_string()),
            btc_at_entry: Some(pos.entry_btc),
            price_to_beat_at_entry: None,
            ptb_margin_at_entry: None,
            seconds_to_expiry_at_entry: None,
            spread_at_entry: None,
            round_trip_loss_pct_at_entry: None,
            signal_score: None,
            ptb_margin_at_exit: None,
            exit_mode: Some(exit_mode.to_string()),
            favorable_ptb_at_exit: None,
            ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
            ptb_tier_at_exit: None,
            entry_mode: pos.entry_mode.clone(),
            exit_suppressed_count: Some(pos.exit_suppressed_count),
        };

        info!(symbol=%pos.symbol, pnl=pnl, reason=%reason, "Paper position closed at requested price");
        self.trade_history.push(trade.clone());
        Some(trade)
    }

    fn collect_exit_decisions(
        &mut self,
        excluded_positions: &HashSet<(String, String)>,
    ) -> Vec<(usize, &'static str)> {
        let mut to_close = Vec::new();
        let mut suppressed_exits: Vec<(usize, String, String, f64, &'static str)> = Vec::new();
        let now_secs = now_unix_secs();

        for (idx, pos) in self.open_positions.iter().enumerate() {
            if excluded_positions.contains(&(pos.symbol.clone(), pos.direction.clone())) {
                continue;
            }

            let current_price = self.get_share_price(&pos.symbol, &pos.direction);
            if current_price <= 0.0 {
                continue;
            }

            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };

            let decision = evaluate_position_exit(
                pos,
                &PositionExitContext {
                    current_price,
                    current_btc: state.last_binance,
                    last_price_to_beat: state.last_price_to_beat,
                    last_market_end_ts: state.last_market_end_ts,
                },
                &self.config,
                now_secs,
            );

            if let Some(reason) = decision.suppressed_reason {
                suppressed_exits.push((
                    idx,
                    pos.symbol.clone(),
                    pos.direction.clone(),
                    pos.entry_spike,
                    reason,
                ));
                continue;
            }

            if let Some(reason) = decision.reason {
                to_close.push((idx, reason));
            }
        }

        for (idx, symbol, direction, spike, reason) in suppressed_exits {
            let should_emit = if let Some(pos) = self.open_positions.get_mut(idx) {
                pos.exit_suppressed_count += 1;

                let should_emit = pos.last_suppressed_exit_signal.as_ref().is_none_or(
                    |(last_reason, last_at)| {
                        last_reason != reason
                            || last_at.elapsed() >= Self::SUPPRESSED_EXIT_SIGNAL_INTERVAL
                    },
                );

                if should_emit {
                    pos.last_suppressed_exit_signal = Some((reason.to_string(), Instant::now()));
                }

                should_emit
            } else {
                false
            };

            if should_emit {
                self.events.push(WalletEvent {
                    symbol,
                    direction,
                    spike,
                    status: "HOLD".to_string(),
                    reason: format!("EXIT_SUPPRESSED_PTB_HOLD({})", reason),
                });
            }
        }

        for pos in self.open_positions.iter_mut() {
            let state = match self.symbol_states.get(&pos.symbol) {
                Some(s) => s,
                None => continue,
            };

            update_exit_persistence(pos, state.last_binance, &self.config);
        }

        to_close
    }

    pub fn planned_exit_intents(
        &mut self,
        excluded_positions: &HashSet<(String, String)>,
    ) -> Vec<PlannedExit> {
        self.collect_exit_decisions(excluded_positions)
            .into_iter()
            .filter_map(|(idx, reason)| {
                let pos = self.open_positions.get(idx)?;
                Some(PlannedExit {
                    symbol: pos.symbol.clone(),
                    direction: pos.direction.clone(),
                    price: self.get_share_price(&pos.symbol, &pos.direction),
                    reason,
                })
            })
            .collect()
    }

    pub async fn try_close_position(&mut self) -> bool {
        let mut to_close = self.collect_exit_decisions(&HashSet::new());

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
            let ptb_margin_at_exit =
                ptb_margin(&pos.direction, state.last_binance, state.last_price_to_beat);
            let remaining_secs = state.last_market_end_ts.and_then(|end| {
                let now = now_unix_secs();
                (end > now).then_some(end - now)
            });
            let exit_mode = exit_mode_for(reason, ptb_margin_at_exit, remaining_secs, &self.config);
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
        threshold_usd: f64,
        allow_scaling: bool,
        current_btc: f64,
    ) -> std::result::Result<u32, String> {
        let existing_same_direction = self
            .open_positions
            .iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();
        let pending_same_direction = self
            .pending_entries
            .iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();

        let state = self.symbol_states.get(symbol).cloned().unwrap_or_default();
        let (bids, asks) = if direction == "UP" {
            (state.up_bids.as_slice(), state.up_asks.as_slice())
        } else {
            (state.down_bids.as_slice(), state.down_asks.as_slice())
        };
        let (bid, ask) = directional_bid_ask(
            state.up_bid,
            state.up_ask,
            state.down_bid,
            state.down_ask,
            direction,
        );

        let plan = build_entry_plan(EntryPlanInput {
            symbol,
            direction,
            spike,
            threshold_usd,
            allow_scaling,
            current_btc,
            balance: self.balance,
            portfolio_pct: self.portfolio_pct,
            config: &self.config,
            existing_same_direction,
            pending_same_direction,
            bid,
            ask,
            bids,
            asks,
            price_to_beat: state.last_price_to_beat,
            market_end_ts: state.last_market_end_ts,
            submitted_at: Instant::now(),
        })?;
        let scale_level = plan.scale_level;

        // Reserve balance immediately so concurrent entries don't over-allocate
        self.balance -= plan.position_size + plan.buy_fee;
        self.pending_entries.push(plan);

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

    pub fn rollback_pending_entry_at(
        &mut self,
        symbol: &str,
        direction: &str,
        submitted_at: Instant,
    ) {
        if let Some(idx) = self.pending_entries.iter().position(|p| {
            p.symbol == symbol && p.direction == direction && p.submitted_at == submitted_at
        }) {
            let entry = self.pending_entries.remove(idx);
            self.balance += entry.position_size + entry.buy_fee;
            info!(symbol=%symbol, direction=%direction, restored=entry.position_size + entry.buy_fee, "Rolled back exact pending paper entry");
        } else {
            self.rollback_pending_entry(symbol, direction);
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
        submitted_at: Instant,
        live_entry_price: f64,
        live_shares: f64,
        live_position_size: f64,
        live_buy_fee: f64,
    ) {
        if let Some(entry) = self.pending_entries.iter_mut().find(|p| {
            p.symbol == symbol && p.direction == direction && p.submitted_at == submitted_at
        }) {
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
        } else {
            warn!(symbol=%symbol, direction=%direction, "Pending paper entry missing for exact live fill sync");
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
                last_suppressed_exit_signal: None,
            });
            info!(symbol=%sym, direction=%dir, requested_price=p.entry_price, fill_price=fill_price, spread_slippage=slippage, shares=actual_shares, level=level, "Position opened with FAK orderbook simulation");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::config::AppConfig;

    use super::{PaperWallet, PendingEntry};

    #[test]
    fn sync_pending_to_live_fill_targets_exact_pending_entry() {
        let mut config = AppConfig::load().expect("config");
        config.paper_trading = true;
        let mut wallet = PaperWallet::new(config);

        let first_submitted_at = Instant::now() - Duration::from_millis(20);
        let second_submitted_at = Instant::now() - Duration::from_millis(10);

        wallet.pending_entries = vec![
            PendingEntry {
                symbol: "BTC-1".to_string(),
                direction: "UP".to_string(),
                spike: 10.0,
                entry_price: 0.40,
                scale_level: 1,
                position_size: 4.0,
                shares: 10.0,
                buy_fee: 0.1,
                submitted_at: first_submitted_at,
                entry_btc: 100000.0,
                live_synced: false,
                price_to_beat_at_entry: None,
                ptb_margin_at_entry: None,
                seconds_to_expiry_at_entry: None,
                spread_at_entry: None,
                round_trip_loss_pct_at_entry: None,
                signal_score: None,
                ptb_tier_at_entry: None,
                entry_mode: Some("scalp".to_string()),
            },
            PendingEntry {
                symbol: "BTC-1".to_string(),
                direction: "UP".to_string(),
                spike: 12.0,
                entry_price: 0.45,
                scale_level: 2,
                position_size: 6.0,
                shares: 13.0,
                buy_fee: 0.2,
                submitted_at: second_submitted_at,
                entry_btc: 100010.0,
                live_synced: false,
                price_to_beat_at_entry: None,
                ptb_margin_at_entry: None,
                seconds_to_expiry_at_entry: None,
                spread_at_entry: None,
                round_trip_loss_pct_at_entry: None,
                signal_score: None,
                ptb_tier_at_entry: None,
                entry_mode: Some("ptb_hold".to_string()),
            },
        ];

        wallet.sync_pending_to_live_fill("BTC-1", "UP", second_submitted_at, 0.51, 11.0, 5.61, 0.0);

        assert_eq!(wallet.pending_entries[0].entry_price, 0.40);
        assert!(!wallet.pending_entries[0].live_synced);

        assert_eq!(wallet.pending_entries[1].entry_price, 0.51);
        assert_eq!(wallet.pending_entries[1].shares, 11.0);
        assert_eq!(wallet.pending_entries[1].position_size, 5.61);
        assert!(wallet.pending_entries[1].live_synced);
    }
}
