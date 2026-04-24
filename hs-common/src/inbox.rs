//! Format-agnostic commit primitive for the client-side inbox watcher.
//!
//! A user drops a file into `papers/manually_downloaded/<name>.<ext>` (usually
//! via an rclone-NFS mount to the Garage S3 bucket). The watcher reads it,
//! optionally transforms it (EPUB → HTML), then calls into this module to:
//!
//! 1. Write the bytes to the canonical sharded key under `papers/<shard>/...`
//! 2. Publish a `papers.ingested` NATS event so the server-side scribe picks it up
//! 3. Delete the original drop-zone copy
//!
//! Ordering is intentional: the target must be durable before the source is
//! deleted; the publish happens in between so duplicate events are benign
//! (scribe's event subscriber dedups via `storage.exists(md_key)`).

use crate::event_bus::EventBus;
use crate::storage::Storage;

/// File extensions the inbox sweeper (and the `Inbox` pipeline count that
/// surfaces it in `hs status`) treat as first-class ingestable sources.
/// Anything else under `papers/manually_downloaded/` is ignored.
pub const INBOX_SUPPORTED_EXTS: &[&str] = &["pdf", "html", "htm", "epub"];

/// True iff a filename dropped into `papers/manually_downloaded/` is a
/// convertible paper source. Shared by the sweeper's per-file dispatch
/// and by `hs status`'s inbox-queue counter, so the dashboard number can
/// never lie about what the daemon will actually relocate on the next tick.
///
/// Rejects:
/// - Empty names and `._*` macOS AppleDouble resource forks.
/// - Names with no `.` (no extension).
/// - Known in-flight / transient suffixes: `.download`, `.part`,
///   `.crdownload`, `.tmp` (browser downloads still writing).
/// - E-book formats we don't support end-to-end: `.azw3`, `.azw`, `.mobi`.
///
/// Accepts only the case-insensitive extensions in [`INBOX_SUPPORTED_EXTS`].
pub fn is_inbox_candidate_filename(filename: &str) -> bool {
    if filename.is_empty() || filename.starts_with("._") {
        return false;
    }
    let Some((_stem, ext)) = filename.rsplit_once('.') else {
        return false;
    };
    let ext_lc = ext.to_ascii_lowercase();
    // Fast reject: transient / unsupported extensions the sweeper already
    // blacklists at `crates/hs/src/scribe_inbox.rs:97-101`.
    if matches!(
        ext_lc.as_str(),
        "download" | "part" | "crdownload" | "tmp" | "azw3" | "azw" | "mobi"
    ) {
        return false;
    }
    INBOX_SUPPORTED_EXTS
        .iter()
        .any(|allowed| *allowed == ext_lc)
}

/// Outcome of a single relocate attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Target put + NATS publish + source delete all succeeded.
    Relocated,
    /// Target already existed (another client got there first, or the source
    /// is a stale duplicate). The source was deleted; put/publish not called.
    AlreadyAtTarget,
    /// Target put + publish succeeded, but deleting the source failed.
    /// Next sweep will see `AlreadyAtTarget` and clean the orphan source.
    PartialLeftSource,
}

