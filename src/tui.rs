use std::{
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction as LayoutDirection, Layout},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, Paragraph, Sparkline, Wrap},
    Frame, Terminal,
};

use crate::{
    app::TradingApp,
    errors::AppError,
    number_format::{decimal, integer},
    pattern::Direction,
    trading::TradingState,
};

struct TerminalGuard;

const MUTED_GRAY: Color = Color::Rgb(80, 80, 80);
const DEEP_GRAY: Color = Color::Rgb(72, 72, 72);

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
    let visual_height = if area.height >= 38 { 14 } else { 10 };
    let rows = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(visual_height),
            Constraint::Length(10),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);

    let mode_color = match (app.config.mode, app.live_stage) {
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Learning) => Color::Yellow,
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Live) => Color::Cyan,
        (crate::config::Mode::Live, crate::learning::LiveStage::Learning) => Color::Yellow,
        (crate::config::Mode::Live, crate::learning::LiveStage::Live) => Color::Red,
    };
    let account = app.account_profile.as_ref().map_or_else(
        || "Cuenta: esperando los datos de IOL".into(),
        |profile| {
            format!(
                "Cuenta {} · {}",
                profile.account_number,
                profile.full_name()
            )
        },
    );
    let connection = simple_connection_status(&app.realtime_status);
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                " OPCIONES / IOL ",
                Style::default()
                    .fg(Color::Black)
                    .bg(mode_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  {} · {} · actualización {}  ",
                mode_name(app.config.mode, app.live_stage),
                app.config.ticker,
                integer(app.ticks)
            )),
            Span::styled(
                if app.paused { "PAUSADO" } else { &app.status },
                Style::default()
                    .fg(if app.paused { Color::Yellow } else { DEEP_GRAY })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!(" {account} · ")),
            Span::styled(
                connection.clone(),
                Style::default().fg(connection_color(&connection)),
            ),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, rows[0]);

    let upper = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(rows[1]);
    render_market_chart(frame, app, upper[0]);
    let insights = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(upper[1]);
    frame.render_widget(market_panel(app), insights[0]);
    frame.render_widget(trend_panel(app), insights[1]);

    let lower = Layout::default()
        .direction(LayoutDirection::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[2]);
    render_position_panel(frame, app, lower[0]);
    render_risk_panel(frame, app, lower[1]);

    let journal = if area.width >= 100 && rows[3].height >= 7 {
        let sections = Layout::default()
            .direction(LayoutDirection::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(rows[3]);
        render_equity_curve(frame, app, sections[0]);
        sections[1]
    } else {
        rows[3]
    };
    let visible_logs = journal.height.saturating_sub(2) as usize;
    let skip_logs = app.logs().len().saturating_sub(visible_logs);
    let log_lines: Vec<Line> = app
        .logs()
        .iter()
        .skip(skip_logs)
        .map(|entry| {
            Line::from(vec![
                Span::styled("◆ ", Style::default().fg(log_color(&entry.message))),
                Span::styled(
                    argentina_time(entry.timestamp_secs),
                    Style::default().fg(DEEP_GRAY),
                ),
                Span::raw(" · "),
                Span::styled(
                    entry.message.clone(),
                    Style::default().fg(log_color(&entry.message)),
                ),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(log_lines)
            .block(
                Block::default()
                    .title(" Lo que fue pasando ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        journal,
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " q",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" salir  "),
            Span::styled("⏯ espacio/p", Style::default().fg(Color::Yellow)),
            Span::raw(" pausar  "),
            Span::styled(
                "⚠ k",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" freno  "),
            Span::styled("◆ c", Style::default().fg(Color::Magenta)),
            Span::raw(" vender  "),
            Span::styled("▣ s", Style::default().fg(Color::Blue)),
            Span::raw(" guardar"),
        ]))
        .style(Style::default().fg(MUTED_GRAY)),
        rows[4],
    );
}

fn render_market_chart(frame: &mut Frame, app: &TradingApp, area: ratatui::layout::Rect) {
    let samples = app.price_history();
    if samples.len() < 2 {
        frame.render_widget(
            Paragraph::new("  Reuniendo precios para dibujar la sesión…")
                .style(Style::default().fg(DEEP_GRAY))
                .block(
                    Block::default()
                        .title(" ⟡ Pulso del mercado ")
                        .borders(Borders::ALL),
                ),
            area,
        );
        return;
    }

    let prices: Vec<(f64, f64)> = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| (index as f64, sample.price))
        .collect();
    let sma = app.current_trend.as_ref().map_or_else(
        || samples.iter().map(|sample| sample.price).sum::<f64>() / samples.len() as f64,
        |trend| trend.sma,
    );
    let x_max = (samples.len() - 1) as f64;
    let sma_line = [(0.0, sma), (x_max, sma)];
    let current = [prices[prices.len() - 1]];
    let (lower, upper) = chart_bounds(samples.iter().map(|sample| sample.price).chain([sma]));
    let signal_color = app
        .current_trend
        .as_ref()
        .map_or(MUTED_GRAY, |trend| direction_color(trend.direction));
    let signal = app.current_trend.as_ref().map_or("SIN SEÑAL", |trend| {
        if !trend.confirmed {
            "OBSERVANDO"
        } else {
            match trend.direction {
                Direction::Up => "▲ SUBA CONFIRMADA",
                Direction::Down => "▼ BAJA CONFIRMADA",
                Direction::Neutral => "◆ NEUTRAL",
            }
        }
    });
    let first_time = samples
        .front()
        .map_or_else(|| "—".into(), |sample| short_time(sample.timestamp_secs));
    let last_time = samples
        .back()
        .map_or_else(|| "—".into(), |sample| short_time(sample.timestamp_secs));
    let datasets = vec![
        Dataset::default()
            .name("Precio")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&prices),
        Dataset::default()
            .name("SMA")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&sma_line),
        Dataset::default()
            .marker(symbols::Marker::Block)
            .graph_type(GraphType::Scatter)
            .style(
                Style::default()
                    .fg(signal_color)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&current),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Line::from(vec![
                    Span::styled(
                        " ⟡ PULSO ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        signal,
                        Style::default()
                            .fg(signal_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DEEP_GRAY)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(DEEP_GRAY))
                .bounds([0.0, x_max])
                .labels([first_time, last_time]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(DEEP_GRAY))
                .bounds([lower, upper])
                .labels([decimal(lower, 2), decimal(upper, 2)]),
        );
    frame.render_widget(chart, area);
}

