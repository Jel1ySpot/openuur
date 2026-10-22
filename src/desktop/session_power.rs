//! Process-wide sleep inhibition for active remote sessions.
//!
//! The official clients expect the host to stay reachable while a session is
//! open, so the first live peer takes a logind `sleep`/`idle` block inhibitor
//! and the last peer releases it. The inhibitor handle is the file descriptor
//! logind returns: dropping the last reference closes it and revokes the block
//! without another D-Bus round trip.
//!
//! Captures do not talk to the bus. They subscribe to [`RemoteSessionPower::subscribe`]
//! and pause while `PrepareForSleep(true)` is in effect, which keeps a busy
//! PipeWire loop, portal recovery, and input injection out of the suspend path.
//! `UU_PREVENT_SLEEP=0` disables both the inhibitor and the pause behaviour.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;
use zbus::zvariant::OwnedFd;

use super::desktop_power::CALL_TIMEOUT;

pub(crate) const LOGIND_DEST: &str = "org.freedesktop.login1";
pub(crate) const LOGIND_PATH: &str = "/org/freedesktop/login1";
pub(crate) const LOGIND_IFACE: &str = "org.freedesktop.login1.Manager";
/// Upper bound on the whole asynchronous setup, including bus handshakes that
/// are not covered by a single call timeout.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff between PrepareForSleep re-subscriptions after a bus reset.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(2);

/// Reference-counted holder of the process-wide inhibitor.
pub struct RemoteSessionPower;

static POWER: OnceLock<Mutex<PowerState<Services>>> = OnceLock::new();
static SLEEP: OnceLock<watch::Sender<bool>> = OnceLock::new();
/// Generation-stamped sleep state. A listener captured by an older generation
/// can never publish, so a late `PrepareForSleep(true)` cannot outlive the
/// guard that owns it.
static GATE: OnceLock<Mutex<SleepGate>> = OnceLock::new();

/// Keeps the process-wide inhibitor alive until every guard is dropped.
#[must_use = "dropping the guard releases the process-wide sleep inhibitor"]
pub struct RemoteSessionPowerGuard {
    _private: (),
}

impl RemoteSessionPower {
    /// Take one reference on the process-wide inhibitor.
    ///
    /// This is synchronous on purpose: it runs on the signaling loop, and the
    /// system-bus handshake must not stall incoming messages. The handshake
    /// starts in the background on the first reference and degrades to a
    /// refcount-only guard when no system bus or runtime is available.
    pub fn acquire() -> RemoteSessionPowerGuard {
        let epoch = power_state()
            .lock()
            .map(|mut state| state.acquire())
            .ok()
            .flatten();
        if let Some(epoch) = epoch {
            let generation = begin_generation();
            if prevent_sleep_enabled() {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let services = match tokio::time::timeout(
                            SETUP_TIMEOUT,
                            Services::acquire(generation),
                        )
                        .await
                        {
                            Ok(services) => services,
                            Err(_) => {
                                tracing::warn!("session power setup timed out");
                                Services::empty()
                            }
                        };
                        install_services(epoch, services);
                    });
                }
            }
        }
        RemoteSessionPowerGuard { _private: () }
    }

    /// Watches whether `PrepareForSleep(true)` is currently in effect.
    pub fn subscribe() -> watch::Receiver<bool> {
        sleep_sender().subscribe()
    }

    pub fn sleeping() -> bool {
        gate()
            .lock()
            .map(|gate| gate.sleeping)
            .unwrap_or_else(|poisoned| poisoned.into_inner().sleeping)
    }
}

impl Drop for RemoteSessionPowerGuard {
    fn drop(&mut self) {
        let released = power_state()
            .lock()
            .map(|mut state| {
                let services = state.release();
                (services, state.count == 0)
            })
            .ok();
        if let Some((services, last)) = released {
            if let Some(services) = services {
                // Dropping the logind fd closes the inhibitor; the GNOME guard
                // already releases itself from a short-lived thread.
                services.stop();
            }
            if last {
                // Invalidate any listener still running and make a later
                // session start unpaused even if the host was suspended when
                // the last peer went away.
                reset_gate();
            }
        }
    }
}

fn power_state() -> &'static Mutex<PowerState<Services>> {
    POWER.get_or_init(|| Mutex::new(PowerState::default()))
}

fn sleep_sender() -> &'static watch::Sender<bool> {
    SLEEP.get_or_init(|| watch::channel(false).0)
}

