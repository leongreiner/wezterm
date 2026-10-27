//! Web engine of the Hivegrid web pane.
//!
//! Out-of-process headless Chrome driven over the DevTools protocol on `--remote-debugging-pipe`
//! (no TCP port): one browser process per profile name, one page target per view, frames from
//! `Page.startScreencast` decoded to RGBA here and handed out through a latest-wins slot.
//! The crate has no dependency on the mux or the GUI; `WebPane` (in `mux`) and the renderer sit
//! on the types below.
//!
//! ```text
//! WebEngine::create_view ──► WebView ──► take_frame() ─► Arc<Frame> (RGBA)   renderer
//!                              ▲  │ notify(WebEvent::{Frame,Info,State,Cursor,..})
//!        mouse/key/navigate ───┘  └──► Chrome (CDP over pipe)
//! ```
//!
//! Environment knobs (all optional): `HIVEGRID_CHROME` (browser binary),
//! `HIVEGRID_WEB_QUALITY` (screencast JPEG quality 1..=100, default 80),
//! `HIVEGRID_WEB_SCALE` (frame size in percent of the view, 25..=100, default 100),
//! `HIVEGRID_WEB_DEVTOOLS=1` (also open a loopback DevTools port, see `WebView::devtools_url`).
//! Chrome's stderr goes to `<data_root>/profile-<name>.log` (rewritten per launch).

pub mod apc;
mod browser;
mod cdp;
mod engine;
#[cfg(test)]
mod fake;
mod frame;
pub mod keys;
#[cfg(test)]
mod lifecycle_tests;
mod urls;
mod view;

pub use apc::{ApcCommand, ApcParser, HeaderOp};
pub use engine::CdpEngine;
pub use frame::take_decode_times;
pub use browser::profile_dir_name;
pub use urls::{normalize_url, resolve_location};

use std::path::PathBuf;
use std::sync::Arc;

/// One decoded screencast frame.
pub struct Frame {
    /// Increases by one per decoded frame of a view; 0 is never used.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// sRGB, not premultiplied, `width * height * 4` bytes, alpha always 255.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("seq", &self.seq)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default)]
pub struct WebConfig {
    /// Browser binary the spawn command or the Settings asked for (`mux::webpane::browser_choice`
    /// decides which); `None` = discover (`HIVEGRID_CHROME`, `/opt/google/chrome/chrome`,
    /// `google-chrome-stable`, `google-chrome`, `chromium`, `chromium-browser`). Read when a
    /// browser process starts: the pool is keyed by profile alone (one Chrome per profile
    /// directory), so a view joins the browser already running for its profile whatever binary
    /// it names, and a changed choice applies to the next browser process of the profile.
    pub chrome: Option<PathBuf>,
    /// Profile name; one browser process and one persistent profile directory per name.
    pub profile: String,
    /// Ephemeral cookie/storage context (`Target.createBrowserContext`) for this view only.
    pub private: bool,
    /// `~/.local/share/hivegrid/web`; profiles live in `<data_root>/profile-<name>` (mode 0700).
    pub data_root: PathBuf,
    /// Extra Chrome command line flags, appended after the defaults (e.g. `--no-sandbox`).
    pub extra_flags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebState {
    Starting,
    Running,
    Crashed { reason: String },
}

/// Longest page title kept (characters).
pub const MAX_TITLE_CHARS: usize = 512;
/// Longest page URL kept for display (characters).
pub const MAX_URL_CHARS: usize = 2048;

/// Text a page controls (title, URL, dialog message) made safe to show or log: control
/// characters (`char::is_control`, U+2028/2029, bidi controls) are dropped and at most `max`
/// characters are kept. Escape sequences cannot survive it.
pub fn sanitize_display(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{2028}' | '\u{2029}' | '\u{061C}' | '\u{200E}' | '\u{200F}'
                        | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
                )
        })
        .take(max)
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageInfo {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewSize {
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// 96 = scale 1.0.
    pub dpi: u32,
}

/// What a view tells its owner. `notify` may be called from any engine thread and must not
/// block; the usual reaction is to wake the GUI and re-read `take_frame`/`info`/`state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebEvent {
    /// A new frame is available through `take_frame`.
    Frame,
    /// `info()` changed (URL, title, loading, history flags).
    Info,
    /// `state()` changed.
    State,
    /// The CSS cursor name under the pointer changed (`pointer`, `text`, `default`, ...).
    Cursor(String),
    /// The result of `copy_selection` (only sent when something is selected).
    ClipboardText(String),
    /// The page closed itself (`window.close()`); the view is finished. Not sent for `close()`.
    Closed,
}

