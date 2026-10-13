//! Derive v3 TESTNET execution (Sepolia deployment, `testnet.api.derive.xyz/v3`).
//!
//! Source: https://docs.derive.xyz/authentication/action-signing and getting-started/contracts
//! (read 2026-10-05):
//! - Every order is an EIP-712 `Action(uint256 subaccountId,uint256 nonce,address module,bytes data,
//!   uint256 expiry,address owner,address signer)`, module = TRADE_MODULE (same on every network),
//!   data = abi.encode(address asset, uint256 sub_id, int256 limit_price, int256 amount,
//!   uint256 max_fee, uint256 recipient_id, bool is_bid), decimals scaled 1e18 (<= 12 decimals).
//! - Domain: name "Matching", version "1.0", chainId 11155111, verifyingContract = v2 Matching;
//!   testnet DOMAIN_SEPARATOR 0x24d674cd…3e85 (checked in tests against our own computation).
//! - Signature: raw secp256k1 over keccak(0x1901 || DOMAIN_SEPARATOR || actionHash).
//! - REST auth: X-DeriveWallet (owner), X-DeriveTimestamp (ms), X-DeriveSignature =
//!   EIP-191 personal_sign(timestamp) by the signer.
//! - `max_fee` is per unit of volume [OpenAPI].
//!
//! Idempotency: Derive has no client order id. Our nonce is derived from the stored order
//! (timestamp_ms x 1e6 + salt), so a lost acknowledgement is reconciled by finding that nonce in
//! `private/get_order_history` — never by resubmitting.
//! Safety: ONLY a testnet constructor; URL, chain id and domain are constants.

use crate::aevo_exec::{address_word, eth_address, keccak, parse_address, sign_digest, u256};
use crate::exec::{ExecOrder, ExecutionVenue, VenueBalance, VenueOrderState, VenueStatus};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result, invalid, opt_dec};
use async_trait::async_trait;
use k256::ecdsa::SigningKey;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};
use std::fmt;

pub const TESTNET_REST: &str = "https://testnet.api.derive.xyz/v3";
pub const TESTNET_WS: &str = "wss://testnet.api.derive.xyz/v3/ws";
pub const TESTNET_V3_NETWORK: &str = "derive-v3-testnet";
const CHAIN_ID: u64 = 11_155_111;
const MATCHING: &str = "0xeB8d770ec18DB98Db922E9D83260A585b9F0DeAD";
pub const TRADE_MODULE: &str = "0xB8D20c2B7a1Ad2EE33Bc50eF10876eD3035b5e7b";
/// Published by Derive for the testnet; `domain_separator()` must reproduce it (unit test).
pub const DOCUMENTED_TESTNET_DOMAIN: &str =
    "0x24d674cd5f2b9d564691c51e9d88f649b99246a2244dd74ce27b96578d773e85";
const SIGNATURE_TTL_SECS: i64 = 300;
const P: PlatformId = PlatformId::Derive;

pub fn action_typehash() -> [u8; 32] {
    keccak(b"Action(uint256 subaccountId,uint256 nonce,address module,bytes data,uint256 expiry,address owner,address signer)")
}

