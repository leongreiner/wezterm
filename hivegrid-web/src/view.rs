//! One web page as the pane sees it: a CDP target of a shared `Browser`.
//!
//! Threads per view: a *worker* (navigation, lifecycle, CDP events other than frames, resize
//! debounce) and a *decoder* (base64 + JPEG decode of screencast frames, so a slow decode never
//! delays a navigation or an input acknowledgement). Input goes straight from the caller's
//! thread to the CDP writer queue and never waits for either.

use crate::browser::Browser;
use crate::cdp::{CdpError, Event};
use crate::engine::EngineShared;
use crate::frame::{decode_jpeg, record_decode, Decoded, FrameSlot};
use crate::keys;
use crate::urls::normalize_url;
use crate::{
    sanitize_display, Frame, PageInfo, ViewSize, WebConfig, WebEvent, WebKey, WebMouse, WebMouseKind, WebState,
    WebView, MAX_TITLE_CHARS, MAX_URL_CHARS,
};
use anyhow::{anyhow, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(40);
const RESIZE_MAX_DELAY: Duration = Duration::from_millis(150);
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// How long frames of an unexpected size are dropped after the size was configured.
const SIZE_SETTLE: Duration = Duration::from_millis(500);
/// Longest time frames are held back waiting for the first navigation to commit.
const HOLD_FRAMES: Duration = Duration::from_secs(5);
/// How often the page title is re-read while the view is visible.
const TITLE_POLL: Duration = Duration::from_secs(1);
const DEFAULT_QUALITY: u32 = 80;

/// Injected into every document: reports `cursor` changes under the pointer through the
/// `hgcursor` binding (`Runtime.bindingCalled`).
const CURSOR_JS: &str = r#"(() => {
  if (window.__hgcursor) return;
  window.__hgcursor = true;
  let last = '';
  const textUnder = (el, x, y) => {
    if (getComputedStyle(el).userSelect === 'none') return false;
    const r = document.caretRangeFromPoint && document.caretRangeFromPoint(x, y);
    const n = r && r.startContainer;
    if (!n || n.nodeType !== 3 || !(el === n.parentElement || el.contains(n))) return false;
    const g = document.createRange();
    g.selectNodeContents(n);
    for (const b of g.getClientRects())
      if (x >= b.left && x <= b.right && y >= b.top && y <= b.bottom) return true;
    return false;
  };
  addEventListener('mousemove', (e) => {
    let c;
    try {
      const el = e.target;
      c = getComputedStyle(el).cursor || 'auto';
      if (c.startsWith('url(')) c = c.replace(/^(url\([^)]*\)(\s+[\d.]+\s+[\d.]+)?\s*,\s*)+/, '') || 'auto';
      if (c === 'auto') {
        const t = el.tagName;
        const editable = el.isContentEditable || t === 'TEXTAREA' ||
          (t === 'INPUT' && /^(text|search|url|tel|email|password|number)?$/i.test(el.type));
        c = (editable || textUnder(el, e.clientX, e.clientY)) ? 'text' : 'default';
      }
    } catch (_) { c = 'default'; }
    if (c !== last) { last = c; try { window.hgcursor(c); } catch (_) {} }
  }, { capture: true, passive: true });
})();"#;

/// The CSS cursor keywords a pane passes on. The page reports its cursor through a binding any
/// script can call with any string, so only these exact values get through.
const CURSORS: &[&str] = &[
    "auto", "default", "none", "context-menu", "help", "pointer", "progress", "wait", "cell",
    "crosshair", "text", "vertical-text", "alias", "copy", "move", "no-drop", "not-allowed",
    "grab", "grabbing", "e-resize", "n-resize", "ne-resize", "nw-resize", "s-resize",
    "se-resize", "sw-resize", "w-resize", "ew-resize", "ns-resize", "nesw-resize",
    "nwse-resize", "col-resize", "row-resize", "all-scroll", "zoom-in", "zoom-out",
];

/// Whether a URL a page asked to open (`window.open`, `target=_blank`) may load in the pane.
fn popup_url_allowed(url: &str) -> bool {
    let u = url.trim_start().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://")
}

/// The allowlisted keyword for a `hgcursor` payload (case-insensitive, trimmed), if it is one.
fn allowed_cursor(payload: &str) -> Option<&'static str> {
    if payload.len() > 32 {
        return None;
    }
    let p = payload.trim();
    CURSORS.iter().copied().find(|c| c.eq_ignore_ascii_case(p))
}

/// Text of the current selection, including the selection inside a text field.
const COPY_JS: &str = r#"(() => {
  const a = document.activeElement;
  if (a && (a.tagName === 'TEXTAREA' || a.tagName === 'INPUT') &&
      typeof a.selectionStart === 'number' && a.selectionEnd > a.selectionStart) {
    try { return a.value.substring(a.selectionStart, a.selectionEnd); } catch (e) {}
  }
  return String(getSelection());
})()"#;

#[derive(Clone)]
struct Desired {
    /// Where the page should be (and, after a crash, what to reload).
    url: String,
    size: ViewSize,
    visible: bool,
    focused: bool,
    zoom: u32,
}

/// The live connection of a view to a page of a browser.
pub(crate) struct Attached {
    pub browser: Arc<Browser>,
    pub session: String,
    pub target_id: String,
    /// Private views live in their own browser context.
    pub context: Option<String>,
    /// Browser window of the page (`Browser.setWindowBounds`), when Chrome told us.
    pub window_id: Option<i64>,
    pub epoch: u64,
}

struct Shared {
    info: PageInfo,
    state: WebState,
    cursor: Option<String>,
    main_frame: Option<String>,
}

struct Expect {
    w: u32,
    h: u32,
    /// When the size was last (re)configured.
    since: Instant,
}

/// The newest frame refused for its size, shown anyway at `show_at` (see `decode_loop`).
struct Rejected {
    d: Decoded,
    epoch: u64,
    /// The `Expect::since` it was refused under.
    since: Instant,
    show_at: Instant,
}

enum FrameJob {
    Frame {
        attached: Arc<Attached>,
        screencast_session: i64,
        data: String,
        /// Arrived while the view waits for its first navigation to commit: not shown.
        held: bool,
    },
    Stop,
}

enum Msg {
    Cdp { epoch: u64, ev: Event },
    Target(Event),
    BrowserDied { browser_id: u64, reason: String },
    Navigate(String),
    Back,
    Forward,
    Reload(bool),
    Stop,
    /// `desired` changed (size, visibility, focus, zoom).
    Sync,
    Restart,
    Revive,
    Close,
}

