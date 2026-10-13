//! Deribit TESTNET execution (`https://test.deribit.com/api/v2`), USDC-settled LINEAR options only.
//!
//! Sources (read 2026-10-06, docs.deribit.com):
//! - `public/auth` grant_type=client_credentials {client_id, client_secret} or grant_type=refresh_token
//!   {refresh_token} -> {access_token, expires_in (SECONDS), refresh_token, scope, token_type "bearer"};
//!   HTTP private calls carry `Authorization: bearer <access_token>`.
//! - `private/buy` / `private/sell` {instrument_name, amount (options: BASE currency units), type
//!   "limit", price, time_in_force ("good_til_cancelled" | "good_til_day" | "fill_or_kill" |
//!   "immediate_or_cancel"), label (<= 64 chars), post_only, reduce_only} -> {order, trades}.
//!   order_state in {open, filled, rejected, cancelled, untriggered, triggered}.
//! - `private/get_order_state_by_label` {currency, label} -> ARRAY of orders; only open orders and
//!   orders closed in the last 30 minutes. `private/get_order_state` {order_id} has the same 30 min
//!   window; older orders are found by `private/get_order_history_by_currency` (we scan it as a fallback).
//! - `private/cancel` {order_id} -> the order object.
//! - `private/get_account_summary` {currency}: `available_funds` = max(0, margin_balance -
//!   initial_margin) ("funds available to open or enlarge positions"), `equity` = balance + futures
//!   session PnL + options mark value.
//! - Errors (docs.deribit.com/articles/errors): JSON-RPC `error {code, message, data}`, sent over
//!   HTTP 400 (verified live on the testnet). 10004 order_not_found, 11044 not_open_order,
//!   10009 not_enough_funds, 10028 too_many_requests, 13004 invalid_credentials, 13009 unauthorized,
//!   10040 retry, 10047 matching_engine_queue_full, 11094 internal_server_error.
//!
//! Units: linear USDC options have contract_size 1 (verified live on testnet + mainnet), so
//! `ExecOrder.qty_native` = amount in underlying, `limit_price_native` = USDC per underlying = Deribit
//! `price`. Prices are rounded to the tick CONSERVATIVELY (buy down, sell up), honouring
//! `tick_size_steps` (testnet BTC_USDC: tick 5, 20 above 1000).
//!
//! Idempotency: the order `label` is derived from the stored order (`oa-<timestamp>-<salt>`), so it is
//! the precomputed venue id; a lost acknowledgement is reconciled by label, never by resubmitting.
//!
//! Safety: ONLY a testnet constructor; every response must carry `"testnet": true` (Deribit adds it
//! on the test environment) or it is refused. Secrets travel only in POST bodies / headers (never in a
//! URL that could end up in an error message) and are redacted from Debug and identity().
//!
//! TESTNET READ CONNECTOR (not wired here, the lead owns deribit.rs): the public read API is identical
//! on the testnet (verified live 2026-10-06: `public/get_instruments?currency=USDC&kind=option` -> 3,056
//! linear USDC options, 618 BTC_USDC + 494 ETH_USDC; `get_book_summary_by_currency` -> 566 BTC_USDC and
//! 359 ETH_USDC two-sided books). `DeribitConnector::new(TESTNET_REST)` already parses it, BUT its
//! `network()` and instrument keys use the mainnet label `deribit-usdc-linear`. A
//! `DeribitConnector::testnet()` must therefore carry a `network` field set to [`TESTNET_NETWORK`]
//! (as `AevoConnector::testnet()` does), used both in `network()` and in `parse_instrument`'s key,
//! otherwise the execution service's network check refuses (or worse, mislabels) the legs.

use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{REQUEST_TIMEOUT, client};
use crate::{ConnectorError, Result, opt_dec};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const TESTNET_REST: &str = "https://test.deribit.com/api/v2";
/// Network label for testnet instruments AND this venue (a testnet read connector must use it).
pub const TESTNET_NETWORK: &str = "deribit-testnet";
pub const ENV_CLIENT_ID: &str = "OA_DERIBIT_TESTNET_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "OA_DERIBIT_TESTNET_CLIENT_SECRET";
/// Linear options are listed and looked up under the USDC currency.
const CURRENCY: &str = "USDC";
pub const LABEL_PREFIX: &str = "oa-";
const LABEL_MAX: usize = 64;
/// Refresh the access token this long before it expires (capped at 1/5 of its lifetime).
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
const HISTORY_SCAN: u32 = 200;
const P: PlatformId = PlatformId::Deribit;

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

fn not_representable(d: impl std::fmt::Display) -> ConnectorError {
    // The execution service treats this prefix as a definite rejection (nothing was sent).
    crate::not_representable(P, d)
}

