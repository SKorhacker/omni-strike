//! Persistent WebSocket for Binance European Options (public market streams, no key): partial-depth
//! book snapshots per symbol + mark/index per underlying, automatic reconnection, 23 h rotation.
//!
//! Protocol [DOC https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-options/api/ws-streams/public
//! and .../ws-streams/market; base URL constant `DERIVATIVES_TRADING_OPTIONS_WS_STREAMS_PROD_URL =
//! 'wss://fstream.binance.com'` and the `{base}/{public|market}/stream?streams=` composition in the
//! official `binance/binance-connector-js` (`common/src/constants.ts`, `common/src/websocket.ts`)],
//! all checked LIVE on 2026-10-07:
//! - The old host `wss://nbstream.binance.com/eoptions/ws` answers HTTP 404 (retired).
//! - Book streams are served ONLY on `/public/...`, mark/index ONLY on `/market/...` (subscribing a
//!   book stream on `/market` or a mark stream on `/public` is acknowledged but never sends data).
//! - Subscribe `{"method":"SUBSCRIBE","params":[..],"id":n}` -> `{"result":null,"id":n}`. Stream names
//!   use the LOWERCASE symbol (`btc-261008-83500-c@depth20@100ms`; uppercase gets nothing). An unknown
//!   symbol is ACKNOWLEDGED and simply never sends data (not detectable as a rejection). 200 streams
//!   in one request were acknowledged; 250 got no answer at all -> batches of `SUBSCRIBE_BATCH`.
//! - `<symbol>@depth<5|10|20>@<100ms|500ms>` (partial depth): `{"e":"depthUpdate","E":event ms,"T":
//!   transaction ms,"s":SYMBOL,"U","u","pu","b":[[price,qty]..],"a":[..]}`, prices USDT and sizes in
//!   contracts exactly like REST `/eapi/v1/depth`. Each message is the COMPLETE top 20, pushed ONLY
//!   WHEN THE BOOK CHANGES (a quiet book sends nothing, not even a first snapshot after subscribing).
//!   `u` is in the SAME id space as REST `lastUpdateId`; `pu` = previous `u` of the symbol. LIVE:
//!   25/25 REST snapshots whose `lastUpdateId` had been streamed matched the streamed top 20 exactly.
//! - `<underlying>@optionMarkPrice` (1 s): array of `{"s","mp" mark,"i" index,"E",..}` for EVERY
//!   option of the underlying (~216 KB per message for BTCUSDT).
//! - Keepalive: the official client answers server PINGs with PONG (tungstenite does it
//!   automatically) and renews each connection after 23 h (`MAX_CONNECTION_DURATION`).
//!
//! Implemented here:
//! - Books: `SHARDS` connections to `/public/stream` (stable hash per symbol), each bounded to
//!   `MAX_PER_SHARD` symbols (LRU evicted on subscribe), idle ones dropped after `IDLE_UNSUBSCRIBE`.
//!   Mark/index: one connection to `/market/stream`, one stream per wanted underlying.
//! - Ordering: a book message with `u <= last` is a duplicate/out-of-order and dropped; `pu != last u`
//!   is counted as a gap (the message is a full snapshot, so it still resynchronises).
//! - Quiet books: since nothing is pushed until a change, a book is SEEDED from a REST depth the
//!   connector fetched anyway (no extra REST weight), accepted only if the REST request was sent at
//!   least `SEED_MIN_DELAY_AFTER_ACK` after the subscription was acknowledged on the live connection
//!   (LIVE: REST depth lags the stream by up to ~0.56 s, so an earlier REST state could predate the
//!   subscription and miss a change) and only if its `lastUpdateId` is newer than the cached book.
//! - Freshness: changes are pushed (100 ms aggregation) and frames on one connection arrive in
//!   order, so a synchronised book is current as of the LAST frame received on its connection (data
//!   or the PONG of our `PING_EVERY` ping). It is served only while that connection has spoken within
//!   `MAX_SILENCE`, and `confirmed_at` (that frame's arrival) becomes `BookSnapshot.received_at`.
//! - On any disconnection (error, idle timeout, rotation) every cached book/mark of that connection is
//!   INVALIDATED; reconnection uses exponential backoff (none after a planned rotation) and restores
//!   all subscriptions.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const MAINNET_WS_BASE: &str = "wss://fstream.binance.com";
/// Partial depth levels (max documented: 5, 10, 20).
pub const BOOK_LEVELS: usize = 20;
const SHARDS: usize = 3;
/// Per-connection stream limit is not documented for options; 200 per request verified live.
const MAX_PER_SHARD: usize = 200;
const SUBSCRIBE_BATCH: usize = 100;
/// Spacing between consecutive SUBSCRIBE requests on one connection (message-rate courtesy).
const CONTROL_SPACING: Duration = Duration::from_millis(250);
const IDLE_UNSUBSCRIBE: Duration = Duration::from_secs(600);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A book is served only if its connection delivered a frame within this window.
pub const MAX_SILENCE: Duration = Duration::from_secs(5);
/// Mark/index pushes come every 1 s; older than this -> REST enrichment instead.
pub const MARK_MAX_AGE: Duration = Duration::from_secs(5);
pub const SEED_MIN_DELAY_AFTER_ACK: Duration = Duration::from_secs(2);

