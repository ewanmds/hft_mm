use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table},
    Frame,
};
use std::time::Instant;

use crate::config::Config;
use crate::types::MmState;

pub fn draw(frame: &mut Frame, config: &Config, state: &MmState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Title
            Constraint::Length(6),  // Stats table
            Constraint::Min(12),   // Info panel
        ])
        .split(frame.size());

    draw_title(frame, chunks[0], config);
    draw_stats(frame, chunks[1], config, state);
    draw_info(frame, chunks[2], config, state);
}

fn draw_title(frame: &mut Frame, area: Rect, config: &Config) {
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            format!(" HFT MM v1.0 | {} ", config.token.symbol),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " [Rust] ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Ctrl+C to stop ",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));

    frame.render_widget(title, area);
}

fn draw_stats(frame: &mut Frame, area: Rect, config: &Config, state: &MmState) {
    let cpm = state.stats.cost_per_million();
    let cpm_color = if cpm < 100.0 {
        Color::Green
    } else if cpm < 200.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let pnl_color = if state.stats.session_pnl >= 0.0 {
        Color::Green
    } else {
        Color::Red
    };

    let header = Row::new(vec!["PAIR", "COST/1M", "VOLUME", "FEES", "PNL"])
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

    let row = Row::new(vec![
        Span::styled(config.token.symbol.to_string(), Style::default().fg(Color::White)),
        Span::styled(format!("${:.1}", cpm), Style::default().fg(cpm_color)),
        Span::styled(
            format!("${:.2}", state.stats.total_volume),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("${:.4}", state.stats.total_fees),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("${:+.4}", state.stats.session_pnl),
            Style::default().fg(pnl_color),
        ),
    ]);

    let widths = [
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Length(14),
    ];

    let table = Table::new(vec![row], widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Stats "));

    frame.render_widget(table, area);
}

fn draw_info(frame: &mut Frame, area: Rect, config: &Config, state: &MmState) {
    let elapsed = state.start_time.elapsed().as_secs();
    let h = elapsed / 3600;
    let m = (elapsed % 3600) / 60;
    let s = elapsed % 60;
    let runtime = if h > 0 {
        format!("{}h{}m{}s", h, m, s)
    } else {
        format!("{}m{}s", m, s)
    };

    let vph = if elapsed > 0 {
        state.stats.total_volume / (elapsed as f64 / 3600.0)
    } else {
        0.0
    };

    let fpm = if elapsed > 0 {
        state.stats.fills_count as f64 / (elapsed as f64 / 60.0)
    } else {
        0.0
    };

    let maker_pct = state.stats.maker_ratio() * 100.0;
    let rt_pct = state.stats.rt_ratio() * 100.0;
    let rt_avg = if state.stats.rt_count > 0 {
        state.stats.rt_profit / state.stats.rt_count as f64
    } else {
        0.0
    };

    let pos_color = if state.position.abs() < 0.001 {
        Color::DarkGray
    } else if state.position > 0.0 {
        Color::Green
    } else {
        Color::Red
    };

    let mid_str = state.bbo.map(|b| format!("${:.4}", b.mid)).unwrap_or_else(|| "---".to_string());
    let bias_color = if state.signals.bias > 0.1 {
        Color::Green
    } else if state.signals.bias < -0.1 {
        Color::Red
    } else {
        Color::Yellow
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("  Mid Price  ", Style::default().fg(Color::DarkGray)),
            Span::styled(&mid_str, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("  Vol/Hour   ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.2}", vph), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("  Runtime    ", Style::default().fg(Color::DarkGray)),
            Span::styled(&runtime, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Pair       ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} | Lev {}x", config.token.symbol, config.token.target_leverage),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Position   ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:+.6}", state.position),
                Style::default().fg(pos_color),
            ),
            Span::styled(
                format!("  uPnL ${:+.4}", state.unrealized_pnl),
                Style::default().fg(if state.unrealized_pnl >= 0.0 { Color::Green } else { Color::Red }),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Orders     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}B", state.buy_oids.len()),
                Style::default().fg(Color::Green),
            ),
            Span::styled(" / ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}A", state.sell_oids.len()),
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                format!("  | Spread {}t", state.current_spread_ticks),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Fills      ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ({:.1}/min) | Maker {:.0}%", state.stats.fills_count, fpm, maker_pct),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  RT         ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} ({:.0}%) | profit ${:.4} (avg ${:.4})",
                    state.stats.rt_count, rt_pct, state.stats.rt_profit, rt_avg
                ),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Scale      ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:.0}%", state.size_scale * 100.0),
                Style::default().fg(if state.size_scale >= 0.9 { Color::Green } else { Color::Yellow }),
            ),
            Span::styled(
                format!("  | Vol {:.4}%", state.volatility),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Signals    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "Bias {:+.2} | EMA {:+.1}bps | MACD {:+.1}bps | RSI {:.0} | BB z {:+.2} | VWAP {:+.1}bps",
                    state.signals.bias,
                    state.signals.ema_gap_bps,
                    state.signals.macd_hist_bps,
                    state.signals.rsi,
                    state.signals.bollinger_z,
                    state.signals.quote_vwap_dev_bps,
                ),
                Style::default().fg(bias_color),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Status     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                &state.last_status,
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];

    let info = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Info "));

    frame.render_widget(info, area);
}

/// Check for Ctrl+C or 'q' keypress (non-blocking)
pub fn check_quit() -> bool {
    if event::poll(std::time::Duration::from_millis(0)).unwrap_or(false) {
        if let Ok(Event::Key(key)) = event::read() {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return true;
            }
            if key.code == KeyCode::Char('q') {
                return true;
            }
        }
    }
    false
}
