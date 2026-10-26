use std::sync::Arc;

use anyhow::Result;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::browser::{FetchOptions, RenderMode, WebFetcher};
use crate::markdown::{MarkdownOptions, html_to_markdown};
use crate::search::{SearchEngine, SearchOptions, WebSearcher, format_search_response_markdown};

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "mcp-web";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone)]
pub struct McpServer {
    fetcher: Arc<WebFetcher>,
    searcher: Arc<WebSearcher>,
}

impl McpServer {
    pub fn new(fetcher: WebFetcher) -> Self {
        let searcher = WebSearcher::new(fetcher.clone());
        Self {
            fetcher: Arc::new(fetcher),
            searcher: Arc::new(searcher),
        }
    }

    /// Run the MCP server loop over standard input / standard output.
    pub async fn run_stdio(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        info!(
            "Starting {} v{} MCP stdio server",
            SERVER_NAME, SERVER_VERSION
        );

        // Launch the headless browser in the background so the first SPA request is fast.
        self.fetcher.warm_up_browser();

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                debug!("EOF reached on stdin; shutting down MCP server");
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Support optional LSP-style `Content-Length: <N>` framing as well as standard newline-delimited JSON
            if let Some(len_str) = trimmed.strip_prefix("Content-Length:")
                && let Ok(content_len) = len_str.trim().parse::<usize>()
            {
                // Consume blank header separator line(s)
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let mut body_buf = vec![0u8; content_len];
                tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body_buf).await?;
                let body_str = String::from_utf8_lossy(&body_buf);
                if let Some(resp) = self.handle_raw_message(&body_str).await {
                    let serialized = serde_json::to_string(&resp)?;
                    stdout.write_all(serialized.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
                continue;
            }

            if let Some(resp) = self.handle_raw_message(trimmed).await {
                let serialized = serde_json::to_string(&resp)?;
                stdout.write_all(serialized.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
            }
        }

        self.fetcher.shutdown_browser().await;
        Ok(())
    }

    pub async fn handle_raw_message(&self, raw: &str) -> Option<JsonRpcResponse> {
        let req: JsonRpcRequest = match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(err) => {
                error!(error = %err, "Failed to parse incoming JSON-RPC message");
                return Some(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {err}"),
                        data: None,
                    }),
                });
            }
        };

        // Notifications do not have an `id` and must not receive a response
        let id = req.id.clone()?;
        let _ = &req.jsonrpc;

        match self.dispatch(&req.method, req.params).await {
            Ok(result) => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            }),
            Err((code, message)) => Some(JsonRpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(JsonRpcError {
                    code,
                    message,
                    data: None,
                }),
            }),
        }
    }

    async fn dispatch(&self, method: &str, params: Option<Value>) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let client_version = params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);

                let negotiated_version = match client_version {
                    "2025-06-18" | "2025-03-26" | "2024-11-05" => client_version,
                    _ => DEFAULT_PROTOCOL_VERSION,
                };

                Ok(json!({
                    "protocolVersion": negotiated_version,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION
                    },
                    "instructions": "Provides internet access to LLMs. Use `web_search` to search the web (Google first, automatic fallback to DuckDuckGo), `fetch_page` to fetch raw HTML (with full JavaScript SPA execution), `summarize_page` to convert a web page into token-optimized Markdown to minimize token usage, and `fetch_batch` to fetch multiple pages concurrently."
                }))
            }
            "ping" => Ok(json!({})),
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            "tools/list" => Ok(Self::list_tools_payload()),
            "tools/call" => {
                let p =
                    params.ok_or_else(|| (-32602, "Missing params for tools/call".to_string()))?;
                let tool_name = p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| (-32602, "Missing tool `name`".to_string()))?;
                let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(self.call_tool(tool_name, args).await)
            }
            other if other.starts_with("notifications/") => Ok(json!({})),
            other => {
                warn!(method = %other, "Unknown JSON-RPC method called");
                Err((-32601, format!("Method not found: {other}")))
            }
        }
    }

    pub fn list_tools_payload() -> Value {
        json!({
            "tools": [
                {
                    "name": "web_search",
                    "description": "Search the internet for up-to-date information. Queries Google first and automatically falls back to DuckDuckGo if Google is blocked or unavailable. Can optionally fetch and convert the top N result pages into compact Markdown in a single call.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query keywords or question."
                            },
                            "max_results": {
                                "type": "integer",
                                "description": "Maximum number of search results to return (1-25, default: 10).",
                                "default": 10
                            },
                            "engine": {
                                "type": "string",
                                "enum": ["auto", "google", "duckduckgo"],
                                "description": "'auto' (default) tries Google first and falls back to DuckDuckGo. Set 'google' or 'duckduckgo' to force a specific engine.",
                                "default": "auto"
                            },
                            "language": {
                                "type": "string",
                                "description": "Preferred language code for search results (e.g. 'en', 'fr', 'de').",
                                "default": "en"
                            },
                            "fetch_top_n": {
                                "type": "integer",
                                "description": "If > 0 (max 5), automatically fetches the top N result URLs and appends their Markdown content to save an extra round-trip.",
                                "default": 0
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "fetch_page",
                    "description": "Fetch the raw HTML content of a web page (supports dynamic JavaScript SPAs via headless browser rendering). Executes JavaScript and preserves the full rendered DOM.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "Full HTTP/HTTPS URL of the web page to read."
                            },
                            "render_mode": {
                                "type": "string",
                                "enum": ["auto", "browser", "http"],
                                "description": "'auto' (default) fetches via fast HTTP and automatically upgrades to Headless Chrome if an SPA shell (React/Vue/Next/Angular) or JS challenge is detected. 'browser' forces Headless Chrome rendering. 'http' uses static HTTP only.",
                                "default": "auto"
                            },
                            "wait_ms": {
                                "type": "integer",
                                "description": "Virtual time budget in milliseconds for SPA JavaScript execution when using Headless Chrome (default: 3500, range: 500-20000).",
                                "default": 3500
                            },
                            "max_chars": {
                                "type": "integer",
                                "description": "Maximum character count of returned raw HTML to prevent context overflow (default: 100000, 0 for unlimited).",
                                "default": 100000
                            }
                        },
                        "required": ["url"]
                    }
                },
                {
                    "name": "summarize_page",
                    "description": "Fetch a web page (rendering JavaScript SPAs as needed) and convert its primary content into clean, token-optimized Markdown to minimize LLM token consumption. Strips scripts, styles, navigation bars, cookie banners, ads, and tracking parameters.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "Full HTTP/HTTPS URL of the web page to read."
                            },
                            "render_mode": {
                                "type": "string",
                                "enum": ["auto", "browser", "http"],
                                "description": "'auto' (default) fetches via fast HTTP and automatically upgrades to Headless Chrome if an SPA shell is detected. 'browser' forces Headless Chrome rendering. 'http' uses static HTTP only.",
                                "default": "auto"
                            },
                            "wait_ms": {
                                "type": "integer",
                                "description": "Virtual time budget in milliseconds for SPA JavaScript execution when using Headless Chrome (default: 3500, range: 500-20000).",
                                "default": 3500
                            },
                            "extract_main_content": {
                                "type": "boolean",
                                "description": "When true (default), extracts the main article/content region and removes navbars, sidebars, footers, and cookie banners to minimize token consumption.",
                                "default": true
                            },
                            "selector": {
                                "type": "string",
                                "description": "Optional CSS selector (e.g. 'article', '#main', '.docs-content') to extract a specific element."
                            },
                            "max_chars": {
                                "type": "integer",
                                "description": "Maximum character count of returned Markdown to prevent context bloat (default: 40000, 0 for unlimited).",
                                "default": 40000
                            },
                            "include_links": {
                                "type": "boolean",
                                "description": "Keep hyperlinks in the Markdown output (default: true).",
                                "default": true
                            },
                            "include_images": {
                                "type": "boolean",
                                "description": "Keep image tags in the Markdown output (default: false to save tokens).",
                                "default": false
                            }
                        },
                        "required": ["url"]
                    }
                },
                {
                    "name": "fetch_batch",
                    "description": "Fetch multiple web page URLs concurrently (up to 5 at once), rendering SPAs as needed and converting each page into token-optimized Markdown.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "urls": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "List of HTTP/HTTPS URLs to fetch in parallel (max 5)."
                            },
                            "render_mode": {
                                "type": "string",
                                "enum": ["auto", "browser", "http"],
                                "default": "auto"
                            },
                            "max_chars_per_page": {
                                "type": "integer",
                                "description": "Maximum Markdown characters per page (default: 20000).",
                                "default": 20000
                            }
                        },
                        "required": ["urls"]
                    }
                }
            ]
        })
    }

    async fn call_tool(&self, tool_name: &str, args: Value) -> Value {
        match tool_name {
            "web_search" => self.handle_web_search(args).await,
            "fetch_page" | "fetch" | "web_fetch" => self.handle_fetch_raw_page(args).await,
            "summarize" | "summarize_page" => self.handle_summarize_page(args).await,
            "fetch_batch" => self.handle_fetch_batch(args).await,
            other => Self::tool_error(format!("Unknown tool '{other}'")),
        }
    }

    async fn handle_web_search(&self, args: Value) -> Value {
        let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
            return Self::tool_error("Missing required string parameter `query`");
        };

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as usize;
        let engine = SearchEngine::from_str_opt(args.get("engine").and_then(|v| v.as_str()));
        let language = args
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("en")
            .to_string();
        let fetch_top_n = args
            .get("fetch_top_n")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(5) as usize;

        let search_opts = SearchOptions {
            query: query.to_string(),
            max_results,
            engine,
            language,
        };

        match self.searcher.search(&search_opts).await {
            Ok(outcome) => {
                let resp = &outcome.response;
                let mut md = format_search_response_markdown(resp);
                md.push_str(&format!("\n\n*{}*", outcome.timing_label()));

                // Optional inline fetching of top N search results
                if fetch_top_n > 0 && !resp.results.is_empty() {
                    let top_urls: Vec<String> = resp
                        .results
                        .iter()
                        .take(fetch_top_n)
                        .map(|r| r.url.clone())
                        .collect();

                    md.push_str("\n\n---\n## Extracted Page Contents (Top Results)\n");

                    let futs = top_urls.iter().map(|u| {
                        let fetcher = self.fetcher.clone();
                        let url = u.clone();
                        async move {
                            let fetch_opts = FetchOptions::default();
                            let md_opts = MarkdownOptions {
                                max_chars: 12_000,
                                ..Default::default()
                            };
                            let doc = fetcher.fetch(&url, &fetch_opts).await?;
                            let converted = html_to_markdown(&doc.html, &doc.final_url, &md_opts);
                            Ok::<_, anyhow::Error>((url, converted))
                        }
                    });

                    for res in join_all(futs).await {
                        match res {
                            Ok((url, doc)) => {
                                md.push_str(&format!(
                                    "\n### Source: {}\n*Estimated tokens: ~{} (reduced by {}%)*\n\n{}\n",
                                    url, doc.estimated_tokens, doc.token_reduction_percent, doc.markdown
                                ));
                            }
                            Err(err) => {
                                md.push_str(&format!(
                                    "\n### Source Error\nFailed to fetch page: {err}\n"
                                ));
                            }
                        }
                    }
                }

                Self::tool_success(md)
            }
            Err(err) => Self::tool_error(format!("Web search failed: {err:#}")),
        }
    }

    async fn handle_fetch_raw_page(&self, args: Value) -> Value {
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return Self::tool_error("Missing required string parameter `url`");
        };

        let render_mode =
            RenderMode::from_str_opt(args.get("render_mode").and_then(|v| v.as_str()));
        let wait_ms = args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(3500);
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(100_000) as usize;

        let fetch_opts = FetchOptions {
            render_mode,
            wait_ms,
            timeout_ms: 25_000,
        };

        match self.fetcher.fetch(url, &fetch_opts).await {
            Ok(fetched) => {
                let mode_label = if fetched.rendered_with_browser {
                    "Headless Chrome (SPA/JS rendered)"
                } else {
                    "Static HTTP"
                };

                let raw_chars_count = fetched.html.chars().count();
                let (body, truncated) = if max_chars > 0 && raw_chars_count > max_chars {
                    let slice: String = fetched.html.chars().take(max_chars).collect();
                    (
                        format!("{slice}\n\n<!-- [Raw HTML truncated at {max_chars} characters] -->"),
                        true,
                    )
                } else {
                    (fetched.html.clone(), false)
                };

                let mut output = String::new();
                output.push_str(&format!(
                    "> **URL**: {}\n> **Render Mode**: {} | **Raw Size**: {} KB | **Fetch**: {} ms{}{}\n\n",
                    fetched.final_url,
                    mode_label,
                    fetched.html.len() / 1024,
                    fetched.elapsed_ms,
                    if fetched.cached { " (cached)" } else { "" },
                    if truncated { " | *(truncated)*" } else { "" }
                ));
                output.push_str(&body);

                Self::tool_success(output)
            }
            Err(err) => Self::tool_error(format!("Failed to fetch '{url}': {err:#}")),
        }
    }

    async fn handle_summarize_page(&self, args: Value) -> Value {
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return Self::tool_error("Missing required string parameter `url`");
        };

        let render_mode =
            RenderMode::from_str_opt(args.get("render_mode").and_then(|v| v.as_str()));
        let wait_ms = args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(3500);
        let extract_main_content = args
            .get("extract_main_content")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let selector = args
            .get("selector")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(40_000) as usize;
        let include_links = args
            .get("include_links")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let include_images = args
            .get("include_images")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let fetch_opts = FetchOptions {
            render_mode,
            wait_ms,
            timeout_ms: 25_000,
        };

        let md_opts = MarkdownOptions {
            extract_main_content,
            selector,
            include_links,
            include_images,
            max_chars,
        };

        match self.fetcher.fetch(url, &fetch_opts).await {
            Ok(fetched) => {
                let doc = html_to_markdown(&fetched.html, &fetched.final_url, &md_opts);
                let mode_label = if fetched.rendered_with_browser {
                    "Headless Chrome (SPA/JS rendered)"
                } else {
                    "Static HTTP"
                };

                let mut output = String::new();
                output.push_str(&format!(
                    "> **URL**: {}\n> **Render Mode**: {} | **Tokens**: ~{} ({}% reduction from {} KB raw HTML) | **Fetch**: {} ms{}\n",
                    doc.url,
                    mode_label,
                    doc.estimated_tokens,
                    doc.token_reduction_percent,
                    doc.raw_html_bytes / 1024,
                    fetched.elapsed_ms,
                    if fetched.cached { " (cached)" } else { "" }
                ));
                if let Some(ref desc) = doc.description {
                    output.push_str(&format!("> **Description**: {desc}\n"));
                }
                output.push('\n');
                output.push_str(&doc.markdown);

                Self::tool_success(output)
            }
            Err(err) => Self::tool_error(format!("Failed to summarize '{url}': {err:#}")),
        }
    }

    async fn handle_fetch_batch(&self, args: Value) -> Value {
        let Some(urls_val) = args.get("urls").and_then(|v| v.as_array()) else {
            return Self::tool_error("Missing required array parameter `urls`");
        };

        let urls: Vec<String> = urls_val
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .take(5)
            .collect();

        if urls.is_empty() {
            return Self::tool_error("`urls` array must contain at least one valid URL string");
        }

        let render_mode =
            RenderMode::from_str_opt(args.get("render_mode").and_then(|v| v.as_str()));
        let max_chars = args
            .get("max_chars_per_page")
            .and_then(|v| v.as_u64())
            .unwrap_or(20_000) as usize;

        let futs = urls.into_iter().map(|url| {
            let fetcher = self.fetcher.clone();
            async move {
                let fetch_opts = FetchOptions {
                    render_mode,
                    wait_ms: 3500,
                    timeout_ms: 25_000,
                };
                let md_opts = MarkdownOptions {
                    max_chars,
                    ..Default::default()
                };
                match fetcher.fetch(&url, &fetch_opts).await {
                    Ok(fetched) => {
                        let doc = html_to_markdown(&fetched.html, &fetched.final_url, &md_opts);
                        format!(
                            "## Page: {}\n*Tokens: ~{} ({}% saved)*\n\n{}\n",
                            doc.url,
                            doc.estimated_tokens,
                            doc.token_reduction_percent,
                            doc.markdown
                        )
                    }
                    Err(e) => format!("## Page: {url}\n*Error fetching page*: {e:#}\n"),
                }
            }
        });

        let sections = join_all(futs).await;
        Self::tool_success(sections.join("\n---\n\n"))
    }

    fn tool_success(text: impl Into<String>) -> Value {
        json!({
            "content": [
                {
                    "type": "text",
                    "text": text.into()
                }
            ],
            "isError": false
        })
    }

    fn tool_error(message: impl Into<String>) -> Value {
        json!({
            "content": [
                {
                    "type": "text",
                    "text": message.into()
                }
            ],
            "isError": true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handles_initialize_and_tools_list() {
        let fetcher = WebFetcher::new(None, None).unwrap();
        let server = McpServer::new(fetcher);

        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test-client","version":"1.0"}}}"#;
        let init_resp = server.handle_raw_message(init_req).await.unwrap();
        assert!(init_resp.error.is_none());
        let res = init_resp.result.unwrap();
        assert_eq!(res["serverInfo"]["name"], "mcp-web");

        let tools_req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let tools_resp = server.handle_raw_message(tools_req).await.unwrap();
        assert!(tools_resp.error.is_none());
        let tools = tools_resp.result.unwrap()["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[1]["name"], "fetch_page");
        assert_eq!(tools[2]["name"], "summarize_page");
        assert_eq!(tools[3]["name"], "fetch_batch");
    }
}
