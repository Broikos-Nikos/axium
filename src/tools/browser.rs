use anyhow::Result;

/// Fetch a URL and return its text content.
/// Tries headless Chromium first (renders JavaScript), falls back to simple HTTP.
pub async fn browse_url(url: &str) -> Result<String> {
    if url.is_empty() {
        anyhow::bail!("Empty URL");
    }
    // Block dangerous URL schemes (file://, data:, javascript:, chrome://)
    let lower = url.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        anyhow::bail!("Only http:// and https:// URLs are allowed (got {})", url.split(':').next().unwrap_or("unknown"));
    }

    // Block requests to private/internal networks (SSRF protection)
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            let h = host.to_lowercase();
            if h == "localhost" || h.ends_with(".local") || h == "metadata.google.internal"
                || h.starts_with("169.254.") || h.starts_with("10.")
                || h.starts_with("192.168.") || h == "[::1]" || h == "0.0.0.0"
            {
                anyhow::bail!("URLs targeting internal/private networks are blocked");
            }
            // Check 172.16-31.x.x and 127.x.x.x
            if let Ok(ip) = h.parse::<std::net::IpAddr>() {
                match ip {
                    std::net::IpAddr::V4(v4) => {
                        if v4.is_loopback() || v4.is_private() || v4.is_link_local() {
                            anyhow::bail!("URLs targeting internal/private networks are blocked");
                        }
                    }
                    std::net::IpAddr::V6(v6) => {
                        if v6.is_loopback() {
                            anyhow::bail!("URLs targeting internal/private networks are blocked");
                        }
                    }
                }
            }
        }
    }

    // Try headless Chromium for full JavaScript rendering
    if let Ok(text) = browse_chromium(url).await {
        if !text.trim().is_empty() {
            return Ok(cap_output(text));
        }
    }

    // Fallback to simple HTTP fetch (no JavaScript)
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) Axium/0.3")
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {} for {}", status, url);
    }

    let body = resp.text().await?;
    Ok(cap_output(strip_html(&body)))
}

/// Render a page using headless Chromium with JavaScript support.
async fn browse_chromium(url: &str) -> Result<String> {
    // Try common Chromium binary names
    for browser in ["chromium-browser", "chromium", "google-chrome"] {
        if let Ok(text) = try_chromium(browser, url).await {
            return Ok(text);
        }
    }
    anyhow::bail!("No Chromium browser available")
}

async fn try_chromium(browser: &str, url: &str) -> Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new(browser)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--disable-translate",
                "--disable-extensions",
                "--dump-dom",
                "--virtual-time-budget=5000",
                url,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Chromium timed out after 30s"))??;

    if !output.status.success() {
        anyhow::bail!("Chromium exited with {}", output.status);
    }

    let html = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(strip_html(&html))
}

/// Cap text output at 8000 characters.
fn cap_output(text: String) -> String {
    if text.len() > 8000 {
        let mut b = 8000;
        while b > 0 && !text.is_char_boundary(b) { b -= 1; }
        format!("{}\n\n[Truncated — {} chars total]", &text[..b], text.len())
    } else {
        text
    }
}

/// Efficient single-pass HTML tag stripping for text extraction.
fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 3);
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut last_was_space = false;

    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let b = bytes[i];
        if !in_tag && b == b'<' {
            // Check for script/style tags using byte slice comparison
            let remaining = &bytes[i..];
            if remaining.len() >= 7 && remaining[..7].eq_ignore_ascii_case(b"<script") {
                in_script = true;
            } else if remaining.len() >= 6 && remaining[..6].eq_ignore_ascii_case(b"<style") {
                in_style = true;
            } else if remaining.len() >= 9 && remaining[..9].eq_ignore_ascii_case(b"</script>") {
                in_script = false;
            } else if remaining.len() >= 8 && remaining[..8].eq_ignore_ascii_case(b"</style>") {
                in_style = false;
            }
            in_tag = true;
        } else if in_tag && b == b'>' {
            in_tag = false;
        } else if !in_tag && !in_script && !in_style {
            if b.is_ascii_whitespace() {
                if !last_was_space {
                    result.push(' ');
                    last_was_space = true;
                }
            } else {
                // Safe: we handle multi-byte by checking for ASCII first
                if b.is_ascii() {
                    result.push(b as char);
                } else {
                    // Multi-byte UTF-8: extract the full character
                    if let Some(c) = html[i..].chars().next() {
                        result.push(c);
                        i += c.len_utf8() - 1; // -1 because loop increments
                    }
                }
                last_was_space = false;
            }
        }
        i += 1;
    }

    // Decode common HTML entities (&amp; last to avoid double-decoding)
    result
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Search the web via DuckDuckGo and return top results.
pub async fn web_search(query: &str, http: &reqwest::Client) -> Result<String> {
    if query.trim().is_empty() {
        anyhow::bail!("Empty search query");
    }

    let encoded = urlencoding::encode(query);
    let url = format!("https://html.duckduckgo.com/html/?q={}", encoded);

    let resp = http
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) Axium/0.3")
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Search request failed: {}", e))?;

    if !resp.status().is_success() {
        anyhow::bail!("DuckDuckGo returned HTTP {}", resp.status());
    }

    let html = resp.text().await?;

    // Extract results: title from <a class="result__a">, snippet from <a class="result__snippet">
    let mut results = Vec::new();

    // Extract result URLs (uddg= parameter contains the actual URL)
    for result_block in html.split("class=\"result results_links") {
        if results.len() >= 8 { break; }

        // Extract title + URL from result__a link
        let title = extract_between(result_block, "class=\"result__a\"", "</a>")
            .map(|s| strip_html(&s));
        let raw_href = extract_between(result_block, "class=\"result__a\" href=\"", "\"");
        let url = raw_href.and_then(|h| extract_uddg_url(&h));

        // Extract snippet from result__snippet
        let snippet = extract_between(result_block, "class=\"result__snippet\"", "</a>")
            .map(|s| strip_html(&s));

        if let (Some(title), Some(url)) = (title, url) {
            let title = title.trim().to_string();
            let snippet = snippet.map(|s| s.trim().to_string()).unwrap_or_default();
            if !title.is_empty() && !url.is_empty() {
                results.push(format!(
                    "{}. {} \n   {}\n   {}",
                    results.len() + 1,
                    title,
                    url,
                    snippet
                ));
            }
        }
    }

    if results.is_empty() {
        Ok(format!("No results found for: {}", query))
    } else {
        Ok(results.join("\n\n"))
    }
}

/// Extract text between a start marker and an end marker.
fn extract_between<'a>(html: &'a str, start: &str, end: &str) -> Option<String> {
    let start_idx = html.find(start)?;
    let after_start = &html[start_idx + start.len()..];
    // Skip past the closing > of the opening tag
    let content_start = after_start.find('>')? + 1;
    let content = &after_start[content_start..];
    let end_idx = content.find(end)?;
    Some(content[..end_idx].to_string())
}

/// Extract the actual URL from DuckDuckGo's redirect: ?uddg=ENCODED_URL&...
fn extract_uddg_url(href: &str) -> Option<String> {
    if let Some(start) = href.find("uddg=") {
        let after = &href[start + 5..];
        let end = after.find('&').unwrap_or(after.len());
        let encoded = &after[..end];
        Some(urlencoding::decode(encoded).unwrap_or_default().to_string())
    } else if href.starts_with("http") {
        Some(href.to_string())
    } else {
        None
    }
}

