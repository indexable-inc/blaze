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

pub use blaze_git_fetch::{
    Oid as GitOid,
    store::{ObjectSource as GitObjectSource, StoreWhy as GitStoreWhy},
};

use blaze_own::{Alloc, Vec};
use blaze_text::{Put, Show, Writes as _};

use crate::pack::Sink;
use crate::plan::Unit;
use crate::unpack::{self, Limits, Made, UnpackError};

/// Where cargo keeps registry archives, under its home (`CARGO_HOME`, else `<home>/.cargo`; the
/// command reads both at its edge, rule 75).
pub const CARGO_CACHE: &str = "registry/cache";

/// Where cargo keeps bare git clones, under its home (fetch.md, piece 14).
pub const CARGO_GIT_DB: &str = "git/db";

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

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
    is_lower_hex(hash, 16)
}

/// Whether `folder`, a name listed in cargo's [`CARGO_GIT_DB`], is one cargo keeps `repo`'s
/// bare repository in: `<name>-` followed by 16 lowercase hex digits, where `<name>` is the
/// last non-empty URL path segment of `repo` with any trailing `.git` stripped, or `_empty`
/// when the URL has no path segment (cargo's `GitSource::ident` shape). The commit id's SHA-1
/// binds the bytes regardless; filtering by repository name keeps the tries few and avoids
/// indexing unrelated repositories' packs.
pub fn git_db_folder(repo: &str, folder: &str) -> bool {
    let without_query = repo.split(['?', '#']).next().unwrap_or(repo);
    let url_path = match without_query.split_once("://") {
        Some((_, after_scheme)) => after_scheme.split_once('/').map_or("", |(_, path)| path),
        None => without_query,
    };
    let trimmed = url_path.trim_end_matches('/');
    let stem = if trimmed.is_empty() {
        "_empty"
    } else {
        let raw = trimmed.rsplit('/').next().unwrap_or("");
        let stripped = raw.strip_suffix(".git").unwrap_or(raw);
        if stripped.is_empty() { "_empty" } else { stripped }
    };
    if stem == "." || stem == ".." || stem.contains('/') || stem.contains('\\') {
        return false;
    }
    let Some(hash) = folder.strip_prefix(stem).and_then(|rest| rest.strip_prefix('-')) else {
        return false;
    };
    is_lower_hex(hash, 16)
}

/// Whether `name`, a file listed in `<cargo home>/git/db/<folder>/objects/pack`, is a git
/// pack index file (`pack-<40 lowercase hex digits>.idx`). Ensures a single safe path component.
pub fn git_idx_file(name: &str) -> bool {
    let Some(hex) = name.strip_prefix("pack-").and_then(|rest| rest.strip_suffix(".idx")) else {
        return false;
    };
    is_lower_hex(hex, 40)
}

/// Whether `name`, a file listed in `<cargo home>/git/db/<folder>/objects/pack`, is a git
/// pack data file (`pack-<40 lowercase hex digits>.pack`). Ensures a single safe path component.
pub fn git_pack_file(name: &str) -> bool {
    let Some(hex) = name.strip_prefix("pack-").and_then(|rest| rest.strip_suffix(".pack")) else {
        return false;
    };
    is_lower_hex(hex, 40)
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
    /// The unit is a registry crate: cargo's `git/db` holds bare git packs only.
    NotGit,
    /// The unit's name or version is longer than the plan allows or holds a character outside
    /// its alphabet: no file name is made of it.
    Name,
    /// The archive's SHA-256 is not the lock's: another archive under the same name.
    Mismatch { want: [u8; 32], got: [u8; 32] },
    /// The archive is the lock's, yet unpacking it under the limits refused it: the same
    /// refusal the network path gives these bytes.
    Unpack(UnpackError),
}

impl AdoptRefused {
    /// Whether adoption failed because of heap allocation exhaustion rather than missing or
    /// malformed local cache data.
    pub fn is_alloc(&self) -> bool {
        matches!(self, AdoptRefused::Unpack(why) if why.is_alloc())
    }
}

