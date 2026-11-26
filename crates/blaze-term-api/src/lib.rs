//! Pure `#![no_std]`, zero-unsafe, sans-IO terminal symbol and entity registry protocol.
//!
//! Provides:
//! - Content-addressable / PUA-mapped custom symbols (`Svg`, `FontGlyph`, `Png`, `Text`)
//! - Entity metadata bindings (`File` by `(dev, ino)`, `Host`, `Repo`, `Branch`, `Process`, `Custom`)
//! - JSON-RPC request dispatcher (`TermRegistry::apply_json`)
//! - OSC 7770 escape-sequence handler (`TermRegistry::apply_osc_7770`) supporting both JSON and
//!   compact `;`-separated `k=v` syntax
//! - Deterministic canonical snapshot encoding and decoding (`encode_snapshot` / `decode_snapshot`)

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec};

mod protocol;
mod registry;

#[cfg(test)]
mod tests;

pub use registry::TermRegistry;

/// Protocol version advertised in `ping`, `info`, and persistent snapshots.
pub const PROTOCOL_VERSION: u32 = 1;
/// Inclusive start of the Private Use Area range reserved for registered symbols.
pub const PUA_START: u32 = 0xE100;
/// Inclusive end of the Private Use Area range reserved for registered symbols.
pub const PUA_END: u32 = 0xEFFF;
/// Maximum number of simultaneously registered symbols.
pub const MAX_SYMBOLS: usize = 2048;
/// Maximum number of simultaneously bound entities.
pub const MAX_ENTITIES: usize = 8192;
/// Maximum byte length of any inbound JSON-RPC, OSC 7770, or snapshot payload.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
/// OSC sequence numeric identifier (`ESC ] 7770 ; <payload> ESC \`).
pub const OSC_CODE: u32 = 7770;
/// Maximum byte length of a symbol slug name.
pub const MAX_SYMBOL_NAME_BYTES: usize = 96;

/// Category of an entity tracked by the terminal registry.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum EntityKind {
    File,
    Host,
    Repo,
    Branch,
    Process,
    Custom,
}

impl EntityKind {
    /// Canonical lowercase wire name for this kind.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Host => "host",
            Self::Repo => "repo",
            Self::Branch => "branch",
            Self::Process => "process",
            Self::Custom => "custom",
        }
    }

    /// Parse from canonical wire name.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "file" => Some(Self::File),
            "host" => Some(Self::Host),
            "repo" => Some(Self::Repo),
            "branch" => Some(Self::Branch),
            "process" => Some(Self::Process),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable identifier of a terminal entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityId<'a> {
    pub kind: EntityKind,
    pub id: AString<'a>,
}

impl<'a> EntityId<'a> {
    /// Construct an entity id from a kind and identifier string.
    pub fn new(kind: EntityKind, id: &str, alloc: Alloc<'a>) -> Self {
        Self { kind, id: String::from_str_in(id, alloc) }
    }

    /// Construct a `File` entity id formatted canonically as `"{dev}:{ino}"`.
    pub fn file(dev: u64, ino: u64, alloc: Alloc<'a>) -> Self {
        let mut buf = [0u8; 48];
        let formatted = format_u64_pair(dev, ino, &mut buf);
        Self { kind: EntityKind::File, id: String::from_str_in(formatted, alloc) }
    }

    /// Parse `(dev, ino)` if this is a `File` entity formatted as `"{dev}:{ino}"`.
    pub fn parse_file_dev_ino(&self) -> Option<(u64, u64)> {
        if self.kind != EntityKind::File {
            return None;
        }
        let (dev_s, ino_s) = self.id.as_str().split_once(':')?;
        let dev = parse_strict_u64(dev_s)?;
        let ino = parse_strict_u64(ino_s)?;
        Some((dev, ino))
    }

    /// Parse a compact `"<kind>:<id>"` string (e.g. `"file:16777234:9812345"`, `"host:hydra"`).
    pub fn parse_compact(s: &str, alloc: Alloc<'a>) -> Option<Self> {
        let (kind_s, rest) = s.split_once(':')?;
        if rest.is_empty() {
            return None;
        }
        let kind = EntityKind::from_str(kind_s)?;
        Some(Self::new(kind, rest, alloc))
    }

    /// Deep-clone this entity id into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> EntityId<'b> {
        EntityId { kind: self.kind, id: String::from_str_in(self.id.as_str(), alloc) }
    }
}

/// Visual representation source for a registered symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolSource<'a> {
    Svg {
        markup: AString<'a>,
        monochrome: bool,
    },
    FontGlyph {
        font_name: Option<AString<'a>>,
        font_data_b64: Option<AString<'a>>,
        codepoint: u32,
        tint: Option<[u8; 4]>,
    },
    Png {
        data_b64: AString<'a>,
    },
    Text {
        text: AString<'a>,
    },
}

