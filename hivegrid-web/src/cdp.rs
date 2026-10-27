//! Chrome DevTools Protocol over `--remote-debugging-pipe`.
//!
//! Chrome reads commands from fd 3 and writes responses and events to fd 4; every message is
//! one JSON document terminated by a NUL byte. This module owns that framing, request ids and
//! the routing of responses (by id) and events (by flattened `sessionId`). It knows nothing
//! about browsers or pages, so it runs unchanged over a socketpair in the tests.
//!
//! Threads: one reader (parses and dispatches) and one writer (drains a queue), so no caller
//! ever blocks on the pipe and the reader never blocks on a command it is answering. Response
//! callbacks and event sinks run on the reader thread and must be quick; anything slow belongs
//! on a thread of the sink's own.

use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// A CDP event: `{"method":..,"params":..,"sessionId":..}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// `None` for browser-level events.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CdpError {
    /// Chrome answered with an `error` object.
    Protocol { code: i64, message: String },
    /// The pipe is gone (Chrome exited or the connection was closed).
    Closed,
    /// No answer within the caller's deadline.
    Timeout,
}

impl fmt::Display for CdpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CdpError::Protocol { code, message } => write!(f, "CDP error {code}: {message}"),
            CdpError::Closed => f.write_str("CDP connection closed"),
            CdpError::Timeout => f.write_str("CDP request timed out"),
        }
    }
}

impl std::error::Error for CdpError {}

pub type Callback = Box<dyn FnOnce(Result<Value, CdpError>) + Send>;
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// Largest CDP message kept (bytes). A bigger one (a page can make Chrome send arbitrarily large
/// strings: `console.log`, a binding call, a title) is discarded unread and the connection stays up.
pub(crate) const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
/// Mouse-move commands allowed to wait in the writer queue; more are dropped (Chrome stalled).
const MAX_QUEUED_MOVES: usize = 64;

/// One queued write.
struct Outgoing {
    bytes: Vec<u8>,
    is_move: bool,
}

struct Pending {
    method: &'static str,
    cb: Option<Callback>,
}

struct Inner {
    /// `None` once `close` dropped it; the writer thread then ends and Chrome sees EOF.
    tx: Mutex<Option<flume::Sender<Outgoing>>>,
    queued_moves: Arc<AtomicUsize>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    sinks: Mutex<HashMap<Option<String>, Sink>>,
    closed: AtomicBool,
    close_hooks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

/// A handle to one CDP pipe. Cheap to clone; the transport ends when every handle is dropped
/// (the writer closes, Chrome exits, the reader sees EOF) or when `close` is called.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    pub fn new<R, W>(reader: R, writer: W) -> Connection
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (tx, rx) = flume::unbounded::<Outgoing>();
        let inner = Arc::new(Inner {
            tx: Mutex::new(Some(tx)),
            queued_moves: Arc::new(AtomicUsize::new(0)),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            sinks: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            close_hooks: Mutex::new(Vec::new()),
        });

        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("hgweb-cdp-reader".into())
            .spawn(move || reader_loop(reader, weak))
            .expect("spawn cdp reader");

        let weak = Arc::downgrade(&inner);
        let moves = inner.queued_moves.clone();
        std::thread::Builder::new()
            .name("hgweb-cdp-writer".into())
            .spawn(move || writer_loop(writer, rx, weak, moves))
            .expect("spawn cdp writer");

        Connection { inner }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Run `f` once when the pipe closes (immediately if it already has).
    pub fn on_close(&self, f: impl FnOnce() + Send + 'static) {
        let mut hooks = self.inner.close_hooks.lock();
        if self.inner.closed.load(Ordering::Acquire) {
            drop(hooks);
            f();
        } else {
            hooks.push(Box::new(f));
        }
    }

    /// Stop writing. Chrome sees EOF on its command pipe and exits; the reader then ends.
    pub fn close(&self) {
        self.inner.tx.lock().take();
    }

    /// Route the events of one flattened session (`None` = browser level) to `sink`.
    pub fn set_sink(&self, session: Option<String>, sink: Sink) {
        self.inner.sinks.lock().insert(session, sink);
    }

    pub fn remove_sink(&self, session: &Option<String>) {
        self.inner.sinks.lock().remove(session);
    }

