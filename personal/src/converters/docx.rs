//! DOCX → markdown using the pure-Rust `docx-rs` crate. We walk the document
//! body once, emitting one markdown block per paragraph or table. Headings
//! (`Heading 1` through `Heading 6` styles) become markdown `#` levels and
//! tables become markdown pipe tables (first row is the header). Lists and
//! embedded media are not deeply supported — if the user has a high-fidelity
//! requirement we'll know from real fixtures and revisit (pandoc shell-out).
//!
//! A DOCX is a zip, so it is held to the same `scribe.epub.*` expansion
//! limits as an EPUB ([`EpubLimits`]) before the parser sees it.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use hs_scribe::epub::EpubLimits;

use crate::error::{PersonalError, Result};

fn err(source: anyhow::Error) -> PersonalError {
    PersonalError::Converter {
        format: "docx",
        source,
    }
}

pub fn convert(bytes: Vec<u8>) -> Result<String> {
    let limits = hs_scribe::config::ScribeConfig::load()
        .map_err(|e| err(anyhow::anyhow!("cannot read the scribe.epub limits: {e}")))?
        .epub;
    convert_with(&limits, &bytes)
}

/// [`convert`] under explicit limits.
pub fn convert_with(limits: &EpubLimits, bytes: &[u8]) -> Result<String> {
    check_expansion(limits, bytes).map_err(err)?;

    let docx = docx_rs::read_docx(bytes)
        .map_err(|e| err(anyhow::anyhow!("docx-rs parse failed: {e:?}")))?;

    let links = &hyperlink_targets(bytes).map_err(err)?;
    let mut out = String::new();
    for child in &docx.document.children {
        match child {
            docx_rs::DocumentChild::Paragraph(p) => {
                push_paragraph(&mut out, p, links).map_err(err)?
            }
            docx_rs::DocumentChild::Table(t) => push_table(&mut out, t, links).map_err(err)?,
            docx_rs::DocumentChild::StructuredDataTag(sdt) => {
                push_sdt(&mut out, sdt, links).map_err(err)?
            }
            docx_rs::DocumentChild::TableOfContents(toc) => {
                push_toc(&mut out, toc, links).map_err(err)?
            }
            docx_rs::DocumentChild::BookmarkStart(_)
            | docx_rs::DocumentChild::BookmarkEnd(_)
            | docx_rs::DocumentChild::CommentStart(_)
            | docx_rs::DocumentChild::CommentEnd(_) => {}
            // docx-rs's reader never produces a section block and keeps its
            // children private; one here cannot be read, so refuse it.
            docx_rs::DocumentChild::Section(_) => {
                return Err(err(anyhow::anyhow!(
                    "DOCX body holds a section block, which cannot be converted"
                )))
            }
        }
    }

    if out.trim().is_empty() {
        return Err(err(anyhow::anyhow!(
            "document contained no extractable text"
        )));
    }

    Ok(out)
}

/// Refuse a zip that is over the entry-count, per-entry or total-inflated
/// caps. Declared sizes are checked first (a bomb lies about them), then the
/// bytes each entry actually inflates to are counted without being kept.
fn check_expansion(limits: &EpubLimits, bytes: &[u8]) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("DOCX is not a readable zip archive: {e}"))?;
    if zip.len() > limits.max_entries {
        anyhow::bail!(
            "DOCX has {} entries, over the limit of {}",
            zip.len(),
            limits.max_entries
        );
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip
            .by_index(i)
            .map_err(|e| anyhow::anyhow!("DOCX entry {i} is unreadable: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        if entry.size() > limits.max_entry_bytes {
            anyhow::bail!(
                "DOCX entry `{name}` declares {} bytes, over the per-entry limit of {}",
                entry.size(),
                limits.max_entry_bytes
            );
        }
        let allowance = limits
            .max_entry_bytes
            .min(limits.max_total_bytes.saturating_sub(total));
        let produced = std::io::copy(
            &mut entry.take(allowance.saturating_add(1)),
            &mut std::io::sink(),
        )
        .map_err(|e| anyhow::anyhow!("DOCX entry `{name}` failed to decompress: {e}"))?;
        if produced > limits.max_entry_bytes {
            anyhow::bail!(
                "DOCX entry `{name}` expands past the per-entry limit of {} bytes",
                limits.max_entry_bytes
            );
        }
        total += produced;
        if total > limits.max_total_bytes {
            anyhow::bail!(
                "DOCX expands past the total limit of {} bytes",
                limits.max_total_bytes
            );
        }
    }
    Ok(())
}

