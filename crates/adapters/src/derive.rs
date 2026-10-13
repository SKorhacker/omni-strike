//! Derive (formerly Lyra) — **v3 stack** (`api.derive.xyz/v3`). v3 went live on mainnet
//! (Ethereum L1 zk-app, chainId 1) and replaced v2 (`api.lyra.finance`), whose accounts and
//! positions were snapshotted into v3. Execution: `derive_exec`, `derive_user`.
//! Source: https://docs.derive.xyz/migrating/breaking-changes (read 2026-10-09).
//!
//! Verified:
//! - `POST /public/get_all_instruments {currency, instrument_type:"option", expired:false,
//!   page, page_size}` [LIVE] (paginated; includes not-yet-active instruments -> filtered on
//!   `is_active`). The recorded v2 `public/get_instruments` fixtures share the entry shape.
//! - `POST /public/get_ticker {instrument_name}` -> slim ticker, index `I` & mark `M` [LIVE].
//! - Orderbook: WS only, channel `orderbook.{name}.{group}.{depth}`, full snapshots with an
//!   incrementing `publish_id` [LIVE]. We read ONE snapshot per request (no delta handling).
//! - Expiry unix seconds; amounts in base units (1 contract = 1 underlying); prices USDC per unit.

use crate::http::{client, read_json};
use crate::{Connector, ConnectorError, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub const MAINNET_REST: &str = "https://api.derive.xyz/v3";
/// Order books + tickers stream (`orderbook.*` / `ticker_slim.*`) [LIVE 2026-10-08].
pub const MAINNET_WS: &str = "wss://api.derive.xyz/v3/ws";
/// v3 mainnet: same tag as the mainnet non-custodial execution venue (`derive_user::mainnet`).
pub const NETWORK: &str = "derive-v3-mainnet";
/// Network tag of the retired v2 demo stack (still accepted by the TESTNET risk limits).
pub const TESTNET_NETWORK: &str = "derive-v2-testnet";
/// Bump when the mapping below changes.
pub const SPEC_VERSION: &str = "derive-v3/2026-10-09";
/// Price grouping 1 (no aggregation) and 100 levels per side (max documented depth).
const BOOK_GROUP: u32 = 1;
const BOOK_DEPTH: u32 = 100;
const WS_TIMEOUT: Duration = Duration::from_secs(8);

const FEE_SOURCE: &str = "https://legacy.docs.derive.xyz/reference/fees-1";
const FEE_READ_ON: &str = "2026-10-05";
/// [DOC legacy docs/settlement]: European, cash-settled in USDC at 08:00 UTC.
const SETTLEMENT_WINDOW: &str = "30-minute TWAP ending at expiry, sampled every minute";

const P: PlatformId = PlatformId::Derive;

pub struct DeriveConnector {
    http: reqwest::Client,
    rest: String,
    ws: String,
    /// Network label put in every instrument key: mainnet and testnet never collide.
    network: &'static str,
    /// Persistent subscription (preferred). `None` = one WS handshake per fetch.
    stream: Option<std::sync::Arc<crate::derive_stream::DeriveStream>>,
}

/// How long to wait for the first message of a new stream subscription before falling back
/// to a one-shot snapshot.
const STREAM_FIRST_WAIT: Duration = Duration::from_secs(4);
/// A streamed ticker older than this is not used for index/mark (REST fallback instead).
const STREAM_TICKER_MAX_AGE: Duration = Duration::from_secs(5);

impl Default for DeriveConnector {
    /// v3 MAINNET.
    fn default() -> Self {
        Self::new(MAINNET_REST, MAINNET_WS)
    }
}

impl DeriveConnector {
    pub fn new(rest: &str, ws: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            ws: ws.into(),
            network: NETWORK,
            stream: None,
        }
    }

    /// Derive v3 TESTNET (Sepolia): the stack with mintable test tokens and the official SDKs.
    /// Instruments tagged `derive-v3-testnet` (matches the v3 testnet execution venue).
    pub fn testnet_v3() -> Self {
        Self {
            network: crate::derive_exec::TESTNET_V3_NETWORK,
            ..Self::new(
                crate::derive_exec::TESTNET_REST,
                crate::derive_exec::TESTNET_WS,
            )
        }
    }

    /// Same connector, with a persistent WebSocket subscription manager (requires a Tokio runtime).
    pub fn with_stream(mut self) -> Self {
        self.stream = Some(crate::derive_stream::DeriveStream::spawn(&self.ws));
        self
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let resp = self
            .http
            .post(format!("{}/{method}", self.rest))
            .json(&params)
            .send()
            .await;
        let mut v = read_json(P, resp).await?;
        if let Some(err) = v.get("error") {
            return Err(invalid(P, format!("{method}: rpc error {err}")));
        }
        v.get_mut("result")
            .map(Value::take)
            .ok_or_else(|| invalid(P, format!("{method}: missing result")))
    }

    /// REST ticker normalised to `index_price` / `mark_price` (v3 answers the slim `I` / `M`).
    async fn ticker(&self, name: &str) -> Result<Value> {
        let t = self
            .rpc("public/get_ticker", json!({"instrument_name": name}))
            .await?;
        Ok(json!({"index_price": t["I"], "mark_price": t["M"]}))
    }

    async fn ws_book(&self, name: &str) -> Result<Value> {
        let channel = format!("orderbook.{name}.{BOOK_GROUP}.{BOOK_DEPTH}");
        let fut = async {
            let transport = |e: tokio_tungstenite::tungstenite::Error| ConnectorError::Transport {
                platform: P,
                source: Box::new(e),
            };
            let (mut ws, _) = tokio_tungstenite::connect_async(self.ws.as_str())
                .await
                .map_err(transport)?;
            let sub = json!({"id": 1, "method": "subscribe", "params": {"channels": [channel]}});
            ws.send(Message::text(sub.to_string()))
                .await
                .map_err(transport)?;
            while let Some(msg) = ws.next().await {
                let msg = msg.map_err(transport)?;
                let Message::Text(t) = msg else { continue };
                let v: Value = serde_json::from_str(t.as_str())
                    .map_err(|e| invalid(P, format!("ws frame not JSON: {e}")))?;
                if v.get("id") == Some(&json!(1)) {
                    let status = &v["result"]["status"][&channel];
                    if status != "ok" {
                        return Err(invalid(P, format!("subscribe {channel} refused: {v}")));
                    }
                    continue;
                }
                if v["params"]["channel"] == channel.as_str() {
                    let _ = ws.close(None).await;
                    return Ok(v["params"]["data"].clone());
                }
            }
            Err(invalid(
                P,
                "websocket closed before first orderbook snapshot",
            ))
        };
        tokio::time::timeout(WS_TIMEOUT, fut)
            .await
            .map_err(|_| ConnectorError::Timeout {
                platform: P,
                after: WS_TIMEOUT,
            })?
    }
}

