//! Integration tests against a real headless Chrome. Skipped (with a note on stderr) when no
//! Chrome/Chromium can be found; set `HIVEGRID_CHROME` to choose one.

use hivegrid_web::{
    CdpEngine, Frame, ViewSize, WebConfig, WebEngine, WebEvent, WebKey, WebMouse, WebMouseKind,
    WebState, WebView,
};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::input::{KeyCode, Modifiers};

/// `HGWEB_TEST_LOG=debug|trace` prints the crate's log output (shown for failing tests).
struct TestLog;

impl log::Log for TestLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::max_level()
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

fn init_logging() {
    let level = match std::env::var("HGWEB_TEST_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        _ => log::LevelFilter::Warn,
    };
    let _ = log::set_logger(&TestLog).map(|()| log::set_max_level(level));
}

fn chrome_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HIVEGRID_CHROME") {
        return Some(PathBuf::from(p));
    }
    let fixed = PathBuf::from("/opt/google/chrome/chrome");
    if fixed.exists() {
        return Some(fixed);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"] {
            let p = dir.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

macro_rules! need_chrome {
    () => {
        match chrome_path() {
            Some(p) => p,
            None => {
                eprintln!("skipped: no Chrome or Chromium found (set HIVEGRID_CHROME)");
                return;
            }
        }
    };
}

fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    eventually_for(Duration::from_secs(30), what, &mut f)
}

fn eventually_for(limit: Duration, what: &str, f: &mut dyn FnMut() -> bool) {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(f(), "timed out waiting for: {what}");
}

struct Rig {
    engine: CdpEngine,
    root: tempfile::TempDir,
    chrome: PathBuf,
}

impl Rig {
    fn new(chrome: PathBuf) -> Rig {
        init_logging();
        Rig {
            engine: CdpEngine::new(),
            root: tempfile::tempdir().unwrap(),
            chrome,
        }
    }

    fn cfg(&self, profile: &str, private: bool) -> WebConfig {
        WebConfig {
            chrome: Some(self.chrome.clone()),
            profile: profile.to_string(),
            private,
            data_root: self.root.path().join("web"),
            extra_flags: vec![],
        }
    }

    fn open(&self, url: &str, w: u32, h: u32, profile: &str, private: bool) -> (Arc<dyn WebView>, Events) {
        self.open_dpi(url, w, h, 96, profile, private)
    }

    fn open_dpi(&self, url: &str, w: u32, h: u32, dpi: u32, profile: &str, private: bool) -> (Arc<dyn WebView>, Events) {
        let events = Events::default();
        let e = events.clone();
        let v = self
            .engine
            .create_view(
                url,
                &self.cfg(profile, private),
                ViewSize { pixel_width: w, pixel_height: h, dpi },
                Box::new(move |ev| e.0.lock().push(ev)),
            )
            .unwrap();
        (v, events)
    }

    /// Process ids whose command line mentions this rig's profile directories: the browser and
    /// all of its helpers.
    fn chrome_processes(&self) -> Vec<u32> {
        let needle = self.root.path().to_string_lossy().into_owned();
        let mut out = vec![];
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                if String::from_utf8_lossy(&cmd).contains(&needle) {
                    out.push(pid);
                }
            }
        }
        out
    }
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<WebEvent>>>);

fn running(v: &Arc<dyn WebView>) -> Arc<Frame> {
    eventually("running with a frame", || {
        if let WebState::Crashed { reason } = v.state() {
            panic!("view crashed: {reason}");
        }
        v.state() == WebState::Running && v.take_frame().is_some()
    });
    v.take_frame().unwrap()
}

fn centre(f: &Frame) -> [u8; 3] {
    let i = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
    [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2]]
}

fn near(got: [u8; 3], want: [u8; 3]) -> bool {
    got.iter().zip(want).all(|(g, w)| (*g as i32 - w as i32).abs() <= 14)
}

