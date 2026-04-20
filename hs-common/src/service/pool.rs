use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;

use super::protocol::{ReadinessInfo, ServiceClient};

/// Generic server pool with readiness-based selection and round-robin tie-breaking.
pub struct ServicePool<C: ServiceClient> {
    clients: Vec<C>,
    next: AtomicUsize,
}

impl<C: ServiceClient> ServicePool<C> {
    pub fn new(clients: Vec<C>) -> Self {
        Self {
            clients,
            next: AtomicUsize::new(0),
        }
    }

    /// Number of concurrent operations to allow (2 per server).
    pub fn concurrency(&self) -> usize {
        (self.clients.len() * 2).max(1)
    }

    /// Pick the least-loaded ready server with round-robin tie-breaking.
    pub async fn pick_server(&self) -> Result<&C> {
        let futures: Vec<_> = self
            .clients
            .iter()
            .map(|c| async move { (c, c.readiness().await) })
            .collect();
        let results = futures::future::join_all(futures).await;

        for (c, r) in &results {
            match r {
                Err(e) => tracing::warn!(
                    server = %c.url(),
                    error = %e,
                    "readiness probe failed; excluding from this dispatch"
                ),
                Ok(rr) if !rr.is_ready() => tracing::debug!(
                    server = %c.url(),
                    "server has no available slots; excluding from this dispatch"
                ),
                _ => {}
            }
        }

        let max_avail = results
            .iter()
            .filter_map(|(_, r)| r.as_ref().ok())
            .filter(|r| r.is_ready())
            .map(|r| r.available_slots())
            .max();

        let Some(max_avail) = max_avail else {
            anyhow::bail!("No servers are ready");
        };

        let candidates: Vec<&C> = results
            .iter()
            .filter_map(|(c, r)| {
                if let Ok(r) = r {
                    if r.is_ready() && r.available_slots() == max_avail {
                        return Some(*c);
                    }
                }
                None
            })
            .collect();

        if candidates.is_empty() {
            anyhow::bail!("No servers are ready");
        }

        let idx = self.next.fetch_add(1, Ordering::Relaxed) % candidates.len();
        Ok(candidates[idx])
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

    /// Get a reference to all clients.
    pub fn clients(&self) -> &[C] {
        &self.clients
    }
}