impl Show for AdoptRefused {
    fn show<P: Put + ?Sized>(&self, out: &mut P) {
        match self {
            AdoptRefused::NotACrate => out.str("a git commit is not a registry archive"),
            AdoptRefused::NotGit => out.str("a registry crate is not a git commit"),
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

/// `unit`'s record made on demand from a bare git repository's object database (`objects/`):
/// loose objects and all indexed packfiles (`packs`: `(raw .idx bytes, .pack byte length)`),
/// reading only objects reachable from the lock's pinned commit SHA-1. The record equals, byte
/// for byte, the one the network path ([`unpack::git_record_in`]) makes for the same commit, and
/// is filed at the same key.
pub fn adopt_git_db_in<'a, D: blaze_git_fetch::store::ObjectSource<Alloc<'a>>, S: Sink>(
    unit: &Unit<'_, '_>,
    packs: &[(&[u8], u64)],
    source: &mut D,
    limits: &Limits,
    out: S,
    alloc: Alloc<'a>,
) -> Result<(S, Made), AdoptRefused> {
    let Unit::Git { commit, .. } = unit else {
        return Err(AdoptRefused::NotGit);
    };
    unpack::git_db_record_in(*commit, packs, source, limits, out, alloc).map_err(AdoptRefused::Unpack)
}

/// [`adopt_git_db_in`], into the heap.
pub fn adopt_git_db<'a, D: blaze_git_fetch::store::ObjectSource<Alloc<'a>>>(
    unit: &Unit<'_, '_>,
    packs: &[(&[u8], u64)],
    source: &mut D,
    limits: &Limits,
    alloc: Alloc<'a>,
) -> Result<(Vec<u8, Alloc<'a>>, Made), AdoptRefused> {
    adopt_git_db_in(unit, packs, source, limits, Vec::new_in(alloc), alloc)
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
    use blaze_git_fetch::{
        Kind as GitKind, Oid,
        store::{ObjectSource, StoreWhy},
    };
    use blaze_manifest::lock::LockId;
    use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
    use blaze_tar::{Make, Writer};
    use blaze_text::Writes as _;

    use super::{
        AdoptRefused, Miss, Tried, Try, adopt, adopt_git_db, archive_name, crates_io_folder, git_db_folder, git_idx_file,
    };
    use crate::pack::PackView;
    use crate::plan::Unit;
    use crate::unpack::{self, Limits, UnpackError};

    fn git_zlib(bytes: &[u8]) -> OwnVec<u8, Alloc<'static>> {
        let mut out = OwnVec::new_in(blaze_own::testing::heap().alloc());
        blaze_deflate::zlib(bytes, blaze_deflate::Level::Fast, blaze_own::testing::heap().alloc(), &mut out).expect("zlib");
        out
    }

    fn git_loose(kind: GitKind, body: &[u8]) -> OwnVec<u8, Alloc<'static>> {
        let mut raw = OwnVec::new_in(blaze_own::testing::heap().alloc());
        let header = alloc::format!("{} {}\0", kind.name(), body.len());
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(body);
        git_zlib(raw.as_slice())
    }

    /// Builds a packfile AND its matching version-2 `.idx` file for `objects`.
    fn git_pack_and_idx(objects: &[(GitKind, &[u8])]) -> (OwnVec<u8, Alloc<'static>>, OwnVec<u8, Alloc<'static>>) {
        let alloc = blaze_own::testing::heap().alloc();
        let mut pack = OwnVec::new_in(alloc);
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&u32::try_from(objects.len()).expect("count").to_be_bytes());
        let mut entries: std::vec::Vec<(Oid, u32)> = std::vec::Vec::new();
        for (kind, bytes) in objects {
            let offset = u32::try_from(pack.len()).expect("offset");
            let oid = Oid::of(*kind, bytes);
            entries.push((oid, offset));
            let number: u8 = match kind {
                GitKind::Commit => 1,
                GitKind::Tree => 2,
                GitKind::Blob => 3,
                GitKind::Tag => 4,
            };
            let mut size = u64::try_from(bytes.len()).expect("len");
            let mut byte = (number << 4) | u8::try_from(size & 15).expect("low4");
            size >>= 4;
            while size > 0 {
                pack.push(byte | 0x80);
                byte = u8::try_from(size & 0x7f).expect("low7");
                size >>= 7;
            }
            pack.push(byte);
            let z = git_zlib(bytes);
            pack.extend_from_slice(z.as_slice());
        }
        let pack_sha = blaze_git_fetch::sha1::sha1(pack.as_slice());
        pack.extend_from_slice(&pack_sha);

        entries.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let mut idx = OwnVec::new_in(alloc);
        idx.extend_from_slice(&[0xff, b't', b'O', b'c']);
        idx.extend_from_slice(&2u32.to_be_bytes());
        for first in 0u8..=255 {
            let count = entries.iter().filter(|(oid, _)| oid.0[0] <= first).count();
            idx.extend_from_slice(&u32::try_from(count).expect("fanout").to_be_bytes());
        }
        for (oid, _) in &entries {
            idx.extend_from_slice(&oid.0);
        }
        for _ in &entries {
            idx.extend_from_slice(&0u32.to_be_bytes());
        }
        for (_, offset) in &entries {
            idx.extend_from_slice(&offset.to_be_bytes());
        }
        idx.extend_from_slice(&pack_sha);
        let idx_sha = blaze_git_fetch::sha1::sha1(idx.as_slice());
        idx.extend_from_slice(&idx_sha);
        (pack, idx)
    }

