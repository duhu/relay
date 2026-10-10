//! How long this Mac has gone without keyboard or mouse input.
//!
//! The runtime asks this when a trigger device disconnects: a Logitech device
//! drops Bluetooth on its own after a long idle stretch, and that departure is
//! the device falling asleep, not the user pressing Easy-Switch (spec §5).
//!
//! The reading needs no permission; it is the same counter the screen saver
//! and display sleep go by.

use std::time::Duration;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

/// `kCGEventSourceStateHIDSystemState`: input from every HID device, not only
/// the events this process posted.
#[cfg(target_os = "macos")]
const HID_SYSTEM_STATE: i32 = 1;

/// `kCGAnyInputEventType`: keys, clicks, moves and scrolls alike.
#[cfg(target_os = "macos")]
const ANY_INPUT_EVENT: u32 = u32::MAX;

/// Seconds since the last keyboard or mouse input this Mac saw, from
/// CoreGraphics. `None` when the HID system has no answer.
#[cfg(target_os = "macos")]
pub fn seconds_since_last_input() -> Option<Duration> {
    // SAFETY: a plain query on two integer constants; it touches no memory of ours.
    let seconds =
        unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT_EVENT) };
    from_seconds(seconds)
}

/// Seconds since the last keyboard or mouse input this Mac saw. There is no
/// such counter off macOS, so the answer is always "unknown".
#[cfg(not(target_os = "macos"))]
pub fn seconds_since_last_input() -> Option<Duration> {
    None
}

/// A usable idle time, or `None` for a negative, NaN, infinite or absurdly
/// large reading (`from_secs_f64` would panic on the last one).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn from_seconds(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_reading_is_kept() {
        assert_eq!(from_seconds(7200.5), Some(Duration::from_secs_f64(7200.5)));
        assert_eq!(from_seconds(0.0), Some(Duration::ZERO));
    }

    #[test]
    fn a_reading_that_makes_no_sense_is_unknown() {
        assert_eq!(from_seconds(-1.0), None);
        assert_eq!(from_seconds(f64::NAN), None);
        assert_eq!(from_seconds(f64::INFINITY), None);
        assert_eq!(from_seconds(f64::NEG_INFINITY), None);
    }

    #[test]
    fn a_huge_finite_reading_is_unknown_not_a_panic() {
        assert_eq!(from_seconds(1e300), None);
        assert_eq!(from_seconds(f64::MAX), None);
    }
}