/// Deterministic, unique order label (<= 64 chars): the precomputed venue id.
pub fn label_for(o: &ExecOrder) -> String {
    let l = format!("{LABEL_PREFIX}{}-{}", o.timestamp, o.salt);
    debug_assert!(l.len() <= LABEL_MAX);
    l
}

/// Tick schedule of an instrument: base tick + `tick_size_steps` ({above_price, tick_size}).
#[derive(Debug, Clone, PartialEq)]
pub struct TickSchedule {
    pub base: Decimal,
    /// (above_price, tick_size), sorted by above_price.
    pub steps: Vec<(Decimal, Decimal)>,
}

impl TickSchedule {
    /// From the frozen instrument metadata (`tick_size` inserted by the engine, `tick_size_steps`
    /// copied from Deribit's instrument). Missing/invalid tick -> refused (never guessed).
    pub fn from_meta(meta: &Value) -> Result<Self> {
        let base = opt_dec(P, meta.get("tick_size"), "tick_size")
            .map_err(not_representable)?
            .filter(|t| *t > Decimal::ZERO)
            .ok_or_else(|| not_representable("missing tick_size"))?;
        let mut steps = vec![];
        for s in meta["tick_size_steps"].as_array().into_iter().flatten() {
            let above =
                opt_dec(P, s.get("above_price"), "above_price").map_err(not_representable)?;
            let tick = opt_dec(P, s.get("tick_size"), "tick_size").map_err(not_representable)?;
            match (above, tick) {
                (Some(a), Some(t)) if t > Decimal::ZERO => steps.push((a, t)),
                _ => return Err(not_representable(format!("bad tick_size_steps entry {s}"))),
            }
        }
        steps.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self { base, steps })
    }

    /// Tick applying at `price` ("above_price: the price from which the increased tick applies").
    pub fn tick_at(&self, price: Decimal) -> Decimal {
        self.steps
            .iter()
            .rev()
            .find(|(above, _)| price >= *above)
            .map(|(_, t)| *t)
            .unwrap_or(self.base)
    }

    /// Round conservatively: BUY down (never pay more than the limit), SELL up (never receive less).
    /// Returns None when the result is not a positive price on the grid.
    pub fn round(&self, price: Decimal, side: Side) -> Option<Decimal> {
        let mut p = price;
        for _ in 0..4 {
            let t = self.tick_at(p);
            let q = p / t;
            let r = match side {
                Side::Buy => q.floor(),
                Side::Sell => q.ceil(),
            } * t;
            if r <= Decimal::ZERO {
                return None;
            }
            if (r / self.tick_at(r)).fract().is_zero() {
                return Some(r.normalize());
            }
            p = r; // landed across a step boundary off-grid: round again with the new tick
        }
        None
    }
}

/// Decimal -> JSON number (Deribit wants numbers). Values here are short (ticks, 0.01 lots).
fn num(d: Decimal) -> Value {
    serde_json::from_str(&d.normalize().to_string()).unwrap_or(Value::Null)
}

/// Builds (method, params) of the order. Refuses anything that is not a linear USDC option priced
/// per 1 underlying, a non-positive or off-lot amount, or a missing tick.
pub fn order_request(o: &ExecOrder) -> Result<(&'static str, Value)> {
    if !o.native_id.contains("_USDC-") {
        return Err(not_representable(format!(
            "{} is not a linear USDC option (inverse options are priced in coin)",
            o.native_id
        )));
    }
    if let Some(m) = opt_dec(
        P,
        o.venue_meta.get("contract_multiplier"),
        "contract_multiplier",
    )
    .map_err(not_representable)?
        && m != Decimal::ONE
    {
        return Err(not_representable(format!("contract multiplier {m} != 1")));
    }
    if o.qty_native <= Decimal::ZERO {
        return Err(not_representable("amount <= 0"));
    }
    if let Some(step) = opt_dec(P, o.venue_meta.get("min_trade_amount"), "min_trade_amount")
        .map_err(not_representable)?
        .filter(|s| *s > Decimal::ZERO)
        && (o.qty_native < step || !(o.qty_native / step).fract().is_zero())
    {
        return Err(not_representable(format!(
            "amount {} is not a multiple of min_trade_amount {step}",
            o.qty_native
        )));
    }
    let ticks = TickSchedule::from_meta(&o.venue_meta)?;
    let price = ticks
        .round(o.limit_price_native, o.side)
        .ok_or_else(|| not_representable(format!("price {} on tick grid", o.limit_price_native)))?;
    let method = match o.side {
        Side::Buy => "private/buy",
        Side::Sell => "private/sell",
    };
    Ok((
        method,
        json!({
            "instrument_name": o.native_id,
            "amount": num(o.qty_native),
            "type": "limit",
            "price": num(price),
            "time_in_force": match o.time_in_force {
                TimeInForce::Ioc => "immediate_or_cancel",
                TimeInForce::Gtc => "good_til_cancelled",
            },
            "label": label_for(o),
            "post_only": false,
            "reduce_only": false,
        }),
    ))
}

