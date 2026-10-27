//! View lifecycle against the scripted fake browser: startup, navigation, input mapping,
//! sharing and shutdown of browsers, crashes and restarts, policies.

use crate::fake::{eventually, FakeChrome, FakeLauncher};
use crate::*;
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use termwiz::input::{KeyCode, Modifiers};

const SIZE: ViewSize = ViewSize {
    pixel_width: 200,
    pixel_height: 100,
    dpi: 96,
};

struct Harness {
    engine: CdpEngine,
    launcher: Arc<FakeLauncher>,
}

impl Harness {
    fn new() -> Self {
        let launcher = FakeLauncher::new();
        Harness {
            engine: CdpEngine::with_launcher(launcher.clone()),
            launcher,
        }
    }

    fn cfg(&self, profile: &str, private: bool) -> WebConfig {
        WebConfig {
            chrome: None,
            profile: profile.to_string(),
            private,
            data_root: std::path::PathBuf::from("/nonexistent"),
            extra_flags: vec![],
        }
    }

    fn open(&self, url: &str, profile: &str) -> (Arc<dyn WebView>, Arc<Mutex<Vec<WebEvent>>>) {
        self.open_with(url, self.cfg(profile, false))
    }

    fn open_with(&self, url: &str, cfg: WebConfig) -> (Arc<dyn WebView>, Arc<Mutex<Vec<WebEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let e = events.clone();
        let v = self
            .engine
            .create_view(url, &cfg, SIZE, Box::new(move |ev| e.lock().push(ev)))
            .unwrap();
        (v, events)
    }

    fn running(&self, url: &str, profile: &str) -> (Arc<dyn WebView>, Arc<Mutex<Vec<WebEvent>>>) {
        let (v, e) = self.open(url, profile);
        wait_running(&v);
        (v, e)
    }
}

fn wait_running(v: &Arc<dyn WebView>) {
    eventually("view running", || v.state() == WebState::Running);
}

fn wait_frame(v: &Arc<dyn WebView>) -> Arc<Frame> {
    eventually("a frame", || v.take_frame().is_some());
    v.take_frame().unwrap()
}

fn crashed_reason(v: &Arc<dyn WebView>) -> Option<String> {
    match v.state() {
        WebState::Crashed { reason } => Some(reason),
        _ => None,
    }
}

fn mouse(kind: WebMouseKind) -> WebMouse {
    WebMouse {
        kind,
        x: 10.5,
        y: 20.0,
        button: 0,
        buttons: 0,
        mods: 0,
        click_count: 1,
        dx: 0.0,
        dy: 0.0,
    }
}

fn session_cmds(c: &FakeChrome, method: &str) -> Vec<serde_json::Value> {
    c.all(method).into_iter().map(|c| c.params).collect()
}

#[test]
fn starts_in_starting_then_streams_frames_for_the_requested_size() {
    let h = Harness::new();
    let (v, events) = h.open("example.com", "main");
    // The URL is known at once, normalised; the pane can show it while Chrome starts.
    assert_eq!(v.info().url, "https://example.com");
    assert!(v.info().loading);
    assert!(v.take_frame().is_none());

    wait_running(&v);
    let f = wait_frame(&v);
    assert_eq!((f.width, f.height), (200, 100));
    assert!(f.seq >= 1);
    let centre = &f.rgba[((50 * 200 + 100) * 4) as usize..][..4];
    assert!(centre[0] > 240 && centre[1] < 16 && centre[2] > 240 && centre[3] == 255, "{centre:?}");

    eventually("title and load end", || {
        let i = v.info();
        i.title == "Title of https://example.com" && !i.loading
    });
    // The owner is told after the frame is stored, so the event may trail what `take_frame` shows.
    eventually("frame, state and info events", || {
        let evs = events.lock();
        evs.contains(&WebEvent::Frame) && evs.contains(&WebEvent::State) && evs.contains(&WebEvent::Info)
    });

    let c = h.launcher.chrome(0);
    let create = c.last("Target.createTarget").unwrap().params;
    assert_eq!(create["newWindow"], true, "every view gets its own window: tabs of one window are hidden");
    assert_eq!((create["width"].as_f64(), create["height"].as_f64()), (Some(200.0), Some(100.0)));
    let metrics = c.last("Emulation.setDeviceMetricsOverride").unwrap().params;
    assert_eq!(metrics["deviceScaleFactor"], 1.0);
    let sc = c.last("Page.startScreencast").unwrap().params;
    assert_eq!((sc["format"].clone(), sc["maxWidth"].clone(), sc["maxHeight"].clone()), (json!("jpeg"), json!(200), json!(100)));
    assert_eq!(c.last("Page.navigate").unwrap().params["url"], "https://example.com");
    assert_eq!(c.last("Runtime.addBinding").unwrap().params["name"], "hgcursor");
    assert!(c.count("Browser.setDownloadBehavior") >= 1, "downloads denied");
    let denied = session_cmds(&c, "Browser.setPermission");
    assert!(denied.iter().all(|p| p["setting"] == "denied") && denied.len() > 10);
    // Ordering: the screencast is on before the page is told to load.
    let order: Vec<String> = c.commands().into_iter().map(|c| c.method).collect();
    let pos = |m: &str| order.iter().position(|x| x == m).unwrap();
    assert!(pos("Page.enable") < pos("Page.startScreencast"));
    assert!(pos("Page.startScreencast") < pos("Page.navigate"));
}