pub(crate) struct ViewCore {
    pub id: u64,
    pub cfg: WebConfig,
    engine: Arc<EngineShared>,
    notify: Box<dyn Fn(WebEvent) + Send + Sync>,
    slot: FrameSlot,
    shared: Mutex<Shared>,
    desired: Mutex<Desired>,
    attached: Mutex<Option<Arc<Attached>>>,
    tx: flume::Sender<Msg>,
    frame_tx: flume::Sender<FrameJob>,
    closed: AtomicBool,
    /// Crashed because the browser process went away: a relaunch by any view revives it.
    revivable: AtomicBool,
    epoch: AtomicU64,
    tuning: Tuning,
    /// Serialises storing a decoded frame with tearing the page down (see `decode_loop`).
    frame_gate: Mutex<()>,
    /// While `Some(deadline)` and before it, frames are acknowledged but not shown.
    hold_frames: Mutex<Option<Instant>>,
    /// The frame size Chrome should be producing; see `check_size`.
    expect: Mutex<Expect>,
}

/// The `WebView` handed to the pane. Dropping it closes the view.
pub(crate) struct View {
    core: Arc<ViewCore>,
}

/// Screencast tuning, read once per view from the environment: `HIVEGRID_WEB_QUALITY` (JPEG
/// quality 1..=100, default 80) and `HIVEGRID_WEB_SCALE` (25..=100 percent of the view's pixel
/// size, default 100; Chrome scales the frame down before encoding it, and the renderer
/// stretches it back, so it trades sharpness for CPU).
///
/// There is deliberately no frame rate cap: `everyNthFrame` skips *frames*, not time, so it
/// drops damage frames (a blinking caret that changes every other frame would freeze, and a
/// static page could never produce its first frame).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tuning {
    quality: u32,
    scale_percent: u32,
}

impl Tuning {
    fn from_env() -> Self {
        Self::from_vars(
            std::env::var("HIVEGRID_WEB_QUALITY").ok().as_deref(),
            std::env::var("HIVEGRID_WEB_SCALE").ok().as_deref(),
        )
    }

    fn from_vars(quality: Option<&str>, scale: Option<&str>) -> Self {
        let quality = quality
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|q| (1..=100).contains(q))
            .unwrap_or(DEFAULT_QUALITY);
        let scale_percent = scale
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|s| (25..=100).contains(s))
            .unwrap_or(100);
        Tuning {
            quality,
            scale_percent,
        }
    }
}

fn window_id_of(browser: &Browser, target_id: &str) -> Option<i64> {
    browser
        .conn
        .call("Browser.getWindowForTarget", json!({"targetId": target_id}), None, CALL_TIMEOUT)
        .ok()
        .and_then(|v| v["windowId"].as_i64())
}

/// Device scale factor of a view size.
pub(crate) fn scale_of(size: ViewSize) -> f64 {
    size.dpi.max(1) as f64 / 96.0
}

fn css_size(size: ViewSize, zoom: u32) -> (f64, f64, f64) {
    let dsf = scale_of(size);
    let z = zoom.clamp(10, 500) as f64 / 100.0;
    let scale = dsf * z;
    (
        (size.pixel_width as f64 / scale).round().max(1.0),
        (size.pixel_height as f64 / scale).round().max(1.0),
        scale,
    )
}

/// The size of the screencast frames Chrome produces for a view: the emulated CSS viewport times
/// the device scale factor the *browser* runs at (`--force-device-scale-factor`; the emulated
/// scale of `setDeviceMetricsOverride` only reaches the page, not the frame), scaled down to fit
/// `maxWidth` x `maxHeight` (never up). A view whose page scale (dpi times zoom) is above the
/// browser's gets frames smaller than its pixels, which the renderer stretches.
fn expected_frame(size: ViewSize, zoom: u32, browser_scale: f64, t: Tuning) -> (u32, u32) {
    let (css_w, css_h, _) = css_size(size, zoom);
    let (frame_w, frame_h) = (css_w * browser_scale, css_h * browser_scale);
    let max_w = (size.pixel_width as u64 * t.scale_percent as u64 / 100).max(1) as f64;
    let max_h = (size.pixel_height as u64 * t.scale_percent as u64 / 100).max(1) as f64;
    let fit = (max_w / frame_w).min(max_h / frame_h).min(1.0);
    (
        (frame_w * fit).round().max(1.0) as u32,
        (frame_h * fit).round().max(1.0) as u32,
    )
}

fn metrics_params(size: ViewSize, zoom: u32) -> Value {
    let (w, h, dsf) = css_size(size, zoom);
    json!({"width": w, "height": h, "deviceScaleFactor": dsf, "mobile": false})
}

impl View {
    pub fn spawn(
        engine: Arc<EngineShared>,
        id: u64,
        url: &str,
        cfg: &WebConfig,
        size: ViewSize,
        notify: Box<dyn Fn(WebEvent) + Send + Sync>,
    ) -> Result<Arc<View>> {
        let url = normalize_url(url).unwrap_or_else(|| {
            log::warn!("web view {id}: refused to open {:?}", sanitize_display(url, MAX_URL_CHARS));
            "about:blank".to_string()
        });
        let (tx, rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::unbounded();
        let core = Arc::new(ViewCore {
            id,
            cfg: cfg.clone(),
            engine: engine.clone(),
            notify,
            slot: FrameSlot::default(),
            shared: Mutex::new(Shared {
                info: PageInfo {
                    url: url.clone(),
                    loading: true,
                    ..Default::default()
                },
                state: WebState::Starting,
                cursor: None,
                main_frame: None,
            }),
            desired: Mutex::new(Desired {
                url,
                size,
                visible: true,
                focused: false,
                zoom: 100,
            }),
            attached: Mutex::new(None),
            tx,
            frame_tx,
            closed: AtomicBool::new(false),
            revivable: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            tuning: Tuning::from_env(),
            frame_gate: Mutex::new(()),
            hold_frames: Mutex::new(None),
            expect: Mutex::new(Expect {
                w: size.pixel_width,
                h: size.pixel_height,
                since: Instant::now(),
            }),
        });
        engine.register(&core);

        let c = core.clone();
        std::thread::Builder::new()
            .name(format!("hgweb-view-{id}"))
            .spawn(move || {
                let c2 = c.clone();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Worker::new(c).run(rx)))
                    .is_err()
                {
                    c2.thread_panicked("view worker");
                }
            })?;
        let c = core.clone();
        std::thread::Builder::new()
            .name(format!("hgweb-decode-{id}"))
            .spawn(move || {
                let c2 = c.clone();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.decode_loop(frame_rx)))
                    .is_err()
                {
                    c2.thread_panicked("frame decoder");
                }
            })?;
        Ok(Arc::new(View { core }))
    }
}

