use std::{io, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction as LayoutDirection, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::{app::TradingApp, errors::AppError, pattern::Direction, trading::TradingState};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, AppError> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub async fn run(app: &mut TradingApp) -> Result<(), AppError> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let tick_duration = Duration::from_secs(app.config.check_interval_secs);
    let mut next_tick = tokio::time::Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char(' ') | KeyCode::Char('p') => app.toggle_pause(),
                        KeyCode::Char('k') => {
                            app.toggle_kill_switch()?;
                        }
                        KeyCode::Char('c') => {
                            app.manual_close().await?;
                        }
                        KeyCode::Char('s') => {
                            app.snapshot()?;
                        }
                        _ => {}
                    }
                }
            }
        }
        if tokio::time::Instant::now() >= next_tick {
            let running = app.step().await?;
            next_tick = tokio::time::Instant::now() + tick_duration;
            if !running && app.engine.position.is_none() {
                terminal.draw(|frame| draw(frame, app))?;
            }
        }
    }
    terminal.show_cursor()?;
    Ok(())
}

fn draw(frame: &mut Frame, app: &TradingApp) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    let mode_color = match app.config.mode {
        crate::config::Mode::Replay => Color::Cyan,
        crate::config::Mode::Paper => Color::Yellow,
        crate::config::Mode::Live => Color::Red,
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " OPTIONS / IOL ",
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {:?} · {} · tick {}  ",
            app.config.mode, app.config.ticker, app.ticks
        )),
        Span::styled(
            if app.paused { "PAUSADO" } else { &app.status },
            Style::default().fg(if app.paused {
                Color::Yellow
            } else {
                Color::White
            }),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, rows[0]);

    let upper = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    frame.render_widget(market_panel(app), upper[0]);
    frame.render_widget(trend_panel(app), upper[1]);

    let lower = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[2]);
    frame.render_widget(position_panel(app), lower[0]);
    frame.render_widget(risk_panel(app), lower[1]);

    let visible_logs = rows[3].height.saturating_sub(2) as usize;
    let skip_logs = app.logs().len().saturating_sub(visible_logs);
    let log_lines: Vec<Line> = app
        .logs()
        .iter()
        .skip(skip_logs)
        .map(|message| Line::from(format!("• {message}")))
        .collect();
    frame.render_widget(
        Paragraph::new(log_lines)
            .block(Block::default().title(" Eventos ").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        rows[3],
    );

    frame.render_widget(
        Paragraph::new(
            " q salir · espacio/p pausar · k kill switch · c cerrar posición · s snapshot ",
        )
        .style(Style::default().fg(Color::DarkGray)),
        rows[4],
    );
}

fn market_panel(app: &TradingApp) -> Paragraph<'static> {
    let lines = if let Some(frame) = &app.current_frame {
        let selected = app
            .selected_option
            .as_deref()
            .and_then(|symbol| frame.option(symbol));
        vec![
            Line::from(format!("Subyacente  {:>12.2}", frame.underlying.last)),
            Line::from(format!(
                "Bid / Ask   {:>7} / {:<7}",
                price(frame.underlying.bid),
                price(frame.underlying.ask)
            )),
            Line::from(format!("Opciones    {:>12}", frame.options.len())),
            Line::from(format!(
                "Seleccionada {}",
                selected.map_or("—", |option| option.symbol.as_str())
            )),
            Line::from(format!(
                "Prima B/A   {} / {}",
                selected.map_or_else(|| "—".into(), |option| price(option.bid)),
                selected.map_or_else(|| "—".into(), |option| price(option.ask))
            )),
        ]
    } else {
        vec![Line::from("Esperando datos de mercado…")]
    };
    Paragraph::new(lines).block(Block::default().title(" Mercado ").borders(Borders::ALL))
}

fn trend_panel(app: &TradingApp) -> Gauge<'static> {
    let (label, ratio, color) = app.current_trend.as_ref().map_or_else(
        || ("sin muestras".into(), 0.0, Color::DarkGray),
        |trend| {
            let direction = match trend.direction {
                Direction::Up => "SUBA",
                Direction::Down => "BAJA",
                Direction::Neutral => "NEUTRA",
            };
            let strength = trend.r_squared.unwrap_or(0.0).clamp(0.0, 1.0);
            (
                format!(
                    "{} · {} · SMA {:.2} · σ {:.3} · R² {:.2}",
                    direction,
                    if trend.confirmed {
                        "CONFIRMADA"
                    } else {
                        "parcial"
                    },
                    trend.sma,
                    trend.volatility,
                    strength
                ),
                strength,
                match trend.direction {
                    Direction::Up => Color::Green,
                    Direction::Down => Color::Red,
                    Direction::Neutral => Color::Gray,
                },
            )
        },
    );
    Gauge::default()
        .block(Block::default().title(" Tendencia ").borders(Borders::ALL))
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(label)
}

fn position_panel(app: &TradingApp) -> Paragraph<'static> {
    let lines = if let Some(position) = &app.engine.position {
        let pnl = app.current_pnl;
        vec![
            Line::from(format!("Estado       {:?}", app.engine.state)),
            Line::from(format!("Contrato     {}", position.option_symbol)),
            Line::from(format!(
                "Tipo / qty   {:?} / {} x {}",
                position.kind, position.contracts, position.contract_multiplier
            )),
            Line::from(format!("Entrada      {:.4}", position.entry_price)),
            Line::from(format!(
                "P&L neto     {}",
                pnl.map_or_else(|| "—".into(), |value| format!("{:.2}", value.net))
            )),
            Line::from(format!(
                "Objetivo     {}",
                pnl.map_or_else(|| "—".into(), |value| format!("{:.2}", value.threshold))
            )),
        ]
    } else {
        vec![
            Line::from(format!("Estado       {:?}", app.engine.state)),
            Line::from("Sin posición activa"),
            Line::from(format!("Última salida {:?}", app.engine.last_exit_reason)),
        ]
    };
    Paragraph::new(lines).block(Block::default().title(" Posición ").borders(Borders::ALL))
}

fn risk_panel(app: &TradingApp) -> Paragraph<'static> {
    let metrics = app.metrics();
    let state_color = if app.risk.state.kill_switch || app.engine.state == TradingState::Halted {
        Color::Red
    } else {
        Color::Green
    };
    Paragraph::new(vec![
        Line::from(vec![
            Span::raw("Kill switch  "),
            Span::styled(
                if app.risk.state.kill_switch {
                    "ACTIVO"
                } else {
                    "normal"
                },
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!(
            "Presupuesto compra {:.2}",
            app.risk.limits.max_notional
        )),
        Line::from(format!(
            "Pérdida/día  {:.2}",
            app.risk.limits.max_daily_loss
        )),
        Line::from(format!("Realizado    {:.2}", metrics.realized_pnl)),
        Line::from(format!(
            "Trades       {} / {} ({}W/{}L)",
            metrics.trades, app.risk.limits.max_trades_per_day, metrics.wins, metrics.losses
        )),
    ])
    .block(Block::default().title(" Riesgo ").borders(Borders::ALL))
}

fn price(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:.2}"))
}