#[test]
fn navigation_and_history() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    eventually("first load", || !v.info().loading);
    eventually("history flag", || v.info().can_go_back); // about:blank precedes the first page

    v.navigate("rust-lang.org");
    eventually("second page", || v.info().url == "https://rust-lang.org" && !v.info().loading);
    assert_eq!(c.last("Page.navigate").unwrap().params["url"], "https://rust-lang.org");
    eventually("history flags", || v.info().can_go_back && !v.info().can_go_forward);

    v.back();
    eventually("back", || v.info().url == "https://example.com");
    assert_eq!(c.last("Page.navigateToHistoryEntry").unwrap().params["entryId"], 101);
    eventually("forward flag", || v.info().can_go_forward);

    v.forward();
    eventually("forward", || v.info().url == "https://rust-lang.org");
    assert_eq!(c.last("Page.navigateToHistoryEntry").unwrap().params["entryId"], 102);

    v.reload(false);
    eventually("reload", || c.count("Page.reload") == 1);
    assert_eq!(c.last("Page.reload").unwrap().params["ignoreCache"], false);
    v.reload(true);
    eventually("hard reload", || c.count("Page.reload") == 2);
    assert_eq!(c.last("Page.reload").unwrap().params["ignoreCache"], true);
    v.stop();
    eventually("stop", || c.count("Page.stopLoading") == 1);

    v.navigate("localhost:3000/x");
    eventually("localhost", || c.last("Page.navigate").unwrap().params["url"] == "http://localhost:3000/x");
    v.navigate("data:text/html,<b>hi</b>");
    eventually("data url", || c.last("Page.navigate").unwrap().params["url"] == "data:text/html,<b>hi</b>");
}

#[test]
fn navigation_failures_show_the_requested_url_and_a_restart_retries_it() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    // The page's own load events must be out of the way before the injected ones.
    eventually("first load done", || v.info().title == "Title of https://example.com" && !v.info().loading);
    c.session_event(
        0,
        "Page.frameNavigated",
        json!({"frame": {"id": "F1", "url": "chrome-error://chromewebdata/", "unreachableUrl": "https://nope.invalid/"}}),
    );
    eventually("unreachable url", || v.info().url == "https://nope.invalid/");
    // An error page without the original URL is shown as is but never becomes the reload target.
    c.session_event(
        0,
        "Page.frameNavigated",
        json!({"frame": {"id": "F1", "url": "chrome-error://chromewebdata/"}}),
    );
    eventually("error page url", || v.info().url == "chrome-error://chromewebdata/");
    c.crash("exited with code 1");
    eventually("crashed", || crashed_reason(&v).is_some());
    v.restart();
    wait_running(&v);
    let c = h.launcher.chrome(1);
    eventually("the requested url is retried", || {
        c.last("Page.navigate").is_some_and(|n| n.params["url"] == "https://nope.invalid/")
    });
}

#[test]
fn mouse_events_map_to_cdp() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);

    v.mouse(WebMouse { buttons: 1, mods: 2, click_count: 2, ..mouse(WebMouseKind::Down) });
    v.mouse(WebMouse { buttons: 0, click_count: 2, ..mouse(WebMouseKind::Up) });
    v.mouse(WebMouse { button: 2, buttons: 2, ..mouse(WebMouseKind::Down) });
    v.mouse(mouse(WebMouseKind::Move));
    v.mouse(WebMouse { buttons: 4, button: 0, ..mouse(WebMouseKind::Move) });
    v.mouse(WebMouse { dy: -100.0, dx: 20.0, ..mouse(WebMouseKind::Wheel) });
    eventually("6 mouse events", || c.count("Input.dispatchMouseEvent") == 6);
    let m = session_cmds(&c, "Input.dispatchMouseEvent");

    assert_eq!(m[0]["type"], "mousePressed");
    assert_eq!((m[0]["button"].clone(), m[0]["buttons"].clone(), m[0]["clickCount"].clone()), (json!("left"), json!(1), json!(2)));
    assert_eq!((m[0]["x"].as_f64(), m[0]["y"].as_f64(), m[0]["modifiers"].clone()), (Some(10.5), Some(20.0), json!(2)));
    assert_eq!(m[1]["type"], "mouseReleased");
    assert_eq!(m[1]["button"], "left");
    assert_eq!(m[2]["button"], "right");
    assert_eq!((m[3]["type"].clone(), m[3]["button"].clone()), (json!("mouseMoved"), json!("none")));
    assert_eq!((m[4]["type"].clone(), m[4]["button"].clone()), (json!("mouseMoved"), json!("middle")), "a drag names the held button");
    assert_eq!(m[5]["type"], "mouseWheel");
    assert_eq!((m[5]["deltaX"].as_f64(), m[5]["deltaY"].as_f64()), (Some(20.0), Some(-100.0)));
    // Commands go to the page session, not the browser.
    assert!(c.all("Input.dispatchMouseEvent").iter().all(|c| c.session.is_some()));
}

#[test]
fn key_and_text_input_map_to_cdp() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);

    v.key(WebKey { down: true, key: KeyCode::Char('a'), mods: Modifiers::NONE });
    v.key(WebKey { down: false, key: KeyCode::Char('a'), mods: Modifiers::NONE });
    v.key(WebKey { down: true, key: KeyCode::Char('a'), mods: Modifiers::CTRL });
    v.key(WebKey { down: true, key: KeyCode::Enter, mods: Modifiers::NONE });
    v.key(WebKey { down: true, key: KeyCode::Shift, mods: Modifiers::SHIFT }); // bare modifier: nothing
    v.key(WebKey { down: true, key: KeyCode::Function(5), mods: Modifiers::NONE });
    v.insert_text("héllo");
    v.insert_text("");
    eventually("keys and text", || c.count("Input.insertText") == 1 && c.count("Input.dispatchKeyEvent") == 5);
    let k = session_cmds(&c, "Input.dispatchKeyEvent");
    assert_eq!((k[0]["type"].clone(), k[0]["text"].clone(), k[0]["key"].clone()), (json!("keyDown"), json!("a"), json!("a")));
    assert_eq!(k[1]["type"], "keyUp");
    assert_eq!((k[2]["type"].clone(), k[2]["commands"].clone()), (json!("rawKeyDown"), json!(["selectAll"])));
    assert_eq!((k[3]["key"].clone(), k[3]["text"].clone(), k[3]["windowsVirtualKeyCode"].clone()), (json!("Enter"), json!("\r"), json!(13)));
    assert_eq!((k[4]["key"].clone(), k[4]["windowsVirtualKeyCode"].clone()), (json!("F5"), json!(116)));
    assert_eq!(c.last("Input.insertText").unwrap().params["text"], "héllo");
}

