//! Device discovery over HID++: enumerate every node that exposes
//! `ChangeHost` (0x1814).
//!
//! The settings window's "scan devices" button ends up here. Where
//! [`super::logitech`] is handed an identity and goes looking for it, this
//! walks every HID++ node the machine reports and asks each one whether it can
//! switch hosts at all, then describes the ones that can so the user can pick
//! them into the config.
//!
//! The same handle discipline `AGENTS.md` demands applies: a node is opened,
//! asked and closed before the next one is tried, and nothing is cached.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hidpp::channel::HidppChannel;
use hidpp::device::Device;
use hidpp::feature::change_host::ChangeHostFeature;
use hidpp::feature::device_information::DeviceInformationFeature;
use hidpp::feature::device_type_and_name::{DeviceType, DeviceTypeAndNameFeature};
use hidpp::feature::CreatableFeature;
use openlogi_hid::backend::{HidBackend, NodeInfo};
use serde::Serialize;
use tokio::time::Instant;

use super::logitech::{error_chain, open_change_host, PROBE_TIMEOUT};
use crate::types::{DeviceId, DeviceRole};

/// Budget for the descriptive half of one node: everything after the device has
/// already proved it speaks HID++ 2.0 and exposes `ChangeHost`.
///
/// Deliberately larger than [`PROBE_TIMEOUT`]: describing a node costs a dozen
/// round trips — the host info, two feature lookups, the name length plus a
/// chunk per 16 characters, the device info and the serial. A device that is
/// going to answer still answers each of those in tens of milliseconds, so this
/// only ever bites a node that went quiet halfway through.
const NODE_TIMEOUT: Duration = Duration::from_secs(3);

/// Budget for the whole scan, so the settings window cannot hang on a machine
/// full of silent HID++ nodes. Whatever was found by then is returned.
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// The three budgets one scan spends, staged from cheapest to dearest.
///
/// Staged because the two halves of a node visit cost wildly different things.
/// Ruling a node out — open it, ping it for a protocol version, ask `Root`
/// where `ChangeHost` lives — is two round trips, and most nodes on a real
/// machine are ruled out: a keyboard alone publishes several. Giving each of
/// those the descriptive budget is what lets a handful of silent nodes eat the
/// whole scan before the switchable device at the end of the list is ever
/// opened. So the probe gets a tight bound of its own, and only a node that has
/// already answered earns the longer one.
pub(crate) struct ScanBudget {
    /// Bounds `open_change_host`: the open, the version ping and the 0x1814
    /// lookup.
    probe: Duration,
    /// Bounds the descriptive phase: host info, name, type, device info, serial.
    node: Duration,
    /// Deadline for the whole scan, across every node.
    total: Duration,
}

impl Default for ScanBudget {
    fn default() -> Self {
        Self {
            probe: PROBE_TIMEOUT,
            node: NODE_TIMEOUT,
            total: SCAN_TIMEOUT,
        }
    }
}

/// What one probed node hands the descriptive phase: the channel it is spoken
/// to over, the HID++ 2.0 device on it, and its `ChangeHost` feature.
type Opened = (Arc<HidppChannel>, Device, Arc<ChangeHostFeature>);

/// One switchable device the scan found, as the settings window lists it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DiscoveredDevice {
    /// The `vid:pid` form `DeviceConfig.id` uses.
    pub id: DeviceId,
    /// What narrows the id when two identical devices are attached. `None`
    /// when neither the device nor its HID node reports one.
    pub serial: Option<String>,
    /// Marketing name; display only, it never takes part in matching.
    pub name: String,
    /// How many host slots the device has.
    pub host_count: u8,
    /// The slot it is on right now, 0-based as HID++ numbers them.
    pub current_host: u8,
    /// What the device says it is. Only a guess: the user still confirms it.
    pub role_guess: DeviceRole,
}

/// Every switchable Logitech device this machine's real HID stack can see.
///
/// Never fails: a scan that cannot enumerate, or a device that will not answer,
/// is an empty or shorter list plus a `debug!` line — the settings window shows
/// what was found, not an error nobody can act on.
pub async fn scan_switchable_devices() -> Vec<DiscoveredDevice> {
    scan_with_backend(openlogi_hid::host::backend()).await
}

/// The same scan over an arbitrary backend — the seam tests drive.
pub async fn scan_with_backend(backend: Arc<dyn HidBackend>) -> Vec<DiscoveredDevice> {
    scan_with_backend_and_budget(backend, &ScanBudget::default()).await
}

