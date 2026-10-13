//! Aevo TESTNET execution: EIP-712 order signing + HMAC-SHA256 REST authentication.
//!
//! Source: https://www.aevo.xyz/docs/api-reference/signing-orders (read 2026-10-05)
//! - `Order(address maker,bool isBuy,uint256 limitPrice,uint256 amount,uint256 salt,uint256 instrument,uint256 timestamp)`;
//!   limitPrice/amount in 6-decimal fixed point; instrument = numeric instrument id;
//!   timestamp in seconds; signed by the SIGNING KEY.
//! - Domain `EIP712Domain(string name,string version,uint256 chainId)`:
//!   testnet = ("Aevo Testnet", "1", 11155111).
//!
//! Source: https://www.aevo.xyz/docs/api-reference/urls/authentication
//! - AEVO-KEY, AEVO-TIMESTAMP (ns), AEVO-SIGNATURE = hex(HMAC_SHA256(secret,
//!   "key,timestamp,METHOD,path,body")), body empty for GET/DELETE.
//! - POST /orders, GET /orders/{id}, DELETE /orders/{id}; "order_id is the hash of the order
//!   payload" -> we precompute the EIP-712 digest as the expected id (HYPOTHESIS until the first
//!   testnet order confirms it; reconciliation falls back to the id returned by the venue).
//!
//! Safety: this type has ONLY a testnet constructor. URL and EIP-712 domain are constants.

use crate::exec::{
    ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState, VenueStatus,
};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result, invalid, opt_dec};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use k256::ecdsa::SigningKey;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use std::fmt;

pub const TESTNET_REST: &str = crate::aevo::TESTNET_REST;
const DOMAIN_NAME: &str = "Aevo Testnet";
const DOMAIN_VERSION: &str = "1";
const DOMAIN_CHAIN_ID: u64 = 11_155_111;
const P: PlatformId = PlatformId::Aevo;

pub(crate) fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

pub(crate) fn u256(v: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&v.to_be_bytes());
    out
}

pub(crate) fn address_word(addr: &[u8; 20]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(addr);
    out
}

/// ERC-20 `approve(spender, amount)` calldata; `spender` is a constant address.
pub(crate) fn erc20_approve(spender: &str, amount: u128) -> Vec<u8> {
    let mut out = keccak(b"approve(address,uint256)")[..4].to_vec();
    out.extend_from_slice(&address_word(&parse_address(spender).expect("constant")));
    out.extend_from_slice(&u256(amount));
    out
}

pub fn parse_address(s: &str) -> Option<[u8; 20]> {
    let h = s.strip_prefix("0x").unwrap_or(s);
    let b = hex::decode(h).ok()?;
    b.try_into().ok()
}

/// EIP-712 fields of an Aevo order (already in fixed point).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderFields {
    pub maker: [u8; 20],
    pub is_buy: bool,
    pub limit_price: u128,
    pub amount: u128,
    pub salt: u128,
    pub instrument: u128,
    pub timestamp: u128,
}

pub fn domain_separator(name: &str, version: &str, chain_id: u64) -> [u8; 32] {
    let th = keccak(b"EIP712Domain(string name,string version,uint256 chainId)");
    let mut buf = Vec::with_capacity(128);
    buf.extend_from_slice(&th);
    buf.extend_from_slice(&keccak(name.as_bytes()));
    buf.extend_from_slice(&keccak(version.as_bytes()));
    buf.extend_from_slice(&u256(chain_id as u128));
    keccak(&buf)
}

pub fn order_digest(f: &OrderFields, domain: [u8; 32]) -> [u8; 32] {
    let th = keccak(
        b"Order(address maker,bool isBuy,uint256 limitPrice,uint256 amount,uint256 salt,uint256 instrument,uint256 timestamp)",
    );
    let mut buf = Vec::with_capacity(32 * 8);
    buf.extend_from_slice(&th);
    buf.extend_from_slice(&address_word(&f.maker));
    buf.extend_from_slice(&u256(f.is_buy as u128));
    for v in [f.limit_price, f.amount, f.salt, f.instrument, f.timestamp] {
        buf.extend_from_slice(&u256(v));
    }
    let struct_hash = keccak(&buf);
    let mut msg = Vec::with_capacity(66);
    msg.extend_from_slice(&[0x19, 0x01]);
    msg.extend_from_slice(&domain);
    msg.extend_from_slice(&struct_hash);
    keccak(&msg)
}

