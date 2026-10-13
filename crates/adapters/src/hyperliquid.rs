//! Hyperliquid HIP-4 outcome markets — BTC/ETH fixed-threshold price BINARIES, public REST, READ-ONLY.
//! Onchain CLOB (HyperCore): trading needs a funded Hyperliquid account (wallet signature), no KYC.
//!
//! Verified live 2026-10-06:
//! - `POST /info {"type":"outcomeMeta"}`: 307 outcomes; `outcomes[]` with `outcome` (id), `name`,
//!   `description` (pipe-delimited `key:value`), `sideSpecs` ([Yes, No]), `quoteToken` ("USDC" on all
//!   307), optional `venue` (deployer: "out", "skew", "txyz"; absent on HL's own "Recurring" markets),
//!   `deployerFeeScale`; plus `questions[]` (multi-outcome groups) and top-level `feeScale` "1.0".
//! - `POST /info {"type":"outcomeTemplates"}`: template `binaryPrice` = "{perp} above {threshold} at
//!   {time}?", "resolves to Yes if the {perp} price is above {threshold} at {time} ... Settlement is
//!   according to the {seconds}-second TWAP of {priceDescription} price ending at {time}".
//! - `POST /info {"type":"l2Book","coin":"#<10*outcome+side>"}`: `levels` = [bids, asks] of
//!   `{px, sz, n}`, `time` (ms). Side 0 = Yes [DOC asset-ids: encoding = 10*outcome + side, spot coin
//!   `#<encoding>`, asset id 100_000_000 + encoding]. Unknown coin -> JSON `null`.
//!
//! Mapped (BTC/ETH only, Yes side = Call, strike = threshold, payout 1 per contract):
//! - `name == "Recurring"`, `class:priceBinary` (HL's own daily market, settles 06:00 UTC to the HL perp
//!   mark) [DOC contract-specifications#recurring-outcomes: YES iff the mark linearly interpolated
//!   between the updates immediately before/after the settlement time is >= targetPrice].
//! - `name == "template:binaryPrice"` with `perp` exactly BTC/ETH (third-party deployers; settlement
//!   source = the template's `priceDescription`, e.g. Pyth BTC/USD, HL BTC perp trade or mark).
//!
//! Skipped: priceBucket / "Recurring Named Outcome" (ranges), priceTouch (path-dependent), every other
//! template (sports, rates, IPO, awards), other underlyings (SOL, HYPE, `xyz:*`), expired outcomes.
//!
//! Quote currency: `quoteToken` read from the response ("USDC" everywhere on 2026-10-06; some third-party
//! write-ups say outcomes settle in USDH — the live API says USDC, which is what we report).
//!
//! Fees: UNKNOWN. HIP-4 page: "Fees are currently zero for outcome markets for initial testing"; the
//! fees page: outcome trading "only charges fees when closing or settling, not when opening", maker pays
//! zero instead of a rebate, but no outcome rate is published; live meta carries `feeScale` 1.0 and
//! `deployerFeeScale` 1.0 without the base rate. No number is assumed.

use crate::http::{client, read_json};
use crate::{Connector, ConnectorError, Result, dec, invalid};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::{Value, json};
use std::collections::HashMap;

pub const MAINNET_INFO: &str = "https://api.hyperliquid.xyz/info";
pub const NETWORK: &str = "hyperliquid-mainnet";
pub const SPEC_VERSION: &str = "hyperliquid-hip4/2026-10-06";
const HIP4_DOC: &str = "https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-4-outcome-markets";
const FEE_SOURCE: &str = "https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees (outcome trading section) + HIP-4 page";
const RECURRING_WINDOW: &str = "HL perp mark linearly interpolated between the mark updates immediately before and after expiry; YES iff >= target (contract-specifications#recurring-outcomes)";
/// Underlyings mapped by this connector (exact `perp` / `underlying` value in the description).
const UNDERLYINGS: [&str; 2] = ["BTC", "ETH"];
/// Outcome asset ids start here [DOC asset-ids].
const ASSET_ID_BASE: u64 = 100_000_000;

