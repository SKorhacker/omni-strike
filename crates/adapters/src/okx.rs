//! OKX — BTC/ETH options, public REST, READ-ONLY. Centralised exchange (not onchain): trading needs
//! the user's own OKX account (KYC) and API keys. No key is used here.
//!
//! Verified live 2026-10-06:
//! - `GET /api/v5/public/instruments?instType=OPTION&uly=BTC-USD` (resp. `ETH-USD`): 1,260 BTC and
//!   1,130 ETH options. The same `uly` returns TWO families:
//!   * coin-margined (inverse): `instFamily = BTC-USD`, `ctType = inverse`, `settleCcy = BTC`,
//!     `ctMult` 0.01 (BTC) / 0.1 (ETH), `ctVal` 1, `ctValCcy` BTC/ETH, `tickSz` 0.0001 (coin);
//!     654 BTC + 662 ETH.
//!   * USD-margined ("USDS-M", `_UM`): `instFamily = BTC-USD_UM`, `ctType = linear`,
//!     `settleCcy = USD`, `ctMult` 0.01 for BTC AND ETH, `tickSz` 5 (BTC) / 0.2 (ETH) USD;
//!     606 BTC + 468 ETH (6 ETH in `state = preopen`, skipped).
//!
//!   Common: `stk`, `optType` C/P, `expTime` (ms, 08:00 UTC), `minSz` 1, `lotSz` 1, `state`.
//! - `GET /api/v5/market/books?instId=X&sz=N`: `asks`/`bids` `[px, sz, "0" (deprecated), n_orders]`,
//!   `ts` (ms). `sz` is in contracts. The response does NOT echo `instId`.
//! - `px` is quoted PER 1 UNIT OF UNDERLYING (not per contract): inverse in coin (e.g. 0.0105 BTC
//!   for BTC-USD-261009-86000-C), `_UM` in USD (e.g. 930 USD for the same strike/expiry with index
//!   85,966 → 0.0105 × 85,966 ≈ 903). Per native contract = px × ctMult × ctVal.
//! - `GET /api/v5/market/index-tickers?instId=BTC-USD`: `idxPx`, `ts`.
//! - `GET /api/v5/public/mark-price?instType=OPTION&instId=X`: `markPx` (same unit as `px`).
//!
//! Canonical mapping (one quote currency per request in the engine):
//! - Level price = premium per NATIVE contract in USD(C)-equivalent; level size = native contracts.
//! - Inverse: px (coin per unit) × multiplier × index price of the SAME snapshot (fetched in
//!   parallel with the book). The premium is really paid in BTC/ETH: `premium_currency` is "USDC"
//!   for comparison only, `native.premium_native_currency` keeps the coin, `native.converted_at_index`
//!   = true, and `settlement_asset` = the coin (comparability flags SETTLEMENT_ASSET_DIFFERS).
//!   If the index request fails, the book is rejected (never converted with a guessed price).
//! - `_UM`: px (USD per unit) × multiplier; no conversion. OKX says margin/premium/settlement are in
//!   USD with USDC/USDG support "varying by region"; the product is a regulated offering in the UAE
//!   and Australia (wholesale clients only) per
//!   https://www.okx.com/en-us/learn/btc-eth-options-stablecoin-margin. `_UM` books are much thinner
//!   than the inverse ones (ETH `_UM` often one-sided/empty on 2026-10-06). We list BOTH families:
//!   `_UM` is the natively USD-priced one (preferred when available to the user), inverse is where
//!   the depth is.
//!
//! Specs [DOC https://www.okx.com/en-us/help/360037737011]: European, cash-settled, auto-exercise ITM,
//! expiry 08:00 UTC, delivery price = TWAP of the OKX index over the last 30 minutes before expiry
//! (200 ms snapshots). Inverse tick documented as 0.0001 coin below 0.005 and 0.0005 above (the API
//! reports a single `tickSz` 0.0001); `_UM` BTC tick documented 1 USD but the API reports 5 (API used).
//!
//! Fees [DOC https://www.okx.com/en-us/help/trading-fee-rules-faq]:
//! `Min(Fee rate × ctMult × ctVal × n, 7% × Option premium × ctMult × ctVal × n)`, i.e. rate × the
//! underlying quantity (charged in coin for inverse = rate × qty × index in USD), capped at 7 % of the
//! premium. Cap 12.5 % → 7 % since 2025-05-15
//! [https://www.okx.com/en-us/help/okx-to-adjust-parameters-for-options-fee-calculation].
//! Regular (Lv1) maker 0.030 % / taker 0.030 % since 2025-02-10
//! [https://www.okx.com/en-us/help/okx-to-adjust-options-trading-fees]. The fee FAQ worked example
//! uses 0.02 %/0.03 % as an illustration only. UNVERIFIED: the fee doc does not say whether `_UM`
//! options use exactly the same formula (we apply it with USD notional at the OKX index).
//!
//! Bulk top of book (verified live 2026-10-06): `GET /api/v5/market/tickers?instType=OPTION&uly=BTC-USD`
//! returns BOTH families in one response (1,260 BTC = 654 inverse + 606 `_UM`; ETH 1,132), each with
//! `instId`, `bidPx`/`bidSz`/`askPx`/`askSz` (same units as `/market/books`: px per underlying unit,
//! size in contracts; an empty side is `px ""`, `sz "0"`) and `ts` (ms). The mark comes from
//! `GET /api/v5/public/mark-price?instType=OPTION&uly=BTC-USD` (all 1,260 marks in one call) and the
//! index from `/market/index-tickers`, fetched once per call. Each ticker is turned into a 1-level
//! book and goes through `parse_book`, so bulk and full-book prices are identical by construction.
//! Rate limits [DOC https://www.okx.com/docs-v5/en/]: tickers 20 req/2 s, mark-price 10 req/2 s,
//! index-tickers 20 req/2 s (per IP): 3 requests per underlying every 2 s fits.
//!
//! Optional persistent WebSocket stream (`with_stream()`, see `okx_stream.rs`): `books`
//! (incremental, `prevSeqId` chain) + `index-tickers` + `mark-price`; the materialised book has the
//! REST `/market/books` shape and goes through `parse_book`, so stream and REST books are identical.

