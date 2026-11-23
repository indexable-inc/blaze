//! Pure `#![no_std]` sans-IO DNS wire message encoder and decoder (RFC 1035, RFC 3596, RFC 2782).
//!
//! - Encodes standard DNS queries ([`encode_query_into`]) and arbitrary DNS messages
//!   ([`encode_message_into`]) with strict validation of label lengths (`1..=63`), wire name
//!   lengths (`<= 255`), header fields, TXT chunk lengths, and RDATA lengths.
//! - Decodes untrusted DNS packets ([`decode_message_in`]) under explicit [`DnsLimits`], handling
//!   RFC 1035 §4.1.4 name compression pointers (`0b11xxxxxx`) with strict cycle detection via both
//!   a jump counter and a visited-offset guard, and validating every `RDLENGTH` against packet
//!   bounds.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero recursion, zero panicking
//!   slice indexing.

use core::alloc::Allocator;
use core::fmt;
use core::net::{Ipv4Addr, Ipv6Addr};
use core::str;

use blaze_own::{AllocFailure, String, Vec};

const HEADER_LEN: usize = 12;
const MAX_LABEL_LEN: usize = 63;
const MAX_WIRE_NAME_LEN: usize = 255;
const POINTER_TAG_MASK: u8 = 0xC0;
const POINTER_TAG: u8 = 0xC0;
const SOA_FIXED_TRAILER_LEN: usize = 20;

/// DNS resource record / query type (`TYPE` / `QTYPE`, RFC 1035 / RFC 3596 / RFC 2782 / RFC 6891).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordType {
    /// IPv4 host address (`1`).
    A,
    /// Authoritative name server (`2`).
    Ns,
    /// Canonical name alias (`5`).
    Cname,
    /// Start of a zone of authority (`6`).
    Soa,
    /// Domain name pointer (`12`).
    Ptr,
    /// Mail exchange (`15`).
    Mx,
    /// Text strings (`16`).
    Txt,
    /// IPv6 host address (`28`).
    Aaaa,
    /// Service location (`33`).
    Srv,
    /// EDNS0 pseudo-record (`41`).
    Opt,
    /// Any other 16-bit DNS type code.
    Other(u16),
}

impl RecordType {
    /// Decode a wire 16-bit type code into a [`RecordType`].
    #[inline]
    #[must_use]
    pub const fn from_u16(code: u16) -> Self {
        match code {
            1 => RecordType::A,
            2 => RecordType::Ns,
            5 => RecordType::Cname,
            6 => RecordType::Soa,
            12 => RecordType::Ptr,
            15 => RecordType::Mx,
            16 => RecordType::Txt,
            28 => RecordType::Aaaa,
            33 => RecordType::Srv,
            41 => RecordType::Opt,
            other => RecordType::Other(other),
        }
    }

    /// Encode this [`RecordType`] to its wire 16-bit code.
    #[inline]
    #[must_use]
    pub const fn to_u16(self) -> u16 {
        match self {
            RecordType::A => 1,
            RecordType::Ns => 2,
            RecordType::Cname => 5,
            RecordType::Soa => 6,
            RecordType::Ptr => 12,
            RecordType::Mx => 15,
            RecordType::Txt => 16,
            RecordType::Aaaa => 28,
            RecordType::Srv => 33,
            RecordType::Opt => 41,
            RecordType::Other(code) => code,
        }
    }
}

impl fmt::Display for RecordType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordType::A => f.write_str("A"),
            RecordType::Ns => f.write_str("NS"),
            RecordType::Cname => f.write_str("CNAME"),
            RecordType::Soa => f.write_str("SOA"),
            RecordType::Ptr => f.write_str("PTR"),
            RecordType::Mx => f.write_str("MX"),
            RecordType::Txt => f.write_str("TXT"),
            RecordType::Aaaa => f.write_str("AAAA"),
            RecordType::Srv => f.write_str("SRV"),
            RecordType::Opt => f.write_str("OPT"),
            RecordType::Other(code) => write!(f, "TYPE{code}"),
        }
    }
}

/// DNS resource record / query class (`CLASS` / `QCLASS`, RFC 1035).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordClass {
    /// Internet (`1`).
    In,
    /// CSNET (`2`).
    Cs,
    /// CHAOS (`3`).
    Ch,
    /// Hesiod (`4`).
    Hs,
    /// Wildcard any class (`255`).
    Any,
    /// Any other 16-bit DNS class code.
    Other(u16),
}

impl RecordClass {
    /// Decode a wire 16-bit class code into a [`RecordClass`].
    #[inline]
    #[must_use]
    pub const fn from_u16(code: u16) -> Self {
        match code {
            1 => RecordClass::In,
            2 => RecordClass::Cs,
            3 => RecordClass::Ch,
            4 => RecordClass::Hs,
            255 => RecordClass::Any,
            other => RecordClass::Other(other),
        }
    }

    /// Encode this [`RecordClass`] to its wire 16-bit code.
    #[inline]
    #[must_use]
    pub const fn to_u16(self) -> u16 {
        match self {
            RecordClass::In => 1,
            RecordClass::Cs => 2,
            RecordClass::Ch => 3,
            RecordClass::Hs => 4,
            RecordClass::Any => 255,
            RecordClass::Other(code) => code,
        }
    }
}

impl fmt::Display for RecordClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordClass::In => f.write_str("IN"),
            RecordClass::Cs => f.write_str("CS"),
            RecordClass::Ch => f.write_str("CH"),
            RecordClass::Hs => f.write_str("HS"),
            RecordClass::Any => f.write_str("ANY"),
            RecordClass::Other(code) => write!(f, "CLASS{code}"),
        }
    }
}

/// DNS header 4-bit response code (`RCODE`, RFC 1035 §4.1.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResponseCode {
    /// No error condition (`0`).
    NoError,
    /// Format error (`1`).
    FormErr,
    /// Server failure (`2`).
    ServFail,
    /// Non-existent domain (`3`).
    NxDomain,
    /// Not implemented (`4`).
    NotImp,
    /// Query refused (`5`).
    Refused,
    /// Any other 4-bit response code (`6..=15`).
    Other(u8),
}

impl ResponseCode {
    /// Decode a 4-bit wire RCODE into a [`ResponseCode`].
    #[inline]
    #[must_use]
    pub const fn from_u8(code: u8) -> Self {
        match code {
            0 => ResponseCode::NoError,
            1 => ResponseCode::FormErr,
            2 => ResponseCode::ServFail,
            3 => ResponseCode::NxDomain,
            4 => ResponseCode::NotImp,
            5 => ResponseCode::Refused,
            other => ResponseCode::Other(other),
        }
    }

    /// Encode this [`ResponseCode`] to its wire value.
    #[inline]
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            ResponseCode::NoError => 0,
            ResponseCode::FormErr => 1,
            ResponseCode::ServFail => 2,
            ResponseCode::NxDomain => 3,
            ResponseCode::NotImp => 4,
            ResponseCode::Refused => 5,
            ResponseCode::Other(code) => code,
        }
    }
}

impl fmt::Display for ResponseCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ResponseCode::NoError => f.write_str("NOERROR"),
            ResponseCode::FormErr => f.write_str("FORMERR"),
            ResponseCode::ServFail => f.write_str("SERVFAIL"),
            ResponseCode::NxDomain => f.write_str("NXDOMAIN"),
            ResponseCode::NotImp => f.write_str("NOTIMP"),
            ResponseCode::Refused => f.write_str("REFUSED"),
            ResponseCode::Other(code) => write!(f, "RCODE{code}"),
        }
    }
}

/// DNS 12-byte message header flags and transaction ID (RFC 1035 §4.1.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub id: u16,
    pub is_response: bool,
    pub opcode: u8,
    pub authoritative: bool,
    pub truncated: bool,
    pub recursion_desired: bool,
    pub recursion_available: bool,
    pub rcode: ResponseCode,
}

/// A single entry in the DNS question section (RFC 1035 §4.1.2).
pub struct Question<A: Allocator> {
    pub name: String<A>,
    pub qtype: RecordType,
    pub qclass: RecordClass,
}

impl<A: Allocator> fmt::Debug for Question<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Question")
            .field("name", &self.name.as_str())
            .field("qtype", &self.qtype)
            .field("qclass", &self.qclass)
            .finish()
    }
}