impl Drop for View {
    fn drop(&mut self) {
        self.core.close();
    }
}

impl ViewCore {
    fn emit(&self, ev: WebEvent) {
        (self.notify)(ev);
    }

    fn current(&self) -> Option<Arc<Attached>> {
        self.attached.lock().clone()
    }

    /// The device scale factor of the browser the page lives in (1 before there is one).
    fn browser_scale(&self) -> f64 {
        self.current().map_or(1.0, |a| a.browser.scale)
    }

    /// The scale factor this view's pane asks for (`dpi / 96`): what a browser launched for
    /// it runs at.
    pub fn wanted_scale(&self) -> f64 {
        scale_of(self.desired.lock().size)
    }

    /// `Browser.setWindowBounds` state (`normal` / `minimized`) of the page's window: a
    /// minimized window hides its page, so rendering stops entirely until it is restored.
    fn window_state(&self, state: &str) {
        if let Some(a) = self.current() {
            if let Some(w) = a.window_id {
                a.browser.conn.notify(
                    "Browser.setWindowBounds",
                    json!({"windowId": w, "bounds": {"windowState": state}}),
                    None,
                );
            }
        }
    }

    /// Send a command to the page, if there is one. Never blocks.
    fn page_send(&self, method: &'static str, params: Value) {
        if let Some(a) = self.current() {
            a.browser.conn.notify(method, params, Some(&a.session));
        }
    }

    fn set_state(&self, state: WebState) {
        let changed = {
            let mut s = self.shared.lock();
            if s.state == state {
                false
            } else {
                s.state = state;
                true
            }
        };
        if changed {
            self.emit(WebEvent::State);
        }
    }

    /// A thread of the view panicked: the pane shows its placeholder instead of a frozen page.
    fn thread_panicked(&self, what: &str) {
        log::error!("web view {}: {what} thread panicked", self.id);
        self.revivable.store(false, Ordering::Release);
        {
            let _gate = self.frame_gate.lock();
            self.slot.clear();
        }
        self.update_info(|i| i.loading = false);
        self.set_state(WebState::Crashed {
            reason: format!("internal error in the {what}"),
        });
    }

    /// Most pixels a decoded frame may have: a few times the area the view asked for.
    fn max_frame_pixels(&self) -> usize {
        let e = self.expect.lock();
        (e.w as usize * e.h as usize * 4).max(1 << 20)
    }

    fn update_info(&self, f: impl FnOnce(&mut PageInfo)) {
        let changed = {
            let mut s = self.shared.lock();
            let before = s.info.clone();
            f(&mut s.info);
            // Title and URL are the page's: made safe and bounded here, for every writer.
            s.info.title = sanitize_display(&s.info.title, MAX_TITLE_CHARS);
            s.info.url = sanitize_display(&s.info.url, MAX_URL_CHARS);
            s.info != before
        };
        if changed {
            self.emit(WebEvent::Info);
        }
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Msg::Close);
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// From a browser: its process or pipe is gone.
    pub fn browser_died(&self, browser_id: u64, reason: &str) {
        let _ = self.tx.send(Msg::BrowserDied {
            browser_id,
            reason: reason.to_string(),
        });
    }

    /// From a browser: a `Target.*` event about this view's page.
    pub fn target_event(&self, ev: Event) {
        let _ = self.tx.send(Msg::Target(ev));
    }

    /// Another view relaunched the browser of this profile: come back if the browser loss is
    /// what crashed this view.
    pub fn revive(&self) {
        if self.revivable.load(Ordering::Acquire) {
            let _ = self.tx.send(Msg::Revive);
        }
    }

    /// CDP session events, on the reader thread. Frames go to the decoder, the rest to the worker.
    fn session_event(&self, epoch: u64, mut ev: Event) {
        match ev.method.as_str() {
            "Page.screencastFrame" => {
                let Some(att) = self.current().filter(|a| a.epoch == epoch) else {
                    return;
                };
                let data = match ev.params["data"].take() {
                    Value::String(s) => s,
                    _ => return,
                };
                let screencast_session = ev.params["sessionId"].as_i64().unwrap_or(0);
                // Decided here, in event order, not later on the decoder thread.
                let held = self.hold_frames.lock().is_some_and(|until| Instant::now() < until);
                let _ = self.frame_tx.send(FrameJob::Frame {
                    attached: att,
                    screencast_session,
                    data,
                    held,
                });
                return;
            }
            // Page script can call the binding as often as it likes: it is judged here, on the
            // reader thread, and never queued for the worker.
            "Runtime.bindingCalled" if ev.params["name"] == "hgcursor" => {
                if let Some(c) = ev.params["payload"].as_str().and_then(allowed_cursor) {
                    self.cursor_reported(c);
                }
                return;
            }
            "Page.frameNavigated" if ev.params["frame"]["parentId"].is_null() => {
                // The page this view was created for has committed: its frames are shown from
                // here on (until then the initial blank document would flash white).
                *self.hold_frames.lock() = None;
            }
            _ => {}
        }
        let _ = self.tx.send(Msg::Cdp { epoch, ev });
    }

    /// The page's cursor changed to `c` (already vetted). Tells the owner only on a change.
    fn cursor_reported(&self, c: &'static str) {
        let changed = {
            let mut s = self.shared.lock();
            if s.cursor.as_deref() == Some(c) {
                false
            } else {
                s.cursor = Some(c.to_string());
                true
            }
        };
        if changed {
            self.emit(WebEvent::Cursor(c.to_string()));
        }
    }

    /// `Ok` when a decoded frame has the size this view asked Chrome for; `Err(since)` (when the
    /// expectation was set) when it has not. Right after a resize (and at startup) Chrome still
    /// delivers frames of the previous surface size for a few frames; those are not shown until
    /// `SIZE_SETTLE` has passed, and after that everything is accepted, so a size Chrome refuses to
    /// produce (window limits, a page scale above the browser's `--force-device-scale-factor`)
    /// cannot starve the view.
    fn check_size(&self, w: u32, h: u32) -> Result<(), Instant> {
        let e = self.expect.lock();
        if (w.abs_diff(e.w) <= 2 && h.abs_diff(e.h) <= 2) || e.since.elapsed() > SIZE_SETTLE {
            Ok(())
        } else {
            Err(e.since)
        }
    }

    /// The frame size to expect for `d`, given the device scale factor Chrome was launched with.
    fn expect_size(&self, d: &Desired, browser_scale: f64) {
        let (w, h) = expected_frame(d.size, d.zoom, browser_scale, self.tuning);
        *self.expect.lock() = Expect {
            w,
            h,
            since: Instant::now(),
        };
    }