    /// Send a command. `cb` runs on the reader thread with the result; without one the answer
    /// is only logged when it is an error. Returns the request id.
    pub fn send(
        &self,
        method: &'static str,
        params: Value,
        session: Option<&str>,
        cb: Option<Callback>,
    ) -> u64 {
        let inner = &self.inner;
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        if inner.closed.load(Ordering::Acquire) {
            if let Some(cb) = cb {
                cb(Err(CdpError::Closed));
            }
            return id;
        }
        let is_move = method == "Input.dispatchMouseEvent" && params["type"] == "mouseMoved";
        if is_move && inner.queued_moves.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED_MOVES {
            inner.queued_moves.fetch_sub(1, Ordering::AcqRel);
            if let Some(cb) = cb {
                cb(Err(CdpError::Timeout));
            }
            return id;
        }
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let mut bytes = serde_json::to_vec(&msg).expect("serialise CDP command");
        bytes.push(0);

        // Registered before the write: the answer can arrive before `send` returns.
        inner.pending.lock().insert(id, Pending { method, cb });
        let sent = match &*inner.tx.lock() {
            Some(tx) => tx.send(Outgoing { bytes, is_move }).is_ok(),
            None => false,
        };
        if !sent {
            if is_move {
                inner.queued_moves.fetch_sub(1, Ordering::AcqRel);
            }
            if let Some(p) = inner.pending.lock().remove(&id) {
                if let Some(cb) = p.cb {
                    cb(Err(CdpError::Closed));
                }
            }
        }
        id
    }

    /// Fire and forget; a failure is logged by the reader.
    pub fn notify(&self, method: &'static str, params: Value, session: Option<&str>) {
        self.send(method, params, session, None);
    }

    /// Send and wait for the answer.
    pub fn call(
        &self,
        method: &'static str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        let (tx, rx) = flume::bounded(1);
        let id = self.send(
            method,
            params,
            session,
            Some(Box::new(move |r| {
                let _ = tx.send(r);
            })),
        );
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                if self.is_closed() {
                    Err(CdpError::Closed)
                } else {
                    Err(CdpError::Timeout)
                }
            }
        }
    }

    /// `Target.attachToTarget` with a flattened session, registering `sink` before the first
    /// event of the new session can be read (the registration happens on the reader thread,
    /// right where the answer is dispatched).
    pub fn attach(&self, target_id: &str, sink: Sink, timeout: Duration) -> Result<String, CdpError> {
        let (tx, rx) = flume::bounded(1);
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let id = self.send(
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            None,
            Some(Box::new(move |r| {
                let r = r.and_then(|v| {
                    v.get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or(CdpError::Protocol {
                            code: -1,
                            message: "attachToTarget returned no sessionId".into(),
                        })
                });
                if let (Ok(session), Some(inner)) = (&r, weak.upgrade()) {
                    inner.sinks.lock().insert(Some(session.clone()), sink);
                }
                let _ = tx.send(r);
            })),
        );
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                if self.is_closed() {
                    Err(CdpError::Closed)
                } else {
                    Err(CdpError::Timeout)
                }
            }
        }
    }
}

impl Inner {
    /// The pipe is gone: fail everything outstanding and run the close hooks once.
    fn shut(&self) {
        let hooks = {
            let mut hooks = self.close_hooks.lock();
            if self.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            std::mem::take(&mut *hooks)
        };
        self.tx.lock().take();
        let pending: Vec<Pending> = self.pending.lock().drain().map(|(_, p)| p).collect();
        for p in pending {
            if let Some(cb) = p.cb {
                guarded("response callback", || cb(Err(CdpError::Closed)));
            }
        }
        self.sinks.lock().clear();
        for h in hooks {
            guarded("close hook", h);
        }
    }

    fn dispatch(&self, mut msg: Value) {
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let Some(p) = self.pending.lock().remove(&id) else {
                return;
            };
            let result = match msg.get_mut("error") {
                Some(err) => Err(CdpError::Protocol {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                }),
                None => Ok(msg.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            match p.cb {
                Some(cb) => guarded("response callback", || cb(result)),
                None => {
                    if let Err(e) = result {
                        log::debug!("{} failed: {e}", p.method);
                    }
                }
            }
        } else if let Some(method) = msg.get("method").and_then(Value::as_str) {
            let method = method.to_string();
            let session_id = msg.get("sessionId").and_then(Value::as_str).map(str::to_string);
            let params = msg.get_mut("params").map(Value::take).unwrap_or(Value::Null);
            let sink = self.sinks.lock().get(&session_id).cloned();
            match sink {
                Some(sink) => guarded("event sink", || {
                    sink(Event {
                        method,
                        params,
                        session_id,
                    })
                }),
                None => log::trace!("unrouted CDP event {method} (session {session_id:?})"),
            }
        }
    }
}

fn guarded(what: &str, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        log::error!("{what} panicked; the CDP reader keeps running");
    }
}

