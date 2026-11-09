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

/// blaze's crate-level syntax gates: what a crate root's inner attributes turn on that changes
/// how the crate's files parse (docs/v2/plan/design/edition.md). Each is off unless the crate
/// asks for it, so a crate that asks for none parses exactly as rustc's grammar says. A file's
/// tree is a function of its text, its [`Edition`] and these, and every key of a parse holds
/// all three (span's `EditionedFileId` carries them beside the edition).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SyntaxGates(u8);

impl SyntaxGates {
    /// No gate: rustc's grammar.
    pub const NONE: SyntaxGates = SyntaxGates(0);
    /// `#![blaze::uniq]`: `&uniq T`, `&'a uniq T`, `&uniq expr`, `&uniq self` and
    /// `ref uniq x` are the unique reference, spelled for what it guarantees; each is exactly
    /// its `mut` spelling from the syntax tree on.
    pub const UNIQ: SyntaxGates = SyntaxGates(1);
    /// How many low bits the gates take.
    pub const BITS: u32 = 1;

    /// The gates as their bits, for a key or an encoding.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The gates of `bits`, `None` when a bit names no gate.
    pub const fn from_bits(bits: u8) -> Option<SyntaxGates> {
        match bits >> Self::BITS {
            0 => Some(SyntaxGates(bits)),
            _ => None,
        }
    }

    /// Whether `#![blaze::uniq]` is on.
    pub const fn uniq(self) -> bool {
        self.0 & Self::UNIQ.0 != 0
    }

    /// These gates and `other`'s.
    pub const fn with(self, other: SyntaxGates) -> SyntaxGates {
        SyntaxGates(self.0 | other.0)
    }
}
