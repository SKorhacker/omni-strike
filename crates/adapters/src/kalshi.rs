//! Kalshi — CFTC-regulated prediction market (centralised exchange, not onchain). READ-ONLY connector
//! for the crypto "price ABOVE threshold" binaries (`ProductFamily::Binary`). Market data is public
//! (no key); TRADING requires a verified Kalshi account (US persons, KYC) — never assumed here.
//!
//! Verified live 2026-10-06 (public REST `https://api.elections.kalshi.com/trade-api/v2`):
//! - Series `KXBTCD` (BTC) / `KXETHD` (ETH), title "price Above/below", `fee_type: quadratic`,
//!   `fee_multiplier: 1`. Hourly events (`KXBTCD-26OCT0609`, 9 AM EDT) plus the 5 PM EDT daily and a
//!   Friday event. 318 BTC + 390 ETH open markets at 12:30 UTC; every one `market_type: binary`,
//!   `strike_type: greater`, `floor_strike` e.g. 95299.99, `notional_value_dollars: "1.0000"`.
//! - `rules_primary` (verbatim): "If the simple average of the sixty seconds of CF Benchmarks' Bitcoin
//!   Real-Time Index (BRTI) before 9 AM EDT is above 95299.99 at 9 AM EDT on Oct 6, 2026, then the
//!   market resolves to Yes." => YES pays iff average > floor_strike (STRICTLY above; the UI subtitle
//!   "$95,300 or above" is the same thing at cent precision). ETH: Ethereum Real-Time Index (ERTI).
//! - Times: the ticker encodes Eastern Time (DST!) and is NEVER parsed. `close_time` (UTC, e.g.
//!   `2026-10-06T13:00:00Z` = 9 AM EDT) is the instant named in the rules (average of the 60 s before
//!   it) => `expiry`. `expiration_time` / `latest_expiration_time` (a week later) are only the latest
//!   settlement deadline and `expected_expiration_time` (+5 min) the expected settlement — kept in
//!   `native`, not used as expiry. `can_close_early: true` is kept in `native`.
//! - `GET /markets/{ticker}/orderbook`: `{"orderbook_fp": {"yes_dollars": [[price, size]...],
//!   "no_dollars": [...]}}`, BIDS ONLY, ascending by price, fixed-point strings ("0.4300",
//!   "3123.00"). A NO bid at q is a YES ask at 1 - q (same size). No timestamp, no index price.
//! - `GET /markets?series_ticker=..&status=open` carries top of book WITH sizes
//!   (`yes_bid_dollars`/`yes_bid_size_fp`, `yes_ask_dollars`/`yes_ask_size_fp`; the ask size equals
//!   the best NO bid size of the full book) => used for `bulk_top_of_book`.
//! - Ranges (`KXBTC`, which also lists `greater`/`less`/`between` markets) and up/down series
//!   (`KXBTC15M`, `greater_or_equal` vs the opening price) are NOT threshold contracts at a fixed
//!   strike: only `KXBTCD`/`KXETHD` are queried, and markets are re-checked by series prefix.
//!
//! [DOC https://docs.kalshi.com/getting_started/fixed_point_migration] prices are dollar strings on
//! the grid given by `price_ranges` (`linear_cent` = one band, step 0.01); contract counts are
//! fixed-point with minimum granularity 0.01 contracts (fractional contracts allowed).
//!
//! Fees [series `fee_type`/`fee_multiplier`; DOC https://docs.kalshi.com/api-reference/market/get-series
//! -> https://kalshi.com/docs/kalshi-fee-schedule.pdf]: `quadratic` = General Trading Fees table:
//! taker fee = ceil_to_cent(0.07 x multiplier x C x P x (1 - P)); makers pay nothing unless the type
//! is `quadratic_with_maker_fees` (0.25 x the taker rate = 0.0175) or
//! `quadratic_with_combo_maker_fees` (0.5 x = 0.035). Rounding [DOC getting_started/fee_rounding]:
//! fees are computed to $0.000001 and the balance change is aligned to $0.01 for non-direct members
//! (an order-level accumulator rebates the overpayment), i.e. ~ceil to the cent per order; direct
//! members align to $0.0001. Event-level fee overrides (`GET /events/fee_changes`, empty for
//! KXBTCD on 2026-10-06) are NOT applied.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;

pub const MAINNET_REST: &str = "https://api.elections.kalshi.com/trade-api/v2";
pub const NETWORK: &str = "kalshi-exchange";
pub const SPEC_VERSION: &str = "kalshi-threshold/2026-10-06";
const FEE_SOURCE: &str = "https://kalshi.com/docs/kalshi-fee-schedule.pdf (General Trading Fees: \
    0.07 x C x P x (1-P), rounded up to the cent) + series fee_type/fee_multiplier \
    (https://docs.kalshi.com/api-reference/market/get-series)";
