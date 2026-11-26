//! `hs scribe boxes`: what the corpus holds towards word-box sidecars, and
//! queueing the builds that are missing.
//!
//! Nothing is built here. A build is a `scribe.boxes` event, and the running
//! `hs scribe watch-events` consumes it (`hs_scribe::box_watch`).

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Subcommand;
use hs_common::event_bus::specs;
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use hs_common::stores::{DocumentLayout, StoreKind, StoreSpec, Stores};
use hs_scribe::box_watch::{self, BoxSurvey, BOXED_SOURCE_EXTENSION};
use hs_scribe::config::ScribeConfig;

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

pub async fn dispatch(action: BoxesAction, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match action {
        BoxesAction::CatchUp { store, dry_run } => {
            cmd_catch_up(store.as_deref(), dry_run, reporter).await
        }
        BoxesAction::Status => cmd_status(reporter).await,
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

/// Survey each store and report its counts.
async fn survey_stores<'s>(
    storage: &dyn Storage,
    selected: &[(&'s StoreSpec, &'s DocumentLayout)],
    reporter: &Arc<dyn Reporter>,
) -> Result<Vec<BoxSurvey>> {
    let mut surveys = Vec::with_capacity(selected.len());
    for (spec, layout) in selected {
        reporter.status("Scan", &format!("store {}", spec.name));
        let found = box_watch::survey(storage, layout)
            .await
            .with_context(|| format!("surveying store {}", spec.name))?;
        reporter.status(
            "Store",
            &format!(
                "{}: {} PDFs, {} converted, {} with word boxes, {} missing",
                spec.name,
                found.pdfs,
                found.converted,
                found.boxed,
                found.missing.len()
            ),
        );
        surveys.push(found);
    }
    Ok(surveys)
}

async fn cmd_catch_up(
    store: Option<&str>,
    dry_run: bool,
    reporter: &Arc<dyn Reporter>,
) -> Result<()> {
    let stop = crate::shutdown::cooperative();
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let stores = Stores::load().context("reading the store registry")?;
    let selected = boxed_stores(&stores, store)?;

    let missing: Vec<String> = survey_stores(&*storage, &selected, reporter)
        .await?
        .into_iter()
        .flat_map(|found| found.missing)
        .collect();

    if dry_run {
        for key in &missing {
            println!("{key}");
        }
        reporter.finish("Dry-run complete — no events published.");
        return Ok(());
    }
    if missing.is_empty() {
        reporter.finish("Nothing to do — every converted PDF has its word boxes.");
        return Ok(());
    }

    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus for scribe.boxes publish")?;
    let mut requested = 0usize;
    let mut errors = Vec::new();
    let mut interrupted = false;
    for (i, key) in missing.iter().enumerate() {
        if stop.requested() {
            interrupted = true;
            break;
        }
        match box_watch::request_boxes(&*bus, key).await {
            Ok(()) => requested += 1,
            Err(e) => errors.push(e.to_string()),
        }
        if (i + 1) % PROGRESS_EVERY_REQUESTS == 0 {
            reporter.status("Queue", &format!("requested {requested}/{}", missing.len()));
        }
    }

    reporter.finish(&format!(
        "Catch-up queued — boxes_requested={requested} errors={}",
        errors.len()
    ));
    crate::pipeline_cmd::fail_if_errors("scribe boxes catch-up", &errors, interrupted)
}

async fn cmd_status(reporter: &Arc<dyn Reporter>) -> Result<()> {
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage().context("building storage backend")?;
    let stores = Stores::load().context("reading the store registry")?;
    let selected = boxed_stores(&stores, None)?;
    let missing: usize = survey_stores(&*storage, &selected, reporter)
        .await?
        .iter()
        .map(|found| found.missing.len())
        .sum();

    // The counts above are printed whatever the queue says. A queue that
    // cannot be read is an error, never a depth of zero.
    let bus = cfg
        .build_event_bus()
        .await
        .context("building event bus to read the scribe.boxes queue")?;
    let depth = bus.queue_depth(&specs::SCRIBE_BOXES).await.context(
        "reading the scribe.boxes queue (its consumer exists once an `hs scribe watch-events` \
         of this version has started)",
    )?;
    reporter.finish(&format!(
        "Word boxes — missing={missing} queued={} building={}",
        depth.pending, depth.ack_pending
    ));
    Ok(())
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
}
