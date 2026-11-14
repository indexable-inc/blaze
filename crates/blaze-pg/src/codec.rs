//! The binary format of the five storages and their one-dimensional arrays, both ways.
//!
//! | Storage | Postgres type | OID | Array OID | Binary value |
//! |---|---|---|---|---|
//! | `Boolean` | `boolean` | 16 | 1000 | one byte, 0 or 1 |
//! | `Integer` | `bigint` | 20 | 1016 | 8 bytes, big-endian two's complement |
//! | `Real` | `double precision` | 701 | 1022 | 8 bytes, the IEEE 754 bits big-endian |
//! | `Text` | `text` | 25 | 1009 | the UTF-8 bytes (no NUL: Postgres text holds none) |
//! | `Blob` | `bytea` | 17 | 1001 | the bytes |
//!
//! An array is `int32` dimensions (1, or 0 when empty), `int32` has-null flag (0 or 1), the
//! element OID, then per dimension its `int32` length and lower bound (always 1 here), then each
//! element as `int32` length (-1 for null) and its bytes. Nothing else is read: a second
//! dimension, another lower bound, another element type is refused by name.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use blaze_sql::desc::{Storage, ValueRef};

use crate::proto::{MESSAGE_MOST, put_i32, put_u32};

pub const BOOL: u32 = 16;
pub const BYTEA: u32 = 17;
pub const INT8: u32 = 20;
pub const TEXT: u32 = 25;
pub const FLOAT8: u32 = 701;
pub const BOOL_ARRAY: u32 = 1000;
pub const BYTEA_ARRAY: u32 = 1001;
pub const TEXT_ARRAY: u32 = 1009;
pub const INT8_ARRAY: u32 = 1016;
pub const FLOAT8_ARRAY: u32 = 1022;

/// The OID of a value of `storage`.
pub fn scalar_oid(storage: Storage) -> u32 {
    match storage {
        Storage::Boolean => BOOL,
        Storage::Integer => INT8,
        Storage::Real => FLOAT8,
        Storage::Text => TEXT,
        Storage::Blob => BYTEA,
    }
}

/// The OID of a one-dimensional array of `storage`.
pub fn array_oid(storage: Storage) -> u32 {
    match storage {
        Storage::Boolean => BOOL_ARRAY,
        Storage::Integer => INT8_ARRAY,
        Storage::Real => FLOAT8_ARRAY,
        Storage::Text => TEXT_ARRAY,
        Storage::Blob => BYTEA_ARRAY,
    }
}

/// A type's name for messages; OIDs this crate does not speak are named by number by the
/// caller.
pub fn type_name(oid: u32) -> Option<&'static str> {
    Some(match oid {
        BOOL => "boolean",
        BYTEA => "bytea",
        INT8 => "bigint",
        TEXT => "text",
        FLOAT8 => "double precision",
        BOOL_ARRAY => "boolean[]",
        BYTEA_ARRAY => "bytea[]",
        TEXT_ARRAY => "text[]",
        INT8_ARRAY => "bigint[]",
        FLOAT8_ARRAY => "double precision[]",
        23 => "integer",
        21 => "smallint",
        700 => "real",
        1043 => "character varying",
        1042 => "character",
        1700 => "numeric",
        0 => "unspecified",
        _ => return None,
    })
}

/// An OID as `bigint (OID 20)`, or `OID 12345` for one without a name here.
pub struct Oid(pub u32);

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match type_name(self.0) {
            Some(name) => write!(f, "{name} (OID {})", self.0),
            None => write!(f, "OID {}", self.0),
        }
    }
}

fn storage_name(storage: Storage) -> &'static str {
    match storage {
        Storage::Boolean => "boolean",
        Storage::Integer => "bigint",
        Storage::Real => "double precision",
        Storage::Text => "text",
        Storage::Blob => "bytea",
    }
}

