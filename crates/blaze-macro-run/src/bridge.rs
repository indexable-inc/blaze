//! The bytes of the pinned bridge's protocol (`library/proc_macro/src/bridge/rpc.rs`), read and
//! written as the client writes and reads them: little-endian; a `usize` is eight bytes on every
//! target; a handle is a nonzero `u32`; a string is its length then its bytes; `Option`'s tag is
//! `Some` 0 and `None` 1, `Result`'s `Ok` 0 and `Err` 1, `Bound`'s `Included` 0, `Excluded` 1 and
//! `Unbounded` 2, each a byte. The tests in method.rs hold these to the pinned sources.
//!
//! The call's and the outcome's own bytes (call.rs, outcome.rs) are written with the same
//! [`Reader`] and [`Writer`]: one cursor for every byte format of this crate.
//!
//! Moved here from blaze-proc-macro wire.rs, now with no `std`, its errors typed to the field
//! ([`Field`]) so a [`Malformed`] crosses the process boundary as bytes (outcome.rs). The old
//! file is deleted when the wasm host takes this server (piece 2).

use core::fmt;
use core::ops::Bound;

use blaze_own::AVec;

/// Which value a [`Malformed`] was in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Field {
    Byte = 0,
    Bool = 1,
    U32 = 2,
    Usize = 3,
    Handle = 4,
    Str = 5,
    OptionTag = 6,
    ResultTag = 7,
    BoundTag = 8,
    List = 9,
    Delimiter = 10,
    LitKind = 11,
    Level = 12,
    IdentKind = 13,
    TokenTree = 14,
    Spacing = 15,
    TokenKind = 16,
    Origin = 17,
    Place = 18,
    Transparency = 19,
    Version = 20,
    OutcomeTag = 21,
    ProcKind = 22,
    Edition = 23,
    Char = 24,
    Method = 25,
    Error = 26,
    Limit = 27,
}

impl Field {
    const ALL: [Field; 28] = [
        Field::Byte,
        Field::Bool,
        Field::U32,
        Field::Usize,
        Field::Handle,
        Field::Str,
        Field::OptionTag,
        Field::ResultTag,
        Field::BoundTag,
        Field::List,
        Field::Delimiter,
        Field::LitKind,
        Field::Level,
        Field::IdentKind,
        Field::TokenTree,
        Field::Spacing,
        Field::TokenKind,
        Field::Origin,
        Field::Place,
        Field::Transparency,
        Field::Version,
        Field::OutcomeTag,
        Field::ProcKind,
        Field::Edition,
        Field::Char,
        Field::Method,
        Field::Error,
        Field::Limit,
    ];

    pub fn byte(self) -> u8 {
        // The declared discriminants are the indexes of `ALL`.
        let mut at = 0u8;
        for field in Field::ALL {
            if field == self {
                return at;
            }
            at = at.saturating_add(1);
        }
        u8::MAX
    }

    pub fn from_byte(byte: u8) -> Option<Field> {
        Field::ALL.get(usize::from(byte)).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Field::Byte => "a byte",
            Field::Bool => "a bool",
            Field::U32 => "a u32",
            Field::Usize => "a usize",
            Field::Handle => "a handle",
            Field::Str => "a string",
            Field::OptionTag => "an Option tag",
            Field::ResultTag => "a Result tag",
            Field::BoundTag => "a Bound tag",
            Field::List => "a list",
            Field::Delimiter => "a Delimiter tag",
            Field::LitKind => "a LitKind tag",
            Field::Level => "a Level tag",
            Field::IdentKind => "an IdentKind tag",
            Field::TokenTree => "a TokenTree tag",
            Field::Spacing => "a spacing",
            Field::TokenKind => "a token kind",
            Field::Origin => "a token origin",
            Field::Place => "a span's place",
            Field::Transparency => "a transparency",
            Field::Version => "the format version",
            Field::OutcomeTag => "the outcome's tag",
            Field::ProcKind => "a proc macro kind",
            Field::Edition => "an edition",
            Field::Char => "a punctuation character",
            Field::Method => "a method tag",
            Field::Error => "an error tag",
            Field::Limit => "a limit tag",
        }
    }
}

/// Bytes no writer of the format writes: a client that is not the pinned bridge's, a process
/// that is not blaze's session, or a bug.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Malformed {
    /// Cut short in the named value.
    Short(Field),
    /// A tag the format never writes there.
    Tag(Field, u8),
    /// The handle 0, which no handle is.
    ZeroHandle,
    /// A string that is not UTF-8.
    NotUtf8,
    /// Bytes after the last value.
    Trailing,
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::Short(field) => write!(f, "bytes cut short in {}", field.name()),
            Malformed::Tag(field, value) => write!(f, "{} {value}, which is never written there", field.name()),
            Malformed::ZeroHandle => f.write_str("the handle 0, which no handle is"),
            Malformed::NotUtf8 => f.write_str("a string that is not UTF-8"),
            Malformed::Trailing => f.write_str("bytes after the last value"),
        }
    }
}

impl Malformed {
    /// Its three bytes, as a stopped outcome carries it (outcome.rs).
    pub(crate) fn bytes(self) -> [u8; 3] {
        match self {
            Malformed::Short(field) => [0, field.byte(), 0],
            Malformed::Tag(field, value) => [1, field.byte(), value],
            Malformed::ZeroHandle => [2, 0, 0],
            Malformed::NotUtf8 => [3, 0, 0],
            Malformed::Trailing => [4, 0, 0],
        }
    }

    pub(crate) fn from_bytes(bytes: [u8; 3]) -> Option<Malformed> {
        let field = Field::from_byte(bytes[1]);
        Some(match bytes[0] {
            0 => Malformed::Short(field?),
            1 => Malformed::Tag(field?, bytes[2]),
            2 => Malformed::ZeroHandle,
            3 => Malformed::NotUtf8,
            4 => Malformed::Trailing,
            _ => return None,
        })
    }
}

