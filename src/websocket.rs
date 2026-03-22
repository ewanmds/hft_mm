use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::types::{Bbo, PendingRt, Side};

/// Events pushed from WebSocket to the main loop
#[derive(Debug)]
pub enum WsEvent {
    /// New L2 book update
    L2Update {
        best_bid: f64,
        best_ask: f64,
        mid: f64,
    },
    /// Fill received
    Fill {
        tid: String,
        coin: String,
        price: f64,
        size: f64,
        fee: f64,
        closed_pnl: f64,
        oid: u64,
        is_buy: bool,
    },
    /// Position update
    PositionUpdate {
        position: f64,
        unrealized_pnl: f64,
        entry_price: Option<f64>,
        margin_used: f64,
        equity: f64,
    },
    /// Connection lost
    Disconnected,
}

/// Shared state updated atomically by the WS reader thread
pub struct WsSharedState {
    pub bbo: Option<Bbo>,
    pub last_msg_ts: f64,
    pub ready: bool,
}

impl WsSharedState {
    pub fn new() -> Self {
        Self {
            bbo: None,
            last_msg_ts: 0.0,
            ready: false,
        }
    }
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Launch WebSocket connection and return event channel
pub async fn connect_ws(
    base_url: &str,
    symbol: &str,
    account_address: &str,
    shared: Arc<RwLock<WsSharedState>>,
    event_tx: mpsc::UnboundedSender<WsEvent>,
) -> Result<()> {
    // Convert HTTP URL to WebSocket URL
    let ws_url = base_url
        .replace("https://", "wss://")
        .replace("http://", "ws://")
        + "/ws";

    let symbol = symbol.to_string();
    let account = account_address.to_string();

    tokio::spawn(async move {
        loop {
            match connect_and_run(&ws_url, &symbol, &account, &shared, &event_tx).await {
                Ok(_) => {
                    warn!("WebSocket closed normally, reconnecting...");
                }
                Err(e) => {
                    error!("WebSocket error: {}, reconnecting in 1s...", e);
                }
            }
            let _ = event_tx.send(WsEvent::Disconnected);
            {
                let mut s = shared.write();
                s.ready = false;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    });

    Ok(())
}

async fn connect_and_run(
    ws_url: &str,
    symbol: &str,
    account: &str,
    shared: &Arc<RwLock<WsSharedState>>,
    event_tx: &mpsc::UnboundedSender<WsEvent>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    info!("WebSocket connected to {}", ws_url);

    // Subscribe to L2 book
    let l2_sub = json!({
        "method": "subscribe",
        "subscription": {
            "type": "l2Book",
            "coin": symbol,
        }
    });
    write.send(Message::Text(l2_sub.to_string())).await?;

    // Subscribe to user events
    let user_sub = json!({
        "method": "subscribe",
        "subscription": {
            "type": "userEvents",
            "user": account,
        }
    });
    write.send(Message::Text(user_sub.to_string())).await?;

    // Ping task
    let ping_write = Arc::new(tokio::sync::Mutex::new(write));
    let ping_handle = {
        let pw = ping_write.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
                let mut w = pw.lock().await;
                if w.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        })
    };

    // Read loop — performance critical
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // Fast path: parse JSON
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    process_message(&data, symbol, account, shared, event_tx);
                }
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(e) => {
                error!("WS read error: {}", e);
                break;
            }
            _ => {}
        }
    }

    ping_handle.abort();
    Ok(())
}

/// Process incoming WS message — inlined for speed
#[inline]
fn process_message(
    data: &Value,
    symbol: &str,
    account: &str,
    shared: &Arc<RwLock<WsSharedState>>,
    event_tx: &mpsc::UnboundedSender<WsEvent>,
) {
    let channel = data.get("channel").and_then(|v| v.as_str()).unwrap_or("");

    match channel {
        "l2Book" => process_l2(data, symbol, shared, event_tx),
        "user" => process_user(data, symbol, event_tx),
        _ => {
            // Check nested data structure
            if let Some(inner) = data.get("data") {
                if inner.get("levels").is_some() {
                    process_l2(data, symbol, shared, event_tx);
                } else if inner.get("fills").is_some() || inner.get("clearinghouseState").is_some() {
                    process_user(data, symbol, event_tx);
                }
            }
        }
    }
}

