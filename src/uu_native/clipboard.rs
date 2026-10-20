//! UU text clipboard protocol.
//!
//! The official protocol negotiates a format list, asks for a format by key,
//! transfers numbered data blocks and confirms each block. Only UTF-16
//! `CF_UNICODETEXT` and the Mac UTF-8 pasteboard type are supported; file and
//! image formats are deliberately not advertised.

use super::wire::{self, Field};
use std::collections::BTreeMap;

pub const MAX_TEXT: usize = 30 * 1024;
const UNICODE_TEXT: u64 = 13;
const MAC_TEXT: &str = "public.utf8-plain-text";
const BLOCK_BYTES: usize = 0x20000;
const MAX_BYTES: usize = MAX_TEXT * 2 + 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextFormat {
    Windows,
    Mac,
}

impl TextFormat {
    fn of(format: &[u8]) -> Option<Self> {
        let fields = wire::decode(format)?;
        if wire::uint(&fields, 1) == Some(UNICODE_TEXT) {
            return Some(Self::Windows);
        }
        let named =
            |number| wire::blob(&fields, number).and_then(|value| std::str::from_utf8(value).ok());
        (named(2) == Some(MAC_TEXT) || named(3) == Some(MAC_TEXT)).then_some(Self::Mac)
    }

    fn id(self) -> u64 {
        match self {
            Self::Windows => UNICODE_TEXT,
            Self::Mac => 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Windows => "CF_UNICODETEXT",
            Self::Mac => MAC_TEXT,
        }
    }

    fn encode(self, text: &str) -> Vec<u8> {
        match self {
            Self::Windows => encode_windows(text),
            Self::Mac => text.as_bytes().to_vec(),
        }
    }

    fn decode(self, data: &[u8]) -> Result<String, String> {
        match self {
            Self::Windows => decode_windows(data),
            Self::Mac => {
                let data = data.strip_suffix(&[0]).unwrap_or(data);
                if data.len() > MAX_TEXT {
                    return Err("clipboard text exceeds limit".to_owned());
                }
                let text = std::str::from_utf8(data)
                    .map_err(|_| "invalid UTF-8 clipboard text".to_owned())?;
                if text.contains('\0') {
                    return Err("embedded NUL in clipboard text".to_owned());
                }
                Ok(text.to_owned())
            }
        }
    }
}

struct Pending {
    key: String,
    format: TextFormat,
    count: Option<usize>,
    blocks: BTreeMap<usize, Vec<u8>>,
}

#[derive(Default)]
pub struct ClipboardSession {
    local: Option<String>,
    pending: Option<Pending>,
}

