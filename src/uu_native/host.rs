//! Native UU host: keeps the device online and answers official controller
//! terminal sessions over WebRTC.

use super::initialization::{self, Capabilities};
use super::network_diagnostics;
use super::rtc;
use super::signal::{Event, Outbox, Signal};
use super::system_actions::PeerActions;
use super::system_metrics::Permission;
use super::term::{self, HostTerminal};
use super::wire;
use crate::config::Config;
use crate::desktop::session_env::{self, SessionIdentity};
use crate::desktop::{self, DesktopOptions, DesktopSession, InputSink};
use crate::error::{Error, Result};
use crate::terminal::session::SessionManager;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::sctp_transport::sctp_transport_capabilities::SCTPTransportCapabilities;
use webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState;

/// The signaling room lease is refreshed after this much idle time. Active
/// WebRTC peers keep the current room: reconnecting signaling under them would
/// churn ICE candidates for no reason.
pub const IDLE_ROOM_RENEWAL: Duration = Duration::from_secs(900);

#[derive(Clone)]
struct Route {
    client: String,
    ice: String,
    app: String,
}

/// A capture plus the login session it belongs to.
struct StartedDesktop {
    desktop: Arc<DesktopSession>,
    session: session_env::SessionEnv,
    source: desktop::SourceKind,
}

/// The portal session stays open after its viewer disconnects. Reusing it is
/// the only reliable way to avoid a fresh screen-share consent prompt on
/// every connection: not every compositor returns a restore token.
struct CachedDesktop {
    desktop: Arc<DesktopSession>,
    session: session_env::SessionEnv,
    source: desktop::SourceKind,
}

impl CachedDesktop {
    fn reusable(&self, source: desktop::SourceKind, current: Option<&SessionIdentity>) -> bool {
        desktop_cache_matches(self.source, &self.session.identity(), source, current)
    }
}

fn desktop_cache_matches(
    cached_source: desktop::SourceKind,
    cached_session: &SessionIdentity,
    source: desktop::SourceKind,
    current: Option<&SessionIdentity>,
) -> bool {
    cached_source == source && current == Some(cached_session)
}

enum PendingDesktop {
    Starting(tokio::sync::oneshot::Receiver<std::result::Result<StartedDesktop, String>>),
    Ready(StartedDesktop),
}

struct Pending {
    capture_type: u64,
    ice: Option<String>,
    client: String,
    app: String,
    header: Value,
    created_at: std::time::Instant,
    controller_mac: bool,
    desktop: Option<PendingDesktop>,
}

/// Process-lifetime host state. Signaling rooms reconnect underneath it, but
/// WebRTC peers and terminal sessions survive.
struct HostRuntime {
    manager: SessionManager,
    peers: HashMap<String, HostPeer>,
    pending: Vec<Pending>,
    outbox: Outbox,
    /// Portal capture retained across peer connections, until the graphical
    /// session changes.
    desktop_cache: Option<CachedDesktop>,
}

enum RoomExit {
    Reconnect {
        error: String,
        connected: bool,
        /// The room API rejected the stored login token (HTTP 422 / code 1120).
        token_expired: bool,
    },
    Shutdown,
}

struct HostPeer {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    route: Route,
    desktop: Option<Arc<DesktopSession>>,
    /// Active BUNDLE mid/m-line negotiated by this answer. Outgoing ICE
    /// candidates use it because webrtc-rs leaves `sdpMid` empty.
    bundle_media: Arc<Mutex<Option<(String, u16)>>>,
    /// Set exactly for desktop peers; used to close captures that belong to a
    /// session which no longer exists.
    session: Option<SessionIdentity>,
    actions: Arc<PeerActions>,
    stopped: AtomicBool,
    /// One reference on the process-wide sleep inhibitor. Released by normal
    /// struct drop on every removal path; it tracks peer lifetime rather than
    /// WebRTC connection state, so `finish` leaves it alone.
    _power: crate::desktop::session_power::RemoteSessionPowerGuard,
}

impl HostPeer {
    async fn answer(&self, offer: &str) -> Result<String> {
        rtc::set_offer(&self.pc, offer)
            .await
            .map_err(Error::Transport)?;
        let answer = rtc::local_answer(&self.pc, &self.bundle_media)
            .await
            .map_err(Error::Transport)?;
        Ok(answer)
    }

    async fn accept_answer(&self, answer: &str) -> Result<()> {
        rtc::set_answer(&self.pc, answer)
            .await
            .map_err(Error::Transport)
    }

    async fn add_candidate(&self, candidate: &Value) -> Result<()> {
        rtc::add_candidate(&self.pc, candidate)
            .await
            .map_err(Error::Transport)
    }

    async fn close(&self) {
        self.finish();
        let _ = self.pc.close().await;
    }

    /// Release capture/input and session-scoped host settings exactly once.
    fn finish(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(desktop) = &self.desktop {
            if desktop.backend != desktop::Backend::Wayland {
                desktop.stop();
            }
            // Portal capture is cached across connections. Its input lease
            // releases held keys only when this peer is still the owner, so a
            // closing old peer cannot clear the replacement's input.
            self.actions.session_ended();
        }
    }
}

