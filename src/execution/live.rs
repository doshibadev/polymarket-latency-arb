use std::collections::HashMap;
use std::str::FromStr;
use std::time::Instant;

use alloy::primitives::U256 as AlloyU256;
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::sol;
use chrono::{Datelike, SecondsFormat};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{LocalSigner, Normal, Signer as _};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::ctf::Client as CtfClient;
use polymarket_client_sdk::types::{address, Address, Decimal, U256};
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteSynchronous};
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::execution::actor::LiveWalletSnapshot;
use crate::execution::model::{OpenPosition, PendingEntry, TradeRecord};
use crate::execution::shared::{
    share_mid_price, sync_position_price_state, update_btc_extrema,
};
use crate::polymarket::{BookLevel, MarketData};

// Polygon mainnet contract addresses (from Polymarket docs + reference bot)
const USDC: Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const CTF: Address = address!("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");
const EXCHANGE: Address = address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
const NEG_RISK_EXCHANGE: Address = address!("0xC5d563A36AE78145C45a50134d48A1215220f80a");
const NEG_RISK_ADAPTER: Address = address!("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296");

fn rpc_url() -> String {
    std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| "https://rpc.ankr.com/polygon".to_string())
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }
    #[sol(rpc)]
    interface IERC1155 {
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }
}

type K256Signer = LocalSigner<k256::ecdsa::SigningKey>;
type AuthClient = Client<Authenticated<Normal>>;
type RedeemClient = CtfClient<DynProvider>;

#[derive(Clone, Copy)]
enum WalletKind {
    Eoa,
    Gnosis,
}

impl WalletKind {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "eoa" => Ok(Self::Eoa),
            "gnosis" => Ok(Self::Gnosis),
            other => Err(format!(
                "Invalid WALLET_TYPE={other}. Expected `eoa` or `gnosis`."
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Eoa => "eoa",
            Self::Gnosis => "gnosis",
        }
    }
}

pub struct LivePreflightCheck {
    pub name: &'static str,
    pub detail: String,
}

pub struct LivePreflightReport {
    pub wallet_address: String,
    pub wallet_type: String,
    pub usdc_balance: f64,
    pub checks: Vec<LivePreflightCheck>,
}

impl LivePreflightReport {
    fn log_success(&self) {
        info!(
            wallet_address = %self.wallet_address,
            wallet_type = %self.wallet_type,
            usdc_balance = self.usdc_balance,
            checks = self.checks.len(),
            "Live startup preflight passed"
        );
        for check in &self.checks {
            info!(check = check.name, detail = %check.detail, "Preflight check passed");
        }
    }
}

#[derive(Clone)]
struct LiveWalletStateSnapshot {
    balance: f64,
    starting_balance: f64,
    trade_count: u64,
    wins: u64,
    losses: u64,
    total_fees_paid: f64,
    total_volume: f64,
    cumulative_pnl: f64,
    daily_pnl: f64,
    wallet_address: String,
    wallet_type: String,
}

#[derive(Clone)]
struct PersistedLivePosition {
    position_id: String,
    symbol: String,
    direction: String,
    entry_price: f64,
    avg_entry_price: f64,
    shares: f64,
    position_size: f64,
    buy_fee: f64,
    entry_spike: f64,
    highest_price: f64,
    scale_level: u32,
    hold_to_resolution: bool,
    peak_spike: f64,
    entry_btc: f64,
    peak_btc: f64,
    trough_btc: f64,
    trailing_stop_activated: bool,
    on_chain_shares: Option<f64>,
    ptb_tier_at_entry: Option<String>,
    entry_mode: Option<String>,
    exit_suppressed_count: u32,
    entry_time: String,
}

enum LiveDbWriteCommand {
    Save {
        wallet: LiveWalletStateSnapshot,
        trades: Vec<TradeRecord>,
        positions: Vec<PersistedLivePosition>,
    },
}