fn wait_centre(v: &Arc<dyn WebView>, want: [u8; 3]) {
    eventually(&format!("centre pixel {want:?}"), || {
        v.take_frame().is_some_and(|f| near(centre(&f), want))
    });
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn click(v: &Arc<dyn WebView>, x: f32, y: f32) {
    for kind in [WebMouseKind::Down, WebMouseKind::Up] {
        v.mouse(WebMouse {
            kind,
            x,
            y,
            button: 0,
            buttons: if kind == WebMouseKind::Down { 1 } else { 0 },
            mods: 0,
            click_count: 1,
            dx: 0.0,
            dy: 0.0,
        });
    }
}

fn key(v: &Arc<dyn WebView>, c: char) {
    for down in [true, false] {
        v.key(WebKey { down, key: KeyCode::Char(c), mods: Modifiers::NONE });
    }
}

/// A tiny HTTP server for the cookie and popup tests.
fn serve() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = s;
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (extra, body) = match path.as_str() {
                    "/set" => ("Set-Cookie: sid=1; Path=/\r\n", "<title>set-done</title>ok".to_string()),
                    "/setp" => ("Set-Cookie: sid=persist; Path=/; Max-Age=3600\r\n", "<title>setp-done</title>ok".to_string()),
                    "/read" => ("", "<title>x</title><script>document.title='cookie:'+(document.cookie||'none')</script>".to_string()),
                    "/popup" => ("", "<title>popup-page</title>popup".to_string()),
                    "/link" => ("", "<title>link</title><body style=margin:0><a style='display:block;position:absolute;left:0;top:0;width:300px;height:200px' target=_blank href=/popup>go</a>".to_string()),
                    "/open" => ("", "<title>opener</title><body style=margin:0 onmousedown=\"window.open('/popup')\">click".to_string()),
                    _ => ("", "<title>other</title>".to_string()),
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
                    body.len()
                );
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn loads_a_data_page_navigates_and_leaves_no_process_behind() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, events) = rig.open(
        "data:text/html,<body style=\"margin:0;background:%23ff00ff\"><title>magenta</title>",
        320,
        240,
        "t1",
        false,
    );
    assert_eq!(v.state(), WebState::Starting);
    let f = running(&v);
    assert_eq!((f.width, f.height), (320, 240));
    // The initial about:blank frame is held back, so what arrives first is the page (possibly
    // still unpainted white for a frame): its colour shows up within a moment.
    assert_eq!(f.rgba.len(), 320 * 240 * 4);
    assert_eq!(f.rgba[3], 255);
    wait_centre(&v, [255, 0, 255]);
    eventually("title", || v.info().title == "magenta" && !v.info().loading);
    assert!(v.info().url.starts_with("data:text/html"), "{}", v.info().url);
    assert!(events.0.lock().contains(&WebEvent::Frame));

    // Navigate: bare hosts are normalised, data: is passed through.
    v.navigate("data:text/html,<body style=\"margin:0;background:%2300ff00\"><title>green</title>");
    wait_centre(&v, [0, 255, 0]);
    eventually("green title", || v.info().title == "green");
    eventually("history", || v.info().can_go_back && !v.info().can_go_forward);
    v.back();
    wait_centre(&v, [255, 0, 255]);
    eventually("forward flag", || v.info().can_go_forward);

    // Resize: the frame follows in physical pixels.
    v.resize(ViewSize { pixel_width: 400, pixel_height: 300, dpi: 96 });
    eventually("resized frame", || v.take_frame().is_some_and(|f| (f.width, f.height) == (400, 300)));
    // HiDPI: same physical size, the page sees a 2x device pixel ratio (200x150 CSS px).
    v.resize(ViewSize { pixel_width: 400, pixel_height: 300, dpi: 192 });
    // The title follows `resize` events: the override may land after the page's first script run.
    v.navigate("data:text/html,<body style=margin:0><script>function u(){document.title=innerWidth+'x'+innerHeight+'@'+devicePixelRatio}u();onresize=u</script>");
    eventually("dpr", || v.info().title == "200x150@2");

    // Closing the only view ends Chrome and all its helpers.
    let pids = rig.engine.browser_pids();
    assert_eq!(pids.len(), 1);
    assert!(!rig.chrome_processes().is_empty());
    v.close();
    eventually("every chrome process gone", || rig.chrome_processes().is_empty());
    assert!(!pid_alive(pids[0]), "browser process {} lingers (zombie or alive)", pids[0]);
}

