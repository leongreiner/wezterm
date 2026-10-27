//! A scripted stand-in for Chrome's CDP pipe, for the view lifecycle tests: it answers the
//! commands the engine sends, emits the events Chrome would (with real JPEG frames) and can be
//! made to crash. It also records every command so tests can assert on the wire traffic.

use crate::browser::{ExitInfo, Launched, Launcher};
use crate::cdp::testutil::{spawn_peer, Peer};
use crate::frame::testutil::solid_jpeg;
use crate::WebConfig;
use anyhow::{bail, Result};
use base64::Engine as _;
use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Cmd {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

#[derive(Default)]
struct Target {
    id: String,
    session: Option<String>,
    history: Vec<String>,
    index: usize,
    screencast: bool,
    /// css width, css height, device scale factor
    metrics: (f64, f64, f64),
    max: (u32, u32),
}

#[derive(Default)]
struct State {
    log: Vec<Cmd>,
    targets: Vec<Target>,
    next: u32,
    contexts: u32,
}

#[derive(Default)]
struct Exit {
    info: Mutex<Option<ExitInfo>>,
    cv: Condvar,
}

/// One fake browser process.
pub struct FakeChrome {
    state: Arc<Mutex<State>>,
    peer: Mutex<Option<Arc<Peer>>>,
    exit: Arc<Exit>,
    /// Manual mode: no automatic navigation events or frames; the test drives them.
    manual: bool,
}

impl FakeChrome {
    pub fn commands(&self) -> Vec<Cmd> {
        self.state.lock().log.clone()
    }

    pub fn count(&self, method: &str) -> usize {
        self.state.lock().log.iter().filter(|c| c.method == method).count()
    }

    pub fn last(&self, method: &str) -> Option<Cmd> {
        self.state.lock().log.iter().rev().find(|c| c.method == method).cloned()
    }

    pub fn all(&self, method: &str) -> Vec<Cmd> {
        self.state.lock().log.iter().filter(|c| c.method == method).cloned().collect()
    }

    /// The process dies: the pipe closes, `wait` returns.
    pub fn crash(&self, description: &str) {
        self.exit_with(description);
    }

    fn exit_with(&self, description: &str) {
        {
            let mut i = self.exit.info.lock();
            if i.is_none() {
                *i = Some(ExitInfo {
                    description: description.to_string(),
                    log_tail: None,
                });
            }
        }
        self.exit.cv.notify_all();
        if let Some(p) = self.peer.lock().as_ref() {
            p.hang_up();
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exit.info.lock().is_some()
    }

    fn peer(&self) -> Arc<Peer> {
        self.peer.lock().clone().expect("peer running")
    }

    fn session_of(&self, target_index: usize) -> (String, String) {
        let st = self.state.lock();
        let t = &st.targets[target_index];
        (t.id.clone(), t.session.clone().expect("target attached"))
    }

    /// Emit a session-scoped event for the `i`-th target.
    pub fn session_event(&self, i: usize, method: &str, params: Value) {
        let (_, s) = self.session_of(i);
        self.peer().send(&json!({"method": method, "params": params, "sessionId": s}));
    }

    /// Emit a browser-level event.
    pub fn browser_event(&self, method: &str, params: Value) {
        self.peer().send(&json!({"method": method, "params": params}));
    }

    /// Emit a screencast frame of `w`x`h` for the `i`-th target.
    pub fn frame(&self, i: usize, w: u32, h: u32, rgb: [u8; 3]) {
        let jpeg = solid_jpeg(w, h, rgb);
        let data = base64::engine::general_purpose::STANDARD.encode(jpeg);
        self.session_event(
            i,
            "Page.screencastFrame",
            json!({"data": data, "metadata": {}, "sessionId": 1}),
        );
    }

    pub fn target_id(&self, i: usize) -> String {
        self.session_of(i).0
    }
}

fn frame_event(session: &str, t: &Target) -> Option<Value> {
    if !t.screencast {
        return None;
    }
    let (cw, ch, dsf) = t.metrics;
    let (mut w, mut h) = ((cw * dsf).round() as u32, (ch * dsf).round() as u32);
    if t.max.0 > 0 {
        w = w.min(t.max.0);
        h = h.min(t.max.1);
    }
    if w == 0 || h == 0 {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD.encode(solid_jpeg(w, h, [255, 0, 255]));
    Some(json!({
        "method": "Page.screencastFrame",
        "params": {"data": data, "metadata": {}, "sessionId": 1},
        "sessionId": session,
    }))
}

fn nav_events(session: &str, t: &Target) -> Vec<Value> {
    let url = &t.history[t.index];
    vec![
        json!({"method": "Page.frameStartedLoading", "params": {"frameId": "F1"}, "sessionId": session}),
        json!({"method": "Page.frameNavigated", "params": {"frame": {"id": "F1", "url": url}}, "sessionId": session}),
        json!({"method": "Target.targetInfoChanged", "params": {"targetInfo": {
            "targetId": t.id, "type": "page", "title": url, "url": url}}}),
        json!({"method": "Page.domContentEventFired", "params": {"timestamp": 1.0}, "sessionId": session}),
        json!({"method": "Page.frameStoppedLoading", "params": {"frameId": "F1"}, "sessionId": session}),
    ]
}

fn handle(chrome: &FakeChrome, cmd: &Value) -> Vec<Value> {
    let id = cmd["id"].clone();
    let method = cmd["method"].as_str().unwrap_or("").to_string();
    let params = cmd["params"].clone();
    let session = cmd["sessionId"].as_str().map(str::to_string);
    let mut st = chrome.state.lock();
    st.log.push(Cmd {
        method: method.clone(),
        params: params.clone(),
        session: session.clone(),
    });
    let mut out: Vec<Value> = Vec::new();
    let mut result = json!({});
    let ti = session
        .as_ref()
        .and_then(|s| st.targets.iter().position(|t| t.session.as_deref() == Some(s)));
    match method.as_str() {
        "Target.createBrowserContext" => {
            st.contexts += 1;
            result = json!({"browserContextId": format!("CTX{}", st.contexts)});
        }
        "Target.createTarget" => {
            st.next += 1;
            let id = format!("T{}", st.next);
            let (w, h) = (params["width"].as_f64().unwrap_or(800.0), params["height"].as_f64().unwrap_or(600.0));
            st.targets.push(Target {
                id: id.clone(),
                history: vec!["about:blank".into()],
                metrics: (w, h, 1.0),
                ..Default::default()
            });
            out.push(json!({"method": "Target.targetCreated", "params": {"targetInfo": {
                "targetId": id, "type": "page", "url": "about:blank"}}}));
            result = json!({"targetId": id});
        }
        "Target.attachToTarget" => {
            st.next += 1;
            let sid = format!("S{}", st.next);
            if let Some(t) = st.targets.iter_mut().find(|t| Some(t.id.as_str()) == params["targetId"].as_str()) {
                t.session = Some(sid.clone());
            }
            result = json!({"sessionId": sid});
        }
        "Target.closeTarget" => {
            let tid = params["targetId"].as_str().unwrap_or("").to_string();
            st.targets.retain(|t| t.id != tid);
            out.push(json!({"method": "Target.targetDestroyed", "params": {"targetId": tid}}));
            result = json!({"success": true});
        }
        "Browser.getWindowForTarget" => {
            let n = st.targets.iter().position(|t| Some(t.id.as_str()) == params["targetId"].as_str());
            result = json!({"windowId": 7000 + n.unwrap_or(0), "bounds": {"windowState": "normal"}});
        }
        "Page.getFrameTree" => result = json!({"frameTree": {"frame": {"id": "F1"}}}),
        "Page.startScreencast" => {
            if let Some(i) = ti {
                let t = &mut st.targets[i];
                t.screencast = true;
                t.max = (
                    params["maxWidth"].as_u64().unwrap_or(0) as u32,
                    params["maxHeight"].as_u64().unwrap_or(0) as u32,
                );
                if let Some(f) = frame_event(session.as_deref().unwrap(), t).filter(|_| !chrome.manual) {
                    out.push(f);
                }
            }
        }
        "Page.stopScreencast" => {
            if let Some(i) = ti {
                st.targets[i].screencast = false;
            }
        }
        "Emulation.setDeviceMetricsOverride" => {
            if let Some(i) = ti {
                let t = &mut st.targets[i];
                t.metrics = (
                    params["width"].as_f64().unwrap_or(0.0),
                    params["height"].as_f64().unwrap_or(0.0),
                    params["deviceScaleFactor"].as_f64().unwrap_or(1.0),
                );
                if let Some(f) = frame_event(session.as_deref().unwrap(), t).filter(|_| !chrome.manual) {
                    out.push(f);
                }
            }
        }
        "Page.navigate" => {
            if let Some(i) = ti {
                let url = params["url"].as_str().unwrap_or("").to_string();
                let t = &mut st.targets[i];
                t.history.truncate(t.index + 1);
                t.history.push(url);
                t.index = t.history.len() - 1;
                if !chrome.manual {
                    out.extend(nav_events(session.as_deref().unwrap(), t));
                    if let Some(f) = frame_event(session.as_deref().unwrap(), t) {
                        out.push(f);
                    }
                }
            }
            result = json!({"frameId": "F1"});
        }
        "Page.reload" => {
            if let Some(i) = ti {
                let t = &st.targets[i];
                out.extend(nav_events(session.as_deref().unwrap(), t));
            }
        }
        "Page.getNavigationHistory" => {
            if let Some(i) = ti {
                let t = &st.targets[i];
                let entries: Vec<Value> = t
                    .history
                    .iter()
                    .enumerate()
                    .map(|(n, u)| json!({"id": 100 + n, "url": u, "title": ""}))
                    .collect();
                result = json!({"currentIndex": t.index, "entries": entries});
            }
        }
        "Page.navigateToHistoryEntry" => {
            if let Some(i) = ti {
                let want = params["entryId"].as_i64().unwrap_or(0) - 100;
                let t = &mut st.targets[i];
                t.index = (want.max(0) as usize).min(t.history.len() - 1);
                out.extend(nav_events(session.as_deref().unwrap(), t));
            }
        }
        "Runtime.evaluate" => {
            let value = if params["expression"] == "document.title" {
                ti.map_or(String::new(), |i| {
                    let t = &st.targets[i];
                    format!("Title of {}", t.history[t.index])
                })
            } else {
                "selected text".to_string()
            };
            result = json!({"result": {"type": "string", "value": value}});
        }
        "Browser.close" => {
            drop(st);
            // Answer, then go away like Chrome does.
            let peer = chrome.peer();
            peer.send(&json!({"id": id, "result": {}}));
            chrome.exit_with("exited with code 0");
            return vec![];
        }
        _ => {}
    }
    let mut msgs = vec![json!({"id": id, "result": result})];
    if let Some(s) = &session {
        msgs[0]["sessionId"] = json!(s);
    }
    msgs.extend(out);
    msgs
}

/// A launcher that starts fake browsers.
#[derive(Default)]
pub struct FakeLauncher {
    pub launches: AtomicUsize,
    pub chromes: Mutex<Vec<Arc<FakeChrome>>>,
    /// While set, launching fails with this message.
    pub fail: Mutex<Option<String>>,
    next_pid: AtomicU32,
    /// New browsers start in manual mode (see `FakeChrome::manual`).
    pub manual: std::sync::atomic::AtomicBool,
    /// The configuration of every launch (its extra flags carry the device scale factor).
    pub cfgs: Mutex<Vec<WebConfig>>,
}

impl FakeLauncher {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeLauncher {
            next_pid: AtomicU32::new(40000),
            ..Default::default()
        })
    }

    pub fn chrome(&self, i: usize) -> Arc<FakeChrome> {
        self.chromes.lock()[i].clone()
    }

    pub fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
}

impl Launcher for FakeLauncher {
    fn launch(&self, cfg: &WebConfig, _devtools: bool) -> Result<Launched> {
        self.cfgs.lock().push(cfg.clone());
        if let Some(msg) = self.fail.lock().clone() {
            bail!("{msg}");
        }
        self.launches.fetch_add(1, Ordering::SeqCst);
        let (ours, theirs): (UnixStream, UnixStream) = UnixStream::pair()?;
        let chrome = Arc::new(FakeChrome {
            state: Arc::new(Mutex::new(State::default())),
            peer: Mutex::new(None),
            exit: Arc::new(Exit::default()),
            manual: self.manual.load(Ordering::SeqCst),
        });
        let c = chrome.clone();
        let peer = spawn_peer(theirs, move |cmd| handle(&c, cmd));
        *chrome.peer.lock() = Some(peer);
        self.chromes.lock().push(chrome.clone());

        let exit = chrome.exit.clone();
        let wait: Box<dyn FnOnce() -> ExitInfo + Send> = Box::new(move || {
            let mut g = exit.info.lock();
            while g.is_none() {
                exit.cv.wait_for(&mut g, Duration::from_millis(50));
            }
            g.clone().unwrap()
        });
        let killer = chrome.clone();
        Ok(Launched {
            reader: Box::new(ours.try_clone()?),
            writer: Box::new(ours),
            pid: self.next_pid.fetch_add(1, Ordering::SeqCst),
            wait,
            kill: Arc::new(move || killer.exit_with("killed by signal 9 (SIGKILL)")),
            profile_dir: None,
        })
    }
}

/// Poll until `f` holds (3 s default).
pub fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    eventually_for(Duration::from_secs(5), what, &mut f)
}

pub fn eventually_for(limit: Duration, what: &str, f: &mut dyn FnMut() -> bool) {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f(), "timed out waiting for: {what}");
}
