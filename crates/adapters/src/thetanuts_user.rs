//! NON-CUSTODIAL Thetanuts V4 execution (Base): we build the `OptionBook.fillOrder` transaction, the
//! END USER's wallet sends it, and we verify the on-chain receipt. Omni Strike never signs or sends.
//!
//! Source: official SDK `@thetanuts-finance/thetanuts-client` 0.3.0 (npm, read 2026-10-08):
//! `fillOrder(Order order, bytes signature, address referrer) returns (address optionAddress)` with
//! `Order = (address maker, uint256 orderExpiryTimestamp, address collateral, bool isCall,
//! address priceFeed, address implementation, bool isLong, uint256 maxCollateralUsable,
//! uint256[] strikes, uint256 expiry, uint256 price, uint256 numContracts, bytes extraOptionData)`.
//! Every field comes from the maker's signed order EXCEPT `numContracts` (6 decimals), which the
//! taker sets (SDK `buildContractOrder`). The buyer is `msg.sender`; USDC allowance to the
//! OptionBook is the user's responsibility (we return the approval call, never send it).
//! Event `OrderFilled(uint256 indexed nonce, address indexed buyer, address indexed seller,
//! address optionAddress, uint256 premiumAmount, uint256 feeCollected, address referrer,
//! uint256 referralFeePaid, bool sellerWasMaker)` gives the realised premium.
//! Reconciliation needs no user credential: the receipt is public (`get_order(tx_hash)`).

use crate::aevo_exec::{
    address_word as word_addr, erc20_approve, keccak, parse_address, u256 as word_u,
};
use crate::exec::{ExecOrder, ExecutionVenue, VenueBalance, VenueOrderState, VenueStatus};
use crate::http::{client, read_json};
use crate::thetanuts::{OPTION_BOOK, ORDERS_URL, USDC};
use crate::{ConnectorError, Result};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::PlatformId;
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Thetanuts;
pub const NETWORK: &str = crate::thetanuts::NETWORK;
pub const CHAIN_ID: u64 = 8453;
pub const BASE_RPC: &str = "https://base-rpc.publicnode.com";

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}

fn pad32(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    v.resize(b.len().div_ceil(32) * 32, 0);
    v
}
fn u_of(v: &Value, f: &str) -> Result<u128> {
    match v {
        Value::String(s) => s.parse().map_err(|_| bad(format!("{f}: {s}"))),
        Value::Number(n) => n.as_u64().map(u128::from).ok_or_else(|| bad(f.to_string())),
        _ => Err(bad(format!("{f} missing"))),
    }
}
fn addr_of(v: &Value, f: &str) -> Result<[u8; 20]> {
    v.as_str()
        .and_then(parse_address)
        .ok_or_else(|| bad(format!("{f}: address")))
}