/// A page that reports its viewport and device pixel ratio in its title, on a flat colour.
const SCALE_PAGE: &str = "data:text/html,<body style=\"margin:0;background:%23ff00ff\"><script>function u(){document.title=innerWidth+'x'+innerHeight+'@'+devicePixelRatio}u();onresize=u</script>";

/// At a scale other than 1 Chrome's screencast frames are CSS-sized unless the browser itself
/// runs at that scale. A static page sends exactly one frame, so a refused size left the pane
/// blank for good (the placeholder stayed): the frame must come, at physical size.
fn static_page_gets_a_physical_frame(dpi: u32, css: (u32, u32), ratio: &str) {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, _) = rig.open_dpi(SCALE_PAGE, 546, 800, dpi, "hidpi", false);
    let f = running(&v);
    assert_eq!((f.width, f.height), (546, 800), "dpi {dpi}: frame must have the pane's pixels");
    wait_centre(&v, [255, 0, 255]);
    eventually("css viewport and ratio", || v.info().title == format!("{}x{}@{ratio}", css.0, css.1));
    // The first frame is already the final one: nothing was held back and shown late.
    assert_eq!(v.take_frame().map(|f| (f.width, f.height)), Some((546, 800)));
}

#[test]
fn a_static_page_gets_a_physical_size_frame_at_dpi_144() {
    static_page_gets_a_physical_frame(144, (364, 533), "1.5");
}

#[test]
fn a_static_page_gets_a_physical_size_frame_at_dpi_192() {
    static_page_gets_a_physical_frame(192, (273, 400), "2");
}

#[test]
fn views_of_different_scales_in_one_browser_never_stay_blank() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    // The browser starts at 2.0 (first view): a 1.0 view is scaled down to its own pixels.
    let (a, _) = rig.open_dpi(SCALE_PAGE, 400, 300, 192, "mix", false);
    let f = running(&a);
    assert_eq!((f.width, f.height), (400, 300));
    let (b, _) = rig.open_dpi(SCALE_PAGE, 400, 300, 96, "mix", false);
    eventually("frame of the 1.0 view at its pixels", || b.take_frame().is_some_and(|f| (f.width, f.height) == (400, 300)));
    eventually("1.0 ratio", || b.info().title == "400x300@1");
    assert_eq!(rig.engine.browser_pids().len(), 1);
    a.close();
    b.close();
    eventually("browser gone", || rig.chrome_processes().is_empty());

    // The other way round: the browser runs at 1.0, a 2.0 view gets CSS-sized frames, which are
    // shown (stretched by the renderer) after the settling time instead of never.
    let (c, _) = rig.open_dpi(SCALE_PAGE, 400, 300, 96, "mix2", false);
    running(&c);
    let (d, _) = rig.open_dpi(SCALE_PAGE, 400, 300, 192, "mix2", false);
    eventually("a frame for the 2.0 view", || d.take_frame().is_some_and(|f| (f.width, f.height) == (200, 150)));
    eventually("2.0 ratio", || d.info().title == "200x150@2");
}

#[test]
fn evaluates_in_the_page_and_round_trips_the_selection() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, events) = rig.open(
        "data:text/html,<p id=p>hello world</p><input id=i value='field text'><script>getSelection().selectAllChildren(document.getElementById('p'))</script>",
        400,
        200,
        "t2",
        false,
    );
    running(&v);
    eventually("loaded", || !v.info().loading);
    v.copy_selection();
    eventually("selection text", || events.0.lock().contains(&WebEvent::ClipboardText("hello world".into())));
    v.close();
}

