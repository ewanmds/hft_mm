/// Shared trading logic used by both terminal (main.rs) and headless (bot.rs) modes.

use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::exchange::{HyperLiquidExchange, OrderRequest};
use crate::risk::{apply_position_limits, check_risks, should_refresh, RefreshReason, RiskAction};
use crate::strategy::{as_quotes, calculate_levels, compute_price_variance, update_volatility};
use crate::types::{Bbo, EventLevel, MmState, PendingRt, Side, TrackedOrder};
use crate::websocket::WsEvent;

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

pub fn fill_lock_secs(config: &Config) -> f64 {
    let _ = config;
    0.25
}

pub fn extract_order_details(
    result: &serde_json::Value,
    requests: &[OrderRequest],
    now: f64,
) -> HashMap<u64, TrackedOrder> {
    let mut details = HashMap::new();
    if let Some(statuses) = result
        .pointer("/response/data/statuses")
        .and_then(|v| v.as_array())
    {
        for (i, status) in statuses.iter().enumerate() {
            let Some(oid) = status
                .get("resting")
                .and_then(|v| v.get("oid"))
                .and_then(|v| v.as_u64())
            else {
                continue;
            };
            let Some(req) = requests.get(i) else {
                continue;
            };
            details.insert(
                oid,
                TrackedOrder {
                    price: req.limit_px,
                    size: req.sz,
                    is_buy: req.is_buy,
                    ts: now,
                    target_price: req.limit_px,
                    drift_ticks: 0,
                },
            );
        }
    }
    details
}

pub async fn wait_for_cancel_ack(
    config: &Config,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
    now: f64,
) -> bool {
    let (pending_since, last_check_ts, tracked_oids) = {
        let s = state.read();
        let Some(pending_since) = s.cancel_pending_since else {
            return false;
        };
        let tracked = s
            .buy_oids
            .iter()
            .chain(s.sell_oids.iter())
            .copied()
            .collect::<HashSet<_>>();
        (pending_since, s.last_cancel_check_ts, tracked)
    };

    if now - last_check_ts < 0.15 {
        return true;
    }

    {
        let mut s = state.write();
        s.last_cancel_check_ts = now;
    }

    match exchange.open_orders().await {
        Ok(open_orders) => {
            let base = config.base_coin().to_string();
            let remaining: HashSet<u64> = open_orders
                .into_iter()
                .filter(|o| o.coin == config.token.symbol || o.coin == base)
                .filter_map(|o| tracked_oids.contains(&o.oid).then_some(o.oid))
                .collect();

            if remaining.is_empty() {
                let mut s = state.write();
                s.buy_oids.retain(|o| remaining.contains(o));
                s.sell_oids.retain(|o| remaining.contains(o));
                s.order_details.retain(|o, _| remaining.contains(o));
                s.cancel_pending_since = None;
                false
            } else if now - pending_since > 3.0 {
                // Timed out — clear tracked state and proceed
                let mut s = state.write();
                s.buy_oids.clear();
                s.sell_oids.clear();
                s.order_details.clear();
                s.cancel_pending_since = None;
                false
            } else {
                true
            }
        }
        Err(_) => {
            // Can't check, wait a bit
            if now - pending_since > 3.0 {
                let mut s = state.write();
                s.buy_oids.clear();
                s.sell_oids.clear();
                s.order_details.clear();
                s.cancel_pending_since = None;
                false
            } else {
                true
            }
        }
    }
}

fn normalized_inventory(position: f64, mid: f64, risk_unit_usd: f64) -> f64 {
    if mid <= 0.0 || risk_unit_usd <= 0.0 {
        return position;
    }
    let unit_size = (risk_unit_usd / mid).max(1e-9);
    position / unit_size
}

fn managed_sides(
    refresh: &RefreshReason,
    bbo: Bbo,
    anchor_price: Option<f64>,
    has_bids: bool,
    has_asks: bool,
) -> (bool, bool) {
    match refresh {
        RefreshReason::NoRefresh => (false, false),
        RefreshReason::Initial => (true, true),
        RefreshReason::StopExit { sell } => (!*sell, *sell),
        RefreshReason::Replenish => (!has_bids, !has_asks),
        RefreshReason::SameSideLock(side) => match side {
            Side::Buy => (false, true),
            Side::Sell => (true, false),
        },
        RefreshReason::UrgentDrift(_) | RefreshReason::NormalDrift(_) => {
            if let Some(anchor) = anchor_price {
                if bbo.mid >= anchor {
                    (true, false)
                } else {
                    (false, true)
                }
            } else {
                (true, true)
            }
        }
        RefreshReason::SpreadChange { .. } | RefreshReason::QuoteTtl(_) | RefreshReason::Periodic => {
            (true, true)
        }
    }
}

