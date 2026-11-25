//! Drift tests between the `hs` command tree and the user documentation.
//!
//! * `docs/reference/cli.md` has one heading `` ### `hs <path>` `` for every
//!   command path, and no heading for a path that does not exist.
//! * Every `hs …` command written in a user-facing page names a real command.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::CommandFactory;

use crate::cli::Cli;

/// The full command tree, built the way `hs --help` builds it.
fn command_tree() -> clap::Command {
    let mut root = Cli::command();
    root.build();
    root
}

/// Push `"<prefix> <name>"` for every visible subcommand below `cmd`,
/// intermediate groups included.
fn command_paths(cmd: &clap::Command, prefix: &str, out: &mut Vec<String>) {
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() || sub.get_name() == "help" {
            continue;
        }
        let path = format!("{prefix} {}", sub.get_name());
        out.push(path.clone());
        command_paths(sub, &path, out);
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A repository file's text; panics naming the path when it is missing.
fn repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The command paths named by the headings of a reference page: a line of
/// `##`, `###` or `####`, a space, then `` `hs <path>` `` and nothing else.
fn documented_commands(markdown: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in markdown.lines() {
        let line = line.trim_end();
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if !(2..=4).contains(&hashes) {
            continue;
        }
        let Some(rest) = line[hashes..].strip_prefix(' ') else {
            continue;
        };
        let Some(inner) = rest
            .strip_prefix("`hs ")
            .and_then(|r| r.strip_suffix('`'))
            .map(|r| format!("hs {r}"))
        else {
            continue;
        };
        found.insert(inner);
    }
    found
}

/// Panic with both directions of a set mismatch.
fn assert_same_sets(what: &str, expected: &BTreeSet<String>, documented: &BTreeSet<String>) {
    let missing: Vec<&String> = expected.difference(documented).collect();
    let extra: Vec<&String> = documented.difference(expected).collect();
    if missing.is_empty() && extra.is_empty() {
        return;
    }
    let list = |items: &[&String]| {
        if items.is_empty() {
            "  (none)".to_string()
        } else {
            items
                .iter()
                .map(|s| format!("  {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    panic!(
        "{what} and the code disagree.\nmissing from docs:\n{}\ndocuments no command:\n{}",
        list(&missing),
        list(&extra)
    );
}

#[test]
fn every_command_has_a_reference_entry_and_every_entry_is_a_command() {
    let root = command_tree();
    let mut paths = Vec::new();
    command_paths(&root, "hs", &mut paths);
    let expected: BTreeSet<String> = paths.into_iter().collect();

    let documented = documented_commands(&repo_file("docs/reference/cli.md"));
    assert_same_sets("docs/reference/cli.md", &expected, &documented);
}

// ── Commands mentioned in user documentation ─────────────────────

/// Directories under `docs/` that are internal records, not user documentation.
const INTERNAL_DOC_DIRS: [&str; 3] = ["loop", "specs", "research"];

/// Every `*.md` under `dir`, recursively, skipping internal records.
fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let internal = dir.ends_with("docs") && INTERNAL_DOC_DIRS.contains(&name.as_str());
            if !internal {
                collect_markdown(&path, out);
            }
        } else if name.ends_with(".md") && !name.starts_with("robustness-audit-") {
            out.push(path);
        }
    }
}

/// The user-facing markdown files, as paths relative to the repository root.
fn user_doc_files() -> Vec<String> {
    let root = repo_root();
    let mut files: Vec<String> = vec!["README.md".into(), "CONTRIBUTING.md".into()];

    let mut docs = Vec::new();
    collect_markdown(&root.join("docs"), &mut docs);
    files.extend(docs.iter().map(|p| rel_to(&root, p)));

    let mut crate_dirs: Vec<_> = std::fs::read_dir(root.join("crates"))
        .unwrap_or_else(|e| panic!("cannot read crates/: {e}"))
        .flatten()
        .map(|e| e.path())
        .collect();
    crate_dirs.sort();
    for dir in crate_dirs {
        let readme = dir.join("README.md");
        if readme.is_file() {
            files.push(rel_to(&root, &readme));
        }
    }
    files.push("paper/README.md".into());
    files.push("hs-common/README.md".into());
    files
}

fn rel_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every text that claims to be an `hs` command: lines inside fenced code
/// blocks, and inline code spans everywhere else.
fn command_candidates(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            let cmd = trimmed.strip_prefix("$ ").unwrap_or(trimmed);
            if cmd.starts_with("hs ") {
                out.push(cmd.to_string());
            }
        } else {
            let parts: Vec<&str> = line.split('`').collect();
            // Odd-numbered parts are inside backticks; the last part is never
            // closed by a following backtick.
            for (i, part) in parts.iter().enumerate() {
                if i % 2 == 1 && i + 1 < parts.len() && part.starts_with("hs ") {
                    out.push((*part).to_string());
                }
            }
        }
    }
    out
}

