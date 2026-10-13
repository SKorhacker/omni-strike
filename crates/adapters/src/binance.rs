//! Binance European Options — USDT-quoted/settled options, public REST, READ-ONLY. Centralised
//! exchange (not onchain): trading needs the user's own Binance account (KYC) and API keys.
//!
//! Verified live 2026-10-06 (HTTP 200):
//! - `GET /eapi/v1/exchangeInfo`: `optionSymbols` = 1,574 symbols, all `status = TRADING`;
//!   646 `BTCUSDT` + 550 `ETHUSDT` (also BNB/SOL/XRP/DOGE/XAU/XAG underlyings, ignored here).
//!   Fields: `symbol` (e.g. `BTC-261007-87000-C`), `side` CALL/PUT, `strikePrice`, `expiryDate`
//!   (ms, 08:00 UTC), `unit` (contract multiplier, 1 for BTC/ETH), `quoteAsset` USDT, `minQty`,
//!   filters `PRICE_FILTER.tickSize` / `LOT_SIZE.stepSize`. `optionContracts[].settleAsset` = USDT,
//!   `optionAssets` = [USDT] only. Rate limit REQUEST_WEIGHT 400/min.
//! - `GET /eapi/v1/depth?symbol=X&limit=N`: `bids`/`asks` `[price USDT, qty contracts]` (strings),
//!   `T` (ms), `lastUpdateId`. Accepted `limit` values probed live: 10, 20, 50, 100, 1000
//!   (5, 7, 500, 1001 -> `-4021 Invalid depth limit`, although the docs list 500).
//! - `GET /eapi/v1/index?underlying=BTCUSDT`: `{indexPrice, time}`.
//! - `GET /eapi/v1/mark?symbol=X`: array of `{symbol, markPrice, markIV, delta, ...}`.
//!
//! PREMIUM CURRENCY = USDT (not USDC): not directly 1:1 comparable with USDC venues; the
//! comparison policy is decided outside this connector.
//!
//! Fees [DOC https://www.binance.com/en/fee/optionsTrading + FAQ
//! https://www.binance.com/en/support/faq/binance-options-trading-fees-5326e5de61c34fed98abe28d2f175a23
//! (updated 2026-03-06)]: regular user maker 0.024 %, taker 0.024 %;
//! Trading Fee = min(rate * Index Price * Contract Unit, 10 % * Option Traded Price) * size.
//! Exercise fee 0.015 % of settlement price capped at 10 % of option value (not modelled here).
//! Settlement: European, auto-exercised, cash in USDT. The exact settlement-price window is NOT
//! verified from an official Binance page, so `settlement_window` is left `None`.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::{Value, json};
use std::sync::Arc;

/// Persistent WebSocket book stream (see `binance_stream.rs` for the verified protocol).
#[path = "binance_stream.rs"]
pub mod stream;

pub const MAINNET_REST: &str = "https://eapi.binance.com";
pub const MAINNET_WS: &str = stream::MAINNET_WS_BASE;
pub const NETWORK: &str = "binance-eoptions";
pub const SPEC_VERSION: &str = "binance-eoptions/2026-10-06";
/// Live-probed allowed values: 10, 20, 50, 100, 1000.
const BOOK_DEPTH: &str = "100";
const QUOTE: &str = "USDT";
const FEE_SOURCE: &str = "https://www.binance.com/en/fee/optionsTrading + https://www.binance.com/en/support/faq/binance-options-trading-fees-5326e5de61c34fed98abe28d2f175a23 (0.024% of index * unit, cap 10% of option traded price)";
/// Regular-user maker and taker rate (0.024 %).
const FEE_RATE: Decimal = Decimal::from_parts(24, 0, 0, false, 5);
/// Cap: 10 % of the option traded price.
const FEE_CAP: Decimal = Decimal::from_parts(1, 0, 0, false, 1);

const P: PlatformId = PlatformId::Binance;

pub struct BinanceConnector {
    http: reqwest::Client,
    rest: String,
    ws: String,
    /// Persistent book stream (preferred when synchronised). `None` = REST only.
    stream: Option<Arc<stream::BinanceStream>>,
}