fn validate_markets(markets: &[MarketData]) -> Result<(), String> {
    if markets.is_empty() {
        return Err("No active markets found for live trading startup".to_string());
    }

    for market in markets {
        if market.symbol.trim().is_empty() {
            return Err("Live market preflight failed: empty symbol".to_string());
        }
        if market.condition_id.trim().is_empty() {
            return Err(format!(
                "Live market preflight failed: missing condition_id for {}",
                market.symbol
            ));
        }
        if market.up_token_id.trim().is_empty() || market.down_token_id.trim().is_empty() {
            return Err(format!(
                "Live market preflight failed: missing token ids for {}",
                market.symbol
            ));
        }
        if market.window_end_ts == 0 {
            return Err(format!(
                "Live market preflight failed: invalid window_end_ts for {}",
                market.symbol
            ));
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum RetryMode {
    Entry,
    Exit,
}

impl RetryMode {
    fn max_attempts(self) -> u32 {
        match self {
            Self::Entry => 2,
            Self::Exit => 3,
        }
    }

    fn backoff_ms(self, attempt: u32) -> u64 {
        match self {
            Self::Entry => 100 * (attempt + 1) as u64,
            Self::Exit => 200 * (attempt + 1) as u64,
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

async fn connect_redeem_client(
    signer: &K256Signer,
) -> Result<RedeemClient, Box<dyn std::error::Error + Send + Sync>> {
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(&rpc_url())
        .await?
        .erased();
    Ok(CtfClient::new(provider, POLYGON)?)
}

/// Check and set required on-chain approvals — view calls are FREE (no gas)
/// Only approve() and setApprovalForAll() cost gas, and only on first run
pub async fn ensure_approvals(
    signer: &K256Signer,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(&rpc_url())
        .await?;

    let owner = signer.address();

    // Check POL balance first — write txs need gas
    let pol_balance = provider.get_balance(owner).await.unwrap_or_default();
    let pol_f64 = pol_balance.to::<u128>() as f64 / 1e18;
    if pol_f64 < 0.001 {
        return Err(format!(
            "Insufficient POL for gas: {:.6} POL. Send at least 0.1 POL to {} to pay for approval transactions.",
            pol_f64, owner
        ).into());
    }
    info!(
        pol_balance = pol_f64,
        "POL balance sufficient for approvals"
    );

    let usdc = IERC20::new(USDC, provider.clone());
    let ctf = IERC1155::new(CTF, provider.clone());

    // All contracts that need USDC + CTF approval
    let targets = [
        ("Exchange", EXCHANGE),
        ("Neg Risk Exchange", NEG_RISK_EXCHANGE),
        ("Neg Risk Adapter", NEG_RISK_ADAPTER),
    ];

    for (name, target) in &targets {
        // allowance() is a view call — zero gas, zero cost
        let allowance = usdc.allowance(owner, *target).call().await?;
        if allowance < AlloyU256::from(1_000_000u64) {
            info!(contract = name, "Approving USDC (one-time gas cost)...");
            usdc.approve(*target, AlloyU256::MAX)
                .send()
                .await?
                .watch()
                .await?;
            info!(contract = name, "USDC approved");
        } else {
            info!(contract = name, "USDC already approved (no gas)");
        }

        // isApprovedForAll() is a view call — zero gas, zero cost
        let approved = ctf.isApprovedForAll(owner, *target).call().await?;
        if !approved {
            info!(contract = name, "Approving CTF (one-time gas cost)...");
            ctf.setApprovalForAll(*target, true)
                .send()
                .await?
                .watch()
                .await?;
            info!(contract = name, "CTF approved");
        } else {
            info!(contract = name, "CTF already approved (no gas)");
        }
    }

    info!("All approvals confirmed");
    Ok(())
}

/// Mirrors PaperWallet exactly but executes real CLOB orders
/// Track in-flight orders to prevent race conditions
#[derive(Clone)]
struct PendingOrder {
    symbol: String,
    direction: String,
    submitted_at: std::time::Instant,
}

/// Cache share prices from WebSocket
#[derive(Clone, Default)]
struct DirectionalQuoteCache {
    pub bid: f64,
    pub ask: f64,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub updated_at: Option<Instant>,
}

#[derive(Clone, Default)]
struct SymbolPriceCache {
    pub up: DirectionalQuoteCache,
    pub down: DirectionalQuoteCache,
}

#[derive(Clone)]
struct LiveQuoteSnapshot {
    bid: f64,
    ask: f64,
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
    age_ms: u64,
}

enum LiveSubmitSide {
    Open,
    Close,
}

fn validate_live_submit_against_quote(
    config: &AppConfig,
    quote: &LiveQuoteSnapshot,
    side: LiveSubmitSide,
    reference_price: f64,
    planned_spread: Option<f64>,
    notional_usdc: f64,
    open_positions_after_submit: usize,
    daily_pnl: f64,
) -> Result<(), String> {
    if config.live_max_order_usdc > 0.0 && notional_usdc > config.live_max_order_usdc {
        return Err(format!(
            "LIVE_MAX_ORDER_USDC_EXCEEDED({:.2}>{:.2})",
            notional_usdc, config.live_max_order_usdc
        ));
    }

    if config.live_max_session_loss_usdc > 0.0 && daily_pnl <= -config.live_max_session_loss_usdc {
        return Err(format!("LIVE_MAX_SESSION_LOSS_REACHED({:.2})", daily_pnl));
    }

    if open_positions_after_submit > config.live_max_open_positions {
        return Err(format!(
            "LIVE_MAX_OPEN_POSITIONS_EXCEEDED({}>{})",
            open_positions_after_submit, config.live_max_open_positions
        ));
    }

    if quote.age_ms > config.live_max_quote_age_ms {
        return Err(format!(
            "LIVE_QUOTE_STALE({}ms>{}ms)",
            quote.age_ms, config.live_max_quote_age_ms
        ));
    }
    if quote.bid <= 0.0 || quote.ask <= 0.0 {
        return Err("LIVE_QUOTE_EMPTY".to_string());
    }

    let slippage = (config.live_max_slippage_cents / 100.0).clamp(0.0, 0.10);
    let current_spread = quote.ask - quote.bid;
    if let Some(planned_spread) = planned_spread {
        if current_spread > planned_spread + slippage {
            return Err(format!(
                "LIVE_SPREAD_WIDENED({:.4}>{:.4})",
                current_spread,
                planned_spread + slippage
            ));
        }
    }

    match side {
        LiveSubmitSide::Open => {
            if quote.ask > reference_price + slippage {
                return Err(format!(
                    "LIVE_ASK_SLIPPAGE({:.4}>{:.4})",
                    quote.ask,
                    reference_price + slippage
                ));
            }
            if quote.asks.is_empty() {
                return Err("LIVE_ASK_DEPTH_EMPTY".to_string());
            }
            let buy_limit = (quote.ask + slippage).min(0.99);
            let (_, _, spend) =
                crate::execution::planner::estimate_fak_buy(&quote.asks, notional_usdc, buy_limit)
                    .ok_or_else(|| "LIVE_ENTRY_DEPTH_VANISHED".to_string())?;
            if spend + 0.01 < notional_usdc {
                return Err(format!(
                    "LIVE_ENTRY_DEPTH_THIN({:.2}<{:.2})",
                    spend, notional_usdc
                ));
            }
        }
        LiveSubmitSide::Close => {
            if quote.bid + slippage < reference_price {
                return Err(format!(
                    "LIVE_BID_SLIPPAGE({:.4}<{:.4})",
                    quote.bid,
                    reference_price - slippage
                ));
            }
            if quote.bids.is_empty() {
                return Err("LIVE_BID_DEPTH_EMPTY".to_string());
            }
            let sell_limit = if quote.bid > slippage {
                (quote.bid - slippage).max(0.01)
            } else {
                0.01
            };
            crate::execution::planner::estimate_fak_sell(
                &quote.bids,
                notional_usdc / reference_price.max(0.01),
                sell_limit,
            )
            .ok_or_else(|| "LIVE_EXIT_DEPTH_VANISHED".to_string())?;
        }
    }

    Ok(())
}

pub struct LiveWallet {
    pub balance: f64,
    pub starting_balance: f64,
    pub trade_count: u64,
    pub wins: u64,
    pub losses: u64,
    pub total_fees_paid: f64,
    pub total_volume: f64,
    pub open_positions: Vec<OpenPosition>,
    pub trade_history: Vec<TradeRecord>,
    pub wallet_address: String,
    pub cumulative_pnl: f64,
    pub daily_pnl: f64,
    pub last_reset_day: u32,
    config: AppConfig,
    clob: AuthClient,
    signer: K256Signer,
    token_map: HashMap<String, (String, String)>, // token_id -> (symbol, direction)
    pending_order_ids: HashMap<String, String>,   // "symbol:direction" -> order_id
    order_timestamps: Vec<std::time::Instant>,    // For rate limiting
    pending_orders: Vec<PendingOrder>, // Track in-flight orders to prevent race conditions
    price_cache: HashMap<String, SymbolPriceCache>,
    zero_balance_flags: HashMap<String, Instant>, // "symbol:direction" -> first zero-balance detection time
    redeem_client: Option<RedeemClient>,
    wallet_type: WalletKind,
    db: Option<Pool<Sqlite>>,
    db_writer: Option<mpsc::Sender<LiveDbWriteCommand>>,
    saved_trade_count: usize,
}

impl LiveWallet {
    const DB_URL: &'static str = "sqlite://lattice-live.db";

    pub async fn new(config: AppConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let private_key =
            std::env::var(PRIVATE_KEY_VAR).map_err(|_| "POLYMARKET_PRIVATE_KEY not set in .env")?;

        let signer = K256Signer::from_str(&private_key)?.with_chain_id(Some(POLYGON));

        let wallet_address = format!("{:?}", signer.address());
        let wallet_type = WalletKind::parse(
            &std::env::var("WALLET_TYPE").unwrap_or_else(|_| "gnosis".to_string()),
        )
        .map_err(|err| err.to_string())?;
        info!(
            eoa_address = %wallet_address,
            wallet_type = wallet_type.as_str(),
            "Live wallet"
        );

        // Authenticate — use GnosisSafe for browser wallet (polymarket.com account)
        // or Eoa for standalone EOA wallet. Set WALLET_TYPE=eoa in .env for EOA.
        let clob_config = Config::builder().use_server_time(true).build();
        let clob = {
            use polymarket_client_sdk::clob::types::SignatureType;
            let builder = Client::new("https://clob.polymarket.com", clob_config)?
                .authentication_builder(&signer);
            if matches!(wallet_type, WalletKind::Eoa) {
                builder
                    .signature_type(SignatureType::Eoa)
                    .authenticate()
                    .await?
            } else {
                // GnosisSafe: SDK auto-derives the proxy wallet address via CREATE2
                builder
                    .signature_type(SignatureType::GnosisSafe)
                    .authenticate()
                    .await?
            }
        };

        // Validate API credentials work
        match clob.api_keys().await {
            Ok(_) => info!("API credentials validated"),
            Err(e) => warn!(
                "API credential validation failed: {} — trading may not work",
                e
            ),
        }

        // Fetch real USDC balance from CLOB
        let balance = match clob.balance_allowance(Default::default()).await {
            Ok(b) => {
                let raw: f64 = b.balance.try_into().unwrap_or(0.0);
                let bal = raw / 1_000_000.0;
                info!(balance = bal, "Live USDC balance");
                bal
            }
            Err(e) => {
                warn!("Could not fetch balance: {}", e);
                0.0
            }
        };

        let redeem_client = connect_redeem_client(&signer).await.ok();

        Ok(Self {
            balance,
            starting_balance: balance,
            trade_count: 0,
            wins: 0,
            losses: 0,
            total_fees_paid: 0.0,
            total_volume: 0.0,
            open_positions: Vec::new(),
            trade_history: Vec::new(),
            wallet_address,
            cumulative_pnl: 0.0,
            daily_pnl: 0.0,
            last_reset_day: chrono::Utc::now().date_naive().day(),
            config,
            clob,
            signer,
            token_map: HashMap::new(),
            pending_order_ids: HashMap::new(),
            order_timestamps: Vec::new(),
            pending_orders: Vec::new(),
            price_cache: HashMap::new(),
            zero_balance_flags: HashMap::new(),
            redeem_client,
            wallet_type,
            db: None,
            db_writer: None,
            saved_trade_count: 0,
        })
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
                    LiveDbWriteCommand::Save {
                        wallet,
                        trades,
                        positions,
                    } => {
                        let mut tx = match db.begin().await {
                            Ok(tx) => tx,
                            Err(err) => {
                                warn!("Failed to start live wallet save transaction: {err}");
                                continue;
                            }
                        };

                        let save_result = async {
                            sqlx::query(
                                r#"
                                INSERT INTO live_wallet_state (
                                    id, balance, starting_balance, trade_count, wins, losses,
                                    total_fees_paid, total_volume, cumulative_pnl, daily_pnl,
                                    wallet_address, wallet_type, updated_at
                                )
                                VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                                ON CONFLICT(id) DO UPDATE SET
                                    balance = excluded.balance,
                                    starting_balance = excluded.starting_balance,
                                    trade_count = excluded.trade_count,
                                    wins = excluded.wins,
                                    losses = excluded.losses,
                                    total_fees_paid = excluded.total_fees_paid,
                                    total_volume = excluded.total_volume,
                                    cumulative_pnl = excluded.cumulative_pnl,
                                    daily_pnl = excluded.daily_pnl,
                                    wallet_address = excluded.wallet_address,
                                    wallet_type = excluded.wallet_type,
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
                            .bind(wallet.cumulative_pnl)
                            .bind(wallet.daily_pnl)
                            .bind(&wallet.wallet_address)
                            .bind(&wallet.wallet_type)
                            .bind(chrono::Utc::now().to_rfc3339())
                            .execute(&mut *tx)
                            .await?;

                            for trade in &trades {
                                sqlx::query(
                                    r#"
                                    INSERT INTO live_trades (
                                        position_id, symbol, type, question, direction, entry_price, exit_price, shares, cost,
                                        pnl, cumulative_pnl, balance_after, timestamp, close_reason, btc_at_entry,
                                        price_to_beat_at_entry, ptb_margin_at_entry, seconds_to_expiry_at_entry,
                                        spread_at_entry, round_trip_loss_pct_at_entry, signal_score,
                                        ptb_margin_at_exit, exit_mode, favorable_ptb_at_exit, ptb_tier_at_entry,
                                        ptb_tier_at_exit, entry_mode, exit_suppressed_count
                                    )
                                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                                    "#,
                                )
                                .bind(&trade.position_id)
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

                            sqlx::query("DELETE FROM live_open_positions")
                                .execute(&mut *tx)
                                .await?;

                            for position in &positions {
                                sqlx::query(
                                    r#"
                                    INSERT INTO live_open_positions (
                                        position_id, symbol, direction, entry_price, avg_entry_price, shares, position_size,
                                        buy_fee, entry_spike, highest_price, scale_level, hold_to_resolution,
                                        peak_spike, entry_btc, peak_btc, trough_btc, trailing_stop_activated,
                                        on_chain_shares, ptb_tier_at_entry, entry_mode, exit_suppressed_count,
                                        entry_time, updated_at
                                    )
                                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                                    "#,
                                )
                                .bind(&position.position_id)
                                .bind(&position.symbol)
                                .bind(&position.direction)
                                .bind(position.entry_price)
                                .bind(position.avg_entry_price)
                                .bind(position.shares)
                                .bind(position.position_size)
                                .bind(position.buy_fee)
                                .bind(position.entry_spike)
                                .bind(position.highest_price)
                                .bind(position.scale_level as i64)
                                .bind(position.hold_to_resolution)
                                .bind(position.peak_spike)
                                .bind(position.entry_btc)
                                .bind(position.peak_btc)
                                .bind(position.trough_btc)
                                .bind(position.trailing_stop_activated)
                                .bind(position.on_chain_shares)
                                .bind(&position.ptb_tier_at_entry)
                                .bind(&position.entry_mode)
                                .bind(position.exit_suppressed_count as i64)
                                .bind(&position.entry_time)
                                .bind(chrono::Utc::now().to_rfc3339())
                                .execute(&mut *tx)
                                .await?;
                            }

                            std::result::Result::<(), sqlx::Error>::Ok(())
                        }
                        .await;

                        match save_result {
                            Ok(()) => {
                                if let Err(err) = tx.commit().await {
                                    warn!("Failed to commit live wallet state: {err}");
                                }
                            }
                            Err(err) => {
                                warn!("Failed to save live wallet state: {err}");
                                let _ = tx.rollback().await;
                            }
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
            CREATE TABLE IF NOT EXISTS live_wallet_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                balance REAL NOT NULL,
                starting_balance REAL NOT NULL,
                trade_count INTEGER NOT NULL,
                wins INTEGER NOT NULL,
                losses INTEGER NOT NULL,
                total_fees_paid REAL NOT NULL,
                total_volume REAL NOT NULL,
                cumulative_pnl REAL NOT NULL,
                daily_pnl REAL NOT NULL,
                wallet_address TEXT NOT NULL,
                wallet_type TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(db)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS live_trades (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                position_id TEXT,
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
            CREATE TABLE IF NOT EXISTS live_open_positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                position_id TEXT NOT NULL,
                symbol TEXT NOT NULL,
                direction TEXT NOT NULL,
                entry_price REAL NOT NULL,
                avg_entry_price REAL NOT NULL,
                shares REAL NOT NULL,
                position_size REAL NOT NULL,
                buy_fee REAL NOT NULL,
                entry_spike REAL NOT NULL,
                highest_price REAL NOT NULL,
                scale_level INTEGER NOT NULL,
                hold_to_resolution INTEGER NOT NULL,
                peak_spike REAL NOT NULL,
                entry_btc REAL NOT NULL,
                peak_btc REAL NOT NULL,
                trough_btc REAL NOT NULL,
                trailing_stop_activated INTEGER NOT NULL,
                on_chain_shares REAL,
                ptb_tier_at_entry TEXT,
                entry_mode TEXT,
                exit_suppressed_count INTEGER NOT NULL,
                entry_time TEXT,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(db)
        .await?;

        let _ = sqlx::query("ALTER TABLE live_open_positions ADD COLUMN entry_time TEXT")
            .execute(db)
            .await;
        let _ = sqlx::query("ALTER TABLE live_open_positions ADD COLUMN position_id TEXT")
            .execute(db)
            .await;
        let _ = sqlx::query("ALTER TABLE live_trades ADD COLUMN position_id TEXT")
            .execute(db)
            .await;
        let _ = sqlx::query(
            "UPDATE live_open_positions SET position_id = symbol || ':' || direction || ':' || id WHERE position_id IS NULL OR position_id = ''",
        )
        .execute(db)
        .await;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_live_trades_timestamp ON live_trades(timestamp)",
        )
        .execute(db)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_live_open_positions_symbol_direction ON live_open_positions(symbol, direction)",
        )
        .execute(db)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_live_open_positions_position_id ON live_open_positions(position_id)",
        )
        .execute(db)
        .await?;

        Ok(())
    }

    fn wallet_state_snapshot(&self) -> LiveWalletStateSnapshot {
        LiveWalletStateSnapshot {
            balance: self.balance,
            starting_balance: self.starting_balance,
            trade_count: self.trade_count,
            wins: self.wins,
            losses: self.losses,
            total_fees_paid: self.total_fees_paid,
            total_volume: self.total_volume,
            cumulative_pnl: self.cumulative_pnl,
            daily_pnl: self.daily_pnl,
            wallet_address: self.wallet_address.clone(),
            wallet_type: self.wallet_type.as_str().to_string(),
        }
    }

    fn persisted_open_positions(&self) -> Vec<PersistedLivePosition> {
        self.open_positions
            .iter()
            .map(|position| PersistedLivePosition {
                position_id: position.position_id.clone(),
                symbol: position.symbol.clone(),
                direction: position.direction.clone(),
                entry_price: position.entry_price,
                avg_entry_price: position.avg_entry_price,
                shares: position.shares,
                position_size: position.position_size,
                buy_fee: position.buy_fee,
                entry_spike: position.entry_spike,
                highest_price: position.highest_price,
                scale_level: position.scale_level,
                hold_to_resolution: position.hold_to_resolution,
                peak_spike: position.peak_spike,
                entry_btc: position.entry_btc,
                peak_btc: position.peak_btc,
                trough_btc: position.trough_btc,
                trailing_stop_activated: position.trailing_stop_activated,
                on_chain_shares: position.on_chain_shares,
                ptb_tier_at_entry: position.ptb_tier_at_entry.clone(),
                entry_mode: position.entry_mode.clone(),
                exit_suppressed_count: position.exit_suppressed_count,
                entry_time: chrono::Utc::now()
                    .checked_sub_signed(
                        chrono::TimeDelta::from_std(position.entry_time.elapsed())
                            .unwrap_or_else(|_| chrono::TimeDelta::zero()),
                    )
                    .unwrap_or_else(chrono::Utc::now)
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
            })
            .collect()
    }

    fn restored_entry_time(timestamp: Option<&str>) -> Instant {
        let Some(timestamp) = timestamp else {
            return Instant::now();
        };
        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
            return Instant::now();
        };
        let age = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
        if age <= chrono::TimeDelta::zero() {
            return Instant::now();
        }
        let age_std = age
            .to_std()
            .unwrap_or_else(|_| std::time::Duration::from_secs(0));
        Instant::now()
            .checked_sub(age_std)
            .unwrap_or_else(Instant::now)
    }

    pub async fn load_state(&mut self) {
        let db = match self.db().await {
            Ok(db) => db,
            Err(err) => {
                warn!("Failed to open live wallet database: {err}");
                return;
            }
        };

        let wallet_row = match sqlx::query(
            r#"
            SELECT balance, starting_balance, trade_count, wins, losses, total_fees_paid,
                   total_volume, cumulative_pnl, daily_pnl, wallet_address
            FROM live_wallet_state
            WHERE id = 1
            "#,
        )
        .fetch_optional(&db)
        .await
        {
            Ok(row) => row,
            Err(err) => {
                warn!("Failed to load live wallet state: {err}");
                return;
            }
        };

        let Some(wallet_row) = wallet_row else {
            return;
        };

        self.balance = wallet_row.get("balance");
        self.starting_balance = wallet_row.get("starting_balance");
        self.trade_count = wallet_row.get::<i64, _>("trade_count") as u64;
        self.wins = wallet_row.get::<i64, _>("wins") as u64;
        self.losses = wallet_row.get::<i64, _>("losses") as u64;
        self.total_fees_paid = wallet_row.get("total_fees_paid");
        self.total_volume = wallet_row.get("total_volume");
        self.cumulative_pnl = wallet_row.get("cumulative_pnl");
        self.daily_pnl = wallet_row.get("daily_pnl");
        self.wallet_address = wallet_row.get("wallet_address");

        match sqlx::query("SELECT * FROM live_trades ORDER BY id ASC")
            .fetch_all(&db)
            .await
        {
            Ok(rows) => {
                self.trade_history = rows
                    .iter()
                    .map(|row| TradeRecord {
                        position_id: row.try_get("position_id").ok(),
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
                    })
                    .collect();
                self.saved_trade_count = self.trade_history.len();
            }
            Err(err) => warn!("Failed to load live trade history: {err}"),
        }

        match sqlx::query(
            r#"
            SELECT position_id, symbol, direction, entry_price, avg_entry_price, shares, position_size, buy_fee,
                   entry_spike, highest_price, scale_level, hold_to_resolution, peak_spike,
                   entry_btc, peak_btc, trough_btc, trailing_stop_activated, on_chain_shares,
                   ptb_tier_at_entry, entry_mode, exit_suppressed_count, entry_time, updated_at
            FROM live_open_positions
            ORDER BY id ASC
            "#,
        )
        .fetch_all(&db)
        .await
        {
            Ok(rows) => {
                self.open_positions = rows
                    .iter()
                    .map(|row| {
                        let entry_time: Option<String> = row.get("entry_time");
                        let updated_at: Option<String> = row.get("updated_at");
                        OpenPosition {
                            position_id: row.get("position_id"),
                            symbol: row.get("symbol"),
                            direction: row.get("direction"),
                            entry_price: row.get("entry_price"),
                            avg_entry_price: row.get("avg_entry_price"),
                            shares: row.get("shares"),
                            position_size: row.get("position_size"),
                            buy_fee: row.get("buy_fee"),
                            entry_spike: row.get("entry_spike"),
                            entry_time: Self::restored_entry_time(
                                entry_time.as_deref().or(updated_at.as_deref()),
                            ),
                            highest_price: row.get("highest_price"),
                            scale_level: row.get::<i64, _>("scale_level") as u32,
                            hold_to_resolution: row.get("hold_to_resolution"),
                            peak_spike: row.get("peak_spike"),
                            entry_btc: row.get("entry_btc"),
                            peak_btc: row.get("peak_btc"),
                            trough_btc: row.get("trough_btc"),
                            spike_faded_since: None,
                            trend_reversed_since: None,
                            trailing_stop_activated: row.get("trailing_stop_activated"),
                            on_chain_shares: row.get("on_chain_shares"),
                            ptb_tier_at_entry: row.get("ptb_tier_at_entry"),
                            entry_mode: row.get("entry_mode"),
                            exit_suppressed_count: row.get::<i64, _>("exit_suppressed_count")
                                as u32,
                            last_suppressed_exit_signal: None,
                        }
                    })
                    .collect();
            }
            Err(err) => warn!("Failed to load live open positions: {err}"),
        }

        info!(
            "Loaded live wallet state from SQLite ({} trades, {} open positions)",
            self.trade_history.len(),
            self.open_positions.len()
        );
    }

    pub async fn save_state(&mut self) {
        match self.db().await {
            Ok(_) => {}
            Err(err) => {
                warn!("Failed to open live wallet database for save: {err}");
                return;
            }
        }

        let wallet = self.wallet_state_snapshot();
        let new_trades = self.trade_history[self.saved_trade_count..].to_vec();
        let positions = self.persisted_open_positions();

        if let Some(writer) = &self.db_writer {
            match writer.try_send(LiveDbWriteCommand::Save {
                wallet,
                trades: new_trades.clone(),
                positions,
            }) {
                Ok(()) => {
                    self.saved_trade_count = self.trade_history.len();
                }
                Err(mpsc::error::TrySendError::Full(_)) if new_trades.is_empty() => {
                    warn!("Live DB writer backed up, skipped snapshot-only save");
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!("Live DB writer backed up, retaining unsaved live trades for retry");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!("Failed to queue live wallet save: DB writer closed");
                }
            }
        }
    }

    pub async fn run_startup_preflight(
        &mut self,
        initial_markets: &[MarketData],
    ) -> Result<LivePreflightReport, Box<dyn std::error::Error + Send + Sync>> {
        validate_markets(initial_markets).map_err(|err| err.to_string())?;

        let mut checks = vec![LivePreflightCheck {
            name: "market_metadata",
            detail: format!("{} active market(s) ready", initial_markets.len()),
        }];

        self.clob
            .api_keys()
            .await
            .map_err(|err| format!("Live preflight failed: API credentials invalid: {err}"))?;
        checks.push(LivePreflightCheck {
            name: "api_credentials",
            detail: "CLOB API credentials validated".to_string(),
        });

        let redeem_client = connect_redeem_client(&self.signer)
            .await
            .map_err(|err| format!("Live preflight failed: Polygon RPC unavailable: {err}"))?;
        self.redeem_client = Some(redeem_client);
        checks.push(LivePreflightCheck {
            name: "polygon_rpc",
            detail: format!("RPC reachable via {}", rpc_url()),
        });

        self.ensure_approvals()
            .await
            .map_err(|err| format!("Live preflight failed: approvals not ready: {err}"))?;
        checks.push(LivePreflightCheck {
            name: "approvals",
            detail: "USDC + CTF approvals confirmed".to_string(),
        });

        self.clob
            .update_balance_allowance(Default::default())
            .await
            .map_err(|err| {
                format!("Live preflight failed: could not refresh USDC balance: {err}")
            })?;
        let balance = self
            .clob
            .balance_allowance(Default::default())
            .await
            .map_err(|err| format!("Live preflight failed: could not fetch USDC balance: {err}"))?;
        let raw: f64 = balance.balance.try_into().unwrap_or(0.0);
        let usdc_balance = raw / 1_000_000.0;
        if usdc_balance < 1.0 {
            return Err(format!(
                "Live preflight failed: wallet has ${usdc_balance:.2} USDC, need at least $1.00 to place live orders"
            )
            .into());
        }
        let recovered_state_exists = self.trade_count > 0
            || !self.trade_history.is_empty()
            || !self.open_positions.is_empty();
        self.balance = usdc_balance;
        if !recovered_state_exists && self.starting_balance <= 0.0 {
            self.starting_balance = usdc_balance;
        }
        checks.push(LivePreflightCheck {
            name: "usdc_balance",
            detail: format!("wallet balance ${usdc_balance:.2}"),
        });

        let report = LivePreflightReport {
            wallet_address: self.wallet_address.clone(),
            wallet_type: self.wallet_type.as_str().to_string(),
            usdc_balance,
            checks,
        };
        report.log_success();
        Ok(report)
    }

    pub async fn recover_startup_state(
        &mut self,
        initial_markets: &[MarketData],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.load_state().await;

        for market in initial_markets {
            self.register_tokens(&market.up_token_id, &market.down_token_id, &market.symbol);
        }

        if self.open_positions.is_empty() && self.trade_history.is_empty() {
            return Ok(());
        }

        info!(
            trades = self.trade_history.len(),
            open_positions = self.open_positions.len(),
            "Recovering persisted live wallet state before actor startup"
        );

        self.sync_from_clob().await;
        self.save_state().await;
        Ok(())
    }

    pub async fn ensure_approvals(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        ensure_approvals(&self.signer).await
    }

    pub fn snapshot(&self) -> LiveWalletSnapshot {
        LiveWalletSnapshot {
            balance: self.balance,
            starting_balance: self.starting_balance,
            wins: self.wins,
            losses: self.losses,
            total_fees_paid: self.total_fees_paid,
            total_volume: self.total_volume,
            open_positions: self.open_positions.clone(),
            trade_history: self.trade_history.clone(),
            wallet_address: self.wallet_address.clone(),
            cumulative_pnl: self.cumulative_pnl,
            daily_pnl: self.daily_pnl,
        }
    }

    /// Sync real balance + validate open positions against CLOB
    pub async fn sync_from_clob(&mut self) {
        // Force CLOB to refresh cached USDC balance, then query
        let _ = self.clob.update_balance_allowance(Default::default()).await;
        if let Ok(b) = self.clob.balance_allowance(Default::default()).await {
            if let Ok(raw) = f64::try_from(b.balance) {
                let new_balance = raw / 1_000_000.0;
                if (new_balance - self.balance).abs() > 0.01 {
                    info!(
                        previous = self.balance,
                        on_chain = new_balance,
                        "USDC balance synced from CLOB"
                    );
                }
                self.balance = new_balance;
            }
        }

        // Check open orders — remove phantom positions (unfilled/cancelled buys)
        use polymarket_client_sdk::clob::types::request::OrdersRequest;
        use polymarket_client_sdk::clob::types::OrderStatusType;

        if let Ok(orders) = self.clob.orders(&OrdersRequest::default(), None).await {
            let live_ids: std::collections::HashSet<String> = orders
                .data
                .iter()
                .filter(|o| matches!(o.status, OrderStatusType::Live | OrderStatusType::Matched))
                .map(|o| o.id.to_string())
                .collect();

            self.open_positions.retain(|pos| {
                let key = format!("{}:{}", pos.symbol, pos.direction);
                match self.pending_order_ids.get(&key) {
                    Some(id) => live_ids.contains(id),
                    None => true, // no pending order = already filled, check token balance below
                }
            });
        }

        // External close detection: check if positions were sold externally or resolved.
        // Safety guards:
        //   1. Only check positions older than 30 seconds (CLOB cache is stale after recent buys)
        //   2. Call update_balance_allowance first to refresh the CLOB's cached on-chain balance
        //   3. Require zero balance to persist for 10+ seconds (two consecutive sync_from_clob calls)
        use polymarket_client_sdk::clob::types::AssetType;
        let now = Instant::now();
        let mut stale_indices = Vec::new();
        let mut still_zero_keys = Vec::new();
        let mut on_chain_updates: Vec<(usize, f64)> = Vec::new(); // (idx, shares) to apply after loop

        for (idx, pos) in self.open_positions.iter().enumerate() {
            // Skip positions younger than 30 seconds — CLOB cache may not have updated yet
            if pos.entry_time.elapsed().as_secs() < 30 {
                continue;
            }

            let pending_order_key = format!("{}:{}", pos.symbol, pos.direction);
            if self.pending_order_ids.contains_key(&pending_order_key) {
                continue;
            } // Handled by order check above

            let key = pos.position_id.clone();
            if let Some(token_id) = self.get_token_id(&pos.symbol, &pos.direction) {
                // Force CLOB to refresh its cached on-chain balance before querying
                let update_req =
                    polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                        .asset_type(AssetType::Conditional)
                        .token_id(token_id.clone())
                        .build();
                let _ = self.clob.update_balance_allowance(update_req).await;

                // Now query the (hopefully fresh) balance
                let query_req =
                    polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                        .asset_type(AssetType::Conditional)
                        .token_id(token_id)
                        .build();
                if let Ok(b) = self.clob.balance_allowance(query_req).await {
                    let raw: f64 = b.balance.try_into().unwrap_or(1.0); // Default to 1 (keep) on parse error
                                                                        // Conditional token balances are in atomic units, divide by 1e6
                    let shares = raw / 1_000_000.0;
                    info!(symbol=%pos.symbol, direction=%pos.direction,
                          on_chain_raw=raw, on_chain_shares=shares, tracked_shares=pos.shares,
                          age_secs=pos.entry_time.elapsed().as_secs(),
                          "Position balance check");
                    if shares < 0.001 {
                        // Zero balance detected — check if this is the second consecutive zero reading
                        if let Some(first_zero) = self.zero_balance_flags.get(&key) {
                            if first_zero.elapsed().as_secs() >= 10 {
                                // Confirmed: zero for 10+ seconds after the 30s grace period
                                stale_indices.push(idx);
                                still_zero_keys.push(key.clone());
                                info!(symbol=%pos.symbol, direction=%pos.direction,
                                    age_secs=pos.entry_time.elapsed().as_secs(),
                                    "Position confirmed externally closed (zero balance for 10+s)");
                            }
                            // else: not enough time since first zero — keep waiting
                        } else {
                            // First time seeing zero — flag it, will recheck on next sync
                            self.zero_balance_flags.insert(key.clone(), now);
                            warn!(symbol=%pos.symbol, direction=%pos.direction,
                                "Zero token balance detected — flagged, will confirm on next sync");
                        }
                    } else {
                        // Balance is positive — clear any previous zero flag and store on-chain shares
                        self.zero_balance_flags.remove(&key);
                        on_chain_updates.push((idx, shares));
                    }
                }
            }
        }

        // Apply on-chain share counts to positions (collected during read-only loop above)
        for (idx, shares) in on_chain_updates {
            if idx < self.open_positions.len() {
                self.open_positions[idx].on_chain_shares = Some(shares);
            }
        }

        // Remove confirmed stale positions
        for idx in stale_indices.into_iter().rev() {
            let pos = self.open_positions.remove(idx);
            let key = pos.position_id.clone();
            self.zero_balance_flags.remove(&key);
            let pnl = -(pos.position_size + pos.buy_fee); // Assume total loss (don't know sell price)
            self.cumulative_pnl += pnl;
            self.daily_pnl += pnl;
            self.trade_count += 1;
            self.losses += 1;
            self.total_fees_paid += pos.buy_fee;
            self.trade_history.push(TradeRecord {
                position_id: Some(pos.position_id.clone()),
                symbol: pos.symbol.clone(),
                r#type: "exit".to_string(),
                question: String::new(),
                direction: pos.direction.clone(),
                entry_price: Some(pos.entry_price),
                exit_price: Some(0.0),
                shares: pos.shares,
                cost: pos.position_size,
                pnl: Some(pnl),
                cumulative_pnl: Some(self.cumulative_pnl),
                balance_after: Some(self.balance),
                timestamp: now_rfc3339(),
                close_reason: Some("external_close".to_string()),
                btc_at_entry: Some(pos.entry_btc),
                price_to_beat_at_entry: None,
                ptb_margin_at_entry: None,
                seconds_to_expiry_at_entry: None,
                spread_at_entry: None,
                round_trip_loss_pct_at_entry: None,
                signal_score: None,
                ptb_margin_at_exit: None,
                exit_mode: Some("forced".to_string()),
                favorable_ptb_at_exit: None,
                ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                ptb_tier_at_exit: None,
                entry_mode: pos.entry_mode.clone(),
                exit_suppressed_count: Some(pos.exit_suppressed_count),
            });
            warn!(symbol=%pos.symbol, direction=%pos.direction, "Position removed — confirmed no shares on-chain (external close/resolution)");
        }

        // Clean up zero_balance_flags for positions that no longer exist
        self.zero_balance_flags
            .retain(|key, _| self.open_positions.iter().any(|p| p.position_id == *key));
    }

    /// Auto-redeem winning tokens after market resolution (on-chain tx, costs gas)
    pub async fn redeem_resolved_positions(&mut self, condition_id: &str) {
        use alloy::primitives::B256;
        use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;

        if self.redeem_client.is_none() {
            self.redeem_client = match connect_redeem_client(&self.signer).await {
                Ok(client) => Some(client),
                Err(e) => {
                    error!("Redeem client init failed: {}", e);
                    return;
                }
            };
        }

        let condition_id_bytes = match B256::from_str(condition_id) {
            Ok(b) => b,
            Err(e) => {
                error!("Invalid condition_id {}: {}", condition_id, e);
                return;
            }
        };

        // Redeem both index sets — CTF only pays out the winning one
        let request = RedeemPositionsRequest::builder()
            .collateral_token(USDC)
            .parent_collection_id(B256::ZERO)
            .condition_id(condition_id_bytes)
            .index_sets(vec![AlloyU256::from(1), AlloyU256::from(2)])
            .build();

        let Some(ctf_client) = self.redeem_client.as_ref() else {
            error!("Redeem client missing after init");
            return;
        };

        match ctf_client.redeem_positions(&request).await {
            Ok(r) => {
                info!(condition_id=%condition_id, tx=%r.transaction_hash, "Positions redeemed")
            }
            Err(e) => {
                self.redeem_client = None;
                error!(condition_id=%condition_id, error=%e, "Redeem failed")
            }
        }
    }

    pub fn register_tokens(&mut self, up_token: &str, down_token: &str, symbol: &str) {
        self.token_map
            .insert(up_token.to_string(), (symbol.to_string(), "UP".to_string()));
        self.token_map.insert(
            down_token.to_string(),
            (symbol.to_string(), "DOWN".to_string()),
        );
    }

    pub fn clear_tokens(&mut self, symbol: &str) {
        self.token_map.retain(|_, (s, _)| s != symbol);
    }

    /// Clear cached prices for a symbol (used during market transitions)
    pub fn clear_price_cache(&mut self, symbol: &str) {
        self.price_cache.remove(symbol);
    }

    /// Update share prices from WebSocket
    pub fn update_share_price(
        &mut self,
        symbol: &str,
        direction: &str,
        bid: f64,
        ask: f64,
        bids: Vec<BookLevel>,
        asks: Vec<BookLevel>,
        timestamp: Instant,
    ) {
        let cache = self.price_cache.entry(symbol.to_string()).or_default();
        let quote = if direction == "UP" {
            &mut cache.up
        } else {
            &mut cache.down
        };
        quote.bid = bid;
        quote.ask = ask;
        if !bids.is_empty() {
            quote.bids = bids;
        }
        if !asks.is_empty() {
            quote.asks = asks;
        }
        quote.updated_at = Some(timestamp);

        sync_position_price_state(
            &mut self.open_positions,
            symbol,
            direction,
            share_mid_price(bid, ask),
            self.config.trailing_stop_activation,
        );
    }

    /// Get bid/ask from cache
    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(cache) = self.price_cache.get(symbol) {
            let quote = Self::directional_quote_cache(cache, direction);
            (quote.bid, quote.ask)
        } else {
            (0.0, 0.0)
        }
    }

    /// Get share price from cache
    pub fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        share_mid_price(bid, ask)
    }

    /// Clean up stale pending orders (called periodically by engine)
    pub fn cleanup_pending_orders(&mut self) {
        let before = self.pending_orders.len();
        let now = std::time::Instant::now();
        self.pending_orders
            .retain(|p| now.duration_since(p.submitted_at).as_secs() < 5);
        let after = self.pending_orders.len();
        if before > after {
            warn!("Cleaned up {} stale pending orders", before - after);
        }
    }

    /// Update BTC trailing stop tracking for all open positions of a symbol
    pub fn update_btc_trailing(&mut self, symbol: &str, current_btc: f64) {
        update_btc_extrema(&mut self.open_positions, symbol, current_btc);
    }

    pub async fn verify_position_balance(
        &mut self,
        position_id: &str,
        symbol: &str,
        direction: &str,
    ) {
        use polymarket_client_sdk::clob::types::AssetType;

        let Some(token_id) = self.get_token_id(symbol, direction) else {
            return;
        };

        let update_req =
            polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_id.clone())
                .build();
        let _ = self.clob.update_balance_allowance(update_req).await;

        let query_req =
            polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_id)
                .build();

        let Ok(balance) = self.clob.balance_allowance(query_req).await else {
            return;
        };

        let raw: f64 = balance.balance.try_into().unwrap_or(0.0);
        let on_chain = raw / 1_000_000.0;
        if on_chain <= 0.0 {
            return;
        }

        if let Some(pos) = self
            .open_positions
            .iter_mut()
            .rev()
            .find(|pos| pos.position_id == position_id)
        {
            let estimated = pos.shares;
            if on_chain > estimated * 0.95 && on_chain < estimated * 1.05 {
                pos.shares = on_chain;
                pos.on_chain_shares = Some(on_chain);

                if let Some(trade) = self.trade_history.iter_mut().rev().find(|trade| {
                    trade.r#type == "entry"
                        && trade.position_id.as_deref() == Some(position_id)
                        && trade.symbol == symbol
                        && trade.direction == direction
                        && trade.exit_price.is_none()
                }) {
                    trade.shares = on_chain;
                }

                info!(symbol=%symbol, direction=%direction, estimated, on_chain,
                      "Async share-balance verification confirmed live fill");
            } else {
                info!(symbol=%symbol, direction=%direction, estimated, on_chain,
                      "Async share-balance verification mismatch, keeping estimated fill");
            }
        }
    }

    fn get_token_id(&self, symbol: &str, direction: &str) -> Option<U256> {
        self.token_map
            .iter()
            .find(|(_, (s, d))| s == symbol && d == direction)
            .and_then(|(id, _)| U256::from_str(id).ok())
    }

    fn round_to_tick(price: f64, tick: f64) -> f64 {
        if tick <= 0.0 {
            price // Return original price if tick is invalid
        } else {
            (price / tick).round() * tick
        }
    }

    fn live_slippage_dollars(&self) -> f64 {
        (self.config.live_max_slippage_cents / 100.0).clamp(0.0, 0.10)
    }

    fn directional_quote_cache<'a>(
        cache: &'a SymbolPriceCache,
        direction: &str,
    ) -> &'a DirectionalQuoteCache {
        if direction == "UP" {
            &cache.up
        } else {
            &cache.down
        }
    }

    fn current_quote(&self, symbol: &str, direction: &str) -> Option<LiveQuoteSnapshot> {
        let cache = self.price_cache.get(symbol)?;
        let quote = Self::directional_quote_cache(cache, direction);
        let updated_at = quote.updated_at?;
        let age_ms = Instant::now().duration_since(updated_at).as_millis() as u64;
        Some(LiveQuoteSnapshot {
            bid: quote.bid,
            ask: quote.ask,
            bids: quote.bids.clone(),
            asks: quote.asks.clone(),
            age_ms,
        })
    }

    fn validate_live_submit_quote(
        &self,
        symbol: &str,
        direction: &str,
        side: LiveSubmitSide,
        reference_price: f64,
        planned_spread: Option<f64>,
        notional_usdc: f64,
        open_positions_after_submit: usize,
    ) -> Result<LiveQuoteSnapshot, String> {
        let quote = self
            .current_quote(symbol, direction)
            .ok_or_else(|| "LIVE_QUOTE_MISSING".to_string())?;
        validate_live_submit_against_quote(
            &self.config,
            &quote,
            side,
            reference_price,
            planned_spread,
            notional_usdc,
            open_positions_after_submit,
            self.daily_pnl,
        )?;
        Ok(quote)
    }

    async fn post_with_retry(
        clob: &AuthClient,
        signer: &K256Signer,
        token_id: U256,
        price: Decimal,
        shares: Decimal,
        usdc: Decimal,
        side: Side,
        retry_mode: RetryMode,
    ) -> Result<(String, Decimal, Decimal), String> {
        // Returns (order_id, filled_shares, filled_usdc)
        use polymarket_client_sdk::clob::types::Amount;
        let max_attempts = retry_mode.max_attempts();
        for attempt in 0..max_attempts {
            // For market orders:
            // - BUY: amount is USDC to spend (pre-rounded to 2 decimals for FOK)
            // - SELL: amount is shares to sell
            let amount_result = match side {
                Side::Buy => Amount::usdc(usdc), // Pre-rounded USDC amount for buys
                Side::Sell => Amount::shares(shares), // Shares for sells
                _ => return Err("Unsupported side".to_string()),
            };

            let amount = match amount_result {
                Ok(a) => a,
                Err(e) => return Err(format!("Invalid amount: {}", e)),
            };

            // Skip if amount is zero
            if amount.as_inner().is_zero() {
                return Err("Amount is zero".to_string());
            }

            let order = clob
                .market_order()
                .token_id(token_id)
                .amount(amount)
                .price(price)
                .side(side.clone())
                .order_type(OrderType::FAK) // FAK = Fill-And-Kill (fills as much as possible, cancels rest)
                .build()
                .await
                .map_err(|e| e.to_string())?;

            let signed = clob.sign(signer, order).await.map_err(|e| e.to_string())?;

            match clob.post_order(signed).await {
                Ok(r) if r.success => {
                    info!(order_id=%r.order_id, status=%r.status, "Order confirmed by CLOB");
                    // Return order_id, filled shares (taker_amount), and filled USDC (maker_amount for buys)
                    // For buys: taker_amount = shares, maker_amount = USDC
                    // For sells: taker_amount = USDC, maker_amount = shares
                    let (filled_shares, filled_usdc) = match side {
                        Side::Buy => (r.taking_amount, r.making_amount),
                        Side::Sell => (r.making_amount, r.taking_amount),
                        _ => (r.taking_amount, r.making_amount),
                    };
                    return Ok((r.order_id.to_string(), filled_shares, filled_usdc));
                }
                Ok(r) => {
                    let msg = r
                        .error_msg
                        .as_ref()
                        .map(|e| format!("CLOB rejected: {}", e))
                        .unwrap_or_else(|| {
                            format!("Order failed: success={}, status={:?}", r.success, r.status)
                        });
                    warn!(attempt=attempt+1, error=%msg, "Order attempt failed");
                    if attempt + 1 >= max_attempts {
                        return Err(msg);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        retry_mode.backoff_ms(attempt),
                    ))
                    .await;
                }
                Err(e) => {
                    let msg = format!("Network error: {}", e);
                    // "no orders found to match" means the order book is empty — don't retry
                    if msg.contains("no orders found to match") {
                        warn!("FAK order found no liquidity (empty book) — not retrying");
                        return Err(msg);
                    }
                    // "not enough balance" means we don't have enough shares/USDC — retrying won't help
                    if msg.contains("not enough balance") {
                        warn!("Insufficient balance/allowance — not retrying");
                        return Err(msg);
                    }
                    warn!(attempt=attempt+1, error=%msg, "Order network error");
                    if attempt + 1 >= max_attempts {
                        return Err(msg);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        retry_mode.backoff_ms(attempt),
                    ))
                    .await;
                }
            }
        }
        Err("Max retries exceeded".to_string())
    }

    /// Execute exact paper-generated plan on Polymarket.
    /// Paper planner stays source of truth for strategy, sizing, and gating.
    pub async fn open_position(&mut self, plan: &PendingEntry) -> Result<u32, String> {
        let symbol = plan.symbol.as_str();
        let direction = plan.direction.as_str();
        let scale_level = plan.scale_level;
        let current_btc = plan.entry_btc;
        let planned_entry_price = plan.entry_price;
        let planned_position_size = plan.position_size;
        let planned_shares = plan.shares;
        let spike = plan.spike;
        let entry_mode = plan.entry_mode.clone();
        let ptb_tier_at_entry = plan.ptb_tier_at_entry.clone();

        let pending_marker = std::time::Instant::now();
        self.pending_orders.push(PendingOrder {
            symbol: plan.symbol.clone(),
            direction: plan.direction.clone(),
            submitted_at: pending_marker,
        });

        macro_rules! cleanup_and_return {
            ($err:expr) => {{
                self.pending_orders.retain(|p| {
                    !(p.symbol == plan.symbol
                        && p.direction == plan.direction
                        && p.submitted_at == pending_marker)
                });
                return Err($err);
            }};
        }

        let token_id = match self.get_token_id(symbol, direction) {
            Some(id) => id,
            None => cleanup_and_return!("NO_TOKEN_ID".to_string()),
        };

        let tick = 0.01f64;
        let open_positions_after_submit = self.open_positions.len() + self.pending_orders.len();
        let quote = match self.validate_live_submit_quote(
            symbol,
            direction,
            LiveSubmitSide::Open,
            planned_entry_price,
            plan.spread_at_entry,
            planned_position_size,
            open_positions_after_submit,
        ) {
            Ok(quote) => quote,
            Err(err) => cleanup_and_return!(err),
        };

        // MARKET ORDER: For FAK buys, the price field is the worst-price limit (max we'll pay per share).
        // The SDK uses this price to calculate taker_amount (shares) and maker_amount (USDC) for the
        // on-chain order struct. Setting it too far from the actual ask causes mismatched amounts that
        // won't match resting orders on the book.
        //
        // Strategy: use the cached ask + 4 cents buffer. This means:
        // - We'll fill at the actual ask (or better)
        // - Max overpay is 4 cents per share above the cached ask
        // - If the ask has moved more than 4 cents, we don't chase it
        let ask = quote.ask;

        // Max overpay: env-controlled slippage buffer above the current best ask.
        let market_price = (ask + self.live_slippage_dollars()).min(0.99);

        info!(symbol=%symbol, direction=%direction, cached_ask=ask, quote_age_ms=quote.age_ms,
              limit_price=market_price, max_order_usdc=self.config.live_max_order_usdc,
              position_size=planned_position_size, "Sending FAK buy order");

        let rounded_price = Self::round_to_tick(market_price, tick);

        // Round shares to 2 decimal places for Polymarket API
        // Polymarket requires: maker amount max 2 decimals, taker amount max 4 decimals
        let rounded_shares = (planned_shares * 100.0).floor() / 100.0;

        // Skip if rounded to 0 shares
        if rounded_shares < 0.01 {
            cleanup_and_return!(format!(
                "SHARES_TOO_SMALL: {:.4} shares rounds to 0",
                planned_shares
            ));
        }

        let rounded_usdc_budget = (planned_position_size * 100.0).floor() / 100.0;
        if rounded_usdc_budget < 1.0 {
            cleanup_and_return!(format!(
                "BELOW_MIN_ORDER_SIZE: ${:.2} < $1 minimum",
                rounded_usdc_budget
            ));
        }
        if rounded_usdc_budget > self.balance {
            cleanup_and_return!("INSUFFICIENT_BALANCE".to_string());
        }
        let usdc_amount = rounded_usdc_budget;

        let price = match Decimal::try_from(rounded_price) {
            Ok(p) => p,
            Err(e) => cleanup_and_return!(e.to_string()),
        };
        let size = match Decimal::try_from(rounded_shares) {
            Ok(s) => s,
            Err(e) => cleanup_and_return!(e.to_string()),
        };
        let usdc = match Decimal::try_from(usdc_amount) {
            Ok(u) => u,
            Err(e) => cleanup_and_return!(e.to_string()),
        };

        let (order_id, filled_shares, filled_usdc) = if self.config.live_dry_run_orders {
            info!(symbol=%symbol, direction=%direction, "LIVE_DRY_RUN_ORDERS enabled; skipping real buy submit");
            (format!("dry-run-buy-{}", plan.position_id), size, usdc)
        } else {
            match Self::post_with_retry(
                &self.clob,
                &self.signer,
                token_id,
                price,
                size,
                usdc,
                Side::Buy,
                RetryMode::Entry,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => cleanup_and_return!(e),
            }
        };

        // Use actual filled amounts (FAK may fill partially)
        let actual_shares: f64 = filled_shares.try_into().unwrap_or(rounded_shares);
        let actual_usdc: f64 = filled_usdc.try_into().unwrap_or(usdc_amount);

        // Guard against zero-fill FAK — don't create phantom positions
        // Note: balance has NOT been deducted yet (that happens at line 675), so we just clean up and exit
        if actual_shares <= 0.0 || actual_usdc <= 0.0 {
            self.pending_orders.retain(|p| {
                !(p.symbol == symbol
                    && p.direction == direction
                    && p.submitted_at == pending_marker)
            });
            warn!(symbol=%symbol, direction=%direction, "FAK got zero fill — no position created");
            return Err("FAK_NO_FILL".to_string());
        }

        // Recalculate position size based on actual fill
        let final_position_size = actual_usdc;
        // Compute actual fill price from FAK response (may differ from cached WebSocket price due to book-walking)
        let actual_entry_price = actual_usdc / actual_shares;

        // CLOB deducts taker fee from conditional tokens on buys (not from USDC).
        // Polymarket docs: "Buy orders: fees are collected in shares"
        // The FAK response's `taking_amount` is gross (before fee). Calculate net shares.
        let fee_in_shares =
            actual_shares * self.config.crypto_fee_rate * (1.0 - actual_entry_price);
        let estimated_net = actual_shares - fee_in_shares;

        // Try to get ACTUAL on-chain balance to use instead of the estimate.
        // Only accept it if it's within 5% of our estimate (rejects stale dust from old trades).
        let net_shares = estimated_net;

        // Fee already absorbed into fewer shares — don't double-count in USDC accounting
        let final_buy_fee = 0.0;

        info!(symbol=%symbol, direction=%direction, planned_price=planned_entry_price, fill_price=actual_entry_price,
              requested_shares=rounded_shares, gross_shares=actual_shares, net_shares=net_shares,
              filled_usdc=actual_usdc, order_id=%order_id, "Live BUY filled (FAK)");

        // Order successfully placed - remove from pending orders
        self.pending_orders.retain(|p| {
            !(p.symbol == symbol && p.direction == direction && p.submitted_at == pending_marker)
        });

        // Track order timestamp for rate limiting
        self.order_timestamps.push(std::time::Instant::now());

        // FAK orders fill instantly — do NOT track in pending_order_ids.
        // sync_from_clob() checks pending_order_ids against "Live" CLOB orders,
        // but FAK orders are already Filled/Killed (not "Live"), so tracking them
        // causes sync_from_clob to silently delete the position 200ms later.
        // The pending_order_ids map is only for resting limit orders (GTC).

        // Reserve balance with actual filled amounts
        self.balance -= final_position_size + final_buy_fee;

        self.trade_history.push(TradeRecord {
            position_id: Some(plan.position_id.clone()),
            symbol: symbol.to_string(),
            r#type: "entry".to_string(),
            question: String::new(),
            direction: direction.to_string(),
            entry_price: Some(actual_entry_price),
            exit_price: None,
            shares: net_shares, // Net shares after CLOB fee deduction
            cost: final_position_size,
            pnl: None,
            cumulative_pnl: Some(self.cumulative_pnl),
            balance_after: Some(self.balance),
            timestamp: now_rfc3339(),
            close_reason: None,
            btc_at_entry: Some(current_btc),
            price_to_beat_at_entry: plan.price_to_beat_at_entry,
            ptb_margin_at_entry: plan.ptb_margin_at_entry,
            seconds_to_expiry_at_entry: plan.seconds_to_expiry_at_entry,
            spread_at_entry: plan.spread_at_entry,
            round_trip_loss_pct_at_entry: plan.round_trip_loss_pct_at_entry,
            signal_score: plan.signal_score,
            ptb_margin_at_exit: None,
            exit_mode: None,
            favorable_ptb_at_exit: None,
            ptb_tier_at_entry: ptb_tier_at_entry.clone(),
            ptb_tier_at_exit: None,
            entry_mode: entry_mode.clone(),
            exit_suppressed_count: Some(0),
        });

        self.open_positions.push(OpenPosition {
            position_id: plan.position_id.clone(),
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price: actual_entry_price,
            avg_entry_price: actual_entry_price,
            shares: net_shares, // Net shares after CLOB fee deduction
            position_size: final_position_size,
            buy_fee: final_buy_fee,
            entry_spike: spike,
            entry_time: Instant::now(),
            highest_price: actual_entry_price,
            scale_level,
            hold_to_resolution: matches!(entry_mode.as_deref(), Some("ptb_hold")),
            peak_spike: spike.abs(),
            // BTC price set immediately at entry to avoid race condition - matches paper.rs
            entry_btc: current_btc,
            peak_btc: current_btc,
            trough_btc: current_btc,
            spike_faded_since: None,
            trend_reversed_since: None,
            trailing_stop_activated: false,
            on_chain_shares: None, // Async verification updates this after the realism delay
            ptb_tier_at_entry,
            entry_mode,
            exit_suppressed_count: 0,
            last_suppressed_exit_signal: None,
        });

        Ok(scale_level)
    }

    pub async fn close_position_at(&mut self, idx: usize, _current_price: f64, reason: &str) {
        if idx >= self.open_positions.len() {
            return;
        }

        // Validate we CAN sell before removing the position
        let pos = &self.open_positions[idx];
        let token_id = match self.get_token_id(&pos.symbol, &pos.direction) {
            Some(id) => id,
            None => {
                // No token ID = market transitioned. Record loss and remove position.
                let pos = self.open_positions.remove(idx);
                let pnl = -(pos.position_size + pos.buy_fee);
                self.cumulative_pnl += pnl;
                self.daily_pnl += pnl;
                self.trade_count += 1;
                self.losses += 1;
                self.total_fees_paid += pos.buy_fee;
                self.trade_history.push(TradeRecord {
                    position_id: Some(pos.position_id.clone()),
                    symbol: pos.symbol.clone(),
                    r#type: "exit".to_string(),
                    question: String::new(),
                    direction: pos.direction.clone(),
                    entry_price: Some(pos.entry_price),
                    exit_price: Some(0.0),
                    shares: pos.shares,
                    cost: pos.position_size,
                    pnl: Some(pnl),
                    cumulative_pnl: Some(self.cumulative_pnl),
                    balance_after: Some(self.balance),
                    timestamp: now_rfc3339(),
                    close_reason: Some(format!("{}_no_token", reason)),
                    btc_at_entry: Some(pos.entry_btc),
                    price_to_beat_at_entry: None,
                    ptb_margin_at_entry: None,
                    seconds_to_expiry_at_entry: None,
                    spread_at_entry: None,
                    round_trip_loss_pct_at_entry: None,
                    signal_score: None,
                    ptb_margin_at_exit: None,
                    exit_mode: Some("forced".to_string()),
                    favorable_ptb_at_exit: None,
                    ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                    ptb_tier_at_exit: None,
                    entry_mode: pos.entry_mode.clone(),
                    exit_suppressed_count: Some(pos.exit_suppressed_count),
                });
                error!(
                    "No token ID for {} {} — recorded loss",
                    pos.symbol, pos.direction
                );
                return;
            }
        };

        let quote = match self.validate_live_submit_quote(
            &pos.symbol,
            &pos.direction,
            LiveSubmitSide::Close,
            _current_price,
            None,
            pos.shares * _current_price,
            self.open_positions.len(),
        ) {
            Ok(quote) => quote,
            Err(err) => {
                warn!(position_id=%pos.position_id, symbol=%pos.symbol, direction=%pos.direction, reason=%err, "Rejected live close before submit");
                return;
            }
        };
        let bid = quote.bid;
        let tick = 0.01f64;

        // If no bid price or position too small to sell, record as loss instead of silently dropping
        // For FAK sells, the price is the MINIMUM we'll accept per share.
        // Use bid - configured slippage buffer as our worst acceptable price.
        // The CLOB fills at resting bid prices, so we typically get the actual bid.
        let slippage = self.live_slippage_dollars();
        let market_price = if bid > slippage {
            (bid - slippage).max(0.01)
        } else if bid > 0.0 {
            0.01 // Very low bid — accept anything to exit
        } else {
            0.0
        };

        let can_sell = bid > 0.0 && pos.shares * bid >= 1.0; // Use bid for min order check

        if !can_sell {
            // Can't sell — record exit as loss, restore nothing (shares are on-chain but unsellable)
            let pos = self.open_positions.remove(idx);
            let pnl = -(pos.position_size + pos.buy_fee);
            self.cumulative_pnl += pnl;
            self.daily_pnl += pnl;
            self.trade_count += 1;
            self.losses += 1;
            self.total_fees_paid += pos.buy_fee;
            self.trade_history.push(TradeRecord {
                position_id: Some(pos.position_id.clone()),
                symbol: pos.symbol.clone(),
                r#type: "exit".to_string(),
                question: String::new(),
                direction: pos.direction.clone(),
                entry_price: Some(pos.entry_price),
                exit_price: Some(0.0),
                shares: pos.shares,
                cost: pos.position_size,
                pnl: Some(pnl),
                cumulative_pnl: Some(self.cumulative_pnl),
                balance_after: Some(self.balance),
                timestamp: now_rfc3339(),
                close_reason: Some(format!("{}_unsellable", reason)),
                btc_at_entry: Some(pos.entry_btc),
                price_to_beat_at_entry: None,
                ptb_margin_at_entry: None,
                seconds_to_expiry_at_entry: None,
                spread_at_entry: None,
                round_trip_loss_pct_at_entry: None,
                signal_score: None,
                ptb_margin_at_exit: None,
                exit_mode: Some("forced".to_string()),
                favorable_ptb_at_exit: None,
                ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                ptb_tier_at_exit: None,
                entry_mode: pos.entry_mode.clone(),
                exit_suppressed_count: Some(pos.exit_suppressed_count),
            });
            warn!(
                "Position unsellable: {} {} — bid={:.4}, value=${:.2}",
                pos.symbol,
                pos.direction,
                bid,
                pos.shares * bid
            );
            return;
        }

        // Now safe to remove — we know we can attempt the sell
        let pos = self.open_positions.remove(idx);

        // Cancel pending buy if still open
        let key = format!("{}:{}", pos.symbol, pos.direction);
        if let Some(order_id) = self.pending_order_ids.remove(&key) {
            if let Err(e) = self.clob.cancel_order(&order_id).await {
                warn!("Could not cancel buy order {}: {}", order_id, e);
            }
        }

        // Sell strategy: use on_chain_shares if available (exact from sync_from_clob),
        // otherwise fall back to pos.shares (net of fee from Fix 1).
        let best_shares = pos.on_chain_shares.unwrap_or(pos.shares);
        let sell_shares = (best_shares * 100.0).floor() / 100.0; // Round to 2 decimals

        let rounded = Self::round_to_tick(market_price, tick);
        let price = match Decimal::try_from(rounded) {
            Ok(p) => p,
            Err(e) => {
                // Record loss on conversion error
                let pnl = -(pos.position_size + pos.buy_fee);
                self.cumulative_pnl += pnl;
                self.daily_pnl += pnl;
                self.trade_count += 1;
                self.losses += 1;
                self.trade_history.push(TradeRecord {
                    position_id: Some(pos.position_id.clone()),
                    symbol: pos.symbol.clone(),
                    r#type: "exit".to_string(),
                    question: String::new(),
                    direction: pos.direction.clone(),
                    entry_price: Some(pos.entry_price),
                    exit_price: Some(0.0),
                    shares: pos.shares,
                    cost: pos.position_size,
                    pnl: Some(pnl),
                    cumulative_pnl: Some(self.cumulative_pnl),
                    balance_after: Some(self.balance),
                    timestamp: now_rfc3339(),
                    close_reason: Some(format!("{}_price_error", reason)),
                    btc_at_entry: Some(pos.entry_btc),
                    price_to_beat_at_entry: None,
                    ptb_margin_at_entry: None,
                    seconds_to_expiry_at_entry: None,
                    spread_at_entry: None,
                    round_trip_loss_pct_at_entry: None,
                    signal_score: None,
                    ptb_margin_at_exit: None,
                    exit_mode: Some("forced".to_string()),
                    favorable_ptb_at_exit: None,
                    ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
                    ptb_tier_at_exit: None,
                    entry_mode: pos.entry_mode.clone(),
                    exit_suppressed_count: Some(pos.exit_suppressed_count),
                });
                error!("Invalid sell price: {}", e);
                return;
            }
        };

        // Try selling up to 5 times: handles "not enough balance" retries, partial fills,
        // AND transient CLOB errors (500s). On partial fill, retries the remainder.
        let mut current_sell_shares = sell_shares;
        let mut total_filled_usdc = 0.0f64;
        let mut total_filled_shares = 0.0f64;

        for sell_attempt in 0..5u32 {
            let size = match Decimal::try_from(current_sell_shares) {
                Ok(s) => s,
                Err(e) => {
                    error!("Invalid sell size on attempt {}: {}", sell_attempt + 1, e);
                    break;
                }
            };
            let usdc_for_sell = Decimal::ZERO;

            let submit_result = if self.config.live_dry_run_orders {
                info!(symbol=%pos.symbol, direction=%pos.direction, "LIVE_DRY_RUN_ORDERS enabled; skipping real sell submit");
                Ok((
                    format!("dry-run-sell-{}-{}", pos.position_id, sell_attempt + 1),
                    size,
                    Decimal::try_from(current_sell_shares * rounded).unwrap_or(Decimal::ZERO),
                ))
            } else {
                Self::post_with_retry(
                    &self.clob,
                    &self.signer,
                    token_id,
                    price,
                    size,
                    usdc_for_sell,
                    Side::Sell,
                    RetryMode::Exit,
                )
                .await
            };

            match submit_result {
                Ok((id, filled_shares, filled_usdc)) => {
                    let fs: f64 = filled_shares.try_into().unwrap_or(0.0);
                    let fu: f64 = filled_usdc.try_into().unwrap_or(0.0);
                    total_filled_shares += fs;
                    total_filled_usdc += fu;

                    let remaining = current_sell_shares - fs;
                    if remaining >= 0.01 && remaining * market_price >= 1.0 {
                        // Partial fill — retry selling the remainder
                        info!(symbol=%pos.symbol, reason=%reason, order_id=%id,
                              filled=fs, remaining=remaining, attempt=sell_attempt+1,
                              "Partial SELL fill, retrying remainder");
                        current_sell_shares = (remaining * 100.0).floor() / 100.0;
                        continue;
                    }

                    // Full fill or remainder too small to sell
                    if remaining > 0.01 {
                        warn!(symbol=%pos.symbol, abandoned_shares=remaining,
                              value=remaining * market_price,
                              "Remainder below $1 min — shares left on-chain");
                    }
                    info!(symbol=%pos.symbol, reason=%reason, price=rounded, order_id=%id,
                          filled_shares=total_filled_shares, filled_usdc=total_filled_usdc,
                          attempt=sell_attempt+1, "Live SELL complete (FAK)");
                    break;
                }
                Err(e) if e.contains("not enough balance") && sell_attempt < 4 => {
                    // CLOB says we don't have enough shares — refresh balance and retry
                    let wait_ms = match sell_attempt {
                        0 => 300,
                        1 => 700,
                        2 => 1000,
                        _ => 1500,
                    };
                    warn!(symbol=%pos.symbol, attempt=sell_attempt+1,
                          tried_shares=current_sell_shares, wait_ms=wait_ms,
                          "Sell rejected (not enough balance) — refreshing and retrying");

                    use polymarket_client_sdk::clob::types::AssetType;
                    let update_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                        .asset_type(AssetType::Conditional)
                        .token_id(token_id.clone())
                        .build();
                    let _ = self.clob.update_balance_allowance(update_req).await;

                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;

                    let balance_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                        .asset_type(AssetType::Conditional)
                        .token_id(token_id.clone())
                        .build();
                    if let Ok(b) = self.clob.balance_allowance(balance_req).await {
                        let raw: f64 = b.balance.try_into().unwrap_or(0.0);
                        let actual = raw / 1_000_000.0;
                        if actual > 0.001 {
                            current_sell_shares = (actual * 100.0).floor() / 100.0;
                            info!(symbol=%pos.symbol, actual_shares=actual, selling=current_sell_shares,
                                  "Refreshed share balance, retrying sell");
                        }
                    }
                }
                Err(e) if e.contains("no orders found to match") => {
                    // Empty book — retrying won't help, push back for later
                    warn!(symbol=%pos.symbol, "Sell failed (empty book) — no liquidity to sell into");
                    self.open_positions.push(pos);
                    return;
                }
                Err(e) if sell_attempt < 4 => {
                    // Transient error (500, network, etc.) — retry in the outer loop
                    // post_with_retry already tried 3 times internally; give it another shot
                    warn!(symbol=%pos.symbol, attempt=sell_attempt+1, error=%e,
                          "Sell failed (transient) — retrying in outer loop");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    // Final attempt exhausted — put position back for orphan retry
                    error!(
                        "Sell order failed after all attempts: {} — putting position back ({} {})",
                        e, pos.symbol, pos.direction
                    );
                    self.open_positions.push(pos);
                    return;
                }
            }
        }

        let gross_revenue = if total_filled_usdc > 0.0 {
            total_filled_usdc
        } else {
            // No fills at all — put position back
            error!(
                "All sell attempts got zero fills — putting position back ({} {})",
                pos.symbol, pos.direction
            );
            self.open_positions.push(pos);
            return;
        };

        // If we reach here, the sell succeeded — safe to calculate PnL
        let sell_fee =
            pos.shares * self.config.crypto_fee_rate * pos.entry_price * (1.0 - pos.entry_price);
        let net_revenue = gross_revenue - sell_fee;
        let pnl = net_revenue - (pos.position_size + pos.buy_fee);

        self.balance += net_revenue;
        self.cumulative_pnl += pnl;
        self.daily_pnl += pnl;
        self.trade_count += 1;
        if pnl > 0.0 {
            self.wins += 1;
        } else {
            self.losses += 1;
        }
        self.total_fees_paid += pos.buy_fee + sell_fee;
        self.total_volume += pos.position_size + gross_revenue;

        self.trade_history.push(TradeRecord {
            position_id: Some(pos.position_id.clone()),
            symbol: pos.symbol.clone(),
            r#type: "exit".to_string(),
            question: String::new(),
            direction: pos.direction.clone(),
            entry_price: Some(pos.entry_price),
            exit_price: Some(gross_revenue / pos.shares),
            shares: pos.shares,
            cost: pos.position_size,
            pnl: Some(pnl),
            cumulative_pnl: Some(self.cumulative_pnl),
            balance_after: Some(self.balance),
            timestamp: now_rfc3339(),
            close_reason: Some(reason.to_string()),
            btc_at_entry: Some(pos.entry_btc),
            price_to_beat_at_entry: None,
            ptb_margin_at_entry: None,
            seconds_to_expiry_at_entry: None,
            spread_at_entry: None,
            round_trip_loss_pct_at_entry: None,
            signal_score: None,
            ptb_margin_at_exit: None,
            exit_mode: Some("scalp".to_string()),
            favorable_ptb_at_exit: None,
            ptb_tier_at_entry: pos.ptb_tier_at_entry.clone(),
            ptb_tier_at_exit: None,
            entry_mode: pos.entry_mode.clone(),
            exit_suppressed_count: Some(pos.exit_suppressed_count),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        validate_live_submit_against_quote, validate_markets, LiveQuoteSnapshot, LiveSubmitSide,
        WalletKind,
    };
    use crate::config::AppConfig;
    use crate::polymarket::BookLevel;
    use crate::polymarket::MarketData;

    fn sample_market() -> MarketData {
        MarketData {
            symbol: "BTC".to_string(),
            condition_id: "cond".to_string(),
            question: "BTC up or down?".to_string(),
            up_price: 0.51,
            down_price: 0.49,
            up_token_id: "up".to_string(),
            down_token_id: "down".to_string(),
            window_end_ts: 123,
        }
    }

    fn test_config() -> AppConfig {
        let mut config = AppConfig::load().expect("config should load");
        config.live_max_order_usdc = 20.0;
        config.live_max_session_loss_usdc = 25.0;
        config.live_max_open_positions = 1;
        config.live_max_slippage_cents = 4.0;
        config.live_max_quote_age_ms = 1200;
        config
    }

    fn quote() -> LiveQuoteSnapshot {
        LiveQuoteSnapshot {
            bid: 0.48,
            ask: 0.52,
            bids: vec![BookLevel {
                price: 0.48,
                size: 100.0,
            }],
            asks: vec![BookLevel {
                price: 0.52,
                size: 100.0,
            }],
            age_ms: 100,
        }
    }

    #[test]
    fn wallet_kind_parse_rejects_invalid_values() {
        assert!(matches!(WalletKind::parse("eoa"), Ok(WalletKind::Eoa)));
        assert!(matches!(
            WalletKind::parse("gnosis"),
            Ok(WalletKind::Gnosis)
        ));
        assert!(WalletKind::parse("bad").is_err());
    }

    #[test]
    fn validate_markets_rejects_missing_live_metadata() {
        let mut market = sample_market();
        market.up_token_id.clear();
        assert!(validate_markets(&[market]).is_err());
        assert!(validate_markets(&[]).is_err());
        assert!(validate_markets(&[sample_market()]).is_ok());
    }

    #[test]
    fn live_submit_guard_rejects_stale_quotes() {
        let config = test_config();
        let mut live_quote = quote();
        live_quote.age_ms = 5_000;

        let err = validate_live_submit_against_quote(
            &config,
            &live_quote,
            LiveSubmitSide::Open,
            0.50,
            Some(0.04),
            10.0,
            1,
            0.0,
        )
        .expect_err("stale quote should fail");

        assert!(err.starts_with("LIVE_QUOTE_STALE"));
    }

    #[test]
    fn live_submit_guard_rejects_widened_spread() {
        let config = test_config();
        let mut live_quote = quote();
        live_quote.bid = 0.40;
        live_quote.ask = 0.52;

        let err = validate_live_submit_against_quote(
            &config,
            &live_quote,
            LiveSubmitSide::Open,
            0.50,
            Some(0.04),
            10.0,
            1,
            0.0,
        )
        .expect_err("widened spread should fail");

        assert!(err.starts_with("LIVE_SPREAD_WIDENED"));
    }

    #[test]
    fn live_submit_guard_rejects_notional_over_cap() {
        let config = test_config();

        let err = validate_live_submit_against_quote(
            &config,
            &quote(),
            LiveSubmitSide::Open,
            0.50,
            Some(0.04),
            25.0,
            1,
            0.0,
        )
        .expect_err("notional cap should fail");

        assert!(err.starts_with("LIVE_MAX_ORDER_USDC_EXCEEDED"));
    }

    #[test]
    fn live_submit_guard_rejects_missing_entry_depth() {
        let config = test_config();
        let mut live_quote = quote();
        live_quote.asks.clear();

        let err = validate_live_submit_against_quote(
            &config,
            &live_quote,
            LiveSubmitSide::Open,
            0.50,
            Some(0.04),
            10.0,
            1,
            0.0,
        )
        .expect_err("missing depth should fail");

        assert!(err.starts_with("LIVE_ASK_DEPTH_EMPTY"));
    }

    #[test]
    fn live_submit_guard_accepts_valid_entry_quote() {
        let config = test_config();

        validate_live_submit_against_quote(
            &config,
            &quote(),
            LiveSubmitSide::Open,
            0.50,
            Some(0.04),
            10.0,
            1,
            0.0,
        )
        .expect("valid live quote should pass");
    }
}
