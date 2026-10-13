//! Thetanuts Finance V4 — OptionBook on Base mainnet, READ-ONLY.
//!
//! Sources (read/probed 2026-10-06):
//! - Official SDK `@thetanuts-finance/thetanuts-client` 0.3.0 (npm): order feed host, OptionBook
//!   address, decimals (prices/strikes 8 decimals, USDC 6), collateral math per order type.
//! - Order feed `GET {ORDERS_URL}` -> `data.orders[]`: maker-signed EIP-712 limit orders anyone can
//!   fill on-chain (`OptionBook.fillOrder`), plus `data.market_data` (spot per underlying) and
//!   `data.timestamp`. [LIVE] 387 orders, one maker, BTC/ETH expiries 1-3 days, 10k USDC max each.
//! - On-chain [LIVE eth_call, Base]: `OptionBook.getValidNumContracts(impl, [strike], 1)` = strike
//!   USDC per contract for single-strike USDC-collateral orders (puts AND calls);
//!   `OptionBook.minPremiumAmount()` = 50 USDC; `OptionFactory.calculateFee(n, premium, price)` =
//!   min(0.06 % x n x underlying price, 12.5 % x premium) (reproduced on 5 inputs).
//!
//! What becomes a book: single-strike orders where the maker is SHORT (`isLong = false`) with
//! USDC collateral -> ASK levels we can buy (price = `price`/1e8 USDC per contract of 1 underlying,
//! size = maxCollateralUsable / strike). Multi-leg structures, maker-long orders (we would have to
//! post collateral to sell) and non-USDC collateral are skipped. Bids are therefore empty.
//! Remaining size: `amountFilled` is not subtracted (the fill reverts if exhausted) — assumption.
//! Settlement: cash in USDC at expiry (08:00 UTC) on the order's Chainlink price feed, European.

use crate::http::{client, read_json};
use crate::{Connector, Result, invalid};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const ORDERS_URL: &str = "https://round-snowflake-9c31.devops-118.workers.dev";
pub const NETWORK: &str = "thetanuts-base-mainnet";
pub const SPEC_VERSION: &str = "thetanuts-v4/2026-10-06";
/// Base mainnet USDC (SDK chain config).
pub const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
pub const OPTION_BOOK: &str = "0x1bDff855d6811728acaDC00989e79143a2bdfDed";
const FEE_SOURCE: &str = "On-chain OptionFactory.calculateFee (Base 0x8118dad971debffb49b9280047659174128a8b94), probed 2026-10-06";
/// Minimum premium per fill, `OptionBook.minPremiumAmount()` = 50 USDC [LIVE].
pub const MIN_PREMIUM_USDC: Decimal = Decimal::from_parts(50, 0, 0, false, 0);
/// The order feed is one document for every market: refetched at most this often.
const SNAPSHOT_TTL: Duration = Duration::from_millis(800);

const P: PlatformId = PlatformId::Thetanuts;

pub struct ThetanutsConnector {
    http: reqwest::Client,
    url: String,
    snapshot: Mutex<Option<(Instant, Value, std::time::Duration)>>,
}

impl Default for ThetanutsConnector {
    fn default() -> Self {
        Self::new(ORDERS_URL)
    }
}

impl ThetanutsConnector {
    pub fn new(url: &str) -> Self {
        Self {
            http: client(),
            url: url.into(),
            snapshot: Mutex::new(None),
        }
    }

    /// Order feed, shared by every instrument for SNAPSHOT_TTL. Returns (body, network time).
    async fn orders(&self) -> Result<(Value, std::time::Duration)> {
        let mut g = self.snapshot.lock().await;
        if let Some((at, v, _)) = g.as_ref()
            && at.elapsed() < SNAPSHOT_TTL
        {
            return Ok((v.clone(), std::time::Duration::ZERO));
        }
        let t0 = Instant::now();
        let v = read_json(P, self.http.get(&self.url).send().await).await?;
        let net = t0.elapsed();
        *g = Some((Instant::now(), v.clone(), net));
        Ok((v, net))
    }
}

fn u(v: &Value, f: &str) -> Result<u128> {
    match v {
        Value::String(s) => s
            .parse()
            .map_err(|_| invalid(P, format!("{f}: not an integer: {s}"))),
        Value::Number(n) => n
            .as_u64()
            .map(u128::from)
            .ok_or_else(|| invalid(P, format!("{f}: {n}"))),
        _ => Err(invalid(P, format!("{f}: missing"))),
    }
}

fn scaled(raw: u128, decimals: u32) -> Decimal {
    Decimal::from_i128_with_scale(raw as i128, decimals).normalize()
}

