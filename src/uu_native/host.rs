//! Native UU host: keeps the device online and answers official controller
//! terminal sessions over WebRTC.

use super::initialization::{self, Capabilities};
use super::network_diagnostics;
use super::rtc;
use super::signal::{Event, Signal};
use super::system_actions::PeerActions;
use super::system_metrics::Permission;
use super::term::{self, HostTerminal};
use super::wire;
use crate::config::Config;
use crate::desktop::{self, DesktopOptions, DesktopSession, InputSink};
use crate::error::{Error, Result};
use crate::terminal::session::SessionManager;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::sctp_transport::sctp_transport_capabilities::SCTPTransportCapabilities;
use webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState;

#[derive(Clone)]
struct Route {
    client: String,
    ice: String,
    app: String,
}

struct Pending {
    capture_type: u64,
    ice: Option<String>,
    client: String,
    app: String,
    header: Value,
    desktop:
        Option<tokio::sync::oneshot::Receiver<std::result::Result<Arc<DesktopSession>, String>>>,
}

struct HostPeer {
    pc: Arc<webrtc::peer_connection::RTCPeerConnection>,
    route: Route,
    desktop: Option<Arc<DesktopSession>>,
    actions: Arc<PeerActions>,
    stopped: AtomicBool,
}

impl HostPeer {
    async fn answer(&self, offer: &str) -> Result<String> {
        rtc::set_offer(&self.pc, offer)
            .await
            .map_err(Error::Transport)?;
        let answer = rtc::local_answer(&self.pc)
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
            desktop.stop();
            self.actions.session_ended();
        }
    }
}

