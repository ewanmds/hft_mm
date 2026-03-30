use anyhow::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};
use tracing::{error, info};

use crate::config::{available_tokens, default_config, Config};
use crate::exchange::HyperLiquidExchange;
use crate::trading::{now_secs, process_ws_event, run_iteration};
use crate::types::{Bbo, EventLevel, MmState, StatusSnapshot};
use crate::websocket::{connect_ws, WsSharedState};

fn now_ts_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishedSession {
    pub token: String,
    pub exchange_name: String,
    pub start_ts_ms: u64,
    pub end_ts_ms: u64,
    pub duration_secs: u64,
    pub gross_pnl: f64,
    pub net_pnl: f64,
    pub volume: f64,
    pub fees: f64,
    pub fills: u64,
    pub rt_count: u64,
    pub cost_per_million: f64,
    pub avg_queue_time: f64,
    pub avg_spread_capture: f64,
    pub cancel_repost_count: u64,
    pub markout_1s_avg: f64,
    pub markout_5s_avg: f64,
    pub markout_10s_avg: f64,
    pub stop_reason: String,
}

struct ActiveSession {
    token_name: String,
    config: Arc<RwLock<Config>>,
    mm_state: Arc<RwLock<MmState>>,
    shutdown_tx: watch::Sender<bool>,
}

pub struct BotManager {
    active: Arc<RwLock<Option<ActiveSession>>>,
    sessions: Arc<RwLock<Vec<FinishedSession>>>,
    sessions_path: String,
}

impl BotManager {
    pub fn new() -> Self {
        let sessions_path = "sessions.json".to_string();
        let sessions = Self::load_sessions(&sessions_path);
        Self {
            active: Arc::new(RwLock::new(None)),
            sessions: Arc::new(RwLock::new(sessions)),
            sessions_path,
        }
    }

    fn load_sessions(path: &str) -> Vec<FinishedSession> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => vec![],
        }
    }

    fn save_sessions(&self) {
        let sessions = self.sessions.read();
        if let Ok(data) = serde_json::to_string_pretty(&*sessions) {
            let _ = std::fs::write(&self.sessions_path, data);
        }
    }

    pub fn is_running(&self) -> bool {
        self.active.read().is_some()
    }

    pub fn status(&self) -> Option<StatusSnapshot> {
        let active = self.active.read();
        let session = active.as_ref()?;
        let state = session.mm_state.read();
        let config = session.config.read();
        Some(state.to_snapshot(&session.token_name))
    }

    pub fn sessions(&self) -> Vec<FinishedSession> {
        self.sessions.read().clone()
    }

    pub async fn start(
        &self,
        token_name: &str,
        order_size_usd: Option<f64>,
        _leverage: Option<f64>,
        time_limit_secs: Option<u64>,
    ) -> Result<()> {
        if self.is_running() {
            anyhow::bail!("Bot already running");
        }

        let tokens = available_tokens();
        let mut token_config = tokens
            .get(token_name)
            .ok_or_else(|| anyhow::anyhow!("Unknown token: {}", token_name))?
            .clone();

        // Apply overrides
        if let Some(size) = order_size_usd {
            token_config.order_size_usd = size.max(token_config.min_size);
        }

        let config = default_config(token_config);

        if config.agent_private_key.is_empty() || config.account_address.is_empty() {
            anyhow::bail!("HL_AGENT_KEY and HL_ACCOUNT environment variables are required");
        }

        let token_name_owned = token_name.to_string();
        let exchange = Arc::new(HyperLiquidExchange::new(&config).await?);
        let config_arc = Arc::new(RwLock::new(config));
        let mm_state = Arc::new(RwLock::new(MmState::new()));

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Start the trading task
        let exchange_clone = exchange.clone();
        let config_clone = config_arc.clone();
        let state_clone = mm_state.clone();
        let token_clone = token_name_owned.clone();
        let active_sessions = self.sessions.clone();
        let active_lock = self.active.clone();
        let sessions_path = self.sessions_path.clone();
        let start_ts = now_ts_ms();

        tokio::spawn(async move {
            let stop_reason = match run_headless(
                &config_clone,
                &exchange_clone,
                &state_clone,
                shutdown_rx,
                time_limit_secs,
            )
            .await
            {
                Ok(reason) => reason,
                Err(e) => {
                    error!("Bot error: {}", e);
                    format!("Error: {}", e)
                }
            };

            // Record finished session
            let finished = {
                let s = state_clone.read();
                let stats = &s.stats;
                let end_ts = now_ts_ms();
                let config_snap = config_clone.read();
                let exchange_name = if config_snap.token.symbol.starts_with("xyz:") {
                    "TradeXYZ"
                } else {
                    "KiloMarkets"
                }.to_string();
                FinishedSession {
                    token: token_clone.clone(),
                    exchange_name,
                    start_ts_ms: start_ts,
                    end_ts_ms: end_ts,
                    duration_secs: (end_ts - start_ts) / 1000,
                    gross_pnl: stats.session_pnl,
                    net_pnl: stats.session_pnl - stats.total_fees,
                    volume: stats.total_volume,
                    fees: stats.total_fees,
                    fills: stats.fills_count,
                    rt_count: stats.rt_count,
                    cost_per_million: stats.cost_per_million(),
                    avg_queue_time: stats.avg_queue_time(),
                    avg_spread_capture: stats.avg_spread_capture(),
                    cancel_repost_count: stats.cancel_repost_count,
                    markout_1s_avg: stats.avg_markout_1s(),
                    markout_5s_avg: stats.avg_markout_5s(),
                    markout_10s_avg: stats.avg_markout_10s(),
                    stop_reason,
                }
            };

            {
                let mut sessions = active_sessions.write();
                sessions.insert(0, finished);
                // Keep last 100 sessions
                sessions.truncate(100);
            }

            // Persist sessions
            {
                let sessions = active_sessions.read();
                if let Ok(data) = serde_json::to_string_pretty(&*sessions) {
                    let _ = std::fs::write(&sessions_path, data);
                }
            }

            // Clear active session
            *active_lock.write() = None;
            info!("Bot session ended for {}", token_clone);
        });

        // Register as active session
        *self.active.write() = Some(ActiveSession {
            token_name: token_name_owned,
            config: config_arc,
            mm_state,
            shutdown_tx,
        });

        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let active = self.active.read();
        if let Some(session) = active.as_ref() {
            let _ = session.shutdown_tx.send(true);
            Ok(())
        } else {
            anyhow::bail!("No active session")
        }
    }
}