fn market_panel(app: &TradingApp) -> Paragraph<'static> {
    let lines = if let Some(frame) = &app.current_frame {
        let selected = app
            .selected_option
            .as_deref()
            .and_then(|symbol| frame.option(symbol));
        vec![
            Line::from(vec![
                Span::styled(
                    format!(
                        "{} {}",
                        frame.underlying.ticker,
                        decimal(frame.underlying.last, 2)
                    ),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  bid/ask {} / {}",
                    price(frame.underlying.bid),
                    price(frame.underlying.ask)
                )),
            ]),
            Line::from(format!(
                "◇ {} · {} · {} opciones · {}",
                selected.map_or("sin opción", |option| option.symbol.as_str()),
                selected.map_or("—", |option| option_kind_name(option.kind)),
                integer(frame.options.len()),
                quote_age(frame.underlying.timestamp_secs)
            )),
            Line::from(vec![
                Span::raw(format!(
                    "Prima {} / {}  ",
                    selected.map_or_else(|| "—".into(), |option| price(option.bid)),
                    selected.map_or_else(|| "—".into(), |option| price(option.ask))
                )),
                Span::styled(
                    selected
                        .and_then(|option| option.spread_percentage())
                        .map_or_else(
                            || "spread —".into(),
                            |spread| format!("spread {}%", decimal(spread, 2)),
                        ),
                    Style::default().fg(spread_color(
                        selected.and_then(|option| option.spread_percentage()),
                        app.config.max_option_spread_percentage,
                    )),
                ),
            ]),
        ]
    } else {
        vec![Line::from("Esperando datos de mercado…")]
    };
    Paragraph::new(lines).block(
        Block::default()
            .title(" ◇ Mercado y opción ")
            .borders(Borders::ALL),
    )
}

