//! The header row of a web pane: back, forward and reload buttons, an address/search field and a
//! collapse button, drawn as the first row of the pane's cell grid (the page texture starts one
//! row lower, see `render/web.rs`). Everything here is pure: state, hit testing, line editing and
//! the escape text of the row; `WebPane` wires it to the view.

use super::history::Completion;
use hivegrid_web::PageInfo;
use std::ops::Range;
use wezterm_term::unicode_column_width;

/// A pane needs at least this many rows for a header: below it the page keeps the whole pane.
pub const MIN_PANE_ROWS: usize = 3;
/// Columns of a button (` ‹ `).
const BUTTON: usize = 3;
/// A pane narrower than this has the field and the collapse button only.
const NAV_MIN_COLS: usize = 30;
/// A pane narrower than this has the field only.
const COLLAPSE_MIN_COLS: usize = 8;
/// What the field shows while the page has no address.
const HINT: &str = "Search or enter address";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Back,
    Forward,
    Reload,
    Field,
    Collapse,
}

/// Where the parts of the row sit, in columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub back: Option<Range<usize>>,
    pub forward: Option<Range<usize>>,
    pub reload: Option<Range<usize>>,
    pub field: Range<usize>,
    pub collapse: Option<Range<usize>>,
}

pub fn layout(cols: usize) -> Layout {
    if cols >= NAV_MIN_COLS {
        Layout {
            back: Some(0..BUTTON),
            forward: Some(BUTTON..2 * BUTTON),
            reload: Some(2 * BUTTON..3 * BUTTON),
            field: 3 * BUTTON..cols - BUTTON,
            collapse: Some(cols - BUTTON..cols),
        }
    } else if cols >= COLLAPSE_MIN_COLS {
        Layout {
            back: None,
            forward: None,
            reload: None,
            field: 0..cols - BUTTON,
            collapse: Some(cols - BUTTON..cols),
        }
    } else {
        Layout { back: None, forward: None, reload: None, field: 0..cols, collapse: None }
    }
}

impl Layout {
    pub fn hit(&self, col: usize) -> Option<Hit> {
        let at = |r: &Option<Range<usize>>| r.as_ref().is_some_and(|r| r.contains(&col));
        if at(&self.back) {
            Some(Hit::Back)
        } else if at(&self.forward) {
            Some(Hit::Forward)
        } else if at(&self.reload) {
            Some(Hit::Reload)
        } else if self.field.contains(&col) {
            Some(Hit::Field)
        } else if at(&self.collapse) {
            Some(Hit::Collapse)
        } else {
            None
        }
    }
}

/// Keys of the line editor that are not plain text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKey {
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    KillToStart,
    KillToEnd,
    DeleteWord,
    SelectAll,
    /// Take the offered completion (Tab).
    Accept,
    /// Select the next / previous candidate of the dropdown.
    Next,
    Prev,
}

/// Rows of candidates the dropdown shows at most.
pub const DROPDOWN_MAX: usize = 5;

/// The address field while it is being typed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    text: Vec<char>,
    cursor: usize,
    /// The whole text is selected: the next character replaces it.
    all: bool,
    /// What history offers for the text, best first. The first one is shown inline after the
    /// text; with more than one they are also listed below the field.
    list: Vec<Completion>,
    /// The candidate Enter and Tab act on: 0 is the inline one, more is picked with the arrows.
    sel: usize,
}

fn char_width(c: char) -> usize {
    unicode_column_width(c.encode_utf8(&mut [0; 4]), None)
}

fn width_of(chars: &[char]) -> usize {
    chars.iter().map(|c| char_width(*c)).sum()
}

impl Edit {
    pub fn new(text: &str) -> Self {
        let text: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
        Self { cursor: text.len(), all: !text.is_empty(), text, list: vec![], sel: 0 }
    }

    /// The caret is after the last character and nothing is selected: the only place a completion
    /// can be offered.
    pub fn at_end(&self) -> bool {
        !self.all && self.cursor == self.text.len()
    }

