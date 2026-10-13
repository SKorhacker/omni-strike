//! HTTP API v1 (Axum). Thin layer: validation + mapping to `Engine`. Errors are typed:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

use crate::engine::{
    CompareRequest, Engine, EngineError, ExplainRequest, RouteQuoteRequest, SearchQuery,
    SimulateRequest, replay,
};
use crate::execution::ExecError;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Default wall-clock cap per request (OA_REQUEST_TIMEOUT_MS overrides). Execution routes are exempt:
/// an order being sent must never be cut by a timeout (its outcome would be unknown).
pub const REQUEST_TIMEOUT_MS: u64 = 20_000;
/// Paths that send orders: never timed out (and run detached, see `execute_testnet`/`noncustodial_submit`).
const NO_TIMEOUT_PATHS: [&str; 2] = ["/v1/routes/execute-testnet", "/v1/noncustodial/submit"];
/// Default in-flight request cap (OA_MAX_INFLIGHT overrides). Requests are mostly in-memory work.
pub const MAX_INFLIGHT_REQUESTS: usize = 4096;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub api_token: Option<String>,
    /// Live savings tracker (dashboard); None when not started (tests, MCP).
    pub live: Option<Arc<crate::live::LiveTracker>>,
    /// Integrator API keys, settings and rate limits.
    pub integrators: Arc<crate::integrators::Integrators>,
    /// Shared push topics (SSE quote streams).
    pub push: Arc<crate::push::PushHub>,
    /// OA_ADMIN_TOKEN: required for /v1/admin/* (None = admin endpoints disabled).
    pub admin_token: Option<String>,
    /// OA_REQUIRE_API_KEY=true: every /v1 call needs an integrator key (or the legacy token).
    pub require_api_key: bool,
    /// Per-client-IP limit for callers without an integrator key (OA_PUBLIC_RPS, 0 = off).
    pub public_limiter: Arc<IpLimiter>,
    inflight: Arc<tokio::sync::Semaphore>,
}

/// Token bucket per client IP (burst = 2 x rps) for anonymous / shared-token callers, e.g. the
/// public web UI behind Caddy. Loopback clients (local tools) are exempt. Bounded memory: idle
/// entries are dropped when the table grows.
#[derive(Debug)]
pub struct IpLimiter {
    rps: u32,
    buckets: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, (f64, Instant)>>,
}

impl IpLimiter {
    pub fn new(rps: u32) -> Self {
        Self {
            rps,
            buckets: Default::default(),
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("OA_PUBLIC_RPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
        )
    }

    pub fn allow(&self, ip: std::net::IpAddr) -> bool {
        if self.rps == 0 || ip.is_loopback() {
            return true;
        }
        let (rate, cap) = (self.rps as f64, (self.rps * 2) as f64);
        let now = Instant::now();
        let mut g = self.buckets.lock().expect("ip buckets");
        if g.len() > 50_000 {
            g.retain(|_, (_, at)| now.duration_since(*at).as_secs() < 60);
        }
        let (tokens, at) = g.entry(ip).or_insert((cap, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * rate).min(cap);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Client IP: the peer address, or the right-most `X-Forwarded-For` entry when the peer is a local
/// proxy (private / loopback address, e.g. Caddy on the compose network). None without ConnectInfo.
fn client_ip(req: &Request, headers: &HeaderMap) -> Option<std::net::IpAddr> {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()?
        .0
        .ip();
    let proxied = match peer {
        std::net::IpAddr::V4(v) => v.is_private() || v.is_loopback(),
        std::net::IpAddr::V6(v) => v.is_loopback() || (v.segments()[0] & 0xfe00) == 0xfc00,
    };
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok());
    Some(if proxied {
        forwarded.unwrap_or(peer)
    } else {
        peer
    })
}

impl AppState {
    /// Override the global in-flight cap (tests, embedding).
    pub fn with_max_inflight(mut self, n: usize) -> Self {
        self.inflight = Arc::new(tokio::sync::Semaphore::new(n));
        self
    }

    pub fn new(engine: Arc<Engine>, api_token: Option<String>) -> Self {
        Self {
            engine,
            api_token,
            live: None,
            integrators: Arc::new(crate::integrators::Integrators::in_memory()),
            push: Arc::new(crate::push::PushHub::default()),
            admin_token: None,
            require_api_key: false,
            public_limiter: Arc::new(IpLimiter::new(0)),
            inflight: Arc::new(tokio::sync::Semaphore::new(
                std::env::var("OA_MAX_INFLIGHT")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(MAX_INFLIGHT_REQUESTS),
            )),
        }
    }
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Invalid(m) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_REQUEST", m)
            }
            EngineError::NotFound(m) => Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", m),
            EngineError::Busy(m) => Self::new(StatusCode::SERVICE_UNAVAILABLE, "SERVICE_BUSY", m),
            EngineError::Upstream(m) => {
                Self::new(StatusCode::BAD_GATEWAY, "UPSTREAM_UNAVAILABLE", m)
            }
            EngineError::Persistence(m) => {
                tracing::error!(error = %m, "persistence error");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "PERSISTENCE_ERROR",
                    "could not persist or read the record",
                )
            }
        }
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Path parameters whose rejections use our error format (a malformed id answers JSON 400, not
/// plain text).
pub struct ApiPath<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiPath<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(ApiPath(v)),
            Err(e) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                e.body_text(),
            )),
        }
    }
}

