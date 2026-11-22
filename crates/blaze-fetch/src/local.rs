//! A crate's archive found on this machine, adopted only by the lock's pin (sans-IO; fetch.md,
//! "End state: dependencies on the command path", piece 1).
//!
//! The network is the costliest place a `.crate` comes from. Another tool on the machine often
//! holds the very bytes already: cargo keeps every archive it downloaded under
//! `<cargo home>/registry/cache/<registry folder>/<name>-<version>.crate`, and those bytes are
//! the ones whose SHA-256 the lock's `checksum` states. The pin makes the place irrelevant:
//!
//! - nothing is believed of the place: not the folder's name, not the file's name, not cargo's
//!   unpacked copy under `registry/src` (whose files anyone may have edited, and which no digest
//!   covers); only the archive's bytes, compared with the lock's SHA-256 before one is inflated;
//! - an adopted archive becomes exactly the record the network path makes
//!   ([`unpack::crate_record_in`]: same limits, same path rules, same bytes), at the same key
//!   ([`Unit::key`]), so a reader cannot tell which place the bytes came from;
//! - an archive that is not the lock's is [`AdoptRefused::Mismatch`], never adopted; the caller
//!   records the try ([`Try`]) and goes on to the next place in the order fetch.md names, and a
//!   unit no place could give fails with every try named ([`Tried`]): nothing is skipped
//!   quietly.
//!
//! The IO (listing cargo's cache, reading one file) is the driver's: this module decides which
//! file names to read ([`crates_io_folder`], [`archive_name`]) and what the bytes are worth.

#![blaze::sans_io]

use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes as _};

use crate::pack::Sink;
use crate::plan::Unit;
use crate::unpack::{self, Limits, Made, UnpackError};

/// Where cargo keeps registry archives, under its home (`CARGO_HOME`, else `<home>/.cargo`; the
/// command reads both at its edge, rule 75).
pub const CARGO_CACHE: &str = "registry/cache";

/// Whether `folder`, a name listed in cargo's [`CARGO_CACHE`], is one cargo keeps crates.io's
/// archives in: `index.crates.io-` (the sparse index) or `github.com-` (the git index), then 16
/// lowercase hex digits (cargo's hash of the source id, which differs between cargo versions:
/// so the prefix decides, not one hash). Another registry's folder is never read: its archives
/// may share a crates.io crate's name and version, and blaze does not fetch other registries.
/// The digest would refuse a wrong archive anyway; the rule keeps the tries few and named.
pub fn crates_io_folder(folder: &str) -> bool {
    let hash = match folder.strip_prefix("index.crates.io-").or_else(|| folder.strip_prefix("github.com-")) {
        Some(hash) => hash,
        None => return false,
    };
    hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The longest crate name and version the plan lets through (plan.rs: `{1,64}` each).
const PART_MOST: usize = 64;
const SUFFIX: &str = ".crate";
const NAME_MOST: usize = PART_MOST + 1 + PART_MOST + SUFFIX.len();

/// A `.crate`'s file name, `<name>-<version>.crate`: one path component, no `/`, built from
/// characters the plan's alphabets allow and checked again here (whatever becomes a path is).
#[derive(Clone, Copy)]
pub struct ArchiveName {
    bytes: [u8; NAME_MOST],
    len: u8,
}

impl ArchiveName {
    pub fn as_str(&self) -> &str {
        // Only ASCII bytes are ever written (`archive_name`).
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl core::fmt::Debug for ArchiveName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an archive found on the machine is not a unit's record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdoptRefused {
    /// The unit is a git commit: cargo's `.crate` cache holds registry archives only.
    NotACrate,
    /// The unit's name or version is longer than the plan allows or holds a character outside
    /// its alphabet: no file name is made of it.
    Name,
    /// The archive's SHA-256 is not the lock's: another archive under the same name.
    Mismatch { want: [u8; 32], got: [u8; 32] },
    /// The archive is the lock's, yet unpacking it under the limits refused it: the same
    /// refusal the network path gives these bytes.
    Unpack(UnpackError),
}

impl Show for AdoptRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            AdoptRefused::NotACrate => out.str("a git commit is not a registry archive"),
            AdoptRefused::Name => out.str("its name or version is not one a file name is made of"),
            AdoptRefused::Mismatch { want, got } => {
                out.str("its sha256 is ").hex_bytes(got).str(", not the lock's ").hex_bytes(want)
            }
            AdoptRefused::Unpack(why) => out.str("the lock's archive, refused unpacking: ").show(why),
        };
    }
}