    /// Put a decoded frame in the slot unless the page was torn down meanwhile (checked under
    /// the gate teardown takes). Returns whether it was stored.
    fn publish(&self, d: Decoded, epoch: u64) -> bool {
        let _gate = self.frame_gate.lock();
        let still_current = self.epoch.load(Ordering::Acquire) == epoch;
        if still_current {
            self.slot.store(d);
        }
        still_current
    }

    fn ack(att: &Attached, screencast_session: i64) {
        att.browser.conn.notify(
            "Page.screencastFrameAck",
            json!({"sessionId": screencast_session}),
            Some(&att.session),
        );
    }

    /// Show a frame that was refused for its size once the size had its settling time, so a
    /// static page (Chrome sends no further frame) is never left blank.
    fn show_rejected(&self, r: Rejected) {
        if self.expect.lock().since != r.since {
            return; // resized meanwhile: this frame is of an even older surface
        }
        if self.publish(r.d, r.epoch) {
            self.emit(WebEvent::Frame);
        }
    }

    fn decode_loop(self: Arc<Self>, rx: flume::Receiver<FrameJob>) {
        let mut jpeg = Vec::new();
        let mut warned = false;
        let mut rejected: Option<Rejected> = None;
        'outer: loop {
            let first = match rejected.as_ref().map(|r| r.show_at) {
                Some(at) => match rx.recv_deadline(at) {
                    Ok(job) => job,
                    Err(flume::RecvTimeoutError::Timeout) => {
                        if let Some(r) = rejected.take() {
                            self.show_rejected(r);
                        }
                        continue;
                    }
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                },
            };
            let mut job = first;
            // Latest wins: frames that queued up behind a slow decode are only acknowledged.
            loop {
                match rx.try_recv() {
                    Ok(next @ FrameJob::Frame { .. }) => {
                        if let FrameJob::Frame {
                            attached,
                            screencast_session,
                            ..
                        } = &job
                        {
                            Self::ack(attached, *screencast_session);
                        }
                        job = next;
                    }
                    Ok(FrameJob::Stop) => break 'outer,
                    Err(_) => break,
                }
            }
            let FrameJob::Frame {
                attached,
                screencast_session,
                data,
                held,
            } = job
            else {
                break;
            };

            let current = self.epoch.load(Ordering::Acquire) == attached.epoch
                && !self.is_closed()
                && self.current().is_some_and(|a| a.epoch == attached.epoch);
            if current && held {
                // Not decoded, not shown: only acknowledged so Chrome keeps sending.
                Self::ack(&attached, screencast_session);
            } else if current {
                jpeg.clear();
                let started = Instant::now();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode_vec(data.as_bytes(), &mut jpeg)
                    .map_err(|e| anyhow!("screencast frame is not base64: {e}"))
                    .and_then(|_| decode_jpeg(&jpeg, self.slot.take_spare(), self.max_frame_pixels()));
                match decoded {
                    Ok(d) => {
                        record_decode(started.elapsed());
                        match self.check_size(d.width, d.height) {
                            Ok(()) => {
                                rejected = None;
                                // Re-checked under the gate: teardown may have run during the decode.
                                let shown = self.publish(d, attached.epoch);
                                Self::ack(&attached, screencast_session);
                                if shown {
                                    self.emit(WebEvent::Frame);
                                }
                            }
                            Err(since) => {
                                // Kept, to be shown when the settling time is over if nothing
                                // better has come by then.
                                rejected = Some(Rejected {
                                    d,
                                    epoch: attached.epoch,
                                    since,
                                    show_at: since + SIZE_SETTLE + Duration::from_millis(20),
                                });
                                Self::ack(&attached, screencast_session);
                            }
                        }
                    }
                    Err(e) => {
                        Self::ack(&attached, screencast_session);
                        if !warned {
                            warned = true;
                            log::warn!("dropping undecodable screencast frame: {e:#}");
                        }
                    }
                }
            } else {
                Self::ack(&attached, screencast_session);
            }
        }
    }

    // ---- input: straight to the writer queue ----

    fn mouse(&self, ev: WebMouse) {
        if self.is_closed() {
            return;
        }
        fn button_name(b: u8) -> &'static str {
            match b {
                0 => "left",
                1 => "middle",
                2 => "right",
                3 => "back",
                4 => "forward",
                _ => "none",
            }
        }
        // `buttons` is the DOM bitmask; the lowest set bit names the button of a drag.
        fn from_mask(m: u8) -> &'static str {
            match m {
                m if m & 1 != 0 => "left",
                m if m & 2 != 0 => "right",
                m if m & 4 != 0 => "middle",
                m if m & 8 != 0 => "back",
                m if m & 16 != 0 => "forward",
                _ => "none",
            }
        }
        // The pane works in CSS pixels at zoom 100; a zoomed page has a CSS viewport of
        // `pixels / zoom` (see `css_size`), so positions shrink with it.
        let zoom = self.desired.lock().zoom.clamp(10, 500) as f64 / 100.0;
        let mut p = json!({
            "x": ev.x as f64 / zoom,
            "y": ev.y as f64 / zoom,
            "modifiers": ev.mods,
            "buttons": ev.buttons,
            "pointerType": "mouse",
        });
        match ev.kind {
            WebMouseKind::Move => {
                p["type"] = json!("mouseMoved");
                p["button"] = json!(from_mask(ev.buttons));
            }
            WebMouseKind::Down | WebMouseKind::Up => {
                p["type"] = json!(if ev.kind == WebMouseKind::Down {
                    "mousePressed"
                } else {
                    "mouseReleased"
                });
                p["button"] = json!(button_name(ev.button));
                p["clickCount"] = json!(ev.click_count.max(1));
            }
            WebMouseKind::Wheel => {
                p["type"] = json!("mouseWheel");
                p["deltaX"] = json!(ev.dx);
                p["deltaY"] = json!(ev.dy);
            }
        }
        self.page_send("Input.dispatchMouseEvent", p);
    }

    fn key(&self, ev: WebKey) {
        if self.is_closed() {
            return;
        }
        if let Some(k) = keys::translate(&ev.key, ev.mods) {
            self.page_send("Input.dispatchKeyEvent", k.params(ev.down));
        }
    }

    fn copy_selection(self: &Arc<Self>) {
        let Some(a) = self.current() else { return };
        let weak = Arc::downgrade(self);
        a.browser.conn.send(
            "Runtime.evaluate",
            json!({"expression": COPY_JS, "returnByValue": true}),
            Some(&a.session),
            Some(Box::new(move |r| {
                let text = r
                    .ok()
                    .and_then(|v| v["result"]["value"].as_str().map(str::to_string))
                    .unwrap_or_default();
                if !text.is_empty() {
                    if let Some(core) = weak.upgrade() {
                        core.emit(WebEvent::ClipboardText(text));
                    }
                }
            })),
        );
    }

    /// Ask the page for its history position and flags (back/forward availability).
    fn refresh_history(self: &Arc<Self>) {
        let Some(a) = self.current() else { return };
        let weak = Arc::downgrade(self);
        a.browser.conn.send(
            "Page.getNavigationHistory",
            json!({}),
            Some(&a.session),
            Some(Box::new(move |r| {
                let (Ok(v), Some(core)) = (r, weak.upgrade()) else {
                    return;
                };
                let (Some(cur), Some(n)) = (
                    v["currentIndex"].as_i64(),
                    v["entries"].as_array().map(Vec::len),
                ) else {
                    return;
                };
                core.update_info(|i| {
                    i.can_go_back = cur > 0;
                    i.can_go_forward = (cur as usize) + 1 < n;
                });
            })),
        );
    }

    /// Read `document.title`. Chrome's own target title lags (it is the file name or URL until
    /// a navigation settles and does not follow later changes), so the title is asked for on
    /// load events and polled.
    fn refresh_title(self: &Arc<Self>) {
        let Some(a) = self.current() else { return };
        let weak = Arc::downgrade(self);
        a.browser.conn.send(
            "Runtime.evaluate",
            json!({"expression": "document.title", "returnByValue": true}),
            Some(&a.session),
            Some(Box::new(move |r| {
                if let (Ok(v), Some(core)) = (r, weak.upgrade()) {
                    if let Some(t) = v["result"]["value"].as_str() {
                        core.update_info(|i| i.title = t.to_string());
                    }
                }
            })),
        );
    }

    /// Step through the history: `delta` -1 back, +1 forward.
    fn go_history(self: &Arc<Self>, delta: i64) {
        let Some(a) = self.current() else { return };
        let weak = Arc::downgrade(self);
        a.browser.conn.send(
            "Page.getNavigationHistory",
            json!({}),
            Some(&a.session),
            Some(Box::new(move |r| {
                let (Ok(v), Some(core)) = (r, weak.upgrade()) else {
                    return;
                };
                let Some(cur) = v["currentIndex"].as_i64() else {
                    return;
                };
                let entry = v["entries"]
                    .as_array()
                    .and_then(|e| usize::try_from(cur + delta).ok().and_then(|i| e.get(i)))
                    .and_then(|e| e["id"].as_i64());
                if let Some(id) = entry {
                    core.page_send("Page.navigateToHistoryEntry", json!({"entryId": id}));
                }
            })),
        );
    }
}

