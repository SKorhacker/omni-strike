//! Replay connectors built from REAL responses recorded on 2026-10-05
//! (`fixtures/recorded-2026-10-05`). Every snapshot they return is tagged
//! `Provenance::Fixture`. Used by tests and the deterministic benchmark only; the server
//! binary never registers them. Failure injection lets tests exercise error paths.

use async_trait::async_trait;
use chrono::Utc;
use oa_adapters::{Connector, ConnectorError, MarketData, Result, aevo, derive};
use oa_core::domain::{Instrument, PlatformId, Provenance};
use oa_core::fees::FeeSchedule;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recorded-2026-10-05")
}

fn load(dir: &Path, name: &str) -> Value {
    let p = dir.join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}")))
        .unwrap_or_else(|e| panic!("{p:?}: {e}"))
}

#[derive(Debug, Clone)]
pub enum Inject {
    None,
    /// Every call fails with a timeout error.
    Timeout,
    /// Every call is rate limited.
    RateLimited(Duration),
    /// Every book call returns a malformed payload error.
    InvalidResponse,
    /// Sleep before answering (to exercise wait timeouts).
    Delay(Duration),
    /// Every book call: the platform reports stale data.
    UpstreamStale,
    /// Every call panics (a connector bug).
    Panic,
}

pub struct FixtureConnector {
    platform: PlatformId,
    network: &'static str,
    catalogs: HashMap<String, Vec<Instrument>>,
    /// native_id -> (book json, index/mark json)
    books: HashMap<String, (Value, Value)>,
    pub inject: Mutex<Inject>,
}

impl FixtureConnector {
    pub fn derive() -> Self {
        let d = fixtures_dir();
        let mut catalogs = HashMap::new();
        for u in ["BTC", "ETH"] {
            let f = load(
                &d,
                &format!("derive_v2_get_instruments_{}.json", u.to_lowercase()),
            );
            catalogs.insert(
                u.to_string(),
                derive::parse_instruments(&f["result"]).expect("fixture"),
            );
        }
        let ws = load(&d, "derive_v2_ws_orderbook.json");
        let ticker = load(&d, "derive_v2_get_ticker_ETH-20261030-2800-C.json")["result"].clone();
        let mut books = HashMap::new();
        books.insert(
            "ETH-20261030-2800-C".to_string(),
            (ws[1]["params"]["data"].clone(), ticker),
        );
        Self {
            platform: PlatformId::Derive,
            network: derive::NETWORK,
            catalogs,
            books,
            inject: Mutex::new(Inject::None),
        }
    }

    pub fn aevo() -> Self {
        let d = fixtures_dir();
        let mut catalogs = HashMap::new();
        for u in ["BTC", "ETH"] {
            let f = load(&d, &format!("aevo_markets_{}.json", u.to_lowercase()));
            catalogs.insert(u.to_string(), aevo::parse_markets(&f).expect("fixture"));
        }
        let mut books = HashMap::new();
        books.insert(
            "ETH-30OCT26-2800-C".to_string(),
            (
                load(&d, "aevo_orderbook_ETH-30OCT26-2800-C.json"),
                load(&d, "aevo_instrument_ETH-30OCT26-2800-C.json"),
            ),
        );
        Self {
            platform: PlatformId::Aevo,
            network: aevo::NETWORK,
            catalogs,
            books,
            inject: Mutex::new(Inject::None),
        }
    }

    pub fn set_inject(&self, i: Inject) {
        *self.inject.lock().unwrap() = i;
    }