fn trend_panel(app: &TradingApp) -> Paragraph<'static> {
    let (lines, color) = app.current_trend.as_ref().map_or_else(
        || {
            (
                vec![Line::from(
                    "Todavía no hay suficientes precios para decidir.",
                )],
                DEEP_GRAY,
            )
        },
        |trend| {
            let direction = match trend.direction {
                Direction::Up => "Parece estar subiendo",
                Direction::Down => "Parece estar bajando",
                Direction::Neutral => "No hay una dirección clara",
            };
            let strength = trend.r_squared.unwrap_or(0.0).clamp(0.0, 1.0);
            (
                vec![
                    Line::from(vec![
                        Span::styled(
                            if trend.confirmed { "● " } else { "◌ " },
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(direction, Style::default().add_modifier(Modifier::BOLD)),
                    ]),
                    Line::from(format!(
                        "SMA {} · ritmo {}",
                        decimal(trend.sma, 2),
                        signed_decimal(trend.slope, 4)
                    )),
                    Line::from(format!(
                        "Ruido {} · señal {} · racha {}",
                        decimal(trend.volatility, 3),
                        confidence_name(strength),
                        integer(trend.samples)
                    )),
                ],
                match trend.direction {
                    Direction::Up => Color::Green,
                    Direction::Down => Color::Red,
                    Direction::Neutral => MUTED_GRAY,
                },
            )
        },
    );
    Paragraph::new(lines)
        .style(Style::default().fg(color))
        .block(
            Block::default()
                .title(" ◉ Lectura de tendencia ")
                .borders(Borders::ALL),
        )
}

fn render_position_panel(frame: &mut Frame, app: &TradingApp, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" ◆ Posición · radar P&L ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DEEP_GRAY));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(inner);

    let Some(position) = &app.engine.position else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        "◇ EN ESPERA  ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(trading_state_name(app.engine.state)),
                ]),
                Line::from("El radar se activa al comprar una opción."),
                Line::from(format!(
                    "Última salida: {}",
                    app.engine
                        .last_exit_reason
                        .map_or("todavía ninguna", exit_reason_name)
                )),
            ]),
            inner,
        );
        return;
    };

    let pnl = app.current_pnl;
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("◆ {}", position.option_symbol),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " · {} · {}×{}",
                    position_kind_name(position.kind),
                    integer(position.contracts),
                    integer(position.contract_multiplier)
                )),
            ]),
            Line::from(format!(
                "Entrada {} · {}",
                decimal(position.entry_price, 4),
                trading_state_name(app.engine.state)
            )),
            Line::from(pnl.map_or_else(
                || "Neto incluye costo de compra, costo de venta e impuesto estimado.".into(),
                |value| {
                    format!(
                        "Costos: compra {} · venta {} · impuesto {}",
                        decimal(value.entry_cost, 2),
                        decimal(value.exit_cost, 2),
                        decimal(value.tax, 2)
                    )
                },
            )),
        ]),
        sections[0],
    );
    let stop = app.risk.limits.max_loss_per_trade.max(0.01);
    let target = pnl.map_or(stop, |value| value.threshold.max(0.01));
    let net = pnl.map_or(0.0, |value| value.net);
    let ratio = ((net + stop) / (stop + target)).clamp(0.0, 1.0);
    let color = pnl_color(net, target, stop);
    let label = format!(
        "STOP −{}  ◀  P&L {}{}  ▶  META +{}",
        decimal(stop, 0),
        if net >= 0.0 { "+" } else { "" },
        decimal(net, 2),
        decimal(target, 2)
    );
    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::TOP))
            .ratio(ratio)
            .label(label)
            .use_unicode(true)
            .gauge_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().fg(Color::White).bg(Color::Black)),
        sections[1],
    );
}

