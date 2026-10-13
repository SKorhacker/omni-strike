//! End-to-end tests of the critical journey through the real HTTP router, venue actors and
//! engine, fed by REPLAYED real responses (Provenance::Fixture) and an in-memory store.
//! No network. These tests do NOT verify the live integrations (see adapters live-tests).

use crate::api::{AppState, router};
use crate::engine::Engine;
use crate::fixtures::{FixtureConnector, Inject};
use crate::metrics::Metrics;
use crate::store::MemStore;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use oa_adapters::Connector;
use oa_core::domain::TradingMode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const EXPIRY: &str = "2026-10-30T08:00:00Z";

/// Decimal fields are JSON strings; compare by value, not representation ("1.00" == "1").
fn num(v: &Value) -> rust_decimal::Decimal {
    v.as_str()
        .unwrap_or_else(|| panic!("not a decimal string: {v}"))
        .parse()
        .unwrap()
}

struct Harness {
    app: Router,
    aevo: Arc<FixtureConnector>,
    store: Arc<MemStore>,
}

fn harness_with(token: Option<&str>) -> Harness {
    let derive = Arc::new(FixtureConnector::derive());
    let aevo = Arc::new(FixtureConnector::aevo());
    let store = Arc::new(MemStore::default());
    let engine = Arc::new(Engine::new(
        vec![
            derive as Arc<dyn Connector>,
            aevo.clone() as Arc<dyn Connector>,
        ],
        store.clone(),
        Arc::new(Metrics::default()),
        TradingMode::Simulation,
        Duration::from_secs(300),
        Duration::from_secs(10),
        4,
    ));
    Harness {
        app: router(AppState::new(engine, token.map(Into::into))),
        aevo,
        store,
    }
}

fn harness() -> Harness {
    harness_with(None)
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_auth(app, method, uri, body, None).await
}