impl<A: Allocator> PartialEq for Question<A> {
    fn eq(&self, other: &Self) -> bool {
        self.name.as_str() == other.name.as_str()
            && self.qtype == other.qtype
            && self.qclass == other.qclass
    }
}

impl<A: Allocator> Eq for Question<A> {}

/// Typed DNS resource record payload (`RDATA`).
pub enum RData<A: Allocator> {
    /// IPv4 address (`A`).
    A(Ipv4Addr),
    /// IPv6 address (`AAAA`).
    Aaaa(Ipv6Addr),
    /// Canonical domain name (`CNAME`).
    Cname(String<A>),
    /// Authoritative name server (`NS`).
    Ns(String<A>),
    /// Domain name pointer (`PTR`).
    Ptr(String<A>),
    /// Mail exchange (`MX`).
    Mx { preference: u16, exchange: String<A> },
    /// Character-string chunks (`TXT`).
    Txt(Vec<Vec<u8, A>, A>),
    /// Service location (`SRV`, RFC 2782).
    Srv { priority: u16, weight: u16, port: u16, target: String<A> },
    /// Start of authority (`SOA`, RFC 1035 §3.3.13).
    Soa {
        mname: String<A>,
        rname: String<A>,
        serial: u32,
        refresh: u32,
        retry: u32,
        expire: u32,
        minimum: u32,
    },
    /// Raw opaque RDATA for `OPT` or unrecognized record types.
    Unknown { rtype: u16, raw: Vec<u8, A> },
}

struct TxtChunksDebug<'a, A: Allocator>(&'a [Vec<u8, A>]);

impl<A: Allocator> fmt::Debug for TxtChunksDebug<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for chunk in self.0 {
            list.entry(&chunk.as_slice());
        }
        list.finish()
    }
}

impl<A: Allocator> fmt::Debug for RData<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RData::A(ip) => f.debug_tuple("A").field(ip).finish(),
            RData::Aaaa(ip) => f.debug_tuple("Aaaa").field(ip).finish(),
            RData::Cname(name) => f.debug_tuple("Cname").field(&name.as_str()).finish(),
            RData::Ns(name) => f.debug_tuple("Ns").field(&name.as_str()).finish(),
            RData::Ptr(name) => f.debug_tuple("Ptr").field(&name.as_str()).finish(),
            RData::Mx { preference, exchange } => f
                .debug_struct("Mx")
                .field("preference", preference)
                .field("exchange", &exchange.as_str())
                .finish(),
            RData::Txt(chunks) => {
                f.debug_tuple("Txt").field(&TxtChunksDebug(chunks.as_slice())).finish()
            }
            RData::Srv { priority, weight, port, target } => f
                .debug_struct("Srv")
                .field("priority", priority)
                .field("weight", weight)
                .field("port", port)
                .field("target", &target.as_str())
                .finish(),
            RData::Soa { mname, rname, serial, refresh, retry, expire, minimum } => f
                .debug_struct("Soa")
                .field("mname", &mname.as_str())
                .field("rname", &rname.as_str())
                .field("serial", serial)
                .field("refresh", refresh)
                .field("retry", retry)
                .field("expire", expire)
                .field("minimum", minimum)
                .finish(),
            RData::Unknown { rtype, raw } => f
                .debug_struct("Unknown")
                .field("rtype", rtype)
                .field("raw", &raw.as_slice())
                .finish(),
        }
    }
}

impl<A: Allocator> PartialEq for RData<A> {
    fn eq(&self, other: &Self) -> bool {
        match self {
            RData::A(a) => {
                let RData::A(b) = other else {
                    return false;
                };
                a == b
            }
            RData::Aaaa(a) => {
                let RData::Aaaa(b) = other else {
                    return false;
                };
                a == b
            }
            RData::Cname(a) => {
                let RData::Cname(b) = other else {
                    return false;
                };
                a.as_str() == b.as_str()
            }
            RData::Ns(a) => {
                let RData::Ns(b) = other else {
                    return false;
                };
                a.as_str() == b.as_str()
            }
            RData::Ptr(a) => {
                let RData::Ptr(b) = other else {
                    return false;
                };
                a.as_str() == b.as_str()
            }
            RData::Mx { preference: pa, exchange: ea } => {
                let RData::Mx { preference: pb, exchange: eb } = other else {
                    return false;
                };
                pa == pb && ea.as_str() == eb.as_str()
            }
            RData::Txt(a) => {
                let RData::Txt(b) = other else {
                    return false;
                };
                a.len() == b.len()
                    && a.iter().zip(b.iter()).all(|(x, y)| x.as_slice() == y.as_slice())
            }
            RData::Srv { priority: pa, weight: wa, port: poa, target: ta } => {
                let RData::Srv { priority: pb, weight: wb, port: pob, target: tb } = other else {
                    return false;
                };
                pa == pb && wa == wb && poa == pob && ta.as_str() == tb.as_str()
            }
            RData::Soa {
                mname: ma,
                rname: ra,
                serial: sa,
                refresh: rfa,
                retry: rta,
                expire: ea,
                minimum: mna,
            } => {
                let RData::Soa {
                    mname: mb,
                    rname: rb,
                    serial: sb,
                    refresh: rfb,
                    retry: rtb,
                    expire: eb,
                    minimum: mnb,
                } = other
                else {
                    return false;
                };
                ma.as_str() == mb.as_str()
                    && ra.as_str() == rb.as_str()
                    && sa == sb
                    && rfa == rfb
                    && rta == rtb
                    && ea == eb
                    && mna == mnb
            }
            RData::Unknown { rtype: ta, raw: ra } => {
                let RData::Unknown { rtype: tb, raw: rb } = other else {
                    return false;
                };
                ta == tb && ra.as_slice() == rb.as_slice()
            }
        }
    }
}

impl<A: Allocator> Eq for RData<A> {}

/// A single DNS resource record (RFC 1035 §4.1.3).
pub struct Record<A: Allocator> {
    pub name: String<A>,
    pub rtype: RecordType,
    pub rclass: RecordClass,
    pub ttl: u32,
    pub rdata: RData<A>,
}

impl<A: Allocator> fmt::Debug for Record<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("name", &self.name.as_str())
            .field("rtype", &self.rtype)
            .field("rclass", &self.rclass)
            .field("ttl", &self.ttl)
            .field("rdata", &self.rdata)
            .finish()
    }
}

impl<A: Allocator> PartialEq for Record<A> {
    fn eq(&self, other: &Self) -> bool {
        self.name.as_str() == other.name.as_str()
            && self.rtype == other.rtype
            && self.rclass == other.rclass
            && self.ttl == other.ttl
            && self.rdata == other.rdata
    }
}

impl<A: Allocator> Eq for Record<A> {}

/// A complete DNS wire message (header + question, answer, authority, and additional sections).
pub struct Message<A: Allocator> {
    pub header: Header,
    pub questions: Vec<Question<A>, A>,
    pub answers: Vec<Record<A>, A>,
    pub authorities: Vec<Record<A>, A>,
    pub additionals: Vec<Record<A>, A>,
}

impl<A: Allocator> fmt::Debug for Message<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("header", &self.header)
            .field("questions", &self.questions.as_slice())
            .field("answers", &self.answers.as_slice())
            .field("authorities", &self.authorities.as_slice())
            .field("additionals", &self.additionals.as_slice())
            .finish()
    }
}

impl<A: Allocator> PartialEq for Message<A> {
    fn eq(&self, other: &Self) -> bool {
        self.header == other.header
            && self.questions.as_slice() == other.questions.as_slice()
            && self.answers.as_slice() == other.answers.as_slice()
            && self.authorities.as_slice() == other.authorities.as_slice()
            && self.additionals.as_slice() == other.additionals.as_slice()
    }
}

impl<A: Allocator> Eq for Message<A> {}

/// Bounds enforced when decoding untrusted DNS wire packets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DnsLimits {
    /// Maximum packet length in bytes.
    pub most_packet_bytes: usize,
    /// Maximum total questions + resource records across all four sections.
    pub most_records: usize,
    /// Maximum compression pointer jumps permitted while decoding a single domain name.
    pub most_name_jumps: usize,
}