/// Query string extractor whose rejections use our error format.
pub struct ApiQuery<T>(pub T);

impl<S, T> axum::extract::FromRequestParts<S> for ApiQuery<T>
where
    T: serde::de::DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(ApiQuery(v)),
            Err(e) => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_REQUEST",
                e.body_text(),
            )),
        }
    }
}

/// JSON body extractor whose rejections use our error format.
pub struct ApiJson<T>(pub T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;
    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) => {
                // Keep the real cause: body too large (413) or not JSON (415) are not schema errors.
                let (status, code) = match e.status() {
                    StatusCode::PAYLOAD_TOO_LARGE => {
                        (StatusCode::PAYLOAD_TOO_LARGE, "BODY_TOO_LARGE")
                    }
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => {
                        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "UNSUPPORTED_MEDIA_TYPE")
                    }
                    _ => (StatusCode::UNPROCESSABLE_ENTITY, "INVALID_REQUEST"),
                };
                Err(ApiError::new(status, code, e.body_text()))
            }
        }
    }
}

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/platforms", get(platforms))
        .route("/instruments", get(instruments))
        .route("/quotes/compare", post(compare))
        .route("/comparability/explain", post(explain))
        .route("/routes/preview", post(preview))
        .route("/orders/simulate", post(simulate))
        .route("/simulations/{id}", get(get_simulation))
        .route("/simulations/{id}/replay", post(replay_simulation))
        .route("/metrics", get(metrics))
        // Executable routing. Execution exists only in TESTNET mode (403 otherwise).
        .route("/routes/quote", post(route_quote))
        .route("/routes/execute-testnet", post(execute_testnet))
        .route("/orders", get(list_orders).post(execution_disabled))
        .route("/orders/prepare", post(execution_disabled))
        .route("/orders/{id}", get(get_order))
        .route("/orders/{id}/cancel", post(cancel_order))
        .route("/exec-stats", get(exec_stats))
        .route("/balances", get(balances))
        .route("/positions", get(positions_unsupported))
        .route("/live/savings", get(live_savings))
        .route("/strategies/quote", post(strategy_quote))
        .route("/strategies/route", post(strategy_route))
        .route("/binaries/compare", post(binaries_compare))
        .route("/noncustodial/prepare", post(noncustodial_prepare))
        .route("/noncustodial/submit", post(noncustodial_submit))
        .route(
            "/noncustodial/derive/subaccounts",
            post(noncustodial_derive_subaccounts),
        )
        .route("/noncustodial/orders", post(noncustodial_orders))
        .route(
            "/noncustodial/derive/account",
            post(noncustodial_derive_account),
        )
        .route("/stream/quotes", get(stream_quotes))
        .route("/integrator/me", get(integrator_me))
        .route("/integrator/settings", put(integrator_update_settings))
        .route(
            "/admin/integrators",
            get(admin_list_integrators).post(admin_create_integrator),
        )
        .route(
            "/admin/integrators/{id}/settings",
            put(admin_update_settings),
        )
        .route("/admin/integrators/{id}/active", put(admin_set_active));
    let timeout = std::time::Duration::from_millis(
        std::env::var("OA_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(REQUEST_TIMEOUT_MS)
            .clamp(1_000, 600_000),
    );
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status": "live"})) }),
        )
        .route("/health/ready", get(ready))
        .route(
            "/openapi.json",
            get(|| async { Json(crate::openapi::document()) }),
        )
        .route("/docs", get(docs_page))
        .route(
            "/docs/integrators.md",
            get(|| async {
                (
                    [("content-type", "text/markdown; charset=utf-8")],
                    INTEGRATOR_GUIDE,
                )
            }),
        )
        .nest("/v1", v1)
        .fallback(|| async {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "no such route (see /docs)",
            )
        })
        .route_layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(middleware::from_fn(move |req, next| {
            timeout_and_panic(timeout, req, next)
        }))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // gzip/brotli when the client accepts it; SSE, images and tiny bodies are left alone.
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state)
}

/// Outermost guard: a request that runs longer than `limit` answers 504 (JSON) instead of holding a
/// connection and a permit; a handler panic answers 500 (JSON) instead of resetting the connection.
async fn timeout_and_panic(limit: std::time::Duration, req: Request, next: Next) -> Response {
    use futures_util::FutureExt;
    let exempt = NO_TIMEOUT_PATHS.contains(&req.uri().path());
    let run = std::panic::AssertUnwindSafe(next.run(req)).catch_unwind();
    let out = if exempt {
        Ok(run.await)
    } else {
        tokio::time::timeout(limit, run).await
    };
    match out {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => {
            tracing::error!("handler panicked");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "internal error (logged)",
            )
            .into_response()
        }
        Err(_) => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "TIMEOUT",
            format!("request exceeded {} ms", limit.as_millis()),
        )
        .into_response(),
    }
}