async fn call_auth(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = b
        .body(
            body.map(|v| Body::from(v.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn eth_call_2800(qty: &str, accepted: Value) -> Value {
    json!({
        "underlying": "ETH", "option_kind": "call", "strike": "2800", "expiry": EXPIRY,
        "side": "buy", "quantity": qty, "accepted_differences": accepted,
        "baseline_platform": "derive"
    })
}

fn platforms_of(v: &Value, group: &str) -> Vec<String> {
    v[group]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            q["estimate"]["instrument"]["platform"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// Country filter with the installed policy: Aevo excludes Singapore, Derive does not.
#[tokio::test]
async fn venues_excluding_the_users_country_are_never_compared() {
    let h = harness();
    let accept = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    let mut req = eth_call_2800("1", accept.clone());
    req["user_country"] = json!("sg");
    let (s, c) = call(&h.app, "POST", "/v1/quotes/compare", Some(req)).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(platforms_of(&c, "ranked"), ["derive"]);
    let geo: Vec<_> = c["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| u["kind"] == "GEO_RESTRICTED")
        .map(|u| u["platform"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(geo, ["aevo"], "{c}");

    // A country no venue excludes: both venues are compared.
    let mut req = eth_call_2800("1", accept.clone());
    req["user_country"] = json!("BR");
    let (_, c) = call(&h.app, "POST", "/v1/quotes/compare", Some(req)).await;
    assert_eq!(platforms_of(&c, "ranked").len(), 2, "{c}");

    // Malformed code: refused, never silently ignored.
    let mut req = eth_call_2800("1", accept);
    req["user_country"] = json!("FRA");
    let (s, _) = call(&h.app, "POST", "/v1/quotes/compare", Some(req)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn journey_inventory_search_compare_explain_preview_simulate_replay() {
    let h = harness();

    // 1. Inventory: all 21 platforms, only the integrated ones are VERIFIED_READ_ONLY.
    let (s, p) = call(&h.app, "GET", "/v1/platforms", None).await;
    assert_eq!(s, StatusCode::OK);
    let list = p["platforms"].as_array().unwrap();
    assert_eq!(list.len(), 28);
    for x in list {
        let verified = x["integration_level"] == "VERIFIED_READ_ONLY";
        assert_eq!(
            verified,
            x["id"] == "derive"
                || x["id"] == "aevo"
                || x["id"] == "paradex"
                || x["id"] == "thetanuts"
                || x["id"] == "rysk"
                || x["id"] == "callput"
                || x["id"] == "hypercall"
                || x["id"] == "myriad"
                || x["id"] == "limitless"
                || x["id"] == "hegic"
                || x["id"] == "deribit"
                || x["id"] == "okx"
                || x["id"] == "bybit"
                || x["id"] == "binance"
                || x["id"] == "bullish"
                || x["id"] == "coincall"
                || x["id"] == "gate"
                || x["id"] == "delta"
                || x["id"] == "kalshi"
                || x["id"] == "hyperliquid",
            "{}",
            x["id"]
        );
        let in_harness = x["id"] == "derive" || x["id"] == "aevo";
        assert_eq!(x["implemented"].as_array().unwrap().is_empty(), !in_harness);
    }
    assert_eq!(p["mode"], "SIMULATION");

    // 2. Search: the ETH 2800 call of 30 Oct is listed on both platforms.
    let (s, sr) = call(
        &h.app,
        "GET",
        &format!("/v1/instruments?underlying=ETH&option_kind=call&expiry={EXPIRY}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{sr}");
    let g = sr["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["strike"] == "2800")
        .expect("2800 listed");
    assert_eq!(g["listings"].as_array().unwrap().len(), 2);
    assert!(sr["expiries"].as_array().unwrap().len() > 5);

    // 3. Compare WITHOUT accepting differences: Aevo is similar-with-differences -> ineligible.
    let (s, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(platforms_of(&c, "ranked"), ["derive"]);
    assert_eq!(platforms_of(&c, "ineligible"), ["aevo"]);
    let aevo = &c["ineligible"][0]["estimate"];
    assert_eq!(aevo["ineligible"], json!(["DIFFERENCES_NOT_ACCEPTED"]));
    assert_eq!(
        aevo["comparability"]["verdict"],
        "ECONOMICALLY_SIMILAR_WITH_DIFFERENCES"
    );
    assert_eq!(
        aevo["comparability"]["reasons"],
        json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"])
    );
    assert_eq!(
        c["comparability"][0]["report"]["verdict"],
        "ECONOMICALLY_SIMILAR_WITH_DIFFERENCES"
    );
    for q in c["ranked"]
        .as_array()
        .unwrap()
        .iter()
        .chain(c["ineligible"].as_array().unwrap())
    {
        assert_eq!(q["estimate"]["provenance"], "FIXTURE");
        assert_eq!(q["estimate"]["firmness"], "INDICATIVE");
        assert_eq!(q["funding"]["status"], "UNVERIFIED");
    }
    // Derive: 1 ETH at 103.2, fee = 0.5 + min(0.03% x 1 x index, 12.5% x mark) — all known.
    let d = &c["ranked"][0]["estimate"];
    assert_eq!(num(&d["covered_underlying"]), rust_decimal_macros::dec!(1));
    assert_eq!(
        num(&d["avg_price_per_underlying"]),
        rust_decimal_macros::dec!(103.2)
    );
    assert_eq!(d["fee"]["status"], "known");
    // Baseline (derive) is the only ranked destination: estimated saving vs itself is exactly 0.
    assert_eq!(
        num(&c["estimated_saving_vs_baseline"]["amount"]),
        rust_decimal_macros::dec!(0)
    );

    // 4. Explain.
    let (s, e) = call(&h.app, "POST", "/v1/comparability/explain", Some(json!({
        "a": {"platform": "derive", "network": "derive-v3-mainnet", "native_id": "ETH-20261030-2800-C"},
        "b": {"platform": "aevo", "network": "aevo-mainnet", "native_id": "ETH-30OCT26-2800-C"}}))).await;
    assert_eq!(s, StatusCode::OK, "{e}");
    assert_eq!(e["route_eligible_without_acceptance"], false);
    assert_eq!(
        e["codes_to_accept_for_economic_comparison"],
        json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"])
    );

    // 5. Accept the differences explicitly: both ranked, Aevo cheaper for 1 ETH.
    let accepted = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    let (_, c2) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", accepted.clone())),
    )
    .await;
    assert_eq!(platforms_of(&c2, "ranked"), ["aevo", "derive"]);
    let saving = &c2["estimated_saving_vs_baseline"];
    assert!(saving["label"].as_str().unwrap().contains("not realised"));
    assert!(saving["amount"].as_str().unwrap().parse::<f64>().unwrap() > 0.0);

    // 6. Preview + simulate with the same acceptance.
    let mut sim_req = eth_call_2800("1", accepted.clone());
    sim_req["price_tolerance_bps"] = json!(50);
    let (s, pv) = call(&h.app, "POST", "/v1/routes/preview", Some(sim_req.clone())).await;
    assert_eq!(s, StatusCode::OK, "{pv}");
    let route = &pv["route"];
    assert_eq!(route["destination"]["platform"], "aevo");
    assert_eq!(route["accepted_differences_in_scope"], accepted);
    // limit = 101.7 * 1.005 = 102.2085 -> floored to the 0.1 tick
    assert_eq!(
        num(&route["limit_price_per_underlying"]),
        rust_decimal_macros::dec!(102.2)
    );

    let (s, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(sim_req)).await;
    assert_eq!(s, StatusCode::OK, "{sim}");
    assert_eq!(sim["status"], "SIMULATED_FILL");
    assert_eq!(sim["accepted_differences"], accepted);
    assert_eq!(
        num(&sim["result"]["filled_underlying"]),
        rust_decimal_macros::dec!(1)
    );
    assert_eq!(sim["result"]["within_limit_price"], true);
    assert_eq!(sim["books_used"].as_array().unwrap().len(), 2);
    assert_eq!(h.store.simulations.lock().unwrap().len(), 1);

    // 7. Stored record round-trips and replays identically from recorded inputs.
    let id = sim["id"].as_str().unwrap();
    let (s, got) = call(&h.app, "GET", &format!("/v1/simulations/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["result"], sim["result"]);
    let (_, rp) = call(
        &h.app,
        "POST",
        &format!("/v1/simulations/{id}/replay"),
        None,
    )
    .await;
    assert_eq!(rp["identical"], true, "{rp}");
}

#[tokio::test]
async fn insufficient_depth_is_incomplete_not_ranked() {
    let h = harness();
    // Aevo shows 55 contracts of asks in the recorded book; Derive ~196.
    let accepted = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("100", accepted)),
    )
    .await;
    assert_eq!(platforms_of(&c, "ranked"), ["derive"]);
    assert_eq!(platforms_of(&c, "incomplete"), ["aevo"]);
    let a = &c["incomplete"][0]["estimate"];
    assert_eq!(num(&a["covered_underlying"]), rust_decimal_macros::dec!(55));
    assert!(
        a["ineligible"]
            .as_array()
            .unwrap()
            .contains(&json!("PARTIAL_COVERAGE"))
    );
    // Suggestion: the common fillable quantity (Aevo ETH step 0.1, Derive min 0.1) = 55.
    let cq = &c["common_quantity"];
    assert_eq!(num(&cq["quantity"]), rust_decimal_macros::dec!(55));
    assert_eq!(cq["platforms"], json!(["derive", "aevo"]));
    // Re-running at that quantity yields a complete comparison of both platforms.
    let accepted = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    let (_, c2) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("55", accepted)),
    )
    .await;
    assert_eq!(platforms_of(&c2, "ranked").len(), 2);
    assert!(c2["common_quantity"].is_null());
}

#[tokio::test]
async fn no_common_quantity_without_accepted_differences() {
    let h = harness();
    // Aevo is also blocked by DIFFERENCES_NOT_ACCEPTED: no suggestion may hide that.
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("100", json!([]))),
    )
    .await;
    assert!(c["common_quantity"].is_null());
}

#[tokio::test]
async fn no_comparable_listing_gives_no_route() {
    let h = harness();
    let mut r = eth_call_2800("1", json!([]));
    r["strike"] = json!("2799");
    let (s, pv) = call(&h.app, "POST", "/v1/routes/preview", Some(r)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(pv["route"].is_null());
    let kinds: Vec<_> = pv["compare"]["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["NO_LISTING", "NO_LISTING"]);
    assert!(!pv["no_route_reasons"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn simulated_balances_drive_funding_status() {
    let h = harness();
    let mut r = eth_call_2800("1", json!([]));
    r["simulated_balances"] = json!({"derive": "50"});
    let (_, c) = call(&h.app, "POST", "/v1/quotes/compare", Some(r)).await;
    assert_eq!(
        c["ranked"][0]["funding"]["status"],
        "SIMULATED_INSUFFICIENT"
    );
}

#[tokio::test]
async fn simulation_scenarios_price_move_and_strategy_limits() {
    let h = harness();
    let accepted = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    // Plain simulation: 4 stress scenarios on the recorded Aevo book (101.7 x 30 for 1 ETH).
    let mut r = eth_call_2800("1", accepted.clone());
    r["price_tolerance_bps"] = json!(50);
    let (s, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(r.clone())).await;
    assert_eq!(s, StatusCode::OK, "{sim}");
    assert_eq!(sim["status"], "SIMULATED_FILL");
    assert_eq!(sim["strategy"], "manual");
    let sc: Vec<(String, String)> = sim["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["name"].as_str().unwrap().into(),
                x["outcome"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(sc[0], ("as_recorded".into(), "FILL".into()));
    assert_eq!(
        sc[2],
        ("adverse_200bps".into(), "LIMIT_EXCEEDED".into()),
        "{sc:?}"
    );
    assert_eq!(sc[3].0, "half_depth");

    // The user saw 100 (lower than the fresh 101.7): limit = 100 * 1.005 = 100.5 -> rejected.
    let mut q = r.clone();
    q["quoted"] = json!({"platform": "aevo", "avg_price_per_underlying": "100", "computed_at": "2026-10-05T12:00:00Z"});
    let (_, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(q)).await;
    assert_eq!(sim["status"], "REJECTED_PRICE_MOVED", "{}", sim["route"]);
    assert_eq!(
        num(&sim["route"]["limit_price_per_underlying"]),
        rust_decimal_macros::dec!(100.5)
    );
    assert_eq!(
        num(&sim["price_drift"]["drift_bps"]),
        rust_decimal_macros::dec!(170)
    );
    let id = sim["id"].as_str().unwrap();
    let (_, rp) = call(
        &h.app,
        "POST",
        &format!("/v1/simulations/{id}/replay"),
        None,
    )
    .await;
    assert_eq!(rp["identical"], true);

    // Unknown strategy -> risk rejection.
    let mut u = r.clone();
    u["strategy"] = json!("yolo");
    let (_, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(u)).await;
    assert_eq!(sim["status"], "REJECTED_BY_RISK_CHECKS");
    assert!(
        sim["risk_violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "UNKNOWN_STRATEGY")
    );

    // Session usage accumulates across successful simulations.
    let (_, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(r)).await;
    assert!(num(&sim["session_usage_before"]["total"]) > rust_decimal_macros::dec!(0));
}

#[tokio::test]
async fn sell_simulation_is_rejected_by_risk_checks() {
    let h = harness();
    let mut r = eth_call_2800("1", json!([]));
    r["side"] = json!("sell");
    let (s, sim) = call(&h.app, "POST", "/v1/orders/simulate", Some(r)).await;
    assert_eq!(s, StatusCode::OK, "{sim}");
    assert_eq!(sim["status"], "REJECTED_BY_RISK_CHECKS");
    assert!(
        sim["risk_violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "SELL_DISABLED")
    );
}

#[tokio::test]
async fn input_validation() {
    let h = harness();
    let mut r = eth_call_2800("1", json!([]));
    r["underlying"] = json!("DOGE");
    assert_eq!(
        call(&h.app, "POST", "/v1/quotes/compare", Some(r)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let r = eth_call_2800("0", json!([]));
    assert_eq!(
        call(&h.app, "POST", "/v1/quotes/compare", Some(r)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // A core mismatch can never be "accepted".
    let r = eth_call_2800("1", json!(["STRIKE_MISMATCH"]));
    let (s, e) = call(&h.app, "POST", "/v1/quotes/compare", Some(r)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e["error"]["code"], "INVALID_REQUEST");
    let mut r = eth_call_2800("1", json!([]));
    r["unexpected"] = json!(1);
    assert_eq!(
        call(&h.app, "POST", "/v1/quotes/compare", Some(r)).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&h.app, "GET", "/v1/simulations/not-a-uuid", None)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn execution_is_disabled_and_positions_unsupported() {
    let h = harness();
    for (m, u) in [("POST", "/v1/orders"), ("POST", "/v1/orders/prepare")] {
        let (s, e) = call(&h.app, m, u, Some(json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{u}");
        assert_eq!(e["error"]["code"], "EXECUTION_DISABLED");
    }
    assert_eq!(
        call(&h.app, "GET", "/v1/positions", None).await.0,
        StatusCode::NOT_IMPLEMENTED
    );
}

#[tokio::test]
async fn upstream_timeout_and_invalid_response_are_reported_per_platform() {
    let h = harness();
    h.aevo.set_inject(Inject::InvalidResponse);
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    let u = &c["unavailable"][0];
    assert_eq!(
        (u["platform"].as_str(), u["kind"].as_str()),
        (Some("aevo"), Some("BOOK_UNAVAILABLE"))
    );
    assert!(u["detail"].as_str().unwrap().contains("invalid response"));
    assert_eq!(
        platforms_of(&c, "ranked"),
        ["derive"],
        "one failing platform does not hide the other"
    );

    let h = harness();
    h.aevo.set_inject(Inject::Timeout);
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert_eq!(c["unavailable"][0]["kind"], "CATALOG_UNAVAILABLE");
    assert!(
        c["unavailable"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
}

#[tokio::test]
async fn rate_limit_opens_the_circuit_then_recovers() {
    let h = harness();
    h.aevo
        .set_inject(Inject::RateLimited(Duration::from_millis(1200)));
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert!(
        c["unavailable"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("rate limited")
    );
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert!(
        c["unavailable"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("circuit open"),
        "{}",
        c["unavailable"]
    );
    let (_, p) = call(&h.app, "GET", "/v1/platforms", None).await;
    let aevo = p["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "aevo")
        .unwrap();
    assert_eq!(aevo["health"]["state"], "CIRCUIT_OPEN");
    // After the backoff, a healthy upstream is used again.
    h.aevo.set_inject(Inject::None);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert!(
        c["unavailable"].as_array().unwrap().is_empty(),
        "{}",
        c["unavailable"]
    );
}

#[tokio::test]
async fn upstream_stale_invalidates_but_never_opens_the_circuit() {
    let h = harness();
    h.aevo.set_inject(Inject::UpstreamStale);
    for _ in 0..6 {
        let (_, c) = call(
            &h.app,
            "POST",
            "/v1/quotes/compare",
            Some(eth_call_2800("1", json!([]))),
        )
        .await;
        let u = &c["unavailable"][0];
        assert_eq!(u["kind"], "BOOK_UNAVAILABLE");
        assert!(u["detail"].as_str().unwrap().contains("stale data"), "{u}");
    }
    let (_, p) = call(&h.app, "GET", "/v1/platforms", None).await;
    let aevo = p["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "aevo")
        .unwrap();
    assert_ne!(
        aevo["health"]["state"], "CIRCUIT_OPEN",
        "{}",
        aevo["health"]
    );
    h.aevo.set_inject(Inject::None);
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert!(c["unavailable"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn latency_budget_bounds_the_answer_and_slow_venues_join_the_next_one() {
    let h = harness();
    let mut req = eth_call_2800("1", json!([]));
    req["latency_budget_ms"] = json!(150);
    req["max_age_ms"] = json!(1000);
    // Catalogs and books loaded once (in production catalogs are pre-loaded and kept fresh).
    assert_eq!(
        call(&h.app, "POST", "/v1/quotes/compare", Some(req.clone()))
            .await
            .0,
        StatusCode::OK
    );
    // Aevo becomes slow; once its cached book is older than max_age, a refetch is needed.
    h.aevo.set_inject(Inject::Delay(Duration::from_millis(900)));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t = std::time::Instant::now();
    let (s, c) = call(&h.app, "POST", "/v1/quotes/compare", Some(req.clone())).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        t.elapsed() < Duration::from_millis(600),
        "bounded by the budget: {:?}",
        t.elapsed()
    );
    let slow: Vec<_> = c["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| u["kind"] == "SLOW_VENUE")
        .collect();
    assert_eq!(slow.len(), 1, "{}", c["unavailable"]);
    assert_eq!(slow[0]["platform"], "aevo");
    // The fetch kept running: once it lands, the same request includes Aevo.
    h.aevo.set_inject(Inject::None);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let (_, c) = call(&h.app, "POST", "/v1/quotes/compare", Some(req)).await;
    assert!(
        c["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["kind"] != "SLOW_VENUE"),
        "{}",
        c["unavailable"]
    );
    // Out-of-range budget is rejected.
    let mut bad = eth_call_2800("1", json!([]));
    bad["latency_budget_ms"] = json!(10);
    assert_eq!(
        call(&h.app, "POST", "/v1/quotes/compare", Some(bad))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn recently_viewed_books_are_kept_warm_in_the_background() {
    let h = harness();
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    assert!(c["unavailable"].as_array().unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let (_, m) = call(&h.app, "GET", "/v1/metrics", None).await;
    let hot = m["counters"]["hot_refreshes.aevo"].as_u64().unwrap_or(0);
    assert!(
        hot >= 1,
        "aevo book refreshed in the background: {}",
        m["counters"]
    );
    // The next comparison is served from the warm cache: data younger than the refresh period.
    let (_, c) = call(
        &h.app,
        "POST",
        "/v1/quotes/compare",
        Some(eth_call_2800("1", json!([]))),
    )
    .await;
    let a = c["ineligible"][0]["estimate"].clone();
    assert!(a["age_ms"].as_i64().unwrap() < 1500, "{}", a["age_ms"]);
}

#[tokio::test]
async fn strategy_quote_prices_every_leg_and_plans_with_the_atomicity_ladder() {
    let h = harness();
    let accepted = json!(["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"]);
    let leg = |side: &str| json!({"option_kind": "call", "strike": "2800", "expiry": EXPIRY, "side": side});
    // Buy and sell the same call: the plan pays the spread + fees (positive net cost).
    let (s, q) = call(
        &h.app,
        "POST",
        "/v1/strategies/quote",
        Some(json!({"underlying": "ETH", "quantity": "1", "legs": [leg("buy"), leg("sell")], "accepted_differences": accepted})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{q}");
    let plan = &q["plan"];
    assert!(plan.is_object(), "{q}");
    assert_eq!(plan["legs"].as_array().unwrap().len(), 2);
    assert!(
        num(&plan["net_cost"]) > rust_decimal::Decimal::ZERO,
        "round trip costs money: {}",
        plan["net_cost"]
    );
    assert!(matches!(
        plan["kind"].as_str(),
        Some("SINGLE_VENUE") | Some("SPLIT")
    ));
    assert_eq!(
        q["legs"][0]["quotes"].as_array().unwrap().len(),
        2,
        "both venues quote the buy leg"
    );
    // A leg nobody can fill -> explicit no_plan_reason, still 200 with per-leg details.
    let bad = json!({"option_kind": "call", "strike": "123456", "expiry": EXPIRY, "side": "buy"});
    let (s, q) = call(
        &h.app,
        "POST",
        "/v1/strategies/quote",
        Some(json!({"underlying": "ETH", "quantity": "1", "legs": [leg("buy"), bad]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(q["plan"].is_null());
    assert!(
        q["no_plan_reason"].as_str().unwrap().contains("leg 1"),
        "{}",
        q["no_plan_reason"]
    );
    // Validation.
    let (s, _) = call(
        &h.app,
        "POST",
        "/v1/strategies/quote",
        Some(json!({"underlying": "ETH", "quantity": "1", "legs": []})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

fn harness_admin() -> Router {
    let engine = Arc::new(Engine::new(
        vec![
            Arc::new(FixtureConnector::derive()) as Arc<dyn Connector>,
            Arc::new(FixtureConnector::aevo()) as Arc<dyn Connector>,
        ],
        Arc::new(MemStore::default()),
        Arc::new(Metrics::default()),
        TradingMode::Simulation,
        Duration::from_secs(300),
        Duration::from_secs(10),
        64,
    ));
    let mut st = AppState::new(engine, None);
    st.admin_token = Some("admin-token-0123456789abcdef".into());
    st.require_api_key = true;
    router(st)
}

#[tokio::test]
async fn integrators_keys_settings_rate_limits_and_limits() {
    let app = harness_admin();
    let admin = Some("admin-token-0123456789abcdef");
    // Admin endpoints need the admin token.
    let (s, _) = call_auth(
        &app,
        "POST",
        "/v1/admin/integrators",
        Some(json!({"name": "Acme"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, c) = call_auth(
        &app,
        "POST",
        "/v1/admin/integrators",
        Some(json!({"name": "Acme"})),
        admin,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    let key = c["api_key"].as_str().unwrap().to_string();
    assert!(key.starts_with("os_"));
    // Key required, wrong key refused, right key accepted.
    assert_eq!(
        call_auth(&app, "GET", "/v1/platforms", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call_auth(&app, "GET", "/v1/platforms", None, Some("os_nope"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (s, me) = call_auth(&app, "GET", "/v1/integrator/me", None, Some(&key)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["name"], "Acme");
    assert_eq!(
        me["settings"]["simulation_first"], true,
        "default limits ON"
    );
    // Self-service settings: within caps OK, beyond caps refused.
    let mut settings = me["settings"].clone();
    settings["allowed_underlyings"] = json!(["BTC"]);
    settings["rate_limit_rps"] = json!(3);
    let (s, upd) = call_auth(
        &app,
        "PUT",
        "/v1/integrator/settings",
        Some(settings.clone()),
        Some(&key),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{upd}");
    let mut too_much = settings.clone();
    too_much["rate_limit_rps"] = json!(100000);
    let (s, e) = call_auth(
        &app,
        "PUT",
        "/v1/integrator/settings",
        Some(too_much),
        Some(&key),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("self-service cap")
    );
    // Limits: ETH not allowed for this integrator.
    let leg = json!({"option_kind": "call", "strike": "2800", "expiry": EXPIRY, "side": "buy"});
    let (s, e) = call_auth(
        &app,
        "POST",
        "/v1/strategies/quote",
        Some(json!({"underlying": "ETH", "quantity": "1", "legs": [leg]})),
        Some(&key),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e["error"]["code"], "LIMIT_EXCEEDED");
    // ... and the single-option route path enforces the same limits.
    let (s, e) = call_auth(
        &app,
        "POST",
        "/v1/routes/quote",
        Some(
            json!({"underlying": "ETH", "option_kind": "call", "strike": "2800", "expiry": EXPIRY,
                    "side": "buy", "quantity": "1", "accepted_differences": []}),
        ),
        Some(&key),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert_eq!(e["error"]["code"], "LIMIT_EXCEEDED");
    // Rate limit: burst = 2 x 3 rps -> the 7th quick call is throttled.
    let mut codes = vec![];
    for _ in 0..8 {
        codes.push(
            call_auth(&app, "GET", "/v1/integrator/me", None, Some(&key))
                .await
                .0,
        );
    }
    assert!(codes.contains(&StatusCode::TOO_MANY_REQUESTS), "{codes:?}");
    // Admin can list (no key material) and deactivate.
    let (s, list) = call_auth(&app, "GET", "/v1/admin/integrators", None, admin).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!list.to_string().contains(&key));
    let id = c["integrator"]["id"].as_str().unwrap();
    let (s, _) = call_auth(
        &app,
        "PUT",
        &format!("/v1/admin/integrators/{id}/active"),
        Some(json!({"active": false})),
        admin,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        call_auth(&app, "GET", "/v1/platforms", None, Some(&key))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn sse_stream_pushes_shared_quote_events() {
    let h = harness();
    let uri = format!(
        "/v1/stream/quotes?underlying=ETH&option_kind=call&strike=2800&expiry={}&side=buy&quantity=1&interval_ms=250",
        EXPIRY.replace(':', "%3A")
    );
    let req = Request::builder()
        .method("GET")
        .uri(&uri)
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut body = resp.into_body().into_data_stream();
    use futures_util::StreamExt;
    let first = tokio::time::timeout(Duration::from_secs(10), body.next())
        .await
        .expect("event within 10 s")
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("event: quote"), "{text}");
    let data: Value =
        serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap();
    assert!(data["venues"].is_array());
    assert!(data["t"].is_string());
    // invalid quantity -> 422 (not a stream)
    let bad = uri.replace("quantity=1", "quantity=0");
    assert_eq!(
        call(&h.app, "GET", &bad, None).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn bearer_token_is_enforced_when_configured() {
    let h = harness_with(Some("s3cret"));
    assert_eq!(
        call(&h.app, "GET", "/v1/platforms", None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call_auth(&h.app, "GET", "/v1/platforms", None, Some("wrong"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call_auth(&h.app, "GET", "/v1/platforms", None, Some("s3cret"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h.app, "GET", "/health/live", None).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn readiness_and_openapi() {
    let h = harness();
    // Catalogs load on first use.
    let _ = call(&h.app, "GET", "/v1/instruments?underlying=BTC", None).await;
    let (s, r) = call(&h.app, "GET", "/health/ready", None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (_, doc) = call(&h.app, "GET", "/openapi.json", None).await;
    for p in [
        "/v1/platforms",
        "/v1/instruments",
        "/v1/quotes/compare",
        "/v1/routes/preview",
        "/v1/orders/simulate",
        "/health/live",
        "/health/ready",
    ] {
        assert!(doc["paths"].get(p).is_some(), "{p}");
    }
    assert!(
        doc["components"]["schemas"]
            .get("CompareResponse")
            .is_some()
    );
    assert!(doc["components"]["schemas"].get("QuoteEstimate").is_some());
}

/// LIVE journey through the real router/actors/engine with the REAL connectors (public,
/// read-only endpoints) and the in-memory store. Opt-in:
/// `cargo test -p oa-app --features live-tests live_journey -- --nocapture`
#[cfg(feature = "live-tests")]
#[tokio::test]
async fn live_journey_real_connectors() {
    use oa_adapters::aevo::AevoConnector;
    use oa_adapters::derive::DeriveConnector;
    let store = Arc::new(MemStore::default());
    let engine = Arc::new(Engine::new(
        vec![
            Arc::new(DeriveConnector::default()) as Arc<dyn Connector>,
            Arc::new(AevoConnector::default()),
        ],
        store.clone(),
        Arc::new(Metrics::default()),
        TradingMode::Simulation,
        Duration::from_secs(300),
        Duration::from_secs(10),
        4,
    ));
    let app = router(AppState::new(engine.clone(), None));

    let (s, sr) = call(
        &app,
        "GET",
        "/v1/instruments?underlying=BTC&option_kind=call&limit=1000",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{sr}");
    for c in sr["catalogs"].as_array().unwrap() {
        eprintln!(
            "catalog {} ok={} n={}",
            c["platform"], c["ok"], c["instruments"]
        );
        assert_eq!(c["ok"], true);
    }
    // First expiry >= 2 days ahead listed on both platforms, middle common strike.
    let now = chrono::Utc::now() + chrono::Duration::days(2);
    let exp = sr["expiries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["platforms"].as_array().unwrap().len() >= 2)
        .map(|e| e["expiry"].as_str().unwrap().to_string())
        .find(|e| e.parse::<chrono::DateTime<chrono::Utc>>().unwrap() > now)
        .expect("a common expiry");
    let groups: Vec<_> = sr["groups"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["expiry"] == exp.as_str() && g["listings"].as_array().unwrap().len() >= 2)
        .collect();
    let g = groups[groups.len() / 2];
    eprintln!("contract: BTC call {} {}", g["strike"], exp);

    let base = json!({"underlying": "BTC", "option_kind": "call", "strike": g["strike"], "expiry": exp,
                      "side": "buy", "quantity": "0.1", "accepted_differences": [], "baseline_platform": "derive"});
    let (s, c) = call(&app, "POST", "/v1/quotes/compare", Some(base.clone())).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    let all: Vec<&Value> = ["ranked", "incomplete", "ineligible"]
        .iter()
        .flat_map(|k| c[*k].as_array().unwrap())
        .collect();
    for q in &all {
        let e = &q["estimate"];
        eprintln!(
            "  {} {} prov={} age={}ms covered={}/{} avg={} fee={} total={} inel={}",
            e["instrument"]["platform"],
            e["instrument"]["native_id"],
            e["provenance"],
            e["age_ms"],
            e["covered_underlying"],
            e["requested_underlying"],
            e["avg_price_per_underlying"],
            e["fee"],
            e["total_estimated"],
            e["ineligible"]
        );
        assert_eq!(e["provenance"], "LIVE_DATA");
    }
    eprintln!("  unavailable: {}", c["unavailable"]);
    let verdict = &c["comparability"][0]["report"];
    eprintln!("  verdict: {} {}", verdict["verdict"], verdict["reasons"]);
    assert_ne!(verdict["verdict"], "STRICTLY_COMPARABLE");

    let codes = verdict["reasons"].clone();
    let mut acc = base.clone();
    acc["accepted_differences"] = codes.clone();
    acc["price_tolerance_bps"] = json!(50);
    let (s, sim) = call(&app, "POST", "/v1/orders/simulate", Some(acc)).await;
    assert_eq!(s, StatusCode::OK, "{sim}");
    eprintln!(
        "  simulation: {} route={} result={}",
        sim["status"], sim["route"]["destination"], sim["result"]
    );
    assert_eq!(sim["accepted_differences"], codes);
    let id = sim["id"].as_str().unwrap();
    let (_, rp) = call(&app, "POST", &format!("/v1/simulations/{id}/replay"), None).await;
    assert_eq!(rp["identical"], true);
    let (_, m) = call(&app, "GET", "/v1/metrics", None).await;
    for (k, v) in m["timings"].as_object().unwrap() {
        eprintln!(
            "  metric {k}: n={} p50={} max={}",
            v["count"], v["p50"], v["max"]
        );
    }
    engine.shutdown().await;
}

// ------------------------------------------------------------------ routing & testnet execution

mod routing_and_execution {
    use super::*;
    use crate::execution::ExecutionService;
    use crate::fixtures::{FakeFill, FakeVenue};
    use oa_core::domain::PlatformId;

    const ACCEPT: [&str; 2] = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"];

    struct Tn {
        app: Router,
        fake: Arc<FakeVenue>,
        store: Arc<MemStore>,
    }

    /// TESTNET engine: replayed books re-tagged as testnet networks, FAKE Aevo execution venue.
    fn testnet(fill: FakeFill, kill_switch: bool) -> Tn {
        let derive =
            Arc::new(FixtureConnector::derive().with_network(oa_adapters::derive::TESTNET_NETWORK));
        let aevo =
            Arc::new(FixtureConnector::aevo().with_network(oa_adapters::aevo::TESTNET_NETWORK));
        let store = Arc::new(MemStore::default());
        let engine = Arc::new(Engine::new(
            vec![derive as Arc<dyn Connector>, aevo as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Testnet,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        let fake = Arc::new(FakeVenue::new(
            PlatformId::Aevo,
            oa_adapters::aevo::TESTNET_NETWORK,
            fill,
        ));
        let mut exec = ExecutionService::new(
            TradingMode::Testnet,
            vec![fake.clone() as Arc<dyn oa_adapters::exec::ExecutionVenue>],
            store.clone(),
            kill_switch,
        );
        Arc::get_mut(&mut exec).unwrap().submit_timeout = Duration::from_millis(500);
        engine.attach_execution(exec).unwrap();
        Tn {
            app: router(AppState::new(engine, None)),
            fake,
            store,
        }
    }

    fn intent(qty: &str, split: bool) -> Value {
        json!({"underlying": "ETH", "option_kind": "call", "strike": "2800", "expiry": EXPIRY,
               "side": "buy", "quantity": qty, "accepted_differences": ACCEPT,
               "split": {"allow_split": split, "min_saving_bps": "0"}})
    }

    async fn quote_and_execute(t: &Tn, qty: &str) -> (Value, (StatusCode, Value)) {
        let (s, q) = call(&t.app, "POST", "/v1/routes/quote", Some(intent(qty, false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        let body = json!({"route_id": q["route_id"], "route_hash": q["route_hash"]});
        let r = call(&t.app, "POST", "/v1/routes/execute-testnet", Some(body)).await;
        (q, r)
    }

    fn strategy(legs: Value) -> Value {
        json!({"underlying": "ETH", "quantity": "1", "legs": legs, "accepted_differences": ACCEPT, "unwind_cost_cap_bps": 30})
    }

    fn leg(kind: &str, side: &str) -> Value {
        json!({"option_kind": kind, "strike": "2800", "expiry": EXPIRY, "side": side})
    }

    async fn route_and_execute(t: &Tn, legs: Value) -> (Value, (StatusCode, Value)) {
        let (s, r) = call(&t.app, "POST", "/v1/strategies/route", Some(strategy(legs))).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let body = json!({"route_id": r["route_id"], "route_hash": r["route_hash"]});
        let e = call(&t.app, "POST", "/v1/routes/execute-testnet", Some(body)).await;
        (r, e)
    }

    /// NON-CUSTODIAL Derive: prepare -> the USER signs (local key standing in for a wallet) ->
    /// submit -> the fake Derive receives the user's signature; the server never held a key.
    #[tokio::test]
    async fn noncustodial_prepare_sign_submit_flow() {
        use k256::ecdsa::SigningKey;
        use oa_adapters::aevo_exec::{eth_address, sign_digest};
        use oa_adapters::derive_user::{DeriveUserSignedExec, personal_digest};
        use std::sync::Mutex;

        // Fake Derive /private/order: records the request, answers "filled".
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let g = got.clone();
        let fake = axum::Router::new().route(
            "/private/order",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
                let g = g.clone();
                async move {
                    g.lock().unwrap().push(b.clone());
                    axum::Json(json!({"result": {"order": {"order_id": "o-1", "order_status": "filled",
                        "filled_amount": b["amount"], "average_price": b["limit_price"], "instrument_name": b["instrument_name"]}}}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });

        let derive = Arc::new(
            FixtureConnector::derive().with_network(oa_adapters::derive_exec::TESTNET_V3_NETWORK),
        );
        let store = Arc::new(MemStore::default());
        let engine = Arc::new(Engine::new(
            vec![derive as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Testnet,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        let venue = Arc::new(DeriveUserSignedExec::with_rest(&format!("http://{addr}")));
        let exec = ExecutionService::new(
            TradingMode::Testnet,
            vec![venue as Arc<dyn oa_adapters::exec::ExecutionVenue>],
            store.clone(),
            false,
        );
        engine.attach_execution(exec).unwrap();
        let app = router(AppState::new(engine, None));

        let (s, q) = call(&app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["executable"], true, "{q}");

        let key = SigningKey::from_slice(&[5u8; 32]).unwrap();
        let addr_hex = format!("0x{}", hex::encode(eth_address(&key)));
        let account = json!({"owner": addr_hex, "subaccount_id": 12345});
        let (s, p) = call(&app, "POST", "/v1/noncustodial/prepare",
            Some(json!({"route_id": q["route_id"], "route_hash": q["route_hash"], "account": account}))).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        let legs = p["legs"].as_array().unwrap();
        assert!(!legs.is_empty());
        assert_eq!(legs[0]["typed_data"]["primaryType"], "Action");

        // Nothing is sent before the user signs.
        assert!(got.lock().unwrap().is_empty());

        // The USER signs each digest + the auth timestamp.
        let sign = |d: &[u8; 32]| format!("0x{}", hex::encode(sign_digest(&key, d).unwrap()));
        let sigs: Vec<Value> = legs
            .iter()
            .map(|l| {
                let d: [u8; 32] =
                    hex::decode(l["digest"].as_str().unwrap().trim_start_matches("0x"))
                        .unwrap()
                        .try_into()
                        .unwrap();
                json!({"order_id": l["order_id"], "signature": sign(&d)})
            })
            .collect();
        let ts = chrono::Utc::now().timestamp_millis();
        let sub = json!({"route_id": q["route_id"], "auth_timestamp_ms": ts,
                         "auth_signature": sign(&personal_digest(&ts.to_string())), "signatures": sigs});
        let (s, e) = call(&app, "POST", "/v1/noncustodial/submit", Some(sub.clone())).await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["orders"][0]["state"], "FILLED", "{e}");
        let sent = got.lock().unwrap().clone();
        assert_eq!(sent.len(), legs.len());
        assert_eq!(sent[0]["signature"], sigs[0]["signature"]);

        // Single use: replaying the same signed submission is refused.
        let (s, _) = call(&app, "POST", "/v1/noncustodial/submit", Some(sub)).await;
        assert_ne!(s, StatusCode::OK);
        assert_eq!(
            got.lock().unwrap().len(),
            legs.len(),
            "replay never reaches the venue"
        );
    }

    /// MAINNET NON-CUSTODIAL (Derive v3, Ethereum L1 domain): custodial venues are dropped, the
    /// custodial execute path is refused, default caps apply (100 USDC/order), and a user-signed
    /// order under the cap goes through with a MAINNET signature (fake Derive host, nothing real).
    #[tokio::test]
    async fn mainnet_noncustodial_derive_flow_and_guards() {
        use k256::ecdsa::SigningKey;
        use oa_adapters::aevo_exec::{eth_address, sign_digest};
        use oa_adapters::derive_exec::{DeriveTestnetExec, TESTNET_REST, domain_separator};
        use oa_adapters::derive_user::{DeriveNet, DeriveUserSignedExec, personal_digest};
        use oa_adapters::exec::ExecutionVenue;
        use std::sync::Mutex;

        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let g = got.clone();
        let fake = axum::Router::new().route(
            "/private/order",
            axum::routing::post(move |axum::Json(b): axum::Json<Value>| {
                let g = g.clone();
                async move {
                    g.lock().unwrap().push(b.clone());
                    axum::Json(json!({"result": {"order": {"order_id": "m-1", "order_status": "filled",
                        "filled_amount": b["amount"], "average_price": b["limit_price"], "instrument_name": b["instrument_name"]}}}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });

        // Mainnet catalog (default fixture network = derive-v3-mainnet).
        let derive = Arc::new(FixtureConnector::derive());
        let store = Arc::new(MemStore::default());
        let engine = Arc::new(Engine::new(
            vec![derive as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::MainnetNonCustodial,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        // A CUSTODIAL venue offered alongside must be dropped by the service.
        let env = |k: &str| match k {
            "OA_DERIVE_TESTNET_WALLET" => {
                Some("0x0000000000000000000000000000000000000001".to_string())
            }
            "OA_DERIVE_TESTNET_SUBACCOUNT_ID" => Some("9".to_string()),
            "OA_DERIVE_TESTNET_SIGNER_KEY" => Some(
                "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318".to_string(),
            ),
            _ => None,
        };
        let custodial = DeriveTestnetExec::testnet_from_lookup(env, TESTNET_REST).unwrap();
        let nc = DeriveUserSignedExec::on(DeriveNet::MAINNET, &format!("http://{addr}"));
        let exec = ExecutionService::new(
            TradingMode::MainnetNonCustodial,
            vec![
                Arc::new(custodial) as Arc<dyn ExecutionVenue>,
                Arc::new(nc) as Arc<dyn ExecutionVenue>,
            ],
            store.clone(),
            false,
        );
        let ids = exec.venue_identities();
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert_eq!(ids[0]["custody"], "NON_CUSTODIAL");
        assert_eq!(ids[0]["chain_id"], 1);
        engine.attach_execution(exec).unwrap();
        let app = router(AppState::new(engine, None));

        // Custodial path: refused even with a valid route.
        let (s, q) = call(&app, "POST", "/v1/routes/quote", Some(intent("0.5", false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        let body = json!({"route_id": q["route_id"], "route_hash": q["route_hash"]});
        let (s, e) = call(&app, "POST", "/v1/routes/execute-testnet", Some(body)).await;
        assert_ne!(s, StatusCode::OK, "{e}");
        assert!(
            e.to_string()
                .contains("custodial execution does not exist on mainnet"),
            "{e}"
        );

        let key = SigningKey::from_slice(&[5u8; 32]).unwrap();
        let owner = format!("0x{}", hex::encode(eth_address(&key)));
        let account = json!({"owner": owner, "subaccount_id": 4242});

        // Over the default 100 USDC cap (1 ETH call at ~103 USDC): refused before any signature.
        let (s, q1) = call(&app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        assert_eq!(s, StatusCode::OK, "{q1}");
        let (s, p1) = call(&app, "POST", "/v1/noncustodial/prepare",
            Some(json!({"route_id": q1["route_id"], "route_hash": q1["route_hash"], "account": account}))).await;
        assert_ne!(s, StatusCode::OK, "over-cap order must be refused: {p1}");

        // Under the cap: prepare -> the USER signs (mainnet domain) -> submit.
        let (s, q) = call(&app, "POST", "/v1/routes/quote", Some(intent("0.5", false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["executable"], true, "{q}");
        let (s, p) = call(&app, "POST", "/v1/noncustodial/prepare",
            Some(json!({"route_id": q["route_id"], "route_hash": q["route_hash"], "account": account}))).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        let legs = p["legs"].as_array().unwrap().clone();
        assert_eq!(legs[0]["typed_data"]["domain"]["chainId"], 1, "{p}");
        assert!(
            got.lock().unwrap().is_empty(),
            "nothing sent before the user signs"
        );

        let sign = |d: &[u8; 32]| format!("0x{}", hex::encode(sign_digest(&key, d).unwrap()));
        let digest = |l: &Value| -> [u8; 32] {
            hex::decode(l["digest"].as_str().unwrap().trim_start_matches("0x"))
                .unwrap()
                .try_into()
                .unwrap()
        };
        let ts = chrono::Utc::now().timestamp_millis();
        let sigs: Vec<Value> = legs
            .iter()
            .map(|l| json!({"order_id": l["order_id"], "signature": sign(&digest(l))}))
            .collect();
        let sub = json!({"route_id": q["route_id"], "auth_timestamp_ms": ts,
                         "auth_signature": sign(&personal_digest(&ts.to_string())), "signatures": sigs});
        let (s, e) = call(&app, "POST", "/v1/noncustodial/submit", Some(sub)).await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["orders"][0]["state"], "FILLED", "{e}");
        assert_eq!(e["orders"][0]["mode"], "MAINNET_NONCUSTODIAL", "{e}");
        assert_eq!(got.lock().unwrap().len(), legs.len());
        // The digest the user signed is under the MAINNET separator (not replayable on testnet).
        assert_eq!(
            hex::encode(domain_separator(1)),
            "da616dfabb88681b08e1592820a41d55ddc62d68de110e327ae99d734506fe19"
        );
    }

    #[test]
    fn mainnet_limits_default_low_and_are_hard_capped() {
        use crate::engine::{MAINNET_HARD_MAX_ORDER, mainnet_nc_limits};
        let d = mainnet_nc_limits(|_| None);
        assert_eq!(d.mode, TradingMode::MainnetNonCustodial);
        assert!(d.kill_switch);
        assert!(!d.allow_sell);
        assert_eq!(d.max_order_cost, rust_decimal_macros::dec!(100));
        assert_eq!(d.max_session_cost, rust_decimal_macros::dec!(500));
        assert!(d.allowed_networks.contains("derive-v3-mainnet"));
        assert!(!d.allowed_networks.iter().any(|n| n.contains("testnet")));
        let big = mainnet_nc_limits(|k| (k == "OA_MAINNET_MAX_ORDER_USD").then(|| "1e9".into()));
        assert_eq!(big.max_order_cost, MAINNET_HARD_MAX_ORDER);
        let huge =
            mainnet_nc_limits(|k| (k == "OA_MAINNET_MAX_ORDER_USD").then(|| "999999999".into()));
        assert_eq!(huge.max_order_cost, MAINNET_HARD_MAX_ORDER);
        assert!(huge.max_session_cost >= huge.max_order_cost);
        let neg = mainnet_nc_limits(|k| (k == "OA_MAINNET_MAX_ORDER_USD").then(|| "-5".into()));
        assert_eq!(neg.max_order_cost, rust_decimal_macros::dec!(100));
    }

    /// NON-CUSTODIAL Aevo: the user signs with its signing key and the INTEGRATOR computes the HMAC
    /// with the user's API secret; Omni Strike forwards the byte-identical body.
    #[tokio::test]
    async fn noncustodial_aevo_signing_key_and_integrator_hmac() {
        use k256::ecdsa::SigningKey;
        use oa_adapters::aevo_exec::{auth_message, eth_address, hmac_signature, sign_digest};
        use oa_adapters::aevo_user::{AevoUserSignedExec, SIGNATURE_PLACEHOLDER};
        use std::sync::Mutex;

        let got: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let g = got.clone();
        let fake = axum::Router::new().route(
            "/orders",
            axum::routing::post(move |h: axum::http::HeaderMap, b: String| {
                let g = g.clone();
                async move {
                    let v: Value = serde_json::from_str(&b).unwrap();
                    g.lock()
                        .unwrap()
                        .push((h["aevo-signature"].to_str().unwrap().to_string(), b));
                    axum::Json(json!({"order_id": "0x1", "order_status": "filled",
                        "filled": v["amount"], "avg_price": v["limit_price"]}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });

        let aevo =
            Arc::new(FixtureConnector::aevo().with_network(oa_adapters::aevo::TESTNET_NETWORK));
        let store = Arc::new(MemStore::default());
        let engine = Arc::new(Engine::new(
            vec![aevo as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Testnet,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        let venue = Arc::new(AevoUserSignedExec::with_rest(&format!("http://{addr}")));
        let exec = ExecutionService::new(
            TradingMode::Testnet,
            vec![venue as Arc<dyn oa_adapters::exec::ExecutionVenue>],
            store.clone(),
            false,
        );
        engine.attach_execution(exec).unwrap();
        let app = router(AppState::new(engine, None));

        let (s, q) = call(&app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["executable"], true, "{q}");
        let account_key = SigningKey::from_slice(&[11u8; 32]).unwrap();
        let signing_key = SigningKey::from_slice(&[12u8; 32]).unwrap();
        let hexa = |k: &SigningKey| format!("0x{}", hex::encode(eth_address(k)));
        let (s, p) = call(&app, "POST", "/v1/noncustodial/prepare", Some(json!({
            "route_id": q["route_id"], "route_hash": q["route_hash"],
            "accounts": {"aevo": {"maker": hexa(&account_key), "signer": hexa(&signing_key), "api_key": "user-api-key"}}}))).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        let leg = &p["legs"][0];
        assert_eq!(leg["typed_data"]["primaryType"], "Order");
        let template = leg["body_template"].as_str().unwrap();

        // Integrator side.
        let d: [u8; 32] = hex::decode(leg["digest"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap()
            .try_into()
            .unwrap();
        let sig = format!("0x{}", hex::encode(sign_digest(&signing_key, &d).unwrap()));
        let body = template.replace(SIGNATURE_PLACEHOLDER, &sig);
        let ts = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap()
            .to_string();
        let hmac = hmac_signature(
            "user-api-secret",
            &auth_message("user-api-key", &ts, "POST", "/orders", &body),
        );
        let (s, e) = call(&app, "POST", "/v1/noncustodial/submit", Some(json!({
            "route_id": q["route_id"],
            "signatures": [{"order_id": leg["order_id"], "signature": sig, "venue_auth": {"timestamp_ns": ts, "hmac": hmac}}]}))).await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["orders"][0]["state"], "FILLED", "{e}");
        let sent = got.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![(hmac, body)],
            "the venue receives exactly what the integrator authenticated"
        );
    }

    /// A signature from the WRONG key is refused before anything reaches the venue.
    #[tokio::test]
    async fn noncustodial_wrong_signer_is_refused_locally() {
        use k256::ecdsa::SigningKey;
        use oa_adapters::aevo_exec::{eth_address, sign_digest};
        use oa_adapters::derive_user::{DeriveUserSignedExec, personal_digest};

        let derive = Arc::new(
            FixtureConnector::derive().with_network(oa_adapters::derive_exec::TESTNET_V3_NETWORK),
        );
        let store = Arc::new(MemStore::default());
        let engine = Arc::new(Engine::new(
            vec![derive as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Testnet,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        // Unroutable venue URL: if anything were sent, the order would end UNKNOWN, not REJECTED.
        let venue = Arc::new(DeriveUserSignedExec::with_rest("http://127.0.0.1:9"));
        let exec = ExecutionService::new(
            TradingMode::Testnet,
            vec![venue as Arc<dyn oa_adapters::exec::ExecutionVenue>],
            store.clone(),
            false,
        );
        engine.attach_execution(exec).unwrap();
        let app = router(AppState::new(engine, None));
        let (_, q) = call(&app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        let owner = SigningKey::from_slice(&[5u8; 32]).unwrap();
        let thief = SigningKey::from_slice(&[6u8; 32]).unwrap();
        let account = json!({"owner": format!("0x{}", hex::encode(eth_address(&owner))), "subaccount_id": 12345});
        let (_, p) = call(&app, "POST", "/v1/noncustodial/prepare",
            Some(json!({"route_id": q["route_id"], "route_hash": q["route_hash"], "account": account}))).await;
        let l = &p["legs"][0];
        let d: [u8; 32] = hex::decode(l["digest"].as_str().unwrap().trim_start_matches("0x"))
            .unwrap()
            .try_into()
            .unwrap();
        let ts = chrono::Utc::now().timestamp_millis();
        let sub = json!({"route_id": q["route_id"], "auth_timestamp_ms": ts,
            "auth_signature": format!("0x{}", hex::encode(sign_digest(&thief, &personal_digest(&ts.to_string())).unwrap())),
            "signatures": [{"order_id": l["order_id"], "signature": format!("0x{}", hex::encode(sign_digest(&thief, &d).unwrap()))}]});
        let (s, e) = call(&app, "POST", "/v1/noncustodial/submit", Some(sub)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{e}");
        assert!(e.to_string().contains("does not match"), "{e}");
        // Refused locally before anything was sent or consumed: the row is still only VALIDATED.
        assert!(
            store
                .orders
                .lock()
                .unwrap()
                .iter()
                .all(|o| o.state == crate::execution::OrderState::Validated)
        );
    }

    #[tokio::test]
    async fn strategy_legs_execute_in_parallel_with_sides() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            false,
        );
        let (r, (s, e)) =
            route_and_execute(&t, json!([leg("call", "buy"), leg("call", "sell")])).await;
        assert_eq!(r["executable"], true, "{}", r["not_executable_reasons"]);
        assert!(
            r["max_loss"].is_string(),
            "bounded strategy: {}",
            r["max_loss"]
        );
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["strategy_outcome"], "COMPLETE", "{e}");
        let sides: Vec<_> = e["orders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["side"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(sides, vec!["buy", "sell"]);
        assert!(e["unwind_orders"].as_array().unwrap().is_empty());
        assert_eq!(*t.fake.submissions.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn failed_leg_triggers_capped_unwind_of_the_filled_leg() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            false,
        );
        *t.fake.reject_next_sells.lock().unwrap() = 1; // the strategy's sell leg fails
        let (_, (s, e)) =
            route_and_execute(&t, json!([leg("call", "buy"), leg("call", "sell")])).await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["strategy_outcome"], "UNWOUND", "{e}");
        let u = e["unwind_orders"].as_array().unwrap();
        assert_eq!(u.len(), 1, "the filled BUY leg is sold back");
        assert_eq!(u[0]["side"], "sell");
        assert_eq!(u[0]["state"], "FILLED");
        assert!(
            u[0]["leg_index"].as_i64().unwrap() >= 100,
            "unwind legs are numbered from 100"
        );
        // journal: 2 legs + 1 unwind
        assert_eq!(*t.fake.submissions.lock().unwrap(), 3);
        assert!(e["notes"].to_string().contains("All-or-nothing"));
    }

    #[tokio::test]
    async fn naked_short_call_is_not_executable_by_default() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            false,
        );
        let (s, r) = call(
            &t.app,
            "POST",
            "/v1/strategies/route",
            Some(strategy(json!([leg("call", "sell")]))),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["executable"], false);
        assert!(
            r["not_executable_reasons"]
                .to_string()
                .contains("UNBOUNDED"),
            "{}",
            r["not_executable_reasons"]
        );
        let body = json!({"route_id": r["route_id"], "route_hash": r["route_hash"]});
        let (s, _) = call(&t.app, "POST", "/v1/routes/execute-testnet", Some(body)).await;
        assert_ne!(s, StatusCode::OK);
        assert_eq!(*t.fake.submissions.lock().unwrap(), 0, "nothing sent");
    }

    #[tokio::test]
    async fn quote_in_simulation_mode_is_not_executable() {
        let h = harness();
        let (s, q) = call(&h.app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        assert_eq!(q["executable"], false);
        assert!(
            q["not_executable_reasons"][0]
                .as_str()
                .unwrap()
                .contains("only in TESTNET")
        );
        let body = json!({"route_id": q["route_id"], "route_hash": q["route_hash"]});
        let (s, e) = call(&h.app, "POST", "/v1/routes/execute-testnet", Some(body)).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(e["error"]["code"], "EXECUTION_DISABLED");
    }

    #[tokio::test]
    async fn split_is_chosen_on_real_books_when_it_saves() {
        let h = harness();
        // Recorded books: Aevo 55 contracts at ~101.7, Derive deeper but 103.2+ -> buy 100 ETH.
        let (_, q) = call(
            &h.app,
            "POST",
            "/v1/routes/quote",
            Some(intent("100", true)),
        )
        .await;
        let p = &q["plan"];
        assert_eq!(p["kind"], "SPLIT", "{}", p["reason"]);
        assert_eq!(p["legs"].as_array().unwrap().len(), 2);
        assert!(num(&p["estimated_saving_vs_best_single"]) > rust_decimal_macros::dec!(0));
        assert_eq!(
            num(&p["covered_underlying"]),
            rust_decimal_macros::dec!(100)
        );
        // Without split: single venue (Derive), never a silent split.
        let (_, q) = call(
            &h.app,
            "POST",
            "/v1/routes/quote",
            Some(intent("100", false)),
        )
        .await;
        assert_eq!(q["plan"]["kind"], "SINGLE");
        assert_eq!(q["plan"]["legs"][0]["instrument"]["platform"], "derive");
    }

    #[tokio::test]
    async fn differences_must_be_accepted_to_route_across_venues() {
        let h = harness();
        let mut r = intent("1", true);
        r["accepted_differences"] = json!([]);
        let (_, q) = call(&h.app, "POST", "/v1/routes/quote", Some(r)).await;
        let aevo = q["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["instrument"]["platform"] == "aevo")
            .unwrap();
        assert_eq!(aevo["routable"], false);
        assert!(
            aevo["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("DifferencesNotAccepted"))
        );
        assert!(
            q["plan"]["legs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["instrument"]["platform"] == "derive")
        );
    }

    #[tokio::test]
    async fn testnet_happy_path_records_quote_vs_fill() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0.5),
            },
            false,
        );
        let (q, (s, r)) = quote_and_execute(&t, "1").await;
        assert_eq!(q["executable"], true, "{}", q["not_executable_reasons"]);
        // Only the venue with a testnet execution adapter is routable.
        let d = q["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["instrument"]["platform"] == "derive")
            .unwrap();
        assert!(
            d["reasons"]
                .as_array()
                .unwrap()
                .contains(&json!("NO_TESTNET_EXECUTION_FOR_THIS_VENUE"))
        );
        assert_eq!(s, StatusCode::OK, "{r}");
        let o = &r["orders"][0];
        assert_eq!(o["state"], "FILLED");
        assert_eq!(o["is_final"], true);
        assert_eq!(o["network"], "aevo-testnet");
        assert!(o["submit_to_ack_ms"].is_number());
        let (_, stats) = call(&t.app, "GET", "/v1/exec-stats", None).await;
        let v = &stats["venues"][0];
        assert_eq!(v["filled"], 1);
        assert!(v["slippage_bps_mean"].is_string(), "{v}");
        assert!(
            v["sample_note"]
                .as_str()
                .unwrap()
                .contains("not statistically meaningful")
        );
        let (s, got) = call(
            &t.app,
            "GET",
            &format!("/v1/orders/{}", o["id"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(got["state"], "FILLED");
    }

    #[tokio::test]
    async fn route_quote_is_single_use_and_hash_bound() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            false,
        );
        let (q, (s, _)) = quote_and_execute(&t, "1").await;
        assert_eq!(s, StatusCode::OK);
        let again = json!({"route_id": q["route_id"], "route_hash": q["route_hash"]});
        let (s, e) = call(&t.app, "POST", "/v1/routes/execute-testnet", Some(again)).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(
            e["error"]["message"]
                .as_str()
                .unwrap()
                .contains("already used")
        );
        let (_, q2) = call(&t.app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        let tampered = json!({"route_id": q2["route_id"], "route_hash": "00"});
        let (s, e) = call(&t.app, "POST", "/v1/routes/execute-testnet", Some(tampered)).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(e["error"]["message"].as_str().unwrap().contains("hash"));
        assert_eq!(
            *t.fake.submissions.lock().unwrap(),
            1,
            "exactly one order ever sent"
        );
    }

    #[tokio::test]
    async fn real_venue_balance_gates_routing() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            false,
        );
        *t.fake.available.lock().unwrap() = rust_decimal_macros::dec!(0);
        let (_, q) = call(&t.app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        let a = q["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["instrument"]["platform"] == "aevo")
            .unwrap();
        assert!(
            a["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r.as_str().unwrap().starts_with("INSUFFICIENT_FUNDS")),
            "{a}"
        );
        assert_eq!(q["executable"], false);
        assert_eq!(
            num(&q["balances"]["aevo"]["available"]),
            rust_decimal_macros::dec!(0)
        );
        // 50 USDC available: 1 ETH of the 2800C (~103 USDC) cannot be bought; the plan is capped.
        *t.fake.available.lock().unwrap() = rust_decimal_macros::dec!(50);
        let (_, q) = call(&t.app, "POST", "/v1/routes/quote", Some(intent("1", false))).await;
        let p = &q["plan"];
        assert!(
            num(&p["expected_total"]) <= rust_decimal_macros::dec!(50),
            "{p}"
        );
        assert_eq!(p["fully_covered"], false);
        assert_eq!(q["executable"], false);
        let (_, b) = call(&t.app, "GET", "/v1/balances", None).await;
        assert_eq!(
            num(&b["balances"]["aevo"]["available"]),
            rust_decimal_macros::dec!(50)
        );
        assert_eq!(*t.fake.submissions.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn kill_switch_blocks_execution() {
        let t = testnet(
            FakeFill::Full {
                improvement: rust_decimal_macros::dec!(0),
            },
            true,
        );
        let (q, (s, e)) = quote_and_execute(&t, "1").await;
        assert_eq!(q["executable"], false);
        assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
        assert_eq!(*t.fake.submissions.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn partial_fill_and_rejection_are_final_and_reported() {
        let t = testnet(FakeFill::Fraction(rust_decimal_macros::dec!(0.5)), false);
        let (_, (_, r)) = quote_and_execute(&t, "1").await;
        assert_eq!(r["orders"][0]["state"], "PARTIALLY_FILLED");
        assert_eq!(r["orders"][0]["is_final"], true);
        assert!(
            r["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("Partial execution"))
        );
        let t = testnet(FakeFill::Reject, false);
        let (_, (_, r)) = quote_and_execute(&t, "1").await;
        assert_eq!(r["orders"][0]["state"], "REJECTED");
        assert!(
            r["orders"][0]["last_error"]
                .as_str()
                .unwrap()
                .contains("INSUFFICIENT_BALANCE")
        );
    }

    #[tokio::test]
    async fn lost_ack_is_reconciled_never_resubmitted() {
        let t = testnet(FakeFill::AckLost, false);
        let (_, (s, r)) = quote_and_execute(&t, "1").await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let o = &r["orders"][0];
        assert_eq!(o["state"], "FILLED", "found by its precomputed id: {o}");
        assert!(o["reconcile_attempts"].as_u64().unwrap() >= 1);
        assert_eq!(
            *t.fake.submissions.lock().unwrap(),
            1,
            "NO resubmission after a lost ack"
        );
        let events: Vec<String> = t
            .store
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.1.clone())
            .collect();
        assert_eq!(
            &events[..4],
            ["VALIDATED", "RESERVED", "SUBMITTING", "UNKNOWN"],
            "intent persisted before sending"
        );
    }

    #[tokio::test]
    async fn order_never_created_stays_unknown_not_final() {
        let t = testnet(FakeFill::NeverCreated, false);
        let (_, (_, r)) = quote_and_execute(&t, "1").await;
        let o = &r["orders"][0];
        assert_eq!(o["state"], "UNKNOWN");
        assert_eq!(o["is_final"], false);
        assert_eq!(*t.fake.submissions.lock().unwrap(), 1);
        assert!(
            r["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap().contains("reconciles"))
        );
    }
}

mod reliability {
    use super::*;

    fn engine() -> (Arc<Engine>, Arc<MemStore>) {
        let store = Arc::new(MemStore::default());
        let e = Arc::new(Engine::new(
            vec![Arc::new(FixtureConnector::derive()) as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Simulation,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        (e, store)
    }

    /// With every in-flight permit taken, business routes are shed (503) but probes still answer.
    #[tokio::test]
    async fn health_probes_are_never_shed() {
        let (e, _) = engine();
        let app = router(AppState::new(e, None).with_max_inflight(0));
        let (s, _) = call(&app, "GET", "/health/live", None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, v) = call(&app, "GET", "/v1/platforms", None).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(v["error"]["code"], "SERVICE_BUSY");
    }

    #[tokio::test]
    async fn unknown_route_and_bad_path_answer_json() {
        let (e, _) = engine();
        let app = router(AppState::new(e, None));
        let (s, v) = call(&app, "GET", "/v1/nope", None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "NOT_FOUND");
        let (s, v) = call(&app, "GET", "/v1/orders/not-a-uuid", None).await;
        assert!(s.is_client_error(), "{s}");
        assert!(v["error"]["code"].is_string(), "{v}");
    }

    /// Observations go through the batching writer: written shortly after, never on the request path.
    #[tokio::test]
    async fn observations_are_written_by_the_batching_writer() {
        let (e, store) = engine();
        let app = router(AppState::new(e.clone(), None));
        for _ in 0..3 {
            let (s, _) = call(
                &app,
                "POST",
                "/v1/quotes/compare",
                Some(eth_call_2800("1", json!([]))),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(*store.quotes.lock().unwrap() >= 3);
        assert!(
            e.metrics
                .snapshot()
                .counters
                .get("observations_written")
                .copied()
                .unwrap_or(0)
                >= 3
        );
    }

    /// A stream on an invalid request sends an error event marked `last`, then closes.
    #[tokio::test]
    async fn push_stream_closes_on_an_invalid_request() {
        let (e, _) = engine();
        let hub = crate::push::PushHub::default();
        let q: crate::push::StreamQuery = serde_json::from_value(json!({
            "underlying": "ZZZZZZZZZZZZ", "option_kind": "call", "strike": "2800",
            "expiry": EXPIRY, "side": "buy", "quantity": "1", "interval_ms": 250
        }))
        .unwrap();
        let mut sub = hub.subscribe(&e, q).unwrap();
        let m = tokio::time::timeout(Duration::from_secs(3), sub.rx.recv())
            .await
            .unwrap()
            .unwrap();
        match &*m {
            crate::push::PushMsg::Error { last, .. } => assert!(*last),
            other => panic!("expected an error event, got {other:?}"),
        }
    }
}

mod prepared_persistence {
    use super::*;
    use crate::execution::ExecutionService;
    use k256::ecdsa::SigningKey;
    use oa_adapters::aevo_exec::{eth_address, sign_digest};
    use oa_adapters::derive_user::{DeriveUserSignedExec, personal_digest};

    const ACCEPT: [&str; 2] = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN"];

    /// A fresh engine + execution service over `store` (a "process"), Derive user-signed venue
    /// pointed at a fake host that fills everything.
    async fn process(store: Arc<MemStore>) -> Router {
        let fake = axum::Router::new()
            .route(
                "/private/order",
                axum::routing::post(|axum::Json(b): axum::Json<Value>| async move {
                    axum::Json(json!({"result": {"order": {"order_id": "o-1", "order_status": "filled",
                        "filled_amount": b["amount"], "average_price": b["limit_price"], "instrument_name": b["instrument_name"]}}}))
                }),
            )
            .route(
                "/private/get_subaccount",
                axum::routing::post(
                    |h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| async move {
                        axum::Json(json!({"result": {"subaccount_id": b["subaccount_id"],
                            "wallet": h["x-derivewallet"].to_str().unwrap(), "subaccount_value": "99.5",
                            "positions": [{"instrument_name": "ETH-20261030-2800-C", "amount": "1"}]}}))
                    },
                ),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, fake).await.unwrap() });
        let derive = Arc::new(
            FixtureConnector::derive().with_network(oa_adapters::derive_exec::TESTNET_V3_NETWORK),
        );
        let engine = Arc::new(Engine::new(
            vec![derive as Arc<dyn Connector>],
            store.clone(),
            Arc::new(Metrics::default()),
            TradingMode::Testnet,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        let venue = Arc::new(DeriveUserSignedExec::with_rest(&format!("http://{addr}")));
        let exec = ExecutionService::new(
            TradingMode::Testnet,
            vec![venue as Arc<dyn oa_adapters::exec::ExecutionVenue>],
            store,
            false,
        );
        engine.attach_execution(exec).unwrap();
        router(AppState::new(engine, None))
    }

    fn key() -> SigningKey {
        SigningKey::from_slice(&[5u8; 32]).unwrap()
    }

    async fn prepare(app: &Router) -> (StatusCode, Value) {
        let intent = json!({"underlying": "ETH", "option_kind": "call", "strike": "2800", "expiry": EXPIRY,
            "side": "buy", "quantity": "1", "accepted_differences": ACCEPT,
            "split": {"allow_split": false, "min_saving_bps": "0"}});
        let (s, q) = call(app, "POST", "/v1/routes/quote", Some(intent)).await;
        assert_eq!(s, StatusCode::OK, "{q}");
        let owner = format!("0x{}", hex::encode(eth_address(&key())));
        call(
            app,
            "POST",
            "/v1/noncustodial/prepare",
            Some(json!({"route_id": q["route_id"],
            "route_hash": q["route_hash"], "account": {"owner": owner, "subaccount_id": 12345}})),
        )
        .await
    }

    fn signed(p: &Value) -> Value {
        let k = key();
        let sign = |d: &[u8; 32]| format!("0x{}", hex::encode(sign_digest(&k, d).unwrap()));
        let sigs: Vec<Value> = p["legs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                let d: [u8; 32] =
                    hex::decode(l["digest"].as_str().unwrap().trim_start_matches("0x"))
                        .unwrap()
                        .try_into()
                        .unwrap();
                json!({"order_id": l["order_id"], "signature": sign(&d)})
            })
            .collect();
        let ts = chrono::Utc::now().timestamp_millis();
        json!({"route_id": p["route_id"], "auth_timestamp_ms": ts,
               "auth_signature": sign(&personal_digest(&ts.to_string())), "signatures": sigs})
    }

    /// Prepared on one process, signed and submitted on ANOTHER one sharing the database (restart,
    /// or a second instance behind the load balancer). Still single use.
    #[tokio::test]
    async fn prepared_execution_survives_a_restart_and_stays_single_use() {
        let store = Arc::new(MemStore::default());
        let first = process(store.clone()).await;
        let (s, p) = prepare(&first).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        drop(first);

        let second = process(store.clone()).await;
        let sub = signed(&p);
        // An incomplete submission is refused and does NOT burn the prepared execution.
        let mut partial = sub.clone();
        partial["signatures"] = json!([]);
        let (s, _) = call(&second, "POST", "/v1/noncustodial/submit", Some(partial)).await;
        assert_ne!(s, StatusCode::OK);
        // Forged signatures (another key) are refused BEFORE consuming it.
        let mut forged = sub.clone();
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        for sig in forged["signatures"].as_array_mut().unwrap() {
            let d = personal_digest("not the order");
            sig["signature"] = json!(format!(
                "0x{}",
                hex::encode(sign_digest(&other, &d).unwrap())
            ));
        }
        let (s, e) = call(&second, "POST", "/v1/noncustodial/submit", Some(forged)).await;
        assert_ne!(s, StatusCode::OK, "{e}");
        let (s, e) = call(
            &second,
            "POST",
            "/v1/noncustodial/submit",
            Some(sub.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{e}");
        assert_eq!(e["orders"][0]["state"], "FILLED", "{e}");

        let third = process(store).await;
        let (s, _) = call(&third, "POST", "/v1/noncustodial/submit", Some(sub)).await;
        assert_ne!(s, StatusCode::OK, "replayed on another instance");
    }

    /// The session cap is the journal's committed spend over the rolling window: a restart does not
    /// reset it, and orders closed with nothing filled do not count.
    #[tokio::test]
    async fn session_cap_comes_from_the_journal() {
        let store = Arc::new(MemStore::default());
        let app = process(store.clone()).await;
        let (s, p) = prepare(&app).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        // A big UNSIGNED preparation (1e6 > the TESTNET session cap) does not count: preparing
        // without signing cannot exhaust the cap for everybody else.
        let mut big = store.orders.lock().unwrap()[0].clone();
        big.id = uuid::Uuid::new_v4();
        big.limit_price_native = rust_decimal::Decimal::from(1_000_000);
        store.orders.lock().unwrap().push(big.clone());
        let (s, e) = prepare(&app).await;
        assert_eq!(s, StatusCode::OK, "unsigned rows must not count: {e}");

        // The same order once SENT (by a previous process) counts.
        for o in store
            .orders
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|o| o.id == big.id)
        {
            o.state = crate::execution::OrderState::Acknowledged;
            o.submitted_at = Some(chrono::Utc::now());
        }

        let restarted = process(store.clone()).await;
        let (s, e) = prepare(&restarted).await;
        assert_ne!(s, StatusCode::OK);
        assert!(e.to_string().contains("SESSION_COST_ABOVE_CAP"), "{e}");

        // Same row, but rejected by the venue with nothing filled: it no longer counts.
        for o in store
            .orders
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|o| o.id == big.id)
        {
            o.is_final = true;
            o.filled_native = rust_decimal::Decimal::ZERO;
        }
        let (s, e) = prepare(&restarted).await;
        assert_eq!(s, StatusCode::OK, "{e}");

        // The cap is checked again at SUBMISSION: spend committed after the preparation refuses
        // it, and nothing is sent.
        let (s, p2) = prepare(&restarted).await;
        assert_eq!(s, StatusCode::OK, "{p2}");
        for o in store
            .orders
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|o| o.id == big.id)
        {
            o.is_final = false;
            o.filled_native = o.qty_native;
        }
        let (s, e) = call(
            &restarted,
            "POST",
            "/v1/noncustodial/submit",
            Some(signed(&p2)),
        )
        .await;
        assert_ne!(s, StatusCode::OK);
        assert!(e.to_string().contains("SESSION_COST_ABOVE_CAP"), "{e}");
    }

    /// Derive REST login: personal_sign of the bare timestamp.
    fn wallet_auth(k: &SigningKey) -> Value {
        let ts = chrono::Utc::now().timestamp_millis();
        let sig = sign_digest(k, &personal_digest(&ts.to_string())).unwrap();
        json!({"wallet": format!("0x{}", hex::encode(eth_address(k))), "timestamp_ms": ts,
               "signature": format!("0x{}", hex::encode(sig))})
    }

    /// Omni Strike "read my orders": personal_sign of the domain-bound message.
    fn orders_auth(k: &SigningKey) -> Value {
        let ts = chrono::Utc::now().timestamp_millis();
        let w = format!("0x{}", hex::encode(eth_address(k)));
        let sig = sign_digest(k, &personal_digest(&crate::api::orders_message(&w, ts))).unwrap();
        json!({"wallet": w, "timestamp_ms": ts, "signature": format!("0x{}", hex::encode(sig))})
    }

    /// A user lists THEIR orders (and only theirs) after proving control of the wallet; the Derive
    /// REST auth signature is not journaled; the public journal is closed on mainnet; positions and
    /// balances come from Derive with the user's own auth.
    #[tokio::test]
    async fn users_read_their_own_orders_and_derive_account() {
        let store = Arc::new(MemStore::default());
        let app = process(store.clone()).await;
        let (s, p) = prepare(&app).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        let (s, e) = call(&app, "POST", "/v1/noncustodial/submit", Some(signed(&p))).await;
        assert_eq!(s, StatusCode::OK, "{e}");

        let (s, mine) = call(
            &app,
            "POST",
            "/v1/noncustodial/orders",
            Some(orders_auth(&key())),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{mine}");
        let orders = mine["orders"].as_array().unwrap();
        assert_eq!(orders.len(), 1, "{mine}");
        assert_eq!(orders[0]["state"], "FILLED");
        assert_eq!(
            orders[0]["venue_meta"]["user"]["auth_signature"],
            "[redacted]"
        );

        let other = SigningKey::from_slice(&[6u8; 32]).unwrap();
        let (s, theirs) = call(
            &app,
            "POST",
            "/v1/noncustodial/orders",
            Some(orders_auth(&other)),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(theirs["orders"], json!([]), "another wallet sees nothing");

        // Someone else's address with my signature: refused.
        let mut forged = orders_auth(&other);
        forged["wallet"] = mine["wallet"].clone();
        let (s, _) = call(&app, "POST", "/v1/noncustodial/orders", Some(forged)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        // A venue login signature (bare timestamp) is not accepted here.
        let (s, _) = call(
            &app,
            "POST",
            "/v1/noncustodial/orders",
            Some(wallet_auth(&key())),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

        let mut req = wallet_auth(&key());
        req["subaccount_id"] = json!(12345);
        let (s, a) = call(&app, "POST", "/v1/noncustodial/derive/account", Some(req)).await;
        assert_eq!(s, StatusCode::OK, "{a}");
        assert_eq!(a["account"]["positions"][0]["amount"], "1");
        assert_eq!(
            a["account"]["wallet"], mine["wallet"],
            "forwarded with the user's own auth"
        );

        // Mainnet non-custodial: the journal is not listable publicly.
        let mainnet = Arc::new(Engine::new(
            vec![],
            store,
            Arc::new(Metrics::default()),
            TradingMode::MainnetNonCustodial,
            Duration::from_secs(300),
            Duration::from_secs(10),
            4,
        ));
        let mainnet = router(AppState::new(mainnet, None));
        let (s, e) = call(&mainnet, "GET", "/v1/orders", None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
        assert!(e.to_string().contains("PRIVATE"), "{e}");
    }
}
