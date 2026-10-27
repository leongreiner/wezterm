use super::*;

#[derive(Default)]
struct FakeState {
    calls: Vec<String>,
    mouse: Vec<WebMouse>,
    keys: Vec<WebKey>,
    info: PageInfo,
    state: Option<WebState>,
    cursor: Option<String>,
    frame: Option<Arc<Frame>>,
    devtools: Option<String>,
}

#[derive(Default)]
struct FakeView {
    st: Mutex<FakeState>,
}

impl FakeView {
    fn call(&self, c: impl Into<String>) {
        self.st.lock().calls.push(c.into());
    }
    fn calls(&self) -> Vec<String> {
        self.st.lock().calls.clone()
    }
    fn mouse(&self) -> Vec<WebMouse> {
        self.st.lock().mouse.clone()
    }
    fn set_info(&self, url: &str, title: &str) {
        let mut st = self.st.lock();
        st.info.url = url.to_string();
        st.info.title = title.to_string();
    }
    fn set_state(&self, s: WebState) {
        self.st.lock().state = Some(s);
    }
}

impl WebView for FakeView {
    fn navigate(&self, url: &str) {
        self.call(format!("navigate {url}"));
    }
    fn back(&self) {
        self.call("back");
    }
    fn forward(&self) {
        self.call("forward");
    }
    fn reload(&self, hard: bool) {
        self.call(format!("reload {hard}"));
    }
    fn stop(&self) {
        self.call("stop");
    }
    fn resize(&self, size: ViewSize) {
        self.call(format!(
            "resize {}x{}@{}",
            size.pixel_width, size.pixel_height, size.dpi
        ));
    }
    fn set_visible(&self, visible: bool) {
        self.call(format!("visible {visible}"));
    }
    fn set_focused(&self, focused: bool) {
        self.call(format!("focused {focused}"));
    }
    fn mouse(&self, ev: WebMouse) {
        self.st.lock().mouse.push(ev);
    }
    fn key(&self, ev: WebKey) {
        self.st.lock().keys.push(ev);
    }
    fn insert_text(&self, text: &str) {
        self.call(format!("insert {text}"));
    }
    fn copy_selection(&self) {
        self.call("copy_selection");
    }
    fn take_frame(&self) -> Option<Arc<Frame>> {
        self.st.lock().frame.clone()
    }
    fn info(&self) -> PageInfo {
        self.st.lock().info.clone()
    }
    fn state(&self) -> WebState {
        self.st.lock().state.clone().unwrap_or(WebState::Running)
    }
    fn cursor(&self) -> Option<String> {
        self.st.lock().cursor.clone()
    }
    fn close(&self) {
        self.call("close");
    }
    fn restart(&self) {
        self.call("restart");
    }
    fn set_zoom(&self, percent: u32) {
        self.call(format!("zoom {percent}"));
    }
    fn devtools_url(&self) -> Option<String> {
        self.st.lock().devtools.clone()
    }
}

type Notify = Box<dyn Fn(WebEvent) + Send + Sync>;

struct FakeEngine {
    view: Arc<FakeView>,
    notify: Mutex<Option<Notify>>,
    created: Mutex<Vec<(String, WebConfig, ViewSize)>>,
}

impl FakeEngine {
    fn new() -> Self {
        Self {
            view: Arc::new(FakeView::default()),
            notify: Mutex::new(None),
            created: Mutex::new(vec![]),
        }
    }
    fn emit(&self, e: WebEvent) {
        (self.notify.lock().as_ref().expect("view created"))(e);
    }
}

impl WebEngine for FakeEngine {
    fn create_view(
        &self,
        url: &str,
        cfg: &WebConfig,
        size: ViewSize,
        notify: Box<dyn Fn(WebEvent) + Send + Sync>,
    ) -> Result<Arc<dyn WebView>> {
        self.created
            .lock()
            .push((url.to_string(), cfg.clone(), size));
        *self.notify.lock() = Some(notify);
        let view: Arc<dyn WebView> = self.view.clone();
        Ok(view)
    }
}

#[derive(Default)]
struct FakeHost {
    notes: Mutex<Vec<MuxNotification>>,
    pruned: Mutex<usize>,
    /// While set, queued `PaneOutput`s stay "in flight" (the main thread is busy).
    hold: AtomicBool,
    held: Mutex<Vec<Arc<AtomicBool>>>,
}

impl FakeHost {
    /// The main thread catches up.
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        for p in self.held.lock().drain(..) {
            p.store(false, Ordering::SeqCst);
        }
    }
}

impl WebHost for FakeHost {
    fn notify(&self, n: MuxNotification) {
        self.notes.lock().push(n);
    }
    fn pane_output(&self, pane_id: PaneId, pending: &Arc<AtomicBool>) {
        if pending.swap(true, Ordering::SeqCst) {
            return;
        }
        self.notes.lock().push(MuxNotification::PaneOutput(pane_id));
        if self.hold.load(Ordering::SeqCst) {
            self.held.lock().push(Arc::clone(pending));
        } else {
            pending.store(false, Ordering::SeqCst);
        }
    }
    fn prune(&self) {
        *self.pruned.lock() += 1;
    }
}

