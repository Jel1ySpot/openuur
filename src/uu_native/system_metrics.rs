//! `gv_pb.SystemMetrics` (Message field 13).
//!
//! Reports measured permissions, the desktop sound capability, host settings
//! and the file-transfer save location. Nothing here claims a capability the
//! host does not have: permission values are `Unknown` until measured.

use super::wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum Permission {
    Unknown = 0,
    Reject = 1,
    Allow = 2,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub sound: bool,
    pub keep_mute: bool,
    pub mute_permanent: bool,
    pub auto_lock: bool,
}

/// `SystemMetrics` body.
pub fn payload(
    capture: Permission,
    operation: Permission,
    control: Permission,
    location: &str,
    settings: Settings,
) -> Vec<u8> {
    let mut setting = Vec::new();
    for (field, enabled) in [
        (1u32, settings.keep_mute),
        (2, settings.mute_permanent),
        (3, settings.auto_lock),
    ] {
        if enabled {
            wire::int(field, 1, &mut setting);
        }
    }
    if !location.is_empty() {
        wire::bytes(5, location.as_bytes(), &mut setting);
    }
    for (field, state) in [(8u32, capture), (9, operation), (10, control)] {
        if state != Permission::Unknown {
            wire::int(field, state as u64, &mut setting);
        }
    }

    // GeneralMetric field 3 (general) with system_sound_support = 2 (support).
    let mut general = Vec::new();
    if settings.sound {
        wire::int(2, 2, &mut general);
    }
    let mut out = Vec::new();
    wire::bytes(3, &general, &mut out);
    wire::bytes(4, &setting, &mut out);
    out
}

/// `Message` field 26 RemoteDownloadPath { 1: download_path }.
pub fn remote_download_path(path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    wire::bytes(1, path.as_bytes(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(data: &[u8]) -> Vec<wire::Field> {
        wire::decode(data).unwrap()
    }

    #[test]
    fn measured_permissions_use_official_enum_values() {
        let body = payload(
            Permission::Allow,
            Permission::Allow,
            Permission::Allow,
            "",
            Settings::default(),
        );
        let fields = fields(&body);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].number, 3);
        assert_eq!(fields[0].value, wire::Value::Bytes(Vec::new()));
        let setting = wire::decode(wire::blob(&fields, 4).unwrap()).unwrap();
        assert_eq!(
            setting.iter().map(|field| field.number).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );
        assert!(setting
            .iter()
            .all(|field| field.value == wire::Value::Varint(2)));
    }

    #[test]
    fn unknown_permissions_are_omitted_and_sound_is_reported() {
        let body = payload(
            Permission::Unknown,
            Permission::Reject,
            Permission::Unknown,
            "/home/user",
            Settings {
                sound: true,
                keep_mute: true,
                ..Default::default()
            },
        );
        let fields = fields(&body);
        let setting = wire::decode(wire::blob(&fields, 4).unwrap()).unwrap();
        assert!(wire::uint(&setting, 1) == Some(1));
        assert_eq!(wire::blob(&setting, 5), Some(&b"/home/user"[..]));
        assert_eq!(wire::uint(&setting, 9), Some(1));
        assert_eq!(wire::uint(&setting, 8), None);
        let general = wire::decode(wire::blob(&fields, 3).unwrap()).unwrap();
        assert_eq!(wire::uint(&general, 2), Some(2));
    }

    #[test]
    fn remote_download_path_is_a_single_string() {
        let body = remote_download_path("/home/x");
        assert_eq!(
            wire::decode(&body).unwrap()[0].value,
            wire::Value::Bytes(b"/home/x".to_vec())
        );
    }
}
