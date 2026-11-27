//! Signed, expiring links to the documents, for the MCP tools.
//!
//! A search hit names a document the caller cannot open: the
//! `pdf_path` it carries is a storage key on the server's disk. This
//! module turns that into a `links` object — a LAN URL and a remote
//! URL — that the user of the agent can actually click.
//!
//! The links are signed by the same code the docs service verifies
//! with, so a link the MCP hands out opens exactly what the service
//! serves.
//!
//! A **short** link is an alias for one of those: a 12-character code stored
//! in the corpus, answered by the docs service with a redirect to the signed
//! URL. It is minted only when a caller asks, because minting one is a
//! storage write and a search hit is not the right place to make one.

use hs_common::docs::{Highlight, LinkFormat, LinkKey, LinkParams, ShortLink};

/// The `links` object on a document-returning tool's response.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct DocumentLinks {
    /// The original document: the PDF, the HTML page or the EPUB.
    pub source: Option<LinkPair>,
    /// The converted markdown.
    pub markdown: Option<LinkPair>,
    /// When the links stop working.
    pub expires_at: String,
}

/// One document's two URLs.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct LinkPair {
    /// The URL that works from the LAN.
    pub lan: String,
    /// The URL that works from away from home.
    pub remote: String,
}

/// A short link: one code, both origins.
///
/// The code is stored once in the corpus (see
/// [`hs_common::docs::ShortLinks`]) and answered with a redirect that is
/// relative, so the LAN and remote forms differ only in the origin and both
/// keep working when the link is re-minted under the same code.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ShortLinkPair {
    /// The URL that works from the LAN.
    pub lan: String,
    /// The URL that works from away from home.
    pub remote: String,
    /// The code itself, which is what both URLs end with.
    pub code: String,
}

/// The mints links for the tools.
pub struct LinkMinter {
    key: Option<LinkKey>,
    lan_base_url: Option<String>,
    remote_base_url: Option<String>,
    link_ttl_secs: u64,
    subject: String,
}