fn render_risk_panel(frame: &mut Frame, app: &TradingApp, area: ratatui::layout::Rect) {
    let metrics = app.metrics();
    let halted = app.risk.state.kill_switch || app.engine.state == TradingState::Halted;
    let state_color = if halted { Color::Red } else { Color::Green };
    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(" ⚑ RIESGO · cerrado "),
            Span::styled(
                format!(
                    "{}{} ",
                    if metrics.realized_pnl >= 0.0 { "+" } else { "" },
                    decimal(metrics.realized_pnl, 2)
                ),
                Style::default()
                    .fg(if metrics.realized_pnl >= 0.0 {
                        Color::Green
                    } else {
                        Color::Red
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if halted { Color::Red } else { DEEP_GRAY }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if halted {
                    "■ FRENO ACTIVO"
                } else {
                    "● SISTEMA HABILITADO"
                },
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " · compra máx. {}",
                decimal(app.risk.limits.max_notional, 0)
            )),
        ])),
        sections[0],
    );

    let loss_used = (-app.risk.state.realized_pnl).max(0.0);
    let loss_ratio = (loss_used / app.risk.limits.max_daily_loss).clamp(0.0, 1.0);
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(" Pérdida diaria consumida ")
                    .borders(Borders::TOP),
            )
            .ratio(loss_ratio)
            .label(format!(
                "{} / {}  ·  {:.0}%",
                decimal(loss_used, 2),
                decimal(app.risk.limits.max_daily_loss, 2),
                loss_ratio * 100.0
            ))
            .use_unicode(true)
            .gauge_style(gauge_style(risk_color(loss_ratio, halted))),
        sections[1],
    );
    let trade_ratio =
        (metrics.trades as f64 / app.risk.limits.max_trades_per_day as f64).clamp(0.0, 1.0);
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(" Operaciones del día ")
                    .borders(Borders::TOP),
            )
            .ratio(trade_ratio)
            .label(format!(
                "{} / {}  ·  ✓{}  ✕{}",
                integer(metrics.trades),
                integer(app.risk.limits.max_trades_per_day),
                integer(metrics.wins),
                integer(metrics.losses)
            ))
            .use_unicode(true)
            .gauge_style(gauge_style(risk_color(trade_ratio, false))),
        sections[2],
    );
}

