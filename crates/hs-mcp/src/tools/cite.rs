//! `cite`: check a quote against the document it is from, and return what
//! a reader needs to check it too.
//!
//! A quote the document does not contain is refused, with the closest
//! passage named: the tool never returns a citation for words that are not
//! there. A quote that is there comes back as the document has it, with
//! where it is (its lines, and for a PDF whose word boxes are built its
//! page and the rectangles around its words) and signed links that open the
//! document on it.

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use hs_common::catalog::CatalogEntry;
use hs_common::docs::{Highlight, HighlightSpan, LinkFormat};
use hs_common::quote::{self, Placement, QuoteMatch};
use hs_common::storage::Storage;
use hs_common::stores::{DerivedText, DocumentLayout, StoreKind, StoreSpec};
use hs_common::word_boxes::{self, WordBoxes, WordOrigin};
use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;

use super::{catalog_entry, contents_result, object, publication_year, source_extension};
use crate::links::{LinkMinter, LinkPair, MintedLink, ShortLinkPair};
use crate::stem::Stem;
use crate::HomeStillMcp;

/// The source format whose pages the word boxes describe.
const PDF_EXTENSION: &str = "pdf";

/// How long the docs service is given to render one page image.
const PAGE_IMAGE_TIMEOUT_SECS: u64 = 60;

/// The largest page image handed to a client, in bytes. The client keeps
/// the image in its context; 5 MiB is the most Claude accepts for one.
const MAX_PAGE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// The media type of a page image.
const PAGE_IMAGE_MEDIA_TYPE: &str = "image/png";

/// Why `cite` cannot run on a host that mints no links: a citation is the
/// links a reader checks it with.
const LINKS_NOT_CONFIGURED: &str =
    "`cite` returns signed links to the document, and this host cannot mint them: set \
     `docs.lan_base_url` and `docs.remote_base_url` in ~/.home-still/config.yaml, and check the \
     server's log for a link key that could not be read";

/// The client the page images are fetched with.
static PAGE_IMAGE_CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    hs_common::http::http_client(Duration::from_secs(PAGE_IMAGE_TIMEOUT_SECS))
        .map_err(|e| format!("{e:#}"))
});

/// The arguments of `cite`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CiteParams {
    #[schemars(
        description = "The store the passage was found in: the `store` of the hit. Default: `academic`. For `references` the quote is checked against the document the reference list belongs to; for `abstracts` against the abstract in the document's catalog entry."
    )]
    pub store: Option<String>,
    #[schemars(description = "The document's id: `meta.doc_id` of the hit.")]
    pub doc_id: Stem,
    #[schemars(
        description = "The passage, word for word as you would present it: at least 4 words, at most 2000 characters. Markup, line breaks, spacing, hyphenation, case and typography may differ from the document; the words may not."
    )]
    pub quote: String,
    #[schemars(
        description = "A line of the document near the passage (`meta.line_start` of the hit). When the document says the quote more than once, the occurrence nearest this line is the one cited. Not for `abstracts`."
    )]
    pub near_line: Option<u32>,
    #[schemars(
        description = "When true, also return the page as an image with the passage boxed. Only for a passage located on a PDF page (`boxes.status` is `located`); for any other the call fails."
    )]
    #[serde(default)]
    pub image: bool,
    #[schemars(
        description = "Also store a 12-character code that redirects to one of the links, returned as `<base>/s/<code>` under `links.short`: `viewer`, `page`, `source` or `markdown`. Use it when the URL has to be short enough to paste or read out loud. `viewer` and `page` exist only for a located passage. Storing the code writes one small record to the corpus."
    )]
    pub short: Option<String>,
}

/// Which of a citation's links a short link stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortKind {
    Viewer,
    Page,
    Source,
    Markdown,
}

