//! `CdpEngine`: the `WebEngine` that owns one browser process per profile name.

use crate::browser::{Browser, ChromeLauncher, Launcher};
use crate::view::{View, ViewCore};
use crate::{ViewSize, WebConfig, WebEngine, WebEvent, WebView};
use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// How long a new launch waits for the previous browser of the profile to exit (they share
/// one profile directory, which Chrome locks).
const RETIRE_WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ProfState {
    browser: Option<Arc<Browser>>,
    views: Vec<Weak<ViewCore>>,
    /// Browsers on their way out; a relaunch waits for them.
    retiring: Vec<Arc<Browser>>,
}

#[derive(Default)]
struct Profile {
    /// Serialises launches so concurrent views of one profile share one browser.
    gate: Mutex<()>,
    state: Mutex<ProfState>,
}

pub(crate) struct EngineShared {
    launcher: Arc<dyn Launcher>,
    profiles: Mutex<HashMap<String, Arc<Profile>>>,
    next_view: AtomicU64,
}

impl EngineShared {
    fn profile(&self, name: &str) -> Arc<Profile> {
        // Keyed by the directory name: profiles that share a directory share a browser.
        let key = crate::browser::profile_dir_name(name);
        self.profiles.lock().entry(key).or_default().clone()
    }

    pub fn register(&self, view: &Arc<ViewCore>) {
        let p = self.profile(&view.cfg.profile);
        p.state.lock().views.push(Arc::downgrade(view));
    }

    /// The browser of the view's profile, launching it when there is none (or the old one is
    /// gone). Blocks while Chrome starts; called from view worker threads only.
    pub fn acquire(&self, view: &ViewCore) -> Result<Arc<Browser>> {
        let prof = self.profile(&view.cfg.profile);
        let _gate = prof.gate.lock();
        let retiring = {
            let mut st = prof.state.lock();
            if let Some(b) = &st.browser {
                if b.is_usable() {
                    return Ok(b.clone());
                }
            }
            if let Some(old) = st.browser.take() {
                st.retiring.push(old);
            }
            std::mem::take(&mut st.retiring)
        };
        for old in &retiring {
            old.shutdown();
            if !old.wait_exit(RETIRE_WAIT) {
                log::warn!("previous browser of profile {:?} is still running", view.cfg.profile);
            }
        }

        let browser = Browser::launch(&self.launcher, &view.cfg, view.wanted_scale())?;
        let others: Vec<Arc<ViewCore>> = {
            let mut st = prof.state.lock();
            st.browser = Some(browser.clone());
            st.views
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|v| v.id != view.id)
                .collect()
        };
        // Views that lost their page to the old browser's death come back on the new one.
        for v in others {
            v.revive();
        }
        Ok(browser)
    }

    /// A view is finished: forget it and stop the browser when it was the last one.
    pub fn release(&self, view: &ViewCore) {
        let prof = self.profile(&view.cfg.profile);
        let last = {
            let mut st = prof.state.lock();
            st.views
                .retain(|w| w.upgrade().is_some_and(|v| v.id != view.id));
            st.retiring.retain(|b| !b.wait_exit(Duration::ZERO));
            if st.views.is_empty() {
                st.browser.take().inspect(|b| st.retiring.push(b.clone()))
            } else {
                None
            }
        };
        if let Some(b) = last {
            b.shutdown();
        }
    }
}

impl EngineShared {
    /// Quit every browser orderly and wait for the processes, then kill what is left.
    fn shutdown_all(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut browsers: Vec<Arc<Browser>> = vec![];
        for prof in self.profiles.lock().values() {
            let mut st = prof.state.lock();
            browsers.extend(st.browser.take());
            browsers.append(&mut st.retiring);
        }
        for b in &browsers {
            b.shutdown();
        }
        for b in &browsers {
            if !b.wait_exit(deadline.saturating_duration_since(Instant::now())) {
                log::warn!("browser {} did not quit in time; killing it", b.pid());
                b.kill_now();
            }
        }
    }
}

/// The Chrome-over-CDP engine. Cheap to create; browsers start when the first view of a
/// profile is created.
pub struct CdpEngine {
    shared: Arc<EngineShared>,
}

impl CdpEngine {
    pub fn new() -> Self {
        Self::with_launcher(Arc::new(ChromeLauncher))
    }

    pub(crate) fn with_launcher(launcher: Arc<dyn Launcher>) -> Self {
        CdpEngine {
            shared: Arc::new(EngineShared {
                launcher,
                profiles: Mutex::new(HashMap::new()),
                next_view: AtomicU64::new(1),
            }),
        }
    }

    /// Process ids of the running browsers (one per profile with live views), for diagnostics
    /// such as CPU accounting in the spike.
    pub fn browser_pids(&self) -> Vec<u32> {
        self.shared
            .profiles
            .lock()
            .values()
            .filter_map(|p| p.state.lock().browser.as_ref().map(|b| b.pid()))
            .collect()
    }
}

impl Default for CdpEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WebEngine for CdpEngine {
    fn create_view(
        &self,
        url: &str,
        cfg: &WebConfig,
        size: ViewSize,
        notify: Box<dyn Fn(WebEvent) + Send + Sync>,
    ) -> Result<Arc<dyn WebView>> {
        let mut cfg = cfg.clone();
        if cfg.profile.is_empty() {
            cfg.profile = "main".to_string();
        }
        let id = self.shared.next_view.fetch_add(1, Ordering::Relaxed);
        let view = View::spawn(self.shared.clone(), id, url, &cfg, size, notify)?;
        Ok(view)
    }

    fn shutdown(&self, timeout: Duration) {
        self.shared.shutdown_all(timeout);
    }
}
