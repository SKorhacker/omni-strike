//! NON-CUSTODIAL CallPut execution (Base): we build `PositionManager.createOpenPosition`, the END
//! USER's wallet sends it (with the ETH execution fee as value), a keeper executes the request, and
//! we read its status on-chain. Omni Strike never signs or sends.
//!
//! Sources (2026-10-08): docs.callput.app developers/{integration-guide, request-lifecycle-reference,
//! option-token-id-and-state-machine}; signature from a live transaction + openchain signature DB:
//! `createOpenPosition(uint16 underlyingAssetIndex, uint8 length, bool[4] isBuys, bytes32[4]
//! optionIds, bool[4] isCalls, uint256 minSize, address[] path, uint256 amountIn,
//! uint256 minOutWhenSwap, address leadTrader)` payable (value = `executionFee()`), selector
//! 0x4fd5f4af. Live decode: optionId's top 16 bits = underlyingAssetIndex, amountIn = USDC budget
//! (6 dp), minSize = 1e18-scaled size floor, path = [USDC].
//! Event `GenerateRequestKey(address indexed account, bytes32 indexed key, bool indexed isOpen)`.
//! `openPositionRequests(bytes32)` returns 14 words [LIVE decode of an executed request]:
//! 0 account, 1 underlyingAssetIndex, 2 expiry, 3 optionTokenId, 4 minSize (1e18), 5 amountIn (1e6),
//! 6 minOutWhenSwap, 7-8 internal, 9 status (0 Pending, 1 Cancelled, 2 Executed), 10 sizeOut (1e18),
//! 11 executionPrice (1e30), 12 processBlockTime, 13 amountOut.
//! Sizing: amountIn = qty x (limit + max fee per unit); minSize = qty -> the keeper cancels the
//! request (nothing bought, budget refunded) if it cannot deliver the size within that budget.

use crate::aevo_exec::{address_word as w_addr, erc20_approve, keccak, parse_address, u256 as w_u};
use crate::exec::{ExecOrder, ExecutionVenue, VenueBalance, VenueOrderState, VenueStatus};
use crate::http::{client, read_json};
use crate::{ConnectorError, Result, invalid};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side};
use serde_json::{Value, json};

const P: PlatformId = PlatformId::Callput;
pub const NETWORK: &str = crate::callput::NETWORK;
pub const POSITION_MANAGER: &str = "0x83B04701B227B045CBBAF921377137fF595a54af";
/// USDC SPENDER: PositionManager pulls the budget through the Callput Router, so the allowance must be
/// given to the Router (official agent repo ayggdrasil/callput-option-agent `src/config.ts` ROUTER,
/// read 2026-10-09). Approving the PositionManager made the transaction revert ("ERC20: transfer
/// amount exceeds allowance", eth_call simulation against Base mainnet, 2026-10-09).
pub const ROUTER: &str = "0xfc61ba50AE7B9C4260C9f04631Ff28D5A2Fa4EB2";
pub const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
pub const CHAIN_ID: u64 = 8453;
pub const BASE_RPC: &str = "https://base-rpc.publicnode.com";

fn bad(d: impl std::fmt::Display) -> ConnectorError {
    crate::not_representable(P, d)
}
fn scaled_floor(x: Decimal, decimals: u32) -> Option<u128> {
    use rust_decimal::prelude::ToPrimitive;
    (x * Decimal::from(10u128.pow(decimals))).floor().to_u128()
}
fn scaled_ceil(x: Decimal, decimals: u32) -> Option<u128> {
    use rust_decimal::prelude::ToPrimitive;
    (x * Decimal::from(10u128.pow(decimals))).ceil().to_u128()
}

/// ABI-encode a single-leg BUY open request.
pub fn encode_open(
    option_id: &[u8; 32],
    is_call: bool,
    min_size_e18: u128,
    amount_in_6: u128,
) -> Vec<u8> {
    encode_request(&[(true, *option_id, is_call)], min_size_e18, amount_in_6)
}