/// Map one `get_instruments` entry. Returns `Ok(None)` for non-option or inactive entries.
pub fn parse_instrument(v: &Value) -> Result<Option<Instrument>> {
    if v["instrument_type"] != "option" || v["is_active"] != true {
        return Ok(None);
    }
    let name = v["instrument_name"]
        .as_str()
        .ok_or_else(|| invalid(P, "instrument_name"))?;
    let od = &v["option_details"];
    let kind = match od["option_type"].as_str() {
        Some("C") => OptionKind::Call,
        Some("P") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: option_type {other:?}"))),
    };
    let expiry_s = od["expiry"]
        .as_i64()
        .ok_or_else(|| invalid(P, format!("{name}: expiry")))?;
    let expiry = Utc
        .timestamp_opt(expiry_s, 0)
        .single()
        .ok_or_else(|| invalid(P, format!("{name}: expiry {expiry_s} out of range")))?;
    let underlying = v["base_currency"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: base_currency")))?;
    let quote = v["quote_currency"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: quote")))?;
    let index = od["index"].as_str().unwrap_or("?");
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &od["strike"], "strike")?),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: Decimal::ONE,
        premium_currency: quote.into(),
        collateral_assets: vec![quote.into()],
        settlement_asset: Some(quote.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("derive:{index}")),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        tick_size: opt_dec(P, v.get("tick_size"), "tick_size")?,
        min_amount: opt_dec(P, v.get("minimum_amount"), "minimum_amount")?,
        amount_step: opt_dec(P, v.get("amount_step"), "amount_step")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: v.clone(),
    }))
}

