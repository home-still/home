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

/// The storage prefix short links live under.
pub const SHORT_LINK_PREFIX: &str = "short-links";

/// The path short links are served from. A short link is
/// `<base>/s/<code>`; the service answers it with a redirect to the
/// signed link it stands for.
pub const SHORT_LINK_PATH: &str = "/s";

/// Characters in a short-link code.
///
/// Twelve characters from a 31-symbol alphabet is 59 bits. The code is not
/// what protects a document — opening it lands on a signed, expiring link
/// that is checked in full — so the length only has to make a code
/// impossible to guess and short enough to read out loud.
pub const SHORT_LINK_CODE_LEN: usize = 12;

/// The alphabet a code is drawn from: lower-case letters and digits with
/// the look-alikes (`0`, `1`, `i`, `l`, `o`) removed, so a code copied off a
/// screen by hand does not turn into a different code.
const SHORT_LINK_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// One short link: what a code stands for.
///
/// The target is a **path**, not a URL. A short link and the signed link it
/// points at live on the same origin — the LAN base URL and the remote base
/// URL serve the same paths — so one code works both at home and away, which
/// a stored absolute URL could not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShortLink {
    /// The path and query the redirect goes to, from the site root.
    pub target: String,
    /// The fragment the redirect appends.
    ///
    /// A fragment is never sent to a server, so a redirect has to carry it
    /// itself: a PDF page (`#page=11`) or a quoted passage (`#:~:text=…`)
    /// would otherwise be lost between the short link and the document.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fragment: String,
    /// When the code stops working, as a Unix timestamp in seconds.
    ///
    /// The signed link it points at has its own expiry, and this one is not
    /// the same clock: this is what `hs docs prune` deletes on, so an expired
    /// code is garbage rather than a redirect into a 403.
    pub expires_at: u64,
}

impl ShortLink {
    /// The `Location` a request for this code is answered with: the target,
    /// the query string and the fragment, relative to whichever origin
    /// served the request.
    pub fn location(&self) -> String {
        format!("{}{}", self.target, self.fragment)
    }
}

/// The storage key a code lives at.
///
/// Codes are unsharded: they are already random, and a listing under one
/// prefix is what `hs docs prune` walks.
pub fn short_link_key(code: &str) -> String {
    format!("{SHORT_LINK_PREFIX}/{code}.json")
}

/// Is this a well-formed short-link code?
///
/// The check is what keeps a code from being a path: it accepts exactly the
/// alphabet [`new_short_link_code`] emits, at exactly
/// [`SHORT_LINK_CODE_LEN`], so `/s/../docs-link.key` is a 404 and never a
/// storage read.
pub fn is_valid_short_code(code: &str) -> bool {
    code.len() == SHORT_LINK_CODE_LEN && code.bytes().all(|b| SHORT_LINK_ALPHABET.contains(&b))
}