/// Run the bot in headless mode (no terminal UI)
async fn run_headless(
    config: &Arc<RwLock<Config>>,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
    mut shutdown_rx: watch::Receiver<bool>,
    time_limit_secs: Option<u64>,
) -> Result<String> {
    use crate::websocket::WsEvent;

    let ws_shared = Arc::new(RwLock::new(WsSharedState::new()));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WsEvent>();

    let (base_url, symbol, account) = {
        let c = config.read();
        (c.base_url.clone(), c.token.symbol.clone(), c.account_address.clone())
    };

    connect_ws(&base_url, &symbol, &account, ws_shared.clone(), event_tx).await?;

    info!("WebSocket connecting for {}...", symbol);

    // Wait for first BBO
    let wait_start = Instant::now();
    let mut got_bbo = false;
    while !got_bbo && wait_start.elapsed() < Duration::from_secs(15) {
        if let Ok(evt) = event_rx.try_recv() {
            if let WsEvent::L2Update { best_bid, best_ask, mid, .. } = evt {
                let mut s = state.write();
                s.bbo = Some(Bbo { best_bid, best_ask, mid });
                s.last_ws_msg_ts = now_secs();
                got_bbo = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    if !got_bbo {
        anyhow::bail!("No market data received in 15s");
    }

    info!("Market data received, starting trading loop for {}", symbol);

    // Optional time limit
    if let Some(limit) = time_limit_secs {
        let shutdown_tx_clone = shutdown_rx.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(limit)).await;
            let mut s = state_clone.write();
            s.running = false;
            s.trading_enabled = false;
            s.set_status_with_level(EventLevel::Warn, "Time limit reached");
            info!("Time limit reached, stopping bot");
        });
    }

    // Main trading loop
    let stop_reason = loop {
        let config_snapshot = config.read().clone();

        // Drain WebSocket events
        let mut n = 0;
        while let Ok(evt) = event_rx.try_recv() {
            process_ws_event(&config_snapshot, state, evt);
            n += 1;
            if n > 50 { break; }
        }

        // Check shutdown
        if *shutdown_rx.borrow() {
            let mut s = state.write();
            s.running = false;
            s.trading_enabled = false;
            break "Stop requested".to_string();
        }

        let should_stop = {
            let s = state.read();
            !s.running
        };
        if should_stop {
            let s = state.read();
            break s.last_status.clone();
        }

        // Trading iteration
        run_iteration(&config_snapshot, exchange, state).await;
        state.write().record_metrics_sample();

        tokio::time::sleep(Duration::from_micros(config_snapshot.timing.refresh_fast_us)).await;
    };

    // Cancel all orders on exit
    let all_oids: Vec<(String, u64)> = {
        let s = state.read();
        let symbol = config.read().token.symbol.clone();
        s.buy_oids.iter().chain(s.sell_oids.iter())
            .map(|&oid| (symbol.clone(), oid))
            .collect()
    };
    if !all_oids.is_empty() {
        let _ = exchange.bulk_cancel(&all_oids).await;
    }

    // Cancel via open orders API
    let (symbol, base) = {
        let c = config.read();
        (c.token.symbol.clone(), c.base_coin().to_string())
    };
    if let Ok(open) = exchange.open_orders().await {
        let cancels: Vec<(String, u64)> = open
            .iter()
            .filter(|o| o.coin == symbol || o.coin == base)
            .map(|o| (o.coin.clone(), o.oid))
            .collect();
        if !cancels.is_empty() {
            let _ = exchange.bulk_cancel(&cancels).await;
        }
    }

    info!("Bot stopped: {}", stop_reason);
    Ok(stop_reason)
}
