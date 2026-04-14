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
/// Track in-flight orders to prevent race conditions
#[derive(Clone)]
struct PendingOrder {
    symbol: String,
    direction: String,
    submitted_at: std::time::Instant,
}

/// Cache share prices from WebSocket (matches paper.rs)
#[derive(Clone, Default)]
struct SymbolPriceCache {
    pub up_bid: f64,
    pub up_ask: f64,
    pub down_bid: f64,
    pub down_ask: f64,
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
    pending_orders: Vec<PendingOrder>,            // Track in-flight orders to prevent race conditions
    price_cache: HashMap<String, SymbolPriceCache>, // Cache WebSocket prices (matches paper.rs)
    zero_balance_flags: HashMap<String, Instant>, // "symbol:direction" -> first zero-balance detection time
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
            pending_orders: Vec::new(),
            price_cache: HashMap::new(),
            zero_balance_flags: HashMap::new(),
        })
    }

    pub async fn ensure_approvals(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        ensure_approvals(&self.signer).await
    }

    /// Sync real balance + validate open positions against CLOB
    pub async fn sync_from_clob(&mut self) {
        // Force CLOB to refresh cached USDC balance, then query
        let _ = self.clob.update_balance_allowance(Default::default()).await;
        if let Ok(b) = self.clob.balance_allowance(Default::default()).await {
            if let Ok(raw) = f64::try_from(b.balance) {
                let new_balance = raw / 1_000_000.0;
                if (new_balance - self.balance).abs() > 0.01 {
                    info!(previous=self.balance, on_chain=new_balance, "USDC balance synced from CLOB");
                }
                self.balance = new_balance;
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
            if pos.entry_time.elapsed().as_secs() < 30 { continue; }

            let key = format!("{}:{}", pos.symbol, pos.direction);
            if self.pending_order_ids.contains_key(&key) { continue; } // Handled by order check above

            if let Some(token_id) = self.get_token_id(&pos.symbol, &pos.direction) {
                // Force CLOB to refresh its cached on-chain balance before querying
                let update_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Conditional)
                    .token_id(token_id.clone())
                    .build();
                let _ = self.clob.update_balance_allowance(update_req).await;

                // Now query the (hopefully fresh) balance
                let query_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
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
            let key = format!("{}:{}", pos.symbol, pos.direction);
            self.zero_balance_flags.remove(&key);
            let pnl = -(pos.position_size + pos.buy_fee); // Assume total loss (don't know sell price)
            self.cumulative_pnl += pnl;
            self.daily_pnl += pnl;
            self.trade_count += 1;
            self.losses += 1;
            self.total_fees_paid += pos.buy_fee;
            self.trade_history.push(TradeRecord {
                symbol: pos.symbol.clone(), r#type: "exit".to_string(), question: String::new(),
                direction: pos.direction.clone(), entry_price: Some(pos.entry_price),
                exit_price: Some(0.0), shares: pos.shares, cost: pos.position_size,
                pnl: Some(pnl), cumulative_pnl: Some(self.cumulative_pnl),
                balance_after: Some(self.balance), timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: Some("external_close".to_string()),
            });
            warn!(symbol=%pos.symbol, direction=%pos.direction, "Position removed — confirmed no shares on-chain (external close/resolution)");
        }

        // Clean up zero_balance_flags for positions that no longer exist
        self.zero_balance_flags.retain(|key, _| {
            self.open_positions.iter().any(|p| format!("{}:{}", p.symbol, p.direction) == *key)
        });
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

    /// Clear cached prices for a symbol (used during market transitions)
    pub fn clear_price_cache(&mut self, symbol: &str) {
        self.price_cache.remove(symbol);
    }

    /// Update share prices from WebSocket (matches paper.rs)
    pub fn update_share_price(&mut self, symbol: &str, direction: &str, bid: f64, ask: f64) {
        let cache = self.price_cache.entry(symbol.to_string()).or_default();
        if direction == "UP" {
            cache.up_bid = bid;
            cache.up_ask = ask;
        } else {
            cache.down_bid = bid;
            cache.down_ask = ask;
        }

        // Keep highest_price in sync on live positions (mirrors paper.rs behavior)
        let mid_price = (bid + ask) / 2.0;
        for pos in &mut self.open_positions {
            if pos.symbol == symbol && pos.direction == direction {
                if direction == "UP" {
                    if mid_price > pos.highest_price { pos.highest_price = mid_price; }
                } else {
                    if mid_price < pos.highest_price { pos.highest_price = mid_price; }
                }
            }
        }
    }

    /// Get bid/ask from cache (matches paper.rs)
    pub fn get_bid_ask(&self, symbol: &str, direction: &str) -> (f64, f64) {
        if let Some(cache) = self.price_cache.get(symbol) {
            if direction == "UP" {
                (cache.up_bid, cache.up_ask)
            } else {
                (cache.down_bid, cache.down_ask)
            }
        } else {
            (0.0, 0.0)
        }
    }

    /// Get share price from cache (matches paper.rs)
    pub fn get_share_price(&self, symbol: &str, direction: &str) -> f64 {
        let (bid, ask) = self.get_bid_ask(symbol, direction);
        if bid > 0.0 && ask > 0.0 {
            (bid + ask) / 2.0
        } else {
            0.0
        }
    }

    /// Clean up stale pending orders (called periodically by engine)
    pub fn cleanup_pending_orders(&mut self) {
        let before = self.pending_orders.len();
        let now = std::time::Instant::now();
        self.pending_orders.retain(|p| now.duration_since(p.submitted_at).as_secs() < 5);
        let after = self.pending_orders.len();
        if before > after {
            warn!("Cleaned up {} stale pending orders", before - after);
        }
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
        if tick <= 0.0 {
            price  // Return original price if tick is invalid
        } else {
            (price / tick).round() * tick
        }
    }

    async fn post_with_retry(
        clob: &AuthClient,
        signer: &K256Signer,
        token_id: U256,
        price: Decimal,
        shares: Decimal,
        usdc: Decimal,
        side: Side,
    ) -> Result<(String, Decimal, Decimal), String> {  // Returns (order_id, filled_shares, filled_usdc)
        use polymarket_client_sdk::clob::types::Amount;
        for attempt in 0..3u32 {
            // For market orders:
            // - BUY: amount is USDC to spend (pre-rounded to 2 decimals for FOK)
            // - SELL: amount is shares to sell
            let amount_result = match side {
                Side::Buy => Amount::usdc(usdc),     // Pre-rounded USDC amount for buys
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
            
            let order = clob.market_order()
                .token_id(token_id)
                .amount(amount)
                .price(price)
                .side(side.clone())
                .order_type(OrderType::FAK)  // FAK = Fill-And-Kill (fills as much as possible, cancels rest)
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
        if self.starting_balance > 0.0 {
            let drawdown = (self.starting_balance - self.balance) / self.starting_balance;
            if drawdown > self.config.max_drawdown_pct {
                return Err(format!("DRAWDOWN_EXCEEDED: {:.1}% > {:.1}%", drawdown * 100.0, self.config.max_drawdown_pct * 100.0));
            }
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
        // Clean up stale pending orders (older than 5 seconds = failed/stuck)
        let now = std::time::Instant::now();
        self.pending_orders.retain(|p| now.duration_since(p.submitted_at).as_secs() < 5);
        
        // Count existing positions + in-flight orders (matches paper.rs logic)
        let existing: Vec<_> = self.open_positions.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let pending_same = self.pending_orders.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .count();
        let scale_level = (existing.len() + pending_same) as u32 + 1;

        // In HOLD mode (allow_scaling=true), ignore MAX_SCALE_LEVEL to add to winning positions
        if !allow_scaling && scale_level > 1 { 
            // Log why we're rejecting - helps debug stuck pending orders
            if pending_same > 0 {
                warn!("MAX_SCALE_LEVEL rejected: {} existing positions, {} pending orders for {} {}", existing.len(), pending_same, symbol, direction);
            }
            return Err("MAX_SCALE_LEVEL".to_string()); 
        }

        // Mark this order as in-flight BEFORE any async operations to prevent race conditions
        let pending_marker = now;
        self.pending_orders.push(PendingOrder {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            submitted_at: pending_marker,
        });

        // Helper macro to clean up pending order on early return
        macro_rules! cleanup_and_return {
            ($err:expr) => {{
                self.pending_orders.retain(|p| !(p.symbol == symbol && p.direction == direction && p.submitted_at == pending_marker));
                return Err($err);
            }};
        }

        let token_id = match self.get_token_id(symbol, direction) {
            Some(id) => id,
            None => cleanup_and_return!("NO_TOKEN_ID".to_string()),
        };

        // Get entry price from WebSocket cache (matches paper.rs exactly)
        let entry_price = self.get_share_price(symbol, direction);

        // ENTRY PRICE CHECKS - matches paper.rs exactly
        if entry_price <= 0.0 { cleanup_and_return!("NO_PRICE_DATA".to_string()); }
        if entry_price > self.config.max_entry_price { cleanup_and_return!("PRICE_TOO_HIGH".to_string()); }
        if entry_price < self.config.min_entry_price { cleanup_and_return!("PRICE_TOO_LOW".to_string()); }

        // Position sizing based on share price tiers (matches paper.rs)
        // Cheaper shares = smaller position (higher risk), expensive shares = larger position
        let portfolio_pct = if entry_price < 0.20 {
            0.10  // 10% for very cheap shares
        } else if entry_price < 0.50 {
            0.15  // 15% for mid-range shares
        } else if entry_price < 0.75 {
            0.20  // 20% for higher-priced shares
        } else {
            self.config.portfolio_pct  // config default for 75c+
        };
        let position_size = self.balance * portfolio_pct * (1.0 / scale_level as f64);

        // Cap position size at $20
        let position_size = position_size.min(20.0);

        let shares = position_size / entry_price;
        let buy_fee = shares * self.config.crypto_fee_rate * entry_price * (1.0 - entry_price);

        // Preliminary balance check (will recheck after rounding)
        if (position_size + buy_fee) > self.balance { cleanup_and_return!("INSUFFICIENT_BALANCE".to_string()); }

        // ADDITIONAL SAFETY CHECKS for live trading
        if entry_price < 0.05 {
            cleanup_and_return!(format!("CRITICAL_SAFETY: Entry price {:.3} < 0.05 (5 cents minimum)", entry_price));
        }

        let tick = 0.01f64;
        
        // MARKET ORDER: For FAK buys, the price field is the worst-price limit (max we'll pay per share).
        // The SDK uses this price to calculate taker_amount (shares) and maker_amount (USDC) for the
        // on-chain order struct. Setting it too far from the actual ask causes mismatched amounts that
        // won't match resting orders on the book.
        //
        // Strategy: use the cached ask + 4 cents buffer. This means:
        // - We'll fill at the actual ask (or better)
        // - Max overpay is 4 cents per share above the cached ask
        // - If the ask has moved more than 4 cents, we don't chase it
        let (_bid, ask) = self.get_bid_ask(symbol, direction);

        // Validate ask price exists (if cache shows no ask, book is likely empty)
        if ask <= 0.0 {
            cleanup_and_return!("NO_ASK_PRICE".to_string());
        }

        // Max overpay: 4 cents above the current best ask, capped at valid price range
        let market_price = (ask + 0.04).min(0.99);

        info!(symbol=%symbol, direction=%direction, cached_ask=ask, limit_price=market_price,
              position_size=position_size, "Sending FAK buy order");
        
        let rounded_price = Self::round_to_tick(market_price, tick);
        
        // Round shares to 2 decimal places for Polymarket API
        // Polymarket requires: maker amount max 2 decimals, taker amount max 4 decimals
        let rounded_shares = (shares * 100.0).floor() / 100.0;
        
        // Skip if rounded to 0 shares
        if rounded_shares < 0.01 {
            cleanup_and_return!(format!("SHARES_TOO_SMALL: {:.4} shares rounds to 0", shares));
        }
        
        // Recalculate actual cost with rounded shares
        let actual_position_size = rounded_shares * entry_price;
        let actual_buy_fee = rounded_shares * self.config.crypto_fee_rate * entry_price * (1.0 - entry_price);
        
        // Check minimum order size ($1 minimum on Polymarket)
        if actual_position_size < 1.0 {
            cleanup_and_return!(format!("BELOW_MIN_ORDER_SIZE: ${:.2} < $1 minimum", actual_position_size));
        }
        
        // For FAK buys, the USDC amount is how much we want to SPEND, not shares × limit_price.
        // We want to spend our position_size worth of USDC at whatever price the book offers.
        // Round to 2 decimals as required by Polymarket API.
        let usdc_amount = (actual_position_size * 100.0).floor() / 100.0;
        
        // Final balance check with actual amounts
        if (actual_position_size + actual_buy_fee) > self.balance {
            cleanup_and_return!("INSUFFICIENT_BALANCE_AFTER_ROUNDING".to_string());
        }
        
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

        // Place REAL market order on Polymarket CLOB - uses ask+buffer to ensure fill
        let (order_id, filled_shares, filled_usdc) = match Self::post_with_retry(&self.clob, &self.signer, token_id, price, size, usdc, Side::Buy).await {
            Ok(result) => result,
            Err(e) => cleanup_and_return!(e),
        };
        
        // Use actual filled amounts (FAK may fill partially)
        let actual_shares: f64 = filled_shares.try_into().unwrap_or(rounded_shares);
        let actual_usdc: f64 = filled_usdc.try_into().unwrap_or(usdc_amount);

        // Guard against zero-fill FAK — don't create phantom positions
        // Note: balance has NOT been deducted yet (that happens at line 675), so we just clean up and exit
        if actual_shares <= 0.0 || actual_usdc <= 0.0 {
            self.pending_orders.retain(|p| !(p.symbol == symbol && p.direction == direction && p.submitted_at == pending_marker));
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
        let fee_in_shares = actual_shares * self.config.crypto_fee_rate * (1.0 - actual_entry_price);
        let estimated_net = actual_shares - fee_in_shares;

        // Try to get ACTUAL on-chain balance to use instead of the estimate.
        // Only accept it if it's within 5% of our estimate (rejects stale dust from old trades).
        let net_shares = {
            use polymarket_client_sdk::clob::types::AssetType;
            let update_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_id)
                .build();
            let _ = self.clob.update_balance_allowance(update_req).await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let query_req = polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_id)
                .build();
            match self.clob.balance_allowance(query_req).await {
                Ok(b) => {
                    let raw: f64 = b.balance.try_into().unwrap_or(0.0);
                    let on_chain = raw / 1_000_000.0;
                    // Sanity check: on-chain must be within 5% of our fee estimate
                    // This rejects stale dust from previous trades
                    if on_chain > estimated_net * 0.95 && on_chain < estimated_net * 1.05 {
                        info!(symbol=%symbol, estimated=estimated_net, on_chain=on_chain,
                              "Using verified on-chain balance as net shares");
                        on_chain
                    } else {
                        info!(symbol=%symbol, estimated=estimated_net, on_chain=on_chain,
                              "On-chain balance doesn't match estimate, using fee formula");
                        estimated_net
                    }
                }
                Err(_) => estimated_net,
            }
        };

        // Fee already absorbed into fewer shares — don't double-count in USDC accounting
        let final_buy_fee = 0.0;

        info!(symbol=%symbol, direction=%direction, cached_price=entry_price, fill_price=actual_entry_price,
              requested_shares=rounded_shares, gross_shares=actual_shares, net_shares=net_shares,
              filled_usdc=actual_usdc, order_id=%order_id, "Live BUY filled (FAK)");

        // Order successfully placed - remove from pending orders
        self.pending_orders.retain(|p| !(p.symbol == symbol && p.direction == direction && p.submitted_at == pending_marker));

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
            symbol: symbol.to_string(),
            r#type: "entry".to_string(),
            question: String::new(),
            direction: direction.to_string(),
            entry_price: Some(actual_entry_price),
            exit_price: None,
            shares: net_shares,  // Net shares after CLOB fee deduction
            cost: final_position_size,
            pnl: None,
            cumulative_pnl: Some(self.cumulative_pnl),
            balance_after: Some(self.balance),
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: None,
        });

        self.open_positions.push(OpenPosition {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price: actual_entry_price,
            avg_entry_price: actual_entry_price,
            shares: net_shares,  // Net shares after CLOB fee deduction
            position_size: final_position_size,
            buy_fee: final_buy_fee,
            entry_spike: spike,
            entry_time: Instant::now(),
            highest_price: actual_entry_price,
            scale_level,
            hold_to_resolution: false,
            peak_spike: spike.abs(),
            // BTC price set immediately at entry to avoid race condition - matches paper.rs
            entry_btc: current_btc,
            peak_btc: current_btc,
            trough_btc: current_btc,
            spike_faded_since: None,
            trend_reversed_since: None,
            trailing_stop_activated: false,
            on_chain_shares: None, // Set later by sync_from_clob with actual on-chain balance
        });

        Ok(scale_level)
    }

    pub async fn close_position_at(&mut self, idx: usize, _current_price: f64, reason: &str) {
        if idx >= self.open_positions.len() { return; }

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
                    symbol: pos.symbol.clone(), r#type: "exit".to_string(), question: String::new(),
                    direction: pos.direction.clone(), entry_price: Some(pos.entry_price),
                    exit_price: Some(0.0), shares: pos.shares, cost: pos.position_size,
                    pnl: Some(pnl), cumulative_pnl: Some(self.cumulative_pnl),
                    balance_after: Some(self.balance), timestamp: chrono::Local::now().to_rfc3339(),
                    close_reason: Some(format!("{}_no_token", reason)),
                });
                error!("No token ID for {} {} — recorded loss", pos.symbol, pos.direction);
                return;
            }
        };

        let (bid, _ask) = self.get_bid_ask(&pos.symbol, &pos.direction);
        let tick = 0.01f64;

        // If no bid price or position too small to sell, record as loss instead of silently dropping
        // For FAK sells, the price is the MINIMUM we'll accept per share.
        // Use bid - 4 cents as our worst acceptable price (matching the buy-side 4c buffer).
        // The CLOB fills at resting bid prices, so we typically get the actual bid.
        let market_price = if bid > 0.04 {
            (bid - 0.04).max(0.01)
        } else if bid > 0.0 {
            0.01  // Very low bid — accept anything to exit
        } else {
            0.0
        };

        let can_sell = bid > 0.0 && pos.shares * bid >= 1.0;  // Use bid for min order check

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
                symbol: pos.symbol.clone(), r#type: "exit".to_string(), question: String::new(),
                direction: pos.direction.clone(), entry_price: Some(pos.entry_price),
                exit_price: Some(0.0), shares: pos.shares, cost: pos.position_size,
                pnl: Some(pnl), cumulative_pnl: Some(self.cumulative_pnl),
                balance_after: Some(self.balance), timestamp: chrono::Local::now().to_rfc3339(),
                close_reason: Some(format!("{}_unsellable", reason)),
            });
            warn!("Position unsellable: {} {} — bid={:.4}, value=${:.2}", pos.symbol, pos.direction, bid, pos.shares * bid);
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
                    symbol: pos.symbol.clone(), r#type: "exit".to_string(), question: String::new(),
                    direction: pos.direction.clone(), entry_price: Some(pos.entry_price),
                    exit_price: Some(0.0), shares: pos.shares, cost: pos.position_size,
                    pnl: Some(pnl), cumulative_pnl: Some(self.cumulative_pnl),
                    balance_after: Some(self.balance), timestamp: chrono::Local::now().to_rfc3339(),
                    close_reason: Some(format!("{}_price_error", reason)),
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

            match Self::post_with_retry(&self.clob, &self.signer, token_id, price, size, usdc_for_sell, Side::Sell).await {
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
                    error!("Sell order failed after all attempts: {} — putting position back ({} {})",
                           e, pos.symbol, pos.direction);
                    self.open_positions.push(pos);
                    return;
                }
            }
        }

        let gross_revenue = if total_filled_usdc > 0.0 {
            total_filled_usdc
        } else {
            // No fills at all — put position back
            error!("All sell attempts got zero fills — putting position back ({} {})", pos.symbol, pos.direction);
            self.open_positions.push(pos);
            return;
        };

        // If we reach here, the sell succeeded — safe to calculate PnL
        let sell_fee = pos.shares * self.config.crypto_fee_rate * pos.entry_price * (1.0 - pos.entry_price);
        let net_revenue = gross_revenue - sell_fee;
        let pnl = net_revenue - (pos.position_size + pos.buy_fee);

        self.balance += net_revenue;
        self.cumulative_pnl += pnl;
        self.daily_pnl += pnl;
        self.trade_count += 1;
        if pnl > 0.0 { self.wins += 1; } else { self.losses += 1; }
        self.total_fees_paid += pos.buy_fee + sell_fee;
        self.total_volume += pos.position_size + gross_revenue;

        self.trade_history.push(TradeRecord {
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
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: Some(reason.to_string()),
        });
    }
}