impl LinkMinter {
    /// Build a minter from the `docs:` config section.
    ///
    /// A missing `docs:` section is not an error: the tools still work,
    /// they just do not carry links. That is the difference between
    /// "this deployment has not configured links" and "this deployment
    /// is broken", and only the second is worth failing over. A section
    /// that is there and cannot be read is the second: taken for a missing
    /// one, it would send the operator looking for what is in the file.
    pub fn from_config(
        file: &hs_common::config_file::ConfigFile,
    ) -> Result<Self, hs_common::config_file::ConfigError> {
        let config: Option<DocsConfig> = file.section::<DocsConfig>("docs")?;
        let key = match hs_common::docs::load_or_create_key(&hs_common::docs::default_key_path()) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::warn!("document links are unavailable: {e:#}");
                None
            }
        };
        Ok(Self {
            key,
            lan_base_url: config.as_ref().and_then(|c| c.lan_base_url.clone()),
            remote_base_url: config.as_ref().and_then(|c| c.remote_base_url.clone()),
            link_ttl_secs: config
                .as_ref()
                .and_then(|c| c.link_ttl_secs)
                .unwrap_or(DEFAULT_TTL_SECS),
            subject: config
                .as_ref()
                .and_then(|c| c.default_subject.clone())
                .unwrap_or_else(default_subject),
        })
    }

    /// Is this minter able to mint anything?
    pub fn is_enabled(&self) -> bool {
        self.key.is_some() && self.lan_base_url.is_some() && self.remote_base_url.is_some()
    }

    /// Sign one link to one view of a document.
    ///
    /// `None` when this host holds no signing key, or when the link is one
    /// the signer refuses: a stem that is not a stem, or a page view that
    /// names no page.
    ///
    /// `page` is the page the passage is on, `passage` the text to select
    /// (a quote, or the passage's first line) and `highlight` the words to
    /// box. The page is what a PDF viewer scrolls to; the text is what a
    /// text fragment names, so the browser selects the passage itself; the
    /// highlight is what the viewer, the page view and the page image draw.
    ///
    /// `collection` is the store the document is in. It is part of the
    /// signature, so a link for a personal document is a capability for
    /// that document and not for an academic one with the same stem.
    pub fn link(
        &self,
        stem: &str,
        collection: &str,
        format: LinkFormat,
        page: Option<u32>,
        passage: Option<&str>,
        highlight: Option<&Highlight>,
    ) -> Option<MintedLink> {
        let params = hs_common::docs::mint(
            self.key.as_ref()?,
            stem,
            format,
            &self.subject,
            collection,
            page,
            highlight,
            self.link_ttl_secs,
        )
        .ok()?;
        Some(MintedLink {
            fragment: fragment_for(format, params.page, passage),
            params,
        })
    }

    /// The two URLs a signed link is opened on. `None` when the base URLs
    /// are not configured.
    pub fn urls(&self, link: &MintedLink) -> Option<LinkPair> {
        Some(LinkPair {
            lan: url(self.lan_base_url.as_ref()?, link),
            remote: url(self.remote_base_url.as_ref()?, link),
        })
    }

    /// Mint the source and markdown links of one document: what a search
    /// hit and a catalog entry carry.
    ///
    /// `None` when links are not configured, or when the document's
    /// stem is not a stem: a document with no valid stem has no link.
    /// `page`, `passage` and `collection` are as for [`LinkMinter::link`].
    pub fn mint(
        &self,
        stem: &str,
        collection: &str,
        page: Option<u32>,
        passage: Option<&str>,
    ) -> Option<DocumentLinks> {
        if !self.is_enabled() {
            return None;
        }
        let source = self.link(stem, collection, LinkFormat::Source, page, passage, None);
        let markdown = self.link(stem, collection, LinkFormat::Markdown, page, passage, None);
        let expires_at = source
            .iter()
            .chain(&markdown)
            .map(MintedLink::expires_at)
            .max()?;
        Some(DocumentLinks {
            source: source.as_ref().and_then(|link| self.urls(link)),
            markdown: markdown.as_ref().and_then(|link| self.urls(link)),
            expires_at: expires_at_iso(expires_at),
        })
    }

    /// The two URLs a code answers on.
    pub fn short_urls(&self, code: &str) -> Option<ShortLinkPair> {
        Some(short_link_urls(
            self.lan_base_url.as_ref()?,
            self.remote_base_url.as_ref()?,
            code,
        ))
    }
}

/// The `docs:` section of `~/.home-still/config.yaml`.
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct DocsConfig {
    /// The base URL the LAN links point at, e.g. `http://<host>:7446`.
    #[serde(default)]
    pub lan_base_url: Option<String>,
    /// The base URL the remote links point at, e.g. `https://<host>`.
    #[serde(default)]
    pub remote_base_url: Option<String>,
    /// How long a link works, in seconds.
    #[serde(default)]
    pub link_ttl_secs: Option<u64>,
    /// The subject links are bound to.
    #[serde(default)]
    pub default_subject: Option<String>,
}

/// The default link lifetime, in seconds.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// The URL of a signed link on one origin.
///
/// The fragment points the browser at the passage: a page for a PDF, a
/// quoted line for the markdown. The page is *also* in the query
/// string, where the signature covers it.
fn url(base: &str, link: &MintedLink) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}{}{}", link.params.to_path(), link.fragment)
}

/// One signed link: the request the docs service verifies, and where the
/// browser lands in what the service answers.
///
/// Built by [`LinkMinter::link`]. The long URLs ([`LinkMinter::urls`]) and
/// the short link's record ([`MintedLink::short_link`]) are read from the
/// same signature, so a code redirects to exactly the link the caller was
/// also given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedLink {
    params: LinkParams,
    fragment: String,
}

impl MintedLink {
    /// When the link stops working, as a Unix timestamp in seconds.
    pub fn expires_at(&self) -> u64 {
        self.params.expires_at
    }

    /// What a short link to this link is stored as.
    ///
    /// The target is a path, so one code works on whichever origin served
    /// it. The fragment is stored with it because a redirect cannot put one
    /// back: the browser is already following a `Location` by the time it
    /// would matter.
    pub fn short_link(&self) -> ShortLink {
        ShortLink {
            target: self.params.to_path(),
            fragment: self.fragment.clone(),
            expires_at: self.params.expires_at,
        }
    }
}

