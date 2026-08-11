//! The server-sent events stream: the subscriber registry and its fan-out.
//!
//! ```text
//!   engine thread                          writer threads (one per subscriber)
//!   ─────────────                          ───────────────────────────────────
//!   Observer::state_folded ─┐
//!   Observer::rule_considered ├─▶ push ──▶ Arc<Mutex<Registry>> ──▶ pop ──▶ socket
//!   Observer::event_received ─┤            + Condvar::notify            (blocking)
//!   Observer::command_failed ─┘
//! ```
//!
//! # The one invariant that matters
//!
//! **`Observer` callbacks run on the engine thread, inside `Engine::drain`.** A
//! blocking write to a slow HTTP client from that thread would stall the run
//! loop — precisely the failure the northbound architecture exists to prevent
//! (see the module docs on why the API cannot hold the `Engine`).
//!
//! So the engine side never touches a socket. It pushes onto a **bounded**
//! per-subscriber queue and returns. Each subscriber owns a writer thread that
//! blocks on its own queue and does the I/O, so a slow client stalls only itself.
//!
//! On overflow the **oldest** frame is dropped and a `lagged` flag is set; the
//! writer then emits `event: lagged`, telling the client to re-sync with
//! `GET /devices` rather than silently believing a stale view. Dropping the
//! newest instead would be wrong — the newest frame is the one that carries
//! current truth.
//!
//! # Why the observer path must not panic
//!
//! `Engine::notify` is a plain loop with no unwind protection: a panic in any
//! `Observer` unwinds through `drain`, through `advance`, and out of the host's
//! run loop, killing the process. An optional adapter must not be able to do
//! that, so **nothing on this path may panic** — no `unwrap`, no indexing, no
//! slicing, no arithmetic that can overflow. Mutexes go through
//! the module's `lock` helper, which recovers from poisoning rather than
//! propagating.
//!
//! # Why sourcing lives on `RestApiObserver`
//!
//! The engine fans only `state_folded` to its `northbound` list; every other
//! `Observer` callback goes to the ordinary observer list alone. An adapter that
//! implemented `rule_considered` would silently never be called. All four stream
//! sources are therefore driven from [`RestApiObserver`](super::RestApiObserver),
//! which the host registers with `add_observer`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::{json, Value};

use super::lock;

/// Frames buffered per subscriber before the oldest is dropped. Sized so a
/// dashboard that pauses briefly (a background tab, a GC pause) loses nothing,
/// while a client that has genuinely stopped reading cannot grow the queue
/// without bound.
pub const QUEUE_CAPACITY: usize = 256;

/// One SSE frame: an event name and its JSON payload. Pre-rendered on the engine
/// thread so the writer thread only formats and writes bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub event: &'static str,
    pub data: Value,
}

impl Frame {
    /// The wire form: `event:` and `data:` lines, terminated by a blank line.
    ///
    /// `data` is serialized compactly and always on one line — a JSON value never
    /// contains a raw newline, so no continuation handling is needed. On the
    /// (unreachable) serialization failure a `null` payload is emitted rather
    /// than panicking; see the module docs.
    pub fn encode(&self) -> String {
        let data = serde_json::to_string(&self.data).unwrap_or_else(|_| "null".to_string());
        format!("event: {}\ndata: {}\n\n", self.event, data)
    }
}

/// One subscriber's mailbox. The engine thread pushes; the writer thread pops.
struct Subscriber {
    id: u64,
    queue: VecDeque<Frame>,
    /// Set when a push overflowed and dropped a frame. The writer emits a
    /// `lagged` event and clears it, so the client learns its view has a hole.
    lagged: bool,
    /// Cleared when the writer's socket dies or the connection is dropped, so
    /// the engine stops queueing for a subscriber nobody will ever read.
    live: bool,
}

/// Every live subscriber. Guarded by one mutex, taken only for pushes and pops —
/// **never** held across socket I/O.
#[derive(Default)]
pub struct Registry {
    subscribers: Vec<Subscriber>,
    next_id: u64,
}

/// The shared registry plus the condvar writer threads block on.
///
/// Cloned into the [`RestApiObserver`](super::RestApiObserver) (which pushes) and
/// into every writer thread (which pops).
#[derive(Clone, Default)]
pub struct Broadcaster {
    inner: Arc<BroadcasterInner>,
}

#[derive(Default)]
struct BroadcasterInner {
    registry: Mutex<Registry>,
    /// Signalled after every push so a blocked writer wakes promptly.
    ready: Condvar,
    /// Live subscriber count, readable without taking the mutex — the stream
    /// budget check on the accept path must not contend with the engine thread.
    count: AtomicU64,
    /// Set on host shutdown so writer threads unblock and exit.
    shutdown: AtomicBool,
}