/// The scan proper, with its three budgets handed in so a test can shrink them
/// to something it can wait for.
pub(crate) async fn scan_with_backend_and_budget(
    backend: Arc<dyn HidBackend>,
    budget: &ScanBudget,
) -> Vec<DiscoveredDevice> {
    let deadline = Instant::now() + budget.total;

    let nodes = match backend.enumerate_hidpp().await {
        Ok(nodes) => nodes,
        Err(err) => {
            // The scan found nothing and it was not the devices' doing: the
            // user pressed "scan devices" and will get an empty list back, so
            // the reason has to be visible at the default log level.
            tracing::warn!(
                error = %error_chain(&err),
                "enumerate_hidpp failed; nothing to scan"
            );
            return Vec::new();
        }
    };

    let mut found: Vec<DiscoveredDevice> = Vec::new();
    let mut seen: HashSet<Fingerprint> = HashSet::new();

    for node in &nodes {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::debug!(
                nodes = nodes.len(),
                found = found.len(),
                "scan budget spent; returning what was described so far"
            );
            break;
        }

        // `describe` stages the probe and the descriptive budgets itself; this
        // wrapper only holds it to the scan's own deadline. Dropping the future
        // drops the channel it opened, which closes the OS handle before the
        // next node is tried.
        match tokio::time::timeout(remaining, describe(backend.as_ref(), node, budget)).await {
            Ok(Ok(device)) => {
                if seen.insert(fingerprint(&device)) {
                    found.push(device);
                } else {
                    tracing::debug!(node = %node.id, "another HID node of a device already found");
                }
            }
            Ok(Err(reason)) => {
                tracing::debug!(node = %node.id, reason = %reason, "not a switchable device")
            }
            Err(_elapsed) => tracing::debug!(
                node = %node.id,
                "the scan deadline passed while this node was being described"
            ),
        }
    }

    found
}

/// Opens one node and describes the device behind it, or says why it is not a
/// switchable device.
///
/// Only `ChangeHost` and its host info are required: a device that has 0x1814
/// but hides its name, serial or type is still switchable, so every other
/// lookup falls back to what the HID node itself reported.
async fn describe(
    backend: &dyn HidBackend,
    node: &NodeInfo,
    budget: &ScanBudget,
) -> Result<DiscoveredDevice, String> {
    let opened = match tokio::time::timeout(budget.probe, open_change_host(backend, node)).await {
        Ok(result) => result?,
        // A node that opens and then says nothing is ruled out on the cheap
        // budget, exactly like one that answered "no ChangeHost". Dropping the
        // probe future here drops whatever channel it had opened.
        Err(_elapsed) => {
            return Err(format!("silent for {} ms", budget.probe.as_millis()));
        }
    };

    // Only now, with the device having proved it answers and switches hosts,
    // is the longer budget worth spending. `opened` moves in, so a timeout
    // drops the channel with the future.
    match tokio::time::timeout(budget.node, describe_opened(node, opened)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(format!(
            "stopped answering after {} ms of description",
            budget.node.as_millis()
        )),
    }
}

/// Everything a node that already proved itself switchable is asked.
///
/// Takes the open handles by value: the caller bounds this with
/// [`ScanBudget::node`], and dropping the future has to close the channel.
async fn describe_opened(
    node: &NodeInfo,
    (_channel, mut device, change_host): Opened,
) -> Result<DiscoveredDevice, String> {
    let hosts = change_host
        .get_host_info()
        .await
        .map_err(|err| format!("get_host_info: {}", error_chain(&err)))?;

    let (name, role) = match optional_feature::<DeviceTypeAndNameFeature>(&mut device).await {
        Some(feature) => (read_name(&feature).await, read_role(&feature).await),
        None => (None, None),
    };
    let serial = match optional_feature::<DeviceInformationFeature>(&mut device).await {
        Some(feature) => read_serial(&feature).await,
        None => None,
    };

    Ok(DiscoveredDevice {
        id: DeviceId(format!("{:04x}:{:04x}", node.vendor_id, node.product_id)),
        serial: serial.or_else(|| node.serial_number.as_deref().and_then(clean)),
        name: name.unwrap_or_else(|| node.name.clone()),
        host_count: hosts.host_count,
        current_host: hosts.current_host,
        role_guess: role.unwrap_or(DeviceRole::Other),
    })
    // The channel, the device and both features drop here, before the caller's
    // timeout wrapper resolves.
}

