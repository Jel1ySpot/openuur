//! Minimal protobuf wire helpers for the UU control channel.
//!
//! The control channel carries `gv_pb.Message` frames. Only the fields needed
//! by the terminal and echo paths are encoded here; unknown fields are ignored
//! by the peer.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(Vec<u8>),
    Fixed32([u8; 4]),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub number: u32,
    pub value: Value,
}

pub fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn int(number: u32, value: u64, out: &mut Vec<u8>) {
    put_varint(u64::from(number) << 3, out);
    put_varint(value, out);
}

pub fn bytes(number: u32, value: &[u8], out: &mut Vec<u8>) {
    put_varint((u64::from(number) << 3) | 2, out);
    put_varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

/// A `fixed64` field, used by protobuf `double`.
pub fn fixed64(number: u32, value: u64, out: &mut Vec<u8>) {
    put_varint((u64::from(number) << 3) | 1, out);
    out.extend_from_slice(&value.to_le_bytes());
}

pub fn decode(mut data: &[u8]) -> Option<Vec<Field>> {
    let mut fields = Vec::new();
    while !data.is_empty() {
        let key = read_varint(&mut data)?;
        let number = u32::try_from(key >> 3).ok()?;
        if number == 0 {
            return None;
        }
        match key & 7 {
            0 => fields.push(Field {
                number,
                value: Value::Varint(read_varint(&mut data)?),
            }),
            1 => {
                if data.len() < 8 {
                    return None;
                }
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&data[..8]);
                data = &data[8..];
                fields.push(Field {
                    number,
                    value: Value::Fixed64(bytes),
                });
            }
            2 => {
                let len = usize::try_from(read_varint(&mut data)?).ok()?;
                if data.len() < len {
                    return None;
                }
                fields.push(Field {
                    number,
                    value: Value::Bytes(data[..len].to_vec()),
                });
                data = &data[len..];
            }
            5 => {
                if data.len() < 4 {
                    return None;
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&data[..4]);
                data = &data[4..];
                fields.push(Field {
                    number,
                    value: Value::Fixed32(bytes),
                });
            }
            _ => return None,
        }
    }
    Some(fields)
}

fn read_varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.first()?;
        *data = &data[1..];
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

pub fn uint(fields: &[Field], number: u32) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| match (field.number, &field.value) {
            (n, Value::Varint(v)) if n == number => Some(*v),
            _ => None,
        })
}

pub fn blob(fields: &[Field], number: u32) -> Option<&[u8]> {
    fields
        .iter()
        .find_map(|field| match (field.number, &field.value) {
            (n, Value::Bytes(v)) if n == number => Some(v.as_slice()),
            _ => None,
        })
}

pub fn envelope(seq: u64, kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    int(1, seq, &mut out);
    bytes(kind, payload, &mut out);
    out
}

pub fn request(request_id: u64, case: u32, body: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    int(1, request_id, &mut header);
    let mut rpc = Vec::new();
    bytes(1, &header, &mut rpc);
    bytes(case, body, &mut rpc);
    envelope(request_id, 21, &rpc)
}

pub fn response(request_id: u64, case: u32, body: &[u8]) -> Vec<u8> {
    let mut header = Vec::new();
    int(1, request_id, &mut header);
    let mut rpc = Vec::new();
    bytes(1, &header, &mut rpc);
    bytes(case, body, &mut rpc);
    envelope(request_id, 22, &rpc)
}

pub fn echo_response(seq: u64, request_seq: u64) -> Vec<u8> {
    let mut action = Vec::new();
    int(1, 1, &mut action);
    bytes(
        2,
        format!("{{\"seq\":{request_seq}}}").as_bytes(),
        &mut action,
    );
    let mut flags = Vec::new();
    for (field, version) in [(2u32, 2u64), (3, 1), (4, 2), (6, 2), (7, 2), (9, 1)] {
        int(field, version, &mut flags);
    }
    bytes(4, &flags, &mut action);
    envelope(seq, 3, &action)
}

pub fn echo_request(seq: u64) -> Vec<u8> {
    let mut action = Vec::new();
    let mut flags = Vec::new();
    for (field, version) in [(2u32, 2u64), (3, 1), (4, 2), (6, 2), (7, 2), (9, 1)] {
        int(field, version, &mut flags);
    }
    bytes(4, &flags, &mut action);
    envelope(seq, 3, &action)
}

pub fn terminal_env_response(request_id: u64) -> Vec<u8> {
    let mut body = Vec::new();
    bytes(2, b"ok", &mut body);
    response(request_id, 24, &body)
}

pub fn terminal_rename_response(request_id: u64, session: u64) -> Vec<u8> {
    let mut body = Vec::new();
    if session != 0 {
        int(1, session, &mut body);
    }
    bytes(3, b"ok", &mut body);
    response(request_id, 26, &body)
}

pub fn echo_request_seq(data: &[u8]) -> Option<u64> {
    let fields = decode(data)?;
    let seq = uint(&fields, 1)?;
    let action = blob(&fields, 3)?;
    let action = decode(action)?;
    let value = uint(&action, 1).unwrap_or(0);
    (value == 0).then_some(seq)
}

pub fn echo_response_matches(data: &[u8], request_seq: u64) -> bool {
    let Some(fields) = decode(data) else {
        return false;
    };
    let Some(action) = blob(&fields, 3) else {
        return false;
    };
    let Some(action) = decode(action) else {
        return false;
    };
    if uint(&action, 1) != Some(1) {
        return false;
    }
    let Some(args) = blob(&action, 2) else {
        return false;
    };
    let Ok(args) = serde_json::from_slice::<serde_json::Value>(args) else {
        return false;
    };
    args.get("seq").and_then(serde_json::Value::as_u64) == Some(request_seq)
}