#[test]
fn copy_selection_reports_the_selected_text() {
    let h = Harness::new();
    let (v, events) = h.running("example.com", "main");
    v.copy_selection();
    eventually("clipboard text", || events.lock().contains(&WebEvent::ClipboardText("selected text".into())));
    let c = h.launcher.chrome(0);
    let expr = c
        .all("Runtime.evaluate")
        .into_iter()
        .map(|c| c.params)
        .find(|p| p["expression"].as_str().is_some_and(|e| e.contains("getSelection")))
        .expect("the selection was read from the page");
    assert_eq!(expr["returnByValue"], true);
}

#[test]
fn javascript_dialogs_are_answered_by_policy() {
    let h = Harness::new();
    let (_v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    for ty in ["alert", "confirm", "prompt", "beforeunload"] {
        c.session_event(0, "Page.javascriptDialogOpening", json!({"type": ty, "message": "m", "url": "u"}));
    }
    eventually("four answers", || c.count("Page.handleJavaScriptDialog") == 4);
    let a: Vec<bool> = session_cmds(&c, "Page.handleJavaScriptDialog")
        .iter()
        .map(|p| p["accept"].as_bool().unwrap())
        .collect();
    assert_eq!(a, [true, false, false, true], "alert and beforeunload accepted, confirm and prompt cancelled");
}

#[test]
fn cursor_changes_are_reported_once() {
    let h = Harness::new();
    let (v, events) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    assert_eq!(v.cursor(), None);
    for shape in ["pointer", "pointer", "text"] {
        c.session_event(0, "Runtime.bindingCalled", json!({"name": "hgcursor", "payload": shape, "executionContextId": 1}));
    }
    c.session_event(0, "Runtime.bindingCalled", json!({"name": "other", "payload": "x"}));
    eventually("text cursor", || v.cursor().as_deref() == Some("text"));
    let cursors: Vec<WebEvent> = events.lock().iter().filter(|e| matches!(e, WebEvent::Cursor(_))).cloned().collect();
    assert_eq!(cursors, [WebEvent::Cursor("pointer".into()), WebEvent::Cursor("text".into())]);
}

#[test]
fn views_of_a_profile_share_one_browser_which_ends_with_the_last_view() {
    let h = Harness::new();
    let (a, _) = h.running("a.example", "work");
    let (b, _) = h.running("b.example", "work");
    assert_eq!(h.launcher.launches(), 1);
    let c = h.launcher.chrome(0);
    assert_eq!(c.count("Target.createTarget"), 2);
    assert_ne!(c.target_id(0), c.target_id(1));
    // Both pages deliver frames independently.
    wait_frame(&a);
    wait_frame(&b);

    let (other, _) = h.running("c.example", "play");
    assert_eq!(h.launcher.launches(), 2, "another profile, another browser");

    a.close();
    eventually("a's target closed", || c.count("Target.closeTarget") == 1);
    assert_eq!(c.count("Browser.close"), 0, "b still needs the browser");
    assert!(!c.has_exited());

    b.close();
    eventually("browser asked to quit", || c.count("Browser.close") == 1);
    eventually("browser gone", || c.has_exited());
    assert!(!h.launcher.chrome(1).has_exited(), "the other profile is untouched");
    drop(other);
    eventually("dropping a view closes it", || h.launcher.chrome(1).has_exited());
}

#[test]
fn a_new_view_after_the_last_one_closed_gets_a_fresh_browser() {
    let h = Harness::new();
    let (a, _) = h.running("a.example", "main");
    a.close();
    let first = h.launcher.chrome(0);
    // Once the browser has been told to quit, a new view of the profile cannot reuse it: it waits
    // for the old process to be gone (they share one profile directory) and launches again.
    eventually("old browser told to quit", || first.count("Browser.close") == 1);
    let (b, _) = h.running("b.example", "main");
    assert_eq!(h.launcher.launches(), 2);
    assert!(first.has_exited());
    wait_frame(&b);
}

#[test]
fn a_changed_browser_applies_to_the_next_browser_process_of_the_profile() {
    // One profile directory is locked by one browser process, so the pool is keyed by profile
    // alone: a view asking for another binary joins the running browser (a second Chrome on the
    // same directory would hand the work to the first and exit), and the new binary starts with
    // the next browser process, after the last view of the profile closed.
    let h = Harness::new();
    let with = |path: &str| WebConfig {
        chrome: Some(path.into()),
        ..h.cfg("main", false)
    };
    let (a, _) = h.open_with("a.example", with("/old/chrome"));
    wait_running(&a);
    let (b, _) = h.open_with("b.example", with("/new/chrome"));
    wait_running(&b);
    assert_eq!(h.launcher.launches(), 1, "the running browser is joined");
    assert_eq!(h.launcher.cfgs.lock()[0].chrome.as_deref(), Some(std::path::Path::new("/old/chrome")));

    a.close();
    b.close();
    let first = h.launcher.chrome(0);
    eventually("old browser gone", || first.has_exited());
    let (c, _) = h.open_with("c.example", with("/new/chrome"));
    wait_running(&c);
    assert_eq!(h.launcher.launches(), 2);
    assert_eq!(h.launcher.cfgs.lock()[1].chrome.as_deref(), Some(std::path::Path::new("/new/chrome")));
}

#[test]
fn private_views_use_their_own_context() {
    let h = Harness::new();
    let (shared, _) = h.running("a.example", "main");
    let (p, _) = h.open_with("b.example", h.cfg("main", true));
    wait_running(&p);
    assert_eq!(h.launcher.launches(), 1, "private views still share the browser process");
    let c = h.launcher.chrome(0);
    assert_eq!(c.count("Target.createBrowserContext"), 1);
    assert_eq!(c.last("Target.createBrowserContext").unwrap().params["disposeOnDetach"], true);
    let creates = session_cmds(&c, "Target.createTarget");
    assert!(creates[0].get("browserContextId").is_none());
    assert_eq!(creates[1]["browserContextId"], "CTX1");
    // Policies apply to the private context too.
    assert!(session_cmds(&c, "Browser.setDownloadBehavior").iter().any(|p| p["browserContextId"] == "CTX1"));
    assert!(session_cmds(&c, "Browser.setPermission").iter().any(|p| p["browserContextId"] == "CTX1"));

    p.close();
    eventually("context disposed", || c.count("Target.disposeBrowserContext") == 1);
    assert_eq!(c.last("Target.disposeBrowserContext").unwrap().params["browserContextId"], "CTX1");
    drop(shared);
}

#[test]
fn browser_crash_marks_every_view_and_a_restart_revives_all_of_them() {
    let h = Harness::new();
    let (a, a_events) = h.running("a.example", "main");
    let (b, _) = h.running("b.example", "main");
    wait_frame(&a);
    wait_frame(&b);
    a.navigate("a2.example");
    eventually("a navigated", || a.info().url == "https://a2.example");

    h.launcher.chrome(0).crash("killed by signal 9 (SIGKILL)");
    for v in [&a, &b] {
        eventually("crash noticed", || crashed_reason(v).is_some());
        let r = crashed_reason(v).unwrap();
        assert!(r.contains("Browser engine exited") && r.contains("signal 9"), "{r}");
        assert!(v.take_frame().is_none(), "a dead page shows the placeholder, not a stale frame");
    }
    assert!(a_events.lock().contains(&WebEvent::State));
    assert_eq!(h.launcher.launches(), 1, "no automatic relaunch: the user decides");

    a.restart();
    eventually("a back", || a.state() == WebState::Running);
    eventually("b revived by a's relaunch", || b.state() == WebState::Running);
    assert_eq!(h.launcher.launches(), 2, "one relaunch serves both views");
    let c = h.launcher.chrome(1);
    eventually("frames again", || a.take_frame().is_some() && b.take_frame().is_some());
    // Each came back on its own last URL.
    eventually("each view reloads its own last url", || {
        let navs: Vec<String> = session_cmds(&c, "Page.navigate")
            .iter()
            .map(|p| p["url"].as_str().unwrap().to_string())
            .collect();
        navs.contains(&"https://a2.example".to_string()) && navs.contains(&"https://b.example".to_string())
    });
    assert_eq!(c.count("Target.createTarget"), 2);
}

#[test]
fn a_renderer_crash_needs_an_explicit_restart_in_the_same_browser() {
    let h = Harness::new();
    let (a, _) = h.running("a.example", "main");
    let (b, _) = h.running("b.example", "main");
    wait_frame(&a);
    let c = h.launcher.chrome(0);
    let old = c.target_id(0);
    c.browser_event("Target.targetCrashed", json!({"targetId": old, "status": "crashed", "errorCode": 11}));
    eventually("a crashed", || crashed_reason(&a).is_some());
    assert!(crashed_reason(&a).unwrap().contains("crashed"));
    assert!(a.take_frame().is_none());
    assert_eq!(b.state(), WebState::Running, "the other page is not affected");

    a.reload(false); // reload on a crashed page restarts it
    eventually("a restarted", || a.state() == WebState::Running);
    assert_eq!(h.launcher.launches(), 1);
    assert_eq!(c.count("Target.createTarget"), 3);
    assert!(session_cmds(&c, "Target.closeTarget").iter().any(|p| p["targetId"] == old.as_str()), "the dead target is closed");
    eventually("a reloaded its url", || c.last("Page.navigate").unwrap().params["url"] == "https://a.example");
    eventually("frame after restart", || a.take_frame().is_some());
}

#[test]
fn navigating_a_crashed_view_restarts_it_on_the_new_url() {
    let h = Harness::new();
    let (a, _) = h.running("a.example", "main");
    let c = h.launcher.chrome(0);
    c.browser_event("Target.targetCrashed", json!({"targetId": c.target_id(0), "status": "oom"}));
    eventually("crashed", || crashed_reason(&a).is_some());
    a.navigate("elsewhere.example");
    eventually("running again", || a.state() == WebState::Running);
    eventually("new url loaded", || c.last("Page.navigate").unwrap().params["url"] == "https://elsewhere.example");
}

#[test]
fn a_browser_that_cannot_start_is_a_crashed_view_with_the_reason() {
    let h = Harness::new();
    *h.launcher.fail.lock() = Some("no Chrome or Chromium found: set HIVEGRID_CHROME".into());
    let (v, _) = h.open("example.com", "main");
    eventually("crashed", || crashed_reason(&v).is_some());
    assert!(crashed_reason(&v).unwrap().contains("HIVEGRID_CHROME"));
    assert!(!v.info().loading);
    assert_eq!(h.launcher.launches(), 0);

    *h.launcher.fail.lock() = None;
    v.restart();
    wait_running(&v);
    assert_eq!(h.launcher.launches(), 1);
    wait_frame(&v);
}

#[test]
fn resizes_are_debounced_and_the_frame_follows() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    wait_frame(&v);
    let c = h.launcher.chrome(0);
    let before = c.count("Emulation.setDeviceMetricsOverride");

    for i in 1..=20u32 {
        v.resize(ViewSize { pixel_width: 200 + i * 10, pixel_height: 100 + i * 5, dpi: 96 });
    }
    v.resize(ViewSize { pixel_width: 0, pixel_height: 50, dpi: 96 }); // ignored
    eventually("final size applied", || {
        v.take_frame().is_some_and(|f| (f.width, f.height) == (400, 200))
    });
    let overrides = c.count("Emulation.setDeviceMetricsOverride") - before;
    assert!((1..=2).contains(&overrides), "20 resizes in a burst became {overrides} overrides");
    let last = c.last("Emulation.setDeviceMetricsOverride").unwrap().params;
    assert_eq!((last["width"].as_f64(), last["height"].as_f64()), (Some(400.0), Some(200.0)));
    // The screencast restarts with the new bounds.
    assert_eq!(c.last("Page.startScreencast").unwrap().params["maxWidth"], 400);

    // HiDPI: CSS size shrinks, scale factor rises, the frame stays in physical pixels.
    v.resize(ViewSize { pixel_width: 400, pixel_height: 200, dpi: 192 });
    eventually("dpr override", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 2.0);
    let last = c.last("Emulation.setDeviceMetricsOverride").unwrap().params;
    assert_eq!((last["width"].as_f64(), last["height"].as_f64()), (Some(200.0), Some(100.0)));
}

