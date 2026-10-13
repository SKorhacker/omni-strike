//! Paradex — public REST, READ-ONLY.
//!
//! Verified live 2026-10-05:
//! - `GET /v1/markets` returns every market (7,767; 7,702 options; filters are ignored except
//!   `market=`). Options: `asset_kind = OPTION`, `option_type` CALL/PUT, `strike_price`,
//!   `expiry_at` (unix ms; mostly 08:00 UTC, some 20:00/21:00), size in underlying units
//!   (`order_size_increment` 0.001 BTC), `price_tick_size`, `min_notional` (USD), quote USD,
//!   settlement USDC, `fee_config.api_fee` (taker 0.0001, cap 0.125).
//! - `GET /v1/orderbook/{market}?depth=N`: STANDARD book `[price, size]`, `seq_no`,
//!   `last_updated_at` (ms). This is the only book API orders can trade against.
//! - `GET /v1/orderbook/{market}/interactive` also shows Retail-Price-Improvement orders, which
//!   only match Retail (interactive-token) takers; API ("Pro") orders never match them
//!   [DOC trading/rpi, trading/trader-profiles]. Live 2026-10-06: standard option books empty,
//!   532/1,240 BTC options two-sided in the interactive view.
//!   -> Used ONLY when the standard book is empty, tagged `BookAccess::RetailOnly`: displayed for
//!   price discovery, never eligible for ranking or routing (Ineligibility::RetailOnlyLiquidity).
//! - `GET /v1/markets/summary?market=X`: `mark_price`, `underlying_price`.
//!
//! Settlement [DOC trading/dated-options/overview]: European, cash, USDC, 30-min TWAP of the
//! Paradex spot oracle before expiry.

use crate::http::{client, get_json};
use crate::{Connector, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;

pub const MAINNET_REST: &str = "https://api.prod.paradex.trade/v1";
pub const NETWORK: &str = "paradex-mainnet";
pub const TESTNET_REST: &str = "https://api.testnet.paradex.trade/v1";
pub const TESTNET_NETWORK: &str = "paradex-testnet";
pub const SPEC_VERSION: &str = "paradex/2026-10-05";
const BOOK_DEPTH: &str = "100";
const FEE_SOURCE: &str =
    "https://docs.paradex.trade/trading/trading-fees (per-market fee_config.api_fee)";
const SETTLEMENT_WINDOW: &str = "30-minute TWAP of the Paradex spot oracle before expiry";

const P: PlatformId = PlatformId::Paradex;

pub struct ParadexConnector {
    http: reqwest::Client,
    rest: String,
    network: &'static str,
}

impl Default for ParadexConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl ParadexConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            network: NETWORK,
        }
    }

    pub fn testnet() -> Self {
        Self {
            network: TESTNET_NETWORK,
            ..Self::new(TESTNET_REST)
        }
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        get_json(&self.http, P, format!("{}{path}", self.rest), query).await
    }
}

fn ms_to_utc(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    let ms = v
        .as_i64()
        .ok_or_else(|| invalid(P, format!("{field}: expected integer ms, got {v}")))?;
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| invalid(P, format!("{field}: out of range")))
}

pub fn parse_market(v: &Value, underlying: &str) -> Result<Option<Instrument>> {
    if v["asset_kind"] != "OPTION" || v["base_currency"] != underlying {
        return Ok(None);
    }
    let name = v["symbol"].as_str().ok_or_else(|| invalid(P, "symbol"))?;
    let kind = match v["option_type"].as_str() {
        Some("CALL") => OptionKind::Call,
        Some("PUT") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: option_type {other:?}"))),
    };
    let expiry = ms_to_utc(&v["expiry_at"], "expiry_at")?;
    if expiry <= Utc::now() {
        return Ok(None);
    }
    let settle = v["settlement_currency"].as_str().unwrap_or("USDC");
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["strike_price"], "strike_price")?),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: Decimal::ONE,
        // Quoted in USD, premium settled in USDC [DOC]; we compare in the settlement currency.
        premium_currency: settle.into(),
        collateral_assets: vec![settle.into()],
        settlement_asset: Some(settle.into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("paradex:{underlying} spot oracle")),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        tick_size: opt_dec(P, v.get("price_tick_size"), "price_tick_size")?,
        // The venue minimum is a NOTIONAL (min_notional, USD), not a quantity -> kept in `native`.
        min_amount: None,
        amount_step: opt_dec(P, v.get("order_size_increment"), "order_size_increment")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: v.clone(),
    }))
}

