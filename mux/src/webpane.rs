//! `WebPane`: a pane that shows a web page (Hivegrid fork).
//!
//! The page lives in an out-of-process browser behind the `hivegrid_web::WebView` trait; this
//! file is the `mux::Pane` around it. A pane has no pty: input goes straight to the view,
//! frames are drawn by the GUI (`wezterm-gui/src/termwindow/render/web.rs`) as a texture in the
//! pane's rectangle, and everything that expects cells (`get_lines`, `cli get-text`, a pane
//! without a frame yet or with a crashed browser) sees a small placeholder grid: URL, title,
//! state.
//!
//! A web pane is created by the sentinel program `hivegrid:web <url>` (see [`parse_spawn`]),
//! which `LocalDomain::spawn_pane` hands to [`spawn`]. It is controlled by writing
//! `ESC _ hgweb ; <command> [; <arg>] ESC \` to the pane (`hivegrid_web::apc`).

use crate::domain::DomainId;
use crate::pane::{
    alloc_pane_id, impl_get_logical_lines_via_get_lines, CachePolicy, CloseReason,
    ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, WithPaneLines,
};
use crate::renderable::{
    terminal_for_each_logical_line_in_stable_range_mut, terminal_get_cursor_position,
    terminal_get_dimensions, terminal_get_dirty_lines, terminal_get_lines, terminal_with_lines_mut,
    RenderableDimensions, StableCursorPosition,
};
use crate::{Mux, MuxNotification};
use anyhow::{anyhow, Result};
use config::keyassignment::ScrollbackEraseMode;
use hivegrid_web::{
    sanitize_display, ApcParser, Frame, PageInfo, MAX_TITLE_CHARS, MAX_URL_CHARS, ViewSize, WebConfig, WebEngine, WebEvent, WebKey, WebMouse,
    WebMouseKind, WebState, WebView,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::CommandBuilder;
use rangeset::RangeSet;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use termwiz::input::Modifiers;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, ClipboardSelection, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
};

/// The program name that asks a domain for a web pane instead of a process.
pub const SPAWN_SENTINEL: &str = "hivegrid:web";
/// Environment variable (on the spawn command) naming the browser profile.
pub const ENV_PROFILE: &str = "HIVEGRID_WEB_PROFILE";
/// Environment variable (on the spawn command), `1` for an ephemeral profile.
pub const ENV_PRIVATE: &str = "HIVEGRID_WEB_PRIVATE";
/// Environment variable (on the spawn command) naming the browser binary of this pane: the
/// highest rung of the choice (see [`browser_choice`]). `wezterm cli split-pane --browser`.
pub const ENV_BROWSER: &str = "HIVEGRID_WEB_BROWSER";
/// Extra browser command line flags, whitespace separated (e.g. `--no-sandbox` when Chrome
/// cannot create its sandbox; unsafe). Read when a pane is spawned.
pub const ENV_FLAGS: &str = "HIVEGRID_WEB_FLAGS";
const DEFAULT_PROFILE: &str = "main";
const DEFAULT_URL: &str = "about:blank";
/// How the engine words a browser exit (`hivegrid_web`); the placeholder does not repeat it.
const CRASH_PREFIX: &str = "Browser engine exited";

/// Cell size assumed when a pane is created or resized without pixel dimensions.
const FALLBACK_CELL: (usize, usize) = (10, 20);
/// Consecutive presses within this time and distance (physical px) count as a multi-click.
const CLICK_INTERVAL: Duration = Duration::from_millis(400);
const CLICK_SLOP_PX: f32 = 4.0;
/// What the amount of a wheel event (`MouseButton::WheelUp(n)` and friends) counts. The window
/// backends disagree: X11 reports 5 "lines" per notch, Wayland the pointer axis value times the
/// window scale, i.e. pixels (about 10 per notch of a wheel, more for a compositor that reports
/// 15 [INFERENCE: not observed here]; a touchpad reports its finger movement). The GUI says which
/// it is (`set_wheel_units`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelUnits {
    Lines,
    Pixels,
}

