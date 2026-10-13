//! Bybit — USDT-settled BTC/ETH options, public REST v5, READ-ONLY. Centralised exchange (not
//! onchain): trading needs the user's own Bybit account (KYC) and API keys.
//!
//! Verified live 2026-10-06 (~09:56 UTC):
//! - `GET /v5/market/instruments-info?category=option&baseCoin=X&limit=1000`: BTC 724 symbols
//!   (670 `Trading` + 54 `Delivering`), ETH 600 (552 + 48), all on one page (`nextPageCursor` empty
//!   at limit 1000; default limit is 500 per the docs, so the cursor is followed anyway). EVERY
//!   symbol is `quoteCoin = settleCoin = USDT` (symbols `BTC-23OCT26-98000-C-USDT`); no USDC
//!   options were returned (a `settleCoin=USDC` query param is ignored). Fields: `optionsType`
//!   Call/Put, `deliveryTime` (ms, 08:00 UTC), `priceFilter.tickSize` (BTC 5, ETH 0.1),
//!   `lotSizeFilter.minOrderQty`/`qtyStep` (BTC 0.01, ETH 0.1), `deliveryFeeRate` 0.00015.
//!   The strike is NOT a field: it is parsed from the symbol (`BASE-EXPIRY-STRIKE-C|P-QUOTE`).
//! - `GET /v5/market/orderbook?category=option&symbol=X&limit=25` (option range [1, 25], default 1):
//!   `result.s`, `b` (desc) / `a` (asc) as `[price, size]` strings, `ts` / `cts` (ms). An unknown
//!   symbol returns `retCode 0` with `result: []` (rejected here as a mismatch).
//! - `GET /v5/market/tickers?category=option&symbol=X`: `indexPrice`, `underlyingPrice` (the
//!   expiry's forward/futures reference, not stored: BookSnapshot has no field for it), `markPrice`.
//! - Bulk: `GET /v5/market/tickers?category=option&baseCoin=BTC` (no symbol) = every option of the
//!   base coin in one response (BTC 674 rows, ~11:47 UTC), each with `bid1Price`/`bid1Size`/
//!   `ask1Price`/`ask1Size` (empty side = `"0"`/`"0"`), `indexPrice`, `markPrice`; only the
//!   envelope `time` (ms) as timestamp. Top equals `/v5/market/orderbook` level 0 (same symbol).
//!
//! Contract size: quantities are in units of the base coin (fee doc example "Traded Size: 0.3 BTC",
//! `minOrderQty` 0.01 for BTC), i.e. 1 native contract unit = 1 BTC/ETH -> multiplier 1. There is no
//! explicit `contractSize` field in the API response.
//!
//! Settlement [DOC https://www.bybit.com/en/help-center/article/Introduction-to-Bybit-Options]:
//! European, cash-settled, auto-exercised, margined and settled in USDT; final settlement price =
//! average index price over the 30 minutes before expiry.
//!
//! Fees [DOC https://www.bybit.com/en/help-center/article?language=en_US&id=000001544, updated
//! 2026-08-19; tiers on https://www.bybit.com/en/help-center/article/Trading-Fee-Structure]:
//! non-VIP taker 0.03 %, maker 0.02 % of the INDEX price per contract, capped at 7 % of the option
//! traded price. Delivery fee (BTC/ETH 0.015 %, cap 12.5 % of option value, none for dailies) is not
//! a trading cost and is not modelled.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Persistent public WebSocket (books + tickers), see `bybit_stream.rs`.
#[path = "bybit_stream.rs"]
pub mod stream;

pub use stream::{MAINNET_WS, TESTNET_WS};

/// How long a first request waits for the stream before the REST fallback (first live snapshot
/// arrived 163 ms after subscribe on 2026-10-07).
const STREAM_FIRST_WAIT: Duration = Duration::from_millis(1500);
/// A streamed ticker older than this is not used for index/mark (REST ticker instead).
const STREAM_TICKER_MAX_AGE: Duration = Duration::from_secs(5);