impl ClipboardSession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_local(&mut self, text: Option<String>) {
        self.local = text.filter(|text| text.len() <= MAX_TEXT && !text.contains('\0'));
    }

    pub fn offer(&mut self, id: u64, text: String) -> Result<Vec<u8>, String> {
        if text.len() > MAX_TEXT || text.contains('\0') {
            return Err("clipboard text exceeds limit".to_owned());
        }
        self.local = Some(text);
        let mut list = Vec::new();
        for format in [TextFormat::Windows, TextFormat::Mac] {
            let mut item = Vec::new();
            wire::int(1, format.id(), &mut item);
            wire::bytes(2, format.name().as_bytes(), &mut item);
            wire::bytes(1, &item, &mut list);
        }
        Ok(request(id, 1, &list))
    }

    /// Handles `Message.rpc_request` field 21. Returns full outbound
    /// `Message` payloads and newly completed text.
    pub fn handle_request(
        &mut self,
        rpc: &[Field],
        mut next_id: impl FnMut() -> u64,
    ) -> Result<(Vec<Vec<u8>>, Option<String>), String> {
        let Some(clip) = wire::blob(rpc, 9) else {
            return Ok((Vec::new(), None));
        };
        let clip = wire::decode(clip).ok_or("invalid clipboard request")?;
        let header = wire::blob(rpc, 1).ok_or("missing clipboard header")?;
        let mut out = Vec::new();
        if let Some(list) = wire::blob(&clip, 1) {
            out.push(response(header, 1, &[]));
            let fields = wire::decode(list).ok_or("invalid clipboard format list")?;
            let offered: Vec<TextFormat> = fields
                .iter()
                .filter(|field| field.number == 1)
                .filter_map(|field| match &field.value {
                    wire::Value::Bytes(format) => TextFormat::of(format),
                    _ => None,
                })
                .collect();
            let chosen = if offered.contains(&TextFormat::Windows) {
                Some(TextFormat::Windows)
            } else {
                offered.first().copied()
            };
            self.pending = None;
            if let Some(format) = chosen {
                let id = next_id();
                let key = format!(
                    "{:08x}-0000-4000-8000-{:012x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as u32,
                    id & 0xffff_ffff_ffff
                );
                let mut ask = Vec::new();
                wire::int(1, format.id(), &mut ask);
                wire::bytes(2, key.as_bytes(), &mut ask);
                wire::bytes(3, format.name().as_bytes(), &mut ask);
                self.pending = Some(Pending {
                    key,
                    format,
                    count: None,
                    blocks: BTreeMap::new(),
                });
                out.push(request(id, 2, &ask));
            }
        } else if let Some(ask) = wire::blob(&clip, 2) {
            let ask = wire::decode(ask).ok_or("invalid clipboard ask")?;
            let key = wire::blob(&ask, 2)
                .and_then(|value| std::str::from_utf8(value).ok())
                .ok_or("invalid clipboard block key")?;
            if key.is_empty() || key.len() > 256 {
                return Err("invalid clipboard block key".to_owned());
            }
            let format = match wire::uint(&ask, 1) {
                Some(UNICODE_TEXT) => Some(TextFormat::Windows),
                _ => {
                    let name =
                        wire::blob(&ask, 3).and_then(|value| std::str::from_utf8(value).ok());
                    (name == Some(MAC_TEXT)).then_some(TextFormat::Mac)
                }
            };
            let data = format
                .zip(self.local.as_ref())
                .map(|(format, text)| format.encode(text));
            let mut confirm = Vec::new();
            wire::int(1, if data.is_some() { 1 } else { 2 }, &mut confirm);
            wire::bytes(2, key.as_bytes(), &mut confirm);
            if let Some(data) = &data {
                wire::int(3, data.chunks(BLOCK_BYTES).len() as u64, &mut confirm);
            }
            out.push(response(header, 2, &confirm));
            if let Some(data) = data {
                for (index, part) in data.chunks(BLOCK_BYTES).enumerate() {
                    let mut block = Vec::new();
                    wire::bytes(1, key.as_bytes(), &mut block);
                    wire::int(2, index as u64, &mut block);
                    wire::bytes(3, part, &mut block);
                    out.push(request(next_id(), 3, &block));
                }
            }
        } else if let Some(block) = wire::blob(&clip, 3) {
            let block = wire::decode(block).ok_or("invalid clipboard block")?;
            let key = wire::blob(&block, 1)
                .and_then(|value| std::str::from_utf8(value).ok())
                .ok_or("invalid clipboard block key")?;
            let index = wire::uint(&block, 2).unwrap_or(0) as usize;
            let data = wire::blob(&block, 3).unwrap_or_default();
            if data.len() > MAX_BYTES || index > MAX_BYTES / BLOCK_BYTES + 1 {
                return Err("clipboard block exceeds limit".to_owned());
            }
            let pending = self.pending.as_mut().ok_or("unsolicited clipboard block")?;
            if key != pending.key {
                return Err("unexpected clipboard block key".to_owned());
            }
            if pending.blocks.values().map(Vec::len).sum::<usize>() + data.len() > MAX_BYTES {
                return Err("clipboard transfer exceeds limit".to_owned());
            }
            pending.blocks.insert(index, data.to_vec());
            let mut confirm = Vec::new();
            wire::bytes(1, key.as_bytes(), &mut confirm);
            wire::int(2, index as u64, &mut confirm);
            wire::int(3, 1, &mut confirm);
            out.push(response(header, 3, &confirm));
            let text = self.complete()?;
            return Ok((out, text));
        }
        Ok((out, None))
    }

    /// Handles `Message.rpc_response` field 22 and returns completed text.
    pub fn handle_response(&mut self, rpc: &[Field]) -> Result<Option<String>, String> {
        let Some(clip) = wire::blob(rpc, 5) else {
            return Ok(None);
        };
        let clip = wire::decode(clip).ok_or("invalid clipboard response")?;
        let Some(confirm) = wire::blob(&clip, 2) else {
            return Ok(None);
        };
        let confirm = wire::decode(confirm).ok_or("invalid clipboard confirmation")?;
        let Some(pending) = self.pending.as_mut() else {
            return Ok(None);
        };
        let key = wire::blob(&confirm, 2)
            .and_then(|value| std::str::from_utf8(value).ok())
            .unwrap_or("");
        if key != pending.key {
            return Ok(None);
        }
        if wire::uint(&confirm, 1) != Some(1) {
            self.pending = None;
            return Ok(None);
        }
        let count = wire::uint(&confirm, 3).unwrap_or(0) as usize;
        if count == 0 || count > MAX_BYTES / BLOCK_BYTES + 2 {
            self.pending = None;
            return Err("invalid clipboard block count".to_owned());
        }
        pending.count = Some(count);
        self.complete()
    }

    fn complete(&mut self) -> Result<Option<String>, String> {
        let Some(pending) = &self.pending else {
            return Ok(None);
        };
        let Some(count) = pending.count else {
            return Ok(None);
        };
        let first = if (0..count).all(|index| pending.blocks.contains_key(&index)) {
            0
        } else if (1..=count).all(|index| pending.blocks.contains_key(&index)) {
            1
        } else {
            return Ok(None);
        };
        let data: Vec<u8> = (first..first + count)
            .flat_map(|index| pending.blocks[&index].iter().copied())
            .collect();
        let format = pending.format;
        self.pending = None;
        format.decode(&data).map(Some)
    }
}

