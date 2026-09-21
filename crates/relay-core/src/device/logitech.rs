//! M1 device layer: switch hosts natively over HID++ `ChangeHost` (0x1814).
//!
//! Replaces the M0 `host-switch-tool` subprocess with direct calls through
//! `openlogi-hid` (enumerate/open the HID node) and `openlogi-hidpp` (the
//! protocol). The handle discipline `AGENTS.md` demands is kept literally:
//! every trait method enumerates, opens, acts and drops the channel before it
//! returns — nothing is cached between calls.

use std::error::Error as StdError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hidpp::channel::{ChannelError, HidppChannel};
use hidpp::device::Device;
use hidpp::feature::change_host::ChangeHostFeature;
use hidpp::feature::CreatableFeature;
use hidpp::protocol::v20::Hidpp20Error;
use openlogi_hid::backend::{HidBackend, NodeInfo};

use super::{DeviceError, HostSwitchable};
use crate::types::{DeviceId, HostIndex, HostInfo};

/// One enumerate → open → act round trip that takes longer than this means the
/// device is gone or wedged.
///
/// It has to leave room for several candidate probes: a Logitech device
/// publishes more than one HID node and each one costs up to [`PROBE_TIMEOUT`]
/// before it is ruled out, on top of an enumeration that alone can take a
/// second.
const OP_TIMEOUT: Duration = Duration::from_secs(8);

/// Budget for probing one candidate node.
///
/// openlogi bounds a single HID++ request by `HidppChannel::SEND_RESPONSE_TIMEOUT`
/// (5 s), which is the budget for the *whole* operation here. Without a
/// per-candidate bound the first node that opens but never answers consumes
/// everything and the node that would have worked is never tried. A device that
/// is going to answer answers in tens of milliseconds, so this is generous.
///
/// [`super::discovery`] spends the same budget on the same work — it rules a
/// node out through [`open_change_host`] too — so it reuses this constant
/// rather than keeping a second number that would drift from it.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// How much of [`OP_TIMEOUT`] is kept back from candidate probing.
///
/// Probing has to run out of budget *before* the operation does, so a device
/// whose every node stays silent is reported as [`DeviceError::NotFound`] ("no
/// node here exposes ChangeHost") and not as [`DeviceError::Timeout`] ("the
/// device is wedged") — and so the coordinator is not left in `Switching` for
/// several times [`OP_TIMEOUT`] across two devices. Without a margin the last
/// probe and the operation expire on the same instant and which one is observed
/// is a race.
const OPEN_MARGIN: Duration = Duration::from_millis(500);

/// HID++ device index for a device talking to us directly (BLE or USB cable)
/// rather than through a receiver.
pub(crate) const DIRECT_DEVICE_INDEX: u8 = 0xFF;

/// The two handles one opened node hands back: the channel and the feature
/// bound to it. The caller owns both and drops them at a point we can see.
type Opened = (Arc<HidppChannel>, Arc<ChangeHostFeature>);

pub struct LogitechHidpp {
    id: DeviceId,
    /// Narrows the match when several nodes share the `vid:pid`.
    serial: Option<String>,
    /// Display name only; it never takes part in matching (unlike M0).
    name: String,
    /// The HID backend to enumerate and open through.
    ///
    /// A backend is a factory, not a device handle: holding one caches nothing
    /// about the device, and the channel it hands out is still opened and
    /// dropped inside a single call. Keeping it as a field is what lets a test
    /// pass `openlogi_hid::replay::ReplayBackend` in through
    /// [`LogitechHidpp::with_backend`].
    backend: Arc<dyn HidBackend>,
}

impl LogitechHidpp {
    /// The device as this machine's real HID stack sees it.
    pub fn new(id: DeviceId, serial: Option<String>, name: String) -> Self {
        Self::with_backend(id, serial, name, openlogi_hid::host::backend())
    }

    /// The same device over an arbitrary backend — the seam tests drive.
    pub fn with_backend(
        id: DeviceId,
        serial: Option<String>,
        name: String,
        backend: Arc<dyn HidBackend>,
    ) -> Self {
        Self {
            id,
            serial,
            name,
            backend,
        }
    }

    /// How this device is named in a log line or an error message: the
    /// configured `vid:pid` plus the serial that narrows it, when there is one.
    fn identity(&self) -> String {
        match self.serial.as_deref().filter(|serial| !serial.is_empty()) {
            Some(serial) => format!("{} serial {serial}", self.id),
            None => self.id.to_string(),
        }
    }

