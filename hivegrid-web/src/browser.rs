//! Chrome discovery, launch and supervision, and the per-process `Browser` that views share.

use crate::cdp::{CdpError, Connection, Event};
use crate::view::ViewCore;
use crate::WebConfig;
use anyhow::{anyhow, bail, Context, Result};
use parking_lot::{Condvar, Mutex};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// How long Chrome gets to answer the first command.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a graceful `Browser.close` may take before the process group is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

const FIXED_CHROME_PATHS: &[&str] = &["/opt/google/chrome/chrome"];
const CHROME_NAMES: &[&str] = &["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"];

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Find a Chrome binary: the configured path, `HIVEGRID_CHROME`, `/opt/google/chrome/chrome`,
/// then the usual names on `PATH`. An explicit setting that is not executable is an error
/// rather than a silent fallback.
pub(crate) fn find_chrome(configured: Option<&Path>) -> Result<PathBuf> {
    find_chrome_in(
        configured,
        std::env::var_os("HIVEGRID_CHROME"),
        std::env::var_os("PATH"),
        FIXED_CHROME_PATHS,
    )
}

pub(crate) fn find_chrome_in(
    configured: Option<&Path>,
    env: Option<OsString>,
    path_var: Option<OsString>,
    fixed: &[&str],
) -> Result<PathBuf> {
    if let Some(p) = configured {
        if is_executable(p) {
            return Ok(p.to_path_buf());
        }
        bail!("the configured browser {} is not an executable file", p.display());
    }
    if let Some(e) = env.filter(|e| !e.is_empty()) {
        let p = PathBuf::from(&e);
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("HIVEGRID_CHROME={} is not an executable file", p.display());
    }
    for f in fixed {
        let p = Path::new(f);
        if is_executable(p) {
            return Ok(p.to_path_buf());
        }
    }
    if let Some(path) = path_var {
        for dir in std::env::split_paths(&path) {
            for name in CHROME_NAMES {
                let p = dir.join(name);
                if is_executable(&p) {
                    return Ok(p);
                }
            }
        }
    }
    bail!(
        "no Chrome or Chromium found: install Google Chrome or Chromium, choose a browser in \
         Settings (Web), or set HIVEGRID_CHROME to the browser binary"
    )
}

/// Directory name of a profile: injective, so two profile names never share a directory (and
/// so one Chrome). ASCII letters, digits, `-`, `_` and `.` stay; every other byte (including `/`
/// and `%`) becomes `%XX`. The empty name is `main`.
pub fn profile_dir_name(profile: &str) -> String {
    use std::fmt::Write as _;
    if profile.is_empty() {
        return "profile-main".to_string();
    }
    let mut s = String::from("profile-");
    for b in profile.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            s.push(b as char);
        } else {
            let _ = write!(s, "%{b:02X}");
        }
    }
    s
}

/// Environment variables Chrome inherits; everything else of the GUI process (tokens, `WEZTERM_*`,
/// agent credentials) stays out of the browser and its renderers.
pub(crate) fn chrome_env_allowed(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "HOME",
        "PATH",
        "LANG",
        "TZ",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "DBUS_SESSION_BUS_ADDRESS",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "all_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
    ];
    EXACT.contains(&name)
        || name.starts_with("XDG_")
        || name.starts_with("LC_")
        || name.starts_with("HIVEGRID_WEB_")
}

