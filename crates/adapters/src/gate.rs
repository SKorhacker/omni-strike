//! Gate — USDT-settled BTC/ETH options, public REST (API v4), READ-ONLY. Centralised exchange (not
//! onchain): trading needs the user's own Gate account (KYC) and API keys.
//!
//! Verified live 2026-10-06:
//! - `GET /options/contracts?underlying=BTC_USDT`: 628 BTC + 606 ETH contracts (12 expiries,
//!   08:00 UTC). Fields `name` (`BTC_USDT-20261008-86000-C`), `is_call`, `strike_price`,
//!   `expiration_time` (s), `multiplier` "0.01" (underlying per contract), `order_price_round`
//!   (USDT per 1 underlying), `order_size_min` 1, `taker_fee_rate`/`maker_fee_rate` 0.0003,
//!   `price_limit_fee_rate` 0.125, `is_active`.
//! - `GET /options/order_book?contract=X`: `asks`/`bids` `{p, s}`, `p` = USDT per 1 UNDERLYING
//!   unit, `s` = number of contracts; `update` (s, float). Level price here = `p x multiplier`
//!   (premium per native contract), size = contracts.
//! - `GET /options/underlying/tickers/BTC_USDT` (`index_price`), `GET /options/contracts/{name}`
//!   (`mark_price`, per underlying unit -> x multiplier).
//! - Cross-check: ATM 86000-C 8OCT26 701/729 USDT per BTC, mark 709.8 (Coincall 708.8, Bullish
//!   710.8). Liquidity: OI 68,449 BTC contracts (684 BTC) + 379,110 ETH contracts (3,791 ETH);
//!   several BTC per level near the money.
//!
//! Rules [DOC https://www.gate.com/help/other/optionstradingrules/45829]: European-style, USDT
//! settlement, 0.01 BTC/ETH per contract. Trading fee = Min(rate x Index Price, 12.5 % x Option
//! Price) x Amount. Exercise fee (non-daily options only) = Min(0.015 % x Index, 12.5 % x payoff):
//! not modelled (paid at expiry, not at trade).
//!
//! Bulk top of book (verified live 2026-10-06): `GET /options/tickers?underlying=BTC_USDT` returns
//! every contract (628 BTC) with `name`, `bid1_price`/`bid1_size`/`ask1_price`/`ask1_size` (price
//! per 1 underlying unit like the order book `p`, size in contracts as a JSON integer; an empty
//! side is `"0"` / `0`), `mark_price` (per underlying unit), `index_price`. Tops matched the
//! `/options/order_book` top level exactly when fetched in the same instant. The tickers carry NO
//! timestamp: `source_ts` = the venue's `X-Out-Time` response header (µs, server send time).
//! Public options endpoints: 200 req / 10 s (`X-Gate-RateLimit-Limit` header).

