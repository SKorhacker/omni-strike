//! Platform connectors. Each connector maps a platform's native API to canonical
//! `oa_core` types and keeps native ids/metadata. Connectors never decide routing.

pub mod aevo;
pub mod aevo_exec;
pub mod aevo_user;
pub mod binance;
pub mod bullish;
pub mod bybit;
pub mod bybit_exec;
pub mod callput;
pub mod callput_user;
pub mod coincall;
pub mod delta;
pub mod deribit;
pub mod deribit_exec;
pub mod derive;
pub mod derive_exec;
pub mod derive_stream;
pub mod derive_user;
pub mod exec;
pub mod fx;
pub mod gate;
pub mod hegic;
mod http;
pub mod hypercall;
pub mod hypercall_user;
pub mod hyperliquid;
pub mod hyperliquid_user;
pub mod kalshi;
pub mod limitless;
pub mod limitless_user;
pub mod myriad;
pub mod network;
pub mod okx;
pub mod okx_exec;
pub mod paradex;
pub mod paradex_user;
pub mod registry;
pub mod rysk;
pub mod thetanuts;
pub mod thetanuts_user;

use async_trait::async_trait;
use oa_core::domain::{BookSnapshot, Capability, Instrument, PlatformId};
use oa_core::fees::FeeSchedule;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConnectorError {
    #[error("{platform}: request timed out after {after:?}")]
    Timeout {
        platform: PlatformId,
        after: Duration,
    },
    #[error("{platform}: rate limited (retry after {retry_after:?})")]
    RateLimited {
        platform: PlatformId,
        retry_after: Option<Duration>,
    },
    #[error("{platform}: HTTP {status}: {body}")]
    Http {
        platform: PlatformId,
        status: u16,
        body: String,
    },
    #[error("{platform}: transport error: {source}")]
    Transport {
        platform: PlatformId,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The response did not match the documented/expected shape. Never silently patched.
    #[error("{platform}: invalid response: {detail}")]
    InvalidResponse {
        platform: PlatformId,
        detail: String,
    },
    #[error("{platform}: unknown instrument {native_id}")]
    UnknownInstrument {
        platform: PlatformId,
        native_id: String,
    },
    /// The platform itself reports that its data is stale (e.g. Aevo `503 STALE_DATA`).
    /// Not a sign of an unhealthy connection: does not count toward the circuit breaker.
    #[error("{platform}: platform reports stale data ({detail})")]
    UpstreamStale {
        platform: PlatformId,
        detail: String,
    },
}

impl ConnectorError {
    pub fn platform(&self) -> PlatformId {
        match self {
            Self::Timeout { platform, .. }
            | Self::RateLimited { platform, .. }
            | Self::Http { platform, .. }
            | Self::Transport { platform, .. }
            | Self::InvalidResponse { platform, .. }
            | Self::UnknownInstrument { platform, .. }
            | Self::UpstreamStale { platform, .. } => *platform,
        }
    }
}

pub type Result<T> = std::result::Result<T, ConnectorError>;

/// A book snapshot plus how long each stage took (monotonic clock).
#[derive(Debug, Clone)]
pub struct MarketData {
    pub book: BookSnapshot,
    /// Request(s) sent -> last response body received (external latency, incl. TLS/WS setup).
    pub network: Duration,
    /// Raw JSON -> canonical `BookSnapshot` (internal normalisation cost).
    pub normalize: Duration,
    /// How the snapshot was obtained: "rest", "ws_stream", "ws_one_shot".
    pub path: &'static str,
}

impl MarketData {
    /// REST snapshot; `normalize` = time elapsed since `t1` (start of parsing).
    pub(crate) fn rest(book: BookSnapshot, network: Duration, t1: std::time::Instant) -> Self {
        Self {
            book,
            network,
            normalize: t1.elapsed(),
            path: "rest",
        }
    }
}

/// What THIS connector implements (distinct from what the platform offers; see `registry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Implemented {
    Yes,
    No,
}