/// Binds a feature the device may or may not have.
///
/// A device that simply lacks the feature is not a failure here, and neither is
/// a lookup that errors: both mean "ask the HID node instead".
async fn optional_feature<F: CreatableFeature>(device: &mut Device) -> Option<Arc<F>> {
    match device.root().get_feature(F::ID).await {
        Ok(Some(entry)) => Some(device.add_feature::<F>(entry.index)),
        Ok(None) => None,
        Err(err) => {
            tracing::debug!(
                feature = format!("{:#06x}", F::ID),
                error = %error_chain(&err),
                "feature lookup failed; falling back to the HID node"
            );
            None
        }
    }
}

/// The marketing name from `DeviceTypeAndName` (0x0005).
async fn read_name(feature: &DeviceTypeAndNameFeature) -> Option<String> {
    match feature.get_whole_device_name().await {
        Ok(name) => clean(&name),
        Err(err) => {
            tracing::debug!(error = %error_chain(&err), "cannot read the device name");
            None
        }
    }
}

/// The role guess from `DeviceTypeAndName::getDeviceType` (0x0005).
async fn read_role(feature: &DeviceTypeAndNameFeature) -> Option<DeviceRole> {
    match feature.get_device_type().await {
        Ok(device_type) => Some(role_of(device_type)),
        Err(err) => {
            tracing::debug!(error = %error_chain(&err), "cannot read the device type");
            None
        }
    }
}

/// The serial from `DeviceInformation` (0x0003).
///
/// `getSerialNumber` only exists from feature version 4, and answers
/// `InvalidFunctionId` before that, so the capability flag is checked first
/// exactly as the feature's own documentation asks.
async fn read_serial(feature: &DeviceInformationFeature) -> Option<String> {
    let info = match feature.get_device_info().await {
        Ok(info) => info,
        Err(err) => {
            tracing::debug!(error = %error_chain(&err), "cannot read the device info");
            return None;
        }
    };
    if !info.capabilities.serial_number {
        return None;
    }
    match feature.get_serial_number().await {
        Ok(serial) => clean(&serial),
        Err(err) => {
            tracing::debug!(error = %error_chain(&err), "cannot read the serial number");
            None
        }
    }
}

/// What this device is for, as far as the scan can tell.
///
/// Only the two roles Relay acts on are named; everything else — a receiver, a
/// headset, a presenter — is [`DeviceRole::Other`] and the user picks.
fn role_of(device_type: DeviceType) -> DeviceRole {
    match device_type {
        DeviceType::Keyboard => DeviceRole::Keyboard,
        // A trackball is a mouse that does not move: it is the same pointer to
        // everything above this layer, and Logitech's own switchable trackballs
        // (the MX Ergo) report themselves as one.
        DeviceType::Mouse | DeviceType::Trackpad | DeviceType::Trackball => DeviceRole::Mouse,
        _ => DeviceRole::Other,
    }
}

/// A string the device sent, or `None` when there is nothing in it.
///
/// `getSerialNumber` hands back a fixed 12-byte field and `getDeviceName` a
/// fixed-width chunk, so anything shorter arrives padded with NULs or spaces.
fn clean(raw: &str) -> Option<String> {
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// What makes two HID nodes the same physical device.
///
/// A serial is the real answer: one device publishes several HID++ nodes and
/// every one of them reports the same serial. Without a serial the marketing
/// name is the best tiebreak left — it merges two identical unserialised
/// devices, which the settings window could not have told apart anyway, and
/// that is better than listing one device several times, which is what every
/// Logitech device would otherwise do.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Fingerprint {
    Serial(DeviceId, String),
    Name(DeviceId, String),
}

