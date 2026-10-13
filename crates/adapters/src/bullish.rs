//! Bullish — USDC-settled BTC options, public REST, READ-ONLY. Centralised exchange (not onchain):
//! trading needs the user's own Bullish account (KYC, eligible jurisdictions only) and API keys.
//!
//! Verified live 2026-10-06:
//! - `GET /trading-api/v1/markets?marketType=OPTION`: 1,386 option markets, ALL on BTC (no ETH
//!   options listed). Fields `symbol` (`BTC-USDC-20261008-86000-C`), `optionType` CALL/PUT,
//!   `optionStrikePrice`, `expiryDatetime` (08:00 UTC), `contractMultiplier` "1",
//!   `settlementAssetSymbol` USDC, `quoteSymbol` USDC, `tickSize` (USDC), `minQuantityLimit`
//!   0.0005, `quantityPrecision` 8, `premiumCapRatio` 0.10, `marketEnabled`/`createOrderEnabled`.
//! - `GET /trading-api/v1/markets/{symbol}/orderbook/hybrid`: `bids`/`asks`
//!   `{price, priceLevelQuantity}` with price in USDC per contract (= per 1 BTC, multiplier 1) and
//!   quantity in contracts (= BTC); `timestamp` (ms, string).
//! - `GET /trading-api/v1/index-prices/BTC` (`price`), `GET /trading-api/v1/markets/{symbol}/tick`
//!   (`markPrice` per contract). Cross-check: ATM 86000-C 8OCT26 670/720 vs Deribit-like books,
//!   mark 710.8 = Gate 709.8 = Coincall 708.8 USD per BTC.
//! - Liquidity: large displayed OI (sampled ATM strikes ~184 M USD) and 5-13 BTC per level near
//!   the money, but small 24h on-screen volume (~24 k USD on the sampled ATM strikes).
//!
//! Style/settlement [DOC https://paradigm.co/help/products/derivatives/venues/bullish]: European,
//! cash-settled in USDC. Settlement price methodology not found in public docs.
//!
//! Fees [DOC https://www.bullish.com/global-derivatives]: "Options fees capped at 10% of premium",
//! "Zero expiry fees", maker rebates. The per-trade RATE is not public (account-tier based, no
//! public endpoint): the schedule uses the documented cap (`premiumCapRatio` = 0.10 from the API)
//! as an UPPER BOUND, which can overstate the real fee for near-the-money options.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;

pub const MAINNET_REST: &str = "https://api.exchange.bullish.com/trading-api";
pub const NETWORK: &str = "bullish-usdc-options";
pub const SPEC_VERSION: &str = "bullish-options/2026-10-06";
const FEE_SOURCE: &str = "https://www.bullish.com/global-derivatives (options fees capped at 10% of premium; per-trade rate not public)";

const P: PlatformId = PlatformId::Bullish;

pub struct BullishConnector {
    http: reqwest::Client,
    rest: String,
}

impl Default for BullishConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl BullishConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        get_json(&self.http, P, format!("{}{path}", self.rest), query).await
    }
}

fn ms_str(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    let ms = match v {
        Value::String(s) => s.parse::<i64>().ok(),
        other => other.as_i64(),
    }
    .ok_or_else(|| invalid(P, format!("{field}: expected ms timestamp, got {v}")))?;
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| invalid(P, format!("{field}: out of range")))
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["marketType"] != "OPTION"
        || v["underlyingBaseSymbol"] != underlying
        || v["settlementAssetSymbol"] != "USDC"
        || v["quoteSymbol"] != "USDC"
    {
        return Ok(None);
    }
    if v["marketEnabled"] != true || v["createOrderEnabled"] != true {
        return Ok(None);
    }
    let name = v["symbol"].as_str().ok_or_else(|| invalid(P, "symbol"))?;
    let kind = match v["optionType"].as_str() {
        Some("CALL") => OptionKind::Call,
        Some("PUT") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: optionType {other:?}"))),
    };
    let multiplier = dec(P, &v["contractMultiplier"], "contractMultiplier")?;
    // Prices were verified to be USDC per contract with multiplier 1 (= per BTC). Any other
    // multiplier would make the price convention unverified: refuse rather than guess.
    if multiplier != Decimal::ONE {
        return Err(invalid(
            P,
            format!("{name}: contractMultiplier {multiplier} (only 1 verified)"),
        ));
    }
    let expiry: DateTime<Utc> = v["expiryDatetime"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .ok_or_else(|| invalid(P, format!("{name}: expiryDatetime")))?
        .with_timezone(&Utc);
    if expiry <= now {
        return Ok(None);
    }
    let qty_precision = v["quantityPrecision"]
        .as_str()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| invalid(P, format!("{name}: quantityPrecision")))?;
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["optionStrikePrice"], "optionStrikePrice")?.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: multiplier,
        premium_currency: "USDC".into(),
        collateral_assets: vec!["USDC".into()],
        settlement_asset: Some("USDC".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("bullish:{underlying} index")),
        settlement_window: None,
        tick_size: opt_dec(P, v.get("tickSize"), "tickSize")?.map(|d| d.normalize()),
        min_amount: opt_dec(P, v.get("minQuantityLimit"), "minQuantityLimit")?,
        amount_step: Some(Decimal::new(1, qty_precision)),
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "marketId": v["marketId"], "premiumCapRatio": v["premiumCapRatio"],
            "feeGroupId": v["feeGroupId"], "maxQuantityLimit": v["maxQuantityLimit"],
            "venue_type": "centralised_exchange",
            "account_required": "user's own Bullish account + API keys (KYC, eligible jurisdictions)"
        }),
    }))
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v
        .as_array()
        .ok_or_else(|| invalid(P, "markets: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
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
            let price = dec(P, &l["price"], "level.price")?;
            let size = dec(P, &l["priceLevelQuantity"], "level.priceLevelQuantity")?;
            if price <= Decimal::ZERO || size <= Decimal::ZERO {
                return Err(invalid(P, format!("non-positive book level {l}")));
            }
            Ok(Level { price, size })
        })
        .collect()
}