pub async fn run(mut config: Config, password: Option<String>) -> Result<()> {
    // Detect HEVC once per process, in the background: `ffmpeg -encoders`
    // lists encoders whose driver cannot initialize, so only a real frame is
    // trustworthy. Negotiation reads the cached result.
    tokio::spawn(async {
        crate::desktop::encoder::probe_hevc_available().await;
    });
    crate::desktop::virtual_audio::init();
    let _virtual_audio = crate::desktop::virtual_audio::Cleanup;
    // Discover the login session even before the first peer so a system
    // service can serve desktop offers as soon as the user logs in.
    session_env::ensure_monitor();
    let mut runtime = HostRuntime {
        manager: SessionManager::new(password),
        peers: HashMap::new(),
        pending: Vec::new(),
        outbox: Outbox::new(),
        desktop_cache: None,
    };
    let mut backoff = Duration::from_secs(3);
    let mut expired_logged = false;
    // Registered once for the whole process: during reconnect backoff and
    // capture startup, SIGTERM must still stop the daemon cleanly.
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    'run: loop {
        let exit = tokio::select! {
            exit = enter_room(&config, &mut runtime) => exit,
            _ = wait_for_shutdown(&mut terminate) => RoomExit::Shutdown,
            _ = wait_for_shutdown(&mut interrupt) => RoomExit::Shutdown,
        };
        match exit {
            RoomExit::Shutdown => break,
            RoomExit::Reconnect {
                error,
                connected,
                token_expired,
            } => {
                if !(token_expired && expired_logged) {
                    // An unchanged expired token would otherwise warn every
                    // 30 s; the dedicated message below already explains it.
                    tracing::warn!("native UU host: {error}");
                }
                if connected {
                    backoff = Duration::from_secs(3);
                    expired_logged = false;
                }
                if token_expired {
                    // The room API rejected the stored token. Re-read the
                    // config so a login performed while the service was
                    // running is picked up without a restart; otherwise wait
                    // for one and say so once.
                    match config.reload_token() {
                        Some(token) if config.token.as_deref() != Some(token.as_str()) => {
                            tracing::info!(
                                "native UU login token changed on disk; reconnecting with the new token"
                            );
                            config.token = Some(token);
                            backoff = Duration::from_secs(1);
                            expired_logged = false;
                        }
                        _ => {
                            if !expired_logged {
                                tracing::warn!(
                                    "native UU login token expired; run `openuur login` for this account. \
                                     The service keeps retrying and adopts the new token automatically."
                                );
                                expired_logged = true;
                            }
                            backoff = Duration::from_secs(30);
                        }
                    }
                }
                // Session changes still invalidate desktop captures while
                // signaling is down, so a dead compositor is not held pinned
                // through the whole backoff.
                let deadline = tokio::time::Instant::now() + backoff;
                let mut session_rx = session_env::subscribe();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        _ = wait_for_shutdown(&mut terminate) => break 'run,
                        _ = wait_for_shutdown(&mut interrupt) => break 'run,
                        changed = session_rx.changed() => {
                            if changed.is_err() {
                                tokio::time::sleep_until(deadline).await;
                                break;
                            }
                            let current = session_rx.borrow_and_update().available().cloned();
                            close_desktop_peers(&mut runtime, current.as_ref()).await;
                            drop_desktop_cache(&mut runtime, current.as_ref());
                            cancel_pending_desktop(&mut runtime);
                        }
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
    for peer in runtime.peers.values() {
        peer.close().await;
    }
    Ok(())
}

async fn enter_room(config: &Config, runtime: &mut HostRuntime) -> RoomExit {
    let room = match super::api::create(config).await {
        Ok(room) => room,
        Err(error) => {
            return RoomExit::Reconnect {
                token_expired: error.is_token_expired(),
                error: error.to_string(),
                connected: false,
            }
        }
    };
    let mut signal = match Signal::connect(&room.servers, &room.token, false).await {
        Ok(signal) => signal,
        Err(error) => {
            return RoomExit::Reconnect {
                error,
                connected: false,
                token_expired: false,
            }
        }
    };
    runtime.outbox.attach(signal.sender());
    signal.room_info();
    let exit = room_loop(config, runtime, &mut signal).await;
    runtime.outbox.detach();
    exit
}

async fn room_loop(config: &Config, runtime: &mut HostRuntime, signal: &mut Signal) -> RoomExit {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // A sleep (rather than an interval) lets an active session defer the
    // renewal by one minute without burning the full 900 seconds again.
    let renewal = tokio::time::sleep(IDLE_ROOM_RENEWAL);
    tokio::pin!(renewal);
    let mut session_rx = session_env::subscribe();
    // Apply a session change that happened while signaling was disconnected.
    let current = session_rx.borrow_and_update().available().cloned();
    close_desktop_peers(runtime, current.as_ref()).await;
    drop_desktop_cache(runtime, current.as_ref());

    loop {
        tokio::select! {
            message = signal.recv() => {
                let Some(message) = message else {
                    return RoomExit::Reconnect {
                        error: "signaling stream ended".to_owned(),
                        connected: true,
                        token_expired: false,
                    };
                };
                match message {
                    Event::Closed(error) => return RoomExit::Reconnect {
                        error,
                        connected: true,
                        token_expired: false,
                    },
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "bmsg_push" => {
                        let status = data.get("data").and_then(|d| d.get("status")).and_then(Value::as_str).unwrap_or("-");
                        let available = data.get("data").and_then(|d| d.get("publisher_availability_status")).and_then(Value::as_str).unwrap_or("-");
                        tracing::info!("native UU device status: {status} availability={available}");
                    }
                    Event::Text { name, data } if name == "be-controlled" => {
                        remember_connect(runtime, data, Vec::new(), config);
                    }
                    Event::Binary { name, data, attachments } if name == "be-controlled" => {
                        remember_connect(runtime, data, attachments, config);
                    }
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "soac" => {
                        handle_soac(
                            &runtime.manager,
                            &runtime.outbox,
                            &mut runtime.peers,
                            &mut runtime.pending,
                            &mut runtime.desktop_cache,
                            &data,
                            config,
                        )
                        .await;
                    }
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "released" || name == "left" || name == "leave" => {
                        let room_level = release(&mut runtime.peers, &data).await;
                        if room_level {
                            // No ice_id: this is a room-level event, not a
                            // peer release. Keep terminal/file/port-mapping
                            // sessions and renew signaling instead.
                            tracing::warn!(
                                "native UU room {name} without a peer id; renewing signaling (connected peers stay up)"
                            );
                            return RoomExit::Reconnect {
                                error: format!("room {name} without a peer id"),
                                connected: true,
                                token_expired: false,
                            };
                        }
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => maintain(runtime).await,
            _ = &mut renewal => {
                let active = runtime.peers.values().any(|peer| {
                    peer.pc.connection_state() == RTCPeerConnectionState::Connected
                });
                if active {
                    tracing::info!(
                        "signaling room renewal deferred: a remote peer is still connected"
                    );
                    renewal
                        .as_mut()
                        .reset(tokio::time::Instant::now() + Duration::from_secs(60));
                } else {
                    tracing::info!(
                        "signaling room idle for {} s; refreshing room credentials",
                        IDLE_ROOM_RENEWAL.as_secs()
                    );
                    return RoomExit::Reconnect {
                        error: "idle signaling room renewal".to_owned(),
                        connected: true,
                        token_expired: false,
                    };
                }
            }
            changed = session_rx.changed() => {
                if changed.is_ok() {
                    let current = session_rx.borrow_and_update().available().cloned();
                    match &current {
                        Some(env) => {
                            tracing::info!("desktop session changed: {}", env.describe())
                        }
                        None => {
                            tracing::info!("desktop session lost; closing desktop peers")
                        }
                    }
                    close_desktop_peers(runtime, current.as_ref()).await;
                    drop_desktop_cache(runtime, current.as_ref());
                    cancel_pending_desktop(runtime);
                }
            }
        }
    }
}

async fn wait_for_shutdown(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            let _ = signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Periodic cleanup: expired desktop offers are dropped (which stops their
/// capture task), and peers that the WebRTC stack already closed are removed.
async fn maintain(runtime: &mut HostRuntime) {
    runtime
        .pending
        .retain(|pending| pending.created_at.elapsed() < Duration::from_secs(120));
    let closed: Vec<String> = runtime
        .peers
        .iter()
        .filter(|(_, peer)| {
            matches!(
                peer.pc.connection_state(),
                RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
            )
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in closed {
        if let Some(peer) = runtime.peers.remove(&key) {
            peer.close().await;
        }
    }
}

fn remember_connect(
    runtime: &mut HostRuntime,
    data: Value,
    attachments: Vec<Vec<u8>>,
    config: &Config,
) {
    let header = data;
    let attachment = attachments.first().cloned().or_else(|| {
        header
            .get("app_data")
            .or_else(|| header.get("data").and_then(|data| data.get("app_data")))
            .and_then(Value::as_str)
            .and_then(|text| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).ok()
            })
    });
    let capture_type = attachment
        .as_deref()
        .and_then(|bytes| {
            wire::decode(bytes).or_else(|| {
                (bytes.first() == Some(&4))
                    .then(|| wire::decode(&bytes[1..]))
                    .flatten()
            })
        })
        .and_then(|fields| wire::uint(&fields, 1))
        .unwrap_or(0);
    // 1 desktop, 2 window, 5 file transfer, 7 quick launch, 8 terminal,
    // 9 port mapping.
    if !matches!(capture_type, 1 | 2 | 5 | 7 | 8 | 9) {
        return;
    }
    let platform = attachment
        .as_deref()
        .and_then(|bytes| {
            wire::decode(bytes).or_else(|| {
                (bytes.first() == Some(&4))
                    .then(|| wire::decode(&bytes[1..]))
                    .flatten()
            })
        })
        .and_then(|fields| wire::uint(&fields, 8));
    let controller_mac = crate::desktop::input_map::controller_is_mac(&header, platform);
    let desktop = if matches!(capture_type, 1 | 2) {
        let source = if capture_type == 2 {
            desktop::SourceKind::Window
        } else {
            config.display_source()
        };
        let current_identity = session_env::current().map(|env| env.identity());
        let cached = runtime
            .desktop_cache
            .as_ref()
            .filter(|cached| {
                !cached.desktop.is_stopped() && cached.reusable(source, current_identity.as_ref())
            })
            .map(|cached| {
                tracing::info!(
                    "reusing the open portal capture session for source={}",
                    source.as_str()
                );
                PendingDesktop::Ready(StartedDesktop {
                    desktop: cached.desktop.clone(),
                    session: cached.session.clone(),
                    source,
                })
            });
        match cached {
            Some(ready) => Some(ready),
            None => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let key = desktop::portal_permissions::key(source.as_str());
                let options = DesktopOptions {
                    restore_token: desktop::portal_permissions::restore_token(&key),
                    source,
                    clipboard: true,
                };
                let choice = config.desktop_backend();
                tokio::spawn(async move {
                    let result = async {
                        let session =
                            session_env::wait_for_session(session_env::SESSION_WAIT).await?;
                        // The monitor is the sole writer of the projected
                        // environment; re-applying here could revert a newer
                        // session it just applied.
                        let selection = desktop::effective_selection(choice, &session);
                        // Auto keeps non-GNOME/KDE Wayland sessions working
                        // through the portal when the compositor lacks the
                        // wlr protocols; an explicit wlroots request fails.
                        let resolved = desktop::backend_for_source(
                            selection,
                            options.source,
                            choice == desktop::BackendChoice::Auto,
                        )?;
                        if resolved != selection.effective {
                            tracing::warn!(
                                "the native backend captures whole outputs; \
                                 using xdg-desktop-portal for this capture source"
                            );
                        }
                        let backend = if resolved == desktop::Backend::Wlroots {
                            match tokio::task::spawn_blocking(desktop::probe_wlroots).await {
                                Ok(Ok(())) => desktop::Backend::Wlroots,
                                Ok(Err(error)) if choice == desktop::BackendChoice::Auto => {
                                    tracing::warn!(
                                        "native wlroots capture unavailable ({error}); \
                                         using xdg-desktop-portal"
                                    );
                                    desktop::Backend::Wayland
                                }
                                Ok(Err(error)) => {
                                    return Err(format!("wlroots backend unavailable: {error}"))
                                }
                                Err(error) => {
                                    return Err(format!("wlroots capability probe failed: {error}"))
                                }
                            }
                        } else {
                            resolved
                        };
                        let desktop = desktop::start_with(backend, options).await.map(Arc::new)?;
                        // A session change while the portal was starting must
                        // not attach a stale capture to a later offer.
                        if session_env::current().map(|env| env.identity())
                            != Some(session.identity())
                        {
                            desktop.stop();
                            return Err(
                                "graphical session changed during capture startup".to_owned()
                            );
                        }
                        Ok(StartedDesktop {
                            desktop,
                            session,
                            source,
                        })
                    }
                    .await;
                    let _ = tx.send(result);
                });
                Some(PendingDesktop::Starting(rx))
            }
        }
    } else {
        None
    };
    if runtime.pending.len() >= 32 {
        runtime.pending.remove(0);
    }
    runtime.pending.push(Pending {
        capture_type,
        ice: find_str(&header, &["ice_id", "iceId"]),
        client: find_str(&header, &["client_id", "clientId"]).unwrap_or_default(),
        app: find_str(&header, &["app_control_id", "appControlId"]).unwrap_or_default(),
        header,
        created_at: std::time::Instant::now(),
        controller_mac,
        desktop,
    });
    tracing::info!("native UU capture_type={capture_type} request remembered");
}

#[allow(clippy::too_many_arguments)]
async fn create_peer(
    manager: &SessionManager,
    outbox: &Outbox,
    route: Route,
    header: &Value,
    desktop: Option<Arc<DesktopSession>>,
    session: Option<SessionIdentity>,
    capture_type: u64,
    controller_mac: bool,
    config: &Config,
) -> Result<HostPeer> {
    let configuration = rtc::ice_config(header).map_err(Error::Protocol)?;
    let pc = rtc::peer(true, configuration)
        .await
        .map_err(Error::Transport)?;
    let actions = Arc::new(PeerActions::default());
    if desktop.is_some() {
        actions.session_started();
    }
    let video_track = if desktop.is_some() {
        Some(
            super::desktop::add_track(&pc)
                .await
                .map_err(Error::Protocol)?,
        )
    } else {
        None
    };
    let settings = desktop.as_ref().map(|_| {
        let (tx, _) =
            tokio::sync::watch::channel(crate::uu_native::video_settings::VideoSettings::default());
        Arc::new(tx)
    });
    let binary: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>> =
        Arc::new(Mutex::new(None));
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let binary_writer = binary.clone();
    tokio::spawn(async move {
        while let Some(data) = term_rx.recv().await {
            let channel = binary_writer.lock().unwrap().clone();
            if let Some(channel) = channel {
                let _ = channel.send(&bytes::Bytes::from(data)).await;
            }
        }
    });
    let terminal = HostTerminal::new(manager.clone(), term_tx);
    terminal.event_loop();
    install_host_channels(
        &pc,
        manager.clone(),
        terminal,
        binary,
        desktop.clone(),
        settings.clone(),
        video_track.clone(),
        actions.clone(),
        capture_type,
        controller_mac,
        config.clone(),
    );
    network_diagnostics::start(Arc::downgrade(&pc));

    // A network switch shows up as a prolonged Disconnected state. Restart ICE
    // once per outage; the viewer answers the host offer with `soac answer`.
    {
        let pc = pc.clone();
        let outbox = outbox.clone();
        let route = route.clone();
        tokio::spawn(async move {
            let mut disconnected: Option<tokio::time::Instant> = None;
            let mut restart_attempts = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                match pc.connection_state() {
                    RTCPeerConnectionState::Connected => {
                        disconnected = None;
                        restart_attempts = 0;
                    }
                    RTCPeerConnectionState::Disconnected => {
                        let now = tokio::time::Instant::now();
                        let since = *disconnected.get_or_insert(now);
                        // A signaling outage must not consume the three restart
                        // attempts on offers nobody can deliver.
                        if !outbox.is_connected() {
                            continue;
                        }
                        if now.duration_since(since) >= Duration::from_secs(4)
                            && restart_attempts < 3
                        {
                            restart_attempts += 1;
                            match super::session_ice::restart_offer(&pc).await {
                                Ok(sdp) => {
                                    tracing::info!(
                                        "network switch: sending ICE restart offer (attempt {restart_attempts})"
                                    );
                                    outbox.soac(json!({
                                        "client_id": route.client,
                                        "data": {
                                            "type": "offer",
                                            "sdp": sdp,
                                            "ice_id": route.ice,
                                            "app_control_id": route.app,
                                        }
                                    }));
                                    disconnected = Some(now);
                                }
                                Err(error) => {
                                    tracing::warn!("ICE restart offer failed: {error}");
                                    disconnected = Some(now);
                                }
                            }
                        }
                    }
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => break,
                    _ => disconnected = None,
                }
            }
        });
    }

    let state_pc = pc.clone();
    let state_desktop = desktop.clone();
    let state_actions = actions.clone();
    pc.on_peer_connection_state_change(Box::new(move |state| {
        tracing::info!("native UU peer state: {state:?}");
        let pc = state_pc.clone();
        let desktop = state_desktop.clone();
        let actions = state_actions.clone();
        Box::pin(async move {
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                if let Some(desktop) = &desktop {
                    if desktop.backend != desktop::Backend::Wayland {
                        desktop.stop();
                    }
                    actions.session_ended();
                }
            }
            if state == RTCPeerConnectionState::Connected
                && pc.sctp().state() != RTCSctpTransportState::Connected
            {
                tracing::info!("native UU starting SCTP after DTLS");
                if let Err(error) = pc
                    .sctp()
                    .start(SCTPTransportCapabilities {
                        max_message_size: 262_144,
                    })
                    .await
                {
                    tracing::warn!("native UU SCTP start failed: {error}");
                }
            }
        })
    }));

    let outbox_for_candidates = outbox.clone();
    let signal_route = route.clone();
    let peer_bundle_media = Arc::new(Mutex::new(None));
    let candidate_bundle_media = peer_bundle_media.clone();
    pc.on_ice_candidate(Box::new(move |candidate| {
        let outbox = outbox_for_candidates.clone();
        let route = signal_route.clone();
        let bundle_media = candidate_bundle_media.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else {
                return;
            };
            let media = bundle_media
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let Some(value) = rtc::candidate_json(&candidate, media.as_ref()) else {
                return;
            };
            outbox.soac(json!({
                "client_id": route.client,
                "data": {
                    "type": "candidate",
                    "candidate": value,
                    "ice_id": route.ice,
                    "app_control_id": route.app,
                }
            }));
        })
    }));

    if let (Some(desktop), Some(track), Some(settings)) =
        (desktop.clone(), video_track, settings.clone())
    {
        super::desktop::spawn(
            pc.clone(),
            track,
            desktop.clone(),
            outbox.clone(),
            route.client.clone(),
            route.ice.clone(),
            route.app.clone(),
            settings.subscribe(),
        );
        if crate::desktop::audio::enabled() {
            match super::desktop::add_audio_track(&pc).await {
                Ok(audio) => {
                    tokio::spawn(crate::desktop::audio::stream(pc.clone(), audio));
                }
                Err(error) => tracing::warn!("desktop audio unavailable: {error}"),
            }
        }
    }

    Ok(HostPeer {
        pc,
        route,
        desktop,
        bundle_media: peer_bundle_media,
        session,
        actions,
        stopped: AtomicBool::new(false),
        _power: crate::desktop::session_power::RemoteSessionPower::acquire(),
    })
}

#[allow(dead_code, clippy::too_many_arguments)]
fn install_host_channels_legacy(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    manager: SessionManager,
    terminal: Arc<HostTerminal>,
    binary: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>>,
    desktop: Option<Arc<DesktopSession>>,
    settings: Option<
        Arc<tokio::sync::watch::Sender<crate::uu_native::video_settings::VideoSettings>>,
    >,
) {
    pc.on_data_channel(Box::new(move |channel| {
        let label = channel.label().to_owned();
        let manager = manager.clone();
        let binary = binary.clone();
        let terminal = terminal.clone();
        let desktop = desktop.clone();
        let settings = settings.clone();
        Box::pin(async move {
            tracing::info!("native UU data channel opened: {label}");
            let ready_label = label.clone();
            let ready_channel = channel.clone();
            let ready_desktop = desktop.clone();
            channel.on_open(Box::new(move || {
                let ready_label = ready_label.clone();
                let ready_channel = ready_channel.clone();
                let ready_desktop = ready_desktop.clone();
                Box::pin(async move {
                    tracing::info!("native UU data channel ready: {ready_label}");
                    if ready_label == "CONTROL_DATA_CHANNEL" {
                        if let Some(desktop) = &ready_desktop {
                            let (width, height) = desktop.active_size();
                            let screen = crate::uu_native::video_settings::screen_sources_list(
                                &desktop.screens(),
                                desktop.screen(),
                                None,
                                "Linux desktop",
                            );
                            let _ = ready_channel
                                .send(&bytes::Bytes::from(crate::uu_native::wire::message(0, 7, &screen)))
                                .await;
                            let _ = ready_channel
                                .send(&bytes::Bytes::from(crate::uu_native::wire::message(0, 8, &[])))
                                .await;
                            let codecs = crate::uu_native::video_settings::codec_negotiation(
                                width, height, 60, true,
                            );
                            let _ = ready_channel
                                .send(&bytes::Bytes::from(crate::uu_native::wire::message(0, 18, &codecs)))
                                .await;
                        }
                    }
                })
            }));
            if label == "BINARY_DATA_CHANNEL" {
                *binary.lock().unwrap() = Some(channel.clone());
                let closing = terminal.clone();
                channel.on_close(Box::new(move || {
                    closing.close();
                    Box::pin(async {})
                }));
            }
            let port_mapping = if label == "FILE_DATA_CHANNEL" {
                let (file_tx, mut file_rx) = mpsc::channel::<Vec<u8>>(64);
                let channel_for_file = channel.clone();
                tokio::spawn(async move {
                    while let Some(message) = file_rx.recv().await {
                        let _ = channel_for_file.send(&bytes::Bytes::from(message)).await;
                    }
                });
                let file_sequence = Arc::new(std::sync::atomic::AtomicU64::new(1));
                Some((
                    crate::uu_native::port_mapping::PortMapping::new(
                        file_tx.clone(),
                        file_sequence.clone(),
                    ),
                    crate::uu_native::file_transfer::FileTransfer::new(
                        file_tx,
                        None,
                        Config::default().file_save_path(),
                        file_sequence,
                    ),
                ))
            } else {
                None
            };

            if label == "FILE_DATA_CHANNEL" {
                if let Some((mapping, _)) = &port_mapping {
                    let mapping = mapping.clone();
                    channel.on_close(Box::new(move || {
                        mapping.shutdown();
                        Box::pin(async {})
                    }));
                }
            }
            let clipboard = if label == "TEXT_DATA_CHANNEL" {
                desktop.as_ref().map(|_| {
                    let bridge = Arc::new(crate::desktop::clipboard::start());
                    let state = Arc::new(Mutex::new(
                        crate::uu_native::clipboard::ClipboardSession::new(),
                    ));
                    let next_id = Arc::new(std::sync::atomic::AtomicU64::new(1 << 32));
                    let mut outgoing = bridge.outgoing.clone();
                    let channel_for_outgoing = channel.clone();
                    let state_for_outgoing = state.clone();
                    let next_for_outgoing = next_id.clone();
                    tokio::spawn(async move {
                        loop {
                            if outgoing.changed().await.is_err() {
                                break;
                            }
                            let Some(text) = outgoing.borrow_and_update().clone() else {
                                continue;
                            };
                            let message = {
                                let Ok(mut state) = state_for_outgoing.lock() else {
                                    break;
                                };
                                state.offer(
                                    next_for_outgoing
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                    text,
                                )
                            };
                            match message {
                                Ok(message) => {
                                    let _ = channel_for_outgoing
                                        .send(&bytes::Bytes::from(message))
                                        .await;
                                }
                                Err(error) => tracing::debug!("clipboard offer rejected: {error}"),
                            }
                        }
                    });
                    (bridge, state, next_id)
                })
            } else {
                None
            };

            if label == "CONTROL_DATA_CHANNEL"
                || label == "TEXT_DATA_CHANNEL"
                || label == "BINARY_DATA_CHANNEL"
                || label == "FILE_DATA_CHANNEL"
                || label.contains("STREAMER")
            {
                let manager = manager.clone();
                let terminal = terminal.clone();
                let message_channel = channel.clone();
                let message_label = label.clone();
                channel.on_message(Box::new(move |message| {
                    let manager = manager.clone();
                    let terminal = terminal.clone();
                    let channel = message_channel.clone();
                    let label = message_label.clone();
                    let desktop = desktop.clone();
                    let settings = settings.clone();
                    let clipboard = clipboard.clone();
                    let port_mapping = port_mapping.clone();
                    Box::pin(async move {
                        let data = message.data.to_vec();
                        if let Some((port_mapping, file_transfer)) = &port_mapping {
                            if port_mapping.handle(&data, &label).await {
                                return;
                            }
                            if label == "FILE_DATA_CHANNEL"
                                && file_transfer.handle(&data, &label).await
                            {
                                return;
                            }
                        }
                        if label == "BINARY_DATA_CHANNEL" && term::is_frame(&data) {
                            terminal.handle(&data);
                            return;
                        }
                        if let Some(desktop) = &desktop {
                            if let Ok(value) = serde_json::from_slice::<Value>(&data) {
                                if handle_desktop_input(desktop.input(), &value) {
                                    return;
                                }
                            }
                        }
                        if let Some((_, _, rpc_fields)) = wire::rpc_request(&data) {
                            if let Some((bridge, state, next_id)) = &clipboard {
                                let result = {
                                    let Ok(mut state) = state.lock() else {
                                        return;
                                    };
                                    if let Some(text_change) = wire::blob(&rpc_fields, 10) {
                                        if let Some(text_change) = wire::decode(text_change) {
                                            if let Some(text) = wire::blob(&text_change, 2)
                                                .and_then(|value| std::str::from_utf8(value).ok())
                                            {
                                                bridge
                                                    .incoming
                                                    .send_replace(Some(text.to_owned()));
                                            }
                                        }
                                    }
                                    if wire::blob(&rpc_fields, 9).is_some() {
                                        Some(state.handle_request(&rpc_fields, || {
                                            next_id.fetch_add(
                                                1,
                                                std::sync::atomic::Ordering::Relaxed,
                                            )
                                        }))
                                    } else {
                                        None
                                    }
                                };
                                if let Some(result) = result {
                                    match result {
                                        Ok((messages, text)) => {
                                            for message in messages {
                                                let _ = channel
                                                    .send(&bytes::Bytes::from(message))
                                                    .await;
                                            }
                                            if let Some(text) = text {
                                                bridge.incoming.send_replace(Some(text));
                                            }
                                        }
                                        Err(error) => tracing::debug!(
                                            "clipboard request rejected: {error}"
                                        ),
                                    }
                                    return;
                                }
                            }
                        }
                        if let Some(fields) = wire::decode(&data) {
                            if let Some(rpc) = wire::blob(&fields, 21) {
                                if let Some(rpc) = wire::decode(rpc) {
                                    if let Some(header) = wire::blob(&rpc, 1) {
                                        let id = wire::decode(header)
                                            .and_then(|h| wire::uint(&h, 1))
                                            .unwrap_or(0);
                                        if rpc.iter().any(|f| f.number == 28) {
                                            tracing::debug!(
                                                "native UU terminal check_env request {id} answered"
                                            );
                                            let _ = channel
                                                .send(&bytes::Bytes::from(
                                                    wire::terminal_env_response(id),
                                                ))
                                                .await;
                                            return;
                                        }
                                        if let Some(body) = wire::blob(&rpc, 29) {
                                            if let Some(body) = wire::decode(body) {
                                                let session =
                                                    wire::uint(&body, 1).unwrap_or(0) as u16;
                                                let name = wire::blob(&body, 2)
                                                    .and_then(|b| std::str::from_utf8(b).ok())
                                                    .unwrap_or("");
                                                let _ = manager.rename(session, name);
                                                let _ = channel
                                                    .send(&bytes::Bytes::from(
                                                        wire::terminal_rename_response(
                                                            id,
                                                            session as u64,
                                                        ),
                                                    ))
                                                    .await;
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            if label == "CONTROL_DATA_CHANNEL" || label == "TEXT_DATA_CHANNEL" {
                                if let Some(rpc_fields) = wire::rpc_response(&data) {
                                    if let Some((bridge, state, _)) = &clipboard {
                                        let text = state
                                            .lock()
                                            .ok()
                                            .and_then(|mut state| state.handle_response(&rpc_fields).ok().flatten());
                                        if let Some(text) = text {
                                            bridge.incoming.send_replace(Some(text));
                                        }
                                    }
                                }
                            }
                            if label == "CONTROL_DATA_CHANNEL" {
                                if let Some(key) = wire::launch_app_key(&data) {
                                    if let Err(error) = crate::desktop::apps::launch(&key) {
                                        tracing::warn!("quick launch failed: {error}");
                                    }
                                    return;
                                }
                                if let Some((request_id, case, rpc)) = wire::rpc_request(&data) {
                                    match case {
                                        2 | 3 | 4 | 6 => {
                                            if let (Some(settings), Some(desktop)) =
                                                (settings.as_ref(), desktop.as_ref())
                                            {
                                                let h264 = true;
                                                let h265 = true;
                                                if let Some(response) = crate::uu_native::video_settings::apply_rpc(
                                                    &rpc,
                                                    settings,
                                                    desktop.active_size(),
                                                    h264,
                                                    h265,
                                                ) {
                                                    let _ = channel
                                                        .send(&bytes::Bytes::from(wire::message(0, 22, &response)))
                                                        .await;
                                                    let sync = crate::uu_native::video_settings::sync_payload(
                                                        *settings.borrow(),
                                                        desktop.active_size(),
                                                    );
                                                    let _ = channel
                                                        .send(&bytes::Bytes::from(wire::message(0, 25, &sync)))
                                                        .await;
                                                }
                                            }
                                        }
                                        23 => {
                                            let apps: Vec<_> = crate::desktop::apps::catalog()
                                                .into_iter()
                                                .map(|app| {
                                                    (
                                                        app.key,
                                                        app.name,
                                                        String::new(),
                                                        app.target_path,
                                                        app.app_id,
                                                    )
                                                })
                                                .collect();
                                            let _ = channel
                                                .send(&bytes::Bytes::from(
                                                    wire::quick_launch_scan_response(request_id, &apps),
                                                ))
                                                .await;
                                        }
                                        24 => {
                                            let key = wire::blob(&rpc, 24)
                                                .and_then(|body| {
                                                    let body = wire::decode(body)?;
                                                    wire::blob(&body, 1).map(<[u8]>::to_vec)
                                                })
                                                .and_then(|key| {
                                                    String::from_utf8(key).ok()
                                                })
                                                .unwrap_or_default();
                                            let icon = crate::desktop::apps::icon(&key);
                                            let _ = channel
                                                .send(&bytes::Bytes::from(
                                                    wire::quick_launch_icon_response(
                                                        request_id,
                                                        &key,
                                                        icon.as_deref(),
                                                    ),
                                                ))
                                                .await;
                                        }
                                        _ => {}
                                    }
                                }
                                if let Some((screen_id, request_seq, config_flag)) =
                                    wire::capture_config(&data)
                                {
                                    let switched = desktop
                                        .as_ref()
                                        .is_some_and(|desktop| desktop.switch_screen(screen_id as u32));
                                    if switched {
                                        tracing::info!("desktop switched to screen {screen_id}");
                                    } else {
                                        tracing::warn!("unknown desktop screen {screen_id}");
                                    }
                                    let mut body = Vec::new();
                                    wire::int(1, request_seq, &mut body);
                                    wire::int(2, config_flag, &mut body);
                                    wire::int(3, if desktop.is_some() && switched { 0 } else { 1 }, &mut body);
                                    let _ = channel
                                        .send(&bytes::Bytes::from(wire::message(0, 19, &body)))
                                        .await;
                                }
                                if let Some(seq) = wire::echo_request_seq(&data) {
                                    let _ = channel
                                        .send(&bytes::Bytes::from(wire::echo_response(
                                            1_000_000 + seq,
                                            seq,
                                        )))
                                        .await;
                                }
                            }
                        }
                    })
                }));
            }
        })
    }));
}

async fn send_framed(
    channel: &Arc<webrtc::data_channel::RTCDataChannel>,
    sequence: &AtomicU64,
    which: u32,
    payload: &[u8],
) -> bool {
    let seq = sequence.fetch_add(1, Ordering::Relaxed);
    channel
        .send(&bytes::Bytes::from(wire::message(seq, which, payload)))
        .await
        .is_ok()
}

/// Reply body for the official viewer's STREAMER telemetry ping.
pub fn streamer_pong(data: &[u8]) -> Option<&'static [u8]> {
    let value: Value = serde_json::from_slice(data).ok()?;
    (value.get("type").and_then(Value::as_str) == Some("ping")).then_some(br#"{"type":"pong"}"#)
}

#[allow(clippy::too_many_arguments)]
fn install_host_channels(
    pc: &Arc<webrtc::peer_connection::RTCPeerConnection>,
    manager: SessionManager,
    terminal: Arc<HostTerminal>,
    binary: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>>,
    desktop: Option<Arc<DesktopSession>>,
    settings: Option<
        Arc<tokio::sync::watch::Sender<crate::uu_native::video_settings::VideoSettings>>,
    >,
    video_track: Option<Arc<crate::desktop::video_track::VideoTrack>>,
    actions: Arc<PeerActions>,
    capture_type: u64,
    controller_mac: bool,
    config: Config,
) {
    let sequence = Arc::new(AtomicU64::new(1));
    // Every desktop peer maps controller shortcuts in front of the backend.
    // The lease makes the newest desktop peer the only one that can inject:
    // a delayed event from a replaced peer is dropped, including its releases.
    let mapped_input: Option<Arc<dyn crate::desktop::InputSink>> =
        desktop.as_ref().map(|desktop| {
            let lease = crate::desktop::lease::InputLease::claim(desktop.input());
            {
                let lease = lease.clone();
                let pc = pc.clone();
                tokio::spawn(async move {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                    loop {
                        match pc.connection_state() {
                            RTCPeerConnectionState::Connected => lease.activate(),
                            RTCPeerConnectionState::Disconnected
                            | RTCPeerConnectionState::Closed
                            | RTCPeerConnectionState::Failed => lease.deactivate(),
                            _ => {}
                        }
                        if matches!(
                            pc.connection_state(),
                            RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
                        ) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                });
            }
            let input: Arc<dyn crate::desktop::InputSink> =
                Arc::new(crate::desktop::lease::LeasedInput::new(lease));
            Arc::new(
                crate::desktop::input_map::MappedInput::new(
                    input,
                    controller_mac,
                    desktop.backend == crate::desktop::Backend::X11,
                )
                .with_lock(crate::desktop::session_lock::watch_locked()),
            ) as Arc<dyn crate::desktop::InputSink>
        });
    let clipboard_ready = Arc::new(AtomicBool::new(false));
    let sound = crate::desktop::audio::enabled();
    let save_path = config.file_save_path();
    let save_location = save_path.to_string_lossy().into_owned();
    // One file-transfer session per peer. Viewers send FileTransferFtp RPCs on
    // CONTROL (the official host accepts TEXT and FILE too), while replies and
    // host-initiated requests travel on FILE_DATA_CHANNEL. Create the session
    // up front so those requests are not dropped when they arrive on CONTROL.
    let (file_tx, mut file_rx) = mpsc::channel::<Vec<u8>>(64);
    // The viewer may close and reopen FILE_DATA_CHANNEL between transfers.
    // Forward through whichever channel is current instead of binding the
    // queue to the first one, and never tear down the per-peer session on a
    // channel close.
    let file_channel: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>> =
        Arc::new(Mutex::new(None));
    {
        let file_channel = file_channel.clone();
        tokio::spawn(async move {
            while let Some(message) = file_rx.recv().await {
                let bytes = message.len();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let target = loop {
                    if let Some(channel) = file_channel.lock().unwrap().clone() {
                        break Some(channel);
                    }
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                };
                match target {
                    Some(channel) => {
                        if channel.send(&bytes::Bytes::from(message)).await.is_err() {
                            tracing::debug!("FILE channel send failed for {bytes} bytes");
                        } else {
                            tracing::debug!("FILE channel sent {bytes} bytes");
                        }
                    }
                    None => tracing::debug!("FILE channel missing; dropped {bytes} bytes"),
                }
            }
        });
    }
    let control_channel: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>> =
        Arc::new(Mutex::new(None));
    let text_channel: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>> =
        Arc::new(Mutex::new(None));
    // Official viewers do not always read RPC replies from FILE_DATA_CHANNEL;
    // mirror each reply onto the channel its request arrived on.
    let (mirror_tx, mut mirror_rx) = mpsc::channel::<(String, Vec<u8>)>(64);
    {
        let control_channel = control_channel.clone();
        let text_channel = text_channel.clone();
        tokio::spawn(async move {
            while let Some((label, message)) = mirror_rx.recv().await {
                let slot = match label.as_str() {
                    "CONTROL_DATA_CHANNEL" => control_channel.clone(),
                    "TEXT_DATA_CHANNEL" => text_channel.clone(),
                    _ => continue,
                };
                let bytes = message.len();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let target = loop {
                    let target = slot.lock().unwrap().clone();
                    if target.is_some() {
                        break target;
                    }
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                };
                match target {
                    Some(channel) => {
                        if channel.send(&bytes::Bytes::from(message)).await.is_err() {
                            tracing::debug!("{label} file mirror send failed for {bytes} bytes");
                        } else {
                            tracing::debug!("{label} file mirror sent {bytes} bytes");
                        }
                    }
                    None => tracing::debug!("{label} file mirror missing; dropped {bytes} bytes"),
                }
            }
        });
    }
    let file_transfer = Arc::new(crate::uu_native::file_transfer::FileTransfer::new(
        file_tx.clone(),
        Some(mirror_tx),
        save_path,
        sequence.clone(),
    ));
    let port_mapping =
        crate::uu_native::port_mapping::PortMapping::new(file_tx, Arc::new(AtomicU64::new(1)));
    {
        // Like the reference host: port-mapping streams live for the peer, not
        // for one FILE_DATA_CHANNEL incarnation.
        let pc = pc.clone();
        let mapping = port_mapping.clone();
        tokio::spawn(async move {
            use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
            while !matches!(pc.connection_state(), State::Closed | State::Failed) {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            mapping.shutdown();
        });
    }
    let capture_change_type = if capture_type == 2 { 1u32 } else { 0u32 };
    let desktop_requested = matches!(capture_type, 1 | 2);
    let advertised_fps = crate::desktop::encoder::display_fps();
    let screen_type = if config.display_source() == desktop::SourceKind::Virtual {
        1u32
    } else {
        0u32
    };

    pc.on_data_channel(Box::new(move |channel| {
        let label = channel.label().to_owned();
        let manager = manager.clone();
        let binary = binary.clone();
        let terminal = terminal.clone();
        let desktop = desktop.clone();
        let settings = settings.clone();
        let video_track = video_track.clone();
        let actions = actions.clone();
        let sequence = sequence.clone();
        let clipboard_ready = clipboard_ready.clone();
        let save_location = save_location.clone();
        let file_transfer = file_transfer.clone();
        let port_mapping = port_mapping.clone();
        let file_channel = file_channel.clone();
        let control_channel = control_channel.clone();
        let text_channel = text_channel.clone();
        let mapped_input = mapped_input.clone();
        Box::pin(async move {
            tracing::info!("native UU data channel opened: {label}");

            if label == "BINARY_DATA_CHANNEL" {
                *binary.lock().unwrap() = Some(channel.clone());
                let closing = terminal.clone();
                channel.on_close(Box::new(move || {
                    closing.close();
                    Box::pin(async {})
                }));
            }

            if label == "CONTROL_DATA_CHANNEL" {
                *control_channel.lock().unwrap() = Some(channel.clone());
                let slot = control_channel.clone();
                let closing = channel.clone();
                channel.on_close(Box::new(move || {
                    let mut current = slot.lock().unwrap();
                    if current
                        .as_ref()
                        .is_some_and(|open| Arc::ptr_eq(open, &closing))
                    {
                        *current = None;
                    }
                    Box::pin(async {})
                }));
            }

            if label == "TEXT_DATA_CHANNEL" {
                *text_channel.lock().unwrap() = Some(channel.clone());
                let slot = text_channel.clone();
                let closing = channel.clone();
                channel.on_close(Box::new(move || {
                    let mut current = slot.lock().unwrap();
                    if current
                        .as_ref()
                        .is_some_and(|open| Arc::ptr_eq(open, &closing))
                    {
                        *current = None;
                    }
                    Box::pin(async {})
                }));
            }

            if label == "FILE_DATA_CHANNEL" {
                *file_channel.lock().unwrap() = Some(channel.clone());
                let slot = file_channel.clone();
                let closing = channel.clone();
                channel.on_close(Box::new(move || {
                    tracing::info!("native UU data channel closed: FILE_DATA_CHANNEL");
                    let mut current = slot.lock().unwrap();
                    if current
                        .as_ref()
                        .is_some_and(|open| Arc::ptr_eq(open, &closing))
                    {
                        *current = None;
                    }
                    Box::pin(async {})
                }));
            }

            let clipboard = if label == "TEXT_DATA_CHANNEL" {
                let bridge = desktop
                    .as_ref()
                    .and_then(|desktop| desktop.clipboard_bridge())
                    .unwrap_or_else(crate::desktop::clipboard::start);
                let crate::desktop::clipboard::ClipboardBridge {
                    incoming,
                    mut outgoing,
                    ready,
                } = bridge;
                {
                    let flag = clipboard_ready.clone();
                    let mut ready = ready;
                    tokio::spawn(async move {
                        if ready.wait_for(|value| *value).await.is_ok() {
                            flag.store(true, Ordering::Release);
                        }
                    });
                }
                let state = Arc::new(Mutex::new(
                    crate::uu_native::clipboard::ClipboardSession::new(),
                ));
                let next_id = Arc::new(AtomicU64::new(1 << 32));
                let channel_for_outgoing = channel.clone();
                let state_for_outgoing = state.clone();
                let next_for_outgoing = next_id.clone();
                tokio::spawn(async move {
                    loop {
                        if outgoing.changed().await.is_err() {
                            break;
                        }
                        let Some(text) = outgoing.borrow_and_update().clone() else {
                            continue;
                        };
                        let message = {
                            let Ok(mut state) = state_for_outgoing.lock() else {
                                break;
                            };
                            state.offer(
                                next_for_outgoing.fetch_add(1, Ordering::Relaxed),
                                text,
                            )
                        };
                        match message {
                            Ok(message) => {
                                if channel_for_outgoing
                                    .send(&bytes::Bytes::from(message))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(error) => tracing::debug!("clipboard offer rejected: {error}"),
                        }
                    }
                });
                Some((incoming, state, next_id))
            } else {
                None
            };
            let clipboard_assembly = Arc::new(Mutex::new(
                crate::uu_native::clipboard::ClipboardAssembly::default(),
            ));

            // CONTROL initialization: announce the selected target before
            // secure-desktop state (the Android viewer needs this ordering to
            // enter its auto-unlock flow).
            {
                let ready_channel = channel.clone();
                let ready_desktop = desktop.clone();
                let ready_video = video_track.clone();
                let ready_actions = actions.clone();
                let ready_sequence = sequence.clone();
                let ready_clipboard = clipboard_ready.clone();
                let ready_location = save_location.clone();
                let ready_label = label.clone();
                channel.on_open(Box::new(move || {
                    let ready_channel = ready_channel.clone();
                    let ready_desktop = ready_desktop.clone();
                    let ready_video = ready_video.clone();
                    let ready_actions = ready_actions.clone();
                    let ready_sequence = ready_sequence.clone();
                    let ready_clipboard = ready_clipboard.clone();
                    let ready_location = ready_location.clone();
                    let ready_label = ready_label.clone();
                    Box::pin(async move {
                        tracing::info!("native UU data channel ready: {ready_label}");
                        if ready_label != "CONTROL_DATA_CHANNEL" {
                            return;
                        }
                        let Some(desktop) = &ready_desktop else {
                            // A desktop offer whose portal capture failed still
                            // gets a truthful permission report instead of an
                            // unanswered offer.
                            if desktop_requested {
                                if let Some(body) = initialization::capture_change(
                                    capture_change_type,
                                    0,
                                    "capture unavailable",
                                ) {
                                    send_framed(&ready_channel, &ready_sequence, 8, &body).await;
                                }
                                let payload = ready_actions.metrics_payload(
                                    sound,
                                    Permission::Reject,
                                    Permission::Allow,
                                    Permission::Allow,
                                    &ready_location,
                                );
                                send_framed(&ready_channel, &ready_sequence, 13, &payload).await;
                        let download_path =
                            crate::uu_native::system_metrics::remote_download_path(&ready_location);
                        send_framed(&ready_channel, &ready_sequence, 26, &download_path).await;
                                let state = initialization::secure_desktop(
                                    crate::desktop::session_lock::effective_locked(
                                        crate::desktop::session_lock::locked_now(),
                                    ),
                                    false,
                                    crate::desktop::session_lock::supported(),
                                );
                                send_framed(&ready_channel, &ready_sequence, 15, &state).await;
                            }
                            return;
                        };
                        let screens = desktop.screens();
                        let active = desktop.screen();
                        if let Some(body) = initialization::screen_sources_from(
                            &screens,
                            active,
                            None,
                            "Linux desktop",
                            screen_type,
                            advertised_fps,
                        ) {
                            send_framed(&ready_channel, &ready_sequence, 7, &body).await;
                        }
                        if let Some(body) =
                            initialization::capture_change(capture_change_type, 0, "Linux desktop")
                        {
                            send_framed(&ready_channel, &ready_sequence, 8, &body).await;
                        }
                        let (width, height) = desktop.active_size();
                        let (h264, h265) = ready_video
                            .as_ref()
                            .map(|track| track.available())
                            .unwrap_or((true, false));
                        let h265 = h265
                            && crate::desktop::encoder::hevc_available();
                        let codecs =
                            crate::uu_native::video_settings::codec_negotiation(
                                width,
                                height,
                                advertised_fps,
                                h265,
                            );
                        send_framed(&ready_channel, &ready_sequence, 18, &codecs).await;
                        let _ = h264;
                        let permissions =
                            initialization::permissions(false, true, true, sound, false);
                        send_framed(&ready_channel, &ready_sequence, 15, &permissions).await;
                        let locked = crate::desktop::session_lock::effective_locked(
                            crate::desktop::session_lock::locked_now(),
                        );
                        let state = initialization::secure_desktop(
                            locked,
                            false,
                            crate::desktop::session_lock::supported(),
                        );
                        send_framed(&ready_channel, &ready_sequence, 15, &state).await;
                        let payload = ready_actions.metrics_payload(
                            sound,
                            Permission::Allow,
                            Permission::Allow,
                            Permission::Allow,
                            &ready_location,
                        );
                        send_framed(&ready_channel, &ready_sequence, 13, &payload).await;
                        let download_path =
                            crate::uu_native::system_metrics::remote_download_path(&ready_location);
                        send_framed(&ready_channel, &ready_sequence, 26, &download_path).await;

                        // Report every lock transition on this CONTROL channel.
                        let watch = crate::desktop::session_lock::watch_locked();
                        let watch_channel = ready_channel.clone();
                        let watch_sequence = ready_sequence.clone();
                        tokio::spawn(async move {
                            let mut watch = watch;
                            let mut sent: Option<bool> = None;
                            loop {
                                let locked = *watch.borrow_and_update();
                                if let Some(locked) = locked.filter(|value| sent != Some(*value)) {
                                    let body = initialization::secure_desktop(
                                        locked,
                                        false,
                                        crate::desktop::session_lock::supported(),
                                    );
                                    if !send_framed(&watch_channel, &watch_sequence, 15, &body).await {
                                        break;
                                    }
                                    sent = Some(locked);
                                }
                                tokio::select! {
                                    changed = watch.changed() => {
                                        if changed.is_err() { break; }
                                    }
                                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                                }
                                if watch_channel.ready_state()
                                    != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                                {
                                    break;
                                }
                            }
                        });
                        // Cursor shapes: publish the current shape when the
                        // channel opens, then each change at most 30 times a
                        // second. Backends without a global cursor (portal)
                        // never produce an image, so this task stays idle.
                        let mut cursor = desktop.cursor();
                        let cursor_channel = ready_channel.clone();
                        let cursor_sequence = ready_sequence.clone();
                        tokio::spawn(async move {
                            let mut sent_serial = None;
                            loop {
                                if cursor_channel.ready_state()
                                    != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                                {
                                    break;
                                }
                                let image = cursor.borrow_and_update().clone();
                                if let Some(image) = image {
                                    if sent_serial != Some(image.serial) {
                                        if let Some(body) = initialization::cursor_shape(&image) {
                                            if !send_framed(
                                                &cursor_channel,
                                                &cursor_sequence,
                                                15,
                                                &body,
                                            )
                                            .await
                                            {
                                                break;
                                            }
                                            sent_serial = Some(image.serial);
                                            tokio::time::sleep(Duration::from_millis(33)).await;
                                            continue;
                                        }
                                    }
                                }
                                tokio::select! {
                                    changed = cursor.changed() => {
                                        if changed.is_err() { break; }
                                    }
                                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                                }
                            }
                        });
                        let _ = ready_clipboard;
                    })
                }));
            }

            if label == "CONTROL_DATA_CHANNEL"
                || label == "TEXT_DATA_CHANNEL"
                || label == "BINARY_DATA_CHANNEL"
                || label == "FILE_DATA_CHANNEL"
                || label.contains("STREAMER")
            {
                let manager = manager.clone();
                let terminal = terminal.clone();
                let message_channel = channel.clone();
                let message_label = label.clone();
                let actions = actions.clone();
                let sequence = sequence.clone();
                let clipboard_assembly = clipboard_assembly.clone();
                let clipboard_ready = clipboard_ready.clone();
                let sound = sound;
                let save_location = save_location.clone();
                let capture_change_type = capture_change_type;
                channel.on_message(Box::new(move |message| {
                    let manager = manager.clone();
                    let terminal = terminal.clone();
                    let channel = message_channel.clone();
                    let label = message_label.clone();
                    let desktop = desktop.clone();
                    let settings = settings.clone();
                    let clipboard = clipboard.clone();
                    let clipboard_assembly = clipboard_assembly.clone();
                    let clipboard_ready = clipboard_ready.clone();
                    let port_mapping = port_mapping.clone();
                    let file_transfer = file_transfer.clone();
                    let actions = actions.clone();
                    let sequence = sequence.clone();
                    let save_location = save_location.clone();
                    let video_track = video_track.clone();
                    let mapped_input = mapped_input.clone();
                    let lock_watch = crate::desktop::session_lock::watch_locked();
                    Box::pin(async move {
                        let data = message.data.to_vec();
                        if label.contains("STREAMER") {
                            // The official viewer probes this telemetry channel
                            // with JSON pings. Answer immediately; never feed
                            // telemetry into the input/clipboard decoders.
                            if let Some(pong) = streamer_pong(&data) {
                                let _ = channel
                                    .send(&bytes::Bytes::from_static(pong))
                                    .await;
                            }
                            return;
                        }
                        if port_mapping.handle(&data, &label).await {
                            return;
                        }
                        // Viewers put FileTransferFtp RPCs on CONTROL; accept
                        // them on TEXT and FILE as well, like the official host.
                        if matches!(
                            label.as_str(),
                            "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL"
                        ) && file_transfer.handle(&data, &label).await
                        {
                            return;
                        }
                        if label == "BINARY_DATA_CHANNEL" && term::is_frame(&data) {
                            terminal.handle(&data);
                            return;
                        }
                        if label == "BINARY_DATA_CHANNEL" {
                            tracing::debug!(
                                "native UU non-TERM BINARY message: {} bytes",
                                data.len()
                            );
                        }

                        // Official ClipboardChange (field 17), possibly chunked.
                        if let Some(fields) = wire::decode(&data) {
                            for field in &fields {
                                if let (17, wire::Value::Bytes(payload)) =
                                    (field.number, &field.value)
                                {
                                    if let Some((incoming, _, _)) = &clipboard {
                                        let result = clipboard_assembly
                                            .lock()
                                            .ok()
                                            .and_then(|mut assembly| assembly.accept(payload).ok())
                                            .flatten();
                                        if let Some(text) = result {
                                            incoming.send_replace(Some(text));
                                        }
                                    }
                                }
                            }
                        }

                        if let Some(input) = &mapped_input {
                            if let Ok(value) = serde_json::from_slice::<Value>(&data) {
                                let locked =
                                    crate::desktop::session_lock::effective_locked(*lock_watch.borrow());
                                if !locked && handle_desktop_input(input.clone(), &value) {
                                    crate::desktop::clipboard::mark_focus();
                                    return;
                                }
                                if locked {
                                    // Releases sent while the session was
                                    // locking never reached the mapping layer;
                                    // clear translated state before unlock.
                                    input.reset();
                                    if value.get("action").is_some() {
                                        return;
                                    }
                                }
                            }
                        }

                        let rpc = wire::rpc_request(&data);
                        if let Some((_request_id, case, rpc_fields)) = rpc.clone() {
                            if let Some((incoming, state, next_id)) = &clipboard {
                                let result = {
                                    let Ok(mut state) = state.lock() else {
                                        return;
                                    };
                                    if let Some(text_change) = wire::blob(&rpc_fields, 10) {
                                        if let Some(text_change) = wire::decode(text_change) {
                                            if let Some(text) = wire::blob(&text_change, 2)
                                                .and_then(|value| std::str::from_utf8(value).ok())
                                            {
                                                incoming.send_replace(Some(text.to_owned()));
                                            }
                                        }
                                    }
                                    if wire::blob(&rpc_fields, 9).is_some() {
                                        Some(state.handle_request(&rpc_fields, || {
                                            next_id.fetch_add(1, Ordering::Relaxed)
                                        }))
                                    } else {
                                        None
                                    }
                                };
                                if let Some(result) = result {
                                    match result {
                                        Ok((messages, text)) => {
                                            for message in messages {
                                                let _ = channel
                                                    .send(&bytes::Bytes::from(message))
                                                    .await;
                                            }
                                            if let Some(text) = text {
                                                incoming.send_replace(Some(text));
                                            }
                                        }
                                        Err(error) => tracing::debug!(
                                            "clipboard request rejected: {error}"
                                        ),
                                    }
                                    return;
                                }
                            }
                            // Session/plugin RPCs that have no desktop payload.
                            if matches!(case, 15 | 21) {
                                if let Some(body) = super::system_actions::session_rpc_response(&rpc_fields)
                                {
                                    send_framed(&channel, &sequence, 22, &body).await;
                                    return;
                                }
                            }
                            if (16..=20).contains(&case) {
                                if let Some(body) = super::system_actions::plugin_response(&rpc_fields) {
                                    send_framed(&channel, &sequence, 22, &body).await;
                                    return;
                                }
                            }
                            // Terminal RPCs (check_env/rename) arrive on either
                            // CONTROL or BINARY: the official Windows client
                            // sends check_env on BINARY next to TERM frames.
                            if let Some(header) = wire::blob(&rpc_fields, 1) {
                                let id = wire::decode(header)
                                    .and_then(|header| wire::uint(&header, 1))
                                    .unwrap_or(0);
                                if rpc_fields.iter().any(|field| field.number == 28) {
                                    tracing::debug!(
                                        "native UU terminal check_env request {id} answered"
                                    );
                                    let body = wire::terminal_env_response(id);
                                    let _ = channel
                                        .send(&bytes::Bytes::from(body))
                                        .await;
                                    return;
                                }
                                if let Some(body) = wire::blob(&rpc_fields, 29) {
                                    if let Some(body) = wire::decode(body) {
                                        let session = wire::uint(&body, 1).unwrap_or(0) as u16;
                                        let name = wire::blob(&body, 2)
                                            .and_then(|value| std::str::from_utf8(value).ok())
                                            .unwrap_or("");
                                        let _ = manager.rename(session, name);
                                        let body = wire::terminal_rename_response(
                                            id,
                                            session as u64,
                                        );
                                        let _ = channel
                                            .send(&bytes::Bytes::from(body))
                                            .await;
                                        return;
                                    }
                                }
                            }
                            if label != "CONTROL_DATA_CHANNEL" {
                                return;
                            }
                            match case {
                                    2 | 3 | 4 | 6 => {
                                        if let (Some(settings), Some(desktop)) =
                                            (settings.as_ref(), desktop.as_ref())
                                        {
                                            let (h264, h265) = video_track
                                                .as_ref()
                                                .map(|track| track.available())
                                                .unwrap_or((true, false));
                                            let h265 = h265
                                                && crate::desktop::encoder::hevc_available();
                                            // X11 changes the host display mode for the
                                            // viewer's resolution choice. Probe the mode
                                            // transaction synchronously on a worker before
                                            // applying the settings against the new size.
                                            let requested = crate::uu_native::video_settings::requested_resolution(&rpc_fields);
                                            let native = if desktop.backend
                                                == crate::desktop::Backend::X11
                                                && crate::desktop::display_mode::enabled()
                                            {
                                                match requested {
                                                    Some(request) => {
                                                        let resize = desktop.clone();
                                                        tokio::task::spawn_blocking(move || {
                                                            resize.apply_resolution(request)
                                                        })
                                                        .await
                                                        .ok()
                                                        .flatten()
                                                        .unwrap_or_else(|| desktop.active_size())
                                                    }
                                                    None => desktop.active_size(),
                                                }
                                            } else {
                                                desktop.active_size()
                                            };
                                            if let Some(response) =
                                                crate::uu_native::video_settings::apply_rpc(
                                                    &rpc_fields,
                                                    settings,
                                                    native,
                                                    h264,
                                                    h265,
                                                )
                                            {
                                                send_framed(&channel, &sequence, 22, &response).await;
                                                let sync =
                                                    crate::uu_native::video_settings::sync_payload(
                                                        *settings.borrow(),
                                                        native,
                                                    );
                                                send_framed(&channel, &sequence, 25, &sync).await;
                                            }
                                        }
                                    }
                                    23 => {
                                        let apps: Vec<_> = crate::desktop::apps::catalog()
                                            .into_iter()
                                            .map(|app| {
                                                (
                                                    app.key,
                                                    app.name,
                                                    String::new(),
                                                    app.target_path,
                                                    app.app_id,
                                                )
                                            })
                                            .collect();
                                        let body =
                                            wire::quick_launch_scan_response(0, &apps);
                                        // Preserve the viewer's request id.
                                        let request_id = wire::blob(&rpc_fields, 1)
                                            .and_then(wire::decode)
                                            .and_then(|header| wire::uint(&header, 1))
                                            .unwrap_or(0);
                                        let _ = body;
                                        let body = wire::quick_launch_scan_response(request_id, &apps);
                                        let _ = channel.send(&bytes::Bytes::from(body)).await;
                                    }
                                    24 => {
                                        let key = wire::blob(&rpc_fields, 24)
                                            .and_then(|body| {
                                                let body = wire::decode(body)?;
                                                wire::blob(&body, 1).map(<[u8]>::to_vec)
                                            })
                                            .and_then(|key| String::from_utf8(key).ok())
                                            .unwrap_or_default();
                                        let icon = crate::desktop::apps::icon(&key);
                                        let request_id = wire::blob(&rpc_fields, 1)
                                            .and_then(wire::decode)
                                            .and_then(|header| wire::uint(&header, 1))
                                            .unwrap_or(0);
                                        let body = wire::quick_launch_icon_response(
                                            request_id,
                                            &key,
                                            icon.as_deref(),
                                        );
                                        let _ = channel.send(&bytes::Bytes::from(body)).await;
                                    }
                                    _ => {}
                                }
                        }

                        if let Some(simple) = initialization::parse_simple_action(&data) {
                            match simple.action {
                                initialization::action::ECHO_REQUEST => {
                                    let ready = clipboard_ready.load(Ordering::Relaxed);
                                    let caps = Capabilities::host_desktop(ready);
                                    let seq = sequence.fetch_add(1, Ordering::Relaxed);
                                    let body = initialization::echo_response(
                                        seq,
                                        initialization::now_secs(),
                                        simple.seq,
                                        caps,
                                        ready,
                                    );
                                    let _ = channel.send(&bytes::Bytes::from(body)).await;
                                }
                                initialization::action::AUTO_UNLOCK_SYSTEM => {
                                    let password =
                                        crate::desktop::session_lock::password_from_args(&simple.args);
                                    let channel = channel.clone();
                                    let sequence = sequence.clone();
                                    tokio::spawn(async move {
                                        let result = match password {
                                            Some(password) => tokio::task::spawn_blocking(move || {
                                                crate::desktop::session_lock::unlock(password)
                                            })
                                            .await
                                            .unwrap_or(
                                                crate::desktop::session_lock::UnlockResult::Failed,
                                            ),
                                            None => {
                                                crate::desktop::session_lock::UnlockResult::WrongPassword
                                            }
                                        };
                                        tracing::info!("auto unlock result: {result:?}");
                                        if result
                                            != crate::desktop::session_lock::UnlockResult::Success
                                        {
                                            let body = initialization::report_error(
                                                crate::desktop::session_lock::UNLOCK_FAILED_CODE,
                                                "linux auto unlock failed",
                                            );
                                            send_framed(&channel, &sequence, 12, &body).await;
                                        }
                                    });
                                }
                                initialization::action::ACQUIRE_SYSTEM_INFO => {
                                    let payload = actions.metrics_payload(
                                        sound,
                                        Permission::Allow,
                                        Permission::Allow,
                                        Permission::Allow,
                                        &save_location,
                                    );
                                    send_framed(&channel, &sequence, 13, &payload).await;
                                }
                                action
                                    if matches!(
                                        action,
                                        initialization::action::SHOW_DESKTOP
                                            | initialization::action::REBOOT_SYSTEM
                                            | initialization::action::LOCK_SYSTEM
                                            | initialization::action::SHUTDOWN_SYSTEM
                                            | initialization::action::SHOW_TASKMGR
                                            | initialization::action::MUTE_SYSTEM
                                            | initialization::action::MUTE_RESTORE
                                            | initialization::action::MUTE_PERMANENT
                                            | initialization::action::MUTE_TEMPORARY
                                            | initialization::action::SET_AUTO_LOCK
                                    ) =>
                                {
                                    let actions = actions.clone();
                                    let input = desktop.as_ref().map(|desktop| desktop.input());
                                    let value = simple.value;
                                    let args = simple.args.clone();
                                    let channel = channel.clone();
                                    let sequence = sequence.clone();
                                    let sound = sound;
                                    let save_location = save_location.clone();
                                    let actions_worker = actions.clone();
                                    let actions_report = actions.clone();
                                    let report = tokio::task::spawn_blocking(move || {
                                        actions_worker.handle(action, value, &args, || {
                                            if let Some(input) = &input {
                                                input.key(0x5B, true);
                                                input.key(0x44, true);
                                                input.key(0x44, false);
                                                input.key(0x5B, false);
                                            }
                                        })
                                    })
                                    .await
                                    .unwrap_or(false);
                                    if report {
                                        let payload = actions_report.metrics_payload(
                                            sound,
                                            Permission::Allow,
                                            Permission::Allow,
                                            Permission::Allow,
                                            &save_location,
                                        );
                                        send_framed(&channel, &sequence, 13, &payload).await;
                                    }
                                }
                                initialization::action::RESTART_CAPTURE => {
                                    if let Some(desktop) = &desktop {
                                        let body = initialization::capture_change(
                                            capture_change_type,
                                            0,
                                            "restart",
                                        );
                                        if let Some(body) = body {
                                            send_framed(&channel, &sequence, 8, &body).await;
                                        }
                                        let screens = desktop.screens();
                                        if let Some(body) = initialization::screen_sources_from(
                                            &screens,
                                            desktop.screen(),
                                            None,
                                            "Linux desktop",
                                            screen_type,
                                            advertised_fps,
                                        ) {
                                            send_framed(&channel, &sequence, 7, &body).await;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }

                        if let Some(id) = initialization::query_system_state(&data) {
                            if id == 1 {
                                let locked = crate::desktop::session_lock::effective_locked(
                            crate::desktop::session_lock::locked_now(),
                        );
                                let body = initialization::secure_desktop(
                                    locked,
                                    false,
                                    crate::desktop::session_lock::supported(),
                                );
                                send_framed(&channel, &sequence, 15, &body).await;
                            }
                        }

                        if label == "CONTROL_DATA_CHANNEL" || label == "TEXT_DATA_CHANNEL" {
                            if let Some(rpc_fields) = wire::rpc_response(&data) {
                                if let Some((incoming, state, _)) = &clipboard {
                                    let text = state
                                        .lock()
                                        .ok()
                                        .and_then(|mut state| {
                                            state.handle_response(&rpc_fields).ok().flatten()
                                        });
                                    if let Some(text) = text {
                                        incoming.send_replace(Some(text));
                                    }
                                }
                            }
                        }

                        if label == "CONTROL_DATA_CHANNEL" {
                            if let Some(key) = wire::launch_app_key(&data) {
                                if let Err(error) = crate::desktop::apps::launch(&key) {
                                    tracing::warn!("quick launch failed: {error}");
                                }
                                return;
                            }
                            if let Some((screen_id, request_seq, config_flag)) =
                                wire::capture_config(&data)
                            {
                                let switched = desktop
                                    .as_ref()
                                    .is_some_and(|desktop| desktop.switch_screen(screen_id as u32));
                                if switched {
                                    tracing::info!("desktop switched to screen {screen_id}");
                                } else {
                                    tracing::warn!("unknown desktop screen {screen_id}");
                                }
                                let mut body = Vec::new();
                                wire::int(1, request_seq, &mut body);
                                wire::int(2, config_flag, &mut body);
                                wire::int(
                                    3,
                                    if desktop.is_some() && switched { 0 } else { 1 },
                                    &mut body,
                                );
                                send_framed(&channel, &sequence, 19, &body).await;
                                if let Some(desktop) = &desktop {
                                    let screens = desktop.screens();
                                    if let Some(body) = initialization::screen_sources_from(
                                        &screens,
                                        desktop.screen(),
                                        None,
                                        "Linux desktop",
                                        screen_type,
                                        advertised_fps,
                                    ) {
                                        send_framed(&channel, &sequence, 7, &body).await;
                                    }
                                    if let Some(settings) = &settings {
                                        let sync = crate::uu_native::video_settings::sync_payload(
                                            *settings.borrow(),
                                            desktop.active_size(),
                                        );
                                        send_framed(&channel, &sequence, 25, &sync).await;
                                    }
                                }
                            }
                        }
                    })
                }));
            }
        })
    }));
}

async fn handle_soac(
    manager: &SessionManager,
    outbox: &Outbox,
    peers: &mut HashMap<String, HostPeer>,
    pending: &mut Vec<Pending>,
    desktop_cache: &mut Option<CachedDesktop>,
    data: &Value,
    config: &Config,
) {
    let body = data.get("data").unwrap_or(data);
    let ice = body.get("ice_id").and_then(Value::as_str).unwrap_or("");
    let client = data.get("client_id").and_then(Value::as_str).unwrap_or("");
    let app = body
        .get("app_control_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    match body.get("type").and_then(Value::as_str) {
        Some("offer") => {
            let index = pending
                .iter()
                .position(|item| !ice.is_empty() && item.ice.as_deref() == Some(ice))
                .or_else(|| (!pending.is_empty()).then_some(0));
            let mut request = index.map(|index| pending.remove(index));
            let route = request
                .as_ref()
                .map(|item| Route {
                    client: if item.client.is_empty() {
                        client.to_owned()
                    } else {
                        item.client.clone()
                    },
                    ice: ice.to_owned(),
                    app: if item.app.is_empty() {
                        app.to_owned()
                    } else {
                        item.app.clone()
                    },
                })
                .unwrap_or_else(|| Route {
                    client: client.to_owned(),
                    ice: ice.to_owned(),
                    app: app.to_owned(),
                });
            if route.ice.is_empty() {
                return;
            }
            if let Some(peer) = peers.get(&route.ice) {
                if let Ok(offer) = rtc::answer_sdp(body) {
                    if let Ok(answer) = peer.answer(&offer).await {
                        tracing::info!("native UU re-sent answer for ice={}", peer.route.ice);
                        outbox.soac(json!({
                            "client_id": peer.route.client,
                            "data": {
                                "type": "answer",
                                "sdp": answer,
                                "ice_id": peer.route.ice,
                                "app_control_id": peer.route.app,
                            }
                        }));
                    }
                }
                return;
            }
            let header = request
                .as_ref()
                .map(|item| item.header.clone())
                .unwrap_or(Value::Null);
            let capture_type = request.as_ref().map(|item| item.capture_type).unwrap_or(0);
            let controller_mac = request
                .as_ref()
                .map(|item| item.controller_mac)
                .unwrap_or_else(|| crate::desktop::input_map::controller_is_mac(&header, None));
            let mut desktop = if matches!(capture_type, 1 | 2) {
                let pending_desktop = request.as_mut().and_then(|item| item.desktop.take());
                match pending_desktop {
                    None => {
                        // The request was cancelled (for example by a
                        // session change). Still answer; CONTROL reports the
                        // failure.
                        tracing::warn!("native UU desktop offer has no capture request");
                        None
                    }
                    Some(PendingDesktop::Ready(started)) => Some(started),
                    Some(PendingDesktop::Starting(receiver)) => {
                        match tokio::time::timeout(Duration::from_secs(90), receiver).await {
                            Ok(Ok(Ok(started))) => Some(started),
                            Ok(Ok(Err(error))) => {
                                tracing::warn!("native UU desktop capture unavailable: {error}");
                                None
                            }
                            Ok(Err(_)) => {
                                tracing::warn!("native UU desktop capture task stopped");
                                None
                            }
                            Err(_) => {
                                tracing::warn!("native UU desktop capture timed out");
                                None
                            }
                        }
                    }
                }
            } else {
                None
            };
            if matches!(capture_type, 1 | 2) && desktop.is_none() {
                // Answer anyway: the CONTROL channel reports "capture
                // unavailable" so a pre-login or failed-portal offer fails
                // visibly instead of timing out unanswered.
                tracing::warn!(
                    "native UU desktop offer without a usable capture; answering without video"
                );
            }
            if let Some(started) = &desktop {
                if session_env::current().map(|env| env.identity())
                    != Some(started.session.identity())
                {
                    started.desktop.stop();
                    tracing::warn!(
                        "native UU desktop session changed before the offer; answering without video"
                    );
                    desktop = None;
                }
            }
            if let Some(started) = &desktop {
                cache_portal_desktop(desktop_cache, peers, started);
            }
            let session = desktop.as_ref().map(|started| started.session.identity());
            match create_peer(
                manager,
                outbox,
                route.clone(),
                &header,
                desktop.map(|started| started.desktop),
                session,
                capture_type,
                controller_mac,
                config,
            )
            .await
            {
                Ok(peer) => {
                    peers.insert(route.ice.clone(), peer);
                    let answered = {
                        let Some(peer) = peers.get(&route.ice) else {
                            return;
                        };
                        match rtc::answer_sdp(body) {
                            Ok(offer) => peer.answer(&offer).await,
                            Err(error) => Err(Error::Protocol(error)),
                        }
                    };
                    match answered {
                        Ok(answer) => {
                            if let Some(peer) = peers.get(&route.ice) {
                                tracing::info!("native UU answer sent for ice={}", peer.route.ice);
                                outbox.soac(json!({
                                    "client_id": peer.route.client,
                                    "data": {
                                        "type": "answer",
                                        "sdp": answer,
                                        "ice_id": peer.route.ice,
                                        "app_control_id": peer.route.app,
                                    }
                                }))
                            }
                        }
                        Err(error) => {
                            tracing::warn!("native UU answer failed: {error}");
                            // A peer that never received an answer stays in
                            // `New` forever, pinning its capture and the
                            // process-wide sleep inhibitor.
                            if let Some(peer) = peers.remove(&route.ice) {
                                peer.close().await;
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!("native UU peer setup failed: {error}"),
            }
        }
        Some("answer") => {
            // Response to a host-initiated ICE-restart offer.
            let Some(peer) = peers.get(ice) else {
                return;
            };
            match rtc::answer_sdp(body) {
                Ok(answer) => {
                    if let Err(error) = peer.accept_answer(&answer).await {
                        tracing::warn!("native UU ICE-restart answer failed: {error}");
                    } else {
                        tracing::info!("native UU ICE-restart answer applied for ice={ice}");
                    }
                }
                Err(error) => tracing::warn!("native UU answer decode failed: {error}"),
            }
        }
        Some("candidate") => {
            let Some(peer) = peers.get(ice) else {
                return;
            };
            if let Some(candidate) = body.get("candidate") {
                if let Err(error) = peer.add_candidate(candidate).await {
                    tracing::warn!("native UU candidate failed: {error}");
                }
            }
        }
        _ => {}
    }
}

/// Retain a portal capture for the next offer. X11/wlroots sessions own a
/// display mode or output state that must be released at the end of a remote
/// session, so only the portal (which does not change the display) is cached.
fn cache_portal_desktop(
    cache: &mut Option<CachedDesktop>,
    peers: &HashMap<String, HostPeer>,
    started: &StartedDesktop,
) {
    if started.desktop.backend != desktop::Backend::Wayland {
        return;
    }
    let next = CachedDesktop {
        desktop: started.desktop.clone(),
        session: started.session.clone(),
        source: started.source,
    };
    let old = cache.replace(next);
    if let Some(old) = old {
        if !Arc::ptr_eq(&old.desktop, &started.desktop)
            && !peers.values().any(|peer| {
                peer.desktop
                    .as_ref()
                    .is_some_and(|desktop| Arc::ptr_eq(desktop, &old.desktop))
            })
        {
            // No peer uses the previous source any more; close its portal
            // instead of leaving a second capture indicator running.
            old.desktop.stop();
        }
    }
}

/// Drop the retained portal capture when it no longer belongs to the active
/// login session. Peers tied to an old session are already closed by
/// `close_desktop_peers`, so this handle is the last owner.
fn drop_desktop_cache(runtime: &mut HostRuntime, current: Option<&session_env::SessionEnv>) {
    let current = current.map(session_env::SessionEnv::identity);
    let stale = runtime
        .desktop_cache
        .as_ref()
        .is_some_and(|cached| current.as_ref() != Some(&cached.session.identity()));
    if stale {
        if let Some(cached) = runtime.desktop_cache.take() {
            cached.desktop.stop();
            tracing::info!("released cached portal capture: graphical session changed");
        }
    }
}

/// A desktop peer belongs to a login session that no longer exists.
fn desktop_peer_mismatch(
    has_desktop: bool,
    session: Option<&SessionIdentity>,
    current: Option<&SessionIdentity>,
) -> bool {
    has_desktop && session != current
}

fn desktop_peer_keys(
    peers: &HashMap<String, HostPeer>,
    current: Option<&session_env::SessionEnv>,
) -> Vec<String> {
    let current = current.map(session_env::SessionEnv::identity);
    peers
        .iter()
        .filter(|(_, peer)| {
            desktop_peer_mismatch(
                peer.desktop.is_some(),
                peer.session.as_ref(),
                current.as_ref(),
            )
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// Close only desktop captures whose session changed. Terminal, file, and
/// port-mapping peers stay connected.
async fn close_desktop_peers(runtime: &mut HostRuntime, current: Option<&session_env::SessionEnv>) {
    for key in desktop_peer_keys(&runtime.peers, current) {
        if let Some(peer) = runtime.peers.remove(&key) {
            tracing::info!("closing desktop peer {key}: its login session changed");
            peer.close().await;
        }
    }
}

/// Drop captures that have not finished starting. Each task stops its portal
/// or PipeWire session when its receiver is gone.
fn cancel_pending_desktop(runtime: &mut HostRuntime) {
    let mut cancelled = 0;
    for pending in &mut runtime.pending {
        if pending.desktop.take().is_some() {
            cancelled += 1;
        }
    }
    if cancelled > 0 {
        tracing::debug!("cancelled {cancelled} pending desktop captures after a session change");
    }
}

/// Close the peers named by a release event. Returns true when the event had
/// no peer identifier at all: that is a room-level event and must not tear
/// down the terminal, file, and port-mapping peers connected to this room.
/// Releases may name the peer by `ice_id`, `client_id`, or `app_control_id`.
async fn release(peers: &mut HashMap<String, HostPeer>, data: &Value) -> bool {
    let body = data.get("data").unwrap_or(data);
    let field = |keys: &[&str]| -> Option<&str> {
        keys.iter()
            .find_map(|key| body.get(*key).or_else(|| data.get(*key)))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let ice = field(&["ice_id", "iceId"]);
    let client = field(&["client_id", "clientId"]);
    let app = field(&["app_control_id", "appControlId"]);
    if ice.is_none() && client.is_none() && app.is_none() {
        return true;
    }
    let keys: Vec<String> = peers
        .iter()
        .filter(|(_, peer)| match (ice, client, app) {
            (Some(ice), _, _) => peer.route.ice == ice,
            (None, Some(client), app) => {
                peer.route.client == client && app.is_none_or(|app| peer.route.app == app)
            }
            (None, None, Some(app)) => peer.route.app == app,
            (None, None, None) => false,
        })
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        if let Some(peer) = peers.remove(&key) {
            peer.close().await;
        }
    }
    false
}

fn handle_desktop_input(input: Arc<dyn InputSink>, value: &Value) -> bool {
    if value.get("heartbeat").and_then(Value::as_str) == Some("kbd_heartbeat") {
        return true;
    }
    let Some(action) = value.get("action").and_then(Value::as_str) else {
        return false;
    };
    let integer = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)))
            .and_then(|n| i32::try_from(n).ok())
    };
    match action {
        "kbd_press" => {
            if let Some(key) = integer("key") {
                input.key(key, true);
            }
        }
        "kbd_release" => {
            if let Some(key) = integer("key") {
                input.key(key, false);
            }
        }
        "kbd_click" => {
            if let Some(key) = integer("key") {
                input.key(key, true);
                input.key(key, false);
            }
        }
        "mouse_press" => {
            if let Some(button) = integer("button") {
                input.button(button, true);
            }
        }
        "mouse_release" => {
            if let Some(button) = integer("button") {
                input.button(button, false);
            }
        }
        "mouse_click" => {
            if let Some(button) = integer("button") {
                input.button(button, true);
                input.button(button, false);
            }
        }
        "mouse_move_absolute" => {
            if let (Some(x), Some(y)) = (
                value.get("abs_x").and_then(Value::as_f64),
                value.get("abs_y").and_then(Value::as_f64),
            ) {
                input.pointer_absolute(x, y);
            }
        }
        "mouse_move_relative" => {
            if let (Some(dx), Some(dy)) = (integer("delta_x"), integer("delta_y")) {
                input.pointer_relative(dx, dy);
            }
        }
        "mouse_scroll" => {
            if let (Some(dx), Some(dy)) = (integer("delta_x"), integer("delta_y")) {
                tracing::debug!("desktop scroll: dx={dx} dy={dy}");
                input.scroll(dx, dy);
            } else {
                tracing::debug!("desktop scroll: unsupported delta shape: {value}");
            }
        }
        "text_input" => {
            if let Some(text) = value.get("content").and_then(Value::as_str) {
                if text.len() <= 16 * 1024 {
                    input.text(text);
                }
            }
        }
        _ => return false,
    }
    true
}

fn find_str(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
    }
    if let Some(data) = value.get("data") {
        return find_str(data, keys);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamer_ping_gets_pong_and_telemetry_is_ignored() {
        assert_eq!(
            streamer_pong(br#"{"type":"ping"}"#),
            Some(&br#"{"type":"pong"}"#[..])
        );
        assert_eq!(streamer_pong(br#"{"type":"stats"}"#), None);
        assert_eq!(streamer_pong(b"not-json"), None);
    }

    #[test]
    fn idle_room_renewal_is_nine_hundred_seconds() {
        assert_eq!(IDLE_ROOM_RENEWAL.as_secs(), 900);
    }

    #[test]
    fn room_level_release_keeps_peers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut peers = HashMap::new();
            // No peer id: a room-level event, not a request to close peers.
            assert!(release(&mut peers, &json!({"data": {"ice_id": ""}})).await);
            assert!(release(&mut peers, &json!({"data": {}})).await);
            // A named peer release is not room-level.
            assert!(!release(&mut peers, &json!({"ice_id": "missing"})).await);
            assert!(!release(&mut peers, &json!({"data": {"client_id": "viewer"}})).await);
        });
    }

    #[test]
    fn terminal_connect_options_are_remembered() {
        let mut runtime = HostRuntime {
            manager: SessionManager::new(None),
            peers: HashMap::new(),
            pending: Vec::new(),
            outbox: Outbox::new(),
            desktop_cache: None,
        };
        let attachment = wire::connect_options(8, 4, "device", "4.43.0");
        remember_connect(
            &mut runtime,
            json!({"ice_id":"ice","client_id":"client","app_control_id":"app"}),
            vec![attachment],
            &Config::default(),
        );
        assert_eq!(runtime.pending.len(), 1);
        assert_eq!(runtime.pending[0].ice.as_deref(), Some("ice"));
        assert_eq!(runtime.pending[0].client, "client");
        assert_eq!(runtime.pending[0].app, "app");
    }

    #[test]
    fn desktop_control_json_maps_to_input_events() {
        struct Sink(std::sync::Mutex<Vec<String>>);
        impl InputSink for Sink {
            fn pointer_absolute(&self, x: f64, y: f64) {
                self.0.lock().unwrap().push(format!("abs:{x}:{y}"));
            }
            fn pointer_relative(&self, dx: i32, dy: i32) {
                self.0.lock().unwrap().push(format!("rel:{dx}:{dy}"));
            }
            fn button(&self, button: i32, pressed: bool) {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("btn:{button}:{pressed}"));
            }
            fn scroll(&self, dx: i32, dy: i32) {
                self.0.lock().unwrap().push(format!("scroll:{dx}:{dy}"));
            }
            fn key(&self, vk: i32, pressed: bool) {
                self.0.lock().unwrap().push(format!("key:{vk}:{pressed}"));
            }
            fn text(&self, text: &str) {
                self.0.lock().unwrap().push(format!("text:{text}"));
            }
        }
        let sink = Arc::new(Sink(std::sync::Mutex::new(Vec::new())));
        let input: Arc<dyn InputSink> = sink.clone();
        assert!(handle_desktop_input(
            input.clone(),
            &json!({"action":"mouse_move_absolute","abs_x":0.5,"abs_y":0.25})
        ));
        assert!(handle_desktop_input(
            input.clone(),
            &json!({"action":"mouse_press","button":1})
        ));
        assert!(handle_desktop_input(
            input.clone(),
            &json!({"action":"kbd_click","key":65})
        ));
        assert!(handle_desktop_input(
            input,
            &json!({"action":"text_input","content":"你好"})
        ));
        let events = sink.0.lock().unwrap();
        assert_eq!(events[0], "abs:0.5:0.25");
        assert_eq!(events[1], "btn:1:true");
        assert_eq!(events[2], "key:65:true");
        assert_eq!(events[3], "key:65:false");
        assert_eq!(events[4], "text:你好");
    }
    #[test]
    fn route_fields_are_read_from_nested_headers() {
        let header = json!({"data":{"ice_id":"i","client_id":"c","app_control_id":"a"}});
        assert_eq!(find_str(&header, &["ice_id"]).as_deref(), Some("i"));
        assert_eq!(find_str(&header, &["client_id"]).as_deref(), Some("c"));
    }

    fn identity(leader_pid: u32) -> SessionIdentity {
        SessionIdentity {
            session_id: "2".to_owned(),
            session_type: session_env::SessionType::Wayland,
            display: Some("wayland-0".to_owned()),
            runtime_dir: "/run/user/1000".into(),
            bus_address: "unix:path=/run/user/1000/bus".to_owned(),
            desktop: session_env::DesktopKind::Gnome,
            leader_pid,
        }
    }

    #[test]
    fn only_mismatched_desktop_peers_are_released() {
        let old = identity(100);
        let new = identity(200);
        // Same session: a desktop capture stays connected.
        assert!(!desktop_peer_mismatch(true, Some(&old), Some(&old)));
        // A compositor restart changes the leader pid and invalidates capture.
        assert!(desktop_peer_mismatch(true, Some(&old), Some(&new)));
        // Losing the session closes desktop captures...
        assert!(desktop_peer_mismatch(true, Some(&old), None));
        // ...but never terminal, file or port-mapping peers.
        assert!(!desktop_peer_mismatch(false, Some(&old), None));
        assert!(!desktop_peer_mismatch(false, None, Some(&new)));
        // A peer without a recorded session cannot be reused safely once a
        // session appears.
        assert!(desktop_peer_mismatch(true, None, Some(&new)));
    }

    #[test]
    fn cached_portal_capture_is_scoped_to_source_and_session() {
        let session = identity(100);
        let other = identity(200);
        assert!(desktop_cache_matches(
            desktop::SourceKind::Monitor,
            &session,
            desktop::SourceKind::Monitor,
            Some(&session)
        ));
        assert!(!desktop_cache_matches(
            desktop::SourceKind::Monitor,
            &session,
            desktop::SourceKind::Window,
            Some(&session)
        ));
        assert!(!desktop_cache_matches(
            desktop::SourceKind::Monitor,
            &session,
            desktop::SourceKind::Monitor,
            Some(&other)
        ));
        assert!(!desktop_cache_matches(
            desktop::SourceKind::Monitor,
            &session,
            desktop::SourceKind::Monitor,
            None
        ));
    }
}

#[cfg(test)]
mod local_webrtc_tests {
    use super::*;
    use crate::uu_native::term;

    #[tokio::test]
    async fn local_offer_answer_and_terminal_round_trip() {
        let manager = SessionManager::new(None);
        let host_pc = rtc::peer(
            true,
            webrtc::peer_connection::configuration::RTCConfiguration::default(),
        )
        .await
        .unwrap();
        let binary: Arc<Mutex<Option<Arc<webrtc::data_channel::RTCDataChannel>>>> =
            Arc::new(Mutex::new(None));
        let (host_tx, mut host_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let writer_binary = binary.clone();
        tokio::spawn(async move {
            while let Some(data) = host_rx.recv().await {
                let channel = writer_binary.lock().unwrap().clone();
                if let Some(channel) = channel {
                    let _ = channel.send(&bytes::Bytes::from(data)).await;
                }
            }
        });
        let terminal = HostTerminal::new(manager.clone(), host_tx);
        terminal.event_loop();
        install_host_channels(
            &host_pc,
            manager,
            terminal,
            binary.clone(),
            None,
            None,
            None,
            Arc::new(PeerActions::default()),
            8,
            false,
            Config::default(),
        );

        let viewer_pc = rtc::peer(
            false,
            webrtc::peer_connection::configuration::RTCConfiguration::default(),
        )
        .await
        .unwrap();
        let channels = rtc::data_channels(&viewer_pc).await.unwrap();
        let (viewer_tx, mut viewer_rx) = mpsc::unbounded_channel::<(usize, Vec<u8>)>();
        for (index, channel) in channels.iter().enumerate() {
            let viewer_tx = viewer_tx.clone();
            channel.on_message(Box::new(move |message| {
                let _ = viewer_tx.send((index, message.data.to_vec()));
                Box::pin(async {})
            }));
        }

        let (host_candidates_tx, mut host_candidates_rx) = mpsc::unbounded_channel();
        let host_for_ice = host_pc.clone();
        host_pc.on_ice_candidate(Box::new(move |candidate| {
            let tx = host_candidates_tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(init) = candidate.to_json() {
                        let _ = tx.send(init);
                    }
                }
            })
        }));
        let (viewer_candidates_tx, mut viewer_candidates_rx) = mpsc::unbounded_channel();
        viewer_pc.on_ice_candidate(Box::new(move |candidate| {
            let tx = viewer_candidates_tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(init) = candidate.to_json() {
                        let _ = tx.send(init);
                    }
                }
            })
        }));

        let viewer_bundle = Mutex::new(None);
        let offer = rtc::local_offer(&viewer_pc, &viewer_bundle).await.unwrap();
        rtc::set_offer(&host_pc, &offer).await.unwrap();
        let host_bundle = Mutex::new(None);
        let answer = rtc::local_answer(&host_pc, &host_bundle).await.unwrap();
        rtc::set_answer(&viewer_pc, &answer).await.unwrap();
        assert!(
            host_bundle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            "answer must expose an active BUNDLE m-line"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                Some(candidate) = host_candidates_rx.recv() => {
                    let _ = viewer_pc.add_ice_candidate(candidate).await;
                }
                Some(candidate) = viewer_candidates_rx.recv() => {
                    let _ = host_pc.add_ice_candidate(candidate).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if rtc::channel_open(&channels[4]) {
                break;
            }
        }
        assert!(rtc::channel_open(&channels[4]), "data channel did not open");

        // The official Windows client sends terminal_check_env_req on BINARY
        // next to TERM frames; it must be answered there, not only on CONTROL.
        let _ = channels[4]
            .send(&bytes::Bytes::from(wire::request(7, 28, &[])))
            .await;
        let mut env_answered = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                Some((channel, data)) = viewer_rx.recv() => {
                    if channel == 4 {
                        if let Some(fields) = wire::decode(&data) {
                            if let Some(rpc) = wire::blob(&fields, 22) {
                                if let Some(rpc) = wire::decode(rpc) {
                                    if wire::blob(&rpc, 24).is_some() {
                                        env_answered = true;
                                    }
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if env_answered {
                break;
            }
        }
        assert!(env_answered, "check_env response not received on BINARY");

        let open = serde_json::json!({"cols":80,"rows":24,"option":"none","shell":"bash"});
        let _ = channels[4]
            .send(&bytes::Bytes::from(term::encode(
                term::OPEN,
                0,
                open.to_string().as_bytes(),
            )))
            .await;
        let mut opened = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                Some((_, data)) = viewer_rx.recv() => {
                    if let Some(frame) = term::decode(&data) {
                        if frame.kind == term::OPEN_RESP {
                            let value: Value = serde_json::from_slice(&frame.payload).unwrap();
                            assert_eq!(value["code"], 0);
                            opened = true;
                            let _ = channels[4].send(&bytes::Bytes::from(term::encode(
                                term::STDIN,
                                frame.session,
                                b"echo LOCAL_TEST; exit 3\n",
                            ))).await;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if opened {
                break;
            }
        }
        assert!(opened, "OPEN_RESP not received");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                Some((_, data)) = viewer_rx.recv() => {
                    if let Some(frame) = term::decode(&data) {
                        if frame.kind == term::STDOUT {
                            output.extend(frame.payload.clone());
                        }
                        if frame.kind == term::EXIT {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("LOCAL_TEST"));
        let _ = host_for_ice;
    }
}

#[cfg(test)]
mod host_terminal_events {
    use super::*;
    use crate::uu_native::term;
    #[tokio::test]
    async fn async_host_terminal_streams() {
        let manager = SessionManager::new(None);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let terminal = HostTerminal::new(manager.clone(), tx);
        terminal.event_loop();
        terminal.handle(&term::encode(
            term::OPEN,
            0,
            br#"{"cols":80,"rows":24,"option":"none","shell":"bash"}"#,
        ));
        let frame = term::decode(&rx.recv().await.unwrap()).unwrap();
        let session = frame.session;
        terminal.handle(&term::encode(
            term::STDIN,
            session,
            b"echo EVENTS_OK; exit 3\n",
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(data)) =
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                if let Some(frame) = term::decode(&data) {
                    if frame.kind == term::STDOUT {
                        output.extend(frame.payload);
                    }
                    if frame.kind == term::EXIT {
                        break;
                    }
                }
            }
        }
        assert!(String::from_utf8_lossy(&output).contains("EVENTS_OK"));
    }
}