impl FakeHost {
    fn clipboard(&self) -> Vec<Option<String>> {
        self.notes
            .lock()
            .iter()
            .filter_map(|n| match n {
                MuxNotification::AssignClipboard { clipboard, .. } => Some(clipboard.clone()),
                _ => None,
            })
            .collect()
    }
}

struct Fixture {
    engine: FakeEngine,
    host: Arc<FakeHost>,
    pane: WebPane,
}

fn size(cols: usize, rows: usize, pw: usize, ph: usize, dpi: u32) -> TerminalSize {
    TerminalSize {
        cols,
        rows,
        pixel_width: pw,
        pixel_height: ph,
        dpi,
    }
}

fn spawn_of(url: &str) -> WebSpawn {
    WebSpawn {
        url: url.to_string(),
        profile: "main".to_string(),
        private: false,
    }
}

fn fixture_sized(s: TerminalSize) -> Fixture {
    let engine = FakeEngine::new();
    let host = Arc::new(FakeHost::default());
    let pane = WebPane::new(
        &engine,
        host.clone(),
        7,
        3,
        s,
        spawn_of("https://example.com/a?b=c"),
    )
    .unwrap();
    Fixture { engine, host, pane }
}

/// 80x24 cells of 10x20 px at scale 1.
fn fixture() -> Fixture {
    fixture_sized(size(80, 24, 800, 480, 96))
}

fn mouse(
    kind: MouseEventKind,
    button: MouseButton,
    x: usize,
    y: i64,
    xo: isize,
    yo: isize,
) -> MouseEvent {
    MouseEvent {
        kind,
        x,
        y,
        x_pixel_offset: xo,
        y_pixel_offset: yo,
        button,
        modifiers: KeyModifiers::NONE,
    }
}

fn text_of(pane: &WebPane) -> Vec<String> {
    let (_, lines) = pane.get_lines(0..24);
    lines
        .iter()
        .map(|l| l.as_str().trim_end().to_string())
        .collect()
}

#[test]
fn spawn_command_round_trips_through_parse_spawn() {
    let cmd = spawn_command("https://x.test/p", Some("work"), true);
    assert_eq!(
        parse_spawn(&cmd),
        Some(WebSpawn {
            url: "https://x.test/p".into(),
            profile: "work".into(),
            private: true
        })
    );
    let cmd = spawn_command("https://x.test/", None, false);
    assert_eq!(
        parse_spawn(&cmd),
        Some(WebSpawn {
            url: "https://x.test/".into(),
            profile: "main".into(),
            private: false
        })
    );
}

#[test]
fn parse_spawn_defaults_and_rejects() {
    let cmd = CommandBuilder::from_argv(vec![SPAWN_SENTINEL.into()]);
    assert_eq!(parse_spawn(&cmd).unwrap().url, "about:blank");
    let cmd = CommandBuilder::from_argv(vec![SPAWN_SENTINEL.into(), "  ".into()]);
    assert_eq!(parse_spawn(&cmd).unwrap().url, "about:blank");
    assert_eq!(
        parse_spawn(&CommandBuilder::from_argv(vec!["bash".into()])),
        None
    );
    assert_eq!(parse_spawn(&CommandBuilder::new_default_prog()), None);
    // The sentinel only counts as the program, not as an argument.
    assert_eq!(
        parse_spawn(&CommandBuilder::from_argv(vec![
            "echo".into(),
            SPAWN_SENTINEL.into()
        ])),
        None
    );
}

#[test]
fn parse_spawn_ignores_the_ambient_environment() {
    // `CommandBuilder::new` copies the process environment as base variables; only variables
    // set on the command count.
    std::env::set_var(ENV_PROFILE, "ambient");
    let mut cmd = CommandBuilder::new(SPAWN_SENTINEL);
    cmd.arg("https://a.test/");
    std::env::remove_var(ENV_PROFILE);
    assert_eq!(
        cmd.get_env(ENV_PROFILE).and_then(|v| v.to_str()),
        Some("ambient")
    );
    assert_eq!(parse_spawn(&cmd).unwrap().profile, "main");
    cmd.env(ENV_PROFILE, "set");
    assert_eq!(parse_spawn(&cmd).unwrap().profile, "set");
}

#[test]
fn spawn_without_an_engine_explains_itself() {
    // The test binary never calls `enable`.
    assert!(!is_enabled());
    let err = spawn(1, size(10, 5, 100, 100, 96), spawn_of("about:blank"))
        .err()
        .expect("no engine enabled");
    assert!(
        format!("{err:#}").contains("Hivegrid GUI"),
        "{err:#}",
        err = err
    );
}

