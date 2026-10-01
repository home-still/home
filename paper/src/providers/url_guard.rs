//! Which URLs the downloader may fetch.
//!
//! The URLs a download fetches are not ours: they come out of Unpaywall,
//! Semantic Scholar, OpenAlex, CrossRef, CORE and Europe PMC records, and out
//! of `Location:` headers on the way. Public papers do not live on private
//! addresses, so a URL that points at one — loopback, RFC 1918, link-local
//! (including the cloud metadata address), CGNAT, multicast, documentation
//! ranges — is a request an attacker is trying to make *from this host*, not
//! a paper. [`UrlPolicy::public_only`] refuses all of them:
//!
//! * before any request: scheme must be `http`/`https`, no `user:pass@`, no
//!   literal non-public IP, no `localhost`;
//! * on every redirect hop (a custom [`redirect::Policy`] runs the same check
//!   against the `Location` target);
//! * for hostnames, at connect time: a [`Resolve`] implementation hands the
//!   HTTP client only addresses that passed [`is_public_ip`], so the check is
//!   made on the very addresses the socket is opened to. There is no
//!   "resolve, check, resolve again" window, which is what DNS rebinding
//!   needs.
//!
//! # Residual limits
//!
//! * If `HTTP_PROXY`/`HTTPS_PROXY` is set, reqwest connects to the proxy and
//!   the proxy resolves the target: the hostname check does not apply (the
//!   literal-IP, scheme and redirect checks still do). Do not point this
//!   process at a proxy that can reach internal hosts.
//! * A hostname that resolves publicly is trusted to stay public for the life
//!   of that connection.
//!
//! The policy that admits loopback exists only under `cfg(test)`
//! ([`UrlPolicy::allow_loopback_for_tests`]) so tests can talk to a fake
//! server on `127.0.0.1`; no production constructor can produce it.

use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use url::{Host, Url};

use crate::error::PaperError;

/// Redirect hops followed before giving up (reqwest's own default is 10).
pub const MAX_REDIRECTS: usize = 10;

/// A URL (or the address behind a hostname) the policy refused. Carried
/// through reqwest's redirect and resolver errors so [`classify`] can turn it
/// back into [`PaperError::UnsafeUrl`].
#[derive(Debug)]
pub struct Refused(pub String);

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for Refused {}

/// What the downloader is allowed to connect to.
#[derive(Debug, Clone, Copy)]
pub struct UrlPolicy {
    /// Admit `127.0.0.0/8`, `::1` and `localhost` (and nothing else private).
    /// Always `false` outside `cfg(test)`.
    allow_loopback: bool,
}

impl UrlPolicy {
    /// The production policy: public addresses only.
    pub const fn public_only() -> Self {
        Self {
            allow_loopback: false,
        }
    }

    /// Test-only: let a fake server on `127.0.0.1` be fetched while every
    /// other private range (metadata address, RFC 1918 …) stays refused, so
    /// redirect-to-private behaviour can still be exercised.
    #[cfg(test)]
    pub(crate) const fn allow_loopback_for_tests() -> Self {
        Self {
            allow_loopback: true,
        }
    }

    /// Parse `raw` and apply [`UrlPolicy::check`].
    pub fn parse_and_check(&self, raw: &str) -> Result<Url, PaperError> {
        let url = Url::parse(raw).map_err(|e| PaperError::UnsafeUrl {
            url: display_url(raw),
            reason: format!("not a valid URL ({e})"),
        })?;
        self.check(&url).map_err(|reason| PaperError::UnsafeUrl {
            url: display_url(url.as_str()),
            reason,
        })?;
        Ok(url)
    }

    /// Static checks that need no network. `Err` carries the reason.
    pub fn check(&self, url: &Url) -> Result<(), String> {
        match url.scheme() {
            "http" | "https" => {}
            other => return Err(format!("scheme {other:?} is not http or https")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("URL carries credentials (user:password@)".to_string());
        }
        match url.host() {
            None => Err("URL has no host".to_string()),
            Some(Host::Ipv4(ip)) => self.check_ip(IpAddr::V4(ip)),
            Some(Host::Ipv6(ip)) => self.check_ip(IpAddr::V6(ip)),
            Some(Host::Domain(name)) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                if name.is_empty() {
                    return Err("URL has an empty host name".to_string());
                }
                if !self.allow_loopback && (name == "localhost" || name.ends_with(".localhost")) {
                    return Err(format!("host {name:?} is loopback"));
                }
                Ok(())
            }
        }
    }

    fn check_ip(&self, ip: IpAddr) -> Result<(), String> {
        if self.allow_loopback && ip.is_loopback() {
            return Ok(());
        }
        if is_public_ip(ip) {
            Ok(())
        } else {
            Err(format!("address {ip} is not a public address"))
        }
    }

