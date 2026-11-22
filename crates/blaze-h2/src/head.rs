//! Fields and heads: what a request or a response carries in its HEADERS, and the checks RFC 9113
//! section 8 puts on them, the same for what we send (a typed refusal before a byte goes out) and
//! what we receive (a malformed message: a stream error of type PROTOCOL_ERROR, section 8.1.1).
//!
//! Section 8.2.1: "A field name MUST NOT contain characters in the ranges 0x00-0x20, 0x41-0x5a,
//! or 0x7f-0xff (all ranges inclusive). ... With the exception of pseudo-header fields ..., which
//! have a name that starts with a single colon, field names MUST NOT include a colon (ASCII COLON,
//! 0x3a). A field value MUST NOT contain the zero value (ASCII NUL, 0x00), line feed (ASCII LF,
//! 0x0a), or carriage return (ASCII CR, 0x0d) at any position. A field value MUST NOT start or
//! end with an ASCII whitespace character (ASCII SP or HTAB, 0x20 or 0x09)."
//!
//! Section 8.2.2: "An endpoint MUST NOT generate an HTTP/2 message containing connection-specific
//! header fields ... (e.g., Keep-Alive, Proxy-Connection, Transfer-Encoding, and Upgrade) ...
//! The only exception to this is the TE header field, which MAY be present in an HTTP/2 request;
//! when it is, it MUST NOT contain any value other than "trailers"." `connection` itself is the
//! first such field.
//!
//! Section 8.3: "Pseudo-header fields are only valid in the context in which they are defined.
//! ... All pseudo-header fields MUST appear in a field block before all regular field lines. ...
//! The same pseudo-header field name MUST NOT appear more than once in a field block. ... Trailers
//! MUST NOT include pseudo-header fields." Section 8.3.1: a request has exactly one `:method`,
//! `:scheme` and `:path` (not empty for `http` and `https`), unless it is a CONNECT (section 8.5:
//! `:method` and `:authority` only). Section 8.3.2: a response has `:status` alone. `:protocol`
//! (RFC 8441) is refused by name: we never send SETTINGS_ENABLE_CONNECT_PROTOCOL.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, AllocFailure};

/// One field as the caller gives it or reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field<'f> {
    pub name: &'f [u8],
    pub value: &'f [u8],
    /// Never indexed by HPACK (RFC 7541 section 7.1.3): a secret such as `authorization`.
    pub sensitive: bool,
}

impl<'f> Field<'f> {
    /// A field HPACK may index.
    pub const fn new(name: &'f [u8], value: &'f [u8]) -> Field<'f> {
        Field { name, value, sensitive: false }
    }

    /// A field HPACK never indexes.
    pub const fn sensitive(name: &'f [u8], value: &'f [u8]) -> Field<'f> {
        Field { name, value, sensitive: true }
    }
}

#[derive(Clone, Copy, Debug)]
struct Span {
    name_end: u32,
    value_end: u32,
    sensitive: bool,
}

/// A decoded field list, its bytes in the caller's allocator.
pub struct Fields<'a> {
    bytes: AVec<'a, u8>,
    spans: AVec<'a, Span>,
}

impl<'a> Fields<'a> {
    /// An empty list.
    pub fn new_in(alloc: Alloc<'a>) -> Fields<'a> {
        Fields { bytes: AVec::new_in(alloc), spans: AVec::new_in(alloc) }
    }

    /// Appends a field.
    pub fn push(&mut self, name: &[u8], value: &[u8], sensitive: bool) -> Result<(), AllocFailure> {
        self.bytes.try_reserve(name.len() + value.len())?;
        self.spans.try_reserve(1)?;
        self.bytes.try_extend_from_slice(name)?;
        let name_end = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        self.bytes.try_extend_from_slice(value)?;
        let value_end = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        if let Err(refused) = self.spans.try_push(Span { name_end, value_end, sensitive }) {
            return Err(refused.why);
        }
        Ok(())
    }

    /// Empties the list, keeping its buffers.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
    }

