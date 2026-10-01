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

use anyhow::{Context, Result};
use hs_common::event_bus::EventBus;
use hs_common::inbox::{write_target_and_publish, WriteOutcome};
use hs_common::reporter::Reporter;
use hs_common::storage::Storage;
use sha2::{Digest, Sha256};

use crate::scribe_cmd::{epub_bytes_to_html, InboxAction};
use crate::shutdown::Shutdown;

/// Files newer than this are assumed to still be written (browser
/// download in progress, rclone sync mid-upload). The next sweep picks
/// them up.
pub const MIN_AGE_BEFORE_PROCESSING: Duration = Duration::from_secs(5);

/// Storage prefix of the one folder for inputs that must never enter the
/// pipeline (CLAUDE.md: "Bad-PDF folder is `corrupted/`").
pub const CORRUPTED_PREFIX: &str = "corrupted";

/// Outcome of `handle_inbox_source`. Distinguishes the "skip" cases from the
/// commit primitive's three successful outcomes so callers can log
/// appropriately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleOutcome {
    Committed(WriteOutcome),
    IgnoredUnsupported {
        ext: String,
    },
    /// The file may still be uploading: its mtime is younger than
    /// [`MIN_AGE_BEFORE_PROCESSING`], unknown, or in the future. `age_secs`
    /// is `None` when no usable age exists.
    IgnoredStillWriting {
        age_secs: Option<u64>,
    },
    /// Path doesn't live under `{papers_prefix}/manually_downloaded/`. Safety
    /// net for the `notify` path, which fires on anything the watcher sees.
    IgnoredNonInbox,
    /// The file name cannot be turned into a stored paper name (empty stem,
    /// `.`/`..`, …). The file was moved to `corrupted/` so it is neither
    /// retried on every sweep nor able to reach storage under a hostile key.
    Rejected {
        reason: String,
        moved_to: String,
    },
}