/// Auth (optional bearer token), global in-flight limit and latency measurement.
async fn guard(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> Response {
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".into());
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let unauthorized =
        |m: &str| ApiError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", m).into_response();
    let mut req = req;
    if path.starts_with("/v1/admin") {
        let ok = match (&st.admin_token, &bearer) {
            (Some(a), Some(b)) => constant_time_eq(a.as_bytes(), b.as_bytes()),
            _ => false,
        };
        if !ok {
            return unauthorized("admin token required (OA_ADMIN_TOKEN)");
        }
    } else if !path.starts_with("/health") && !path.starts_with("/docs") && path != "/openapi.json"
    {
        let legacy = matches!((&st.api_token, &bearer), (Some(t), Some(b)) if constant_time_eq(t.as_bytes(), b.as_bytes()));
        let public_limited =
            || client_ip(&req, &headers).is_some_and(|ip| !st.public_limiter.allow(ip));
        match &bearer {
            _ if legacy && public_limited() => return rate_limited(),
            _ if legacy => {}
            Some(b) => match st.integrators.authenticate(b) {
                Some(i) => {
                    if !st.integrators.allow(i.id, i.settings.rate_limit_rps) {
                        st.engine.metrics.incr(format!("rate_limited {}", i.id));
                        let mut r = ApiError::new(
                            StatusCode::TOO_MANY_REQUESTS,
                            "RATE_LIMITED",
                            format!("rate limit {} req/s exceeded", i.settings.rate_limit_rps),
                        )
                        .into_response();
                        r.headers_mut()
                            .insert("retry-after", axum::http::HeaderValue::from_static("1"));
                        return r;
                    }
                    req.extensions_mut().insert(i);
                }
                None => return unauthorized("unknown or inactive API key"),
            },
            None if st.api_token.is_some() || st.require_api_key => {
                return unauthorized("missing API key (Authorization: Bearer <key>)");
            }
            None if public_limited() => return rate_limited(),
            None => {}
        }
    }
    // Probes are never shed: an overloaded but alive process must not be restarted for being busy.
    if path.starts_with("/health") {
        return next.run(req).await;
    }
    let Ok(_permit) = st.inflight.clone().try_acquire_owned() else {
        st.engine.metrics.incr("api_rejected_busy");
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVICE_BUSY",
            "too many in-flight requests",
        )
        .into_response();
    };
    let t = Instant::now();
    let resp = next.run(req).await;
    st.engine
        .metrics
        .observe(format!("api_latency {path}"), t.elapsed());
    st.engine
        .metrics
        .incr(format!("api_status {} {path}", resp.status().as_u16()));
    resp
}

fn rate_limited() -> Response {
    let mut r = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "RATE_LIMITED",
        "too many requests from this address",
    )
    .into_response();
    r.headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    r
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn ready(State(st): State<AppState>) -> Response {
    let r = st.engine.readiness().await;
    let code = if r.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(r)).into_response()
}

#[derive(Serialize)]
struct PlatformsResponse {
    mode: oa_core::domain::TradingMode,
    platforms: Vec<crate::engine::PlatformView>,
}

async fn platforms(State(st): State<AppState>) -> Json<PlatformsResponse> {
    Json(PlatformsResponse {
        mode: st.engine.mode,
        platforms: st.engine.platforms().await,
    })
}

async fn instruments(
    State(st): State<AppState>,
    q: Result<Query<SearchQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<crate::engine::SearchResponse> {
    let Query(q) = q.map_err(|e| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_REQUEST",
            e.body_text(),
        )
    })?;
    Ok(Json(st.engine.search(q).await?))
}

async fn compare(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<CompareRequest>,
) -> ApiResult<crate::engine::CompareResponse> {
    Ok(Json(st.engine.compare(r).await?.response))
}

async fn explain(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<ExplainRequest>,
) -> ApiResult<crate::engine::ExplainResponse> {
    Ok(Json(st.engine.explain(r).await?))
}

async fn preview(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<SimulateRequest>,
) -> ApiResult<crate::engine::RoutePreview> {
    let (p, _) = st
        .engine
        .preview(r.compare, r.price_tolerance_bps.unwrap_or(50))
        .await?;
    Ok(Json(p))
}

async fn simulate(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<SimulateRequest>,
) -> ApiResult<crate::engine::SimulationRecord> {
    Ok(Json(st.engine.simulate(r).await?))
}

fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(id).map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_REQUEST",
            "id must be a UUID",
        )
    })
}