pub fn parse_instruments(result: &Value) -> Result<Vec<Instrument>> {
    let arr = result
        .as_array()
        .ok_or_else(|| invalid(P, "get_instruments: result not array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        if let Some(i) = parse_instrument(v)? {
            out.push(i);
        }
    }
    Ok(out)
}

/// Price string where "0" means "no level" (observed on `get_ticker` best_ask/best_bid).
fn positive(v: Option<Decimal>) -> Option<Decimal> {
    v.filter(|d| *d > Decimal::ZERO)
}

/// Build a snapshot from a WS `orderbook` data object + `get_ticker` result.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    ticker: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    if book["instrument_name"] != instrument.key.native_id.as_str() {
        return Err(invalid(
            P,
            format!(
                "book for {} instead of {}",
                book["instrument_name"], instrument.key.native_id
            ),
        ));
    }
    let ts = book["timestamp"]
        .as_i64()
        .ok_or_else(|| invalid(P, "book.timestamp"))?;
    Ok(BookSnapshot {
        access: oa_core::domain::BookAccess::Api,
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, book, "bids", "book", "level.size")?,
        asks: crate::pair_levels(P, book, "asks", "book", "level.size")?,
        source_ts: Utc.timestamp_millis_opt(ts).single(),
        received_at,
        index_price: positive(opt_dec(P, ticker.get("index_price"), "index_price")?),
        mark_price: positive(opt_dec(P, ticker.get("mark_price"), "mark_price")?),
        provenance,
    })
}

/// Taker: base_fee + min(taker_rate x notional, cap x mark value); maker without base fee.
/// Values are read per instrument from the API (they match the documented table).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let n = &instrument.native;
    let field = |k: &str| -> Result<Decimal> { dec(P, &n[k], k) };
    let (base, taker, maker, cap) = (
        field("base_fee")?,
        field("taker_fee_rate")?,
        field("maker_fee_rate")?,
        field("mark_price_fee_rate_cap")?,
    );
    let capped = |rate| FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate },
            FeeFormula::PctOfMarkValue { rate: cap },
        ],
    };
    Ok(FeeSchedule {
        taker: FeeFormula::Sum {
            parts: vec![FeeFormula::Fixed { amount: base }, capped(taker)],
        },
        maker: capped(maker),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: FEE_READ_ON.into(),
        notes: "Base fee assumed charged once per order (doc example: one $0.5 for a 2-contract \
                order); if charged per fill, a multi-level walk costs more. Cap modelled on mark \
                value per API field `mark_price_fee_rate_cap` (doc table says 'premium'). Volume/\
                staking discounts and builder extra_fees NOT modelled."
            .into(),
    })
}