fn render_equity_curve(frame: &mut Frame, app: &TradingApp, area: ratatui::layout::Rect) {
    let mut cumulative = 0.0;
    let values: Vec<f64> = app
        .portfolio
        .closed_trades()
        .iter()
        .map(|trade| {
            cumulative += trade.net_pnl;
            cumulative
        })
        .collect();
    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(" ∿ CURVA NETA  "),
            Span::styled(
                format!(
                    "{}{} ",
                    if cumulative >= 0.0 { "+" } else { "" },
                    decimal(cumulative, 2)
                ),
                Style::default()
                    .fg(if cumulative >= 0.0 {
                        Color::Green
                    } else {
                        Color::Red
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL);
    if values.is_empty() {
        frame.render_widget(
            Paragraph::new("La curva nace con el primer cierre.")
                .style(Style::default().fg(DEEP_GRAY))
                .block(block),
            area,
        );
        return;
    }
    let data = sparkline_values(&values);
    frame.render_widget(
        Sparkline::default()
            .block(block)
            .data(&data)
            .max(100)
            .bar_set(symbols::bar::NINE_LEVELS)
            .style(Style::default().fg(if cumulative >= 0.0 {
                Color::Green
            } else {
                Color::Red
            })),
        area,
    );
}

fn chart_bounds(values: impl IntoIterator<Item = f64>) -> (f64, f64) {
    let mut minimum = f64::INFINITY;
    let mut maximum = f64::NEG_INFINITY;
    for value in values.into_iter().filter(|value| value.is_finite()) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return (0.0, 1.0);
    }
    let padding = ((maximum - minimum) * 0.08)
        .max(maximum.abs() * 0.0005)
        .max(0.01);
    (minimum - padding, maximum + padding)
}

fn sparkline_values(values: &[f64]) -> Vec<u64> {
    let minimum = values.iter().copied().fold(0.0_f64, f64::min);
    let maximum = values.iter().copied().fold(0.0_f64, f64::max);
    let span = maximum - minimum;
    if span <= f64::EPSILON {
        return vec![50; values.len()];
    }
    values
        .iter()
        .map(|value| (((value - minimum) / span) * 99.0).round() as u64 + 1)
        .collect()
}

fn direction_color(direction: Direction) -> Color {
    match direction {
        Direction::Up => Color::Green,
        Direction::Down => Color::Red,
        Direction::Neutral => MUTED_GRAY,
    }
}

fn pnl_color(net: f64, target: f64, stop: f64) -> Color {
    if net >= target {
        Color::Green
    } else if net <= -stop * 0.75 {
        Color::Red
    } else if net < 0.0 {
        Color::LightRed
    } else {
        Color::Yellow
    }
}

fn risk_color(ratio: f64, halted: bool) -> Color {
    if halted || ratio >= 0.8 {
        Color::Red
    } else if ratio >= 0.5 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn gauge_style(color: Color) -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(color)
        .add_modifier(Modifier::BOLD)
}

fn spread_color(spread: Option<f64>, limit: f64) -> Color {
    match spread {
        None => DEEP_GRAY,
        Some(value) if value > limit => Color::Red,
        Some(value) if value > limit * 0.7 => Color::Yellow,
        Some(_) => Color::Green,
    }
}

fn connection_color(status: &str) -> Color {
    let status = status.to_lowercase();
    if status.contains("error")
        || status.contains("rechaz")
        || status.contains("desconect")
        || status.contains("fall")
    {
        Color::Red
    } else if status.contains("esperando") || status.contains("conectando") {
        Color::Yellow
    } else if status.contains("no necesita") {
        Color::Cyan
    } else {
        Color::Green
    }
}

fn log_color(message: &str) -> Color {
    let message = message.to_lowercase();
    if message.contains("límite")
        || message.contains("freno")
        || message.contains("rechaz")
        || message.contains("error")
    {
        Color::Red
    } else if message.contains("venta realizada") || message.contains("ganancia") {
        Color::Green
    } else if message.contains("compra") || message.contains("opción") {
        Color::Magenta
    } else {
        MUTED_GRAY
    }
}

fn short_time(timestamp_secs: i64) -> String {
    argentina_time(timestamp_secs)[..5].to_owned()
}

fn price(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |value| decimal(value, 2))
}

fn signed_decimal(value: f64, precision: usize) -> String {
    let formatted = decimal(value, precision);
    if value.is_sign_negative() {
        formatted
    } else {
        format!("+{formatted}")
    }
}

fn mode_name(mode: crate::config::Mode, live_stage: crate::learning::LiveStage) -> &'static str {
    match (mode, live_stage) {
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Learning) => {
            "READONLY · LEARNING (SIN ÓRDENES)"
        }
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Live) => {
            "READONLY · LIVE (SÓLO AVISA)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Learning) => {
            "LIVE · APRENDIZAJE (DINERO SIMULADO)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Live) => {
            "LIVE · OPERACIÓN (DINERO REAL)"
        }
    }
}

fn option_kind_name(kind: crate::market::OptionKind) -> &'static str {
    match kind {
        crate::market::OptionKind::Call => "CALL ▲",
        crate::market::OptionKind::Put => "PUT ▼",
    }
}

