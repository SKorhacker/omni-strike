//! NON-CUSTODIAL Paradex execution: the END USER's Starknet account signs the order (SNIP-12 typed
//! data); its client obtains a short-lived JWT (5 min) with its own key; Omni Strike forwards one
//! order with them and never stores the JWT (redacted before journaling).
//!
//! Source: official SDK tradeparadex/paradex-py (main, read 2026-10-08):
//! - typed data `StarkNetDomain{name "Paradex", chainId = felt(short string of
//!   /system/config.starknet_chain_id), version "1"}`, primaryType `Order{timestamp (ms, nonce),
//!   market (short string), side ("1" buy / "2" sell), orderType ("LIMIT"), size, price}` with
//!   size/price as integers scaled 1e8 (`chain_size`, `chain_price`).
//! - POST /orders body `{market, side "BUY"|"SELL", size, type "LIMIT", price, instruction,
//!   client_id, signature, signature_timestamp}`; header `Authorization: Bearer <jwt>`.
//! - chain ids [LIVE /system/config]: mainnet PRIVATE_SN_PARACLEAR_MAINNET, testnet
//!   PRIVATE_SN_PARACLEAR_TESTNET.
//!
//! LIQUIDITY CAVEAT [LIVE 2026-10-08]: option books reachable by API orders were EMPTY on 25/25
//! sampled contracts (liquidity is Retail-Price-Improvement only). This executor is ready for when
//! standard liquidity exists or Paradex grants integrator access; IOC orders will not fill today.
//! The Starknet signature is verified by Paradex (we do not hold the user's public key).

use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result, opt_dec};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Paradex;

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

/// Short string -> felt hex (Cairo short string: big-endian ASCII, <= 31 chars).
pub fn short_string_felt(s: &str) -> Option<String> {
    (s.len() <= 31 && s.is_ascii()).then(|| format!("0x{}", hex::encode(s.as_bytes())))
}

fn e8(d: Decimal) -> Option<String> {
    let x = d * Decimal::from(100_000_000);
    (x.fract() == Decimal::ZERO && x >= Decimal::ZERO).then(|| x.normalize().to_string())
}

/// Typed data the user's Starknet wallet signs for this order.
pub fn order_typed_data(o: &ExecOrder, chain_id: &str) -> Result<Value> {
    let chain = short_string_felt(chain_id).ok_or_else(|| bad("chain id"))?;
    short_string_felt(&o.native_id).ok_or_else(|| bad("market name longer than 31 chars"))?;
    Ok(json!({
        "domain": {"name": "Paradex", "chainId": chain, "version": "1"},
        "primaryType": "Order",
        "types": {
            "StarkNetDomain": [{"name": "name", "type": "felt"}, {"name": "chainId", "type": "felt"}, {"name": "version", "type": "felt"}],
            "Order": [{"name": "timestamp", "type": "felt"}, {"name": "market", "type": "felt"}, {"name": "side", "type": "felt"},
                      {"name": "orderType", "type": "felt"}, {"name": "size", "type": "felt"}, {"name": "price", "type": "felt"}]
        },
        "message": {
            "timestamp": signature_timestamp(o).to_string(),
            "market": o.native_id,
            "side": if o.side == Side::Buy { "1" } else { "2" },
            "orderType": "LIMIT",
            "size": e8(o.qty_native).ok_or_else(|| bad("size: more than 8 decimals"))?,
            "price": e8(o.limit_price_native).ok_or_else(|| bad("price: more than 8 decimals"))?,
        }
    }))
}

/// Signature timestamp (ms) derived from the stored order: reproducible between prepare and submit.
pub fn signature_timestamp(o: &ExecOrder) -> i64 {
    o.timestamp * 1000 + (o.salt % 1000) as i64
}

pub fn parse_order(v: &Value) -> Result<VenueOrderState> {
    let id = v["id"]
        .as_str()
        .ok_or_else(|| bad(format!("order without id: {v}")))?;
    let size = opt_dec(P, v.get("size"), "size")?.unwrap_or_default();
    let remaining = opt_dec(P, v.get("remaining_size"), "remaining_size")?.unwrap_or(size);
    let filled = (size - remaining).max(Decimal::ZERO);
    let avg = opt_dec(P, v.get("avg_fill_price"), "avg_fill_price")?.filter(|d| *d > Decimal::ZERO);
    let (status, fin) = match v["status"].as_str().unwrap_or("") {
        "CLOSED" if filled >= size && size > Decimal::ZERO => (VenueStatus::Filled, true),
        "CLOSED" if filled > Decimal::ZERO => (VenueStatus::PartiallyFilled, true),
        "CLOSED" => (VenueStatus::Cancelled, true),
        "OPEN" | "NEW" | "UNTRIGGERED" => (
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Open
            },
            false,
        ),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    Ok(VenueOrderState {
        venue_order_id: id.into(),
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final: fin,
        raw: v.clone(),
    })
}

#[derive(Debug)]
pub struct ParadexUserSignedExec {
    http: reqwest::Client,
    rest: String,
    chain_id: &'static str,
    network: &'static str,
}