    /// How many fields.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether it holds no field.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The field at `i`, in the order it came.
    pub fn get(&self, i: usize) -> Option<Field<'_>> {
        let span = *self.spans.as_slice().get(i)?;
        let start = match i {
            0 => 0,
            _ => self.spans.as_slice()[i - 1].value_end,
        };
        let bytes = self.bytes.as_slice();
        let start = usize::try_from(start).ok()?;
        let name_end = usize::try_from(span.name_end).ok()?;
        let value_end = usize::try_from(span.value_end).ok()?;
        Some(Field { name: &bytes[start..name_end], value: &bytes[name_end..value_end], sensitive: span.sensitive })
    }

    /// The fields in the order they came.
    pub fn iter(&self) -> impl Iterator<Item = Field<'_>> + '_ {
        (0..self.len()).filter_map(move |i| self.get(i))
    }

    /// The first value of the field `name` (lower-case), if any.
    pub fn value(&self, name: &[u8]) -> Option<&[u8]> {
        self.iter().find(|f| f.name == name).map(|f| f.value)
    }

    /// A response's `:status`, once checked.
    pub fn status(&self) -> Option<u16> {
        parse_status(self.value(b":status")?)
    }
}

/// A pseudo-header field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pseudo {
    Method,
    Scheme,
    Authority,
    Path,
    Status,
}

/// What is wrong with a head (RFC 9113 sections 8.1, 8.2, 8.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeadWhy {
    /// An empty field name.
    NameEmpty,
    /// An upper-case letter in a field name (section 8.2.1: names are lower-case).
    NameUpperCase,
    /// A byte no field name may hold (control, space, colon past the first byte, 0x7f and up).
    NameByte { byte: u8 },
    /// NUL, CR or LF in a field value.
    ValueByte { byte: u8 },
    /// A field value that starts or ends with SP or HTAB.
    ValueEdgeSpace,
    /// A connection-specific field (`connection`, `proxy-connection`, `keep-alive`,
    /// `transfer-encoding`, `upgrade`), refused by name (section 8.2.2).
    ConnectionSpecific,
    /// `te` with a value other than `trailers`.
    TeNotTrailers,
    /// A pseudo-header this context does not define (`:protocol` among them).
    PseudoUnknown,
    /// A pseudo-header after a regular field.
    PseudoAfterRegular,
    /// The same pseudo-header twice.
    PseudoTwice { which: Pseudo },
    /// A pseudo-header the message needs is missing.
    PseudoMissing { which: Pseudo },
    /// A pseudo-header in trailers.
    PseudoInTrailers,
    /// A CONNECT with `:scheme` or `:path`.
    ConnectWithPath,
    /// An empty `:path` on an `http` or `https` request.
    PathEmpty,
    /// A `:method` that is no token.
    Method,
    /// A `:status` that is not three digits, or 101 (section 8.6: HTTP/2 has no protocol
    /// switch).
    Status,
    /// `:authority` and `host` that differ (section 8.3.1).
    AuthorityHost,
    /// A `content-length` that is no number, or two that differ.
    ContentLength,
    /// A HEADERS after the final response's that does not end the stream (trailers end it,
    /// section 8.1).
    TrailersNotEnd,
    /// An informational response (1xx) that ends the stream (section 8.1).
    InformationalEnd,
}

