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
use header::{
    layout, render_strip, strip_layout, Edit, EditKey, Header, Hit, Hover, StripHit, MAX_TABS,
    MIN_PANE_ROWS,
};
use history::History;
use hivegrid_web::{ApcCommand, HeaderOp, TabOp};

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
/// The search engine of a location that is no address when the settings do not say (Google).
const DEFAULT_SEARCH: &str = "https://www.google.com/search?q=%s";
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

/// `web.search` of the settings file: the URL template (with `%s`) typed words become. Google
/// when the file has no such key (or no file); `None` (no search) when the key is empty or is
/// not an http(s) template. The Lua and Python readers agree (`config/settings.lua`).
fn settings_search() -> Option<String> {
    let path = settings_file_in(std::env::var_os("HIVEGRID_SETTINGS"), &config::HOME_DIR);
    search_from_settings(std::fs::read_to_string(path).ok().as_deref())
}

fn search_from_settings(json: Option<&str>) -> Option<String> {
    let value = json.and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok());
    let stored = value.as_ref().and_then(|v| v.get("web")).and_then(|w| w.get("search"));
    match stored {
        None => Some(DEFAULT_SEARCH.to_string()),
        Some(s) => s
            .as_str()
            .filter(|t| t.contains("%s") && (t.starts_with("http://") || t.starts_with("https://")))
            .map(str::to_string),
    }
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
        Arc::clone(engine),
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

