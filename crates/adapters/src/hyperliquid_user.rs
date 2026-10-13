//! NON-CUSTODIAL Hyperliquid HIP-4 execution (outcome / binary markets): the END USER's wallet or
//! its approved API agent signs the L1 action; Omni Strike verifies the signer and forwards it.
//!
//! Reference: official `hyperliquid-python-sdk` (`utils/signing.py`, installed locally, 2026-10-08):
//! - order action = msgpack({"type":"order","orders":[{"a","b","p","s","r","t":{"limit":{"tif":"Ioc"}}}],
//!   "grouping":"na"}) in that key order; asset id of an outcome side = 100_000_000 + encoding
//!   [DOC asset-ids, see hyperliquid.rs]; `p`/`s` are `float_to_wire` strings (<= 8 decimals, no
//!   trailing zeros).
//! - action_hash = keccak(msgpack(action) || nonce (8 bytes BE) || 0x00 (no vault)).
//! - phantom agent {source "a" (mainnet), connectionId = action_hash}, EIP-712 domain
//!   {name "Exchange", version "1", chainId 1337, verifyingContract 0x0}, type
//!   `Agent(string source,bytes32 connectionId)`.
//! - POST https://api.hyperliquid.xyz/exchange {action, nonce, signature {r, s, v}, vaultAddress null}.
//!
//! Tests reproduce the SDK byte for byte (msgpack, action hash, signature recovery).

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

const P: PlatformId = PlatformId::Hyperliquid;
pub const EXCHANGE_URL: &str = "https://api.hyperliquid.xyz/exchange";
const ASSET_ID_BASE: u64 = 100_000_000;

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

