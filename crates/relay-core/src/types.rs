//! Shared value types: hosts, device identity and trigger events.

use serde::{Deserialize, Serialize};

/// A host slot, 0-based, exactly as HID++ numbers them. The UI shows index + 1.
pub type HostIndex = u8;

/// Stable identity of a configured input device.
///
/// M0 uses the `vid:pid` hex form, e.g. `"046d:b366"`; later milestones may
/// append a unit id, so anything after the second field is opaque here.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Parses the leading `vid:pid` pair, or `None` if the id is not in that form.
    pub fn vid_pid(&self) -> Option<(u16, u16)> {
        let mut parts = self.0.split(':');
        let vid = u16::from_str_radix(parts.next()?, 16).ok()?;
        let pid = u16::from_str_radix(parts.next()?, 16).ok()?;
        Some((vid, pid))
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a device is used for; drives the switch order and the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceRole {
    Keyboard,
    Mouse,
    Other,
}

/// Host slots as reported by a device's HID++ `ChangeHost` feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostInfo {
    pub count: u8,
    pub current: u8,
}

/// Anything that can ask for a switch: presence changes or an explicit request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerEvent {
    DeviceLeft(DeviceId),
    DeviceArrived(DeviceId),
    Manual {
        target: HostIndex,
        source: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vid_pid_parses_lowercase_hex() {
        assert_eq!(
            DeviceId("046d:b366".to_string()).vid_pid(),
            Some((0x046d, 0xb366))
        );
    }

    #[test]
    fn vid_pid_rejects_non_hex() {
        assert_eq!(DeviceId("mx-keyboard".to_string()).vid_pid(), None);
    }
}