impl<'a> SymbolSource<'a> {
    /// Canonical lowercase wire kind name (`"svg"`, `"font_glyph"`, `"png"`, `"text"`).
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Svg { .. } => "svg",
            Self::FontGlyph { .. } => "font_glyph",
            Self::Png { .. } => "png",
            Self::Text { .. } => "text",
        }
    }

    /// Deep-clone this symbol source into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> SymbolSource<'b> {
        match self {
            Self::Svg { markup, monochrome } => SymbolSource::Svg {
                markup: String::from_str_in(markup.as_str(), alloc),
                monochrome: *monochrome,
            },
            Self::FontGlyph { font_name, font_data_b64, codepoint, tint } => {
                SymbolSource::FontGlyph {
                    font_name: font_name.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
                    font_data_b64: font_data_b64
                        .as_ref()
                        .map(|s| String::from_str_in(s.as_str(), alloc)),
                    codepoint: *codepoint,
                    tint: *tint,
                }
            }
            Self::Png { data_b64 } => {
                SymbolSource::Png { data_b64: String::from_str_in(data_b64.as_str(), alloc) }
            }
            Self::Text { text } => {
                SymbolSource::Text { text: String::from_str_in(text.as_str(), alloc) }
            }
        }
    }
}

/// Registered symbol mapped to a Private Use Area codepoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolRecord<'a> {
    pub name: AString<'a>,
    pub codepoint: u32,
    pub width_cells: u8,
    pub source: SymbolSource<'a>,
    pub revision: u64,
}

impl<'a> SymbolRecord<'a> {
    /// Deep-clone this symbol record into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> SymbolRecord<'b> {
        SymbolRecord {
            name: String::from_str_in(self.name.as_str(), alloc),
            codepoint: self.codepoint,
            width_cells: self.width_cells,
            source: self.source.clone_in(alloc),
            revision: self.revision,
        }
    }

    /// Return the assigned PUA codepoint as a `char`.
    pub fn codepoint_char(&self) -> Option<char> {
        char::from_u32(self.codepoint)
    }
}

/// Metadata bound to a specific [`EntityId`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityMeta<'a> {
    pub id: EntityId<'a>,
    pub symbol: Option<AString<'a>>,
    pub display_name: Option<AString<'a>>,
    pub color: Option<[u8; 4]>,
    pub badge: Option<AString<'a>>,
    pub hint_path: Option<AString<'a>>,
    pub attrs: AVec<'a, (AString<'a>, AString<'a>)>,
    pub revision: u64,
}

impl<'a> EntityMeta<'a> {
    /// Create an empty metadata record for `id` with no optional fields set.
    pub fn new(id: EntityId<'a>, alloc: Alloc<'a>) -> Self {
        Self {
            id,
            symbol: None,
            display_name: None,
            color: None,
            badge: None,
            hint_path: None,
            attrs: Vec::new_in(alloc),
            revision: 0,
        }
    }

    /// Deep-clone this entity metadata into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> EntityMeta<'b> {
        let mut attrs = Vec::with_capacity_in(self.attrs.len(), alloc);
        for (k, v) in self.attrs.iter() {
            attrs.push((
                String::from_str_in(k.as_str(), alloc),
                String::from_str_in(v.as_str(), alloc),
            ));
        }
        EntityMeta {
            id: self.id.clone_in(alloc),
            symbol: self.symbol.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            display_name: self
                .display_name
                .as_ref()
                .map(|s| String::from_str_in(s.as_str(), alloc)),
            color: self.color,
            badge: self.badge.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            hint_path: self.hint_path.as_ref().map(|s| String::from_str_in(s.as_str(), alloc)),
            attrs,
            revision: self.revision,
        }
    }
}

/// Host server metadata injected into sans-IO RPC calls (used by `"info"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerMeta<'a> {
    pub pid: u32,
    pub socket_path: &'a str,
}

/// Result of handling an inbound OSC 7770 payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OscOutcome<'a> {
    pub mutated: bool,
    pub reply_osc: Option<AString<'a>>,
    pub response_json: AString<'a>,
}

/// Typed protocol and registry errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError<'a> {
    TooLarge { len: usize, max: usize },
    InvalidJson(AString<'a>),
    InvalidField { field: &'static str, reason: AString<'a> },
    UnknownOp(AString<'a>),
    CapacityExceeded { kind: &'static str, max: usize },
    CodepointOutOfRange { codepoint: u32 },
    CodepointInUse { codepoint: u32, owner: AString<'a> },
    NotFound { kind: &'static str, key: AString<'a> },
    InvalidBase64 { field: &'static str, reason: AString<'a> },
}

impl<'a> ApiError<'a> {
    /// Machine-readable snake_case error code returned in JSON error responses.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "too_large",
            Self::InvalidJson(_) => "invalid_json",
            Self::InvalidField { .. } => "invalid_field",
            Self::UnknownOp(_) => "unknown_op",
            Self::CapacityExceeded { .. } => "capacity_exceeded",
            Self::CodepointOutOfRange { .. } => "codepoint_out_of_range",
            Self::CodepointInUse { .. } => "codepoint_in_use",
            Self::NotFound { .. } => "not_found",
            Self::InvalidBase64 { .. } => "invalid_base64",
        }
    }

    /// Format a human-readable error message into `alloc`.
    pub fn message_in<'b>(&self, alloc: Alloc<'b>) -> AString<'b> {
        let mut out = String::new_in(alloc);
        blaze_own::format_into!(&mut out, "{self}");
        out
    }
}

