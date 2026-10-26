use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use moka::future::Cache;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::cdp::{BrowserPool, LaunchError};

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

/// Rendering mode for fetching web pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    /// Fetch via fast HTTP first; automatically upgrade to Headless Chrome if an SPA shell
    /// or JavaScript challenge is detected.
    #[default]
    Auto,
    /// Always render the page inside Headless Chrome (best for known SPAs).
    Browser,
    /// Only use fast static HTTP requests (never spawn a headless browser).
    Http,
}

impl RenderMode {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("browser") | Some("spa") | Some("js") => Self::Browser,
            Some("http") | Some("static") => Self::Http,
            _ => Self::Auto,
        }
    }
}

/// Options for fetching a single web page.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub render_mode: RenderMode,
    /// Virtual time budget (in milliseconds) granted to Headless Chrome for JS execution / SPA hydration.
    pub wait_ms: u64,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            render_mode: RenderMode::Auto,
            wait_ms: 3500,
            timeout_ms: 25000,
        }
    }
}

/// Raw HTML response along with metadata about how it was retrieved.
#[derive(Debug, Clone, Default)]
pub struct FetchedDocument {
    pub final_url: String,
    pub html: String,
    pub rendered_with_browser: bool,
    pub spa_detected: bool,
    pub status_code: Option<u16>,
    /// `true` when this document was served from the in-memory cache (or coalesced
    /// with an identical in-flight request) instead of being fetched from the network.
    pub cached: bool,
    /// Wall-clock time spent by this call, in milliseconds.
    pub elapsed_ms: u64,
}

/// Default time-to-live for cached pages.
const DEFAULT_PAGE_CACHE_TTL_SECS: u64 = 300;
/// Maximum total size (in bytes of HTML) kept in the page cache.
const PAGE_CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Static HTTP stage timeout cap in `RenderMode::Auto`, so a slow server cannot delay the browser upgrade.
const AUTO_HTTP_TIMEOUT_MS: u64 = 10_000;