/// ABI-encode `fillOrder(order, signature, referrer)` for a signed maker order (`o` = the feed's
/// `order` object) and the taker's `num_contracts` (6 decimals).
pub fn encode_fill_order(
    o: &Value,
    signature: &str,
    num_contracts: u128,
    referrer: &[u8; 20],
) -> Result<Vec<u8>> {
    let strikes: Vec<u128> = o["strikes"]
        .as_array()
        .ok_or_else(|| bad("strikes"))?
        .iter()
        .map(|s| u_of(s, "strike"))
        .collect::<Result<_>>()?;
    let extra = hex::decode(
        o["extraOptionData"]
            .as_str()
            .unwrap_or("0x")
            .trim_start_matches("0x"),
    )
    .map_err(|_| bad("extraOptionData"))?;
    let sig = hex::decode(signature.trim_start_matches("0x")).map_err(|_| bad("signature"))?;
    // Tuple head: 13 slots; dynamic members (strikes, extraOptionData) are offsets within the tuple.
    let mut head: Vec<[u8; 32]> = vec![
        word_addr(&addr_of(&o["maker"], "maker")?),
        word_u(u_of(&o["orderExpiryTimestamp"], "orderExpiryTimestamp")?),
        word_addr(&addr_of(&o["collateral"], "collateral")?),
        word_u((o["isCall"] == true) as u128),
        word_addr(&addr_of(&o["priceFeed"], "priceFeed")?),
        word_addr(&addr_of(&o["implementation"], "implementation")?),
        word_u((o["isLong"] == true) as u128),
        word_u(u_of(&o["maxCollateralUsable"], "maxCollateralUsable")?),
        [0u8; 32], // strikes offset
        word_u(u_of(&o["expiry"], "expiry")?),
        word_u(u_of(&o["price"], "price")?),
        word_u(num_contracts),
        [0u8; 32], // extraOptionData offset
    ];
    let mut tail = Vec::new();
    let strikes_off = 13 * 32;
    head[8] = word_u(strikes_off as u128);
    tail.extend_from_slice(&word_u(strikes.len() as u128));
    for s in &strikes {
        tail.extend_from_slice(&word_u(*s));
    }
    head[12] = word_u((strikes_off + tail.len()) as u128);
    tail.extend_from_slice(&word_u(extra.len() as u128));
    tail.extend_from_slice(&pad32(&extra));
    let mut tuple = Vec::new();
    for w in &head {
        tuple.extend_from_slice(w);
    }
    tuple.extend_from_slice(&tail);
    // Outer: (tuple offset, bytes offset, referrer) + tuple + signature bytes.
    let mut out = keccak(b"fillOrder((address,uint256,address,bool,address,address,bool,uint256,uint256[],uint256,uint256,uint256,bytes),bytes,address)")[..4].to_vec();
    let tuple_off = 3 * 32;
    let sig_off = tuple_off + tuple.len();
    out.extend_from_slice(&word_u(tuple_off as u128));
    out.extend_from_slice(&word_u(sig_off as u128));
    out.extend_from_slice(&word_addr(referrer));
    out.extend_from_slice(&tuple);
    out.extend_from_slice(&word_u(sig.len() as u128));
    out.extend_from_slice(&pad32(&sig));
    Ok(out)
}

/// `approve(OptionBook, amount)` on USDC — returned for the user's wallet, never sent by us.
pub fn encode_usdc_approve(amount_6: u128) -> Vec<u8> {
    erc20_approve(OPTION_BOOK, amount_6)
}

#[derive(Debug, Clone)]
pub struct PreparedTx {
    pub to: String,
    pub data: String,
    pub chain_id: u64,
    pub num_contracts: u128,
    pub price: Decimal,
    pub approval: Value,
}

fn dec_scaled(raw: u128, decimals: u32) -> Decimal {
    Decimal::from_i128_with_scale(raw as i128, decimals).normalize()
}

/// Pick the cheapest live maker order for `o.native_id` at a price <= the leg's limit with enough
/// size, and build the user's transaction.
pub fn build_fill(
    feed: &Value,
    o: &ExecOrder,
    referrer: &[u8; 20],
    now_s: i64,
) -> Result<PreparedTx> {
    let orders = feed["data"]["orders"]
        .as_array()
        .ok_or_else(|| bad("order feed"))?;
    let qty6 = {
        use rust_decimal::prelude::ToPrimitive;
        let q = o.qty_native * Decimal::from(1_000_000);
        if q.fract() != Decimal::ZERO {
            return Err(bad("quantity has more than 6 decimals"));
        }
        q.to_u128().ok_or_else(|| bad("quantity"))?
    };
    let mut best: Option<(&Value, u128)> = None;
    for w in orders {
        let od = &w["order"];
        if od["ticker"].as_str() != Some(o.native_id.as_str())
            || od["isLong"] != false
            || w["chainId"] != CHAIN_ID
        {
            continue;
        }
        if u_of(&od["orderExpiryTimestamp"], "orderExpiryTimestamp")
            .map(|t| t as i64 <= now_s + 30)
            .unwrap_or(true)
        {
            continue;
        }
        let (Ok(price), Ok(coll), Some(k)) = (
            u_of(&od["price"], "price"),
            u_of(&od["maxCollateralUsable"], "maxCollateralUsable"),
            od["strikes"].get(0).and_then(|s| u_of(s, "strike").ok()),
        ) else {
            continue;
        };
        // Max contracts (6 dp) = collateral (6 dp) x 1e8 / strike (8 dp) [SDK calculateMaxContracts].
        if k == 0 || coll * 100_000_000 / k < qty6 || dec_scaled(price, 8) > o.limit_price_native {
            continue;
        }
        if best
            .is_none_or(|(b, _)| u_of(&b["order"]["price"], "price").unwrap_or(u128::MAX) > price)
        {
            best = Some((w, price));
        }
    }
    let (w, price) =
        best.ok_or_else(|| bad("no maker order at or below the limit with enough size"))?;
    let sig = w["signature"]
        .as_str()
        .ok_or_else(|| bad("order signature"))?;
    let data = encode_fill_order(&w["order"], sig, qty6, referrer)?;
    // Premium = numContracts x price; fee on top (OptionFactory.calculateFee) -> approve 2 x premium
    // as a generous ceiling the user may lower.
    let premium6 = qty6 * price / 100_000_000;
    Ok(PreparedTx {
        to: OPTION_BOOK.to_string(),
        data: format!("0x{}", hex::encode(data)),
        chain_id: CHAIN_ID,
        num_contracts: qty6,
        price: dec_scaled(price, 8),
        approval: json!({"to": USDC, "data": format!("0x{}", hex::encode(encode_usdc_approve(premium6 * 2))), "note": "USDC allowance for the OptionBook (send only if the current allowance is lower)"}),
    })
}

