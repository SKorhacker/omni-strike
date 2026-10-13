//! Myriad — crypto price binaries ("BTC above $K on <date>, 4PM UTC?"), READ-ONLY.
//!
//! Sources (probed 2026-10-08, public, no key):
//! - `GET https://api-v2.myriadprotocol.com/events?limit=100&state=open` -> `data[]` events
//!   ("BTC price on October 11, 4PM UTC") with `markets[]` {id, networkId, title "BTC above $82,000
//!   on October 11, 4PM UTC?", expiresAt, tradingModel "ob", token USD1 (18 decimals), fees
//!   {taker_fee_bps_array by price cent: 3 .. 180 (at 0.50) .. 1, maker 0}}.
//! - `GET /markets/{id}/orderbook?network_id=56` -> YES-outcome {bids, asks: [[price x1e18,
//!   shares x1e18]]}; one share pays 1 USD1 if YES.
//!
//! Only the BNB Chain order-book markets (networkId 56, real USD1) are used; AMM markets on other
//! networks (some in points) are skipped.
//! Mapping: family Binary, option_kind Call (= above), quantity = payout in USD1.
//! Fee: taker bps of the premium peaks at 180 (price 0.50) -> 1.80 % of premium used as a
//! conservative ceiling (exact rate depends on the fill price).

use crate::http::{client, read_json};
use crate::{Connector, Result, invalid};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const API: &str = "https://api-v2.myriadprotocol.com";
pub const NETWORK: &str = "myriad-bnb";
pub const SPEC_VERSION: &str = "myriad-v2/2026-10-08";
const BOOK_NETWORK_ID: i64 = 56;
const EVENTS_TTL: Duration = Duration::from_secs(30);
const P: PlatformId = PlatformId::Myriad;

pub struct MyriadConnector {
    http: reqwest::Client,
    api: String,
    events: Mutex<Option<(Instant, Value)>>,
}

impl Default for MyriadConnector {
    fn default() -> Self {
        Self::new(API)
    }
}

impl MyriadConnector {
    pub fn new(api: &str) -> Self {
        Self {
            http: client(),
            api: api.trim_end_matches('/').into(),
            events: Mutex::new(None),
        }
    }
    async fn events(&self) -> Result<Value> {
        let mut g = self.events.lock().await;
        if let Some((at, v)) = g.as_ref()
            && at.elapsed() < EVENTS_TTL
        {
            return Ok(v.clone());
        }
        let v = read_json(
            P,
            self.http
                .get(format!("{}/events", self.api))
                .query(&[("limit", "100"), ("state", "open")])
                .send()
                .await,
        )
        .await?;
        *g = Some((Instant::now(), v.clone()));
        Ok(v)
    }
}