/// The user agent of a Chrome that is not headless: what `chrome --version` printed
/// (`Google Chrome 154.0.8037.92`) as Chrome's reduced user agent. Headless Chrome's own says
/// `HeadlessChrome`, which sites such as Google's sign-in take as automation. `None` when the
/// output holds no `<major>.` version.
pub(crate) fn user_agent_from_version(output: &str) -> Option<String> {
    let major = output
        .split_whitespace()
        .find_map(|w| w.split_once('.').map(|(m, _)| m).filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit())))?;
    Some(format!(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

/// `chrome --version`, run with the same minimal environment as the browser itself.
fn chrome_version(chrome: &Path) -> Option<String> {
    let out = Command::new(chrome)
        .arg("--version")
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(chrome_env_allowed)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Command line flags of the browser (everything after the binary). `user_agent` replaces the
/// headless one unless the extra flags bring their own.
pub(crate) fn chrome_args(
    cfg: &WebConfig,
    profile_dir: &Path,
    devtools_port: bool,
    user_agent: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "--headless=new",
        "--remote-debugging-pipe",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-sync",
        "--disable-background-networking",
        "--disable-features=Translate,MediaRouter",
        "--password-store=basic",
        "--force-color-profile=srgb",
        // The windows of a headless browser are never "visible" to a window manager; without
        // these Chrome throttles timers and rAF of pages it considers occluded.
        "--disable-renderer-backgrounding",
        "--disable-backgrounding-occluded-windows",
        "--disable-background-timer-throttling",
        // Headless Chrome sets navigator.webdriver; the browser is driven by a person here.
        "--disable-blink-features=AutomationControlled",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!("--user-data-dir={}", profile_dir.display()));
    if let Some(ua) = user_agent.filter(|_| !cfg.extra_flags.iter().any(|f| f.starts_with("--user-agent"))) {
        a.push(format!("--user-agent={ua}"));
    }
    if devtools_port {
        log::warn!(
            "HIVEGRID_WEB_DEVTOOLS=1: Chrome opens a remote debugging port on 127.0.0.1 without \
             authentication; any local process can control the browser and read its cookies"
        );
        a.push("--remote-debugging-port=0".to_string());
    }
    a.extend(cfg.extra_flags.iter().cloned());
    a.push("about:blank".to_string());
    a
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))?;
    let meta = std::fs::metadata(path)?;
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExitInfo {
    /// "exited with code 1", "killed by signal 9 (SIGKILL)".
    pub description: String,
    /// The last lines Chrome printed to stderr (launch failures: sandbox, missing libs).
    pub log_tail: Option<String>,
}

impl std::fmt::Display for ExitInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)?;
        if let Some(t) = &self.log_tail {
            write!(f, ": {t}")?;
        }
        Ok(())
    }
}

fn describe_status(st: std::process::ExitStatus) -> String {
    if let Some(c) = st.code() {
        format!("exited with code {c}")
    } else if let Some(s) = st.signal() {
        let name = match s {
            libc::SIGKILL => " (SIGKILL)",
            libc::SIGSEGV => " (SIGSEGV)",
            libc::SIGABRT => " (SIGABRT)",
            libc::SIGTERM => " (SIGTERM)",
            libc::SIGBUS => " (SIGBUS)",
            _ => "",
        };
        format!("killed by signal {s}{name}")
    } else {
        "exited".to_string()
    }
}

fn log_tail(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = &lines[lines.len().saturating_sub(6)..];
    (!tail.is_empty()).then(|| tail.join(" | "))
}

/// What a launcher hands back: the two pipe ends and the means to supervise the process.
pub(crate) struct Launched {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub pid: u32,
    /// Blocks until the process is gone and reaped.
    pub wait: Box<dyn FnOnce() -> ExitInfo + Send>,
    /// Forcefully ends the whole process group; no effect once the process is gone.
    pub kill: Arc<dyn Fn() + Send + Sync>,
    /// Where `<data_root>/profile-<name>/DevToolsActivePort` lives, when a port was requested.
    pub profile_dir: Option<PathBuf>,
}

/// `Launched` minus the blocking `wait`, which stays on the supervisor thread.
struct Started {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    pid: u32,
    kill: Arc<dyn Fn() + Send + Sync>,
    profile_dir: Option<PathBuf>,
}

