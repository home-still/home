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
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub root: PathBuf,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            root: crate::default_project_dir(),
        }
    }
}

/// `Debug` is hand-written: it must not print the access key or the secret
/// (a `{:?}` of any config embedding this struct would otherwise log them).
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct S3ConfigYaml {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub allow_http: bool,
}

impl std::fmt::Debug for S3ConfigYaml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3ConfigYaml")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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

/// Expand `${VAR}` references against the environment. An unset (or
/// non-UTF-8) variable is an error — expanding it to "" would hand empty S3
/// credentials to the backend with nothing to say why. Errors name the
/// variable, never the surrounding text (it may be a literal secret).
#[cfg(feature = "storage-s3")]
fn expand(s: &str) -> anyhow::Result<String> {
    expand_with(s, |v| std::env::var(v).ok())
}

#[cfg(feature = "storage-s3")]
fn expand_with(s: &str, lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let Some(end) = after.find('}') else {
            anyhow::bail!("unterminated `${{` in a storage config value");
        };
        let var = &after[..end];
        let val = lookup(var).ok_or_else(|| {
            anyhow::anyhow!(
                "environment variable `{var}` referenced by the storage config is not set"
            )
        })?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expand a leading `~` to the home directory and require an absolute
/// result. A path that does not start with `~` never consults the home
/// directory; one that does fails loudly when the home directory is unknown.
fn expand_home(p: &std::path::Path) -> anyhow::Result<PathBuf> {
    let s = p.to_string_lossy();
    crate::config_file::expand_home_path("storage.local.root", &s, || {
        crate::home_dir().map_err(|e| format!("{e:#}"))
    })
    .map_err(|e| anyhow::anyhow!(e))
}

/// Build-time check that a required S3 setting is present.
#[cfg(feature = "storage-s3")]
fn require_s3_field(name: &str, value: &str) -> anyhow::Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("storage.backend=s3 requires storage.s3.{name} to be set (it is empty)");
    }
    Ok(())
}