    /// The candidates for the text now in the field (none unless the caret is at its end).
    pub fn set_suggestions(&mut self, list: Vec<Completion>) {
        self.list = if self.at_end() { list } else { vec![] };
        self.sel = 0;
    }

    /// The address Enter opens instead of the text: that of the selected candidate.
    pub fn suggested_url(&self) -> Option<&str> {
        self.list.get(self.sel).map(|c| c.url.as_str())
    }

    /// The candidates the dropdown lists (only when there is more than the inline one).
    pub fn dropdown(&self) -> &[Completion] {
        if self.list.len() >= 2 {
            &self.list[..self.list.len().min(DROPDOWN_MAX)]
        } else {
            &[]
        }
    }

    pub fn selected(&self) -> usize {
        self.sel
    }

    /// Select candidate `i` of the dropdown (a hover); out of range is ignored.
    pub fn select(&mut self, i: usize) {
        if i < self.dropdown().len() {
            self.sel = i;
        }
    }

    /// The candidate's address that stands in the field while it is selected with the arrows.
    fn preview(&self) -> Option<&str> {
        (self.sel > 0).then(|| self.list.get(self.sel)).flatten().map(|c| c.display.as_str())
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    /// Typed or pasted text: tabs and line breaks become spaces, other control characters are
    /// dropped.
    pub fn insert(&mut self, s: &str) {
        let clean: Vec<char> = s
            .chars()
            .filter_map(|c| match c {
                '\t' | '\n' | '\r' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        if clean.is_empty() {
            return;
        }
        self.list.clear();
        self.sel = 0;
        if self.all {
            self.text.clear();
            self.cursor = 0;
            self.all = false;
        }
        let n = clean.len();
        self.text.splice(self.cursor..self.cursor, clean);
        self.cursor += n;
    }

    pub fn key(&mut self, key: EditKey) {
        let all = std::mem::replace(&mut self.all, false);
        // Any key but the ones that take or move through the offers drops them (Backspace gives
        // them up, as in a browser's address bar).
        let (list, sel) = (std::mem::take(&mut self.list), std::mem::take(&mut self.sel));
        match key {
            EditKey::Next | EditKey::Prev if list.len() >= 2 && !all => {
                let last = list.len().min(DROPDOWN_MAX) - 1;
                self.sel = if key == EditKey::Next { (sel + 1).min(last) } else { sel.saturating_sub(1) };
                self.list = list;
                return;
            }
            EditKey::Accept | EditKey::Right | EditKey::End if !all && !list.is_empty() => {
                if sel > 0 {
                    // A candidate picked with the arrows replaces the text.
                    self.text = list[sel].display.chars().collect();
                    self.cursor = self.text.len();
                    return;
                }
                if self.cursor == self.text.len() && !list[0].suffix.is_empty() {
                    self.text.extend(list[0].suffix.chars());
                    self.cursor = self.text.len();
                    return;
                }
            }
            _ => {}
        }
        match key {
            EditKey::Accept | EditKey::Next | EditKey::Prev => {}
            EditKey::SelectAll => self.all = !self.text.is_empty(),
            _ if all && matches!(key, EditKey::Backspace | EditKey::Delete | EditKey::DeleteWord | EditKey::KillToStart | EditKey::KillToEnd) => {
                self.text.clear();
                self.cursor = 0;
            }
            EditKey::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.text.remove(self.cursor);
                }
            }
            EditKey::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            EditKey::Left => self.cursor = if all { 0 } else { self.cursor.saturating_sub(1) },
            EditKey::Right => self.cursor = if all { self.text.len() } else { (self.cursor + 1).min(self.text.len()) },
            EditKey::Home => self.cursor = 0,
            EditKey::End => self.cursor = self.text.len(),
            EditKey::KillToStart => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            EditKey::KillToEnd => self.text.truncate(self.cursor),
            EditKey::DeleteWord => {
                let mut start = self.cursor;
                while start > 0 && self.text[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.text[start - 1] != ' ' {
                    start -= 1;
                }
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
        }
    }

    /// The characters shown in a field `room` columns wide, as (before the caret, at the caret,
    /// after it): the window scrolls so the caret stays visible. The caret sits on a character,
    /// or on the blank past the end.
    fn window(&self, room: usize) -> (String, Option<char>, String) {
        let mut start = 0;
        // Keep one column for the caret itself.
        while start < self.cursor && width_of(&self.text[start..self.cursor]) + 1 > room {
            start += 1;
        }
        let mut used = 0;
        let mut before = String::new();
        let mut at = None;
        let mut after = String::new();
        for (i, c) in self.text.iter().enumerate().skip(start) {
            let w = char_width(*c);
            if used + w > room {
                break;
            }
            used += w;
            match i.cmp(&self.cursor) {
                std::cmp::Ordering::Less => before.push(*c),
                std::cmp::Ordering::Equal => at = Some(*c),
                std::cmp::Ordering::Greater => after.push(*c),
            }
        }
        (before, at, after)
    }
}

/// State of the header row of one pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// The row is part of the pane (a field being typed in shows it regardless).
    pub shown: bool,
    pub edit: Option<Edit>,
    /// Why the last Enter did nothing; cleared by the next key.
    pub error: Option<String>,
    pub hover: Hover,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hover {
    pub in_header: bool,
    pub hit: Option<Hit>,
}

impl Header {
    pub fn new() -> Self {
        Self { shown: true, edit: None, error: None, hover: Hover::default() }
    }

    /// Rows the header takes of a pane `pane_rows` tall: 0 or 1.
    pub fn rows(&self, pane_rows: usize) -> usize {
        usize::from((self.shown || self.edit.is_some()) && pane_rows >= MIN_PANE_ROWS)
    }

    /// Rows at the top of the pane the page does not cover: the header row and the dropdown.
    pub fn top_rows(&self, pane_rows: usize) -> usize {
        self.rows(pane_rows) + self.dropdown_rows(pane_rows)
    }

    /// Rows of the dropdown below the header: the candidates of the field being typed in, when
    /// there is more than the inline one, as many as leave a row of the page.
    pub fn dropdown_rows(&self, pane_rows: usize) -> usize {
        match &self.edit {
            Some(e) if self.error.is_none() && self.rows(pane_rows) == 1 => {
                e.dropdown().len().min(pane_rows.saturating_sub(MIN_PANE_ROWS - 1))
            }
            _ => 0,
        }
    }

    /// The dropdown rows as escape text, each exactly `cols` wide: under the field, the selected
    /// candidate in reverse video.
    pub fn render_dropdown(&self, cols: usize, pane_rows: usize) -> Vec<String> {
        let n = self.dropdown_rows(pane_rows);
        let Some(edit) = &self.edit else { return vec![] };
        let lay = layout(cols);
        (0..n)
            .map(|i| {
                let mut row = Row::new(cols);
                row.pad("", lay.field.start);
                let style = if i == edit.selected() { "100;7" } else { "100" };
                let width = lay.field.len();
                row.put(width, style, &format!(" {}", fit_plain(&edit.dropdown()[i].display, width.saturating_sub(2))));
                row.finish()
            })
            .collect()
    }

    /// The row as escape text for a terminal `cols` wide (home position, no line break): one
    /// SGR run per part, exactly `cols` columns.
    pub fn render(&self, cols: usize, info: &PageInfo) -> String {
        let lay = layout(cols);
        let mut row = Row::new(cols);
        let button = |row: &mut Row, range: &Option<Range<usize>>, glyph: &str, on: bool, hit: Hit| {
            if let Some(range) = range {
                let hovered = on && self.hover.hit == Some(hit);
                let style = if hovered { "1;7" } else if on { "" } else { "2" };
                row.put(range.len(), style, &format!(" {glyph} "));
            }
        };
        button(&mut row, &lay.back, "‹", info.can_go_back, Hit::Back);
        button(&mut row, &lay.forward, "›", info.can_go_forward, Hit::Forward);
        button(&mut row, &lay.reload, if info.loading { "✕" } else { "↻" }, true, Hit::Reload);

        // The field: one blank of padding each side, none in a field of one or two columns.
        const FIELD: &str = "100";
        let pad = usize::from(lay.field.len() >= 3);
        let room = lay.field.len() - 2 * pad;
        row.put(pad, FIELD, &" ".repeat(pad));
        match (&self.edit, &self.error) {
            (Some(_), Some(error)) => {
                row.put(room, "100;31", &fit(error, room));
            }
            (Some(edit), None) => {
                if let Some(shown) = edit.preview() {
                    // An arrow-selected candidate stands in the field.
                    row.put(room, "100;7", &fit_plain(shown, room));
                    row.put(pad, FIELD, &" ".repeat(pad));
                    if let Some(range) = &lay.collapse {
                        row.put(range.len(), if self.hover.hit == Some(Hit::Collapse) { "1;7" } else { "" }, " ▴ ");
                    }
                    return row.finish();
                }
                let (before, at, after) = edit.window(room);
                if edit.all {
                    let shown = format!("{before}{}{after}", at.map(String::from).unwrap_or_default());
                    row.put(room, "100;7", &shown);
                } else {
                    let start = row.used;
                    row.push(FIELD, &before);
                    // A completion on offer: its first character carries the caret.
                    let ghost = match (edit.list.first(), at, after.is_empty()) {
                        (Some(c), None, true) => fit_plain(&c.suffix, room.saturating_sub(row.used - start)),
                        _ => String::new(),
                    };
                    if ghost.is_empty() {
                        row.push("100;7", &at.map(String::from).unwrap_or_else(|| " ".into()));
                        row.push(FIELD, &after);
                    } else {
                        let mut chars = ghost.chars();
                        let first: String = chars.by_ref().take(1).collect();
                        row.push("100;7", &first);
                        row.push("100;2", chars.as_str());
                    }
                    row.pad(FIELD, room.saturating_sub(row.used - start));
                }
            }
            (None, _) => {
                let url = if info.url == "about:blank" { "" } else { info.url.as_str() };
                if url.is_empty() {
                    row.put(room, "100;2", &fit(HINT, room));
                } else {
                    row.put(room, FIELD, &fit(url, room));
                }
            }
        }
        row.put(pad, FIELD, &" ".repeat(pad));
        if let Some(range) = &lay.collapse {
            let hovered = self.hover.hit == Some(Hit::Collapse);
            row.put(range.len(), if hovered { "1;7" } else { "" }, " ▴ ");
        }
        row.finish()
    }
}

/// The start of `s` that fits `cols` columns.
fn fit_plain(s: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars().filter(|c| !c.is_control()) {
        let w = char_width(c);
        if width + w > cols {
            break;
        }
        width += w;
        out.push(c);
    }
    out
}

/// `s` cut to `cols` columns, ending in `…` when something was cut.
fn fit(s: &str, cols: usize) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    if unicode_column_width(&s, None) <= cols {
        return s;
    }
    let mut out = String::new();
    let mut width = 0;
    for c in s.chars() {
        let w = char_width(c);
        if width + w + 1 > cols {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// A row of styled runs, padded to its width.
struct Row {
    cols: usize,
    used: usize,
    out: String,
}

impl Row {
    fn new(cols: usize) -> Self {
        Self { cols, used: 0, out: String::new() }
    }

    /// `text` in `style` (an SGR parameter list, `""` for the default) without padding.
    fn push(&mut self, style: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if style.is_empty() {
            self.out.push_str("\x1b[0m");
        } else {
            self.out.push_str(&format!("\x1b[0;{style}m"));
        }
        self.out.push_str(text);
        self.used += unicode_column_width(text, None);
    }

    /// `text` in `style`, blank-padded on the right to `width` columns.
    fn put(&mut self, width: usize, style: &str, text: &str) {
        self.push(style, text);
        self.pad(style, width.saturating_sub(unicode_column_width(text, None)));
    }

    fn pad(&mut self, style: &str, count: usize) {
        if count > 0 {
            self.push(style, &" ".repeat(count));
        }
    }

    fn finish(mut self) -> String {
        let rest = self.cols.saturating_sub(self.used);
        self.pad("", rest);
        self.out.push_str("\x1b[0m");
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        // Drop the SGR runs.
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn info(url: &str) -> PageInfo {
        PageInfo { url: url.into(), ..PageInfo::default() }
    }

    #[test]
    fn the_layout_degrades_with_the_width() {
        let wide = layout(60);
        assert_eq!(wide.back, Some(0..3));
        assert_eq!(wide.reload, Some(6..9));
        assert_eq!(wide.field, 9..57);
        assert_eq!(wide.collapse, Some(57..60));
        let mid = layout(20);
        assert_eq!((mid.back.clone(), mid.field.clone(), mid.collapse.clone()), (None, 0..17, Some(17..20)));
        let tiny = layout(6);
        assert_eq!((tiny.field.clone(), tiny.collapse.clone()), (0..6, None));
        for cols in [1, 6, 8, 29, 30, 120] {
            let l = layout(cols);
            assert_eq!(l.collapse.as_ref().map_or(l.field.end, |c| c.end), cols, "{cols}");
        }
    }

    #[test]
    fn a_column_is_one_part() {
        let l = layout(40);
        assert_eq!(l.hit(0), Some(Hit::Back));
        assert_eq!(l.hit(5), Some(Hit::Forward));
        assert_eq!(l.hit(8), Some(Hit::Reload));
        assert_eq!(l.hit(9), Some(Hit::Field));
        assert_eq!(l.hit(36), Some(Hit::Field));
        assert_eq!(l.hit(37), Some(Hit::Collapse));
        assert_eq!(l.hit(40), None);
    }

    #[test]
    fn the_header_needs_a_pane_of_three_rows_and_hides_on_request() {
        let mut h = Header::new();
        assert_eq!((h.rows(24), h.rows(3), h.rows(2), h.rows(1)), (1, 1, 0, 0));
        h.shown = false;
        assert_eq!(h.rows(24), 0);
        h.edit = Some(Edit::new("x"));
        assert_eq!(h.rows(24), 1, "typing shows a hidden header");
        assert_eq!(h.rows(2), 0);
    }

    #[test]
    fn typing_replaces_a_selected_address_and_editing_keys_work() {
        let mut e = Edit::new("https://a.test");
        e.insert("b");
        assert_eq!(e.text(), "b");
        e.insert("cd");
        e.key(EditKey::Left);
        e.key(EditKey::Backspace);
        assert_eq!(e.text(), "bd");
        e.key(EditKey::Home);
        e.key(EditKey::Delete);
        assert_eq!(e.text(), "d");
        e.key(EditKey::End);
        e.insert("e f");
        e.key(EditKey::DeleteWord);
        assert_eq!(e.text(), "de ");
        e.key(EditKey::DeleteWord);
        assert_eq!(e.text(), "");
        let mut e = Edit::new("hello world");
        e.key(EditKey::Left);
        assert_eq!(e.text(), "hello world");
        e.key(EditKey::Home);
        e.key(EditKey::KillToEnd);
        assert_eq!(e.text(), "");
        let mut e = Edit::new("abc");
        e.key(EditKey::Backspace);
        assert_eq!(e.text(), "", "Backspace over a selection clears it");
        let mut e = Edit::new("abc");
        e.key(EditKey::Right);
        e.insert("d");
        assert_eq!(e.text(), "abcd", "Right over a selection puts the caret at its end");
        let mut e = Edit::new("abc");
        e.key(EditKey::Left);
        e.insert("d");
        assert_eq!(e.text(), "dabc", "Left over a selection puts the caret at its start");
        let mut e = Edit::new("abc");
        e.key(EditKey::Right);
        e.insert("d");
        e.key(EditKey::KillToStart);
        assert_eq!(e.text(), "");
    }

    #[test]
    fn pasted_text_is_one_clean_line() {
        let mut e = Edit::new("");
        e.insert("a\tb\r\nc\u{1b}[31md\u{7}");
        assert_eq!(e.text(), "a b  c[31md");
    }

    #[test]
    fn the_row_is_exactly_the_pane_width_and_shows_the_address() {
        let h = Header::new();
        for cols in [1, 5, 8, 20, 29, 30, 80] {
            let text = plain(&h.render(cols, &info("https://example.com/a")));
            assert_eq!(unicode_column_width(&text, None), cols, "{cols}: {text:?}");
        }
        let text = plain(&h.render(40, &info("https://example.com/a")));
        assert!(text.starts_with(" ‹  ›  ↻ "), "{:?}", text);
        assert!(text.contains("https://example.com/a"));
        assert!(text.ends_with(" ▴ "));
    }

    #[test]
    fn the_row_keeps_its_width_while_typing_and_on_an_error() {
        for cols in [1, 5, 8, 12, 29, 30, 80] {
            for text in ["", "a", "hello world", "日本語の検索 and more words than room"] {
                for caret_moves in [0, 3, 100] {
                    let mut h = Header::new();
                    let mut e = Edit::new(text);
                    e.key(EditKey::End);
                    e.insert("");
                    for _ in 0..caret_moves {
                        e.key(EditKey::Left);
                    }
                    h.edit = Some(e);
                    let row = plain(&h.render(cols, &info("https://x.test")));
                    assert_eq!(unicode_column_width(&row, None), cols, "{cols} {text:?} {caret_moves}: {row:?}");
                    h.edit.as_mut().unwrap().insert("x");
                    h.error = Some("cannot open a javascript: address in a web pane".into());
                    let row = plain(&h.render(cols, &info("")));
                    assert_eq!(unicode_column_width(&row, None), cols, "error {cols}: {row:?}");
                }
            }
        }
    }

    #[test]
    fn a_long_address_is_cut_with_an_ellipsis_and_a_blank_page_shows_the_hint() {
        let h = Header::new();
        let long = format!("https://example.com/{}", "x".repeat(200));
        let text = plain(&h.render(50, &info(&long)));
        assert!(text.contains('…') && unicode_column_width(&text, None) == 50, "{:?}", text);
        assert!(plain(&h.render(50, &info("about:blank"))).contains(HINT));
        assert!(plain(&h.render(50, &info(""))).contains(HINT));
    }

    #[test]
    fn buttons_follow_the_page_state() {
        let h = Header::new();
        let mut i = info("https://x.test");
        assert!(h.render(40, &i).contains("\x1b[0;2m ‹ "), "back is dim without history");
        i.can_go_back = true;
        assert!(!h.render(40, &i).contains("\x1b[0;2m ‹ "));
        assert!(plain(&h.render(40, &i)).contains("↻"));
        i.loading = true;
        assert!(plain(&h.render(40, &i)).contains("✕"));
    }

    #[test]
    fn the_edit_window_keeps_the_caret_in_sight() {
        let mut e = Edit::new("0123456789abcdefghij");
        e.key(EditKey::End);
        let (before, at, after) = e.window(10);
        assert_eq!(at, None);
        assert_eq!((before.as_str(), after.as_str()), ("bcdefghij", ""));
        e.key(EditKey::Home);
        let (before, at, after) = e.window(10);
        assert_eq!((before.as_str(), at, after.as_str()), ("", Some('0'), "123456789"));
    }

    #[test]
    fn an_error_replaces_the_text_until_the_next_key() {
        let mut h = Header::new();
        h.edit = Some(Edit::new("javascript:1"));
        h.error = Some("cannot open a javascript: address in a web pane".into());
        let text = plain(&h.render(60, &info("")));
        assert!(text.contains("cannot open a javascript:"), "{:?}", text);
        assert!(!text.contains("javascript:1"));
    }
}