fn gate() -> &'static Mutex<SleepGate> {
    GATE.get_or_init(|| Mutex::new(SleepGate::default()))
}

/// Invalidates older listeners and returns the generation they must present.
fn begin_generation() -> u64 {
    gate()
        .lock()
        .map(|mut gate| {
            gate.generation = gate.generation.wrapping_add(1);
            gate.generation
        })
        .unwrap_or(0)
}

fn generation_current(generation: u64) -> bool {
    gate()
        .lock()
        .map(|gate| gate.generation == generation)
        .unwrap_or(false)
}

/// Publish a suspend/resume transition only for the generation that owns the
/// inhibitor now. The check and the watch update share one lock, so a release
/// that bumps the generation is never overwritten by an in-flight listener.
fn publish_from(generation: u64, sleeping: bool) {
    let publish = {
        let Ok(mut gate) = gate().lock() else {
            return;
        };
        if gate.generation != generation || gate.sleeping == sleeping {
            false
        } else {
            gate.sleeping = sleeping;
            true
        }
    };
    if publish {
        let _ = sleep_sender().send_replace(sleeping);
    }
}

/// Reset to awake and invalidate every listener captured before this point.
fn reset_gate() {
    let changed = {
        let Ok(mut gate) = gate().lock() else {
            return;
        };
        gate.generation = gate.generation.wrapping_add(1);
        let changed = gate.sleeping;
        gate.sleeping = false;
        changed
    };
    if changed {
        let _ = sleep_sender().send_replace(false);
    }
}

pub(crate) fn prevent_sleep_enabled() -> bool {
    prevent_sleep_enabled_value(std::env::var("UU_PREVENT_SLEEP").ok().as_deref())
}

fn prevent_sleep_enabled_value(value: Option<&str>) -> bool {
    value.map_or(true, |value| value != "0")
}

fn install_services(epoch: u64, services: Services) {
    let discarded = power_state()
        .lock()
        .map(|mut state| state.install(epoch, services))
        .ok()
        .flatten();
    if let Some(services) = discarded {
        // Revoked services must be stopped, not merely dropped: a dropped
        // JoinHandle only detaches the listener task.
        services.stop();
    }
}

struct SleepGate {
    generation: u64,
    sleeping: bool,
}

impl Default for SleepGate {
    fn default() -> Self {
        Self {
            generation: 0,
            sleeping: false,
        }
    }
}

/// Reference counting shared by the real service set and the tests.
struct PowerState<T> {
    count: usize,
    epoch: u64,
    services: Option<T>,
}

impl<T> Default for PowerState<T> {
    fn default() -> Self {
        Self {
            count: 0,
            epoch: 0,
            services: None,
        }
    }
}

impl<T> PowerState<T> {
    /// Returns the new epoch only on the 0 -> 1 transition.
    fn acquire(&mut self) -> Option<u64> {
        self.count = self.count.saturating_add(1);
        if self.count != 1 {
            return None;
        }
        self.epoch = self.epoch.wrapping_add(1);
        Some(self.epoch)
    }

    /// Returns the services when this epoch is stale or nobody is left.
    fn install(&mut self, epoch: u64, services: T) -> Option<T> {
        if self.count == 0 || self.epoch != epoch || self.services.is_some() {
            return Some(services);
        }
        self.services = Some(services);
        None
    }

    /// Returns the services only on the 1 -> 0 transition.
    fn release(&mut self) -> Option<T> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        if self.count == 0 {
            self.services.take()
        } else {
            None
        }
    }
}

/// Everything the first reference acquires and the last reference releases.
struct Services {
    logind: Option<OwnedFd>,
    gnome: Option<super::desktop_power::IdleInhibitor>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

impl Services {
    fn empty() -> Self {
        Self {
            logind: None,
            gnome: None,
            listener: None,
        }
    }

