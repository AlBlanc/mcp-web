pub mod browser;
pub mod cdp;
pub mod http;
pub mod markdown;
pub mod mcp;
pub mod search;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::browser::{FetchOptions, RenderMode, WebFetcher};
use crate::http::HttpServer;
use crate::markdown::{MarkdownOptions, html_to_markdown};
use crate::mcp::McpServer;
use crate::search::{SearchEngine, SearchOptions, WebSearcher, format_search_response_markdown};

#[derive(Debug, Parser)]
#[command(
    name = "mcp-web",
    version,
    about = "MCP server providing low-token web page to Markdown conversion (SPA-ready) and web search (Google -> DuckDuckGo fallback)"
)]
struct Cli {
    /// Optional path to a Chrome/Chromium executable for SPA rendering.
    #[arg(long, env = "CHROME_PATH", global = true)]
    chrome_path: Option<PathBuf>,

    /// Custom User-Agent header string.
    #[arg(long, env = "MCP_WEB_USER_AGENT", global = true)]
    user_agent: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the Model Context Protocol (MCP) server (stdio by default, or HTTP/SSE when `--http` is specified).
    Serve {
        /// Optional HTTP bind address (e.g. `0.0.0.0:8000` or `127.0.0.1:8080`) to run as a multi-client HTTP/SSE server.
        #[arg(long, env = "MCP_WEB_HTTP_ADDR")]
        http: Option<SocketAddr>,
    },

    /// Run as a dedicated multi-client HTTP/SSE server.
    Http {
        /// Socket address to bind to (e.g. `0.0.0.0:8000` or `127.0.0.1:8080`).
        #[arg(short, long, env = "MCP_WEB_HTTP_ADDR", default_value = "0.0.0.0:8000")]
        bind: SocketAddr,
    },

    /// Execute a web search from the CLI (tries Google first, falls back to DuckDuckGo) and print Markdown.
    Search {
        /// Search query string.
        query: String,

        /// Maximum number of results (1-25).
        #[arg(short = 'n', long, default_value_t = 10)]
        max_results: usize,

        /// Search engine: auto (Google -> DuckDuckGo), google, or duckduckgo.
        #[arg(short, long, default_value = "auto")]
        engine: String,
    },

    /// Fetch the raw HTML content of a web page (including dynamic SPAs rendered via headless browser).
    Fetch {
        /// Target URL to fetch.
        url: String,

        /// Render mode: auto, browser (Headless Chrome for SPAs), or http.
        #[arg(short, long, default_value = "auto")]
        mode: String,

        /// Virtual time budget in milliseconds for SPA JavaScript execution in browser mode.
        #[arg(short, long, default_value_t = 3500)]
        wait_ms: u64,

        /// Maximum characters in the output (0 = unlimited).
        #[arg(long, default_value_t = 100000)]
        max_chars: usize,
    },

    /// Fetch a web page (rendering SPAs as needed) and convert it into token-optimized Markdown.
    Summarize {
        /// Target URL to fetch and summarize.
        url: String,

        /// Render mode: auto, browser (Headless Chrome for SPAs), or http.
        #[arg(short, long, default_value = "auto")]
        mode: String,

        /// Virtual time budget in milliseconds for SPA JavaScript execution in browser mode.
        #[arg(short, long, default_value_t = 3500)]
        wait_ms: u64,

        /// Maximum characters in the output Markdown (0 = unlimited).
        #[arg(long, default_value_t = 40000)]
        max_chars: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // IMPORTANT: Always write logs to `stderr` so `stdout` remains strictly reserved for MCP JSON-RPC frames.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let fetcher = WebFetcher::new(cli.chrome_path, cli.user_agent)?;

    match cli.command.unwrap_or(Commands::Serve { http: None }) {
        Commands::Serve { http: Some(addr) } | Commands::Http { bind: addr } => {
            fetcher.warm_up_browser();
            let server = Arc::new(McpServer::new(fetcher));
            let http_server = HttpServer::new(server, addr);
            http_server.run().await?;
        }
        Commands::Serve { http: None } => {
            let server = McpServer::new(fetcher);
            server.run_stdio().await?;
        }
        Commands::Search {
            query,
            max_results,
            engine,
        } => {
            let searcher = WebSearcher::new(fetcher);
            let outcome = searcher
                .search(&SearchOptions {
                    query,
                    max_results,
                    engine: SearchEngine::from_str_opt(Some(&engine)),
                    language: "en".to_string(),
                })
                .await?;
            println!(
                "{}\n\n*{}*",
                format_search_response_markdown(&outcome.response),
                outcome.timing_label()
            );
        }
        Commands::Fetch {
            url,
            mode,
            wait_ms,
            max_chars,
        } => {
            let fetched = fetcher
                .fetch(
                    &url,
                    &FetchOptions {
                        render_mode: RenderMode::from_str_opt(Some(&mode)),
                        wait_ms,
                        timeout_ms: 25_000,
                    },
                )
                .await?;
            let raw_chars = fetched.html.chars().count();
            let body = if max_chars > 0 && raw_chars > max_chars {
                let slice: String = fetched.html.chars().take(max_chars).collect();
                format!("{slice}\n\n<!-- [Truncated at {max_chars} characters] -->")
            } else {
                fetched.html
            };
            println!(
                "> **URL**: {}\n> **Rendered with Browser**: {} | **Raw Size**: {} KB | **Fetch**: {} ms\n\n{}",
                fetched.final_url,
                fetched.rendered_with_browser,
                body.len() / 1024,
                fetched.elapsed_ms,
                body
            );
        }
        Commands::Summarize {
            url,
            mode,
            wait_ms,
            max_chars,
        } => {
            let fetched = fetcher
                .fetch(
                    &url,
                    &FetchOptions {
                        render_mode: RenderMode::from_str_opt(Some(&mode)),
                        wait_ms,
                        timeout_ms: 25_000,
                    },
                )
                .await?;
            let doc = html_to_markdown(
                &fetched.html,
                &fetched.final_url,
                &MarkdownOptions {
                    max_chars,
                    ..Default::default()
                },
            );
            println!(
                "> **URL**: {}\n> **Rendered with Browser**: {} | **Tokens**: ~{} ({}% saved) | **Fetch**: {} ms\n\n{}",
                doc.url,
                fetched.rendered_with_browser,
                doc.estimated_tokens,
                doc.token_reduction_percent,
                fetched.elapsed_ms,
                doc.markdown
            );
        }
    }

    Ok(())
}
