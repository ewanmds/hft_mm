mod api;
mod bot;
mod config;
mod dashboard;
mod exchange;
mod risk;
mod strategy;
mod trading;
mod types;
mod websocket;

use anyhow::Result;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::error;

use api::{AppState, create_router};
use bot::BotManager;
use config::{available_tokens, default_config, Config};
use exchange::{HyperLiquidExchange, OrderRequest};
use risk::{check_risks, should_refresh, apply_position_limits, RefreshReason, RiskAction};
use strategy::{as_quotes, calculate_levels, compute_price_variance, update_volatility};
use trading::now_secs;
use types::{Bbo, EventLevel, MmState, PendingRt, Side, TrackedOrder};
use websocket::{connect_ws, WsEvent, WsSharedState};

fn fill_lock_secs(config: &Config) -> f64 {
    config.timing.rt_wait_sec.clamp(0.25, 1.0)
}

fn extract_order_details(result: &Value, requests: &[OrderRequest], now: f64) -> HashMap<u64, TrackedOrder> {
    let mut details = HashMap::new();
    if let Some(statuses) = result.pointer("/response/data/statuses").and_then(|v| v.as_array()) {
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

async fn wait_for_cancel_ack(
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
            let base = config.base_coin();
            let remaining: HashSet<u64> = open_orders
                .into_iter()
                .filter(|o| o.coin == config.token.symbol || o.coin == base)
                .filter_map(|o| tracked_oids.contains(&o.oid).then_some(o.oid))
                .collect();

            let mut s = state.write();
            s.last_api_sync_ts = now;
            if remaining.is_empty() {
                s.buy_oids.clear();
                s.sell_oids.clear();
                s.order_details.clear();
                s.cancel_pending_since = None;
                s.last_cancel_check_ts = now;
                return false;
            }

            s.buy_oids.retain(|oid| remaining.contains(oid));
            s.sell_oids.retain(|oid| remaining.contains(oid));
            s.order_details.retain(|oid, _| remaining.contains(oid));
            s.set_status_with_level(
                EventLevel::Warn,
                format!(
                "Cancel ack {:.2}s | {} live",
                now - pending_since,
                remaining.len()
                ),
            );
            true
        }
        Err(e) => {
            let mut s = state.write();
            let short: String = e.to_string().chars().take(72).collect();
            s.set_status_with_level(EventLevel::Error, format!("Cancel sync: {}", short));
            true
        }
    }
}

async fn handle_inactive_state(
    config: &Config,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
    now: f64,
    level: EventLevel,
    status: String,
) {
    if wait_for_cancel_ack(config, exchange, state, now).await {
        return;
    }

    let cancel_oids: Vec<(String, u64)> = {
        let s = state.read();
        s.buy_oids
            .iter()
            .chain(s.sell_oids.iter())
            .map(|&oid| (config.token.symbol.to_string(), oid))
            .collect()
    };

    if !cancel_oids.is_empty() {
        match exchange.bulk_cancel(&cancel_oids).await {
            Ok(_) => {
                let mut s = state.write();
                s.cancel_pending_since = Some(now);
                s.last_cancel_check_ts = 0.0;
                s.last_api_sync_ts = now;
                s.stats.cancel_batches += 1;
                s.stats.cancel_requests += cancel_oids.len() as u64;
                s.set_status_with_level(
                    level,
                    format!("{status} | pulling {} live orders", cancel_oids.len()),
                );
            }
            Err(e) => {
                let mut s = state.write();
                let short: String = e.to_string().chars().take(96).collect();
                s.set_status_with_level(EventLevel::Error, format!("Pause cancel error: {}", short));
            }
        }
        return;
    }

    let mut s = state.write();
    s.set_status_with_level(level, status);
}

