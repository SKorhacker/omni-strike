//! TESTNET execution of a route quote, persistence-first.
//!
//! Crash windows (documented in docs/testnet.md):
//! 1. intent persisted (SUBMITTING, with salt, timestamp and the PRECOMPUTED venue order id)
//! 2. request sent
//! 3. acknowledgement received
//! 4. result persisted
//!
//! A crash/timeout between 2 and 4 leaves SUBMITTING/UNKNOWN in the journal. Recovery NEVER
//! resubmits: the OrderTracker looks the order up by its precomputed id (Aevo's order id is the
//! hash of the signed payload — hypothesis until confirmed on the first testnet order) and by the
//! id returned when known, and updates the journal.
//!
//! MAINNET_NONCUSTODIAL: same journal and gates, but only user-signed venues are accepted
//! (`identity().custody == "NON_CUSTODIAL"`, enforced in `new`) and the custodial `execute` path is
//! refused: every mainnet order is signed (or sent) by the end user.
//!
//! Gates before any submission: mode == TESTNET or MAINNET_NONCUSTODIAL, kill switch OFF
//! (OA_KILL_SWITCH=false in the env file), route quote single-use + hash-bound + unexpired, every leg on an execution
//! venue whose network equals the instrument's network, `risk::check(for_submission = true)`.