impl Broadcaster {
    /// How many subscribers are attached. Used to enforce the stream budget.
    pub fn subscriber_count(&self) -> usize {
        self.inner.count.load(Ordering::SeqCst) as usize
    }

    /// Register a new subscriber, returning its handle.
    ///
    /// **Register before serializing the snapshot, not after.** Deltas that land
    /// during serialization queue behind it and are delivered after, which is
    /// correct: a superseded value is harmless, a missing one is not. Doing it in
    /// the other order reopens the lost-update window the snapshot exists to
    /// close.
    pub fn subscribe(&self) -> Subscription {
        let mut registry = lock(&self.inner.registry);
        let id = registry.next_id;
        registry.next_id = registry.next_id.wrapping_add(1);
        registry.subscribers.push(Subscriber {
            id,
            queue: VecDeque::new(),
            lagged: false,
            live: true,
        });
        let count = registry.subscribers.len() as u64;
        drop(registry);
        self.inner.count.store(count, Ordering::SeqCst);
        Subscription {
            broadcaster: self.clone(),
            id,
        }
    }

    /// Queue a frame for every live subscriber. Called on the **engine thread**:
    /// it takes the mutex, pushes, drops the guard, and returns. It never blocks
    /// on I/O and never panics.
    pub fn broadcast(&self, frame: Frame) {
        {
            let mut registry = lock(&self.inner.registry);
            if registry.subscribers.is_empty() {
                return;
            }
            for sub in registry.subscribers.iter_mut() {
                if !sub.live {
                    continue;
                }
                // Drop the *oldest* on overflow: the newest frame carries current
                // truth, so it is the one worth keeping.
                if sub.queue.len() >= QUEUE_CAPACITY {
                    sub.queue.pop_front();
                    sub.lagged = true;
                }
                sub.queue.push_back(frame.clone());
            }
        }
        // Notify outside the lock, so a woken writer does not immediately block
        // on a mutex this thread still holds.
        self.inner.ready.notify_all();
    }

    /// Unblock every writer thread so they can observe shutdown and exit.
    pub fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.ready.notify_all();
    }

    fn is_shutdown(&self) -> bool {
        self.inner.shutdown.load(Ordering::SeqCst)
    }

    /// Drop a subscriber and release its slot. Idempotent.
    fn remove(&self, id: u64) {
        let mut registry = lock(&self.inner.registry);
        registry.subscribers.retain(|s| s.id != id);
        let count = registry.subscribers.len() as u64;
        drop(registry);
        self.inner.count.store(count, Ordering::SeqCst);
    }
}

/// What one streaming connection holds. Dropping it deregisters the subscriber,
/// so a dead client is reaped with no engine involvement.
pub struct Subscription {
    broadcaster: Broadcaster,
    id: u64,
}

/// What [`Subscription::next_batch`] returned.
pub enum Batch {
    /// Frames to write, and whether a `lagged` notice should precede them.
    Frames { frames: Vec<Frame>, lagged: bool },
    /// Nothing arrived before the timeout — write a keepalive comment.
    Idle,
    /// The host is shutting down, or this subscriber was removed. Stop.
    Done,
}

