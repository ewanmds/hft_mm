/// Shared trading logic used by both terminal (main.rs) and headless (bot.rs) modes.

use anyhow::Result;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    config.timing.rt_wait_sec.clamp(0.25, 1.0)
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

/// Process a single WebSocket event into MmState
pub fn process_ws_event(config: &Config, state: &Arc<RwLock<MmState>>, evt: WsEvent) {
    let mut s = state.write();
    let now = now_secs();

    match evt {
        WsEvent::L2Update { best_bid, best_ask, mid } => {
            s.bbo = Some(Bbo { best_bid, best_ask, mid });
            s.last_ws_msg_ts = now;
            s.price_history.push_back(mid);
            if s.price_history.len() > config.timing.vol_window {
                s.price_history.pop_front();
            }
            s.volatility = update_volatility(&s.price_history);
            // Update T-t for Avellaneda-Stoikov
            let elapsed = now - (s.as_t0_ms as f64 / 1000.0);
            s.t_remaining_secs = (config.as_model.t_secs - elapsed).max(1.0);
            // Update A-S display values
            let sigma2 = compute_price_variance(&s.price_history, config.as_model.sigma_window).max(1e-18);
            let (r, d) = as_quotes(mid, s.position, sigma2, s.t_remaining_secs, config.as_model.gamma, config.as_model.kappa);
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
                    s.pending_rts.remove(i);
                    rt_matched = true;
                    break;
                }
            }

            if !rt_matched {
                s.pending_rts.push(PendingRt { side: fill_side, price, size, time: now });
                s.pending_rts.retain(|p| (now - p.time) < 300.0);
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

/// Core trading iteration
pub async fn run_iteration(
    config: &Config,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
) {
    use tracing::error;

    let now = now_secs();
    let now_ms = (now * 1000.0) as u64;

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

    // ── Momentum filter: pause quoting during strong directional moves ────────
    if config.momentum.enabled {
        let (still_paused, should_detect) = {
            let s = state.read();
            let still_paused = now < s.momentum_pause_until;
            let can_detect = s.price_history.len() >= config.momentum.lookback;
            (still_paused, can_detect)
        };

        if still_paused {
            let mut s = state.write();
            let remaining = s.momentum_pause_until - now;
            s.last_status = format!("TREND PAUSE {:.0}s", remaining);
            return;
        }

        if should_detect {
            let (is_trending, net_ticks) = {
                let s = state.read();
                let n = s.price_history.len();
                let lookback = config.momentum.lookback.min(n);
                let current = s.price_history[n - 1];
                let past = s.price_history[n - lookback];
                let net_ticks = (current - past).abs() / config.token.tick_size;
                (net_ticks >= config.momentum.min_move_ticks, net_ticks)
            };

            if is_trending {
                // Cancel all open orders and pause
                let cancel_oids: Vec<(String, u64)> = {
                    let mut s = state.write();
                    let oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                        .map(|&oid| (config.token.symbol.clone(), oid)).collect();
                    s.buy_oids.clear();
                    s.sell_oids.clear();
                    s.order_details.clear();
                    s.cancel_pending_since = None;
                    s.momentum_pause_until = now + config.momentum.pause_sec;
                    s.anchor_price = None; // force requote after pause
                    s.push_event(EventLevel::Warn, "TREND", format!(
                        "Momentum detected: {:.1}t move, pausing {:.0}s",
                        net_ticks, config.momentum.pause_sec
                    ));
                    s.last_status = format!("TREND PAUSE {:.0}s ({:.1}t)", config.momentum.pause_sec, net_ticks);
                    oids
                };
                if !cancel_oids.is_empty() {
                    let _ = exchange.bulk_cancel(&cancel_oids).await;
                }
                return;
            }
        }
    }

    // ── Hedge: close position via IOC market order if unrealized loss > hedge_loss_pct ─
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
            // Cancel all open orders first
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
            // Send IOC reduce-only market order to close position
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

    if wait_for_cancel_ack(config, exchange, state, now).await {
        return;
    }

    let (bbo, position, risk_action, refresh) = {
        let s = state.read();
        let bbo = match s.bbo {
            Some(b) => b,
            None => return,
        };
        let risk_action = check_risks(config, &s, now);
        let refresh = should_refresh(config, &s, now, &risk_action);
        (bbo, s.position, risk_action, refresh)
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
        RefreshReason::RtWait(remaining) => {
            let mut s = state.write();
            let rt_r = s.stats.rt_ratio();
            s.last_status = format!(
                "RT WAIT {:.2}s | {}B/{}A | RT:{}({:.0}%)",
                remaining, s.buy_oids.len(), s.sell_oids.len(), s.stats.rt_count, rt_r * 100.0,
            );
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

    let cancel_oids: Vec<(String, u64)> = {
        let s = state.read();
        s.buy_oids.iter().chain(s.sell_oids.iter())
            .map(|&oid| (config.token.symbol.clone(), oid))
            .collect()
    };
    if !cancel_oids.is_empty() {
        let cancel_result = exchange.bulk_cancel(&cancel_oids).await;
        {
            let mut s = state.write();
            match cancel_result {
                Ok(_) => {
                    s.cancel_pending_since = Some(now);
                    s.last_cancel_check_ts = 0.0;
                    s.last_status = format!("Cancel {} -> {} live", reason_str, cancel_oids.len());
                }
                Err(e) => {
                    let short: String = e.to_string().chars().take(96).collect();
                    s.last_status = format!("Cancel error: {}", short);
                }
            }
        }
        return;
    }

    let mut requests: Vec<OrderRequest> = Vec::with_capacity(bid_levels.len() + ask_levels.len());
    for lvl in &bid_levels {
        requests.push(OrderRequest {
            coin: config.token.symbol.clone(),
            is_buy: true,
            sz: lvl.size,
            limit_px: lvl.price,
            reduce_only: false,
            tif: "Alo".to_string(),
        });
    }
    for lvl in &ask_levels {
        requests.push(OrderRequest {
            coin: config.token.symbol.clone(),
            is_buy: false,
            sz: lvl.size,
            limit_px: lvl.price,
            reduce_only: false,
            tif: "Alo".to_string(),
        });
    }

    if !requests.is_empty() {
        let order_result = exchange.bulk_orders(&requests).await;
        {
            let mut s = state.write();
            match order_result {
                Ok(result) => {
                    let (new_buys, new_sells, margin_errs, first_error) =
                        exchange.parse_bulk_result(&result, &requests);
                    let details = extract_order_details(&result, &requests, now);
                    s.buy_oids = new_buys;
                    s.sell_oids = new_sells;
                    s.order_details = details;
                    s.cancel_pending_since = None;

                    let nb = s.buy_oids.len();
                    let na = s.sell_oids.len();

                    if margin_errs > 0 {
                        s.size_scale = (s.size_scale * config.margin.reject_decay)
                            .max(config.margin.min_size_scale);
                        s.margin_pause_until = now + config.margin.reject_cooldown;
                        s.last_status =
                            format!("Margin reject x{} | scale={:.2}", margin_errs, s.size_scale);
                    } else if let Some(ref err) = first_error {
                        let err_short: String = err.chars().take(72).collect();
                        if nb == 0 && na == 0 {
                            s.last_status = format!("Order reject: {}", err_short);
                        } else {
                            s.last_status = format!(
                                "{} -> {}B/{}A | partial: {}", reason_str, nb, na, err_short
                            );
                        }
                    } else if nb > 0 || na > 0 {
                        s.size_scale = (s.size_scale + config.margin.recovery_step).min(1.0);
                    }

                    s.last_quote_ts = now;
                    s.anchor_price = Some(bbo.mid);
                    s.anchor_spread_ticks = Some(
                        ((bbo.best_ask - bbo.best_bid) / config.token.tick_size) as i32,
                    );
                    s.last_sync_time = now;

                    if margin_errs == 0 && first_error.is_none() {
                        s.last_status = format!(
                            "{} -> {}B/{}A @ ${:.2} S:{}t",
                            reason_str, nb, na, bbo.mid, s.current_spread_ticks
                        );
                    }
                }
                Err(e) => {
                    s.buy_oids.clear();
                    s.sell_oids.clear();
                    s.order_details.clear();
                    let short: String = e.to_string().chars().take(96).collect();
                    s.last_status = format!("Order error: {}", short);
                }
            }
        }
    } else {
        let mut s = state.write();
        s.buy_oids.clear();
        s.sell_oids.clear();
        s.order_details.clear();
        s.last_status = format!("{} -> 0B/0A (no levels)", reason_str);
    }
}