/// CSS pixels a page scrolls per line of a `Lines` amount: 5 lines (an X11 notch) are 100 px.
const CSS_PX_PER_LINE: f32 = 20.0;
/// CSS pixels per unit of a `Pixels` amount, which is in device pixels: a Wayland notch of 10
/// scrolls 100 px. Touchpads, which send the amount of finger movement, scroll proportionally
/// fast; the window layer drops the discrete/smooth distinction that would tell them apart.
const CSS_PX_PER_WAYLAND_UNIT: f32 = 10.0;

static WHEEL_UNITS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Called by the GUI once its window connection exists.
pub fn set_wheel_units(units: WheelUnits) {
    WHEEL_UNITS.store(units as u8, Ordering::Relaxed);
}

fn wheel_units() -> WheelUnits {
    match WHEEL_UNITS.load(Ordering::Relaxed) {
        1 => WheelUnits::Pixels,
        _ => WheelUnits::Lines,
    }
}

/// The scroll in CSS pixels for a wheel `amount` in `units` at a window scale of `scale`
/// (`dpi / 96`). Chrome scrolls a CDP wheel delta 1:1 (checked at scales 1 to 2).
pub fn wheel_css_px(amount: usize, units: WheelUnits, scale: f32) -> f32 {
    match units {
        WheelUnits::Lines => amount as f32 * CSS_PX_PER_LINE,
        WheelUnits::Pixels => amount as f32 / scale.max(0.1) * CSS_PX_PER_WAYLAND_UNIT,
    }
}

static ENGINE: OnceLock<Arc<dyn WebEngine>> = OnceLock::new();

/// Called once by the GUI right after the `Mux` is created. A process that never calls it
/// (`wezterm-mux-server`, `wezterm cli`) refuses to spawn web panes.
pub fn enable(engine: Arc<dyn WebEngine>) {
    if ENGINE.set(engine).is_err() {
        log::debug!("web engine already enabled");
    }
}

/// The GUI is exiting: let the browsers quit orderly so cookies and logins reach their profiles
/// (see `WebEngine::shutdown`). A no-op in a process without an engine.
pub fn shutdown(timeout: Duration) {
    if let Some(engine) = ENGINE.get() {
        engine.shutdown(timeout);
    }
}

pub fn is_enabled() -> bool {
    ENGINE.get().is_some()
}

/// What a `hivegrid:web` spawn command asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSpawn {
    pub url: String,
    pub profile: String,
    pub private: bool,
    /// The browser binary the spawn command names itself, if it does.
    pub browser: Option<PathBuf>,
}

/// The spawn command for a web pane: what `wezterm cli split-pane --url` sends and what
/// Lua's `args = { 'hivegrid:web', url }` plus `set_environment_variables` spell out. `browser`
/// is the binary this pane is to use whatever the settings say (see [`browser_choice`]).
pub fn spawn_command(
    url: &str,
    profile: Option<&str>,
    private: bool,
    browser: Option<&Path>,
) -> CommandBuilder {
    let mut cmd = CommandBuilder::from_argv(vec![SPAWN_SENTINEL.into(), url.into()]);
    if let Some(profile) = profile {
        cmd.env(ENV_PROFILE, profile);
    }
    if private {
        cmd.env(ENV_PRIVATE, "1");
    }
    if let Some(browser) = browser {
        // The GUI's working directory is not the caller's: a relative path is made absolute here.
        let browser = std::path::absolute(browser).unwrap_or_else(|_| browser.to_path_buf());
        cmd.env(ENV_BROWSER, browser);
    }
    cmd
}

