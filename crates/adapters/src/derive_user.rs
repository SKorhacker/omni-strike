//! NON-CUSTODIAL Derive execution: the END USER signs, Omni Strike never holds a key.
//!
//! Flow (per order):
//! 1. `prepare_order` builds the exact EIP-712 `Action` the user must sign (typed data for
//!    `eth_signTypedData_v4` + its digest) — same encoding as `derive_exec` (tested against Derive's
//!    reference vectors).
//! 2. The user's wallet / session key signs it, plus an EIP-191 `personal_sign` of a millisecond
//!    timestamp (Derive's REST authentication).
//! 3. `DeriveUserSignedExec::submit` RECOVERS both signers locally and refuses anything that does not
//!    match the declared signer (or a stale auth timestamp) before forwarding to Derive.
//!
//! The signatures travel inside `ExecOrder::venue_meta["user"]`:
//! `{owner, signer, subaccount_id, signature, auth_timestamp_ms, auth_signature}`.
//! Reconciliation: the server holds no credentials, so it cannot query the user's private order
//! history; IOC answers are final in the submit response, anything else stays UNKNOWN for the user /
//! integrator to check (documented in the API).

use crate::aevo_exec::{keccak, parse_address};
use crate::derive_exec::{
    TESTNET_REST, TESTNET_V3_NETWORK, TRADE_MODULE, TradeData, action_digest, dec_str,
    domain_separator, e18, encode_trade_data, nonce_for, parse_order,
};

/// One Derive v3 deployment. Only the chain id (hence the EIP-712 domain), the host and our network
/// tag differ: module addresses and the `verifyingContract` are identical on every network
/// [docs.derive.xyz/migrating/breaking-changes, read 2026-10-09].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeriveNet {
    pub chain_id: u64,
    pub rest: &'static str,
    pub network: &'static str,
}

impl DeriveNet {
    /// Ethereum Sepolia (mintable test tokens).
    pub const TESTNET: Self = Self {
        chain_id: 11_155_111,
        rest: TESTNET_REST,
        network: TESTNET_V3_NETWORK,
    };
    /// Ethereum mainnet (v3 zk-app settling on L1). REAL funds: the end user signs every order.
    pub const MAINNET: Self = Self {
        chain_id: 1,
        rest: crate::derive::MAINNET_REST,
        network: crate::derive::NETWORK,
    };
    /// The deployment a venue advertises in `identity()["chain_id"]`; unknown ids are refused.
    pub fn by_chain_id(id: u64) -> Option<Self> {
        [Self::TESTNET, Self::MAINNET]
            .into_iter()
            .find(|n| n.chain_id == id)
    }
}
use crate::exec::{ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result};
use async_trait::async_trait;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Derive;
const MATCHING: &str = "0xeB8d770ec18DB98Db922E9D83260A585b9F0DeAD";
/// Signed orders stay valid this long (the user has this window to sign and submit).
pub const SIGNATURE_TTL_SECS: i64 = 300;
/// Auth timestamp tolerance (Derive also enforces its own window).
const AUTH_MAX_SKEW_MS: i64 = 120_000;

/// The user's Derive account, declared at prepare time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAccount {
    pub owner: [u8; 20],
    pub signer: [u8; 20],
    pub subaccount_id: u64,
}

impl UserAccount {
    pub fn from_json(v: &Value) -> std::result::Result<Self, String> {
        let addr = |k: &str| {
            v[k].as_str()
                .and_then(parse_address)
                .ok_or_else(|| format!("{k}: expected a 0x-prefixed 20-byte address"))
        };
        let owner = addr("owner")?;
        // Signer defaults to the owner (wallet signs directly); a session key is a separate address.
        let signer = if v["signer"].is_null() {
            owner
        } else {
            addr("signer")?
        };
        let subaccount_id = v["subaccount_id"]
            .as_u64()
            .or_else(|| v["subaccount_id"].as_str().and_then(|s| s.parse().ok()))
            .filter(|x| *x > 0)
            .ok_or("subaccount_id: expected a positive integer")?;
        Ok(Self {
            owner,
            signer,
            subaccount_id,
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"owner": hex20(&self.owner), "signer": hex20(&self.signer), "subaccount_id": self.subaccount_id})
    }
}