#[derive(Default)]
struct Applied {
    size: Option<ViewSize>,
    zoom: u32,
    visible: bool,
    focused: bool,
}

struct Worker {
    core: Arc<ViewCore>,
    applied: Applied,
    resize_since: Option<Instant>,
    resize_due: Option<Instant>,
    /// Next `document.title` poll (Chrome sends no event for title changes).
    title_due: Option<Instant>,
}

impl Worker {
    fn new(core: Arc<ViewCore>) -> Self {
        Worker {
            core,
            applied: Applied::default(),
            resize_since: None,
            resize_due: None,
            title_due: None,
        }
    }

    fn run(mut self, rx: flume::Receiver<Msg>) {
        self.bring_up();
        loop {
            let deadline = [self.resize_due, self.title_due].into_iter().flatten().min();
            let msg = match deadline {
                Some(due) => match rx.recv_deadline(due) {
                    Ok(m) => Some(m),
                    Err(flume::RecvTimeoutError::Timeout) => None,
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break,
                },
            };
            match msg {
                Some(m) => {
                    if !self.handle(m) {
                        break;
                    }
                }
                None => self.on_timer(),
            }
        }
        self.finish();
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        if self.resize_due.is_some_and(|d| d <= now) {
            self.resize_due = None;
            self.resize_since = None;
            self.apply_size();
        }
        if self.title_due.is_some_and(|d| d <= now) {
            self.title_due = Some(now + TITLE_POLL);
            if self.applied.visible {
                self.core.refresh_title();
            }
        }
    }

    /// Returns false when the view is finished.
    fn handle(&mut self, msg: Msg) -> bool {
        let core = self.core.clone();
        match msg {
            Msg::Close => return false,
            Msg::Cdp { epoch, ev } => {
                if core.epoch.load(Ordering::Acquire) == epoch {
                    self.on_session_event(ev);
                }
            }
            Msg::Target(ev) => return self.on_target_event(ev),
            Msg::BrowserDied { browser_id, reason } => {
                let ours = core.current().is_some_and(|a| a.browser.id == browser_id);
                if ours {
                    self.drop_attachment(false);
                    self.crash(reason, true);
                }
            }
            Msg::Navigate(url) => {
                let Some(url) = normalize_url(&url) else {
                    log::warn!("web view: refused to navigate to {:?}", sanitize_display(&url, MAX_URL_CHARS));
                    return true;
                };
                core.desired.lock().url = url.clone();
                if self.is_crashed() {
                    self.restart();
                } else {
                    core.update_info(|i| {
                        i.url = url.clone();
                        i.loading = true;
                    });
                    core.page_send("Page.navigate", json!({"url": url}));
                }
            }
            Msg::Back => core.go_history(-1),
            Msg::Forward => core.go_history(1),
            Msg::Reload(hard) => {
                if self.is_crashed() {
                    self.restart();
                } else {
                    core.page_send("Page.reload", json!({"ignoreCache": hard}));
                }
            }
            Msg::Stop => core.page_send("Page.stopLoading", json!({})),
            Msg::Sync => self.sync(),
            Msg::Restart => self.restart(),
            Msg::Revive => {
                if self.is_crashed() && core.revivable.load(Ordering::Acquire) {
                    self.bring_up();
                }
            }
        }
        true
    }

    fn is_crashed(&self) -> bool {
        matches!(self.core.shared.lock().state, WebState::Crashed { .. })
    }

    fn crash(&mut self, reason: String, revivable: bool) {
        let core = &self.core;
        core.revivable.store(revivable, Ordering::Release);
        {
            let _gate = core.frame_gate.lock();
            core.slot.clear();
        }
        core.update_info(|i| i.loading = false);
        core.set_state(WebState::Crashed { reason });
        self.title_due = None;
    }

    fn restart(&mut self) {
        self.drop_attachment(true);
        self.bring_up();
    }

