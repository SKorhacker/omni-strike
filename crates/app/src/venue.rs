//! One Kameo actor per integrated platform.
//!
//! The actor OWNS long-lived state: instrument catalogs, a bounded cache of book snapshots,
//! in-flight fetch bookkeeping and health / circuit-breaker state. It never awaits network
//! I/O inside a handler: fetches run in spawned tasks that publish into a `watch` channel
//! (the cache value) and then report back with `FetchDone`.
//!
//! Mailbox: bounded(256). Callers use `mailbox_timeout(100ms)` + `reply_timeout(2s)`.
//! All messages are queries or bookkeeping; no order command ever goes through this actor.
//! Snapshots are "latest value" semantics: a newer full snapshot supersedes an older one.

use chrono::{DateTime, Utc};
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::error::SendError;
use kameo::mailbox;
use kameo::message::{Context, Message};
use oa_adapters::{Connector, ConnectorError, MarketData};
use oa_core::domain::{BookSnapshot, Instrument, PlatformId};
use schemars::JsonSchema;
use serde::Serialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

use crate::metrics::Metrics;

pub const MAILBOX_CAPACITY: usize = 256;
pub const MAILBOX_TIMEOUT: Duration = Duration::from_millis(100);
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
pub const BOOK_CACHE_CAPACITY: usize = 256;
const FAILURES_BEFORE_OPEN: u32 = 5;
const CIRCUIT_OPEN_FOR: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
/// Keep-warm for polled (non-streamed) venues: books requested in the last HOT_WINDOW are
/// refreshed in the background once older than HOT_REFRESH, at most MAX_HOT per venue.
/// Measured with oa-bench (kept only because it lowered fresh-compare latency).
pub const HOT_WINDOW: Duration = Duration::from_secs(30);
pub const HOT_REFRESH: Duration = Duration::from_millis(1000);
pub const MAX_HOT: usize = 4;
const HOT_TICK: Duration = Duration::from_millis(250);
const NEGATIVE_CACHE: Duration = Duration::from_secs(2);
/// Hard deadline of one background fetch (connectors may chain several 10 s HTTP calls). Past it the
/// slot is released with an error instead of staying "in flight" forever.
const BOOK_FETCH_DEADLINE: Duration = Duration::from_secs(15);
const CATALOG_FETCH_DEADLINE: Duration = Duration::from_secs(30);

/// Run a connector call with a deadline and panic isolation: a hung or panicking connector yields an
/// error, so `FetchDone` is ALWAYS sent and the cache slot never stays wedged "in flight".
async fn guarded<T>(
    platform: PlatformId,
    deadline: Duration,
    fut: impl std::future::Future<Output = Result<T, ConnectorError>>,
) -> Result<T, ConnectorError> {
    use futures_util::FutureExt;
    match tokio::time::timeout(deadline, std::panic::AssertUnwindSafe(fut).catch_unwind()).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            tracing::error!(%platform, "connector panicked during a fetch");
            Err(ConnectorError::InvalidResponse {
                platform,
                detail: "connector panicked (logged)".into(),
            })
        }
        Err(_) => Err(ConnectorError::Timeout {
            platform,
            after: deadline,
        }),
    }
}

#[derive(Debug, Clone)]
pub enum FetchState<T> {
    Pending,
    Ready {
        value: Arc<T>,
        at: DateTime<Utc>,
        mono: Instant,
    },
    Failed {
        error: String,
        at: DateTime<Utc>,
        mono: Instant,
    },
}

struct Slot<T> {
    tx: Arc<watch::Sender<FetchState<T>>>,
    inflight: bool,
    last_access: Instant,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(FetchState::Pending).0),
            inflight: false,
            last_access: Instant::now(),
        }
    }

    /// Start a fetch unless one is running or the current value is fresh enough.
    fn needs_fetch(&self, max_age: Duration) -> bool {
        if self.inflight {
            return false;
        }
        !matches!(&*self.tx.borrow(), FetchState::Ready { mono, .. } if mono.elapsed() <= max_age)
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CatalogInfo {
    pub underlying: String,
    pub state: &'static str,
    pub instruments: Option<usize>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, kameo::Reply)]
