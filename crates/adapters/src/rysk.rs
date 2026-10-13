//! Rysk V12 (HyperEVM / Ethereum) — READ-ONLY, INDICATIVE.
//!
//! Source (probed 2026-10-07/08): `GET https://v12.rysk.finance/api/inventory` -> per underlying
//! `combinations["<strike>-<expiry ts>"]` = {expiry "16OCT26", expiration_timestamp (s, 08:00 UTC),
//! timeToExpiryDays, strike, isPut, apy (%), index, timestamp, bid=0, ask=0}.
//! Product [DOC docs.rysk.finance]: users SELL covered calls / cash-secured puts into a 2-second RFQ
//! auction among whitelisted makers; European, fully collateralised, PHYSICALLY settled. The
//! inventory only carries an indicative APY on the collateral; firm prices need the taker RFQ
//! (wallet) which is not integrated.
//!
//! Normalisation: one BID level per contract = indicative premium per underlying unit
//!   call: apy/100 x index x days/365 (collateral = 1 underlying)
//!   put : apy/100 x strike x days/365 (collateral = strike in USDT)
//! (matches the venue UI on samples: BTC 83000P 55.4 % ~ $1,089). No size is published: the level
//! carries a nominal size and the book is tagged `BookAccess::IndicativeRfq` -> shown for price
//! discovery, never ranked or routed (Ineligibility::IndicativeRfqOnly).

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

pub const INVENTORY_URL: &str = "https://v12.rysk.finance/api/inventory";
pub const NETWORK: &str = "rysk-v12-hyperevm";
pub const SPEC_VERSION: &str = "rysk-v12/2026-10-08";
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);
/// No size is published; nominal size so the indicative price is visible (never routed).
const NOMINAL_SIZE: Decimal = Decimal::from_parts(1_000_000, 0, 0, false, 0);
const P: PlatformId = PlatformId::Rysk;

pub struct RyskConnector {
    http: reqwest::Client,
    url: String,
    snapshot: Mutex<Option<(Instant, Value)>>,
}

impl Default for RyskConnector {
    fn default() -> Self {
        Self::new(INVENTORY_URL)
    }
}

impl RyskConnector {
    pub fn new(url: &str) -> Self {
        Self {
            http: client(),
            url: url.into(),
            snapshot: Mutex::new(None),
        }
    }

    async fn inventory(&self) -> Result<(Value, Duration)> {
        let mut g = self.snapshot.lock().await;
        if let Some((at, v)) = g.as_ref()
            && at.elapsed() < SNAPSHOT_TTL
        {
            return Ok((v.clone(), Duration::ZERO));
        }
        let t0 = Instant::now();
        let v = read_json(P, self.http.get(&self.url).send().await).await?;
        *g = Some((Instant::now(), v.clone()));
        Ok((v, t0.elapsed()))
    }
}

fn dec(v: &Value, f: &str) -> Result<Decimal> {
    v.as_f64()
        .and_then(|x| Decimal::try_from(x).ok())
        .ok_or_else(|| invalid(P, format!("{f}: not a number")))
}

pub fn native_id(underlying: &str, expiry: &str, strike: Decimal, is_put: bool) -> String {
    format!(
        "{underlying}-{expiry}-{}-{}",
        strike.normalize(),
        if is_put { "P" } else { "C" }
    )
}

struct Combo {
    native_id: String,
    is_put: bool,
    strike: Decimal,
    expiry: DateTime<Utc>,
    premium: Decimal,
    index: Decimal,
    ts: Option<DateTime<Utc>>,
}