pub trait WebEngine: Send + Sync {
    /// Create a view. Returns at once: the browser is started (or joined) on a background
    /// thread and `state()` is `Starting` until the page is up; a failure to start Chrome
    /// shows up as `WebState::Crashed` with an actionable reason, not as an error here.
    fn create_view(
        &self,
        url: &str,
        cfg: &WebConfig,
        size: ViewSize,
        notify: Box<dyn Fn(WebEvent) + Send + Sync>,
    ) -> anyhow::Result<Arc<dyn WebView>>;

    /// The process is about to exit: let every browser quit orderly (`Browser.close`, which
    /// flushes cookies and storage to the profile) and wait for them, at most `timeout` in all.
    /// A browser that has not exited by then is killed. Without this a browser would die with
    /// its parent (`PR_SET_PDEATHSIG`) and lose what it had not yet written.
    fn shutdown(&self, _timeout: std::time::Duration) {}
}

pub trait WebView: Send + Sync {
    /// Navigate; `url` goes through [`normalize_url`]; a refused address is ignored.
    fn navigate(&self, url: &str);
    fn back(&self);
    fn forward(&self);
    fn reload(&self, hard: bool);
    fn stop(&self);
    /// Debounced inside (trailing 40 ms, at most 150 ms behind a continuous drag).
    fn resize(&self, size: ViewSize);
    /// `false` stops the screencast and minimizes the page's window (the page is then hidden:
    /// no rendering, no animation frames); `true` restores it and resumes.
    fn set_visible(&self, visible: bool);
    fn set_focused(&self, focused: bool);
    fn mouse(&self, ev: WebMouse);
    fn key(&self, ev: WebKey);
    fn insert_text(&self, text: &str);
    /// The result arrives as `WebEvent::ClipboardText`.
    fn copy_selection(&self);
    /// The latest frame; `None` until the first one and again while the view is `Crashed`.
    fn take_frame(&self) -> Option<Arc<Frame>>;
    fn info(&self) -> PageInfo;
    fn state(&self) -> WebState;
    fn cursor(&self) -> Option<String>;
    /// Closes the page; the browser process of the profile exits when its last view closes.
    fn close(&self);

    // Additions to the §2.2 contract, all with defaults so existing implementations (fakes
    // in `mux` tests) keep compiling. They back the `restart`, `zoom` and `devtools` commands
    // of the APC control channel (`apc::ApcCommand::apply`).

    /// Relaunch a crashed page/browser (also useful on a hung page).
    fn restart(&self) {}
    /// Page zoom in percent, 10..=500.
    fn set_zoom(&self, _percent: u32) {}
    /// `http://127.0.0.1:<port>/devtools/inspector.html?...` for this page; only with
    /// `HIVEGRID_WEB_DEVTOOLS=1`.
    fn devtools_url(&self) -> Option<String> {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WebMouseKind {
    Move,
    Down,
    Up,
    Wheel,
}

/// A pointer event in CSS pixels inside the view.
///
/// `button` (Down/Up): DOM numbering, 0 left, 1 middle, 2 right, 3 back, 4 forward.
/// `buttons`: DOM bitmask of the buttons held (1 left, 2 right, 4 middle, 8 back, 16 forward);
/// for `Move` it decides whether this is a drag (`button` is ignored there).
/// `mods`: CDP bitmask, Alt=1, Ctrl=2, Meta=4, Shift=8 (see `keys::MOD_*`).
/// `dx`/`dy`: wheel deltas in CSS pixels (negative `dy` scrolls up).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WebMouse {
    pub kind: WebMouseKind,
    pub x: f32,
    pub y: f32,
    pub button: u8,
    pub buttons: u8,
    pub mods: u8,
    pub click_count: u8,
    pub dx: f32,
    pub dy: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WebKey {
    pub down: bool,
    pub key: termwiz::input::KeyCode,
    pub mods: termwiz::input::Modifiers,
}