async fn get_simulation(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<crate::engine::SimulationRecord> {
    Ok(Json(st.engine.get_simulation(parse_id(&id)?).await?))
}

async fn replay_simulation(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<serde_json::Value> {
    let rec = st.engine.get_simulation(parse_id(&id)?).await?;
    let recomputed = replay(&rec);
    Ok(Json(json!({
        "id": rec.id,
        "stored": rec.result,
        "recomputed": recomputed,
        "identical": recomputed == rec.result,
        "note": "Replay uses only the recorded books, contract terms and fee schedules (no network).",
    })))
}

async fn metrics(State(st): State<AppState>) -> Json<crate::metrics::Snapshot> {
    Json(st.engine.metrics.snapshot())
}

impl From<ExecError> for ApiError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::Disabled(m) => Self::new(StatusCode::FORBIDDEN, "EXECUTION_DISABLED", m),
            ExecError::Route(m) => Self::new(StatusCode::CONFLICT, "ROUTE_INVALID", m),
            ExecError::Risk(v) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "RISK_REJECTED",
                serde_json::to_string(&v).unwrap_or_default(),
            ),
            ExecError::NotFound(m) => Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", m),
            ExecError::Persistence(m) => {
                tracing::error!(error = %m, "execution persistence error");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "PERSISTENCE_ERROR",
                    "journal write/read failed",
                )
            }
        }
    }
}

async fn route_quote(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(r): ApiJson<RouteQuoteRequest>,
) -> ApiResult<crate::engine::RouteQuote> {
    enforce_limits(&i, &r.compare.underlying, None)?;
    // Notional of a single option = quantity x strike (binaries: the payout).
    let notional = r.compare.quantity * r.compare.strike.max(oa_core::Decimal::ONE);
    enforce_limits(&i, &r.compare.underlying, Some(notional))?;
    let q = st.engine.route_quote(r).await?;
    if let Some(axum::Extension(me)) = &i {
        if let Some(p) = q
            .plan
            .iter()
            .flat_map(|p| &p.legs)
            .find(|l| !me.settings.allows_platform(l.instrument.platform))
        {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "LIMIT_EXCEEDED",
                format!(
                    "venue {} not allowed by your settings (allowed_platforms)",
                    p.instrument.platform
                ),
            ));
        }
        st.integrators.note_route(q.route_id, me.id, notional);
    }
    Ok(Json(q))
}

/// Daily notional, charged when a quoted route is executed (testnet) or prepared (non-custodial).
fn charge_daily(st: &AppState, i: &MaybeIntegrator, route_id: Uuid) -> Result<(), ApiError> {
    match i {
        Some(axum::Extension(me)) => st
            .integrators
            .charge_daily(route_id, me)
            .map_err(|m| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "LIMIT_EXCEEDED", m)),
        None => Ok(()),
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecuteRequest {
    pub route_id: Uuid,
    pub route_hash: String,
}

/// The integrator's webhook target, if it set one.
fn webhook_of(i: &MaybeIntegrator) -> Option<(String, String)> {
    i.as_ref().and_then(|axum::Extension(me)| {
        me.settings
            .webhook_url
            .clone()
            .map(|u| (u, me.webhook_secret.clone()))
    })
}

/// Runs an order-sending future DETACHED from the HTTP connection: if the client disconnects, the
/// orders, the journal writes, the strategy unwind and the signed webhook still complete (dropping
/// the handler future mid-send would leave SUBMITTING rows and skip the unwind).
async fn run_detached<F>(
    st: &AppState,
    hook: Option<(String, String)>,
    f: F,
) -> ApiResult<crate::execution::ExecutionReport>
where
    F: std::future::Future<Output = Result<crate::execution::ExecutionReport, ExecError>>
        + Send
        + 'static,
{
    let metrics = st.engine.metrics.clone();
    let handle = tokio::spawn(async move {
        let r = f.await;
        // Signed webhook to the integrator (fire-and-forget, retried; never delays the response).
        if let (Ok(report), Some((url, secret))) = (&r, hook)
            && let Ok(data) = serde_json::to_value(report)
        {
            crate::webhooks::deliver(url, secret, "execution.report", data, metrics);
        }
        r
    });
    match handle.await {
        Ok(r) => Ok(Json(r?)),
        Err(e) => {
            tracing::error!(error = %e, "execution task failed");
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                "execution task failed: check GET /v1/orders before retrying",
            ))
        }
    }
}

async fn execute_testnet(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(r): ApiJson<ExecuteRequest>,
) -> ApiResult<crate::execution::ExecutionReport> {
    charge_daily(&st, &i, r.route_id)?;
    let engine = st.engine.clone();
    run_detached(&st, webhook_of(&i), async move {
        engine.execute_testnet(r.route_id, &r.route_hash).await
    })
    .await
}

#[derive(serde::Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

/// Mainnet non-custodial: the journal holds END USERS' orders (wallets, signatures, tx hashes).
/// Nobody reads it through the API; each user lists their own with POST /v1/noncustodial/orders.
fn private_journal(st: &AppState) -> Result<(), ApiError> {
    if st.engine.mode == oa_core::domain::TradingMode::MainnetNonCustodial {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "PRIVATE",
            "the order journal is private in MAINNET_NONCUSTODIAL: list your own orders with \
             POST /v1/noncustodial/orders (wallet signature)",
        ));
    }
    Ok(())
}

