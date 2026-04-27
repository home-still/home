//! HTML shape detection + paywall-stub heuristics.
//!
//! Shared between the `paper` downloader (checks before persisting)
//! and the `hs-scribe` event watcher (checks before converting to
//! markdown). A central definition means both choke points agree on
//! what counts as junk, and any new pattern only has to be added in
//! one place.

/// True when the first ~512 bytes look like an HTML document. Used as
/// a quick gate before running the more expensive paywall heuristics.
pub fn looks_like_html(header: &[u8]) -> bool {
    let s = String::from_utf8_lossy(&header[..header.len().min(512)]).to_lowercase();
    s.contains("<!doctype html") || s.contains("<html") || s.contains("<head")
}

/// True when `content` looks like a paywall / error / landing page
/// rather than real article content.
pub fn is_paywall_html(content: &str) -> bool {
    let lower = content.to_lowercase();

    // Paywall indicators
    let has_login = lower.contains("sign in")
        || lower.contains("log in")
        || lower.contains("access denied")
        || lower.contains("403 forbidden")
        || lower.contains("subscription required")
        || lower.contains("purchase this article")
        || lower.contains("institutional access");

    // Paper indicators — meaningful article structure
    let has_article =
        lower.contains("<article") || (lower.contains("abstract") && lower.contains("references"));

    // Short pages with login prompts are almost certainly paywalls
    if has_login && content.len() < 100_000 {
        return true;
    }

    // If it has login indicators but no article structure, it's a paywall
    if has_login && !has_article {
        return true;
    }

    // Strip HTML tags and measure actual visible text
    let text_only = strip_html_tags(&lower);
    let text_len = text_only.trim().len();

    // Very short pages without article structure are junk (landing pages, error pages)
    if text_len < 500 && !has_article {
        return true;
    }

    // Loading / interstitial pages (PMC download stub, etc.)
    if lower.contains("preparing to download")
        || lower.contains("hhs vulnerability disclosure")
        || lower.contains("please wait while the document loads")
    {
        return true;
    }

    // Journal metadata pages (impact factor, citescore) with no paper body
    let is_journal_meta = lower.contains("impact factor")
        || lower.contains("citescore")
        || lower.contains("aims and scope");
    if is_journal_meta && !has_article {
        return true;
    }

    // Institutional/repository landing pages with navigation but no paper
    let is_landing = lower.contains("clinical trials")
        || lower.contains("browse collections")
        || lower.contains("search results")
        || lower.contains("cookie policy");
    if is_landing && !has_article {
        return true;
    }

    false
}

/// Strip HTML tags to get visible text content.
pub fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pmc_preparing_to_download_stub() {
        let html = "<html><head></head><body>\
            <p>Preparing to download ...</p>\
            <footer>HHS Vulnerability Disclosure</footer>\
            </body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn rejects_real_article() {
        let html = "<html><body><article>\
            <h1>Novel methods in X</h1>\
            <h2>Abstract</h2><p>We present...</p>\
            <h2>References</h2><ol><li>Smith 2020</li></ol>\
            </article></body></html>";
        assert!(!is_paywall_html(html));
    }

    #[test]
    fn detects_login_wall() {
        let html = "<html><body>Please sign in to access this article.</body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn looks_like_html_accepts_doctype() {
        assert!(looks_like_html(b"<!DOCTYPE html><html>..."));
        assert!(looks_like_html(b"<html lang=\"en\">"));
    }

    #[test]
    fn looks_like_html_rejects_pdf() {
        assert!(!looks_like_html(b"%PDF-1.7\n..."));
    }
}
