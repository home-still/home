use anyhow::{Context, Result};
use hs_scribe::client::{ProgressEvent, ScribeClient};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub struct ScribePool {
    clients: Vec<ScribeClient>,
    dispatch_sem: Arc<tokio::sync::Semaphore>,
    next_server: AtomicUsize,
}

impl ScribePool {
    pub fn new(servers: &[String]) -> Self {
        let clients: Vec<ScribeClient> = servers.iter().map(|url| ScribeClient::new(url)).collect();
        let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(clients.len().max(1)));
        Self {
            clients,
            dispatch_sem,
            next_server: AtomicUsize::new(0),
        }
    }

    /// Pick the least-loaded ready server with round-robin tie-breaking.
    async fn pick_server(&self) -> Result<&ScribeClient> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        // Find the highest available slot count among ready servers
        let max_avail = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|r| r.ready)
            .map(|r| r.vlm_slots_available)
            .max();

        let Some(max_avail) = max_avail else {
            anyhow::bail!("No scribe servers are ready");
        };

        // Collect all servers tied at the max
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

        // Round-robin among tied candidates
        let idx = self.next_server.fetch_add(1, Ordering::Relaxed) % candidates.len();
        Ok(candidates[idx])
    }

    /// Convert one PDF via the best available server.
    /// Acquires a dispatch permit to limit parallelism to server count.
    /// Returns (server_url, markdown) on success.
    pub async fn convert_one(
        &self,
        pdf_bytes: Vec<u8>,
        on_progress: impl Fn(ProgressEvent) + Send + Sync + 'static,
    ) -> Result<(String, String)> {
        let _permit = self
            .dispatch_sem
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("Dispatch semaphore closed: {e}"))?;

        // Try up to 3 times to find a ready server
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