pub struct VenueHealth {
    pub platform: PlatformId,
    pub network: String,
    /// UP | DEGRADED | CIRCUIT_OPEN | NO_DATA_YET
    pub state: &'static str,
    pub consecutive_failures: u32,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub circuit_open_until: Option<DateTime<Utc>>,
    pub catalogs: Vec<CatalogInfo>,
    pub cached_books: usize,
    pub book_cache_capacity: usize,
    pub fetches_inflight: usize,
    pub mailbox_capacity: usize,
}

#[derive(Debug, Clone, thiserror::Error, Serialize, JsonSchema)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VenueError {
    #[error("circuit open until {until}: {reason}")]
    CircuitOpen {
        reason: String,
        until: DateTime<Utc>,
    },
    #[error("venue actor busy (mailbox full or slow)")]
    Busy,
    #[error("venue actor not running")]
    ActorDown,
    #[error("timed out after {waited_ms} ms waiting for data")]
    Timeout { waited_ms: u64 },
    #[error("fetch failed: {message}")]
    FetchFailed { message: String },
}

#[derive(Default)]
struct Health {
    consecutive_failures: u32,
    last_success_at: Option<DateTime<Utc>>,
    last_error: Option<(String, DateTime<Utc>)>,
    open_until: Option<(Instant, DateTime<Utc>, String)>,
}

pub struct VenueActor {
    connector: Arc<dyn Connector>,
    metrics: Arc<Metrics>,
    catalogs: HashMap<String, Slot<Vec<Instrument>>>,
    books: HashMap<String, Slot<MarketData>>,
    /// Instrument per cached book (needed to refetch it in the background).
    book_instruments: HashMap<String, Instrument>,
    /// Background bulk top-of-book per native id (when the venue offers it): (fetched at, book).
    tops: HashMap<String, (Instant, BookSnapshot)>,
    health: Health,
}

impl Actor for VenueActor {
    type Args = Self;
    type Error = Infallible;

    async fn on_start(state: Self, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(state)
    }

    /// A panic inside one handler must not take the venue down for the rest of the process (every
    /// later ask would answer ActorDown): log it and keep serving. The state it guards is caches and
    /// counters, rebuilt by the next fetch.
    async fn on_panic(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        err: kameo::error::PanicError,
    ) -> Result<std::ops::ControlFlow<kameo::error::ActorStopReason>, Self::Error> {
        let p = self.connector.platform();
        tracing::error!(platform = %p, error = %err, "venue actor handler panicked; continuing");
        self.metrics.incr(format!("actor_panics.{p}"));
        Ok(std::ops::ControlFlow::Continue(()))
    }
}

impl VenueActor {
    fn circuit(&mut self) -> Result<(), VenueError> {
        if let Some((until, until_utc, reason)) = &self.health.open_until {
            if Instant::now() < *until {
                return Err(VenueError::CircuitOpen {
                    reason: reason.clone(),
                    until: *until_utc,
                });
            }
            // Half-open: the next failure reopens at once, a success closes it.
            self.health.open_until = None;
            self.health.consecutive_failures = self
                .health
                .consecutive_failures
                .min(FAILURES_BEFORE_OPEN - 1);
        }
        Ok(())
    }

    fn evict_books_if_full(&mut self) {
        while self.books.len() >= BOOK_CACHE_CAPACITY {
            let victim = self
                .books
                .iter()
                .filter(|(_, s)| !s.inflight)
                .min_by_key(|(_, s)| s.last_access)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.books.remove(&k);
                    self.book_instruments.remove(&k);
                    self.metrics.incr("book_cache_evictions");
                }
                None => break, // everything in flight: allow temporary overflow
            }
        }
    }
}

