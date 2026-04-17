export type Position = {
  symbol: string;
  cost: number;
  entry_price: number;
  current_price: number;
  shares: number;
  side: string;
  pnl: number;
  level: number;
  held_ms: number;
  hold_to_resolution: boolean;
};

export type Trade = {
  symbol: string;
  type: string;
  question: string;
  direction: string;
  entry_price?: number | null;
  exit_price?: number | null;
  shares: number;
  cost: number;
  pnl?: number | null;
  cumulative_pnl?: number | null;
  balance_after?: number | null;
  timestamp: string;
  close_reason?: string | null;
};

export type HistoryPoint = {
  t: string;
  v: number;
};

export type SignalEvent = {
  t: string;
  s: string;
  d: string;
  v: number;
  st: string;
  r?: string | null;
};

export type MarketSnapshot = {
  question: string;
  up_price: number;
  down_price: number;
  binance: number;
  chainlink: number;
  spike: number;
  ema_offset: number;
  price_to_beat?: number | null;
  end_ts?: number | null;
};

export type Config = {
  threshold_bps: number;
  portfolio_pct: number;
  crypto_fee_rate: number;
  max_entry_price: number;
  min_entry_price: number;
  trend_reversal_pct: number;
  trend_reversal_threshold: number;
  spike_faded_pct: number;
  spike_faded_ms: number;
  min_hold_ms: number;
  trailing_stop_pct: number;
  trailing_stop_activation: number;
  stop_loss_pct: number;
  hold_min_share_price: number;
  hold_safety_margin: number;
  early_exit_loss_pct: number;
  hold_margin_per_second: number;
  hold_max_seconds: number;
  hold_max_crossings: number;
  spike_sustain_ms: number;
  execution_delay_ms: number;
  min_price_distance: number;
  max_orders_per_minute: number;
  max_daily_loss: number;
  max_exposure_per_market: number;
  max_drawdown_pct: number;
  trend_filter_enabled: boolean;
  trend_min_magnitude_usd: number;
  counter_trend_multiplier: number;
  trend_max_magnitude_usd: number;
  ptb_neutral_zone_usd: number;
  ptb_max_counter_distance_usd: number;
};

export type TerminalSnapshot = {
  balance: number;
  starting_balance: number;
  total_portfolio_value: number;
  unrealized_pnl: number;
  realized_pnl: number;
  total_pnl: number;
  cumulative_pnl: number;
  daily_pnl: number;
  wins: number;
  losses: number;
  total_fees: number;
  total_volume: number;
  positions: Position[];
  trades: Trade[];
  history: HistoryPoint[];
  signals: SignalEvent[];
  markets: Record<string, MarketSnapshot>;
  wallet_address?: string | null;
  running: boolean;
  runtime_ms: number;
  is_live: boolean;
  config: Config;
};

export type FastSnapshotMessage = {
  _kind: "fast";
  snapshot: Partial<Omit<TerminalSnapshot, "trades" | "history" | "config" | "wallet_address">>;
};

export type SlowSnapshotMessage = {
  _kind: "slow";
  snapshot: Pick<TerminalSnapshot, "trades" | "history" | "config" | "wallet_address">;
};

export type SnapshotMessage = FastSnapshotMessage | SlowSnapshotMessage;

export type FeedStatus = "connecting" | "live" | "offline";

export type CommandPayload =
  | { _type: "start" }
  | { _type: "stop" }
  | { _type: "reset" }
  | { _type: "close_position"; index: number };

export type ConfigField =
  | keyof Pick<
      Config,
      | "threshold_bps"
      | "portfolio_pct"
      | "crypto_fee_rate"
      | "max_entry_price"
      | "min_entry_price"
      | "trend_reversal_pct"
      | "trend_reversal_threshold"
      | "spike_faded_pct"
      | "spike_faded_ms"
      | "min_hold_ms"
      | "trailing_stop_pct"
      | "trailing_stop_activation"
      | "stop_loss_pct"
      | "hold_min_share_price"
      | "hold_safety_margin"
      | "early_exit_loss_pct"
      | "hold_margin_per_second"
      | "hold_max_seconds"
      | "hold_max_crossings"
      | "spike_sustain_ms"
      | "execution_delay_ms"
      | "min_price_distance"
      | "max_orders_per_minute"
      | "max_daily_loss"
      | "max_exposure_per_market"
      | "max_drawdown_pct"
      | "trend_filter_enabled"
      | "trend_min_magnitude_usd"
      | "counter_trend_multiplier"
      | "trend_max_magnitude_usd"
      | "ptb_neutral_zone_usd"
      | "ptb_max_counter_distance_usd"
    >;

export const EMPTY_CONFIG: Config = {
  threshold_bps: 0,
  portfolio_pct: 0,
  crypto_fee_rate: 0,
  max_entry_price: 0,
  min_entry_price: 0,
  trend_reversal_pct: 0,
  trend_reversal_threshold: 0,
  spike_faded_pct: 0,
  spike_faded_ms: 0,
  min_hold_ms: 0,
  trailing_stop_pct: 0,
  trailing_stop_activation: 0,
  stop_loss_pct: 0,
  hold_min_share_price: 0,
  hold_safety_margin: 0,
  early_exit_loss_pct: 0,
  hold_margin_per_second: 0,
  hold_max_seconds: 0,
  hold_max_crossings: 0,
  spike_sustain_ms: 0,
  execution_delay_ms: 0,
  min_price_distance: 0,
  max_orders_per_minute: 0,
  max_daily_loss: 0,
  max_exposure_per_market: 0,
  max_drawdown_pct: 0,
  trend_filter_enabled: false,
  trend_min_magnitude_usd: 0,
  counter_trend_multiplier: 0,
  trend_max_magnitude_usd: 0,
  ptb_neutral_zone_usd: 0,
  ptb_max_counter_distance_usd: 0,
};

export const EMPTY_SNAPSHOT: TerminalSnapshot = {
  balance: 0,
  starting_balance: 0,
  total_portfolio_value: 0,
  unrealized_pnl: 0,
  realized_pnl: 0,
  total_pnl: 0,
  cumulative_pnl: 0,
  daily_pnl: 0,
  wins: 0,
  losses: 0,
  total_fees: 0,
  total_volume: 0,
  positions: [],
  trades: [],
  history: [],
  signals: [],
  markets: {},
  wallet_address: null,
  running: false,
  runtime_ms: 0,
  is_live: false,
  config: EMPTY_CONFIG,
};
