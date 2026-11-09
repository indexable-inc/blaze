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
/// parser accepts (docs/v2/plan/design/edition.md): with none on, the parser is Rust's, error
/// for error. A file's tree is a function of its text, its [`Edition`] and this set, and every
/// key of a parse holds all three (the `span` crate's `EditionedFileId` carries the set beside
/// the edition). Each bit is one extension; the set is closed, and an unknown bit is refused
/// when read back ([`Extensions::from_bits`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Extensions {
    bits: u8,
}

impl Extensions {
    /// No extension: Rust's syntax.
    pub const NONE: Extensions = Extensions { bits: 0 };
    /// `#![blaze::uniq]`: `&uniq T`, `&'a uniq T`, `&uniq expr`, `&uniq self`, `ref uniq x`
    /// and the pattern `&uniq pat` are the unique reference, spelled for what it guarantees; each is exactly
    /// its `mut` spelling from the syntax tree on (docs/v2/plan/design/edition.md, change 10).
    pub const UNIQ: Extensions = Extensions { bits: 1 << 0 };
    /// `#![blaze::error_unions]`: `A | B` in type position (docs/v2/plan/design/edition.md,
    /// change 2).
    pub const ERROR_UNIONS: Extensions = Extensions { bits: 1 << 1 };
    /// Every extension, in bit order: what a test walks to cover each set.
    pub const ALL: [Extensions; 2] = [Extensions::UNIQ, Extensions::ERROR_UNIONS];
    /// How many low bits the set takes: one per extension.
    pub const BITS: u32 = Self::ALL.len() as u32;
    /// Every bit some extension has.
    const KNOWN: u8 = (1 << Self::BITS) - 1;

    /// The set as one byte, for a key or an encoding, as [`Extensions::from_bits`] reads it
    /// back.
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

    /// Whether `#![blaze::uniq]` is on.
    pub const fn uniq(self) -> bool {
        self.bits & Self::UNIQ.bits != 0
    }

    /// Whether `#![blaze::error_unions]` is on.
    pub const fn error_unions(self) -> bool {
        self.bits & Self::ERROR_UNIONS.bits != 0
    }
}
