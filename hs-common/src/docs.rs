//! Signed, expiring links to the documents in the corpus.
//!
//! An agent that finds a passage must be able to hand the user
//! something they can open. A storage key is not that: it is a path on
//! the machine that holds the corpus, which the user's laptop cannot
//! reach. This module mints a URL that a browser *can* open, and
//! verifies it again on the way in.
//!
//! # The link
//!
//! A link names one document, one format, one subject and one expiry:
//!
//! ```text
//! <base>/docs/v1/<stem>/<format>?exp=<unix>&sub=<subject>&fmt=<format>&sig=<hex>
//! ```
//!
//! `sig` is `HMAC-SHA256(key, "v1|stem|fmt|sub|exp")`, hex-encoded.
//! Every field the signature covers is in the query string, so a client
//! that changes the stem, the format, the subject or the expiry
//! invalidates the signature without touching the key.
//!
//! # Why a query string and not a header
//!
//! A browser cannot attach an `Authorization` header to a link someone
//! clicks. The signature travels in the query string so the link works
//! in a browser, in a PDF viewer, and in `curl`.
//!
//! # What the signature does not leak
//!
//! A query-string signature ends up in proxy logs and in browser
//! history. Three things bound that exposure: the link expires, it is
//! bound to one document and one format, and it is bound to the subject
//! it was minted for. Revoking a subject invalidates its links (see
//! [`Revocations`]).
//!
//! # One path
//!
//! There is one signer and one verifier, in this module, and both the
//! service and every minter call them. There is no "trusted LAN"
//! variant of the check: the LAN URL and the remote URL carry the same
//! signature and are checked by the same code.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The version tag in the signed payload. A change to the signing rule
/// changes the version, so an old link can never be verified by a new
/// verifier and vice versa.
pub const LINK_VERSION: &str = "v1";

/// The shortest a link key may be. A short key is a weak key.
pub const MIN_KEY_LEN: usize = 32;

/// Which document a link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFormat {
    /// The original object: the PDF, the HTML page or the EPUB.
    Source,
    /// The converted markdown.
    Markdown,
}

impl LinkFormat {
    /// The name that appears in the URL path and in the signature.
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkFormat::Source => "source",
            LinkFormat::Markdown => "markdown",
        }
    }

    /// Parse the format name from a URL or a query string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "source" => Some(LinkFormat::Source),
            "markdown" => Some(LinkFormat::Markdown),
            _ => None,
        }
    }
}

impl fmt::Display for LinkFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a link was refused.
///
/// The variants are deliberately coarse: a caller that learns *why* a
/// link failed learns something about the corpus. A refused link says
/// only that it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The signature does not match the payload, or was not a signature
    /// at all.
    BadSignature,
    /// The link's expiry is in the past.
    Expired,
    /// The subject the link was minted for has been revoked.
    Revoked,
    /// The stem is not a valid document stem.
    BadStem,
    /// The format is not one this service serves.
    BadFormat,
    /// A required query parameter is missing.
    MissingParameter(&'static str),
    /// The link key is unusable (too short, or missing).
    BadKey,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Every variant reads the same to a client: the link was
        // refused, and nothing more.
        f.write_str("link refused")
    }
}

impl std::error::Error for LinkError {}

/// The pieces of a link, as they travel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkParams {
    /// The document stem.
    pub stem: String,
    /// Which document the link opens.
    pub format: LinkFormat,
    /// The subject the link was minted for.
    pub subject: String,
    /// When the link was minted, as a Unix timestamp in seconds.
    ///
    /// The link carries this so a revocation can be applied to it: a
    /// link minted *before* a subject was revoked is refused, and a link
    /// minted after it is not — the subject is a new device wearing the
    /// same name.
    pub issued_at: u64,
    /// When the link stops working, as a Unix timestamp in seconds.
    pub expires_at: u64,
    /// The hex signature.
    pub signature: String,
}

impl LinkParams {
    /// The exact string the signature is computed over.
    ///
    /// The order and the separators are fixed: a verifier and a signer
    /// that disagree here would disagree silently.
    fn signed_payload(
        stem: &str,
        format: LinkFormat,
        subject: &str,
        issued_at: u64,
        expires_at: u64,
    ) -> String {
        format!(
            "{LINK_VERSION}|{stem}|{}|{subject}|{issued_at}|{expires_at}",
            format.as_str()
        )
    }

    /// The query string, without a leading `?`.
    pub fn to_query(&self) -> String {
        format!(
            "iat={}&exp={}&sub={}&fmt={}&sig={}",
            self.issued_at,
            self.expires_at,
            url_encode(&self.subject),
            self.format.as_str(),
            self.signature
        )
    }
}

