//! Mouse buttons turned into keyboard shortcuts (P9).
//!
//! This module is the pure half: what a configured button resolves to, and
//! the CGEventFlags the synthesized key event carries. The macOS half is
//! [`tap`], an event tap that swallows the button and posts the keys. Nothing
//! here talks to the mouse — the buttons arrive as ordinary
//! `OtherMouseDown/Up` events.

use std::collections::BTreeMap;

use crate::config::{ButtonAction, KeyCombo, Modifier, MouseButtonMapping, Preset};

#[cfg(target_os = "macos")]
pub mod tap;

const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CTRL: u64 = 0x0004_0000;
const FLAG_OPT: u64 = 0x0008_0000;
const FLAG_CMD: u64 = 0x0010_0000;
const FLAG_NUMERIC_PAD: u64 = 0x0020_0000;
const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;
const KEY_F11: u16 = 103;

/// F1–F20 plus Home, End, Page Up/Down and Forward Delete: the keys a real
/// keyboard reports with the secondary-Fn flag set.
const FN_KEYS: [u16; 25] = [
    122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113, 106, 64, 79, 80, 90,
    115, 119, 116, 121, 117,
];

/// A key and the flags to post it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyChord {
    pub key_code: u16,
    pub flags: u64,
}

impl KeyChord {
    fn new(key_code: u16, modifiers: &[Modifier]) -> Self {
        let mut flags = 0;
        for modifier in modifiers {
            flags |= match modifier {
                Modifier::Cmd => FLAG_CMD,
                Modifier::Ctrl => FLAG_CTRL,
                Modifier::Opt => FLAG_OPT,
                Modifier::Shift => FLAG_SHIFT,
            };
        }
        if (KEY_LEFT..=KEY_UP).contains(&key_code) {
            flags |= FLAG_NUMERIC_PAD | FLAG_SECONDARY_FN;
        } else if FN_KEYS.contains(&key_code) {
            flags |= FLAG_SECONDARY_FN;
        }
        KeyChord { key_code, flags }
    }
}

fn preset_chord(preset: Preset) -> KeyChord {
    match preset {
        Preset::MissionControl => KeyChord::new(KEY_UP, &[Modifier::Ctrl]),
        Preset::AppWindows => KeyChord::new(KEY_DOWN, &[Modifier::Ctrl]),
        Preset::ShowDesktop => KeyChord::new(KEY_F11, &[]),
        Preset::SpaceLeft => KeyChord::new(KEY_LEFT, &[Modifier::Ctrl]),
        Preset::SpaceRight => KeyChord::new(KEY_RIGHT, &[Modifier::Ctrl]),
    }
}

/// Button number → the chord it stands for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ButtonMap(BTreeMap<i64, KeyChord>);

impl ButtonMap {
    pub fn from_config(mappings: &[MouseButtonMapping]) -> Self {
        let mut map = BTreeMap::new();
        for mapping in mappings {
            let chord = match &mapping.action {
                ButtonAction::Preset(preset) => preset_chord(*preset),
                ButtonAction::Keys(KeyCombo {
                    key_code,
                    modifiers,
                }) => KeyChord::new(*key_code, modifiers),
            };
            // `validate` refuses duplicates; the first row wins regardless.
            map.entry(i64::from(mapping.button)).or_insert(chord);
        }
        ButtonMap(map)
    }

    /// `button` is the raw CGEvent field, hence `i64`.
    pub fn resolve(&self, button: i64) -> Option<KeyChord> {
        self.0.get(&button).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(button: u8, action: ButtonAction) -> MouseButtonMapping {
        MouseButtonMapping { button, action }
    }

    #[test]
    fn mission_control_is_control_up_with_the_arrow_flags() {
        let map = ButtonMap::from_config(&[row(6, ButtonAction::Preset(Preset::MissionControl))]);
        assert_eq!(
            map.resolve(6),
            Some(KeyChord {
                key_code: 126,
                flags: FLAG_CTRL | FLAG_NUMERIC_PAD | FLAG_SECONDARY_FN
            })
        );
    }

    #[test]
    fn show_desktop_is_a_bare_f11_with_the_fn_flag() {
        let map = ButtonMap::from_config(&[row(4, ButtonAction::Preset(Preset::ShowDesktop))]);
        assert_eq!(
            map.resolve(4),
            Some(KeyChord {
                key_code: 103,
                flags: FLAG_SECONDARY_FN
            })
        );
    }

    #[test]
    fn every_preset_resolves() {
        for preset in [
            Preset::MissionControl,
            Preset::AppWindows,
            Preset::ShowDesktop,
            Preset::SpaceLeft,
            Preset::SpaceRight,
        ] {
            assert!(
                ButtonMap::from_config(&[row(6, ButtonAction::Preset(preset))])
                    .resolve(6)
                    .is_some()
            );
        }
    }

    #[test]
    fn a_recorded_shortcut_carries_only_its_modifiers() {
        let keys = KeyCombo {
            key_code: 33,
            modifiers: vec![Modifier::Cmd, Modifier::Shift],
        };
        let map = ButtonMap::from_config(&[row(3, ButtonAction::Keys(keys))]);
        assert_eq!(
            map.resolve(3),
            Some(KeyChord {
                key_code: 33,
                flags: FLAG_CMD | FLAG_SHIFT
            })
        );
    }

    #[test]
    fn an_unmapped_button_resolves_to_nothing() {
        let map = ButtonMap::from_config(&[row(6, ButtonAction::Preset(Preset::SpaceLeft))]);
        assert_eq!(map.resolve(3), None);
        assert!(ButtonMap::default().is_empty());
    }
}
