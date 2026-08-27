use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    secure_fs::{read_limited, write_atomic},
    time_utils::argentina_date_parts,
};

const MARKET_OPEN_MINUTE: u16 = 630;
const MARKET_CLOSE_MINUTE: u16 = 1_020;
const RETRY_DELAY: Duration = Duration::from_secs(300);
const MAX_CALENDAR_BYTES: u64 = 4_194_304;
const MAX_HOLIDAY_RESPONSE_BYTES: usize = 524_288;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketScheduleStatus {
    pub open: bool,
    pub entries_allowed: bool,
    pub force_pre_break_exit: bool,
    pub expiry_exit_due: bool,
    pub pre_break: bool,
    pub next_session_days: u32,
    pub lunch_slowdown: bool,
    pub lunch_reconfirming: bool,
    pub headline: String,
    pub detail: String,
}

impl MarketScheduleStatus {
    fn open() -> Self {
        Self {
            open: true,
            entries_allowed: true,
            force_pre_break_exit: false,
            expiry_exit_due: false,
            pre_break: false,
            next_session_days: 1,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            headline: "ONLINE · MERCADO ABIERTO".into(),
            detail: "Rueda argentina abierta hasta las 17:00".into(),
        }
    }

    fn observing(entries_from_minute: u16) -> Self {
        Self {
            open: true,
            entries_allowed: false,
            force_pre_break_exit: false,
            expiry_exit_due: false,
            pre_break: false,
            next_session_days: 1,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            headline: "ONLINE · OBSERVANDO APERTURA".into(),
            detail: format!(
                "Recopilando precios · entradas habilitadas a las {}",
                format_minute(entries_from_minute)
            ),
        }
    }