#[test]
fn same_size_resize_does_nothing() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    let before = c.count("Emulation.setDeviceMetricsOverride");
    v.resize(SIZE);
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(c.count("Emulation.setDeviceMetricsOverride"), before);
}

#[test]
fn hidden_views_stop_the_screencast_and_minimize_their_window() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    let state = |c: &FakeChrome| c.last("Browser.setWindowBounds").map(|c| c.params);
    assert!(state(&c).is_none(), "a visible view never touches its window");
    v.set_visible(false);
    eventually("stopped", || c.count("Page.stopScreencast") == 1);
    eventually("window minimized", || state(&c).is_some_and(|p| p["bounds"]["windowState"] == "minimized"));
    assert_eq!(state(&c).unwrap()["windowId"], 7000);
    assert_eq!(c.count("Page.setWebLifecycleState"), 0, "frozen pages stay hidden after unfreezing; not used");
    let starts = c.count("Page.startScreencast");
    v.set_visible(false); // no change, no traffic
    v.set_visible(true);
    eventually("resumed", || c.count("Page.startScreencast") == starts + 1);
    eventually("window restored", || state(&c).is_some_and(|p| p["bounds"]["windowState"] == "normal"));
}

#[test]
fn a_view_hidden_while_starting_ends_up_minimized() {
    let h = Harness::new();
    let (v, _) = h.open("example.com", "main");
    v.set_visible(false);
    wait_running(&v);
    let c = h.launcher.chrome(0);
    eventually("window minimized", || {
        c.last("Browser.setWindowBounds").is_some_and(|c| c.params["bounds"]["windowState"] == "minimized")
    });
    v.set_visible(true);
    eventually("screencast on", || c.count("Page.startScreencast") >= 1);
    eventually("window restored", || {
        c.last("Browser.setWindowBounds").is_some_and(|c| c.params["bounds"]["windowState"] == "normal")
    });
}

