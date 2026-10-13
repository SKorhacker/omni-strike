//! Aevo — public REST only in V1.
//!
//! Verified:
//! - `GET /markets?asset=BTC&instrument_type=OPTION` [LIVE]; expiry = unix NANOSECONDS (string).
//! - `GET /orderbook?instrument_name=...` = full snapshot `[price, amount, iv]`, `last_updated` ns [LIVE].
//!   (`/orderbook/{name}` is 404.)
//! - `GET /instrument/{name}` -> `index_price`, `mark_price` [LIVE].
//! - 1 contract = 1 underlying [DOC fee examples: notional = qty x index].
//!
//! The WS `orderbook-100ms` channel sends deltas without a sequence number; its checksum
//! has not been verified by us -> NOT used. REST snapshots only.

use crate::http::{client, get_json};
use crate::{Connector, ConnectorError, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;

pub const MAINNET_REST: &str = "https://api.aevo.xyz";
pub const NETWORK: &str = "aevo-mainnet";
/// Aevo testnet — verified live 2026-10-05: 946 BTC options listed.
pub const TESTNET_REST: &str = "https://api-testnet.aevo.xyz";
pub const TESTNET_NETWORK: &str = "aevo-testnet";
pub const SPEC_VERSION: &str = "aevo/2026-10-05";

const FEE_SOURCE: &str = "https://www.aevo.xyz/docs/aevo-products/aevo-exchange/fees/options-fees";
const FEE_READ_ON: &str = "2026-10-05";

const P: PlatformId = PlatformId::Aevo;
const STALE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_millis(300);

fn is_stale_data(e: &ConnectorError) -> bool {
    matches!(e, ConnectorError::Http { status: 503, body, .. } if body.contains("STALE_DATA"))
}

pub struct AevoConnector {
    http: reqwest::Client,
    rest: String,
    network: &'static str,
}

impl Default for AevoConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl AevoConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            network: NETWORK,
        }
    }

    /// Aevo testnet endpoints, instruments tagged `aevo-testnet`.
    pub fn testnet() -> Self {
        Self {
            network: TESTNET_NETWORK,
            ..Self::new(TESTNET_REST)
        }
    }

    async fn get_once(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        get_json(&self.http, P, format!("{}{path}", self.rest), query).await
    }

    /// `503 {"error":"STALE_DATA"}` (observed live 2026-10-05) is retried ONCE after a short
    /// pause; if it persists it becomes a typed `UpstreamStale` error.
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        match self.get_once(path, query).await {
            Err(e) if is_stale_data(&e) => {
                tokio::time::sleep(STALE_RETRY_AFTER).await;
                self.get_once(path, query).await.map_err(|e| {
                    if is_stale_data(&e) {
                        ConnectorError::UpstreamStale {
                            platform: P,
                            detail: format!("{path}: 503 STALE_DATA (after 1 retry)"),
                        }
                    } else {
                        e
                    }
                })
            }
            other => other,
        }
    }
}

fn ns_to_utc(ns: &Value, field: &str) -> Result<DateTime<Utc>> {
    let n: i64 = ns
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid(P, format!("{field}: expected ns integer string, got {ns}")))?;
    Ok(Utc.timestamp_nanos(n))
}

pub fn parse_market(v: &Value) -> Result<Option<Instrument>> {
    if v["instrument_type"] != "OPTION" || v["is_active"] != true {
        return Ok(None);
    }
    let name = v["instrument_name"]
        .as_str()
        .ok_or_else(|| invalid(P, "instrument_name"))?;
    let kind = match v["option_type"].as_str() {
        Some("call") => OptionKind::Call,
        Some("put") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: option_type {other:?}"))),
    };
    let underlying = v["underlying_asset"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: underlying")))?;
    let quote = v["quote_asset"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: quote")))?;
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["strike"], "strike")?),
        expiry: Some(ns_to_utc(&v["expiry"], "expiry")?),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: Decimal::ONE,
        premium_currency: quote.into(),
        collateral_assets: vec![quote.into()],
        settlement_asset: Some(quote.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("aevo:{underlying} index (median of CEX pairs)")),
        // Not documented; settlement timestamps suggest point-in-time. Kept unknown.
        settlement_window: None,
        tick_size: opt_dec(P, v.get("price_step"), "price_step")?,
        // `min_order_value` semantics are not documented -> not used as a minimum amount.
        min_amount: None,
        amount_step: opt_dec(P, v.get("amount_step"), "amount_step")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: v.clone(),
    }))
}

pub fn parse_markets(v: &Value) -> Result<Vec<Instrument>> {
    let arr = v
        .as_array()
        .ok_or_else(|| invalid(P, "markets: not an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        if let Some(i) = parse_market(m)? {
            out.push(i);
        }
    }
    Ok(out)
}