use crate::http::{client, read_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAINNET_REST: &str = "https://api.gateio.ws/api/v4";
pub const NETWORK: &str = "gate-usdt-options";
pub const SPEC_VERSION: &str = "gate-options/2026-10-06";
const BOOK_DEPTH: &str = "50";
const FEE_SOURCE: &str = "https://www.gate.com/help/other/optionstradingrules/45829 (Min(rate x index, 12.5% x option price) x amount; rates from the contract API)";

const P: PlatformId = PlatformId::Gate;
/// Max age of the contract catalog reused by `bulk_top_of_book` (multipliers).
const CATALOG_TTL: Duration = Duration::from_secs(300);

/// Active contracts of one underlying by `name`.
type Catalog = Arc<HashMap<String, Instrument>>;

pub struct GateConnector {
    http: reqwest::Client,
    rest: String,
    /// underlying → (fetched at, catalog); refreshed by every `list_instruments`.
    catalogs: Mutex<HashMap<String, (Instant, Catalog)>>,
}

impl Default for GateConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl GateConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            catalogs: Mutex::new(HashMap::new()),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        Ok(self.get_timed(path, query).await?.0)
    }

    /// Body + the venue's `X-Out-Time` header (server send time, µs) when present.
    async fn get_timed(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(Value, Option<DateTime<Utc>>)> {
        let resp = self
            .http
            .get(format!("{}{path}", self.rest))
            .query(query)
            .send()
            .await;
        let out_time = resp.as_ref().ok().and_then(|r| {
            let us = r.headers().get("x-out-time")?.to_str().ok()?.parse().ok()?;
            Utc.timestamp_micros(us).single()
        });
        let v = read_json(P, resp).await?;
        if let Some(label) = v.get("label") {
            return Err(invalid(P, format!("{path}: {label} {}", v["message"])));
        }
        Ok((v, out_time))
    }

    /// Cached catalog (≤ `CATALOG_TTL` old), else a fresh `list_instruments`.
    async fn catalog(&self, underlying: &str) -> Result<Catalog> {
        if let Some((at, c)) = self.catalogs.lock().expect("catalog lock").get(underlying)
            && at.elapsed() < CATALOG_TTL
        {
            return Ok(c.clone());
        }
        self.list_instruments(underlying).await?;
        self.catalogs
            .lock()
            .expect("catalog lock")
            .get(underlying)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| invalid(P, "catalog not cached after listing"))
    }
}