    /// Best-effort acquisition: a missing system bus or a refusing logind
    /// leaves the guard usable rather than failing the peer.
    async fn acquire(generation: u64) -> Self {
        let mut logind = None;
        let mut listener = None;
        match tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::system()).await {
            Ok(Ok(connection)) => {
                // Some distributions only accept `sleep`, so try the combined
                // list first and fall back. `block` still lets the user
                // override it.
                for what in ["sleep:idle", "sleep"] {
                    match inhibit(&connection, what).await {
                        Ok(fd) => {
                            logind = Some(fd);
                            tracing::info!("logind sleep inhibitor acquired ({what})");
                            break;
                        }
                        Err(error) => {
                            tracing::warn!("logind sleep inhibition ({what}) failed: {error}")
                        }
                    }
                }
                listener = Some(spawn_prepare_for_sleep(generation));
            }
            Ok(Err(error)) => {
                tracing::warn!("system bus unavailable; sleep inhibition disabled: {error}")
            }
            Err(_) => {
                tracing::warn!("system bus unavailable; sleep inhibition disabled: timed out")
            }
        }

        // The GNOME idle inhibitor uses the session bus, so it is independent
        // of whether a system bus exists.
        let gnome = match super::desktop_power::IdleInhibitor::acquire().await {
            Ok(inhibitor) => Some(inhibitor),
            Err(error) => {
                tracing::warn!("desktop idle inhibition unavailable: {error}");
                None
            }
        };
        Self {
            logind,
            gnome,
            listener,
        }
    }

    fn stop(self) {
        if let Some(listener) = self.listener {
            listener.abort();
        }
        if self.logind.is_some() {
            tracing::info!("logind sleep inhibitor released");
        }
        // Closing the fd revokes the logind inhibitor; the GNOME guard has its
        // own Drop-based release path.
        drop(self.logind);
        drop(self.gnome);
    }
}

async fn inhibit(connection: &zbus::Connection, what: &str) -> Result<OwnedFd, String> {
    let arguments = (what, "openuur", "Remote desktop session is active", "block");
    let call = connection.call_method(
        Some(LOGIND_DEST),
        LOGIND_PATH,
        Some(LOGIND_IFACE),
        "Inhibit",
        &arguments,
    );
    let reply = tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| "sleep inhibition timed out".to_owned())?
        .map_err(|error| format!("sleep inhibition failed: {error}"))?;
    reply
        .body()
        .deserialize()
        .map_err(|error| format!("sleep inhibition reply: {error}"))
}

fn prepare_for_sleep_rule() -> Result<zbus::MatchRule<'static>, String> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(LOGIND_DEST)
        .map_err(|error| format!("freeze rule sender: {error}"))?
        .interface(LOGIND_IFACE)
        .map_err(|error| format!("freeze rule interface: {error}"))?
        .member("PrepareForSleep")
        .map_err(|error| format!("freeze rule member: {error}"))?
        .path(LOGIND_PATH)
        .map_err(|error| format!("freeze rule path: {error}"))?
        .build())
}

