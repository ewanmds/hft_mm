use serde::Serialize;
use std::collections::HashMap;

/// Serializable token info for the web API
#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub name: String,
    pub symbol: String,
    pub default_leverage: f64,
    pub default_order_size_usd: f64,
}

/// Token-specific configuration
#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub symbol: String,
    pub tick_size: f64,
    pub min_size: f64,
    pub order_size_usd: f64,
    pub target_leverage: f64,
    pub num_levels: usize,
    pub level_multipliers: Vec<f64>,
}

/// Risk parameters
#[derive(Debug, Clone)]
pub struct RiskConfig {
    pub stop_loss_usd: f64,
    pub session_pnl_kill: f64,
    pub min_fills_kill: u64,
    pub cost_m_kill: f64,
    pub cost_m_min_vol: f64,
    /// Close position via market order if unrealized loss exceeds this fraction of notional (e.g. 0.04 = 4%)
    pub hedge_loss_pct: f64,
    /// Hard position cap: stop posting inventory-building orders if |pos * mid| > this (USD notional)
    pub position_cap_usd: f64,
    /// Seconds after a fill to sample markout (how far price moved against fill)
    pub markout_sample_sec: f64,
    /// Adverse markout threshold in ticks: fills where price moved > this against us count as adverse
    pub markout_threshold_ticks: f64,
    /// Exponential decay half-life for markout score (seconds)
    pub markout_halflife_sec: f64,
}

/// Spread & grid parameters
#[derive(Debug, Clone)]
pub struct SpreadConfig {
    pub min_spread_ticks: f64,
    pub base_spread_ticks: f64,
    pub max_spread_ticks: f64,
    pub skew_factor: f64,
    pub level_tick_spacing: u32,
}

/// Timing parameters (in seconds)
#[derive(Debug, Clone)]
pub struct TimingConfig {
    pub refresh_fast_us: u64,       // microseconds
    pub refresh_normal_us: u64,
    pub refresh_slow_us: u64,
    pub api_sync_sec: f64,
    pub drift_ticks: u32,
    pub urgent_drift_ticks: u32,
    pub periodic_sync_sec: f64,
    pub rt_wait_sec: f64,
    pub max_quote_ttl: f64,
    pub feed_stale_sec: f64,
    pub vol_window: usize,
}

/// Margin management
#[derive(Debug, Clone)]
pub struct MarginConfig {
    pub reject_cooldown: f64,
    pub reject_decay: f64,
    pub recovery_step: f64,
    pub min_size_scale: f64,
    pub close_cooldown: f64,
}

/// RT adaptive spread
#[derive(Debug, Clone)]
pub struct RtAdaptConfig {
    pub min_fills_for_adapt: u64,
    pub widen_threshold_ratio: f64,
    pub widen_threshold_profit: f64,
}

/// Directional signal overlay for quote skewing
#[derive(Debug, Clone)]
pub struct SignalConfig {
    pub enabled: bool,
    pub min_history: usize,
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub bollinger_window: usize,
    pub bollinger_stddev: f64,
    pub macd_fast: usize,
    pub macd_slow: usize,
    pub macd_signal: usize,
    pub rsi_period: usize,
    pub vwap_window: usize,
    pub max_fair_skew_ticks: f64,
    pub side_size_skew: f64,
    pub side_size_floor: f64,
}

/// Momentum filter — pauses quoting during strong directional moves
#[derive(Debug, Clone)]
pub struct MomentumConfig {
    pub enabled: bool,
    /// Number of recent price samples to look at
    pub lookback: usize,
    /// Directional move threshold in ticks to trigger a pause
    pub min_move_ticks: f64,
    /// How long to stay out of the market after a trend is detected (seconds)
    pub pause_sec: f64,
}

/// Pure Avellaneda-Stoikov model parameters
#[derive(Debug, Clone)]
pub struct AsModelConfig {
    /// Risk aversion coefficient γ — scales both inventory skew and spread width.
    /// Unit: 1/USD² (calibrate so γ·σ²·T gives spreads in the right tick range).
    pub gamma: f64,
    /// Market order arrival intensity κ (orders/second estimate).
    /// Higher κ → tighter optimal spread.
    pub kappa: f64,
    /// Session horizon T in seconds. T-t counts down; resets every t_secs.
    /// Larger T → wider spreads; as T-t→0 spreads narrow and inventory is flattened.
    pub t_secs: f64,
    /// Rolling window for σ² (price-change variance) estimate.
    pub sigma_window: usize,
    /// Session stop-loss: kill session if PnL drops below -max_loss_usd.
    pub max_loss_usd: f64,
    /// Flatten inventory via market order at session reset if |notional| > this.
    pub flatten_threshold_usd: f64,
}

/// Full bot configuration
#[derive(Debug, Clone)]
pub struct Config {
    pub agent_private_key: String,
    pub account_address: String,
    pub base_url: String,
    pub token: TokenConfig,
    pub risk: RiskConfig,
    pub spread: SpreadConfig,
    pub timing: TimingConfig,
    pub margin: MarginConfig,
    pub rt_adapt: RtAdaptConfig,
    pub signals: SignalConfig,
    pub as_model: AsModelConfig,
    pub momentum: MomentumConfig,
}