impl ShortKind {
    fn parse(asked: &str) -> Result<Self, String> {
        match asked {
            "viewer" => Ok(Self::Viewer),
            "page" => Ok(Self::Page),
            "source" => Ok(Self::Source),
            "markdown" => Ok(Self::Markdown),
            other => Err(format!(
                "unknown short link {other:?}: use `viewer`, `page`, `source` or `markdown`"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Page => "page",
            Self::Source => "source",
            Self::Markdown => "markdown",
        }
    }
}

/// What `cite` answers for a quote the document contains.
#[derive(Debug, serde::Serialize)]
pub struct Citation {
    /// The store that was asked.
    pub store: String,
    /// The document's id.
    pub doc_id: String,
    /// The document's title, when its catalog entry has one.
    pub title: Option<String>,
    /// The document's authors, in the catalog entry's order.
    pub authors: Vec<String>,
    /// The year the document was published.
    pub year: Option<u16>,
    /// The document's DOI.
    pub doi: Option<String>,
    /// The address the document was captured from.
    pub source_url: Option<String>,
    /// The passage, as the document has it.
    pub quote: CitedQuote,
    /// Where the passage is on the pages of its source.
    pub boxes: CitedBoxes,
    /// The links that open the document.
    pub links: CitationLinks,
}

/// The passage a quote was found as.
#[derive(Debug, serde::Serialize)]
pub struct CitedQuote {
    /// The passage as the document has it.
    pub text: String,
    /// Whether the quote was found as typed, rather than as the same words
    /// in the same order.
    pub exact: bool,
    /// Which text the quote was checked against.
    pub found_in: FoundIn,
    /// The page of the source the passage starts on, when that is known.
    pub page: Option<u32>,
    /// The line of the converted text the passage starts on. `None` for an
    /// abstract, which is not a line of the document.
    pub line_start: Option<u32>,
    /// The line of the converted text the passage ends on.
    pub line_end: Option<u32>,
}

/// The text a quote was checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoundIn {
    /// The document's converted text.
    Document,
    /// The abstract in the document's catalog entry.
    Abstract,
}

/// Where a passage is on the pages of its source.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CitedBoxes {
    /// The passage's words were found on the page.
    Located {
        /// Where the page's words were read from. `ocr` when any page the
        /// passage is on was read by character recognition.
        origin: WordOrigin,
        /// The share of the passage's words found on the page, in order:
        /// the lowest of the pages the passage is on.
        matched_ratio: f32,
        /// The rectangles around the passage, one page at a time.
        pages: Vec<PageRects>,
    },
    /// The source has word boxes and the passage could not be placed in
    /// them.
    NotLocated,
    /// The source is a PDF whose word boxes are not built yet.
    Pending,
    /// The passage has no place on a page: the source is not a PDF, or the
    /// quote is from an abstract.
    NotApplicable,
}

impl CitedBoxes {
    /// The status, as the result spells it.
    fn status(&self) -> &'static str {
        match self {
            Self::Located { .. } => "located",
            Self::NotLocated => "not_located",
            Self::Pending => "pending",
            Self::NotApplicable => "not_applicable",
        }
    }

    /// The page a located passage starts on.
    fn first_page(&self) -> Option<u32> {
        match self {
            Self::Located { pages, .. } => pages.first().map(|rects| rects.page),
            Self::NotLocated | Self::Pending | Self::NotApplicable => None,
        }
    }
}

/// The rectangles around a passage on one page.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct PageRects {
    /// The page, 1-based.
    pub page: u32,
    /// One rectangle per line of the passage, each `[x0, y0, x1, y1]` in
    /// ten-thousandths of the page's width and height from its top left
    /// corner.
    pub rects: Vec<[u16; 4]>,
}

/// The links of a citation.
#[derive(Debug, serde::Serialize)]
pub struct CitationLinks {
    /// The whole PDF in the viewer, opened on the boxed passage. Only for
    /// a located passage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer: Option<LinkPair>,
    /// The passage's page as a picture, the passage boxed. Only for a
    /// located passage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<LinkPair>,
    /// The original document, at the passage's page when that is known.
    pub source: LinkPair,
    /// The converted text, with the passage selected.
    pub markdown: LinkPair,
    /// The address the document was captured from, with the passage
    /// selected.
    pub original_url: Option<String>,
    /// The short link that was asked for.
    pub short: Option<ShortLinkPair>,
    /// When the links stop working.
    pub expires_at: String,
}

/// A citation, and the page image that goes with it when one was asked for.
#[derive(Debug)]
pub struct Cited {
    /// The citation.
    pub citation: Citation,
    /// The passage's page as a PNG, the passage boxed.
    pub page_image: Option<Vec<u8>>,
}

impl Cited {
    /// The result's content: the citation as JSON, then the page image.
    pub fn into_contents(self) -> Result<Vec<Content>, String> {
        let mut content = vec![Content::text(crate::to_json(&self.citation)?)];
        if let Some(png) = self.page_image {
            content.push(Content::image(
                base64::engine::general_purpose::STANDARD.encode(png),
                PAGE_IMAGE_MEDIA_TYPE,
            ));
        }
        Ok(content)
    }
}

/// What the tool answers: the citation, or the refusal as a tool error, the
/// shape every other tool's `Err` takes.
pub fn tool_result(outcome: Result<Cited, String>) -> CallToolResult {
    contents_result(outcome.and_then(Cited::into_contents))
}

