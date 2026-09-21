//! HID presence via `IOHIDManager`, on a thread of its own.
//!
//! The manager needs a CFRunLoop, and that run loop never returns, so the
//! watcher owns one dedicated thread for the life of the process. Everything
//! it learns leaves as a [`RawHidEvent`] on a tokio channel; the matching and
//! removal callbacks do no work beyond reading three properties.
//!
//! Neither enumeration nor presence needs the input-monitoring permission: we
//! never open a device or read a report (see [`crate::permissions`]).

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{
    kCFRunLoopDefaultMode, CFArray, CFDictionary, CFNumber, CFRetained, CFRunLoop, CFString, CFType,
};
use objc2_io_kit::{kIOHIDOptionsTypeNone, IOHIDDevice, IOHIDManager, IOReturn};
use tokio::sync::mpsc::UnboundedSender;

use super::{RawHidEvent, TriggerError};

/// IOKit property keys (`kIOHIDVendorIDKey` and friends, which are `CStr`).
const VENDOR_ID_KEY: &str = "VendorID";
const PRODUCT_ID_KEY: &str = "ProductID";
const PRODUCT_KEY: &str = "Product";

/// Starts watching for the devices in `watched`, forever.
///
/// Matching is by **vendor** only: the tracker above filters by `(vid, pid)`,
/// so a config edit that adds another product of a vendor we already watch
/// needs no restart. An empty `watched` matches every HID device, which is
/// what happens while there is no usable config yet.
pub fn start_watcher(
    watched: Vec<(u16, u16)>,
    tx: UnboundedSender<RawHidEvent>,
) -> Result<(), TriggerError> {
    std::thread::Builder::new()
        .name("relay-hid".to_string())
        .spawn(move || run_watcher(&watched, tx))
        .map(|_handle| ())
        .map_err(|err| TriggerError::Thread(err.to_string()))
}

/// Every HID device currently on this machine, one row per `vid:pid`.
///
/// Used to fill in `config.json`; duplicates (a device publishes several HID
/// nodes) are folded into the first one seen.
pub fn list_hid_devices() -> Vec<(u16, u16, String)> {
    let manager = IOHIDManager::new(None, kIOHIDOptionsTypeNone);
    // SAFETY: `None` means "match everything", so there are no dictionary
    // generics to get wrong.
    unsafe { manager.set_device_matching(None) };

    let Some(devices) = manager.devices() else {
        return Vec::new();
    };
    let count = devices.count().max(0) as usize;
    let mut values: Vec<*const c_void> = vec![std::ptr::null(); count];
    // SAFETY: `values` is a C array of exactly `count` pointers, as
    // `CFSetGetValues` requires, and the set holds `IOHIDDevice`s.
    unsafe { devices.values(values.as_mut_ptr()) };

    let mut listed: Vec<(u16, u16, String)> = Vec::with_capacity(count);
    for value in values {
        let Some(device) = NonNull::new(value.cast_mut().cast::<IOHIDDevice>()) else {
            continue;
        };
        // SAFETY: the pointer comes from the set returned above, which owns
        // its devices for as long as `devices` is alive.
        let device = unsafe { device.as_ref() };
        let Some((vid, pid)) = vid_pid(device) else {
            continue;
        };
        if listed.iter().any(|(v, p, _)| (*v, *p) == (vid, pid)) {
            continue;
        }
        listed.push((vid, pid, product_name(device)));
    }
    listed.sort_by_key(|(vid, pid, _)| (*vid, *pid));
    listed
}

/// The watcher thread: set up the manager, then run the run loop forever.
fn run_watcher(watched: &[(u16, u16)], tx: UnboundedSender<RawHidEvent>) {
    let manager = IOHIDManager::new(None, kIOHIDOptionsTypeNone);
    set_vendor_matching(&manager, watched);

    // The callbacks run for the life of the process, so the sender they share
    // is leaked on purpose: nothing may free it while IOKit can still call in.
    let context: *mut c_void = Box::into_raw(Box::new(tx)).cast();
    // SAFETY: both callbacks have the signature `IOHIDDeviceCallback` asks
    // for, and `context` is the leaked sender they expect, alive forever.
    unsafe {
        manager.register_device_matching_callback(Some(device_added), context);
        manager.register_device_removal_callback(Some(device_removed), context);
    }

    let Some(run_loop) = CFRunLoop::current() else {
        tracing::error!("the HID watcher thread has no run loop; presence is off");
        return;
    };
    // SAFETY: reading an immutable Core Foundation constant.
    let Some(mode) = (unsafe { kCFRunLoopDefaultMode }) else {
        tracing::error!("kCFRunLoopDefaultMode is unavailable; presence is off");
        return;
    };
    // SAFETY: the manager is scheduled on this thread's run loop, which is
    // the one that runs below; it is never unscheduled.
    unsafe { manager.schedule_with_run_loop(&run_loop, mode) };

    // The manager is never opened: matching and removal callbacks fire from
    // the scheduled run loop alone, and opening would claim every HID device on
    // the bus — needless contention with whatever else is reading them.
    tracing::info!(vendors = watched.len(), "HID presence watcher started");

    CFRunLoop::run();
    tracing::warn!("the HID run loop returned; presence is off until restart");
}

