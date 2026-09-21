//! Prints every switchable Logitech device this machine can see.
//!
//! `cargo run -p relay-core --example scan`
//!
//! Opening a HID++ channel needs Input Monitoring, which a terminal that was
//! never granted it does not have — from such a shell the list comes back empty
//! and that is the expected result, not a bug. The real check is the settings
//! window's "scan devices" button inside the signed app bundle.

use relay_core::device::discovery::{scan_switchable_devices, DiscoveredDevice};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "relay_core=debug".into()),
        )
        .init();

    let found = scan_switchable_devices().await;
    if found.is_empty() {
        println!("no switchable device found (is Input Monitoring granted to this shell?)");
        return;
    }

    for device in &found {
        let DiscoveredDevice {
            id,
            serial,
            name,
            host_count,
            current_host,
            role_guess,
        } = device;
        println!(
            "{name}  {id}  serial {}  host {}/{host_count}  role {role_guess:?}",
            serial.as_deref().unwrap_or("-"),
            current_host + 1,
        );
    }
}
