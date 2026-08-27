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
    layout::{Alignment, Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Clear, Dataset, Gauge, GraphType, Paragraph, Sparkline, Wrap,
    },
    Frame, Terminal,
};

use crate::{
    app::TradingApp,
    errors::AppError,
    iol_client::WebsocketConnectionState,
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
    let mut connection_failure_is_terminal = false;

    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if connection_failure_is_terminal {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('s') => app.snapshot()?,
                            _ => {}
                        }
                        continue;
                    }
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
        if !connection_failure_is_terminal && tokio::time::Instant::now() >= next_tick {
            let running = match app.step().await {
                Ok(running) => running,
                Err(mut last_error @ AppError::Connection(_)) => {
                    let attempts = app.config.connection_retry_attempts;
                    let delay = Duration::from_secs(app.config.connection_retry_delay_secs);
                    let mut recovered = None;
                    for attempt in 1..=attempts {
                        app.mark_connection_retry(attempt, attempts, &last_error);
                        terminal.draw(|frame| draw(frame, app))?;
                        tokio::time::sleep(delay).await;
                        match app.step().await {
                            Ok(running) => {
                                app.mark_connection_restored();
                                recovered = Some(running);
                                break;
                            }
                            Err(error @ AppError::Connection(_)) => last_error = error,
                            Err(error) => return Err(error),
                        }
                    }
                    match recovered {
                        Some(running) => running,
                        None => {
                            app.mark_connection_not_operational(attempts, &last_error)?;
                            connection_failure_is_terminal = true;
                            terminal.draw(|frame| draw(frame, app))?;
                            true
                        }
                    }
                }
                Err(error) => return Err(error),
            };
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
    let operational_status = crate::redaction::sanitize_operational_message(&app.status);
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
        (
            crate::config::Mode::Readonly,
            crate::learning::LiveStage::Eligible
            | crate::learning::LiveStage::Armed
            | crate::learning::LiveStage::Canary
            | crate::learning::LiveStage::Live,
        ) => Color::Cyan,
        (crate::config::Mode::Live, crate::learning::LiveStage::Learning) => Color::Yellow,
        (crate::config::Mode::Live, crate::learning::LiveStage::Eligible) => Color::LightYellow,
        (crate::config::Mode::Live, crate::learning::LiveStage::Armed) => Color::LightMagenta,
        (crate::config::Mode::Live, crate::learning::LiveStage::Canary) => Color::LightRed,
        (crate::config::Mode::Live, crate::learning::LiveStage::Live) => Color::Red,
    };
    let account = app.account_profile.as_ref().map_or_else(
        || "Cuenta: esperando los datos de IOL".into(),
        |profile| {
            format!(
                "Cuenta {} · {}",
                profile.masked_account_number(),
                profile.redacted_name()
            )
        },
    );
    let iol_status = if app.connection_operational {
        "IOL: ONLINE"
    } else {
        "IOL: OFFLINE"
    };
    let websocket = websocket_status(app.websocket_status);
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
                if app.paused {
                    "PAUSADO"
                } else {
                    &operational_status
                },
                Style::default()
                    .fg(if app.paused { Color::Yellow } else { DEEP_GRAY })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw(format!(" {account} · ")),
            Span::styled(
                iol_status,
                Style::default().fg(if app.connection_operational {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
            Span::raw(" · "),
            Span::styled(
                websocket,
                Style::default().fg(websocket_color(app.websocket_status)),
            ),
            Span::raw(" · "),
            Span::styled(
                app.market_status.clone(),
                Style::default()
                    .fg(if app.market_force_pre_break_exit {
                        Color::Red
                    } else if !app.market_open || !app.market_entries_allowed || app.lunch_slowdown
                    {
                        Color::Yellow
                    } else {
                        Color::Green
                    })
                    .add_modifier(Modifier::BOLD),
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
                    formatted_log_message(&entry.message, entry.repetitions),
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
    if !app.connection_operational {
        render_not_operational(frame, area, &operational_status);
    } else if !app.market_open {
        render_market_offline(frame, area, &app.market_status, &app.market_status_detail);
    }
}

fn formatted_log_message(message: &str, repetitions: u64) -> String {
    if repetitions > 1 {
        format!("{message} ({repetitions})")
    } else {
        message.to_string()
    }
}

fn render_market_offline(frame: &mut Frame, area: Rect, headline: &str, detail: &str) {
    let vertical = Layout::vertical([
        Constraint::Percentage(35),
        Constraint::Length(7),
        Constraint::Percentage(35),
    ])
    .split(area);
    let popup = Layout::horizontal([
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
    ])
    .split(vertical[1])[1];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                headline,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(detail),
            Line::from("No se solicitan cotizaciones ni se abren operaciones."),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(" OFFLINE ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_not_operational(frame: &mut Frame, area: Rect, detail: &str) {
    let vertical = Layout::vertical([
        Constraint::Percentage(30),
        Constraint::Length(9),
        Constraint::Percentage(30),
    ])
    .split(area);
    let popup = Layout::horizontal([
        Constraint::Percentage(10),
        Constraint::Percentage(80),
        Constraint::Percentage(10),
    ])
    .split(vertical[1])[1];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "NO OPERATIVO · SIN CONEXIÓN CON IOL",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(detail),
            Line::from(""),
            Line::from("No se procesan precios ni órdenes. Presione q para salir."),
        ])
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(" ALERTA CRÍTICA ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        popup,
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
    let title = app.current_vix_display().map_or_else(
        || Line::from(" ◇ Mercado · VIX NO DISPONIBLE "),
        |(vix, state)| vix_title(app, vix, state),
    );
    let lines = if let Some(frame) = &app.current_frame {
        let selected = app
            .selected_option
            .as_deref()
            .and_then(|symbol| frame.option(symbol));
        let option_count = frame.option_chain_quality.as_ref().map_or_else(
            || format!("{} opciones", integer(frame.options.len())),
            |quality| {
                format!(
                    "{}/{} opciones válidas",
                    integer(quality.accepted_contracts),
                    integer(quality.catalog_contracts)
                )
            },
        );
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
                "◇ {} · {} · {} · {}",
                selected.map_or("sin opción", |option| option.symbol.as_str()),
                selected.map_or("—", |option| option_kind_name(option.kind)),
                option_count,
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
    Paragraph::new(lines).block(Block::default().title(title).borders(Borders::ALL))
}

fn vix_title(
    app: &TradingApp,
    vix: crate::market::VixObservation,
    freshness: crate::market::VixFreshnessState,
) -> Line<'static> {
    if freshness == crate::market::VixFreshnessState::Stale {
        return Line::from(vec![
            Span::raw(" ◇ Mercado · VIX "),
            Span::styled(
                "DESACTUALIZADO",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
    }
    if freshness == crate::market::VixFreshnessState::PreviousClose {
        return Line::from(vec![
            Span::raw(" ◇ Mercado · VIX "),
            Span::styled(decimal(vix.level, 2), Style::default().fg(Color::Yellow)),
            Span::raw(" · "),
            Span::styled(
                "CIERRE PREVIO",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]);
    }
    let change = vix.change_percentage();
    let (regime, color) =
        if change.is_some_and(|value| value >= app.config.vix_spike_change_percentage) {
            ("SALTO", Color::Red)
        } else if vix.level >= app.config.vix_elevated_level {
            ("ELEVADO", Color::Yellow)
        } else {
            ("NORMAL", Color::Green)
        };
    let change = change.map_or_else(
        || "".into(),
        |value| format!(" {}%", signed_decimal(value, 2)),
    );
    Line::from(vec![
        Span::raw(" ◇ Mercado · VIX "),
        Span::styled(
            decimal(vix.level, 2),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(change, Style::default().fg(color)),
        Span::raw(" · "),
        Span::styled("VIGENTE", Style::default().fg(color)),
        Span::raw(" · "),
        Span::styled(
            regime,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ])
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
                " · compra máx. {} · disco {:.0}% · libres {}",
                decimal(app.risk.limits.max_notional, 0),
                app.storage_capacity
                    .quota_usage_ratio(app.config.data_dir_max_bytes)
                    * 100.0,
                human_bytes(app.storage_capacity.available_bytes)
            )),
            Span::styled(
                if app.clock_synchronized {
                    " · reloj OK"
                } else {
                    " · reloj NO VERIFICADO"
                },
                Style::default().fg(if app.clock_synchronized {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
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

fn human_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB)
    }
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

fn websocket_status(status: WebsocketConnectionState) -> &'static str {
    match status {
        WebsocketConnectionState::Disabled => "WS: DESACTIVADO",
        WebsocketConnectionState::Connecting => "WS: CONECTANDO",
        WebsocketConnectionState::Connected => "WS: CONECTADO",
        WebsocketConnectionState::Reconnecting => "WS: RECONECTANDO",
        WebsocketConnectionState::Offline => "WS: OFFLINE",
    }
}

fn websocket_color(status: WebsocketConnectionState) -> Color {
    match status {
        WebsocketConnectionState::Connected => Color::Green,
        WebsocketConnectionState::Connecting | WebsocketConnectionState::Reconnecting => {
            Color::Yellow
        }
        WebsocketConnectionState::Disabled => Color::DarkGray,
        WebsocketConnectionState::Offline => Color::Red,
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
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Eligible) => {
            "READONLY · ELEGIBLE (SÓLO AVISA)"
        }
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Armed) => {
            "READONLY · ARMED INACTIVO (SIN ÓRDENES)"
        }
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Canary) => {
            "READONLY · CANARY INACTIVO (SIN ÓRDENES)"
        }
        (crate::config::Mode::Readonly, crate::learning::LiveStage::Live) => {
            "READONLY · LIVE (SÓLO AVISA)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Learning) => {
            "LIVE · APRENDIZAJE (DINERO SIMULADO)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Eligible) => {
            "LIVE · ELEGIBLE (ESPERA AUTORIZACIÓN)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Armed) => {
            "LIVE · ARMED (PREFLIGHT)"
        }
        (crate::config::Mode::Live, crate::learning::LiveStage::Canary) => {
            "LIVE · CANARY (DINERO REAL LIMITADO)"
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
        crate::trading::ExitReason::WeekendRisk => "cierre previo a una pausa prolongada",
        crate::trading::ExitReason::ExpiryRisk => "cierre previo al límite de vencimiento",
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
    let (hours, minutes, seconds) = crate::time_utils::argentina_hms(timestamp_secs);
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::{
        argentina_time, chart_bounds, draw, formatted_log_message, render_market_offline,
        render_not_operational, risk_color, signed_decimal, sparkline_values, websocket_status,
    };
    use crate::app::TradingApp;
    use crate::iol_client::WebsocketConnectionState;
    use ratatui::{backend::TestBackend, style::Color, Terminal};

    #[test]
    fn formats_explicit_positive_sign() {
        assert_eq!(signed_decimal(1_234.5, 2), "+1.234,50");
    }

    #[test]
    fn repeated_log_message_has_a_compact_counter() {
        assert_eq!(formatted_log_message("Sin cambios", 1), "Sin cambios");
        assert_eq!(formatted_log_message("Sin cambios", 7), "Sin cambios (7)");
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

    #[test]
    fn websocket_header_uses_short_operational_labels() {
        assert_eq!(
            websocket_status(WebsocketConnectionState::Disabled),
            "WS: DESACTIVADO"
        );
        assert_eq!(
            websocket_status(WebsocketConnectionState::Connected),
            "WS: CONECTADO"
        );
        assert_eq!(
            websocket_status(WebsocketConnectionState::Reconnecting),
            "WS: RECONECTANDO"
        );
        assert_eq!(
            websocket_status(WebsocketConnectionState::Offline),
            "WS: OFFLINE"
        );
    }

    #[test]
    fn closed_market_popup_says_offline_and_explains_the_reason() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_market_offline(
                    frame,
                    frame.area(),
                    "OFFLINE · MERCADO CERRADO",
                    "Feriado: Día de la Independencia",
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("OFFLINE · MERCADO CERRADO"));
        assert!(rendered.contains("Feriado: Día de la Independencia"));
        assert!(rendered.contains("No se solicitan cotizaciones"));
    }

    #[test]
    fn connection_failure_popup_is_short_and_explicit() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_not_operational(frame, frame.area(), "Timeout consultando IOL");
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("NO OPERATIVO · SIN CONEXIÓN CON IOL"));
        assert!(rendered.contains("Timeout consultando IOL"));
        assert!(rendered.contains("No se procesan precios ni órdenes"));
    }

    #[test]
    fn complete_dashboard_renders_operational_state_on_wide_and_compact_terminals() {
        let data_dir = std::env::temp_dir().join(format!(
            "options-tui-render-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = crate::config::tests::config();
        config.data_dir = data_dir.clone();
        config.capture_market_data = false;
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.connection_operational = true;
        app.market_open = true;
        app.market_entries_allowed = true;
        app.market_status = "ONLINE · MERCADO ABIERTO".into();
        app.status = "Observando".into();

        for (width, height) in [(120, 42), (80, 30)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("OPCIONES / IOL"));
            assert!(rendered.contains("IOL: ONLINE"));
            if width >= 100 {
                assert!(rendered.contains("MERCADO ABIERTO"));
            }
            assert!(rendered.contains("Lo que fue pasando"));
        }
        drop(app);
        std::fs::remove_dir_all(data_dir).unwrap();
    }
}
