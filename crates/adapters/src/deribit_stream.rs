//! Persistent Deribit public WebSocket (JSON-RPC 2.0): ONE connection, on-demand subscriptions,
//! full-depth books rebuilt from incremental updates, automatic reconnection.
//!
//! Verified live on 2026-10-07 (`wss://www.deribit.com/ws/api/v2`, no keys):
//! - `public/subscribe {channels}` answers `result: [channels actually subscribed]`. An unknown
//!   instrument is silently left out (`result: []`), so a requested channel missing from the
//!   result is a refusal. ONE forbidden channel fails the WHOLE request: `book.{i}.raw` answers
//!   error 13778 `raw_subscriptions_not_available_for_unauthorized` -> raw is not usable here.
//! - `book.{i}.100ms`: first notification `type: "snapshot"` (levels `["new", price, amount]`),
//!   then `type: "change"` with `change_id` / `prev_change_id`, levels `["new"|"change"|"delete",
//!   price, amount]`, `timestamp` (ms). 236 changes on 15 instruments in 40 s, all continuous.
//!   Notifications are CHANGE-DRIVEN: a quiet book sends nothing for many seconds.
//! - `book.{i}.none.20.100ms` (grouped snapshots) is capped at 20 levels while REST serves 50 ->
//!   not used: the delta channel gives the full depth and lets us validate continuity.
//! - `ticker.{i}.100ms`: `index_price`, `mark_price`, `timestamp`; also change-driven.
//! - `public/set_heartbeat {interval: 10}` -> server sends `heartbeat {type: "heartbeat"}` every
//!   10 s and `heartbeat {type: "test_request"}`, which MUST be answered with `public/test`
//!   (answered `{"version": ..}`), otherwise the server closes the connection.
//!
//! Consequences implemented here:
//! - Sequence: a `change` is applied only if `prev_change_id` == our `change_id`. Older/equal ids
//!   are duplicates (dropped). Any other id is a GAP: the book is discarded (never served) and the
//!   book channel is unsubscribed + resubscribed to obtain a fresh snapshot. A change before the
//!   snapshot, an unknown action or a crossed book after an update also force a resync.
//! - Freshness: since nothing is sent while a book is unchanged, the age of the last BOOK message
//!   says nothing about staleness. What proves the cached book is current is that the (ordered)
//!   connection is alive: we send `public/test` every `PING_EVERY` and every frame received
//!   refreshes `confirmed_at`. The connector uses `confirmed_at` as `received_at`.
//! - On any disconnection every cached book/ticker is INVALIDATED, the connection is re-established
//!   with exponential backoff, the heartbeat re-armed and all subscriptions restored.
//! - Subscriptions are bounded (`MAX_INSTRUMENTS`, LRU) and idle ones dropped after `IDLE_UNSUBSCRIBE`.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use oa_core::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

/// Levels served per side: identical to the REST `get_order_book` depth.
pub const SERVED_DEPTH: usize = 50;
const HEARTBEAT_INTERVAL_S: u64 = 10;
/// No frame at all for this long (pings every second, heartbeat every 10 s) = dead connection.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const PING_EVERY: Duration = Duration::from_secs(1);
const IDLE_UNSUBSCRIBE: Duration = Duration::from_secs(600);
const MAX_INSTRUMENTS: usize = 200;
/// Channels per subscribe request (conservative batching).
const CHANNELS_PER_REQUEST: usize = 100;
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub(crate) fn book_channel(name: &str) -> String {
    format!("book.{name}.100ms")
}
pub(crate) fn ticker_channel(name: &str) -> String {
    format!("ticker.{name}.100ms")
}

type Side = BTreeMap<Decimal, (Value, Value)>;

#[derive(Debug, Clone)]
struct Book {
    /// price -> (original price JSON, original amount JSON): served byte-identical to REST.
    bids: Side,
    asks: Side,
    change_id: i64,
    timestamp: Value,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StreamStats {
    pub connected: bool,
    pub connects: u64,
    pub disconnects: u64,
    pub messages: u64,
    pub book_changes_applied: u64,
    pub sequence_gaps: u64,
    pub duplicates_or_out_of_order: u64,
    pub resyncs: u64,
    pub invalidations: u64,
    pub subscribe_rejections: u64,
    pub heartbeat_test_requests: u64,
    pub subscribed_instruments: usize,
    pub last_error: Option<String>,
}

enum Pending {
    Subscribe(Vec<String>),
    Other(&'static str),
}

#[derive(Default)]
struct State {
    books: HashMap<String, Book>,
    tickers: HashMap<String, Value>,
    /// Arrival of the last frame on the current connection (proves cached books are current).
    last_rx: Option<(DateTime<Utc>, Instant)>,
    /// instrument -> last use
    wanted: HashMap<String, Instant>,
    rejected: HashMap<String, String>,
    pending: HashMap<u64, Pending>,
    stats: StreamStats,
}

enum Cmd {
    Subscribe(String),
}

/// A served book, already in the REST `get_order_book` shape (`instrument_name`, `timestamp`,
/// `change_id`, `bids`/`asks` as `[price, amount]`, `index_price`, `mark_price`).
#[derive(Debug, Clone)]
pub struct Ready {
    pub book: Value,
    /// Arrival of the last frame on the live connection: the book is known current as of then.
    pub confirmed_at: DateTime<Utc>,
    pub confirmed_mono: Instant,
}

pub enum Snapshot {
    Ready(Ready),
    /// The platform refused the subscription (e.g. unknown instrument).
    Rejected(String),
    /// No data within the wait (disconnected, still subscribing, resyncing...).
    NotYet,
}

pub struct DeribitStream {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl DeribitStream {
    pub fn spawn(url: &str) -> Arc<Self> {
        Self::spawn_with(url, PING_EVERY)
    }

