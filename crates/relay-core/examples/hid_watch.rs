//! Manual check of the HID presence watcher.
//!
//! ```text
//! cargo run -p relay-core --example hid_watch -- 046d:b366 046d:b023
//! ```
//!
//! Prints one line per tracker event. Pressing Easy-Switch on the keyboard
//! must print `DeviceLeft`, and switching back `DeviceArrived`.

use relay_core::trigger::{presence, PresenceTracker};
use relay_core::types::DeviceId;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let ids: Vec<DeviceId> = std::env::args().skip(1).map(DeviceId).collect();
    if ids.is_empty() {
        eprintln!("usage: hid_watch <vid:pid> [vid:pid ...]");
        eprintln!("devices on this machine:");
        for (vid, pid, product) in presence::list_hid_devices() {
            eprintln!("  {vid:04x}:{pid:04x}  {product}");
        }
        return;
    }

    let watched: Vec<(u16, u16)> = ids.iter().filter_map(DeviceId::vid_pid).collect();
    println!("watching {watched:?} (ctrl-c to stop)");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    presence::start_watcher(watched, tx).expect("start the watcher");

    let mut tracker = PresenceTracker::new(ids);
    while let Some(raw) = rx.recv().await {
        println!("raw    {raw:?}");
        if let Some(event) = tracker.feed(raw) {
            println!("event  {event:?}");
        }
    }
}
