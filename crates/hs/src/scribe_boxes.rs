//! `hs scribe boxes`: what the corpus holds towards word-box sidecars, and
//! queueing the builds that are missing.
//!
//! Nothing is built here. A build is a `scribe.boxes` event, and the running
//! `hs scribe watch-events` consumes it (`hs_scribe::box_watch`).

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::event_bus::{specs, QueueDepth};
use hs_common::global_args::{GlobalArgs, OutputFormat};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, StoreKind, StoreSpec, Stores};
use hs_scribe::box_watch::{self, BoxSurvey, BOXED_SOURCE_EXTENSION};
use hs_scribe::config::ScribeConfig;
use serde::Serialize;

/// How many requests are published between two progress lines.
const PROGRESS_EVERY_REQUESTS: usize = 500;

#[derive(Subcommand, Debug)]
pub enum BoxesAction {
    /// Queue a `scribe.boxes` build for every converted PDF that has no
    /// word-box sidecar. The running `hs scribe watch-events` builds them.
    CatchUp {
        /// Only this document store (default: every store that holds PDFs).
        #[arg(long)]
        store: Option<String>,
        /// Print the storage key of each PDF that would be queued, one per
        /// line, and publish nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Per store: PDFs, converted PDFs, sidecars built and sidecars
    /// missing; then the depth of the `scribe.boxes` queue.
    Status,
}

pub async fn dispatch(
    action: BoxesAction,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    match action {
        BoxesAction::CatchUp { store, dry_run } => {
            cmd_catch_up(store.as_deref(), dry_run, global, reporter).await
        }
        BoxesAction::Status => cmd_status(global, reporter).await,
    }
}

/// The stores whose own documents may be PDFs: all of them, or the one
/// named. A named store that is derived from another, holds no documents, or
/// takes no PDFs is refused with the reason.
fn boxed_stores<'s>(
    stores: &'s Stores,
    only: Option<&str>,
) -> Result<Vec<(&'s StoreSpec, &'s DocumentLayout)>> {
    let holds_pdfs = |spec: &'s StoreSpec| match &spec.kind {
        StoreKind::Documents(layout)
            if layout.source_extensions.contains(&BOXED_SOURCE_EXTENSION) =>
        {
            Some((spec, layout))
        }
        _ => None,
    };
    let all: Vec<_> = stores.iter().filter_map(holds_pdfs).collect();
    let Some(name) = only else {
        return Ok(all);
    };
    let spec = stores.get(name)?;
    let Some(selected) = holds_pdfs(spec) else {
        let names: Vec<&str> = all.iter().map(|(spec, _)| spec.name).collect();
        anyhow::bail!(
            "store `{name}` holds no PDFs of its own, so it has no word boxes; \
             the stores that do: {}",
            names.join(", ")
        );
    };
    Ok(vec![selected])
}

/// What one store holds towards word boxes.
#[derive(Debug, Serialize)]
struct StoreBoxes {
    store: &'static str,
    pdfs: usize,
    converted: usize,
    with_boxes: usize,
    missing: usize,
}

impl StoreBoxes {
    fn new(store: &'static str, found: &BoxSurvey) -> Self {
        Self {
            store,
            pdfs: found.pdfs,
            converted: found.converted,
            with_boxes: found.boxed,
            missing: found.missing.len(),
        }
    }

    fn line(&self) -> String {
        format!(
            "{}: {} PDFs, {} converted, {} with word boxes, {} missing",
            self.store, self.pdfs, self.converted, self.with_boxes, self.missing
        )
    }
}

/// Every selected store's counts, and the converted PDFs that lack a sidecar.
struct Surveyed {
    stores: Vec<StoreBoxes>,
    missing_keys: Vec<String>,
}

/// Survey each store.
async fn survey_stores<'s>(
    storage: &dyn Storage,
    selected: &[(&'s StoreSpec, &'s DocumentLayout)],
    reporter: &Arc<dyn Reporter>,
) -> Result<Surveyed> {
    let mut surveyed = Surveyed {
        stores: Vec::with_capacity(selected.len()),
        missing_keys: Vec::new(),
    };
    for (spec, layout) in selected {
        reporter.status("Scan", &format!("store {}", spec.name));
        let found = box_watch::survey(storage, layout)
            .await
            .with_context(|| format!("surveying store {}", spec.name))?;
        surveyed.stores.push(StoreBoxes::new(spec.name, &found));
        surveyed.missing_keys.extend(found.missing);
    }
    Ok(surveyed)
}