/// "BTC above $82,000 on October 11, 4PM UTC?" -> (BTC, 82000).
pub fn parse_title(title: &str) -> Option<(String, Decimal)> {
    let mut words = title.split_whitespace();
    let underlying = words.next()?.to_ascii_uppercase();
    if !words.next()?.eq_ignore_ascii_case("above") {
        return None;
    }
    let k: String = words
        .next()?
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    Some((underlying, k.parse().ok()?))
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let events = v["data"]
        .as_array()
        .ok_or_else(|| invalid(P, "events: data missing"))?;
    let mut out = vec![];
    for m in events
        .iter()
        .flat_map(|e| e["markets"].as_array().into_iter().flatten())
    {
        if m["networkId"].as_i64() != Some(BOOK_NETWORK_ID)
            || m["tradingModel"].as_str() != Some("ob")
        {
            continue;
        }
        let Some((u, strike)) = m["title"].as_str().and_then(parse_title) else {
            continue;
        };
        let expiry = m["expiresAt"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let (Some(id), Some(expiry)) = (m["id"].as_i64(), expiry) else {
            continue;
        };
        if u != underlying || expiry <= now || m["state"].as_str() != Some("open") {
            continue;
        }
        out.push(Instrument {
            key: InstrumentKey { platform: P, network: NETWORK.into(), native_id: id.to_string() },
            family: ProductFamily::Binary,
            underlying: underlying.into(),
            option_kind: Some(OptionKind::Call),
            strike: Some(strike),
            expiry: Some(expiry),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: Decimal::ONE,
            premium_currency: "USD1".into(),
            collateral_assets: vec!["USD1".into()],
            settlement_asset: Some("USD1".into()),
            settlement_mechanism: Some(SettlementMechanism::Cash),
            settlement_index: Some(format!("myriad:{underlying} resolution source")),
            settlement_window: Some("at the stated time (4PM UTC)".into()),
            tick_size: Some(Decimal::new(1, 2)),
            min_amount: None,
            amount_step: None,
            is_active: true,
            spec_version: SPEC_VERSION.into(),
            native: serde_json::json!({"title": m["title"], "slug": m["slug"], "network_id": BOOK_NETWORK_ID}),
        });
    }
    out.sort_by(|a, b| (a.expiry, a.strike).cmp(&(b.expiry, b.strike)));
    Ok(out)
}

fn e18(v: &Value) -> Option<Decimal> {
    let s = v.as_str()?;
    let n: i128 = s.parse().ok()?;
    // 1e18 fixed point; keep 8 decimals.
    Some(Decimal::from_i128_with_scale(n / 10_000_000_000, 8).normalize())
}

fn levels(v: &Value) -> Vec<Level> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            Some(Level {
                price: e18(&l[0])?,
                size: e18(&l[1])?,
            })
        })
        .filter(|l| l.price > Decimal::ZERO && l.price < Decimal::ONE && l.size > Decimal::ZERO)
        .collect()
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let mut bids = levels(&v["bids"]);
    let mut asks = levels(&v["asks"]);
    bids.sort_by(|a, b| b.price.cmp(&a.price));
    asks.sort_by(|a, b| a.price.cmp(&b.price));
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids,
        asks,
        source_ts: None,
        received_at,
        index_price: None,
        mark_price: None,
        provenance,
        access: BookAccess::Api,
    })
}

pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    FeeSchedule {
        taker: FeeFormula::PctOfPremium { rate: Decimal::new(18, 3) },
        maker: FeeFormula::PctOfPremium { rate: Decimal::ZERO },
        currency: instrument.premium_currency.clone(),
        source_url: "https://api-v2.myriadprotocol.com/events (markets[].fees, read 2026-10-08)".into(),
        verified_on: "2026-10-08".into(),
        notes: "Taker fee is 3-180 bps of the premium depending on price (peak 180 at 0.50); 1.80 % used as a conservative ceiling. Maker 0.".into(),
    }
}

#[async_trait]
impl Connector for MyriadConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        parse_instruments(&self.events().await?, underlying, Utc::now())
    }
    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = Instant::now();
        let v = read_json(
            P,
            self.http
                .get(format!(
                    "{}/markets/{}/orderbook",
                    self.api, instrument.key.native_id
                ))
                .query(&[("network_id", BOOK_NETWORK_ID.to_string())])
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
        "2026-10-08T08:15:00Z".parse().unwrap()
    }

    #[test]
    fn titles_parse_to_underlying_and_strike() {
        assert_eq!(
            parse_title("BTC above $82,000 on October 11, 4PM UTC?"),
            Some(("BTC".into(), dec!(82000)))
        );
        assert_eq!(
            parse_title("XRP above $2.5 on October 12, 4PM UTC?"),
            Some(("XRP".into(), dec!(2.5)))
        );
        assert_eq!(parse_title("Bitcoin ATH before September 30, 2026?"), None);
    }

    #[test]
    fn ladder_markets_become_above_binaries() {
        let v = load("myriad_events.json");
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let i = btc.iter().find(|i| i.key.native_id == "3522").unwrap();
        assert_eq!(i.family, ProductFamily::Binary);
        assert_eq!(i.strike, Some(dec!(82000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-11T16:00:00+00:00");
    }

    #[test]
    fn book_is_scaled_from_1e18() {
        let v = load("myriad_events.json");
        let i = parse_instruments(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "3522")
            .unwrap();
        let b = parse_book(
            &i,
            &load("myriad_orderbook_btc82000.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(b.bids[0].price, dec!(0.65));
        assert_eq!(b.asks[0].price, dec!(0.69));
        assert!(b.bids[0].size > dec!(17) && b.bids[0].size < dec!(18));
    }
}
