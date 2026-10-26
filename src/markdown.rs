use once_cell::sync::Lazy;
use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

static MULTI_NEWLINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
static TRAILING_SPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)[ \t]+$").unwrap());
static EMPTY_LINK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\s*\]\([^)]*\)").unwrap());
static HTML_COMMENT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());

/// Configuration for HTML-to-Markdown extraction and token reduction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownOptions {
    /// Extract the main content / article region and strip navigation, footers, sidebars, and cookie banners.
    pub extract_main_content: bool,
    /// Optional CSS selector to scope extraction to a specific DOM subtree.
    pub selector: Option<String>,
    /// Keep hyperlinks (`[text](url)`) in the output Markdown.
    pub include_links: bool,
    /// Keep image references (`![alt](src)`) in the output Markdown (false by default to save tokens).
    pub include_images: bool,
    /// Maximum characters in the output Markdown (`0` = unlimited).
    pub max_chars: usize,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            extract_main_content: true,
            selector: None,
            include_links: true,
            include_images: false,
            max_chars: 40_000,
        }
    }
}

/// Result of converting an HTML document into token-optimized Markdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkdownDocument {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub markdown: String,
    pub raw_html_bytes: usize,
    pub markdown_bytes: usize,
    pub estimated_tokens: usize,
    pub token_reduction_percent: f64,
    pub truncated: bool,
}

/// Convert raw HTML from `page_url` into clean, token-efficient Markdown.
pub fn html_to_markdown(html: &str, page_url: &str, options: &MarkdownOptions) -> MarkdownDocument {
    let raw_html_bytes = html.len();
    let base_url = Url::parse(page_url).ok();

    let document = Html::parse_document(html);
    let title = extract_title(&document);
    let description = extract_meta_description(&document);

    // 1. Select target subtree (user selector -> main content candidate -> body -> document root)
    let selected_html = select_content_subtree(&document, options);

    // 2. Sanitize DOM and normalize URLs before Markdown conversion
    let cleaned_html = sanitize_and_normalize_html(&selected_html, base_url.as_ref(), options);

    // 3. Convert cleaned HTML into Markdown using `htmd`
    let mut skipped_tags: Vec<&str> = vec![
        "script", "style", "noscript", "svg", "iframe", "canvas", "template", "form", "button",
        "input", "select", "textarea",
    ];
    if !options.include_images {
        skipped_tags.push("img");
        skipped_tags.push("picture");
    }

    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(skipped_tags)
        .build();

    let raw_md = converter
        .convert(&cleaned_html)
        .unwrap_or_else(|_| strip_tags_fallback(&cleaned_html));

    // 4. Post-process Markdown to maximize signal-to-token ratio
    let mut markdown = post_process_markdown(&raw_md, options.include_links);

    // Prepend page title as H1 if not already present in the Markdown body
    if let Some(ref t) = title {
        let trimmed_title = t.trim();
        if !trimmed_title.is_empty() && !markdown.starts_with("# ") {
            markdown = format!("# {trimmed_title}\n\n{markdown}");
        }
    }

    // 5. Apply smart boundary truncation if `max_chars` is set
    let mut truncated = false;
    if options.max_chars > 0 && markdown.chars().count() > options.max_chars {
        markdown = truncate_markdown_cleanly(&markdown, options.max_chars);
        truncated = true;
    }

    let markdown_bytes = markdown.len();
    // Standard LLM approximation: ~4 bytes/chars per token
    let estimated_tokens = (markdown_bytes / 4).max(1);
    let raw_estimated_tokens = (raw_html_bytes / 4).max(1);
    let token_reduction_percent = if raw_estimated_tokens > estimated_tokens {
        ((raw_estimated_tokens - estimated_tokens) as f64 / raw_estimated_tokens as f64) * 100.0
    } else {
        0.0
    };

    MarkdownDocument {
        url: page_url.to_string(),
        title,
        description,
        markdown,
        raw_html_bytes,
        markdown_bytes,
        estimated_tokens,
        token_reduction_percent: (token_reduction_percent * 10.0).round() / 10.0,
        truncated,
    }
}

