use anyhow::Result;
use hs_scribe::client::{ProgressEvent, ScribeClient};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct ScribePool {
    clients: Vec<ScribeClient>,
    next_server: AtomicUsize,
}

impl ScribePool {
    pub fn new(servers: &[String]) -> Self {
        let clients: Vec<ScribeClient> = servers.iter().map(|url| ScribeClient::new(url)).collect();
        Self {
            clients,
            next_server: AtomicUsize::new(0),
        }
    }

    /// Number of concurrent conversions to allow (2 per server).
    pub fn concurrency(&self) -> usize {
        (self.clients.len() * 2).max(1)
    }

    /// Pick the least-loaded ready server with round-robin tie-breaking.
    async fn pick_server(&self) -> Result<&ScribeClient> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        let max_avail = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|r| r.ready)
            .map(|r| r.vlm_slots_available)
            .max();

        let Some(max_avail) = max_avail else {
            anyhow::bail!("No scribe servers are ready");
        };

        let candidates: Vec<&ScribeClient> = results
            .iter()
            .filter_map(|(c, r)| {
                if let Ok(r) = r {
                    if r.ready && r.vlm_slots_available == max_avail {
                        return Some(*c);
                    }
                }
                None
            })
            .collect();

        if candidates.is_empty() {
            anyhow::bail!("No scribe servers are ready");
        }

        let idx = self.next_server.fetch_add(1, Ordering::Relaxed) % candidates.len();
        Ok(candidates[idx])
    }

    /// Convert one PDF via the best available server.
    /// Caller is responsible for limiting concurrency (e.g. via a semaphore).
    /// Returns (server_url, markdown) on success.
    pub async fn convert_one(
        &self,
        pdf_bytes: Vec<u8>,
        on_progress: impl Fn(ProgressEvent) + Send + Sync + 'static,
    ) -> Result<(String, String)> {
        let mut last_err = None;
        for attempt in 0..3 {
            match self.pick_server().await {
                Ok(client) => {
                    let url = client.url().to_string();
                    let short = url
                        .strip_prefix("http://")
                        .or_else(|| url.strip_prefix("https://"))
                        .unwrap_or(&url);
                    on_progress(ProgressEvent {
                        stage: "server".into(),
                        page: 0,
                        total_pages: 0,
                        message: format!("→ {short}"),
                    });
                    let md = client.convert_with_progress(pdf_bytes, on_progress).await?;
                    return Ok((url, md));
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No servers available")))
    }

    /// Health check all servers. Returns (url, reachable) pairs.
    pub async fn check_all(&self) -> Vec<(String, bool)> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c.url().to_string(), c.health().await.is_ok()) })
            .collect();
        futures::future::join_all(futures).await
    }
}
