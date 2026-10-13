//! Signed webhooks to integrators (execution reports, strategy outcomes).
//!
//! Request: `POST <webhook_url>` with JSON body `{"type", "id", "created_at", "data"}` and headers
//! `X-OmniStrike-Event`, `X-OmniStrike-Timestamp` (unix s), `X-OmniStrike-Signature` =
//! `v1=` + hex(HMAC-SHA256(webhook_secret, "<timestamp>.<body>")). Integrators verify the signature and
//! reject timestamps older than 5 minutes (replay protection). Delivery: up to 4 attempts with
//! exponential backoff (1 s, 4 s, 16 s); any 2xx = delivered. Never blocks the API response.

use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;

use crate::metrics::Metrics;

pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// Address a webhook may be delivered to: public unicast only (no loopback, private, link-local,
/// CGNAT, unique-local, multicast or unspecified) — the server must not be usable as a proxy into
/// its own network.
fn public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_unspecified()
                || v.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        std::net::IpAddr::V6(v) => {
            let seg = v.segments()[0];
            if let Some(m) = v.to_ipv4_mapped() {
                return public_ip(m.into());
            }
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (seg & 0xfe00) == 0xfc00
                || (seg & 0xffc0) == 0xfe80)
        }
    }
}

/// Validate a webhook URL: `https://` to a host name or public IP, no credentials, <= 2048 chars.
/// `allow_local` (OA_WEBHOOK_ALLOW_LOCAL=true, development only) also accepts `http://` to a
/// loopback host.
pub fn validate_url(u: &str, allow_local: bool) -> Result<(), String> {
    let err = || {
        "webhook_url must be https:// to a public host, without credentials, <= 2048 chars"
            .to_string()
    };
    if u.len() > 2048 {
        return Err(err());
    }
    let url = reqwest::Url::parse(u).map_err(|_| err())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(err());
    }
    let host = url.host_str().ok_or_else(err)?;
    let ip: Option<std::net::IpAddr> = host.trim_matches(['[', ']']).parse().ok();
    let loopback = ip.map_or(host.eq_ignore_ascii_case("localhost"), |i| i.is_loopback());
    let literal_ok = ip.map_or(!loopback, public_ip);
    match url.scheme() {
        "https" if literal_ok => Ok(()),
        "http" if allow_local && loopback => Ok(()),
        _ => Err(err()),
    }
}

/// OA_WEBHOOK_ALLOW_LOCAL=true: development hosts (http://localhost) accepted.
pub fn allow_local() -> bool {
    std::env::var("OA_WEBHOOK_ALLOW_LOCAL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("true"))
}

/// Resolves the host once, refuses non-public addresses (except an http loopback URL, which only
/// passes validation in development), and pins the connection to the checked address so DNS
/// rebinding cannot swap it. Redirects are never followed.
async fn pinned_client(url: &str) -> Result<reqwest::Client, String> {
    let u = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    let host = u.host_str().ok_or("no host")?.to_string();
    let port = u.port_or_known_default().ok_or("no port")?;
    let addrs: Vec<std::net::SocketAddr> =
        tokio::net::lookup_host((host.trim_matches(['[', ']']), port))
            .await
            .map_err(|e| format!("resolve: {e}"))?
            .collect();
    let dev_loopback = u.scheme() == "http" && addrs.iter().all(|a| a.ip().is_loopback());
    let addr = addrs
        .into_iter()
        .find(|a| dev_loopback || public_ip(a.ip()))
        .ok_or("webhook host resolves to a non-public address")?;
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, addr)
        .build()
        .map_err(|e| e.to_string())
}

