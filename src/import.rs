//! Text helpers for M5 capture: slugging titles for filenames, stripping
//! fetched HTML down to plain markdown-ish paragraphs, and the [`Fetcher`]
//! seam `i` pulls a URL through.
//!
//! [`slug`] and [`html_to_markdown`] are total: any `&str` input returns a
//! value, never panics.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// End-to-end budget for one import fetch, connect and body included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// One blocking GET. The seam for `i` import: ureq in the binary, a fake in tests.
/// Nothing outside `import` and `sync` names ureq.
pub trait Fetcher {
    /// The response body of a GET on `url`, or a short one-line reason.
    fn get(&self, url: &str) -> Result<String>;
}

/// Blocking client over ureq (rustls), one agent for the process.
pub struct UreqFetcher {
    agent: ureq::Agent,
}

impl UreqFetcher {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(FETCH_TIMEOUT))
            .build();
        UreqFetcher {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher for UreqFetcher {
    fn get(&self, url: &str) -> Result<String> {
        // Errors stay short: they are shown on one status row as `import failed · <reason>`.
        let mut response = self
            .agent
            .get(url)
            .header("User-Agent", "grain")
            .call()
            .map_err(|e| anyhow!("{}", crate::sync::api::transport_reason(&e)))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(anyhow!("http {status}"));
        }
        response
            .body_mut()
            .read_to_string()
            .map_err(|e| anyhow!("{}", crate::sync::api::transport_reason(&e)))
    }
}

/// Fetch `url` and strip it: the `<title>` and the paragraph body.
pub fn fetch_title_body(fetcher: &dyn Fetcher, url: &str) -> Result<(Option<String>, String)> {
    let html = fetcher.get(url).with_context(|| format!("fetching {url}"))?;
    Ok(html_to_markdown(&html))
}