/// Write `bytes` to `target_key`, publish `papers.ingested`, delete `source_key`.
///
/// See the module docs for the ordering rationale. On any storage/bus failure
/// after the put, the function still tries to delete the source — the target
/// is in place and further progress is possible; the alternative (leaving the
/// source) just means the next sweep retries.
pub async fn write_target_and_publish(
    storage: &dyn Storage,
    bus: &dyn EventBus,
    source_key: &str,
    target_key: &str,
    bytes: Vec<u8>,
) -> anyhow::Result<WriteOutcome> {
    // Fast-path: target already present. This is the duplicate-drop case
    // and the recovery path for a prior PartialLeftSource.
    if storage.exists(target_key).await.unwrap_or(false) {
        // Best-effort source cleanup. 404 is fine.
        if let Err(e) = storage.delete(source_key).await {
            tracing::warn!(src = source_key, error = %e, "delete source after AlreadyAtTarget failed");
        }
        return Ok(WriteOutcome::AlreadyAtTarget);
    }

    // Target put is load-bearing. Any failure here aborts without touching the
    // source, so the next sweep can retry from scratch.
    storage
        .put(target_key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("put target {target_key}: {e}"))?;

    // Publish is best-effort: if NATS is down, the target is still in place.
    // A later `catalog_repair` forward sweep will synthesize a row, and the
    // pipeline catches up on the next normal event or reconcile cycle.
    let payload = serde_json::json!({
        "key": target_key,
        "source": "inbox",
    });
    if let Err(e) = bus
        .publish(
            "papers.ingested",
            &serde_json::to_vec(&payload).unwrap_or_default(),
        )
        .await
    {
        tracing::warn!(
            target = target_key,
            error = %e,
            "papers.ingested publish failed (target in place, will recover via catalog_repair)",
        );
    }

    // Delete source. Failure → PartialLeftSource; the next sweep hits the
    // fast-path above and cleans up the orphan.
    match storage.delete(source_key).await {
        Ok(()) => Ok(WriteOutcome::Relocated),
        Err(e) => {
            tracing::warn!(
                src = source_key,
                error = %e,
                "delete source after successful put+publish failed; will be cleaned next sweep"
            );
            Ok(WriteOutcome::PartialLeftSource)
        }
    }
}

#[cfg(test)]
mod candidate_filter_tests {
    use super::is_inbox_candidate_filename;

    #[test]
    fn accepts_supported_extensions_case_insensitively() {
        for name in [
            "foo.pdf",
            "foo.PDF",
            "bar.html",
            "bar.HTM",
            "baz.epub",
            "W12345.pdf",
        ] {
            assert!(is_inbox_candidate_filename(name), "expected accept: {name}");
        }
    }

    #[test]
    fn rejects_macos_resource_forks_and_empty() {
        for name in ["", "._foo.pdf", "._Archive.pdf", "._", "._.epub"] {
            assert!(
                !is_inbox_candidate_filename(name),
                "expected reject: {name}"
            );
        }
    }

