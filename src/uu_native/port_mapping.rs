//! UU port mapping (controlled side).
//!
//! The viewer creates rules; every local connection is a stream identified by
//! `PortMappingFrame {session_id, rule_id, stream_id}`. SYN payload is JSON
//! with `target_host`/`target_port`; DATA carries raw TCP bytes. Frames are
//! accepted only on `FILE_DATA_CHANNEL`.

use super::wire;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

type StreamWriter = (
    mpsc::Sender<Vec<u8>>,
    tokio::task::AbortHandle,
    tokio::task::AbortHandle,
);

const MESSAGE_FIELD: u32 = 27;
const SYN: u64 = 0;
const FIN: u64 = 1;
const DATA: u64 = 2;
const SYN_ACK: u64 = 3;
const PROTOCOL_VERSION: u64 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_CHUNK: usize = 16 * 1024;
/// Frames buffered per direction before the stream is failed. Neither queue
/// is awaited on the data-channel callback: it is shared with file transfer
/// and its SCTP association also carries desktop input and terminal traffic.
const WRITE_QUEUE_FRAMES: usize = 256;

pub fn enabled() -> bool {
    std::env::var("UU_PORT_MAPPING").map_or(true, |value| value != "0")
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Id {
    Varint(u64),
    Bytes(Vec<u8>),
    Missing,
}

impl Id {
    fn of(fields: &[wire::Field], number: u32) -> Self {
        match fields
            .iter()
            .find(|field| field.number == number)
            .map(|field| &field.value)
        {
            Some(wire::Value::Varint(value)) => Self::Varint(*value),
            Some(wire::Value::Bytes(value)) => Self::Bytes(value.clone()),
            _ => Self::Missing,
        }
    }

    fn encode(&self, number: u32, out: &mut Vec<u8>) {
        match self {
            Self::Varint(value) => wire::int(number, *value, out),
            Self::Bytes(value) => wire::bytes(number, value, out),
            Self::Missing => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    session: Id,
    rule: Id,
    stream: Id,
}

fn frame(key: &Key, kind: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    key.session.encode(1, &mut out);
    key.rule.encode(2, &mut out);
    key.stream.encode(3, &mut out);
    if kind != 0 {
        wire::int(4, kind, &mut out);
    }
    if !payload.is_empty() {
        wire::bytes(5, payload, &mut out);
    }
    out
}

fn parse_syn(payload: &[u8]) -> Result<(String, u16), String> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| "invalid SYN payload".to_owned())?;
    let host = value
        .get("target_host")
        .or_else(|| value.get("targetHost"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "missing target host".to_owned())?;
    let port = value
        .get("target_port")
        .or_else(|| value.get("targetPort"))
        .and_then(|value| value.as_u64())
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| "invalid target port".to_owned())?;
    Ok((host.to_owned(), port))
}

fn syn_ack(ok: bool, error: Option<&str>) -> Vec<u8> {
    let mut value = serde_json::json!({"version": PROTOCOL_VERSION, "ok": ok});
    if let Some(error) = error {
        value["error"] = serde_json::Value::from(error);
    }
    value.to_string().into_bytes()
}

pub struct PortMapping {
    outbound: mpsc::Sender<Vec<u8>>,
    streams: Mutex<HashMap<Key, StreamWriter>>,
    sequence: Arc<AtomicU64>,
}

impl PortMapping {
    /// `outbound` sends a fully framed `Message` to `FILE_DATA_CHANNEL`.
    pub fn new(outbound: mpsc::Sender<Vec<u8>>, sequence: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            outbound,
            streams: Mutex::new(HashMap::new()),
            sequence,
        })
    }

    /// Handles one complete DataChannel message. Returns true when it was a
    /// port-mapping message. Queues never await: a full queue fails the
    /// mapping stream cleanly (FIN + target close) rather than blocking the
    /// shared data channel or dropping bytes mid-stream.
    pub async fn handle(self: &Arc<Self>, data: &[u8], channel: &str) -> bool {
        let Some(fields) = wire::decode(data) else {
            return false;
        };
        let Some(raw) = wire::blob(&fields, MESSAGE_FIELD) else {
            return false;
        };
        if channel != "FILE_DATA_CHANNEL" {
            tracing::warn!("port mapping frame ignored on {channel}");
            return true;
        }
        let Some(frame) = wire::decode(raw) else {
            return true;
        };
        let key = Key {
            session: Id::of(&frame, 1),
            rule: Id::of(&frame, 2),
            stream: Id::of(&frame, 3),
        };
        let kind = wire::uint(&frame, 4).unwrap_or(SYN);
        let payload = wire::blob(&frame, 5).unwrap_or_default();
        match kind {
            SYN => self.syn(key, payload).await,
            DATA => {
                let writer = self
                    .streams
                    .lock()
                    .unwrap()
                    .get(&key)
                    .map(|stream| stream.0.clone());
                match writer {
                    Some(writer) => {
                        // Never await here: this runs on the shared file data
                        // channel, and blocking it would also stall the SCTP
                        // association (desktop input and terminal). Overflow
                        // fails the mapping cleanly instead of dropping bytes
                        // in the middle of the relayed TCP stream.
                        if writer.try_send(payload.to_vec()).is_err() {
                            tracing::warn!(
                                "port mapping target is not draining; closing the stream"
                            );
                            self.close(&key, true);
                        }
                    }
                    None => {
                        self.send(key, FIN, Vec::new());
                    }
                }
            }
            FIN => self.close(&key, false),
            _ => {}
        }
        true
    }

    /// Queue one outbound frame without awaiting. Returns false when the
    /// viewer channel could not accept it, so the caller can fail the mapping
    /// instead of leaving a silently truncated TCP stream.
    fn send(&self, key: Key, kind: u64, payload: Vec<u8>) -> bool {
        let message = wire::message(
            self.sequence.fetch_add(1, Ordering::Relaxed),
            MESSAGE_FIELD,
            &frame(&key, kind, &payload),
        );
        if self.outbound.try_send(message).is_err() {
            tracing::warn!("port mapping outbound queue is full");
            return false;
        }
        true
    }

    async fn syn(self: &Arc<Self>, key: Key, payload: &[u8]) {
        let request = if enabled() {
            parse_syn(payload)
        } else {
            Err("port mapping disabled on this host".to_owned())
        };
        let (host, port) = match request {
            Ok(target) => target,
            Err(error) => {
                let _ = self.send(key, SYN_ACK, syn_ack(false, Some(&error)));
                return;
            }
        };
        let mapping = self.clone();
        tokio::spawn(async move {
            let connected = tokio::time::timeout(
                CONNECT_TIMEOUT,
                tokio::net::TcpStream::connect((host.as_str(), port)),
            )
            .await;
            let stream = match connected {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    let _ = mapping.send(
                        key,
                        SYN_ACK,
                        syn_ack(false, Some(&format!("connect failed: {}", error.kind()))),
                    );
                    return;
                }
                Err(_) => {
                    let _ = mapping.send(key, SYN_ACK, syn_ack(false, Some("connect timeout")));
                    return;
                }
            };
            let _ = stream.set_nodelay(true);
            let (mut read, mut write) = stream.into_split();
            let (writer, mut writes) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE_FRAMES);
            let _ = mapping.send(key.clone(), SYN_ACK, syn_ack(true, None));
            let write_task = tokio::spawn(async move {
                while let Some(data) = writes.recv().await {
                    if write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                let _ = write.shutdown().await;
            });
            let reader_mapping = mapping.clone();
            let reader_key = key.clone();
            let (registered, wait_registered) = tokio::sync::oneshot::channel::<()>();
            let reader = tokio::spawn(async move {
                if wait_registered.await.is_err() {
                    return;
                }
                let mut buffer = vec![0u8; READ_CHUNK];
                loop {
                    match read.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(length) => {
                            let payload = buffer[..length].to_vec();
                            if !reader_mapping.send(reader_key.clone(), DATA, payload) {
                                tracing::warn!(
                                    "port mapping viewer channel is not draining; closing the mapping"
                                );
                                if reader_mapping.finish_local(&reader_key) {
                                    let _ =
                                        reader_mapping.send(reader_key.clone(), FIN, Vec::new());
                                }
                                break;
                            }
                        }
                    }
                }
                reader_mapping.finish_reader(reader_key);
            });
            mapping.streams.lock().unwrap().insert(
                key,
                (writer, reader.abort_handle(), write_task.abort_handle()),
            );
            let _ = registered.send(());
        });
    }

    /// The target reached EOF. Remove the stream without aborting this
    /// reader (aborting from inside its own task would cancel the FIN send),
    /// then tell the viewer once the FIN is queued.
    fn finish_reader(&self, key: Key) {
        let removed = self.streams.lock().unwrap().remove(&key);
        if let Some((writer, _reader, write_task)) = removed {
            write_task.abort();
            drop(writer);
            let _ = self.send(key, FIN, Vec::new());
        }
    }

    /// Remove the stream from its own reader task after the viewer channel
    /// stalled: stop the write task and close the target socket, without
    /// aborting the current task.
    fn finish_local(&self, key: &Key) -> bool {
        match self.streams.lock().unwrap().remove(key) {
            Some((writer, _reader, write_task)) => {
                write_task.abort();
                drop(writer);
                true
            }
            None => false,
        }
    }

    fn close(&self, key: &Key, send_fin: bool) {
        let removed = self.streams.lock().unwrap().remove(key);
        if let Some((writer, reader, write_task)) = removed {
            reader.abort();
            write_task.abort();
            drop(writer);
            if send_fin {
                let _ = self.send(key.clone(), FIN, Vec::new());
            }
        }
    }

    pub fn shutdown(&self) {
        let streams: Vec<_> = self.streams.lock().unwrap().drain().collect();
        for (_, (writer, reader, write_task)) in streams {
            reader.abort();
            write_task.abort();
            drop(writer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_syn_keys() {
        assert_eq!(
            parse_syn(br#"{"target_host":"127.0.0.1","target_port":22}"#).unwrap(),
            ("127.0.0.1".to_owned(), 22)
        );
        assert!(parse_syn(br#"{"target_host":"","target_port":22}"#).is_err());
        assert!(parse_syn(br#"{"target_port":22}"#).is_err());
    }

    #[test]
    fn frames_keep_identity_types() {
        let key = Key {
            session: Id::Bytes(b"s1".to_vec()),
            rule: Id::Varint(7),
            stream: Id::Varint(300),
        };
        let fields = wire::decode(&frame(&key, DATA, b"abc")).unwrap();
        assert_eq!(Id::of(&fields, 1), Id::Bytes(b"s1".to_vec()));
        assert_eq!(Id::of(&fields, 2), Id::Varint(7));
        assert_eq!(Id::of(&fields, 3), Id::Varint(300));
    }

    #[tokio::test]
    async fn finish_reader_removes_the_stream_and_queues_fin() {
        let (outbound, mut queued) = mpsc::channel(4);
        let mapping = PortMapping::new(outbound, Arc::new(AtomicU64::new(1)));
        let key = Key {
            session: Id::Varint(1),
            rule: Id::Varint(2),
            stream: Id::Varint(3),
        };
        let (writer, _writes) = mpsc::channel(1);
        let reader = tokio::spawn(std::future::pending::<()>()).abort_handle();
        let write_task = tokio::spawn(std::future::pending::<()>()).abort_handle();
        mapping
            .streams
            .lock()
            .unwrap()
            .insert(key.clone(), (writer, reader, write_task));
        mapping.finish_reader(key.clone());
        assert!(mapping.streams.lock().unwrap().is_empty());
        let message = queued.recv().await.expect("FIN should be queued");
        let outer = wire::decode(&message).unwrap();
        let raw = wire::blob(&outer, MESSAGE_FIELD).unwrap();
        let fields = wire::decode(raw).unwrap();
        assert_eq!(wire::uint(&fields, 4), Some(FIN));
    }
}