// ---- minimal MessagePack (only what an order action needs) ----
fn mp_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    match b.len() {
        n if n < 32 => out.push(0xa0 | n as u8),
        n if n < 256 => out.extend_from_slice(&[0xd9, n as u8]),
        n => {
            out.push(0xda);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
    }
    out.extend_from_slice(b);
}
fn mp_uint(out: &mut Vec<u8>, v: u64) {
    match v {
        0..=0x7f => out.push(v as u8),
        0x80..=0xff => out.extend_from_slice(&[0xcc, v as u8]),
        0x100..=0xffff => {
            out.push(0xcd);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xce);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        }
        _ => {
            out.push(0xcf);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}
fn mp_map(out: &mut Vec<u8>, n: usize) {
    out.push(0x80 | n as u8);
}
fn mp_bool(out: &mut Vec<u8>, b: bool) {
    out.push(if b { 0xc3 } else { 0xc2 });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderWire {
    pub asset: u64,
    pub is_buy: bool,
    pub price: String,
    pub size: String,
    pub tif: &'static str,
}

pub fn pack_order_action(o: &OrderWire) -> Vec<u8> {
    let mut b = Vec::with_capacity(128);
    mp_map(&mut b, 3);
    mp_str(&mut b, "type");
    mp_str(&mut b, "order");
    mp_str(&mut b, "orders");
    b.push(0x91); // fixarray(1)
    mp_map(&mut b, 6);
    mp_str(&mut b, "a");
    mp_uint(&mut b, o.asset);
    mp_str(&mut b, "b");
    mp_bool(&mut b, o.is_buy);
    mp_str(&mut b, "p");
    mp_str(&mut b, &o.price);
    mp_str(&mut b, "s");
    mp_str(&mut b, &o.size);
    mp_str(&mut b, "r");
    mp_bool(&mut b, false);
    mp_str(&mut b, "t");
    mp_map(&mut b, 1);
    mp_str(&mut b, "limit");
    mp_map(&mut b, 1);
    mp_str(&mut b, "tif");
    mp_str(&mut b, o.tif);
    mp_str(&mut b, "grouping");
    mp_str(&mut b, "na");
    b
}

pub fn action_hash(o: &OrderWire, nonce: u64) -> [u8; 32] {
    let mut b = pack_order_action(o);
    b.extend_from_slice(&nonce.to_be_bytes());
    b.push(0x00); // no vault
    keccak(&b)
}

/// EIP-712 digest of the phantom agent the user's key signs (mainnet source "a").
pub fn agent_digest(connection_id: &[u8; 32]) -> [u8; 32] {
    let mut d = Vec::with_capacity(160);
    d.extend_from_slice(&keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    d.extend_from_slice(&keccak(b"Exchange"));
    d.extend_from_slice(&keccak(b"1"));
    d.extend_from_slice(&u256(1337));
    d.extend_from_slice(&address_word(&[0u8; 20]));
    let domain = keccak(&d);
    let mut s = Vec::with_capacity(96);
    s.extend_from_slice(&keccak(b"Agent(string source,bytes32 connectionId)"));
    s.extend_from_slice(&keccak(b"a"));
    s.extend_from_slice(connection_id);
    let mut m = vec![0x19, 0x01];
    m.extend_from_slice(&domain);
    m.extend_from_slice(&keccak(&s));
    keccak(&m)
}

fn wire_num(d: Decimal) -> Option<String> {
    let r = d.round_dp(8);
    (r == d && d > Decimal::ZERO).then(|| r.normalize().to_string())
}

/// Outcome coin "#<encoding>" -> order wire for this ExecOrder.
pub fn order_wire(o: &ExecOrder) -> Result<OrderWire> {
    let enc: u64 = o
        .native_id
        .strip_prefix('#')
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| bad("outcome coin must be #<encoding>"))?;
    Ok(OrderWire {
        asset: ASSET_ID_BASE + enc,
        is_buy: o.side == Side::Buy,
        price: wire_num(o.limit_price_native).ok_or_else(|| bad("price: > 8 decimals"))?,
        size: wire_num(o.qty_native).ok_or_else(|| bad("size: > 8 decimals"))?,
        tif: match o.time_in_force {
            TimeInForce::Ioc => "Ioc",
            TimeInForce::Gtc => "Gtc",
        },
    })
}

pub fn nonce(o: &ExecOrder) -> u64 {
    (o.timestamp.max(0) as u64) * 1000 + o.salt % 1000
}

pub fn typed_data(connection_id: &[u8; 32]) -> Value {
    json!({
        "types": {
            "EIP712Domain": [{"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                             {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}],
            "Agent": [{"name": "source", "type": "string"}, {"name": "connectionId", "type": "bytes32"}]
        },
        "primaryType": "Agent",
        "domain": {"name": "Exchange", "version": "1", "chainId": 1337, "verifyingContract": "0x0000000000000000000000000000000000000000"},
        "message": {"source": "a", "connectionId": format!("0x{}", hex::encode(connection_id))}
    })
}

/// The exact action the user's signature commits to (also returned at prepare for client checks).
pub fn action_json(o: &OrderWire) -> Value {
    json!({"type": "order", "orders": [{"a": o.asset, "b": o.is_buy, "p": o.price, "s": o.size, "r": false, "t": {"limit": {"tif": o.tif}}}], "grouping": "na"})
}

#[derive(Debug)]
pub struct HyperliquidUserSignedExec {
    http: reqwest::Client,
    url: String,
}

impl HyperliquidUserSignedExec {
    pub fn mainnet() -> Self {
        Self::with_url(EXCHANGE_URL)
    }
    pub fn with_url(url: &str) -> Self {
        Self {
            http: client(),
            url: url.into(),
        }
    }
}

#[async_trait]
impl ExecutionVenue for HyperliquidUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        crate::hyperliquid::NETWORK
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        Some(format!("nonce:{}", nonce(o)))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let signer = u["signer"]
            .as_str()
            .or(u["wallet"].as_str())
            .and_then(parse_address)
            .ok_or_else(|| bad("account signer/wallet"))?;
        let sig = u["signature"]
            .as_str()
            .ok_or_else(|| bad("missing user signature"))?;
        let w = order_wire(o)?;
        let n = nonce(o);
        let h = action_hash(&w, n);
        if recover_address(&agent_digest(&h), sig) != Some(signer) {
            return Err(bad("signature does not match the declared wallet/agent"));
        }
        let raw = hex::decode(sig.trim_start_matches("0x")).map_err(|_| bad("signature hex"))?;
        if raw.len() != 65 {
            return Err(bad("signature must be 65 bytes"));
        }
        let v = if raw[64] < 27 { raw[64] + 27 } else { raw[64] };
        let body = json!({"action": action_json(&w), "nonce": n, "vaultAddress": null,
                          "signature": {"r": format!("0x{}", hex::encode(&raw[..32])), "s": format!("0x{}", hex::encode(&raw[32..64])), "v": v}});
        let r = read_json(P, self.http.post(&self.url).json(&body).send().await).await?;
        if r["status"] != "ok" {
            return Err(ConnectorError::Http {
                platform: P,
                status: 400,
                body: r.to_string().chars().take(300).collect(),
            });
        }
        let st = &r["response"]["data"]["statuses"][0];
        let (status, filled, avg, id) = if let Some(f) = st.get("filled") {
            let sz = f["totalSz"]
                .as_str()
                .and_then(|x| x.parse().ok())
                .unwrap_or_default();
            let px = f["avgPx"].as_str().and_then(|x| x.parse().ok());
            (VenueStatus::Filled, sz, px, f["oid"].to_string())
        } else if let Some(e) = st.get("error") {
            return Err(ConnectorError::Http {
                platform: P,
                status: 400,
                body: e.to_string(),
            });
        } else {
            (
                VenueStatus::Cancelled,
                Decimal::ZERO,
                None,
                format!("nonce:{n}"),
            )
        };
        let qty = o.qty_native;
        let status = if status == VenueStatus::Filled && filled < qty {
            VenueStatus::PartiallyFilled
        } else {
            status
        };
        Ok(VenueOrderState {
            venue_order_id: id,
            status,
            filled_native: filled,
            avg_price_native: avg,
            is_final: true,
            raw: r,
        })
    }
    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None)
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (IOC)"),
        })
    }
    fn identity(&self) -> Value {
        json!({"venue": crate::hyperliquid::NETWORK, "custody": "NON_CUSTODIAL", "signed_by": "end user wallet or approved API agent (L1 action, phantom agent EIP-712)"})
    }
    async fn balance(&self) -> Result<VenueBalance> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: "balance: user account".into(),
        })
    }
}