pub fn parse_order(o: &Value) -> Result<VenueOrderState> {
    let id = o["order_id"]
        .as_str()
        .ok_or_else(|| crate::invalid(P, format!("order without order_id: {o}")))?;
    let filled = opt_dec(P, o.get("filled_amount"), "filled_amount")?.unwrap_or_default();
    let avg = opt_dec(P, o.get("average_price"), "average_price")?.filter(|d| *d > Decimal::ZERO);
    let raw = o["order_state"].as_str().unwrap_or("").to_lowercase();
    let partial_or = |none: VenueStatus| {
        if filled > Decimal::ZERO {
            VenueStatus::PartiallyFilled
        } else {
            none
        }
    };
    let (status, is_final) = match raw.as_str() {
        "filled" => (VenueStatus::Filled, true),
        "open" | "untriggered" | "triggered" => (partial_or(VenueStatus::Open), false),
        // IOC remainder cancelled by the exchange: terminal (partial) fill.
        "cancelled" => (partial_or(VenueStatus::Cancelled), true),
        "rejected" => (partial_or(VenueStatus::Rejected), true),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    Ok(VenueOrderState {
        venue_order_id: id.to_string(),
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final,
        raw: o.clone(),
    })
}

/// JSON-RPC error -> typed error. 4xx (except 404/408/409/429) means "definitely nothing created";
/// 5xx means the outcome is unknown and must be reconciled.
pub fn map_rpc_error(err: &Value) -> ConnectorError {
    let code = err["code"].as_i64().unwrap_or(0);
    let body: String = format!(
        "deribit {code} {} {}",
        err["message"].as_str().unwrap_or(""),
        err.get("data").map(|d| d.to_string()).unwrap_or_default()
    )
    .chars()
    .take(300)
    .collect();
    let status = match code {
        10028 => {
            return ConnectorError::RateLimited {
                platform: P,
                retry_after: None,
            };
        }
        13004 | 13009 => 401,
        13021 => 403,
        10004 => 404,
        // "should be retried" / queue full / internal error: maybe processed -> unknown outcome.
        10040 | 10047 | 11094 | -32603 => 503,
        _ => 400,
    };
    ConnectorError::Http {
        platform: P,
        status,
        body,
    }
}

/// Envelope check: `"testnet": true` required, error mapped, `result` returned.
pub fn unwrap_envelope(http_status: u16, body: &str) -> Result<Value> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) if (200..300).contains(&http_status) => {
            return Err(crate::invalid(P, format!("not JSON: {e}")));
        }
        Err(_) => {
            return Err(ConnectorError::Http {
                platform: P,
                status: http_status,
                body: body.chars().take(300).collect(),
            });
        }
    };
    if v.get("testnet") != Some(&Value::Bool(true)) {
        return Err(crate::invalid(
            P,
            "response is not flagged testnet:true; refusing (testnet-only executor)",
        ));
    }
    if let Some(err) = v.get("error") {
        return Err(map_rpc_error(err));
    }
    if !(200..300).contains(&http_status) {
        return Err(ConnectorError::Http {
            platform: P,
            status: http_status,
            body: body.chars().take(300).collect(),
        });
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| crate::invalid(P, "no result"))
}

// ---------------------------------------------------------------------------------------------
// Token cache (pure decision logic, clock injected)
// ---------------------------------------------------------------------------------------------

/// Secret holder: never printed.
struct Secret(String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug)]
struct Token {
    access: Secret,
    refresh: Option<Secret>,
    /// Proactive refresh point (expiry minus margin).
    refresh_after: Instant,
    /// Hard expiry announced by the venue.
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAction {
    Use,
    Refresh,
    Authenticate,
}

fn token_action(t: Option<&Token>, now: Instant) -> TokenAction {
    match t {
        Some(t) if now < t.refresh_after => TokenAction::Use,
        Some(t) if now < t.expires_at && t.refresh.is_some() => TokenAction::Refresh,
        _ => TokenAction::Authenticate,
    }
}

fn token_from_auth(v: &Value, now: Instant) -> Result<Token> {
    let access = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| crate::invalid(P, "auth: no access_token"))?;
    let secs = v["expires_in"]
        .as_u64()
        .ok_or_else(|| crate::invalid(P, "auth: no expires_in"))?;
    let life = Duration::from_secs(secs);
    let margin = REFRESH_MARGIN.min(life / 5);
    Ok(Token {
        access: Secret(access.to_string()),
        refresh: v["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| Secret(s.to_string())),
        refresh_after: now + (life - margin),
        expires_at: now + life,
    })
}

// ---------------------------------------------------------------------------------------------
// Venue
// ---------------------------------------------------------------------------------------------

pub struct DeribitTestnetExec {
    http: reqwest::Client,
    rest: String,
    client_id: Secret,
    client_secret: Secret,
    token: tokio::sync::Mutex<Option<Token>>,
    next_id: AtomicU64,
}

impl fmt::Debug for DeribitTestnetExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeribitTestnetExec")
            .field("rest", &self.rest)
            .field("client_id", &client_id_prefix(&self.client_id.0))
            .field("keys", &"<redacted>")
            .finish()
    }
}

