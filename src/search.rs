use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use moka::future::Cache;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use url::Url;

use crate::browser::{WebFetcher, cache_ttl};
use crate::markdown::resolve_and_clean_url;

/// Default time-to-live for cached search responses.
const DEFAULT_SEARCH_CACHE_TTL_SECS: u64 = 600;
/// Default time Google is skipped after a block/failure (circuit breaker cooldown).
const DEFAULT_GOOGLE_COOLDOWN_SECS: u64 = 300;
/// Timeout for each Google HTTP attempt in `auto` mode (fail fast, DuckDuckGo is the fallback).
const GOOGLE_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for each DuckDuckGo HTTP attempt.
const DDG_HTTP_TIMEOUT: Duration = Duration::from_secs(6);

/// Which search engine to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    /// Try Google first; automatically fall back to DuckDuckGo if Google is unavailable or blocked.
    #[default]
    Auto,
    /// Only use Google Search.
    Google,
    /// Only use DuckDuckGo Search.
    DuckDuckGo,
}

impl SearchEngine {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("google") => Self::Google,
            Some("duckduckgo") | Some("ddg") => Self::DuckDuckGo,
            _ => Self::Auto,
        }
    }
}

/// Parameters for a web search request.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub max_results: usize,
    pub engine: SearchEngine,
    pub language: String,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            query: String::new(),
            max_results: 10,
            engine: SearchEngine::Auto,
            language: "en".to_string(),
        }
    }
}

/// A single organic web search result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResultItem {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Complete response from the web search pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub provider_used: String,
    pub fallback_triggered: bool,
    pub fallback_reason: Option<String>,
    pub results: Vec<SearchResultItem>,
}

/// A search response plus how it was obtained (cache hit, latency).
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub response: SearchResponse,
    /// `true` when served from the cache or coalesced with an identical in-flight search.
    pub cached: bool,
    /// Wall-clock time spent by this call, in milliseconds.
    pub elapsed_ms: u64,
}

impl SearchOutcome {
    /// Short human/LLM-readable timing label, e.g. `searched in 412 ms` or `served from cache in 0 ms`.
    pub fn timing_label(&self) -> String {
        if self.cached {
            format!("served from cache in {} ms", self.elapsed_ms)
        } else {
            format!("searched in {} ms", self.elapsed_ms)
        }
    }
}

/// Executes web searches using Google first with automatic fallback to DuckDuckGo.
///
/// Speed features:
/// - responses are cached in memory and identical concurrent searches are coalesced;
/// - a circuit breaker skips Google for a cooldown period after it blocks us, so subsequent
///   searches go straight to DuckDuckGo instead of wasting seconds on a known-blocked engine.
#[derive(Clone)]
pub struct WebSearcher {
    fetcher: WebFetcher,
    cache: Cache<String, Arc<SearchResponse>>,
    google_blocked_until: Arc<Mutex<Option<Instant>>>,
    google_cooldown: Duration,
}