const SETTLEMENT_WINDOW: &str =
    "simple average of the 60 one-second CF Benchmarks RTI prices in the minute before close_time";
/// [DOC fixed_point_migration] "Minimum granularity is 0.01 contracts".
const CONTRACT_STEP: Decimal = Decimal::from_parts(1, 0, 0, false, 2);
const MAX_PAGES: usize = 20;

const P: PlatformId = PlatformId::Kalshi;

/// Threshold ("above") series per underlying and the CF Benchmarks index named in its rules.
fn series_for(underlying: &str) -> Option<(&'static str, &'static str)> {
    match underlying {
        "BTC" => Some(("KXBTCD", "BRTI")),
        "ETH" => Some(("KXETHD", "ERTI")),
        _ => None,
    }
}

pub struct KalshiConnector {
    http: reqwest::Client,
    rest: String,
}

impl Default for KalshiConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl KalshiConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        get_json(&self.http, P, format!("{}{path}", self.rest), query).await
    }

    /// All open markets of a series (cursor pagination).
    async fn open_markets(&self, series: &str) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut q = vec![
                ("series_ticker", series),
                ("status", "open"),
                ("limit", "1000"),
            ];
            if !cursor.is_empty() {
                q.push(("cursor", cursor.as_str()));
            }
            let v = self.get("/markets", &q).await?;
            let page = v["markets"]
                .as_array()
                .ok_or_else(|| invalid(P, "/markets: no markets array"))?;
            out.extend(page.iter().cloned());
            match v["cursor"].as_str() {
                Some(c) if !c.is_empty() && !page.is_empty() => cursor = c.to_owned(),
                _ => return Ok(out),
            }
        }
        Err(invalid(
            P,
            format!("/markets {series}: more than {MAX_PAGES} pages"),
        ))
    }
}

fn utc(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    v[field]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| {
            invalid(
                P,
                format!("{field}: expected RFC 3339 UTC time, got {}", v[field]),
            )
        })
}

/// True for a live, fixed-threshold "above" binary of `series` (skips ranges, up/down, other series).
fn is_threshold_market(v: &Value, series: &str) -> bool {
    let prefix = format!("{series}-");
    v["ticker"].as_str().is_some_and(|t| t.starts_with(&prefix))
        && v["event_ticker"]
            .as_str()
            .is_some_and(|t| t.starts_with(&prefix))
        && v["market_type"] == "binary"
        && v["strike_type"] == "greater"
        && matches!(v["status"].as_str(), Some("active" | "open"))
        && v["floor_strike"].is_number()
}

/// Uniform tick from `price_ranges`; `None` when the grid has several different steps.
fn uniform_tick(v: &Value) -> Result<Option<Decimal>> {
    let Some(bands) = v["price_ranges"].as_array() else {
        return Ok(None);
    };
    let mut tick: Option<Decimal> = None;
    for b in bands {
        let step = dec(P, &b["step"], "price_ranges.step")?;
        match tick {
            Some(t) if t != step => return Ok(None),
            _ => tick = Some(step),
        }
    }
    Ok(tick.map(|t| t.normalize()))
}

/// `series` = the `GET /series/{ticker}` `series` object (fee type/multiplier, settlement source).
pub fn parse_market(
    v: &Value,
    series: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    let Some((series_ticker, index)) = series_for(underlying) else {
        return Ok(None);
    };
    if !is_threshold_market(v, series_ticker) {
        return Ok(None);
    }
    let ticker = v["ticker"].as_str().unwrap_or_default();
    let expiry = utc(v, "close_time")?;
    if expiry <= now {
        return Ok(None);
    }
    let payout = dec(P, &v["notional_value_dollars"], "notional_value_dollars")?.normalize();
    if payout <= Decimal::ZERO {
        return Err(invalid(P, format!("{ticker}: non-positive notional")));
    }
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: ticker.into(),
        },
        family: ProductFamily::Binary,
        underlying: underlying.into(),
        // YES pays iff the 60 s average is STRICTLY above floor_strike (rules_primary).
        option_kind: Some(OptionKind::Call),
        strike: Some(dec(P, &v["floor_strike"], "floor_strike")?.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: payout,
        premium_currency: "USD".into(),
        collateral_assets: vec!["USD".into()],
        settlement_asset: Some("USD".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("cf-benchmarks:{index} 60s average")),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        tick_size: uniform_tick(v)?,
        min_amount: Some(CONTRACT_STEP),
        amount_step: Some(CONTRACT_STEP),
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "ticker": ticker, "event_ticker": v["event_ticker"], "series_ticker": series_ticker,
            "strike_type": v["strike_type"], "floor_strike": v["floor_strike"],
            "rules_primary": v["rules_primary"], "close_time": v["close_time"],
            "expiration_time": v["expiration_time"],
            "expected_expiration_time": v["expected_expiration_time"],
            "latest_expiration_time": v["latest_expiration_time"],
            "can_close_early": v["can_close_early"],
            "price_level_structure": v["price_level_structure"], "price_ranges": v["price_ranges"],
            "fee_type": series["fee_type"], "fee_multiplier": series["fee_multiplier"],
            "settlement_sources": series["settlement_sources"],
            "venue_type": "centralised_exchange (CFTC-regulated DCM)",
            "account_required": "verified Kalshi account (US persons, KYC) to trade; market data is public"
        }),
    }))
}