/// `book` = orderbook/hybrid, `index` = index-prices/{asset}, `tick` = markets/{symbol}/tick.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    index: &Value,
    tick: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let id = instrument.key.native_id.as_str();
    if book["symbol"] != id || tick["symbol"] != id {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    if index["assetSymbol"] != instrument.underlying.as_str() {
        return Err(invalid(P, "index asset does not match the underlying"));
    }
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(book, "bids")?,
        asks: levels(book, "asks")?,
        source_ts: Some(ms_str(&book["timestamp"], "timestamp")?),
        received_at,
        index_price: positive(opt_dec(P, index.get("price"), "index.price")?),
        // Multiplier is 1 (enforced in parse_instrument): mark per BTC == mark per contract.
        mark_price: positive(opt_dec(P, tick.get("markPrice"), "tick.markPrice")?),
        provenance,
        access: BookAccess::Api,
    })
}

/// UPPER BOUND: the documented 10 % premium cap (`premiumCapRatio`), because the per-trade
/// rate is not published. Real fee = min(rate x notional, cap x premium) <= cap x premium.
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let cap = dec(P, &instrument.native["premiumCapRatio"], "premiumCapRatio")?;
    let f = FeeFormula::PctOfPremium { rate: cap };
    Ok(FeeSchedule {
        taker: f.clone(),
        maker: f,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "UPPER BOUND: 10 % of premium (documented cap, premiumCapRatio from the API). The per-trade rate is account-tier based and not public, so the real fee is <= this (often much lower near the money); maker rebates not modelled. Zero expiry fees. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for BullishConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    /// Keep-warm every 6 s: public rate limits trip at 1 s with several hot books (2026-10-06).
    fn hot_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(6)
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self.get("/v1/markets", &[("marketType", "OPTION")]).await?;
        parse_instruments(&v, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let id = &instrument.key.native_id;
        let (book_path, index_path, tick_path) = (
            format!("/v1/markets/{id}/orderbook/hybrid"),
            format!("/v1/index-prices/{}", instrument.underlying),
            format!("/v1/markets/{id}/tick"),
        );
        let (book, index, tick) = tokio::join!(
            self.get(&book_path, &[]),
            self.get(&index_path, &[]),
            self.get(&tick_path, &[]),
        );
        let (book, index, tick) = (book?, index?, tick?);
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book,
            &index,
            &tick,
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
    fn inst() -> Instrument {
        parse_instruments(&fixture("bullish_markets_option.json"), "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-USDC-20261008-86000-C")
            .unwrap()
    }

    #[test]
    fn parses_btc_usdc_options() {
        let v = fixture("bullish_markets_option.json");
        assert!(parse_instruments(&v, "ETH", at()).unwrap().is_empty());
        assert_eq!(parse_instruments(&v, "BTC", at()).unwrap().len(), 6);
        let i = inst();
        assert_eq!(i.strike, Some(dec!(86000)));
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-08T08:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.min_amount, Some(dec!(0.0005)));
        assert_eq!(i.amount_step, Some(dec!(0.00000001)));
        assert_eq!(i.tick_size, Some(dec!(10)));
        // Expired by the time of the snapshot -> dropped.
        let later: DateTime<Utc> = "2026-10-09T00:00:00Z".parse().unwrap();
        assert!(parse_instruments(&v, "BTC", later).unwrap().is_empty());
    }

    #[test]
    fn rejects_unverified_multiplier() {
        let mut v = fixture("bullish_markets_option.json")[0].clone();
        v["contractMultiplier"] = "0.1".into();
        assert!(parse_instrument(&v, "BTC", at()).is_err());
    }

    #[test]
    fn book_and_fees() {
        let i = inst();
        let b = parse_book(
            &i,
            &fixture("bullish_book.json"),
            &fixture("bullish_index_btc.json"),
            &fixture("bullish_tick.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(720),
                size: dec!(0.484)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(670),
                size: dec!(5.95)
            }
        );
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert!(b.index_price.unwrap() > dec!(80000));
        assert_eq!(b.mark_price, Some(dec!(710.7979)));
        let mut other = i.clone();
        other.key.native_id = "BTC-USDC-20261008-86000-P".into();
        assert!(
            parse_book(
                &other,
                &fixture("bullish_book.json"),
                &fixture("bullish_index_btc.json"),
                &fixture("bullish_tick.json"),
                at(),
                Provenance::Fixture
            )
            .is_err()
        );

        let f = fee_schedule(&i).unwrap();
        // Buy 0.484 BTC at 720 = 348.48 USDC premium -> upper-bound fee 10 % = 34.848.
        let fee = f.taker.compute(&oa_core::fees::FeeInputs {
            qty_native: dec!(0.484),
            qty_underlying: dec!(0.484),
            premium: dec!(348.48),
            index_price: b.index_price,
            mark_price: None,
        });
        assert_eq!(
            fee,
            oa_core::fees::FeeAmount::Known {
                amount: dec!(34.848)
            }
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_bullish_options() {
        let c = BullishConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "bullish {} instruments; {} bids {:?} asks {:?} index {:?} mark {:?}",
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