#[test]
fn creation_passes_url_profile_and_size_to_the_engine() {
    let engine = FakeEngine::new();
    let host = Arc::new(FakeHost::default());
    let spawn = WebSpawn {
        url: "https://e.test/".into(),
        profile: "p1".into(),
        private: true,
    };
    // No pixel size: 10x20 px cells and 96 dpi are assumed.
    let pane = WebPane::new(&engine, host, 1, 1, size(40, 10, 0, 0, 0), spawn).unwrap();
    let created = engine.created.lock();
    let (url, cfg, vs) = &created[0];
    assert_eq!(url, "https://e.test/");
    assert_eq!(cfg.profile, "p1");
    assert!(cfg.private);
    assert!(
        cfg.data_root.ends_with("hivegrid/web"),
        "{:?}",
        cfg.data_root
    );
    assert_eq!(
        *vs,
        ViewSize {
            pixel_width: 400,
            pixel_height: 200,
            dpi: 96
        }
    );
    assert_eq!(pane.view_pixel_size(), (400, 200));
}

#[test]
fn dimensions_follow_resize_and_have_no_scrollback() {
    let f = fixture();
    let d = f.pane.get_dimensions();
    assert_eq!((d.cols, d.viewport_rows), (80, 24));
    assert_eq!((d.pixel_width, d.pixel_height, d.dpi), (800, 480, 96));
    assert_eq!(d.scrollback_rows, d.viewport_rows);
    assert_eq!(d.physical_top, 0);

    f.pane.resize(size(50, 10, 750, 300, 144)).unwrap();
    let d = f.pane.get_dimensions();
    assert_eq!((d.cols, d.viewport_rows), (50, 10));
    assert_eq!((d.pixel_width, d.pixel_height, d.dpi), (750, 300, 144));
    assert_eq!(d.scrollback_rows, d.viewport_rows);
    assert_eq!(f.pane.view_pixel_size(), (750, 300));
    assert_eq!(f.engine.view.calls(), vec!["resize 750x300@144"]);

    // Same size again: nothing to tell the view.
    f.pane.resize(size(50, 10, 750, 300, 144)).unwrap();
    assert_eq!(f.engine.view.calls().len(), 1);
    // Zero pixel size falls back to 10x20 cells.
    f.pane.resize(size(20, 5, 0, 0, 96)).unwrap();
    assert_eq!(f.pane.view_pixel_size(), (200, 100));
}

#[test]
fn placeholder_shows_url_title_and_state() {
    let f = fixture();
    let t = text_of(&f.pane);
    assert_eq!(t[0], "https://example.com/a?b=c");
    assert_eq!(t[1], "");

    f.engine.view.set_info("https://example.org/x", "Example");
    f.engine.view.st.lock().info.loading = true;
    let t = text_of(&f.pane);
    assert_eq!(&t[..3], ["https://example.org/x", "Example", "loading…"]);

    f.engine.view.set_state(WebState::Starting);
    assert_eq!(text_of(&f.pane)[2], "starting browser…");

    f.engine.view.set_state(WebState::Crashed {
        reason: "exited with signal 9".into(),
    });
    let t = text_of(&f.pane);
    assert_eq!(t[2], "Press Enter to restart the browser.");
    assert_eq!(t[3], "Browser engine exited: exited with signal 9");
    // The previous text is gone, not appended to.
    assert_eq!(t[4], "");
}

#[test]
fn crash_reason_with_log_lines_keeps_the_instruction_visible() {
    let f = fixture();
    f.engine
        .view
        .set_info("data:text/html,".to_string().as_str(), "");
    f.engine.view.set_state(WebState::Crashed {
        reason: "Browser engine exited: killed by signal 9 (SIGKILL): WARNING: x | I0000 line two | E1 line three"
            .into(),
    });
    let t = text_of(&f.pane);
    assert_eq!(t[2], "Press Enter to restart the browser.");
    // The engine's own wording is not repeated; the log lines get a row each.
    assert!(
        t[3].starts_with("Browser engine exited: killed by signal 9"),
        "{:?}",
        t
    );
    assert!(!t[3].contains("Browser engine exited: Browser engine exited"));
    let text = t.join("\n");
    assert!(
        text.contains("I0000 line two") && text.contains("E1 line three"),
        "{}",
        text
    );
}

#[test]
fn a_huge_url_takes_one_row() {
    let f = fixture();
    let url = format!("data:text/html,{}", "%3Cb%3E".repeat(500));
    f.engine.view.set_info(&url, &"T".repeat(200));
    f.engine.view.set_state(WebState::Crashed {
        reason: "gone".into(),
    });
    let t = text_of(&f.pane);
    assert!(
        t[0].starts_with("data:text/html,%3Cb%3E") && t[0].ends_with('…'),
        "{:?}",
        t[0]
    );
    assert_eq!(t[0].chars().count(), 80);
    assert_eq!(t[1].chars().count(), 80);
    assert_eq!(t[2], "Press Enter to restart the browser.");
    // The whole URL is still the pane's directory.
    assert_eq!(
        f.pane
            .get_current_working_dir(CachePolicy::AllowStale)
            .unwrap()
            .as_str(),
        url
    );
}

#[test]
fn placeholder_text_is_never_interpreted_as_escapes() {
    let f = fixture();
    f.engine
        .view
        .set_info("https://e.test/", "evil\x1b[2Jtitle\r\nsecond");
    let t = text_of(&f.pane);
    assert_eq!(t[1], "evil[2Jtitlesecond");
}