/// The result of `hs scribe boxes catch-up`.
#[derive(Debug, Serialize)]
struct CatchUpReport {
    dry_run: bool,
    stores: Vec<StoreBoxes>,
    /// Converted PDFs with no sidecar.
    missing: usize,
    /// `scribe.boxes` events published.
    requested: usize,
    /// Events that could not be published.
    errors: usize,
    /// Storage key of each PDF a dry run would queue; empty otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keys: Vec<String>,
}

impl CatchUpReport {
    fn text(&self) -> String {
        if self.dry_run {
            return self.keys.join("\n");
        }
        if self.missing == 0 {
            return "Nothing to do — every converted PDF has its word boxes.".to_string();
        }
        format!(
            "Queued {} of {} word-box builds ({} failed to publish)",
            self.requested, self.missing, self.errors
        )
    }
}

/// The result of `hs scribe boxes status`.
#[derive(Debug, Serialize)]
struct StatusReport {
    stores: Vec<StoreBoxes>,
    /// Converted PDFs with no sidecar, over every store.
    missing: usize,
    /// `scribe.boxes` events no worker has taken yet.
    queued: u64,
    /// Events a worker has taken and not yet acknowledged.
    building: u64,
}

impl StatusReport {
    fn text(&self) -> String {
        let mut lines: Vec<String> = self.stores.iter().map(StoreBoxes::line).collect();
        lines.push(format!(
            "scribe.boxes queue: {} queued, {} building; {} PDFs missing word boxes",
            self.queued, self.building, self.missing
        ));
        lines.join("\n")
    }
}

/// The result in the format `--output` asked for: pretty JSON, one compact
/// JSON line, or the human text.
fn render_result<T: Serialize>(format: &OutputFormat, report: &T, text: &str) -> Result<String> {
    Ok(match format {
        OutputFormat::Json => serde_json::to_string_pretty(report)?,
        OutputFormat::Ndjson => serde_json::to_string(report)?,
        OutputFormat::Text => text.to_string(),
    })
}

/// Print a command's result on stdout, where a script reads it. Progress and
/// diagnostics go through the reporter, on stderr.
fn print_result<T: Serialize>(format: &OutputFormat, report: &T, text: &str) -> Result<()> {
    let rendered = render_result(format, report, text)?;
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    Ok(())
}

async fn cmd_catch_up(
    store: Option<&str>,
    dry_run: bool,
    global: &GlobalArgs,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let stop = crate::shutdown::cooperative();
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let stores = Stores::load().context("reading the store registry")?;
    let selected = boxed_stores(&stores, store)?;

    let Surveyed {
        stores: counts,
        missing_keys: missing,
    } = survey_stores(&*storage, &selected, reporter).await?;

    let mut report = CatchUpReport {
        dry_run,
        stores: counts,
        missing: missing.len(),
        requested: 0,
        errors: 0,
        keys: Vec::new(),
    };

    if dry_run {
        report.keys = missing;
        print_result(&global.output, &report, &report.text())?;
        reporter.finish("Dry-run complete — no events published.");
        return Ok(());
    }
    if missing.is_empty() {
        return print_result(&global.output, &report, &report.text());
    }

    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus for scribe.boxes publish")?;
    let mut errors = Vec::new();
    let mut interrupted = false;
    for (i, key) in missing.iter().enumerate() {
        if stop.requested() {
            interrupted = true;
            break;
        }
        match box_watch::request_boxes(&*bus, key).await {
            Ok(()) => report.requested += 1,
            Err(e) => errors.push(e.to_string()),
        }
        if (i + 1) % PROGRESS_EVERY_REQUESTS == 0 {
            reporter.status(
                "Queue",
                &format!("requested {}/{}", report.requested, missing.len()),
            );
        }
    }
    report.errors = errors.len();

    print_result(&global.output, &report, &report.text())?;
    crate::pipeline_cmd::fail_if_errors("scribe boxes catch-up", &errors, interrupted)
}

async fn cmd_status(global: &GlobalArgs, reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let stores = Stores::load().context("reading the store registry")?;
    let selected = boxed_stores(&stores, None)?;
    let surveyed = survey_stores(&*storage, &selected, reporter).await?;

    // A queue that cannot be read is an error, never a depth of zero. The
    // counts are still said before the error is returned.
    let depth = match read_queue_depth(&cfg).await {
        Ok(depth) => depth,
        Err(e) => {
            for counts in &surveyed.stores {
                reporter.status("Store", &counts.line());
            }
            return Err(e);
        }
    };
    let report = StatusReport {
        missing: surveyed.missing_keys.len(),
        stores: surveyed.stores,
        queued: depth.pending,
        building: depth.ack_pending,
    };
    print_result(&global.output, &report, &report.text())
}

