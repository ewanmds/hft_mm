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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use api::{AppState, create_router};
use bot::BotManager;
use config::{available_tokens, default_config, Config};
use exchange::HyperLiquidExchange;
use trading::now_secs;
use types::{Bbo, EventLevel, MmState};
use websocket::{connect_ws, WsEvent, WsSharedState};

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
            trading::process_ws_event(&config_snapshot, state, evt);
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
        trading::run_iteration(&config_snapshot, exchange, state).await;
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