    /// Apply this policy to a client under construction: every redirect hop
    /// is checked, and hostnames resolve only to public addresses.
    pub fn harden(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        let policy = *self;
        let builder = builder.redirect(redirect::Policy::custom(move |attempt| {
            // `previous()` holds the initial URL plus every hop already
            // followed, so `> MAX_REDIRECTS` allows exactly MAX_REDIRECTS hops
            // (reqwest's own `Policy::limited` counts the same way).
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error(Refused(format!("more than {MAX_REDIRECTS} redirects")));
            }
            match policy.check(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(reason) => {
                    let target = display_url(attempt.url().as_str());
                    attempt.error(Refused(format!("redirect to {target}: {reason}")))
                }
            }
        }));
        if self.allow_loopback {
            builder
        } else {
            builder.dns_resolver(Arc::new(PublicOnlyResolver))
        }
    }
}

/// Resolves hostnames through the system resolver and refuses the lookup if
/// *any* answer is non-public.
#[derive(Debug)]
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_public(&host).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

async fn resolve_public(host: &str) -> Result<Vec<SocketAddr>, Box<dyn StdError + Send + Sync>> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
    if addrs.is_empty() {
        return Err(Box::new(Refused(format!(
            "host {host:?} did not resolve to any address"
        ))));
    }
    if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
        return Err(Box::new(Refused(format!(
            "host {host:?} resolves to the non-public address {}",
            bad.ip()
        ))));
    }
    Ok(addrs)
}

/// Turn a failed request into a [`PaperError`], recognising a policy refusal
/// raised from a redirect or from the resolver anywhere in the error chain.
pub fn classify(url: &str, err: reqwest::Error) -> PaperError {
    let mut source: Option<&(dyn StdError + 'static)> = Some(&err);
    while let Some(e) = source {
        if let Some(refused) = e.downcast_ref::<Refused>() {
            return PaperError::UnsafeUrl {
                url: display_url(url),
                reason: refused.0.clone(),
            };
        }
        source = e.source();
    }
    PaperError::from(err)
}

/// A URL safe to put in an error message or log: scheme, host, port and path
/// only — no query string (signed links), fragment or credentials. Text that
/// is not a URL is cut to 120 characters.
pub fn display_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(url) if url.host_str().is_some() => {
            let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
            if let Some(port) = url.port() {
                out.push_str(&format!(":{port}"));
            }
            out.push_str(url.path());
            out
        }
        _ => {
            let shown: String = raw.chars().take(120).collect();
            if shown.len() < raw.len() {
                format!("{shown}…")
            } else {
                shown
            }
        }
    }
}

/// True for addresses that can legitimately host a public paper.
///
/// `Ipv4Addr::is_global` and friends are unstable, so the ranges are listed
/// by hand. IPv6 forms that embed an IPv4 address (mapped, compatible, NAT64,
/// 6to4) are judged by the embedded address, so `::ffff:127.0.0.1` is not a
/// way around the loopback check.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                                     // 0.0.0.0/8 "this network"
        || a == 10                               // 10.0.0.0/8
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10 CGNAT
        || a == 127                              // loopback
        || (a == 169 && b == 254)                // link-local; cloud metadata 169.254.169.254
        || (a == 172 && (16..=31).contains(&b))  // 172.16.0.0/12
        || (a == 192 && b == 0 && c == 0)        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)        // TEST-NET-1
        || (a == 192 && b == 168)                // 192.168.0.0/16
        || (a == 198 && (b == 18 || b == 19))    // 198.18.0.0/15 benchmarking
        || (a == 198 && b == 51 && c == 100)     // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)      // TEST-NET-3
        || a >= 224) // multicast, reserved, broadcast
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = embedded_v4(&ip) {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00                 // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80                 // fe80::/10 link-local
        || (s[0] & 0xffc0) == 0xfec0                 // fec0::/10 site-local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8)        // 2001:db8::/32 documentation
        || (s[0] == 0x2001 && s[1] == 0)             // 2001::/32 Teredo
        || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)) // 100::/64 discard
}

