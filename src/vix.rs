use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::{header, Client};
use serde::Deserialize;
use thiserror::Error;

use crate::market::VixObservation;

#[derive(Debug, Error)]
pub enum VixClientError {
    #[error("error HTTP al consultar VIX: {0}")]
    Http(#[from] reqwest::Error),
    #[error("respuesta VIX invalida: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Deserialize)]
struct VixResponse {
    level: f64,
    previous_close: f64,
    timestamp_secs: i64,
    #[serde(default)]
    previous_close_timestamp_secs: Option<i64>,
    #[serde(default)]
    value_kind: crate::market::VixValueKind,
}

/// Cliente para un adaptador HTTP controlado por el operador.
///
/// El endpoint debe responder JSON con `level`, `previous_close` y
/// `timestamp_secs`. De esta forma el motor no depende de scraping ni de un
/// proveedor comercial concreto y el contrato se puede probar localmente.
pub struct VixClient {
    http: Client,
    url: String,
    refresh_interval: Duration,
    max_age_secs: u64,
    previous_close_max_age_secs: u64,
    cached: Option<(VixObservation, Instant)>,
}

impl VixClient {
    pub fn new(
        url: impl Into<String>,
        refresh_interval_secs: u64,
        max_age_secs: u64,
        previous_close_max_age_secs: u64,
        bearer_token: Option<&str>,
    ) -> Result<Self, VixClientError> {
        let mut headers = header::HeaderMap::new();
        if let Some(token) = bearer_token.filter(|token| !token.trim().is_empty()) {
            let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| VixClientError::InvalidResponse("token HTTP invalido".into()))?;
            value.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, value);
        }
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .user_agent("options-trading/0.1")
            .build()?;
        Ok(Self {
            http,
            url: url.into(),
            refresh_interval: Duration::from_secs(refresh_interval_secs.max(1)),
            max_age_secs,
            previous_close_max_age_secs,
            cached: None,
        })
    }

    pub async fn observation(
        &mut self,
        market_timestamp_secs: i64,
    ) -> Result<VixObservation, VixClientError> {
        if let Some((observation, fetched_at)) = self.cached {
            if cache_is_usable(
                observation,
                fetched_at.elapsed(),
                self.refresh_interval,
                market_timestamp_secs,
                self.max_age_secs,
                self.previous_close_max_age_secs,
            ) {
                return Ok(observation);
            }
        }
        let response = self.http.get(&self.url).send().await?.error_for_status()?;
        let response = decode_vix_response(response).await?;
        let mut observation = VixObservation {
            level: response.level,
            previous_close: Some(response.previous_close),
            timestamp_secs: response.timestamp_secs,
            previous_close_timestamp_secs: response.previous_close_timestamp_secs,
            value_kind: response.value_kind,
        };
        if observation.freshness_state(
            market_timestamp_secs,
            self.max_age_secs,
            self.previous_close_max_age_secs,
        ) == crate::market::VixFreshnessState::Stale
        {
            return Err(VixClientError::InvalidResponse(
                "observación VIX desactualizada para su tipo".into(),
            ));
        }
        if observation
            .validate_previous_close(market_timestamp_secs, self.previous_close_max_age_secs)
            .is_err()
        {
            observation.previous_close = None;
            observation.previous_close_timestamp_secs = None;
        }
        self.cached = Some((observation, Instant::now()));
        Ok(observation)
    }
}

fn cache_is_usable(
    observation: VixObservation,
    elapsed: Duration,
    refresh_interval: Duration,
    market_timestamp_secs: i64,
    max_age_secs: u64,
    previous_close_max_age_secs: u64,
) -> bool {
    elapsed < refresh_interval
        && observation.freshness_state(
            market_timestamp_secs,
            max_age_secs,
            previous_close_max_age_secs,
        ) != crate::market::VixFreshnessState::Stale
}