    fn closed(detail: impl Into<String>) -> Self {
        Self {
            open: false,
            entries_allowed: false,
            force_pre_break_exit: false,
            expiry_exit_due: false,
            pre_break: false,
            next_session_days: 0,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            headline: "OFFLINE · MERCADO CERRADO".into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct LocalDate {
    year: i32,
    month: u8,
    day: u8,
}

impl LocalDate {
    fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalMoment {
    date: LocalDate,
    weekday_from_monday: u8,
    minute_of_day: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Holiday {
    fecha: String,
    #[serde(default)]
    tipo: String,
    nombre: String,
}

pub const EXCHANGE_CALENDAR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExchangeSessionKind {
    Open,
    Closed,
    SpecialHours,
    TradingWithoutSettlement,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExchangeSession {
    date: String,
    status: ExchangeSessionKind,
    #[serde(default)]
    open: Option<String>,
    #[serde(default)]
    close: Option<String>,
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExchangeCalendarManifest {
    schema_version: u32,
    source_url: String,
    source_sha256: String,
    retrieved_at_secs: i64,
    valid_from: String,
    valid_until: String,
    sessions: Vec<ExchangeSession>,
}

#[derive(Debug, Clone, Copy)]
pub struct MarketCalendarPolicy {
    pub entry_delay_after_open_mins: u32,
    pub weekend_risk_enabled: bool,
    pub pre_break_last_entry_minute: u16,
    pub pre_break_force_exit_minute: u16,
    pub expiry_day_force_exit_minute: u16,
    pub lunch_slowdown_enabled: bool,
    pub lunch_slowdown_start_minute: u16,
    pub lunch_slowdown_end_minute: u16,
    pub post_lunch_confirmation_mins: u32,
    pub lunch_position_factor: f64,
}

pub struct MarketCalendar {
    http: Client,
    api_base_url: String,
    cache_dir: PathBuf,
    holidays: HashMap<i32, HashMap<String, String>>,
    retry_after: HashMap<i32, Instant>,
    last_errors: HashMap<i32, String>,
    entry_delay_after_open_mins: u32,
    weekend_risk_enabled: bool,
    pre_break_last_entry_minute: u16,
    pre_break_force_exit_minute: u16,
    expiry_day_force_exit_minute: u16,
    lunch_slowdown_enabled: bool,
    lunch_slowdown_start_minute: u16,
    lunch_slowdown_end_minute: u16,
    post_lunch_confirmation_mins: u32,
    lunch_position_factor: f64,
    exchange_calendar: Option<ExchangeCalendarManifest>,
    require_exchange_calendar: bool,
}

impl MarketCalendar {
    pub fn new(
        api_base_url: impl Into<String>,
        cache_dir: PathBuf,
        policy: MarketCalendarPolicy,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("options-trading/0.1")
                .build()?,
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            cache_dir,
            holidays: HashMap::new(),
            retry_after: HashMap::new(),
            last_errors: HashMap::new(),
            entry_delay_after_open_mins: policy.entry_delay_after_open_mins,
            weekend_risk_enabled: policy.weekend_risk_enabled,
            pre_break_last_entry_minute: policy.pre_break_last_entry_minute,
            pre_break_force_exit_minute: policy.pre_break_force_exit_minute,
            expiry_day_force_exit_minute: policy.expiry_day_force_exit_minute,
            lunch_slowdown_enabled: policy.lunch_slowdown_enabled,
            lunch_slowdown_start_minute: policy.lunch_slowdown_start_minute,
            lunch_slowdown_end_minute: policy.lunch_slowdown_end_minute,
            post_lunch_confirmation_mins: policy.post_lunch_confirmation_mins,
            lunch_position_factor: policy.lunch_position_factor,
            exchange_calendar: None,
            require_exchange_calendar: false,
        })
    }

    pub fn with_exchange_calendar(
        mut self,
        path: Option<&Path>,
        required: bool,
    ) -> Result<Self, String> {
        self.require_exchange_calendar = required;
        let Some(path) = path else {
            return Ok(self);
        };
        let bytes = read_limited(path, MAX_CALENDAR_BYTES)
            .map_err(|error| format!("no se pudo leer calendario BYMA: {error}"))?;
        let manifest: ExchangeCalendarManifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("calendario BYMA inválido: {error}"))?;
        validate_exchange_calendar(&manifest)?;
        self.exchange_calendar = Some(manifest);
        Ok(self)
    }

    pub async fn status(&mut self, timestamp_secs: i64) -> MarketScheduleStatus {
        let Some(moment) = argentina_moment(timestamp_secs) else {
            return MarketScheduleStatus::closed(
                "Referencia temporal fuera del rango representable",
            );
        };
        let exchange_session = self.exchange_session(moment.date).cloned();
        if self.require_exchange_calendar && !self.exchange_calendar_covers(moment.date) {
            return MarketScheduleStatus::closed(
                "Calendario bursátil BYMA ausente o fuera de vigencia",
            );
        }
        if exchange_session
            .as_ref()
            .is_some_and(|session| session.status == ExchangeSessionKind::Closed)
        {
            return MarketScheduleStatus::closed(format!(
                "Rueda cerrada por BYMA: {}",
                exchange_session
                    .as_ref()
                    .map_or("sin detalle", |session| session.name.as_str())
            ));
        }
        let (open_minute, close_minute) = exchange_session
            .as_ref()
            .and_then(exchange_session_minutes)
            .unwrap_or((MARKET_OPEN_MINUTE, MARKET_CLOSE_MINUTE));
        let explicitly_open = exchange_session.as_ref().is_some_and(|session| {
            matches!(
                session.status,
                ExchangeSessionKind::Open
                    | ExchangeSessionKind::SpecialHours
                    | ExchangeSessionKind::TradingWithoutSettlement
            )
        });
        let regular_schedule = schedule_for_moment_with_hours(
            moment,
            self.entry_delay_after_open_mins,
            open_minute,
            close_minute,
            explicitly_open,
        );
        if !regular_schedule.open {
            return regular_schedule;
        }

        if !explicitly_open {
            let calendar_available = self.ensure_year(moment.date.year).await;
            if !calendar_available {
                let detail = self.last_errors.get(&moment.date.year).map_or_else(
                    || "No se pudo validar el calendario de feriados".into(),
                    |error| format!("Calendario de feriados no disponible: {error}"),
                );
                return MarketScheduleStatus::closed(detail);
            }
            if let Some(name) = self.holiday_name(moment.date) {
                return MarketScheduleStatus::closed(format!("Feriado: {name}"));
            }
        }
        let mut schedule = regular_schedule;
        if let Some(session) = exchange_session {
            schedule.detail = match session.status {
                ExchangeSessionKind::SpecialHours => format!(
                    "Horario especial BYMA {}–{} · {}",
                    format_minute(open_minute),
                    format_minute(close_minute),
                    session.name
                ),
                ExchangeSessionKind::TradingWithoutSettlement => {
                    format!(
                        "Rueda BYMA con negociación sin liquidación · {}",
                        session.name
                    )
                }
                ExchangeSessionKind::Open | ExchangeSessionKind::Closed => session.name,
            };
        }
        let schedule = if self.weekend_risk_enabled {
            let Some(next_session_days) = self.days_until_next_session(timestamp_secs).await else {
                return MarketScheduleStatus::closed(
                    "No se pudo determinar la próxima rueda para controlar el riesgo de pausa",
                );
            };
            self.apply_risk_policy(schedule, moment, next_session_days)
        } else {
            schedule
        };
        self.apply_lunch_policy(schedule, moment)
    }

    pub fn replay_risk_status(
        &self,
        timestamp_secs: i64,
        next_session_days: u32,
    ) -> MarketScheduleStatus {
        let Some(moment) = argentina_moment(timestamp_secs) else {
            return MarketScheduleStatus::closed(
                "Timestamp de replay fuera del rango representable",
            );
        };
        let mut schedule = MarketScheduleStatus::open();
        schedule.headline = "ONLINE · REPLAY".into();
        schedule.detail = "Weekend Risk usa las fechas grabadas en el dataset".into();
        let schedule = self.apply_risk_policy(schedule, moment, next_session_days);
        self.apply_lunch_policy(schedule, moment)
    }

    fn apply_risk_policy(
        &self,
        mut schedule: MarketScheduleStatus,
        moment: LocalMoment,
        next_session_days: u32,
    ) -> MarketScheduleStatus {
        if !self.weekend_risk_enabled {
            return schedule;
        }
        schedule.next_session_days = next_session_days;
        schedule.expiry_exit_due = moment.minute_of_day >= self.expiry_day_force_exit_minute;
        if next_session_days <= 1 {
            return schedule;
        }

        schedule.pre_break = true;
        if moment.minute_of_day >= self.pre_break_force_exit_minute {
            schedule.entries_allowed = false;
            schedule.force_pre_break_exit = true;
            schedule.headline = "ONLINE · CIERRE OBLIGATORIO".into();
            schedule.detail = format!(
                "Próxima rueda en {next_session_days} días · cerrando posiciones antes de las 17:00"
            );
        } else if moment.minute_of_day >= self.pre_break_last_entry_minute {
            schedule.entries_allowed = false;
            schedule.headline = "ONLINE · PAUSA PRÓXIMA".into();
            schedule.detail = format!(
                "Próxima rueda en {next_session_days} días · nuevas entradas bloqueadas desde las {}",
                format_minute(self.pre_break_last_entry_minute)
            );
        }
        schedule
    }

    fn apply_lunch_policy(
        &self,
        mut schedule: MarketScheduleStatus,
        moment: LocalMoment,
    ) -> MarketScheduleStatus {
        if !self.lunch_slowdown_enabled {
            return schedule;
        }
        let minute = u32::from(moment.minute_of_day);
        let lunch_start = u32::from(self.lunch_slowdown_start_minute);
        let lunch_end = u32::from(self.lunch_slowdown_end_minute);
        let recovery_end = lunch_end.saturating_add(self.post_lunch_confirmation_mins);
        let higher_priority_block = schedule.force_pre_break_exit || !schedule.entries_allowed;
        if minute >= lunch_start && minute < lunch_end {
            schedule.lunch_slowdown = true;
            if !higher_priority_block {
                schedule.headline = "ONLINE · LIQUIDEZ DE MEDIODÍA".into();
                schedule.detail = format!(
                    "Exposición al {:.0}% · salidas siempre habilitadas",
                    self.lunch_position_factor * 100.0
                );
            }
        } else if minute >= lunch_end && minute < recovery_end {
            schedule.entries_allowed = false;
            schedule.lunch_reconfirming = true;
            if !higher_priority_block {
                schedule.headline = "ONLINE · RECONFIRMANDO DESPUÉS DEL MEDIODÍA".into();
                schedule.detail = format!(
                    "Nuevas entradas desde las {}",
                    format_minute(recovery_end.min(u32::from(u16::MAX)) as u16)
                );
            }
        }
        schedule
    }

    async fn days_until_next_session(&mut self, timestamp_secs: i64) -> Option<u32> {
        for days in 1..=14_u32 {
            let candidate = argentina_moment(
                timestamp_secs.saturating_add(i64::from(days).saturating_mul(86_400)),
            )?;
            if self.require_exchange_calendar && !self.exchange_calendar_covers(candidate.date) {
                return None;
            }
            let session = self.exchange_session(candidate.date);
            if session.is_some_and(|session| session.status == ExchangeSessionKind::Closed) {
                continue;
            }
            let explicitly_open = session.is_some_and(|session| {
                matches!(
                    session.status,
                    ExchangeSessionKind::Open
                        | ExchangeSessionKind::SpecialHours
                        | ExchangeSessionKind::TradingWithoutSettlement
                )
            });
            if candidate.weekday_from_monday >= 5 && !explicitly_open {
                continue;
            }
            if !self.ensure_year(candidate.date.year).await {
                return None;
            }
            if explicitly_open || self.holiday_name(candidate.date).is_none() {
                return Some(days);
            }
        }
        None
    }

    fn holiday_name(&self, date: LocalDate) -> Option<&str> {
        self.holidays
            .get(&date.year)
            .and_then(|holidays| holidays.get(&date.iso()))
            .map(String::as_str)
    }

    fn exchange_calendar_covers(&self, date: LocalDate) -> bool {
        let iso = date.iso();
        self.exchange_calendar.as_ref().is_some_and(|calendar| {
            date_is_within_inclusive_range(&iso, &calendar.valid_from, &calendar.valid_until)
        })
    }

    fn exchange_session(&self, date: LocalDate) -> Option<&ExchangeSession> {
        let iso = date.iso();
        self.exchange_calendar
            .as_ref()?
            .sessions
            .iter()
            .find(|session| session.date == iso)
    }

    async fn ensure_year(&mut self, year: i32) -> bool {
        if self.holidays.contains_key(&year) {
            return true;
        }
        let path = self.cache_path(year);
        if let Ok(bytes) = read_limited(&path, MAX_CALENDAR_BYTES) {
            match serde_json::from_slice::<Vec<Holiday>>(&bytes) {
                Ok(holidays) => {
                    if holidays_are_valid(year, &holidays) {
                        self.store_year(year, holidays);
                        return true;
                    }
                    self.last_errors
                        .insert(year, "cache vacío o con fechas de otro año".into());
                }
                Err(error) => {
                    self.last_errors
                        .insert(year, format!("cache inválido: {error}"));
                }
            }
        }
        if self
            .retry_after
            .get(&year)
            .is_some_and(|retry_after| retry_window_is_active(Instant::now(), *retry_after))
        {
            return false;
        }
        match self.fetch_year(year).await {
            Ok(holidays) => {
                let cache_error = write_cache_atomic(&path, &holidays)
                    .err()
                    .map(|error| format!("no se pudo guardar el cache: {error}"));
                self.store_year(year, holidays);
                self.retry_after.remove(&year);
                if let Some(error) = cache_error {
                    self.last_errors.insert(year, error);
                } else {
                    self.last_errors.remove(&year);
                }
                true
            }
            Err(error) => {
                let retry_after = Instant::now()
                    .checked_add(RETRY_DELAY)
                    .expect("cinco minutos caben en Instant");
                self.retry_after.insert(year, retry_after);
                self.last_errors.insert(year, error);
                false
            }
        }
    }

    async fn fetch_year(&self, year: i32) -> Result<Vec<Holiday>, String> {
        let response = self
            .http
            .get(format!("{}/{year}", self.api_base_url))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let holidays = decode_holidays(response).await?;
        if !holidays_are_valid(year, &holidays) {
            return Err("respuesta vacía o con fechas de otro año".into());
        }
        Ok(holidays)
    }

    fn store_year(&mut self, year: i32, holidays: Vec<Holiday>) {
        self.holidays.insert(
            year,
            holidays
                .into_iter()
                .map(|holiday| (holiday.fecha, holiday.nombre))
                .collect(),
        );
    }

    fn cache_path(&self, year: i32) -> PathBuf {
        self.cache_dir.join(format!("feriados-{year}.json"))
    }
}

async fn decode_holidays(response: reqwest::Response) -> Result<Vec<Holiday>, String> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !calendar_content_type_is_json(&content_type) {
        return Err("Content-Type del calendario no es JSON".into());
    }
    if !declared_calendar_size_is_allowed(response.content_length()) {
        return Err("respuesta de calendario demasiado grande".into());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if !streamed_calendar_size_is_allowed(bytes.len(), chunk.len()) {
            return Err("respuesta de calendario demasiado grande".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn calendar_content_type_is_json(content_type: &str) -> bool {
    content_type.contains("application/json") || content_type.contains("+json")
}

fn declared_calendar_size_is_allowed(content_length: Option<u64>) -> bool {
    content_length.is_none_or(|length| length <= MAX_HOLIDAY_RESPONSE_BYTES as u64)
}

fn streamed_calendar_size_is_allowed(current_bytes: usize, next_bytes: usize) -> bool {
    current_bytes.saturating_add(next_bytes) <= MAX_HOLIDAY_RESPONSE_BYTES
}

fn retry_window_is_active(now: Instant, retry_after: Instant) -> bool {
    now < retry_after
}

fn holidays_are_valid(year: i32, holidays: &[Holiday]) -> bool {
    let prefix = format!("{year:04}-");
    let mut dates = HashSet::new();
    !holidays.is_empty()
        && holidays.iter().all(|holiday| {
            holiday.fecha.starts_with(&prefix)
                && valid_iso_date(&holiday.fecha)
                && !holiday.nombre.trim().is_empty()
                && dates.insert(holiday.fecha.as_str())
        })
}

fn write_cache_atomic(path: &Path, holidays: &[Holiday]) -> Result<(), std::io::Error> {
    let bytes = serde_json::to_vec_pretty(holidays).map_err(std::io::Error::other)?;
    write_atomic(path, &bytes)
}

#[cfg(test)]
fn schedule_for_moment(
    moment: LocalMoment,
    entry_delay_after_open_mins: u32,
) -> MarketScheduleStatus {
    schedule_for_moment_with_hours(
        moment,
        entry_delay_after_open_mins,
        MARKET_OPEN_MINUTE,
        MARKET_CLOSE_MINUTE,
        false,
    )
}

fn schedule_for_moment_with_hours(
    moment: LocalMoment,
    entry_delay_after_open_mins: u32,
    open_minute: u16,
    close_minute: u16,
    explicitly_open_on_weekend: bool,
) -> MarketScheduleStatus {
    if moment.weekday_from_monday >= 5 && !explicitly_open_on_weekend {
        MarketScheduleStatus::closed("Fin de semana · abre el próximo día hábil a las 10:30")
    } else if moment.minute_of_day < open_minute {
        MarketScheduleStatus::closed(format!(
            "Fuera de horario · abre hoy a las {}",
            format_minute(open_minute)
        ))
    } else if moment.minute_of_day >= close_minute {
        MarketScheduleStatus::closed(format!(
            "Fuera de horario · rueda finalizada a las {}",
            format_minute(close_minute)
        ))
    } else if u32::from(moment.minute_of_day)
        < u32::from(open_minute).saturating_add(entry_delay_after_open_mins)
    {
        let entries_from = u32::from(open_minute)
            .saturating_add(entry_delay_after_open_mins)
            .min(u32::from(close_minute)) as u16;
        MarketScheduleStatus::observing(entries_from)
    } else {
        let mut status = MarketScheduleStatus::open();
        status.detail = format!(
            "Rueda argentina abierta hasta las {}",
            format_minute(close_minute)
        );
        status
    }
}

fn validate_exchange_calendar(manifest: &ExchangeCalendarManifest) -> Result<(), String> {
    if manifest.schema_version != EXCHANGE_CALENDAR_SCHEMA_VERSION {
        return Err(format!(
            "schema de calendario BYMA {} no soportado",
            manifest.schema_version
        ));
    }
    let source = reqwest::Url::parse(&manifest.source_url)
        .map_err(|_| "source_url de calendario BYMA inválida".to_string())?;
    if source.scheme() != "https" || source.host_str().is_none() {
        return Err("source_url de calendario BYMA debe usar HTTPS".into());
    }
    if !canonical_lowercase_sha256(&manifest.source_sha256) {
        return Err("source_sha256 de calendario BYMA inválido".into());
    }
    if manifest.retrieved_at_secs <= 0 {
        return Err("vigencia de calendario BYMA inválida".into());
    }
    if !valid_iso_date(&manifest.valid_from) {
        return Err("vigencia de calendario BYMA inválida".into());
    }
    if !valid_iso_date(&manifest.valid_until) {
        return Err("vigencia de calendario BYMA inválida".into());
    }
    if !inclusive_date_range_is_ordered(&manifest.valid_from, &manifest.valid_until) {
        return Err("vigencia de calendario BYMA inválida".into());
    }
    let mut dates = HashSet::new();
    for session in &manifest.sessions {
        if !valid_iso_date(&session.date)
            || !date_is_within_inclusive_range(
                &session.date,
                &manifest.valid_from,
                &manifest.valid_until,
            )
            || session.name.trim().is_empty()
            || !dates.insert(session.date.as_str())
        {
            return Err(format!(
                "sesión BYMA inválida o duplicada: {}",
                session.date
            ));
        }
        match session.status {
            ExchangeSessionKind::SpecialHours => {
                let (Some(open), Some(close)) = (
                    session.open.as_deref().and_then(parse_minute),
                    session.close.as_deref().and_then(parse_minute),
                ) else {
                    return Err(format!(
                        "sesión especial {} no tiene horario válido",
                        session.date
                    ));
                };
                if open >= close {
                    return Err(format!(
                        "sesión especial {} tiene horario invertido",
                        session.date
                    ));
                }
            }
            ExchangeSessionKind::Open
            | ExchangeSessionKind::Closed
            | ExchangeSessionKind::TradingWithoutSettlement => {
                if session.open.is_some() || session.close.is_some() {
                    return Err(format!(
                        "sólo special_hours admite horarios en {}",
                        session.date
                    ));
                }
            }
        }
    }
    Ok(())
}

fn canonical_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn inclusive_date_range_is_ordered(start: &str, end: &str) -> bool {
    start <= end
}

fn date_is_within_inclusive_range(date: &str, start: &str, end: &str) -> bool {
    start <= date && date <= end
}

fn exchange_session_minutes(session: &ExchangeSession) -> Option<(u16, u16)> {
    (session.status == ExchangeSessionKind::SpecialHours).then(|| {
        Some((
            parse_minute(session.open.as_deref()?)?,
            parse_minute(session.close.as_deref()?)?,
        ))
    })?
}

fn parse_minute(value: &str) -> Option<u16> {
    let (hour, minute) = value.split_once(':')?;
    let hour = hour.parse::<u16>().ok()?;
    let minute = minute.parse::<u16>().ok()?;
    (hour <= 23 && minute <= 59).then_some(hour * 60 + minute)
}

fn valid_iso_date(value: &str) -> bool {
    if value.len() != 10
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
    {
        return false;
    }
    let year = value[0..4].parse::<i32>().ok();
    let month = value[5..7].parse::<u8>().ok();
    let day = value[8..10].parse::<u8>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    if year < 1970 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=max_day).contains(&day)
}

fn format_minute(minute_of_day: u16) -> String {
    format!("{:02}:{:02}", minute_of_day / 60, minute_of_day % 60)
}

fn argentina_moment(timestamp_secs: i64) -> Option<LocalMoment> {
    argentina_date_parts(timestamp_secs).map(
        |(year, month, day, weekday_from_monday, minute_of_day)| LocalMoment {
            date: LocalDate { year, month, day },
            weekday_from_monday,
            minute_of_day,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn serve_once(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{address}")
    }

    fn json_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn calendar_time_and_size_constants_are_exact() {
        assert_eq!(MARKET_OPEN_MINUTE, 630);
        assert_eq!(MARKET_CLOSE_MINUTE, 1_020);
        assert_eq!(RETRY_DELAY, Duration::from_secs(300));
        assert_eq!(MAX_CALENDAR_BYTES, 4_194_304);
        assert_eq!(MAX_HOLIDAY_RESPONSE_BYTES, 524_288);

        assert!(calendar_content_type_is_json("application/json"));
        assert!(calendar_content_type_is_json(
            "application/problem+json; charset=utf-8"
        ));
        assert!(!calendar_content_type_is_json("text/plain"));

        assert!(declared_calendar_size_is_allowed(None));
        assert!(declared_calendar_size_is_allowed(Some(524_288)));
        assert!(!declared_calendar_size_is_allowed(Some(524_289)));
        assert!(streamed_calendar_size_is_allowed(524_287, 1));
        assert!(!streamed_calendar_size_is_allowed(524_288, 1));

        let now = Instant::now();
        assert!(retry_window_is_active(
            now,
            now.checked_add(Duration::from_nanos(1)).unwrap()
        ));
        assert!(!retry_window_is_active(now, now));
        assert!(!retry_window_is_active(
            now,
            now.checked_sub(Duration::from_nanos(1)).unwrap()
        ));
    }

    #[test]
    fn iso_dates_validate_real_month_lengths_and_leap_years() {
        assert!(valid_iso_date("1970-01-01"));
        assert!(valid_iso_date("2028-02-29"));
        assert!(!valid_iso_date("2100-02-29"));
        assert!(valid_iso_date("2400-02-29"));
        assert!(!valid_iso_date("2026-02-29"));
        assert!(!valid_iso_date("2026-04-31"));
        assert!(valid_iso_date("2026-12-31"));
    }

    #[test]
    fn argentina_market_is_open_only_from_1030_until_1700() {
        assert!(!schedule_for_moment(moment(2026, 8, 25, 10, 29, 1), 45).open);
        assert!(schedule_for_moment(moment(2026, 8, 25, 10, 30, 1), 45).open);
        assert!(schedule_for_moment(moment(2026, 8, 25, 16, 59, 1), 45).open);
        assert!(!schedule_for_moment(moment(2026, 8, 25, 17, 0, 1), 45).open);
    }

    #[test]
    fn opening_observation_collects_data_but_delays_entries() {
        let opening = schedule_for_moment(moment(2026, 8, 25, 10, 30, 1), 45);
        assert!(opening.open);
        assert!(!opening.entries_allowed);
        assert_eq!(opening.headline, "ONLINE · OBSERVANDO APERTURA");
        assert!(opening.detail.contains("11:15"));

        let still_observing = schedule_for_moment(moment(2026, 8, 25, 11, 14, 1), 45);
        assert!(!still_observing.entries_allowed);

        let enabled = schedule_for_moment(moment(2026, 8, 25, 11, 15, 1), 45);
        assert!(enabled.open);
        assert!(enabled.entries_allowed);
        assert_eq!(enabled.headline, "ONLINE · MERCADO ABIERTO");
    }

    #[test]
    fn weekends_are_offline() {
        let status = schedule_for_moment(moment(2026, 8, 29, 12, 0, 5), 45);
        assert!(!status.open);
        assert!(status.detail.contains("Fin de semana"));
    }

    #[test]
    fn unix_conversion_uses_buenos_aires_timezone() {
        let epoch = argentina_moment(3 * 60 * 60).unwrap();
        assert_eq!(
            epoch.date,
            LocalDate {
                year: 1970,
                month: 1,
                day: 1
            }
        );
        assert_eq!(epoch.weekday_from_monday, 3);
        assert_eq!(epoch.minute_of_day, 0);
    }

    #[tokio::test]
    async fn configured_holidays_are_offline_during_market_hours() {
        let mut calendar = test_calendar();
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-03-24".into(),
                tipo: "inamovible".into(),
                nombre: "Día Nacional de la Memoria".into(),
            }],
        );

        let status = calendar
            .status(argentina_timestamp(2026, 3, 24, 12, 0))
            .await;

        assert!(!status.open);
        assert_eq!(status.headline, "OFFLINE · MERCADO CERRADO");
        assert!(status.detail.contains("Día Nacional de la Memoria"));
    }

    #[tokio::test]
    async fn exchange_calendar_overrides_civil_holidays_and_exchange_closures() {
        let mut calendar = test_calendar();
        calendar.exchange_calendar = Some(exchange_manifest(vec![
            ExchangeSession {
                date: "2026-03-23".into(),
                status: ExchangeSessionKind::TradingWithoutSettlement,
                open: None,
                close: None,
                name: "Negociación sin liquidación".into(),
            },
            ExchangeSession {
                date: "2026-12-31".into(),
                status: ExchangeSessionKind::Closed,
                open: None,
                close: None,
                name: "Sin negociación".into(),
            },
        ]));
        calendar.require_exchange_calendar = true;
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-03-23".into(),
                tipo: "puente".into(),
                nombre: "Día no laborable civil".into(),
            }],
        );

        let trading = calendar
            .status(argentina_timestamp(2026, 3, 23, 12, 0))
            .await;
        assert!(trading.open);
        assert!(trading.detail.contains("sin liquidación"));

        let closed = calendar
            .status(argentina_timestamp(2026, 12, 31, 12, 0))
            .await;
        assert!(!closed.open);
        assert!(closed.detail.contains("Sin negociación"));
    }

    #[tokio::test]
    async fn explicit_byma_weekend_session_opens_without_the_auxiliary_civil_feed() {
        let mut calendar = test_calendar();
        calendar.weekend_risk_enabled = false;
        calendar.exchange_calendar = Some(exchange_manifest(vec![ExchangeSession {
            date: "2026-08-29".into(),
            status: ExchangeSessionKind::Open,
            open: None,
            close: None,
            name: "Rueda extraordinaria BYMA".into(),
        }]));
        calendar.require_exchange_calendar = true;

        let status = calendar
            .status(argentina_timestamp(2026, 8, 29, 12, 0))
            .await;
        assert!(status.open);
        assert!(status.entries_allowed);
        assert_eq!(status.detail, "Rueda extraordinaria BYMA");
        assert!(calendar.holidays.is_empty());
    }

    #[tokio::test]
    async fn special_exchange_hours_are_authoritative() {
        let mut calendar = test_calendar();
        calendar.exchange_calendar = Some(exchange_manifest(vec![ExchangeSession {
            date: "2026-08-25".into(),
            status: ExchangeSessionKind::SpecialHours,
            open: Some("11:00".into()),
            close: Some("14:00".into()),
            name: "Rueda reducida".into(),
        }]));
        calendar.require_exchange_calendar = true;
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-03-24".into(),
                tipo: "inamovible".into(),
                nombre: "Feriado".into(),
            }],
        );