#[test]
fn placeholder_is_a_logical_line_source() {
    let f = fixture();
    let lines = f.pane.get_logical_lines(0..3);
    assert!(!lines.is_empty());
    assert!(lines[0]
        .logical
        .as_str()
        .starts_with("https://example.com/a?b=c"));
}

#[test]
fn placeholder_survives_a_tiny_pane() {
    let f = fixture_sized(size(5, 1, 50, 20, 96));
    f.engine.view.set_state(WebState::Crashed {
        reason: "boom\nline two".into(),
    });
    // One row: only the URL fits; no scrollback is built up.
    let d = f.pane.get_dimensions();
    assert_eq!((d.viewport_rows, d.scrollback_rows), (1, 1));
    let (_, lines) = f.pane.get_lines(0..1);
    assert_eq!(lines.len(), 1);
}

#[test]
fn metadata() {
    let f = fixture();
    assert_eq!(f.pane.pane_id(), 7);
    assert_eq!(f.pane.domain_id(), 3);
    assert_eq!(f.pane.tty_name(), None);
    assert!(f.pane.is_mouse_grabbed());
    assert!(!f.pane.is_alt_screen_active());
    assert!(f.pane.can_close_without_prompting(CloseReason::Window));
    assert!(f.pane.reader().unwrap().is_none());
    assert_eq!(
        f.pane.copy_user_vars(),
        HashMap::from([("hivegrid_web".to_string(), "1".to_string())])
    );
    // Not chrome and not fixed width: a grid pane for tab.rs.
    assert!(!f.pane.copy_user_vars().contains_key("hivegrid_chrome"));
    assert!(!f.pane.copy_user_vars().contains_key("hivegrid_fixed_width"));
    assert_eq!(
        f.pane.get_foreground_process_name(CachePolicy::AllowStale),
        None
    );
}

#[test]
fn title_prefers_the_page_title_then_the_url() {
    let f = fixture();
    assert_eq!(f.pane.get_title(), "https://example.com/a?b=c");
    f.engine.view.set_info("https://example.org/", "");
    assert_eq!(f.pane.get_title(), "https://example.org/");
    f.engine.view.set_info("https://example.org/", "Org");
    assert_eq!(f.pane.get_title(), "Org");
}

#[test]
fn cwd_is_the_page_url_unless_it_is_a_file() {
    let f = fixture();
    assert_eq!(
        f.pane
            .get_current_working_dir(CachePolicy::AllowStale)
            .unwrap()
            .as_str(),
        "https://example.com/a?b=c"
    );
    f.engine.view.set_info("https://example.org/x", "T");
    assert_eq!(
        f.pane
            .get_current_working_dir(CachePolicy::AllowStale)
            .unwrap()
            .as_str(),
        "https://example.org/x"
    );
    f.engine.view.set_info("data:text/html,<b>hi</b>", "");
    assert_eq!(
        f.pane
            .get_current_working_dir(CachePolicy::AllowStale)
            .unwrap()
            .scheme(),
        "data"
    );
    f.engine.view.set_info("file:///tmp/x.html", "");
    assert_eq!(
        f.pane.get_current_working_dir(CachePolicy::AllowStale),
        None
    );
    f.engine.view.set_info("not a url", "");
    assert_eq!(
        f.pane.get_current_working_dir(CachePolicy::AllowStale),
        None
    );
}

#[test]
fn mouse_position_math_uses_cell_size_offset_and_dpi() {
    // 100x40 cells of 12x24 px at 1.5x: pointer in cell (3, 2) at offset (5, 7).
    let f = fixture_sized(size(100, 40, 1200, 960, 144));
    f.pane
        .mouse_event(mouse(MouseEventKind::Move, MouseButton::None, 3, 2, 5, 7))
        .unwrap();
    let m = f.engine.view.mouse();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].kind, WebMouseKind::Move);
    // physical (3*12+5, 2*24+7) = (41, 55) -> CSS / 1.5
    assert!((m[0].x - 41.0 / 1.5).abs() < 1e-3, "{}", m[0].x);
    assert!((m[0].y - 55.0 / 1.5).abs() < 1e-3, "{}", m[0].y);
}

#[test]
fn mouse_position_clamps_negative_offsets_and_rows() {
    let size = size(80, 24, 800, 480, 96);
    let ev = mouse(MouseEventKind::Move, MouseButton::None, 0, -1, -30, -9);
    assert_eq!(pane_pixel_position(&size, &ev), (0.0, 0.0));
    let ev = mouse(MouseEventKind::Move, MouseButton::None, 79, 23, 9, 19);
    assert_eq!(pane_pixel_position(&size, &ev), (799.0, 479.0));
}