pub fn parse_markets(v: &Value, underlying: &str) -> Result<Vec<Instrument>> {
    let arr = v["results"]
        .as_array()
        .ok_or_else(|| invalid(P, "markets: missing results"))?;
    let mut out = vec![];
    for m in arr {
        if let Some(i) = parse_market(m, underlying)? {
            out.push(i);
        }
    }
    Ok(out)
}

/// `book` = `/orderbook/{m}` or `/orderbook/{m}/interactive` (caller sets `access`), `summary` = one result of
/// `/markets/summary?market=m`.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    summary: &Value,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    if book["market"] != instrument.key.native_id.as_str()
        || summary["symbol"] != instrument.key.native_id.as_str()
    {
        return Err(invalid(P, "response market does not match the request"));
    }
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: crate::pair_levels(P, book, "bids", "orderbook", "level.size")?,
        asks: crate::pair_levels(P, book, "asks", "orderbook", "level.size")?,
        source_ts: Some(ms_to_utc(&book["last_updated_at"], "last_updated_at")?),
        received_at,
        index_price: positive(opt_dec(
            P,
            summary.get("underlying_price"),
            "underlying_price",
        )?),
        mark_price: positive(opt_dec(P, summary.get("mark_price"), "mark_price")?),
        provenance,
        access: BookAccess::Api,
    })
}

/// API taker/maker fee per market: `fee` x notional (size x underlying price), capped at
/// `fee_cap` x premium. HYPOTHESIS on the notional basis (docs state 1 bp capped at 12.5 % of
/// premium for API orders); flagged in the notes.
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let f = &instrument.native["fee_config"]["api_fee"];
    let part = |side: &str| -> Result<FeeFormula> {
        Ok(FeeFormula::Min {
            parts: vec![
                FeeFormula::PctOfUnderlyingNotional {
                    rate: dec(P, &f[side]["fee"], "fee")?,
                },
                FeeFormula::PctOfPremium {
                    rate: dec(P, &f[side]["fee_cap"], "fee_cap")?,
                },
            ],
        })
    };
    Ok(FeeSchedule {
        taker: part("taker_fee")?,
        maker: part("maker_fee")?,
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-05".into(),
        notes: "API fee from the market's fee_config (retail/interactive fees differ and do not apply to API orders). \
                Notional basis = size x underlying price (interpretation). Venue minimum is a notional (min_notional), not checked here."
            .into(),
    })
}

