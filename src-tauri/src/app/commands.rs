//! The commands the settings and log windows call over `invoke`.
//!
//! Everything the UI needs goes through here: the config file it edits and the
//! [`CoreHandle`] it asks for status, logs and manual switches. Saving writes
//! the file and stops there — the core notices the change through its watcher
//! and reloads itself (invariant 7 in `docs/overview.md`), so no command ever
//! pokes the core's state directly.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use relay_core::config::{Config, SCHEMA_VERSION};
use relay_core::device::discovery::{self, DiscoveredDevice};
use relay_core::display::capabilities::{self, InputSource};
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

/// What an export onto the live config file is told, in words the user can act
/// on: the destination has to be some other file.
const SAME_FILE: &str =
    "this is the config Relay is using; export it to another folder or under another name";

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

/// Reads a config file the user picked, without touching this machine's own.
///
/// The wizard shows the machine list out of it before asking which one this
/// Mac is, so the answer has to be available before anything is written.
#[tauri::command]
pub fn read_config_file(path: String) -> Result<Config, String> {
    let cfg = Config::load(Path::new(&path)).map_err(|err| err.to_string())?;
    // Gate here as well as on import: a file this Relay cannot read should be
    // refused before the wizard lists its machines and asks two questions.
    check_schema(&cfg)?;
    Ok(cfg)
}

/// Refuses a config file this Relay does not know how to read.
///
/// A file from a newer Relay may parse anyway — nothing here denies unknown
/// fields — and still mean something else. There is no migration, so say so
/// rather than half-import it. Both commands that take a foreign file call
/// this, so the message cannot drift between them.
fn check_schema(cfg: &Config) -> Result<(), String> {
    if cfg.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "this file is schema version {}, and this Relay reads version {SCHEMA_VERSION}",
            cfg.schema_version
        ));
    }
    Ok(())
}

/// Adopts the config at `path` as this machine's and writes it.
///
/// The file's own `this_host` and `leave_to` are ignored — they belong to the
/// machine that exported it. Nothing is written unless the adopted config
/// validates, so a bad file leaves the existing one exactly as it was.
#[tauri::command]
pub fn import_config(
    path: String,
    this_host: HostIndex,
    leave_to: Option<HostIndex>,
) -> Result<Config, String> {
    import_config_to(&paths::config_path(), Path::new(&path), this_host, leave_to)
}

/// The body of [`import_config`] against an explicit live path, so the order it
/// works in — schema gate, adopt, validate, atomic write — can be tested
/// without touching the real config file. Every step before the write is a
/// plain early return, which is what keeps a bad file from destroying a good
/// config.
fn import_config_to(
    live: &Path,
    source: &Path,
    this_host: HostIndex,
    leave_to: Option<HostIndex>,
) -> Result<Config, String> {
    let mut cfg = Config::load(source).map_err(|err| err.to_string())?;
    check_schema(&cfg)?;
    cfg.adopt(this_host, leave_to)
        .map_err(|err| err.to_string())?;
    save_config_to(live, &cfg)?;
    Ok(cfg)
}

/// Copies the live config file to `path`.
///
/// The file, not the window's copy of it: what is exported has to be the
/// config that is actually running, so an unsaved edit must not travel.
#[tauri::command]
pub fn export_config(path: String) -> Result<(), String> {
    export_config_to(&paths::config_path(), Path::new(&path))
}

/// The body of [`export_config`] against an explicit source, so the guard below
/// can be tested without touching the real config file.
fn export_config_to(source: &Path, dest: &Path) -> Result<(), String> {
    // `fs::copy` onto its own source truncates it to nothing and still reports
    // success, so the one destination we must refuse is the file we are reading.
    // Paths cannot settle this — a symlink, a hard link and a case-different
    // spelling all name the same inode — so compare the inode itself.
    if let (Ok(source_meta), Ok(dest_meta)) = (fs::metadata(source), fs::metadata(dest)) {
        if source_meta.dev() == dest_meta.dev() && source_meta.ino() == dest_meta.ino() {
            return Err(SAME_FILE.to_string());
        }
    }
    fs::copy(source, dest)
        .map(|_| ())
        .map_err(|err| err.to_string())
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

/// Which input sources a display says it has, for the 显示器 row's picker.
///
/// Empty when the display will not answer the capabilities request, which is
/// an answer, not an error: the settings window then shows the plain number
/// box it always had. `edid_uuid` is empty for "the first external display",
/// exactly as the config spells it.
///
/// Takes about a second, so the window asks once when it opens and again only
/// when the user rescans.
#[tauri::command]
pub async fn list_input_sources(edid_uuid: String) -> Vec<InputSource> {
    capabilities::read_input_sources(Some(edid_uuid)).await
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

    #[test]
    fn exporting_onto_the_live_file_is_refused_and_the_file_survives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        save_config_to(&path, &valid_config()).expect("save");
        let before = fs::read(&path).expect("read");

        export_config_to(&path, &path).expect_err("exporting onto itself must be refused");

        // The refusal is incidental; what matters is that the config is still
        // there. `fs::copy` would have left it at zero bytes.
        assert_eq!(fs::read(&path).expect("read"), before);
    }

    /// Writes `cfg` to `path` as the file a user would have picked — plain
    /// serialization, not [`save_config_to`], so an invalid one can be written.
    fn write_source(path: &Path, cfg: &Config) {
        fs::write(path, serde_json::to_string_pretty(cfg).expect("serialize")).expect("write");
    }

    /// A live file and a source file in one temp dir, with the live file's bytes
    /// as they were before the import.
    fn import_fixture() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
        Vec<u8>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("config.json");
        let source = dir.path().join("exported.json");
        save_config_to(&live, &valid_config()).expect("save the live config");
        let before = fs::read(&live).expect("read");
        (dir, live, source, before)
    }

    #[test]
    fn a_newer_schema_is_refused_and_the_live_config_survives() {
        let (_dir, live, source, before) = import_fixture();
        let mut newer = valid_config();
        newer.schema_version = SCHEMA_VERSION + 1;
        write_source(&source, &newer);

        let err = import_config_to(&live, &source, 1, None).expect_err("must refuse");
        assert!(err.contains("schema version"), "unexpected message: {err}");
        assert_eq!(fs::read(&live).expect("read"), before);
    }

    #[test]
    fn an_adopt_that_fails_leaves_the_live_config_alone() {
        let (_dir, live, source, before) = import_fixture();
        write_source(&source, &valid_config());

        // The example declares hosts 1 and 2; 9 is not one of them.
        import_config_to(&live, &source, 9, None).expect_err("must refuse an undeclared host");
        assert_eq!(fs::read(&live).expect("read"), before);
    }

    #[test]
    fn a_good_import_becomes_this_machine_s_config() {
        let (_dir, live, source, _before) = import_fixture();
        let exported = valid_config(); // this_host 1, as the other Mac wrote it
        write_source(&source, &exported);

        let returned = import_config_to(&live, &source, 2, Some(1)).expect("must import");
        let written = Config::load(&live).expect("load");
        assert_eq!(written, returned);

        // Only this machine's two answers differ from the file that travelled.
        let mut expected = exported;
        expected.this_host = 2;
        for device in &mut expected.devices {
            device.leave_to = device.is_trigger.then_some(1);
        }
        assert_eq!(written, expected);
    }
}
