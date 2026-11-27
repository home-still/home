//! `capture`: keep a web page the caller has read, in the `web` store.
//!
//! The caller fetched the page; this stores what it hands over under an id
//! made from the page's address, writes the catalog entry and announces
//! the document, so the same pipeline that converts and indexes a paper
//! converts and indexes the page. Capturing an address again replaces the
//! document that was captured before.

use hs_common::catalog::CatalogEntry;
use hs_common::storage::Storage;
use hs_common::stores::DocumentLayout;
use rmcp::schemars;

use super::object_exists;
use crate::HomeStillMcp;

/// The largest page `capture` stores: 2 MiB of text is several books, and
/// more than that is a download, not a page somebody read.
pub const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;
/// The hexadecimal characters of the address's SHA-256 that make a
/// captured page's id: 128 bits, so two addresses do not share one.
pub const WEB_STEM_HEX_CHARS: usize = 32;
/// Who the announcement says stored the page.
const EVENT_SOURCE: &str = "mcp:capture";

/// What the content of a captured page is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureContentType {
    /// The page's HTML, as fetched.
    Html,
    /// The page as markdown.
    Markdown,
    /// The page as plain text.
    Text,
}

impl CaptureContentType {
    /// The extension a page of this type is stored under.
    fn extension(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Markdown => "md",
            Self::Text => "txt",
        }
    }
}

/// The arguments of `capture`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CaptureParams {
    #[schemars(
        description = "The page's address, `http://` or `https://`. Its fragment (`#...`) is not part of the page's identity."
    )]
    pub url: String,
    #[schemars(
        description = "The page's content, at most 2 MiB: the whole page, not a summary of it, because citations are checked against this text."
    )]
    pub content: String,
    #[schemars(description = "What `content` is written in: `html`, `markdown` or `text`.")]
    pub content_type: CaptureContentType,
    #[schemars(description = "The page's title, as `documents` and citations show it.")]
    pub title: Option<String>,
}

/// What `capture` answers.
#[derive(Debug, serde::Serialize)]
pub struct Captured {
    /// The store the page is in.
    pub store: &'static str,
    /// The page's id: what `status`, `read` and `cite` take.
    pub doc_id: String,
    /// The address the page is filed under: the one given, without its
    /// fragment.
    pub source_url: String,
    /// The bytes stored.
    pub size_bytes: u64,
    /// A page captured from this address before was replaced.
    pub replaced: bool,
    /// What happens next.
    pub next: &'static str,
}

/// The address a page is filed under: `url` as an http(s) address without
/// its fragment.
fn page_address(url: &str) -> Result<String, String> {
    let mut address = reqwest::Url::parse(url.trim())
        .map_err(|e| format!("`url` {url:?} is not an address: {e}"))?;
    if !matches!(address.scheme(), "http" | "https") {
        return Err(format!(
            "`url` must be an http:// or https:// address; {url:?} is {}:",
            address.scheme()
        ));
    }
    address.set_fragment(None);
    Ok(address.into())
}

/// The id of the page at `address`.
fn web_stem(address: &str) -> String {
    let mut digest = hs_common::word_boxes::source_sha256(address.as_bytes());
    digest.truncate(WEB_STEM_HEX_CHARS);
    digest
}

/// Check what a caller handed over before anything is stored.
fn check_content(content: &str, content_type: CaptureContentType) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("`content` is empty: there is no page to capture".to_string());
    }
    if content.len() > MAX_CAPTURE_BYTES {
        return Err(format!(
            "`content` is {} bytes and a captured page is at most {MAX_CAPTURE_BYTES}: \
             nothing was stored",
            content.len()
        ));
    }
    if content_type == CaptureContentType::Html && hs_common::html::is_paywall_html(content) {
        return Err(
            "`content` is a paywall or sign-in page, not the page itself: nothing was stored"
                .to_string(),
        );
    }
    Ok(())
}

/// Remove what an earlier capture of the same address left: its source
/// under any extension and its converted text. The watcher converts a
/// source only when it has no converted text, so the old text must go for
/// the new page to be read. Returns whether there was anything.
async fn remove_previous(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    stem: &str,
) -> Result<bool, String> {
    let mut keys: Vec<String> = layout
        .source_extensions
        .iter()
        .map(|extension| layout.source_key(stem, extension))
        .collect();
    keys.push(layout.markdown_key(stem));
    let mut replaced = false;
    for key in keys {
        if object_exists(storage, &key).await? {
            storage
                .delete(&key)
                .await
                .map_err(|e| format!("removing the earlier capture {key} failed: {e:#}"))?;
            replaced = true;
        }
    }
    Ok(replaced)
}

impl HomeStillMcp {
    /// Store a page in the `web` store and announce it.
    pub(crate) async fn capture_page(&self, p: CaptureParams) -> Result<String, String> {
        let address = page_address(&p.url)?;
        check_content(&p.content, p.content_type)?;
        let title = p
            .title
            .map(|title| title.trim().to_string())
            .filter(|title| !title.is_empty());
        self.require_event_bus("capture").await?;

        let store = hs_common::stores::WEB;
        let (_, layout) = self.stores.documents(store).map_err(|e| e.to_string())?;
        let storage: &dyn Storage = &*self.storage;
        let stem = web_stem(&address);

        let replaced = remove_previous(storage, layout, &stem).await?;
        // A fresh entry: the stamps of an earlier capture describe text
        // that is no longer there.
        let entry = CatalogEntry {
            title,
            source_url: Some(address.clone()),
            ..CatalogEntry::default()
        };
        let size_bytes = self
            .store_document(
                layout,
                &stem,
                p.content_type.extension(),
                p.content.into_bytes(),
                entry,
                EVENT_SOURCE,
            )
            .await?;

        crate::to_json(&Captured {
            store,
            doc_id: stem,
            source_url: address,
            size_bytes,
            replaced,
            next: "The page is queued for conversion and indexing. `status` with this `doc_id` \
                   and store `web` says when it is converted and indexed; then myelin's `recall` \
                   finds it and `cite` checks quotes against it.",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fragment_is_not_part_of_a_pages_identity() {
        let plain = page_address("https://example.org/a?b=1").expect("an address");
        let anchored = page_address("https://example.org/a?b=1#section-2").expect("an address");
        assert_eq!(plain, anchored);
        assert_eq!(web_stem(&plain), web_stem(&anchored));
        assert_eq!(web_stem(&plain).len(), WEB_STEM_HEX_CHARS);
        assert_ne!(
            web_stem(&plain),
            web_stem(&page_address("https://example.org/a?b=2").expect("an address"))
        );
    }

    #[test]
    fn only_http_addresses_are_captured() {
        for refused in [
            "file:///etc/passwd",
            "ftp://example.org/x",
            "not an address",
        ] {
            assert!(page_address(refused).is_err(), "{refused}");
        }
        assert!(page_address("http://example.org/").is_ok());
    }

    #[test]
    fn empty_and_oversized_content_is_refused() {
        assert!(check_content("  \n", CaptureContentType::Text).is_err());
        let big = "a".repeat(MAX_CAPTURE_BYTES + 1);
        let refused = check_content(&big, CaptureContentType::Markdown).unwrap_err();
        assert!(refused.contains("nothing was stored"), "{refused}");
        assert!(check_content(&"a".repeat(MAX_CAPTURE_BYTES), CaptureContentType::Text).is_ok());
    }
}