fn extract_title(document: &Html) -> Option<String> {
    if let Ok(sel) = Selector::parse("title")
        && let Some(el) = document.select(&sel).next()
    {
        let text = el.text().collect::<Vec<_>>().join(" ");
        let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !clean.is_empty() {
            return Some(clean);
        }
    }
    if let Ok(sel) = Selector::parse("meta[property='og:title'], meta[name='twitter:title']") {
        for el in document.select(&sel) {
            if let Some(content) = el.value().attr("content") {
                let clean = content.trim();
                if !clean.is_empty() {
                    return Some(clean.to_string());
                }
            }
        }
    }
    None
}

fn extract_meta_description(document: &Html) -> Option<String> {
    let selectors = [
        "meta[name='description']",
        "meta[property='og:description']",
        "meta[name='twitter:description']",
    ];
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in document.select(&sel) {
                if let Some(content) = el.value().attr("content") {
                    let clean = content.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !clean.is_empty() {
                        return Some(clean);
                    }
                }
            }
        }
    }
    None
}

fn select_content_subtree(document: &Html, options: &MarkdownOptions) -> String {
    // 1. Explicit CSS selector provided by the caller
    if let Some(ref custom_sel) = options.selector
        && let Ok(sel) = Selector::parse(custom_sel)
    {
        let matched: Vec<String> = document.select(&sel).map(|el| el.html()).collect();
        if !matched.is_empty() {
            return matched.join("\n\n");
        }
    }

    // 2. If `extract_main_content` is enabled, look for semantic main/article containers
    // with substantial text content (Readability-inspired candidate selection)
    if options.extract_main_content {
        let priority_selectors = [
            "main article",
            "article[role='main']",
            "main",
            "[role='main']",
            "article",
            "#main-content",
            "#content",
            ".markdown-body",
            ".article-body",
            ".post-content",
            ".entry-content",
            ".doc-content",
            ".documentation",
        ];

        let mut best_html: Option<String> = None;
        let mut best_score: usize = 0;

        for sel_str in priority_selectors {
            if let Ok(sel) = Selector::parse(sel_str) {
                for el in document.select(&sel) {
                    let text_len = visible_text_score(&el);
                    // Must have meaningful content (at least 180 chars) to be chosen as main container
                    if text_len >= 180 && text_len > best_score {
                        best_score = text_len;
                        best_html = Some(el.html());
                    }
                }
                if best_score >= 400 {
                    break;
                }
            }
        }

        if let Some(chosen) = best_html {
            return chosen;
        }
    }

    // 3. Fallback to <body> or full document
    if let Ok(body_sel) = Selector::parse("body")
        && let Some(body) = document.select(&body_sel).next()
    {
        return body.html();
    }

    document.html()
}

fn visible_text_score(el: &ElementRef<'_>) -> usize {
    let mut len = 0usize;
    for node in el.descendants() {
        if let Some(text) = node.value().as_text() {
            let mut in_noise = false;
            for anc in node.ancestors() {
                if let Some(anc_el) = anc.value().as_element()
                    && matches!(
                        anc_el.name(),
                        "script" | "style" | "nav" | "footer" | "aside" | "noscript" | "svg"
                    )
                {
                    in_noise = true;
                    break;
                }
            }
            if !in_noise {
                len += text.trim().len();
            }
        }
    }
    len
}

/// Cleans noisy elements, resolves relative URLs to absolute URLs, and removes tracking params.
fn sanitize_and_normalize_html(
    html_fragment: &str,
    base_url: Option<&Url>,
    options: &MarkdownOptions,
) -> String {
    let without_comments = HTML_COMMENT_RE.replace_all(html_fragment, "");
    let fragment = Html::parse_fragment(&without_comments);

    let mut out = String::with_capacity(without_comments.len());
    serialize_clean_children(fragment.root_element(), base_url, options, &mut out);
    out
}