#[path = "okx_stream.rs"]
mod stream;

use crate::http::{client, get_json};
use crate::{Connector, ConnectorError, Result, dec, invalid, opt_dec};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAINNET_REST: &str = "https://www.okx.com";
pub const NETWORK: &str = "okx-mainnet";
pub const SPEC_VERSION: &str = "okx/2026-10-06";
const BOOK_DEPTH: &str = "50";
const QUOTE: &str = "USDC";
const FEE_SOURCE: &str = "https://www.okx.com/en-us/help/trading-fee-rules-faq (options: Min(rate x underlying qty, 7% x premium)); Lv1 rates 0.03%/0.03%: https://www.okx.com/en-us/help/okx-to-adjust-options-trading-fees";
const SETTLEMENT_WINDOW: &str =
    "30-minute TWAP of the OKX index before expiry (08:00 UTC, 200 ms snapshots)";
/// Regular (Lv1) user, maker and taker, since 2025-02-10.
const FEE_RATE_LV1: Decimal = Decimal::from_parts(3, 0, 0, false, 4);
/// 7 % of premium, since 2025-05-15.
const PREMIUM_CAP: Decimal = Decimal::from_parts(7, 0, 0, false, 2);

const P: PlatformId = PlatformId::Okx;
/// Max age of the instrument catalog reused by `bulk_top_of_book` (multipliers / families).
const CATALOG_TTL: Duration = Duration::from_secs(300);

/// Live instruments of one underlying by `instId`.
type Catalog = Arc<HashMap<String, Instrument>>;

pub struct OkxConnector {
    http: reqwest::Client,
    rest: String,
    /// underlying → (fetched at, catalog); refreshed by every `list_instruments`.
    catalogs: Mutex<HashMap<String, (Instant, Catalog)>>,
    /// Public WebSocket URL used by `with_stream` (demo: `wspap` host + `x-simulated-trading`).
    ws: String,
    demo: bool,
    /// Persistent book stream (preferred when fresh). `None` = REST only.
    stream: Option<Arc<stream::OkxStream>>,
}

/// A streamed book is served only if the connection delivered a frame within this age (index
/// and mark entries too). Otherwise REST, as without a stream.
const STREAM_MAX_AGE: Duration = Duration::from_secs(3);

impl Default for OkxConnector {
    fn default() -> Self {
        Self::new(MAINNET_REST)
    }
}

impl OkxConnector {
    pub fn new(rest: &str) -> Self {
        Self {
            http: client(),
            rest: rest.trim_end_matches('/').into(),
            catalogs: Mutex::new(HashMap::new()),
            ws: stream::MAINNET_WS.into(),
            demo: false,
            stream: None,
        }
    }

    /// OKX DEMO market data (same host, `x-simulated-trading: 1`): demo instruments, books and
    /// ticks differ from live. Wrap in `network::OnNetwork(.., okx_exec::DEMO_NETWORK)`.
    /// The stream (if enabled) uses the demo WS host `wspap.okx.com` with the same header.
    pub fn demo() -> Self {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "x-simulated-trading",
            reqwest::header::HeaderValue::from_static("1"),
        );
        Self {
            http: crate::http::client_with_headers(h),
            ws: stream::DEMO_WS.into(),
            demo: true,
            ..Self::new(MAINNET_REST)
        }
    }

    /// Same connector with a persistent public WebSocket book stream (requires a Tokio runtime).
    /// `get_market_data` serves the streamed book when fresh, else REST.
    pub fn with_stream(mut self) -> Self {
        self.stream = Some(stream::OkxStream::spawn(&self.ws, self.demo));
        self
    }

    /// Streamed book of `instrument` if the stream can serve it now (no REST call).
    async fn stream_book(
        &self,
        s: &stream::OkxStream,
        instrument: &Instrument,
        wait: Duration,
    ) -> Result<Option<crate::MarketData>> {
        let t0 = Instant::now();
        let uly = format!("{}-USD", instrument.underlying);
        let id = instrument.key.native_id.as_str();
        let need_index = is_inverse(&instrument.native)?;
        match s.snapshot(id, &uly, need_index, STREAM_MAX_AGE, wait).await {
            stream::Snapshot::Ready(v) => {
                let network = t0.elapsed();
                let t1 = Instant::now();
                let book = parse_book(
                    instrument,
                    &v.book,
                    v.index.as_ref(),
                    v.mark.as_ref(),
                    v.confirmed_at,
                    Provenance::LiveData,
                )?;
                Ok(Some(crate::MarketData {
                    book,
                    network,
                    normalize: t1.elapsed(),
                    path: "ws_stream",
                }))
            }
            stream::Snapshot::Rejected(reason) => Err(ConnectorError::UnknownInstrument {
                platform: P,
                native_id: format!("{id} ({reason})"),
            }),
            stream::Snapshot::NotYet => Ok(None),
        }
    }

    /// Cached catalog (≤ `CATALOG_TTL` old), else a fresh `list_instruments`.
    async fn catalog(&self, underlying: &str) -> Result<Catalog> {
        if let Some((at, c)) = self.catalogs.lock().expect("catalog lock").get(underlying)
            && at.elapsed() < CATALOG_TTL
        {
            return Ok(c.clone());
        }
        self.list_instruments(underlying).await?;
        self.catalogs
            .lock()
            .expect("catalog lock")
            .get(underlying)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| invalid(P, "catalog not cached after listing"))
    }

    /// OKX v5 envelope: `{"code":"0","msg":"","data":[...]}`; any other code is an error.
    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let mut v = get_json(&self.http, P, format!("{}{path}", self.rest), query).await?;
        take_data(&mut v, path)
    }
}

