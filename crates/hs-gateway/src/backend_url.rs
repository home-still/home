//! Validation of backend base URLs: the static `routes` in the gateway config
//! and the URLs devices announce through the registry.
//!
//! Both are used as `{base}{path}` when proxying, so a base must be exactly
//! `scheme://host[:port]`. Registry URLs are supplied by enrolled devices and
//! therefore get a stricter check ([`check_registrable`]).
//!
//! # What registry registration allows and refuses
//!
//! Legitimate registrants are LAN hosts announcing themselves: the only
//! in-tree registrant (`hs serve scribe|distill|mcp`) always advertises
//! `http://<its own LAN IP>:<port>`. So private ranges (10/8, 172.16/12,
//! 192.168/16, fc00::/7, CGNAT/Tailscale 100.64/10) are allowed. Refused:
//!
//! * schemes other than `http` / `https`;
//! * userinfo (`user:pass@`), a path, query or fragment;
//! * hostnames: a name cannot be checked against the blocklist below without
//!   resolving it, and a name that resolves to a safe address at registration
//!   can be re-pointed afterwards (DNS rebinding), so the host must be an IP
//!   literal;
//! * addresses that are never a LAN peer and are classic SSRF targets:
//!   loopback (127/8, ::1), unspecified / "this network" (0/8, ::), link-local
//!   (169.254/16, fe80::/10 — this covers the 169.254.169.254 cloud metadata
//!   endpoint), multicast, IPv4 broadcast, the AWS IPv6 metadata range
//!   (fd00:ec2::/32), the Alibaba metadata address (100.100.100.200), and the
//!   IPv4-mapped / NAT64 forms of any of those.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Parse `raw` as `scheme://host[:port]` and return its canonical form.
pub fn parse_backend_base(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("scheme must be http or https".into());
    }
    if url.host().is_none() {
        return Err("URL has no host".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL must not contain credentials".into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("URL must not contain a query or fragment".into());
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err("URL must not contain a path".into());
    }
    if url.port() == Some(0) {
        return Err("port 0 is not valid".into());
    }
    Ok(url)
}

/// `scheme://host[:port]`, no trailing slash.
pub fn canonical(url: &Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out
}

/// Validate and canonicalize a static route target from the gateway config.
pub fn normalize_route(raw: &str) -> Result<String, String> {
    parse_backend_base(raw).map(|u| canonical(&u))
}

/// Validate and canonicalize a URL a device asked to register.
pub fn normalize_registrable(raw: &str) -> Result<String, String> {
    let url = parse_backend_base(raw)?;
    match url.host() {
        Some(Host::Ipv4(ip)) => check_ip(IpAddr::V4(ip))?,
        Some(Host::Ipv6(ip)) => check_ip(IpAddr::V6(ip))?,
        _ => {
            return Err(
                "host must be an IP address literal (hostnames cannot be checked against the \
                 loopback/link-local/metadata blocklist)"
                    .into(),
            )
        }
    }
    Ok(canonical(&url))
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    match blocked_reason(ip) {
        Some(reason) => Err(format!(
            "{ip} is not an acceptable backend address: {reason}"
        )),
        None => Ok(()),
    }
}

fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return blocked_v4(mapped);
            }
            blocked_v6(v6)
        }
    }
}

fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_loopback() {
        Some("loopback")
    } else if o[0] == 0 {
        Some("unspecified / this-network")
    } else if ip.is_link_local() {
        Some("link-local (cloud metadata range)")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if ip.is_broadcast() {
        Some("broadcast")
    } else if o == [100, 100, 100, 200] {
        Some("cloud metadata address")
    } else {
        None
    }
}

fn blocked_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let s = ip.segments();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if s[0] & 0xffc0 == 0xfe80 {
        Some("link-local")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if s[0] == 0xfd00 && s[1] == 0x0ec2 {
        Some("cloud metadata range")
    } else if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        Some("NAT64 (embeds an arbitrary IPv4 address)")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_addresses_are_registrable_and_canonicalized() {
        for (raw, want) in [
            ("http://192.168.1.10:7433", "http://192.168.1.10:7433"),
            ("http://10.0.0.5:7433/", "http://10.0.0.5:7433"),
            ("http://172.16.5.5:7434", "http://172.16.5.5:7434"),
            ("https://192.0.2.7:8443", "https://192.0.2.7:8443"),
            ("http://100.64.1.1:7433", "http://100.64.1.1:7433"),
            ("http://[fd12:3456::1]:7433", "http://[fd12:3456::1]:7433"),
        ] {
            assert_eq!(normalize_registrable(raw).as_deref(), Ok(want), "{raw}");
        }
    }

    #[test]
    fn ssrf_targets_are_refused() {
        for raw in [
            "http://127.0.0.1:7433",
            "http://127.1.2.3:7433",
            "http://[::1]:7433",
            "http://0.0.0.0:7433",
            "http://169.254.169.254:80",
            "http://169.254.10.10:7433",
            "http://[fe80::1]:7433",
            "http://[fd00:ec2::254]:80",
            "http://100.100.100.200:80",
            "http://224.0.0.1:7433",
            "http://255.255.255.255:7433",
            "http://[::ffff:127.0.0.1]:7433",
            "http://[::ffff:169.254.169.254]:80",
            "http://[64:ff9b::7f00:1]:80",
        ] {
            assert!(normalize_registrable(raw).is_err(), "{raw} must be refused");
        }
    }

    #[test]
    fn malformed_or_unexpected_shapes_are_refused() {
        for raw in [
            "ftp://192.168.1.10:21",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://user:pw@192.168.1.10:7433",
            "http://192.168.1.10:7433/scribe",
            "http://192.168.1.10:7433/?x=1",
            "http://192.168.1.10:7433/#frag",
            "http://192.168.1.10:0",
            "http://scribe.example.local:7433",
            "http://localhost:7433",
            "not a url",
            "",
        ] {
            assert!(
                normalize_registrable(raw).is_err(),
                "{raw:?} must be refused"
            );
        }
    }

    #[test]
    fn static_routes_may_use_hostnames_and_loopback() {
        assert_eq!(
            normalize_route("http://127.0.0.1:7445").as_deref(),
            Ok("http://127.0.0.1:7445")
        );
        assert_eq!(
            normalize_route("http://scribe.example.local:7433/").as_deref(),
            Ok("http://scribe.example.local:7433")
        );
        assert!(normalize_route("http://host:7433/sub").is_err());
        assert!(normalize_route("ftp://host").is_err());
    }
}