/// Timings (overridable in tests).
#[derive(Debug, Clone)]
pub struct Config {
    pub ping_every: Duration,
    /// No frame at all for this long = dead connection.
    pub idle_timeout: Duration,
    /// Planned reconnection (official client: 23 h; connections are valid 24 h).
    pub rotate_after: Duration,
    pub housekeeping_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(10),
            rotate_after: Duration::from_secs(23 * 3600),
            housekeeping_every: Duration::from_secs(5),
        }
    }
}

fn book_stream(symbol: &str) -> String {
    format!("{}@depth{BOOK_LEVELS}@100ms", symbol.to_lowercase())
}
fn mark_stream(underlying: &str) -> String {
    format!("{}@optionMarkPrice", underlying.to_lowercase())
}

fn shard_of(symbol: &str) -> usize {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    symbol.hash(&mut h);
    (h.finish() % SHARDS as u64) as usize
}

/// Index of the mark/index connection in `State::conns`.
const MARKET: usize = SHARDS;

#[derive(Debug, Clone)]
struct Book {
    /// REST-shaped `{"bids","asks","T"}` so `binance::parse_book` reads it unchanged.
    depth: Value,
    update_id: i64,
    from_seed: bool,
}

#[derive(Debug, Clone)]
struct Mark {
    mark: Value,
    index: Value,
    mono: Instant,
    received_at: DateTime<Utc>,
}

/// A servable book.
#[derive(Debug, Clone)]
pub struct BookView {
    /// `{"bids","asks","T"}`; `T` = venue event time `E` of the push (REST `T` for a seed).
    pub depth: Value,
    /// Arrival time of the last frame on the book's connection (the book is current as of then).
    pub confirmed_at: DateTime<Utc>,
    pub update_id: i64,
    pub from_seed: bool,
}