#[test]
fn mouse_keyboard_text_and_cursor_reach_the_page() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let page = "data:text/html,<body style=margin:0;cursor:crosshair onmousedown=\"document.title='down:'+event.button+':'+event.clientX+','+event.clientY+':'+event.detail\"><input id=i autofocus style='position:absolute;left:0;top:100px;width:300px;height:30px' oninput=\"document.title='in:'+this.value\"><div style='position:absolute;left:0;top:0;width:50px;height:50px;background:red;cursor:pointer'></div>";
    let (v, events) = rig.open(page, 400, 300, "t3", false);
    running(&v);
    eventually("loaded", || !v.info().loading);

    // Cursor: the red box asks for a pointer, the rest of the page for a crosshair.
    v.mouse(WebMouse { kind: WebMouseKind::Move, x: 20.0, y: 20.0, button: 0, buttons: 0, mods: 0, click_count: 0, dx: 0.0, dy: 0.0 });
    eventually("pointer cursor", || v.cursor().as_deref() == Some("pointer"));
    v.mouse(WebMouse { kind: WebMouseKind::Move, x: 300.0, y: 250.0, button: 0, buttons: 0, mods: 0, click_count: 0, dx: 0.0, dy: 0.0 });
    eventually("crosshair cursor", || v.cursor().as_deref() == Some("crosshair"));
    assert!(events.0.lock().contains(&WebEvent::Cursor("pointer".into())));
    v.mouse(WebMouse { kind: WebMouseKind::Move, x: 100.0, y: 115.0, button: 0, buttons: 0, mods: 0, click_count: 0, dx: 0.0, dy: 0.0 });
    eventually("text cursor over a text field", || v.cursor().as_deref() == Some("text"));

    // Mouse: coordinates are CSS pixels in the view; a double click carries detail 2.
    v.mouse(WebMouse { kind: WebMouseKind::Down, x: 200.0, y: 220.0, button: 0, buttons: 1, mods: 0, click_count: 1, dx: 0.0, dy: 0.0 });
    eventually("mousedown seen", || v.info().title == "down:0:200,220:1");
    v.mouse(WebMouse { kind: WebMouseKind::Up, x: 200.0, y: 220.0, button: 0, buttons: 0, mods: 0, click_count: 1, dx: 0.0, dy: 0.0 });
    v.mouse(WebMouse { kind: WebMouseKind::Down, x: 200.0, y: 220.0, button: 0, buttons: 1, mods: 0, click_count: 2, dx: 0.0, dy: 0.0 });
    eventually("double click detail", || v.info().title == "down:0:200,220:2");
    v.mouse(WebMouse { kind: WebMouseKind::Up, x: 200.0, y: 220.0, button: 0, buttons: 0, mods: 0, click_count: 2, dx: 0.0, dy: 0.0 });

    // Keyboard: click into the field, type through the key table, then insert composed text.
    click(&v, 100.0, 115.0);
    for c in "hi".chars() {
        key(&v, c);
    }
    eventually("typed text", || v.info().title == "in:hi");
    v.key(WebKey { down: true, key: KeyCode::Char('Z'), mods: Modifiers::SHIFT });
    v.key(WebKey { down: false, key: KeyCode::Char('Z'), mods: Modifiers::SHIFT });
    eventually("shifted char", || v.info().title == "in:hiZ");
    v.key(WebKey { down: true, key: KeyCode::Backspace, mods: Modifiers::NONE });
    v.key(WebKey { down: false, key: KeyCode::Backspace, mods: Modifiers::NONE });
    eventually("backspace", || v.info().title == "in:hi");
    v.insert_text("é!");
    eventually("insert_text", || v.info().title == "in:hié!");
    // Select all + typing replaces: the editing command path.
    v.key(WebKey { down: true, key: KeyCode::Char('a'), mods: Modifiers::CTRL });
    v.key(WebKey { down: false, key: KeyCode::Char('a'), mods: Modifiers::CTRL });
    key(&v, 'x');
    eventually("ctrl+a then type", || v.info().title == "in:x");
    v.close();
}