/// Lowercase ASCII slug: first six whitespace-separated words, lowercased,
/// with every maximal run of characters that are not ASCII `a-z0-9` (word
/// separators and punctuation alike, intra-word or not) folded to a single
/// `-`, then trimmed of leading/trailing `-`. Non-ASCII input (or input with
/// no ASCII alphanumerics) yields an empty string.
pub fn slug(s: &str) -> String {
    let joined = s.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    let lower = joined.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut in_run = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Decode a named or numeric HTML entity starting right after the leading
/// `&` at byte offset `start` in `html`. Returns the decoded string and the
/// byte offset just past the terminating `;` (or the entity name itself
/// when there is no `;`, in which case nothing is consumed beyond `&`).
fn decode_entity(html: &str, start: usize) -> (String, usize) {
    let rest = &html[start..];
    let Some(semi) = rest.find(';') else {
        return ("&".to_string(), start);
    };
    // Entities are short; bail out to a literal `&` if this doesn't look like one.
    if semi > 32 {
        return ("&".to_string(), start);
    }
    let body = &rest[..semi];
    let end = start + semi + 1;
    let decoded = match body {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" => Some("'".to_string()),
        "nbsp" => Some(" ".to_string()),
        _ if body.starts_with("#x") || body.starts_with("#X") => {
            u32::from_str_radix(&body[2..], 16).ok().and_then(char::from_u32).map(String::from)
        }
        _ if body.starts_with('#') => {
            body[1..].parse::<u32>().ok().and_then(char::from_u32).map(String::from)
        }
        _ => None,
    };
    match decoded {
        Some(text) => (text, end),
        None => (format!("&{body};"), end),
    }
}

/// Decode entities and collapse ASCII whitespace runs to a single space.
fn decode_and_collapse(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.char_indices().peekable();
    let mut last_was_space = false;
    while let Some((i, c)) = chars.next() {
        if c == '&' {
            let (text, end) = decode_entity(raw, i + 1);
            for dc in text.chars() {
                if dc.is_whitespace() {
                    if !last_was_space {
                        out.push(' ');
                        last_was_space = true;
                    }
                } else {
                    out.push(dc);
                    last_was_space = false;
                }
            }
            // Skip chars already consumed by the entity.
            while let Some(&(j, _)) = chars.peek() {
                if j < end {
                    chars.next();
                } else {
                    break;
                }
            }
        } else if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out
}

const DROP_WITH_CONTENTS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "head", "nav", "header", "footer", "aside",
    "form", "iframe",
];

const BLOCK_TAGS: &[&str] = &[
    "p", "div", "br", "li", "tr", "blockquote", "pre", "section", "table", "ul", "ol", "dl", "dt",
    "dd", "hr", "h1", "h2", "h3", "h4", "h5", "h6",
];

/// One token from a single-pass tokenizer over the HTML source.
enum Token<'a> {
    /// An opening or self-closing tag, e.g. `<p class="x">` -> name `p`.
    Open(&'a str),
    /// A closing tag, e.g. `</p>` -> name `p`.
    Close(&'a str),
    /// Plain text between tags.
    Text(&'a str),
}

/// Tokenize `html` into a flat sequence of tags and text runs. Never panics;
/// an unterminated `<...` at the end of input is treated as an open tag with
/// whatever name is present (possibly empty).
fn tokenize(html: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut text_start = 0usize;
    while i < len {
        if bytes[i] == b'<' {
            if text_start < i {
                tokens.push(Token::Text(&html[text_start..i]));
            }
            // Comment?
            if html[i..].starts_with("<!--") {
                let comment_start = i;
                match html[i..].find("-->") {
                    Some(off) => i += off + 3,
                    None => i = len,
                }
                let _ = comment_start;
                text_start = i;
                continue;
            }
            let tag_start = i + 1;
            let close = html[tag_start..].find('>');
            let Some(rel_end) = close else {
                // Unterminated tag: consume rest as an open tag body.
                let body = &html[tag_start..len];
                push_tag_token(&mut tokens, body);
                i = len;
                text_start = i;
                continue;
            };
            let tag_end = tag_start + rel_end;
            let body = &html[tag_start..tag_end];
            push_tag_token(&mut tokens, body);
            i = tag_end + 1;
            text_start = i;
        } else {
            i += 1;
        }
    }
    if text_start < len {
        tokens.push(Token::Text(&html[text_start..len]));
    }
    tokens
}

/// Parse a tag body (the part between `<` and `>`, exclusive) into a
/// `Token::Open` (plus a synthetic matching `Token::Close` when the tag is
/// self-closing, e.g. `<svg ... />`) or a `Token::Close`, and push it.
fn push_tag_token<'a>(tokens: &mut Vec<Token<'a>>, body: &'a str) {
    let body = body.trim();
    if let Some(rest) = body.strip_prefix('/') {
        let name = tag_name(rest);
        tokens.push(Token::Close(name));
        return;
    }
    let (open_body, self_closing) = match body.strip_suffix('/') {
        Some(rest)
            if rest.ends_with(char::is_whitespace)
                || rest.ends_with('"')
                || rest.ends_with('\'')
                || rest.trim_end() == tag_name(rest) =>
        {
            (rest.trim_end(), true)
        }
        // An unquoted attribute ending in `/` (e.g. `src=http://x.com/`) is
        // not self-closing: keep the full body, slash included, as a plain
        // open tag.
        _ => (body, false),
    };
    let name = tag_name(open_body);
    tokens.push(Token::Open(name));
    if self_closing {
        tokens.push(Token::Close(name));
    }
}

/// The tag name at the start of `s` (up to whitespace, `/` or end), lowercase-compared by caller.
fn tag_name(s: &str) -> &str {
    let end = s
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(s.len());
    &s[..end]
}

/// Case-insensitive tag name match against a lowercase ASCII name list.
fn name_is(name: &str, target: &str) -> bool {
    name.eq_ignore_ascii_case(target)
}

fn name_in(name: &str, list: &[&str]) -> bool {
    list.iter().any(|t| name_is(name, t))
}

fn heading_level(name: &str) -> Option<usize> {
    if name.len() == 2 {
        let bytes = name.as_bytes();
        if (bytes[0] == b'h' || bytes[0] == b'H') && bytes[1].is_ascii_digit() {
            let level = (bytes[1] - b'0') as usize;
            if (1..=6).contains(&level) {
                return Some(level);
            }
        }
    }
    None
}

/// Scope `html` to the first `<main>…</main>` or `<article>…</article>`
/// (whichever opens first), else `<body>…</body>`, else the whole input.
/// Matching is by tag name only (case-insensitive), not by nesting depth.
fn scope(html: &str) -> &str {
    let lower_find = |name: &str| -> Option<(usize, usize)> {
        // Find first case-insensitive `<name` open tag start and its content start/end.
        find_scoped_region(html, name)
    };
    if let Some(main_pos) = find_tag_open_pos(html, "main") {
        if let Some(article_pos) = find_tag_open_pos(html, "article") {
            if article_pos < main_pos {
                if let Some(region) = lower_find("article") {
                    return &html[region.0..region.1];
                }
            }
        }
        if let Some(region) = lower_find("main") {
            return &html[region.0..region.1];
        }
    } else if let Some(region) = lower_find("article") {
        return &html[region.0..region.1];
    }
    if let Some(region) = lower_find("body") {
        return &html[region.0..region.1];
    }
    html
}

/// Byte offset where an opening tag `<name` (case-insensitive, word-boundary)
/// starts, or `None`.
fn find_tag_open_pos(html: &str, name: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let needle = format!("<{name}");
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(&needle) {
        let pos = search_from + rel;
        let after = pos + needle.len();
        let ok_boundary = lower[after..]
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(true);
        if ok_boundary {
            return Some(pos);
        }
        search_from = pos + 1;
    }
    None
}

/// Find the content region (start, end) between the first `<name...>` and its
/// matching `</name>` (first occurrence, no nesting awareness), case-insensitive.
fn find_scoped_region(html: &str, name: &str) -> Option<(usize, usize)> {
    let open_pos = find_tag_open_pos(html, name)?;
    let lower = html.to_ascii_lowercase();
    let gt = lower[open_pos..].find('>')? + open_pos;
    let content_start = gt + 1;
    let close_needle = format!("</{name}");
    let rel_close = lower[content_start..].find(&close_needle)?;
    let content_end = content_start + rel_close;
    Some((content_start, content_end))
}

/// Convert scoped HTML into paragraph-joined markdown-ish text per the spec.
pub fn html_to_markdown(html: &str) -> (Option<String>, String) {
    let title = extract_title(html);
    let scoped = scope(html);
    let tokens = tokenize(scoped);

    let mut paragraphs: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_prefix: Option<String> = None;
    // Depth counter per dropped-with-contents tag name; >0 means "skip text".
    let mut skip_depth: usize = 0;
    let mut skip_stack: Vec<&str> = Vec::new();

    let flush = |paragraphs: &mut Vec<String>,
                 current: &mut String,
                 prefix: &mut Option<String>,
                 keep_prefix_when_empty: bool| {
        let collapsed = decode_and_collapse(current);
        let trimmed = collapsed.trim();
        if !trimmed.is_empty() {
            let text = match prefix.take() {
                Some(p) => format!("{p}{trimmed}"),
                None => trimmed.to_string(),
            };
            paragraphs.push(text);
        } else if !keep_prefix_when_empty {
            prefix.take();
        }
        current.clear();
    };

    for token in tokens {
        match token {
            Token::Text(t) => {
                if skip_depth == 0 {
                    current.push_str(t);
                }
            }
            Token::Open(name) => {
                if name_in(name, DROP_WITH_CONTENTS) {
                    skip_depth += 1;
                    skip_stack.push(dropped_name(name));
                    continue;
                }
                if skip_depth > 0 {
                    continue;
                }
                if let Some(level) = heading_level(name) {
                    flush(&mut paragraphs, &mut current, &mut current_prefix, false);
                    current_prefix = Some(format!("{} ", "#".repeat(level)));
                } else if name_is(name, "li") {
                    flush(&mut paragraphs, &mut current, &mut current_prefix, false);
                    current_prefix = Some("- ".to_string());
                } else if name_in(name, BLOCK_TAGS) {
                    flush(&mut paragraphs, &mut current, &mut current_prefix, true);
                }
                // Other tags: no boundary, no output for the tag itself.
            }
            Token::Close(name) => {
                if skip_depth > 0 {
                    if skip_stack.last().map(|n| name_is(name, n)).unwrap_or(false) {
                        skip_stack.pop();
                        skip_depth -= 1;
                    }
                    continue;
                }
                if heading_level(name).is_some() || name_is(name, "li") || name_in(name, BLOCK_TAGS)
                {
                    flush(&mut paragraphs, &mut current, &mut current_prefix, false);
                }
            }
        }
    }
    // An unclosed drop tag (`<script>` with no `</script>`) can leave
    // `skip_depth` above zero at end of input. `current` never receives text
    // while `skip_depth > 0`, so this is redundant today, but it makes the
    // invariant explicit: never flush a paragraph while still inside a
    // dropped scope.
    if skip_depth > 0 {
        current.clear();
        current_prefix = None;
    }
    flush(&mut paragraphs, &mut current, &mut current_prefix, false);

    let mut body = paragraphs.join("\n\n");
    if !body.is_empty() {
        body.push('\n');
    }
    (title, body)
}

/// Canonicalized lowercase name for tracking a dropped tag's matching close,
/// so `<Script>` closes on `</SCRIPT>` too.
fn dropped_name(name: &str) -> &str {
    DROP_WITH_CONTENTS
        .iter()
        .find(|t| name_is(name, t))
        .copied()
        .unwrap_or(name)
}

/// The decoded, whitespace-collapsed text of the first `<title>` anywhere in
/// `html` (before scoping), or `None` when absent or empty after collapsing.
fn extract_title(html: &str) -> Option<String> {
    let (start, end) = find_scoped_region(html, "title")?;
    let raw = &html[start..end];
    let decoded = decode_and_collapse(raw);
    let trimmed = decoded.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn slug_takes_six_words_lowercase_ascii() {
        assert_eq!(
            slug("Large citrus fruit, with a thick rind & mild taste?"),
            "large-citrus-fruit-with-a-thick"
        );
    }

    #[test]
    fn slug_of_non_ascii_is_empty() {
        assert_eq!(slug("柚子 是什么"), "");
        assert_eq!(slug("  "), "");
    }

    #[test]
    fn slug_folds_intra_word_punctuation_to_dashes() {
        assert_eq!(slug("Mother-in-law's tongue"), "mother-in-law-s-tongue");
    }

    #[test]
    fn html_scopes_to_main_and_drops_chrome() {
        let input = "<html><head><title>Pomelo - Wikipedia</title><style>x</style></head><body><nav>Menu</nav><main><h1>Pomelo</h1><p>The pomelo is a <b>citrus</b> fruit &amp; large.</p><script>bad()</script><ul><li>one</li><li>two</li></ul><h2>Uses</h2><p>Juice.</p></main><footer>foot</footer></body></html>";
        let (title, body) = html_to_markdown(input);
        assert_eq!(title, Some("Pomelo - Wikipedia".to_string()));
        assert_eq!(
            body,
            "# Pomelo\n\nThe pomelo is a citrus fruit & large.\n\n- one\n\n- two\n\n## Uses\n\nJuice.\n"
        );
    }

    #[test]
    fn html_without_main_uses_body() {
        let (title, body) = html_to_markdown("<body><div>a</div><aside>side</aside><div>b</div></body>");
        assert_eq!(body, "a\n\nb\n");
        assert_eq!(title, None);
    }

    #[test]
    fn html_entities_and_whitespace() {
        let (_, body) = html_to_markdown("<p>a&nbsp;&#8212;&#x41;  b\n c</p>");
        assert_eq!(body, "a —A b c\n");
    }

    #[test]
    fn html_unclosed_tags_do_not_panic() {
        let (_, body) = html_to_markdown("<p><b>open");
        assert_eq!(body, "open\n");
        let (_, body) = html_to_markdown("");
        assert_eq!(body, "");
        // Only assert that it returns without panicking; either result is acceptable.
        let (_, _body) = html_to_markdown("<");
    }

    #[test]
    fn html_is_case_insensitive() {
        let (_, body) = html_to_markdown("<MAIN><P>x</P></MAIN>");
        assert_eq!(body, "x\n");
    }

    #[test]
    fn html_self_closing_drop_tag_does_not_swallow_the_rest() {
        let (_, body) = html_to_markdown("<p>a</p><svg class=\"icon\"/><p>b</p>");
        assert_eq!(body, "a\n\nb\n");
    }

    #[test]
    fn html_unclosed_drop_tag_does_not_swallow_earlier_text() {
        let (_, body) = html_to_markdown("<p>a</p><script>x");
        assert_eq!(body, "a\n");
    }

    #[test]
    fn li_prefix_survives_an_inner_block() {
        let (_, body) = html_to_markdown("<ul><li><p>x</p></li></ul>");
        assert_eq!(body, "- x\n");
        let (_, body) = html_to_markdown("<li>a<p>b</p></li>");
        assert_eq!(body, "- a\n\nb\n");
        // An empty item does not leak its prefix onto the next paragraph.
        let (_, body) = html_to_markdown("<li></li><p>y</p>");
        assert_eq!(body, "y\n");
    }

    #[test]
    fn only_h1_to_h6_are_headings() {
        let (_, body) = html_to_markdown("<h0>x</h0>");
        assert_eq!(body, "x\n");
        let (_, body) = html_to_markdown("<h7>x</h7>");
        assert_eq!(body, "x\n");
        let (_, body) = html_to_markdown("<h6>x</h6>");
        assert_eq!(body, "###### x\n");
    }

    #[test]
    fn unquoted_attribute_ending_in_slash_is_not_self_closing() {
        // The iframe's opening tag is not self-closing, so it is a normal
        // open tag whose (dropped) content runs until `</iframe>`.
        let (_, body) =
            html_to_markdown("<p>a</p><iframe src=http://x.com/>hidden</iframe><p>b</p>");
        assert_eq!(body, "a\n\nb\n");
        // Self-closing forms (trailing-space, quoted-attribute) are still recognised.
        let (_, body) = html_to_markdown("<p>a</p><br/><img src=\"a\"/><p>b</p>");
        assert_eq!(body, "a\n\nb\n");
        // `<svg />` self-closes, so the following `x` is plain text and `<p>`
        // starts a new paragraph.
        let (_, body) = html_to_markdown("<p>a</p><svg />x<p>b</p>");
        assert_eq!(body, "a\n\nx\n\nb\n");
    }
}