/// What `read_message` found.
#[derive(Debug, PartialEq, Eq)]
enum Read1 {
    /// Clean EOF between messages.
    Eof,
    /// A complete message (terminator stripped) is in the buffer.
    Message,
    /// The message exceeded the cap: skipped up to its NUL, nothing stored.
    Oversized(usize),
    /// EOF inside a message.
    Truncated,
}

/// Read one NUL-terminated message into `buf` (cleared first), keeping at most `max` bytes. An
/// oversized message is consumed up to its terminator without being stored.
fn read_message<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>, max: usize) -> std::io::Result<Read1> {
    buf.clear();
    let mut seen = 0usize;
    let mut overflow = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            buf.clear();
            return Ok(if seen == 0 && !overflow {
                Read1::Eof
            } else {
                Read1::Truncated
            });
        }
        let (take, done) = match chunk.iter().position(|b| *b == 0) {
            Some(p) => (p, true),
            None => (chunk.len(), false),
        };
        seen += take;
        if !overflow {
            if seen > max {
                overflow = true;
                buf.clear();
                buf.shrink_to(64 * 1024);
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        reader.consume(take + usize::from(done));
        if done {
            return Ok(if overflow { Read1::Oversized(seen) } else { Read1::Message });
        }
    }
}

fn reader_loop<R: Read>(reader: R, weak: Weak<Inner>) {
    let mut reader = BufReader::with_capacity(256 * 1024, reader);
    let mut buf = Vec::with_capacity(64 * 1024);
    loop {
        match read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES) {
            Ok(Read1::Eof | Read1::Truncated) => break,
            Ok(Read1::Oversized(n)) => {
                log::warn!("CDP message of {n} bytes dropped (limit {MAX_MESSAGE_BYTES})");
            }
            Ok(Read1::Message) => {
                let Some(inner) = weak.upgrade() else { return };
                match serde_json::from_slice::<Value>(&buf) {
                    Ok(v) => inner.dispatch(v),
                    Err(e) => log::warn!("undecodable CDP message ({} bytes): {e}", buf.len()),
                }
            }
            Err(e) => {
                log::debug!("CDP read failed: {e}");
                break;
            }
        }
        if buf.capacity() > 8 * 1024 * 1024 {
            buf = Vec::with_capacity(64 * 1024);
        }
    }
    if let Some(inner) = weak.upgrade() {
        inner.shut();
    }
}