/// A quote checked against its document, before any link to it is made.
pub(crate) struct Checked<'a> {
    /// The store whose documents the quote was checked in: the store asked
    /// for, or for a derived store the one it is derived from.
    pub holder: &'a StoreSpec,
    /// The document's catalog entry.
    pub card: Option<CatalogEntry>,
    /// The passage, as the text it was checked against has it.
    pub found: QuoteMatch,
    /// The quote was checked against the abstract in the catalog entry,
    /// not against the converted text.
    pub from_abstract: bool,
    /// Where the passage is on the pages of its source.
    pub boxes: CitedBoxes,
    /// The words a link boxes: there for a located passage and for no
    /// other.
    pub highlight: Option<Highlight>,
    /// The page of the source the passage starts on, when that is known.
    pub page: Option<u32>,
}

impl HomeStillMcp {
    /// Check a quote against its document. `cite` and `synthesis_save`
    /// both stand on this one check.
    pub(crate) async fn check_quote(
        &self,
        asked: &str,
        stem: &str,
        quoted: &str,
        near_line: Option<u32>,
    ) -> Result<Checked<'_>, String> {
        let spec = self.stores.get(asked).map_err(|e| e.to_string())?;
        // A derived store's documents, and so its links, are its parent's.
        let (holder, layout) = self.stores.documents(asked).map_err(|e| e.to_string())?;
        let storage: &dyn Storage = &*self.storage;

        let extension = source_extension(storage, layout, stem)
            .await?
            .ok_or_else(|| {
                format!(
                    "the store {:?} has no document {stem:?}: no source was found under any of \
                     its extensions ({})",
                    holder.name,
                    layout.source_extensions.join(", ")
                )
            })?;
        let card = catalog_entry(storage, holder, layout, stem).await?;