/// `book` = `/orderbook` response, `inst` = `/instrument/{name}` response.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    inst: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    if book["type"] != "snapshot" {
        return Err(invalid(
            P,
            format!("expected a snapshot, got type {}", book["type"]),
        ));
    }
    if book["instrument_name"] != instrument.key.native_id.as_str()
        || inst["instrument_name"] != instrument.key.native_id.as_str()
    {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        access: oa_core::domain::BookAccess::Api,
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, book, "bids", "orderbook", "level.amount")?,
        asks: crate::pair_levels(P, book, "asks", "orderbook", "level.amount")?,
        source_ts: Some(ns_to_utc(&book["last_updated"], "last_updated")?),
        received_at,
        index_price: positive(opt_dec(P, inst.get("index_price"), "index_price")?),
        mark_price: positive(opt_dec(P, inst.get("mark_price"), "mark_price")?),
        provenance,
    })
}

/// [DOC] taker 0.05% / maker 0.03% of notional (qty x index), never above 12.5% of the
/// option price (doc example uses the traded price).
pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let capped = |rate| FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate },
            FeeFormula::PctOfPremium {
                rate: Decimal::new(125, 3),
            },
        ],
    };
    FeeSchedule {
        taker: capped(Decimal::new(5, 4)),
        maker: capped(Decimal::new(3, 4)),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: FEE_READ_ON.into(),
        notes: "Trade fee only. Settlement fee (0.015% of notional for ITM options, cap 12.5% of \
                value, not on dailies) is a conditional expiry cost, not included. Builder fees \
                not modelled."
            .into(),
    }
}