/// Token selection menu
fn select_token() -> String {
    let tokens = available_tokens();
    let mut names: Vec<&&str> = tokens.keys().collect();
    names.sort();

    println!("\n{}", "=".repeat(60));
    println!("  HYPERLIQUID HFT MARKET MAKER [Rust]");
    println!("{}\n", "=".repeat(60));

    for (i, name) in names.iter().enumerate() {
        let cfg = &tokens[**name];
        println!(
            "  [{:>2}] {:<15} Lev:{}x  Size:${}",
            i + 1,
            cfg.symbol,
            cfg.target_leverage,
            cfg.order_size_usd,
        );
    }
    println!("\n  [Enter] Default: XYZ100");

    loop {
        let mut input = String::new();
        print!("\n  Choose (1-{}): ", names.len());
        use std::io::Write;
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_line(&mut input).unwrap();
        let input = input.trim();

        if input.is_empty() {
            return "XYZ100".to_string();
        }
        if input == "q" {
            std::process::exit(0);
        }
        if let Ok(idx) = input.parse::<usize>() {
            if idx >= 1 && idx <= names.len() {
                return names[idx - 1].to_string();
            }
        }
        println!("  Invalid input.");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file (if present) before anything reads env vars
    let _ = dotenvy::dotenv();

    // Init logging
    tracing_subscriber::fmt()
        .with_env_filter("hft_mm=info")
        .with_target(false)
        .init();

    // Check for headless / API server mode
    let headless = std::env::var("HEADLESS").unwrap_or_default() == "1"
        || std::env::args().any(|a| a == "--headless");

    if headless {
        let api_key = std::env::var("API_KEY")
            .unwrap_or_else(|_| "changeme".to_string());
        let bot = BotManager::new();
        let app_state = Arc::new(AppState { bot, api_key });
        let router = create_router(app_state);
        let addr = "0.0.0.0:3001";
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("API server listening on http://{}", addr);
        axum::serve(listener, router).await?;
        return Ok(());
    }

    // Token selection
    let token_name = select_token();
    let tokens = available_tokens();
    let token_config = tokens
        .get(token_name.as_str())
        .expect("Invalid token")
        .clone();

    println!(
        "\n  Token: {} | Lev: {}x | Size: ${}",
        token_config.symbol,
        token_config.target_leverage,
        token_config.order_size_usd,
    );

    let initial_config = default_config(token_config);

    if initial_config.agent_private_key.is_empty() || initial_config.account_address.is_empty() {
        eprintln!("\n  ERROR: Set HL_AGENT_KEY and HL_ACCOUNT environment variables!");
        eprintln!("  Example:");
        eprintln!("    export HL_AGENT_KEY=0x...");
        eprintln!("    export HL_ACCOUNT=0x...");
        std::process::exit(1);
    }

    // Init exchange client
    let exchange = Arc::new(HyperLiquidExchange::new(&initial_config).await?);
    let config = Arc::new(RwLock::new(initial_config));

    // Init state
    let state = Arc::new(RwLock::new(MmState::new()));

    // Init WebSocket shared state
    let ws_shared = Arc::new(RwLock::new(WsSharedState::new()));

    // Event channel (WebSocket -> main loop)
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WsEvent>();

    // Connect WebSocket
    let ws_config = config.read().clone();
    connect_ws(
        &ws_config.base_url,
        &ws_config.token.symbol,
        &ws_config.account_address,
        ws_shared.clone(),
        event_tx,
    )
    .await?;

    println!("  WebSocket connecting...\n");

    // Wait for first BBO
    let mut got_bbo = false;
    let wait_start = Instant::now();
    while !got_bbo && wait_start.elapsed() < Duration::from_secs(10) {
        if let Ok(evt) = event_rx.try_recv() {
            if let WsEvent::L2Update { best_bid, best_ask, mid } = evt {
                let mut s = state.write();
                s.bbo = Some(Bbo { best_bid, best_ask, mid });
                s.last_ws_msg_ts = now_secs();
                got_bbo = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    if !got_bbo {
        eprintln!("  ERROR: No market data received in 10s. Check symbol and connection.");
        std::process::exit(1);
    }

    // Start terminal dashboard
    let terminal = {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        )?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        ratatui::Terminal::new(backend)?
    };
    let terminal = Arc::new(std::sync::Mutex::new(terminal));

    let signal_state = state.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let mut s = signal_state.write();
            s.trading_enabled = false;
            s.running = false;
            s.push_event(EventLevel::Warn, "Runtime", "Shutdown requested via Ctrl+C");
            s.set_status_with_level(EventLevel::Warn, "Shutdown requested via Ctrl+C");
        }
    });

    // Main loop
    let result = run_loop(&config, &exchange, &state, &mut event_rx, &terminal).await;

    // Restore terminal
    {
        let mut term = terminal.lock().unwrap();
        crossterm::terminal::disable_raw_mode().ok();
        crossterm::execute!(
            term.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        ).ok();
        term.show_cursor().ok();
    }

    // Print summary
    {
        let s = state.read();
        let config_snapshot = config.read().clone();
        print_summary(&config_snapshot, &s, "Shutdown");
    }

    // Cancel all orders on exit
    let s = state.read();
    let config_snapshot = config.read().clone();
    let all_oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
        .map(|&oid| (config_snapshot.token.symbol.to_string(), oid))
        .collect();
    drop(s);

    if !all_oids.is_empty() {
        let _ = exchange.bulk_cancel(&all_oids).await;
    }

    // Also cancel via open orders API
    if let Ok(open) = exchange.open_orders().await {
        let base = config_snapshot.base_coin();
        let cancels: Vec<(String, u64)> = open
            .iter()
            .filter(|o| o.coin == config_snapshot.token.symbol || o.coin == base)
            .map(|o| (o.coin.clone(), o.oid))
            .collect();
        if !cancels.is_empty() {
            let _ = exchange.bulk_cancel(&cancels).await;
        }
    }

    if let Err(e) = result {
        eprintln!("Error: {}", e);
    }

    Ok(())
}