pub const MAINNET_REST: &str = "https://api.bybit.com";
pub const NETWORK: &str = "bybit-mainnet";
/// Bybit TESTNET public REST (pair with `network::OnNetwork(.., bybit_exec::TESTNET_NETWORK)`).
pub const TESTNET_REST: &str = "https://api-testnet.bybit.com";
pub const SPEC_VERSION: &str = "bybit/2026-10-06";
const BOOK_DEPTH: &str = "25";
const PAGE_LIMIT: &str = "1000";
const MAX_PAGES: usize = 20;
const FEE_SOURCE: &str = "https://www.bybit.com/en/help-center/article?language=en_US&id=000001544 (options: taker 0.03% / maker 0.02% of index price, cap 7% of option price)";
const SETTLEMENT_WINDOW: &str = "average Bybit index price over the 30 minutes before expiry";

const P: PlatformId = PlatformId::Bybit;

pub struct BybitConnector {
    http: reqwest::Client,
    rest: String,
    ws: String,
    /// Persistent WebSocket subscription manager (preferred). `None` = REST only.
    stream: Option<Arc<stream::BybitStream>>,
}

impl Default for BybitConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl BybitConnector {
    /// REST base URL; the WebSocket host follows it (testnet REST -> testnet WS, else mainnet).
    pub fn new(rest: &str) -> Self {
        let rest = rest.trim_end_matches('/');
        let ws = if rest == TESTNET_REST {
            TESTNET_WS
        } else {
            MAINNET_WS
        };
        Self {
            http: client(),
            rest: rest.into(),
            ws: ws.into(),
            stream: None,
        }
    }

    /// Bybit TESTNET public REST + WS (instrument keys still say `bybit-mainnet`: wrap in
    /// `network::OnNetwork(.., bybit_exec::TESTNET_NETWORK)` as the server does).
    pub fn testnet() -> Self {
        Self::new(TESTNET_REST)
    }

    /// Override the WebSocket URL (tests, proxies).
    pub fn with_ws_url(mut self, ws: &str) -> Self {
        self.ws = ws.into();
        self
    }

    /// Same connector with a persistent WebSocket book stream (requires a Tokio runtime).
    /// `get_market_data` then serves from the stream and falls back to REST when it is not ready.
    pub fn with_stream(mut self) -> Self {
        self.stream = Some(stream::BybitStream::spawn(&self.ws));
        self
    }

    async fn rest_ticker(&self, sym: &str) -> Result<Value> {
        self.get(
            "/v5/market/tickers",
            &[("category", "option"), ("symbol", sym)],
        )
        .await
    }

    /// Whole v5 envelope (`retCode`, `result`, `time`, ...), not yet checked.
    async fn get_envelope(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        get_json(&self.http, P, format!("{}{path}", self.rest), query).await
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let v = self.get_envelope(path, query).await?;
        unwrap_result(path, v)
    }
}

/// Bybit v5 envelope: `{retCode, retMsg, result, ...}`; anything but `retCode 0` is an error.
fn unwrap_result(path: &str, mut v: Value) -> Result<Value> {
    match v["retCode"].as_i64() {
        Some(0) => v
            .get_mut("result")
            .map(Value::take)
            .ok_or_else(|| invalid(P, format!("{path}: no result"))),
        _ => Err(invalid(
            P,
            format!("{path}: retCode {} {}", v["retCode"], v["retMsg"]),
        )),
    }
}

