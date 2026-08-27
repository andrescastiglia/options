use std::time::{Duration, Instant};

use chrono::DateTime;
use reqwest::{
    header::{CACHE_CONTROL, DATE},
    Client,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockObservation {
    pub source: String,
    pub source_timestamp_secs: i64,
    pub observed_at_secs: i64,
    pub absolute_skew_secs: u64,
}

#[derive(Debug, Error)]
pub enum ClockReferenceError {
    #[error("error HTTP de la referencia horaria: {0}")]
    Http(#[from] reqwest::Error),
    #[error("la referencia horaria no informó un encabezado Date válido")]
    MissingDate,
    #[error("desvío del reloj local: {observed}s; máximo {maximum}s")]
    ExcessiveSkew { observed: u64, maximum: u64 },
}

#[derive(Debug)]
pub struct ClockReferenceClient {
    http: Client,
    url: String,
    refresh: Duration,
    maximum_skew_secs: u64,
    next_refresh: Instant,
    last_observation: Option<ClockObservation>,
    last_verified_at: Option<Instant>,
}

impl ClockReferenceClient {
    pub fn new(
        url: impl Into<String>,
        refresh_secs: u64,
        maximum_skew_secs: u64,
    ) -> Result<Self, ClockReferenceError> {
        Ok(Self {
            http: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("options-trading/0.1 clock-check")
                .build()?,
            url: url.into(),
            refresh: Duration::from_secs(refresh_secs),
            maximum_skew_secs,
            next_refresh: Instant::now(),
            last_observation: None,
            last_verified_at: None,
        })
    }

    pub async fn verify(
        &mut self,
        local_timestamp_secs: i64,
    ) -> Result<ClockObservation, ClockReferenceError> {
        let now = Instant::now();
        if cache_window_open(now, self.next_refresh) {
            if let (Some(observation), Some(verified_at)) =
                (&self.last_observation, self.last_verified_at)
            {
                let expected_reference = observation
                    .source_timestamp_secs
                    .saturating_add(now.saturating_duration_since(verified_at).as_secs() as i64);
                let skew = expected_reference.abs_diff(local_timestamp_secs);
                if !skew_is_acceptable(skew, self.maximum_skew_secs) {
                    self.last_observation = None;
                    self.last_verified_at = None;
                    self.next_refresh = now;
                    return Err(ClockReferenceError::ExcessiveSkew {
                        observed: skew,
                        maximum: self.maximum_skew_secs,
                    });
                }
                let mut cached = observation.clone();
                cached.source_timestamp_secs = expected_reference;
                cached.observed_at_secs = local_timestamp_secs;
                cached.absolute_skew_secs = skew;
                return Ok(cached);
            }
        }
        let response = self
            .http
            .get(&self.url)
            .header(CACHE_CONTROL, "no-cache, no-store")
            .send()
            .await?
            .error_for_status()?;
        let date = response
            .headers()
            .get(DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
            .map(|value| value.timestamp())
            .ok_or(ClockReferenceError::MissingDate)?;
        let skew = date.abs_diff(local_timestamp_secs);
        if !skew_is_acceptable(skew, self.maximum_skew_secs) {
            self.last_observation = None;
            self.last_verified_at = None;
            self.next_refresh = now;
            return Err(ClockReferenceError::ExcessiveSkew {
                observed: skew,
                maximum: self.maximum_skew_secs,
            });
        }
        let observation = ClockObservation {
            source: self.url.clone(),
            source_timestamp_secs: date,
            observed_at_secs: local_timestamp_secs,
            absolute_skew_secs: skew,
        };
        self.last_observation = Some(observation.clone());
        self.last_verified_at = Some(now);
        self.next_refresh = now + self.refresh;
        Ok(observation)
    }
}

fn cache_window_open(now: Instant, next_refresh: Instant) -> bool {
    now < next_refresh
}

fn skew_is_acceptable(skew_secs: u64, maximum_skew_secs: u64) -> bool {
    skew_secs <= maximum_skew_secs
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    fn serve_dates(dates: &[String]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let dates = dates.to_vec();
        thread::spawn(move || {
            for date in dates {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2_048];
                let _ = stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nDate: {date}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        format!("http://{address}/clock")
    }

    fn serve(date: &str) -> String {
        serve_dates(&[date.to_string()])
    }

    #[test]
    fn cache_and_skew_boundaries_are_exact() {
        let boundary = Instant::now();
        assert!(cache_window_open(
            boundary - Duration::from_nanos(1),
            boundary,
        ));
        assert!(!cache_window_open(boundary, boundary));

        assert!(skew_is_acceptable(30, 30));
        assert!(!skew_is_acceptable(31, 30));
    }

    #[tokio::test]
    async fn independent_clock_has_a_closed_skew_boundary() {
        let timestamp = 1_787_673_600;
        let date = chrono::DateTime::from_timestamp(timestamp + 30, 0)
            .unwrap()
            .to_rfc2822();
        let mut client = ClockReferenceClient::new(serve(&date), 300, 30).unwrap();
        let observation = client.verify(timestamp).await.unwrap();
        assert_eq!(observation.absolute_skew_secs, 30);

        let cached = client.verify(timestamp + 30).await.unwrap();
        assert_eq!(cached.absolute_skew_secs, 0);

        assert!(matches!(
            client.verify(timestamp + 61).await,
            Err(ClockReferenceError::ExcessiveSkew { observed: 31, .. })
        ));

        let date = chrono::DateTime::from_timestamp(timestamp + 31, 0)
            .unwrap()
            .to_rfc2822();
        let mut client = ClockReferenceClient::new(serve(&date), 300, 30).unwrap();
        assert!(matches!(
            client.verify(timestamp).await,
            Err(ClockReferenceError::ExcessiveSkew { observed: 31, .. })
        ));
    }

    #[tokio::test]
    async fn a_bad_observation_is_not_cached_and_a_valid_retry_recovers() {
        let timestamp = 1_787_673_600;
        let invalid = chrono::DateTime::from_timestamp(timestamp + 31, 0)
            .unwrap()
            .to_rfc2822();
        let valid = chrono::DateTime::from_timestamp(timestamp + 30, 0)
            .unwrap()
            .to_rfc2822();
        let mut client =
            ClockReferenceClient::new(serve_dates(&[invalid, valid]), 300, 30).unwrap();

        assert!(matches!(
            client.verify(timestamp).await,
            Err(ClockReferenceError::ExcessiveSkew { observed: 31, .. })
        ));
        let recovered = client.verify(timestamp).await.unwrap();
        assert_eq!(recovered.absolute_skew_secs, 30);
        assert_eq!(recovered.source_timestamp_secs, timestamp + 30);
    }

    #[tokio::test]
    async fn a_cache_window_without_an_observation_fetches_the_reference() {
        let timestamp = 1_787_673_600;
        let date = chrono::DateTime::from_timestamp(timestamp, 0)
            .unwrap()
            .to_rfc2822();
        let mut client = ClockReferenceClient::new(serve(&date), 300, 30).unwrap();
        client.next_refresh = Instant::now() + Duration::from_secs(60);

        let observation = client.verify(timestamp).await.unwrap();
        assert_eq!(observation.source_timestamp_secs, timestamp);
        assert_eq!(observation.absolute_skew_secs, 0);
    }
}