        let from_abstract = matches!(
            spec.kind,
            StoreKind::Derived {
                text: DerivedText::CatalogAbstract,
                ..
            }
        );
        let (found, boxes, highlight) = if from_abstract {
            if near_line.is_some() {
                return Err(format!(
                    "`near_line` is a line of a document, and the store {asked:?} is checked \
                     against the abstract in the catalog entry: leave it out"
                ));
            }
            let abstract_text = card
                .as_ref()
                .and_then(|card| card.abstract_text.as_deref())
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| {
                    format!(
                        "the catalog entry of {stem:?} holds no abstract to check the quote \
                         against; cite the passage from the store {:?} instead",
                        holder.name
                    )
                })?;
            let found = quote::locate(abstract_text, quoted, None)
                .map_err(|e| format!("the abstract of {stem:?}: {e}"))?;
            (found, CitedBoxes::NotApplicable, None)
        } else {
            let markdown = object(storage, &layout.markdown_key(stem))
                .await?
                .ok_or_else(|| {
                    format!(
                        "{stem:?} has not been converted to text yet, so there is nothing to \
                         check the quote against"
                    )
                })?;
            let markdown = String::from_utf8(markdown)
                .map_err(|e| format!("the converted text of {stem:?} is not UTF-8: {e}"))?;
            let found = quote::locate(&markdown, quoted, near_line)
                .map_err(|e| format!("{stem:?}: {e}"))?;
            let (boxes, highlight) = if extension == PDF_EXTENSION {
                place(storage, layout, stem, &markdown, &found).await?
            } else {
                (CitedBoxes::NotApplicable, None)
            };
            (found, boxes, highlight)
        };

        let page = match boxes {
            CitedBoxes::Located { .. } => boxes.first_page(),
            // Without a placement the page is the one the conversion
            // recorded for this part of the text, which it does only when
            // its page count agreed with the source's.
            CitedBoxes::NotLocated | CitedBoxes::Pending => {
                card.as_ref().and_then(|card| recorded_page(card, &found))
            }
            CitedBoxes::NotApplicable => None,
        };
        Ok(Checked {
            holder,
            card,
            found,
            from_abstract,
            boxes,
            highlight,
            page,
        })
    }

    /// Check a quote against its document and build the citation.
    pub(crate) async fn cite_quote(&self, p: CiteParams) -> Result<Cited, String> {
        let minter = self
            .link_minter
            .as_deref()
            .filter(|minter| minter.is_enabled())
            .ok_or(LINKS_NOT_CONFIGURED)?;
        let short = p.short.as_deref().map(ShortKind::parse).transpose()?;
        let asked = p.store.as_deref().unwrap_or(hs_common::stores::ACADEMIC);
        let stem: &str = &p.doc_id;
        let Checked {
            holder,
            card,
            found,
            from_abstract,
            boxes,
            highlight,
            page,
        } = self.check_quote(asked, stem, &p.quote, p.near_line).await?;
        // An abstract's words were not looked for in the converted text, so
        // the markdown link selects nothing.
        let passage = (!from_abstract).then_some(found.text.as_str());
        let sign = |format: LinkFormat, highlight: Option<&Highlight>| {
            minter
                .link(stem, holder.name, format, page, passage, highlight)
                .ok_or_else(|| format!("no {format} link could be signed for {stem:?}"))
        };
        let source = sign(LinkFormat::Source, None)?;
        let markdown = sign(LinkFormat::Markdown, None)?;
        let viewer = highlight
            .as_ref()
            .map(|highlight| sign(LinkFormat::Viewer, Some(highlight)))
            .transpose()?;
        let page_view = highlight
            .as_ref()
            .map(|highlight| sign(LinkFormat::Page, Some(highlight)))
            .transpose()?;

        let short_target = match short {
            None => None,
            Some(kind) => Some(
                match kind {
                    ShortKind::Viewer => viewer.as_ref(),
                    ShortKind::Page => page_view.as_ref(),
                    ShortKind::Source => Some(&source),
                    ShortKind::Markdown => Some(&markdown),
                }
                .ok_or_else(|| {
                    format!(
                        "a short `{}` link opens the passage boxed on its page, and this \
                         passage's boxes are `{}`: ask for `source` or `markdown`",
                        kind.as_str(),
                        boxes.status()
                    )
                })?,
            ),
        };

        let page_image = if p.image {
            let highlight = highlight.as_ref().ok_or_else(|| {
                format!(
                    "the quote is in {stem:?}, but there is no page image of it: its boxes are \
                     `{}`. Call again without `image`",
                    boxes.status()
                )
            })?;
            let link = sign(LinkFormat::PageImage, Some(highlight))?;
            let url = minter.urls(&link).ok_or(LINKS_NOT_CONFIGURED)?.lan;
            Some(fetch_page_image(&url).await?)
        } else {
            None
        };

        // The one write, made last: a call that fails leaves no code behind.
        let short = match short_target {
            Some(link) => Some(self.store_short_link(minter, link).await?),
            None => None,
        };

        let expires_at = viewer
            .iter()
            .chain(&page_view)
            .map(MintedLink::expires_at)
            .fold(source.expires_at().min(markdown.expires_at()), u64::min);
        let urls = |link: &MintedLink| minter.urls(link).ok_or(LINKS_NOT_CONFIGURED);
        let links = CitationLinks {
            viewer: viewer.as_ref().map(urls).transpose()?,
            page: page_view.as_ref().map(urls).transpose()?,
            source: urls(&source)?,
            markdown: urls(&markdown)?,
            original_url: card
                .as_ref()
                .and_then(|card| card.source_url.as_deref())
                .map(|address| original_url(address, passage)),
            short,
            expires_at: crate::links::expires_at_iso(expires_at),
        };

        let card = card.unwrap_or_default();
        Ok(Cited {
            citation: Citation {
                store: asked.to_string(),
                doc_id: stem.to_string(),
                title: card.title,
                authors: card.authors.into_iter().map(|author| author.name).collect(),
                year: card.publication_date.as_deref().and_then(publication_year),
                doi: card.doi,
                source_url: card.source_url,
                quote: CitedQuote {
                    text: found.text,
                    exact: found.exact,
                    found_in: if from_abstract {
                        FoundIn::Abstract
                    } else {
                        FoundIn::Document
                    },
                    page,
                    line_start: (!from_abstract).then_some(found.line_start),
                    line_end: (!from_abstract).then_some(found.line_end),
                },
                boxes,
                links,
            },
            page_image,
        })
    }

    /// Store a short link to `link` and return the URLs its code answers on.
    ///
    /// A short link is an alias for a signed one: the code is a record in
    /// the corpus, answered with a redirect, so minting one is a write. It
    /// fails loudly, because a code the caller is shown and the service
    /// cannot resolve is a 404 the reader would find instead of the
    /// document.
    async fn store_short_link(
        &self,
        minter: &LinkMinter,
        link: &MintedLink,
    ) -> Result<ShortLinkPair, String> {
        let code = loop {
            let candidate = hs_common::docs::new_short_link_code();
            // A collision is 1e-18 unlikely, but overwriting someone else's
            // code would silently repoint their link: retry instead.
            if self
                .short_links
                .get(&candidate)
                .await
                .map_err(|e| format!("the short links could not be read: {e:#}"))?
                .is_none()
            {
                break candidate;
            }
        };
        self.short_links
            .put(&code, &link.short_link())
            .await
            .map_err(|e| {
                format!(
                    "the short link could not be stored, so no code was issued; call again \
                     without `short` for the long links: {e:#}"
                )
            })?;
        minter
            .short_urls(&code)
            .ok_or_else(|| LINKS_NOT_CONFIGURED.to_string())
    }
}

