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
    /// Delete the short links whose signed link has expired.
    Prune,
}

#[derive(Args, Debug)]
pub struct DocsLinkArgs {
    /// The document's stem.
    pub stem: String,

    /// Link to the converted markdown instead of the original document.
    #[arg(long)]
    pub markdown: bool,

    /// The store the document is in: `academic`, `personal`, `web` or
    /// `syntheses`. A stem can name a document in more than one store.
    /// The store is part of the link's signature, so a link for one
    /// cannot be edited into a link for another.
    #[arg(long, default_value = hs_common::stores::ACADEMIC)]
    pub collection: String,

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

    /// Mint a short link — `<base>/s/<code>` — instead of the full signed URL.
    ///
    /// The code is stored in the corpus and answered with a redirect, so it
    /// needs the storage section of the config. It outlives nothing the full
    /// link does not: it expires with it, and `hs docs prune` deletes it.
    #[arg(long)]
    pub short: bool,
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
        DocsCmd::Prune => prune(reporter).await,
    }
}

/// Delete the short links whose signed link has expired.
async fn prune(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let storage = short_link_storage()?;
    let store = hs_common::docs::ShortLinks::new(storage);
    let deleted = store.prune().await?;
    reporter.status(
        "Pruned",
        &format!("{} expired short link(s)", deleted.len()),
    );
    Ok(())
}

/// The corpus's storage, for the short-link table.
fn short_link_storage() -> Result<Arc<dyn hs_common::storage::Storage>> {
    let file = hs_common::config_file::ConfigFile::load()?;
    let storage_cfg = file
        .section::<hs_common::storage::StorageConfig>("storage")?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "short links live in the corpus's storage, which needs a `storage:` section in \
                 ~/.home-still/config.yaml (backend: local|s3)"
            )
        })?;
    let storage = storage_cfg
        .build()
        .map_err(|e| anyhow::anyhow!("storage config invalid: {e:#}"))?;
    Ok(storage)
}

/// Mint a link.
async fn link(args: DocsLinkArgs, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let file = hs_common::config_file::ConfigFile::load()?;
    let config: Option<DocsConfig> = file.section::<DocsConfig>("docs")?;
    // The store is checked here, where the name was typed: a link to a
    // store that does not exist would be minted, signed and then answered
    // with a 404. A derived store's documents are its parent's.
    let stores = hs_common::stores::Stores::from_config(&file.load_system()?, &file)?;
    let (store, _) = stores.documents(&args.collection)?;

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

    let params = hs_common::docs::mint(
        &key, &args.stem, format, &subject, store.name, args.page, None, ttl,
    )
    .map_err(|e| anyhow::anyhow!("minting the link failed: {e}"))?;

    let lan = link_url(&base_url, &params);
    if !args.short {
        // The URL is this command's result, so it goes to stdout: it is what
        // a caller pipes or captures, and it must survive `--quiet`. The
        // reporter writes progress to stderr and renders differently on a
        // terminal; it keeps the one line a human reads, the expiry.
        println!("{lan}");
        if let Some(remote_base) = remote_base {
            println!("{}", link_url(&remote_base, &params));
        }
        reporter.status(
            "Expires",
            &format!("{} ({} seconds)", expires_at_iso(params.expires_at), ttl),
        );
        return Ok(());
    }

    // A short link is an alias for the signed one above: the code goes in the
    // corpus and the service answers it with a redirect. Nothing else changes,
    // so the signed link is still what a browser ends up on.
    let store = hs_common::docs::ShortLinks::new(short_link_storage()?);
    let code = loop {
        let candidate = hs_common::docs::new_short_link_code();
        // A collision is vanishingly unlikely, but overwriting someone else's
        // code would silently repoint their link.
        if store.get(&candidate).await?.is_none() {
            break candidate;
        }
    };
    let fragment = params
        .page
        .map(hs_common::docs::pdf_page_fragment)
        .unwrap_or_default();
    store
        .put(
            &code,
            &hs_common::docs::ShortLink {
                target: params.to_path(),
                fragment,
                expires_at: params.expires_at,
            },
        )
        .await?;

    println!(
        "{}{}/{code}",
        base_url.trim_end_matches('/'),
        hs_common::docs::SHORT_LINK_PATH
    );
    if let Some(remote_base) = remote_base {
        println!(
            "{}{}/{code}",
            remote_base.trim_end_matches('/'),
            hs_common::docs::SHORT_LINK_PATH
        );
    }
    reporter.status(
        "Expires",
        &format!("{} ({} seconds)", expires_at_iso(params.expires_at), ttl),
    );
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
    format!("{base}{}{fragment}", params.to_path())
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
    let dt = chrono::DateTime::from_timestamp(unix_secs as i64, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_url_carries_the_stem_the_format_and_the_signature() {
        let key = LinkKey::from_bytes(vec![3u8; 32]).unwrap();
        let params = hs_common::docs::mint(
            &key,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            None,
            60,
        )
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
            "academic",
            None,
            None,
            60,
        )
        .unwrap();
        let url = link_url("http://host:7446", &params);
        assert!(url.contains("/markdown?"));
    }
}