fn secs(v: &Value, f: &str) -> Result<DateTime<Utc>> {
    let s = u(v, f)? as i64;
    Utc.timestamp_opt(s, 0)
        .single()
        .ok_or_else(|| invalid(P, format!("{f}: out of range")))
}

/// One buyable order: (ticker, underlying, is_call, strike, expiry, ask price, contracts, raw).
struct Ask {
    ticker: String,
    underlying: String,
    is_call: bool,
    strike: Decimal,
    expiry: DateTime<Utc>,
    price: Decimal,
    size: Decimal,
    price_feed: String,
}

fn buyable_asks(v: &Value, now: DateTime<Utc>) -> Result<Vec<Ask>> {
    let orders = v["data"]["orders"]
        .as_array()
        .ok_or_else(|| invalid(P, "data.orders missing"))?;
    let mut out = vec![];
    for o in orders {
        let od = &o["order"];
        let strikes = od["strikes"].as_array().map(Vec::len).unwrap_or(0);
        let usdc = od["collateral"]
            .as_str()
            .is_some_and(|c| c.eq_ignore_ascii_case(USDC));
        let Some(ticker) = od["ticker"].as_str() else {
            continue;
        };
        if strikes != 1 || !usdc || od["isLong"] != false || o["chainId"] != 8453 {
            continue;
        }
        if secs(&od["orderExpiryTimestamp"], "orderExpiryTimestamp")? <= now {
            continue;
        }
        let strike = scaled(u(&od["strikes"][0], "strike")?, 8);
        let price = scaled(u(&od["price"], "price")?, 8);
        let coll = scaled(u(&od["maxCollateralUsable"], "maxCollateralUsable")?, 6);
        if strike <= Decimal::ZERO || price <= Decimal::ZERO || coll <= Decimal::ZERO {
            continue;
        }
        let underlying = ticker.split('-').next().unwrap_or_default().to_string();
        out.push(Ask {
            ticker: ticker.into(),
            underlying,
            is_call: od["isCall"] == true,
            strike,
            expiry: secs(&od["expiry"], "expiry")?,
            price,
            // Collateral per contract = strike USDC [LIVE getValidNumContracts]; 6-decimal contracts.
            size: (coll / strike).round_dp_with_strategy(6, rust_decimal::RoundingStrategy::ToZero),
            price_feed: od["priceFeed"].as_str().unwrap_or_default().to_string(),
        });
    }
    Ok(out)
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let mut by: BTreeMap<String, Ask> = BTreeMap::new();
    for a in buyable_asks(v, now)? {
        if a.underlying != underlying || a.expiry <= now {
            continue;
        }
        let keep_cheaper = by.get(&a.ticker).is_none_or(|b| a.price < b.price);
        if keep_cheaper {
            by.insert(a.ticker.clone(), a);
        }
    }
    Ok(by
        .into_values()
        .map(|a| Instrument {
            key: InstrumentKey { platform: P, network: NETWORK.into(), native_id: a.ticker.clone() },
            family: ProductFamily::DatedOption,
            underlying: underlying.into(),
            option_kind: Some(if a.is_call { OptionKind::Call } else { OptionKind::Put }),
            strike: Some(a.strike),
            expiry: Some(a.expiry),
            exercise_style: Some(ExerciseStyle::European),
            contract_multiplier: Decimal::ONE,
            premium_currency: "USDC".into(),
            collateral_assets: vec!["USDC".into()],
            settlement_asset: Some("USDC".into()),
            settlement_mechanism: Some(SettlementMechanism::Cash),
            settlement_index: Some(format!("chainlink:{underlying}/USD (Base feed {})", a.price_feed)),
            settlement_window: Some("price feed at expiry".into()),
            tick_size: None,
            // The venue minimum is a PREMIUM (50 USDC per fill): expressed in contracts at the
            // best price when listed; re-checked at estimate time from the live price.
            min_amount: Some((MIN_PREMIUM_USDC / a.price).round_dp_with_strategy(6, rust_decimal::RoundingStrategy::AwayFromZero)),
            amount_step: Some(Decimal::new(1, 6)),
            is_active: true,
            spec_version: SPEC_VERSION.into(),
            native: serde_json::json!({"ticker": a.ticker, "price_feed": a.price_feed, "option_book": OPTION_BOOK}),
        })
        .collect())
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let mut asks: Vec<Level> = buyable_asks(v, received_at)?
        .into_iter()
        .filter(|a| a.ticker == instrument.key.native_id)
        .map(|a| Level {
            price: a.price,
            size: a.size,
        })
        .collect();
    asks.sort_by(|a, b| a.price.cmp(&b.price));
    let ts = v["data"]["timestamp"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let index = v["data"]["market_data"][&instrument.underlying]
        .as_f64()
        .and_then(|f| Decimal::try_from(f).ok())
        .filter(|d| *d > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: vec![],
        asks,
        source_ts: ts,
        received_at,
        index_price: index,
        mark_price: None,
        provenance,
        access: BookAccess::Api,
    })
}