fn position_kind_name(kind: crate::trading::PositionKind) -> &'static str {
    match kind {
        crate::trading::PositionKind::Call => "CALL (suba)",
        crate::trading::PositionKind::Put => "PUT (baja)",
    }
}

fn trading_state_name(state: TradingState) -> &'static str {
    match state {
        TradingState::Idle => "esperando una oportunidad",
        TradingState::SearchingCall => "buscando una opción para una suba",
        TradingState::SearchingPut => "buscando una opción para una baja",
        TradingState::Buying => "intentando comprar",
        TradingState::CallActive | TradingState::PutActive => "siguiendo la compra",
        TradingState::Selling => "intentando vender",
        TradingState::Halted => "detenido por seguridad",
    }
}

fn exit_reason_name(reason: crate::trading::ExitReason) -> &'static str {
    match reason {
        crate::trading::ExitReason::ProfitTarget => "se alcanzó la ganancia buscada",
        crate::trading::ExitReason::StopLoss => "se llegó al límite de pérdida",
        crate::trading::ExitReason::TrendReversal => "el precio cambió de dirección",
        crate::trading::ExitReason::Timeout => "se cumplió el tiempo máximo",
        crate::trading::ExitReason::RiskLimit => "se alcanzó un límite de seguridad",
        crate::trading::ExitReason::Manual => "venta pedida por la persona",
        crate::trading::ExitReason::Defensive => "venta preventiva por un dato dudoso",
    }
}

fn confidence_name(value: f64) -> &'static str {
    if value >= 0.8 {
        "alta"
    } else if value >= 0.5 {
        "media"
    } else {
        "baja"
    }
}

fn simple_connection_status(status: &str) -> String {
    status
        .replace("WebSocket", "Conexión con IOL")
        .replace("websocket", "conexión con IOL")
        .replace("WS", "IOL")
}

fn quote_age(timestamp_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64);
    let seconds = now.saturating_sub(timestamp_secs).max(0);
    match seconds {
        0..=1 => "menos de 2 segundos".into(),
        2..=59 => format!("{} segundos", integer(seconds)),
        _ => format!("{} minutos", integer(seconds / 60)),
    }
}

fn argentina_time(timestamp_secs: i64) -> String {
    const ARGENTINA_UTC_OFFSET_SECS: i64 = -3 * 60 * 60;
    let seconds_today = (timestamp_secs + ARGENTINA_UTC_OFFSET_SECS).rem_euclid(24 * 60 * 60);
    let hours = seconds_today / (60 * 60);
    let minutes = (seconds_today % (60 * 60)) / 60;
    let seconds = seconds_today % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::{argentina_time, chart_bounds, risk_color, signed_decimal, sparkline_values};
    use ratatui::style::Color;

    #[test]
    fn formats_explicit_positive_sign() {
        assert_eq!(signed_decimal(1_234.5, 2), "+1.234,50");
    }

    #[test]
    fn formats_log_timestamp_as_argentine_time_only() {
        // 2026-08-21 15:30:45 UTC = 12:30:45 en Argentina.
        assert_eq!(argentina_time(1_787_326_245), "12:30:45");
    }

    #[test]
    fn chart_bounds_leave_visible_space_for_flat_prices() {
        let (lower, upper) = chart_bounds([100.0, 100.0]);
        assert!(lower < 100.0);
        assert!(upper > 100.0);
    }

    #[test]
    fn equity_curve_scaling_preserves_shape_across_losses_and_gains() {
        assert_eq!(sparkline_values(&[-10.0, 0.0, 20.0]), vec![1, 34, 100]);
    }

    #[test]
    fn risk_palette_escalates_from_green_to_yellow_to_red() {
        assert_eq!(risk_color(0.2, false), Color::Green);
        assert_eq!(risk_color(0.6, false), Color::Yellow);
        assert_eq!(risk_color(0.8, false), Color::Red);
        assert_eq!(risk_color(0.0, true), Color::Red);
    }
}
