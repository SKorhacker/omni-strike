//! NON-CUSTODIAL Limitless execution (Base, CTF exchange): the END USER's EOA signs the EIP-712
//! `Order`; the integrator HMAC-authenticates the exact request body with the user's scoped API
//! token (the secret never reaches us); we verify the signer and forward.
//!
//! Source: docs.limitless.exchange developers/{eip712-signing, authentication, quickstart/python}
//! (read 2026-10-08): domain {name "Limitless CTF Exchange", version "1", chainId 8453,
//! verifyingContract = market venue.exchange}; `Order(uint256 salt,address maker,address signer,
//! address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,
//! uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)`, expiration = nonce = 0,
//! side 0 BUY, signatureType 0 EOA, feeRateBps = the user's `rank.feeRateBps`. FAK BUY: makerAmount
//! = price x size x 1e6 (spend cap), takerAmount = size x 1e6 (minimum shares). Body
//! `{"order": {...fields, "expiration": "0", "price", "signature"}, "ownerId", "orderType": "FAK",
//! "marketSlug"}`; headers lmts-api-key / lmts-timestamp (ISO-8601) / lmts-signature =
//! base64(HMAC-SHA256(secret, "<ts>\nPOST\n/orders\n<body>")). Profile must be in `eoa` mode.

use crate::aevo_exec::{address_word, keccak, parse_address, u256};
use crate::derive_user::{hex20, recover_address};
use crate::exec::{ExecOrder, ExecutionVenue, VenueBalance, VenueOrderState, VenueStatus};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Limitless;
pub const API: &str = crate::limitless::API;
pub const SIGNATURE_PLACEHOLDER: &str = "__SIGNATURE__";

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

/// Decimal string (up to 78 digits) -> 32-byte big-endian word.
pub fn dec_to_word(s: &str) -> Option<[u8; 32]> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut w = [0u8; 32];
    for d in s.bytes() {
        let mut carry = (d - b'0') as u32;
        for byte in w.iter_mut().rev() {
            let v = *byte as u32 * 10 + carry;
            *byte = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return None; // overflow
        }
    }
    Some(w)
}

#[derive(Debug, Clone)]
pub struct LimitlessAccount {
    pub maker: [u8; 20],
    pub owner_id: u64,
    pub fee_rate_bps: u32,
    pub api_token_id: String,
}

impl LimitlessAccount {
    pub fn from_json(v: &Value) -> std::result::Result<Self, String> {
        Ok(Self {
            maker: v["maker"].as_str().and_then(parse_address).ok_or("maker: the user's EOA (profile in eoa mode)")?,
            owner_id: v["owner_id"].as_u64().ok_or("owner_id: the user's Limitless profile id")?,
            fee_rate_bps: v["fee_rate_bps"].as_u64().map(|x| x as u32).ok_or("fee_rate_bps: rank.feeRateBps from GET /profiles/me")?,
            api_token_id: v["api_token_id"].as_str().filter(|s| !s.is_empty() && s.len() <= 128).ok_or("api_token_id: the user's scoped token id (the secret stays with the integrator)")?.to_string(),
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"maker": hex20(&self.maker), "owner_id": self.owner_id, "fee_rate_bps": self.fee_rate_bps, "api_token_id": self.api_token_id})
    }
}

#[derive(Debug, Clone)]
pub struct PreparedLimitless {
    pub digest: [u8; 32],
    pub typed_data: Value,
    pub body_template: String,
}

