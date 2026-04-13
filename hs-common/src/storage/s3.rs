use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use object_store::{aws::AmazonS3Builder, ObjectStore, ObjectStoreExt, PutPayload};

use super::{ObjectMeta, Storage};

pub struct S3Storage {
    inner: Box<dyn ObjectStore>,
    bucket: String,
}

#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub allow_http: bool,
}

impl S3Storage {
    pub fn new(cfg: S3Config) -> anyhow::Result<Self> {
        let store = AmazonS3Builder::new()
            .with_endpoint(&cfg.endpoint)
            .with_bucket_name(&cfg.bucket)
            .with_access_key_id(&cfg.access_key)
            .with_secret_access_key(&cfg.secret_key)
            .with_region(&cfg.region)
            .with_allow_http(cfg.allow_http)
            .with_virtual_hosted_style_request(false)
            .build()?;
        Ok(Self {
            inner: Box::new(store),
            bucket: cfg.bucket,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

fn path(key: &str) -> object_store::path::Path {
    object_store::path::Path::from(key)
}

#[async_trait]
impl Storage for S3Storage {
    async fn get(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let res = self.inner.get(&path(key)).await?;
        Ok(res.bytes().await?.to_vec())
    }

    async fn put(&self, key: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let payload = PutPayload::from(Bytes::from(bytes));
        self.inner.put(&path(key), payload).await?;
        Ok(())
    }

    async fn head(&self, key: &str) -> anyhow::Result<Option<ObjectMeta>> {
        match self.inner.head(&path(key)).await {
            Ok(m) => Ok(Some(ObjectMeta {
                key: m.location.to_string(),
                size: m.size,
                last_modified: Some(m.last_modified.into()),
                etag: m.e_tag,
            })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let prefix_path = if prefix.is_empty() {
            None
        } else {
            Some(path(prefix))
        };
        let mut stream = self.inner.list(prefix_path.as_ref());
        let mut out = Vec::new();
        while let Some(res) = stream.next().await {
            let m = res?;
            out.push(ObjectMeta {
                key: m.location.to_string(),
                size: m.size,
                last_modified: Some(m.last_modified.into()),
                etag: m.e_tag,
            });
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> anyhow::Result<()> {
        match self.inner.delete(&path(key)).await {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minio_config() -> Option<S3Config> {
        let endpoint = std::env::var("HS_S3_ENDPOINT").ok()?;
        let access = std::env::var("HS_S3_ACCESS_KEY").ok()?;
        let secret = std::env::var("HS_S3_SECRET_KEY").ok()?;
        let bucket = std::env::var("HS_S3_BUCKET").unwrap_or_else(|_| "papers".into());
        Some(S3Config {
            endpoint,
            bucket,
            access_key: access,
            secret_key: secret,
            region: "us-east-1".into(),
            allow_http: true,
        })
    }

    #[tokio::test]
    async fn s3_roundtrip() {
        let Some(cfg) = minio_config() else {
            eprintln!("skipping: set HS_S3_ENDPOINT/ACCESS_KEY/SECRET_KEY to run");
            return;
        };
        let s = S3Storage::new(cfg).unwrap();
        let key = "hs-test/smoke.txt";
        s.put(key, b"hello-s3".to_vec()).await.unwrap();
        assert_eq!(s.get(key).await.unwrap(), b"hello-s3");
        let meta = s.head(key).await.unwrap().unwrap();
        assert_eq!(meta.size, 8);
        assert!(s.exists(key).await.unwrap());
        let list = s.list("hs-test/").await.unwrap();
        assert!(list.iter().any(|m| m.key == key));
        s.delete(key).await.unwrap();
        assert!(!s.exists(key).await.unwrap());
    }
}