/// Look for a located passage in the word boxes of its PDF: where it is,
/// and for a passage that was placed the words a link boxes.
async fn place(
    storage: &dyn Storage,
    layout: &DocumentLayout,
    stem: &str,
    markdown: &str,
    found: &QuoteMatch,
) -> Result<(CitedBoxes, Option<Highlight>), String> {
    let key = word_boxes::boxes_key(layout, stem);
    let Some(bytes) = object(storage, &key).await? else {
        return Ok((CitedBoxes::Pending, None));
    };
    let boxes = word_boxes::decode(&bytes)
        .map_err(|e| format!("the word boxes at {key} could not be read: {e}"))?;
    let placements = quote::place(markdown, found, &boxes);
    Ok(match located(&boxes, &placements)? {
        Some((boxes, highlight)) => (boxes, Some(highlight)),
        None => (CitedBoxes::NotLocated, None),
    })
}

/// Read a passage's placements out of the word boxes they were made from:
/// the rectangles around it, and its words as a link names them. `None`
/// when the passage was not placed.
fn located(
    boxes: &WordBoxes,
    placements: &[Placement],
) -> Result<Option<(CitedBoxes, Highlight)>, String> {
    let Some(first) = placements.first() else {
        return Ok(None);
    };
    let mut origin = WordOrigin::TextLayer;
    let mut matched_ratio = first.span.matched_ratio;
    let mut spans = Vec::with_capacity(placements.len());
    let mut pages = Vec::with_capacity(placements.len());
    for placement in placements {
        let words = boxes.page(placement.page).ok_or_else(|| {
            format!(
                "the word boxes place the passage on page {}, which they do not hold",
                placement.page
            )
        })?;
        if words.origin == WordOrigin::Ocr {
            origin = WordOrigin::Ocr;
        }
        matched_ratio = matched_ratio.min(placement.span.matched_ratio);
        spans.push(HighlightSpan {
            page: placement.page,
            start: placement.span.start,
            end: placement.span.end,
        });
        pages.push(PageRects {
            page: placement.page,
            rects: quote::line_rects(words, placement.span.start..placement.span.end),
        });
    }
    let highlight = Highlight::new(spans)
        .map_err(|e| format!("the passage's place on the page does not fit in a link: {e}"))?;
    Ok(Some((
        CitedBoxes::Located {
            origin,
            matched_ratio,
            pages,
        },
        highlight,
    )))
}

/// The page the conversion recorded for the part of the text a passage
/// starts in.
fn recorded_page(card: &CatalogEntry, found: &QuoteMatch) -> Option<u32> {
    card.conversion
        .as_ref()?
        .pages
        .iter()
        .find(|page| (page.char_start..page.char_end).contains(&found.char_start))
        .and_then(|page| u32::try_from(page.page).ok())
}

/// The address a document was captured from, pointing at a passage: the
/// address without its own fragment, then the passage's text fragment.
fn original_url(address: &str, passage: Option<&str>) -> String {
    let page = address.split('#').next().unwrap_or(address);
    let fragment = passage
        .map(hs_common::docs::text_fragment)
        .unwrap_or_default();
    format!("{page}{fragment}")
}

/// Fetch a page image from the docs service.
///
/// The URL is a signed link, so no failure message repeats it.
async fn fetch_page_image(url: &str) -> Result<Vec<u8>, String> {
    let client = PAGE_IMAGE_CLIENT.as_ref().map_err(Clone::clone)?;
    let response = client.get(url).send().await.map_err(|e| {
        format!(
            "the docs service did not answer the request for the page image: {}",
            e.without_url()
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "the docs service answered the request for the page image with {status}"
        ));
    }
    let media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if media_type != PAGE_IMAGE_MEDIA_TYPE {
        return Err(format!(
            "the docs service answered the request for the page image with {media_type:?}, not \
             {PAGE_IMAGE_MEDIA_TYPE}"
        ));
    }
    let bytes = response.bytes().await.map_err(|e| {
        format!(
            "the page image could not be read from the docs service: {}",
            e.without_url()
        )
    })?;
    if bytes.len() > MAX_PAGE_IMAGE_BYTES {
        return Err(format!(
            "the page image is {} bytes, more than the {MAX_PAGE_IMAGE_BYTES} a client accepts; \
             call again without `image` and open `links.page`",
            bytes.len()
        ));
    }
    Ok(bytes.to_vec())
}