fn fingerprint(device: &DiscoveredDevice) -> Fingerprint {
    match device.serial.as_deref() {
        // A serial is free-form text the device formats as it likes, and
        // `logitech`'s node matching already treats it case-insensitively; two
        // spellings of one serial must not become two rows here either.
        Some(serial) => Fingerprint::Serial(device.id.clone(), serial.to_ascii_uppercase()),
        None => Fingerprint::Name(device.id.clone(), device.name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use openlogi_fixture::CassetteExchange;
    use openlogi_hid::replay::{ReplayBackend, ReplayTopology};

    use super::*;
    use crate::device::replay_support::{
        cassette, exchange, get_feature, get_host_info, long, no_feature, ping, replay_channel,
        replay_node, short, silent_ping, CHANGE_HOST_INDEX, VID,
    };

    const PID: u16 = 0xb023;
    /// Feature indices the scripted devices report.
    const TYPE_AND_NAME_INDEX: u8 = 0x02;
    const DEVICE_INFO_INDEX: u8 = 0x03;

    /// A 16-byte long-report payload carrying `value`, NUL-padded the way a
    /// device pads its fixed-width text fields.
    fn text(value: &str) -> [u8; 16] {
        let mut payload = [0u8; 16];
        payload[..value.len()].copy_from_slice(value.as_bytes());
        payload
    }

    /// `DeviceTypeAndName::getDeviceNameCount` then the single chunk that
    /// covers a name of at most 16 characters.
    fn device_name(name: &str) -> Vec<CassetteExchange> {
        vec![
            exchange(
                short(TYPE_AND_NAME_INDEX, 0x00, [0, 0, 0]),
                Some(short(
                    TYPE_AND_NAME_INDEX,
                    0x00,
                    [
                        u8::try_from(name.len()).expect("test name fits a byte"),
                        0,
                        0,
                    ],
                )),
            ),
            exchange(
                short(TYPE_AND_NAME_INDEX, 0x10, [0, 0, 0]),
                Some(long(TYPE_AND_NAME_INDEX, 0x10, text(name))),
            ),
        ]
    }

    fn device_type(device_type: DeviceType) -> CassetteExchange {
        exchange(
            short(TYPE_AND_NAME_INDEX, 0x20, [0, 0, 0]),
            Some(short(
                TYPE_AND_NAME_INDEX,
                0x20,
                [u8::from(device_type), 0, 0],
            )),
        )
    }

    /// `DeviceInformation::getDeviceInfo`, with the serial capability flag in
    /// payload byte 14, then the serial itself.
    fn device_serial(serial: &str) -> Vec<CassetteExchange> {
        let mut info = [0u8; 16];
        info[14] = 0x01;
        vec![
            exchange(
                short(DEVICE_INFO_INDEX, 0x00, [0, 0, 0]),
                Some(long(DEVICE_INFO_INDEX, 0x00, info)),
            ),
            exchange(
                short(DEVICE_INFO_INDEX, 0x20, [0, 0, 0]),
                Some(long(DEVICE_INFO_INDEX, 0x20, text(serial))),
            ),
        ]
    }

    /// Everything a fully cooperative device answers during a scan.
    fn switchable(name: &str, serial: &str, device_type: DeviceType) -> Vec<CassetteExchange> {
        let mut exchanges = vec![
            ping(),
            get_feature(0x1814, CHANGE_HOST_INDEX),
            get_host_info(3, 1),
            get_feature(0x0005, TYPE_AND_NAME_INDEX),
            get_feature(0x0003, DEVICE_INFO_INDEX),
        ];
        exchanges.extend(device_name(name));
        exchanges.push(self::device_type(device_type));
        exchanges.extend(device_serial(serial));
        exchanges
    }

    /// A device with `ChangeHost` but neither 0x0005 nor 0x0003.
    fn switchable_but_mute() -> Vec<CassetteExchange> {
        vec![
            ping(),
            get_feature(0x1814, CHANGE_HOST_INDEX),
            get_host_info(2, 0),
            no_feature(0x0005),
            no_feature(0x0003),
        ]
    }

    /// A HID++ node that is not a switchable device at all.
    fn not_switchable() -> Vec<CassetteExchange> {
        vec![ping(), no_feature(0x1814)]
    }

    /// One node of a scripted topology: how it enumerates, and what the device
    /// behind it answers.
    struct Scripted {
        id: &'static str,
        pid: u16,
        /// What the HID node itself reports — the scan's fallback, so it is a
        /// name no test should see unless the device stayed mute.
        node_name: &'static str,
        node_serial: Option<&'static str>,
        channel: &'static str,
        exchanges: Vec<CassetteExchange>,
    }

    impl Scripted {
        fn new(
            id: &'static str,
            pid: u16,
            channel: &'static str,
            exchanges: Vec<CassetteExchange>,
        ) -> Self {
            Self {
                id,
                pid,
                node_name: "Node name nobody should see",
                node_serial: None,
                channel,
                exchanges,
            }
        }

        /// What the HID node reports about the device, for the fallbacks.
        fn reported_as(mut self, name: &'static str, serial: Option<&'static str>) -> Self {
            self.node_name = name;
            self.node_serial = serial;
            self
        }
    }

    /// One `ReplayBackend` over `nodes`, enumerated in the order given.
    fn backend(nodes: Vec<Scripted>) -> Arc<ReplayBackend> {
        let mut topology = ReplayTopology {
            nodes: Vec::new(),
            channels: Vec::new(),
        };
        let mut cassettes = Vec::new();
        for scripted in nodes {
            let info = crate::device::replay_support::node(
                VID,
                scripted.pid,
                scripted.id,
                scripted.node_name,
                scripted.node_serial,
            );
            topology.nodes.push(replay_node(info, scripted.channel));
            topology.channels.push(replay_channel(scripted.channel));
            cassettes.push(cassette(scripted.channel, scripted.exchanges));
        }
        Arc::new(ReplayBackend::new(topology, cassettes).expect("replay topology is valid"))
    }

    fn scan(
        backend: &Arc<ReplayBackend>,
    ) -> impl std::future::Future<Output = Vec<DiscoveredDevice>> {
        scan_with_backend(Arc::clone(backend) as Arc<dyn HidBackend>)
    }

    /// The same scan on budgets a test can wait out: the probe is the one that
    /// has to bite, so it is the one made tiny.
    async fn scan_with_probe_budget(
        backend: &Arc<ReplayBackend>,
        probe: Duration,
    ) -> Vec<DiscoveredDevice> {
        scan_with_backend_and_budget(
            Arc::clone(backend) as Arc<dyn HidBackend>,
            &ScanBudget {
                probe,
                ..ScanBudget::default()
            },
        )
        .await
    }

    // --- pure helpers -----------------------------------------------------

    #[test]
    fn only_pointing_and_typing_devices_get_a_role() {
        assert_eq!(role_of(DeviceType::Keyboard), DeviceRole::Keyboard);
        assert_eq!(role_of(DeviceType::Mouse), DeviceRole::Mouse);
        assert_eq!(role_of(DeviceType::Trackpad), DeviceRole::Mouse);
        assert_eq!(role_of(DeviceType::Trackball), DeviceRole::Mouse);
        assert_eq!(role_of(DeviceType::Receiver), DeviceRole::Other);
        assert_eq!(role_of(DeviceType::Headset), DeviceRole::Other);
    }

    #[test]
    fn a_padded_fixed_width_field_loses_its_padding() {
        assert_eq!(clean("ABCD1234\0\0\0\0"), Some("ABCD1234".to_string()));
        assert_eq!(clean("  MX Keys  "), Some("MX Keys".to_string()));
        assert_eq!(clean("\0\0\0"), None);
        assert_eq!(clean(""), None);
    }

    // --- the scan itself --------------------------------------------------

    #[tokio::test]
    async fn a_scripted_device_is_described_from_its_own_answers() {
        let backend = backend(vec![Scripted::new(
            "node-a",
            PID,
            "mouse",
            switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
        )
        .reported_as("Node name nobody should see", Some("node-serial"))]);

        let found = scan(&backend).await;

        assert_eq!(
            found,
            vec![DiscoveredDevice {
                id: DeviceId("046d:b023".to_string()),
                serial: Some("ABCD1234EFGH".to_string()),
                name: "MX Master 3".to_string(),
                host_count: 3,
                current_host: 1,
                role_guess: DeviceRole::Mouse,
            }]
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn a_device_without_the_descriptive_features_falls_back_to_its_hid_node() {
        let backend = backend(vec![Scripted::new(
            "node-a",
            0xb366,
            "keyboard",
            switchable_but_mute(),
        )
        .reported_as("Craft Keyboard", Some("NODE-SERIAL"))]);

        let found = scan(&backend).await;

        assert_eq!(
            found,
            vec![DiscoveredDevice {
                id: DeviceId("046d:b366".to_string()),
                serial: Some("NODE-SERIAL".to_string()),
                name: "Craft Keyboard".to_string(),
                host_count: 2,
                current_host: 0,
                // Nothing said what it is, and the scan does not guess from a name.
                role_guess: DeviceRole::Other,
            }]
        );
    }

    #[tokio::test]
    async fn a_node_without_change_host_is_skipped() {
        let backend = backend(vec![
            Scripted::new("node-plain", 0xc52b, "plain", not_switchable()),
            Scripted::new(
                "node-mouse",
                PID,
                "mouse",
                switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
            ),
        ]);

        let found = scan(&backend).await;

        assert_eq!(
            found.iter().map(|d| d.id.to_string()).collect::<Vec<_>>(),
            vec!["046d:b023"]
        );
        // The plain node's cassette scripts the ping and the 0x1814 lookup and
        // nothing else, so this also says the scan stopped there: a 0x0005 or
        // 0x0003 lookup on it would be an unmatched request.
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn a_silent_node_is_abandoned_on_the_probe_budget_alone() {
        // The regression: one node that opens and never answers used to hold
        // the whole per-node budget, and a machine with a few of them spent the
        // scan deadline before reaching the device the user was looking for.
        let backend = backend(vec![
            Scripted::new("node-silent", 0xc52b, "silent", vec![silent_ping()]),
            Scripted::new(
                "node-mouse",
                PID,
                "mouse",
                switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
            ),
        ]);

        let started = std::time::Instant::now();
        let found = scan_with_probe_budget(&backend, Duration::from_millis(50)).await;
        let elapsed = started.elapsed();

        assert_eq!(
            found.iter().map(|d| d.id.to_string()).collect::<Vec<_>>(),
            vec!["046d:b023"],
            "the node behind the silent one must still be described"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the silent node must cost its 50 ms probe budget, not the 3 s node \
             one; took {elapsed:?}"
        );
        assert_eq!(
            backend.channel_lifetime_count("silent").expect("known"),
            0,
            "abandoning the probe must close the channel it opened"
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn two_hid_nodes_of_one_device_are_listed_once() {
        // Every Logitech device publishes more than one HID++ node; the scan
        // must not turn that into two rows in the settings window.
        let backend = backend(vec![
            Scripted::new(
                "node-a",
                PID,
                "first",
                switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
            ),
            // The same device, reporting its serial in the other case.
            Scripted::new(
                "node-b",
                PID,
                "second",
                switchable("MX Master 3", "abcd1234efgh", DeviceType::Mouse),
            ),
        ]);

        let found = scan(&backend).await;

        assert_eq!(found.len(), 1, "one physical device, one row: {found:?}");
        assert_eq!(found[0].serial.as_deref(), Some("ABCD1234EFGH"));
    }

    #[tokio::test]
    async fn two_unserialised_nodes_of_one_device_are_listed_once() {
        let backend = backend(vec![
            Scripted::new("node-a", 0xb366, "first", switchable_but_mute())
                .reported_as("Craft Keyboard", None),
            Scripted::new("node-b", 0xb366, "second", switchable_but_mute())
                .reported_as("Craft Keyboard", None),
        ]);

        let found = scan(&backend).await;

        assert_eq!(found.len(), 1, "one physical device, one row: {found:?}");
        assert_eq!(found[0].serial, None);
    }

    #[tokio::test]
    async fn two_different_devices_are_both_listed_in_enumeration_order() {
        let backend = backend(vec![
            Scripted::new("node-kb", 0xb366, "keyboard", switchable_but_mute())
                .reported_as("Craft Keyboard", Some("KB-SERIAL")),
            Scripted::new(
                "node-mouse",
                PID,
                "mouse",
                switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
            ),
        ]);

        let found = scan(&backend).await;

        assert_eq!(
            found.iter().map(|d| d.id.to_string()).collect::<Vec<_>>(),
            vec!["046d:b366", "046d:b023"]
        );
    }

    #[tokio::test]
    async fn every_opened_channel_is_closed_before_the_scan_returns() {
        // AGENTS.md invariant 3: open → act → close, nothing cached. That has
        // to hold for the node that was skipped too.
        let backend = backend(vec![
            Scripted::new("node-plain", 0xc52b, "plain", not_switchable()),
            Scripted::new(
                "node-mouse",
                PID,
                "mouse",
                switchable("MX Master 3", "ABCD1234EFGH", DeviceType::Mouse),
            ),
        ]);

        assert_eq!(scan(&backend).await.len(), 1);

        for channel in ["plain", "mouse"] {
            assert_eq!(
                backend.channel_lifetime_count(channel).expect("known"),
                0,
                "{channel} must have been dropped before the scan returned"
            );
        }
    }

    #[tokio::test]
    async fn a_machine_with_no_hid_nodes_scans_to_an_empty_list() {
        let backend = backend(Vec::new());
        assert_eq!(scan(&backend).await, Vec::new());
    }
}