async fn list_orders(
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Vec<crate::execution::OrderRow>> {
    private_journal(&st)?;
    Ok(Json(
        st.engine
            .orders(q.limit.unwrap_or(100).clamp(1, 1000))
            .await?,
    ))
}

async fn get_order(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<crate::execution::OrderRow> {
    private_journal(&st)?;
    Ok(Json(st.engine.order(parse_id(&id)?).await?))
}

async fn cancel_order(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<crate::execution::OrderRow> {
    let id = parse_id(&id)?;
    let exec = st.engine.execution().ok_or_else(|| {
        ApiError::from(ExecError::Disabled(
            "cancellation exists only in TESTNET".into(),
        ))
    })?;
    Ok(Json(exec.cancel(id).await?))
}

async fn exec_stats(State(st): State<AppState>) -> ApiResult<serde_json::Value> {
    let stats = st.engine.exec_stats().await?;
    Ok(Json(json!({
        "mode": st.engine.mode,
        "venues": stats,
        "execution_venues": st.engine.execution().map(|e| e.venue_identities()).unwrap_or_default(),
        "note": "TESTNET orders only. Slippage = (average fill - quoted average) / quoted average, per underlying unit; positive = worse for the buyer.",
    })))
}

async fn balances(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(st.engine.balances().await)
}

async fn execution_disabled() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "EXECUTION_DISABLED",
        "Direct order submission is disabled. Use POST /v1/routes/quote then \
         POST /v1/routes/execute-testnet (TESTNET mode only), or POST /v1/orders/simulate.",
    )
}

async fn positions_unsupported() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "UNSUPPORTED",
        "The server holds no account to report positions for. Non-custodial users: POST \
         /v1/noncustodial/derive/account with a wallet signature (Derive positions and balances).",
    )
}

/// Live dashboard snapshot: continuous "router vs Derive alone" comparison on a liquid basket.
async fn live_savings(
    State(st): State<AppState>,
) -> Result<Json<crate::live::LiveSnapshot>, ApiError> {
    match &st.live {
        Some(t) => Ok(Json(t.snapshot())),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "LIVE_TRACKER_OFF",
            "live tracker not started on this server",
        )),
    }
}

/// Multi-leg strategy quote: per-leg best venues + atomicity-aware plan.
async fn strategy_quote(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(mut r): ApiJson<crate::strategy::StrategyRequest>,
) -> ApiResult<crate::strategy::StrategyQuote> {
    enforce_limits(&i, &r.underlying, None)?;
    if let Some(axum::Extension(me)) = &i
        && r.single_venue_tolerance_bps.is_none()
    {
        r.single_venue_tolerance_bps = Some(me.settings.single_venue_tolerance_bps);
    }
    let q = crate::strategy::quote(&st.engine, r).await?;
    let ratio_sum: oa_core::Decimal = q.legs.iter().map(|l| l.quantity).sum();
    enforce_limits(
        &i,
        &q.request.underlying,
        q.index_price.map(|ix| ix * ratio_sum),
    )?;
    Ok(Json(q))
}

// ---------------------------------------------------------------- integrators

type MaybeIntegrator = Option<axum::Extension<crate::integrators::Integrator>>;

fn integrator_or_401(i: MaybeIntegrator) -> Result<crate::integrators::Integrator, ApiError> {
    i.map(|e| e.0).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "integrator API key required",
        )
    })
}

/// The calling integrator (settings, limits).
async fn integrator_me(i: MaybeIntegrator) -> ApiResult<crate::integrators::Integrator> {
    Ok(Json(integrator_or_401(i)?.redacted()))
}

/// Self-service settings (default limits ON; hard caps apply).
async fn integrator_update_settings(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(s): ApiJson<crate::integrators::IntegratorSettings>,
) -> ApiResult<crate::integrators::Integrator> {
    let me = integrator_or_401(i)?;
    st.integrators
        .update_settings(me.id, s, false)
        .await
        .map(|i| Json(i.redacted()))
        .map_err(integrator_error)
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIntegratorRequest {
    pub name: String,
    #[serde(default)]
    pub settings: Option<crate::integrators::IntegratorSettings>,
}

async fn admin_create_integrator(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<CreateIntegratorRequest>,
) -> ApiResult<crate::integrators::CreatedIntegrator> {
    st.integrators
        .create(&r.name, r.settings)
        .await
        .map(Json)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_REQUEST", e))
}

async fn admin_list_integrators(
    State(st): State<AppState>,
) -> ApiResult<Vec<crate::integrators::Integrator>> {
    Ok(Json(
        st.integrators
            .list()
            .into_iter()
            .map(|i| i.redacted())
            .collect(),
    ))
}

async fn admin_update_settings(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(s): ApiJson<crate::integrators::IntegratorSettings>,
) -> ApiResult<crate::integrators::Integrator> {
    st.integrators
        .update_settings(id, s, true)
        .await
        .map(|i| Json(i.redacted()))
        .map_err(integrator_error)
}

#[derive(serde::Deserialize)]
struct ActiveBody {
    active: bool,
}

async fn admin_set_active(
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(b): ApiJson<ActiveBody>,
) -> ApiResult<crate::integrators::Integrator> {
    st.integrators
        .set_active(id, b.active)
        .await
        .map(|i| Json(i.redacted()))
        .map_err(integrator_error)
}

/// Integrator store errors: unknown id 404, storage failure 503, anything else invalid input 422.
fn integrator_error(e: String) -> ApiError {
    if e == crate::integrators::UNKNOWN_INTEGRATOR {
        ApiError::new(StatusCode::NOT_FOUND, "NOT_FOUND", e)
    } else if e.starts_with(crate::integrators::PERSISTENCE_PREFIX) {
        tracing::error!(error = %e, "integrator persistence error");
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "PERSISTENCE_ERROR",
            "could not save the change; nothing was modified",
        )
    } else {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_REQUEST", e)
    }
}