pub(crate) trait Launcher: Send + Sync {
    /// Called on the browser's supervisor thread, which must outlive the process: the kernel
    /// ties `PR_SET_PDEATHSIG` to the *thread* that forked.
    fn launch(&self, cfg: &WebConfig, devtools_port: bool) -> Result<Launched>;
}

pub(crate) struct ChromeLauncher;

fn pipe_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills `fds` on success; both ends are owned from here on.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe2");
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

/// Move `fd` above the stdio and CDP slots so the child's `dup2(.., 3|4)` cannot clobber it.
fn lift(fd: OwnedFd) -> Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor we then own.
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if n < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_DUPFD_CLOEXEC)");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

impl Launcher for ChromeLauncher {
    fn launch(&self, cfg: &WebConfig, devtools_port: bool) -> Result<Launched> {
        let chrome = find_chrome(cfg.chrome.as_deref())?;
        private_dir(&cfg.data_root)?;
        let profile_dir = cfg.data_root.join(profile_dir_name(&cfg.profile));
        private_dir(&profile_dir)?;
        let log_path = cfg
            .data_root
            .join(format!("{}.log", profile_dir_name(&cfg.profile)));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&log_path)
            .with_context(|| format!("open {}", log_path.display()))?;
        // A stale port file from a previous run would be read as this run's port.
        let _ = std::fs::remove_file(profile_dir.join("DevToolsActivePort"));

        let (to_chrome_r, to_chrome_w) = pipe_pair()?;
        let (from_chrome_r, from_chrome_w) = pipe_pair()?;
        let child_in = lift(to_chrome_r)?;
        let child_out = lift(from_chrome_w)?;
        let (in_fd, out_fd) = (child_in.as_raw_fd(), child_out.as_raw_fd());
        let parent_pid = unsafe { libc::getpid() };

        let mut cmd = Command::new(&chrome);
        let user_agent = chrome_version(&chrome).and_then(|v| user_agent_from_version(&v));
        cmd.args(chrome_args(cfg, &profile_dir, devtools_port, user_agent.as_deref()))
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(chrome_env_allowed)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .process_group(0);
        // SAFETY: only async-signal-safe calls between fork and exec (dup2, prctl, getppid, _exit).
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(in_fd, 3) < 0 || libc::dup2(out_fd, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Chrome dies with its parent (SIGKILL), even when the parent is killed.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died between fork and prctl.
                if libc::getppid() != parent_pid {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("start {}", chrome.display()))?;
        drop(child_in);
        drop(child_out);

        let pid = child.id();
        let alive = Arc::new(Mutex::new(true));
        let kill: Arc<dyn Fn() + Send + Sync> = {
            let alive = alive.clone();
            Arc::new(move || {
                let alive = alive.lock();
                if *alive {
                    // SAFETY: the leader is not reaped while `alive` is true, so its pid (and
                    // process group id) cannot belong to anyone else.
                    unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
                }
            })
        };
        let wait_log = log_path.clone();
        let wait: Box<dyn FnOnce() -> ExitInfo + Send> = Box::new(move || {
            // Wait for the exit without reaping, so the group can still be addressed safely.
            loop {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let r = unsafe {
                    libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT)
                };
                if r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
            {
                let mut alive = alive.lock();
                // Zygote, renderers, GPU and utility processes of a dead browser go with it.
                unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
                *alive = false;
            }
            let description = match child.wait() {
                Ok(st) => describe_status(st),
                Err(e) => format!("wait failed: {e}"),
            };
            ExitInfo {
                description,
                log_tail: log_tail(&wait_log),
            }
        });

        Ok(Launched {
            reader: Box::new(std::fs::File::from(from_chrome_r)),
            writer: Box::new(std::fs::File::from(to_chrome_w)),
            pid,
            wait,
            kill,
            profile_dir: Some(profile_dir),
        })
    }
}

struct ExitState {
    info: Mutex<Option<ExitInfo>>,
    cv: Condvar,
}