/// min(0.06 % x contracts x underlying price, 12.5 % x premium) [LIVE calculateFee].
pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let f = || FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional {
                rate: Decimal::new(6, 4),
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
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Formula reproduced from OptionFactory.calculateFee on 5 inputs. ASSUMPTION: charged to the taker on top of the premium (conservative). Minimum premium 50 USDC per fill (OptionBook.minPremiumAmount).".into(),
    }
}

#[async_trait]
impl Connector for ThetanutsConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let (v, _) = self.orders().await?;
        parse_instruments(&v, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let (v, network) = self.orders().await?;
        let received_at = Utc::now();
        let t1 = Instant::now();
        let book = parse_book(instrument, &v, received_at, Provenance::LiveData)?;
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
        crate::fixture("recorded-2026-10-06", "thetanuts_orders.json")
    }
    fn at() -> DateTime<Utc> {
        // Feed timestamp of the recording (orders valid ~2 min after it).
        "2026-10-06T08:04:50Z".parse().unwrap()
    }

    #[test]
    fn normalises_buyable_single_strike_usdc_orders() {
        let v = fixture();
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let eth = parse_instruments(&v, "ETH", at()).unwrap();
        assert!(!btc.is_empty() && !eth.is_empty());
        let i = btc
            .iter()
            .find(|i| i.key.native_id == "BTC-7OCT26-84000-P")
            .unwrap();
        assert_eq!(i.strike, Some(dec!(84000)));
        assert_eq!(i.option_kind, Some(OptionKind::Put));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-07T08:00:00+00:00");
        assert_eq!(i.key.network, NETWORK);
        // 50 USDC minimum premium at 113.30977282 per contract.
        assert_eq!(i.min_amount, Some(dec!(0.441269)));
        assert!(eth.iter().all(|i| i.underlying == "ETH"));
    }

    #[test]
    fn book_is_asks_only_sized_by_collateral() {
        let v = fixture();
        let i = parse_instruments(&v, "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-7OCT26-84000-P")
            .unwrap();
        let b = parse_book(&i, &v, at(), Provenance::Fixture).unwrap();
        assert!(b.bids.is_empty());
        assert_eq!(b.asks[0].price, dec!(113.30977282));
        // 10,000 USDC / 84,000 strike.
        assert_eq!(b.asks[0].size, dec!(0.119047));
        assert_eq!(b.index_price, Some(dec!(85616.48)));
        assert_eq!(b.access, BookAccess::Api);
    }

    #[test]
    fn expired_orders_are_dropped() {
        let later: DateTime<Utc> = "2026-10-06T09:00:00Z".parse().unwrap();
        assert!(
            parse_instruments(&fixture(), "BTC", later)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fee_matches_on_chain_calculate_fee() {
        use oa_core::fees::FeeInputs;
        let v = fixture();
        let i = parse_instruments(&v, "BTC", at()).unwrap().remove(0);
        let s = fee_schedule(&i);
        // calculateFee(1 contract, premium 1000, price 85600) = 51.36 ; (1, 113.31, 85600) = 14.16375
        let fee = |premium: Decimal| {
            s.taker.compute(&FeeInputs {
                qty_native: dec!(1),
                qty_underlying: dec!(1),
                premium,
                index_price: Some(dec!(85600)),
                mark_price: None,
            })
        };
        assert_eq!(
            fee(dec!(1000)),
            oa_core::fees::FeeAmount::Known {
                amount: dec!(51.36)
            }
        );
        assert_eq!(
            fee(dec!(113.31)),
            oa_core::fees::FeeAmount::Known {
                amount: dec!(14.16375)
            }
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_catalog_and_book() {
        let c = ThetanutsConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty(), "no buyable Thetanuts BTC order");
        let md = c.get_market_data(&cat[0]).await.unwrap();
        println!(
            "thetanuts {} asks {:?} index {:?}",
            cat[0].key.native_id,
            md.book.asks.first(),
            md.book.index_price
        );
        assert!(!md.book.asks.is_empty());
    }
}