    /// Enumerate, pick the candidate nodes, and open the first one that really
    /// speaks HID++ and exposes `ChangeHost`.
    ///
    /// Every caller wraps this in an [`OP_TIMEOUT`] the moment it starts, so
    /// the deadline taken here is that same budget less [`OPEN_MARGIN`]:
    /// enumeration and the probes together are held to it, and running out of
    /// it is [`DeviceError::NotFound`], not [`DeviceError::Timeout`].
    async fn open(&self) -> Result<Opened, DeviceError> {
        let deadline = tokio::time::Instant::now() + OP_TIMEOUT - OPEN_MARGIN;

        let nodes = self.backend.enumerate_hidpp().await.map_err(|err| {
            DeviceError::Tool(format!(
                "{}: enumerate_hidpp: {}",
                self.identity(),
                error_chain(&err)
            ))
        })?;

        let candidates = select_candidates(&nodes, &self.id, self.serial.as_deref());
        let mut outcomes: Vec<String> = Vec::with_capacity(candidates.len());

        for node in &candidates {
            // Each probe gets [`PROBE_TIMEOUT`], but never more than what is
            // left of the operation's own budget: enough silent nodes would
            // otherwise spend more than [`OP_TIMEOUT`] between them and the
            // caller would be told `Timeout` — "the device is wedged" — when
            // the truth is that no node here exposes `ChangeHost`.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                outcomes.push(format!(
                    "{} further node(s) not tried: the operation budget is spent",
                    candidates.len() - outcomes.len()
                ));
                break;
            }

            let budget = remaining.min(PROBE_TIMEOUT);

            match tokio::time::timeout(budget, self.open_node(node)).await {
                Ok(Ok(opened)) => return Ok(opened),
                Ok(Err(reason)) => outcomes.push(format!("{}: {reason}", node.id)),
                // A node that opens but never answers is simply not the HID++
                // node we want — the same verdict as one that answers "no".
                // Dropping the probe future here drops the channel it opened,
                // which closes the OS handle before the next candidate.
                Err(_elapsed) => {
                    outcomes.push(format!("{}: silent for {} ms", node.id, budget.as_millis()))
                }
            }
        }

        if candidates.is_empty() {
            // Routine, not a fault: on every automatic switch the other machine
            // may already hold the device, and then it publishes no node here.
            tracing::debug!(
                device = %self.identity(),
                hidpp_nodes = nodes.len(),
                "no HID++ node at this vid:pid"
            );
        } else {
            // The operation is about to fail with `NotFound`, which says
            // nothing about why. This line is the only record of what each
            // candidate answered, and it fires at most once per failed
            // operation, so it earns the default log level.
            tracing::info!(
                device = %self.identity(),
                hidpp_nodes = nodes.len(),
                candidates = candidates.len(),
                outcomes = ?outcomes,
                "no candidate node exposed ChangeHost (0x1814)"
            );
        }
        Err(DeviceError::NotFound)
    }

    /// Opens one node, or says why it is not the HID++ node we want.
    async fn open_node(&self, node: &NodeInfo) -> Result<Opened, String> {
        let (channel, _device, feature) = open_change_host(self.backend.as_ref(), node).await?;
        // `_device` goes out of scope here; the feature and the channel keep
        // the only references, and the caller drops those too.
        Ok((channel, feature))
    }
}

/// Opens `node` as a HID++ 2.0 device exposing `ChangeHost` (0x1814), or says
/// why it is not such a node.
///
/// This is the one place that knows the open sequence — open the HID node,
/// speak HID++ 2.0 to the direct device index, ask `Root` where `ChangeHost`
/// lives. [`super::discovery`] needs the [`Device`] back as well, because it
/// binds the descriptive features onto the same channel before closing it;
/// [`LogitechHidpp`] just drops it.
pub(crate) async fn open_change_host(
    backend: &dyn HidBackend,
    node: &NodeInfo,
) -> Result<(Arc<HidppChannel>, Device, Arc<ChangeHostFeature>), String> {
    let channel = match backend.open_hidpp(node).await {
        Ok(Some(channel)) => channel,
        Ok(None) => return Err("node does not speak HID++".to_string()),
        Err(err) => return Err(format!("open_hidpp: {}", error_chain(&err))),
    };

    let mut device = match Device::new(Arc::clone(&channel), DIRECT_DEVICE_INDEX).await {
        Ok(device) => device,
        Err(err) => return Err(format!("no HID++ 2.0 device: {}", error_chain(&err))),
    };

    let entry = match device.root().get_feature(ChangeHostFeature::ID).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return Err("no ChangeHost (0x1814)".to_string()),
        Err(err) => return Err(format!("feature lookup: {}", error_chain(&err))),
    };

    let feature = device.add_feature::<ChangeHostFeature>(entry.index);
    Ok((channel, device, feature))
}