    #[test]
    fn rejects_extensionless_and_transient_suffixes() {
        for name in [
            "README",
            "no_dot",
            "foo.pdf.download",
            "foo.pdf.part",
            "foo.pdf.crdownload",
            "foo.tmp",
            "book.azw3",
            "book.mobi",
            "book.azw",
        ] {
            assert!(
                !is_inbox_candidate_filename(name),
                "expected reject: {name}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_bus::NoOpBus;
    use crate::storage::LocalFsStorage;

    fn mk_env() -> (tempfile::TempDir, LocalFsStorage, NoOpBus) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = LocalFsStorage::new(tmp.path());
        (tmp, storage, NoOpBus)
    }

    #[tokio::test]
    async fn standard_relocate() {
        let (_tmp, storage, bus) = mk_env();
        storage
            .put("papers/manually_downloaded/foo.pdf", b"pdfbytes".to_vec())
            .await
            .unwrap();

        let out = write_target_and_publish(
            &storage,
            &bus,
            "papers/manually_downloaded/foo.pdf",
            "papers/fo/foo.pdf",
            b"pdfbytes".to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(out, WriteOutcome::Relocated);
        assert_eq!(storage.get("papers/fo/foo.pdf").await.unwrap(), b"pdfbytes");
        assert!(!storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn target_already_exists_deletes_source_only() {
        let (_tmp, storage, bus) = mk_env();
        storage
            .put("papers/manually_downloaded/foo.pdf", b"new".to_vec())
            .await
            .unwrap();
        storage
            .put("papers/fo/foo.pdf", b"already-here".to_vec())
            .await
            .unwrap();

        let out = write_target_and_publish(
            &storage,
            &bus,
            "papers/manually_downloaded/foo.pdf",
            "papers/fo/foo.pdf",
            b"new".to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(out, WriteOutcome::AlreadyAtTarget);
        // Target is unchanged — we never overwrite on AlreadyAtTarget.
        assert_eq!(
            storage.get("papers/fo/foo.pdf").await.unwrap(),
            b"already-here"
        );
        // Source is cleaned up.
        assert!(!storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn publish_failure_still_deletes_source() {
        // NoOpBus never fails, so this test uses an explicit failing bus.
        struct FailingBus;
        #[async_trait::async_trait]
        impl EventBus for FailingBus {
            async fn publish(&self, _subject: &str, _payload: &[u8]) -> anyhow::Result<()> {
                anyhow::bail!("bus is down")
            }
            async fn consume(
                &self,
                _spec: &crate::event_bus::ConsumerSpec,
            ) -> anyhow::Result<crate::event_bus::EventStream> {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
        let (_tmp, storage, _) = mk_env();
        storage
            .put("papers/manually_downloaded/foo.pdf", b"x".to_vec())
            .await
            .unwrap();

        let out = write_target_and_publish(
            &storage,
            &FailingBus,
            "papers/manually_downloaded/foo.pdf",
            "papers/fo/foo.pdf",
            b"x".to_vec(),
        )
        .await
        .unwrap();

        // Target is written, source is deleted — we don't strand data over
        // a transient NATS outage.
        assert_eq!(out, WriteOutcome::Relocated);
        assert!(storage.exists("papers/fo/foo.pdf").await.unwrap());
        assert!(!storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn delete_source_failure_returns_partial() {
        // LocalFsStorage treats missing files as Ok on delete, so we can't
        // trivially make delete fail. Use a wrapper that forwards everything
        // except delete, which always errors.
        struct DeleteFailsStorage<S>(S);
        #[async_trait::async_trait]
        impl<S: Storage> Storage for DeleteFailsStorage<S> {
            async fn get(&self, k: &str) -> anyhow::Result<Vec<u8>> {
                self.0.get(k).await
            }
            async fn put(&self, k: &str, b: Vec<u8>) -> anyhow::Result<()> {
                self.0.put(k, b).await
            }
            async fn head(&self, k: &str) -> anyhow::Result<Option<crate::storage::ObjectMeta>> {
                self.0.head(k).await
            }
            async fn list(&self, p: &str) -> anyhow::Result<Vec<crate::storage::ObjectMeta>> {
                self.0.list(p).await
            }
            async fn delete(&self, _k: &str) -> anyhow::Result<()> {
                anyhow::bail!("simulated s3 delete flake")
            }
        }
        let (_tmp, inner, bus) = mk_env();
        inner
            .put("papers/manually_downloaded/foo.pdf", b"x".to_vec())
            .await
            .unwrap();
        let storage = DeleteFailsStorage(inner);

        let out = write_target_and_publish(
            &storage,
            &bus,
            "papers/manually_downloaded/foo.pdf",
            "papers/fo/foo.pdf",
            b"x".to_vec(),
        )
        .await
        .unwrap();

        assert_eq!(out, WriteOutcome::PartialLeftSource);
        // Target is in place; source still exists because delete failed.
        assert!(storage.exists("papers/fo/foo.pdf").await.unwrap());
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn put_failure_preserves_source() {
        struct PutFailsStorage<S>(S);
        #[async_trait::async_trait]
        impl<S: Storage> Storage for PutFailsStorage<S> {
            async fn get(&self, k: &str) -> anyhow::Result<Vec<u8>> {
                self.0.get(k).await
            }
            async fn put(&self, _k: &str, _b: Vec<u8>) -> anyhow::Result<()> {
                anyhow::bail!("simulated s3 put flake")
            }
            async fn head(&self, k: &str) -> anyhow::Result<Option<crate::storage::ObjectMeta>> {
                self.0.head(k).await
            }
            async fn list(&self, p: &str) -> anyhow::Result<Vec<crate::storage::ObjectMeta>> {
                self.0.list(p).await
            }
            async fn delete(&self, k: &str) -> anyhow::Result<()> {
                self.0.delete(k).await
            }
        }
        let (_tmp, inner, bus) = mk_env();
        inner
            .put("papers/manually_downloaded/foo.pdf", b"x".to_vec())
            .await
            .unwrap();
        let storage = PutFailsStorage(inner);

        let result = write_target_and_publish(
            &storage,
            &bus,
            "papers/manually_downloaded/foo.pdf",
            "papers/fo/foo.pdf",
            b"x".to_vec(),
        )
        .await;

        assert!(result.is_err(), "put failure must bubble up");
        // Source is untouched — no data loss.
        assert!(storage
            .exists("papers/manually_downloaded/foo.pdf")
            .await
            .unwrap());
    }
}
