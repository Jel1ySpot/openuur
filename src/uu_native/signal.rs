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

pub struct Signal {
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::Receiver<Event>,
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
        for _ in 0..12 {
            let message = tokio::time::timeout(CONNECT_TIMEOUT, ws.next())
                .await
                .map_err(|_| "signaling handshake timed out".to_owned())?
                .ok_or_else(|| "signaling closed during handshake".to_owned())?
                .map_err(|e| format!("signaling handshake failed: {e}"))?;
            match message {
                Message::Text(text) if text.starts_with('0') => {
                    opened = true;
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
        tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
        });
        let tx = out_tx.clone();
        tokio::spawn(async move {
            let mut pending: Option<(String, Value, usize, Vec<Vec<u8>>)> = None;
            // The official controlled session refreshes its reconnect key every
            // 30s (first refresh 30s after the socket.io namespace connects).
            // Without it the server closes the namespace after about 30s, which
            // used to tear down every active peer.
            let mut next_refresh = tokio::time::Instant::now() + Duration::from_secs(30);
            let mut refresh_id: u64 = 1000;
            loop {
                let message = tokio::select! {
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
        Ok(Self { tx, rx: event_rx })
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
}