impl Subscription {
    /// Block until frames are queued, the timeout elapses, or shutdown.
    ///
    /// Drains everything pending in one batch so a burst costs one wakeup and one
    /// write rather than one of each per frame.
    pub fn next_batch(&self, timeout: std::time::Duration) -> Batch {
        let inner = &self.broadcaster.inner;
        let mut registry = lock(&inner.registry);

        loop {
            if self.broadcaster.is_shutdown() {
                return Batch::Done;
            }
            let Some(sub) = registry.subscribers.iter_mut().find(|s| s.id == self.id) else {
                // Removed out from under us (host shutdown, or a duplicate
                // reap). Nothing to write and nothing to wait for.
                return Batch::Done;
            };
            if !sub.queue.is_empty() || sub.lagged {
                let frames: Vec<Frame> = sub.queue.drain(..).collect();
                let lagged = std::mem::take(&mut sub.lagged);
                return Batch::Frames { frames, lagged };
            }

            // Nothing pending: wait for a push. The guard is released while
            // blocked, so the engine thread can push freely.
            let (guard, wait) = match inner.ready.wait_timeout(registry, timeout) {
                Ok(pair) => pair,
                // Poisoned: recover the guard rather than propagating a panic
                // into this connection's thread.
                Err(poisoned) => poisoned.into_inner(),
            };
            registry = guard;
            if wait.timed_out() {
                return Batch::Idle;
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.broadcaster.remove(self.id);
    }
}

// --- frame constructors ------------------------------------------------------
//
// Built on the engine thread, so they are kept to plain JSON assembly with no
// fallible work.

pub fn state_frame(device: &str, capability: &str, value: Value) -> Frame {
    Frame {
        event: "state",
        data: json!({ "device": device, "capability": capability, "value": value }),
    }
}

pub fn rule_frame(rule: &str, truth: &str, fired: bool) -> Frame {
    Frame {
        event: "rule",
        data: json!({ "rule": rule, "truth": truth, "fired": fired }),
    }
}

/// A device action. `depth` distinguishes an event that *started* a causal chain
/// (0) from one produced by a cascade — **not** a physical press from an
/// API-injected one. Both enter the queue at depth 0, and that
/// indistinguishability is deliberate.
pub fn action_frame(device: &str, event: &str, depth: u32) -> Frame {
    Frame {
        event: "action",
        data: json!({ "device": device, "event": event, "depth": depth }),
    }
}

pub fn command_failed_frame(command: Value, reason: &str, attempts: u32) -> Frame {
    Frame {
        event: "command_failed",
        data: json!({ "command": command, "reason": reason, "attempts": attempts }),
    }
}

pub fn snapshot_frame(devices: Value) -> Frame {
    Frame {
        event: "snapshot",
        data: json!({ "devices": devices }),
    }
}

/// Emitted when a subscriber's queue overflowed: its view has a hole, so it
/// should re-sync with `GET /devices`.
pub fn lagged_frame() -> Frame {
    Frame {
        event: "lagged",
        data: json!({ "message": "frames were dropped; re-sync with GET /devices" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(n: u64) -> Frame {
        Frame {
            event: "state",
            data: json!({ "n": n }),
        }
    }

    #[test]
    fn a_frame_encodes_as_sse() {
        let frame = Frame {
            event: "state",
            data: json!({ "device": "lamp" }),
        };
        assert_eq!(
            frame.encode(),
            "event: state\ndata: {\"device\":\"lamp\"}\n\n"
        );
    }

    #[test]
    fn broadcast_reaches_every_subscriber() {
        let broadcaster = Broadcaster::default();
        let a = broadcaster.subscribe();
        let b = broadcaster.subscribe();
        assert_eq!(broadcaster.subscriber_count(), 2);

        broadcaster.broadcast(test_frame(1));

        for sub in [&a, &b] {
            match sub.next_batch(std::time::Duration::from_millis(0)) {
                Batch::Frames { frames, lagged } => {
                    assert_eq!(frames, vec![test_frame(1)]);
                    assert!(!lagged);
                }
                _ => panic!("expected frames"),
            }
        }
    }

    #[test]
    fn overflow_drops_the_oldest_and_flags_lagged() {
        let broadcaster = Broadcaster::default();
        let sub = broadcaster.subscribe();

        // One more than the queue holds.
        for n in 0..(QUEUE_CAPACITY as u64 + 1) {
            broadcaster.broadcast(test_frame(n));
        }

        match sub.next_batch(std::time::Duration::from_millis(0)) {
            Batch::Frames { frames, lagged } => {
                assert!(lagged, "overflow must be reported");
                assert_eq!(frames.len(), QUEUE_CAPACITY);
                // The oldest went, the newest stayed: the newest carries truth.
                assert_eq!(frames.first(), Some(&test_frame(1)));
                assert_eq!(frames.last(), Some(&test_frame(QUEUE_CAPACITY as u64)));
            }
            _ => panic!("expected frames"),
        }
    }

    #[test]
    fn dropping_a_subscription_deregisters_it() {
        let broadcaster = Broadcaster::default();
        let sub = broadcaster.subscribe();
        assert_eq!(broadcaster.subscriber_count(), 1);
        drop(sub);
        assert_eq!(broadcaster.subscriber_count(), 0);

        // Broadcasting with no subscribers is a no-op, not an error.
        broadcaster.broadcast(test_frame(1));
    }

    #[test]
    fn an_idle_subscriber_times_out_rather_than_blocking_forever() {
        let broadcaster = Broadcaster::default();
        let sub = broadcaster.subscribe();

        assert!(matches!(
            sub.next_batch(std::time::Duration::from_millis(10)),
            Batch::Idle
        ));
    }

    #[test]
    fn shutdown_releases_a_blocked_subscriber() {
        let broadcaster = Broadcaster::default();
        let sub = broadcaster.subscribe();
        broadcaster.shutdown();

        assert!(matches!(
            sub.next_batch(std::time::Duration::from_secs(30)),
            Batch::Done
        ));
    }
}
