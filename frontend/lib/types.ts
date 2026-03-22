export interface TokenInfo {
  name: string;
  symbol: string;
  default_leverage: number;
  default_order_size_usd: number;
}

export interface StartRequest {
  token: string;
  order_size_usd?: number;
  leverage?: number;
  time_limit_secs?: number;
}

export interface SignalState {
  bias: number;
  ema_gap_bps: number;
  bollinger_z: number;
  macd_hist_bps: number;
  rsi: number;
  quote_vwap_dev_bps: number;
}

export interface Stats {
  total_volume: number;
  total_fees: number;
  session_pnl: number;
  fills_count: number;
  maker_fills: number;
  taker_fills: number;
  best_pnl: number;
  rt_count: number;
  rt_profit: number;
  order_batches: number;
  orders_posted: number;
  cancel_batches: number;
  cancel_requests: number;
}

export interface UiEvent {
  ts_ms: number;
  level: "info" | "success" | "warn" | "error";
  title: string;
  details: string;
}

export interface HistoryPoint {
  ts_ms: number;
  mid: number;
  session_pnl: number;
  position: number;
  spread_ticks: number;
  volatility: number;
  inventory_notional: number;
  drawdown: number;
}

export interface StatusSnapshot {
  token: string;
  running: boolean;
  trading_enabled: boolean;
  position: number;
  unrealized_pnl: number;
  entry_price: number | null;
  equity: number;
  mid_price: number | null;
  spread_ticks: number;
  volatility: number;
  signals: SignalState;
  stats: Stats;
  buy_orders: number;
  sell_orders: number;
  size_scale: number;
  last_status: string;
  uptime_secs: number;
  start_ts_ms: number;
  recent_events: UiEvent[];
  metrics_history: HistoryPoint[];
}

export interface FinishedSession {
  token: string;
  exchange_name: string;
  start_ts_ms: number;
  end_ts_ms: number;
  duration_secs: number;
  gross_pnl: number;
  net_pnl: number;
  volume: number;
  fees: number;
  fills: number;
  rt_count: number;
  cost_per_million: number;
  stop_reason: string;
}

export function costPerMillion(stats: Stats): number {
  if (stats.total_volume <= 0) return 0;
  return (stats.total_fees - stats.session_pnl) / (stats.total_volume / 1e6);
}

export function makerRatio(stats: Stats): number {
  const total = stats.maker_fills + stats.taker_fills;
  return total > 0 ? stats.maker_fills / total : 0;
}

export function formatDuration(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

export function formatTimeAgo(ts_ms: number): string {
  const diff = Date.now() - ts_ms;
  const h = Math.floor(diff / 3600000);
  const m = Math.floor((diff % 3600000) / 60000);
  if (h > 23) return `${Math.floor(h / 24)}d ago`;
  if (h > 0) return `${h}h ago`;
  if (m > 0) return `${m}m ago`;
  return "just now";
}
