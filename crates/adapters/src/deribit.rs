//! Deribit — USDC-settled LINEAR options, public REST, READ-ONLY. Centralised exchange (not onchain):
//! trading needs the user's own Deribit account (KYC) and API keys.
//!
//! Verified live 2026-10-06:
//! - `GET /public/get_instruments?currency=USDC&kind=option&expired=false`: 3,342 linear options
//!   (`instrument_type = linear`, `settlement_currency = USDC`), 598 BTC_USDC + 504 ETH_USDC; fields
//!   `strike`, `option_type`, `expiration_timestamp` (ms, 08:00 UTC), `contract_size` 1,
//!   `min_trade_amount`, `tick_size`, `taker_commission` 0.0003.
//! - `GET /public/get_order_book?instrument_name=X&depth=N`: `asks`/`bids` `[price USDC, amount]`,
//!   `timestamp` (ms), `index_price`, `mark_price`. Books are deep (10+ BTC, hundreds of ETH at the
//!   top levels) and priced in line with the inverse (coin-settled) books.
//! - Inverse options (premium/settlement in BTC/ETH) are NOT used: not directly comparable.
//!
//! Fees [DOC https://www.deribit.com/kb/fees]: options taker/maker 0.03 % of the underlying, capped at
//! 12.5 % of the option price. Settlement: European, cash in USDC, delivery price = 30-minute TWAP of
//! the Deribit index before expiry.

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

#[path = "deribit_stream.rs"]
mod stream;
pub use stream::StreamStats;

pub const MAINNET_REST: &str = "https://www.deribit.com/api/v2";
/// Public JSON-RPC WebSocket (verified live 2026-10-07, see `deribit_stream.rs`).
pub const MAINNET_WS: &str = "wss://www.deribit.com/ws/api/v2";
pub const NETWORK: &str = "deribit-usdc-linear";
/// Deribit TESTNET public REST (pair with `network::OnNetwork(.., deribit_exec::TESTNET_NETWORK)`).
pub const TESTNET_REST: &str = "https://test.deribit.com/api/v2";
pub const SPEC_VERSION: &str = "deribit-linear/2026-10-06";
const BOOK_DEPTH: &str = "50";
const FEE_SOURCE: &str =
    "https://www.deribit.com/kb/fees (options: 0.03% of underlying, cap 12.5% of option price)";
const SETTLEMENT_WINDOW: &str = "30-minute TWAP of the Deribit index before expiry";

const P: PlatformId = PlatformId::Deribit;

/// A streamed book whose connection was last confirmed alive longer ago than this is not served
/// (REST fallback instead).
const STREAM_MAX_AGE: Duration = Duration::from_secs(3);
/// Wait for the first book of a NEW subscription (only while connected; otherwise REST at once).
const STREAM_FIRST_WAIT: Duration = Duration::from_millis(1000);

pub struct DeribitConnector {
    http: reqwest::Client,
    rest: String,
    ws: String,
    /// Persistent WS book stream (preferred). `None` = REST only.
    stream: Option<Arc<stream::DeribitStream>>,
}

impl Default for DeribitConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl DeribitConnector {
    pub fn new(rest: &str) -> Self {
        let rest: String = rest.trim_end_matches('/').into();
        // https://{host}/api/v2 -> wss://{host}/ws/api/v2 (mainnet and test.deribit.com alike).
        let ws = rest
            .replacen("https://", "wss://", 1)
            .replacen("/api/v2", "/ws/api/v2", 1);
        Self {
            http: client(),
            rest,
            ws,
            stream: None,
        }
    }

    /// Same connector, with a persistent WebSocket book stream (requires a Tokio runtime).
    /// Books are served from the stream when confirmed fresh, else from REST as before.
    pub fn with_stream(mut self) -> Self {
        self.stream = Some(stream::DeribitStream::spawn(&self.ws));
        self
    }

    /// Override the WebSocket URL (call before `with_stream`).
    pub fn with_ws_url(mut self, ws: &str) -> Self {
        self.ws = ws.into();
        self
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let mut v = get_json(&self.http, P, format!("{}{path}", self.rest), query).await?;
        if let Some(e) = v.get("error") {
            return Err(invalid(P, format!("{path}: {e}")));
        }
        // Moved out, not deep-copied (catalog responses hold thousands of instruments).
        v.get_mut("result")
            .map(Value::take)
            .ok_or_else(|| invalid(P, format!("{path}: no result")))
    }
}

fn ms(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    crate::ts_ms(P, v.as_i64(), field)
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["kind"] != "option"
        || v["instrument_type"] != "linear"
        || v["settlement_currency"] != "USDC"
        || v["base_currency"] != underlying
    {
        return Ok(None);
    }
    if v["is_active"] == false {
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
    let expiry = ms(&v["expiration_timestamp"], "expiration_timestamp")?;
    if expiry <= now {
        return Ok(None);
    }
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["strike"], "strike")?.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: dec(P, &v["contract_size"], "contract_size")?,
        premium_currency: "USDC".into(),
        collateral_assets: vec!["USDC".into()],
        settlement_asset: Some("USDC".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!(
            "deribit:{}",
            v["price_index"].as_str().unwrap_or("index")
        )),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        tick_size: opt_dec(P, v.get("tick_size"), "tick_size")?,
        min_amount: opt_dec(P, v.get("min_trade_amount"), "min_trade_amount")?,
        amount_step: opt_dec(P, v.get("min_trade_amount"), "min_trade_amount")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "instrument_id": v["instrument_id"], "taker_commission": v["taker_commission"],
            "maker_commission": v["maker_commission"], "tick_size_steps": v["tick_size_steps"],
            "venue_type": "centralised_exchange", "account_required": "user's own Deribit account + API keys (KYC)"
        }),
    }))
}

