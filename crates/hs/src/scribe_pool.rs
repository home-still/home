use anyhow::Result;
use hs_common::service::pool::ServicePool;
use hs_scribe::client::{ProgressEvent, ScribeClient};
use std::time::Duration;

pub struct ScribePool {
    inner: ServicePool<ScribeClient>,
}

impl ScribePool {
    /// Build a pool whose `ScribeClient`s carry the given convert-request
    /// timeout. The timeout caps each PDF conversion so a stuck backend
    /// (e.g. Ollama hang) can't pin dispatchers indefinitely.
    pub fn new(servers: &[String], convert_timeout: Duration) -> Self {
        let clients: Vec<ScribeClient> = servers
            .iter()
            .map(|url| ScribeClient::new_with_timeout(url, convert_timeout))
            .collect();
        Self {
            inner: ServicePool::new(clients),
        }
    }

    /// Number of concurrent conversions to allow (2 per server).
    pub fn concurrency(&self) -> usize {
        self.inner.concurrency()
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
            match self.inner.pick_server().await {
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
        self.inner.check_all().await
    }
}