/// ABI-encode an open request of 1..=4 legs `(is_buy, option_id, is_call)` (same underlying).
pub fn encode_request(
    legs: &[(bool, [u8; 32], bool)],
    min_size_e18: u128,
    amount_in_6: u128,
) -> Vec<u8> {
    assert!((1..=4).contains(&legs.len()));
    let underlying = u16::from_be_bytes([legs[0].1[0], legs[0].1[1]]);
    let mut out = keccak(b"createOpenPosition(uint16,uint8,bool[4],bytes32[4],bool[4],uint256,address[],uint256,uint256,address)")[..4].to_vec();
    let mut head: Vec<[u8; 32]> = vec![w_u(underlying as u128), w_u(legs.len() as u128)];
    for k in 0..4 {
        head.push(w_u(legs.get(k).is_some_and(|l| l.0) as u128)); // isBuys
    }
    for k in 0..4 {
        head.push(legs.get(k).map(|l| l.1).unwrap_or([0u8; 32])); // optionIds
    }
    for k in 0..4 {
        head.push(w_u(legs.get(k).is_some_and(|l| l.2) as u128)); // isCalls
    }
    head.push(w_u(min_size_e18));
    let path_offset = (head.len() + 4) * 32; // + path offset, amountIn, minOut, leadTrader
    head.push(w_u(path_offset as u128));
    head.push(w_u(amount_in_6));
    head.push(w_u(0)); // minOutWhenSwap (no swap: USDC in)
    head.push([0u8; 32]); // leadTrader
    for w in &head {
        out.extend_from_slice(w);
    }
    out.extend_from_slice(&w_u(1));
    out.extend_from_slice(&w_addr(&parse_address(USDC).expect("constant")));
    out
}

pub fn encode_usdc_approve(amount_6: u128) -> Vec<u8> {
    erc20_approve(ROUTER, amount_6)
}

/// Build the user's transaction for one BUY leg.
/// SINGLE-LEG request. NOT accepted by the live contract any more: PositionManager reverts with
/// `StrategyNotAllowed()` (0xce23f153) for length 1, while a 2-leg call spread passes (eth_call on Base
/// mainnet, 2026-10-09). Kept for the encoding tests; the venue refuses to prepare it.
pub fn build_open(o: &ExecOrder, execution_fee_wei: u128) -> Result<Value> {
    if o.side != Side::Buy {
        return Err(bad(
            "CallPut: only buys are integrated (selling needs collateral handling)",
        ));
    }
    let option_id = option_id(o)?;
    let is_call = o.native_id.ends_with("-C");
    if !is_call && !o.native_id.ends_with("-P") {
        return Err(bad("instrument name must end with -C or -P"));
    }
    let fee = o.max_fee_per_unit.unwrap_or_default();
    let budget = o.qty_native * (o.limit_price_native + fee);
    let amount_in = scaled_ceil(budget, 6).ok_or_else(|| bad("budget"))?;
    let min_size = scaled_floor(o.qty_native, 18).ok_or_else(|| bad("size"))?;
    let data = encode_open(&option_id, is_call, min_size, amount_in);
    Ok(json!({
        "chain_id": CHAIN_ID, "to": POSITION_MANAGER,
        "data": format!("0x{}", hex::encode(data)),
        "value": format!("0x{execution_fee_wei:x}"),
        "num_contracts": min_size.to_string(), "budget_usdc": budget.round_dp(6).to_string(),
        "approval": {"to": USDC, "data": format!("0x{}", hex::encode(encode_usdc_approve(amount_in))), "spender": ROUTER, "note": "USDC allowance for the Callput Router (send only if the current allowance is lower)"},
        "note": "Asynchronous: a keeper executes or cancels the request (max 180 s); the budget is refunded if cancelled."
    }))
}

/// `ETH-16OCT26-2600-C` -> (underlying, expiry code, strike, is_call).
fn parse_name(n: &str) -> Option<(&str, &str, Decimal, bool)> {
    let p: Vec<&str> = n.split('-').collect();
    let [u, e, k, t] = p.as_slice() else {
        return None;
    };
    let is_call = match *t {
        "C" => true,
        "P" => false,
        _ => return None,
    };
    Some((u, e, k.parse().ok()?, is_call))
}