/// The fragment that points a browser at the part of the document a link is
/// about.
///
/// The source is a PDF or an HTML page: the browser can only scroll it to a
/// page. The markdown is text: the browser can select the passage in it. The
/// viewer, the page view and the page image show the passage themselves, from
/// the page and the highlight in the query, where the signature covers them.
/// One function, because a long link and the short link that redirects to it
/// must land the reader in the same place.
fn fragment_for(format: LinkFormat, page: Option<u32>, passage: Option<&str>) -> String {
    match format {
        LinkFormat::Source => page
            .map(hs_common::docs::pdf_page_fragment)
            .unwrap_or_default(),
        LinkFormat::Markdown => passage
            .map(hs_common::docs::text_fragment)
            .unwrap_or_default(),
        LinkFormat::Viewer | LinkFormat::Page | LinkFormat::PageImage => String::new(),
    }
}

/// The subject a link is bound to when none is configured.
fn default_subject() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The two URLs a short-link code answers on.
///
/// The code is the whole path segment, so the URL carries nothing that names
/// the document: what it opens lives in the stored record and is checked when
/// the redirect is followed.
fn short_link_urls(lan_base: &str, remote_base: &str, code: &str) -> ShortLinkPair {
    let lan = lan_base.trim_end_matches('/');
    let remote = remote_base.trim_end_matches('/');
    ShortLinkPair {
        lan: format!("{lan}{}/{code}", hs_common::docs::SHORT_LINK_PATH),
        remote: format!("{remote}{}/{code}", hs_common::docs::SHORT_LINK_PATH),
        code: code.to_string(),
    }
}

