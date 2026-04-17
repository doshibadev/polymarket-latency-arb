import { useEffect, useMemo, useRef, useState } from "react";
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  ErrorBar,
  Rectangle,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
  useXAxisScale,
  useYAxisScale,
} from "recharts";
import { fetchConfig, normalizeConfig, saveConfig, sendCommand } from "./api";
import { useTerminalFeed } from "./useTerminalFeed";
import { Config, ConfigField, EMPTY_CONFIG, HistoryPoint, MarketSnapshot, Trade } from "./types";

type SettingsField = {
  key: ConfigField;
  label: string;
  step?: string;
  kind?: "threshold" | "percent";
  group: "entry" | "hold" | "risk" | "trend" | "execution";
};

type ChartMode = "line" | "candle";
type WorkspaceTab = "trade" | "markets" | "positions" | "activity" | "portfolio";

type EquityRow = {
  x: number;
  label: string;
  value: number;
};

type CandleRow = {
  x: number;
  label: string;
  open: number;
  close: number;
  high: number;
  low: number;
  body: [number, number];
  wick: [number, number];
  up: boolean;
};

type CursorProps = {
  points?: Array<{ x: number; y: number }>;
  width?: number;
  height?: number;
};

const SETTINGS_FIELDS: SettingsField[] = [
  { key: "threshold_bps", label: "Spike Threshold ($)", step: "1", kind: "threshold", group: "entry" },
  { key: "portfolio_pct", label: "Portfolio Per Trade (%)", step: "0.1", kind: "percent", group: "entry" },
  { key: "max_entry_price", label: "Max Entry Price", step: "0.01", group: "entry" },
  { key: "min_entry_price", label: "Min Entry Price", step: "0.01", group: "entry" },
  { key: "spike_faded_pct", label: "Spike Faded Threshold", step: "0.01", group: "entry" },
  { key: "spike_faded_ms", label: "Spike Faded Delay (ms)", step: "1", group: "entry" },
  { key: "spike_sustain_ms", label: "Spike Sustain (ms)", step: "1", group: "entry" },
  { key: "trailing_stop_pct", label: "Trailing Stop (%)", step: "0.1", group: "risk" },
  { key: "trailing_stop_activation", label: "Trail Activation (%)", step: "0.1", group: "risk" },
  { key: "stop_loss_pct", label: "Stop Loss (%)", step: "0.1", group: "risk" },
  { key: "early_exit_loss_pct", label: "Early Exit Loss (%)", step: "0.1", kind: "percent", group: "risk" },
  { key: "max_daily_loss", label: "Max Daily Loss ($)", step: "0.01", group: "risk" },
  { key: "max_exposure_per_market", label: "Max Market Exposure ($)", step: "0.01", group: "risk" },
  { key: "max_drawdown_pct", label: "Max Drawdown (%)", step: "0.1", kind: "percent", group: "risk" },
  { key: "min_hold_ms", label: "Min Hold (ms)", step: "1", group: "hold" },
  { key: "hold_min_share_price", label: "Hold Min Share Price", step: "0.01", group: "hold" },
  { key: "hold_safety_margin", label: "Hold Safety Margin", step: "0.01", group: "hold" },
  { key: "hold_margin_per_second", label: "Hold Margin / Second", step: "0.0001", group: "hold" },
  { key: "hold_max_seconds", label: "Hold Max Seconds", step: "1", group: "hold" },
  { key: "hold_max_crossings", label: "Hold Max Crossings", step: "1", group: "hold" },
  { key: "trend_reversal_pct", label: "Trend Reversal (%)", step: "0.1", group: "trend" },
  { key: "trend_reversal_threshold", label: "Trend Reversal ($)", step: "0.1", group: "trend" },
  { key: "trend_min_magnitude_usd", label: "Trend Min Magnitude ($)", step: "0.1", group: "trend" },
  { key: "counter_trend_multiplier", label: "Counter Trend Multiplier", step: "0.01", group: "trend" },
  { key: "trend_max_magnitude_usd", label: "Trend Max Magnitude ($)", step: "0.1", group: "trend" },
  { key: "ptb_neutral_zone_usd", label: "PTB Neutral Zone ($)", step: "0.1", group: "trend" },
  { key: "ptb_max_counter_distance_usd", label: "PTB Max Counter Distance ($)", step: "0.1", group: "trend" },
  { key: "crypto_fee_rate", label: "Fee Rate", step: "0.001", group: "execution" },
  { key: "execution_delay_ms", label: "Execution Delay (ms)", step: "1", group: "execution" },
  { key: "min_price_distance", label: "Min Price Distance", step: "0.01", group: "execution" },
  { key: "max_orders_per_minute", label: "Max Orders/Min", step: "1", group: "execution" },
];

const SETTINGS_GROUPS: Array<{ key: SettingsField["group"]; label: string; help: string }> = [
  { key: "entry", label: "Entry Filters", help: "Trigger thresholds and price acceptance." },
  { key: "hold", label: "Hold Behavior", help: "Minimum hold time and hold-to-resolution guards." },
  { key: "risk", label: "Risk Limits", help: "Stops, drawdown caps, and portfolio protection." },
  { key: "trend", label: "Trend Filter", help: "Trend reversal and price-to-beat gating." },
  { key: "execution", label: "Execution", help: "Fees, delays, and order-rate controls." },
];