/// A CallPut DEBIT vertical spread: same underlying/expiry/type, equal size, BUY the more expensive
/// strike (lower call / higher put) and SELL the other. The only shape we route (bounded loss = debit).
pub fn check_spread(buy: &ExecOrder, sell: &ExecOrder) -> Result<()> {
    let (b, s) = (
        parse_name(&buy.native_id).ok_or_else(|| bad("buy leg name"))?,
        parse_name(&sell.native_id).ok_or_else(|| bad("sell leg name"))?,
    );
    if buy.side != Side::Buy || sell.side != Side::Sell {
        return Err(bad("spread needs one BUY and one SELL leg"));
    }
    if b.0 != s.0 || b.1 != s.1 || b.3 != s.3 {
        return Err(bad("spread legs must share underlying, expiry and type"));
    }
    if buy.qty_native != sell.qty_native {
        return Err(bad("spread legs must have the same size"));
    }
    let debit = if b.3 { b.2 < s.2 } else { b.2 > s.2 };
    if !debit {
        return Err(bad(
            "only DEBIT spreads are routed (buy the lower-strike call / higher-strike put)",
        ));
    }
    Ok(())
}

fn option_id(o: &ExecOrder) -> Result<[u8; 32]> {
    let id_hex = o.venue_meta["option_id"]
        .as_str()
        .ok_or_else(|| bad("option_id missing"))?;
    let raw = hex::decode(id_hex.trim_start_matches("0x")).map_err(|_| bad("option_id"))?;
    raw.try_into().map_err(|_| bad("option_id length"))
}

/// ONE transaction for a 2-leg debit spread (the only strategy shape the live contract accepts for
/// openings, verified by eth_call 2026-10-09). Budget = size x (buy limit - sell limit + both fee caps):
/// the keeper cancels (and refunds) if it cannot deliver `size` within it.
pub fn build_spread(buy: &ExecOrder, sell: &ExecOrder, execution_fee_wei: u128) -> Result<Value> {
    check_spread(buy, sell)?;
    let is_call = buy.native_id.ends_with("-C");
    let fees = buy.max_fee_per_unit.unwrap_or_default() + sell.max_fee_per_unit.unwrap_or_default();
    let budget = buy.qty_native * (buy.limit_price_native - sell.limit_price_native + fees);
    if budget <= Decimal::ZERO {
        return Err(bad("non-positive spread budget"));
    }
    let amount_in = scaled_ceil(budget, 6).ok_or_else(|| bad("budget"))?;
    let min_size = scaled_floor(buy.qty_native, 18).ok_or_else(|| bad("size"))?;
    let data = encode_request(
        &[
            (true, option_id(buy)?, is_call),
            (false, option_id(sell)?, is_call),
        ],
        min_size,
        amount_in,
    );
    Ok(json!({
        "chain_id": CHAIN_ID, "to": POSITION_MANAGER,
        "data": format!("0x{}", hex::encode(data)),
        "value": format!("0x{execution_fee_wei:x}"),
        "num_contracts": min_size.to_string(), "budget_usdc": budget.round_dp(6).to_string(),
        "approval": {"to": USDC, "data": format!("0x{}", hex::encode(encode_usdc_approve(amount_in))), "spender": ROUTER, "note": "USDC allowance for the Callput Router (send only if the current allowance is lower)"},
        "bundle": "ONE transaction for both spread legs: send it once and submit its tx_hash for both order ids",
        "note": "Asynchronous: a keeper executes or cancels the request (max 180 s); the budget is refunded if cancelled."
    }))
}

fn word(hexs: &str, i: usize) -> Option<u128> {
    let h = hexs.trim_start_matches("0x");
    let w = h.get(i * 64..(i + 1) * 64)?;
    if !w[..32].bytes().all(|b| b == b'0') {
        return None;
    }
    u128::from_str_radix(&w[32..], 16).ok()
}