/// The expiry, as an ISO timestamp.
pub(crate) fn expires_at_iso(unix_secs: u64) -> String {
    let dt = chrono::DateTime::from_timestamp(unix_secs as i64, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A minter built for a test: a fixed key, fixed URLs.
#[cfg(test)]
impl LinkMinter {
    pub fn for_test(key: LinkKey, lan: &str, remote: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            key: Some(key),
            lan_base_url: Some(lan.to_string()),
            remote_base_url: Some(remote.to_string()),
            link_ttl_secs: DEFAULT_TTL_SECS,
            subject: "test".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minter() -> LinkMinter {
        let key = LinkKey::from_bytes(vec![5u8; 32]).unwrap();
        let m = LinkMinter::for_test(key, "http://host:7446", "https://public.example");
        std::sync::Arc::try_unwrap(m).unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn a_minted_link_carries_both_urls_and_an_expiry() {
        let links = minter()
            .mint("10.1234_abcd", "academic", None, None)
            .unwrap();
        let source = links.source.unwrap();
        assert!(source
            .lan
            .starts_with("http://host:7446/docs/v1/10.1234_abcd/source?"));
        assert!(source
            .remote
            .starts_with("https://public.example/docs/v1/10.1234_abcd/source?"));
        assert!(!links.expires_at.is_empty());
        let markdown = links.markdown.unwrap();
        assert!(markdown.lan.contains("/markdown?"));
    }

    #[test]
    fn a_link_names_the_page_and_points_at_it() {
        // The PDF link scrolls to the page: the page is in the query
        // (where the signature covers it) and in the fragment (where
        // the browser reads it).
        let links = minter()
            .mint("10.1234_abcd", "academic", Some(11), None)
            .unwrap();
        let source = links.source.unwrap();
        assert!(source.lan.contains("page=11"), "{}", source.lan);
        assert!(source.lan.ends_with("#page=11"), "{}", source.lan);
        assert!(source.remote.ends_with("#page=11"), "{}", source.remote);
    }

    #[test]
    fn a_markdown_link_quotes_the_passage() {
        // The markdown link selects the passage: a text fragment names
        // the text, and the browser finds it in the document it
        // loaded.
        let links = minter()
            .mint(
                "10.1234_abcd",
                "academic",
                Some(11),
                Some("The passage's first line"),
            )
            .unwrap();
        let markdown = links.markdown.unwrap();
        assert!(markdown.lan.contains("#:~:text="), "{}", markdown.lan);
        assert!(
            markdown.lan.contains("The%20passage%27s%20first%20line"),
            "{}",
            markdown.lan
        );
        // The PDF link points at a page, not at text.
        assert!(links.source.unwrap().lan.ends_with("#page=11"));
    }

    #[test]
    fn a_stem_that_is_not_a_stem_has_no_link() {
        assert!(minter()
            .mint("../etc/passwd", "academic", None, None)
            .is_none());
        assert!(minter().mint("", "academic", None, None).is_none());
    }

    #[test]
    fn a_short_link_is_a_code_on_both_origins() {
        let pair = minter().short_urls("abcdefghjkmn").unwrap();
        assert_eq!(pair.lan, "http://host:7446/s/abcdefghjkmn");
        assert_eq!(pair.remote, "https://public.example/s/abcdefghjkmn");
        assert_eq!(pair.code, "abcdefghjkmn");
    }

    #[test]
    fn a_short_link_carries_the_page_and_the_fragment() {
        // The redirect points at a path, so the same code works on whichever
        // origin served it; the page travels in the query *and* in the fragment
        // because a browser cannot be made to re-add a fragment after a
        // redirect.
        let short = minter()
            .link(
                "10.1234_abcd",
                "academic",
                LinkFormat::Source,
                Some(11),
                None,
                None,
            )
            .unwrap()
            .short_link();
        assert!(
            short.target.starts_with("/docs/v1/10.1234_abcd/source?"),
            "{short:?}"
        );
        assert!(short.target.contains("page=11"));
        assert_eq!(short.fragment, "#page=11");
        assert!(short.expires_at > hs_common::docs::now_secs());
    }

    #[test]
    fn a_markdown_short_link_quotes_the_passage() {
        let short = minter()
            .link(
                "10.1234_abcd",
                "academic",
                LinkFormat::Markdown,
                Some(11),
                Some("The passage's first line"),
                None,
            )
            .unwrap()
            .short_link();
        assert!(short.target.contains("/markdown?"), "{short:?}");
        assert!(short.fragment.starts_with("#:~:text="), "{short:?}");
    }

    /// The code and the long URL are read from one signature: what the
    /// redirect lands on is the link the caller was also given.
    #[test]
    fn a_short_link_redirects_to_the_long_link() {
        let m = minter();
        let link = m
            .link(
                "10.1234_abcd",
                "academic",
                LinkFormat::Source,
                Some(11),
                None,
                None,
            )
            .unwrap();
        let lan = m.urls(&link).unwrap().lan;
        assert_eq!(
            format!("http://host:7446{}", link.short_link().location()),
            lan
        );
    }

    /// The viewer and the page view show the passage themselves: the words
    /// to box travel in the query, where the signature covers them, and the
    /// link needs no fragment.
    #[test]
    fn a_viewer_link_carries_the_highlight_in_its_query() {
        let highlight = Highlight::parse("11.4-9").unwrap();
        let m = minter();
        for format in [LinkFormat::Viewer, LinkFormat::Page] {
            let link = m
                .link(
                    "10.1234_abcd",
                    "academic",
                    format,
                    Some(11),
                    Some("The passage's first line"),
                    Some(&highlight),
                )
                .unwrap();
            let lan = m.urls(&link).unwrap().lan;
            assert!(lan.contains("page=11&hl=11.4-9&sig="), "{lan}");
            assert!(!lan.contains('#'), "{lan}");
        }
    }

    /// A page view is one page of the document: a link to it that names no
    /// page is refused when it is signed, not answered with a 404 later.
    #[test]
    fn a_page_link_without_a_page_is_not_minted() {
        assert!(minter()
            .link(
                "10.1234_abcd",
                "academic",
                LinkFormat::Page,
                None,
                None,
                None
            )
            .is_none());
    }

    #[test]
    fn a_minter_without_configuration_mints_nothing() {
        let m = LinkMinter {
            key: None,
            lan_base_url: None,
            remote_base_url: None,
            link_ttl_secs: 60,
            subject: "test".to_string(),
        };
        assert!(!m.is_enabled());
        assert!(m.mint("10.1234_abcd", "academic", None, None).is_none());
    }
}
