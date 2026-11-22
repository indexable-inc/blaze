//! SETTINGS (RFC 9113 section 6.5): the values the peer sent, the RFC's limits on each, and our
//! own [`Limits`], the caps every buffer and count here is held to (rule 18).
//!
//! Section 6.5.2, the defined settings and their initial values:
//! - SETTINGS_HEADER_TABLE_SIZE (0x01): "The initial value is 4,096 octets."
//! - SETTINGS_ENABLE_PUSH (0x02): "The initial value of SETTINGS_ENABLE_PUSH is 1. ... Any value
//!   other than 0 or 1 MUST be treated as a connection error of type PROTOCOL_ERROR. ... A client
//!   MUST treat receipt of a SETTINGS frame with SETTINGS_ENABLE_PUSH set to 1 as a connection
//!   error of type PROTOCOL_ERROR."
//! - SETTINGS_MAX_CONCURRENT_STREAMS (0x03): "Initially, there is no limit to this value."
//! - SETTINGS_INITIAL_WINDOW_SIZE (0x04): "The initial value is 2^16-1 (65,535) octets. ... Values
//!   above the maximum flow-control window size of 2^31-1 MUST be treated as a connection error
//!   of type FLOW_CONTROL_ERROR."
//! - SETTINGS_MAX_FRAME_SIZE (0x05): "The initial value is 2^14 (16,384) octets. The value
//!   advertised by an endpoint MUST be between this initial value and the maximum allowed frame
//!   size (2^24-1 or 16,777,215 octets), inclusive. Values outside this range MUST be treated as a
//!   connection error of type PROTOCOL_ERROR."
//! - SETTINGS_MAX_HEADER_LIST_SIZE (0x06): "The initial value of this setting is unlimited."
//!
//! "An endpoint that receives a SETTINGS frame with any unknown or unsupported identifier MUST
//! ignore that setting."

#![blaze::sans_io]

use core::{fmt, time::Duration};

use crate::{
    conn::Role,
    error::{ErrorCode, Rule},
    frame::{MAX_31, SettingEntry, setting},
    hpack::Strings,
};

/// The RFC's initial SETTINGS_INITIAL_WINDOW_SIZE, which is also every connection window's
/// initial size (section 6.9.2).
pub const DEFAULT_WINDOW: u32 = 65_535;
/// The RFC's initial and least SETTINGS_MAX_FRAME_SIZE.
pub const DEFAULT_FRAME: u32 = 16_384;
/// The RFC's largest SETTINGS_MAX_FRAME_SIZE.
pub const MOST_FRAME: u32 = 16_777_215;
/// The RFC's initial SETTINGS_HEADER_TABLE_SIZE.
pub const DEFAULT_TABLE: u32 = 4_096;

/// The settings one side sent, as in force.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    pub header_table_size: u32,
    pub enable_push: bool,
    /// None: no limit.
    pub max_concurrent_streams: Option<u32>,
    pub initial_window_size: u32,
    pub max_frame_size: u32,
    /// None: no limit.
    pub max_header_list_size: Option<u32>,
}

impl Settings {
    /// The initial values of section 6.5.2, in force until a SETTINGS frame says otherwise.
    pub const INITIAL: Settings = Settings {
        header_table_size: DEFAULT_TABLE,
        enable_push: true,
        max_concurrent_streams: None,
        initial_window_size: DEFAULT_WINDOW,
        max_frame_size: DEFAULT_FRAME,
        max_header_list_size: None,
    };

