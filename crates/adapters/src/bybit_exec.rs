//! Bybit TESTNET execution (options, REST v5 on `api-testnet.bybit.com`).
//!
//! Sources (read 2026-10-06):
//! - https://bybit-exchange.github.io/docs/v5/guide (Integration guidance / authentication):
//!   testnet host `https://api-testnet.bybit.com`; headers X-BAPI-API-KEY, X-BAPI-TIMESTAMP (ms),
//!   X-BAPI-RECV-WINDOW (default 5000), X-BAPI-SIGN = hex(HMAC_SHA256(secret,
//!   timestamp + apiKey + recvWindow + (queryString | jsonBodyString))). The doc's worked example
//!   ("1658384314791XXXXXXXXXX5000category=option&symbol=BTC-29JUL22-25000-C" ->
//!   410e0f38…f30b) does NOT publish its secret, so it cannot be reproduced; our vectors come from
//!   an independent Python `hmac` computation (see tests).
//! - /v5/order/create: for `category=option`, `orderLinkId` is REQUIRED, unique, <= 36 chars of
//!   [A-Za-z0-9_-]. `qty` in base-coin units. The acknowledgement is ASYNCHRONOUS ("indicates that
//!   the request was successfully accepted"): the order state is read back afterwards.
//! - /v5/order/realtime (open + up to 500 recent closed per category, filter `orderLinkId`) and
//!   /v5/order/history (cancelled/rejected kept 24 h, filled 7 d+). Fields orderId, orderLinkId,
//!   orderStatus, cumExecQty, avgPrice, leavesQty, rejectReason.
//! - Enum orderStatus: New, PartiallyFilled, Untriggered (open); Rejected, PartiallyFilledCanceled
//!   (spot only), Filled, Cancelled ("in derivatives ... may have an executed qty"), Triggered,
//!   Deactivated (closed).
//! - /v5/order/cancel: {category, symbol, orderId|orderLinkId} (symbol is needed -> read first).
//! - /v5/account/wallet-balance?accountType=UNIFIED: options trade only in a Unified Trading
//!   Account. Account-level `totalAvailableBalance` (USD), coin-level `walletBalance`, `locked`,
//!   `totalOrderIM`, `totalPositionIM` (`availableToWithdraw` deprecated 2025-01-09).
//!
//! UNITS: the engine sees Bybit prices converted USDT -> USDC by `fx::FxAdapter` at the rate frozen
//! in `instrument.native.fx.usdc_per_usdt`, passed to us in `ExecOrder.venue_meta`. The limit is
//! converted back with THAT rate (order refused if absent), rounded to the USDT tick in the
//! direction that can only make the order less aggressive (buy DOWN, sell UP). Fill prices are
//! converted to USDC with the same rate (cached per orderLinkId; after a restart a live Kraken
//! rate is used and flagged in `raw.fx_source`). Quantity = coins (multiplier 1), never rounded:
//! a qty that is not a multiple of `qtyStep` is refused.
//!
//! Venue order id = our `orderLinkId` (derived from the order's timestamp + salt), so a lost
//! acknowledgement is reconciled by lookup, never by resubmission; a resubmission would be rejected
//! by Bybit as a duplicate orderLinkId (110072), which we reconcile too.
//! Safety: ONLY a testnet constructor; host is a constant.

use crate::aevo_exec::{TestnetConfigError, hmac_signature};
use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result, invalid, opt_dec};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

pub const TESTNET_REST: &str = "https://api-testnet.bybit.com";
pub const TESTNET_NETWORK: &str = "bybit-testnet";
pub const RECV_WINDOW_MS: &str = "5000";
const KRAKEN_FX: &str = "https://api.kraken.com/0/public/Ticker?pair=USDTZUSD,USDCUSD";
const P: PlatformId = PlatformId::Bybit;
const ENV_KEY: &str = "OA_BYBIT_TESTNET_API_KEY";
const ENV_SECRET: &str = "OA_BYBIT_TESTNET_API_SECRET";

// ---------------------------------------------------------------- pure helpers

