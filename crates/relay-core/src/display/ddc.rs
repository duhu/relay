//! M2 display layer: speak DDC/CI ourselves over `IOAVService`.
//!
//! Apple Silicon has no public DDC path, so this takes the route `m1ddc`
//! takes: walk the IOService plane, pair every `IOMobileFramebuffer` with the
//! external `DCPAVServiceProxy` that follows it, then write — or read — VCP
//! 0x60 through the private `ioav_ffi` symbols.
//!
//! Unlike `m1ddc` we never touch CoreGraphics or CoreDisplay: the framebuffers
//! found while walking the registry already carry `EDID UUID` and
//! `DisplayAttributes`, which is everything the config needs to name a display.
//!
//! Everything above the FFI edge is a pure function or goes through
//! [`DdcTransport`], so the packets, the pairing rule, the selection rule and
//! the write and read loops are unit tested with no display attached.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;
use std::time::Duration;

use async_trait::async_trait;
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_io_kit::{
    io_iterator_t, io_object_t, io_registry_entry_t, kIOMainPortDefault,
    kIORegistryIterateRecursively, kIOReturnSuccess, kIOServicePlane, IOIteratorNext,
    IOObjectConformsTo, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntryCreateIterator, IORegistryEntryGetName, IORegistryEntryGetParentEntry,
    IORegistryEntrySearchCFProperty, IORegistryGetRootEntry, IOReturn,
};
use serde::Serialize;

use super::{ioav_ffi, DisplayError, DisplayInput};

/// DDC/CI over I2C lives at chip address 0x37 on all but one bridge.
const DDC_CHIP_ADDRESS_DEFAULT: u32 = 0x37;
/// MCDP29xx bridges route DDC through 0xB7 instead.
const DDC_CHIP_ADDRESS_MCDP29XX: u32 = 0xB7;
/// The I2C data address DDC/CI writes go to.
pub(crate) const DDC_DATA_ADDRESS: u32 = 0x51;
/// VCP feature code "Input Source".
pub(crate) const VCP_INPUT_SOURCE: u8 = 0x60;
/// Displays drop writes often enough that `m1ddc` sends every packet twice;
/// these are its `DDC_ITERATIONS` and `DDC_WAIT`.
const DDC_ITERATIONS: usize = 2;
pub(crate) const DDC_WAIT: Duration = Duration::from_millis(10);
/// MCDP29xx bridges need longer than [`DDC_WAIT`] before the reply can be
/// fetched — 10 ms comes back empty on those — so `m1ddc` waits 50 ms there
/// (`DDC_MCDP_READ_WAIT` in its `sources/i2c.m`). This is the *pre-read* wait
/// only: the write cadence stays [`DDC_WAIT`] everywhere, or every packet
/// bound for such a bridge would go out at half speed.
const DDC_MCDP_READ_WAIT: Duration = Duration::from_millis(50);
/// How many bytes a "Get VCP Feature Reply" is fetched in, as `m1ddc` asks
/// for it; only the first ten carry the answer.
const DDC_REPLY_LEN: usize = 12;
/// How many times a read is attempted before giving up.
const DDC_READ_ATTEMPTS: usize = 2;
/// How long to wait before the second attempt.
///
/// Measured on 2026-09-20: right after the monitor changed input source the
/// DDC channel of the Mac it switched *to* is not ready yet — the display is
/// not even discoverable, so the read fails outright about 1.5 s in, and in
/// that same window a *write* fails its first try with `display not present`
/// and succeeds on a retry ~0.6 s later. By ~2 s reads succeed again, so one
/// more attempt after a short pause covers it.
const DDC_READ_RETRY_WAIT: Duration = Duration::from_millis(400);
/// A DDC write that takes longer than this means the display is not answering
/// (the budget the M0 shell-out used).
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(8);
/// What a framebuffer with no `ProductName` is called.
const UNKNOWN_DISPLAY: &str = "Unknown Display";

/// The registry class every display framebuffer conforms to.
const FRAMEBUFFER_CLASS: &CStr = c"IOMobileFramebuffer";
/// The registry entry name of the node that carries DDC.
const PROXY_NAME: &str = "DCPAVServiceProxy";
/// `Location` of a proxy that drives an external display.
const LOCATION_EXTERNAL: &str = "External";
/// The bridge whose DDC lives at [`DDC_CHIP_ADDRESS_MCDP29XX`].
const MCDP29XX_PROVIDER: &str = "AppleDCPMCDP29XX";

/// One display the settings page can offer to add.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DiscoveredDisplay {
    pub edid_uuid: String,
    pub name: String,
}

/// Every external display this machine can drive over DDC, in registry order.
///
/// Discovery is best effort: a machine with no external display, or an
/// IORegistry we cannot walk, yields an empty list rather than an error.
pub async fn list_displays() -> Vec<DiscoveredDisplay> {
    // The proxies are dropped inside the blocking task: no `io_object_t`
    // handle outlives the thread that opened it.
    let scan = tokio::task::spawn_blocking(|| {
        scan_displays()
            .into_iter()
            .map(|display| DiscoveredDisplay {
                edid_uuid: display.edid_uuid,
                name: display.name,
            })
            .collect::<Vec<_>>()
    });

    match scan.await {
        Ok(displays) => displays,
        Err(err) => {
            tracing::debug!(%err, "display discovery failed");
            Vec::new()
        }
    }
}

/// A configured display, looked up afresh on every write.
///
/// Nothing is cached: an `IOAVServiceRef` does not survive a sleep or a cable
/// swap, and switches are rare enough that a full registry walk is cheap.
pub struct DdcDisplay {
    /// `None` (or an empty string) means "the first external display", which
    /// is what a single-monitor setup wants.
    edid_uuid: Option<String>,
    name: String,
}