/// Why a value cannot be bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Its bytes are longer than a message may be.
    TooLong { len: usize },
    /// A text holds a NUL at this byte, which Postgres text cannot hold.
    TextHasNul { at: usize },
    /// Element `index` of an array of `expected` is a value of `got`.
    ElementStorage { index: usize, expected: Storage, got: Storage },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            EncodeError::TooLong { len } => {
                write!(f, "its {len} bytes are more than a message holds ({MESSAGE_MOST})")
            }
            EncodeError::TextHasNul { at } => {
                write!(f, "its text has a NUL at byte {at}, which Postgres text cannot hold")
            }
            EncodeError::ElementStorage { index, expected, got } => write!(
                f,
                "element {index} of an array of {} is a {} value",
                storage_name(expected),
                storage_name(got)
            ),
        }
    }
}

/// Why bytes the server sent are not a value of the storage they were read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// A `storage` value of `len` bytes: boolean takes 1, bigint and double precision 8.
    Length { storage: Storage, len: usize },
    /// A boolean byte other than 0 or 1.
    Boolean { byte: u8 },
    /// Text that is not UTF-8 past this byte.
    NotUtf8 { valid_up_to: usize },
    /// A value length below -1.
    NegativeLength { len: i32 },
    /// An array of other than 0 or 1 dimensions.
    Dimensions { dimensions: i32 },
    /// A has-null flag other than 0 or 1.
    NullFlag { flag: i32 },
    /// A null element in an array whose flag says it has none.
    NullNotFlagged { index: u32 },
    /// An array's element OID, where `expected` was asked for.
    ElementOid { expected: u32, got: u32 },
    /// An array's lower bound other than 1.
    LowerBound { bound: i32 },
    /// An array's length below 0.
    ArrayLength { len: i32 },
    /// The bytes ended inside `field`.
    Truncated { field: &'static str },
    /// Bytes after the last element.
    Trailing { bytes: usize },
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ValueError::Length { storage, len } => {
                let want = match storage {
                    Storage::Boolean => "1 byte",
                    Storage::Integer | Storage::Real => "8 bytes",
                    Storage::Text | Storage::Blob => "any length",
                };
                write!(f, "a {} value of {len} bytes (it takes {want})", storage_name(storage))
            }
            ValueError::Boolean { byte } => {
                write!(f, "a boolean byte {byte} (it is 0 or 1)")
            }
            ValueError::NotUtf8 { valid_up_to } => {
                write!(f, "text that is not UTF-8 after byte {valid_up_to}")
            }
            ValueError::NegativeLength { len } => {
                write!(f, "a value length of {len} (below -1, the null)")
            }
            ValueError::Dimensions { dimensions } => {
                write!(
                    f,
                    "an array of {dimensions} dimensions (only one-dimensional arrays are read)"
                )
            }
            ValueError::NullFlag { flag } => write!(f, "an array has-null flag of {flag} (0 or 1)"),
            ValueError::NullNotFlagged { index } => {
                write!(f, "a null element {index} in an array whose flag says it has none")
            }
            ValueError::ElementOid { expected, got } => {
                write!(f, "an array of {}, where one of {} was asked for", Oid(got), Oid(expected))
            }
            ValueError::LowerBound { bound } => {
                write!(f, "an array with lower bound {bound} (only 1 is read)")
            }
            ValueError::ArrayLength { len } => write!(f, "an array of length {len}"),
            ValueError::Truncated { field } => write!(f, "bytes that end inside the {field}"),
            ValueError::Trailing { bytes } => {
                write!(f, "{bytes} bytes after the array's last element")
            }
        }
    }
}

/// The bytes of a non-null `value`, without a length. Text is checked for NUL.
fn body<'v>(
    value: &ValueRef<'v>,
    scratch: &'v mut [u8; 8],
) -> Result<Option<&'v [u8]>, EncodeError> {
    Ok(Some(match *value {
        ValueRef::Null => return Ok(None),
        ValueRef::Boolean(it) => {
            scratch[0] = u8::from(it);
            &scratch[..1]
        }
        ValueRef::Integer(it) => {
            *scratch = it.to_be_bytes();
            &scratch[..]
        }
        ValueRef::Real(it) => {
            *scratch = it.to_bits().to_be_bytes();
            &scratch[..]
        }
        ValueRef::Text(it) => {
            if let Some(at) = it.bytes().position(|b| b == 0) {
                return Err(EncodeError::TextHasNul { at });
            }
            it.as_bytes()
        }
        ValueRef::Blob(it) => it,
    }))
}