fn ms(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    crate::ts_ms(P, v.as_str().map_or(v.as_i64(), |s| s.parse().ok()), field)
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    // Only live order books: skip PreLaunch / Delivering / Closed.
    if v["baseCoin"] != underlying || v["status"] != "Trading" {
        return Ok(None);
    }
    let name = v["symbol"].as_str().ok_or_else(|| invalid(P, "symbol"))?;
    let kind = match v["optionsType"].as_str() {
        Some("Call") => OptionKind::Call,
        Some("Put") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: optionsType {other:?}"))),
    };
    let expiry = ms(&v["deliveryTime"], "deliveryTime")?;
    if expiry <= now {
        return Ok(None);
    }
    // Symbol layout verified live: BASE-DMMMYY-STRIKE-C|P-QUOTE (e.g. BTC-23OCT26-98000-C-USDT).
    let parts: Vec<&str> = name.split('-').collect();
    let (strike, cp) = match parts.as_slice() {
        [base, _exp, strike, cp, _quote] if *base == underlying => (*strike, *cp),
        _ => return Err(invalid(P, format!("{name}: unexpected symbol layout"))),
    };
    if !matches!((cp, kind), ("C", OptionKind::Call) | ("P", OptionKind::Put)) {
        return Err(invalid(P, format!("{name}: symbol/optionsType mismatch")));
    }
    let strike = dec(P, &Value::String(strike.into()), "strike")?;
    let quote = v["quoteCoin"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: quoteCoin")))?;
    let settle = v["settleCoin"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: settleCoin")))?;
    let lot = &v["lotSizeFilter"];
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(strike.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        // Quantity is in base-coin units (docs: "Traded Size: 0.3 BTC"); no contractSize field.
        contract_multiplier: Decimal::ONE,
        // Exact coin as reported (USDT on 2026-10-06), NOT assumed equal to USDC.
        premium_currency: quote.into(),
        collateral_assets: vec![settle.into()],
        settlement_asset: Some(settle.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("bybit:{underlying}-index")),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        tick_size: opt_dec(P, v["priceFilter"].get("tickSize"), "tickSize")?,
        min_amount: opt_dec(P, lot.get("minOrderQty"), "minOrderQty")?,
        amount_step: opt_dec(P, lot.get("qtyStep"), "qtyStep")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "symbolId": v["symbolId"], "displayName": v["displayName"], "status": v["status"],
            "quoteCoin": quote, "settleCoin": settle, "deliveryFeeRate": v["deliveryFeeRate"],
            "maxOrderQty": lot["maxOrderQty"], "minPrice": v["priceFilter"]["minPrice"],
            "venue_type": "centralised_exchange", "account_required": "user's own Bybit account + API keys (KYC)"
        }),
    }))
}

/// Parse the `result` object of one instruments-info page.
pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v["list"]
        .as_array()
        .ok_or_else(|| invalid(P, "instruments-info: list missing"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

/// `book` = orderbook `result`, `ticker` = tickers `result` (list with one entry).
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    ticker: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let id = instrument.key.native_id.as_str();
    if book["s"] != id {
        return Err(invalid(P, "orderbook symbol does not match the request"));
    }
    let t = ticker["list"]
        .as_array()
        .and_then(|l| l.iter().find(|x| x["symbol"] == id))
        .ok_or_else(|| invalid(P, "tickers: requested symbol missing"))?;
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, book, "b", "orderbook", "level.size")?,
        asks: crate::pair_levels(P, book, "a", "orderbook", "level.size")?,
        source_ts: Some(ms(&book["ts"], "ts")?),
        received_at,
        index_price: positive(opt_dec(P, t.get("indexPrice"), "indexPrice")?),
        mark_price: positive(opt_dec(P, t.get("markPrice"), "markPrice")?),
        provenance,
        access: BookAccess::Api,
    })
}

/// One side of a bulk ticker row: `None` when the side is empty (Bybit sends `"0"`/`"0"`, verified
/// live 2026-10-06) or not strictly positive. Never invents a level.
fn top_level(row: &Value, price: &str, size: &str) -> Result<Option<Level>> {
    let p = opt_dec(P, row.get(price), price)?;
    let s = opt_dec(P, row.get(size), size)?;
    Ok(match (p, s) {
        (Some(price), Some(size)) if price > Decimal::ZERO && size > Decimal::ZERO => {
            Some(Level { price, size })
        }
        _ => None,
    })
}

