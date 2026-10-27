//! The addresses a profile has visited, for the header field's inline completion: typing `te`
//! offers `ams.microsoft.com` when that is the site visited most (and most recently) among those
//! starting with it. Only `http(s)` addresses without credentials are kept, without query and
//! fragment, as URL, visit count and last visit; no titles, no page content. Stored per profile next to its browser profile
//! (`profile-<name>.history.json`, mode 0600); a private pane records nothing.

use std::collections::HashMap;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 2000;
const MAX_URL_BYTES: usize = 2048;
const DAY: u64 = 86_400;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    url: String,
    visits: u32,
    last: u64,
}

/// What typing offers: `suffix` goes after the typed text; `url` is where Enter on it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub suffix: String,
    pub url: String,
}

#[derive(Default)]
pub struct History {
    entries: Vec<Entry>,
    path: Option<PathBuf>,
}

/// `https://a.test/x#f` -> (`https`, `a.test`, `/x`) for an address worth remembering.
fn parts(url: &str) -> Option<(&str, &str, &str)> {
    if url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    // Neither the query nor the fragment is kept: they carry search words, session ids and
    // sign-in tokens, and the page is the same without them.
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    Some((scheme, authority, &tail[..path_end]))
}

/// The address as kept: no query, no fragment, a path of at least `/`.
fn canonical(url: &str) -> Option<String> {
    let (scheme, authority, path) = parts(url)?;
    let path = if path.is_empty() { "/" } else { path };
    Some(format!("{scheme}://{}{path}", authority.to_ascii_lowercase()))
}

/// The address without scheme and a leading `www.`, what a person types.
fn shown(url: &str) -> Option<(String, String)> {
    let (_, authority, path) = parts(url)?;
    let host = authority.strip_prefix("www.").unwrap_or(authority);
    Some((host.to_string(), format!("{host}{path}")))
}

/// Firefox-like frecency weight of a visit that is `age` seconds old.
fn weight(age: u64) -> f64 {
    match age / DAY {
        0..=3 => 100.0,
        4..=13 => 70.0,
        14..=30 => 50.0,
        31..=89 => 30.0,
        _ => 10.0,
    }
}

impl Entry {
    fn score(&self, now: u64) -> f64 {
        self.visits as f64 * weight(now.saturating_sub(self.last))
    }
}