/// String signed by Bybit v5: timestamp + apiKey + recvWindow + (queryString | jsonBody).
pub fn sign_payload(ts_ms: &str, api_key: &str, recv_window: &str, query_or_body: &str) -> String {
    format!("{ts_ms}{api_key}{recv_window}{query_or_body}")
}

pub fn sign(secret: &str, ts_ms: &str, api_key: &str, recv_window: &str, qb: &str) -> String {
    hmac_signature(secret, &sign_payload(ts_ms, api_key, recv_window, qb))
}

/// Query string exactly as sent AND signed (insertion order kept; RFC 3986 unreserved kept as is).
pub fn query_string(params: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    params
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Deterministic orderLinkId (<= 36 chars, [A-Za-z0-9-]): "oa-<timestamp_s>-<salt>".
pub fn order_link_id(o: &ExecOrder) -> Option<String> {
    let id = format!("oa-{}-{}", o.timestamp, o.salt);
    (id.len() <= 36
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

fn meta_dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// USDC per 1 USDT frozen in the instrument (`venue_meta.fx.usdc_per_usdt`). Required; must be
/// within +-2 % of 1 (same sanity band as `fx::usdc_per_usdt`).
pub fn fx_rate(meta: &Value) -> Option<Decimal> {
    let r = meta_dec(&meta["fx"]["usdc_per_usdt"])?;
    (r > Decimal::ZERO && (r - Decimal::ONE).abs() <= Decimal::new(2, 2)).then_some(r)
}

/// Documented Bybit defaults (instruments-info, 2026-10-06), used only when the frozen metadata
/// lacks them: (tickSize USDT, qtyStep coins).
fn default_filters(symbol: &str) -> Option<(Decimal, Decimal)> {
    match symbol.split('-').next()? {
        "BTC" => Some((Decimal::from(5), Decimal::new(1, 2))),
        "ETH" => Some((Decimal::new(1, 1), Decimal::new(1, 1))),
        _ => None,
    }
}

/// USDC limit (per coin) -> USDT limit on the tick grid. Buy rounds DOWN, sell rounds UP (never
/// more aggressive than the engine's limit). `None` if the result is not strictly positive.
pub fn usdt_limit(
    limit_usdc: Decimal,
    rate: Decimal,
    tick: Decimal,
    side: Side,
) -> Option<Decimal> {
    if rate <= Decimal::ZERO || tick <= Decimal::ZERO || limit_usdc <= Decimal::ZERO {
        return None;
    }
    let raw = limit_usdc / rate;
    let steps = raw / tick;
    let steps = match side {
        Side::Buy => steps.floor(),
        Side::Sell => steps.ceil(),
    };
    let px = (steps * tick).normalize();
    (px > Decimal::ZERO).then_some(px)
}

/// USDT price -> USDC (same direction as `fx::convert_book`).
pub fn usdc_price(usdt: Decimal, rate: Decimal) -> Decimal {
    (usdt * rate).round_dp(8)
}

/// Quantity must be a positive exact multiple of the step (never silently rounded).
pub fn check_qty(qty: Decimal, step: Decimal) -> bool {
    qty > Decimal::ZERO && step > Decimal::ZERO && (qty / step).fract().is_zero()
}

/// Order body for /v5/order/create (pure; the caller signs the exact serialised string).
pub fn order_body(o: &ExecOrder) -> std::result::Result<(Value, Decimal), String> {
    let rate = fx_rate(&o.venue_meta).ok_or("missing/invalid venue_meta.fx.usdc_per_usdt")?;
    let defaults = default_filters(&o.native_id);
    let tick = meta_dec(&o.venue_meta["tick_size"])
        .or(defaults.map(|d| d.0))
        .ok_or("unknown tick size")?;
    let step = meta_dec(&o.venue_meta["qty_step"])
        .or_else(|| meta_dec(&o.venue_meta["amount_step"]))
        .or(defaults.map(|d| d.1))
        .ok_or("unknown qty step")?;
    if !check_qty(o.qty_native, step) {
        return Err(format!(
            "qty {} is not a multiple of qtyStep {step}",
            o.qty_native
        ));
    }
    let px = usdt_limit(o.limit_price_native, rate, tick, o.side).ok_or("limit price")?;
    let link = order_link_id(o).ok_or("orderLinkId")?;
    Ok((
        json!({
            "category": "option",
            "symbol": o.native_id,
            "side": match o.side { Side::Buy => "Buy", Side::Sell => "Sell" },
            "orderType": "Limit",
            "qty": o.qty_native.normalize().to_string(),
            "price": px.to_string(),
            "timeInForce": match o.time_in_force { TimeInForce::Ioc => "IOC", TimeInForce::Gtc => "GTC" },
            "orderLinkId": link,
        }),
        rate,
    ))
}

/// Bybit answers HTTP 200 with a `retCode`. Definite rejections -> Http 400 (nothing created),
/// server-side trouble or unknown codes -> Http 500 (outcome unknown, reconcile by lookup).
pub fn map_ret_code(code: i64, msg: &str) -> Option<ConnectorError> {
    let body: String = format!("retCode {code}: {msg}").chars().take(300).collect();
    let status = match code {
        0 => return None,
        10006 | 10018 => {
            return Some(ConnectorError::RateLimited {
                platform: P,
                retry_after: None,
            });
        }
        10000 | 10016 => 500, // server timeout / internal error: the order may exist
        10001..=10099 => 400, // params, timestamp, key, signature, permission, IP
        110_000..=110_999 => 400, // order / position business rejections
        _ => 500,
    };
    Some(ConnectorError::Http {
        platform: P,
        status,
        body,
    })
}

/// One Bybit order object -> state; prices converted to USDC with `rate`.
pub fn parse_order(o: &Value, rate: Decimal) -> Result<VenueOrderState> {
    let link = o["orderLinkId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(P, format!("order without orderLinkId: {o}")))?;
    let filled = opt_dec(P, o.get("cumExecQty"), "cumExecQty")?.unwrap_or_default();
    let avg = opt_dec(P, o.get("avgPrice"), "avgPrice")?
        .filter(|d| *d > Decimal::ZERO)
        .map(|d| usdc_price(d, rate));
    let raw = o["orderStatus"].as_str().unwrap_or("");
    let part = |f: bool| {
        if filled > Decimal::ZERO {
            VenueStatus::PartiallyFilled
        } else if f {
            VenueStatus::Cancelled
        } else {
            VenueStatus::Open
        }
    };
    let (status, fin) = match raw {
        "New" | "Untriggered" | "Triggered" => (part(false), false),
        "PartiallyFilled" => (VenueStatus::PartiallyFilled, false),
        "Filled" => (VenueStatus::Filled, true),
        // IOC remainder cancelled: terminal (partial if anything executed).
        "Cancelled" | "PartiallyFilledCanceled" | "Deactivated" => (part(true), true),
        "Rejected" => (VenueStatus::Rejected, true),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    Ok(VenueOrderState {
        venue_order_id: link.to_string(),
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final: fin,
        raw: o.clone(),
    })
}

/// Spendable USDT in a UTA: min(account `totalAvailableBalance` (USD), USDT walletBalance - locked
/// - totalOrderIM - totalPositionIM), floored at 0. Conservative on purpose.
pub fn available_usdt(result: &Value) -> Result<(Decimal, Option<Decimal>, Value)> {
    let acct = result["list"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| invalid(P, "wallet-balance: empty list (no UNIFIED account?)"))?;
    let total_avail = opt_dec(
        P,
        acct.get("totalAvailableBalance"),
        "totalAvailableBalance",
    )?;
    let usdt = acct["coin"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["coin"].as_str() == Some("USDT"));
    let coin_free = match usdt {
        Some(c) => {
            let g = |k: &str| opt_dec(P, c.get(k), k).map(|x| x.unwrap_or_default());
            Some(g("walletBalance")? - g("locked")? - g("totalOrderIM")? - g("totalPositionIM")?)
        }
        None => None,
    };
    let avail = match (total_avail, coin_free) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a
            .or(b)
            .ok_or_else(|| invalid(P, "wallet-balance: no available balance field"))?,
    }
    .max(Decimal::ZERO);
    let equity = opt_dec(P, acct.get("totalEquity"), "totalEquity")?;
    let raw = json!({"accountType": acct["accountType"], "totalEquity": acct["totalEquity"],
        "totalAvailableBalance": acct["totalAvailableBalance"], "totalInitialMargin": acct["totalInitialMargin"],
        "usdt": usdt.map(|c| json!({"walletBalance": c["walletBalance"], "locked": c["locked"],
            "totalOrderIM": c["totalOrderIM"], "totalPositionIM": c["totalPositionIM"]}))});
    Ok((avail, equity, raw))
}

// ---------------------------------------------------------------- venue

struct Secret(String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

pub struct BybitTestnetExec {
    http: reqwest::Client,
    rest: String,
    api_key: Secret,
    api_secret: Secret,
    /// orderLinkId -> usdc_per_usdt frozen at submission (in memory; lost on restart).
    orders: Mutex<HashMap<String, Decimal>>,
}

impl fmt::Debug for BybitTestnetExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BybitTestnetExec")
            .field("rest", &self.rest)
            .field("api_key_prefix", &self.key_prefix())
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl BybitTestnetExec {
    /// Reads OA_BYBIT_TESTNET_API_KEY / OA_BYBIT_TESTNET_API_SECRET. Values are never logged.
    pub fn testnet_from_env() -> std::result::Result<Self, TestnetConfigError> {
        Self::testnet_from_lookup(|k| std::env::var(k).ok(), TESTNET_REST)
    }

    pub fn testnet_from_lookup(
        get: impl Fn(&str) -> Option<String>,
        rest: &str,
    ) -> std::result::Result<Self, TestnetConfigError> {
        let need = |k: &'static str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(TestnetConfigError::Missing(k))
        };
        let key = need(ENV_KEY)?;
        if !key.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(TestnetConfigError::Malformed(ENV_KEY));
        }
        Ok(Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            api_key: Secret(key),
            api_secret: Secret(need(ENV_SECRET)?),
            orders: Mutex::new(HashMap::new()),
        })
    }

    fn key_prefix(&self) -> String {
        format!("{}…", self.api_key.0.chars().take(4).collect::<String>())
    }

    fn remember(&self, link: &str, rate: Decimal) {
        if let Ok(mut m) = self.orders.lock() {
            m.insert(link.to_string(), rate);
        }
    }

    fn remembered(&self, link: &str) -> Option<Decimal> {
        self.orders.lock().ok()?.get(link).copied()
    }

    /// Live USDC/USDT rate (only when the frozen one is unknown, e.g. after a restart).
    async fn live_rate(&self) -> Result<Decimal> {
        let v = read_json(P, self.http.get(KRAKEN_FX).send().await).await?;
        crate::fx::usdc_per_usdt(&v, P)
    }

    /// Signed request; returns `result` when retCode == 0, typed error otherwise.
    async fn call(&self, method: reqwest::Method, path: &str, qb: String) -> Result<Value> {
        let ts = chrono::Utc::now().timestamp_millis().to_string();
        let sig = sign(
            &self.api_secret.0,
            &ts,
            &self.api_key.0,
            RECV_WINDOW_MS,
            &qb,
        );
        let is_get = method == reqwest::Method::GET;
        let url = if is_get && !qb.is_empty() {
            format!("{}{}?{}", self.rest, path, qb)
        } else {
            format!("{}{}", self.rest, path)
        };
        let mut rb = self
            .http
            .request(method, url)
            .header("X-BAPI-API-KEY", &self.api_key.0)
            .header("X-BAPI-TIMESTAMP", &ts)
            .header("X-BAPI-RECV-WINDOW", RECV_WINDOW_MS)
            .header("X-BAPI-SIGN", sig);
        if !is_get {
            rb = rb.header("content-type", "application/json").body(qb);
        }
        let v = read_json(P, rb.send().await).await?;
        let code = v["retCode"]
            .as_i64()
            .ok_or_else(|| invalid(P, format!("{path}: no retCode")))?;
        if let Some(e) = map_ret_code(code, v["retMsg"].as_str().unwrap_or("")) {
            return Err(e);
        }
        Ok(v["result"].clone())
    }

    async fn find(&self, path: &str, link: &str) -> Result<Option<Value>> {
        let q = query_string(&[("category", "option"), ("orderLinkId", link)]);
        let r = self.call(reqwest::Method::GET, path, q).await?;
        Ok(r["list"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|o| o["orderLinkId"].as_str() == Some(link))
            .cloned())
    }

    async fn rate_for(&self, link: &str) -> Result<(Decimal, &'static str)> {
        match self.remembered(link) {
            Some(r) => Ok((r, "frozen at submission")),
            None => Ok((
                self.live_rate().await?,
                "live kraken (rate at submission unknown)",
            )),
        }
    }

    fn ack_state(link: &str, raw: Value) -> VenueOrderState {
        VenueOrderState {
            venue_order_id: link.to_string(),
            status: VenueStatus::Open,
            filled_native: Decimal::ZERO,
            avg_price_native: None,
            is_final: false,
            raw,
        }
    }
}

#[async_trait]
impl ExecutionVenue for BybitTestnetExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        TESTNET_NETWORK
    }

    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        order_link_id(order)
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let (body, rate) = order_body(o).map_err(|d| crate::not_representable(P, d))?;
        let link = body["orderLinkId"].as_str().unwrap_or_default().to_string();
        self.remember(&link, rate);
        match self
            .call(reqwest::Method::POST, "/v5/order/create", body.to_string())
            .await
        {
            Ok(ack) => {
                // Asynchronous ack: read the state back; if not visible yet, report it as open.
                match self.get_order(&link).await {
                    Ok(Some(s)) => Ok(s),
                    _ => Ok(Self::ack_state(&link, json!({"ack": ack}))),
                }
            }
            // Duplicate orderLinkId: the order already exists (lost ack) -> reconcile, never resubmit.
            Err(ConnectorError::Http {
                status: 400, body, ..
            }) if body.starts_with("retCode 110072") => match self.get_order(&link).await? {
                Some(s) => Ok(s),
                None => Ok(Self::ack_state(&link, json!({"duplicate": body}))),
            },
            Err(e) => Err(e),
        }
    }

    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        let found = match self.find("/v5/order/realtime", id).await? {
            Some(o) => Some(o),
            None => self.find("/v5/order/history", id).await?,
        };
        let Some(o) = found else { return Ok(None) };
        let (rate, src) = self.rate_for(id).await?;
        let mut s = parse_order(&o, rate)?;
        if let Some(m) = s.raw.as_object_mut() {
            m.insert("fx_usdc_per_usdt".into(), json!(rate.to_string()));
            m.insert("fx_source".into(), json!(src));
        }
        Ok(Some(s))
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        let current =
            self.get_order(id)
                .await?
                .ok_or_else(|| ConnectorError::UnknownInstrument {
                    platform: P,
                    native_id: id.into(),
                })?;
        if current.is_final {
            return Ok(current);
        }
        let symbol = current.raw["symbol"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let body = json!({"category": "option", "symbol": symbol, "orderLinkId": id});
        match self
            .call(reqwest::Method::POST, "/v5/order/cancel", body.to_string())
            .await
        {
            Ok(_) => {}
            // Already filled/cancelled between our read and the cancel: re-read below.
            Err(ConnectorError::Http {
                status: 400, body, ..
            }) if body.starts_with("retCode 110001") => {}
            Err(e) => return Err(e),
        }
        Ok(self.get_order(id).await?.unwrap_or(current))
    }

    fn identity(&self) -> Value {
        json!({"venue": TESTNET_NETWORK, "rest": self.rest, "api_key_prefix": self.key_prefix(),
               "account": "Unified Trading Account required for options"})
    }

    /// GET /v5/account/wallet-balance?accountType=UNIFIED&coin=USDT. Spendable USDT (see
    /// `available_usdt`) converted to USDC at the live Kraken rate, so the engine's budget is in
    /// the same currency as the converted premiums.
    async fn balance(&self) -> Result<VenueBalance> {
        let q = query_string(&[("accountType", "UNIFIED"), ("coin", "USDT")]);
        let r = self
            .call(reqwest::Method::GET, "/v5/account/wallet-balance", q)
            .await?;
        let (avail, equity, mut raw) = available_usdt(&r)?;
        let rate = self.live_rate().await?;
        if let Some(m) = raw.as_object_mut() {
            m.insert("usdc_per_usdt".into(), json!(rate.to_string()));
        }
        Ok(VenueBalance {
            available: usdc_price(avail, rate),
            equity: equity.map(|e| usdc_price(e, rate)),
            currency: "USDC".into(),
            raw,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn order(side: Side, qty: Decimal, limit: Decimal, meta: Value) -> ExecOrder {
        ExecOrder {
            native_id: "BTC-30OCT26-98000-C-USDT".into(),
            venue_instrument_id: None,
            venue_meta: meta,
            max_fee_per_unit: None,
            side,
            qty_native: qty,
            limit_price_native: limit,
            salt: 18_446_744_073_709_551_615,
            timestamp: 1_791_210_000,
            time_in_force: TimeInForce::Ioc,
        }
    }

    /// Independent vectors computed with Python's stdlib:
    /// ```text
    /// import hmac,hashlib
    /// hmac.new(b'YYYYYYYYYY', ('1658384314791'+'XXXXXXXXXX'+'5000'
    ///          +'category=option&symbol=BTC-29JUL22-25000-C').encode(), hashlib.sha256).hexdigest()
    /// hmac.new(b'testsecret', ('1700000000000'+'testkey'+'5000'
    ///          +'{"category":"option","symbol":"BTC-30OCT26-98000-C-USDT"}').encode(), hashlib.sha256).hexdigest()
    /// ```
    /// The GET payload is the docs' own example string (its secret is not published, hence a
    /// stand-in secret; the doc's output 410e0f38… cannot be reproduced without it).
    #[test]
    fn signature_matches_independent_python_hmac() {
        let q = query_string(&[("category", "option"), ("symbol", "BTC-29JUL22-25000-C")]);
        assert_eq!(
            sign_payload("1658384314791", "XXXXXXXXXX", "5000", &q),
            "1658384314791XXXXXXXXXX5000category=option&symbol=BTC-29JUL22-25000-C"
        );
        assert_eq!(
            sign("YYYYYYYYYY", "1658384314791", "XXXXXXXXXX", "5000", &q),
            "37813c67fafb3017e92354eb88f218e7e52a98f9f5eb74cfcf0b21f17edb143b"
        );
        assert_eq!(
            sign(
                "testsecret",
                "1700000000000",
                "testkey",
                "5000",
                r#"{"category":"option","symbol":"BTC-30OCT26-98000-C-USDT"}"#
            ),
            "b7b21343cf9c4c22a0d52e56bd79b557873d4cd673647fe2693c323156adba6c"
        );
    }

    #[test]
    fn query_string_encodes_only_reserved_chars() {
        assert_eq!(
            query_string(&[("a", "B-1_x.y~"), ("c", "d e&f")]),
            "a=B-1_x.y~&c=d%20e%26f"
        );
    }

    /// Worked examples. rate 0.99986 USDC/USDT, BTC tick 5 USDT.
    /// Buy limit 1000 USDC -> 1000/0.99986 = 1000.14002 USDT -> floor to 1000 (pays at most
    /// 1000*0.99986 = 999.86 USDC <= 1000). Sell limit 1000 USDC -> ceil to 1005 USDT.
    /// Rate 1.0002 (USDT above USDC): buy 1000 USDC -> 999.80 USDT -> 995.
    #[test]
    fn price_conversion_is_never_more_aggressive() {
        let r = dec!(0.99986);
        assert_eq!(
            usdt_limit(dec!(1000), r, dec!(5), Side::Buy),
            Some(dec!(1000))
        );
        assert_eq!(
            usdt_limit(dec!(1000), r, dec!(5), Side::Sell),
            Some(dec!(1005))
        );
        assert_eq!(
            usdt_limit(dec!(1000), dec!(1.0002), dec!(5), Side::Buy),
            Some(dec!(995))
        );
        assert_eq!(
            usdt_limit(dec!(1000), dec!(1.0002), dec!(5), Side::Sell),
            Some(dec!(1000))
        );
        // ETH tick 0.1: 25.00 USDC / 0.99986 = 25.0035 -> buy 25.0, sell 25.1
        assert_eq!(
            usdt_limit(dec!(25), r, dec!(0.1), Side::Buy),
            Some(dec!(25))
        );
        assert_eq!(
            usdt_limit(dec!(25), r, dec!(0.1), Side::Sell),
            Some(dec!(25.1))
        );
        // Buy below one tick -> nothing representable.
        assert_eq!(usdt_limit(dec!(4), r, dec!(5), Side::Buy), None);
        for (lim, side) in [(dec!(1234.56), Side::Buy), (dec!(1234.56), Side::Sell)] {
            let px = usdt_limit(lim, r, dec!(5), side).unwrap();
            let back = px * r;
            match side {
                Side::Buy => assert!(back <= lim),
                Side::Sell => assert!(back >= lim),
            }
        }
        // Fill at 1000 USDT -> 999.86 USDC.
        assert_eq!(usdc_price(dec!(1000), r), dec!(999.86));
    }

    #[test]
    fn order_body_requires_fx_and_exact_qty() {
        let meta = json!({"fx": {"usdc_per_usdt": "0.99986"}, "tick_size": "5"});
        let (b, rate) = order_body(&order(Side::Buy, dec!(0.3), dec!(1000), meta.clone())).unwrap();
        assert_eq!(rate, dec!(0.99986));
        assert_eq!(
            b,
            json!({"category": "option", "symbol": "BTC-30OCT26-98000-C-USDT", "side": "Buy",
                   "orderType": "Limit", "qty": "0.3", "price": "1000", "timeInForce": "IOC",
                   "orderLinkId": "oa-1791210000-18446744073709551615"})
        );
        assert!(b["orderLinkId"].as_str().unwrap().len() <= 36);
        // No fx rate -> refused (never assume 1 USDT = 1 USDC).
        assert!(
            order_body(&order(
                Side::Buy,
                dec!(0.3),
                dec!(1000),
                json!({"tick_size": "5"})
            ))
            .is_err()
        );
        // De-pegged rate -> refused.
        assert!(
            order_body(&order(
                Side::Buy,
                dec!(0.3),
                dec!(1000),
                json!({"fx": {"usdc_per_usdt": "0.95"}})
            ))
            .is_err()
        );
        // qty not a multiple of 0.01 -> refused, not rounded.
        assert!(order_body(&order(Side::Buy, dec!(0.305), dec!(1000), meta.clone())).is_err());
        // tick_size null in meta -> documented BTC default (5) applies.
        let m2 = json!({"fx": {"usdc_per_usdt": "0.99986"}, "tick_size": null});
        let (b2, _) = order_body(&order(Side::Sell, dec!(0.01), dec!(1000), m2)).unwrap();
        assert_eq!(b2["price"], "1005");
        assert_eq!(b2["side"], "Sell");
    }

    #[test]
    fn status_mapping_never_invents_finality() {
        let s = |st: &str, q: &str| {
            parse_order(
                &json!({"orderLinkId": "oa-1", "symbol": "X", "orderStatus": st, "cumExecQty": q, "avgPrice": "1000"}),
                dec!(0.99986),
            )
            .unwrap()
        };
        let f = s("Filled", "0.3");
        assert_eq!((f.status, f.is_final), (VenueStatus::Filled, true));
        assert_eq!(f.avg_price_native, Some(dec!(999.86)));
        let p = s("Cancelled", "0.1");
        assert_eq!((p.status, p.is_final), (VenueStatus::PartiallyFilled, true));
        assert_eq!(s("Cancelled", "0").status, VenueStatus::Cancelled);
        assert_eq!(
            s("PartiallyFilledCanceled", "0.1").status,
            VenueStatus::PartiallyFilled
        );
        assert_eq!(s("Rejected", "0").status, VenueStatus::Rejected);
        let n = s("New", "0");
        assert_eq!((n.status, n.is_final), (VenueStatus::Open, false));
        let pf = s("PartiallyFilled", "0.1");
        assert_eq!(
            (pf.status, pf.is_final),
            (VenueStatus::PartiallyFilled, false)
        );
        let u = s("Weird", "0");
        assert_eq!(
            (u.status, u.is_final),
            (VenueStatus::Unknown("Weird".into()), false)
        );
        // avgPrice "0" / "" -> no average.
        let z = parse_order(
            &json!({"orderLinkId": "a", "orderStatus": "New", "cumExecQty": "0", "avgPrice": ""}),
            dec!(1),
        )
        .unwrap();
        assert_eq!(z.avg_price_native, None);
        assert!(parse_order(&json!({"orderStatus": "Filled"}), dec!(1)).is_err());
    }

    #[test]
    fn ret_codes_split_definite_from_unknown() {
        let st = |c| match map_ret_code(c, "m") {
            Some(ConnectorError::Http { status, .. }) => status,
            Some(ConnectorError::RateLimited { .. }) => 429,
            _ => 0,
        };
        assert_eq!(st(0), 0);
        assert_eq!(st(10004), 400, "bad signature: nothing created");
        assert_eq!(st(110007), 400, "insufficient balance");
        assert_eq!(st(10016), 500, "internal error: outcome unknown");
        assert_eq!(st(10000), 500);
        assert_eq!(
            st(999_999),
            500,
            "unknown code is never a definite rejection"
        );
        assert_eq!(st(10006), 429);
    }

    #[test]
    fn balance_is_conservative() {
        let r = json!({"list": [{"accountType": "UNIFIED", "totalEquity": "1500", "totalAvailableBalance": "1200",
            "coin": [{"coin": "USDT", "walletBalance": "1000", "locked": "0", "totalOrderIM": "50", "totalPositionIM": "100"}]}]});
        let (a, e, raw) = available_usdt(&r).unwrap();
        assert_eq!((a, e), (dec!(850), Some(dec!(1500))));
        assert_eq!(raw["usdt"]["walletBalance"], "1000");
        assert!(available_usdt(&json!({"list": []})).is_err());
    }

    #[test]
    fn config_requires_both_secrets_and_never_prints_them() {
        let env = |m: &'static [(&'static str, &'static str)]| {
            move |k: &str| m.iter().find(|(a, _)| *a == k).map(|(_, b)| b.to_string())
        };
        const FULL: &[(&str, &str)] = &[
            ("OA_BYBIT_TESTNET_API_KEY", "AbCdSECRETKEY123"), // gitleaks:allow
            ("OA_BYBIT_TESTNET_API_SECRET", "s3cr3tVALUEzz"),
        ];
        let e = BybitTestnetExec::testnet_from_lookup(env(FULL), TESTNET_REST).unwrap();
        let dbg = format!("{e:?} {}", e.identity());
        assert!(
            !dbg.contains("SECRETKEY") && !dbg.contains("s3cr3t"),
            "{dbg}"
        );
        assert!(dbg.contains("AbCd"), "key prefix shown for diagnostics");
        assert_eq!(e.network(), "bybit-testnet");
        assert_eq!(e.identity()["venue"], "bybit-testnet");
        assert_eq!(
            BybitTestnetExec::testnet_from_lookup(env(&FULL[..1]), TESTNET_REST).unwrap_err(),
            TestnetConfigError::Missing("OA_BYBIT_TESTNET_API_SECRET")
        );
        let o = order(Side::Buy, dec!(0.3), dec!(1000), Value::Null);
        assert_eq!(
            e.precompute_order_id(&o).as_deref(),
            Some("oa-1791210000-18446744073709551615")
        );
    }
}