/// `v` = the WHOLE envelope of `GET /v5/market/tickers?category=option&baseCoin=X` (the top-level
/// `time`, ms, is the only timestamp: rows carry none). Rows give `bid1Price`/`bid1Size`/
/// `ask1Price`/`ask1Size` in the same units as `/v5/market/orderbook` (USDT per contract, size in
/// base-coin contracts; checked equal to the orderbook top live 2026-10-06) and `indexPrice`/
/// `markPrice` exactly as `parse_book` reads them from the single-symbol ticker.
pub fn parse_bulk_tops(
    v: &Value,
    underlying: &str,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<Vec<BookSnapshot>> {
    if v["retCode"].as_i64() != Some(0) {
        return Err(invalid(
            P,
            format!("tickers: retCode {} {}", v["retCode"], v["retMsg"]),
        ));
    }
    let source_ts = ms(&v["time"], "time")?;
    let arr = v["result"]["list"]
        .as_array()
        .ok_or_else(|| invalid(P, "tickers: list missing"))?;
    let prefix = format!("{underlying}-");
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    let mut out = Vec::with_capacity(arr.len());
    for row in arr {
        let sym = row["symbol"]
            .as_str()
            .ok_or_else(|| invalid(P, "tickers: symbol"))?;
        if !sym.starts_with(&prefix) {
            continue;
        }
        out.push(BookSnapshot {
            instrument: InstrumentKey {
                platform: P,
                network: NETWORK.into(),
                native_id: sym.into(),
            },
            bids: top_level(row, "bid1Price", "bid1Size")?
                .into_iter()
                .collect(),
            asks: top_level(row, "ask1Price", "ask1Size")?
                .into_iter()
                .collect(),
            source_ts: Some(source_ts),
            received_at,
            index_price: positive(opt_dec(P, row.get("indexPrice"), "indexPrice")?),
            mark_price: positive(opt_dec(P, row.get("markPrice"), "markPrice")?),
            provenance,
            access: BookAccess::Api,
        });
    }
    Ok(out)
}

/// min(rate x index, 7 % x option price) x size, non-VIP rates from the fee page.
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let part = |rate: Decimal| FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate },
            FeeFormula::PctOfPremium {
                rate: Decimal::new(7, 2),
            },
        ],
    };
    Ok(FeeSchedule {
        taker: part(Decimal::new(3, 4)),
        maker: part(Decimal::new(2, 4)),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Non-VIP: taker 0.03%, maker 0.02% of index price per contract, capped at 7% of the option traded price. VIP / Pro tiers (lower rates, Pro caps 4-7%) not modelled; regional rates may differ. Delivery fee (BTC/ETH 0.015%, cap 12.5% of option value) not included. Paid in USDT. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for BybitConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let now = Utc::now();
        let mut out = vec![];
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let mut q = vec![
                ("category", "option"),
                ("baseCoin", underlying),
                ("limit", PAGE_LIMIT),
            ];
            if !cursor.is_empty() {
                q.push(("cursor", cursor.as_str()));
            }
            let v = self.get("/v5/market/instruments-info", &q).await?;
            out.extend(parse_instruments(&v, underlying, now)?);
            let empty = v["list"].as_array().is_none_or(|l| l.is_empty());
            match v["nextPageCursor"].as_str() {
                Some(c) if !c.is_empty() && !empty => cursor = c.into(),
                _ => return Ok(out),
            }
        }
        Err(invalid(P, "instruments-info: too many pages"))
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let sym = instrument.key.native_id.as_str();
        if let Some(s) = &self.stream {
            let t0 = std::time::Instant::now();
            match s
                .snapshot(sym, STREAM_FIRST_WAIT, STREAM_TICKER_MAX_AGE)
                .await
            {
                stream::Snapshot::Ready {
                    book,
                    ticker,
                    confirmed_at,
                } => {
                    let ticker = match ticker {
                        Some(t) => t,
                        None => self.rest_ticker(sym).await?,
                    };
                    let network = t0.elapsed();
                    let t1 = std::time::Instant::now();
                    // received_at = arrival of the last frame on the book's connection (the book is
                    // exactly current then); source_ts = venue `ts` of the last book change.
                    let book = parse_book(
                        instrument,
                        &book,
                        &ticker,
                        confirmed_at,
                        Provenance::LiveData,
                    )?;
                    return Ok(crate::MarketData {
                        book,
                        network,
                        normalize: t1.elapsed(),
                        path: "ws_stream",
                    });
                }
                stream::Snapshot::Rejected(reason) => {
                    return Err(crate::ConnectorError::UnknownInstrument {
                        platform: P,
                        native_id: format!("{sym} ({reason})"),
                    });
                }
                stream::Snapshot::NotYet => {
                    tracing::debug!(instrument = sym, "bybit stream not ready, REST fallback");
                }
            }
        }
        let t0 = std::time::Instant::now();
        let book_q = [
            ("category", "option"),
            ("symbol", sym),
            ("limit", BOOK_DEPTH),
        ];
        let ticker_q = [("category", "option"), ("symbol", sym)];
        let (book, ticker) = tokio::join!(
            self.get("/v5/market/orderbook", &book_q),
            self.get("/v5/market/tickers", &ticker_q),
        );
        let (book, ticker) = (book?, ticker?);
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book,
            &ticker,
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
                s.subscribe(&i.key.native_id);
            }
        }
    }

    fn streams_books(&self) -> bool {
        self.stream.is_some()
    }

    fn diagnostics(&self) -> Option<Value> {
        self.stream
            .as_ref()
            .map(|s| serde_json::to_value(s.stats()).unwrap_or_default())
    }

    /// ONE request: `/v5/market/tickers?category=option&baseCoin=X` (BTC 674 rows, 2026-10-06).
    /// One call per underlying every 2 s (default) is far below Bybit's per-IP HTTP limit.
    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        let q = [("category", "option"), ("baseCoin", underlying)];
        let v = self.get_envelope("/v5/market/tickers", &q).await?;
        parse_bulk_tops(&v, underlying, Utc::now(), Provenance::LiveData).map(Some)
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
    fn result(name: &str) -> Value {
        unwrap_result(name, fixture(name)).unwrap()
    }
    const SYM: &str = "BTC-23OCT26-98000-C-USDT";

    #[test]
    fn keeps_only_trading_options_of_the_underlying() {
        let v = result("bybit_instruments_btc.json");
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        // 4 recorded rows: 3 Trading + 1 Delivering (today's expiry) which is skipped.
        assert_eq!(btc.len(), 3);
        assert!(parse_instruments(&v, "ETH", at()).unwrap().is_empty());
        let eth = parse_instruments(&result("bybit_instruments_eth.json"), "ETH", at()).unwrap();
        assert_eq!(eth.len(), 2);
        assert_eq!(eth[0].tick_size, Some(dec!(0.1)));
        let i = btc.iter().find(|i| i.key.native_id == SYM).unwrap();
        assert_eq!(i.strike, Some(dec!(98000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-23T08:00:00+00:00");
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.premium_currency, "USDT");
        assert_eq!(i.settlement_asset.as_deref(), Some("USDT"));
        assert_eq!(i.tick_size, Some(dec!(5)));
        assert_eq!(i.min_amount, Some(dec!(0.01)));
        assert_eq!(i.amount_step, Some(dec!(0.01)));
    }

    #[test]
    fn book_and_fees() {
        let i = parse_instruments(&result("bybit_instruments_btc.json"), "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == SYM)
            .unwrap();
        let b = parse_book(
            &i,
            &result("bybit_book_btc.json"),
            &result("bybit_ticker_btc.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(165),
                size: dec!(67.4)
            }
        );
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(160),
                size: dec!(15.86)
            }
        );
        assert_eq!(b.index_price, Some(dec!(86008.50310121)));
        assert_eq!(b.mark_price, Some(dec!(156.03699422)));
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791280839454);

        // Unknown symbol: Bybit answers retCode 0 with `result: []` -> must be rejected.
        assert!(
            parse_book(
                &i,
                &serde_json::json!([]),
                &result("bybit_ticker_btc.json"),
                at(),
                Provenance::Fixture
            )
            .is_err()
        );

        let f = fee_schedule(&i).unwrap();
        let fee = |formula: &FeeFormula, premium| {
            formula.compute(&oa_core::fees::FeeInputs {
                qty_native: dec!(0.3),
                qty_underlying: dec!(0.3),
                premium,
                index_price: Some(dec!(42000)),
                mark_price: None,
            })
        };
        // Doc examples (maker): min(0.02% x 42000, 7% x 3000) x 0.3 = 2.52; min(8.4, 7% x 30) x 0.3 = 0.63.
        // `premium` is the total premium traded (price x size).
        assert_eq!(
            fee(&f.maker, dec!(900)),
            oa_core::fees::FeeAmount::Known { amount: dec!(2.52) }
        );
        assert_eq!(
            fee(&f.maker, dec!(9)),
            oa_core::fees::FeeAmount::Known { amount: dec!(0.63) }
        );
        // Taker: min(0.03% x 42000 = 12.6, 210) x 0.3 = 3.78.
        assert_eq!(
            fee(&f.taker, dec!(900)),
            oa_core::fees::FeeAmount::Known { amount: dec!(3.78) }
        );
    }

    #[test]
    fn non_zero_ret_code_is_an_error() {
        let v = serde_json::json!({"retCode": 10001, "retMsg": "params error", "result": {}});
        assert!(unwrap_result("x", v).is_err());
    }

    /// Recorded live 2026-10-06 ~11:48 UTC within ~5 s: bulk tickers (trimmed to 3 rows), then the
    /// orderbook + single ticker + instruments-info of BTC-25DEC26-85000-C-USDT.
    #[test]
    fn bulk_tops_match_the_connector_book() {
        const S: &str = "BTC-25DEC26-85000-C-USDT";
        let tops = parse_bulk_tops(
            &fixture("bybit_bulk_tickers_btc.json"),
            "BTC",
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(tops.len(), 3);
        // ETH request on a BTC payload: nothing leaks across underlyings.
        assert!(
            parse_bulk_tops(
                &fixture("bybit_bulk_tickers_btc.json"),
                "ETH",
                at(),
                Provenance::Fixture
            )
            .unwrap()
            .is_empty()
        );
        let bulk = tops.iter().find(|b| b.instrument.native_id == S).unwrap();

        // The connector's own full-book path for the same instrument.
        let inst = parse_instruments(&result("bybit_bulk_instrument.json"), "BTC", at())
            .unwrap()
            .pop()
            .unwrap();
        let full = parse_book(
            &inst,
            &result("bybit_bulk_book.json"),
            &result("bybit_bulk_ticker.json"),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // Same key (platform, network, native_id) as the catalogue / full book.
        assert_eq!(bulk.instrument, inst.key);
        assert_eq!(bulk.instrument, full.instrument);
        // Worked example: bulk bid 7100 x 7.06, ask 7160 x 7.06 (USDT per 1-BTC contract, size in
        // contracts) == orderbook b[0] / a[0]. Cost of buying 1 contract = 7160 USDT either way.
        let bid = Level {
            price: dec!(7100),
            size: dec!(7.06),
        };
        let ask = Level {
            price: dec!(7160),
            size: dec!(7.06),
        };
        assert_eq!(bulk.bids, vec![bid]);
        assert_eq!(bulk.asks, vec![ask]);
        assert_eq!(full.bids[0], bid);
        assert_eq!(full.asks[0], ask);
        assert_eq!(bulk.asks[0].price * inst.contract_multiplier, dec!(7160));
        // Index identical; mark drifted by 0.39 USDT between the two recordings (same semantics).
        assert_eq!(bulk.index_price, Some(dec!(86317.80901619)));
        assert_eq!(bulk.index_price, full.index_price);
        assert_eq!(bulk.mark_price, Some(dec!(7140.79161262)));
        assert_eq!(full.mark_price, Some(dec!(7141.18356177)));
        assert_eq!(bulk.source_ts.unwrap().timestamp_millis(), 1791287285308);
        assert_eq!(bulk.access, BookAccess::Api);
        assert_eq!(bulk.provenance, Provenance::Fixture);

        // Empty bid side ("0"/"0") is dropped, never turned into a level.
        let one_sided = tops
            .iter()
            .find(|b| b.instrument.native_id == "BTC-8OCT26-93000-C-USDT")
            .unwrap();
        assert!(one_sided.bids.is_empty());
        assert_eq!(
            one_sided.asks,
            vec![Level {
                price: dec!(10),
                size: dec!(30.99)
            }]
        );

        let bad = serde_json::json!({"retCode": 10001, "retMsg": "x", "result": {}, "time": 1});
        assert!(parse_bulk_tops(&bad, "BTC", at(), Provenance::Fixture).is_err());
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_bybit_bulk_tops() {
        let c = BybitConnector::default();
        for u in ["BTC", "ETH"] {
            let tops = c.bulk_top_of_book(u).await.unwrap().unwrap();
            let two: Vec<_> = tops
                .iter()
                .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
                .collect();
            println!(
                "bybit bulk {u}: {} rows, {} two-sided; e.g. {} bid {:?} ask {:?} index {:?}",
                tops.len(),
                two.len(),
                two[0].instrument.native_id,
                two[0].bids[0],
                two[0].asks[0],
                two[0].index_price
            );
            assert!(!two.is_empty());
        }
        // Bulk top vs the connector's own full book for one liquid instrument.
        let cat = c.list_instruments("BTC").await.unwrap();
        let tops = c.bulk_top_of_book("BTC").await.unwrap().unwrap();
        let b = tops
            .iter()
            .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
            .max_by_key(|b| b.bids[0].size.min(b.asks[0].size))
            .unwrap();
        let i = cat.iter().find(|i| i.key == b.instrument).unwrap();
        let md = c.get_market_data(i).await.unwrap();
        let (fb, fa) = (md.book.bids[0], md.book.asks[0]);
        println!(
            "bybit {} bulk {:?}/{:?} full {:?}/{:?}",
            i.key.native_id, b.bids[0], b.asks[0], fb, fa
        );
        // Allow drift between two requests: within 5 % of the price.
        let close = |x: Decimal, y: Decimal| (x - y).abs() <= y * dec!(0.05);
        assert!(close(b.bids[0].price, fb.price) && close(b.asks[0].price, fa.price));
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_bybit_options() {
        let c = BybitConnector::default();
        for u in ["BTC", "ETH"] {
            let cat = c.list_instruments(u).await.unwrap();
            assert!(!cat.is_empty());
            println!(
                "bybit {u}: {} trading options, premium {}",
                cat.len(),
                cat[0].premium_currency
            );
        }
        let cat = c.list_instruments("BTC").await.unwrap();
        let i = &cat[cat.len() / 2];
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "bybit {} bid {:?} ask {:?} index {:?} mark {:?} network {:?}",
            i.key.native_id,
            md.book.bids.first(),
            md.book.asks.first(),
            md.book.index_price,
            md.book.mark_price,
            md.network
        );
    }
}