pub fn new_secret() -> String {
    format!(
        "whsec_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Fire-and-forget delivery with retries (spawned; never awaited by the API handler).
pub fn deliver(url: String, secret: String, event: &str, data: Value, metrics: Arc<Metrics>) {
    let event = event.to_string();
    tokio::spawn(async move {
        let body = json!({"type": event, "id": uuid::Uuid::new_v4(), "created_at": Utc::now(), "data": data}).to_string();
        let client = match pinned_client(&url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "webhook refused");
                metrics.incr("webhooks_failed");
                return;
            }
        };
        for (attempt, wait) in [0u64, 1, 4, 16].into_iter().enumerate() {
            if wait > 0 {
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            let ts = Utc::now().timestamp();
            let r = client
                .post(&url)
                .header("content-type", "application/json")
                .header("X-OmniStrike-Event", &event)
                .header("X-OmniStrike-Timestamp", ts.to_string())
                .header("X-OmniStrike-Signature", sign(&secret, ts, &body))
                .body(body.clone())
                .send()
                .await;
            match r {
                Ok(resp) if resp.status().is_success() => {
                    metrics.incr("webhooks_delivered");
                    return;
                }
                Ok(resp) => {
                    tracing::warn!(attempt, status = %resp.status(), "webhook rejected by the integrator")
                }
                Err(e) => tracing::warn!(attempt, error = %e, "webhook delivery failed"),
            }
        }
        metrics.incr("webhooks_failed");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_an_independent_hmac() {
        // python3 -c "import hmac,hashlib;print(hmac.new(b'whsec_test', b'1791300000.{\"a\":1}', hashlib.sha256).hexdigest())"
        assert_eq!(
            sign("whsec_test", 1_791_300_000, "{\"a\":1}"),
            "v1=5932a440433e064a555c1fb4432732b7b6bafb7a9fe2c00d134944288c70fef3"
        );
    }

    #[test]
    fn urls_must_be_https_to_public_hosts() {
        assert!(validate_url("https://hooks.acme.io/omni", false).is_ok());
        assert!(validate_url("https://93.184.216.34/hook", false).is_ok());
        for bad in [
            "http://acme.io/hook",
            "ftp://x",
            "http://localhost:3000/hook",
            "http://localhost@169.254.169.254/latest",
            "https://user:pw@hooks.acme.io/",
            "https://10.0.0.5/hook",
            "https://169.254.169.254/latest",
            "https://[::1]/hook",
            "https://100.64.1.1/hook",
        ] {
            assert!(validate_url(bad, false).is_err(), "{bad}");
        }
        assert!(validate_url("http://localhost:3000/hook", true).is_ok());
        assert!(validate_url("http://localhost.evil.com/hook", true).is_err());
    }

    #[tokio::test]
    async fn delivery_refuses_private_targets() {
        assert!(pinned_client("https://10.1.2.3/hook").await.is_err());
        assert!(pinned_client("https://127.0.0.1/hook").await.is_err());
        assert!(
            pinned_client("http://127.0.0.1:9/hook").await.is_ok(),
            "dev loopback"
        );
    }

    #[tokio::test]
    async fn delivery_is_signed_and_retried_until_2xx() {
        use std::sync::Mutex;
        let got: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (g, c) = (got.clone(), calls.clone());
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let (g, c) = (g.clone(), c.clone());
                async move {
                    // First attempt fails, second succeeds.
                    if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    }
                    let h = |k: &str| headers.get(k).unwrap().to_str().unwrap().to_string();
                    g.lock().unwrap().push((
                        h("x-omnistrike-timestamp"),
                        h("x-omnistrike-signature"),
                        body,
                    ));
                    axum::http::StatusCode::OK
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let m = Arc::new(Metrics::default());
        deliver(
            format!("http://{addr}/hook"),
            "whsec_k".into(),
            "execution.report",
            json!({"x": 1}),
            m.clone(),
        );
        for _ in 0..40 {
            if !got.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let g = got.lock().unwrap();
        assert_eq!(g.len(), 1, "delivered on the retry");
        let (ts, sig, body) = &g[0];
        assert_eq!(
            *sig,
            sign("whsec_k", ts.parse().unwrap(), body),
            "signature verifies"
        );
        assert!(body.contains("\"type\":\"execution.report\""));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
