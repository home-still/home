//! `hs docs link` — mint a signed, expiring link to a document.
//!
//! The links the MCP tools hand out and the links this command prints
//! come from the same signer, so a link minted here opens exactly what
//! a link minted for an agent opens.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hs_common::docs::{LinkFormat, LinkKey};
use hs_common::reporter::Reporter;

#[derive(Subcommand, Debug)]
pub enum DocsCmd {
    /// Mint a link to a document.
    Link(DocsLinkArgs),
}

#[derive(Args, Debug)]
pub struct DocsLinkArgs {
    /// The document's stem.
    pub stem: String,

    /// Link to the converted markdown instead of the original document.
    #[arg(long)]
    pub markdown: bool,

    /// The page to open, 1-based. The link opens that page and points
    /// at it.
    #[arg(long)]
    pub page: Option<u32>,

    /// The subject the link is bound to. Defaults to this machine.
    #[arg(long)]
    pub subject: Option<String>,

    /// How long the link works, in seconds. Defaults to the configured
    /// TTL.
    #[arg(long)]
    pub ttl_secs: Option<u64>,

    /// The base URL the link points at. Defaults to the configured LAN
    /// base URL.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Print the remote URL as well as the LAN one.
    #[arg(long)]
    pub remote: bool,
}

/// The default link lifetime, in seconds.
///
/// An hour is long enough to open a document, read a passage and share
/// the link, and short enough that a link pasted into a chat log stops
/// working before the log is forgotten.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Run a `hs docs` command.
pub async fn dispatch(cmd: DocsCmd, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match cmd {
        DocsCmd::Link(args) => link(args, reporter).await,
    }
}

/// Mint a link.
async fn link(args: DocsLinkArgs, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let file = hs_common::config_file::ConfigFile::load()?;
    let config: Option<DocsConfig> = file.section::<DocsConfig>("docs")?;

    let format = if args.markdown {
        LinkFormat::Markdown
    } else {
        LinkFormat::Source
    };

    let base_url = match &args.base_url {
        Some(url) => url.clone(),
        None => config
            .as_ref()
            .and_then(|c| c.lan_base_url.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no base URL for the link. Set `docs.lan_base_url` in \
                     ~/.home-still/config.yaml, or pass --base-url.\n\
                     Example: docs:\n  lan_base_url: http://<host>:7446"
                )
            })?,
    };
    let remote_base = args
        .remote
        .then(|| config.as_ref().and_then(|c| c.remote_base_url.clone()))
        .flatten();

    let ttl = args
        .ttl_secs
        .or(config.as_ref().and_then(|c| c.link_ttl_secs))
        .unwrap_or(DEFAULT_TTL_SECS);

    let subject = args
        .subject
        .or_else(|| config.as_ref().and_then(|c| c.default_subject.clone()))
        .unwrap_or_else(default_subject);

    let key_path = hs_common::docs::default_key_path();
    let key = load_key(&key_path)?;

    let params = hs_common::docs::mint(&key, &args.stem, format, &subject, args.page, ttl)
        .map_err(|e| anyhow::anyhow!("minting the link failed: {e}"))?;

    let lan = link_url(&base_url, &params);
    reporter.finish(&lan);
    if let Some(remote_base) = remote_base {
        let remote = link_url(&remote_base, &params);
        reporter.finish(&remote);
    }
    reporter.finish(&format!(
        "expires at {} ({} seconds)",
        expires_at_iso(params.expires_at),
        ttl
    ));
    Ok(())
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
    /// The subject links are bound to when none is given.
    #[serde(default)]
    pub default_subject: Option<String>,
}

/// The URL for a link's parameters.
///
/// A link that names a page carries the page twice: in the query
/// string, which the server checks (and which the signature covers),
/// and in the fragment, which the *browser* uses to scroll to it. The
/// fragment is never sent to the server, so it cannot be signed — which
/// is why the page is in the query as well.
fn link_url(base: &str, params: &hs_common::docs::LinkParams) -> String {
    let base = base.trim_end_matches('/');
    let fragment = params
        .page
        .map(hs_common::docs::pdf_page_fragment)
        .unwrap_or_default();
    format!(
        "{base}/docs/v1/{}/{}?{}{fragment}",
        params.stem,
        params.format.as_str(),
        params.to_query()
    )
}

/// The subject a link is bound to when none is configured: the name of
/// this machine.
fn default_subject() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// Load the link key, creating it if it is not there.
fn load_key(path: &std::path::Path) -> Result<LinkKey> {
    hs_common::docs::load_or_create_key(path)
        .with_context(|| format!("loading the docs link key at {}", path.display()))
}

/// The expiry, as an ISO timestamp.
fn expires_at_iso(unix_secs: u64) -> String {
    let dt = chrono::DateTime::from_timestamp(unix_secs as i64, 0).unwrap_or_else(|| {
        chrono::DateTime::from_timestamp(0, 0).expect("the epoch is a timestamp")
    });
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_url_carries_the_stem_the_format_and_the_signature() {
        let key = LinkKey::from_bytes(vec![3u8; 32]).unwrap();
        let params =
            hs_common::docs::mint(&key, "10.1234_abcd", LinkFormat::Source, "laptop", None, 60)
                .unwrap();
        let url = link_url("http://host:7446/", &params);
        assert!(url.starts_with("http://host:7446/docs/v1/10.1234_abcd/source?"));
        assert!(url.contains("sub=laptop"));
        assert!(url.contains("fmt=source"));
        assert!(url.contains("sig="));
    }

    #[test]
    fn a_link_url_for_the_markdown_says_so() {
        let key = LinkKey::from_bytes(vec![3u8; 32]).unwrap();
        let params = hs_common::docs::mint(
            &key,
            "10.1234_abcd",
            LinkFormat::Markdown,
            "laptop",
            None,
            60,
        )
        .unwrap();
        let url = link_url("http://host:7446", &params);
        assert!(url.contains("/markdown?"));
    }
}
