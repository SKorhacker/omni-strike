//! Persistent Bybit v5 public WebSocket for OPTIONS: on-demand subscriptions, a local order book
//! maintained from snapshot + deltas, latest ticker (index/mark) per symbol, automatic reconnection.
//!
//! Protocol [DOC https://bybit-exchange.github.io/docs/v5/ws/connect, .../websocket/public/orderbook,
//! .../websocket/public/ticker], all checked live 2026-10-07 on mainnet and testnet:
//! - URLs `wss://stream.bybit.com/v5/public/option`, testnet `wss://stream-testnet.bybit.com/v5/public/option`.
//! - Subscribe `{"req_id","op":"subscribe","args":[topics]}`; options: at most 10 args per request and
//!   2000 args per connection. Answer `{"success":true,"conn_id",..,"data":{"failTopics":[],
//!   "successTopics":[..]},"type":"COMMAND_RESP"}`. LIVE: an unknown symbol is ACCEPTED (listed in
//!   `successTopics`) and simply never sends data, so it is not detectable as a rejection.
//! - Heartbeat `{"op":"ping"}` every 20 s (doc); options answer `{"args":["<ms>"],"op":"pong"}`.
//! - `orderbook.25.{symbol}` (option depth 25, push 20 ms): `{topic, ts, type: snapshot|delta, id,
//!   data:{s, b, a, u, seq}, cts}`; levels `[price, size]` strings in the same units as the REST
//!   `/v5/market/orderbook` (USDT per 1-coin contract, size in contracts). Delta rules (doc): size 0 =
//!   delete, unknown price = insert, known price = update; `u == 1` = service-restart snapshot that
//!   replaces the book. LIVE: `u` is +1 per message per symbol (282276 -> 282277 -> ...), `seq` is
//!   the cross sequence (not contiguous) and is only recorded.
//! - `tickers.{symbol}` (options: snapshot-only, push 100 ms): `data.indexPrice` / `data.markPrice`.
//!
//! Implemented here:
//! - A delta whose `u` is not exactly last+1 is a GAP: the book is dropped (never served) and the
//!   orderbook topic is unsubscribed + resubscribed to obtain a fresh snapshot. `u <= last` =
//!   duplicate / out of order, dropped. Deltas before the first snapshot are ignored.
//! - Freshness: Bybit pushes a delta on every change, and frames on one connection arrive in order,
//!   so a synchronised book is exactly current as of the LAST frame received on its connection
//!   (any topic, or a pong). `confirmed_at` = that arrival time; a book is only served while that
//!   connection has spoken within `MAX_SILENCE`. A quiet (unchanged) book on a live connection is
//!   therefore still served, with its true confirmation time.
//! - On any disconnection every book/ticker of that connection is INVALIDATED; reconnection uses
//!   exponential backoff and restores all subscriptions (10 args per request).
//! - Sharding: instruments are spread over `SHARDS` connections by a stable hash, each bounded to
//!   `MAX_INSTRUMENTS_PER_SHARD` (x2 topics = 800 args < 2000), least-recently-used evicted; idle
//!   subscriptions are dropped after `IDLE_UNSUBSCRIBE`. A connection is opened only once it has a
//!   subscription.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub const MAINNET_WS: &str = "wss://stream.bybit.com/v5/public/option";
pub const TESTNET_WS: &str = "wss://stream-testnet.bybit.com/v5/public/option";
pub const DEPTH: usize = 25;
/// Doc: max 10 args per subscribe request for options.
const ARGS_PER_REQUEST: usize = 10;
/// Doc: max 2000 args per option connection; 2 topics per instrument -> 400 x 2 = 800 (margin).
const MAX_INSTRUMENTS_PER_SHARD: usize = 400;
const SHARDS: usize = 2;
const PING_EVERY: Duration = Duration::from_secs(20);
/// No frame at all (data or pong) for this long = dead connection (> ping period).
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// A book is served only if its connection delivered a frame within this window.
pub const MAX_SILENCE: Duration = Duration::from_secs(5);
const IDLE_UNSUBSCRIBE: Duration = Duration::from_secs(600);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

fn book_topic(sym: &str) -> String {
    format!("orderbook.{DEPTH}.{sym}")
}
fn ticker_topic(sym: &str) -> String {
    format!("tickers.{sym}")
}

#[derive(Debug, Clone)]
struct Book {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    u: i64,
    seq: Option<i64>,
    /// Venue `ts` (ms) of the last applied message.
    venue_ts: Option<i64>,
}

