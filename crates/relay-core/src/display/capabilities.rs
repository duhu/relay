//! Asking a display what it can do: the DDC/CI capabilities string (0xF3).

use std::time::Duration;

use super::ddc::{self, DdcTransport, DDC_DATA_ADDRESS, DDC_WAIT, TOOL_TIMEOUT, VCP_INPUT_SOURCE};

/// One input source a display says it has.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InputSource {
    pub code: u8,
    /// What MCCS calls `code`, or `None` for a code the standard does not
    /// name. The UI shows the bare number for those.
    pub name: Option<&'static str>,
}

/// The capabilities request's VCP opcode, and the opcode its reply carries.
const VCP_CAPABILITIES: u8 = 0xF3;
const CAPS_REPLY: u8 = 0xE3;

/// Big enough for the longest fragment (32 bytes) plus its envelope.
const CAPS_REPLY_LEN: usize = 64;

/// The most fragments we will ask for, and the longest string we will keep.
///
/// The display we have needs 24 rounds for 338 bytes; these are the guards
/// against one that never sends its end marker.
const CAPS_MAX_ROUNDS: usize = 64;
const CAPS_MAX_BYTES: usize = 2048;

/// The MCCS 2.2 names for VCP 0x60's values.
///
/// These are the standard's names, not the labels printed next to the
/// monitor's sockets — the display does not say which physical port is
/// "DisplayPort 1". The settings window covers that by marking the input the
/// monitor is showing right now.
pub fn input_source_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x01 => "VGA 1",
        0x02 => "VGA 2",
        0x03 => "DVI 1",
        0x04 => "DVI 2",
        0x05 => "Composite 1",
        0x06 => "Composite 2",
        0x07 => "S-Video 1",
        0x08 => "S-Video 2",
        0x09 => "Tuner 1",
        0x0A => "Tuner 2",
        0x0B => "Tuner 3",
        0x0C => "Component 1",
        0x0D => "Component 2",
        0x0E => "Component 3",
        0x0F => "DisplayPort 1",
        0x10 => "DisplayPort 2",
        0x11 => "HDMI 1",
        0x12 => "HDMI 2",
        _ => return None,
    })
}

