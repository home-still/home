use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, Utc};

use crate::logging::spool;
use crate::storage::Storage;

pub(crate) async fn run_shipper(
    spool_dir: PathBuf,
    storage: Arc<dyn Storage>,
    key_prefix: String,
    interval: Duration,
    delete_on_success: bool,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the first immediate tick so we don't race startup.
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                ship_once(&spool_dir, storage.as_ref(), &key_prefix, delete_on_success).await;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    // Final pass before exit.
                    ship_once(&spool_dir, storage.as_ref(), &key_prefix, delete_on_success).await;
                    break;
                }
            }
        }
    }
}

pub(crate) async fn ship_once(
    spool_dir: &Path,
    storage: &dyn Storage,
    key_prefix: &str,
    delete_on_success: bool,
) {
    let files = match spool::list_closed(spool_dir).await {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(error = %e, "log-shipper: list spool dir failed");
            return;
        }
    };
    for path in files {
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "log-shipper: read failed");
                continue;
            }
        };
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let now = Utc::now();
        let key = format!(
            "{prefix}{year:04}/{month:02}/{day:02}/{filename}",
            prefix = key_prefix,
            year = now.year(),
            month = now.month(),
            day = now.day(),
            filename = filename,
        );
        match storage.put(&key, bytes).await {
            Ok(()) => {
                if delete_on_success {
                    match tokio::fs::remove_file(&path).await {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "log-shipper: delete spool file failed",
                        ),
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "log-shipper: put failed; will retry");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::spool::{Spool, SpoolWriter};
    use crate::storage::{LocalFsStorage, ObjectMeta};
    use async_trait::async_trait;
    use std::io::Write as IoWrite;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn spool_to_storage_roundtrip() {
        let spool_tmp = tempfile::tempdir().unwrap();
        let storage_tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> =
            Arc::new(LocalFsStorage::new(storage_tmp.path().to_path_buf()));

        let spool = Spool::new(spool_tmp.path().to_path_buf()).unwrap();
        let mut writer = SpoolWriter::new(spool.clone());
        for i in 0..10 {
            writeln!(writer, r#"{{"n":{i}}}"#).unwrap();
        }
        writer.flush().unwrap();
        spool.rotate_now().unwrap();

        ship_once(spool_tmp.path(), storage.as_ref(), "hs-test/big/", true).await;

        // spool dir should now only have current.jsonl (empty)
        let leftover = spool::list_closed(spool_tmp.path()).await.unwrap();
        assert!(
            leftover.is_empty(),
            "expected no closed files after ship, got {leftover:?}",
        );

        // Storage should have exactly one object under the prefix.
        let listed = storage.list("hs-test/big/").await.unwrap();
        assert_eq!(listed.len(), 1, "expected 1 shipped object, got {listed:?}");
        let content = storage.get(&listed[0].key).await.unwrap();
        let text = String::from_utf8(content).unwrap();
        assert_eq!(text.lines().count(), 10);
    }

    #[tokio::test]
    async fn ship_failure_keeps_file_in_spool() {
        struct FailingStorage {
            puts: AtomicUsize,
        }
        #[async_trait]
        impl Storage for FailingStorage {
            async fn get(&self, _: &str) -> anyhow::Result<Vec<u8>> {
                unimplemented!()
            }
            async fn put(&self, _: &str, _: Vec<u8>) -> anyhow::Result<()> {
                self.puts.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("simulated PUT failure")
            }
            async fn head(&self, _: &str) -> anyhow::Result<Option<ObjectMeta>> {
                Ok(None)
            }
            async fn list(&self, _: &str) -> anyhow::Result<Vec<ObjectMeta>> {
                Ok(Vec::new())
            }
            async fn delete(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let spool_tmp = tempfile::tempdir().unwrap();
        let storage: Arc<dyn Storage> = Arc::new(FailingStorage {
            puts: AtomicUsize::new(0),
        });

        let spool = Spool::new(spool_tmp.path().to_path_buf()).unwrap();
        let mut writer = SpoolWriter::new(spool.clone());
        writeln!(writer, "line-one").unwrap();
        writer.flush().unwrap();
        spool.rotate_now().unwrap();

        ship_once(spool_tmp.path(), storage.as_ref(), "hs-test/big/", true).await;

        let leftover = spool::list_closed(spool_tmp.path()).await.unwrap();
        assert_eq!(
            leftover.len(),
            1,
            "file should remain in spool on PUT failure"
        );
    }
}