#[test]
fn clicks_carry_dom_button_buttons_and_click_count() {
    let f = fixture();
    let t0 = Instant::now();
    let press = |ms: u64, x: usize| {
        f.pane
            .mouse_event_at(
                mouse(MouseEventKind::Press, MouseButton::Left, x, 1, 2, 3),
                t0 + Duration::from_millis(ms),
            )
            .unwrap();
    };
    let release = |ms: u64| {
        f.pane
            .mouse_event_at(
                mouse(MouseEventKind::Release, MouseButton::Left, 4, 1, 2, 3),
                t0 + Duration::from_millis(ms),
            )
            .unwrap();
    };
    press(0, 4);
    release(30);
    press(200, 4);
    release(230);
    press(350, 4);
    release(380);
    // A fourth press in time restarts the cycle (triple is the longest streak).
    press(500, 4);
    // Too late.
    release(520);
    press(2000, 4);
    // Too far.
    press(2100, 9);
    let m = f.engine.view.mouse();
    let kinds: Vec<_> = m.iter().map(|m| (m.kind, m.click_count)).collect();
    use WebMouseKind::*;
    assert_eq!(
        kinds,
        vec![
            (Down, 1),
            (Up, 1),
            (Down, 2),
            (Up, 2),
            (Down, 3),
            (Up, 3),
            (Down, 1),
            (Up, 1),
            (Down, 1),
            (Down, 1)
        ]
    );
    assert_eq!(m[0].button, 0);
    assert_eq!(
        m[0].buttons, 1,
        "the pressed button is held during mousedown"
    );
    assert_eq!(m[1].buttons, 0, "and released at mouseup");
    assert_eq!((m[0].x, m[0].y), (42.0, 23.0));
}

#[test]
fn right_and_middle_buttons_map_to_dom_numbers() {
    let f = fixture();
    f.pane
        .mouse_event(mouse(MouseEventKind::Press, MouseButton::Right, 1, 1, 0, 0))
        .unwrap();
    f.pane
        .mouse_event(mouse(
            MouseEventKind::Press,
            MouseButton::Middle,
            1,
            1,
            0,
            0,
        ))
        .unwrap();
    let m = f.engine.view.mouse();
    assert_eq!((m[0].button, m[0].buttons), (2, 2));
    assert_eq!((m[1].button, m[1].buttons), (1, 6), "right and middle held");
}

#[test]
fn drag_moves_report_the_held_buttons() {
    let f = fixture();
    f.pane
        .mouse_event(mouse(MouseEventKind::Press, MouseButton::Left, 1, 1, 0, 0))
        .unwrap();
    f.pane
        .mouse_event(mouse(MouseEventKind::Move, MouseButton::Left, 2, 1, 0, 0))
        .unwrap();
    f.pane
        .mouse_event(mouse(
            MouseEventKind::Release,
            MouseButton::Left,
            2,
            1,
            0,
            0,
        ))
        .unwrap();
    f.pane
        .mouse_event(mouse(MouseEventKind::Move, MouseButton::None, 3, 1, 0, 0))
        .unwrap();
    let m = f.engine.view.mouse();
    assert_eq!(m[1].kind, WebMouseKind::Move);
    assert_eq!(m[1].buttons, 1);
    assert_eq!(m[3].buttons, 0);
}

#[test]
fn modifiers_become_cdp_bits() {
    let f = fixture();
    let mut ev = mouse(MouseEventKind::Move, MouseButton::None, 1, 1, 0, 0);
    ev.modifiers = KeyModifiers::CTRL | KeyModifiers::SHIFT;
    f.pane.mouse_event(ev).unwrap();
    assert_eq!(
        f.engine.view.mouse()[0].mods,
        hivegrid_web::keys::MOD_CTRL | hivegrid_web::keys::MOD_SHIFT
    );
}

#[test]
fn x11_wheel_notches_scroll_100_css_px_each() {
    // X11 reports 5 lines per notch.
    let f = fixture();
    for button in [
        MouseButton::WheelUp(5),
        MouseButton::WheelDown(15),
        MouseButton::WheelLeft(10),
        MouseButton::WheelRight(5),
    ] {
        f.pane
            .mouse_event(mouse(MouseEventKind::Press, button, 5, 5, 0, 0))
            .unwrap();
    }
    // The release of a wheel "button" is not an event for the page.
    f.pane
        .mouse_event(mouse(
            MouseEventKind::Release,
            MouseButton::WheelUp(5),
            5,
            5,
            0,
            0,
        ))
        .unwrap();
    let m = f.engine.view.mouse();
    assert_eq!(m.len(), 4);
    assert!(m.iter().all(|m| m.kind == WebMouseKind::Wheel));
    assert_eq!((m[0].dx, m[0].dy), (0.0, -100.0));
    assert_eq!((m[1].dx, m[1].dy), (0.0, 300.0));
    assert_eq!((m[2].dx, m[2].dy), (-200.0, 0.0));
    assert_eq!((m[3].dx, m[3].dy), (100.0, 0.0));
    assert_eq!((m[0].x, m[0].y), (50.0, 100.0));
}

#[test]
fn wayland_wheel_amounts_are_device_pixels() {
    // A notch is ~10 units of the pointer axis times the window scale: 100 css px at any scale.
    assert_eq!(wheel_css_px(10, WheelUnits::Pixels, 1.0), 100.0);
    assert_eq!(wheel_css_px(20, WheelUnits::Pixels, 2.0), 100.0);
    assert_eq!(wheel_css_px(15, WheelUnits::Pixels, 1.5), 100.0);
    // Smooth scrolling arrives in small pieces and adds up to the same distance.
    let parts: f32 = (0..10)
        .map(|_| wheel_css_px(1, WheelUnits::Pixels, 1.0))
        .sum();
    assert_eq!(parts, 100.0);
    // Lines (X11): 5 per notch, whatever the scale.
    assert_eq!(wheel_css_px(5, WheelUnits::Lines, 1.0), 100.0);
    assert_eq!(wheel_css_px(5, WheelUnits::Lines, 2.0), 100.0);
}