#[async_trait]
impl Connector for AevoConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.network
    }
    /// Keep-warm every 3 s: Aevo answered 429 (retry ~0.5 s) at 1 s with several hot books (2026-10-06).
    fn hot_refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(3)
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self
            .get(
                "/markets",
                &[("asset", underlying), ("instrument_type", "OPTION")],
            )
            .await?;
        let mut out = parse_markets(&v)?;
        for i in &mut out {
            i.key.network = self.network.into();
        }
        Ok(out)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let name = instrument.key.native_id.as_str();
        let path = format!("/instrument/{name}");
        let query = [("instrument_name", name)];
        let t0 = std::time::Instant::now();
        let (book, inst) = tokio::join!(self.get("/orderbook", &query), self.get(&path, &[]));
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(
            instrument,
            &book?,
            &inst?,
            received_at,
            Provenance::LiveData,
        )?;
        Ok(crate::MarketData::rest(book, network, t1))
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        Ok(fee_schedule(instrument))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConnectorError;
    use oa_core::fees::{FeeAmount, FeeInputs};
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        crate::fixture("recorded-2026-10-05", name)
    }

    #[test]
    fn catalog_parses_every_option() {
        let all = parse_markets(&fixture("aevo_markets_btc.json")).unwrap();
        assert_eq!(all.len(), 946);
        let eth = parse_markets(&fixture("aevo_markets_eth.json")).unwrap();
        assert_eq!(eth.len(), 786);
    }

    #[test]
    fn nanosecond_expiry_and_normalisation() {
        let all = parse_markets(&fixture("aevo_markets_btc.json")).unwrap();
        let i = all
            .iter()
            .find(|i| i.key.native_id == "BTC-06OCT26-75000-P")
            .unwrap();
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-06T08:00:00+00:00");
        assert_eq!(i.option_kind, Some(OptionKind::Put));
        assert_eq!(i.strike, Some(dec!(75000)));
        assert_eq!(i.tick_size, Some(dec!(2.5)));
        assert_eq!(i.amount_step, Some(dec!(0.01)));
        assert_eq!(i.min_amount, None, "min_order_value semantics unverified");
        assert_eq!(i.settlement_window, None);
        assert_eq!(i.native["instrument_id"], "142360");
    }

    #[test]
    fn every_expiry_is_0800_utc() {
        for i in parse_markets(&fixture("aevo_markets_eth.json")).unwrap() {
            assert_eq!(i.expiry.unwrap().format("%H:%M:%S").to_string(), "08:00:00");
        }
    }

    #[test]
    fn rest_snapshot_builds_book() {
        let eth = parse_markets(&fixture("aevo_markets_eth.json")).unwrap();
        let i = eth
            .into_iter()
            .find(|i| i.key.native_id == "ETH-30OCT26-2800-C")
            .unwrap();
        let b = parse_book(
            &i,
            &fixture("aevo_orderbook_ETH-30OCT26-2800-C.json"),
            &fixture("aevo_instrument_ETH-30OCT26-2800-C.json"),
            Utc::now(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(
            b.bids[0],
            Level {
                price: dec!(93.8),
                size: dec!(25)
            }
        );
        assert_eq!(
            b.asks[0],
            Level {
                price: dec!(101.7),
                size: dec!(30)
            }
        );
        assert_eq!(
            b.source_ts.unwrap().timestamp_nanos_opt(),
            Some(1791199787380157660)
        );
        assert_eq!(b.index_price, Some(dec!(2720.493258)));
        assert_eq!(b.mark_price, Some(dec!(98.164438)));
    }

    #[test]
    fn ws_update_frames_are_rejected_by_the_snapshot_parser() {
        let eth = parse_markets(&fixture("aevo_markets_eth.json")).unwrap();
        let i = eth
            .into_iter()
            .find(|i| i.key.native_id == "ETH-30OCT26-2800-C")
            .unwrap();
        let mut upd = fixture("aevo_orderbook_ETH-30OCT26-2800-C.json");
        upd["type"] = json!("update");
        let r = parse_book(
            &i,
            &upd,
            &fixture("aevo_instrument_ETH-30OCT26-2800-C.json"),
            Utc::now(),
            Provenance::Fixture,
        );
        assert!(matches!(r, Err(ConnectorError::InvalidResponse { .. })));
    }

    #[test]
    fn fee_matches_documented_example() {
        // Doc: taker, 1 contract, ETH $1000, premium $20 -> min(0.05%*1*1000, 0.125*20) = 0.5
        let eth = parse_markets(&fixture("aevo_markets_eth.json")).unwrap();
        let f = fee_schedule(&eth[0]);
        let x = FeeInputs {
            qty_native: dec!(1),
            qty_underlying: dec!(1),
            premium: dec!(20),
            index_price: Some(dec!(1000)),
            mark_price: None,
        };
        assert_eq!(f.taker.compute(&x), FeeAmount::Known { amount: dec!(0.5) });
        assert_eq!(f.maker.compute(&x), FeeAmount::Known { amount: dec!(0.3) });
        // $1 option, ETH $1k: capped at 0.125
        let cheap = FeeInputs {
            premium: dec!(1),
            ..x
        };
        assert_eq!(
            f.taker.compute(&cheap),
            FeeAmount::Known {
                amount: dec!(0.125)
            }
        );
    }

    /// Local HTTP server answering each connection with the next canned (status, body).
    async fn serve_seq(responses: Vec<(&'static str, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf).await;
                let r = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(r.as_bytes()).await.unwrap();
                let _ = s.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn stale_data_is_retried_once_then_succeeds() {
        let url = serve_seq(vec![
            ("503 Service Unavailable", r#"{"error":"STALE_DATA"}"#),
            ("200 OK", "[]"),
        ])
        .await;
        let c = AevoConnector::new(&url);
        assert_eq!(c.get("/markets", &[]).await.unwrap(), json!([]));
    }

    #[tokio::test]
    async fn persistent_stale_data_is_typed() {
        let url = serve_seq(vec![
            ("503 Service Unavailable", r#"{"error":"STALE_DATA"}"#),
            ("503 Service Unavailable", r#"{"error":"STALE_DATA"}"#),
        ])
        .await;
        let c = AevoConnector::new(&url);
        assert!(matches!(
            c.get("/markets", &[]).await,
            Err(ConnectorError::UpstreamStale { .. })
        ));
    }

    #[tokio::test]
    async fn other_503_is_not_retried_nor_stale() {
        let url = serve_seq(vec![(
            "503 Service Unavailable",
            r#"{"error":"MAINTENANCE"}"#,
        )])
        .await;
        let c = AevoConnector::new(&url);
        assert!(matches!(
            c.get("/markets", &[]).await,
            Err(ConnectorError::Http { status: 503, .. })
        ));
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_catalog_and_book() {
        let c = AevoConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        // Nearest-to-the-money call of the first expiry at least 7 days out: should have depth.
        let horizon = Utc::now() + chrono::Duration::days(7);
        let exp = cat
            .iter()
            .filter_map(|i| i.expiry)
            .filter(|e| *e > horizon)
            .min()
            .unwrap();
        let spot = dec(P, &cat[0].native["index_price"], "index_price").unwrap();
        let i = cat
            .iter()
            .filter(|i| i.expiry == Some(exp) && i.option_kind == Some(OptionKind::Call))
            .min_by_key(|i| (i.strike.unwrap() - spot).abs())
            .unwrap();
        let b = c.get_market_data(i).await.unwrap().book;
        assert_eq!(b.provenance, Provenance::LiveData);
        assert!(b.index_price.is_some());
        eprintln!("{}: {} bids / {} asks", i.key, b.bids.len(), b.asks.len());
        assert!(
            !b.bids.is_empty() || !b.asks.is_empty(),
            "ATM book unexpectedly empty"
        );
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live_testnet {
    use super::*;

    #[tokio::test]
    async fn live_testnet_catalog() {
        let c = AevoConnector::testnet();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        assert!(cat.iter().all(|i| i.key.network == TESTNET_NETWORK));
        eprintln!("aevo testnet: {} BTC options", cat.len());
    }
}