function formatMoney(value: number) {
  return `$${Math.abs(value).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

function formatMoneySigned(value: number) {
  const prefix = value > 0 ? "+$" : value < 0 ? "-$" : "$";
  return `${prefix}${Math.abs(value).toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

function formatMs(ms: number) {
  if (ms < 1_000) return `${Math.max(0, Math.round(ms))}ms`;
  return `${(ms / 1_000).toFixed(1)}s`;
}

function formatDuration(ms: number) {
  const totalSeconds = Math.max(0, Math.floor(ms / 1_000));
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${seconds}s`;
  return `${seconds}s`;
}

function formatTimer(endTs?: number | null) {
  if (!endTs) return "00:00";
  const remaining = Math.max(0, endTs - Math.floor(Date.now() / 1_000));
  const mins = Math.floor(remaining / 60);
  const secs = remaining % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

function formatClock(timestamp: string) {
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return timestamp;
  return date.toLocaleTimeString([], { hour12: false });
}

function formatSigned(value: number, digits = 2) {
  return `${value >= 0 ? "+" : ""}${value.toFixed(digits)}`;
}

function formatPct(value: number, digits = 2) {
  return `${value.toFixed(digits)}%`;
}

function formatTs(timestamp?: number | null) {
  if (!timestamp) return "--";
  return new Date(timestamp).toLocaleTimeString([], { hour12: false });
}

function paginate<T>(items: T[], page: number, size: number) {
  const totalPages = Math.max(1, Math.ceil(items.length / size));
  const safePage = Math.min(page, totalPages - 1);
  const start = safePage * size;
  return {
    totalPages,
    safePage,
    start,
    slice: items.slice(start, start + size),
  };
}

function activeMarket(markets: Record<string, MarketSnapshot>) {
  return markets.BTC || markets[Object.keys(markets)[0]] || null;
}

function activeMarketSymbol(markets: Record<string, MarketSnapshot>) {
  return Object.prototype.hasOwnProperty.call(markets, "BTC") ? "BTC" : Object.keys(markets)[0] || null;
}

function displayValue(config: Config, field: SettingsField) {
  const raw = config[field.key];
  if (typeof raw !== "number") return "";
  if (field.kind === "threshold") return String(raw / 100);
  return String(raw);
}

function applyFieldValue<K extends ConfigField>(config: Config, field: SettingsField, rawValue: string): Config[K] {
  const nextValue = Number(rawValue);
  if (Number.isNaN(nextValue)) return config[field.key] as Config[K];
  if (field.kind === "threshold") return Math.round(nextValue * 100) as Config[K];
  return nextValue as Config[K];
}

function buildEquitySeries(history: HistoryPoint[]): EquityRow[] {
  return history.map((point, index) => ({ x: index, label: point.t, value: point.v }));
}

function buildCandleSeries(history: HistoryPoint[]): CandleRow[] {
  if (history.length < 2) return [];
  const rows: CandleRow[] = [];

  for (let index = 1; index < history.length; index += 1) {
    const previous = history[index - 1];
    const current = history[index];
    const open = previous.v;
    const close = current.v;
    const high = Math.max(open, close);
    const low = Math.min(open, close);
    rows.push({
      x: index - 1,
      label: current.t,
      open,
      close,
      high,
      low,
      body: [Math.min(open, close), Math.max(open, close)],
      wick: [
        Math.max(open, close) - low,
        high - Math.max(open, close),
      ],
      up: close >= open,
    });
  }

  return rows;
}

function candleBodyDataKey(entry: CandleRow): [number, number] {
  return entry.body;
}

function candleWickDataKey(entry: CandleRow): [number, number] {
  return entry.wick;
}

function CandleShape(props: any) {
  const payload = props.payload as CandleRow | undefined;
  const color = payload && payload.up ? "#10b981" : "#f43f5e";
  return <Rectangle {...props} fill={color} fillOpacity={0.28} stroke={color} strokeWidth={1.3} radius={0} />;
}

function EquitySegmentsLayer({ rows }: { rows: EquityRow[] }) {
  const xScale = useXAxisScale();
  const yScale = useYAxisScale();

  if (!xScale || !yScale || rows.length < 2) return null;

  return (
    <g>
      {rows.slice(0, -1).map((row, index) => {
        const next = rows[index + 1];
        const x1 = xScale(row.x);
        const x2 = xScale(next.x);
        const y1 = yScale(row.value);
        const y2 = yScale(next.value);

        if ([x1, x2, y1, y2].some((value) => typeof value !== "number" || Number.isNaN(value))) return null;

        const color = next.value >= row.value ? "#10b981" : "#f43f5e";
        return <line key={`${row.x}-${next.x}`} x1={x1 as number} x2={x2 as number} y1={y1 as number} y2={y2 as number} stroke={color} strokeWidth={2.1} strokeLinecap="round" />;
      })}
    </g>
  );
}

function SmoothCursor({ points }: CursorProps) {
  if (!points || points.length === 0) return null;
  const x = points[0]?.x ?? 0;
  const top = points[0]?.y ?? 0;
  const bottom = points[points.length - 1]?.y ?? top;

  return <line className="chart-cursor-line" x1={x} x2={x} y1={top} y2={bottom} />;
}

function EquityTooltip({
  active,
  payload,
  mode,
}: {
  active?: boolean;
  payload?: Array<{ payload: EquityRow | CandleRow }>;
  mode: ChartMode;
}) {
  if (!active || !payload || payload.length === 0) return null;
  const row = payload[0].payload;

  if (mode === "candle" && "open" in row) {
    return (
      <div className="chart-tooltip">
        <div className="chart-tooltip-label">{row.label}</div>
        <div className="chart-tooltip-row"><span>Open</span><strong>{formatMoney(row.open)}</strong></div>
        <div className="chart-tooltip-row"><span>High</span><strong>{formatMoney(row.high)}</strong></div>
        <div className="chart-tooltip-row"><span>Low</span><strong>{formatMoney(row.low)}</strong></div>
        <div className="chart-tooltip-row"><span>Close</span><strong>{formatMoney(row.close)}</strong></div>
      </div>
    );
  }

  if ("value" in row) {
    return (
      <div className="chart-tooltip">
        <div className="chart-tooltip-label">{row.label}</div>
        <div className="chart-tooltip-row"><span>Equity</span><strong>{formatMoney(row.value)}</strong></div>
      </div>
    );
  }

  return null;
}

function SpikeTooltip({
  active,
  payload,
}: {
  active?: boolean;
  payload?: Array<{ payload: { index: number; spike: number } }>;
}) {
  if (!active || !payload || payload.length === 0) return null;
  const row = payload[0].payload;
  return (
    <div className="chart-tooltip chart-tooltip-tight">
      <div className="chart-tooltip-row"><span>Sample</span><strong>#{row.index + 1}</strong></div>
      <div className="chart-tooltip-row"><span>Spike</span><strong>{row.spike >= 0 ? "+" : ""}{row.spike.toFixed(2)}</strong></div>
    </div>
  );
}

function PerformanceChart({
  history,
  chartMode,
  onModeChange,
  startingBalance,
}: {
  history: HistoryPoint[];
  chartMode: ChartMode;
  onModeChange: (mode: ChartMode) => void;
  startingBalance: number;
}) {
  const equityData = useMemo(() => buildEquitySeries(history), [history]);
  const candleData = useMemo(() => buildCandleSeries(history), [history]);
  const activeValues = chartMode === "candle" ? candleData.flatMap((row) => [row.low, row.high]) : equityData.map((row) => row.value);
  const latestValue = chartMode === "candle" ? candleData[candleData.length - 1]?.close ?? startingBalance : equityData[equityData.length - 1]?.value ?? startingBalance;
  const sessionHigh = activeValues.length > 0 ? Math.max(...activeValues) : startingBalance;
  const sessionLow = activeValues.length > 0 ? Math.min(...activeValues) : startingBalance;
  const sessionRange = sessionHigh - sessionLow;
  const lineIsUp = latestValue >= startingBalance;

  return (
    <>
      <div className="panel-header chart-toolbar">
        <div className="chart-summary">
          <div className="panel-title">Session Value Performance</div>
          <div className="chart-stats">
            <span>Last <strong>{formatMoney(latestValue)}</strong></span>
            <span>High <strong>{formatMoney(sessionHigh)}</strong></span>
            <span>Low <strong>{formatMoney(sessionLow)}</strong></span>
            <span>Range <strong>{formatMoney(sessionRange)}</strong></span>
          </div>
        </div>
        <div className="chart-mode-switch">
          <button className={`chart-mode-btn ${chartMode === "line" ? "chart-mode-btn-active" : ""}`} onClick={() => onModeChange("line")}>
            Line
          </button>
          <button className={`chart-mode-btn ${chartMode === "candle" ? "chart-mode-btn-active" : ""}`} onClick={() => onModeChange("candle")}>
            Candle
          </button>
        </div>
      </div>
      <div className="chart-panel">
        <ResponsiveContainer width="100%" height="100%">
          {chartMode === "line" ? (
            <AreaChart data={equityData} margin={{ top: 12, right: 8, bottom: 0, left: 0 }}>
              <defs>
                <linearGradient id="equityFillUp" x1="0" y1="0" x2="0" y2="1">
                  <stop offset="0%" stopColor="#10b981" stopOpacity={0.18} />
                  <stop offset="100%" stopColor="#10b981" stopOpacity={0.01} />
                </linearGradient>
                <linearGradient id="equityFillDown" x1="0" y1="0" x2="0" y2="1">
                  <stop offset="0%" stopColor="#f43f5e" stopOpacity={0.18} />
                  <stop offset="100%" stopColor="#f43f5e" stopOpacity={0.01} />
                </linearGradient>
              </defs>
              <CartesianGrid vertical={false} stroke="#1c1c21" />
              <XAxis
                dataKey="x"
                type="number"
                domain={["dataMin", "dataMax"]}
                tick={{ fill: "#52525b", fontFamily: "JetBrains Mono", fontSize: 9 }}
                tickLine={false}
                axisLine={false}
                tickFormatter={(value) => equityData[value]?.label ?? ""}
                minTickGap={24}
              />
              <YAxis
                orientation="right"
                tick={{ fill: "#52525b", fontFamily: "JetBrains Mono", fontSize: 9 }}
                tickLine={false}
                axisLine={false}
                width={62}
                domain={["dataMin - 1", "dataMax + 1"]}
                tickFormatter={(value) => `$${value}`}
              />
              <Tooltip content={<EquityTooltip mode="line" />} cursor={<SmoothCursor />} />
              <ReferenceLine y={startingBalance} stroke="#52525b" strokeDasharray="3 3" ifOverflow="extendDomain" />
              <Area type="monotone" dataKey="value" stroke="transparent" strokeWidth={0} fill={lineIsUp ? "url(#equityFillUp)" : "url(#equityFillDown)"} isAnimationActive={false} />
              <EquitySegmentsLayer rows={equityData} />
            </AreaChart>
          ) : (
            <BarChart data={candleData} margin={{ top: 12, right: 8, bottom: 0, left: 0 }} barCategoryGap="18%">
              <CartesianGrid vertical={false} stroke="#1c1c21" />
              <XAxis
                dataKey="label"
                type="category"
                tick={{ fill: "#52525b", fontFamily: "JetBrains Mono", fontSize: 9 }}
                tickLine={false}
                axisLine={false}
                minTickGap={24}
              />
              <YAxis
                orientation="right"
                tick={{ fill: "#52525b", fontFamily: "JetBrains Mono", fontSize: 9 }}
                tickLine={false}
                axisLine={false}
                width={62}
                domain={["dataMin - 1", "dataMax + 1"]}
                tickFormatter={(value) => `$${value}`}
              />
              <Tooltip content={<EquityTooltip mode="candle" />} cursor={<SmoothCursor />} />
              <ReferenceLine y={startingBalance} stroke="#52525b" strokeDasharray="3 3" ifOverflow="extendDomain" />
              <Bar dataKey={candleBodyDataKey} shape={<CandleShape />} maxBarSize={14} isAnimationActive={false}>
                <ErrorBar dataKey={candleWickDataKey} width={0} strokeWidth={1.3} />
              </Bar>
            </BarChart>
          )}
        </ResponsiveContainer>
      </div>
    </>
  );
}

function SpikeChart({ values }: { values: number[] }) {
  const data = useMemo(() => values.map((spike, index) => ({ x: index, index, spike })), [values]);
  const lastSpike = values[values.length - 1] ?? 0;
  const spikeStroke = lastSpike >= 0 ? "#10b981" : "#f43f5e";
  const spikeFill = lastSpike >= 0 ? "url(#spikeFillUp)" : "url(#spikeFillDown)";

  return (
    <ResponsiveContainer width="100%" height="100%">
      <AreaChart data={data} margin={{ top: 1, right: 0, bottom: 0, left: 0 }}>
        <defs>
          <linearGradient id="spikeFillUp" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#10b981" stopOpacity={0.14} />
            <stop offset="100%" stopColor="#10b981" stopOpacity={0.01} />
          </linearGradient>
          <linearGradient id="spikeFillDown" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#f43f5e" stopOpacity={0.14} />
            <stop offset="100%" stopColor="#f43f5e" stopOpacity={0.01} />
          </linearGradient>
        </defs>
        <XAxis dataKey="x" type="number" hide domain={["dataMin", "dataMax"]} />
        <YAxis hide domain={["dataMin - 0.25", "dataMax + 0.25"]} />
        <Tooltip content={<SpikeTooltip />} cursor={<SmoothCursor />} />
        <Area type="monotone" dataKey="spike" stroke={spikeStroke} strokeWidth={1.15} fill={spikeFill} isAnimationActive={false} />
      </AreaChart>
    </ResponsiveContainer>
  );
}

function SettingsModal({
  open,
  config,
  busy,
  saveState,
  onClose,
  onChange,
  onSave,
}: {
  open: boolean;
  config: Config;
  busy: boolean;
  saveState: "idle" | "saved" | "error";
  onClose: () => void;
  onChange: <K extends ConfigField>(key: K, nextValue: Config[K]) => void;
  onSave: () => void;
}) {
  if (!open) return null;

  const saveLabel = busy ? "SAVING..." : saveState === "saved" ? "SAVED ✓" : saveState === "error" ? "ERROR" : "Save & Apply Changes";

  return (
    <div className="modal-overlay" onClick={(event) => event.target === event.currentTarget && onClose()}>
      <div className="modal-content">
        <div className="panel-header">
          <div className="panel-title">Bot Configuration</div>
          <div className="modal-close" onClick={onClose}>✕</div>
        </div>
        <div className="settings-groups">
          {SETTINGS_GROUPS.map((group) => (
            <section className="settings-section" key={group.key}>
              <div className="settings-section-head">
                <div className="settings-section-title">{group.label}</div>
                <div className="settings-section-help">{group.help}</div>
              </div>
              <div className="settings-grid">
                {SETTINGS_FIELDS.filter((field) => field.group === group.key).map((field) => (
                  <div className="setting-group" key={field.key}>
                    <label className="setting-label">{field.label}</label>
                    <input
                      type="number"
                      step={field.step ?? "any"}
                      className="setting-input"
                      value={displayValue(config, field)}
                      onChange={(event) => onChange(field.key, applyFieldValue(config, field, event.target.value))}
                    />
                  </div>
                ))}
                {group.key === "trend" ? (
                  <div className="setting-group">
                    <label className="setting-label">Trend Filter Enabled</label>
                    <select
                      className="setting-input"
                      value={config.trend_filter_enabled ? "true" : "false"}
                      onChange={(event) => onChange("trend_filter_enabled", event.target.value === "true")}
                    >
                      <option value="true">Enabled</option>
                      <option value="false">Disabled</option>
                    </select>
                  </div>
                ) : null}
              </div>
            </section>
          ))}
          <button className={`save-btn ${saveState === "saved" ? "save-btn-ok" : saveState === "error" ? "save-btn-error" : ""}`} disabled={busy} onClick={onSave}>
            {saveLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

function RuntimeStrip({
  snapshot,
  status,
  lastUpdatedAt,
  runtime,
}: {
  snapshot: ReturnType<typeof useTerminalFeed>["snapshot"];
  status: ReturnType<typeof useTerminalFeed>["status"];
  lastUpdatedAt: number | null;
  runtime: string;
}) {
  const statusLabel = status === "live" ? "ONLINE" : status === "connecting" ? "CONNECTING" : "OFFLINE";

  return (
    <div className="runtime-strip">
      <div className="runtime-chip"><span className="runtime-key">Feed</span><span className={status === "live" ? "val-up" : "val-down"}>{statusLabel}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Mode</span><span>{snapshot.is_live ? "LIVE" : "PAPER"}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Wallet</span><span>{snapshot.wallet_address ? `${snapshot.wallet_address.slice(0, 6)}...${snapshot.wallet_address.slice(-4)}` : "paper-ledger"}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Exposure</span><span>{formatMoney(snapshot.total_portfolio_value - snapshot.balance)}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Open</span><span>{snapshot.positions.length}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Runtime</span><span>{runtime}</span></div>
      <div className="runtime-chip"><span className="runtime-key">Updated</span><span>{formatTs(lastUpdatedAt)}</span></div>
    </div>
  );
}

function MarketScanner({ markets }: { markets: Record<string, MarketSnapshot> }) {
  const rows = useMemo(
    () =>
      Object.entries(markets)
        .map(([symbol, market]) => ({ symbol, market }))
        .sort((a, b) => Math.abs(b.market.spike || 0) - Math.abs(a.market.spike || 0)),
    [markets],
  );

  return (
    <div className="panel market-scanner-panel">
      <div className="panel-header"><div className="panel-title">Market Scanner</div><div className="panel-count">{rows.length}</div></div>
      {rows.length === 0 ? (
        <div className="empty-list">No markets</div>
      ) : (
        <div className="table-shell">
          <div className="table-head markets-grid">
            <span>Symbol</span><span>Up</span><span>Down</span><span>Spike</span><span>End</span>
          </div>
          <div className="table-body">
            {rows.map(({ symbol, market }) => (
              <div className="table-row markets-grid" key={symbol} title={market.question}>
                <span className="table-strong">{symbol}</span>
                <span className="val-up">{market.up_price.toFixed(4)}</span>
                <span className="val-down">{market.down_price.toFixed(4)}</span>
                <span className={(market.spike || 0) >= 0 ? "val-up" : "val-down"}>{formatSigned(market.spike || 0)}</span>
                <span>{formatTimer(market.end_ts)}</span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function OpenPositionsTable({
  positions,
  busyAction,
  clock,
  entryTimes,
  onClosePosition,
}: {
  positions: ReturnType<typeof useTerminalFeed>["snapshot"]["positions"];
  busyAction: string | null;
  clock: number;
  entryTimes: number[];
  onClosePosition: (index: number) => void;
}) {
  return (
    <div className="panel positions-table-panel">
      <div className="panel-header"><div className="panel-title">Open Positions</div><div className="panel-count">{positions.length}</div></div>
      {positions.length === 0 ? (
        <div className="empty-list">No open positions</div>
      ) : (
        <div className="table-shell">
          <div className="table-head positions-grid">
            <span>Contract</span><span>Shares</span><span>Entry</span><span>Now</span><span>PnL</span><span>Held</span><span>Flags</span><span>Action</span>
          </div>
          <div className="table-body">
            {positions.map((position, index) => {
              const held = formatMs(clock - (entryTimes[index] ?? clock));
              return (
                <div className="table-row positions-grid" key={`${position.symbol}-${position.side}-${index}`}>
                  <span className="table-strong">{position.symbol} {position.side}</span>
                  <span>{position.shares.toFixed(2)}</span>
                  <span>{position.entry_price.toFixed(4)}</span>
                  <span>{position.current_price.toFixed(4)}</span>
                  <span className={position.pnl >= 0 ? "val-up" : "val-down"}>{formatMoneySigned(position.pnl)}</span>
                  <span>{held}</span>
                  <span>{`LVL ${position.level}${position.hold_to_resolution ? " · HOLD" : ""}`}</span>
                  <span><button className="row-btn danger-btn" onClick={() => onClosePosition(index)} disabled={busyAction !== null}>Close</button></span>
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

function TradeTable({
  title,
  trades,
  kind,
  pageSize = 12,
}: {
  title: string;
  trades: Trade[];
  kind: "all" | "settled";
  pageSize?: number;
}) {
  const [page, setPage] = useState(0);
  useEffect(() => {
    setPage((current) => Math.min(current, Math.max(0, Math.ceil(trades.length / pageSize) - 1)));
  }, [pageSize, trades.length]);
  const { slice, safePage, totalPages, start } = paginate(trades, page, pageSize);

  return (
    <div className="panel trade-table-panel">
      <div className="panel-header"><div className="panel-title">{title}</div><div className="panel-count">{trades.length}</div></div>
      {trades.length === 0 ? (
        <div className="empty-list">No rows</div>
      ) : (
        <>
          <div className={`table-head ${kind === "all" ? "trades-grid" : "settled-grid"}`}>
            {kind === "all" ? (
              <>
                <span>Time</span><span>Type</span><span>Dir</span><span>Shares</span><span>Entry</span><span>Exit</span><span>PnL</span><span>Reason</span>
              </>
            ) : (
              <>
                <span>Time</span><span>Dir</span><span>Exit</span><span>PnL</span><span>PnL%</span><span>Reason</span>
              </>
            )}
          </div>
          <div className="table-body">
            {slice.map((trade, index) => {
              const pnl = trade.pnl ?? 0;
              const pnlPct =
                trade.entry_price && trade.exit_price
                  ? (((trade.exit_price - trade.entry_price) / trade.entry_price) * 100 * (trade.direction === "DOWN" ? -1 : 1))
                  : 0;
              return kind === "all" ? (
                <div className="table-row trades-grid" key={`${trade.timestamp}-${trade.symbol}-${start + index}`}>
                  <span>{formatClock(trade.timestamp)}</span>
                  <span className={trade.type === "exit" ? (pnl >= 0 ? "val-up" : "val-down") : ""}>{trade.type.toUpperCase()}</span>
                  <span>{trade.direction}</span>
                  <span>{trade.shares.toFixed(2)}</span>
                  <span>{trade.entry_price != null ? trade.entry_price.toFixed(4) : "—"}</span>
                  <span>{trade.exit_price != null ? trade.exit_price.toFixed(4) : "—"}</span>
                  <span className={trade.type === "exit" ? (pnl >= 0 ? "val-up" : "val-down") : ""}>{trade.type === "exit" ? formatMoneySigned(pnl) : formatMoney(trade.cost)}</span>
                  <span className="table-muted">{trade.close_reason || trade.question || "—"}</span>
                </div>
              ) : (
                <div className="table-row settled-grid" key={`${trade.timestamp}-${trade.symbol}-${start + index}`}>
                  <span>{formatClock(trade.timestamp)}</span>
                  <span>{trade.direction}</span>
                  <span>{trade.exit_price != null ? trade.exit_price.toFixed(4) : "—"}</span>
                  <span className={pnl >= 0 ? "val-up" : "val-down"}>{formatMoneySigned(pnl)}</span>
                  <span className={pnlPct >= 0 ? "val-up" : "val-down"}>{formatPct(pnlPct)}</span>
                  <span className="table-muted">{trade.close_reason || "—"}</span>
                </div>
              );
            })}
          </div>
          <div className="table-footer">
            <span>{start + 1}-{Math.min(start + pageSize, trades.length)} / {trades.length}</span>
            <div className="pager-actions">
              <button className="row-btn" disabled={safePage === 0} onClick={() => setPage((current) => Math.max(current - 1, 0))}>Prev</button>
              <button className="row-btn" disabled={safePage >= totalPages - 1} onClick={() => setPage((current) => Math.min(current + 1, totalPages - 1))}>Next</button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}

export function App() {
  const { snapshot, status, lastUpdatedAt } = useTerminalFeed();
  const [draftConfig, setDraftConfig] = useState<Config>(EMPTY_CONFIG);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [busyAction, setBusyAction] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<"idle" | "saved" | "error">("idle");
  const [clock, setClock] = useState(Date.now());
  const [spikeSeries, setSpikeSeries] = useState<number[]>([]);
  const [chartMode, setChartMode] = useState<ChartMode>("line");
  const [activeTab, setActiveTab] = useState<WorkspaceTab>("trade");
  const [selectedMarketSymbol, setSelectedMarketSymbol] = useState<string>("BTC");
  const positionEntryTimesRef = useRef<number[]>([]);

  const marketSymbols = useMemo(() => Object.keys(snapshot.markets).sort(), [snapshot.markets]);
  const market = useMemo(
    () => snapshot.markets[selectedMarketSymbol] || activeMarket(snapshot.markets),
    [snapshot.markets, selectedMarketSymbol],
  );
  const displayMarketSymbol = marketSymbols.includes(selectedMarketSymbol)
    ? selectedMarketSymbol
    : activeMarketSymbol(snapshot.markets) || "BTC";
  const openPositions = snapshot.positions || [];
  const exits = useMemo(() => snapshot.trades.filter((trade) => trade.type === "exit"), [snapshot.trades]);
  const recentTrades = useMemo(() => snapshot.trades.slice().reverse().slice(0, 30), [snapshot.trades]);
  const settledTrades = useMemo(() => exits.slice().reverse().slice(0, 30), [exits]);
  const signals = useMemo(() => snapshot.signals.slice().reverse(), [snapshot.signals]);
  const wins = snapshot.wins || 0;
  const losses = snapshot.losses || 0;
  const winRate = wins + losses > 0 ? ((wins / (wins + losses)) * 100).toFixed(1) : "0.0";

  const avgWin = useMemo(() => {
    const winTrades = exits.filter((trade) => (trade.pnl ?? 0) > 0);
    if (winTrades.length === 0) return null;
    return winTrades.reduce((sum, trade) => sum + (trade.pnl ?? 0), 0) / winTrades.length;
  }, [exits]);

  const avgLoss = useMemo(() => {
    const lossTrades = exits.filter((trade) => (trade.pnl ?? 0) <= 0);
    if (lossTrades.length === 0) return null;
    return lossTrades.reduce((sum, trade) => sum + (trade.pnl ?? 0), 0) / lossTrades.length;
  }, [exits]);

  const profitFactor = useMemo(() => {
    const grossWin = exits.filter((trade) => (trade.pnl ?? 0) > 0).reduce((sum, trade) => sum + (trade.pnl ?? 0), 0);
    const grossLoss = Math.abs(exits.filter((trade) => (trade.pnl ?? 0) <= 0).reduce((sum, trade) => sum + (trade.pnl ?? 0), 0));
    if (grossLoss > 0) return (grossWin / grossLoss).toFixed(2);
    return grossWin > 0 ? "∞" : "—";
  }, [exits]);

  const feeDrag = snapshot.total_volume > 0 ? ((snapshot.total_fees / snapshot.total_volume) * 100).toFixed(3) : "0.000";
  const marketBasis = market ? (market.chainlink || 0) - (market.binance || 0) : 0;
  const marketPtbGap = market?.price_to_beat != null ? (market.chainlink || 0) - market.price_to_beat : null;

  useEffect(() => {
    void (async () => {
      const config = await fetchConfig().catch(() => normalizeConfig(snapshot.config ?? EMPTY_CONFIG));
      setDraftConfig(config);
    })();
  }, []);

  useEffect(() => {
    if (!settingsOpen) {
      setDraftConfig(normalizeConfig(snapshot.config ?? EMPTY_CONFIG));
    }
  }, [settingsOpen, snapshot.config]);

  useEffect(() => {
    const timer = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    positionEntryTimesRef.current = openPositions.map((position) => Date.now() - (position.held_ms || 0));
  }, [openPositions]);

  useEffect(() => {
    const nextSpike = market?.spike ?? 0;
    setSpikeSeries((current) => [...current, nextSpike].slice(-120));
  }, [market?.spike]);

  useEffect(() => {
    if (marketSymbols.length === 0) return;
    if (!marketSymbols.includes(selectedMarketSymbol)) {
      setSelectedMarketSymbol(activeMarketSymbol(snapshot.markets) || marketSymbols[0]);
    }
  }, [marketSymbols, selectedMarketSymbol, snapshot.markets]);

  async function openSettings() {
    const config = await fetchConfig().catch(() => normalizeConfig(snapshot.config ?? EMPTY_CONFIG));
    setDraftConfig(config);
    setSaveState("idle");
    setSettingsOpen(true);
  }

  async function toggleBot() {
    const nextType = snapshot.running ? "stop" : "start";
    setBusyAction(nextType);
    try {
      await sendCommand({ _type: nextType });
    } finally {
      setBusyAction(null);
    }
  }

  async function closePosition(index: number) {
    setBusyAction("close");
    try {
      await sendCommand({ _type: "close_position", index });
    } finally {
      setBusyAction(null);
    }
  }

  async function resetPaper() {
    const confirmed = window.confirm("Reset paper wallet? This will clear all trade history.");
    if (!confirmed) return;
    setBusyAction("reset");
    try {
      await sendCommand({ _type: "reset" });
    } finally {
      setBusyAction(null);
    }
  }

  async function handleSaveSettings() {
    setBusyAction("save");
    setSaveState("idle");
    try {
      await saveConfig(draftConfig);
      setSaveState("saved");
      window.setTimeout(() => {
        setSettingsOpen(false);
        setSaveState("idle");
      }, 800);
    } catch {
      setSaveState("error");
    } finally {
      window.setTimeout(() => setSaveState("idle"), 2000);
      setBusyAction(null);
    }
  }

  function updateDraft<K extends ConfigField>(key: K, nextValue: Config[K]) {
    setDraftConfig((current) => ({ ...current, [key]: nextValue }));
  }

  const totalPnL = snapshot.total_pnl || 0;
  const realized = snapshot.realized_pnl || 0;
  const unrealized = snapshot.unrealized_pnl || 0;
  const thresholdUsd = (snapshot.config.threshold_bps ?? 0) / 100;
  const thresholdPct = Math.min(thresholdUsd / 100, 1) * 50;
  const spikeMagnitude = Math.min(Math.abs(market?.spike ?? 0) / 100, 1) * 50;
  const realizedAbs = Math.abs(realized);
  const unrealizedAbs = Math.abs(unrealized);
  const pnlTotalAbs = realizedAbs + unrealizedAbs || 1;
  const runtime = snapshot.running ? formatDuration(snapshot.runtime_ms || 0) : "--";

  return (
    <>
      <SettingsModal
        open={settingsOpen}
        config={draftConfig}
        busy={busyAction === "save"}
        saveState={saveState}
        onClose={() => setSettingsOpen(false)}
        onChange={updateDraft}
        onSave={() => void handleSaveSettings()}
      />

      <div className="terminal-container">
        <header className="main-header">
          <div className="logo">
            <div className="logo-diamond" />
            <div>
              <h1>LATTICE TERMINAL</h1>
            </div>
          </div>

          <nav className="top-tabs" aria-label="Workspace">
            {[
              ["trade", "Trade"],
              ["markets", "Markets"],
              ["positions", "Positions"],
              ["activity", "Activity"],
              ["portfolio", "Portfolio"],
            ].map(([key, label]) => (
              <button
                key={key}
                className={`top-tab ${activeTab === key ? "top-tab-active" : ""}`}
                onClick={() => setActiveTab(key as WorkspaceTab)}
              >
                {label}
              </button>
            ))}
          </nav>

          <div className="header-actions">
            <div className="wallet-address" style={{ display: snapshot.is_live && snapshot.wallet_address ? "block" : "none" }}>
              {snapshot.wallet_address ? `${snapshot.wallet_address.slice(0, 6)}...${snapshot.wallet_address.slice(-4)}` : ""}
            </div>
            <button className="settings-btn" onClick={() => void openSettings()}>Settings</button>
            <button
              className="settings-btn"
              onClick={() => void toggleBot()}
              style={{ borderColor: snapshot.running ? "var(--color-down)" : "var(--color-up)", color: snapshot.running ? "var(--color-down)" : "var(--color-up)" }}
              disabled={busyAction !== null}
            >
              {snapshot.running ? "■ STOP" : "▶ START"}
            </button>
            <button
              className="settings-btn"
              onClick={() => void resetPaper()}
              style={{ borderColor: "var(--color-gold)", color: "var(--color-gold)" }}
              disabled={busyAction !== null || snapshot.is_live}
            >
              ↺ RESET
            </button>
          </div>
        </header>

        <RuntimeStrip snapshot={snapshot} status={status} lastUpdatedAt={lastUpdatedAt} runtime={runtime} />

        <div className="metrics-bar">
          <div className="metric-mini"><div className="m-label">Portfolio Value</div><div className="m-value">{formatMoney(snapshot.total_portfolio_value || 0)}</div><div className="metric-sub">Start {formatMoney(snapshot.starting_balance || 0)}</div></div>
          <div className="metric-mini"><div className="m-label">Wallet Balance</div><div className="m-value">{formatMoney(snapshot.balance || 0)}</div><div className="metric-sub">Start {formatMoney(snapshot.starting_balance || 0)}</div></div>
          <div className="metric-mini"><div className="m-label">Total PnL</div><div className={`m-value ${totalPnL >= 0 ? "val-up" : "val-down"}`}>{formatMoneySigned(totalPnL)}</div></div>
          <div className="metric-mini"><div className="m-label">Daily PnL</div><div className={`m-value ${snapshot.daily_pnl >= 0 ? "val-up" : "val-down"}`}>{formatMoneySigned(snapshot.daily_pnl || 0)}</div></div>
          <div className="metric-mini"><div className="m-label">Unrealized PnL</div><div className={`m-value ${unrealized >= 0 ? "val-up" : "val-down"}`}>{formatMoneySigned(unrealized)}</div></div>
          <div className="metric-mini"><div className="m-label">Win Rate</div><div className="m-value">{winRate}%</div><div className="metric-sub">{wins + losses} trades</div></div>
        </div>

        <main className="workspace-shell">
          {activeTab === "trade" ? (
            <div className="workspace-grid workspace-grid-trade">
              <div className="panel fixed-panel">
                <div className="panel-header">
                  <div className="panel-title">Active Market</div>
                  <div className="top-tabs" style={{ flex: "0 0 auto", minWidth: 0, justifyContent: "flex-end" }}>
                    {marketSymbols.map((symbol) => (
                      <button
                        key={symbol}
                        className={`top-tab ${displayMarketSymbol === symbol ? "top-tab-active" : ""}`}
                        onClick={() => setSelectedMarketSymbol(symbol)}
                      >
                        {symbol}
                      </button>
                    ))}
                  </div>
                </div>
                <div className="market-summary">
                  <div className="m-question">{(market?.question || "FETCHING MARKET DATA...").toUpperCase()}</div>
                  <div className="m-timer">{formatTimer(market?.end_ts)}</div>
                  <div className="market-summary-grid">
                    <div className="market-mini"><span className="p-label">{`BINANCE ${displayMarketSymbol}`}</span><span className="p-val">{formatMoney(market?.binance || 0)}</span></div>
                    <div className="market-mini"><span className="p-label">{`CHAINLINK ${displayMarketSymbol}`}</span><span className="p-val">{formatMoney(market?.chainlink || 0)}</span></div>
                    <div className="market-mini"><span className="p-label">ORACLE BASIS</span><span className={`p-val ${marketBasis >= 0 ? "val-up" : "val-down"}`}>{formatMoneySigned(marketBasis)}</span></div>
                    <div className="market-mini"><span className="p-label">PTB GAP</span><span className={`p-val ${marketPtbGap != null && marketPtbGap >= 0 ? "val-up" : "val-down"}`}>{marketPtbGap == null ? "—" : formatMoneySigned(marketPtbGap)}</span></div>
                  </div>
                  <div className="price-line"><span className="p-label">PRICE TO BEAT</span><span className="p-val p-gold">{market?.price_to_beat != null ? formatMoney(market.price_to_beat) : "—"}</span></div>
                  <div className="price-line"><span className="p-label">POLY UP</span><span className="p-val val-up">{(market?.up_price || 0).toFixed(4)}</span></div>
                  <div className="price-line"><span className="p-label">POLY DOWN</span><span className="p-val val-down">{(market?.down_price || 0).toFixed(4)}</span></div>
                  <div className="market-opportunity">
                    <div className="market-opportunity-head">Decision Context</div>
                    <div className="market-opportunity-line"><span>Spike</span><strong className={market && market.spike >= 0 ? "val-up" : "val-down"}>{formatSigned(market?.spike || 0)}</strong></div>
                    <div className="market-opportunity-line"><span>PTB Distance</span><strong className={marketPtbGap != null && marketPtbGap >= 0 ? "val-up" : "val-down"}>{marketPtbGap == null ? "—" : formatMoneySigned(marketPtbGap)}</strong></div>
                    <div className="market-opportunity-line"><span>Outcome Bias</span><strong>{(market?.up_price || 0) >= (market?.down_price || 0) ? "UP LEAN" : "DOWN LEAN"}</strong></div>
                  </div>
                  <div className="spike-block">
                    <div className="spike-meta"><span>SPIKE DELTA</span><span>{(market?.spike || 0).toFixed(2)}</span></div>
                    <div className="spike-viz">
                      <div className="spike-center" />
                      <div
                        className="spike-fill"
                        style={{
                          width: `${spikeMagnitude}%`,
                          left: (market?.spike || 0) >= 0 ? "50%" : `${50 - spikeMagnitude}%`,
                          background: (market?.spike || 0) >= 0 ? "var(--color-up)" : "var(--color-down)",
                        }}
                      />
                      <div className="threshold-marker" style={{ left: `${50 + thresholdPct}%` }} />
                      <div className="threshold-marker" style={{ left: `${50 - thresholdPct}%` }} />
                    </div>
                    <div className="spike-mini-chart"><SpikeChart values={spikeSeries} /></div>
                  </div>
                </div>
              </div>

              <div className="panel chart-shell">
                <PerformanceChart history={snapshot.history} chartMode={chartMode} onModeChange={setChartMode} startingBalance={snapshot.starting_balance || 0} />
              </div>

              <OpenPositionsTable
                positions={openPositions}
                busyAction={busyAction}
                clock={clock}
                entryTimes={positionEntryTimesRef.current}
                onClosePosition={(index) => void closePosition(index)}
              />
            </div>
          ) : null}

          {activeTab === "markets" ? (
            <div className="workspace-grid workspace-grid-two">
              <MarketScanner markets={snapshot.markets} />
              <div className="panel fixed-panel">
                <div className="panel-header">
                  <div className="panel-title">Active Market Detail</div>
                  <div className="top-tabs" style={{ flex: "0 0 auto", minWidth: 0, justifyContent: "flex-end" }}>
                    {marketSymbols.map((symbol) => (
                      <button
                        key={symbol}
                        className={`top-tab ${displayMarketSymbol === symbol ? "top-tab-active" : ""}`}
                        onClick={() => setSelectedMarketSymbol(symbol)}
                      >
                        {symbol}
                      </button>
                    ))}
                  </div>
                </div>
                <div className="market-summary market-summary-spacious">
                  <div className="m-question">{(market?.question || "FETCHING MARKET DATA...").toUpperCase()}</div>
                  <div className="market-summary-grid">
                    <div className="market-mini"><span className="p-label">ENDS IN</span><span className="p-val p-gold">{formatTimer(market?.end_ts)}</span></div>
                    <div className="market-mini"><span className="p-label">SPIKE DELTA</span><span className={`p-val ${market && market.spike >= 0 ? "val-up" : "val-down"}`}>{formatSigned(market?.spike || 0)}</span></div>
                    <div className="market-mini"><span className="p-label">UP MID</span><span className="p-val val-up">{(market?.up_price || 0).toFixed(4)}</span></div>
                    <div className="market-mini"><span className="p-label">DOWN MID</span><span className="p-val val-down">{(market?.down_price || 0).toFixed(4)}</span></div>
                    <div className="market-mini"><span className="p-label">{`BINANCE ${displayMarketSymbol}`}</span><span className="p-val">{formatMoney(market?.binance || 0)}</span></div>
                    <div className="market-mini"><span className="p-label">{`CHAINLINK ${displayMarketSymbol}`}</span><span className="p-val">{formatMoney(market?.chainlink || 0)}</span></div>
                  </div>
                </div>
              </div>
            </div>
          ) : null}

          {activeTab === "positions" ? (
            <div className="workspace-grid workspace-grid-two">
              <OpenPositionsTable
                positions={openPositions}
                busyAction={busyAction}
                clock={clock}
                entryTimes={positionEntryTimesRef.current}
                onClosePosition={(index) => void closePosition(index)}
              />
              <div className="panel fixed-panel">
                <div className="panel-header"><div className="panel-title">Trade Stats</div></div>
                <div className="stats-grid">
                  <div className="stat-cell"><div className="stat-label">Avg Win</div><div className="stat-value val-up">{avgWin != null ? formatMoneySigned(avgWin) : "—"}</div></div>
                  <div className="stat-cell"><div className="stat-label">Avg Loss</div><div className="stat-value val-down">{avgLoss != null ? formatMoneySigned(avgLoss) : "—"}</div></div>
                  <div className="stat-cell"><div className="stat-label">Profit Factor</div><div className={`stat-value ${profitFactor !== "—" && profitFactor !== "∞" && Number(profitFactor) >= 1 ? "val-up" : ""}`}>{profitFactor}</div></div>
                  <div className="stat-cell"><div className="stat-label">Fee Drag</div><div className="stat-value">{feeDrag}%</div></div>
                </div>
                <div className="pnl-summary">
                  <div className="pnl-label-row"><span>REALIZED</span><span>UNREALIZED</span></div>
                  <div className="pnl-bar">
                    <div className="pnl-bar-realized" style={{ width: `${(realizedAbs / pnlTotalAbs) * 100}%` }} />
                    <div className="pnl-bar-unrealized" style={{ width: `${(unrealizedAbs / pnlTotalAbs) * 100}%` }} />
                  </div>
                  <div className="pnl-value-row"><span className="val-up">{formatMoneySigned(realized)}</span><span className="pnl-unrealized-label">{formatMoneySigned(unrealized)}</span></div>
                </div>
              </div>
            </div>
          ) : null}

          {activeTab === "activity" ? (
            <div className="workspace-grid workspace-grid-activity">
              <div className="panel signal-shell">
                <div className="panel-header"><div className="panel-title">Signal Terminal</div><div className="panel-count">{signals.length}</div></div>
                <div className="terminal-panel terminal-panel-full">
                  <div className="term-line term-header"><span>Time</span><span>Sym</span><span>Side</span><span>Spike</span><span>Status</span><span>Reason</span></div>
                  <div id="signal-log">
                    {signals.map((signal, index) => (
                      <div className="term-line" key={`${signal.t}-${signal.s}-${index}`}>
                        <span className="term-time">{signal.t}</span>
                        <span>{signal.s}</span>
                        <span className={signal.d === "UP" ? "val-up" : "val-down"}>{signal.d}</span>
                        <span className="sig-val">{signal.v.toFixed(2)}</span>
                        <span className={signal.st === "EXECUTED" ? "st-exec" : "st-rej"}>{signal.st}</span>
                        <span className="term-reason">{signal.r || ""}</span>
                      </div>
                    ))}
                    {signals.length === 0 ? <div className="term-line empty-term">No signals</div> : null}
                  </div>
                </div>
              </div>
              <TradeTable title="Execution Log" trades={recentTrades} kind="all" />
              <TradeTable title="Recent Settled" trades={settledTrades} kind="settled" pageSize={10} />
            </div>
          ) : null}

          {activeTab === "portfolio" ? (
            <div className="workspace-grid workspace-grid-portfolio">
              <div className="panel chart-shell chart-shell-portfolio">
                <PerformanceChart history={snapshot.history} chartMode={chartMode} onModeChange={setChartMode} startingBalance={snapshot.starting_balance || 0} />
              </div>
            </div>
          ) : null}
        </main>
      </div>
    </>
  );
}
