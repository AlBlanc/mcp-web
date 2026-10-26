# mcp-web

[![CI](https://github.com/AlBlanc/mcp-web/actions/workflows/ci.yml/badge.svg)](https://github.com/AlBlanc/mcp-web/actions/workflows/ci.yml)
[![Release](https://github.com/AlBlanc/mcp-web/actions/workflows/release.yml/badge.svg)](https://github.com/AlBlanc/mcp-web/releases)

A fast, single-binary [Model Context Protocol](https://modelcontextprotocol.io) (MCP) server that gives your LLM access to the internet **while minimizing token consumption**.

Web pages are converted to clean Markdown (no scripts, styles, navigation, cookie banners or tracking parameters), and Single Page Applications (React, Vue, Next.js, Angular, ...) are rendered with a headless browser before conversion.

## Features

- **`web_search`**: searches **Google first**, and automatically falls back to **DuckDuckGo** when Google is blocked, rate-limited or returns nothing.
- **`fetch_page`**: fetches a URL and returns the **raw HTML** as rendered after executing JavaScript (with SPA auto-detection and headless Chrome rendering).
- **`summarize_page`**: fetches a URL (executing JS for SPAs as needed) and converts it to **token-optimized Markdown** with boilerplate removal, link cleanup, and token savings reporting.
- **`fetch_batch`**: fetches up to 5 URLs concurrently.
- **SPA support**: a fast HTTP fetch is tried first; if the page looks like an unhydrated SPA shell (empty `#root`/`#app`/`#__next`, `<noscript>` warnings, JS challenges, ...), it is re-rendered with headless Chrome/Chromium.
- **Token reduction**: main-content extraction, boilerplate removal, relative links resolved to absolute URLs, tracking params (`utm_*`, `fbclid`, `gclid`, ...) stripped, images off by default, and a configurable character cap.
- **Speed**: in-memory caching with request coalescing (identical concurrent calls share one fetch), a Google circuit breaker (after a block, searches go straight to DuckDuckGo instead of waiting on a failing engine), fail-fast timeouts, and per-call timing reported in every response.
- **Zero runtime dependencies**: one native binary. Only SPA rendering needs a Chrome/Chromium/Brave/Edge install, which is auto-detected.

## Installation

### Pre-built binaries

Download the archive for your platform from the [Releases page](../../releases):

| OS | Architecture | Archive |
| :-- | :-- | :-- |
| Linux | x86_64 | `mcp-web-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | aarch64 | `mcp-web-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Apple Silicon | `mcp-web-aarch64-apple-darwin.tar.gz` |
| Windows | x86_64 | `mcp-web-x86_64-pc-windows-msvc.zip` |
| Windows | ARM64 | `mcp-web-aarch64-pc-windows-msvc.zip` |

Each archive has a matching `.sha256` checksum file.

On macOS, a downloaded binary may be quarantined by Gatekeeper:

```bash
xattr -d com.apple.quarantine ./mcp-web
```

### Build from source

Requires a recent stable [Rust toolchain](https://rustup.rs) (edition 2024).

```bash
git clone https://github.com/AlBlanc/mcp-web.git
cd mcp-web
cargo build --release
# binary: target/release/mcp-web
```

## Configure your MCP client

`mcp-web` supports two modes: **Local stdio** (run as a subprocess on the same machine) and **Multi-Client HTTP/SSE** (run as a network service so multiple clients can connect concurrently from any host).

### Mode 1: Local stdio (default)

For local MCP clients (Claude Desktop, Cursor, OpenCode, Antigravity running on the same machine):

```json
{
  "mcpServers": {
    "mcp-web": {
      "command": "/absolute/path/to/mcp-web",
      "args": ["serve"],
      "env": {
        "RUST_LOG": "warn"
      }
    }
  }
}
```

On Windows, use the full path to `mcp-web.exe` (with escaped backslashes in JSON).

---

### Mode 2: Multi-Client Remote Server (HTTP / SSE)

You can run `mcp-web` as a shared network server (on a VPS, a local server, a Docker container, or your main workstation):

```bash
# Start the HTTP/SSE server listening on port 8000
mcp-web http --bind 0.0.0.0:8000

# Or using the serve command:
mcp-web serve --http 0.0.0.0:8000
```

Now, any client (multiple OpenCode instances, remote developer machines, background agents) can connect simultaneously by pointing to the `/sse` URL:

```json
{
  "mcpServers": {
    "web": {
      "url": "http://<SERVER-IP-OR-HOSTNAME>:8000/sse"
    }
  }
}
```

#### Advantages of the Shared Server Mode:
- **Shared Cache**: If Client A searches a query or reads a documentation page, Client B receives the exact same content in **0 ms** directly from the in-memory cache.
- **Shared Browser Pool**: Only one persistent Chrome instance runs on the server, avoiding heavy browser startup on client laptops.
- **Direct HTTP endpoint (`POST /mcp`)**: For curl, scripts, or lightweight web apps that want direct request-response JSON-RPC without maintaining an SSE stream.
- **Health check (`GET /health`)**: Returns status, version, and the current number of active client sessions.

#### Docker deployment

You can run `mcp-web` in Docker with bundled Chromium for headless SPA rendering:

```bash
# Build the Docker image
docker build -t mcp-web .

# Run the container
docker run -d --name mcp-web -p 8000:8000 --init mcp-web
```

Or using `docker compose`:

```yaml
services:
  mcp-web:
    build: .
    ports:
      - "8000:8000"
    restart: unless-stopped
    init: true
```

---

### Environment variables

| Variable | Description |
| :-- | :-- |
| `CHROME_PATH` | Path to a Chrome/Chromium binary (auto-detected by default). |
| `GOOGLE_API_KEY` / `GOOGLE_CX` | Optional. Use the official Google Custom Search JSON API before scraping Google. |
| `MCP_WEB_USER_AGENT` | Override the default User-Agent. |
| `MCP_WEB_HTTP_ADDR` | Default bind address for HTTP/SSE server mode (e.g. `0.0.0.0:8000`). |
| `MCP_WEB_CACHE_TTL_SECS` | In-memory cache TTL in seconds for pages and searches (defaults: 300 for pages, 600 for searches). `0` effectively disables caching. |
| `MCP_WEB_GOOGLE_COOLDOWN_SECS` | How long Google is skipped after it blocks us (default `300`). `0` disables the circuit breaker. |
| `RUST_LOG` | Log level written to `stderr` (`error`, `warn`, `info`, `debug`). |

Logs always go to `stderr`; `stdout` is reserved for MCP JSON-RPC messages.

## Tools

### `web_search`

| Parameter | Type | Default | Description |
| :-- | :-- | :-- | :-- |
| `query` | string | required | Search query. |
| `max_results` | integer | `10` | Number of results (1-25). |
| `engine` | `auto` \| `google` \| `duckduckgo` | `auto` | `auto` tries Google, then DuckDuckGo. |
| `language` | string | `en` | Result language, e.g. `en`, `fr`. |
| `fetch_top_n` | integer | `0` | Also fetch and convert the top N results (max 5) in the same call. |

### `fetch_page`

Fetch the raw HTML content of a web page after executing JavaScript / hydrating SPAs.

| Parameter | Type | Default | Description |
| :-- | :-- | :-- | :-- |
| `url` | string | required | HTTP/HTTPS URL to read. |
| `render_mode` | `auto` \| `browser` \| `http` | `auto` | `auto` upgrades to headless Chrome for SPAs; `browser` always uses it; `http` never does. |
| `wait_ms` | integer | `3500` | JS execution time budget in browser mode (500-20000). |
| `max_chars` | integer | `100000` | Output cap in characters (`0` = unlimited). |

### `summarize_page`

Fetch a web page (executing JS for SPAs as needed) and convert its primary content into token-optimized Markdown to minimize LLM token consumption.

| Parameter | Type | Default | Description |
| :-- | :-- | :-- | :-- |
| `url` | string | required | HTTP/HTTPS URL to read. |
| `render_mode` | `auto` \| `browser` \| `http` | `auto` | `auto` upgrades to headless Chrome for SPAs; `browser` always uses it; `http` never does. |
| `wait_ms` | integer | `3500` | JS execution time budget in browser mode (500-20000). |
| `extract_main_content` | boolean | `true` | Keep only the main content, drop nav/footer/sidebars/banners. |
| `selector` | string | none | CSS selector to extract a specific element. |
| `max_chars` | integer | `40000` | Output cap (`0` = unlimited). |
| `include_links` | boolean | `true` | Keep hyperlinks. |
| `include_images` | boolean | `false` | Keep image references. |

### `fetch_batch`

| Parameter | Type | Default | Description |
| :-- | :-- | :-- | :-- |
| `urls` | string[] | required | Up to 5 URLs fetched in parallel. |
| `render_mode` | `auto` \| `browser` \| `http` | `auto` | Same as `fetch_page`. |
| `max_chars_per_page` | integer | `20000` | Output cap per page. |

## CLI usage

The binary can also be used directly, which is handy for testing:

```bash
# Search (Google first, DuckDuckGo fallback)
mcp-web search "Model Context Protocol specification" -n 5

# Fetch raw HTML (SPA auto-detection & JS execution)
mcp-web fetch "https://react.dev/learn"

# Fetch and summarize into token-optimized Markdown
mcp-web summarize "https://modelcontextprotocol.io/introduction"

# Force headless Chrome rendering with a 4s JS budget
mcp-web summarize "https://react.dev/learn" --mode browser --wait-ms 4000
```

## Project layout

| File | Role |
| :-- | :-- |
| `src/main.rs` | CLI entrypoint (`serve`, `search`, `fetch`). |
| `src/mcp.rs` | MCP JSON-RPC server over stdio and tool definitions. |
| `src/browser.rs` | HTTP/headless-Chrome fetching and SPA detection. |
| `src/markdown.rs` | HTML cleaning and Markdown conversion. |
| `src/search.rs` | Google and DuckDuckGo search with fallback. |

## Releasing

CI runs format, Clippy and tests on every push and pull request ([ci.yml](.github/workflows/ci.yml)).

To publish a release, bump `version` in `Cargo.toml`, then push a matching tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The [release workflow](.github/workflows/release.yml) builds all five targets natively on GitHub-hosted runners, packages them with SHA-256 checksums, and creates a GitHub Release with every archive attached.

## Notes

Scraping search engines may violate their terms of service and can be rate-limited or blocked. For reliable Google results, set `GOOGLE_API_KEY` and `GOOGLE_CX`. Use responsibly.

## License

MIT