#[async_trait]
impl HostSwitchable for LogitechHidpp {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn host_info(&self) -> Result<HostInfo, DeviceError> {
        let read = async {
            let (_channel, feature) = self.open().await?;
            let info = feature.get_host_info().await.map_err(|err| {
                DeviceError::Tool(format!(
                    "{}: get_host_info: {}",
                    self.identity(),
                    error_chain(&err)
                ))
            })?;
            Ok(HostInfo {
                count: info.host_count,
                current: info.current_host,
            })
            // Both handles drop here, before the timeout wrapper resolves.
        };

        match tokio::time::timeout(OP_TIMEOUT, read).await {
            Ok(result) => result,
            Err(_elapsed) => Err(DeviceError::Timeout),
        }
    }

    async fn switch_to_host(&self, index: HostIndex) -> Result<(), DeviceError> {
        // A successful switch resets the device, so it stops answering on the
        // very request that worked. `docs/specs/relay-core.md` §2.2 therefore
        // counts a timeout on the switch itself as success, and this flag is
        // how the outer timeout tells the two apart: with it clear the call
        // hung while enumerating or opening, which is a real `Timeout`; with it
        // set the call reached the switch, which is success.
        //
        // It is set once the node is open and *before* the write, not after it,
        // so a hang inside the write path is reported as success too. That is
        // deliberate: the channel was opened moments earlier by this very call
        // and nothing else holds it, so a write that never returns on it means
        // the device left — which is what a switch does.
        let sent = AtomicBool::new(false);

        let switch = async {
            let (channel, feature) = self.open().await?;
            sent.store(true, Ordering::SeqCst);
            match feature.set_current_host(index).await {
                Ok(()) => Ok(()),
                Err(err) => {
                    let context = format!("{}: set_current_host", self.identity());
                    // Asked *after* the failure: a transport error that means
                    // "the device switched and left" is one the channel can no
                    // longer be connected through.
                    let outcome = classify_switch_error(&err, &context, channel.is_connected());
                    if outcome.is_ok() {
                        tracing::info!(
                            device = %self.identity(),
                            detail = %error_chain(&err),
                            "device disconnected while switching, treating as success"
                        );
                    }
                    outcome
                }
            }
            // Both handles drop here, before the timeout wrapper resolves.
        };

        match tokio::time::timeout(OP_TIMEOUT, switch).await {
            Ok(result) => result,
            Err(_elapsed) if sent.load(Ordering::SeqCst) => {
                tracing::info!(
                    device = %self.identity(),
                    host = index,
                    "no answer after the switch request went out, treating as success"
                );
                Ok(())
            }
            Err(_elapsed) => Err(DeviceError::Timeout),
        }
    }
}

/// The nodes worth opening for this device, in enumeration order.
///
/// The `vid:pid` from the configured [`DeviceId`] is always required: a switch
/// must never reach a device the user did not declare. A configured `serial`
/// then narrows that set to the node reporting it, which is what separates two
/// identical devices; when no candidate reports that serial the `vid:pid`
/// matches stand, because a backend may not read serials at all.
fn select_candidates<'a>(
    nodes: &'a [NodeInfo],
    id: &DeviceId,
    serial: Option<&str>,
) -> Vec<&'a NodeInfo> {
    let Some((vid, pid)) = id.vid_pid() else {
        // Not a `vid:pid` id; nothing can be matched against it.
        return Vec::new();
    };

    let by_id: Vec<&NodeInfo> = nodes
        .iter()
        .filter(|node| node.vendor_id == vid && node.product_id == pid)
        .collect();

    let Some(serial) = serial.filter(|s| !s.is_empty()) else {
        return by_id;
    };

    let by_serial: Vec<&NodeInfo> = by_id
        .iter()
        .copied()
        .filter(|node| {
            node.serial_number
                .as_deref()
                .is_some_and(|reported| reported.eq_ignore_ascii_case(serial))
        })
        .collect();

    if by_serial.is_empty() {
        by_id
    } else {
        by_serial
    }
}