    async fn apply_inject(&self, is_book: bool) -> Result<()> {
        let inj = self.inject.lock().unwrap().clone();
        let p = self.platform;
        match inj {
            Inject::None => Ok(()),
            Inject::Timeout => Err(ConnectorError::Timeout {
                platform: p,
                after: Duration::from_secs(10),
            }),
            Inject::RateLimited(d) => Err(ConnectorError::RateLimited {
                platform: p,
                retry_after: Some(d),
            }),
            Inject::InvalidResponse if is_book => Err(ConnectorError::InvalidResponse {
                platform: p,
                detail: "injected".into(),
            }),
            Inject::InvalidResponse => Ok(()),
            Inject::UpstreamStale if is_book => Err(ConnectorError::UpstreamStale {
                platform: p,
                detail: "injected".into(),
            }),
            Inject::UpstreamStale => Ok(()),
            Inject::Delay(d) => {
                tokio::time::sleep(d).await;
                Ok(())
            }
            Inject::Panic => panic!("injected connector panic"),
        }
    }
}

#[async_trait]
impl Connector for FixtureConnector {
    fn platform(&self) -> PlatformId {
        self.platform
    }
    fn network(&self) -> &'static str {
        self.network
    }

    async fn list_instruments(&self, underlying: &str) -> Result<Vec<Instrument>> {
        self.apply_inject(false).await?;
        Ok(self.catalogs.get(underlying).cloned().unwrap_or_default())
    }

    async fn get_market_data(&self, instrument: &Instrument) -> Result<MarketData> {
        self.apply_inject(true).await?;
        let (book, aux) = self.books.get(&instrument.key.native_id).ok_or_else(|| {
            ConnectorError::UnknownInstrument {
                platform: self.platform,
                native_id: format!("{} (no recorded book fixture)", instrument.key.native_id),
            }
        })?;
        let t = std::time::Instant::now();
        let book = match self.platform {
            PlatformId::Derive => {
                derive::parse_book(instrument, book, aux, Utc::now(), Provenance::Fixture)?
            }
            _ => aevo::parse_book(instrument, book, aux, Utc::now(), Provenance::Fixture)?,
        };
        Ok(MarketData {
            book,
            network: Duration::ZERO,
            normalize: t.elapsed(),
            path: "fixture",
        })
    }

    fn get_fees(&self, instrument: &Instrument) -> Result<FeeSchedule> {
        match self.platform {
            PlatformId::Derive => derive::fee_schedule(instrument),
            _ => Ok(aevo::fee_schedule(instrument)),
        }
    }
}

impl FixtureConnector {
    /// Re-tag every instrument with another network label (e.g. emulate "aevo-testnet").
    pub fn with_network(mut self, network: &'static str) -> Self {
        self.network = network;
        for list in self.catalogs.values_mut() {
            for i in list.iter_mut() {
                i.key.network = network.into();
            }
        }
        self
    }
}

// ------------------------------------------------------------------ execution test double

use oa_adapters::exec::{ExecOrder, ExecutionVenue, VenueBalance, VenueOrderState, VenueStatus};
use oa_core::Decimal;

/// How the fake venue answers the NEXT submissions. TEST DOUBLE ONLY: never registered by the
/// server binary, never presented as a real venue.
#[derive(Debug, Clone)]
pub enum FakeFill {
    /// Fill everything at the order's limit price minus `improvement` per contract.
    Full {
        improvement: Decimal,
    },
    /// Fill this fraction, cancel the rest (IOC).
    Fraction(Decimal),
    Reject,
    /// The venue CREATES and fills the order, but the response is lost (transport error).
    AckLost,
    /// Transport error and the order does NOT exist at the venue.
    NeverCreated,
}

pub struct FakeVenue {
    platform: PlatformId,
    network: &'static str,
    pub behavior: Mutex<FakeFill>,
    pub book: Mutex<HashMap<String, VenueOrderState>>,
    pub submissions: Mutex<u32>,
    pub available: Mutex<Decimal>,
    /// Reject the next N SELL submissions (to simulate one failing strategy leg).
    pub reject_next_sells: Mutex<u32>,
}