fn combos(v: &Value, underlying: &str, now: DateTime<Utc>) -> Result<Vec<Combo>> {
    let Some(map) = v[underlying]["combinations"].as_object() else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for c in map.values() {
        let strike = dec(&c["strike"], "strike")?;
        let ts = c["expiration_timestamp"]
            .as_i64()
            .ok_or_else(|| invalid(P, "expiration_timestamp"))?;
        let expiry = Utc
            .timestamp_opt(ts, 0)
            .single()
            .ok_or_else(|| invalid(P, "expiry"))?;
        if expiry <= now {
            continue;
        }
        let is_put = c["isPut"] == true;
        let apy = dec(&c["apy"], "apy")?;
        let index = dec(&c["index"], "index")?;
        let days = Decimal::from((expiry - now).num_seconds().max(0)) / Decimal::from(86_400);
        let collateral = if is_put { strike } else { index };
        let premium =
            (apy / Decimal::from(100) * collateral * days / Decimal::from(365)).round_dp(6);
        if premium <= Decimal::ZERO || strike <= Decimal::ZERO {
            continue;
        }
        let label = c["expiry"].as_str().unwrap_or_default();
        out.push(Combo {
            native_id: native_id(underlying, label, strike, is_put),
            is_put,
            strike,
            expiry,
            premium,
            index,
            ts: c["timestamp"]
                .as_i64()
                .and_then(|t| Utc.timestamp_opt(t, 0).single()),
        });
    }
    out.sort_by(|a, b| a.native_id.cmp(&b.native_id));
    Ok(out)
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    Ok(combos(v, underlying, now)?
        .into_iter()
        .map(|c| Instrument {
            key: InstrumentKey { platform: P, network: NETWORK.into(), native_id: c.native_id.clone() },
            family: ProductFamily::DatedOption,
            underlying: underlying.into(),
            option_kind: Some(if c.is_put { OptionKind::Put } else { OptionKind::Call }),
            strike: Some(c.strike),
            expiry: Some(c.expiry),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: Decimal::ONE,
            premium_currency: "USDT".into(),
            collateral_assets: vec![if c.is_put { "USDT".into() } else { underlying.to_string() }],
            settlement_asset: Some(if c.is_put { underlying.to_string() } else { "USDT".into() }),
            settlement_mechanism: Some(SettlementMechanism::Physical),
            settlement_index: Some(format!("rysk:{underlying}/USD")),
            settlement_window: Some("at expiry (08:00 UTC)".into()),
            tick_size: None,
            min_amount: None,
            amount_step: None,
            is_active: true,
            spec_version: SPEC_VERSION.into(),
            native: serde_json::json!({"rysk_key": format!("{}-{}", c.strike, c.expiry.timestamp()), "side_available": "sell only (users sell to makers)"}),
        })
        .collect())
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let c = combos(v, &instrument.underlying, received_at)?
        .into_iter()
        .find(|c| c.native_id == instrument.key.native_id)
        .ok_or_else(|| invalid(P, format!("{} no longer offered", instrument.key.native_id)))?;
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: vec![Level {
            price: c.premium,
            size: NOMINAL_SIZE,
        }],
        asks: vec![],
        source_ts: c.ts,
        received_at,
        index_price: Some(c.index),
        mark_price: None,
        provenance,
        access: BookAccess::IndicativeRfq,
    })
}

/// MIN(1 % x underlying price, 12.5 % x option price) x quantity [DOC makers-how-to-integrate-in-the-rfq].
pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let f = || FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional {
                rate: Decimal::new(1, 2),
            },
            FeeFormula::PctOfPremium {
                rate: Decimal::new(125, 3),
            },
        ],
    };
    FeeSchedule {
        taker: f(),
        maker: f(),
        currency: instrument.premium_currency.clone(),
        source_url: "https://docs.rysk.finance/getting-started/protocol-and-product/makers-how-to-integrate-in-the-rfq (read 2026-10-08)".into(),
        verified_on: "2026-10-08".into(),
        notes: "Indicative APY only; firm prices = taker RFQ over WebSocket, which requires Rysk whitelisting (call + 2 days on testnet) — partnership item.".into(),
    }
}

#[async_trait]
impl Connector for RyskConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let (v, _) = self.inventory().await?;
        parse_instruments(&v, underlying, Utc::now())
    }
    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let (v, network) = self.inventory().await?;
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

    fn fixture() -> Value {
        crate::fixture("recorded-2026-10-08", "rysk_inventory.json")
    }
    fn at() -> DateTime<Utc> {
        "2026-10-08T08:00:00Z".parse().unwrap()
    }

    #[test]
    fn instruments_cover_calls_and_puts_with_physical_settlement() {
        let v = fixture();
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        assert!(btc.len() > 30);
        assert!(btc.iter().any(|i| i.option_kind == Some(OptionKind::Put)));
        let i = btc
            .iter()
            .find(|i| i.key.native_id == "BTC-27NOV26-102000-C")
            .unwrap();
        assert_eq!(i.strike, Some(dec!(102000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-11-27T08:00:00+00:00");
        assert_eq!(i.settlement_mechanism, Some(SettlementMechanism::Physical));
        assert!(
            parse_instruments(&v, "XRP", at()).unwrap().is_empty(),
            "XRP listed without expiries"
        );
    }

    #[test]
    fn book_is_one_indicative_bid_from_apy() {
        let v = fixture();
        let i = parse_instruments(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-27NOV26-102000-C")
            .unwrap();
        let b = parse_book(&i, &v, at(), Provenance::Fixture).unwrap();
        assert!(b.asks.is_empty());
        assert_eq!(b.access, BookAccess::IndicativeRfq);
        // call: apy 3.6817 % x index 82951.81 x 50 days / 365
        let expected = dec!(3.681696989393816) / dec!(100) * dec!(82951.81) * dec!(50) / dec!(365);
        assert!(
            (b.bids[0].price - expected).abs() < dec!(0.01),
            "{} vs {expected}",
            b.bids[0].price
        );
    }
}