pub fn domain_separator(chain_id: u64) -> [u8; 32] {
    let mut b = Vec::with_capacity(160);
    b.extend_from_slice(&keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    b.extend_from_slice(&keccak(b"Matching"));
    b.extend_from_slice(&keccak(b"1.0"));
    b.extend_from_slice(&u256(chain_id as u128));
    b.extend_from_slice(&address_word(&parse_address(MATCHING).expect("constant")));
    keccak(&b)
}

/// Decimal -> 1e18 fixed point; refuses negatives and > 12 decimals (the exchange rejects them).
pub fn e18(d: Decimal) -> Option<u128> {
    use rust_decimal::prelude::ToPrimitive;
    if d < Decimal::ZERO {
        return None;
    }
    let scaled_12 = d * Decimal::from(1_000_000_000_000u64);
    if scaled_12.fract() != Decimal::ZERO {
        return None;
    }
    scaled_12.to_u128()?.checked_mul(1_000_000)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeData {
    pub asset: [u8; 20],
    pub sub_id: u128,
    pub limit_price_e18: u128,
    pub amount_e18: u128,
    pub max_fee_e18: u128,
    pub recipient_id: u64,
    pub is_bid: bool,
}

pub fn encode_trade_data(d: &TradeData) -> Vec<u8> {
    let mut b = Vec::with_capacity(32 * 7);
    b.extend_from_slice(&address_word(&d.asset));
    b.extend_from_slice(&u256(d.sub_id));
    b.extend_from_slice(&u256(d.limit_price_e18));
    b.extend_from_slice(&u256(d.amount_e18));
    b.extend_from_slice(&u256(d.max_fee_e18));
    b.extend_from_slice(&u256(d.recipient_id as u128));
    b.extend_from_slice(&u256(d.is_bid as u128));
    b
}

#[allow(clippy::too_many_arguments)]
pub fn action_digest(
    domain: [u8; 32],
    subaccount_id: u64,
    nonce: u128,
    data: &[u8],
    expiry: u64,
    owner: &[u8; 20],
    signer: &[u8; 20],
) -> [u8; 32] {
    let mut b = Vec::with_capacity(32 * 8);
    b.extend_from_slice(&action_typehash());
    b.extend_from_slice(&u256(subaccount_id as u128));
    b.extend_from_slice(&u256(nonce));
    b.extend_from_slice(&address_word(
        &parse_address(TRADE_MODULE).expect("constant"),
    ));
    b.extend_from_slice(&keccak(data));
    b.extend_from_slice(&u256(expiry as u128));
    b.extend_from_slice(&address_word(owner));
    b.extend_from_slice(&address_word(signer));
    let action = keccak(&b);
    let mut m = Vec::with_capacity(66);
    m.extend_from_slice(&[0x19, 0x01]);
    m.extend_from_slice(&domain);
    m.extend_from_slice(&action);
    keccak(&m)
}

/// EIP-191 personal_sign of a text message.
pub fn personal_sign(key: &SigningKey, text: &str) -> std::result::Result<[u8; 65], String> {
    let mut m = format!("\x19Ethereum Signed Message:\n{}", text.len()).into_bytes();
    m.extend_from_slice(text.as_bytes());
    sign_digest(key, &keccak(&m))
}

/// Nonce derived from the stored order: unique (salt) and time-ordered, reproducible for reconciliation.
pub fn nonce_for(order: &ExecOrder) -> u128 {
    (order.timestamp.max(0) as u128) * 1_000 * 1_000_000 + (order.salt as u128 % 1_000_000)
}

/// Decimal string as the exchange re-derives it from the signed e18 word.
pub(crate) fn dec_str(d: Decimal) -> String {
    d.normalize().to_string()
}

struct Secret(SigningKey);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

pub struct DeriveTestnetExec {
    http: reqwest::Client,
    rest: String,
    owner: [u8; 20],
    subaccount_id: u64,
    signer_key: Secret,
    signer: [u8; 20],
    domain: [u8; 32],
}

impl fmt::Debug for DeriveTestnetExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeriveTestnetExec")
            .field("rest", &self.rest)
            .field("subaccount_id", &self.subaccount_id)
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl DeriveTestnetExec {
    /// Reads OA_DERIVE_TESTNET_{WALLET,SUBACCOUNT_ID,SIGNER_KEY}. Values are never logged.
    pub fn testnet_from_env() -> std::result::Result<Self, crate::aevo_exec::TestnetConfigError> {
        Self::testnet_from_lookup(|k| std::env::var(k).ok(), TESTNET_REST)
    }

    pub fn testnet_from_lookup(
        get: impl Fn(&str) -> Option<String>,
        rest: &str,
    ) -> std::result::Result<Self, crate::aevo_exec::TestnetConfigError> {
        use crate::aevo_exec::TestnetConfigError as E;
        let need = |k: &'static str| get(k).filter(|v| !v.trim().is_empty()).ok_or(E::Missing(k));
        let owner = parse_address(need("OA_DERIVE_TESTNET_WALLET")?.trim())
            .ok_or(E::Malformed("OA_DERIVE_TESTNET_WALLET"))?;
        let subaccount_id: u64 = need("OA_DERIVE_TESTNET_SUBACCOUNT_ID")?
            .trim()
            .parse()
            .map_err(|_| E::Malformed("OA_DERIVE_TESTNET_SUBACCOUNT_ID"))?;
        let kb = hex::decode(
            need("OA_DERIVE_TESTNET_SIGNER_KEY")?
                .trim()
                .trim_start_matches("0x"),
        )
        .map_err(|_| E::Malformed("OA_DERIVE_TESTNET_SIGNER_KEY"))?;
        let key = SigningKey::from_slice(&kb)
            .map_err(|_| E::Malformed("OA_DERIVE_TESTNET_SIGNER_KEY"))?;
        let signer = eth_address(&key);
        Ok(Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            owner,
            subaccount_id,
            signer_key: Secret(key),
            signer,
            domain: domain_separator(CHAIN_ID),
        })
    }

    fn hex20(a: &[u8; 20]) -> String {
        format!("0x{}", hex::encode(a))
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let ts = chrono::Utc::now().timestamp_millis().to_string();
        let sig = personal_sign(&self.signer_key.0, &ts)
            .map_err(|e| invalid(P, format!("login signature: {e}")))?;
        let resp = self
            .http
            .post(format!("{}/{method}", self.rest))
            .header("X-DeriveWallet", Self::hex20(&self.owner))
            .header("X-DeriveTimestamp", &ts)
            .header("X-DeriveSignature", format!("0x{}", hex::encode(sig)))
            .json(&params)
            .send()
            .await;
        let v = read_json(P, resp).await?;
        if let Some(err) = v.get("error") {
            // JSON-RPC errors: negative "-32xxx" are server-side (outcome unknown); others are
            // business rejections from the exchange (nothing was created).
            let code = err["code"].as_i64().unwrap_or(0);
            let status = if (-32768..=-32000).contains(&code) {
                500
            } else {
                400
            };
            return Err(ConnectorError::Http {
                platform: P,
                status,
                body: err.to_string().chars().take(300).collect(),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| invalid(P, format!("{method}: no result")))
    }

    fn trade_data(&self, o: &ExecOrder) -> Result<(TradeData, Decimal)> {
        let bad = |d: &str| crate::not_representable(P, d);
        let asset = o.venue_meta["base_asset_address"]
            .as_str()
            .and_then(parse_address)
            .ok_or_else(|| bad("missing base_asset_address"))?;
        let sub_id: u128 = o.venue_meta["base_asset_sub_id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad("missing base_asset_sub_id"))?;
        let max_fee = o
            .max_fee_per_unit
            .ok_or_else(|| bad("missing max_fee_per_unit"))?;
        Ok((
            TradeData {
                asset,
                sub_id,
                limit_price_e18: e18(o.limit_price_native).ok_or_else(|| bad("limit price"))?,
                amount_e18: e18(o.qty_native).ok_or_else(|| bad("amount"))?,
                max_fee_e18: e18(max_fee).ok_or_else(|| bad("max fee"))?,
                recipient_id: self.subaccount_id,
                is_bid: o.side == Side::Buy,
            },
            max_fee,
        ))
    }
}

pub fn parse_order(o: &Value) -> Result<VenueOrderState> {
    let id = o["order_id"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("order without order_id: {o}")))?;
    let filled = opt_dec(P, o.get("filled_amount"), "filled_amount")?.unwrap_or_default();
    let avg = opt_dec(P, o.get("average_price"), "average_price")?.filter(|d| *d > Decimal::ZERO);
    let raw = o["order_status"].as_str().unwrap_or("").to_lowercase();
    let (status, fin) = match raw.as_str() {
        "filled" => (VenueStatus::Filled, true),
        "open" | "untriggered" => (
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Open
            },
            false,
        ),
        // IOC remainder cancelled by the exchange: terminal partial fill.
        "cancelled" | "expired" => (
            if filled > Decimal::ZERO {
                VenueStatus::PartiallyFilled
            } else {
                VenueStatus::Cancelled
            },
            true,
        ),
        "insufficient_margin" | "rejected" => (VenueStatus::Rejected, true),
        other => (VenueStatus::Unknown(other.to_string()), false),
    };
    Ok(VenueOrderState {
        venue_order_id: id.to_string(),
        status,
        filled_native: filled,
        avg_price_native: avg,
        is_final: fin,
        raw: o.clone(),
    })
}

#[async_trait]
impl ExecutionVenue for DeriveTestnetExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        TESTNET_V3_NETWORK
    }

    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        Some(format!("nonce:{}", nonce_for(order)))
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let (data, max_fee) = self.trade_data(o)?;
        let nonce = nonce_for(o);
        let expiry = (o.timestamp + SIGNATURE_TTL_SECS) as u64;
        let digest = action_digest(
            self.domain,
            self.subaccount_id,
            nonce,
            &encode_trade_data(&data),
            expiry,
            &self.owner,
            &self.signer,
        );
        let sig = sign_digest(&self.signer_key.0, &digest)
            .map_err(|e| invalid(P, format!("signing failed: {e}")))?;
        let params = json!({
            "subaccount_id": self.subaccount_id,
            "instrument_name": o.native_id,
            "direction": if o.side == Side::Buy { "buy" } else { "sell" },
            "limit_price": dec_str(o.limit_price_native),
            "amount": dec_str(o.qty_native),
            "max_fee": dec_str(max_fee),
            "nonce": nonce.to_string(),
            "signer": Self::hex20(&self.signer),
            "signature": format!("0x{}", hex::encode(sig)),
            "signature_expiry_sec": expiry,
            "order_type": "limit",
            "time_in_force": match o.time_in_force { crate::exec::TimeInForce::Ioc => "ioc", crate::exec::TimeInForce::Gtc => "gtc" },
            "label": format!("oa-{}", o.salt),
        });
        let r = self.rpc("private/order", params).await?;
        parse_order(&r["order"])
    }

    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        if let Some(n) = id.strip_prefix("nonce:") {
            // Reconciliation by nonce: scan the recent order history (nonce encodes its ms timestamp).
            let ms: i64 = n
                .parse::<u128>()
                .map(|x| (x / 1_000_000) as i64)
                .unwrap_or(0);
            let r = self
                .rpc("private/get_order_history", json!({"subaccount_id": self.subaccount_id, "from_timestamp": ms - 120_000, "to_timestamp": ms + 600_000, "page": 1, "page_size": 100}))
                .await?;
            let found = r["orders"].as_array().into_iter().flatten().find(|o| {
                o["nonce"]
                    .as_str()
                    .map(str::to_string)
                    .or(o["nonce"].as_u64().map(|x| x.to_string()))
                    .as_deref()
                    == Some(n)
            });
            return found.map(parse_order).transpose();
        }
        match self
            .rpc(
                "private/get_order",
                json!({"subaccount_id": self.subaccount_id, "order_id": id}),
            )
            .await
        {
            Ok(r) => parse_order(r.get("order").unwrap_or(&r)).map(Some),
            Err(ConnectorError::Http {
                status: 400, body, ..
            }) if body.to_lowercase().contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
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
        let instrument = current.raw["instrument_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        self.rpc("private/cancel", json!({"subaccount_id": self.subaccount_id, "instrument_name": instrument, "order_id": current.venue_order_id})).await?;
        Ok(self
            .get_order(&current.venue_order_id)
            .await?
            .unwrap_or(current))
    }

    fn identity(&self) -> Value {
        json!({"venue": TESTNET_V3_NETWORK, "owner": Self::hex20(&self.owner), "subaccount_id": self.subaccount_id,
               "signer_address": Self::hex20(&self.signer), "session_key": self.signer != self.owner,
               "domain_separator": format!("0x{}", hex::encode(self.domain))})
    }

    /// `private/get_subaccount`: available = USDC collateral amount (conservative upper bound for
    /// buying options; margin details kept in `raw`).
    async fn balance(&self) -> Result<VenueBalance> {
        let r = self
            .rpc(
                "private/get_subaccount",
                json!({"subaccount_id": self.subaccount_id}),
            )
            .await?;
        let usdc = r["collaterals"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| {
                c["asset_name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case("USDC"))
            })
            .map(|c| opt_dec(P, c.get("amount"), "amount"))
            .transpose()?
            .flatten()
            .unwrap_or_default();
        Ok(VenueBalance {
            available: usdc,
            equity: opt_dec(P, r.get("subaccount_value"), "subaccount_value")?,
            currency: "USDC".into(),
            raw: json!({"subaccount_id": r["subaccount_id"], "subaccount_value": r["subaccount_value"], "initial_margin": r["initial_margin"], "open_orders_margin": r["open_orders_margin"]}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn vectors() -> Value {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/derive_signing_vectors.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn constants_match_derive_documentation() {
        assert_eq!(
            format!("0x{}", hex::encode(domain_separator(CHAIN_ID))),
            DOCUMENTED_TESTNET_DOMAIN
        );
        assert_eq!(
            format!("0x{}", hex::encode(action_typehash())),
            "0x4d7a9f27c403ff9c0f19bce61d76d82f9aa29f8d6d4b0c5474607d9770d1af17"
        );
        let v = vectors();
        assert_eq!(
            format!("0x{}", hex::encode(domain_separator(1))),
            v["domain_mainnet"].as_str().unwrap()
        );
    }

    #[test]
    fn order_signing_matches_reference_implementation() {
        let v = vectors();
        let key = SigningKey::from_slice(&keccak(b"cow")).unwrap();
        let signer = eth_address(&key);
        for c in v["cases"].as_array().unwrap() {
            let d = |k: &str| c[k].as_str().unwrap().parse::<Decimal>().unwrap();
            let data = TradeData {
                asset: parse_address(c["asset"].as_str().unwrap()).unwrap(),
                sub_id: c["sub_id"].as_str().unwrap().parse().unwrap(),
                limit_price_e18: e18(d("limit_price")).unwrap(),
                amount_e18: e18(d("amount")).unwrap(),
                max_fee_e18: e18(d("max_fee")).unwrap(),
                recipient_id: c["subaccount_id"].as_str().unwrap().parse().unwrap(),
                is_bid: c["is_bid"].as_bool().unwrap(),
            };
            let enc = encode_trade_data(&data);
            assert_eq!(
                format!("0x{}", hex::encode(&enc)),
                c["encoded_data"].as_str().unwrap()
            );
            let dg = action_digest(
                domain_separator(c["chain_id"].as_str().unwrap().parse().unwrap()),
                c["subaccount_id"].as_str().unwrap().parse().unwrap(),
                c["nonce"].as_str().unwrap().parse().unwrap(),
                &enc,
                c["expiry"].as_str().unwrap().parse().unwrap(),
                &parse_address(c["owner"].as_str().unwrap()).unwrap(),
                &signer,
            );
            assert_eq!(
                format!("0x{}", hex::encode(dg)),
                c["digest"].as_str().unwrap()
            );
            assert_eq!(
                format!("0x{}", hex::encode(sign_digest(&key, &dg).unwrap())),
                c["signature"].as_str().unwrap()
            );
        }
        let ts = v["login"]["timestamp"].as_str().unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(personal_sign(&key, ts).unwrap())),
            v["login"]["signature"].as_str().unwrap()
        );
    }

    #[test]
    fn e18_refuses_what_the_exchange_would_reject() {
        assert_eq!(e18("310".parse().unwrap()), Some(310 * 10u128.pow(18)));
        assert_eq!(e18("0.000000000001".parse().unwrap()), Some(1_000_000));
        assert_eq!(e18("0.0000000000001".parse().unwrap()), None, "13 decimals");
        assert_eq!(e18("-1".parse().unwrap()), None);
    }

    #[test]
    fn status_mapping() {
        let s = |st: &str, f: &str| {
            parse_order(&json!({"order_id": "u", "order_status": st, "filled_amount": f})).unwrap()
        };
        assert_eq!(s("filled", "1").status, VenueStatus::Filled);
        let p = s("cancelled", "0.5");
        assert_eq!((p.status, p.is_final), (VenueStatus::PartiallyFilled, true));
        assert_eq!(s("cancelled", "0").status, VenueStatus::Cancelled);
        assert_eq!(s("insufficient_margin", "0").status, VenueStatus::Rejected);
        let o = s("open", "0.2");
        assert_eq!(
            (o.status, o.is_final),
            (VenueStatus::PartiallyFilled, false)
        );
        assert!(!s("weird", "0").is_final);
    }

    #[test]
    fn nonce_is_reproducible_and_secrets_are_redacted() {
        let o = ExecOrder {
            native_id: "BTC-X".into(),
            venue_instrument_id: None,
            venue_meta: Value::Null,
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::ONE,
            limit_price_native: Decimal::ONE,
            salt: 42_123_456,
            timestamp: 1_791_210_000,
            time_in_force: crate::exec::TimeInForce::Ioc,
        };
        assert_eq!(nonce_for(&o), 1_791_210_000_000_000_000u128 + 123_456);
        let env = |k: &str| match k {
            "OA_DERIVE_TESTNET_WALLET" => {
                Some("0x0000000000000000000000000000000000000001".to_string())
            }
            "OA_DERIVE_TESTNET_SUBACCOUNT_ID" => Some("9".to_string()),
            "OA_DERIVE_TESTNET_SIGNER_KEY" => Some(
                "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318".to_string(),
            ),
            _ => None,
        };
        let e = DeriveTestnetExec::testnet_from_lookup(env, TESTNET_REST).unwrap();
        let dbg = format!("{e:?} {}", e.identity());
        assert!(!dbg.contains("4c0883a6"), "{dbg}");
        assert_eq!(e.network(), TESTNET_V3_NETWORK);
    }
}