/// One matching dictionary per distinct vendor id.
fn set_vendor_matching(manager: &IOHIDManager, watched: &[(u16, u16)]) {
    let mut vendors: Vec<u16> = watched.iter().map(|(vid, _)| *vid).collect();
    vendors.sort_unstable();
    vendors.dedup();
    if vendors.is_empty() {
        // SAFETY: `None` means "match everything"; no generics to get wrong.
        unsafe { manager.set_device_matching(None) };
        return;
    }

    let key = CFString::from_static_str(VENDOR_ID_KEY);
    let dicts: Vec<CFRetained<CFDictionary<CFString, CFNumber>>> = vendors
        .iter()
        .map(|vid| {
            let value = CFNumber::new_i32(i32::from(*vid));
            CFDictionary::from_slices(&[&*key], &[&*value])
        })
        .collect();
    let refs: Vec<&CFDictionary<CFString, CFNumber>> = dicts.iter().map(|dict| &**dict).collect();
    let array = CFArray::from_objects(&refs);
    // SAFETY: erasing the element type of an array we just built; the array is
    // a plain `CFArrayRef` either way. It holds `CFDictionary`s of `CFString`
    // to `CFNumber`, which is the matching-criteria shape IOKit documents.
    let array = unsafe { CFRetained::cast_unchecked::<CFArray>(array) };
    // SAFETY: as above, the array's contents are the documented shape.
    unsafe { manager.set_device_matching_multiple(Some(&array)) };
}

/// `IOHIDDeviceCallback` for a device that matched.
///
/// # Safety
///
/// `context` must be the leaked `UnboundedSender` from [`run_watcher`], and
/// `device` a live `IOHIDDevice` — both guaranteed by IOKit's contract.
unsafe extern "C-unwind" fn device_added(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: NonNull<IOHIDDevice>,
) {
    // Unwinding into IOKit's run loop is undefined behaviour, so a panic in
    // here (a poisoned lock inside the channel, say) stops at this frame.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: the context outlives every callback (it is leaked), and the
        // device is valid for the duration of this call.
        let (tx, hid) = unsafe { (sender(context), device.as_ref()) };
        let Some((vid, pid)) = vid_pid(hid) else {
            return;
        };
        let _ = tx.send(RawHidEvent::Added {
            node: device.as_ptr() as usize,
            vid,
            pid,
            product: product_name(hid),
        });
    }));
    if caught.is_err() {
        tracing::error!("the HID matching callback panicked; the device is ignored");
    }
}

/// `IOHIDDeviceCallback` for a device that went away.
///
/// # Safety
///
/// Same contract as [`device_added`].
unsafe extern "C-unwind" fn device_removed(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: NonNull<IOHIDDevice>,
) {
    // As in `device_added`: a panic may not unwind into IOKit's run loop.
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: as in `device_added`; the properties of a removed device may
        // no longer be readable, so only the pointer is used.
        let tx = unsafe { sender(context) };
        let _ = tx.send(RawHidEvent::Removed {
            node: device.as_ptr() as usize,
        });
    }));
    if caught.is_err() {
        tracing::error!("the HID removal callback panicked; the device is ignored");
    }
}

/// # Safety
///
/// `context` must be the pointer leaked in [`run_watcher`].
unsafe fn sender<'a>(context: *mut c_void) -> &'a UnboundedSender<RawHidEvent> {
    // SAFETY: the caller guarantees the pointer; the box is never freed.
    unsafe { &*context.cast::<UnboundedSender<RawHidEvent>>() }
}

fn vid_pid(device: &IOHIDDevice) -> Option<(u16, u16)> {
    let vid = property_u16(device, VENDOR_ID_KEY)?;
    let pid = property_u16(device, PRODUCT_ID_KEY)?;
    Some((vid, pid))
}

fn property_u16(device: &IOHIDDevice, key: &'static str) -> Option<u16> {
    let value = device.property(&CFString::from_static_str(key))?;
    u16::try_from(value.downcast_ref::<CFNumber>()?.as_i64()?).ok()
}

/// The device's product name, or a placeholder when it has none.
fn product_name(device: &IOHIDDevice) -> String {
    device
        .property(&CFString::from_static_str(PRODUCT_KEY))
        .as_deref()
        .and_then(CFType::downcast_ref::<CFString>)
        .map(CFString::to_string)
        .unwrap_or_else(|| "(unnamed)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual check: `cargo test -p relay-core -- --ignored --nocapture
    /// list_the_hid_devices_on_this_machine`.
    #[test]
    #[ignore = "reads real hardware"]
    fn list_the_hid_devices_on_this_machine() {
        for (vid, pid, product) in list_hid_devices() {
            println!("{vid:04x}:{pid:04x}  {product}");
        }
    }
}
