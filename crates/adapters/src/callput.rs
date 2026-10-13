//! CallPut (Base) — READ-ONLY. Crypto AND synthetic stock/ETF options (SPY, QQQ, NVDA, TSLA, SPCX…)
//! priced by a shared liquidity pool (sOLP); trades are requests executed by a keeper.
//!
//! Sources (read/probed 2026-10-08):
//! - Public feed [DOC developers/contracts-addresses-and-data-sources]:
//!   `https://app-data-base.s3.ap-southeast-1.amazonaws.com/market-data.json` -> `data.market[U]
//!   .options[expiry][call|put][]` {instrument "BTC-16OCT26-76000-C", strikePrice, markPrice,
//!   riskPremiumRateForBuy/Sell, isOptionAvailable, expiry (s, 08:00 UTC)}, `data.spotIndices[U]`,
//!   `data.olpStats.sOlp.assetAmounts.usdc.availableAmount` (pool liquidity), `timestamp` (ms).
//!   Execution price = mark x (1 + buy rate) to buy, mark x (1 - sell rate) to sell [DOC
//!   traders/pricing-and-execution].
//! - Fees [DOC traders/fees-limits-and-faq]: open buy 0.06 %, open sell 0.03 %, trade fee capped at
//!   12.5 % of the premium; settlement 0.02 % (cap 12.5 %); execution fee = native ETH set in
//!   PositionManager: `executionFee()` = 0.00006 ETH [LIVE eth_call 2026-10-08].
//!   ASSUMPTION: the % applies to the underlying notional (Moby/Derive convention implied by the
//!   premium cap); the doc does not name the base.
//!
//! Size: no per-contract depth is published. The pool must hold the payout: we size a level at
//! available USDC / strike (puts) or / spot (calls) for buys, available / price for sells —
//! a conservative bound, not a quote. Requests are asynchronous (max 180 s validity).

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

pub const FEED_URL: &str = "https://app-data-base.s3.ap-southeast-1.amazonaws.com/market-data.json";
pub const NETWORK: &str = "callput-base-mainnet";
pub const SPEC_VERSION: &str = "callput-v2/2026-10-08";
const SNAPSHOT_TTL: Duration = Duration::from_secs(5);
const FEE_SOURCE: &str = "https://docs.callput.app/traders/fees-limits-and-faq (read 2026-10-08)";
const P: PlatformId = PlatformId::Callput;

pub struct CallputConnector {
    http: reqwest::Client,
    url: String,
    snapshot: Mutex<Option<(Instant, Value)>>,
}

impl Default for CallputConnector {
    fn default() -> Self {
        Self::new(FEED_URL)
    }
}