#[test]
fn wheel_delta_does_not_depend_on_the_scale() {
    // The page scrolls by CSS pixels; the delta per notch is the same at any scale.
    let f = fixture_sized(size(80, 24, 1600, 960, 192));
    f.pane
        .mouse_event(mouse(
            MouseEventKind::Press,
            MouseButton::WheelDown(5),
            10,
            10,
            0,
            0,
        ))
        .unwrap();
    let m = f.engine.view.mouse();
    assert_eq!(m[0].dy, 100.0);
    assert_eq!((m[0].x, m[0].y), (100.0, 200.0));
}

#[test]
fn leaving_the_pane_moves_the_pointer_out() {
    let f = fixture();
    f.pane.mouse_leave();
    let m = f.engine.view.mouse();
    assert_eq!(
        (m[0].kind, m[0].x, m[0].y),
        (WebMouseKind::Move, -1.0, -1.0)
    );
}

#[test]
fn pointer_shape_is_the_views_cursor() {
    let f = fixture();
    assert_eq!(f.pane.mouse_pointer_shape(), None);
    f.engine.view.st.lock().cursor = Some("pointer".into());
    assert_eq!(f.pane.mouse_pointer_shape().as_deref(), Some("pointer"));
}

#[test]
fn keys_are_forwarded_down_and_up() {
    let f = fixture();
    f.pane
        .key_down(
            KeyCode::Char('a'),
            KeyModifiers::LEFT_SHIFT | KeyModifiers::SHIFT,
        )
        .unwrap();
    f.pane
        .key_up(KeyCode::Char('a'), KeyModifiers::NONE)
        .unwrap();
    let keys = f.engine.view.st.lock().keys.clone();
    assert_eq!(
        keys,
        vec![
            WebKey {
                down: true,
                key: KeyCode::Char('a'),
                // positional bits are dropped like every other pane does
                mods: KeyModifiers::SHIFT
            },
            WebKey {
                down: false,
                key: KeyCode::Char('a'),
                mods: KeyModifiers::NONE
            }
        ]
    );
    assert!(f.engine.view.calls().is_empty());
}

#[test]
fn copy_keys_ask_the_view_for_the_selection() {
    let f = fixture();
    f.pane
        .key_down(KeyCode::Char('c'), KeyModifiers::CTRL)
        .unwrap();
    f.pane
        .key_down(KeyCode::Insert, KeyModifiers::CTRL)
        .unwrap();
    // Not copies: plain c, Ctrl+Shift+C, Ctrl+V.
    f.pane
        .key_down(KeyCode::Char('c'), KeyModifiers::NONE)
        .unwrap();
    f.pane
        .key_down(KeyCode::Char('C'), KeyModifiers::CTRL | KeyModifiers::SHIFT)
        .unwrap();
    f.pane
        .key_down(KeyCode::Char('v'), KeyModifiers::CTRL)
        .unwrap();
    assert_eq!(
        f.engine.view.calls(),
        vec!["copy_selection", "copy_selection"]
    );
    // The keys still reach the page (a web app may handle Ctrl+C itself).
    assert_eq!(f.engine.view.st.lock().keys.len(), 5);

    // The selection text comes back as an event and lands on the clipboard.
    f.engine.emit(WebEvent::ClipboardText("picked".into()));
    assert_eq!(f.host.clipboard(), vec![Some("picked".to_string())]);
}

#[test]
fn paste_and_composed_text_insert_text() {
    let f = fixture();
    f.pane.send_paste("pasted\ntext").unwrap();
    f.pane.send_composed_text("é").unwrap();
    assert_eq!(
        f.engine.view.calls(),
        vec!["insert pasted\ntext", "insert é"]
    );
}

#[test]
fn focus_and_visibility_reach_the_view() {
    let f = fixture();
    f.pane.focus_changed(true);
    f.pane.focus_changed(false);
    f.pane.set_visible(false);
    f.pane.set_visible(true);
    assert_eq!(
        f.engine.view.calls(),
        vec![
            "focused true",
            "focused false",
            "visible false",
            "visible true"
        ]
    );
}