const P: PlatformId = PlatformId::Hyperliquid;

pub struct HyperliquidConnector {
    http: reqwest::Client,
    info: String,
}

impl Default for HyperliquidConnector {
    fn default() -> Self {
        Self::new(MAINNET_INFO)
    }
}

impl HyperliquidConnector {
    pub fn new(info: &str) -> Self {
        Self {
            http: client(),
            info: info.trim_end_matches('/').into(),
        }
    }

    async fn post(&self, body: Value) -> Result<Value> {
        let resp = self.http.post(&self.info).json(&body).send().await;
        read_json(P, resp).await
    }
}

/// Spot coin of one side of an outcome: `#<10*outcome + side>` (side 0 = Yes).
pub fn side_coin(outcome: u64, side: u64) -> String {
    format!("#{}", 10 * outcome + side)
}

/// `key:value|key:value` -> map (values may themselves contain ':', e.g. `perp:xyz:SP500`).
fn parse_description(s: &str) -> HashMap<&str, &str> {
    s.split('|').filter_map(|kv| kv.split_once(':')).collect()
}

/// `YYYYMMDD-HHMM`, UTC (the Recurring `expiry:20261007-0600` is documented as 06:00 UTC; templates use
/// the same `dateTime` format).
fn parse_time(s: &str, field: &str) -> Result<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M")
        .map(|n| Utc.from_utc_datetime(&n))
        .map_err(|e| invalid(P, format!("{field}={s}: {e}")))
}

fn ms(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    crate::ts_ms(P, v.as_i64(), field)
}

/// What a mappable outcome resolves on.
struct Spec<'a> {
    underlying: &'a str,
    strike: Decimal,
    expiry: DateTime<Utc>,
    settlement_index: String,
    settlement_window: String,
    yes_rule: &'static str,
}

fn spec<'a>(name: &str, d: &HashMap<&str, &'a str>) -> Result<Option<Spec<'a>>> {
    let get = |k: &str| {
        d.get(k)
            .copied()
            .ok_or_else(|| invalid(P, format!("{name}: description has no `{k}`")))
    };
    match name {
        "Recurring" => {
            if d.get("class") != Some(&"priceBinary") {
                return Ok(None);
            }
            let u = get("underlying")?;
            Ok(Some(Spec {
                underlying: u,
                strike: dec(P, &Value::String(get("targetPrice")?.into()), "targetPrice")?,
                expiry: parse_time(get("expiry")?, "expiry")?,
                settlement_index: format!("Hyperliquid {u} perp mark price (HyperCore)"),
                settlement_window: RECURRING_WINDOW.into(),
                yes_rule: "interpolated mark >= target",
            }))
        }
        "template:binaryPrice" => {
            let seconds = get("seconds")?;
            Ok(Some(Spec {
                underlying: get("perp")?,
                strike: dec(P, &Value::String(get("threshold")?.into()), "threshold")?,
                expiry: parse_time(get("time")?, "time")?,
                settlement_index: get("priceDescription")?.to_string(),
                settlement_window: format!(
                    "{seconds}-second TWAP ending at expiry; YES iff strictly above threshold (template binaryPrice)"
                ),
                yes_rule: "TWAP > threshold",
            }))
        }
        _ => Ok(None),
    }
}