/// Reads a TTL (in seconds) from `MCP_WEB_CACHE_TTL_SECS`, falling back to `default_secs`.
/// A value of `0` effectively disables caching.
pub fn cache_ttl(default_secs: u64) -> Duration {
    let secs = std::env::var("MCP_WEB_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default_secs);
    Duration::from_secs(secs).max(Duration::from_millis(1))
}

/// Manages HTTP client and Headless Chrome/Chromium execution for SPA rendering.
#[derive(Clone)]
pub struct WebFetcher {
    http: Client,
    browser_path: Option<PathBuf>,
    user_agent: String,
    page_cache: Cache<String, Arc<FetchedDocument>>,
    /// Persistent CDP browser (`None` when no Chrome is installed or CDP is disabled).
    pool: Option<Arc<BrowserPool>>,
}

impl WebFetcher {
    pub fn new(
        custom_browser_path: Option<PathBuf>,
        custom_user_agent: Option<String>,
    ) -> Result<Self> {
        let user_agent = custom_user_agent.unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        let http = Client::builder()
            .user_agent(&user_agent)
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::limited(10))
            .timeout(Duration::from_secs(20))
            .build()
            .context("Failed to initialize HTTP client")?;

        let browser_path = custom_browser_path.or_else(discover_chrome_binary);
        if let Some(ref path) = browser_path {
            debug!("Discovered headless browser binary at: {}", path.display());
        } else {
            warn!(
                "No Chrome/Chromium binary found on system; SPA browser rendering will be unavailable unless CHROME_PATH is set."
            );
        }

        let page_cache = Cache::builder()
            .weigher(|_key: &String, doc: &Arc<FetchedDocument>| {
                u32::try_from(doc.html.len()).unwrap_or(u32::MAX)
            })
            .max_capacity(PAGE_CACHE_MAX_BYTES)
            .time_to_live(cache_ttl(DEFAULT_PAGE_CACHE_TTL_SECS))
            .build();

        let pool = match (&browser_path, std::env::var("MCP_WEB_CDP").ok().as_deref()) {
            (Some(path), Some("0") | Some("false")) => {
                debug!("Persistent browser disabled via MCP_WEB_CDP; using one-shot rendering");
                let _ = path;
                None
            }
            (Some(path), _) => Some(BrowserPool::new(path.clone(), user_agent.clone())),
            (None, _) => None,
        };

        Ok(Self {
            http,
            browser_path,
            user_agent,
            page_cache,
            pool,
        })
    }

    pub fn http_client(&self) -> &Client {
        &self.http
    }

    pub fn has_browser(&self) -> bool {
        self.browser_path.is_some()
    }

    pub fn browser_path(&self) -> Option<&Path> {
        self.browser_path.as_deref()
    }

    /// Fetch a web page using the requested `RenderMode` (`Auto`, `Browser`, or `Http`).
    ///
    /// Results are cached in memory (see `MCP_WEB_CACHE_TTL_SECS`), and identical concurrent
    /// requests are coalesced into a single network fetch. Failures are never cached.
    pub async fn fetch(&self, url: &str, options: &FetchOptions) -> Result<FetchedDocument> {
        let started = Instant::now();
        let key = format!("{url}|{:?}|{}", options.render_mode, options.wait_ms);

        let entry = self
            .page_cache
            .entry(key)
            .or_try_insert_with(async { self.fetch_uncached(url, options).await.map(Arc::new) })
            .await
            .map_err(|err| anyhow!("{err:#}"))?;

        let cached = !entry.is_fresh();
        let mut doc = (*entry.into_value()).clone();
        doc.cached = cached;
        doc.elapsed_ms = started.elapsed().as_millis() as u64;

        info!(
            url = %url,
            elapsed_ms = doc.elapsed_ms,
            cached = doc.cached,
            browser = doc.rendered_with_browser,
            spa_detected = doc.spa_detected,
            html_bytes = doc.html.len(),
            "fetch completed"
        );
        Ok(doc)
    }

    async fn fetch_uncached(&self, url: &str, options: &FetchOptions) -> Result<FetchedDocument> {
        let parsed = url::Url::parse(url).with_context(|| format!("Invalid URL: {url}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            bail!(
                "Unsupported URL scheme '{}': only http and https are allowed",
                parsed.scheme()
            );
        }

        match options.render_mode {
            RenderMode::Browser => {
                let html = self
                    .fetch_with_browser(url, options.wait_ms, options.timeout_ms)
                    .await?;
                Ok(FetchedDocument {
                    final_url: url.to_string(),
                    html,
                    rendered_with_browser: true,
                    spa_detected: true,
                    status_code: Some(200),
                    ..Default::default()
                })
            }
            RenderMode::Http => self.fetch_with_http(url, options.timeout_ms).await,
            RenderMode::Auto => {
                // Step 1: Try fast HTTP request first
                match self
                    .fetch_with_http(url, options.timeout_ms.min(AUTO_HTTP_TIMEOUT_MS))
                    .await
                {
                    Ok(mut doc) => {
                        let spa = is_spa_shell(&doc.html);
                        doc.spa_detected = spa;

                        // Step 2: If an SPA shell or JS interstitial is detected and we have a browser, upgrade!
                        if spa && self.has_browser() {
                            info!(
                                url = %url,
                                "SPA shell or JS-dependent page detected; upgrading fetch to Headless Chrome"
                            );
                            match self
                                .fetch_with_browser(
                                    &doc.final_url,
                                    options.wait_ms,
                                    options.timeout_ms,
                                )
                                .await
                            {
                                Ok(rendered_html) if !rendered_html.trim().is_empty() => {
                                    doc.html = rendered_html;
                                    doc.rendered_with_browser = true;
                                }
                                Ok(_) => {
                                    warn!(url = %url, "Headless browser returned empty DOM; keeping HTTP response");
                                }
                                Err(err) => {
                                    warn!(url = %url, error = %err, "Headless browser fallback failed; keeping HTTP response");
                                }
                            }
                        }
                        Ok(doc)
                    }
                    Err(http_err) => {
                        // If static HTTP failed (e.g. TLS fingerprint block / 403) and browser is available, try browser
                        if self.has_browser() {
                            info!(
                                url = %url,
                                error = %http_err,
                                "Static HTTP request failed; attempting fallback via Headless Chrome"
                            );
                            let html = self
                                .fetch_with_browser(url, options.wait_ms, options.timeout_ms)
                                .await
                                .with_context(|| {
                                    format!("Both HTTP ({http_err}) and Headless Browser fetch failed for {url}")
                                })?;
                            Ok(FetchedDocument {
                                final_url: url.to_string(),
                                html,
                                rendered_with_browser: true,
                                spa_detected: true,
                                status_code: Some(200),
                                ..Default::default()
                            })
                        } else {
                            Err(http_err)
                        }
                    }
                }
            }
        }
    }

    /// Perform a fast static HTTP GET with browser-like headers.
    pub async fn fetch_with_http(&self, url: &str, timeout_ms: u64) -> Result<FetchedDocument> {
        let resp = self
            .http
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cache-Control", "no-cache")
            .header("Upgrade-Insecure-Requests", "1")
            .timeout(Duration::from_millis(timeout_ms))
            .send()
            .await
            .with_context(|| format!("HTTP GET failed for {url}"))?;

        let status = resp.status();
        let final_url = resp.url().to_string();

        // If status is 403/429/503, return an error so `RenderMode::Auto` can trigger headless browser fallback
        if status.as_u16() == 403 || status.as_u16() == 429 || status.as_u16() == 503 {
            bail!("HTTP status {} returned for {}", status, url);
        }

        let html = resp
            .text()
            .await
            .with_context(|| format!("Failed reading response body from {url}"))?;

        Ok(FetchedDocument {
            final_url,
            html,
            rendered_with_browser: false,
            spa_detected: false,
            status_code: Some(status.as_u16()),
            ..Default::default()
        })
    }

    /// Render a page in headless Chrome and return the final DOM.
    ///
    /// Uses the persistent CDP browser (fast: no startup cost, adaptive wait) and falls back to
    /// the one-shot `--dump-dom` path only if the persistent browser cannot be launched.
    /// Set `MCP_WEB_CDP=0` to always use the one-shot path.
    pub async fn fetch_with_browser(
        &self,
        url: &str,
        wait_ms: u64,
        timeout_ms: u64,
    ) -> Result<String> {
        if let Some(pool) = &self.pool {
            match pool.render(url, wait_ms, timeout_ms).await {
                Ok(html) => return Ok(html),
                Err(err) if err.downcast_ref::<LaunchError>().is_some() => {
                    warn!(error = %err, "persistent browser unavailable; using one-shot rendering");
                }
                Err(err) => return Err(err),
            }
        }
        self.fetch_with_browser_oneshot(url, wait_ms, timeout_ms)
            .await
    }

    /// Start the persistent browser in the background so the first SPA request is fast.
    pub fn warm_up_browser(&self) {
        if let Some(pool) = &self.pool {
            pool.warm_up();
        }
    }

    /// Shut the persistent browser down (it is relaunched on demand).
    pub async fn shutdown_browser(&self) {
        if let Some(pool) = &self.pool {
            pool.shutdown().await;
        }
    }

    /// Render a page using a fresh Headless Chrome process (`--headless=new --dump-dom`) with an
    /// isolated profile and a virtual time budget so Single Page Applications (React, Vue,
    /// Next.js, Angular, etc.) finish hydrating their DOM before extraction. Slower than the
    /// persistent browser because it pays process startup on every call.
    pub async fn fetch_with_browser_oneshot(
        &self,
        url: &str,
        wait_ms: u64,
        timeout_ms: u64,
    ) -> Result<String> {
        let browser_bin = self
            .browser_path
            .as_ref()
            .ok_or_else(|| anyhow!("No Chrome/Chromium binary available for SPA rendering"))?;

        let temp_profile = TempDir::with_prefix("mcp-web-chrome-")
            .context("Failed to create temporary Chrome profile directory")?;

        let effective_wait = wait_ms.clamp(500, 20_000);
        let effective_timeout = timeout_ms.max(effective_wait + 5_000);

        let mut cmd = Command::new(browser_bin);
        cmd.arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--hide-scrollbars")
            .arg("--mute-audio")
            .arg("--no-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--window-size=1440,900")
            .arg(format!("--user-agent={}", self.user_agent))
            .arg(format!("--user-data-dir={}", temp_profile.path().display()))
            .arg(format!("--virtual-time-budget={effective_wait}"))
            .arg("--dump-dom")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        debug!(
            browser = %browser_bin.display(),
            url = %url,
            wait_ms = effective_wait,
            "Spawning Headless Chrome for SPA DOM rendering"
        );

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn browser at {}", browser_bin.display()))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture browser stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture browser stderr"))?;

        let read_future = async {
            let mut out_buf = Vec::with_capacity(64 * 1024);
            let mut err_buf = Vec::with_capacity(4 * 1024);
            let (out_res, err_res, status_res) = tokio::join!(
                stdout.read_to_end(&mut out_buf),
                stderr.read_to_end(&mut err_buf),
                child.wait()
            );
            out_res?;
            let _ = err_res;
            let status = status_res?;
            Ok::<(Vec<u8>, Vec<u8>, std::process::ExitStatus), std::io::Error>((
                out_buf, err_buf, status,
            ))
        };

        let (out_buf, err_buf, status) =
            timeout(Duration::from_millis(effective_timeout), read_future)
                .await
                .map_err(|_| {
                    anyhow!("Headless Chrome timed out after {effective_timeout}ms rendering {url}")
                })?
                .context("Error reading output from Headless Chrome")?;

        let dom = String::from_utf8_lossy(&out_buf).into_owned();
        if dom.trim().is_empty() {
            let err_str = String::from_utf8_lossy(&err_buf);
            bail!(
                "Headless Chrome exited with status {status} and empty DOM. Stderr: {}",
                err_str.chars().take(500).collect::<String>()
            );
        }

        Ok(dom)
    }
}