impl Config {
    /// Derived: number of price decimals from tick_size
    pub fn price_decimals(&self) -> u32 {
        if self.token.tick_size >= 1.0 {
            0
        } else {
            (-self.token.tick_size.log10().floor()) as u32
        }
    }

    /// Derived: number of size decimals from min_size
    pub fn size_decimals(&self) -> u32 {
        if self.token.min_size >= 1.0 {
            0
        } else {
            (-self.token.min_size.log10().floor()) as u32
        }
    }

    /// Derived: perp dex identifier
    pub fn perp_dex(&self) -> &str {
        if self.token.symbol.starts_with("xyz:") {
            "xyz"
        } else {
            "km"
        }
    }

    /// Derived: base coin name (after colon)
    pub fn base_coin(&self) -> &str {
        self.token.symbol.split(':').last().unwrap_or(&self.token.symbol)
    }
}

pub fn available_tokens() -> HashMap<&'static str, TokenConfig> {
    let mut m = HashMap::new();

    m.insert("US500", TokenConfig {
        symbol: "km:US500".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 80.0, target_leverage: 25.0,
        num_levels: 4, level_multipliers: vec![1.5, 1.0, 0.7, 0.3],
    });
    m.insert("USTECH", TokenConfig {
        symbol: "km:USTECH".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 80.0, target_leverage: 20.0,
        num_levels: 4, level_multipliers: vec![1.5, 1.0, 0.7, 0.3],
    });
    m.insert("USOIL", TokenConfig {
        symbol: "km:USOIL".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 60.0, target_leverage: 20.0,
        num_levels: 3, level_multipliers: vec![1.5, 1.0, 0.5],
    });
    m.insert("GOLD", TokenConfig {
        symbol: "km:GOLD".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 60.0, target_leverage: 22.0,
        num_levels: 3, level_multipliers: vec![1.5, 1.0, 0.5],
    });
    m.insert("BTC", TokenConfig {
        symbol: "km:BTC".to_string(), tick_size: 0.01, min_size: 0.001,
        order_size_usd: 80.0, target_leverage: 25.0,
        num_levels: 4, level_multipliers: vec![1.5, 1.0, 0.7, 0.3],
    });
    m.insert("SILVER", TokenConfig {
        symbol: "xyz:SILVER".to_string(), tick_size: 0.001, min_size: 0.01,
        order_size_usd: 70.0, target_leverage: 20.0,
        num_levels: 2, level_multipliers: vec![1.0, 0.5],
    });
    m.insert("GOLD_XYZ", TokenConfig {
        symbol: "xyz:GOLD".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 70.0, target_leverage: 20.0,
        num_levels: 2, level_multipliers: vec![1.0, 0.5],
    });
    m.insert("XYZ100", TokenConfig {
        symbol: "xyz:XYZ100".to_string(), tick_size: 1.0, min_size: 0.0001,
        order_size_usd: 300.0, target_leverage: 30.0,
        num_levels: 2, level_multipliers: vec![1.0, 0.25],
    });
    m.insert("CL", TokenConfig {
        symbol: "xyz:CL".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 50.0, target_leverage: 20.0,
        num_levels: 2, level_multipliers: vec![1.0, 0.25],
    });
    m.insert("COPPER", TokenConfig {
        symbol: "xyz:COPPER".to_string(), tick_size: 0.0001, min_size: 0.1,
        order_size_usd: 60.0, target_leverage: 20.0,
        num_levels: 3, level_multipliers: vec![1.5, 1.0, 0.5],
    });
    m.insert("PLTR", TokenConfig {
        symbol: "xyz:PLTR".to_string(), tick_size: 0.01, min_size: 0.1,
        order_size_usd: 60.0, target_leverage: 10.0,
        num_levels: 3, level_multipliers: vec![1.5, 1.0, 0.5],
    });
    m.insert("TSLA", TokenConfig {
        symbol: "xyz:TSLA".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 80.0, target_leverage: 10.0,
        num_levels: 4, level_multipliers: vec![1.5, 1.0, 0.7, 0.3],
    });
    m.insert("BRENTOIL", TokenConfig {
        symbol: "xyz:BRENTOIL".to_string(), tick_size: 0.01, min_size: 0.01,
        order_size_usd: 60.0, target_leverage: 20.0,
        num_levels: 3, level_multipliers: vec![1.5, 1.0, 0.5],
    });
    m.insert("SP500", TokenConfig {
        symbol: "xyz:SP500".to_string(), tick_size: 0.1, min_size: 0.01,
        order_size_usd: 50.0, target_leverage: 50.0,
        num_levels: 2, level_multipliers: vec![1.5, 1.0],
    });

    m
}

