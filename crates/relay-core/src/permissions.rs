//! The one TCC permission the core can ask for: Input Monitoring.
//!
//! Presence watching (`IOHIDManager` matching and removal callbacks) does not
//! need it; opening a device and reading its reports does, which is what the
//! native HID++ layer will do in M1. The UI shows the state and offers the
//! prompt.

use objc2_io_kit::{IOHIDAccessType, IOHIDCheckAccess, IOHIDRequestAccess, IOHIDRequestType};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_the_permission_does_not_crash() {
        // Whatever the machine answers, the call must be safe to make from a
        // test binary that has no TCC entry at all.
        let _ = input_monitoring_granted();
    }
}