#[test]
fn focus_toggles_focus_emulation_and_raises_the_page() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    assert_eq!(c.last("Emulation.setFocusEmulationEnabled").unwrap().params["enabled"], false);
    v.set_focused(true);
    eventually("focused", || c.last("Emulation.setFocusEmulationEnabled").unwrap().params["enabled"] == true);
    assert_eq!(c.count("Page.bringToFront"), 1);
    v.set_focused(false);
    eventually("unfocused", || c.last("Emulation.setFocusEmulationEnabled").unwrap().params["enabled"] == false);
}

#[test]
fn zoom_rescales_the_layout_viewport_not_the_frame() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    v.set_zoom(200);
    eventually("zoomed", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 2.0);
    let p = c.last("Emulation.setDeviceMetricsOverride").unwrap().params;
    assert_eq!((p["width"].as_f64(), p["height"].as_f64()), (Some(100.0), Some(50.0)));
    v.set_zoom(5000); // clamped
    eventually("clamped", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 5.0);
}

#[test]
fn mouse_positions_are_divided_by_the_zoom() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    let at = |x: f32, y: f32| WebMouse { x, y, ..mouse(WebMouseKind::Move) };
    v.set_zoom(200);
    eventually("zoomed", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 2.0);
    v.mouse(at(100.0, 50.0));
    eventually("zoomed move", || c.count("Input.dispatchMouseEvent") == 1);
    let m = c.last("Input.dispatchMouseEvent").unwrap().params;
    assert_eq!((m["x"].as_f64(), m["y"].as_f64()), (Some(50.0), Some(25.0)));
    v.set_zoom(125);
    eventually("zoomed 125", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 1.25);
    v.mouse(at(100.0, 50.0));
    eventually("second move", || c.count("Input.dispatchMouseEvent") == 2);
    let m = c.last("Input.dispatchMouseEvent").unwrap().params;
    assert_eq!((m["x"].as_f64(), m["y"].as_f64()), (Some(80.0), Some(40.0)));
    v.set_zoom(100);
    eventually("back to 100", || c.last("Emulation.setDeviceMetricsOverride").unwrap().params["deviceScaleFactor"] == 1.0);
    v.mouse(at(100.0, 50.0));
    eventually("third move", || c.count("Input.dispatchMouseEvent") == 3);
    let m = c.last("Input.dispatchMouseEvent").unwrap().params;
    assert_eq!((m["x"].as_f64(), m["y"].as_f64()), (Some(100.0), Some(50.0)));
}

fn binding(c: &FakeChrome, payload: &str) {
    c.session_event(0, "Runtime.bindingCalled", json!({"name": "hgcursor", "payload": payload, "executionContextId": 1}));
}

