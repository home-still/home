use anyhow::{Context, Result};
use hs_scribe::client::{ProgressEvent, ScribeClient};
use std::sync::Arc;

pub struct ScribePool {
    clients: Vec<ScribeClient>,
    dispatch_sem: Arc<tokio::sync::Semaphore>,
}

impl ScribePool {
    pub fn new(servers: &[String]) -> Self {
        let clients: Vec<ScribeClient> = servers.iter().map(|url| ScribeClient::new(url)).collect();
        let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(clients.len().max(1)));
        Self {
            clients,
            dispatch_sem,
        }
    }

    /// Pick the least-loaded ready server by querying /readiness on all servers.
    async fn pick_server(&self) -> Result<&ScribeClient> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        let mut best: Option<(&ScribeClient, usize)> = None;
        for (client, result) in &results {
            match result {
                Ok(r) if r.ready => match best {
                    Some((_, best_avail)) if r.vlm_slots_available <= best_avail => {}
                    _ => best = Some((client, r.vlm_slots_available)),
                },
                Ok(_) => {}  // not ready
                Err(_) => {} // unreachable
            }
        }

        best.map(|(c, _)| c).context("No scribe servers are ready")
    }

    /// Convert one PDF via the best available server.
    /// Acquires a dispatch permit to limit parallelism to server count.
    /// Convert one PDF via the best available server.
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