/// Percent-encode a value for a query string.
///
/// Only the characters that are actually unsafe in a query value are
/// encoded, so a subject that is a plain device name stays readable in
/// the URL.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode a query-string value.
pub fn url_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The signing key: the secret the links are signed with.
#[derive(Clone)]
pub struct LinkKey {
    bytes: Vec<u8>,
}

impl fmt::Debug for LinkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key is never printed.
        f.write_str("LinkKey(<redacted>)")
    }
}

impl LinkKey {
    /// Take a key from bytes. A key shorter than [`MIN_KEY_LEN`] is
    /// refused: a weak key is a forgeable key.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, LinkError> {
        if bytes.len() < MIN_KEY_LEN {
            return Err(LinkError::BadKey);
        }
        Ok(Self { bytes })
    }

    /// The key's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Sign a payload with this key, hex-encoded.
    fn sign(&self, payload: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.bytes).expect("HMAC accepts any key size");
        mac.update(payload.as_bytes());
        hex_lower(&mac.finalize().into_bytes())
    }
}

/// Hex-encode bytes, lower case.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Now, as a Unix timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a link.
///
/// The expiry is `ttl_secs` from now. The link is bound to the stem,
/// the format and the subject: changing any of them in the URL
/// invalidates the signature.
pub fn mint(
    key: &LinkKey,
    stem: &str,
    format: LinkFormat,
    subject: &str,
    ttl_secs: u64,
) -> Result<LinkParams, LinkError> {
    crate::validate_stem(stem).map_err(|_| LinkError::BadStem)?;
    if subject.is_empty() {
        return Err(LinkError::MissingParameter("sub"));
    }
    let issued_at = now_secs();
    let expires_at = issued_at.saturating_add(ttl_secs);
    let payload = LinkParams::signed_payload(stem, format, subject, issued_at, expires_at);
    let signature = key.sign(&payload);
    Ok(LinkParams {
        stem: stem.to_string(),
        format,
        subject: subject.to_string(),
        issued_at,
        expires_at,
        signature,
    })
}

/// Verify a link's signature and expiry.
///
/// The stem is checked by the caller, because the stem decides which
/// object is read: this function only decides whether the link is
/// genuine.
pub fn verify(
    key: &LinkKey,
    stem: &str,
    format: LinkFormat,
    subject: &str,
    issued_at: u64,
    expires_at: u64,
    signature: &str,
) -> Result<(), LinkError> {
    // A link whose expiry has arrived has expired. The comparison is
    // `>=`, not `>`: a link minted with a zero TTL expires at once, and
    // `>` would let it live for the rest of that second.
    if now_secs() >= expires_at {
        return Err(LinkError::Expired);
    }
    // A link cannot be issued in the future: that would let a link
    // outlive its own expiry.
    if issued_at > now_secs() {
        return Err(LinkError::BadSignature);
    }
    let expected = key.sign(&LinkParams::signed_payload(
        stem, format, subject, issued_at, expires_at,
    ));
    // Constant-time comparison: a link's signature is public, but a
    // timing side channel would let an attacker forge one a byte at a
    // time.
    if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
        return Err(LinkError::BadSignature);
    }
    Ok(())
}

/// Compare two byte strings without leaking where they differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The parameters of a link, read out of a query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkQuery {
    /// When the link was minted, as a Unix timestamp.
    pub issued_at: u64,
    /// The expiry, as a Unix timestamp.
    pub expires_at: u64,
    /// The subject the link was minted for.
    pub subject: String,
    /// The format.
    pub format: LinkFormat,
    /// The signature.
    pub signature: String,
}

impl LinkQuery {
    /// Read the link's parameters out of a query string.
    ///
    /// A missing or malformed parameter is an error, never a default:
    /// a link that cannot be checked is a link that is not served.
    pub fn parse(query: &str) -> Result<Self, LinkError> {
        let mut issued_at: Option<u64> = None;
        let mut expires_at: Option<u64> = None;
        let mut subject: Option<String> = None;
        let mut format: Option<LinkFormat> = None;
        let mut signature: Option<String> = None;

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = pair.split_once('=').ok_or(LinkError::BadSignature)?;
            match name {
                "iat" => {
                    issued_at = Some(
                        value
                            .parse()
                            .map_err(|_| LinkError::MissingParameter("iat"))?,
                    )
                }
                "exp" => {
                    expires_at = Some(
                        value
                            .parse()
                            .map_err(|_| LinkError::MissingParameter("exp"))?,
                    )
                }
                "sub" => {
                    subject = Some(url_decode(value).ok_or(LinkError::BadSignature)?);
                }
                "fmt" => {
                    format = Some(LinkFormat::parse(value).ok_or(LinkError::BadFormat)?);
                }
                "sig" => {
                    signature = Some(url_decode(value).ok_or(LinkError::BadSignature)?);
                }
                _ => {}
            }
        }
        Ok(Self {
            issued_at: issued_at.ok_or(LinkError::MissingParameter("iat"))?,
            expires_at: expires_at.ok_or(LinkError::MissingParameter("exp"))?,
            subject: subject.ok_or(LinkError::MissingParameter("sub"))?,
            format: format.ok_or(LinkError::BadFormat)?,
            signature: signature.ok_or(LinkError::MissingParameter("sig"))?,
        })
    }
}