    fn bring_up(&mut self) {
        self.core.set_state(WebState::Starting);
        self.core.revivable.store(false, Ordering::Release);
        match self.try_bring_up() {
            Ok(()) => {
                self.core.set_state(WebState::Running);
                self.title_due = Some(Instant::now() + TITLE_POLL);
                // A view closed while the browser was starting must not linger.
                if self.core.is_closed() {
                    self.drop_attachment(true);
                }
            }
            Err(e) => {
                log::warn!("web view {} failed to start: {e:#}", self.core.id);
                self.drop_attachment(true);
                // A browser that died under us is worth another go when it is relaunched.
                let revivable = e.downcast_ref::<CdpError>() == Some(&CdpError::Closed);
                self.crash(format!("{e:#}"), revivable);
            }
        }
    }

    fn try_bring_up(&mut self) -> Result<()> {
        let core = self.core.clone();
        let browser = core.engine.acquire(&core)?;
        let d = core.desired.lock().clone();
        core.expect_size(&d, browser.scale);
        // The window starts on about:blank; showing that white frame before the real page
        // commits would flash. Frames are held until the first main-frame navigation commits
        // (or `HOLD_FRAMES` pass, for navigations that never commit).
        *core.hold_frames.lock() = (d.url != "about:blank" && !d.url.is_empty())
            .then(|| Instant::now() + HOLD_FRAMES);
        let epoch = core.epoch.fetch_add(1, Ordering::AcqRel) + 1;

        let context = if core.cfg.private {
            let v = browser.conn.call(
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
                None,
                CALL_TIMEOUT,
            )?;
            let id = v["browserContextId"]
                .as_str()
                .ok_or_else(|| anyhow!("createBrowserContext returned no id"))?
                .to_string();
            browser.apply_policies(Some(&id));
            Some(id)
        } else {
            None
        };

        let (w, h, _) = css_size(d.size, d.zoom);
        let mut create = json!({
            "url": "about:blank",
            "width": w,
            "height": h,
            "newWindow": true,
        });
        if let Some(c) = &context {
            create["browserContextId"] = json!(c);
        }
        let v = browser
            .conn
            .call("Target.createTarget", create, None, CALL_TIMEOUT)?;
        let target_id = v["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("createTarget returned no targetId"))?
            .to_string();

        let weak = Arc::downgrade(&core);
        let session = browser.conn.attach(
            &target_id,
            Arc::new(move |ev| {
                if let Some(c) = weak.upgrade() {
                    c.session_event(epoch, ev);
                }
            }),
            CALL_TIMEOUT,
        )?;
        // The window of the page: minimizing it is how a hidden view stops rendering (a frozen
        // page would stay `hidden` for good after being unfrozen).
        let window_id = window_id_of(&browser, &target_id);
        let att = Arc::new(Attached {
            browser: browser.clone(),
            session,
            target_id: target_id.clone(),
            context,
            window_id,
            epoch,
        });
        *core.attached.lock() = Some(att.clone());
        browser.register_target(&target_id, Arc::downgrade(&core));

        // Everything below is ordered on the one writer queue; only Page.enable is awaited, to
        // learn early that the session is usable.
        let conn = &browser.conn;
        let s = Some(att.session.as_str());
        conn.call("Page.enable", json!({}), s, CALL_TIMEOUT)?;
        conn.notify("Runtime.enable", json!({}), s);
        conn.notify("Runtime.addBinding", json!({"name": "hgcursor"}), s);
        conn.notify(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({"source": CURSOR_JS, "runImmediately": true}),
            s,
        );
        conn.notify("Emulation.setDeviceMetricsOverride", metrics_params(d.size, d.zoom), s);
        conn.notify(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": d.focused}),
            s,
        );
        {
            let weak = Arc::downgrade(&core);
            conn.send(
                "Page.getFrameTree",
                json!({}),
                s,
                Some(Box::new(move |r| {
                    if let (Ok(v), Some(core)) = (r, weak.upgrade()) {
                        if let Some(id) = v["frameTree"]["frame"]["id"].as_str() {
                            core.shared.lock().main_frame = Some(id.to_string());
                        }
                    }
                })),
            );
        }
        if d.visible {
            conn.notify("Page.startScreencast", screencast_params(d.size, core.tuning), s);
        } else {
            core.window_state("minimized");
        }
        let url = if d.url.is_empty() { "about:blank" } else { d.url.as_str() };
        core.update_info(|i| {
            i.url = url.to_string();
            i.loading = true;
        });
        conn.notify("Page.navigate", json!({"url": url}), s);

        self.applied = Applied {
            size: Some(d.size),
            zoom: d.zoom,
            visible: d.visible,
            focused: d.focused,
        };
        self.resize_due = None;
        self.resize_since = None;
        // `desired` may have moved on while the browser was starting.
        self.sync();
        Ok(())
    }

    /// Detach from the page: close the target (and a private context) and forget the session.
    fn drop_attachment(&mut self, close_target: bool) {
        let core = &self.core;
        {
            // A frame being decoded for the old page must not land after the slot was emptied.
            let _gate = core.frame_gate.lock();
            core.epoch.fetch_add(1, Ordering::AcqRel);
            core.slot.clear();
        }
        let Some(att) = core.attached.lock().take() else {
            return;
        };
        att.browser.unregister_target(&att.target_id);
        att.browser.conn.remove_sink(&Some(att.session.clone()));
        if close_target && att.browser.is_usable() {
            att.browser
                .conn
                .notify("Target.closeTarget", json!({"targetId": att.target_id}), None);
            if let Some(c) = &att.context {
                att.browser.conn.notify(
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": c}),
                    None,
                );
            }
        }
        self.applied = Applied::default();
    }

    fn finish(&mut self) {
        let core = self.core.clone();
        core.closed.store(true, Ordering::Release);
        self.drop_attachment(true);
        let _ = core.frame_tx.send(FrameJob::Stop);
        core.engine.release(&core);
    }

    /// Bring the page in line with `desired` (visibility, focus, zoom; size via the debounce).
    fn sync(&mut self) {
        let core = self.core.clone();
        if core.current().is_none() {
            return;
        }
        let d = core.desired.lock().clone();

        if d.zoom != self.applied.zoom {
            self.applied.zoom = d.zoom;
            core.expect_size(&d, core.browser_scale());
            self.apply_metrics(&d);
        }
        if d.visible != self.applied.visible {
            self.applied.visible = d.visible;
            if d.visible {
                core.window_state("normal");
                core.page_send("Page.startScreencast", screencast_params(d.size, core.tuning));
            } else {
                core.page_send("Page.stopScreencast", json!({}));
                core.window_state("minimized");
            }
        }
        if d.focused != self.applied.focused {
            self.applied.focused = d.focused;
            core.page_send(
                "Emulation.setFocusEmulationEnabled",
                json!({"enabled": d.focused}),
            );
            if d.focused {
                core.page_send("Page.bringToFront", json!({}));
            }
        }
        if Some(d.size) != self.applied.size {
            let now = Instant::now();
            let since = *self.resize_since.get_or_insert(now);
            self.resize_due = Some((now + RESIZE_DEBOUNCE).min(since + RESIZE_MAX_DELAY));
        }
    }

    fn apply_metrics(&self, d: &Desired) {
        self.core.page_send("Emulation.setDeviceMetricsOverride", metrics_params(d.size, d.zoom));
    }

    fn apply_size(&mut self) {
        let core = self.core.clone();
        let d = core.desired.lock().clone();
        if core.current().is_none() || Some(d.size) == self.applied.size {
            return;
        }
        self.applied.size = Some(d.size);
        core.expect_size(&d, core.browser_scale());
        self.apply_metrics(&d);
        if self.applied.visible {
            // The screencast keeps the dimensions it was started with.
            core.page_send("Page.stopScreencast", json!({}));
            core.page_send("Page.startScreencast", screencast_params(d.size, core.tuning));
        }
    }

    // ---- events ----

    fn on_session_event(&mut self, ev: Event) {
        let core = self.core.clone();
        let p = &ev.params;
        match ev.method.as_str() {
            "Page.frameNavigated" => {
                let f = &p["frame"];
                if f["parentId"].is_null() {
                    if let Some(id) = f["id"].as_str() {
                        core.shared.lock().main_frame = Some(id.to_string());
                    }
                    // A page that failed to load is shown as chrome-error://; the user wants to
                    // see (and retry) the URL they asked for.
                    let url = f["unreachableUrl"]
                        .as_str()
                        .or_else(|| f["url"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !url.is_empty() {
                        if !url.starts_with("chrome-error:") {
                            core.desired.lock().url = url.clone();
                        }
                        core.update_info(|i| {
                            i.url = url;
                            i.title.clear();
                        });
                    }
                    core.refresh_history();
                }
            }
            "Page.navigatedWithinDocument" => {
                let is_main = {
                    let s = core.shared.lock();
                    s.main_frame.as_deref() == p["frameId"].as_str() || s.main_frame.is_none()
                };
                if let (true, Some(url)) = (is_main, p["url"].as_str()) {
                    core.desired.lock().url = url.to_string();
                    core.update_info(|i| i.url = url.to_string());
                    core.refresh_history();
                    core.refresh_title();
                }
            }
            "Page.frameStartedLoading" => {
                if self.is_main_frame(p["frameId"].as_str()) {
                    core.update_info(|i| i.loading = true);
                }
            }
            "Page.frameStoppedLoading" => {
                if self.is_main_frame(p["frameId"].as_str()) {
                    core.update_info(|i| i.loading = false);
                    core.refresh_history();
                    core.refresh_title();
                }
            }
            "Page.domContentEventFired" | "Page.loadEventFired" => core.refresh_title(),
            "Page.javascriptDialogOpening" => {
                // alert: acknowledge. confirm/prompt: answer "cancel". beforeunload: let the page go.
                let ty = p["type"].as_str().unwrap_or("");
                let accept = matches!(ty, "alert" | "beforeunload");
                log::info!(
                    "web view {}: {ty} dialog dismissed ({}): {}",
                    core.id,
                    if accept { "accepted" } else { "cancelled" },
                    sanitize_display(p["message"].as_str().unwrap_or(""), MAX_TITLE_CHARS)
                );
                core.page_send("Page.handleJavaScriptDialog", json!({"accept": accept}));
            }
            "Page.windowOpen" => {
                // window.open / target=_blank: the new window was closed by the browser level
                // handler; the pane navigates itself to the URL instead.
                // The URL is the page's choice: only web addresses are followed (a script must
                // not steer the pane to data:, file:, javascript: or other schemes).
                if let Some(url) = p["url"].as_str().filter(|u| popup_url_allowed(u)).and_then(normalize_url) {
                    core.desired.lock().url = url.clone();
                    core.update_info(|i| {
                        i.url = url.clone();
                        i.loading = true;
                    });
                    core.page_send("Page.navigate", json!({"url": url}));
                }
            }
            _ => {}
        }
    }

    fn is_main_frame(&self, frame_id: Option<&str>) -> bool {
        let s = self.core.shared.lock();
        match (&s.main_frame, frame_id) {
            (Some(m), Some(f)) => m == f,
            _ => true,
        }
    }

    fn on_target_event(&mut self, ev: Event) -> bool {
        let core = self.core.clone();
        match ev.method.as_str() {
            // Chrome's targetInfo title is the file name or URL until it feels like updating, and
            // it does not follow `document.title`; the title comes from `refresh_title`.
            "Target.targetCrashed" => {
                let status = ev.params["status"].as_str().unwrap_or("crashed");
                // The crashed page is of no use; close its window now rather than at restart.
                self.drop_attachment(true);
                self.crash(format!("The page crashed ({status})"), false);
            }
            "Target.targetDestroyed" => {
                if !core.is_closed() {
                    // The page closed itself (window.close()).
                    core.closed.store(true, Ordering::Release);
                    core.emit(WebEvent::Closed);
                    return false;
                }
            }
            _ => {}
        }
        true
    }
}

fn screencast_params(size: ViewSize, t: Tuning) -> Value {
    let scaled = |px: u32| (px as u64 * t.scale_percent as u64 / 100).max(1);
    json!({
        "format": "jpeg",
        "quality": t.quality,
        "maxWidth": scaled(size.pixel_width),
        "maxHeight": scaled(size.pixel_height),
        "everyNthFrame": 1,
    })
}

impl WebView for View {
    fn navigate(&self, url: &str) {
        let _ = self.core.tx.send(Msg::Navigate(url.to_string()));
    }

    fn back(&self) {
        let _ = self.core.tx.send(Msg::Back);
    }

    fn forward(&self) {
        let _ = self.core.tx.send(Msg::Forward);
    }

    fn reload(&self, hard: bool) {
        let _ = self.core.tx.send(Msg::Reload(hard));
    }

    fn stop(&self) {
        let _ = self.core.tx.send(Msg::Stop);
    }

    fn resize(&self, size: ViewSize) {
        if size.pixel_width == 0 || size.pixel_height == 0 {
            return;
        }
        {
            let mut d = self.core.desired.lock();
            if d.size == size {
                return;
            }
            d.size = size;
        }
        let _ = self.core.tx.send(Msg::Sync);
    }

    fn set_visible(&self, visible: bool) {
        {
            let mut d = self.core.desired.lock();
            if d.visible == visible {
                return;
            }
            d.visible = visible;
        }
        let _ = self.core.tx.send(Msg::Sync);
    }

    fn set_focused(&self, focused: bool) {
        {
            let mut d = self.core.desired.lock();
            if d.focused == focused {
                return;
            }
            d.focused = focused;
        }
        let _ = self.core.tx.send(Msg::Sync);
    }

    fn mouse(&self, ev: WebMouse) {
        self.core.mouse(ev);
    }

    fn key(&self, ev: WebKey) {
        self.core.key(ev);
    }

    fn insert_text(&self, text: &str) {
        if !self.core.is_closed() && !text.is_empty() {
            self.core.page_send("Input.insertText", json!({"text": text}));
        }
    }

    fn copy_selection(&self) {
        self.core.copy_selection();
    }

    fn take_frame(&self) -> Option<Arc<Frame>> {
        self.core.slot.latest()
    }

    fn info(&self) -> PageInfo {
        self.core.shared.lock().info.clone()
    }

    fn state(&self) -> WebState {
        self.core.shared.lock().state.clone()
    }

    fn cursor(&self) -> Option<String> {
        self.core.shared.lock().cursor.clone()
    }

    fn close(&self) {
        self.core.close();
    }

    fn restart(&self) {
        let _ = self.core.tx.send(Msg::Restart);
    }

    fn set_zoom(&self, percent: u32) {
        let z = percent.clamp(10, 500);
        {
            let mut d = self.core.desired.lock();
            if d.zoom == z {
                return;
            }
            d.zoom = z;
        }
        let _ = self.core.tx.send(Msg::Sync);
    }

    fn devtools_url(&self) -> Option<String> {
        let a = self.core.current()?;
        let port = a.browser.devtools_port()?;
        Some(format!(
            "http://127.0.0.1:{port}/devtools/inspector.html?ws=127.0.0.1:{port}/devtools/page/{}",
            a.target_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_frame_follows_the_browsers_scale_not_the_pages() {
        let t = Tuning { quality: 80, scale_percent: 100 };
        let s = |w, h, dpi| ViewSize { pixel_width: w, pixel_height: h, dpi };
        // Browser at the view's own scale: physical pixels, at any dpi.
        assert_eq!(expected_frame(s(546, 800, 96), 100, 1.0, t), (546, 800));
        assert_eq!(expected_frame(s(546, 800, 144), 100, 1.5, t), (546, 800));
        assert_eq!(expected_frame(s(546, 800, 192), 100, 2.0, t), (546, 800));
        // A browser at a higher scale than the view's: scaled down to the view's pixels.
        assert_eq!(expected_frame(s(400, 300, 96), 100, 2.0, t), (400, 300));
        // A browser at a lower scale: Chrome's frames are CSS-sized (what the old code refused).
        assert_eq!(expected_frame(s(546, 800, 144), 100, 1.0, t), (364, 533));
        assert_eq!(expected_frame(s(546, 800, 192), 100, 1.0, t), (273, 400));
        // Zoom enlarges the page scale the same way.
        assert_eq!(expected_frame(s(546, 800, 144), 200, 1.5, t), (273, 401));
        // Reduced frame size (HIVEGRID_WEB_SCALE) caps the result.
        let half = Tuning { quality: 80, scale_percent: 50 };
        assert_eq!(expected_frame(s(546, 800, 144), 100, 1.5, half), (273, 400));
    }

    #[test]
    fn css_size_follows_dpi_and_zoom() {
        let s = ViewSize { pixel_width: 1000, pixel_height: 800, dpi: 96 };
        assert_eq!(css_size(s, 100), (1000.0, 800.0, 1.0));
        let hi = ViewSize { pixel_width: 2000, pixel_height: 1600, dpi: 192 };
        assert_eq!(css_size(hi, 100), (1000.0, 800.0, 2.0));
        // 125%: the page lays out in fewer CSS pixels but the frame stays the view's pixel size.
        let (w, h, dsf) = css_size(s, 125);
        assert_eq!((w, h, dsf), (800.0, 640.0, 1.25));
        // Out-of-range zoom is clamped, tiny views never reach zero.
        assert_eq!(css_size(s, 5).2, 0.1);
        let tiny = ViewSize { pixel_width: 1, pixel_height: 1, dpi: 96 };
        assert_eq!(css_size(tiny, 500).0, 1.0);
    }

    #[test]
    fn metrics_and_screencast_params() {
        let s = ViewSize { pixel_width: 1920, pixel_height: 1080, dpi: 120 };
        let m = metrics_params(s, 100);
        assert_eq!(m["width"], 1536.0);
        assert_eq!(m["height"], 864.0);
        assert_eq!(m["deviceScaleFactor"], 1.25);
        assert_eq!(m["mobile"], false);
        let sc = screencast_params(s, Tuning { quality: 70, scale_percent: 100 });
        assert_eq!(sc["format"], "jpeg");
        assert_eq!(sc["quality"], 70);
        assert_eq!(sc["maxWidth"], 1920);
        assert_eq!(sc["maxHeight"], 1080);
        assert_eq!(sc["everyNthFrame"], 1);
    }

    #[test]
    fn tuning_reads_quality_and_scale() {
        let d = Tuning::from_vars(None, None);
        assert_eq!((d.quality, d.scale_percent), (80, 100));
        let t = Tuning::from_vars(Some("60"), Some("50"));
        assert_eq!((t.quality, t.scale_percent), (60, 50));
        // Garbage and out-of-range values fall back to the defaults.
        let bad = Tuning::from_vars(Some("0"), Some("abc"));
        assert_eq!((bad.quality, bad.scale_percent), (80, 100));
        assert_eq!(Tuning::from_vars(Some("101"), Some("10")).quality, 80);
        assert_eq!(Tuning::from_vars(None, Some("10")).scale_percent, 100);
        assert_eq!(Tuning::from_vars(None, Some("150")).scale_percent, 100);
    }

    #[test]
    fn scale_shrinks_the_screencast_bounds_only() {
        let s = ViewSize { pixel_width: 1000, pixel_height: 800, dpi: 96 };
        let sc = screencast_params(s, Tuning { quality: 80, scale_percent: 50 });
        assert_eq!((sc["maxWidth"].clone(), sc["maxHeight"].clone()), (json!(500), json!(400)));
        assert_eq!(metrics_params(s, 100)["width"], 1000.0, "the page layout is unchanged");
    }
}