/// Integrator limits on intents that can lead to execution (routes, strategies).
fn enforce_limits(
    i: &MaybeIntegrator,
    underlying: &str,
    notional: Option<oa_core::Decimal>,
) -> Result<(), ApiError> {
    let Some(axum::Extension(i)) = i else {
        return Ok(());
    };
    if !i.settings.allows_underlying(underlying) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "LIMIT_EXCEEDED",
            format!("underlying {underlying} not allowed by your settings"),
        ));
    }
    if let Some(n) = notional
        && n > i.settings.max_notional_per_order_usd
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "LIMIT_EXCEEDED",
            format!(
                "order notional {} USD above your max_notional_per_order_usd {}",
                n.round_dp(2),
                i.settings.max_notional_per_order_usd
            ),
        ));
    }
    Ok(())
}

/// Server-Sent Events: live best-venue quotes for one contract (shared topic per contract).
async fn stream_quotes(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiQuery(q): ApiQuery<crate::push::StreamQuery>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    enforce_limits(&i, &q.underlying, None)?;
    let sub = st
        .push
        .subscribe(&st.engine, q)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_REQUEST", e))?;
    st.engine.metrics.incr("push_subscriptions");
    use crate::push::PushMsg;
    // (subscription, done): after an error marked `last`, the stream ends.
    let stream = futures_util::stream::unfold((sub, false), |(mut sub, done)| async move {
        if done {
            return None;
        }
        loop {
            match sub.rx.recv().await {
                Ok(m) => {
                    return match &*m {
                        PushMsg::Quote(s) => Some((
                            Ok(Event::default().event("quote").data(s.as_str())),
                            (sub, false),
                        )),
                        PushMsg::Error { message, last } => {
                            let data = json!({"error": message, "closing": last}).to_string();
                            Some((Ok(Event::default().event("error").data(data)), (sub, *last)))
                        }
                    };
                }
                // A slow client skipped events: keep going with the latest.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}

/// Executable strategy route (single use, 30 s): legs with IOC limits and sides, all-or-nothing
/// with capped unwind. Execute with POST /v1/routes/execute-testnet.
async fn strategy_route(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(mut r): ApiJson<crate::strategy::StrategyRouteRequest>,
) -> ApiResult<crate::strategy::StrategyRoute> {
    enforce_limits(&i, &r.strategy.underlying, None)?;
    let mut allow_unbounded = false;
    if let Some(axum::Extension(me)) = &i {
        r.strategy
            .single_venue_tolerance_bps
            .get_or_insert(me.settings.single_venue_tolerance_bps);
        r.limit_tolerance_bps
            .get_or_insert(me.settings.max_slippage_bps);
        r.unwind_cost_cap_bps
            .get_or_insert(me.settings.unwind_cost_cap_bps);
        allow_unbounded = me.settings.allow_naked_sells;
    }
    let route = crate::strategy::route(&st.engine, r, allow_unbounded).await?;
    let ratio_sum: oa_core::Decimal = route.quote.legs.iter().map(|l| l.quantity).sum();
    let notional = route.quote.index_price.map(|ix| ix * ratio_sum);
    enforce_limits(&i, &route.quote.request.underlying, notional)?;
    if let Some(axum::Extension(me)) = &i {
        if let Some(p) = route
            .route
            .iter()
            .flat_map(|p| &p.legs)
            .find(|l| !me.settings.allows_platform(l.instrument.platform))
        {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "LIMIT_EXCEEDED",
                format!(
                    "venue {} not allowed by your settings (allowed_platforms)",
                    p.instrument.platform
                ),
            ));
        }
        // Unknown index price: charge the strike-weighted size so the daily cap still applies.
        let strikes: oa_core::Decimal = route
            .quote
            .legs
            .iter()
            .map(|l| l.quantity * l.request.strike)
            .sum();
        st.integrators
            .note_route(route.route_id, me.id, notional.unwrap_or(strikes));
    }
    Ok(Json(route))
}

const INTEGRATOR_GUIDE: &str = include_str!("../../../docs/integrators.md");

/// Developer docs: Redoc rendering of /openapi.json + link to the integrator guide.
async fn docs_page() -> axum::response::Html<&'static str> {
    axum::response::Html(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Omni Strike API</title><style>body{margin:0;font-family:system-ui,sans-serif}header{padding:14px 20px;background:#07090d;color:#e8ebf2}
header a{color:#3ef0a5;margin-left:16px;text-decoration:none}</style></head><body>
<header><b>&#9670; Omni Strike</b> &middot; API v1 <a href="/docs/integrators.md">Integrator guide</a><a href="/openapi.json">openapi.json</a></header>
<redoc spec-url="/openapi.json" hide-download-button></redoc>
<script src="https://cdn.jsdelivr.net/npm/redoc@2.1.5/bundles/redoc.standalone.js" integrity="sha384-0GrsyTQc9Oqd8h+b2dbc4XdR2T/DYpy0tLNNstyx+LBMUyiBbcWPbEs9aRmUcaxD" crossorigin="anonymous"></script></body></html>"#,
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareRequest {
    route_id: uuid::Uuid,
    route_hash: String,
    /// The user's accounts per platform. Derive: {"owner", "signer"?, "subaccount_id"};
    /// Aevo: {"maker", "signer" (registered signing key), "api_key"}.
    #[serde(default)]
    accounts: std::collections::BTreeMap<oa_core::domain::PlatformId, serde_json::Value>,
    /// Shorthand for a single-venue route: the account of the route's venue.
    #[serde(default)]
    account: Option<serde_json::Value>,
}

/// NON-CUSTODIAL execution, step 1: returns the EIP-712 payloads the end user signs.
async fn noncustodial_prepare(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(r): ApiJson<PrepareRequest>,
) -> ApiResult<crate::execution::PreparedExecution> {
    charge_daily(&st, &i, r.route_id)?;
    let mut accounts = r.accounts;
    if let Some(a) = r.account {
        // Single account: applies to the venue its fields identify (maker = Aevo, owner = Derive).
        let p = if a.get("maker").is_some() {
            oa_core::domain::PlatformId::Aevo
        } else {
            oa_core::domain::PlatformId::Derive
        };
        accounts.entry(p).or_insert(a);
    }
    Ok(Json(
        st.engine
            .prepare_user_signed(r.route_id, &r.route_hash, &accounts)
            .await?,
    ))
}

/// NON-CUSTODIAL execution, step 2: user signatures in, execution report out (+ webhook).
async fn noncustodial_submit(
    State(st): State<AppState>,
    i: MaybeIntegrator,
    ApiJson(r): ApiJson<crate::execution::SignedSubmission>,
) -> ApiResult<crate::execution::ExecutionReport> {
    let engine = st.engine.clone();
    run_detached(&st, webhook_of(&i), async move {
        engine.submit_user_signed(r).await
    })
    .await
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubaccountsRequest {
    /// The user's Derive wallet (owner).
    wallet: String,
    /// Current unix time in ms, signed with `personal_sign` by the wallet (accepted within 2 min).
    timestamp_ms: i64,
    signature: String,
}

/// READ-ONLY helper for non-custodial clients: the user's Derive subaccount ids, authenticated by the
/// user's own signature (checked here first, forwarded to Derive, never stored).
async fn noncustodial_derive_subaccounts(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<SubaccountsRequest>,
) -> ApiResult<serde_json::Value> {
    let (net, rest) = derive_net(&st)?;
    let ids =
        oa_adapters::derive_user::list_subaccounts(&rest, &r.wallet, r.timestamp_ms, &r.signature)
            .await
            .map_err(lookup_error)?;
    Ok(Json(json!({
        "wallet": r.wallet.to_lowercase(),
        "network": net.network,
        "chain_id": net.chain_id,
        "subaccount_ids": ids,
    })))
}

fn derive_net(st: &AppState) -> Result<(oa_adapters::derive_user::DeriveNet, String), ApiError> {
    st.engine
        .execution()
        .and_then(|e| e.derive_net())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "EXECUTION_DISABLED",
                "no user-signed Derive venue on this server",
            )
        })
}

fn lookup_error(e: oa_adapters::derive_user::LookupError) -> ApiError {
    use oa_adapters::derive_user::LookupError;
    match e {
        LookupError::Auth(m) => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_SIGNATURE", m)
        }
        LookupError::Venue(e) => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "UPSTREAM_UNAVAILABLE",
            e.to_string(),
        ),
    }
}

/// What a wallet signs (`personal_sign`) to read its own orders: bound to this service and
/// purpose, so it cannot be replayed as a venue login or elsewhere.
pub fn orders_message(wallet_lowercase: &str, timestamp_ms: i64) -> String {
    format!("Omni Strike: read my orders\nWallet: {wallet_lowercase}\nTimestamp: {timestamp_ms}")
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MyOrdersRequest {
    /// The user's wallet: the account declared at prepare time (Derive owner, Hyperliquid /
    /// Hypercall wallet, Aevo / Limitless maker, Thetanuts / CallPut address).
    wallet: String,
    /// Current unix time in ms (accepted within 2 min).
    timestamp_ms: i64,
    /// `personal_sign` by the wallet of the text built by `orders_message`:
    /// "Omni Strike: read my orders", "Wallet: <lowercased address>", "Timestamp: <timestamp_ms>"
    /// on three lines.
    signature: String,
    /// Default 100, max 1000.
    #[serde(default)]
    limit: Option<i64>,
}

/// The orders of ONE end user (non-custodial), after a local check of the wallet's signature.
/// Short-lived credentials are never journaled (Paradex JWT, Aevo HMAC, Derive REST auth).
async fn noncustodial_orders(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<MyOrdersRequest>,
) -> ApiResult<serde_json::Value> {
    let w = oa_adapters::derive_user::check_wallet_message(
        &r.wallet,
        r.timestamp_ms,
        &r.signature,
        |w| orders_message(w, r.timestamp_ms),
    )
    .map_err(|m| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "INVALID_SIGNATURE", m))?;
    let wallet = oa_adapters::derive_user::hex20(&w);
    let orders = st
        .engine
        .orders_by_owner(&wallet, r.limit.unwrap_or(100).clamp(1, 1000))
        .await?;
    Ok(Json(
        json!({"wallet": wallet, "mode": st.engine.mode, "orders": orders}),
    ))
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeriveAccountRequest {
    /// The user's Derive wallet (owner).
    wallet: String,
    /// Current unix time in ms, signed with `personal_sign` by the wallet (accepted within 2 min).
    timestamp_ms: i64,
    signature: String,
    subaccount_id: u64,
}

/// READ-ONLY: the user's Derive subaccount (collaterals, positions, open orders, value, margins) as
/// Derive returns it, with the user's own REST authentication. Nothing is stored.
async fn noncustodial_derive_account(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<DeriveAccountRequest>,
) -> ApiResult<serde_json::Value> {
    let (net, rest) = derive_net(&st)?;
    let account = oa_adapters::derive_user::get_subaccount(
        &rest,
        &r.wallet,
        r.timestamp_ms,
        &r.signature,
        r.subaccount_id,
    )
    .await
    .map_err(lookup_error)?;
    Ok(Json(json!({
        "wallet": r.wallet.to_lowercase(),
        "network": net.network,
        "chain_id": net.chain_id,
        "subaccount_id": r.subaccount_id,
        "account": account,
    })))
}

/// Binary contract (prediction markets) vs the dominating options spread at the same expiry.
async fn binaries_compare(
    State(st): State<AppState>,
    ApiJson(r): ApiJson<crate::binaries::BinaryCompareRequest>,
) -> ApiResult<crate::binaries::BinaryCompareResponse> {
    Ok(Json(crate::binaries::compare(&st.engine, r).await?))
}

#[cfg(test)]
mod middleware_tests {
    use super::*;

    /// Same text as the SDKs' ordersMessage / orders_message and the web UI.
    #[test]
    fn orders_message_format_is_stable() {
        assert_eq!(
            orders_message(
                "0xabcdef0000000000000000000000000000000001",
                1_791_000_000_000
            ),
            "Omni Strike: read my orders\nWallet: 0xabcdef0000000000000000000000000000000001\nTimestamp: 1791000000000"
        );
    }

    #[test]
    fn ip_limiter_bursts_then_refuses_and_exempts_loopback() {
        let l = IpLimiter::new(2);
        let ip: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        assert!((0..4).all(|_| l.allow(ip)), "burst = 2 x rps");
        assert!(!l.allow(ip));
        assert!(l.allow("198.51.100.1".parse().unwrap()), "per address");
        assert!((0..100).all(|_| l.allow("127.0.0.1".parse().unwrap())));
        assert!((0..100).all(|_| IpLimiter::new(0).allow(ip)), "0 = off");
    }
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn hit(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(axum::http::Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let b = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null),
        )
    }

    fn app(limit_ms: u64) -> Router {
        Router::new()
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    "late"
                }),
            )
            .route(
                "/boom",
                get(|| async {
                    if std::hint::black_box(true) {
                        panic!("handler bug");
                    }
                    "never"
                }),
            )
            .route(
                "/v1/routes/execute-testnet",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    "sent"
                }),
            )
            .layer(middleware::from_fn(move |r, n| {
                timeout_and_panic(std::time::Duration::from_millis(limit_ms), r, n)
            }))
    }

    #[tokio::test]
    async fn slow_request_answers_504_json() {
        let (s, v) = hit(app(100), "/slow").await;
        assert_eq!(s, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(v["error"]["code"], "TIMEOUT");
    }

    #[tokio::test]
    async fn handler_panic_answers_500_json() {
        let (s, v) = hit(app(1000), "/boom").await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(v["error"]["code"], "INTERNAL_ERROR");
    }

    #[tokio::test]
    async fn execution_paths_are_never_timed_out() {
        let resp = app(100)
            .oneshot(
                axum::http::Request::get("/v1/routes/execute-testnet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
