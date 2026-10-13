//! Quote-currency adapter: presents a USDT-quoted venue in USDC so every venue is ranked in ONE
//! currency, without the hidden "1 USDT = 1 USDC" assumption.
//!
//! Rate: Kraken public ticker (no key) `USDTZUSD` and `USDCUSD` last trades -> USDC per USDT =
//! (USD per USDT) / (USD per USDC). Probed 2026-10-06: 0.99966 / 0.99980 -> 0.99986. Cached 30 s.
//! What changes: book prices, index/mark prices -> USDC; `premium_currency` -> "USDC"; fee schedule
//! currency -> "USDC" (percentage formulas only; fixed/per-contract amounts are converted too).
//! What stays: `settlement_asset` / `collateral_assets` keep "USDT", so comparability flags
//! SETTLEMENT_ASSET_DIFFERS and the user must accept it. The rate used is stored in the instrument's
//! `native.fx` and documented in the fee notes.

use crate::http::{client, read_json};
use crate::{Connector, Implemented, MarketData, Result, dec, invalid};
use async_trait::async_trait;
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const KRAKEN: &str = "https://api.kraken.com/0/public/Ticker?pair=USDTZUSD,USDCUSD";
const TTL: Duration = Duration::from_secs(30);

/// USDC per 1 USDT from a Kraken ticker response.
pub fn usdc_per_usdt(v: &serde_json::Value, p: PlatformId) -> Result<Decimal> {
    if v["error"].as_array().is_some_and(|e| !e.is_empty()) {
        return Err(invalid(p, format!("fx: {}", v["error"])));
    }
    let usdt = dec(p, &v["result"]["USDTZUSD"]["c"][0], "USDTZUSD")?;
    let usdc = dec(p, &v["result"]["USDCUSD"]["c"][0], "USDCUSD")?;
    if usdc <= Decimal::ZERO || usdt <= Decimal::ZERO {
        return Err(invalid(p, "fx: non-positive rate"));
    }
    let r = usdt / usdc;
    // Sanity band: a stablecoin de-peg beyond 2 % must stop comparisons, not be silently converted.
    if (r - Decimal::ONE).abs() > Decimal::new(2, 2) {
        return Err(invalid(p, format!("fx: USDT/USDC {r} outside +-2 %")));
    }
    Ok(r.round_dp(8))
}

fn convert_formula(f: &FeeFormula, rate: Decimal) -> FeeFormula {
    match f {
        FeeFormula::PerContract { amount } => FeeFormula::PerContract {
            amount: (*amount * rate).round_dp(8),
        },
        FeeFormula::Fixed { amount } => FeeFormula::Fixed {
            amount: (*amount * rate).round_dp(8),
        },
        FeeFormula::Sum { parts } => FeeFormula::Sum {
            parts: parts.iter().map(|x| convert_formula(x, rate)).collect(),
        },
        FeeFormula::Min { parts } => FeeFormula::Min {
            parts: parts.iter().map(|x| convert_formula(x, rate)).collect(),
        },
        // Percentages of premium / notional / mark are currency-neutral once inputs are converted.
        other => other.clone(),
    }
}

/// Process-wide USDC/USDT rate (one Kraken call serves every USDT venue).
static SHARED_RATE: std::sync::Mutex<Option<(Instant, Decimal)>> = std::sync::Mutex::new(None);
static REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// A rate older than this is never served (fetched inline instead).
const MAX_STALE: std::time::Duration = std::time::Duration::from_secs(120);

async fn fetch_rate(http: &reqwest::Client, p: PlatformId) -> Result<Decimal> {
    let v = read_json(p, http.get(KRAKEN).send().await).await?;
    let r = usdc_per_usdt(&v, p)?;
    *SHARED_RATE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), r));
    Ok(r)
}

pub struct UsdtAsUsdc<C> {
    inner: C,
    http: reqwest::Client,
    cache: Mutex<Option<(Instant, Decimal)>>,
    /// Whether this venue's instruments are USDT-quoted (learned from `list_instruments`):
    /// 0 = unknown, 1 = yes, 2 = no. Avoids re-listing instruments on every bulk refresh.
    usdt: std::sync::atomic::AtomicU8,
}

impl<C: Connector> UsdtAsUsdc<C> {
    pub fn new(inner: C) -> Self {
        Self {
            inner,
            http: client(),
            cache: Mutex::new(None),
            usdt: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// USDC per USDT, shared by every USDT venue. Fresh (< TTL) or recently stale (< MAX_STALE) values
    /// are answered at once — a stale one also triggers ONE background refresh — so book fetches never
    /// queue behind a Kraken round trip. Only a cold or very stale cache fetches inline (single-flight).
    async fn rate(&self) -> Result<Decimal> {
        let cached = *SHARED_RATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, r)) = cached {
            if at.elapsed() < TTL {
                return Ok(r);
            }
            if at.elapsed() < MAX_STALE {
                if !REFRESHING.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    let (http, p) = (self.http.clone(), self.inner.platform());
                    tokio::spawn(async move {
                        let _ = fetch_rate(&http, p).await;
                        REFRESHING.store(false, std::sync::atomic::Ordering::Release);
                    });
                }
                return Ok(r);
            }
        }
        let _g = self.cache.lock().await;
        if let Some((at, r)) = *SHARED_RATE.lock().unwrap_or_else(|e| e.into_inner())
            && at.elapsed() < TTL
        {
            return Ok(r);
        }
        fetch_rate(&self.http, self.inner.platform()).await
    }
}