#[test]
fn wheel_scrolls_the_page() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let page = "data:text/html,<body style=margin:0><div style='height:5000px'></div><script>onscroll=()=>document.title='y='+Math.round(scrollY)</script>";
    let (v, _) = rig.open(page, 400, 300, "t4", false);
    running(&v);
    eventually("loaded", || !v.info().loading);
    v.mouse(WebMouse { kind: WebMouseKind::Wheel, x: 100.0, y: 100.0, button: 0, buttons: 0, mods: 0, click_count: 0, dx: 0.0, dy: 100.0 });
    eventually("scrolled", || v.info().title == "y=100");
    v.close();
}

#[test]
fn views_share_a_browser_and_private_views_have_their_own_cookies() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let base = serve();
    let (a, _) = rig.open(&format!("{base}/set"), 300, 200, "cookies", false);
    running(&a);
    eventually("cookie set", || a.info().title == "set-done");
    let (shared, _) = rig.open(&format!("{base}/read"), 300, 200, "cookies", false);
    let (private, _) = rig.open(&format!("{base}/read"), 300, 200, "cookies", true);
    running(&shared);
    running(&private);
    assert_eq!(rig.engine.browser_pids().len(), 1, "one browser per profile");
    eventually("shared context sees the cookie", || shared.info().title == "cookie:sid=1");
    eventually("private context does not", || private.info().title == "cookie:none");
    // A different profile has a different browser and its own cookie jar.
    let (other, _) = rig.open(&format!("{base}/read"), 300, 200, "other", false);
    running(&other);
    assert_eq!(rig.engine.browser_pids().len(), 2);
    eventually("other profile has no cookie", || other.info().title == "cookie:none");

    // The browser of a profile outlives all but its last view.
    let pids = rig.engine.browser_pids();
    a.close();
    shared.close();
    std::thread::sleep(Duration::from_millis(500));
    assert!(pids.iter().all(|p| pid_alive(*p)), "private view still needs its browser");
    private.close();
    other.close();
    eventually("all browsers gone", || rig.chrome_processes().is_empty());
}

#[test]
fn an_orderly_shutdown_keeps_persistent_cookies_and_leaves_no_process() {
    let chrome = need_chrome!();
    let mut rig = Rig::new(chrome);
    let base = serve();
    let (v, _) = rig.open(&format!("{base}/setp"), 300, 200, "keep", false);
    running(&v);
    eventually("cookie set", || v.info().title == "setp-done" && !v.info().loading);
    // The GUI's exit path: no view is closed first, the engine is asked to quit its browsers.
    let started = Instant::now();
    rig.engine.shutdown(Duration::from_secs(5));
    assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    assert!(rig.chrome_processes().is_empty(), "browser still running after shutdown: {:?}", rig.chrome_processes());
    drop(v);

    // A new process (a new engine) finds the cookie in the profile.
    rig.engine = CdpEngine::new();
    let (w, _) = rig.open(&format!("{base}/read"), 300, 200, "keep", false);
    running(&w);
    eventually("cookie read", || w.info().title == "cookie:sid=persist");
    w.close();
    eventually("chrome gone", || rig.chrome_processes().is_empty());
}

#[test]
fn profile_directory_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, _) = rig.open("about:blank", 100, 100, "perm", false);
    running(&v);
    let dir = rig.root.path().join("web/profile-perm");
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(rig.root.path().join("web")).unwrap().permissions().mode() & 0o777, 0o700);
    v.close();
}

#[test]
fn popups_load_in_the_same_pane_and_dialogs_do_not_hang() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let base = serve();

    // target=_blank link
    let (v, _) = rig.open(&format!("{base}/link"), 400, 300, "popups", false);
    running(&v);
    eventually("link page", || v.info().title == "link");
    click(&v, 50.0, 50.0);
    eventually("the pane followed the link", || v.info().title == "popup-page");

    // window.open from a user gesture
    v.navigate(&format!("{base}/open"));
    eventually("opener page", || v.info().title == "opener");
    click(&v, 50.0, 50.0);
    eventually("the pane followed window.open", || v.info().title == "popup-page");

    // alert/confirm/prompt must not block the page forever.
    v.navigate("data:text/html,<title>start</title><script>alert('a');var c=confirm('c');var p=prompt('p','d');document.title='done:'+c+':'+p</script>");
    eventually("dialogs answered", || v.info().title == "done:false:null");
    v.close();
}