/// One `outcomeMeta.outcomes[]` entry -> Binary instrument on its YES side, or None if not mappable.
pub fn parse_outcome(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    let outcome = v["outcome"]
        .as_u64()
        .ok_or_else(|| invalid(P, format!("outcome id missing: {v}")))?;
    let name = v["name"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("outcome {outcome}: name")))?;
    let desc = v["description"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("outcome {outcome}: description")))?;
    let d = parse_description(desc);
    let Some(s) = spec(name, &d)? else {
        return Ok(None);
    };
    if s.underlying != underlying || !UNDERLYINGS.contains(&underlying) {
        return Ok(None);
    }
    // Side 0 must be the YES side (sideSpecs order); anything else is not a threshold binary we know.
    match v["sideSpecs"][0]["name"].as_str() {
        Some("Yes" | "template:Yes") => {}
        other => {
            return Err(invalid(
                P,
                format!("outcome {outcome}: side 0 is {other:?}, expected Yes"),
            ));
        }
    }
    if s.expiry <= now {
        return Ok(None);
    }
    if s.strike <= Decimal::ZERO {
        return Err(invalid(
            P,
            format!("outcome {outcome}: non-positive threshold"),
        ));
    }
    let quote = v["quoteToken"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("outcome {outcome}: quoteToken")))?;
    let venue = v["venue"].as_str();
    let encoding = 10 * outcome;
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: side_coin(outcome, 0),
        },
        family: ProductFamily::Binary,
        underlying: underlying.into(),
        option_kind: Some(OptionKind::Call),
        strike: Some(s.strike.normalize()),
        expiry: Some(s.expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: Decimal::ONE,
        premium_currency: quote.into(),
        collateral_assets: vec![quote.into()],
        settlement_asset: Some(quote.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(match venue {
            Some(ven) => format!("{} (deployer {ven})", s.settlement_index),
            None => s.settlement_index,
        }),
        settlement_window: Some(s.settlement_window),
        tick_size: None,
        min_amount: None,
        amount_step: None,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: json!({
            "outcome": outcome, "name": name, "description": desc, "venue": v["venue"],
            "deployerFeeScale": v["deployerFeeScale"], "yes_coin": side_coin(outcome, 0),
            "no_coin": side_coin(outcome, 1), "yes_asset_id": ASSET_ID_BASE + encoding,
            "yes_rule": s.yes_rule, "payout_per_contract": "1 (outcomes resolve to 0 or 1)",
            "venue_type": "onchain_clob", "account_required": "funded Hyperliquid account (wallet signature), no KYC",
            "doc": HIP4_DOC
        }),
    }))
}

pub fn parse_outcome_meta(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v["outcomes"]
        .as_array()
        .ok_or_else(|| invalid(P, "outcomeMeta.outcomes: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_outcome(x, underlying, now)? {
            out.push(i);
        }
    }
    out.sort_by(|a, b| (a.expiry, a.strike).cmp(&(b.expiry, b.strike)));
    Ok(out)
}

fn levels(v: &Value, idx: usize, side: &str) -> Result<Vec<Level>> {
    let arr = v["levels"][idx]
        .as_array()
        .ok_or_else(|| invalid(P, format!("l2Book.levels {side} missing")))?;
    arr.iter()
        .map(|l| {
            let price = dec(P, &l["px"], "level.px")?;
            let size = dec(P, &l["sz"], "level.sz")?;
            if price <= Decimal::ZERO || price >= Decimal::ONE || size <= Decimal::ZERO {
                return Err(invalid(
                    P,
                    format!("book level out of (0,1) / non-positive: {l}"),
                ));
            }
            Ok(Level { price, size })
        })
        .collect()
}

/// `l2Book` of the YES coin -> snapshot. Price = USDC per contract paying 1 (0..1), size = contracts.
pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    if v.is_null() {
        return Err(ConnectorError::UnknownInstrument {
            platform: P,
            native_id: instrument.key.native_id.clone(),
        });
    }
    if v["coin"] != instrument.key.native_id.as_str() {
        return Err(invalid(P, "response coin does not match the request"));
    }
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(v, 0, "bids")?,
        asks: levels(v, 1, "asks")?,
        source_ts: Some(ms(&v["time"], "time")?),
        received_at,
        index_price: None,
        mark_price: None,
        provenance,
        access: BookAccess::Api,
    })
}