#[async_trait]
impl Connector for DeriveConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.network
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        // Paginated, result.instruments.
        let mut all = Vec::new();
        let mut page = 1;
        loop {
            let r = self
                .rpc(
                    "public/get_all_instruments",
                    json!({"currency": underlying, "instrument_type": "option", "expired": false, "page": page, "page_size": 1000}),
                )
                .await?;
            all.extend(r["instruments"].as_array().cloned().unwrap_or_default());
            let pages = r["pagination"]["num_pages"].as_u64().unwrap_or(1);
            if page >= pages {
                break;
            }
            page += 1;
        }
        let mut v = parse_instruments(&Value::Array(all))?;
        for i in &mut v {
            i.key.network = self.network.into();
        }
        Ok(v)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let name = instrument.key.native_id.as_str();
        if let Some(stream) = &self.stream {
            use crate::derive_stream::Snapshot;
            let t0 = std::time::Instant::now();
            match stream.snapshot(name, STREAM_FIRST_WAIT).await {
                Snapshot::Ready { book, ticker } => {
                    let fresh_ticker = ticker
                        .filter(|t| t.mono.elapsed() <= STREAM_TICKER_MAX_AGE)
                        .map(|t| {
                            let it = &t.data["instrument_ticker"];
                            json!({"index_price": it["I"], "mark_price": it["M"]})
                        });
                    let ticker = match fresh_ticker {
                        Some(t) => t,
                        None => self.ticker(name).await?,
                    };
                    let network = t0.elapsed();
                    let t1 = std::time::Instant::now();
                    // received_at = when the WS message arrived: the true age of the data.
                    let snap = parse_book(
                        instrument,
                        &book.data,
                        &ticker,
                        book.received_at,
                        Provenance::LiveData,
                    )?;
                    return Ok(crate::MarketData {
                        book: snap,
                        network,
                        normalize: t1.elapsed(),
                        path: "ws_stream",
                    });
                }
                Snapshot::Rejected(reason) => {
                    return Err(ConnectorError::UnknownInstrument {
                        platform: P,
                        native_id: format!("{name} ({reason})"),
                    });
                }
                Snapshot::NotYet => {
                    // The stream is subscribing: do NOT open a one-shot WebSocket per instrument
                    // (bursts of handshakes get 503s and would open the venue's circuit). Report a
                    // transient, non-health error; the book is pushed by the stream shortly after.
                    return Err(ConnectorError::UpstreamStale {
                        platform: P,
                        detail: format!(
                            "{name}: stream warming up (subscribed, first book not received yet)"
                        ),
                    });
                }
            }
        }
        let t0 = std::time::Instant::now();
        let (book, ticker) = tokio::join!(self.ws_book(name), self.ticker(name));
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book?,
            &ticker?,
            received_at,
            Provenance::LiveData,
        )?;
        Ok(crate::MarketData {
            book,
            network,
            normalize: t1.elapsed(),
            path: "ws_one_shot",
        })
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

    fn diagnostics(&self) -> Option<serde_json::Value> {
        self.stream
            .as_ref()
            .map(|s| serde_json::to_value(s.stats()).unwrap_or_default())
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
        crate::fixture("recorded-2026-10-05", name)
    }

    fn btc() -> Vec<Instrument> {
        parse_instruments(&fixture("derive_v2_get_instruments_btc.json")["result"]).unwrap()
    }

    #[test]
    fn catalog_keeps_only_active_options() {
        let all = fixture("derive_v2_get_instruments_btc.json")["result"]
            .as_array()
            .unwrap()
            .len();
        let parsed = btc();
        assert_eq!(all, 760);
        assert_eq!(
            parsed.len(),
            642,
            "118 inactive instruments must be filtered out"
        );
    }

    #[test]
    fn normalisation_of_a_known_instrument() {
        let i = btc()
            .into_iter()
            .find(|i| i.key.native_id == "BTC-20261030-180000-C")
            .unwrap();
        assert_eq!(i.key.network, NETWORK);
        assert_eq!(i.underlying, "BTC");
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.strike, Some(dec!(180000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-30T08:00:00+00:00");
        assert_eq!(i.contract_multiplier, dec!(1));
        assert_eq!(i.min_amount, Some(dec!(0.01)));
        assert_eq!(i.amount_step, Some(dec!(0.00001)));
        assert_eq!(i.tick_size, Some(dec!(1)));
        assert_eq!(i.premium_currency, "USDC");
        assert_eq!(i.native["option_details"]["index"], "BTC-USD");
    }

    #[test]
    fn every_expiry_is_0800_utc() {
        for i in btc() {
            assert_eq!(
                i.expiry.unwrap().format("%H:%M:%S").to_string(),
                "08:00:00",
                "{}",
                i.key
            );
        }
    }

    #[test]
    fn name_and_fields_agree() {
        // Name format {CCY}-{YYYYMMDD}-{STRIKE}-{C|P} must match parsed fields.
        for i in btc() {
            let parts: Vec<_> = i.key.native_id.split('-').collect();
            assert_eq!(parts[1], i.expiry.unwrap().format("%Y%m%d").to_string());
            assert_eq!(parts[2].parse::<Decimal>().unwrap(), i.strike.unwrap());
            assert_eq!(parts[3] == "C", i.option_kind == Some(OptionKind::Call));
        }
    }

    #[test]
    fn ws_snapshot_and_ticker_build_a_book() {
        let eth =
            parse_instruments(&fixture("derive_v2_get_instruments_eth.json")["result"]).unwrap();
        let i = eth
            .into_iter()
            .find(|i| i.key.native_id == "ETH-20261030-2800-C")
            .unwrap();
        let frames = fixture("derive_v2_ws_orderbook.json");
        let data = &frames[1]["params"]["data"];
        let ticker = &fixture("derive_v2_get_ticker_ETH-20261030-2800-C.json")["result"];
        let b = parse_book(&i, data, ticker, Utc::now(), Provenance::Fixture).unwrap();
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(103.2),
                size: dec!(66.24)
            }
        );
        assert_eq!(b.bids.len(), 6);
        assert!(b.asks.windows(2).all(|w| w[0].price < w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price > w[1].price));
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791199941459);
        assert!(b.index_price.is_some() && b.mark_price.is_some());
        assert_eq!(b.provenance, Provenance::Fixture);
    }

    #[test]
    fn book_for_another_instrument_is_rejected() {
        let i = btc().remove(0);
        let frames = fixture("derive_v2_ws_orderbook.json");
        let r = parse_book(
            &i,
            &frames[1]["params"]["data"],
            &json!({}),
            Utc::now(),
            Provenance::Fixture,
        );
        assert!(matches!(r, Err(ConnectorError::InvalidResponse { .. })));
    }

    #[test]
    fn zero_ticker_price_means_absent_not_zero() {
        let i = btc().remove(0);
        let book =
            json!({"instrument_name": i.key.native_id, "timestamp": 1, "bids": [], "asks": []});
        let t = json!({"index_price": "0", "mark_price": "12"});
        let b = parse_book(&i, &book, &t, Utc::now(), Provenance::Fixture).unwrap();
        assert_eq!(b.index_price, None);
        assert_eq!(b.mark_price, Some(dec!(12)));
    }

    #[test]
    fn non_positive_level_is_invalid_not_dropped() {
        let i = btc().remove(0);
        let book = json!({"instrument_name": i.key.native_id, "timestamp": 1,
                          "bids": [], "asks": [["0", "1"]]});
        assert!(parse_book(&i, &book, &json!({}), Utc::now(), Provenance::Fixture).is_err());
    }

    #[test]
    fn fee_schedule_matches_documented_example() {
        // Doc: buy 2 ETH puts, spot 2200 -> $0.5 + 0.03% * 2 * 2200 = $1.82
        let eth =
            parse_instruments(&fixture("derive_v2_get_instruments_eth.json")["result"]).unwrap();
        let f = fee_schedule(&eth[0]).unwrap();
        let fee = f.taker.compute(&oa_core::fees::FeeInputs {
            qty_native: dec!(2),
            qty_underlying: dec!(2),
            premium: dec!(200),
            index_price: Some(dec!(2200)),
            mark_price: Some(dec!(100)),
        });
        assert_eq!(fee, oa_core::fees::FeeAmount::Known { amount: dec!(1.82) });
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_catalog_and_book() {
        let c = DeriveConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        assert!(cat.iter().all(|i| i.key.network == NETWORK));
        // Pick the nearest expiry with an ATM-ish strike: any active instrument works for shape.
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let b = c.get_market_data(i).await.unwrap().book;
        assert_eq!(b.provenance, Provenance::LiveData);
        assert!(b.index_price.is_some());
        eprintln!("{}: {} bids / {} asks", i.key, b.bids.len(), b.asks.len());
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live_testnet {
    use super::*;

    /// Read-only: Derive demo catalog + one streamed book, instruments tagged testnet.
    #[tokio::test]
    async fn live_testnet_catalog_and_stream_book() {
        let c = DeriveConnector::testnet_v3().with_stream();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        assert!(
            cat.iter()
                .all(|i| i.key.network == crate::derive_exec::TESTNET_V3_NETWORK)
        );
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let md = c.get_market_data(i).await.unwrap();
        eprintln!(
            "testnet {} via {}: {} bids / {} asks, age {} ms",
            i.key,
            md.path,
            md.book.bids.len(),
            md.book.asks.len(),
            (Utc::now() - md.book.received_at).num_milliseconds()
        );
        assert_eq!(md.path, "ws_stream");
    }

    /// Mainnet stream: second read is served from the live subscription without a new handshake.
    #[tokio::test]
    async fn live_mainnet_stream_is_reused() {
        let c = DeriveConnector::default().with_stream();
        let cat = c.list_instruments("BTC").await.unwrap();
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let first = c.get_market_data(i).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let second = c.get_market_data(i).await.unwrap();
        eprintln!(
            "first {:?} second {:?} ; diagnostics {}",
            first.network,
            second.network,
            c.diagnostics().unwrap()
        );
        assert_eq!(second.path, "ws_stream");
        assert!(second.network < std::time::Duration::from_millis(50));
        assert!(second.book.received_at >= first.book.received_at);
    }
}
