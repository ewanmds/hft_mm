use crate::config::Config;
use crate::types::{MmState, OrderLevel, Side};

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
    RtWait(f64),
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
    let fill_lock_active = now < state.fill_lock_until;

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

    if fill_lock_active {
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

    // RT wait cooldown
    let time_since_fill = if state.last_fill_time_rt > 0.0 {
        now - state.last_fill_time_rt
    } else {
        999.0
    };
    let in_rt_wait = time_since_fill < config.timing.rt_wait_sec && state.waiting_for_rt;

    if in_rt_wait && has_both {
        return RefreshReason::RtWait(config.timing.rt_wait_sec - time_since_fill);
    }

    // Missing one side
    if has_any && !has_both && !in_rt_wait {
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

    if drift_ticks >= config.timing.drift_ticks && !in_rt_wait {
        return RefreshReason::NormalDrift(drift_ticks);
    }

    // Spread change
    if let Some(anchor_spread) = state.anchor_spread_ticks {
        if !in_rt_wait {
            let cur_spread = ((bbo.best_ask - bbo.best_bid) / tick) as i32;
            if (cur_spread - anchor_spread).abs() >= 3 {
                return RefreshReason::SpreadChange {
                    from: anchor_spread,
                    to: cur_spread,
                };
            }
        }
    }

    // Quote TTL
    let quote_age = if state.last_quote_ts > 0.0 {
        now - state.last_quote_ts
    } else {
        999.0
    };

    if quote_age >= config.timing.max_quote_ttl && !in_rt_wait {
        return RefreshReason::QuoteTtl(quote_age);
    }

    // Periodic sync
    if (now - state.last_sync_time) > config.timing.periodic_sync_sec && !in_rt_wait {
        return RefreshReason::Periodic;
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

    // Soft toxic flow block: 2+ consecutive same-side fills building inventory →
    // stop posting the building side and let the skew attract opposite fills.
    // This avoids taker fees from market exits while preventing further accumulation.
    if state.consecutive_buy_fills >= 2 && position > 0.0 {
        bids.clear();
    }
    if state.consecutive_sell_fills >= 2 && position < 0.0 {
        asks.clear();
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
