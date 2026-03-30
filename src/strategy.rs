use crate::config::Config;
use crate::types::{Bbo, MmState, OrderLevel, VolRegime};
use std::collections::VecDeque;

// ─── Avellaneda-Stoikov core ────────────────────────────────────────────────

/// Compute σ² = variance of mid-price changes (USD²/tick).
/// Used directly in the A-S formulas without further unit conversion.
pub fn compute_price_variance(prices: &VecDeque<f64>, window: usize) -> f64 {
    let n = prices.len().min(window);
    if n < 5 {
        return 0.0;
    }
    let start = prices.len() - n;
    let mut sum = 0.0;
    let mut sum2 = 0.0;
    let count = (n - 1) as f64;
    for i in (start + 1)..prices.len() {
        let d = prices[i] - prices[i - 1];
        sum += d;
        sum2 += d * d;
    }
    let mean = sum / count;
    ((sum2 / count) - mean * mean).max(0.0)
}

/// Avellaneda-Stoikov quotes.
///
/// Returns `(reservation_price, half_spread)` in USD.
///
/// Formulae (Avellaneda & Stoikov 2008):
///   r   = s - q · γ · σ² · (T-t)
///   δ*  = γ · σ² · (T-t) / 2  +  (1/γ) · ln(1 + γ/κ)
///
/// Parameters:
///   s    = mid price
///   q    = inventory in normalized risk units
///   γ    = risk aversion (config.as_model.gamma)
///   σ²   = price-change variance per tick (USD²/tick)
///   T-t  = time remaining in session (seconds)
///   κ    = market-order arrival intensity (config.as_model.kappa)
pub fn as_quotes(
    mid: f64,
    position: f64,
    sigma2: f64,
    t_remaining: f64,
    gamma: f64,
    kappa: f64,
) -> (f64, f64) {
    let r = mid - position * gamma * sigma2 * t_remaining;
    let delta = (gamma * sigma2 * t_remaining) / 2.0
        + (1.0 / gamma) * (1.0 + gamma / kappa).ln();
    (r, delta)
}

fn inventory_risk_units(position: f64, mid: f64, risk_unit_usd: f64) -> f64 {
    if mid <= 0.0 || risk_unit_usd <= 0.0 {
        return position;
    }
    let unit_size = (risk_unit_usd / mid).max(1e-9);
    position / unit_size
}

// ─── Order grid ─────────────────────────────────────────────────────────────