pub async fn run(config: Config, password: Option<String>) -> Result<()> {
    crate::desktop::virtual_audio::init();
    let _virtual_audio = crate::desktop::virtual_audio::Cleanup;
    let manager = SessionManager::new(password);
    let mut backoff = Duration::from_secs(3);
    loop {
        match connect_once(&config, manager.clone()).await {
            Ok(()) => {
                tracing::warn!("native UU host connection closed");
                backoff = Duration::from_secs(3);
            }
            Err(error) => tracing::warn!("native UU host: {error}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn connect_once(config: &Config, manager: SessionManager) -> Result<()> {
    let room = super::api::create(config).await?;
    let mut signal = Signal::connect(&room.servers, &room.token, false)
        .await
        .map_err(Error::Transport)?;
    signal.room_info();
    let mut events = manager.subscribe();
    let mut peers: HashMap<String, HostPeer> = HashMap::new();
    let mut pending: Vec<Pending> = Vec::new();

    loop {
        tokio::select! {
            message = signal.recv() => {
                let Some(message) = message else { break; };
                match message {
                    Event::Closed(error) => return Err(Error::Transport(error)),
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "bmsg_push" => {
                        let status = data.get("data").and_then(|d| d.get("status")).and_then(Value::as_str).unwrap_or("-");
                        let available = data.get("data").and_then(|d| d.get("publisher_availability_status")).and_then(Value::as_str).unwrap_or("-");
                        tracing::info!("native UU device status: {status} availability={available}");
                    }
                    Event::Text { name, data } if name == "be-controlled" => {
                        remember_connect(&mut pending, data, Vec::new(), config);
                    }
                    Event::Binary { name, data, attachments } if name == "be-controlled" => {
                        remember_connect(&mut pending, data, attachments, config);
                    }
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "soac" => {
                        handle_soac(&manager, &signal, &mut peers, &mut pending, &data, events.resubscribe(), config).await;
                    }
                    Event::Text { name, data } | Event::Binary { name, data, .. } if name == "released" || name == "left" || name == "leave" => {
                        release(&mut peers, &data).await;
                    }
                    _ => {}
                }
            }
            event = events.recv() => {
                if matches!(event, Err(broadcast::error::RecvError::Closed)) { break; }
            }
        }
    }
    for peer in peers.values() {
        peer.close().await;
    }
    Ok(())
}

fn remember_connect(
    pending: &mut Vec<Pending>,
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
    let desktop = if matches!(capture_type, 1 | 2) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let source = if capture_type == 2 {
            desktop::SourceKind::Window
        } else {
            config.display_source()
        };
        let key = desktop::portal_permissions::key(source.as_str());
        let options = DesktopOptions {
            restore_token: desktop::portal_permissions::restore_token(&key),
            source,
            clipboard: true,
        };
        tokio::spawn(async move {
            let result = match desktop::detect_backend() {
                Ok(backend) => desktop::start_with(backend, options).await.map(Arc::new),
                Err(error) => Err(error),
            };
            let _ = tx.send(result);
        });
        Some(rx)
    } else {
        None
    };
    if pending.len() >= 32 {
        pending.remove(0);
    }
    pending.push(Pending {
        capture_type,
        ice: find_str(&header, &["ice_id", "iceId"]),
        client: find_str(&header, &["client_id", "clientId"]).unwrap_or_default(),
        app: find_str(&header, &["app_control_id", "appControlId"]).unwrap_or_default(),
        header,
        desktop,
    });
    tracing::info!("native UU capture_type={capture_type} request remembered");
}

#[allow(clippy::too_many_arguments)]
async fn create_peer(
    manager: &SessionManager,
    signal: &Signal,
    route: Route,
    header: &Value,
    desktop: Option<Arc<DesktopSession>>,
    events: broadcast::Receiver<crate::terminal::session::SessionEvent>,
    capture_type: u64,
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
    terminal.clone().event_loop(events);
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
        config.clone(),
    );
    network_diagnostics::start(Arc::downgrade(&pc));

    // A network switch shows up as a prolonged Disconnected state. Restart ICE
    // once per outage; the viewer answers the host offer with `soac answer`.
    {
        let pc = pc.clone();
        let sender = signal.sender();
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
                        if now.duration_since(since) >= Duration::from_secs(4)
                            && restart_attempts < 3
                        {
                            restart_attempts += 1;
                            match super::session_ice::restart_offer(&pc).await {
                                Ok(sdp) => {
                                    tracing::info!(
                                        "network switch: sending ICE restart offer (attempt {restart_attempts})"
                                    );
                                    sender.soac(json!({
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
                    desktop.stop();
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

    let sender = signal.sender();
    let signal_route = route.clone();
    pc.on_ice_candidate(Box::new(move |candidate| {
        let sender = sender.clone();
        let route = signal_route.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else {
                return;
            };
            let Some(value) = rtc::candidate_json(&candidate) else {
                return;
            };
            sender.soac(json!({
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
            signal.sender(),
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
        actions,
        stopped: AtomicBool::new(false),
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
                Some((
                    crate::uu_native::port_mapping::PortMapping::new(
                        file_tx.clone(),
                        Arc::new(std::sync::atomic::AtomicU64::new(1)),
                    ),
                    crate::uu_native::file_transfer::FileTransfer::new(
                        file_tx,
                        Config::default().file_save_path(),
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
                            if port_mapping.handle(&data, &label) {
                                return;
                            }
                            if label == "FILE_DATA_CHANNEL" && file_transfer.handle(&data).await {
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
    config: Config,
) {
    let sequence = Arc::new(AtomicU64::new(1));
    let clipboard_ready = Arc::new(AtomicBool::new(false));
    let sound = crate::desktop::audio::enabled();
    let save_location = config.file_save_path().to_string_lossy().into_owned();
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
        let config = config.clone();
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

            let port_mapping = if label == "FILE_DATA_CHANNEL" {
                let (file_tx, mut file_rx) = mpsc::channel::<Vec<u8>>(64);
                let channel_for_file = channel.clone();
                tokio::spawn(async move {
                    while let Some(message) = file_rx.recv().await {
                        if channel_for_file
                            .send(&bytes::Bytes::from(message))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                Some((
                    crate::uu_native::port_mapping::PortMapping::new(
                        file_tx.clone(),
                        Arc::new(AtomicU64::new(1)),
                    ),
                    Arc::new(crate::uu_native::file_transfer::FileTransfer::new(
                        file_tx,
                        config.file_save_path(),
                    )),
                ))
            } else {
                None
            };
            if let Some((mapping, transfer)) = &port_mapping {
                let mapping = mapping.clone();
                let transfer = transfer.clone();
                channel.on_close(Box::new(move || {
                    mapping.shutdown();
                    transfer.shutdown();
                    Box::pin(async {})
                }));
            }

            let clipboard = if label == "TEXT_DATA_CHANNEL" {
                let bridge = desktop
                    .as_ref()
                    .and_then(|desktop| desktop.take_clipboard())
                    .unwrap_or_else(crate::desktop::clipboard::start);
                let crate::desktop::clipboard::ClipboardBridge {
                    incoming,
                    mut outgoing,
                    ready,
                } = bridge;
                {
                    let flag = clipboard_ready.clone();
                    tokio::spawn(async move {
                        if ready.await.unwrap_or(false) {
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
                                    crate::desktop::session_lock::locked_now() == Some(true),
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
                        let locked = crate::desktop::session_lock::locked_now() == Some(true);
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
                        let _ = ready_clipboard;
                    })
                }));
            }

            if label == "CONTROL_DATA_CHANNEL"
                || label == "TEXT_DATA_CHANNEL"
                || label == "BINARY_DATA_CHANNEL"
                || label == "FILE_DATA_CHANNEL"
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
                    let actions = actions.clone();
                    let sequence = sequence.clone();
                    let save_location = save_location.clone();
                    let video_track = video_track.clone();
                    let lock_watch = crate::desktop::session_lock::watch_locked();
                    Box::pin(async move {
                        let data = message.data.to_vec();
                        if let Some((port_mapping, file_transfer)) = &port_mapping {
                            if port_mapping.handle(&data, &label) {
                                return;
                            }
                            if label == "FILE_DATA_CHANNEL" && file_transfer.handle(&data).await {
                                return;
                            }
                        }
                        if label == "BINARY_DATA_CHANNEL" && term::is_frame(&data) {
                            terminal.handle(&data);
                            return;
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

                        if let Some(desktop) = &desktop {
                            if let Ok(value) = serde_json::from_slice::<Value>(&data) {
                                let locked = *lock_watch.borrow() != Some(false);
                                if !locked && handle_desktop_input(desktop.input(), &value) {
                                    crate::desktop::clipboard::mark_focus();
                                    return;
                                }
                                if locked && value.get("action").is_some() {
                                    return;
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
                            if label != "CONTROL_DATA_CHANNEL" {
                                return;
                            }
                            if rpc.is_some() {
                                if let Some(header) = wire::blob(&rpc_fields, 1) {
                                    let id = wire::decode(header)
                                        .and_then(|header| wire::uint(&header, 1))
                                        .unwrap_or(0);
                                    if rpc_fields.iter().any(|field| field.number == 28) {
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
                                match case {
                                    2 | 3 | 4 | 6 => {
                                        if let (Some(settings), Some(desktop)) =
                                            (settings.as_ref(), desktop.as_ref())
                                        {
                                            let (h264, h265) = video_track
                                                .as_ref()
                                                .map(|track| track.available())
                                                .unwrap_or((true, false));
                                            if let Some(response) =
                                                crate::uu_native::video_settings::apply_rpc(
                                                    &rpc_fields,
                                                    settings,
                                                    desktop.active_size(),
                                                    h264,
                                                    h265,
                                                )
                                            {
                                                send_framed(&channel, &sequence, 22, &response).await;
                                                let sync =
                                                    crate::uu_native::video_settings::sync_payload(
                                                        *settings.borrow(),
                                                        desktop.active_size(),
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
                                let locked = crate::desktop::session_lock::locked_now() == Some(true);
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
    signal: &Signal,
    peers: &mut HashMap<String, HostPeer>,
    pending: &mut Vec<Pending>,
    data: &Value,
    events: broadcast::Receiver<crate::terminal::session::SessionEvent>,
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
                        signal.soac(json!({
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
            let desktop = if matches!(capture_type, 1 | 2) {
                let receiver = request.as_mut().and_then(|item| item.desktop.take());
                let Some(receiver) = receiver else {
                    tracing::warn!("native UU desktop offer has no capture request");
                    return;
                };
                match tokio::time::timeout(Duration::from_secs(90), receiver).await {
                    Ok(Ok(Ok(desktop))) => Some(desktop),
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
            } else {
                None
            };
            match create_peer(
                manager,
                signal,
                route.clone(),
                &header,
                desktop,
                events,
                capture_type,
                config,
            )
            .await
            {
                Ok(peer) => {
                    peers.insert(route.ice.clone(), peer);
                    if let Some(peer) = peers.get(&route.ice) {
                        match rtc::answer_sdp(body) {
                            Ok(offer) => match peer.answer(&offer).await {
                                Ok(answer) => {
                                    tracing::info!(
                                        "native UU answer sent for ice={}",
                                        peer.route.ice
                                    );
                                    signal.soac(json!({
                                        "client_id": peer.route.client,
                                        "data": {
                                            "type": "answer",
                                            "sdp": answer,
                                            "ice_id": peer.route.ice,
                                            "app_control_id": peer.route.app,
                                        }
                                    }))
                                }
                                Err(error) => tracing::warn!("native UU answer failed: {error}"),
                            },
                            Err(error) => tracing::warn!("native UU offer decode failed: {error}"),
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

async fn release(peers: &mut HashMap<String, HostPeer>, data: &Value) {
    let body = data.get("data").unwrap_or(data);
    let ice = body.get("ice_id").and_then(Value::as_str).unwrap_or("");
    let keys: Vec<String> = peers
        .keys()
        .filter(|key| ice.is_empty() || key.as_str() == ice)
        .cloned()
        .collect();
    for key in keys {
        if let Some(peer) = peers.remove(&key) {
            peer.close().await;
        }
    }
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
            .and_then(Value::as_i64)
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
                input.scroll(dx, dy);
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
    fn terminal_connect_options_are_remembered() {
        let mut pending = Vec::new();
        let attachment = wire::connect_options(8, 4, "device", "4.43.0");
        remember_connect(
            &mut pending,
            json!({"ice_id":"ice","client_id":"client","app_control_id":"app"}),
            vec![attachment],
            &Config::default(),
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ice.as_deref(), Some("ice"));
        assert_eq!(pending[0].client, "client");
        assert_eq!(pending[0].app, "app");
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
        terminal.clone().event_loop(manager.subscribe());
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

        let offer = rtc::local_offer(&viewer_pc).await.unwrap();
        rtc::set_offer(&host_pc, &offer).await.unwrap();
        let answer = rtc::local_answer(&host_pc).await.unwrap();
        rtc::set_answer(&viewer_pc, &answer).await.unwrap();

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
        let events = manager.subscribe();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let terminal = HostTerminal::new(manager.clone(), tx);
        terminal.clone().event_loop(events);
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
