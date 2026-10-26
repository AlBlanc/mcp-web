//! Persistent headless Chrome/Chromium driven over the Chrome DevTools Protocol (CDP).
//!
//! Compared to spawning `chrome --dump-dom` for every page (1-3 s of process startup plus a
//! fixed virtual-time wait), a long-lived browser gives:
//! - no per-request startup cost (the browser is warmed up when the server starts);
//! - a short, adaptive wait: we return as soon as the DOM has stopped changing;
//! - less work per page: images, fonts, media and common trackers are blocked.
//!
//! The browser is relaunched transparently if it crashes, and shut down after an idle period.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::{
    BlockPattern, EnableParams, SetBlockedUrLsParams, SetUserAgentOverrideParams,
};
use chromiumoxide::page::Page;
use futures::StreamExt;
use tempfile::TempDir;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Maximum number of tabs rendering at the same time.
const MAX_CONCURRENT_PAGES: usize = 4;
/// DOM must stay unchanged for this long before a page is considered rendered.
const DOM_QUIET_WINDOW: Duration = Duration::from_millis(500);
/// Polling interval while waiting for the DOM to settle.
const DOM_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often the idle reaper checks whether the browser can be shut down.
const REAPER_INTERVAL: Duration = Duration::from_secs(30);
/// Default idle time after which the browser is shut down (relaunched on demand).
const DEFAULT_IDLE_SECS: u64 = 300;

/// URL patterns blocked in every tab: they never contribute text content.
const BLOCKED_URL_PATTERNS: &[&str] = &[
    // Images
    "*://*:*/*.png",
    "*://*:*/*.jpg",
    "*://*:*/*.jpeg",
    "*://*:*/*.gif",
    "*://*:*/*.webp",
    "*://*:*/*.avif",
    "*://*:*/*.svg",
    "*://*:*/*.ico",
    // Fonts
    "*://*:*/*.woff",
    "*://*:*/*.woff2",
    "*://*:*/*.ttf",
    "*://*:*/*.otf",
    // Media
    "*://*:*/*.mp4",
    "*://*:*/*.webm",
    "*://*:*/*.mp3",
    "*://*:*/*.m3u8",
    // Analytics / ads / trackers
    "*://*.google-analytics.com:*/*",
    "*://*.googletagmanager.com:*/*",
    "*://*.doubleclick.net:*/*",
    "*://*.googlesyndication.com:*/*",
    "*://*.facebook.net:*/*",
    "*://*.hotjar.com:*/*",
    "*://*.segment.io:*/*",
    "*://*.clarity.ms:*/*",
];

/// The browser could not be launched or connected to. The caller may fall back to the
/// one-shot `--dump-dom` rendering path.
#[derive(Debug, thiserror::Error)]
#[error("failed to launch headless browser: {0}")]
pub struct LaunchError(pub String);

struct Instance {
    browser: Browser,
    /// Keeps the temporary profile directory alive for the lifetime of the browser.
    _profile: TempDir,
    closed: Arc<AtomicBool>,
    handler_task: JoinHandle<()>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // The child process is killed when `browser` is dropped (`kill_on_drop`).
        self.handler_task.abort();
    }
}

/// A lazily launched, shared, auto-restarting headless browser.
pub struct BrowserPool {
    binary: PathBuf,
    user_agent: String,
    state: Mutex<Option<Arc<Instance>>>,
    permits: Semaphore,
    last_used: std::sync::Mutex<Instant>,
    idle_timeout: Duration,
    reaper_started: AtomicBool,
}