pub struct GetCatalog {
    pub underlying: String,
    pub max_age: Duration,
}

pub struct GetBook {
    pub instrument: Instrument,
    pub max_age: Duration,
}

pub struct GetHealth;

/// Result of one background bulk top-of-book request.
pub struct BulkTops {
    pub at: Instant,
    pub network: Duration,
    pub books: Vec<BookSnapshot>,
}

/// Periodic keep-warm tick (sent by the handle's timer; dropped if the mailbox is full).
pub struct RefreshHot;

struct FetchFailure {
    message: String,
    retry: Option<Duration>,
    /// The venue answered but THIS item is unusable (stale upstream data, unknown or delisted
    /// instrument): not a sign that the venue is down, so no circuit accounting.
    upstream_stale: bool,
}

enum FetchKey {
    Catalog(String),
    Book(String),
}

struct FetchDone {
    key: FetchKey,
    outcome: Result<(), FetchFailure>,
}

impl Message<GetCatalog> for VenueActor {
    type Reply = Result<watch::Receiver<FetchState<Vec<Instrument>>>, VenueError>;

    async fn handle(
        &mut self,
        msg: GetCatalog,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Err(e) = self.circuit() {
            // Circuit open: no new fetch, but an already loaded catalog is still served.
            return match self.catalogs.get(&msg.underlying) {
                Some(s) if matches!(&*s.tx.borrow(), FetchState::Ready { .. }) => {
                    Ok(s.tx.subscribe())
                }
                _ => Err(e),
            };
        }
        let slot = self
            .catalogs
            .entry(msg.underlying.clone())
            .or_insert_with(Slot::new);
        slot.last_access = Instant::now();
        let rx = slot.tx.subscribe();
        if slot.needs_fetch(msg.max_age) {
            slot.inflight = true;
            let (tx, conn, me, metrics) = (
                slot.tx.clone(),
                self.connector.clone(),
                ctx.actor_ref().clone(),
                self.metrics.clone(),
            );
            let platform = conn.platform();
            tokio::spawn(async move {
                let t0 = Instant::now();
                let r = guarded(
                    platform,
                    CATALOG_FETCH_DEADLINE,
                    conn.list_instruments(&msg.underlying),
                )
                .await;
                metrics.observe("catalog_fetch_total", t0.elapsed());
                let outcome = publish_catalog(&tx, r);
                let _ = me
                    .tell(FetchDone {
                        key: FetchKey::Catalog(msg.underlying),
                        outcome,
                    })
                    .send()
                    .await;
            });
        }
        Ok(rx)
    }
}

impl Message<GetBook> for VenueActor {
    type Reply = Result<watch::Receiver<FetchState<MarketData>>, VenueError>;

    async fn handle(&mut self, msg: GetBook, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let key = msg.instrument.key.native_id.clone();
        if let Err(e) = self.circuit() {
            // Circuit open: no new fetch, but a book still fresh enough (cached or stream-fed) is served.
            return match self.books.get(&key) {
                Some(s) if matches!(&*s.tx.borrow(), FetchState::Ready { mono, .. } if mono.elapsed() <= msg.max_age) => {
                    Ok(s.tx.subscribe())
                }
                _ => Err(e),
            };
        }
        if !self.books.contains_key(&key) {
            self.evict_books_if_full();
        }
        self.book_instruments
            .entry(key.clone())
            .or_insert_with(|| msg.instrument.clone());
        let slot = self.books.entry(key.clone()).or_insert_with(Slot::new);
        slot.last_access = Instant::now();
        let rx = slot.tx.subscribe();
        if slot.needs_fetch(msg.max_age) {
            let have_fresh = matches!(&*slot.tx.borrow(), FetchState::Ready { mono, .. } if mono.elapsed() <= msg.max_age);
            let tx = slot.tx.clone();
            self.start_book_fetch(key.clone(), msg.instrument, ctx.actor_ref().clone());
            // Cold miss: answer at once with the bulk top of book (1 level, flagged by its path) while
            // the full book is fetched; the full book replaces it as soon as it lands.
            if !have_fresh
                && let Some((at, top)) = self.tops.get(&key)
                && at.elapsed() <= msg.max_age
            {
                self.metrics
                    .incr(format!("book_path.{}.bulk_l1", self.connector.platform()));
                tx.send_replace(FetchState::Ready {
                    value: Arc::new(MarketData {
                        book: top.clone(),
                        network: Duration::ZERO,
                        normalize: Duration::ZERO,
                        path: "bulk_l1",
                    }),
                    at: Utc::now(),
                    mono: *at,
                });
            }
        }
        Ok(rx)
    }
}

