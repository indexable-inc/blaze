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

/// A blaze edition (docs/v2/plan/design/std.md, "The blaze edition: opting in";
/// docs/v2/plan/design/edition.md): a package opts in with `edition = "blaze-2026.11"` in
/// its manifest, which is the Rust edition [`BlazeEdition::rust`] plus the crate attribute
/// [`BlazeEdition::crate_attr`] on each of the package's crates (but its build script and a
/// proc-macro target). Under it the crate's `std` is the dependency named
/// [`BlazeEdition::STD`], its prelude is that crate's `prelude::` module
/// [`BlazeEdition::prelude`], and a program's `main` takes its world as parameters. A crate
/// without the attribute is Rust's, unchanged. The one home of every spelling: the manifest's
/// value, the attribute, the prelude's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BlazeEdition {
    /// `blaze-2026.11`: Rust 2024, `std` is bstd, `main` takes `&Cx` and `Io`.
    Blaze2026_11 = 0,
}

/// A manifest's `edition` that starts [`BlazeEdition::MANIFEST_PREFIX`] and is no blaze
/// edition's: the value as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBlazeEdition {
    pub written: String,
}

impl std::error::Error for UnknownBlazeEdition {}
impl fmt::Display for UnknownBlazeEdition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is no blaze edition: this blaze knows ", self.written)?;
        for (at, it) in BlazeEdition::ALL.iter().enumerate() {
            write!(f, "{}`{}`", if at == 0 { "" } else { ", " }, it.manifest())?;
        }
        Ok(())
    }
}

impl BlazeEdition {
    /// The newest: what `blaze new` writes, and what `blaze::not_latest_edition` asks for.
    pub const LATEST: BlazeEdition = BlazeEdition::Blaze2026_11;
    /// Every blaze edition, oldest first.
    pub const ALL: [BlazeEdition; 1] = [BlazeEdition::Blaze2026_11];
    /// What a manifest's `edition` starts with when it names a blaze edition.
    pub const MANIFEST_PREFIX: &'static str = "blaze-";
    /// The name of the dependency a crate of a blaze edition calls `std`: blaze's standard
    /// library's lib (crates/blaze-std).
    pub const STD: &'static str = "bstd";
    /// The path of the crate attribute that says a crate's blaze edition.
    pub const ATTR: &'static str = "blaze::edition";
    /// The module blaze appends to the root of a program of a blaze edition, whose `main`
    /// is the program's entry (it calls the crate's own `main` with the roots it takes).
    pub const ENTRY_MODULE: &'static str = "__blaze_entry";

    /// Its version, as the crate attribute says it: `2026.11`.
    pub const fn version(self) -> &'static str {
        match self {
            BlazeEdition::Blaze2026_11 => "2026.11",
        }
    }

    /// What a manifest's `edition` says for it: `blaze-2026.11`.
    pub const fn manifest(self) -> &'static str {
        match self {
            BlazeEdition::Blaze2026_11 => "blaze-2026.11",
        }
    }

    /// The Rust edition its syntax and name resolution are.
    pub const fn rust(self) -> Edition {
        match self {
            BlazeEdition::Blaze2026_11 => Edition::Edition2024,
        }
    }

    /// The module of its `std`'s `prelude` a crate of it has in scope: `blaze_2026_11`.
    pub const fn prelude(self) -> &'static str {
        match self {
            BlazeEdition::Blaze2026_11 => "blaze_2026_11",
        }
    }

    /// Its crate attribute as given on a command line (rustc's `-Z crate-attr`, no `#![` and
    /// `]`): `blaze::edition = "2026.11"`.
    pub const fn crate_attr(self) -> &'static str {
        match self {
            BlazeEdition::Blaze2026_11 => "blaze::edition = \"2026.11\"",
        }
    }

    /// The edition whose version is `version` (`2026.11`), as its crate attribute says it.
    pub fn from_version(version: &str) -> Option<BlazeEdition> {
        BlazeEdition::ALL.into_iter().find(|it| it.version() == version)
    }

    /// The blaze edition a manifest's `edition` names: `Ok(None)` for one that does not
    /// start [`BlazeEdition::MANIFEST_PREFIX`] (a Rust edition, or nothing blaze reads here),
    /// an error for one that does and is no blaze edition's.
    pub fn from_manifest(written: &str) -> Result<Option<BlazeEdition>, UnknownBlazeEdition> {
        if !written.starts_with(BlazeEdition::MANIFEST_PREFIX) {
            return Ok(None);
        }
        match BlazeEdition::ALL.into_iter().find(|it| it.manifest() == written) {
            Some(it) => Ok(Some(it)),
            None => Err(UnknownBlazeEdition { written: written.to_owned() }),
        }
    }
}

impl fmt::Display for BlazeEdition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.manifest())
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
    /// `#![blaze::records]`: the record type `{ a: T, b: U }`, the record value
    /// `{ a: x, b: y }`, the record pattern `{ a, b: p, .. }` and a record parameter's field
    /// defaults (docs/v2/plan/design/records.md; docs/v2/plan/design/edition.md, change 4).
    pub const RECORDS: Extensions = Extensions { bits: 1 << 2 };
    /// Every extension, in bit order: what a test walks to cover each set.
    pub const ALL: [Extensions; 3] =
        [Extensions::UNIQ, Extensions::ERROR_UNIONS, Extensions::RECORDS];
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

    /// Whether `#![blaze::records]` is on.
    pub const fn records(self) -> bool {
        self.bits & Self::RECORDS.bits != 0
    }
}

#[cfg(test)]
mod blaze_edition_tests {
    use super::{BlazeEdition, Edition};

    /// Each blaze edition is read back from each of its spellings, and every spelling is
    /// derived from the version: the manifest's value, the crate attribute, the prelude.
    #[test]
    fn every_spelling_of_a_blaze_edition_reads_back() {
        for it in BlazeEdition::ALL {
            assert_eq!(BlazeEdition::from_version(it.version()), Some(it));
            assert_eq!(BlazeEdition::from_manifest(it.manifest()), Ok(Some(it)));
            assert_eq!(it.manifest(), format!("{}{}", BlazeEdition::MANIFEST_PREFIX, it.version()));
            assert_eq!(it.crate_attr(), format!("{} = \"{}\"", BlazeEdition::ATTR, it.version()));
            assert_eq!(it.prelude(), format!("blaze_{}", it.version().replace('.', "_")));
            assert_eq!(it.to_string(), it.manifest());
        }
        assert_eq!(BlazeEdition::LATEST, *BlazeEdition::ALL.last().unwrap());
        assert_eq!(BlazeEdition::Blaze2026_11.rust(), Edition::Edition2024);
    }

    /// A Rust edition is no blaze edition and no error; a `blaze-` value blaze does not know
    /// is an error that names it and what is known.
    #[test]
    fn a_manifest_value_is_a_blaze_edition_a_rust_one_or_an_error() {
        for rust in ["2015", "2018", "2021", "2024", "future", ""] {
            assert_eq!(BlazeEdition::from_manifest(rust), Ok(None), "{rust}");
        }
        for unknown in ["blaze-", "blaze-2024", "blaze-2026.12", "blaze-2026.11 "] {
            let refused = BlazeEdition::from_manifest(unknown).unwrap_err();
            assert_eq!(refused.written, unknown);
            let said = refused.to_string();
            assert!(said.contains(unknown) && said.contains("`blaze-2026.11`"), "{said}");
        }
        assert_eq!(BlazeEdition::from_version("blaze-2026.11"), None, "the attribute says the version alone");
    }
}
