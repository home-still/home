//! A pool of lazily reloadable model slots.
//!
//! Each slot is a `std::sync::Mutex<Option<M>>`: `None` after the idle
//! sweeper released the weights, reloaded on the next use. A panic while a
//! slot is held poisons its mutex; that state is surfaced
//! ([`SlotPool::poisoned_slot`], [`SlotError::Poisoned`]) and never papered
//! over, because the model behind it may be mid-inference in an unknown
//! state (the ONNX session is not unwind-safe).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, TryLockError};

#[derive(Debug, PartialEq, Eq)]
pub enum SlotError<E> {
    /// The slot's lock was poisoned by an earlier panic.
    Poisoned { slot: usize },
    /// Loading or running the model failed.
    Failed(E),
}

/// What one [`SlotPool::release_idle`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Released {
    /// Slots whose model was dropped.
    pub released: usize,
    /// Slots in use by an embed; left alone.
    pub busy: usize,
    /// Slots whose lock is poisoned; left alone.
    pub poisoned: usize,
}

pub struct SlotPool<M> {
    slots: Vec<Mutex<Option<M>>>,
    next: AtomicUsize,
}

impl<M> SlotPool<M> {
    /// A pool holding `first` and then `rest`, so it is never empty.
    pub fn new(first: M, rest: Vec<M>) -> Self {
        let slots = std::iter::once(first)
            .chain(rest)
            .map(|m| Mutex::new(Some(m)))
            .collect();
        Self {
            slots,
            next: AtomicUsize::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Run `f` on the next slot's model (round robin), first loading it with
    /// `load` if the slot is empty. Blocks while another call holds the slot.
    pub fn run<R, E>(
        &self,
        load: impl FnOnce() -> Result<M, E>,
        f: impl FnOnce(&mut M) -> Result<R, E>,
    ) -> Result<R, SlotError<E>> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        let mut guard = self.slots[idx]
            .lock()
            .map_err(|_| SlotError::Poisoned { slot: idx })?;
        if guard.is_none() {
            *guard = Some(load().map_err(SlotError::Failed)?);
        }
        let model = guard
            .as_mut()
            .expect("slot was filled above while the lock was held");
        f(model).map_err(SlotError::Failed)
    }

    /// The first slot whose lock a panic poisoned, if any. Never blocks.
    pub fn poisoned_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.is_poisoned())
    }

    /// Drop the model of every slot that is not in use. Never waits on a
    /// busy slot — a slot being used is by definition not idle.
    pub fn release_idle(&self) -> Released {
        let mut out = Released::default();
        for slot in &self.slots {
            match slot.try_lock() {
                Ok(mut guard) => {
                    if guard.take().is_some() {
                        out.released += 1;
                    }
                }
                Err(TryLockError::WouldBlock) => out.busy += 1,
                Err(TryLockError::Poisoned(_)) => out.poisoned += 1,
            }
        }
        out
    }
}

/// True once `now_ms - last_used_ms` has reached the idle window.
pub fn is_idle(now_ms: i64, last_used_ms: i64, idle_window_ms: i64) -> bool {
    now_ms.saturating_sub(last_used_ms) >= idle_window_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::Arc;

    type Pool = SlotPool<Vec<u32>>;

    fn pool(n: usize) -> Pool {
        SlotPool::new(vec![0], (1..n as u32).map(|i| vec![i]).collect())
    }

    #[test]
    fn run_round_robins_over_the_slots() {
        let p = pool(3);
        let seen: Vec<u32> = (0..6)
            .map(|_| p.run(|| Ok::<_, ()>(vec![99]), |m| Ok(m[0])).unwrap())
            .collect();
        assert_eq!(seen, [0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn released_slot_is_reloaded_on_next_use() {
        let p = pool(1);
        assert_eq!(p.release_idle().released, 1);
        let mut loads = 0;
        let got = p
            .run(
                || {
                    loads += 1;
                    Ok::<_, ()>(vec![7])
                },
                |m| Ok(m[0]),
            )
            .unwrap();
        assert_eq!((got, loads), (7, 1));
        // Second use does not reload.
        p.run(
            || -> Result<_, ()> { unreachable!("already loaded") },
            |m| Ok(m[0]),
        )
        .unwrap();
    }

    #[test]
    fn a_failed_load_is_reported_and_leaves_the_slot_empty() {
        let p = pool(1);
        p.release_idle();
        let err = p
            .run(|| Err::<Vec<u32>, _>("oom"), |m| Ok::<_, &str>(m[0]))
            .unwrap_err();
        assert_eq!(err, SlotError::Failed("oom"));
        assert!(p.poisoned_slot().is_none());
    }

    #[test]
    fn a_panic_in_a_slot_is_surfaced_as_poison_forever_after() {
        let p = pool(2);
        assert_eq!(p.poisoned_slot(), None);

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _ = p.run(
                || Ok::<_, ()>(vec![0]),
                |_m| -> Result<(), ()> { panic!("simulated ORT panic") },
            );
        }));
        assert!(panicked.is_err());

        assert_eq!(p.poisoned_slot(), Some(0));
        // Round robin is now at slot 1 (healthy) and then slot 0 again.
        assert!(p.run(|| Ok::<_, ()>(vec![0]), |m| Ok(m[0])).is_ok());
        assert_eq!(
            p.run(|| Ok::<_, ()>(vec![0]), |m| Ok(m[0])).unwrap_err(),
            SlotError::Poisoned { slot: 0 }
        );
        // The sweeper does not touch it either.
        let r = p.release_idle();
        assert_eq!(r.poisoned, 1);
        assert_eq!(r.released, 1);
    }

    #[test]
    fn release_idle_skips_a_slot_that_is_in_use() {
        let p = Arc::new(pool(1));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (leave_tx, leave_rx) = std::sync::mpsc::channel::<()>();
        let worker = {
            let p = p.clone();
            std::thread::spawn(move || {
                p.run(
                    || Ok::<_, ()>(vec![1]),
                    |_m| {
                        entered_tx.send(()).unwrap();
                        leave_rx.recv().unwrap();
                        Ok(())
                    },
                )
                .unwrap();
            })
        };
        entered_rx.recv().unwrap();
        assert_eq!(
            p.release_idle(),
            Released {
                released: 0,
                busy: 1,
                poisoned: 0
            }
        );
        leave_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(p.release_idle().released, 1);
    }

    #[test]
    fn idle_window_is_inclusive_and_clock_skew_safe() {
        assert!(!is_idle(1_000, 600, 500));
        assert!(is_idle(1_100, 600, 500));
        assert!(is_idle(2_000, 600, 500));
        // A last-used stamp from the future (clock step) is not idle.
        assert!(!is_idle(100, 10_000, 500));
    }
}