/// Generate bid/ask order levels using pure Avellaneda-Stoikov pricing.
///
/// δ* is clamped to [min_spread_ticks, max_spread_ticks] from config —
/// the A-S formula sets the shape; the config bounds act as risk guard-rails.
/// Even when clamped, the reservation price r preserves the inventory skew.
pub fn calculate_levels(
    bbo: &Bbo,
    position: f64,
    config: &Config,
    state: &MmState,
) -> (Vec<OrderLevel>, Vec<OrderLevel>, i32) {
    let tick = config.token.tick_size;
    let mid = bbo.mid;

    let sigma2 = if state.ewma_variance > 0.0 {
        state.ewma_variance
    } else {
        compute_price_variance(&state.price_history, config.as_model.sigma_window)
    }
    .max(1e-18);

    let t_remaining = match state.vol_regime {
        VolRegime::High => state.t_remaining_secs.min(150.0),
        VolRegime::Extreme => state.t_remaining_secs.min(60.0),
        _ => state.t_remaining_secs,
    }
    .max(1.0);
    let q_normalized = inventory_risk_units(position, mid, config.as_model.risk_unit_usd);

    let (r_raw, delta_raw) = as_quotes(
        mid,
        q_normalized,
        sigma2,
        t_remaining,
        config.as_model.gamma,
        config.as_model.kappa,
    );

    let imbalance_skew = state.book_imbalance_ema * config.spread.imbalance_weight * tick;
    let r = r_raw + imbalance_skew;

    // Widen spread when recent fills have been adversely marked out
    // markout_score 0→no change, 0.5→1.5x wider, 1.0→3x wider
    let markout_mult = 1.0 + state.markout_score * 2.0;
    let regime_spread_floor = match state.vol_regime {
        VolRegime::Low | VolRegime::Normal => config.spread.min_spread_ticks,
        VolRegime::High => config.spread.min_spread_ticks + 2.0,
        VolRegime::Extreme => config.spread.min_spread_ticks + 4.0,
    };
    let delta = (delta_raw * markout_mult)
        .max(regime_spread_floor * tick)
        .min(config.spread.max_spread_ticks * tick);

    let spread_ticks = ((delta * 2.0) / tick).round() as i32;

    let recent_rt_pnl = if state.recent_rt_pnls.is_empty() {
        0.0
    } else {
        state.recent_rt_pnls.iter().copied().sum::<f64>() / state.recent_rt_pnls.len() as f64
    };
    let base_size = calc_size(mid, config, state.size_scale, state.volatility, spread_ticks, recent_rt_pnl, state.vol_regime);
    if base_size <= 0.0 {
        return (vec![], vec![], spread_ticks);
    }

    let num_levels = config.token.num_levels;
    let multipliers = &config.token.level_multipliers;
    let level_spacing = config.spread.level_tick_spacing as f64 * tick;
    let size_dec = config.size_decimals();
    let price_dec = config.price_decimals();

    let mut bids = Vec::with_capacity(num_levels);
    let mut asks = Vec::with_capacity(num_levels);
    let mut seen_bid: Vec<u64> = Vec::with_capacity(num_levels);
    let mut seen_ask: Vec<u64> = Vec::with_capacity(num_levels);

    for i in 0..num_levels {
        let offset = i as f64 * level_spacing;
        let mult = if i < multipliers.len() { multipliers[i] } else { 0.5 };
        let sz = round_to_decimals(base_size * mult, size_dec);
        if sz < config.token.min_size {
            continue;
        }
        // Hyperliquid rejects orders below $10 notional
        if sz * mid < 10.0 {
            continue;
        }

        // Bid and ask are symmetric around the reservation price r (not mid)
        let p_bid = round_to_decimals(
            (((r - delta - offset) / tick).floor()) * tick,
            price_dec,
        );
        let p_ask = round_to_decimals(
            (((r + delta + offset) / tick).ceil()) * tick,
            price_dec,
        );

        if p_bid > 0.0 && !seen_bid.contains(&p_bid.to_bits()) {
            bids.push(OrderLevel { price: p_bid, size: sz });
            seen_bid.push(p_bid.to_bits());
        }
        if p_ask > 0.0 && !seen_ask.contains(&p_ask.to_bits()) {
            asks.push(OrderLevel { price: p_ask, size: sz });
            seen_ask.push(p_ask.to_bits());
        }
    }

    (bids, asks, spread_ticks)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Order size in asset units, scaled for volatility.
pub fn calc_size(
    mid: f64,
    config: &Config,
    size_scale: f64,
    volatility: f64,
    spread_ticks: i32,
    recent_rt_pnl: f64,
    vol_regime: VolRegime,
) -> f64 {
    if mid <= 0.0 {
        return config.token.min_size;
    }
    let spread_scale = (spread_ticks as f64 / 4.0).clamp(0.6, 2.0);
    let pnl_scale = if recent_rt_pnl > 0.02 {
        1.2
    } else if recent_rt_pnl < -0.01 {
        0.7
    } else {
        1.0
    };
    let regime_scale = match vol_regime {
        VolRegime::Low | VolRegime::Normal => 1.0,
        VolRegime::High => 0.7,
        VolRegime::Extreme => 0.5,
    };
    let base = config.token.order_size_usd / mid * spread_scale * pnl_scale * regime_scale;
    // Reduce size at high volatility to limit adverse selection exposure
    let vol_scale = 1.0 / (1.0 + volatility * 10.0);
    let scaled = base * size_scale * vol_scale.max(0.4);
    round_to_decimals(scaled.max(config.token.min_size), config.size_decimals())
}

/// Rolling volatility σ (as % of mid, for display and size scaling).
pub fn update_volatility(prices: &VecDeque<f64>) -> f64 {
    if prices.len() < 5 {
        return 0.0;
    }
    let n = prices.len();
    let mut sum_r = 0.0;
    let mut sum_r2 = 0.0;
    let count = (n - 1) as f64;
    for i in 1..n {
        if prices[i - 1] > 0.0 {
            let r = (prices[i] - prices[i - 1]) / prices[i - 1];
            sum_r += r;
            sum_r2 += r * r;
        }
    }
    let mean = sum_r / count;
    let var = (sum_r2 / count) - mean * mean;
    var.max(0.0).sqrt() * 100.0
}

pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}
