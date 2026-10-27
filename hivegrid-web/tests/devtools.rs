//! `HIVEGRID_WEB_DEVTOOLS=1` opens a loopback DevTools port next to the pipe. Its own test
//! binary: the variable is read when the browser launches, and nothing else runs concurrently
//! in this process to see it change.

use hivegrid_web::{ApcCommand, CdpEngine, ViewSize, WebConfig, WebEngine, WebState};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HIVEGRID_CHROME") {
        return Some(PathBuf::from(p));
    }
    let p = PathBuf::from("/opt/google/chrome/chrome");
    p.exists().then_some(p)
}

fn http_get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn devtools_url_is_offered_only_with_the_opt_in() {
    let Some(chrome) = chrome() else {
        eprintln!("skipped: no Chrome found (set HIVEGRID_CHROME)");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let cfg = |profile: &str| WebConfig {
        chrome: Some(chrome.clone()),
        profile: profile.into(),
        private: false,
        data_root: root.path().join("web"),
        extra_flags: vec![],
    };
    let size = ViewSize { pixel_width: 300, pixel_height: 200, dpi: 96 };
    let wait_running = |v: &std::sync::Arc<dyn hivegrid_web::WebView>| {
        let end = Instant::now() + Duration::from_secs(30);
        while v.state() != WebState::Running {
            assert!(Instant::now() < end, "never ran: {:?}", v.state());
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // Without the variable: no port.
    std::env::remove_var("HIVEGRID_WEB_DEVTOOLS");
    let engine = CdpEngine::new();
    let off = engine.create_view("about:blank", &cfg("off"), size, Box::new(|_| {})).unwrap();
    wait_running(&off);
    assert_eq!(off.devtools_url(), None);
    assert_eq!(ApcCommand::Devtools.apply(off.as_ref()), None);
    off.close();

    // With it: a loopback inspector URL for this very page, served next to the pipe.
    std::env::set_var("HIVEGRID_WEB_DEVTOOLS", "1");
    let on = engine
        .create_view("data:text/html,<title>inspect me</title>", &cfg("on"), size, Box::new(|_| {}))
        .unwrap();
    wait_running(&on);
    let url = on.devtools_url().expect("devtools url with the opt-in");
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert!(url.contains("/devtools/inspector.html?ws=127.0.0.1:"), "{url}");
    assert_eq!(ApcCommand::Devtools.apply(on.as_ref()).as_deref(), Some(url.as_str()));

    let port: u16 = url["http://127.0.0.1:".len()..].split('/').next().unwrap().parse().unwrap();
    let target = url.rsplit('/').next().unwrap();
    let list = http_get(port, "/json/list");
    assert!(list.contains(target), "the page is listed by the DevTools port: {list}");
    on.close();
    std::env::remove_var("HIVEGRID_WEB_DEVTOOLS");
}
