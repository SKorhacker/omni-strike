//! NON-CUSTODIAL Hypercall execution: the END USER's wallet (or its approved agent) signs an EIP-712
//! `PlaceOrder`; Omni Strike verifies the signer and forwards it. No API key exists on Hypercall.
//!
//! Source: docs.hypercall.xyz "Authentication & Signing" (read 2026-10-08):
//! domain {name "Hypercall", version "1", chainId 999, verifyingContract 0x0}; struct
//! `PlaceOrder(address wallet,string symbol,string side,string size,string price,string tif,
//! string route,string clientId,uint64 nonce)`; side "Buy"/"Sell"; tif "gtc"/"ioc"/"fok"; route
//! "best_execution" (book + RFQ auction) / "book_only" / "rfq_only"; size and price are STRINGS that
//! must match the JSON body exactly; nonce unique per wallet within [-2 days, +1 day] of server ms.
//! `POST /order` body {wallet, symbol, price, size, side, tif, route, client_id, nonce, signature}.
//! Agents are approved once by the wallet (`POST /approve-agent`); Hypercall checks authorization.
//! We use route "best_execution": visible books are mostly empty, the RFQ auction may still fill.
//! Testnet is disabled by Hypercall (production chain 999 only) -> tested against a fake server.

use crate::aevo_exec::{address_word, keccak, parse_address, u256};
use crate::derive_user::{hex20, recover_address};
use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Hypercall;
pub const ROUTE: &str = "best_execution";

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceOrder {
    pub wallet: [u8; 20],
    pub symbol: String,
    pub side: &'static str,
    pub size: String,
    pub price: String,
    pub tif: &'static str,
    pub route: String,
    pub client_id: String,
    pub nonce: u64,
}