impl DdcDisplay {
    pub fn new(edid_uuid: Option<String>, name: String) -> Self {
        Self {
            edid_uuid: edid_uuid.filter(|uuid| !uuid.is_empty()),
            name,
        }
    }
}

#[async_trait]
impl DisplayInput for DdcDisplay {
    fn name(&self) -> &str {
        &self.name
    }

    async fn set_input(&self, code: u8) -> Result<(), DisplayError> {
        let wanted = self.edid_uuid.clone();
        let write =
            tokio::task::spawn_blocking(move || set_input_blocking(wanted.as_deref(), code));

        match tokio::time::timeout(TOOL_TIMEOUT, write).await {
            // A blocking task cannot be cancelled, but it owns every handle it
            // opened and releases them on its way out.
            Err(_elapsed) => Err(DisplayError::Timeout),
            Ok(Err(err)) => Err(DisplayError::Tool(format!("display write panicked: {err}"))),
            Ok(Ok(result)) => result,
        }
    }

    async fn current_input(&self) -> Option<u8> {
        let wanted = self.edid_uuid.clone();
        let read = tokio::task::spawn_blocking(move || current_input_blocking(wanted.as_deref()));

        match tokio::time::timeout(TOOL_TIMEOUT, read).await {
            // Reading is only ever advice, so every failure is the same
            // answer: we do not know.
            Err(_elapsed) => {
                tracing::debug!("the display did not answer the input source read in time");
                None
            }
            Ok(Err(err)) => {
                tracing::debug!(%err, "the display read panicked");
                None
            }
            Ok(Ok(code)) => code,
        }
    }
}

/// The blocking half of [`DdcDisplay::set_input`]: discover, pick, write.
fn set_input_blocking(wanted: Option<&str>, code: u8) -> Result<(), DisplayError> {
    let displays = scan_displays();
    let display = pick(&displays, wanted)?;
    let transport = IoAvTransport::open(&display.proxy)?;
    perform_write(
        &transport,
        transport.chip_address,
        &input_packet(code),
        DDC_WAIT,
    )
}

/// The blocking half of [`DdcDisplay::current_input`]: ask, and ask again.
///
/// The whole of it — the registry walk included — happens here, inside the
/// one `spawn_blocking` and under the one [`TOOL_TIMEOUT`].
fn current_input_blocking(wanted: Option<&str>) -> Option<u8> {
    read_retrying(|| read_input_once(wanted), DDC_READ_RETRY_WAIT)
}

/// Runs `attempt` up to [`DDC_READ_ATTEMPTS`] times, pausing `retry_wait`
/// between tries, and answers with the first usable value.
///
/// The retry repeats the *whole* attempt rather than just the I2C exchange:
/// the failure mode this covers is a display that is not discoverable yet, so
/// the walk has to be redone (see [`DDC_READ_RETRY_WAIT`]). Exhausting the
/// attempts is still "unknown", never an error.
fn read_retrying(mut attempt: impl FnMut() -> Option<u8>, retry_wait: Duration) -> Option<u8> {
    for round in 0..DDC_READ_ATTEMPTS {
        if let Some(code) = attempt() {
            return Some(code);
        }
        if round + 1 < DDC_READ_ATTEMPTS && !retry_wait.is_zero() {
            tracing::debug!("the display did not answer; asking once more");
            std::thread::sleep(retry_wait);
        }
    }
    None
}

/// One read attempt: discover, pick, ask.
///
/// Every handle it opens — the proxies from the walk and the `IOAVService` —
/// is released before it returns, and nothing is cached: the next attempt
/// walks the registry again.
fn read_input_once(wanted: Option<&str>) -> Option<u8> {
    let displays = scan_displays();
    let display = match pick(&displays, wanted) {
        Ok(display) => display,
        Err(err) => {
            tracing::debug!(%err, "cannot read the current input source");
            return None;
        }
    };
    let transport = match IoAvTransport::open(&display.proxy) {
        Ok(transport) => transport,
        Err(err) => {
            tracing::debug!(%err, "cannot open the display's DDC channel");
            return None;
        }
    };
    perform_read(&transport, transport.chip_address, DDC_WAIT)
}

/// The DDC/CI "Set VCP Feature" packet for input source `code`.
///
/// Byte for byte what `m1ddc` sends: header, length, VCP code, big-endian
/// value, then a checksum that also covers the I2C data address.
pub(crate) fn input_packet(code: u8) -> [u8; 6] {
    let body = [0x84u8, 0x03, VCP_INPUT_SOURCE, 0x00, code];
    let checksum = body
        .iter()
        .fold(0x6Eu8 ^ DDC_DATA_ADDRESS as u8, |sum, byte| sum ^ byte);
    [body[0], body[1], body[2], body[3], body[4], checksum]
}

/// The DDC/CI "Get VCP Feature" request for the input source.
///
/// Byte for byte what `m1ddc`'s `prepareDDCRead` sends. Note the checksum:
/// unlike the write above it is seeded with 0x6E alone and does **not** cover
/// the I2C data address. Getting that wrong yields a display that simply
/// never answers.
pub(crate) fn read_input_packet() -> [u8; 4] {
    let body = [0x82u8, 0x01, VCP_INPUT_SOURCE];
    let checksum = body.iter().fold(0x6Eu8, |sum, byte| sum ^ byte);
    [body[0], body[1], body[2], checksum]
}

