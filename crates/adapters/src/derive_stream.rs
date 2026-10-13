//! Persistent Derive WebSocket: ONE connection, on-demand subscriptions, latest full snapshot
//! per channel, automatic reconnection.
//!
//! Verified live on 2026-10-05:
//! - `orderbook.{name}.{group}.{depth}` sends FULL depth-limited snapshots ~1-2/s, even when the
//!   book is empty or unchanged; `publish_id` is sequential PER CHANNEL (+1 per message).
//! - `ticker_slim.{name}.100` sends `instrument_ticker` with `I` (index) and `M` (mark) every 100 ms.
//!   (2026-10-06: first ticker after subscribe 216-450 ms at .100 vs ~1,010 ms at .1000 -> faster cold quotes.)
//!
//! Consequences implemented here:
//! - Each book message is complete, so a sequence GAP (publish_id jump > 1) means we missed
//!   intermediate snapshots, not that the book is corrupt: the new message is a consistent resync.
//!   Gaps are counted and logged. DUPLICATE or OUT-OF-ORDER messages (publish_id <= last) are dropped.
//! - Heartbeat: with at least one subscription, silence for `IDLE_TIMEOUT` means a dead connection.
//! - On any disconnection every cached snapshot is INVALIDATED (never served after a drop), the
//!   connection is re-established with exponential backoff and all subscriptions are restored.
//! - Subscriptions are bounded (`MAX_INSTRUMENTS`, least-recently-used evicted) and idle ones are
//!   dropped after `IDLE_UNSUBSCRIBE`.
//! - Data age is exposed: every entry keeps its reception time; the connector puts it in
//!   `BookSnapshot.received_at`, so freshness checks use the real age of the data.

use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub const BOOK_GROUP: u32 = 1;
pub const BOOK_DEPTH: u32 = 100;
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_UNSUBSCRIBE: Duration = Duration::from_secs(600);
const MAX_INSTRUMENTS: usize = 150;
/// Reconnect ceiling (jittered). 60 s: after an outage the baseline venue is back within a minute,
/// still far from hammering it.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

