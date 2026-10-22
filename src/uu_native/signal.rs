//! Socket.IO v4 / Engine.IO v4 signaling used by the official UU protocol.

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

const STREAMER_VERSION: &str = "V4.9.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone)]
pub enum Event {
    Connected,
    Text {
        name: String,
        data: Value,
    },
    Binary {
        name: String,
        data: Value,
        attachments: Vec<Vec<u8>>,
    },
    Ack {
        id: u64,
        data: Value,
    },
    Closed(String),
}

#[derive(Clone)]
pub struct Sender {
    tx: mpsc::UnboundedSender<Message>,
}

impl Sender {
    pub fn text(&self, text: String) {
        let _ = self.tx.send(Message::Text(text.into()));
    }

    pub fn binary(&self, data: Vec<u8>) {
        let _ = self.tx.send(Message::Binary(data.into()));
    }

    pub fn event(&self, name: &str, payload: Value) {
        self.text(format!("42{}", serde_json::json!([name, payload])));
    }

    pub fn soac(&self, payload: Value) {
        self.event("soac", payload);
    }

    pub fn control(&self, ack_id: u64, header: Value, attachment: Vec<u8>) {
        self.text(format!(
            "451-{}{}",
            ack_id,
            serde_json::json!(["control", header])
        ));
        self.binary(attachment);
    }
}

/// Reconnect-aware outbound signaling. Peers keep one handle for their whole
/// life; a room reconnect swaps the underlying `Sender` instead of tearing
/// WebRTC peers down. While no room is connected, sends are dropped.
#[derive(Clone)]
pub struct Outbox {
    inner: std::sync::Arc<std::sync::Mutex<Option<Sender>>>,
    connected: tokio::sync::watch::Sender<bool>,
}

impl Outbox {
    pub fn new() -> Self {
        let (connected, _) = tokio::sync::watch::channel(false);
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(None)),
            connected,
        }
    }

    pub fn attach(&self, sender: Sender) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(sender);
        }
        self.connected.send_replace(true);
    }

    pub fn detach(&self) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = None;
        }
        self.connected.send_replace(false);
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    pub fn state(&self) -> tokio::sync::watch::Receiver<bool> {
        self.connected.subscribe()
    }

    fn with(&self, send: impl FnOnce(&Sender)) {
        let sender = self.inner.lock().ok().and_then(|slot| slot.clone());
        if let Some(sender) = sender {
            send(&sender);
        }
    }

    pub fn text(&self, text: String) {
        self.with(|sender| sender.text(text));
    }

    pub fn binary(&self, data: Vec<u8>) {
        self.with(|sender| sender.binary(data));
    }

    pub fn event(&self, name: &str, payload: Value) {
        self.with(|sender| sender.event(name, payload));
    }

    pub fn soac(&self, payload: Value) {
        self.with(|sender| sender.soac(payload));
    }

    pub fn control(&self, ack_id: u64, header: Value, attachment: Vec<u8>) {
        self.with(|sender| sender.control(ack_id, header, attachment));
    }
}

