use crate::config::Config;
use crate::types::{MmState, Side};

/// Risk check result
#[derive(Debug)]
pub enum RiskAction {
    /// All clear, continue trading
    Continue,
    /// Kill the bot with reason
    EmergencyStop(String),
    /// Need to exit position (direction: true=sell, false=buy)
    ExitPosition { sell: bool },
    /// Feed is stale, pull all orders
    FeedStale,
}

/// Run all risk checks — called every iteration
#[inline]
pub fn check_risks(config: &Config, state: &MmState, now: f64) -> RiskAction {
    // 1. Cost per million kill switch
    if state.stats.total_volume >= config.risk.cost_m_min_vol {
        let cpm = state.stats.cost_per_million();
        if cpm > config.risk.cost_m_kill {
            return RiskAction::EmergencyStop(format!("Cost/1M ${:.1}", cpm));
        }
    }

    // 2. Session PnL kill switch
    if state.stats.fills_count >= config.risk.min_fills_kill
        && state.stats.session_pnl < config.risk.session_pnl_kill
    {
        return RiskAction::EmergencyStop(format!(
            "PnL ${:.4}",
            state.stats.session_pnl
        ));
    }

    // 3. Feed stale check
    if state.last_ws_msg_ts > 0.0
        && (now - state.last_ws_msg_ts) > config.timing.feed_stale_sec
    {
        return RiskAction::FeedStale;
    }

    // 4. Stop loss on open position
    if state.position.abs() > 0.001 && state.unrealized_pnl < -config.risk.stop_loss_usd {
        let sell = state.position > 0.0;
        return RiskAction::ExitPosition { sell };
    }

    // 5. Toxic flow: handled via soft block in apply_position_limits (consecutive >= 2 →
    // stop posting the building side). No market exit here — taker fees are too expensive.
    // The stop_loss above is the backstop for extreme adverse moves.

    RiskAction::Continue
}

/// Determine if we should refresh orders and why
#[derive(Debug)]
pub enum RefreshReason {
    NoRefresh,
    Initial,
    StopExit { sell: bool },
    Replenish,
    SameSideLock(Side),
    UrgentDrift(u32),
    NormalDrift(u32),
    SpreadChange { from: i32, to: i32 },
    QuoteTtl(f64),
    Periodic,
}

/// Evaluate whether orders need refreshing
pub fn should_refresh(
    config: &Config,
    state: &MmState,
    now: f64,
    risk: &RiskAction,
) -> RefreshReason {
    let tick = config.token.tick_size;
    let has_bids = !state.buy_oids.is_empty();
    let has_asks = !state.sell_oids.is_empty();
    let has_both = has_bids && has_asks;
    let has_any = has_bids || has_asks;

    let bbo = match state.bbo {
        Some(b) => b,
        None => return RefreshReason::NoRefresh,
    };

    // Emergency exit
    if let RiskAction::ExitPosition { sell } = risk {
        return RefreshReason::StopExit { sell: *sell };
    }

    // No orders at all
    if !has_any {
        return RefreshReason::Initial;
    }

    // Missing one side
    if has_any && !has_both {
        return RefreshReason::Replenish;
    }

    // Price drift
    let drift_ticks = if let Some(anchor) = state.anchor_price {
        ((bbo.mid - anchor).abs() / tick) as u32
    } else {
        0
    };

    if drift_ticks >= config.timing.urgent_drift_ticks {
        return RefreshReason::UrgentDrift(drift_ticks);
    }

    if drift_ticks >= config.timing.drift_ticks {
        return RefreshReason::NormalDrift(drift_ticks);
    }

    // Spread change
    if let Some(anchor_spread) = state.anchor_spread_ticks {
        let cur_spread = ((bbo.best_ask - bbo.best_bid) / tick) as i32;
        if (cur_spread - anchor_spread).abs() >= 3 {
            return RefreshReason::SpreadChange {
                from: anchor_spread,
                to: cur_spread,
            };
        }
    }

    // Quote TTL
    let quote_age = if state.last_quote_ts > 0.0 {
        now - state.last_quote_ts
    } else {
        999.0
    };

    if quote_age >= config.timing.max_quote_ttl {
        return RefreshReason::QuoteTtl(quote_age);
    }

    // Periodic sync
    if (now - state.last_sync_time) > config.timing.periodic_sync_sec {
        return RefreshReason::Periodic;
    }

    if now < state.fill_lock_until {
        if let Some(side) = state.fill_lock_side {
            let locked_orders_live = match side {
                Side::Buy => has_bids,
                Side::Sell => has_asks,
            };
            if locked_orders_live {
                return RefreshReason::SameSideLock(side);
            }
        }
    }

    // Stable
    if has_both {
        return RefreshReason::NoRefresh;
    }

    RefreshReason::Replenish
}