/// `Some` when `cmd` is a web pane spawn: `argv[0]` is [`SPAWN_SENTINEL`], `argv[1]` the URL
/// (default `about:blank`). The profile and private flag come from variables set on the command
/// itself; the environment of the GUI process does not count.
pub fn parse_spawn(cmd: &CommandBuilder) -> Option<WebSpawn> {
    let argv = cmd.get_argv();
    if argv.first()?.to_str()? != SPAWN_SENTINEL {
        return None;
    }
    let url = argv
        .get(1)
        .and_then(|u| u.to_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_URL);
    // Only the allowlisted schemes are opened; anything else (javascript:, chrome:, ...) becomes a
    // blank page rather than reaching the browser.
    let url = hivegrid_web::normalize_url(url).unwrap_or_else(|| {
        log::warn!("web pane: refused to open {url:?}, opening {DEFAULT_URL}");
        DEFAULT_URL.to_string()
    });
    let mut profile = DEFAULT_PROFILE.to_string();
    let mut private = false;
    let mut browser = None;
    for (k, v) in cmd.iter_extra_env_as_str() {
        if k == ENV_PROFILE && !v.trim().is_empty() {
            profile = v.trim().to_string();
        } else if k == ENV_PRIVATE {
            private = v == "1";
        } else if k == ENV_BROWSER && !v.trim().is_empty() {
            browser = Some(PathBuf::from(v.trim()));
        }
    }
    Some(WebSpawn {
        url,
        profile,
        private,
        browser,
    })
}

/// The browser binary to ask the engine for, or `None` to let it discover one. The rungs, highest
/// first: what the spawn command names (`--browser`, [`ENV_BROWSER`]), `HIVEGRID_CHROME` (the
/// engine reads it itself, so a set variable means `None` here), the `web.browser` setting
/// (`settings`, read only when no higher rung answers), then the engine's own search.
pub fn browser_choice(
    spawn: Option<&Path>,
    chrome_env: Option<&std::ffi::OsStr>,
    settings: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(p) = spawn {
        return Some(p.to_path_buf());
    }
    if chrome_env.is_some_and(|e| !e.is_empty()) {
        return None;
    }
    settings()
}

/// `web.browser` of the settings file (`$HIVEGRID_SETTINGS`, else
/// `~/.config/hivegrid/settings.json`, the file the settings panel writes): an absolute path, or
/// `None` for "auto", a missing or unreadable file and anything that is not a path.
fn settings_browser() -> Option<PathBuf> {
    let path = settings_file_in(std::env::var_os("HIVEGRID_SETTINGS"), &config::HOME_DIR);
    browser_from_settings(&std::fs::read_to_string(path).ok()?)
}

fn settings_file_in(over: Option<std::ffi::OsString>, home: &Path) -> PathBuf {
    match over.filter(|o| !o.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => home.join(".config").join("hivegrid").join("settings.json"),
    }
}

fn browser_from_settings(json: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let path = PathBuf::from(value.get("web")?.get("browser")?.as_str()?.trim());
    path.is_absolute().then_some(path)
}

/// The refusal for domains that cannot host a web pane (SSH, mux client): called first thing in
/// their spawn paths so the sentinel is never run as a program on a remote.
pub fn refuse_in_remote_domain(cmd: Option<&CommandBuilder>) -> Result<()> {
    if cmd.and_then(parse_spawn).is_some() {
        anyhow::bail!("web panes are only supported in the local domain");
    }
    Ok(())
}

/// The `Mux` side effects of a pane, behind a trait so the pane can be tested without a `Mux`.
pub trait WebHost: Send + Sync {
    fn notify(&self, notification: MuxNotification);
    /// `PaneOutput` for the pane, at most one in flight: `pending` is set while one is queued
    /// and cleared when the owner of the mux handles it. A page that makes its view chatter
    /// (frames, cursor reports) cannot flood the main thread with tasks.
    fn pane_output(&self, pane_id: PaneId, pending: &Arc<AtomicBool>);
    /// The page closed itself: let the mux drop the pane (it reports `is_dead`).
    fn prune(&self);
}

struct MuxHost;

impl WebHost for MuxHost {
    fn notify(&self, notification: MuxNotification) {
        Mux::notify_from_any_thread(notification);
    }

