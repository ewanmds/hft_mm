use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

/// Best bid/offer snapshot
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Bbo {
    pub best_bid: f64,
    pub best_ask: f64,
    pub mid: f64,
}

/// A single order level (price, size)
#[derive(Debug, Clone, Copy)]
pub struct OrderLevel {
    pub price: f64,
    pub size: f64,
}

/// Tracked order details
#[derive(Debug, Clone)]
pub struct TrackedOrder {
    pub price: f64,
    pub size: f64,
    pub is_buy: bool,
    pub ts: f64,
}

/// Pending round-trip leg
#[derive(Debug, Clone)]
pub struct PendingRt {
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub time: f64,
}

/// Fill record for adverse markout tracking
#[derive(Debug, Clone)]
pub struct FillRecord {
    pub side: Side,
    pub fill_price: f64,
    pub time: f64,
    pub checked: bool, // whether markout has been sampled yet
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SignalState {
    pub bias: f64,
    pub ema_gap_bps: f64,
    pub bollinger_z: f64,
    pub macd_hist_bps: f64,
    pub rsi: f64,
    pub quote_vwap_dev_bps: f64,
}

impl SignalState {
    pub fn neutral() -> Self {
        Self {
            bias: 0.0,
            ema_gap_bps: 0.0,
            bollinger_z: 0.0,
            macd_hist_bps: 0.0,
            rsi: 50.0,
            quote_vwap_dev_bps: 0.0,
        }
    }
}

/// Session statistics — all hot-path fields packed for cache locality
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub total_volume: f64,
    pub total_fees: f64,
    pub session_pnl: f64,
    pub fills_count: u64,
    pub maker_fills: u64,
    pub taker_fills: u64,
    pub best_pnl: f64,
    pub rt_count: u64,
    pub rt_profit: f64,
    pub order_batches: u64,
    pub orders_posted: u64,
    pub cancel_batches: u64,
    pub cancel_requests: u64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            total_volume: 0.0,
            total_fees: 0.0,
            session_pnl: 0.0,
            fills_count: 0,
            maker_fills: 0,
            taker_fills: 0,
            best_pnl: 0.0,
            rt_count: 0,
            rt_profit: 0.0,
            order_batches: 0,
            orders_posted: 0,
            cancel_batches: 0,
            cancel_requests: 0,
        }
    }

    pub fn cost_per_million(&self) -> f64 {
        if self.total_volume > 0.0 {
            let net = self.total_fees - self.session_pnl;
            net / (self.total_volume / 1e6)
        } else {
            0.0
        }
    }

    pub fn maker_ratio(&self) -> f64 {
        let total = self.maker_fills + self.taker_fills;
        if total > 0 {
            self.maker_fills as f64 / total as f64
        } else {
            0.0
        }
    }

    pub fn rt_ratio(&self) -> f64 {
        if self.rt_count > 0 && self.fills_count > 0 {
            (self.rt_count * 2) as f64 / self.fills_count as f64
        } else {
            0.0
        }
    }

    pub fn fill_ratio(&self) -> f64 {
        if self.orders_posted > 0 {
            self.fills_count as f64 / self.orders_posted as f64
        } else {
            0.0
        }
    }

    pub fn cancel_ratio(&self) -> f64 {
        if self.orders_posted > 0 {
            self.cancel_requests as f64 / self.orders_posted as f64
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventLevel {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiEvent {
    pub ts_ms: u64,
    pub level: EventLevel,
    pub title: String,
    pub details: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistoryPoint {
    pub ts_ms: u64,
    pub mid: f64,
    pub session_pnl: f64,
    pub position: f64,
    pub spread_ticks: i32,
    pub volatility: f64,
    pub inventory_notional: f64,
    pub drawdown: f64,
}

pub fn now_ts_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Serializable snapshot of MmState for the web API (no Instant fields)
#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub token: String,
    pub running: bool,
    pub trading_enabled: bool,
    pub position: f64,
    pub unrealized_pnl: f64,
    pub entry_price: Option<f64>,
    pub equity: f64,
    pub mid_price: Option<f64>,
    pub spread_ticks: i32,
    pub volatility: f64,
    pub signals: SignalState,
    pub stats: Stats,
    pub buy_orders: usize,
    pub sell_orders: usize,
    pub size_scale: f64,
    pub last_status: String,
    pub uptime_secs: u64,
    pub start_ts_ms: u64,
    pub recent_events: Vec<UiEvent>,
    pub metrics_history: Vec<HistoryPoint>,
    // Avellaneda-Stoikov
    pub t_remaining_secs: f64,
    pub reservation_price: f64,
    pub as_delta: f64,
}

/// Core market maker state
pub struct MmState {
    // Position
    pub position: f64,
    pub unrealized_pnl: f64,
    pub entry_price: Option<f64>,
    pub position_margin: f64,
    pub equity: f64,

    // BBO cache
    pub bbo: Option<Bbo>,

    // Orders
    pub buy_oids: Vec<u64>,
    pub sell_oids: Vec<u64>,
    pub order_details: HashMap<u64, TrackedOrder>,
    pub fill_lock_side: Option<Side>,
    pub fill_lock_until: f64,
    pub cancel_pending_since: Option<f64>,
    pub last_cancel_check_ts: f64,

    // Anchoring
    pub anchor_price: Option<f64>,
    pub anchor_spread_ticks: Option<i32>,

    // Volatility
    pub price_history: VecDeque<f64>,
    pub volatility: f64,
    pub signals: SignalState,

    // Round-trip
    pub pending_rts: Vec<PendingRt>,
    pub last_fill_time_rt: f64,
    pub waiting_for_rt: bool,
    pub rt_spread_adj: i32,

    // Toxic flow detection
    pub consecutive_buy_fills: u32,
    pub consecutive_sell_fills: u32,
    pub adverse_fill_count: u32,  // fills where price moved against us after fill

    // Adverse markout tracking
    pub fill_history: VecDeque<FillRecord>, // recent fills for markout sampling
    pub markout_score: f64,                 // 0.0–1.0: fraction of recent fills that were adverse

    // Timing
    pub last_quote_ts: f64,
    pub last_api_sync_ts: f64,
    pub last_sync_time: f64,
    pub last_close_ts: f64,
    pub last_ws_msg_ts: f64,

    // Margin
    pub size_scale: f64,
    pub margin_pause_until: f64,
    pub momentum_pause_until: f64,

    // Spread
    pub current_spread_ticks: i32,

    // Stats
    pub stats: Stats,
    pub processed_fill_ids: HashSet<String>,

    // Avellaneda-Stoikov
    pub as_t0_ms: u64,          // start of current T-window (ms)
    pub t_remaining_secs: f64,  // T - t (counts down)
    pub reservation_price: f64, // last computed r
    pub as_delta: f64,          // last computed δ*

    // Session
    pub start_time: Instant,
    pub start_ts_ms: u64,
    pub running: bool,
    pub trading_enabled: bool,
    pub manual_mode: bool,
    pub error_message: Option<String>,
    pub last_status: String,
    pub recent_events: VecDeque<UiEvent>,
    pub metrics_history: VecDeque<HistoryPoint>,
    pub last_metrics_sample_ms: u64,
}

impl MmState {
    pub fn new() -> Self {
        let now_ms = now_ts_ms();

        let mut state = Self {
            position: 0.0,
            unrealized_pnl: 0.0,
            entry_price: None,
            position_margin: 0.0,
            equity: 0.0,
            bbo: None,
            buy_oids: Vec::new(),
            sell_oids: Vec::new(),
            order_details: HashMap::new(),
            fill_lock_side: None,
            fill_lock_until: 0.0,
            cancel_pending_since: None,
            last_cancel_check_ts: 0.0,
            anchor_price: None,
            anchor_spread_ticks: None,
            price_history: VecDeque::with_capacity(64),
            volatility: 0.0,
            signals: SignalState::neutral(),
            pending_rts: Vec::new(),
            last_fill_time_rt: 0.0,
            waiting_for_rt: false,
            rt_spread_adj: 0,
            consecutive_buy_fills: 0,
            consecutive_sell_fills: 0,
            adverse_fill_count: 0,
            fill_history: VecDeque::with_capacity(50),
            markout_score: 0.0,
            last_quote_ts: 0.0,
            last_api_sync_ts: 0.0,
            last_sync_time: 0.0,
            last_close_ts: 0.0,
            last_ws_msg_ts: 0.0,
            size_scale: 1.0,
            margin_pause_until: 0.0,
            momentum_pause_until: 0.0,
            current_spread_ticks: 30,
            stats: Stats::new(),
            processed_fill_ids: HashSet::new(),
            as_t0_ms: now_ms,
            t_remaining_secs: 1800.0,
            reservation_price: 0.0,
            as_delta: 0.0,
            start_time: Instant::now(),
            start_ts_ms: now_ms,
            running: true,
            trading_enabled: true,
            manual_mode: false,
            error_message: None,
            last_status: "Initializing...".to_string(),
            recent_events: VecDeque::with_capacity(96),
            metrics_history: VecDeque::with_capacity(360),
            last_metrics_sample_ms: 0,
        };

        state.push_event(
            EventLevel::Info,
            "Runtime",
            "Bot initialized and waiting for market data",
        );
        state.record_metrics_sample();
        state
    }

    pub fn push_event<T, U>(&mut self, level: EventLevel, title: T, details: U)
    where
        T: Into<String>,
        U: Into<String>,
    {
        self.recent_events.push_front(UiEvent {
            ts_ms: now_ts_ms(),
            level,
            title: title.into(),
            details: details.into(),
        });

        while self.recent_events.len() > 80 {
            self.recent_events.pop_back();
        }
    }

    pub fn set_status<S>(&mut self, status: S)
    where
        S: Into<String>,
    {
        self.set_status_with_level(EventLevel::Info, status);
    }

    pub fn set_status_with_level<S>(&mut self, level: EventLevel, status: S)
    where
        S: Into<String>,
    {
        let status = status.into();
        if self.last_status != status {
            self.push_event(level, "Status", status.clone());
        }
        self.last_status = status;
    }

    pub fn set_runtime_error<S>(&mut self, message: S)
    where
        S: Into<String>,
    {
        let message = message.into();
        self.error_message = Some(message.clone());
        self.trading_enabled = false;
        self.set_status_with_level(EventLevel::Error, message);
    }

    pub fn clear_runtime_error(&mut self) {
        self.error_message = None;
    }

    pub fn to_snapshot(&self, token: &str) -> StatusSnapshot {
        StatusSnapshot {
            token: token.to_string(),
            running: self.running,
            trading_enabled: self.trading_enabled,
            position: self.position,
            unrealized_pnl: self.unrealized_pnl,
            entry_price: self.entry_price,
            equity: self.equity,
            mid_price: self.bbo.map(|b| b.mid),
            spread_ticks: self.current_spread_ticks,
            volatility: self.volatility,
            signals: self.signals,
            stats: self.stats.clone(),
            buy_orders: self.buy_oids.len(),
            sell_orders: self.sell_oids.len(),
            size_scale: self.size_scale,
            last_status: self.last_status.clone(),
            uptime_secs: self.start_time.elapsed().as_secs(),
            start_ts_ms: self.start_ts_ms,
            recent_events: self.recent_events.iter().cloned().collect(),
            metrics_history: self.metrics_history.iter().cloned().collect(),
            t_remaining_secs: self.t_remaining_secs,
            reservation_price: self.reservation_price,
            as_delta: self.as_delta,
        }
    }

    pub fn record_metrics_sample(&mut self) {
        let now_ms = now_ts_ms();
        if now_ms.saturating_sub(self.last_metrics_sample_ms) < 1_000 {
            return;
        }

        self.last_metrics_sample_ms = now_ms;
        let mid = self.bbo.map(|b| b.mid).unwrap_or(0.0);
        let drawdown = (self.stats.best_pnl - self.stats.session_pnl).max(0.0);

        self.metrics_history.push_back(HistoryPoint {
            ts_ms: now_ms,
            mid,
            session_pnl: self.stats.session_pnl,
            position: self.position,
            spread_ticks: self.current_spread_ticks,
            volatility: self.volatility,
            inventory_notional: self.position * mid,
            drawdown,
        });

        while self.metrics_history.len() > 360 {
            self.metrics_history.pop_front();
        }
    }
}