/// 65-byte Ethereum signature r||s||v (v = 27/28), low-s, RFC 6979 deterministic.
pub fn sign_digest(key: &SigningKey, digest: &[u8; 32]) -> std::result::Result<[u8; 65], String> {
    let (sig, recid) = key
        .sign_prehash_recoverable(digest)
        .map_err(|e| e.to_string())?;
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&sig.to_bytes());
    out[64] = 27 + recid.to_byte();
    Ok(out)
}

pub fn eth_address(key: &SigningKey) -> [u8; 20] {
    let pt = key.verifying_key().to_encoded_point(false);
    let h = keccak(&pt.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    a
}

/// Decimal -> 6-decimal fixed point, refusing anything that is not exactly representable.
pub fn fixed6(d: Decimal) -> Option<u128> {
    if d < Decimal::ZERO {
        return None;
    }
    let scaled = d * Decimal::from(1_000_000u64);
    if scaled.fract() != Decimal::ZERO {
        return None;
    }
    use rust_decimal::prelude::ToPrimitive;
    scaled.to_u128()
}

pub fn hmac_signature(secret: &str, message: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn auth_message(key: &str, ts_ns: &str, method: &str, path: &str, body: &str) -> String {
    format!("{key},{ts_ns},{},{path},{body}", method.to_uppercase())
}

/// Secret holder: never printed.
struct Secret(String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

pub struct AevoTestnetExec {
    http: reqwest::Client,
    rest: String,
    api_key: Secret,
    api_secret: Secret,
    signing_key: SigningKey,
    maker: [u8; 20],
    domain: [u8; 32],
}

impl fmt::Debug for AevoTestnetExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AevoTestnetExec")
            .field("rest", &self.rest)
            .field("maker", &format!("0x{}", hex::encode(self.maker)))
            .field("keys", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum TestnetConfigError {
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    #[error("{0} is malformed")]
    Malformed(&'static str),
}

impl AevoTestnetExec {
    /// Reads OA_AEVO_TESTNET_{WALLET,API_KEY,API_SECRET,SIGNING_KEY}. Values are never logged.
    pub fn testnet_from_env() -> std::result::Result<Self, TestnetConfigError> {
        Self::testnet_from_lookup(|k| std::env::var(k).ok(), TESTNET_REST)
    }

    pub fn testnet_from_lookup(
        get: impl Fn(&str) -> Option<String>,
        rest: &str,
    ) -> std::result::Result<Self, TestnetConfigError> {
        let need = |k: &'static str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or(TestnetConfigError::Missing(k))
        };
        let maker = parse_address(&need("OA_AEVO_TESTNET_WALLET")?)
            .ok_or(TestnetConfigError::Malformed("OA_AEVO_TESTNET_WALLET"))?;
        let sk_hex = need("OA_AEVO_TESTNET_SIGNING_KEY")?;
        let sk_bytes = hex::decode(sk_hex.trim().trim_start_matches("0x"))
            .map_err(|_| TestnetConfigError::Malformed("OA_AEVO_TESTNET_SIGNING_KEY"))?;
        let signing_key = SigningKey::from_slice(&sk_bytes)
            .map_err(|_| TestnetConfigError::Malformed("OA_AEVO_TESTNET_SIGNING_KEY"))?;
        Ok(Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            api_key: Secret(need("OA_AEVO_TESTNET_API_KEY")?),
            api_secret: Secret(need("OA_AEVO_TESTNET_API_SECRET")?),
            signing_key,
            maker,
            domain: domain_separator(DOMAIN_NAME, DOMAIN_VERSION, DOMAIN_CHAIN_ID),
        })
    }

    fn fields(&self, o: &ExecOrder) -> Result<OrderFields> {
        let bad = |d: &str| crate::not_representable(P, d);
        Ok(OrderFields {
            maker: self.maker,
            is_buy: o.side == Side::Buy,
            limit_price: fixed6(o.limit_price_native).ok_or_else(|| bad("limit price"))?,
            amount: fixed6(o.qty_native).ok_or_else(|| bad("amount"))?,
            salt: o.salt as u128,
            instrument: o
                .venue_instrument_id
                .ok_or_else(|| bad("missing instrument id"))? as u128,
            timestamp: u128::try_from(o.timestamp).map_err(|_| bad("timestamp"))?,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<String>,
    ) -> Result<Value> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
            .to_string();
        let b = body.clone().unwrap_or_default();
        let sig = hmac_signature(
            &self.api_secret.0,
            &auth_message(&self.api_key.0, &ts, method.as_str(), path, &b),
        );
        let mut rb = self
            .http
            .request(method, format!("{}{}", self.rest, path))
            .header("AEVO-KEY", &self.api_key.0)
            .header("AEVO-TIMESTAMP", ts)
            .header("AEVO-SIGNATURE", sig);
        if let Some(b) = body {
            rb = rb.header("content-type", "application/json").body(b);
        }
        read_json(P, rb.send().await).await
    }
}

pub fn parse_state(v: &Value) -> Result<VenueOrderState> {
    let id = v["order_id"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("order response without order_id: {v}")))?;
    let filled = opt_dec(P, v.get("filled"), "filled")?.unwrap_or_default();
    let avg = opt_dec(P, v.get("avg_price"), "avg_price")?.filter(|d| *d > Decimal::ZERO);
    let raw_status = v["order_status"].as_str().unwrap_or("").to_lowercase();
    let is_final = matches!(
        raw_status.as_str(),
        "filled" | "cancelled" | "canceled" | "rejected"
    );
    let status = match raw_status.as_str() {
        "filled" => VenueStatus::Filled,
        "opened" | "open" => {
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Open
            }
        }
        "partial" | "partially_filled" | "partially filled" => VenueStatus::PartiallyFilled,
        // A cancelled IOC with some fill is a terminal partial fill.
        "cancelled" | "canceled" => {
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Cancelled
            }
        }
        "rejected" => VenueStatus::Rejected,
        other => VenueStatus::Unknown(other.to_string()),
    };
    Ok(VenueOrderState {
        venue_order_id: id.to_string(),
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final,
        raw: v.clone(),
    })
}

#[async_trait]
impl ExecutionVenue for AevoTestnetExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        crate::aevo::TESTNET_NETWORK
    }

    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        let f = self.fields(order).ok()?;
        Some(format!("0x{}", hex::encode(order_digest(&f, self.domain))))
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let f = self.fields(o)?;
        let digest = order_digest(&f, self.domain);
        let sig = sign_digest(&self.signing_key, &digest)
            .map_err(|e| invalid(P, format!("signing failed: {e}")))?;
        let body = json!({
            "instrument": f.instrument as u64,
            "maker": format!("0x{}", hex::encode(f.maker)),
            "is_buy": f.is_buy,
            "amount": f.amount.to_string(),
            "limit_price": f.limit_price.to_string(),
            "salt": f.salt.to_string(),
            "signature": format!("0x{}", hex::encode(sig)),
            "timestamp": f.timestamp.to_string(),
            "post_only": false,
            "reduce_only": false,
            "time_in_force": match o.time_in_force { TimeInForce::Ioc => "IOC", TimeInForce::Gtc => "GTC" },
        })
        .to_string();
        let v = self
            .call(reqwest::Method::POST, "/orders", Some(body))
            .await?;
        parse_state(&v)
    }

    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        match self
            .call(reqwest::Method::GET, &format!("/orders/{id}"), None)
            .await
        {
            Ok(v) => parse_state(&v).map(Some),
            Err(ConnectorError::Http { status: 404, .. }) => Ok(None),
            Err(ConnectorError::Http {
                status: 400, body, ..
            }) if body.to_uppercase().contains("NOT_FOUND") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        let v = self
            .call(reqwest::Method::DELETE, &format!("/orders/{id}"), None)
            .await?;
        // DELETE answers {"order_id": ...}; fetch the authoritative state afterwards.
        let oid = v["order_id"].as_str().unwrap_or(id).to_string();
        Ok(self.get_order(&oid).await?.unwrap_or(VenueOrderState {
            venue_order_id: oid,
            status: VenueStatus::Unknown("cancel acknowledged, state not readable".into()),
            filled_native: Decimal::ZERO,
            avg_price_native: None,
            is_final: false,
            raw: v,
        }))
    }

    /// GET /account (authenticated): `available_balance`, `equity` (USDC). The raw response is
    /// reduced to non-personal fields (no email, no API keys list).
    async fn balance(&self) -> Result<VenueBalance> {
        let v = self.call(reqwest::Method::GET, "/account", None).await?;
        let available = opt_dec(P, v.get("available_balance"), "available_balance")?
            .ok_or_else(|| invalid(P, "account without available_balance"))?;
        Ok(VenueBalance {
            available,
            equity: opt_dec(P, v.get("equity"), "equity")?,
            currency: "USDC".into(),
            raw: json!({"account": v["account"], "balance": v["balance"], "available_balance": v["available_balance"], "equity": v["equity"]}),
        })
    }

    fn identity(&self) -> Value {
        json!({
            "venue": "aevo-testnet",
            "maker": format!("0x{}", hex::encode(self.maker)),
            "signing_key_address": format!("0x{}", hex::encode(eth_address(&self.signing_key))),
            "domain": {"name": DOMAIN_NAME, "version": DOMAIN_VERSION, "chainId": DOMAIN_CHAIN_ID},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::path::PathBuf;

    /// Vectors produced by eth_account (scripts/gen_aevo_vectors.py) with the PUBLIC test key
    /// keccak("cow"). Our encoder + signer must reproduce digests AND signatures exactly.
    #[test]
    fn eip712_matches_reference_implementation() {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/aevo_signing_vectors.json");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        let key = SigningKey::from_slice(&keccak(b"cow")).unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(eth_address(&key))).to_lowercase(),
            v["signer_address"].as_str().unwrap().to_lowercase()
        );
        for c in v["cases"].as_array().unwrap() {
            let domain = match c["domain"].as_str().unwrap() {
                "testnet" => domain_separator("Aevo Testnet", "1", 11_155_111),
                _ => domain_separator("Aevo Mainnet", "1", 1),
            };
            let n = |k: &str| c[k].as_str().unwrap().parse::<u128>().unwrap();
            let f = OrderFields {
                maker: parse_address(c["maker"].as_str().unwrap()).unwrap(),
                is_buy: c["isBuy"].as_bool().unwrap(),
                limit_price: n("limitPrice"),
                amount: n("amount"),
                salt: n("salt"),
                instrument: n("instrument"),
                timestamp: n("timestamp"),
            };
            let d = order_digest(&f, domain);
            assert_eq!(
                format!("0x{}", hex::encode(d)),
                c["digest"].as_str().unwrap()
            );
            let sig = sign_digest(&key, &d).unwrap();
            assert_eq!(
                format!("0x{}", hex::encode(sig)),
                c["signature"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn fixed_point_is_exact_or_refused() {
        assert_eq!(fixed6(dec!(101.7)), Some(101_700_000));
        assert_eq!(fixed6(dec!(0.01)), Some(10_000));
        assert_eq!(fixed6(dec!(1.0000001)), None, "would silently round");
        assert_eq!(fixed6(dec!(-1)), None);
    }

    #[test]
    fn hmac_message_format_matches_doc() {
        // Doc example: GET /account with empty body -> trailing comma.
        assert_eq!(
            auth_message("API_KEY", "1673425955575713842", "get", "/account", ""),
            "API_KEY,1673425955575713842,GET,/account,"
        );
        // RFC 4231 test case 2 (HMAC-SHA256, key "Jefe").
        assert_eq!(
            hmac_signature("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn status_mapping_never_invents_finality() {
        let s = |st: &str, filled: &str| {
            parse_state(&json!({"order_id": "0x1", "order_status": st, "filled": filled, "avg_price": "10"})).unwrap().status
        };
        assert_eq!(s("filled", "1"), VenueStatus::Filled);
        assert_eq!(s("cancelled", "0.5"), VenueStatus::PartiallyFilled);
        assert_eq!(s("cancelled", "0"), VenueStatus::Cancelled);
        assert_eq!(s("opened", "0"), VenueStatus::Open);
        assert_eq!(s("weird", "0"), VenueStatus::Unknown("weird".into()));
        assert!(
            parse_state(&json!({"order_status": "filled"})).is_err(),
            "no order_id -> invalid"
        );
    }

    #[test]
    fn config_requires_every_secret_and_never_prints_them() {
        let env = |m: &[(&str, &str)]| {
            let m: std::collections::HashMap<String, String> = m
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            move |k: &str| m.get(k).cloned()
        };
        let full = [
            (
                "OA_AEVO_TESTNET_WALLET",
                "0x0000000000000000000000000000000000000001",
            ),
            ("OA_AEVO_TESTNET_API_KEY", "k-SECRET-1"),
            ("OA_AEVO_TESTNET_API_SECRET", "s-SECRET-2"),
            (
                "OA_AEVO_TESTNET_SIGNING_KEY",
                "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
            ),
        ];
        let e = AevoTestnetExec::testnet_from_lookup(env(&full), TESTNET_REST).unwrap();
        let dbg = format!("{e:?} {}", e.identity());
        assert!(
            !dbg.contains("SECRET") && !dbg.contains("4c0883a6"),
            "{dbg}"
        );
        assert_eq!(
            AevoTestnetExec::testnet_from_lookup(env(&full[..3]), TESTNET_REST).unwrap_err(),
            TestnetConfigError::Missing("OA_AEVO_TESTNET_SIGNING_KEY")
        );
    }
}
