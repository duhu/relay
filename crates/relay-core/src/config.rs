//! The on-disk configuration: model, validation and atomic save.
//!
//! The file at [`Config::default_path`] is the single source of truth; the UI
//! writes it and the core reloads it. Field names mirror `docs/specs/relay-core.md` §6.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use objc2_core_foundation::{CFLocale, CFString, CFType};
use serde::{Deserialize, Serialize};

use crate::types::{DeviceId, DeviceRole, HostIndex};

/// The shape of the config file this build reads and writes. A file carrying
/// any other number was written by a Relay that means something else by these
/// field names, and there is no migration.
pub const SCHEMA_VERSION: u32 = 1;

/// The shortest cooldown between two switches. Repeated `ChangeHost` calls in
/// quick succession have corrupted the Bluetooth connection before (see
/// `AGENTS.md`), so a near-zero cooldown must not be storable at all.
pub const MIN_COOLDOWN_MS: u64 = 1_000;

/// The shortest debounce before a presence event becomes a switch. Below this
/// a single flaky BLE disconnect would fire a switch on its own.
pub const MIN_DEBOUNCE_MS: u64 = 100;

/// The tag the UI and the tray use for Simplified Chinese.
pub const LANG_ZH_HANS: &str = "zh-Hans";
/// The tag the UI and the tray use for English.
pub const LANG_EN: &str = "en";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    /// Which declared host this machine is.
    pub this_host: HostIndex,
    pub hosts: Vec<Host>,
    pub displays: Vec<DisplayConfig>,
    pub devices: Vec<DeviceConfig>,
    pub timing: Timing,
    /// Host index (as a string, e.g. `"0"`) to accelerator, e.g. `"Ctrl+Alt+1"`.
    pub hotkeys: BTreeMap<String, String>,
    pub options: Options,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// The HID++ host slot on the devices; only paired slots may be declared.
    pub index: HostIndex,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub edid_uuid: String,
    pub name: String,
    /// Host index (as a string, e.g. `"0"`) to DDC input source value.
    pub input_by_host: BTreeMap<String, u8>,
}

impl DisplayConfig {
    /// The DDC input source this display must show when `host` owns the screen.
    pub fn input_for(&self, host: HostIndex) -> Option<u8> {
        self.input_by_host.get(&host.to_string()).copied()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub id: DeviceId,
    pub name: String,
    pub role: DeviceRole,
    /// Free-form in M0, e.g. `"ble"`; the device layer owns the vocabulary.
    pub transport: String,
    /// Tells two devices with the same `vid:pid` apart. Absent for everyone who
    /// has one of each, and skipped on save so an old config round-trips byte
    /// for byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// Its leaving this machine starts a switch.
    pub is_trigger: bool,
    /// It is sent along to the target host on every switch.
    pub follow: bool,
    /// Where to switch when this trigger leaves; required with three hosts or more.
    #[serde(default)]
    pub leave_to: Option<HostIndex>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    pub debounce_ms: u64,
    pub cooldown_ms: u64,
    pub ddc_retries: u8,
}

/// Which language the settings window and the tray menu speak.
///
/// `Auto` is never handed to a UI: [`Language::resolve`] turns it into one of
/// the two concrete tags before it reaches [`crate::runtime::Status`], so the
/// window and the menu cannot end up disagreeing about what "follow the
/// system" means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    /// Whatever this Mac's preferred language resolves to.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-Hans")]
    ZhHans,
    #[serde(rename = "en")]
    En,
}

impl Language {
    /// The concrete tag the UI must speak; never `"auto"`.
    pub fn resolve(self) -> &'static str {
        match self {
            Language::ZhHans => LANG_ZH_HANS,
            Language::En => LANG_EN,
            Language::Auto => system_language(),
        }
    }
}

/// What `Auto` resolves to on this Mac.
///
/// The preference is read through `CFLocale::preferred_languages` rather than
/// the `LANG` environment variable: Relay normally starts from `launchd` or
/// the Finder, neither of which sets `LANG`, while the CoreFoundation call
/// reads the same `AppleLanguages` list System Settings writes and works in a
/// process with no GUI session at all. An unreadable or empty list means
/// Chinese, which is what the only user of this app speaks.
///
/// Read once per process: `Status` carries the resolved language and is rebuilt
/// on every core-loop wakeup, and macOS wants an app relaunched before it
/// honours a language change anyway.
fn system_language() -> &'static str {
    static RESOLVED: OnceLock<&'static str> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        preferred_language_tag()
            .as_deref()
            .map_or(LANG_ZH_HANS, tag_language)
    })
}

/// Maps one BCP-47 tag (`zh-Hans-CN`, `en-US`, …) to a tag Relay has a
/// dictionary for. Kept apart from the CoreFoundation call so the rule itself
/// is testable.
fn tag_language(tag: &str) -> &'static str {
    if tag.trim().to_ascii_lowercase().starts_with("zh") {
        LANG_ZH_HANS
    } else {
        LANG_EN
    }
}

