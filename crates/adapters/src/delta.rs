//! Delta Exchange (global, api.delta.exchange) — USDT-settled BTC/ETH options, public REST,
//! READ-ONLY. Centralised exchange (not onchain): trading needs the user's own Delta account
//! (KYC) and API keys. Delta Exchange INDIA (api.india.delta.exchange, INR/USD, Indian residents
//! only) is a different venue and is NOT used here.
//!
//! Verified live 2026-10-06:
//! - `GET /v2/products?contract_types=call_options,put_options&underlying_asset_symbols=BTC&states=live`
//!   (cursor pagination via `meta.after`): 232 BTC + 48 ETH options, settling asset USDT. Fields
//!   `symbol` (`C-BTC-86000-081026`), `contract_type`, `strike_price`, `settlement_time` (RFC 3339,
//!   12:00 UTC), `contract_value` (0.001 BTC / 0.01 ETH per contract), `tick_size` (USDT per 1
//!   underlying), `taker_commission_rate`/`maker_commission_rate` 0.00015,
//!   `product_specs.premium_commission_rate` 0.075, `spot_index` (.DEXBTUSDT / .DEETHUSDT).
//! - `GET /v2/l2orderbook/{symbol}`: `buy`/`sell` `{price, size}`, price = USDT per 1 UNDERLYING
//!   unit, size = contracts; `last_updated_at` (us). Level price here = `price x contract_value`.
//! - `GET /v2/tickers/{symbol}`: `spot_price` (index), `mark_price` (per underlying -> x contract_value).
//! - Cross-check: ATM 86000-C 8OCT26 717/763 USDT per BTC, mark 741 (Gate 709.8, Bullish 710.8:
//!   Delta settles 4 h later, 12:00 vs 08:00 UTC). Liquidity: small, OI ~1.3 M USD BTC + 0.6 M
//!   USD ETH, ~1-2 BTC at the touch near the money, wide spreads.
//!
//! Fees: shape [DOC https://www.delta.exchange/support/solutions/articles/80001177864]:
//! min(rate x notional, cap x premium), notional = spot x qty. The API fields carry the values:
//! on the India venue the same fields (0.0001 / 0.035) match that article exactly, which confirms
//! `premium_commission_rate` is the premium cap. Global values: 0.015 % notional, 7.5 % premium cap.
//! Exercise style: not stated in the docs read (left unknown); cash-settled in USDT.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;

pub const MAINNET_REST: &str = "https://api.delta.exchange";
pub const NETWORK: &str = "delta-global-usdt-options";
pub const SPEC_VERSION: &str = "delta-options/2026-10-06";
const BOOK_DEPTH: &str = "50";
const MAX_PAGES: usize = 20;
const FEE_SOURCE: &str = "https://www.delta.exchange/support/solutions/articles/80001177864-fees-on-options-and-futures-trading (min(rate x notional, cap x premium)); values from /v2/products";

const P: PlatformId = PlatformId::Delta;

pub struct DeltaConnector {
    http: reqwest::Client,
    rest: String,
}