#[test]
fn downloads_are_denied_and_permissions_are_refused() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, _) = rig.open(
        "data:text/html,<title>start</title><script>navigator.permissions.query({name:'geolocation'}).then(r=>document.title='geo:'+r.state)</script>",
        300,
        200,
        "policy",
        false,
    );
    running(&v);
    eventually("geolocation denied", || v.info().title == "geo:denied");
    v.close();
}

#[test]
fn killing_chrome_crashes_the_views_and_restart_brings_them_back() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (a, _) = rig.open("data:text/html,<body style=background:%23ff00ff><title>A</title>", 200, 150, "crash", false);
    let (b, _) = rig.open("data:text/html,<body style=background:%2300ff00><title>B</title>", 200, 150, "crash", false);
    running(&a);
    running(&b);
    eventually("titles", || a.info().title == "A" && b.info().title == "B");
    let pid = rig.engine.browser_pids()[0];
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };

    for v in [&a, &b] {
        eventually("crash noticed", || matches!(v.state(), WebState::Crashed { .. }));
        let WebState::Crashed { reason } = v.state() else { unreachable!() };
        assert!(reason.contains("signal 9"), "{reason}");
        assert!(v.take_frame().is_none());
    }
    eventually("helpers reaped with the leader", || rig.chrome_processes().is_empty());

    a.restart();
    wait_centre(&a, [255, 0, 255]);
    wait_centre(&b, [0, 255, 0]); // revived by the same relaunch
    assert_eq!(a.state(), WebState::Running);
    assert_eq!(b.state(), WebState::Running);
    assert_eq!(rig.engine.browser_pids().len(), 1);
    a.close();
    b.close();
    eventually("clean exit", || rig.chrome_processes().is_empty());
}

#[test]
fn a_hung_renderer_can_be_restarted_in_place() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let (v, _) = rig.open("data:text/html,<body style=background:%2300ff00><title>ok</title>", 200, 150, "restart", false);
    running(&v);
    v.restart();
    // The page is recreated on the same URL.
    eventually("restarted", || v.state() == WebState::Running && v.info().title == "ok");
    wait_centre(&v, [0, 255, 0]);
    v.close();
}

#[test]
fn hidden_views_stop_producing_frames_and_resume() {
    let chrome = need_chrome!();
    let rig = Rig::new(chrome);
    let page = "data:text/html,<body style=margin:0><script>let h=0;(function f(){document.body.style.background='hsl('+(h+=5)+',80%,50%)';requestAnimationFrame(f)})()</script>";
    let (v, _) = rig.open(page, 200, 150, "vis", false);
    running(&v);
    let seq = |v: &Arc<dyn WebView>| v.take_frame().map_or(0, |f| f.seq);
    eventually("frames flow", || seq(&v) > 10);
    v.set_visible(false);
    std::thread::sleep(Duration::from_millis(600)); // in-flight frames drain
    let frozen = seq(&v);
    std::thread::sleep(Duration::from_millis(800));
    assert!(seq(&v) <= frozen + 1, "hidden view kept streaming: {frozen} -> {}", seq(&v));
    v.set_visible(true);
    eventually("frames resume", || seq(&v) > frozen + 10);
    v.close();
}

#[test]
fn no_chrome_is_a_crashed_view_with_an_actionable_reason() {
    let rig = Rig::new(PathBuf::from("/nonexistent/chrome"));
    let (v, _) = rig.open("about:blank", 100, 100, "nochrome", false);
    eventually("crashed", || matches!(v.state(), WebState::Crashed { .. }));
    let WebState::Crashed { reason } = v.state() else { unreachable!() };
    assert!(reason.contains("not an executable file"), "{reason}");
}