impl VenueActor {
    /// Spawn the network fetch of one book; the result is published into its watch channel.
    fn start_book_fetch(&mut self, key: String, instrument: Instrument, me: ActorRef<Self>) {
        let Some(slot) = self.books.get_mut(&key) else {
            return;
        };
        slot.inflight = true;
        let (tx, conn, metrics) = (
            slot.tx.clone(),
            self.connector.clone(),
            self.metrics.clone(),
        );
        let platform = conn.platform();
        tokio::spawn(async move {
            let r = guarded(
                platform,
                BOOK_FETCH_DEADLINE,
                conn.get_market_data(&instrument),
            )
            .await;
            if let Ok(md) = &r {
                metrics.observe(network_metric(platform), md.network);
                metrics.observe(normalize_metric(platform), md.normalize);
                metrics.incr(format!("book_path.{platform}.{}", md.path));
            }
            let outcome = publish(&tx, r.map(|md| (md, ())));
            let _ = me
                .tell(FetchDone {
                    key: FetchKey::Book(key),
                    outcome,
                })
                .send()
                .await;
        });
    }
}

impl Message<BulkTops> for VenueActor {
    type Reply = ();

    async fn handle(&mut self, msg: BulkTops, _ctx: &mut Context<Self, Self::Reply>) {
        let p = self.connector.platform();
        self.metrics
            .observe(format!("bulk_tops_network.{p}"), msg.network);
        for b in msg.books {
            self.tops
                .insert(b.instrument.native_id.clone(), (msg.at, b));
        }
        // Drop entries not refreshed for a minute (expired / delisted contracts).
        self.tops
            .retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
    }
}

impl Message<RefreshHot> for VenueActor {
    type Reply = ();

    async fn handle(&mut self, _: RefreshHot, ctx: &mut Context<Self, Self::Reply>) {
        if self.circuit().is_err() {
            return;
        }
        let refresh = self.connector.hot_refresh_interval().max(HOT_REFRESH);
        let mut hot: Vec<(String, Instant)> = self
            .books
            .iter()
            .filter(|(_, s)| !s.inflight && s.last_access.elapsed() <= HOT_WINDOW)
            .filter(|(_, s)| match &*s.tx.borrow() {
                FetchState::Ready { mono, .. } => mono.elapsed() >= refresh,
                _ => false, // never re-poll a failed/pending book in the background
            })
            .map(|(k, s)| (k.clone(), s.last_access))
            .collect();
        hot.sort_by(|a, b| b.1.cmp(&a.1));
        for (key, _) in hot.into_iter().take(MAX_HOT) {
            if let Some(inst) = self.book_instruments.get(&key).cloned() {
                self.metrics
                    .incr(format!("hot_refreshes.{}", self.connector.platform()));
                self.start_book_fetch(key, inst, ctx.actor_ref().clone());
            }
        }
    }
}

