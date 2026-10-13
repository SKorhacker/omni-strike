//! Coincall — USD-settled BTC/ETH options, public REST, READ-ONLY. Centralised exchange (not
//! onchain): trading needs the user's own Coincall account and API keys.
//!
//! Verified live 2026-10-06:
//! - `GET /open/option/getInstruments/{BTC|ETH}` -> `{code:0, data:[...]}`: 938 BTC + 778 ETH
//!   options, 12 expiries (08:00 UTC). Fields `symbolName` (`BTCUSD-8OCT26-86000-C`), `strike`,
//!   `expirationTimestamp` (ms), `isActive`, `minQty` (BTC 0.01 / ETH 0.1), `tickSize` (1 / 0.1).
//!   Numbers are JSON floats (may use exponent notation): parsed via their decimal text.
//! - `GET /open/option/order/orderbook/v1/{symbol}`: `bids`/`asks` `{price, size}` strings, up to
//!   100 levels, NO timestamp. Price = USD per 1 underlying; size in underlying units.
//! - `GET /open/option/detail/v1/{symbol}`: `multiplier` 1.0 (so 1 native unit = 1 BTC/ETH and the
//!   book price is the premium per native unit), `indexPrice`, `markPrice`, OI, 24h volume.
//!   The connector re-checks `multiplier` on every snapshot and refuses anything else.
//! - `GET /open/public/config/v1` -> `data.optionConfig.{BTCUSD,ETHUSD}`: `settle` "USD",
//!   `takerFee` 0.0003, `makerFee` 0.0003.
//! - Cross-check: ATM 86000-C 8OCT26 680/730, mark 708.8 USD per BTC (Gate 709.8, Bullish 710.8).
//! - Liquidity (sum of the per-expiry option chains): OI 4,578 BTC (~414 M USD) + 54,495 ETH
//!   (~154 M USD); ~3-18 BTC per level near the money.
//!
//! Premium currency: the API reports `settle: "USD"` (Coincall account unit). It is kept as "USD"
//! (not relabelled USDC/USDT) because the stablecoin backing that unit was not verified.
//!
//! Fees: the public config gives `takerFee`/`makerFee` 0.0003 per symbol; no official page with the
//! fee BASIS or a premium cap was found. The schedule charges 0.03 % of the underlying notional
//! with NO cap: an UPPER BOUND whatever the true basis (notional >> premium) or cap.
//! Exercise style / settlement price method: not verified in official docs (left unknown).

use crate::http::{client, read_json};
use crate::{Connector, Result, invalid};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::str::FromStr;

pub const MAINNET_REST: &str = "https://api.coincall.com";
pub const NETWORK: &str = "coincall-usd-options";
pub const SPEC_VERSION: &str = "coincall-options/2026-10-06";
const FEE_SOURCE: &str = "https://api.coincall.com/open/public/config/v1 (optionConfig.takerFee/makerFee); basis and cap not documented publicly";

const P: PlatformId = PlatformId::Coincall;

pub struct CoincallConnector {
    http: reqwest::Client,
    rest: String,
}

impl Default for CoincallConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl CoincallConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
        }
    }

    /// Coincall wraps every answer in `{code, msg, data}`; `code != 0` is an error even on HTTP 200.
    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self.http.get(format!("{}{path}", self.rest)).send().await;
        unwrap_data(read_json(P, resp).await?, path)
    }
}

fn unwrap_data(v: Value, path: &str) -> Result<Value> {
    if v["code"] != 0 {
        return Err(invalid(
            P,
            format!("{path}: code {} msg {}", v["code"], v["msg"]),
        ));
    }
    match v.get("data") {
        Some(Value::Null) | None => Err(invalid(P, format!("{path}: no data"))),
        Some(d) => Ok(d.clone()),
    }
}

/// Decimal from a JSON string or number, accepting exponent notation (`5.1E+2`, `1e-5`)
/// through the number's own decimal text (never through f64 arithmetic).
fn num(v: &Value, field: &str) -> Result<Decimal> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err(invalid(P, format!("{field}: expected number, got {v}"))),
    };
    Decimal::from_str(&s)
        .or_else(|_| Decimal::from_scientific(&s))
        .map_err(|e| invalid(P, format!("{field}={s}: {e}")))
}

fn opt_num(v: Option<&Value>, field: &str) -> Result<Option<Decimal>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(x) => num(x, field).map(Some),
    }
}

