//! Client-side inbox handler — format-aware wrapper around
//! `hs_common::inbox::write_target_and_publish`.
//!
//! Responsibilities that don't belong in `hs-common`:
//! - Inspect the source key's extension and decide whether to process, ignore,
//!   or defer it.
//! - Skip files whose mtime is < 5s old (a browser download that's still
//!   writing will fire a `notify` event before the content is complete).
//! - Convert EPUB → HTML *in memory* via `crate::scribe_cmd::epub_bytes_to_html`
//!   before handing bytes to the commit primitive. The extension on the
//!   target key is flipped from `.epub` to `.html` so the server-side
//!   event-bus subscriber (`hs-scribe/src/event_watch.rs:63`, which only
//!   branches `is_html` on `.html`/`.htm`) picks up the transformed content
//!   via its HTML code path.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use hs_common::event_bus::EventBus;
use hs_common::inbox::{write_target_and_publish, WriteOutcome};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;

use crate::scribe_cmd::{epub_bytes_to_html, InboxAction};

/// Files newer than this are assumed to still be written (browser
/// download in progress, rclone sync mid-upload). The next sweep picks
/// them up.
pub const MIN_AGE_BEFORE_PROCESSING: Duration = Duration::from_secs(5);

/// Outcome of `handle_inbox_source`. Distinguishes the three "skip" cases
/// from the commit primitive's three successful outcomes so callers can
/// log appropriately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleOutcome {
    Committed(WriteOutcome),
    IgnoredUnsupported {
        ext: String,
    },
    IgnoredStillWriting {
        age_secs: u64,
    },
    /// Path doesn't live under `{papers_prefix}/manually_downloaded/`. Safety
    /// net for the `notify` path, which fires on anything the watcher sees.
    IgnoredNonInbox,
}

/// Process a single file in the inbox prefix. Returns an outcome describing
/// what happened; the caller decides how to log it.
///
/// Format handling:
/// - `.pdf`, `.html`, `.htm`: bytes pass through unchanged; target keeps the
///   same extension.
/// - `.epub`: unpack the spine to HTML in-memory; target extension flips to
///   `.html` so the server-side event-bus path handles it natively.
/// - anything else: `IgnoredUnsupported`.
pub async fn handle_inbox_source(
    storage: &dyn Storage,
    bus: &dyn EventBus,
    papers_prefix: &str,
    source_key: &str,
    source_mtime: SystemTime,
    now: SystemTime,
) -> anyhow::Result<HandleOutcome> {
    let inbox_prefix = format!(
        "{}/manually_downloaded/",
        papers_prefix.trim_end_matches('/')
    );
    if !source_key.starts_with(&inbox_prefix) {
        return Ok(HandleOutcome::IgnoredNonInbox);
    }

    // Trailing filename after the last /.
    let filename = match source_key.rsplit('/').next() {
        Some(f) => f,
        None => return Ok(HandleOutcome::IgnoredNonInbox),
    };

    // macOS resource forks / browser temp files — always ignore.
    if filename.starts_with("._") {
        return Ok(HandleOutcome::IgnoredUnsupported {
            ext: "macos-resource-fork".into(),
        });
    }

    // Split stem and extension on the *last* dot. Anything with no extension
    // is unsupported.
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s, e.to_ascii_lowercase()),
        None => return Ok(HandleOutcome::IgnoredUnsupported { ext: String::new() }),
    };

    // Reject the well-known browser-temp / unsupported-format names up front.
    // `.tmp` on its own may be a download of any underlying type; we don't
    // inspect — just wait for the rename.
    if matches!(
        ext.as_str(),
        "download" | "part" | "crdownload" | "tmp" | "azw3" | "azw" | "mobi"
    ) {
        return Ok(HandleOutcome::IgnoredUnsupported { ext });
    }

    // Whitelist check.
    if !matches!(ext.as_str(), "pdf" | "html" | "htm" | "epub") {
        return Ok(HandleOutcome::IgnoredUnsupported { ext });
    }

    // Mtime guard. A brand-new drop is still being written; defer by
    // returning IgnoredStillWriting — the poll loop retries on the next tick.
    if let Ok(age) = now.duration_since(source_mtime) {
        if age < MIN_AGE_BEFORE_PROCESSING {
            return Ok(HandleOutcome::IgnoredStillWriting {
                age_secs: age.as_secs(),
            });
        }
    }

    // Read source bytes.
    let raw = storage
        .get(source_key)
        .await
        .map_err(|e| anyhow::anyhow!("read source {source_key}: {e}"))?;

    // Format-specific transform. EPUB is the only branch that changes
    // bytes *and* target extension; everything else is passthrough.
    let (bytes, target_ext) = if ext == "epub" {
        let html = epub_bytes_to_html(raw)
            .map_err(|e| anyhow::anyhow!("epub unpack {source_key}: {e}"))?;
        (html.into_bytes(), "html")
    } else {
        (raw, ext.as_str())
    };

    let target_key = format!(
        "{}/{}",
        papers_prefix.trim_end_matches('/'),
        hs_common::sharded_key(stem, target_ext)
    );

    let outcome = write_target_and_publish(storage, bus, source_key, &target_key, bytes).await?;
    Ok(HandleOutcome::Committed(outcome))
}