/// The first entry of this Mac's preferred language list, if it has one.
fn preferred_language_tag() -> Option<String> {
    let languages = CFLocale::preferred_languages()?;
    if languages.count() < 1 {
        return None;
    }
    // SAFETY: the array is non-empty, so index 0 is in bounds, and
    // `CFLocaleCopyPreferredLanguages` is documented to return CFStrings; the
    // borrow lives no longer than the array we still own.
    let first = unsafe { languages.value_at_index(0) };
    if first.is_null() {
        return None;
    }
    let first: &CFType = unsafe { &*first.cast::<CFType>() };
    Some(first.downcast_ref::<CFString>()?.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    /// Bring the screen and the following devices back when the trigger device
    /// bounces back to this Mac right after a switch away. Configs written
    /// before this option existed get the current default, which is on.
    #[serde(default = "Options::default_switch_back")]
    pub switch_back_on_reconnect: bool,
    /// Pull the screen and the following devices back when the trigger device
    /// arrives on this Mac while this Mac's last switch handed everything to
    /// another host. Configs written before this option existed get the
    /// current default, which is on.
    #[serde(default = "Options::default_pull_on_arrival")]
    pub pull_on_arrival: bool,
    pub launch_at_login: bool,
    /// Which language the window and the tray speak. Configs written before
    /// this option existed get the current default, which follows the system.
    #[serde(default = "Options::default_language")]
    pub language: Language,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot locate the home directory (HOME is unset)")]
    NoHome,
    #[error("this_host {0} is not declared in hosts")]
    ThisHostNotDeclared(HostIndex),
    #[error("display '{display}' has no input for host {host}")]
    MissingInput { display: String, host: HostIndex },
    #[error("no device is marked is_trigger")]
    NoTriggerDevice,
    #[error("no device is marked follow")]
    NoFollowDevice,
    #[error("device '{id}' is marked both is_trigger and follow; pick one")]
    TriggerCannotFollow { id: DeviceId },
    #[error("device '{id}' is configured twice; every device row needs its own id")]
    DuplicateDevice { id: DeviceId },
    #[error(
        "display '{edid_uuid}' is configured twice; every display row needs its own edid_uuid"
    )]
    DuplicateDisplay { edid_uuid: String },
    #[error("display '{name}' is configured twice; every display row needs its own name")]
    DuplicateDisplayName { name: String },
    #[error(
        "display '{display}' gives hosts {first} and {second} the same input {code}; \
         the screen's input source has to name exactly one host"
    )]
    DuplicateDisplayInput {
        display: String,
        first: HostIndex,
        second: HostIndex,
        code: u8,
    },
    #[error("device '{device}' has leave_to {host}, which is not a declared host")]
    LeaveToNotDeclared { device: DeviceId, host: HostIndex },
    #[error(
        "device '{device}' has leave_to {host}, which is this_host; it cannot switch to itself"
    )]
    LeaveToIsThisHost { device: DeviceId, host: HostIndex },
    #[error("device '{device}' is a trigger and needs leave_to with more than two hosts")]
    MissingLeaveTo { device: DeviceId },
    #[error(
        "cooldown_ms {ms} is too short; at least {MIN_COOLDOWN_MS} ms is required between switches"
    )]
    CooldownTooShort { ms: u64 },
    #[error(
        "debounce_ms {ms} is too short; at least {MIN_DEBOUNCE_MS} ms is required before a switch"
    )]
    DebounceTooShort { ms: u64 },
    #[error("host {index} is not one of the declared hosts")]
    AdoptUnknownHost { index: HostIndex },
    #[error("with three hosts or more, the host to leave towards has to be chosen")]
    AdoptLeaveToRequired,
    #[error("the host to leave towards cannot be this machine")]
    AdoptLeaveToIsThisHost,
}

impl ConfigError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl Options {
    /// The serde default for [`Options::switch_back_on_reconnect`] (spec §6).
    pub fn default_switch_back() -> bool {
        true
    }

    /// The serde default for [`Options::pull_on_arrival`] (spec §6).
    pub fn default_pull_on_arrival() -> bool {
        true
    }

    /// The serde default for [`Options::language`] (spec §6).
    pub fn default_language() -> Language {
        Language::Auto
    }
}