fn request(request_id: u64, case: u32, body: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    wire::int(1, request_id, &mut header);
    let mut clip = Vec::new();
    wire::bytes(case, body, &mut clip);
    let mut rpc = Vec::new();
    wire::bytes(1, &header, &mut rpc);
    wire::bytes(9, &clip, &mut rpc);
    wire::message(0, 21, &rpc)
}

fn response(header: &[u8], case: u32, body: &[u8]) -> Vec<u8> {
    let mut clip = Vec::new();
    wire::bytes(case, body, &mut clip);
    let mut rpc = Vec::new();
    wire::bytes(1, header, &mut rpc);
    wire::bytes(5, &clip, &mut rpc);
    wire::message(0, 22, &rpc)
}

fn encode_windows(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\n")
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn decode_windows(data: &[u8]) -> Result<String, String> {
    if data.len() > MAX_BYTES || !data.len().is_multiple_of(2) {
        return Err("invalid Unicode clipboard length".to_owned());
    }
    let mut chars: Vec<u16> = data
        .chunks_exact(2)
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
        .collect();
    if chars.last() == Some(&0) {
        chars.pop();
    }
    if chars.contains(&0) {
        return Err("embedded NUL in clipboard text".to_owned());
    }
    let text = String::from_utf16(&chars)
        .map_err(|_| "invalid Unicode clipboard text".to_owned())?
        .replace("\r\n", "\n");
    if text.len() > MAX_TEXT {
        return Err("clipboard text exceeds limit".to_owned());
    }
    Ok(text)
}

/// Assembles `Message.clipboard_change` (field 17) chunks. The official
/// viewer may split text across messages with `to_be_continued`.
#[derive(Default)]
pub struct ClipboardAssembly {
    pending: Option<(u32, String, std::time::Instant)>,
}

impl ClipboardAssembly {
    pub fn accept(&mut self, payload: &[u8]) -> Result<Option<String>, String> {
        if payload.len() > MAX_TEXT + 256 {
            self.pending = None;
            return Err("clipboard message exceeds limit".to_owned());
        }
        let fields = wire::decode(payload).ok_or("invalid clipboard wire")?;
        let mut id = 0u32;
        let mut text = "";
        let mut more = false;
        let mut seen = [false; 3];
        for field in &fields {
            if !(1..=3).contains(&field.number) {
                continue;
            }
            let index = field.number as usize - 1;
            if seen[index] {
                self.pending = None;
                return Err("duplicate clipboard field".to_owned());
            }
            seen[index] = true;
            match (field.number, &field.value) {
                (1, wire::Value::Varint(value)) => {
                    id = u32::try_from(*value).map_err(|_| "invalid clipboard content id")?
                }
                (2, wire::Value::Bytes(value)) => {
                    text = std::str::from_utf8(value).map_err(|_| "invalid clipboard UTF-8")?
                }
                (3, wire::Value::Varint(value)) if *value <= 1 => more = *value != 0,
                _ => {
                    self.pending = None;
                    return Err("invalid clipboard field type".to_owned());
                }
            }
        }
        let now = std::time::Instant::now();
        let mut combined = match self.pending.take() {
            Some((previous, text, started))
                if previous == id
                    && now.duration_since(started) < std::time::Duration::from_secs(5) =>
            {
                (text, started)
            }
            _ => (String::new(), now),
        };
        if combined.0.len() + text.len() > MAX_TEXT {
            self.pending = None;
            return Err("clipboard text exceeds limit".to_owned());
        }
        combined.0.push_str(text);
        if more {
            self.pending = Some((id, combined.0, combined.1));
            Ok(None)
        } else {
            Ok(Some(combined.0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_change_chunks_reset_and_complete() {
        let mut assembly = ClipboardAssembly::default();
        let mut first = Vec::new();
        wire::int(1, 7, &mut first);
        wire::bytes(2, b"hello ", &mut first);
        wire::int(3, 1, &mut first);
        assert_eq!(assembly.accept(&first).unwrap(), None);
        let mut second = Vec::new();
        wire::int(1, 7, &mut second);
        wire::bytes(2, b"world", &mut second);
        assert_eq!(
            assembly.accept(&second).unwrap().as_deref(),
            Some("hello world")
        );
        let mut other = Vec::new();
        wire::int(1, 8, &mut other);
        wire::bytes(2, b"x", &mut other);
        assert_eq!(assembly.accept(&other).unwrap().as_deref(), Some("x"));
    }

    #[test]
    fn round_trips_windows_and_mac_text() {
        let text = "UU clipboard 中文🙂\n".repeat(900);
        let mut provider = ClipboardSession::new();
        let mut receiver = ClipboardSession::new();
        let mut id = 10;
        let mut next = || {
            id += 1;
            id
        };
        let offer = provider.offer(1, text.clone()).unwrap();
        let offer = wire::decode(&offer).unwrap();
        let request = wire::decode(wire::blob(&offer, 21).unwrap()).unwrap();
        let (messages, _) = receiver.handle_request(&request, &mut next).unwrap();
        assert_eq!(messages.len(), 2);
        let ask =
            wire::decode(wire::blob(&wire::decode(&messages[1]).unwrap(), 21).unwrap()).unwrap();
        let (sent, _) = provider.handle_request(&ask, &mut next).unwrap();
        let confirm =
            wire::decode(wire::blob(&wire::decode(&sent[0]).unwrap(), 22).unwrap()).unwrap();
        receiver.handle_response(&confirm).unwrap();
        let mut completed = None;
        for (index, message) in sent.iter().enumerate().skip(1) {
            let rpc =
                wire::decode(wire::blob(&wire::decode(message).unwrap(), 21).unwrap()).unwrap();
            let (_, text) = receiver.handle_request(&rpc, &mut next).unwrap();
            if index + 1 == sent.len() {
                completed = text;
            }
        }
        assert_eq!(completed.as_deref(), Some(text.as_str()));
    }
}