impl History {
    /// In memory only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Read from `path` (missing or unreadable: empty) and written back there on every change.
    pub fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .map(|v| Self::entries_of(&v))
            .unwrap_or_default();
        Self { entries, path: Some(path) }
    }

    fn entries_of(v: &serde_json::Value) -> Vec<Entry> {
        let Some(list) = v.get("entries").and_then(|e| e.as_array()) else {
            return vec![];
        };
        list.iter()
            .filter_map(|e| {
                Some(Entry {
                    url: canonical(e.get("u")?.as_str()?)?,
                    visits: e.get("n")?.as_u64()?.min(u32::MAX as u64) as u32,
                    last: e.get("t")?.as_u64()?,
                })
            })
            .take(MAX_ENTRIES)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// One more visit of `url` at `now` (seconds since the epoch). Addresses that are not worth
    /// remembering (other schemes, credentials, control characters) are ignored.
    pub fn record(&mut self, url: &str, now: u64) {
        let Some(url) = canonical(url) else { return };
        match self.entries.iter_mut().find(|e| e.url == url) {
            Some(e) => {
                e.visits = e.visits.saturating_add(1);
                e.last = now;
            }
            None => self.entries.push(Entry { url, visits: 1, last: now }),
        }
        if self.entries.len() > MAX_ENTRIES {
            // The least valuable go: lowest score, then the oldest.
            self.entries.sort_by(|a, b| {
                b.score(now).total_cmp(&a.score(now)).then(b.last.cmp(&a.last))
            });
            self.entries.truncate(MAX_ENTRIES);
        }
        self.save();
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let list: Vec<serde_json::Value> = self
            .entries
            .iter()
            .map(|e| serde_json::json!({ "u": e.url, "n": e.visits, "t": e.last }))
            .collect();
        let text = serde_json::json!({ "v": 1, "entries": list }).to_string();
        if let Err(err) = write_private(path, &text) {
            log::warn!("web history {}: {err}", path.display());
        }
    }

    /// What to offer after `typed`, if anything. Without a `/` the best site (by frecency, summed
    /// over its pages) whose name starts with `typed`; with one the best page whose address does.
    /// A scheme and `www.` at the start of `typed` are ignored, as in the address bar of a
    /// browser. Nothing for empty text, text with blanks, or text that already is the whole
    /// address.
    pub fn complete(&self, typed: &str, now: u64) -> Option<Completion> {
        if typed.is_empty() || typed.chars().any(char::is_whitespace) {
            return None;
        }
        let lower = typed.to_ascii_lowercase();
        let t = lower
            .strip_prefix("https://")
            .or_else(|| lower.strip_prefix("http://"))
            .unwrap_or(&lower);
        let t = t.strip_prefix("www.").unwrap_or(t);
        if t.is_empty() {
            return None;
        }
        if t.contains('/') {
            let mut best: Option<(f64, &Entry, String)> = None;
            for e in &self.entries {
                let Some((_, full)) = shown(&e.url) else { continue };
                if full.len() > t.len() && full.to_ascii_lowercase().starts_with(t) {
                    let s = e.score(now);
                    if best.as_ref().map_or(true, |(b, be, bf)| {
                        s > *b || (s == *b && (full.len(), &e.url) < (bf.len(), &be.url))
                    }) {
                        best = Some((s, e, full));
                    }
                }
            }
            return best.and_then(|(_, e, full)| {
                Some(Completion { suffix: full.get(t.len()..)?.to_string(), url: e.url.clone() })
            });
        }
        // Per site: the summed score, and the page that stands for it.
        let mut sites: HashMap<String, (f64, f64, &Entry)> = HashMap::new();
        for e in &self.entries {
            let Some((host, _)) = shown(&e.url) else { continue };
            let s = e.score(now);
            let site = sites.entry(host).or_insert((0.0, f64::MIN, e));
            site.0 += s;
            if s > site.1 {
                site.1 = s;
                site.2 = e;
            }
        }
        let mut best: Option<(&String, f64, &Entry)> = None;
        for (host, (total, _, rep)) in &sites {
            if host.len() > t.len() && host.to_ascii_lowercase().starts_with(t) {
                if best.as_ref().map_or(true, |(bh, bs, _)| {
                    *total > *bs || (*total == *bs && (host.len(), host) < (bh.len(), *bh))
                }) {
                    best = Some((host, *total, rep));
                }
            }
        }
        let (host, _, rep) = best?;
        let (scheme, authority, _) = parts(&rep.url)?;
        Some(Completion {
            suffix: host.get(t.len()..)?.to_string(),
            url: format!("{scheme}://{authority}/"),
        })
    }
}

