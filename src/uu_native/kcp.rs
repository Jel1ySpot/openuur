//! DC-KCP framing used by UU when `a=x-uuremote-mix-kcp` is negotiated.
//!
//! KCP itself is a reliable ARQ layer over the already reliable DataChannel,
//! matching UU's optional low-latency path. The outer application frame is
//! `[sid:uint16 LE][kind:uint16 LE][payload]`; the conversation id and default
//! parameters are the values recovered from `streamer.dll`.
//!
//! This module is transport-only: the caller decides when the official peer
//! has negotiated KCP. SCTP remains the default fallback.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

pub const CONVERSATION: u32 = 0x23356;
pub const MTU: usize = 1191;
pub const SDP_ATTRIBUTE: &str = "a=x-uuremote-mix-kcp";

struct PacketWriter {
    tx: mpsc::Sender<Vec<u8>>,
}

impl Write for PacketWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let _ = self.tx.try_send(buffer.to_vec());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct KcpTransport {
    inner: Arc<Mutex<kcp::Kcp<PacketWriter>>>,
}

impl KcpTransport {
    /// `output` receives KCP packets that must be sent on the DataChannel.
    pub fn new(output: mpsc::Sender<Vec<u8>>) -> Self {
        let mut kcp = kcp::Kcp::new(CONVERSATION, PacketWriter { tx: output });
        kcp.set_nodelay(true, 5, 2, true);
        kcp.set_wndsize(256, 256);
        let _ = kcp.set_mtu(MTU);
        Self {
            inner: Arc::new(Mutex::new(kcp)),
        }
    }

    pub fn input(&self, packet: &[u8]) -> Result<usize, String> {
        self.inner
            .lock()
            .map_err(|_| "KCP state poisoned".to_owned())?
            .input(packet)
            .map_err(|error| error.to_string())
    }

    pub fn send(&self, payload: &[u8]) -> Result<usize, String> {
        self.inner
            .lock()
            .map_err(|_| "KCP state poisoned".to_owned())?
            .send(payload)
            .map_err(|error| error.to_string())
    }

    pub fn recv(&self) -> Result<Option<Vec<u8>>, String> {
        let mut kcp = self
            .inner
            .lock()
            .map_err(|_| "KCP state poisoned".to_owned())?;
        let size = kcp.peeksize().map_err(|error| error.to_string())?;
        if size == 0 {
            return Ok(None);
        }
        let mut buffer = vec![0u8; size];
        let read = kcp.recv(&mut buffer).map_err(|error| error.to_string())?;
        buffer.truncate(read);
        Ok(Some(buffer))
    }

    pub fn update(&self) -> Result<(), String> {
        self.inner
            .lock()
            .map_err(|_| "KCP state poisoned".to_owned())?
            .update(now_ms() as u32)
            .map_err(|error| error.to_string())
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Application frame for one KCP logical stream.
pub fn frame(sid: u16, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&sid.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse_frame(data: &[u8]) -> Option<(u16, u16, &[u8])> {
    if data.len() < 4 {
        return None;
    }
    Some((
        u16::from_le_bytes([data[0], data[1]]),
        u16::from_le_bytes([data[2], data[3]]),
        &data[4..],
    ))
}

pub fn negotiated(sdp: &str) -> bool {
    sdp.lines()
        .any(|line| line.trim_end().starts_with(SDP_ATTRIBUTE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_logical_stream_identity() {
        let encoded = frame(7, 2, b"hello");
        let (sid, kind, payload) = parse_frame(&encoded).unwrap();
        assert_eq!(sid, 7);
        assert_eq!(kind, 2);
        assert_eq!(payload, b"hello");
        assert!(parse_frame(&[0, 1]).is_none());
    }

    #[test]
    fn negotiation_requires_the_private_sdp_attribute() {
        assert!(negotiated("a=x-uuremote-mix-kcp\r\n"));
        assert!(!negotiated("a=x-uuremote-quic\r\n"));
    }
}