/// The body of `header(...)`, with brackets balanced, or `None`.
///
/// The search is deliberately not anchored to the top level: it takes the
/// first literal occurrence of `header` anywhere in the string, so
/// [`parse_input_codes`] relies on no earlier section containing the text
/// `vcp(`.
fn section_body<'a>(caps: &'a str, header: &str) -> Option<&'a str> {
    let start = caps.find(header)? + header.len();
    let mut depth = 1usize;
    for (offset, ch) in caps[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&caps[start..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `0F 10 11 12 ` → `[15, 16, 17, 18]`, in the order given, without repeats.
fn parse_hex_list(values: &str) -> Vec<u8> {
    let mut codes = Vec::new();
    for token in values.split_ascii_whitespace() {
        if let Ok(code) = u8::from_str_radix(token, 16) {
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes
}

/// The input sources VCP 0x60 lists, straight out of a capabilities string.
///
/// The `vcp(...)` section is a flat run of two-digit hex feature codes, each
/// optionally followed by a bracketed list of the values it accepts:
///
/// ```text
/// vcp(02 04 14(01 05 06) 60(0F 10 11 12 ) 86(01 02))
/// ```
///
/// Only feature 0x60's values are wanted. Searching for the text `60(` would
/// also hit a `60` sitting inside some other feature's value list, so the
/// section is walked properly. Anything that does not parse yields an empty
/// list: a guessed input source would switch the screen to the wrong machine.
pub(crate) fn parse_input_codes(caps: &str) -> Vec<u8> {
    let Some(vcp) = section_body(caps, "vcp(") else {
        return Vec::new();
    };
    let bytes = vcp.as_bytes();
    let mut at = 0usize;

    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_hexdigit() {
            at += 1;
        }
        if start == at {
            // Neither a separator nor a feature code: the string is not
            // shaped the way the standard says.
            return Vec::new();
        }
        let code = u8::from_str_radix(&vcp[start..at], 16).ok();

        if at < bytes.len() && bytes[at] == b'(' {
            let Some(values) = section_body(&vcp[at..], "(") else {
                return Vec::new();
            };
            // The body, plus the two brackets around it.
            at += values.len() + 2;
            if code == Some(VCP_INPUT_SOURCE) {
                return parse_hex_list(values);
            }
        }
    }
    Vec::new()
}

/// The DDC/CI "Capabilities Request" for the fragment starting at `offset`.
///
/// Same checksum rule as the input-source read: seeded with `0x6E` alone and
/// **not** covering the I2C data address, unlike the write packet.
pub(crate) fn caps_packet(offset: u16) -> [u8; 5] {
    let [hi, lo] = offset.to_be_bytes();
    let body = [0x83u8, VCP_CAPABILITIES, hi, lo];
    let checksum = body.iter().fold(0x6Eu8, |sum, byte| sum ^ byte);
    [body[0], body[1], body[2], body[3], checksum]
}

/// The echoed offset and the payload of a capabilities reply, or `None`.
///
/// `buf[1]` holds the message length with its high bit set; three of those
/// bytes are the opcode and the echoed offset, so the rest is payload. A
/// length of exactly three is the standard's end marker: an empty fragment,
/// and 35 is the ceiling — three bytes of envelope plus the 32 of payload a
/// fragment may carry. A longer claim is refused rather than trusted, because
/// honouring it would splice the tail of the read buffer, which is bus data
/// and not ours, into the capabilities string.
pub(crate) fn parse_fragment(buf: &[u8]) -> Option<(u16, &[u8])> {
    if buf.len() < 5 || buf[2] != CAPS_REPLY {
        return None;
    }
    if buf[1] & 0x80 == 0 {
        return None;
    }
    let len = usize::from(buf[1] & 0x7F);
    // Three bytes of envelope plus at most 32 of payload: a longer claim means
    // the tail of the read buffer, which is bus data and not ours, would be
    // spliced into the capabilities string.
    if !(3..=35).contains(&len) || 2 + len > buf.len() {
        return None;
    }
    Some((u16::from_be_bytes([buf[3], buf[4]]), &buf[5..2 + len]))
}

/// Asks the display for its whole capabilities string, fragment by fragment.
///
/// `None` for every failure, including a display that never sends the empty
/// fragment that ends the string: a half-read capabilities string would list
/// fewer input sources than the display has, which is worse than listing none.
pub(crate) fn collect_capabilities(
    transport: &impl DdcTransport,
    chip: u32,
    wait: Duration,
) -> Option<String> {
    let mut assembled: Vec<u8> = Vec::new();
    let mut offset: u16 = 0;

    for _ in 0..CAPS_MAX_ROUNDS {
        if let Err(err) = ddc::perform_write(transport, chip, &caps_packet(offset), wait) {
            tracing::debug!(%err, "the display refused the capabilities request");
            return None;
        }
        let pause = ddc::read_wait(chip, wait);
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }

        let mut reply = [0u8; CAPS_REPLY_LEN];
        if let Err(ret) = transport.read(chip, DDC_DATA_ADDRESS, &mut reply) {
            tracing::debug!(ret = format!("0x{ret:08x}"), "IOAVServiceReadI2C failed");
            return None;
        }
        let Some((echoed, data)) = parse_fragment(&reply) else {
            tracing::debug!(?reply, "the reply is not a capabilities fragment");
            return None;
        };
        if echoed != offset {
            tracing::debug!(echoed, offset, "the display answered a different offset");
            return None;
        }
        if data.is_empty() {
            return String::from_utf8(assembled).ok();
        }
        if assembled.len() + data.len() > CAPS_MAX_BYTES {
            tracing::debug!("the capabilities string is longer than we will read");
            return None;
        }
        assembled.extend_from_slice(data);
        offset = offset.saturating_add(data.len() as u16);
    }

    tracing::debug!("the display never finished its capabilities string");
    None
}

/// Every input source `edid_uuid` says it has, or an empty list.
///
/// An empty list means "the display would not say": it may not implement the
/// capabilities request at all. The caller falls back to letting the user type
/// the code, so this is never an error.
///
/// Slow by this module's standards — the display we have needs about 1.7
/// seconds — so it belongs on the settings window's timeline, never in a
/// switch.
pub async fn read_input_sources(edid_uuid: Option<String>) -> Vec<InputSource> {
    let read =
        tokio::task::spawn_blocking(move || read_input_sources_blocking(edid_uuid.as_deref()));

    match tokio::time::timeout(TOOL_TIMEOUT, read).await {
        Err(_elapsed) => {
            tracing::debug!("the display did not finish its capabilities in time");
            Vec::new()
        }
        Ok(Err(err)) => {
            tracing::debug!(%err, "the capabilities read panicked");
            Vec::new()
        }
        Ok(Ok(sources)) => sources,
    }
}

/// The blocking half of [`read_input_sources`]: discover, pick, ask, parse.
fn read_input_sources_blocking(wanted: Option<&str>) -> Vec<InputSource> {
    let displays = ddc::scan_displays();
    let display = match ddc::pick(&displays, wanted) {
        Ok(display) => display,
        Err(err) => {
            tracing::debug!(%err, "cannot read the display's capabilities");
            return Vec::new();
        }
    };
    let transport = match ddc::IoAvTransport::open(&display.proxy) {
        Ok(transport) => transport,
        Err(err) => {
            tracing::debug!(%err, "cannot open the display's DDC channel");
            return Vec::new();
        }
    };
    let chip = transport.chip_address;
    let Some(caps) = collect_capabilities(&transport, chip, DDC_WAIT) else {
        return Vec::new();
    };
    tracing::debug!(caps, "the display's capabilities");
    parse_input_codes(&caps)
        .into_iter()
        .map(|code| InputSource {
            code,
            name: input_source_name(code),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The string this project's AOC U2790R3B actually returned on
    /// 2026-09-21. Kept verbatim: a parser that cannot read the one display
    /// we have is not worth shipping.
    const AOC: &str = "(prot(monitor)type(LCD)model(U2790PC)cmds(01 02 03 07 0C E3 F3)vcp(02 04 05 08 0C 10 12 14(01 05 06 08 0B) 16 18 1A 52 60(0F 10 11 12 ) 86(01 02 05 0B 0C 0D 0E 0F 10 11 12 13 14) AC AE B2 B6 C6 C8 CA CC(01 02 03 04 05 06 07 09 0A 0B 0C 0D 0E 12 14 16 1E) D6(01 04 05) DC(00 0B 0C 0D 0E 0F 10) DF ED FF)mswhql(1)asset_eep(40)mccs_ver(2.2))";

    #[test]
    fn reads_the_real_monitor_s_input_list() {
        assert_eq!(parse_input_codes(AOC), vec![0x0F, 0x10, 0x11, 0x12]);
    }

    #[test]
    fn a_value_list_inside_another_feature_is_not_mistaken_for_0x60() {
        // 0x14's list contains 60, and 0xCC's contains 0C — neither is the
        // input source feature.
        let caps = "(vcp(14(01 60 05) CC(0C 60) 60(11 12)))";
        assert_eq!(parse_input_codes(caps), vec![0x11, 0x12]);
    }

    #[test]
    fn an_input_list_outside_the_vcp_section_is_not_read() {
        // A model name that happens to contain "60(" — what a `find("60(")`
        // implementation would swallow whole.
        assert_eq!(parse_input_codes("(model(E2460(11 12)))"), Vec::<u8>::new());
        // And the same decoy ahead of a real list must not shift the answer.
        assert_eq!(
            parse_input_codes("(model(E2460(X))vcp(60(11 12)))"),
            vec![0x11, 0x12]
        );
    }

    #[test]
    fn no_input_source_feature_means_no_list() {
        assert_eq!(parse_input_codes("(vcp(02 04 10 12))"), Vec::<u8>::new());
    }

    #[test]
    fn an_input_source_feature_without_values_means_no_list() {
        assert_eq!(parse_input_codes("(vcp(02 60 12))"), Vec::<u8>::new());
    }

    #[test]
    fn a_string_without_a_vcp_section_means_no_list() {
        assert_eq!(
            parse_input_codes("(prot(monitor)type(LCD))"),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn an_unclosed_bracket_means_no_list() {
        assert_eq!(parse_input_codes("(vcp(02 60(11 12"), Vec::<u8>::new());
    }

    #[test]
    fn rubbish_inside_the_vcp_section_means_no_list() {
        assert_eq!(parse_input_codes("(vcp(02 ZZ 60(11)))"), Vec::<u8>::new());
    }

    #[test]
    fn a_repeated_code_is_listed_once() {
        assert_eq!(parse_input_codes("(vcp(60(11 11 12)))"), vec![0x11, 0x12]);
    }

    #[test]
    fn the_standard_names_the_codes_this_project_uses() {
        assert_eq!(input_source_name(0x0F), Some("DisplayPort 1"));
        assert_eq!(input_source_name(0x10), Some("DisplayPort 2"));
        assert_eq!(input_source_name(0x11), Some("HDMI 1"));
        assert_eq!(input_source_name(0x12), Some("HDMI 2"));
        assert_eq!(input_source_name(0x77), None);
    }

    use std::cell::RefCell;

    /// A display that hands out one prepared reply per read, in order.
    struct FragmentingTransport {
        replies: RefCell<Vec<Vec<u8>>>,
        requested: RefCell<Vec<u16>>,
    }

    impl FragmentingTransport {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Self {
                replies: RefCell::new(replies),
                requested: RefCell::default(),
            }
        }
    }

    impl DdcTransport for FragmentingTransport {
        fn write(&self, _chip: u32, _addr: u32, data: &[u8]) -> Result<(), i32> {
            // Every request carries the offset it is asking for; recording it
            // is how the tests check the walk moves forward. `perform_write`
            // sends each packet twice, so only a change is worth recording.
            if data.len() == 5 && data[1] == VCP_CAPABILITIES {
                let offset = u16::from_be_bytes([data[2], data[3]]);
                let mut requested = self.requested.borrow_mut();
                if requested.last() != Some(&offset) {
                    requested.push(offset);
                }
            }
            Ok(())
        }

        fn read(&self, _chip: u32, _addr: u32, buf: &mut [u8]) -> Result<(), i32> {
            let mut replies = self.replies.borrow_mut();
            if replies.is_empty() {
                return Err(-1);
            }
            let reply = replies.remove(0);
            buf.fill(0);
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(())
        }
    }

    /// A capabilities reply carrying `data` at `offset`.
    fn fragment(offset: u16, data: &[u8]) -> Vec<u8> {
        let [hi, lo] = offset.to_be_bytes();
        let mut reply = vec![0x6E, 0x80 | (data.len() as u8 + 3), CAPS_REPLY, hi, lo];
        reply.extend_from_slice(data);
        reply.push(0x00); // The checksum; nothing reads it.
        reply
    }

    #[test]
    fn the_request_packet_matches_the_one_the_monitor_answered() {
        // Checksum seeded with 0x6E alone, exactly like the input-source read.
        assert_eq!(caps_packet(0), [0x83, 0xF3, 0x00, 0x00, 0x6E ^ 0x83 ^ 0xF3]);
        assert_eq!(
            caps_packet(0x0120),
            [0x83, 0xF3, 0x01, 0x20, 0x6E ^ 0x83 ^ 0xF3 ^ 0x01 ^ 0x20]
        );
    }

    #[test]
    fn fragments_are_assembled_until_an_empty_one_arrives() {
        let transport = FragmentingTransport::new(vec![
            fragment(0, b"(vcp(60(11 "),
            fragment(11, b"12)))"),
            fragment(16, b""),
        ]);
        assert_eq!(
            collect_capabilities(&transport, 0x37, Duration::ZERO).as_deref(),
            Some("(vcp(60(11 12)))")
        );
        assert_eq!(*transport.requested.borrow(), vec![0, 11, 16]);
    }

    #[test]
    fn a_reply_for_a_different_offset_is_refused() {
        let transport = FragmentingTransport::new(vec![fragment(99, b"(vcp(")]);
        assert_eq!(collect_capabilities(&transport, 0x37, Duration::ZERO), None);
    }

    #[test]
    fn a_reply_that_is_not_a_capabilities_fragment_is_refused() {
        // A VCP feature reply, which is what arrives when the request was
        // misunderstood.
        let reply = vec![0x6E, 0x88, 0x02, 0x00, 0x60, 0x00, 0x00, 0x12, 0x00, 0x11];
        let transport = FragmentingTransport::new(vec![reply]);
        assert_eq!(collect_capabilities(&transport, 0x37, Duration::ZERO), None);
    }

    #[test]
    fn a_display_that_never_ends_is_given_up_on() {
        let replies = (0..CAPS_MAX_ROUNDS + 1)
            .map(|round| fragment((round * 4) as u16, b"aaaa"))
            .collect();
        let transport = FragmentingTransport::new(replies);
        assert_eq!(collect_capabilities(&transport, 0x37, Duration::ZERO), None);
        assert_eq!(transport.requested.borrow().len(), CAPS_MAX_ROUNDS);
    }

    #[test]
    fn a_fragment_longer_than_one_can_be_is_refused() {
        // The length byte claims 40 where the protocol allows at most 35.
        let reply = vec![0x6E, 0x80 | 40, CAPS_REPLY, 0x00, 0x00, b'x'];
        assert_eq!(parse_fragment(&reply), None);
        let transport = FragmentingTransport::new(vec![reply]);
        assert_eq!(collect_capabilities(&transport, 0x37, Duration::ZERO), None);
    }
}