pub fn parse_instruments(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Instrument>> {
    let arr = v
        .as_array()
        .ok_or_else(|| invalid(P, "instruments: not an array"))?;
    let mut out = vec![];
    for x in arr {
        if let Some(i) = parse_instrument(x, underlying, now)? {
            out.push(i);
        }
    }
    Ok(out)
}

pub fn parse_book(
    instrument: &Instrument,
    v: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    if v["instrument_name"] != instrument.key.native_id.as_str() {
        return Err(invalid(P, "response instrument does not match the request"));
    }
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, v, "bids", "order_book", "level.size")?,
        asks: crate::pair_levels(P, v, "asks", "order_book", "level.size")?,
        source_ts: Some(ms(&v["timestamp"], "timestamp")?),
        received_at,
        index_price: positive(opt_dec(P, v.get("index_price"), "index_price")?),
        mark_price: positive(opt_dec(P, v.get("mark_price"), "mark_price")?),
        provenance,
        access: BookAccess::Api,
    })
}

/// 0.03 % of underlying notional, capped at 12.5 % of the premium (per-instrument commission field).
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let part = |field: &str| -> Result<FeeFormula> {
        Ok(FeeFormula::Min {
            parts: vec![
                FeeFormula::PctOfUnderlyingNotional {
                    rate: dec(P, &instrument.native[field], field)?,
                },
                FeeFormula::PctOfPremium {
                    rate: Decimal::new(125, 3),
                },
            ],
        })
    };
    Ok(FeeSchedule {
        taker: part("taker_commission")?,
        maker: part("maker_commission")?,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: "Commission rate from the instrument (0.0003); 12.5 % premium cap from the fee page. Volume tiers / VIP discounts not modelled. Centralised exchange: needs the user's own account (KYC).".into(),
    })
}

#[async_trait]
impl Connector for DeribitConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self
            .get(
                "/public/get_instruments",
                &[
                    ("currency", "USDC"),
                    ("kind", "option"),
                    ("expired", "false"),
                ],
            )
            .await?;
        parse_instruments(&v, underlying, Utc::now())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        if let Some(s) = &self.stream {
            let name = instrument.key.native_id.as_str();
            let t0 = std::time::Instant::now();
            let wait = if s.is_connected() {
                STREAM_FIRST_WAIT
            } else {
                Duration::ZERO
            };
            match s.snapshot(name, wait).await {
                stream::Snapshot::Ready(r) if r.confirmed_mono.elapsed() <= STREAM_MAX_AGE => {
                    let network = t0.elapsed();
                    let t1 = std::time::Instant::now();
                    // received_at = arrival of the last frame on the live, ordered connection:
                    // Deribit sends book changes only, so that is when this book was last
                    // known current. source_ts = venue timestamp of the last book update.
                    let book =
                        parse_book(instrument, &r.book, r.confirmed_at, Provenance::LiveData)?;
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
                        native_id: format!("{name} ({reason})"),
                    });
                }
                _ => tracing::debug!(instrument = name, "deribit stream not ready, REST fallback"),
            }
        }
        let t0 = std::time::Instant::now();
        let v = self
            .get(
                "/public/get_order_book",
                &[
                    ("instrument_name", &instrument.key.native_id),
                    ("depth", BOOK_DEPTH),
                ],
            )
            .await?;
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let book = parse_book(instrument, &v, received_at, Provenance::LiveData)?;
        Ok(crate::MarketData::rest(book, network, t1))
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

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        fee_schedule(instrument)
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

    #[test]
    fn keeps_only_linear_usdc_options_of_the_underlying() {
        let v = fixture("deribit_instruments_usdc.json");
        let btc = parse_instruments(&v["result"], "BTC", at()).unwrap();
        let eth = parse_instruments(&v["result"], "ETH", at()).unwrap();
        assert!(!btc.is_empty() && !eth.is_empty());
        assert!(
            btc.iter()
                .all(|i| i.key.native_id.starts_with("BTC_USDC-") && i.premium_currency == "USDC")
        );
        let i = btc
            .iter()
            .find(|i| i.key.native_id == "BTC_USDC-7OCT26-80000-C") // gitleaks:allow
            .unwrap();
        assert_eq!(i.strike, Some(dec!(80000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-07T08:00:00+00:00");
        assert_eq!(i.option_kind, Some(OptionKind::Call));
        assert_eq!(i.contract_multiplier, dec!(1));
    }

    #[test]
    fn book_and_fees() {
        let v = fixture("deribit_instruments_usdc.json");
        let i = parse_instruments(&v["result"], "BTC", at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == "BTC_USDC-7OCT26-80000-C") // gitleaks:allow
            .unwrap();
        let b = parse_book(
            &i,
            &fixture("deribit_book_btc_usdc.json")["result"],
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert!(!b.asks.is_empty() && b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.index_price.is_some());
        let f = fee_schedule(&i).unwrap();
        let fee = f.taker.compute(&oa_core::fees::FeeInputs {
            qty_native: dec!(1),
            qty_underlying: dec!(1),
            premium: dec!(6040),
            index_price: Some(dec!(86000)),
            mark_price: None,
        });
        assert_eq!(fee, oa_core::fees::FeeAmount::Known { amount: dec!(25.8) });
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_deribit_linear() {
        let c = DeribitConnector::default();
        let cat = c.list_instruments("ETH").await.unwrap();
        assert!(!cat.is_empty());
        let md = c.get_market_data(&cat[cat.len() / 2]).await.unwrap();
        println!(
            "deribit {} asks {:?}",
            cat[cat.len() / 2].key.native_id,
            md.book.asks.first()
        );
    }
}
