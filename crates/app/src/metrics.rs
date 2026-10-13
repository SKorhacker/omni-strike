//! Minimal in-process metrics: bounded sample windows (monotonic-clock durations) and counters.
//! Percentiles are only reported with their observation count; `sufficient` is false below
//! `MIN_SAMPLES_FOR_PERCENTILES`.

use schemars::JsonSchema;
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

const WINDOW: usize = 10_000;
pub const MIN_SAMPLES_FOR_PERCENTILES: usize = 100;

#[derive(Default)]
pub struct Metrics {
    samples: Mutex<BTreeMap<String, VecDeque<f64>>>,
    counters: Mutex<BTreeMap<String, u64>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Summary {
    pub unit: &'static str,
    pub count: usize,
    /// True when count >= MIN_SAMPLES_FOR_PERCENTILES.
    pub sufficient: bool,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Snapshot {
    /// Durations in milliseconds unless stated otherwise in `unit`.
    pub timings: BTreeMap<String, Summary>,
    pub counters: BTreeMap<String, u64>,
    pub note: &'static str,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    // Nearest-rank method.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.min(sorted.len()) - 1]
}

/// Lock that survives a panicking holder: metrics must never turn one panic into many.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Metrics {
    pub fn observe(&self, name: impl AsRef<str> + Into<String>, d: Duration) {
        self.observe_value(name, d.as_secs_f64() * 1000.0);
    }

    /// Raw value (e.g. data age in ms). No allocation when the series already exists.
    pub fn observe_value(&self, name: impl AsRef<str> + Into<String>, v: f64) {
        let mut s = lock(&self.samples);
        let q = match s.get_mut(name.as_ref()) {
            Some(q) => q,
            None => s.entry(name.into()).or_default(),
        };
        if q.len() == WINDOW {
            q.pop_front();
        }
        q.push_back(v);
    }

    pub fn incr(&self, name: impl AsRef<str> + Into<String>) {
        self.incr_by(name, 1);
    }

    pub fn incr_by(&self, name: impl AsRef<str> + Into<String>, n: u64) {
        let mut c = lock(&self.counters);
        match c.get_mut(name.as_ref()) {
            Some(v) => *v += n,
            None => {
                c.insert(name.into(), n);
            }
        }
    }

    /// Copies the windows under the lock, sorts OUTSIDE it: polling /v1/metrics never stalls the
    /// requests that record samples.
    pub fn snapshot(&self) -> Snapshot {
        let copied: Vec<(String, Vec<f64>)> = lock(&self.samples)
            .iter()
            .map(|(k, q)| (k.clone(), q.iter().copied().collect()))
            .collect();
        let timings = copied
            .into_iter()
            .map(|(k, mut v)| {
                v.sort_by(|a, b| a.total_cmp(b));
                let has = !v.is_empty();
                (
                    k,
                    Summary {
                        unit: "ms",
                        count: v.len(),
                        sufficient: v.len() >= MIN_SAMPLES_FOR_PERCENTILES,
                        p50: has.then(|| pct(&v, 50.0)),
                        p95: has.then(|| pct(&v, 95.0)),
                        p99: has.then(|| pct(&v, 99.0)),
                        max: v.last().copied(),
                    },
                )
            })
            .collect();
        Snapshot {
            timings,
            counters: lock(&self.counters).clone(),
            note: "Process-local, since start, last 10k samples per series. Percentiles with \
                   sufficient=false are not statistically meaningful.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let m = Metrics::default();
        for i in 1..=100 {
            m.observe_value("x", i as f64);
        }
        let s = &m.snapshot().timings["x"];
        assert_eq!(
            (s.p50, s.p95, s.p99, s.max),
            (Some(50.0), Some(95.0), Some(99.0), Some(100.0))
        );
        assert!(s.sufficient);
    }

    #[test]
    fn window_is_bounded() {
        let m = Metrics::default();
        for i in 0..(WINDOW + 50) {
            m.observe_value("x", i as f64);
        }
        assert_eq!(m.snapshot().timings["x"].count, WINDOW);
    }
}
