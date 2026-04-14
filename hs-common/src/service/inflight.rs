use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// RAII guard that increments a counter on creation and decrements on drop.
/// Used to track in-flight requests for readiness reporting.
pub struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