pub fn hex20(a: &[u8; 20]) -> String {
    format!("0x{}", hex::encode(a))
}

/// Everything the user needs to sign one order.
#[derive(Debug, Clone)]
pub struct PreparedOrder {
    pub nonce: u128,
    pub expiry: u64,
    pub digest: [u8; 32],
    /// `eth_signTypedData_v4` payload.
    pub typed_data: Value,
    /// Order parameters as sent to Derive (without signature fields).
    pub params: Value,
}

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    // "order not representable" = definite rejection upstream (nothing was sent to the venue).
    crate::not_representable(P, d)
}

pub fn prepare_order(o: &ExecOrder, acct: &UserAccount, net: DeriveNet) -> Result<PreparedOrder> {
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
    let data = TradeData {
        asset,
        sub_id,
        limit_price_e18: e18(o.limit_price_native).ok_or_else(|| bad("limit price"))?,
        amount_e18: e18(o.qty_native).ok_or_else(|| bad("amount"))?,
        max_fee_e18: e18(max_fee).ok_or_else(|| bad("max fee"))?,
        recipient_id: acct.subaccount_id,
        is_bid: o.side == Side::Buy,
    };
    let encoded = encode_trade_data(&data);
    let nonce = nonce_for(o);
    let expiry = (o.timestamp + SIGNATURE_TTL_SECS) as u64;
    let digest = action_digest(
        domain_separator(net.chain_id),
        acct.subaccount_id,
        nonce,
        &encoded,
        expiry,
        &acct.owner,
        &acct.signer,
    );
    let typed_data = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"}, {"name": "verifyingContract", "type": "address"}
            ],
            "Action": [
                {"name": "subaccountId", "type": "uint256"}, {"name": "nonce", "type": "uint256"},
                {"name": "module", "type": "address"}, {"name": "data", "type": "bytes"},
                {"name": "expiry", "type": "uint256"}, {"name": "owner", "type": "address"},
                {"name": "signer", "type": "address"}
            ]
        },
        "primaryType": "Action",
        "domain": {"name": "Matching", "version": "1.0", "chainId": net.chain_id, "verifyingContract": MATCHING},
        "message": {
            "subaccountId": acct.subaccount_id.to_string(),
            "nonce": nonce.to_string(),
            "module": TRADE_MODULE,
            "data": format!("0x{}", hex::encode(&encoded)),
            "expiry": expiry.to_string(),
            "owner": hex20(&acct.owner),
            "signer": hex20(&acct.signer),
        }
    });
    let params = json!({
        "subaccount_id": acct.subaccount_id,
        "instrument_name": o.native_id,
        "direction": if o.side == Side::Buy { "buy" } else { "sell" },
        "limit_price": dec_str(o.limit_price_native),
        "amount": dec_str(o.qty_native),
        "max_fee": dec_str(max_fee),
        "nonce": nonce.to_string(),
        "signer": hex20(&acct.signer),
        "signature_expiry_sec": expiry,
        "order_type": "limit",
        "time_in_force": match o.time_in_force { TimeInForce::Ioc => "ioc", TimeInForce::Gtc => "gtc" },
        "label": format!("oa-{}", o.salt),
    });
    Ok(PreparedOrder {
        nonce,
        expiry,
        digest,
        typed_data,
        params,
    })
}

/// Why a subaccount lookup failed: our local check of the user's authentication, or the venue.
#[derive(Debug)]
pub enum LookupError {
    Auth(&'static str),
    Venue(ConnectorError),
}

/// Local check of a wallet's proof of control: `personal_sign` of a fresh ms timestamp (Derive's REST
/// authentication). Returns the wallet address; nothing is stored.
pub fn check_wallet_auth(
    wallet: &str,
    timestamp_ms: i64,
    signature: &str,
) -> std::result::Result<[u8; 20], &'static str> {
    check_wallet_message(wallet, timestamp_ms, signature, |_| {
        timestamp_ms.to_string()
    })
}