/// Process a single WebSocket event into MmState
pub fn process_ws_event(config: &Config, state: &Arc<RwLock<MmState>>, evt: WsEvent) {
    let mut s = state.write();
    let now = now_secs();

    match evt {
        WsEvent::L2Update { best_bid, best_ask, mid, bid_depth_5, ask_depth_5 } => {
            let dt = if s.last_variance_update_ts > 0.0 {
                (now - s.last_variance_update_ts).max(0.0)
            } else {
                0.0
            };
            s.bbo = Some(Bbo { best_bid, best_ask, mid });
            s.last_ws_msg_ts = now;
            s.price_history.push_back(mid);
            if s.price_history.len() > config.timing.vol_window {
                s.price_history.pop_front();
            }
            s.volatility = update_volatility(&s.price_history);

            if dt > 0.01 && s.last_mid_for_variance > 0.0 {
                let change = mid - s.last_mid_for_variance;
                let instant_var = (change * change) / dt.max(1e-6);
                let alpha = (1.0 - (-dt * std::f64::consts::LN_2 / 60.0).exp()).clamp(0.001, 0.5);
                let long_alpha = (1.0 - (-dt * std::f64::consts::LN_2 / 600.0).exp()).clamp(0.0005, 0.1);
                s.ewma_variance = if s.ewma_variance > 0.0 {
                    s.ewma_variance * (1.0 - alpha) + instant_var * alpha
                } else {
                    instant_var
                };
                s.long_term_variance = if s.long_term_variance > 0.0 {
                    s.long_term_variance * (1.0 - long_alpha) + instant_var * long_alpha
                } else {
                    instant_var
                };
            }
            s.last_variance_update_ts = now;
            s.last_mid_for_variance = mid;

            let total_depth = bid_depth_5 + ask_depth_5;
            s.book_imbalance = if total_depth > 0.0 {
                ((bid_depth_5 - ask_depth_5) / total_depth).clamp(-1.0, 1.0)
            } else {
                0.0
            };
            if dt > 0.0 {
                let imb_alpha = (1.0 - (-dt * std::f64::consts::LN_2 / 10.0).exp()).clamp(0.01, 0.5);
                s.book_imbalance_ema =
                    s.book_imbalance_ema * (1.0 - imb_alpha) + s.book_imbalance * imb_alpha;
            } else {
                s.book_imbalance_ema = s.book_imbalance;
            }

            let variance_ratio = if s.long_term_variance > 0.0 {
                s.ewma_variance / s.long_term_variance.max(1e-18)
            } else {
                1.0
            };
            s.vol_regime = if variance_ratio < 0.5 {
                crate::types::VolRegime::Low
            } else if variance_ratio < 2.0 {
                crate::types::VolRegime::Normal
            } else if variance_ratio < 5.0 {
                crate::types::VolRegime::High
            } else {
                crate::types::VolRegime::Extreme
            };

            // Update T-t for Avellaneda-Stoikov
            let elapsed = now - (s.as_t0_ms as f64 / 1000.0);
            s.t_remaining_secs = (config.as_model.t_secs - elapsed).max(1.0);
            // Update A-S display values
            let sigma2 = if s.ewma_variance > 0.0 {
                s.ewma_variance
            } else {
                compute_price_variance(&s.price_history, config.as_model.sigma_window)
            }
            .max(1e-18);
            let q_normalized = normalized_inventory(s.position, mid, config.as_model.risk_unit_usd);
            let effective_t = match s.vol_regime {
                crate::types::VolRegime::High => s.t_remaining_secs.min(150.0),
                crate::types::VolRegime::Extreme => s.t_remaining_secs.min(60.0),
                _ => s.t_remaining_secs,
            };
            let (r, d) = as_quotes(
                mid,
                q_normalized,
                sigma2,
                effective_t,
                config.as_model.gamma,
                config.as_model.kappa,
            );
            s.reservation_price = r;
            s.as_delta = d.min(config.spread.max_spread_ticks * config.token.tick_size);
        }

        WsEvent::Fill { tid, price, size, fee, closed_pnl, oid, is_buy, .. } => {
            if s.processed_fill_ids.contains(&tid) {
                return;
            }
            s.processed_fill_ids.insert(tid);

            s.stats.total_volume += price * size;
            s.stats.fills_count += 1;
            s.stats.total_fees += fee.abs();
            s.stats.session_pnl += closed_pnl;

            let fill_side = if is_buy { Side::Buy } else { Side::Sell };

            // Queue time tracking: fill_ts - order_placed_ts
            if let Some(tracked) = s.order_details.get(&oid) {
                let queue_time = now - tracked.ts;
                if queue_time > 0.0 && queue_time < 300.0 {
                    s.stats.queue_time_sum += queue_time;
                    s.stats.queue_time_count += 1;
                }
            }

            // Spread capture: positive = we sold above mid / bought below mid
            if let Some(bbo) = s.bbo {
                let capture = match fill_side {
                    Side::Buy => bbo.mid - price,   // bought below mid = good
                    Side::Sell => price - bbo.mid,  // sold above mid = good
                };
                s.stats.spread_capture_sum += capture;
                s.stats.spread_capture_count += 1;
            }

            let mut rt_matched = false;
            for i in 0..s.pending_rts.len() {
                if s.pending_rts[i].side != fill_side {
                    let prt = &s.pending_rts[i];
                    let (buy_px, sell_px) = if prt.side == Side::Buy {
                        (prt.price, price)
                    } else {
                        (price, prt.price)
                    };
                    let rt_pnl = (sell_px - buy_px) * size.min(prt.size) - fee.abs();
                    s.stats.rt_count += 1;
                    s.stats.rt_profit += rt_pnl;
                    s.recent_rt_pnls.push_back(rt_pnl);
                    while s.recent_rt_pnls.len() > 10 {
                        s.recent_rt_pnls.pop_front();
                    }
                    s.pending_rts.remove(i);
                    rt_matched = true;
                    break;
                }
            }

            if !rt_matched {
                s.pending_rts.push(PendingRt { side: fill_side, price, size, time: now });
                s.pending_rts.retain(|p| (now - p.time) < 60.0);
            }

            // Record fill for adverse markout tracking (multi-horizon)
            let order_ts = s.order_details.get(&oid).map(|t| t.ts).unwrap_or(now);
            s.fill_history.push_back(crate::types::FillRecord {
                side: fill_side,
                fill_price: price,
                time: now,
                checked: false,
                checked_1s: false,
                checked_5s: false,
                checked_10s: false,
                markout_1s: None,
                markout_5s: None,
                markout_10s: None,
                order_ts,
            });
            while s.fill_history.len() > 50 {
                s.fill_history.pop_front();
            }

            // Recent fill sides for graduated toxic flow detection (Phase 6)
            s.recent_fill_sides.push_back((fill_side, now));
            while s.recent_fill_sides.len() > 20 {
                s.recent_fill_sides.pop_front();
            }

            s.last_fill_time_rt = now;
            s.waiting_for_rt = !rt_matched;
            s.fill_lock_side = Some(fill_side);
            s.fill_lock_until = now + fill_lock_secs(config);

            match fill_side {
                Side::Buy => { s.consecutive_buy_fills += 1; s.consecutive_sell_fills = 0; }
                Side::Sell => { s.consecutive_sell_fills += 1; s.consecutive_buy_fills = 0; }
            }

            let is_maker = s.order_details.contains_key(&oid);
            if is_maker { s.stats.maker_fills += 1; } else { s.stats.taker_fills += 1; }
            s.stats.best_pnl = s.stats.best_pnl.max(s.stats.session_pnl);

            let should_remove = if let Some(tracked) = s.order_details.get_mut(&oid) {
                tracked.size = (tracked.size - size).max(0.0);
                tracked.size < config.token.min_size
            } else {
                false
            };
            if should_remove {
                s.order_details.remove(&oid);
                s.buy_oids.retain(|&o| o != oid);
                s.sell_oids.retain(|&o| o != oid);
            }
        }

        WsEvent::PositionUpdate { position, unrealized_pnl, entry_price, margin_used, equity } => {
            s.position = position;
            s.unrealized_pnl = unrealized_pnl;
            s.entry_price = entry_price;
            s.position_margin = margin_used;
            if equity > 0.0 { s.equity = equity; }
        }

        WsEvent::Disconnected => {
            s.last_status = "WS disconnected, reconnecting...".to_string();
        }
    }
}

