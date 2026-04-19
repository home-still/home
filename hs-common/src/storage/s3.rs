use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use object_store::{aws::AmazonS3Builder, ObjectStore, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};

use super::{ObjectMeta, Storage};

type HmacSha256 = Hmac<Sha256>;

pub struct S3Storage {
    inner: Box<dyn ObjectStore>,
    bucket: String,
    endpoint: String,
    access_key: String,
    secret_key: String,
    region: String,
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
            endpoint: cfg.endpoint,
            access_key: cfg.access_key,
            secret_key: cfg.secret_key,
            region: cfg.region,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// HEAD the bucket; if 404, PUT it. Idempotent.
    /// Uses path-style sigv4 — works for Garage and S3-compatible stores.
    pub async fn ensure_bucket(&self) -> anyhow::Result<()> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let url = format!("{}/{}/", self.endpoint.trim_end_matches('/'), self.bucket);

        // HEAD first — avoids re-creating on every startup.
        let head = self.signed_request(&client, "HEAD", &url).send().await?;
        if head.status().is_success() {
            return Ok(());
        }
        if head.status().as_u16() != 404 && head.status().as_u16() != 403 {
            // 403 can mean "exists but permission" OR "missing bucket (S3-compatible quirk)".
            // We fall through to PUT; if it truly exists, the PUT returns 409/200.
            let status = head.status();
            let body = head.text().await.unwrap_or_default();
            anyhow::bail!("HEAD bucket {} failed: {status} {body}", self.bucket);
        }

        let put = self.signed_request(&client, "PUT", &url).send().await?;
        let status = put.status();
        if status.is_success() {
            tracing::info!(bucket = %self.bucket, "created S3 bucket");
            return Ok(());
        }
        let body = put.text().await.unwrap_or_default();
        // BucketAlreadyOwnedByYou / BucketAlreadyExists — fine.
        if body.contains("BucketAlreadyOwnedByYou") || body.contains("BucketAlreadyExists") {
            return Ok(());
        }
        anyhow::bail!("create bucket {} failed: {status} {body}", self.bucket);
    }

    fn signed_request(
        &self,
        client: &reqwest::Client,
        method: &str,
        url: &str,
    ) -> reqwest::RequestBuilder {
        let now = chrono::Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let empty_sha = hex_lower(&Sha256::digest(b""));

        let parsed = url::Url::parse(url).expect("valid url");
        let host = match parsed.port() {
            Some(p) => format!("{}:{}", parsed.host_str().unwrap_or(""), p),
            None => parsed.host_str().unwrap_or("").to_string(),
        };
        let canonical_uri = parsed.path().to_string();

        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{empty_sha}\nx-amz-date:{amz_date}\n");
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{empty_sha}"
        );

        let credential_scope = format!("{date_stamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
            hex_lower(&Sha256::digest(canonical_request.as_bytes()))
        );

        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex_lower(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        let req_method = reqwest::Method::from_bytes(method.as_bytes()).expect("valid method");
        client
            .request(req_method, url)
            .header("host", host)
            .header("x-amz-content-sha256", empty_sha)
            .header("x-amz-date", amz_date)
            .header("authorization", authorization)
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Construct an `object_store::path::Path` from a key string.
///
/// `Path::from` percent-encodes every segment, which double-encodes any `%`
/// already present — so a key like `foo%3Cbar.md` becomes `foo%253Cbar.md`
/// and round-trips through `list → to_string → get` land on a 404. `list`
/// returns paths whose `to_string()` is already percent-encoded, so anything
/// we pipe back into `get`/`head`/`delete` must be treated as pre-encoded.
///
/// `Path::parse` validates an already-encoded path and stores it verbatim
/// — exactly the semantics we want for keys that came out of `list`. For
/// raw callers (e.g. `put("catalog/ab/stem.yaml", …)`), `parse` also succeeds
/// because ordinary ASCII path segments are valid percent-encoded input.
/// Fall back to `Path::from` only if `parse` rejects the input (malformed
/// percent triplets or disallowed characters in an unencoded segment).
fn path(key: &str) -> object_store::path::Path {
    object_store::path::Path::parse(key).unwrap_or_else(|_| object_store::path::Path::from(key))
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

    async fn ensure_ready(&self) -> anyhow::Result<()> {
        self.ensure_bucket().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn garage_config() -> Option<S3Config> {
        let endpoint = std::env::var("HS_S3_ENDPOINT").ok()?;
        let access = std::env::var("HS_S3_ACCESS_KEY").ok()?;
        let secret = std::env::var("HS_S3_SECRET_KEY").ok()?;
        let bucket = std::env::var("HS_S3_BUCKET").unwrap_or_else(|_| "home-still".into());
        Some(S3Config {
            endpoint,
            bucket,
            access_key: access,
            secret_key: secret,
            region: "garage".into(),
            allow_http: true,
        })
    }

    #[tokio::test]
    async fn s3_roundtrip() {
        let Some(cfg) = garage_config() else {
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

    #[tokio::test]
    async fn ensure_bucket_creates_fresh_and_is_idempotent() {
        let Some(mut cfg) = garage_config() else {
            eprintln!("skipping: set HS_S3_ENDPOINT/ACCESS_KEY/SECRET_KEY to run");
            return;
        };
        let unique = format!("hs-test-ensure-{}", chrono::Utc::now().timestamp_millis());
        cfg.bucket = unique.clone();
        let s = S3Storage::new(cfg).unwrap();
        s.ensure_bucket().await.expect("create fresh bucket");
        // Idempotent: second call must not fail.
        s.ensure_bucket().await.expect("second call noop");
        // Verify writes work against the new bucket.
        s.put("probe.txt", b"ok".to_vec()).await.unwrap();
        assert_eq!(s.get("probe.txt").await.unwrap(), b"ok");
        s.delete("probe.txt").await.ok();
    }

    /// Regression: `Path::from` double-encodes `%` in percent-encoded keys,
    /// so a key round-tripping through `list → to_string → get` used to land
    /// on a 404. `path()` now routes through `Path::parse`, which treats the
    /// input as already-encoded.
    #[test]
    fn path_preserves_percent_encoded_input() {
        // Realistic stem from the catalog: DOI with `<` / `>` escaped as
        // `%3C` / `%3E`, parentheses and semicolons left raw.
        let key = "markdown/10/10.1002_(sici)1099-1050(199601)5:1%3C77::aid-hec184%3E3.0.co;2-w.md";
        let p = path(key);
        assert_eq!(
            p.to_string(),
            key,
            "path() must not double-encode already-percent-encoded input"
        );
    }

    /// Ordinary safe ASCII keys round-trip unchanged.
    #[test]
    fn path_preserves_safe_ascii_keys() {
        let key = "catalog/ab/some_stem-123.yaml";
        let p = path(key);
        assert_eq!(p.to_string(), key);
    }
}