impl Default for Outbox {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Signal {
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::Receiver<Event>,
    writer: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Signal {
    fn drop(&mut self) {
        // A half-open socket must not keep a writer or reader task alive after
        // the room is replaced.
        self.writer.abort();
        self.reader.abort();
    }
}

impl Signal {
    pub async fn connect(
        servers: &[String],
        room_token: &str,
        controlling: bool,
    ) -> Result<Self, String> {
        let mut last = String::from("no signaling server");
        for server in servers {
            match Self::connect_one(server, room_token, controlling).await {
                Ok(signal) => return Ok(signal),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    async fn connect_one(
        server: &str,
        room_token: &str,
        controlling: bool,
    ) -> Result<Self, String> {
        let mut url =
            url::Url::parse(server).map_err(|e| format!("invalid signaling server: {e}"))?;
        if url.path().is_empty() || url.path() == "/" {
            url.set_path("/socket.io/");
        }
        url.query_pairs_mut()
            .append_pair("EIO", "4")
            .append_pair("transport", "websocket")
            .append_pair("t", &crate::util::now_ms().to_string());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("invalid signaling request: {e}"))?;
        {
            let headers = request.headers_mut();
            headers.insert(
                "X-NRD-AUTH",
                HeaderValue::from_str(room_token).map_err(|_| "invalid room token header")?,
            );
            if controlling {
                headers.insert("X-NRD-CONTROLLING", HeaderValue::from_static("1"));
            }
            headers.insert(
                "streamer_version",
                HeaderValue::from_static(STREAMER_VERSION),
            );
            headers.insert("streamer_flag", HeaderValue::from_static("1"));
            headers.insert(
                "User-Agent",
                HeaderValue::from_str(&format!("openuur/{}", crate::VERSION))
                    .map_err(|_| "invalid user agent")?,
            );
        }
        let (mut ws, _) =
            tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| "signaling connect timed out".to_owned())?
                .map_err(|e| format!("signaling connect failed: {e}"))?;

        let mut opened = false;
        // Engine.IO open packet: `0{"pingInterval":25000,"pingTimeout":20000}`.
        // The server closes the socket when a ping is missed for longer than
        // both values; use it to detect half-open TCP after suspend/NAT loss.
        let mut ping_window = Duration::from_secs(45);
        for _ in 0..12 {
            let message = tokio::time::timeout(CONNECT_TIMEOUT, ws.next())
                .await
                .map_err(|_| "signaling handshake timed out".to_owned())?
                .ok_or_else(|| "signaling closed during handshake".to_owned())?
                .map_err(|e| format!("signaling handshake failed: {e}"))?;
            match message {
                Message::Text(text) if text.starts_with('0') => {
                    opened = true;
                    if let Ok(open) = serde_json::from_str::<Value>(&text[1..]) {
                        if let Some(window) = heartbeat_window(&open) {
                            ping_window = window;
                        }
                    }
                    ws.send(Message::Text("40".into()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Message::Text(text) if text.starts_with("40") && opened => break,
                Message::Text(text) if text == "2" => {
                    ws.send(Message::Text("3".into()))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Message::Text(text) if text.starts_with("44") => {
                    return Err(format!(
                        "signaling rejected: {}",
                        text.chars().take(80).collect::<String>()
                    ));
                }
                Message::Ping(payload) => {
                    ws.send(Message::Pong(payload))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                _ => {}
            }
        }
        if !opened {
            return Err("signaling did not open Engine.IO".to_owned());
        }

        let (mut writer, mut reader) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::channel::<Event>(256);
        let writer_task = tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
        });
        let tx = out_tx.clone();
        let reader_task = tokio::spawn(async move {
            let mut pending: Option<(String, Value, usize, Vec<Vec<u8>>)> = None;
            let heartbeat = ping_window + Duration::from_secs(5);
            let mut deadline = tokio::time::Instant::now() + heartbeat;
            // The official controlled session refreshes its reconnect key every
            // 30s (first refresh 30s after the socket.io namespace connects).
            // Without it the server closes the namespace after about 30s, which
            // used to tear down every active peer.
            let mut next_refresh = tokio::time::Instant::now() + Duration::from_secs(30);
            let mut refresh_id: u64 = 1000;
            loop {
                let message = tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        let _ = event_tx
                            .send(Event::Closed("signaling heartbeat timed out".into()))
                            .await;
                        break;
                    }
                    _ = tokio::time::sleep_until(next_refresh) => {
                        refresh_id += 1;
                        let packet = format!("42{refresh_id}[\"refresh_reconnect_key\"]");
                        if out_tx.send(Message::Text(packet.into())).is_err() {
                            break;
                        }
                        tracing::debug!("signaling reconnect-key refresh id={refresh_id}");
                        next_refresh =
                            tokio::time::Instant::now() + Duration::from_secs(30);
                        continue;
                    }
                    message = reader.next() => message,
                };
                deadline = tokio::time::Instant::now() + heartbeat;
                let Some(message) = message else { break };
                let message = match message {
                    Ok(message) => message,
                    Err(_) => {
                        let _ = event_tx
                            .send(Event::Closed("signaling read failed".into()))
                            .await;
                        break;
                    }
                };
                match message {
                    Message::Text(text) if text == "2" => {
                        let _ = out_tx.send(Message::Text("3".into()));
                    }
                    Message::Text(text) if text.starts_with("41") || text == "1" => {
                        let _ = event_tx
                            .send(Event::Closed("signaling room closed".into()))
                            .await;
                        break;
                    }
                    Message::Text(text) if text.starts_with("42") => {
                        if let Some((name, data)) = parse_event(&text[2..]) {
                            let _ = event_tx.send(Event::Text { name, data }).await;
                        }
                    }
                    Message::Text(text) if text.starts_with("43") => {
                        if let Some((id, data)) = parse_ack(&text[2..]) {
                            let _ = event_tx.send(Event::Ack { id, data }).await;
                        }
                    }
                    Message::Text(text) if text.starts_with("45") || text.starts_with("46") => {
                        if let Some((is_ack, id, count, name, data)) = parse_binary_header(&text) {
                            if count == 0 {
                                if is_ack {
                                    if let Some(id) = id {
                                        let _ = event_tx.send(Event::Ack { id, data }).await;
                                    }
                                } else {
                                    let event = Event::Binary {
                                        name,
                                        data,
                                        attachments: Vec::new(),
                                    };
                                    let _ = event_tx.send(event).await;
                                }
                            } else {
                                pending = Some((name, data, count, Vec::new()));
                                let _ = is_ack;
                                let _ = id;
                            }
                        }
                    }
                    Message::Binary(data) => {
                        if let Some((name, header, count, mut attachments)) = pending.take() {
                            let mut data = data.to_vec();
                            if data.len() > 2 && data[0] == 4 && data[1] == 0x1f {
                                data.remove(0);
                            }
                            attachments.push(data);
                            if attachments.len() >= count {
                                let data = fill_placeholders(header, &attachments);
                                let _ = event_tx
                                    .send(Event::Binary {
                                        name,
                                        data,
                                        attachments,
                                    })
                                    .await;
                            } else {
                                pending = Some((name, header, count, attachments));
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = out_tx.send(Message::Pong(payload));
                    }
                    Message::Close(_) => {
                        let _ = event_tx
                            .send(Event::Closed("signaling closed".into()))
                            .await;
                        break;
                    }
                    _ => {}
                }
            }
        });
        Ok(Self {
            tx,
            rx: event_rx,
            writer: writer_task,
            reader: reader_task,
        })
    }

    pub fn sender(&self) -> Sender {
        Sender {
            tx: self.tx.clone(),
        }
    }

    pub fn send_text(&self, text: String) {
        self.sender().text(text);
    }

    pub fn send_binary(&self, data: Vec<u8>) {
        self.sender().binary(data);
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    pub fn room_info(&self) {
        self.send_text("4211[\"room_info\"]".to_owned());
    }

    pub fn soac(&self, payload: Value) {
        self.send_text(format!("42{}", serde_json::json!(["soac", payload])));
    }

    /// Send `["control", header]` as a Socket.IO binary event with one attachment.
    pub fn control(&self, ack_id: u64, header: Value, attachment: Vec<u8>) {
        let packet = format!("451-{}{}", ack_id, serde_json::json!(["control", header]));
        self.send_text(packet);
        self.send_binary(attachment);
    }
}

/// Socket.IO ping budget from the Engine.IO open packet, clamped to a sane
/// range so a broken server cannot disable or shorten liveness detection.
fn heartbeat_window(open: &Value) -> Option<Duration> {
    let interval = open.get("pingInterval").and_then(Value::as_u64)?;
    let timeout = open.get("pingTimeout").and_then(Value::as_u64)?;
    Some(
        Duration::from_millis(interval.saturating_add(timeout))
            .clamp(Duration::from_secs(5), Duration::from_secs(300)),
    )
}

fn parse_event(body: &str) -> Option<(String, Value)> {
    let value: Value = serde_json::from_str(body).ok()?;
    let array = value.as_array()?;
    Some((
        array.first()?.as_str()?.to_owned(),
        array.get(1).cloned().unwrap_or(Value::Null),
    ))
}

fn parse_ack(body: &str) -> Option<(u64, Value)> {
    let begin = body.find('[')?;
    let id = body[..begin].trim().parse().ok()?;
    let value = serde_json::from_str(&body[begin..]).ok()?;
    Some((id, value))
}

fn parse_binary_header(text: &str) -> Option<(bool, Option<u64>, usize, String, Value)> {
    let is_ack = text.starts_with("46");
    let dash = text.find('-')?;
    let count: usize = text[2..dash].parse().ok()?;
    if count > 8 {
        return None;
    }
    let begin = text.find('[')?;
    let value: Value = serde_json::from_str(&text[begin..]).ok()?;
    let array = value.as_array()?;
    let name = array.first()?.as_str()?.to_owned();
    let data = array.get(1).cloned().unwrap_or(Value::Null);
    let id = if is_ack {
        text[dash + 1..begin].trim().parse().ok()
    } else {
        None
    };
    Some((is_ack, id, count, name, data))
}

fn fill_placeholders(value: Value, attachments: &[Vec<u8>]) -> Value {
    match value {
        Value::Object(map) => {
            if map.get("_placeholder").and_then(Value::as_bool) == Some(true) {
                if let Some(index) = map.get("num").and_then(Value::as_u64) {
                    if let Some(bytes) = attachments.get(index as usize) {
                        return Value::String(
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        );
                    }
                }
            }
            Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, fill_placeholders(v, attachments)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| fill_placeholders(v, attachments))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_is_replaced_with_base64_attachment() {
        let header = serde_json::json!({"data":{"_placeholder":true,"num":0}});
        let value = fill_placeholders(header, &[b"abc".to_vec()]);
        assert_eq!(value["data"], "YWJj");
    }

    #[test]
    fn heartbeat_window_follows_the_open_packet_and_is_clamped() {
        assert_eq!(
            heartbeat_window(&serde_json::json!({"pingInterval":25000,"pingTimeout":20000})),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            heartbeat_window(&serde_json::json!({"pingInterval":100,"pingTimeout":100})),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            heartbeat_window(&serde_json::json!({"pingInterval":200000,"pingTimeout":200000})),
            Some(Duration::from_secs(300))
        );
        assert_eq!(heartbeat_window(&serde_json::json!({})), None);
    }

    #[test]
    fn outbox_forwards_only_while_a_room_is_attached() {
        let outbox = Outbox::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(!outbox.is_connected());
        outbox.attach(Sender { tx });
        assert!(outbox.is_connected());
        outbox.soac(serde_json::json!({"type":"candidate"}));
        let message = rx.try_recv().expect("attached outbox forwards");
        match message {
            Message::Text(text) => assert!(text.starts_with("42[\"soac\"")),
            other => panic!("unexpected message: {other:?}"),
        }
        outbox.detach();
        assert!(!outbox.is_connected());
        outbox.soac(serde_json::json!({"type":"candidate"}));
        assert!(rx.try_recv().is_err(), "detached outbox drops sends");
    }
}