impl Config {
    /// `~/Library/Application Support/Relay/config.json`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let home = std::env::var_os("HOME").ok_or(ConfigError::NoHome)?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Relay")
            .join("config.json"))
    }

    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = fs::read_to_string(path).map_err(|e| ConfigError::io(path, e))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Writes to a sibling `.tmp` file and renames it over `path`, so a reader
    /// never sees a half-written config.
    pub fn save_atomic(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| ConfigError::io(parent, e))?;
        }

        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);

        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        write_all_synced(&tmp, json.as_bytes()).map_err(|e| ConfigError::io(&tmp, e))?;
        fs::rename(&tmp, path).map_err(|e| ConfigError::io(path, e))
    }

    /// Is `index` one of the declared host slots? Nothing may be switched to a
    /// slot that is not (an undeclared slot may be unpaired).
    pub fn has_host(&self, index: HostIndex) -> bool {
        self.hosts.iter().any(|h| h.index == index)
    }

    pub fn device(&self, id: &DeviceId) -> Option<&DeviceConfig> {
        self.devices.iter().find(|d| &d.id == id)
    }

    /// Rejects everything the coordinator and the plan builder assume.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.has_host(self.this_host) {
            return Err(ConfigError::ThisHostNotDeclared(self.this_host));
        }

        for display in &self.displays {
            // The mapping is also read backwards — the monitor reports an
            // input source and the core asks which host that is (spec §5) —
            // so two hosts sharing one code on one display would make "is the
            // screen here?" unanswerable: this Mac would claim the screen
            // while it is on the other host, and quietly skip a pull it owes.
            let mut hosts_by_input: BTreeMap<u8, HostIndex> = BTreeMap::new();
            for host in &self.hosts {
                let Some(code) = display.input_for(host.index) else {
                    return Err(ConfigError::MissingInput {
                        display: display.name.clone(),
                        host: host.index,
                    });
                };
                if let Some(first) = hosts_by_input.insert(code, host.index) {
                    return Err(ConfigError::DuplicateDisplayInput {
                        display: display.name.clone(),
                        first,
                        second: host.index,
                        code,
                    });
                }
            }
        }

        // `ddc::pick` resolves a row to a connected display by `edid_uuid`,
        // ignoring case, and takes the first match; two rows sharing a uuid
        // would make the second one unreachable. An empty uuid is not an id at
        // all — it means "whichever display is first" — so it is exempt.
        let mut seen_displays: HashSet<String> = HashSet::with_capacity(self.displays.len());
        for display in &self.displays {
            if display.edid_uuid.is_empty() {
                continue;
            }
            if !seen_displays.insert(display.edid_uuid.to_ascii_lowercase()) {
                return Err(ConfigError::DuplicateDisplay {
                    edid_uuid: display.edid_uuid.clone(),
                });
            }
        }

        // `plan::build_plan` carries a display as its `name` alone and the
        // executor resolves that name back to the first matching display, so
        // two rows sharing a name — two identical monitors, which the scan UI
        // names after the same ProductName — would both drive one screen and
        // leave the other one on the old input.
        let mut seen_names: HashSet<&str> = HashSet::with_capacity(self.displays.len());
        for display in &self.displays {
            if !seen_names.insert(display.name.trim()) {
                return Err(ConfigError::DuplicateDisplayName {
                    name: display.name.clone(),
                });
            }
        }

        if self.timing.cooldown_ms < MIN_COOLDOWN_MS {
            return Err(ConfigError::CooldownTooShort {
                ms: self.timing.cooldown_ms,
            });
        }
        if self.timing.debounce_ms < MIN_DEBOUNCE_MS {
            return Err(ConfigError::DebounceTooShort {
                ms: self.timing.debounce_ms,
            });
        }

        if !self.devices.iter().any(|d| d.is_trigger) {
            return Err(ConfigError::NoTriggerDevice);
        }
        if !self.devices.iter().any(|d| d.follow) {
            return Err(ConfigError::NoFollowDevice);
        }

        // The two roles are exclusive per device: a trigger stays on this Mac
        // to notice the next leave, while a follower is handed to the target
        // host. One device cannot do both.
        if let Some(device) = self.devices.iter().find(|d| d.is_trigger && d.follow) {
            return Err(ConfigError::TriggerCannotFollow {
                id: device.id.clone(),
            });
        }

        // The plan builder carries a device as its `id` alone and the executor
        // resolves that id back to the first matching device, so two rows
        // sharing an id — differing only by `serial` — would send `ChangeHost`
        // to the same unit twice and never reach the other one.
        let mut seen: HashSet<&DeviceId> = HashSet::with_capacity(self.devices.len());
        for device in &self.devices {
            if !seen.insert(&device.id) {
                return Err(ConfigError::DuplicateDevice {
                    id: device.id.clone(),
                });
            }
        }

        for device in &self.devices {
            match device.leave_to {
                Some(host) if !self.has_host(host) => {
                    return Err(ConfigError::LeaveToNotDeclared {
                        device: device.id.clone(),
                        host,
                    })
                }
                Some(host) if host == self.this_host => {
                    return Err(ConfigError::LeaveToIsThisHost {
                        device: device.id.clone(),
                        host,
                    })
                }
                // With three hosts or more the target of a leave cannot be inferred.
                None if device.is_trigger && self.hosts.len() > 2 => {
                    return Err(ConfigError::MissingLeaveTo {
                        device: device.id.clone(),
                    })
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Turns a config exported from another machine into this machine's.
    ///
    /// Only two things differ between the machines sharing one keyboard: which
    /// host this one is, and where a trigger device goes when it leaves. Every
    /// other field is shared by construction, so this copies nothing and moves
    /// nothing — it writes those two and stops.
    ///
    /// `leave_to` may be `None` only with exactly two hosts, where the answer
    /// is the other one. With three the file cannot know, and neither can we.
    pub fn adopt(
        &mut self,
        this_host: HostIndex,
        leave_to: Option<HostIndex>,
    ) -> Result<(), ConfigError> {
        let declared = |index: HostIndex| self.hosts.iter().any(|host| host.index == index);
        if !declared(this_host) {
            return Err(ConfigError::AdoptUnknownHost { index: this_host });
        }

        let exit = match leave_to {
            Some(index) if !declared(index) => {
                return Err(ConfigError::AdoptUnknownHost { index });
            }
            // Leaving towards the machine you are on is not leaving.
            Some(index) if index == this_host => return Err(ConfigError::AdoptLeaveToIsThisHost),
            Some(index) => index,
            None => {
                let mut others = self.hosts.iter().filter(|host| host.index != this_host);
                match (others.next(), others.next()) {
                    (Some(only), None) => only.index,
                    _ => return Err(ConfigError::AdoptLeaveToRequired),
                }
            }
        };

        self.this_host = this_host;
        for device in &mut self.devices {
            // A follow device is sent wherever the switch is going; it has no
            // exit of its own, and writing one would be a lie the UI reads back.
            device.leave_to = device.is_trigger.then_some(exit);
        }
        Ok(())
    }
}

fn write_all_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Configurations the `plan` and `coordinator` tests share.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) const KEYBOARD_ID: &str = "046d:b366";
    pub(crate) const MOUSE_ID: &str = "046d:b023";

    /// Two hosts, this machine is host 0, one display, a trigger keyboard and a
    /// following mouse.
    pub(crate) fn two_host_config() -> Config {
        Config {
            schema_version: SCHEMA_VERSION,
            this_host: 0,
            hosts: vec![
                Host {
                    index: 0,
                    name: "Bam.Work".to_string(),
                },
                Host {
                    index: 1,
                    name: "Bam.Mini".to_string(),
                },
            ],
            displays: vec![DisplayConfig {
                edid_uuid: "B9B87925-0000-0000-0000-000000000000".to_string(),
                name: "AOC U2790R3B".to_string(),
                input_by_host: BTreeMap::from([("0".to_string(), 17), ("1".to_string(), 18)]),
            }],
            devices: vec![
                DeviceConfig {
                    id: DeviceId(KEYBOARD_ID.to_string()),
                    name: "MX Mechanical".to_string(),
                    role: DeviceRole::Keyboard,
                    transport: "ble".to_string(),
                    serial: None,
                    is_trigger: true,
                    follow: false,
                    leave_to: None,
                },
                DeviceConfig {
                    id: DeviceId(MOUSE_ID.to_string()),
                    name: "MX Master 3".to_string(),
                    role: DeviceRole::Mouse,
                    transport: "ble".to_string(),
                    serial: None,
                    is_trigger: false,
                    follow: true,
                    leave_to: None,
                },
            ],
            timing: Timing {
                debounce_ms: 800,
                cooldown_ms: 5000,
                ddc_retries: 3,
            },
            hotkeys: BTreeMap::from([
                ("0".to_string(), "Ctrl+Alt+1".to_string()),
                ("1".to_string(), "Ctrl+Alt+2".to_string()),
            ]),
            options: Options {
                switch_back_on_reconnect: true,
                pull_on_arrival: true,
                launch_at_login: true,
                language: Language::Auto,
            },
        }
    }

    /// The same, plus a third host; the keyboard names host 2 as its leave target.
    pub(crate) fn three_host_config() -> Config {
        let mut cfg = two_host_config();
        cfg.hosts.push(Host {
            index: 2,
            name: "Bam.Studio".to_string(),
        });
        cfg.displays[0].input_by_host.insert("2".to_string(), 27);
        cfg.devices[0].leave_to = Some(2);
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{three_host_config, two_host_config};
    use super::*;

    /// The example from `docs/specs/relay-core.md` §6 (device ids concretized to
    /// the M0 `vid:pid` form).
    const SPEC_EXAMPLE: &str = r#"{
  "schema_version": 1,
  "this_host": 0,
  "hosts": [ { "index": 0, "name": "Bam.Work" }, { "index": 1, "name": "Bam.Mini" } ],
  "displays": [
    { "edid_uuid": "B9B87925-0000-0000-0000-000000000000", "name": "AOC U2790R3B", "input_by_host": { "0": 17, "1": 18 } }
  ],
  "devices": [
    { "id": "046d:b366", "name": "MX Mechanical", "role": "keyboard", "transport": "ble",
      "is_trigger": true,  "follow": false, "leave_to": null },
    { "id": "046d:b023", "name": "MX Master 3",   "role": "mouse",    "transport": "ble",
      "is_trigger": false, "follow": true }
  ],
  "timing":  { "debounce_ms": 800, "cooldown_ms": 5000, "ddc_retries": 3 },
  "hotkeys": { "0": "Ctrl+Alt+1", "1": "Ctrl+Alt+2" },
  "options": { "switch_back_on_reconnect": true, "pull_on_arrival": true, "launch_at_login": true,
               "language": "auto" }
}"#;

    fn spec_config() -> Config {
        serde_json::from_str(SPEC_EXAMPLE).expect("spec example must parse")
    }

    #[test]
    fn spec_example_parses_with_expected_fields() {
        let cfg = spec_config();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.this_host, 0);
        assert_eq!(cfg.hosts.len(), 2);
        assert_eq!(cfg.hosts[1].index, 1);
        assert_eq!(cfg.hosts[1].name, "Bam.Mini");
        assert_eq!(cfg.displays[0].name, "AOC U2790R3B");
        assert_eq!(cfg.displays[0].input_for(0), Some(17));
        assert_eq!(cfg.displays[0].input_for(1), Some(18));
        assert_eq!(cfg.displays[0].input_for(2), None);
        assert_eq!(cfg.devices[0].id, DeviceId("046d:b366".to_string()));
        assert_eq!(cfg.devices[0].role, DeviceRole::Keyboard);
        assert_eq!(cfg.devices[0].transport, "ble");
        assert!(cfg.devices[0].is_trigger);
        assert!(!cfg.devices[0].follow);
        assert_eq!(cfg.devices[0].leave_to, None);
        assert_eq!(cfg.devices[1].role, DeviceRole::Mouse);
        assert_eq!(cfg.devices[1].leave_to, None);
        assert_eq!(cfg.timing.debounce_ms, 800);
        assert_eq!(cfg.timing.cooldown_ms, 5000);
        assert_eq!(cfg.timing.ddc_retries, 3);
        assert_eq!(cfg.hotkeys.get("1").map(String::as_str), Some("Ctrl+Alt+2"));
        assert!(cfg.options.switch_back_on_reconnect);
        assert!(cfg.options.launch_at_login);
        cfg.validate().expect("spec example must be valid");
    }

    #[test]
    fn an_options_block_without_switch_back_reads_as_on() {
        // Configs written before the option existed must get the new default.
        let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        value["options"] = serde_json::json!({ "launch_at_login": true });
        let cfg: Config = serde_json::from_value(value).expect("an old options block must parse");
        assert!(cfg.options.switch_back_on_reconnect);
    }

    #[test]
    fn an_options_block_without_pull_on_arrival_reads_as_on() {
        // Configs written before the option existed must get the new default.
        let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        value["options"] =
            serde_json::json!({ "switch_back_on_reconnect": true, "launch_at_login": true });
        let cfg: Config = serde_json::from_value(value).expect("an old options block must parse");
        assert!(cfg.options.pull_on_arrival);
    }

    #[test]
    fn an_explicit_pull_on_arrival_false_is_respected() {
        let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        value["options"]["pull_on_arrival"] = serde_json::Value::Bool(false);
        let cfg: Config = serde_json::from_value(value).expect("parse");
        assert!(!cfg.options.pull_on_arrival);
    }

    #[test]
    fn an_explicit_switch_back_false_is_respected() {
        let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        value["options"]["switch_back_on_reconnect"] = serde_json::Value::Bool(false);
        let cfg: Config = serde_json::from_value(value).expect("parse");
        assert!(!cfg.options.switch_back_on_reconnect);
    }

    #[test]
    fn an_options_block_without_language_follows_the_system() {
        // Configs written before the option existed must get the new default.
        let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        value["options"] = serde_json::json!({ "launch_at_login": true });
        let cfg: Config = serde_json::from_value(value).expect("an old options block must parse");
        assert_eq!(cfg.options.language, Language::Auto);
    }

    #[test]
    fn every_language_round_trips_through_its_json_tag() {
        for (tag, language) in [
            ("auto", Language::Auto),
            ("zh-Hans", Language::ZhHans),
            ("en", Language::En),
        ] {
            let mut value: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
            value["options"]["language"] = serde_json::Value::String(tag.to_string());
            let cfg: Config = serde_json::from_value(value).expect("parse");
            assert_eq!(cfg.options.language, language);

            let written: serde_json::Value =
                serde_json::from_str(&serde_json::to_string(&cfg).expect("serialize")).unwrap();
            assert_eq!(written["options"]["language"], serde_json::json!(tag));
        }
    }

    #[test]
    fn an_explicit_language_resolves_to_itself() {
        // Only `Auto` may consult the machine, so these two are the same
        // everywhere the tests run.
        assert_eq!(Language::ZhHans.resolve(), LANG_ZH_HANS);
        assert_eq!(Language::En.resolve(), LANG_EN);
    }

    #[test]
    fn auto_resolves_to_a_concrete_language() {
        // Whatever this machine prefers, `Auto` must never reach a UI.
        let resolved = Language::Auto.resolve();
        assert!(
            resolved == LANG_ZH_HANS || resolved == LANG_EN,
            "auto resolved to {resolved}"
        );
    }

    #[test]
    fn any_chinese_tag_is_chinese_and_everything_else_is_english() {
        for tag in [
            "zh",
            "zh-Hans",
            "zh-Hans-CN",
            "zh-Hant-TW",
            "ZH-hans",
            " zh-CN ",
        ] {
            assert_eq!(tag_language(tag), LANG_ZH_HANS, "{tag}");
        }
        for tag in ["en", "en-US", "ja-JP", "de", "", "nonsense"] {
            assert_eq!(tag_language(tag), LANG_EN, "{tag}");
        }
    }

    #[test]
    fn spec_example_round_trips_through_json() {
        let cfg = spec_config();
        let json = serde_json::to_string(&cfg).expect("serialize");
        let again: Config = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cfg, again);

        // Field names must survive the trip: compare the JSON trees key by key,
        // ignoring the `leave_to` the spec example omits on the second device.
        let mut original: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        original["devices"][1]["leave_to"] = serde_json::Value::Null;
        let produced: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(original, produced);
    }

    #[test]
    fn a_config_without_serials_keeps_the_field_out_of_the_json() {
        // Old configs have no `serial`; reading one must not invent the key,
        // and writing it back must not add it.
        let cfg = spec_config();
        assert_eq!(cfg.devices[0].serial, None);
        let json = serde_json::to_string(&cfg).expect("serialize");
        assert!(!json.contains("serial"), "unexpected serial key in {json}");
    }

    #[test]
    fn a_device_serial_round_trips() {
        let mut cfg = spec_config();
        cfg.devices[1].serial = Some("A1B2C3".to_string());
        let json = serde_json::to_string(&cfg).expect("serialize");
        let again: Config = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(again.devices[1].serial.as_deref(), Some("A1B2C3"));
        assert_eq!(cfg, again);
    }

    #[test]
    fn this_host_must_be_declared() {
        let mut cfg = spec_config();
        cfg.this_host = 7;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::ThisHostNotDeclared(7))
        ));
    }

    #[test]
    fn every_display_needs_an_input_for_every_host() {
        let mut cfg = spec_config();
        cfg.displays[0].input_by_host.remove("1");
        match cfg.validate() {
            Err(ConfigError::MissingInput { display, host }) => {
                assert_eq!(display, "AOC U2790R3B");
                assert_eq!(host, 1);
            }
            other => panic!("expected MissingInput, got {other:?}"),
        }
    }

    #[test]
    fn at_least_one_trigger_device_is_required() {
        let mut cfg = spec_config();
        cfg.devices[0].is_trigger = false;
        assert!(matches!(cfg.validate(), Err(ConfigError::NoTriggerDevice)));
    }

    #[test]
    fn at_least_one_follow_device_is_required() {
        let mut cfg = spec_config();
        cfg.devices[1].follow = false;
        assert!(matches!(cfg.validate(), Err(ConfigError::NoFollowDevice)));
    }

    #[test]
    fn a_device_cannot_be_both_a_trigger_and_a_follower() {
        let mut cfg = spec_config();
        cfg.validate().expect("one of each role is valid");

        cfg.devices[1].is_trigger = true;
        match cfg.validate() {
            Err(ConfigError::TriggerCannotFollow { id }) => {
                assert_eq!(id, DeviceId("046d:b023".to_string()));
            }
            other => panic!("expected TriggerCannotFollow, got {other:?}"),
        }

        // A second trigger that does not follow is fine.
        cfg.devices[1].follow = false;
        cfg.devices.push(DeviceConfig {
            id: DeviceId("046d:b024".to_string()),
            name: "MX Master 3S".to_string(),
            role: DeviceRole::Mouse,
            transport: "ble".to_string(),
            serial: None,
            is_trigger: false,
            follow: true,
            leave_to: None,
        });
        cfg.validate()
            .expect("two triggers and a follower are valid");
    }

    #[test]
    fn two_devices_cannot_share_an_id() {
        // A second unit of the same model is told apart by `serial`, but the
        // plan and the executor only carry the `id`, so such a pair has to be
        // rejected rather than silently switched twice.
        let mut cfg = spec_config();
        let mut twin = cfg.devices[1].clone();
        twin.serial = Some("BBBB".to_string());
        cfg.devices[1].serial = Some("AAAA".to_string());
        cfg.devices.push(twin);
        match cfg.validate() {
            Err(ConfigError::DuplicateDevice { id }) => {
                assert_eq!(id, DeviceId("046d:b023".to_string()));
            }
            other => panic!("expected DuplicateDevice, got {other:?}"),
        }

        cfg.devices[2].id = DeviceId("046d:b024".to_string());
        cfg.validate().expect("distinct ids are valid");
    }

    #[test]
    fn two_displays_cannot_share_an_edid_uuid() {
        // `ddc::pick` matches the uuid ignoring case and stops at the first
        // hit, so a lowercase twin of an existing row is still the same
        // display and would leave the second row unreachable.
        let mut cfg = spec_config();
        let mut twin = cfg.displays[0].clone();
        twin.edid_uuid = twin.edid_uuid.to_ascii_lowercase();
        twin.name = "AOC twin".to_string();
        cfg.displays.push(twin);
        match cfg.validate() {
            Err(ConfigError::DuplicateDisplay { edid_uuid }) => {
                assert_eq!(edid_uuid, cfg.displays[0].edid_uuid.to_ascii_lowercase());
            }
            other => panic!("expected DuplicateDisplay, got {other:?}"),
        }

        cfg.displays[1].edid_uuid = "B9B87925-0000-0000-0000-000000000001".to_string();
        cfg.validate().expect("distinct uuids are valid");

        // An empty uuid means "the first display", not an id, so a pair of
        // them is this rule's business no more than one of them is.
        cfg.displays[0].edid_uuid = String::new();
        cfg.displays[1].edid_uuid = String::new();
        cfg.validate().expect("empty uuids are not duplicates");
    }

    #[test]
    fn two_displays_cannot_share_a_name() {
        // Two identical monitors get the same ProductName from the scan, and
        // the plan and the executor only carry the name, so such a pair has to
        // be rejected rather than silently driving one screen twice.
        let mut cfg = spec_config();
        let mut twin = cfg.displays[0].clone();
        twin.edid_uuid = "B9B87925-0000-0000-0000-000000000001".to_string();
        twin.name = format!(" {} ", cfg.displays[0].name);
        cfg.displays.push(twin);
        match cfg.validate() {
            Err(ConfigError::DuplicateDisplayName { name }) => {
                assert_eq!(name.trim(), "AOC U2790R3B");
            }
            other => panic!("expected DuplicateDisplayName, got {other:?}"),
        }

        cfg.displays[1].name = "AOC twin".to_string();
        cfg.validate().expect("distinct names are valid");
    }

    #[test]
    fn two_hosts_cannot_share_one_input_on_the_same_display() {
        // The pull decision reads the mapping backwards, so a shared code
        // would let this Mac answer "the screen is here" while it is on the
        // other host.
        let mut cfg = spec_config();
        let this_hosts_input = cfg.displays[0].input_for(0).expect("host 0");
        cfg.displays[0]
            .input_by_host
            .insert("1".to_string(), this_hosts_input);
        match cfg.validate() {
            Err(ConfigError::DuplicateDisplayInput {
                display,
                first,
                second,
                code,
            }) => {
                assert_eq!(display, "AOC U2790R3B");
                assert_eq!((first, second, code), (0, 1, 17));
            }
            other => panic!("expected DuplicateDisplayInput, got {other:?}"),
        }

        cfg.displays[0].input_by_host.insert("1".to_string(), 18);
        cfg.validate().expect("distinct inputs are valid");

        // Two displays may of course put their own hosts on the same code:
        // the clash is only within one screen.
        let mut twin = cfg.displays[0].clone();
        twin.edid_uuid = "B9B87925-0000-0000-0000-000000000001".to_string();
        twin.name = "AOC twin".to_string();
        cfg.displays.push(twin);
        cfg.validate()
            .expect("a second display may reuse the same codes");
    }

    #[test]
    fn leave_to_must_be_a_declared_host() {
        let mut cfg = spec_config();
        cfg.devices[0].leave_to = Some(9);
        match cfg.validate() {
            Err(ConfigError::LeaveToNotDeclared { device, host }) => {
                assert_eq!(device, DeviceId("046d:b366".to_string()));
                assert_eq!(host, 9);
            }
            other => panic!("expected LeaveToNotDeclared, got {other:?}"),
        }
    }

    #[test]
    fn leave_to_cannot_be_this_host() {
        let mut cfg = test_support::three_host_config();
        cfg.devices[0].leave_to = Some(cfg.this_host);
        match cfg.validate() {
            Err(ConfigError::LeaveToIsThisHost { device, host }) => {
                assert_eq!(device, DeviceId(test_support::KEYBOARD_ID.to_string()));
                assert_eq!(host, 0);
            }
            other => panic!("expected LeaveToIsThisHost, got {other:?}"),
        }
    }

    #[test]
    fn three_hosts_require_leave_to_on_every_trigger() {
        let mut cfg = spec_config();
        cfg.hosts.push(Host {
            index: 2,
            name: "Bam.Studio".to_string(),
        });
        cfg.displays[0].input_by_host.insert("2".to_string(), 27);
        match cfg.validate() {
            Err(ConfigError::MissingLeaveTo { device }) => {
                assert_eq!(device, DeviceId("046d:b366".to_string()));
            }
            other => panic!("expected MissingLeaveTo, got {other:?}"),
        }

        cfg.devices[0].leave_to = Some(1);
        cfg.validate().expect("trigger with leave_to is valid");
    }

    #[test]
    fn a_cooldown_under_one_second_is_rejected() {
        let mut cfg = spec_config();
        cfg.timing.cooldown_ms = 999;
        match cfg.validate() {
            Err(ConfigError::CooldownTooShort { ms }) => assert_eq!(ms, 999),
            other => panic!("expected CooldownTooShort, got {other:?}"),
        }

        cfg.timing.cooldown_ms = MIN_COOLDOWN_MS;
        cfg.validate().expect("exactly the minimum is allowed");
    }

    #[test]
    fn a_debounce_under_a_tenth_of_a_second_is_rejected() {
        let mut cfg = spec_config();
        cfg.timing.debounce_ms = 99;
        match cfg.validate() {
            Err(ConfigError::DebounceTooShort { ms }) => assert_eq!(ms, 99),
            other => panic!("expected DebounceTooShort, got {other:?}"),
        }

        cfg.timing.debounce_ms = MIN_DEBOUNCE_MS;
        cfg.validate().expect("exactly the minimum is allowed");
    }

    #[test]
    fn save_atomic_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        let cfg = spec_config();
        cfg.save_atomic(&path).expect("save");
        let loaded = Config::load(&path).expect("load");
        assert_eq!(cfg, loaded);

        // The temporary file must not survive a successful save.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != "config.json")
            .collect();
        assert!(leftovers.is_empty(), "unexpected leftovers: {leftovers:?}");
    }

    #[test]
    fn save_atomic_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        spec_config().save_atomic(&path).expect("first save");

        let mut cfg = spec_config();
        cfg.this_host = 1;
        cfg.save_atomic(&path).expect("second save");

        assert_eq!(Config::load(&path).expect("load").this_host, 1);
    }

    #[test]
    fn load_reports_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Config::load(&dir.path().join("absent.json")),
            Err(ConfigError::Io { .. })
        ));
    }

    #[test]
    fn default_path_is_under_application_support() {
        let path = Config::default_path().expect("HOME is set in tests");
        assert!(
            path.ends_with("Library/Application Support/Relay/config.json"),
            "unexpected default path: {}",
            path.display()
        );
    }

    #[test]
    fn adopting_sets_this_host_and_the_trigger_s_exit() {
        let mut cfg = three_host_config();
        cfg.adopt(2, Some(1)).expect("adopt");
        assert_eq!(cfg.this_host, 2);
        let trigger = cfg
            .devices
            .iter()
            .find(|d| d.is_trigger)
            .expect("a trigger");
        assert_eq!(trigger.leave_to, Some(1));
    }

    #[test]
    fn a_follow_device_never_gets_an_exit() {
        let mut cfg = three_host_config();
        // A config exported from another Mac can carry an exit on a follower; it
        // describes that machine, not this one, so adopting has to clear it.
        cfg.devices[1].leave_to = Some(0);
        assert!(!cfg.devices[1].is_trigger, "devices[1] is the follower");
        cfg.adopt(2, Some(1)).expect("adopt");
        for device in cfg.devices.iter().filter(|d| !d.is_trigger) {
            assert_eq!(device.leave_to, None);
        }
    }

    #[test]
    fn with_two_hosts_the_exit_is_the_other_one() {
        let mut cfg = two_host_config(); // hosts 0 and 1
        cfg.adopt(1, None).expect("adopt");
        let trigger = cfg
            .devices
            .iter()
            .find(|d| d.is_trigger)
            .expect("a trigger");
        assert_eq!(trigger.leave_to, Some(0));
    }

    #[test]
    fn with_three_hosts_an_exit_must_be_given() {
        let mut cfg = three_host_config();
        assert!(matches!(
            cfg.adopt(2, None),
            Err(ConfigError::AdoptLeaveToRequired)
        ));
    }

    #[test]
    fn a_host_that_is_not_declared_cannot_be_adopted() {
        let mut cfg = three_host_config();
        assert!(matches!(
            cfg.adopt(9, Some(1)),
            Err(ConfigError::AdoptUnknownHost { index: 9 })
        ));
    }

    #[test]
    fn the_exit_must_be_a_declared_host_other_than_this_one() {
        let mut cfg = three_host_config();
        assert!(matches!(
            cfg.adopt(2, Some(2)),
            Err(ConfigError::AdoptLeaveToIsThisHost)
        ));
        assert!(matches!(
            cfg.adopt(2, Some(9)),
            Err(ConfigError::AdoptUnknownHost { index: 9 })
        ));
    }

    #[test]
    fn adopting_changes_nothing_else() {
        let before = three_host_config();
        let mut after = before.clone();
        after.adopt(2, Some(1)).expect("adopt");
        // Spell out the only two fields adopting may touch; everything else is
        // compared by the struct itself, so a field added later cannot slip past.
        let mut expected = before.clone();
        expected.this_host = 2;
        expected.devices[0].leave_to = Some(1);
        assert!(expected.devices[0].is_trigger, "devices[0] is the trigger");
        assert_eq!(after, expected);
    }

    #[test]
    fn an_adopted_config_passes_validation() {
        let mut cfg = three_host_config();
        cfg.adopt(2, Some(1)).expect("adopt");
        cfg.validate().expect("an adopted config must be usable");
    }
}