/// Mint a fresh short-link code.
///
/// Uniformly drawn from [`SHORT_LINK_ALPHABET`]; the alphabet's length is
/// not a power of two, so a byte is taken modulo the alphabet length — a
/// bias of at most 8/256 that changes no security property, because the code
/// is a pointer and not a capability.
pub fn new_short_link_code() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; SHORT_LINK_CODE_LEN];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| SHORT_LINK_ALPHABET[*b as usize % SHORT_LINK_ALPHABET.len()] as char)
        .collect()
}

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
    /// The collection is not a valid collection name.
    BadCollection,
    /// The format is not one this service serves.
    BadFormat,
    /// The page is not a page: the pages are 1-based, so page 0 names
    /// no page.
    BadPage,
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
    /// The collection the document is in.
    ///
    /// Signed, like the page: a link names one document in one store.
    pub collection: String,
    /// The page the link opens, 1-based.
    ///
    /// The page is part of the *request*, not part of the document, so
    /// it is signed: a link minted for page 11 opens page 11 and
    /// nothing else. `None` means the whole document.
    pub page: Option<u32>,
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
    /// The query string, without a leading `?`.
    pub fn to_query(&self) -> String {
        let page = self.page.map_or_else(String::new, |p| format!("&page={p}"));
        format!(
            "iat={}&exp={}&sub={}&col={}&fmt={}{}&sig={}",
            self.issued_at,
            self.expires_at,
            url_encode(&self.subject),
            url_encode(&self.collection),
            self.format.as_str(),
            page,
            self.signature
        )
    }

    /// The link as a path from the site root: what a redirect points at.
    ///
    /// A short link stores this rather than a full URL, because the same path
    /// is served on the LAN base URL and on the remote one. The stem is
    /// percent-encoded so a stem with a `/`-adjacent character cannot grow a
    /// path segment.
    pub fn to_path(&self) -> String {
        format!(
            "/docs/v1/{}/{}?{}",
            url_encode(&self.stem),
            self.format.as_str(),
            self.to_query()
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
    collection: &str,
    page: Option<u32>,
    ttl_secs: u64,
) -> Result<LinkParams, LinkError> {
    crate::validate_stem(stem).map_err(|_| LinkError::BadStem)?;
    if subject.is_empty() {
        return Err(LinkError::MissingParameter("sub"));
    }
    if collection.is_empty() {
        return Err(LinkError::MissingParameter("col"));
    }
    // The collection is part of the signature, so it has to be a name
    // the verifier can check — the same rule as the stem.
    crate::validate_stem(collection).map_err(|_| LinkError::BadCollection)?;
    // Page 0 is not a page: the pages are 1-based, and a link that
    // named page 0 would be a link to a page that does not exist.
    if page == Some(0) {
        return Err(LinkError::BadPage);
    }
    let issued_at = now_secs();
    let expires_at = issued_at.saturating_add(ttl_secs);
    let fields = SignedFields {
        stem: stem.to_string(),
        format,
        subject: subject.to_string(),
        collection: collection.to_string(),
        page,
        issued_at,
        expires_at,
    };
    let signature = key.sign(&fields.signed_payload());
    Ok(LinkParams {
        stem: fields.stem,
        format: fields.format,
        subject: fields.subject,
        collection: fields.collection,
        page: fields.page,
        issued_at: fields.issued_at,
        expires_at: fields.expires_at,
        signature,
    })
}

/// The fields of a link that the signature covers.
///
/// They travel together, so they are checked together: a verifier
/// cannot accidentally check one and forget another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedFields {
    /// The document stem.
    pub stem: String,
    /// Which document the link opens.
    pub format: LinkFormat,
    /// The subject the link was minted for.
    pub subject: String,
    /// The collection the document is in.
    ///
    /// The corpus holds more than one: the academic papers and the
    /// personal documents are separate stores with separate objects, and
    /// a stem can name a document in either. The collection is part of
    /// the signature, so a link for a personal document cannot be
    /// edited into a link for an academic one — it is a capability for
    /// one document in one store, not for a stem.
    pub collection: String,
    /// The page the link opens. `None` means the whole document.
    pub page: Option<u32>,
    /// When the link was minted, as a Unix timestamp in seconds.
    pub issued_at: u64,
    /// When the link stops working, as a Unix timestamp in seconds.
    pub expires_at: u64,
}

impl SignedFields {
    /// The exact string the signature is computed over.
    ///
    /// The order and the separators are fixed: a verifier and a signer
    /// that disagree here would disagree silently.
    fn signed_payload(&self) -> String {
        format!(
            "{LINK_VERSION}|{}|{}|{}|{}|{}|{}|{}",
            self.stem,
            self.format.as_str(),
            self.subject,
            self.collection,
            self.page.map_or_else(|| "-".to_string(), |p| p.to_string()),
            self.issued_at,
            self.expires_at,
        )
    }
}