/// Keep a PrepareForSleep subscription alive for this generation. A bus reset
/// ends the stream but not the inhibitor fd, so the task reconnects instead of
/// silently losing suspend notifications.
fn spawn_prepare_for_sleep(generation: u64) -> tokio::task::JoinHandle<()> {
    use futures_util::StreamExt;
    tokio::spawn(async move {
        let mut first_subscription = true;
        while generation_current(generation) {
            let rule = match prepare_for_sleep_rule() {
                Ok(rule) => rule,
                Err(error) => {
                    tracing::warn!("suspend notification unavailable: {error}");
                    tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                    continue;
                }
            };
            let connection =
                match tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::system()).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        tracing::debug!("suspend notification unavailable: {error}");
                        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                        continue;
                    }
                    Err(_) => {
                        tracing::debug!("suspend notification unavailable: timed out");
                        tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                        continue;
                    }
                };
            let subscribed = tokio::time::timeout(
                CALL_TIMEOUT,
                zbus::MessageStream::for_match_rule(rule, &connection, Some(8)),
            )
            .await;
            let mut stream = match subscribed {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    tracing::warn!("suspend notification unavailable: {error}");
                    tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                    continue;
                }
                Err(_) => {
                    tracing::warn!("suspend notification unavailable: timed out");
                    tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                    continue;
                }
            };
            // A resubscription only runs while userspace is executing, so any
            // suspend that happened during the gap has already ended. Clear
            // the stale pause instead of waiting for a signal that was
            // emitted while nobody was listening. The first subscription is
            // left alone: it may legitimately overlap the pre-suspend phase.
            if !first_subscription {
                publish_from(generation, false);
            }
            first_subscription = false;
            while let Some(message) = stream.next().await {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::warn!("PrepareForSleep stream failed: {error}");
                        break;
                    }
                };
                match message.body().deserialize::<bool>() {
                    Ok(sleeping) => {
                        publish_from(generation, sleeping);
                        if sleeping {
                            tracing::info!(
                                "system suspend requested; pausing desktop capture and input"
                            );
                        } else {
                            tracing::info!("system resumed; waking display");
                            // Never block the signal stream on compositor or
                            // X11 calls; capture supervisors resume from the
                            // watch.
                            tokio::spawn(async {
                                let _ = super::desktop_power::wake().await;
                            });
                        }
                    }
                    Err(error) => tracing::warn!("PrepareForSleep payload rejected: {error}"),
                }
            }
            // The connection is dropped here, after the stream that borrows it.
            tokio::time::sleep(RESUBSCRIBE_DELAY).await;
        }
        tracing::debug!("logind PrepareForSleep listener stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PowerState<u8> {
        PowerState::default()
    }

    #[test]
    fn refcount_releases_only_on_the_last_guard() {
        let mut power = state();
        let epoch = power.acquire().expect("first acquisition starts setup");
        assert_eq!(power.acquire(), None);
        assert_eq!(power.install(epoch, 0), None);
        assert_eq!(power.release(), None);
        assert_eq!(power.release(), Some(0));
        assert_eq!(power.release(), None, "extra release must not underflow");
        assert_eq!(power.count, 0);
        assert_eq!(epoch, 1);
    }

    #[test]
    fn setup_after_the_last_release_is_discarded() {
        let mut power = state();
        let epoch = power.acquire().unwrap();
        assert_eq!(power.release(), None);
        // The background handshake finished after the guard went away.
        assert_eq!(power.install(epoch, 7), Some(7));
        assert!(power.services.is_none());
    }

    #[test]
    fn a_stale_epoch_is_discarded() {
        let mut power = state();
        let stale = power.acquire().unwrap();
        power.release();
        let current = power.acquire().unwrap();
        assert_eq!(power.install(stale, 1), Some(1));
        assert_eq!(power.install(current, 2), None);
        assert_eq!(power.services, Some(2));
    }

    #[test]
    fn prevent_sleep_flag_is_opt_out() {
        assert!(prevent_sleep_enabled_value(None));
        assert!(prevent_sleep_enabled_value(Some("1")));
        assert!(!prevent_sleep_enabled_value(Some("0")));
    }

    #[test]
    fn prepare_for_sleep_rule_targets_logind() {
        let rule = prepare_for_sleep_rule().unwrap();
        let debug = format!("{rule:?}");
        assert!(debug.contains("org.freedesktop.login1.Manager"), "{debug}");
        assert!(debug.contains("PrepareForSleep"), "{debug}");
        assert!(debug.contains("/org/freedesktop/login1"), "{debug}");
    }

    #[test]
    fn a_stale_listener_cannot_republish_after_release() {
        let generation = begin_generation();
        publish_from(generation, true);
        assert!(RemoteSessionPower::sleeping());
        reset_gate();
        // The listener was mid-flight when the last guard dropped.
        publish_from(generation, true);
        assert!(
            !RemoteSessionPower::sleeping(),
            "a stale listener republished suspend after release"
        );
        assert!(!*sleep_sender().borrow());
    }

    #[test]
    #[ignore = "requires a running systemd-logind on the system bus"]
    fn systemd_inhibit_list_contains_openuur() {
        fn count() -> usize {
            std::process::Command::new("systemd-inhibit")
                .arg("--list")
                .output()
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter(|line| line.contains("openuur"))
                        .count()
                })
                .unwrap_or(0)
        }

        let previous = std::env::var_os("UU_PREVENT_SLEEP");
        // The ignored test must exercise the real path even in a shell that
        // disables it for a production host.
        std::env::remove_var("UU_PREVENT_SLEEP");
        let before = count();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let guard = runtime.block_on(async {
            let guard = RemoteSessionPower::acquire();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && count() <= before {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            guard
        });
        let raised = count();
        drop(guard);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && count() > before {
            std::thread::sleep(Duration::from_millis(100));
        }
        let after = count();
        match previous {
            Some(value) => std::env::set_var("UU_PREVENT_SLEEP", value),
            None => std::env::remove_var("UU_PREVENT_SLEEP"),
        }
        assert!(raised > before, "no logind inhibitor appeared");
        assert_eq!(after, before, "the inhibitor was not released");
    }
}