/// `data` moved out of the envelope (no deep copy of large instrument lists).
fn take_data(v: &mut Value, path: &str) -> Result<Value> {
    if v["code"] != "0" {
        return Err(invalid(
            P,
            format!("{path}: code {} msg {}", v["code"], v["msg"]),
        ));
    }
    match v.get_mut("data") {
        Some(d) if d.is_array() => Ok(d.take()),
        _ => Err(invalid(P, format!("{path}: no data array"))),
    }
}

#[cfg(test)]
fn data(v: &Value, path: &str) -> Result<Value> {
    if v["code"] != "0" {
        return Err(invalid(
            P,
            format!("{path}: code {} msg {}", v["code"], v["msg"]),
        ));
    }
    v.get("data")
        .filter(|d| d.is_array())
        .cloned()
        .ok_or_else(|| invalid(P, format!("{path}: no data array")))
}

/// First element of a `data` array (books, index-tickers, mark-price return one object).
fn first<'a>(v: &'a Value, what: &str) -> Result<&'a Value> {
    v.get(0)
        .ok_or_else(|| invalid(P, format!("{what}: empty data")))
}

fn ms(v: &Value, field: &str) -> Result<DateTime<Utc>> {
    crate::ts_ms(P, v.as_str().map_or(v.as_i64(), |s| s.parse().ok()), field)
}

fn is_inverse(native: &Value) -> Result<bool> {
    match native["ctType"].as_str() {
        Some("inverse") => Ok(true),
        Some("linear") => Ok(false),
        other => Err(invalid(P, format!("ctType {other:?}"))),
    }
}

pub fn parse_instrument(
    v: &Value,
    underlying: &str,
    now: DateTime<Utc>,
) -> Result<Option<Instrument>> {
    if v["instType"] != "OPTION"
        || v["uly"].as_str() != Some(&format!("{underlying}-USD"))
        || v["ctValCcy"] != underlying
    {
        return Ok(None);
    }
    // `preopen` (listed, not yet tradable) and `suspend` are skipped.
    if v["state"] != "live" {
        return Ok(None);
    }
    let name = v["instId"].as_str().ok_or_else(|| invalid(P, "instId"))?;
    let inverse = is_inverse(v)?;
    let settle = v["settleCcy"]
        .as_str()
        .ok_or_else(|| invalid(P, format!("{name}: settleCcy")))?;
    // Guard the two families we verified; anything else is not mapped.
    if (inverse && settle != underlying) || (!inverse && settle != "USD") {
        return Err(invalid(
            P,
            format!(
                "{name}: unexpected ctType/settleCcy {}/{settle}",
                v["ctType"]
            ),
        ));
    }
    let kind = match v["optType"].as_str() {
        Some("C") => OptionKind::Call,
        Some("P") => OptionKind::Put,
        other => return Err(invalid(P, format!("{name}: optType {other:?}"))),
    };
    let expiry = ms(&v["expTime"], "expTime")?;
    if expiry <= now {
        return Ok(None);
    }
    // Underlying units per native contract = ctMult × ctVal (ctVal is in ctValCcy = the coin).
    let multiplier = (dec(P, &v["ctMult"], "ctMult")? * dec(P, &v["ctVal"], "ctVal")?).normalize();
    if multiplier <= Decimal::ZERO {
        return Err(invalid(P, format!("{name}: non-positive multiplier")));
    }
    let native_tick = opt_dec(P, v.get("tickSz"), "tickSz")?;
    Ok(Some(Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: name.into(),
        },
        family: ProductFamily::DatedOption,
        underlying: underlying.into(),
        option_kind: Some(kind),
        strike: Some(dec(P, &v["stk"], "stk")?.normalize()),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::European),
        contract_multiplier: multiplier,
        premium_currency: QUOTE.into(),
        collateral_assets: if inverse {
            vec![underlying.into()]
        } else {
            vec!["USD".into(), "USDC".into(), "USDG".into()]
        },
        settlement_asset: Some(if inverse {
            underlying.into()
        } else {
            "USD".into()
        }),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("okx:index:{underlying}-USD")),
        settlement_window: Some(SETTLEMENT_WINDOW.into()),
        // `tickSz` is per 1 underlying unit in the native currency. For `_UM` the per-contract
        // USD tick is tickSz × multiplier; for inverse it depends on the live index → None.
        tick_size: if inverse {
            None
        } else {
            native_tick.map(|t| (t * multiplier).normalize())
        },
        min_amount: opt_dec(P, v.get("minSz"), "minSz")?,
        amount_step: opt_dec(P, v.get("lotSz"), "lotSz")?,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: serde_json::json!({
            "instFamily": v["instFamily"], "instIdCode": v["instIdCode"], "ctType": v["ctType"],
            "ctMult": v["ctMult"], "ctVal": v["ctVal"], "ctValCcy": v["ctValCcy"],
            "settleCcy": v["settleCcy"], "tickSz": v["tickSz"],
            "px_unit": "per 1 unit of underlying (level price = px x multiplier)",
            "premium_native_currency": if inverse { underlying } else { "USD" },
            "converted_at_index": inverse,
            "venue_type": "centralised_exchange",
            "account_required": "user's own OKX account + API keys (KYC)",
            "availability": if inverse { "global OKX derivatives (jurisdiction rules apply)" }
                else { "USDS-M options: regulated offering in UAE and Australia (wholesale only); USD/USDC/USDG varies by region" },
        }),
    }))
}