async fn read_queue_depth(cfg: &ScribeConfig) -> Result<QueueDepth> {
    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus to read the scribe.boxes queue")?;
    bus.queue_depth(&specs::SCRIBE_BOXES).await.context(
        "reading the scribe.boxes queue (its consumer exists once an `hs scribe watch-events` \
         of this version has started)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_common::stores::{ACADEMIC, PERSONAL, PERSONAL_PREFIX_DEFAULT, REFERENCES, WEB};

    fn stores() -> Stores {
        Stores::with_personal_prefix(PERSONAL_PREFIX_DEFAULT).unwrap()
    }

    fn names(selected: &[(&StoreSpec, &DocumentLayout)]) -> Vec<&'static str> {
        selected.iter().map(|(spec, _)| spec.name).collect()
    }

    #[test]
    fn word_boxes_belong_to_the_stores_that_hold_pdfs() {
        let stores = stores();
        assert_eq!(
            names(&boxed_stores(&stores, None).unwrap()),
            [ACADEMIC, PERSONAL]
        );
        assert_eq!(
            names(&boxed_stores(&stores, Some(PERSONAL)).unwrap()),
            [PERSONAL]
        );
    }

    #[test]
    fn a_store_with_no_pdfs_of_its_own_is_refused_by_name() {
        let stores = stores();
        // Derived from the academic documents; holds only web pages; unknown.
        for name in [REFERENCES, WEB] {
            let err = boxed_stores(&stores, Some(name)).unwrap_err().to_string();
            assert!(err.contains(name), "{err}");
            assert!(err.contains("academic, personal"), "{err}");
        }
        let unknown = boxed_stores(&stores, Some("nope")).unwrap_err().to_string();
        assert!(unknown.contains("nope"), "{unknown}");
    }

    fn counts(store: &'static str, missing: usize) -> StoreBoxes {
        StoreBoxes {
            store,
            pdfs: 10,
            converted: 9,
            with_boxes: 9 - missing,
            missing,
        }
    }

    fn status_report() -> StatusReport {
        StatusReport {
            stores: vec![counts("academic", 2), counts("personal", 0)],
            missing: 2,
            queued: 5,
            building: 1,
        }
    }

    #[test]
    fn status_result_names_every_store_and_the_queue() {
        let text = status_report().text();
        assert_eq!(
            text,
            "academic: 10 PDFs, 9 converted, 7 with word boxes, 2 missing\n\
             personal: 10 PDFs, 9 converted, 9 with word boxes, 0 missing\n\
             scribe.boxes queue: 5 queued, 1 building; 2 PDFs missing word boxes"
        );
    }

    #[test]
    fn catch_up_result_says_what_was_requested_and_the_nothing_to_do_case() {
        let queued = CatchUpReport {
            dry_run: false,
            stores: vec![counts("academic", 3)],
            missing: 3,
            requested: 2,
            errors: 1,
            keys: Vec::new(),
        };
        assert_eq!(
            queued.text(),
            "Queued 2 of 3 word-box builds (1 failed to publish)"
        );

        let nothing = CatchUpReport {
            missing: 0,
            requested: 0,
            errors: 0,
            ..queued
        };
        assert_eq!(
            nothing.text(),
            "Nothing to do — every converted PDF has its word boxes."
        );
    }

    #[test]
    fn dry_run_result_is_one_storage_key_per_line() {
        let dry = CatchUpReport {
            dry_run: true,
            stores: vec![counts("academic", 2)],
            missing: 2,
            requested: 0,
            errors: 0,
            keys: vec!["papers/ab/a.pdf".into(), "papers/cd/b.pdf".into()],
        };
        assert_eq!(dry.text(), "papers/ab/a.pdf\npapers/cd/b.pdf");
    }

    #[test]
    fn result_follows_the_output_format() {
        let report = status_report();
        let text = report.text();

        assert_eq!(
            render_result(&OutputFormat::Text, &report, &text).unwrap(),
            text
        );

        let pretty = render_result(&OutputFormat::Json, &report, &text).unwrap();
        let json: serde_json::Value = serde_json::from_str(&pretty).unwrap();
        assert_eq!(json["missing"], 2);
        assert_eq!(json["queued"], 5);
        assert_eq!(json["building"], 1);
        assert_eq!(json["stores"][0]["store"], "academic");
        assert_eq!(json["stores"][0]["with_boxes"], 7);

        let line = render_result(&OutputFormat::Ndjson, &report, &text).unwrap();
        assert!(!line.contains('\n'), "{line}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            json
        );
    }
}