type Terminal = Arc<std::sync::Mutex<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>>;

async fn run_loop(
    config: &Arc<RwLock<Config>>,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
    event_rx: &mut mpsc::UnboundedReceiver<WsEvent>,
    terminal: &Terminal,
) -> Result<()> {
    let mut last_draw = Instant::now();

    loop {
        let config_snapshot = config.read().clone();

        // 1. Drain pending WebSocket events (non-blocking batch)
        let mut events_processed = 0;
        while let Ok(evt) = event_rx.try_recv() {
            process_ws_event(&config_snapshot, state, evt);
            events_processed += 1;
            if events_processed > 50 { break; }
        }

        // 2. Check running + quit key
        {
            if dashboard::check_quit() {
                let mut s = state.write();
                s.running = false;
                s.trading_enabled = false;
            }
            let s = state.read();
            if !s.running { break; }
        }

        // 3. Trading iteration
        run_iteration(&config_snapshot, exchange, state).await;
        state.write().record_metrics_sample();

        // 4. Draw terminal dashboard (~10 FPS)
        if last_draw.elapsed() >= Duration::from_millis(100) {
            last_draw = Instant::now();
            let s = state.read();
            let mut term = terminal.lock().unwrap();
            term.draw(|f| dashboard::draw(f, &config_snapshot, &s))?;
        }

        // 5. Yield — hot loop timing
        tokio::time::sleep(Duration::from_micros(config_snapshot.timing.refresh_fast_us)).await;
    }

    Ok(())
}