impl ParadexUserSignedExec {
    pub fn testnet() -> Self {
        Self::new(
            crate::paradex::TESTNET_REST,
            "PRIVATE_SN_PARACLEAR_TESTNET",
            crate::paradex::TESTNET_NETWORK,
        )
    }
    /// MAINNET (real funds), `starknet_chain_id` from GET /system/config [LIVE 2026-10-09].
    /// Only attached by the `mainnet-nc` build. Paradex options liquidity is RPI-only: API books
    /// are usually empty, so most mainnet routes will not select Paradex.
    pub fn mainnet() -> Self {
        Self::new(
            crate::paradex::MAINNET_REST,
            "PRIVATE_SN_PARACLEAR_MAINNET",
            crate::paradex::NETWORK,
        )
    }
    pub fn new(rest: &str, chain_id: &'static str, network: &'static str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            chain_id,
            network,
        }
    }
    pub fn chain_id(&self) -> &'static str {
        self.chain_id
    }
}

#[async_trait]
impl ExecutionVenue for ParadexUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.network
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        Some(format!("client:oa-{}", o.salt))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let sig = u["signature"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| bad("missing user signature"))?;
        let jwt = u["venue_auth"]["jwt"]
            .as_str()
            .filter(|s| s.len() > 20)
            .ok_or_else(|| bad("venue_auth.jwt: the user's short-lived Paradex JWT"))?;
        // The signed message must be the one we prepared (same fields): rebuild it.
        order_typed_data(o, self.chain_id)?;
        let body = json!({
            "market": o.native_id,
            "side": if o.side == Side::Buy { "BUY" } else { "SELL" },
            "size": o.qty_native.normalize().to_string(),
            "type": "LIMIT",
            "price": o.limit_price_native.normalize().to_string(),
            "instruction": match o.time_in_force { TimeInForce::Ioc => "IOC", TimeInForce::Gtc => "GTC" },
            "client_id": format!("oa-{}", o.salt),
            "signature": sig,
            "signature_timestamp": signature_timestamp(o),
        });
        let resp = self
            .http
            .post(format!("{}/orders", self.rest))
            .bearer_auth(jwt)
            .json(&body)
            .send()
            .await;
        parse_order(&read_json(P, resp).await?)
    }
    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None) // needs the user's JWT
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (IOC; cancel from the user's account)"),
        })
    }
    fn identity(&self) -> Value {
        json!({"venue": self.network, "custody": "NON_CUSTODIAL", "signed_by": "end user Starknet account (+ short-lived JWT, never stored)", "chain_id": self.chain_id})
    }
    async fn balance(&self) -> Result<VenueBalance> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: "balance: user account".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> ExecOrder {
        ExecOrder {
            native_id: "BTC-USD-16OCT26-84000-C".into(),
            venue_instrument_id: None,
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(1, 2),
            limit_price_native: Decimal::new(1250, 0),
            salt: 123_456,
            timestamp: 1_791_480_000,
            time_in_force: TimeInForce::Ioc,
        }
    }

    #[test]
    fn typed_data_matches_the_sdk_layout() {
        let t = order_typed_data(&order(), "PRIVATE_SN_PARACLEAR_TESTNET").unwrap();
        assert_eq!(t["primaryType"], "Order");
        assert_eq!(
            t["domain"]["chainId"],
            format!("0x{}", hex::encode("PRIVATE_SN_PARACLEAR_TESTNET"))
        );
        assert_eq!(t["message"]["side"], "1");
        assert_eq!(t["message"]["size"], "1000000"); // 0.01 x 1e8
        assert_eq!(t["message"]["price"], "125000000000"); // 1250 x 1e8
        assert_eq!(t["message"]["timestamp"], "1791480000456");
    }

    #[test]
    fn closed_orders_map_to_fill_states() {
        let f = parse_order(&json!({"id": "1", "status": "CLOSED", "size": "0.01", "remaining_size": "0", "avg_fill_price": "1240"})).unwrap();
        assert_eq!(f.status, VenueStatus::Filled);
        let c = parse_order(
            &json!({"id": "1", "status": "CLOSED", "size": "0.01", "remaining_size": "0.01"}),
        )
        .unwrap();
        assert_eq!(c.status, VenueStatus::Cancelled);
        assert!(
            !parse_order(
                &json!({"id": "1", "status": "NEW", "size": "0.01", "remaining_size": "0.01"})
            )
            .unwrap()
            .is_final
        );
    }

    #[tokio::test]
    async fn submit_forwards_signature_and_bearer_jwt() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<(String, Value)>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/orders",
            axum::routing::post(move |h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| {
                let s2 = s2.clone();
                async move {
                    *s2.lock().unwrap() = Some((h["authorization"].to_str().unwrap().to_string(), b));
                    axum::Json(json!({"id": "o1", "status": "CLOSED", "size": "0.01", "remaining_size": "0", "avg_fill_price": "1249"}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let ex = ParadexUserSignedExec::new(
            &format!("http://{addr}"),
            "PRIVATE_SN_PARACLEAR_TESTNET",
            "paradex-testnet",
        );
        let mut o = order();
        o.venue_meta["user"] = json!({"signature": "[\"0x1\",\"0x2\"]", "venue_auth": {"jwt": "eyJhbGciOiJFUzI1NiJ9.payload.sig"}});
        let st = ex.submit(&o).await.unwrap();
        assert_eq!(st.status, VenueStatus::Filled);
        let (auth, body) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(auth, "Bearer eyJhbGciOiJFUzI1NiJ9.payload.sig");
        assert_eq!(body["instruction"], "IOC");
        assert_eq!(body["signature_timestamp"], 1_791_480_000_456i64);
        // No JWT: refused, nothing sent.
        *seen.lock().unwrap() = None;
        o.venue_meta["user"]["venue_auth"] = json!({});
        assert!(ex.submit(&o).await.is_err());
        assert!(seen.lock().unwrap().is_none());
    }
}
