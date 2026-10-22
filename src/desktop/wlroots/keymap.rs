//! xkbcommon keymap shared by the wlr virtual keyboard and text input.
//!
//! The virtual keyboard protocol makes the client responsible for the keymap
//! it types with. A US/evdev keymap is compiled once; every keysym in its
//! first two levels is indexed to a keycode and shift state. Text with
//! characters outside that map is typed against a temporary keymap that adds
//! the needed keysyms to otherwise unused keycodes.

use std::collections::HashMap;

use xkbcommon::xkb;

/// First XKB keycode (evdev + 8) used for temporary text.
const SPARE_FIRST: u8 = 240;
/// Number of spare keycodes (XKB 240..=255).
const SPARE_COUNT: u8 = 16;

#[derive(Clone)]
pub(crate) struct Keymap {
    keymap: xkb::Keymap,
    /// keysym -> (evdev keycode, needs Shift)
    lookup: HashMap<u32, (u8, bool)>,
    shift: u32,
    ctrl: u32,
    alt: u32,
    logo: u32,
}

impl Keymap {
    pub(crate) fn load() -> Result<Self, String> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            "cannot compile the xkb keymap; install the xkeyboard-config data files".to_owned()
        })?;
        Ok(Self::from_keymap(keymap))
    }

    fn from_keymap(keymap: xkb::Keymap) -> Self {
        let mut lookup = HashMap::new();
        for code in 8u32..=255 {
            let keycode = xkb::Keycode::new(code);
            for level in 0..2u32 {
                for &sym in keymap.key_get_syms_by_level(keycode, 0, level) {
                    if sym.raw() != 0 {
                        lookup
                            .entry(sym.raw())
                            .or_insert(((code - 8) as u8, level == 1));
                    }
                }
            }
        }
        let index = |name: &str| keymap.mod_get_index(name);
        Self {
            shift: index("Shift"),
            ctrl: index("Control"),
            alt: index("Mod1"),
            logo: index("Mod4"),
            keymap,
            lookup,
        }
    }

    /// The keymap text for `zwp_virtual_keyboard_v1.keymap`.
    pub(crate) fn text(&self) -> String {
        self.keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }

    pub(crate) fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        self.lookup.get(&keysym).copied()
    }

    /// Modifier mask bit for Shift.
    pub(crate) fn shift_bit(&self) -> u32 {
        1 << self.shift
    }

    /// Modifier bit for a modifier keysym.
    pub(crate) fn modifier(&self, keysym: u32) -> Option<u32> {
        let index = match keysym {
            0xffe1 | 0xffe2 => self.shift,
            0xffe3 | 0xffe4 => self.ctrl,
            0xffe9 | 0xffea => self.alt,
            0xffeb | 0xffec => self.logo,
            _ => return None,
        };
        Some(1 << index)
    }

    /// Build a keymap that also maps `extras` (keysym, char) to spare
    /// keycodes. Returns `None` when xkbcommon rejects the generated map.
    pub(crate) fn with_extras(&self, extras: &[(u32, char)]) -> Option<(Self, Vec<(char, u8)>)> {
        let mut symbols = String::new();
        let mut assigned = Vec::new();
        for (index, (keysym, ch)) in extras.iter().take(SPARE_COUNT as usize).enumerate() {
            let code = SPARE_FIRST + index as u8;
            // Uxxxx works for every printable Unicode keysym.
            let _ = keysym;
            symbols.push_str(&format!(" key <I{code}> {{ [ U{:04X} ] }};\n", *ch as u32));
            assigned.push((*ch, code - 8));
        }
        if assigned.is_empty() {
            return None;
        }
        let text = format!(
            "xkb_keymap {{\n\
             xkb_keycodes {{ include \"evdev+aliases(qwerty)\" }};\n\
             xkb_types {{ include \"complete\" }};\n\
             xkb_compat {{ include \"complete\" }};\n\
             xkb_symbols {{ include \"pc+us+inet(evdev)\"\n{symbols} }};\n\
             }};\n"
        );
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        Some((Self::from_keymap(keymap), assigned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_keymap_maps_ascii_and_shifted_keys() {
        let keymap = Keymap::load().expect("xkb keymap");
        assert_eq!(keymap.lookup(u32::from(b'a')), Some((30, false)));
        assert_eq!(keymap.lookup(u32::from(b'A')), Some((30, true)));
        assert_eq!(keymap.lookup(0xff0d), Some((28, false))); // Return
        assert!(keymap.lookup(0x0100_4f60).is_none()); // CJK
        assert!(keymap.modifier(0xffe1).is_some());
        assert!(keymap.text().contains("xkb_keymap"));
    }

    #[test]
    fn extra_keymap_adds_spare_keysyms() {
        let keymap = Keymap::load().expect("xkb keymap");
        let (extra, assigned) = keymap
            .with_extras(&[(0x0100_4f60, '你'), (0x0100_597d, '好')])
            .expect("temporary keymap");
        assert_eq!(assigned, vec![('你', 232), ('好', 233)]);
        assert_eq!(extra.lookup(0x0100_4f60), Some((232, false)));
        assert_eq!(extra.lookup(u32::from(b'a')), Some((30, false)));
    }
}
