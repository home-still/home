use super::{fintabnet_dir, GroundTruthSample};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Load FinTabNet ground truth samples from extracted archives.
/// Expected layout after extraction:
///   fintabnet/FinTabNet.c-PDF_Annotations/<TICKER>_<YEAR>_page_<N>_tables.json
///   fintabnet/FinTabNet.c-Structure/{train,val,test}/<TICKER>_<YEAR>_page_<N>_table_<M>.xml
///   fintabnet/FinTabNet.c-Structure/images/<TICKER>_<YEAR>_page_<N>_table_<M>.jpg
pub fn load_fintabnet_samples(
    split: &str,
    limit: Option<usize>,
) -> Result<Vec<GroundTruthSample>> {
    let base = fintabnet_dir();
    let structure_dir = base.join("FinTabNet.c-Structure").join(split);
    let annotations_dir = base.join("FinTabNet.c-PDF_Annotations");
    let images_dir = base.join("FinTabNet.c-Structure").join("images");

    if !structure_dir.exists() {
        anyhow::bail!(
            "FinTabNet structure dir not found at {}. Run scripts/download_datasets.sh first.",
            structure_dir.display()
        );
    }

    let mut xml_entries: Vec<_> = std::fs::read_dir(&structure_dir)
        .with_context(|| format!("Failed to read {}", structure_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "xml")
                .unwrap_or(false)
        })
        .collect();

    xml_entries.sort_by_key(|e| e.file_name());

    let max = limit.unwrap_or(usize::MAX);
    let mut samples = Vec::new();

    for entry in xml_entries.into_iter().take(max) {
        let xml_path = entry.path();
        let stem = xml_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Image path: FinTabNet.c-Structure/images/{stem}.jpg
        let image_path = images_dir.join(format!("{}.jpg", stem));
        if !image_path.exists() {
            continue;
        }

        // Derive page-level annotation JSON path and table index
        // XML stem: TICKER_YEAR_page_N_table_M -> annotation: TICKER_YEAR_page_N_tables.json, index M
        let (page_stem, table_index) = derive_page_stem_and_index(&stem);
        let annotation_path = annotations_dir.join(format!("{}_tables.json", page_stem));

        let table_html = if annotation_path.exists() {
            build_table_html(&annotation_path, table_index).ok().flatten()
        } else {
            None
        };

        samples.push(GroundTruthSample {
            id: stem,
            pdf_path: PathBuf::new(),
            image_path: Some(image_path),
            page_index: None,
            text: None,
            text_blocks: None,
            table_html,
            formula_latex: None,
            markdown: None,
        });
    }

    Ok(samples)
}

/// Convert "TICKER_YEAR_page_N_table_M" to ("TICKER_YEAR_page_N", M)
fn derive_page_stem_and_index(table_stem: &str) -> (String, usize) {
    if let Some(idx) = table_stem.rfind("_table_") {
        let page_stem = table_stem[..idx].to_string();
        let table_index = table_stem[idx + 7..].parse::<usize>().unwrap_or(0);
        (page_stem, table_index)
    } else {
        (table_stem.to_string(), 0)
    }
}

/// Build proper HTML table from a FinTabNet annotation JSON file.
/// Extracts the specific table at `table_index` from the page's array of tables.
fn build_table_html(path: &PathBuf, table_index: usize) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)?;
    let tables: serde_json::Value = serde_json::from_str(&content)?;
    let tables = tables.as_array().context("Expected JSON array")?;

    let table = match tables.get(table_index) {
        Some(t) => t,
        None => return Ok(None),
    };

    let cells = match table["cells"].as_array() {
        Some(c) => c,
        None => return Ok(None),
    };

    if cells.is_empty() {
        return Ok(None);
    }

    // Find grid dimensions
    let mut max_row: usize = 0;
    let mut max_col: usize = 0;
    for cell in cells {
        if let Some(rows) = cell["row_nums"].as_array() {
            for r in rows {
                if let Some(r) = r.as_u64() {
                    max_row = max_row.max(r as usize);
                }
            }
        }
        if let Some(cols) = cell["column_nums"].as_array() {
            for c in cols {
                if let Some(c) = c.as_u64() {
                    max_col = max_col.max(c as usize);
                }
            }
        }
    }

    let num_rows = max_row + 1;
    let num_cols = max_col + 1;

    // Track which cells are occupied (by spanning cells)
    let mut occupied = vec![vec![false; num_cols]; num_rows];

    // Collect cell info: (start_row, start_col, rowspan, colspan, text, is_header)
    struct CellInfo {
        start_row: usize,
        start_col: usize,
        rowspan: usize,
        colspan: usize,
        text: String,
        is_header: bool,
    }

    let mut cell_infos = Vec::new();
    for cell in cells {
        let rows: Vec<usize> = cell["row_nums"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as usize))
            .collect();
        let cols: Vec<usize> = cell["column_nums"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as usize))
            .collect();

        if rows.is_empty() || cols.is_empty() {
            continue;
        }

        let start_row = *rows.iter().min().unwrap();
        let start_col = *cols.iter().min().unwrap();
        let rowspan = rows.len();
        let colspan = cols.len();
        let text = cell["json_text_content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let is_header = cell["is_column_header"].as_bool().unwrap_or(false);

        cell_infos.push(CellInfo {
            start_row,
            start_col,
            rowspan,
            colspan,
            text,
            is_header,
        });
    }

    // Sort cells by position for consistent output
    cell_infos.sort_by_key(|c| (c.start_row, c.start_col));

    // Build HTML
    let mut html = String::from("<table>");

    for row in 0..num_rows {
        html.push_str("<tr>");
        for col in 0..num_cols {
            if occupied[row][col] {
                continue;
            }

            // Find the cell that starts at this position
            if let Some(ci) = cell_infos
                .iter()
                .find(|c| c.start_row == row && c.start_col == col)
            {
                let tag = if ci.is_header { "th" } else { "td" };
                html.push('<');
                html.push_str(tag);

                if ci.rowspan > 1 {
                    html.push_str(&format!(" rowspan=\"{}\"", ci.rowspan));
                }
                if ci.colspan > 1 {
                    html.push_str(&format!(" colspan=\"{}\"", ci.colspan));
                }

                html.push('>');
                html.push_str(&html_escape(&ci.text));
                html.push_str("</");
                html.push_str(tag);
                html.push('>');

                // Mark spanned cells as occupied
                for dr in 0..ci.rowspan {
                    for dc in 0..ci.colspan {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let r = row + dr;
                        let c = col + dc;
                        if r < num_rows && c < num_cols {
                            occupied[r][c] = true;
                        }
                    }
                }
            } else {
                // Empty cell (no annotation for this position)
                html.push_str("<td></td>");
            }
        }
        html.push_str("</tr>");
    }

    html.push_str("</table>");
    Ok(Some(html))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
