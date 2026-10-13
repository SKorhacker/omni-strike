//! Hypercall (options on Hyperliquid) — READ-ONLY.
//!
//! Sources (probed 2026-10-07/08, public, no key):
//! - `GET https://api.hypercall.xyz/markets` -> `data[]` per (underlying, expiry): `index_price`,
//!   `instruments[]` {id "ETH-20261016-2950-P", strike, expiry (s), option_type, status, mark_iv,
//!   trading_mode "orderbook|rfq"}. Underlyings: crypto (BTC, ETH) and SP500, NVDA, MU, MSFT, SPCX…
//! - `GET /orderbook?instrument_name=X` -> JSON-RPC `result` {bids, asks: [[price USD, size]],
//!   mark_price, index_price, timestamp ms}. [LIVE] most books are EMPTY (0/40 two-sided sampled):
//!   prices form in an RFQ / pre-trade auction ("RPI") that needs an account.
//! - Fees: `/instruments` (BTC only) `taker_commission` = `maker_commission` = 0.0003.
//!   ASSUMPTION: % of underlying notional (Deribit convention the API mirrors), applied to every
//!   underlying; cap not documented -> none assumed (conservative).
//!
//! Settlement: cash in USDC at expiry [DOC docs/margining/settlement]; equities expire at the US
//! close (20:00 UTC), crypto at 08:00 UTC.

use crate::http::{client, read_json};
use crate::{Connector, Result, invalid};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const API: &str = "https://api.hypercall.xyz";
pub const NETWORK: &str = "hypercall-hyperliquid";
pub const SPEC_VERSION: &str = "hypercall/2026-10-08";
const MARKETS_TTL: Duration = Duration::from_secs(30);
const P: PlatformId = PlatformId::Hypercall;

pub struct HypercallConnector {
    http: reqwest::Client,
    api: String,
    markets: Mutex<Option<(Instant, Value)>>,
}

impl Default for HypercallConnector {
    fn default() -> Self {
        Self::new(API)
    }
}

impl HypercallConnector {
    pub fn new(api: &str) -> Self {
        Self {
            http: client(),
            api: api.trim_end_matches('/').into(),
            markets: Mutex::new(None),
        }
    }
    async fn markets(&self) -> Result<Value> {
        let mut g = self.markets.lock().await;
        if let Some((at, v)) = g.as_ref()
            && at.elapsed() < MARKETS_TTL
        {
            return Ok(v.clone());
        }
        let v = read_json(
            P,
            self.http.get(format!("{}/markets", self.api)).send().await,
        )
        .await?;
        *g = Some((Instant::now(), v.clone()));
        Ok(v)
    }
}

fn d(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64().and_then(|x| Decimal::try_from(x).ok()),
        _ => None,
    }
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let rows = v["data"]
        .as_array()
        .ok_or_else(|| invalid(P, "markets: data missing"))?;
    let mut out = vec![];
    for row in rows
        .iter()
        .filter(|r| r["underlying"].as_str() == Some(underlying))
    {
        for i in row["instruments"].as_array().into_iter().flatten() {
            let (Some(id), Some(strike), Some(ts)) =
                (i["id"].as_str(), d(&i["strike"]), i["expiry"].as_i64())
            else {
                continue;
            };
            let Some(expiry) = Utc.timestamp_opt(ts, 0).single() else {
                continue;
            };
            if expiry <= now || i["status"].as_str() != Some("ACTIVE") || strike <= Decimal::ZERO {
                continue;
            }
            let kind = match i["option_type"].as_str() {
                Some("call") => OptionKind::Call,
                Some("put") => OptionKind::Put,
                _ => continue,
            };
            out.push(Instrument {
                key: InstrumentKey { platform: P, network: NETWORK.into(), native_id: id.into() },
                family: ProductFamily::DatedOption,
                underlying: underlying.into(),
                option_kind: Some(kind),
                strike: Some(strike),
                expiry: Some(expiry),
                exercise_style: Some(ExerciseStyle::European),
                contract_multiplier: Decimal::ONE,
                premium_currency: "USDC".into(),
                collateral_assets: vec!["USDC".into()],
                settlement_asset: Some("USDC".into()),
                settlement_mechanism: Some(SettlementMechanism::Cash),
                settlement_index: Some(format!("hypercall:{underlying} index")),
                settlement_window: Some("at expiry".into()),
                tick_size: None,
                min_amount: None,
                amount_step: None,
                is_active: true,
                spec_version: SPEC_VERSION.into(),
                native: serde_json::json!({"instrument_id": i["instrument_id"], "trading_mode": i["trading_mode"]}),
            });
        }
    }
    out.sort_by(|a, b| a.key.native_id.cmp(&b.key.native_id));
    Ok(out)
}