/// Publish a fetch result into the cache channel. Errors replace the value: a failed refresh
/// invalidates the previous snapshot (callers must not use stale data silently).
fn publish<T>(
    tx: &watch::Sender<FetchState<T>>,
    r: Result<(T, ()), ConnectorError>,
) -> Result<(), FetchFailure> {
    match r {
        Ok((v, ())) => {
            tx.send_replace(FetchState::Ready {
                value: Arc::new(v),
                at: Utc::now(),
                mono: Instant::now(),
            });
            Ok(())
        }
        Err(e) => {
            let retry = match &e {
                ConnectorError::RateLimited { retry_after, .. } => {
                    Some(retry_after.unwrap_or(Duration::from_secs(5)))
                }
                _ => None,
            };
            let upstream_stale = matches!(
                e,
                ConnectorError::UpstreamStale { .. } | ConnectorError::UnknownInstrument { .. }
            );
            let msg = e.to_string();
            tx.send_replace(FetchState::Failed {
                error: msg.clone(),
                at: Utc::now(),
                mono: Instant::now(),
            });
            Err(FetchFailure {
                message: msg,
                retry,
                upstream_stale,
            })
        }
    }
}

/// Catalogs change slowly: a failed REFRESH keeps serving the last good list for another TTL (the
/// failure still counts for health / the circuit), instead of making every compare wait on, or lose,
/// the venue. A catalog that never loaded publishes the failure.
fn publish_catalog(
    tx: &watch::Sender<FetchState<Vec<Instrument>>>,
    r: Result<Vec<Instrument>, ConnectorError>,
) -> Result<(), FetchFailure> {
    let previous = match (&r, &*tx.borrow()) {
        (Err(_), FetchState::Ready { value, at, .. }) => Some((value.clone(), *at)),
        _ => None,
    };
    let outcome = publish(tx, r.map(|v| (v, ())));
    if let Some((value, at)) = previous {
        tx.send_replace(FetchState::Ready {
            value,
            at,
            mono: Instant::now(),
        });
    }
    outcome
}

fn network_metric(p: PlatformId) -> String {
    format!("book_fetch_network.{p}")
}

fn normalize_metric(p: PlatformId) -> String {
    format!("book_normalize.{p}")
}

impl Message<FetchDone> for VenueActor {
    type Reply = ();

    async fn handle(&mut self, msg: FetchDone, _ctx: &mut Context<Self, Self::Reply>) {
        match &msg.key {
            FetchKey::Catalog(k) => {
                if let Some(s) = self.catalogs.get_mut(k) {
                    s.inflight = false;
                }
            }
            FetchKey::Book(k) => {
                if let Some(s) = self.books.get_mut(k) {
                    s.inflight = false;
                }
            }
        }
        let p = self.connector.platform();
        match msg.outcome {
            Ok(()) => {
                self.health.consecutive_failures = 0;
                self.health.last_success_at = Some(Utc::now());
            }
            Err(FetchFailure {
                upstream_stale: true,
                message,
                ..
            }) => {
                // The platform said its own data is stale: the instrument's cache is already
                // invalidated (Failed), but the connection is healthy -> no circuit accounting.
                self.metrics.incr(format!("upstream_stale.{p}"));
                self.health.last_error = Some((message, Utc::now()));
            }
            Err(FetchFailure {
                message: err,
                retry,
                ..
            }) => {
                self.metrics.incr(format!("fetch_errors.{p}"));
                self.health.consecutive_failures += 1;
                self.health.last_error = Some((err.clone(), Utc::now()));
                let open_for = match retry {
                    Some(r) => Some(r.clamp(Duration::from_secs(1), MAX_RATE_LIMIT_BACKOFF)),
                    None if self.health.consecutive_failures >= FAILURES_BEFORE_OPEN => {
                        Some(CIRCUIT_OPEN_FOR)
                    }
                    None => None,
                };
                if let Some(d) = open_for {
                    self.metrics.incr(format!("circuit_opened.{p}"));
                    tracing::warn!(platform = %p, ?d, %err, "circuit opened");
                    self.health.open_until = Some((
                        Instant::now() + d,
                        Utc::now() + chrono::Duration::from_std(d).unwrap_or_default(),
                        if retry.is_some() {
                            format!("rate limited: {err}")
                        } else {
                            err
                        },
                    ));
                }
            }
        }
    }
}