fn write_private(path: &PathBuf, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn hist(visits: &[(&str, u32, u64)]) -> History {
        let mut h = History::new();
        for (url, n, age_days) in visits {
            for _ in 0..*n {
                h.record(url, NOW - age_days * DAY);
            }
        }
        h
    }

    fn c(h: &History, typed: &str) -> Option<(String, String)> {
        h.complete(typed, NOW).map(|c| (c.suffix, c.url))
    }

    #[test]
    fn typing_the_start_of_a_site_offers_the_rest_and_its_address() {
        let h = hist(&[("https://teams.microsoft.com/v2/?x=1", 5, 0), ("https://tesla.com/", 1, 0)]);
        assert_eq!(c(&h, "te"), Some(("ams.microsoft.com".into(), "https://teams.microsoft.com/".into())));
        assert_eq!(c(&h, "tes"), Some(("la.com".into(), "https://tesla.com/".into())));
        assert_eq!(c(&h, "teams.microsoft.com"), None, "nothing left to offer");
        assert_eq!(c(&h, "x"), None);
        assert_eq!(c(&h, ""), None);
        assert_eq!(c(&h, "te st"), None);
    }

    #[test]
    fn a_site_is_found_without_its_www_and_opens_with_it() {
        let h = hist(&[("https://www.tesla.com/model3", 2, 0)]);
        assert_eq!(c(&h, "te"), Some(("sla.com".into(), "https://www.tesla.com/".into())));
        assert_eq!(c(&h, "www.te"), Some(("sla.com".into(), "https://www.tesla.com/".into())));
        assert_eq!(c(&h, "https://te"), Some(("sla.com".into(), "https://www.tesla.com/".into())));
        assert_eq!(c(&h, "HTTPS://TE"), Some(("sla.com".into(), "https://www.tesla.com/".into())));
    }

    #[test]
    fn the_site_visited_more_and_more_recently_wins() {
        let h = hist(&[("https://teams.test/", 5, 0), ("https://tea.test/", 30, 400)]);
        assert_eq!(c(&h, "te").unwrap().1, "https://teams.test/", "5 recent visits beat 30 a year old");
        let h = hist(&[("https://teams.test/", 1, 0), ("https://tea.test/", 5, 0)]);
        assert_eq!(c(&h, "te").unwrap().1, "https://tea.test/");
        // Pages add up per site.
        let h = hist(&[("https://a.test/1", 1, 0), ("https://a.test/2", 1, 0), ("https://a.test/3", 1, 0), ("https://ab.test/", 2, 0)]);
        assert_eq!(c(&h, "a").unwrap().1, "https://a.test/", "three pages of a.test outweigh two visits of ab.test");
        // A tie goes to the shorter name, then the alphabet: the same answer every time.
        let h = hist(&[("https://bb.test/", 1, 0), ("https://b.test/", 1, 0), ("https://ba.test/", 1, 0)]);
        assert_eq!(c(&h, "b").unwrap().1, "https://b.test/");
    }

    #[test]
    fn a_path_in_the_typed_text_completes_the_page() {
        let h = hist(&[("https://github.com/rust-lang/rust", 3, 0), ("https://github.com/rust-lang/cargo", 1, 0), ("https://github.com/", 9, 0)]);
        assert_eq!(c(&h, "github.com/ru"), Some(("st-lang/rust".into(), "https://github.com/rust-lang/rust".into())));
        assert_eq!(c(&h, "github.com/rust-lang/c"), Some(("argo".into(), "https://github.com/rust-lang/cargo".into())));
        assert_eq!(c(&h, "github.com/rust-lang/rust"), None);
        assert_eq!(c(&h, "gi").unwrap().0, "thub.com");
    }

    #[test]
    fn only_plain_web_addresses_are_kept_and_a_fragment_is_dropped() {
        let mut h = History::new();
        for u in [
            "file:///tmp/a.html",
            "data:text/html,<b>x</b>",
            "about:blank",
            "javascript:alert(1)",
            "https://user:pw@secret.test/",
            "https://a.test/\u{1b}x",
            "https://a b.test/",
            "https://",
            "https:///x",
        ] {
            h.record(u, NOW);
        }
        assert_eq!(h.len(), 0);
        h.record("https://A.Test/p#one", NOW);
        h.record("https://a.test/p?token=secret#two", NOW);
        h.record("https://a.test", NOW);
        assert_eq!(h.len(), 2, "one page twice (host case and fragment ignored), plus the root");
        assert_eq!(c(&h, "a").unwrap().1, "https://a.test/");
        assert!(h.entries.iter().all(|e| !e.url.contains(['?', '#', '='])), "no query reaches the history");
        assert_eq!(c(&h, "a.test/p?tok"), None, "and none is completed");
    }

    #[test]
    fn the_history_is_bounded_and_keeps_what_is_worth_most() {
        let mut h = History::new();
        for i in 0..MAX_ENTRIES + 50 {
            h.record(&format!("https://site{i}.test/"), NOW - 400 * DAY);
        }
        h.record("https://keep.test/", NOW);
        h.record("https://keep.test/", NOW);
        assert!(h.len() <= MAX_ENTRIES);
        assert!(c(&h, "kee").is_some());
    }

    #[test]
    fn the_history_survives_a_restart_privately_and_a_broken_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("hgweb-history-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profile-main.history.json");
        let mut h = History::load(path.clone());
        assert_eq!(h.len(), 0);
        h.record("https://teams.test/a", NOW);
        h.record("https://teams.test/a", NOW);
        let again = History::load(path.clone());
        assert_eq!(again.len(), 1);
        assert_eq!(c(&again, "te"), Some(("ams.test".into(), "https://teams.test/".into())));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(History::load(path.clone()).len(), 0);
        std::fs::write(&path, r#"{"entries":[{"u":"javascript:1","n":1,"t":1},{"u":"https://ok.test/","n":2,"t":5},{"n":1}]}"#).unwrap();
        assert_eq!(History::load(path).len(), 1, "bad entries are skipped, not trusted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