pub fn prepare_order(o: &ExecOrder, a: &LimitlessAccount) -> Result<PreparedLimitless> {
    use rust_decimal::prelude::ToPrimitive;
    if o.side != Side::Buy {
        return Err(bad("only buys are integrated"));
    }
    let exchange_s = o.venue_meta["exchange"]
        .as_str()
        .ok_or_else(|| bad("venue exchange missing"))?;
    let exchange = parse_address(exchange_s).ok_or_else(|| bad("venue exchange"))?;
    let token = o.venue_meta["yes_token"]
        .as_str()
        .ok_or_else(|| bad("yes token missing"))?;
    let token_w = dec_to_word(token).ok_or_else(|| bad("token id"))?;
    let slug = o.native_id.as_str();
    if o.limit_price_native.round_dp(3) != o.limit_price_native {
        return Err(bad("price: at most 3 decimals"));
    }
    let maker_amount = (o.limit_price_native * o.qty_native * Decimal::from(1_000_000))
        .floor()
        .to_u128()
        .ok_or_else(|| bad("amount"))?;
    let taker_amount = (o.qty_native * Decimal::from(1_000_000))
        .floor()
        .to_u128()
        .ok_or_else(|| bad("size"))?;
    let salt = (o.timestamp.max(0) as u128) * 1000 + (o.salt % 1000) as u128;

    let mut d = Vec::with_capacity(160);
    d.extend_from_slice(&keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    d.extend_from_slice(&keccak(b"Limitless CTF Exchange"));
    d.extend_from_slice(&keccak(b"1"));
    d.extend_from_slice(&u256(8453));
    d.extend_from_slice(&address_word(&exchange));
    let domain = keccak(&d);
    let mut s = Vec::with_capacity(32 * 13);
    s.extend_from_slice(&keccak(b"Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)"));
    s.extend_from_slice(&u256(salt));
    s.extend_from_slice(&address_word(&a.maker));
    s.extend_from_slice(&address_word(&a.maker));
    s.extend_from_slice(&address_word(&[0u8; 20]));
    s.extend_from_slice(&token_w);
    s.extend_from_slice(&u256(maker_amount));
    s.extend_from_slice(&u256(taker_amount));
    s.extend_from_slice(&u256(0));
    s.extend_from_slice(&u256(0));
    s.extend_from_slice(&u256(a.fee_rate_bps as u128));
    s.extend_from_slice(&u256(0)); // side BUY
    s.extend_from_slice(&u256(0)); // EOA
    let mut m = vec![0x19, 0x01];
    m.extend_from_slice(&domain);
    m.extend_from_slice(&keccak(&s));
    let digest = keccak(&m);

    let maker = hex20(&a.maker);
    let message = json!({"salt": salt.to_string(), "maker": maker, "signer": maker, "taker": "0x0000000000000000000000000000000000000000",
                         "tokenId": token, "makerAmount": maker_amount.to_string(), "takerAmount": taker_amount.to_string(),
                         "expiration": "0", "nonce": "0", "feeRateBps": a.fee_rate_bps.to_string(), "side": 0, "signatureType": 0});
    let typed_data = json!({
        "types": {
            "EIP712Domain": [{"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                             {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}],
            "Order": [{"name": "salt", "type": "uint256"}, {"name": "maker", "type": "address"}, {"name": "signer", "type": "address"},
                      {"name": "taker", "type": "address"}, {"name": "tokenId", "type": "uint256"}, {"name": "makerAmount", "type": "uint256"},
                      {"name": "takerAmount", "type": "uint256"}, {"name": "expiration", "type": "uint256"}, {"name": "nonce", "type": "uint256"},
                      {"name": "feeRateBps", "type": "uint256"}, {"name": "side", "type": "uint8"}, {"name": "signatureType", "type": "uint8"}]
        },
        "primaryType": "Order",
        "domain": {"name": "Limitless CTF Exchange", "version": "1", "chainId": 8453, "verifyingContract": exchange_s},
        "message": message
    });
    let price: f64 = o.limit_price_native.to_f64().ok_or_else(|| bad("price"))?;
    let body_template = json!({
        "order": {"salt": salt as u64, "maker": maker, "signer": maker, "taker": "0x0000000000000000000000000000000000000000",
                  "tokenId": token, "makerAmount": maker_amount as u64, "takerAmount": taker_amount as u64, "expiration": "0",
                  "nonce": 0, "feeRateBps": a.fee_rate_bps, "side": 0, "signatureType": 0, "price": price, "signature": SIGNATURE_PLACEHOLDER},
        "ownerId": a.owner_id, "orderType": "FAK", "marketSlug": slug
    })
    .to_string();
    Ok(PreparedLimitless {
        digest,
        typed_data,
        body_template,
    })
}

#[derive(Debug)]
pub struct LimitlessUserSignedExec {
    http: reqwest::Client,
    api: String,
}

impl LimitlessUserSignedExec {
    pub fn production() -> Self {
        Self::with_api(API)
    }
    pub fn with_api(api: &str) -> Self {
        Self {
            http: client(),
            api: api.trim_end_matches('/').into(),
        }
    }
}

#[async_trait]
impl ExecutionVenue for LimitlessUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        crate::limitless::NETWORK
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        Some(format!(
            "salt:{}",
            (o.timestamp.max(0) as u128) * 1000 + (o.salt % 1000) as u128
        ))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let a = LimitlessAccount::from_json(u).map_err(bad)?;
        let sig = u["signature"]
            .as_str()
            .ok_or_else(|| bad("missing user signature"))?;
        let auth = &u["venue_auth"];
        let ts = auth["timestamp"]
            .as_str()
            .ok_or_else(|| bad("venue_auth.timestamp (ISO-8601)"))?;
        let hmac = auth["hmac"]
            .as_str()
            .ok_or_else(|| bad("venue_auth.hmac (base64)"))?;
        let p = prepare_order(o, &a)?;
        if recover_address(&p.digest, sig) != Some(a.maker) {
            return Err(bad("order signature does not match the maker"));
        }
        let body = p.body_template.replace(SIGNATURE_PLACEHOLDER, sig);
        let v = read_json(
            P,
            self.http
                .post(format!("{}/orders", self.api))
                .header("lmts-api-key", &a.api_token_id)
                .header("lmts-timestamp", ts)
                .header("lmts-signature", hmac)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await,
        )
        .await?;
        let id = v["order"]["id"]
            .as_str()
            .or(v["id"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.precompute_order_id(o).unwrap_or_default());
        Ok(VenueOrderState {
            venue_order_id: id,
            status: VenueStatus::Unknown(
                "submitted (FAK): check fills on the user's account".into(),
            ),
            filled_native: Decimal::ZERO,
            avg_price_native: None,
            is_final: false,
            raw: v,
        })
    }
    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None)
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (FAK)"),
        })
    }
    fn identity(&self) -> Value {
        json!({"venue": crate::limitless::NETWORK, "custody": "NON_CUSTODIAL", "signed_by": "end user EOA (EIP-712) + integrator-side HMAC"})
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

    fn vector() -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/limitless_eip712_vector.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn big_token_ids_encode_to_words() {
        let w = dec_to_word("255").unwrap();
        assert_eq!(w[31], 255);
        assert!(dec_to_word("1".repeat(80).as_str()).is_none());
        assert!(dec_to_word("12a").is_none());
    }

    /// Digest equals eth_account's on the same order (vector generated independently).
    #[test]
    fn digest_and_signature_match_eth_account() {
        let v = vector();
        let maker = parse_address(v["maker"].as_str().unwrap()).unwrap();
        let o = ExecOrder {
            native_id: "btc-up-or-down-daily".into(),
            venue_instrument_id: None,
            venue_meta: json!({"exchange": v["exchange"], "yes_token": v["token_id"]}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(10, 0),
            limit_price_native: Decimal::new(358, 3),
            salt: 42,
            timestamp: 1_791_480_000,
            time_in_force: crate::exec::TimeInForce::Ioc,
        };
        let a = LimitlessAccount {
            maker,
            owner_id: 7,
            fee_rate_bps: 300,
            api_token_id: "tok".into(),
        };
        let p = prepare_order(&o, &a).unwrap();
        assert_eq!(
            format!("0x{}", hex::encode(p.digest)),
            v["digest"].as_str().unwrap()
        );
        assert_eq!(
            recover_address(&p.digest, v["signature"].as_str().unwrap()),
            Some(maker)
        );
        assert!(p.body_template.contains("\"orderType\":\"FAK\""));
    }
}