impl Message<GetHealth> for VenueActor {
    type Reply = VenueHealth;

    async fn handle(&mut self, _: GetHealth, _ctx: &mut Context<Self, Self::Reply>) -> VenueHealth {
        let open = self.circuit().err();
        let catalogs = self
            .catalogs
            .iter()
            .map(|(u, s)| {
                let (state, instruments, fetched_at, error) = match &*s.tx.borrow() {
                    FetchState::Pending => ("PENDING", None, None, None),
                    FetchState::Ready { value, at, .. } => {
                        ("READY", Some(value.len()), Some(*at), None)
                    }
                    FetchState::Failed { error, at, .. } => {
                        ("FAILED", None, Some(*at), Some(error.clone()))
                    }
                };
                CatalogInfo {
                    underlying: u.clone(),
                    state,
                    instruments,
                    fetched_at,
                    error,
                }
            })
            .collect::<Vec<_>>();
        let state = if open.is_some() {
            "CIRCUIT_OPEN"
        } else if self.health.last_success_at.is_none() {
            "NO_DATA_YET"
        } else if self.health.consecutive_failures > 0 {
            "DEGRADED"
        } else {
            "UP"
        };
        VenueHealth {
            platform: self.connector.platform(),
            network: self.connector.network().into(),
            state,
            consecutive_failures: self.health.consecutive_failures,
            last_success_at: self.health.last_success_at,
            last_error: self.health.last_error.as_ref().map(|e| e.0.clone()),
            last_error_at: self.health.last_error.as_ref().map(|e| e.1),
            circuit_open_until: match open {
                Some(VenueError::CircuitOpen { until, .. }) => Some(until),
                _ => None,
            },
            catalogs,
            cached_books: self.books.len(),
            book_cache_capacity: BOOK_CACHE_CAPACITY,
            fetches_inflight: self.books.values().filter(|s| s.inflight).count()
                + self.catalogs.values().filter(|s| s.inflight).count(),
            mailbox_capacity: MAILBOX_CAPACITY,
        }
    }
}

/// Cheap, cloneable handle used by the engine.
#[derive(Clone)]
pub struct VenueHandle {
    pub actor: ActorRef<VenueActor>,
    pub connector: Arc<dyn Connector>,
    metrics: Arc<Metrics>,
}

fn map_send<M, E>(e: SendError<M, E>) -> VenueError
where
    E: std::fmt::Debug,
{
    match e {
        SendError::HandlerError(_) => VenueError::FetchFailed {
            message: "handler error".into(),
        },
        SendError::ActorNotRunning(_) | SendError::ActorStopped | SendError::ActorRestarting(_) => {
            VenueError::ActorDown
        }
        SendError::MailboxFull(_) | SendError::Timeout(_) => VenueError::Busy,
    }
}

async fn await_fresh<T>(
    mut rx: watch::Receiver<FetchState<T>>,
    asked_at: Instant,
    max_age: Duration,
    wait: Duration,
) -> Result<Arc<T>, VenueError> {
    let r = tokio::time::timeout(
        wait,
        rx.wait_for(|s| match s {
            FetchState::Ready { mono, .. } => mono.elapsed() <= max_age,
            // A failure younger than NEGATIVE_CACHE is answered at once (no wait for a retry that the
            // circuit breaker / rate limit would delay anyway).
            FetchState::Failed { mono, .. } => *mono >= asked_at || mono.elapsed() < NEGATIVE_CACHE,
            FetchState::Pending => false,
        }),
    )
    .await;
    match r {
        Err(_) => Err(VenueError::Timeout {
            waited_ms: wait.as_millis() as u64,
        }),
        Ok(Err(_)) => Err(VenueError::ActorDown),
        Ok(Ok(s)) => match &*s {
            FetchState::Ready { value, .. } => Ok(value.clone()),
            FetchState::Failed { error, .. } => Err(VenueError::FetchFailed {
                message: error.clone(),
            }),
            FetchState::Pending => unreachable!("wait_for predicate excludes Pending"),
        },
    }
}

