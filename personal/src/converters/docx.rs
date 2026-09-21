//! DOCX → markdown using the pure-Rust `docx-rs` crate. We walk the document
//! body once, emitting one markdown line per paragraph. Headings (`Heading 1`
//! through `Heading 6` styles) become markdown `#` levels. Lists, tables, and
//! embedded media are not deeply supported — if the user has a high-fidelity
//! requirement we'll know from real fixtures and revisit (pandoc shell-out).

use crate::error::{PersonalError, Result};

pub fn convert(bytes: Vec<u8>) -> Result<String> {
    let docx = docx_rs::read_docx(&bytes).map_err(|e| PersonalError::Converter {
        format: "docx",
        source: anyhow::anyhow!("docx-rs parse failed: {e:?}"),
    })?;

    let mut out = String::new();
    for child in &docx.document.children {
        if let docx_rs::DocumentChild::Paragraph(p) = child {
            let level = heading_level(p);
            let text = paragraph_text(p);
            if text.trim().is_empty() {
                if !out.is_empty() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                continue;
            }
            if let Some(n) = level {
                let hashes = "#".repeat(n.clamp(1, 6) as usize);
                out.push_str(&format!("{hashes} {text}\n\n"));
            } else {
                out.push_str(&text);
                out.push_str("\n\n");
            }
        }
    }

    if out.trim().is_empty() {
        return Err(PersonalError::Converter {
            format: "docx",
            source: anyhow::anyhow!("document contained no extractable text"),
        });
    }

    Ok(out)
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

fn paragraph_text(p: &docx_rs::Paragraph) -> String {
    let mut s = String::new();
    for child in &p.children {
        if let docx_rs::ParagraphChild::Run(r) = child {
            for rc in &r.children {
                if let docx_rs::RunChild::Text(t) = rc {
                    s.push_str(&t.text);
                }
            }
        }
    }
    s
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
}