/// The current value out of a DDC/CI "Get VCP Feature Reply" for `vcp`.
///
/// `None` unless the reply really is the answer to the question we asked:
/// `buf[2]` is the feature-reply opcode, `buf[3]` the result code (0 = no
/// error) and `buf[4]` the VCP code echoed back. The current value is
/// big-endian in `buf[8..10]`; the maximum in `buf[6..8]` is not used here.
/// A reply shorter than that carries no answer at all.
pub(crate) fn parse_feature_reply(vcp: u8, buf: &[u8]) -> Option<u16> {
    const FEATURE_REPLY: u8 = 0x02;
    const NO_ERROR: u8 = 0x00;

    if buf.len() < 10 {
        return None;
    }
    if buf[2] != FEATURE_REPLY || buf[3] != NO_ERROR || buf[4] != vcp {
        return None;
    }
    Some(u16::from_be_bytes([buf[8], buf[9]]))
}

/// Asks the display which input source it is showing, or `None`.
///
/// The request goes out through the ordinary write path (twice, as `m1ddc`
/// sends everything), then the reply is fetched after one more `wait`.
/// Anything that does not parse as our answer is "unknown": a read failure
/// must never be mistaken for a value.
fn perform_read(transport: &impl DdcTransport, chip: u32, wait: Duration) -> Option<u8> {
    // The request is written at the ordinary cadence; only the pause before
    // listening depends on the bridge.
    if let Err(err) = perform_write(transport, chip, &read_input_packet(), wait) {
        tracing::debug!(%err, "the display refused the input source request");
        return None;
    }
    let wait = read_wait(chip, wait);
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }

    let mut reply = [0u8; DDC_REPLY_LEN];
    if let Err(ret) = transport.read(chip, DDC_DATA_ADDRESS, &mut reply) {
        tracing::debug!(ret = format!("0x{ret:08x}"), "IOAVServiceReadI2C failed");
        return None;
    }

    let Some(value) = parse_feature_reply(VCP_INPUT_SOURCE, &reply) else {
        tracing::debug!(?reply, "the display's reply is not a VCP 0x60 answer");
        return None;
    };
    match u8::try_from(value) {
        Ok(code) => Some(code),
        Err(_) => {
            // The config stores input codes as bytes, so a wider one is a
            // value we could never write back.
            tracing::debug!(value, "the input source does not fit a byte");
            None
        }
    }
}

/// How long to wait after the request before fetching the reply.
///
/// `Duration::ZERO` stays zero: the tests use it to mean "do not sleep".
pub(crate) fn read_wait(chip: u32, wait: Duration) -> Duration {
    if wait.is_zero() || chip != DDC_CHIP_ADDRESS_MCDP29XX {
        wait
    } else {
        DDC_MCDP_READ_WAIT
    }
}

/// The I2C edge, so the loops above it can be tested without a display.
pub(crate) trait DdcTransport {
    fn write(&self, chip: u32, addr: u32, data: &[u8]) -> Result<(), i32>;
    /// Fills `buf` with the display's reply; `Err` carries the `IOReturn`.
    fn read(&self, chip: u32, addr: u32, buf: &mut [u8]) -> Result<(), i32>;
}

/// Sends `packet` [`DDC_ITERATIONS`] times, pausing `wait` before each write.
pub(crate) fn perform_write(
    transport: &impl DdcTransport,
    chip: u32,
    packet: &[u8],
    wait: Duration,
) -> Result<(), DisplayError> {
    for _ in 0..DDC_ITERATIONS {
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        if let Err(ret) = transport.write(chip, DDC_DATA_ADDRESS, packet) {
            return Err(DisplayError::Tool(format!(
                "IOAVServiceWriteI2C returned 0x{ret:08x}"
            )));
        }
    }
    Ok(())
}

/// One node of interest seen while walking the IOService plane, in order.
#[derive(Debug, PartialEq)]
pub(crate) enum RegistryNode<P> {
    /// An `IOMobileFramebuffer`, with whatever its subtree said about it.
    Framebuffer {
        edid_uuid: Option<String>,
        name: Option<String>,
    },
    /// A `DCPAVServiceProxy`; only the external ones can carry DDC.
    Proxy { external: bool, payload: P },
}

/// A framebuffer that has both an identity and a proxy to write through.
#[derive(Debug, PartialEq)]
pub(crate) struct PairedDisplay<P> {
    pub edid_uuid: String,
    pub name: String,
    pub proxy: P,
}

/// Pairs each framebuffer with the first external proxy that follows it.
///
/// This is `m1ddc`'s rule: in IOService plane order a `DCPAVServiceProxy`
/// belongs to the most recent framebuffer, so the first external one after a
/// framebuffer is that display's DDC channel. Framebuffers with no
/// `EDID UUID` cannot be named in the config and are dropped.
pub(crate) fn pair_displays<P>(
    nodes: impl IntoIterator<Item = RegistryNode<P>>,
) -> Vec<PairedDisplay<P>> {
    let mut paired = Vec::new();
    let mut current: Option<(Option<String>, Option<String>)> = None;

    for node in nodes {
        match node {
            RegistryNode::Framebuffer { edid_uuid, name } => current = Some((edid_uuid, name)),
            RegistryNode::Proxy {
                external: false, ..
            } => {}
            RegistryNode::Proxy {
                external: true,
                payload,
            } => {
                // `take` is what keeps a proxy to a single framebuffer.
                let Some((edid_uuid, name)) = current.take() else {
                    continue;
                };
                if let Some(edid_uuid) = edid_uuid {
                    paired.push(PairedDisplay {
                        edid_uuid,
                        name: name.unwrap_or_else(|| UNKNOWN_DISPLAY.to_string()),
                        proxy: payload,
                    });
                }
            }
        }
    }
    paired
}