fn push_table(out: &mut String, t: &docx_rs::Table, links: Links) -> anyhow::Result<()> {
    let md = table_markdown(t, links)?;
    if !md.is_empty() {
        out.push_str(&md);
        out.push('\n');
    }
    Ok(())
}

/// A content control's text is real content (forms, templates): render its
/// paragraphs, tables, loose runs and nested controls in reading order.
fn push_sdt(
    out: &mut String,
    sdt: &docx_rs::StructuredDataTag,
    links: Links,
) -> anyhow::Result<()> {
    for child in &sdt.children {
        match child {
            docx_rs::StructuredDataTagChild::Paragraph(p) => push_paragraph(out, p, links)?,
            docx_rs::StructuredDataTagChild::Table(t) => push_table(out, t, links)?,
            docx_rs::StructuredDataTagChild::StructuredDataTag(inner) => {
                push_sdt(out, inner, links)?
            }
            docx_rs::StructuredDataTagChild::Run(r) => {
                let mut text = String::new();
                run_text(r, &mut text);
                if !text.trim().is_empty() {
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
            }
            docx_rs::StructuredDataTagChild::BookmarkStart(_)
            | docx_rs::StructuredDataTagChild::BookmarkEnd(_)
            | docx_rs::StructuredDataTagChild::CommentStart(_)
            | docx_rs::StructuredDataTagChild::CommentEnd(_) => {}
        }
    }
    Ok(())
}

/// Table-of-contents entries are plain lines.
fn push_toc(out: &mut String, toc: &docx_rs::TableOfContents, links: Links) -> anyhow::Result<()> {
    let push_content = |out: &mut String, c: &docx_rs::TocContent| match c {
        docx_rs::TocContent::Paragraph(p) => push_paragraph(out, p, links),
        docx_rs::TocContent::Table(t) => push_table(out, t, links),
    };
    for c in &toc.before_contents {
        push_content(out, c)?;
    }
    for item in &toc.items {
        if !item.text.trim().is_empty() {
            out.push_str(&item.text);
            out.push_str("\n\n");
        }
    }
    for c in &toc.after_contents {
        push_content(out, c)?;
    }
    Ok(())
}

fn push_paragraph(out: &mut String, p: &docx_rs::Paragraph, links: Links) -> anyhow::Result<()> {
    let level = heading_level(p);
    let text = paragraph_text(p, links)?;
    if text.trim().is_empty() {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        return Ok(());
    }
    if let Some(n) = level {
        let hashes = "#".repeat(n.clamp(1, 6) as usize);
        out.push_str(&format!("{hashes} {text}\n\n"));
    } else {
        out.push_str(&text);
        out.push_str("\n\n");
    }
    Ok(())
}

/// Render a table as a markdown pipe table. Cell paragraphs are joined with a
/// space and `|` is escaped. A table nested in a cell cannot be represented in
/// a pipe table, so it is an error rather than a silent drop.
fn table_markdown(t: &docx_rs::Table, links: Links) -> anyhow::Result<String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for docx_rs::TableChild::TableRow(row) in &t.rows {
        let mut cells = Vec::new();
        for docx_rs::TableRowChild::TableCell(cell) in &row.cells {
            let mut parts = Vec::new();
            for content in &cell.children {
                match content {
                    docx_rs::TableCellContent::Paragraph(p) => {
                        let text = paragraph_text(p, links)?;
                        let text = text.trim();
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                    docx_rs::TableCellContent::Table(_) => {
                        anyhow::bail!("DOCX contains a table nested inside a table cell")
                    }
                    _ => {}
                }
            }
            cells.push(
                parts
                    .join(" ")
                    .replace('\\', "\\\\")
                    .replace('|', "\\|")
                    .replace(['\r', '\n'], " "),
            );
        }
        rows.push(cells);
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return Ok(String::new());
    }
    let mut md = String::new();
    for (i, row) in rows.iter().enumerate() {
        md.push('|');
        for c in 0..width {
            md.push(' ');
            md.push_str(row.get(c).map_or("", String::as_str));
            md.push_str(" |");
        }
        md.push('\n');
        if i == 0 {
            md.push('|');
            md.push_str(&" --- |".repeat(width));
            md.push('\n');
        }
    }
    Ok(md)
}

fn heading_level(p: &docx_rs::Paragraph) -> Option<u8> {
    let style_id = p.property.style.as_ref().map(|s| s.val.as_str())?;
    // docx style IDs for headings are conventionally "Heading1".."Heading9"
    // (Word) or "heading 1".."heading 9" (LibreOffice). Be liberal in what we
    // accept; produce one level per match.
    let lower = style_id.to_ascii_lowercase().replace(' ', "");
    let digits: String = lower
        .strip_prefix("heading")
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u8>().ok().filter(|n| (1..=9).contains(n))
}

fn run_text(r: &docx_rs::Run, s: &mut String) {
    for rc in &r.children {
        if let docx_rs::RunChild::Text(t) = rc {
            s.push_str(&t.text);
        }
    }
}

/// External hyperlink targets by relationship id.
type Links<'a> = &'a HashMap<String, String>;

/// Read `word/_rels/document.xml.rels` and collect external hyperlink
/// targets. A package without that part has no hyperlinks.
fn hyperlink_targets(bytes: &[u8]) -> anyhow::Result<HashMap<String, String>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("DOCX is not a readable zip archive: {e}"))?;
    let mut xml_bytes = Vec::new();
    match zip.by_name("word/_rels/document.xml.rels") {
        Ok(mut entry) => {
            entry
                .read_to_end(&mut xml_bytes)
                .map_err(|e| anyhow::anyhow!("DOCX relationships failed to decompress: {e}"))?;
        }
        Err(zip::result::ZipError::FileNotFound) => return Ok(HashMap::new()),
        Err(e) => anyhow::bail!("DOCX relationships are unreadable: {e}"),
    }
    let mut links = HashMap::new();
    for event in xml::reader::EventReader::new(&xml_bytes[..]) {
        let event =
            event.map_err(|e| anyhow::anyhow!("DOCX relationships are not well-formed: {e}"))?;
        if let xml::reader::XmlEvent::StartElement {
            name, attributes, ..
        } = event
        {
            if name.local_name != "Relationship" {
                continue;
            }
            let attr = |key: &str| {
                attributes
                    .iter()
                    .find(|a| a.name.local_name == key)
                    .map(|a| a.value.clone())
            };
            let is_link = attr("Type").is_some_and(|t| t.ends_with("/hyperlink"));
            if let (true, Some(id), Some(target)) = (is_link, attr("Id"), attr("Target")) {
                links.insert(id, target);
            }
        }
    }
    Ok(links)
}

