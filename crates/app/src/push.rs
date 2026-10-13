//! Push quotes to integrators (Server-Sent Events): `GET /v1/stream/quotes?...`.
//!
//! One TOPIC per distinct (contract, side, quantity): a single background task recomputes the
//! comparison every `interval_ms` and broadcasts a compact event to every subscriber. Cost therefore
//! scales with the number of distinct contracts watched, not with the number of connected users.
//! The task stops when the last subscriber leaves.

use crate::engine::{CompareRequest, Engine};
use chrono::{DateTime, Utc};
use oa_core::Decimal;
use oa_core::domain::{OptionKind, PlatformId, ProductFamily, Side};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::broadcast;

pub const MIN_INTERVAL_MS: u64 = 250;
pub const MAX_TOPICS: usize = 5_000;
/// Consecutive failed recomputations after which a topic gives up and closes its streams.
pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

/// What a topic broadcasts: a quote, or an error (`last` = the stream closes after it).
#[derive(Debug, Clone)]
pub enum PushMsg {
    Quote(String),
    Error { message: String, last: bool },
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StreamQuery {
    pub underlying: String,
    pub option_kind: OptionKind,
    #[schemars(with = "String")]
    pub strike: Decimal,
    pub expiry: DateTime<Utc>,
    pub side: Side,
    #[schemars(with = "String")]
    pub quantity: Decimal,
    #[serde(default)]
    pub family: Option<ProductFamily>,
    /// Push period (>= 250 ms, default 1000).
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VenueTotal {
    pub platform: PlatformId,
    #[schemars(with = "String")]
    pub total: Decimal,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct QuoteEvent {
    pub t: DateTime<Utc>,
    /// Best eligible, fully covered venue (lowest cost when buying, highest proceeds when selling).
    pub best: Option<VenueTotal>,
    /// Every eligible, fully covered venue, best first.
    pub venues: Vec<VenueTotal>,
    #[schemars(with = "Option<String>")]
    pub derive_total: Option<Decimal>,
    pub saving_vs_derive_pct: Option<f64>,
    pub unavailable: usize,
}

struct Topic {
    tx: broadcast::Sender<Arc<PushMsg>>,
}

#[derive(Default)]
pub struct PushHub {
    topics: Mutex<HashMap<String, Weak<Topic>>>,
}

pub struct Subscription {
    pub rx: broadcast::Receiver<Arc<PushMsg>>,
    _topic: Arc<Topic>,
}

fn key(q: &StreamQuery, interval: u64) -> String {
    format!(
        "{}|{:?}|{}|{}|{:?}|{}|{:?}|{interval}",
        q.underlying.to_uppercase(),
        q.option_kind,
        q.strike.normalize(),
        q.expiry.timestamp(),
        q.side,
        q.quantity.normalize(),
        q.family
    )
}

async fn event(engine: &Engine, q: &StreamQuery) -> Result<QuoteEvent, crate::engine::EngineError> {
    let out = engine
        .compare(CompareRequest {
            underlying: q.underlying.clone(),
            option_kind: q.option_kind,
            strike: q.strike,
            expiry: q.expiry,
            side: q.side,
            quantity: q.quantity,
            accepted_differences: crate::live::accepted(),
            reference_platform: None,
            baseline_platform: Some(PlatformId::Derive),
            max_age_ms: None,
            family: q.family,
            latency_budget_ms: None,
            simulated_balances: Default::default(),
            user_country: None,
            venues: vec![],
        })
        .await?;
    let r = out.response;
    let mut venues: Vec<VenueTotal> = r
        .ranked
        .iter()
        .filter(|v| v.estimate.covered_underlying >= q.quantity)
        .filter_map(|v| {
            v.estimate.total_estimated.map(|t| VenueTotal {
                platform: v.estimate.instrument.platform,
                total: t,
            })
        })
        .collect();
    match q.side {
        Side::Buy => venues.sort_by_key(|v| v.total),
        Side::Sell => venues.sort_by_key(|v| std::cmp::Reverse(v.total)),
    }
    let derive_total = venues
        .iter()
        .find(|v| v.platform == PlatformId::Derive)
        .map(|v| v.total);
    let best = venues.first().cloned();
    let saving = match (&best, derive_total) {
        (Some(b), Some(d)) if d > Decimal::ZERO => {
            use rust_decimal::prelude::ToPrimitive;
            let diff = match q.side {
                Side::Buy => d - b.total,
                Side::Sell => b.total - d,
            };
            (diff / d * Decimal::from(100)).to_f64()
        }
        _ => None,
    };
    Ok(QuoteEvent {
        t: Utc::now(),
        best,
        venues,
        derive_total,
        saving_vs_derive_pct: saving,
        unavailable: r.unavailable.len(),
    })
}

impl PushHub {
    pub fn topics(&self) -> usize {
        self.topics
            .lock()
            .expect("hub lock")
            .values()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// Join (or create) the topic for this query.
    pub fn subscribe(&self, engine: &Arc<Engine>, q: StreamQuery) -> Result<Subscription, String> {
        if q.quantity <= Decimal::ZERO {
            return Err("quantity must be > 0".into());
        }
        let interval = q.interval_ms.unwrap_or(1000).max(MIN_INTERVAL_MS);
        let k = key(&q, interval);
        let mut g = self.topics.lock().expect("hub lock");
        g.retain(|_, w| w.strong_count() > 0);
        if let Some(t) = g.get(&k).and_then(|w| w.upgrade()) {
            return Ok(Subscription {
                rx: t.tx.subscribe(),
                _topic: t,
            });
        }
        if g.len() >= MAX_TOPICS {
            return Err(format!(
                "too many distinct streams ({MAX_TOPICS}); retry later"
            ));
        }
        let (tx, rx) = broadcast::channel(16);
        let topic = Arc::new(Topic { tx: tx.clone() });
        g.insert(k, Arc::downgrade(&topic));
        let (e, weak_topic) = (Arc::downgrade(engine), Arc::downgrade(&topic));
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(interval));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut failures = 0u32;
            loop {
                iv.tick().await;
                // Stop when every subscriber left (the Arc<Topic> is held by subscriptions only).
                if weak_topic.strong_count() == 0 {
                    break;
                }
                let Some(engine) = e.upgrade() else { break };
                match event(&engine, &q).await {
                    Ok(ev) => {
                        failures = 0;
                        if let Ok(s) = serde_json::to_string(&ev) {
                            let _ = tx.send(Arc::new(PushMsg::Quote(s)));
                        }
                    }
                    Err(err) => {
                        // A bad request never heals: close at once. Anything else: retry a few times.
                        failures += 1;
                        let last = matches!(err, crate::engine::EngineError::Invalid(_))
                            || failures >= MAX_CONSECUTIVE_FAILURES;
                        let _ = tx.send(Arc::new(PushMsg::Error {
                            message: err.to_string(),
                            last,
                        }));
                        if last {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Subscription { rx, _topic: topic })
    }
}