/// Map an `openPositionRequests` result to a venue state.
pub fn parse_request(ret: &str, id: &str) -> Result<VenueOrderState> {
    let status = word(ret, 9).ok_or_else(|| bad("request status"))?;
    let size = word(ret, 10)
        .map(|s| Decimal::from_i128_with_scale(s as i128, 18).normalize())
        .unwrap_or_default();
    // executionPrice is 1e30-scaled (fits u128): rescale to 18 decimals.
    let price = word(ret, 11)
        .map(|x| Decimal::from_i128_with_scale((x / 1_000_000_000_000) as i128, 18).normalize());
    let (st, fin) = match status {
        0 => (VenueStatus::Open, false),
        1 => (VenueStatus::Cancelled, true),
        2 => (VenueStatus::Filled, true),
        x => (VenueStatus::Unknown(format!("status {x}")), false),
    };
    Ok(VenueOrderState {
        venue_order_id: id.to_string(),
        status: st,
        filled_native: if fin && status == 2 {
            size
        } else {
            Decimal::ZERO
        },
        avg_price_native: if status == 2 { price } else { None },
        is_final: fin,
        raw: json!({"request_status": status, "size_out": size.to_string()}),
    })
}

#[derive(Debug)]
pub struct CallputUserTxExec {
    http: reqwest::Client,
    rpc: String,
}

impl CallputUserTxExec {
    pub fn mainnet() -> Self {
        Self::with_rpc(BASE_RPC)
    }
    pub fn with_rpc(rpc: &str) -> Self {
        Self {
            http: client(),
            rpc: rpc.into(),
        }
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
        if let Some(e) = v.get("error") {
            return Err(invalid(P, format!("{method}: {e}")));
        }
        Ok(v["result"].clone())
    }
    async fn execution_fee(&self) -> Result<u128> {
        let sel = format!("0x{}", hex::encode(&keccak(b"executionFee()")[..4]));
        let r = self
            .rpc(
                "eth_call",
                json!([{"to": POSITION_MANAGER, "data": sel}, "latest"]),
            )
            .await?;
        r.as_str()
            .and_then(|s| word(s, 0))
            .ok_or_else(|| bad("executionFee"))
    }
    /// id = `<tx_hash>:<keccak(calldata)>`.
    async fn check(&self, id: &str) -> Result<Option<VenueOrderState>> {
        let (tx_hash, expected) = id
            .split_once(':')
            .ok_or_else(|| bad(format!("order id {id}")))?;
        let tx = self
            .rpc("eth_getTransactionByHash", json!([tx_hash]))
            .await?;
        let receipt = self
            .rpc("eth_getTransactionReceipt", json!([tx_hash]))
            .await?;
        if tx.is_null() || receipt.is_null() {
            return Ok(None);
        }
        let to_ok = tx["to"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case(POSITION_MANAGER));
        let data_ok = tx["input"]
            .as_str()
            .and_then(|d| hex::decode(d.trim_start_matches("0x")).ok())
            .is_some_and(|b| hex::encode(keccak(&b)) == expected);
        if !to_ok || !data_ok {
            return Err(bad(
                "transaction does not match the prepared createOpenPosition (to/data differ)",
            ));
        }
        if receipt["status"].as_str() != Some("0x1") {
            return Ok(Some(VenueOrderState {
                venue_order_id: id.into(),
                status: VenueStatus::Rejected,
                filled_native: Decimal::ZERO,
                avg_price_native: None,
                is_final: true,
                raw: json!({"receipt_status": receipt["status"]}),
            }));
        }
        let topic = format!(
            "0x{}",
            hex::encode(keccak(b"GenerateRequestKey(address,bytes32,bool)"))
        );
        let key = receipt["logs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| {
                l["topics"][0]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case(&topic))
            })
            .and_then(|l| l["topics"][2].as_str().map(str::to_string))
            .ok_or_else(|| bad("no GenerateRequestKey event in the receipt"))?;
        let sel = hex::encode(&keccak(b"openPositionRequests(bytes32)")[..4]);
        let ret = self.rpc("eth_call", json!([{"to": POSITION_MANAGER, "data": format!("0x{sel}{}", key.trim_start_matches("0x"))}, "latest"])).await?;
        parse_request(ret.as_str().unwrap_or("0x"), id).map(Some)
    }
}