/// The file name of `unit`'s archive in cargo's cache.
pub fn archive_name(unit: &Unit<'_, '_>) -> Result<ArchiveName, AdoptRefused> {
    let Unit::Crate { name, version, .. } = unit else {
        return Err(AdoptRefused::NotACrate);
    };
    let version = version.as_str();
    let name_ok = (1..=PART_MOST).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    let version_ok = (1..=PART_MOST).contains(&version.len())
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'-');
    if !name_ok || !version_ok {
        return Err(AdoptRefused::Name);
    }
    let mut out = ArchiveName { bytes: [0; NAME_MOST], len: 0 };
    let mut at = 0;
    for part in [name.as_bytes(), b"-", version.as_bytes(), SUFFIX.as_bytes()] {
        out.bytes[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    // At most NAME_MOST (135), by the checks above.
    out.len = u8::try_from(at).map_err(|_| AdoptRefused::Name)?;
    Ok(out)
}

/// `unit`'s record made from `archive`, bytes found on this machine, written into `out` (the heap
/// or a reservation, as the network path's): only when `archive`'s SHA-256 is the lock's, checked
/// before a byte is inflated. The record equals, byte for byte, the one the network path makes of
/// the same archive, and is filed at the same key.
pub fn adopt_in<'a, S: Sink>(
    unit: &Unit<'_, '_>,
    archive: &[u8],
    limits: &Limits,
    out: S,
    alloc: Alloc<'a>,
) -> Result<(S, Made), AdoptRefused> {
    let Unit::Crate { name, version, sha256, .. } = unit else {
        return Err(AdoptRefused::NotACrate);
    };
    let got = blaze_sha256::digest(archive);
    if got != *sha256 {
        return Err(AdoptRefused::Mismatch { want: *sha256, got });
    }
    unpack::crate_record_in(name, version.as_str(), *sha256, archive, limits, out, alloc).map_err(AdoptRefused::Unpack)
}

/// [`adopt_in`], into the heap.
pub fn adopt<'a>(
    unit: &Unit<'_, '_>,
    archive: &[u8],
    limits: &Limits,
    alloc: Alloc<'a>,
) -> Result<(Vec<u8, Alloc<'a>>, Made), AdoptRefused> {
    adopt_in(unit, archive, limits, Vec::new_in(alloc), alloc)
}

/// What one place gave for a unit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Miss {
    /// No such file.
    Absent,
    /// The file could not be read (permissions, IO): the place is skipped by name.
    Unreadable,
    /// The file was read and refused.
    Refused(AdoptRefused),
}

/// One place looked in for a unit's archive, and what it gave: kept by the driver so a unit no
/// place gives fails naming every place tried (rule 19), and a refused archive is reported, not
/// passed over in silence (rule 3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Try<'p> {
    /// The place, as the person knows it: the folder under cargo's cache, or a store's name.
    pub place: &'p str,
    pub miss: Miss,
}

/// A unit and every place tried for it, for the finding that names them.
pub struct Tried<'t, 'u, 'l, 'a> {
    pub unit: &'u Unit<'l, 'a>,
    pub tries: &'t [Try<'t>],
}

