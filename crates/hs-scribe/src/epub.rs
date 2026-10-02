//! EPUB reading, in one place: the watcher (EPUB → markdown), the inbox
//! (EPUB → one HTML document) and personal ingest (EPUB → markdown) all go
//! through [`read_chapters`], so one set of limits — `scribe.epub.*` — bounds
//! all three and there is no second chapter loop to forget a cap in.
//!
//! An EPUB is a zip of XML and XHTML, all of it untrusted. The reader is
//! bounded on every axis a hostile archive can scale:
//!
//! * **Bytes.** One [`Budget`] counts every byte inflated (container, OPF,
//!   each chapter) *and* every byte of output produced, and fails at
//!   [`EpubLimits::max_total_bytes`]; each entry is also capped at
//!   [`EpubLimits::max_entry_bytes`] — checked against the declared size
//!   before anything is inflated and against the bytes the decompressor
//!   actually produces (a zip bomb lies about the former). Entries the
//!   reader never needs are never inflated.
//! * **Repetition.** A spine may list one chapter many times (the `epub`
//!   crate re-inflated it for every `<itemref>`: a 6.6 KB archive made 175 MB
//!   of markdown); every chapter is read once, by idref and by the entry it
//!   resolves to.
//! * **Nesting.** The OPF and container are read as a stream of XML events
//!   with a depth bound — no tree is built, so no recursion over one
//!   (the `epub` crate's tree overflowed the stack at 60 000 nested
//!   elements) — and each chapter's markup goes through
//!   [`crate::html::convert_html_to_markdown`]'s own nesting bound.
//! * **Entry count.** At most [`EpubLimits::max_entries`] entries.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Seek};
use xml::attribute::OwnedAttribute;
use xml::reader::{ParserConfig, XmlEvent};

/// Caps on what an EPUB archive may expand to. Exceeding any of them is an
/// error (the document is refused), never a truncated conversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpubLimits {
    /// Most entries (files and directories) the archive may hold. Real
    /// books have hundreds to a few thousand.
    pub max_entries: usize,
    /// Largest any one entry may expand to, in bytes.
    pub max_entry_bytes: u64,
    /// Most bytes one conversion may inflate plus produce, in total.
    pub max_total_bytes: u64,
    /// Bounds on every HTML parse (HTML sources and EPUB chapters alike):
    /// `scribe.epub.html.{max_input_bytes,max_nesting,max_nodes}`.
    pub html: crate::html::HtmlLimits,
}

impl Default for EpubLimits {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_entry_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            html: crate::html::HtmlLimits::default(),
        }
    }
}

impl EpubLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_entries == 0 || self.max_entry_bytes == 0 || self.max_total_bytes == 0 {
            bail!("epub limits must all be at least 1: {self:?}");
        }
        if self.max_entry_bytes > self.max_total_bytes {
            bail!(
                "epub.max_entry_bytes ({}) exceeds epub.max_total_bytes ({})",
                self.max_entry_bytes,
                self.max_total_bytes
            );
        }
        self.html.validate()
    }
}

/// Largest container or package (OPF) document read. Real ones are a few
/// KB to a few hundred KB; this bounds the XML parser's input.
pub const MAX_PACKAGE_XML_BYTES: u64 = 8 * 1024 * 1024;

/// Deepest element nesting accepted in the container and package documents.
/// Real ones nest four or five levels.
pub const MAX_PACKAGE_XML_DEPTH: usize = 64;

/// The one counter every byte of a conversion is charged to: inflated input
/// and produced output alike.
struct Budget {
    limit: u64,
    used: u64,
}

impl Budget {
    fn remaining(&self) -> u64 {
        self.limit - self.used
    }

    fn charge(&mut self, bytes: u64) -> Result<()> {
        match self.used.checked_add(bytes) {
            Some(total) if total <= self.limit => {
                self.used = total;
                Ok(())
            }
            _ => bail!(
                "EPUB conversion exceeds the total limit of {} bytes (bytes inflated plus bytes \
                 produced)",
                self.limit
            ),
        }
    }
}