/// Stable order id `<tx_hash>:<numContracts 6dp>:<keccak(calldata)>`: reconciliation re-verifies the
/// transaction from the id alone (public receipt, no credential).
pub fn order_id(tx_hash: &str, num_contracts: u128, calldata: &str) -> String {
    format!(
        "{}:{num_contracts}:{}",
        tx_hash.to_lowercase(),
        calldata_hash(calldata).unwrap_or_default()
    )
}

fn calldata_hash(input: &str) -> Option<String> {
    hex::decode(input.trim_start_matches("0x"))
        .ok()
        .map(|b| hex::encode(keccak(&b)))
}

/// Parse a receipt into a venue state; refuses a transaction that is not the prepared fillOrder.
pub fn parse_receipt(
    receipt: &Value,
    tx: &Value,
    expected_data_hash: &str,
    id: &str,
) -> Result<VenueOrderState> {
    let to_ok = tx["to"]
        .as_str()
        .is_some_and(|t| t.eq_ignore_ascii_case(OPTION_BOOK));
    let data_ok = tx["input"]
        .as_str()
        .and_then(calldata_hash)
        .is_some_and(|h| h.eq_ignore_ascii_case(expected_data_hash));
    if !to_ok || !data_ok {
        return Err(bad(
            "transaction does not match the prepared fillOrder (to/data differ)",
        ));
    }
    let ok = receipt["status"].as_str() == Some("0x1");
    let topic = format!(
        "0x{}",
        hex::encode(keccak(
            b"OrderFilled(uint256,address,address,address,uint256,uint256,address,uint256,bool)"
        ))
    );
    let filled = receipt["logs"].as_array().into_iter().flatten().find(|l| {
        l["topics"][0]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case(&topic))
    });
    let (status, premium) = match (ok, filled) {
        (true, Some(l)) => {
            // data = optionAddress, premiumAmount, feeCollected, referrer, referralFeePaid, sellerWasMaker
            let d = l["data"]
                .as_str()
                .unwrap_or("0x")
                .trim_start_matches("0x")
                .to_string();
            let prem = d
                .get(64..128)
                .and_then(|w| u128::from_str_radix(&w[32..], 16).ok())
                .map(|x| dec_scaled(x, 6));
            (VenueStatus::Filled, prem)
        }
        (true, None) => (
            VenueStatus::Unknown("mined without OrderFilled event".into()),
            None,
        ),
        (false, _) => (VenueStatus::Rejected, None),
    };
    Ok(VenueOrderState {
        venue_order_id: id.to_string(),
        is_final: !matches!(status, VenueStatus::Unknown(_)),
        filled_native: Decimal::ZERO,
        avg_price_native: None,
        status,
        raw: json!({"receipt_status": receipt["status"], "premium_paid": premium.map(|p| p.to_string()), "block": receipt["blockNumber"]}),
    })
}