/// Returns token list for the web API
pub fn token_info_list() -> Vec<TokenInfo> {
    let mut tokens = available_tokens();
    let order = ["US500","USTECH","USOIL","GOLD","BTC","SILVER","GOLD_XYZ","XYZ100","CL","COPPER","PLTR","TSLA","BRENTOIL","SP500"];
    order.iter().filter_map(|name| {
        tokens.remove(name).map(|t| TokenInfo {
            name: name.to_string(),
            symbol: t.symbol.clone(),
            default_leverage: t.target_leverage,
            default_order_size_usd: t.order_size_usd,
        })
    }).collect()
}

pub fn default_config(token: TokenConfig) -> Config {
    let account_address = std::env::var("HL_ACCOUNT")
        .unwrap_or_else(|_| String::new())
        .trim()
        .to_lowercase();

    Config {
        agent_private_key: std::env::var("HL_AGENT_KEY")
            .unwrap_or_else(|_| String::new()),
        account_address,
        base_url: "https://api.hyperliquid.xyz".to_string(),
        token,
        risk: RiskConfig {
            stop_loss_usd: 0.50,         // very tight — cut losers instantly
            session_pnl_kill: -3.00,     // kill session at -$0.30
            min_fills_kill: 30,           // detect bad session very early
            cost_m_kill: 35.0,           // kill at $15/1M — no tolerance for bleeding
            cost_m_min_vol: 60000.0,       // start monitoring immediately
            hedge_loss_pct: 0.05,          // close position if unrealized loss > 5% of notional — balanced vs taker cost ($86/M)
            position_cap_usd: 300.0,       // hard cap: stop building if |pos * mid| > $300 (1x order_size_usd) — sessions show RT=0 losses from one-sided buildup
            markout_sample_sec: 2.0,       // sample markout 2s after fill
            markout_threshold_ticks: 3.0,  // adverse if price moved > 3 ticks against fill
            markout_halflife_sec: 180.0,   // score decays with 3-min half-life
        },
        spread: SpreadConfig {
            min_spread_ticks: 3.0,       // floor at 3t: breakeven vs fees is ~1t, 3t gives margin
            base_spread_ticks: 3.0,
            max_spread_ticks: 14.0,      // wide ceiling for vol spikes
            skew_factor: 8.0,            // inventory skew: long 0.01u → r shifts -0.08t, helps close positions faster (improves RT ratio)
            level_tick_spacing: 2,       // 2-tick gap between levels — less correlated fills
        },
        timing: TimingConfig {
            refresh_fast_us: 60,         // slower loop — less crossing risk
            refresh_normal_us: 120,
            refresh_slow_us: 250,
            api_sync_sec: 3.0,
            drift_ticks: 8,              // let orders rest 8 ticks before requoting
            urgent_drift_ticks: 15,      // only urgent at 15 ticks — very patient
            periodic_sync_sec: 30.0,     // very patient periodic refresh
            rt_wait_sec: 3.5,            // give 3.5s for RT completion
            max_quote_ttl: 30.0,         // let orders sit 30s — maximize queue priority
            feed_stale_sec: 8.0,
            vol_window: 64,
        },
        margin: MarginConfig {
            reject_cooldown: 6.0,
            reject_decay: 0.85,
            recovery_step: 0.015,        // very slow size recovery
            min_size_scale: 0.20,
            close_cooldown: 3.0,
        },
        rt_adapt: RtAdaptConfig {
            min_fills_for_adapt: 3,      // adapt after just 3 RTs
            widen_threshold_ratio: 0.65,  // require 65% RT completion
            widen_threshold_profit: 0.02, // require $0.02 avg RT profit
        },
        signals: SignalConfig {
            enabled: false,
            min_history: 20,
            ema_fast: 5,
            ema_slow: 15,
            bollinger_window: 14,
            bollinger_stddev: 1.2,
            macd_fast: 6,
            macd_slow: 14,
            macd_signal: 5,
            rsi_period: 8,
            vwap_window: 16,
            max_fair_skew_ticks: 0.0,
            side_size_skew: 0.0,
            side_size_floor: 1.0,
        },
        momentum: MomentumConfig {
            enabled: false,        // disabled: pausing during moves loses queue; A-S σ² widens spread naturally
            lookback: 20,
            min_move_ticks: 8.0,
            pause_sec: 30.0,
        },
        as_model: AsModelConfig {
            // XYZ100 calibration (tick=$1, price ~24000):
            // Target half-spread ≈ 3-4 ticks.
            // δ_half = (γ·σ²·T)/2 + (1/γ)·ln(1+γ/κ)
            // With γ=0.001, κ=1.5, T=600, σ²≈9 (3-tick std):
            //   term1 = 0.001·9·600/2 = 2.7t
            //   term2 = 1000·ln(1.000667) ≈ 0.67t  →  total ≈ 3.4t  ✓
            // Old γ=0.04/κ=0.07 gave term2 ≈ 11t → clamped to max=14t → zero fills.
            gamma: 0.001, // XYZ100: small γ keeps term2 < 1t
            kappa: 1.5,   // realistic fill rate for a liquid index (~1-2/s)
            t_secs: 600.0,       // 10-minute session; shorter T keeps term1 bounded
            sigma_window: 64,
            max_loss_usd: 5.0,
            flatten_threshold_usd: 20.0, // flatten if |notional| > $20 at session reset
        },
    }
}
