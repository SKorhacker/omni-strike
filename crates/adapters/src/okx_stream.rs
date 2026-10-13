//! Persistent OKX public WebSocket for option books: ONE connection, on-demand subscriptions, a
//! locally maintained full-depth book per instrument, automatic reconnection.
//!
//! Verified live 2026-10-07 on `wss://ws.okx.com:8443/ws/v5/public` (frames recorded in
//! `fixtures/recorded-2026-10-07/okx_ws_books.json`):
//! - Subscribe `{"op":"subscribe","args":[{"channel":"books","instId":..}, ..]}`; each arg is acked
//!   `{"event":"subscribe","arg":{..},"connId":..}`. An unknown `instId` answers
//!   `{"event":"error","code":"60018","msg":"Subscribe failed, wrong URL or channel:books,instId:X doesn't exist. ..."}`
//!   (no `arg`: the instrument is read from `msg`); the other subscriptions keep working.
//! - `books` (up to 400 levels): first push `action:"snapshot"` (`prevSeqId:-1`), then
//!   `action:"update"` with ABSOLUTE level sizes (`sz "0"` = delete) and `prevSeqId` = the previous
//!   message's `seqId` (seqIds are not contiguous: 7106464 -> 7106468 is a normal step). With no
//!   change for ~30 s the server pushes an empty update with `prevSeqId == seqId` (heartbeat).
//!   Levels are `[px, sz, "0", n_orders]` exactly like REST `/market/books`.
//! - `checksum` is present but **0 on every message** (options AND spot BTC-USDT, 72/72 messages):
//!   OKX does not currently provide a usable checksum. We verify it (CRC32 of the top 25 levels,
//!   interleaved `bidPx:bidSz:askPx:askSz:..`, signed) only when it is non-zero; integrity relies on
//!   the strict `prevSeqId` chain + a crossed-book check. UNVERIFIED: the CRC string format could
//!   not be checked against a real non-zero checksum.
//! - `unsubscribe` then `subscribe` on the same connection yields a fresh `snapshot` (used to resync).
//! - `index-tickers` (`instId` BTC-USD): `idxPx`, `ts`, pushed every ~0.1-1 s.
//!   `mark-price` (`instId` option): `markPx` in SCIENTIFIC notation (`9.857022996959E-4`), ~1/s;
//!   normalised here to a plain decimal string (REST sends plain decimals).
//! - Keepalive: text `ping` -> text `pong`. The server drops a connection silent for 30 s.
//! - Demo trading: `wss://wspap.okx.com:8443/ws/v5/public` with header `x-simulated-trading: 1`
//!   [DOC https://www.okx.com/docs-v5/en/#overview-websocket]; connected and streamed on 2026-10-07.
//!
//! Integrity rules:
//! - Update whose `prevSeqId` != our `seqId`, checksum mismatch (when non-zero), crossed book or an
//!   unparseable level: the book is dropped at once (never served) and resynced (unsubscribe +
//!   subscribe -> new snapshot). Updates before a snapshot are ignored.
//! - On any disconnection every book/index/mark is INVALIDATED; reconnection uses exponential
//!   backoff and restores all subscriptions.
//! - Freshness: the book is incremental, so a quiet but correct book can go ~30 s without a book
//!   message. A book is served only while the connection is live (last frame of ANY channel within
//!   `max_age`) and the book itself had a message (update or heartbeat) within `BOOK_SILENCE_MAX`.
//!   `confirmed_at` (put in `BookSnapshot.received_at`) = arrival time of the last frame on the
//!   connection: the in-sequence book is known current as of that instant. `source_ts` = the
//!   venue `ts` of the last book message.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const MAINNET_WS: &str = "wss://ws.okx.com:8443/ws/v5/public";
pub const DEMO_WS: &str = "wss://wspap.okx.com:8443/ws/v5/public";
const IDLE_UNSUBSCRIBE: Duration = Duration::from_secs(600);
const MAX_INSTRUMENTS: usize = 400;
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Args per subscribe request (keeps each request far below OKX's request size limit).
const SUB_CHUNK: usize = 100;
/// Heartbeats were observed after ~30-35 s without change: a book silent for longer is not served.
const BOOK_SILENCE_MAX: Duration = Duration::from_secs(60);
/// Number of levels per side covered by the checksum.
const CHECKSUM_LEVELS: usize = 25;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Send `ping` after this long without any frame; a second silent period = dead connection.
    pub ping_after: Duration,
    pub housekeeping: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_after: Duration::from_secs(10),
            housekeeping: Duration::from_secs(30),
        }
    }
}

/// One side: price -> raw level `[px, sz, "0", n]` (strings verbatim, as REST).
type Side = BTreeMap<Decimal, Value>;