impl StorageConfig {
    pub fn build(&self) -> anyhow::Result<Arc<dyn Storage>> {
        match self.backend {
            Backend::Local => Ok(Arc::new(LocalFsStorage::new(expand_home(
                &self.local.root,
            )?))),
            Backend::S3 => {
                #[cfg(feature = "storage-s3")]
                {
                    use anyhow::Context;

                    let access_key =
                        expand(&self.s3.access_key).context("expanding storage.s3.access_key")?;
                    let secret_key =
                        expand(&self.s3.secret_key).context("expanding storage.s3.secret_key")?;
                    require_s3_field("endpoint", &self.s3.endpoint)?;
                    require_s3_field("bucket", &self.s3.bucket)?;
                    require_s3_field("access_key", &access_key)?;
                    require_s3_field("secret_key", &secret_key)?;
                    let cfg = super::s3::S3Config {
                        endpoint: self.s3.endpoint.clone(),
                        bucket: self.s3.bucket.clone(),
                        region: if self.s3.region.is_empty() {
                            "garage".into()
                        } else {
                            self.s3.region.clone()
                        },
                        access_key,
                        secret_key,
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

    #[cfg(feature = "storage-s3")]
    #[test]
    fn parse_s3_yaml_with_env_expand() {
        let lookup = |k: &str| (k == "HS_TEST_SECRET").then(|| "shhh".to_string());
        let yaml = r#"
backend: s3
s3:
  endpoint: http://<host>:<port>
  bucket: home-still
  access_key: <s3-user>
  secret_key: ${HS_TEST_SECRET}
  allow_http: true
"#;
        let cfg: StorageConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.backend, Backend::S3);
        assert_eq!(cfg.s3.bucket, "home-still");
        assert_eq!(expand_with(&cfg.s3.secret_key, lookup).unwrap(), "shhh");
    }

    #[test]
    fn default_is_local() {
        let cfg = StorageConfig::default();
        assert_eq!(cfg.backend, Backend::Local);
        let _storage = cfg.build().unwrap();
    }

    #[test]
    fn a_relative_local_root_is_refused_naming_the_key() {
        let cfg = StorageConfig {
            backend: Backend::Local,
            local: LocalConfig {
                root: PathBuf::from("data/objects"),
            },
            s3: S3ConfigYaml::default(),
        };
        let err = cfg.build().err().expect("relative root must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("storage.local.root") && msg.contains("data/objects"),
            "{msg}"
        );
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

    #[cfg(feature = "storage-s3")]
    #[test]
    fn expand_substitutes_every_reference_and_keeps_non_ascii_literals() {
        let lookup = |k: &str| match k {
            "HS_TEST_EXPAND_A" => Some("alpha".to_string()),
            "HS_TEST_EXPAND_B" => Some("βeta".to_string()),
            _ => None,
        };
        assert_eq!(
            expand_with(
                "pässwörd-${HS_TEST_EXPAND_A}/中/${HS_TEST_EXPAND_B}!",
                lookup
            )
            .unwrap(),
            "pässwörd-alpha/中/βeta!"
        );
        assert_eq!(
            expand_with("no references é", lookup).unwrap(),
            "no references é"
        );
        assert_eq!(
            expand_with("$notabrace {x}", lookup).unwrap(),
            "$notabrace {x}"
        );
    }

    /// RA-77: an unset variable used to expand to "" and produce empty S3
    /// credentials with no error.
    #[cfg(feature = "storage-s3")]
    #[test]
    fn expand_errors_on_an_unset_variable_and_never_echoes_the_input() {
        let err =
            expand_with("prefix-literal-secret-${HS_TEST_EXPAND_UNSET}", |_| None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("HS_TEST_EXPAND_UNSET"), "{msg}");
        assert!(!msg.contains("literal-secret"), "leaked input: {msg}");

        let err = expand_with("abc${UNTERMINATED", |_| None).unwrap_err();
        assert!(!format!("{err:#}").contains("abc"), "leaked input");
    }

    fn s3_cfg() -> StorageConfig {
        StorageConfig {
            backend: Backend::S3,
            local: LocalConfig::default(),
            s3: S3ConfigYaml {
                endpoint: "http://127.0.0.1:1".into(),
                bucket: "b".into(),
                region: String::new(),
                access_key: "ak".into(),
                secret_key: "sk".into(),
                allow_http: true,
            },
        }
    }

    #[cfg(feature = "storage-s3")]
    #[test]
    fn build_s3_rejects_missing_endpoint_bucket_and_keys() {
        assert!(s3_cfg().build().is_ok());
        for field in ["endpoint", "bucket", "access_key", "secret_key"] {
            let mut cfg = s3_cfg();
            match field {
                "endpoint" => cfg.s3.endpoint.clear(),
                "bucket" => cfg.s3.bucket.clear(),
                "access_key" => cfg.s3.access_key.clear(),
                _ => cfg.s3.secret_key.clear(),
            }
            let err = cfg.build().err().expect(field);
            assert!(
                format!("{err:#}").contains(field),
                "error for empty {field} should name it: {err:#}"
            );
        }
    }

    #[cfg(feature = "storage-s3")]
    #[test]
    fn build_s3_fails_when_a_credential_variable_is_unset() {
        // Never set by any test or fixture; build() reads the real environment.
        let mut cfg = s3_cfg();
        cfg.s3.secret_key = "${HS_TEST_BUILD_UNSET}".into();
        let err = cfg.build().err().expect("unset secret var must fail");
        assert!(
            format!("{err:#}").contains("HS_TEST_BUILD_UNSET"),
            "{err:#}"
        );
    }

    #[test]
    fn debug_output_never_contains_s3_credentials() {
        let mut cfg = s3_cfg();
        cfg.s3.access_key = "AKIA-visible-id".into();
        cfg.s3.secret_key = "top-secret-value".into();
        let shown = format!("{cfg:?} {:?}", cfg.s3);
        assert!(!shown.contains("top-secret-value"), "{shown}");
        assert!(!shown.contains("AKIA-visible-id"), "{shown}");
        assert!(
            shown.contains("http://127.0.0.1:1"),
            "endpoint stays visible"
        );
    }
}