impl WebSearcher {
    pub fn new(fetcher: WebFetcher) -> Self {
        let cache = Cache::builder()
            .max_capacity(500)
            .time_to_live(cache_ttl(DEFAULT_SEARCH_CACHE_TTL_SECS))
            .build();
        let cooldown_secs = std::env::var("MCP_WEB_GOOGLE_COOLDOWN_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_GOOGLE_COOLDOWN_SECS);
        Self {
            fetcher,
            cache,
            google_blocked_until: Arc::new(Mutex::new(None)),
            google_cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    /// Remaining cooldown if the Google circuit breaker is open.
    fn google_cooldown_remaining(&self) -> Option<Duration> {
        let guard = self.google_blocked_until.lock().ok()?;
        let until = (*guard)?;
        until.checked_duration_since(Instant::now())
    }

    fn trip_google_breaker(&self) {
        if self.google_cooldown.is_zero() {
            return;
        }
        if let Ok(mut guard) = self.google_blocked_until.lock() {
            *guard = Some(Instant::now() + self.google_cooldown);
        }
    }

    fn reset_google_breaker(&self) {
        if let Ok(mut guard) = self.google_blocked_until.lock() {
            *guard = None;
        }
    }

    /// Run a search, using the cache and request coalescing. Failures are never cached.
    pub async fn search(&self, options: &SearchOptions) -> Result<SearchOutcome> {
        let started = Instant::now();
        let key = format!(
            "{}|{}|{:?}|{}",
            options.query.trim().to_lowercase(),
            options.max_results.clamp(1, 25),
            options.engine,
            options.language
        );

        let entry = self
            .cache
            .entry(key)
            .or_try_insert_with(async { self.search_uncached(options).await.map(Arc::new) })
            .await
            .map_err(|err| anyhow!("{err:#}"))?;

        let cached = !entry.is_fresh();
        let response = (*entry.into_value()).clone();
        let outcome = SearchOutcome {
            response,
            cached,
            elapsed_ms: started.elapsed().as_millis() as u64,
        };

        info!(
            query = %options.query,
            provider = %outcome.response.provider_used,
            results = outcome.response.results.len(),
            elapsed_ms = outcome.elapsed_ms,
            cached = outcome.cached,
            "search completed"
        );
        Ok(outcome)
    }

    async fn search_uncached(&self, options: &SearchOptions) -> Result<SearchResponse> {
        let query = options.query.trim();
        if query.is_empty() {
            bail!("Search query must not be empty");
        }
        let max_results = options.max_results.clamp(1, 25);

        match options.engine {
            SearchEngine::Google => {
                let results = self
                    .search_google(query, max_results, &options.language, true)
                    .await?;
                Ok(SearchResponse {
                    query: query.to_string(),
                    provider_used: "Google".to_string(),
                    fallback_triggered: false,
                    fallback_reason: None,
                    results,
                })
            }
            SearchEngine::DuckDuckGo => {
                let results = self
                    .search_duckduckgo(query, max_results, &options.language)
                    .await?;
                Ok(SearchResponse {
                    query: query.to_string(),
                    provider_used: "DuckDuckGo".to_string(),
                    fallback_triggered: false,
                    fallback_reason: None,
                    results,
                })
            }
            SearchEngine::Auto => {
                // Circuit breaker: if Google blocked us recently, go straight to DuckDuckGo.
                if let Some(remaining) = self.google_cooldown_remaining() {
                    let reason = format!(
                        "Google skipped: blocked recently (circuit breaker open for another {}s)",
                        remaining.as_secs()
                    );
                    debug!(query = %query, "{reason}");
                    let ddg_results = self
                        .search_duckduckgo(query, max_results, &options.language)
                        .await
                        .context("Google skipped by circuit breaker and DuckDuckGo failed")?;
                    return Ok(SearchResponse {
                        query: query.to_string(),
                        provider_used: "DuckDuckGo (Google skipped)".to_string(),
                        fallback_triggered: true,
                        fallback_reason: Some(reason),
                        results: ddg_results,
                    });
                }

                // Step 1: Try Google first as required (fail fast, no headless browser in auto mode)
                match self
                    .search_google(query, max_results, &options.language, false)
                    .await
                {
                    Ok(results) if !results.is_empty() => {
                        info!(
                            query = %query,
                            count = results.len(),
                            "Google search succeeded"
                        );
                        self.reset_google_breaker();
                        Ok(SearchResponse {
                            query: query.to_string(),
                            provider_used: "Google".to_string(),
                            fallback_triggered: false,
                            fallback_reason: None,
                            results,
                        })
                    }
                    Ok(_) => {
                        let reason =
                            "Google returned 0 organic results (likely JS/CAPTCHA interstitial)"
                                .to_string();
                        self.trip_google_breaker();
                        warn!(query = %query, "Google returned 0 results; falling back to DuckDuckGo");
                        let ddg_results = self
                            .search_duckduckgo(query, max_results, &options.language)
                            .await
                            .context(
                                "DuckDuckGo fallback also failed after Google returned 0 results",
                            )?;
                        Ok(SearchResponse {
                            query: query.to_string(),
                            provider_used: "DuckDuckGo (fallback from Google)".to_string(),
                            fallback_triggered: true,
                            fallback_reason: Some(reason),
                            results: ddg_results,
                        })
                    }
                    Err(google_err) => {
                        let reason = format!("{google_err:#}");
                        self.trip_google_breaker();
                        warn!(
                            query = %query,
                            error = %reason,
                            "Google search failed; automatically falling back to DuckDuckGo"
                        );
                        let ddg_results = self
                            .search_duckduckgo(query, max_results, &options.language)
                            .await
                            .with_context(|| {
                                format!("Both Google ({reason}) and DuckDuckGo fallback failed")
                            })?;
                        Ok(SearchResponse {
                            query: query.to_string(),
                            provider_used: "DuckDuckGo (fallback from Google)".to_string(),
                            fallback_triggered: true,
                            fallback_reason: Some(reason),
                            results: ddg_results,
                        })
                    }
                }
            }
        }
    }

    /// Search Google using:
    /// 1. Google Custom Search JSON API (if `GOOGLE_API_KEY` and `GOOGLE_CX` are set)
    /// 2. Google Web Search HTTP endpoint
    /// 3. Google Web Search rendered via Headless Chrome (if HTTP hits a JS challenge)
    async fn search_google(
        &self,
        query: &str,
        max_results: usize,
        language: &str,
        allow_browser: bool,
    ) -> Result<Vec<SearchResultItem>> {
        // 1A: If official Google Custom Search API credentials are configured, use them first
        if let (Ok(api_key), Ok(cx)) = (std::env::var("GOOGLE_API_KEY"), std::env::var("GOOGLE_CX"))
            && !api_key.trim().is_empty()
            && !cx.trim().is_empty()
        {
            match self
                .search_google_custom_api(query, max_results, &api_key, &cx)
                .await
            {
                Ok(items) if !items.is_empty() => return Ok(items),
                Ok(_) => {
                    debug!("Google Custom Search API returned 0 results; trying web search")
                }
                Err(e) => {
                    warn!(error = %e, "Google Custom Search API error; trying web search")
                }
            }
        }

        let encoded_q = urlencoding::encode(query);
        let num = (max_results + 5).min(30);
        let google_url =
            format!("https://www.google.com/search?q={encoded_q}&num={num}&hl={language}&gbv=1");

        // 1B: Try fast HTTP request with Google consent cookies
        let http_res = self
            .fetcher
            .http_client()
            .get(&google_url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", format!("{language},en-US;q=0.9,en;q=0.8"))
            .header(
                "Cookie",
                "CONSENT=YES+cb.20240116-07-p0.en+FX+111; SOCS=CAISHAgBEhJnd3NfMjAyNDAxMTYtMF9SQzIaAmVuIAEaBgiA_LyuBg",
            )
            .timeout(GOOGLE_HTTP_TIMEOUT)
            .send()
            .await;

        if let Ok(resp) = http_res {
            let final_url = resp.url().to_string();
            let status = resp.status();
            if status.is_success()
                && !final_url.contains("/sorry/")
                && let Ok(html) = resp.text().await
                && !is_google_bot_block(&html)
            {
                let parsed = parse_google_serp_html(&html, max_results);
                if !parsed.is_empty() {
                    return Ok(parsed);
                }
            }
        }

        // 1C: Try Headless Chrome on Google Search if available (slow: skipped in `auto` mode)
        if allow_browser && self.fetcher.has_browser() {
            let browser_google_url =
                format!("https://www.google.com/search?q={encoded_q}&num={num}&hl={language}");
            debug!(url = %browser_google_url, "Attempting Google SERP render via Headless Chrome");
            if let Ok(dom) = self
                .fetcher
                .fetch_with_browser(&browser_google_url, 2500, 12000)
                .await
                && !is_google_bot_block(&dom)
            {
                let parsed = parse_google_serp_html(&dom, max_results);
                if !parsed.is_empty() {
                    return Ok(parsed);
                }
            }
        }

        bail!(
            "Google search blocked by anti-bot challenge or returned no parseable organic results"
        )
    }

    async fn search_google_custom_api(
        &self,
        query: &str,
        max_results: usize,
        api_key: &str,
        cx: &str,
    ) -> Result<Vec<SearchResultItem>> {
        let num = max_results.clamp(1, 10);
        let url = format!(
            "https://www.googleapis.com/customsearch/v1?key={}&cx={}&q={}&num={}",
            urlencoding::encode(api_key),
            urlencoding::encode(cx),
            urlencoding::encode(query),
            num
        );

        let resp = self
            .fetcher
            .http_client()
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("Failed calling Google Custom Search API")?;

        if !resp.status().is_success() {
            bail!("Google Custom Search API returned HTTP {}", resp.status());
        }

        let json: serde_json::Value = resp.json().await?;
        let mut results = Vec::new();
        if let Some(items) = json.get("items").and_then(|v| v.as_array()) {
            for item in items.iter().take(max_results) {
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let link = item
                    .get("link")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let snippet = item
                    .get("snippet")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");

                if !title.is_empty() && !link.is_empty() {
                    results.push(SearchResultItem {
                        title,
                        url: link,
                        snippet,
                    });
                }
            }
        }

        Ok(results)
    }

    /// Search DuckDuckGo using:
    /// 1. DuckDuckGo HTML endpoint (`https://html.duckduckgo.com/html/`)
    /// 2. DuckDuckGo Lite endpoint (`https://lite.duckduckgo.com/lite/`)
    /// 3. Headless Chrome fallback
    async fn search_duckduckgo(
        &self,
        query: &str,
        max_results: usize,
        language: &str,
    ) -> Result<Vec<SearchResultItem>> {
        let encoded_q = urlencoding::encode(query);
        let ddg_html_url = format!("https://html.duckduckgo.com/html/?q={encoded_q}");

        // 2A: DuckDuckGo HTML endpoint
        let resp = self
            .fetcher
            .http_client()
            .get(&ddg_html_url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(
                "Accept-Language",
                format!("{language},en-US;q=0.9,en;q=0.8"),
            )
            .header("Referer", "https://html.duckduckgo.com/")
            .timeout(DDG_HTTP_TIMEOUT)
            .send()
            .await;

        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(html) = r.text().await
        {
            let parsed = parse_duckduckgo_html(&html, max_results);
            if !parsed.is_empty() {
                return Ok(parsed);
            }
        }

        // 2B: DuckDuckGo Lite endpoint
        let ddg_lite_url = format!("https://lite.duckduckgo.com/lite/?q={encoded_q}");
        if let Ok(r) = self
            .fetcher
            .http_client()
            .get(&ddg_lite_url)
            .header("Accept", "text/html")
            .header("Referer", "https://lite.duckduckgo.com/")
            .timeout(DDG_HTTP_TIMEOUT)
            .send()
            .await
            && r.status().is_success()
            && let Ok(html) = r.text().await
        {
            let parsed = parse_duckduckgo_html(&html, max_results);
            if !parsed.is_empty() {
                return Ok(parsed);
            }
        }

        // 2C: Headless Chrome fallback on DuckDuckGo HTML
        if self.fetcher.has_browser()
            && let Ok(dom) = self
                .fetcher
                .fetch_with_browser(&ddg_html_url, 2500, 12000)
                .await
        {
            let parsed = parse_duckduckgo_html(&dom, max_results);
            if !parsed.is_empty() {
                return Ok(parsed);
            }
        }

        Err(anyhow!(
            "Failed to retrieve search results from DuckDuckGo for query '{query}'"
        ))
    }
}

fn is_google_bot_block(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("unusual traffic from your computer network")
        || lower.contains("/sorry/index")
        || lower.contains("recaptcha")
        || (lower.contains("httpservice/retry/enablejs") && !lower.contains("<h3"))
}

/// Parses Google SERP HTML (supports both modern JS/desktop DOM and basic `gbv=1` HTML layouts).
pub fn parse_google_serp_html(html: &str, max_results: usize) -> Vec<SearchResultItem> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();

    // Strategy 1: Standard desktop & mobile Google result blocks (`div.tF2Cxc`, `div.g`, `div.ezO2md`, `div.MjjYud`)
    let block_sel = Selector::parse("div.tF2Cxc, div.g, div.ezO2md, div.MjjYud").unwrap();
    let h3_sel = Selector::parse("h3").unwrap();
    let a_sel = Selector::parse("a[href]").unwrap();
    let snippet_sel =
        Selector::parse("div.VwiC3b, span.aCOpRe, div.IsZvec, div.s3v9rd, div.BNeawe.s3v9rd")
            .unwrap();

    for block in document.select(&block_sel) {
        if results.len() >= max_results {
            return results;
        }

        let Some(link_el) = block.select(&a_sel).find(|a| {
            a.value()
                .attr("href")
                .and_then(unwrap_google_href)
                .is_some()
        }) else {
            continue;
        };

        let Some(raw_href) = link_el.value().attr("href") else {
            continue;
        };
        let Some(clean_url) = unwrap_google_href(raw_href) else {
            continue;
        };

        if seen_urls.contains(&clean_url) {
            continue;
        }

        let title = block
            .select(&h3_sel)
            .next()
            .map(|h| h.text().collect::<Vec<_>>().join(" "))
            .or_else(|| {
                let t = link_el.text().collect::<Vec<_>>().join(" ");
                if t.trim().is_empty() { None } else { Some(t) }
            })
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        if title.is_empty() {
            continue;
        }

        let snippet = block
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        seen_urls.insert(clean_url.clone());
        results.push(SearchResultItem {
            title,
            url: clean_url,
            snippet,
        });
    }

    // Strategy 2: Fallback scan for any `<a>` wrapping an `<h3>` (or `a[href^="/url?q="]`)
    if results.is_empty() {
        for a_el in document.select(&a_sel) {
            if results.len() >= max_results {
                break;
            }
            let Some(raw_href) = a_el.value().attr("href") else {
                continue;
            };
            let Some(clean_url) = unwrap_google_href(raw_href) else {
                continue;
            };
            if seen_urls.contains(&clean_url) {
                continue;
            }

            let title = a_el
                .select(&h3_sel)
                .next()
                .map(|h| h.text().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| a_el.text().collect::<Vec<_>>().join(" "))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");

            if title.len() < 3 {
                continue;
            }

            seen_urls.insert(clean_url.clone());
            results.push(SearchResultItem {
                title,
                url: clean_url,
                snippet: String::new(),
            });
        }
    }

    results
}

fn unwrap_google_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("javascript:") {
        return None;
    }

    let candidate = if let Some(rest) = trimmed.strip_prefix("/url?") {
        let dummy = format!("https://www.google.com/url?{rest}");
        let parsed = Url::parse(&dummy).ok()?;
        parsed
            .query_pairs()
            .find(|(k, _)| k == "q" || k == "url")
            .map(|(_, v)| v.into_owned())?
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        return None;
    };

    let parsed = Url::parse(&candidate).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();

    // Filter out internal Google navigation/support/account links
    if host.ends_with("google.com")
        || host.ends_with("googleusercontent.com")
        || host.ends_with("gstatic.com")
    {
        return None;
    }

    Some(resolve_and_clean_url(&candidate, None))
}

/// Parses DuckDuckGo HTML (`html.duckduckgo.com/html/`) and Lite (`lite.duckduckgo.com/lite/`) SERPs.
pub fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<SearchResultItem> {
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    let mut seen_urls = HashSet::new();

    // 1. Standard DuckDuckGo HTML layout (`.result`, `.web-result`)
    let result_sel = Selector::parse(".result, .web-result").unwrap();
    let title_sel = Selector::parse("a.result__a").unwrap();
    let snippet_sel = Selector::parse(".result__snippet").unwrap();

    for block in document.select(&result_sel) {
        if results.len() >= max_results {
            return results;
        }

        // Skip sponsored ads (`.result--ad`)
        if let Some(classes) = block.value().attr("class")
            && classes.contains("result--ad")
        {
            continue;
        }

        let Some(title_el) = block.select(&title_sel).next() else {
            continue;
        };
        let Some(raw_href) = title_el.value().attr("href") else {
            continue;
        };
        let Some(clean_url) = unwrap_ddg_href(raw_href) else {
            continue;
        };

        if seen_urls.contains(&clean_url) {
            continue;
        }

        let title = title_el
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        if title.is_empty() {
            continue;
        }

        let snippet = block
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        seen_urls.insert(clean_url.clone());
        results.push(SearchResultItem {
            title,
            url: clean_url,
            snippet,
        });
    }

    // 2. DuckDuckGo Lite layout (`a.result-link`, `td.result-snippet`)
    if results.is_empty() {
        let lite_link_sel = Selector::parse("a.result-link").unwrap();
        for link_el in document.select(&lite_link_sel) {
            if results.len() >= max_results {
                break;
            }
            let Some(raw_href) = link_el.value().attr("href") else {
                continue;
            };
            let Some(clean_url) = unwrap_ddg_href(raw_href) else {
                continue;
            };
            if seen_urls.contains(&clean_url) {
                continue;
            }
            let title = link_el
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if title.is_empty() {
                continue;
            }
            seen_urls.insert(clean_url.clone());
            results.push(SearchResultItem {
                title,
                url: clean_url,
                snippet: String::new(),
            });
        }
    }

    results
}

fn unwrap_ddg_href(href: &str) -> Option<String> {
    let trimmed = href.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("javascript:") {
        return None;
    }

    let normalized = if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if trimmed.starts_with('/') {
        format!("https://duckduckgo.com{trimmed}")
    } else {
        trimmed.to_string()
    };

    let parsed = Url::parse(&normalized).ok()?;
    let target = if let Some((_, uddg)) = parsed.query_pairs().find(|(k, _)| k == "uddg") {
        uddg.into_owned()
    } else {
        normalized
    };

    let target_parsed = Url::parse(&target).ok()?;
    let host = target_parsed.host_str()?.to_ascii_lowercase();
    if host.contains("duckduckgo.com") {
        return None;
    }

    Some(resolve_and_clean_url(&target, None))
}

/// Format a `SearchResponse` into compact, LLM-friendly Markdown.
pub fn format_search_response_markdown(resp: &SearchResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Web Search Results for \"{}\"\n", resp.query));
    out.push_str(&format!(
        "- **Search Engine**: {}\n- **Results Count**: {}\n\n",
        resp.provider_used,
        resp.results.len()
    ));

    if resp.results.is_empty() {
        out.push_str("*No organic results found.*\n");
        return out;
    }

    for (idx, item) in resp.results.iter().enumerate() {
        out.push_str(&format!(
            "### {}. [{}]({})\n",
            idx + 1,
            item.title,
            item.url
        ));
        out.push_str(&format!("- **URL**: `{}`\n", item.url));
        if !item.snippet.is_empty() {
            out.push_str(&format!("- **Snippet**: {}\n", item.snippet));
        }
        out.push('\n');
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_circuit_breaker_opens_and_resets() {
        let fetcher = WebFetcher::new(None, None).unwrap();
        let searcher = WebSearcher::new(fetcher);
        assert!(searcher.google_cooldown_remaining().is_none());

        searcher.trip_google_breaker();
        let remaining = searcher.google_cooldown_remaining().unwrap();
        assert!(remaining > Duration::from_secs(1));

        searcher.reset_google_breaker();
        assert!(searcher.google_cooldown_remaining().is_none());
    }

    #[test]
    fn parses_google_serp_and_unwraps_redirects() {
        let google_html = r#"
            <html>
            <body>
                <div class="g">
                    <div class="tF2Cxc">
                        <a href="/url?q=https://modelcontextprotocol.io/introduction%3Futm_source%3Dgoogle&amp;sa=U&amp;ved=2ahUKEwj">
                            <h3>Introduction - Model Context Protocol</h3>
                        </a>
                        <div class="VwiC3b">The Model Context Protocol (MCP) is an open protocol that enables seamless integration between LLM applications and external data sources.</div>
                    </div>
                </div>
                <div class="g">
                    <a href="https://www.rust-lang.org/learn">
                        <h3>Learn Rust - Rust Programming Language</h3>
                    </a>
                    <div class="VwiC3b">Get started with Rust. Affectionately nicknamed "the book," The Rust Programming Language will give you an overview of the language.</div>
                </div>
            </body>
            </html>
        "#;

        let results = parse_google_serp_html(google_html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Introduction - Model Context Protocol");
        assert_eq!(
            results[0].url,
            "https://modelcontextprotocol.io/introduction"
        );
        assert!(results[0].snippet.contains("open protocol"));
        assert_eq!(results[1].url, "https://www.rust-lang.org/learn");
    }

    #[test]
    fn parses_duckduckgo_html_and_unwraps_uddg_redirects() {
        let ddg_html = r#"
            <html>
            <body>
                <div class="result results_links results_links_deep web-result">
                    <h2 class="result__title">
                        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio%2Flatest%2Ftokio%2F&amp;rut=1234">
                            tokio - Rust
                        </a>
                    </h2>
                    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio%2Flatest%2Ftokio%2F">
                        A runtime for writing reliable asynchronous applications with Rust.
                    </a>
                </div>
            </body>
            </html>
        "#;

        let results = parse_duckduckgo_html(ddg_html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "tokio - Rust");
        assert_eq!(results[0].url, "https://docs.rs/tokio/latest/tokio/");
        assert!(results[0].snippet.contains("asynchronous applications"));
    }
}