    fn pane_output(&self, pane_id: PaneId, pending: &Arc<AtomicBool>) {
        if pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let pending = Arc::clone(pending);
        promise::spawn::spawn_into_main_thread(async move {
            pending.store(false, Ordering::Release);
            if let Some(mux) = Mux::try_get() {
                mux.notify(MuxNotification::PaneOutput(pane_id));
            }
        })
        .detach();
    }

    fn prune(&self) {
        promise::spawn::spawn_into_main_thread(async {
            if let Some(mux) = Mux::try_get() {
                mux.prune_dead_windows();
            }
        })
        .detach();
    }
}

/// Create a web pane in the process-wide engine and register it with the mux.
pub fn spawn(domain_id: DomainId, size: TerminalSize, spawn: WebSpawn) -> Result<Arc<dyn Pane>> {
    let engine = ENGINE.get().ok_or_else(|| {
        anyhow!(
            "web panes need the Hivegrid GUI: this process has no web engine \
             (wezterm-mux-server and plain `wezterm` cannot host them)"
        )
    })?;
    let pane: Arc<dyn Pane> = Arc::new(WebPane::new(
        &**engine,
        Arc::new(MuxHost),
        alloc_pane_id(),
        domain_id,
        size,
        spawn,
    )?);
    Mux::get().add_pane(&pane)?;
    Ok(pane)
}

fn data_root() -> PathBuf {
    data_root_in(std::env::var_os("XDG_DATA_HOME"), &config::HOME_DIR)
}

/// `$XDG_DATA_HOME/hivegrid/web`; a relative `XDG_DATA_HOME` is ignored (the XDG spec says so,
/// and a profile must not depend on the GUI's working directory).
fn data_root_in(xdg: Option<std::ffi::OsString>, home: &std::path::Path) -> PathBuf {
    let base = match xdg.map(PathBuf::from) {
        Some(d) if d.is_absolute() => d,
        _ => home.join(".local").join("share"),
    };
    base.join("hivegrid").join("web")
}

/// A size with usable pixel dimensions and dpi.
fn normalize_size(mut size: TerminalSize) -> TerminalSize {
    size.rows = size.rows.max(1);
    size.cols = size.cols.max(1);
    if size.pixel_width == 0 {
        size.pixel_width = size.cols * FALLBACK_CELL.0;
    }
    if size.pixel_height == 0 {
        size.pixel_height = size.rows * FALLBACK_CELL.1;
    }
    if size.dpi == 0 {
        size.dpi = 96;
    }
    size
}

fn view_size(size: &TerminalSize) -> ViewSize {
    ViewSize {
        pixel_width: size.pixel_width as u32,
        pixel_height: size.pixel_height as u32,
        dpi: size.dpi,
    }
}

/// Physical pixel position inside the pane of a mouse event given in cells plus a pixel offset
/// inside the cell (the offset is 0..cell for cells past the first and may be negative or
/// larger on the first row/column; see `termwindow/mouseevent.rs`).
pub fn pane_pixel_position(size: &TerminalSize, event: &MouseEvent) -> (f32, f32) {
    let cell_w = (size.pixel_width / size.cols.max(1)).max(1);
    let cell_h = (size.pixel_height / size.rows.max(1)).max(1);
    let x = event.x * cell_w + event.x_pixel_offset.max(0) as usize;
    let y = event.y.max(0) as usize * cell_h + event.y_pixel_offset.max(0) as usize;
    (x as f32, y as f32)
}

fn css_scale(size: &TerminalSize) -> f32 {
    size.dpi.max(1) as f32 / 96.0
}

fn dom_button(button: MouseButton) -> Option<(u8, u8)> {
    // (DOM `button`, DOM `buttons` bit)
    match button {
        MouseButton::Left => Some((0, 1)),
        MouseButton::Right => Some((2, 2)),
        MouseButton::Middle => Some((1, 4)),
        _ => None,
    }
}

