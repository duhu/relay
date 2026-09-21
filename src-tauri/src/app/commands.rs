//! The commands the settings and log windows call over `invoke`.
//!
//! Everything the UI needs goes through here: the config file it edits and the
//! [`CoreHandle`] it asks for status, logs and manual switches. Saving writes
//! the file and stops there — the core notices the change through its watcher
//! and reloads itself (invariant 7 in `docs/overview.md`), so no command ever
//! pokes the core's state directly.

use std::path::Path;

use relay_core::config::Config;
use relay_core::device::discovery::{self, DiscoveredDevice};
use relay_core::display::ddc::{self, DdcDisplay, DiscoveredDisplay};
use relay_core::display::DisplayInput;
use relay_core::executor::SwitchReport;
use relay_core::log::LogEntry;
use relay_core::permissions;
use relay_core::runtime::{CoreHandle, Status};
use relay_core::trigger::presence;
use relay_core::types::HostIndex;
use serde::Serialize;
use tauri::State;

use super::paths;

/// The System Settings pane that holds the Input Monitoring list.
const PRIVACY_PANE_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";

/// One currently connected HID device, as the settings window's device picker
/// shows it. `id` is the `vid:pid` form `DeviceId` uses.
#[derive(Clone, Debug, Serialize)]
pub struct HidDeviceInfo {
    pub vid: String,
    pub pid: String,
    pub id: String,
    pub name: String,
}

/// The config as it is on disk. A missing or unparseable file is an error the
/// settings window shows, not an empty form that would overwrite it on save.
#[tauri::command]
pub fn get_config() -> Result<Config, String> {
    Config::load(&paths::config_path()).map_err(|err| err.to_string())
}

/// Validates `cfg` and, only then, writes it atomically.
#[tauri::command]
pub fn save_config(cfg: Config) -> Result<(), String> {
    save_config_to(&paths::config_path(), &cfg)
}

/// The body of [`save_config`] against an explicit path, so it can be tested
/// without touching the real config file.
fn save_config_to(path: &Path, cfg: &Config) -> Result<(), String> {
    // An invalid config must never reach the file: the core would pick it up
    // and drop to `Unconfigured`.
    cfg.validate().map_err(|err| err.to_string())?;
    cfg.save_atomic(path).map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn get_status(core: State<'_, CoreHandle>) -> Result<Status, String> {
    Ok(core.status().await)
}

/// The settings window's "试切" button: a manual switch, no debounce.
#[tauri::command]
pub async fn trigger_switch(
    core: State<'_, CoreHandle>,
    target: HostIndex,
) -> Result<SwitchReport, String> {
    core.switch(target, "settings")
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
pub fn get_logs(core: State<'_, CoreHandle>) -> Vec<LogEntry> {
    core.logs()
}

/// Enumerating HID takes long enough to stutter the window, so it runs off the
/// main thread.
#[tauri::command(async)]
pub fn list_hid_devices() -> Vec<HidDeviceInfo> {
    presence::list_hid_devices()
        .into_iter()
        .map(|(vid, pid, name)| HidDeviceInfo {
            vid: format!("{vid:04x}"),
            pid: format!("{pid:04x}"),
            id: format!("{vid:04x}:{pid:04x}"),
            name,
        })
        .collect()
}

/// The settings window's "扫描设备" button: every Logitech device on this
/// machine that can switch hosts, with the slots it reports.
///
/// Unlike [`list_hid_devices`] this opens a HID++ channel on each node, so it
/// needs Input Monitoring and takes seconds rather than milliseconds. It never
/// fails: a device that will not answer is simply missing from the list.
#[tauri::command]
pub async fn scan_devices() -> Vec<DiscoveredDevice> {
    discovery::scan_switchable_devices().await
}

/// The settings window's "扫描显示器" button: every external display this Mac
/// can drive over DDC, with the `EDID UUID` the config names it by.
///
/// Unlike [`scan_devices`] this needs no permission and takes milliseconds —
/// DDC goes through the IORegistry, not HID. A machine with no external
/// display simply yields an empty list.
#[tauri::command]
pub async fn list_displays() -> Vec<DiscoveredDisplay> {
    ddc::list_displays().await
}

/// The 显示器 row's 「读取」 button: which input source that display is
/// showing right now, or `None` when it will not say.
///
/// Reading changes nothing on the display, so this needs no confirmation and
/// no permission. `edid_uuid` is empty for "the first external display",
/// exactly as the config spells it. A display that cannot be read is not an
/// error — the settings window then asks the user to type the code by hand.
#[tauri::command]
pub async fn read_display_input(edid_uuid: String) -> Option<u8> {
    let name = if edid_uuid.is_empty() {
        "first".to_string()
    } else {
        edid_uuid.clone()
    };
    DdcDisplay::new(Some(edid_uuid), name).current_input().await
}

#[tauri::command]
pub fn input_monitoring_granted() -> bool {
    permissions::input_monitoring_granted()
}

/// Shows the system prompt, then reloads the core so `Status` republishes the
/// new permission instead of waiting for the next config change.
#[tauri::command]
pub async fn request_input_monitoring(core: State<'_, CoreHandle>) -> Result<bool, String> {
    // The call blocks until the user answers the prompt.
    let granted = tauri::async_runtime::spawn_blocking(permissions::request_input_monitoring)
        .await
        .map_err(|err| err.to_string())?;
    tracing::info!(granted, "the Input Monitoring prompt was answered");

    if granted {
        // The answer to the prompt is what the UI asked for; a failed reload
        // only means `Status` republishes the permission a little later.
        if let Err(err) = core.reload().await {
            tracing::warn!(%err, "reloading the core after the grant failed");
        }
    }
    Ok(granted)
}

/// Opens System Settings on the Input Monitoring list, for a permission that
/// was denied before and can only be flipped by hand.
#[tauri::command(async)]
pub fn open_privacy_settings() -> Result<(), String> {
    // Fire and forget: waiting on `open` would hold the command for as long as
    // System Settings takes to come up, and its exit code says nothing useful.
    std::process::Command::new("open")
        .arg(PRIVACY_PANE_URL)
        .spawn()
        .map(drop)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first-run example, whose `this_host` is a slot no host declares.
    fn invalid_config() -> Config {
        serde_json::from_str(super::super::EXAMPLE_CONFIG).expect("the example config parses")
    }

    fn valid_config() -> Config {
        let mut cfg = invalid_config();
        // The seed declares slots 1 and 2; slot 0 is the unpaired one.
        cfg.this_host = 1;
        cfg
    }

    #[test]
    fn an_invalid_config_is_rejected_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let err = save_config_to(&path, &invalid_config()).expect_err("must not validate");
        assert!(err.contains("this_host"), "unexpected message: {err}");
        assert!(!path.exists(), "a rejected config must not reach the file");
    }

    #[test]
    fn a_valid_config_is_written_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        save_config_to(&path, &valid_config()).expect("must save");
        assert_eq!(Config::load(&path).expect("load"), valid_config());
    }

    #[test]
    fn a_rejected_save_leaves_the_previous_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save_config_to(&path, &valid_config()).expect("first save");

        let mut broken = valid_config();
        broken.devices.iter_mut().for_each(|d| d.is_trigger = false);
        save_config_to(&path, &broken).expect_err("must not validate");

        assert_eq!(Config::load(&path).expect("load"), valid_config());
    }
}