/// Same check for a message of our own, built from the lowercased wallet address (e.g. a
/// domain-bound "read my orders" message that is useless anywhere else).
pub fn check_wallet_message(
    wallet: &str,
    timestamp_ms: i64,
    signature: &str,
    message: impl FnOnce(&str) -> String,
) -> std::result::Result<[u8; 20], &'static str> {
    let w = parse_address(wallet).ok_or("wallet: not a 0x address")?;
    if (chrono::Utc::now().timestamp_millis() - timestamp_ms).abs() > AUTH_MAX_SKEW_MS {
        return Err("timestamp_ms is stale: sign a fresh timestamp");
    }
    if recover_address(&personal_digest(&message(&hex20(&w))), signature) != Some(w) {
        return Err("signature does not match the wallet");
    }
    Ok(w)
}

/// READ-ONLY private Derive call made with the user's own REST authentication (checked locally
/// first). Returns `result`.
async fn private_read(
    rest: &str,
    method: &str,
    wallet: &str,
    timestamp_ms: i64,
    signature: &str,
    body: impl FnOnce(&str) -> Value,
) -> std::result::Result<Value, LookupError> {
    let w = hex20(&check_wallet_auth(wallet, timestamp_ms, signature).map_err(LookupError::Auth)?);
    let resp = client()
        .post(format!("{}/{method}", rest.trim_end_matches('/')))
        .header("X-DeriveWallet", &w)
        .header("X-DeriveTimestamp", timestamp_ms.to_string())
        .header("X-DeriveSignature", signature)
        .json(&body(&w))
        .send()
        .await;
    let v = read_json(P, resp).await.map_err(LookupError::Venue)?;
    if let Some(err) = v.get("error") {
        return Err(LookupError::Venue(ConnectorError::Http {
            platform: P,
            status: 400,
            body: err.to_string().chars().take(300).collect(),
        }));
    }
    Ok(v["result"].clone())
}

/// READ-ONLY: the subaccount ids of `wallet` (Derive `private/get_subaccounts`). Lets a web user find
/// the subaccount id without leaving the page.
pub async fn list_subaccounts(
    rest: &str,
    wallet: &str,
    timestamp_ms: i64,
    signature: &str,
) -> std::result::Result<Vec<u64>, LookupError> {
    let r = private_read(
        rest,
        "private/get_subaccounts",
        wallet,
        timestamp_ms,
        signature,
        |w| json!({"wallet": w}),
    )
    .await?;
    r["subaccount_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .ok_or_else(|| LookupError::Venue(bad("get_subaccounts: no subaccount_ids in the answer")))
}

/// READ-ONLY: one subaccount of `wallet` as Derive reports it (`private/get_subaccount`: collaterals,
/// positions, open orders, value and margins), passed through unchanged. Derive refuses a subaccount
/// the authenticated wallet does not own.
pub async fn get_subaccount(
    rest: &str,
    wallet: &str,
    timestamp_ms: i64,
    signature: &str,
    subaccount_id: u64,
) -> std::result::Result<Value, LookupError> {
    let r = private_read(
        rest,
        "private/get_subaccount",
        wallet,
        timestamp_ms,
        signature,
        |_| json!({"subaccount_id": subaccount_id}),
    )
    .await?;
    if r.is_object() {
        Ok(r)
    } else {
        Err(LookupError::Venue(bad("get_subaccount: empty answer")))
    }
}

/// EIP-191 digest of a text message (what `personal_sign` signs).
pub fn personal_digest(text: &str) -> [u8; 32] {
    let mut m = format!("\x19Ethereum Signed Message:\n{}", text.len()).into_bytes();
    m.extend_from_slice(text.as_bytes());
    keccak(&m)
}

/// Address that produced a 65-byte (r || s || v) signature over `digest`.
pub fn recover_address(digest: &[u8; 32], sig_hex: &str) -> Option<[u8; 20]> {
    let b = hex::decode(sig_hex.trim().trim_start_matches("0x")).ok()?;
    if b.len() != 65 {
        return None;
    }
    let sig = Signature::from_slice(&b[..64]).ok()?;
    let v = if b[64] >= 27 { b[64] - 27 } else { b[64] };
    let rid = RecoveryId::from_byte(v)?;
    let vk = VerifyingKey::recover_from_prehash(digest, &sig, rid).ok()?;
    let pt = vk.to_encoded_point(false);
    let h = keccak(&pt.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Some(a)
}

/// Executes orders signed by the end user. Holds NO key.
#[derive(Debug)]
pub struct DeriveUserSignedExec {
    http: reqwest::Client,
    rest: String,
    net: DeriveNet,
}

impl DeriveUserSignedExec {
    pub fn testnet() -> Self {
        Self::on(DeriveNet::TESTNET, TESTNET_REST)
    }
    /// v3 MAINNET (real funds). Only attached by the `mainnet-nc` build; the user signs everything.
    pub fn mainnet() -> Self {
        Self::on(DeriveNet::MAINNET, DeriveNet::MAINNET.rest)
    }
    /// Testnet signing domain against another host (tests).
    pub fn with_rest(rest: &str) -> Self {
        Self::on(DeriveNet::TESTNET, rest)
    }
    pub fn on(net: DeriveNet, rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            net,
        }
    }
    pub fn net(&self) -> DeriveNet {
        self.net
    }

    /// Local checks of a user-signed order: declared account, fresh auth timestamp, unexpired
    /// signature window, order AND auth signatures recovered to the declared signer.
    fn verify_user<'a>(
        &self,
        o: &'a ExecOrder,
    ) -> Result<(UserAccount, PreparedOrder, &'a str, &'a str, i64)> {
        let u = &o.venue_meta["user"];
        let acct = UserAccount::from_json(u).map_err(bad)?;
        let sig = u["signature"]
            .as_str()
            .ok_or_else(|| bad("missing user signature"))?;
        let auth_sig = u["auth_signature"]
            .as_str()
            .ok_or_else(|| bad("missing auth_signature"))?;
        let ts: i64 = u["auth_timestamp_ms"]
            .as_i64()
            .or_else(|| u["auth_timestamp_ms"].as_str().and_then(|s| s.parse().ok()))
            .ok_or_else(|| bad("missing auth_timestamp_ms"))?;
        if (chrono::Utc::now().timestamp_millis() - ts).abs() > AUTH_MAX_SKEW_MS {
            return Err(bad("auth_timestamp_ms is stale: sign a fresh timestamp"));
        }
        let prepared = prepare_order(o, &acct, self.net)?;
        if (prepared.expiry as i64) <= chrono::Utc::now().timestamp() {
            return Err(bad("order signature window expired: prepare again"));
        }
        if recover_address(&prepared.digest, sig) != Some(acct.signer) {
            return Err(bad("order signature does not match the declared signer"));
        }
        if recover_address(&personal_digest(&ts.to_string()), auth_sig) != Some(acct.signer) {
            return Err(bad("auth signature does not match the declared signer"));
        }
        Ok((acct, prepared, sig, auth_sig, ts))
    }
}