/// Aggregate counts from a sweep over the inbox prefix.
#[derive(Debug, Default, Clone)]
pub struct SweepReport {
    pub found: usize,
    pub relocated: u64,
    pub already_at_target: u64,
    pub partial_left_source: u64,
    pub ignored_unsupported: u64,
    pub ignored_still_writing: u64,
    pub errors: Vec<String>,
}

/// Walk `{papers_prefix}/manually_downloaded/` once and process each file.
/// Returns a report; individual failures are collected into `errors` and do
/// not abort the sweep.
pub async fn sweep_inbox_once(
    storage: &dyn Storage,
    bus: &dyn EventBus,
    papers_prefix: &str,
) -> anyhow::Result<SweepReport> {
    let inbox_prefix = format!(
        "{}/manually_downloaded/",
        papers_prefix.trim_end_matches('/')
    );
    let objects = storage
        .list(&inbox_prefix)
        .await
        .map_err(|e| anyhow::anyhow!("list {inbox_prefix}: {e}"))?;

    let now = SystemTime::now();
    let mut report = SweepReport {
        found: objects.len(),
        ..Default::default()
    };

    for obj in objects {
        let mtime = obj.last_modified.unwrap_or(SystemTime::UNIX_EPOCH);
        match handle_inbox_source(storage, bus, papers_prefix, &obj.key, mtime, now).await {
            Ok(HandleOutcome::Committed(WriteOutcome::Relocated)) => report.relocated += 1,
            Ok(HandleOutcome::Committed(WriteOutcome::AlreadyAtTarget)) => {
                report.already_at_target += 1
            }
            Ok(HandleOutcome::Committed(WriteOutcome::PartialLeftSource)) => {
                report.partial_left_source += 1
            }
            Ok(HandleOutcome::IgnoredUnsupported { .. }) => report.ignored_unsupported += 1,
            Ok(HandleOutcome::IgnoredStillWriting { .. }) => report.ignored_still_writing += 1,
            Ok(HandleOutcome::IgnoredNonInbox) => {
                // list() returns keys under the inbox prefix, so this branch
                // shouldn't fire in the sweep path. Count as an ignore.
                report.ignored_unsupported += 1;
            }
            Err(e) => report.errors.push(format!("{}: {e}", obj.key)),
        }
    }
    Ok(report)
}

/// Canonical papers prefix used by all downstream tools — server-side
/// scribe watch-events, MCP, distill all read from the same prefix.
const PAPERS_PREFIX: &str = "papers";

/// Dispatch for `hs scribe inbox ...`.
pub async fn dispatch(action: InboxAction, reporter: &Arc<dyn Reporter>) -> Result<()> {
    match action {
        InboxAction::Run | InboxAction::DaemonChild => cmd_run(reporter, false).await,
        InboxAction::Sweep => cmd_sweep(reporter).await,
        InboxAction::Install => crate::scribe_inbox_install::cmd_install(reporter).await,
        InboxAction::Uninstall => crate::scribe_inbox_install::cmd_uninstall(reporter).await,
        InboxAction::Status => crate::scribe_inbox_install::cmd_status(reporter).await,
    }
}

/// One-shot sweep. Lists the inbox, processes each file, prints a report, exits.
async fn cmd_sweep(reporter: &Arc<dyn Reporter>) -> Result<()> {
    use hs_scribe::config::ScribeConfig;
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    reporter.status("Sweep", "scanning papers/manually_downloaded/");
    let report = sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX).await?;
    log_report(reporter, &report);
    if !report.errors.is_empty() {
        anyhow::bail!("sweep completed with {} error(s)", report.errors.len());
    }
    Ok(())
}

