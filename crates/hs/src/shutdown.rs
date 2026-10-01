//! Process-wide graceful shutdown.
//!
//! One mechanism for every command: a tokio signal listener (async-signal-safe
//! by construction: tokio's handler only writes to a self-pipe) turns SIGINT
//! into a request flag that long-running loops poll between items.
//!
//! * A command that does NOT call [`cooperative`] keeps the old behavior: the
//!   first Ctrl+C cancels it (see `main.rs`).
//! * A command that DOES call it promises to stop starting new work once
//!   [`Shutdown::requested`] is true, let the item in flight finish, print a
//!   summary of what was done and what was not, and return. `main` then waits
//!   for it up to [`GRACE`] before cancelling anyway.
//! * A second Ctrl+C always exits immediately (130): the escape hatch for a
//!   command stuck on a hung mount.
//!
//! SIGTERM is only listened for by cooperative commands (so `systemctl stop`
//! on a daemon like `hs scribe inbox run` ends it cleanly); every other
//! command keeps the default SIGTERM disposition.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use tokio::sync::watch;

/// How long `main` waits for a cooperative command to wind down after the
/// first Ctrl+C before cancelling it.
pub const GRACE: Duration = Duration::from_secs(30);

/// What a signal meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signalled {
    /// First request: stop gracefully.
    First,
    /// Already requested once: the operator wants out now.
    Repeated,
}

struct Inner {
    requested: AtomicBool,
    cooperative: AtomicBool,
    tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct Shutdown(Arc<Inner>);

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self(Arc::new(Inner {
            requested: AtomicBool::new(false),
            cooperative: AtomicBool::new(false),
            tx,
        }))
    }

    /// Record a shutdown request. Returns whether it was the first one.
    pub fn on_signal(&self) -> Signalled {
        if self.0.requested.swap(true, Ordering::SeqCst) {
            Signalled::Repeated
        } else {
            self.0.tx.send_replace(true);
            Signalled::First
        }
    }

    /// Ask for shutdown (same effect as the first signal). Tests only: in the
    /// binary, shutdown is requested by signals.
    #[cfg(test)]
    pub fn request(&self) {
        self.on_signal();
    }

    pub fn requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// Resolves once shutdown has been requested.
    pub async fn wait(&self) {
        let mut rx = self.0.tx.subscribe();
        // `wait_for` checks the current value first, so a request made before
        // this call is not missed.
        let _ = rx.wait_for(|requested| *requested).await;
    }

    /// True once a command declared itself cooperative.
    pub fn is_cooperative(&self) -> bool {
        self.0.cooperative.load(Ordering::SeqCst)
    }

    /// Listen for `kind` and treat each delivery as a shutdown request; a
    /// repeat exits the process with `exit_code`.
    #[cfg(unix)]
    pub fn listen(&self, kind: tokio::signal::unix::SignalKind, exit_code: i32) {
        let this = self.clone();
        tokio::spawn(async move {
            let Ok(mut stream) = tokio::signal::unix::signal(kind) else {
                // Registration failed: the default disposition stays, i.e. the
                // signal still terminates the process. Nothing to hide.
                return;
            };
            while stream.recv().await.is_some() {
                if this.on_signal() == Signalled::Repeated {
                    std::process::exit(exit_code);
                }
            }
        });
    }
}

static GLOBAL: LazyLock<Shutdown> = LazyLock::new(Shutdown::new);

/// The process-wide instance.
pub fn global() -> &'static Shutdown {
    &GLOBAL
}

/// Start listening for Ctrl+C. Call once, inside the runtime, before running
/// the command.
pub fn install() {
    let shutdown = global();
    #[cfg(unix)]
    shutdown.listen(tokio::signal::unix::SignalKind::interrupt(), 130);
    #[cfg(not(unix))]
    {
        let this = shutdown.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if this.on_signal() == Signalled::Repeated {
                    std::process::exit(130);
                }
            }
        });
    }
}

/// Declare the calling command cooperative (see the module docs) and get the
/// handle it must poll. Also makes SIGTERM a graceful stop.
pub fn cooperative() -> Shutdown {
    let shutdown = global();
    if !shutdown.0.cooperative.swap(true, Ordering::SeqCst) {
        #[cfg(unix)]
        shutdown.listen(tokio::signal::unix::SignalKind::terminate(), 143);
    }
    shutdown.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_signal_requests_and_repeat_is_flagged() {
        let s = Shutdown::new();
        assert!(!s.requested());
        assert_eq!(s.on_signal(), Signalled::First);
        assert!(s.requested());
        assert_eq!(s.on_signal(), Signalled::Repeated);
    }

    #[tokio::test]
    async fn wait_resolves_for_a_request_made_before_and_after() {
        let before = Shutdown::new();
        before.request();
        tokio::time::timeout(Duration::from_secs(2), before.wait())
            .await
            .expect("a request made before wait() must not be missed");

        let after = Shutdown::new();
        let waiter = tokio::spawn({
            let s = after.clone();
            async move { s.wait().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        after.request();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("wait() must wake on a later request")
            .unwrap();
    }

    /// A real signal delivered to the process reaches the flag through the
    /// listener, which is the property the old `libc::signal` handler faked.
    /// SIGUSR1 stands in for SIGINT so the test harness itself is unaffected.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_delivered_signal_sets_the_flag() {
        let s = Shutdown::new();
        s.listen(tokio::signal::unix::SignalKind::user_defined1(), 1);
        // Let the spawned task register its handler before raising.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // SAFETY: raise(3) is async-signal-safe and only signals this thread.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
        tokio::time::timeout(Duration::from_secs(2), s.wait())
            .await
            .expect("signal must set the shutdown flag");
        assert!(s.requested());
    }
}