/// Executes user-sent transactions: builds them, verifies receipts, never sends anything.
#[derive(Debug)]
pub struct ThetanutsUserTxExec {
    http: reqwest::Client,
    rpc: String,
    feed: String,
    /// Omni Strike referrer address for the OptionBook referral split (zero = none).
    pub referrer: [u8; 20],
}

impl ThetanutsUserTxExec {
    pub fn mainnet(referrer: Option<[u8; 20]>) -> Self {
        Self::with_urls(BASE_RPC, ORDERS_URL, referrer)
    }
    pub fn with_urls(rpc: &str, feed: &str, referrer: Option<[u8; 20]>) -> Self {
        Self {
            http: client(),
            rpc: rpc.into(),
            feed: feed.into(),
            referrer: referrer.unwrap_or([0u8; 20]),
        }
    }
    pub async fn build(&self, o: &ExecOrder) -> Result<PreparedTx> {
        let feed = read_json(P, self.http.get(&self.feed).send().await).await?;
        build_fill(&feed, o, &self.referrer, chrono::Utc::now().timestamp())
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let v = read_json(
            P,
            self.http
                .post(&self.rpc)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send()
                .await,
        )
        .await?;
        Ok(v["result"].clone())
    }
    async fn check(&self, id: &str) -> Result<Option<VenueOrderState>> {
        let mut parts = id.split(':');
        let (Some(tx_hash), Some(n), Some(expected)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(bad(format!("order id {id}")));
        };
        let qty = dec_scaled(n.parse().map_err(|_| bad("numContracts in id"))?, 6);
        let tx = self
            .rpc("eth_getTransactionByHash", json!([tx_hash]))
            .await?;
        let receipt = self
            .rpc("eth_getTransactionReceipt", json!([tx_hash]))
            .await?;
        if tx.is_null() || receipt.is_null() {
            return Ok(None); // not mined yet (or unknown hash)
        }
        let mut s = parse_receipt(&receipt, &tx, expected, id)?;
        if s.status == VenueStatus::Filled && qty > Decimal::ZERO {
            s.filled_native = qty;
            s.avg_price_native = s.raw["premium_paid"]
                .as_str()
                .and_then(|p| p.parse::<Decimal>().ok())
                .map(|p| (p / qty).round_dp(8));
        }
        Ok(Some(s))
    }
}

