//! Live savings tracker: continuously compares "our router" with "just use Derive" on a basket of
//! liquid contracts and keeps a snapshot for the live dashboard (`GET /v1/live/savings`).
//!
//! Read-only: it only calls `Engine::compare` (same code path as the API), one contract at a time, so
//! it never takes more than one comparison permit. Figures are ESTIMATES from visible books + fees.
//! "Hypothetical flow" = as if every observation had been a real order of the basket size.

use crate::engine::{CompareRequest, Engine, SearchQuery};
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::comparability::ReasonCode;
use oa_core::domain::{OptionKind, PlatformId, ProductFamily, Side};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const PER_UNDERLYING: usize = 10;
const TICK: Duration = Duration::from_millis(1500);
const BASKET_REFRESH: Duration = Duration::from_secs(600);
const HISTORY: usize = 60;
const FEED: usize = 60;
const SERIES: usize = 400;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VenueQuote {
    pub platform: PlatformId,
    /// Total cost incl. fees (USDC-equivalent), fully covered and eligible.
    #[schemars(with = "String")]
    pub total: Decimal,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Point {
    pub t: DateTime<Utc>,
    pub saving_pct: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ContractRow {
    pub id: String,
    pub underlying: String,
    pub option_kind: OptionKind,
    #[schemars(with = "String")]
    pub strike: Decimal,
    pub expiry: DateTime<Utc>,
    #[schemars(with = "String")]
    pub quantity: Decimal,
    pub best_venue: Option<PlatformId>,
    #[schemars(with = "Option<String>")]
    pub best_total: Option<Decimal>,
    #[schemars(with = "Option<String>")]
    pub derive_total: Option<Decimal>,
    pub saving_usd: Option<f64>,
    pub saving_pct: Option<f64>,
    pub venues: Vec<VenueQuote>,
    pub updated_at: Option<DateTime<Utc>>,
    pub history: VecDeque<Point>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FeedEvent {
    pub t: DateTime<Utc>,
    pub contract: String,
    pub best_venue: PlatformId,
    pub saving_usd: f64,
    pub saving_pct: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SeriesPoint {
    pub t: DateTime<Utc>,
    /// Median saving over the basket's latest observations (%).
    pub median_saving_pct: f64,
    /// Share of the basket where we beat Derive (0..1).
    pub win_rate: f64,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct Summary {
    pub since: Option<DateTime<Utc>>,
    pub observations: u64,
    pub observations_beating_derive: u64,
    /// Sum of savings if every observation had been a real order (estimate).
    pub hypothetical_saving_usd: f64,
    /// Sum of Derive-only costs over the same observations.
    pub hypothetical_derive_cost_usd: f64,
    /// Latest basket view.
    pub win_rate: f64,
    pub median_saving_pct: f64,
    pub mean_saving_pct: f64,
    pub wins_by_venue: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct LiveSnapshot {
    pub updated_at: Option<DateTime<Utc>>,
    pub wording: String,
    pub basket_size: usize,
    pub summary: Summary,
    pub contracts: Vec<ContractRow>,
    pub feed: VecDeque<FeedEvent>,
    pub series: VecDeque<SeriesPoint>,
}

pub struct LiveTracker {
    state: RwLock<LiveSnapshot>,
}

impl LiveTracker {
    pub fn snapshot(&self) -> LiveSnapshot {
        self.state.read().expect("live lock").clone()
    }

    /// Start the background loop (holds only a weak reference to the engine).
    pub fn spawn(engine: &Arc<Engine>) -> Arc<Self> {
        let me = Arc::new(Self {
            state: RwLock::new(LiveSnapshot {
                wording: "Estimated total cost (visible books + documented fees) of the best eligible venue vs Derive alone, for the same contract and size. Not a fill guarantee.".into(),
                ..Default::default()
            }),
        });
        let (t, e) = (me.clone(), Arc::downgrade(engine));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await; // let catalogs load
            let mut i = 0usize;
            let mut built_at: Option<std::time::Instant> = None;
            loop {
                let Some(engine) = e.upgrade() else { break };
                if built_at.is_none_or(|b| b.elapsed() > BASKET_REFRESH) {
                    let rows = build_basket(&engine).await;
                    if !rows.is_empty() {
                        let mut s = t.state.write().expect("live lock");
                        // keep history of contracts still in the basket
                        let old: BTreeMap<String, ContractRow> =
                            s.contracts.drain(..).map(|r| (r.id.clone(), r)).collect();
                        s.contracts = rows
                            .into_iter()
                            .map(|r| old.get(&r.id).cloned().unwrap_or(r))
                            .collect();
                        s.basket_size = s.contracts.len();
                        s.summary.since.get_or_insert(Utc::now());
                        built_at = Some(std::time::Instant::now());
                    }
                }
                let n = t.state.read().expect("live lock").contracts.len();
                if n > 0 {
                    let row = t.state.read().expect("live lock").contracts[i % n].clone();
                    i += 1;
                    if let Some(obs) = observe(&engine, &row).await {
                        t.apply(i % n.max(1), &row.id, obs);
                    }
                }
                drop(engine);
                tokio::time::sleep(TICK).await;
            }
        });
        me
    }

    fn apply(&self, _idx: usize, id: &str, o: Observation) {
        let mut s = self.state.write().expect("live lock");
        let now = Utc::now();
        let Some(row) = s.contracts.iter_mut().find(|r| r.id == id) else {
            return;
        };
        row.best_venue = o.best.as_ref().map(|b| b.0);
        row.best_total = o.best.as_ref().map(|b| b.1);
        row.derive_total = o.derive;
        row.venues = o.venues;
        row.updated_at = Some(now);
        let (sav_usd, sav_pct) = match (o.best.as_ref(), o.derive) {
            (Some((_, b)), Some(d)) if d > Decimal::ZERO => {
                let usd = f(d - *b);
                (Some(usd), Some(usd / f(d) * 100.0))
            }
            _ => (None, None),
        };
        row.saving_usd = sav_usd;
        row.saving_pct = sav_pct;
        if let Some(p) = sav_pct {
            row.history.push_back(Point {
                t: now,
                saving_pct: p,
            });
            while row.history.len() > HISTORY {
                row.history.pop_front();
            }
        }
        let label = format!(
            "{} {} {} {}",
            row.underlying,
            row.strike.normalize(),
            if row.option_kind == OptionKind::Call {
                "C"
            } else {
                "P"
            },
            row.expiry.format("%d%b").to_string().to_uppercase()
        );
        let best_venue = row.best_venue;
        let derive_total = row.derive_total;
        if let (Some(usd), Some(pct), Some(v), Some(d)) =
            (sav_usd, sav_pct, best_venue, derive_total)
        {
            s.summary.observations += 1;
            s.summary.hypothetical_derive_cost_usd += f(d);
            if usd > 0.005 {
                s.summary.observations_beating_derive += 1;
                s.summary.hypothetical_saving_usd += usd;
                *s.summary.wins_by_venue.entry(v.to_string()).or_default() += 1;
                s.feed.push_front(FeedEvent {
                    t: now,
                    contract: label,
                    best_venue: v,
                    saving_usd: usd,
                    saving_pct: pct,
                });
                while s.feed.len() > FEED {
                    s.feed.pop_back();
                }
            }
        }
        // basket view
        let cur: Vec<f64> = s.contracts.iter().filter_map(|r| r.saving_pct).collect();
        if !cur.is_empty() {
            let mut v = cur.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            s.summary.median_saving_pct = v[v.len() / 2];
            s.summary.mean_saving_pct = v.iter().sum::<f64>() / v.len() as f64;
            s.summary.win_rate = v.iter().filter(|x| **x > 0.0001).count() as f64 / v.len() as f64;
            let p = SeriesPoint {
                t: now,
                median_saving_pct: s.summary.median_saving_pct,
                win_rate: s.summary.win_rate,
            };
            s.series.push_back(p);
            while s.series.len() > SERIES {
                s.series.pop_front();
            }
        }
        s.updated_at = Some(now);
    }
}

fn f(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(0.0)
}

struct Observation {
    best: Option<(PlatformId, Decimal)>,
    derive: Option<Decimal>,
    venues: Vec<VenueQuote>,
}

pub(crate) fn accepted() -> Vec<ReasonCode> {
    vec![
        ReasonCode::SettlementIndexDiffers,
        ReasonCode::SettlementWindowUnknown,
        ReasonCode::SettlementWindowDiffers,
        ReasonCode::SettlementAssetDiffers,
        ReasonCode::PremiumCurrencyDiffers,
    ]
}

fn request(row: &ContractRow) -> CompareRequest {
    CompareRequest {
        underlying: row.underlying.clone(),
        option_kind: row.option_kind,
        strike: row.strike,
        expiry: row.expiry,
        side: Side::Buy,
        quantity: row.quantity,
        accepted_differences: accepted(),
        reference_platform: Some(PlatformId::Derive),
        baseline_platform: Some(PlatformId::Derive),
        max_age_ms: None,
        family: None,
        latency_budget_ms: Some(1500),
        simulated_balances: Default::default(),
        user_country: None,
        venues: vec![],
    }
}

async fn observe(engine: &Engine, row: &ContractRow) -> Option<Observation> {
    let out = engine.compare(request(row)).await.ok()?;
    let r = &out.response;
    let covered = |e: &oa_core::cost::QuoteEstimate| {
        e.covered_underlying >= row.quantity && e.total_estimated.is_some()
    };
    let venues: Vec<VenueQuote> = r
        .ranked
        .iter()
        .filter(|q| covered(&q.estimate))
        .map(|q| VenueQuote {
            platform: q.estimate.instrument.platform,
            total: q.estimate.total_estimated.unwrap_or_default(),
        })
        .collect();
    let derive = venues
        .iter()
        .find(|v| v.platform == PlatformId::Derive)
        .map(|v| v.total);
    let best = venues
        .iter()
        .min_by_key(|v| v.total)
        .map(|v| (v.platform, v.total));
    Some(Observation {
        best,
        derive,
        venues,
    })
}

/// Liquid basket: contracts listed on Derive and >= 2 other venues, 1-21 days, strike within +-7 % of
/// the index (taken from a Derive estimate), spread over expiries; 1 BTC / 10 ETH buy orders.
async fn build_basket(engine: &Engine) -> Vec<ContractRow> {
    let now = Utc::now();
    let mut rows = Vec::new();
    for u in ["BTC", "ETH"] {
        let mut cands = Vec::new();
        for kind in [OptionKind::Call, OptionKind::Put] {
            let q = SearchQuery {
                underlying: u.into(),
                option_kind: Some(kind),
                expiry: None,
                family: Some(ProductFamily::DatedOption),
                offset: 0,
                limit: Some(3000),
            };
            if let Ok(r) = engine.search(q).await {
                for g in r.groups {
                    let plats: std::collections::BTreeSet<_> =
                        g.listings.iter().map(|l| l.key.platform).collect();
                    let days = (g.expiry - now).num_hours() as f64 / 24.0;
                    if plats.contains(&PlatformId::Derive)
                        && plats.len() >= 3
                        && (1.0..=21.0).contains(&days)
                    {
                        cands.push(g);
                    }
                }
            }
        }
        if cands.is_empty() {
            continue;
        }
        // index from one Derive estimate
        let probe = &cands[cands.len() / 2];
        let qty = if u == "BTC" {
            Decimal::ONE
        } else {
            Decimal::from(10)
        };
        let mut row0 = row(u, probe.option_kind, probe.strike, probe.expiry, qty);
        row0.quantity = qty;
        let index = match engine.compare(request(&row0)).await {
            Ok(o) => o
                .response
                .ranked
                .iter()
                .chain(o.response.incomplete.iter())
                .find_map(|q| q.estimate.index_price),
            Err(_) => None,
        };
        let Some(index) = index else { continue };
        let lo = index * Decimal::new(93, 2);
        let hi = index * Decimal::new(107, 2);
        let mut near: Vec<_> = cands
            .into_iter()
            .filter(|g| g.strike >= lo && g.strike <= hi)
            .collect();
        // spread over expiries: round-robin by expiry, closest strike first
        near.sort_by_key(|g| (g.expiry, (g.strike - index).abs()));
        let mut by_exp: BTreeMap<DateTime<Utc>, VecDeque<_>> = BTreeMap::new();
        for g in near {
            by_exp.entry(g.expiry).or_default().push_back(g);
        }
        let mut picked = 0;
        while picked < PER_UNDERLYING && by_exp.values().any(|v| !v.is_empty()) {
            for v in by_exp.values_mut() {
                if picked >= PER_UNDERLYING {
                    break;
                }
                if let Some(g) = v.pop_front() {
                    rows.push(row(u, g.option_kind, g.strike, g.expiry, qty));
                    picked += 1;
                }
            }
        }
    }
    rows
}

fn row(
    u: &str,
    kind: OptionKind,
    strike: Decimal,
    expiry: DateTime<Utc>,
    qty: Decimal,
) -> ContractRow {
    ContractRow {
        id: format!(
            "{u}-{kind:?}-{}-{}",
            strike.normalize(),
            expiry.format("%Y%m%d%H")
        ),
        underlying: u.into(),
        option_kind: kind,
        strike,
        expiry,
        quantity: qty,
        best_venue: None,
        best_total: None,
        derive_total: None,
        saving_usd: None,
        saving_pct: None,
        venues: vec![],
        updated_at: None,
        history: VecDeque::new(),
    }
}