/// The three lines of the placeholder plus the state text, one `String` per row.
fn placeholder_lines(initial_url: &str, info: &PageInfo, state: &WebState) -> Vec<String> {
    let url = if info.url.is_empty() {
        initial_url
    } else {
        &info.url
    };
    // Both are bounded and free of control characters at the source (`PageInfo`), so a page
    // cannot make every paint copy and compare megabytes; the caps are enforced here as well.
    let mut lines = vec![
        url.chars().take(MAX_URL_CHARS).collect::<String>(),
        info.title.chars().take(MAX_TITLE_CHARS).collect::<String>(),
    ];
    match state {
        WebState::Starting => lines.push("starting browser…".to_string()),
        WebState::Running => {
            if info.loading {
                lines.push("loading…".to_string());
            }
        }
        WebState::Crashed { reason } => {
            // The instruction first: the reason can be long (it carries the last lines the
            // browser printed, joined with " | ") and must not push it off the pane.
            lines.push("Press Enter to restart the browser.".to_string());
            let mut pieces = reason.split('|').map(str::trim).filter(|l| !l.is_empty());
            let head = pieces.next().unwrap_or("unknown reason");
            lines.push(if head.starts_with(CRASH_PREFIX) {
                head.to_string()
            } else {
                format!("{CRASH_PREFIX}: {head}")
            });
            lines.extend(pieces.take(6).map(str::to_string));
        }
    }
    lines
}