/// Where the link key lives.
///
/// Beside the other per-deployment credentials in `~/.home-still/`, so
/// one directory holds everything secret about a host.
pub fn default_key_path() -> PathBuf {
    crate::hidden_dir()
        .unwrap_or_else(|_| PathBuf::from(".home-still"))
        .join("docs-link.key")
}

/// Load the link key, creating it if it is not there yet.
///
/// The file is created with mode 0600 and never overwritten: an
/// existing key that cannot be read is an error, not a reason to
/// generate a new one (which would silently invalidate every link).
pub fn load_or_create_key(path: &Path) -> anyhow::Result<LinkKey> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let key = LinkKey::from_bytes(bytes).map_err(|e| {
                anyhow::anyhow!(
                    "the docs link key at {} is unusable: {e}. Remove it to generate a new one, \
                     which invalidates every link already issued.",
                    path.display()
                )
            })?;
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = vec![0u8; 32];
            getrandom(&mut bytes)
                .map_err(|e| anyhow::anyhow!("generating a docs link key failed: {e}"))?;
            write_private_file(path, &bytes)?;
            Ok(LinkKey::from_bytes(bytes)?)
        }
        Err(e) => Err(anyhow::anyhow!(
            "reading the docs link key at {} failed: {e}",
            path.display()
        )),
    }
}

/// Fill a buffer with random bytes.
fn getrandom(bytes: &mut [u8]) -> std::io::Result<()> {
    use rand::RngCore;
    rand::rng().fill_bytes(bytes);
    Ok(())
}

/// Write a file with mode 0600, refusing to overwrite an existing one.
///
/// The mode is a Unix concept. On Windows the file is created with the
/// default permissions and the "never overwrite" rule still holds —
/// `create_new` is what enforces that, and it is what matters.
fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating {} failed: {e}", parent.display()))?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path).map_err(|e| {
        anyhow::anyhow!(
            "creating {} failed: {e}. A key that already exists is never overwritten.",
            path.display()
        )
    })?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// The subjects whose links have been revoked, and when.
///
/// The gateway already keeps a revocation list for its own tokens
/// (`cloud-revoked.json`, beside the signing secret). A link's subject
/// is checked against the same list, so revoking a device revokes its
/// links too.
#[derive(Debug, Default, Clone)]
pub struct Revocations {
    /// Subject -> the Unix time it was revoked at.
    entries: std::collections::HashMap<String, u64>,
}