#[derive(Debug, Clone)]
struct Book {
    bids: Side,
    asks: Side,
    seq: i64,
    ts: Value,
    last_msg: Instant,
}

impl Book {
    /// Same shape as one element of REST `/market/books` data: bids descending, asks ascending.
    fn to_rest_shape(&self) -> Value {
        json!({
            "bids": self.bids.values().rev().cloned().collect::<Vec<_>>(),
            "asks": self.asks.values().cloned().collect::<Vec<_>>(),
            "ts": self.ts,
            "seqId": self.seq,
        })
    }
    fn crossed(&self) -> bool {
        matches!((self.bids.keys().next_back(), self.asks.keys().next()), (Some(b), Some(a)) if b >= a)
    }
}

#[derive(Debug, Clone)]
struct Entry {
    data: Value,
    mono: Instant,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StreamStats {
    pub connected: bool,
    pub connects: u64,
    pub disconnects: u64,
    pub messages: u64,
    pub book_snapshots: u64,
    pub book_updates: u64,
    pub heartbeats: u64,
    pub updates_before_snapshot: u64,
    pub sequence_breaks: u64,
    pub checksum_mismatches: u64,
    /// Book messages whose `checksum` was 0 (not verifiable).
    pub checksum_absent: u64,
    pub crossed_books: u64,
    pub invalid_levels: u64,
    pub resyncs: u64,
    pub invalidations: u64,
    pub subscribe_rejections: u64,
    pub pings: u64,
    pub subscribed_instruments: usize,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct State {
    books: HashMap<String, Book>,
    /// index instId (e.g. BTC-USD) -> latest index ticker
    index: HashMap<String, Entry>,
    marks: HashMap<String, Entry>,
    /// instrument -> (last use, index instId)
    wanted: HashMap<String, (Instant, String)>,
    rejected: HashMap<String, String>,
    /// Books to resubscribe (drained by the connection task).
    resync: Vec<String>,
    /// Arrival of the last frame on the CURRENT connection (None when disconnected).
    last_frame: Option<(DateTime<Utc>, Instant)>,
    stats: StreamStats,
}

enum Cmd {
    Subscribe,
}

pub struct OkxStream {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

/// A book ready to be parsed by `okx::parse_book` (REST-identical shapes).
#[derive(Debug, Clone)]
pub struct View {
    /// `{bids, asks, ts, seqId}` like one element of `/market/books` data.
    pub book: Value,
    /// One `index-tickers` element (fresh), if any.
    pub index: Option<Value>,
    /// One `mark-price` element (fresh, plain-decimal `markPx`), if any.
    pub mark: Option<Value>,
    /// Arrival of the last frame on the live connection (the book is current as of then).
    pub confirmed_at: DateTime<Utc>,
}

pub enum Snapshot {
    Ready(View),
    /// The platform refused the subscription (unknown instrument).
    Rejected(String),
    /// Not subscribed before, still subscribing, resyncing, disconnected or stale.
    NotYet,
}

impl OkxStream {
    pub fn spawn(url: &str, demo: bool) -> Arc<Self> {
        Self::spawn_with(url, demo, Timing::default())
    }

    pub fn spawn_with(url: &str, demo: bool, timing: Timing) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Arc::new(Self {
            state: Arc::new(Mutex::new(State::default())),
            notify: Arc::new(Notify::new()),
            cmd: tx,
        });
        tokio::spawn(run(
            url.to_string(),
            demo,
            timing,
            s.state.clone(),
            s.notify.clone(),
            rx,
        ));
        s
    }

    pub fn stats(&self) -> StreamStats {
        let st = self.state.lock().expect("okx stream state");
        let mut s = st.stats.clone();
        s.subscribed_instruments = st.wanted.len();
        s
    }

    /// Subscribe without waiting (prewarm). Returns true if the instrument was not wanted yet.
    pub fn subscribe(&self, inst_id: &str, index_id: &str) -> bool {
        let mut st = self.state.lock().expect("okx stream state");
        let new = !st.wanted.contains_key(inst_id);
        st.wanted
            .insert(inst_id.to_string(), (Instant::now(), index_id.to_string()));
        if new {
            st.rejected.remove(inst_id);
            let _ = self.cmd.send(Cmd::Subscribe);
        }
        new
    }

    /// Current book of `inst_id` if the stream can serve it (see module doc), subscribing on first
    /// use. A NEW subscription returns `NotYet` at once (the caller falls back to REST while the
    /// stream warms). Otherwise waits up to `wait`. `need_index`: inverse books need a fresh index.
    pub async fn snapshot(
        &self,
        inst_id: &str,
        index_id: &str,
        need_index: bool,
        max_age: Duration,
        wait: Duration,
    ) -> Snapshot {
        if self.subscribe(inst_id, index_id) {
            return Snapshot::NotYet;
        }
        let deadline = Instant::now() + wait;
        loop {
            let notified = self.notify.notified();
            {
                let st = self.state.lock().expect("okx stream state");
                if let Some(r) = st.rejected.get(inst_id) {
                    return Snapshot::Rejected(r.clone());
                }
                if let Some(v) = view(&st, inst_id, index_id, need_index, max_age) {
                    return Snapshot::Ready(v);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Snapshot::NotYet;
            }
        }
    }
}

fn view(
    st: &State,
    inst_id: &str,
    index_id: &str,
    need_index: bool,
    max_age: Duration,
) -> Option<View> {
    let (confirmed_at, frame_mono) = st.last_frame?;
    if !st.stats.connected || frame_mono.elapsed() > max_age {
        return None;
    }
    let book = st.books.get(inst_id)?;
    if book.last_msg.elapsed() > BOOK_SILENCE_MAX {
        return None;
    }
    let fresh = |e: &Entry| (e.mono.elapsed() <= max_age).then(|| e.data.clone());
    let index = st.index.get(index_id).and_then(fresh);
    if need_index && index.is_none() {
        return None;
    }
    Some(View {
        book: book.to_rest_shape(),
        index,
        mark: st.marks.get(inst_id).and_then(fresh),
        confirmed_at,
    })
}

fn index_arg(index_id: &str) -> Value {
    json!({"channel": "index-tickers", "instId": index_id})
}
fn inst_args(inst_id: &str) -> [Value; 2] {
    [
        json!({"channel": "books", "instId": inst_id}),
        json!({"channel": "mark-price", "instId": inst_id}),
    ]
}

fn requests(op: &str, args: Vec<Value>) -> Vec<String> {
    args.chunks(SUB_CHUNK)
        .map(|c| json!({"op": op, "args": c}).to_string())
        .collect()
}

/// CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320) — same as zlib `crc32`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// OKX book checksum: top 25 levels, `bidPx:bidSz:askPx:askSz:...` interleaved (the longer side's
/// remaining levels appended in turn), CRC32 as a signed 32-bit integer.
fn checksum(book: &Book) -> i64 {
    let bids: Vec<&Value> = book.bids.values().rev().take(CHECKSUM_LEVELS).collect();
    let asks: Vec<&Value> = book.asks.values().take(CHECKSUM_LEVELS).collect();
    let mut parts: Vec<&str> = Vec::with_capacity(4 * CHECKSUM_LEVELS);
    for i in 0..CHECKSUM_LEVELS {
        for side in [&bids, &asks] {
            if let Some(l) = side.get(i) {
                parts.push(l[0].as_str().unwrap_or(""));
                parts.push(l[1].as_str().unwrap_or(""));
            }
        }
    }
    i64::from(crc32(parts.join(":").as_bytes()) as i32)
}

/// Apply levels (absolute sizes, `"0"` deletes) to one side. Err = unparseable level.
fn apply_side(side: &mut Side, levels: &Value) -> Result<(), String> {
    let Some(arr) = levels.as_array() else {
        return Err("side is not an array".into());
    };
    for l in arr {
        let px = l[0]
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .filter(|p| *p > Decimal::ZERO)
            .ok_or_else(|| format!("bad px in {l}"))?;
        let sz = l[1]
            .as_str()
            .and_then(|s| Decimal::from_str(s).ok())
            .filter(|s| *s >= Decimal::ZERO)
            .ok_or_else(|| format!("bad sz in {l}"))?;
        if sz.is_zero() {
            side.remove(&px);
        } else {
            side.insert(px, l.clone());
        }
    }
    Ok(())
}

fn resync(st: &mut State, inst_id: &str, why: &str) {
    tracing::warn!(inst_id, why, "okx book resync");
    st.books.remove(inst_id);
    st.stats.resyncs += 1;
    if st.wanted.contains_key(inst_id) && !st.resync.iter().any(|x| x == inst_id) {
        st.resync.push(inst_id.to_string());
    }
}

/// `9.857022996959E-4` -> `0.0009857022996959`; plain decimals unchanged; None if unparseable.
fn plain_decimal(s: &str) -> Option<String> {
    Decimal::from_str(s)
        .or_else(|_| Decimal::from_scientific(s))
        .ok()
        .map(|d| d.normalize().to_string())
}

/// Apply one server frame (JSON). Returns true if state changed.
fn apply(st: &mut State, v: &Value) -> bool {
    if let Some(ev) = v["event"].as_str() {
        if ev != "error" {
            return false;
        }
        let msg = v["msg"].as_str().unwrap_or("");
        let inst = msg
            .split("instId:")
            .nth(1)
            .and_then(|r| r.split([' ', ',']).next())
            .filter(|i| st.wanted.contains_key(*i))
            .map(str::to_string);
        if let Some(i) = inst {
            st.stats.subscribe_rejections += 1;
            st.wanted.remove(&i);
            st.books.remove(&i);
            st.marks.remove(&i);
            st.rejected.insert(
                i,
                format!("code {} {msg}", v["code"].as_str().unwrap_or("?")),
            );
        } else {
            st.stats.last_error = Some(format!("okx ws error: {v}"));
        }
        return true;
    }
    let ch = v["arg"]["channel"].as_str().unwrap_or("");
    let Some(id) = v["arg"]["instId"].as_str() else {
        return false;
    };
    let data = &v["data"][0];
    if data.is_null() {
        return false;
    }
    st.stats.messages += 1;
    let mono = Instant::now();
    match ch {
        "books" => apply_book(st, id, v["action"].as_str().unwrap_or(""), data, mono),
        "index-tickers" => {
            st.index.insert(
                id.to_string(),
                Entry {
                    data: data.clone(),
                    mono,
                },
            );
            true
        }
        "mark-price" => {
            let mut d = data.clone();
            match d["markPx"].as_str().map(plain_decimal) {
                Some(Some(px)) => d["markPx"] = Value::String(px),
                Some(None) => return false, // unparseable mark: not cached (enrichment only)
                None => {}
            }
            st.marks.insert(id.to_string(), Entry { data: d, mono });
            true
        }
        _ => false,
    }
}

fn apply_book(st: &mut State, id: &str, action: &str, data: &Value, mono: Instant) -> bool {
    let seq = data["seqId"].as_i64();
    let cs = data["checksum"].as_i64().unwrap_or(0);
    let mut book = match action {
        "snapshot" => {
            st.stats.book_snapshots += 1;
            let mut b = Book {
                bids: Side::new(),
                asks: Side::new(),
                seq: 0,
                ts: Value::Null,
                last_msg: mono,
            };
            if let Err(e) = apply_side(&mut b.bids, &data["bids"])
                .and_then(|_| apply_side(&mut b.asks, &data["asks"]))
            {
                st.stats.invalid_levels += 1;
                resync(st, id, &e);
                return true;
            }
            b
        }
        "update" => {
            st.stats.book_updates += 1;
            let Some(mut b) = st.books.remove(id) else {
                st.stats.updates_before_snapshot += 1;
                return false;
            };
            if data["prevSeqId"].as_i64() != Some(b.seq) {
                st.stats.sequence_breaks += 1;
                resync(
                    st,
                    id,
                    &format!("prevSeqId {} != seqId {}", data["prevSeqId"], b.seq),
                );
                return true;
            }
            if data["prevSeqId"].as_i64() == seq
                && data["bids"].as_array().is_some_and(Vec::is_empty)
                && data["asks"].as_array().is_some_and(Vec::is_empty)
            {
                st.stats.heartbeats += 1;
            }
            if let Err(e) = apply_side(&mut b.bids, &data["bids"])
                .and_then(|_| apply_side(&mut b.asks, &data["asks"]))
            {
                st.stats.invalid_levels += 1;
                resync(st, id, &e);
                return true;
            }
            b.last_msg = mono;
            b
        }
        _ => return false,
    };
    let Some(seq) = seq else {
        resync(st, id, "missing seqId");
        return true;
    };
    book.seq = seq;
    book.ts = data["ts"].clone();
    if cs == 0 {
        st.stats.checksum_absent += 1;
    } else if checksum(&book) != cs {
        st.stats.checksum_mismatches += 1;
        resync(st, id, "checksum mismatch");
        return true;
    }
    if book.crossed() {
        st.stats.crossed_books += 1;
        resync(st, id, "crossed book");
        return true;
    }
    st.books.insert(id.to_string(), book);
    true
}

fn invalidate(st: &mut State) {
    if !st.books.is_empty() || !st.index.is_empty() || !st.marks.is_empty() {
        st.stats.invalidations += 1;
    }
    st.books.clear();
    st.index.clear();
    st.marks.clear();
    st.resync.clear();
    st.last_frame = None;
}

async fn run(
    url: String,
    demo: bool,
    timing: Timing,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        while state.lock().expect("okx stream state").wanted.is_empty() {
            if rx.recv().await.is_none() {
                return;
            }
        }
        let mut req = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                state.lock().expect("okx stream state").stats.last_error = Some(e.to_string());
                return;
            }
        };
        if demo {
            req.headers_mut().insert(
                "x-simulated-trading",
                tokio_tungstenite::tungstenite::http::HeaderValue::from_static("1"),
            );
        }
        let conn = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(req),
        )
        .await;
        let mut ws = match conn {
            Ok(Ok((ws, _))) => ws,
            other => {
                let e = match other {
                    Ok(Err(e)) => e.to_string(),
                    _ => "connect timeout".into(),
                };
                state.lock().expect("okx stream state").stats.last_error = Some(e);
                tokio::time::sleep(crate::http::jittered(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        // Subscriptions live on this connection.
        let mut subscribed: HashSet<String> = HashSet::new();
        let mut indexes: HashSet<String> = HashSet::new();
        {
            let mut st = state.lock().expect("okx stream state");
            st.stats.connected = true;
            st.stats.connects += 1;
        }
        let mut ok = sync_subscriptions(&mut ws, &state, &mut subscribed, &mut indexes).await;
        let mut awaiting_pong = false;
        let mut housekeeping = tokio::time::interval(timing.housekeeping);
        housekeeping.tick().await;
        while ok {
            tokio::select! {
                msg = tokio::time::timeout(timing.ping_after, ws.next()) => {
                    match msg {
                        Ok(Some(Ok(Message::Text(t)))) => {
                            awaiting_pong = false;
                            backoff = Duration::from_millis(500);
                            let resync: Vec<String> = {
                                let mut st = state.lock().expect("okx stream state");
                                st.last_frame = Some((Utc::now(), Instant::now()));
                                if t.as_str() != "pong"
                                    && let Ok(v) = serde_json::from_str::<Value>(t.as_str())
                                {
                                    apply(&mut st, &v);
                                }
                                std::mem::take(&mut st.resync)
                            };
                            notify.notify_waiters();
                            if !resync.is_empty() {
                                let args: Vec<Value> = resync.iter()
                                    .map(|i| json!({"channel": "books", "instId": i})).collect();
                                for op in ["unsubscribe", "subscribe"] {
                                    for r in requests(op, args.clone()) {
                                        if ws.send(Message::text(r)).await.is_err() { ok = false; }
                                    }
                                }
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => { ok = false; }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            state.lock().expect("okx stream state").stats.last_error = Some(e.to_string());
                            ok = false;
                        }
                        Err(_) if !awaiting_pong => {
                            awaiting_pong = true;
                            state.lock().expect("okx stream state").stats.pings += 1;
                            if ws.send(Message::text("ping")).await.is_err() { ok = false; }
                        }
                        Err(_) => {
                            state.lock().expect("okx stream state").stats.last_error =
                                Some("idle timeout (no frame, no pong)".into());
                            ok = false;
                        }
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Subscribe) => {
                            while rx.try_recv().is_ok() {} // batch queued requests
                            ok = sync_subscriptions(&mut ws, &state, &mut subscribed, &mut indexes).await;
                        }
                        None => return,
                    }
                }
                _ = housekeeping.tick() => {
                    let drop: Vec<String> = {
                        let mut st = state.lock().expect("okx stream state");
                        let mut by_age: Vec<(String, Instant)> =
                            st.wanted.iter().map(|(k, v)| (k.clone(), v.0)).collect();
                        by_age.sort_by_key(|x| x.1);
                        let over = by_age.len().saturating_sub(MAX_INSTRUMENTS);
                        let drop: Vec<String> = by_age.iter().enumerate()
                            .filter(|(i, (_, t))| *i < over || t.elapsed() > IDLE_UNSUBSCRIBE)
                            .map(|(_, (k, _))| k.clone()).collect();
                        for k in &drop { st.wanted.remove(k); st.books.remove(k); st.marks.remove(k); }
                        drop
                    };
                    if !drop.is_empty() {
                        for k in &drop { subscribed.remove(k); }
                        let args = drop.iter().flat_map(|i| inst_args(i)).collect();
                        for r in requests("unsubscribe", args) {
                            if ws.send(Message::text(r)).await.is_err() { ok = false; }
                        }
                    }
                }
            }
        }
        {
            let mut st = state.lock().expect("okx stream state");
            st.stats.connected = false;
            st.stats.disconnects += 1;
            invalidate(&mut st);
        }
        notify.notify_waiters();
        tracing::warn!(?backoff, "okx websocket disconnected; reconnecting");
        tokio::time::sleep(crate::http::jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Subscribe every wanted instrument (books + mark-price) and index not yet live on this
/// connection. Returns false if the socket failed.
async fn sync_subscriptions<S>(
    ws: &mut S,
    state: &Arc<Mutex<State>>,
    subscribed: &mut HashSet<String>,
    indexes: &mut HashSet<String>,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    let mut args = vec![];
    {
        let st = state.lock().expect("okx stream state");
        for (id, (_, ix)) in &st.wanted {
            if indexes.insert(ix.clone()) {
                args.push(index_arg(ix));
            }
            if subscribed.insert(id.clone()) {
                args.extend(inst_args(id));
            }
        }
    }
    for r in requests("subscribe", args) {
        if ws.send(Message::text(r)).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    pub(crate) fn recorded() -> Value {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/recorded-2026-10-07/okx_ws_books.json");
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn wanted(st: &mut State, id: &str) {
        st.wanted
            .insert(id.into(), (Instant::now(), "BTC-USD".into()));
    }

    #[test]
    fn crc32_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn checksum_string_is_interleaved_top_levels() {
        let mut b = Book {
            bids: Side::new(),
            asks: Side::new(),
            seq: 1,
            ts: Value::Null,
            last_msg: Instant::now(),
        };
        apply_side(&mut b.bids, &json!([["3", "1"], ["2", "5"]])).unwrap();
        apply_side(&mut b.asks, &json!([["4", "2"]])).unwrap();
        // best bid, best ask, then the remaining bid.
        assert_eq!(checksum(&b), i64::from(crc32(b"3:1:4:2:2:5") as i32));
    }

    #[test]
    fn recorded_snapshot_and_updates_chain() {
        let r = recorded();
        let mut st = State::default();
        wanted(&mut st, "BTC-USD-261009-86000-C");
        for m in r["inverse_books"].as_array().unwrap() {
            assert!(apply(&mut st, m));
        }
        let b = &st.books["BTC-USD-261009-86000-C"];
        assert_eq!(b.seq, 7106473);
        // Last update set the best ask size to 1492 and the best bid size (seq 7106471) to 7476.
        assert_eq!(b.asks.values().next().unwrap()[1], "1492");
        assert_eq!(b.bids.values().next_back().unwrap()[1], "7476");
        assert_eq!(b.asks.len(), 15);
        assert_eq!(b.bids.len(), 7);
        assert_eq!(b.ts, "1791374597505");
        assert_eq!(st.stats.sequence_breaks, 0);
        assert_eq!(st.stats.checksum_absent, 5);
        assert!(st.resync.is_empty());

        // `_UM`: deletions (`sz "0"`) applied.
        wanted(&mut st, "BTC-USD_UM-261127-102000-P");
        for m in r["um_books"].as_array().unwrap() {
            apply(&mut st, m);
        }
        let um = st.books["BTC-USD_UM-261127-102000-P"].to_rest_shape();
        assert_eq!(um["asks"], json!([["18455", "100", "0", "1"]]));
        assert_eq!(um["bids"], json!([["18340", "100", "0", "1"]]));
    }

    #[test]
    fn sequence_break_drops_and_resyncs_and_ignores_until_snapshot() {
        let r = recorded();
        let books = r["inverse_books"].as_array().unwrap();
        let mut st = State::default();
        let id = "BTC-USD-261009-86000-C";
        wanted(&mut st, id);
        apply(&mut st, &books[0]);
        // Skip books[1]: books[2] has prevSeqId 7106464 != 7106463.
        apply(&mut st, &books[2]);
        assert!(!st.books.contains_key(id), "broken book never served");
        assert_eq!(st.stats.sequence_breaks, 1);
        assert_eq!(st.resync, vec![id.to_string()]);
        // Further updates before the new snapshot are ignored.
        assert!(!apply(&mut st, &books[3]));
        assert_eq!(st.stats.updates_before_snapshot, 1);
        apply(&mut st, &books[0]);
        assert!(st.books.contains_key(id));
        // Duplicate update (same prevSeqId as already applied) is a break too.
        apply(&mut st, &books[1]);
        apply(&mut st, &books[1]);
        assert_eq!(st.stats.sequence_breaks, 2);
    }

    #[test]
    fn heartbeat_checksum_mismatch_and_crossed_book() {
        let mut st = State::default();
        let id = "X";
        wanted(&mut st, id);
        let snap = |cs: i64| {
            json!({"arg": {"channel": "books", "instId": id}, "action": "snapshot",
            "data": [{"asks": [["2", "1", "0", "1"]], "bids": [["1", "1", "0", "1"]], "ts": "5",
                      "checksum": cs, "seqId": 10, "prevSeqId": -1}]})
        };
        apply(&mut st, &snap(0));
        apply(
            &mut st,
            &json!({"arg": {"channel": "books", "instId": id}, "action": "update",
            "data": [{"asks": [], "bids": [], "ts": "9", "checksum": 0, "seqId": 10, "prevSeqId": 10}]}),
        );
        assert_eq!(st.stats.heartbeats, 1);
        assert_eq!(st.books[id].ts, "9");
        // Correct non-zero checksum accepted, wrong one resyncs.
        let good = i64::from(crc32(b"1:1:2:1") as i32);
        apply(&mut st, &snap(good));
        assert!(st.books.contains_key(id));
        apply(&mut st, &snap(good + 1));
        assert!(!st.books.contains_key(id));
        assert_eq!(st.stats.checksum_mismatches, 1);
        // Crossed.
        apply(&mut st, &snap(0));
        apply(
            &mut st,
            &json!({"arg": {"channel": "books", "instId": id}, "action": "update",
            "data": [{"asks": [], "bids": [["3", "1", "0", "1"]], "ts": "9", "checksum": 0, "seqId": 11, "prevSeqId": 10}]}),
        );
        assert!(!st.books.contains_key(id));
        assert_eq!(st.stats.crossed_books, 1);
    }

    #[test]
    fn rejection_and_scientific_mark() {
        let r = recorded();
        let mut st = State::default();
        wanted(&mut st, "BTC-USD-991231-1-C");
        apply(&mut st, &r["error"]);
        assert!(st.rejected["BTC-USD-991231-1-C"].contains("60018"));
        assert!(st.wanted.is_empty());
        assert!(!apply(&mut st, &r["ack"]));
        apply(&mut st, &r["mark"]);
        assert_eq!(
            st.marks["BTC-USD-261009-86000-C"].data["markPx"],
            "0.0009857022996959"
        );
        apply(&mut st, &r["index"]);
        assert_eq!(st.index["BTC-USD"].data["idxPx"], "83605.8");
    }

    /// The REST books recorded 2026-10-06, replayed as WS snapshots (+ an update), give exactly
    /// the same canonical book as REST once materialised: same parse/convert path.
    #[test]
    fn stream_book_converts_exactly_like_rest() {
        use crate::okx::{parse_book, parse_instruments};
        use oa_core::domain::Provenance;
        let f = |n: &str| -> Value {
            let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/recorded-2026-10-06")
                .join(n);
            let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            v["data"].clone()
        };
        let at: DateTime<Utc> = "2026-10-06T09:00:00Z".parse().unwrap();
        let cat = parse_instruments(&f("okx_instruments.json"), "BTC", at).unwrap();
        let index = &f("okx_index_btc.json")[0];
        for (id, file) in [
            ("BTC-USD-261009-86000-C", "okx_book_btc_inverse.json"),
            ("BTC-USD_UM-261009-86000-C", "okx_book_btc_um.json"),
        ] {
            let inst = cat.iter().find(|i| i.key.native_id == id).unwrap();
            let rest = &f(file)[0];
            let mut st = State::default();
            wanted(&mut st, id);
            let mut d = rest.clone();
            d["checksum"] = json!(0);
            d["prevSeqId"] = json!(-1);
            apply(
                &mut st,
                &json!({"arg": {"channel": "books", "instId": id}, "action": "snapshot", "data": [d]}),
            );
            let streamed = st.books[id].to_rest_shape();
            let a = parse_book(inst, rest, Some(index), None, at, Provenance::LiveData).unwrap();
            let b =
                parse_book(inst, &streamed, Some(index), None, at, Provenance::LiveData).unwrap();
            assert_eq!(a.bids, b.bids, "{id}");
            assert_eq!(a.asks, b.asks, "{id}");
            assert_eq!(a.source_ts, b.source_ts);
            assert_eq!(a.index_price, b.index_price);
        }
        // Worked example (inverse): 0.0105 BTC/BTC x 0.01 x 85,966.1 = 9.02644050 USD per contract.
        let inst = cat
            .iter()
            .find(|i| i.key.native_id == "BTC-USD-261009-86000-C")
            .unwrap();
        let mut st = State::default();
        wanted(&mut st, "BTC-USD-261009-86000-C");
        let mut d = f("okx_book_btc_inverse.json")[0].clone();
        d["checksum"] = json!(0);
        d["prevSeqId"] = json!(-1);
        apply(
            &mut st,
            &json!({"arg": {"channel": "books", "instId": "BTC-USD-261009-86000-C"}, "action": "snapshot", "data": [d]}),
        );
        let b = parse_book(
            inst,
            &st.books["BTC-USD-261009-86000-C"].to_rest_shape(),
            Some(index),
            None,
            at,
            Provenance::LiveData,
        )
        .unwrap();
        assert_eq!(b.asks[0].price, rust_decimal_macros::dec!(9.02644050));
        assert_eq!(b.asks[0].size, rust_decimal_macros::dec!(3197));
    }

    fn book_frame(id: &str, action: &str, prev: i64, seq: i64, ask: &str) -> String {
        json!({"arg": {"channel": "books", "instId": id}, "action": action, "data": [{
            "asks": [[ask, "1", "0", "1"]], "bids": [], "ts": "1", "checksum": 0,
            "seqId": seq, "prevSeqId": prev}]})
        .to_string()
    }

    async fn next_text<S>(ws: &mut S) -> String
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return t.to_string(),
                Some(Ok(_)) => continue,
                other => panic!("server read: {other:?}"),
            }
        }
    }

    /// Fake OKX: subscribe -> snapshot; ping -> pong; sequence gap -> client must unsubscribe +
    /// resubscribe and get a new snapshot; then connection drop -> invalidation, reconnect, new book.
    #[tokio::test]
    async fn subscribe_ping_resync_reconnect() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let id = "BTC-USD-261009-86000-C";
        let server = tokio::spawn(async move {
            // Round 1.
            let (s, _) = l.accept().await.unwrap();
            let mut ws = accept_async(s).await.unwrap();
            let sub: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(sub["op"], "subscribe");
            let args = sub["args"].as_array().unwrap();
            assert!(args.contains(&json!({"channel": "books", "instId": id})));
            assert!(args.contains(&json!({"channel": "mark-price", "instId": id})));
            assert!(args.contains(&json!({"channel": "index-tickers", "instId": "BTC-USD"})));
            let idx = json!({"arg": {"channel": "index-tickers", "instId": "BTC-USD"},
                             "data": [{"instId": "BTC-USD", "idxPx": "80000", "ts": "1"}]});
            ws.send(Message::text(idx.to_string())).await.unwrap();
            ws.send(Message::text(book_frame(id, "snapshot", -1, 10, "0.001")))
                .await
                .unwrap();
            // Silence -> client pings.
            assert_eq!(next_text(&mut ws).await, "ping");
            ws.send(Message::text("pong")).await.unwrap();
            // Gap.
            ws.send(Message::text(book_frame(id, "update", 11, 12, "0.002")))
                .await
                .unwrap();
            let un: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(un["op"], "unsubscribe");
            assert_eq!(un["args"], json!([{"channel": "books", "instId": id}]));
            let re: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert_eq!(re["op"], "subscribe");
            ws.send(Message::text(idx.to_string())).await.unwrap();
            ws.send(Message::text(book_frame(id, "snapshot", -1, 20, "0.003")))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = ws.close(None).await;
            // Round 2: reconnect restores subscriptions.
            let (s, _) = l.accept().await.unwrap();
            let mut ws = accept_async(s).await.unwrap();
            let sub: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
            assert!(
                sub["args"]
                    .as_array()
                    .unwrap()
                    .contains(&json!({"channel": "books", "instId": id}))
            );
            ws.send(Message::text(idx.to_string())).await.unwrap();
            ws.send(Message::text(book_frame(id, "snapshot", -1, 30, "0.004")))
                .await
                .unwrap();
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if ws.send(Message::text(idx.to_string())).await.is_err() {
                    break;
                }
            }
        });
        let s = OkxStream::spawn_with(
            &url,
            false,
            Timing {
                ping_after: Duration::from_millis(300),
                housekeeping: Duration::from_secs(30),
            },
        );
        let age = Duration::from_secs(5);
        let get = |w: u64| s.snapshot(id, "BTC-USD", true, age, Duration::from_millis(w));
        assert!(matches!(get(0).await, Snapshot::NotYet), "new subscription");
        let ask = |v: View| v.book["asks"][0][0].as_str().unwrap().to_string();
        let first = match get(2000).await {
            Snapshot::Ready(v) => v,
            _ => panic!("first book"),
        };
        assert_eq!(first.index.as_ref().unwrap()["idxPx"], "80000");
        assert_eq!(ask(first), "0.001");
        // Wait for the resync snapshot (0.003); the gapped update (0.002) must never be served.
        let mut seen = vec![];
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Snapshot::Ready(v) = get(100).await {
                let a = ask(v);
                assert_ne!(a, "0.002", "gapped update served");
                if a == "0.004" {
                    seen.push(a);
                    break;
                }
                if seen.last() != Some(&a) {
                    seen.push(a);
                }
            }
        }
        assert!(seen.contains(&"0.003".to_string()), "{seen:?}");
        assert_eq!(seen.last().map(String::as_str), Some("0.004"), "{seen:?}");
        let st = s.stats();
        assert!(st.pings >= 1, "{st:?}");
        assert_eq!(st.sequence_breaks, 1, "{st:?}");
        assert!(
            st.resyncs >= 1 && st.connects >= 2 && st.disconnects >= 1,
            "{st:?}"
        );
        assert!(st.invalidations >= 1, "{st:?}");
        server.abort();
    }
}