/// Read the zip entry `name`, charging the budget for what it inflates to.
/// `Ok(None)` when the archive has no such entry (or it is a directory).
fn read_entry<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
    entry_cap: u64,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>> {
    let entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("EPUB entry `{name}` is unreadable")),
    };
    if entry.is_dir() {
        return Ok(None);
    }
    // The declared size is what a zip bomb lies about, but a declaration
    // over the cap needs no inflating to be refused.
    if entry.size() > entry_cap {
        bail!(
            "EPUB entry `{name}` declares {} bytes, over the per-entry limit of {entry_cap}",
            entry.size()
        );
    }
    // One byte past the allowance proves the entry is too big without
    // producing the rest of it.
    let allowance = entry_cap.min(budget.remaining());
    let mut bytes = Vec::with_capacity(entry.size().min(allowance) as usize);
    entry
        .take(allowance + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("EPUB entry `{name}` failed to decompress"))?;
    let produced = bytes.len() as u64;
    if produced > entry_cap {
        bail!("EPUB entry `{name}` expands past the per-entry limit of {entry_cap} bytes");
    }
    budget.charge(produced)?;
    Ok(Some(bytes))
}

/// Stream the elements of an XML document as (depth, local name, attributes)
/// start events, `depth` being 1 for the root. No tree is built, and
/// nesting past [`MAX_PACKAGE_XML_DEPTH`] is an error at the event that
/// crosses it.
fn xml_elements(
    bytes: &[u8],
    mut on_start: impl FnMut(usize, &str, &[OwnedAttribute]) -> bool,
    mut on_end: impl FnMut(usize, &str),
) -> Result<()> {
    let reader = ParserConfig::new()
        .add_entity("nbsp", " ")
        .add_entity("copy", "©")
        .add_entity("reg", "®")
        .replace_unknown_entity_references(true)
        .max_name_length(1024)
        .max_attributes(256)
        .max_attribute_length(64 * 1024)
        .create_reader(bytes);
    let mut depth = 0usize;
    for event in reader {
        match event.context("EPUB package XML is not well-formed")? {
            XmlEvent::StartElement {
                name, attributes, ..
            } => {
                depth += 1;
                if depth > MAX_PACKAGE_XML_DEPTH {
                    bail!("EPUB package XML nests more than {MAX_PACKAGE_XML_DEPTH} elements deep");
                }
                if !on_start(depth, &name.local_name, &attributes) {
                    return Ok(());
                }
            }
            XmlEvent::EndElement { name } => {
                on_end(depth, &name.local_name);
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    Ok(())
}

fn attr<'a>(attributes: &'a [OwnedAttribute], local_name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|a| a.name.local_name == local_name)
        .map(|a| a.value.as_str())
}

/// `META-INF/container.xml` → the path of the package document: the first
/// `rootfile` element's `full-path`.
fn root_file_path(container: &[u8]) -> Result<String> {
    let mut path = None;
    xml_elements(
        container,
        |_, name, attributes| {
            if name == "rootfile" {
                path = attr(attributes, "full-path").map(str::to_string);
                return false;
            }
            true
        },
        |_, _| {},
    )?;
    path.context("EPUB container.xml names no rootfile with a full-path")
}

/// What a package document contributes to reading order.
struct Package {
    /// manifest `id` → `href` (relative to the package document); a later
    /// item with the same id replaces an earlier one.
    manifest: HashMap<String, String>,
    /// The `idref` of every spine child, in order, repeats included.
    spine: Vec<String>,
}

/// The first `manifest`'s items (children with `id`, `href` and
/// `media-type`) and the first `spine`'s children that carry an `idref`.
fn read_package(opf: &[u8]) -> Result<Package> {
    let mut package = Package {
        manifest: HashMap::new(),
        spine: Vec::new(),
    };
    // (depth of the section element, whether it is still open); `None` until seen.
    let mut manifest: Option<(usize, bool)> = None;
    let mut spine: Option<(usize, bool)> = None;
    // The closure pair shares the state, so it lives in cells.
    let state = std::cell::RefCell::new((&mut package, &mut manifest, &mut spine));
    xml_elements(
        opf,
        |depth, name, attributes| {
            let (package, manifest, spine) = &mut *state.borrow_mut();
            match (name, &*manifest, &*spine) {
                ("manifest", None, _) => **manifest = Some((depth, true)),
                ("spine", _, None) => **spine = Some((depth, true)),
                _ => {}
            }
            if let Some((section, true)) = **manifest {
                if depth == section + 1 {
                    if let (Some(id), Some(href), Some(_)) = (
                        attr(attributes, "id"),
                        attr(attributes, "href"),
                        attr(attributes, "media-type"),
                    ) {
                        package.manifest.insert(id.to_string(), href.to_string());
                    }
                }
            }
            if let Some((section, true)) = **spine {
                if depth == section + 1 {
                    if let Some(idref) = attr(attributes, "idref") {
                        package.spine.push(idref.to_string());
                    }
                }
            }
            true
        },
        |depth, name| {
            let (_, manifest, spine) = &mut *state.borrow_mut();
            if let Some((section, true)) = **manifest {
                if depth == section && name == "manifest" {
                    **manifest = Some((section, false));
                }
            }
            if let Some((section, true)) = **spine {
                if depth == section && name == "spine" {
                    **spine = Some((section, false));
                }
            }
        },
    )?;
    if package.manifest.is_empty() {
        bail!("EPUB package has no manifest items");
    }
    if package.spine.is_empty() {
        bail!("EPUB package has an empty spine");
    }
    Ok(package)
}

/// `href` resolved against the directory of the package document, with
/// `.` and `..` segments applied. `None` when it climbs out of the archive.
fn resolve_href(root_dir: &str, href: &str) -> Option<String> {
    let mut parts: Vec<&str> = root_dir.split('/').filter(|p| !p.is_empty()).collect();
    for segment in href.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Read an EPUB once, in reading order, handing each distinct chapter's
/// XHTML to `on_chapter` together with the conversion's [`Budget`] (so a
/// consumer charges what it produces). A chapter the archive does not
/// contain, or whose bytes are not UTF-8, is skipped with a warning; every
/// other fault — bad zip, over a limit, malformed or too deeply nested
/// package XML, a failing consumer — is an error.
fn read_chapters(
    bytes: &[u8],
    limits: &EpubLimits,
    mut on_chapter: impl FnMut(&str, &mut Budget) -> Result<()>,
) -> Result<()> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(bytes)).context("EPUB is not a readable zip archive")?;
    if zip.len() > limits.max_entries {
        bail!(
            "EPUB has {} entries, over the limit of {}",
            zip.len(),
            limits.max_entries
        );
    }
    let mut budget = Budget {
        limit: limits.max_total_bytes,
        used: 0,
    };
    let xml_cap = MAX_PACKAGE_XML_BYTES.min(limits.max_entry_bytes);

    let container = read_entry(&mut zip, "META-INF/container.xml", xml_cap, &mut budget)?
        .context("EPUB has no META-INF/container.xml")?;
    let root_file = root_file_path(&container)?;
    let opf = read_entry(&mut zip, &root_file, xml_cap, &mut budget)?
        .with_context(|| format!("EPUB package document `{root_file}` is missing"))?;
    let package = read_package(&opf)?;
    let root_dir = root_file.rsplit_once('/').map_or("", |(dir, _)| dir);

    let mut seen_idrefs = HashSet::new();
    let mut seen_entries = HashSet::new();
    let mut skipped = 0usize;
    for idref in &package.spine {
        // Each chapter once: by idref, and by the entry it resolves to (many
        // manifest ids may point at one file).
        if !seen_idrefs.insert(idref.as_str()) {
            continue;
        }
        let Some(href) = package.manifest.get(idref) else {
            skipped += 1;
            continue;
        };
        let Some(entry_name) = resolve_href(root_dir, href) else {
            skipped += 1;
            continue;
        };
        if !seen_entries.insert(entry_name.clone()) {
            continue;
        }
        let content = match read_entry(&mut zip, &entry_name, limits.max_entry_bytes, &mut budget)?
        {
            Some(content) => Some(content),
            // Some packagers percent-encode hrefs but store decoded names.
            None => match percent_encoding::percent_decode_str(&entry_name).decode_utf8() {
                Ok(decoded) if decoded != entry_name => {
                    read_entry(&mut zip, &decoded, limits.max_entry_bytes, &mut budget)?
                }
                _ => None,
            },
        };
        let Some(content) = content else {
            skipped += 1;
            continue;
        };
        match std::str::from_utf8(&content) {
            Ok(xhtml) => on_chapter(xhtml, &mut budget)?,
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            "EPUB spine items skipped: no such archive entry, or not UTF-8 text"
        );
    }
    Ok(())
}