/// Gate's underlying id for a canonical symbol.
fn gate_underlying(underlying: &str) -> String {
    format!("{underlying}_USDT")
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["underlying"] != gate_underlying(underlying).as_str() || v["is_active"] != true {
        return Ok(None);
    }
    let name = v["name"].as_str().ok_or_else(|| invalid(P, "name"))?;
    let kind = match v["is_call"].as_bool() {
        Some(true) => OptionKind::Call,
        Some(false) => OptionKind::Put,
        None => return Err(invalid(P, format!("{name}: is_call"))),
    };
    let secs = v["expiration_time"]
        .as_i64()
        .ok_or_else(|| invalid(P, format!("{name}: expiration_time")))?;
    let expiry = Utc
        .timestamp_opt(secs, 0)
        .single()
        .ok_or_else(|| invalid(P, format!("{name}: expiry out of range")))?;
    if expiry <= now {
        return Ok(None);
    }
    let multiplier = dec(P, &v["multiplier"], "multiplier")?;
    if multiplier <= Decimal::ZERO {
        return Err(invalid(P, format!("{name}: multiplier {multiplier}")));
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
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: multiplier,
        premium_currency: "USDT".into(),
        collateral_assets: vec!["USDT".into()],
        settlement_asset: Some("USDT".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("gate:{} index", gate_underlying(underlying))),
        settlement_window: None,
        // Native tick is per underlying unit; expressed per contract like Level prices.
        tick_size: opt_dec(P, v.get("order_price_round"), "order_price_round")?
            .map(|t| (t * multiplier).normalize()),
        min_amount: opt_dec(P, v.get("order_size_min"), "order_size_min")?,
        amount_step: Some(Decimal::ONE),
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "taker_fee_rate": v["taker_fee_rate"], "maker_fee_rate": v["maker_fee_rate"],
            "price_limit_fee_rate": v["price_limit_fee_rate"], "multiplier": v["multiplier"],
            "order_price_round": v["order_price_round"], "tag": v["tag"],
            "order_size_max": v["order_size_max"],
            "venue_type": "centralised_exchange",
            "account_required": "user's own Gate account + API keys (KYC)"
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
        .ok_or_else(|| invalid(P, "contracts: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn levels(v: &Value, side: &str, multiplier: Decimal) -> Result<Vec<Level>> {
    let arr = v[side]
        .as_array()
        .ok_or_else(|| invalid(P, format!("order_book.{side} missing")))?;
    arr.iter()
        .map(|l| {
            let per_underlying = dec(P, &l["p"], "level.p")?;
            let size = dec(P, &l["s"], "level.s")?;
            if per_underlying <= Decimal::ZERO || size <= Decimal::ZERO {
                return Err(invalid(P, format!("non-positive book level {l}")));
            }
            Ok(Level {
                price: per_underlying * multiplier,
                size,
            })
        })
        .collect()
}

/// `book` = order_book, `und` = underlying ticker (index), `contract` = contracts/{name} (mark).
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    und: &Value,
    contract: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    // The order book response does not echo the contract: check the companion contract call.
    if contract["name"] != instrument.key.native_id.as_str() {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    let m = instrument.contract_multiplier;
    if dec(P, &contract["multiplier"], "multiplier")? != m {
        return Err(invalid(P, "contract multiplier changed since discovery"));
    }
    let ts = dec(P, &book["update"], "update")?;
    let ts_ms = (ts * Decimal::ONE_THOUSAND)
        .trunc()
        .to_string()
        .parse::<i64>()
        .map_err(|_| invalid(P, "update: out of range"))?;
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(book, "bids", m)?,
        asks: levels(book, "asks", m)?,
        source_ts: Utc.timestamp_millis_opt(ts_ms).single(),
        received_at,
        index_price: positive(opt_dec(P, und.get("index_price"), "index_price")?),
        mark_price: positive(opt_dec(P, contract.get("mark_price"), "mark_price")?).map(|x| x * m),
        provenance,
        access: BookAccess::Api,
    })
}

/// One side of a `/options/tickers` row as an order-book side: `[{p, s}]`, or `[]` when the side
/// is empty (price or size 0). Never invents a level.
fn ticker_side(t: &Value, px: &str, sz: &str) -> Result<Value> {
    let p = opt_dec(P, t.get(px), px)?;
    let s = opt_dec(P, t.get(sz), sz)?;
    Ok(match (p, s) {
        (Some(p), Some(s)) if p > Decimal::ZERO && s > Decimal::ZERO => {
            serde_json::json!([{ "p": t[px], "s": t[sz] }])
        }
        _ => serde_json::json!([]),
    })
}

/// `tickers` = `/options/tickers?underlying=X` array. Contracts absent from `catalog` (inactive,
/// expired, other underlying) are skipped. Levels go through the same `levels` scaling as
/// `parse_book` (price per underlying x multiplier, size in contracts); mark x multiplier; index as is.
pub fn parse_bulk_tops(
    tickers: &Value,
    catalog: &HashMap<String, Instrument>,
    source_ts: Option<DateTime<Utc>>,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<Vec<BookSnapshot>> {
    let arr = tickers
        .as_array()
        .ok_or_else(|| invalid(P, "tickers: not an array"))?;
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let Some(instrument) = t["name"].as_str().and_then(|n| catalog.get(n)) else {
            continue;
        };
        if instrument.expiry.is_some_and(|e| e <= received_at) {
            continue;
        }
        let m = instrument.contract_multiplier;
        let book = serde_json::json!({
            "bids": ticker_side(t, "bid1_price", "bid1_size")?,
            "asks": ticker_side(t, "ask1_price", "ask1_size")?,
        });
        out.push(BookSnapshot {
            instrument: instrument.key.clone(),
            bids: levels(&book, "bids", m)?,
            asks: levels(&book, "asks", m)?,
            source_ts,
            received_at,
            index_price: positive(opt_dec(P, t.get("index_price"), "index_price")?),
            mark_price: positive(opt_dec(P, t.get("mark_price"), "mark_price")?).map(|x| x * m),
            provenance,
            access: BookAccess::Api,
        });
    }
    Ok(out)
}

/// Min(rate x underlying notional, 12.5 % x premium), rates from the contract (documented shape).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let n = &instrument.native;
    let cap = dec(P, &n["price_limit_fee_rate"], "price_limit_fee_rate")?;
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
        taker: part("taker_fee_rate")?,
        maker: part("maker_fee_rate")?,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Rates from the contract API (0.0003; the help page's VIP0 table shows 0.028% taker, so this is the conservative value); 12.5 % premium cap = price_limit_fee_rate. VIP tiers not modelled. Exercise fee 0.015% (non-daily options, capped 12.5% of payoff) charged at expiry, not modelled. Premium in USDT. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for GateConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self
            .get(
                "/options/contracts",
                &[("underlying", &gate_underlying(underlying))],
            )
            .await?;
        let out = parse_instruments(&v, underlying, Utc::now())?;
        let by_id = out
            .iter()
            .map(|i| (i.key.native_id.clone(), i.clone()))
            .collect();
        self.catalogs
            .lock()
            .expect("catalog lock")
            .insert(underlying.into(), (Instant::now(), Arc::new(by_id)));
        Ok(out)
    }

    /// One `/options/tickers` request per underlying (contract catalog cached 5 min).
    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        let u = gate_underlying(underlying);
        let q = [("underlying", u.as_str())];
        let (tickers, catalog) = tokio::join!(
            self.get_timed("/options/tickers", &q),
            self.catalog(underlying)
        );
        let received_at = Utc::now();
        let ((tickers, source_ts), catalog) = (tickers?, catalog?);
        parse_bulk_tops(
            &tickers,
            &catalog,
            source_ts,
            received_at,
            Provenance::LiveData,
        )
        .map(Some)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let id = instrument.key.native_id.as_str();
        let und_path = format!(
            "/options/underlying/tickers/{}",
            gate_underlying(&instrument.underlying)
        );
        let contract_path = format!("/options/contracts/{id}");
        let book_query = [("contract", id), ("limit", BOOK_DEPTH), ("with_id", "true")];
        let (book, und, contract) = tokio::join!(
            self.get("/options/order_book", &book_query),
            self.get(&und_path, &[]),
            self.get(&contract_path, &[]),
        );
        let (book, und, contract) = (book?, und?, contract?);
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book,
            &und,
            &contract,
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
        parse_instruments(&fixture("gate_contracts_btc.json"), "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC_USDT-20261008-86000-C")
            .unwrap()
    }

    #[test]
    fn parses_contracts() {
        let v = fixture("gate_contracts_btc.json");
        assert!(parse_instruments(&v, "ETH", at()).unwrap().is_empty());
        assert_eq!(parse_instruments(&v, "BTC", at()).unwrap().len(), 6);
        let i = inst();
        assert_eq!(i.strike, Some(dec!(86000)));
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-08T08:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(0.01));
        assert_eq!(i.tick_size, Some(dec!(0.01)));
        assert_eq!(i.min_amount, Some(dec!(1)));
        assert_eq!(i.premium_currency, "USDT");
    }

    #[test]
    fn book_prices_are_per_contract_and_fees() {
        let i = inst();
        let b = parse_book(
            &i,
            &fixture("gate_book.json"),
            &fixture("gate_underlying_ticker.json"),
            &fixture("gate_contract.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // 729 USDT per BTC x 0.01 BTC per contract = 7.29 USDT per contract, 784 contracts.
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(7.29),
                size: dec!(784)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(7.01),
                size: dec!(142)
            }
        );
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(b.index_price, Some(dec!(86045.03)));
        assert_eq!(b.mark_price, Some(dec!(7.098)));
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791281019954);

        let f = fee_schedule(&i).unwrap();
        let x = |premium| oa_core::fees::FeeInputs {
            qty_native: dec!(30),
            qty_underlying: dec!(0.3),
            premium,
            index_price: Some(dec!(102000)),
            mark_price: None,
        };
        // Gate's own example: Min(0.03% x 102,000, 12.5% x 200) x 0.3 = 7.5 USDT.
        assert_eq!(
            f.taker.compute(&x(dec!(60))),
            oa_core::fees::FeeAmount::Known { amount: dec!(7.5) }
        );
        // Notional leg binds for a rich premium: 0.0003 x 0.3 x 102,000 = 9.18.
        assert_eq!(
            f.taker.compute(&x(dec!(600))),
            oa_core::fees::FeeAmount::Known { amount: dec!(9.18) }
        );
    }

    /// Tickers and order book recorded in the same instant (their tops agreed).
    #[test]
    fn bulk_tops_equal_full_book_tops() {
        let cat: HashMap<String, Instrument> =
            parse_instruments(&fixture("gate_contracts_btc.json"), "BTC", at())
                .unwrap()
                .into_iter()
                .map(|i| (i.key.native_id.clone(), i))
                .collect();
        let ts = Utc.timestamp_micros(1791287295558176).single();
        let tops = parse_bulk_tops(
            &fixture("gate_bulk_tickers_btc.json"),
            &cat,
            ts,
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(tops.len(), 6);
        let i = inst();
        let b = tops.iter().find(|b| b.instrument == i.key).unwrap();
        let full = parse_book(
            &i,
            &fixture("gate_bulk_book.json"),
            &fixture("gate_underlying_ticker.json"),
            &fixture("gate_contract.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!((b.bids.len(), b.asks.len()), (1, 1));
        assert_eq!(b.asks[0], full.asks[0]);
        assert_eq!(b.bids[0], full.bids[0]);
        // Worked example: ask 909 USDT per BTC x 0.01 = 9.09 USDT per contract, 257 contracts;
        // bid 881 x 0.01 = 8.81, 494 contracts; mark 898.7 x 0.01 = 8.987.
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(9.09),
                size: dec!(257)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(8.81),
                size: dec!(494)
            }
        );
        assert_eq!(b.mark_price, Some(dec!(8.987)));
        assert_eq!(b.index_price, Some(dec!(86316.36)));
        assert_eq!(b.source_ts, ts);
        assert_eq!(b.access, BookAccess::Api);

        // Empty bid ("0" / 0) → no level; unknown contract → skipped.
        let one = serde_json::json!([
            {"name": "BTC_USDT-20261008-86000-C", "bid1_price": "0", "bid1_size": 0,
             "ask1_price": "32", "ask1_size": 180, "mark_price": "30", "index_price": "86000"},
            {"name": "BTC_USDT-20991231-1-C", "bid1_price": "1", "bid1_size": 1,
             "ask1_price": "2", "ask1_size": 1}
        ]);
        let tops = parse_bulk_tops(&one, &cat, None, at(), Provenance::Fixture).unwrap();
        assert_eq!(tops.len(), 1);
        assert!(tops[0].bids.is_empty());
        assert_eq!(
            tops[0].asks,
            vec![Level {
                price: dec!(0.32),
                size: dec!(180)
            }]
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_gate_bulk_tops_vs_full_book() {
        let c = GateConnector::default();
        for u in ["BTC", "ETH"] {
            let tops = c.bulk_top_of_book(u).await.unwrap().unwrap();
            let two: Vec<_> = tops
                .iter()
                .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
                .collect();
            println!(
                "gate {u} bulk: {} instruments, {} two-sided",
                tops.len(),
                two.len()
            );
            let b = two[two.len() / 2];
            assert!(b.source_ts.is_some());
            let cat = c.catalog(u).await.unwrap();
            let full = c
                .get_market_data(&cat[&b.instrument.native_id])
                .await
                .unwrap()
                .book;
            println!(
                "gate {} bulk bid {:?} ask {:?} mark {:?} | full bid {:?} ask {:?} mark {:?}",
                b.instrument.native_id,
                b.bids.first(),
                b.asks.first(),
                b.mark_price,
                full.bids.first(),
                full.asks.first(),
                full.mark_price
            );
            // Same units: tops within 10 % of each other (the book moves between the two calls).
            let near = |a: Decimal, f: Decimal| (a - f).abs() <= f * dec!(0.1);
            assert!(near(b.asks[0].price, full.asks[0].price));
            assert!(near(b.bids[0].price, full.bids[0].price));
        }
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_gate_options() {
        let c = GateConnector::default();
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            assert!(!cat.is_empty());
            let i = &cat[cat.len() / 2];
            let md = c.get_market_data(i).await.unwrap();
            println!(
                "gate {u} {} instruments; {} bids {:?} asks {:?} index {:?} mark {:?}",
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