#[test]
fn cursor_reports_are_allowlisted_and_a_flood_stays_bounded() {
    let h = Harness::new();
    let (v, events) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    binding(&c, "pointer");
    eventually("pointer", || v.cursor().as_deref() == Some("pointer"));
    // Anything but a CSS cursor keyword is dropped: junk, markup, urls, and a huge payload.
    for junk in ["", "javascript:alert(1)", "url(x), auto", "pointer; rm -rf", "<b>", &"x".repeat(1_000_000), &"pointer".repeat(10)] {
        binding(&c, junk);
    }
    // The same value again is no change.
    binding(&c, "pointer");
    // Case and padding are tolerated.
    binding(&c, "  TEXT ");
    eventually("text", || v.cursor().as_deref() == Some("text"));
    // A script hammering the binding: 4000 reports alternating between two keywords.
    for i in 0..4000 {
        binding(&c, if i % 2 == 0 { "wait" } else { "grab" });
    }
    eventually("flood processed", || v.cursor().as_deref() == Some("grab"));
    let seen: Vec<String> = events
        .lock()
        .iter()
        .filter_map(|e| match e {
            WebEvent::Cursor(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    assert!(seen.iter().all(|c| ["pointer", "text", "wait", "grab"].contains(&c.as_str())), "{:?}", &seen[..seen.len().min(5)]);
    assert_eq!(&seen[..2], ["pointer", "text"], "no event for junk or for an unchanged value");
    // The worker thread was never involved: the page still answers other commands at once.
    v.navigate("https://b.example");
    eventually("navigation still prompt", || c.count("Page.navigate") == 2);
}

#[test]
fn popups_only_load_web_addresses_in_the_pane() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    eventually("first load done", || v.info().title == "Title of https://example.com" && !v.info().loading);
    let n = c.count("Page.navigate");
    for url in [
        "data:text/html,<script>alert(1)</script>",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "chrome://settings",
        "blob:https://x/1",
        "  JavaScript:alert(1)",
    ] {
        c.session_event(0, "Page.windowOpen", json!({"url": url, "windowName": "", "windowFeatures": [], "userGesture": true}));
    }
    c.session_event(0, "Page.windowOpen", json!({"url": "HTTPS://Ok.example/p", "windowName": "", "windowFeatures": [], "userGesture": true}));
    eventually("the web address is followed", || c.count("Page.navigate") == n + 1);
    assert_eq!(c.last("Page.navigate").unwrap().params["url"], "HTTPS://Ok.example/p");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(c.count("Page.navigate"), n + 1, "no other URL was navigated to");
    assert!(v.info().url.starts_with("HTTPS://Ok"));
}

#[test]
fn popups_are_closed_and_their_url_loads_in_the_pane() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    let opener = c.target_id(0);
    // A popup of our page: closed.
    c.browser_event(
        "Target.targetCreated",
        json!({"targetInfo": {"targetId": "POP1", "type": "page", "openerId": opener, "url": ""}}),
    );
    eventually("popup closed", || session_cmds(&c, "Target.closeTarget").iter().any(|p| p["targetId"] == "POP1"));
    // Its URL arrives as Page.windowOpen on the opener.
    c.session_event(0, "Page.windowOpen", json!({"url": "https://popup.example/x", "windowName": "", "windowFeatures": [], "userGesture": true}));
    eventually("pane follows the link", || v.info().url == "https://popup.example/x");
    assert_eq!(c.last("Page.navigate").unwrap().params["url"], "https://popup.example/x");
    // Targets that are not ours, and non-page targets, are left alone.
    c.browser_event("Target.targetCreated", json!({"targetInfo": {"targetId": "FOREIGN", "type": "page", "openerId": "elsewhere"}}));
    c.browser_event("Target.targetCreated", json!({"targetInfo": {"targetId": "WORKER", "type": "service_worker", "openerId": opener}}));
    c.browser_event("Target.targetCreated", json!({"targetInfo": {"targetId": "SYNC", "type": "page"}}));
    std::thread::sleep(Duration::from_millis(100));
    let closed: Vec<String> = session_cmds(&c, "Target.closeTarget").iter().map(|p| p["targetId"].as_str().unwrap().to_string()).collect();
    assert_eq!(closed, ["POP1"]);
}

#[test]
fn a_page_that_closes_itself_ends_the_view() {
    let h = Harness::new();
    let (a, events) = h.running("a.example", "work");
    let (b, _) = h.running("b.example", "work");
    let c = h.launcher.chrome(0);
    c.browser_event("Target.targetDestroyed", json!({"targetId": c.target_id(0)}));
    eventually("closed event", || events.lock().contains(&WebEvent::Closed));
    // `close()` afterwards is harmless; the browser stays for the other view.
    a.close();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(c.count("Browser.close"), 0);
    assert_eq!(b.state(), WebState::Running);
}

#[test]
fn close_is_quiet_idempotent_and_final() {
    let h = Harness::new();
    let (v, events) = h.running("example.com", "main");
    wait_frame(&v);
    v.close();
    v.close();
    eventually("browser gone", || h.launcher.chrome(0).has_exited());
    assert!(!events.lock().contains(&WebEvent::Closed), "close() is not a page-initiated close");
    // Late calls on a closed view must not panic or resurrect anything.
    v.navigate("x.example");
    v.mouse(mouse(WebMouseKind::Move));
    v.key(WebKey { down: true, key: KeyCode::Char('x'), mods: Modifiers::NONE });
    v.insert_text("x");
    v.copy_selection();
    v.resize(ViewSize { pixel_width: 10, pixel_height: 10, dpi: 96 });
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.launcher.launches(), 1);
}

#[test]
fn restart_of_a_running_view_replaces_its_page() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    let old = c.target_id(0);
    v.restart();
    eventually("new target", || c.count("Target.createTarget") == 2);
    eventually("old target closed", || session_cmds(&c, "Target.closeTarget").iter().any(|p| p["targetId"] == old.as_str()));
    wait_running(&v);
    assert_eq!(h.launcher.launches(), 1);
}

#[test]
fn a_burst_of_frames_is_acknowledged_and_the_latest_wins() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    wait_frame(&v);
    let c = h.launcher.chrome(0);
    // Let the startup frames settle, then count only the burst.
    std::thread::sleep(Duration::from_millis(100));
    let acks_before = c.count("Page.screencastFrameAck");
    for i in 0..6u8 {
        c.frame(0, 200, 100, [i * 40, 0, 0]);
    }
    eventually("burst acknowledged", || c.count("Page.screencastFrameAck") == acks_before + 6);
    // The last frame of the burst (red 200) is the one on show once everything is decoded.
    eventually("latest frame shown", || {
        v.take_frame().is_some_and(|f| {
            let i = ((50 * 200 + 100) * 4) as usize;
            f.rgba[i] >= 190 && f.rgba[i + 1] < 12
        })
    });
    // Every ack names the screencast session of its frame and goes to the page session.
    let acks = c.all("Page.screencastFrameAck");
    assert!(acks.iter().all(|a| a.params["sessionId"] == 1 && a.session.is_some()));
}