/// Apply position limits to bid/ask levels
pub fn apply_position_limits(
    config: &Config,
    state: &MmState,
    bids: &mut Vec<crate::types::OrderLevel>,
    asks: &mut Vec<crate::types::OrderLevel>,
    reason: &RefreshReason,
    now: f64,
) {
    let mid = state.bbo.map(|b| b.mid).unwrap_or(0.0);
    let position = state.position;

    // Hard position cap: if |pos * mid| > cap, only quote on the reducing side
    if config.risk.position_cap_usd > 0.0 && mid > 0.0 {
        let inv_notional = position.abs() * mid;
        if inv_notional > config.risk.position_cap_usd {
            if position > 0.0 {
                bids.clear(); // too long — no more buys
            } else if position < 0.0 {
                asks.clear(); // too short — no more sells
            }
        }
    }

    // Graduated toxic flow response:
    // combine recent same-side flow imbalance with markout score and only restrict
    // the side that would build more inventory.
    let recent_count = state.recent_fill_sides.len().min(10);
    if recent_count > 0 {
        let recent_buys = state
            .recent_fill_sides
            .iter()
            .rev()
            .take(recent_count)
            .filter(|(side, _)| *side == Side::Buy)
            .count();
        let buy_ratio = recent_buys as f64 / recent_count as f64;
        let flow_imbalance = ((buy_ratio - 0.5).abs() * 2.0).clamp(0.0, 1.0);
        let combined_toxic = (state.markout_score * 0.5 + flow_imbalance * 0.5).clamp(0.0, 1.0);

        let (size_scale, widen_ticks) = if combined_toxic > 0.8 {
            (0.0, 0u32)
        } else if combined_toxic >= 0.6 {
            (0.25, 2u32)
        } else if combined_toxic >= 0.4 {
            (0.5, 1u32)
        } else {
            (1.0, 0u32)
        };

        if position > 0.0 {
            if size_scale == 0.0 {
                bids.clear();
            } else if size_scale < 1.0 || widen_ticks > 0 {
                for level in bids.iter_mut() {
                    level.size = floor_to_decimals(level.size * size_scale, config.size_decimals());
                    if widen_ticks > 0 {
                        level.price = (level.price - widen_ticks as f64 * config.token.tick_size).max(config.token.tick_size);
                    }
                }
                bids.retain(|level| level.size >= config.token.min_size);
            }
        } else if position < 0.0 {
            if size_scale == 0.0 {
                asks.clear();
            } else if size_scale < 1.0 || widen_ticks > 0 {
                for level in asks.iter_mut() {
                    level.size = floor_to_decimals(level.size * size_scale, config.size_decimals());
                    if widen_ticks > 0 {
                        level.price += widen_ticks as f64 * config.token.tick_size;
                    }
                }
                asks.retain(|level| level.size >= config.token.min_size);
            }
        }
    }

    if now < state.fill_lock_until {
        if let Some(side) = state.fill_lock_side {
            match side {
                Side::Buy => bids.clear(),
                Side::Sell => asks.clear(),
            }
        }
    }

    match reason {
        RefreshReason::StopExit { sell } => {
            if *sell {
                bids.clear();
                asks.truncate(3);
            } else {
                asks.clear();
                bids.truncate(3);
            }
        }
        RefreshReason::UrgentDrift(drift) => {
            if let Some(anchor) = state.anchor_price {
                if mid > anchor {
                    bids.truncate(1);
                } else {
                    asks.truncate(1);
                }
            }
        }
        _ => {}
    }
}

fn floor_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).floor() / factor
}
