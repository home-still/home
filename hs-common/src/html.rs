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

/// True if `needle` occurs in `haystack` bounded by a non-alphanumeric char
/// (or a string edge) on both sides — i.e. as a standalone phrase, not as a
/// substring of a longer word. Guards short login phrases like "sign in" from
/// false-matching inside legitimate prose ("rendering system design in pbrt",
/// "we assign in the loop", "log into" narration).
fn contains_word_bounded(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let after_ok = end >= haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// True when `content` looks like a paywall / error / landing page
/// rather than real article content.
pub fn is_paywall_html(content: &str) -> bool {
    let lower = content.to_lowercase();

    // Paywall indicators. "sign in" / "log in" are short enough to appear as
    // substrings of ordinary words ("de[sign in]g", "cata[log in]g"), so they
    // require word boundaries; the longer phrases are specific enough as-is.
    let has_login = contains_word_bounded(&lower, "sign in")
        || contains_word_bounded(&lower, "log in")
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

    // Bot-challenge interstitials. Origin servers (PMC, OUP, etc.) bounce
    // suspected bot traffic to a Google reCAPTCHA challenge or a Cloudflare
    // "checking your browser" page. The HTML is unmistakable — base href
    // points at the challenge endpoint, or the page literally announces the
    // check. Without this gate, the downloader saves the verbatim challenge
    // HTML as if it were a legitimate article and the watcher renders a
    // ~131-byte markdown stub.
    if lower.contains("www.google.com/recaptcha/challengepage")
        || lower.contains("checking your browser before accessing")
        || lower.contains("just a moment...")
    {
        return true;
    }

    // Wiley "Cookies disabled" landing page. When Wiley Online Library
    // refuses a request (no JS / no cookies / suspected bot), it returns a
    // navigation-chrome stub whose visible text is dominated by the
    // "Cookies are disabled for this browser. Wiley Online Library requires
    // cookies for authentication..." sentence. Full HTML is large enough
    // that the generic short-page heuristic misses it; the post-conversion
    // markdown is ~366 bytes. The substring below is unique to Wiley's
    // interstitial copy and won't appear in real article body text.
    if lower.contains("wiley online library requires cookies") {
        return true;
    }

    // Anubis / BotStopper Proof-of-Work bot challenges. Self-hosted
    // anti-scraper pages some journals/repos now front their content with.
    // The HTML body is ~5-7 KB (above the short-page heuristic), so anchor
    // on the unique boilerplate phrase. "Verifying connection / one moment
    // while we verify your network connection" is the third variant —
    // short and origin-agnostic. Round-3: Elsevier PT cookie banner,
    // Akamai-style CAPTCHA ("made us think that you are a bot"),
    // generic "Request Rejected" anti-bot page.
    if lower.contains("ai companies aggressively scraping")
        || lower.contains("one moment while we verify your network connection")
        || lower.contains("as páginas que você visitou e os links em que clicou")
        || lower.contains("made us think that you are a bot")
        || lower.contains("to protect our site from automated bots")
        || lower.contains("this site requires cookies to be enabled to function")
        || lower.contains("if you are trying to perform text/data mining")
        || lower.contains("traffic control and bot detection")
        || lower.contains("data, including cookies, are used to provide services")
        || lower.contains("nós usamos cookies para melhorar sua experiência")
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

    // Site-template chrome that VLM/HTML extraction has historically pulled
    // in place of article content. Each set of markers is the diagnostic /
    // navigation furniture of the host site; real papers don't carry them.
    // Guarded by `!has_article` so a real paper that happens to mention one
    // of these strings is not false-positived.
    let is_cambridge_chrome = lower.contains("hostname:")
        && lower.contains("render date:")
        && lower.contains("page-component-");
    let is_pmc_chrome =
        lower.contains("pmcid") && lower.contains("pmid") && lower.contains("copyright notice");
    let is_openalex_chrome = lower.contains("find articles by")
        || lower.contains("create github issue for staff review");
    if (is_cambridge_chrome || is_pmc_chrome || is_openalex_chrome) && !has_article {
        return true;
    }

    false
}

/// True when `content` matches a *known* anti-bot interstitial / cookie-wall
/// stub signature. Narrower than [`is_paywall_html`] — does not include the
/// heuristic short-page / no-article-structure rules, which can false-positive
/// on a legitimately short editorial. Safe to use as a destructive-purge gate
/// where false positives would delete real papers.
pub fn is_known_interstitial(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("www.google.com/recaptcha/challengepage")
        || lower.contains("checking your browser before accessing")
        || lower.contains("just a moment...")
        || lower.contains("wiley online library requires cookies")
        || lower.contains("preparing to download")
        || lower.contains("hhs vulnerability disclosure")
        // Anubis / BotStopper Proof-of-Work bot challenges share the same
        // boilerplate prose. The brand-name strings ("Anubis", "BotStopper")
        // sometimes lose their surrounding whitespace through the
        // HTML→markdown round-trip ("set upBotStopperto"), so anchor on a
        // shared sentence fragment instead. The full phrase is unique to
        // this anti-scraper page and won't appear in academic body text.
        || lower.contains("ai companies aggressively scraping")
        // Generic "verifying connection" stub seen on at least one DOI
        // (10.24124_*). The full sentence is specific enough to avoid
        // false-positives on body text that happens to mention "verify".
        || lower.contains("one moment while we verify your network connection")
        // Elsevier / ScienceDirect Portuguese cookie banner — surfaces when
        // the request lands on the PT-BR locale and gets the consent page
        // instead of the article. The phrase below is from the consent body.
        || lower.contains("as páginas que você visitou e os links em que clicou")
        // Akamai-style CAPTCHA / bot-block (Optica Publishing Group, others).
        // Distinctive boilerplate; the "Incident ID" line is also unique but
        // varies per request, so anchor on the static sentence.
        || lower.contains("made us think that you are a bot")
        // Generic "Request Rejected" / "Preserving Human Intellect" anti-bot
        // page (seen on bjas.journals.ekb.eg etc.). Either substring alone is
        // unique to this block-page boilerplate.
        || lower.contains("to protect our site from automated bots")
        // Generic "site requires cookies to be enabled" stub (seen on
        // 10.1097_chi.* and similar). Stronger than the Wiley variant —
        // unconditional on any markdown that says it.
        || lower.contains("this site requires cookies to be enabled to function")
        // Optica-style "Verification required ... text/data mining" challenge.
        // The TDM-specific carve-out is the unique anchor; real papers don't
        // tell readers to "contact Customer Service" if they're text-mining.
        || lower.contains("if you are trying to perform text/data mining")
        // Cambridge / CABI / Informa "Traffic control and bot detection"
        // page. Specific enough that real article body text won't match.
        || lower.contains("traffic control and bot detection")
        // Polish / EU GDPR cookie consent banner that some publishers
        // (versita.com, sciendo) prepend or append to the article body.
        // Catches both the full-stub case and the contamination case.
        || lower.contains("data, including cookies, are used to provide services")
        // Brazilian gov.br / Capes cookie banner appended to articles
        // accessed through the Capes federation. Same pattern as above.
        || lower.contains("nós usamos cookies para melhorar sua experiência")
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
    fn login_phrase_inside_word_is_not_a_paywall() {
        // "sign in" as a substring of "design in" / "log in" inside
        // "cataloging in" must NOT trip the login heuristic. Regression for a
        // legit short book chapter (PBR3 "Retrospective") that was rejected as
        // `paywall_html` because "rendering system design in pbrt" contains
        // the substring "sign in".
        let body: String = std::iter::repeat_n(
            "pbrt represents one point in the space of rendering system design \
             in practice, and cataloging in detail how we assign in the loop \
             lets designers reason about tradeoffs. ",
            8,
        )
        .collect();
        let html = format!("<html><body><p>{body}</p></body></html>");
        assert!(html.len() < 100_000 && !html.contains("<article"));
        assert!(!is_paywall_html(&html));
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

    #[test]
    fn detects_cambridge_core_paywall_chrome() {
        let html = "<html><body><div id=\"page-component-77f85d65b8\">Login\
            </div><footer>Hostname: page-component-77f85d65b8 Render date: \
            2026-04-15 Total loading time: 0</footer></body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_pmc_chrome_without_article() {
        let html = "<html><body>PMCID: PMC1234 PMID: 5678 Copyright notice \
            All rights reserved</body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_openalex_landing_page() {
        let html = "<html><body><nav>Find articles by author or title</nav>\
            <div>Create GitHub issue for staff review</div></body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_google_recaptcha_challenge_page() {
        // Verbatim shape of the 21,285-byte challenge HTML observed on
        // 2026-05-05 for three different DOIs that bounced to reCAPTCHA.
        // The `<base href>` is the unmistakable marker — strip_html_tags
        // surfaces the inline JavaScript as "text" so the generic length
        // and login heuristics don't fire.
        let html = "<!doctype html><html lang=\"en-US\" dir=\"ltr\">\
            <head><base href=\"https://www.google.com/recaptcha/challengepage/\">\
            <script>window['ppConfig'] = {productName: 'RecaptchaChallengePageUi'};\
            </script></head><body></body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_cloudflare_browser_check_interstitial() {
        let html = "<html><body>Checking your browser before accessing \
            pmc.ncbi.nlm.nih.gov ... Click here if you are not automatically \
            redirected after 5 seconds.</body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_wiley_cookies_disabled_landing() {
        // Reproduction of the 366-byte markdown observed across 85 Wiley
        // DOIs. The HTML version has enough chrome (scripts, navigation)
        // that text_len < 500 doesn't fire — relies on the explicit
        // substring match.
        let html = "<html><body><nav>Journal · Articles · Actions</nav>\
            <h3>Cookies disabled</h3>\
            <p>Cookies are disabled for this browser. Wiley Online Library \
            requires cookies for authentication and use of other site features; \
            therefore, cookies must be enabled to browse the site.</p>\
            </body></html>";
        assert!(is_paywall_html(html));
    }

    #[test]
    fn detects_wiley_landing_in_converted_markdown() {
        // Same string survives the HTML → markdown round-trip; the gate
        // must fire on the markdown form too so distill can refuse it
        // even when an upstream gap let the HTML through.
        let md = "- __Journal\n- __Articles\n## Tools\n### Cookies disabled\n\n\
            Cookies are disabled for this browser. Wiley Online Library \
            requires cookies for authentication and use of other site features.";
        assert!(is_paywall_html(md));
    }

    #[test]
    fn known_interstitial_matches_pmc_cloudflare() {
        let md = "Checking your browser before accessing pmc.ncbi.nlm.nih.gov ...\n\
                  Clickhereif you are not automatically redirected after 5 seconds.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_wiley() {
        let md = "### Cookies disabled\n\n\
                  Cookies are disabled for this browser. Wiley Online Library \
                  requires cookies for authentication and use of other site features.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_anubis() {
        let md = "# Making sure you're not a bot!\n\nLoading...\n\n\
                  You are seeing this because the administrator of this website \
                  has set up Anubis to protect the server against the scourge of \
                  AI companies aggressively scraping websites.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_botstopper() {
        let md = "# Ensuring the security of your connection\n\nLoading...\n\n\
                  You are seeing this because the administrator of this website \
                  has set upBotStopperto protect the server against the scourge of \
                  AI companies aggressively scraping websites.";
        // The HTML→markdown round-trip can collapse "set up BotStopper to" into
        // "set upBotStopperto" — tolerate that by lowercasing on the substring.
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_verifying_connection() {
        let md = "# Verifying connection\n\nOne moment while we verify your network connection.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_elsevier_pt_cookie_banner() {
        let md = "as páginas que você visitou e os links em que clicou. \
                  Nenhuma dessas informações pode ser usada para identificá-lo.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_akamai_captcha() {
        let md = "# We apologize for the inconvenience...\n\n\
                  ...but your activity and behavior on this site made us think \
                  that you are a bot.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_request_rejected() {
        let md = "## Request Rejected\n\n\
                  To protect our site from automated bots, your request has been flagged.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_cookies_required_stub() {
        let md = "This site requires Cookies to be enabled to function. \
                  Please ensure Cookies are turned on and then re-visit the desired page.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_optica_verification() {
        let md =
            "# Verification required!\n\nIn order to better serve you and keep this site secure, \
                  please complete this challenge. If you are trying to perform text/data mining, \
                  please contact Customer Service for assistance.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_cambridge_traffic_control() {
        let md = "# Traffic control and bot detection...\n\n\
                  If this check is preventing you from making use of our resources, \
                  make sure you have cookies enabled.";
        assert!(is_known_interstitial(md));
    }

    #[test]
    fn known_interstitial_matches_eu_cookie_banner_trailer() {
        // Mixed-content case: the chunk lands at the END of a real article
        // markdown. The signature must match the chunk itself, not the whole
        // doc — used by the per-chunk scrub.
        let chunk = "Data, including cookies, are used to provide services, \
                     improve the user experience and to analyze the traffic.";
        assert!(is_known_interstitial(chunk));
    }

    #[test]
    fn known_interstitial_matches_capes_pt_cookie_banner() {
        let chunk = "Nós usamos cookies para melhorar sua experiência de navegação no portal.";
        assert!(is_known_interstitial(chunk));
    }

    #[test]
    fn known_interstitial_rejects_short_real_article() {
        // A short editorial that `is_paywall_html` would heuristically flag
        // (no <article>, no abstract/references headings, < 500 chars).
        // `is_known_interstitial` must NOT delete this — destructive-purge
        // path needs a tighter gate.
        let md = "# Editorial: New approaches to X\n\n\
                  Recent advances in field Y have prompted reconsideration of \
                  longstanding assumptions about Z. We argue that the field \
                  should adopt a different framework.";
        assert!(!is_known_interstitial(md));
    }

    #[test]
    fn pmc_chrome_with_real_article_is_not_paywall() {
        // Real PMC-hosted paper still has those identifier strings; the
        // !has_article guard must let it through.
        let html = "<html><body><article><h1>Real paper</h1>\
            <h2>Abstract</h2><p>...</p>\
            <h2>References</h2><ol><li>x</li></ol>\
            PMCID: PMC1234 PMID: 5678 Copyright notice</article></body></html>";
        assert!(!is_paywall_html(html));
    }
}
