use std::collections::HashMap;
use std::str::FromStr;
use std::time::Instant;

use alloy::primitives::U256 as AlloyU256;
use alloy::providers::ProviderBuilder;
use alloy::sol;
use chrono::Datelike;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{LocalSigner, Normal, Signer as _};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::{Address, Decimal, U256, address};
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::execution::paper::{OpenPosition, TradeRecord};

// Polygon mainnet contract addresses (from Polymarket docs + reference bot)
const USDC:           Address = address!("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const CTF:            Address = address!("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");
const EXCHANGE:       Address = address!("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
const NEG_RISK_EXCHANGE: Address = address!("0xC5d563A36AE78145C45a50134d48A1215220f80a");
const NEG_RISK_ADAPTER:  Address = address!("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296");

fn rpc_url() -> String {
    std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://rpc.ankr.com/polygon".to_string())
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

/// Check and set required on-chain approvals — view calls are FREE (no gas)
/// Only approve() and setApprovalForAll() cost gas, and only on first run
pub async fn ensure_approvals(signer: &K256Signer) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .connect(&rpc_url())
        .await?;

    let owner = signer.address();

    // Check POL balance first — write txs need gas
    use alloy::providers::Provider;
    let pol_balance = provider.get_balance(owner).await.unwrap_or_default();
    let pol_f64 = pol_balance.to::<u128>() as f64 / 1e18;
    if pol_f64 < 0.001 {
        return Err(format!(
            "Insufficient POL for gas: {:.6} POL. Send at least 0.1 POL to {} to pay for approval transactions.",
            pol_f64, owner
        ).into());
    }
    info!(pol_balance = pol_f64, "POL balance sufficient for approvals");

    let usdc = IERC20::new(USDC, provider.clone());
    let ctf = IERC1155::new(CTF, provider.clone());

    // All contracts that need USDC + CTF approval
    let targets = [
        ("Exchange",         EXCHANGE),
        ("Neg Risk Exchange", NEG_RISK_EXCHANGE),
        ("Neg Risk Adapter",  NEG_RISK_ADAPTER),
    ];

    for (name, target) in &targets {
        // allowance() is a view call — zero gas, zero cost
        let allowance = usdc.allowance(owner, *target).call().await?;
        if allowance < AlloyU256::from(1_000_000u64) {
            info!(contract = name, "Approving USDC (one-time gas cost)...");
            usdc.approve(*target, AlloyU256::MAX).send().await?.watch().await?;
            info!(contract = name, "USDC approved");
        } else {
            info!(contract = name, "USDC already approved (no gas)");
        }

        // isApprovedForAll() is a view call — zero gas, zero cost
        let approved = ctf.isApprovedForAll(owner, *target).call().await?;
        if !approved {
            info!(contract = name, "Approving CTF (one-time gas cost)...");
            ctf.setApprovalForAll(*target, true).send().await?.watch().await?;
            info!(contract = name, "CTF approved");
        } else {
            info!(contract = name, "CTF already approved (no gas)");
        }
    }

    info!("All approvals confirmed");
    Ok(())
}

/// Mirrors PaperWallet exactly but executes real CLOB orders
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
}

impl LiveWallet {
    pub async fn new(config: AppConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let private_key = std::env::var(PRIVATE_KEY_VAR)
            .map_err(|_| "POLYMARKET_PRIVATE_KEY not set in .env")?;

        let signer = K256Signer::from_str(&private_key)?
            .with_chain_id(Some(POLYGON));

        let wallet_address = format!("{:?}", signer.address());
        info!(eoa_address = %wallet_address, wallet_type = %std::env::var("WALLET_TYPE").unwrap_or("gnosis".into()), "Live wallet");

        // Authenticate — use GnosisSafe for browser wallet (polymarket.com account)
        // or Eoa for standalone EOA wallet. Set WALLET_TYPE=eoa in .env for EOA.
        let clob_config = Config::builder().use_server_time(true).build();
        let wallet_type = std::env::var("WALLET_TYPE").unwrap_or_else(|_| "gnosis".to_string());
        let clob = {
            use polymarket_client_sdk::clob::types::SignatureType;
            let builder = Client::new("https://clob.polymarket.com", clob_config)?
                .authentication_builder(&signer);
            if wallet_type.to_lowercase() == "eoa" {
                builder.signature_type(SignatureType::Eoa).authenticate().await?
            } else {
                // GnosisSafe: SDK auto-derives the proxy wallet address via CREATE2
                builder.signature_type(SignatureType::GnosisSafe).authenticate().await?
            }
        };

        // Validate API credentials work
        match clob.api_keys().await {
            Ok(_) => info!("API credentials validated"),
            Err(e) => warn!("API credential validation failed: {} — trading may not work", e),
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
        })
    }

    pub async fn ensure_approvals(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        ensure_approvals(&self.signer).await
    }

    /// Sync real balance + validate open positions against CLOB
    pub async fn sync_from_clob(&mut self) {
        // Real balance — ground truth
        if let Ok(b) = self.clob.balance_allowance(Default::default()).await {
            if let Ok(raw) = f64::try_from(b.balance) {
                self.balance = raw / 1_000_000.0;
            }
        }

        // Check open orders — remove phantom positions (unfilled/cancelled buys)
        use polymarket_client_sdk::clob::types::request::OrdersRequest;
        use polymarket_client_sdk::clob::types::OrderStatusType;

        if let Ok(orders) = self.clob.orders(&OrdersRequest::default(), None).await {
            let live_ids: std::collections::HashSet<String> = orders.data.iter()
                .filter(|o| matches!(o.status, OrderStatusType::Live | OrderStatusType::Matched))
                .map(|o| o.id.to_string())
                .collect();

            self.open_positions.retain(|pos| {
                let key = format!("{}:{}", pos.symbol, pos.direction);
                match self.pending_order_ids.get(&key) {
                    Some(id) => live_ids.contains(id),
                    None => true, // no pending order = already filled
                }
            });
        }
    }

    /// Auto-redeem winning tokens after market resolution (on-chain tx, costs gas)
    pub async fn redeem_resolved_positions(&self, condition_id: &str) {
        use polymarket_client_sdk::ctf::Client as CtfClient;
        use polymarket_client_sdk::ctf::types::RedeemPositionsRequest;
        use alloy::primitives::B256;

        let provider = match ProviderBuilder::new().wallet(self.signer.clone()).connect(&rpc_url()).await {
            Ok(p) => p,
            Err(e) => { error!("RPC connect failed for redeem: {}", e); return; }
        };

        let ctf_client = match CtfClient::new(provider, POLYGON) {
            Ok(c) => c,
            Err(e) => { error!("CTF client init failed: {}", e); return; }
        };

        let condition_id_bytes = match B256::from_str(condition_id) {
            Ok(b) => b,
            Err(e) => { error!("Invalid condition_id {}: {}", condition_id, e); return; }
        };

        // Redeem both index sets — CTF only pays out the winning one
        let request = RedeemPositionsRequest::builder()
            .collateral_token(USDC)
            .parent_collection_id(B256::ZERO)
            .condition_id(condition_id_bytes)
            .index_sets(vec![AlloyU256::from(1), AlloyU256::from(2)])
            .build();

        match ctf_client.redeem_positions(&request).await {
            Ok(r) => info!(condition_id=%condition_id, tx=%r.transaction_hash, "Positions redeemed"),
            Err(e) => error!(condition_id=%condition_id, error=%e, "Redeem failed"),
        }
    }

    pub fn register_tokens(&mut self, up_token: &str, down_token: &str, symbol: &str) {
        self.token_map.insert(up_token.to_string(), (symbol.to_string(), "UP".to_string()));
        self.token_map.insert(down_token.to_string(), (symbol.to_string(), "DOWN".to_string()));
    }

    pub fn clear_tokens(&mut self, symbol: &str) {
        self.token_map.retain(|_, (s, _)| s != symbol);
    }

    /// Update BTC trailing stop tracking for all open positions of a symbol
    pub fn update_btc_trailing(&mut self, symbol: &str, current_btc: f64) {
        for pos in &mut self.open_positions {
            if pos.symbol != symbol { continue; }
            
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

    fn get_token_id(&self, symbol: &str, direction: &str) -> Option<U256> {
        self.token_map.iter()
            .find(|(_, (s, d))| s == symbol && d == direction)
            .and_then(|(id, _)| U256::from_str(id).ok())
    }

    fn round_to_tick(price: f64, tick: f64) -> f64 {
        (price / tick).round() * tick
    }

    async fn post_with_retry(
        clob: &AuthClient,
        signer: &K256Signer,
        token_id: U256,
        price: Decimal,
        shares: Decimal,
        side: Side,
    ) -> Result<String, String> {
        use polymarket_client_sdk::clob::types::Amount;
        for attempt in 0..3u32 {
            let order = clob.market_order()
                .token_id(token_id)
                .amount(Amount::shares(shares).map_err(|e| e.to_string())?)
                .price(price)
                .side(side.clone())
                .order_type(OrderType::FOK)
                .build()
                .await
                .map_err(|e| e.to_string())?;

            let signed = clob.sign(signer, order).await.map_err(|e| e.to_string())?;

            match clob.post_order(signed).await {
                Ok(r) if r.success => {
                    info!(order_id=%r.order_id, status=%r.status, "Order confirmed by CLOB");
                    return Ok(r.order_id.to_string());
                },
                Ok(r) => {
                    let msg = r.error_msg.as_ref()
                        .map(|e| format!("CLOB rejected: {}", e))
                        .unwrap_or_else(|| format!("Order failed: success={}, status={:?}", r.success, r.status));
                    warn!(attempt=attempt+1, error=%msg, "Order attempt failed");
                    if attempt == 2 { return Err(msg); }
                    tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt + 1) as u64)).await;
                }
                Err(e) => {
                    let msg = format!("Network error: {}", e);
                    warn!(attempt=attempt+1, error=%msg, "Order network error");
                    if attempt == 2 { return Err(msg); }
                    tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt + 1) as u64)).await;
                }
            }
        }
        Err("Max retries exceeded".to_string())
    }

    /// Open a live position — matches paper.rs logic exactly
    pub async fn open_position(
        &mut self,
        symbol: &str,
        direction: &str,
        spike: f64,
        _threshold_usd: f64,
        allow_scaling: bool,
        current_btc: f64,
    ) -> Result<u32, String> {
        // Reset daily PnL at midnight UTC
        let today = chrono::Utc::now().date_naive().day();
        if today != self.last_reset_day {
            self.daily_pnl = 0.0;
            self.last_reset_day = today;
        }

        // EMERGENCY STOP: If balance drops below $2, refuse new positions
        if self.balance < 2.0 {
            return Err("EMERGENCY_STOP_BALANCE_TOO_LOW".to_string());
        }

        // DRAWDOWN CHECK: Stop if drawdown exceeds threshold
        let drawdown = (self.starting_balance - self.balance) / self.starting_balance;
        if drawdown > self.config.max_drawdown_pct {
            return Err(format!("DRAWDOWN_EXCEEDED: {:.1}% > {:.1}%", drawdown * 100.0, self.config.max_drawdown_pct * 100.0));
        }

        // DAILY LOSS LIMIT: Stop if daily losses exceed threshold
        if self.daily_pnl < -self.config.max_daily_loss {
            return Err(format!("DAILY_LOSS_LIMIT: ${:.2} < -${:.2}", self.daily_pnl, self.config.max_daily_loss));
        }

        // RATE LIMITING: Check orders per minute
        let now = std::time::Instant::now();
        self.order_timestamps.retain(|t| now.duration_since(*t).as_secs() < 60);
        if self.order_timestamps.len() >= self.config.max_orders_per_minute as usize {
            return Err(format!("RATE_LIMIT: {} orders/min exceeded", self.config.max_orders_per_minute));
        }

        // PER-MARKET EXPOSURE: Check total exposure in this market
        let market_exposure: f64 = self.open_positions.iter()
            .filter(|p| p.symbol == symbol)
            .map(|p| p.position_size)
            .sum();
        let position_size_planned = self.balance * self.config.portfolio_pct;
        if market_exposure + position_size_planned > self.config.max_exposure_per_market {
            return Err(format!("MARKET_EXPOSURE_LIMIT: ${:.2} + ${:.2} > ${:.2}", 
                market_exposure, position_size_planned, self.config.max_exposure_per_market));
        }

        // SCALE LEVEL CHECK - matches paper.rs exactly
        let existing: Vec<_> = self.open_positions.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let scale_level = (existing.len()) as u32 + 1;

        // In HOLD mode (allow_scaling=true), ignore MAX_SCALE_LEVEL to add to winning positions
        if !allow_scaling && scale_level > 1 { 
            return Err("MAX_SCALE_LEVEL".to_string()); 
        }

        let token_id = self.get_token_id(symbol, direction)
            .ok_or_else(|| "NO_TOKEN_ID".to_string())?;

        // Verify orderbook exists before placing order
        use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
        let book_req = OrderBookSummaryRequest::builder()
            .token_id(token_id)
            .build();
        
        // Get entry price from orderbook - matches paper.rs logic
        let entry_price = match self.clob.order_book(&book_req).await {
            Ok(book) => {
                // Check if market is active (has bids/asks)
                if book.bids.is_empty() && book.asks.is_empty() {
                    return Err("MARKET_INACTIVE_NO_LIQUIDITY".to_string());
                }
                
                // Calculate mid price from best bid/ask
                let best_bid: f64 = book.bids.first()
                    .and_then(|b| f64::try_from(b.price).ok())
                    .unwrap_or(0.0);
                let best_ask: f64 = book.asks.first()
                    .and_then(|a| f64::try_from(a.price).ok())
                    .unwrap_or(0.0);
                
                if best_bid <= 0.0 || best_ask <= 0.0 {
                    return Err("NO_PRICE_DATA".to_string());
                }
                
                (best_bid + best_ask) / 2.0
            },
            Err(e) => {
                // Orderbook doesn't exist - market likely expired/resolved
                return Err(format!("MARKET_CLOSED: {}", e));
            }
        };

        // ENTRY PRICE CHECKS - matches paper.rs exactly
        if entry_price <= 0.0 { return Err("NO_PRICE_DATA".to_string()); }
        if entry_price > self.config.max_entry_price { return Err("PRICE_TOO_HIGH".to_string()); }
        if entry_price < self.config.min_entry_price { return Err("PRICE_TOO_LOW".to_string()); }

        // POSITION SIZING - matches paper.rs exactly
        let position_size = self.balance * self.config.portfolio_pct * (1.0 / scale_level as f64);
        
        // Position sizing based on share price - scale down for cheaper shares
        // Cheap shares (< 20 cents) are higher risk, use smaller position size
        let position_size = if entry_price < 0.20 {
            position_size * 0.25  // 25% of normal for very cheap shares
        } else if entry_price < 0.50 {
            position_size * 0.50  // 50% of normal for cheap shares
        } else {
            position_size  // Full size for 50+ cent shares
        };
        
        let shares = position_size / entry_price;
        let buy_fee = shares * self.config.crypto_fee_rate * entry_price * (1.0 - entry_price);

        // MINIMUM ORDER SIZE CHECK - matches paper.rs
        if position_size < 1.0 { return Err("BELOW_MIN_ORDER_SIZE".to_string()); }
        if (position_size + buy_fee) > self.balance { return Err("INSUFFICIENT_BALANCE".to_string()); }

        // ADDITIONAL SAFETY CHECKS for live trading
        // These are extra protections beyond paper trading
        if entry_price < 0.05 {
            return Err(format!("CRITICAL_SAFETY: Entry price {:.3} < 0.05 (5 cents minimum)", entry_price));
        }
        if position_size > self.balance * 0.25 { 
            return Err(format!("CRITICAL_SAFETY: Position ${:.2} > 25% of balance ${:.2}", position_size, self.balance)); 
        }
        if position_size > 10.0 { 
            return Err(format!("CRITICAL_SAFETY: Position ${:.2} > $10 maximum per trade", position_size)); 
        }

        let tick = 0.01f64;
        let rounded_price = Self::round_to_tick(entry_price, tick);
        let rounded_shares = ((shares * 100.0).floor() / 100.0).max(5.0); // enforce min 5 shares
        let price = Decimal::try_from(rounded_price).map_err(|e| e.to_string())?;
        let size = Decimal::try_from(rounded_shares).map_err(|e| e.to_string())?;

        let order_id = Self::post_with_retry(&self.clob, &self.signer, token_id, price, size, Side::Buy).await?;
        info!(symbol=%symbol, direction=%direction, shares=rounded_shares, price=rounded_price, order_id=%order_id, "Live BUY placed");

        // Track order timestamp for rate limiting
        self.order_timestamps.push(std::time::Instant::now());

        self.pending_order_ids.insert(format!("{}:{}", symbol, direction), order_id);
        
        // Reserve balance immediately - matches paper.rs logic
        self.balance -= position_size + buy_fee;

        self.trade_history.push(TradeRecord {
            symbol: symbol.to_string(),
            r#type: "entry".to_string(),
            question: String::new(),
            direction: direction.to_string(),
            entry_price: Some(entry_price),
            exit_price: None,
            shares,
            cost: position_size,
            pnl: None,
            cumulative_pnl: Some(self.cumulative_pnl),
            balance_after: Some(self.balance),
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: None,
        });

        self.open_positions.push(OpenPosition {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price,
            avg_entry_price: entry_price,
            shares: rounded_shares,
            position_size,
            buy_fee,
            entry_spike: spike,
            entry_time: Instant::now(),
            highest_price: entry_price,
            scale_level,
            hold_to_resolution: false,
            peak_spike: spike.abs(),
            // BTC price set immediately at entry to avoid race condition - matches paper.rs
            entry_btc: current_btc,
            peak_btc: current_btc,
            trough_btc: current_btc,
            spike_faded_since: None,
            trend_reversed_since: None,
        });

        Ok(scale_level)
    }

    pub async fn close_position_at(&mut self, idx: usize, current_price: f64, reason: &str) {
        if idx >= self.open_positions.len() { return; }
        let pos = self.open_positions.remove(idx);

        // Cancel pending buy if still open
        let key = format!("{}:{}", pos.symbol, pos.direction);
        if let Some(order_id) = self.pending_order_ids.remove(&key) {
            if let Err(e) = self.clob.cancel_order(&order_id).await {
                warn!("Could not cancel buy order {}: {}", order_id, e);
            }
        }

        let token_id = match self.get_token_id(&pos.symbol, &pos.direction) {
            Some(id) => id,
            None => { error!("No token ID for {} {}", pos.symbol, pos.direction); return; }
        };

        let tick = 0.01f64;
        let rounded = Self::round_to_tick(current_price, tick);
        let price = match Decimal::try_from(rounded) {
            Ok(p) => p,
            Err(e) => { error!("Invalid sell price: {}", e); return; }
        };
        let size = match Decimal::try_from((pos.shares * 100.0).floor() / 100.0) {            Ok(s) => s,
            Err(e) => { error!("Invalid sell size: {}", e); return; }
        };

        match Self::post_with_retry(&self.clob, &self.signer, token_id, price, size, Side::Sell).await {
            Ok(id) => {
                info!(symbol=%pos.symbol, reason=%reason, price=rounded, order_id=%id, "Live SELL placed");
                // TODO: Add order status tracking to confirm actual fill price/size
            },
            Err(e) => error!("Sell order failed: {}", e),
        }

        // Sell is maker (limit) — no fee
        let net_revenue = pos.shares * current_price;
        let pnl = net_revenue - (pos.position_size + pos.buy_fee);

        self.balance += net_revenue;
        self.cumulative_pnl += pnl;
        self.daily_pnl += pnl;
        self.trade_count += 1;
        if pnl > 0.0 { self.wins += 1; } else { self.losses += 1; }
        self.total_fees_paid += pos.buy_fee;
        self.total_volume += pos.position_size + (pos.shares * current_price);

        self.trade_history.push(TradeRecord {
            symbol: pos.symbol.clone(),
            r#type: "exit".to_string(),
            question: String::new(),
            direction: pos.direction.clone(),
            entry_price: Some(pos.entry_price),
            exit_price: Some(current_price),
            shares: pos.shares,
            cost: pos.position_size,
            pnl: Some(pnl),
            cumulative_pnl: Some(self.cumulative_pnl),
            balance_after: Some(self.balance),
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: Some(reason.to_string()),
        });
    }
}
