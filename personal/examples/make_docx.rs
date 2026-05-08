//! Tiny one-shot fixture builder. Emits a small DOCX so the personal CLI's
//! end-to-end smoke can ingest a real `.docx` without pulling in pandoc or
//! python-docx.
//!
//! Usage: `cargo run -p personal --release --example make_docx -- <path>`

use docx_rs::*;
use std::env;

fn main() {
    let path = env::args().nth(1).expect("usage: make_docx <output-path>");
    let docx = Docx::new()
        .add_paragraph(
            Paragraph::new()
                .style("Heading1")
                .add_run(Run::new().add_text("2024 W2 Summary")),
        )
        .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Employer: Acme Corporation")))
        .add_paragraph(Paragraph::new().add_run(Run::new().add_text("Wages: 95,000.00")))
        .add_paragraph(
            Paragraph::new().add_run(Run::new().add_text("Federal income tax withheld: 14,200.00")),
        );
    let f = std::fs::File::create(&path).expect("create");
    docx.build().pack(f).expect("pack");
    println!("wrote {path}");
}
