//! OKX **Demo Trading** execution (simulated money). There is NO live constructor: every request
//! carries `x-simulated-trading: 1` and demo API keys (separate from live keys) are required.
//!
//! Sources (official v5 docs, https://www.okx.com/docs-v5/en/, read 2026-10-06):
//! - Authentication: headers OK-ACCESS-KEY, OK-ACCESS-SIGN, OK-ACCESS-TIMESTAMP, OK-ACCESS-PASSPHRASE;
//!   `sign = Base64(HMAC_SHA256(secret, timestamp + METHOD + requestPath + body))`, timestamp ISO-8601
//!   with milliseconds UTC (e.g. `2020-12-08T09:08:57.715Z`), METHOD upper case, GET query string is
//!   part of requestPath, body omitted when empty.
//! - Demo trading: "x-simulated-trading: 1 needs to be added to the header of the Demo Trading
//!   request". Demo keys are created under Trade > Demo Trading > Demo Trading API.
//! - `POST /api/v5/trade/order`: instId, tdMode, side, ordType (`ioc` = "Immediate-or-cancel
//!   order", px applicable), px, sz (contracts for OPTION), clOrdId (case-sensitive alphanumerics,
//!   <= 32 chars, unique among PENDING orders; `GET /trade/order` returns the latest match).
//!   For options exactly one of px/pxUsd/pxVol; `px` for options is in the venue unit (coin per
//!   1 underlying for inverse, USD per 1 underlying for `_UM`). We send `px` (not `pxUsd`) so that
//!   the USD->coin conversion and its rounding are ours and conservative (see `limit_to_px`).
//! - tdMode for options (order-endpoint notes): Spot mode "OPTION buyer: cash"; Futures mode,
//!   Multi-currency margin and Portfolio margin: "Cross FUTURES/SWAP/OPTION: cross". We read
//!   `acctLv` from `GET /api/v5/account/config` (1 Spot, 2 Futures, 3 Multi-ccy, 4 Portfolio) once
//!   and cache it; `OA_OKX_DEMO_TD_MODE` (cash|cross|isolated) overrides it.
//! - Tick: for OPTION `tickSz` is "the minimum tickSz among tick band"; the real tick depends on the
//!   price band (`GET /api/v5/public/instrument-tick-bands?instType=OPTION&instFamily=..`). Demo
//!   bands differ from live ones (measured 2026-10-06: BTC-USD_UM demo [0,5) tick 1 / [5,150000)
//!   tick 5, live tick 5 everywhere; inverse BTC-USD [0,0.005) 0.0001 / [0.005,..) 0.0005), so the
//!   bands are fetched FROM DEMO (cached 1 h).
//! - `GET /api/v5/trade/order?instId=..&clOrdId=..`: state live | partially_filled | filled |
//!   canceled | mmp_canceled ("For IOC orders partially filled before cancellation, accFillSz may be
//!   non-zero"); accFillSz in contracts; avgPx in the px unit (coin for inverse options).
//! - `GET /api/v5/trade/fills?instType=OPTION&instId=..&ordId=..` (last 3 days): per fill `fillPx`,
//!   `fillSz`, `fillIdxPx` ("Index price at the moment of trade execution"), `fillPxUsd`.
//! - `POST /api/v5/trade/cancel-order` {instId, clOrdId}: sCode 0 only means "request accepted";
//!   the result is read back with get order.
//! - Errors: envelope `{"code","msg","data":[{"sCode","sMsg"}]}`. 50001 (503), 50004 ("request
//!   timeout, does not mean that the request was successful or failed"), 50013 (busy), 50026
//!   (system error) and 51149 ("Order timed out ... check the request result") are UNKNOWN outcomes
//!   -> mapped to HTTP 503 (never a definite rejection, even though OKX sends 50004 with HTTP 400).
//!   50011/50040/50061 = rate limits. 51603 "Order does not exist" -> 404 (`get_order` = None).
//!   51011/51016 duplicate id -> 409 (reconcile). Every other numeric code = definite business or
//!   auth rejection -> 400/401.
//!
//! Units (critical). The engine's `ExecOrder.limit_price_native` is USD(C) per NATIVE contract and
//! `qty_native` is in contracts (see `okx.rs`: level price = px x multiplier [x index for inverse]).
//! - inverse (coin-margined): px = limit / (index_usd x multiplier), index = `GET
//!   /api/v5/market/index-tickers?instId=BTC-USD` fetched at submit time (from demo).
//! - `_UM` (linear): px = limit / multiplier.
//!
//! Rounding to the band tick is conservative FOR THE USER: buy -> round DOWN (never pay above the
//! engine's limit), sell -> round UP (never sell below it). A buy limit below one tick is refused.
//! The index moves between quote and submit: the conversion keeps the USD limit at the submit-time
//! index, which is the correct guarantee (the coin price paid is worth <= limit USD at that index).
//!
//! Average fill price back to USD per contract: `_UM` = avgPx x multiplier. Inverse = size-weighted
//! sum(fillPx x fillIdxPx x fillSz) / sum(fillSz) x multiplier, using the index AT FILL TIME from the
//! fills endpoint. If fills are unavailable, incomplete (sum fillSz != accFillSz) or lack fillIdxPx,
//! `avg_price_native` is None and `raw.oa_avg_px_note` says why (never converted at a guessed index).
//!
//! Every failure BEFORE the order is sent (index, bands, account mode, representability) is
//! reported as `InvalidResponse { detail: "order not representable: ..." }` = definite rejection:
//! nothing was created on the venue.

use crate::aevo_exec::TestnetConfigError;
use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{REQUEST_TIMEOUT, client};
use crate::{ConnectorError, Result, dec, opt_dec};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEMO_REST: &str = "https://www.okx.com";
/// Network label of the demo environment. Demo books/instruments differ from live ones, so routes
/// must be priced from a demo read connector carrying the same label.
pub const DEMO_NETWORK: &str = "okx-demo";
const P: PlatformId = PlatformId::Okx;
const BANDS_TTL: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------------------------

/// Standard base64 (RFC 4648, with padding).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// `timestamp + METHOD + requestPath(+?query) + body`.
pub fn prehash(ts: &str, method: &str, request_path: &str, body: &str) -> String {
    format!("{ts}{}{request_path}{body}", method.to_uppercase())
}

/// OK-ACCESS-SIGN = Base64(HMAC-SHA256(secret, prehash)).
pub fn sign(secret: &str, prehash: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(prehash.as_bytes());
    base64(&mac.finalize().into_bytes())
}

