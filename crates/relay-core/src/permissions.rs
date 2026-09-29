//! The TCC permissions the core can ask for: Input Monitoring and
//! Accessibility.
//!
//! Presence watching (`IOHIDManager` matching and removal callbacks) does not
//! need Input Monitoring; opening a device and reading its reports does, which
//! is what the native HID++ layer will do in M1. Accessibility is only for the
//! mouse-button remapper ([`crate::buttons::tap`]). The UI shows both states
//! and offers the prompts.

use std::ffi::c_void;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_io_kit::{IOHIDAccessType, IOHIDCheckAccess, IOHIDRequestAccess, IOHIDRequestType};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: &'static CFString;
}

/// Does this process have Input Monitoring?
pub fn input_monitoring_granted() -> bool {
    IOHIDCheckAccess(IOHIDRequestType::ListenEvent) == IOHIDAccessType::Granted
}

/// Asks for Input Monitoring, showing the system prompt the first time.
///
/// This blocks until the user answers (and returns `false` right away once the
/// permission was denied before, leaving the user to flip it in System
/// Settings), so callers on an async runtime must use `spawn_blocking`.
pub fn request_input_monitoring() -> bool {
    IOHIDRequestAccess(IOHIDRequestType::ListenEvent)
}

/// Does this process have Accessibility? The mouse-button remapper needs it
/// both to swallow a button and to post the shortcut in its place.
pub fn accessibility_granted() -> bool {
    // SAFETY: a plain query with no arguments.
    unsafe { AXIsProcessTrusted() }
}

/// Registers Relay in the Accessibility list and shows the system prompt
/// pointing there. Returns whether access is already granted; it never is on
/// the first call, because the switch can only be flipped in System Settings.
pub fn request_accessibility() -> bool {
    // SAFETY: `kAXTrustedCheckOptionPrompt` is a CFString constant owned by
    // the framework and valid for the life of the process.
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::from_slices(&[key], &[CFBoolean::new(true)]);
    let options: *const CFDictionary = options.as_opaque();
    // SAFETY: `options` is a valid CFDictionary for the duration of the call.
    unsafe { AXIsProcessTrustedWithOptions(options.cast()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_the_permission_does_not_crash() {
        // Whatever the machine answers, the call must be safe to make from a
        // test binary that has no TCC entry at all.
        let _ = input_monitoring_granted();
    }

    #[test]
    fn checking_accessibility_does_not_crash() {
        let _ = accessibility_granted();
    }
}