async fn decode_vix_response(response: reqwest::Response) -> Result<VixResponse, VixClientError> {
    const MAX_VIX_BYTES: usize = 64 * 1024;
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.contains("application/json") && !content_type.contains("+json") {
        return Err(VixClientError::InvalidResponse(
            "Content-Type no es JSON".into(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VIX_BYTES as u64)
    {
        return Err(VixClientError::InvalidResponse(
            "respuesta VIX demasiado grande".into(),
        ));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_VIX_BYTES {
            return Err(VixClientError::InvalidResponse(
                "respuesta VIX demasiado grande".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| VixClientError::InvalidResponse(format!("JSON inválido: {error}")))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use super::*;

    fn raw_server(
        response: String,
        inspect: impl FnOnce(&str) + Send + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4_096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            inspect(&request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}/vix"), handle)
    }

    fn sequence_server(responses: Vec<String>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 2_048];
                let read = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..read]).contains("GET /vix"));
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}/vix"), handle)
    }

    fn json_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn server(content_type: &'static str, body: &'static str) -> (String, thread::JoinHandle<()>) {
        raw_server(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
            |request| assert!(request.contains("GET /vix")),
        )
    }

    #[tokio::test]
    async fn accepts_current_vix_and_independently_validates_previous_close() {
        let body = r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1000,"previous_close_timestamp_secs":900,"value_kind":"current"}"#;
        let (url, server) = server("application/json", body);
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();

        let observation = client.observation(1_100).await.unwrap();

        assert_eq!(observation.level, 22.0);
        assert_eq!(
            observation.validated_change_percentage(1_100, 300),
            Some(10.0)
        );
        assert_eq!(client.observation(1_100).await.unwrap(), observation);
        server.join().unwrap();
    }

    #[test]
    fn cache_refresh_interval_has_an_exact_expired_boundary() {
        let observation = VixObservation {
            level: 22.0,
            previous_close: Some(20.0),
            timestamp_secs: 1_000,
            previous_close_timestamp_secs: Some(900),
            value_kind: crate::market::VixValueKind::Current,
        };
        let ttl = Duration::from_secs(60);

        assert!(cache_is_usable(
            observation,
            ttl - Duration::from_nanos(1),
            ttl,
            1_100,
            200,
            300,
        ));
        assert!(!cache_is_usable(observation, ttl, ttl, 1_100, 200, 300,));
    }

    #[tokio::test]
    async fn keeps_current_level_but_drops_a_stale_previous_close() {
        let body = r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1000,"previous_close_timestamp_secs":100,"value_kind":"current"}"#;
        let (url, server) = server("application/json", body);
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();

        let observation = client.observation(1_100).await.unwrap();

        assert_eq!(observation.level, 22.0);
        assert_eq!(observation.previous_close, None);
        assert_eq!(observation.change_percentage(), None);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_non_json_content_type() {
        let body = r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1000}"#;
        let (url, server) = server("text/plain", body);
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();

        assert!(client.observation(1_100).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_stale_future_invalid_and_malformed_observations() {
        for body in [
            r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":899,"value_kind":"current"}"#,
            r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1401,"value_kind":"current"}"#,
            r#"{"level":0.0,"previous_close":20.0,"timestamp_secs":1000,"value_kind":"current"}"#,
            r#"{"level":"not-a-number"}"#,
        ] {
            let (url, server) = server("application/json", body);
            let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();
            assert!(
                client.observation(1_100).await.is_err(),
                "body aceptado: {body}"
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn bearer_token_is_sent_but_never_required_in_the_url() {
        let body = r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1000,"previous_close_timestamp_secs":900,"value_kind":"current"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, server) = raw_server(response, |request| {
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer operator-secret"));
            assert!(!request.lines().next().unwrap().contains("operator-secret"));
        });
        let mut client = VixClient::new(url, 60, 200, 300, Some("operator-secret")).unwrap();
        assert!(client.observation(1_100).await.is_ok());
        server.join().unwrap();

        assert!(VixClient::new(
            "http://127.0.0.1/vix",
            60,
            200,
            300,
            Some("invalid\nheader")
        )
        .is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_declared_and_streamed_bodies() {
        let declared = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 65537\r\nConnection: close\r\n\r\n".to_string();
        let (url, server) = raw_server(declared, |_| {});
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();
        assert!(client.observation(1_100).await.is_err());
        server.join().unwrap();

        let oversized = "x".repeat(65 * 1024);
        let streamed = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{}\r\n0\r\n\r\n",
            oversized.len(),
            oversized
        );
        let (url, server) = raw_server(streamed, |_| {});
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();
        assert!(client.observation(1_100).await.is_err());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn accepts_a_valid_body_exactly_at_the_size_limit() {
        let json = r#"{"level":22.0,"previous_close":20.0,"timestamp_secs":1000,"previous_close_timestamp_secs":900,"value_kind":"current"}"#;
        let body = format!("{json}{}", " ".repeat(64 * 1024 - json.len()));
        assert_eq!(body.len(), 64 * 1024);
        let response = json_response(&body);
        let (url, server) = raw_server(response, |_| {});
        let mut client = VixClient::new(url, 60, 200, 300, None).unwrap();

        assert_eq!(client.observation(1_100).await.unwrap().level, 22.0);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn stale_cache_refetches_and_http_errors_are_not_cached() {
        let first = r#"{"level":20.0,"previous_close":19.0,"timestamp_secs":1000,"previous_close_timestamp_secs":900,"value_kind":"current"}"#;
        let second = r#"{"level":23.0,"previous_close":20.0,"timestamp_secs":1300,"previous_close_timestamp_secs":1200,"value_kind":"current"}"#;
        let (url, server) = sequence_server(vec![json_response(first), json_response(second)]);
        let mut client = VixClient::new(url, 600, 200, 300, None).unwrap();
        assert_eq!(client.observation(1_100).await.unwrap().level, 20.0);
        assert_eq!(client.observation(1_300).await.unwrap().level, 23.0);
        server.join().unwrap();

        let response = "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".to_string();
        let (url, server) = raw_server(response, |_| {});
        let mut client = VixClient::new(url, 60, 200, 300, Some("   ")).unwrap();
        assert!(client.observation(1_100).await.is_err());
        server.join().unwrap();
    }
}