fn client_id_prefix(id: &str) -> String {
    format!("{}…", id.chars().take(4).collect::<String>())
}

impl DeribitTestnetExec {
    /// Reads OA_DERIBIT_TESTNET_{CLIENT_ID,CLIENT_SECRET}. Values are never logged.
    pub fn testnet_from_env() -> std::result::Result<Self, crate::aevo_exec::TestnetConfigError> {
        Self::testnet_from_lookup(|k| std::env::var(k).ok(), TESTNET_REST)
    }

    /// `rest` exists for tests (local fake server); production passes [`TESTNET_REST`]. Responses
    /// are refused unless flagged `testnet: true`, whatever the URL.
    pub fn testnet_from_lookup(
        get: impl Fn(&str) -> Option<String>,
        rest: &str,
    ) -> std::result::Result<Self, crate::aevo_exec::TestnetConfigError> {
        use crate::aevo_exec::TestnetConfigError as E;
        let need = |k: &'static str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or(E::Missing(k))
        };
        Ok(Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            client_id: Secret(need(ENV_CLIENT_ID)?),
            client_secret: Secret(need(ENV_CLIENT_SECRET)?),
            token: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// JSON-RPC over HTTP POST (params in the body, never in the URL).
    async fn post(&self, method: &str, params: Value, bearer: Option<&str>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut rb = self
            .http
            .post(format!("{}/{method}", self.rest))
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if let Some(t) = bearer {
            rb = rb.header("Authorization", format!("bearer {t}"));
        }
        let send_err = |e: reqwest::Error| {
            if e.is_timeout() {
                ConnectorError::Timeout {
                    platform: P,
                    after: REQUEST_TIMEOUT,
                }
            } else {
                ConnectorError::Transport {
                    platform: P,
                    source: Box::new(e.without_url()),
                }
            }
        };
        let resp = rb.send().await.map_err(send_err)?;
        let status = resp.status().as_u16();
        if status == 429 {
            return Err(ConnectorError::RateLimited {
                platform: P,
                retry_after: None,
            });
        }
        let body = resp.text().await.map_err(send_err)?;
        unwrap_envelope(status, &body)
    }

    /// Cached access token; refreshes before expiry, re-authenticates if the refresh fails.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        let now = Instant::now();
        let action = token_action(guard.as_ref(), now);
        if action == TokenAction::Use {
            return Ok(guard
                .as_ref()
                .expect("Use implies a token")
                .access
                .0
                .clone());
        }
        let refreshed = if action == TokenAction::Refresh {
            let rt = guard
                .as_ref()
                .and_then(|t| t.refresh.as_ref())
                .map(|s| s.0.clone())
                .unwrap_or_default();
            self.post(
                "public/auth",
                json!({"grant_type": "refresh_token", "refresh_token": rt}),
                None,
            )
            .await
            .ok()
        } else {
            None
        };
        let v = match refreshed {
            Some(v) => v,
            None => {
                self.post(
                    "public/auth",
                    json!({"grant_type": "client_credentials", "client_id": self.client_id.0, "client_secret": self.client_secret.0}),
                    None,
                )
                .await?
            }
        };
        let t = token_from_auth(&v, Instant::now())?;
        let access = t.access.0.clone();
        *guard = Some(t);
        Ok(access)
    }

    /// Authenticated call. On 13009 (expired/revoked token: the request was refused before any
    /// processing) the token is dropped and the call is retried ONCE with a fresh one.
    async fn private(&self, method: &str, params: Value) -> Result<Value> {
        let tok = self.access_token().await?;
        match self.post(method, params.clone(), Some(&tok)).await {
            Err(ConnectorError::Http {
                status: 401, body, ..
            }) if body.contains("13009") => {
                *self.token.lock().await = None;
                let tok = self.access_token().await?;
                self.post(method, params, Some(&tok)).await
            }
            r => r,
        }
    }

    fn find_in(list: &Value, key: &str, id: &str) -> Result<Option<VenueOrderState>> {
        let hits: Vec<&Value> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| o[key].as_str() == Some(id))
            .collect();
        let mut ids: Vec<&str> = hits.iter().filter_map(|o| o["order_id"].as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() > 1 {
            return Err(crate::invalid(
                P,
                format!("{key}={id} matches several orders: {ids:?}"),
            ));
        }
        hits.first().map(|o| parse_order(o)).transpose()
    }