impl fmt::Display for HeadWhy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HeadWhy::NameEmpty => f.write_str("an empty field name (RFC 9113 section 8.2.1)"),
            HeadWhy::NameUpperCase => f.write_str("an upper-case field name (RFC 9113 section 8.2.1)"),
            HeadWhy::NameByte { byte } => write!(f, "byte {byte:#04x} in a field name (RFC 9113 section 8.2.1)"),
            HeadWhy::ValueByte { byte } => write!(f, "byte {byte:#04x} in a field value (RFC 9113 section 8.2.1)"),
            HeadWhy::ValueEdgeSpace => f.write_str("a field value with space at an end (RFC 9113 section 8.2.1)"),
            HeadWhy::ConnectionSpecific => f.write_str("a connection-specific field (RFC 9113 section 8.2.2)"),
            HeadWhy::TeNotTrailers => f.write_str("`te` other than `trailers` (RFC 9113 section 8.2.2)"),
            HeadWhy::PseudoUnknown => f.write_str("a pseudo-header not defined here (RFC 9113 section 8.3)"),
            HeadWhy::PseudoAfterRegular => f.write_str("a pseudo-header after a regular field (RFC 9113 section 8.3)"),
            HeadWhy::PseudoTwice { which } => write!(f, "{which:?} twice (RFC 9113 section 8.3)"),
            HeadWhy::PseudoMissing { which } => write!(f, "no {which:?} (RFC 9113 section 8.3)"),
            HeadWhy::PseudoInTrailers => f.write_str("a pseudo-header in trailers (RFC 9113 section 8.1)"),
            HeadWhy::ConnectWithPath => f.write_str("a CONNECT with :scheme or :path (RFC 9113 section 8.5)"),
            HeadWhy::PathEmpty => f.write_str("an empty :path (RFC 9113 section 8.3.1)"),
            HeadWhy::Method => f.write_str("a :method that is no token (RFC 9113 section 8.3.1)"),
            HeadWhy::Status => f.write_str("a :status that is not three digits, or 101 (RFC 9113 sections 8.3.2, 8.6)"),
            HeadWhy::AuthorityHost => f.write_str(":authority and host differ (RFC 9113 section 8.3.1)"),
            HeadWhy::ContentLength => f.write_str("a content-length that is no number (RFC 9113 section 8.1.1)"),
            HeadWhy::TrailersNotEnd => f.write_str("trailers that do not end the stream (RFC 9113 section 8.1)"),
            HeadWhy::InformationalEnd => f.write_str("a 1xx response that ends the stream (RFC 9113 section 8.1)"),
        }
    }
}

/// A head refused before it was sent: the field (its index among the caller's fields, or none
/// for a pseudo-header the head's own members give) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HeadFault {
    pub index: Option<u16>,
    pub why: HeadWhy,
}

impl fmt::Display for HeadFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(index) => write!(f, "field {index}: {}", self.why),
            None => write!(f, "{}", self.why),
        }
    }
}

/// A request's head as the caller gives it. The pseudo-headers are members, so their order and
/// number are right by construction; `fields` are the regular fields.
#[derive(Clone, Copy, Debug)]
pub struct RequestHead<'h> {
    pub method: &'h [u8],
    /// None only for CONNECT.
    pub scheme: Option<&'h [u8]>,
    pub authority: Option<&'h [u8]>,
    /// None only for CONNECT.
    pub path: Option<&'h [u8]>,
    pub fields: &'h [Field<'h>],
}

/// A response's head as the caller gives it.
#[derive(Clone, Copy, Debug)]
pub struct ResponseHead<'h> {
    pub status: u16,
    pub fields: &'h [Field<'h>],
}

/// Whether `byte` is a token character (RFC 9110 section 5.6.2).
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The checks of section 8.2.1 on a regular field's name, and section 8.2.2's refusals.
pub fn check_regular(name: &[u8], value: &[u8]) -> Result<(), HeadWhy> {
    if name.is_empty() {
        return Err(HeadWhy::NameEmpty);
    }
    for &byte in name {
        if byte.is_ascii_uppercase() {
            return Err(HeadWhy::NameUpperCase);
        }
        if byte <= 0x20 || byte >= 0x7f || byte == b':' {
            return Err(HeadWhy::NameByte { byte });
        }
    }
    check_value(value)?;
    match name {
        b"connection" | b"proxy-connection" | b"keep-alive" | b"transfer-encoding" | b"upgrade" => {
            Err(HeadWhy::ConnectionSpecific)
        }
        b"te" if value != b"trailers" => Err(HeadWhy::TeNotTrailers),
        _ => Ok(()),
    }
}