/// Account helper for the API layer: {"wallet": "0x…", "signer": "0x…" (approved agent, optional)}.
pub fn account(v: &Value) -> std::result::Result<Value, String> {
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
    Ok(json!({"wallet": hex20(&w), "signer": hex20(&s)}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector() -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hyperliquid_sdk_order_vector.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }
    fn wire() -> OrderWire {
        OrderWire {
            asset: 100_090_150,
            is_buy: true,
            price: "0.14".into(),
            size: "10".into(),
            tif: "Ioc",
        }
    }

    #[test]
    fn msgpack_and_action_hash_match_the_official_sdk() {
        let v = vector();
        assert_eq!(
            hex::encode(pack_order_action(&wire())),
            v["packed"].as_str().unwrap()
        );
        let h = action_hash(&wire(), v["nonce"].as_u64().unwrap());
        assert_eq!(
            format!("0x{}", hex::encode(h)),
            v["action_hash"].as_str().unwrap()
        );
    }

    #[test]
    fn sdk_signature_recovers_to_the_sdk_address() {
        let v = vector();
        let h = action_hash(&wire(), v["nonce"].as_u64().unwrap());
        let s = &v["signature"];
        let sig = format!(
            "0x{}{}{:02x}",
            s["r"].as_str().unwrap().trim_start_matches("0x"),
            s["s"].as_str().unwrap().trim_start_matches("0x"),
            s["v"].as_u64().unwrap()
        );
        let addr = recover_address(&agent_digest(&h), &sig).unwrap();
        assert_eq!(
            hex20(&addr).to_lowercase(),
            v["address"].as_str().unwrap().to_lowercase()
        );
    }

    #[test]
    fn exec_order_maps_to_the_wire() {
        let o = ExecOrder {
            native_id: "#90150".into(),
            venue_instrument_id: None,
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(10, 0),
            limit_price_native: Decimal::new(14, 2),
            salt: 123,
            timestamp: 1_791_480_000,
            time_in_force: TimeInForce::Ioc,
        };
        assert_eq!(order_wire(&o).unwrap(), wire());
        assert_eq!(nonce(&o), 1_791_480_000_123);
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live_mainnet {
    use super::*;
    use crate::Connector;
    use crate::aevo_exec::{eth_address, sign_digest};
    use k256::ecdsa::SigningKey;

    /// ZERO MONEY against Hyperliquid MAINNET: a throwaway (never funded) key signs one of our HIP-4
    /// orders; HL answers that the signer has no account and names the address IT recovered. Equal to
    /// ours = HL hashes our action exactly like we do (msgpack, nonce, phantom agent). Nothing can fill.
    #[tokio::test]
    async fn hyperliquid_recovers_our_throwaway_signer() {
        let cat = crate::hyperliquid::HyperliquidConnector::default()
            .list_instruments("BTC")
            .await
            .unwrap();
        let i = cat.first().expect("at least one BTC outcome");
        let key = SigningKey::from_slice(&rand_key()).unwrap();
        let me = eth_address(&key);
        let o = ExecOrder {
            native_id: i.key.native_id.clone(),
            venue_instrument_id: None,
            venue_meta: Value::Null,
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(1, 0),
            limit_price_native: Decimal::new(1, 2),
            salt: 1,
            timestamp: chrono::Utc::now().timestamp(),
            time_in_force: TimeInForce::Ioc,
        };
        let w = order_wire(&o).unwrap();
        let n = nonce(&o);
        let sig = sign_digest(&key, &agent_digest(&action_hash(&w, n))).unwrap();
        let v = if sig[64] < 27 { sig[64] + 27 } else { sig[64] };
        let body = json!({"action": action_json(&w), "nonce": n, "vaultAddress": null,
            "signature": {"r": format!("0x{}", hex::encode(&sig[..32])), "s": format!("0x{}", hex::encode(&sig[32..64])), "v": v}});
        let r: Value = client()
            .post(EXCHANGE_URL)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("{} -> {r}", i.key.native_id);
        assert!(
            r.to_string().to_lowercase().contains(&hex::encode(me)),
            "HL recovered another address: our hash or signature differs from HL's"
        );
    }

    fn rand_key() -> [u8; 32] {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        keccak(&t.to_be_bytes())
    }
}