/// Writes `len` as a value's `int32` length, refusing one past [`MESSAGE_MOST`].
fn put_len<A: Allocator>(out: &mut Vec<u8, A>, len: usize) -> Result<(), EncodeError> {
    if len > MESSAGE_MOST {
        return Err(EncodeError::TooLong { len });
    }
    // At most `MESSAGE_MOST`, below `i32::MAX`.
    put_i32(out, len as i32);
    Ok(())
}

/// Writes `value` as a bind parameter or a column: its `int32` length (-1 for null), then its
/// binary bytes.
pub fn encode<A: Allocator>(value: ValueRef<'_>, out: &mut Vec<u8, A>) -> Result<(), EncodeError> {
    let mut scratch = [0u8; 8];
    match body(&value, &mut scratch)? {
        None => put_i32(out, -1),
        Some(bytes) => {
            put_len(out, bytes.len())?;
            out.extend_from_slice(bytes);
        }
    }
    Ok(())
}

/// Writes a one-dimensional array of `storage` holding `items` (nulls allowed), with its
/// `int32` length first, as a bind parameter.
pub fn encode_array<A: Allocator>(
    storage: Storage,
    items: &[ValueRef<'_>],
    out: &mut Vec<u8, A>,
) -> Result<(), EncodeError> {
    let length_at = out.len();
    put_i32(out, 0);
    let start = out.len();
    let has_null = items.iter().any(|it| matches!(it, ValueRef::Null));
    if items.len() > MESSAGE_MOST / 4 {
        return Err(EncodeError::TooLong { len: items.len().saturating_mul(4) });
    }
    put_i32(out, i32::from(!items.is_empty()));
    put_i32(out, i32::from(has_null));
    put_u32(out, scalar_oid(storage));
    if !items.is_empty() {
        // At most `MESSAGE_MOST / 4`, checked above.
        put_i32(out, items.len() as i32);
        put_i32(out, 1);
    }
    for (index, item) in items.iter().enumerate() {
        match item.storage() {
            None => {}
            Some(got) if got == storage => {}
            Some(got) => return Err(EncodeError::ElementStorage { index, expected: storage, got }),
        }
        encode(*item, out)?;
        if out.len() - start > MESSAGE_MOST {
            return Err(EncodeError::TooLong { len: out.len() - start });
        }
    }
    let len = out.len() - start;
    // Checked against `MESSAGE_MOST` after each element, below `i32::MAX`.
    let field = (len as i32).to_be_bytes();
    if let Some(slot) = out.get_mut(length_at..start) {
        slot.copy_from_slice(&field);
    }
    Ok(())
}

/// The value of `storage` in `bytes` (`None` is the null).
pub fn decode(storage: Storage, bytes: Option<&[u8]>) -> Result<ValueRef<'_>, ValueError> {
    let Some(bytes) = bytes else {
        return Ok(ValueRef::Null);
    };
    let eight = |bytes: &[u8]| -> Result<[u8; 8], ValueError> {
        bytes.try_into().map_err(|_| ValueError::Length { storage, len: bytes.len() })
    };
    Ok(match storage {
        Storage::Boolean => match *bytes {
            [0] => ValueRef::Boolean(false),
            [1] => ValueRef::Boolean(true),
            [byte] => return Err(ValueError::Boolean { byte }),
            _ => return Err(ValueError::Length { storage, len: bytes.len() }),
        },
        Storage::Integer => ValueRef::Integer(i64::from_be_bytes(eight(bytes)?)),
        Storage::Real => ValueRef::Real(f64::from_bits(u64::from_be_bytes(eight(bytes)?))),
        Storage::Text => match core::str::from_utf8(bytes) {
            Ok(text) => ValueRef::Text(text),
            Err(error) => return Err(ValueError::NotUtf8 { valid_up_to: error.valid_up_to() }),
        },
        Storage::Blob => ValueRef::Blob(bytes),
    })
}

