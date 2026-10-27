//! The web pane's control channel: `ESC _ hgweb ; <cmd> [; <arg>] ESC \`.
//!
//! Bytes written to the pane (`pane:send_text`, `wezterm cli send-text --no-paste`) go through
//! an [`ApcParser`]; everything that is not a well-formed `hgweb` sequence is ignored, so a
//! pane that is typed into is not hurt by it. The parser is streaming: a sequence may arrive
//! split over any number of writes.

use crate::WebView;

/// Longest payload accepted (`data:` URLs can be long; a runaway sequence is dropped).
const MAX_PAYLOAD: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApcCommand {
    Navigate(String),
    Back,
    Forward,
    Reload { hard: bool },
    Stop,
    /// Relaunch the browser/page of a crashed pane.
    Restart,
    /// Put the page URL on the clipboard.
    CopyUrl,
    /// Page zoom in percent (10..=500).
    Zoom(u32),
    /// Put the DevTools inspector URL on the clipboard (only with `HIVEGRID_WEB_DEVTOOLS=1`).
    Devtools,
    /// Show, hide or toggle the pane's header row. Pane-level: the view knows nothing of it.
    Header(HeaderOp),
    /// Start typing in the pane's address field. Pane-level.
    Location,
}

/// What `header;<op>` asks of the header row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderOp {
    Toggle,
    Show,
    Hide,
}

impl ApcCommand {
    /// Parse the payload between `ESC _` and `ESC \`, e.g. `hgweb;navigate;https://x`.
    pub fn parse(payload: &[u8]) -> Option<ApcCommand> {
        let s = std::str::from_utf8(payload).ok()?;
        let rest = s.strip_prefix("hgweb;")?;
        // Only the first `;` separates the command: URLs may contain more.
        let (cmd, arg) = match rest.split_once(';') {
            Some((c, a)) => (c, Some(a)),
            None => (rest, None),
        };
        match (cmd, arg) {
            ("navigate", Some(a)) if !a.trim().is_empty() => Some(ApcCommand::Navigate(a.to_string())),
            ("back", None) => Some(ApcCommand::Back),
            ("forward", None) => Some(ApcCommand::Forward),
            ("reload", None) => Some(ApcCommand::Reload { hard: false }),
            ("hardreload", None) => Some(ApcCommand::Reload { hard: true }),
            ("stop", None) => Some(ApcCommand::Stop),
            ("restart", None) => Some(ApcCommand::Restart),
            ("copy-url", None) => Some(ApcCommand::CopyUrl),
            ("devtools", None) => Some(ApcCommand::Devtools),
            ("location", None) => Some(ApcCommand::Location),
            ("header", Some("toggle")) => Some(ApcCommand::Header(HeaderOp::Toggle)),
            ("header", Some("show")) => Some(ApcCommand::Header(HeaderOp::Show)),
            ("header", Some("hide")) => Some(ApcCommand::Header(HeaderOp::Hide)),
            ("zoom", Some(a)) => {
                let pct: u32 = a.trim().trim_end_matches('%').parse().ok()?;
                (10..=500).contains(&pct).then_some(ApcCommand::Zoom(pct))
            }
            _ => None,
        }
    }

    /// Execute on `view`. `copy-url` and `devtools` do not act on the page: they return the text
    /// the pane should put on the clipboard (`MuxNotification::AssignClipboard`). `header` and
    /// `location` belong to the pane, which handles them itself; here they do nothing.
    pub fn apply(&self, view: &dyn WebView) -> Option<String> {
        match self {
            ApcCommand::Navigate(u) => view.navigate(u),
            ApcCommand::Back => view.back(),
            ApcCommand::Forward => view.forward(),
            ApcCommand::Reload { hard } => view.reload(*hard),
            ApcCommand::Stop => view.stop(),
            ApcCommand::Restart => view.restart(),
            ApcCommand::Zoom(p) => view.set_zoom(*p),
            ApcCommand::CopyUrl => {
                let url = view.info().url;
                return (!url.is_empty()).then_some(url);
            }
            ApcCommand::Devtools => return view.devtools_url(),
            ApcCommand::Header(_) | ApcCommand::Location => {}
        }
        None
    }
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    /// Saw ESC outside a sequence.
    Esc,
    /// Inside `ESC _ ...`, collecting the payload.
    Payload,
    /// Saw ESC inside the payload; `\` ends it.
    PayloadEsc,
    /// A payload that exceeded the limit: skip to its terminator.
    Overflow,
    OverflowEsc,
}