/// Background bulk top-of-book loop (one request per underlying per period). Stops for a venue that
/// has no bulk endpoint; on errors it backs off (rate limits honoured) and keeps the last data.
fn spawn_bulk_tops(
    actor: ActorRef<VenueActor>,
    connector: Arc<dyn Connector>,
    metrics: Arc<Metrics>,
) {
    tokio::spawn(async move {
        let period = connector
            .bulk_refresh_interval()
            .max(Duration::from_millis(500));
        loop {
            let mut supported = false;
            for u in crate::engine::supported_underlyings() {
                if !actor.is_alive() {
                    return;
                }
                let t = Instant::now();
                match connector.bulk_top_of_book(u).await {
                    Ok(None) => {}
                    Ok(Some(books)) => {
                        supported = true;
                        let _ = actor
                            .tell(BulkTops {
                                at: t,
                                network: t.elapsed(),
                                books,
                            })
                            .send()
                            .await;
                    }
                    Err(e) => {
                        supported = true;
                        metrics.incr(format!("bulk_tops_errors.{}", connector.platform()));
                        let wait = match e {
                            oa_adapters::ConnectorError::RateLimited { retry_after, .. } => {
                                retry_after.unwrap_or(Duration::from_secs(10))
                            }
                            _ => Duration::from_secs(5),
                        };
                        tokio::time::sleep(wait).await;
                    }
                }
            }
            if !supported {
                return;
            }
            tokio::time::sleep(period).await;
        }
    });
}

impl VenueHandle {
    pub fn spawn(connector: Arc<dyn Connector>, metrics: Arc<Metrics>) -> Self {
        let actor = VenueActor {
            connector: connector.clone(),
            metrics: metrics.clone(),
            catalogs: HashMap::new(),
            books: HashMap::new(),
            book_instruments: HashMap::new(),
            tops: HashMap::new(),
            health: Health::default(),
        };
        let streams = connector.streams_books();
        let actor = VenueActor::spawn_with_mailbox(actor, mailbox::bounded(MAILBOX_CAPACITY));
        if !streams
            && std::env::var("OA_HOT_REFRESH")
                .map(|v| v != "off")
                .unwrap_or(true)
        {
            let a = actor.clone();
            tokio::spawn(async move {
                let mut iv = tokio::time::interval(HOT_TICK);
                loop {
                    iv.tick().await;
                    // try_send: a full mailbox just skips one keep-warm tick (safe: it is a cache).
                    if !a.is_alive() {
                        break;
                    }
                    let _ = a.tell(RefreshHot).try_send();
                }
            });
        }
        if std::env::var("OA_BULK_TOPS")
            .map(|v| v != "off")
            .unwrap_or(true)
        {
            spawn_bulk_tops(actor.clone(), connector.clone(), metrics.clone());
        }
        Self {
            actor,
            connector,
            metrics,
        }
    }

    pub fn platform(&self) -> PlatformId {
        self.connector.platform()
    }

    pub async fn catalog(
        &self,
        underlying: &str,
        max_age: Duration,
        wait: Duration,
    ) -> Result<Arc<Vec<Instrument>>, VenueError> {
        let asked = Instant::now();
        let rx = self
            .actor
            .ask(GetCatalog {
                underlying: underlying.into(),
                max_age,
            })
            .mailbox_timeout(MAILBOX_TIMEOUT)
            .reply_timeout(REPLY_TIMEOUT)
            .await;
        self.metrics.observe("venue_ask_roundtrip", asked.elapsed());
        let rx = rx.map_err(|e| match e {
            SendError::HandlerError(v) => v,
            other => map_send(other),
        })?;
        await_fresh(rx, asked, max_age, wait).await
    }

