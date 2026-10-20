//! Scoped audio routing for "静音运行".
//!
//! While the host is controlled, a null sink becomes the default output and
//! existing streams are moved to it: the physical speakers go silent, while
//! the controller still receives the null sink's monitor. Restoring returns
//! the previous default sink (and its mute state). A crash-recovery record is
//! written before the route is installed, so a leftover null sink is cleaned
//! on the next start.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

const SINK: &str = "openuur-remote-sink";
const MONITOR: &str = "openuur-remote-sink.monitor";

#[derive(serde::Serialize, serde::Deserialize)]
struct Route {
    previous: String,
    module: Option<u32>,
    #[serde(default)]
    pipewire: bool,
}

struct State {
    route: Option<Route>,
    _lock: std::fs::File,
}

static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();

fn state_path() -> Option<PathBuf> {
    crate::config::Config::path()
        .ok()
        .map(|path| path.with_file_name("virtual_audio.json"))
}

fn run(program: &str, args: &[&str]) -> Result<String, ()> {
    let output = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| ())?;
    if !output.status.success() {
        return Err(());
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|_| ())
}

fn pactl(args: &[&str]) -> Result<String, ()> {
    run("pactl", args)
}

fn pipewire_only() -> bool {
    pactl(&["info"]).is_err() && run("wpctl", &["status"]).is_ok()
}

fn default_sink(pipewire: bool) -> Result<String, ()> {
    if !pipewire {
        return pactl(&["get-default-sink"]);
    }
    run("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"])?
        .lines()
        .find_map(|line| {
            line.trim()
                .trim_start_matches("* ")
                .strip_prefix("node.name = ")
                .map(|value| value.trim_matches('"').to_owned())
        })
        .ok_or(())
}

fn set_default(pipewire: bool, name: &str) -> Result<(), ()> {
    if !pipewire {
        return pactl(&["set-default-sink", name]).map(|_| ());
    }
    let id = pw_node(name, false).ok_or(())?;
    run("wpctl", &["set-default", &id.to_string()]).map(|_| ())
}

fn sink_id(name: &str) -> Option<String> {
    pactl(&["list", "short", "sinks"])
        .ok()?
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?;
            (fields.next()? == name).then(|| id.to_owned())
        })
}

fn move_inputs(from: &str, to: &str) {
    let Some(id) = sink_id(from) else { return };
    let Ok(list) = pactl(&["list", "short", "sink-inputs"]) else {
        return;
    };
    for line in list.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(input), Some(sink)) = (fields.next(), fields.next()) {
            if sink == id {
                let _ = pactl(&["move-sink-input", input, to]);
            }
        }
    }
}

fn pw_nodes(dump: &str, name: &str, null_sink: bool) -> Vec<u32> {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_str(dump) else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|object| {
            let props = object.get("info")?.get("props")?;
            if props.get("node.name")?.as_str()? != name {
                return None;
            }
            if null_sink
                && props.get("factory.name").and_then(|value| value.as_str())
                    != Some("support.null-audio-sink")
            {
                return None;
            }
            u32::try_from(object.get("id")?.as_u64()?).ok()
        })
        .collect()
}

fn pw_node(name: &str, null_sink: bool) -> Option<u32> {
    pw_nodes(&run("pw-dump", &["Node"]).ok()?, name, null_sink)
        .first()
        .copied()
}

fn owned_modules(list: &str) -> Vec<u32> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.parse().ok()?;
            if fields.next()? != "module-null-sink" {
                return None;
            }
            fields
                .next()?
                .split_whitespace()
                .any(|argument| argument == "sink_name=openuur-remote-sink")
                .then_some(id)
        })
        .collect()
}

fn persist(route: &Route) -> Result<(), ()> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(path) = state_path() else {
        return Err(());
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = path.with_extension("json.new");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|_| ())?;
    file.write_all(&serde_json::to_vec(route).map_err(|_| ())?)
        .map_err(|_| ())?;
    file.sync_all().map_err(|_| ())?;
    std::fs::rename(temporary, path).map_err(|_| ())
}