/// `s` cut to `cols` terminal columns, ending in `…` when something was cut.
fn fit_columns(s: &str, cols: usize) -> String {
    if wezterm_term::unicode_column_width(s, None) <= cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = wezterm_term::unicode_column_width(c.encode_utf8(&mut [0; 4]), None);
        if width + w + 1 > cols {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// What the placeholder terminal shows now.
struct Placeholder {
    term: Terminal,
    shown: Option<Vec<String>>,
}

impl Placeholder {
    fn show(&mut self, lines: Vec<String>) {
        if self.shown.as_ref() == Some(&lines) {
            return;
        }
        // Home, erase display and scrollback, hide the cursor, no wrap marks.
        let mut text = String::from("\x1b[?25l\x1b[H\x1b[2J\x1b[3J");
        let size = self.term.get_size();
        let (rows, cols) = (size.rows, size.cols);
        // Rows used so far: a long line wraps onto several, and text past the last row would
        // scroll the first rows (the URL) away.
        let mut used = 0;
        for (i, line) in lines.iter().enumerate() {
            if used >= rows {
                break;
            }
            // Control characters in a page title must not become escape sequences.
            let line: String = line.chars().filter(|c| !c.is_control()).collect();
            // URL and title stay on one row each (a data: URL can be kilobytes), so the state
            // text below them is always visible.
            let room = if i < 2 { 1 } else { rows - used };
            let line = fit_columns(&line, room * cols);
            let height = (wezterm_term::unicode_column_width(&line, None).max(1) + cols - 1) / cols;
            if used > 0 {
                text.push_str("\r\n");
            }
            text.push_str(&line);
            used += height;
        }
        self.term.advance_bytes(text);
        self.shown = Some(lines);
    }
}

struct Pointer {
    /// DOM `buttons` bitmask of the buttons held.
    buttons: u8,
    last_press: Option<LastPress>,
}

struct LastPress {
    at: Instant,
    button: u8,
    pos: (f32, f32),
    count: u8,
}

/// What the view reports, turned into mux notifications. Lives in the view's callback.
struct Events {
    pane_id: PaneId,
    host: Arc<dyn WebHost>,
    closed: Arc<AtomicBool>,
    /// A `PaneOutput` is queued for the main thread (see `WebHost::pane_output`).
    output_pending: Arc<AtomicBool>,
}

impl Events {
    fn handle(&self, event: WebEvent) {
        let pane_id = self.pane_id;
        match event {
            WebEvent::Frame | WebEvent::State | WebEvent::Cursor(_) => {
                self.host.pane_output(pane_id, &self.output_pending);
            }
            WebEvent::Info => {
                // Title and URL changed: the tab and window titles follow `get_title`.
                self.host.notify(MuxNotification::Alert {
                    pane_id,
                    alert: Alert::CurrentWorkingDirectoryChanged,
                });
                self.host.pane_output(pane_id, &self.output_pending);
            }
            WebEvent::ClipboardText(text) => {
                self.host.notify(MuxNotification::AssignClipboard {
                    pane_id,
                    selection: ClipboardSelection::Clipboard,
                    clipboard: Some(text),
                });
            }
            WebEvent::Closed => {
                self.closed.store(true, Ordering::SeqCst);
                self.host.pane_output(pane_id, &self.output_pending);
                self.host.prune();
            }
        }
    }
}

/// `Pane::writer` of a web pane: bytes written to it are the control channel.
struct ControlWriter {
    pane_id: PaneId,
    view: Arc<dyn WebView>,
    host: Arc<dyn WebHost>,
    parser: ApcParser,
}

impl std::io::Write for ControlWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for command in self.parser.feed(buf) {
            log::debug!("web pane {}: {:?}", self.pane_id, command);
            if let Some(text) = command.apply(&*self.view) {
                self.host.notify(MuxNotification::AssignClipboard {
                    pane_id: self.pane_id,
                    selection: ClipboardSelection::Clipboard,
                    clipboard: Some(text),
                });
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct WebPane {
    pane_id: PaneId,
    domain_id: DomainId,
    view: Arc<dyn WebView>,
    host: Arc<dyn WebHost>,
    initial_url: String,
    /// The page closed itself (`window.close()`).
    closed: Arc<AtomicBool>,
    killed: AtomicBool,
    placeholder: Mutex<Placeholder>,
    writer: Mutex<ControlWriter>,
    pointer: Mutex<Pointer>,
}

impl WebPane {
    pub fn new(
        engine: &dyn WebEngine,
        host: Arc<dyn WebHost>,
        pane_id: PaneId,
        domain_id: DomainId,
        size: TerminalSize,
        spawn: WebSpawn,
    ) -> Result<Self> {
        let size = normalize_size(size);
        let closed = Arc::new(AtomicBool::new(false));
        let events = Events {
            pane_id,
            host: Arc::clone(&host),
            closed: Arc::clone(&closed),
            output_pending: Arc::new(AtomicBool::new(false)),
        };
        let cfg = WebConfig {
            chrome: browser_choice(
                spawn.browser.as_deref(),
                std::env::var_os("HIVEGRID_CHROME").as_deref(),
                settings_browser,
            ),
            profile: spawn.profile.clone(),
            private: spawn.private,
            data_root: data_root(),
            extra_flags: std::env::var(ENV_FLAGS)
                .map(|f| f.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
        };
        let view = engine.create_view(
            &spawn.url,
            &cfg,
            view_size(&size),
            Box::new(move |event| events.handle(event)),
        )?;

        let term = Terminal::new(
            size,
            Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(Vec::new()),
        );
        let writer = ControlWriter {
            pane_id,
            view: Arc::clone(&view),
            host: Arc::clone(&host),
            parser: ApcParser::new(),
        };
        Ok(Self {
            pane_id,
            domain_id,
            view,
            host,
            initial_url: sanitize_display(&spawn.url, MAX_URL_CHARS),
            closed,
            killed: AtomicBool::new(false),
            placeholder: Mutex::new(Placeholder { term, shown: None }),
            writer: Mutex::new(writer),
            pointer: Mutex::new(Pointer {
                buttons: 0,
                last_press: None,
            }),
        })
    }

    /// The latest frame of the page; `None` until the first one and while the browser is
    /// down: the cell renderer then draws the placeholder.
    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.view.take_frame()
    }

    pub fn frame_seq(&self) -> u64 {
        self.frame().map(|f| f.seq).unwrap_or(0)
    }

    /// Size in physical pixels the page is rendered at.
    pub fn view_pixel_size(&self) -> (u32, u32) {
        let size = self.size();
        (size.pixel_width as u32, size.pixel_height as u32)
    }

    pub fn set_visible(&self, visible: bool) {
        self.view.set_visible(visible);
    }

    pub fn state(&self) -> WebState {
        self.view.state()
    }

    fn size(&self) -> TerminalSize {
        self.placeholder.lock().term.get_size()
    }

    /// The placeholder terminal, brought up to date with the view.
    fn term(&self) -> MappedMutexGuard<'_, Terminal> {
        let lines = placeholder_lines(&self.initial_url, &self.view.info(), &self.view.state());
        let mut ph = self.placeholder.lock();
        ph.show(lines);
        MutexGuard::map(ph, |ph| &mut ph.term)
    }

    fn is_crashed(&self) -> bool {
        matches!(self.view.state(), WebState::Crashed { .. })
    }

    fn mouse_event_at(&self, event: MouseEvent, now: Instant) -> Result<()> {
        let size = self.size();
        let mods = hivegrid_web::keys::cdp_modifiers(event.modifiers);
        let (px, py) = pane_pixel_position(&size, &event);
        let scale = css_scale(&size);
        let (x, y) = (px / scale, py / scale);
        let mut pointer = self.pointer.lock();

        let mut web = WebMouse {
            kind: WebMouseKind::Move,
            x,
            y,
            button: 0,
            buttons: pointer.buttons,
            mods,
            click_count: 0,
            dx: 0.0,
            dy: 0.0,
        };

        // Wheel notches arrive as presses of a wheel "button".
        let (units, ratio) = (wheel_units(), css_scale(&size));
        let css = |n: usize| wheel_css_px(n, units, ratio);
        let wheel = match event.button {
            MouseButton::WheelUp(n) => Some((0.0, -css(n))),
            MouseButton::WheelDown(n) => Some((0.0, css(n))),
            MouseButton::WheelLeft(n) => Some((-css(n), 0.0)),
            MouseButton::WheelRight(n) => Some((css(n), 0.0)),
            _ => None,
        };
        if let Some((dx, dy)) = wheel {
            if event.kind == MouseEventKind::Press {
                web.kind = WebMouseKind::Wheel;
                web.dx = dx;
                web.dy = dy;
                self.view.mouse(web);
            }
            return Ok(());
        }

        match event.kind {
            MouseEventKind::Press => {
                let Some((button, bit)) = dom_button(event.button) else {
                    return Ok(());
                };
                let count = match &pointer.last_press {
                    Some(last)
                        if last.button == button
                            && now.saturating_duration_since(last.at) <= CLICK_INTERVAL
                            && (last.pos.0 - px).abs() <= CLICK_SLOP_PX
                            && (last.pos.1 - py).abs() <= CLICK_SLOP_PX =>
                    {
                        last.count % 3 + 1
                    }
                    _ => 1,
                };
                pointer.last_press = Some(LastPress {
                    at: now,
                    button,
                    pos: (px, py),
                    count,
                });
                pointer.buttons |= bit;
                web.kind = WebMouseKind::Down;
                web.button = button;
                web.buttons = pointer.buttons;
                web.click_count = count;
            }
            MouseEventKind::Release => {
                let Some((button, bit)) = dom_button(event.button) else {
                    return Ok(());
                };
                pointer.buttons &= !bit;
                web.kind = WebMouseKind::Up;
                web.button = button;
                web.buttons = pointer.buttons;
                web.click_count = pointer
                    .last_press
                    .as_ref()
                    .filter(|l| l.button == button)
                    .map(|l| l.count)
                    .unwrap_or(1);
            }
            MouseEventKind::Move => {
                // The GUI names the held button on a drag; trust it over our own state, which
                // misses a release that happened outside the window.
                web.buttons = match dom_button(event.button) {
                    Some((_, bit)) => pointer.buttons | bit,
                    None => pointer.buttons,
                };
            }
        }
        drop(pointer);
        self.view.mouse(web);
        Ok(())
    }

    fn is_copy_key(key: &KeyCode, mods: KeyModifiers) -> bool {
        mods == Modifiers::CTRL
            && matches!(
                key,
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('\u{3}') | KeyCode::Insert
            )
    }
}

impl Drop for WebPane {
    fn drop(&mut self) {
        // A pane dropped without `kill` (failed spawn, mux teardown) must not leave its page.
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.view.close();
        }
    }
}

impl Pane for WebPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        terminal_get_cursor_position(&mut self.term())
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.term().current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.term(), lines, seqno)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(&mut self.term(), lines, for_line);
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.term(), lines, with_lines)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        terminal_get_lines(&mut self.term(), lines)
    }

    /// The placeholder grid is never a source of links: the URL on its first row would otherwise
    /// turn the pointer into a hand over the top of the page and shadow the page's own cursor.
    fn apply_hyperlinks(&self, _lines: Range<StableRowIndex>, _rules: &[termwiz::hyperlink::Rule]) {
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        let mut dims = terminal_get_dimensions(&mut self.term());
        // The placeholder never has history; nothing to scroll.
        dims.scrollback_rows = dims.viewport_rows;
        dims
    }

    fn get_title(&self) -> String {
        let info = self.view.info();
        if info.title.is_empty() {
            if info.url.is_empty() {
                self.initial_url.clone()
            } else {
                sanitize_display(&info.url, MAX_URL_CHARS)
            }
        } else {
            sanitize_display(&info.title, MAX_TITLE_CHARS)
        }
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        true
    }

    fn send_paste(&self, text: &str) -> Result<()> {
        self.view.insert_text(text);
        Ok(())
    }

    fn send_composed_text(&self, text: &str) -> Result<()> {
        self.view.insert_text(text);
        Ok(())
    }

    fn reader(&self) -> Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer.lock(), |w| {
            let w: &mut dyn std::io::Write = w;
            w
        })
    }

    fn resize(&self, size: TerminalSize) -> Result<()> {
        let size = normalize_size(size);
        {
            let mut ph = self.placeholder.lock();
            if ph.term.get_size() == size {
                return Ok(());
            }
            ph.term.resize(size);
            // Reflowed text: draw it again at the next read.
            ph.shown = None;
        }
        self.view.resize(view_size(&size));
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        if self.is_crashed() {
            if key == KeyCode::Enter {
                self.view.restart();
            }
            return Ok(());
        }
        let mods = mods.remove_positional_mods();
        if Self::is_copy_key(&key, mods) {
            // The selection text comes back as `WebEvent::ClipboardText`.
            self.view.copy_selection();
        }
        self.view.key(WebKey {
            down: true,
            key,
            mods,
        });
        Ok(())
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        if self.is_crashed() {
            return Ok(());
        }
        self.view.key(WebKey {
            down: false,
            key,
            mods: mods.remove_positional_mods(),
        });
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent) -> Result<()> {
        self.mouse_event_at(event, Instant::now())
    }

    fn mouse_leave(&self) {
        self.view.mouse(WebMouse {
            kind: WebMouseKind::Move,
            x: -1.0,
            y: -1.0,
            button: 0,
            buttons: self.pointer.lock().buttons,
            mods: 0,
            click_count: 0,
            dx: 0.0,
            dy: 0.0,
        });
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        self.placeholder.lock().term.set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.placeholder.lock().term.get_config())
    }

    fn focus_changed(&self, focused: bool) {
        self.view.set_focused(focused);
    }

    fn kill(&self) {
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.view.close();
            self.host.notify(MuxNotification::PaneOutput(self.pane_id));
        }
    }

    fn is_dead(&self) -> bool {
        self.killed.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst)
    }

    fn palette(&self) -> ColorPalette {
        self.placeholder.lock().term.palette()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        HashMap::from([("hivegrid_web".to_string(), "1".to_string())])
    }

    fn erase_scrollback(&self, _erase_mode: ScrollbackEraseMode) {}

    fn is_mouse_grabbed(&self) -> bool {
        true
    }

    fn mouse_pointer_shape(&self) -> Option<String> {
        self.view.cursor()
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        let info = self.view.info();
        let url = if info.url.is_empty() {
            &self.initial_url
        } else {
            &info.url
        };
        Url::parse(url).ok().filter(|u| u.scheme() != "file")
    }

    fn tty_name(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests;