/// The checks of section 8.2.1 on a field's value.
pub fn check_value(value: &[u8]) -> Result<(), HeadWhy> {
    if let Some(&byte) = value.iter().find(|&&b| b == 0 || b == b'\r' || b == b'\n') {
        return Err(HeadWhy::ValueByte { byte });
    }
    let edge = |b: Option<&u8>| matches!(b, Some(b' ' | b'\t'));
    if edge(value.first()) || edge(value.last()) {
        return Err(HeadWhy::ValueEdgeSpace);
    }
    Ok(())
}

/// A three-digit status, not 101.
pub fn parse_status(value: &[u8]) -> Option<u16> {
    let [a, b, c] = *value else { return None };
    if !(a.is_ascii_digit() && b.is_ascii_digit() && c.is_ascii_digit()) || a == b'0' {
        return None;
    }
    let status = u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0');
    (status != 101).then_some(status)
}

/// A `content-length` value: decimal digits only.
fn parse_length(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u64 = 0;
    for &d in value {
        n = n.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
    }
    Some(n)
}

/// What a received head says that the stream needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checked {
    /// The `content-length`, when given.
    pub content_length: Option<u64>,
    /// For a response, its status; for a request, 0.
    pub status: u16,
    /// For a request: whether its method is HEAD (its response has no body).
    pub head_method: bool,
}

/// The regular fields' checks and the `content-length`, from field index `from` on.
fn check_regulars(fields: &Fields<'_>, from: usize) -> Result<Option<u64>, HeadWhy> {
    let mut length = None;
    for field in fields.iter().skip(from) {
        if field.name.first() == Some(&b':') {
            return Err(HeadWhy::PseudoAfterRegular);
        }
        check_regular(field.name, field.value)?;
        if field.name == b"content-length" {
            let n = parse_length(field.value).ok_or(HeadWhy::ContentLength)?;
            if length.is_some_and(|before| before != n) {
                return Err(HeadWhy::ContentLength);
            }
            length = Some(n);
        }
    }
    Ok(length)
}

/// Checks a received request's head (section 8.3.1).
pub fn check_request(fields: &Fields<'_>) -> Result<Checked, HeadWhy> {
    let (mut method, mut scheme, mut authority, mut path) = (None, None, None, None);
    let mut pseudo = 0;
    for field in fields.iter() {
        if field.name.first() != Some(&b':') {
            break;
        }
        pseudo += 1;
        check_value(field.value)?;
        let (slot, which) = match field.name {
            b":method" => (&mut method, Pseudo::Method),
            b":scheme" => (&mut scheme, Pseudo::Scheme),
            b":authority" => (&mut authority, Pseudo::Authority),
            b":path" => (&mut path, Pseudo::Path),
            _ => return Err(HeadWhy::PseudoUnknown),
        };
        if slot.replace(field.value).is_some() {
            return Err(HeadWhy::PseudoTwice { which });
        }
    }
    let method = method.ok_or(HeadWhy::PseudoMissing { which: Pseudo::Method })?;
    if method.is_empty() || !method.iter().all(|&b| is_tchar(b)) {
        return Err(HeadWhy::Method);
    }
    if method == b"CONNECT" {
        if scheme.is_some() || path.is_some() {
            return Err(HeadWhy::ConnectWithPath);
        }
        authority.ok_or(HeadWhy::PseudoMissing { which: Pseudo::Authority })?;
    } else {
        scheme.ok_or(HeadWhy::PseudoMissing { which: Pseudo::Scheme })?;
        let path = path.ok_or(HeadWhy::PseudoMissing { which: Pseudo::Path })?;
        if path.is_empty() {
            return Err(HeadWhy::PathEmpty);
        }
    }
    let content_length = check_regulars(fields, pseudo)?;
    if let (Some(authority), Some(host)) = (authority, fields.value(b"host"))
        && authority != host
    {
        return Err(HeadWhy::AuthorityHost);
    }
    Ok(Checked { content_length, status: 0, head_method: method == b"HEAD" })
}

