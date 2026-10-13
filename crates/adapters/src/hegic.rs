//! Hegic (Arbitrum) — READ-ONLY, ask side. Pool-underwritten options priced on-chain.
//!
//! Sources (probed 2026-10-08):
//! - PriceCalculator contracts (addresses from the Hegic app bundle, verified by eth_call):
//!   `calculatePremium(uint256 period_s, uint256 amount, uint256 strike_1e8)` (selector 0x156f1d74)
//!   -> premium in USDC (6 decimals). amount: 1e18 per ETH, 1e8 per BTC.
//!   [LIVE] any period from ~7 to 91 days and any strike within bounds (calls about -11 % .. +10 %
//!   of spot, puts wider) are accepted; outside -> revert 0xf7ce2970.
//! - Spot: Chainlink ETH/USD and BTC/USD feeds on Arbitrum (`latestAnswer`, 8 decimals).
//!
//! Product (secondary sources; doc.hegic.co no longer resolves): AMERICAN options (exercise any time
//! before expiry), cash-settled in USDC; the premium is all-in (no separate trading fee).
//! EXECUTION CAVEAT (app bundle ABI, 2026-10-08): `OperationalTreasury.buy(strategy, holder, amount,
//! period, additional)` takes NO strike: strategies are fixed moneyness (CALL_100 = ATM, 110/120/130 %
//! OTM) and the strike is set to spot at purchase. A fixed listed strike is therefore not executable
//! as such: books are tagged `BookAccess::IndicativeRfq` (shown, never ranked or routed).
//! Mapping: Hegic has no listed contracts -> we expose the contracts other venues list: Fridays
//! 08:00 UTC 7-90 days out, strikes on the usual grid (BTC 1000, ETH 50) near spot. Each book is
//! ONE ask level = calculatePremium for that exact expiry and strike. Size: the pool's limit is not
//! read; a conservative nominal size (0.5 BTC / 10 ETH) is used.

use crate::http::{client, read_json};
use crate::{Connector, Result, invalid};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration as CDuration, TimeZone, Utc, Weekday};
use oa_core::Decimal;
use oa_core::domain::*;
use oa_core::fees::{FeeFormula, FeeSchedule};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const RPC: &str = "https://arb1.arbitrum.io/rpc";
pub const NETWORK: &str = "hegic-arbitrum";
pub const SPEC_VERSION: &str = "hegic-v8888/2026-10-08";
const P: PlatformId = PlatformId::Hegic;
const SPOT_TTL: Duration = Duration::from_secs(10);

struct Market {
    underlying: &'static str,
    call: &'static str,
    put: &'static str,
    feed: &'static str,
    amount_unit: u128,
    step: i64,
    nominal_size: Decimal,
}

const MARKETS: [Market; 2] = [
    Market {
        underlying: "ETH",
        call: "0xD60D991667ac1Dae7fDb6D914a2ae5ffA1d209F2",
        put: "0x3614Ee77E8e2F5363e4C2eadC0AeD51C1977CBD4",
        feed: "0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612",
        amount_unit: 1_000_000_000_000_000_000,
        step: 50,
        nominal_size: Decimal::from_parts(10, 0, 0, false, 0),
    },
    Market {
        underlying: "BTC",
        call: "0xCD36fff23d57a2c8777B7a5999689de5b2de5834",
        put: "0xd0691aa2e101Ed268E83c8D8c33493c49AE4885F",
        feed: "0x6ce185860a4963106506C203335A2910413708e9",
        amount_unit: 100_000_000,
        step: 1000,
        nominal_size: Decimal::from_parts(5, 0, 0, false, 1),
    },
];

fn market(u: &str) -> Option<&'static Market> {
    MARKETS.iter().find(|m| m.underlying == u)
}

pub struct HegicConnector {
    http: reqwest::Client,
    rpc: String,
    spot: Mutex<Vec<(String, Instant, Decimal)>>,
}

impl Default for HegicConnector {
    fn default() -> Self {
        Self::new(RPC)
    }
}

impl HegicConnector {
    pub fn new(rpc: &str) -> Self {
        Self {
            http: client(),
            rpc: rpc.into(),
            spot: Mutex::new(vec![]),
        }
    }

