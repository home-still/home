//! Are the gateway's backends reachable?
//!
//! The gateway proxies to other hosts. A backend that cannot be
//! reached — because the firewall drops the port, or the host is down,
//! or the address is wrong — makes **every** request to that service
//! fail, and the only symptom is a `502` or a `504` in a browser.
//!
//! The check runs at startup, from the gateway's own host, because that
//! is the only vantage point from which "can a client reach the
//! backend?" is the same question. A backend that answers its own
//! `/health` on its own host proves nothing: a firewall that drops the
//! port leaves the service perfectly healthy in place and unreachable
//! from everywhere else.
//!
//! A backend that is down is **not** fatal. The gateway's job is to
//! proxy; a backend that is temporarily away must not take the gateway
//! with it, or every other service stops working because one host is
//! rebooting. So the check reports, loudly, and the gateway starts.

use std::time::Duration;

/// How long to wait for a backend's `/health`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The result of probing one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reachability {
    /// The backend answered its `/health` with a success status.
    Reachable,
    /// The backend answered, but not with a success status.
    Answered(u16),
    /// The backend could not be reached at all.
    Unreachable(String),
}

/// Probe one backend's `/health`.
///
/// The backend's `Authorization` header is not sent: `/health` is the
/// one route every backend leaves open, and sending a token the probe
/// does not need would turn a reachability question into a
/// authentication one.
pub async fn probe(base_url: &str) -> Reachability {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => return Reachability::Unreachable(format!("cannot build the client: {e}")),
    };
    match client.get(&url).send().await {
        Ok(r) => {
            let status = r.status().as_u16();
            if r.status().is_success() {
                Reachability::Reachable
            } else {
                Reachability::Answered(status)
            }
        }
        Err(e) => Reachability::Unreachable(describe(&e)),
    }
}

/// A one-line description of why a backend could not be reached.
///
/// `reqwest`'s own message names the error kind; what the operator
/// needs is the **action**: a timeout means a dropped packet (a
/// firewall), a refusal means nothing is listening.
fn describe(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out — the host did not answer (a firewall that drops the port?)".into();
    }
    if e.is_connect() {
        return "connection refused — nothing is listening on that port".into();
    }
    e.to_string()
}

/// Probe every backend of every service, and report what is not
/// reachable.
///
/// The report is a list of lines the caller logs. It is **not** an
/// error: a backend that is down must not stop the gateway.
pub async fn report(routes: &std::collections::HashMap<String, Vec<String>>) -> Vec<String> {
    let mut lines = Vec::new();
    for (service, urls) in routes {
        for url in urls {
            let verdict = probe(url).await;
            match verdict {
                Reachability::Reachable => {}
                Reachability::Answered(code) => lines.push(format!(
                    "{service} backend {url} answered HTTP {code} on /health — it is not healthy"
                )),
                Reachability::Unreachable(why) => lines.push(format!(
                    "{service} backend {url} is not reachable ({why}) — every request to \
                     `{service}` will fail until it is"
                )),
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that is not listening must be reported as unreachable,
    /// with a reason that names the **action** — not reqwest's raw
    /// error kind.
    #[tokio::test]
    async fn a_backend_that_is_not_listening_is_reported() {
        // Port 1 on the loopback: nothing listens there.
        let verdict = probe("http://127.0.0.1:1").await;
        match verdict {
            Reachability::Unreachable(why) => {
                assert!(
                    why.contains("refused") || why.contains("timed out"),
                    "the reason must name what to look at: {why}"
                );
            }
            other => panic!("expected unreachable, got {other:?}"),
        }
    }

    /// A backend that answers with a non-success status is reported as
    /// answering — and named as not healthy. A `401` means the token is
    /// wrong, which is a different problem from an unreachable host.
    #[tokio::test]
    async fn a_backend_that_answers_with_an_error_is_reported_as_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
                .await;
        });
        let verdict = probe(&format!("http://127.0.0.1:{port}")).await;
        assert_eq!(verdict, Reachability::Answered(401));
    }

    /// A backend that answers `/health` with a success status is
    /// reachable.
    #[tokio::test]
    async fn a_healthy_backend_is_reachable() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        });
        let verdict = probe(&format!("http://127.0.0.1:{port}")).await;
        assert_eq!(verdict, Reachability::Reachable);
    }

    /// The report names the service and the backend, so the operator
    /// knows **which** route is dead.
    #[tokio::test]
    async fn the_report_names_the_service_and_the_backend() {
        let mut routes = std::collections::HashMap::new();
        routes.insert("docs".to_string(), vec!["http://127.0.0.1:1".to_string()]);
        let lines = report(&routes).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("docs"), "{}", lines[0]);
        assert!(lines[0].contains("127.0.0.1:1"), "{}", lines[0]);
    }
}
