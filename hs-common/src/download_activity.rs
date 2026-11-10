//! Live view of paper downloads in progress.
//!
//! A download is invisible to every storage-backed counter in the pipeline
//! panel until it finishes: [`paper::providers::downloader`] drains the whole
//! response into memory and then does a single `storage.put`, so there is no
//! partial object for a listing to observe grow. `downloaded_at` is stamped
//! afterwards too, so the History feed's `Download` rows also only appear once
//! the transfer is over.
//!
//! `hs paper download` has always shown per-file byte bars, because the batch
//! path passes an `on_progress` callback down to `download_by_url`. The MCP
//! `paper_download` tool — the path agents use — called `download_by_doi`,
//! which had no such parameter, so those downloads reported nothing at any
//! point.
//!
//! This registry is the one place both callers can publish to, and
//! `hs status` reads it through the status snapshot the MCP server builds.
//! It is deliberately process-local: the counters it feeds answer "is a
//! download happening right now", which is only ever true of the process
//! doing the downloading. A download that outlived its process is not
//! running, so there is nothing to recover.

use std::sync::{LazyLock, Mutex, MutexGuard};

/// One download the process is currently performing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActiveDownload {
    /// The storage stem — one identity per paper (`paper::stem::paper_stem`).
    pub stem: String,
    /// Best-known title; empty when the caller had only a DOI.
    pub title: String,
    /// Bytes received so far.
    pub bytes_downloaded: u64,
    /// `Content-Length` when the server declared one, else `None`.
    pub bytes_total: Option<u64>,
}

impl ActiveDownload {
    /// `1.2 MB / 8.4 MB`, or `1.2 MB` when the total is unknown.
    pub fn progress_text(&self) -> String {
        match self.bytes_total {
            Some(total) => format!(
                "{} / {}",
                crate::fmt::fmt_bytes(self.bytes_downloaded),
                crate::fmt::fmt_bytes(total)
            ),
            None => crate::fmt::fmt_bytes(self.bytes_downloaded),
        }
    }
}

static REGISTRY: LazyLock<Mutex<Vec<ActiveDownload>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Take the registry lock. A poisoned lock cannot be allowed to hide live
/// progress, and the guarded data is a plain `Vec` whose invariants are
/// restored by every write below, so recover the inner value rather than
/// panicking a status read.
fn lock() -> MutexGuard<'static, Vec<ActiveDownload>> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a download as started. Idempotent per `stem`: a retry replaces the
/// previous entry instead of showing the same paper twice.
pub fn start(stem: &str, title: &str) {
    let mut active = lock();
    active.retain(|d| d.stem != stem);
    active.push(ActiveDownload {
        stem: stem.to_string(),
        title: title.to_string(),
        bytes_downloaded: 0,
        bytes_total: None,
    });
}

/// Record bytes received for `stem`. A `stem` that was never started is
/// ignored: progress for a transfer this process does not track would be a
/// phantom row in the panel, which is worse than a missing one.
pub fn progress(stem: &str, bytes_downloaded: u64, bytes_total: Option<u64>) {
    let mut active = lock();
    if let Some(entry) = active.iter_mut().find(|d| d.stem == stem) {
        entry.bytes_downloaded = bytes_downloaded;
        // A server that declared a length only once: keep the first one.
        if entry.bytes_total.is_none() {
            entry.bytes_total = bytes_total;
        }
    }
}

/// Remove `stem` from the active set — on success, failure, or a skip.
pub fn finish(stem: &str) {
    lock().retain(|d| d.stem != stem);
}

/// The downloads in progress, in start order.
pub fn snapshot() -> Vec<ActiveDownload> {
    lock().clone()
}

/// How many downloads are in progress.
pub fn active_count() -> usize {
    lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests share one process-global registry, so they must not run
    /// concurrently with each other.
    fn guard() -> MutexGuard<'static, ()> {
        static SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear() {
        lock().clear();
    }

    #[test]
    fn a_started_download_is_visible_with_its_bytes() {
        let _serial = guard();
        clear();
        start("10.1234_abc", "A Paper");
        progress("10.1234_abc", 1200, Some(4800));

        let active = snapshot();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].stem, "10.1234_abc");
        assert_eq!(active[0].bytes_downloaded, 1200);
        assert_eq!(active[0].bytes_total, Some(4800));
        clear();
    }

    #[test]
    fn finishing_removes_the_download() {
        let _serial = guard();
        clear();
        start("10.1234_abc", "A Paper");
        finish("10.1234_abc");
        assert!(snapshot().is_empty());
        assert_eq!(active_count(), 0);
        clear();
    }

    #[test]
    fn progress_for_an_untracked_stem_adds_nothing() {
        let _serial = guard();
        clear();
        progress("10.9999_ghost", 500, Some(1000));
        assert!(
            snapshot().is_empty(),
            "progress must not invent a row for a transfer this process never started"
        );
        clear();
    }

    #[test]
    fn restarting_the_same_stem_does_not_duplicate_it() {
        let _serial = guard();
        clear();
        start("10.1234_abc", "A Paper");
        progress("10.1234_abc", 900, Some(4000));
        // A retry starts the same paper again.
        start("10.1234_abc", "A Paper");
        let active = snapshot();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].bytes_downloaded, 0, "a restart resets progress");
        clear();
    }

    #[test]
    fn a_declared_total_is_kept_when_later_chunks_omit_it() {
        let _serial = guard();
        clear();
        start("10.1234_abc", "A Paper");
        progress("10.1234_abc", 100, Some(5000));
        progress("10.1234_abc", 200, None);
        let active = snapshot();
        assert_eq!(active[0].bytes_total, Some(5000));
        assert_eq!(active[0].bytes_downloaded, 200);
        clear();
    }

    #[test]
    fn progress_text_shows_a_ratio_only_when_the_total_is_known() {
        let known = ActiveDownload {
            stem: "s".into(),
            title: "t".into(),
            bytes_downloaded: 1_048_576,
            bytes_total: Some(2_097_152),
        };
        assert_eq!(known.progress_text(), "1 MB / 2 MB");

        let unknown = ActiveDownload {
            bytes_total: None,
            ..known
        };
        assert_eq!(unknown.progress_text(), "1 MB");
    }
}