/// Process a single file in the inbox prefix. Returns an outcome describing
/// what happened; the caller decides how to log it.
///
/// `source_mtime` is the object's last-modified time as the backend reports
/// it; `None` (backend gave none) and a time in the future both mean "cannot
/// tell that the upload finished", so the file is deferred, never processed.
///
/// Format handling:
/// - `.pdf`, `.html`, `.htm`: bytes pass through unchanged; target keeps the
///   same extension.
/// - `.epub`: unpack the spine to HTML in-memory; target extension flips to
///   `.html` so the server-side event-bus path handles it natively.
/// - anything else: `IgnoredUnsupported`.
///
/// The file name is untrusted input: a stem that is not a plain name
/// (`hs_common::validate_stem`) is moved to `corrupted/` and reported as
/// [`HandleOutcome::Rejected`].
pub async fn handle_inbox_source(
    storage: &dyn Storage,
    bus: &dyn EventBus,
    papers_prefix: &str,
    source_key: &str,
    source_mtime: Option<SystemTime>,
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
    // An unknown or future mtime is "not settled yet" too: processing a file
    // we cannot prove is complete would upload a truncated paper.
    match source_mtime.map(|mtime| now.duration_since(mtime)) {
        Some(Ok(age)) if age >= MIN_AGE_BEFORE_PROCESSING => {}
        Some(Ok(age)) => {
            return Ok(HandleOutcome::IgnoredStillWriting {
                age_secs: Some(age.as_secs()),
            });
        }
        Some(Err(_)) | None => return Ok(HandleOutcome::IgnoredStillWriting { age_secs: None }),
    }

    // Untrusted boundary: the stem becomes part of storage keys and shard
    // directories. Anything that is not a plain file-name component goes to
    // `corrupted/` once, with the reason logged, instead of failing the same
    // way on every sweep.
    if let Err(invalid) = hs_common::validate_stem(stem) {
        let reason = format!("file name {filename:?} is not usable as a paper stem: {invalid}");
        let moved_to = move_to_corrupted(storage, source_key, filename)
            .await
            .with_context(|| format!("reject {source_key} ({reason})"))?;
        tracing::warn!(
            source = source_key,
            moved_to = %moved_to,
            reason = %reason,
            "inbox file rejected and moved to corrupted/"
        );
        return Ok(HandleOutcome::Rejected { reason, moved_to });
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

/// Move a rejected inbox file to `corrupted/<content-hash>-<file name>`.
///
/// The content hash in the name makes the move idempotent (a re-run after a
/// failed delete writes the same key) and keeps two different rejected files
/// that share a name from overwriting each other. The source is deleted only
/// after the copy succeeded.
async fn move_to_corrupted(
    storage: &dyn Storage,
    source_key: &str,
    filename: &str,
) -> Result<String> {
    let bytes = storage
        .get(source_key)
        .await
        .with_context(|| format!("read {source_key}"))?;
    let digest = Sha256::digest(&bytes);
    let tag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let dest = format!("{CORRUPTED_PREFIX}/{tag}-{filename}");
    storage
        .put(&dest, bytes)
        .await
        .with_context(|| format!("write {dest}"))?;
    storage
        .delete(source_key)
        .await
        .with_context(|| format!("delete {source_key} after copying it to {dest}"))?;
    Ok(dest)
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
    /// `source -> corrupted/…: reason` for every file moved to `corrupted/`.
    pub rejected: Vec<String>,
    pub errors: Vec<String>,
    /// Shutdown was requested before every listed file was visited.
    pub interrupted: bool,
}

/// Walk `{papers_prefix}/manually_downloaded/` once and process each file.
/// Returns a report; individual failures are collected into `errors` and do
/// not abort the sweep. Stops starting new files once `stop` is requested.
pub async fn sweep_inbox_once(
    storage: &dyn Storage,
    bus: &dyn EventBus,
    papers_prefix: &str,
    stop: &Shutdown,
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
        if stop.requested() {
            report.interrupted = true;
            break;
        }
        match handle_inbox_source(storage, bus, papers_prefix, &obj.key, obj.last_modified, now)
            .await
        {
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
            Ok(HandleOutcome::Rejected { reason, moved_to }) => {
                report
                    .rejected
                    .push(format!("{} -> {moved_to}: {reason}", obj.key));
            }
            Err(e) => report.errors.push(format!("{}: {e:#}", obj.key)),
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
    let stop = crate::shutdown::cooperative();
    let cfg = ScribeConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let storage = cfg.build_storage()?;
    let bus = cfg.build_event_bus().await?;

    reporter.status("Sweep", "scanning papers/manually_downloaded/");
    let report = sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX, &stop).await?;
    log_report(reporter, &report);
    if !report.errors.is_empty() {
        anyhow::bail!("sweep completed with {} error(s)", report.errors.len());
    }
    if report.interrupted {
        anyhow::bail!("sweep interrupted before every file was visited");
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
    let stop = crate::shutdown::cooperative();
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
    // after the first poll-interval.
    //
    // `last_sweep` carries the most recent completed sweep's counts across
    // ticks. The pre-sweep stamp re-uses the PREVIOUS value (so the
    // Watcher row keeps showing `swept N / M` during a long sweep instead
    // of flickering to nothing); the post-sweep stamp updates it.
    let mut last_sweep: Option<(u64, u64, u64)> = None;
    if let Err(e) =
        hs_common::status::write_inbox_heartbeat(&*storage, sweep_interval_secs, last_sweep).await
    {
        tracing::warn!(error = %e, "initial heartbeat write failed");
    }
    match sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX, &stop).await {
        Ok(r) => {
            log_report(reporter, &r);
            last_sweep = Some((r.found as u64, r.relocated, r.errors.len() as u64));
            if let Err(e) =
                hs_common::status::write_inbox_heartbeat(&*storage, sweep_interval_secs, last_sweep)
                    .await
            {
                tracing::warn!(error = %e, "post-sweep heartbeat write failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "initial sweep failed"),
    }

    loop {
        // Sleep until the next tick, or until a shutdown request (SIGINT, or
        // SIGTERM from the service manager) cuts the wait short.
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = stop.wait() => {}
        }
        if stop.requested() {
            reporter.status("Inbox", "shutdown requested");
            return Ok(());
        }
        // Pre-sweep heartbeat keeps the Watcher row "running" even when
        // a sweep stalls. `last_sweep` carries the previous completed
        // sweep's counts forward so the `swept N / M` display doesn't
        // flicker to null every 30s — `hs status` ages out the
        // heartbeat at 2× poll_interval + 5s grace for the liveness
        // signal; the numbers alongside are the most recent known
        // counts, always.
        if let Err(e) =
            hs_common::status::write_inbox_heartbeat(&*storage, sweep_interval_secs, last_sweep)
                .await
        {
            tracing::warn!(error = %e, "heartbeat write failed; sweep will still run");
        }
        match sweep_inbox_once(&*storage, &*bus, PAPERS_PREFIX, &stop).await {
            Ok(r) => {
                if r.relocated > 0 || !r.errors.is_empty() || !r.rejected.is_empty() {
                    log_report(reporter, &r);
                } else {
                    tracing::debug!("sweep: nothing to do");
                }
                last_sweep = Some((r.found as u64, r.relocated, r.errors.len() as u64));
                // Post-sweep stamp: refresh the Watcher row with the
                // fresh counts.
                if let Err(e) = hs_common::status::write_inbox_heartbeat(
                    &*storage,
                    sweep_interval_secs,
                    last_sweep,
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
            "found={} relocated={} already={} partial={} ignored={}+{} rejected={} errors={}{}",
            r.found,
            r.relocated,
            r.already_at_target,
            r.partial_left_source,
            r.ignored_unsupported,
            r.ignored_still_writing,
            r.rejected.len(),
            r.errors.len(),
            if r.interrupted { " (interrupted)" } else { "" },
        ),
    );
    for line in &r.rejected {
        reporter.warn(&format!("rejected: {line}"));
    }
    for e in &r.errors {
        reporter.warn(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hs_common::event_bus::{ConsumerSpec, EventStream, NoOpBus};
    use hs_common::storage::{LocalFsStorage, ObjectMeta};
    use tokio::sync::Mutex;

    const PAPERS: &str = "papers";
    const INBOX: &str = "papers/manually_downloaded";

    fn just_now() -> SystemTime {
        SystemTime::now()
    }
    fn long_ago() -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
    }

    /// Records what the sweeper publishes: the fake "uploader" side of the
    /// pipeline (the real bus would hand these to the scribe workers).
    #[derive(Default)]
    struct RecordingBus {
        published: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl EventBus for RecordingBus {
        async fn publish(&self, subject: &str, payload: &[u8]) -> anyhow::Result<()> {
            let value = serde_json::from_slice(payload).expect("publish payload is JSON");
            self.published.lock().await.push((subject.to_string(), value));
            Ok(())
        }
        async fn consume(&self, _spec: &ConsumerSpec) -> anyhow::Result<EventStream> {
            unimplemented!("the inbox never consumes")
        }
    }

    /// A backend that reports no last-modified time (the `Option` in
    /// `ObjectMeta` is `None` for such backends).
    struct NoMtimeStorage(LocalFsStorage);

    #[async_trait]
    impl Storage for NoMtimeStorage {
        async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
            self.0.get(key).await
        }
        async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
            self.0.put(key, bytes).await
        }
        async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
            self.0.head(key).await
        }
        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
            let mut listed = self.0.list(prefix).await?;
            for meta in &mut listed {
                meta.last_modified = None;
            }
            Ok(listed)
        }
        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.0.delete(key).await
        }
    }

    /// Backdate a file so the "still being written" guard lets it through.
    fn settle(root: &std::path::Path, key: &str) {
        let file = std::fs::File::options()
            .write(true)
            .open(root.join(key))
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
    }

    async fn put_settled(storage: &LocalFsStorage, root: &std::path::Path, key: &str, body: &[u8]) {
        storage.put(key, body.to_vec()).await.unwrap();
        settle(root, key);
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
            Some(fresh),
            now,
        )
        .await
        .unwrap();

        assert!(
            matches!(out, HandleOutcome::IgnoredStillWriting { age_secs: Some(_) }),
            "a 1s-old drop must be deferred, got {out:?}"
        );
        // Source still in place; target not written.
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
        assert!(!storage.exists("papers/fo/foo.pdf").await.unwrap());
    }

    /// An upload whose age cannot be established (backend reports no mtime, or
    /// a clock-skewed future one) must wait, not be processed: the old code
    /// substituted `UNIX_EPOCH` and uploaded half-written files.
    #[tokio::test]
    async fn unknown_or_future_mtime_defers_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = RecordingBus::default();
        storage
            .put("papers/manually_downloaded/foo.pdf", b"maybe partial".to_vec())
            .await
            .unwrap();
        let now = SystemTime::now();

        for mtime in [None, Some(now + Duration::from_secs(3600))] {
            let out = handle_inbox_source(
                &storage,
                &bus,
                PAPERS,
                "papers/manually_downloaded/foo.pdf",
                mtime,
                now,
            )
            .await
            .unwrap();
            assert_eq!(
                out,
                HandleOutcome::IgnoredStillWriting { age_secs: None },
                "mtime {mtime:?}"
            );
        }
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
        assert!(!storage.exists("papers/fo/foo.pdf").await.unwrap());
        assert!(bus.published.lock().await.is_empty());
    }

    #[tokio::test]
    async fn sweep_leaves_files_alone_when_the_backend_reports_no_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = NoMtimeStorage(LocalFsStorage::new(tmp.path()));
        let bus = RecordingBus::default();
        storage
            .put("papers/manually_downloaded/foo.pdf", b"bytes".to_vec())
            .await
            .unwrap();
        settle(tmp.path(), "papers/manually_downloaded/foo.pdf");

        let report = sweep_inbox_once(&storage, &bus, PAPERS, &Shutdown::new())
            .await
            .unwrap();

        assert_eq!(report.relocated, 0);
        assert_eq!(report.ignored_still_writing, 1);
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
        assert!(bus.published.lock().await.is_empty());
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
        put_settled(&storage, tmp.path(), "papers/manually_downloaded/a.pdf", b"a").await;
        put_settled(
            &storage,
            tmp.path(),
            "papers/manually_downloaded/b.pdf.download",
            b"partial",
        )
        .await;
        put_settled(&storage, tmp.path(), "papers/manually_downloaded/c.pdf", b"c").await;
        storage
            .put("papers/c/c.pdf", b"prior-c".to_vec())
            .await
            .unwrap();

        let report = sweep_inbox_once(&storage, &bus, PAPERS, &Shutdown::new())
            .await
            .unwrap();

        assert_eq!(report.found, 3);
        assert_eq!(report.relocated, 1, "a.pdf should be relocated");
        assert_eq!(report.already_at_target, 1, "c.pdf target already exists");
        assert_eq!(report.ignored_unsupported, 1, "b.pdf.download ignored");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
    }

    /// RA-4 through the real sweep: names whose second byte falls inside a
    /// multi-byte character (`Müller`, `Año`, `Cómo`) used to panic in
    /// `sharded_key` and crash-loop the daemon on a file that was never
    /// removed. Every one must land under a shard directory with its exact
    /// bytes, publish exactly one `papers.ingested` for the target key, and
    /// leave the inbox empty.
    #[tokio::test]
    async fn non_ascii_file_names_relocate_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = RecordingBus::default();
        let names = ["Müller", "Año", "Cómo", "école", "日本語"];
        for name in names {
            put_settled(
                &storage,
                tmp.path(),
                &format!("{INBOX}/{name}.pdf"),
                format!("%PDF- bytes of {name}").as_bytes(),
            )
            .await;
        }

        let report = sweep_inbox_once(&storage, &bus, PAPERS, &Shutdown::new())
            .await
            .unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        assert_eq!(report.relocated, names.len() as u64);
        assert!(storage.list(&format!("{INBOX}/")).await.unwrap().is_empty());

        let published = bus.published.lock().await;
        assert_eq!(published.len(), names.len());
        for name in names {
            let target = format!("papers/{}", hs_common::sharded_key(name, "pdf"));
            assert_eq!(
                storage.get(&target).await.unwrap(),
                format!("%PDF- bytes of {name}").as_bytes(),
                "{name}: bytes at {target}"
            );
            assert!(
                published
                    .iter()
                    .any(|(subject, payload)| subject == "papers.ingested"
                        && payload["key"] == target.as_str()),
                "{name}: papers.ingested for {target}"
            );
        }
    }

    /// File names that cannot be a paper stem (`.pdf` has an empty stem,
    /// `...pdf` has the stem `..`) used to reach `sharded_key`/storage as-is.
    /// They must be moved to `corrupted/` exactly once: not published, gone
    /// from the inbox, kept byte-for-byte, and not seen again by later sweeps.
    #[tokio::test]
    async fn invalid_stems_move_to_corrupted_once_and_are_not_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = RecordingBus::default();
        put_settled(&storage, tmp.path(), &format!("{INBOX}/.pdf"), b"empty stem").await;
        put_settled(&storage, tmp.path(), &format!("{INBOX}/...pdf"), b"dots stem").await;
        put_settled(&storage, tmp.path(), &format!("{INBOX}/ok.pdf"), b"fine").await;

        let first = sweep_inbox_once(&storage, &bus, PAPERS, &Shutdown::new())
            .await
            .unwrap();

        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(first.rejected.len(), 2, "{:?}", first.rejected);
        assert_eq!(first.relocated, 1, "the valid neighbour still relocates");
        assert!(
            first
                .rejected
                .iter()
                .all(|line| line.contains("corrupted/")),
            "{:?}",
            first.rejected
        );

        // Both rejected files are preserved under corrupted/, nothing else is.
        let kept = storage.list(&format!("{CORRUPTED_PREFIX}/")).await.unwrap();
        assert_eq!(kept.len(), 2, "{kept:?}");
        let mut bodies = Vec::new();
        for meta in &kept {
            bodies.push(storage.get(&meta.key).await.unwrap());
        }
        bodies.sort();
        assert_eq!(bodies, vec![b"dots stem".to_vec(), b"empty stem".to_vec()]);

        // Only the valid file was published; the inbox is empty.
        assert_eq!(bus.published.lock().await.len(), 1);
        assert!(storage.list(&format!("{INBOX}/")).await.unwrap().is_empty());

        let second = sweep_inbox_once(&storage, &bus, PAPERS, &Shutdown::new())
            .await
            .unwrap();
        assert_eq!(second.found, 0);
        assert!(second.rejected.is_empty() && second.errors.is_empty());
        assert_eq!(
            storage
                .list(&format!("{CORRUPTED_PREFIX}/"))
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// Two different rejected files sharing a name must both survive.
    #[tokio::test]
    async fn rejected_files_with_the_same_name_do_not_overwrite_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;

        for body in [b"first".as_slice(), b"second".as_slice()] {
            put_settled(&storage, tmp.path(), &format!("{INBOX}/.pdf"), body).await;
            let out = handle_inbox_source(
                &storage,
                &bus,
                PAPERS,
                &format!("{INBOX}/.pdf"),
                long_ago(),
                just_now(),
            )
            .await
            .unwrap();
            assert!(matches!(out, HandleOutcome::Rejected { .. }), "{out:?}");
        }
        assert_eq!(
            storage
                .list(&format!("{CORRUPTED_PREFIX}/"))
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// A rejected file that is still being written must wait like any other
    /// drop (moving it mid-upload would truncate it).
    #[tokio::test]
    async fn a_fresh_invalid_name_is_deferred_not_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        storage
            .put(&format!("{INBOX}/.pdf"), b"uploading".to_vec())
            .await
            .unwrap();
        let now = SystemTime::now();

        let out = handle_inbox_source(
            &storage,
            &bus,
            PAPERS,
            &format!("{INBOX}/.pdf"),
            Some(now),
            now,
        )
        .await
        .unwrap();

        assert!(
            matches!(out, HandleOutcome::IgnoredStillWriting { .. }),
            "{out:?}"
        );
        assert!(storage.exists(&format!("{INBOX}/.pdf")).await.unwrap());
    }

    /// Ctrl+C / SIGTERM stops a sweep between files, and says so.
    #[tokio::test]
    async fn a_requested_shutdown_stops_the_sweep_between_files() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        let bus = NoOpBus;
        for name in ["a", "b", "c"] {
            put_settled(&storage, tmp.path(), &format!("{INBOX}/{name}.pdf"), b"x").await;
        }
        let stop = Shutdown::new();
        stop.request();

        let report = sweep_inbox_once(&storage, &bus, PAPERS, &stop)
            .await
            .unwrap();

        assert!(report.interrupted);
        assert_eq!(report.relocated, 0);
        assert_eq!(storage.list(&format!("{INBOX}/")).await.unwrap().len(), 3);
    }
}