/// Sample fill history for adverse markouts (multi-horizon) and update markout_score.
/// Called every iteration.
fn update_markout_score(state: &mut crate::types::MmState, now: f64, config: &Config) {
    let mid = match state.bbo {
        Some(b) => b.mid,
        None => return,
    };
    let tick = config.token.tick_size;
    let threshold = config.risk.markout_threshold_ticks * tick;
    let halflife = config.risk.markout_halflife_sec;

    // Sample markouts at multiple horizons
    for fill in state.fill_history.iter_mut() {
        let age = now - fill.time;

        // 1s horizon
        if !fill.checked_1s && age >= 1.0 {
            fill.checked_1s = true;
            let chg = match fill.side {
                crate::types::Side::Buy => mid - fill.fill_price,  // +ve = price rose (good for buy)
                crate::types::Side::Sell => fill.fill_price - mid, // +ve = price fell (good for sell)
            };
            fill.markout_1s = Some(chg);
            state.stats.markout_1s_sum += chg;
            state.stats.markout_1s_count += 1;
        }

        // 2s horizon (existing — drives markout_score)
        if !fill.checked && age >= config.risk.markout_sample_sec {
            fill.checked = true;
            let adverse = match fill.side {
                crate::types::Side::Buy => mid < fill.fill_price - threshold,
                crate::types::Side::Sell => mid > fill.fill_price + threshold,
            };
            if adverse {
                state.adverse_fill_count += 1;
            }
        }

        // 5s horizon
        if !fill.checked_5s && age >= 5.0 {
            fill.checked_5s = true;
            let chg = match fill.side {
                crate::types::Side::Buy => mid - fill.fill_price,
                crate::types::Side::Sell => fill.fill_price - mid,
            };
            fill.markout_5s = Some(chg);
            state.stats.markout_5s_sum += chg;
            state.stats.markout_5s_count += 1;
        }

        // 10s horizon
        if !fill.checked_10s && age >= 10.0 {
            fill.checked_10s = true;
            let chg = match fill.side {
                crate::types::Side::Buy => mid - fill.fill_price,
                crate::types::Side::Sell => fill.fill_price - mid,
            };
            fill.markout_10s = Some(chg);
            state.stats.markout_10s_sum += chg;
            state.stats.markout_10s_count += 1;
        }
    }

    // Recompute markout_score: exponentially weighted adverse ratio (uses 2s horizon)
    let mut weighted_adverse = 0.0_f64;
    let mut weight_total = 0.0_f64;
    for fill in state.fill_history.iter() {
        if !fill.checked { continue; }
        let age = (now - fill.time).max(0.0);
        let w = (-age * std::f64::consts::LN_2 / halflife).exp();
        let adverse = match fill.side {
            crate::types::Side::Buy => mid < fill.fill_price - threshold,
            crate::types::Side::Sell => mid > fill.fill_price + threshold,
        };
        if adverse { weighted_adverse += w; }
        weight_total += w;
    }
    state.markout_score = if weight_total > 0.0 {
        (weighted_adverse / weight_total).min(1.0)
    } else {
        0.0
    };
}