/// `data` array of `/api/v5/public/instruments`.
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

/// Native px (per underlying unit) → premium per native contract in USD(C)-equivalent.
fn to_contract_usd(
    px: Decimal,
    instrument: &Instrument,
    index: Option<Decimal>,
) -> Result<Decimal> {
    let per_contract = px * instrument.contract_multiplier;
    if is_inverse(&instrument.native)? {
        let idx = index.ok_or_else(|| {
            invalid(
                P,
                "index price unavailable: cannot convert coin premium to USD",
            )
        })?;
        Ok(per_contract * idx)
    } else {
        Ok(per_contract)
    }
}

fn levels(
    v: &Value,
    side: &str,
    instrument: &Instrument,
    index: Option<Decimal>,
) -> Result<Vec<Level>> {
    let arr = v[side]
        .as_array()
        .ok_or_else(|| invalid(P, format!("books.{side} missing")))?;
    arr.iter()
        .map(|l| {
            let px = dec(P, &l[0], "level.px")?;
            let size = dec(P, &l[1], "level.sz")?;
            if px <= Decimal::ZERO || size <= Decimal::ZERO {
                return Err(invalid(P, format!("non-positive book level {l}")));
            }
            Ok(Level {
                price: to_contract_usd(px, instrument, index)?,
                size,
            })
        })
        .collect()
}

/// `book`: one element of `/market/books` data. `index`: one element of `/market/index-tickers`
/// data (mandatory for inverse instruments). `mark`: one element of `/public/mark-price` data.
pub fn parse_book(
    instrument: &Instrument,
    book: &Value,
    index: Option<&Value>,
    mark: Option<&Value>,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<BookSnapshot> {
    let positive = |d: Option<Decimal>| d.filter(|x| *x > Decimal::ZERO);
    let index_price = match index {
        Some(ix) => {
            if ix["instId"].as_str() != Some(&format!("{}-USD", instrument.underlying)) {
                return Err(invalid(P, "index ticker does not match the underlying"));
            }
            positive(opt_dec(P, ix.get("idxPx"), "idxPx")?)
        }
        None => None,
    };
    let mark_price = match mark {
        Some(m) if m["instId"] == instrument.key.native_id.as_str() => {
            match positive(opt_dec(P, m.get("markPx"), "markPx")?) {
                // A mark that cannot be converted (no index) is left unknown, not guessed.
                Some(px) => to_contract_usd(px, instrument, index_price).ok(),
                None => None,
            }
        }
        Some(_) => return Err(invalid(P, "mark price does not match the instrument")),
        None => None,
    };
    Ok(BookSnapshot {
        instrument: instrument.key.clone(),
        bids: levels(book, "bids", instrument, index_price)?,
        asks: levels(book, "asks", instrument, index_price)?,
        source_ts: Some(ms(&book["ts"], "ts")?),
        received_at,
        index_price,
        mark_price,
        provenance,
        access: BookAccess::Api,
    })
}

/// One side of a `/market/tickers` row as a `/market/books` side: `[[px, sz]]`, or `[]` when the
/// side is empty (`px ""` / `sz "0"`). Never invents a level.
fn ticker_side(t: &Value, px: &str, sz: &str) -> Result<Value> {
    let p = opt_dec(P, t.get(px), px)?;
    let s = opt_dec(P, t.get(sz), sz)?;
    Ok(match (p, s) {
        (Some(p), Some(s)) if p > Decimal::ZERO && s > Decimal::ZERO => {
            serde_json::json!([[t[px], t[sz]]])
        }
        _ => serde_json::json!([]),
    })
}

/// `tickers` / `marks`: `data` arrays of `/market/tickers` and `/public/mark-price` (instType OPTION,
/// one `uly`); `index`: one element of `/market/index-tickers`. Tickers of instruments absent from
/// `catalog` (not live, expired, other underlying) are skipped. Each ticker becomes a 1-level book
/// parsed by `parse_book`, so units and conversion are exactly those of `get_market_data`.
pub fn parse_bulk_tops(
    tickers: &Value,
    catalog: &HashMap<String, Instrument>,
    index: &Value,
    marks: Option<&Value>,
    received_at: DateTime<Utc>,
    provenance: Provenance,
) -> Result<Vec<BookSnapshot>> {
    let arr = tickers
        .as_array()
        .ok_or_else(|| invalid(P, "tickers: not an array"))?;
    let marks: HashMap<&str, &Value> = marks
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m["instId"].as_str()?, m)))
        .collect();
    let mut out = Vec::with_capacity(arr.len());
    for t in arr {
        let Some(instrument) = t["instId"].as_str().and_then(|id| catalog.get(id)) else {
            continue;
        };
        if instrument.expiry.is_some_and(|e| e <= received_at) {
            continue;
        }
        let book = serde_json::json!({
            "bids": ticker_side(t, "bidPx", "bidSz")?,
            "asks": ticker_side(t, "askPx", "askSz")?,
            "ts": t["ts"],
        });
        out.push(parse_book(
            instrument,
            &book,
            Some(index),
            marks.get(instrument.key.native_id.as_str()).copied(),
            received_at,
            provenance,
        )?);
    }
    Ok(out)
}