/// Verify a link's signature and expiry.
///
/// The stem is checked by the caller, because the stem decides which
/// object is read: this function only decides whether the link is
/// genuine.
pub fn verify(key: &LinkKey, fields: &SignedFields, signature: &str) -> Result<(), LinkError> {
    // A link whose expiry has arrived has expired. The comparison is
    // `>=`, not `>`: a link minted with a zero TTL expires at once, and
    // `>` would let it live for the rest of that second.
    if now_secs() >= fields.expires_at {
        return Err(LinkError::Expired);
    }
    // A link cannot be issued in the future: that would let a link
    // outlive its own expiry.
    if fields.issued_at > now_secs() {
        return Err(LinkError::BadSignature);
    }
    let expected = key.sign(&fields.signed_payload());
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

/// The URL fragment that points a browser at a page of a PDF.
///
/// A PDF viewer honours `#page=N` and scrolls to that page. The
/// fragment is never sent to the server, so the page that the *server*
/// serves is named separately in the query string — the query is what
/// the signature covers.
pub fn pdf_page_fragment(page: u32) -> String {
    format!("#page={page}")
}

/// The longest text a fragment quotes, in characters.
///
/// The text-fragment spec asks for a *short* distinctive snippet, and
/// a URL has a practical length limit. Measured over the corpus's own
/// passages, the first line of a chunk averages 308 characters and
/// reaches 878 — too long for a URL a user has to read, and longer
/// than the browser needs to find the passage.
const MAX_FRAGMENT_CHARS: usize = 120;

/// The URL fragment that points a browser at a span of text.
///
/// This is the W3C **Text Fragments** form, `#:~:text=`: the browser
/// finds the quoted text in the document it loaded, scrolls to it and
/// selects it. It needs nothing from the server — the fragment is
/// resolved in the client, against the bytes the server sent.
///
/// The quoted text is the beginning of the passage, cut at a word
/// boundary: long enough to be distinctive, short enough for a URL.
/// The first line of the passage is the right snippet for a second
/// reason — it is the passage's `line_start`, so the service can find
/// it by line number without searching the text.
pub fn text_fragment(line: &str) -> String {
    format!("#:~:text={}", url_encode_fragment(clip(line)))
}

/// Cut a snippet to [`MAX_FRAGMENT_CHARS`], at a word boundary.
///
/// The cut is at a word boundary because a snippet that ends
/// mid-word is not a substring of the document, and the browser
/// matches on a substring: it would find nothing.
fn clip(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.chars().count() <= MAX_FRAGMENT_CHARS {
        return trimmed;
    }
    let cut = trimmed
        .char_indices()
        .nth(MAX_FRAGMENT_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    // Back up to the last space, so the snippet ends on a word.
    let end = trimmed[..cut].rfind(' ').unwrap_or(cut);
    trimmed[..end].trim_end()
}

/// Percent-encode a value for a URL fragment.
///
/// The text-fragment syntax gives `&`, `#` and `=` meaning, so they
/// are encoded; the rest of the punctuation a line contains is left
/// readable, because a fragment a human can read is a fragment a human
/// can check.
fn url_encode_fragment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'!'
            | b'*'
            | b'('
            | b')' => out.push(b as char),
            // `'` is encoded: the text-fragment syntax treats it as a
            // delimiter, and a passage with an apostrophe in it is
            // common enough to be worth getting right.
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
    /// The collection the document is in.
    pub collection: String,
    /// The format.
    pub format: LinkFormat,
    /// The page the link opens. `None` means the whole document.
    pub page: Option<u32>,
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
        let mut collection: Option<String> = None;
        let mut format: Option<LinkFormat> = None;
        let mut page: Option<u32> = None;
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
                "col" => {
                    collection = Some(url_decode(value).ok_or(LinkError::BadSignature)?);
                }
                "fmt" => {
                    format = Some(LinkFormat::parse(value).ok_or(LinkError::BadFormat)?);
                }
                "page" => {
                    let parsed: u32 = value
                        .parse()
                        .map_err(|_| LinkError::MissingParameter("page"))?;
                    if parsed == 0 {
                        return Err(LinkError::BadPage);
                    }
                    page = Some(parsed);
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
            collection: collection.ok_or(LinkError::MissingParameter("col"))?,
            format: format.ok_or(LinkError::BadFormat)?,
            page,
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

/// The short links this deployment has minted.
///
/// A short link is a code and the signed link behind it, kept in the
/// corpus's storage beside the documents. It is an alias table and not a
/// second signing scheme: opening a code lands on the same signed, expiring
/// URL an agent would otherwise have pasted, which is checked in full. The
/// code itself is a pointer, so a stale one is a redirect into a 403, which
/// is why [`ShortLink::expires_at`] is stored and pruned.
///
/// # One path
///
/// There is one store: this one. Minting a code writes a record, and serving
/// one reads the same record; no in-memory map survives a restart, so a
/// short link handed out yesterday works after a redeploy or it is reported
/// missing, but never answered from a cache that disagrees with storage.
#[cfg(feature = "storage")]
#[derive(Clone)]
pub struct ShortLinks {
    storage: std::sync::Arc<dyn crate::storage::Storage>,
}

#[cfg(feature = "storage")]
impl ShortLinks {
    /// A store over `storage`.
    pub fn new(storage: std::sync::Arc<dyn crate::storage::Storage>) -> Self {
        Self { storage }
    }

    /// Store a code's target and fragment.
    ///
    /// The write is what makes the code work, so its failure is returned: a
    /// caller that prints a code it could not store has printed a 404.
    pub async fn put(&self, code: &str, link: &ShortLink) -> anyhow::Result<()> {
        if !is_valid_short_code(code) {
            anyhow::bail!("{code:?} is not a short-link code");
        }
        let key = short_link_key(code);
        let bytes = serde_json::to_vec(link)
            .map_err(|e| anyhow::anyhow!("serializing the short link for {code} failed: {e}"))?;
        self.storage.put(&key, bytes).await.map_err(|e| {
            anyhow::anyhow!(
                "storing the short link {code} at {this_key} failed: {e:#}",
                this_key = key
            )
        })
    }

    /// The link a code stands for, or `None` when there is no such code.
    ///
    /// An expired code is not returned: a code outlives its usefulness the
    /// moment the link behind it stops working, and a redirect into a 403 is
    /// worse than a 404 that says the code is unknown.
    pub async fn get(&self, code: &str) -> anyhow::Result<Option<ShortLink>> {
        if !is_valid_short_code(code) {
            return Ok(None);
        }
        let key = short_link_key(code);
        let bytes = match self.storage.get(&key).await {
            Ok(bytes) => bytes,
            Err(e) if crate::storage::is_not_found(&e) => return Ok(None),
            Err(e) => anyhow::bail!("reading the short link {code} failed: {e:#}"),
        };
        let link: ShortLink = serde_json::from_slice(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "the short link {code} at {key} is malformed: {e}. Refusing to guess what it \
                 pointed at."
            )
        })?;
        if now_secs() >= link.expires_at {
            return Ok(None);
        }
        Ok(Some(link))
    }

    /// Every stored code, expired or not.
    pub async fn list(&self) -> anyhow::Result<Vec<(String, ShortLink)>> {
        let objects = self
            .storage
            .list(SHORT_LINK_PREFIX)
            .await
            .map_err(|e| anyhow::anyhow!("listing {SHORT_LINK_PREFIX} failed: {e:#}"))?;
        let mut out = Vec::new();
        for object in objects {
            let Some(code) = object
                .key
                .strip_prefix(SHORT_LINK_PREFIX)
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| rest.strip_suffix(".json"))
            else {
                continue;
            };
            let bytes = self.storage.get(&object.key).await.map_err(|e| {
                anyhow::anyhow!("reading the short link {} failed: {e:#}", object.key)
            })?;
            let link: ShortLink = serde_json::from_slice(&bytes).map_err(|e| {
                anyhow::anyhow!(
                    "the short link at {} is malformed: {e}. Refusing to guess what it pointed \
                     at.",
                    object.key
                )
            })?;
            out.push((code.to_string(), link));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Delete every code whose link has expired. Returns the codes deleted.
    pub async fn prune(&self) -> anyhow::Result<Vec<String>> {
        let now = now_secs();
        let mut deleted = Vec::new();
        for (code, link) in self.list().await? {
            if now < link.expires_at {
                continue;
            }
            self.storage
                .delete(&short_link_key(&code))
                .await
                .map_err(|e| anyhow::anyhow!("deleting the short link {code} failed: {e:#}"))?;
            deleted.push(code);
        }
        Ok(deleted)
    }
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
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            3600,
        )
        .unwrap();
        verify(
            &k,
            &SignedFields {
                stem: "10.1234_abcd".to_string(),
                format: LinkFormat::Source,
                subject: "laptop".to_string(),
                collection: "academic".to_string(),
                page: link.page,
                issued_at: link.issued_at,
                expires_at: link.expires_at,
            },
            &link.signature,
        )
        .unwrap();
    }

    #[test]
    fn a_tampered_stem_format_subject_or_expiry_does_not_verify() {
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            3600,
        )
        .unwrap();
        // Any field the signature covers, changed, invalidates it.
        let tampered = |stem: &str, format: LinkFormat, subject: &str, expires_at: u64| {
            verify(
                &k,
                &SignedFields {
                    stem: stem.to_string(),
                    format,
                    subject: subject.to_string(),
                    collection: "academic".to_string(),
                    page: link.page,
                    issued_at: link.issued_at,
                    expires_at,
                },
                &link.signature,
            )
        };
        assert_eq!(
            tampered(
                "10.1234_other",
                LinkFormat::Source,
                "laptop",
                link.expires_at
            ),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            tampered(
                "10.1234_abcd",
                LinkFormat::Markdown,
                "laptop",
                link.expires_at
            ),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            tampered("10.1234_abcd", LinkFormat::Source, "phone", link.expires_at),
            Err(LinkError::BadSignature)
        );
        assert_eq!(
            tampered(
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                link.expires_at + 1
            ),
            Err(LinkError::BadSignature)
        );
    }

    #[test]
    fn a_link_signed_with_another_key_does_not_verify() {
        let a = key();
        let b = LinkKey::from_bytes(vec![9u8; 32]).unwrap();
        let link = mint(
            &a,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            3600,
        )
        .unwrap();
        assert_eq!(
            verify(
                &b,
                &SignedFields {
                    stem: "10.1234_abcd".to_string(),
                    format: LinkFormat::Source,
                    subject: "laptop".to_string(),
                    collection: "academic".to_string(),
                    page: link.page,
                    issued_at: link.issued_at,
                    expires_at: link.expires_at,
                },
                &link.signature,
            ),
            Err(LinkError::BadSignature)
        );
    }

    #[test]
    fn an_expired_link_is_refused() {
        let k = key();
        // A link minted in the past is already expired.
        let expires_at = now_secs() - 1;
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            0,
        )
        .unwrap();
        assert_eq!(
            verify(
                &k,
                &SignedFields {
                    stem: "10.1234_abcd".to_string(),
                    format: LinkFormat::Source,
                    subject: "laptop".to_string(),
                    collection: "academic".to_string(),
                    page: link.page,
                    issued_at: link.issued_at,
                    expires_at,
                },
                &link.signature,
            ),
            Err(LinkError::Expired)
        );
        assert!(link.expires_at <= now_secs());
    }

    #[test]
    fn a_query_string_round_trips() {
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "lap top",
            "academic",
            None,
            3600,
        )
        .unwrap();
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
            LinkQuery::parse("iat=1&sub=laptop&col=academic&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("exp")
        );
        assert_eq!(
            LinkQuery::parse("exp=1&sub=laptop&col=academic&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("iat")
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&col=academic&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("sub")
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&col=academic&sig=ab").unwrap_err(),
            LinkError::BadFormat
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&col=academic&fmt=source").unwrap_err(),
            LinkError::MissingParameter("sig")
        );
        // An unknown format is refused, not guessed at.
        assert_eq!(
            LinkQuery::parse("iat=1&exp=1&sub=laptop&col=academic&fmt=pdf&sig=ab").unwrap_err(),
            LinkError::BadFormat
        );
    }

    #[test]
    fn a_stem_that_is_not_a_stem_is_refused_at_minting() {
        let k = key();
        for stem in ["../etc/passwd", "a/b", "", "..", "a\\b"] {
            assert_eq!(
                mint(
                    &k,
                    stem,
                    LinkFormat::Source,
                    "laptop",
                    "academic",
                    None,
                    3600
                )
                .unwrap_err(),
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
    fn a_page_is_signed() {
        // A link minted for page 11 must open page 11 and nothing
        // else: the page is part of the request, and the request is
        // what the signature covers.
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            Some(11),
            3600,
        )
        .unwrap();
        verify(
            &k,
            &SignedFields {
                stem: "10.1234_abcd".to_string(),
                format: LinkFormat::Source,
                subject: "laptop".to_string(),
                collection: "academic".to_string(),
                page: Some(11),
                issued_at: link.issued_at,
                expires_at: link.expires_at,
            },
            &link.signature,
        )
        .unwrap();

        // The same signature with a different page does not verify.
        assert_eq!(
            verify(
                &k,
                &SignedFields {
                    stem: "10.1234_abcd".to_string(),
                    format: LinkFormat::Source,
                    subject: "laptop".to_string(),
                    collection: "academic".to_string(),
                    page: Some(12),
                    issued_at: link.issued_at,
                    expires_at: link.expires_at,
                },
                &link.signature,
            ),
            Err(LinkError::BadSignature)
        );
        // And neither does the same link with no page.
        assert_eq!(
            verify(
                &k,
                &SignedFields {
                    stem: "10.1234_abcd".to_string(),
                    format: LinkFormat::Source,
                    subject: "laptop".to_string(),
                    collection: "academic".to_string(),
                    page: None,
                    issued_at: link.issued_at,
                    expires_at: link.expires_at,
                },
                &link.signature,
            ),
            Err(LinkError::BadSignature)
        );
    }

    /// The collection is part of the signature, like the page.
    ///
    /// The corpus holds more than one store — the academic papers and
    /// the personal documents — and a stem can name a document in
    /// either. A link is a capability for one document in one store, so
    /// changing the collection invalidates it.
    #[test]
    fn a_tampered_collection_does_not_verify() {
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "personal",
            None,
            3600,
        )
        .unwrap();
        verify(
            &k,
            &SignedFields {
                stem: "10.1234_abcd".to_string(),
                format: LinkFormat::Source,
                subject: "laptop".to_string(),
                collection: "personal".to_string(),
                page: link.page,
                issued_at: link.issued_at,
                expires_at: link.expires_at,
            },
            &link.signature,
        )
        .unwrap();

        // The same signature with a different collection does not.
        assert_eq!(
            verify(
                &k,
                &SignedFields {
                    stem: "10.1234_abcd".to_string(),
                    format: LinkFormat::Source,
                    subject: "laptop".to_string(),
                    collection: "academic".to_string(),
                    page: link.page,
                    issued_at: link.issued_at,
                    expires_at: link.expires_at,
                },
                &link.signature,
            ),
            Err(LinkError::BadSignature)
        );
    }

    #[test]
    fn a_link_without_a_collection_is_refused() {
        // A missing parameter is an error, never a default: a link that
        // cannot be checked is a link that is not served.
        assert_eq!(
            LinkQuery::parse("iat=1&exp=2&sub=laptop&fmt=source&sig=ab").unwrap_err(),
            LinkError::MissingParameter("col")
        );
    }

    #[test]
    fn a_collection_that_is_not_a_stem_is_refused() {
        let k = key();
        assert_eq!(
            mint(
                &k,
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                "../etc",
                None,
                3600
            )
            .unwrap_err(),
            LinkError::BadCollection
        );
    }

    #[test]
    fn page_zero_is_not_a_page() {
        // The pages are 1-based, so page 0 names no page.
        let k = key();
        assert_eq!(
            mint(
                &k,
                "10.1234_abcd",
                LinkFormat::Source,
                "laptop",
                "academic",
                Some(0),
                3600
            )
            .unwrap_err(),
            LinkError::BadPage
        );
        assert_eq!(
            LinkQuery::parse("iat=1&exp=2&sub=laptop&col=academic&fmt=source&page=0&sig=ab")
                .unwrap_err(),
            LinkError::BadPage
        );
    }

    #[test]
    fn a_query_string_carries_the_page() {
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            Some(11),
            3600,
        )
        .unwrap();
        let query = link.to_query();
        assert!(query.contains("page=11"), "{query}");
        let parsed = LinkQuery::parse(&query).unwrap();
        assert_eq!(parsed.page, Some(11));
        assert_eq!(parsed.signature, link.signature);
    }

    #[test]
    fn a_link_with_no_page_has_no_page_in_its_query() {
        let k = key();
        let link = mint(
            &k,
            "10.1234_abcd",
            LinkFormat::Source,
            "laptop",
            "academic",
            None,
            3600,
        )
        .unwrap();
        assert!(!link.to_query().contains("page="), "{}", link.to_query());
        assert_eq!(LinkQuery::parse(&link.to_query()).unwrap().page, None);
    }

    #[test]
    fn a_pdf_fragment_points_at_the_page() {
        assert_eq!(crate::docs::pdf_page_fragment(11), "#page=11");
    }

    #[test]
    fn a_text_fragment_quotes_the_passage() {
        // The text fragment is the browser-native way to point at a span
        // of text: it names the text, and the browser finds it.
        let fragment = crate::docs::text_fragment("The rapid acquisition of memory");
        assert!(fragment.starts_with("#:~:text="), "{fragment}");
        // The text is percent-encoded, as the spec requires: a space is
        // %20 and the characters that would end the fragment early are
        // encoded too.
        assert!(fragment.contains("%20"), "{fragment}");
        let fragment = crate::docs::text_fragment("a & b = c?");
        assert!(fragment.contains("%26"), "{fragment}");
        assert!(fragment.contains("%3F"), "{fragment}");
        // Ordinary words stay readable.
        let fragment = crate::docs::text_fragment("plain words here");
        assert!(fragment.contains("plain%20words%20here"), "{fragment}");
    }

    #[test]
    fn a_text_fragment_is_short_enough_for_a_url() {
        // The corpus's passages run to hundreds of characters on their
        // first line, and a URL has a practical length limit.
        let long = "During acute stress, the limbic system and thalamus are activated by the \
                     cerebral cortex through the reticular activating system, which subsequently \
                     activates the hypothalamus, triggering an autonomic nervous system and \
                     endocrine response, resulting in catecholamine and cortisol secretion.";
        let fragment = crate::docs::text_fragment(long);
        let quoted = crate::docs::url_decode(fragment.trim_start_matches("#:~:text=")).unwrap();
        assert!(
            quoted.chars().count() <= 120,
            "the snippet is {} chars: {quoted}",
            quoted.chars().count()
        );
        // And it ends on a word, so the browser's substring match finds
        // it: a snippet cut mid-word is not in the document.
        assert!(!quoted.ends_with(' '), "{quoted}");
        assert!(
            long.contains(&quoted),
            "the snippet is not a substring: {quoted}"
        );
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
            LinkError::BadPage,
            LinkError::MissingParameter("exp"),
            LinkError::BadKey,
        ];
        let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
        assert!(rendered.iter().all(|r| r == &rendered[0]), "{rendered:?}");
    }

    #[test]
    fn a_short_code_is_the_alphabet_at_a_fixed_length() {
        // The alphabet has no look-alikes, so a code read off a screen by hand
        // does not turn into a different code.
        for _ in 0..200 {
            let code = new_short_link_code();
            assert!(is_valid_short_code(&code), "{code}");
            assert!(
                !code.contains('0')
                    && !code.contains('1')
                    && !code.contains('i')
                    && !code.contains('l')
                    && !code.contains('o'),
                "{code}"
            );
        }
        // And the check is what keeps a code from being a path.
        for bad in [
            "",
            "short",
            "/etc/passwd",
            "../docs-link.key",
            "abcdefghjkm!",
            "ABCDEFGHJKMN",
            "abcdefghjkmnp",
        ] {
            assert!(!is_valid_short_code(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_link_path_is_what_a_redirect_points_at() {
        // A short link stores a path, not a URL: the same path is served on the
        // LAN base URL and on the remote one.
        let params = mint(
            &key(),
            "10.1234_abcd",
            LinkFormat::Markdown,
            "laptop",
            "academic",
            Some(3),
            60,
        )
        .unwrap();
        let path = params.to_path();
        assert!(
            path.starts_with("/docs/v1/10.1234_abcd/markdown?"),
            "{path}"
        );
        assert!(path.contains("page=3"), "{path}");
        assert!(
            !path.contains("://"),
            "a redirect target must be a path: {path}"
        );
    }
}

#[cfg(all(test, feature = "storage"))]
mod short_link_tests {
    use super::*;
    use crate::storage::{LocalFsStorage, Storage};

    fn store(dir: &std::path::Path) -> ShortLinks {
        ShortLinks::new(std::sync::Arc::new(LocalFsStorage::new(dir)))
    }

    /// A code outlives nothing: it is a pointer to a signed link, so what it
    /// stores is the path, the fragment and the expiry — and reading it back
    /// before the expiry gives exactly that.
    #[tokio::test]
    async fn a_stored_code_resolves_until_it_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let link = ShortLink {
            target: "/docs/v1/10.1234_abcd/source?iat=1&exp=2&sig=x".to_string(),
            fragment: "#page=11".to_string(),
            expires_at: now_secs() + 60,
        };
        store.put("abcdefghjkmn", &link).await.unwrap();

        let found = store.get("abcdefghjkmn").await.unwrap().unwrap();
        assert_eq!(found, link);
        assert_eq!(
            found.location(),
            format!("{}{}", link.target, link.fragment)
        );
    }

    /// A browser follows a `Location` and cannot be made to re-add a fragment,
    /// so the fragment travels with the record.
    #[tokio::test]
    async fn an_expired_code_is_not_served() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        store
            .put(
                "abcdefghjkmn",
                &ShortLink {
                    target: "/docs/v1/10.1234_abcd/source?iat=1&exp=2&sig=x".to_string(),
                    fragment: String::new(),
                    expires_at: now_secs(),
                },
            )
            .await
            .unwrap();

        // The signed link behind it has expired, so the code is a redirect into
        // a 403: answered as unknown instead.
        assert!(store.get("abcdefghjkmn").await.unwrap().is_none());
    }

    /// Pruning is what stops the table growing forever, and it must not delete a
    /// code that still works.
    #[tokio::test]
    async fn pruning_deletes_only_the_expired_codes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path());
        let live = ShortLink {
            target: "/docs/v1/live/source?iat=1&exp=2&sig=x".to_string(),
            fragment: String::new(),
            expires_at: now_secs() + 3600,
        };
        let dead = ShortLink {
            expires_at: now_secs() - 1,
            ..live.clone()
        };
        store.put("aaaaaaaaaaaa", &live).await.unwrap();
        store.put("bbbbbbbbbbbb", &dead).await.unwrap();

        let deleted = store.prune().await.unwrap();

        assert_eq!(deleted, ["bbbbbbbbbbbb"]);
        assert!(store.get("aaaaaaaaaaaa").await.unwrap().is_some());
        assert!(store.get("bbbbbbbbbbbb").await.unwrap().is_none());
    }

    /// A code that is not a code is never read from storage: the check is what
    /// keeps `/s/../docs-link.key` a 404 rather than a key lookup.
    #[tokio::test]
    async fn a_malformed_code_is_never_read() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = std::sync::Arc::new(LocalFsStorage::new(tmp.path()));
        storage
            .put(
                "secrets.txt",
                b"a secret that must never be served".to_vec(),
            )
            .await
            .unwrap();
        let store = ShortLinks::new(storage);

        for bad in ["../secrets", "secrets.txt", "", "abcdefghjkm"] {
            assert!(store.get(bad).await.unwrap().is_none(), "{bad:?}");
        }
    }
}
