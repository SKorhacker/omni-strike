//! Deterministic replay benchmark of INTERNAL costs (no network): recorded books served by
//! replay connectors through the real actors, engine and HTTP router.
//! Usage: cargo run --release -p oa-app --bin oa-bench -- [iterations]

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use axum::body::Body;
use axum::http::Request;
use oa_adapters::Connector;
use oa_app::api::{AppState, router};
use oa_app::engine::{CompareRequest, Engine};
use oa_app::fixtures::FixtureConnector;
use oa_app::metrics::Metrics;
use oa_app::store::MemStore;
use oa_core::comparability::ReasonCode;
use oa_core::domain::{OptionKind, PlatformId, Side, TradingMode};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

#[tokio::main]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    let metrics = Arc::new(Metrics::default());
    let engine = Arc::new(Engine::new(
        vec![
            Arc::new(FixtureConnector::derive()) as Arc<dyn Connector>,
            Arc::new(FixtureConnector::aevo()) as Arc<dyn Connector>,
        ],
        Arc::new(MemStore::default()),
        metrics.clone(),
        TradingMode::Simulation,
        Duration::from_secs(3600),
        Duration::from_secs(60),
        4,
    ));
    let req = CompareRequest {
        underlying: "ETH".into(),
        option_kind: OptionKind::Call,
        strike: rust_decimal::Decimal::from(2800),
        expiry: chrono::DateTime::from_str("2026-10-30T08:00:00Z").unwrap(),
        side: Side::Buy,
        quantity: rust_decimal::Decimal::from_str("5").unwrap(),
        accepted_differences: vec![
            ReasonCode::SettlementIndexDiffers,
            ReasonCode::SettlementWindowUnknown,
        ],
        reference_platform: None,
        baseline_platform: Some(PlatformId::Derive),
        max_age_ms: Some(60_000),
        latency_budget_ms: None,
        family: None,
        simulated_balances: Default::default(),
        user_country: None,
        venues: vec![],
    };
    // Warm caches (catalog parse of ~1.7k instruments per platform happens here).
    engine.compare(req.clone()).await.expect("warm-up compare");

    let t = Instant::now();
    for _ in 0..n {
        engine.compare(req.clone()).await.expect("compare");
    }
    let engine_wall = t.elapsed();

    let app = router(AppState::new(engine.clone(), None));
    let body = serde_json::to_string(&req).unwrap();
    let t = Instant::now();
    for _ in 0..n {
        let r = Request::post("/v1/quotes/compare")
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap();
        let resp = app.clone().oneshot(r).await.unwrap();
        assert!(resp.status().is_success());
        let _ = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
    }
    let api_wall = t.elapsed();

    let snap = metrics.snapshot();
    println!(
        "scenario: ETH 2800C 30OCT26, buy 5, 2 platforms (replayed books, cache hits), sequential, {n} iterations per phase"
    );
    println!(
        "engine phase wall: {:?} ({:.0} compares/s)",
        engine_wall,
        n as f64 / engine_wall.as_secs_f64()
    );
    println!(
        "api phase wall:    {:?} ({:.0} req/s)",
        api_wall,
        n as f64 / api_wall.as_secs_f64()
    );
    println!("{}", serde_json::to_string_pretty(&snap).unwrap());
    engine.shutdown().await;
}