impl ExitState {
    fn set(&self, info: ExitInfo) {
        *self.info.lock() = Some(info);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> Option<ExitInfo> {
        let deadline = Instant::now() + timeout;
        let mut g = self.info.lock();
        while g.is_none() {
            if self.cv.wait_until(&mut g, deadline).timed_out() {
                break;
            }
        }
        g.clone()
    }
}

static NEXT_BROWSER_ID: AtomicU64 = AtomicU64::new(1);

/// One Chrome process with its CDP connection, shared by every view of a profile.
pub(crate) struct Browser {
    pub id: u64,
    pub conn: Connection,
    pid: u32,
    exit: Arc<ExitState>,
    kill: Arc<dyn Fn() + Send + Sync>,
    closing: AtomicBool,
    dead: Mutex<Option<String>>,
    /// Page targets of live views, by CDP target id.
    targets: Mutex<HashMap<String, Weak<ViewCore>>>,
    devtools_port: AtomicU16,
    /// The device scale factor Chrome runs at (`--force-device-scale-factor`): the size of its
    /// screencast frames follows it, whatever a page emulates.
    pub scale: f64,
}

/// Permissions denied by default; pages never get a prompt in a pane. Left alone are the ones
/// pages rely on and that expose nothing about the user: clipboard writes (they only reach
/// the headless browser's own clipboard), storage access, durable storage, background sync,
/// wake locks, keyboard/pointer lock and fullscreen. Names are the W3C `PermissionDescriptor`
/// names `Browser.setPermission` takes (checked against Chrome 154).
const DENIED_PERMISSIONS: &[&str] = &[
    "geolocation",
    "notifications",
    "push",
    "midi",
    "midi-sysex",
    "camera",
    "camera-ptz",
    "microphone",
    "display-capture",
    "ambient-light-sensor",
    "accelerometer",
    "gyroscope",
    "magnetometer",
    "idle-detection",
    "window-management",
    "local-fonts",
    "clipboard-read",
    "payment-handler",
    "nfc",
    "speaker-selection",
    "captured-surface-control",
    "local-network-access",
    "periodic-background-sync",
];

const SCALE_FLAG: &str = "--force-device-scale-factor";

/// The scale in a `--force-device-scale-factor=N` of the user's own flags (the last one wins).
fn scale_flag(flags: &[String]) -> Option<f64> {
    flags
        .iter()
        .rev()
        .find_map(|f| f.strip_prefix(SCALE_FLAG)?.strip_prefix('=')?.parse().ok())
        .filter(|s: &f64| s.is_finite() && *s > 0.0)
}

fn permission_descriptor(name: &str) -> Value {
    match name {
        "midi-sysex" => json!({"name": "midi", "sysex": true}),
        "camera-ptz" => json!({"name": "camera", "panTiltZoom": true}),
        "push" => json!({"name": "push", "userVisibleOnly": true}),
        n => json!({"name": n}),
    }
}

impl Browser {
    /// Launch Chrome (on its own supervisor thread) and do the CDP handshake. `scale` is the
    /// device scale factor to run at (a flag in `cfg.extra_flags` wins).
    pub fn launch(launcher: &Arc<dyn Launcher>, cfg: &WebConfig, scale: f64) -> Result<Arc<Browser>> {
        let mut cfg = cfg.clone();
        let scale = match scale_flag(&cfg.extra_flags) {
            Some(own) => own,
            None => {
                cfg.extra_flags.push(format!("{SCALE_FLAG}={scale}"));
                scale
            }
        };
        let devtools = devtools_enabled();
        let exit = Arc::new(ExitState {
            info: Mutex::new(None),
            cv: Condvar::new(),
        });
        let (ready_tx, ready_rx) = flume::bounded::<Result<Started>>(1);
        {
            let launcher = launcher.clone();
            let cfg = cfg.clone();
            let exit = exit.clone();
            std::thread::Builder::new()
                .name(format!("hgweb-supervisor-{}", cfg.profile))
                .spawn(move || match launcher.launch(&cfg, devtools) {
                    Ok(l) => {
                        let Launched {
                            reader,
                            writer,
                            pid,
                            wait,
                            kill,
                            profile_dir,
                        } = l;
                        let started = Started {
                            reader,
                            writer,
                            pid,
                            kill,
                            profile_dir,
                        };
                        if ready_tx.send(Ok(started)).is_err() {
                            return;
                        }
                        exit.set(wait());
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                })
                .context("spawn supervisor thread")?;
        }
        let started = ready_rx
            .recv()
            .map_err(|_| anyhow!("browser supervisor ended unexpectedly"))??;

        let browser = Arc::new(Browser {
            id: NEXT_BROWSER_ID.fetch_add(1, Ordering::Relaxed),
            conn: Connection::new(started.reader, started.writer),
            pid: started.pid,
            exit,
            kill: started.kill,
            closing: AtomicBool::new(false),
            dead: Mutex::new(None),
            targets: Mutex::new(HashMap::new()),
            devtools_port: AtomicU16::new(0),
            scale,
        });
        let weak = Arc::downgrade(&browser);
        browser.conn.set_sink(None, {
            let weak = weak.clone();
            Arc::new(move |ev| {
                if let Some(b) = weak.upgrade() {
                    b.browser_event(ev);
                }
            })
        });
        browser.conn.on_close(move || {
            if let Some(b) = weak.upgrade() {
                b.connection_lost();
            }
        });

        match browser.handshake(started.profile_dir.as_deref(), devtools) {
            Ok(port) => {
                browser.devtools_port.store(port.unwrap_or(0), Ordering::Release);
                Ok(browser)
            }
            Err(e) => {
                browser.closing.store(true, Ordering::SeqCst);
                (browser.kill)();
                Err(e.context("Chrome failed to start"))
            }
        }
    }

    fn handshake(&self, profile_dir: Option<&Path>, devtools: bool) -> Result<Option<u16>> {
        let call = |m: &'static str, p: Value| self.conn.call(m, p, None, STARTUP_TIMEOUT);
        match call("Browser.getVersion", json!({})) {
            Ok(_) => {}
            Err(CdpError::Closed) => {
                let info = self.exit.wait(Duration::from_secs(3));
                bail!(
                    "Chrome exited during startup{}",
                    info.map(|i| format!(": {i}")).unwrap_or_default()
                );
            }
            Err(e) => bail!("Chrome did not answer: {e}"),
        }
        call(
            "Target.setDiscoverTargets",
            json!({"discover": true, "filter": [{"type": "page"}]}),
        )
            .map_err(|e| anyhow!("Target.setDiscoverTargets: {e}"))?;
        self.apply_policies(None);
        Ok(if devtools {
            profile_dir.and_then(read_devtools_port)
        } else {
            None
        })
    }

    /// Downloads off and permission prompts answered "denied", for the default context or one
    /// private context.
    pub fn apply_policies(&self, context: Option<&str>) {
        let mut p = json!({"behavior": "deny"});
        if let Some(c) = context {
            p["browserContextId"] = json!(c);
        }
        self.conn.notify("Browser.setDownloadBehavior", p, None);
        for name in DENIED_PERMISSIONS {
            let mut p = json!({"permission": permission_descriptor(name), "setting": "denied"});
            if let Some(c) = context {
                p["browserContextId"] = json!(c);
            }
            self.conn.notify("Browser.setPermission", p, None);
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn devtools_port(&self) -> Option<u16> {
        match self.devtools_port.load(Ordering::Acquire) {
            0 => None,
            p => Some(p),
        }
    }

    /// Usable for new views: neither shutting down nor gone.
    pub fn is_usable(&self) -> bool {
        !self.closing.load(Ordering::Acquire) && self.dead.lock().is_none() && !self.conn.is_closed()
    }

    /// Wait for the process to be gone.
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        self.exit.wait(timeout).is_some()
    }

    pub fn register_target(&self, target_id: &str, view: Weak<ViewCore>) {
        self.targets.lock().insert(target_id.to_string(), view);
    }

    pub fn unregister_target(&self, target_id: &str) {
        self.targets.lock().remove(target_id);
    }

    fn view_of(&self, target_id: &str) -> Option<Arc<ViewCore>> {
        self.targets.lock().get(target_id).and_then(Weak::upgrade)
    }

    /// Graceful shutdown: `Browser.close`, then the process group is killed if Chrome has not
    /// exited within the grace period. Returns at once.
    pub fn shutdown(self: &Arc<Self>) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.dead.lock().is_some() || self.conn.is_closed() {
            (self.kill)();
            return;
        }
        self.conn.notify("Browser.close", json!({}), None);
        let me = self.clone();
        let _ = std::thread::Builder::new()
            .name("hgweb-reaper".into())
            .spawn(move || {
                if me.exit.wait(SHUTDOWN_GRACE).is_none() {
                    log::warn!("Chrome ignored Browser.close; killing it");
                    (me.kill)();
                    me.exit.wait(Duration::from_secs(5));
                }
                me.conn.close();
            });
    }

    /// Kill the process group now (an orderly `shutdown` did not get it to exit).
    pub fn kill_now(&self) {
        (self.kill)();
    }

    fn connection_lost(self: &Arc<Self>) {
        // The exit status explains the loss best; the supervisor usually has it within ms.
        let reason = match self.exit.wait(Duration::from_millis(1500)) {
            Some(i) => format!("Browser engine exited: {i}"),
            None => "Browser engine connection lost".to_string(),
        };
        *self.dead.lock() = Some(reason.clone());
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        log::warn!("{reason}");
        let views: Vec<Arc<ViewCore>> = self
            .targets
            .lock()
            .drain()
            .filter_map(|(_, v)| v.upgrade())
            .collect();
        for v in views {
            v.browser_died(self.id, &reason);
        }
    }

    fn browser_event(self: &Arc<Self>, ev: Event) {
        log::trace!("browser event {} {}", ev.method, ev.params);
        match ev.method.as_str() {
            "Target.targetCrashed" | "Target.targetDestroyed" => {
                let id = ev.params["targetId"].as_str();
                if let Some(v) = id.and_then(|id| self.view_of(id)) {
                    v.target_event(ev);
                }
            }
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                let opener = info["openerId"].as_str().and_then(|o| self.view_of(o));
                if info["type"] == "page" && opener.is_some() {
                    // A popup (window.open, target=_blank): the pane navigates itself to the URL
                    // (see `Page.windowOpen` in the view); the extra window is not wanted.
                    if let Some(id) = info["targetId"].as_str() {
                        self.conn
                            .notify("Target.closeTarget", json!({"targetId": id}), None);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Reached when the last reference goes without an orderly shutdown (engine dropped):
        // ask Chrome to quit, and make sure it cannot outlive us.
        if !self.closing.swap(true, Ordering::AcqRel) && !self.conn.is_closed() {
            self.conn.notify("Browser.close", json!({}), None);
        }
        let kill = self.kill.clone();
        let exit = self.exit.clone();
        let _ = std::thread::Builder::new()
            .name("hgweb-reaper".into())
            .spawn(move || {
                if exit.wait(SHUTDOWN_GRACE).is_none() {
                    kill();
                }
            });
    }
}

pub(crate) fn devtools_enabled() -> bool {
    std::env::var_os("HIVEGRID_WEB_DEVTOOLS").is_some_and(|v| v == OsStr::new("1"))
}

/// Chrome writes `<port>\n<browser ws path>` to `DevToolsActivePort` once the port is open.
fn read_devtools_port(profile_dir: &Path) -> Option<u16> {
    let path = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Some(p) = s.lines().next().and_then(|l| l.trim().parse().ok()) {
                return Some(p);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn exe(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn discovery_order_is_config_env_fixed_path() {
        let d = tempfile::tempdir().unwrap();
        let cfg = exe(d.path(), "cfg-chrome");
        let env = exe(d.path(), "env-chrome");
        let fixed = exe(d.path(), "fixed-chrome");
        let fixed_s = fixed.to_str().unwrap();
        let path_dir = tempfile::tempdir().unwrap();
        let on_path = exe(path_dir.path(), "chromium");
        let path = Some(OsString::from(path_dir.path()));

        let find = |c: Option<&Path>, e: Option<&Path>, fx: &[&str]| {
            find_chrome_in(c, e.map(|p| p.as_os_str().to_owned()), path.clone(), fx)
        };
        assert_eq!(find(Some(&cfg), Some(&env), &[fixed_s]).unwrap(), cfg);
        assert_eq!(find(None, Some(&env), &[fixed_s]).unwrap(), env);
        assert_eq!(find(None, None, &[fixed_s]).unwrap(), fixed);
        assert_eq!(find(None, None, &["/nonexistent/chrome"]).unwrap(), on_path);
    }

    #[test]
    fn path_lookup_prefers_google_chrome_stable() {
        let d = tempfile::tempdir().unwrap();
        exe(d.path(), "chromium");
        let stable = exe(d.path(), "google-chrome-stable");
        exe(d.path(), "google-chrome");
        let got = find_chrome_in(None, None, Some(OsString::from(d.path())), &[]).unwrap();
        assert_eq!(got, stable);
    }

    #[test]
    fn an_explicit_setting_that_does_not_resolve_is_an_error_not_a_fallback() {
        let d = tempfile::tempdir().unwrap();
        let fixed = exe(d.path(), "fixed");
        let missing = d.path().join("missing");
        let e = find_chrome_in(Some(&missing), None, None, &[fixed.to_str().unwrap()]).unwrap_err();
        assert!(e.to_string().contains("not an executable"), "{e}");
        let e = find_chrome_in(
            None,
            Some(missing.as_os_str().to_owned()),
            None,
            &[fixed.to_str().unwrap()],
        )
        .unwrap_err();
        assert!(e.to_string().contains("HIVEGRID_CHROME"), "{e}");
        // Not executable also counts as missing.
        let plain = d.path().join("plain");
        fs::write(&plain, "x").unwrap();
        assert!(find_chrome_in(Some(&plain), None, None, &[]).is_err());
    }

    #[test]
    fn nothing_found_explains_what_to_do() {
        let e = find_chrome_in(None, None, Some(OsString::from("/nonexistent")), &[]).unwrap_err();
        assert!(e.to_string().contains("HIVEGRID_CHROME"), "{e}");
        let e = find_chrome_in(None, Some(OsString::new()), None, &[]).unwrap_err();
        assert!(e.to_string().contains("no Chrome"), "empty env is unset: {e}");
    }

    #[test]
    fn profile_names_become_safe_directory_names() {
        assert_eq!(profile_dir_name("main"), "profile-main");
        assert_eq!(profile_dir_name(""), "profile-main");
        assert_eq!(profile_dir_name("../etc/passwd"), "profile-..%2Fetc%2Fpasswd");
        assert_eq!(profile_dir_name(".hidden"), "profile-.hidden");
        assert_eq!(profile_dir_name("work 1/é"), "profile-work%201%2F%C3%A9");
        assert!(!profile_dir_name("a/b").contains('/'));
    }

    #[test]
    fn profile_directory_names_never_collide() {
        let names = ["a/b", "a_b", "a%2Fb", "a b", "a%20b", "A", "a", "é", "%C3%A9", "..", "."];
        let mut dirs: Vec<String> = names.iter().map(|n| profile_dir_name(n)).collect();
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), names.len());
    }

    #[test]
    fn chrome_gets_a_minimal_environment() {
        for ok in ["HOME", "PATH", "XDG_RUNTIME_DIR", "LC_ALL", "LANG", "TZ", "DISPLAY", "WAYLAND_DISPLAY",
            "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS", "http_proxy", "HTTPS_PROXY", "NO_PROXY", "ALL_PROXY",
            "HIVEGRID_WEB_FLAGS"]
        {
            assert!(chrome_env_allowed(ok), "{ok}");
        }
        for no in ["WEZTERM_PANE", "WEZTERM_UNIX_SOCKET", "GITHUB_TOKEN", "ANTHROPIC_API_KEY", "SSH_AUTH_SOCK",
            "HIVEGRID_CHROME", "LD_PRELOAD", "XDG"]
        {
            assert!(!chrome_env_allowed(no), "{no}");
        }
    }

    #[test]
    fn default_flags_keep_the_sandbox_and_the_pipe() {
        let cfg = WebConfig {
            extra_flags: vec!["--foo".into()],
            ..Default::default()
        };
        let args = chrome_args(&cfg, Path::new("/d/profile-x"), false, None);
        assert!(args.contains(&"--headless=new".to_string()));
        assert!(args.contains(&"--remote-debugging-pipe".to_string()));
        assert!(args.contains(&"--user-data-dir=/d/profile-x".to_string()));
        assert!(!args.iter().any(|a| a.contains("no-sandbox")));
        assert!(!args.iter().any(|a| a.starts_with("--remote-debugging-port")));
        assert_eq!(args.last().unwrap(), "about:blank");
        assert_eq!(args[args.len() - 2], "--foo");
        let with_port = chrome_args(&cfg, Path::new("/d/p"), true, None);
        assert!(with_port.contains(&"--remote-debugging-port=0".to_string()));
    }

    #[test]
    fn the_user_agent_is_that_of_a_plain_chrome_unless_the_flags_bring_one() {
        let ua = user_agent_from_version("Google Chrome 154.0.8037.92 \n").unwrap();
        assert!(ua.contains("Chrome/154.0.0.0 Safari/537.36") && !ua.contains("Headless"), "{ua}");
        assert_eq!(user_agent_from_version("Chromium 131.0.6778.204 built on Debian").unwrap(), user_agent_from_version("Google Chrome 131.1").unwrap());
        assert_eq!(user_agent_from_version("no version here"), None);
        let mut cfg = WebConfig::default();
        let has = |a: &[String]| a.iter().filter(|f| f.starts_with("--user-agent=")).count();
        let args = chrome_args(&cfg, Path::new("/d/p"), false, Some(&ua));
        assert_eq!(has(&args), 1);
        assert!(args.contains(&"--disable-blink-features=AutomationControlled".to_string()));
        assert_eq!(has(&chrome_args(&cfg, Path::new("/d/p"), false, None)), 0);
        cfg.extra_flags = vec!["--user-agent=mine".into()];
        let args = chrome_args(&cfg, Path::new("/d/p"), false, Some(&ua));
        assert_eq!(has(&args), 1);
        assert!(args.contains(&"--user-agent=mine".to_string()));
    }

    #[test]
    fn data_dirs_are_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a/b/web");
        private_dir(&p).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
        // An existing, too-open directory is tightened.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&p).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn exit_descriptions() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(describe_status(std::process::ExitStatus::from_raw(1 << 8)), "exited with code 1");
        assert_eq!(
            describe_status(std::process::ExitStatus::from_raw(9)),
            "killed by signal 9 (SIGKILL)"
        );
    }

    #[test]
    fn log_tail_keeps_the_last_lines() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("log");
        fs::write(&p, (1..=10).map(|i| format!("line {i}\n\n")).collect::<String>()).unwrap();
        let t = log_tail(&p).unwrap();
        assert!(t.ends_with("line 10") && t.starts_with("line 5"), "{t}");
        assert_eq!(log_tail(&d.path().join("nope")), None);
    }
}