fn book_channel(name: &str) -> String {
    format!("orderbook.{name}.{BOOK_GROUP}.{BOOK_DEPTH}")
}
fn ticker_channel(name: &str) -> String {
    format!("ticker_slim.{name}.100")
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub data: Value,
    pub received_at: DateTime<Utc>,
    pub mono: Instant,
    pub publish_id: Option<i64>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct StreamStats {
    pub connected: bool,
    pub connects: u64,
    pub disconnects: u64,
    pub messages: u64,
    pub sequence_gaps: u64,
    pub duplicates_or_out_of_order: u64,
    pub invalidations: u64,
    pub subscribe_rejections: u64,
    pub subscribed_instruments: usize,
    pub last_error: Option<String>,
}

#[derive(Default)]
struct State {
    books: HashMap<String, Entry>,
    tickers: HashMap<String, Entry>,
    /// instrument -> last use
    wanted: HashMap<String, Instant>,
    rejected: HashMap<String, String>,
    stats: StreamStats,
}

enum Cmd {
    Subscribe(String),
}

pub struct DeriveStream {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

/// Result of a snapshot request.
pub enum Snapshot {
    Ready {
        book: Entry,
        ticker: Option<Entry>,
    },
    /// The platform refused the subscription (e.g. unknown instrument).
    Rejected(String),
    /// No data within the wait (disconnected, still subscribing...).
    NotYet,
}

impl DeriveStream {
    pub fn spawn(url: &str) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let s = Arc::new(Self {
            state: Arc::new(Mutex::new(State::default())),
            notify: Arc::new(Notify::new()),
            cmd: tx,
        });
        tokio::spawn(run(url.to_string(), s.state.clone(), s.notify.clone(), rx));
        s
    }

    pub fn stats(&self) -> StreamStats {
        let st = self.state.lock().expect("stream state");
        let mut s = st.stats.clone();
        s.subscribed_instruments = st.wanted.len();
        s
    }

    /// Latest book (+ ticker) for `name`, subscribing on first use. Waits up to `wait` for the
    /// first message. Never returns data received before the last disconnection.
    /// Subscribe without waiting (prewarm): the book is pushed into the cache when it arrives.
    pub fn subscribe(&self, name: &str) {
        let mut st = self.state.lock().expect("stream state");
        let new = !st.wanted.contains_key(name);
        st.wanted.insert(name.to_string(), Instant::now());
        if new {
            let _ = self.cmd.send(Cmd::Subscribe(name.to_string()));
        }
    }

    pub async fn snapshot(&self, name: &str, wait: Duration) -> Snapshot {
        let deadline = Instant::now() + wait;
        {
            let mut st = self.state.lock().expect("stream state");
            st.rejected.remove(name);
            let new = !st.wanted.contains_key(name);
            st.wanted.insert(name.to_string(), Instant::now());
            if new {
                let _ = self.cmd.send(Cmd::Subscribe(name.to_string()));
            }
        }
        loop {
            let notified = self.notify.notified();
            {
                let st = self.state.lock().expect("stream state");
                if let Some(r) = st.rejected.get(name) {
                    return Snapshot::Rejected(r.clone());
                }
                if let Some(b) = st.books.get(name) {
                    return Snapshot::Ready {
                        book: b.clone(),
                        ticker: st.tickers.get(name).cloned(),
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

fn invalidate(st: &mut State) {
    if !st.books.is_empty() || !st.tickers.is_empty() {
        st.stats.invalidations += 1;
    }
    st.books.clear();
    st.tickers.clear();
}

/// Apply one server message. Returns true if state changed.
fn apply(st: &mut State, v: &Value) -> bool {
    if v.get("method") != Some(&json!("subscription")) {
        // Subscription acknowledgements: record refused channels.
        if let Some(status) = v["result"]["status"].as_object() {
            for (ch, s) in status {
                // "already subscribed" means the channel IS live (duplicate request, e.g. after a
                // reconnect race): not a refusal.
                if s != "ok"
                    && s != "already subscribed"
                    && let Some(name) = ch.split('.').nth(1)
                {
                    st.stats.subscribe_rejections += 1;
                    st.rejected.insert(name.to_string(), format!("{ch}: {s}"));
                    st.wanted.remove(name);
                }
            }
            return !status.is_empty();
        }
        return false;
    }
    let ch = v["params"]["channel"].as_str().unwrap_or("");
    let data = &v["params"]["data"];
    let mut parts = ch.splitn(3, '.');
    let (kind, name) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or("").to_string(),
    );
    st.stats.messages += 1;
    let now = (Utc::now(), Instant::now());
    match kind {
        "orderbook" => {
            let pid = data["publish_id"].as_i64();
            if let (Some(p), Some(prev)) = (pid, st.books.get(&name).and_then(|e| e.publish_id)) {
                if p <= prev {
                    st.stats.duplicates_or_out_of_order += 1;
                    return false;
                }
                if p > prev + 1 {
                    st.stats.sequence_gaps += 1;
                    tracing::debug!(
                        channel = ch,
                        prev,
                        p,
                        "derive book sequence gap (full snapshot resyncs)"
                    );
                }
            }
            st.books.insert(
                name,
                Entry {
                    data: data.clone(),
                    received_at: now.0,
                    mono: now.1,
                    publish_id: pid,
                },
            );
            true
        }
        "ticker_slim" => {
            st.tickers.insert(
                name,
                Entry {
                    data: data.clone(),
                    received_at: now.0,
                    mono: now.1,
                    publish_id: None,
                },
            );
            true
        }
        _ => false,
    }
}

async fn run(
    url: String,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut next_id: u64 = 1;
    loop {
        // Wait for at least one subscription before connecting.
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
        let mut ws = match conn {
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
        let names: Vec<String> = {
            let mut st = state.lock().expect("stream state");
            st.stats.connected = true;
            st.stats.connects += 1;
            st.wanted.keys().cloned().collect()
        };
        let mut ok = true;
        if !names.is_empty() {
            let chans: Vec<String> = names
                .iter()
                .flat_map(|n| [book_channel(n), ticker_channel(n)])
                .collect();
            next_id += 1;
            ok = ws
                .send(Message::text(
                    json!({"id": next_id, "method": "subscribe", "params": {"channels": chans}})
                        .to_string(),
                ))
                .await
                .is_ok();
        }
        let mut housekeeping = tokio::time::interval(Duration::from_secs(30));
        while ok {
            tokio::select! {
                msg = tokio::time::timeout(IDLE_TIMEOUT, ws.next()) => {
                    match msg {
                        Ok(Some(Ok(Message::Text(t)))) => {
                            backoff = Duration::from_millis(500);
                            if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                                let changed = apply(&mut state.lock().expect("stream state"), &v);
                                if changed { notify.notify_waiters(); }
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => { ok = false; }
                        Ok(Some(Ok(_))) => {}
                        Ok(Some(Err(e))) => {
                            state.lock().expect("stream state").stats.last_error = Some(e.to_string());
                            ok = false;
                        }
                        Err(_) => {
                            state.lock().expect("stream state").stats.last_error = Some("idle timeout (no message for 10 s)".into());
                            ok = false;
                        }
                    }
                }
                cmd = rx.recv() => {
                    match cmd {
                        Some(Cmd::Subscribe(n)) => {
                            next_id += 1;
                            let sub = json!({"id": next_id, "method": "subscribe",
                                             "params": {"channels": [book_channel(&n), ticker_channel(&n)]}});
                            if ws.send(Message::text(sub.to_string())).await.is_err() { ok = false; }
                        }
                        None => return,
                    }
                }
                _ = housekeeping.tick() => {
                    // Drop idle subscriptions and enforce the bound (LRU).
                    let drop: Vec<String> = {
                        let mut st = state.lock().expect("stream state");
                        let mut by_age: Vec<(String, Instant)> = st.wanted.iter().map(|(k, v)| (k.clone(), *v)).collect();
                        by_age.sort_by_key(|x| x.1);
                        let over = by_age.len().saturating_sub(MAX_INSTRUMENTS);
                        let drop: Vec<String> = by_age.iter().enumerate()
                            .filter(|(i, (_, t))| *i < over || t.elapsed() > IDLE_UNSUBSCRIBE)
                            .map(|(_, (k, _))| k.clone()).collect();
                        for k in &drop { st.wanted.remove(k); st.books.remove(k); st.tickers.remove(k); }
                        drop
                    };
                    if !drop.is_empty() {
                        next_id += 1;
                        let chans: Vec<String> = drop.iter().flat_map(|n| [book_channel(n), ticker_channel(n)]).collect();
                        let m = json!({"id": next_id, "method": "unsubscribe", "params": {"channels": chans}});
                        if ws.send(Message::text(m.to_string())).await.is_err() { ok = false; }
                    }
                }
            }
        }
        // Disconnected: invalidate everything, then reconnect with backoff.
        {
            let mut st = state.lock().expect("stream state");
            st.stats.connected = false;
            st.stats.disconnects += 1;
            invalidate(&mut st);
        }
        notify.notify_waiters();
        tracing::warn!(?backoff, "derive websocket disconnected; reconnecting");
        tokio::time::sleep(crate::http::jittered(backoff)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    fn book_msg(name: &str, pid: i64, ask: &str) -> String {
        json!({"method": "subscription", "params": {"channel": book_channel(name), "data": {
            "timestamp": 1, "instrument_name": name, "publish_id": pid,
            "bids": [], "asks": [[ask, "1"]]}}})
        .to_string()
    }

    #[test]
    fn duplicates_out_of_order_and_gaps() {
        let mut st = State::default();
        let p = |s: String| serde_json::from_str::<Value>(&s).unwrap();
        assert!(apply(&mut st, &p(book_msg("X", 10, "1"))));
        assert!(
            !apply(&mut st, &p(book_msg("X", 10, "2"))),
            "duplicate dropped"
        );
        assert!(
            !apply(&mut st, &p(book_msg("X", 9, "3"))),
            "out of order dropped"
        );
        assert_eq!(st.books["X"].data["asks"][0][0], "1");
        assert!(
            apply(&mut st, &p(book_msg("X", 13, "4"))),
            "gap: newer full snapshot accepted"
        );
        assert_eq!(st.stats.sequence_gaps, 1);
        assert_eq!(st.stats.duplicates_or_out_of_order, 2);
        assert_eq!(st.books["X"].data["asks"][0][0], "4");
    }

    #[test]
    fn already_subscribed_is_not_a_rejection() {
        let mut st = State::default();
        st.wanted.insert("X".into(), Instant::now());
        apply(
            &mut st,
            &json!({"id": 1, "result": {"status": {"ticker_slim.X.100": "already subscribed", "orderbook.X.1.100": "ok"}}}),
        );
        assert!(st.rejected.is_empty());
        assert!(st.wanted.contains_key("X"));
        assert_eq!(st.stats.subscribe_rejections, 0);
    }

    #[test]
    fn rejected_subscription_is_reported() {
        let mut st = State::default();
        st.wanted.insert("BAD".into(), Instant::now());
        let ack =
            json!({"id": 2, "result": {"status": {"orderbook.BAD.1.100": "Invalid channel"}}});
        apply(&mut st, &ack);
        assert!(st.rejected["BAD"].contains("Invalid channel"));
        assert!(!st.wanted.contains_key("BAD"));
    }

    /// Local WS server: serves one snapshot, then drops the connection. The client must
    /// invalidate the cached book, reconnect, resubscribe and serve the NEW snapshot only.
    #[tokio::test]
    async fn reconnects_resubscribes_and_invalidates() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            for round in 0..2 {
                let (s, _) = l.accept().await.unwrap();
                let mut ws = accept_async(s).await.unwrap();
                // Wait for the subscribe request, acknowledge, send one book.
                let req: Value = loop {
                    if let Some(Ok(Message::Text(t))) = ws.next().await {
                        break serde_json::from_str(t.as_str()).unwrap();
                    }
                };
                let chans = req["params"]["channels"].clone();
                assert!(
                    chans
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c == "orderbook.X.1.100")
                );
                let ack =
                    json!({"id": req["id"], "result": {"status": {"orderbook.X.1.100": "ok"}}});
                ws.send(Message::text(ack.to_string())).await.unwrap();
                ws.send(Message::text(book_msg(
                    "X",
                    1,
                    if round == 0 { "100" } else { "200" },
                )))
                .await
                .unwrap();
                if round == 0 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = ws.close(None).await; // drop the connection
                } else {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });
        let s = DeriveStream::spawn(&url);
        match s.snapshot("X", Duration::from_secs(3)).await {
            Snapshot::Ready { book, .. } => assert_eq!(book.data["asks"][0][0], "100"),
            _ => panic!("first snapshot"),
        }
        // After the drop, the old book must never be served; the new one arrives after reconnect.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let mut got = None;
        for _ in 0..30 {
            if let Snapshot::Ready { book, .. } = s.snapshot("X", Duration::from_millis(200)).await
            {
                assert_ne!(
                    book.data["asks"][0][0], "100",
                    "stale pre-disconnect book served"
                );
                got = Some(book);
                break;
            }
        }
        assert_eq!(
            got.expect("snapshot after reconnect").data["asks"][0][0],
            "200"
        );
        let st = s.stats();
        assert!(
            st.connects >= 2 && st.disconnects >= 1 && st.invalidations >= 1,
            "{st:?}"
        );
    }
}