    /// Fallback beyond the 30-minute window of the state endpoints.
    async fn scan_history(&self, key: &str, id: &str) -> Result<Option<VenueOrderState>> {
        let r = self
            .private(
                "private/get_order_history_by_currency",
                json!({"currency": CURRENCY, "kind": "option", "count": HISTORY_SCAN, "include_unfilled": true}),
            )
            .await?;
        Self::find_in(&r, key, id)
    }
}

#[async_trait]
impl ExecutionVenue for DeribitTestnetExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        TESTNET_NETWORK
    }

    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        Some(label_for(order))
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let (method, params) = order_request(o)?;
        let r = self.private(method, params).await?;
        parse_order(&r["order"])
    }

    /// `id` is either our label (`oa-…`, precomputed) or a Deribit order id.
    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        if id.starts_with(LABEL_PREFIX) {
            let r = self
                .private(
                    "private/get_order_state_by_label",
                    json!({"currency": CURRENCY, "label": id}),
                )
                .await?;
            if let Some(s) = Self::find_in(&r, "label", id)? {
                return Ok(Some(s));
            }
            return self.scan_history("label", id).await;
        }
        match self
            .private("private/get_order_state", json!({"order_id": id}))
            .await
        {
            Ok(r) => parse_order(&r).map(Some),
            Err(ConnectorError::Http { status: 404, .. }) => {
                self.scan_history("order_id", id).await
            }
            Err(e) => Err(e),
        }
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        let current = self.get_order(id).await?.ok_or_else(|| {
            crate::invalid(P, format!("cancel: order {id} not found on the venue"))
        })?;
        if current.is_final {
            return Ok(current);
        }
        match self
            .private(
                "private/cancel",
                json!({"order_id": current.venue_order_id}),
            )
            .await
        {
            Ok(r) => parse_order(&r),
            // 11044 not_open_order: it closed meanwhile (filled/cancelled) -> read its final state.
            Err(ConnectorError::Http {
                status: 400, body, ..
            }) if body.contains("11044") => Ok(self
                .get_order(&current.venue_order_id)
                .await?
                .unwrap_or(current)),
            Err(e) => Err(e),
        }
    }

    fn identity(&self) -> Value {
        json!({"venue": TESTNET_NETWORK, "rest": self.rest, "client_id_prefix": client_id_prefix(&self.client_id.0),
               "auth": "client_credentials", "currency": CURRENCY})
    }

    /// `private/get_account_summary` currency=USDC: available = `available_funds` (margin_balance -
    /// initial_margin, floored at 0), equity = `equity` (balance + futures session PnL + options mark
    /// value). Raw reduced to margin fields (no `extended` personal data requested).
    async fn balance(&self) -> Result<VenueBalance> {
        let r = self
            .private("private/get_account_summary", json!({"currency": CURRENCY}))
            .await?;
        let available = opt_dec(P, r.get("available_funds"), "available_funds")?
            .ok_or_else(|| crate::invalid(P, "account summary without available_funds"))?;
        Ok(VenueBalance {
            available,
            equity: opt_dec(P, r.get("equity"), "equity")?,
            currency: CURRENCY.into(),
            raw: json!({"currency": r["currency"], "balance": r["balance"], "equity": r["equity"],
                        "available_funds": r["available_funds"], "margin_balance": r["margin_balance"],
                        "initial_margin": r["initial_margin"], "maintenance_margin": r["maintenance_margin"]}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn meta() -> Value {
        // As frozen by the engine from the testnet instrument (live 2026-10-06).
        json!({"tick_size": "5", "tick_size_steps": [{"tick_size": 20.0, "above_price": 1000}],
               "contract_multiplier": "1", "min_trade_amount": 0.01, "instrument_id": 650913})
    }

    fn order(side: Side, qty: Decimal, px: Decimal) -> ExecOrder {
        ExecOrder {
            native_id: "BTC_USDC-7OCT26-75000-C".into(),
            venue_instrument_id: Some(650913),
            venue_meta: meta(),
            max_fee_per_unit: None,
            side,
            qty_native: qty,
            limit_price_native: px,
            salt: 123_456_789_012,
            timestamp: 1_791_287_289,
            time_in_force: TimeInForce::Ioc,
        }
    }

    #[test]
    fn tick_rounding_is_conservative_and_honours_steps() {
        let t = TickSchedule::from_meta(&meta()).unwrap();
        assert_eq!(t.tick_at(dec!(999)), dec!(5));
        assert_eq!(t.tick_at(dec!(1000)), dec!(20));
        assert_eq!(t.round(dec!(11213), Side::Buy), Some(dec!(11200)));
        assert_eq!(t.round(dec!(11201), Side::Sell), Some(dec!(11220)));
        assert_eq!(t.round(dec!(1003), Side::Buy), Some(dec!(1000)));
        assert_eq!(t.round(dec!(997), Side::Sell), Some(dec!(1000)));
        assert_eq!(t.round(dec!(12.4), Side::Buy), Some(dec!(10)));
        assert_eq!(t.round(dec!(12.4), Side::Sell), Some(dec!(15)));
        assert_eq!(
            t.round(dec!(11200), Side::Buy),
            Some(dec!(11200)),
            "on grid unchanged"
        );
        assert_eq!(
            t.round(dec!(3), Side::Buy),
            None,
            "rounds to zero -> refused"
        );
        // Boundary not on the coarse grid: sell lands on 1010 (>= above, tick 20) -> re-rounded.
        let odd = TickSchedule {
            base: dec!(5),
            steps: vec![(dec!(1010), dec!(20))],
        };
        assert_eq!(odd.round(dec!(1007), Side::Sell), Some(dec!(1020)));
        assert!(
            TickSchedule::from_meta(&json!({})).is_err(),
            "no tick -> refused"
        );
    }

    #[test]
    fn order_request_matches_the_documented_shape() {
        let (m, p) = order_request(&order(Side::Buy, dec!(0.1), dec!(11213.7))).unwrap();
        assert_eq!(m, "private/buy");
        assert_eq!(
            p,
            json!({"instrument_name": "BTC_USDC-7OCT26-75000-C", "amount": 0.1, "type": "limit",
                   "price": 11200, "time_in_force": "immediate_or_cancel",
                   "label": "oa-1791287289-123456789012", "post_only": false, "reduce_only": false})
        );
        assert!(p["label"].as_str().unwrap().len() <= LABEL_MAX);
        let (m, p) = order_request(&order(Side::Sell, dec!(1), dec!(11201))).unwrap();
        assert_eq!((m, p["price"].clone()), ("private/sell", json!(11220)));

        let refused = |o: ExecOrder| match order_request(&o) {
            Err(ConnectorError::InvalidResponse { detail, .. }) => {
                assert!(detail.starts_with("order not representable"), "{detail}")
            }
            other => panic!("{other:?}"),
        };
        refused(order(Side::Buy, dec!(0.015), dec!(100)));
        refused(order(Side::Buy, dec!(0.001), dec!(100)));
        refused(order(Side::Buy, dec!(0), dec!(100)));
        refused(order(Side::Buy, dec!(1), dec!(2)));
        let mut inverse = order(Side::Buy, dec!(1), dec!(100));
        inverse.native_id = "BTC-7OCT26-75000-C".into();
        refused(inverse);
        let mut mult = order(Side::Buy, dec!(1), dec!(100));
        mult.venue_meta["contract_multiplier"] = json!("10");
        refused(mult);
    }

    #[test]
    fn status_mapping_never_invents_finality() {
        let s = |st: &str, f: f64| {
            parse_order(&json!({"order_id": "USDC-1", "order_state": st, "filled_amount": f, "average_price": 0.0}))
                .unwrap()
        };
        let f = s("filled", 1.0);
        assert_eq!((f.status, f.is_final), (VenueStatus::Filled, true));
        let p = s("cancelled", 0.5);
        assert_eq!((p.status, p.is_final), (VenueStatus::PartiallyFilled, true));
        assert_eq!(s("cancelled", 0.0).status, VenueStatus::Cancelled);
        assert_eq!(s("rejected", 0.0).status, VenueStatus::Rejected);
        let o = s("open", 0.2);
        assert_eq!(
            (o.status, o.is_final),
            (VenueStatus::PartiallyFilled, false)
        );
        assert!(!s("untriggered", 0.0).is_final);
        let u = s("weird", 0.0);
        assert_eq!(
            (u.status, u.is_final),
            (VenueStatus::Unknown("weird".into()), false)
        );
        assert_eq!(
            s("filled", 1.0).avg_price_native,
            None,
            "0 average = no fill price"
        );
        assert!(parse_order(&json!({"order_state": "filled"})).is_err());
    }

    #[test]
    fn error_mapping_separates_definite_rejections_from_unknown_outcomes() {
        let st = |code: i64| match map_rpc_error(&json!({"code": code, "message": "m"})) {
            ConnectorError::Http { status, .. } => status,
            ConnectorError::RateLimited { .. } => 429,
            e => panic!("{e:?}"),
        };
        assert_eq!(st(10009), 400, "not_enough_funds: refused");
        assert_eq!(st(-32602), 400, "invalid params: refused");
        assert_eq!(st(13009), 401);
        assert_eq!(st(13004), 401);
        assert_eq!(st(10004), 404);
        assert_eq!(st(10028), 429);
        for c in [10040, 10047, 11094] {
            assert_eq!(st(c), 503, "{c}: outcome unknown");
        }
        // Envelope: testnet flag mandatory; error over HTTP 400 as observed live.
        let live = r#"{"jsonrpc":"2.0","error":{"code":13009,"data":{"reason":"invalid_token"},"message":"unauthorized"},"testnet":true}"#;
        assert!(
            matches!(unwrap_envelope(400, live), Err(ConnectorError::Http { status: 401, body, .. }) if body.contains("13009"))
        );
        assert!(unwrap_envelope(200, r#"{"result":1,"testnet":false}"#).is_err());
        assert!(unwrap_envelope(200, r#"{"result":1}"#).is_err());
        assert_eq!(
            unwrap_envelope(200, r#"{"result":1,"testnet":true}"#).unwrap(),
            json!(1)
        );
        assert!(matches!(
            unwrap_envelope(502, "<html>bad gateway</html>"),
            Err(ConnectorError::Http { status: 502, .. })
        ));
    }

    #[test]
    fn token_cache_refreshes_before_expiry() {
        let t0 = Instant::now();
        assert_eq!(token_action(None, t0), TokenAction::Authenticate);
        let tok = token_from_auth(
            &json!({"access_token": "A", "refresh_token": "R", "expires_in": 900, "token_type": "bearer"}),
            t0,
        )
        .unwrap();
        assert_eq!(token_action(Some(&tok), t0), TokenAction::Use);
        assert_eq!(
            token_action(Some(&tok), t0 + Duration::from_secs(839)),
            TokenAction::Use
        );
        assert_eq!(
            token_action(Some(&tok), t0 + Duration::from_secs(840)),
            TokenAction::Refresh
        );
        assert_eq!(
            token_action(Some(&tok), t0 + Duration::from_secs(900)),
            TokenAction::Authenticate
        );
        // Short lifetime: margin capped at 1/5 of it.
        let short = token_from_auth(&json!({"access_token": "A", "expires_in": 100}), t0).unwrap();
        assert_eq!(
            token_action(Some(&short), t0 + Duration::from_secs(79)),
            TokenAction::Use
        );
        assert_eq!(
            token_action(Some(&short), t0 + Duration::from_secs(80)),
            TokenAction::Authenticate,
            "no refresh token -> full auth"
        );
        assert!(token_from_auth(&json!({"expires_in": 10}), t0).is_err());
        let secret = token_from_auth(
            &json!({"access_token": "ACCESS-SECRET", "refresh_token": "REFRESH-SECRET", "expires_in": 900}),
            t0,
        )
        .unwrap();
        let dbg = format!("{secret:?}");
        assert!(
            !dbg.contains("SECRET") && dbg.contains("<redacted>"),
            "{dbg}"
        );
    }

    fn lookup(id: &str, secret: &str) -> impl Fn(&str) -> Option<String> {
        let (id, secret) = (id.to_string(), secret.to_string());
        move |k: &str| match k {
            ENV_CLIENT_ID => Some(id.clone()),
            ENV_CLIENT_SECRET => Some(secret.clone()),
            _ => None,
        }
    }

    #[test]
    fn config_requires_both_secrets_and_never_prints_them() {
        let e = DeribitTestnetExec::testnet_from_lookup(
            lookup("CLIDxyz123", "TOPSECRET42"),
            TESTNET_REST,
        )
        .unwrap();
        let dbg = format!("{e:?} {}", e.identity());
        assert!(
            !dbg.contains("TOPSECRET42") && !dbg.contains("xyz123"),
            "{dbg}"
        );
        assert!(dbg.contains("CLID…"));
        assert_eq!(e.network(), TESTNET_NETWORK);
        assert_eq!(e.identity()["venue"], "deribit-testnet");
        assert_eq!(
            DeribitTestnetExec::testnet_from_lookup(|_| None, TESTNET_REST).unwrap_err(),
            crate::aevo_exec::TestnetConfigError::Missing(ENV_CLIENT_ID)
        );
        let only_id = |k: &str| (k == ENV_CLIENT_ID).then(|| "x".to_string());
        assert_eq!(
            DeribitTestnetExec::testnet_from_lookup(only_id, TESTNET_REST).unwrap_err(),
            crate::aevo_exec::TestnetConfigError::Missing(ENV_CLIENT_SECRET)
        );
        assert_eq!(
            e.precompute_order_id(&order(Side::Buy, dec!(1), dec!(100)))
                .as_deref(),
            Some("oa-1791287289-123456789012")
        );
    }

    /// Local fake server: answers each connection with the next canned body, records requests.
    async fn fake(bodies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let rec = seen.clone();
        tokio::spawn(async move {
            for (status, body) in bodies {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![];
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len || n == 0 {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                rec.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(resp.as_bytes()).await.unwrap();
                let _ = s.shutdown().await;
            }
        });
        (format!("http://{addr}/api/v2"), seen)
    }

    #[tokio::test]
    async fn submit_authenticates_once_then_reuses_and_retries_on_expired_token() {
        let auth = |tok: &str| {
            (200, json!({"jsonrpc":"2.0","id":1,"testnet":true,"result":{"access_token":tok,"refresh_token":"R","expires_in":900,"scope":"trade:read_write","token_type":"bearer"}}).to_string())
        };
        let filled = json!({"jsonrpc":"2.0","id":2,"testnet":true,"result":{"order":{"order_id":"USDC-77","order_state":"filled","filled_amount":0.1,"average_price":11190.0,"label":"oa-1791287289-123456789012","instrument_name":"BTC_USDC-7OCT26-75000-C"},"trades":[]}}).to_string();
        let expired = r#"{"jsonrpc":"2.0","error":{"code":13009,"data":{"reason":"invalid_token"},"message":"unauthorized"},"testnet":true}"#.to_string();
        let (url, seen) = fake(vec![
            auth("TOK1"),
            (200, filled.clone()),
            (200, filled.clone()),
            (400, expired),
            auth("TOK2"),
            (200, filled),
        ])
        .await;
        let e = DeribitTestnetExec::testnet_from_lookup(lookup("cid", "SECRET-XYZ"), &url).unwrap();
        let o = order(Side::Buy, dec!(0.1), dec!(11213));
        for _ in 0..3 {
            let s = e.submit(&o).await.unwrap();
            assert_eq!(
                (s.status, s.filled_native),
                (VenueStatus::Filled, dec!(0.1))
            );
            assert_eq!(s.avg_price_native, Some(dec!(11190)));
        }
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 6);
        assert!(reqs[0].starts_with("POST /api/v2/public/auth "));
        assert!(reqs[0].contains("\"client_credentials\"") && reqs[0].contains("SECRET-XYZ"));
        assert!(reqs[1].starts_with("POST /api/v2/private/buy "));
        assert!(reqs[1].contains("authorization: bearer TOK1"));
        assert!(reqs[1].contains("\"price\":11200") && reqs[1].contains("immediate_or_cancel"));
        assert!(
            reqs[2].contains("bearer TOK1"),
            "token reused, no second auth"
        );
        assert!(reqs[4].contains("public/auth"), "13009 -> fresh auth");
        assert!(reqs[5].contains("bearer TOK2"));
        assert!(
            reqs.iter()
                .filter(|r| !r.contains("public/auth"))
                .all(|r| !r.contains("SECRET-XYZ")),
            "secret only in the auth body"
        );
    }

    #[tokio::test]
    async fn get_order_by_label_falls_back_to_history_and_rejections_are_typed() {
        let auth = (
            200,
            json!({"testnet":true,"result":{"access_token":"T","expires_in":900}}).to_string(),
        );
        let empty = (200, json!({"testnet":true,"result":[]}).to_string());
        let hist = (
            200,
            json!({"testnet":true,"result":[
            {"order_id":"USDC-1","label":"oa-other","order_state":"filled","filled_amount":1},
            {"order_id":"USDC-9","label":"oa-1-2","order_state":"cancelled","filled_amount":0}]})
            .to_string(),
        );
        let funds = (
            400,
            json!({"testnet":true,"error":{"code":10009,"message":"not_enough_funds"}}).to_string(),
        );
        let (url, seen) = fake(vec![auth, empty.clone(), hist, empty.clone(), empty, funds]).await;
        let e = DeribitTestnetExec::testnet_from_lookup(lookup("cid", "s"), &url).unwrap();
        let s = e.get_order("oa-1-2").await.unwrap().unwrap();
        assert_eq!(
            (s.venue_order_id.as_str(), s.status, s.is_final),
            ("USDC-9", VenueStatus::Cancelled, true)
        );
        assert!(e.get_order("oa-missing").await.unwrap().is_none());
        let r = e.submit(&order(Side::Buy, dec!(1), dec!(100))).await;
        assert!(
            matches!(r, Err(ConnectorError::Http { status: 400, .. })),
            "{r:?}"
        );
        let reqs = seen.lock().unwrap().clone();
        assert!(
            reqs[1].contains("private/get_order_state_by_label")
                && reqs[1].contains("\"currency\":\"USDC\"")
        );
        assert!(
            reqs[2].contains("get_order_history_by_currency")
                && reqs[2].contains("\"include_unfilled\":true")
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_testnet_lists_linear_usdc_options() {
        let c = crate::deribit::DeribitConnector::new(TESTNET_REST);
        use crate::Connector;
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            println!("deribit testnet {u}: {} linear USDC options", cat.len());
            assert!(!cat.is_empty());
        }
    }
}