/// Checks a received response's head (section 8.3.2).
pub fn check_response(fields: &Fields<'_>) -> Result<Checked, HeadWhy> {
    let mut status = None;
    let mut pseudo = 0;
    for field in fields.iter() {
        if field.name.first() != Some(&b':') {
            break;
        }
        pseudo += 1;
        if field.name != b":status" {
            return Err(HeadWhy::PseudoUnknown);
        }
        if status.replace(field.value).is_some() {
            return Err(HeadWhy::PseudoTwice { which: Pseudo::Status });
        }
    }
    let status = status.ok_or(HeadWhy::PseudoMissing { which: Pseudo::Status })?;
    let status = parse_status(status).ok_or(HeadWhy::Status)?;
    let content_length = check_regulars(fields, pseudo)?;
    Ok(Checked { content_length, status, head_method: false })
}

/// Checks received trailers (section 8.1: no pseudo-header).
pub fn check_trailers(fields: &Fields<'_>) -> Result<(), HeadWhy> {
    for field in fields.iter() {
        if field.name.first() == Some(&b':') {
            return Err(HeadWhy::PseudoInTrailers);
        }
        check_regular(field.name, field.value)?;
    }
    Ok(())
}

/// Checks a request head we are about to send.
pub fn check_outgoing_request(head: &RequestHead<'_>) -> Result<(), HeadFault> {
    let fault = |why| HeadFault { index: None, why };
    if head.method.is_empty() || !head.method.iter().all(|&b| is_tchar(b)) {
        return Err(fault(HeadWhy::Method));
    }
    if head.method == b"CONNECT" {
        if head.scheme.is_some() || head.path.is_some() {
            return Err(fault(HeadWhy::ConnectWithPath));
        }
        if head.authority.is_none() {
            return Err(fault(HeadWhy::PseudoMissing { which: Pseudo::Authority }));
        }
    } else {
        let scheme = head.scheme.ok_or(fault(HeadWhy::PseudoMissing { which: Pseudo::Scheme }))?;
        check_value(scheme).map_err(fault)?;
        let path = head.path.ok_or(fault(HeadWhy::PseudoMissing { which: Pseudo::Path }))?;
        if path.is_empty() {
            return Err(fault(HeadWhy::PathEmpty));
        }
        check_value(path).map_err(fault)?;
    }
    if let Some(authority) = head.authority {
        check_value(authority).map_err(fault)?;
    }
    check_outgoing_fields(head.fields)?;
    if let Some(authority) = head.authority
        && let Some(host) = head.fields.iter().find(|f| f.name == b"host")
        && host.value != authority
    {
        return Err(fault(HeadWhy::AuthorityHost));
    }
    Ok(())
}

/// Checks a response head we are about to send.
pub fn check_outgoing_response(head: &ResponseHead<'_>) -> Result<(), HeadFault> {
    if !(100..=999).contains(&head.status) || head.status == 101 {
        return Err(HeadFault { index: None, why: HeadWhy::Status });
    }
    check_outgoing_fields(head.fields)
}

/// Checks regular fields we are about to send (also trailers).
pub fn check_outgoing_fields(fields: &[Field<'_>]) -> Result<(), HeadFault> {
    for (i, field) in fields.iter().enumerate() {
        let index = Some(u16::try_from(i).unwrap_or(u16::MAX));
        if field.name.first() == Some(&b':') {
            return Err(HeadFault { index, why: HeadWhy::PseudoUnknown });
        }
        check_regular(field.name, field.value).map_err(|why| HeadFault { index, why })?;
    }
    Ok(())
}

/// The three ASCII digits of `status` (100 to 999).
pub fn status_digits(status: u16) -> [u8; 3] {
    let digit = |n: u16| u8::try_from(n % 10).unwrap_or(0) + b'0';
    [digit(status / 100), digit(status / 10), digit(status)]
}