/// Process a single WebSocket event
fn process_ws_event(config: &Config, state: &Arc<RwLock<MmState>>, evt: WsEvent) {
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
            let sigma2 = compute_price_variance(&s.price_history, config.as_model.sigma_window).max(1e-18);
            let (r, d) = as_quotes(mid, s.position, sigma2, s.t_remaining_secs, config.as_model.gamma, config.as_model.kappa);
            s.reservation_price = r;
            s.as_delta = d.min(config.spread.max_spread_ticks * config.token.tick_size);
        }

        WsEvent::Fill { tid, price, size, fee, closed_pnl, oid, is_buy, .. } => {
            if s.processed_fill_ids.contains(&tid) { return; }
            s.processed_fill_ids.insert(tid);

            s.stats.total_volume += price * size;
            s.stats.fills_count += 1;
            s.stats.total_fees += fee.abs();
            s.stats.session_pnl += closed_pnl;

            let fill_side = if is_buy { Side::Buy } else { Side::Sell };

            // Round-trip matching
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
                s.pending_rts.push(PendingRt {
                    side: fill_side, price, size, time: now,
                });
                s.pending_rts.retain(|p| (now - p.time) < 300.0);
            }

            s.last_fill_time_rt = now;
            s.waiting_for_rt = !rt_matched;
            s.fill_lock_side = Some(fill_side);
            s.fill_lock_until = now + fill_lock_secs(config);

            // Toxic flow tracking: consecutive same-side fills
            match fill_side {
                Side::Buy => {
                    s.consecutive_buy_fills += 1;
                    s.consecutive_sell_fills = 0;
                }
                Side::Sell => {
                    s.consecutive_sell_fills += 1;
                    s.consecutive_buy_fills = 0;
                }
            }

            // xyz perps charge positive maker fee (no rebate), so detect via order tracking
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
async fn run_iteration(
    config: &Config,
    exchange: &Arc<HyperLiquidExchange>,
    state: &Arc<RwLock<MmState>>,
) {
    let now = now_secs();

    if wait_for_cancel_ack(config, exchange, state, now).await {
        return;
    }

    // Read snapshot
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

    // Handle emergency
    match &risk_action {
        RiskAction::EmergencyStop(reason) => {
            error!("EMERGENCY STOP: {}", reason);
            let mut s = state.write();
            s.running = false;
            s.last_status = format!("EMERGENCY: {}", reason);
            let all_oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                .map(|&oid| (config.token.symbol.to_string(), oid))
                .collect();
            s.buy_oids.clear();
            s.sell_oids.clear();
            s.order_details.clear();
            s.cancel_pending_since = None;
            drop(s);
            if !all_oids.is_empty() { let _ = exchange.bulk_cancel(&all_oids).await; }
            return;
        }
        RiskAction::FeedStale => {
            let mut s = state.write();
            let all_oids: Vec<(String, u64)> = s.buy_oids.iter().chain(s.sell_oids.iter())
                .map(|&oid| (config.token.symbol.to_string(), oid))
                .collect();
            s.buy_oids.clear();
            s.sell_oids.clear();
            s.order_details.clear();
            s.cancel_pending_since = None;
            s.last_status = format!("Feed stale > {:.0}s - pulled", config.timing.feed_stale_sec);
            drop(s);
            if !all_oids.is_empty() { let _ = exchange.bulk_cancel(&all_oids).await; }
            return;
        }
        _ => {}
    }

    // Handle refresh
    let reason_str = match &refresh {
        RefreshReason::NoRefresh => {
            let mut s = state.write();
            let drift = s.anchor_price.map(|a| ((bbo.mid - a).abs() / config.token.tick_size) as u32).unwrap_or(0);
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

    // Calculate levels
    let (mut bid_levels, mut ask_levels, spread_ticks) = {
        let s = state.read();
        calculate_levels(&bbo, position, config, &s)
    };

    {
        let mut s = state.write();
        s.current_spread_ticks = spread_ticks;
    }

    // Apply limits
    {
        let s = state.read();
        apply_position_limits(config, &s, &mut bid_levels, &mut ask_levels, &refresh, now);
    }

    // Cancel existing first and wait for ack on a later iteration before reposting.
    let cancel_oids: Vec<(String, u64)> = {
        let s = state.read();
        s.buy_oids.iter().chain(s.sell_oids.iter())
            .map(|&oid| (config.token.symbol.to_string(), oid))
            .collect()
    };
    if !cancel_oids.is_empty() {
        match exchange.bulk_cancel(&cancel_oids).await {
            Ok(_) => {
                let mut s = state.write();
                s.cancel_pending_since = Some(now);
                s.last_cancel_check_ts = 0.0;
                s.last_status = format!("Cancel {} -> {} live", reason_str, cancel_oids.len());
            }
            Err(e) => {
                let mut s = state.write();
                let short: String = e.to_string().chars().take(96).collect();
                s.last_status = format!("Cancel error: {}", short);
            }
        }
        return;
    }

    // Build and place orders
    let mut requests: Vec<OrderRequest> = Vec::with_capacity(bid_levels.len() + ask_levels.len());
    for lvl in &bid_levels {
        requests.push(OrderRequest {
            coin: config.token.symbol.to_string(),
            is_buy: true,
            sz: lvl.size,
            limit_px: lvl.price,
            reduce_only: false,
            tif: "Alo".to_string(),
        });
    }
    for lvl in &ask_levels {
        requests.push(OrderRequest {
            coin: config.token.symbol.to_string(),
            is_buy: false,
            sz: lvl.size,
            limit_px: lvl.price,
            reduce_only: false,
            tif: "Alo".to_string(),
        });
    }

    if !requests.is_empty() {
        match exchange.bulk_orders(&requests).await {
            Ok(result) => {
                let (new_buys, new_sells, margin_errs, first_error) =
                    exchange.parse_bulk_result(&result, &requests);
                let details = extract_order_details(&result, &requests, now);
                let mut s = state.write();
                s.buy_oids = new_buys;
                s.sell_oids = new_sells;
                s.order_details = details;
                s.cancel_pending_since = None;

                let nb = s.buy_oids.len();
                let na = s.sell_oids.len();

                if margin_errs > 0 {
                    s.size_scale = (s.size_scale * config.margin.reject_decay).max(config.margin.min_size_scale);
                    s.margin_pause_until = now + config.margin.reject_cooldown;
                    s.last_status = format!("Margin reject x{} | scale={:.2}", margin_errs, s.size_scale);
                } else if let Some(ref err) = first_error {
                    let err_short: String = err.chars().take(72).collect();
                    if nb == 0 && na == 0 {
                        s.last_status = format!("Order reject: {}", err_short);
                    } else {
                        s.last_status =
                            format!("{} -> {}B/{}A | partial: {}", reason_str, nb, na, err_short);
                    }
                } else if nb > 0 || na > 0 {
                    s.size_scale = (s.size_scale + config.margin.recovery_step).min(1.0);
                }

                s.last_quote_ts = now;
                s.anchor_price = Some(bbo.mid);
                s.anchor_spread_ticks = Some(((bbo.best_ask - bbo.best_bid) / config.token.tick_size) as i32);
                s.last_sync_time = now;

                if margin_errs == 0 && first_error.is_none() {
                    s.last_status = format!("{} -> {}B/{}A @ ${:.2} S:{}t", reason_str, nb, na, bbo.mid, s.current_spread_ticks);
                }
            }
            Err(e) => {
                let mut s = state.write();
                s.buy_oids.clear();
                s.sell_oids.clear();
                s.order_details.clear();
                let msg = e.to_string();
                let short: String = msg.chars().take(96).collect();
                s.last_status = format!("Order error: {}", short);
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

fn print_summary(config: &Config, state: &MmState, reason: &str) {
    let elapsed = state.start_time.elapsed().as_secs_f64();
    let hrs = elapsed / 3600.0;
    let cpm = state.stats.cost_per_million();
    let maker = state.stats.maker_ratio() * 100.0;
    let rt_ratio = state.stats.rt_ratio();
    let rt_avg = if state.stats.rt_count > 0 {
        state.stats.rt_profit / state.stats.rt_count as f64
    } else { 0.0 };

    println!("\n{}", "=".repeat(60));
    println!("SESSION SUMMARY | {} | {}", config.token.symbol, reason);
    println!("{}", "=".repeat(60));
    println!("Runtime     : {:.2}h", hrs);
    println!("PnL         : ${:.4} (${:.4}/h)", state.stats.session_pnl, state.stats.session_pnl / hrs.max(0.01));
    println!("Round-trips : {} ({:.0}%) | profit ${:.4} (avg ${:.4})", state.stats.rt_count, rt_ratio * 100.0, state.stats.rt_profit, rt_avg);
    println!("Volume      : ${:.2} | Fees ${:.4}", state.stats.total_volume, state.stats.total_fees);
    println!("Cost/1M     : ${:.2}", cpm);
    println!("Fills       : {} | Maker {:.0}%", state.stats.fills_count, maker);
    println!("{}\n", "=".repeat(60));
}