#[async_trait]
impl ExecutionVenue for CallputUserTxExec {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        let u = &o.venue_meta["user"];
        let data = hex::decode(u["prepared_data"].as_str()?.trim_start_matches("0x")).ok()?;
        Some(format!(
            "{}:{}",
            u["tx_hash"].as_str()?.to_lowercase(),
            hex::encode(keccak(&data))
        ))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        let id = self
            .precompute_order_id(o)
            .ok_or_else(|| bad("tx_hash + prepared transaction required"))?;
        let tx_hash = id.split(':').next().unwrap_or_default();
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
        Ok(self.check(&id).await?.unwrap_or(VenueOrderState {
            venue_order_id: id,
            status: VenueStatus::Unknown("transaction not mined yet".into()),
            filled_native: Decimal::ZERO,
            avg_price_native: None,
            is_final: false,
            raw: json!({}),
        }))
    }
    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        self.check(id).await
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: format!("{id} (cancel a pending request from the user's wallet)"),
        })
    }
    async fn prepare_user_tx(&self, _o: &ExecOrder) -> Result<Option<Value>> {
        Err(bad(
            "CallPut accepts only 2-leg spreads on-chain (single options revert with StrategyNotAllowed)",
        ))
    }
    async fn prepare_user_tx_bundle(&self, orders: &[ExecOrder]) -> Result<Option<Value>> {
        let [a, b] = orders else {
            return Err(bad("CallPut: a bundle is exactly 2 spread legs"));
        };
        let (buy, sell) = if a.side == Side::Buy { (a, b) } else { (b, a) };
        build_spread(buy, sell, self.execution_fee().await?).map(Some)
    }
    fn identity(&self) -> Value {
        json!({"venue": NETWORK, "custody": "NON_CUSTODIAL", "signed_by": "end user wallet sends the transaction", "position_manager": POSITION_MANAGER})
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

    /// A transaction sent by another address is never attributed to the user's order.
    #[tokio::test]
    async fn submit_refuses_a_transaction_from_another_sender() {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async {
                axum::Json(json!({"jsonrpc": "2.0", "id": 1,
                    "result": {"from": "0x2222222222222222222222222222222222222222", "to": POSITION_MANAGER, "input": "0x00"}}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let v = CallputUserTxExec::with_rpc(&format!("http://{addr}/"));
        let mut o = order();
        o.venue_meta["user"] = json!({"address": "0x1111111111111111111111111111111111111111",
            "tx_hash": format!("0x{}", "ab".repeat(32)), "prepared_data": "0x00"});
        let e = v.submit(&o).await.unwrap_err().to_string();
        assert!(e.contains("not sent by the declared address"), "{e}");
    }

    fn order() -> ExecOrder {
        ExecOrder {
            native_id: "BTC-30OCT26-78000-C".into(),
            venue_instrument_id: None,
            venue_meta: json!({"option_id": "0x0001006ad1d98000000001349800000000000000000000000000000000000000"}),
            max_fee_per_unit: Some(Decimal::new(5, 0)),
            side: Side::Buy,
            qty_native: Decimal::new(1, 1),
            limit_price_native: Decimal::new(6500, 0),
            salt: 1,
            timestamp: 0,
            time_in_force: crate::exec::TimeInForce::Ioc,
        }
    }

    fn leg(name: &str, side: Side, limit: i64) -> ExecOrder {
        let mut o = order();
        o.native_id = name.into();
        o.side = side;
        o.limit_price_native = Decimal::new(limit, 0);
        o.max_fee_per_unit = Some(Decimal::new(1, 0));
        o
    }

    /// Debit call spread -> one 2-leg request shaped like the live executed sample (isBuys [1,0],
    /// same type), budget = size x (buy - sell + fee caps). Wrong shapes are refused.
    #[test]
    fn builds_a_debit_spread_and_refuses_other_shapes() {
        let buy = leg("BTC-30OCT26-78000-C", Side::Buy, 900);
        let mut sell = leg("BTC-30OCT26-80000-C", Side::Sell, 400);
        sell.venue_meta = json!({"option_id": "0x0001006ad1d98000000001388000000000000000000000000000000000000000"});
        let tx = build_spread(&buy, &sell, 1).unwrap();
        assert_eq!(tx["budget_usdc"], "50.2"); // 0.1 x (900 - 400 + 1 + 1)
        let d = hex::decode(tx["data"].as_str().unwrap().trim_start_matches("0x")).unwrap();
        let w = |i: usize| &d[4 + 32 * i..4 + 32 * (i + 1)];
        assert_eq!(w(1)[31], 2, "length 2");
        assert_eq!((w(2)[31], w(3)[31]), (1, 0), "isBuys [true, false]");
        assert_eq!((w(10)[31], w(11)[31]), (1, 1), "isCalls [true, true]");
        // Credit spread (buy the higher call) / mixed types / different size: refused.
        assert!(build_spread(&sell_as_buy(&sell), &buy_as_sell(&buy), 1).is_err());
        let put = leg("BTC-30OCT26-80000-P", Side::Sell, 400);
        assert!(check_spread(&buy, &put).is_err());
        let mut small = sell.clone();
        small.qty_native = Decimal::new(5, 2);
        assert!(check_spread(&buy, &small).is_err());
    }

    fn sell_as_buy(o: &ExecOrder) -> ExecOrder {
        let mut x = o.clone();
        x.side = Side::Buy;
        x
    }
    fn buy_as_sell(o: &ExecOrder) -> ExecOrder {
        let mut x = o.clone();
        x.side = Side::Sell;
        x
    }

    /// Writes the call so scripts/check_callput_abi.py can decode it with eth_abi.
    #[test]
    fn builds_a_buy_request_with_budget_and_size_floor() {
        let tx = build_open(&order(), 60_000_000_000_000).unwrap();
        assert_eq!(tx["to"], POSITION_MANAGER);
        assert_eq!(tx["value"], "0x3691d6afc000");
        assert_eq!(tx["budget_usdc"], "650.5"); // 0.1 x (6500 + 5)
        assert!(tx["data"].as_str().unwrap().starts_with("0x4fd5f4af"));
        // The allowance goes to the Router (the PositionManager pulls through it), never to the PM.
        let ap = tx["approval"]["data"].as_str().unwrap().to_lowercase();
        assert!(ap.starts_with("0x095ea7b3"));
        assert_eq!(&ap[34..74], ROUTER.trim_start_matches("0x").to_lowercase());
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/callput_open_vector.json");
        std::fs::write(out, tx.to_string()).unwrap();
        let mut sell = order();
        sell.side = Side::Sell;
        assert!(build_open(&sell, 1).is_err());
    }

    #[test]
    fn request_status_words_map_to_states() {
        // Live executed request (2026-10-08): status 2, sizeOut 11.94e18, executionPrice 3.982e30.
        let mut words = vec![0u128; 14];
        words[9] = 2;
        words[10] = 11_943_548_970_366_649_924;
        words[11] = 3_982_000_000_000_000_000_000_000_000_000;
        let enc = |w: &[u128]| -> String {
            std::iter::once("0x".to_string())
                .chain(w.iter().map(|x| format!("{x:064x}")))
                .collect()
        };
        let s = parse_request(&enc(&words), "id").unwrap();
        assert_eq!(s.status, VenueStatus::Filled);
        assert_eq!(s.filled_native.round_dp(4), Decimal::new(119435, 4));
        assert_eq!(s.avg_price_native, Some(Decimal::new(3982, 3)));
        words[9] = 0;
        assert!(!parse_request(&enc(&words), "id").unwrap().is_final);
        words[9] = 1;
        let c = parse_request(&enc(&words), "id").unwrap();
        assert_eq!(c.status, VenueStatus::Cancelled);
        assert_eq!(c.filled_native, Decimal::ZERO);
    }
}
