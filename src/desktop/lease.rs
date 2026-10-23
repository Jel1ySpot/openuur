//! Desktop input ownership.
//!
//! A new desktop connection supersedes any older one. The old WebRTC peer may
//! stay connected briefly (video, terminal, file transfer), but its delayed
//! input callbacks must not type into the new session or release keys that the
//! new owner holds. Each desktop peer receives a lease; only the newest lease
//! may inject, and activation additionally follows the peer connection state.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use super::InputSink;

struct Owner {
    id: u64,
    sink: Option<Weak<dyn InputSink>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static OWNER: Mutex<Option<Owner>> = Mutex::new(None);

/// One desktop peer's claim on the shared input path.
pub struct InputLease {
    id: u64,
    sink: Arc<dyn InputSink>,
    active: AtomicBool,
}

impl InputLease {
    /// Claim ownership and release whatever the previous lease still held.
    pub fn claim(sink: Arc<dyn InputSink>) -> Arc<Self> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // Hold the owner lock through the old sink's release so an in-flight
        // event from the previous peer cannot race the replacement.
        let mut owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = owner
            .take()
            .and_then(|owner| owner.sink)
            .and_then(|sink| sink.upgrade());
        if let Some(previous) = previous {
            previous.reset();
        }
        *owner = Some(Owner {
            id,
            sink: Some(Arc::downgrade(&sink)),
        });
        drop(owner);
        Arc::new(Self {
            id,
            sink,
            active: AtomicBool::new(false),
        })
    }

    fn is_current(&self) -> bool {
        OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|owner| owner.id == self.id)
    }

    /// Called when the WebRTC peer becomes connected.
    pub fn activate(&self) {
        let owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owner.as_ref().is_some_and(|owner| owner.id == self.id)
            && !self.active.swap(true, Ordering::AcqRel)
        {
            // A reconnect must not inherit the previous connection's holds.
            self.sink.reset();
            tracing::info!("desktop input lease activated");
        }
    }

    /// Called on disconnect, peer close, and capture stop.
    pub fn deactivate(&self) {
        let owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owner.as_ref().is_some_and(|owner| owner.id == self.id)
            && self.active.swap(false, Ordering::AcqRel)
        {
            self.sink.reset();
            tracing::info!("desktop input lease paused");
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.is_current()
    }

    /// Release held keys/buttons without giving up ownership. Used for lock
    /// and suspend transitions; the same peer can resume injecting.
    pub fn release(&self) {
        let owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owner.as_ref().is_some_and(|owner| owner.id == self.id) {
            self.sink.reset();
        }
    }

    /// Inject only while this is both the newest lease and its peer is up.
    /// The owner lock is held through the action, so a replacement cannot
    /// interleave with a delayed event from this peer.
    fn with_input(&self, action: impl FnOnce(&dyn InputSink)) {
        let owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(owner) = owner.as_ref() else {
            return;
        };
        if owner.id != self.id {
            static STALE_LOGS: AtomicU64 = AtomicU64::new(0);
            if STALE_LOGS.fetch_add(1, Ordering::Relaxed) < 12 {
                tracing::warn!("input dropped: peer is no longer the desktop owner");
            }
            return;
        }
        if !self.active.load(Ordering::Acquire) {
            static INACTIVE_LOGS: AtomicU64 = AtomicU64::new(0);
            if INACTIVE_LOGS.fetch_add(1, Ordering::Relaxed) < 12 {
                tracing::warn!("input dropped: desktop peer is not active yet");
            }
            return;
        }
        action(self.sink.as_ref());
    }
}

impl Drop for InputLease {
    fn drop(&mut self) {
        let mut owner = OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owner.as_ref().is_some_and(|owner| owner.id == self.id) {
            if self.active.swap(false, Ordering::AcqRel) {
                self.sink.reset();
            }
            *owner = None;
        }
    }
}

/// Compatibility name for the per-desktop lease concept.
pub type DesktopOwner = InputLease;

/// An [`InputSink`] that forwards only through the current active lease.
pub struct LeasedInput {
    lease: Arc<InputLease>,
}

impl LeasedInput {
    pub fn new(lease: Arc<InputLease>) -> Self {
        Self { lease }
    }
}

impl InputSink for LeasedInput {
    fn pointer_absolute(&self, x: f64, y: f64) {
        self.lease.with_input(|input| input.pointer_absolute(x, y));
    }

    fn pointer_relative(&self, dx: i32, dy: i32) {
        self.lease
            .with_input(|input| input.pointer_relative(dx, dy));
    }

    fn button(&self, button: i32, pressed: bool) {
        self.lease.with_input(|input| input.button(button, pressed));
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.lease.with_input(|input| input.scroll(dx, dy));
    }

    fn key(&self, vk: i32, pressed: bool) {
        self.lease.with_input(|input| input.key(vk, pressed));
    }

    fn text(&self, text: &str) {
        self.lease.with_input(|input| input.text(text));
    }

    fn reset(&self) {
        self.lease.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct Counter {
        keys: AtomicUsize,
        resets: AtomicUsize,
    }

    impl InputSink for Counter {
        fn pointer_absolute(&self, _: f64, _: f64) {}
        fn pointer_relative(&self, _: i32, _: i32) {}
        fn button(&self, _: i32, _: bool) {}
        fn scroll(&self, _: i32, _: i32) {}
        fn key(&self, _: i32, _: bool) {
            self.keys.fetch_add(1, Ordering::Relaxed);
        }
        fn text(&self, _: &str) {}
        fn reset(&self) {
            self.resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn only_the_newest_active_lease_injects() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sink = Arc::new(Counter::default());
        let old = InputLease::claim(sink.clone());
        old.activate();
        let old_input = LeasedInput::new(old.clone());
        old_input.key(65, true);
        assert_eq!(sink.keys.load(Ordering::Relaxed), 1);

        let new = InputLease::claim(sink.clone());
        // Activation released stale holds, and claiming released the old
        // session before it could be reused.
        assert_eq!(sink.resets.load(Ordering::Relaxed), 2);
        new.activate();
        assert_eq!(sink.resets.load(Ordering::Relaxed), 3);
        old_input.key(65, true);
        assert_eq!(sink.keys.load(Ordering::Relaxed), 1);
        LeasedInput::new(new.clone()).key(65, true);
        assert_eq!(sink.keys.load(Ordering::Relaxed), 2);

        // A late disconnect from the old peer must not clear the new one.
        old.deactivate();
        assert!(new.is_active());
        new.deactivate();
        assert!(!new.is_active());
    }

    #[test]
    fn inactive_lease_does_not_inject() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sink = Arc::new(Counter::default());
        let lease = InputLease::claim(sink.clone());
        LeasedInput::new(lease.clone()).key(65, true);
        assert_eq!(sink.keys.load(Ordering::Relaxed), 0);
        lease.activate();
        LeasedInput::new(lease.clone()).key(65, true);
        assert_eq!(sink.keys.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn dropping_the_current_lease_releases_input() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sink = Arc::new(Counter::default());
        let lease = InputLease::claim(sink.clone());
        lease.activate();
        drop(lease);
        assert_eq!(sink.resets.load(Ordering::Relaxed), 2);
    }
}