        let before = calendar
            .status(argentina_timestamp(2026, 8, 25, 10, 59))
            .await;
        assert!(!before.open);
        let open = calendar
            .status(argentina_timestamp(2026, 8, 25, 12, 0))
            .await;
        assert!(open.open);
        assert!(open.detail.contains("11:00–14:00"));
        let after = calendar
            .status(argentina_timestamp(2026, 8, 25, 14, 0))
            .await;
        assert!(!after.open);
    }

    #[tokio::test]
    async fn live_calendar_fails_closed_without_a_covering_manifest() {
        let mut calendar = test_calendar().with_exchange_calendar(None, true).unwrap();
        let status = calendar
            .status(argentina_timestamp(2026, 8, 25, 12, 0))
            .await;
        assert!(!status.open);
        assert!(status.detail.contains("BYMA"));
    }

    #[tokio::test]
    async fn friday_blocks_entries_then_forces_exit_before_the_weekend() {
        let mut calendar = test_calendar();
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-03-24".into(),
                tipo: "inamovible".into(),
                nombre: "Día Nacional de la Memoria".into(),
            }],
        );

        let before_cutoff = calendar
            .status(argentina_timestamp(2026, 8, 21, 14, 59))
            .await;
        assert!(before_cutoff.entries_allowed);
        assert!(before_cutoff.pre_break);
        assert_eq!(before_cutoff.next_session_days, 3);

        let entry_cutoff = calendar
            .status(argentina_timestamp(2026, 8, 21, 15, 0))
            .await;
        assert!(!entry_cutoff.entries_allowed);
        assert!(!entry_cutoff.force_pre_break_exit);
        assert_eq!(entry_cutoff.headline, "ONLINE · PAUSA PRÓXIMA");

        let forced_exit = calendar
            .status(argentina_timestamp(2026, 8, 21, 16, 30))
            .await;
        assert!(forced_exit.force_pre_break_exit);
        assert_eq!(forced_exit.headline, "ONLINE · CIERRE OBLIGATORIO");
    }

    #[tokio::test]
    async fn a_holiday_eve_is_treated_like_a_friday() {
        let mut calendar = test_calendar();
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-08-28".into(),
                tipo: "inamovible".into(),
                nombre: "Feriado de prueba".into(),
            }],
        );

        let status = calendar
            .status(argentina_timestamp(2026, 8, 27, 15, 0))
            .await;

        assert!(status.pre_break);
        assert!(!status.entries_allowed);
        assert_eq!(status.next_session_days, 4);
    }

    #[tokio::test]
    async fn expiry_cutoff_is_exposed_on_regular_sessions() {
        let mut calendar = test_calendar();
        calendar.store_year(
            2026,
            vec![Holiday {
                fecha: "2026-03-24".into(),
                tipo: "inamovible".into(),
                nombre: "Día Nacional de la Memoria".into(),
            }],
        );

        let status = calendar
            .status(argentina_timestamp(2026, 8, 25, 15, 15))
            .await;

        assert!(status.expiry_exit_due);
        assert!(!status.pre_break);
    }

    #[test]
    fn replay_uses_recorded_time_and_known_session_gap_without_network() {
        let calendar = test_calendar();
        let status = calendar.replay_risk_status(argentina_timestamp(2026, 8, 21, 16, 30), 3);

        assert!(status.open);
        assert!(status.force_pre_break_exit);
        assert_eq!(status.next_session_days, 3);
    }

    #[test]
    fn lunch_regime_reduces_risk_then_requires_post_lunch_confirmation() {
        let calendar = test_calendar();

        let lunch = calendar.replay_risk_status(argentina_timestamp(2026, 8, 25, 12, 30), 1);
        assert!(lunch.lunch_slowdown);
        assert!(lunch.entries_allowed);
        assert_eq!(lunch.headline, "ONLINE · LIQUIDEZ DE MEDIODÍA");

        let recovery = calendar.replay_risk_status(argentina_timestamp(2026, 8, 25, 14, 0), 1);
        assert!(recovery.lunch_reconfirming);
        assert!(!recovery.entries_allowed);
        assert!(recovery.detail.contains("14:05"));

        let normal = calendar.replay_risk_status(argentina_timestamp(2026, 8, 25, 14, 5), 1);
        assert!(!normal.lunch_slowdown);
        assert!(!normal.lunch_reconfirming);
        assert!(normal.entries_allowed);
    }

    #[test]
    fn oversized_exchange_manifest_is_rejected_before_json_parsing() {
        let path = std::env::temp_dir().join(format!(
            "options-calendar-oversized-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CALENDAR_BYTES + 1).unwrap();
        drop(file);

        let error = match test_calendar().with_exchange_calendar(Some(&path), true) {
            Err(error) => error,
            Ok(_) => panic!("un manifiesto sobredimensionado fue aceptado"),
        };
        assert!(error.contains("mayor que"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn holiday_payload_requires_real_unique_dates_and_nonempty_names() {
        let valid = Holiday {
            fecha: "2026-03-24".into(),
            tipo: "inamovible".into(),
            nombre: "Memoria".into(),
        };
        assert!(holidays_are_valid(2026, std::slice::from_ref(&valid)));
        assert!(!holidays_are_valid(2026, &[]));

        for holiday in [
            Holiday {
                fecha: "2025-03-24".into(),
                ..valid.clone()
            },
            Holiday {
                fecha: "2026-99-99".into(),
                ..valid.clone()
            },
            Holiday {
                nombre: "  ".into(),
                ..valid.clone()
            },
        ] {
            assert!(!holidays_are_valid(2026, &[holiday]));
        }
        assert!(!holidays_are_valid(2026, &[valid.clone(), valid]));
    }

    #[test]
    fn exchange_manifest_validation_covers_every_contractual_rejection() {
        let valid_session = ExchangeSession {
            date: "2026-08-25".into(),
            status: ExchangeSessionKind::Closed,
            open: None,
            close: None,
            name: "Sin rueda".into(),
        };
        let valid = exchange_manifest(vec![valid_session.clone()]);
        assert!(validate_exchange_calendar(&valid).is_ok());
        assert!(canonical_lowercase_sha256(&"0".repeat(64)));
        assert!(!canonical_lowercase_sha256(&"A".repeat(64)));
        assert!(!canonical_lowercase_sha256(&"g".repeat(64)));
        assert!(!canonical_lowercase_sha256(&"a".repeat(63)));

        assert!(inclusive_date_range_is_ordered("2026-01-01", "2026-01-01"));
        assert!(!inclusive_date_range_is_ordered("2026-01-02", "2026-01-01"));
        assert!(date_is_within_inclusive_range(
            "2026-01-01",
            "2026-01-01",
            "2026-12-31"
        ));
        assert!(date_is_within_inclusive_range(
            "2026-12-31",
            "2026-01-01",
            "2026-12-31"
        ));
        assert!(!date_is_within_inclusive_range(
            "2025-12-31",
            "2026-01-01",
            "2026-12-31"
        ));
        assert!(!date_is_within_inclusive_range(
            "2027-01-01",
            "2026-01-01",
            "2026-12-31"
        ));

        let mutations: &[fn(&mut ExchangeCalendarManifest)] = &[
            |m| m.schema_version = 2,
            |m| m.source_url = "not-a-url".into(),
            |m| m.source_url = "http://www.byma.com.ar/calendario".into(),
            |m| m.source_sha256 = "a".repeat(63),
            |m| m.source_sha256 = "z".repeat(64),
            |m| m.source_sha256 = "A".repeat(64),
            |m| m.retrieved_at_secs = 0,
            |m| m.valid_from = "not-a-date".into(),
            |m| m.valid_until = "2026-13-01".into(),
            |m| m.valid_until = "2025-12-31".into(),
            |m| {
                m.valid_from = "2026-12-31".into();
                m.valid_until = "2026-01-01".into();
                m.sessions.clear();
            },
            |m| m.sessions[0].date = "not-a-date".into(),
            |m| m.sessions[0].date = "2027-01-01".into(),
            |m| m.sessions[0].name = " ".into(),
            |m| m.sessions.push(m.sessions[0].clone()),
            |m| {
                m.sessions[0].status = ExchangeSessionKind::SpecialHours;
                m.sessions[0].open = None;
                m.sessions[0].close = Some("14:00".into());
            },
            |m| {
                m.sessions[0].status = ExchangeSessionKind::SpecialHours;
                m.sessions[0].open = Some("14:00".into());
                m.sessions[0].close = Some("11:00".into());
            },
            |m| m.sessions[0].open = Some("11:00".into()),
            |m| m.sessions[0].close = Some("14:00".into()),
            |m| {
                m.sessions[0].status = ExchangeSessionKind::SpecialHours;
                m.sessions[0].open = Some("invalid".into());
                m.sessions[0].close = Some("14:00".into());
            },
            |m| {
                m.sessions[0].status = ExchangeSessionKind::SpecialHours;
                m.sessions[0].open = Some("11:00".into());
                m.sessions[0].close = Some("invalid".into());
            },
        ];
        for mutate in mutations {
            let mut candidate = valid.clone();
            mutate(&mut candidate);
            assert!(validate_exchange_calendar(&candidate).is_err());
        }
    }

    #[test]
    fn exchange_calendar_file_loader_accepts_only_valid_manifests() {
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("valid.json");
        let invalid_path = directory.path().join("invalid.json");
        std::fs::write(
            &valid_path,
            serde_json::to_vec(&exchange_manifest(Vec::new())).unwrap(),
        )
        .unwrap();
        std::fs::write(&invalid_path, b"not-json").unwrap();

        let loaded = test_calendar()
            .with_exchange_calendar(Some(&valid_path), true)
            .unwrap();
        assert!(loaded.exchange_calendar.is_some());
        assert!(loaded.require_exchange_calendar);
        assert!(test_calendar()
            .with_exchange_calendar(Some(&invalid_path), true)
            .is_err());
        assert!(test_calendar()
            .with_exchange_calendar(Some(&directory.path().join("missing.json")), true)
            .is_err());
    }

    #[tokio::test]
    async fn valid_holiday_cache_is_loaded_without_network() {
        let directory = tempfile::tempdir().unwrap();
        let holidays = vec![Holiday {
            fecha: "2026-03-24".into(),
            tipo: "inamovible".into(),
            nombre: "Memoria".into(),
        }];
        std::fs::write(
            directory.path().join("feriados-2026.json"),
            serde_json::to_vec(&holidays).unwrap(),
        )
        .unwrap();
        let mut calendar = MarketCalendar::new(
            "http://127.0.0.1:1",
            directory.path().to_path_buf(),
            test_policy(),
        )
        .unwrap();

        assert!(calendar.ensure_year(2026).await);
        assert_eq!(
            calendar.holiday_name(LocalDate {
                year: 2026,
                month: 3,
                day: 24
            }),
            Some("Memoria")
        );
    }

    #[tokio::test]
    async fn network_calendar_is_validated_cached_and_reused() {
        let directory = tempfile::tempdir().unwrap();
        let body = serde_json::to_string(&vec![Holiday {
            fecha: "2026-03-24".into(),
            tipo: "inamovible".into(),
            nombre: "Memoria".into(),
        }])
        .unwrap();
        let endpoint = serve_once(json_response("200 OK", &body));
        let mut calendar =
            MarketCalendar::new(endpoint, directory.path().to_path_buf(), test_policy()).unwrap();

        assert!(calendar.ensure_year(2026).await);
        assert!(calendar.cache_path(2026).is_file());
        calendar.holidays.clear();
        assert!(calendar.ensure_year(2026).await);
        assert!(!calendar.last_errors.contains_key(&2026));
    }

    #[tokio::test]
    async fn failed_calendar_fetch_is_rate_limited_and_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = serve_once(json_response("503 Service Unavailable", "{}"));
        let mut calendar =
            MarketCalendar::new(endpoint, directory.path().to_path_buf(), test_policy()).unwrap();

        assert!(!calendar.ensure_year(2026).await);
        assert!(calendar.retry_after.contains_key(&2026));
        assert!(calendar.last_errors.contains_key(&2026));
        assert!(!calendar.ensure_year(2026).await);
    }

    #[tokio::test]
    async fn corrupt_caches_are_rejected_before_a_valid_network_replacement() {
        for cached in [
            b"not-json".to_vec(),
            r#"[{"fecha":"2025-03-24","tipo":"x","nombre":"Otro año"}]"#
                .as_bytes()
                .to_vec(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("feriados-2026.json"), cached).unwrap();
            let body = r#"[{"fecha":"2026-03-24","tipo":"x","nombre":"Memoria"}]"#;
            let endpoint = serve_once(json_response("200 OK", body));
            let mut calendar =
                MarketCalendar::new(endpoint, directory.path().to_path_buf(), test_policy())
                    .unwrap();

            assert!(calendar.ensure_year(2026).await);
            assert_eq!(calendar.holidays[&2026].len(), 1);
            assert!(!calendar.last_errors.contains_key(&2026));
        }
    }

    #[tokio::test]
    async fn cache_write_failure_keeps_a_nonfatal_diagnostic() {
        let directory = tempfile::tempdir().unwrap();
        let unusable_cache_root = directory.path().join("not-a-directory");
        std::fs::write(&unusable_cache_root, b"file").unwrap();
        let body = r#"[{"fecha":"2026-03-24","tipo":"x","nombre":"Memoria"}]"#;
        let endpoint = serve_once(json_response("200 OK", body));
        let mut calendar =
            MarketCalendar::new(endpoint, unusable_cache_root, test_policy()).unwrap();

        assert!(calendar.ensure_year(2026).await);
        assert!(calendar.holidays.contains_key(&2026));
        assert!(calendar.last_errors[&2026].contains("guardar"));
    }

    #[tokio::test]
    async fn calendar_http_contract_rejects_type_size_json_and_wrong_year() {
        let oversized_chunk = "x".repeat(512 * 1024 + 1);
        let cases = [
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                512 * 1024 + 1
            ),
            json_response("200 OK", "not-json"),
            json_response(
                "200 OK",
                r#"[{"fecha":"2025-03-24","tipo":"x","nombre":"Otro año"}]"#,
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{}\r\n0\r\n\r\n",
                oversized_chunk.len(),
                oversized_chunk
            ),
        ];
        for response in cases {
            let directory = tempfile::tempdir().unwrap();
            let endpoint = serve_once(response);
            let calendar =
                MarketCalendar::new(endpoint, directory.path().to_path_buf(), test_policy())
                    .unwrap();
            assert!(calendar.fetch_year(2026).await.is_err());
        }
    }

    #[test]
    fn minute_parser_and_special_hours_are_total_over_malformed_inputs() {
        assert_eq!(parse_minute("00:00"), Some(0));
        assert_eq!(parse_minute("23:59"), Some(23 * 60 + 59));
        for invalid in ["1200", "xx:00", "12:xx", "24:00", "12:60"] {
            assert_eq!(parse_minute(invalid), None);
        }

        let regular = ExchangeSession {
            date: "2026-08-25".into(),
            status: ExchangeSessionKind::Open,
            open: None,
            close: None,
            name: "Normal".into(),
        };
        assert_eq!(exchange_session_minutes(&regular), None);
        let special = ExchangeSession {
            status: ExchangeSessionKind::SpecialHours,
            open: Some("11:00".into()),
            close: Some("14:00".into()),
            ..regular
        };
        assert_eq!(exchange_session_minutes(&special), Some((660, 840)));
    }

    #[test]
    fn iso_date_parser_rejects_each_malformed_component() {
        for invalid in [
            "20260825",
            "2026/08/25",
            "2026/08-25",
            "2026-08/25",
            "xxxx-08-25",
            "2026-xx-25",
            "2026-08-xx",
            "1969-12-31",
            "2026-00-01",
            "2026-01-00",
            "2100-02-29",
        ] {
            assert!(!valid_iso_date(invalid), "{invalid} fue aceptada");
        }
        assert!(valid_iso_date("2000-02-29"));
        assert!(valid_iso_date("2400-02-29"));
    }

    #[tokio::test]
    async fn valid_json_cache_for_another_year_is_never_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let mut calendar = MarketCalendar::new(
            "https://example.invalid",
            directory.path().to_path_buf(),
            test_policy(),
        )
        .unwrap();
        let wrong_year = vec![Holiday {
            fecha: "2025-03-24".into(),
            tipo: "inamovible".into(),
            nombre: "Año incorrecto".into(),
        }];
        write_cache_atomic(&calendar.cache_path(2026), &wrong_year).unwrap();
        calendar.retry_after.insert(
            2026,
            Instant::now().checked_add(Duration::from_secs(60)).unwrap(),
        );

        assert!(!calendar.ensure_year(2026).await);
        assert!(!calendar.holidays.contains_key(&2026));
        assert!(calendar
            .last_errors
            .get(&2026)
            .is_some_and(|error| error.contains("otro año")));
    }

    #[tokio::test]
    async fn status_fails_closed_when_civil_calendar_is_unavailable() {
        let mut calendar = test_calendar();
        calendar
            .retry_after
            .insert(2026, Instant::now() + Duration::from_secs(60));
        let generic = calendar
            .status(argentina_timestamp(2026, 8, 25, 12, 0))
            .await;
        assert!(!generic.open);
        assert_eq!(
            generic.detail,
            "No se pudo validar el calendario de feriados"
        );

        calendar.last_errors.insert(2026, "fallo controlado".into());
        let detailed = calendar
            .status(argentina_timestamp(2026, 8, 25, 12, 0))
            .await;
        assert!(!detailed.open);
        assert!(detailed.detail.contains("fallo controlado"));
    }

    #[tokio::test]
    async fn next_session_search_honors_exchange_closures_and_explicit_weekend_opening() {
        let mut calendar = test_calendar();
        calendar.exchange_calendar = Some(exchange_manifest(vec![
            ExchangeSession {
                date: "2026-08-26".into(),
                status: ExchangeSessionKind::Closed,
                open: None,
                close: None,
                name: "Cerrada".into(),
            },
            ExchangeSession {
                date: "2026-08-29".into(),
                status: ExchangeSessionKind::Open,
                open: None,
                close: None,
                name: "Rueda excepcional".into(),
            },
        ]));
        calendar.require_exchange_calendar = true;
        calendar.store_year(
            2026,
            vec![
                Holiday {
                    fecha: "2026-08-27".into(),
                    tipo: "x".into(),
                    nombre: "Feriado".into(),
                },
                Holiday {
                    fecha: "2026-08-28".into(),
                    tipo: "x".into(),
                    nombre: "Feriado adicional".into(),
                },
            ],
        );

        assert_eq!(
            calendar
                .days_until_next_session(argentina_timestamp(2026, 8, 25, 12, 0))
                .await,
            Some(4)
        );
    }

    #[tokio::test]
    async fn required_manifest_gap_aborts_next_session_search() {
        let mut calendar = test_calendar();
        calendar.exchange_calendar = Some(exchange_manifest(Vec::new()));
        calendar.require_exchange_calendar = true;
        calendar.exchange_calendar.as_mut().unwrap().valid_until = "2026-08-25".into();
        assert_eq!(
            calendar
                .days_until_next_session(argentina_timestamp(2026, 8, 25, 12, 0))
                .await,
            None
        );
    }

    #[test]
    fn extreme_timestamps_fail_closed_instead_of_inventing_a_civil_date() {
        assert!(argentina_moment(i64::MIN).is_none());
        assert!(argentina_moment(i64::MAX).is_none());
        let calendar = test_calendar();
        assert!(!calendar.replay_risk_status(i64::MIN, 1).open);
    }

    #[test]
    fn lunch_never_overrides_a_higher_priority_weekend_block() {
        let mut calendar = test_calendar();
        calendar.lunch_slowdown_start_minute = 15 * 60;
        calendar.lunch_slowdown_end_minute = 16 * 60;
        calendar.post_lunch_confirmation_mins = 60;

        let lunch = calendar.replay_risk_status(argentina_timestamp(2026, 8, 21, 15, 0), 3);
        assert!(lunch.lunch_slowdown);
        assert_eq!(lunch.headline, "ONLINE · PAUSA PRÓXIMA");

        let recovery = calendar.replay_risk_status(argentina_timestamp(2026, 8, 21, 16, 30), 3);
        assert!(recovery.lunch_reconfirming);
        assert_eq!(recovery.headline, "ONLINE · CIERRE OBLIGATORIO");
    }

    #[test]
    fn disabled_risk_policies_leave_replay_entries_unchanged() {
        let mut calendar = test_calendar();
        calendar.weekend_risk_enabled = false;
        calendar.lunch_slowdown_enabled = false;
        let status = calendar.replay_risk_status(argentina_timestamp(2026, 8, 21, 16, 30), 3);
        assert!(status.entries_allowed);
        assert!(!status.force_pre_break_exit);
        assert!(!status.lunch_slowdown);
    }

    fn test_calendar() -> MarketCalendar {
        MarketCalendar::new(
            "https://example.invalid",
            std::env::temp_dir().join("unused-market-calendar-test"),
            test_policy(),
        )
        .unwrap()
    }

    fn test_policy() -> MarketCalendarPolicy {
        MarketCalendarPolicy {
            entry_delay_after_open_mins: 45,
            weekend_risk_enabled: true,
            pre_break_last_entry_minute: 15 * 60,
            pre_break_force_exit_minute: 16 * 60 + 30,
            expiry_day_force_exit_minute: 15 * 60 + 15,
            lunch_slowdown_enabled: true,
            lunch_slowdown_start_minute: 12 * 60 + 30,
            lunch_slowdown_end_minute: 14 * 60,
            post_lunch_confirmation_mins: 5,
            lunch_position_factor: 0.5,
        }
    }

    fn exchange_manifest(sessions: Vec<ExchangeSession>) -> ExchangeCalendarManifest {
        ExchangeCalendarManifest {
            schema_version: EXCHANGE_CALENDAR_SCHEMA_VERSION,
            source_url: "https://www.byma.com.ar/mercado/calendario-bursatil".into(),
            source_sha256: "a".repeat(64),
            retrieved_at_secs: 1_787_673_600,
            valid_from: "2026-01-01".into(),
            valid_until: "2026-12-31".into(),
            sessions,
        }
    }

    fn moment(
        year: i32,
        month: u8,
        day: u8,
        hour: u16,
        minute: u16,
        weekday_from_monday: u8,
    ) -> LocalMoment {
        LocalMoment {
            date: LocalDate { year, month, day },
            weekday_from_monday,
            minute_of_day: hour * 60 + minute,
        }
    }

    fn argentina_timestamp(year: i32, month: u8, day: u8, hour: u16, minute: u16) -> i64 {
        let adjusted_year = i64::from(year) - i64::from(month <= 2);
        let era = adjusted_year.div_euclid(400);
        let year_of_era = adjusted_year - era * 400;
        let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
        let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        days * 86_400 + i64::from(hour * 60 + minute) * 60 + 10_800
    }
}