#[async_trait]
impl ExecutionVenue for ThetanutsUserTxExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        let u = &o.venue_meta["user"];
        let n = u["num_contracts"].as_str()?.parse().ok()?;
        Some(order_id(
            u["tx_hash"].as_str()?,
            n,
            u["prepared_data"].as_str()?,
        ))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let u = &o.venue_meta["user"];
        let tx_hash = u["tx_hash"]
            .as_str()
            .filter(|h| {
                h.len() == 66
                    && h.starts_with("0x")
                    && h[2..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or_else(|| {
                bad("tx_hash (0x + 64 hex) of the fillOrder transaction sent by the user")
            })?;
        let expected = u["prepared_data"]
            .as_str()
            .ok_or_else(|| bad("prepared transaction missing"))?;
        let n: u128 = u["num_contracts"]
            .as_str()
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| bad("prepared numContracts missing"))?;
        // The transaction must come from the account declared at prepare time: a public
        // transaction of someone else cannot be attributed to this order.
        if let Some(addr) = o.venue_meta["user"]["address"].as_str() {
            let tx = self
                .rpc("eth_getTransactionByHash", json!([tx_hash]))
                .await?;
            if let Some(from) = tx["from"].as_str()
                && !from.eq_ignore_ascii_case(addr)
            {
                return Err(bad("transaction was not sent by the declared address"));
            }
        }
        let id = order_id(tx_hash, n, expected);
        match self.check(&id).await? {
            Some(s) => Ok(s),
            None => Ok(VenueOrderState {
                venue_order_id: id,
                status: VenueStatus::Unknown("transaction not mined yet".into()),
                filled_native: Decimal::ZERO,
                avg_price_native: None,
                is_final: false,
                raw: json!({"tx_hash": tx_hash}),
            }),
        }
    }
    /// Public receipt lookup by order id (tx hash + size + calldata hash): no credential needed.
    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        self.check(id).await
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (on-chain fills cannot be cancelled)"),
        })
    }
    async fn prepare_user_tx(&self, o: &ExecOrder) -> Result<Option<Value>> {
        let t = self.build(o).await?;
        Ok(Some(
            json!({"chain_id": t.chain_id, "to": t.to, "data": t.data, "value": "0x0",
                       "num_contracts": t.num_contracts.to_string(), "maker_price": t.price.to_string(),
                       "approval": t.approval}),
        ))
    }
    fn identity(&self) -> Value {
        json!({"venue": NETWORK, "custody": "NON_CUSTODIAL", "signed_by": "end user wallet sends the transaction", "option_book": OPTION_BOOK, "referrer": format!("0x{}", hex::encode(self.referrer))})
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

    fn feed() -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/recorded-2026-10-06/thetanuts_orders.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn exec_order(native: &str, qty: Decimal, limit: Decimal) -> ExecOrder {
        ExecOrder {
            native_id: native.into(),
            venue_instrument_id: None,
            venue_meta: json!({}),
            max_fee_per_unit: None,
            side: oa_core::domain::Side::Buy,
            qty_native: qty,
            limit_price_native: limit,
            salt: 1,
            timestamp: 0,
            time_in_force: crate::exec::TimeInForce::Ioc,
        }
    }

    /// Writes the encoded call so scripts/check_thetanuts_abi.py can decode it with eth_abi.
    #[test]
    fn builds_a_fill_for_the_cheapest_order_within_the_limit() {
        let f = feed();
        let now = 1_791_222_290; // 2026-10-06T08:04:50Z (recording)
        let o = exec_order(
            "BTC-7OCT26-84000-P",
            Decimal::new(1, 1),
            Decimal::new(120, 0),
        );
        let tx = build_fill(&f, &o, &[0u8; 20], now).unwrap();
        assert_eq!(tx.to, OPTION_BOOK);
        assert_eq!(tx.num_contracts, 100_000);
        assert!(tx.price <= Decimal::new(120, 0));
        assert!(tx.data.starts_with("0x"));
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/thetanuts_fill_vector.json");
        let w = f["data"]["orders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["order"]["ticker"] == "BTC-7OCT26-84000-P" && price_of(w) == tx.price)
            .unwrap()
            .clone();
        std::fs::write(out, json!({"data": tx.data, "order": w["order"], "signature": w["signature"], "num_contracts": tx.num_contracts.to_string()}).to_string()).unwrap();
        // A limit below every order: refused, nothing built.
        assert!(
            build_fill(
                &f,
                &exec_order("BTC-7OCT26-84000-P", Decimal::new(1, 1), Decimal::new(1, 0)),
                &[0u8; 20],
                now
            )
            .is_err()
        );
    }

    pub fn price_of(w: &Value) -> Decimal {
        dec_scaled(u_of(&w["order"]["price"], "price").unwrap(), 8)
    }

    #[test]
    fn receipt_must_match_the_prepared_call() {
        let data = "0xabcdef";
        let tx = json!({"to": OPTION_BOOK, "input": data});
        let topic = format!("0x{}", hex::encode(keccak(b"OrderFilled(uint256,address,address,address,uint256,uint256,address,uint256,bool)")));
        let premium = format!("{:064x}{:064x}", 0u8, 11_330_977u64);
        let receipt =
            json!({"status": "0x1", "logs": [{"topics": [topic], "data": format!("0x{premium}")}]});
        let h = calldata_hash(data).unwrap();
        let s = parse_receipt(&receipt, &tx, &h, "0xhash").unwrap();
        assert_eq!(s.status, VenueStatus::Filled);
        assert_eq!(s.raw["premium_paid"], "11.330977");
        // Same receipt, different calldata: refused.
        assert!(
            parse_receipt(
                &receipt,
                &json!({"to": OPTION_BOOK, "input": "0x00"}),
                data,
                "0xhash"
            )
            .is_err()
        );
        // Reverted transaction = rejected.
        let r = parse_receipt(&json!({"status": "0x0", "logs": []}), &tx, &h, "0xhash").unwrap();
        assert_eq!(r.status, VenueStatus::Rejected);
    }
}
