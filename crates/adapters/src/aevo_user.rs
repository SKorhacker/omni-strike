//! NON-CUSTODIAL Aevo execution: the END USER signs; Omni Strike holds neither the signing key nor
//! the API secret.
//!
//! Aevo needs two things per order (see `aevo_exec` for the sourced spec):
//! 1. an EIP-712 `Order` signed by the account's registered SIGNING KEY;
//! 2. REST authentication: `AEVO-KEY`, `AEVO-TIMESTAMP` (ns) and `AEVO-SIGNATURE` =
//!    hex(HMAC_SHA256(api_secret, "key,timestamp,POST,/orders,<body>")).
//!
//! `prepare_order` returns the typed data AND the exact request body with a `__SIGNATURE__`
//! placeholder. The integrator signs the typed data, substitutes the signature into the body,
//! computes the HMAC with the user's API secret ON ITS SIDE, and sends us
//! `{signature, timestamp_ns, hmac}`. We rebuild the identical body, recover the signer, and forward.
//! The API key (an identifier, not a secret) travels in the account.
//!
//! Account: `{"maker": "0x…" (Aevo account), "signer": "0x…" (registered signing key), "api_key": "…"}`.

use crate::aevo_exec::{
    OrderFields, domain_separator, fixed6, order_digest, parse_address, parse_state,
};
use crate::derive_user::recover_address;
use crate::exec::{ExecOrder, ExecutionVenue, TimeInForce, VenueBalance, VenueOrderState};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result};
use async_trait::async_trait;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Aevo;
const DOMAIN_VERSION: &str = "1";

/// One Aevo deployment: EIP-712 domain `{name, version "1", chainId}` + REST host + network tag.
/// Source: api-docs.aevo.xyz/reference/signing-orders (read 2026-10-09): mainnet
/// `make_domain(name="Aevo Mainnet", version="1", chainId=1)`, testnet `"Aevo Testnet"`, 11155111.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AevoNet {
    pub domain_name: &'static str,
    pub chain_id: u64,
    pub rest: &'static str,
    pub network: &'static str,
}

impl AevoNet {
    pub const TESTNET: Self = Self {
        domain_name: "Aevo Testnet",
        chain_id: 11_155_111,
        rest: crate::aevo::TESTNET_REST,
        network: crate::aevo::TESTNET_NETWORK,
    };
    /// REAL funds: the end user signs and HMAC-authenticates every order.
    pub const MAINNET: Self = Self {
        domain_name: "Aevo Mainnet",
        chain_id: 1,
        rest: crate::aevo::MAINNET_REST,
        network: crate::aevo::NETWORK,
    };
    pub fn by_chain_id(id: u64) -> Option<Self> {
        [Self::TESTNET, Self::MAINNET]
            .into_iter()
            .find(|n| n.chain_id == id)
    }
    pub fn domain(&self) -> [u8; 32] {
        domain_separator(self.domain_name, DOMAIN_VERSION, self.chain_id)
    }
}
pub const SIGNATURE_PLACEHOLDER: &str = "__SIGNATURE__";
/// Aevo rejects stale timestamps; we refuse earlier than that.
const AUTH_MAX_SKEW_NS: i128 = 120_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AevoAccount {
    pub maker: [u8; 20],
    pub signer: [u8; 20],
    pub api_key: String,
}