#[test]
fn writer_runs_apc_commands() {
    let f = fixture();
    {
        let mut w = f.pane.writer();
        // A command split over writes, plain text around it and a foreign sequence.
        w.write_all(b"hello \x1b_hgweb;navigate;https://exa")
            .unwrap();
        w.write_all(b"mple.org/?a=b;c\x1b\\ typed text").unwrap();
        w.write_all(b"\x1b_hgweb;back\x1b\\\x1b_hgweb;forward\x1b\\")
            .unwrap();
        w.write_all(b"\x1b_hgweb;reload\x1b\\\x1b_hgweb;hardreload\x1b\\")
            .unwrap();
        w.write_all(b"\x1b_hgweb;stop\x1b\\\x1b_hgweb;restart\x1b\\")
            .unwrap();
        w.write_all(b"\x1b_hgweb;zoom;125\x1b\\\x1b_other;x\x1b\\")
            .unwrap();
        w.write_all(b"\x1b_hgweb;bogus\x1b\\").unwrap();
        w.flush().unwrap();
    }
    assert_eq!(
        f.engine.view.calls(),
        vec![
            "navigate https://example.org/?a=b;c",
            "back",
            "forward",
            "reload false",
            "reload true",
            "stop",
            "restart",
            "zoom 125",
        ]
    );
}

#[test]
fn writer_copy_url_and_devtools_use_the_clipboard() {
    let f = fixture();
    f.engine.view.set_info("https://example.org/page", "T");
    let mut w = f.pane.writer();
    w.write_all(b"\x1b_hgweb;copy-url\x1b\\").unwrap();
    // No devtools URL (the feature is off): nothing is copied.
    w.write_all(b"\x1b_hgweb;devtools\x1b\\").unwrap();
    drop(w);
    f.engine.view.st.lock().devtools = Some("http://127.0.0.1:9222/devtools/x".into());
    f.pane
        .writer()
        .write_all(b"\x1b_hgweb;devtools\x1b\\")
        .unwrap();
    assert_eq!(
        f.host.clipboard(),
        vec![
            Some("https://example.org/page".to_string()),
            Some("http://127.0.0.1:9222/devtools/x".to_string())
        ]
    );
}

#[test]
fn enter_restarts_a_crashed_browser_and_nothing_else_is_forwarded() {
    let f = fixture();
    f.engine.view.set_state(WebState::Crashed {
        reason: "gone".into(),
    });
    f.pane
        .key_down(KeyCode::Char('x'), KeyModifiers::NONE)
        .unwrap();
    assert!(f.engine.view.calls().is_empty());
    f.pane.key_down(KeyCode::Enter, KeyModifiers::NONE).unwrap();
    f.pane.key_up(KeyCode::Enter, KeyModifiers::NONE).unwrap();
    assert_eq!(f.engine.view.calls(), vec!["restart"]);
    assert!(f.engine.view.st.lock().keys.is_empty());
    assert!(!f.pane.is_dead(), "a crashed browser keeps its pane");

    // Back to normal after the restart: Enter is an ordinary key again.
    f.engine.view.set_state(WebState::Running);
    f.pane.key_down(KeyCode::Enter, KeyModifiers::NONE).unwrap();
    assert_eq!(f.engine.view.calls(), vec!["restart"]);
    assert_eq!(f.engine.view.st.lock().keys.len(), 1);
}

#[test]
fn frames_come_from_the_view() {
    let f = fixture();
    assert!(f.pane.frame().is_none());
    assert_eq!(f.pane.frame_seq(), 0);
    f.engine.view.st.lock().frame = Some(Arc::new(Frame {
        seq: 9,
        width: 2,
        height: 1,
        rgba: vec![0; 8],
    }));
    assert_eq!(f.pane.frame_seq(), 9);
    assert_eq!(f.pane.frame().unwrap().width, 2);
}

#[test]
fn kill_closes_the_view_once_and_marks_the_pane_dead() {
    let f = fixture();
    assert!(!f.pane.is_dead());
    f.pane.kill();
    f.pane.kill();
    assert!(f.pane.is_dead());
    assert_eq!(f.engine.view.calls(), vec!["close"]);
    let view = f.engine.view.clone();
    drop(f.pane);
    assert_eq!(
        view.calls(),
        vec!["close"],
        "drop after kill does not close twice"
    );
}

#[test]
fn dropping_a_live_pane_closes_its_page() {
    let f = fixture();
    let view = f.engine.view.clone();
    drop(f.pane);
    assert_eq!(view.calls(), vec!["close"]);
}

#[test]
fn page_closing_itself_kills_the_pane() {
    let f = fixture();
    assert!(!f.pane.is_dead());
    f.engine.emit(WebEvent::Closed);
    assert!(f.pane.is_dead());
    assert_eq!(*f.host.pruned.lock(), 1);
}

#[test]
fn events_notify_the_mux() {
    let f = fixture();
    f.engine.emit(WebEvent::Frame);
    f.engine.emit(WebEvent::State);
    f.engine.emit(WebEvent::Cursor("text".into()));
    f.engine.emit(WebEvent::Info);
    let notes = f.host.notes.lock();
    let outputs = notes
        .iter()
        .filter(|n| matches!(n, MuxNotification::PaneOutput(7)))
        .count();
    assert_eq!(outputs, 4);
    assert!(notes.iter().any(|n| matches!(
        n,
        MuxNotification::Alert {
            pane_id: 7,
            alert: Alert::CurrentWorkingDirectoryChanged
        }
    )));
}

