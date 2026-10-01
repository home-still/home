//! Validation of the operator-supplied backend URLs in `cloud.gateway.routes`.
//!
//! A route is used as `{base}{path}` when proxying, so it must be exactly
//! `scheme://host[:port]`. Routes are written by the operator, so loopback and
//! private addresses are fine (the documented mcp route is loopback).

use url::Url;

/// Validate `raw` as `http(s)://host[:port]` (no credentials, path, query or
/// fragment) and return its canonical form without a trailing slash.
pub fn normalize_route(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("scheme must be http or https".into());
    }
    let Some(host) = url.host_str() else {
        return Err("URL has no host".into());
    };
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
    let mut out = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    Ok(out)
}

/// The gateway's own public origin: like a route, but https only.
pub fn canonical_origin(url: &Url) -> String {
    let mut out = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_may_use_hostnames_loopback_and_private_addresses() {
        for (raw, want) in [
            ("http://127.0.0.1:7445", "http://127.0.0.1:7445"),
            (
                "http://scribe.example.local:7433/",
                "http://scribe.example.local:7433",
            ),
            ("https://192.0.2.7:8443", "https://192.0.2.7:8443"),
            ("http://[::1]:7433", "http://[::1]:7433"),
        ] {
            assert_eq!(normalize_route(raw).as_deref(), Ok(want), "{raw}");
        }
    }

    #[test]
    fn malformed_routes_are_refused() {
        for raw in [
            "ftp://host",
            "file:///etc/passwd",
            "http://user:pw@host:7433",
            "http://host:7433/sub",
            "http://host:7433/?x=1",
            "http://host:7433/#f",
            "http://host:0",
            "host:7433",
            "not a url",
            "",
        ] {
            assert!(normalize_route(raw).is_err(), "{raw:?}");
        }
    }
}