fn serialize_clean_children(
    parent: ElementRef<'_>,
    base_url: Option<&Url>,
    options: &MarkdownOptions,
    out: &mut String,
) {
    for child in parent.children() {
        match child.value() {
            scraper::Node::Text(text) => {
                out.push_str(&html_escape_text(&text.text));
            }
            scraper::Node::Element(el) => {
                let Some(child_el) = ElementRef::wrap(child) else {
                    continue;
                };
                let tag = el.name();

                // Always drop non-content / script / invisible tags
                if matches!(
                    tag,
                    "script"
                        | "style"
                        | "noscript"
                        | "svg"
                        | "iframe"
                        | "canvas"
                        | "template"
                        | "form"
                        | "button"
                        | "input"
                        | "select"
                        | "textarea"
                        | "dialog"
                ) {
                    continue;
                }

                if !options.include_images && matches!(tag, "img" | "picture" | "source") {
                    continue;
                }

                // Drop hidden elements
                if el.attr("hidden").is_some()
                    || el.attr("aria-hidden") == Some("true")
                    || el
                        .attr("style")
                        .map(|s| {
                            let lower = s.to_ascii_lowercase();
                            lower.contains("display:none") || lower.contains("display: none")
                        })
                        .unwrap_or(false)
                {
                    continue;
                }

                // When `extract_main_content` is enabled, drop navigation chrome, footers, sidebars, and cookie banners
                if options.extract_main_content {
                    if matches!(tag, "nav" | "footer" | "aside") {
                        continue;
                    }
                    if let Some(role) = el.attr("role")
                        && matches!(
                            role,
                            "navigation"
                                | "banner"
                                | "contentinfo"
                                | "complementary"
                                | "search"
                                | "dialog"
                        )
                    {
                        continue;
                    }
                    if is_boilerplate_element(el) {
                        continue;
                    }
                }

                // Handle links (`<a>`)
                if tag == "a" {
                    if !options.include_links {
                        serialize_clean_children(child_el, base_url, options, out);
                        continue;
                    }

                    if let Some(href) = el.attr("href") {
                        let trimmed_href = href.trim();
                        if trimmed_href.is_empty()
                            || trimmed_href.starts_with('#')
                            || trimmed_href.to_ascii_lowercase().starts_with("javascript:")
                            || trimmed_href.to_ascii_lowercase().starts_with("mailto:")
                        {
                            serialize_clean_children(child_el, base_url, options, out);
                            continue;
                        }

                        let resolved = resolve_and_clean_url(trimmed_href, base_url);
                        out.push_str("<a href=\"");
                        out.push_str(&html_escape_attr(&resolved));
                        out.push_str("\">");
                        serialize_clean_children(child_el, base_url, options, out);
                        out.push_str("</a>");
                        continue;
                    }
                }

                // Handle images (`<img>`) when `include_images` is true
                if tag == "img" {
                    if let Some(src) = el.attr("src") {
                        // Skip inline base64 images to prevent massive token bloat
                        if src.trim().starts_with("data:") {
                            continue;
                        }
                        let resolved = resolve_and_clean_url(src.trim(), base_url);
                        let alt = el.attr("alt").unwrap_or("");
                        out.push_str("<img src=\"");
                        out.push_str(&html_escape_attr(&resolved));
                        out.push_str("\" alt=\"");
                        out.push_str(&html_escape_attr(alt));
                        out.push_str("\">");
                    }
                    continue;
                }

                // Standard semantic tag serialization
                out.push('<');
                out.push_str(tag);
                out.push('>');
                serialize_clean_children(child_el, base_url, options, out);
                if !is_void_element(tag) {
                    out.push_str("</");
                    out.push_str(tag);
                    out.push('>');
                }
            }
            _ => {}
        }
    }
}

fn is_boilerplate_element(el: &scraper::node::Element) -> bool {
    let id = el.attr("id").unwrap_or("").to_ascii_lowercase();
    let class = el.attr("class").unwrap_or("").to_ascii_lowercase();
    let combined = format!("{id} {class}");

    let noise_keywords = [
        "cookie-banner",
        "cookie-consent",
        "onetrust",
        "gdpr",
        "newsletter-signup",
        "social-share",
        "share-buttons",
        "advertisement",
        "ad-container",
        "sidebar",
        "site-footer",
        "site-header",
        "navbar",
        "breadcrumb",
        "popup-modal",
    ];

    noise_keywords.iter().any(|kw| combined.contains(kw))
}

fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Resolves relative URLs against `base_url` and strips noisy tracking query parameters.
pub fn resolve_and_clean_url(raw_url: &str, base_url: Option<&Url>) -> String {
    let parsed = match Url::parse(raw_url) {
        Ok(u) => Some(u),
        Err(_) => base_url.and_then(|base| base.join(raw_url).ok()),
    };

    let Some(mut url) = parsed else {
        return raw_url.to_string();
    };

    // Strip tracking query parameters to reduce token usage
    let tracking_params = [
        "utm_source",
        "utm_medium",
        "utm_campaign",
        "utm_term",
        "utm_content",
        "utm_id",
        "fbclid",
        "gclid",
        "dclid",
        "msclkid",
        "mc_cid",
        "mc_eid",
        "ref_src",
    ];

    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !tracking_params.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if pairs.is_empty() {
        url.set_query(None);
    } else {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            serializer.append_pair(&k, &v);
        }
        let cleaned_query = serializer.finish();
        url.set_query(Some(&cleaned_query));
    }

    url.to_string()
}