/// Convert an EPUB archive's bytes to markdown, chapters in spine order,
/// each through the shared HTML walker so the two paths produce
/// structurally compatible markdown. Everything is bounded by `limits`.
pub fn convert_epub_to_markdown_with(
    bytes: &[u8],
    limits: &EpubLimits,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<String> {
    let mut out = String::new();
    read_chapters(bytes, limits, |xhtml, budget| {
        let md = crate::html::convert_html_to_markdown(xhtml, &limits.html, cancel)?;
        if !md.trim().is_empty() {
            let separator = if out.is_empty() { 0 } else { 2 };
            budget.charge((separator + md.len()) as u64)?;
            if separator > 0 {
                out.push_str("\n\n");
            }
            out.push_str(&md);
        }
        Ok(())
    })?;
    Ok(out)
}

/// Unpack an EPUB's chapters, in spine order, into one HTML string (each
/// chapter's XHTML, blank-line separated) for a caller that hands the HTML
/// to the HTML converter later — the inbox. Everything is bounded by `limits`.
pub fn convert_epub_to_html_with(bytes: &[u8], limits: &EpubLimits) -> Result<String> {
    let mut out = String::new();
    read_chapters(bytes, limits, |xhtml, budget| {
        if !xhtml.trim().is_empty() {
            let separator = if out.is_empty() { 0 } else { 2 };
            budget.charge((separator + xhtml.len()) as u64)?;
            if separator > 0 {
                out.push_str("\n\n");
            }
            out.push_str(xhtml);
        }
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
        buf.into_inner()
    }

    const CONTAINER: &str = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

    /// An OPF with one manifest item per `(id, href)` and a spine of `idrefs`.
    fn opf(items: &[(&str, &str)], idrefs: &[&str]) -> String {
        let mut s = String::from(
            r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:identifier id="id">x</dc:identifier><dc:title>t</dc:title></metadata><manifest>"#,
        );
        for (id, href) in items {
            s.push_str(&format!(
                r#"<item id="{id}" href="{href}" media-type="application/xhtml+xml"/>"#
            ));
        }
        s.push_str("</manifest><spine>");
        for idref in idrefs {
            s.push_str(&format!(r#"<itemref idref="{idref}"/>"#));
        }
        s.push_str("</spine></package>");
        s
    }

    fn chapter(title: &str, paragraphs: usize) -> Vec<u8> {
        let mut s = format!("<html><body><h1>{title}</h1>");
        for i in 0..paragraphs {
            s.push_str(&format!(
                "<p>{title} paragraph {i}: lorem ipsum dolor sit amet consectetur.</p>"
            ));
        }
        s.push_str("</body></html>");
        s.into_bytes()
    }

    /// An EPUB with the given chapters (`(file name under OEBPS/, bytes)`),
    /// manifest ids `c0, c1, ...`, and a spine of the given idrefs.
    fn epub(chapters: &[(&str, Vec<u8>)], idrefs: &[&str]) -> Vec<u8> {
        let items: Vec<(String, &str)> = chapters
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (format!("c{i}"), *name))
            .collect();
        let item_refs: Vec<(&str, &str)> = items.iter().map(|(id, h)| (id.as_str(), *h)).collect();
        let mut entries: Vec<(String, Vec<u8>)> = vec![
            ("mimetype".into(), b"application/epub+zip".to_vec()),
            (
                "META-INF/container.xml".into(),
                CONTAINER.as_bytes().to_vec(),
            ),
            (
                "OEBPS/content.opf".into(),
                opf(&item_refs, idrefs).into_bytes(),
            ),
        ];
        for (name, bytes) in chapters {
            entries.push((format!("OEBPS/{name}"), bytes.clone()));
        }
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        zip_of(&refs)
    }

    fn small() -> EpubLimits {
        EpubLimits {
            max_entries: 8,
            max_entry_bytes: 1 << 20,
            max_total_bytes: 3 << 20,
            html: crate::html::HtmlLimits::default(),
        }
    }

    // ── happy path (none existed) ───────────────────────────────────

    #[test]
    fn a_multi_chapter_epub_converts_in_spine_order() {
        let book = epub(
            &[
                ("one.xhtml", chapter("Alpha", 3)),
                ("two.xhtml", chapter("Bravo", 3)),
                ("three.xhtml", chapter("Charlie", 3)),
            ],
            // Reading order differs from manifest order.
            &["c2", "c0", "c1"],
        );
        let md = convert_epub_to_markdown_with(&book, &EpubLimits::default(), &NEVER).unwrap();
        let at = |needle: &str| {
            md.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {md}"))
        };
        assert!(
            at("# Charlie") < at("# Alpha") && at("# Alpha") < at("# Bravo"),
            "{md}"
        );
        assert!(md.contains("Charlie paragraph 2"), "{md}");
        // The inbox projection carries the same chapters, unconverted.
        let html = convert_epub_to_html_with(&book, &EpubLimits::default()).unwrap();
        assert!(
            html.contains("<h1>Charlie</h1>") && html.contains("<h1>Bravo</h1>"),
            "{html}"
        );
        assert!(html.find("Charlie").unwrap() < html.find("Alpha").unwrap());
    }

    #[test]
    fn hrefs_with_dot_segments_and_percent_encoding_resolve() {
        let mut entries: Vec<(&str, Vec<u8>)> = vec![
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            (
                "OEBPS/content.opf",
                opf(
                    &[
                        ("a", "../Text/a.xhtml"),
                        ("b", "My%20Chapter.xhtml"),
                        ("c", "./c.xhtml"),
                    ],
                    &["a", "b", "c"],
                )
                .into_bytes(),
            ),
        ];
        entries.push(("Text/a.xhtml", chapter("Alpha", 1)));
        entries.push(("OEBPS/My Chapter.xhtml", chapter("Bravo", 1)));
        entries.push(("OEBPS/c.xhtml", chapter("Charlie", 1)));
        let md = convert_epub_to_markdown_with(&zip_of(&entries), &EpubLimits::default(), &NEVER)
            .unwrap();
        for title in ["Alpha", "Bravo", "Charlie"] {
            assert!(
                md.contains(&format!("# {title}")),
                "{title} missing from {md}"
            );
        }
    }

    #[test]
    fn a_spine_item_with_no_entry_is_skipped_not_fatal() {
        let mut entries: Vec<(&str, Vec<u8>)> = vec![
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            (
                "OEBPS/content.opf",
                opf(&[("a", "a.xhtml"), ("gone", "gone.xhtml")], &["gone", "a"]).into_bytes(),
            ),
        ];
        entries.push(("OEBPS/a.xhtml", chapter("Alpha", 1)));
        let md = convert_epub_to_markdown_with(&zip_of(&entries), &EpubLimits::default(), &NEVER)
            .unwrap();
        assert!(md.contains("# Alpha"), "{md}");
    }

    // ── F2: spine repetition ────────────────────────────────────────

    #[test]
    fn a_chapter_listed_in_the_spine_many_times_is_converted_once() {
        // The review's construction: one ~1 MB chapter, 200 itemrefs, and a
        // 6.6 KB archive. The `epub` crate produced 175 MB from it.
        let chapter_bytes = chapter("Bomb", 1).len();
        let mut big = String::from("<html><body>");
        while big.len() < 1_000_000 {
            big.push_str("<p>lorem ipsum dolor sit amet consectetur</p>");
        }
        big.push_str("</body></html>");
        let refs: Vec<&str> = std::iter::repeat_n("c0", 200).collect();
        let book = epub(&[("ch.xhtml", big.into_bytes())], &refs);
        assert!(
            book.len() < 20_000,
            "{} (chapter {chapter_bytes})",
            book.len()
        );
        let md = convert_epub_to_markdown_with(&book, &EpubLimits::default(), &NEVER).unwrap();
        assert!(
            md.len() < 2_000_000,
            "one chapter's worth of markdown, got {}",
            md.len()
        );
        let html = convert_epub_to_html_with(&book, &EpubLimits::default()).unwrap();
        assert!(html.len() < 2_000_000, "got {}", html.len());
    }

    #[test]
    fn many_manifest_ids_for_one_file_are_still_one_chapter() {
        // The idref dedupe alone is not enough: distinct ids may share an href.
        let items: Vec<(String, String)> = (0..300)
            .map(|i| (format!("id{i}"), "ch.xhtml".to_string()))
            .collect();
        let item_refs: Vec<(&str, &str)> = items
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let idrefs: Vec<&str> = items.iter().map(|(a, _)| a.as_str()).collect();
        let mut big = String::from("<html><body>");
        while big.len() < 500_000 {
            big.push_str("<p>lorem ipsum dolor sit amet consectetur</p>");
        }
        big.push_str("</body></html>");
        let book = zip_of(&[
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            ("OEBPS/content.opf", opf(&item_refs, &idrefs).into_bytes()),
            ("OEBPS/ch.xhtml", big.into_bytes()),
        ]);
        let html = convert_epub_to_html_with(&book, &EpubLimits::default()).unwrap();
        assert!(html.len() < 600_000, "got {}", html.len());
    }

    #[test]
    fn inflated_and_produced_bytes_share_one_budget() {
        // Two distinct chapters of ~1.2 MB each, every entry under the
        // per-entry cap. Inflating them costs ~2.4 MB and the HTML
        // projection produces ~2.4 MB more: more than the 3 MiB budget
        // together, though neither side exceeds it alone.
        let mut big = String::from("<html><body>");
        while big.len() < 1_200_000 {
            big.push_str("<p>lorem ipsum dolor sit amet consectetur</p>");
        }
        big.push_str("</body></html>");
        let book = epub(
            &[
                ("a.xhtml", big.clone().into_bytes()),
                ("b.xhtml", big.into_bytes()),
            ],
            &["c0", "c1"],
        );
        let limits = EpubLimits {
            max_entries: 100,
            max_entry_bytes: 2 << 20,
            max_total_bytes: 3 << 20,
            html: crate::html::HtmlLimits::default(),
        };
        let err = convert_epub_to_html_with(&book, &limits).unwrap_err();
        assert!(err.to_string().contains("total limit"), "{err:#}");
    }

    // ── RA-15: the zip layer (the same caps, now on what is actually read) ──

    #[test]
    fn a_highly_compressible_chapter_over_the_cap_is_refused_without_inflating_it() {
        // 64 MiB of zeros deflates to ~64 KiB: the archive itself is tiny.
        let bomb = epub(&[("bomb.xhtml", vec![b' '; 64 << 20])], &["c0"]);
        assert!(
            bomb.len() < 200_000,
            "fixture must be small: {}",
            bomb.len()
        );
        let started = std::time::Instant::now();
        let err = convert_epub_to_markdown_with(&bomb, &small(), &NEVER).unwrap_err();
        assert!(err.to_string().contains("per-entry limit"), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn an_entry_that_inflates_past_its_declared_size_is_stopped_at_the_cap() {
        // The header claims 100 bytes; the deflate stream holds 5 MiB.
        let mut book = epub(&[("lie.xhtml", vec![b' '; 5 << 20])], &["c0"]);
        let mut patched = 0;
        // Local header (PK\x03\x04): uncompressed size at +22; central
        // directory header (PK\x01\x02): at +24.
        for (sig, off) in [(&b"PK\x03\x04"[..], 22), (&b"PK\x01\x02"[..], 24)] {
            let mut at = 0;
            while let Some(rel) = book[at..].windows(4).position(|w| w == sig) {
                let pos = at + rel;
                let name_len_off = if off == 22 { 26 } else { 28 };
                let name_len =
                    u16::from_le_bytes([book[pos + name_len_off], book[pos + name_len_off + 1]])
                        as usize;
                let name_at = pos + if off == 22 { 30 } else { 46 };
                if &book[name_at..name_at + name_len] == b"OEBPS/lie.xhtml" {
                    book[pos + off..pos + off + 4].copy_from_slice(&100u32.to_le_bytes());
                    patched += 1;
                }
                at = pos + 4;
            }
        }
        assert_eq!(patched, 2, "both headers of the chapter must be patched");
        // The declared-size check passes (100 bytes); only the bytes the
        // decompressor actually produces can catch it, and they are stopped
        // at the entry cap, not read to the end.
        let err = convert_epub_to_html_with(&book, &small()).unwrap_err();
        assert!(
            err.to_string().contains("expands past the per-entry limit"),
            "{err:#}"
        );
    }

    #[test]
    fn chapters_that_each_fit_but_not_together_are_refused() {
        let chunk = |c: char| vec![c as u8; 1000 * 1024];
        let book = epub(
            &[
                ("a.xhtml", chunk('a')),
                ("b.xhtml", chunk('b')),
                ("c.xhtml", chunk('c')),
            ],
            &["c0", "c1", "c2"],
        );
        let err = convert_epub_to_html_with(&book, &small()).unwrap_err();
        assert!(err.to_string().contains("total limit"), "{err:#}");
    }

    #[test]
    fn too_many_entries_are_refused_before_any_is_read() {
        let entries: Vec<(String, Vec<u8>)> =
            (0..9).map(|i| (format!("e{i}.txt"), vec![b'x'])).collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let err = convert_epub_to_markdown_with(&zip_of(&refs), &small(), &NEVER).unwrap_err();
        assert!(err.to_string().contains("9 entries"), "{err:#}");
    }

    #[test]
    fn entries_the_reader_never_needs_are_never_inflated() {
        // A 64 MiB cover image over the per-entry cap is not a reason to
        // refuse a book whose chapters are fine: nothing reads it.
        let book = epub(
            &[
                ("a.xhtml", chapter("Alpha", 2)),
                ("cover.bin", vec![0u8; 64 << 20]),
            ],
            &["c0"],
        );
        let md = convert_epub_to_markdown_with(&book, &small(), &NEVER).unwrap();
        assert!(md.contains("# Alpha"));
    }

    #[test]
    fn a_non_zip_is_an_error_not_a_panic() {
        assert!(convert_epub_to_markdown_with(b"not a zip at all", &small(), &NEVER).is_err());
        assert!(convert_epub_to_markdown_with(b"", &small(), &NEVER).is_err());
        // A zip that is not an EPUB.
        let z = zip_of(&[("readme.txt", b"hello".to_vec())]);
        assert!(convert_epub_to_markdown_with(&z, &small(), &NEVER).is_err());
    }

    #[test]
    fn limits_must_be_positive_and_consistent() {
        EpubLimits::default().validate().unwrap();
        for bad in [
            EpubLimits {
                max_entries: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_entry_bytes: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_total_bytes: 0,
                ..EpubLimits::default()
            },
            EpubLimits {
                max_entry_bytes: 10,
                max_total_bytes: 5,
                ..EpubLimits::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    // ── F3: hostile nesting, in a child process on a 2 MiB stack ────

    const CHILD_ENV: &str = "HS_SCRIBE_CHILD_EPUBS";

    /// Entry point of the child process: convert each EPUB file listed in
    /// the environment on a thread with tokio's default 2 MiB blocking
    /// stack and report `ok <bytes>` or `err <message>`.
    #[test]
    fn child_entry() {
        let Ok(files) = std::env::var(CHILD_ENV) else {
            return;
        };
        for file in files.split('\n') {
            let bytes = std::fs::read(file).unwrap();
            let outcome = std::thread::Builder::new()
                .stack_size(2 << 20)
                .spawn(move || {
                    convert_epub_to_markdown_with(&bytes, &EpubLimits::default(), &NEVER)
                })
                .unwrap()
                .join()
                .unwrap();
            match outcome {
                Ok(md) => println!("RESULT ok {}", md.len()),
                Err(e) => println!("RESULT err {}", format!("{e:#}").replace('\n', " ")),
            }
        }
        std::process::exit(0);
    }

    /// Convert each case in a child process; the child must exit normally and
    /// in bounded time. Returns what it reported per case.
    fn run_epub_cases(dir: &std::path::Path, cases: &[Vec<u8>]) -> Vec<String> {
        let mut paths = Vec::new();
        for (i, bytes) in cases.iter().enumerate() {
            let path = dir.join(format!("case-{i}.epub"));
            std::fs::write(&path, bytes).unwrap();
            paths.push(path.display().to_string());
        }
        let started = std::time::Instant::now();
        let child =
            crate::child_proc::run("epub::tests::child_entry", CHILD_ENV, &paths.join("\n"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "took {:?}",
            started.elapsed()
        );
        let results = child.results();
        assert_eq!(results.len(), cases.len(), "{}", child.stdout);
        results
    }

    fn nested_xml(depth: usize) -> String {
        format!("{}{}", "<x>".repeat(depth), "</x>".repeat(depth))
    }

    #[test]
    fn deeply_nested_package_xml_is_refused_in_bounded_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut cases = Vec::new();
        // The review's EPUB: 60 000 nested elements in the OPF's metadata
        // (a 1.6 KB archive once compressed) aborted the old reader.
        let hostile_opf = opf(&[("a", "a.xhtml")], &["a"])
            .replace("</metadata>", &format!("{}</metadata>", nested_xml(60_000)));
        cases.push(zip_of(&[
            ("META-INF/container.xml", CONTAINER.as_bytes().to_vec()),
            ("OEBPS/content.opf", hostile_opf.into_bytes()),
            ("OEBPS/a.xhtml", chapter("A", 1)),
        ]));
        // The same in the container, and a million deep.
        let hostile_container = CONTAINER.replace(
            "<rootfiles>",
            &format!("<rootfiles>{}", nested_xml(1_000_000)),
        );
        cases.push(zip_of(&[
            ("META-INF/container.xml", hostile_container.into_bytes()),
            (
                "OEBPS/content.opf",
                opf(&[("a", "a.xhtml")], &["a"]).into_bytes(),
            ),
            ("OEBPS/a.xhtml", chapter("A", 1)),
        ]));
        let outcomes = run_epub_cases(dir.path(), &cases);
        for (i, outcome) in outcomes.iter().enumerate() {
            assert!(outcome.starts_with("err "), "case {i}: {outcome}");
            assert!(outcome.contains("nests more than"), "case {i}: {outcome}");
        }
    }

    #[test]
    fn a_chapter_nested_past_the_html_bound_fails_the_book_in_bounded_time() {
        let dir = tempfile::tempdir().unwrap();
        let deep = format!(
            "<html><body>{}x{}</body></html>",
            "<div>".repeat(20_000),
            "</div>".repeat(20_000)
        );
        let book = epub(&[("a.xhtml", deep.into_bytes())], &["c0"]);
        let outcomes = run_epub_cases(dir.path(), &[book]);
        assert!(outcomes[0].starts_with("err "), "{}", outcomes[0]);
        assert!(outcomes[0].contains("nest"), "{}", outcomes[0]);
    }
}