fn domain_separator() -> [u8; 32] {
    let mut b = Vec::with_capacity(160);
    b.extend_from_slice(&keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    b.extend_from_slice(&keccak(b"Hypercall"));
    b.extend_from_slice(&keccak(b"1"));
    b.extend_from_slice(&u256(999));
    b.extend_from_slice(&address_word(&[0u8; 20]));
    keccak(&b)
}

pub fn digest(o: &PlaceOrder) -> [u8; 32] {
    let mut b = Vec::with_capacity(32 * 10);
    b.extend_from_slice(&keccak(b"PlaceOrder(address wallet,string symbol,string side,string size,string price,string tif,string route,string clientId,uint64 nonce)"));
    b.extend_from_slice(&address_word(&o.wallet));
    for s in [
        o.symbol.as_str(),
        o.side,
        o.size.as_str(),
        o.price.as_str(),
        o.tif,
        o.route.as_str(),
        o.client_id.as_str(),
    ] {
        b.extend_from_slice(&keccak(s.as_bytes()));
    }
    b.extend_from_slice(&u256(o.nonce as u128));
    let mut m = vec![0x19, 0x01];
    m.extend_from_slice(&domain_separator());
    m.extend_from_slice(&keccak(&b));
    keccak(&m)
}

/// Decimal as the exact string signed and sent (no exponent, trailing zeros removed).
fn dec_string(d: Decimal) -> String {
    d.normalize().to_string()
}

pub fn place_order(o: &ExecOrder, wallet: [u8; 20]) -> Result<PlaceOrder> {
    if o.qty_native <= Decimal::ZERO || o.limit_price_native <= Decimal::ZERO {
        return Err(bad("size and price must be > 0"));
    }
    Ok(PlaceOrder {
        wallet,
        symbol: o.native_id.clone(),
        side: if o.side == Side::Buy { "Buy" } else { "Sell" },
        size: dec_string(o.qty_native),
        price: dec_string(o.limit_price_native),
        tif: match o.time_in_force {
            TimeInForce::Ioc => "ioc",
            TimeInForce::Gtc => "gtc",
        },
        route: ROUTE.into(),
        client_id: format!("oa-{}", o.salt),
        nonce: (o.timestamp.max(0) as u64) * 1000 + o.salt % 1000,
    })
}

pub fn typed_data(p: &PlaceOrder) -> Value {
    json!({
        "types": {
            "EIP712Domain": [{"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                             {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}],
            "PlaceOrder": [{"name": "wallet", "type": "address"}, {"name": "symbol", "type": "string"}, {"name": "side", "type": "string"},
                           {"name": "size", "type": "string"}, {"name": "price", "type": "string"}, {"name": "tif", "type": "string"},
                           {"name": "route", "type": "string"}, {"name": "clientId", "type": "string"}, {"name": "nonce", "type": "uint64"}]
        },
        "primaryType": "PlaceOrder",
        "domain": {"name": "Hypercall", "version": "1", "chainId": 999, "verifyingContract": "0x0000000000000000000000000000000000000000"},
        "message": {"wallet": hex20(&p.wallet), "symbol": p.symbol, "side": p.side, "size": p.size, "price": p.price,
                    "tif": p.tif, "route": p.route, "clientId": p.client_id, "nonce": p.nonce}
    })
}

/// Account: {"wallet": "0x…", "signer": "0x…" (approved agent, optional)}.
pub fn account(v: &Value) -> std::result::Result<([u8; 20], [u8; 20]), String> {
    let w = v["wallet"]
        .as_str()
        .and_then(parse_address)
        .ok_or("wallet: 0x-prefixed address")?;
    let s = if v["signer"].is_null() {
        w
    } else {
        v["signer"]
            .as_str()
            .and_then(parse_address)
            .ok_or("signer: 0x-prefixed address")?
    };
    Ok((w, s))
}

/// Best-effort mapping of the /order answer (response schema not documented field by field).
pub fn parse_answer(v: &Value, client_id: &str) -> VenueOrderState {
    let id = v["order_id"]
        .as_str()
        .or(v["id"].as_str())
        .or(v["orderId"].as_str())
        .unwrap_or(client_id)
        .to_string();
    let num = |k: &str| {
        v[k].as_str()
            .and_then(|s| s.parse::<Decimal>().ok())
            .or_else(|| v[k].as_f64().and_then(|f| Decimal::try_from(f).ok()))
    };
    let filled = num("filled_size").or(num("filled")).unwrap_or_default();
    let avg = num("avg_price")
        .or(num("average_price"))
        .filter(|p| *p > Decimal::ZERO);
    let raw_status = v["status"].as_str().unwrap_or("").to_ascii_lowercase();
    let (status, fin) = match raw_status.as_str() {
        "filled" => (VenueStatus::Filled, true),
        "cancelled" | "canceled" | "expired" if filled > Decimal::ZERO => {
            (VenueStatus::PartiallyFilled, true)
        }
        "cancelled" | "canceled" | "expired" => (VenueStatus::Cancelled, true),
        "rejected" => (VenueStatus::Rejected, true),
        "open" | "new" | "pending" => (VenueStatus::Open, false),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    VenueOrderState {
        venue_order_id: id,
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final: fin,
        raw: v.clone(),
    }
}

#[derive(Debug)]
pub struct HypercallUserSignedExec {
    http: reqwest::Client,
    api: String,
}

impl HypercallUserSignedExec {
    pub fn production() -> Self {
        Self::with_api(crate::hypercall::API)
    }
    pub fn with_api(api: &str) -> Self {
        Self {
            http: client(),
            api: api.trim_end_matches('/').into(),
        }
    }
}

#[async_trait]
impl ExecutionVenue for HypercallUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        crate::hypercall::NETWORK
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        Some(format!("client:oa-{}", o.salt))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let (wallet, signer) = account(u).map_err(bad)?;
        let sig = u["signature"]
            .as_str()
            .ok_or_else(|| bad("missing user signature"))?;
        let p = place_order(o, wallet)?;
        if recover_address(&digest(&p), sig) != Some(signer) {
            return Err(bad(
                "order signature does not match the declared wallet/agent",
            ));
        }
        let body = json!({"wallet": hex20(&wallet), "symbol": p.symbol, "price": p.price, "size": p.size, "side": p.side,
                          "tif": p.tif, "route": p.route, "client_id": p.client_id, "nonce": p.nonce, "signature": sig});
        let v = read_json(
            P,
            self.http
                .post(format!("{}/order", self.api))
                .json(&body)
                .send()
                .await,
        )
        .await?;
        if let Some(e) = v.get("error") {
            return Err(ConnectorError::Http {
                platform: P,
                status: 400,
                body: e.to_string().chars().take(300).collect(),
            });
        }
        Ok(parse_answer(&v, &p.client_id))
    }
    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None)
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (cancel needs a CancelOrder signed by the user)"),
        })
    }
    fn identity(&self) -> Value {
        json!({"venue": crate::hypercall::NETWORK, "custody": "NON_CUSTODIAL", "signed_by": "end user wallet or its approved agent (EIP-712 PlaceOrder)", "route": ROUTE})
    }
    async fn balance(&self) -> Result<VenueBalance> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: "balance: user wallet".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aevo_exec::{eth_address, sign_digest};
    use k256::ecdsa::SigningKey;

    /// Example from the Hypercall docs; digest computed independently with eth_account.
    #[test]
    fn digest_matches_eth_account_on_the_documented_example() {
        let p = PlaceOrder {
            wallet: parse_address("0x1111111111111111111111111111111111111111").unwrap(),
            symbol: "BTC-20250131-100000-C".into(),
            side: "Buy",
            size: "0.1".into(),
            price: "100.0".into(),
            tif: "gtc",
            route: "best_execution".into(),
            client_id: "mm-1".into(),
            nonce: 123,
        };
        assert_eq!(
            format!("0x{}", hex::encode(digest(&p))),
            "0x1c19bed002565be6d49a25d15848e79ff82a2c638a9bc59e23d72fb30f2fc8b5"
        );
    }

    #[tokio::test]
    async fn submit_verifies_the_signer_and_forwards_exact_strings() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<Value>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/order",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
                let s2 = s2.clone();
                async move {
                    *s2.lock().unwrap() = Some(b);
                    axum::Json(json!({"order_id": "h1", "status": "filled", "filled_size": "0.1", "avg_price": "396"}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let key = SigningKey::from_slice(&[21u8; 32]).unwrap();
        let wallet = eth_address(&key);
        let mut o = ExecOrder {
            native_id: "ETH-20261016-2950-P".into(),
            venue_instrument_id: None,
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(10, 2),
            limit_price_native: Decimal::new(39600, 2),
            salt: 42,
            timestamp: 1_791_480_000,
            time_in_force: TimeInForce::Ioc,
        };
        let p = place_order(&o, wallet).unwrap();
        assert_eq!((p.size.as_str(), p.price.as_str()), ("0.1", "396"));
        let sig = format!("0x{}", hex::encode(sign_digest(&key, &digest(&p)).unwrap()));
        o.venue_meta["user"] = json!({"wallet": hex20(&wallet), "signature": sig});
        let ex = HypercallUserSignedExec::with_api(&format!("http://{addr}"));
        let st = ex.submit(&o).await.unwrap();
        assert_eq!(st.status, VenueStatus::Filled);
        let b = seen.lock().unwrap().clone().unwrap();
        assert_eq!(
            (b["size"].as_str(), b["price"].as_str(), b["route"].as_str()),
            (Some("0.1"), Some("396"), Some("best_execution"))
        );

        // Signed by another key: refused, nothing sent.
        *seen.lock().unwrap() = None;
        let other = SigningKey::from_slice(&[22u8; 32]).unwrap();
        o.venue_meta["user"]["signature"] = json!(format!(
            "0x{}",
            hex::encode(sign_digest(&other, &digest(&p)).unwrap())
        ));
        assert!(
            ex.submit(&o)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(seen.lock().unwrap().is_none());
    }
}