pub fn convert_instrument(mut i: Instrument, rate: Decimal) -> Instrument {
    if i.premium_currency == "USDT" {
        i.premium_currency = "USDC".into();
        if let Some(o) = i.native.as_object_mut() {
            o.insert("fx".into(), serde_json::json!({"premium_native_currency": "USDT", "usdc_per_usdt": rate.to_string(), "source": "kraken USDTZUSD/USDCUSD"}));
        }
        // Tick is in USDT; leave it as is (only used for limit-price rounding at execution time).
    }
    i
}

pub fn convert_book(mut b: BookSnapshot, rate: Decimal) -> BookSnapshot {
    for l in b.bids.iter_mut().chain(b.asks.iter_mut()) {
        l.price = (l.price * rate).round_dp(8);
    }
    b.index_price = b.index_price.map(|x| (x * rate).round_dp(8));
    b.mark_price = b.mark_price.map(|x| (x * rate).round_dp(8));
    b
}

#[async_trait]
impl<C: Connector> Connector for UsdtAsUsdc<C> {
    fn platform(&self) -> PlatformId {
        self.inner.platform()
    }
    fn network(&self) -> &'static str {
        self.inner.network()
    }
    fn implemented(&self, cap: Capability) -> Implemented {
        self.inner.implemented(cap)
    }
    fn streams_books(&self) -> bool {
        self.inner.streams_books()
    }
    fn prewarm(&self, instruments: &[Instrument]) {
        self.inner.prewarm(instruments)
    }
    fn hot_refresh_interval(&self) -> Duration {
        self.inner.hot_refresh_interval()
    }
    fn bulk_refresh_interval(&self) -> Duration {
        self.inner.bulk_refresh_interval()
    }

    async fn bulk_top_of_book(&self, underlying: &str) -> Result<Option<Vec<BookSnapshot>>> {
        let Some(books) = self.inner.bulk_top_of_book(underlying).await? else {
            return Ok(None);
        };
        // Same rule as get_market_data: only venues whose instruments were converted are converted.
        let usdt = match self.usdt.load(std::sync::atomic::Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => self
                .list_instruments(underlying)
                .await
                .map(|v| v.iter().any(|i| i.native.get("fx").is_some()))
                .unwrap_or(false),
        };
        if !usdt {
            return Ok(Some(books));
        }
        let r = self.rate().await?;
        Ok(Some(
            books.into_iter().map(|b| convert_book(b, r)).collect(),
        ))
    }
    fn diagnostics(&self) -> Option<serde_json::Value> {
        self.inner.diagnostics()
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let v = self.inner.list_instruments(underlying).await?;
        let usdt = v.iter().any(|i| i.premium_currency == "USDT");
        self.usdt.store(
            if usdt { 1 } else { 2 },
            std::sync::atomic::Ordering::Relaxed,
        );
        if !usdt {
            return Ok(v);
        }
        let r = self.rate().await?;
        Ok(v.into_iter().map(|i| convert_instrument(i, r)).collect())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<MarketData> {
        // The engine holds the CONVERTED instrument; the inner connector expects its own currency.
        let mut native = instrument.clone();
        let converted = native.native.get("fx").is_some();
        if converted {
            native.premium_currency = "USDT".into();
        }
        let mut md = self.inner.get_market_data(&native).await?;
        if converted {
            let r = self.rate().await?;
            md.book = convert_book(md.book, r);
        }
        Ok(md)
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        let Some(fx) = instrument.native.get("fx") else {
            return self.inner.get_fees(instrument);
        };
        let rate: Decimal = fx["usdc_per_usdt"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Decimal::ONE);
        let mut native = instrument.clone();
        native.premium_currency = "USDT".into();
        let mut f = self.inner.get_fees(&native)?;
        f.taker = convert_formula(&f.taker, rate);
        f.maker = convert_formula(&f.maker, rate);
        f.currency = "USDC".into();
        f.notes = format!(
            "{} Converted from USDT at {rate} USDC/USDT (Kraken).",
            f.notes
        );
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn rate_from_kraken_and_depeg_guard() {
        let ok = json!({"error": [], "result": {"USDTZUSD": {"c": ["0.99966", "1"]}, "USDCUSD": {"c": ["0.99980", "1"]}}});
        assert_eq!(
            usdc_per_usdt(&ok, PlatformId::Binance).unwrap(),
            dec!(0.99985997)
        );
        let depeg = json!({"error": [], "result": {"USDTZUSD": {"c": ["0.95", "1"]}, "USDCUSD": {"c": ["1.0", "1"]}}});
        assert!(usdc_per_usdt(&depeg, PlatformId::Binance).is_err());
    }

    #[test]
    fn book_and_fixed_fees_are_converted_percentages_untouched() {
        let b = BookSnapshot {
            instrument: InstrumentKey {
                platform: PlatformId::Binance,
                network: "x".into(),
                native_id: "X".into(),
            },
            bids: vec![Level {
                price: dec!(100),
                size: dec!(1),
            }],
            asks: vec![Level {
                price: dec!(110),
                size: dec!(1),
            }],
            source_ts: None,
            received_at: chrono::Utc::now(),
            index_price: Some(dec!(80000)),
            mark_price: None,
            provenance: Provenance::Fixture,
            access: BookAccess::Api,
        };
        let c = convert_book(b, dec!(0.999));
        assert_eq!(c.asks[0].price, dec!(109.89));
        assert_eq!(c.index_price, Some(dec!(79920)));
        let f = FeeFormula::Min {
            parts: vec![
                FeeFormula::PctOfPremium { rate: dec!(0.1) },
                FeeFormula::Fixed { amount: dec!(10) },
            ],
        };
        assert_eq!(
            convert_formula(&f, dec!(0.999)),
            FeeFormula::Min {
                parts: vec![
                    FeeFormula::PctOfPremium { rate: dec!(0.1) },
                    FeeFormula::Fixed { amount: dec!(9.99) }
                ]
            }
        );
    }
}