/// Automatically locates an installed Chrome / Chromium / Brave / Edge binary across macOS, Linux, and Windows.
pub fn discover_chrome_binary() -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("CHROME_PATH") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Some(p);
        }
    }

    let candidates = [
        // macOS standard locations
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        // Linux standard locations
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];

    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Some(path);
        }
    }

    // Also check PATH for common binary names
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for bin_name in ["google-chrome", "chromium", "chromium-browser", "chrome"] {
                let full = dir.join(bin_name);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }

    None
}

/// Heuristic detector that inspects raw HTML to determine whether a page is an unhydrated
/// Single Page Application (SPA) shell or requires JavaScript execution to display its content.
pub fn is_spa_shell(html: &str) -> bool {
    let trimmed = html.trim();
    if trimmed.is_empty() {
        return true;
    }

    let document = Html::parse_document(trimmed);

    // 1. Check <noscript> messages that explicitly complain about JavaScript being disabled
    if let Ok(noscript_sel) = Selector::parse("noscript") {
        for el in document.select(&noscript_sel) {
            let text = el.text().collect::<String>().to_ascii_lowercase();
            if text.contains("enable javascript")
                || text.contains("requires javascript")
                || text.contains("need to enable javascript")
                || text.contains("javascript is required")
                || text.contains("without javascript enabled")
            {
                // Check if the rest of the body has very little visible text
                let visible_chars = extract_visible_body_text_len(&document);
                if visible_chars < 600 {
                    return true;
                }
            }
        }
    }

    // 2. Check common SPA mount roots (#root, #app, #__next, #__nuxt, app-root)
    let spa_selectors = [
        "#root",
        "#app",
        "#__next",
        "#__nuxt",
        "app-root",
        "[data-reactroot]",
        "#svelte",
    ];

    let visible_chars = extract_visible_body_text_len(&document);
    let script_count = Selector::parse("script")
        .map(|s| document.select(&s).count())
        .unwrap_or(0);

    for sel_str in spa_selectors {
        if let Ok(sel) = Selector::parse(sel_str)
            && let Some(root_el) = document.select(&sel).next()
        {
            let root_text: String = root_el.text().collect();
            let root_text_len = root_text.split_whitespace().collect::<String>().len();
            // Empty or near-empty SPA mount container ("Loading...", "")
            if root_text_len < 120 && visible_chars < 400 && script_count >= 1 {
                return true;
            }
        }
    }

    // 3. High script-to-text ratio: many scripts but almost no visible text in <body>
    if script_count >= 3 && visible_chars < 220 {
        return true;
    }

    // 4. Check for common JS challenge interstitials
    let lower = trimmed.to_ascii_lowercase();
    if (lower.contains("just a moment...") || lower.contains("checking your browser"))
        && visible_chars < 500
    {
        return true;
    }

    false
}