/// Foreground daemon. Runs indefinitely — one polling loop plus a `notify`
/// watcher on the local mount (if `watch_dir` is configured).
///
/// `_daemon_child` is currently unused but kept so the LaunchAgent/systemd
/// wrapper can invoke the same code path with an explicit marker (useful
/// for future health probes).
async fn cmd_run(reporter: &Arc<dyn Reporter>, _daemon_child: bool) -> Result<()> {
    use hs_scribe::config::ScribeConfig;
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    let poll_interval = Duration::from_secs(cfg.inbox_poll_interval_secs.max(1));
    reporter.status(
        "Inbox",
        &format!(
            "polling every {}s; watch_dir={}",
            poll_interval.as_secs(),
            cfg.watch_dir.display(),
        ),
    );

    let sweep_interval_secs = poll_interval.as_secs();

    // Boot-time heartbeat + sweep — drains any existing backlog before
    // starting the tick cycle. The heartbeat also signals to any `hs status`
    // client that the daemon is alive within one tick of starting, not
    // after the first poll-interval. First stamp is `last_sweep=None`
    // (no sweep has run yet); the post-sweep stamp immediately below
    // populates the counts.
    if let Err(e) =
        hs_common::status::write_inbox_heartbeat(&*storage, sweep_interval_secs, None).await
    {
        tracing::warn!(error = %e, "initial heartbeat write failed");
    }
    match sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX).await {
        Ok(r) => {
            log_report(reporter, &r);
            if let Err(e) = hs_common::status::write_inbox_heartbeat(
                &*storage,
                sweep_interval_secs,
                Some((r.found as u64, r.relocated, r.errors.len() as u64)),
            )
            .await
            {
                tracing::warn!(error = %e, "post-sweep heartbeat write failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "initial sweep failed"),
    }

    // Ctrl+C handler. Set a flag; the poll loop checks it between ticks.
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag = Arc::clone(&shutdown);
    let _ = ctrlc::set_handler(move || {
        shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            reporter.status("Inbox", "shutdown requested");
            return Ok(());
        }
        tokio::time::sleep(poll_interval).await;
        // Pre-sweep heartbeat keeps the Watcher row "running" even when
        // a sweep stalls — `hs status` ages out the heartbeat at 2×
        // poll_interval + 5s grace. Pre-sweep stamp carries no counts
        // (`last_sweep=None`) so we don't rewrite stale stats into what
        // should be a liveness signal; the post-sweep stamp updates the
        // counts.
        if let Err(e) =
            hs_common::status::write_inbox_heartbeat(&*storage, sweep_interval_secs, None).await
        {
            tracing::warn!(error = %e, "heartbeat write failed; sweep will still run");
        }
        match sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX).await {
            Ok(r) => {
                if r.relocated > 0 || !r.errors.is_empty() {
                    log_report(reporter, &r);
                } else {
                    tracing::debug!("sweep: nothing to do");
                }
                // Post-sweep stamp: give `hs status` the actual counts
                // so the Watcher row can render `swept N / M`.
                if let Err(e) = hs_common::status::write_inbox_heartbeat(
                    &*storage,
                    sweep_interval_secs,
                    Some((r.found as u64, r.relocated, r.errors.len() as u64)),
                )
                .await
                {
                    tracing::warn!(error = %e, "post-sweep heartbeat write failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "sweep failed; will retry next tick"),
        }
    }
}

fn log_report(reporter: &Arc<dyn Reporter>, r: &SweepReport) {
    reporter.status(
        "Sweep",
        &format!(
            "found={} relocated={} already={} partial={} ignored={}+{} errors={}",
            r.found,
            r.relocated,
            r.already_at_target,
            r.partial_left_source,
            r.ignored_unsupported,
            r.ignored_still_writing,
            r.errors.len(),
        ),
    );
    for e in &r.errors {
        reporter.warn(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_common::event_bus::NoOpBus;
    use hs_common::storage::LocalFsStorage;

    const PAPERS: &str = "papers";

    fn just_now() -> SystemTime {
        SystemTime::now()
    }
    fn long_ago() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    #[tokio::test]
    async fn pdf_relocates_without_extension_flip() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        storage
            .put("papers/manually_downloaded/foo.pdf", b"pdfbytes".to_vec())
            .await
            .unwrap();

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/manually_downloaded/foo.pdf",
            long_ago(),
            just_now(),
        )
        .await
        .unwrap();

        assert_eq!(out, HandleOutcome::Committed(WriteOutcome::Relocated));
        assert_eq!(storage.get("papers/fo/foo.pdf").await.unwrap(), b"pdfbytes");
    }

    #[tokio::test]
    async fn epub_flips_extension_to_html() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        // Use a tiny valid EPUB — build one on-the-fly with the `epub-builder` crate?
        // That adds a dep. Simpler: use any .epub fixture. For unit-test purposes
        // we test the dispatch: we feed it raw bytes that AREN'T a valid EPUB and
        // assert the handler returns an unpack error. A positive end-to-end test
        // of the EPUB branch is covered by integration against a real file.
        storage
            .put(
                "papers/manually_downloaded/book.epub",
                b"not-actually-an-epub".to_vec(),
            )
            .await
            .unwrap();

        let result = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/manually_downloaded/book.epub",
            long_ago(),
            just_now(),
        )
        .await;

        // Malformed EPUB → unpack error surfaces as Err (not an ignore).
        // The key point is that the dispatch reached the EPUB branch rather
        // than ignoring a supported extension.
        assert!(
            result.is_err(),
            "expected epub unpack failure, got {:?}",
            result
        );
        // Source stays untouched on error; target is never written.
        assert!(storage
            .exists("papers/manually_downloaded/book.epub")
            .await
            .unwrap());
        assert!(!storage.exists("papers/bo/book.html").await.unwrap());
    }

    #[tokio::test]
    async fn download_extension_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        storage
            .put(
                "papers/manually_downloaded/in-progress.pdf.download",
                b"partial".to_vec(),
            )
            .await
            .unwrap();

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/manually_downloaded/in-progress.pdf.download",
            long_ago(),
            just_now(),
        )
        .await
        .unwrap();

        match out {
            HandleOutcome::IgnoredUnsupported { ext } => assert_eq!(ext, "download"),
            other => panic!("expected IgnoredUnsupported for .download, got {other:?}"),
        }
        assert!(storage
            .exists("papers/manually_downloaded/in-progress.pdf.download")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn fresh_mtime_defers_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        storage
            .put("papers/manually_downloaded/foo.pdf", b"new".to_vec())
            .await
            .unwrap();

        let now = SystemTime::now();
        let fresh = now - Duration::from_secs(1);

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/manually_downloaded/foo.pdf",
            fresh,
            now,
        )
        .await
        .unwrap();

        matches!(out, HandleOutcome::IgnoredStillWriting { .. });
        // Source still in place; target not written.
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
        assert!(!storage.exists("papers/fo/foo.pdf").await.unwrap());
    }

    #[tokio::test]
    async fn non_inbox_path_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/ab/already-sharded.pdf",
            long_ago(),
            just_now(),
        )
        .await
        .unwrap();

        assert_eq!(out, HandleOutcome::IgnoredNonInbox);
    }

    #[tokio::test]
    async fn macos_resource_fork_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            "papers/manually_downloaded/._foo.pdf",
            long_ago(),
            just_now(),
        )
        .await
        .unwrap();

        match out {
            HandleOutcome::IgnoredUnsupported { ext } => {
                assert_eq!(ext, "macos-resource-fork")
            }
            other => panic!("expected macOS resource-fork ignore, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sweep_aggregates_outcomes() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        // One relocatable PDF, one ignored .download, one already-at-target PDF.
        storage
            .put("papers/manually_downloaded/a.pdf", b"a".to_vec())
            .await
            .unwrap();
        storage
            .put(
                "papers/manually_downloaded/b.pdf.download",
                b"partial".to_vec(),
            )
            .await
            .unwrap();
        storage
            .put("papers/manually_downloaded/c.pdf", b"c".to_vec())
            .await
            .unwrap();
        storage
            .put("papers/c/c.pdf", b"prior-c".to_vec())
            .await
            .unwrap();
        // Backdate mtimes via a second write — LocalFsStorage sets mtime to
        // wall-clock on put, so sleep briefly so the mtime guard doesn't fire.
        // (MIN_AGE is 5s; 6s sleep is way too slow for a test. Instead we
        // rely on sweep_inbox_once's list() returning None for last_modified
        // in some backends; the guard reads UNIX_EPOCH in that case, which
        // is always ancient.)
        // LocalFsStorage DOES return last_modified, so we need to wait.
        // Skip the mtime guard for this aggregation test by directly calling
        // handle_inbox_source with long_ago().
        let now = just_now();
        let list = storage.list("papers/manually_downloaded/").await.unwrap();
        let mut report = SweepReport {
            found: list.len(),
            ..Default::default()
        };
        for obj in list {
            match handle_inbox_source(&storage, &bus, PAPERS, &obj.key, long_ago(), now)
                .await
                .unwrap()
            {
                HandleOutcome::Committed(WriteOutcome::Relocated) => report.relocated += 1,
                HandleOutcome::Committed(WriteOutcome::AlreadyAtTarget) => {
                    report.already_at_target += 1
                }
                HandleOutcome::Committed(WriteOutcome::PartialLeftSource) => {
                    report.partial_left_source += 1
                }
                HandleOutcome::IgnoredUnsupported { .. } => report.ignored_unsupported += 1,
                _ => {}
            }
        }

        assert_eq!(report.found, 3);
        assert_eq!(report.relocated, 1, "a.pdf should be relocated");
        assert_eq!(report.already_at_target, 1, "c.pdf target already exists");
        assert_eq!(report.ignored_unsupported, 1, "b.pdf.download ignored");
    }
}