/// ISO-8601, milliseconds, UTC, e.g. `2020-12-08T09:08:57.715Z`.
pub fn iso_ms(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Query string sent AND signed byte-for-byte. Values are restricted to characters that need no
/// percent-encoding, so what we sign is exactly what the server sees.
pub fn query(params: &[(&str, &str)]) -> std::result::Result<String, String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ','))
    };
    let mut parts = vec![];
    for (k, v) in params {
        if !ok(k) || !ok(v) {
            return Err(format!("query parameter {k}={v:?} has unsafe characters"));
        }
        parts.push(format!("{k}={v}"));
    }
    Ok(parts.join("&"))
}

// ---------------------------------------------------------------------------------------------
// Units
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickBand {
    pub min_px: Decimal,
    pub max_px: Decimal,
    pub tick: Decimal,
}

/// `data[0].tickBand` of `/public/instrument-tick-bands`.
pub fn parse_tick_bands(data: &Value, inst_family: &str) -> Result<Vec<TickBand>> {
    let fam = data
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["instFamily"].as_str() == Some(inst_family))
        .ok_or_else(|| crate::invalid(P, format!("no tick bands for {inst_family}")))?;
    let mut out = vec![];
    for b in fam["tickBand"].as_array().into_iter().flatten() {
        let band = TickBand {
            min_px: dec(P, &b["minPx"], "minPx")?,
            max_px: dec(P, &b["maxPx"], "maxPx")?,
            tick: dec(P, &b["tickSz"], "tickSz")?,
        };
        if band.tick <= Decimal::ZERO || band.max_px <= band.min_px {
            return Err(crate::invalid(P, format!("bad tick band {b}")));
        }
        out.push(band);
    }
    if out.is_empty() {
        return Err(crate::invalid(
            P,
            format!("empty tick bands for {inst_family}"),
        ));
    }
    out.sort_by(|a, b| a.min_px.cmp(&b.min_px));
    Ok(out)
}

fn band_tick(p: Decimal, bands: &[TickBand]) -> Option<Decimal> {
    bands
        .iter()
        .find(|b| p >= b.min_px && p < b.max_px)
        .map(|b| b.tick)
}

/// Round to the band tick, conservatively for the user: buy DOWN, sell UP. The result must lie on
/// the tick of ITS OWN band (a sell rounded up across a band boundary is re-checked).
pub fn round_px(raw: Decimal, side: Side, bands: &[TickBand]) -> Option<Decimal> {
    if raw <= Decimal::ZERO {
        return None;
    }
    let mut p = raw;
    for _ in 0..4 {
        let t = band_tick(p, bands)?;
        let q = p.checked_div(t)?;
        let r = match side {
            Side::Buy => q.floor(),
            Side::Sell => q.ceil(),
        } * t;
        if r <= Decimal::ZERO {
            return None; // buy limit below one tick: not representable
        }
        let t2 = band_tick(r, bands)?;
        if (r % t2).is_zero() {
            return Some(r.normalize());
        }
        p = r;
    }
    None
}

/// Engine limit (USD per native contract) -> venue px (per 1 underlying unit, coin for inverse,
/// USD for `_UM`), rounded conservatively. `index_usd` is mandatory for inverse instruments.
pub fn limit_to_px(
    limit_usd_per_contract: Decimal,
    multiplier: Decimal,
    inverse: bool,
    index_usd: Option<Decimal>,
    side: Side,
    bands: &[TickBand],
) -> std::result::Result<Decimal, String> {
    if limit_usd_per_contract <= Decimal::ZERO || multiplier <= Decimal::ZERO {
        return Err("non-positive limit or multiplier".into());
    }
    let per_unit_usd = limit_usd_per_contract
        .checked_div(multiplier)
        .ok_or("division overflow")?;
    let raw = if inverse {
        let idx = index_usd
            .filter(|i| *i > Decimal::ZERO)
            .ok_or("inverse instrument without a positive index price")?;
        per_unit_usd.checked_div(idx).ok_or("division overflow")?
    } else {
        per_unit_usd
    };
    round_px(raw, side, bands).ok_or_else(|| {
        format!("limit {limit_usd_per_contract} USD/contract -> px {raw} is outside the tick bands or below one tick")
    })
}

/// Venue px (per 1 underlying unit) -> USD per native contract.
pub fn px_to_usd_per_contract(
    px: Decimal,
    multiplier: Decimal,
    inverse: bool,
    index_usd: Option<Decimal>,
) -> Option<Decimal> {
    let per_contract = px * multiplier;
    if inverse {
        index_usd
            .filter(|i| *i > Decimal::ZERO)
            .map(|i| per_contract * i)
    } else {
        Some(per_contract)
    }
}

/// Inverse fills -> size-weighted USD per contract using each fill's index at execution.
/// Err(reason) when it cannot be computed honestly.
pub fn inverse_avg_usd_per_contract(
    fills: &[Value],
    acc_fill_sz: Decimal,
    multiplier: Decimal,
) -> std::result::Result<Decimal, String> {
    let mut sz_sum = Decimal::ZERO;
    let mut usd_sum = Decimal::ZERO;
    for f in fills {
        let g = |k: &str| {
            opt_dec(P, f.get(k), k)
                .ok()
                .flatten()
                .filter(|d| *d > Decimal::ZERO)
                .ok_or(format!("fill without positive {k}"))
        };
        let (px, sz, idx) = (g("fillPx")?, g("fillSz")?, g("fillIdxPx")?);
        sz_sum += sz;
        usd_sum += px * idx * sz;
    }
    if sz_sum.is_zero() || sz_sum != acc_fill_sz {
        return Err(format!(
            "fills incomplete: sum fillSz {sz_sum} != accFillSz {acc_fill_sz}"
        ));
    }
    Ok((usd_sum / sz_sum * multiplier).normalize())
}

/// clOrdId derived from the stored order: alphanumeric, <= 32 chars ("oa" + <=10 + <=20 digits).
pub fn cl_ord_id(o: &ExecOrder) -> String {
    format!("oa{}{}", o.timestamp.clamp(0, 9_999_999_999), o.salt)
}

/// Our venue order id: `instId:clOrdId` (lookup by clOrdId needs the instId).
pub fn venue_id(inst_id: &str, cl: &str) -> String {
    format!("{inst_id}:{cl}")
}