impl Default for BinanceConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl BinanceConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            ws: MAINNET_WS.into(),
            stream: None,
        }
    }

    /// Same connector with a persistent WebSocket book stream (requires a Tokio runtime). Books are
    /// served from the stream when synchronised and confirmed within `stream::MAX_SILENCE`; REST
    /// otherwise (and the REST answer then seeds the quiet book in the stream cache).
    pub fn with_stream(mut self) -> Self {
        self.stream = Some(stream::BinanceStream::spawn(&self.ws));
        self
    }

    /// Binance errors come back as non-2xx `{code, msg}` (handled by `read_json` as `Http`).
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let v = get_json(&self.http, P, format!("{}{path}", self.rest), query).await?;
        if let (Some(code), Some(msg)) = (v.get("code"), v.get("msg")) {
            return Err(invalid(P, format!("{path}: {code} {msg}")));
        }
        Ok(v)
    }
}

fn ms(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    crate::ts_ms(P, v.as_i64(), field)
}

fn filter<'a>(v: &'a Value, kind: &str, field: &str) -> Option<&'a Value> {
    v["filters"]
        .as_array()?
        .iter()
        .find(|f| f["filterType"] == kind)
        .and_then(|f| f.get(field))
}

/// `underlying` is the base asset as used across connectors ("BTC", "ETH").
pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["underlying"] != format!("{underlying}{QUOTE}").as_str() || v["quoteAsset"] != QUOTE {
        return Ok(None);
    }
    if v["status"] != "TRADING" {
        return Ok(None);
    }
    let name = v["symbol"].as_str().ok_or_else(|| invalid(P, "symbol"))?;
    let kind = match v["side"].as_str() {
        Some("CALL") => OptionKind::Call,
        Some("PUT") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: side {other:?}"))),
    };
    let expiry = ms(&v["expiryDate"], "expiryDate")?;
    if expiry <= now {
        return Ok(None);
    }
    let step = opt_dec(P, filter(v, "LOT_SIZE", "stepSize"), "stepSize")?;
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["strikePrice"], "strikePrice")?.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: dec(P, &v["unit"], "unit")?,
        premium_currency: QUOTE.into(),
        collateral_assets: vec![QUOTE.into()],
        settlement_asset: Some(QUOTE.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("binance:index:{underlying}{QUOTE}")),
        // Not verified from an official Binance page (see module docs).
        settlement_window: None,
        tick_size: opt_dec(P, filter(v, "PRICE_FILTER", "tickSize"), "tickSize")?
            .map(|d| d.normalize()),
        min_amount: opt_dec(P, v.get("minQty"), "minQty")?,
        amount_step: step,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "underlying": v["underlying"], "unit": v["unit"], "maxQty": v["maxQty"],
            "priceScale": v["priceScale"], "quantityScale": v["quantityScale"],
            "liquidationFeeRate": v["liquidationFeeRate"],
            "price_filter": { "minPrice": filter(v, "PRICE_FILTER", "minPrice"), "maxPrice": filter(v, "PRICE_FILTER", "maxPrice") },
            "venue_type": "centralised_exchange", "account_required": "user's own Binance account + API keys (KYC)"
        }),
    }))
}

/// `v` = the whole `/eapi/v1/exchangeInfo` response.
pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v["optionSymbols"]
        .as_array()
        .ok_or_else(|| invalid(P, "exchangeInfo.optionSymbols: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

/// `depth` = `/eapi/v1/depth`, `index` = `/eapi/v1/index`, `mark` = `/eapi/v1/mark` (array).
/// Index/mark are optional enrichments: the caller passes `None` if those requests failed.
pub fn parse_book(
    instrument: &Instrument,
    depth: &Value,
    index: Option<&Value>,
    mark: Option<&Value>,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    let index_price = match index {
        Some(i) => positive(opt_dec(P, i.get("indexPrice"), "indexPrice")?),
        None => None,
    };
    let mark_price = match mark {
        Some(m) => {
            let row = m
                .as_array()
                .and_then(|a| {
                    a.iter()
                        .find(|r| r["symbol"] == instrument.key.native_id.as_str())
                })
                .ok_or_else(|| invalid(P, "mark response does not contain the instrument"))?;
            positive(opt_dec(P, row.get("markPrice"), "markPrice")?)
        }
        None => None,
    };
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, depth, "bids", "depth", "level.size")?,
        asks: crate::pair_levels(P, depth, "asks", "depth", "level.size")?,
        source_ts: Some(ms(&depth["T"], "T")?),
        received_at,
        index_price,
        mark_price,
        provenance,
        access: BookAccess::Api,
    })
}