/// The IPv4 address an IPv6 address carries, for the transition forms.
fn embedded_v4(ip: &Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    let from =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if s[..6].iter().all(|&x| x == 0) {
        // ::a.b.c.d (deprecated IPv4-compatible; also covers ::1 and ::)
        return Some(from(s[6], s[7]));
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        // 64:ff9b::/96 NAT64
        return Some(from(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        // 2002::/16 6to4
        return Some(from(s[1], s[2]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(ip: &str) -> bool {
        is_public_ip(ip.parse().unwrap())
    }

    #[test]
    fn private_and_special_ipv4_ranges_are_not_public() {
        for ip in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.255.255",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "169.254.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.0.1",
            "192.168.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!public(ip), "{ip} must not be public");
        }
    }

    #[test]
    fn ordinary_ipv4_addresses_are_public() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.0.1.1",
            "192.167.255.255",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
        ] {
            assert!(public(ip), "{ip} must be public");
        }
    }

    #[test]
    fn private_and_transition_ipv6_forms_are_not_public() {
        for ip in [
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "100::1",
            // IPv4 hidden inside IPv6:
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(!public(ip), "{ip} must not be public");
        }
    }

    #[test]
    fn ordinary_ipv6_addresses_are_public() {
        for ip in [
            "2606:4700:4700::1111",
            "2a00:1450:4001:81b::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(public(ip), "{ip} must be public");
        }
    }

    fn verdict(policy: UrlPolicy, url: &str) -> Result<(), String> {
        policy.check(&Url::parse(url).unwrap())
    }

    #[test]
    fn production_policy_refuses_the_ssrf_classics() {
        let p = UrlPolicy::public_only();
        for url in [
            "file:///etc/passwd",
            "ftp://example.org/paper.pdf",
            "gopher://example.org/",
            "data:application/pdf;base64,JVBERi0=",
            "http://user:secret@example.org/paper.pdf",
            "https://user@example.org/paper.pdf",
            "http://127.0.0.1/paper.pdf",
            "http://127.1/",
            "http://2130706433/", // decimal 127.0.0.1
            "http://0x7f000001/", // hex 127.0.0.1
            "http://0177.0.0.1/", // octal 127.0.0.1
            "http://0.0.0.0:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://10.0.0.5/paper.pdf",
            "https://192.168.1.10:8443/paper.pdf",
            "http://172.16.0.1/",
            "http://localhost/",
            "http://LOCALHOST:7433/",
            "http://localhost./",
            "http://papers.localhost/",
        ] {
            assert!(
                verdict(p, url).is_err(),
                "{url} must be refused, got {:?}",
                verdict(p, url)
            );
        }
    }

    #[test]
    fn production_policy_admits_public_http_and_https() {
        let p = UrlPolicy::public_only();
        for url in [
            "https://arxiv.org/pdf/2005.11401",
            "http://export.arxiv.org/api/query?id_list=1",
            "https://93.184.216.34/paper.pdf",
            "https://[2606:4700:4700::1111]/paper.pdf",
            "https://example.org:8443/a/b.pdf?token=abc#frag",
        ] {
            assert_eq!(verdict(p, url), Ok(()), "{url}");
        }
    }

    #[test]
    fn test_policy_admits_only_loopback_not_other_private_ranges() {
        let p = UrlPolicy::allow_loopback_for_tests();
        assert_eq!(verdict(p, "http://127.0.0.1:4000/x.pdf"), Ok(()));
        assert_eq!(verdict(p, "http://localhost:4000/x.pdf"), Ok(()));
        for url in [
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "file:///etc/passwd",
            "http://user:pw@127.0.0.1/",
        ] {
            assert!(verdict(p, url).is_err(), "{url}");
        }
    }

    #[test]
    fn parse_and_check_reports_the_url_without_its_query_string() {
        let err = UrlPolicy::public_only()
            .parse_and_check("http://10.0.0.1/paper.pdf?sig=SECRET")
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, PaperError::UnsafeUrl { .. }), "{msg}");
        assert!(msg.contains("http://10.0.0.1/paper.pdf"), "{msg}");
        assert!(!msg.contains("SECRET"), "{msg}");
    }

    #[test]
    fn garbage_urls_are_refused_not_panicked_on() {
        for raw in [
            "",
            "not a url",
            "http://",
            "://x",
            "\u{1f600}",
            "http://[::1",
        ] {
            assert!(
                matches!(
                    UrlPolicy::public_only().parse_and_check(raw),
                    Err(PaperError::UnsafeUrl { .. })
                ),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn display_url_truncates_non_urls_on_a_char_boundary() {
        let long = "日".repeat(500);
        let shown = display_url(&long);
        assert!(shown.chars().count() <= 121, "{}", shown.chars().count());
        assert!(shown.ends_with('…'));
    }

    #[tokio::test]
    async fn hostnames_resolving_to_loopback_are_refused_at_resolution() {
        // `localhost` comes from the hosts file: no network involved.
        let err = resolve_public("localhost").await.unwrap_err();
        assert!(err.downcast_ref::<Refused>().is_some(), "{err}");
    }
}
