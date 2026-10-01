//! Token revocation.
//!
//! Tokens are stateless HMAC blobs, so revoking one means remembering that a
//! *subject* (device name, or `oauth:<client_id>`) was revoked at a point in
//! time: every token for that subject issued at or before that instant is
//! rejected, and tokens minted after it (a fresh enrollment) are not. The map
//! is persisted next to the signing secret so a gateway restart does not
//! resurrect a revoked device. Entries are one short string and one integer and
//! are only added by an administrator, so the file is not reaped.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use hs_common::auth::token::{self, TokenClaims};
use parking_lot::Mutex;

pub struct Revocations {
    path: PathBuf,
    /// subject -> unix time of revocation
    revoked_at: Mutex<HashMap<String, u64>>,
}

impl Revocations {
    /// Load the revocation file; a missing file is an empty list, a malformed
    /// one is an error (never silently treated as "nothing revoked").
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let revoked_at = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data)
                .with_context(|| format!("parsing revocation list {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        Ok(Self {
            path,
            revoked_at: Mutex::new(revoked_at),
        })
    }

    /// Revoke every token issued to `subject` up to and including now.
    /// Persisted before it takes effect in memory; returns the revocation time.
    pub fn revoke(&self, subject: &str) -> anyhow::Result<u64> {
        let now = token::now_epoch();
        let mut map = self.revoked_at.lock();
        let mut next = map.clone();
        next.insert(subject.to_string(), now);
        persist(&self.path, &next)?;
        *map = next;
        Ok(now)
    }

    /// True if `claims` was issued at or before its subject's revocation.
    pub fn is_revoked(&self, claims: &TokenClaims) -> bool {
        self.revoked_at
            .lock()
            .get(&claims.sub)
            .is_some_and(|&at| claims.iat <= at)
    }
}

/// Atomically replace `path` with `map`, the new file being 0600 from creation.
fn persist(path: &Path, map: &HashMap<String, u64>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(serde_json::to_string_pretty(map)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_common::auth::token::TokenType;

    fn claims(sub: &str, iat: u64) -> TokenClaims {
        TokenClaims {
            sub: sub.into(),
            iat,
            exp: iat + 3600,
            scope: vec!["scribe".into()],
            typ: TokenType::Access,
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hs-gw-revoke-{name}-{}-{}",
            std::process::id(),
            token::now_epoch()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("cloud-revoked.json")
    }

    #[test]
    fn revocation_covers_earlier_tokens_only() {
        let path = temp_path("window");
        let revocations = Revocations::load(path.clone()).unwrap();
        let before = token::now_epoch().saturating_sub(100);
        assert!(!revocations.is_revoked(&claims("laptop", before)));

        let at = revocations.revoke("laptop").unwrap();
        assert!(revocations.is_revoked(&claims("laptop", before)));
        assert!(revocations.is_revoked(&claims("laptop", at)));
        // A fresh enrollment after the revocation is a new credential.
        assert!(!revocations.is_revoked(&claims("laptop", at + 1)));
        // Other subjects are untouched.
        assert!(!revocations.is_revoked(&claims("desktop", before)));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn revocation_survives_a_restart() {
        let path = temp_path("restart");
        let at = Revocations::load(path.clone())
            .unwrap()
            .revoke("laptop")
            .unwrap();

        let reloaded = Revocations::load(path.clone()).unwrap();
        assert!(reloaded.is_revoked(&claims("laptop", at)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_corrupt_revocation_file_is_an_error() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "not json").unwrap();
        assert!(Revocations::load(path.clone()).is_err());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