#[derive(Debug, Clone)]
pub struct MarkView {
    pub mark: Value,
    pub index: Value,
    pub received_at: DateTime<Utc>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StreamStats {
    pub connects: u64,
    pub disconnects: u64,
    pub rotations: u64,
    pub book_messages: u64,
    pub mark_messages: u64,
    pub sequence_gaps: u64,
    pub duplicates_or_out_of_order: u64,
    pub invalidations: u64,
    pub subscribe_errors: u64,
    pub seeds_accepted: u64,
    pub seeds_refused: u64,
    pub pings_sent: u64,
    pub pongs: u64,
    pub server_pings: u64,
    pub served_from_stream: u64,
    pub rest_fallbacks: u64,
    pub last_error: Option<String>,
    pub connections: Vec<ConnStats>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ConnStats {
    pub kind: &'static str,
    pub connected: bool,
    pub subscribed: usize,
    pub acknowledged: usize,
    pub cached: usize,
    pub last_frame_age_ms: Option<u128>,
}

#[derive(Default)]
struct Conn {
    connected: bool,
    last_frame: Option<(Instant, DateTime<Utc>)>,
    /// key (SYMBOL or UNDERLYING, uppercase) -> last use
    wanted: HashMap<String, Instant>,
    /// key -> acknowledgement time on the CURRENT connection
    acked: HashMap<String, Instant>,
    /// request id -> keys
    pending: HashMap<u64, Vec<String>>,
    books: HashMap<String, Book>,
    marks: HashMap<String, Mark>,
}

impl Conn {
    fn live(&self) -> Option<DateTime<Utc>> {
        match self.last_frame {
            Some((m, at)) if self.connected && m.elapsed() <= MAX_SILENCE => Some(at),
            _ => None,
        }
    }
}

struct State {
    conns: Vec<Conn>,
    stats: StreamStats,
}

enum Cmd {
    Subscribe(String),
    Unsubscribe(String),
}

pub struct BinanceStream {
    state: Arc<Mutex<State>>,
    cmds: Vec<mpsc::UnboundedSender<Cmd>>,
}

impl BinanceStream {
    pub fn spawn(base: &str) -> Arc<Self> {
        Self::spawn_with(base, Config::default())
    }

    /// `base` = `wss://fstream.binance.com` (paths `/public/stream` and `/market/stream` appended).
    pub fn spawn_with(base: &str, cfg: Config) -> Arc<Self> {
        let base = base.trim_end_matches('/');
        let state = Arc::new(Mutex::new(State {
            conns: (0..=SHARDS).map(|_| Conn::default()).collect(),
            stats: StreamStats::default(),
        }));
        let mut cmds = Vec::new();
        for idx in 0..=SHARDS {
            let (tx, rx) = mpsc::unbounded_channel();
            cmds.push(tx);
            let url = if idx == MARKET {
                format!("{base}/market/stream")
            } else {
                format!("{base}/public/stream")
            };
            tokio::spawn(run(idx, url, cfg.clone(), state.clone(), rx));
        }
        Arc::new(Self { state, cmds })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("binance stream state")
    }

    /// Mark `symbol` (and its underlying's mark/index stream, e.g. "BTCUSDT") as wanted; subscribes on
    /// first use, refreshes the LRU time otherwise. Never waits.
    pub fn subscribe(&self, symbol: &str, underlying: &str) {
        let (sym, und) = (symbol.to_uppercase(), underlying.to_uppercase());
        let mut st = self.lock();
        let shard = shard_of(&sym);
        touch(&mut st.conns[shard], &sym, &self.cmds[shard], MAX_PER_SHARD);
        touch(&mut st.conns[MARKET], &und, &self.cmds[MARKET], usize::MAX);
    }

    /// Latest synchronised book, if its connection is alive and spoke within `MAX_SILENCE`.
    pub fn book(&self, symbol: &str) -> Option<BookView> {
        let sym = symbol.to_uppercase();
        let st = self.lock();
        let c = &st.conns[shard_of(&sym)];
        let confirmed_at = c.live()?;
        c.books.get(&sym).map(|b| BookView {
            depth: b.depth.clone(),
            confirmed_at,
            update_id: b.update_id,
            from_seed: b.from_seed,
        })
    }

    /// Latest mark/index of `symbol` if received within `MARK_MAX_AGE` on a live connection.
    pub fn mark(&self, symbol: &str) -> Option<MarkView> {
        let st = self.lock();
        let c = &st.conns[MARKET];
        if !c.connected {
            return None;
        }
        c.marks
            .get(&symbol.to_uppercase())
            .filter(|m| m.mono.elapsed() <= MARK_MAX_AGE)
            .map(|m| MarkView {
                mark: m.mark.clone(),
                index: m.index.clone(),
                received_at: m.received_at,
            })
    }

    /// Offer a REST `/eapi/v1/depth` answer (request sent at `sent_at`) as the baseline of a quiet
    /// book. Returns true if accepted (see module docs for the rules).
    pub fn offer_seed(&self, symbol: &str, depth: &Value, sent_at: Instant) -> bool {
        let sym = symbol.to_uppercase();
        let mut st = self.lock();
        let ok = seed(&mut st.conns[shard_of(&sym)], &sym, depth, sent_at);
        if ok {
            st.stats.seeds_accepted += 1;
        } else {
            st.stats.seeds_refused += 1;
        }
        ok
    }

    pub fn note_served(&self, from_stream: bool) {
        let mut st = self.lock();
        if from_stream {
            st.stats.served_from_stream += 1;
        } else {
            st.stats.rest_fallbacks += 1;
        }
    }

    pub fn stats(&self) -> StreamStats {
        let st = self.lock();
        let mut s = st.stats.clone();
        s.connections = st
            .conns
            .iter()
            .enumerate()
            .map(|(i, c)| ConnStats {
                kind: if i == MARKET { "market" } else { "books" },
                connected: c.connected,
                subscribed: c.wanted.len(),
                acknowledged: c.acked.len(),
                cached: if i == MARKET {
                    c.marks.len()
                } else {
                    c.books.len()
                },
                last_frame_age_ms: c.last_frame.map(|(m, _)| m.elapsed().as_millis()),
            })
            .collect();
        s
    }
}

fn touch(c: &mut Conn, key: &str, tx: &mpsc::UnboundedSender<Cmd>, cap: usize) {
    if let Some(t) = c.wanted.get_mut(key) {
        *t = Instant::now();
        return;
    }
    while c.wanted.len() >= cap {
        let Some(oldest) = c.wanted.iter().min_by_key(|x| *x.1).map(|x| x.0.clone()) else {
            break;
        };
        forget(c, &oldest);
        let _ = tx.send(Cmd::Unsubscribe(oldest));
    }
    c.wanted.insert(key.to_string(), Instant::now());
    let _ = tx.send(Cmd::Subscribe(key.to_string()));
}

fn forget(c: &mut Conn, key: &str) {
    c.wanted.remove(key);
    c.acked.remove(key);
    c.books.remove(key);
}

fn seed(c: &mut Conn, sym: &str, depth: &Value, sent_at: Instant) -> bool {
    if !c.connected || !c.wanted.contains_key(sym) {
        return false;
    }
    match c.acked.get(sym) {
        Some(a) if sent_at >= *a + SEED_MIN_DELAY_AFTER_ACK => {}
        _ => return false,
    }
    let Some(l) = depth["lastUpdateId"].as_i64() else {
        return false;
    };
    if c.books.get(sym).is_some_and(|b| b.update_id >= l) {
        return false;
    }
    let side = |k: &str| match depth[k].as_array() {
        Some(a) => Value::Array(a.iter().take(BOOK_LEVELS).cloned().collect()),
        None => Value::Null,
    };
    if !depth["T"].is_i64() || side("bids").is_null() || side("asks").is_null() {
        return false;
    }
    c.books.insert(
        sym.to_string(),
        Book {
            depth: json!({"bids": side("bids"), "asks": side("asks"), "T": depth["T"]}),
            update_id: l,
            from_seed: true,
        },
    );
    true
}

/// Partial-depth payload -> REST-shaped depth (`T` = event time `E`). None if malformed.
pub fn normalize_depth(d: &Value) -> Option<(String, i64, Option<i64>, Value)> {
    let sym = d["s"].as_str()?.to_uppercase();
    let u = d["u"].as_i64()?;
    if !d["b"].is_array() || !d["a"].is_array() || !d["E"].is_i64() {
        return None;
    }
    Some((
        sym,
        u,
        d["pu"].as_i64(),
        json!({"bids": d["b"], "asks": d["a"], "T": d["E"]}),
    ))
}

/// Apply one text frame of connection `idx`.
fn apply(idx: usize, st: &mut State, v: &Value, now: Instant) {
    let State { conns, stats } = st;
    let c = &mut conns[idx];
    if v.get("stream").is_none() {
        // Request answer: {"result":null,"id":n} (or an error object).
        if let Some(keys) = v["id"].as_u64().and_then(|id| c.pending.remove(&id)) {
            if v.get("error").is_some_and(|e| !e.is_null()) {
                stats.subscribe_errors += 1;
                stats.last_error = Some(format!("subscribe error: {}", v["error"]));
                for k in keys {
                    forget(c, &k);
                }
            } else {
                for k in keys {
                    if c.wanted.contains_key(&k) {
                        c.acked.insert(k, now);
                    }
                }
            }
        }
        return;
    }
    let data = &v["data"];
    if idx == MARKET {
        let Some(rows) = data.as_array() else { return };
        stats.mark_messages += 1;
        let at = Utc::now();
        for r in rows {
            if r["e"] != "markPrice" {
                continue;
            }
            if let Some(s) = r["s"].as_str() {
                c.marks.insert(
                    s.to_string(),
                    Mark {
                        mark: r["mp"].clone(),
                        index: r["i"].clone(),
                        mono: now,
                        received_at: at,
                    },
                );
            }
        }
        return;
    }
    if data["e"] != "depthUpdate" {
        return;
    }
    let Some((sym, u, pu, depth)) = normalize_depth(data) else {
        stats.last_error = Some("malformed depthUpdate".into());
        return;
    };
    if !c.wanted.contains_key(&sym) {
        return; // late message after an unsubscribe
    }
    stats.book_messages += 1;
    if let Some(prev) = c.books.get(&sym) {
        if u <= prev.update_id {
            stats.duplicates_or_out_of_order += 1;
            return;
        }
        if !prev.from_seed && pu != Some(prev.update_id) {
            stats.sequence_gaps += 1;
            tracing::debug!(symbol = %sym, prev = prev.update_id, ?pu, u, "binance book gap (full snapshot resyncs)");
        }
    }
    c.books.insert(
        sym,
        Book {
            depth,
            update_id: u,
            from_seed: false,
        },
    );
}

fn stream_name(idx: usize, key: &str) -> String {
    if idx == MARKET {
        mark_stream(key)
    } else {
        book_stream(key)
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Send SUBSCRIBE/UNSUBSCRIBE in batches, registering subscribe ids as pending.
async fn request(
    ws: &mut Ws,
    idx: usize,
    state: &Mutex<State>,
    next_id: &mut u64,
    method: &str,
    keys: Vec<String>,
) -> bool {
    for (n, chunk) in keys.chunks(SUBSCRIBE_BATCH).enumerate() {
        if n > 0 {
            tokio::time::sleep(CONTROL_SPACING).await;
        }
        *next_id += 1;
        let params: Vec<String> = chunk.iter().map(|k| stream_name(idx, k)).collect();
        if method == "SUBSCRIBE" {
            state.lock().expect("binance stream state").conns[idx]
                .pending
                .insert(*next_id, chunk.to_vec());
        }
        let m = json!({"method": method, "params": params, "id": *next_id});
        if ws.send(Message::text(m.to_string())).await.is_err() {
            return false;
        }
    }
    true
}

async fn run(
    idx: usize,
    url: String,
    cfg: Config,
    state: Arc<Mutex<State>>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut next_id: u64 = 0;
    let lock = || state.lock().expect("binance stream state");
    loop {
        while lock().conns[idx].wanted.is_empty() {
            if rx.recv().await.is_none() {
                return;
            }
        }
        let conn = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await;
        let mut ws = match conn {
            Ok(Ok((ws, _))) => ws,
            other => {
                lock().stats.last_error = Some(match other {
                    Ok(Err(e)) => format!("connect {url}: {e}"),
                    _ => format!("connect {url}: timeout"),
                });
                tokio::time::sleep(crate::http::jittered(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let keys: Vec<String> = {
            let mut st = lock();
            st.stats.connects += 1;
            let c = &mut st.conns[idx];
            c.connected = true;
            c.last_frame = Some((Instant::now(), Utc::now()));
            c.wanted.keys().cloned().collect()
        };
        // Commands queued while disconnected are covered by this full resubscription.
        while rx.try_recv().is_ok() {}
        let started = Instant::now();
        let mut ok = request(&mut ws, idx, &state, &mut next_id, "SUBSCRIBE", keys).await;
        let mut rotated = false;
        let mut ping = tokio::time::interval(cfg.ping_every);
        let mut housekeeping = tokio::time::interval(cfg.housekeeping_every);
        ping.tick().await;
        housekeeping.tick().await;
        // Dead-connection detection: the idle clock is reset ONLY by a received frame (the per-await
        // timeout below restarts whenever another branch fires, e.g. every ping, so on its own it never
        // caught a half-open connection whose pings still enter the kernel send buffer).
        let mut last_frame = Instant::now();
        while ok {
            tokio::select! {
                msg = tokio::time::timeout(cfg.idle_timeout, ws.next()) => {
                    let now = Instant::now();
                    if let Ok(Some(Ok(_))) = &msg {
                        last_frame = now;
                        lock().conns[idx].last_frame = Some((now, Utc::now()));
                    }
                    match msg {
                        Ok(Some(Ok(Message::Text(t)))) => {
                            backoff = Duration::from_millis(500);
                            if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                                apply(idx, &mut lock(), &v, now);
                            }
                        }
                        // tungstenite queues the PONG reply itself (flushed on the next read/write).
                        Ok(Some(Ok(Message::Ping(_)))) => lock().stats.server_pings += 1,
                        Ok(Some(Ok(Message::Pong(_)))) => lock().stats.pongs += 1,
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => ok = false,
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            lock().stats.last_error = Some(e.to_string());
                            ok = false;
                        }
                        Err(_) => {
                            lock().stats.last_error = Some(format!("idle timeout (no frame for {:?})", cfg.idle_timeout));
                            ok = false;
                        }
                    }
                }
                cmd = rx.recv() => {
                    let Some(first) = cmd else { return };
                    let (mut subs, mut unsubs) = (Vec::new(), Vec::new());
                    let mut next = Some(first);
                    while let Some(c) = next {
                        match c {
                            Cmd::Subscribe(k) => subs.push(k),
                            Cmd::Unsubscribe(k) => unsubs.push(k),
                        }
                        next = rx.try_recv().ok();
                    }
                    let subs: Vec<String> = {
                        let st = lock();
                        let c = &st.conns[idx];
                        let mut v: Vec<String> = subs.into_iter()
                            .filter(|k| c.wanted.contains_key(k) && !c.acked.contains_key(k)
                                && !c.pending.values().any(|p| p.contains(k)))
                            .collect();
                        v.sort();
                        v.dedup();
                        v
                    };
                    if !unsubs.is_empty() {
                        ok = request(&mut ws, idx, &state, &mut next_id, "UNSUBSCRIBE", unsubs).await;
                    }
                    if ok && !subs.is_empty() {
                        ok = request(&mut ws, idx, &state, &mut next_id, "SUBSCRIBE", subs).await;
                    }
                }
                _ = ping.tick() => {
                    if last_frame.elapsed() > cfg.idle_timeout {
                        lock().stats.last_error = Some(format!("idle timeout (no frame for {:?})", cfg.idle_timeout));
                        ok = false;
                    } else {
                        ok = ws.send(Message::Ping(Vec::new().into())).await.is_ok();
                        lock().stats.pings_sent += 1;
                    }
                }
                _ = housekeeping.tick() => {
                    if started.elapsed() >= cfg.rotate_after {
                        lock().stats.rotations += 1;
                        let _ = ws.close(None).await;
                        rotated = true;
                        ok = false;
                        continue;
                    }
                    let idle: Vec<String> = {
                        let mut st = lock();
                        let c = &mut st.conns[idx];
                        let idle: Vec<String> = c.wanted.iter()
                            .filter(|(_, t)| t.elapsed() > IDLE_UNSUBSCRIBE)
                            .map(|(k, _)| k.clone()).collect();
                        for k in &idle {
                            forget(c, k);
                            if idx == MARKET {
                                c.marks.retain(|s, _| !s.starts_with(k.trim_end_matches("USDT")));
                            }
                        }
                        idle
                    };
                    if !idle.is_empty() {
                        ok = request(&mut ws, idx, &state, &mut next_id, "UNSUBSCRIBE", idle).await;
                    }
                }
            }
        }
        {
            let mut st = lock();
            let State { conns, stats } = &mut *st;
            let c = &mut conns[idx];
            c.connected = false;
            stats.disconnects += 1;
            if !c.books.is_empty() || !c.marks.is_empty() {
                stats.invalidations += 1;
            }
            c.books.clear();
            c.marks.clear();
            c.acked.clear();
            c.pending.clear();
        }
        if rotated {
            continue;
        }
        tracing::warn!(
            ?backoff,
            url,
            "binance options websocket disconnected; reconnecting"
        );
        tokio::time::sleep(crate::http::jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    /// Recorded live 2026-10-07 on `wss://fstream.binance.com/public/stream` (levels trimmed).
    const DEPTH_MSG: &str = r#"{"stream":"btc-261008-83500-p@depth10@100ms","data":{"e":"depthUpdate","E":1791374653874,"T":1791374653824,"s":"BTC-261008-83500-P","U":42957795864,"u":42957795955,"pu":42957792728,"b":[["470.000","30.00"],["465.000","16.37"],["460.000","15.27"]],"a":[["475.000","9.76"],["480.000","24.06"],["485.000","9.36"]]}}"#;
    const DEPTH_MSG_NEXT: &str = r#"{"stream":"btc-261008-83500-p@depth10@100ms","data":{"e":"depthUpdate","E":1791374653989,"T":1791374653915,"s":"BTC-261008-83500-P","U":42957795996,"u":42957796020,"pu":42957795955,"b":[["470.000","31.38"],["465.000","16.37"]],"a":[["475.000","9.76"],["480.000","16.06"]]}}"#;
    /// Recorded live 2026-10-07 on `wss://fstream.binance.com/market/stream` (one row kept).
    const MARK_MSG: &str = r#"{"stream":"btcusdt@optionMarkPrice","data":[{"s":"BTC-261225-92000-C","mp":"2980.849","E":1791374757964,"e":"markPrice","i":"83656.94152174","P":"0.000","bo":"3000.000","ao":"3010.000","bq":"2.20","aq":"14.22","b":"0.36934078","a":"0.37004054","hl":"5365.000","ll":"600.000","vo":"0.368","rf":"0.0502","d":"0.34201726","t":"-33.47855445","g":"0.00002555","v":"142.77273659"}]}"#;
    const SYM: &str = "BTC-261008-83500-P";

    fn p(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
    fn state() -> State {
        State {
            conns: (0..=SHARDS).map(|_| Conn::default()).collect(),
            stats: StreamStats::default(),
        }
    }

    #[test]
    fn recorded_depth_parses_like_rest() {
        let (sym, u, pu, depth) = normalize_depth(&p(DEPTH_MSG)["data"]).unwrap();
        assert_eq!((sym.as_str(), u, pu), (SYM, 42957795955, Some(42957792728)));
        let inst = crate::binance::parse_instruments(
            &json!({"optionSymbols": [{"symbol": SYM, "underlying": "BTCUSDT", "quoteAsset": "USDT",
                "status": "TRADING", "side": "PUT", "expiryDate": 1791403200000i64, "strikePrice": "83500",
                "unit": 1, "filters": []}]}),
            "BTC",
            "2026-10-07T00:00:00Z".parse().unwrap(),
        )
        .unwrap()
        .remove(0);
        let b = crate::binance::parse_book(
            &inst,
            &depth,
            Some(&json!({"indexPrice": "83656.94152174"})),
            Some(&json!([{"symbol": SYM, "markPrice": "471.5"}])),
            Utc::now(),
            oa_core::domain::Provenance::LiveData,
        )
        .unwrap();
        assert_eq!(b.bids[0].price.to_string(), "470.000");
        assert_eq!(b.asks[0].size.to_string(), "9.76");
        assert_eq!(b.source_ts.unwrap().timestamp_millis(), 1791374653874); // event time E
    }

    #[test]
    fn sequence_duplicates_and_gaps() {
        let mut st = state();
        let i = shard_of(SYM);
        st.conns[i].wanted.insert(SYM.into(), Instant::now());
        let now = Instant::now();
        apply(i, &mut st, &p(DEPTH_MSG), now);
        apply(i, &mut st, &p(DEPTH_MSG_NEXT), now); // pu == previous u: contiguous
        assert_eq!(st.stats.sequence_gaps, 0);
        apply(i, &mut st, &p(DEPTH_MSG), now); // older u
        assert_eq!(st.stats.duplicates_or_out_of_order, 1);
        assert_eq!(st.conns[i].books[SYM].update_id, 42957796020);
        let mut gap = p(DEPTH_MSG_NEXT);
        gap["data"]["u"] = json!(42957799999i64);
        gap["data"]["pu"] = json!(42957799000i64);
        apply(i, &mut st, &gap, now);
        assert_eq!(st.stats.sequence_gaps, 1);
        assert_eq!(st.conns[i].books[SYM].update_id, 42957799999);
        // Unwanted symbol (unsubscribed): ignored.
        let mut other = p(DEPTH_MSG);
        other["data"]["s"] = json!("ETH-261008-2500-C");
        apply(shard_of("ETH-261008-2500-C"), &mut st, &other, now);
        assert!(
            !st.conns[shard_of("ETH-261008-2500-C")]
                .books
                .contains_key("ETH-261008-2500-C")
        );
    }

    #[test]
    fn recorded_mark_rows_are_cached() {
        let mut st = state();
        apply(MARKET, &mut st, &p(MARK_MSG), Instant::now());
        let m = &st.conns[MARKET].marks["BTC-261225-92000-C"];
        assert_eq!(
            (m.mark.as_str(), m.index.as_str()),
            (Some("2980.849"), Some("83656.94152174"))
        );
    }

    #[test]
    fn ack_and_seed_rules() {
        let mut st = state();
        let i = shard_of(SYM);
        let t0 = Instant::now();
        {
            let c = &mut st.conns[i];
            c.connected = true;
            c.wanted.insert(SYM.into(), t0);
            c.pending.insert(7, vec![SYM.into()]);
        }
        let rest = json!({"bids": [["1.0","2"]], "asks": [], "T": 1791375124603i64, "lastUpdateId": 42957795955i64});
        assert!(
            !seed(&mut st.conns[i], SYM, &rest, t0),
            "not acknowledged yet"
        );
        apply(i, &mut st, &json!({"result": null, "id": 7}), t0);
        assert!(st.conns[i].acked.contains_key(SYM));
        assert!(
            !seed(
                &mut st.conns[i],
                SYM,
                &rest,
                t0 + Duration::from_millis(500)
            ),
            "REST sent too soon after the ack"
        );
        let later = t0 + SEED_MIN_DELAY_AFTER_ACK;
        assert!(seed(&mut st.conns[i], SYM, &rest, later));
        assert!(st.conns[i].books[SYM].from_seed);
        // A stream push with a newer id replaces the seed (no gap counted against a seed)...
        apply(i, &mut st, &p(DEPTH_MSG_NEXT), later);
        assert!(!st.conns[i].books[SYM].from_seed);
        assert_eq!(st.stats.sequence_gaps, 0);
        // ...and an older REST answer never overwrites a newer streamed book.
        assert!(!seed(&mut st.conns[i], SYM, &rest, later));
        // Subscribe error: forgotten.
        st.conns[i].pending.insert(8, vec![SYM.into()]);
        apply(
            i,
            &mut st,
            &json!({"error": {"code": 2, "msg": "Invalid request"}, "id": 8}),
            later,
        );
        assert!(!st.conns[i].wanted.contains_key(SYM) && st.stats.subscribe_errors == 1);
    }

    #[test]
    fn lru_bound_evicts_oldest() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut c = Conn::default();
        touch(&mut c, "A", &tx, 2);
        std::thread::sleep(Duration::from_millis(2));
        touch(&mut c, "B", &tx, 2);
        touch(&mut c, "C", &tx, 2);
        assert!(!c.wanted.contains_key("A") && c.wanted.len() == 2);
        let mut unsub = vec![];
        while let Ok(m) = rx.try_recv() {
            if let Cmd::Unsubscribe(k) = m {
                unsub.push(k);
            }
        }
        assert_eq!(unsub, vec!["A".to_string()]);
    }

    /// Local fake server for both paths: the mark connection gets a mark row each 200 ms; the book
    /// connection gets an ack + one book, a server PING whose PONG must come back, then (round 0)
    /// is dropped. The client must invalidate, reconnect, resubscribe and serve only the new book.
    #[tokio::test]
    async fn fake_server_subscribe_ping_pong_reconnect_invalidate() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", l.local_addr().unwrap());
        let book_rounds = Arc::new(AtomicUsize::new(0));
        let pongs_seen = Arc::new(AtomicUsize::new(0));
        let (br, ps) = (book_rounds.clone(), pongs_seen.clone());
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let (br, ps) = (br.clone(), ps.clone());
                tokio::spawn(async move {
                    let mut ws = accept_async(s).await.unwrap();
                    let req: Value = loop {
                        match ws.next().await {
                            Some(Ok(Message::Text(t))) => {
                                break serde_json::from_str(t.as_str()).unwrap();
                            }
                            Some(Ok(_)) => continue,
                            _ => return,
                        }
                    };
                    assert_eq!(req["method"], "SUBSCRIBE");
                    let params = req["params"].to_string();
                    ws.send(Message::text(
                        json!({"result": null, "id": req["id"]}).to_string(),
                    ))
                    .await
                    .unwrap();
                    if params.contains("@optionMarkPrice") {
                        assert!(params.contains("btcusdt@optionMarkPrice"));
                        loop {
                            let m = MARK_MSG.replace("BTC-261225-92000-C", SYM);
                            if ws.send(Message::text(m)).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    }
                    assert!(
                        params.contains("btc-261008-83500-p@depth20@100ms"),
                        "{params}"
                    );
                    let round = br.fetch_add(1, Ordering::SeqCst);
                    let mut m = p(DEPTH_MSG);
                    m["data"]["b"][0][0] = json!(if round == 0 { "100.000" } else { "200.000" });
                    ws.send(Message::text(m.to_string())).await.unwrap();
                    ws.send(Message::Ping(b"hb".to_vec().into())).await.unwrap();
                    let deadline = Instant::now()
                        + Duration::from_millis(if round == 0 { 600 } else { 10_000 });
                    while Instant::now() < deadline {
                        match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
                            Ok(Some(Ok(Message::Pong(d)))) if d.as_ref() == b"hb" => {
                                ps.fetch_add(1, Ordering::SeqCst);
                            }
                            Ok(None) | Ok(Some(Err(_))) => return,
                            _ => {}
                        }
                    }
                    let _ = ws.close(None).await;
                });
            }
        });
        let cfg = Config {
            ping_every: Duration::from_millis(300),
            idle_timeout: Duration::from_secs(3),
            ..Config::default()
        };
        let s = BinanceStream::spawn_with(&base, cfg);
        s.subscribe(SYM, "BTCUSDT");
        let wait_book = |want: &'static str| {
            let s = s.clone();
            async move {
                for _ in 0..60 {
                    if let Some(b) = s.book(SYM)
                        && b.depth["bids"][0][0] == want
                    {
                        return b;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("book {want} never served; {:?}", s.stats());
            }
        };
        let b = wait_book("100.000").await;
        assert!(!b.from_seed);
        assert!(
            s.mark(SYM).is_some(),
            "mark/index from the market connection"
        );
        // After the drop: never the old book; the new one after reconnect.
        let mut seen_new = false;
        for _ in 0..80 {
            if let Some(b) = s.book(SYM) {
                if b.depth["bids"][0][0] == "200.000" {
                    seen_new = true;
                    break;
                }
                assert_eq!(
                    book_rounds.load(Ordering::SeqCst),
                    1,
                    "old book served after the drop"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(seen_new, "{:?}", s.stats());
        tokio::time::sleep(Duration::from_millis(400)).await;
        let st = s.stats();
        assert!(
            pongs_seen.load(Ordering::SeqCst) >= 1,
            "client answered the server PING"
        );
        assert!(
            st.connects >= 3 && st.disconnects >= 1 && st.invalidations >= 1,
            "{st:?}"
        );
        assert!(
            st.pongs >= 1 && st.pings_sent >= 1 && st.server_pings >= 1,
            "{st:?}"
        );
    }

    /// Planned rotation: the connection is closed and reopened without backoff, books invalidated.
    #[tokio::test]
    async fn rotation_reconnects() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let Ok(mut ws) = accept_async(s).await else {
                        return;
                    };
                    while let Some(Ok(m)) = ws.next().await {
                        if let Message::Text(t) = m {
                            let req: Value = serde_json::from_str(t.as_str()).unwrap();
                            let _ = ws
                                .send(Message::text(
                                    json!({"result": null, "id": req["id"]}).to_string(),
                                ))
                                .await;
                            if req["params"].to_string().contains("@depth20") {
                                let _ = ws.send(Message::text(DEPTH_MSG)).await;
                            }
                        }
                    }
                });
            }
        });
        let cfg = Config {
            rotate_after: Duration::from_millis(300),
            housekeeping_every: Duration::from_millis(100),
            ..Config::default()
        };
        let s = BinanceStream::spawn_with(&base, cfg);
        s.subscribe(SYM, "BTCUSDT");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let st = s.stats();
        assert!(st.rotations >= 2 && st.connects >= 4, "{st:?}");
        assert_eq!(st.last_error, None, "{st:?}");
    }
}
