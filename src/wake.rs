//! A cross-thread wakeup for the host run loop.
//!
//! The engine core is event-driven and deterministic: it only ever advances on
//! explicit `inject`/`advance` calls. But a *host* driving it in real time needs
//! to decide *when* to make those calls. Two things can mean "there's work to do":
//!
//! 1. **A scheduled wake** — a timer comes due, or the clock rolls to a new
//!    minute. The host knows these times ahead (see `Engine::next_wake_delay`),
//!    so it can sleep exactly until the soonest one.
//! 2. **Inbound I/O** — a device reports state, a button is pressed. These arrive
//!    asynchronously on a transport's background thread, at unpredictable times.
//!
//! Without (2) the host would have to poll on a short fixed interval just in case
//! I/O showed up — burning wakeups when idle and adding latency. The [`Waker`]
//! closes that gap: a transport thread calls [`Waker::wake`] right after it queues
//! inbound events, and the host's [`WakeListener::wait`] — blocked until the next
//! scheduled wake — returns immediately. So the loop sleeps until the *earlier* of
//! "next scheduled wake" or "inbound arrived", and never spins.
//!
//! This lives entirely outside the deterministic core. Tests drive `advance` by
//! hand and never touch it; replay is unaffected.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// A cloneable handle a transport's background thread calls to wake the host.
#[derive(Clone)]
pub struct Waker(Sender<()>);

impl Waker {
    /// Signal that inbound work is queued. Best-effort: if the listener has been
    /// dropped (host shutting down), the signal is silently discarded.
    pub fn wake(&self) {
        let _ = self.0.send(());
    }
}

/// Why [`WakeListener::wait`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeReason {
    /// A [`Waker`] signaled inbound work.
    Signaled,
    /// The timeout elapsed — a scheduled wake (timer/clock) is due.
    TimedOut,
}

/// The host side: block until a signal or a timeout.
pub struct WakeListener(Receiver<()>);

impl WakeListener {
    /// Block until a [`Waker`] signals or `timeout` elapses, whichever is first.
    ///
    /// Signals coalesce: several wakes collapse into one return, and the host's
    /// next drain picks up everything queued. If no `Waker` is live at all (e.g. a
    /// mock build with no async transports), there is nothing that could ever
    /// signal, so this degrades to a plain timed sleep — which is correct, since
    /// the only thing left to wait for is the scheduled timeout.
    pub fn wait(&self, timeout: Duration) -> WakeReason {
        match self.0.recv_timeout(timeout) {
            Ok(()) => WakeReason::Signaled,
            Err(RecvTimeoutError::Timeout) => WakeReason::TimedOut,
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(timeout);
                WakeReason::TimedOut
            }
        }
    }
}

/// Create a linked `(Waker, WakeListener)` pair. Clone the `Waker` to as many
/// transports as you like; the host holds the single `WakeListener`.
pub fn wake_channel() -> (Waker, WakeListener) {
    let (tx, rx) = channel();
    (Waker(tx), WakeListener(rx))
}