/// Every link of an error, flattened into one line.
///
/// `thiserror` does not fold a source's text into the outer `Display`, so
/// `Hidpp20Error::Channel(ChannelError::Timeout)` prints only "the HID++
/// channel returned an error". Anything that ends up in a log or a
/// [`DeviceError::Tool`] payload goes through here instead.
pub(crate) fn error_chain(err: &dyn StdError) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(link) = source {
        text.push_str(": ");
        text.push_str(&link.to_string());
        source = link.source();
    }
    text
}

/// What a failed `set_current_host` means, given the request already went out.
///
/// A device that stops answering is success per `docs/specs/relay-core.md`
/// §2.2: the request left this machine and the device went quiet because it
/// switched. Which error that is depends on where in the stack the link died,
/// so the whole source chain is walked for the `ChannelError` underneath —
/// matching `Hidpp20Error`'s own text cannot work, because it never mentions
/// its source.
///
/// `still_connected` is what the channel said about its transport after the
/// failure; see [`link_gone_after_send`] for the one verdict it decides.
fn classify_switch_error(
    err: &Hidpp20Error,
    context: &str,
    still_connected: bool,
) -> Result<(), DeviceError> {
    if link_gone_after_send(err, still_connected) {
        Ok(())
    } else {
        Err(DeviceError::Tool(format!(
            "{context}: {}",
            error_chain(err)
        )))
    }
}

