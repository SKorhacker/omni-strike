//! Shared HTTP plumbing: explicit timeouts, neutral User-Agent (no personal data),
//! typed rate-limit and status errors.

use crate::{ConnectorError, Result};
use oa_core::domain::PlatformId;
use std::time::Duration;

pub(crate) const USER_AGENT: &str = concat!("omni-strike/", env!("CARGO_PKG_VERSION"));
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Common settings: explicit timeouts, compressed responses (venue catalogs are large JSON), and warm
/// connections kept alive between polls instead of a new TCP + TLS handshake each time.
fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
}

pub(crate) fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("static reqwest client configuration is valid")
}

/// Same client with extra default headers (e.g. OKX demo `x-simulated-trading: 1`).
pub(crate) fn client_with_headers(headers: reqwest::header::HeaderMap) -> reqwest::Client {
    builder()
        .default_headers(headers)
        .build()
        .expect("static reqwest client configuration is valid")
}

/// "Equal jitter" for reconnect backoff: between d/2 and d, so a fleet of replicas (or many streams
/// in one process) does not reconnect in lockstep after a venue outage.
pub(crate) fn jittered(d: Duration) -> Duration {
    let half = d / 2;
    let span = half.as_millis().max(1) as u64;
    let noise = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    half + Duration::from_millis(noise % span)
}

fn map_send_err(platform: PlatformId, e: reqwest::Error) -> ConnectorError {
    if e.is_timeout() {
        ConnectorError::Timeout {
            platform,
            after: REQUEST_TIMEOUT,
        }
    } else {
        ConnectorError::Transport {
            platform,
            source: Box::new(e),
        }
    }
}

/// `Retry-After` in seconds (standard) or Aevo's `X-RETRY-AFTER` in nanoseconds.
fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ns) = h
        .get("x-retry-after")
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
    {
        return Some(Duration::from_nanos(ns));
    }
    h.get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// `GET url?query`, then `read_json`.
pub(crate) async fn get_json<U: reqwest::IntoUrl>(
    http: &reqwest::Client,
    platform: PlatformId,
    url: U,
    query: &[(&str, &str)],
) -> Result<serde_json::Value> {
    read_json(platform, http.get(url).query(query).send().await).await
}

pub(crate) async fn read_json(
    platform: PlatformId,
    resp: std::result::Result<reqwest::Response, reqwest::Error>,
) -> Result<serde_json::Value> {
    let resp = resp.map_err(|e| map_send_err(platform, e))?;
    let status = resp.status();
    if status.as_u16() == 429 {
        return Err(ConnectorError::RateLimited {
            platform,
            retry_after: retry_after(resp.headers()),
        });
    }
    let body = resp.bytes().await.map_err(|e| map_send_err(platform, e))?;
    if !status.is_success() {
        // Char-boundary-safe truncation (a byte index could split a UTF-8 character).
        let body: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        return Err(ConnectorError::Http {
            platform,
            status: status.as_u16(),
            body,
        });
    }
    // Parsed straight from the bytes (no intermediate UTF-8 String copy of large catalogs).
    serde_json::from_slice(&body).map_err(|e| crate::invalid(platform, format!("not JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_between_half_and_full_backoff() {
        for _ in 0..200 {
            let j = jittered(Duration::from_millis(1000));
            assert!(
                j >= Duration::from_millis(500) && j <= Duration::from_millis(1000),
                "{j:?}"
            );
        }
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One-shot local HTTP server returning a canned response. No external network.
    async fn serve(raw: String) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await;
            s.write_all(raw.as_bytes()).await.unwrap();
            let _ = s.shutdown().await;
        });
        format!("http://{addr}/")
    }

    fn resp(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}
{headers}content-length: {}
connection: close

{body}",
            body.len()
        )
    }

    async fn call(raw: String) -> Result<serde_json::Value> {
        let url = serve(raw).await;
        read_json(PlatformId::Aevo, client().get(url).send().await).await
    }

    #[tokio::test]
    async fn rate_limit_is_typed_with_ns_retry_after() {
        let r = call(resp(
            "429 Too Many Requests",
            "x-retry-after: 1500000000
",
            "{}",
        ))
        .await;
        match r {
            Err(ConnectorError::RateLimited { retry_after, .. }) => {
                assert_eq!(retry_after, Some(Duration::from_millis(1500)))
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_body_truncation_is_utf8_safe() {
        let body = "é".repeat(400); // 800 bytes, every char is 2 bytes
        match call(resp("500 Internal Server Error", "", &body)).await {
            Err(ConnectorError::Http {
                status: 500, body, ..
            }) => assert_eq!(body.chars().count(), 300),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_success_is_invalid_response_not_empty_success() {
        let r = call(resp("200 OK", "", "<html>maintenance</html>")).await;
        assert!(matches!(r, Err(ConnectorError::InvalidResponse { .. })));
    }

    #[tokio::test]
    async fn connection_refused_is_transport_error() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let r = read_json(
            PlatformId::Aevo,
            client().get(format!("http://{addr}/")).send().await,
        )
        .await;
        assert!(matches!(r, Err(ConnectorError::Transport { .. })));
    }
}