pub fn connect_options(capture_type: u32, platform: u32, device: &str, version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    int(1, u64::from(capture_type), &mut out);
    if matches!(capture_type, 1 | 8) {
        int(2, u64::MAX, &mut out);
    }
    int(8, u64::from(platform), &mut out);
    bytes(9, device.as_bytes(), &mut out);
    int(10, 1, &mut out);
    bytes(12, version.as_bytes(), &mut out);
    out
}

pub fn control_header(app_control_id: &str, control_id: &str) -> serde_json::Value {
    let streamer = serde_json::json!({
        "control_id": control_id,
        "device_capability": {
            "display_info": [],
            "ice_id": "",
            "video_codec_capability": []
        }
    });
    serde_json::json!({
        "app_control_id": app_control_id,
        "app_data": {"_placeholder": true, "num": 0},
        "streamer_data": streamer.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_nested_fields() {
        let mut inner = Vec::new();
        int(1, 7, &mut inner);
        let mut outer = Vec::new();
        bytes(21, &inner, &mut outer);
        let fields = decode(&outer).unwrap();
        let nested = decode(blob(&fields, 21).unwrap()).unwrap();
        assert_eq!(uint(&nested, 1), Some(7));
    }

    #[test]
    fn fixed64_encodes_double_bits_little_endian() {
        let mut out = Vec::new();
        fixed64(7, 1.0f64.to_bits(), &mut out);
        let fields = decode(&out).unwrap();
        assert_eq!(fields[0].number, 7);
        assert_eq!(fields[0].value, Value::Fixed64(1.0f64.to_le_bytes()));
    }
}

/// Wrap a top-level `Message` body in the common sequence/timestamp fields.
pub fn message(seq: u64, which: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if seq != 0 {
        int(1, seq, &mut out);
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    int(2, timestamp, &mut out);
    bytes(which, payload, &mut out);
    out
}

/// QuickLaunchScanResponse (RpcResponse case 19).
pub fn quick_launch_scan_response(
    request_id: u64,
    apps: &[(String, String, String, String, String)],
) -> Vec<u8> {
    let mut scan = Vec::new();
    for (key, name, icon, target, app_id) in apps {
        let mut item = Vec::new();
        bytes(1, key.as_bytes(), &mut item);
        bytes(2, name.as_bytes(), &mut item);
        if !icon.is_empty() {
            bytes(3, icon.as_bytes(), &mut item);
        }
        if !target.is_empty() {
            bytes(4, target.as_bytes(), &mut item);
        }
        if !app_id.is_empty() {
            bytes(5, app_id.as_bytes(), &mut item);
        }
        bytes(3, &item, &mut scan);
    }
    let mut header = Vec::new();
    int(1, request_id, &mut header);
    let mut rpc = Vec::new();
    bytes(1, &header, &mut rpc);
    bytes(19, &scan, &mut rpc);
    message(0, 22, &rpc)
}

/// QuickLaunchAppIconResponse (RpcResponse case 20).
pub fn quick_launch_icon_response(request_id: u64, key: &str, icon: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    bytes(3, key.as_bytes(), &mut body);
    if let Some(icon) = icon {
        if !icon.is_empty() {
            bytes(4, icon, &mut body);
        }
    }
    let mut header = Vec::new();
    int(1, request_id, &mut header);
    let mut rpc = Vec::new();
    bytes(1, &header, &mut rpc);
    bytes(20, &body, &mut rpc);
    message(0, 22, &rpc)
}

/// Extracts `LaunchApp.app_key` (or the visible name as a fallback).
pub fn launch_app_key(data: &[u8]) -> Option<String> {
    let fields = decode(data)?;
    let app = blob(&fields, 4)?;
    let app = decode(app)?;
    blob(&app, 5)
        .and_then(|key| std::str::from_utf8(key).ok())
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            blob(&app, 2)
                .and_then(|name| std::str::from_utf8(name).ok())
                .map(str::to_owned)
        })
}

/// Returns the selected screen id from `Message.capture_config` (field 9).
pub fn capture_config_screen(data: &[u8]) -> Option<u64> {
    capture_config(data).map(|(screen, _, _)| screen)
}

/// `(screen_id, request_seq, config_flag)` from `Message.capture_config`.
/// `request_seq` is the outer `Message.seq`; the host echoes it in
/// `CaptureConfigResponse`.
pub fn capture_config(data: &[u8]) -> Option<(u64, u64, u64)> {
    let fields = decode(data)?;
    let config = blob(&fields, 9)?;
    let config = decode(config)?;
    Some((
        uint(&config, 5).unwrap_or(0),
        uint(&fields, 1).unwrap_or(0),
        uint(&config, 8).unwrap_or(0),
    ))
}

/// RPC request id and case number from `Message.rpc_request` (field 21).
pub fn rpc_request(data: &[u8]) -> Option<(u64, u32, Vec<Field>)> {
    let message = decode(data)?;
    let rpc = decode(blob(&message, 21)?)?;
    let id = blob(&rpc, 1)
        .and_then(decode)
        .and_then(|header| uint(&header, 1))
        .unwrap_or(0);
    let (case, _) = rpc
        .iter()
        .find(|field| field.number != 1 && matches!(field.value, Value::Bytes(_)))
        .map(|field| (field.number, ()))?;
    Some((id, case, rpc))
}

/// Decode a `Message.rpc_response` body (field 22).
pub fn rpc_response(data: &[u8]) -> Option<Vec<Field>> {
    let message = decode(data)?;
    decode(blob(&message, 22)?)
}