#[async_trait]
impl ExecutionVenue for DeriveUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.net.network
    }
    fn precompute_order_id(&self, order: &ExecOrder) -> Option<String> {
        Some(format!("nonce:{}", nonce_for(order)))
    }

    fn check_user_signatures(&self, o: &ExecOrder) -> Result<()> {
        self.verify_user(o).map(|_| ())
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let (acct, prepared, sig, auth_sig, ts) = self.verify_user(o)?;
        let mut params = prepared.params;
        params["signature"] = json!(sig);
        let resp = self
            .http
            .post(format!("{}/private/order", self.rest))
            .header("X-DeriveWallet", hex20(&acct.owner))
            .header("X-DeriveTimestamp", ts.to_string())
            .header("X-DeriveSignature", auth_sig)
            .json(&params)
            .send()
            .await;
        let v = read_json(P, resp).await?;
        if let Some(err) = v.get("error") {
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
        parse_order(&v["result"]["order"])
    }

    /// No server-side credentials: the order is known only through the submit answer.
    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None)
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (user-signed orders are IOC; cancel from the user's wallet)"),
        })
    }

    fn identity(&self) -> Value {
        json!({"venue": self.net.network, "custody": "NON_CUSTODIAL", "signed_by": "end user (wallet or session key)",
               "chain_id": self.net.chain_id, "rest": self.rest,
               "domain_separator": format!("0x{}", hex::encode(domain_separator(self.net.chain_id)))})
    }

    async fn balance(&self) -> Result<VenueBalance> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: "balance: user-held account, query it with the user's credentials".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aevo_exec::{eth_address, sign_digest};
    use k256::ecdsa::SigningKey;
    use oa_core::Decimal;

    fn order() -> ExecOrder {
        ExecOrder {
            native_id: "ETH-20261030-2600-C".into(),
            venue_instrument_id: None,
            venue_meta: json!({"base_asset_address": "0x4BB4C3CDc7562f08e9910A0C7D8bB7e108861eB4", "base_asset_sub_id": "39614082287924319838483674368"}),
            max_fee_per_unit: Some(Decimal::new(150, 2)),
            side: Side::Buy,
            qty_native: Decimal::new(1, 1),
            limit_price_native: Decimal::new(4510, 2),
            salt: 42,
            timestamp: chrono::Utc::now().timestamp(),
            time_in_force: TimeInForce::Ioc,
        }
    }

    fn user_key() -> SigningKey {
        SigningKey::from_slice(&[7u8; 32]).unwrap()
    }

    #[test]
    fn prepared_digest_equals_the_custodial_encoding() {
        let k = user_key();
        let a = eth_address(&k);
        let acct = UserAccount {
            owner: a,
            signer: a,
            subaccount_id: 12345,
        };
        let p = prepare_order(&order(), &acct, DeriveNet::TESTNET).unwrap();
        // Same function the tested custodial executor uses; message fields mirror the digest inputs.
        assert_eq!(p.typed_data["message"]["subaccountId"], "12345");
        assert_eq!(p.typed_data["primaryType"], "Action");
        assert_eq!(p.params["direction"], "buy");
        assert!(p.params.get("signature").is_none());
    }

    /// Writes the typed data + our digest so scripts/check_typed_data.py can verify it with an
    /// independent EIP-712 implementation (eth_account): a wallet signing `typed_data` must sign
    /// exactly `digest`.
    #[test]
    fn export_typed_data_vector() {
        let a = eth_address(&user_key());
        let acct = UserAccount {
            owner: a,
            signer: a,
            subaccount_id: 12345,
        };
        let mut o = order();
        o.timestamp = 1_791_400_000;
        for (net, file) in [
            (DeriveNet::TESTNET, "derive_typed_data_vector.json"),
            (DeriveNet::MAINNET, "derive_mainnet_typed_data_vector.json"),
        ] {
            let p = prepare_order(&o, &acct, net).unwrap();
            let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target")
                .join(file);
            std::fs::write(
                out,
                json!({"typed_data": p.typed_data, "digest": format!("0x{}", hex::encode(p.digest))})
                    .to_string(),
            )
            .unwrap();
        }
    }

    /// Mainnet = Ethereum L1 (chainId 1): the separator Derive publishes for v3 mainnet, and a
    /// mainnet signature can never be replayed on testnet (different digest), nor the reverse.
    #[test]
    fn mainnet_domain_is_derive_published_value_and_differs_from_testnet() {
        assert_eq!(
            format!(
                "0x{}",
                hex::encode(domain_separator(DeriveNet::MAINNET.chain_id))
            ),
            "0xda616dfabb88681b08e1592820a41d55ddc62d68de110e327ae99d734506fe19"
        );
        assert_eq!(DeriveNet::by_chain_id(1), Some(DeriveNet::MAINNET));
        assert_eq!(DeriveNet::by_chain_id(957), None);
        let a = eth_address(&user_key());
        let acct = UserAccount {
            owner: a,
            signer: a,
            subaccount_id: 1,
        };
        let o = order();
        let m = prepare_order(&o, &acct, DeriveNet::MAINNET).unwrap();
        let t = prepare_order(&o, &acct, DeriveNet::TESTNET).unwrap();
        assert_ne!(m.digest, t.digest);
        assert_eq!(m.typed_data["domain"]["chainId"], 1);
        assert_eq!(
            DeriveUserSignedExec::mainnet().network(),
            "derive-v3-mainnet"
        );
        assert_eq!(DeriveUserSignedExec::mainnet().identity()["chain_id"], 1);
    }

    #[test]
    fn signer_recovery_accepts_the_right_key_only() {
        let k = user_key();
        let a = eth_address(&k);
        let d = keccak(b"x");
        let sig = format!("0x{}", hex::encode(sign_digest(&k, &d).unwrap()));
        assert_eq!(recover_address(&d, &sig), Some(a));
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let sig2 = format!("0x{}", hex::encode(sign_digest(&other, &d).unwrap()));
        assert_ne!(recover_address(&d, &sig2), Some(a));
        assert_eq!(recover_address(&d, "0x1234"), None);
    }

    #[test]
    fn account_parsing_defaults_signer_to_owner() {
        let a = UserAccount::from_json(
            &json!({"owner": "0x1111111111111111111111111111111111111111", "subaccount_id": 5}),
        )
        .unwrap();
        assert_eq!(a.owner, a.signer);
        assert!(UserAccount::from_json(&json!({"owner": "0x11", "subaccount_id": 5})).is_err());
        assert!(
            UserAccount::from_json(
                &json!({"owner": "0x1111111111111111111111111111111111111111", "subaccount_id": 0})
            )
            .is_err()
        );
    }

    /// Subaccount lookup: bad auth refused locally (nothing forwarded), good auth forwarded with
    /// Derive's headers, ids parsed.
    #[tokio::test]
    async fn subaccount_lookup_checks_auth_then_forwards() {
        use std::sync::{Arc, Mutex};
        let hits: Arc<Mutex<Vec<axum::http::HeaderMap>>> = Arc::default();
        let h2 = hits.clone();
        let app = axum::Router::new().route(
            "/private/get_subaccounts",
            axum::routing::post(move |h: axum::http::HeaderMap| {
                let h2 = h2.clone();
                async move {
                    h2.lock().unwrap().push(h);
                    axum::Json(json!({"result": {"subaccount_ids": [12345, 90001], "wallet": "x"}}))
                }
            }),
        )
        .route(
            "/private/get_subaccount",
            axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                axum::Json(json!({"result": {"subaccount_id": b["subaccount_id"], "subaccount_value": "12.5",
                    "positions": [{"instrument_name": "ETH-20261030-1900-C", "amount": "0.1"}], "collaterals": []}}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let rest = format!("http://{addr}");
        let k = user_key();
        let w = hex20(&eth_address(&k));
        let ts = chrono::Utc::now().timestamp_millis();
        let sign = |text: &str| {
            format!(
                "0x{}",
                hex::encode(sign_digest(&k, &personal_digest(text)).unwrap())
            )
        };
        let sig = sign(&ts.to_string());
        assert!(matches!(
            list_subaccounts(&rest, &w, ts, &sign("1")).await,
            Err(LookupError::Auth(_))
        ));
        let old = ts - 600_000;
        assert!(matches!(
            list_subaccounts(&rest, &w, old, &sign(&old.to_string())).await,
            Err(LookupError::Auth(_))
        ));
        assert!(
            hits.lock().unwrap().is_empty(),
            "bad auth never reaches the venue"
        );
        let ids = list_subaccounts(&rest, &w, ts, &sig).await.unwrap();
        assert_eq!(ids, vec![12345, 90001]);
        assert_eq!(hits.lock().unwrap()[0]["x-derivewallet"], w.as_str());
        let acct = get_subaccount(&rest, &w, ts, &sig, 12345).await.unwrap();
        assert_eq!(acct["subaccount_id"], 12345);
        assert_eq!(acct["positions"][0]["amount"], "0.1");
        assert!(matches!(
            get_subaccount(&rest, &w, ts, &sign("1"), 12345).await,
            Err(LookupError::Auth(_))
        ));
    }

    /// Full submit against a fake Derive: signatures verified, headers and params forwarded.
    #[tokio::test]
    async fn submit_verifies_and_forwards_user_signatures() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<(axum::http::HeaderMap, Value)>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/private/order",
            axum::routing::post(move |h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| {
                let s2 = s2.clone();
                async move {
                    *s2.lock().unwrap() = Some((h, b));
                    axum::Json(json!({"result": {"order": {"order_id": "abc", "order_status": "filled", "filled_amount": "0.1", "average_price": "45", "instrument_name": "ETH-20261030-2600-C"}}}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let k = user_key();
        let a = eth_address(&k);
        let acct = UserAccount {
            owner: a,
            signer: a,
            subaccount_id: 12345,
        };
        let mut o = order();
        let p = prepare_order(&o, &acct, DeriveNet::TESTNET).unwrap();
        let ts = chrono::Utc::now().timestamp_millis();
        let mut u = acct.to_json();
        u["signature"] = json!(format!(
            "0x{}",
            hex::encode(sign_digest(&k, &p.digest).unwrap())
        ));
        u["auth_timestamp_ms"] = json!(ts);
        u["auth_signature"] = json!(format!(
            "0x{}",
            hex::encode(sign_digest(&k, &personal_digest(&ts.to_string())).unwrap())
        ));
        o.venue_meta["user"] = u.clone();
        let ex = DeriveUserSignedExec::with_rest(&format!("http://{addr}"));
        let st = ex.submit(&o).await.unwrap();
        assert_eq!(st.venue_order_id, "abc");
        let (h, b) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(h["x-derivewallet"], hex20(&a));
        assert_eq!(b["signature"], u["signature"]);

        // Tampered order (price changed after signing) is refused locally, never sent.
        *seen.lock().unwrap() = None;
        let mut bad_o = o.clone();
        bad_o.limit_price_native = Decimal::new(9999, 2);
        let e = ex.submit(&bad_o).await.unwrap_err().to_string();
        assert!(e.contains("does not match"), "{e}");
        assert!(seen.lock().unwrap().is_none());

        // Missing signature = definite rejection, nothing sent.
        let mut no_sig = order();
        no_sig.venue_meta["user"] = acct.to_json();
        assert!(
            ex.submit(&no_sig)
                .await
                .unwrap_err()
                .to_string()
                .contains("order not representable")
        );
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live_mainnet {
    use super::*;
    use crate::Connector;
    use crate::aevo_exec::{eth_address, sign_digest};
    use k256::ecdsa::SigningKey;
    use oa_core::Decimal;

    /// READ-ONLY against Derive v3 MAINNET, executes nothing: (1) our order `Action.data` for a real
    /// mainnet instrument goes through Derive's own decoder (`public/decode_action`), (2) if Derive
    /// accepts a throwaway wallet's auth on `private/order_debug` (rebuilds, never executes), its
    /// `typed_data_hash` must equal the digest we hand to users.
    #[tokio::test]
    async fn mainnet_order_encoding_is_accepted_by_derive_v3() {
        let cat = crate::derive::DeriveConnector::default()
            .list_instruments("ETH")
            .await
            .unwrap();
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let o = ExecOrder {
            native_id: i.key.native_id.clone(),
            venue_instrument_id: None,
            venue_meta: json!({"base_asset_address": i.native["base_asset_address"],
                               "base_asset_sub_id": i.native["base_asset_sub_id"]}),
            max_fee_per_unit: Some(Decimal::new(150, 2)),
            side: Side::Buy,
            qty_native: Decimal::ONE,
            limit_price_native: i.tick_size.unwrap_or(Decimal::ONE),
            salt: 7,
            timestamp: chrono::Utc::now().timestamp(),
            time_in_force: TimeInForce::Ioc,
        };
        let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
        let me = eth_address(&key);
        let acct = UserAccount {
            owner: me,
            signer: me,
            subaccount_id: 1,
        };
        let p = prepare_order(&o, &acct, DeriveNet::MAINNET).unwrap();
        let http = client();
        let dec: Value = http
            .post(format!("{}/public/decode_action", DeriveNet::MAINNET.rest))
            .json(&json!({"action_type": "order", "encoded_data": p.typed_data["message"]["data"]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("{} decode_action: {dec}", o.native_id);
        assert!(dec.get("error").is_none(), "{dec}");
        assert!(dec["result"]["decoded"].is_object(), "{dec}");

        let ts = chrono::Utc::now().timestamp_millis();
        let auth = format!(
            "0x{}",
            hex::encode(sign_digest(&key, &personal_digest(&ts.to_string())).unwrap())
        );
        let sig = format!("0x{}", hex::encode(sign_digest(&key, &p.digest).unwrap()));
        let mut params = p.params.clone();
        params["signature"] = json!(sig);
        let dbg: Value = http
            .post(format!("{}/private/order_debug", DeriveNet::MAINNET.rest))
            .header("X-DeriveWallet", hex20(&me))
            .header("X-DeriveTimestamp", ts.to_string())
            .header("X-DeriveSignature", auth)
            .json(&params)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("order_debug: {dbg}");
        if let Some(h) = dbg["result"]["typed_data_hash"].as_str() {
            assert_eq!(
                h.trim_start_matches("0x"),
                hex::encode(p.digest),
                "Derive's mainnet digest differs from ours"
            );
            eprintln!("MATCH: Derive mainnet typed_data_hash == our digest");
        }
    }
}