fn levels(v: &Value) -> Vec<Level> {
    let mut out: Vec<Level> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(Level {
                price: d(&l[0])?,
                size: d(&l[1])?,
            })
        })
        .filter(|l| l.price > Decimal::ZERO && l.size > Decimal::ZERO)
        .collect();
    out.dedup_by(|a, b| {
        a.price == b.price && {
            b.size += a.size;
            true
        }
    });
    out
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let r = v.get("result").ok_or_else(|| {
        invalid(
            P,
            format!("orderbook {}: no result", instrument.key.native_id),
        )
    })?;
    let mut bids = levels(&r["bids"]);
    let mut asks = levels(&r["asks"]);
    bids.sort_by(|a, b| b.price.cmp(&a.price));
    asks.sort_by(|a, b| a.price.cmp(&b.price));
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids,
        asks,
        source_ts: r["timestamp"]
            .as_i64()
            .and_then(|t| Utc.timestamp_millis_opt(t).single()),
        received_at,
        index_price: d(&r["index_price"]).filter(|x| *x > Decimal::ZERO),
        mark_price: d(&r["mark_price"]).filter(|x| *x > Decimal::ZERO),
        provenance,
        access: BookAccess::Api,
    })
}

pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let f = || FeeFormula::PctOfUnderlyingNotional {
        rate: Decimal::new(3, 4),
    };
    FeeSchedule {
        taker: f(),
        maker: f(),
        currency: instrument.premium_currency.clone(),
        source_url:
            "https://api.hypercall.xyz/instruments (taker_commission 0.0003, read 2026-10-08)"
                .into(),
        verified_on: "2026-10-08".into(),
        notes:
            "ASSUMPTION: 0.03 % of underlying notional for every underlying, no cap (conservative)."
                .into(),
    }
}

#[async_trait]
impl Connector for HypercallConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        parse_instruments(&self.markets().await?, underlying, Utc::now())
    }
    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = Instant::now();
        let v = read_json(
            P,
            self.http
                .get(format!("{}/orderbook", self.api))
                .query(&[("instrument_name", instrument.key.native_id.as_str())])
                .send()
                .await,
        )
        .await?;
        let network = t0.elapsed();
        let t1 = Instant::now();
        let book = parse_book(instrument, &v, Utc::now(), Provenance::LiveData)?;
        Ok(crate::MarketData::rest(book, network, t1))
    }
    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        Ok(fee_schedule(instrument))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn load(name: &str) -> Value {
        crate::fixture("recorded-2026-10-08", name)
    }
    fn at() -> DateTime<Utc> {
        "2026-10-08T08:10:00Z".parse().unwrap()
    }

    #[test]
    fn catalog_covers_crypto_and_index_options() {
        let v = load("hypercall_markets.json");
        let eth = parse_instruments(&v, "ETH", at()).unwrap();
        let spx = parse_instruments(&v, "SP500", at()).unwrap();
        assert!(!eth.is_empty() && !spx.is_empty());
        let i = eth
            .iter()
            .find(|i| i.key.native_id == "ETH-20261016-2950-P")
            .unwrap();
        assert_eq!(i.strike, Some(dec!(2950)));
        assert_eq!(i.option_kind, Some(OptionKind::Put));
    }

    #[test]
    fn book_levels_are_usd_price_and_size() {
        let v = load("hypercall_markets.json");
        let i = parse_instruments(&v, "ETH", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "ETH-20261016-2950-P")
            .unwrap();
        let b = parse_book(
            &i,
            &load("hypercall_orderbook_eth_2950p.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(b.asks[0].price, dec!(396));
        assert!(b.bids.is_empty());
        assert_eq!(b.index_price, Some(dec!(2566.4)));
    }
}