    /// Takes one entry of a SETTINGS frame received by `receiver`, or says which rule it broke
    /// and the code it is refused with.
    pub fn apply(&mut self, entry: SettingEntry, receiver: Role) -> Result<(), (Rule, ErrorCode)> {
        let bad = |code| (Rule::SettingsValue { id: entry.id, value: entry.value }, code);
        match entry.id {
            setting::HEADER_TABLE_SIZE => self.header_table_size = entry.value,
            setting::ENABLE_PUSH => match (entry.value, receiver) {
                (0, _) => self.enable_push = false,
                (1, Role::Server) => self.enable_push = true,
                _ => return Err(bad(ErrorCode::PROTOCOL_ERROR)),
            },
            setting::MAX_CONCURRENT_STREAMS => self.max_concurrent_streams = Some(entry.value),
            setting::INITIAL_WINDOW_SIZE => {
                if entry.value > MAX_31 {
                    return Err(bad(ErrorCode::FLOW_CONTROL_ERROR));
                }
                self.initial_window_size = entry.value;
            }
            setting::MAX_FRAME_SIZE => {
                if !(DEFAULT_FRAME..=MOST_FRAME).contains(&entry.value) {
                    return Err(bad(ErrorCode::PROTOCOL_ERROR));
                }
                self.max_frame_size = entry.value;
            }
            setting::MAX_HEADER_LIST_SIZE => self.max_header_list_size = Some(entry.value),
            // Unknown: ignored, as the section says.
            _ => {}
        }
        Ok(())
    }
}

/// Our caps, every one the caller's choice ([`Limits::DEFAULT`] is a reasoned start). The
/// settings we send are made from them: SETTINGS_HEADER_TABLE_SIZE = `hpack_table`,
/// SETTINGS_ENABLE_PUSH = 0, SETTINGS_MAX_CONCURRENT_STREAMS = `concurrent_streams`,
/// SETTINGS_INITIAL_WINDOW_SIZE = `stream_window`, SETTINGS_MAX_FRAME_SIZE = `frame_size`,
/// SETTINGS_MAX_HEADER_LIST_SIZE = `header_list`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of a decoded field list (RFC 7541 section 4.1 accounting). Past it: a
    /// stream error ENHANCE_YOUR_CALM, the connection kept (HPACK stays in step).
    pub header_list: u32,
    /// The most encoded bytes of one field block (HEADERS and its CONTINUATIONs). Past it: a
    /// connection error ENHANCE_YOUR_CALM (the block cannot be decoded, so HPACK would part
    /// ways).
    pub header_block: u32,
    /// The most CONTINUATION frames in one field block (empty ones grow no buffer but cost
    /// time). Past it: a connection error ENHANCE_YOUR_CALM.
    pub continuations: u16,
    /// The most streams the peer may open at once. Past it: a stream error REFUSED_STREAM.
    pub concurrent_streams: u32,
    /// The largest frame payload we take. Past it: a connection error FRAME_SIZE_ERROR.
    pub frame_size: u32,
    /// The most bytes of the decoder's dynamic table.
    pub hpack_table: u32,
    /// The most bytes of the encoder's dynamic table, whatever the peer allows.
    pub hpack_encoder_table: u32,
    /// Each stream's receive window, and so the most body bytes buffered per stream.
    pub stream_window: u32,
    /// The connection's receive window, and so the most body bytes buffered in all.
    pub conn_window: u32,
    /// The most DATA bytes queued for [`crate::Connection::transmit`]: past it `send_body`
    /// takes nothing more.
    pub out_buffer: u32,
    /// The most SETTINGS and PING acknowledgements owed and not yet transmitted. Past it: a
    /// connection error ENHANCE_YOUR_CALM (the SETTINGS and PING floods).
    pub pending_acks: u16,
    /// The most RST_STREAM frames owed to the peer and not yet transmitted. Past it: a
    /// connection error ENHANCE_YOUR_CALM.
    pub pending_resets: u16,
    /// The most RST_STREAM frames the peer may send per `peer_resets_window` (a budget that
    /// refills evenly over the window). Past it: a connection error ENHANCE_YOUR_CALM (the
    /// rapid reset).
    pub peer_resets: u16,
    pub peer_resets_window: Duration,
    /// The most informational (1xx) responses on one stream. Past it: a stream error
    /// ENHANCE_YOUR_CALM.
    pub informational: u8,
    /// The most PINGs of ours unanswered.
    pub pings: u16,
    /// How long our SETTINGS may go unacknowledged: then a connection error SETTINGS_TIMEOUT.
    pub settings_timeout: Duration,
    /// How long a stream may have bytes to send and no window: then it is reset with CANCEL.
    pub stall_timeout: Duration,
    /// How HPACK writes strings.
    pub strings: Strings,
}