/// Picks the configured display, or the first one when none is configured.
///
/// UUIDs are compared case-insensitively: the registry prints them upper case
/// and a hand-edited config should not fail over that.
pub(crate) fn pick<'a, P>(
    displays: &'a [PairedDisplay<P>],
    wanted: Option<&str>,
) -> Result<&'a PairedDisplay<P>, DisplayError> {
    let found = match wanted.filter(|uuid| !uuid.is_empty()) {
        None => displays.first(),
        Some(uuid) => displays
            .iter()
            .find(|display| display.edid_uuid.eq_ignore_ascii_case(uuid)),
    };
    found.ok_or_else(|| DisplayError::Tool("display not present".to_string()))
}

// ---------------------------------------------------------------------------
// The IOKit edge: everything below here talks to the registry.
// ---------------------------------------------------------------------------

/// IOKit's `io_name_t`: a fixed 128-byte C string buffer.
const NAME_BUF_LEN: usize = 128;
type NameBuf = [c_char; NAME_BUF_LEN];

/// `MACH_PORT_NULL`, which every IOKit lookup returns on failure.
const MACH_PORT_NULL: io_object_t = 0;
/// `KERN_SUCCESS`, the value IOKit's `kern_return_t` shares with
/// `kIOReturnSuccess`; `objc2_io_kit` exports only the latter.
const KERN_SUCCESS: IOReturn = kIOReturnSuccess;

/// Owns an `io_object_t` and releases it on drop.
struct IoObject(io_object_t);

impl IoObject {
    fn new(raw: io_object_t) -> Option<Self> {
        (raw != MACH_PORT_NULL).then_some(Self(raw))
    }

    fn raw(&self) -> io_object_t {
        self.0
    }
}

impl Drop for IoObject {
    fn drop(&mut self) {
        IOObjectRelease(self.0);
    }
}

/// A `DCPAVServiceProxy` entry, kept until we know which display wins.
pub(crate) struct Proxy {
    entry: IoObject,
    /// Decided at discovery time, because it needs the entry's parent.
    chip_address: u32,
}

/// An open `IOAVService`, released when the write is done.
pub(crate) struct IoAvTransport {
    service: CFRetained<CFType>,
    pub(crate) chip_address: u32,
}

impl IoAvTransport {
    pub(crate) fn open(proxy: &Proxy) -> Result<Self, DisplayError> {
        // SAFETY: `proxy.entry` owns a live `DCPAVServiceProxy` handle.
        let service =
            unsafe { ioav_ffi::create_with_service(proxy.entry.raw()) }.ok_or_else(|| {
                DisplayError::Tool("IOAVServiceCreateWithService returned null".to_string())
            })?;
        Ok(Self {
            service,
            chip_address: proxy.chip_address,
        })
    }
}

impl DdcTransport for IoAvTransport {
    fn write(&self, chip: u32, addr: u32, data: &[u8]) -> Result<(), i32> {
        // SAFETY: `self.service` can only have come from
        // `create_with_service`, which is what `write_i2c` requires.
        match unsafe { ioav_ffi::write_i2c(&self.service, chip, addr, data) } {
            ioav_ffi::IO_RETURN_SUCCESS => Ok(()),
            ret => Err(ret),
        }
    }

    fn read(&self, chip: u32, addr: u32, buf: &mut [u8]) -> Result<(), i32> {
        // SAFETY: as above, `self.service` came from `create_with_service`.
        match unsafe { ioav_ffi::read_i2c(&self.service, chip, addr, buf) } {
            ioav_ffi::IO_RETURN_SUCCESS => Ok(()),
            ret => Err(ret),
        }
    }
}

/// Walks the IOService plane and returns every display we can drive.
pub(crate) fn scan_displays() -> Vec<PairedDisplay<Proxy>> {
    pair_displays(walk_registry())
}

/// Collects the framebuffers and proxies of the IOService plane, in order.
fn walk_registry() -> Vec<RegistryNode<Proxy>> {
    let mut nodes = Vec::new();

    // SAFETY: reading an immutable IOKit constant.
    let main_port = unsafe { kIOMainPortDefault };
    let Some(root) = IoObject::new(IORegistryGetRootEntry(main_port)) else {
        tracing::debug!("the IORegistry root is unavailable");
        return nodes;
    };

    let Some(mut plane) = name_buf(kIOServicePlane.to_bytes()) else {
        return nodes;
    };
    let mut raw_iterator: io_iterator_t = MACH_PORT_NULL;
    // SAFETY: `plane` is a 128-byte C string and `raw_iterator` is a live
    // out-pointer, which is all the call needs.
    let ret = unsafe {
        IORegistryEntryCreateIterator(
            root.raw(),
            &mut plane,
            kIORegistryIterateRecursively,
            &mut raw_iterator,
        )
    };
    if ret != KERN_SUCCESS {
        tracing::debug!(ret, "cannot iterate the IOService plane");
        return nodes;
    }
    let Some(iterator) = IoObject::new(raw_iterator) else {
        return nodes;
    };

    let Some(mut framebuffer_class) = name_buf(FRAMEBUFFER_CLASS.to_bytes()) else {
        return nodes;
    };
    while let Some(entry) = IoObject::new(IOIteratorNext(iterator.raw())) {
        // SAFETY: `framebuffer_class` is a 128-byte C string.
        if unsafe { IOObjectConformsTo(entry.raw(), &mut framebuffer_class) } {
            nodes.push(RegistryNode::Framebuffer {
                edid_uuid: search_string(&entry, "EDID UUID"),
                name: product_name(&entry),
            });
            continue;
        }

        if entry_name(&entry).as_deref() != Some(PROXY_NAME) {
            continue;
        }
        let external = search_string(&entry, "Location").as_deref() == Some(LOCATION_EXTERNAL);
        let chip_address = chip_address(&entry);
        nodes.push(RegistryNode::Proxy {
            external,
            payload: Proxy {
                entry,
                chip_address,
            },
        });
    }
    nodes
}