fn paragraph_text(p: &docx_rs::Paragraph, links: Links) -> anyhow::Result<String> {
    let mut s = String::new();
    paragraph_children_text(&p.children, links, &mut s)?;
    Ok(s)
}

/// Inline text of paragraph children. Hyperlinks with an external target
/// become `[text](url)`; tracked insertions are kept; tracked deletions and
/// markers with no text of their own are skipped. An inline content control
/// holding a table cannot be flattened into a line, so it is an error.
fn paragraph_children_text(
    children: &[docx_rs::ParagraphChild],
    links: Links,
    s: &mut String,
) -> anyhow::Result<()> {
    use docx_rs::ParagraphChild as C;
    for child in children {
        match child {
            C::Run(r) => run_text(r, s),
            C::Insert(ins) => {
                for c in &ins.children {
                    match c {
                        docx_rs::InsertChild::Run(r) => run_text(r, s),
                        docx_rs::InsertChild::Delete(_)
                        | docx_rs::InsertChild::CommentStart(_)
                        | docx_rs::InsertChild::CommentEnd(_) => {}
                    }
                }
            }
            C::Hyperlink(h) => {
                let mut text = String::new();
                paragraph_children_text(&h.children, links, &mut text)?;
                // docx-rs's reader drops link targets, so they come from the
                // package relationships keyed by the link's rid; anchors are
                // internal jumps with no URL.
                let target = match &h.link {
                    docx_rs::HyperlinkData::External { rid, .. } => links.get(rid),
                    docx_rs::HyperlinkData::Anchor { .. } => None,
                };
                match target {
                    Some(url) if !text.is_empty() => s.push_str(&format!("[{text}]({url})")),
                    _ => s.push_str(&text),
                }
            }
            C::StructuredDataTag(sdt) => sdt_inline_text(sdt, links, s)?,
            C::Delete(_)
            | C::BookmarkStart(_)
            | C::BookmarkEnd(_)
            | C::CommentStart(_)
            | C::CommentEnd(_)
            | C::PageNum(_)
            | C::NumPages(_) => {}
        }
    }
    Ok(())
}