impl Limits {
    /// The defaults: enough for a remote cache's many concurrent record fetches, small enough
    /// that a hostile peer holds a connection to about `conn_window` + 64 KiB of buffers.
    pub const DEFAULT: Limits = Limits {
        header_list: 64 << 10,
        header_block: 64 << 10,
        continuations: 32,
        concurrent_streams: 128,
        frame_size: DEFAULT_FRAME,
        hpack_table: DEFAULT_TABLE,
        hpack_encoder_table: DEFAULT_TABLE,
        stream_window: 256 << 10,
        conn_window: 4 << 20,
        out_buffer: 1 << 20,
        pending_acks: 16,
        pending_resets: 64,
        peer_resets: 128,
        peer_resets_window: Duration::from_secs(10),
        informational: 8,
        pings: 4,
        settings_timeout: Duration::from_secs(10),
        stall_timeout: Duration::from_secs(30),
        strings: Strings::Shorter,
    };

    /// Whether every limit is one the RFC allows and the machine can honour.
    pub fn check(&self) -> Result<(), LimitsFault> {
        let within = |what: &'static str, value: u64, least: u64, most: u64| {
            if (least..=most).contains(&value) { Ok(()) } else { Err(LimitsFault { what, value, least, most }) }
        };
        // A peer may send 65,535 bytes on a stream before it has our SETTINGS (section 6.9.2):
        // a smaller stream window would be one it breaks without fault.
        within("stream_window", u64::from(self.stream_window), u64::from(DEFAULT_WINDOW), u64::from(MAX_31))?;
        within("conn_window", u64::from(self.conn_window), u64::from(DEFAULT_WINDOW), u64::from(MAX_31))?;
        within("frame_size", u64::from(self.frame_size), u64::from(DEFAULT_FRAME), u64::from(MOST_FRAME))?;
        within("header_block", u64::from(self.header_block), 1, u64::from(u32::MAX))?;
        within("continuations", u64::from(self.continuations), 1, u64::from(u16::MAX))?;
        within("pending_acks", u64::from(self.pending_acks), 1, u64::from(u16::MAX))?;
        within("pending_resets", u64::from(self.pending_resets), 1, u64::from(u16::MAX))?;
        within("peer_resets", u64::from(self.peer_resets), 1, u64::from(u16::MAX))?;
        within("out_buffer", u64::from(self.out_buffer), 1, u64::from(u32::MAX))?;
        Ok(())
    }

    /// The SETTINGS entries we send.
    pub fn entries(&self) -> [SettingEntry; 6] {
        [
            SettingEntry { id: setting::HEADER_TABLE_SIZE, value: self.hpack_table },
            SettingEntry { id: setting::ENABLE_PUSH, value: 0 },
            SettingEntry { id: setting::MAX_CONCURRENT_STREAMS, value: self.concurrent_streams },
            SettingEntry { id: setting::INITIAL_WINDOW_SIZE, value: self.stream_window },
            SettingEntry { id: setting::MAX_FRAME_SIZE, value: self.frame_size },
            SettingEntry { id: setting::MAX_HEADER_LIST_SIZE, value: self.header_list },
        ]
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DEFAULT
    }
}

/// A limit outside what the RFC or the machine allows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LimitsFault {
    pub what: &'static str,
    pub value: u64,
    pub least: u64,
    pub most: u64,
}

impl fmt::Display for LimitsFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "limit `{}` = {} is outside {}..={}", self.what, self.value, self.least, self.most)
    }
}