fn split_id(id: &str) -> Option<(&str, &str)> {
    let (i, c) = id.rsplit_once(':')?;
    (!i.is_empty() && !c.is_empty()).then_some((i, c))
}

/// `BTC-USD-261009-86000-C` -> `BTC-USD`; `BTC-USD_UM-261009-86000-C` -> `BTC-USD_UM`.
pub fn inst_family(inst_id: &str) -> Option<String> {
    let mut it = inst_id.split('-');
    let (a, b) = (it.next()?, it.next()?);
    Some(format!("{a}-{b}"))
}

/// Index id for an option family: `BTC-USD_UM` -> `BTC-USD`.
fn index_id(family: &str) -> &str {
    family.strip_suffix("_UM").unwrap_or(family)
}

/// tdMode from `acctLv` (see module doc).
pub fn td_mode_for(acct_lv: &str) -> std::result::Result<&'static str, String> {
    match acct_lv {
        "1" => Ok("cash"),
        "2" | "3" | "4" => Ok("cross"),
        other => Err(format!("unknown account mode acctLv={other:?}")),
    }
}

// ---------------------------------------------------------------------------------------------
// Response classification
// ---------------------------------------------------------------------------------------------

/// Map an OKX HTTP answer to `data` or a typed error (see module doc).
pub fn classify(http_status: u16, body: &str) -> Result<Value> {
    let http_err = |status: u16, b: String| ConnectorError::Http {
        platform: P,
        status,
        body: b.chars().take(300).collect(),
    };
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            if http_status == 429 {
                return Err(ConnectorError::RateLimited {
                    platform: P,
                    retry_after: None,
                });
            }
            if (200..300).contains(&http_status) {
                return Err(crate::invalid(P, format!("not JSON: {e}")));
            }
            return Err(http_err(http_status, body.to_string()));
        }
    };
    let code = v["code"]
        .as_str()
        .map(str::to_string)
        .or_else(|| v["code"].as_i64().map(|c| c.to_string()));
    let s0 = &v["data"][0];
    let scode = s0["sCode"].as_str().filter(|s| !s.is_empty() && *s != "0");
    if code.as_deref() == Some("0") && scode.is_none() && (200..300).contains(&http_status) {
        return v
            .get("data")
            .filter(|d| d.is_array())
            .cloned()
            .ok_or_else(|| crate::invalid(P, "no data array"));
    }
    // Effective code: the per-item sCode when present (place/cancel), else the envelope code.
    let (eff, msg) = match scode {
        Some(s) => (s.to_string(), s0["sMsg"].as_str().unwrap_or("").to_string()),
        None => (
            code.clone().unwrap_or_default(),
            v["msg"].as_str().unwrap_or("").to_string(),
        ),
    };
    let detail = format!("code {eff}: {msg}");
    if http_status == 429 || matches!(eff.as_str(), "50011" | "50040" | "50061") {
        return Err(ConnectorError::RateLimited {
            platform: P,
            retry_after: None,
        });
    }
    let status = match eff.as_str() {
        "50001" | "50004" | "50013" | "50026" | "51149" => 503,
        "51603" => 404,
        "51011" | "51016" => 409,
        c if c.starts_with("501") && http_status == 401 => 401,
        c if c.len() == 5 && c.chars().all(|x| x.is_ascii_digit()) => {
            if http_status >= 500 {
                503
            } else {
                400
            }
        }
        // "1"/"2" without an item code, or anything unparseable: outcome unknown.
        _ => 503,
    };
    Err(http_err(status, detail))
}

// ---------------------------------------------------------------------------------------------
// Order state
// ---------------------------------------------------------------------------------------------

/// One element of `/trade/order` data -> state (prices still in the venue px unit; see
/// `ExecutionVenue::get_order` for the USD conversion).
pub fn parse_order(o: &Value) -> Result<(VenueOrderState, Option<Decimal>)> {
    let inst = o["instId"].as_str().unwrap_or_default();
    let cl = o["clOrdId"].as_str().unwrap_or_default();
    if inst.is_empty() || cl.is_empty() {
        return Err(crate::invalid(
            P,
            format!("order without instId/clOrdId: {o}"),
        ));
    }
    let filled = opt_dec(P, o.get("accFillSz"), "accFillSz")?.unwrap_or_default();
    let avg_px = opt_dec(P, o.get("avgPx"), "avgPx")?.filter(|d| *d > Decimal::ZERO);
    let raw = o["state"].as_str().unwrap_or("").to_string();
    let (status, fin) = match raw.as_str() {
        "filled" => (VenueStatus::Filled, true),
        "live" => (VenueStatus::Open, false),
        "partially_filled" => (VenueStatus::PartiallyFilled, false),
        // IOC remainder cancelled: terminal partial fill when something filled.
        "canceled" | "mmp_canceled" => (
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Cancelled
            },
            true,
        ),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    Ok((
        VenueOrderState {
            venue_order_id: venue_id(inst, cl),
            status,
            filled_native: filled,
            avg_price_native: None,
            is_final: fin,
            raw: o.clone(),
        },
        avg_px,
    ))
}

/// `/account/balance` data -> balance in USD (see `ExecutionVenue::balance`).
pub fn parse_balance(data: &Value) -> Result<VenueBalance> {
    let a = data
        .get(0)
        .ok_or_else(|| crate::invalid(P, "balance: empty data"))?;
    let equity = opt_dec(P, a.get("totalEq"), "totalEq")?;
    let mut per_ccy = vec![];
    let mut sum = Decimal::ZERO;
    for d in a["details"].as_array().into_iter().flatten() {
        let avail = opt_dec(P, d.get("availBal"), "availBal")?.unwrap_or_default();
        let eq = opt_dec(P, d.get("eq"), "eq")?.unwrap_or_default();
        let eq_usd = opt_dec(P, d.get("eqUsd"), "eqUsd")?.unwrap_or_default();
        let avail_usd = if eq > Decimal::ZERO {
            avail * eq_usd / eq
        } else {
            Decimal::ZERO
        };
        sum += avail_usd;
        per_ccy.push(
            json!({"ccy": d["ccy"], "availBal": d["availBal"], "eq": d["eq"], "eqUsd": d["eqUsd"],
                            "availUsd": avail_usd.round_dp(8).normalize().to_string()}),
        );
    }
    let available = match opt_dec(P, a.get("availEq"), "availEq")? {
        Some(x) => x,
        None => sum,
    };
    Ok(VenueBalance {
        available: available.round_dp(8).normalize(),
        equity,
        currency: "USD".into(),
        raw: json!({"totalEq": a["totalEq"], "availEq": a["availEq"], "adjEq": a["adjEq"], "details": per_ccy,
                    "available_source": if a["availEq"].as_str().is_some_and(|s| !s.is_empty()) { "availEq (account level)" } else { "sum(availBal x eqUsd/eq) over currencies" }}),
    })
}