impl Revocations {
    /// Read a revocation list from a JSON file.
    ///
    /// A missing file is an empty list. A malformed file is an error:
    /// silently treating a corrupt list as empty would serve links whose
    /// subject was revoked.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "reading the revocation list at {} failed: {e}",
                    path.display()
                ))
            }
        };
        let entries: std::collections::HashMap<String, u64> =
            serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!(
                    "the revocation list at {} is malformed: {e}. Refusing to treat it as empty, \
                     which would serve links whose subject was revoked.",
                    path.display()
                )
            })?;
        Ok(Self { entries })
    }

    /// Is this subject revoked?
    ///
    /// A link minted *before* the revocation is refused; a link minted
    /// after it is not, because the subject is a new device wearing the
    /// same name.
    pub fn is_revoked(&self, subject: &str, issued_at: u64) -> bool {
        self.entries
            .get(subject)
            .is_some_and(|revoked_at| issued_at <= *revoked_at)
    }

    /// Revoke a subject.
    pub fn revoke(&mut self, subject: &str, at: u64) {
        self.entries.insert(subject.to_string(), at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> LinkKey {
        LinkKey::from_bytes(vec![7u8; 32]).unwrap()
    }

    #[test]
    fn a_minted_link_verifies() {
        let k = key();
        let link = mint(&k, "10.1234_abcd", LinkFormat::Source, "laptop", 3600).unwrap();
        verify(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            link.issued_at,
            link.expires_at,
            &link.signature,
        )
        .unwrap();
    }

    #[test]
    fn a_tampered_stem_format_subject_or_expiry_does_not_verify() {
        let k = key();
        let link = mint(&k, "10.1234_abcd", LinkFormat::Source, "laptop", 3600).unwrap();
        // Any field the signature covers, changed, invalidates it.
        assert_eq!(
            verify(
                &k,
                "10.1234_other",
                LinkFormat::Source,
                "laptop",
                link.issued_at,
                link.expires_at,
                &link.signature
            ),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            verify(
                &k,
                "10.1234_abcd",
                LinkFormat::Markdown,
                "laptop",
                link.issued_at,
                link.expires_at,
                &link.signature
            ),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            verify(
                &k,
                "10.1234_abcd",
                LinkFormat::Source,
                "phone",
                link.issued_at,
                link.expires_at,
                &link.signature
            ),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            verify(
                &k,
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                link.issued_at,
                link.expires_at + 1,
                &link.signature
            ),
            Err(LinkError::BadSignature)
        );
    }

    #[test]
    fn a_link_signed_with_another_key_does_not_verify() {
        let a = key();
        let b = LinkKey::from_bytes(vec![9u8; 32]).unwrap();
        let link = mint(&a, "10.1234_abcd", LinkFormat::Source, "laptop", 3600).unwrap();
        assert_eq!(
            verify(
                &b,
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                link.issued_at,
                link.expires_at,
                &link.signature
            ),
            Err(LinkError::BadSignature)
        );
    }

    #[test]
    fn an_expired_link_is_refused() {
        let k = key();
        // A link minted in the past is already expired.
        let expires_at = now_secs() - 1;
        let link = mint(&k, "10.1234_abcd", LinkFormat::Source, "laptop", 0).unwrap();
        assert_eq!(
            verify(
                &k,
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                link.issued_at,
                expires_at,
                &link.signature
            ),
            Err(LinkError::Expired)
        );
        assert!(link.expires_at <= now_secs());
    }

    #[test]
    fn a_query_string_round_trips() {
        let k = key();
        let link = mint(&k, "10.1234_abcd", LinkFormat::Source, "lap top", 3600).unwrap();
        let query = link.to_query();
        let parsed = LinkQuery::parse(&query).unwrap();
        assert_eq!(parsed.expires_at, link.expires_at);
        assert_eq!(parsed.subject, "lap top");
        assert_eq!(parsed.format, link.format);
        assert_eq!(parsed.signature, link.signature);
    }

    #[test]
    fn a_query_string_missing_a_parameter_is_refused() {
        assert_eq!(
            LinkQuery::parse("iat=1&sub=laptop&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("exp")
        );
        assert_eq!(
            LinkQuery::parse("exp=1&sub=laptop&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("iat")
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("sub")
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&sig=ab").unwrap_err(),
            LinkError::BadFormat
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&fmt=source").unwrap_err(),
            LinkError::MissingParameter("sig")
        );
        // An unknown format is refused, not guessed at.
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&fmt=pdf&sig=ab").unwrap_err(),
            LinkError::BadFormat
        );
    }

    #[test]
    fn a_stem_that_is_not_a_stem_is_refused_at_minting() {
        let k = key();
        for stem in ["../etc/passwd", "a/b", "", "..", "a\\b"] {
            assert_eq!(
                mint(&k, stem, LinkFormat::Source, "laptop", 3600).unwrap_err(),
                LinkError::BadStem,
                "{stem:?}"
            );
        }
    }

    #[test]
    fn a_short_key_is_refused() {
        assert_eq!(
            LinkKey::from_bytes(vec![0u8; 8]).unwrap_err(),
            LinkError::BadKey
        );
    }

    #[test]
    fn a_revoked_subject_is_refused() {
        let mut revocations = Revocations::default();
        assert!(!revocations.is_revoked("laptop", 100));
        revocations.revoke("laptop", 200);
        // A link minted before the revocation is refused.
        assert!(revocations.is_revoked("laptop", 100));
        assert!(revocations.is_revoked("laptop", 200));
        // A link minted after it is not: the subject is a new device.
        assert!(!revocations.is_revoked("laptop", 201));
    }

    #[test]
    fn a_refused_link_says_nothing_about_why() {
        // A client that learns why a link failed learns something about
        // the corpus. Every reason reads the same.
        let reasons = [
            LinkError::BadSignature,
            LinkError::Expired,
            LinkError::Revoked,
            LinkError::BadStem,
            LinkError::BadFormat,
            LinkError::MissingParameter("exp"),
            LinkError::BadKey,
        ];
        let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
        assert!(rendered.iter().all(|r| r == &rendered[0]), "{rendered:?}");
    }
}