#[test]
fn frames_of_the_wrong_size_are_dropped_until_the_size_settles() {
    let h = Harness::new();
    let (v, events) = h.running("example.com", "main");
    let first = wait_frame(&v);
    assert_eq!((first.width, first.height), (200, 100));
    let c = h.launcher.chrome(0);
    std::thread::sleep(Duration::from_millis(50));
    let frames_before = events.lock().iter().filter(|e| **e == WebEvent::Frame).count();
    let acks = c.count("Page.screencastFrameAck");

    // Right after startup Chrome may still deliver the previous surface size: not shown.
    c.frame(0, 77, 33, [0, 0, 255]);
    eventually("acked", || c.count("Page.screencastFrameAck") == acks + 1);
    std::thread::sleep(Duration::from_millis(50));
    let shown = v.take_frame().unwrap();
    assert_eq!((shown.width, shown.height), (200, 100), "the stale-sized frame was not stored");
    assert_eq!(
        events.lock().iter().filter(|e| **e == WebEvent::Frame).count(),
        frames_before,
        "and the owner was not woken for it"
    );

    // A size Chrome insists on after the settle time is accepted rather than starving the view.
    std::thread::sleep(Duration::from_millis(550));
    c.frame(0, 77, 33, [0, 0, 255]);
    eventually("accepted after settling", || v.take_frame().is_some_and(|f| (f.width, f.height) == (77, 33)));
}