/// Bytes, read from their start.
pub struct Reader<'b> {
    bytes: &'b [u8],
}

impl<'b> Reader<'b> {
    pub fn new(bytes: &'b [u8]) -> Reader<'b> {
        Reader { bytes }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes not read yet.
    pub fn rest(&self) -> &'b [u8] {
        self.bytes
    }

    /// [`Malformed::Trailing`] unless every byte was read.
    pub fn end(&self) -> Result<(), Malformed> {
        if self.bytes.is_empty() { Ok(()) } else { Err(Malformed::Trailing) }
    }

    pub fn take(&mut self, len: usize, field: Field) -> Result<&'b [u8], Malformed> {
        if len > self.bytes.len() {
            return Err(Malformed::Short(field));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self, field: Field) -> Result<[u8; N], Malformed> {
        let Some((head, rest)) = self.bytes.split_first_chunk::<N>() else { return Err(Malformed::Short(field)) };
        self.bytes = rest;
        Ok(*head)
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.array::<1>(Field::Byte)?[0])
    }

    /// A byte that is a tag of `field`.
    pub fn tag(&mut self, field: Field) -> Result<u8, Malformed> {
        Ok(self.array::<1>(field)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, Malformed> {
        match self.tag(Field::Bool)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Malformed::Tag(Field::Bool, other)),
        }
    }

    pub fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.array::<4>(Field::U32)?))
    }

    pub fn usize(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.array::<8>(Field::Usize)?))
    }

    pub fn handle(&mut self) -> Result<u32, Malformed> {
        match u32::from_le_bytes(self.array::<4>(Field::Handle)?) {
            0 => Err(Malformed::ZeroHandle),
            handle => Ok(handle),
        }
    }

    pub fn bytes(&mut self) -> Result<&'b [u8], Malformed> {
        let len = usize::try_from(self.usize()?).map_err(|_| Malformed::Short(Field::Str))?;
        self.take(len, Field::Str)
    }

    pub fn str(&mut self) -> Result<&'b str, Malformed> {
        core::str::from_utf8(self.bytes()?).map_err(|_| Malformed::NotUtf8)
    }

    /// The tag of an `Option`: whether it is `Some`.
    pub fn some(&mut self) -> Result<bool, Malformed> {
        match self.tag(Field::OptionTag)? {
            0 => Ok(true),
            1 => Ok(false),
            other => Err(Malformed::Tag(Field::OptionTag, other)),
        }
    }

    /// The tag of a `Result`: whether it is `Ok`.
    pub fn ok(&mut self) -> Result<bool, Malformed> {
        match self.tag(Field::ResultTag)? {
            0 => Ok(true),
            1 => Ok(false),
            other => Err(Malformed::Tag(Field::ResultTag, other)),
        }
    }

    pub fn bound(&mut self) -> Result<Bound<u64>, Malformed> {
        match self.tag(Field::BoundTag)? {
            0 => Ok(Bound::Included(self.usize()?)),
            1 => Ok(Bound::Excluded(self.usize()?)),
            2 => Ok(Bound::Unbounded),
            other => Err(Malformed::Tag(Field::BoundTag, other)),
        }
    }

    /// The length of a list, each element at least a byte: a length past the bytes left is
    /// refused before anything is reserved for it.
    pub fn count(&mut self) -> Result<usize, Malformed> {
        match usize::try_from(self.usize()?) {
            Ok(len) if len <= self.bytes.len() => Ok(len),
            _ => Err(Malformed::Short(Field::List)),
        }
    }
}

/// Bytes, written onto a buffer the caller reuses.
pub struct Writer<'w, 'a> {
    pub out: &'w mut AVec<'a, u8>,
}

impl Writer<'_, '_> {
    /// Writes `value` over the `usize` written at `at`: a count known only once what it counts
    /// is written.
    pub fn set_usize(&mut self, at: usize, value: u64) {
        if let Some(slot) = self.out.get_mut(at..at + 8) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }

    pub fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }

    pub fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn usize(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn handle(&mut self, handle: u32) {
        self.u32(handle);
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.usize(len64(value.len()));
        self.out.extend_from_slice(value);
    }

    pub fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// A string whose bytes are `parts` end to end.
    pub fn str_parts(&mut self, parts: &[&str]) {
        let len = parts.iter().map(|p| p.len()).sum::<usize>();
        self.usize(len64(len));
        for part in parts {
            self.out.extend_from_slice(part.as_bytes());
        }
    }

    /// A string formatted in place: its length patched once it is written.
    pub fn str_fmt(&mut self, args: fmt::Arguments<'_>) {
        let at = self.out.len();
        self.usize(0);
        let start = self.out.len();
        // Writing into a vector never fails; a `Display` impl that fails leaves what it wrote.
        let _ = fmt::write(&mut Sink { out: &mut *self.out }, args);
        let len = self.out.len() - start;
        self.set_usize(at, len64(len));
    }

    pub fn some(&mut self, some: bool) {
        self.out.push(if some { 0 } else { 1 });
    }

    pub fn ok(&mut self, ok: bool) {
        self.out.push(if ok { 0 } else { 1 });
    }
}

/// `fmt::Write` onto a byte buffer.
pub(crate) struct Sink<'w, 'a> {
    pub(crate) out: &'w mut AVec<'a, u8>,
}

impl fmt::Write for Sink<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.out.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

pub(crate) fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// `n` as a `u32` index; every table here is capped far below `u32::MAX` (server.rs
/// `ServerLimits`, outcome.rs `OutcomeLimits`), so the saturation is never reached.
pub(crate) fn index(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

pub(crate) fn wide(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