    fn spawn_with(url: &str, ping: Duration) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Arc::new(Self {
            state: Arc::new(Mutex::new(State::default())),
            notify: Arc::new(Notify::new()),
            cmd: tx,
        });
        tokio::spawn(run(
            url.to_string(),
            ping,
            s.state.clone(),
            s.notify.clone(),
            rx,
        ));
        s
    }

    pub fn stats(&self) -> StreamStats {
        let st = self.state.lock().expect("stream state");
        let mut s = st.stats.clone();
        s.subscribed_instruments = st.wanted.len();
        s
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().expect("stream state").stats.connected
    }

    /// Subscribe without waiting (prewarm).
    pub fn subscribe(&self, name: &str) {
        let mut st = self.state.lock().expect("stream state");
        let new = !st.wanted.contains_key(name);
        st.wanted.insert(name.to_string(), Instant::now());
        if new {
            let _ = self.cmd.send(Cmd::Subscribe(name.to_string()));
        }
    }

    /// Latest book (+ ticker index/mark) for `name`, subscribing on first use. Waits up to `wait`
    /// for book AND ticker. Never returns data received before the last disconnection or gap.
    pub async fn snapshot(&self, name: &str, wait: Duration) -> Snapshot {
        let deadline = Instant::now() + wait;
        {
            let mut st = self.state.lock().expect("stream state");
            st.rejected.remove(name);
        }
        self.subscribe(name);
        loop {
            let notified = self.notify.notified();
            {
                let st = self.state.lock().expect("stream state");
                if let Some(r) = st.rejected.get(name) {
                    return Snapshot::Rejected(r.clone());
                }
                if let (Some(b), Some(t), Some(rx)) =
                    (st.books.get(name), st.tickers.get(name), st.last_rx)
                {
                    return Snapshot::Ready(Ready {
                        book: render(name, b, t),
                        confirmed_at: rx.0,
                        confirmed_mono: rx.1,
                    });
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, notified).await.is_err() {
                return Snapshot::NotYet;
            }
        }
    }
}

/// REST-shaped book: bids descending, asks ascending, `SERVED_DEPTH` levels per side.
fn render(name: &str, b: &Book, ticker: &Value) -> Value {
    let lvl = |(p, a): &(Value, Value)| json!([p, a]);
    json!({
        "instrument_name": name,
        "timestamp": b.timestamp,
        "change_id": b.change_id,
        "bids": b.bids.values().rev().take(SERVED_DEPTH).map(lvl).collect::<Vec<_>>(),
        "asks": b.asks.values().take(SERVED_DEPTH).map(lvl).collect::<Vec<_>>(),
        "index_price": ticker["index_price"],
        "mark_price": ticker["mark_price"],
    })
}

fn invalidate(st: &mut State) {
    if !st.books.is_empty() || !st.tickers.is_empty() {
        st.stats.invalidations += 1;
    }
    st.books.clear();
    st.tickers.clear();
    st.last_rx = None;
    st.pending.clear();
}

fn reject(st: &mut State, name: &str, reason: String) {
    st.stats.subscribe_rejections += 1;
    st.rejected.insert(name.to_string(), reason);
    st.wanted.remove(name);
    st.books.remove(name);
    st.tickers.remove(name);
}

/// What the connection loop must do after a frame.
#[derive(Debug, Default, PartialEq)]
struct Outcome {
    changed: bool,
    /// Server sent `heartbeat/test_request`: answer `public/test` now.
    answer_test: bool,
    /// Instrument whose book must be resubscribed (gap / inconsistency).
    resync: Option<String>,
}

/// Apply one level update to a side. Errors on unknown action / unparsable number.
fn apply_level(side: &mut Side, l: &Value) -> Result<(), String> {
    let action = l[0].as_str().ok_or_else(|| format!("level action {l}"))?;
    let price = crate::dec(super::P, &l[1], "level.price").map_err(|e| e.to_string())?;
    let amount = crate::dec(super::P, &l[2], "level.amount").map_err(|e| e.to_string())?;
    match action {
        "new" | "change" if amount > Decimal::ZERO => {
            side.insert(price, (l[1].clone(), l[2].clone()));
        }
        "new" | "change" | "delete" => {
            side.remove(&price);
        }
        other => return Err(format!("unknown level action {other:?}")),
    }
    Ok(())
}

fn apply_levels(book: &mut Book, data: &Value) -> Result<(), String> {
    for (key, side) in [("bids", &mut book.bids), ("asks", &mut book.asks)] {
        let arr = data[key]
            .as_array()
            .ok_or_else(|| format!("{key} missing"))?;
        for l in arr {
            apply_level(side, l)?;
        }
    }
    if let (Some(bb), Some(ba)) = (book.bids.keys().next_back(), book.asks.keys().next())
        && bb >= ba
    {
        return Err(format!("crossed book bid {bb} >= ask {ba}"));
    }
    Ok(())
}

/// Apply one server frame.
fn apply(st: &mut State, v: &Value) -> Outcome {
    let mut out = Outcome::default();
    match v["method"].as_str() {
        Some("heartbeat") => {
            if v["params"]["type"] == "test_request" {
                st.stats.heartbeat_test_requests += 1;
                out.answer_test = true;
            }
            return out;
        }
        Some("subscription") => {}
        _ => {
            // Response to one of our requests.
            let Some(p) = v["id"].as_u64().and_then(|id| st.pending.remove(&id)) else {
                return out;
            };
            match p {
                Pending::Subscribe(chans) => {
                    if let Some(e) = v.get("error") {
                        st.stats.last_error = Some(format!("subscribe: {e}"));
                        for ch in &chans {
                            if let Some(n) = ch.split('.').nth(1) {
                                reject(st, n, format!("{ch}: {e}"));
                            }
                        }
                        out.changed = true;
                    } else if let Some(ok) = v["result"].as_array() {
                        for ch in &chans {
                            if !ok.iter().any(|c| c == ch.as_str())
                                && let Some(n) = ch.split('.').nth(1)
                                && st.wanted.contains_key(n)
                            {
                                reject(st, n, format!("{ch}: not subscribed by the venue"));
                                out.changed = true;
                            }
                        }
                    }
                }
                Pending::Other(what) => {
                    if let Some(e) = v.get("error") {
                        st.stats.last_error = Some(format!("{what}: {e}"));
                    }
                }
            }
            return out;
        }
    }
    let ch = v["params"]["channel"].as_str().unwrap_or("");
    let data = &v["params"]["data"];
    let mut parts = ch.split('.');
    let (kind, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    st.stats.messages += 1;
    if !st.wanted.contains_key(name) {
        return out; // late message after unsubscribe
    }
    match kind {
        "ticker" => {
            st.tickers.insert(name.to_string(), data.clone());
            out.changed = true;
        }
        "book" => {
            let change_id = data["change_id"].as_i64();
            let resync = |st: &mut State, out: &mut Outcome, why: String| {
                tracing::debug!(channel = ch, why, "deribit book resync");
                st.books.remove(name);
                st.stats.resyncs += 1;
                out.resync = Some(name.to_string());
                out.changed = true;
            };
            match (data["type"].as_str(), change_id) {
                (Some("snapshot"), Some(id)) => {
                    let mut b = Book {
                        bids: Side::new(),
                        asks: Side::new(),
                        change_id: id,
                        timestamp: data["timestamp"].clone(),
                    };
                    match apply_levels(&mut b, data) {
                        Ok(()) => {
                            st.books.insert(name.to_string(), b);
                            out.changed = true;
                        }
                        Err(e) => resync(st, &mut out, e),
                    }
                }
                (Some("change"), Some(id)) => {
                    let Some(b) = st.books.get_mut(name) else {
                        // Waiting for the (re)subscription snapshot: ignore.
                        return out;
                    };
                    if id <= b.change_id {
                        st.stats.duplicates_or_out_of_order += 1;
                        return out;
                    }
                    if data["prev_change_id"].as_i64() != Some(b.change_id) {
                        st.stats.sequence_gaps += 1;
                        let why = format!(
                            "gap: have {} got prev {}",
                            b.change_id, data["prev_change_id"]
                        );
                        resync(st, &mut out, why);
                        return out;
                    }
                    let mut next = b.clone();
                    next.change_id = id;
                    next.timestamp = data["timestamp"].clone();
                    match apply_levels(&mut next, data) {
                        Ok(()) => {
                            *b = next;
                            st.stats.book_changes_applied += 1;
                            out.changed = true;
                        }
                        Err(e) => resync(st, &mut out, e),
                    }
                }
                _ => resync(st, &mut out, format!("unexpected book frame {data}")),
            }
        }
        _ => {}
    }
    out
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Conn {
    ws: Ws,
    next_id: u64,
    state: Arc<Mutex<State>>,
}

impl Conn {
    async fn request(&mut self, method: &str, params: Value, p: Pending) -> bool {
        self.next_id += 1;
        let id = self.next_id;
        self.state
            .lock()
            .expect("stream state")
            .pending
            .insert(id, p);
        let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.ws.send(Message::text(m.to_string())).await.is_ok()
    }

    async fn subscribe(&mut self, chans: Vec<String>) -> bool {
        for c in chans.chunks(CHANNELS_PER_REQUEST) {
            let ok = self
                .request(
                    "public/subscribe",
                    json!({"channels": c}),
                    Pending::Subscribe(c.to_vec()),
                )
                .await;
            if !ok {
                return false;
            }
        }
        true
    }

    async fn unsubscribe(&mut self, chans: Vec<String>) -> bool {
        self.request(
            "public/unsubscribe",
            json!({"channels": chans}),
            Pending::Other("unsubscribe"),
        )
        .await
    }
}

fn both(names: &[String]) -> Vec<String> {
    names
        .iter()
        .flat_map(|n| [book_channel(n), ticker_channel(n)])
        .collect()
}

async fn run(
    url: String,
    ping: Duration,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut backoff = Duration::from_millis(500);
    loop {
        while state.lock().expect("stream state").wanted.is_empty() {
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
        let ws = match conn {
            Ok(Ok((ws, _))) => ws,
            other => {
                let e = match other {
                    Ok(Err(e)) => e.to_string(),
                    _ => "connect timeout".into(),
                };
                state.lock().expect("stream state").stats.last_error = Some(e);
                tokio::time::sleep(crate::http::jittered(backoff)).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        // Commands queued before the connection are covered by `wanted`.
        while rx.try_recv().is_ok() {}
        let names: Vec<String> = {
            let mut st = state.lock().expect("stream state");
            st.stats.connected = true;
            st.stats.connects += 1;
            st.wanted.keys().cloned().collect()
        };
        let mut c = Conn {
            ws,
            next_id: 0,
            state: state.clone(),
        };
        let mut ok = c
            .request(
                "public/set_heartbeat",
                json!({"interval": HEARTBEAT_INTERVAL_S}),
                Pending::Other("set_heartbeat"),
            )
            .await
            && c.subscribe(both(&names)).await;
        let mut pinger = tokio::time::interval(ping);
        pinger.reset();
        let mut housekeeping = tokio::time::interval(Duration::from_secs(30));
        // Dead-connection detection: the idle clock is reset ONLY by a received frame (the per-await
        // timeout below restarts whenever another branch fires, e.g. every ping, so on its own it never
        // caught a half-open connection whose pings still enter the kernel send buffer).
        let mut last_frame = Instant::now();
        while ok {
            tokio::select! {
                msg = tokio::time::timeout(IDLE_TIMEOUT, c.ws.next()) => {
                    if let Ok(Some(Ok(_))) = &msg {
                        last_frame = Instant::now();
                    }
                    match msg {
                        Ok(Some(Ok(Message::Text(t)))) => {
                            backoff = Duration::from_millis(500);
                            let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                            let out = {
                                let mut st = state.lock().expect("stream state");
                                st.last_rx = Some((Utc::now(), Instant::now()));
                                apply(&mut st, &v)
                            };
                            // Every frame refreshes `confirmed_at`: wake waiters regardless.
                            notify.notify_waiters();
                            if out.answer_test {
                                ok = c.request("public/test", json!({}), Pending::Other("test")).await;
                            }
                            if let Some(n) = out.resync {
                                ok = ok
                                    && c.unsubscribe(vec![book_channel(&n)]).await
                                    && c.subscribe(vec![book_channel(&n)]).await;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => { ok = false; }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            state.lock().expect("stream state").stats.last_error = Some(e.to_string());
                            ok = false;
                        }
                        Err(_) => {
                            state.lock().expect("stream state").stats.last_error =
                                Some(format!("idle timeout (no frame for {IDLE_TIMEOUT:?})"));
                            ok = false;
                        }
                    }
                }
                cmd = rx.recv() => {
                    let Some(Cmd::Subscribe(first)) = cmd else { return };
                    let mut names = vec![first];
                    while let Ok(Cmd::Subscribe(n)) = rx.try_recv() { names.push(n); }
                    ok = c.subscribe(both(&names)).await;
                }
                _ = pinger.tick() => {
                    if last_frame.elapsed() > IDLE_TIMEOUT {
                        state.lock().expect("stream state").stats.last_error =
                            Some(format!("idle timeout (no frame for {IDLE_TIMEOUT:?})"));
                        ok = false;
                    } else {
                        ok = c.request("public/test", json!({}), Pending::Other("ping")).await;
                    }
                }
                _ = housekeeping.tick() => {
                    let drop: Vec<String> = {
                        let mut st = state.lock().expect("stream state");
                        let mut by_age: Vec<(String, Instant)> =
                            st.wanted.iter().map(|(k, v)| (k.clone(), *v)).collect();
                        by_age.sort_by_key(|x| x.1);
                        let over = by_age.len().saturating_sub(MAX_INSTRUMENTS);
                        let drop: Vec<String> = by_age.iter().enumerate()
                            .filter(|(i, (_, t))| *i < over || t.elapsed() > IDLE_UNSUBSCRIBE)
                            .map(|(_, (k, _))| k.clone()).collect();
                        for k in &drop { st.wanted.remove(k); st.books.remove(k); st.tickers.remove(k); }
                        drop
                    };
                    if !drop.is_empty() {
                        ok = c.unsubscribe(both(&drop)).await;
                    }
                }
            }
        }
        {
            let mut st = state.lock().expect("stream state");
            st.stats.connected = false;
            st.stats.disconnects += 1;
            invalidate(&mut st);
        }
        notify.notify_waiters();
        tracing::warn!(?backoff, "deribit websocket disconnected; reconnecting");
        tokio::time::sleep(crate::http::jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oa_core::domain::Provenance;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const I: &str = "BTC_USDC-30OCT26-78000-P";

    fn note(ch: String, data: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "subscription", "params": {"channel": ch, "data": data}})
    }
    /// Shapes recorded live 2026-10-07 (see module docs).
    fn snap(name: &str, id: i64, bids: Value, asks: Value) -> Value {
        note(
            book_channel(name),
            json!({"timestamp": 1791374593823_i64, "type": "snapshot", "change_id": id,
                   "instrument_name": name, "bids": bids, "asks": asks}),
        )
    }
    fn change(name: &str, prev: i64, id: i64, bids: Value, asks: Value) -> Value {
        note(
            book_channel(name),
            json!({"timestamp": 1791374634004_i64, "type": "change", "change_id": id,
                   "prev_change_id": prev, "instrument_name": name, "bids": bids, "asks": asks}),
        )
    }
    fn ticker(name: &str, index: f64, mark: f64) -> Value {
        note(
            ticker_channel(name),
            json!({"timestamp": 1791374593823_i64, "instrument_name": name,
                   "index_price": index, "mark_price": mark, "best_ask_price": 3080.0}),
        )
    }
    fn state_with(name: &str) -> State {
        let mut st = State::default();
        st.wanted.insert(name.into(), Instant::now());
        st
    }

    #[test]
    fn snapshot_then_continuous_changes() {
        let mut st = state_with(I);
        let o = apply(
            &mut st,
            &snap(
                I,
                100,
                json!([["new", 755.0, 10.0], ["new", 750.0, 5.0]]),
                json!([["new", 780.0, 2.0]]),
            ),
        );
        assert!(o.changed && o.resync.is_none());
        // Recorded change shape: modify + delete.
        let o = apply(
            &mut st,
            &change(
                I,
                100,
                105,
                json!([["change", 755.0, 29.79], ["delete", 750.0, 0.0]]),
                json!([]),
            ),
        );
        assert!(o.changed && o.resync.is_none());
        apply(&mut st, &ticker(I, 83610.84, 760.5));
        let b = render(I, &st.books[I], &st.tickers[I]);
        assert_eq!(b["bids"], json!([[755.0, 29.79]]));
        assert_eq!(b["asks"], json!([[780.0, 2.0]]));
        assert_eq!(b["change_id"], 105);
        assert_eq!(b["timestamp"], 1791374634004_i64);
        assert_eq!(b["index_price"], 83610.84);
        assert_eq!(st.stats.book_changes_applied, 1);
    }

    #[test]
    fn duplicates_dropped_gap_forces_resync() {
        let mut st = state_with(I);
        apply(
            &mut st,
            &snap(I, 100, json!([]), json!([["new", 780.0, 2.0]])),
        );
        apply(
            &mut st,
            &change(I, 100, 101, json!([]), json!([["new", 779.0, 1.0]])),
        );
        let dup = apply(
            &mut st,
            &change(I, 100, 101, json!([]), json!([["new", 1.0, 1.0]])),
        );
        assert!(!dup.changed, "duplicate dropped");
        assert_eq!(st.stats.duplicates_or_out_of_order, 1);
        assert_eq!(st.books[I].asks.len(), 2);
        // prev 150 != 101: gap -> book discarded, resubscribe requested.
        let gap = apply(
            &mut st,
            &change(I, 150, 160, json!([]), json!([["new", 778.0, 1.0]])),
        );
        assert_eq!(gap.resync.as_deref(), Some(I));
        assert!(
            !st.books.contains_key(I),
            "book after a gap must not be served"
        );
        assert_eq!(st.stats.sequence_gaps, 1);
        // Changes before the new snapshot are ignored; the snapshot restores the book.
        assert_eq!(
            apply(&mut st, &change(I, 160, 161, json!([]), json!([]))),
            Outcome::default()
        );
        apply(
            &mut st,
            &snap(I, 200, json!([]), json!([["new", 781.0, 3.0]])),
        );
        assert_eq!(st.books[I].change_id, 200);
    }

    #[test]
    fn crossed_book_and_unknown_action_resync() {
        let mut st = state_with(I);
        apply(
            &mut st,
            &snap(
                I,
                1,
                json!([["new", 700.0, 1.0]]),
                json!([["new", 780.0, 2.0]]),
            ),
        );
        let o = apply(
            &mut st,
            &change(I, 1, 2, json!([["new", 790.0, 1.0]]), json!([])),
        );
        assert!(o.resync.is_some() && !st.books.contains_key(I));
        apply(&mut st, &snap(I, 3, json!([]), json!([])));
        let o = apply(
            &mut st,
            &change(I, 3, 4, json!([["upsert", 1.0, 1.0]]), json!([])),
        );
        assert!(o.resync.is_some());
    }

    #[test]
    fn depth_capped_and_sorted_like_rest() {
        let mut st = state_with(I);
        let bids: Vec<Value> = (1..=60).map(|p| json!(["new", p as f64, 1.0])).collect();
        let asks: Vec<Value> = (100..160)
            .rev()
            .map(|p| json!(["new", p as f64, 1.0]))
            .collect();
        apply(&mut st, &snap(I, 1, json!(bids), json!(asks)));
        apply(&mut st, &ticker(I, 1.0, 1.0));
        let b = render(I, &st.books[I], &st.tickers[I]);
        let (bb, aa) = (b["bids"].as_array().unwrap(), b["asks"].as_array().unwrap());
        assert_eq!((bb.len(), aa.len()), (SERVED_DEPTH, SERVED_DEPTH));
        assert_eq!(bb[0][0], 60.0);
        assert_eq!(aa[0][0], 100.0);
        assert_eq!(
            SERVED_DEPTH.to_string(),
            super::super::BOOK_DEPTH,
            "same depth as REST"
        );
    }

    #[test]
    fn subscribe_ack_errors_and_heartbeat() {
        let mut st = state_with("GOOD");
        st.wanted.insert("BAD".into(), Instant::now());
        st.pending.insert(
            7,
            Pending::Subscribe(vec![book_channel("GOOD"), book_channel("BAD")]),
        );
        // Recorded: unknown instruments are silently absent from the result.
        apply(
            &mut st,
            &json!({"jsonrpc": "2.0", "id": 7, "result": [book_channel("GOOD")]}),
        );
        assert!(st.rejected["BAD"].contains("not subscribed"));
        assert!(!st.wanted.contains_key("BAD") && st.wanted.contains_key("GOOD"));
        st.pending
            .insert(8, Pending::Subscribe(vec![book_channel("GOOD")]));
        apply(
            &mut st,
            &json!({"jsonrpc": "2.0", "id": 8, "error": {"code": 13778,
                    "message": "raw_subscriptions_not_available_for_unauthorized"}}),
        );
        assert!(st.rejected["GOOD"].contains("13778"));
        let hb =
            json!({"params": {"type": "test_request"}, "method": "heartbeat", "jsonrpc": "2.0"});
        assert!(apply(&mut st, &hb).answer_test);
        let hb = json!({"params": {"type": "heartbeat"}, "method": "heartbeat", "jsonrpc": "2.0"});
        assert!(!apply(&mut st, &hb).answer_test);
    }

    /// The REST book fixture replayed as a WS snapshot + ticker must parse into the SAME
    /// canonical book through the shared `parse_book` (ids, units, index/mark, source_ts).
    #[test]
    fn stream_book_identical_to_rest_book() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/recorded-2026-10-06");
        let read = |f: &str| -> Value {
            serde_json::from_str(&std::fs::read_to_string(root.join(f)).unwrap()).unwrap()
        };
        let rest = read("deribit_book_btc_usdc.json")["result"].clone();
        let name = rest["instrument_name"].as_str().unwrap().to_string();
        let inst = super::super::parse_instruments(
            &read("deribit_instruments_usdc.json")["result"],
            "BTC",
            "2026-10-06T09:00:00Z".parse().unwrap(),
        )
        .unwrap()
        .into_iter()
        .find(|i| i.key.native_id == name)
        .unwrap();
        let tag = |side: &Value| -> Value {
            side.as_array()
                .unwrap()
                .iter()
                .map(|l| json!(["new", l[0], l[1]]))
                .collect()
        };
        let mut st = state_with(&name);
        let mut s = snap(
            &name,
            rest["change_id"].as_i64().unwrap(),
            tag(&rest["bids"]),
            tag(&rest["asks"]),
        );
        s["params"]["data"]["timestamp"] = rest["timestamp"].clone();
        apply(&mut st, &s);
        apply(
            &mut st,
            &note(
                ticker_channel(&name),
                json!({"index_price": rest["index_price"], "mark_price": rest["mark_price"]}),
            ),
        );
        let ws = render(&name, &st.books[&name], &st.tickers[&name]);
        let at = Utc::now();
        let a = super::super::parse_book(&inst, &rest, at, Provenance::Fixture).unwrap();
        let b = super::super::parse_book(&inst, &ws, at, Provenance::Fixture).unwrap();
        assert_eq!(a.bids, b.bids);
        assert_eq!(a.asks, b.asks);
        assert_eq!(a.index_price, b.index_price);
        assert_eq!(a.mark_price, b.mark_price);
        assert_eq!(a.source_ts, b.source_ts);
        assert_eq!(a.instrument, b.instrument);
    }

    async fn next_req(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Value {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
                Some(Ok(_)) => {}
                other => panic!("client gone: {other:?}"),
            }
        }
    }

    /// Fake Deribit: round 0 = heartbeat setup, subscribe ack, test_request must be answered,
    /// one book, then drop. Round 1 = resubscribe, book, sequence gap -> client must unsubscribe
    /// + resubscribe the book channel, then the fresh snapshot is served (never the old ones).
    #[tokio::test]
    async fn heartbeat_reconnect_invalidate_and_gap_resync() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<&'static str>();
        tokio::spawn(async move {
            for round in 0..2 {
                let (s, _) = l.accept().await.unwrap();
                let mut ws = accept_async(s).await.unwrap();
                let hb = next_req(&mut ws).await;
                assert_eq!(hb["method"], "public/set_heartbeat");
                assert_eq!(hb["params"]["interval"], 10);
                ws.send(Message::text(
                    json!({"jsonrpc":"2.0","id":hb["id"],"result":"ok"}).to_string(),
                ))
                .await
                .unwrap();
                let sub = next_req(&mut ws).await;
                assert_eq!(sub["method"], "public/subscribe");
                let chans = sub["params"]["channels"].clone();
                assert!(
                    chans
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c == "book.X.100ms")
                );
                ws.send(Message::text(
                    json!({"jsonrpc":"2.0","id":sub["id"],"result":chans}).to_string(),
                ))
                .await
                .unwrap();
                if round == 0 {
                    ws.send(Message::text(
                        json!({"params":{"type":"test_request"},"method":"heartbeat","jsonrpc":"2.0"}).to_string(),
                    ))
                    .await
                    .unwrap();
                    let t = next_req(&mut ws).await;
                    assert_eq!(t["method"], "public/test");
                    seen_tx.send("test_answered").unwrap();
                    ws.send(Message::text(
                        json!({"jsonrpc":"2.0","id":t["id"],"result":{"version":"1"}}).to_string(),
                    ))
                    .await
                    .unwrap();
                }
                let price = if round == 0 { 100.0 } else { 200.0 };
                for m in [
                    ticker("X", 1.0, 2.0),
                    snap("X", 10, json!([]), json!([["new", price, 1.0]])),
                ] {
                    ws.send(Message::text(m.to_string())).await.unwrap();
                }
                if round == 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = ws.close(None).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                ws.send(Message::text(
                    change("X", 99, 120, json!([]), json!([["new", 150.0, 1.0]])).to_string(),
                ))
                .await
                .unwrap();
                let un = next_req(&mut ws).await;
                assert_eq!(un["method"], "public/unsubscribe");
                assert_eq!(un["params"]["channels"], json!(["book.X.100ms"]));
                let re = next_req(&mut ws).await;
                assert_eq!(re["method"], "public/subscribe");
                assert_eq!(re["params"]["channels"], json!(["book.X.100ms"]));
                seen_tx.send("resynced").unwrap();
                ws.send(Message::text(
                    json!({"jsonrpc":"2.0","id":re["id"],"result":["book.X.100ms"]}).to_string(),
                ))
                .await
                .unwrap();
                ws.send(Message::text(
                    snap("X", 300, json!([]), json!([["new", 300.0, 1.0]])).to_string(),
                ))
                .await
                .unwrap();
                // Keep alive with frames (client idle timeout is 5 s).
                for _ in 0..30 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let hb =
                        json!({"params":{"type":"heartbeat"},"method":"heartbeat","jsonrpc":"2.0"});
                    if ws.send(Message::text(hb.to_string())).await.is_err() {
                        break;
                    }
                }
            }
        });
        let s = DeribitStream::spawn_with(&url, Duration::from_secs(3600));
        let ask = |r: &Ready| r.book["asks"][0][0].as_f64().unwrap();
        match s.snapshot("X", Duration::from_secs(3)).await {
            Snapshot::Ready(r) => {
                assert_eq!(ask(&r), 100.0);
                assert_eq!(r.book["index_price"], 1.0);
            }
            _ => panic!("first snapshot"),
        }
        assert_eq!(seen_rx.recv().await, Some("test_answered"));
        let mut asks = vec![];
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            // Yield: a cached snapshot returns without awaiting (single-threaded test runtime).
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Snapshot::Ready(r) = s.snapshot("X", Duration::from_millis(100)).await {
                let a = ask(&r);
                if asks.last() != Some(&a) {
                    asks.push(a);
                }
                if a == 300.0 {
                    break;
                }
            }
        }
        assert_eq!(asks.last(), Some(&300.0), "{asks:?} {:?}", s.stats());
        assert_eq!(seen_rx.recv().await, Some("resynced"));
        // After the drop only post-reconnect books: 200 (maybe), never 100 again, never the gapped 150.
        assert!(
            asks.first() == Some(&100.0) || asks.first() == Some(&200.0),
            "{asks:?}"
        );
        assert!(
            !asks[1..].contains(&100.0) && !asks.contains(&150.0),
            "{asks:?}"
        );
        assert_eq!(asks.last(), Some(&300.0), "{asks:?}");
        let st = s.stats();
        assert!(
            st.connects >= 2
                && st.disconnects >= 1
                && st.invalidations >= 1
                && st.sequence_gaps == 1
                && st.heartbeat_test_requests == 1,
            "{st:?}"
        );
    }

    /// Live, read-only: 3 liquid BTC_USDC options via the stream vs REST.
    #[cfg(feature = "live-tests")]
    #[tokio::test]
    async fn live_stream_vs_rest() {
        use super::super::DeribitConnector;
        use crate::Connector;
        let rest = DeribitConnector::default();
        let summary = rest
            .get(
                "/public/get_book_summary_by_currency",
                &[("currency", "USDC"), ("kind", "option")],
            )
            .await
            .unwrap();
        let mut liquid: Vec<&Value> = summary
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| {
                x["instrument_name"]
                    .as_str()
                    .unwrap()
                    .starts_with("BTC_USDC-")
                    && x["bid_price"].as_f64().is_some()
                    && x["ask_price"].as_f64().is_some()
            })
            .collect();
        liquid.sort_by(|a, b| {
            b["volume_usd"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["volume_usd"].as_f64().unwrap_or(0.0))
        });
        let names: Vec<String> = liquid
            .iter()
            .take(3)
            .map(|x| x["instrument_name"].as_str().unwrap().to_string())
            .collect();
        let cat = rest.list_instruments("BTC").await.unwrap();
        let insts: Vec<_> = names
            .iter()
            .map(|n| cat.iter().find(|i| &i.key.native_id == n).unwrap().clone())
            .collect();
        let c = DeribitConnector::default().with_stream();
        let t0 = Instant::now();
        c.prewarm(&insts);
        let stream = c.stream.as_ref().unwrap();
        for n in &names {
            let first = loop {
                if let Snapshot::Ready(_) = stream.snapshot(n, Duration::from_secs(5)).await {
                    break t0.elapsed();
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(15),
                    "no stream data for {n}"
                );
            };
            println!(
                "{n}: first book+ticker {} ms after prewarm (incl. connect)",
                first.as_millis()
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        for i in &insts {
            let t = Instant::now();
            let s = c.get_market_data(i).await.unwrap();
            let served = t.elapsed();
            let r = rest.get_market_data(i).await.unwrap();
            assert_eq!(s.path, "ws_stream");
            assert_eq!(r.path, "rest");
            let top = |b: &oa_core::domain::BookSnapshot| {
                (
                    b.bids.first().map(|l| l.price),
                    b.asks.first().map(|l| l.price),
                )
            };
            println!(
                "{}: stream {:?} (served in {:?}, confirmed {} ms ago, {} lvls) | rest {:?} ({:?}) | idx {:?}/{:?} mark {:?}/{:?}",
                i.key.native_id,
                top(&s.book),
                served,
                (Utc::now() - s.book.received_at).num_milliseconds(),
                s.book.bids.len() + s.book.asks.len(),
                top(&r.book),
                r.network,
                s.book.index_price,
                r.book.index_price,
                s.book.mark_price,
                r.book.mark_price,
            );
            let near = |a: Option<Decimal>, b: Option<Decimal>| match (a, b) {
                (Some(a), Some(b)) => ((a - b) / b).abs() < Decimal::new(5, 2),
                (a, b) => a == b,
            };
            let (sb, sa) = top(&s.book);
            let (rb, ra) = top(&r.book);
            assert!(near(sb, rb) && near(sa, ra), "stream top far from REST top");
            assert!(
                served < Duration::from_millis(20),
                "stream read must be local"
            );
        }
        println!("diagnostics {}", c.diagnostics().unwrap());
    }
}