#[test]
fn other_pane_methods_are_harmless() {
    let f = fixture();
    assert_eq!(f.pane.get_cursor_position().x, 0);
    let before = f.pane.get_current_seqno();
    f.engine.view.set_info("https://e.test/", "changed");
    // A change of text moves the sequence number so the renderer redraws.
    assert!(f.pane.get_current_seqno() > before);
    assert!(f.pane.get_config().is_some());
    let _ = f.pane.palette();
}

#[test]
fn resolve_cwd_only_uses_file_urls() {
    let mux = Mux::new(None);
    let f = fixture();
    let pane: Arc<dyn Pane> = Arc::new(f.pane);
    // A page URL is not a directory: the next terminal split must not inherit its path.
    assert_eq!(
        mux.resolve_cwd(None, Some(pane.clone()), 3, CachePolicy::AllowStale),
        None
    );
    // An explicit directory still wins.
    assert_eq!(
        mux.resolve_cwd(
            Some("/tmp".into()),
            Some(pane.clone()),
            3,
            CachePolicy::AllowStale
        )
        .as_deref(),
        Some("/tmp")
    );
    // A pane in another domain is never consulted.
    assert_eq!(
        mux.resolve_cwd(None, Some(pane), 99, CachePolicy::AllowStale),
        None
    );
}

#[test]
fn hyperlink_rules_never_touch_the_placeholder() {
    let f = fixture();
    let rules = [termwiz::hyperlink::Rule::new(r"https?://\S+", "$0").unwrap()];
    f.pane.apply_hyperlinks(0..3, &rules);
    let (_, lines) = f.pane.get_lines(0..3);
    assert!(
        lines
            .iter()
            .all(|l| l.visible_cells().all(|c| c.attrs().hyperlink().is_none())),
        "the URL row must not become a link"
    );
}

#[test]
fn a_flood_of_view_events_queues_one_pane_output_at_a_time() {
    let f = fixture();
    f.host.hold.store(true, Ordering::SeqCst);
    for i in 0..10_000 {
        f.engine.emit(if i % 2 == 0 {
            WebEvent::Frame
        } else {
            WebEvent::Cursor("pointer".into())
        });
    }
    let outputs = |h: &FakeHost| {
        h.notes
            .lock()
            .iter()
            .filter(|n| matches!(n, MuxNotification::PaneOutput(7)))
            .count()
    };
    assert_eq!(
        outputs(&f.host),
        1,
        "10000 events, one task for the main thread"
    );
    // Once the main thread has caught up, the next event is delivered again.
    f.host.release();
    f.engine.emit(WebEvent::Frame);
    assert_eq!(outputs(&f.host), 2);
}

#[test]
fn long_wrapped_log_lines_never_scroll_the_url_away() {
    // 20 columns x 6 rows; the reason has lines that wrap onto several rows each.
    let f = fixture_sized(size(20, 6, 200, 120, 96));
    let long = "e".repeat(100);
    f.engine.view.set_state(WebState::Crashed {
        reason: format!("Browser engine exited: gone | {long} | {long} | {long}"),
    });
    let (_, lines) = f.pane.get_lines(0..6);
    let rows: Vec<String> = lines
        .iter()
        .map(|l| l.as_str().trim_end().to_string())
        .collect();
    assert_eq!(rows.len(), 6);
    assert_eq!(
        rows[0], "https://example.com…",
        "the URL stays on row 0: {:?}",
        rows
    );
    assert!(rows[2].starts_with("Press Enter"), "{:?}", rows);
    let d = f.pane.get_dimensions();
    assert_eq!(
        (d.physical_top, d.scrollback_rows),
        (0, 6),
        "nothing scrolled"
    );
}

#[test]
fn relative_xdg_data_home_is_ignored() {
    use std::ffi::OsString;
    use std::path::Path;
    let home = Path::new("/home/u");
    assert_eq!(
        data_root_in(Some(OsString::from("/x/data")), home),
        Path::new("/x/data/hivegrid/web")
    );
    for bad in [Some(OsString::from("rel/data")), Some(OsString::new()), None] {
        assert_eq!(data_root_in(bad, home), Path::new("/home/u/.local/share/hivegrid/web"));
    }
}

#[test]
fn spawn_refuses_schemes_outside_the_allowlist() {
    for u in ["javascript:alert(1)", "chrome://settings", "devtools://x", "about:flags", "blob:x"] {
        let cmd = CommandBuilder::from_argv(vec![SPAWN_SENTINEL.into(), u.into()]);
        assert_eq!(parse_spawn(&cmd).unwrap().url, "about:blank", "{u}");
    }
    let cmd = CommandBuilder::from_argv(vec![SPAWN_SENTINEL.into(), "example.com".into()]);
    assert_eq!(parse_spawn(&cmd).unwrap().url, "https://example.com");
}

#[test]
fn placeholder_text_is_bounded() {
    let info = PageInfo {
        url: "u".repeat(MAX_URL_CHARS * 3),
        title: "t".repeat(MAX_TITLE_CHARS * 3),
        ..Default::default()
    };
    let lines = placeholder_lines("about:blank", &info, &WebState::Running);
    assert_eq!(lines[0].chars().count(), MAX_URL_CHARS);
    assert_eq!(lines[1].chars().count(), MAX_TITLE_CHARS);
}