/// The page's share of a pane: the pane minus its header row.
fn view_size(size: &TerminalSize, header_rows: usize) -> ViewSize {
    let cell_h = size.pixel_height / size.rows.max(1);
    ViewSize {
        pixel_width: size.pixel_width as u32,
        pixel_height: size.pixel_height.saturating_sub(header_rows * cell_h).max(1) as u32,
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
    /// The header row and the lines below it as last written.
    shown: Option<(Vec<String>, Vec<String>)>,
}

impl Placeholder {
    fn show(&mut self, header: Vec<String>, lines: Vec<String>) {
        if let Some((h, l)) = &self.shown {
            if *h == header && *l == lines {
                return;
            }
        }
        // Home, erase display and scrollback, hide the cursor, no wrap marks.
        let mut text = String::from("\x1b[?25l\x1b[H\x1b[2J\x1b[3J");
        let size = self.term.get_size();
        let (rows, cols) = (size.rows, size.cols);
        // Rows used so far: a long line wraps onto several, and text past the last row would
        // scroll the first rows (the URL) away.
        let mut used = 0;
        for (i, row) in header.iter().take(rows).enumerate() {
            if i > 0 {
                text.push_str("\r\n");
            }
            text.push_str(row);
            used = i + 1;
        }
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
        self.shown = Some((header, lines));
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

/// What a view reports, turned into mux notifications. Lives in the view's callback.
struct Events {
    /// Weak: the control holds the views, and the views hold this callback.
    ctl: std::sync::Weak<HeaderCtl>,
    tab_id: u64,
    /// Where visited addresses go (`None`: a private pane), and the view to ask for them. The view
    /// is set once it exists; it holds this callback, so only weakly.
    recorder: Option<Recorder>,
}

struct Recorder {
    history: Arc<Mutex<History>>,
    view: Arc<OnceLock<std::sync::Weak<dyn WebView>>>,
    last: Mutex<String>,
}

impl Recorder {
    /// The page moved: remember its address once per change.
    fn visited(&self) {
        let Some(view) = self.view.get().and_then(|w| w.upgrade()) else { return };
        let url = view.info().url;
        let mut last = self.last.lock();
        if url.is_empty() || *last == url {
            return;
        }
        last.clone_from(&url);
        self.history.lock().record(&url, now_secs());
    }
}

impl Events {
    fn handle(&self, event: WebEvent) {
        if let Some(ctl) = self.ctl.upgrade() {
            ctl.on_event(self.tab_id, event, self.recorder.as_ref());
        }
    }
}

/// The history of a profile: its file next to the browser profile. Tests keep it in memory, so
/// they neither read nor write the real one.
#[cfg(not(test))]
fn load_history(profile: &str) -> History {
    let name = hivegrid_web::profile_dir_name(profile);
    History::load(data_root().join(format!("{name}.history.json")))
}

/// Rows of the tab strip for `count` tabs in a pane `pane_rows` tall: one when the pane has room
/// for it and there are two tabs or more, or the header is shown (so a lone tab carries the `+`).
/// A collapsed header leaves a single tab the bare page.
fn strip_rows_for(count: usize, header: &Header, pane_rows: usize) -> usize {
    usize::from(pane_rows > MIN_PANE_ROWS && (count >= 2 || header.shown))
}

#[cfg(test)]
fn load_history(_profile: &str) -> History {
    History::new()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One page of the pane.
struct Tab {
    id: u64,
    view: Arc<dyn WebView>,
    /// The address the tab was opened on, shown until the page reports its own.
    initial_url: String,
}

/// The tabs of a pane; never empty once the pane exists. Only the active tab is shown and has
/// the focus: the others are hidden (their pages stop rendering) and catch up when picked.
struct Tabs {
    list: Vec<Tab>,
    active: usize,
    next_id: u64,
    /// What the GUI last said about the pane; the active tab gets it.
    visible: bool,
    focused: bool,
}

impl Tabs {
    fn active(&self) -> &Tab {
        &self.list[self.active]
    }

    /// True for the active tab; also while the pane has no tab yet (its first page is being created).
    fn is_active(&self, id: u64) -> bool {
        self.list.get(self.active).map_or(true, |t| t.id == id)
    }

    /// Takes tab `idx` out (the list keeps at least one). The tab right of it takes its place, else
    /// the one on its left. Returns it, and the view now active when the active tab was the one
    /// taken.
    fn take(&mut self, idx: usize) -> (Tab, Option<Arc<dyn WebView>>) {
        let tab = self.list.remove(idx);
        let was_active = idx == self.active;
        if idx < self.active || (was_active && self.active >= self.list.len()) {
            self.active -= 1;
        }
        (tab, was_active.then(|| Arc::clone(&self.active().view)))
    }
}

/// What a tab says about itself in the strip: the title, else the host of the address, else
/// "New tab" for a blank page.
fn tab_title(info: &PageInfo, initial_url: &str) -> String {
    if !info.title.is_empty() {
        return info.title.clone();
    }
    let url = if info.url.is_empty() { initial_url } else { &info.url };
    if url.is_empty() || url == DEFAULT_URL {
        return "New tab".to_string();
    }
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// What the header row, the tabs and the control channel share with the pane: the header state,
/// the pages, and the ways they change the views and the placeholder. No lock is held across
/// another, except the header's across the tabs' (never the other way round).
struct HeaderCtl {
    pane_id: PaneId,
    engine: Arc<dyn WebEngine>,
    /// The browser, profile and privacy every tab of the pane opens with.
    cfg: WebConfig,
    host: Arc<dyn WebHost>,
    output_pending: Arc<AtomicBool>,
    placeholder: Arc<Mutex<Placeholder>>,
    header: Mutex<Header>,
    /// The addresses of the profile, for the field's completion; `None` in a private pane, which
    /// remembers nothing.
    history: Option<Arc<Mutex<History>>>,
    tabs: Mutex<Tabs>,
    /// The last page closed itself (`window.close()`).
    closed: AtomicBool,
}

impl HeaderCtl {
    fn pane_size(&self) -> TerminalSize {
        self.placeholder.lock().term.get_size()
    }

    /// The active tab's page.
    fn view(&self) -> Arc<dyn WebView> {
        Arc::clone(&self.tabs.lock().active().view)
    }

    /// The active tab's page and the address it was opened on.
    fn active_tab(&self) -> (Arc<dyn WebView>, String) {
        let t = self.tabs.lock();
        (Arc::clone(&t.active().view), t.active().initial_url.clone())
    }

    fn initial_url(&self) -> String {
        self.tabs.lock().active().initial_url.clone()
    }

    /// Rows of the tab strip in a pane `pane_rows` tall: one when the pane has room for it and
    /// there are two tabs or more, or the header is shown (a lone tab then carries the `+`).
    fn strip_rows(&self, pane_rows: usize) -> usize {
        let count = self.tabs.lock().list.len();
        strip_rows_for(count, &self.header.lock(), pane_rows)
    }

    /// Rows at the top of the pane the page does not cover: the tab strip, the header row (0 or
    /// 1) and the dropdown of the field.
    fn rows(&self) -> usize {
        let rows = self.pane_size().rows;
        let strip = self.strip_rows(rows);
        strip + self.header.lock().top_rows(rows - strip)
    }

    fn redraw(&self) {
        self.host.pane_output(self.pane_id, &self.output_pending);
    }

    /// Applies `f` to the header. When that adds or removes the row, the page is resized to what
    /// is left of the pane. The pane is drawn again either way.
    fn change<R>(&self, f: impl FnOnce(&mut Header) -> R) -> R {
        let size = self.pane_size();
        let count = self.tabs.lock().list.len();
        let (result, before, after) = {
            let mut h = self.header.lock();
            let rows = |h: &Header| {
                let strip = strip_rows_for(count, h, size.rows);
                strip + h.top_rows(size.rows - strip)
            };
            let before = rows(&h);
            let result = f(&mut h);
            (result, before, rows(&h))
        };
        if before != after {
            self.placeholder.lock().shown = None;
            self.view().resize(view_size(&size, after));
        }
        self.redraw();
        result
    }

    fn apply(&self, op: HeaderOp) {
        self.change(|h| {
            h.shown = match op {
                HeaderOp::Toggle => !h.shown,
                HeaderOp::Show => true,
                HeaderOp::Hide => false,
            };
            if !h.shown {
                h.edit = None;
                h.error = None;
            }
        });
    }

    fn is_editing(&self) -> bool {
        self.header.lock().edit.is_some()
    }

    /// Start typing in the address field, the page's address selected. A hidden header is shown
    /// for the edit only.
    fn begin_edit(&self) {
        let url = self.view().info().url;
        self.change(|h| {
            if h.edit.is_none() {
                h.edit = Some(Edit::new(if url == "about:blank" { "" } else { &url }));
                h.error = None;
            }
        });
    }

    fn cancel_edit(&self) {
        if self.is_editing() {
            self.change(|h| {
                h.edit = None;
                h.error = None;
            });
        }
    }

    fn edit_key(&self, key: EditKey) {
        self.change(|h| {
            h.error = None;
            if let Some(e) = &mut h.edit {
                e.key(key);
            }
        });
    }

    /// Typed (`suggest`) or pasted text into the field. Typing at the end of the text offers the
    /// history's completion; a paste never does, so it cannot be redirected by one.
    fn edit_insert(&self, text: &str, suggest: bool) {
        let offer = |typed: &str| match &self.history {
            Some(history) => history.lock().suggest(typed, now_secs(), header::DROPDOWN_MAX),
            None => vec![],
        };
        self.change(|h| {
            h.error = None;
            if let Some(e) = &mut h.edit {
                e.insert(text);
                let found = if suggest && e.at_end() { offer(&e.text()) } else { vec![] };
                e.set_suggestions(found);
            }
        });
    }

    /// Enter in the field: open the address, or search for the words. A location that cannot
    /// be opened stays in the field, with the reason in its place until the next key.
    fn commit(&self) {
        let (text, suggested) = match &self.header.lock().edit {
            Some(e) => (e.text(), e.suggested_url().map(str::to_string)),
            None => return,
        };
        if text.trim().is_empty() {
            self.cancel_edit();
            return;
        }
        // Enter on a completion opens what it stands for.
        if let Some(url) = suggested {
            self.view().navigate(&url);
            self.cancel_edit();
            return;
        }
        match hivegrid_web::resolve_location(&text, settings_search().as_deref()) {
            Ok(url) => {
                self.view().navigate(&url);
                self.cancel_edit();
            }
            Err(why) => self.change(|h| h.error = Some(why)),
        }
    }

    fn set_hover(&self, hover: Hover) {
        {
            let mut h = self.header.lock();
            if h.hover == hover {
                return;
            }
            h.hover = hover;
        }
        self.redraw();
    }

    /// A press on a part of the header row.
    fn click(&self, hit: Option<Hit>) {
        if hit != Some(Hit::Field) {
            self.cancel_edit();
        }
        let info = self.view().info();
        match hit {
            Some(Hit::Back) if info.can_go_back => self.view().back(),
            Some(Hit::Forward) if info.can_go_forward => self.view().forward(),
            Some(Hit::Reload) if info.loading => self.view().stop(),
            Some(Hit::Reload) => self.view().reload(false),
            Some(Hit::Field) => self.begin_edit(),
            Some(Hit::Collapse) => self.apply(HeaderOp::Hide),
            _ => {}
        }
    }

    // Tabs ------------------------------------------------------------------------------------

    /// Creates the page of a tab; it is not part of the pane until the caller adds it.
    fn create_tab(self: &Arc<Self>, id: u64, url: &str, size: hivegrid_web::ViewSize) -> Result<Tab> {
        let view_slot: Arc<OnceLock<std::sync::Weak<dyn WebView>>> = Arc::new(OnceLock::new());
        let events = Events {
            ctl: Arc::downgrade(self),
            tab_id: id,
            recorder: self.history.as_ref().map(|h| Recorder {
                history: Arc::clone(h),
                view: Arc::clone(&view_slot),
                last: Mutex::new(String::new()),
            }),
        };
        let view = self
            .engine
            .create_view(url, &self.cfg, size, Box::new(move |event| events.handle(event)))?;
        let _ = view_slot.set(Arc::downgrade(&view));
        Ok(Tab { id, view, initial_url: sanitize_display(url, MAX_URL_CHARS) })
    }

    /// The titles of the tabs in the strip and the active index.
    fn tab_titles(&self) -> (Vec<String>, usize) {
        let (tabs, active) = {
            let t = self.tabs.lock();
            let tabs: Vec<_> = t.list.iter().map(|t| (Arc::clone(&t.view), t.initial_url.clone())).collect();
            (tabs, t.active)
        };
        (tabs.iter().map(|(v, initial)| tab_title(&v.info(), initial)).collect(), active)
    }

    /// After the tabs changed: `before` is the rows the page gave up before. The pane is drawn
    /// again; a tab that became active (`activated`) gets the size, visibility and focus of the
    /// pane, and the tab and window titles follow it.
    fn after_tab_change(&self, before: usize, activated: Option<Arc<dyn WebView>>) {
        let size = self.pane_size();
        let after = self.rows();
        if before != after || activated.is_some() {
            self.placeholder.lock().shown = None;
        }
        let (visible, focused) = {
            let t = self.tabs.lock();
            (t.visible, t.focused)
        };
        match activated {
            Some(view) => {
                view.resize(view_size(&size, after));
                view.set_visible(visible);
                view.set_focused(focused);
            }
            None if before != after => self.view().resize(view_size(&size, after)),
            None => {}
        }
        self.host.notify(MuxNotification::Alert {
            pane_id: self.pane_id,
            alert: Alert::CurrentWorkingDirectoryChanged,
        });
        self.redraw();
    }

    /// A new tab on `url` right of the active one, which it makes active. False when the pane
    /// holds `MAX_TABS` already or the page could not be created.
    fn open_tab(self: &Arc<Self>, url: &str) -> bool {
        self.cancel_edit();
        let before = self.rows();
        let (id, count) = {
            let mut t = self.tabs.lock();
            if t.list.len() >= MAX_TABS {
                return false;
            }
            t.next_id += 1;
            (t.next_id - 1, t.list.len())
        };
        let size = self.pane_size();
        // The strip is there for the new tab: the page starts at the size it will have.
        let strip = usize::from(size.rows > MIN_PANE_ROWS);
        let top = strip + self.header.lock().top_rows(size.rows - strip);
        let tab = match self.create_tab(id, url, view_size(&size, top)) {
            Ok(tab) => tab,
            Err(e) => {
                log::warn!("web pane {}: cannot open a tab: {e:#}", self.pane_id);
                return false;
            }
        };
        let view = Arc::clone(&tab.view);
        let old = {
            let mut t = self.tabs.lock();
            let old = Arc::clone(&t.active().view);
            let at = t.active + 1;
            t.list.insert(at, tab);
            t.active = at;
            old
        };
        old.set_visible(false);
        old.set_focused(false);
        self.after_tab_change(before, Some(view));
        true
    }

    fn switch_to(&self, idx: usize) {
        self.cancel_edit();
        let before = self.rows();
        let (old, new) = {
            let mut t = self.tabs.lock();
            if idx >= t.list.len() || idx == t.active {
                return;
            }
            let old = Arc::clone(&t.active().view);
            t.active = idx;
            (old, Arc::clone(&t.active().view))
        };
        old.set_visible(false);
        old.set_focused(false);
        self.after_tab_change(before, Some(new));
    }

    /// The tab `delta` places from the active one, wrapping around.
    fn switch_by(&self, delta: isize) {
        let (count, active) = {
            let t = self.tabs.lock();
            (t.list.len() as isize, t.active as isize)
        };
        if count >= 2 {
            self.switch_to((active + delta).rem_euclid(count) as usize);
        }
    }

    /// Closes tab `idx` and its page. The last tab stays: closing the pane is the pane's own
    /// business.
    fn close_tab(&self, idx: usize) {
        if self.tabs.lock().list.len() < 2 {
            return;
        }
        self.cancel_edit();
        let before = self.rows();
        let taken = {
            let mut t = self.tabs.lock();
            if t.list.len() < 2 || idx >= t.list.len() {
                return;
            }
            t.take(idx)
        };
        taken.0.view.close();
        self.after_tab_change(before, taken.1);
    }

    /// A page closed itself (`window.close()`): its tab goes; the last page takes the pane with it.
    fn tab_gone(&self, id: u64) {
        let was_active = {
            let t = self.tabs.lock();
            if !t.list.iter().any(|tab| tab.id == id) {
                return;
            }
            t.active().id == id
        };
        if was_active {
            self.cancel_edit();
        }
        let before = self.rows();
        let outcome = {
            let mut t = self.tabs.lock();
            let Some(idx) = t.list.iter().position(|tab| tab.id == id) else { return };
            if t.list.len() == 1 {
                None
            } else {
                Some(t.take(idx).1)
            }
        };
        match outcome {
            Some(activated) => self.after_tab_change(before, activated),
            None => {
                self.closed.store(true, Ordering::SeqCst);
                self.redraw();
                self.host.prune();
            }
        }
    }

    /// Closes every page: the pane is going away.
    fn close_all(&self) {
        let views: Vec<_> = self.tabs.lock().list.iter().map(|t| Arc::clone(&t.view)).collect();
        for view in views {
            view.close();
        }
    }

    /// What the control channel, the keys and the strip ask of the tabs.
    fn tab_op(self: &Arc<Self>, op: TabOp) {
        match op {
            TabOp::New(url) => {
                let edit = url.is_none();
                let url = match url {
                    None => DEFAULT_URL.to_string(),
                    Some(u) => match hivegrid_web::normalize_url(&u) {
                        Some(u) => u,
                        None => {
                            log::warn!("web pane {}: refused to open {u:?} in a tab", self.pane_id);
                            return;
                        }
                    },
                };
                if self.open_tab(&url) && edit {
                    self.begin_edit();
                }
            }
            TabOp::Close => {
                let active = self.tabs.lock().active;
                self.close_tab(active);
            }
            TabOp::Next => self.switch_by(1),
            TabOp::Prev => self.switch_by(-1),
            TabOp::Select(n) => self.switch_to(n - 1),
        }
    }

    /// An event of tab `tab_id`'s page. Only the active tab draws the pane; every tab can change
    /// its title in the strip.
    fn on_event(&self, tab_id: u64, event: WebEvent, recorder: Option<&Recorder>) {
        let active = self.tabs.lock().is_active(tab_id);
        match event {
            WebEvent::Frame | WebEvent::Cursor(_) => {
                if active {
                    self.redraw();
                }
            }
            WebEvent::State => self.redraw(),
            WebEvent::Info => {
                if let Some(r) = recorder {
                    r.visited();
                }
                if active {
                    // Title and URL changed: the tab and window titles follow `get_title`.
                    self.host.notify(MuxNotification::Alert {
                        pane_id: self.pane_id,
                        alert: Alert::CurrentWorkingDirectoryChanged,
                    });
                }
                self.redraw();
            }
            WebEvent::ClipboardText(text) => {
                self.host.notify(MuxNotification::AssignClipboard {
                    pane_id: self.pane_id,
                    selection: ClipboardSelection::Clipboard,
                    clipboard: Some(text),
                });
            }
            WebEvent::Closed => self.tab_gone(tab_id),
        }
    }
}

/// `Pane::writer` of a web pane: bytes written to it are the control channel.
struct ControlWriter {
    pane_id: PaneId,
    host: Arc<dyn WebHost>,
    ctl: Arc<HeaderCtl>,
    parser: ApcParser,
}

impl std::io::Write for ControlWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for command in self.parser.feed(buf) {
            log::debug!("web pane {}: {:?}", self.pane_id, command);
            match &command {
                ApcCommand::Header(op) => {
                    self.ctl.apply(*op);
                    continue;
                }
                ApcCommand::Location => {
                    self.ctl.begin_edit();
                    continue;
                }
                ApcCommand::Tab(op) => {
                    self.ctl.tab_op(op.clone());
                    continue;
                }
                _ => {}
            }
            if let Some(text) = command.apply(&*self.ctl.view()) {
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
    host: Arc<dyn WebHost>,
    killed: AtomicBool,
    placeholder: Arc<Mutex<Placeholder>>,
    /// The header row and the tabs (state, editing) and what they do to the views.
    ctl: Arc<HeaderCtl>,
    writer: Mutex<ControlWriter>,
    pointer: Mutex<Pointer>,
}

impl WebPane {
    pub fn new(
        engine: Arc<dyn WebEngine>,
        host: Arc<dyn WebHost>,
        pane_id: PaneId,
        domain_id: DomainId,
        size: TerminalSize,
        spawn: WebSpawn,
    ) -> Result<Self> {
        let size = normalize_size(size);
        let history = (!spawn.private).then(|| Arc::new(Mutex::new(load_history(&spawn.profile))));
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
        let term = Terminal::new(
            size,
            Arc::new(config::TermConfig::new()),
            "WezTerm",
            config::wezterm_version(),
            Box::new(Vec::new()),
        );
        let placeholder = Arc::new(Mutex::new(Placeholder { term, shown: None }));
        let ctl = Arc::new(HeaderCtl {
            pane_id,
            engine,
            cfg,
            host: Arc::clone(&host),
            output_pending: Arc::new(AtomicBool::new(false)),
            placeholder: Arc::clone(&placeholder),
            header: Mutex::new(Header::new()),
            history,
            tabs: Mutex::new(Tabs { list: vec![], active: 0, next_id: 1, visible: true, focused: false }),
            closed: AtomicBool::new(false),
        });
        let header = Header::new();
        let strip = strip_rows_for(1, &header, size.rows);
        let first = ctl.create_tab(0, &spawn.url, view_size(&size, strip + header.top_rows(size.rows - strip)))?;
        ctl.tabs.lock().list.push(first);
        let writer = ControlWriter {
            pane_id,
            host: Arc::clone(&host),
            ctl: Arc::clone(&ctl),
            parser: ApcParser::new(),
        };
        Ok(Self {
            pane_id,
            domain_id,
            host,
            killed: AtomicBool::new(false),
            placeholder,
            ctl,
            writer: Mutex::new(writer),
            pointer: Mutex::new(Pointer {
                buttons: 0,
                last_press: None,
            }),
        })
    }

    /// The active tab's page.
    fn view(&self) -> Arc<dyn WebView> {
        self.ctl.view()
    }

    /// The latest frame of the page; `None` until the first one and while the browser is
    /// down: the cell renderer then draws the placeholder.
    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.view().take_frame()
    }

    pub fn frame_seq(&self) -> u64 {
        self.frame().map(|f| f.seq).unwrap_or(0)
    }

    /// Size in physical pixels the page is rendered at: the pane without its header row.
    pub fn view_pixel_size(&self) -> (u32, u32) {
        let v = view_size(&self.size(), self.ctl.rows());
        (v.pixel_width, v.pixel_height)
    }

    /// Rows at the top of the pane the page does not cover: the header row while it is shown, and
    /// the dropdown of candidates under the field. The renderer starts the page texture below.
    pub fn header_rows(&self) -> usize {
        self.ctl.rows()
    }

    pub fn set_visible(&self, visible: bool) {
        let view = {
            let mut t = self.ctl.tabs.lock();
            t.visible = visible;
            Arc::clone(&t.active().view)
        };
        view.set_visible(visible);
    }

    pub fn state(&self) -> WebState {
        self.view().state()
    }

    fn size(&self) -> TerminalSize {
        self.placeholder.lock().term.get_size()
    }

    /// The placeholder terminal, brought up to date with the view.
    fn term(&self) -> MappedMutexGuard<'_, Terminal> {
        let (titles, active) = self.ctl.tab_titles();
        let (view, initial_url) = self.ctl.active_tab();
        let info = view.info();
        let size = self.size();
        let header = {
            let h = self.ctl.header.lock();
            let strip = strip_rows_for(titles.len(), &h, size.rows);
            let mut rows = vec![];
            if strip == 1 {
                rows.push(render_strip(size.cols, &titles, active, h.hover.strip));
            }
            let room = size.rows - strip;
            if h.rows(room) == 1 {
                rows.push(h.render(size.cols, &info));
                rows.extend(h.render_dropdown(size.cols, room));
            }
            rows
        };
        let lines = placeholder_lines(&initial_url, &info, &view.state());
        let mut ph = self.placeholder.lock();
        ph.show(header, lines);
        MutexGuard::map(ph, |ph| &mut ph.term)
    }

    fn is_crashed(&self) -> bool {
        matches!(self.view().state(), WebState::Crashed { .. })
    }

    fn mouse_event_at(&self, event: MouseEvent, now: Instant) -> Result<()> {
        let size = self.size();
        let mods = hivegrid_web::keys::cdp_modifiers(event.modifiers);
        let header_rows = self.ctl.rows();
        let mut pointer = self.pointer.lock();
        // The header row and its dropdown take the pointer, unless a press that began in the page
        // is still held (a drag may leave the page).
        if (event.y as usize) < header_rows && event.y >= 0 && pointer.buttons == 0 {
            drop(pointer);
            self.header_mouse(&size, &event);
            return Ok(());
        }
        self.ctl.set_hover(Hover::default());
        let (px, py) = pane_pixel_position(&size, &event);
        let header_px = header_rows * (size.pixel_height / size.rows.max(1)).max(1);
        let py = (py - header_px as f32).max(0.0);
        let scale = css_scale(&size);
        let (x, y) = (px / scale, py / scale);

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
                self.view().mouse(web);
            }
            return Ok(());
        }

        match event.kind {
            MouseEventKind::Press => {
                let Some((button, bit)) = dom_button(event.button) else {
                    return Ok(());
                };
                self.ctl.cancel_edit();
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
        self.view().mouse(web);
        Ok(())
    }

    /// A pointer event on a candidate of the dropdown: hovering selects it, a click opens it.
    fn dropdown_mouse(&self, event: &MouseEvent) {
        let i = event.y as usize - 1;
        match event.kind {
            MouseEventKind::Move => {
                self.ctl.set_hover(Hover { in_header: true, ..Hover::default() });
                self.ctl.change(|h| {
                    if let Some(e) = &mut h.edit {
                        e.select(i);
                    }
                });
            }
            MouseEventKind::Press if matches!(event.button, MouseButton::Left) => {
                self.ctl.change(|h| {
                    if let Some(e) = &mut h.edit {
                        e.select(i);
                    }
                });
                self.ctl.commit();
            }
            _ => {}
        }
    }

    /// A pointer event on the tab strip: a click picks a tab or opens a new one, the close button
    /// or a middle click closes one.
    fn strip_mouse(&self, size: &TerminalSize, event: &MouseEvent) {
        let (count, active) = {
            let t = self.ctl.tabs.lock();
            (t.list.len(), t.active)
        };
        let hit = strip_layout(size.cols, count, active).hit(event.x);
        match event.kind {
            MouseEventKind::Move => self.ctl.set_hover(Hover { in_header: true, strip: hit, ..Hover::default() }),
            MouseEventKind::Press => {
                match (event.button, hit) {
                    (MouseButton::Left, Some(StripHit::Tab(i))) => self.ctl.switch_to(i),
                    (MouseButton::Left, Some(StripHit::Close(i)))
                    | (MouseButton::Middle, Some(StripHit::Tab(i) | StripHit::Close(i))) => self.ctl.close_tab(i),
                    (MouseButton::Left, Some(StripHit::New)) => self.ctl.tab_op(TabOp::New(None)),
                    _ => {}
                }
                self.ctl.set_hover(Hover::default());
            }
            _ => {}
        }
    }

    /// A pointer event on the tab strip or the header row (and its dropdown).
    fn header_mouse(&self, size: &TerminalSize, event: &MouseEvent) {
        let strip = self.ctl.strip_rows(size.rows);
        if (event.y as usize) < strip {
            self.strip_mouse(size, event);
            return;
        }
        let mut event = *event;
        event.y -= strip as i64;
        if event.y > 0 {
            self.dropdown_mouse(&event);
            return;
        }
        let hit = layout(size.cols).hit(event.x);
        match event.kind {
            MouseEventKind::Move => self.ctl.set_hover(Hover { in_header: true, hit, ..Hover::default() }),
            MouseEventKind::Press if matches!(event.button, MouseButton::Left) => {
                self.ctl.click(hit)
            }
            _ => {}
        }
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
        // A pane dropped without `kill` (failed spawn, mux teardown) must not leave its pages.
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.ctl.close_all();
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
        let info = self.view().info();
        if info.title.is_empty() {
            if info.url.is_empty() {
                self.ctl.initial_url()
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
        if self.ctl.is_editing() {
            self.ctl.edit_insert(text, false);
        } else {
            self.view().insert_text(text);
        }
        Ok(())
    }

    fn send_composed_text(&self, text: &str) -> Result<()> {
        if self.ctl.is_editing() {
            self.ctl.edit_insert(text, true);
        } else {
            self.view().insert_text(text);
        }
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
        self.view().resize(view_size(&size, self.ctl.rows()));
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        let mods = mods.remove_positional_mods();
        // The address field takes every key while it is being typed in.
        if self.ctl.is_editing() {
            match field_key(&key, mods) {
                FieldKey::Commit => self.ctl.commit(),
                FieldKey::Cancel => self.ctl.cancel_edit(),
                FieldKey::Edit(k) => self.ctl.edit_key(k),
                FieldKey::Text(s) => self.ctl.edit_insert(&s, true),
                FieldKey::Ignore => {}
            }
            return Ok(());
        }
        // The tab keys of a browser.
        if let Some(op) = tab_key(&key, mods) {
            self.ctl.tab_op(op);
            return Ok(());
        }
        if self.is_crashed() {
            if key == KeyCode::Enter {
                self.view().restart();
            }
            return Ok(());
        }
        if is_location_key(&key, mods) {
            self.ctl.begin_edit();
            return Ok(());
        }
        if Self::is_copy_key(&key, mods) {
            // The selection text comes back as `WebEvent::ClipboardText`.
            self.view().copy_selection();
        }
        self.view().key(WebKey {
            down: true,
            key,
            mods,
        });
        Ok(())
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> Result<()> {
        if self.is_crashed() || self.ctl.is_editing() || tab_key(&key, mods.remove_positional_mods()).is_some() {
            return Ok(());
        }
        self.view().key(WebKey {
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
        self.ctl.set_hover(Hover::default());
        self.view().mouse(WebMouse {
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
        let view = {
            let mut t = self.ctl.tabs.lock();
            t.focused = focused;
            Arc::clone(&t.active().view)
        };
        view.set_focused(focused);
    }

    fn kill(&self) {
        if !self.killed.swap(true, Ordering::SeqCst) {
            self.ctl.close_all();
            self.host.notify(MuxNotification::PaneOutput(self.pane_id));
        }
    }

    fn is_dead(&self) -> bool {
        self.killed.load(Ordering::SeqCst) || self.ctl.closed.load(Ordering::SeqCst)
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
        let hover = self.ctl.header.lock().hover;
        if hover.in_header {
            let shape = match hover.hit {
                _ if hover.strip.is_some() => "pointer",
                Some(Hit::Field) => "text",
                Some(_) => "pointer",
                None => "default",
            };
            return Some(shape.to_string());
        }
        self.view().cursor()
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        let (view, initial_url) = self.ctl.active_tab();
        let info = view.info();
        let url = if info.url.is_empty() {
            &initial_url
        } else {
            &info.url
        };
        Url::parse(url).ok().filter(|u| u.scheme() != "file")
    }

    fn tty_name(&self) -> Option<String> {
        None
    }
}

/// Ctrl+L: type in the address field, as in a browser.
fn is_location_key(key: &KeyCode, mods: KeyModifiers) -> bool {
    mods == Modifiers::CTRL && matches!(key, KeyCode::Char('l') | KeyCode::Char('L') | KeyCode::Char('\u{c}'))
}

/// The tab keys of a browser: Ctrl+T new, Ctrl+W close, Ctrl+Tab / Ctrl+PageDown next,
/// Ctrl+Shift+Tab / Ctrl+PageUp previous, Ctrl+1 to Ctrl+9 pick.
fn tab_key(key: &KeyCode, mods: KeyModifiers) -> Option<TabOp> {
    if mods == Modifiers::CTRL | Modifiers::SHIFT {
        return matches!(key, KeyCode::Tab).then_some(TabOp::Prev);
    }
    if mods != Modifiers::CTRL {
        return None;
    }
    match key {
        KeyCode::Char('t') | KeyCode::Char('T') | KeyCode::Char('\u{14}') => Some(TabOp::New(None)),
        KeyCode::Char('w') | KeyCode::Char('W') | KeyCode::Char('\u{17}') => Some(TabOp::Close),
        KeyCode::Tab | KeyCode::PageDown => Some(TabOp::Next),
        KeyCode::PageUp => Some(TabOp::Prev),
        KeyCode::Char(c @ '1'..='9') => Some(TabOp::Select(*c as usize - '0' as usize)),
        _ => None,
    }
}

/// What a key does in the address field.
#[derive(Debug, PartialEq, Eq)]
enum FieldKey {
    Commit,
    Cancel,
    Edit(EditKey),
    Text(String),
    /// Swallowed: a key that is neither text nor an editing command never reaches the page.
    Ignore,
}

fn field_key(key: &KeyCode, mods: KeyModifiers) -> FieldKey {
    let ctrl = mods.contains(Modifiers::CTRL);
    match key {
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => FieldKey::Commit,
        KeyCode::Escape | KeyCode::Char('\u{1b}') => FieldKey::Cancel,
        KeyCode::Backspace | KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => {
            FieldKey::Edit(if ctrl { EditKey::DeleteWord } else { EditKey::Backspace })
        }
        KeyCode::Delete => FieldKey::Edit(EditKey::Delete),
        KeyCode::LeftArrow => FieldKey::Edit(EditKey::Left),
        KeyCode::RightArrow => FieldKey::Edit(EditKey::Right),
        KeyCode::Home => FieldKey::Edit(EditKey::Home),
        KeyCode::End => FieldKey::Edit(EditKey::End),
        KeyCode::Char(c) => {
            // A control character is Ctrl plus a letter (Ctrl+A is U+0001).
            let letter = match *c {
                c if ('\u{1}'..='\u{1a}').contains(&c) => Some((c as u8 - 1 + b'a') as char),
                c if ctrl => Some(c.to_ascii_lowercase()),
                _ => None,
            };
            match letter {
                Some('a') => FieldKey::Edit(EditKey::Home),
                Some('e') => FieldKey::Edit(EditKey::End),
                Some('u') => FieldKey::Edit(EditKey::KillToStart),
                Some('k') => FieldKey::Edit(EditKey::KillToEnd),
                Some('w') => FieldKey::Edit(EditKey::DeleteWord),
                Some('l') => FieldKey::Edit(EditKey::SelectAll),
                Some(_) => FieldKey::Ignore,
                None if mods.intersects(Modifiers::ALT | Modifiers::SUPER) => FieldKey::Ignore,
                None => FieldKey::Text(c.to_string()),
            }
        }
        KeyCode::Tab => FieldKey::Edit(EditKey::Accept),
        KeyCode::DownArrow => FieldKey::Edit(EditKey::Next),
        KeyCode::UpArrow => FieldKey::Edit(EditKey::Prev),
        _ => FieldKey::Ignore,
    }
}

mod header;
mod history;

#[cfg(test)]
mod tests;