/// Min(0.03 % × underlying notional at the OKX index, 7 % × premium), both in USD(C)-equivalent.
pub fn fee_schedule(instrument: &Instrument) -> Result<FeeSchedule> {
    let inverse = is_inverse(&instrument.native)?;
    let part = || FeeFormula::Min {
        parts: vec![
            FeeFormula::PctOfUnderlyingNotional { rate: FEE_RATE_LV1 },
            FeeFormula::PctOfPremium { rate: PREMIUM_CAP },
        ],
    };
    Ok(FeeSchedule {
        taker: part(),
        maker: part(),
        currency: instrument.premium_currency.clone(),
        source_url: FEE_SOURCE.into(),
        verified_on: "2026-10-06".into(),
        notes: format!(
            "Regular (Lv1) maker/taker 0.03 % of underlying, capped at 7 % of premium. {} VIP tiers / RFQ combo discounts not modelled. Exercise fee (Min(0.02 %, taker rate, 7 % of settlement value); none for non-Friday dailies) not included. Centralised exchange: needs the user's own account (KYC).",
            if inverse {
                "Charged in the coin; converted to USD at the OKX index like the premium."
            } else {
                "USD-margined (_UM): fee formula documented for options generally, not specifically confirmed for _UM."
            }
        ),
    })
}