/// The chip address DDC lives at behind this proxy.
///
/// MCDP29xx bridges answer on 0xB7; everything else on 0x37. The provider
/// class is a property of the proxy's parent, as in `m1ddc`.
fn chip_address(proxy: &IoObject) -> u32 {
    let Some(mut plane) = name_buf(kIOServicePlane.to_bytes()) else {
        return DDC_CHIP_ADDRESS_DEFAULT;
    };
    let mut raw_parent: io_registry_entry_t = MACH_PORT_NULL;
    // SAFETY: `plane` is a 128-byte C string and `raw_parent` a live
    // out-pointer.
    let ret = unsafe { IORegistryEntryGetParentEntry(proxy.raw(), &mut plane, &mut raw_parent) };
    if ret != KERN_SUCCESS {
        return DDC_CHIP_ADDRESS_DEFAULT;
    }
    let Some(parent) = IoObject::new(raw_parent) else {
        return DDC_CHIP_ADDRESS_DEFAULT;
    };

    let key = CFString::from_str("EPICProviderClass");
    // SAFETY: `key` is a live CFString and `None` means the default
    // allocator; the returned value is owned by us.
    let value = unsafe { IORegistryEntryCreateCFProperty(parent.raw(), Some(&key), None, 0) };
    let is_mcdp29xx = value
        .as_ref()
        .and_then(|value| value.downcast_ref::<CFString>())
        .is_some_and(|class| class.to_string() == MCDP29XX_PROVIDER);

    if is_mcdp29xx {
        DDC_CHIP_ADDRESS_MCDP29XX
    } else {
        DDC_CHIP_ADDRESS_DEFAULT
    }
}

/// The display name a framebuffer advertises, if it advertises one.
fn product_name(framebuffer: &IoObject) -> Option<String> {
    let attributes = search_property(framebuffer, "DisplayAttributes")?;
    let attributes = attributes.downcast_ref::<CFDictionary>()?;
    let products =
        dictionary_value(attributes, "ProductAttributes")?.downcast_ref::<CFDictionary>()?;
    let name = dictionary_value(products, "ProductName")?.downcast_ref::<CFString>()?;
    Some(name.to_string())
}

/// A string property of an entry or, recursively, of its children.
fn search_string(entry: &IoObject, key: &str) -> Option<String> {
    let value = search_property(entry, key)?;
    Some(value.downcast_ref::<CFString>()?.to_string())
}

/// `IORegistryEntrySearchCFProperty`, recursive, on the IOService plane.
///
/// The properties we want (`EDID UUID`, `DisplayAttributes`, `Location`) sit
/// somewhere under the entry rather than on it, which is why the search has to
/// recurse.
fn search_property(entry: &IoObject, key: &str) -> Option<CFRetained<CFType>> {
    let mut plane = name_buf(kIOServicePlane.to_bytes())?;
    let key = CFString::from_str(key);
    // SAFETY: `plane` is a 128-byte C string, `key` a live CFString, and
    // `None` means the default allocator; the value comes back owned.
    unsafe {
        IORegistryEntrySearchCFProperty(
            entry.raw(),
            &mut plane,
            Some(&key),
            None,
            kIORegistryIterateRecursively,
        )
    }
}

/// Looks `key` up in a CF dictionary that holds CF types.
fn dictionary_value<'a>(dictionary: &'a CFDictionary, key: &str) -> Option<&'a CFType> {
    let key = CFString::from_str(key);
    let key: *const CFString = &*key;
    // SAFETY: the dictionary comes from IOKit, so its keys and values are CF
    // types; the lookup borrows from the dictionary and may return null.
    let value = unsafe { dictionary.value(key.cast::<c_void>()) };
    // SAFETY: a non-null value is a CF type owned by the dictionary, which
    // outlives the borrow.
    NonNull::new(value.cast_mut().cast::<CFType>()).map(|value| unsafe { value.as_ref() })
}

/// The registry entry's own name, e.g. `DCPAVServiceProxy`.
fn entry_name(entry: &IoObject) -> Option<String> {
    let mut buf: NameBuf = [0; NAME_BUF_LEN];
    // SAFETY: `buf` is exactly the 128-byte buffer IOKit writes the name into.
    if unsafe { IORegistryEntryGetName(entry.raw(), &mut buf) } != KERN_SUCCESS {
        return None;
    }
    // SAFETY: on success IOKit leaves a nul-terminated name in `buf`.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    name.to_str().ok().map(str::to_string)
}