#[async_trait]
impl Connector for ParadexConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        self.network
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self.get("/markets", &[]).await?;
        let mut out = parse_markets(&v, underlying)?;
        for i in &mut out {
            i.key.network = self.network.into();
        }
        Ok(out)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let name = instrument.key.native_id.as_str();
        let (path, path_rpi) = (
            format!("/orderbook/{name}"),
            format!("/orderbook/{name}/interactive"),
        );
        let q_book = [("depth", BOOK_DEPTH)];
        let q_sum = [("market", name)];
        let t0 = std::time::Instant::now();
        // All three in parallel: the RPI book is only used if the standard one is empty.
        let (book, rpi, sum) = tokio::join!(
            self.get(&path, &q_book),
            self.get(&path_rpi, &q_book),
            self.get("/markets/summary", &q_sum)
        );
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        let sum = sum?;
        let summary = sum["results"]
            .get(0)
            .cloned()
            .ok_or_else(|| invalid(P, "summary: empty results"))?;
        let standard = parse_book(
            instrument,
            &book?,
            &summary,
            received_at,
            Provenance::LiveData,
        )?;
        let (book, path) = if standard.bids.is_empty() && standard.asks.is_empty() {
            match rpi {
                Ok(r) => {
                    let mut b =
                        parse_book(instrument, &r, &summary, received_at, Provenance::LiveData)?;
                    b.access = BookAccess::RetailOnly;
                    (b, "rest_rpi")
                }
                Err(_) => (standard, "rest"),
            }
        } else {
            (standard, "rest")
        };
        Ok(crate::MarketData {
            book,
            network,
            normalize: t1.elapsed(),
            path,
        })
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

    fn btc_put() -> Instrument {
        // The recorded market expires 2026-10-16; parse it as of the recording (skip the past filter).
        let m = &fixture("paradex_markets_sample.json")["results"][0];
        let mut i = parse_market(m, "BTC").unwrap().expect("option");
        i.key.network = NETWORK.into();
        i
    }

    #[test]
    fn normalises_a_recorded_option() {
        let i = btc_put();
        assert_eq!(i.key.native_id, "BTC-USD-16OCT26-85000-P"); // gitleaks:allow
        assert_eq!(i.option_kind, Some(OptionKind::Put));
        assert_eq!(i.strike, Some(dec!(85000)));
        assert_eq!(i.expiry.unwrap().to_rfc3339(), "2026-10-16T08:00:00+00:00");
        assert_eq!(i.amount_step, Some(dec!(0.001)));
        assert_eq!(i.tick_size, Some(dec!(0.01)));
        assert_eq!(i.premium_currency, "USDC");
        assert_eq!(i.min_amount, None, "min is a notional, not a quantity");
        assert_eq!(i.native["min_notional"], "20");
    }

    #[test]
    fn other_underlyings_and_perps_are_skipped() {
        let m = &fixture("paradex_markets_sample.json")["results"][0];
        assert!(parse_market(m, "ETH").unwrap().is_none());
        let mut perp = m.clone();
        perp["asset_kind"] = serde_json::json!("PERP");
        assert!(parse_market(&perp, "BTC").unwrap().is_none());
    }

    #[test]
    fn standard_book_is_api_and_rpi_book_parses() {
        let i = btc_put();
        let summary = &fixture("paradex_summary_option.json")["results"][0];
        let std = parse_book(
            &i,
            &fixture("paradex_orderbook_option.json"),
            summary,
            Utc::now(),
            Provenance::Fixture,
        )
        .unwrap();
        assert!(
            std.asks.is_empty() && std.bids.is_empty(),
            "recorded standard book was empty"
        );
        assert_eq!(std.mark_price, Some(dec!(1327.54482149)));
        assert_eq!(std.index_price, Some(dec!(86197.00881948)));
        assert_eq!(std.access, BookAccess::Api);
        // The interactive (RPI) book parses; the connector tags it RetailOnly when the standard one is empty.
        let rpi = parse_book(
            &i,
            &fixture("paradex_orderbook_interactive_option.json"),
            summary,
            Utc::now(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(
            rpi.asks[0],
            Level {
                price: dec!(1391.55),
                size: dec!(0.001)
            }
        );
    }

    #[test]
    fn api_fee_from_market_config() {
        let f = fee_schedule(&btc_put()).unwrap();
        let x = oa_core::fees::FeeInputs {
            qty_native: dec!(0.1),
            qty_underlying: dec!(0.1),
            premium: dec!(139.155),
            index_price: Some(dec!(86000)),
            mark_price: None,
        };
        // min(0.0001 x 0.1 x 86000 = 0.86, 0.125 x 139.155 = 17.39) = 0.86
        assert_eq!(
            f.taker.compute(&x),
            oa_core::fees::FeeAmount::Known { amount: dec!(0.86) }
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_catalog_and_standard_book() {
        let c = ParadexConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(cat.len() > 100, "{}", cat.len());
        let i = cat
            .iter()
            .filter(|i| i.expiry.unwrap() > Utc::now() + chrono::Duration::days(2))
            .min_by_key(|i| i.expiry)
            .unwrap();
        let b = c.get_market_data(i).await.unwrap().book;
        eprintln!(
            "paradex {}: standard book {} bids / {} asks, mark {:?}",
            i.key,
            b.bids.len(),
            b.asks.len(),
            b.mark_price
        );
        assert_eq!(b.provenance, Provenance::LiveData);
    }
}