use chrono::{DateTime, Utc};
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::mailbox;
use kameo::message::{Context, Message};
use oa_adapters::ConnectorError;
use oa_adapters::exec::{ExecOrder, ExecutionVenue, TimeInForce, VenueOrderState, VenueStatus};
use oa_core::Decimal;
use oa_core::domain::{PlatformId, Side, TradingMode};
use oa_core::fees::FeeAmount;
use oa_core::risk::{self, OrderIntent, RiskLimits, SessionUsage};
use oa_core::routing::RoutePlan;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderState {
    Created,
    Validated,
    Submitting,
    Acknowledged,
    PartiallyFilled,
    Filled,
    CancelPending,
    Cancelled,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OrderRow {
    pub id: Uuid,
    pub route_id: Uuid,
    pub leg_index: i32,
    pub mode: TradingMode,
    pub platform: PlatformId,
    pub network: String,
    pub native_id: String,
    pub side: Side,
    pub qty_native: Decimal,
    pub limit_price_native: Decimal,
    pub contract_multiplier: Decimal,
    pub salt: u64,
    pub timestamp: i64,
    pub venue_instrument_id: Option<u64>,
    #[schemars(with = "serde_json::Value")]
    #[serde(default)]
    pub venue_meta: serde_json::Value,
    /// Max fee per unit of volume signed into the order (Derive).
    #[serde(default)]
    pub max_fee_per_unit: Option<Decimal>,
    /// Quote used for this leg.
    pub expected_avg_per_underlying: Decimal,
    pub expected_total: Decimal,
    pub expected_fee: Option<Decimal>,
    pub precomputed_venue_id: Option<String>,
    pub venue_order_id: Option<String>,
    pub state: OrderState,
    pub is_final: bool,
    pub filled_native: Decimal,
    pub avg_fill_price_native: Option<Decimal>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub acked_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub submit_to_ack_ms: Option<i64>,
    pub reconcile_attempts: u32,
    #[schemars(with = "Option<serde_json::Value>")]
    pub last_venue_response: Option<serde_json::Value>,
}

impl OrderRow {
    /// The end user who owns this order (non-custodial): the account declared at prepare time,
    /// lowercased. None for orders on the server's own (custodial testnet) accounts.
    pub fn owner(&self) -> Option<String> {
        let u = self.venue_meta.get("user")?;
        ["owner", "wallet", "maker", "address", "account"]
            .iter()
            .find_map(|k| u.get(*k).and_then(|v| v.as_str()))
            .map(str::to_lowercase)
    }

    /// (fill - expected) / expected in bps, per underlying; positive = worse for a buyer.
    pub fn slippage_bps(&self) -> Option<Decimal> {
        let fill = self.avg_fill_price_native? / self.contract_multiplier;
        if self.expected_avg_per_underlying.is_zero() || self.filled_native.is_zero() {
            return None;
        }
        Some(
            ((fill - self.expected_avg_per_underlying) / self.expected_avg_per_underlying
                * Decimal::from(10_000))
            .round_dp(2),
        )
    }

    fn exec_order(&self) -> ExecOrder {
        ExecOrder {
            native_id: self.native_id.clone(),
            venue_instrument_id: self.venue_instrument_id,
            venue_meta: self.venue_meta.clone(),
            max_fee_per_unit: self.max_fee_per_unit,
            side: self.side,
            qty_native: self.qty_native,
            limit_price_native: self.limit_price_native,
            salt: self.salt,
            timestamp: self.timestamp,
            time_in_force: TimeInForce::Ioc,
        }
    }

    fn apply(&mut self, s: &VenueOrderState) {
        self.venue_order_id = Some(s.venue_order_id.clone());
        self.filled_native = s.filled_native;
        self.avg_fill_price_native = s.avg_price_native;
        self.last_venue_response = Some(s.raw.clone());
        let state = match &s.status {
            VenueStatus::Filled => OrderState::Filled,
            VenueStatus::PartiallyFilled => OrderState::PartiallyFilled,
            VenueStatus::Cancelled => OrderState::Cancelled,
            VenueStatus::Rejected => OrderState::Rejected,
            VenueStatus::Open => OrderState::Acknowledged,
            VenueStatus::Unknown(x) => {
                self.last_error = Some(format!("unknown venue status {x:?}"));
                OrderState::Unknown
            }
        };
        self.state = state;
        // Finality comes from the venue adapter, never inferred from a status name here.
        self.is_final = s.is_final && state != OrderState::Unknown;
        self.updated_at = Utc::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StoredRoute {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub route_hash: String,
    pub mode: TradingMode,
    pub plan: RoutePlan,
    /// native_id -> (venue numeric instrument id, contract multiplier)
    pub instruments: BTreeMap<String, InstrumentRef>,
    pub executable: bool,
    pub not_executable_reasons: Vec<String>,
    /// Present for multi-leg strategies: all-or-nothing execution with capped unwind.
    #[serde(default)]
    pub strategy: Option<StrategyRisk>,
}

/// Risk frame of a multi-leg strategy, frozen with its route.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StrategyRisk {
    /// Worst loss at expiry (premium currency); None = unbounded (net short calls).
    pub max_loss: Option<Decimal>,
    pub allow_unbounded: bool,
    /// Index price at quote time (to size the unwind cost cap in notional terms).
    pub index_price: Option<Decimal>,
    /// Max acceptable unwind cost, bps of the strategy notional.
    pub unwind_cost_cap_bps: u32,
}

/// What execution needs to know about an instrument, frozen at quote time.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InstrumentRef {
    pub venue_instrument_id: Option<u64>,
    pub contract_multiplier: Decimal,
    /// Venue signing metadata (Derive: base_asset_address, base_asset_sub_id).
    #[schemars(with = "serde_json::Value")]
    pub venue_meta: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum ConsumeOutcome {
    Consumed(Box<StoredRoute>),
    NotFound,
    HashMismatch,
    Expired,
    AlreadyConsumed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionReport {
    pub route_id: Uuid,
    pub mode: TradingMode,
    pub orders: Vec<OrderRow>,
    pub requested_underlying: Decimal,
    pub filled_underlying: Decimal,
    pub expected_total: Decimal,
    pub notes: Vec<String>,
    /// Strategy routes: unwind orders sent because a leg failed (all-or-nothing policy).
    #[serde(default)]
    pub unwind_orders: Vec<OrderRow>,
    /// Strategy routes: COMPLETE (every leg filled), UNWOUND, UNWIND_INCOMPLETE or PENDING.
    #[serde(default)]
    pub strategy_outcome: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("execution disabled: {0}")]
    Disabled(String),
    #[error("route quote: {0}")]
    Route(String),
    #[error("refused by risk checks: {0:?}")]
    Risk(Vec<risk::RiskViolation>),
    #[error("persistence: {0}")]
    Persistence(String),
    #[error("not found: {0}")]
    NotFound(String),
}

fn is_definite_rejection(e: &ConnectorError) -> bool {
    // 4xx (except 404/408/409/429) = the venue understood and refused: nothing was created.
    matches!(e, ConnectorError::Http { status, .. } if (400..500).contains(status) && ![404, 408, 409, 429].contains(status))
        || matches!(e, ConnectorError::InvalidResponse { detail, .. } if detail.starts_with("order not representable"))
}

pub struct ExecutionService {
    pub mode: TradingMode,
    venues: BTreeMap<PlatformId, Arc<dyn ExecutionVenue>>,
    store: Arc<dyn Store>,
    kill_switch: bool,
    /// Serializes the "read committed spend -> validate -> journal" sequence of this process, so two
    /// concurrent requests cannot both fit under the same remaining cap.
    session: tokio::sync::Mutex<()>,
    /// Rolling window of the session caps (OA_SESSION_WINDOW_HOURS, default 24 h).
    pub session_window: chrono::Duration,
    tracker: ActorRef<OrderTracker>,
    pub submit_timeout: Duration,
}

/// A non-custodial execution waiting for the user's signatures, kept in the database
/// (`prepared_executions`): survives a restart, shared by every instance, taken once.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedState {
    route: StoredRoute,
    rows: Vec<OrderRow>,
    accounts: BTreeMap<PlatformId, serde_json::Value>,
}

/// Session window from OA_SESSION_WINDOW_HOURS (1..=720), default 24 h.
pub fn session_window_from_env() -> chrono::Duration {
    let h = std::env::var("OA_SESSION_WINDOW_HOURS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|h| (1..=720).contains(h))
        .unwrap_or(24);
    chrono::Duration::hours(h)
}

/// One order the end user must sign (non-custodial execution).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreparedLeg {
    pub order_id: Uuid,
    pub leg_index: i32,
    pub platform: PlatformId,
    pub native_id: String,
    pub side: Side,
    pub qty_native: Decimal,
    pub limit_price_native: Decimal,
    pub max_fee_per_unit: Option<Decimal>,
    /// EIP-712 payload for `eth_signTypedData_v4` (sign with the declared `signer`).
    #[schemars(with = "serde_json::Value")]
    pub typed_data: serde_json::Value,
    /// keccak digest of `typed_data` (what the signature must cover).
    pub digest: String,
    pub signature_expires_at: DateTime<Utc>,
    /// Aevo: exact POST /orders body with `__SIGNATURE__`; substitute the signature, then compute
    /// `venue_auth.hmac` = hex(HMAC_SHA256(api_secret, "api_key,timestamp_ns,POST,/orders,<body>")).
    /// Limitless: same idea (base64 HMAC). Hyperliquid: `{"action", "nonce"}` whose msgpack hash is
    /// the signed connectionId (informative, nothing to substitute).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_template: Option<String>,
    /// Transaction venues (Thetanuts): the unsigned transaction the user's wallet sends
    /// ({chain_id, to, data, value, approval}); then submit its `tx_hash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<serde_json::Value>")]
    pub transaction: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreparedExecution {
    pub route_id: Uuid,
    /// Accounts used, per platform.
    #[schemars(with = "serde_json::Value")]
    pub accounts: BTreeMap<PlatformId, serde_json::Value>,
    pub legs: Vec<PreparedLeg>,
    /// How to produce `auth_signature` for the submit call.
    pub auth_instructions: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LegSignature {
    pub order_id: Uuid,
    /// 65-byte hex signature of the leg's `typed_data` by the declared signer (signed venues).
    #[serde(default)]
    pub signature: Option<String>,
    /// Transaction venues: hash of the transaction the user's wallet sent.
    #[serde(default)]
    pub tx_hash: Option<String>,
    /// Venue-specific per-order authentication (Aevo: {"timestamp_ns", "hmac"}).
    #[serde(default)]
    #[schemars(with = "Option<serde_json::Value>")]
    pub venue_auth: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SignedSubmission {
    pub route_id: Uuid,
    /// Derive legs: current unix time in ms, signed with `personal_sign` by the signer.
    #[serde(default)]
    pub auth_timestamp_ms: Option<i64>,
    #[serde(default)]
    pub auth_signature: Option<String>,
    pub signatures: Vec<LegSignature>,
}

impl ExecutionService {
    pub fn new(
        mode: TradingMode,
        venues: Vec<Arc<dyn ExecutionVenue>>,
        store: Arc<dyn Store>,
        kill_switch: bool,
    ) -> Arc<Self> {
        let venues: BTreeMap<_, _> = venues
            .into_iter()
            .filter(|v| {
                let nc = v.identity()["custody"].as_str() == Some("NON_CUSTODIAL");
                if mode == TradingMode::MainnetNonCustodial && !nc {
                    tracing::error!(platform = %v.platform(), "custodial venue REFUSED in MAINNET_NONCUSTODIAL");
                }
                mode != TradingMode::MainnetNonCustodial || nc
            })
            .map(|v| (v.platform(), v))
            .collect();
        let tracker = OrderTracker::spawn_with_mailbox(
            OrderTracker {
                venues: venues.clone(),
                store: store.clone(),
            },
            mailbox::bounded(64),
        );
        Arc::new(Self {
            mode,
            venues,
            store,
            kill_switch,
            session: tokio::sync::Mutex::new(()),
            session_window: session_window_from_env(),
            tracker,
            submit_timeout: Duration::from_secs(10),
        })
    }

    pub fn venue_identities(&self) -> Vec<serde_json::Value> {
        self.venues.values().map(|v| v.identity()).collect()
    }

    /// Deployment of the attached user-signed Derive venue (testnet or mainnet) and the REST host it
    /// talks to, if any.
    pub fn derive_net(&self) -> Option<(oa_adapters::derive_user::DeriveNet, String)> {
        let id = self
            .venues
            .get(&PlatformId::Derive)
            .map(|v| v.identity())
            .filter(|i| i["custody"].as_str() == Some("NON_CUSTODIAL"))?;
        let net = id["chain_id"]
            .as_u64()
            .and_then(oa_adapters::derive_user::DeriveNet::by_chain_id)?;
        let rest = id["rest"].as_str().unwrap_or(net.rest).to_string();
        Some((net, rest))
    }

    pub fn executable_network(&self, p: PlatformId) -> Option<&'static str> {
        self.venues.get(&p).map(|v| v.network())
    }

    pub fn kill_switch(&self) -> bool {
        self.kill_switch
    }

    /// Real balances of every execution venue (errors kept per venue, never turned into 0).
    pub async fn balances(
        &self,
    ) -> BTreeMap<PlatformId, Result<oa_adapters::exec::VenueBalance, String>> {
        // All venues at once, each bounded: one slow venue API no longer adds its latency to every
        // route quote (they were asked one after another).
        let asks = self
            .venues
            .iter()
            // Non-custodial venues: the balance is the END USER's, checked by the venue at submit.
            .filter(|(_, v)| v.identity()["custody"].as_str() != Some("NON_CUSTODIAL"))
            .map(|(p, v)| async move {
                let r = match tokio::time::timeout(Duration::from_secs(3), v.balance()).await {
                    Ok(r) => r.map_err(|e| e.to_string()),
                    Err(_) => Err("balance: no answer within 3 s".to_string()),
                };
                (*p, r)
            });
        futures_util::future::join_all(asks)
            .await
            .into_iter()
            .collect()
    }

    /// Start periodic reconciliation and pick up non-final orders left by a previous run.
    pub fn start_tracker(self: &Arc<Self>) {
        let t = self.tracker.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(10));
            loop {
                iv.tick().await;
                if t.tell(ReconcileTick).send().await.is_err() {
                    break;
                }
            }
        });
    }

    pub async fn execute(
        &self,
        route_id: Uuid,
        route_hash: &str,
        limits: &RiskLimits,
    ) -> Result<ExecutionReport, ExecError> {
        if self.mode == TradingMode::MainnetNonCustodial {
            return Err(ExecError::Disabled(
                "custodial execution does not exist on mainnet: use POST /v1/noncustodial/prepare \
                 (the end user signs every order)"
                    .into(),
            ));
        }
        let route = self.take_route(route_id, route_hash).await?;
        let rows = self.build_rows(&route, limits, None).await?;
        self.send_rows(route, rows, true).await
    }

    /// NON-CUSTODIAL step 1: validate the route and return what the end user must sign. Omni Strike
    /// holds no key; rows are journaled as VALIDATED and expire if never signed.
    pub async fn prepare_user_signed(
        &self,
        route_id: Uuid,
        route_hash: &str,
        accounts: &BTreeMap<PlatformId, serde_json::Value>,
        limits: &RiskLimits,
    ) -> Result<PreparedExecution, ExecError> {
        use oa_adapters::derive_user::SIGNATURE_TTL_SECS;
        // Validate accounts BEFORE consuming the route.
        let mut normalized = BTreeMap::new();
        for (p, a) in accounts {
            let n = match p {
                PlatformId::Derive => {
                    oa_adapters::derive_user::UserAccount::from_json(a).map(|x| x.to_json())
                }
                PlatformId::Aevo => {
                    oa_adapters::aevo_user::AevoAccount::from_json(a).map(|x| x.to_json())
                }
                PlatformId::Hyperliquid => oa_adapters::hyperliquid_user::account(a),
                PlatformId::Limitless => oa_adapters::limitless_user::LimitlessAccount::from_json(a).map(|x| x.to_json()),
                PlatformId::Hypercall => oa_adapters::hypercall_user::account(a).map(|(w, s)| {
                    serde_json::json!({"wallet": oa_adapters::derive_user::hex20(&w), "signer": oa_adapters::derive_user::hex20(&s)})
                }),
                PlatformId::Paradex => a["account"]
                    .as_str()
                    .filter(|x| {
                        x.starts_with("0x")
                            && x.len() <= 66
                            && x[2..].bytes().all(|b| b.is_ascii_hexdigit())
                    })
                    .map(|x| serde_json::json!({"account": x.to_lowercase()}))
                    .ok_or_else(|| {
                        "account: the user's Paradex (Starknet) account address".to_string()
                    }),
                PlatformId::Thetanuts | PlatformId::Callput => a["address"]
                    .as_str()
                    .and_then(oa_adapters::aevo_exec::parse_address)
                    .map(|x| serde_json::json!({"address": format!("0x{}", hex::encode(x))}))
                    .ok_or_else(|| {
                        "address: the wallet that will send the transaction".to_string()
                    }),
                other => Err(format!(
                    "non-custodial execution is not available on {other} yet"
                )),
            }
            .map_err(|e| ExecError::Route(format!("{p} account: {e}")))?;
            normalized.insert(*p, n);
        }
        let route = self.take_route(route_id, route_hash).await?;
        for l in &route.plan.legs {
            let p = l.instrument.platform;
            let nc = self
                .venues
                .get(&p)
                .is_some_and(|v| v.identity()["custody"].as_str() == Some("NON_CUSTODIAL"));
            if !nc {
                return Err(ExecError::Route(format!(
                    "{p}: non-custodial execution is not enabled on this server"
                )));
            }
            if !normalized.contains_key(&p) {
                return Err(ExecError::Route(format!(
                    "missing the user's {p} account (accounts.{p})"
                )));
            }
        }
        let mut rows = self.build_rows(&route, limits, Some(&normalized)).await?;
        // CallPut: the contract only accepts multi-leg requests (2-leg debit spreads), so its legs
        // share ONE transaction built from all of them.
        let callput: Vec<ExecOrder> = rows
            .iter()
            .filter(|r| r.platform == PlatformId::Callput)
            .map(|r| r.exec_order())
            .collect();
        let callput_bundle = if callput.is_empty() {
            None
        } else {
            let venue = self
                .venues
                .get(&PlatformId::Callput)
                .expect("checked above");
            Some(
                venue
                    .prepare_user_tx_bundle(&callput)
                    .await
                    .map_err(|e| ExecError::Route(e.to_string()))?
                    .ok_or_else(|| {
                        ExecError::Route("callput: venue built no transaction".into())
                    })?,
            )
        };
        let mut legs = Vec::with_capacity(rows.len());
        for r in &mut rows {
            let acct = &normalized[&r.platform];
            let o = r.exec_order();
            let err = |e: oa_adapters::ConnectorError| ExecError::Route(e.to_string());
            let mut transaction = None;
            // Signing domain of the ATTACHED venue (testnet or mainnet); never a default.
            let chain_id = self
                .venues
                .get(&r.platform)
                .and_then(|v| v.identity()["chain_id"].as_u64());
            let (typed_data, digest, expiry, body_template) = match r.platform {
                PlatformId::Derive => {
                    let a = oa_adapters::derive_user::UserAccount::from_json(acct)
                        .map_err(ExecError::Route)?;
                    let net = chain_id
                        .and_then(oa_adapters::derive_user::DeriveNet::by_chain_id)
                        .ok_or_else(|| {
                            ExecError::Route("Derive venue has no known chain id".into())
                        })?;
                    let p = oa_adapters::derive_user::prepare_order(&o, &a, net).map_err(err)?;
                    (p.typed_data, p.digest, p.expiry as i64, None)
                }
                PlatformId::Aevo => {
                    let a = oa_adapters::aevo_user::AevoAccount::from_json(acct)
                        .map_err(ExecError::Route)?;
                    let net = chain_id
                        .and_then(oa_adapters::aevo_user::AevoNet::by_chain_id)
                        .ok_or_else(|| {
                            ExecError::Route("Aevo venue has no known chain id".into())
                        })?;
                    let p = oa_adapters::aevo_user::prepare_order(&o, &a, net).map_err(err)?;
                    (
                        p.typed_data,
                        p.digest,
                        o.timestamp + SIGNATURE_TTL_SECS,
                        Some(p.body_template),
                    )
                }
                PlatformId::Limitless => {
                    let a = oa_adapters::limitless_user::LimitlessAccount::from_json(acct)
                        .map_err(ExecError::Route)?;
                    let p = oa_adapters::limitless_user::prepare_order(&o, &a).map_err(err)?;
                    (
                        p.typed_data,
                        p.digest,
                        o.timestamp + SIGNATURE_TTL_SECS,
                        Some(p.body_template),
                    )
                }
                PlatformId::Hyperliquid => {
                    let w = oa_adapters::hyperliquid_user::order_wire(&o).map_err(err)?;
                    let h = oa_adapters::hyperliquid_user::action_hash(
                        &w,
                        oa_adapters::hyperliquid_user::nonce(&o),
                    );
                    // The signed connectionId is an opaque hash: return the action + nonce so the
                    // client can recompute it before signing.
                    let action = serde_json::json!({
                        "action": oa_adapters::hyperliquid_user::action_json(&w),
                        "nonce": oa_adapters::hyperliquid_user::nonce(&o),
                    });
                    (
                        oa_adapters::hyperliquid_user::typed_data(&h),
                        oa_adapters::hyperliquid_user::agent_digest(&h),
                        o.timestamp + SIGNATURE_TTL_SECS,
                        Some(action.to_string()),
                    )
                }
                PlatformId::Hypercall => {
                    let (w, _) =
                        oa_adapters::hypercall_user::account(acct).map_err(ExecError::Route)?;
                    let p = oa_adapters::hypercall_user::place_order(&o, w).map_err(err)?;
                    (
                        oa_adapters::hypercall_user::typed_data(&p),
                        oa_adapters::hypercall_user::digest(&p),
                        o.timestamp + SIGNATURE_TTL_SECS,
                        None,
                    )
                }
                PlatformId::Paradex => {
                    let chain = self
                        .venues
                        .get(&r.platform)
                        .and_then(|v| v.identity()["chain_id"].as_str().map(str::to_string))
                        .ok_or_else(|| ExecError::Route("Paradex venue has no chain id".into()))?;
                    let td =
                        oa_adapters::paradex_user::order_typed_data(&o, &chain).map_err(err)?;
                    (td, [0u8; 32], o.timestamp + SIGNATURE_TTL_SECS, None)
                }
                PlatformId::Thetanuts | PlatformId::Callput => {
                    // The user's wallet sends this transaction; we only verify the receipt.
                    let venue = self.venues.get(&r.platform).expect("checked above");
                    let tx = match (&callput_bundle, r.platform) {
                        (Some(b), PlatformId::Callput) => b.clone(),
                        _ => venue
                            .prepare_user_tx(&o)
                            .await
                            .map_err(err)?
                            .ok_or_else(|| ExecError::Route("venue built no transaction".into()))?,
                    };
                    r.venue_meta["user"]["prepared_data"] = tx["data"].clone();
                    r.venue_meta["user"]["num_contracts"] = tx["num_contracts"].clone();
                    transaction = Some(tx);
                    (
                        serde_json::Value::Null,
                        [0u8; 32],
                        o.timestamp + SIGNATURE_TTL_SECS,
                        None,
                    )
                }
                other => return Err(ExecError::Route(format!("{other}: not supported"))),
            };
            legs.push(PreparedLeg {
                order_id: r.id,
                leg_index: r.leg_index,
                platform: r.platform,
                native_id: r.native_id.clone(),
                side: r.side,
                qty_native: r.qty_native,
                limit_price_native: r.limit_price_native,
                max_fee_per_unit: r.max_fee_per_unit,
                typed_data,
                digest: format!("0x{}", hex::encode(digest)),
                signature_expires_at: DateTime::from_timestamp(expiry, 0).unwrap_or_default(),
                body_template,
                transaction,
            });
        }
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(SIGNATURE_TTL_SECS - 30);
        let state = PreparedState {
            route,
            rows,
            accounts: normalized.clone(),
        };
        let payload =
            serde_json::to_value(&state).map_err(|e| ExecError::Persistence(e.to_string()))?;
        self.store
            .save_prepared(
                route_id,
                &format!("{:?}", self.mode),
                now,
                expires_at,
                &payload,
            )
            .await
            .map_err(|e| ExecError::Persistence(e.to_string()))?;
        Ok(PreparedExecution {
            route_id,
            accounts: normalized,
            legs,            auth_instructions: "Derive legs: auth_signature = personal_sign(String(auth_timestamp_ms)) by the Derive signer (accepted within 2 minutes). Hypercall legs: EIP-712 PlaceOrder signed by the wallet or its approved agent. Paradex legs: typed_data signed by the user's Starknet account + per leg venue_auth = {jwt} (short-lived, never stored). Thetanuts legs: send `transaction` from the account address, then pass its tx_hash. Aevo legs: per leg venue_auth = {timestamp_ns, hmac} with hmac = hex(HMAC_SHA256(api_secret, api_key + ',' + timestamp_ns + ',POST,/orders,' + body)), body = body_template with __SIGNATURE__ replaced by that leg's signature.".into(),
            expires_at,
        })
    }

    /// NON-CUSTODIAL step 2: attach the user's signatures (verified by the venue adapter before
    /// anything is sent) and submit; strategy legs go out in parallel. An incomplete strategy is
    /// NOT auto-unwound: that needs new user signatures.
    pub async fn submit_user_signed(
        &self,
        sub: SignedSubmission,
    ) -> Result<ExecutionReport, ExecError> {
        let gone = || {
            ExecError::NotFound("prepared execution (unknown, already submitted or expired)".into())
        };
        let persistence = |e: crate::store::StoreError| ExecError::Persistence(e.to_string());
        let payload = self
            .store
            .get_prepared(sub.route_id, Utc::now())
            .await
            .map_err(persistence)?
            .ok_or_else(gone)?;
        let PreparedState {
            route,
            mut rows,
            accounts,
        } = serde_json::from_value(payload).map_err(|e| ExecError::Persistence(e.to_string()))?;
        // An incomplete submission is refused WITHOUT consuming the prepared execution.
        if let Some(r) = rows
            .iter()
            .find(|r| !sub.signatures.iter().any(|s| s.order_id == r.id))
        {
            return Err(ExecError::Route(format!(
                "missing signature for order {}",
                r.id
            )));
        }
        if rows.iter().any(|r| r.platform == PlatformId::Derive)
            && (sub.auth_timestamp_ms.is_none() || sub.auth_signature.is_none())
        {
            return Err(ExecError::Route(
                "Derive legs need auth_timestamp_ms + auth_signature".into(),
            ));
        }
        for r in &mut rows {
            let sig = sub
                .signatures
                .iter()
                .find(|s| s.order_id == r.id)
                .expect("checked");
            let mut u = accounts.get(&r.platform).cloned().unwrap_or_default();
            u["signature"] = serde_json::json!(sig.signature);
            u["tx_hash"] = serde_json::json!(sig.tx_hash);
            for k in ["prepared_data", "num_contracts"] {
                if let Some(v) = r.venue_meta["user"].get(k).cloned() {
                    u[k] = v;
                }
            }
            u["auth_timestamp_ms"] = serde_json::json!(sub.auth_timestamp_ms);
            u["auth_signature"] = serde_json::json!(sub.auth_signature);
            u["venue_auth"] = sig.venue_auth.clone().unwrap_or_default();
            r.venue_meta["user"] = u;
        }
        // Signatures checked locally BEFORE consuming: a forged submission cannot burn it.
        for r in &rows {
            self.venues[&r.platform]
                .check_user_signatures(&r.exec_order())
                .map_err(|e| ExecError::Route(e.to_string()))?;
        }
        // Single use, across every instance sharing the database.
        if !self
            .store
            .take_prepared(sub.route_id, Utc::now())
            .await
            .map_err(persistence)?
        {
            return Err(gone());
        }
        self.send_rows(route, rows, false).await
    }

    /// Consume a single-use route after the mode / kill-switch / executability checks.
    async fn take_route(&self, route_id: Uuid, route_hash: &str) -> Result<StoredRoute, ExecError> {
        if !self.mode.executes() {
            return Err(ExecError::Disabled(format!(
                "mode is {:?}; execution exists only in TESTNET or MAINNET_NONCUSTODIAL",
                self.mode
            )));
        }
        if self.kill_switch {
            return Err(ExecError::Disabled(
                "kill switch is ON (set OA_KILL_SWITCH=false in this profile's env file)".into(),
            ));
        }
        let route = match self
            .store
            .consume_route(route_id, route_hash, Utc::now())
            .await
            .map_err(|e| ExecError::Persistence(e.to_string()))?
        {
            ConsumeOutcome::Consumed(r) => *r,
            ConsumeOutcome::NotFound => return Err(ExecError::Route("unknown route id".into())),
            ConsumeOutcome::HashMismatch => {
                return Err(ExecError::Route(
                    "route hash does not match (payload changed)".into(),
                ));
            }
            ConsumeOutcome::Expired => {
                return Err(ExecError::Route(
                    "route quote expired: request a new quote".into(),
                ));
            }
            ConsumeOutcome::AlreadyConsumed => {
                return Err(ExecError::Route(
                    "route quote already used (single use)".into(),
                ));
            }
        };
        if !route.executable {
            return Err(ExecError::Route(format!(
                "route not executable: {:?}",
                route.not_executable_reasons
            )));
        }
        if let Some(sr) = &route.strategy
            && sr.max_loss.is_none()
            && !sr.allow_unbounded
        {
            return Err(ExecError::Route(
                "strategy with UNBOUNDED loss (net short calls) refused: allow_naked_sells is off"
                    .into(),
            ));
        }
        Ok(route)
    }

    /// Pre-validate and persist one VALIDATED row per leg (nothing sent). `user` = non-custodial
    /// account stored in `venue_meta.user` (the user signs later).
    async fn build_rows(
        &self,
        route: &StoredRoute,
        limits: &RiskLimits,
        users: Option<&BTreeMap<PlatformId, serde_json::Value>>,
    ) -> Result<Vec<OrderRow>, ExecError> {
        let route_id = route.id;
        // Pre-validate EVERY leg before sending anything (no half-validated routes).
        let mut rows = Vec::new();
        // Session caps = committed spend of this mode over the rolling window, read from the journal
        // (survives restarts, shared by every instance). No journal, no execution.
        let _serial = self.session.lock().await;
        let spent = self
            .store
            .committed_spend_since(
                &format!("{:?}", self.mode),
                Utc::now() - self.session_window,
            )
            .await
            .map_err(|e| ExecError::Persistence(format!("session spend: {e}")))?;
        let mut usage = SessionUsage::default();
        usage.add("manual", spent);
        for (k, leg) in route.plan.legs.iter().enumerate() {
            let venue = self.venues.get(&leg.instrument.platform).ok_or_else(|| {
                ExecError::Route(format!(
                    "{} has no execution venue",
                    leg.instrument.platform
                ))
            })?;
            if venue.network() != leg.instrument.network {
                return Err(ExecError::Route(format!(
                    "network mismatch: venue {} vs instrument {}",
                    venue.network(),
                    leg.instrument.network
                )));
            }
            let iref = route
                .instruments
                .get(&crate::engine::instrument_ref_key(
                    leg.instrument.platform,
                    &leg.instrument.native_id,
                ))
                .cloned()
                .ok_or_else(|| {
                    ExecError::Route(format!(
                        "instrument {} missing from the route quote",
                        leg.instrument.native_id
                    ))
                })?;
            let (venue_id, mult) = (iref.venue_instrument_id, iref.contract_multiplier);
            let max_fee = match &leg.fee {
                FeeAmount::Known { amount } => *amount,
                FeeAmount::Unknown { .. } => {
                    return Err(ExecError::Route("unknown fee on a leg".into()));
                }
            };
            let intent = OrderIntent {
                instrument: leg.instrument.clone(),
                family: oa_core::domain::ProductFamily::DatedOption,
                underlying: route.plan.legs[k]
                    .instrument
                    .native_id
                    .split('-')
                    .next()
                    .unwrap_or("")
                    .to_string(),
                side: leg.side,
                qty_underlying: leg.qty_underlying,
                limit_price_per_underlying: leg.limit_price_native / mult,
                max_fee,
                strategy: "manual".into(),
            };
            let reference = Some(leg.expected_avg_per_underlying);
            let v = risk::check(&intent, limits, &usage, reference, true);
            // Strategy legs: a SELL is judged at strategy level (bounded max loss), not per leg.
            let v: Vec<_> = if route.strategy.is_some() && leg.side == Side::Sell {
                v.into_iter()
                    .filter(|x| {
                        !matches!(
                            x,
                            risk::RiskViolation::SellDisabled
                                | risk::RiskViolation::MaxLossUnbounded
                        )
                    })
                    .collect()
            } else {
                v
            };
            if !v.is_empty() {
                return Err(ExecError::Risk(v));
            }
            if leg.side == Side::Buy {
                usage.add("manual", leg.limit_price_native * leg.qty_native + max_fee);
            }
            let now = Utc::now();
            let salt: u64 = (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64;
            let mut row = OrderRow {
                id: Uuid::new_v4(),
                route_id,
                leg_index: k as i32,
                mode: self.mode,
                platform: leg.instrument.platform,
                network: leg.instrument.network.clone(),
                native_id: leg.instrument.native_id.clone(),
                side: leg.side,
                qty_native: leg.qty_native,
                limit_price_native: leg.limit_price_native,
                contract_multiplier: mult,
                salt,
                timestamp: now.timestamp(),
                venue_instrument_id: venue_id,
                venue_meta: {
                    let mut m = iref.venue_meta.clone();
                    if let Some(u) = users.and_then(|u| u.get(&leg.instrument.platform)) {
                        m["user"] = u.clone();
                    }
                    m
                },
                // 1.5 x the estimated fee per contract, rounded UP to the cent (never below 0.01):
                // the venue charges its real fee, this is only the signed ceiling.
                max_fee_per_unit: Some(
                    ((max_fee / leg.qty_native) * Decimal::new(15, 1))
                        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::AwayFromZero)
                        .max(Decimal::new(1, 2)),
                ),
                expected_avg_per_underlying: leg.expected_avg_per_underlying,
                expected_total: leg.expected_total,
                expected_fee: Some(max_fee),
                precomputed_venue_id: None,
                venue_order_id: None,
                state: OrderState::Validated,
                is_final: false,
                filled_native: Decimal::ZERO,
                avg_fill_price_native: None,
                last_error: None,
                created_at: now,
                submitted_at: None,
                acked_at: None,
                updated_at: now,
                submit_to_ack_ms: None,
                reconcile_attempts: 0,
                last_venue_response: None,
            };
            row.precomputed_venue_id = venue.precompute_order_id(&row.exec_order());
            self.persist(&row, "VALIDATED", serde_json::json!({"leg": k}))
                .await?;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Session cap at SEND time, under the session lock: committed spend (orders sent or being sent;
    /// unsigned VALIDATED rows never count, so preparing without signing cannot exhaust the cap) +
    /// this route's buy cost must fit; then the rows are journaled SUBMITTING before the lock is
    /// released, so concurrent submissions see each other.
    async fn reserve_session_spend(&self, rows: &mut [OrderRow]) -> Result<(), ExecError> {
        let _serial = self.session.lock().await;
        let spent = self
            .store
            .committed_spend_since(
                &format!("{:?}", self.mode),
                Utc::now() - self.session_window,
            )
            .await
            .map_err(|e| ExecError::Persistence(format!("session spend: {e}")))?;
        let cost: Decimal = rows
            .iter()
            .filter(|r| r.side == Side::Buy)
            .map(|r| r.limit_price_native * r.qty_native + r.expected_fee.unwrap_or_default())
            .sum();
        let cap = crate::engine::risk_limits(self.mode).max_session_cost;
        if spent + cost > cap {
            for r in rows.iter_mut() {
                r.state = OrderState::Cancelled;
                r.is_final = true;
                r.last_error = Some("session cap reached at submission: nothing sent".into());
                r.updated_at = Utc::now();
                let _ = self.persist(r, "SESSION_CAP", serde_json::json!({})).await;
            }
            return Err(ExecError::Risk(vec![
                risk::RiskViolation::SessionCostAboveCap {
                    would_be: (spent + cost).normalize().to_string(),
                    cap: cap.normalize().to_string(),
                },
            ]));
        }
        for r in rows.iter_mut() {
            r.state = OrderState::Submitting;
            r.submitted_at = Some(Utc::now());
            r.updated_at = Utc::now();
            self.persist(r, "RESERVED", serde_json::json!({})).await?;
        }
        Ok(())
    }

    /// Submit validated rows (parallel for strategies), unwind if allowed, build the report.
    async fn send_rows(
        &self,
        route: StoredRoute,
        mut rows: Vec<OrderRow>,
        auto_unwind: bool,
    ) -> Result<ExecutionReport, ExecError> {
        let route_id = route.id;
        self.reserve_session_spend(&mut rows).await?;

        // Submit legs. IOC: no resting order is ever left behind. Strategy legs go out IN PARALLEL
        // (smallest window in which one leg can fail after another filled).
        let mut notes = vec![];
        if route.strategy.is_some() {
            let futs = rows.drain(..).map(|r| self.submit_row(r));
            for r in futures_util::future::join_all(futs).await {
                let (row, note) = r?;
                notes.extend(note);
                rows.push(row);
            }
        } else {
            let mut done = Vec::with_capacity(rows.len());
            for r in rows.drain(..) {
                let (row, note) = self.submit_row(r).await?;
                notes.extend(note);
                done.push(row);
            }
            rows = done;
        }
        let incomplete = rows.iter().any(|r| r.filled_native < r.qty_native);
        let (unwind_orders, strategy_outcome) = match &route.strategy {
            Some(_) if !auto_unwind && incomplete => {
                notes.push("All-or-nothing: a leg did not fill completely. Non-custodial orders cannot be unwound without the user's signature: request a quote for the opposite legs and sign it.".into());
                (vec![], Some("UNWIND_REQUIRES_USER_SIGNATURE".to_string()))
            }
            Some(_) if !auto_unwind => (vec![], Some("COMPLETE".to_string())),
            Some(sr) => {
                let (u, o, n) = self.unwind_if_incomplete(&rows, sr, route_id).await?;
                notes.extend(n);
                (u, Some(o))
            }
            None => (vec![], None),
        };
        let filled: Decimal = rows
            .iter()
            .map(|r| r.filled_native * r.contract_multiplier)
            .sum();
        if filled < route.plan.requested_underlying {
            notes.push(format!(
                "Partial execution: {} of {} filled. Unfilled IOC quantity was cancelled by the venue; no automatic re-routing in this version.",
                filled.normalize(),
                route.plan.requested_underlying.normalize()
            ));
        }
        if rows.iter().any(|r| !r.is_final) {
            notes.push("Some legs are not final yet (UNKNOWN/open): the OrderTracker reconciles them every 10 s.".into());
        }
        Ok(ExecutionReport {
            route_id,
            mode: self.mode,
            requested_underlying: route.plan.requested_underlying,
            expected_total: route.plan.expected_total,
            filled_underlying: filled,
            orders: rows,
            notes,
            unwind_orders,
            strategy_outcome,
        })
    }

    /// Submit one validated row (persist SUBMITTING, send, map the answer, reconcile UNKNOWN).
    async fn submit_row(&self, mut row: OrderRow) -> Result<(OrderRow, Option<String>), ExecError> {
        let venue = self.venues[&row.platform].clone();
        let mut note = None;
        row.state = OrderState::Submitting;
        row.submitted_at = Some(Utc::now());
        row.updated_at = Utc::now();
        // Persistence-first: if the intent cannot be journaled, the leg is NOT sent (and is reported
        // as rejected so a strategy's other legs are still unwound, instead of aborting the batch).
        if let Err(e) = self
            .persist(
                &row,
                "SUBMITTING",
                serde_json::json!({"precomputed_venue_id": row.precomputed_venue_id}),
            )
            .await
        {
            row.state = OrderState::Rejected;
            row.is_final = true;
            row.last_error = Some(format!("not sent: journal unavailable ({e})"));
            return Ok((
                row,
                Some("a leg was not sent: the order journal is unavailable".into()),
            ));
        }
        let t0 = Instant::now();
        match tokio::time::timeout(self.submit_timeout, venue.submit(&row.exec_order())).await {
            Ok(Ok(state)) => {
                row.submit_to_ack_ms = Some(t0.elapsed().as_millis() as i64);
                row.acked_at = Some(Utc::now());
                row.apply(&state);
                if let Some(p) = &row.precomputed_venue_id
                    && Some(p) != row.venue_order_id.as_ref()
                {
                    note = Some(format!(
                        "leg {}: venue order id differs from the precomputed id; reconciliation uses the returned id",
                        row.leg_index
                    ));
                }
                let detail = serde_json::json!({"status": format!("{:?}", state.status)});
                if let Some(n) = self.persist_after_send(&row, "ACK", detail).await {
                    note = Some(n);
                }
            }
            Ok(Err(e)) if is_definite_rejection(&e) => {
                row.state = OrderState::Rejected;
                row.is_final = true;
                row.last_error = Some(e.to_string());
                row.updated_at = Utc::now();
                let detail = serde_json::json!({"error": e.to_string()});
                if let Some(n) = self.persist_after_send(&row, "REJECTED", detail).await {
                    note = Some(n);
                }
            }
            Ok(Err(e)) => {
                // Transport error / 5xx: the order MAY exist. Never resubmit: reconcile.
                row.state = OrderState::Unknown;
                row.last_error = Some(e.to_string());
                row.updated_at = Utc::now();
                let detail = serde_json::json!({"error": e.to_string()});
                if let Some(n) = self.persist_after_send(&row, "UNKNOWN", detail).await {
                    note = Some(n);
                }
                self.reconcile_now(&mut row).await;
            }
            Err(_) => {
                row.state = OrderState::Unknown;
                row.last_error = Some(format!("no answer within {:?}", self.submit_timeout));
                row.updated_at = Utc::now();
                let detail = serde_json::json!({"error": "timeout"});
                if let Some(n) = self.persist_after_send(&row, "UNKNOWN", detail).await {
                    note = Some(n);
                }
                self.reconcile_now(&mut row).await;
            }
        }
        Ok((row, note))
    }

    /// Journal write AFTER the venue answered: the order exists (or may), so a database error must not
    /// turn the response into a failure. One retry, then the outcome is still returned with a note;
    /// the SUBMITTING row already journaled lets the tracker reconcile it later.
    async fn persist_after_send(
        &self,
        row: &OrderRow,
        event: &str,
        detail: serde_json::Value,
    ) -> Option<String> {
        if self.persist(row, event, detail.clone()).await.is_ok() {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        match self.persist(row, event, detail).await {
            Ok(()) => None,
            Err(e) => {
                tracing::error!(order = %row.id, event, error = %e, "journal write failed after send");
                Some(format!(
                    "leg {}: the venue answered but the journal write failed ({e}); the tracker reconciles it",
                    row.leg_index
                ))
            }
        }
    }

    /// All-or-nothing: if any strategy leg is not fully filled, send opposite IOC orders for every
    /// filled quantity, priced so the total unwind cost stays within the cap (bps of notional).
    async fn unwind_if_incomplete(
        &self,
        rows: &[OrderRow],
        sr: &StrategyRisk,
        route_id: Uuid,
    ) -> Result<(Vec<OrderRow>, String, Vec<String>), ExecError> {
        let complete = rows
            .iter()
            .all(|r| r.is_final && r.filled_native >= r.qty_native);
        if complete {
            return Ok((
                vec![],
                "COMPLETE".into(),
                vec!["All strategy legs filled.".into()],
            ));
        }
        if rows.iter().any(|r| !r.is_final) {
            return Ok((vec![], "PENDING".into(), vec!["A strategy leg is not final yet (UNKNOWN/open): unwind decision deferred to reconciliation; no blind unwind.".into()]));
        }
        let filled: Vec<&OrderRow> = rows
            .iter()
            .filter(|r| r.filled_native > Decimal::ZERO)
            .collect();
        if filled.is_empty() {
            return Ok((
                vec![],
                "UNWOUND".into(),
                vec!["No strategy leg filled: nothing to unwind.".into()],
            ));
        }
        let notional: Decimal = rows
            .iter()
            .map(|r| r.qty_native * r.contract_multiplier)
            .sum::<Decimal>()
            * sr.index_price.unwrap_or(Decimal::ZERO);
        let cap_total = notional * Decimal::from(sr.unwind_cost_cap_bps) / Decimal::from(10_000);
        let filled_contracts: Decimal = filled.iter().map(|r| r.filled_native).sum();
        let mut out = vec![];
        let mut notes = vec![format!(
            "All-or-nothing: a strategy leg did not fill completely; unwinding {} filled leg(s) within a cost cap of {} ({} bps of notional).",
            filled.len(),
            cap_total.round_dp(2),
            sr.unwind_cost_cap_bps
        )];
        for (k, r) in filled.iter().enumerate() {
            let fill = r.avg_fill_price_native.unwrap_or(r.limit_price_native);
            // Share of the cap for this leg, per contract.
            let per_contract = if filled_contracts.is_zero() {
                Decimal::ZERO
            } else {
                cap_total / filled_contracts
            };
            let (side, limit) = match r.side {
                Side::Buy => (Side::Sell, (fill - per_contract).max(Decimal::new(1, 8))),
                Side::Sell => (Side::Buy, fill + per_contract),
            };
            let now = Utc::now();
            let mut u = OrderRow {
                id: Uuid::new_v4(),
                route_id,
                leg_index: 100 + k as i32,
                side,
                qty_native: r.filled_native,
                limit_price_native: limit,
                salt: (Uuid::new_v4().as_u128() % 1_000_000_000_000) as u64,
                timestamp: now.timestamp(),
                expected_avg_per_underlying: fill / r.contract_multiplier,
                expected_total: fill * r.filled_native,
                precomputed_venue_id: None,
                venue_order_id: None,
                state: OrderState::Validated,
                is_final: false,
                filled_native: Decimal::ZERO,
                avg_fill_price_native: None,
                last_error: None,
                created_at: now,
                submitted_at: None,
                acked_at: None,
                updated_at: now,
                submit_to_ack_ms: None,
                reconcile_attempts: 0,
                last_venue_response: None,
                ..(*r).clone()
            };
            let venue = self.venues[&u.platform].clone();
            u.precomputed_venue_id = venue.precompute_order_id(&u.exec_order());
            self.persist(
                &u,
                "UNWIND_VALIDATED",
                serde_json::json!({"unwinds_leg": r.leg_index}),
            )
            .await?;
            let (u, n) = self.submit_row(u).await?;
            notes.extend(n);
            out.push(u);
        }
        let all_unwound = out
            .iter()
            .all(|u| u.is_final && u.filled_native >= u.qty_native);
        let outcome = if all_unwound {
            "UNWOUND"
        } else {
            "UNWIND_INCOMPLETE"
        };
        if !all_unwound {
            notes.push("Unwind could not be completed within the cost cap: the residual position is KEPT and flagged (no unbounded unwind). The integrator must decide.".into());
        }
        Ok((out, outcome.into(), notes))
    }

    async fn reconcile_now(&self, row: &mut OrderRow) {
        let venue = self.venues[&row.platform].clone();
        for wait in [300u64, 800, 2000] {
            tokio::time::sleep(Duration::from_millis(wait)).await;
            if reconcile_once(venue.as_ref(), row).await {
                break;
            }
        }
        let _ = self
            .persist(
                row,
                "RECONCILE",
                serde_json::json!({"attempts": row.reconcile_attempts}),
            )
            .await;
    }

    async fn persist(
        &self,
        row: &OrderRow,
        event: &str,
        detail: serde_json::Value,
    ) -> Result<(), ExecError> {
        tracing::info!(target: "testnet", order = %row.id, leg = row.leg_index, state = ?row.state, event, "order event");
        // Short-lived user credentials (Paradex JWT, Aevo HMAC, Derive REST auth) are used once,
        // never journaled.
        let mut safe = row.clone();
        if let Some(u) = safe
            .venue_meta
            .get_mut("user")
            .and_then(|u| u.as_object_mut())
            && u.get("auth_signature").is_some_and(|v| v.is_string())
        {
            u.insert("auth_signature".into(), serde_json::json!("[redacted]"));
        }
        if let Some(a) = safe
            .venue_meta
            .get_mut("user")
            .and_then(|u| u.get_mut("venue_auth"))
            && let Some(m) = a.as_object_mut()
        {
            for k in ["jwt", "hmac"] {
                if m.contains_key(k) {
                    m.insert(k.into(), serde_json::json!("[redacted]"));
                }
            }
        }
        self.store
            .upsert_order(&safe, event, &detail)
            .await
            .map_err(|e| ExecError::Persistence(e.to_string()))
    }

    pub async fn cancel(&self, order_id: Uuid) -> Result<OrderRow, ExecError> {
        if !self.mode.executes() {
            return Err(ExecError::Disabled(
                "cancellation exists only in TESTNET or MAINNET_NONCUSTODIAL".into(),
            ));
        }
        let mut row = self
            .store
            .get_order(order_id)
            .await
            .map_err(|e| ExecError::Persistence(e.to_string()))?
            .ok_or_else(|| ExecError::NotFound(order_id.to_string()))?;
        if row.is_final {
            return Ok(row);
        }
        let venue = self
            .venues
            .get(&row.platform)
            .ok_or_else(|| ExecError::Disabled("no execution venue".into()))?
            .clone();
        let id = row
            .venue_order_id
            .clone()
            .or(row.precomputed_venue_id.clone())
            .ok_or_else(|| ExecError::NotFound("venue order id".into()))?;
        row.state = OrderState::CancelPending;
        self.persist(&row, "CANCEL_PENDING", serde_json::json!({}))
            .await?;
        match venue.cancel(&id).await {
            Ok(s) => row.apply(&s),
            Err(e) => {
                row.last_error = Some(e.to_string());
                row.state = OrderState::Unknown;
            }
        }
        self.persist(&row, "CANCEL_RESULT", serde_json::json!({}))
            .await?;
        Ok(row)
    }
}

/// One lookup by the best known id. Returns true when the state is now known.
async fn reconcile_once(venue: &dyn ExecutionVenue, row: &mut OrderRow) -> bool {
    row.reconcile_attempts += 1;
    let ids: Vec<String> = [row.venue_order_id.clone(), row.precomputed_venue_id.clone()]
        .into_iter()
        .flatten()
        .collect();
    for id in ids {
        match venue.get_order(&id).await {
            Ok(Some(s)) => {
                row.apply(&s);
                return !matches!(row.state, OrderState::Unknown);
            }
            Ok(None) => continue,
            Err(e) => row.last_error = Some(format!("reconcile: {e}")),
        }
    }
    false
}

/// Kameo actor: reconciles non-final orders from the journal (also after a restart).
pub struct OrderTracker {
    venues: BTreeMap<PlatformId, Arc<dyn ExecutionVenue>>,
    store: Arc<dyn Store>,
}

impl Actor for OrderTracker {
    type Args = Self;
    type Error = Infallible;
    async fn on_start(state: Self, _r: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(state)
    }
    /// One failed reconciliation pass must not stop reconciliation for good: log and keep going.
    async fn on_panic(
        &mut self,
        _r: kameo::actor::WeakActorRef<Self>,
        err: kameo::error::PanicError,
    ) -> Result<std::ops::ControlFlow<kameo::error::ActorStopReason>, Self::Error> {
        tracing::error!(error = %err, "order tracker panicked; continuing");
        Ok(std::ops::ControlFlow::Continue(()))
    }
}

pub struct ReconcileTick;

impl Message<ReconcileTick> for OrderTracker {
    type Reply = ();
    async fn handle(&mut self, _: ReconcileTick, _ctx: &mut Context<Self, Self::Reply>) {
        let open = match self.store.open_orders().await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(error = %e, "reconcile: cannot read journal");
                return;
            }
        };
        for mut row in open {
            // Non-custodial rows the user never signed: expire them (nothing was ever sent).
            if row.state == OrderState::Validated
                && Utc::now() - row.created_at
                    > chrono::Duration::seconds(oa_adapters::derive_user::SIGNATURE_TTL_SECS + 60)
            {
                row.state = OrderState::Cancelled;
                row.is_final = true;
                row.last_error = Some("never signed/submitted: expired".into());
                row.updated_at = Utc::now();
                let _ = self
                    .store
                    .upsert_order(&row, "EXPIRED_UNSIGNED", &serde_json::json!({}))
                    .await;
                continue;
            }
            // Never touch an order that is still validating/submitting in this process: only
            // UNKNOWN / ACKNOWLEDGED / CANCEL_PENDING / stale SUBMITTING (> 30 s) are reconciled.
            let stale_submitting = row.state == OrderState::Submitting
                && row
                    .submitted_at
                    .is_some_and(|t| Utc::now() - t > chrono::Duration::seconds(30));
            if !(matches!(
                row.state,
                OrderState::Unknown
                    | OrderState::Acknowledged
                    | OrderState::CancelPending
                    | OrderState::PartiallyFilled
            ) || stale_submitting)
            {
                continue;
            }
            let Some(v) = self.venues.get(&row.platform).cloned() else {
                continue;
            };
            let known = reconcile_once(v.as_ref(), &mut row).await;
            let _ = self
                .store
                .upsert_order(&row, "RECONCILE_TICK", &serde_json::json!({"known": known}))
                .await;
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VenueExecStats {
    pub platform: PlatformId,
    pub orders: usize,
    pub filled: usize,
    pub partially_filled: usize,
    pub rejected: usize,
    pub cancelled: usize,
    pub unknown_or_open: usize,
    /// filled quantity / requested quantity over all orders.
    pub fill_ratio: Option<Decimal>,
    pub slippage_bps_mean: Option<Decimal>,
    pub slippage_bps_max: Option<Decimal>,
    pub submit_to_ack_ms_p50: Option<i64>,
    pub submit_to_ack_ms_max: Option<i64>,
    /// Penalty used by the router (bps): mean adverse slippage + 100 x reject rate, only with >= 10 orders.
    pub router_penalty_bps: Decimal,
    pub sample_note: String,
}

pub const MIN_ORDERS_FOR_PENALTY: usize = 10;

pub fn exec_stats(orders: &[OrderRow]) -> Vec<VenueExecStats> {
    let mut by: BTreeMap<PlatformId, Vec<&OrderRow>> = BTreeMap::new();
    for o in orders {
        by.entry(o.platform).or_default().push(o);
    }
    by.into_iter()
        .map(|(p, os)| {
            let n = os.len();
            let count = |s: OrderState| os.iter().filter(|o| o.state == s).count();
            let req: Decimal = os.iter().map(|o| o.qty_native).sum();
            let fil: Decimal = os.iter().map(|o| o.filled_native).sum();
            let slips: Vec<Decimal> = os.iter().filter_map(|o| o.slippage_bps()).collect();
            let mean = (!slips.is_empty()).then(|| slips.iter().copied().sum::<Decimal>() / Decimal::from(slips.len()));
            let mut lat: Vec<i64> = os.iter().filter_map(|o| o.submit_to_ack_ms).collect();
            lat.sort();
            let rejected = count(OrderState::Rejected);
            let penalty = if n >= MIN_ORDERS_FOR_PENALTY {
                mean.unwrap_or_default().max(Decimal::ZERO) + Decimal::from(100) * Decimal::from(rejected) / Decimal::from(n)
            } else {
                Decimal::ZERO
            };
            VenueExecStats {
                platform: p,
                orders: n,
                filled: count(OrderState::Filled),
                partially_filled: count(OrderState::PartiallyFilled),
                rejected,
                cancelled: count(OrderState::Cancelled),
                unknown_or_open: os.iter().filter(|o| !o.is_final).count(),
                fill_ratio: (req > Decimal::ZERO).then(|| (fil / req).round_dp(4)),
                slippage_bps_mean: mean.map(|m| m.round_dp(2)),
                slippage_bps_max: slips.iter().copied().max(),
                submit_to_ack_ms_p50: lat.get(lat.len().saturating_sub(1) / 2).copied(),
                submit_to_ack_ms_max: lat.last().copied(),
                router_penalty_bps: penalty.round_dp(2),
                sample_note: if n < MIN_ORDERS_FOR_PENALTY {
                    format!("{n} orders: below {MIN_ORDERS_FOR_PENALTY}, no routing penalty applied (not statistically meaningful)")
                } else {
                    format!("{n} orders")
                },
            }
        })
        .collect()
}