fn extract_visible_body_text_len(document: &Html) -> usize {
    let Ok(body_sel) = Selector::parse("body") else {
        return 0;
    };
    let Some(body) = document.select(&body_sel).next() else {
        return 0;
    };

    // Measure text inside body excluding <script>, <style>, <noscript>, <svg>
    let mut total_text_len = 0usize;
    for node in body.descendants() {
        if let Some(text) = node.value().as_text() {
            // Check if any ancestor is script/style/noscript/svg
            let mut hidden_ancestor = false;
            for ancestor in node.ancestors() {
                if let Some(el) = ancestor.value().as_element() {
                    let tag = el.name();
                    if matches!(tag, "script" | "style" | "noscript" | "svg" | "template") {
                        hidden_ancestor = true;
                        break;
                    }
                }
            }
            if !hidden_ancestor {
                total_text_len += text.trim().len();
            }
        }
    }
    total_text_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Starts a tiny local HTTP server that counts requests and answers slowly enough
    /// for concurrent callers to overlap.
    async fn spawn_counting_server() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits_srv.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                    hits.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let body = "<html><body><main><p>hello</p></main></body></html>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}/page"), hits)
    }

    #[tokio::test]
    async fn caches_pages_and_coalesces_concurrent_requests() {
        let (url, hits) = spawn_counting_server().await;
        let fetcher = WebFetcher::new(None, None).unwrap();
        let opts = FetchOptions {
            render_mode: RenderMode::Http,
            ..Default::default()
        };

        // Three identical concurrent requests must produce a single network hit.
        let (a, b, c) = tokio::join!(
            fetcher.fetch(&url, &opts),
            fetcher.fetch(&url, &opts),
            fetcher.fetch(&url, &opts)
        );
        let docs = [a.unwrap(), b.unwrap(), c.unwrap()];
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(docs.iter().filter(|d| !d.cached).count(), 1);

        // A later request is a cache hit and much faster than the 150 ms server delay.
        let again = fetcher.fetch(&url, &opts).await.unwrap();
        assert!(again.cached);
        assert!(again.elapsed_ms < 100);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_cache_failures() {
        // Nothing listens on this port, so the fetch fails and must not be cached.
        let fetcher = WebFetcher::new(None, None).unwrap();
        let opts = FetchOptions {
            render_mode: RenderMode::Http,
            timeout_ms: 1000,
            ..Default::default()
        };
        let url = "http://127.0.0.1:9/never";
        assert!(fetcher.fetch(url, &opts).await.is_err());
        assert!(fetcher.fetch(url, &opts).await.is_err());
    }

    #[test]
    fn detects_react_and_vue_spa_shells() {
        let react_shell = r#"
            <!DOCTYPE html>
            <html>
            <head><title>React App</title></head>
            <body>
                <noscript>You need to enable JavaScript to run this app.</noscript>
                <div id="root"></div>
                <script src="/static/js/bundle.js"></script>
            </body>
            </html>
        "#;
        assert!(is_spa_shell(react_shell));

        let vue_shell = r#"
            <!DOCTYPE html>
            <html>
            <body>
                <div id="app">Loading...</div>
                <script type="module" src="/src/main.ts"></script>
            </body>
            </html>
        "#;
        assert!(is_spa_shell(vue_shell));
    }

    #[test]
    fn does_not_flag_hydrated_or_ssr_articles_as_spa_shell() {
        let ssr_article = format!(
            r#"
            <!DOCTYPE html>
            <html>
            <head><title>Rust Async Architecture</title></head>
            <body>
                <div id="__next">
                    <main>
                        <article>
                            <h1>Building High-Performance Servers in Rust</h1>
                            <p>{}</p>
                        </article>
                    </main>
                </div>
                <script src="/_next/static/chunks/main.js"></script>
            </body>
            </html>
            "#,
            "Rust provides zero-cost abstractions and fearless concurrency for modern network services. ".repeat(15)
        );
        assert!(!is_spa_shell(&ssr_article));
    }
}