impl FakeVenue {
    pub fn new(platform: PlatformId, network: &'static str, behavior: FakeFill) -> Self {
        Self {
            platform,
            network,
            behavior: Mutex::new(behavior),
            book: Mutex::new(HashMap::new()),
            submissions: Mutex::new(0),
            available: Mutex::new(Decimal::from(10_000)),
            reject_next_sells: Mutex::new(0),
        }
    }
    fn id_for(o: &ExecOrder) -> String {
        format!("fake-{}-{}", o.native_id, o.salt)
    }
}

#[async_trait]
impl ExecutionVenue for FakeVenue {
    fn platform(&self) -> PlatformId {
        self.platform
    }
    fn network(&self) -> &'static str {
        self.network
    }
    fn precompute_order_id(&self, o: &ExecOrder) -> Option<String> {
        Some(Self::id_for(o))
    }
    async fn submit(&self, o: &ExecOrder) -> Result<VenueOrderState> {
        *self.submissions.lock().unwrap() += 1;
        let id = Self::id_for(o);
        let mk = |filled: Decimal, status: VenueStatus, price: Option<Decimal>| VenueOrderState {
            venue_order_id: id.clone(),
            is_final: !matches!(status, VenueStatus::Open),
            status,
            filled_native: filled,
            avg_price_native: price,
            raw: serde_json::json!({"fake": true}),
        };
        if o.side == oa_core::domain::Side::Sell {
            let mut n = self.reject_next_sells.lock().unwrap();
            if *n > 0 {
                *n -= 1;
                return Err(ConnectorError::Http {
                    platform: self.platform,
                    status: 400,
                    body: "{\"error\":\"SELL_REJECTED_BY_TEST\"}".into(),
                });
            }
        }
        let b = self.behavior.lock().unwrap().clone();
        match b {
            FakeFill::Full { improvement } => {
                let s = mk(
                    o.qty_native,
                    VenueStatus::Filled,
                    Some(o.limit_price_native - improvement),
                );
                self.book.lock().unwrap().insert(id.clone(), s.clone());
                Ok(s)
            }
            FakeFill::Fraction(f) => {
                let s = mk(
                    o.qty_native * f,
                    VenueStatus::PartiallyFilled,
                    Some(o.limit_price_native),
                );
                self.book.lock().unwrap().insert(id.clone(), s.clone());
                Ok(s)
            }
            FakeFill::Reject => Err(ConnectorError::Http {
                platform: self.platform,
                status: 400,
                body: "{\"error\":\"INSUFFICIENT_BALANCE\"}".into(),
            }),
            FakeFill::AckLost => {
                self.book.lock().unwrap().insert(
                    id.clone(),
                    mk(
                        o.qty_native,
                        VenueStatus::Filled,
                        Some(o.limit_price_native),
                    ),
                );
                Err(ConnectorError::Timeout {
                    platform: self.platform,
                    after: Duration::from_secs(10),
                })
            }
            FakeFill::NeverCreated => Err(ConnectorError::Timeout {
                platform: self.platform,
                after: Duration::from_secs(10),
            }),
        }
    }
    async fn get_order(&self, id: &str) -> Result<Option<VenueOrderState>> {
        Ok(self.book.lock().unwrap().get(id).cloned())
    }
    async fn cancel(&self, id: &str) -> Result<VenueOrderState> {
        let mut b = self.book.lock().unwrap();
        let s = b.get_mut(id).ok_or(ConnectorError::UnknownInstrument {
            platform: self.platform,
            native_id: id.into(),
        })?;
        if !s.is_final {
            s.status = VenueStatus::Cancelled;
            s.is_final = true;
        }
        Ok(s.clone())
    }
    fn identity(&self) -> serde_json::Value {
        serde_json::json!({"venue": "FAKE (test double)", "network": self.network})
    }
    async fn balance(&self) -> Result<VenueBalance> {
        let a = *self.available.lock().unwrap();
        Ok(VenueBalance {
            available: a,
            equity: Some(a),
            currency: "USDC".into(),
            raw: serde_json::json!({"fake": true}),
        })
    }
}