    async fn eth_call(&self, to: &str, data: String) -> Result<String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": [{"to": to, "data": data}, "latest"]});
        let v = read_json(P, self.http.post(&self.rpc).json(&body).send().await).await?;
        if let Some(e) = v.get("error") {
            return Err(invalid(P, format!("eth_call reverted: {e}")));
        }
        v["result"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| invalid(P, "eth_call: no result"))
    }

    async fn spot(&self, m: &Market) -> Result<Decimal> {
        {
            let g = self.spot.lock().await;
            if let Some((_, _, s)) = g
                .iter()
                .find(|(u, at, _)| u == m.underlying && at.elapsed() < SPOT_TTL)
            {
                return Ok(*s);
            }
        }
        let r = self.eth_call(m.feed, "0x50d25bcd".into()).await?;
        let s = decode_uint(&r, 8).ok_or_else(|| invalid(P, "chainlink latestAnswer"))?;
        let mut g = self.spot.lock().await;
        g.retain(|(u, _, _)| u != m.underlying);
        g.push((m.underlying.to_string(), Instant::now(), s));
        Ok(s)
    }
}

/// First 32-byte word as an unsigned integer scaled by 10^-decimals (None if it exceeds u128).
pub fn decode_uint(hex_word: &str, decimals: u32) -> Option<Decimal> {
    let h = hex_word.trim_start_matches("0x");
    let word = h.get(..64)?;
    if !word[..32].bytes().all(|b| b == b'0') {
        return None;
    }
    let n = u128::from_str_radix(&word[32..], 16).ok()?;
    Some(Decimal::from_i128_with_scale(i128::try_from(n).ok()?, decimals).normalize())
}

pub fn premium_calldata(period_s: u64, amount: u128, strike: Decimal) -> Option<String> {
    use rust_decimal::prelude::ToPrimitive;
    let k = (strike * Decimal::from(100_000_000)).trunc().to_u128()?;
    Some(format!("0x156f1d74{period_s:064x}{amount:064x}{k:064x}"))
}

/// Fridays 08:00 UTC between 7 and 90 days after `now`.
pub fn expiries(now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut d = now.date_naive();
    let mut out = vec![];
    for _ in 0..100 {
        d = d.succ_opt().expect("date");
        if d.weekday() != Weekday::Fri {
            continue;
        }
        let t = Utc.from_utc_datetime(&d.and_hms_opt(8, 0, 0).expect("time"));
        let left = t - now;
        if left >= CDuration::days(7) && left <= CDuration::days(90) {
            out.push(t);
        }
    }
    out
}

/// Grid strikes within [lo, hi] x spot.
pub fn strikes(spot: Decimal, step: i64, lo: Decimal, hi: Decimal) -> Vec<Decimal> {
    let step = Decimal::from(step);
    let mut k = (spot * lo / step).ceil() * step;
    let mut out = vec![];
    while k <= spot * hi {
        out.push(k);
        k += step;
    }
    out
}

fn native_id(u: &str, expiry: DateTime<Utc>, k: Decimal, put: bool) -> String {
    format!(
        "HEGIC-{u}-{}-{}-{}",
        expiry.format("%Y%m%d"),
        k.normalize(),
        if put { "P" } else { "C" }
    )
}

fn instrument(m: &Market, expiry: DateTime<Utc>, k: Decimal, put: bool) -> Instrument {
    Instrument {
        key: InstrumentKey {
            platform: P,
            network: NETWORK.into(),
            native_id: native_id(m.underlying, expiry, k, put),
        },
        family: ProductFamily::DatedOption,
        underlying: m.underlying.into(),
        option_kind: Some(if put {
            OptionKind::Put
        } else {
            OptionKind::Call
        }),
        strike: Some(k),
        expiry: Some(expiry),
        exercise_style: Some(ExerciseStyle::American),
        contract_multiplier: Decimal::ONE,
        premium_currency: "USDC".into(),
        collateral_assets: vec!["USDC".into()],
        settlement_asset: Some("USDC".into()),
        settlement_mechanism: Some(SettlementMechanism::Cash),
        settlement_index: Some(format!("chainlink:{}/USD (Arbitrum)", m.underlying)),
        settlement_window: Some("on exercise (American) / expiry".into()),
        tick_size: None,
        min_amount: None,
        amount_step: None,
        is_active: true,
        spec_version: SPEC_VERSION.into(),
        native: json!({"calculator": if put { m.put } else { m.call }, "amount_unit": m.amount_unit.to_string()}),
    }
}

pub fn catalog(underlying: &str, spot: Decimal, now: DateTime<Utc>) -> Vec<Instrument> {
    let Some(m) = market(underlying) else {
        return vec![];
    };
    let mut out = vec![];
    for e in expiries(now) {
        for k in strikes(spot, m.step, Decimal::new(90, 2), Decimal::new(110, 2)) {
            out.push(instrument(m, e, k, false));
        }
        for k in strikes(spot, m.step, Decimal::new(85, 2), Decimal::new(115, 2)) {
            out.push(instrument(m, e, k, true));
        }
    }
    out
}