/// `cfg` = `data.optionConfig.<BASE>USD` from the public config (fee rates, settle unit).
pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    cfg: &Value,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["baseCurrency"] != underlying || v["isActive"] != true {
        return Ok(None);
    }
    let name = v["symbolName"]
        .as_str()
        .ok_or_else(|| invalid(P, "symbolName"))?;
    if !name.starts_with(&format!("{underlying}USD-")) {
        return Err(invalid(
            P,
            format!("{name}: unexpected symbol for {underlying}"),
        ));
    }
    let kind = match name.rsplit('-').next() {
        Some("C") => OptionKind::Call,
        Some("P") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: option suffix {other:?}"))),
    };
    let exp_ms = v["expirationTimestamp"]
        .as_i64()
        .ok_or_else(|| invalid(P, format!("{name}: expirationTimestamp")))?;
    let expiry = Utc
        .timestamp_millis_opt(exp_ms)
        .single()
        .ok_or_else(|| invalid(P, format!("{name}: expiry out of range")))?;
    if expiry <= now {
        return Ok(None);
    }
    let settle = cfg["settle"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: optionConfig.settle missing")))?;
    let min_qty = opt_num(v.get("minQty"), "minQty")?.map(|d| d.normalize());
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(num(&v["strike"], "strike")?.normalize()),
        expiry: Some(expiry),
        exercise_style: None,
        // Verified via /open/option/detail (`multiplier` 1.0); re-checked on every snapshot.
        contract_multiplier: Decimal::ONE,
        premium_currency: settle.into(),
        collateral_assets: vec![settle.into()],
        settlement_asset: Some(settle.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("coincall:{underlying}USD index")),
        settlement_window: None,
        tick_size: opt_num(v.get("tickSize"), "tickSize")?.map(|d| d.normalize()),
        min_amount: min_qty,
        amount_step: min_qty,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "takerFee": cfg["takerFee"], "makerFee": cfg["makerFee"], "settle": settle,
            "startTimestamp": v["startTimestamp"],
            "venue_type": "centralised_exchange",
            "account_required": "user's own Coincall account + API keys"
        }),
    }))
}

/// `instruments` = `data` of getInstruments, `config` = `data` of /open/public/config/v1.
pub fn parse_instruments(
    instruments: &Value,
    config: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = instruments
        .as_array()
        .ok_or_else(|| invalid(P, "instruments: not an array"))?;
    let cfg = &config["optionConfig"][format!("{underlying}USD")];
    if !cfg.is_object() {
        return Err(invalid(P, format!("optionConfig.{underlying}USD missing")));
    }
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, cfg, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn levels(v: &Value, side: &str) -> Result<Vec<Level>> {
    let arr = v[side]
        .as_array()
        .ok_or_else(|| invalid(P, format!("orderbook.{side} missing")))?;
    arr.iter()
        .map(|l| {
            let price = num(&l["price"], "level.price")?;
            let size = num(&l["size"], "level.size")?;
            if price <= Decimal::ZERO || size <= Decimal::ZERO {
                return Err(invalid(P, format!("non-positive book level {l}")));
            }
            Ok(Level { price, size })
        })
        .collect()
}

/// `book` / `detail` = the `data` objects of the orderbook and option-detail endpoints.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    detail: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let id = instrument.key.native_id.as_str();
    if book["symbol"] != id || detail["symbol"] != id {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    let m = num(&detail["multiplier"], "detail.multiplier")?;
    if m != instrument.contract_multiplier {
        return Err(invalid(
            P,
            format!("{id}: multiplier {m} != {}", instrument.contract_multiplier),
        ));
    }
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(book, "bids")?,
        asks: levels(book, "asks")?,
        // The REST order book carries no timestamp.
        source_ts: None,
        received_at,
        index_price: positive(opt_num(detail.get("indexPrice"), "indexPrice")?),
        // Multiplier 1 (checked above): mark per underlying unit == mark per contract.
        mark_price: positive(opt_num(detail.get("markPrice"), "markPrice")?),
        provenance,
        access: BookAccess::Api,
    })
}

/// UPPER BOUND: `takerFee`/`makerFee` x underlying notional, uncapped (see module docs).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let part = |field: &str| -> Result<FeeFormula> {
        Ok(FeeFormula::PctOfUnderlyingNotional {
            rate: num(&instrument.native[field], field)?,
        })
    };
    Ok(FeeSchedule {
        taker: part("takerFee")?,
        maker: part("makerFee")?,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "UPPER BOUND: rate from the public config applied to the underlying notional with no premium cap (basis/cap not documented publicly; a cap or a premium basis would only lower it). Exercise/delivery fee not modelled. Centralised exchange: needs the user's own account.".into(),
    })
}

