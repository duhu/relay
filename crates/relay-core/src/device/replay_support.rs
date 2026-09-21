//! Scripting helpers shared by the replay-backed device tests.
//!
//! Both [`super::logitech`] and [`super::discovery`] drive a scripted HID++
//! device through `openlogi_hid::replay::ReplayBackend`, and both need the same
//! plumbing to say it: a raw report, a cassette exchange, a topology of nodes
//! and channels. Only the exchanges that are specific to one test — the ones
//! that script a particular feature's answers — stay in that test's own module.

use openlogi_fixture::{
    CassetteExchange, HidCassette, ReportSupport, RequestMatch, FIXTURE_SCHEMA_VERSION,
};
use openlogi_hid::backend::NodeInfo;
use openlogi_hid::replay::{
    ChannelConnection, NodePresence, OpenOutcome, RawWriterAvailability, ReplayChannel, ReplayNode,
    ReplayTopology,
};

use super::logitech::DIRECT_DEVICE_INDEX;

/// Logitech's USB vendor id, which every scripted node reports.
pub(crate) const VID: u16 = 0x046d;

/// Feature index every scripted device in these tests reports for `ChangeHost`
/// (0x1814).
pub(crate) const CHANGE_HOST_INDEX: u8 = 0x06;

/// One short HID++ 2.0 report addressed to the direct device index.
///
/// `function` is the whole byte 3: the 4-bit function id in the high nibble,
/// the software id in the low one. `RequestMatch::Hidpp20` clears the software
/// id, so writing `0` there matches whatever the channel stamps.
pub(crate) fn short(feature: u8, function: u8, payload: [u8; 3]) -> Vec<u8> {
    vec![
        0x10,
        DIRECT_DEVICE_INDEX,
        feature,
        function,
        payload[0],
        payload[1],
        payload[2],
    ]
}

/// The long form of [`short`], for the answers that do not fit in three bytes.
///
/// A device may answer a short request with a long report; anything that reads
/// past payload byte 2 — a serial, a name chunk, the device-info capability
/// flags — has to be scripted this way.
pub(crate) fn long(feature: u8, function: u8, payload: [u8; 16]) -> Vec<u8> {
    let mut report = vec![0x11, DIRECT_DEVICE_INDEX, feature, function];
    report.extend_from_slice(&payload);
    report
}

/// One scripted request/response pair the device *must* be asked for.
///
/// `required` on purpose: an optional exchange is invisible to
/// `ReplayBackend::require_complete`, so a test that scripts a dozen answers
/// and then asserts "fully replayed" would pass while the code under test
/// asked for none of them. Scripting exactly what is consumed is the point of
/// the cassette; anything genuinely conditional says so with
/// [`optional_exchange`].
pub(crate) fn exchange(request: Vec<u8>, response: Option<Vec<u8>>) -> CassetteExchange {
    CassetteExchange {
        request_match: RequestMatch::Hidpp20,
        request,
        response,
        required: true,
    }
}

/// The same pair, but one `require_complete` tolerates going unasked.
pub(crate) fn optional_exchange(request: Vec<u8>, response: Option<Vec<u8>>) -> CassetteExchange {
    CassetteExchange {
        required: false,
        ..exchange(request, response)
    }
}

/// `protocol::determine_version`'s HID++ 2.0 ping, answered as version 4.
pub(crate) fn ping() -> CassetteExchange {
    exchange(
        short(0x00, 0x10, [0, 0, 0]),
        Some(short(0x00, 0x10, [4, 0, 0])),
    )
}

/// The ping on a channel a test expects nothing to be asked of.
///
/// A cassette may not be empty, so "this node is never opened" cannot be said
/// by scripting nothing; it is said by scripting the one exchange an open would
/// begin with and marking it optional, so `require_complete` stays meaningful.
pub(crate) fn unasked_ping() -> CassetteExchange {
    optional_exchange(
        short(0x00, 0x10, [0, 0, 0]),
        Some(short(0x00, 0x10, [4, 0, 0])),
    )
}

/// The ping with no answer at all: the node opens and then stays silent.
pub(crate) fn silent_ping() -> CassetteExchange {
    exchange(short(0x00, 0x10, [0, 0, 0]), None)
}

/// `Root::getFeature(id)`, answered with `index`.
pub(crate) fn get_feature(id: u16, index: u8) -> CassetteExchange {
    let [id_hi, id_lo] = id.to_be_bytes();
    exchange(
        short(0x00, 0x00, [id_hi, id_lo, 0x00]),
        Some(short(0x00, 0x00, [index, 0x00, 0x00])),
    )
}

/// `Root::getFeature(id)` on a device that does not have the feature: index 0
/// is how HID++ 2.0 says "not present".
pub(crate) fn no_feature(id: u16) -> CassetteExchange {
    get_feature(id, 0x00)
}

/// `ChangeHost::getHostInfo`, answered with `count` slots and `current` as the
/// one in use.
pub(crate) fn get_host_info(count: u8, current: u8) -> CassetteExchange {
    exchange(
        short(CHANGE_HOST_INDEX, 0x00, [0, 0, 0]),
        Some(short(CHANGE_HOST_INDEX, 0x00, [count, current, 0])),
    )
}

pub(crate) fn cassette(channel: &str, exchanges: Vec<CassetteExchange>) -> HidCassette {
    HidCassette {
        schema_version: FIXTURE_SCHEMA_VERSION,
        name: format!("relay-{channel}"),
        channel: channel.to_string(),
        report_support: ReportSupport::ShortAndLong,
        exchanges,
    }
}

pub(crate) fn node(vid: u16, pid: u16, id: &str, name: &str, serial: Option<&str>) -> NodeInfo {
    NodeInfo {
        id: id.to_string().into(),
        vendor_id: vid,
        product_id: pid,
        usage_page: 0xff43,
        usage_id: 0x0202,
        name: name.to_string(),
        manufacturer: Some("Logitech".to_string()),
        serial_number: serial.map(str::to_string),
    }
}

pub(crate) fn replay_node(info: NodeInfo, channel: &str) -> ReplayNode {
    ReplayNode {
        info,
        presence: NodePresence::Present,
        open_outcome: OpenOutcome::Hidpp,
        channel: Some(channel.to_string()),
        raw_writer: RawWriterAvailability::Capture,
        receiver_slots: Vec::new(),
    }
}

pub(crate) fn replay_channel(id: &str) -> ReplayChannel {
    ReplayChannel {
        id: id.to_string(),
        connection: ChannelConnection::Connected,
        report_support: ReportSupport::ShortAndLong,
    }
}

pub(crate) fn empty_topology() -> ReplayTopology {
    ReplayTopology {
        nodes: Vec::new(),
        channels: Vec::new(),
    }
}