/// Unknown on purpose: no published outcome rate (see module doc).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let reason = "HIP-4 outcome fee rate not published: HIP-4 page says fees are currently zero for initial testing, the fees page says outcome fees are charged only on closing/settling (makers pay zero, no rebate) without a rate, and live outcomeMeta only gives feeScale/deployerFeeScale 1.0 (multipliers of an unpublished base)";
    Ok(FeeSchedule {
        taker: FeeFormula::Unknown {
            reason: reason.into(),
        },
        maker: FeeFormula::Unknown {
            reason: reason.into(),
        },
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Opening trades documented as fee-free, closing/settlement fee rate unverified; builder fees may apply on sells with a builder code.".into(),
    })
}

#[async_trait]
impl Connector for HyperliquidConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        if !UNDERLYINGS.contains(&underlying) {
            return Ok(vec![]);
        }
        let v = self.post(json!({"type": "outcomeMeta"})).await?;
        parse_outcome_meta(&v, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let v = self
            .post(json!({"type": "l2Book", "coin": instrument.key.native_id}))
            .await?;
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(instrument, &v, received_at, Provenance::LiveData)?;
        Ok(crate::MarketData::rest(book, network, t1))
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        fee_schedule(instrument)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn fixture(name: &str) -> Value {
        crate::fixture("recorded-2026-10-06", name)
    }
    fn at() -> DateTime<Utc> {
        "2026-10-06T12:00:00Z".parse().unwrap()
    }
    fn ids(v: &[Instrument]) -> Vec<&str> {
        v.iter().map(|i| i.key.native_id.as_str()).collect()
    }

    #[test]
    fn maps_btc_eth_threshold_binaries_and_skips_the_rest() {
        let v = fixture("hyperliquid_outcome_meta.json");
        let btc = parse_outcome_meta(&v, "BTC", at()).unwrap();
        // Recurring 9015, skew Pyth 7581 + HL-trade 9084, out mark 9148. Skipped: priceTouch 7173,
        // priceBucket named outcome 9021 / fallback 9020, sports 1473, xyz:XYZ100 7113, SOL/HYPE.
        let mut got = ids(&btc);
        got.sort();
        assert_eq!(got, ["#75810", "#90150", "#90840", "#91480"]);
        let eth = parse_outcome_meta(&v, "ETH", at()).unwrap();
        let mut got = ids(&eth);
        got.sort();
        assert_eq!(got, ["#75870", "#90160", "#91500"]);
        assert!(parse_outcome_meta(&v, "SOL", at()).unwrap().is_empty());
        assert!(parse_outcome_meta(&v, "HYPE", at()).unwrap().is_empty());
        assert!(
            btc.iter()
                .chain(&eth)
                .all(|i| i.family == ProductFamily::Binary
                    && i.option_kind == Some(OptionKind::Call)
                    && i.contract_multiplier == dec!(1)
                    && i.premium_currency == "USDC")
        );
    }

    #[test]
    fn recurring_btc_daily_mapping_and_utc_expiry() {
        let v = fixture("hyperliquid_outcome_meta.json");
        let btc = parse_outcome_meta(&v, "BTC", at()).unwrap();
        let r = btc.iter().find(|i| i.key.native_id == "#90150").unwrap();
        assert_eq!(r.strike, Some(dec!(85501)));
        assert_eq!(r.expiry.unwrap().to_rfc3339(), "2026-10-07T06:00:00+00:00");
        assert!(
            r.settlement_index
                .as_deref()
                .unwrap()
                .contains("BTC perp mark")
        );
        assert_eq!(r.native["no_coin"], "#90151");
        assert_eq!(r.native["yes_asset_id"], 100_090_150u64);
        let p = btc.iter().find(|i| i.key.native_id == "#75810").unwrap();
        assert_eq!(p.strike, Some(dec!(85090)));
        assert_eq!(p.expiry.unwrap().to_rfc3339(), "2026-10-09T08:00:00+00:00");
        assert_eq!(
            p.settlement_index.as_deref(),
            Some("the Pyth BTC/USD price (deployer skew)")
        );
        assert!(
            p.settlement_window
                .as_deref()
                .unwrap()
                .starts_with("90-second TWAP")
        );
        let eth = parse_outcome_meta(&v, "ETH", at()).unwrap();
        let m = eth.iter().find(|i| i.key.native_id == "#91500").unwrap();
        assert_eq!(m.strike, Some(dec!(2714.9)));
        // Sorted by expiry: the 12:30 market comes first.
        assert_eq!(eth[0].key.native_id, "#91500");
    }

    #[test]
    fn expired_outcomes_are_skipped() {
        let v = fixture("hyperliquid_outcome_meta.json");
        let later: DateTime<Utc> = "2026-10-06T12:30:00Z".parse().unwrap();
        let btc = parse_outcome_meta(&v, "BTC", later).unwrap();
        assert!(!ids(&btc).contains(&"#91480")); // expiry 12:30 == now -> skipped
        assert_eq!(btc.len(), 3);
    }

    #[test]
    fn book_worked_example_and_unknown_fee() {
        let v = fixture("hyperliquid_outcome_meta.json");
        let i = parse_outcome_meta(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "#90150")
            .unwrap();
        let b = parse_book(
            &i,
            &fixture("hyperliquid_l2book_90150.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // "BTC >= 85,501 at 2026-10-07 06:00Z": best bid 0.719 x 121, best ask 0.725 x 34 -> buying
        // 34 contracts costs 34 * 0.725 = 24.65 USDC and pays 34 USDC if YES (implied p ~ 72.2 %).
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(0.719),
                size: dec!(121)
            }
        );
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(0.725),
                size: dec!(34)
            }
        );
        assert!(b.bids.windows(2).all(|w| w[0].price > w[1].price));
        assert!(b.asks.windows(2).all(|w| w[0].price < w[1].price));
        assert_eq!(b.asks[0].price * b.asks[0].size, dec!(24.650));
        assert_eq!(
            b.source_ts.unwrap().to_rfc3339(),
            "2026-10-06T12:29:33.837+00:00"
        );
        assert_eq!(b.access, BookAccess::Api);
        // Mismatched coin and unknown coin (`null`) are typed errors.
        let mut other = i.clone();
        other.key.native_id = "#75810".into();
        assert!(
            parse_book(
                &other,
                &fixture("hyperliquid_l2book_90150.json"),
                at(),
                Provenance::Fixture
            )
            .is_err()
        );
        assert!(matches!(
            parse_book(&i, &Value::Null, at(), Provenance::Fixture),
            Err(ConnectorError::UnknownInstrument { .. })
        ));
        let f = fee_schedule(&i).unwrap();
        assert!(matches!(f.taker, FeeFormula::Unknown { .. }));
        assert!(matches!(f.maker, FeeFormula::Unknown { .. }));
    }

    #[test]
    fn description_values_may_contain_colons() {
        let d = parse_description("perp:xyz:SP500|threshold:7590|time:20261007-2000");
        assert_eq!(d["perp"], "xyz:SP500");
        assert!(parse_time("2026-10-07", "time").is_err());
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_hyperliquid_hip4_binaries() {
        let c = HyperliquidConnector::default();
        let btc = c.list_instruments("BTC").await.unwrap();
        let eth = c.list_instruments("ETH").await.unwrap();
        println!(
            "hyperliquid HIP-4 binaries: BTC {} ETH {}",
            btc.len(),
            eth.len()
        );
        for i in btc.iter().chain(&eth) {
            println!(
                "  {} {} >{} {} [{}]",
                i.key.native_id,
                i.underlying,
                i.strike.unwrap(),
                i.expiry.unwrap(),
                i.settlement_index.as_deref().unwrap_or("")
            );
        }
        assert!(!btc.is_empty());
        let i = btc
            .iter()
            .find(|i| i.native["name"] == "Recurring")
            .unwrap_or(&btc[0]);
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "book {} bid {:?} ask {:?} ts {:?} ({} ms)",
            i.key.native_id,
            md.book.bids.first(),
            md.book.asks.first(),
            md.book.source_ts,
            md.network.as_millis()
        );
    }
}
