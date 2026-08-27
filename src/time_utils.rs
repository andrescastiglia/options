use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::America::Argentina::Buenos_Aires;

pub fn argentina_datetime(timestamp_secs: i64) -> Option<DateTime<chrono_tz::Tz>> {
    DateTime::<Utc>::from_timestamp(timestamp_secs, 0)
        .map(|timestamp| timestamp.with_timezone(&Buenos_Aires))
}

pub fn argentina_session_day(timestamp_secs: i64) -> i64 {
    let Some(local) = argentina_datetime(timestamp_secs) else {
        return timestamp_secs
            .saturating_sub(3 * 60 * 60)
            .div_euclid(86_400);
    };
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("fecha Unix válida");
    local.date_naive().signed_duration_since(epoch).num_days()
}

pub fn argentina_hms(timestamp_secs: i64) -> (u32, u32, u32) {
    argentina_datetime(timestamp_secs)
        .map(|local| (local.hour(), local.minute(), local.second()))
        .unwrap_or_else(|| {
            let seconds = timestamp_secs
                .saturating_sub(3 * 60 * 60)
                .rem_euclid(86_400) as u32;
            (seconds / 3_600, seconds % 3_600 / 60, seconds % 60)
        })
}

pub fn argentina_date_parts(timestamp_secs: i64) -> Option<(i32, u8, u8, u8, u16)> {
    argentina_datetime(timestamp_secs).map(|local| {
        (
            local.year(),
            local.month() as u8,
            local.day() as u8,
            local.weekday().num_days_from_monday() as u8,
            (local.hour() * 60 + local.minute()) as u16,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buenos_aires_timezone_converts_epoch_and_modern_dates() {
        assert_eq!(argentina_hms(3 * 3_600), (0, 0, 0));
        assert_eq!(argentina_session_day(3 * 3_600), 0);
        assert_eq!(argentina_hms(1_787_326_245), (12, 30, 45));
    }
}