impl fmt::Display for ApiError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => {
                write!(f, "message is {len} bytes, exceeding maximum of {max} bytes")
            }
            Self::InvalidJson(msg) => write!(f, "invalid JSON: {msg}"),
            Self::InvalidField { field, reason } => write!(f, "invalid field `{field}`: {reason}"),
            Self::UnknownOp(op) => write!(f, "unknown operation `{op}`"),
            Self::CapacityExceeded { kind, max } => {
                write!(f, "{kind} capacity exceeded (maximum {max})")
            }
            Self::CodepointOutOfRange { codepoint } => {
                write!(
                    f,
                    "codepoint U+{codepoint:04X} is outside PUA range U+{PUA_START:04X}..=U+{PUA_END:04X}"
                )
            }
            Self::CodepointInUse { codepoint, owner } => {
                write!(f, "codepoint U+{codepoint:04X} is already assigned to symbol `{owner}`")
            }
            Self::NotFound { kind, key } => write!(f, "{kind} `{key}` not found"),
            Self::InvalidBase64 { field, reason } => {
                write!(f, "invalid base64 in field `{field}`: {reason}")
            }
        }
    }
}

impl core::error::Error for ApiError<'_> {}

/// Parse `"#rrggbb"` (alpha = `0xFF`) or `"#rrggbbaa"` into RGBA bytes.
pub fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let hex = s.strip_prefix('#')?;
    let bytes = hex.as_bytes();
    match bytes.len() {
        6 => {
            let r = parse_hex_byte(bytes[0], bytes[1])?;
            let g = parse_hex_byte(bytes[2], bytes[3])?;
            let b = parse_hex_byte(bytes[4], bytes[5])?;
            Some([r, g, b, 0xFF])
        }
        8 => {
            let r = parse_hex_byte(bytes[0], bytes[1])?;
            let g = parse_hex_byte(bytes[2], bytes[3])?;
            let b = parse_hex_byte(bytes[4], bytes[5])?;
            let a = parse_hex_byte(bytes[6], bytes[7])?;
            Some([r, g, b, a])
        }
        _ => None,
    }
}

/// Format RGBA bytes as `"#rrggbb"` (when alpha is `0xFF`) or `"#rrggbbaa"` (otherwise).
pub fn format_hex_color<'a>(rgba: [u8; 4], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::with_capacity_in(9, alloc);
    let [r, g, b, a] = rgba;
    if a == 0xFF {
        blaze_own::format_into!(&mut out, "#{r:02x}{g:02x}{b:02x}");
    } else {
        blaze_own::format_into!(&mut out, "#{r:02x}{g:02x}{b:02x}{a:02x}");
    }
    out
}

/// Wrap a JSON payload in an OSC 7770 ST-terminated escape sequence: `\x1b]7770;<json>\x1b\\`.
pub fn encode_osc_7770_json<'a>(json: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::with_capacity_in(json.len().saturating_add(12), alloc);
    blaze_own::format_into!(&mut out, "\x1b]{OSC_CODE};{json}\x1b\\");
    out
}

pub(crate) fn parse_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn parse_hex_byte(hi: u8, lo: u8) -> Option<u8> {
    Some((parse_hex_nibble(hi)? << 4) | parse_hex_nibble(lo)?)
}

pub(crate) fn parse_strict_u64(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

pub(crate) fn parse_codepoint_str(s: &str) -> Option<u32> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed
        .strip_prefix("U+")
        .or_else(|| trimmed.strip_prefix("u+"))
        .or_else(|| trimmed.strip_prefix("0x"))
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return u32::from_str_radix(hex, 16).ok();
    }
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

pub(crate) fn is_valid_symbol_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SYMBOL_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

pub(crate) fn format_u64_pair<'b>(a: u64, b: u64, buf: &'b mut [u8; 48]) -> &'b str {
    let mut pos = write_u64_decimal(a, buf, 0);
    buf[pos] = b':';
    pos += 1;
    pos = write_u64_decimal(b, buf, pos);
    match core::str::from_utf8(&buf[..pos]) {
        Ok(s) => s,
        Err(_) => unreachable!("ASCII digits and ':' are always valid UTF-8"),
    }
}

fn write_u64_decimal(mut n: u64, buf: &mut [u8; 48], start: usize) -> usize {
    if n == 0 {
        buf[start] = b'0';
        return start + 1;
    }
    let mut tmp = [0u8; 20];
    let mut len = 0usize;
    while n > 0 {
        let digit = u8::try_from(n % 10).unwrap_or(0);
        tmp[len] = b'0' + digit;
        n /= 10;
        len += 1;
    }
    let mut pos = start;
    while len > 0 {
        len -= 1;
        buf[pos] = tmp[len];
        pos += 1;
    }
    pos
}
