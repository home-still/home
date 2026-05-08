//! 2-column academic PDF repro tests. See
//! `tests/repro/2col-academic/README.md` for fixture management.
//!
//! `#[ignore]` by default: this test posts PDFs to a live scribe server,
//! which CI doesn't have. Local invocation:
//!
//! ```bash
//! HS_SCRIBE_REPRO=1 \
//! HS_SCRIBE_TEST_BACKEND=openai-compat \
//! HS_SCRIBE_TEST_SCRIBE_URL=http://192.168.1.110:7433 \
//! cargo test -p hs-scribe --test repro_2col_test -- --ignored --nocapture
//! ```
//!
//! Skips silently when no fixtures are present so that adding the test
//! file doesn't require committing PDFs in the same PR.

use std::path::PathBuf;
use std::time::Duration;

use hs_scribe::client::ScribeClient;
use hs_scribe::diag::TruncationCounts;
use hs_scribe::postprocess::{
    clean_repetitions_per_page, is_bibliography_page, longest_repeated_run_bytes, qc_verdict,
    QcVerdict,
};

const FIXTURE_DIR: &str = "tests/repro/2col-academic";
const GOLDEN_SUBDIR: &str = "golden";
/// Maximum tolerated character-level Levenshtein ratio vs golden. Loose
/// enough to absorb whitespace / punctuation jitter from VLM nondeterminism
/// at greedy decoding (FP16 logit ties etc.); tight enough to catch a
/// hallucinated paragraph or a dropped column.
const LEVENSHTEIN_RATIO_LIMIT: f64 = 0.005;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

/// Discover `<stem>.pdf` files under the fixture dir. Returns paths in
/// stable sort order so failure messages are consistent.
fn discover_fixtures(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pdf"))
        .collect();
    out.sort();
    out
}

/// Iterative O(m·n) char-level Levenshtein with O(min(m, n)) memory.
/// Using a hand-rolled impl keeps the test crate dep-free.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (a_chars, b_chars) = if a_chars.len() < b_chars.len() {
        (b_chars, a_chars)
    } else {
        (a_chars, b_chars)
    };
    if b_chars.is_empty() {
        return a_chars.len();
    }
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr: Vec<usize> = vec![0; b_chars.len() + 1];
    for (i, ca) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

#[tokio::test]
#[ignore = "requires HS_SCRIBE_REPRO=1 + a live scribe server"]
async fn repro_2col_e2e() {
    if std::env::var("HS_SCRIBE_REPRO").ok().as_deref() != Some("1") {
        eprintln!("HS_SCRIBE_REPRO != 1; skipping (set it to opt in)");
        return;
    }

    let backend = std::env::var("HS_SCRIBE_TEST_BACKEND").unwrap_or_default();
    if backend == "ollama" {
        // ollama#14493 / #10767: the Go VLM runner silently drops
        // repeat_penalty / frequency_penalty / presence_penalty. A pass
        // against Ollama is measuring untuned model behavior — useless
        // as a regression signal for the sampling-param work in PR1.
        // Migrate to vLLM via OpenAI-compat to get an authoritative test.
        panic!(
            "Ollama backend not supported for repro tests; ollama#14493 \
             silently drops penalty params. Use HS_SCRIBE_TEST_BACKEND=openai-compat \
             pointing at a vLLM (or compatible) server."
        );
    }

    let scribe_url = std::env::var("HS_SCRIBE_TEST_SCRIBE_URL")
        .expect("HS_SCRIBE_TEST_SCRIBE_URL required (e.g. http://192.168.1.110:7433)");

    let dir = fixture_dir();
    let fixtures = discover_fixtures(&dir);
    if fixtures.is_empty() {
        eprintln!(
            "no .pdf fixtures under {} — see README.md for how to add one",
            dir.display()
        );
        return;
    }

    let golden_dir = dir.join(GOLDEN_SUBDIR);
    let client = ScribeClient::new_with_timeout(&scribe_url, Duration::from_secs(1800))
        .expect("scribe client");

    let mut failures: Vec<String> = Vec::new();

    for pdf in &fixtures {
        let stem = pdf
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("ascii stem")
            .to_string();
        eprintln!("─── {stem} ───");

        let pdf_bytes = std::fs::read(pdf).expect("read fixture pdf");
        let conv = match client
            .convert_with_progress(
                pdf_bytes,
                Some(Duration::from_secs(1800)),
                Some(stem.as_str()),
                |_| {},
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{stem}: convert failed: {e:#}"));
                continue;
            }
        };

        let (cleaned, per_page_truncs): (String, Vec<TruncationCounts>) =
            clean_repetitions_per_page(&conv.markdown);
        let total_truncs: usize = per_page_truncs.iter().map(|t| t.total()).sum();
        let longest_run = longest_repeated_run_bytes(&conv.markdown);
        let bib: Vec<bool> = (0..per_page_truncs.len())
            .map(|i| {
                conv.per_page_region_classes
                    .get(i)
                    .map(|c| is_bibliography_page(c))
                    .unwrap_or(false)
            })
            .collect();
        let verdict = qc_verdict(&per_page_truncs, &bib, longest_run);

        if total_truncs != 0 {
            failures.push(format!(
                "{stem}: total_truncations={total_truncs} (expected 0 — \
                 sampling params or model regressed)"
            ));
        }
        if verdict != QcVerdict::Accept {
            failures.push(format!("{stem}: qc_verdict={verdict:?} (expected Accept)"));
        }

        let golden_path = golden_dir.join(format!("{stem}.md"));
        if !golden_path.exists() {
            // First run: print the captured output so the operator can
            // review and save it as the golden. Don't fail — the README
            // documents this bootstrap step.
            eprintln!(
                "no golden at {} — captured {} chars; review and save as golden/{}.md",
                golden_path.display(),
                cleaned.chars().count(),
                stem
            );
            eprintln!("──── captured output ────");
            eprintln!("{cleaned}");
            eprintln!("──── end captured output ────");
            continue;
        }
        let golden = std::fs::read_to_string(&golden_path).expect("read golden");
        let golden_chars = golden.chars().count();
        if golden_chars == 0 {
            failures.push(format!("{stem}: golden is empty"));
            continue;
        }
        let dist = levenshtein(&cleaned, &golden);
        let ratio = dist as f64 / golden_chars as f64;
        eprintln!(
            "{stem}: truncations={total_truncs} verdict={verdict:?} \
             levenshtein={dist}/{golden_chars} ({ratio:.4})"
        );
        if ratio >= LEVENSHTEIN_RATIO_LIMIT {
            failures.push(format!(
                "{stem}: drift {ratio:.4} >= limit {LEVENSHTEIN_RATIO_LIMIT}; \
                 regenerate golden if the change was intentional and update the \
                 pin block in README.md"
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "repro_2col failures ({}):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn levenshtein_identity_zero() {
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn levenshtein_substitution_one() {
        assert_eq!(levenshtein("abc", "abd"), 1);
    }

    #[test]
    fn levenshtein_insertion() {
        assert_eq!(levenshtein("abc", "abxc"), 1);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn levenshtein_swapped_args_symmetric() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("sitting", "kitten"), 3);
    }

    #[test]
    fn discover_fixtures_returns_empty_for_missing_dir() {
        let nope = std::path::PathBuf::from("/no/such/path/here/zzz");
        assert!(discover_fixtures(&nope).is_empty());
    }
}