#[test]
fn a_refused_frame_is_shown_once_the_size_settled_when_nothing_better_comes() {
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let (v, events) = h.open("data:text/html,x", "main");
    wait_running(&v);
    let c = h.launcher.chrome(0);
    eventually("navigation sent", || c.count("Page.navigate") == 1);
    c.session_event(0, "Page.frameNavigated", json!({"frame": {"id": "F1", "url": "data:text/html,x"}}));

    // A static page: Chrome sends one frame, and its size is not the one asked for.
    c.frame(0, 77, 33, [0, 0, 255]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(v.take_frame().is_none(), "not shown while the size may still settle");
    // Nothing else comes, but the pane must not stay blank.
    eventually("the refused frame shows after the settling time", || {
        v.take_frame().is_some_and(|f| (f.width, f.height) == (77, 33))
    });
    assert!(events.lock().contains(&WebEvent::Frame), "and the owner was woken for it");
}

#[test]
fn a_refused_frame_is_dropped_when_the_size_changes_again() {
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let (v, _) = h.open("data:text/html,x", "main");
    wait_running(&v);
    let c = h.launcher.chrome(0);
    eventually("navigation sent", || c.count("Page.navigate") == 1);
    c.session_event(0, "Page.frameNavigated", json!({"frame": {"id": "F1", "url": "data:text/html,x"}}));
    c.frame(0, 77, 33, [0, 0, 255]);
    std::thread::sleep(Duration::from_millis(60));
    // The pane is resized: the refused frame is of an older surface and must never show.
    v.resize(ViewSize { pixel_width: 300, pixel_height: 150, dpi: 96 });
    std::thread::sleep(Duration::from_millis(900));
    assert!(v.take_frame().is_none(), "{:?}", v.take_frame());
}

#[test]
fn the_browser_runs_at_the_scale_of_its_first_view_and_frames_of_that_size_show_at_once() {
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    let v = h
        .engine
        .create_view(
            "data:text/html,x",
            &h.cfg("main", false),
            ViewSize { pixel_width: 546, pixel_height: 800, dpi: 144 },
            Box::new(move |ev| e.lock().push(ev)),
        )
        .unwrap();
    wait_running(&v);
    let flags = h.launcher.cfgs.lock()[0].extra_flags.clone();
    assert_eq!(flags, vec!["--force-device-scale-factor=1.5".to_string()]);

    let c = h.launcher.chrome(0);
    eventually("navigation sent", || c.count("Page.navigate") == 1);
    // The page sees the right ratio and a CSS viewport of pixels / 1.5.
    let m = c.last("Emulation.setDeviceMetricsOverride").unwrap().params;
    assert_eq!((m["width"].as_f64(), m["height"].as_f64(), m["deviceScaleFactor"].as_f64()), (Some(364.0), Some(533.0), Some(1.5)));
    c.session_event(0, "Page.frameNavigated", json!({"frame": {"id": "F1", "url": "data:text/html,x"}}));
    // Chrome at that scale delivers physical-size frames: shown at once, not after a wait.
    c.frame(0, 546, 800, [255, 0, 255]);
    eventually_quick(&v, (546, 800));
}

fn eventually_quick(v: &Arc<dyn WebView>, want: (u32, u32)) {
    let end = std::time::Instant::now() + Duration::from_millis(300);
    while std::time::Instant::now() < end {
        if v.take_frame().is_some_and(|f| (f.width, f.height) == want) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no {want:?} frame within 300 ms (before the 500 ms settling time)");
}

#[test]
fn a_flag_of_the_users_own_decides_the_browser_scale() {
    let h = Harness::new();
    let mut cfg = h.cfg("main", false);
    cfg.extra_flags = vec!["--force-device-scale-factor=2".to_string()];
    let (v, _) = h.open_with("data:text/html,x", cfg);
    wait_running(&v);
    let flags = h.launcher.cfgs.lock()[0].extra_flags.clone();
    assert_eq!(flags, vec!["--force-device-scale-factor=2".to_string()], "no second flag is added");
}

#[test]
fn no_frame_survives_a_crash_even_when_one_is_being_decoded() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    wait_frame(&v);
    let c = h.launcher.chrome(0);
    // A flood of large frames keeps the decoder busy at the moment of the crash.
    for _ in 0..30 {
        c.frame(0, 200, 100, [1, 2, 3]);
    }
    c.crash("killed by signal 9 (SIGKILL)");
    eventually("crashed", || crashed_reason(&v).is_some());
    for _ in 0..50 {
        assert!(v.take_frame().is_none(), "a frame was stored after the crash");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_initial_blank_frame_is_held_until_the_first_navigation_commits() {
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let (v, events) = h.open("example.com", "main");
    wait_running(&v);
    let c = h.launcher.chrome(0);
    eventually("navigation sent", || c.count("Page.navigate") == 1);

    // The window's about:blank renders before the page commits: acknowledged, not shown.
    let acks = c.count("Page.screencastFrameAck");
    c.frame(0, 200, 100, [255, 255, 255]);
    eventually("blank frame acknowledged", || c.count("Page.screencastFrameAck") == acks + 1);
    std::thread::sleep(Duration::from_millis(50));
    assert!(v.take_frame().is_none(), "the white blank frame must not show");
    assert!(!events.lock().contains(&WebEvent::Frame));

    // Commit, then the page's own frames show; ordering decides, not decoder timing.
    c.session_event(0, "Page.frameNavigated", json!({"frame": {"id": "F1", "url": "https://example.com"}}));
    c.frame(0, 200, 100, [255, 0, 255]);
    eventually("page frame shown", || v.take_frame().is_some());
    let f = v.take_frame().unwrap();
    assert!(f.rgba[0] > 240 && f.rgba[1] < 16 && f.rgba[2] > 240);
}

#[test]
fn about_blank_views_and_later_navigations_are_never_held() {
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let (v, _) = h.open("about:blank", "main");
    wait_running(&v);
    let c = h.launcher.chrome(0);
    c.frame(0, 200, 100, [255, 255, 255]);
    eventually("blank shown at once", || v.take_frame().is_some());

    // After the first commit, navigating again keeps the old frame on show until the new one.
    v.navigate("example.com");
    eventually("navigation sent", || c.count("Page.navigate") == 2);
    let before = v.take_frame().unwrap().seq;
    c.frame(0, 200, 100, [1, 2, 3]);
    eventually("later frame shown", || v.take_frame().is_some_and(|f| f.seq > before));
}

#[test]
fn held_frames_are_released_after_a_timeout_when_nothing_ever_commits() {
    // `HOLD_FRAMES` bounds how long a navigation that never commits can blank the view.
    let h = Harness::new();
    h.launcher.manual.store(true, std::sync::atomic::Ordering::SeqCst);
    let (v, _) = h.open("example.com", "main");
    wait_running(&v);
    let c = h.launcher.chrome(0);
    c.frame(0, 200, 100, [9, 9, 9]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(v.take_frame().is_none());
    // Chrome keeps sending frames while the page animates; the first one after the hold
    // expires (real time: five seconds) is shown.
    let started = std::time::Instant::now();
    crate::fake::eventually_for(Duration::from_secs(9), "shown after the hold expired", &mut || {
        c.frame(0, 200, 100, [9, 9, 9]);
        std::thread::sleep(Duration::from_millis(200));
        v.take_frame().is_some()
    });
    assert!(started.elapsed() > Duration::from_secs(3), "released far too early");
}

#[test]
fn garbage_frames_are_acknowledged_and_ignored() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let good = wait_frame(&v).seq;
    let c = h.launcher.chrome(0);
    std::thread::sleep(Duration::from_millis(50));
    let acks = c.count("Page.screencastFrameAck");
    c.session_event(0, "Page.screencastFrame", json!({"data": "@@not base64@@", "metadata": {}, "sessionId": 7}));
    c.session_event(0, "Page.screencastFrame", json!({"data": "aGVsbG8=", "metadata": {}, "sessionId": 8}));
    eventually("both acked", || c.count("Page.screencastFrameAck") == acks + 2);
    assert!(v.take_frame().unwrap().seq >= good);
    assert_eq!(v.state(), WebState::Running);
}

#[test]
fn restart_and_zoom_and_devtools_are_defaulted_on_the_trait() {
    // A view implementation written against §2.2 compiles without the additions.
    struct Minimal;
    impl WebView for Minimal {
        fn navigate(&self, _: &str) {}
        fn back(&self) {}
        fn forward(&self) {}
        fn reload(&self, _: bool) {}
        fn stop(&self) {}
        fn resize(&self, _: ViewSize) {}
        fn set_visible(&self, _: bool) {}
        fn set_focused(&self, _: bool) {}
        fn mouse(&self, _: WebMouse) {}
        fn key(&self, _: WebKey) {}
        fn insert_text(&self, _: &str) {}
        fn copy_selection(&self) {}
        fn take_frame(&self) -> Option<Arc<Frame>> {
            None
        }
        fn info(&self) -> PageInfo {
            PageInfo::default()
        }
        fn state(&self) -> WebState {
            WebState::Starting
        }
        fn cursor(&self) -> Option<String> {
            None
        }
        fn close(&self) {}
    }
    let m = Minimal;
    m.restart();
    m.set_zoom(100);
    assert_eq!(m.devtools_url(), None);
}

#[test]
fn devtools_url_is_absent_without_the_opt_in() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    assert_eq!(v.devtools_url(), None);
}

#[test]
fn page_controlled_url_is_sanitised_and_capped() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    eventually("first load done", || v.info().title == "Title of https://example.com" && !v.info().loading);
    let evil = format!("https://x.example/\x1b]52;c;AAAA\x07\u{202e}{}", "a".repeat(10_000));
    c.session_event(0, "Page.frameNavigated", json!({"frame": {"id": "F", "url": evil}}));
    eventually("url shown", || v.info().url.starts_with("https://x.example/"));
    let url = v.info().url;
    assert!(url.chars().count() <= MAX_URL_CHARS, "{}", url.len());
    assert!(!url.chars().any(|ch| ch.is_control() || ch == '\u{202e}'), "{url:?}");
}

#[test]
fn navigation_to_a_refused_scheme_is_ignored() {
    let h = Harness::new();
    let (v, _) = h.running("example.com", "main");
    let c = h.launcher.chrome(0);
    // The initial navigation is sent after the view runs: count from once it has loaded.
    eventually("first load done", || v.info().title == "Title of https://example.com" && !v.info().loading);
    let n = c.count("Page.navigate");
    for url in ["javascript:alert(1)", "chrome://settings", "about:flags", "devtools://devtools/x", "ftp://h/f"] {
        v.navigate(url);
    }
    v.navigate("https://ok.example/");
    eventually("the allowed one navigates", || c.count("Page.navigate") == n + 1);
    assert_eq!(c.last("Page.navigate").unwrap().params["url"], "https://ok.example/");
}