impl AevoAccount {
    pub fn from_json(v: &Value) -> std::result::Result<Self, String> {
        let addr = |k: &str| {
            v[k].as_str()
                .and_then(parse_address)
                .ok_or_else(|| format!("{k}: expected a 0x-prefixed 20-byte address"))
        };
        let maker = addr("maker")?;
        let signer = if v["signer"].is_null() {
            maker
        } else {
            addr("signer")?
        };
        let api_key = v["api_key"]
            .as_str()
            .map(str::trim)
            .filter(|k| !k.is_empty() && k.len() <= 128 && k.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or("api_key: the user's Aevo API key (identifier; the secret never leaves the integrator)")?
            .to_string();
        Ok(Self {
            maker,
            signer,
            api_key,
        })
    }
    pub fn to_json(&self) -> Value {
        json!({"maker": crate::derive_user::hex20(&self.maker), "signer": crate::derive_user::hex20(&self.signer), "api_key": self.api_key})
    }
}

#[derive(Debug, Clone)]
pub struct PreparedAevo {
    pub digest: [u8; 32],
    pub typed_data: Value,
    /// Exact POST /orders body with `__SIGNATURE__` to substitute before computing the HMAC.
    pub body_template: String,
}

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

fn fields(o: &ExecOrder, acct: &AevoAccount) -> Result<OrderFields> {
    Ok(OrderFields {
        maker: acct.maker,
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

pub fn prepare_order(o: &ExecOrder, acct: &AevoAccount, net: AevoNet) -> Result<PreparedAevo> {
    let f = fields(o, acct)?;
    let digest = order_digest(&f, net.domain());
    let typed_data = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"}, {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"}
            ],
            "Order": [
                {"name": "maker", "type": "address"}, {"name": "isBuy", "type": "bool"},
                {"name": "limitPrice", "type": "uint256"}, {"name": "amount", "type": "uint256"},
                {"name": "salt", "type": "uint256"}, {"name": "instrument", "type": "uint256"},
                {"name": "timestamp", "type": "uint256"}
            ]
        },
        "primaryType": "Order",
        "domain": {"name": net.domain_name, "version": DOMAIN_VERSION, "chainId": net.chain_id},
        "message": {
            "maker": crate::derive_user::hex20(&f.maker), "isBuy": f.is_buy,
            "limitPrice": f.limit_price.to_string(), "amount": f.amount.to_string(),
            "salt": f.salt.to_string(), "instrument": f.instrument.to_string(),
            "timestamp": f.timestamp.to_string()
        }
    });
    // serde_json objects are key-sorted: the serialization is deterministic.
    let body_template = json!({
        "instrument": f.instrument as u64,
        "maker": crate::derive_user::hex20(&f.maker),
        "is_buy": f.is_buy,
        "amount": f.amount.to_string(),
        "limit_price": f.limit_price.to_string(),
        "salt": f.salt.to_string(),
        "signature": SIGNATURE_PLACEHOLDER,
        "timestamp": f.timestamp.to_string(),
        "post_only": false,
        "reduce_only": false,
        "time_in_force": match o.time_in_force { TimeInForce::Ioc => "IOC", TimeInForce::Gtc => "GTC" },
    })
    .to_string();
    Ok(PreparedAevo {
        digest,
        typed_data,
        body_template,
    })
}

/// Executes orders signed (and HMAC-authenticated) by the end user. Holds NO secret.
#[derive(Debug)]
pub struct AevoUserSignedExec {
    http: reqwest::Client,
    rest: String,
    net: AevoNet,
}

impl AevoUserSignedExec {
    pub fn testnet() -> Self {
        Self::on(AevoNet::TESTNET, AevoNet::TESTNET.rest)
    }
    /// MAINNET (real funds). Only attached by the `mainnet-nc` build.
    pub fn mainnet() -> Self {
        Self::on(AevoNet::MAINNET, AevoNet::MAINNET.rest)
    }
    /// Testnet signing domain against another host (tests).
    pub fn with_rest(rest: &str) -> Self {
        Self::on(AevoNet::TESTNET, rest)
    }
    pub fn on(net: AevoNet, rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            net,
        }
    }
}