impl Show for Tried<'_, '_, '_, '_> {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        out.show(self.unit).str(": not on this machine");
        for (at, tried) in self.tries.iter().enumerate() {
            out.str(if at == 0 { " (" } else { "; " }).str(tried.place).str(": ");
            match &tried.miss {
                Miss::Absent => out.str("absent"),
                Miss::Unreadable => out.str("could not be read"),
                Miss::Refused(why) => out.show(why),
            };
        }
        if !self.tries.is_empty() {
            out.str(")");
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_manifest::lock::LockId;
    use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
    use blaze_tar::{Make, Writer};
    use blaze_text::Writes as _;

    use super::{AdoptRefused, Miss, Tried, Try, adopt, archive_name, crates_io_folder};
    use crate::pack::PackView;
    use crate::plan::Unit;
    use crate::unpack::{self, Limits};

    /// A `.crate`: a gzip of a tar whose every entry is under `c-1.0.0/`.
    fn archive(lib: &[u8]) -> OwnVec<u8, Alloc<'static>> {
        let mut tar = OwnVec::new_in(blaze_own::testing::heap().alloc());
        let mut w = Writer::new();
        w.entry("c-1.0.0", Make::Dir, &[], &mut tar).expect("a folder entry");
        w.entry("c-1.0.0/Cargo.toml", Make::File { exec: false }, b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n", &mut tar)
            .expect("a manifest entry");
        w.entry("c-1.0.0/src/lib.rs", Make::File { exec: false }, lib, &mut tar).expect("a source entry");
        w.finish(&mut tar).expect("the tar's end");
        let mut gz = OwnVec::new_in(blaze_own::testing::heap().alloc());
        blaze_deflate::gzip(tar.as_slice(), blaze_deflate::Level::Fast, blaze_own::testing::heap().alloc(), &mut gz).expect("gzip");
        gz
    }

    /// The pin is the only thing believed (fetch.md, "Trust steps", step 3): an archive found on the
    /// machine under the lock's name and version is adopted when its SHA-256 is the lock's, and
    /// the record it makes is the network path's record of the same bytes, opening under the
    /// lock's source id; another archive under the same name (a valid crate, one byte of a file
    /// different) is refused with both digests, so taking it without the check fails this test.
    ///
    /// Expected: `test local::tests::an_archive_on_the_machine_is_adopted_only_by_the_locks_sha256 ... ok`
    #[test]
    fn an_archive_on_the_machine_is_adopted_only_by_the_locks_sha256() {
        let pinned = archive(b"pub fn a() {}\n");
        let other = archive(b"pub fn b() {}\n");
        let sha256 = blaze_sha256::digest(pinned.as_slice());
        let mut version = OwnString::new_in(blaze_own::testing::heap().alloc());
        version.str("1.0.0");
        let unit = Unit::Crate { package: LockId(0), name: "c", version, sha256 };
        let limits = Limits::REAL;

        assert_eq!(archive_name(&unit).expect("a crate's file name").as_str(), "c-1.0.0.crate");

        let (record, made) = adopt(&unit, pinned.as_slice(), &limits, blaze_own::testing::heap().alloc()).expect("the lock's archive is adopted");
        let (network, _) = unpack::crate_record("c", "1.0.0", sha256, pinned.as_slice(), &limits, blaze_own::testing::heap().alloc())
            .expect("the network path's record of the same bytes");
        assert_eq!(record.as_slice(), network.as_slice(), "an adopted record is the network path's, byte for byte");
        assert_eq!(made.files, 2);
        let view = PackView::open_source(&unit.source(), record.as_slice()).expect("it opens under the lock's source id");
        assert_eq!(view.find("src/lib.rs").map(|file| file.bytes), Some(&b"pub fn a() {}\n"[..]));

        let got = blaze_sha256::digest(other.as_slice());
        assert_ne!(got, sha256);
        assert_eq!(
            adopt(&unit, other.as_slice(), &limits, blaze_own::testing::heap().alloc()).map(|(_, made)| made),
            Err(AdoptRefused::Mismatch { want: sha256, got }),
            "another archive under the lock's name is refused before it is inflated"
        );

        // The words a failure prints name the unit and every place tried.
        let tries = [
            Try { place: "index.crates.io-1949cf8c6b5b557f", miss: Miss::Absent },
            Try { place: "github.com-1ecc6299db9ec823", miss: Miss::Refused(AdoptRefused::NotACrate) },
        ];
        let mut words = OwnString::new_in(blaze_own::testing::heap().alloc());
        words.show(&Tried { unit: &unit, tries: &tries });
        assert_eq!(
            words.as_str(),
            "crate c 1.0.0: not on this machine (index.crates.io-1949cf8c6b5b557f: absent; github.com-1ecc6299db9ec823: a git commit is not a registry archive)"
        );
    }

    /// Only crates.io's folders of cargo's cache are read, whatever hash cargo's version gave
    /// them; a git commit has no archive name; a name past the plan's alphabet makes no path.
    ///
    /// Expected: `test local::tests::only_crates_io_folders_and_plan_names_make_paths ... ok`
    #[test]
    fn only_crates_io_folders_and_plan_names_make_paths() {
        assert!(crates_io_folder("index.crates.io-1949cf8c6b5b557f"));
        assert!(crates_io_folder("index.crates.io-6f17d22bba15001f"));
        assert!(crates_io_folder("github.com-1ecc6299db9ec823"));
        assert!(!crates_io_folder("my-registry.example-1949cf8c6b5b557f"));
        assert!(!crates_io_folder("index.crates.io-1949CF8C6B5B557F"));
        assert!(!crates_io_folder("index.crates.io-1949cf8c6b5b557"));
        assert!(!crates_io_folder("index.crates.io-1949cf8c6b5b557f/.."));

        let git = Unit::Git { repo: "https://example.com/r", commit: blaze_git_fetch::Oid([0; 20]), packages: 1 };
        assert_eq!(archive_name(&git).map(|name| name.len), Err(AdoptRefused::NotACrate));

        let mut version = OwnString::new_in(blaze_own::testing::heap().alloc());
        version.str("1.0.0/../x");
        let escape = Unit::Crate { package: LockId(0), name: "c", version, sha256: [0; 32] };
        assert_eq!(archive_name(&escape).map(|name| name.len), Err(AdoptRefused::Name));
    }
}
