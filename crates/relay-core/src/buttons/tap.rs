//! The macOS half of P9: a CGEventTap that swallows mapped mouse buttons and
//! posts the shortcut each one stands for.
//!
//! Needs Accessibility. Without it the tap cannot be created; the thread
//! retries every five seconds, so a grant in System Settings takes effect
//! without a restart.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use objc2_core_foundation::{kCFRunLoopCommonModes, CFMachPort, CFRetained, CFRunLoop};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventMask, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use super::{ButtonMap, KeyChord};
use crate::permissions;

const RETRY: Duration = Duration::from_secs(5);

/// The handle the runtime keeps: the tap thread reads the map it writes.
pub struct ButtonRemapper {
    map: Arc<RwLock<ButtonMap>>,
}

/// Everything the tap callback needs. Leaked once per process: the tap lives
/// until the process exits, and so must the pointer it was created with.
struct TapState {
    map: Arc<RwLock<ButtonMap>>,
    /// Chords whose key-down was posted, by button number, so the key-up
    /// matches the key-down even if the mapping changed in between.
    held: Mutex<BTreeMap<i64, KeyChord>>,
    /// The tap itself, for re-enabling it after the system disabled it.
    port: OnceLock<CFRetained<CFMachPort>>,
}

impl ButtonRemapper {
    /// Starts the tap thread and returns at once. The map starts empty, so no
    /// button is touched until [`ButtonRemapper::set_map`] says otherwise.
    pub fn start() -> ButtonRemapper {
        let map = Arc::new(RwLock::new(ButtonMap::default()));
        let shared = Arc::clone(&map);
        std::thread::Builder::new()
            .name("relay-buttons".into())
            .spawn(move || run(shared))
            .expect("spawn the mouse-button thread");
        ButtonRemapper { map }
    }

    /// Replaces the mappings; the next button press already uses them.
    pub fn set_map(&self, map: ButtonMap) {
        if let Ok(mut current) = self.map.write() {
            *current = map;
        }
    }
}

fn run(map: Arc<RwLock<ButtonMap>>) {
    // Built on this thread because `CFMachPort` is not `Send`: the state is
    // only ever touched here, by the callback the run loop below calls.
    let state: &'static TapState = Box::leak(Box::new(TapState {
        map,
        held: Mutex::new(BTreeMap::new()),
        port: OnceLock::new(),
    }));
    let mut warned = false;
    loop {
        let wanted = state.map.read().map(|m| !m.is_empty()).unwrap_or(false);
        // No tap until something is mapped: an idle tap would still sit in
        // the path of every middle and side click for nothing.
        if wanted && permissions::accessibility_granted() {
            let mask: CGEventMask = (1 << CGEventType::OtherMouseDown.0 as u64)
                | (1 << CGEventType::OtherMouseUp.0 as u64);
            // SAFETY: `callback` matches `CGEventTapCallBack` and never
            // unwinds; `state` is leaked, so it outlives the tap.
            let port = unsafe {
                CGEvent::tap_create(
                    CGEventTapLocation::HIDEventTap,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    mask,
                    Some(callback),
                    state as *const TapState as *mut c_void,
                )
            };
            match port {
                Some(port) => {
                    install(state, port);
                    // `CFRunLoop::run` only returns once the loop has no
                    // sources left, which cannot happen while the tap exists.
                    tracing::error!(
                        "the mouse-button run loop stopped; buttons are no longer remapped"
                    );
                    return;
                }
                None if !warned => {
                    warned = true;
                    tracing::warn!("could not create the mouse-button event tap; retrying");
                }
                None => tracing::debug!("could not create the mouse-button event tap; retrying"),
            }
        }
        std::thread::sleep(RETRY);
    }
}

/// Adds the tap to this thread's run loop, enables it and runs the loop.
///
/// A tap comes back from `tap_create` already enabled, so every way out that
/// leaves it without a run loop disables it first: an enabled tap nobody
/// services would hold up every middle and side click until the system
/// timed it out.
fn install(state: &'static TapState, port: CFRetained<CFMachPort>) {
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        tracing::error!("could not create a run-loop source for the mouse-button tap");
        CGEvent::tap_enable(&port, false);
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        tracing::error!("the mouse-button thread has no run loop");
        CGEvent::tap_enable(&port, false);
        return;
    };
    // SAFETY: a framework constant, valid for the life of the process.
    let common = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&source), common);
    CGEvent::tap_enable(&port, true);
    let port = state.port.get_or_init(|| port);
    tracing::info!("mouse-button tap installed");
    CFRunLoop::run();
    // Unreachable in practice, see above.
    CGEvent::tap_enable(port, false);
}

/// The tap callback. Returning null swallows the event; returning `event`
/// passes it on unchanged.
///
/// Must not panic: it is called from C. Every lock failure passes the event
/// through, which leaves the mouse working as if Relay were not there.
unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    let pass = event.as_ptr();
    if user_info.is_null() {
        return pass;
    }
    // SAFETY: `user_info` is the leaked `TapState` given to `tap_create`.
    let state = unsafe { &*(user_info as *const TapState) };

    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        // The system turns a tap off when a callback was too slow or the user
        // asked for it (secure input); without this the buttons would fall
        // back to their defaults until the next restart.
        if let Some(port) = state.port.get() {
            CGEvent::tap_enable(port, true);
        }
        return pass;
    }

    // SAFETY: the event is valid for the duration of the callback.
    let button = CGEvent::integer_value_field(
        Some(unsafe { event.as_ref() }),
        CGEventField::MouseEventButtonNumber,
    );

    if ty == CGEventType::OtherMouseDown {
        let Ok(map) = state.map.read() else {
            return pass;
        };
        let Some(chord) = map.resolve(button) else {
            return pass;
        };
        drop(map);
        let Ok(mut held) = state.held.lock() else {
            return pass;
        };
        held.insert(button, chord);
        drop(held);
        post(chord, true);
        null_mut()
    } else if ty == CGEventType::OtherMouseUp {
        let Ok(mut held) = state.held.lock() else {
            return pass;
        };
        // The chord recorded at key-down, not a fresh lookup: a mapping
        // changed mid-press must not leave a modifier stuck down.
        let Some(chord) = held.remove(&button) else {
            return pass;
        };
        drop(held);
        post(chord, false);
        null_mut()
    } else {
        pass
    }
}

/// Posts one key event with the chord's flags, as a keyboard would send it.
fn post(chord: KeyChord, down: bool) {
    let Some(event) = CGEvent::new_keyboard_event(None, chord.key_code, down) else {
        return;
    };
    CGEvent::set_flags(Some(&event), CGEventFlags(chord.flags));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}