    pub async fn book(
        &self,
        instrument: &Instrument,
        max_age: Duration,
        wait: Duration,
    ) -> Result<Arc<MarketData>, VenueError> {
        let asked = Instant::now();
        let rx = self
            .actor
            .ask(GetBook {
                instrument: instrument.clone(),
                max_age,
            })
            .mailbox_timeout(MAILBOX_TIMEOUT)
            .reply_timeout(REPLY_TIMEOUT)
            .await;
        self.metrics.observe("venue_ask_roundtrip", asked.elapsed());
        let rx = rx.map_err(|e| match e {
            SendError::HandlerError(v) => v,
            other => map_send(other),
        })?;
        await_fresh(rx, asked, max_age, wait).await
    }

    pub async fn health(&self) -> Option<VenueHealth> {
        self.actor
            .ask(GetHealth)
            .mailbox_timeout(MAILBOX_TIMEOUT)
            .reply_timeout(REPLY_TIMEOUT)
            .await
            .ok()
    }

    pub async fn stop(&self) {
        let _ = self.actor.stop_gracefully().await;
        self.actor.wait_for_shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{FixtureConnector, Inject};

    fn handle() -> (Arc<FixtureConnector>, VenueHandle) {
        let c = Arc::new(FixtureConnector::derive());
        let v = VenueHandle::spawn(c.clone(), Arc::new(Metrics::default()));
        (c, v)
    }

    /// A failed REFRESH keeps serving the last good catalog (it used to replace it with an error).
    #[tokio::test]
    async fn failed_catalog_refresh_keeps_the_last_good_list() {
        let (c, v) = handle();
        let first = v
            .catalog("ETH", Duration::from_secs(300), Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!first.is_empty());
        c.set_inject(Inject::Timeout);
        tokio::time::sleep(Duration::from_millis(120)).await;
        let again = v
            .catalog("ETH", Duration::from_millis(50), Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(first.len(), again.len());
    }

    /// A panicking connector yields an error and releases the slot: a later request fetches again.
    #[tokio::test]
    async fn panicking_fetch_does_not_wedge_the_slot() {
        let (c, v) = handle();
        let cat = v
            .catalog("ETH", Duration::from_secs(300), Duration::from_secs(2))
            .await
            .unwrap();
        let inst = cat
            .iter()
            .find(|i| i.key.native_id == "ETH-20261030-2800-C")
            .unwrap()
            .clone();
        c.set_inject(Inject::Panic);
        assert!(
            v.book(&inst, Duration::from_secs(10), Duration::from_secs(2))
                .await
                .is_err()
        );
        c.set_inject(Inject::None);
        tokio::time::sleep(NEGATIVE_CACHE + Duration::from_millis(100)).await;
        let md = v
            .book(&inst, Duration::from_secs(10), Duration::from_secs(3))
            .await;
        assert!(md.is_ok(), "slot stayed wedged: {md:?}");
    }

    /// Unknown / delisted instruments are item-level errors: they never open the venue's circuit.
    #[tokio::test]
    async fn unknown_instruments_do_not_open_the_circuit() {
        let (_c, v) = handle();
        let cat = v
            .catalog("ETH", Duration::from_secs(300), Duration::from_secs(2))
            .await
            .unwrap();
        for n in 0..(FAILURES_BEFORE_OPEN + 2) {
            let mut i = cat[0].clone();
            i.key.native_id = format!("ETH-NOPE-{n}");
            assert!(
                v.book(&i, Duration::from_secs(10), Duration::from_secs(2))
                    .await
                    .is_err()
            );
        }
        let h = v.health().await.unwrap();
        assert_ne!(h.state, "CIRCUIT_OPEN");
        assert_eq!(h.consecutive_failures, 0);
    }
}