    struct MemGitDb {
        loose: std::collections::BTreeMap<[u8; 20], std::vec::Vec<u8>>,
        packs: std::vec::Vec<std::vec::Vec<u8>>,
        pack_bytes_read: u64,
        loose_hits: u32,
    }

    impl ObjectSource<Alloc<'static>> for MemGitDb {
        fn read_loose(&mut self, oid: &Oid, out: &mut OwnVec<u8, Alloc<'static>>) -> Result<bool, StoreWhy> {
            out.clear();
            let Some(bytes) = self.loose.get(&oid.0) else { return Ok(false) };
            self.loose_hits += 1;
            out.extend_from_slice(bytes);
            Ok(true)
        }

        fn read_pack_range(&mut self, pack_idx: usize, offset: u64, len: u64, out: &mut OwnVec<u8, Alloc<'static>>) -> Result<bool, StoreWhy> {
            out.clear();
            let Some(pack) = self.packs.get(pack_idx) else { return Ok(false) };
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
            let Some(slice) = pack.get(start..end) else { return Ok(false) };
            self.pack_bytes_read += len;
            out.extend_from_slice(slice);
            Ok(true)
        }
    }

    fn git_tree_of(entries: &mut [(&str, u32, Oid)]) -> OwnVec<u8, Alloc<'static>> {
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = OwnVec::new_in(blaze_own::testing::heap().alloc());
        for (name, mode, oid) in entries.iter() {
            let header = alloc::format!("{mode:o} {name}\0");
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(&oid.0);
        }
        out
    }

    fn git_pkt(payload: &[u8], out: &mut OwnVec<u8, Alloc<'static>>) {
        let len = alloc::format!("{:04x}", payload.len() + 4);
        out.extend_from_slice(len.as_bytes());
        out.extend_from_slice(payload);
    }

    fn git_answer_of(commit: Oid, pack: &[u8]) -> OwnVec<u8, Alloc<'static>> {
        let mut out = OwnVec::new_in(blaze_own::testing::heap().alloc());
        git_pkt(b"shallow-info\n", &mut out);
        let shallow = alloc::format!("shallow {commit}\n");
        git_pkt(shallow.as_bytes(), &mut out);
        out.extend_from_slice(b"0001");
        git_pkt(b"packfile\n", &mut out);
        for piece in pack.chunks(4096) {
            let mut line = OwnVec::new_in(blaze_own::testing::heap().alloc());
            line.push(1u8);
            line.extend_from_slice(piece);
            git_pkt(line.as_slice(), &mut out);
        }
        out.extend_from_slice(b"0000");
        out
    }

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
        assert_eq!(
            view.read("src/lib.rs", blaze_own::testing::heap().alloc()).expect("reads src/lib.rs").map(|file| file.bytes.as_slice().to_vec()),
            Some(b"pub fn a() {}\n".to_vec())
        );

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

        assert!(git_db_folder("https://github.com/indexable-inc/tieline", "tieline-21f4a75d056d557d"));
        assert!(git_db_folder("https://github.com/openai-oss-forks/rusty_v8.git", "rusty_v8-2d66e6cfa36ad61f"));
        assert!(git_db_folder("https://github.com/helix-editor/nucleo.git/", "nucleo-425d994cd74b3654"));
        assert!(git_db_folder("https://git.example.com", "_empty-0123456789abcdef"));
        assert!(git_db_folder("https://git.example.com/", "_empty-0123456789abcdef"));
        assert!(!git_db_folder("https://git.example.com", "git.example.com-0123456789abcdef"));
        assert!(!git_db_folder("https://github.com/indexable-inc/tieline", "other-21f4a75d056d557d"));
        assert!(!git_db_folder("https://github.com/indexable-inc/tieline", "tieline-21F4A75D056D557D"));
        assert!(!git_db_folder("https://github.com/indexable-inc/tieline", "tieline-21f4a75d056d557"));
        assert!(!git_db_folder("https://github.com/indexable-inc/tieline", "tieline-21f4a75d056d557d/.."));

        assert!(git_idx_file("pack-deb4c32aa0c0c0d5e3c055beea5b005e51329808.idx"));
        assert!(!git_idx_file("pack-deb4c32aa0c0c0d5e3c055beea5b005e51329808.pack"));
        assert!(!git_idx_file("pack-DEB4C32AA0C0C0D5E3C055BEEA5B005E51329808.idx"));
        assert!(!git_idx_file("tmp_pack_123456.idx"));
        assert!(!git_idx_file("pack-short.idx"));
    }

    /// Piece 14: a git object database (loose objects + multiple indexed packs) adopts the
    /// pinned commit on demand, producing bytes identical to the HTTPS path without reading
    /// unreachable history or reading pack bytes at all when the commit is absent.
    ///
    /// Expected: `test local::tests::a_git_db_on_the_machine_is_adopted_only_by_the_locks_commit ... ok`
    #[test]
    fn a_git_db_on_the_machine_is_adopted_only_by_the_locks_commit() {
        let alloc = blaze_own::testing::heap().alloc();
        let lib = b"pub fn g() -> u32 { 7 }\n";
        let manifest = b"[package]\nname = \"g\"\nversion = \"0.1.0\"\n";
        let big_unreachable = vec![b'x'; 256 * 1024];
        let lib_oid = Oid::of(GitKind::Blob, lib);
        let manifest_oid = Oid::of(GitKind::Blob, manifest);
        let src_tree = git_tree_of(&mut [("lib.rs", 0o100644, lib_oid)]);
        let src_oid = Oid::of(GitKind::Tree, src_tree.as_slice());
        let sub_oid = Oid([0x42; 20]);
        let root_tree = git_tree_of(&mut [
            ("Cargo.toml", 0o100644, manifest_oid),
            ("src", 0o40000, src_oid),
            ("sub", 0o160000, sub_oid),
        ]);
        let root_oid = Oid::of(GitKind::Tree, root_tree.as_slice());
        let commit_bytes = alloc::format!("tree {root_oid}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\ninit\n");
        let commit = Oid::of(GitKind::Commit, commit_bytes.as_bytes());
        let unit = Unit::Git { repo: "https://github.com/example/g.git", commit, packages: 1 };
        let limits = Limits::REAL;

        // Reference HTTPS record from a single pack containing the commit's objects.
        let (full_pack, _) = git_pack_and_idx(&[
            (GitKind::Blob, lib),
            (GitKind::Blob, manifest),
            (GitKind::Tree, src_tree.as_slice()),
            (GitKind::Tree, root_tree.as_slice()),
            (GitKind::Commit, commit_bytes.as_bytes()),
        ]);
        let answer = git_answer_of(commit, full_pack.as_slice());
        let (network, _) = unpack::git_record(commit, answer.as_slice(), 1 << 20, &limits, alloc)
            .expect("the network path's record");

        // Split the repository across loose objects + 2 packs (including a large unreachable blob
        // in pack 0 that must never be read).
        let (pack0, idx0) = git_pack_and_idx(&[
            (GitKind::Blob, big_unreachable.as_slice()),
            (GitKind::Tree, root_tree.as_slice()),
            (GitKind::Blob, manifest),
        ]);
        let (pack1, idx1) = git_pack_and_idx(&[
            (GitKind::Tree, src_tree.as_slice()),
            (GitKind::Blob, lib),
        ]);
        let mut db = MemGitDb {
            loose: std::collections::BTreeMap::from([(commit.0, git_loose(GitKind::Commit, commit_bytes.as_bytes()).as_slice().to_vec())]),
            packs: vec![pack0.as_slice().to_vec(), pack1.as_slice().to_vec()],
            pack_bytes_read: 0,
            loose_hits: 0,
        };
        let pack_tables = [
            (idx0.as_slice(), u64::try_from(pack0.len()).expect("len")),
            (idx1.as_slice(), u64::try_from(pack1.len()).expect("len")),
        ];
        let (record, made) = adopt_git_db(&unit, &pack_tables, &mut db, &limits, alloc)
            .expect("split loose + multi-pack git db is adopted");
        assert_eq!(record.as_slice(), network.as_slice(), "adopted git db record equals network path byte for byte");
        assert_eq!(made.files, 3);
        assert_eq!(db.loose_hits, 1, "pinned commit must be read and verified exactly once");
        assert!(db.pack_bytes_read < 4096, "must not read unreachable pack objects: read {}", db.pack_bytes_read);
        let view = PackView::open_source(&unit.source(), record.as_slice()).expect("opens under git source id");
        assert_eq!(
            view.read("src/lib.rs", alloc).expect("read").map(|file| file.bytes.as_slice().to_vec()),
            Some(lib.to_vec())
        );
        let sub_entry = view.find_entry("sub").expect("submodule entry");
        assert_eq!(sub_entry.mode, crate::Mode::Submodule);
        assert_eq!(sub_entry.submodule_commit(), Some(sub_oid.0));

        // Aggregate object count cap (`PackLimits::most_objects`) is enforced across the ODB walk.
        let mut tight = limits;
        tight.git_pack.most_objects = 3;
        assert!(matches!(
            adopt_git_db(&unit, &pack_tables, &mut db, &tight, alloc),
            Err(AdoptRefused::Unpack(UnpackError::GitStore(StoreWhy::Entry(blaze_git_fetch::PackWhy::ObjectsOver { .. }))))
        ));

        // Missing commit reads ZERO packfile bytes.
        let missing_unit = Unit::Git { repo: "https://github.com/example/g.git", commit: Oid([0x55; 20]), packages: 1 };
        db.pack_bytes_read = 0;
        assert_eq!(
            adopt_git_db(&missing_unit, &pack_tables, &mut db, &limits, alloc).map(|(_, m)| m),
            Err(AdoptRefused::Unpack(UnpackError::NoCommit))
        );
        assert_eq!(db.pack_bytes_read, 0, "missing commit must read zero packfile bytes");

        // Tampered loose object is refused by its SHA-1 check.
        let mut tampered_db = MemGitDb {
            loose: std::collections::BTreeMap::from([(commit.0, git_loose(GitKind::Commit, b"tree 0000000000000000000000000000000000000000\n").as_slice().to_vec())]),
            packs: vec![],
            pack_bytes_read: 0,
            loose_hits: 0,
        };
        assert!(matches!(
            adopt_git_db(&unit, &[], &mut tampered_db, &limits, alloc),
            Err(AdoptRefused::Unpack(UnpackError::GitStore(StoreWhy::Id { .. })))
        ));

        let mut version = OwnString::new_in(alloc);
        version.str("1.0.0");
        let krate = Unit::Crate { package: LockId(0), name: "c", version, sha256: [0; 32] };
        assert_eq!(
            adopt_git_db(&krate, &pack_tables, &mut db, &limits, alloc).map(|(_, m)| m),
            Err(AdoptRefused::NotGit)
        );
    }
}