#[async_trait]
pub trait Connector: Send + Sync {
    fn platform(&self) -> PlatformId;
    /// Network/environment label used in instrument keys.
    fn network(&self) -> &'static str;
    /// Default: read-only market data (catalog + books).
    fn implemented(&self, cap: Capability) -> Implemented {
        match cap {
            Capability::InstrumentDiscovery | Capability::MarketData | Capability::Orderbook => {
                Implemented::Yes
            }
            _ => Implemented::No,
        }
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>>;
    /// Full book snapshot (with index/mark prices when available) + measured timings.
    async fn get_market_data(&self, instrument: &Instrument) -> Result<MarketData>;
    /// Fee schedule applicable to this instrument, with its documentary source.
    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule>;

    /// Background keep-warm period for recently viewed books (polled venues). Venues with tight
    /// public rate limits override it (measured 2026-10-06: Binance and Bullish rate-limited at 1 s).
    fn hot_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }

    /// Top of book (<= 1 level per side) for EVERY option of `underlying` in ONE request, in the
    /// same units as `get_market_data` (premium per native contract, same `native_id`s). Refreshed in
    /// the background so a first request on a contract is priced at once; the full book is fetched
    /// right after. `Ok(None)` = the venue has no such endpoint (default).
    async fn bulk_top_of_book(&self, _underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        Ok(None)
    }

    /// Streaming venues: subscribe in advance (no waiting, no one-shot fallback). Default no-op.
    fn prewarm(&self, _instruments: &[Instrument]) {}

    /// Period of the background `bulk_top_of_book` refresh.
    fn bulk_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    /// True when books are pushed by a persistent stream (no need to poll them warm).
    fn streams_books(&self) -> bool {
        false
    }

    /// Connector-specific live diagnostics (e.g. WebSocket stream stats). None by default.
    fn diagnostics(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Parse a decimal transmitted as a JSON string (or number) without going through f64.
pub(crate) fn dec(
    platform: PlatformId,
    v: &serde_json::Value,
    field: &str,
) -> Result<rust_decimal::Decimal> {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    match v {
        serde_json::Value::String(s) => {
            Decimal::from_str(s).map_err(|e| invalid(platform, format!("{field}={s}: {e}")))
        }
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string())
            .map_err(|e| invalid(platform, format!("{field}: {e}"))),
        _ => Err(invalid(
            platform,
            format!("{field}: expected decimal string, got {v}"),
        )),
    }
}

pub(crate) fn opt_dec(
    platform: PlatformId,
    v: Option<&serde_json::Value>,
    field: &str,
) -> Result<Option<rust_decimal::Decimal>> {
    match v {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.is_empty() => Ok(None),
        Some(x) => dec(platform, x, field).map(Some),
    }
}

pub(crate) fn invalid(platform: PlatformId, detail: impl Into<String>) -> ConnectorError {
    ConnectorError::InvalidResponse {
        platform,
        detail: detail.into(),
    }
}

/// One book side `v[side]` of `[price, size, ..]` arrays (native units); every level must be > 0.
pub(crate) fn pair_levels(
    platform: PlatformId,
    v: &serde_json::Value,
    side: &str,
    what: &str,
    size_field: &str,
) -> Result<Vec<oa_core::domain::Level>> {
    let arr = v[side]
        .as_array()
        .ok_or_else(|| invalid(platform, format!("{what}.{side} missing")))?;
    arr.iter()
        .map(|l| {
            let price = dec(platform, &l[0], "level.price")?;
            let size = dec(platform, &l[1], size_field)?;
            if price <= rust_decimal::Decimal::ZERO || size <= rust_decimal::Decimal::ZERO {
                return Err(invalid(platform, format!("non-positive book level {l}")));
            }
            Ok(oa_core::domain::Level { price, size })
        })
        .collect()
}

/// "order not representable" = definite rejection upstream (nothing was sent to the venue).
pub(crate) fn not_representable(platform: PlatformId, d: impl std::fmt::Display) -> ConnectorError {
    invalid(platform, format!("order not representable: {d}"))
}

/// Unix milliseconds -> UTC timestamp.
pub(crate) fn ts_ms(
    platform: PlatformId,
    ms: Option<i64>,
    field: &str,
) -> Result<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    let ms = ms.ok_or_else(|| invalid(platform, format!("{field}: expected ms")))?;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| invalid(platform, format!("{field}: out of range")))
}

/// Recorded API response `fixtures/<dir>/<name>` (tests only).
#[cfg(test)]
pub(crate) fn fixture(dir: &str, name: &str) -> serde_json::Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(dir)
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}