#[async_trait]
impl Connector for CoincallConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    /// Keep-warm every 3 s: public rate limits trip at 1 s with several hot books (2026-10-06).
    fn hot_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3)
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let ins_path = format!("/open/option/getInstruments/{underlying}");
        let (ins, cfg) = tokio::join!(self.get(&ins_path), self.get("/open/public/config/v1"));
        parse_instruments(&ins?, &cfg?, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let id = &instrument.key.native_id;
        let (book_path, detail_path) = (
            format!("/open/option/order/orderbook/v1/{id}"),
            format!("/open/option/detail/v1/{id}"),
        );
        let (book, detail) = tokio::join!(self.get(&book_path), self.get(&detail_path));
        let (book, detail) = (book?, detail?);
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book,
            &detail,
            received_at,
            Provenance::LiveData,
        )?;
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
        "2026-10-06T09:00:00Z".parse().unwrap()
    }
    fn data(name: &str) -> Value {
        unwrap_data(fixture(name), name).unwrap()
    }
    fn inst() -> Instrument {
        parse_instruments(
            &data("coincall_instruments_btc.json"),
            &data("coincall_config_option.json"),
            "BTC",
            at(),
        )
        .unwrap()
        .into_iter()
        .find(|i| i.key.native_id == "BTCUSD-8OCT26-86000-C")
        .unwrap()
    }

    #[test]
    fn parses_instruments() {
        let all = parse_instruments(
            &data("coincall_instruments_btc.json"),
            &data("coincall_config_option.json"),
            "BTC",
            at(),
        )
        .unwrap();
        assert_eq!(all.len(), 6);
        let i = inst();
        assert_eq!(i.strike, Some(dec!(86000)));
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-08T08:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.min_amount, Some(dec!(0.01)));
        assert_eq!(i.tick_size, Some(dec!(1)));
        assert_eq!(i.premium_currency, "USD");
        assert!(all.iter().any(|i| i.option_kind == Some(OptionKind::Put)));
    }

    #[test]
    fn error_code_is_not_success_and_exponents_parse() {
        let e = serde_json::json!({"code": 4003, "msg": "token auth fail", "data": null});
        assert!(unwrap_data(e, "/x").is_err());
        let v: Value = serde_json::from_str(r#"{"a": 5.1E+2, "b": 1e-5}"#).unwrap();
        assert_eq!(num(&v["a"], "a").unwrap(), dec!(510));
        assert_eq!(num(&v["b"], "b").unwrap(), dec!(0.00001));
    }

    #[test]
    fn book_and_fees() {
        let i = inst();
        let b = parse_book(
            &i,
            &data("coincall_book.json"),
            &data("coincall_detail.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(730),
                size: dec!(3)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(680),
                size: dec!(17.77)
            }
        );
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(b.index_price, Some(dec!(86041.83184782)));
        assert_eq!(b.mark_price, Some(dec!(708.7705204)));
        assert!(b.source_ts.is_none());

        let mut d = data("coincall_detail.json");
        d["multiplier"] = serde_json::json!(0.01);
        assert!(
            parse_book(
                &i,
                &data("coincall_book.json"),
                &d,
                at(),
                Provenance::Fixture
            )
            .is_err()
        );

        let f = fee_schedule(&i).unwrap();
        // 3 BTC at 730: 0.0003 x 3 x 86,000 = 77.4 USD (uncapped upper bound).
        let fee = f.taker.compute(&oa_core::fees::FeeInputs {
            qty_native: dec!(3),
            qty_underlying: dec!(3),
            premium: dec!(2190),
            index_price: Some(dec!(86000)),
            mark_price: None,
        });
        assert_eq!(fee, oa_core::fees::FeeAmount::Known { amount: dec!(77.4) });
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_coincall_options() {
        let c = CoincallConnector::default();
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            assert!(!cat.is_empty());
            let i = &cat[cat.len() / 2];
            let md = c.get_market_data(i).await.unwrap();
            println!(
                "coincall {u} {} instruments; {} bids {:?} asks {:?} index {:?} mark {:?}",
                cat.len(),
                i.key.native_id,
                md.book.bids.first(),
                md.book.asks.first(),
                md.book.index_price,
                md.book.mark_price
            );
            assert!(md.book.index_price.is_some());
        }
    }
}