/// A word that can name a subcommand.
fn is_command_word(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Walk `candidate` down the command tree. `Some((word, path))` names the
/// first word that is not a subcommand of the command reached so far.
fn first_unknown_word(root: &clap::Command, candidate: &str) -> Option<(String, String)> {
    let mut words = candidate.split_whitespace();
    words.next()?; // the leading `hs`
    let mut cmd = root;
    let mut path = String::from("hs");
    for (index, word) in words.enumerate() {
        if !is_command_word(word) {
            // A flag, a quoted argument, a placeholder like `<path>`: the
            // command part of the line is over.
            return None;
        }
        match cmd.find_subcommand(word) {
            Some(sub) => {
                cmd = sub;
                path = format!("{path} {}", sub.get_name());
            }
            None if index == 0 => return Some((word.to_string(), path)),
            None if cmd.has_subcommands() && cmd.get_positionals().next().is_none() => {
                return Some((word.to_string(), path));
            }
            None => return None,
        }
    }
    None
}

#[test]
fn commands_mentioned_in_user_docs_exist() {
    let root = command_tree();
    let mut errors = Vec::new();
    for file in user_doc_files() {
        let text = repo_file(&file);
        for candidate in command_candidates(&text) {
            if let Some((word, path)) = first_unknown_word(&root, &candidate) {
                errors.push(format!(
                    "{file}: `{candidate}` — `{word}` is not a subcommand of `{path}`"
                ));
            }
        }
    }
    assert!(
        errors.is_empty(),
        "{} documented command(s) do not exist:\n{}",
        errors.len(),
        errors.join("\n")
    );
}

mod parsing {
    use super::*;

    #[test]
    fn headings_are_read_at_levels_two_to_four_only() {
        let md = "# `hs top`\n## `hs scribe`\n### `hs scribe inbox sweep`  \n\
                  #### `hs a b c`\n##### `hs deep`\n### `hs`\n### hs scribe\n\
                  ### `hs trailing` text\n";
        let found: Vec<String> = documented_commands(md).into_iter().collect();
        assert_eq!(
            found,
            ["hs a b c", "hs scribe", "hs scribe inbox sweep"].map(String::from)
        );
    }

    #[test]
    fn candidates_come_from_fences_and_inline_spans() {
        let md = "Run `hs status` now, or `hs` alone, or `unclosed hs x\n\
                  ```bash\n$ hs paper search \"x\"\n  hs config path\nls hs\n```\n\
                  after `hs serve docs --install` done\r\n";
        assert_eq!(
            command_candidates(md),
            [
                "hs status",
                "hs paper search \"x\"",
                "hs config path",
                "hs serve docs --install"
            ]
            .map(String::from)
        );
    }

    #[test]
    fn the_walk_names_the_first_word_that_is_not_a_subcommand() {
        let root = command_tree();
        // A real path, with flags and arguments after it.
        assert_eq!(first_unknown_word(&root, "hs scribe inbox sweep"), None);
        assert_eq!(
            first_unknown_word(&root, "hs scribe convert paper.pdf --out a.md"),
            None
        );
        // A positional argument is not mistaken for a subcommand.
        assert_eq!(first_unknown_word(&root, "hs scribe convert paper"), None);
        assert_eq!(
            first_unknown_word(&root, "hs distill search attention"),
            None
        );
        // Flags and placeholders end the walk.
        assert_eq!(first_unknown_word(&root, "hs --version"), None);
        assert_eq!(first_unknown_word(&root, "hs <command> --help"), None);
        // Typos are found, at the top level and below it.
        assert_eq!(
            first_unknown_word(&root, "hs pipeline catchup"),
            Some(("catchup".into(), "hs pipeline".into()))
        );
        assert_eq!(
            first_unknown_word(&root, "hs bogus"),
            Some(("bogus".into(), "hs".into()))
        );
    }
}