#[derive(Debug, Clone)]
struct Ticker {
    index: Value,
    mark: Value,
    mono: Instant,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StreamStats {
    pub connections: usize,
    pub connected: usize,
    pub connects: u64,
    pub disconnects: u64,
    pub messages: u64,
    pub snapshots: u64,
    pub deltas: u64,
    pub sequence_gaps: u64,
    pub resyncs: u64,
    pub duplicates_or_out_of_order: u64,
    pub deltas_without_snapshot: u64,
    pub invalid_messages: u64,
    pub invalidations: u64,
    pub subscribe_rejections: u64,
    pub pongs: u64,
    pub subscribed_instruments: usize,
    pub last_error: Option<String>,
}

impl StreamStats {
    fn add(&mut self, o: &StreamStats) {
        self.connections += o.connections;
        self.connected += o.connected;
        self.connects += o.connects;
        self.disconnects += o.disconnects;
        self.messages += o.messages;
        self.snapshots += o.snapshots;
        self.deltas += o.deltas;
        self.sequence_gaps += o.sequence_gaps;
        self.resyncs += o.resyncs;
        self.duplicates_or_out_of_order += o.duplicates_or_out_of_order;
        self.deltas_without_snapshot += o.deltas_without_snapshot;
        self.invalid_messages += o.invalid_messages;
        self.invalidations += o.invalidations;
        self.subscribe_rejections += o.subscribe_rejections;
        self.pongs += o.pongs;
        self.subscribed_instruments += o.subscribed_instruments;
        if o.last_error.is_some() {
            self.last_error = o.last_error.clone();
        }
    }
}

#[derive(Default)]
struct State {
    books: HashMap<String, Book>,
    tickers: HashMap<String, Ticker>,
    /// symbol -> last use
    wanted: HashMap<String, Instant>,
    rejected: HashMap<String, String>,
    connected: bool,
    /// Arrival of the last frame on the current connection.
    last_frame: Option<(DateTime<Utc>, Instant)>,
    stats: StreamStats,
}

enum Cmd {
    Subscribe(String),
}

struct Shard {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

pub struct BybitStream {
    shards: Vec<Shard>,
}

/// Result of a snapshot request.
pub enum Snapshot {
    Ready {
        /// `{s, b, a, ts}`: same shape as the REST orderbook `result` (reuses `parse_book`).
        book: Value,
        /// `{list:[{symbol, indexPrice, markPrice}]}` (REST tickers shape), if fresh.
        ticker: Option<Value>,
        /// Arrival of the last frame on the book's connection (the book is current as of then).
        confirmed_at: DateTime<Utc>,
    },
    /// The platform refused the subscription (`failTopics`).
    Rejected(String),
    /// No synchronised book on a live connection within the wait.
    NotYet,
}

fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    m.lock().expect("bybit stream state")
}

impl BybitStream {
    pub fn spawn(url: &str) -> Arc<Self> {
        Self::spawn_with(url, PING_EVERY)
    }

    fn spawn_with(url: &str, ping_every: Duration) -> Arc<Self> {
        let shards = (0..SHARDS)
            .map(|_| {
                let (tx, rx) = mpsc::unbounded_channel();
                let s = Shard {
                    state: Arc::new(Mutex::new(State::default())),
                    notify: Arc::new(Notify::new()),
                    cmd: tx,
                };
                tokio::spawn(run(
                    url.to_string(),
                    s.state.clone(),
                    s.notify.clone(),
                    rx,
                    ping_every,
                ));
                s
            })
            .collect();
        Arc::new(Self { shards })
    }

    fn shard(&self, sym: &str) -> &Shard {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        sym.hash(&mut h);
        &self.shards[(h.finish() as usize) % self.shards.len()]
    }

    pub fn stats(&self) -> StreamStats {
        let mut out = StreamStats::default();
        for sh in &self.shards {
            let st = lock(&sh.state);
            let mut s = st.stats.clone();
            s.connections = 1;
            s.connected = usize::from(st.connected);
            s.subscribed_instruments = st.wanted.len();
            out.add(&s);
        }
        out
    }

    fn touch(&self, sym: &str) -> &Shard {
        let sh = self.shard(sym);
        let mut st = lock(&sh.state);
        let new = !st.wanted.contains_key(sym);
        st.wanted.insert(sym.to_string(), Instant::now());
        if new {
            let _ = sh.cmd.send(Cmd::Subscribe(sym.to_string()));
        }
        sh
    }

