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

use hs_common::docs::{LinkFormat, LinkKey};

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
    /// is broken", and only the second is worth failing over.
    pub fn from_config(file: &hs_common::config_file::ConfigFile) -> Self {
        let config: Option<DocsConfig> = file.section::<DocsConfig>("docs").ok().flatten();
        let key = match hs_common::docs::load_or_create_key(&hs_common::docs::default_key_path()) {
            Ok(key) => Some(key),
            Err(e) => {
                tracing::warn!("document links are unavailable: {e:#}");
                None
            }
        };
        Self {
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
        }
    }

    /// Is this minter able to mint anything?
    pub fn is_enabled(&self) -> bool {
        self.key.is_some() && self.lan_base_url.is_some() && self.remote_base_url.is_some()
    }

    /// Mint the links for one document.
    ///
    /// `None` when links are not configured, or when the document's
    /// stem is not a stem: a document with no valid stem has no link.
    pub fn mint(&self, stem: &str) -> Option<DocumentLinks> {
        let key = self.key.as_ref()?;
        let lan_base = self.lan_base_url.as_ref()?;
        let remote_base = self.remote_base_url.as_ref()?;

        let mut out = DocumentLinks {
            source: None,
            markdown: None,
            expires_at: String::new(),
        };
        let mut expires_at = 0u64;
        for (format, slot) in [
            (LinkFormat::Source, &mut out.source),
            (LinkFormat::Markdown, &mut out.markdown),
        ] {
            let Ok(params) =
                hs_common::docs::mint(key, stem, format, &self.subject, self.link_ttl_secs)
            else {
                continue;
            };
            expires_at = expires_at.max(params.expires_at);
            *slot = Some(LinkPair {
                lan: url(lan_base, &params),
                remote: url(remote_base, &params),
            });
        }
        if out.source.is_none() && out.markdown.is_none() {
            return None;
        }
        out.expires_at = expires_at_iso(expires_at);
        Some(out)
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

/// The URL for a link's parameters.
fn url(base: &str, params: &hs_common::docs::LinkParams) -> String {
    let base = base.trim_end_matches('/');
    format!(
        "{base}/docs/v1/{}/{}?{}",
        params.stem,
        params.format.as_str(),
        params.to_query()
    )
}

/// The subject a link is bound to when none is configured.
fn default_subject() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The expiry, as an ISO timestamp.
fn expires_at_iso(unix_secs: u64) -> String {
    let dt = chrono::DateTime::from_timestamp(unix_secs as i64, 0).unwrap_or_else(|| {
        chrono::DateTime::from_timestamp(0, 0).expect("the epoch is a timestamp")
    });
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
        let links = minter().mint("10.1234_abcd").unwrap();
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
    fn a_stem_that_is_not_a_stem_has_no_link() {
        assert!(minter().mint("../etc/passwd").is_none());
        assert!(minter().mint("").is_none());
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
        assert!(m.mint("10.1234_abcd").is_none());
    }
}
