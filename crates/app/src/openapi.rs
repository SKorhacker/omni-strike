//! OpenAPI 3.0 document. Paths are declared here; component schemas are GENERATED from the
//! Rust types (schemars), so they cannot drift from what the API serialises.

use crate::engine::*;
use schemars::generate::SchemaSettings;
use serde_json::{Value, json};

fn body(schema: &Value) -> Value {
    json!({"required": true, "content": {"application/json": {"schema": schema}}})
}

fn ok(schema: &Value, desc: &str) -> Value {
    json!({"description": desc, "content": {"application/json": {"schema": schema}}})
}

pub fn document() -> Value {
    let mut g = SchemaSettings::openapi3().into_generator();
    let r = |s: schemars::Schema| s.to_value();
    let compare_req = r(g.subschema_for::<CompareRequest>());
    let compare_resp = r(g.subschema_for::<CompareResponse>());
    let search_resp = r(g.subschema_for::<SearchResponse>());
    let platform = r(g.subschema_for::<PlatformView>());
    let explain_req = r(g.subschema_for::<ExplainRequest>());
    let explain_resp = r(g.subschema_for::<ExplainResponse>());
    let sim_req = r(g.subschema_for::<SimulateRequest>());
    let preview_resp = r(g.subschema_for::<RoutePreview>());
    let sim_rec = r(g.subschema_for::<SimulationRecord>());
    let ready = r(g.subschema_for::<Readiness>());
    let metrics = r(g.subschema_for::<crate::metrics::Snapshot>());
    let route_req = r(g.subschema_for::<RouteQuoteRequest>());
    let route_quote = r(g.subschema_for::<RouteQuote>());
    let exec_req = r(g.subschema_for::<crate::api::ExecuteRequest>());
    let exec_report = r(g.subschema_for::<crate::execution::ExecutionReport>());
    let nc_prepare_req = r(g.subschema_for::<crate::api::PrepareRequest>());
    let nc_subaccounts_req = r(g.subschema_for::<crate::api::SubaccountsRequest>());
    let nc_orders_req = r(g.subschema_for::<crate::api::MyOrdersRequest>());
    let nc_account_req = r(g.subschema_for::<crate::api::DeriveAccountRequest>());
    let bin_req = r(g.subschema_for::<crate::binaries::BinaryCompareRequest>());
    let bin_resp = r(g.subschema_for::<crate::binaries::BinaryCompareResponse>());
    let nc_prepared = r(g.subschema_for::<crate::execution::PreparedExecution>());
    let nc_submit = r(g.subschema_for::<crate::execution::SignedSubmission>());
    let order = r(g.subschema_for::<crate::execution::OrderRow>());
    let stats = r(g.subschema_for::<crate::execution::VenueExecStats>());
    let strat_req = r(g.subschema_for::<crate::strategy::StrategyRequest>());
    let strat_quote = r(g.subschema_for::<crate::strategy::StrategyQuote>());
    let live = r(g.subschema_for::<crate::live::LiveSnapshot>());
    let integrator = r(g.subschema_for::<crate::integrators::Integrator>());
    let int_settings = r(g.subschema_for::<crate::integrators::IntegratorSettings>());
    let int_created = r(g.subschema_for::<crate::integrators::CreatedIntegrator>());
    let int_create = r(g.subschema_for::<crate::api::CreateIntegratorRequest>());
    let quote_event = r(g.subschema_for::<crate::push::QuoteEvent>());
    let schemas = g.take_definitions(true);

    let error = json!({"$ref": "#/components/schemas/Error"});
    let errors = json!({
        "401": {"description": "UNAUTHORIZED (only when OA_API_TOKEN is set)", "content": {"application/json": {"schema": error}}},
        "422": {"description": "INVALID_REQUEST", "content": {"application/json": {"schema": error}}},
        "503": {"description": "SERVICE_BUSY (in-flight or concurrent-comparison limit)", "content": {"application/json": {"schema": error}}},
    });
    let with_errors = |mut base: Value| {
        for (k, v) in errors.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    };
    let disabled = json!({"description": "EXECUTION_DISABLED: READ_ONLY + SIMULATION MVP", "content": {"application/json": {"schema": error}}});

    let mut components = serde_json::Map::from_iter(schemas);
    components.insert(
        "Error".into(),
        json!({"type": "object", "required": ["error"], "properties": {"error": {"type": "object",
            "required": ["code", "message"], "properties": {
                "code": {"type": "string", "enum": ["INVALID_REQUEST", "NOT_FOUND", "SERVICE_BUSY",
                    "PERSISTENCE_ERROR", "UNAUTHORIZED", "EXECUTION_DISABLED", "UNSUPPORTED",
                    "ROUTE_INVALID", "RISK_REJECTED", "RATE_LIMITED", "LIMIT_EXCEEDED"]},
                "message": {"type": "string"}}}}}),
    );

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Omni Strike API",
            "version": "v1",
            "description": "Best execution for crypto options across onchain venues, centralized exchanges and prediction \
                markets. Modes: READ_ONLY and SIMULATION (default build), TESTNET (`--features testnet`), \
                MAINNET_NONCUSTODIAL (`--features mainnet-nc`: the end user signs every order; the server holds no key). \
                Quotes are estimates from public order books, never a guaranteed price. Decimals are JSON strings; \
                times are UTC RFC 3339; quantities are in underlying units unless a field says `native`. Auth: none \
                on a loopback bind; otherwise `Authorization: Bearer <OA_API_TOKEN or integrator key>`. \
                Guide: /docs/integrators.md."
        },
        "paths": {
            "/health/live": {"get": {"summary": "Liveness", "responses": {"200": {"description": "live"}}}},
            "/health/ready": {"get": {"summary": "Readiness: database reachable and at least one catalog loaded",
                "responses": {"200": ok(&ready, "ready"), "503": ok(&ready, "not ready")}}},
            "/v1/platforms": {"get": {"summary": "All 12 platforms with verified status, capabilities and live collection health",
                "responses": {"200": ok(&json!({"type": "object", "properties": {"mode": {"type": "string"}, "platforms": {"type": "array", "items": platform}}}), "inventory")}}},
            "/v1/instruments": {"get": {"summary": "Search dated options (grouped by kind/strike/expiry across platforms)",
                "parameters": [
                    {"name": "underlying", "in": "query", "required": true, "schema": {"type": "string", "description": "BTC, ETH, SOL, XRP, HYPE (server config OA_UNDERLYINGS)"}},
                    {"name": "option_kind", "in": "query", "schema": {"type": "string", "enum": ["call", "put"]}},
                    {"name": "expiry", "in": "query", "schema": {"type": "string", "format": "date-time"}},
                    {"name": "offset", "in": "query", "schema": {"type": "integer", "minimum": 0}},
                    {"name": "limit", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 1000}}],
                "responses": with_errors(json!({"200": ok(&search_resp, "groups")}))}},
            "/v1/quotes/compare": {"post": {"summary": "Quantity-aware comparison: ranked / incomplete / ineligible / unavailable",
                "requestBody": body(&compare_req), "responses": with_errors(json!({"200": ok(&compare_resp, "comparison")}))}},
            "/v1/comparability/explain": {"post": {"summary": "Explain the comparability of two instruments",
                "requestBody": body(&explain_req), "responses": with_errors(json!({"200": ok(&explain_resp, "explanation"), "404": {"description": "NOT_FOUND"}}))}},
            "/v1/routes/preview": {"post": {"summary": "Single-destination route preview (nothing is sent)",
                "requestBody": body(&sim_req), "responses": with_errors(json!({"200": ok(&preview_resp, "preview")}))}},
            "/v1/orders/simulate": {"post": {"summary": "Simulate the previewed route; the record is persisted with all inputs",
                "requestBody": body(&sim_req), "responses": with_errors(json!({"200": ok(&sim_rec, "simulation record")}))}},
            "/v1/simulations/{id}": {"get": {"summary": "Stored simulation record",
                "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "responses": with_errors(json!({"200": ok(&sim_rec, "record"), "404": {"description": "NOT_FOUND"}}))}},
            "/v1/simulations/{id}/replay": {"post": {"summary": "Recompute a stored simulation from its recorded inputs only",
                "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "responses": with_errors(json!({"200": {"description": "stored vs recomputed, `identical` flag"}}))}},
            "/v1/metrics": {"get": {"summary": "Process-local timings and counters", "responses": {"200": ok(&metrics, "metrics")}}},
            "/v1/strategies/quote": {"post": {"summary": "Multi-leg strategy (spreads, straddles, condors, flies, custom; 1-6 legs, buy and sell): every leg priced on every venue, then the atomicity ladder picks ONE venue for all legs when within `single_venue_tolerance_bps` of the best split, else the best venue per leg. Net cost: positive = debit, negative = credit",
                "requestBody": body(&strat_req), "responses": with_errors(json!({"200": ok(&strat_quote, "strategy quote")}))}},
            "/v1/stream/quotes": {"get": {"summary": "Server-Sent Events: live best-venue quote for one contract (event `quote`, JSON QuoteEvent). Query: underlying, option_kind, strike, expiry, side, quantity, family?, interval_ms? (>= 250, default 1000). One shared computation per distinct contract",
                "responses": with_errors(json!({"200": {"description": "text/event-stream of QuoteEvent", "content": {"text/event-stream": {"schema": quote_event}}}}))}},
            "/v1/integrator/me": {"get": {"summary": "The calling integrator (API key): settings and limits", "responses": with_errors(json!({"200": ok(&integrator, "integrator")}))}},
            "/v1/integrator/settings": {"put": {"summary": "Self-service settings (default limits ON; hard caps apply; beyond them the operator sets limits through the admin API)", "requestBody": body(&int_settings), "responses": with_errors(json!({"200": ok(&integrator, "updated integrator")}))}},
            "/v1/admin/integrators": {"get": {"summary": "ADMIN (OA_ADMIN_TOKEN): list integrators (no key material)", "responses": with_errors(json!({"200": ok(&json!({"type": "array", "items": integrator}), "integrators")}))},
                "post": {"summary": "ADMIN: create an integrator; the API key is returned ONCE", "requestBody": body(&int_create), "responses": with_errors(json!({"200": ok(&int_created, "created")}))}},
            "/v1/admin/integrators/{id}/settings": {"put": {"summary": "ADMIN: set settings beyond self-service caps", "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}], "requestBody": body(&int_settings), "responses": with_errors(json!({"200": ok(&integrator, "updated")}))}},
            "/v1/admin/integrators/{id}/active": {"put": {"summary": "ADMIN: activate / deactivate an API key", "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}], "responses": with_errors(json!({"200": ok(&integrator, "updated")}))}},
            "/v1/live/savings": {"get": {"summary": "Live dashboard feed: continuous best-venue vs Derive-alone comparison on a liquid basket", "responses": {"200": ok(&live, "live snapshot"), "404": {"description": "LIVE_TRACKER_OFF"}}}},
            "/v1/routes/quote": {"post": {"summary": "Executable routing for a BUY intent: single venue or opt-in split (2-3 venues), IOC limits per leg. Returns a single-use route_id + route_hash valid 30 s",
                "requestBody": body(&route_req), "responses": with_errors(json!({"200": ok(&route_quote, "route quote")}))}},
            "/v1/routes/execute-testnet": {"post": {"summary": "TESTNET ONLY: execute a route quote (single use, hash-bound, risk-checked, kill switch). 403 in any other mode",
                "requestBody": body(&exec_req), "responses": with_errors(json!({"200": ok(&exec_report, "execution report"),
                    "403": disabled, "409": {"description": "ROUTE_INVALID (unknown, expired, already used, hash mismatch, not executable)"},
                    "422": {"description": "RISK_REJECTED or INVALID_REQUEST"}}))}},
            "/v1/binaries/compare": {"post": {"summary": "Binary contract (prediction markets: 'BTC above K at T') vs the cheapest options vertical spread that pays AT LEAST the same in every scenario (super-replication, same expiry). Returns both, and which is cheaper",
                "requestBody": body(&bin_req), "responses": with_errors(json!({"200": ok(&bin_resp, "binary vs options")}))}},
            "/v1/balances": {"get": {"summary": "Balances of the server's own execution accounts (TESTNET; empty in other modes; non-custodial venues omitted: the balance is the user's)",
                "responses": {"200": {"description": "balances per platform", "content": {"application/json": {"schema": {"type": "object"}}}}}}},
            "/v1/noncustodial/prepare": {"post": {"summary": "NON-CUSTODIAL execution step 1 (Derive): consumes a route quote and returns, per leg, the EIP-712 typed data the END USER signs (eth_signTypedData_v4). Omni Strike holds no key. Valid 5 min",
                "requestBody": body(&nc_prepare_req), "responses": with_errors(json!({"200": ok(&nc_prepared, "payloads to sign"), "403": disabled,
                    "409": {"description": "ROUTE_INVALID"}, "422": {"description": "RISK_REJECTED or INVALID_REQUEST"}}))}},
            "/v1/noncustodial/submit": {"post": {"summary": "NON-CUSTODIAL execution step 2: the user's signatures (one per leg + personal_sign of a ms timestamp). Signatures are verified against the declared signer BEFORE anything is sent; single use. Incomplete strategies are reported UNWIND_REQUIRES_USER_SIGNATURE (no automatic unwind without the user)",
                "requestBody": body(&nc_submit), "responses": with_errors(json!({"200": ok(&exec_report, "execution report"), "404": {"description": "NOT_FOUND (unknown, already submitted or expired)"}}))}},
            "/v1/noncustodial/derive/subaccounts": {"post": {"summary": "READ-ONLY helper: the user's Derive subaccount ids. Auth = the wallet's personal_sign of a fresh ms timestamp (checked locally, forwarded to Derive, never stored)",
                "requestBody": body(&nc_subaccounts_req), "responses": with_errors(json!({"200": {"description": "{wallet, network, chain_id, subaccount_ids}", "content": {"application/json": {"schema": {"type": "object"}}}},
                    "403": disabled, "422": {"description": "INVALID_SIGNATURE"}, "502": {"description": "UPSTREAM_UNAVAILABLE"}}))}},
            "/v1/noncustodial/orders": {"post": {"summary": "The orders of ONE end user, newest first. Auth = the wallet's personal_sign of a fresh ms timestamp (checked locally). Short-lived credentials are never journaled",
                "requestBody": body(&nc_orders_req), "responses": with_errors(json!({"200": ok(&json!({"type": "object", "properties": {"wallet": {"type": "string"}, "orders": {"type": "array", "items": order}}}), "{wallet, mode, orders}"),
                    "422": {"description": "INVALID_SIGNATURE"}}))}},
            "/v1/noncustodial/derive/account": {"post": {"summary": "READ-ONLY: the user's Derive subaccount (collaterals, positions, open orders, value, margins) as Derive returns it. Auth = the wallet's personal_sign of a fresh ms timestamp (checked locally, forwarded to Derive, never stored)",
                "requestBody": body(&nc_account_req), "responses": with_errors(json!({"200": {"description": "{wallet, network, chain_id, subaccount_id, account}", "content": {"application/json": {"schema": {"type": "object"}}}},
                    "403": disabled, "422": {"description": "INVALID_SIGNATURE"}, "502": {"description": "UPSTREAM_UNAVAILABLE"}}))}},
            "/v1/orders": {"get": {"summary": "Order journal (newest first). TESTNET only: 403 PRIVATE in MAINNET_NONCUSTODIAL (use /v1/noncustodial/orders)", "parameters": [{"name": "limit", "in": "query", "schema": {"type": "integer", "minimum": 1, "maximum": 1000}}],
                "responses": {"200": ok(&json!({"type": "array", "items": order}), "orders")}},
                "post": {"summary": "DISABLED: direct submission; use /v1/routes/quote + /v1/routes/execute-testnet", "responses": {"403": disabled}}},
            "/v1/orders/prepare": {"post": {"summary": "DISABLED", "responses": {"403": disabled}}},
            "/v1/orders/{id}": {"get": {"summary": "TESTNET order: state, fills, quote vs fill, reconciliation", "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "responses": with_errors(json!({"200": ok(&order, "order"), "404": {"description": "NOT_FOUND"}}))}},
            "/v1/orders/{id}/cancel": {"post": {"summary": "TESTNET: cancel a non-final order (IOC orders are normally already final)", "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"}}],
                "responses": with_errors(json!({"200": ok(&order, "order after cancel"), "403": disabled, "404": {"description": "NOT_FOUND"}}))}},
            "/v1/exec-stats": {"get": {"summary": "Execution statistics per venue (TESTNET orders)", "responses": {"200": ok(&json!({"type": "object", "properties": {"venues": {"type": "array", "items": stats}}}), "stats")}}},
            "/v1/positions": {"get": {"summary": "UNSUPPORTED on the server's accounts; a non-custodial user reads their Derive positions with POST /v1/noncustodial/derive/account", "responses": {"501": {"description": "UNSUPPORTED"}}}}
        },
        "components": {"schemas": Value::Object(components)}
    })
}
