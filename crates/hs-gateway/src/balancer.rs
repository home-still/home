//! Backend selection: the static `cloud.gateway.routes` are the only source of
//! truth. A service has one URL or a list; requests round-robin across the
//! list. Health is passive: an instance that failed to accept a connection is
//! skipped for a short cooldown. If every instance is cooling down the
//! rotation continues anyway (an attempt beats a certain 502).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

struct Pool {
    urls: Vec<String>,
    next: AtomicUsize,
    down_until: Vec<Mutex<Option<Instant>>>,
}

pub struct Balancer {
    pools: HashMap<String, Pool>,
    cooldown: Duration,
}

/// The chosen instance: its base URL and its index (for [`Balancer::mark_failed`]).
pub struct Pick {
    pub index: usize,
    pub url: String,
}

impl Balancer {
    pub fn new(routes: &HashMap<String, Vec<String>>, cooldown: Duration) -> Self {
        let pools = routes
            .iter()
            .map(|(service, urls)| {
                (
                    service.clone(),
                    Pool {
                        urls: urls.clone(),
                        next: AtomicUsize::new(0),
                        down_until: urls.iter().map(|_| Mutex::new(None)).collect(),
                    },
                )
            })
            .collect();
        Self { pools, cooldown }
    }

    /// Next instance for `service`, or `None` if the service has no route.
    pub fn pick(&self, service: &str) -> Option<Pick> {
        let pool = self.pools.get(service)?;
        let n = pool.urls.len();
        let start = pool.next.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let healthy = (0..n)
            .map(|i| (start + i) % n)
            .find(|&i| pool.down_until[i].lock().is_none_or(|t| t <= now));
        let index = healthy.unwrap_or(start % n);
        Some(Pick {
            index,
            url: pool.urls[index].clone(),
        })
    }

    /// Record that connecting to instance `index` of `service` failed.
    pub fn mark_failed(&self, service: &str, index: usize) {
        if let Some(pool) = self.pools.get(service) {
            if let Some(slot) = pool.down_until.get(index) {
                *slot.lock() = Some(Instant::now() + self.cooldown);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balancer(cooldown: Duration) -> Balancer {
        let mut routes = HashMap::new();
        routes.insert(
            "scribe".to_string(),
            vec![
                "http://a:1".to_string(),
                "http://b:1".to_string(),
                "http://c:1".to_string(),
            ],
        );
        routes.insert("mcp".to_string(), vec!["http://m:1".to_string()]);
        Balancer::new(&routes, cooldown)
    }

    fn urls(b: &Balancer, service: &str, n: usize) -> Vec<String> {
        (0..n).map(|_| b.pick(service).unwrap().url).collect()
    }

    #[test]
    fn round_robins_across_the_list() {
        let b = balancer(Duration::from_secs(60));
        assert_eq!(
            urls(&b, "scribe", 6),
            [
                "http://a:1",
                "http://b:1",
                "http://c:1",
                "http://a:1",
                "http://b:1",
                "http://c:1"
            ]
        );
        assert_eq!(urls(&b, "mcp", 2), ["http://m:1", "http://m:1"]);
        assert!(b.pick("distill").is_none());
    }

    #[test]
    fn a_failed_instance_is_skipped_during_its_cooldown_then_returns() {
        let b = balancer(Duration::from_millis(80));
        let bad = b.pick("scribe").unwrap();
        assert_eq!(bad.url, "http://a:1");
        b.mark_failed("scribe", bad.index);
        let seen = urls(&b, "scribe", 6);
        assert!(!seen.contains(&"http://a:1".to_string()), "{seen:?}");
        assert!(
            seen.contains(&"http://b:1".to_string()) && seen.contains(&"http://c:1".to_string())
        );

        std::thread::sleep(Duration::from_millis(120));
        assert!(urls(&b, "scribe", 3).contains(&"http://a:1".to_string()));
    }

    #[test]
    fn when_everything_is_cooling_down_requests_still_rotate() {
        let b = balancer(Duration::from_secs(60));
        for i in 0..3 {
            b.mark_failed("scribe", i);
        }
        let seen = urls(&b, "scribe", 3);
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|u| u.starts_with("http://")));
    }
}