/// Copies `bytes` into IOKit's fixed-size name buffer.
///
/// `None` when `bytes` would not fit with its terminating nul: IOKit reads
/// the buffer as a C string, so a truncated copy would name the wrong thing.
/// Every caller passes a constant, so in practice this never happens.
fn name_buf(bytes: &[u8]) -> Option<NameBuf> {
    if bytes.len() >= NAME_BUF_LEN {
        tracing::debug!(?bytes, "name does not fit io_name_t");
        return None;
    }
    let mut buf: NameBuf = [0; NAME_BUF_LEN];
    for (slot, byte) in buf.iter_mut().zip(bytes) {
        *slot = *byte as c_char;
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// One I2C call the code under test made, in order.
    #[derive(Debug, PartialEq)]
    enum Call {
        Write { chip: u32, addr: u32, data: Vec<u8> },
        Read { chip: u32, addr: u32, len: usize },
    }

    /// Records what the DDC loops do instead of touching IOKit.
    #[derive(Default)]
    struct FakeTransport {
        calls: RefCell<Vec<Call>>,
        fail_with: Option<i32>,
        /// What a read answers: the bytes it fills in, or the `IOReturn` it
        /// fails with. `None` means no read was expected.
        reply: Option<Result<Vec<u8>, i32>>,
        /// Counts the channels this fake holds open, the way the HID
        /// backend's `channel_lifetime_count` does.
        live: Option<Rc<Cell<usize>>>,
    }

    impl FakeTransport {
        /// A transport that answers `reply` and, like the real one, counts as
        /// an open channel until it is dropped.
        fn open(live: &Rc<Cell<usize>>, reply: Result<Vec<u8>, i32>) -> Self {
            live.set(live.get() + 1);
            Self {
                calls: RefCell::default(),
                fail_with: None,
                reply: Some(reply),
                live: Some(Rc::clone(live)),
            }
        }

        /// A transport whose every write fails; it reads nothing.
        fn refusing(ret: i32) -> Self {
            Self {
                calls: RefCell::default(),
                fail_with: Some(ret),
                reply: None,
                live: None,
            }
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.calls
                .borrow()
                .iter()
                .filter_map(|call| match call {
                    Call::Write { data, .. } => Some(data.clone()),
                    Call::Read { .. } => None,
                })
                .collect()
        }
    }

    impl Drop for FakeTransport {
        fn drop(&mut self) {
            if let Some(live) = &self.live {
                live.set(live.get() - 1);
            }
        }
    }

    impl DdcTransport for FakeTransport {
        fn write(&self, chip: u32, addr: u32, data: &[u8]) -> Result<(), i32> {
            self.calls.borrow_mut().push(Call::Write {
                chip,
                addr,
                data: data.to_vec(),
            });
            match self.fail_with {
                Some(ret) => Err(ret),
                None => Ok(()),
            }
        }

        fn read(&self, chip: u32, addr: u32, buf: &mut [u8]) -> Result<(), i32> {
            self.calls.borrow_mut().push(Call::Read {
                chip,
                addr,
                len: buf.len(),
            });
            match self.reply.as_ref().expect("no read was expected") {
                Ok(bytes) => {
                    for (slot, byte) in buf.iter_mut().zip(bytes) {
                        *slot = *byte;
                    }
                    Ok(())
                }
                Err(ret) => Err(*ret),
            }
        }
    }

    /// A feature reply for VCP 0x60 carrying `value`, as the AOC sends it.
    fn feature_reply(value: u8) -> Vec<u8> {
        vec![
            0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x1B, 0x00, value, 0x00, 0x00,
        ]
    }

    fn two_displays() -> Vec<PairedDisplay<u32>> {
        pair_displays(vec![
            RegistryNode::Framebuffer {
                edid_uuid: Some("AAAA".to_string()),
                name: Some("Display A".to_string()),
            },
            RegistryNode::Proxy {
                external: true,
                payload: 1,
            },
            RegistryNode::Framebuffer {
                edid_uuid: Some("BBBB".to_string()),
                name: None,
            },
            RegistryNode::Proxy {
                external: false,
                payload: 2,
            },
            RegistryNode::Proxy {
                external: true,
                payload: 3,
            },
        ])
    }

    #[test]
    // The checksum is spelled out the way `m1ddc` computes it, `^ 0x00` and
    // all, so the two can be compared by eye.
    #[allow(clippy::identity_op)]
    fn the_input_packet_is_byte_for_byte_what_m1ddc_sends() {
        let expected = [
            0x84,
            0x03,
            0x60,
            0x00,
            0x11,
            0x6E ^ 0x51 ^ 0x84 ^ 0x03 ^ 0x60 ^ 0x00 ^ 0x11,
        ];
        assert_eq!(input_packet(17), expected);
    }

    #[test]
    fn a_proxy_belongs_to_the_framebuffer_before_it() {
        let paired = two_displays();
        assert_eq!(paired.len(), 2, "got {paired:?}");
        // A takes the first external proxy; B skips the internal one and
        // takes the third.
        assert_eq!(paired[0].edid_uuid, "AAAA");
        assert_eq!(paired[0].name, "Display A");
        assert_eq!(paired[0].proxy, 1);
        assert_eq!(paired[1].edid_uuid, "BBBB");
        assert_eq!(paired[1].name, "Unknown Display");
        assert_eq!(paired[1].proxy, 3);
    }

    #[test]
    fn a_framebuffer_without_an_edid_uuid_is_dropped() {
        let paired = pair_displays(vec![
            RegistryNode::Framebuffer {
                edid_uuid: None,
                name: Some("Built-in".to_string()),
            },
            RegistryNode::Proxy {
                external: true,
                payload: 1,
            },
        ]);
        assert!(paired.is_empty(), "got {paired:?}");
    }

    #[test]
    fn no_configured_uuid_picks_the_first_display() {
        let paired = two_displays();
        for wanted in [None, Some("")] {
            let picked = pick(&paired, wanted).expect("a display");
            assert_eq!(picked.edid_uuid, "AAAA", "for {wanted:?}");
        }
    }

    #[test]
    fn a_configured_uuid_picks_that_display() {
        let paired = two_displays();
        let picked = pick(&paired, Some("bbbb")).expect("a display");
        assert_eq!(picked.proxy, 3);
    }

    #[test]
    fn an_unknown_uuid_is_not_present() {
        let paired = two_displays();
        let err = pick(&paired, Some("CCCC")).expect_err("no such display");
        match err {
            DisplayError::Tool(message) => assert_eq!(message, "display not present"),
            other => panic!("expected Tool, got {other:?}"),
        }
        let none: [PairedDisplay<u32>; 0] = [];
        assert!(pick(&none, None).is_err());
    }

    #[test]
    fn a_write_is_repeated_twice() {
        let transport = FakeTransport::default();
        perform_write(
            &transport,
            DDC_CHIP_ADDRESS_DEFAULT,
            &input_packet(17),
            Duration::ZERO,
        )
        .expect("write");

        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 2, "got {calls:?}");
        for call in calls.iter() {
            assert_eq!(
                call,
                &Call::Write {
                    chip: 0x37,
                    addr: 0x51,
                    data: input_packet(17).to_vec(),
                }
            );
        }
    }

    #[test]
    fn a_failed_write_carries_the_ioreturn() {
        let transport = FakeTransport::refusing(0xE000_02C7u32 as i32);
        let err = perform_write(
            &transport,
            DDC_CHIP_ADDRESS_DEFAULT,
            &input_packet(17),
            Duration::ZERO,
        )
        .expect_err("tool error");

        match err {
            DisplayError::Tool(message) => assert!(
                message.contains("0xe00002c7"),
                "expected the IOReturn in {message:?}"
            ),
            other => panic!("expected Tool, got {other:?}"),
        }
        // The loop stops at the first failure.
        assert_eq!(transport.calls.borrow().len(), 1);
    }

    /// These two are `m1ddc`'s and the displays here were tuned against them;
    /// changing either is a hardware decision, not a cleanup.
    #[test]
    fn the_write_cadence_is_the_one_m1ddc_uses() {
        assert_eq!(DDC_ITERATIONS, 2);
        assert_eq!(DDC_WAIT, Duration::from_millis(10));
    }

    /// The same kind of guard for the read: `m1ddc` waits five times as long
    /// before reading behind an MCDP29xx bridge, and only before reading. A
    /// "simplification" that makes the two waits one would either lose the
    /// replies on those bridges or halve the write cadence everywhere.
    #[test]
    fn only_the_read_waits_longer_behind_an_mcdp29xx_bridge() {
        assert_ne!(
            DDC_MCDP_READ_WAIT, DDC_WAIT,
            "the pre-read wait is deliberately not the write wait",
        );
        assert_eq!(DDC_MCDP_READ_WAIT, Duration::from_millis(50));
        assert_eq!(
            read_wait(DDC_CHIP_ADDRESS_MCDP29XX, DDC_WAIT),
            DDC_MCDP_READ_WAIT
        );
        assert_eq!(read_wait(DDC_CHIP_ADDRESS_DEFAULT, DDC_WAIT), DDC_WAIT);
        // The tests' "do not sleep" stays "do not sleep" on every bridge.
        assert!(read_wait(DDC_CHIP_ADDRESS_MCDP29XX, Duration::ZERO).is_zero());
    }

    /// The config carries `edid_uuid` as a string, so "not configured" comes
    /// in as `""`; the runtime hands it straight over.
    #[test]
    fn an_empty_configured_uuid_means_the_first_display() {
        assert_eq!(
            DdcDisplay::new(Some(String::new()), "A".into()).edid_uuid,
            None
        );
        assert_eq!(DdcDisplay::new(None, "A".into()).edid_uuid, None);
        assert_eq!(
            DdcDisplay::new(Some("AAAA".into()), "A".into()).edid_uuid,
            Some("AAAA".to_string())
        );
    }

    #[test]
    // Spelled out the way `m1ddc`'s `prepareDDCRead` computes it: unlike the
    // write, the read checksum does *not* cover the I2C data address.
    #[allow(clippy::identity_op)]
    fn the_read_request_is_byte_for_byte_what_m1ddc_sends() {
        let expected = [0x82, 0x01, 0x60, 0x6E ^ 0x82 ^ 0x01 ^ 0x60 ^ 0x00];
        assert_eq!(read_input_packet(), expected);
        // The write's checksum seed is the one that differs.
        assert_ne!(read_input_packet()[3], 0x6E ^ 0x51 ^ 0x82 ^ 0x01 ^ 0x60);
    }

    #[test]
    fn a_feature_reply_carries_the_current_input_source() {
        // The reply the AOC sent while showing this Mac's HDMI 1.
        assert_eq!(parse_feature_reply(0x60, &feature_reply(0x11)), Some(17));
        // Both value bytes are read, big-endian.
        let mut wide = feature_reply(0x11);
        wide[8] = 0x01;
        assert_eq!(parse_feature_reply(0x60, &wide), Some(0x0111));
    }

    #[test]
    fn a_reply_that_is_not_the_answer_we_asked_for_is_refused() {
        for (what, byte, value) in [
            ("not a feature reply", 2usize, 0x03u8),
            ("an error result", 3, 0x01),
            ("another VCP code", 4, 0x10),
        ] {
            let mut reply = feature_reply(0x11);
            reply[byte] = value;
            assert_eq!(parse_feature_reply(0x60, &reply), None, "{what}");
        }
        // A reply cut short carries no value to read.
        let short = feature_reply(0x11);
        for len in 0..10 {
            assert_eq!(parse_feature_reply(0x60, &short[..len]), None, "len {len}");
        }
        // And we only accept the feature we asked about.
        assert_eq!(parse_feature_reply(0x10, &feature_reply(0x11)), None);
    }

    #[test]
    fn a_read_asks_twice_and_then_listens() {
        let live = Rc::new(Cell::new(0));
        let transport = FakeTransport::open(&live, Ok(feature_reply(0x11)));

        let got = perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO);

        assert_eq!(got, Some(17));
        let calls = transport.calls.borrow();
        assert_eq!(
            *calls,
            vec![
                Call::Write {
                    chip: 0x37,
                    addr: 0x51,
                    data: read_input_packet().to_vec(),
                },
                Call::Write {
                    chip: 0x37,
                    addr: 0x51,
                    data: read_input_packet().to_vec(),
                },
                Call::Read {
                    chip: 0x37,
                    addr: 0x51,
                    len: 12,
                },
            ],
            "the request goes out twice, then one 12-byte reply is read",
        );
    }

    #[test]
    fn a_display_that_will_not_answer_is_unknown_and_holds_nothing_open() {
        let live = Rc::new(Cell::new(0));
        let got = {
            let transport = FakeTransport::open(&live, Err(0xE000_02C7u32 as i32));
            assert_eq!(live.get(), 1, "the fake holds its channel while reading");
            perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO)
        };

        assert_eq!(got, None, "a failed read is 'unknown', not a guess");
        assert_eq!(
            live.get(),
            0,
            "a failed read must release every channel it opened",
        );
    }

    #[test]
    fn a_request_the_display_refuses_is_unknown() {
        let transport = FakeTransport::refusing(0xE000_02C7u32 as i32);
        assert_eq!(
            perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO),
            None,
        );
        // Nothing is read once the request failed.
        assert_eq!(transport.writes().len(), 1);
        assert_eq!(transport.calls.borrow().len(), 1);
    }

    /// Right after a switch the channel is not ready yet, so one failed read
    /// is not the answer: the second attempt is the one that gets a value.
    #[test]
    fn a_read_that_fails_is_asked_once_more() {
        let live = Rc::new(Cell::new(0));
        let rounds = Cell::new(0usize);
        let replies = RefCell::new(vec![Err(0xE000_02C7u32 as i32), Ok(feature_reply(0x11))]);

        let got = read_retrying(
            || {
                rounds.set(rounds.get() + 1);
                let reply = replies.borrow_mut().remove(0);
                let transport = FakeTransport::open(&live, reply);
                perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO)
            },
            // The tests' "do not sleep"; the real pause is DDC_READ_RETRY_WAIT.
            Duration::ZERO,
        );

        assert_eq!(got, Some(17), "the second attempt's answer counts");
        assert_eq!(
            rounds.get(),
            2,
            "the whole attempt is repeated, walk and all"
        );
        assert_eq!(live.get(), 0, "both attempts released their channel");
    }

    #[test]
    fn a_read_that_succeeds_is_not_repeated() {
        let live = Rc::new(Cell::new(0));
        let rounds = Cell::new(0usize);

        let got = read_retrying(
            || {
                rounds.set(rounds.get() + 1);
                let transport = FakeTransport::open(&live, Ok(feature_reply(0x11)));
                perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO)
            },
            Duration::ZERO,
        );

        assert_eq!(got, Some(17));
        assert_eq!(rounds.get(), 1, "a display that answers is asked once");
    }

    #[test]
    fn a_display_that_fails_twice_is_unknown_and_holds_nothing_open() {
        let live = Rc::new(Cell::new(0));
        let rounds = Cell::new(0usize);

        let got = read_retrying(
            || {
                rounds.set(rounds.get() + 1);
                let transport = FakeTransport::open(&live, Err(0xE000_02C7u32 as i32));
                assert_eq!(live.get(), 1, "the fake holds its channel while reading");
                perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO)
            },
            Duration::ZERO,
        );

        assert_eq!(got, None, "two failures are 'unknown', not a guess");
        assert_eq!(rounds.get(), DDC_READ_ATTEMPTS, "and no third attempt");
        assert_eq!(
            live.get(),
            0,
            "a failed read must release every channel it opened",
        );
    }

    /// The retry has to fit the runtime's decision budget: two healthy reads
    /// (~70 ms each) plus this pause must stay well inside it.
    #[test]
    fn the_retry_pause_is_the_measured_one() {
        assert_eq!(DDC_READ_ATTEMPTS, 2);
        assert_eq!(DDC_READ_RETRY_WAIT, Duration::from_millis(400));
    }

    #[test]
    fn an_input_source_that_does_not_fit_a_byte_is_unknown() {
        // The config stores input codes as `u8`, so a wider value is one we
        // could never write back; treat it as "did not understand".
        let mut wide = feature_reply(0x11);
        wide[8] = 0x01;
        let live = Rc::new(Cell::new(0));
        let transport = FakeTransport::open(&live, Ok(wide));
        assert_eq!(
            perform_read(&transport, DDC_CHIP_ADDRESS_DEFAULT, Duration::ZERO),
            None,
        );
    }

    #[test]
    fn a_name_that_does_not_fit_io_name_t_is_refused() {
        let plane = name_buf(kIOServicePlane.to_bytes()).expect("IOService fits");
        assert_eq!(plane[0], b'I' as c_char);
        // 128 bytes leave no room for the nul IOKit reads up to.
        assert!(name_buf(&[b'x'; NAME_BUF_LEN]).is_none());
        assert!(name_buf(&[b'x'; NAME_BUF_LEN - 1]).is_some());
    }
}