#[async_trait]
impl ExecutionVenue for AevoUserSignedExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.net.network
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        let acct = AevoAccount::from_json(&o.venue_meta["user"]).ok()?;
        prepare_order(o, &acct, self.net)
            .ok()
            .map(|p| format!("0x{}", hex::encode(p.digest)))
    }

    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let acct = AevoAccount::from_json(u).map_err(bad)?;
        let sig = u["signature"]
            .as_str()
            .ok_or_else(|| bad("missing user signature"))?;
        let auth = &u["venue_auth"];
        let ts = auth["timestamp_ns"]
            .as_str()
            .map(str::to_string)
            .or_else(|| auth["timestamp_ns"].as_u64().map(|x| x.to_string()))
            .ok_or_else(|| bad("missing venue_auth.timestamp_ns"))?;
        let hmac = auth["hmac"]
            .as_str()
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| bad("venue_auth.hmac: 64 hex chars expected"))?;
        let ts_n: i128 = ts.parse().map_err(|_| bad("timestamp_ns"))?;
        let now = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as i128;
        if (now - ts_n).abs() > AUTH_MAX_SKEW_NS {
            return Err(bad("venue_auth.timestamp_ns is stale: authenticate again"));
        }
        let p = prepare_order(o, &acct, self.net)?;
        if recover_address(&p.digest, sig) != Some(acct.signer) {
            return Err(bad(
                "order signature does not match the declared signing key",
            ));
        }
        let body = p.body_template.replace(SIGNATURE_PLACEHOLDER, sig);
        let resp = self
            .http
            .post(format!("{}/orders", self.rest))
            .header("AEVO-KEY", &acct.api_key)
            .header("AEVO-TIMESTAMP", ts)
            .header("AEVO-SIGNATURE", hmac)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        parse_state(&read_json(P, resp).await?)
    }

    async fn get_order(&self, _id: &str) -> Result<Option<VenueOrderState>> {
        Ok(None)
    }

    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (user-signed orders are IOC; cancel from the user's account)"),
        })
    }

    fn identity(&self) -> Value {
        json!({"venue": self.net.network, "custody": "NON_CUSTODIAL", "signed_by": "end user (signing key + HMAC computed by the integrator)",
               "chain_id": self.net.chain_id, "domain_name": self.net.domain_name})
    }

    async fn balance(&self) -> Result<VenueBalance> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: "balance: user-held account".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aevo_exec::{auth_message, eth_address, hmac_signature, sign_digest};
    use k256::ecdsa::SigningKey;
    use oa_core::Decimal;

    fn order() -> ExecOrder {
        ExecOrder {
            native_id: "ETH-30OCT26-2800-C".into(),
            venue_instrument_id: Some(12345),
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(1, 1),
            limit_price_native: Decimal::new(10320, 2),
            salt: 77,
            timestamp: chrono::Utc::now().timestamp(),
            time_in_force: TimeInForce::Ioc,
        }
    }

    #[test]
    fn prepared_digest_is_the_custodial_digest() {
        let k = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let a = eth_address(&k);
        let acct = AevoAccount {
            maker: a,
            signer: a,
            api_key: "k".into(),
        };
        let o = order();
        let p = prepare_order(&o, &acct, AevoNet::TESTNET).unwrap();
        let f = fields(&o, &acct).unwrap();
        assert_eq!(p.digest, order_digest(&f, AevoNet::TESTNET.domain()));
        assert!(p.body_template.contains(SIGNATURE_PLACEHOLDER));
        assert_eq!(p.typed_data["primaryType"], "Order");
    }

    /// Mainnet domain {"Aevo Mainnet", "1", 1}: value computed independently with eth_abi/eth_utils.
    #[test]
    fn mainnet_domain_matches_independent_computation() {
        assert_eq!(
            format!("0x{}", hex::encode(AevoNet::MAINNET.domain())),
            "0xdf5276d0f8693a141ea4206e935faccce5030239f2d742038298b3d78658806d"
        );
        let k = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let a = eth_address(&k);
        let acct = AevoAccount {
            maker: a,
            signer: a,
            api_key: "k".into(),
        };
        let o = order();
        let m = prepare_order(&o, &acct, AevoNet::MAINNET).unwrap();
        assert_ne!(
            m.digest,
            prepare_order(&o, &acct, AevoNet::TESTNET).unwrap().digest
        );
        assert_eq!(m.typed_data["domain"]["name"], "Aevo Mainnet");
        let v = AevoUserSignedExec::mainnet();
        assert_eq!(v.network(), crate::aevo::NETWORK);
        assert_eq!(v.identity()["chain_id"], 1);
    }

    #[tokio::test]
    async fn submit_forwards_user_signature_and_hmac_exactly() {
        use std::sync::{Arc, Mutex};
        let seen: Arc<Mutex<Option<(axum::http::HeaderMap, String)>>> = Arc::default();
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/orders",
            axum::routing::post(move |h: axum::http::HeaderMap, b: String| {
                let s2 = s2.clone();
                async move {
                    *s2.lock().unwrap() = Some((h, b));
                    axum::Json(json!({"order_id": "0xabc", "order_status": "filled", "filled": "0.1", "avg_price": "103.2"}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let account_key = SigningKey::from_slice(&[3u8; 32]).unwrap();
        let signing_key = SigningKey::from_slice(&[4u8; 32]).unwrap();
        let acct = AevoAccount {
            maker: eth_address(&account_key),
            signer: eth_address(&signing_key),
            api_key: "user-key".into(),
        };
        let mut o = order();
        let p = prepare_order(&o, &acct, AevoNet::TESTNET).unwrap();
        // Integrator side: sign, substitute, HMAC with the user's secret (never sent to us).
        let sig = format!(
            "0x{}",
            hex::encode(sign_digest(&signing_key, &p.digest).unwrap())
        );
        let body = p.body_template.replace(SIGNATURE_PLACEHOLDER, &sig);
        let ts = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap()
            .to_string();
        let hmac = hmac_signature(
            "user-secret",
            &auth_message("user-key", &ts, "POST", "/orders", &body),
        );
        let mut u = acct.to_json();
        u["signature"] = json!(sig);
        u["venue_auth"] = json!({"timestamp_ns": ts, "hmac": hmac});
        o.venue_meta["user"] = u;

        let ex = AevoUserSignedExec::with_rest(&format!("http://{addr}"));
        ex.submit(&o).await.unwrap();
        let (h, b) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(
            b, body,
            "byte-identical body, so the integrator's HMAC verifies"
        );
        assert_eq!(h["aevo-signature"], hmac.as_str());
        assert_eq!(h["aevo-key"], "user-key");

        // Signed by the account key instead of the registered signing key: refused locally.
        *seen.lock().unwrap() = None;
        let mut wrong = o.clone();
        wrong.venue_meta["user"]["signature"] = json!(format!(
            "0x{}",
            hex::encode(sign_digest(&account_key, &p.digest).unwrap())
        ));
        assert!(
            ex.submit(&wrong)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert!(seen.lock().unwrap().is_none());
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live_testnet {
    use super::*;
    use crate::Connector;
    use crate::aevo_exec::{auth_message, hmac_signature, sign_digest};
    use k256::ecdsa::SigningKey;
    use oa_core::Decimal;

    fn env_testnet() -> std::collections::HashMap<String, String> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env.testnet");
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect()
    }

    /// ZERO MONEY (Aevo TESTNET, account with 0 USDC): the NON-CUSTODIAL path end to end — order
    /// signed with the registered signing key, HMAC computed "integrator side", body forwarded by
    /// `AevoUserSignedExec`. Aevo must get past authentication and signature and refuse on funds.
    /// Keys come from .env.testnet (never printed).
    #[tokio::test]
    async fn aevo_testnet_accepts_our_signature_and_hmac() {
        let e = env_testnet();
        let (Some(wallet), Some(key), Some(secret), Some(signing)) = (
            e.get("OA_AEVO_TESTNET_WALLET"),
            e.get("OA_AEVO_TESTNET_API_KEY"),
            e.get("OA_AEVO_TESTNET_API_SECRET"),
            e.get("OA_AEVO_TESTNET_SIGNING_KEY"),
        ) else {
            eprintln!("SKIP: .env.testnet has no Aevo testnet account");
            return;
        };
        let sk = SigningKey::from_slice(&hex::decode(signing.trim_start_matches("0x")).unwrap())
            .unwrap();
        let cat = crate::aevo::AevoConnector::testnet()
            .list_instruments("ETH")
            .await
            .unwrap();
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let acct_json = json!({"maker": wallet, "signer": format!("0x{}", hex::encode(crate::aevo_exec::eth_address(&sk))), "api_key": key});
        let mut o = ExecOrder {
            native_id: i.key.native_id.clone(),
            venue_instrument_id: i.native["instrument_id"]
                .as_str()
                .and_then(|s| s.parse().ok()),
            venue_meta: json!({"user": acct_json}),
            max_fee_per_unit: None,
            side: Side::Buy,
            qty_native: Decimal::new(1, 1),
            limit_price_native: i.tick_size.unwrap_or(Decimal::ONE),
            salt: chrono::Utc::now().timestamp_subsec_nanos() as u64,
            timestamp: chrono::Utc::now().timestamp(),
            time_in_force: TimeInForce::Ioc,
        };
        let acct = AevoAccount::from_json(&o.venue_meta["user"]).unwrap();
        let p = prepare_order(&o, &acct, AevoNet::TESTNET).unwrap();
        let sig = format!("0x{}", hex::encode(sign_digest(&sk, &p.digest).unwrap()));
        let ts = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap()
            .to_string();
        let body = p.body_template.replace(SIGNATURE_PLACEHOLDER, &sig);
        let mac = hmac_signature(secret, &auth_message(key, &ts, "POST", "/orders", &body));
        o.venue_meta["user"]["signature"] = json!(sig);
        o.venue_meta["user"]["venue_auth"] = json!({"timestamp_ns": ts, "hmac": mac});
        let r = AevoUserSignedExec::testnet().submit(&o).await;
        let txt = format!("{r:?}");
        eprintln!(
            "{} -> {}",
            i.key.native_id,
            txt.chars().take(400).collect::<String>()
        );
        let low = txt.to_lowercase();
        assert!(
            !low.contains("signature") && !low.contains("unauthor") && !low.contains("auth"),
            "Aevo rejected our signature/HMAC: {txt}"
        );
        // Negative control: same order signed by ANOTHER key must be refused differently (proves the
        // answer above came after Aevo's signature check, not before it). Our executor refuses it
        // locally first, so this request goes straight to Aevo.
        let other = SigningKey::from_slice(&[0x11; 32]).unwrap();
        let bad_sig = format!("0x{}", hex::encode(sign_digest(&other, &p.digest).unwrap()));
        let bad_body = p.body_template.replace(SIGNATURE_PLACEHOLDER, &bad_sig);
        let ts2 = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap()
            .to_string();
        let mac2 = hmac_signature(
            secret,
            &auth_message(key, &ts2, "POST", "/orders", &bad_body),
        );
        let r2: Value = crate::http::client()
            .post(format!("{}/orders", AevoNet::TESTNET.rest))
            .header("AEVO-KEY", key.as_str())
            .header("AEVO-TIMESTAMP", ts2)
            .header("AEVO-SIGNATURE", mac2)
            .header("content-type", "application/json")
            .body(bad_body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("negative control (wrong signer) -> {r2}");
        assert_ne!(r2.to_string(), "{\"error\":\"IOC_ORDER_REJECTED\"}");
    }
}
