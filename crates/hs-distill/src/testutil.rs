//! Test-only loopback HTTP server. Binds an ephemeral 127.0.0.1 port, so
//! tests never reach a real service.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    pub request_line: String,
    pub body: String,
}

pub(crate) enum Reply {
    /// Respond with this status and JSON body.
    Json(u16, String),
    /// Accept the request and never answer.
    Hang,
}

pub(crate) struct FakeHttp {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeHttp {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().clone()
    }
}

/// Start a server that answers every request with `reply(&request)`.
pub(crate) async fn serve(reply: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> FakeHttp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(reply);
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.clone();
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let Some(req) = read_request(&mut sock).await else {
                    return;
                };
                recorded.lock().push(req.clone());
                match reply(&req) {
                    Reply::Json(status, body) => {
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    }
                    Reply::Hang => tokio::time::sleep(Duration::from_secs(3600)).await,
                }
            });
        }
    });
    FakeHttp { addr, requests }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        request_line: head.lines().next().unwrap_or_default().to_string(),
        body: String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string(),
    })
}
