//! Load generator: N concurrent clients sending real comparisons to a running server for D seconds.
//!
//!   oa-load [--api http://127.0.0.1:8080] [--concurrency 200] [--seconds 30] [--key os_...]
//!
//! Contracts: listed on >= 3 venues, 1-30 days (fetched once from /v1/instruments). Reports
//! requests/s, latency percentiles and status codes. Read-only (POST /v1/quotes/compare only).
//! NOTE: client and server on the same machine compete for CPU — results are a lower bound.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn arg(name: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter()
        .position(|x| x == name)
        .and_then(|i| a.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api = arg("--api", "http://127.0.0.1:8080");
    let conc: usize = arg("--concurrency", "200").parse()?;
    let secs: u64 = arg("--seconds", "30").parse()?;
    let key = arg("--key", "");
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(conc)
        .timeout(Duration::from_secs(30))
        .build()?;

    let now = chrono::Utc::now();
    let mut contracts: Vec<Value> = Vec::new();
    for u in ["BTC", "ETH"] {
        for k in ["call", "put"] {
            let mut rq = client.get(format!(
                "{api}/v1/instruments?underlying={u}&option_kind={k}&limit=3000"
            ));
            if !key.is_empty() {
                rq = rq.bearer_auth(&key);
            }
            let r: Value = rq.send().await?.json().await?;
            for g in r["groups"].as_array().into_iter().flatten() {
                let n = g["listings"]
                    .as_array()
                    .map(|l| {
                        l.iter()
                            .filter_map(|x| x["key"]["platform"].as_str().map(str::to_string))
                            .collect::<std::collections::BTreeSet<_>>()
                            .len()
                    })
                    .unwrap_or(0);
                let exp = g["expiry"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                if n >= 3 && exp.is_some_and(|e| e > now && e < now + chrono::Duration::days(30)) {
                    contracts.push(json!({"underlying": u, "option_kind": k, "strike": g["strike"], "expiry": g["expiry"],
                        "side": "buy", "quantity": if u == "BTC" { "1" } else { "10" },
                        "accepted_differences": ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "SETTLEMENT_WINDOW_DIFFERS", "SETTLEMENT_ASSET_DIFFERS", "PREMIUM_CURRENCY_DIFFERS"]}));
                }
            }
        }
    }
    if contracts.is_empty() {
        return Err("no contracts found".into());
    }
    // A realistic working set: many users look at the popular contracts.
    contracts.truncate(200);
    println!(
        "{} contracts in the working set, {conc} concurrent clients, {secs} s against {api}",
        contracts.len()
    );
    let contracts = Arc::new(contracts);
    let stop = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for w in 0..conc {
        let (client, contracts, stop, api, key) = (
            client.clone(),
            contracts.clone(),
            stop.clone(),
            api.clone(),
            key.clone(),
        );
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            let mut codes: BTreeMap<u16, u64> = BTreeMap::new();
            let mut i = w * 7919;
            while !stop.load(Ordering::Relaxed) {
                let body = &contracts[i % contracts.len()];
                i += 1;
                let t = Instant::now();
                let mut rq = client.post(format!("{api}/v1/quotes/compare")).json(body);
                if !key.is_empty() {
                    rq = rq.bearer_auth(&key);
                }
                let code = match rq.send().await {
                    Ok(r) => {
                        let c = r.status().as_u16();
                        let _ = r.bytes().await;
                        c
                    }
                    Err(_) => 0,
                };
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
                *codes.entry(code).or_default() += 1;
            }
            (lat, codes)
        }));
    }
    tokio::time::sleep(Duration::from_secs(secs)).await;
    stop.store(true, Ordering::Relaxed);
    let mut lat = Vec::new();
    let mut codes: BTreeMap<u16, u64> = BTreeMap::new();
    for t in tasks {
        let (l, c) = t.await?;
        lat.extend(l);
        for (k, v) in c {
            *codes.entry(k).or_default() += v;
        }
    }
    let el = t0.elapsed().as_secs_f64();
    let n = lat.len();
    let ok = codes.get(&200).copied().unwrap_or(0);
    println!(
        "requests {n} in {el:.1} s = {:.0} req/s ({:.0} OK/s) | latency ms p50 {:.1} p95 {:.1} p99 {:.1} max {:.1} | status {codes:?}",
        n as f64 / el,
        ok as f64 / el,
        pct(&mut lat.clone(), 0.50),
        pct(&mut lat.clone(), 0.95),
        pct(&mut lat.clone(), 0.99),
        lat.iter().cloned().fold(0.0, f64::max)
    );
    Ok(())
}