/// Core trading iteration
pub async fn run_iteration(
    config: &Config,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
) {
    use tracing::error;

    let now = now_secs();
    let now_ms = (now * 1000.0) as u64;

    // ── Adverse markout score update ───────────────────────────────────────
    {
        let mut s = state.write();
        update_markout_score(&mut s, now, config);
    }

    // ── Avellaneda-Stoikov: T-window reset ────────────────────────────────
    {
        let (needs_reset, should_flatten, position, mid) = {
            let s = state.read();
            let elapsed = now - (s.as_t0_ms as f64 / 1000.0);
            let needs_reset = elapsed >= config.as_model.t_secs;
            let inv_notional = s.position.abs() * s.bbo.map(|b| b.mid).unwrap_or(0.0);
            let should_flatten = needs_reset && inv_notional > config.as_model.flatten_threshold_usd;
            (needs_reset, should_flatten, s.position, s.bbo.map(|b| b.mid).unwrap_or(0.0))
        };
        if needs_reset {
            {
                let mut s = state.write();
                s.as_t0_ms = now_ms;
                s.t_remaining_secs = config.as_model.t_secs;
                s.push_event(EventLevel::Info, "A-S Reset", format!(
                    "T-window reset | pos={:.4} | t_secs={:.0}", position, config.as_model.t_secs
                ));
            }
            if should_flatten && mid > 0.0 {
                // Flatten residual inventory with a market order (reduce-only)
                let flatten_size = (position.abs()).max(config.token.min_size);
                let is_buy = position < 0.0; // buy to close short, sell to close long
                let req = crate::exchange::OrderRequest {
                    coin: config.token.symbol.clone(),
                    is_buy,
                    sz: flatten_size,
                    limit_px: if is_buy { mid * 1.01 } else { mid * 0.99 },
                    reduce_only: true,
                    tif: "Ioc".to_string(),
                };
                let _ = exchange.bulk_orders(&[req]).await;
                let mut s = state.write();
                s.push_event(EventLevel::Warn, "A-S Flatten", format!(
                    "Session end flatten: {} {:.4} units", if is_buy { "BUY" } else { "SELL" }, flatten_size
                ));
            }
        }
    }

    // ── A-S max drawdown stop ─────────────────────────────────────────────
    {
        let session_pnl = { state.read().stats.session_pnl };
        if session_pnl < -config.as_model.max_loss_usd {
            let mut s = state.write();
            s.running = false;
            s.set_status_with_level(
                EventLevel::Error,
                format!("A-S STOP: max loss ${:.2} reached (PnL ${:.4})",
                    config.as_model.max_loss_usd, session_pnl),
            );
            return;
        }
    }

    // ── Hedge: close position via IOC market order if unrealized loss > hedge_loss_pct ─
    // NOTE: runs BEFORE momentum filter so position protection is always active
    if config.risk.hedge_loss_pct > 0.0 {
        let (should_hedge, position, mid, unrealized_pnl) = {
            let s = state.read();
            let mid = s.bbo.map(|b| b.mid).unwrap_or(0.0);
            let notional = s.position.abs() * mid;
            let should = s.position != 0.0
                && mid > 0.0
                && notional > 0.0
                && s.unrealized_pnl < -(notional * config.risk.hedge_loss_pct);
            (should, s.position, mid, s.unrealized_pnl)
        };
        if should_hedge {
            let cancel_oids: Vec<(String, u64)> = {
                let mut s = state.write();
                let oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                    .map(|&oid| (config.token.symbol.clone(), oid)).collect();
                s.buy_oids.clear();
                s.sell_oids.clear();
                s.order_details.clear();
                s.cancel_pending_since = None;
                oids
            };
            if !cancel_oids.is_empty() {
                let _ = exchange.bulk_cancel(&cancel_oids).await;
            }
            let is_buy = position < 0.0;
            let hedge_size = position.abs();
            let req = crate::exchange::OrderRequest {
                coin: config.token.symbol.clone(),
                is_buy,
                sz: hedge_size,
                limit_px: if is_buy { mid * 1.01 } else { mid * 0.99 },
                reduce_only: true,
                tif: "Ioc".to_string(),
            };
            let _ = exchange.bulk_orders(&[req]).await;
            let mut s = state.write();
            s.push_event(EventLevel::Warn, "HEDGE", format!(
                "Position hedged: {} {:.4} @ ${:.2} | upnl={:.4}",
                if is_buy { "BUY" } else { "SELL" }, hedge_size, mid, unrealized_pnl
            ));
            s.last_status = format!("HEDGE {} {:.4} @ ${:.2}", if is_buy { "BUY" } else { "SELL" }, hedge_size, mid);
        }
    }

    // ── Momentum filter: pause quoting during strong directional moves ────────
    if config.momentum.enabled {
        let (still_paused, should_detect, position, mid) = {
            let s = state.read();
            let still_paused = now < s.momentum_pause_until;
            let can_detect = s.price_history.len() >= config.momentum.lookback;
            let mid = s.bbo.map(|b| b.mid).unwrap_or(0.0);
            (still_paused, can_detect, s.position, mid)
        };

        if still_paused {
            let mut s = state.write();
            let remaining = s.momentum_pause_until - now;
            s.last_status = format!("TREND PAUSE {:.0}s", remaining);
            return;
        }

        if should_detect {
            let (is_trending, net_ticks, trend_direction) = {
                let s = state.read();
                let n = s.price_history.len();
                let lookback = config.momentum.lookback.min(n);
                let current = s.price_history[n - 1];
                let past = s.price_history[n - lookback];
                let delta = current - past;
                let net_ticks = delta.abs() / config.token.tick_size;
                (net_ticks >= config.momentum.min_move_ticks, net_ticks, delta)
            };

            if is_trending {
                // Cancel all open orders
                let cancel_oids: Vec<(String, u64)> = {
                    let mut s = state.write();
                    let oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                        .map(|&oid| (config.token.symbol.clone(), oid)).collect();
                    s.buy_oids.clear();
                    s.sell_oids.clear();
                    s.order_details.clear();
                    s.cancel_pending_since = None;
                    s.momentum_pause_until = now + config.momentum.pause_sec;
                    s.anchor_price = None;
                    s.push_event(EventLevel::Warn, "TREND", format!(
                        "{} {:.1}t move — pausing {:.0}s",
                        if trend_direction < 0.0 { "DOWN" } else { "UP" },
                        net_ticks, config.momentum.pause_sec
                    ));
                    s.last_status = format!("TREND PAUSE {:.0}s ({:.1}t)", config.momentum.pause_sec, net_ticks);
                    oids
                };
                if !cancel_oids.is_empty() {
                    let _ = exchange.bulk_cancel(&cancel_oids).await;
                }

                // If we have inventory going against the trend, close it via IOC market order
                // e.g. long position during a dump → sell IOC to cut losses immediately
                let inventory_against_trend =
                    (trend_direction < 0.0 && position > 0.0) ||
                    (trend_direction > 0.0 && position < 0.0);

                if inventory_against_trend && mid > 0.0 {
                    let is_buy = position < 0.0;
                    let close_size = position.abs();
                    let req = crate::exchange::OrderRequest {
                        coin: config.token.symbol.clone(),
                        is_buy,
                        sz: close_size,
                        limit_px: if is_buy { mid * 1.01 } else { mid * 0.99 },
                        reduce_only: true,
                        tif: "Ioc".to_string(),
                    };
                    let _ = exchange.bulk_orders(&[req]).await;
                    let mut s = state.write();
                    s.push_event(EventLevel::Warn, "TREND CLOSE", format!(
                        "Closing {} {:.4} against trend @ ${:.2}",
                        if is_buy { "SHORT" } else { "LONG" }, close_size, mid
                    ));
                }
                return;
            }
        }
    }

    {
        let mut s = state.write();
        if s.last_margin_reject_ts > 0.0 && (now - s.last_margin_reject_ts) >= 60.0 && s.size_scale < 1.0 {
            s.size_scale = (s.size_scale + 0.1).min(1.0);
            s.last_margin_reject_ts = now;
        }
    }

    let (bbo, position, risk_action, refresh, anchor_price, has_bids, has_asks) = {
        let s = state.read();
        let bbo = match s.bbo {
            Some(b) => b,
            None => return,
        };
        let risk_action = check_risks(config, &s, now);
        let refresh = should_refresh(config, &s, now, &risk_action);
        (
            bbo,
            s.position,
            risk_action,
            refresh,
            s.anchor_price,
            !s.buy_oids.is_empty(),
            !s.sell_oids.is_empty(),
        )
    };

    // Use explicit blocks so guards are DEFINITELY dropped before any `.await`
    let emergency_oids: Option<Vec<(String, u64)>> = {
        match &risk_action {
            RiskAction::EmergencyStop(reason) => {
                error!("EMERGENCY STOP: {}", reason);
                let oids = {
                    let mut s = state.write();
                    s.running = false;
                    s.last_status = format!("EMERGENCY: {}", reason);
                    let oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                        .map(|&oid| (config.token.symbol.clone(), oid)).collect();
                    s.buy_oids.clear(); s.sell_oids.clear();
                    s.order_details.clear(); s.cancel_pending_since = None;
                    oids
                };
                Some(oids)
            }
            RiskAction::FeedStale => {
                let oids = {
                    let mut s = state.write();
                    let stale_sec = config.timing.feed_stale_sec;
                    let oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                        .map(|&oid| (config.token.symbol.clone(), oid)).collect();
                    s.buy_oids.clear(); s.sell_oids.clear();
                    s.order_details.clear(); s.cancel_pending_since = None;
                    s.last_status = format!("Feed stale > {:.0}s - pulled", stale_sec);
                    oids
                };
                Some(oids)
            }
            _ => None,
        }
    };
    if let Some(oids) = emergency_oids {
        if !oids.is_empty() { let _ = exchange.bulk_cancel(&oids).await; }
        return;
    }

    let reason_str = match &refresh {
        RefreshReason::NoRefresh => {
            let mut s = state.write();
            let drift = s.anchor_price
                .map(|a| ((bbo.mid - a).abs() / config.token.tick_size) as u32)
                .unwrap_or(0);
            s.last_status = format!("STABLE @ ${:.2} | {}t drift", bbo.mid, drift);
            return;
        }
        RefreshReason::Initial => "Initial",
        RefreshReason::StopExit { sell } => if *sell { "STOP SELL" } else { "STOP BUY" },
        RefreshReason::Replenish => "Replenish",
        RefreshReason::SameSideLock(side) => if *side == Side::Buy { "LOCK BUY" } else { "LOCK SELL" },
        RefreshReason::UrgentDrift(_) => "DRIFT",
        RefreshReason::NormalDrift(_) => "Drift",
        RefreshReason::SpreadChange { .. } => "SPREAD",
        RefreshReason::QuoteTtl(_) => "TTL",
        RefreshReason::Periodic => "Periodic",
    };

    let (mut bid_levels, mut ask_levels, spread_ticks) = {
        let s = state.read();
        calculate_levels(&bbo, position, config, &s)
    };

    {
        let mut s = state.write();
        s.current_spread_ticks = spread_ticks;
    }

    {
        let s = state.read();
        apply_position_limits(config, &s, &mut bid_levels, &mut ask_levels, &refresh, now);
    }

    // ── Modify-in-place: diff desired levels against existing orders ──────────
    // Instead of cancel-all + repost, we:
    //   KEEP  : existing order within keep_ticks of desired price
    //   MODIFY: existing order drift >= modify_ticks from desired price
    //   CANCEL: existing order with no matching desired level
    //   ADD   : desired level with no matching existing order
    let (manage_bids, manage_asks) = managed_sides(&refresh, bbo, anchor_price, has_bids, has_asks);
    if !manage_bids && !manage_asks {
        let mut s = state.write();
        s.last_status = format!("{} | holding opposite side", reason_str);
        return;
    }

    let keep_ticks = 1u32;
    let size_keep_eps = 0.05;
    let tick = config.token.tick_size;

    let all_desired: Vec<(bool, f64, f64)> = bid_levels
        .iter()
        .filter(|_| manage_bids)
        .map(|l| (true, l.price, l.size))
        .chain(
            ask_levels
                .iter()
                .filter(|_| manage_asks)
                .map(|l| (false, l.price, l.size)),
        )
        .collect();

    let (modify_reqs, cancel_oids_diff, add_reqs, kept_updates) = {
        let s = state.read();
        let mut modify_reqs: Vec<crate::exchange::ModifyRequest> = Vec::new();
        let mut cancel_oids_diff: Vec<(String, u64)> = Vec::new();
        let mut add_reqs: Vec<OrderRequest> = Vec::new();
        let mut matched_desired: Vec<bool> = vec![false; all_desired.len()];
        let mut kept_updates: Vec<(u64, f64, f64, u32)> = Vec::new();

        // For each existing order, find the closest desired level of the same side
        for &oid in s
            .buy_oids
            .iter()
            .filter(|_| manage_bids)
            .chain(s.sell_oids.iter().filter(|_| manage_asks))
        {
            let tracked = match s.order_details.get(&oid) {
                Some(t) => t,
                None => {
                    cancel_oids_diff.push((config.token.symbol.clone(), oid));
                    continue;
                }
            };

            // Find the closest unmatched desired level on the same side
            let mut best_match: Option<(usize, f64)> = None;
            for (di, &(is_buy_d, px_d, _)) in all_desired.iter().enumerate() {
                if matched_desired[di] { continue; }
                if is_buy_d != tracked.is_buy { continue; }
                let dist = (tracked.price - px_d).abs() / tick;
                if best_match.is_none() || dist < best_match.unwrap().1 {
                    best_match = Some((di, dist));
                }
            }

            match best_match {
                None => {
                    // No desired level for this order — cancel it
                    cancel_oids_diff.push((config.token.symbol.clone(), oid));
                }
                Some((di, dist_ticks)) => {
                    matched_desired[di] = true;
                    let (_, desired_px, desired_sz) = all_desired[di];
                    let denom = tracked.size.abs().max(desired_sz.abs()).max(config.token.min_size);
                    let size_change_ratio = (tracked.size - desired_sz).abs() / denom;
                    let drift_ticks = dist_ticks.round() as u32;
                    if dist_ticks <= keep_ticks as f64 && size_change_ratio <= size_keep_eps {
                        // Price is close enough — keep as-is
                        kept_updates.push((oid, desired_px, desired_sz, drift_ticks));
                    } else {
                        // Modify to new price (resets queue priority but avoids naked period)
                        modify_reqs.push(crate::exchange::ModifyRequest {
                            oid,
                            new_price: desired_px,
                            new_size: desired_sz,
                            is_buy: tracked.is_buy,
                            reduce_only: false,
                        });
                    }
                }
            }
        }

        // Desired levels that had no matching existing order → post new
        for (di, matched) in matched_desired.iter().enumerate() {
            if !matched {
                let (is_buy, px, sz) = all_desired[di];
                add_reqs.push(OrderRequest {
                    coin: config.token.symbol.clone(),
                    is_buy,
                    sz,
                    limit_px: px,
                    reduce_only: false,
                    tif: "Alo".to_string(),
                });
            }
        }

        (modify_reqs, cancel_oids_diff, add_reqs, kept_updates)
    };

    // If all desired levels are within keep_ticks and nothing to cancel — true stable
    if modify_reqs.is_empty() && cancel_oids_diff.is_empty() && add_reqs.is_empty() {
        let mut s = state.write();
        for (oid, target_price, target_size, drift_ticks) in &kept_updates {
            if let Some(tracked) = s.order_details.get_mut(oid) {
                tracked.target_price = *target_price;
                tracked.size = *target_size;
                tracked.drift_ticks = *drift_ticks;
            }
        }
        let drift = s.anchor_price
            .map(|a| ((bbo.mid - a).abs() / tick) as u32)
            .unwrap_or(0);
        s.last_status = format!("STABLE(keep) @ ${:.2} | {}t drift", bbo.mid, drift);
        return;
    }

    // Execute cancels first (for orders that no longer belong)
    if !cancel_oids_diff.is_empty() {
        match exchange.bulk_cancel(&cancel_oids_diff).await {
            Ok(_) => {
                let mut s = state.write();
                for (_, oid) in &cancel_oids_diff {
                    s.buy_oids.retain(|&o| o != *oid);
                    s.sell_oids.retain(|&o| o != *oid);
                    s.order_details.remove(oid);
                }
                if !add_reqs.is_empty() {
                    s.stats.cancel_repost_count += 1;
                }
            }
            Err(e) => {
                let mut s = state.write();
                let short: String = e.to_string().chars().take(96).collect();
                s.last_status = format!("Cancel error: {}", short);
                return;
            }
        }
    }

    // Execute modifies
    if !modify_reqs.is_empty() {
        let mod_result = exchange.bulk_modify(&modify_reqs).await;
        {
            let mut s = state.write();
            match mod_result {
                Ok(_) => {
                    // Update tracked prices for modified orders
                    for m in &modify_reqs {
                        if let Some(tracked) = s.order_details.get_mut(&m.oid) {
                            tracked.price = m.new_price;
                            tracked.size = m.new_size;
                            tracked.target_price = m.new_price;
                            tracked.drift_ticks = 0;
                        }
                    }
                }
                Err(e) => {
                    let short: String = e.to_string().chars().take(80).collect();
                    s.last_status = format!("Modify error: {}", short);
                    // Fall through — add_reqs will still be processed below
                }
            }
        }
    }

    // Post new orders for unmatched levels
    if !add_reqs.is_empty() {
        let order_result = exchange.bulk_orders(&add_reqs).await;
        {
            let mut s = state.write();
            match order_result {
                Ok(result) => {
                    let (new_buys, new_sells, margin_errs, first_error) =
                        exchange.parse_bulk_result(&result, &add_reqs);
                    let new_details = extract_order_details(&result, &add_reqs, now);

                    for (oid, target_price, target_size, drift_ticks) in &kept_updates {
                        if let Some(tracked) = s.order_details.get_mut(oid) {
                            tracked.target_price = *target_price;
                            tracked.size = *target_size;
                            tracked.drift_ticks = *drift_ticks;
                        }
                    }
                    for oid in new_buys {
                        if !s.buy_oids.contains(&oid) {
                            s.buy_oids.push(oid);
                        }
                    }
                    for oid in new_sells {
                        if !s.sell_oids.contains(&oid) {
                            s.sell_oids.push(oid);
                        }
                    }
                    for (k, v) in new_details { s.order_details.insert(k, v); }
                    s.cancel_pending_since = None;

                    let nb = s.buy_oids.len();
                    let na = s.sell_oids.len();

                    if margin_errs > 0 {
                        s.size_scale = (s.size_scale * config.margin.reject_decay)
                            .max(config.margin.min_size_scale);
                        s.margin_pause_until = now + config.margin.reject_cooldown;
                        s.last_margin_reject_ts = now;
                        s.last_status =
                            format!("Margin reject x{} | scale={:.2}", margin_errs, s.size_scale);
                    } else if let Some(ref err) = first_error {
                        let err_short: String = err.chars().take(72).collect();
                        s.last_status = format!("{} -> {}B/{}A | {}", reason_str, nb, na, err_short);
                    } else {
                        s.size_scale = (s.size_scale + config.margin.recovery_step).min(1.0);
                        s.last_status = format!(
                            "{} -> {}B/{}A @ ${:.2} S:{}t (mod:{} add:{})",
                            reason_str, nb, na, bbo.mid, s.current_spread_ticks,
                            modify_reqs.len(), add_reqs.len()
                        );
                    }
                }
                Err(e) => {
                    let short: String = e.to_string().chars().take(96).collect();
                    s.last_status = format!("Order error: {}", short);
                }
            }
            s.last_quote_ts = now;
            s.anchor_price = Some(bbo.mid);
            s.anchor_spread_ticks = Some(
                ((bbo.best_ask - bbo.best_bid) / tick) as i32,
            );
            s.last_sync_time = now;
        }
    } else {
        // Only modifies, no new orders
        let mut s = state.write();
        // Rebuild OID lists: keep kept + modified
        for (oid, target_price, target_size, drift_ticks) in &kept_updates {
            if let Some(tracked) = s.order_details.get_mut(oid) {
                tracked.target_price = *target_price;
                tracked.size = *target_size;
                tracked.drift_ticks = *drift_ticks;
            }
        }
        s.cancel_pending_since = None;
        s.last_quote_ts = now;
        s.anchor_price = Some(bbo.mid);
        s.anchor_spread_ticks = Some(((bbo.best_ask - bbo.best_bid) / tick) as i32);
        s.last_sync_time = now;
        let nb = s.buy_oids.len();
        let na = s.sell_oids.len();
        s.last_status = format!(
            "{} -> {}B/{}A mod:{} add:{} keep:{} cancel:{}",
            reason_str,
            nb,
            na,
            modify_reqs.len(),
            add_reqs.len(),
            kept_updates.len(),
            cancel_oids_diff.len()
        );
        // Only cancels happened, no desired levels — clear everything
    }
}