/// Streaming parser for the control channel.
#[derive(Debug, Default)]
pub struct ApcParser {
    state: State,
    payload: Vec<u8>,
}

impl ApcParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns the commands completed by them, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ApcCommand> {
        let mut out = Vec::new();
        for &b in bytes {
            self.state = match std::mem::take(&mut self.state) {
                State::Ground => {
                    if b == 0x1b {
                        State::Esc
                    } else {
                        State::Ground
                    }
                }
                State::Esc => match b {
                    b'_' => {
                        self.payload.clear();
                        State::Payload
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                },
                State::Payload => match b {
                    0x1b => State::PayloadEsc,
                    _ => {
                        if self.payload.len() >= MAX_PAYLOAD {
                            self.payload.clear();
                            State::Overflow
                        } else {
                            self.payload.push(b);
                            State::Payload
                        }
                    }
                },
                State::PayloadEsc => match b {
                    b'\\' => {
                        if let Some(c) = ApcCommand::parse(&self.payload) {
                            out.push(c);
                        }
                        self.payload.clear();
                        State::Ground
                    }
                    // ESC ESC: the first was data we do not keep; stay armed.
                    0x1b => State::PayloadEsc,
                    // An ESC that does not terminate aborts the sequence; `_` restarts one.
                    b'_' => {
                        self.payload.clear();
                        State::Payload
                    }
                    _ => {
                        self.payload.clear();
                        State::Ground
                    }
                },
                State::Overflow => {
                    if b == 0x1b {
                        State::OverflowEsc
                    } else {
                        State::Overflow
                    }
                }
                State::OverflowEsc => match b {
                    b'\\' => State::Ground,
                    0x1b => State::OverflowEsc,
                    _ => State::Overflow,
                },
            };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(body: &str) -> Vec<u8> {
        format!("\x1b_{body}\x1b\\").into_bytes()
    }

    fn parse_all(bytes: &[u8]) -> Vec<ApcCommand> {
        ApcParser::new().feed(bytes)
    }

    #[test]
    fn every_command_parses() {
        let cases = [
            ("hgweb;navigate;https://example.com/a?b=1", ApcCommand::Navigate("https://example.com/a?b=1".into())),
            ("hgweb;back", ApcCommand::Back),
            ("hgweb;forward", ApcCommand::Forward),
            ("hgweb;reload", ApcCommand::Reload { hard: false }),
            ("hgweb;hardreload", ApcCommand::Reload { hard: true }),
            ("hgweb;stop", ApcCommand::Stop),
            ("hgweb;restart", ApcCommand::Restart),
            ("hgweb;copy-url", ApcCommand::CopyUrl),
            ("hgweb;devtools", ApcCommand::Devtools),
            ("hgweb;zoom;125", ApcCommand::Zoom(125)),
            ("hgweb;location", ApcCommand::Location),
            ("hgweb;header;toggle", ApcCommand::Header(HeaderOp::Toggle)),
            ("hgweb;header;show", ApcCommand::Header(HeaderOp::Show)),
            ("hgweb;header;hide", ApcCommand::Header(HeaderOp::Hide)),
            ("hgweb;zoom;80%", ApcCommand::Zoom(80)),
        ];
        for (body, want) in cases {
            assert_eq!(parse_all(&seq(body)), vec![want], "{body}");
        }
    }

    #[test]
    fn navigate_argument_keeps_semicolons() {
        assert_eq!(
            parse_all(&seq("hgweb;navigate;data:text/html;base64,PGI+")),
            vec![ApcCommand::Navigate("data:text/html;base64,PGI+".into())]
        );
    }

    #[test]
    fn malformed_or_foreign_sequences_are_ignored() {
        for body in [
            "hgweb",
            "hgweb;",
            "hgweb;nope",
            "hgweb;navigate",
            "hgweb;navigate;   ",
            "hgweb;back;extra",
            "hgweb;zoom",
            "hgweb;zoom;5",
            "hgweb;zoom;9999",
            "hgweb;zoom;abc",
            "other;back",
            "HGWEB;back",
        ] {
            assert!(parse_all(&seq(body)).is_empty(), "{body}");
        }
        assert!(parse_all(b"plain text \x1b[31mred\x1b[0m").is_empty());
        assert!(ApcCommand::parse(&[0xff, 0xfe]).is_none());
    }

    #[test]
    fn text_around_and_between_sequences_is_skipped() {
        let mut bytes = b"junk".to_vec();
        bytes.extend(seq("hgweb;back"));
        bytes.extend(b"more\x1b[Aiunk");
        bytes.extend(seq("hgweb;reload"));
        assert_eq!(parse_all(&bytes), vec![ApcCommand::Back, ApcCommand::Reload { hard: false }]);
    }

    #[test]
    fn sequences_may_arrive_one_byte_at_a_time() {
        let bytes = seq("hgweb;navigate;https://example.com");
        let mut p = ApcParser::new();
        let mut got = Vec::new();
        for b in &bytes {
            got.extend(p.feed(std::slice::from_ref(b)));
        }
        assert_eq!(got, vec![ApcCommand::Navigate("https://example.com".into())]);
    }

    #[test]
    fn an_unterminated_sequence_is_aborted_by_the_next_one() {
        let mut bytes = b"\x1b_hgweb;navigate;https://lost".to_vec();
        bytes.extend(seq("hgweb;stop"));
        assert_eq!(parse_all(&bytes), vec![ApcCommand::Stop]);
    }

    #[test]
    fn oversized_payloads_are_dropped_and_the_parser_recovers() {
        let mut bytes = b"\x1b_hgweb;navigate;".to_vec();
        bytes.resize(bytes.len() + MAX_PAYLOAD + 10, b'a');
        bytes.extend(b"\x1b\\");
        bytes.extend(seq("hgweb;back"));
        assert_eq!(parse_all(&bytes), vec![ApcCommand::Back]);
    }

    #[derive(Default)]
    struct Spy(parking_lot::Mutex<Vec<String>>);
    impl WebView for Spy {
        fn navigate(&self, url: &str) { self.0.lock().push(format!("navigate {url}")) }
        fn back(&self) { self.0.lock().push("back".into()) }
        fn forward(&self) { self.0.lock().push("forward".into()) }
        fn reload(&self, hard: bool) { self.0.lock().push(format!("reload {hard}")) }
        fn stop(&self) { self.0.lock().push("stop".into()) }
        fn resize(&self, _: crate::ViewSize) {}
        fn set_visible(&self, _: bool) {}
        fn set_focused(&self, _: bool) {}
        fn mouse(&self, _: crate::WebMouse) {}
        fn key(&self, _: crate::WebKey) {}
        fn insert_text(&self, _: &str) {}
        fn copy_selection(&self) {}
        fn take_frame(&self) -> Option<std::sync::Arc<crate::Frame>> { None }
        fn info(&self) -> crate::PageInfo {
            crate::PageInfo { url: "https://now.example/".into(), ..Default::default() }
        }
        fn state(&self) -> crate::WebState { crate::WebState::Running }
        fn cursor(&self) -> Option<String> { None }
        fn close(&self) {}
        fn restart(&self) { self.0.lock().push("restart".into()) }
        fn set_zoom(&self, p: u32) { self.0.lock().push(format!("zoom {p}")) }
        fn devtools_url(&self) -> Option<String> { Some("http://127.0.0.1:9/x".into()) }
    }

    #[test]
    fn apply_drives_the_view_and_returns_clipboard_text() {
        let spy = Spy::default();
        for c in [
            ApcCommand::Navigate("a.b".into()),
            ApcCommand::Back,
            ApcCommand::Forward,
            ApcCommand::Reload { hard: true },
            ApcCommand::Stop,
            ApcCommand::Restart,
            ApcCommand::Zoom(150),
        ] {
            assert_eq!(c.apply(&spy), None);
        }
        assert_eq!(
            *spy.0.lock(),
            ["navigate a.b", "back", "forward", "reload true", "stop", "restart", "zoom 150"]
        );
        assert_eq!(ApcCommand::CopyUrl.apply(&spy).as_deref(), Some("https://now.example/"));
        assert_eq!(ApcCommand::Devtools.apply(&spy).as_deref(), Some("http://127.0.0.1:9/x"));
    }
}