fn writer_loop<W: Write>(
    mut writer: W,
    rx: flume::Receiver<Outgoing>,
    weak: Weak<Inner>,
    moves: Arc<AtomicUsize>,
) {
    for msg in rx.iter() {
        if msg.is_move {
            moves.fetch_sub(1, Ordering::AcqRel);
        }
        if let Err(e) = writer.write_all(&msg.bytes).and_then(|_| writer.flush()) {
            log::debug!("CDP write failed: {e}");
            if let Some(inner) = weak.upgrade() {
                inner.shut();
            }
            return;
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A scripted CDP peer on one end of a socketpair.
    use super::*;
    use std::os::unix::net::UnixStream;

    /// The peer side: reads NUL-framed commands and lets a handler answer.
    pub struct Peer {
        pub out: Arc<Mutex<UnixStream>>,
        stream: UnixStream,
    }

    impl Peer {
        pub fn send(&self, v: &Value) {
            let mut bytes = serde_json::to_vec(v).unwrap();
            bytes.push(0);
            let _ = self.out.lock().write_all(&bytes);
        }

        /// Close the peer's end: the connection under test sees EOF.
        pub fn hang_up(&self) {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
    }

    /// A connected (Connection, peer stream) pair; the caller drives the peer.
    pub fn pair() -> (Connection, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let conn = Connection::new(a.try_clone().unwrap(), a);
        (conn, b)
    }

    /// Spawn a scripted peer. `handler` gets each decoded command and returns the messages to
    /// write back (responses and events), in order.
    pub fn spawn_peer<F>(stream: UnixStream, mut handler: F) -> Arc<Peer>
    where
        F: FnMut(&Value) -> Vec<Value> + Send + 'static,
    {
        let peer = Arc::new(Peer {
            out: Arc::new(Mutex::new(stream.try_clone().unwrap())),
            stream: stream.try_clone().unwrap(),
        });
        let p = peer.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(0, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        let cmd: Value = serde_json::from_slice(&buf).unwrap();
                        for m in handler(&cmd) {
                            p.send(&m);
                        }
                    }
                }
            }
        });
        peer
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Hands out one byte per `read`, to exercise frames split across reads.
    struct Dribble(std::io::Cursor<Vec<u8>>);
    impl Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(1);
            self.0.read(&mut buf[..n])
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn commands_are_nul_terminated_json_with_increasing_ids_and_session() {
        let cap = Capture::default();
        // A reader that never yields keeps the connection open.
        let (_keep, r) = std::os::unix::net::UnixStream::pair().unwrap();
        let c = Connection::new(r, cap.clone());
        c.notify("Page.enable", json!({}), None);
        c.notify("Page.navigate", json!({"url": "about:blank"}), Some("S1"));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let bytes = cap.0.lock().clone();
            let frames: Vec<&[u8]> = bytes.split(|b| *b == 0).filter(|f| !f.is_empty()).collect();
            if frames.len() == 2 && bytes.last() == Some(&0) {
                let a: Value = serde_json::from_slice(frames[0]).unwrap();
                let b: Value = serde_json::from_slice(frames[1]).unwrap();
                assert_eq!(a["id"], 1);
                assert_eq!(a["method"], "Page.enable");
                assert!(a.get("sessionId").is_none());
                assert_eq!(b["id"], 2);
                assert_eq!(b["sessionId"], "S1");
                assert_eq!(b["params"]["url"], "about:blank");
                return;
            }
            assert!(std::time::Instant::now() < deadline, "writer produced {bytes:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn byte_at_a_time_reads_still_yield_whole_messages() {
        // The reader does not start until the sink is registered, so no event is unrouted.
        let (go_tx, go_rx) = flume::bounded::<()>(1);
        struct Gated(flume::Receiver<()>, bool, Dribble);
        impl Read for Gated {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.1 {
                    if self.0.recv().is_err() {
                        return Ok(0);
                    }
                    self.1 = true;
                }
                self.2.read(buf)
            }
        }
        let mut wire = Vec::new();
        for i in 0..3 {
            wire.extend_from_slice(
                serde_json::to_string(&json!({"method":"Test.ev","params":{"i":i},"sessionId":"S"}))
                    .unwrap()
                    .as_bytes(),
            );
            wire.push(0);
        }
        let seen = Arc::new(Mutex::new(Vec::<Event>::new()));
        let c = Connection::new(
            Gated(go_rx, false, Dribble(std::io::Cursor::new(wire))),
            Capture::default(),
        );
        let s = seen.clone();
        c.set_sink(Some("S".into()), Arc::new(move |e| s.lock().push(e)));
        go_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while seen.lock().len() < 3 {
            assert!(std::time::Instant::now() < deadline, "got {:?}", seen.lock());
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = seen.lock();
        assert_eq!(seen[0].params["i"], 0);
        assert_eq!(seen[2].params["i"], 2);
        assert_eq!(seen[1].session_id.as_deref(), Some("S"));
    }

    #[test]
    fn messages_split_across_writes_and_back_to_back_are_decoded() {
        let (conn, peer) = pair();
        let got = Arc::new(Mutex::new(Vec::<Event>::new()));
        let g = got.clone();
        conn.set_sink(Some("S".into()), Arc::new(move |e| g.lock().push(e)));
        let p = spawn_peer(peer, |_| vec![]);
        // One message and the head of the next in a single write; the tail follows later.
        let m1 = serde_json::to_vec(&json!({"method":"A","params":{},"sessionId":"S"})).unwrap();
        let m2 = serde_json::to_vec(&json!({"method":"B","params":{},"sessionId":"S"})).unwrap();
        let mut raw = Vec::new();
        raw.extend_from_slice(&m1);
        raw.push(0);
        raw.extend_from_slice(&m2[..5]);
        p.out.lock().write_all(&raw).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(got.lock().len(), 1, "a partial message must not be dispatched");
        let mut rest = m2[5..].to_vec();
        rest.push(0);
        p.out.lock().write_all(&rest).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while got.lock().len() < 2 {
            assert!(std::time::Instant::now() < deadline, "got {:?}", got.lock());
            std::thread::sleep(Duration::from_millis(5));
        }
        let names: Vec<String> = got.lock().iter().map(|e| e.method.clone()).collect();
        assert_eq!(names, ["A", "B"]);
    }

    #[test]
    fn out_of_order_responses_reach_their_own_callers() {
        let (conn, peer) = pair();
        let held = Arc::new(Mutex::new(Vec::<u64>::new()));
        let h = held.clone();
        let _p = spawn_peer(peer, move |cmd| {
            let id = cmd["id"].as_u64().unwrap();
            let mut h = h.lock();
            h.push(id);
            if h.len() < 3 {
                return vec![]; // hold the first two
            }
            // Answer newest first.
            let mut out = Vec::new();
            for id in h.drain(..).rev() {
                out.push(json!({"id": id, "result": {"echo": id}}));
            }
            out
        });
        let results = Arc::new(Mutex::new(HashMap::<u64, u64>::new()));
        let mut ids = Vec::new();
        for _ in 0..3 {
            let r = results.clone();
            let id = conn.send(
                "Test.cmd",
                json!({}),
                None,
                Some(Box::new(move |res| {
                    let v = res.unwrap();
                    r.lock().insert(v["echo"].as_u64().unwrap(), v["echo"].as_u64().unwrap());
                })),
            );
            ids.push(id);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while results.lock().len() < 3 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        for id in ids {
            assert_eq!(results.lock().get(&id), Some(&id), "response for {id} went astray");
        }
    }

    #[test]
    fn protocol_errors_and_sync_calls() {
        let (conn, peer) = pair();
        let _p = spawn_peer(peer, |cmd| {
            let id = cmd["id"].clone();
            match cmd["method"].as_str().unwrap() {
                "Good.call" => vec![json!({"id": id, "result": {"ok": true}})],
                "Bad.call" => vec![json!({"id": id, "error": {"code": -32601, "message": "nope"}})],
                _ => vec![],
            }
        });
        let ok = conn.call("Good.call", json!({}), None, Duration::from_secs(2)).unwrap();
        assert_eq!(ok["ok"], true);
        let err = conn.call("Bad.call", json!({}), None, Duration::from_secs(2)).unwrap_err();
        assert_eq!(
            err,
            CdpError::Protocol {
                code: -32601,
                message: "nope".into()
            }
        );
        let err = conn
            .call("Silent.call", json!({}), None, Duration::from_millis(50))
            .unwrap_err();
        assert_eq!(err, CdpError::Timeout);
        assert!(conn.inner.pending.lock().is_empty(), "timed-out id must not leak");
    }

    #[test]
    fn events_are_routed_by_session_and_browser_level_goes_to_none() {
        let (conn, peer) = pair();
        let a = Arc::new(Mutex::new(Vec::<String>::new()));
        let b = Arc::new(Mutex::new(Vec::<String>::new()));
        let browser = Arc::new(Mutex::new(Vec::<String>::new()));
        for (key, store) in [(Some("A".to_string()), a.clone()), (Some("B".to_string()), b.clone()), (None, browser.clone())] {
            conn.set_sink(key, Arc::new(move |e| store.lock().push(e.method)));
        }
        let p = spawn_peer(peer, |_| vec![]);
        p.send(&json!({"method":"X.a","params":{},"sessionId":"A"}));
        p.send(&json!({"method":"X.b","params":{},"sessionId":"B"}));
        p.send(&json!({"method":"X.browser","params":{}}));
        p.send(&json!({"method":"X.nobody","params":{},"sessionId":"C"}));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while a.lock().is_empty() || b.lock().is_empty() || browser.lock().is_empty() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*a.lock(), ["X.a"]);
        assert_eq!(*b.lock(), ["X.b"]);
        assert_eq!(*browser.lock(), ["X.browser"]);
    }

    #[test]
    fn attach_registers_the_sink_before_the_first_event() {
        let (conn, peer) = pair();
        let _p = spawn_peer(peer, |cmd| {
            if cmd["method"] == "Target.attachToTarget" {
                // The response and an event of the new session arrive back to back.
                vec![
                    json!({"id": cmd["id"], "result": {"sessionId": "NEW"}}),
                    json!({"method": "Page.first", "params": {}, "sessionId": "NEW"}),
                ]
            } else {
                vec![]
            }
        });
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s = seen.clone();
        let sid = conn
            .attach("T1", Arc::new(move |e| s.lock().push(e.method)), Duration::from_secs(2))
            .unwrap();
        assert_eq!(sid, "NEW");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while seen.lock().is_empty() {
            assert!(std::time::Instant::now() < deadline, "first event was dropped");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*seen.lock(), ["Page.first"]);
    }

    #[test]
    fn hang_up_fails_pending_and_runs_close_hooks_once() {
        let (conn, peer) = pair();
        let p = spawn_peer(peer, |_| vec![]);
        let hooks = Arc::new(AtomicUsize::new(0));
        let h = hooks.clone();
        conn.on_close(move || {
            h.fetch_add(1, Ordering::SeqCst);
        });
        let (tx, rx) = flume::bounded(1);
        conn.send(
            "Never.answered",
            json!({}),
            None,
            Some(Box::new(move |r| {
                let _ = tx.send(r);
            })),
        );
        p.hang_up();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Err(CdpError::Closed));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while hooks.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(conn.is_closed());
        // After the pipe is gone, new commands fail at once and late hooks run immediately.
        let err = conn.call("Late.call", json!({}), None, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err, CdpError::Closed);
        let late = Arc::new(AtomicBool::new(false));
        let l = late.clone();
        conn.on_close(move || l.store(true, Ordering::SeqCst));
        assert!(late.load(Ordering::SeqCst));
        assert_eq!(hooks.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_panicking_sink_does_not_kill_the_reader() {
        let (conn, peer) = pair();
        let p = spawn_peer(peer, |_| vec![]);
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        conn.set_sink(
            None,
            Arc::new(move |e| {
                if e.method == "Boom" {
                    panic!("sink bug");
                }
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );
        p.send(&json!({"method":"Boom","params":{}}));
        p.send(&json!({"method":"Fine","params":{}}));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while count.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn oversized_message_is_skipped_and_the_next_one_read() {
        let mut data = vec![b'x'; 100];
        data.push(0);
        data.extend_from_slice(b"{\"a\":1}\0");
        data.extend_from_slice(b"tail");
        let mut r = BufReader::with_capacity(7, Dribble(std::io::Cursor::new(data)));
        let mut buf = Vec::new();
        assert_eq!(read_message(&mut r, &mut buf, 10).unwrap(), Read1::Oversized(100));
        assert!(buf.is_empty(), "an oversized message is never stored");
        assert_eq!(read_message(&mut r, &mut buf, 10).unwrap(), Read1::Message);
        assert_eq!(buf, b"{\"a\":1}");
        assert_eq!(read_message(&mut r, &mut buf, 10).unwrap(), Read1::Truncated);
        assert_eq!(read_message(&mut r, &mut buf, 10).unwrap(), Read1::Eof);
    }

    #[test]
    fn message_of_exactly_the_limit_is_kept() {
        let mut data = vec![b'y'; 10];
        data.push(0);
        let mut r = BufReader::new(std::io::Cursor::new(data));
        let mut buf = Vec::new();
        assert_eq!(read_message(&mut r, &mut buf, 10).unwrap(), Read1::Message);
        assert_eq!(buf.len(), 10);
    }

    #[test]
    fn connection_survives_an_oversized_message() {
        let (conn, stream) = pair();
        let peer = spawn_peer(stream, |_| Vec::new());
        let got = Arc::new(AtomicUsize::new(0));
        let g = got.clone();
        conn.set_sink(None, Arc::new(move |_| {
            g.fetch_add(1, Ordering::SeqCst);
        }));
        let mut big = vec![b'z'; MAX_MESSAGE_BYTES + 1000];
        big.push(0);
        let _ = peer.out.lock().write_all(&big);
        peer.send(&json!({"method": "Test.after", "params": {}}));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.load(Ordering::SeqCst), 1);
        assert!(!conn.is_closed());
    }

    #[test]
    fn queued_mouse_moves_are_bounded() {
        struct Blocked(flume::Receiver<()>);
        impl Write for Blocked {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                let _ = self.0.recv();
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (go_tx, go_rx) = flume::bounded::<()>(1);
        let (_keep, r) = std::os::unix::net::UnixStream::pair().unwrap();
        let c = Connection::new(r, Blocked(go_rx));
        for _ in 0..(MAX_QUEUED_MOVES * 4) {
            c.notify("Input.dispatchMouseEvent", json!({"type": "mouseMoved"}), None);
        }
        assert!(c.inner.queued_moves.load(Ordering::SeqCst) <= MAX_QUEUED_MOVES);
        assert!(c.inner.pending.lock().len() <= MAX_QUEUED_MOVES + 1);
        drop(go_tx);
    }
}