fn post_process_markdown(raw_md: &str, include_links: bool) -> String {
    let mut cleaned = raw_md.replace('\r', "");

    if include_links {
        cleaned = EMPTY_LINK_RE.replace_all(&cleaned, "").into_owned();
    }

    cleaned = TRAILING_SPACE_RE.replace_all(&cleaned, "").into_owned();
    cleaned = MULTI_NEWLINE_RE.replace_all(&cleaned, "\n\n").into_owned();

    cleaned.trim().to_string()
}

fn truncate_markdown_cleanly(md: &str, max_chars: usize) -> String {
    if md.chars().count() <= max_chars {
        return md.to_string();
    }

    let mut byte_idx = 0;
    for (count, (idx, _)) in md.char_indices().enumerate() {
        if count == max_chars {
            byte_idx = idx;
            break;
        }
    }
    if byte_idx == 0 {
        return md.to_string();
    }

    let slice = &md[..byte_idx];
    // Prefer breaking at the last paragraph or newline boundary
    let cut_point = slice
        .rfind("\n\n")
        .filter(|&pos| pos > (byte_idx * 3) / 4)
        .or_else(|| slice.rfind('\n').filter(|&pos| pos > (byte_idx * 3) / 4))
        .unwrap_or(byte_idx);

    format!(
        "{}\n\n---\n*[Content truncated at {} characters to reduce token consumption]*",
        slice[..cut_point].trim_end(),
        max_chars
    )
}

fn strip_tags_fallback(html: &str) -> String {
    let doc = Html::parse_fragment(html);
    doc.root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn html_escape_attr(s: &str) -> String {
    html_escape_text(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_and_strips_boilerplate_for_token_efficiency() {
        let raw_html = r#"
            <!DOCTYPE html>
            <html>
            <head>
                <title>MCP Web Server Guide</title>
                <meta name="description" content="Learn how MCP Web reduces tokens.">
                <script>console.log("huge bundle".repeat(500));</script>
                <style>body { color: #333; }</style>
            </head>
            <body>
                <nav><a href="/home">Home</a> | <a href="/pricing">Pricing</a></nav>
                <div class="cookie-banner">Accept all cookies to continue</div>
                <main>
                    <article>
                        <h1>MCP Web Server Guide</h1>
                        <p>Welcome to the <strong>Model Context Protocol</strong> web server documentation. It converts complex SPAs into compact Markdown so LLMs spend far fewer tokens reading the web.</p>
                        <p>Check out the <a href="/docs/getting-started?utm_source=twitter&fbclid=123&lang=en">Getting Started Guide</a> for configuration details and examples.</p>
                        <pre><code>cargo run -- serve</code></pre>
                    </article>
                </main>
                <footer>Copyright 2026</footer>
            </body>
            </html>
        "#;

        let doc = html_to_markdown(
            raw_html,
            "https://example.com/blog/mcp",
            &MarkdownOptions::default(),
        );

        assert_eq!(doc.title.as_deref(), Some("MCP Web Server Guide"));
        assert_eq!(
            doc.description.as_deref(),
            Some("Learn how MCP Web reduces tokens.")
        );
        assert!(doc.markdown.contains("# MCP Web Server Guide"));
        assert!(doc.markdown.contains("**Model Context Protocol**"));
        // Verify relative link was resolved AND utm_source/fbclid were stripped while keeping `lang=en`
        assert!(
            doc.markdown
                .contains("https://example.com/docs/getting-started?lang=en")
        );
        // Verify nav, cookie-banner, script, and footer were stripped
        assert!(!doc.markdown.contains("Accept all cookies"));
        assert!(!doc.markdown.contains("Copyright 2026"));
        assert!(!doc.markdown.contains("huge bundle"));
        assert!(doc.markdown_bytes < doc.raw_html_bytes);
    }
}