#[inline]
fn process_l2(
    data: &Value,
    symbol: &str,
    shared: &Arc<RwLock<WsSharedState>>,
    event_tx: &mpsc::UnboundedSender<WsEvent>,
) {
    let payload = data.get("data").unwrap_or(data);
    let levels = match payload.get("levels").and_then(|v| v.as_array()) {
        Some(l) if l.len() >= 2 => l,
        _ => return,
    };

    let bids = match levels[0].as_array() {
        Some(b) if !b.is_empty() => b,
        _ => return,
    };
    let asks = match levels[1].as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };

    let best_bid: f64 = match bids[0].get("px").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    let best_ask: f64 = match asks[0].get("px").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => return,
    };
    let mid = (best_bid + best_ask) * 0.5;

    // Update shared state (lock-free write via parking_lot)
    {
        let mut s = shared.write();
        s.bbo = Some(Bbo { best_bid, best_ask, mid });
        s.last_msg_ts = now_secs();
        s.ready = true;
    }

    let _ = event_tx.send(WsEvent::L2Update { best_bid, best_ask, mid });
}

#[inline]
fn process_user(
    data: &Value,
    symbol: &str,
    event_tx: &mpsc::UnboundedSender<WsEvent>,
) {
    let payload = data.get("data").unwrap_or(data);
    let base = symbol.split(':').last().unwrap_or(symbol);

    // Position update
    if let Some(ch) = payload.get("clearinghouseState") {
        let mut position = 0.0f64;
        let mut unrealized = 0.0f64;
        let mut entry_px: Option<f64> = None;
        let mut margin = 0.0f64;

        if let Some(positions) = ch.get("assetPositions").and_then(|v| v.as_array()) {
            for entry in positions {
                let pos = &entry["position"];
                let coin = pos["coin"].as_str().unwrap_or("");

                if coin == symbol || coin == base || symbol.ends_with(coin) {
                    position = pos["szi"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    unrealized = pos["unrealizedPnl"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    entry_px = pos["entryPx"].as_str()
                        .and_then(|s| s.parse().ok())
                        .filter(|&v: &f64| v > 0.0);
                    margin = pos["marginUsed"].as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    break;
                }
            }
        }

        let equity = ch.get("marginSummary")
            .and_then(|ms| ms.get("accountValue"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);

        let _ = event_tx.send(WsEvent::PositionUpdate {
            position,
            unrealized_pnl: unrealized,
            entry_price: entry_px,
            margin_used: margin,
            equity,
        });
    }

    // Fill processing
    if let Some(fills) = payload.get("fills").and_then(|v| v.as_array()) {
        for fill in fills {
            let coin = fill["coin"].as_str().unwrap_or("");
            if coin != symbol && coin != base && !symbol.ends_with(coin) {
                continue;
            }

            let tid = fill.get("tid").map(|v| v.to_string()).unwrap_or_default();
            let price: f64 = fill["px"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let size: f64 = fill["sz"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let fee: f64 = fill.get("fee").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let closed_pnl: f64 = fill.get("closedPnl").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let oid: u64 = fill.get("oid").and_then(|v| v.as_u64()).unwrap_or(0);

            let dir = fill.get("dir").and_then(|v| v.as_str()).unwrap_or("");
            let is_buy = dir.contains("Long") || dir.contains("Buy");

            let _ = event_tx.send(WsEvent::Fill {
                tid,
                coin: coin.to_string(),
                price,
                size,
                fee,
                closed_pnl,
                oid,
                is_buy,
            });
        }
    }
}