/// `markets` = the `/markets` response (or its `markets` array).
pub fn parse_markets(
    markets: &Value,
    series: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = markets
        .get("markets")
        .unwrap_or(markets)
        .as_array()
        .ok_or_else(|| invalid(P, "markets: not an array"))?;
    let mut out = vec![];
    for m in arr {
        if let Some(i) = parse_market(m, series, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn bid_levels(book: &Value, side: &str, payout: Decimal) -> Result<Vec<Level>> {
    let arr = match book.get(side) {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(Value::Array(a)) => a,
        Some(other) => return Err(invalid(P, format!("orderbook.{side}: {other}"))),
    };
    arr.iter()
        .map(|l| {
            let price = dec(P, &l[0], "level.price")?;
            let size = dec(P, &l[1], "level.size")?;
            if price <= Decimal::ZERO || price >= payout || size <= Decimal::ZERO {
                return Err(invalid(P, format!("orderbook.{side}: bad level {l}")));
            }
            Ok(Level { price, size })
        })
        .collect()
}

/// YES book from Kalshi's bids-only book: bids = YES bids; asks = `payout - NO bid` (same sizes).
pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let book = v
        .get("orderbook_fp")
        .or_else(|| v.get("orderbook"))
        .filter(|b| b.is_object())
        .ok_or_else(|| invalid(P, "orderbook: missing orderbook_fp"))?;
    let payout = instrument.contract_multiplier;
    let mut bids = bid_levels(book, "yes_dollars", payout)?;
    bids.sort_by(|a, b| b.price.cmp(&a.price));
    let mut asks: Vec<Level> = bid_levels(book, "no_dollars", payout)?
        .into_iter()
        .map(|l| Level {
            price: payout - l.price,
            size: l.size,
        })
        .collect();
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

/// Top of book for every threshold market of `underlying` from ONE `/markets` listing.
pub fn parse_bulk_tops(
    markets: &[Value],
    underlying: &str,
    now: DateTime<Utc>,
    provenance: Provenance,
) -> Result<Vec<BookSnapshot>> {
    let Some((series, _)) = series_for(underlying) else {
        return Ok(vec![]);
    };
    let mut out = vec![];
    for m in markets {
        if !is_threshold_market(m, series) || utc(m, "close_time")? <= now {
            continue;
        }
        let ticker = m["ticker"].as_str().unwrap_or_default();
        let payout = dec(P, &m["notional_value_dollars"], "notional_value_dollars")?;
        let level = |px: &str, sz: &str| -> Result<Option<Level>> {
            let price = opt_dec(P, m.get(px), px)?;
            let size = opt_dec(P, m.get(sz), sz)?;
            Ok(match (price, size) {
                (Some(price), Some(size))
                    if price > Decimal::ZERO && price < payout && size > Decimal::ZERO =>
                {
                    Some(Level { price, size })
                }
                _ => None,
            })
        };
        out.push(BookSnapshot {
            instrument: InstrumentKey {
                platform: P,
                network: NETWORK.into(),
                native_id: ticker.into(),
            },
            bids: level("yes_bid_dollars", "yes_bid_size_fp")?
                .into_iter()
                .collect(),
            asks: level("yes_ask_dollars", "yes_ask_size_fp")?
                .into_iter()
                .collect(),
            source_ts: None,
            received_at: now,
            index_price: None,
            mark_price: None,
            provenance,
            access: BookAccess::Api,
        });
    }
    Ok(out)
}

/// Quadratic fee from the series `fee_type` / `fee_multiplier` stored on the instrument.
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let n = &instrument.native;
    let mult = dec(P, &n["fee_multiplier"], "fee_multiplier")?;
    let quad = |rate: Decimal| FeeFormula::QuadraticProbability {
        rate: (rate * mult).normalize(),
        round_up_to: Some(Decimal::new(1, 2)),
    };
    let taker_rate = Decimal::new(7, 2);
    let fee_type = n["fee_type"].as_str().unwrap_or_default();
    let (taker, maker) = match fee_type {
        "quadratic" => (
            quad(taker_rate),
            FeeFormula::Fixed {
                amount: Decimal::ZERO,
            },
        ),
        "quadratic_with_maker_fees" => (quad(taker_rate), quad(Decimal::new(175, 4))),
        "quadratic_with_combo_maker_fees" => (quad(taker_rate), quad(Decimal::new(35, 3))),
        other => {
            let u = FeeFormula::Unknown {
                reason: format!("Kalshi fee_type {other:?} not modelled"),
            };
            (u.clone(), u)
        }
    };
    Ok(FeeSchedule {
        taker,
        maker,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: format!(
            "Series fee_type {fee_type}, fee_multiplier {mult}. Taker ceil_cent(0.07 x mult x C x P x (1-P)); \
             maker fee only on *_with_maker_fees series. Rounding modelled as ceil to $0.01 per order \
             (non-direct member balance precision; direct members $0.0001). Event fee overrides not \
             applied. Trading needs a verified Kalshi account (US persons, KYC)."
        ),
    })
}

#[async_trait]
impl Connector for KalshiConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let Some((series, _)) = series_for(underlying) else {
            return Ok(vec![]);
        };
        let s = self.get(&format!("/series/{series}"), &[]).await?;
        let s = s
            .get("series")
            .cloned()
            .ok_or_else(|| invalid(P, "/series: no series object"))?;
        let markets = self.open_markets(series).await?;
        parse_markets(&Value::Array(markets), &s, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let t0 = std::time::Instant::now();
        let v = self
            .get(
                &format!("/markets/{}/orderbook", instrument.key.native_id),
                &[],
            )
            .await?;
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(instrument, &v, received_at, Provenance::LiveData)?;
        Ok(crate::MarketData::rest(book, network, t1))
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        fee_schedule(instrument)
    }

    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        let Some((series, _)) = series_for(underlying) else {
            return Ok(Some(vec![]));
        };
        let markets = self.open_markets(series).await?;
        parse_bulk_tops(&markets, underlying, Utc::now(), Provenance::LiveData).map(Some)
    }

    fn bulk_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oa_core::fees::{FeeAmount, FeeInputs};
    use rust_decimal_macros::dec;

    fn fixture(name: &str) -> Value {
        crate::fixture("recorded-2026-10-06", name)
    }
    fn at() -> DateTime<Utc> {
        "2026-10-06T12:30:00Z".parse().unwrap()
    }
    fn series() -> Value {
        fixture("kalshi_series_btc.json")["series"].clone()
    }
    const T: &str = "KXBTCD-26OCT0617-T86249.99";
    fn btc() -> Vec<Instrument> {
        parse_markets(&fixture("kalshi_markets_btc.json"), &series(), "BTC", at()).unwrap()
    }

    #[test]
    fn maps_threshold_markets_with_utc_close_time() {
        let cat = btc();
        assert_eq!(cat.len(), 4);
        let i = cat.iter().find(|i| i.key.native_id == T).unwrap();
        assert_eq!(i.family, ProductFamily::Binary);
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.strike, Some(dec!(86249.99)));
        // 5 PM EDT = 21:00 UTC (close_time), not the ET-coded ticker nor expiration_time.
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-06T21:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.tick_size, Some(dec!(0.01)));
        assert_eq!(i.min_amount, Some(dec!(0.01)));
        assert_eq!(i.premium_currency, "USD");
        assert_eq!(
            i.settlement_index.as_deref(),
            Some("cf-benchmarks:BRTI 60s average")
        );
        // The hourly 9 AM EDT (13:00 UTC) market drops out once closed.
        let later: DateTime<Utc> = "2026-10-06T13:00:00Z".parse().unwrap();
        let n = parse_markets(&fixture("kalshi_markets_btc.json"), &series(), "BTC", later)
            .unwrap()
            .len();
        assert_eq!(n, 3);
        // Other underlying: not this series.
        assert!(
            parse_markets(&fixture("kalshi_markets_btc.json"), &series(), "ETH", at())
                .unwrap()
                .is_empty()
        );
        let eth =
            parse_markets(&fixture("kalshi_markets_eth.json"), &series(), "ETH", at()).unwrap();
        assert_eq!(eth.len(), 2);
        assert!(eth[0].settlement_index.as_deref().unwrap().contains("ERTI"));
    }

    #[test]
    fn skips_range_and_non_threshold_series() {
        // KXBTC lists greater / less / between markets: none is a KXBTCD threshold contract.
        let v = fixture("kalshi_markets_kxbtc_ranges.json");
        let types: Vec<_> = v["markets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["strike_type"].as_str().unwrap())
            .collect();
        assert_eq!(types, ["greater", "less", "between"]);
        assert!(
            parse_markets(&v, &series(), "BTC", at())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn book_complements_no_bids_into_yes_asks() {
        let i = btc().into_iter().find(|i| i.key.native_id == T).unwrap();
        let b = parse_book(
            &i,
            &fixture("kalshi_orderbook_btc.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // Recorded: best YES bid 0.43 x 3123; best NO bid 0.56 x 2721 => YES ask 1 - 0.56 = 0.44 x 2721.
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(0.43),
                size: dec!(3123)
            }
        );
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(0.44),
                size: dec!(2721)
            }
        );
        // Next NO bid 0.55 x 5871.62 => second YES ask 0.45 (fractional size kept).
        assert_eq!(
            b.asks[1],
            Level {
                price: dec!(0.45),
                size: dec!(5871.62)
            }
        );
        assert!(b.bids.windows(2).all(|w| w[0].price > w[1].price));
        assert!(b.asks.windows(2).all(|w| w[0].price < w[1].price));
        assert!(b.bids[0].price < b.asks[0].price);
        assert_eq!(b.index_price, None);
        assert_eq!(b.access, BookAccess::Api);
    }

    #[test]
    fn bulk_tops_from_market_listing() {
        let v = fixture("kalshi_markets_btc.json");
        let tops = parse_bulk_tops(
            v["markets"].as_array().unwrap(),
            "BTC",
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(tops.len(), 4);
        let t = tops.iter().find(|b| b.instrument.native_id == T).unwrap();
        assert_eq!(
            t.bids,
            vec![Level {
                price: dec!(0.43),
                size: dec!(3123)
            }]
        );
        assert_eq!(
            t.asks,
            vec![Level {
                price: dec!(0.44),
                size: dec!(2720.27)
            }]
        );
        // No YES bid (0.0000 x 0.00) => empty side, never a fabricated level.
        let far = tops
            .iter()
            .find(|b| b.instrument.native_id == "KXBTCD-26OCT0609-T95299.99")
            .unwrap();
        assert!(far.bids.is_empty() && far.asks.len() == 1);
    }

    #[test]
    fn quadratic_taker_fee_rounds_up_to_the_cent() {
        let i = btc().into_iter().find(|i| i.key.native_id == T).unwrap();
        let f = fee_schedule(&i).unwrap();
        assert_eq!(
            f.taker,
            FeeFormula::QuadraticProbability {
                rate: dec!(0.07),
                round_up_to: Some(dec!(0.01))
            }
        );
        // C = 100 contracts at P = 0.55: 0.07 * 100 * 0.55 * 0.45 = 1.7325 -> 1.74 USD.
        let fee = f.taker.compute(&FeeInputs {
            qty_native: dec!(100),
            qty_underlying: dec!(100),
            premium: dec!(55),
            index_price: None,
            mark_price: None,
        });
        assert_eq!(fee, FeeAmount::Known { amount: dec!(1.74) });
        assert_eq!(f.maker, FeeFormula::Fixed { amount: dec!(0) });
        assert_eq!(f.currency, "USD");
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_kalshi_btc_thresholds() {
        let c = KalshiConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        println!("kalshi open BTC threshold markets: {}", cat.len());
        assert!(!cat.is_empty());
        let tops = c.bulk_top_of_book("BTC").await.unwrap().unwrap();
        let two = tops
            .iter()
            .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
            .count();
        println!("kalshi bulk tops: {} rows, {two} two-sided", tops.len());
        // Most liquid-looking: the two-sided market closest to 50c.
        let pick = tops
            .iter()
            .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
            .min_by_key(|b| (b.bids[0].price - dec!(0.5)).abs())
            .unwrap();
        let i = cat.iter().find(|i| i.key == pick.instrument).unwrap();
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "kalshi {} strike {:?} expiry {:?} bid {:?} ask {:?} ({} bid / {} ask levels, {:?})",
            i.key.native_id,
            i.strike,
            i.expiry,
            md.book.bids.first(),
            md.book.asks.first(),
            md.book.bids.len(),
            md.book.asks.len(),
            md.network
        );
        assert!(!md.book.asks.is_empty());
    }
}