impl Default for DeltaConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl DeltaConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
        }
    }

    /// Full answer (`{success, result, meta?}`); `success != true` is an error.
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let v = get_json(&self.http, P, format!("{}{path}", self.rest), query).await?;
        if v["success"] != true {
            return Err(invalid(P, format!("{path}: {}", v["error"])));
        }
        Ok(v)
    }
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    let kind = match v["contract_type"].as_str() {
        Some("call_options") => OptionKind::Call,
        Some("put_options") => OptionKind::Put,
        _ => return Ok(None),
    };
    if v["underlying_asset"]["symbol"] != underlying
        || v["settling_asset"]["symbol"] != "USDT"
        || v["notional_type"] != "vanilla"
        || v["state"] != "live"
        || v["trading_status"] != "operational"
    {
        return Ok(None);
    }
    let name = v["symbol"].as_str().ok_or_else(|| invalid(P, "symbol"))?;
    let expiry: DateTime<Utc> = v["settlement_time"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .ok_or_else(|| invalid(P, format!("{name}: settlement_time")))?
        .with_timezone(&Utc);
    if expiry <= now {
        return Ok(None);
    }
    let cv = dec(P, &v["contract_value"], "contract_value")?.normalize();
    if cv <= Decimal::ZERO || v["contract_unit_currency"] != underlying {
        return Err(invalid(
            P,
            format!("{name}: contract_value {cv} / unit currency"),
        ));
    }
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["strike_price"], "strike_price")?.normalize()),
        expiry: Some(expiry),
        exercise_style: None,
        contract_multiplier: cv,
        premium_currency: "USDT".into(),
        collateral_assets: vec!["USDT".into()],
        settlement_asset: Some("USDT".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!(
            "delta:{}",
            v["spot_index"]["symbol"].as_str().unwrap_or("index")
        )),
        settlement_window: None,
        // Native tick is per underlying unit; expressed per contract like Level prices.
        tick_size: opt_dec(P, v.get("tick_size"), "tick_size")?.map(|t| (t * cv).normalize()),
        min_amount: Some(Decimal::ONE),
        amount_step: Some(Decimal::ONE),
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "product_id": v["id"], "taker_commission_rate": v["taker_commission_rate"],
            "maker_commission_rate": v["maker_commission_rate"],
            "premium_commission_rate": v["product_specs"]["premium_commission_rate"],
            "contract_value": v["contract_value"], "tick_size_per_underlying": v["tick_size"],
            "position_size_limit": v["position_size_limit"],
            "venue_type": "centralised_exchange",
            "account_required": "user's own Delta Exchange (global) account + API keys (KYC)"
        }),
    }))
}

/// `v` = the `result` array of one /v2/products page.
pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v
        .as_array()
        .ok_or_else(|| invalid(P, "products: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn levels(v: &Value, side: &str, cv: Decimal) -> Result<Vec<Level>> {
    let arr = v[side]
        .as_array()
        .ok_or_else(|| invalid(P, format!("l2orderbook.{side} missing")))?;
    arr.iter()
        .map(|l| {
            let per_underlying = dec(P, &l["price"], "level.price")?;
            let size = dec(P, &l["size"], "level.size")?;
            if per_underlying <= Decimal::ZERO || size <= Decimal::ZERO {
                return Err(invalid(P, format!("non-positive book level {l}")));
            }
            Ok(Level {
                price: per_underlying * cv,
                size,
            })
        })
        .collect()
}

/// `book` / `ticker` = the `result` objects of l2orderbook and tickers/{symbol}.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    ticker: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let id = instrument.key.native_id.as_str();
    if book["symbol"] != id || ticker["symbol"] != id {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    let cv = instrument.contract_multiplier;
    if dec(P, &ticker["contract_value"], "contract_value")? != cv {
        return Err(invalid(P, "contract_value changed since discovery"));
    }
    let us = book["last_updated_at"]
        .as_i64()
        .ok_or_else(|| invalid(P, "last_updated_at: expected microseconds"))?;
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(book, "buy", cv)?,
        asks: levels(book, "sell", cv)?,
        source_ts: Utc.timestamp_micros(us).single(),
        received_at,
        index_price: positive(opt_dec(P, ticker.get("spot_price"), "spot_price")?),
        mark_price: positive(opt_dec(P, ticker.get("mark_price"), "mark_price")?).map(|x| x * cv),
        provenance,
        access: BookAccess::Api,
    })
}