fn recover(route: &Route) -> Result<(), ()> {
    let pipewire = route.pipewire;
    // Do not overwrite a default the user changed during remote control.
    if default_sink(pipewire).is_ok_and(|sink| sink == SINK)
        && set_default(pipewire, &route.previous).is_err()
    {
        tracing::warn!("previous default audio output unavailable; using server default");
    }
    if pipewire {
        if let Ok(dump) = run("pw-dump", &["Node"]) {
            for id in pw_nodes(&dump, SINK, true) {
                let _ = run("pw-cli", &["destroy", &id.to_string()]);
            }
        }
    } else {
        move_inputs(SINK, &route.previous);
        if let Ok(modules) = pactl(&["list", "short", "modules"]) {
            for id in owned_modules(&modules) {
                let _ = pactl(&["unload-module", &id.to_string()]);
            }
        }
    }
    if let Some(path) = state_path() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn state() -> MutexGuard<'static, Option<State>> {
    STATE
        .get_or_init(|| {
            let init = (|| {
                use std::os::unix::fs::OpenOptionsExt;
                let path = state_path().ok_or(())?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|_| ())?;
                }
                let lock = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(path.with_extension("lock"))
                    .map_err(|_| ())?;
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    return Err(());
                }
                let route = if path.exists() {
                    let old: Route = serde_json::from_slice(&std::fs::read(&path).map_err(|_| ())?)
                        .map_err(|_| ())?;
                    if recover(&old).is_err() {
                        Some(old)
                    } else {
                        None
                    }
                } else {
                    None
                };
                Ok(State { route, _lock: lock })
            })();
            Mutex::new(init.ok())
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Recover a route left behind by a previous crash. Safe at startup even when
/// mute is disabled.
pub fn init() {
    drop(state());
}

/// Install the null sink and make it the default output.
pub fn enable() -> bool {
    let mut guard = state();
    let Some(state) = guard.as_mut() else {
        return false;
    };
    if state.route.is_some() {
        return pactl(&["get-default-sink"]).is_ok_and(|sink| sink == SINK)
            || pipewire_only() && default_sink(true).is_ok_and(|sink| sink == SINK);
    }
    let result = (|| {
        let pipewire = pipewire_only();
        let previous = default_sink(pipewire)?;
        if previous == SINK {
            return Err(());
        }
        if pw_node(SINK, false).is_some() {
            return Err(());
        }
        let mut route = Route {
            previous: previous.clone(),
            module: None,
            pipewire,
        };
        persist(&route)?;
        let id = if pipewire {
            run(
                "pw-cli",
                &[
                    "create-node",
                    "adapter",
                    "{ factory.name=support.null-audio-sink node.name=openuur-remote-sink \
                     node.description=OpenUUR-Remote media.class=Audio/Sink object.linger=true \
                     audio.position=[FL FR] }",
                ],
            )?;
            let mut node = None;
            for _ in 0..20 {
                node = pw_node(SINK, true);
                if node.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            node.ok_or(())?
        } else {
            pactl(&[
                "load-module",
                "module-null-sink",
                "sink_name=openuur-remote-sink",
                "sink_properties=device.description=OpenUUR-Remote",
            ])?
            .parse()
            .map_err(|_| ())?
        };
        route.module = Some(id);
        persist(&route)?;
        state.route = Some(route);
        set_default(pipewire, SINK)?;
        if !pipewire {
            move_inputs(&previous, SINK);
        }
        Ok(())
    })();
    if result.is_err() {
        if let Some(route) = state.route.as_ref() {
            if recover(route).is_ok() {
                state.route = None;
            }
        }
        tracing::warn!("virtual audio routing unavailable; using the muted-monitor path");
    }
    result.is_ok()
}

/// Restore the previous default sink and remove the null sink.
pub fn disable() {
    let mut guard = state();
    let Some(state) = guard.as_mut() else { return };
    if let Some(route) = state.route.as_ref() {
        if recover(route).is_ok() {
            state.route = None;
        } else {
            tracing::warn!("virtual audio restore incomplete; recovery record retained");
        }
    }
}

pub fn active() -> bool {
    state().as_ref().is_some_and(|state| state.route.is_some())
}

/// PulseAudio source name used for desktop audio capture.
pub fn source() -> String {
    if active() {
        MONITOR.to_owned()
    } else {
        std::env::var("UU_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".to_owned())
    }
}

/// Restores routing on ordinary host shutdown. SIGKILL is recovered at the
/// next startup.
pub struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        disable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_only_claims_the_exact_null_sink() {
        let modules = "1\tmodule-null-sink\tsink_name=openuur-remote-sink\t0\n\
                       2\tmodule-null-sink\tsink_name=openuur-remote-sink-other\t0\n\
                       3\tmodule-other\tsink_name=openuur-remote-sink\t0\n";
        assert_eq!(owned_modules(modules), vec![1]);
    }

    #[test]
    fn pipewire_recovery_only_claims_owned_null_sinks() {
        let dump = r#"[
            {"id":5,"info":{"props":{"node.name":"openuur-remote-sink","factory.name":"support.null-audio-sink"}}},
            {"id":6,"info":{"props":{"node.name":"openuur-remote-sink","factory.name":"api.alsa.pcm.sink"}}},
            {"id":7,"info":{"props":{"node.name":"other","factory.name":"support.null-audio-sink"}}},
            {"id":8}
        ]"#;
        assert_eq!(pw_nodes(dump, SINK, true), vec![5]);
        assert_eq!(pw_nodes(dump, SINK, false), vec![5, 6]);
    }
}
