use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[cfg(feature = "storage-s3")]
pub mod s3;

#[cfg(feature = "storage-s3")]
pub use s3::S3Storage;

pub mod config;
pub use config::{Backend, StorageConfig};

/// Walk the cause chain of `err` and return true if any wrapped error
/// indicates "object not found in storage." Used by event handlers to
/// classify a `get(...) failed` as Permanent — the bytes don't exist
/// and re-delivering the event will never make them appear.
///
/// Recognizes both backends:
/// - S3 / object_store: `object_store::Error::NotFound { .. }`
/// - LocalFs: `std::io::Error` with `ErrorKind::NotFound`
pub fn is_not_found(err: &anyhow::Error) -> bool {
    for cause in err.chain() {
        #[cfg(feature = "storage-s3")]
        if let Some(oe) = cause.downcast_ref::<object_store::Error>() {
            if matches!(oe, object_store::Error::NotFound { .. }) {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::NotFound {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<SystemTime>,
    pub etag: Option<String>,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()>;
    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>>;
    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>>;
    async fn delete(&self, key: &str) -> anyhow::Result<()>;

    async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.head(key).await?.is_some())
    }

    /// Provision any container the backend needs before writes succeed
    /// (e.g. an S3 bucket). Default is a noop — only S3 overrides.
    /// Idempotent; callers can call on every startup.
    async fn ensure_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct LocalFsStorage {
    root: PathBuf,
}

impl LocalFsStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn key_from(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
    }
}

#[async_trait]
impl Storage for LocalFsStorage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = self.resolve(key);
        Ok(tokio::fs::read(&path).await?)
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let path = self.resolve(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        let path = self.resolve(key);
        match tokio::fs::metadata(&path).await {
            Ok(md) if md.is_file() => Ok(Some(ObjectMeta {
                key: key.to_string(),
                size: md.len(),
                last_modified: md.modified().ok(),
                etag: None,
            })),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let start = self.resolve(prefix);
        let mut out = Vec::new();
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            while let Some(entry) = rd.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    let md = entry.metadata().await?;
                    if let Some(key) = self.key_from(&path) {
                        out.push(ObjectMeta {
                            key,
                            size: md.len(),
                            last_modified: md.modified().ok(),
                            etag: None,
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.resolve(key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());

        s.put("a/b/x.txt", b"hello".to_vec()).await.unwrap();
        assert_eq!(s.get("a/b/x.txt").await.unwrap(), b"hello");

        let meta = s.head("a/b/x.txt").await.unwrap().unwrap();
        assert_eq!(meta.size, 5);
        assert_eq!(meta.key, "a/b/x.txt");

        assert!(s.exists("a/b/x.txt").await.unwrap());
        assert!(!s.exists("a/b/missing.txt").await.unwrap());

        s.put("a/c/y.txt", b"yo".to_vec()).await.unwrap();
        let mut list = s.list("a").await.unwrap();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].key, "a/b/x.txt");
        assert_eq!(list[1].key, "a/c/y.txt");

        s.delete("a/b/x.txt").await.unwrap();
        assert!(!s.exists("a/b/x.txt").await.unwrap());
        s.delete("a/b/x.txt").await.unwrap();
    }

    #[tokio::test]
    async fn is_not_found_recognises_local_fs_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalFsStorage::new(tmp.path());
        let err = s.get("does/not/exist.bin").await.unwrap_err();
        assert!(is_not_found(&err), "expected NotFound, got: {err:#}");
    }

    #[test]
    fn is_not_found_walks_cause_chain() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let wrapped = anyhow::Error::from(io).context("get(papers/x.html) failed");
        assert!(is_not_found(&wrapped));
    }

    #[test]
    fn is_not_found_rejects_unrelated_errors() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let wrapped = anyhow::Error::from(io);
        assert!(!is_not_found(&wrapped));
    }
}
