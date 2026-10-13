//! Limitless (Base) — crypto "Up or Down" binaries, READ-ONLY.
//!
//! Sources (probed 2026-10-08, public, no key):
//! - `GET https://api.limitless.exchange/markets/active?limit=25&page=N` -> `data[]` {title "BTC Up or
//!   Down Daily|Hourly|15 Min|5 Min", slug, expirationTimestamp (ms), tradeType "clob",
//!   metadata.openPrice (price to beat), description (5/15-min: "Price to Beat … was $X")}.
//!   Resolves Up if the resolution price > price to beat (tie = 50/50) -> binary "above K" with
//!   K = price to beat, known once the window opened.
//! - `GET /markets/{slug}/orderbook` -> YES ("Up") token {bids, asks: [{price, size x1e6}]}.
//!
//! Fees: markets carry `metadata.fee = true` but no rate in the API -> UNKNOWN (premium shown as a
//! lower bound; never ranked). Volume < $2k/day on crypto (2026-10-07).

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

pub const API: &str = "https://api.limitless.exchange";
pub const NETWORK: &str = "limitless-base";
pub const SPEC_VERSION: &str = "limitless/2026-10-08";
const MARKETS_TTL: Duration = Duration::from_secs(30);
const MAX_PAGES: usize = 40;
const P: PlatformId = PlatformId::Limitless;

pub struct LimitlessConnector {
    http: reqwest::Client,
    api: String,
    markets: Mutex<Option<(Instant, Value)>>,
}

impl Default for LimitlessConnector {
    fn default() -> Self {
        Self::new(API)
    }
}

impl LimitlessConnector {
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
        let mut all = vec![];
        for page in 1..=MAX_PAGES {
            let r = read_json(
                P,
                self.http
                    .get(format!("{}/markets/active", self.api))
                    .query(&[("limit", "25".to_string()), ("page", page.to_string())])
                    .send()
                    .await,
            )
            .await?;
            let batch = r["data"].as_array().cloned().unwrap_or_default();
            let n = batch.len();
            all.extend(
                batch
                    .into_iter()
                    .filter(|m| up_down_underlying(m).is_some()),
            );
            if n < 25 {
                break;
            }
        }
        let v = serde_json::json!({ "data": all });
        *g = Some((Instant::now(), v.clone()));
        Ok(v)
    }
}

fn up_down_underlying(m: &Value) -> Option<String> {
    let t = m["title"].as_str()?;
    let (u, rest) = t.split_once(' ')?;
    rest.starts_with("Up or Down")
        .then(|| u.to_ascii_uppercase())
}

/// Price to beat: metadata.openPrice, else "… was $X" in the description (5/15-min markets).
pub fn price_to_beat(m: &Value) -> Option<Decimal> {
    if let Some(p) = m["metadata"]["openPrice"]
        .as_str()
        .and_then(|s| s.parse::<Decimal>().ok())
    {
        return Some(p.round_dp(8));
    }
    let d = m["description"].as_str()?;
    let i = d.find("Price to Beat captured")?;
    let after = &d[i..];
    let j = after.find("was $")? + 5;
    let num: String = after[j..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.trim_end_matches('.')
        .parse::<Decimal>()
        .ok()
        .map(|x| x.round_dp(8))
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
    for m in rows {
        if up_down_underlying(m).as_deref() != Some(underlying)
            || m["tradeType"].as_str() != Some("clob")
        {
            continue;
        }
        let (Some(slug), Some(ms), Some(strike)) = (
            m["slug"].as_str(),
            m["expirationTimestamp"].as_i64(),
            price_to_beat(m),
        ) else {
            continue;
        };
        let Some(expiry) = Utc.timestamp_millis_opt(ms).single() else {
            continue;
        };
        if expiry <= now || strike <= Decimal::ZERO {
            continue;
        }
        out.push(Instrument {
            key: InstrumentKey {
                platform: P,
                network: NETWORK.into(),
                native_id: slug.into(),
            },
            family: ProductFamily::Binary,
            underlying: underlying.into(),
            option_kind: Some(OptionKind::Call),
            strike: Some(strike),
            expiry: Some(expiry),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: Decimal::ONE,
            premium_currency: "USDC".into(),
            collateral_assets: vec!["USDC".into()],
            settlement_asset: Some("USDC".into()),
            settlement_mechanism: Some(SettlementMechanism::Cash),
            settlement_index: Some(format!(
                "limitless:{underlying} (Binance candle or Chainlink TWAP per market rules)"
            )),
            settlement_window: Some("market rules (description)".into()),
            tick_size: Some(Decimal::new(1, 3)),
            min_amount: None,
            amount_step: None,
            is_active: true,
            spec_version: SPEC_VERSION.into(),
            native: serde_json::json!({"title": m["title"], "tie": "50/50", "exchange": m["venue"]["exchange"], "yes_token": m["tokens"]["yes"]}),
        });
    }
    out.sort_by(|a, b| a.expiry.cmp(&b.expiry));
    Ok(out)
}

fn levels(v: &Value) -> Vec<Level> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let price = l["price"]
                .as_f64()
                .and_then(|x| Decimal::try_from(x).ok())?;
            let size = Decimal::from(l["size"].as_i64()?) / Decimal::from(1_000_000);
            Some(Level { price, size })
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
    let u = || FeeFormula::Unknown {
        reason: "Limitless markets flag fees (metadata.fee = true) but the API publishes no rate"
            .into(),
    };
    FeeSchedule {
        taker: u(),
        maker: u(),
        currency: instrument.premium_currency.clone(),
        source_url: "https://api.limitless.exchange/markets/active (read 2026-10-08)".into(),
        verified_on: "2026-10-08".into(),
        notes: "Fee rate unpublished: premium only (lower bound).".into(),
    }
}

#[async_trait]
impl Connector for LimitlessConnector {
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
                .get(format!(
                    "{}/markets/{}/orderbook",
                    self.api, instrument.key.native_id
                ))
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
        "2026-10-08T08:14:00Z".parse().unwrap()
    }

    #[test]
    fn up_down_markets_become_above_binaries_at_the_price_to_beat() {
        let v = load("limitless_markets.json");
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let daily = btc
            .iter()
            .find(|i| i.key.native_id == "btc-up-or-down-daily-p-1791378316052")
            .unwrap();
        assert_eq!(daily.strike, Some(dec!(83470.97)));
        assert_eq!(
            daily.expiry.unwrap().to_rfc3339(),
            "2026-10-08T16:00:00+00:00"
        );
        assert_eq!(daily.family, ProductFamily::Binary);
        // 15-min market: strike parsed from the description.
        let m15 = btc
            .iter()
            .find(|i| i.key.native_id == "btc-up-or-down-15-min-1791447300")
            .unwrap();
        assert_eq!(m15.strike, Some(dec!(82884.05414603)));
    }

    #[test]
    fn book_sizes_are_scaled_from_1e6() {
        let v = load("limitless_markets.json");
        let i = parse_instruments(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "btc-up-or-down-daily-p-1791378316052")
            .unwrap();
        let b = parse_book(
            &i,
            &load("limitless_orderbook_btc_daily.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(b.bids[0].price, dec!(0.282));
        assert_eq!(b.bids[0].size, dec!(116));
        assert_eq!(b.asks[0].price, dec!(0.358));
    }
}