impl BrowserPool {
    pub fn new(binary: PathBuf, user_agent: String) -> Arc<Self> {
        let idle_secs = std::env::var("MCP_WEB_BROWSER_IDLE_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_IDLE_SECS);
        Arc::new(Self {
            binary,
            user_agent,
            state: Mutex::new(None),
            permits: Semaphore::new(MAX_CONCURRENT_PAGES),
            last_used: std::sync::Mutex::new(Instant::now()),
            idle_timeout: Duration::from_secs(idle_secs.max(1)),
            reaper_started: AtomicBool::new(false),
        })
    }

    fn touch(&self) {
        if let Ok(mut guard) = self.last_used.lock() {
            *guard = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|g| g.elapsed())
            .unwrap_or_default()
    }

    /// Launch the browser in the background so the first real request does not pay for it.
    pub fn warm_up(self: &Arc<Self>) {
        let pool = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            match pool.instance().await {
                Ok(_) => info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "headless browser warmed up"
                ),
                Err(err) => warn!(error = %err, "headless browser warm-up failed"),
            }
        });
    }

    /// Shut the browser down now (it is relaunched on demand).
    pub async fn shutdown(&self) {
        let mut guard = self.state.lock().await;
        *guard = None;
    }

    /// Render `url` and return the resulting DOM as HTML.
    ///
    /// `wait_ms` is the maximum time spent waiting for the DOM to settle after the page
    /// loads; `timeout_ms` bounds the whole navigation.
    pub async fn render(
        self: &Arc<Self>,
        url: &str,
        wait_ms: u64,
        timeout_ms: u64,
    ) -> Result<String> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| anyhow!("browser pool is closed"))?;

        let mut attempt = 0;
        loop {
            let inst = self.instance().await?;
            match self.render_on(&inst, url, wait_ms, timeout_ms).await {
                Ok(html) => {
                    self.touch();
                    return Ok(html);
                }
                Err(err) if inst.closed.load(Ordering::SeqCst) && attempt == 0 => {
                    attempt += 1;
                    warn!(error = %err, "headless browser died; relaunching and retrying once");
                    self.discard(&inst).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn render_on(
        &self,
        inst: &Instance,
        url: &str,
        wait_ms: u64,
        timeout_ms: u64,
    ) -> Result<String> {
        let page = inst
            .browser
            .new_page("about:blank")
            .await
            .context("failed to open a new browser tab")?;

        let budget = Duration::from_millis(timeout_ms.max(wait_ms + 2_000));
        let max_settle = Duration::from_millis(wait_ms.clamp(500, 20_000));

        let result = tokio::time::timeout(budget, async {
            self.prepare_page(&page).await;
            page.goto(url)
                .await
                .with_context(|| format!("navigation to {url} failed"))?;
            wait_for_dom_to_settle(&page, max_settle).await;
            page.content().await.context("failed to read page content")
        })
        .await;

        // Always close the tab, even on error or timeout, so tabs never leak.
        let _ = page.close().await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(anyhow!(
                "headless browser timed out after {}ms rendering {url}",
                budget.as_millis()
            )),
        }
    }

    /// Per-tab setup: user agent and resource blocking. Best effort: failures are not fatal.
    async fn prepare_page(&self, page: &Page) {
        if let Err(err) = page
            .set_user_agent(SetUserAgentOverrideParams::new(self.user_agent.clone()))
            .await
        {
            debug!(error = %err, "failed to set user agent");
        }
        if let Err(err) = page.execute(EnableParams::default()).await {
            debug!(error = %err, "failed to enable network domain");
        }
        let patterns: Vec<BlockPattern> = BLOCKED_URL_PATTERNS
            .iter()
            .map(|p| BlockPattern::new(*p, true))
            .collect();
        let params = SetBlockedUrLsParams::builder()
            .url_patterns(patterns)
            .build();
        if let Err(err) = page.execute(params).await {
            debug!(error = %err, "failed to set blocked URL patterns");
        }
    }

    /// Return the live browser, launching it if needed. Launches are serialized.
    async fn instance(self: &Arc<Self>) -> Result<Arc<Instance>> {
        let mut guard = self.state.lock().await;
        if let Some(inst) = guard.as_ref()
            && !inst.closed.load(Ordering::SeqCst)
        {
            return Ok(inst.clone());
        }
        let inst = Arc::new(self.launch().await?);
        *guard = Some(inst.clone());
        self.touch();
        self.start_reaper();
        Ok(inst)
    }

    /// Drop `inst` from the pool if it is still the current instance.
    async fn discard(&self, inst: &Arc<Instance>) {
        let mut guard = self.state.lock().await;
        if let Some(current) = guard.as_ref()
            && Arc::ptr_eq(current, inst)
        {
            *guard = None;
        }
    }

    async fn launch(&self) -> Result<Instance> {
        let profile = TempDir::with_prefix("mcp-web-chrome-")
            .map_err(|e| LaunchError(format!("cannot create temporary profile: {e}")))?;

        let mut builder = BrowserConfig::builder()
            .chrome_executable(&self.binary)
            .new_headless_mode()
            .user_data_dir(profile.path())
            .window_size(1440, 900)
            .launch_timeout(Duration::from_secs(20))
            .request_timeout(Duration::from_secs(30))
            .args([
                "--disable-gpu",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-translate",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-dev-shm-usage",
                "--disable-blink-features=AutomationControlled",
            ]);
        if should_disable_sandbox() {
            builder = builder.no_sandbox();
        }
        let config = builder.build().map_err(|e| LaunchError(e.to_string()))?;

        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| LaunchError(e.to_string()))?;

        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = closed.clone();
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
            closed_flag.store(true, Ordering::SeqCst);
        });

        Ok(Instance {
            browser,
            _profile: profile,
            closed,
            handler_task,
        })
    }

    /// Start (once) a background task that shuts the browser down after an idle period.
    fn start_reaper(self: &Arc<Self>) {
        if self.reaper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REAPER_INTERVAL).await;
                let Some(pool) = weak.upgrade() else { break };
                if pool.idle_for() >= pool.idle_timeout
                    && pool.permits.available_permits() == MAX_CONCURRENT_PAGES
                {
                    let mut guard = pool.state.lock().await;
                    if guard.take().is_some() {
                        info!("headless browser shut down after idle period");
                    }
                }
            }
        });
    }
}

/// Chrome's sandbox needs user namespaces, which are often unavailable in containers and CI
/// on Linux. Keep it enabled elsewhere. Override with `MCP_WEB_NO_SANDBOX=0|1`.
fn should_disable_sandbox() -> bool {
    match std::env::var("MCP_WEB_NO_SANDBOX").ok().as_deref() {
        Some("1") | Some("true") => true,
        Some("0") | Some("false") => false,
        _ => cfg!(target_os = "linux"),
    }
}

/// Wait until the DOM (and the number of loaded resources) stops changing for
/// `DOM_QUIET_WINDOW`, or until `max_wait` elapses. SPAs typically render within a few
/// hundred milliseconds after the load event, so this is much faster than a fixed delay.
async fn wait_for_dom_to_settle(page: &Page, max_wait: Duration) {
    const SIGNAL_JS: &str = "[document.documentElement ? document.documentElement.outerHTML.length : 0, performance.getEntriesByType('resource').length]";

    let started = Instant::now();
    let mut last: Option<(i64, i64)> = None;
    let mut stable_since = Instant::now();

    loop {
        let signal = match page.evaluate(SIGNAL_JS).await {
            Ok(res) => res.into_value::<(i64, i64)>().ok(),
            Err(_) => None,
        };

        if let Some(sig) = signal {
            if last != Some(sig) {
                last = Some(sig);
                stable_since = Instant::now();
            } else if sig.0 > 0 && stable_since.elapsed() >= DOM_QUIET_WINDOW {
                break;
            }
        }

        if started.elapsed() >= max_wait {
            break;
        }
        tokio::time::sleep(DOM_POLL_INTERVAL).await;
    }
}
