use std::collections::HashMap;
use std::str::FromStr;
use std::time::Instant;

use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{LocalSigner, Normal, Signer as _};
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::{Decimal, U256};
use polymarket_client_sdk::{POLYGON, PRIVATE_KEY_VAR};
use tracing::{error, info};
use crate::config::AppConfig;
use crate::execution::paper::{OpenPosition, TradeRecord};

type K256Signer = LocalSigner<k256::ecdsa::SigningKey>;
type AuthClient = Client<Authenticated<Normal>>;

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
    config: AppConfig,
    clob: AuthClient,
    signer: K256Signer,
    token_map: HashMap<String, (String, String)>, // token_id -> (symbol, direction)
}

impl LiveWallet {
    pub async fn new(config: AppConfig) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let private_key = std::env::var(PRIVATE_KEY_VAR)
            .map_err(|_| "POLYMARKET_PRIVATE_KEY not set in .env")?;

        let signer = K256Signer::from_str(&private_key)?
            .with_chain_id(Some(POLYGON));

        let clob_config = Config::builder().use_server_time(true).build();
        let clob = Client::new("https://clob.polymarket.com", clob_config)?
            .authentication_builder(&signer)
            .authenticate()
            .await?;

        info!("Live wallet authenticated with Polymarket CLOB");

        Ok(Self {
            balance: config.starting_balance,
            starting_balance: config.starting_balance,
            trade_count: 0,
            wins: 0,
            losses: 0,
            total_fees_paid: 0.0,
            total_volume: 0.0,
            open_positions: Vec::new(),
            trade_history: Vec::new(),
            config,
            clob,
            signer,
            token_map: HashMap::new(),
        })
    }

    pub fn register_tokens(&mut self, up_token: &str, down_token: &str, symbol: &str) {
        self.token_map.insert(up_token.to_string(), (symbol.to_string(), "UP".to_string()));
        self.token_map.insert(down_token.to_string(), (symbol.to_string(), "DOWN".to_string()));
    }

    pub fn clear_tokens(&mut self, symbol: &str) {
        self.token_map.retain(|_, (s, _)| s != symbol);
    }

    fn get_token_id(&self, symbol: &str, direction: &str) -> Option<U256> {
        self.token_map.iter()
            .find(|(_, (s, d))| s == symbol && d == direction)
            .and_then(|(id, _)| U256::from_str(id).ok())
    }

    pub async fn open_position(
        &mut self,
        symbol: &str,
        direction: &str,
        binance: f64,
        chainlink: f64,
        spike: f64,
        entry_price: f64,
        threshold_usd: f64,
    ) -> Result<u32, String> {
        let existing: Vec<_> = self.open_positions.iter()
            .filter(|p| p.symbol == symbol && p.direction == direction)
            .collect();
        let scale_level = existing.len() as u32 + 1;

        if scale_level > 3 { return Err("MAX_SCALE_LEVEL".to_string()); }
        if entry_price <= 0.0 { return Err("NO_PRICE_DATA".to_string()); }
        if entry_price > self.config.max_entry_price { return Err("PRICE_TOO_HIGH".to_string()); }

        let token_id = self.get_token_id(symbol, direction)
            .ok_or_else(|| "NO_TOKEN_ID".to_string())?;

        let position_size = self.balance * self.config.portfolio_pct * (1.0 / scale_level as f64);
        let shares = position_size / entry_price;
        // Buy is taker — fee: C * 0.072 * p * (1-p)
        let buy_fee = shares * self.config.crypto_fee_rate * entry_price * (1.0 - entry_price);

        if (position_size + buy_fee) > self.balance {
            return Err("INSUFFICIENT_BALANCE".to_string());
        }

        let price = Decimal::try_from(entry_price).map_err(|e| e.to_string())?;
        let size = Decimal::try_from(shares).map_err(|e| e.to_string())?;

        let order = self.clob
            .limit_order()
            .token_id(token_id)
            .price(price)
            .size(size)
            .side(Side::Buy)
            .order_type(OrderType::GTC)
            .build()
            .await
            .map_err(|e| e.to_string())?;

        let signed = self.clob.sign(&self.signer, order).await.map_err(|e| e.to_string())?;
        let response = self.clob.post_order(signed).await.map_err(|e| e.to_string())?;

        info!(symbol=%symbol, direction=%direction, shares=shares, price=entry_price,
              order_id=%response.order_id, success=%response.success, "Live BUY order placed");

        self.balance -= position_size + buy_fee;

        let spike_bonus = (spike.abs() - threshold_usd) * self.config.spike_scaling_factor;
        let profit_target = entry_price * (1.0 + self.config.profit_target_pct + spike_bonus);

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
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: None,
        });

        self.open_positions.push(OpenPosition {
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            entry_price,
            shares,
            position_size,
            buy_fee,
            entry_spike: spike,
            entry_time: Instant::now(),
            highest_price: entry_price,
            scale_level,
            hold_to_resolution: false,
        });

        Ok(scale_level)
    }

    pub async fn close_position_at(&mut self, idx: usize, current_price: f64, reason: &str) {
        if idx >= self.open_positions.len() { return; }
        let pos = self.open_positions.remove(idx);

        let token_id = match self.get_token_id(&pos.symbol, &pos.direction) {
            Some(id) => id,
            None => {
                error!("No token ID for {} {}, cannot place sell order", pos.symbol, pos.direction);
                return;
            }
        };

        let price = match Decimal::try_from(current_price) {
            Ok(p) => p,
            Err(e) => { error!("Invalid sell price: {}", e); return; }
        };
        let size = match Decimal::try_from(pos.shares) {
            Ok(s) => s,
            Err(e) => { error!("Invalid sell size: {}", e); return; }
        };

        match self.clob.limit_order()
            .token_id(token_id)
            .price(price)
            .size(size)
            .side(Side::Sell)
            .order_type(OrderType::GTC)
            .build()
            .await
        {
            Ok(order) => match self.clob.sign(&self.signer, order).await {
                Ok(signed) => match self.clob.post_order(signed).await {
                    Ok(r) => info!(symbol=%pos.symbol, reason=%reason, price=current_price,
                                   order_id=%r.order_id, success=%r.success, "Live SELL order placed"),
                    Err(e) => error!("Failed to post sell order: {}", e),
                },
                Err(e) => error!("Failed to sign sell order: {}", e),
            },
            Err(e) => error!("Failed to build sell order: {}", e),
        }

        // Sell is maker (limit) — no fee per Polymarket docs
        let net_revenue = pos.shares * current_price;
        let pnl = net_revenue - (pos.position_size + pos.buy_fee);

        self.balance += net_revenue;
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
            timestamp: chrono::Local::now().to_rfc3339(),
            close_reason: Some(reason.to_string()),
        });
    }
}