    /// Subscribe without waiting (prewarm): the book is maintained as soon as it arrives.
    pub fn subscribe(&self, sym: &str) {
        self.touch(sym);
    }

    /// Latest synchronised book (+ fresh ticker) for `sym`, subscribing on first use. Waits up to
    /// `wait`. Never returns a book received before the last disconnection, nor one whose
    /// connection has been silent for more than `MAX_SILENCE`.
    pub async fn snapshot(&self, sym: &str, wait: Duration, ticker_max_age: Duration) -> Snapshot {
        let deadline = Instant::now() + wait;
        lock(&self.shard(sym).state).rejected.remove(sym);
        let sh = self.touch(sym);
        loop {
            let notified = sh.notify.notified();
            {
                let st = lock(&sh.state);
                if let Some(r) = st.rejected.get(sym) {
                    return Snapshot::Rejected(r.clone());
                }
                if let (true, Some(b), Some((at, mono))) =
                    (st.connected, st.books.get(sym), st.last_frame)
                    && mono.elapsed() <= MAX_SILENCE
                {
                    let ticker = st
                        .tickers
                        .get(sym)
                        .filter(|t| t.mono.elapsed() <= ticker_max_age)
                        .map(|t| {
                            json!({"list": [{"symbol": sym, "indexPrice": t.index, "markPrice": t.mark}]})
                        });
                    return Snapshot::Ready {
                        book: book_value(sym, b),
                        ticker,
                        confirmed_at: at,
                    };
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Snapshot::NotYet;
            }
        }
    }
}

fn book_value(sym: &str, b: &Book) -> Value {
    let lv = |(p, s): (&Decimal, &Decimal)| json!([p.to_string(), s.to_string()]);
    json!({
        "s": sym,
        "b": b.bids.iter().rev().take(DEPTH).map(lv).collect::<Vec<_>>(),
        "a": b.asks.iter().take(DEPTH).map(lv).collect::<Vec<_>>(),
        "ts": b.venue_ts,
        "u": b.u,
        "seq": b.seq,
    })
}

fn invalidate(st: &mut State) {
    if !st.books.is_empty() || !st.tickers.is_empty() {
        st.stats.invalidations += 1;
    }
    st.books.clear();
    st.tickers.clear();
}

/// Apply `[[price, size], ...]` to one side. Size 0 deletes. Err on a malformed level.
fn apply_side(side: &mut BTreeMap<Decimal, Decimal>, levels: &Value) -> Result<(), String> {
    let Some(arr) = levels.as_array() else {
        return Err("side is not an array".into());
    };
    for l in arr {
        let num = |x: &Value| {
            x.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .ok_or_else(|| format!("bad level {l}"))
        };
        let (p, s) = (num(&l[0])?, num(&l[1])?);
        if p <= Decimal::ZERO || s < Decimal::ZERO {
            return Err(format!("non-positive level {l}"));
        }
        if s.is_zero() {
            side.remove(&p);
        } else {
            side.insert(p, s);
        }
    }
    Ok(())
}

/// What the connection loop must do after a message.
#[derive(Debug, Default, PartialEq)]
struct Effect {
    changed: bool,
    /// Symbol whose book lost sync: resubscribe its orderbook topic for a fresh snapshot.
    resync: Option<String>,
}

/// Apply one server message received at `now`.
fn apply(st: &mut State, v: &Value, now: (DateTime<Utc>, Instant)) -> Effect {
    st.last_frame = Some(now);
    let changed = Effect {
        changed: true,
        resync: None,
    };
    if v["op"] == "pong" || v["ret_msg"] == "pong" {
        st.stats.pongs += 1;
        // A pong proves every earlier frame was delivered: books are confirmed up to now.
        return changed;
    }
    if v["type"] == "COMMAND_RESP" || v.get("success").is_some() {
        if v["success"] == false {
            st.stats.last_error = Some(format!("command refused: {v}"));
        }
        let mut any = false;
        for t in v["data"]["failTopics"].as_array().into_iter().flatten() {
            let Some(t) = t.as_str() else { continue };
            let sym = t.split_once('.').map(|x| x.1).unwrap_or(t);
            let sym = sym.strip_prefix(&format!("{DEPTH}.")).unwrap_or(sym);
            st.stats.subscribe_rejections += 1;
            st.rejected.insert(sym.to_string(), format!("{t}: refused"));
            st.wanted.remove(sym);
            st.books.remove(sym);
            st.tickers.remove(sym);
            any = true;
        }
        return Effect {
            changed: any,
            resync: None,
        };
    }
    let topic = v["topic"].as_str().unwrap_or("");
    let data = &v["data"];
    if let Some(sym) = topic.strip_prefix("tickers.") {
        st.stats.messages += 1;
        st.tickers.insert(
            sym.to_string(),
            Ticker {
                index: data["indexPrice"].clone(),
                mark: data["markPrice"].clone(),
                mono: now.1,
            },
        );
        return changed;
    }
    let Some(sym) = topic.strip_prefix(&format!("orderbook.{DEPTH}.")) else {
        return Effect::default();
    };
    st.stats.messages += 1;
    let (Some(u), true) = (data["u"].as_i64(), data["s"] == sym) else {
        st.stats.invalid_messages += 1;
        return Effect::default();
    };
    let venue_ts = v["ts"].as_i64();
    let seq = data["seq"].as_i64();
    if v["type"] == "snapshot" || u == 1 {
        st.stats.snapshots += 1;
        let mut b = Book {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            u,
            seq,
            venue_ts,
        };
        if let Err(e) = apply_side(&mut b.bids, &data["b"]).and(apply_side(&mut b.asks, &data["a"]))
        {
            st.stats.invalid_messages += 1;
            st.stats.last_error = Some(format!("{sym} snapshot: {e}"));
            st.books.remove(sym);
            return Effect::default();
        }
        st.books.insert(sym.to_string(), b);
        return changed;
    }
    if v["type"] != "delta" {
        st.stats.invalid_messages += 1;
        return Effect::default();
    }
    st.stats.deltas += 1;
    let Some(b) = st.books.get_mut(sym) else {
        // Waiting for the (re)subscription snapshot.
        st.stats.deltas_without_snapshot += 1;
        return Effect::default();
    };
    if u <= b.u {
        st.stats.duplicates_or_out_of_order += 1;
        return Effect::default();
    }
    if u != b.u + 1 {
        tracing::warn!(sym, prev = b.u, u, "bybit book sequence gap; resyncing");
        st.books.remove(sym);
        st.stats.sequence_gaps += 1;
        st.stats.resyncs += 1;
        return Effect {
            changed: true,
            resync: Some(sym.to_string()),
        };
    }
    let mut nb = b.clone();
    match apply_side(&mut nb.bids, &data["b"]).and(apply_side(&mut nb.asks, &data["a"])) {
        Ok(()) => {
            nb.u = u;
            nb.seq = seq;
            nb.venue_ts = venue_ts;
            *b = nb;
            changed
        }
        Err(e) => {
            st.books.remove(sym);
            st.stats.invalid_messages += 1;
            st.stats.resyncs += 1;
            st.stats.last_error = Some(format!("{sym} delta: {e}"));
            Effect {
                changed: true,
                resync: Some(sym.to_string()),
            }
        }
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send_op(ws: &mut Ws, req_id: &mut u64, op: &str, args: &[String]) -> bool {
    for chunk in args.chunks(ARGS_PER_REQUEST) {
        *req_id += 1;
        let m = json!({"req_id": req_id.to_string(), "op": op, "args": chunk});
        if ws.send(Message::text(m.to_string())).await.is_err() {
            return false;
        }
    }
    true
}

fn topics(syms: &[String]) -> Vec<String> {
    syms.iter()
        .flat_map(|s| [book_topic(s), ticker_topic(s)])
        .collect()
}

async fn run(
    url: String,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    ping_every: Duration,
) {
    let mut backoff = Duration::from_millis(500);
    let mut req_id: u64 = 0;
    loop {
        // Connect only once this shard has at least one subscription.
        while lock(&state).wanted.is_empty() {
            match rx.recv().await {
                Some(Cmd::Subscribe(_)) => {}
                None => return,
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
                let e = match other {
                    Ok(Err(e)) => e.to_string(),
                    _ => "connect timeout".into(),
                };
                lock(&state).stats.last_error = Some(e);
                tokio::time::sleep(crate::http::jittered(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let syms: Vec<String> = {
            let mut st = lock(&state);
            st.connected = true;
            st.last_frame = None;
            st.stats.connects += 1;
            // Commands queued before the connection are covered by this full resubscription.
            while rx.try_recv().is_ok() {}
            st.wanted.keys().cloned().collect()
        };
        let mut ok = send_op(&mut ws, &mut req_id, "subscribe", &topics(&syms)).await;
        let mut ping = tokio::time::interval(ping_every);
        ping.tick().await;
        let mut housekeeping = tokio::time::interval(Duration::from_secs(30));
        housekeeping.tick().await;
        // Dead-connection detection: the idle clock is reset ONLY by a received frame (the per-await
        // timeout below restarts whenever another branch fires, e.g. every ping, so on its own it never
        // caught a half-open connection whose pings still enter the kernel send buffer).
        let mut last_frame = Instant::now();
        while ok {
            tokio::select! {
                msg = tokio::time::timeout(IDLE_TIMEOUT, ws.next()) => {
                    if let Ok(Some(Ok(_))) = &msg {
                        last_frame = Instant::now();
                    }
                    match msg {
                        Ok(Some(Ok(Message::Text(t)))) => {
                            backoff = Duration::from_millis(500);
                            let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else {
                                lock(&state).stats.invalid_messages += 1;
                                continue;
                            };
                            let eff = apply(&mut lock(&state), &v, (Utc::now(), Instant::now()));
                            if eff.changed { notify.notify_waiters(); }
                            if let Some(sym) = eff.resync {
                                let t = [book_topic(&sym)];
                                ok = send_op(&mut ws, &mut req_id, "unsubscribe", &t).await
                                    && send_op(&mut ws, &mut req_id, "subscribe", &t).await;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => { ok = false; }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            lock(&state).stats.last_error = Some(e.to_string());
                            ok = false;
                        }
                        Err(_) => {
                            lock(&state).stats.last_error = Some(format!("idle timeout (no frame for {IDLE_TIMEOUT:?})"));
                            ok = false;
                        }
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Subscribe(s)) => {
                            ok = send_op(&mut ws, &mut req_id, "subscribe", &topics(&[s])).await;
                        }
                        None => return,
                    }
                }
                _ = ping.tick() => {
                    if last_frame.elapsed() > IDLE_TIMEOUT {
                        ok = false;
                    } else {
                        req_id += 1;
                        let m = json!({"req_id": req_id.to_string(), "op": "ping"});
                        ok = ws.send(Message::text(m.to_string())).await.is_ok();
                    }
                }
                _ = housekeeping.tick() => {
                    // Drop idle subscriptions and enforce the per-connection bound (LRU).
                    let drop: Vec<String> = {
                        let mut st = lock(&state);
                        let mut by_age: Vec<(String, Instant)> = st.wanted.iter().map(|(k, v)| (k.clone(), *v)).collect();
                        by_age.sort_by_key(|x| x.1);
                        let over = by_age.len().saturating_sub(MAX_INSTRUMENTS_PER_SHARD);
                        let drop: Vec<String> = by_age.iter().enumerate()
                            .filter(|(i, (_, t))| *i < over || t.elapsed() > IDLE_UNSUBSCRIBE)
                            .map(|(_, (k, _))| k.clone()).collect();
                        for k in &drop { st.wanted.remove(k); st.books.remove(k); st.tickers.remove(k); }
                        drop
                    };
                    if !drop.is_empty() {
                        ok = send_op(&mut ws, &mut req_id, "unsubscribe", &topics(&drop)).await;
                    }
                }
            }
        }
        {
            let mut st = lock(&state);
            st.connected = false;
            st.last_frame = None;
            st.stats.disconnects += 1;
            invalidate(&mut st);
        }
        notify.notify_waiters();
        tracing::warn!(
            ?backoff,
            "bybit option websocket disconnected; reconnecting"
        );
        tokio::time::sleep(crate::http::jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const S: &str = "BTC-8OCT26-85000-C-USDT";

    async fn recv_json(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> Value {
        loop {
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                return serde_json::from_str(t.as_str()).unwrap();
            }
        }
    }

    fn now() -> (DateTime<Utc>, Instant) {
        (Utc::now(), Instant::now())
    }
    fn book(kind: &str, u: i64, b: Value, a: Value) -> Value {
        json!({"topic": book_topic(S), "ts": 1791374565374i64 + u, "type": kind, "id": "x",
               "data": {"s": S, "b": b, "a": a, "u": u, "seq": 77449598305i64 + u}, "cts": 1})
    }
    fn top(st: &State) -> (Value, Value) {
        let v = book_value(S, &st.books[S]);
        (v["b"].clone(), v["a"].clone())
    }

    /// Shapes copied from live frames of 2026-10-07 (snapshot u 282276, deltas 282277...).
    #[test]
    fn snapshot_then_deltas_with_deletes() {
        let mut st = State::default();
        apply(
            &mut st,
            &book(
                "snapshot",
                282276,
                json!([["115", "33.96"], ["110", "27.03"]]),
                json!([["120", "25.56"], ["195", "1"], ["200", "3"]]),
            ),
            now(),
        );
        // Delete 200, update 195, insert a bid.
        let e = apply(
            &mut st,
            &book(
                "delta",
                282277,
                json!([["105", "23.33"]]),
                json!([["195", "2"], ["200", "0"]]),
            ),
            now(),
        );
        assert_eq!(
            e,
            Effect {
                changed: true,
                resync: None
            }
        );
        let (b, a) = top(&st);
        assert_eq!(
            b,
            json!([["115", "33.96"], ["110", "27.03"], ["105", "23.33"]])
        );
        assert_eq!(a, json!([["120", "25.56"], ["195", "2"]]));
        // Size 0 on the best bid removes it.
        apply(
            &mut st,
            &book("delta", 282278, json!([["115", "0"]]), json!([])),
            now(),
        );
        assert_eq!(top(&st).0[0], json!(["110", "27.03"]));
        assert_eq!(st.books[S].u, 282278);
        assert_eq!(st.stats.snapshots, 1);
        assert_eq!(st.stats.deltas, 2);
        // The served book has the REST shape -> reuses the connector parser.
        let v = book_value(S, &st.books[S]);
        assert_eq!(v["s"], S);
        assert_eq!(v["ts"], 1791374565374i64 + 282278);
    }

    #[test]
    fn duplicate_gap_and_resync() {
        let mut st = State::default();
        apply(
            &mut st,
            &book("snapshot", 10, json!([["5", "1"]]), json!([["6", "1"]])),
            now(),
        );
        apply(
            &mut st,
            &book("delta", 11, json!([["5", "2"]]), json!([])),
            now(),
        );
        // Duplicate / out of order: dropped, book unchanged.
        let e = apply(
            &mut st,
            &book("delta", 11, json!([["5", "9"]]), json!([])),
            now(),
        );
        assert!(!e.changed);
        assert_eq!(st.stats.duplicates_or_out_of_order, 1);
        assert_eq!(top(&st).0[0], json!(["5", "2"]));
        // Gap (13 after 11): book dropped and resync requested.
        let e = apply(
            &mut st,
            &book("delta", 13, json!([["5", "3"]]), json!([])),
            now(),
        );
        assert_eq!(e.resync.as_deref(), Some(S));
        assert!(!st.books.contains_key(S));
        assert_eq!(st.stats.sequence_gaps, 1);
        // Deltas before the new snapshot are ignored.
        apply(
            &mut st,
            &book("delta", 14, json!([["5", "4"]]), json!([])),
            now(),
        );
        assert!(!st.books.contains_key(S));
        assert_eq!(st.stats.deltas_without_snapshot, 1);
        // New snapshot restores the book.
        apply(
            &mut st,
            &book("snapshot", 20, json!([["4", "1"]]), json!([["7", "1"]])),
            now(),
        );
        assert_eq!(top(&st).0, json!([["4", "1"]]));
        // u == 1 (service restart) replaces the book even if labelled delta.
        apply(
            &mut st,
            &book("delta", 1, json!([["3", "1"]]), json!([])),
            now(),
        );
        assert_eq!(top(&st), (json!([["3", "1"]]), json!([])));
    }

    #[test]
    fn malformed_delta_drops_the_book_and_resyncs() {
        let mut st = State::default();
        apply(
            &mut st,
            &book("snapshot", 1, json!([["5", "1"]]), json!([])),
            now(),
        );
        let e = apply(
            &mut st,
            &book("delta", 2, json!([["-5", "1"]]), json!([])),
            now(),
        );
        assert_eq!(e.resync.as_deref(), Some(S));
        assert!(!st.books.contains_key(S));
    }

    #[test]
    fn command_resp_fail_topics_and_pong() {
        let mut st = State::default();
        st.wanted.insert(S.into(), Instant::now());
        let ok = json!({"success": true, "conn_id": "c", "data": {"failTopics": [], "successTopics": [book_topic(S)]}, "type": "COMMAND_RESP"});
        assert!(!apply(&mut st, &ok, now()).changed);
        assert!(st.rejected.is_empty());
        let fail = json!({"success": false, "conn_id": "c", "data": {"failTopics": [book_topic(S)], "successTopics": []}, "type": "COMMAND_RESP"});
        apply(&mut st, &fail, now());
        assert!(st.rejected[S].contains("refused"));
        assert!(!st.wanted.contains_key(S));
        apply(
            &mut st,
            &json!({"args": ["1791374565641"], "op": "pong"}),
            now(),
        );
        assert_eq!(st.stats.pongs, 1);
        assert!(st.last_frame.is_some());
    }

    #[test]
    fn ticker_index_and_mark() {
        let mut st = State::default();
        apply(
            &mut st,
            &json!({"topic": ticker_topic(S), "ts": 1, "type": "snapshot", "id": "t",
            "data": {"symbol": S, "indexPrice": "83638.73985613", "markPrice": "117.2242083"}}),
            now(),
        );
        assert_eq!(st.tickers[S].index, "83638.73985613");
        assert_eq!(st.tickers[S].mark, "117.2242083");
    }

    /// Fake Bybit server: round 0 acks the subscription, answers a ping, sends a snapshot, then a
    /// GAPPED delta (expects unsubscribe + subscribe of the orderbook topic and answers with a new
    /// snapshot), then drops the connection. Round 1: the client must have invalidated, reconnected
    /// and resubscribed; only the round-1 book may be served.
    #[tokio::test]
    async fn fake_server_subscribe_ping_resync_reconnect() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let snap = |u: i64, ask: &str| {
            Message::text(
                book("snapshot", u, json!([["100", "1"]]), json!([[ask, "1"]])).to_string(),
            )
        };
        tokio::spawn(async move {
            for round in 0..2 {
                let (s, _) = l.accept().await.unwrap();
                let mut ws = accept_async(s).await.unwrap();
                let req = recv_json(&mut ws).await;
                assert_eq!(req["op"], "subscribe");
                let args = req["args"].as_array().unwrap();
                assert!(args.len() <= ARGS_PER_REQUEST);
                assert!(
                    args.contains(&json!(book_topic(S))) && args.contains(&json!(ticker_topic(S)))
                );
                let ack = json!({"success": true, "conn_id": "c", "data": {"failTopics": [], "successTopics": args}, "type": "COMMAND_RESP"});
                ws.send(Message::text(ack.to_string())).await.unwrap();
                ws.send(Message::text(
                    json!({"args": ["1"], "op": "pong"}).to_string(),
                ))
                .await
                .unwrap();
                if round == 0 {
                    ws.send(snap(5, "300")).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    // Gap: 7 after 5.
                    ws.send(Message::text(
                        book("delta", 7, json!([]), json!([["301", "1"]])).to_string(),
                    ))
                    .await
                    .unwrap();
                    let mut seen = vec![];
                    while seen.len() < 2 {
                        let v = recv_json(&mut ws).await;
                        if v["op"] != "ping" {
                            seen.push((v["op"].clone(), v["args"].clone()));
                        }
                    }
                    assert_eq!(seen[0], (json!("unsubscribe"), json!([book_topic(S)])));
                    assert_eq!(seen[1], (json!("subscribe"), json!([book_topic(S)])));
                    ws.send(snap(50, "310")).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = ws.close(None).await;
                } else {
                    ws.send(snap(1, "400")).await.unwrap();
                    // The client heartbeats: `{"req_id", "op":"ping"}` (period shortened for the test).
                    let p = recv_json(&mut ws).await;
                    assert_eq!(p["op"], "ping", "{p}");
                    assert!(p["req_id"].is_string());
                    ws.send(Message::text(
                        json!({"args": ["1"], "op": "pong"}).to_string(),
                    ))
                    .await
                    .unwrap();
                    for _ in 0..50 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if ws
                            .send(Message::text(
                                json!({"args": ["1"], "op": "pong"}).to_string(),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        let s = BybitStream::spawn_with(&url, Duration::from_millis(150));
        let ask = |snap: Snapshot| match snap {
            Snapshot::Ready { book, .. } => Some(book["a"][0][0].as_str().unwrap().to_string()),
            _ => None,
        };
        let w = Duration::from_secs(3);
        assert_eq!(ask(s.snapshot(S, w, w).await).as_deref(), Some("300"));
        // After the gap: never the gapped book; the resync snapshot arrives.
        let mut got = None;
        for _ in 0..40 {
            if let Some(a) = ask(s.snapshot(S, Duration::from_millis(100), w).await) {
                if a == "310" {
                    got = Some(a);
                    break;
                }
                assert_eq!(a, "300", "only the pre-gap or resynced book may be served");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got.as_deref(), Some("310"));
        // After the drop: invalidated, then the round-1 book only.
        let mut got = None;
        for _ in 0..60 {
            if let Some(a) = ask(s.snapshot(S, Duration::from_millis(100), w).await) {
                if a == "400" {
                    got = Some(a);
                    break;
                }
                assert_eq!(a, "310");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got.as_deref(), Some("400"));
        let st = s.stats();
        assert!(
            st.connects >= 2 && st.disconnects >= 1 && st.invalidations >= 1,
            "{st:?}"
        );
        assert_eq!(st.sequence_gaps, 1);
        assert!(st.pongs >= 2);
    }
}

#[cfg(all(test, feature = "live-tests"))]
mod live {
    use crate::Connector;
    use crate::bybit::BybitConnector;
    use std::time::{Duration, Instant};

    /// 3 liquid BTC options: first-message latency of the stream, stream top vs REST top, then a
    /// warm read served from the cache. Read-only public endpoints.
    #[tokio::test]
    async fn live_bybit_stream_vs_rest() {
        let rest = BybitConnector::default();
        let ws = BybitConnector::default().with_stream();
        let cat = rest.list_instruments("BTC").await.unwrap();
        let mut tops: Vec<_> = rest
            .bulk_top_of_book("BTC")
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .filter(|b| !b.bids.is_empty() && !b.asks.is_empty())
            .collect();
        tops.sort_by_key(|b| std::cmp::Reverse(b.bids[0].size.min(b.asks[0].size)));
        let mut checked = 0;
        for b in tops.iter().take(3) {
            let i = cat.iter().find(|i| i.key == b.instrument).unwrap();
            let t0 = Instant::now();
            let s = ws.get_market_data(i).await.unwrap();
            let first = t0.elapsed();
            let r = rest.get_market_data(i).await.unwrap();
            let t1 = Instant::now();
            let warm = ws.get_market_data(i).await.unwrap();
            let warm_t = t1.elapsed();
            println!(
                "{}: first {first:?} via {} | stream {:?}/{:?} rest {:?}/{:?} (rest {:?}) | warm {warm_t:?} via {} age {} ms | index {:?} mark {:?}",
                i.key.native_id,
                s.path,
                s.book.bids[0],
                s.book.asks[0],
                r.book.bids[0],
                r.book.asks[0],
                r.network,
                warm.path,
                (chrono::Utc::now() - warm.book.received_at).num_milliseconds(),
                s.book.index_price,
                s.book.mark_price,
            );
            assert_eq!(s.path, "ws_stream");
            assert_eq!(warm.path, "ws_stream");
            assert!(warm_t < Duration::from_millis(50));
            assert!(s.book.index_price.is_some() && s.book.mark_price.is_some());
            assert!(s.book.bids.windows(2).all(|w| w[0].price > w[1].price));
            assert!(s.book.asks.windows(2).all(|w| w[0].price < w[1].price));
            assert!(s.book.bids.len() <= 25 && s.book.asks.len() <= 25);
            // Same units as REST (USDT per contract); allow drift between the two reads.
            let close = |x: oa_core::Decimal, y: oa_core::Decimal| {
                (x - y).abs() <= y * oa_core::Decimal::new(5, 2)
            };
            assert!(close(s.book.bids[0].price, r.book.bids[0].price));
            assert!(close(s.book.asks[0].price, r.book.asks[0].price));
            checked += 1;
        }
        assert_eq!(checked, 3);
        println!("diagnostics {}", ws.diagnostics().unwrap());
    }

    /// Testnet host answers on the same protocol (book may be empty there).
    #[tokio::test]
    async fn live_bybit_testnet_stream() {
        let c = BybitConnector::testnet().with_stream();
        let cat = c.list_instruments("BTC").await.unwrap();
        let i = cat.iter().min_by_key(|i| i.expiry).unwrap();
        let t0 = Instant::now();
        let md = c.get_market_data(i).await.unwrap();
        println!(
            "testnet {} via {} in {:?}: {} bids / {} asks; {}",
            i.key.native_id,
            md.path,
            t0.elapsed(),
            md.book.bids.len(),
            md.book.asks.len(),
            c.diagnostics().unwrap()
        );
        assert_eq!(md.path, "ws_stream");
    }
}