impl CallputConnector {
    pub fn new(url: &str) -> Self {
        Self {
            http: client(),
            url: url.into(),
            snapshot: Mutex::new(None),
        }
    }
    async fn feed(&self) -> Result<(Value, Duration)> {
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

fn num(v: &Value) -> Option<Decimal> {
    v.as_f64().and_then(|x| Decimal::try_from(x).ok())
}

struct Row<'a> {
    raw: &'a Value,
    is_put: bool,
    strike: Decimal,
    expiry: DateTime<Utc>,
    mark: Decimal,
}

fn rows<'a>(v: &'a Value, underlying: &str, now: DateTime<Utc>) -> Result<Vec<Row<'a>>> {
    let Some(opts) = v["data"]["market"][underlying]["options"].as_object() else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for (exp, sides) in opts {
        let ts: i64 = exp
            .parse()
            .map_err(|_| invalid(P, format!("expiry key {exp}")))?;
        let expiry = Utc
            .timestamp_opt(ts, 0)
            .single()
            .ok_or_else(|| invalid(P, "expiry"))?;
        if expiry <= now {
            continue;
        }
        for (side, is_put) in [("call", false), ("put", true)] {
            for r in sides[side].as_array().into_iter().flatten() {
                let (Some(strike), Some(mark)) = (num(&r["strikePrice"]), num(&r["markPrice"]))
                else {
                    continue;
                };
                if r["isOptionAvailable"] != true
                    || mark <= Decimal::ZERO
                    || r["instrument"].as_str().is_none()
                {
                    continue;
                }
                out.push(Row {
                    raw: r,
                    is_put,
                    strike,
                    expiry,
                    mark,
                });
            }
        }
    }
    Ok(out)
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let mut list: Vec<Instrument> = rows(v, underlying, now)?
        .into_iter()
        .map(|r| Instrument {
            key: InstrumentKey { platform: P, network: NETWORK.into(), native_id: r.raw["instrument"].as_str().unwrap_or_default().to_string() },
            family: ProductFamily::DatedOption,
            underlying: underlying.into(),
            option_kind: Some(if r.is_put { OptionKind::Put } else { OptionKind::Call }),
            strike: Some(r.strike),
            expiry: Some(r.expiry),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: Decimal::ONE,
            premium_currency: "USDC".into(),
            collateral_assets: vec!["USDC".into()],
            settlement_asset: Some("USDC".into()),
            settlement_mechanism: Some(SettlementMechanism::Cash),
            settlement_index: Some(format!("callput:SETTLE_PRICE_FEED {underlying}")),
            settlement_window: Some("settlement price feed at expiry".into()),
            tick_size: None,
            min_amount: None,
            amount_step: None,
            is_active: true,
            spec_version: SPEC_VERSION.into(),
            native: serde_json::json!({"option_id": r.raw["optionId"], "synthetic": !matches!(underlying, "BTC" | "ETH")}),
        })
        .collect();
    list.sort_by(|a, b| a.key.native_id.cmp(&b.key.native_id));
    Ok(list)
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let u = &instrument.underlying;
    let r = rows(v, u, received_at)?
        .into_iter()
        .find(|r| r.raw["instrument"].as_str() == Some(instrument.key.native_id.as_str()))
        .ok_or_else(|| invalid(P, format!("{} not available", instrument.key.native_id)))?;
    let spot = num(&v["data"]["spotIndices"][u]).filter(|s| *s > Decimal::ZERO);
    let avail = num(&v["data"]["olpStats"]["sOlp"]["assetAmounts"]["usdc"]["availableAmount"])
        .unwrap_or_default();
    let buy = num(&r.raw["riskPremiumRateForBuy"]).unwrap_or_default();
    let sell = num(&r.raw["riskPremiumRateForSell"]).unwrap_or_default();
    let ask = (r.mark * (Decimal::ONE + buy)).round_dp(6);
    let bid = (r.mark * (Decimal::ONE - sell)).round_dp(6);
    let floor = |x: Decimal| x.round_dp_with_strategy(6, rust_decimal::RoundingStrategy::ToZero);
    // Payout the pool must reserve per contract: strike (put) / spot (call, conservative).
    let reserve = if r.is_put { Some(r.strike) } else { spot };
    let asks = match reserve {
        Some(res) if avail > Decimal::ZERO && ask > Decimal::ZERO => vec![Level {
            price: ask,
            size: floor(avail / res),
        }],
        _ => vec![],
    };
    let bids = if avail > Decimal::ZERO && bid > Decimal::ZERO {
        vec![Level {
            price: bid,
            size: floor(avail / bid),
        }]
    } else {
        vec![]
    };
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids,
        asks,
        source_ts: v["timestamp"]
            .as_i64()
            .and_then(|t| Utc.timestamp_millis_opt(t).single()),
        received_at,
        index_price: spot,
        mark_price: Some(r.mark),
        provenance,
        access: BookAccess::Api,
    })
}

/// Open buy 0.06 % (sell 0.03 %) of underlying notional, capped at 12.5 % of the premium.
pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let f = |rate: Decimal| FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate },
            FeeFormula::PctOfPremium {
                rate: Decimal::new(125, 3),
            },
        ],
    };
    FeeSchedule {
        taker: f(Decimal::new(6, 4)),
        maker: f(Decimal::new(3, 4)),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-08".into(),
        notes: "taker = open buy 0.06 %, maker slot = open sell 0.03 % (CallPut has no maker/taker: a buy pays 0.06 %, a sell 0.03 %). ASSUMPTION: base = underlying notional. Settlement fee 0.02 % (cap 12.5 %) on ITM payouts not included. Execution fee 0.00006 ETH per request counted in the gas line.".into(),
    }
}

#[async_trait]
impl Connector for CallputConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let (v, _) = self.feed().await?;
        parse_instruments(&v, underlying, Utc::now())
    }
    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let (v, network) = self.feed().await?;
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
        crate::fixture("recorded-2026-10-08", "callput_market_data.json")
    }
    fn at() -> DateTime<Utc> {
        "2026-10-08T08:05:00Z".parse().unwrap()
    }

    #[test]
    fn lists_crypto_and_synthetic_stock_options() {
        let v = fixture();
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let spy = parse_instruments(&v, "SPY", at()).unwrap();
        assert!(!btc.is_empty() && !spy.is_empty());
        let i = btc
            .iter()
            .find(|i| i.key.native_id == "BTC-30OCT26-78000-C")
            .unwrap();
        assert_eq!(i.strike, Some(dec!(78000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-30T08:00:00+00:00");
        assert_eq!(spy[0].native["synthetic"], true);
    }

    #[test]
    fn book_is_mark_plus_minus_risk_premium_sized_by_pool() {
        let v = fixture();
        let i = parse_instruments(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-30OCT26-78000-C")
            .unwrap();
        let b = parse_book(&i, &v, at(), Provenance::Fixture).unwrap();
        let mark = dec!(6373.389200869911);
        assert!(
            (b.asks[0].price - mark * (dec!(1) + dec!(0.021402948177920263))).abs() < dec!(0.01)
        );
        assert!(
            (b.bids[0].price - mark * (dec!(1) - dec!(0.029758301434440866))).abs() < dec!(0.01)
        );
        assert!(b.asks[0].price > b.bids[0].price);
        // 115,466.90 USDC available / spot 82,980.08 per call.
        assert_eq!(b.asks[0].size, dec!(1.391501));
    }
}
