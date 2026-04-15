use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use super::{LocalFsStorage, Storage};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Local,
    S3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    pub root: PathBuf,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            root: dirs::home_dir().unwrap_or_default().join("home-still"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct S3ConfigYaml {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub allow_http: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: Backend,
    pub local: LocalConfig,
    pub s3: S3ConfigYaml,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Local,
            local: LocalConfig::default(),
            s3: S3ConfigYaml::default(),
        }
    }
}

fn expand(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = s[i + 2..].find('}') {
                let var = &s[i + 2..i + 2 + end];
                if let Ok(val) = std::env::var(var) {
                    out.push_str(&val);
                }
                i += 2 + end + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn expand_home(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        return dirs::home_dir().unwrap_or_default().join(rest);
    }
    if s == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    p.to_path_buf()
}

impl StorageConfig {
    pub fn build(&self) -> anyhow::Result<Arc<dyn Storage>> {
        match self.backend {
            Backend::Local => Ok(Arc::new(LocalFsStorage::new(expand_home(&self.local.root)))),
            Backend::S3 => {
                #[cfg(feature = "storage-s3")]
                {
                    let cfg = super::s3::S3Config {
                        endpoint: self.s3.endpoint.clone(),
                        bucket: self.s3.bucket.clone(),
                        region: if self.s3.region.is_empty() {
                            "us-east-1".into()
                        } else {
                            self.s3.region.clone()
                        },
                        access_key: expand(&self.s3.access_key),
                        secret_key: expand(&self.s3.secret_key),
                        allow_http: self.s3.allow_http,
                    };
                    Ok(Arc::new(super::s3::S3Storage::new(cfg)?))
                }
                #[cfg(not(feature = "storage-s3"))]
                {
                    anyhow::bail!("storage.backend=s3 requires the `storage-s3` cargo feature");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local_yaml() {
        let yaml = r#"
backend: local
local:
  root: /tmp/hs-test
"#;
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.backend, Backend::Local);
        assert_eq!(cfg.local.root, PathBuf::from("/tmp/hs-test"));
    }

    #[test]
    fn parse_s3_yaml_with_env_expand() {
        std::env::set_var("HS_TEST_SECRET", "shhh");
        let yaml = r#"
backend: s3
s3:
  endpoint: http://three:9000
  bucket: papers
  access_key: hs-admin
  secret_key: ${HS_TEST_SECRET}
  allow_http: true
"#;
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.backend, Backend::S3);
        assert_eq!(cfg.s3.bucket, "papers");
        assert_eq!(expand(&cfg.s3.secret_key), "shhh");
    }

    #[test]
    fn default_is_local() {
        let cfg = StorageConfig::default();
        assert_eq!(cfg.backend, Backend::Local);
        let _storage = cfg.build().unwrap();
    }

    #[tokio::test]
    async fn build_local_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = StorageConfig {
            backend: Backend::Local,
            local: LocalConfig {
                root: tmp.path().to_path_buf(),
            },
            s3: S3ConfigYaml::default(),
        };
        let s = cfg.build().unwrap();
        s.put("k/v.txt", b"hi".to_vec()).await.unwrap();
        assert_eq!(s.get("k/v.txt").await.unwrap(), b"hi");
    }
}
