//! Native controller: join a room, negotiate WebRTC, and attach to a remote
//! terminal over the BINARY_DATA_CHANNEL.

use super::rtc;
use super::signal::{Event, Signal};
use super::term;
use super::wire;
use crate::config::Config;
use crate::error::{Error, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::peer_connection::RTCPeerConnection;

#[derive(Clone)]
struct Route {
    client: String,
    ice: String,
    app: String,
}

pub async fn run(
    config: &Config,
    device: &str,
    shell: Option<String>,
    requested_session: Option<u32>,
) -> Result<()> {
    let room = super::api::join(config, device).await?;
    let mut signal = Signal::connect(&room.servers, &room.token, true)
        .await
        .map_err(Error::Transport)?;
    let app = uuid::Uuid::new_v4().to_string();
    let ice = hex::encode(rand::random::<[u8; 16]>());
    let control = wire::control_header(&app, &app);
    let options = wire::connect_options(8, 3, device, super::UU_CLIENT_VERSION);
    signal.control(11, control, options);

    let ack = loop {
        match signal.recv().await {
            Some(Event::Ack { id: 11, data }) => break control_ack(&data)?,
            Some(Event::Closed(error)) => return Err(Error::Transport(error)),
            Some(_) => {}
            None => return Err(Error::Transport("signaling closed during control".into())),
        }
    };
    tracing::info!("native UU control acknowledged");
    let remote_client = ack
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Protocol("control ack has no peer client_id".into()))?
        .to_owned();
    let assigned_ice = ack
        .get("ice_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(ice);
    let route = Arc::new(Mutex::new(Route {
        client: remote_client.clone(),
        ice: assigned_ice.clone(),
        app: app.clone(),
    }));
    let configuration = rtc::ice_config(&ack).map_err(Error::Protocol)?;
    let pc = rtc::peer(false, configuration)
        .await
        .map_err(Error::Transport)?;
    let early = Arc::new(Mutex::new(Vec::<Value>::new()));
    let answer_seen = Arc::new(AtomicBool::new(false));
    install_candidates(
        &pc,
        route.clone(),
        signal.sender(),
        early.clone(),
        answer_seen.clone(),
    );
    let channels = rtc::data_channels(&pc).await.map_err(Error::Transport)?;
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<(usize, Vec<u8>)>();
    let echo_seen = Arc::new(AtomicBool::new(false));
    install_channels(&channels, incoming_tx, echo_seen.clone());

    let offer_sdp = rtc::local_offer(&pc).await.map_err(Error::Transport)?;
    tracing::info!("native UU offer created ({} bytes)", offer_sdp.len());
    signal.soac(json!({
        "client_id": remote_client,
        "data": {
            "type": "offer",
            "sdp": offer_sdp,
            "ice_id": route.lock().unwrap().ice,
            "app_control_id": app,
            "ice_network_type": 3,
        }
    }));

    let mut queued = VecDeque::new();
    let mut answered = false;
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(35);
    loop {
        if rtc::channel_open(&channels[0])
            && rtc::channel_open(&channels[4])
            && echo_seen.load(Ordering::Acquire)
        {
            tracing::info!("native UU data channels ready");
            break;
        }
        if tokio::time::Instant::now() >= ready_deadline {
            return Err(Error::Transport("native WebRTC setup timed out".into()));
        }
        let event = if let Some(event) = queued.pop_front() {
            event
        } else {
            tokio::select! {
                event = signal.recv() => event.ok_or_else(|| Error::Transport("signaling closed".into()))?,
                message = incoming_rx.recv() => {
                    if let Some((index, data)) = message {
                        if index == 0 { handle_echo(&channels[0], &data, &echo_seen).await; }
                    }
                    continue;
                }
                _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
            }
        };
        match event {
            Event::Closed(error) => return Err(Error::Transport(error)),
            Event::Text { name, data } | Event::Binary { name, data, .. } if name == "soac" => {
                if let Some((body, client, ice, app)) = soac_body(&data) {
                    if !route_matches(&route, client.as_deref(), ice.as_deref(), app.as_deref()) {
                        continue;
                    }
                    match body.get("type").and_then(Value::as_str) {
                        Some("answer") => {
                            if !answered {
                                let sdp = rtc::answer_sdp(&body).map_err(Error::Protocol)?;
                                rtc::set_answer(&pc, &sdp).await.map_err(Error::Transport)?;
                                answered = true;
                                tracing::info!("native UU answer applied");
                                answer_seen.store(true, Ordering::Release);
                                let early: Vec<Value> = early.lock().unwrap().drain(..).collect();
                                for candidate in early {
                                    let _ = rtc::add_candidate(&pc, &candidate).await;
                                }
                            }
                        }
                        Some("candidate") => {
                            if let Some(candidate) = body.get("candidate") {
                                if answered {
                                    rtc::add_candidate(&pc, candidate)
                                        .await
                                        .map_err(Error::Transport)?;
                                } else {
                                    let mut list = early.lock().unwrap();
                                    if list.len() < 256 {
                                        list.push(candidate.clone());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Text { name, data } | Event::Binary { name, data, .. }
                if (name == "released" || name == "left" || name == "leave")
                    && route_matches_value(&route, &data) =>
            {
                return Err(Error::Transport("remote session released".into()));
            }
            _ => {}
        }
    }

    drop(queued);
    run_terminal(
        channels,
        incoming_rx,
        signal,
        route,
        pc,
        shell,
        requested_session,
    )
    .await
}

fn install_candidates(
    pc: &Arc<RTCPeerConnection>,
    route: Arc<Mutex<Route>>,
    sender: super::signal::Sender,
    early: Arc<Mutex<Vec<Value>>>,
    answered: Arc<AtomicBool>,
) {
    let callback_pc = pc.clone();
    pc.on_ice_candidate(Box::new(move |candidate| {
        let route = route.clone();
        let sender = sender.clone();
        let pc = callback_pc.clone();
        let early = early.clone();
        let answered = answered.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else {
                return;
            };
            let Some(value) = rtc::candidate_json(&candidate) else {
                return;
            };
            let route = route.lock().unwrap().clone();
            sender.soac(json!({
                "client_id": route.client,
                "data": {
                    "type": "candidate",
                    "candidate": value,
                    "ice_id": route.ice,
                    "app_control_id": route.app,
                }
            }));
            if !answered.load(Ordering::Acquire) {
                let mut list = early.lock().unwrap();
                if list.len() < 256 {
                    list.push(value);
                }
            } else {
                let _ = rtc::add_candidate(&pc, &value).await;
            }
        })
    }));
}

fn install_channels(
    channels: &[Arc<webrtc::data_channel::RTCDataChannel>],
    incoming: mpsc::UnboundedSender<(usize, Vec<u8>)>,
    echo_seen: Arc<AtomicBool>,
) {
    for (index, channel) in channels.iter().enumerate() {
        let incoming = incoming.clone();
        let echo_seen = echo_seen.clone();
        let channel_reply = channels[0].clone();
        channel.on_message(Box::new(move |message| {
            let data = message.data.to_vec();
            let _ = incoming.send((index, data.clone()));
            let echo_seen = echo_seen.clone();
            let reply = channel_reply.clone();
            Box::pin(async move {
                if index == 0 {
                    if let Some(seq) = wire::echo_request_seq(&data) {
                        let _ = reply
                            .send(&bytes::Bytes::from(wire::echo_response(
                                1_000_000 + seq,
                                seq,
                            )))
                            .await;
                    }
                    if wire::echo_response_matches(&data, 1) {
                        echo_seen.store(true, Ordering::Release);
                    }
                }
            })
        }));
        if index == 0 {
            let open_channel = channel.clone();
            channel.on_open(Box::new(move || {
                let channel = open_channel.clone();
                Box::pin(async move {
                    let _ = channel
                        .send(&bytes::Bytes::from(wire::echo_request(1)))
                        .await;
                })
            }));
        }
    }
}

async fn handle_echo(
    channel: &Arc<webrtc::data_channel::RTCDataChannel>,
    data: &[u8],
    echo_seen: &AtomicBool,
) {
    if let Some(seq) = wire::echo_request_seq(data) {
        let _ = channel
            .send(&bytes::Bytes::from(wire::echo_response(
                1_000_000 + seq,
                seq,
            )))
            .await;
    }
    if wire::echo_response_matches(data, 1) {
        echo_seen.store(true, Ordering::Release);
    }
}

fn control_ack(data: &Value) -> Result<Value> {
    let value = if let Some(array) = data.as_array() {
        if array.first().and_then(Value::as_str) != Some("success") {
            return Err(Error::Protocol("control request refused".into()));
        }
        array.get(1).cloned().unwrap_or(Value::Null)
    } else {
        data.clone()
    };
    if value.is_object() {
        Ok(value)
    } else {
        Err(Error::Protocol(
            "control acknowledgement is malformed".into(),
        ))
    }
}

type SoacBody = (Value, Option<String>, Option<String>, Option<String>);

fn soac_body(data: &Value) -> Option<SoacBody> {
    let client = data
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let body = data.get("data").cloned().unwrap_or_else(|| data.clone());
    let ice = body
        .get("ice_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let app = body
        .get("app_control_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    body.as_object()?;
    Some((body, client, ice, app))
}

fn route_matches(
    route: &Arc<Mutex<Route>>,
    client: Option<&str>,
    ice: Option<&str>,
    app: Option<&str>,
) -> bool {
    let route = route.lock().unwrap();
    client.is_none_or(|value| value == route.client)
        && ice.is_none_or(|value| value == route.ice)
        && app.is_none_or(|value| value == route.app)
}

fn route_matches_value(route: &Arc<Mutex<Route>>, data: &Value) -> bool {
    if let Some((_, client, ice, app)) = soac_body(data) {
        route_matches(route, client.as_deref(), ice.as_deref(), app.as_deref())
    } else {
        true
    }
}

async fn run_terminal(
    channels: Vec<Arc<webrtc::data_channel::RTCDataChannel>>,
    mut incoming: mpsc::UnboundedReceiver<(usize, Vec<u8>)>,
    mut signal: Signal,
    route: Arc<Mutex<Route>>,
    pc: Arc<RTCPeerConnection>,
    shell: Option<String>,
    requested_session: Option<u32>,
) -> Result<()> {
    let (term_tx, mut term_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let binary = channels[4].clone();
    tokio::spawn(async move {
        while let Some(data) = term_rx.recv().await {
            if binary.send(&bytes::Bytes::from(data)).await.is_err() {
                break;
            }
        }
    });

    let request_id = 20u64;
    let _ = channels[0]
        .send(&bytes::Bytes::from(wire::request(request_id, 28, &[])))
        .await;
    let env_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < env_deadline {
        tokio::select! {
            message = incoming.recv() => {
                if let Some((index, data)) = message {
                    if (index == 0 || index == 4) && rpc_response_id(&data, 24) == Some(request_id) {
                        break;
                    }
                } else { break; }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    let (cols, rows) = terminal_size();
    let mut open = json!({"cols": cols, "rows": rows, "option": "none"});
    if let Some(shell) = shell.as_deref().filter(|s| !s.trim().is_empty()) {
        open["shell"] = Value::String(shell.to_owned());
    }
    let session = requested_session.unwrap_or(0);
    let _ = term_tx.send(term::encode(
        term::OPEN,
        session,
        open.to_string().as_bytes(),
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut session_id = session;
    let mut opened = false;
    let mut unlock_attempts = 0;
    'open_loop: while !opened {
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Transport("terminal open timed out".into()));
        }
        tokio::select! {
            message = incoming.recv() => {
                let Some((index, data)) = message else {
                    return Err(Error::Transport("terminal channel closed".into()));
                };
                if index != 4 { continue; }
                let Some(frame) = term::decode(&data) else { continue; };
                match frame.kind {
                    term::STDOUT => term::ViewerTerminal::write_stdout(&frame.payload),
                    term::OPEN_RESP => {
                        let value: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                        let code = value.get("code").and_then(Value::as_i64).unwrap_or(1);
                        if code == 401 && unlock_attempts < 3 {
                            unlock_attempts += 1;
                            let password = prompt_password()?;
                            let _ = term_tx.send(term::encode(
                                term::UNLOCK_REQ,
                                0,
                                serde_json::json!({"password": password}).to_string().as_bytes(),
                            ));
                            let unlock_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                            while tokio::time::Instant::now() < unlock_deadline {
                                let message = tokio::select! {
                                    message = incoming.recv() => message,
                                    event = signal.recv() => {
                                        if let Some(Event::Closed(error)) = event { return Err(Error::Transport(error)); }
                                        continue;
                                    }
                                    _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                                };
                                let Some((index, data)) = message else { return Err(Error::Transport("terminal channel closed".into())); };
                                if index != 4 { continue; }
                                let Some(frame) = term::decode(&data) else { continue; };
                                if frame.kind != term::UNLOCK_RESP { continue; }
                                let result: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                                let result_code = result.get("code").and_then(Value::as_i64).unwrap_or(1);
                                if result_code == 0 {
                                    let _ = term_tx.send(term::encode(term::OPEN, session_id, open.to_string().as_bytes()));
                                    continue 'open_loop;
                                }
                                break;
                            }
                            let _ = term_tx.send(term::encode(term::OPEN, session_id, open.to_string().as_bytes()));
                            continue 'open_loop;
                        }
                        if code != 0 {
                            let message = value.get("message").and_then(Value::as_str).unwrap_or("open failed");
                            return Err(Error::Session(message.to_owned()));
                        }
                        session_id = if frame.session != 0 {
                            frame.session
                        } else {
                            value
                                .get("session_id")
                                .and_then(|v| {
                                    v.as_u64()
                                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                                })
                                .unwrap_or(0) as u32
                        };
                        opened = true;
                    }
                    term::ERROR => {
                        let value: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                        let message = value.get("message").and_then(Value::as_str).unwrap_or("terminal error");
                        return Err(Error::Session(message.to_owned()));
                    }
                    _ => {}
                }
            }
            event = signal.recv() => {
                match event {
                    Some(Event::Closed(error)) => return Err(Error::Transport(error)),
                    Some(Event::Text { name, data }) | Some(Event::Binary { name, data, .. }) if name == "soac" => {
                        handle_soac(&pc, &route, &data, None, None).await;
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }

    let _raw = term::RawMode::enable();
    term::spawn_stdin(term_tx.clone(), session_id);
    let mut last_size = (cols, rows);
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => {
                let size = terminal_size();
                if size != last_size {
                    last_size = size;
                    let payload = json!({"cols": size.0, "rows": size.1}).to_string();
                    let _ = term_tx.send(term::encode(term::RESIZE, session_id, payload.as_bytes()));
                }
            }
            message = incoming.recv() => {
                let Some((index, data)) = message else { break; };
                if index != 4 { continue; }
                let Some(frame) = term::decode(&data) else { continue; };
                match frame.kind {
                    term::STDOUT => term::ViewerTerminal::write_stdout(&frame.payload),
                    term::EXIT => {
                        let value: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                        let code = value.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
                        term::ViewerTerminal::write_stdout(format!("\r\n[remote exited {code}]\r\n").as_bytes());
                        break;
                    }
                    term::ERROR => {
                        let value: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                        let message = value.get("message").and_then(Value::as_str).unwrap_or("terminal error");
                        term::ViewerTerminal::write_stdout(format!("\r\n[terminal error: {message}]\r\n").as_bytes());
                        break;
                    }
                    _ => {}
                }
            }
            event = signal.recv() => {
                match event {
                    Some(Event::Closed(error)) => return Err(Error::Transport(error)),
                    Some(Event::Text { name, data }) | Some(Event::Binary { name, data, .. }) if name == "soac" => {
                        handle_soac(&pc, &route, &data, None, None).await;
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = term_tx.send(term::encode(term::DETACH, session_id, &[]));
    let _ = pc.close().await;
    Ok(())
}

async fn handle_soac(
    pc: &Arc<RTCPeerConnection>,
    route: &Arc<Mutex<Route>>,
    data: &Value,
    _early: Option<()>,
    _answered: Option<()>,
) {
    if let Some((body, client, ice, app)) = soac_body(data) {
        if !route_matches(route, client.as_deref(), ice.as_deref(), app.as_deref()) {
            return;
        }
        if body.get("type").and_then(Value::as_str) == Some("candidate") {
            if let Some(candidate) = body.get("candidate") {
                let _ = rtc::add_candidate(pc, candidate).await;
            }
        }
    }
}

fn rpc_response_id(data: &[u8], case: u32) -> Option<u64> {
    let fields = wire::decode(data)?;
    let rpc = fields
        .iter()
        .find_map(|field| match (field.number, &field.value) {
            (22, wire::Value::Bytes(bytes)) => Some(bytes.as_slice()),
            _ => None,
        })?;
    let rpc = wire::decode(rpc)?;
    wire::blob(&rpc, case)?;
    let header = wire::blob(&rpc, 1)?;
    wire::uint(&wire::decode(header)?, 1)
}

fn prompt_password() -> Result<String> {
    use std::io::Write;
    eprint!("device password: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(Error::Io)?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err(Error::Session("empty password".into()));
    }
    Ok(password)
}

fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_ack_requires_success() {
        assert!(control_ack(&json!(["failure", {}])).is_err());
        assert_eq!(
            control_ack(&json!(["success", {"client_id": "c"}])).unwrap()["client_id"],
            "c"
        );
    }
}