fn sdt_inline_text(
    sdt: &docx_rs::StructuredDataTag,
    links: Links,
    s: &mut String,
) -> anyhow::Result<()> {
    use docx_rs::StructuredDataTagChild as C;
    for child in &sdt.children {
        match child {
            C::Run(r) => run_text(r, s),
            C::Paragraph(p) => paragraph_children_text(&p.children, links, s)?,
            C::StructuredDataTag(inner) => sdt_inline_text(inner, links, s)?,
            C::Table(_) => anyhow::bail!("DOCX has a table inside an inline content control"),
            C::BookmarkStart(_) | C::BookmarkEnd(_) | C::CommentStart(_) | C::CommentEnd(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use docx_rs::*;

    fn build_fixture() -> Vec<u8> {
        let docx = Docx::new()
            .add_paragraph(
                Paragraph::new()
                    .style("Heading1")
                    .add_run(Run::new().add_text("Lab Results 2024")),
            )
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Cholesterol: 180 mg/dL")))
            .add_paragraph(
                Paragraph::new()
                    .style("Heading2")
                    .add_run(Run::new().add_text("Notes")),
            )
            .add_paragraph(
                Paragraph::new()
                    .add_run(Run::new().add_text("Follow-up scheduled for next month.")),
            );
        let mut cur = std::io::Cursor::new(Vec::new());
        docx.build().pack(&mut cur).expect("docx pack");
        cur.into_inner()
    }

    #[test]
    fn docx_roundtrip_preserves_headings_and_paragraphs() {
        let bytes = build_fixture();
        let md = convert(bytes).expect("convert");
        assert!(md.contains("# Lab Results 2024"), "missing H1: {md}");
        assert!(md.contains("## Notes"), "missing H2: {md}");
        assert!(
            md.contains("Cholesterol: 180 mg/dL"),
            "missing body line: {md}"
        );
        assert!(
            md.contains("Follow-up scheduled for next month."),
            "missing body line: {md}"
        );
    }

    #[test]
    fn docx_with_no_text_is_a_hard_error() {
        let docx = Docx::new();
        let mut cur = std::io::Cursor::new(Vec::new());
        docx.build().pack(&mut cur).unwrap();
        let err = convert(cur.into_inner()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("docx"), "error should mention format: {msg}");
    }

    fn pack(docx: Docx) -> Vec<u8> {
        let mut cur = std::io::Cursor::new(Vec::new());
        docx.build().pack(&mut cur).expect("docx pack");
        cur.into_inner()
    }

    fn cell(text: &str) -> TableCell {
        TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text(text)))
    }

    #[test]
    fn tables_become_markdown_tables_in_reading_order() {
        let docx = Docx::new()
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Before")))
            .add_table(Table::new(vec![
                TableRow::new(vec![cell("Test"), cell("Value")]),
                TableRow::new(vec![cell("LDL | calc"), cell("120")]),
            ]))
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("After")));
        let md = convert_with(&EpubLimits::default(), &pack(docx)).unwrap();
        let table = "| Test | Value |\n| --- | --- |\n| LDL \\| calc | 120 |\n";
        assert!(md.contains(table), "table missing: {md}");
        let (b, t, a) = (
            md.find("Before").unwrap(),
            md.find("| Test").unwrap(),
            md.find("After").unwrap(),
        );
        assert!(b < t && t < a, "wrong order: {md}");
    }

    #[test]
    fn a_table_only_document_is_not_empty() {
        let docx = Docx::new().add_table(Table::new(vec![TableRow::new(vec![cell("only")])]));
        let md = convert_with(&EpubLimits::default(), &pack(docx)).unwrap();
        assert!(md.contains("| only |"), "{md}");
    }

    #[test]
    fn a_nested_table_is_an_error_not_a_drop() {
        let inner = Table::new(vec![TableRow::new(vec![cell("inner")])]);
        let outer = Table::new(vec![TableRow::new(vec![TableCell::new().add_table(inner)])]);
        let err =
            convert_with(&EpubLimits::default(), &pack(Docx::new().add_table(outer))).unwrap_err();
        assert!(format!("{err:#}").contains("nested"), "{err:#}");
    }

    fn zip_with(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn an_entry_over_the_per_entry_limit_is_refused() {
        let bomb = zip_with(&[("word/document.xml", vec![b'a'; 1_000_000])]);
        assert!(bomb.len() < 10_000, "fixture should be highly compressed");
        let limits = EpubLimits {
            max_entry_bytes: 4096,
            max_total_bytes: 8192,
            ..EpubLimits::default()
        };
        let err = convert_with(&limits, &bomb).unwrap_err();
        assert!(format!("{err:#}").contains("per-entry limit"), "{err:#}");
    }

    #[test]
    fn many_entries_over_the_total_limit_are_refused() {
        let entries: Vec<(String, Vec<u8>)> = (0..4)
            .map(|i| (format!("f{i}"), vec![b'a'; 3000]))
            .collect();
        let refs: Vec<(&str, Vec<u8>)> = entries
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let limits = EpubLimits {
            max_entry_bytes: 4096,
            max_total_bytes: 8192,
            ..EpubLimits::default()
        };
        let err = convert_with(&limits, &zip_with(&refs)).unwrap_err();
        assert!(format!("{err:#}").contains("total limit"), "{err:#}");
    }

    #[test]
    fn too_many_entries_are_refused() {
        let limits = EpubLimits {
            max_entries: 1,
            ..EpubLimits::default()
        };
        let z = zip_with(&[("a", vec![1]), ("b", vec![2])]);
        let err = convert_with(&limits, &z).unwrap_err();
        assert!(format!("{err:#}").contains("entries"), "{err:#}");
    }

    #[test]
    fn text_inside_a_content_control_is_kept_in_order() {
        let sdt = StructuredDataTag::new()
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Patient: Jane")))
            .add_table(Table::new(vec![TableRow::new(vec![cell("in-sdt-table")])]));
        let docx = Docx::new()
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Before")))
            .add_structured_data_tag(sdt)
            .add_paragraph(Paragraph::new().add_run(Run::new().add_text("After")));
        let md = convert_with(&EpubLimits::default(), &pack(docx)).unwrap();
        let (b, p, t, a) = (
            md.find("Before").unwrap(),
            md.find("Patient: Jane").expect(&md),
            md.find("| in-sdt-table |").expect(&md),
            md.find("After").unwrap(),
        );
        assert!(b < p && p < t && t < a, "wrong order: {md}");
    }

    #[test]
    fn hyperlink_and_tracked_insertion_text_are_kept_and_deletion_skipped() {
        let para = Paragraph::new()
            .add_run(Run::new().add_text("See "))
            .add_hyperlink(
                Hyperlink::new("https://example.com/x", HyperlinkType::External)
                    .add_run(Run::new().add_text("the site")),
            )
            .add_insert(Insert::new(Run::new().add_text(" inserted")))
            .add_delete(Delete::new().add_run(Run::new().add_delete_text(" removed")));
        let md = convert_with(
            &EpubLimits::default(),
            &pack(Docx::new().add_paragraph(para)),
        )
        .unwrap();
        assert!(
            md.contains("See [the site](https://example.com/x) inserted"),
            "{md}"
        );
        assert!(!md.contains("removed"), "{md}");
    }
}
