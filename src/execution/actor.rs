use std::time::Instant;

use tokio::sync::mpsc;
use tracing::warn;

use crate::execution::paper::{OpenPosition, TradeRecord};

use super::live::LiveWallet;

#[derive(Clone, Default)]
pub struct LiveWalletSnapshot {
    pub balance: f64,
    pub starting_balance: f64,
    pub wins: u64,
    pub losses: u64,
    pub total_fees_paid: f64,
    pub total_volume: f64,
    pub open_positions: Vec<OpenPosition>,
    pub trade_history: Vec<TradeRecord>,
    pub wallet_address: String,
    pub cumulative_pnl: f64,
    pub daily_pnl: f64,
}

#[derive(Clone)]
pub struct LiveOpenFill {
    pub level: u32,
    pub position: OpenPosition,
}

#[derive(Clone)]
pub struct LatencyTrace {
    pub operation: &'static str,
    pub symbol: String,
    pub direction: String,
    pub feed_received_at: Instant,
    pub decision_at: Instant,
    pub intent_sent_at: Instant,
    pub actor_received_at: Option<Instant>,
    pub submit_started_at: Option<Instant>,
    pub submit_finished_at: Option<Instant>,
}

pub enum LiveCommand {
    UpdateSharePrice {
        symbol: String,
        direction: String,
        best_bid: f64,
        best_ask: f64,
    },
    UpdateBtcTrailing {
        symbol: String,
        price: f64,
    },
    CleanupPendingOrders,
    ClearTokens {
        symbol: String,
    },
    ClearPriceCache {
        symbol: String,
    },
    RegisterTokens {
        up_token_id: String,
        down_token_id: String,
        symbol: String,
    },
    OpenPosition {
        symbol: String,
        direction: String,
        spike: f64,
        threshold_usd: f64,
        in_hold_mode: bool,
        current_btc: f64,
        latency_trace: LatencyTrace,
    },
    ClosePosition {
        symbol: String,
        direction: String,
        price: f64,
        reason: &'static str,
        latency_trace: LatencyTrace,
    },
    SyncFromClob,
    RedeemResolved {
        condition_id: String,
    },
    VerifyPositionBalance {
        symbol: String,
        direction: String,
    },
}

pub enum LiveEvent {
    Snapshot(LiveWalletSnapshot),
    OpenPositionResult {
        symbol: String,
        direction: String,
        spike: f64,
        result: Result<LiveOpenFill, String>,
        snapshot: LiveWalletSnapshot,
        latency_trace: LatencyTrace,
    },
    ClosePositionResult {
        symbol: String,
        direction: String,
        reason: &'static str,
        snapshot: LiveWalletSnapshot,
        latency_trace: LatencyTrace,
    },
}

#[derive(Clone)]
pub struct LiveExecutionHandle {
    tx: mpsc::Sender<LiveCommand>,
}

impl LiveExecutionHandle {
    pub async fn send(
        &self,
        command: LiveCommand,
    ) -> Result<(), mpsc::error::SendError<LiveCommand>> {
        self.tx.send(command).await
    }

    pub fn try_send(
        &self,
        command: LiveCommand,
    ) -> Result<(), mpsc::error::TrySendError<LiveCommand>> {
        self.tx.try_send(command)
    }
}