impl DnsLimits {
    /// Default bounds: 65,535 bytes (maximum DNS TCP message), 512 records, 16 pointer jumps.
    pub const USUAL: Self = Self {
        most_packet_bytes: 65_535,
        most_records: 512,
        most_name_jumps: 16,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// Typed failure returned by DNS message encoding or decoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DnsError {
    /// Wire packet exceeded [`DnsLimits::most_packet_bytes`].
    PacketTooLarge { bytes: usize, limit: usize },
    /// Wire packet ended before a header, name, question, or resource record completed.
    Truncated,
    /// Total section count exceeded [`DnsLimits::most_records`] or `u16::MAX` on encode.
    TooManyRecords { count: usize, limit: usize },
    /// Header `opcode` or `rcode` exceeded its 4-bit wire field (`0..=15`).
    InvalidHeaderField,
    /// Domain name contained an empty label, NUL octet, embedded dot, or invalid UTF-8.
    InvalidName,
    /// Domain name label exceeded the RFC 1035 maximum of 63 octets.
    LabelTooLong { len: usize },
    /// Total wire-encoded domain name exceeded the RFC 1035 maximum of 255 octets.
    NameTooLong { len: usize },
    /// Encountered a reserved label length tag (`0b01xxxxxx` or `0b10xxxxxx`).
    UnsupportedLabelType { tag: u8 },
    /// Compression pointer formed a cycle back to an already-visited offset.
    CompressionLoop { offset: usize },
    /// Compression pointer targeted an offset outside the valid packet name region.
    CompressionOutOfBounds { offset: usize },
    /// Name decoding exceeded [`DnsLimits::most_name_jumps`].
    TooManyNameJumps { jumps: usize, limit: usize },
    /// Resource record `RDATA` length or structure did not match its `TYPE`.
    InvalidRData { rtype: RecordType },
    /// A `TXT` record chunk exceeded 255 octets.
    InvalidTxtChunkLen { len: usize },
    /// Encoded `RDATA` length exceeded `u16::MAX` bytes.
    RDataTooLarge { len: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AllocFailure> for DnsError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DnsError::PacketTooLarge { bytes, limit } => {
                write!(f, "bstd::dns: packet size {bytes} bytes exceeds limit {limit}")
            }
            DnsError::Truncated => f.write_str("bstd::dns: truncated wire packet"),
            DnsError::TooManyRecords { count, limit } => {
                write!(f, "bstd::dns: record count {count} exceeds limit {limit}")
            }
            DnsError::InvalidHeaderField => {
                f.write_str("bstd::dns: header opcode or rcode exceeds 4-bit range (0..=15)")
            }
            DnsError::InvalidName => {
                f.write_str("bstd::dns: invalid domain name (empty label, NUL, or non-UTF-8)")
            }
            DnsError::LabelTooLong { len } => {
                write!(f, "bstd::dns: label length {len} exceeds RFC 1035 limit of 63 octets")
            }
            DnsError::NameTooLong { len } => {
                write!(f, "bstd::dns: wire name length {len} exceeds RFC 1035 limit of 255 octets")
            }
            DnsError::UnsupportedLabelType { tag } => {
                write!(f, "bstd::dns: unsupported DNS label tag 0x{tag:02x}")
            }
            DnsError::CompressionLoop { offset } => {
                write!(f, "bstd::dns: compression pointer cycle at packet offset {offset}")
            }
            DnsError::CompressionOutOfBounds { offset } => {
                write!(f, "bstd::dns: compression pointer target {offset} is out of bounds")
            }
            DnsError::TooManyNameJumps { jumps, limit } => {
                write!(f, "bstd::dns: name compression jumps {jumps} exceeds limit {limit}")
            }
            DnsError::InvalidRData { rtype } => {
                write!(f, "bstd::dns: malformed or mismatched RDATA for record type {rtype}")
            }
            DnsError::InvalidTxtChunkLen { len } => {
                write!(f, "bstd::dns: TXT chunk length {len} exceeds 255 octets")
            }
            DnsError::RDataTooLarge { len } => {
                write!(f, "bstd::dns: encoded RDATA length {len} exceeds 65535 bytes")
            }
            DnsError::Alloc(err) => write!(f, "bstd::dns: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for DnsError {}

/// Encode a single-question standard DNS query directly into `out`.
///
/// Leaves `out` unchanged if `name` is invalid or allocation fails.
pub fn encode_query_into<A: Allocator>(
    id: u16,
    name: &str,
    qtype: RecordType,
    rd: bool,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    let wire_name_len = validate_name_wire_len(name)?;
    let start_len = out.len();
    let total_add = HEADER_LEN + wire_name_len + 4;
    out.try_reserve(total_add)?;

    let header = Header {
        id,
        is_response: false,
        opcode: 0,
        authoritative: false,
        truncated: false,
        recursion_desired: rd,
        recursion_available: false,
        rcode: ResponseCode::NoError,
    };

    let result = encode_query_inner(header, name, qtype, out);
    if result.is_err() {
        out.truncate(start_len);
    }
    result
}

fn encode_query_inner<A: Allocator>(
    header: Header,
    name: &str,
    qtype: RecordType,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    encode_header_into(header, 1, 0, 0, 0, out)?;
    encode_name_into(name, out)?;
    out.try_extend_from_slice(&qtype.to_u16().to_be_bytes())?;
    out.try_extend_from_slice(&RecordClass::In.to_u16().to_be_bytes())?;
    Ok(())
}

/// Encode a full DNS [`Message`] in wire format into `out`.
///
/// Validates header fields, label lengths (`1..=63`), wire name lengths (`<= 255`), TXT chunk
/// lengths (`<= 255`), and `RDLENGTH` (`<= 65535`). Leaves `out` unchanged on error.
pub fn encode_message_into<A: Allocator>(
    msg: &Message<A>,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    let qdcount = u16::try_from(msg.questions.len()).map_err(|_| DnsError::TooManyRecords {
        count: msg.questions.len(),
        limit: usize::from(u16::MAX),
    })?;
    let ancount = u16::try_from(msg.answers.len()).map_err(|_| DnsError::TooManyRecords {
        count: msg.answers.len(),
        limit: usize::from(u16::MAX),
    })?;
    let nscount = u16::try_from(msg.authorities.len()).map_err(|_| DnsError::TooManyRecords {
        count: msg.authorities.len(),
        limit: usize::from(u16::MAX),
    })?;
    let arcount = u16::try_from(msg.additionals.len()).map_err(|_| DnsError::TooManyRecords {
        count: msg.additionals.len(),
        limit: usize::from(u16::MAX),
    })?;

    let start_len = out.len();
    let result = encode_message_inner(msg, qdcount, ancount, nscount, arcount, out);
    if result.is_err() {
        out.truncate(start_len);
    }
    result
}

fn encode_message_inner<A: Allocator>(
    msg: &Message<A>,
    qdcount: u16,
    ancount: u16,
    nscount: u16,
    arcount: u16,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    encode_header_into(msg.header, qdcount, ancount, nscount, arcount, out)?;
    for question in msg.questions.as_slice() {
        encode_name_into(question.name.as_str(), out)?;
        out.try_extend_from_slice(&question.qtype.to_u16().to_be_bytes())?;
        out.try_extend_from_slice(&question.qclass.to_u16().to_be_bytes())?;
    }
    for record in msg.answers.as_slice() {
        encode_record_into(record, out)?;
    }
    for record in msg.authorities.as_slice() {
        encode_record_into(record, out)?;
    }
    for record in msg.additionals.as_slice() {
        encode_record_into(record, out)?;
    }
    Ok(())
}

/// Decode a DNS wire packet into a [`Message`] allocated in `alloc`, enforcing `limits`.
pub fn decode_message_in<A: Allocator + Copy>(
    packet: &[u8],
    limits: DnsLimits,
    alloc: A,
) -> Result<Message<A>, DnsError> {
    decode_message_in_planted(packet, limits, alloc, false)
}

fn decode_message_in_planted<A: Allocator + Copy>(
    packet: &[u8],
    limits: DnsLimits,
    alloc: A,
    plant_skip_loop_check: bool,
) -> Result<Message<A>, DnsError> {
    if packet.len() > limits.most_packet_bytes {
        return Err(DnsError::PacketTooLarge {
            bytes: packet.len(),
            limit: limits.most_packet_bytes,
        });
    }
    if packet.len() < HEADER_LEN {
        return Err(DnsError::Truncated);
    }

    let id = read_u16(packet, 0)?;
    let b2 = read_u8(packet, 2)?;
    let b3 = read_u8(packet, 3)?;
    let qdcount = usize::from(read_u16(packet, 4)?);
    let ancount = usize::from(read_u16(packet, 6)?);
    let nscount = usize::from(read_u16(packet, 8)?);
    let arcount = usize::from(read_u16(packet, 10)?);

    let total_records = qdcount
        .saturating_add(ancount)
        .saturating_add(nscount)
        .saturating_add(arcount);
    if total_records > limits.most_records {
        return Err(DnsError::TooManyRecords {
            count: total_records,
            limit: limits.most_records,
        });
    }

    let header = Header {
        id,
        is_response: (b2 & 0x80) != 0,
        opcode: (b2 >> 3) & 0x0F,
        authoritative: (b2 & 0x04) != 0,
        truncated: (b2 & 0x02) != 0,
        recursion_desired: (b2 & 0x01) != 0,
        recursion_available: (b3 & 0x80) != 0,
        rcode: ResponseCode::from_u8(b3 & 0x0F),
    };

    let mut pos = HEADER_LEN;

    let rem_q = packet.len().saturating_sub(pos);
    let mut questions = Vec::try_with_capacity_in(qdcount.min(rem_q / 5), alloc)?;
    for _ in 0..qdcount {
        let (name, next_pos) =
            decode_name_in_planted(packet, pos, limits, alloc, plant_skip_loop_check)?;
        let qtype = RecordType::from_u16(read_u16(packet, next_pos)?);
        let qclass = RecordClass::from_u16(read_u16(packet, next_pos + 2)?);
        pos = next_pos + 4;
        if let Err(refused) = questions.try_push(Question { name, qtype, qclass }) {
            return Err(DnsError::Alloc(refused.why));
        }
    }

    let answers = decode_records_section(
        packet,
        &mut pos,
        ancount,
        limits,
        alloc,
        plant_skip_loop_check,
    )?;
    let authorities = decode_records_section(
        packet,
        &mut pos,
        nscount,
        limits,
        alloc,
        plant_skip_loop_check,
    )?;
    let additionals = decode_records_section(
        packet,
        &mut pos,
        arcount,
        limits,
        alloc,
        plant_skip_loop_check,
    )?;

    Ok(Message { header, questions, answers, authorities, additionals })
}

fn decode_records_section<A: Allocator + Copy>(
    packet: &[u8],
    pos: &mut usize,
    count: usize,
    limits: DnsLimits,
    alloc: A,
    plant_skip_loop_check: bool,
) -> Result<Vec<Record<A>, A>, DnsError> {
    let rem = packet.len().saturating_sub(*pos);
    let mut records = Vec::try_with_capacity_in(count.min(rem / 11), alloc)?;
    for _ in 0..count {
        let (name, after_name) =
            decode_name_in_planted(packet, *pos, limits, alloc, plant_skip_loop_check)?;
        let rtype = RecordType::from_u16(read_u16(packet, after_name)?);
        let rclass = RecordClass::from_u16(read_u16(packet, after_name + 2)?);
        let ttl = read_u32(packet, after_name + 4)?;
        let rdlength = usize::from(read_u16(packet, after_name + 8)?);
        let rdata_start = after_name + 10;
        let Some(rdata_end) = rdata_start.checked_add(rdlength) else {
            return Err(DnsError::Truncated);
        };
        if rdata_end > packet.len() {
            return Err(DnsError::Truncated);
        }
        let rdata = decode_rdata(
            packet,
            rtype,
            rdata_start,
            rdata_end,
            limits,
            alloc,
            plant_skip_loop_check,
        )?;
        *pos = rdata_end;
        if let Err(refused) = records.try_push(Record { name, rtype, rclass, ttl, rdata }) {
            return Err(DnsError::Alloc(refused.why));
        }
    }
    Ok(records)
}

fn decode_rdata<A: Allocator + Copy>(
    packet: &[u8],
    rtype: RecordType,
    rdata_start: usize,
    rdata_end: usize,
    limits: DnsLimits,
    alloc: A,
    plant_skip_loop_check: bool,
) -> Result<RData<A>, DnsError> {
    let rdlen = rdata_end - rdata_start;
    match rtype {
        RecordType::A => {
            if rdlen != 4 {
                return Err(DnsError::InvalidRData { rtype });
            }
            let b0 = read_u8(packet, rdata_start)?;
            let b1 = read_u8(packet, rdata_start + 1)?;
            let b2 = read_u8(packet, rdata_start + 2)?;
            let b3 = read_u8(packet, rdata_start + 3)?;
            Ok(RData::A(Ipv4Addr::new(b0, b1, b2, b3)))
        }
        RecordType::Aaaa => {
            if rdlen != 16 {
                return Err(DnsError::InvalidRData { rtype });
            }
            let Some(slice) = packet.get(rdata_start..rdata_end) else {
                return Err(DnsError::Truncated);
            };
            let Ok(octets) = <[u8; 16]>::try_from(slice) else {
                return Err(DnsError::InvalidRData { rtype });
            };
            Ok(RData::Aaaa(Ipv6Addr::from(octets)))
        }
        RecordType::Cname => {
            let (name, end) =
                decode_name_in_planted(packet, rdata_start, limits, alloc, plant_skip_loop_check)?;
            if end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            Ok(RData::Cname(name))
        }
        RecordType::Ns => {
            let (name, end) =
                decode_name_in_planted(packet, rdata_start, limits, alloc, plant_skip_loop_check)?;
            if end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            Ok(RData::Ns(name))
        }
        RecordType::Ptr => {
            let (name, end) =
                decode_name_in_planted(packet, rdata_start, limits, alloc, plant_skip_loop_check)?;
            if end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            Ok(RData::Ptr(name))
        }
        RecordType::Mx => {
            if rdlen < 3 {
                return Err(DnsError::InvalidRData { rtype });
            }
            let preference = read_u16(packet, rdata_start)?;
            let (exchange, end) = decode_name_in_planted(
                packet,
                rdata_start + 2,
                limits,
                alloc,
                plant_skip_loop_check,
            )?;
            if end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            Ok(RData::Mx { preference, exchange })
        }
        RecordType::Txt => {
            let mut chunks = Vec::new_in(alloc);
            let mut cur = rdata_start;
            while cur < rdata_end {
                let len = usize::from(read_u8(packet, cur)?);
                let data_start = cur + 1;
                let Some(data_end) = data_start.checked_add(len) else {
                    return Err(DnsError::InvalidRData { rtype });
                };
                if data_end > rdata_end {
                    return Err(DnsError::InvalidRData { rtype });
                }
                let Some(slice) = packet.get(data_start..data_end) else {
                    return Err(DnsError::Truncated);
                };
                let mut chunk = Vec::try_with_capacity_in(len, alloc)?;
                chunk.try_extend_from_slice(slice)?;
                if let Err(refused) = chunks.try_push(chunk) {
                    return Err(DnsError::Alloc(refused.why));
                }
                cur = data_end;
            }
            Ok(RData::Txt(chunks))
        }
        RecordType::Srv => {
            if rdlen < 7 {
                return Err(DnsError::InvalidRData { rtype });
            }
            let priority = read_u16(packet, rdata_start)?;
            let weight = read_u16(packet, rdata_start + 2)?;
            let port = read_u16(packet, rdata_start + 4)?;
            let (target, end) = decode_name_in_planted(
                packet,
                rdata_start + 6,
                limits,
                alloc,
                plant_skip_loop_check,
            )?;
            if end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            Ok(RData::Srv { priority, weight, port, target })
        }
        RecordType::Soa => {
            let (mname, after_mname) =
                decode_name_in_planted(packet, rdata_start, limits, alloc, plant_skip_loop_check)?;
            if after_mname >= rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            let (rname, after_rname) =
                decode_name_in_planted(packet, after_mname, limits, alloc, plant_skip_loop_check)?;
            let Some(expected_end) = after_rname.checked_add(SOA_FIXED_TRAILER_LEN) else {
                return Err(DnsError::InvalidRData { rtype });
            };
            if expected_end != rdata_end {
                return Err(DnsError::InvalidRData { rtype });
            }
            let serial = read_u32(packet, after_rname)?;
            let refresh = read_u32(packet, after_rname + 4)?;
            let retry = read_u32(packet, after_rname + 8)?;
            let expire = read_u32(packet, after_rname + 12)?;
            let minimum = read_u32(packet, after_rname + 16)?;
            Ok(RData::Soa { mname, rname, serial, refresh, retry, expire, minimum })
        }
        RecordType::Opt | RecordType::Other(_) => {
            let Some(slice) = packet.get(rdata_start..rdata_end) else {
                return Err(DnsError::Truncated);
            };
            let mut raw = Vec::try_with_capacity_in(rdlen, alloc)?;
            raw.try_extend_from_slice(slice)?;
            Ok(RData::Unknown { rtype: rtype.to_u16(), raw })
        }
    }
}

fn encode_header_into<A: Allocator>(
    header: Header,
    qdcount: u16,
    ancount: u16,
    nscount: u16,
    arcount: u16,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    let rcode_raw = header.rcode.to_u8();
    if header.opcode > 0x0F || rcode_raw > 0x0F {
        return Err(DnsError::InvalidHeaderField);
    }

    let b2 = (u8::from(header.is_response) << 7)
        | (header.opcode << 3)
        | (u8::from(header.authoritative) << 2)
        | (u8::from(header.truncated) << 1)
        | u8::from(header.recursion_desired);
    let b3 = (u8::from(header.recursion_available) << 7) | rcode_raw;

    let [id0, id1] = header.id.to_be_bytes();
    let [qd0, qd1] = qdcount.to_be_bytes();
    let [an0, an1] = ancount.to_be_bytes();
    let [ns0, ns1] = nscount.to_be_bytes();
    let [ar0, ar1] = arcount.to_be_bytes();

    out.try_extend_from_slice(&[id0, id1, b2, b3, qd0, qd1, an0, an1, ns0, ns1, ar0, ar1])?;
    Ok(())
}

fn validate_name_wire_len(name: &str) -> Result<usize, DnsError> {
    if name.is_empty() || name == "." {
        return Ok(1);
    }
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    if trimmed.is_empty() {
        return Err(DnsError::InvalidName);
    }
    let mut wire_len = 1usize; // trailing 0x00 root octet
    for label in trimmed.split('.') {
        if label.is_empty() || label.as_bytes().contains(&0) {
            return Err(DnsError::InvalidName);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(DnsError::LabelTooLong { len: label.len() });
        }
        wire_len += 1 + label.len();
        if wire_len > MAX_WIRE_NAME_LEN {
            return Err(DnsError::NameTooLong { len: wire_len });
        }
    }
    Ok(wire_len)
}

fn encode_name_into<A: Allocator>(name: &str, out: &mut Vec<u8, A>) -> Result<(), DnsError> {
    let wire_len = validate_name_wire_len(name)?;
    out.try_reserve(wire_len)?;
    if name.is_empty() || name == "." {
        if let Err(refused) = out.try_push(0) {
            return Err(DnsError::Alloc(refused.why));
        }
        return Ok(());
    }
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    for label in trimmed.split('.') {
        let Ok(len_byte) = u8::try_from(label.len()) else {
            return Err(DnsError::LabelTooLong { len: label.len() });
        };
        if let Err(refused) = out.try_push(len_byte) {
            return Err(DnsError::Alloc(refused.why));
        }
        out.try_extend_from_slice(label.as_bytes())?;
    }
    if let Err(refused) = out.try_push(0) {
        return Err(DnsError::Alloc(refused.why));
    }
    Ok(())
}

fn encode_record_into<A: Allocator>(
    record: &Record<A>,
    out: &mut Vec<u8, A>,
) -> Result<(), DnsError> {
    validate_rdata_matches_type(record.rtype, &record.rdata)?;
    encode_name_into(record.name.as_str(), out)?;
    out.try_extend_from_slice(&record.rtype.to_u16().to_be_bytes())?;
    out.try_extend_from_slice(&record.rclass.to_u16().to_be_bytes())?;
    out.try_extend_from_slice(&record.ttl.to_be_bytes())?;

    let rdlen_pos = out.len();
    out.try_extend_from_slice(&[0, 0])?;
    let rdata_start = out.len();

    match &record.rdata {
        RData::A(ip) => {
            out.try_extend_from_slice(&ip.octets())?;
        }
        RData::Aaaa(ip) => {
            out.try_extend_from_slice(&ip.octets())?;
        }
        RData::Cname(name) | RData::Ns(name) | RData::Ptr(name) => {
            encode_name_into(name.as_str(), out)?;
        }
        RData::Mx { preference, exchange } => {
            out.try_extend_from_slice(&preference.to_be_bytes())?;
            encode_name_into(exchange.as_str(), out)?;
        }
        RData::Txt(chunks) => {
            for chunk in chunks.as_slice() {
                let Ok(len_byte) = u8::try_from(chunk.len()) else {
                    return Err(DnsError::InvalidTxtChunkLen { len: chunk.len() });
                };
                if let Err(refused) = out.try_push(len_byte) {
                    return Err(DnsError::Alloc(refused.why));
                }
                out.try_extend_from_slice(chunk.as_slice())?;
            }
        }
        RData::Srv { priority, weight, port, target } => {
            out.try_extend_from_slice(&priority.to_be_bytes())?;
            out.try_extend_from_slice(&weight.to_be_bytes())?;
            out.try_extend_from_slice(&port.to_be_bytes())?;
            encode_name_into(target.as_str(), out)?;
        }
        RData::Soa { mname, rname, serial, refresh, retry, expire, minimum } => {
            encode_name_into(mname.as_str(), out)?;
            encode_name_into(rname.as_str(), out)?;
            out.try_extend_from_slice(&serial.to_be_bytes())?;
            out.try_extend_from_slice(&refresh.to_be_bytes())?;
            out.try_extend_from_slice(&retry.to_be_bytes())?;
            out.try_extend_from_slice(&expire.to_be_bytes())?;
            out.try_extend_from_slice(&minimum.to_be_bytes())?;
        }
        RData::Unknown { raw, .. } => {
            out.try_extend_from_slice(raw.as_slice())?;
        }
    }

    let rdata_len = out.len() - rdata_start;
    let Ok(rdlen_u16) = u16::try_from(rdata_len) else {
        return Err(DnsError::RDataTooLarge { len: rdata_len });
    };
    let rdlen_bytes = rdlen_u16.to_be_bytes();
    if let Some(slot) = out.get_mut(rdlen_pos..rdlen_pos + 2) {
        slot.copy_from_slice(&rdlen_bytes);
    }
    Ok(())
}

fn validate_rdata_matches_type<A: Allocator>(
    rtype: RecordType,
    rdata: &RData<A>,
) -> Result<(), DnsError> {
    let matches = match rdata {
        RData::A(_) => rtype == RecordType::A,
        RData::Aaaa(_) => rtype == RecordType::Aaaa,
        RData::Cname(_) => rtype == RecordType::Cname,
        RData::Ns(_) => rtype == RecordType::Ns,
        RData::Ptr(_) => rtype == RecordType::Ptr,
        RData::Mx { .. } => rtype == RecordType::Mx,
        RData::Txt(_) => rtype == RecordType::Txt,
        RData::Srv { .. } => rtype == RecordType::Srv,
        RData::Soa { .. } => rtype == RecordType::Soa,
        RData::Unknown { rtype: raw_code, .. } => match rtype {
            RecordType::Opt | RecordType::Other(_) => rtype.to_u16() == *raw_code,
            RecordType::A
            | RecordType::Ns
            | RecordType::Cname
            | RecordType::Soa
            | RecordType::Ptr
            | RecordType::Mx
            | RecordType::Txt
            | RecordType::Aaaa
            | RecordType::Srv => false,
        },
    };
    if matches { Ok(()) } else { Err(DnsError::InvalidRData { rtype }) }
}

fn decode_name_in_planted<A: Allocator + Copy>(
    packet: &[u8],
    start: usize,
    limits: DnsLimits,
    alloc: A,
    plant_skip_loop_check: bool,
) -> Result<(String<A>, usize), DnsError> {
    let mut out = String::new_in(alloc);
    let mut visited_segments: Vec<(usize, usize), A> = Vec::new_in(alloc);
    let mut seg_start = start;
    let mut cur = start;
    let mut end_after_name: Option<usize> = None;
    let mut jumps = 0usize;
    let mut wire_len = 0usize;

    loop {
        if !plant_skip_loop_check && offset_in_segments(cur, visited_segments.as_slice()) {
            return Err(DnsError::CompressionLoop { offset: cur });
        }

        let len_byte = read_u8(packet, cur)?;
        let tag = len_byte & POINTER_TAG_MASK;

        if tag == POINTER_TAG {
            let lo = read_u8(packet, cur + 1)?;
            let hi = len_byte & 0x3F;
            let target = usize::from(u16::from_be_bytes([hi, lo]));

            if target < HEADER_LEN || target >= packet.len() {
                return Err(DnsError::CompressionOutOfBounds { offset: target });
            }

            if end_after_name.is_none() {
                end_after_name = Some(cur + 2);
            }

            let cur_seg_end = cur + 1;
            if !plant_skip_loop_check {
                if (seg_start..=cur_seg_end).contains(&target)
                    || offset_in_segments(target, visited_segments.as_slice())
                {
                    return Err(DnsError::CompressionLoop { offset: target });
                }
                if let Err(refused) = visited_segments.try_push((seg_start, cur_seg_end)) {
                    return Err(DnsError::Alloc(refused.why));
                }
            }

            jumps += 1;
            if jumps > limits.most_name_jumps {
                return Err(DnsError::TooManyNameJumps { jumps, limit: limits.most_name_jumps });
            }

            seg_start = target;
            cur = target;
            continue;
        }

        if tag != 0 {
            return Err(DnsError::UnsupportedLabelType { tag });
        }

        if len_byte == 0 {
            wire_len += 1;
            if wire_len > MAX_WIRE_NAME_LEN {
                return Err(DnsError::NameTooLong { len: wire_len });
            }
            let next_pos = end_after_name.unwrap_or(cur + 1);
            return Ok((out, next_pos));
        }

        let label_len = usize::from(len_byte);
        wire_len += 1 + label_len;
        if wire_len > MAX_WIRE_NAME_LEN - 1 {
            return Err(DnsError::NameTooLong { len: wire_len + 1 });
        }

        let label_start = cur + 1;
        let Some(label_end) = label_start.checked_add(label_len) else {
            return Err(DnsError::Truncated);
        };
        let Some(label_bytes) = packet.get(label_start..label_end) else {
            return Err(DnsError::Truncated);
        };
        if label_bytes.contains(&b'.') || label_bytes.contains(&0) {
            return Err(DnsError::InvalidName);
        }
        let Ok(label_str) = str::from_utf8(label_bytes) else {
            return Err(DnsError::InvalidName);
        };

        if !out.is_empty() {
            out.try_push('.')?;
        }
        out.try_push_str(label_str)?;
        cur = label_end;
    }
}

#[inline]
fn offset_in_segments(offset: usize, segments: &[(usize, usize)]) -> bool {
    for &(start, end) in segments {
        if (start..=end).contains(&offset) {
            return true;
        }
    }
    false
}

#[inline]
fn read_u8(packet: &[u8], offset: usize) -> Result<u8, DnsError> {
    packet.get(offset).copied().ok_or(DnsError::Truncated)
}

#[inline]
fn read_u16(packet: &[u8], offset: usize) -> Result<u16, DnsError> {
    let b0 = read_u8(packet, offset)?;
    let b1 = read_u8(packet, offset + 1)?;
    Ok(u16::from_be_bytes([b0, b1]))
}

#[inline]
fn read_u32(packet: &[u8], offset: usize) -> Result<u32, DnsError> {
    let b0 = read_u8(packet, offset)?;
    let b1 = read_u8(packet, offset + 1)?;
    let b2 = read_u8(packet, offset + 2)?;
    let b3 = read_u8(packet, offset + 3)?;
    Ok(u32::from_be_bytes([b0, b1, b2, b3]))
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;
    use core::net::{Ipv4Addr, Ipv6Addr};

    use blaze_own::testing::heap;
    use blaze_own::{String as OwnString, Vec as OwnVec, format_in};
    use blaze_sim::{Broke, Config, Tape, check, control};

    use super::{
        DnsError, DnsLimits, Header, Message, Question, RData, Record, RecordClass, RecordType,
        ResponseCode, decode_message_in, decode_message_in_planted, encode_message_into,
        encode_query_into,
    };

    #[test]
    fn encode_and_decode_standard_query() {
        let h = heap();
        let alloc = h.alloc();
        let mut wire = OwnVec::new_in(alloc);
        assert_eq!(encode_query_into(0x1234, "example.com", RecordType::A, true, &mut wire), Ok(()));

        let decoded = match decode_message_in(wire.as_slice(), DnsLimits::USUAL, alloc) {
            Ok(m) => m,
            Err(e) => panic!("decode failed: {e}"),
        };
        assert_eq!(decoded.header.id, 0x1234);
        assert!(!decoded.header.is_response);
        assert!(decoded.header.recursion_desired);
        assert_eq!(decoded.header.rcode, ResponseCode::NoError);
        assert_eq!(decoded.questions.len(), 1);
        let q = match decoded.questions.first() {
            Some(q) => q,
            None => panic!("missing question"),
        };
        assert_eq!(q.name.as_str(), "example.com");
        assert_eq!(q.qtype, RecordType::A);
        assert_eq!(q.qclass, RecordClass::In);
        assert!(decoded.answers.is_empty());
        assert!(decoded.authorities.is_empty());
        assert!(decoded.additionals.is_empty());
    }

    #[test]
    fn encode_and_decode_all_record_types() {
        let h = heap();
        let alloc = h.alloc();

        let mut txt_chunks = OwnVec::new_in(alloc);
        let mut c1 = OwnVec::new_in(alloc);
        c1.extend_from_slice(b"v=spf1 include:_spf.example.com ~all");
        let mut c2 = OwnVec::new_in(alloc);
        c2.extend_from_slice(b"second-chunk");
        txt_chunks.push(c1);
        txt_chunks.push(c2);

        let mut opt_raw = OwnVec::new_in(alloc);
        opt_raw.extend_from_slice(&[0x00, 0x0A, 0x00, 0x02, 0xCA, 0xFE]);

        let mut questions = OwnVec::new_in(alloc);
        questions.push(Question {
            name: OwnString::from_str_in("www.example.com", alloc),
            qtype: RecordType::A,
            qclass: RecordClass::In,
        });

        let mut answers = OwnVec::new_in(alloc);
        answers.push(Record {
            name: OwnString::from_str_in("www.example.com", alloc),
            rtype: RecordType::Cname,
            rclass: RecordClass::In,
            ttl: 300,
            rdata: RData::Cname(OwnString::from_str_in("edge.example.com", alloc)),
        });
        answers.push(Record {
            name: OwnString::from_str_in("edge.example.com", alloc),
            rtype: RecordType::A,
            rclass: RecordClass::In,
            ttl: 60,
            rdata: RData::A(Ipv4Addr::new(93, 184, 216, 34)),
        });
        answers.push(Record {
            name: OwnString::from_str_in("edge.example.com", alloc),
            rtype: RecordType::Aaaa,
            rclass: RecordClass::In,
            ttl: 60,
            rdata: RData::Aaaa(Ipv6Addr::new(0x2606, 0x2800, 0x0220, 0x0001, 0x0248, 0x1893, 0x25c8, 0x1946)),
        });
        answers.push(Record {
            name: OwnString::from_str_in("example.com", alloc),
            rtype: RecordType::Mx,
            rclass: RecordClass::In,
            ttl: 3600,
            rdata: RData::Mx {
                preference: 10,
                exchange: OwnString::from_str_in("mail.example.com", alloc),
            },
        });
        answers.push(Record {
            name: OwnString::from_str_in("example.com", alloc),
            rtype: RecordType::Txt,
            rclass: RecordClass::In,
            ttl: 600,
            rdata: RData::Txt(txt_chunks),
        });
        answers.push(Record {
            name: OwnString::from_str_in("_https._tcp.example.com", alloc),
            rtype: RecordType::Srv,
            rclass: RecordClass::In,
            ttl: 120,
            rdata: RData::Srv {
                priority: 5,
                weight: 100,
                port: 443,
                target: OwnString::from_str_in("srv1.example.com", alloc),
            },
        });
        answers.push(Record {
            name: OwnString::from_str_in("34.216.184.93.in-addr.arpa", alloc),
            rtype: RecordType::Ptr,
            rclass: RecordClass::In,
            ttl: 900,
            rdata: RData::Ptr(OwnString::from_str_in("edge.example.com", alloc)),
        });

        let mut authorities = OwnVec::new_in(alloc);
        authorities.push(Record {
            name: OwnString::from_str_in("example.com", alloc),
            rtype: RecordType::Ns,
            rclass: RecordClass::In,
            ttl: 86_400,
            rdata: RData::Ns(OwnString::from_str_in("ns1.example.com", alloc)),
        });
        authorities.push(Record {
            name: OwnString::from_str_in("example.com", alloc),
            rtype: RecordType::Soa,
            rclass: RecordClass::In,
            ttl: 86_400,
            rdata: RData::Soa {
                mname: OwnString::from_str_in("ns1.example.com", alloc),
                rname: OwnString::from_str_in("hostmaster.example.com", alloc),
                serial: 2026_1123_01,
                refresh: 7200,
                retry: 900,
                expire: 1_209_600,
                minimum: 300,
            },
        });

        let mut additionals = OwnVec::new_in(alloc);
        additionals.push(Record {
            name: OwnString::new_in(alloc),
            rtype: RecordType::Opt,
            rclass: RecordClass::Other(4096),
            ttl: 0,
            rdata: RData::Unknown { rtype: 41, raw: opt_raw },
        });

        let msg = Message {
            header: Header {
                id: 0xABCD,
                is_response: true,
                opcode: 0,
                authoritative: true,
                truncated: false,
                recursion_desired: true,
                recursion_available: true,
                rcode: ResponseCode::NoError,
            },
            questions,
            answers,
            authorities,
            additionals,
        };

        let mut wire = OwnVec::new_in(alloc);
        assert_eq!(encode_message_into(&msg, &mut wire), Ok(()));

        let decoded = match decode_message_in(wire.as_slice(), DnsLimits::USUAL, alloc) {
            Ok(m) => m,
            Err(e) => panic!("decode failed: {e}"),
        };
        assert_eq!(decoded, msg);
    }

    #[test]
    fn decode_compressed_pointer_response() {
        let h = heap();
        let alloc = h.alloc();

        // Hand-crafted DNS response with RFC 1035 §4.1.4 compression pointers:
        // Header: id=0x0042, QR=1 RD=1 RA=1, QD=1, AN=2, NS=0, AR=0
        // Offset 12: QNAME = 3,"www", 7,"example", 3,"com", 0  (17 bytes -> ends at 29)
        // Offset 29: QTYPE=A (1), QCLASS=IN (1)                (4 bytes -> ends at 33)
        // Offset 33: Answer 1 NAME = 0xC0 0x0C ("www.example.com")
        //            TYPE=CNAME (5), CLASS=IN (1), TTL=60, RDLEN=6
        //            RDATA = 3,"cdn", 0xC0 0x10 (points to offset 16 = "example.com")
        // Offset 51: Answer 2 NAME = 0xC0 0x2D (points to offset 45 = "cdn.example.com")
        //            TYPE=A (1), CLASS=IN (1), TTL=60, RDLEN=4
        //            RDATA = 1.2.3.4
        let packet: &[u8] = &[
            // Header (0..12)
            0x00, 0x42, 0x81, 0x80, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
            // Question (12..33): "www.example.com"
            3, b'w', b'w', b'w', 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm',
            0, 0x00, 0x01, 0x00, 0x01,
            // Answer 1 (33..51): pointer to 12 ("www.example.com") -> CNAME "cdn" + ptr to 16
            0xC0, 0x0C, 0x00, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x06,
            3, b'c', b'd', b'n', 0xC0, 0x10,
            // Answer 2 (51..67): pointer to 45 ("cdn.example.com") -> A 1.2.3.4
            0xC0, 0x2D, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3C, 0x00, 0x04,
            1, 2, 3, 4,
        ];

        let decoded = match decode_message_in(packet, DnsLimits::USUAL, alloc) {
            Ok(m) => m,
            Err(e) => panic!("compressed decode failed: {e}"),
        };
        assert_eq!(decoded.header.id, 0x0042);
        assert_eq!(decoded.answers.len(), 2);

        let a0 = match decoded.answers.first() {
            Some(r) => r,
            None => panic!("missing answer 0"),
        };
        assert_eq!(a0.name.as_str(), "www.example.com");
        assert_eq!(a0.rdata, RData::Cname(OwnString::from_str_in("cdn.example.com", alloc)));

        let a1 = match decoded.answers.get(1) {
            Some(r) => r,
            None => panic!("missing answer 1"),
        };
        assert_eq!(a1.name.as_str(), "cdn.example.com");
        assert_eq!(a1.rdata, RData::A(Ipv4Addr::new(1, 2, 3, 4)));
    }

    #[test]
    fn refuses_cyclic_compression_pointer_and_truncated_packets() {
        let h = heap();
        let alloc = h.alloc();

        // Self-loop compression pointer at offset 12: 0xC0 0x0C.
        let self_loop: &[u8] = &[
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01,
        ];
        assert_eq!(
            decode_message_in(self_loop, DnsLimits::USUAL, alloc),
            Err(DnsError::CompressionLoop { offset: 12 })
        );

        // Two-node mutual cycle: offset 12 -> offset 14 -> offset 12.
        let two_cycle: &[u8] = &[
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xC0, 0x0E, 0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01,
        ];
        assert_eq!(
            decode_message_in(two_cycle, DnsLimits::USUAL, alloc),
            Err(DnsError::CompressionLoop { offset: 12 })
        );

        // Label followed by backward pointer into itself: offset 12 has 1,'a',0xC0,0x0C.
        let label_cycle: &[u8] = &[
            0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            1, b'a', 0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01,
        ];
        assert_eq!(
            decode_message_in(label_cycle, DnsLimits::USUAL, alloc),
            Err(DnsError::CompressionLoop { offset: 12 })
        );

        // Truncated header.
        assert_eq!(decode_message_in(&[0u8; 11], DnsLimits::USUAL, alloc), Err(DnsError::Truncated));

        // Truncated RDLENGTH beyond packet bounds.
        let truncated_rdata: &[u8] = &[
            0x00, 0x01, 0x81, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x10, 0x00, 0x04, 1, 2,
        ];
        assert_eq!(
            decode_message_in(truncated_rdata, DnsLimits::USUAL, alloc),
            Err(DnsError::Truncated)
        );

        // Label > 63 bytes refused on encode.
        let mut wire = OwnVec::new_in(alloc);
        let mut long_label = OwnString::new_in(alloc);
        for _ in 0..64 {
            long_label.push('a');
        }
        long_label.push_str(".com");
        assert_eq!(
            encode_query_into(1, long_label.as_str(), RecordType::A, true, &mut wire),
            Err(DnsError::LabelTooLong { len: 64 })
        );
        assert!(wire.is_empty());

        // Wire name > 255 bytes refused on encode (4 * 64 + 1 = 257).
        let mut long_name = OwnString::new_in(alloc);
        for idx in 0..4 {
            if idx > 0 {
                long_name.push('.');
            }
            for _ in 0..63 {
                long_name.push('a');
            }
        }
        assert_eq!(
            encode_query_into(1, long_name.as_str(), RecordType::A, true, &mut wire),
            Err(DnsError::NameTooLong { len: 257 })
        );
        assert!(wire.is_empty());
    }

    fn draw_name<A: Allocator + Copy>(tape: &Tape, alloc: A) -> OwnString<A> {
        const LABELS: &[&str] = &[
            "a",
            "www",
            "api",
            "example",
            "com",
            "org",
            "edge-01",
            "_srv",
            "_tcp",
            "mail",
            "ns1",
        ];
        let count = tape.int_in(0usize..=4, "label_count");
        let mut s = OwnString::new_in(alloc);
        for i in 0..count {
            if i > 0 {
                s.push('.');
            }
            let lbl = LABELS.get(tape.index(LABELS.len(), "label")).copied().unwrap_or("a");
            s.push_str(lbl);
        }
        s
    }

    fn draw_record<A: Allocator + Copy>(tape: &Tape, alloc: A) -> Record<A> {
        let name = draw_name(tape, alloc);
        let rclass = match tape.int_in(0u8..=4, "rclass") {
            0 => RecordClass::In,
            1 => RecordClass::Ch,
            2 => RecordClass::Hs,
            3 => RecordClass::Any,
            _ => RecordClass::Other(tape.int_in(5u16..=200, "rclass_other")),
        };
        let ttl = tape.next_u32("ttl");
        let (rtype, rdata) = match tape.int_in(0u8..=10, "rdata_kind") {
            0 => {
                let mut oct = [0u8; 4];
                tape.fill(&mut oct, "v4");
                let [a, b, c, d] = oct;
                (RecordType::A, RData::A(Ipv4Addr::new(a, b, c, d)))
            }
            1 => {
                let mut oct = [0u8; 16];
                tape.fill(&mut oct, "v6");
                (RecordType::Aaaa, RData::Aaaa(Ipv6Addr::from(oct)))
            }
            2 => (RecordType::Cname, RData::Cname(draw_name(tape, alloc))),
            3 => (RecordType::Ns, RData::Ns(draw_name(tape, alloc))),
            4 => (RecordType::Ptr, RData::Ptr(draw_name(tape, alloc))),
            5 => (
                RecordType::Mx,
                RData::Mx {
                    preference: tape.int_in(0u16..=65_535, "mx_pref"),
                    exchange: draw_name(tape, alloc),
                },
            ),
            6 => {
                let chunk_count = tape.int_in(0usize..=3, "txt_chunks");
                let mut chunks = OwnVec::new_in(alloc);
                for _ in 0..chunk_count {
                    let len = tape.int_in(0usize..=16, "txt_len");
                    let mut buf = OwnVec::with_capacity_in(len, alloc);
                    for _ in 0..len {
                        buf.push(tape.int_in(0u8..=255, "txt_byte"));
                    }
                    chunks.push(buf);
                }
                (RecordType::Txt, RData::Txt(chunks))
            }
            7 => (
                RecordType::Srv,
                RData::Srv {
                    priority: tape.int_in(0u16..=100, "srv_prio"),
                    weight: tape.int_in(0u16..=100, "srv_weight"),
                    port: tape.int_in(0u16..=65_535, "srv_port"),
                    target: draw_name(tape, alloc),
                },
            ),
            8 => (
                RecordType::Soa,
                RData::Soa {
                    mname: draw_name(tape, alloc),
                    rname: draw_name(tape, alloc),
                    serial: tape.next_u32("soa_serial"),
                    refresh: tape.next_u32("soa_refresh"),
                    retry: tape.next_u32("soa_retry"),
                    expire: tape.next_u32("soa_expire"),
                    minimum: tape.next_u32("soa_min"),
                },
            ),
            9 => {
                let len = tape.int_in(0usize..=12, "opt_len");
                let mut raw = OwnVec::with_capacity_in(len, alloc);
                for _ in 0..len {
                    raw.push(tape.int_in(0u8..=255, "opt_byte"));
                }
                (RecordType::Opt, RData::Unknown { rtype: 41, raw })
            }
            _ => {
                let code = tape.int_in(100u16..=200, "other_rtype");
                let len = tape.int_in(0usize..=12, "other_len");
                let mut raw = OwnVec::with_capacity_in(len, alloc);
                for _ in 0..len {
                    raw.push(tape.int_in(0u8..=255, "other_byte"));
                }
                (RecordType::Other(code), RData::Unknown { rtype: code, raw })
            }
        };
        Record { name, rtype, rclass, ttl, rdata }
    }

    fn run_pointer_loop_oracle<A: Allocator + Copy>(
        tape: &Tape,
        plant_skip_loop_check: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let id = tape.int_in(0u16..=65_535, "id");
        let kind = tape.int_in(0u8..=2, "cycle_kind");
        let [id0, id1] = id.to_be_bytes();
        let mut pkt = OwnVec::new_in(alloc);
        // Header: QDCOUNT = 1
        pkt.extend_from_slice(&[id0, id1, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        match kind {
            0 => {
                // Direct self-loop at offset 12: 0xC0 0x0C
                pkt.extend_from_slice(&[0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
            }
            1 => {
                // Mutual 2-cycle: 12 -> 14 -> 12
                pkt.extend_from_slice(&[0xC0, 0x0E, 0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
            }
            _ => {
                // Label then back-pointer to offset 12
                pkt.extend_from_slice(&[1, b'x', 0xC0, 0x0C, 0x00, 0x01, 0x00, 0x01]);
            }
        }

        match decode_message_in_planted(
            pkt.as_slice(),
            DnsLimits::USUAL,
            alloc,
            plant_skip_loop_check,
        ) {
            Err(DnsError::CompressionLoop { .. }) => Ok(()),
            Ok(_) => Err(Broke::Oracle("cyclic pointer accepted as valid message".into())),
            Err(other) => Err(Broke::Oracle(
                format_in!(alloc, "cyclic pointer not refused as CompressionLoop: {other}")
                    .as_str()
                    .into(),
            )),
        }
    }

    /// The invariant: Every valid DNS message round-trips losslessly through wire encoding and decoding, and hostile or cyclic-pointer packets are refused without panicking.
    #[test]
    fn property_dns_round_trip_and_fuzz() {
        let h = heap();
        let alloc = h.alloc();
        check(Config::cases(256), |tape| -> Result<(), Broke> {
            let header = Header {
                id: tape.int_in(0u16..=65_535, "id"),
                is_response: tape.coin("qr"),
                opcode: tape.int_in(0u8..=15, "opcode"),
                authoritative: tape.coin("aa"),
                truncated: tape.coin("tc"),
                recursion_desired: tape.coin("rd"),
                recursion_available: tape.coin("ra"),
                rcode: ResponseCode::from_u8(tape.int_in(0u8..=15, "rcode")),
            };

            let q_count = tape.int_in(0usize..=3, "q_count");
            let mut questions = OwnVec::new_in(alloc);
            for _ in 0..q_count {
                questions.push(Question {
                    name: draw_name(tape, alloc),
                    qtype: RecordType::from_u16(tape.int_in(1u16..=50, "qtype")),
                    qclass: RecordClass::from_u16(if tape.coin("qclass_in") { 1 } else { 255 }),
                });
            }

            let an_count = tape.int_in(0usize..=4, "an_count");
            let mut answers = OwnVec::new_in(alloc);
            for _ in 0..an_count {
                answers.push(draw_record(tape, alloc));
            }

            let ns_count = tape.int_in(0usize..=2, "ns_count");
            let mut authorities = OwnVec::new_in(alloc);
            for _ in 0..ns_count {
                authorities.push(draw_record(tape, alloc));
            }

            let ar_count = tape.int_in(0usize..=2, "ar_count");
            let mut additionals = OwnVec::new_in(alloc);
            for _ in 0..ar_count {
                additionals.push(draw_record(tape, alloc));
            }

            let original = Message { header, questions, answers, authorities, additionals };
            let mut wire = OwnVec::new_in(alloc);
            encode_message_into(&original, &mut wire).map_err(|e| {
                Broke::Oracle(format_in!(alloc, "encode_message_into failed: {e}").as_str().into())
            })?;
            let decoded =
                decode_message_in(wire.as_slice(), DnsLimits::USUAL, alloc).map_err(|e| {
                    Broke::Oracle(
                        format_in!(alloc, "decode_message_in failed: {e}").as_str().into(),
                    )
                })?;
            if decoded != original {
                return Err(Broke::Oracle("DNS message round-trip mismatch".into()));
            }

            // Hostile fuzz packet must never panic and must cleanly return Ok or Err.
            let fuzz_len = tape.int_in(0usize..=96, "fuzz_len");
            let mut fuzz = OwnVec::with_capacity_in(fuzz_len, alloc);
            for _ in 0..fuzz_len {
                fuzz.push(tape.int_in(0u8..=255, "fuzz_byte"));
            }
            if let Ok(msg) = decode_message_in(fuzz.as_slice(), DnsLimits::USUAL, alloc)
                && msg.header.opcode > 15
            {
                return Err(Broke::Oracle("decoded opcode exceeded 4-bit range".into()));
            }

            // Cyclic compression pointers must always be refused as CompressionLoop.
            run_pointer_loop_oracle(tape, false, alloc)?;
            Ok(())
        });
    }

    #[test]
    fn the_oracle_catches_pointer_loop_miss() {
        let h = heap();
        let alloc = h.alloc();
        control(
            Config::cases(64),
            false,
            true,
            "cyclic pointer not refused as CompressionLoop",
            |tape, plant| run_pointer_loop_oracle(tape, plant, alloc),
        );
    }
}