/// min(commission rate x spot x qty, premium_commission_rate x premium).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let n = &instrument.native;
    let cap = dec(P, &n["premium_commission_rate"], "premium_commission_rate")?;
    let part = |field: &str| -> Result<FeeFormula> {
        Ok(FeeFormula::Min {
            parts: vec![
                FeeFormula::PctOfUnderlyingNotional {
                    rate: dec(P, &n[field], field)?,
                },
                FeeFormula::PctOfPremium { rate: cap },
            ],
        })
    };
    Ok(FeeSchedule {
        taker: part("taker_commission_rate")?,
        maker: part("maker_commission_rate")?,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Rates per product from the API (global: 0.015% of spot notional, cap 7.5% of premium = product_specs.premium_commission_rate; field meaning confirmed against the India help article). Settlement fees and fee tiers not modelled. Premium in USDT. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for DeltaConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let now = Utc::now();
        let mut out = vec![];
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut q = vec![
                ("contract_types", "call_options,put_options"),
                ("underlying_asset_symbols", underlying),
                ("states", "live"),
                ("page_size", "500"),
            ];
            if let Some(a) = after.as_deref() {
                q.push(("after", a));
            }
            let v = self.get("/v2/products", &q).await?;
            out.extend(parse_instruments(&v["result"], underlying, now)?);
            match v["meta"]["after"].as_str() {
                Some(a) if !a.is_empty() => after = Some(a.to_owned()),
                _ => return Ok(out),
            }
        }
        Err(invalid(
            P,
            format!("/v2/products: more than {MAX_PAGES} pages"),
        ))
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let id = &instrument.key.native_id;
        let (book_path, ticker_path) =
            (format!("/v2/l2orderbook/{id}"), format!("/v2/tickers/{id}"));
        let (book, ticker) = tokio::join!(
            self.get(&book_path, &[("depth", BOOK_DEPTH)]),
            self.get(&ticker_path, &[]),
        );
        let (book, ticker) = (book?, ticker?);
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book["result"],
            &ticker["result"],
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
        parse_instruments(&fixture("delta_products_btc.json")["result"], "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "C-BTC-86000-081026")
            .unwrap()
    }

    #[test]
    fn parses_products() {
        let v = &fixture("delta_products_btc.json")["result"];
        assert!(parse_instruments(v, "ETH", at()).unwrap().is_empty());
        assert_eq!(parse_instruments(v, "BTC", at()).unwrap().len(), 4);
        let i = inst();
        assert_eq!(i.strike, Some(dec!(86000)));
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-08T12:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(0.001));
        assert_eq!(i.tick_size, Some(dec!(0.0001)));
        assert_eq!(i.premium_currency, "USDT");
        assert_eq!(i.settlement_index.as_deref(), Some("delta:.DEXBTUSDT"));
    }

    #[test]
    fn book_prices_are_per_contract_and_fees() {
        let i = inst();
        let b = parse_book(
            &i,
            &fixture("delta_book.json")["result"],
            &fixture("delta_ticker.json")["result"],
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // 763 USDT per BTC x 0.001 BTC per contract = 0.763 USDT per contract, 1153 contracts.
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(0.763),
                size: dec!(1153)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(0.717),
                size: dec!(1730)
            }
        );
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(b.index_price, Some(dec!(86028.1)));
        assert_eq!(b.mark_price, Some(dec!(0.74103571662)));
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791281023523);

        let f = fee_schedule(&i).unwrap();
        let x = |premium| oa_core::fees::FeeInputs {
            qty_native: dec!(1000),
            qty_underlying: dec!(1),
            premium,
            index_price: Some(dec!(86000)),
            mark_price: None,
        };
        // 1 BTC at 763: min(0.00015 x 86,000 = 12.9, 7.5% x 763 = 57.225) = 12.9.
        assert_eq!(
            f.taker.compute(&x(dec!(763))),
            oa_core::fees::FeeAmount::Known { amount: dec!(12.9) }
        );
        // Cheap OTM: min(12.9, 7.5% x 100 = 7.5) = 7.5.
        assert_eq!(
            f.taker.compute(&x(dec!(100))),
            oa_core::fees::FeeAmount::Known { amount: dec!(7.5) }
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_delta_options() {
        let c = DeltaConnector::default();
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            assert!(!cat.is_empty());
            let i = &cat[cat.len() / 2];
            let md = c.get_market_data(i).await.unwrap();
            println!(
                "delta {u} {} instruments; {} bids {:?} asks {:?} index {:?} mark {:?}",
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
