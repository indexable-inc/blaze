//! The edition of the Rust language used in a crate.
// This should live in a separate crate because we use it in both actual code and codegen.
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Edition {
    // The syntax context stuff needs the discriminants to start from 0 and be consecutive.
    Edition2015 = 0,
    Edition2018,
    Edition2021,
    Edition2024,
}

impl Edition {
    pub const DEFAULT: Edition = Edition::Edition2015;
    pub const LATEST: Edition = Edition::Edition2024;
    pub const CURRENT: Edition = Edition::Edition2024;

    pub fn from_u32(u32: u32) -> Edition {
        match u32 {
            0 => Edition::Edition2015,
            1 => Edition::Edition2018,
            2 => Edition::Edition2021,
            3 => Edition::Edition2024,
            _ => panic!("invalid edition"),
        }
    }

    pub fn at_least_2024(self) -> bool {
        self >= Edition::Edition2024
    }

    pub fn at_least_2021(self) -> bool {
        self >= Edition::Edition2021
    }

    pub fn at_least_2018(self) -> bool {
        self >= Edition::Edition2018
    }

    pub fn number(&self) -> usize {
        match self {
            Edition::Edition2015 => 2015,
            Edition::Edition2018 => 2018,
            Edition::Edition2021 => 2021,
            Edition::Edition2024 => 2024,
        }
    }

    pub fn iter() -> impl Iterator<Item = Edition> {
        [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024]
            .iter()
            .copied()
    }
}

#[derive(Debug)]
pub struct ParseEditionError {
    invalid_input: String,
}

impl std::error::Error for ParseEditionError {}
impl fmt::Display for ParseEditionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid edition: {:?}", self.invalid_input)
    }
}

impl std::str::FromStr for Edition {
    type Err = ParseEditionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let res = match s {
            "2015" => Edition::Edition2015,
            "2018" => Edition::Edition2018,
            "2021" => Edition::Edition2021,
            "2024" => Edition::Edition2024,
            _ => return Err(ParseEditionError { invalid_input: s.to_owned() }),
        };
        Ok(res)
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edition::Edition2015 => "2015",
            Edition::Edition2018 => "2018",
            Edition::Edition2021 => "2021",
            Edition::Edition2024 => "2024",
        })
    }
}

/// blaze's syntax extensions a crate turns on with a crate attribute, which change what the
/// parser accepts: with none on, the parser is Rust's, error for error. A file is parsed with
/// its crate's set (the `span` crate's `EditionedFileId` carries it beside the edition), so the
/// set is part of every key a parse is stored under. Each bit is one extension; the set is
/// closed, and an unknown bit is refused when read back ([`Extensions::from_bits`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Extensions {
    bits: u8,
}

impl Extensions {
    /// No extension: Rust's syntax.
    pub const NONE: Extensions = Extensions { bits: 0 };
    /// `#![blaze::records]`: the record type `{ a: T, b: U }`, the record value
    /// `{ a: x, b: y }`, the record pattern `{ a, b: p, .. }` and a record parameter's field
    /// defaults (docs/v2/plan/design/records.md; docs/v2/plan/design/edition.md, change 4).
    /// Bit 2: bits 0 and 1 are `uniq` and `error_unions` on the edition's integration branch,
    /// so the fold there adds this constant, its accessor and one `EXTENSION_ATTRS` row.
    pub const RECORDS: Extensions = Extensions { bits: 1 << 2 };
    /// Every bit some extension has.
    const KNOWN: u8 = Self::RECORDS.bits;

    /// The set as one byte, as [`Extensions::from_bits`] reads it back.
    pub const fn bits(self) -> u8 {
        self.bits
    }

    /// The set of `bits`, or `None` when a bit is no extension's.
    pub const fn from_bits(bits: u8) -> Option<Extensions> {
        match bits & !Self::KNOWN {
            0 => Some(Extensions { bits }),
            _ => None,
        }
    }

    /// Both sets' extensions.
    #[must_use]
    pub const fn with(self, other: Extensions) -> Extensions {
        Extensions { bits: self.bits | other.bits }
    }

    /// Whether no extension is on.
    pub const fn is_none(self) -> bool {
        self.bits == 0
    }

    /// Whether `#![blaze::records]` is on.
    pub const fn records(self) -> bool {
        self.bits & Self::RECORDS.bits != 0
    }
}