pub fn fee_schedule(instrument: &Instrument) -> FeeSchedule {
    let z = || FeeFormula::PctOfPremium {
        rate: Decimal::ZERO,
    };
    FeeSchedule {
        taker: z(),
        maker: z(),
        currency: instrument.premium_currency.clone(),
        source_url: "Hegic PriceCalculator.calculatePremium (Arbitrum, read 2026-10-08)".into(),
        verified_on: "2026-10-08".into(),
        notes: "Premium returned by calculatePremium is the price paid (no separate trading fee; secondary sources). Arbitrum gas counted in the gas line.".into(),
    }
}

#[async_trait]
impl Connector for HegicConnector {
    fn platform(&self) -> PlatformId {
        P
    }
    fn network(&self) -> &'static str {
        NETWORK
    }
    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        let Some(m) = market(underlying) else {
            return Ok(vec![]);
        };
        Ok(catalog(underlying, self.spot(m).await?, Utc::now()))
    }
    async fn get_market_data(&self, instrument: &Instrument) -> Result<crate::MarketData> {
        let m = market(&instrument.underlying).ok_or_else(|| invalid(P, "underlying"))?;
        let (Some(k), Some(expiry)) = (instrument.strike, instrument.expiry) else {
            return Err(invalid(P, "instrument without strike/expiry"));
        };
        let t0 = Instant::now();
        let now = Utc::now();
        let period = (expiry - now).num_seconds();
        if period <= 0 {
            return Err(invalid(P, "expired"));
        }
        let put = instrument.option_kind == Some(OptionKind::Put);
        let data = premium_calldata(period as u64, m.amount_unit, k)
            .ok_or_else(|| invalid(P, "strike"))?;
        let (spot, prem) = tokio::join!(
            self.spot(m),
            self.eth_call(if put { m.put } else { m.call }, data)
        );
        let premium = decode_uint(&prem?, 6).ok_or_else(|| invalid(P, "premium"))?;
        let network = t0.elapsed();
        let book = BookSnapshot {
            instrument: instrument.key.clone(),
            bids: vec![],
            asks: if premium > Decimal::ZERO {
                vec![Level {
                    price: premium,
                    size: m.nominal_size,
                }]
            } else {
                vec![]
            },
            source_ts: Some(now),
            received_at: Utc::now(),
            index_price: spot.ok(),
            mark_price: None,
            provenance: Provenance::LiveData,
            // buy() takes no strike: the strike is set to spot x moneyness when the user buys, so a
            // fixed listed strike is NOT executable as such -> indicative, never routed.
            access: BookAccess::IndicativeRfq,
        };
        Ok(crate::MarketData {
            book,
            network,
            normalize: std::time::Duration::ZERO,
            path: "eth_call",
        })
    }
    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        Ok(fee_schedule(instrument))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn decodes_premium_and_chainlink_words() {
        // 55.755902 USDC (live 2026-10-08 ETH 2600C 9 days).
        let w = format!("0x{:064x}", 55_755_902u64);
        assert_eq!(decode_uint(&w, 6), Some(dec!(55.755902)));
        assert_eq!(decode_uint(&format!("0x{:064x}", 0u64), 6), Some(dec!(0)));
        assert_eq!(decode_uint("0x12", 6), None);
    }

    #[test]
    fn calldata_matches_the_probed_encoding() {
        let c =
            premium_calldata(9 * 86_400 + 3_600, 1_000_000_000_000_000_000, dec!(2600)).unwrap();
        assert!(c.starts_with("0x156f1d74"));
        assert_eq!(c.len(), 10 + 3 * 64);
        assert!(c.ends_with(&format!("{:064x}", 260_000_000_000u64)));
    }

    #[test]
    fn catalog_uses_fridays_7_to_90_days_and_grid_strikes() {
        let now: DateTime<Utc> = "2026-10-08T09:00:00Z".parse().unwrap();
        let e = expiries(now);
        assert_eq!(e[0].to_rfc3339(), "2026-10-16T08:00:00+00:00");
        assert!(
            e.iter()
                .all(|t| t.weekday() == Weekday::Fri && (*t - now) <= CDuration::days(90))
        );
        let ks = strikes(dec!(2566), 50, dec!(0.9), dec!(1.1));
        assert_eq!(ks.first(), Some(&dec!(2350)));
        assert_eq!(ks.last(), Some(&dec!(2800)));
        let cat = catalog("ETH", dec!(2566), now);
        assert!(
            cat.iter()
                .any(|i| i.key.native_id == "HEGIC-ETH-20261016-2600-C")
        );
        assert!(
            cat.iter()
                .all(|i| i.exercise_style == Some(ExerciseStyle::American))
        );
        assert!(catalog("SOL", dec!(150), now).is_empty());
    }
}