/// Splits one length-prefixed value off `bytes`: `None` for the null. `field` names it in
/// errors.
pub(crate) fn split_value<'a>(
    bytes: &mut &'a [u8],
    field: &'static str,
) -> Result<Option<&'a [u8]>, ValueError> {
    let Some((len, rest)) = bytes.split_first_chunk::<4>() else {
        return Err(ValueError::Truncated { field });
    };
    let len = i32::from_be_bytes(*len);
    *bytes = rest;
    if len == -1 {
        return Ok(None);
    }
    let Ok(len) = usize::try_from(len) else {
        return Err(ValueError::NegativeLength { len });
    };
    let Some((value, rest)) = bytes.split_at_checked(len) else {
        return Err(ValueError::Truncated { field });
    };
    *bytes = rest;
    Ok(Some(value))
}

/// The elements of a one-dimensional array of `storage` whose binary bytes are `bytes`
/// (without the value's own length). The header is checked here, each element as it is read.
pub fn decode_array(storage: Storage, bytes: &[u8]) -> Result<Elements<'_>, ValueError> {
    let mut rest = bytes;
    let mut int = |field: &'static str| -> Result<i32, ValueError> {
        let Some((word, after)) = rest.split_first_chunk::<4>() else {
            return Err(ValueError::Truncated { field });
        };
        rest = after;
        Ok(i32::from_be_bytes(*word))
    };
    let dimensions = int("array's dimensions")?;
    let flag = int("array's has-null flag")?;
    let element = int("array's element OID")? as u32;
    if !(0..=1).contains(&flag) {
        return Err(ValueError::NullFlag { flag });
    }
    if element != scalar_oid(storage) {
        return Err(ValueError::ElementOid { expected: scalar_oid(storage), got: element });
    }
    let left = match dimensions {
        0 => 0,
        1 => {
            let len = int("array's length")?;
            let bound = int("array's lower bound")?;
            if bound != 1 {
                return Err(ValueError::LowerBound { bound });
            }
            match u32::try_from(len) {
                Ok(len) => len,
                Err(_) => return Err(ValueError::ArrayLength { len }),
            }
        }
        dimensions => return Err(ValueError::Dimensions { dimensions }),
    };
    Ok(Elements { storage, rest, left, index: 0, nulls: flag == 1 })
}

/// The elements of an array, decoded as they are read: an error ends it.
#[derive(Clone, Debug)]
pub struct Elements<'a> {
    storage: Storage,
    rest: &'a [u8],
    left: u32,
    index: u32,
    nulls: bool,
}

impl<'a> Elements<'a> {
    /// How many elements are still to come, as the header says.
    pub fn left(&self) -> u32 {
        self.left
    }

    /// After the last element: the array's bytes are used up.
    pub fn finish(&self) -> Result<(), ValueError> {
        match (self.left, self.rest.len()) {
            (0, 0) => Ok(()),
            (0, bytes) => Err(ValueError::Trailing { bytes }),
            (_, _) => Err(ValueError::Truncated { field: "array's elements" }),
        }
    }
}

impl<'a> Iterator for Elements<'a> {
    type Item = Result<ValueRef<'a>, ValueError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let index = self.index;
        self.index = self.index.saturating_add(1);
        let result = (|| {
            let value = split_value(&mut self.rest, "array element")?;
            if value.is_none() && !self.nulls {
                return Err(ValueError::NullNotFlagged { index });
            }
            decode(self.storage, value)
        })();
        if result.is_err() {
            // An error ends the array: nothing after it is read.
            self.left = 0;
            self.rest = &[];
        }
        Some(result)
    }
}