/// Whether this error means "the request was issued and the link then went
/// away", rather than "the device answered and said no" or "the write never
/// landed".
///
/// `ChangeHostFeature::set_current_host` is fire-and-forget
/// (`FeatureEndpoint::notify` → `HidppChannel::send_and_forget`), and that call
/// does exactly two things: it refuses a message the channel cannot carry, and
/// it writes the report. So only two [`ChannelError`]s are actually reachable
/// here — [`ChannelError::MessageTypeNotSupported`] from the refusal and
/// [`ChannelError::Implementation`] from the write — and the rest of this match
/// is defensive, kept because `ChannelError` is `#[non_exhaustive]` and
/// `notify` could grow a wait.
///
/// - [`ChannelError::Implementation`] — the raw HID write failed. That is
///   "the device left mid-switch" only when the transport agrees it is gone:
///   `still_connected` is [`HidppChannel::is_connected`] asked right after the
///   failure. A write that failed on a channel still reporting itself connected
///   is a plain write failure — a wedged transport, a malformed report, a
///   refused handle — and calling it a successful switch would tell the user
///   their keyboard moved when it did not.
/// - [`ChannelError::Timeout`] / [`ChannelError::NoResponse`] — defensive: they
///   can only arise from waiting for an answer, and an answer that never comes
///   after the report went out is the completed switch itself. `Timeout`'s own
///   documentation names "connected to another host" as a cause.
///
/// Everything else is a genuine failure and keeps its message:
/// [`ChannelError::HidppNotSupported`], [`ChannelError::MessageTypeNotSupported`],
/// [`ChannelError::InvalidRawReportLength`] and [`ChannelError::ReportDescriptor`]
/// are all refusals raised *before* anything reaches the wire, and
/// `Hidpp20Error::Feature` / `Hidpp20Error::UnsupportedResponse` mean the device
/// answered — so they never wrap a `ChannelError` at all and fall through to
/// `false`.
fn link_gone_after_send(err: &(dyn StdError + 'static), still_connected: bool) -> bool {
    let mut link = Some(err);
    while let Some(current) = link {
        if let Some(channel) = current.downcast_ref::<ChannelError>() {
            return match channel {
                ChannelError::Timeout | ChannelError::NoResponse => true,
                ChannelError::Implementation(_) => !still_connected,
                _ => false,
            };
        }
        link = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use openlogi_fixture::{CassetteExchange, RequestMatch};
    use openlogi_hid::backend::NodeId;
    use openlogi_hid::replay::{ChannelConnection, ReplayBackend, ReplayNode, ReplayTopology};

    use super::*;
    use crate::device::replay_support::{
        cassette, empty_topology, exchange, get_feature, get_host_info, optional_exchange, ping,
        replay_channel, short, silent_ping, unasked_ping, CHANGE_HOST_INDEX, VID,
    };

    const PID: u16 = 0xb023;
    /// Marketing name every scripted node in this module reports.
    const NAME: &str = "MX Master 3";

    fn node(vid: u16, pid: u16, id: &str, serial: Option<&str>) -> NodeInfo {
        crate::device::replay_support::node(vid, pid, id, NAME, serial)
    }

    fn ids(candidates: &[&NodeInfo]) -> Vec<String> {
        candidates.iter().map(|n| n.id.to_string()).collect()
    }

    // --- select_candidates ------------------------------------------------

    #[test]
    fn only_nodes_with_the_configured_vid_and_pid_are_candidates() {
        let nodes = vec![
            node(0x046d, 0xb023, "a", None),
            node(0x046d, 0xb366, "b", None),
            node(0x05ac, 0xb023, "c", None),
        ];
        let picked = select_candidates(&nodes, &DeviceId("046d:b023".to_string()), None);
        assert_eq!(ids(&picked), vec!["a"]);
    }

    #[test]
    fn candidates_keep_enumeration_order() {
        let nodes = vec![
            node(0x046d, 0xb023, "first", None),
            node(0x046d, 0xb023, "second", None),
            node(0x046d, 0xb023, "third", None),
        ];
        let picked = select_candidates(&nodes, &DeviceId("046d:b023".to_string()), None);
        assert_eq!(ids(&picked), vec!["first", "second", "third"]);
    }

    #[test]
    fn a_configured_serial_narrows_to_the_node_reporting_it() {
        let nodes = vec![
            node(0x046d, 0xb023, "other-unit", Some("AAAA")),
            node(0x046d, 0xb023, "mine", Some("BBBB")),
        ];
        let picked = select_candidates(&nodes, &DeviceId("046d:b023".to_string()), Some("BBBB"));
        assert_eq!(ids(&picked), vec!["mine"]);
    }

    #[test]
    fn a_serial_match_ignores_case() {
        let nodes = vec![node(0x046d, 0xb023, "mine", Some("abcd1234"))];
        let picked =
            select_candidates(&nodes, &DeviceId("046d:b023".to_string()), Some("ABCD1234"));
        assert_eq!(ids(&picked), vec!["mine"]);
    }

    #[test]
    fn a_serial_no_node_reports_falls_back_to_vid_pid() {
        // Backends do not always read serials; a configured one must not make
        // the device unreachable.
        let nodes = vec![
            node(0x046d, 0xb023, "a", None),
            node(0x046d, 0xb023, "b", None),
        ];
        let picked = select_candidates(&nodes, &DeviceId("046d:b023".to_string()), Some("BBBB"));
        assert_eq!(ids(&picked), vec!["a", "b"]);
    }

    #[test]
    fn an_empty_serial_is_the_same_as_none() {
        let nodes = vec![node(0x046d, 0xb023, "a", Some("AAAA"))];
        let picked = select_candidates(&nodes, &DeviceId("046d:b023".to_string()), Some(""));
        assert_eq!(ids(&picked), vec!["a"]);
    }

    #[test]
    fn an_id_that_is_not_vid_pid_matches_nothing() {
        let nodes = vec![node(0x046d, 0xb023, "a", None)];
        let picked = select_candidates(&nodes, &DeviceId("mx-master".to_string()), None);
        assert!(picked.is_empty(), "got {:?}", ids(&picked));
    }

    // --- error classification ---------------------------------------------

    fn disconnected() -> Hidpp20Error {
        let transport: Box<dyn StdError + Send + Sync> = Box::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "replay HID channel is disconnected",
        ));
        Hidpp20Error::from(ChannelError::from(transport))
    }

    #[test]
    fn error_chain_joins_every_link_of_the_real_error_types() {
        // The regression this guards: `Hidpp20Error`'s own `Display` says
        // nothing about its source, so a substring rule over it can never see
        // the channel's reason.
        let err = Hidpp20Error::from(ChannelError::Timeout);
        assert_eq!(err.to_string(), "the HID++ channel returned an error");
        assert_eq!(
            error_chain(&err),
            "the HID++ channel returned an error: the HID channel operation timed out"
        );
        assert_eq!(
            error_chain(&disconnected()),
            "the HID++ channel returned an error: \
             the HID channel implementation returned an error: \
             replay HID channel is disconnected"
        );
    }

    /// The two states the channel can report after the failure.
    const STILL_CONNECTED: bool = true;
    const GONE: bool = false;

    #[test]
    fn a_timeout_on_the_switch_request_counts_as_success() {
        assert!(
            classify_switch_error(&Hidpp20Error::from(ChannelError::Timeout), "ctx", GONE).is_ok()
        );
    }

    #[test]
    fn a_silent_device_on_the_switch_request_counts_as_success() {
        assert!(
            classify_switch_error(&Hidpp20Error::from(ChannelError::NoResponse), "ctx", GONE)
                .is_ok()
        );
    }

    #[test]
    fn a_transport_that_died_on_the_switch_request_counts_as_success() {
        assert!(classify_switch_error(&disconnected(), "ctx", GONE).is_ok());
    }

    #[test]
    fn a_transport_failure_on_a_live_channel_is_a_failure() {
        // The write failed but the link is still there, so the device did not
        // leave — nothing switched, and saying otherwise would tell the user
        // their keyboard moved when it did not.
        assert!(matches!(
            classify_switch_error(&disconnected(), "ctx", STILL_CONNECTED),
            Err(DeviceError::Tool(_))
        ));
    }

    #[test]
    fn a_pre_wire_channel_refusal_is_a_failure() {
        // Nothing reached the device, so there is nothing to call a success.
        for err in [
            Hidpp20Error::from(ChannelError::HidppNotSupported),
            Hidpp20Error::from(ChannelError::MessageTypeNotSupported),
            Hidpp20Error::from(ChannelError::InvalidRawReportLength(0)),
        ] {
            assert!(
                classify_switch_error(&err, "ctx", GONE).is_err(),
                "{err:?} must stay a failure"
            );
        }
    }

    #[test]
    fn a_device_level_refusal_is_a_failure() {
        // The device answered; it was still there.
        assert!(classify_switch_error(
            &Hidpp20Error::Feature(hidpp::protocol::v20::ErrorType::InvalidArgument),
            "ctx",
            GONE
        )
        .is_err());
    }

    #[test]
    fn any_other_switch_failure_carries_its_message() {
        match classify_switch_error(&Hidpp20Error::UnsupportedResponse, "ctx", GONE) {
            Err(DeviceError::Tool(message)) => assert_eq!(
                message,
                "ctx: the received response from the device is (partly) unsupported"
            ),
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_message_names_the_device_and_its_serial() {
        let with_serial = device(
            Some("BBBB"),
            Arc::new(ReplayBackend::new(empty_topology(), Vec::new()).expect("empty topology")),
        );
        assert_eq!(with_serial.identity(), "046d:b023 serial BBBB");
        let without = device(
            None,
            Arc::new(ReplayBackend::new(empty_topology(), Vec::new()).expect("empty topology")),
        );
        assert_eq!(without.identity(), "046d:b023");
    }

    #[test]
    fn id_and_name_are_what_the_config_said() {
        let backend =
            Arc::new(ReplayBackend::new(empty_topology(), Vec::new()).expect("empty topology"));
        let device = LogitechHidpp::with_backend(
            DeviceId("046d:b023".to_string()),
            Some("BBBB".to_string()),
            "MX Master 3".to_string(),
            backend,
        );
        assert_eq!(device.id(), &DeviceId("046d:b023".to_string()));
        assert_eq!(device.name(), "MX Master 3");
    }

    // --- replay-backed scripting ------------------------------------------

    /// `Root::getFeature(0x1814)`, answered with [`CHANGE_HOST_INDEX`].
    fn get_change_host_feature() -> CassetteExchange {
        get_feature(ChangeHostFeature::ID, CHANGE_HOST_INDEX)
    }

    /// `ChangeHost::setCurrentHost` — fire-and-forget, so no response.
    fn set_current_host(host: u8) -> CassetteExchange {
        exchange(short(CHANGE_HOST_INDEX, 0x10, [host, 0, 0]), None)
    }

    fn replay_node(id: &str, pid: u16, channel: &str) -> ReplayNode {
        crate::device::replay_support::replay_node(node(VID, pid, id, None), channel)
    }

    /// One node at the configured `vid:pid` on one channel, with `exchanges`
    /// scripted on it.
    fn one_node_backend(channel: &str, exchanges: Vec<CassetteExchange>) -> Arc<ReplayBackend> {
        let topology = ReplayTopology {
            nodes: vec![replay_node("node-a", PID, channel)],
            channels: vec![replay_channel(channel)],
        };
        Arc::new(
            ReplayBackend::new(topology, vec![cassette(channel, exchanges)])
                .expect("replay topology is valid"),
        )
    }

    fn device(serial: Option<&str>, backend: Arc<ReplayBackend>) -> LogitechHidpp {
        LogitechHidpp::with_backend(
            DeviceId("046d:b023".to_string()),
            serial.map(str::to_string),
            "MX Master 3".to_string(),
            backend as Arc<dyn HidBackend>,
        )
    }

    #[tokio::test]
    async fn host_info_reads_the_scripted_change_host_reply() {
        let backend = one_node_backend(
            "mouse",
            vec![ping(), get_change_host_feature(), get_host_info(3, 1)],
        );
        let device = device(None, Arc::clone(&backend));

        let info = device.host_info().await.expect("scripted device answers");

        assert_eq!(
            info,
            HostInfo {
                count: 3,
                current: 1
            }
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn every_opened_channel_is_closed_before_the_call_returns() {
        // AGENTS.md invariant 3: open → act → close, nothing cached between
        // calls. Two calls must therefore open twice and leave nothing behind.
        let backend = one_node_backend(
            "mouse",
            vec![
                ping(),
                get_change_host_feature(),
                get_host_info(3, 1),
                ping(),
                get_change_host_feature(),
                set_current_host(2),
            ],
        );
        let device = device(None, Arc::clone(&backend));
        let node_id = NodeId::from("node-a".to_string());

        device.host_info().await.expect("scripted device answers");
        assert_eq!(backend.open_count(&node_id).expect("known node"), 1);
        assert_eq!(
            backend.channel_lifetime_count("mouse").expect("known"),
            0,
            "host_info must drop its channel before returning"
        );

        device.switch_to_host(2).await.expect("switch is accepted");
        assert_eq!(backend.open_count(&node_id).expect("known node"), 2);
        assert_eq!(
            backend.channel_lifetime_count("mouse").expect("known"),
            0,
            "switch_to_host must drop its channel before returning"
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn a_device_that_leaves_right_after_the_switch_request_is_a_success() {
        // Spec §2.2: the switch itself is what makes the device stop answering.
        //
        // No `set_current_host` exchange on purpose: the link is cut before the
        // switch request is written, so the replay channel refuses the write
        // without ever consulting the cassette. Scripting an answer nothing can
        // consume is what `require_complete` below exists to catch.
        let backend = one_node_backend("mouse", vec![ping(), get_change_host_feature()]);
        // Hold the feature lookup's reply so the link can be cut in the gap
        // between "the device is still here" and the switch request.
        let barrier = backend
            .hold_next_response(
                "mouse",
                RequestMatch::Hidpp20,
                &short(0x00, 0x00, [0x18, 0x14, 0x00]),
            )
            .expect("known channel");
        let device = device(None, Arc::clone(&backend));

        let switching = tokio::spawn(async move { device.switch_to_host(2).await });

        barrier.request_written().await;
        backend
            .set_channel_connection("mouse", ChannelConnection::Disconnected)
            .expect("known channel");
        barrier.release();

        assert!(switching.await.expect("no panic").is_ok());
        let written = backend
            .channel_completion("mouse")
            .expect("known channel")
            .written_reports;
        assert!(
            written
                .iter()
                .any(|report| report[..4] == [0x10, DIRECT_DEVICE_INDEX, CHANGE_HOST_INDEX, 0x11]),
            "the switch request must have gone out: {written:02x?}"
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test]
    async fn a_silent_candidate_does_not_starve_the_ones_behind_it() {
        // openlogi's per-request budget equals the whole operation budget, so
        // without a per-candidate bound the second node is never reached.
        let topology = ReplayTopology {
            nodes: vec![
                replay_node("node-silent", PID, "silent"),
                replay_node("node-real", PID, "real"),
            ],
            channels: vec![replay_channel("silent"), replay_channel("real")],
        };
        let backend = Arc::new(
            ReplayBackend::new(
                topology,
                vec![
                    cassette("silent", vec![silent_ping()]),
                    cassette(
                        "real",
                        vec![ping(), get_change_host_feature(), get_host_info(3, 1)],
                    ),
                ],
            )
            .expect("replay topology is valid"),
        );
        let device = device(None, Arc::clone(&backend));

        let started = std::time::Instant::now();
        let info = device.host_info().await.expect("the second node answers");
        let elapsed = started.elapsed();

        assert_eq!(
            info,
            HostInfo {
                count: 3,
                current: 1
            }
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "the silent node must be abandoned on its own budget, not openlogi's \
             5 s per-request one; took {elapsed:?}"
        );
        assert_eq!(
            backend
                .open_count(&NodeId::from("node-silent".to_string()))
                .expect("known node"),
            1,
            "the silent node must have been tried"
        );
        assert_eq!(
            backend
                .open_count(&NodeId::from("node-real".to_string()))
                .expect("known node"),
            1,
            "and then given up on in favour of the next one"
        );
        assert_eq!(
            backend.channel_lifetime_count("silent").expect("known"),
            0,
            "abandoning the probe must close the channel it opened, not leak it \
             for the rest of the operation"
        );
        backend.require_complete().expect("cassette fully replayed");
    }

    #[tokio::test(start_paused = true)]
    async fn enough_silent_candidates_exhaust_the_budget_and_are_not_found() {
        // Six silent nodes cost more than the operation budget between them.
        // The verdict must still be "no node here exposes ChangeHost"
        // (`NotFound`), not "the device is wedged" (`Timeout`) — the caller
        // retries on the latter and would sit in `Switching` for tens of
        // seconds. Time is paused, so the probes cost nothing in wall clock.
        const SILENT: usize = 6;
        let ids: Vec<String> = (0..SILENT).map(|i| format!("silent-{i}")).collect();
        let topology = ReplayTopology {
            nodes: ids
                .iter()
                .map(|id| replay_node(id, PID, id))
                .collect::<Vec<_>>(),
            channels: ids.iter().map(|id| replay_channel(id)).collect(),
        };
        let backend = Arc::new(
            ReplayBackend::new(
                topology,
                // Optional: the last node is the one the spent budget must stop
                // us from ever opening, so its ping is never consumed.
                ids.iter()
                    .map(|id| cassette(id, vec![optional_exchange(silent_ping().request, None)]))
                    .collect(),
            )
            .expect("replay topology is valid"),
        );
        let device = device(None, Arc::clone(&backend));

        assert!(
            matches!(device.host_info().await, Err(DeviceError::NotFound)),
            "a budget spent on silent nodes is NotFound, never Timeout"
        );

        let opened: usize = ids
            .iter()
            .filter(|id| {
                backend
                    .open_count(&NodeId::from((*id).clone()))
                    .expect("known node")
                    > 0
            })
            .count();
        assert!(
            (1..SILENT).contains(&opened),
            "the probes must run, and the budget must stop them before the last \
             candidate; opened {opened} of {SILENT}"
        );
        for id in &ids {
            assert_eq!(
                backend.channel_lifetime_count(id).expect("known channel"),
                0,
                "{id}: every channel the probes opened must be dropped before the call returns"
            );
        }
    }

    #[tokio::test]
    async fn a_switch_write_that_fails_on_a_live_channel_is_an_error() {
        // The complement of
        // `a_device_that_leaves_right_after_the_switch_request_is_a_success`:
        // the write failed, but the channel still reports itself connected, so
        // nothing switched. Scripting no `setCurrentHost` exchange is what makes
        // the replay channel refuse the write without touching its connection
        // state.
        let backend = one_node_backend("mouse", vec![ping(), get_change_host_feature()]);
        let device = device(None, Arc::clone(&backend));

        match device.switch_to_host(2).await {
            Err(DeviceError::Tool(message)) => assert!(
                message.starts_with("046d:b023: set_current_host:"),
                "the message must name the operation: {message}"
            ),
            other => panic!("expected Tool, got {other:?}"),
        }
        assert_eq!(
            backend.channel_lifetime_count("mouse").expect("known"),
            0,
            "a failed switch must still close its channel"
        );
    }

    #[tokio::test]
    async fn no_node_at_the_configured_vid_pid_is_not_found() {
        let topology = ReplayTopology {
            nodes: vec![replay_node("other-device", 0xb366, "other")],
            channels: vec![replay_channel("other")],
        };
        // Nothing is required of it: the node is never opened, so a required
        // exchange here would be one `require_complete` could never see
        // consumed.
        let backend = Arc::new(
            ReplayBackend::new(topology, vec![cassette("other", vec![unasked_ping()])])
                .expect("replay topology is valid"),
        );
        let device = device(None, Arc::clone(&backend));

        assert!(matches!(
            device.host_info().await,
            Err(DeviceError::NotFound)
        ));
        assert_eq!(
            backend
                .open_count(&NodeId::from("other-device".to_string()))
                .expect("known node"),
            0,
            "a node the user did not declare must never be opened"
        );
        backend.require_complete().expect("nothing was asked of it");
    }
}