#[async_trait]
impl Connector for OkxConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let uly = format!("{underlying}-USD");
        let v = self
            .get(
                "/api/v5/public/instruments",
                &[("instType", "OPTION"), ("uly", &uly)],
            )
            .await?;
        let mut out = parse_instruments(&v, underlying, Utc::now())?;
        // The engine keeps ONE listing per (kind, strike, expiry) per venue: the deep coin-margined
        // book first, the thin regional `_UM` one after it.
        out.sort_by_key(|i| i.key.native_id.contains("_UM"));
        let by_id = out
            .iter()
            .map(|i| (i.key.native_id.clone(), i.clone()))
            .collect();
        self.catalogs
            .lock()
            .expect("catalog lock")
            .insert(underlying.into(), (Instant::now(), Arc::new(by_id)));
        Ok(out)
    }

    /// One `/market/tickers` (both families) + one `/public/mark-price` + one index per call.
    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        let uly = format!("{underlying}-USD");
        let tickers_q = [("instType", "OPTION"), ("uly", uly.as_str())];
        let index_q = [("instId", uly.as_str())];
        let (tickers, index, marks, catalog) = tokio::join!(
            self.get("/api/v5/market/tickers", &tickers_q),
            self.get("/api/v5/market/index-tickers", &index_q),
            self.get("/api/v5/public/mark-price", &tickers_q),
            self.catalog(underlying),
        );
        let received_at = Utc::now();
        // Index mandatory (inverse conversion, never a guessed price); mark is an enrichment.
        let (tickers, index, catalog) = (tickers?, index?, catalog?);
        parse_bulk_tops(
            &tickers,
            &catalog,
            first(&index, "index-tickers")?,
            marks.ok().as_ref(),
            received_at,
            Provenance::LiveData,
        )
        .map(Some)
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        // Stream first (no waiting: a cold or stale book goes to REST while the stream warms).
        if let Some(s) = &self.stream {
            if let Some(md) = self.stream_book(s, instrument, Duration::ZERO).await? {
                return Ok(md);
            }
            tracing::debug!(instrument = %instrument.key, "okx stream not ready, REST fallback");
        }
        let id = instrument.key.native_id.as_str();
        let uly = format!("{}-USD", instrument.underlying);
        let book_q = [("instId", id), ("sz", BOOK_DEPTH)];
        let index_q = [("instId", uly.as_str())];
        let mark_q = [("instType", "OPTION"), ("instId", id)];
        let t0 = std::time::Instant::now();
        let (book, index, mark) = tokio::join!(
            self.get("/api/v5/market/books", &book_q),
            self.get("/api/v5/market/index-tickers", &index_q),
            self.get("/api/v5/public/mark-price", &mark_q),
        );
        let network = t0.elapsed();
        let received_at = Utc::now();
        let t1 = std::time::Instant::now();
        // Book mandatory. Index mandatory for inverse (conversion; parse_book errors without it),
        // otherwise an enrichment. Mark is an enrichment (failure → None).
        let book = book?;
        let index = index.ok();
        let mark = mark.ok();
        let book = parse_book(
            instrument,
            first(&book, "books")?,
            index.as_ref().and_then(|v| v.get(0)),
            mark.as_ref().and_then(|v| v.get(0)),
            received_at,
            Provenance::LiveData,
        )?;
        Ok(crate::MarketData::rest(book, network, t1))
    }

    fn prewarm(&self, instruments: &[Instrument]) {
        if let Some(s) = &self.stream {
            for i in instruments {
                s.subscribe(&i.key.native_id, &format!("{}-USD", i.underlying));
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
    use oa_core::fees::{FeeAmount, FeeInputs};
    use rust_decimal_macros::dec;

    fn fixture(name: &str) -> Value {
        data(&crate::fixture("recorded-2026-10-06", name), name).unwrap()
    }
    fn at() -> DateTime<Utc> {
        "2026-10-06T09:00:00Z".parse().unwrap()
    }
    fn inst(id: &str, underlying: &str) -> Instrument {
        parse_instruments(&fixture("okx_instruments.json"), underlying, at())
            .unwrap()
            .into_iter()
            .find(|i| i.key.native_id == id)
            .unwrap()
    }

    #[test]
    fn parses_both_families_with_exact_multipliers() {
        let v = fixture("okx_instruments.json");
        let btc = parse_instruments(&v, "BTC", at()).unwrap();
        let eth = parse_instruments(&v, "ETH", at()).unwrap();
        assert_eq!(btc.len(), 4);
        // preopen ETH _UM skipped.
        assert_eq!(eth.len(), 2);

        let c = inst("BTC-USD-261007-80000-C", "BTC");
        assert_eq!(c.strike, Some(dec!(80000)));
        assert_eq!(c.option_kind, Some(OptionKind::Call));
        assert_eq!(c.expiry.unwrap().to_rfc3339(), "2026-10-07T08:00:00+00:00");
        assert_eq!(c.contract_multiplier, dec!(0.01));
        assert_eq!(c.premium_currency, "USDC");
        assert_eq!(c.settlement_asset.as_deref(), Some("BTC"));
        assert_eq!(c.native["converted_at_index"], true);
        assert_eq!(c.tick_size, None);
        assert_eq!(c.min_amount, Some(dec!(1)));
        assert_eq!(
            inst("BTC-USD-261007-80000-P", "BTC").option_kind,
            Some(OptionKind::Put)
        );

        let um = inst("BTC-USD_UM-261009-86000-C", "BTC");
        assert_eq!(um.contract_multiplier, dec!(0.01));
        assert_eq!(um.settlement_asset.as_deref(), Some("USD"));
        assert_eq!(um.native["converted_at_index"], false);
        assert_eq!(um.tick_size, Some(dec!(0.05))); // 5 USD per BTC × 0.01

        // ETH inverse contract = 0.1 ETH, ETH _UM contract = 0.01 ETH.
        assert_eq!(
            inst("ETH-USD-261007-2500-C", "ETH").contract_multiplier,
            dec!(0.1)
        );
        assert_eq!(
            inst("ETH-USD_UM-261007-2450-C", "ETH").contract_multiplier,
            dec!(0.01)
        );
    }

    #[test]
    fn inverse_book_converted_to_usd_per_contract() {
        let i = inst("BTC-USD-261009-86000-C", "BTC");
        let b = parse_book(
            &i,
            &fixture("okx_book_btc_inverse.json")[0],
            Some(&fixture("okx_index_btc.json")[0]),
            Some(&fixture("okx_mark_btc_inverse.json")[0]),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(b.index_price, Some(dec!(85966.1)));
        // ask 0.0105 BTC per BTC × 0.01 BTC/contract × 85,966.1 = 9.02644050 USD per contract.
        assert_eq!(b.asks[0].price, dec!(9.02644050));
        assert_eq!(b.asks[0].size, dec!(3197));
        assert_eq!(b.bids[0].price, dec!(0.01) * dec!(0.01) * dec!(85966.1));
        assert!(b.asks.windows(2).all(|w| w[0].price <= w[1].price));
        assert!(b.bids.windows(2).all(|w| w[0].price >= w[1].price));
        assert_eq!(
            b.mark_price,
            Some(dec!(0.0102690505631982) * dec!(0.01) * dec!(85966.1))
        );
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791280771408);
        assert_eq!(b.access, BookAccess::Api);
        // Without an index the coin book cannot be converted: error, never a guess.
        assert!(
            parse_book(
                &i,
                &fixture("okx_book_btc_inverse.json")[0],
                None,
                None,
                at(),
                Provenance::Fixture
            )
            .is_err()
        );
    }

    #[test]
    fn um_book_is_usd_per_unit_times_multiplier() {
        let i = inst("BTC-USD_UM-261009-86000-C", "BTC");
        let b = parse_book(
            &i,
            &fixture("okx_book_btc_um.json")[0],
            None,
            None,
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        // 930 USD per BTC × 0.01 = 9.30 USD per contract.
        assert_eq!(b.asks[0].price, dec!(9.30));
        assert_eq!(b.asks[0].size, dec!(798));
        assert_eq!(b.bids[0].price, dec!(8.55));
    }

    #[test]
    fn fee_matches_okx_worked_example_and_cap() {
        let f = fee_schedule(&inst("BTC-USD-261009-86000-C", "BTC")).unwrap();
        // OKX FAQ: 100 contracts × 0.01 BTC, premium 0.05 BTC/BTC, taker 0.03 % → 0.0003 BTC.
        // At index 86,000: 0.0003 BTC = 25.8 USD; premium = 0.05 × 1 × 86,000 = 4,300 USD.
        let fee = f.taker.compute(&FeeInputs {
            qty_native: dec!(100),
            qty_underlying: dec!(1),
            premium: dec!(4300),
            index_price: Some(dec!(86000)),
            mark_price: None,
        });
        assert_eq!(fee, FeeAmount::Known { amount: dec!(25.8) });
        // Deep OTM: premium 0.0005 BTC/BTC → 43 USD for 1 BTC; cap 7 % = 3.01 < 25.8.
        let fee = f.taker.compute(&FeeInputs {
            qty_native: dec!(100),
            qty_underlying: dec!(1),
            premium: dec!(43),
            index_price: Some(dec!(86000)),
            mark_price: None,
        });
        assert_eq!(fee, FeeAmount::Known { amount: dec!(3.01) });
        assert_eq!(f.currency, "USDC");
    }

    /// Fixtures recorded in the same instant (tops agreed) for tickers, books, index and marks.
    #[test]
    fn bulk_tops_equal_full_book_tops() {
        let cat: HashMap<String, Instrument> = ["BTC", "ETH"]
            .iter()
            .flat_map(|u| parse_instruments(&fixture("okx_instruments.json"), u, at()).unwrap())
            .map(|i| (i.key.native_id.clone(), i))
            .collect();
        let index = fixture("okx_bulk_index_btc.json");
        let marks = fixture("okx_bulk_mark_btc.json");
        let tops = parse_bulk_tops(
            &fixture("okx_bulk_tickers_btc.json"),
            &cat,
            &index[0],
            Some(&marks),
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert_eq!(tops.len(), 4);
        let top = |id: &str| tops.iter().find(|b| b.instrument.native_id == id).unwrap();
        for (id, book) in [
            ("BTC-USD-261009-86000-C", "okx_bulk_book_inverse.json"),
            ("BTC-USD_UM-261009-86000-C", "okx_bulk_book_um.json"),
        ] {
            let i = &cat[id];
            let mark = marks.as_array().unwrap().iter().find(|m| m["instId"] == id);
            let full = parse_book(
                i,
                &fixture(book)[0],
                Some(&index[0]),
                mark,
                at(),
                Provenance::Fixture,
            )
            .unwrap();
            let b = top(id);
            assert_eq!(b.asks.len(), 1);
            assert_eq!(b.bids.len(), 1);
            assert_eq!(b.asks[0], full.asks[0], "{id} ask");
            assert_eq!(b.bids[0], full.bids[0], "{id} bid");
            assert_eq!(b.index_price, full.index_price);
            assert_eq!(b.mark_price, full.mark_price);
            assert_eq!(b.access, BookAccess::Api);
            assert!(b.source_ts.is_some());
        }
        // Worked example (inverse): ask 0.013 BTC per BTC x 0.01 BTC/contract x index 86,310
        // = 11.2203 USD per contract, 2,349 contracts; `_UM`: 1,155 USD x 0.01 = 11.55 USD.
        let inv = top("BTC-USD-261009-86000-C");
        assert_eq!(
            inv.asks[0],
            Level {
                price: dec!(11.2203),
                size: dec!(2349)
            }
        );
        assert_eq!(inv.bids[0].price, dec!(0.0125) * dec!(0.01) * dec!(86310));
        assert_eq!(inv.source_ts.unwrap().timestamp_millis(), 1791287294095);
        let um = top("BTC-USD_UM-261009-86000-C");
        assert_eq!(um.asks[0].price, dec!(11.55));
        assert_eq!(um.bids[0].price, dec!(10.80));
        // Empty bid (`bidPx ""`, `bidSz "0"`) → no level, ask kept.
        let one = top("BTC-USD-261007-80000-P");
        assert!(one.bids.is_empty());
        assert_eq!(one.asks[0].size, dec!(5867));
        // Without the catalog entry nothing is emitted (never a guessed multiplier).
        let none = parse_bulk_tops(
            &fixture("okx_bulk_tickers_btc.json"),
            &HashMap::new(),
            &index[0],
            None,
            at(),
            Provenance::Fixture,
        )
        .unwrap();
        assert!(none.is_empty());
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_okx_bulk_tops_vs_full_book() {
        let c = OkxConnector::default();
        for u in ["BTC", "ETH"] {
            let tops = c.bulk_top_of_book(u).await.unwrap().unwrap();
            let two = tops
                .iter()
                .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
                .count();
            let um = tops
                .iter()
                .filter(|b| b.instrument.native_id.contains("_UM"))
                .count();
            println!(
                "okx {u} bulk: {} instruments ({um} _UM), {two} two-sided",
                tops.len()
            );
            assert!(two > 0);
            let b = tops
                .iter()
                .find(|b| {
                    !b.instrument.native_id.contains("_UM")
                        && !b.bids.is_empty()
                        && !b.asks.is_empty()
                })
                .unwrap();
            let cat = c.catalog(u).await.unwrap();
            let full = c
                .get_market_data(&cat[&b.instrument.native_id])
                .await
                .unwrap()
                .book;
            println!(
                "okx {} bulk bid {:?} ask {:?} | full bid {:?} ask {:?} | mark {:?}/{:?}",
                b.instrument.native_id,
                b.bids.first(),
                b.asks.first(),
                full.bids.first(),
                full.asks.first(),
                b.mark_price,
                full.mark_price
            );
            // Same units: tops within 10 % of each other (the book moves between the two calls).
            let near = |a: Decimal, f: Decimal| (a - f).abs() <= f * dec!(0.1);
            assert!(near(b.asks[0].price, full.asks[0].price));
            assert!(near(b.bids[0].price, full.bids[0].price));
        }
    }

    /// 3 liquid BTC-USD (inverse) options: first-message latency, then stream top vs REST top.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_okx_stream_vs_rest() {
        let rest = OkxConnector::default();
        let c = OkxConnector::default().with_stream();
        let cat = c.catalog("BTC").await.unwrap();
        let mut tops = rest.bulk_top_of_book("BTC").await.unwrap().unwrap();
        tops.retain(|b| {
            !b.instrument.native_id.contains("_UM") && !b.bids.is_empty() && !b.asks.is_empty()
        });
        tops.sort_by_key(|b| std::cmp::Reverse(b.bids[0].size.min(b.asks[0].size)));
        let picks: Vec<Instrument> = tops
            .iter()
            .take(3)
            .map(|b| cat[&b.instrument.native_id].clone())
            .collect();
        assert_eq!(picks.len(), 3);
        let s = c.stream.as_ref().unwrap();
        for i in &picks {
            let t0 = Instant::now();
            let first = c.stream_book(s, i, Duration::from_secs(5)).await.unwrap(); // new subscription: NotYet at once
            assert!(first.is_none());
            let md = loop {
                if let Some(md) = c.stream_book(s, i, Duration::from_secs(5)).await.unwrap() {
                    break md;
                }
                assert!(t0.elapsed() < Duration::from_secs(8), "no streamed book");
            };
            println!(
                "okx stream {} first usable book after {:?}",
                i.key.native_id,
                t0.elapsed()
            );
            assert_eq!(md.path, "ws_stream");
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let near = |a: Decimal, f: Decimal| (a - f).abs() <= f * rust_decimal_macros::dec!(0.1);
        for i in &picks {
            let (s_md, r_md) = tokio::join!(c.get_market_data(i), rest.get_market_data(i));
            let (s_md, r_md) = (s_md.unwrap(), r_md.unwrap());
            println!(
                "okx {} stream ({:?}, age {} ms) bid {:?} ask {:?} levels {}/{} | rest ({:?}) bid {:?} ask {:?} levels {}/{} | idx {:?}/{:?} mark {:?}/{:?}",
                i.key.native_id,
                s_md.network,
                (Utc::now() - s_md.book.received_at).num_milliseconds(),
                s_md.book.bids.first(),
                s_md.book.asks.first(),
                s_md.book.bids.len(),
                s_md.book.asks.len(),
                r_md.network,
                r_md.book.bids.first(),
                r_md.book.asks.first(),
                r_md.book.bids.len(),
                r_md.book.asks.len(),
                s_md.book.index_price,
                r_md.book.index_price,
                s_md.book.mark_price,
                r_md.book.mark_price,
            );
            assert_eq!(s_md.path, "ws_stream");
            assert_eq!(r_md.path, "rest");
            assert!(s_md.network < Duration::from_millis(20));
            assert!(near(s_md.book.asks[0].price, r_md.book.asks[0].price));
            assert!(near(s_md.book.bids[0].price, r_md.book.bids[0].price));
            assert!(s_md.book.mark_price.is_some());
        }
        println!("diagnostics {}", c.diagnostics().unwrap());
        let d = c.diagnostics().unwrap();
        assert_eq!(d["sequence_breaks"], 0);
        assert_eq!(d["connected"], true);
    }

    /// Demo host (`wspap`, `x-simulated-trading: 1`) streams demo books.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_okx_demo_stream() {
        let c = OkxConnector::demo().with_stream();
        let cat = c.list_instruments("BTC").await.unwrap();
        let i = cat
            .iter()
            .filter(|i| i.native["ctType"] == "inverse")
            .min_by_key(|i| i.expiry)
            .unwrap();
        let s = c.stream.as_ref().unwrap();
        assert!(c.stream_book(s, i, Duration::ZERO).await.unwrap().is_none());
        let md = c
            .stream_book(s, i, Duration::from_secs(8))
            .await
            .unwrap()
            .expect("demo streamed book");
        println!(
            "okx demo {} bids {} asks {} idx {:?} ; {}",
            i.key.native_id,
            md.book.bids.len(),
            md.book.asks.len(),
            md.book.index_price,
            c.diagnostics().unwrap()
        );
    }

    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_okx_options() {
        let c = OkxConnector::default();
        let cat = c.list_instruments("BTC").await.unwrap();
        assert!(!cat.is_empty());
        let inv = cat
            .iter()
            .filter(|i| i.native["ctType"] == "inverse")
            .count();
        println!(
            "okx BTC: {} options ({inv} inverse, {} _UM)",
            cat.len(),
            cat.len() - inv
        );
        let i = cat
            .iter()
            .find(|i| i.native["ctType"] == "inverse")
            .unwrap();
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "okx {} mult {} index {:?} mark {:?} best ask {:?} best bid {:?} net {:?}",
            i.key.native_id,
            i.contract_multiplier,
            md.book.index_price,
            md.book.mark_price,
            md.book.asks.first(),
            md.book.bids.first(),
            md.network
        );
    }
}