// ---------------------------------------------------------------------------------------------
// Venue
// ---------------------------------------------------------------------------------------------

struct Secret(String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Clone, Copy)]
struct Spec {
    inverse: bool,
    multiplier: Decimal,
}

pub struct OkxDemoExec {
    http: reqwest::Client,
    rest: String,
    api_key: Secret,
    api_secret: Secret,
    passphrase: Secret,
    td_mode_override: Option<&'static str>,
    acct_lv: Mutex<Option<String>>,
    specs: Mutex<HashMap<String, Spec>>,
    bands: Mutex<HashMap<String, (Instant, Vec<TickBand>)>>,
}

impl fmt::Debug for OkxDemoExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OkxDemoExec")
            .field("rest", &self.rest)
            .field("demo", &true)
            .field("td_mode_override", &self.td_mode_override)
            .field("keys", &"<redacted>")
            .finish()
    }
}

fn not_representable(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

impl OkxDemoExec {
    /// Reads OA_OKX_DEMO_{API_KEY,API_SECRET,PASSPHRASE} (+ optional OA_OKX_DEMO_TD_MODE).
    /// Values are never logged.
    pub fn testnet_from_env() -> std::result::Result<Self, TestnetConfigError> {
        Self::testnet_from_lookup(|k| std::env::var(k).ok(), DEMO_REST)
    }

    pub fn testnet_from_lookup(
        get: impl Fn(&str) -> Option<String>,
        rest: &str,
    ) -> std::result::Result<Self, TestnetConfigError> {
        use TestnetConfigError as E;
        let need = |k: &'static str| -> std::result::Result<String, E> {
            let v = get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or(E::Missing(k))?;
            let v = v.trim().to_string();
            // Must be a valid header value / HMAC key: printable ASCII, no spaces.
            if !v.chars().all(|c| c.is_ascii_graphic()) {
                return Err(E::Malformed(k));
            }
            Ok(v)
        };
        let api_key = need("OA_OKX_DEMO_API_KEY")?;
        let api_secret = need("OA_OKX_DEMO_API_SECRET")?;
        let passphrase = need("OA_OKX_DEMO_PASSPHRASE")?;
        let td_mode_override = match get("OA_OKX_DEMO_TD_MODE")
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .as_deref()
        {
            None => None,
            Some("cash") => Some("cash"),
            Some("cross") => Some("cross"),
            Some("isolated") => Some("isolated"),
            Some(_) => return Err(E::Malformed("OA_OKX_DEMO_TD_MODE")),
        };
        Ok(Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            api_key: Secret(api_key),
            api_secret: Secret(api_secret),
            passphrase: Secret(passphrase),
            td_mode_override,
            acct_lv: Mutex::new(None),
            specs: Mutex::new(HashMap::new()),
            bands: Mutex::new(HashMap::new()),
        })
    }

    /// `path_q` = path + optional `?query`, signed exactly as sent.
    async fn call(
        &self,
        method: reqwest::Method,
        path_q: &str,
        body: Option<String>,
        auth: bool,
    ) -> Result<Value> {
        let mut rb = self
            .http
            .request(method.clone(), format!("{}{}", self.rest, path_q))
            .header("x-simulated-trading", "1");
        if auth {
            let ts = iso_ms(chrono::Utc::now());
            let s = sign(
                &self.api_secret.0,
                &prehash(&ts, method.as_str(), path_q, body.as_deref().unwrap_or("")),
            );
            rb = rb
                .header("OK-ACCESS-KEY", &self.api_key.0)
                .header("OK-ACCESS-SIGN", s)
                .header("OK-ACCESS-TIMESTAMP", ts)
                .header("OK-ACCESS-PASSPHRASE", &self.passphrase.0);
        }
        if let Some(b) = body {
            rb = rb.header("content-type", "application/json").body(b);
        }
        let resp = rb.send().await.map_err(send_err)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(send_err)?;
        classify(status, &text)
    }

    async fn get(&self, path: &str, params: &[(&str, &str)], auth: bool) -> Result<Value> {
        let q = query(params).map_err(|e| crate::invalid(P, e))?;
        let pq = if q.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{q}")
        };
        self.call(reqwest::Method::GET, &pq, None, auth).await
    }

    async fn td_mode(&self) -> Result<&'static str> {
        if let Some(m) = self.td_mode_override {
            return Ok(m);
        }
        let cached = self.acct_lv.lock().expect("acct_lv").clone();
        let lv = match cached {
            Some(lv) => lv,
            None => {
                let d = self.get("/api/v5/account/config", &[], true).await?;
                let lv = d[0]["acctLv"]
                    .as_str()
                    .ok_or_else(|| crate::invalid(P, "account config without acctLv"))?
                    .to_string();
                *self.acct_lv.lock().expect("acct_lv") = Some(lv.clone());
                lv
            }
        };
        td_mode_for(&lv).map_err(|e| crate::invalid(P, e))
    }

    async fn tick_bands(&self, family: &str) -> Result<Vec<TickBand>> {
        if let Some((t, b)) = self.bands.lock().expect("bands").get(family)
            && t.elapsed() < BANDS_TTL
        {
            return Ok(b.clone());
        }
        let d = self
            .get(
                "/api/v5/public/instrument-tick-bands",
                &[("instType", "OPTION"), ("instFamily", family)],
                false,
            )
            .await?;
        let b = parse_tick_bands(&d, family)?;
        self.bands
            .lock()
            .expect("bands")
            .insert(family.to_string(), (Instant::now(), b.clone()));
        Ok(b)
    }

    async fn index_usd(&self, family: &str) -> Result<Decimal> {
        let id = index_id(family);
        let d = self
            .get("/api/v5/market/index-tickers", &[("instId", id)], false)
            .await?;
        let x = &d[0];
        if x["instId"].as_str() != Some(id) {
            return Err(crate::invalid(P, format!("index ticker mismatch for {id}")));
        }
        opt_dec(P, x.get("idxPx"), "idxPx")?
            .filter(|i| *i > Decimal::ZERO)
            .ok_or_else(|| crate::invalid(P, format!("no positive idxPx for {id}")))
    }

    /// Contract spec: from the route's frozen `venue_meta` at submit, else cache, else the demo
    /// public instruments endpoint (needed by reconciliation after a restart).
    async fn spec(&self, inst_id: &str, meta: Option<&Value>) -> Result<Spec> {
        if let Some(m) = meta
            && let Some(s) = spec_from_meta(m)
        {
            self.specs.lock().expect("specs").insert(inst_id.into(), s);
            return Ok(s);
        }
        if let Some(s) = self.specs.lock().expect("specs").get(inst_id) {
            return Ok(*s);
        }
        let fam = inst_family(inst_id)
            .ok_or_else(|| crate::invalid(P, format!("bad instId {inst_id}")))?;
        let d = self
            .get(
                "/api/v5/public/instruments",
                &[
                    ("instType", "OPTION"),
                    ("instFamily", &fam),
                    ("instId", inst_id),
                ],
                false,
            )
            .await?;
        let s = d
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["instId"].as_str() == Some(inst_id))
            .and_then(spec_from_meta)
            .ok_or_else(|| crate::invalid(P, format!("no contract spec for {inst_id}")))?;
        self.specs.lock().expect("specs").insert(inst_id.into(), s);
        Ok(s)
    }

    async fn read_order(&self, inst_id: &str, cl: &str) -> Result<Option<VenueOrderState>> {
        let d = match self
            .get(
                "/api/v5/trade/order",
                &[("instId", inst_id), ("clOrdId", cl)],
                true,
            )
            .await
        {
            Ok(d) => d,
            Err(ConnectorError::Http { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(o) = d.get(0) else {
            return Ok(None);
        };
        let (mut st, avg_px) = parse_order(o)?;
        if st.filled_native.is_zero() {
            return Ok(Some(st));
        }
        let Some(avg_px) = avg_px else {
            note(&mut st.raw, "filled but avgPx empty");
            return Ok(Some(st));
        };
        let spec = self.spec(inst_id, None).await?;
        if !spec.inverse {
            st.avg_price_native =
                px_to_usd_per_contract(avg_px, spec.multiplier, false, None).map(|d| d.normalize());
            return Ok(Some(st));
        }
        // Inverse: convert with the index AT FILL TIME from the fills endpoint.
        let ord_id = o["ordId"].as_str().unwrap_or_default().to_string();
        let fills = self
            .get(
                "/api/v5/trade/fills",
                &[
                    ("instType", "OPTION"),
                    ("instId", inst_id),
                    ("ordId", &ord_id),
                ],
                true,
            )
            .await;
        match fills {
            Ok(f) => {
                let arr = f.as_array().cloned().unwrap_or_default();
                match inverse_avg_usd_per_contract(&arr, st.filled_native, spec.multiplier) {
                    Ok(usd) => st.avg_price_native = Some(usd),
                    Err(e) => note(&mut st.raw, &e),
                }
                if let Some(obj) = st.raw.as_object_mut() {
                    obj.insert("oa_fills".into(), Value::Array(arr));
                }
            }
            Err(e) => note(&mut st.raw, &format!("fills unavailable: {e}")),
        }
        Ok(Some(st))
    }
}

fn note(raw: &mut Value, why: &str) {
    if let Some(o) = raw.as_object_mut() {
        o.insert(
            "oa_avg_px_note".into(),
            json!(format!("avg price NOT converted to USD: {why}")),
        );
    }
}

fn spec_from_meta(m: &Value) -> Option<Spec> {
    let inverse = match m["ctType"].as_str()? {
        "inverse" => true,
        "linear" => false,
        _ => return None,
    };
    let d = |k: &str| opt_dec(P, m.get(k), k).ok().flatten();
    let multiplier = d("contract_multiplier")
        .or_else(|| Some(d("ctMult")? * d("ctVal")?))
        .filter(|x| *x > Decimal::ZERO)?;
    Some(Spec {
        inverse,
        multiplier,
    })
}

fn send_err(e: reqwest::Error) -> ConnectorError {
    if e.is_timeout() {
        ConnectorError::Timeout {
            platform: P,
            after: REQUEST_TIMEOUT,
        }
    } else {
        ConnectorError::Transport {
            platform: P,
            source: Box::new(e),
        }
    }
}

#[async_trait]
impl ExecutionVenue for OkxDemoExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        DEMO_NETWORK
    }

    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        Some(venue_id(&order.native_id, &cl_ord_id(order)))
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        // --- Everything here happens BEFORE anything is sent: failures = definite rejection. ---
        if o.time_in_force != TimeInForce::Ioc {
            return Err(not_representable("only IOC orders are supported"));
        }
        if o.qty_native <= Decimal::ZERO || !o.qty_native.fract().is_zero() {
            return Err(not_representable(format!(
                "size {} is not a positive whole number of contracts",
                o.qty_native
            )));
        }
        let fam = o.venue_meta["instFamily"]
            .as_str()
            .map(str::to_string)
            .or_else(|| inst_family(&o.native_id))
            .ok_or_else(|| not_representable("no instFamily"))?;
        let spec = self
            .spec(&o.native_id, Some(&o.venue_meta))
            .await
            .map_err(not_representable)?;
        let bands = self.tick_bands(&fam).await.map_err(not_representable)?;
        let index = if spec.inverse {
            Some(self.index_usd(&fam).await.map_err(not_representable)?)
        } else {
            None
        };
        let px = limit_to_px(
            o.limit_price_native,
            spec.multiplier,
            spec.inverse,
            index,
            o.side,
            &bands,
        )
        .map_err(not_representable)?;
        let td_mode = self.td_mode().await.map_err(not_representable)?;
        let cl = cl_ord_id(o);
        let body = json!({
            "instId": o.native_id,
            "tdMode": td_mode,
            "side": if o.side == Side::Buy { "buy" } else { "sell" },
            "ordType": "ioc",
            "px": px.to_string(),
            "sz": o.qty_native.normalize().to_string(),
            "clOrdId": cl,
        })
        .to_string();
        // --- Sent. From here, transport errors / 5xx are UNKNOWN outcomes (reconciled by clOrdId). ---
        let ack = self
            .call(
                reqwest::Method::POST,
                "/api/v5/trade/order",
                Some(body),
                true,
            )
            .await?;
        match self.read_order(&o.native_id, &cl).await {
            Ok(Some(mut st)) => {
                if let Some(obj) = st.raw.as_object_mut() {
                    obj.insert(
                        "oa_submit".into(),
                        json!({"px": px.to_string(), "index_usd": index.map(|i| i.to_string()), "tdMode": td_mode}),
                    );
                }
                Ok(st)
            }
            _ => Ok(VenueOrderState {
                venue_order_id: venue_id(&o.native_id, &cl),
                status: VenueStatus::Unknown("acknowledged, state not read yet".into()),
                filled_native: Decimal::ZERO,
                avg_price_native: None,
                is_final: false,
                raw: json!({"ack": ack, "oa_submit": {"px": px.to_string(), "index_usd": index.map(|i| i.to_string()), "tdMode": td_mode}}),
            }),
        }
    }

    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        let (inst, cl) = split_id(id).ok_or_else(|| ConnectorError::UnknownInstrument {
            platform: P,
            native_id: id.into(),
        })?;
        self.read_order(inst, cl).await
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        let (inst, cl) = split_id(id).ok_or_else(|| ConnectorError::UnknownInstrument {
            platform: P,
            native_id: id.into(),
        })?;
        let current =
            self.read_order(inst, cl)
                .await?
                .ok_or_else(|| ConnectorError::UnknownInstrument {
                    platform: P,
                    native_id: id.into(),
                })?;
        if current.is_final {
            return Ok(current);
        }
        let body = json!({"instId": inst, "clOrdId": cl}).to_string();
        match self
            .call(
                reqwest::Method::POST,
                "/api/v5/trade/cancel-order",
                Some(body),
                true,
            )
            .await
        {
            Ok(_) => {}
            // Already filled/cancelled/cancelling: the state read below is authoritative.
            Err(ConnectorError::Http { body, .. })
                if ["51400", "51401", "51402", "51410"]
                    .iter()
                    .any(|c| body.starts_with(&format!("code {c}"))) => {}
            Err(e) => return Err(e),
        }
        Ok(self.read_order(inst, cl).await?.unwrap_or(current))
    }

    fn identity(&self) -> Value {
        let k = &self.api_key.0;
        let prefix: String = k.chars().take(4).collect();
        json!({
            "venue": DEMO_NETWORK,
            "demo": true,
            "header": "x-simulated-trading: 1",
            "rest": self.rest,
            "api_key_prefix": format!("{prefix}..."),
            "td_mode_override": self.td_mode_override,
        })
    }

    /// `GET /api/v5/account/balance`. `equity` = `totalEq` (USD). `available` (USD) = `availEq`
    /// (account level, Multi-currency / Portfolio margin only) when present, else the sum over
    /// currencies of `availBal x eqUsd / eq`. In Spot/Futures mode a coin-margined option is paid
    /// in that coin only, so the sum is an UPPER BOUND; per-currency figures are kept in `raw`.
    async fn balance(&self) -> Result<VenueBalance> {
        let d = self.get("/api/v5/account/balance", &[], true).await?;
        parse_balance(&d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    /// Vectors computed independently with Python (secret = the docs' example SecretKey):
    /// python3 -c "import hmac,hashlib,base64; print(base64.b64encode(hmac.new(
    ///   b'22582BD0CFF14C41EDBF1AB98506286D', MSG.encode(), hashlib.sha256).digest()).decode())"
    /// The docs give no (secret, signature) pair, only the formula and the GET example message
    /// `timestamp + 'GET' + '/api/v5/account/balance?ccy=BTC'`, which is vector 1.
    #[test]
    fn signature_matches_independent_implementation() {
        let secret = "22582BD0CFF14C41EDBF1AB98506286D"; // gitleaks:allow
        let ts = "2020-12-08T09:08:57.715Z";
        let get = prehash(ts, "get", "/api/v5/account/balance?ccy=BTC", "");
        assert_eq!(
            get,
            "2020-12-08T09:08:57.715ZGET/api/v5/account/balance?ccy=BTC"
        );
        assert_eq!(
            sign(secret, &get),
            "HiZhvSfMtWJA3uUIVXV3a/bSXNPCWvYFXoGCVS8V4zY="
        );
        let body = r#"{"instId":"BTC-USD-261009-86000-C","tdMode":"cross","side":"buy","ordType":"ioc","px":"0.0105","sz":"2","clOrdId":"oa1791287379123"}"#;
        assert_eq!(
            sign(secret, &prehash(ts, "POST", "/api/v5/trade/order", body)),
            "n7iktuUVYtWSJwaFmzCZR8CF7XpTFilvCvelNG1yfWU="
        );
        // RFC 4231 case 2, base64 of 5bdcc146...3843.
        assert_eq!(
            sign("Jefe", "what do ya want for nothing?"),
            "W9zBRr9gdU5qBCQmCJV1x1oAPwidJzmDnexYuWTsOEM="
        );
    }

    #[test]
    fn timestamp_and_query_format() {
        use chrono::TimeZone;
        let t = chrono::Utc.timestamp_millis_opt(1_607_418_537_715).unwrap();
        assert_eq!(iso_ms(t), "2020-12-08T09:08:57.715Z");
        assert_eq!(
            query(&[("instId", "BTC-USD-261009-86000-C"), ("clOrdId", "oa12")]).unwrap(),
            "instId=BTC-USD-261009-86000-C&clOrdId=oa12"
        );
        assert!(query(&[("instId", "a b")]).is_err(), "would need encoding");
        assert!(query(&[("instId", "x&y=z")]).is_err());
    }

    fn inverse_bands() -> Vec<TickBand> {
        // Demo BTC-USD bands measured 2026-10-06.
        vec![
            TickBand {
                min_px: dec!(0),
                max_px: dec!(0.005),
                tick: dec!(0.0001),
            },
            TickBand {
                min_px: dec!(0.005),
                max_px: dec!(10000000),
                tick: dec!(0.0005),
            },
        ]
    }
    fn um_bands() -> Vec<TickBand> {
        // Demo BTC-USD_UM bands measured 2026-10-06.
        vec![
            TickBand {
                min_px: dec!(0),
                max_px: dec!(5),
                tick: dec!(1),
            },
            TickBand {
                min_px: dec!(5),
                max_px: dec!(150000),
                tick: dec!(5),
            },
        ]
    }

    #[test]
    fn inverse_limit_conversion_worked_examples() {
        let b = inverse_bands();
        let m = dec!(0.01); // BTC inverse ctMult 0.01 x ctVal 1
        let idx = Some(dec!(86000));
        // 0.0105 BTC x 0.01 x 86000 = 9.03 USD/contract -> exactly 0.0105 (on the 0.0005 tick).
        assert_eq!(
            limit_to_px(dec!(9.03), m, true, idx, Side::Buy, &b).unwrap(),
            dec!(0.0105)
        );
        assert_eq!(
            limit_to_px(dec!(9.03), m, true, idx, Side::Sell, &b).unwrap(),
            dec!(0.0105)
        );
        // 9.10 / 860 = 0.0105814 -> buy DOWN 0.0105 (pays 9.03 <= 9.10), sell UP 0.0110.
        assert_eq!(
            limit_to_px(dec!(9.10), m, true, idx, Side::Buy, &b).unwrap(),
            dec!(0.0105)
        );
        assert_eq!(
            limit_to_px(dec!(9.10), m, true, idx, Side::Sell, &b).unwrap(),
            dec!(0.011)
        );
        // Low band: 2.00 / 860 = 0.0023256 -> 0.0023 / 0.0024 (tick 0.0001).
        assert_eq!(
            limit_to_px(dec!(2), m, true, idx, Side::Buy, &b).unwrap(),
            dec!(0.0023)
        );
        assert_eq!(
            limit_to_px(dec!(2), m, true, idx, Side::Sell, &b).unwrap(),
            dec!(0.0024)
        );
        // Band boundary: 0.00499 -> sell UP lands on 0.0050 (valid in the upper band).
        assert_eq!(round_px(dec!(0.00499), Side::Sell, &b), Some(dec!(0.005)));
        // Upper band, buy: 0.0052 -> 0.0050 (still >= band min, on its tick).
        assert_eq!(round_px(dec!(0.0052), Side::Buy, &b), Some(dec!(0.005)));
        // Buy below one tick is refused (never rounds a buy to 0 or up).
        assert!(limit_to_px(dec!(0.05), m, true, idx, Side::Buy, &b).is_err());
        // Inverse without index: refused, never guessed.
        assert!(limit_to_px(dec!(9.03), m, true, None, Side::Buy, &b).is_err());
        // Buy guarantee: px x mult x index <= limit for any limit.
        for l in [dec!(1.37), dec!(4.3), dec!(4.31), dec!(55.55), dec!(1234.5)] {
            let px = limit_to_px(l, m, true, idx, Side::Buy, &b).unwrap();
            assert!(
                px_to_usd_per_contract(px, m, true, idx).unwrap() <= l,
                "{l}"
            );
            let px = limit_to_px(l, m, true, idx, Side::Sell, &b).unwrap();
            assert!(
                px_to_usd_per_contract(px, m, true, idx).unwrap() >= l,
                "{l}"
            );
        }
    }

    #[test]
    fn um_limit_conversion_worked_examples() {
        let b = um_bands();
        let m = dec!(0.01);
        // 9.31 USD/contract / 0.01 = 931 USD per BTC -> buy 930, sell 935 (tick 5 above 5).
        assert_eq!(
            limit_to_px(dec!(9.31), m, false, None, Side::Buy, &b).unwrap(),
            dec!(930)
        );
        assert_eq!(
            limit_to_px(dec!(9.31), m, false, None, Side::Sell, &b).unwrap(),
            dec!(935)
        );
        // 0.034 USD/contract -> 3.4 USD/BTC in the tick-1 band -> buy 3, sell 4.
        assert_eq!(
            limit_to_px(dec!(0.034), m, false, None, Side::Buy, &b).unwrap(),
            dec!(3)
        );
        assert_eq!(
            limit_to_px(dec!(0.034), m, false, None, Side::Sell, &b).unwrap(),
            dec!(4)
        );
        // Above the last band: refused.
        assert!(limit_to_px(dec!(2000), m, false, None, Side::Buy, &b).is_err());
        assert_eq!(
            px_to_usd_per_contract(dec!(930), m, false, None),
            Some(dec!(9.30))
        );
    }

    #[test]
    fn inverse_avg_uses_index_at_fill_time() {
        let fills = vec![
            json!({"fillPx": "0.0105", "fillSz": "1", "fillIdxPx": "86000"}),
            json!({"fillPx": "0.011", "fillSz": "3", "fillIdxPx": "86100"}),
        ];
        // (0.0105*86000*1 + 0.011*86100*3) / 4 * 0.01 = (903 + 2841.3)/4*0.01 = 9.36075
        assert_eq!(
            inverse_avg_usd_per_contract(&fills, dec!(4), dec!(0.01)).unwrap(),
            dec!(9.36075)
        );
        assert!(
            inverse_avg_usd_per_contract(&fills, dec!(5), dec!(0.01)).is_err(),
            "incomplete"
        );
        let no_idx = vec![json!({"fillPx": "0.0105", "fillSz": "1", "fillIdxPx": ""})];
        assert!(inverse_avg_usd_per_contract(&no_idx, dec!(1), dec!(0.01)).is_err());
    }

    #[test]
    fn status_mapping_never_invents_finality() {
        let s = |st: &str, f: &str| {
            parse_order(
                &json!({"instId": "BTC-USD-261009-86000-C", "clOrdId": "oa1", "ordId": "9",
                                "state": st, "accFillSz": f, "avgPx": ""}),
            )
            .unwrap()
            .0
        };
        let x = s("filled", "2");
        assert_eq!((x.status, x.is_final), (VenueStatus::Filled, true));
        let x = s("canceled", "1");
        assert_eq!((x.status, x.is_final), (VenueStatus::PartiallyFilled, true));
        let x = s("canceled", "0");
        assert_eq!((x.status, x.is_final), (VenueStatus::Cancelled, true));
        let x = s("mmp_canceled", "0");
        assert_eq!((x.status, x.is_final), (VenueStatus::Cancelled, true));
        let x = s("live", "0");
        assert_eq!((x.status, x.is_final), (VenueStatus::Open, false));
        let x = s("partially_filled", "1");
        assert_eq!(
            (x.status, x.is_final),
            (VenueStatus::PartiallyFilled, false)
        );
        let x = s("weird", "0");
        assert_eq!(
            (x.status, x.is_final),
            (VenueStatus::Unknown("weird".into()), false)
        );
        assert_eq!(x.venue_order_id, "BTC-USD-261009-86000-C:oa1");
        assert!(parse_order(&json!({"state": "filled"})).is_err());
    }

    #[test]
    fn error_classification() {
        let st = |h: u16, b: &str| match classify(h, b) {
            Ok(_) => 200,
            Err(ConnectorError::Http { status, .. }) => status,
            Err(ConnectorError::RateLimited { .. }) => 429,
            Err(ConnectorError::InvalidResponse { .. }) => 1,
            Err(e) => panic!("{e:?}"),
        };
        assert_eq!(
            st(
                200,
                r#"{"code":"0","msg":"","data":[{"sCode":"0","ordId":"1"}]}"#
            ),
            200
        );
        // Business rejection in sCode: definite.
        assert_eq!(
            st(
                200,
                r#"{"code":"1","msg":"Operation failed.","data":[{"sCode":"51008","sMsg":"Insufficient"}]}"#
            ),
            400
        );
        // 50004 arrives with HTTP 400 but the outcome is UNKNOWN.
        assert_eq!(
            st(
                400,
                r#"{"code":"50004","msg":"API endpoint request timeout","data":[]}"#
            ),
            503
        );
        assert_eq!(
            st(
                200,
                r#"{"code":"1","msg":"","data":[{"sCode":"51149","sMsg":"Order timed out"}]}"#
            ),
            503
        );
        assert_eq!(st(503, r#"{"code":"50001","msg":"busy","data":[]}"#), 503);
        assert_eq!(
            st(200, r#"{"code":"1","msg":"Operation failed.","data":[]}"#),
            503,
            "no item code"
        );
        assert_eq!(
            st(
                401,
                r#"{"code":"50113","msg":"Invalid signature.","data":[]}"#
            ),
            401
        );
        assert_eq!(
            st(
                200,
                r#"{"code":"51603","msg":"Order does not exist.","data":[]}"#
            ),
            404
        );
        assert_eq!(
            st(
                200,
                r#"{"code":"1","msg":"","data":[{"sCode":"51016","sMsg":"dup"}]}"#
            ),
            409
        );
        assert_eq!(
            st(200, r#"{"code":"50011","msg":"Rate limit","data":[]}"#),
            429
        );
        assert_eq!(st(502, "<html>bad gateway</html>"), 502);
        assert_eq!(st(200, "<html>maintenance</html>"), 1);
    }

    #[test]
    fn ids_family_and_td_mode() {
        let o = ExecOrder {
            native_id: "BTC-USD-261009-86000-C".into(),
            venue_instrument_id: None,
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: dec!(2),
            limit_price_native: dec!(9.03),
            salt: u64::MAX,
            timestamp: 1_791_287_379,
            time_in_force: TimeInForce::Ioc,
        };
        let cl = cl_ord_id(&o);
        assert!(
            cl.len() <= 32 && cl.chars().all(|c| c.is_ascii_alphanumeric()),
            "{cl}"
        );
        assert_eq!(
            split_id(&venue_id(&o.native_id, &cl)),
            Some((o.native_id.as_str(), cl.as_str()))
        );
        assert_eq!(
            inst_family("BTC-USD_UM-261007-66000-C").as_deref(),
            Some("BTC-USD_UM")
        );
        assert_eq!(index_id("BTC-USD_UM"), "BTC-USD");
        assert_eq!(td_mode_for("1"), Ok("cash"));
        assert_eq!(td_mode_for("2"), Ok("cross"));
        assert!(td_mode_for("9").is_err());
        let meta = json!({"ctType": "inverse", "ctMult": "0.01", "ctVal": "1", "contract_multiplier": "0.01"});
        let s = spec_from_meta(&meta).unwrap();
        assert!(s.inverse && s.multiplier == dec!(0.01));
        assert!(spec_from_meta(&json!({"ctType": "inverse"})).is_none());
    }

    #[test]
    fn balance_uses_usd_fields() {
        let d = json!([{"totalEq": "100500", "availEq": "", "adjEq": "",
            "details": [
                {"ccy": "BTC", "availBal": "1", "eq": "1.2", "eqUsd": "103200"},
                {"ccy": "USDT", "availBal": "50", "eq": "100", "eqUsd": "99.9"}]}]);
        let b = parse_balance(&d).unwrap();
        // 1 x 103200/1.2 + 50 x 99.9/100 = 86000 + 49.95
        assert_eq!(b.available, dec!(86049.95));
        assert_eq!(b.equity, Some(dec!(100500)));
        assert_eq!(b.currency, "USD");
        let d = json!([{"totalEq": "10", "availEq": "7.5", "details": []}]);
        assert_eq!(parse_balance(&d).unwrap().available, dec!(7.5));
    }

    #[test]
    fn config_requires_every_secret_and_never_prints_them() {
        let env = |m: &[(&str, &str)]| {
            let m: HashMap<String, String> = m
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            move |k: &str| m.get(k).cloned()
        };
        let full = [
            ("OA_OKX_DEMO_API_KEY", "abcdKEYSECRETPART-1234"),
            ("OA_OKX_DEMO_API_SECRET", "SECRET-22582BD0CFF1"), // gitleaks:allow
            ("OA_OKX_DEMO_PASSPHRASE", "PASSSECRET"),
        ];
        let e = OkxDemoExec::testnet_from_lookup(env(&full), DEMO_REST).unwrap();
        let dbg = format!("{e:?} {}", e.identity());
        assert!(
            !dbg.contains("SECRET") && !dbg.contains("KEYSECRET") && !dbg.contains("22582"),
            "{dbg}"
        );
        assert_eq!(e.identity()["api_key_prefix"], "abcd...");
        assert_eq!(e.identity()["demo"], true);
        assert_eq!(e.network(), "okx-demo");
        assert_eq!(
            OkxDemoExec::testnet_from_lookup(env(&full[..2]), DEMO_REST).unwrap_err(),
            TestnetConfigError::Missing("OA_OKX_DEMO_PASSPHRASE")
        );
        let mut bad = full.to_vec();
        bad[2] = ("OA_OKX_DEMO_PASSPHRASE", "pass word");
        assert_eq!(
            OkxDemoExec::testnet_from_lookup(env(&bad), DEMO_REST).unwrap_err(),
            TestnetConfigError::Malformed("OA_OKX_DEMO_PASSPHRASE")
        );
        let mut td = full.to_vec();
        td.push(("OA_OKX_DEMO_TD_MODE", "margin"));
        assert_eq!(
            OkxDemoExec::testnet_from_lookup(env(&td), DEMO_REST).unwrap_err(),
            TestnetConfigError::Malformed("OA_OKX_DEMO_TD_MODE")
        );
    }
}