/// min(0.024 % of underlying notional at index, 10 % of premium), maker = taker (regular user).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let part = || FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate: FEE_RATE },
            FeeFormula::PctOfPremium { rate: FEE_CAP },
        ],
    };
    Ok(FeeSchedule {
        taker: part(),
        maker: part(),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Regular user 0.024% maker/taker of index * contract unit, capped at 10% of the option traded price (official fee page + FAQ). Exercise fee 0.015% (cap 10% of option value) not modelled. VIP tiers / promotions not modelled. Premium and fees in USDT. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for BinanceConnector {
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
        let v = self.get("/eapi/v1/exchangeInfo", &[]).await?;
        parse_instruments(&v, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let symbol = instrument.key.native_id.as_str();
        let underlying = format!("{}{QUOTE}", instrument.underlying);
        if let Some(s) = &self.stream {
            s.subscribe(symbol, &underlying);
            if let Some(b) = s.book(symbol) {
                let t0 = std::time::Instant::now();
                let (index, mark) = match s.mark(symbol) {
                    Some(m) => (
                        Some(json!({"indexPrice": m.index})),
                        Some(json!([{"symbol": symbol, "markPrice": m.mark}])),
                    ),
                    None => {
                        let (iq, mq) =
                            ([("underlying", underlying.as_str())], [("symbol", symbol)]);
                        let (i, m) = tokio::join!(
                            self.get("/eapi/v1/index", &iq),
                            self.get("/eapi/v1/mark", &mq),
                        );
                        (i.ok(), m.ok())
                    }
                };
                let network = t0.elapsed();
                let t1 = std::time::Instant::now();
                // received_at = arrival of the last frame on the book's connection (the pushed book
                // is current as of then); source_ts = venue event time of the last push.
                let book = parse_book(
                    instrument,
                    &b.depth,
                    index.as_ref(),
                    mark.as_ref(),
                    b.confirmed_at,
                    Provenance::LiveData,
                )?;
                s.note_served(true);
                return Ok(crate::MarketData {
                    book,
                    network,
                    normalize: t1.elapsed(),
                    path: "ws_stream",
                });
            }
        }
        let depth_q = [("symbol", symbol), ("limit", BOOK_DEPTH)];
        let index_q = [("underlying", underlying.as_str())];
        let mark_q = [("symbol", symbol)];
        let t0 = std::time::Instant::now();
        let (depth, index, mark) = tokio::join!(
            self.get("/eapi/v1/depth", &depth_q),
            self.get("/eapi/v1/index", &index_q),
            self.get("/eapi/v1/mark", &mark_q),
        );
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        // The book is mandatory; index/mark are enrichments (a failure leaves them None,
        // which makes notional-based fees `Unknown` rather than guessed).
        let depth = depth?;
        if let Some(s) = &self.stream {
            s.note_served(false);
            s.offer_seed(symbol, &depth, t0);
        }
        let book = parse_book(
            instrument,
            &depth,
            index.as_ref().ok(),
            mark.as_ref().ok(),
            received_at,
            Provenance::LiveData,
        )?;
        Ok(crate::MarketData::rest(book, network, t1))
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        fee_schedule(instrument)
    }

    fn prewarm(&self, instruments: &[Instrument]) {
        if let Some(s) = &self.stream {
            for i in instruments {
                s.subscribe(&i.key.native_id, &format!("{}{QUOTE}", i.underlying));
            }
        }
    }

    fn streams_books(&self) -> bool {
        self.stream.is_some()
    }

    fn diagnostics(&self) -> Option<serde_json::Value> {
        self.stream
            .as_ref()
            .map(|s| serde_json::to_value(s.stats()).unwrap_or_default())
    }

    /// NOT available: Binance European Options has no public REST endpoint giving best bid/ask WITH
    /// quantities for all symbols (checked live + docs 2026-10-06). `GET /eapi/v1/ticker` (weight 40
    /// for all symbols, 1,574 rows) has `bidPrice`/`askPrice` but NO bid/ask quantity (only
    /// `lastQty`/`volume`, which are trades); `GET /eapi/v1/mark` (weight 5) has `bidIV`/`askIV`/
    /// `markPrice`, no sizes; `/eapi/v1/bookTicker` and `/eapi/v1/ticker/bookTicker` do not exist
    /// (HTML 404). A top level without a size would be invented, so the bulk path is disabled and
    /// the per-symbol `/eapi/v1/depth` remains the only source of quotes. Explicit for clarity.
    async fn bulk_top_of_book(&self, _underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        Ok(None)
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
        "2026-10-06T09:00:00Z".parse().unwrap()
    }
    fn btc_call() -> Instrument {
        parse_instruments(&fixture("binance_exchange_info.json"), "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-261007-87000-C")
            .unwrap()
    }

    #[test]
    fn keeps_only_usdt_options_of_the_underlying() {
        let v = fixture("binance_exchange_info.json");
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let eth = parse_instruments(&v, "ETH", at()).unwrap();
        assert_eq!(btc.len(), 3);
        assert_eq!(eth.len(), 1);
        assert!(
            btc.iter()
                .all(|i| i.key.native_id.starts_with("BTC-") && i.premium_currency == "USDT")
        );
        let i = btc_call();
        assert_eq!(i.strike, Some(dec!(87000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-07T08:00:00+00:00");
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.tick_size, Some(dec!(5)));
        assert_eq!(i.amount_step, Some(dec!(0.01)));
        assert_eq!(i.settlement_asset.as_deref(), Some("USDT"));
        let put = btc
            .iter()
            .find(|i| i.key.native_id == "BTC-261007-86500-P")
            .unwrap();
        assert_eq!(put.option_kind, Some(OptionKind::Put));
        // Expired by then -> dropped.
        let later = "2026-10-08T00:00:00Z".parse().unwrap();
        assert_eq!(parse_instruments(&v, "BTC", later).unwrap().len(), 1);
    }

    #[test]
    fn book_index_mark_and_fees() {
        let i = btc_call();
        let b = parse_book(
            &i,
            &fixture("binance_depth_btc.json"),
            Some(&fixture("binance_index_btc.json")),
            Some(&fixture("binance_mark_btc.json")),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(140),
                size: dec!(91.71)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(135),
                size: dec!(0.5)
            }
        );
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(b.index_price, Some(dec!(86091.98826087)));
        assert_eq!(b.mark_price, Some(dec!(128.005)));
        assert_eq!(b.access, BookAccess::Api);

        let f = fee_schedule(&i).unwrap();
        // Cheap OTM call: cap binds (0.024% * 86091.99 = 20.66 > 10% * 140 = 14).
        let capped = f.taker.compute(&FeeInputs {
            qty_native: dec!(1),
            qty_underlying: dec!(1),
            premium: dec!(140),
            index_price: b.index_price,
            mark_price: b.mark_price,
        });
        assert_eq!(capped, FeeAmount::Known { amount: dec!(14.0) });
        // Official FAQ example: 3 ETHUSDT calls at 1,000, index 2,000 -> 1.44 USDT.
        let faq = f.taker.compute(&FeeInputs {
            qty_native: dec!(3),
            qty_underlying: dec!(3),
            premium: dec!(3000),
            index_price: Some(dec!(2000)),
            mark_price: None,
        });
        assert_eq!(faq, FeeAmount::Known { amount: dec!(1.44) });
    }

    #[test]
    fn mark_for_another_symbol_is_rejected() {
        let mut i = btc_call();
        i.key.native_id = "BTC-261007-86500-P".into();
        assert!(
            parse_book(
                &i,
                &fixture("binance_depth_btc.json"),
                None,
                Some(&fixture("binance_mark_btc.json")),
                at(),
                Provenance::Fixture,
            )
            .is_err()
        );
    }

    /// Why `bulk_top_of_book` is `None`: the only bulk quote endpoint (`/eapi/v1/ticker`, row
    /// recorded live 2026-10-06) has bid/ask PRICES but no bid/ask QUANTITY field.
    #[tokio::test]
    async fn no_bulk_tops_because_the_bulk_ticker_has_no_sizes() {
        let rows = fixture("binance_bulk_ticker_sample.json");
        let row = rows[0].as_object().unwrap();
        assert!(row.contains_key("bidPrice") && row.contains_key("askPrice"));
        assert!(
            !row.keys()
                .any(|k| k.contains("bidQty") || k.contains("askQty") || k.ends_with("Size"))
        );
        let c = BinanceConnector::new("http://127.0.0.1:9");
        assert!(c.bulk_top_of_book("BTC").await.unwrap().is_none());
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_binance_bulk_ticker_still_has_no_sizes() {
        let c = BinanceConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        let sym = cat[0].key.native_id.as_str();
        // Single-symbol variant (weight 1); if Binance ever adds quantities, revisit bulk tops.
        let v = c.get("/eapi/v1/ticker", &[("symbol", sym)]).await.unwrap();
        let row = v.as_array().and_then(|a| a.first()).unwrap();
        println!(
            "binance ticker row keys: {:?}",
            row.as_object().unwrap().keys()
        );
        assert!(row.get("bidPrice").is_some());
        assert!(row.get("bidQty").is_none() && row.get("askQty").is_none());
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_binance_eoptions() {
        let c = BinanceConnector::default();
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            assert!(!cat.is_empty());
            println!("binance {u}: {} instruments", cat.len());
        }
        let mut cat = c.list_instruments("BTC").await.unwrap();
        // Nearest expiry first: far-dated books are often empty.
        cat.sort_by_key(|i| (i.expiry, i.key.native_id.clone()));
        let i = &cat[0];
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "binance {} bid {:?} ask {:?} index {:?} mark {:?} levels {}/{} net {:?}",
            i.key.native_id,
            md.book.bids.first(),
            md.book.asks.first(),
            md.book.index_price,
            md.book.mark_price,
            md.book.bids.len(),
            md.book.asks.len(),
            md.network
        );
    }

    /// Stream vs REST on the 3 nearest-the-money BTCUSDT options of the nearest expiry: first pushed
    /// book latency after subscribing, then the same top of book from the stream and from REST.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_binance_stream_vs_rest() {
        let c = BinanceConnector::default().with_stream();
        let mut cat = c.list_instruments("BTC").await.unwrap();
        let idx = c
            .get("/eapi/v1/index", &[("underlying", "BTCUSDT")])
            .await
            .unwrap();
        let spot: Decimal = dec(P, &idx["indexPrice"], "indexPrice").unwrap();
        let first = cat.iter().filter_map(|i| i.expiry).min().unwrap();
        cat.retain(|i| i.expiry == Some(first));
        cat.sort_by_key(|i| (i.strike.unwrap() - spot).abs());
        let picks: Vec<Instrument> = cat.into_iter().take(3).collect();
        let s = c.stream.clone().unwrap();
        let t0 = std::time::Instant::now();
        c.prewarm(&picks);
        let mut first_push = vec![None; picks.len()];
        while t0.elapsed() < std::time::Duration::from_secs(15)
            && first_push.iter().any(|x| x.is_none())
        {
            for (k, i) in picks.iter().enumerate() {
                if first_push[k].is_none() && s.book(&i.key.native_id).is_some() {
                    first_push[k] = Some(t0.elapsed());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        println!(
            "first pushed book after subscribe: {first_push:?} (None = no change pushed in 15 s)"
        );
        // Quiet books: the first REST answer (sent >= 2 s after the ack) seeds the stream cache.
        let mut served = 0;
        for i in &picks {
            let sym = i.key.native_id.as_str();
            let a = c.get_market_data(i).await.unwrap();
            let b = c.get_market_data(i).await.unwrap();
            let rest = c
                .get("/eapi/v1/depth", &[("symbol", sym), ("limit", "20")])
                .await
                .unwrap();
            let r = parse_book(i, &rest, None, None, Utc::now(), Provenance::LiveData).unwrap();
            println!(
                "{sym}: call1 {} {:?} | call2 {} net {:?} age {} ms bid {:?} ask {:?} idx {:?} mark {:?} | REST bid {:?} ask {:?}",
                a.path,
                a.network,
                b.path,
                b.network,
                (Utc::now() - b.book.received_at).num_milliseconds(),
                b.book.bids.first(),
                b.book.asks.first(),
                b.book.index_price,
                b.book.mark_price,
                r.bids.first(),
                r.asks.first()
            );
            if b.path == "ws_stream" {
                served += 1;
                assert!(b.book.index_price.is_some() && b.book.mark_price.is_some());
                if let (Some(x), Some(y)) = (b.book.bids.first(), b.book.asks.first()) {
                    assert!(x.price < y.price, "crossed streamed book");
                }
            }
        }
        println!("diagnostics {}", c.diagnostics().unwrap());
        assert!(served >= 1, "no book served from the stream");
    }
}