pub fn spawn_live_execution(
    mut wallet: LiveWallet,
) -> (LiveExecutionHandle, mpsc::Receiver<LiveEvent>) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel(4096);
    let (event_tx, event_rx) = mpsc::channel(256);
    let delayed_cmd_tx = cmd_tx.clone();

    tokio::spawn(async move {
        wallet.save_state().await;
        let _ = event_tx.send(LiveEvent::Snapshot(wallet.snapshot())).await;

        while let Some(command) = cmd_rx.recv().await {
            match command {
                LiveCommand::UpdateSharePrice {
                    symbol,
                    direction,
                    best_bid,
                    best_ask,
                } => {
                    wallet.update_share_price(&symbol, &direction, best_bid, best_ask);
                }
                LiveCommand::UpdateBtcTrailing { symbol, price } => {
                    wallet.update_btc_trailing(&symbol, price);
                }
                LiveCommand::CleanupPendingOrders => {
                    wallet.cleanup_pending_orders();
                }
                LiveCommand::ClearTokens { symbol } => {
                    wallet.clear_tokens(&symbol);
                }
                LiveCommand::ClearPriceCache { symbol } => {
                    wallet.clear_price_cache(&symbol);
                }
                LiveCommand::RegisterTokens {
                    up_token_id,
                    down_token_id,
                    symbol,
                } => {
                    wallet.register_tokens(&up_token_id, &down_token_id, &symbol);
                }
                LiveCommand::OpenPosition {
                    symbol,
                    direction,
                    spike,
                    threshold_usd,
                    in_hold_mode,
                    current_btc,
                    mut latency_trace,
                } => {
                    latency_trace.actor_received_at = Some(Instant::now());
                    latency_trace.submit_started_at = Some(Instant::now());
                    let result = wallet
                        .open_position(
                            &symbol,
                            &direction,
                            spike,
                            threshold_usd,
                            in_hold_mode,
                            current_btc,
                        )
                        .await
                        .and_then(|level| {
                            wallet
                                .open_positions
                                .last()
                                .cloned()
                                .map(|position| LiveOpenFill { level, position })
                                .ok_or_else(|| "LIVE_POSITION_MISSING_AFTER_FILL".to_string())
                        });
                    latency_trace.submit_finished_at = Some(Instant::now());

                    if result.is_ok() {
                        let verify_tx = delayed_cmd_tx.clone();
                        let verify_symbol = symbol.clone();
                        let verify_direction = direction.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                            let _ = verify_tx
                                .send(LiveCommand::VerifyPositionBalance {
                                    symbol: verify_symbol,
                                    direction: verify_direction,
                                })
                                .await;
                        });
                    }

                    wallet.save_state().await;
                    let snapshot = wallet.snapshot();
                    if event_tx
                        .send(LiveEvent::OpenPositionResult {
                            symbol,
                            direction,
                            spike,
                            result,
                            snapshot,
                            latency_trace,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                LiveCommand::ClosePosition {
                    symbol,
                    direction,
                    price,
                    reason,
                    mut latency_trace,
                } => {
                    latency_trace.actor_received_at = Some(Instant::now());
                    latency_trace.submit_started_at = Some(Instant::now());
                    if let Some(idx) = wallet
                        .open_positions
                        .iter()
                        .position(|pos| pos.symbol == symbol && pos.direction == direction)
                    {
                        wallet.close_position_at(idx, price, reason).await;
                    }
                    latency_trace.submit_finished_at = Some(Instant::now());

                    wallet.save_state().await;
                    if event_tx
                        .send(LiveEvent::ClosePositionResult {
                            symbol,
                            direction,
                            reason,
                            snapshot: wallet.snapshot(),
                            latency_trace,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                LiveCommand::SyncFromClob => {
                    wallet.sync_from_clob().await;
                    wallet.save_state().await;
                    if event_tx
                        .send(LiveEvent::Snapshot(wallet.snapshot()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                LiveCommand::RedeemResolved { condition_id } => {
                    wallet.redeem_resolved_positions(&condition_id).await;
                    wallet.save_state().await;
                    if event_tx
                        .send(LiveEvent::Snapshot(wallet.snapshot()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                LiveCommand::VerifyPositionBalance { symbol, direction } => {
                    wallet.verify_position_balance(&symbol, &direction).await;
                    wallet.save_state().await;
                    if event_tx
                        .send(LiveEvent::Snapshot(wallet.snapshot()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }

        warn!("Live execution task stopped");
    });

    (LiveExecutionHandle { tx: cmd_tx }, event_rx)
}
