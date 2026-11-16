//! The parts of a checkout's `.git` that `log`, `status` and `describe` read (decision D54):
//! the object store (loose objects, version 2 pack indexes and their packs, `alternates`) and
//! the index (`.git/index`, versions 2 to 4). Sans-IO through [`GitFiles`]; the formats are
//! blaze-git-fetch's ([`blaze_git_fetch::store`]), shared with the fetched pack.
//!
//! What is read here is not the script's read list: the fact is the question and its answer's
//! digest. The answer for one commit id never changes (an object is named by its bytes), so
//! recomputing `log -1` costs one read of HEAD and its ref, plus the object only when HEAD moved
//! or the record is new.

use blaze_git_fetch::{
    Kind, Oid,
    store::{self, Base, Commit, Idx},
};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::git::{FileKind, GitFile, GitFiles, Refused, StoreFile};

/// The most bytes of one object, inflated; a larger one is refused.
pub const MOST_OBJECT: u64 = 64 << 20;
/// The most deltas on one object's chain: git's own ceiling for `pack.depth` (4095).
pub const MOST_DEPTH: usize = 4095;
/// The most objects one pack index may list.
pub const MOST_IDX_OBJECTS: u32 = 1 << 28;
/// The most packs, and the most alternates, read.
pub const MOST_PACKS: usize = 1024;
pub const MOST_ALTERNATES: usize = 16;
/// The most index entries, and the most worktree entries walked for untracked files.
pub const MOST_ENTRIES: u32 = 4 << 20;

/// The object folders: the repository's, then each alternate's.
pub(crate) struct Store<'a> {
    dirs: AVec<'a, AString<'a>>,
}

fn join<'a>(base: &str, name: &str, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::from_str_in(base, alloc);
    if !base.is_empty() {
        out.push('/');
    }
    out.push_str(name);
    out
}

fn hex_str(oid: &Oid) -> [u8; 40] {
    oid.hex()
}

impl<'a> Store<'a> {
    /// `common/objects` and its `info/alternates` (one level; an alternate with its own
    /// alternates is refused rather than followed).
    pub(crate) fn open(common: &str, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<Store<'a>, Refused> {
        let objects = join(common, "objects", alloc);
        let mut dirs = Vec::new_in(alloc);
        let mut bytes = Vec::new_in(alloc);
        if files.read(&join(&objects, "info/alternates", alloc), &mut bytes) {
            let text = core::str::from_utf8(&bytes).map_err(|_| Refused::Malformed(GitFile::Alternates))?;
            for line in text.lines() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if dirs.len() == MOST_ALTERNATES {
                    return Err(Refused::Malformed(GitFile::Alternates));
                }
                let dir = crate::git::locate(&objects, line, GitFile::Alternates, files, alloc)?;
                let mut nested = Vec::new_in(alloc);
                if files.read(&join(&dir, "info/alternates", alloc), &mut nested) {
                    return Err(Refused::NestedAlternates);
                }
                dirs.push(dir);
            }
        }
        dirs.insert(0, objects);
        for dir in dirs.iter() {
            if files.stat(&join(dir, "pack/multi-pack-index", alloc)).is_some() {
                return Err(Refused::MultiPackIndex);
            }
        }
        Ok(Store { dirs })
    }

    /// The `pack-<id>` names under each folder, sorted.
    fn packs(&self, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<AVec<'a, (usize, Oid)>, Refused> {
        let mut out = Vec::new_in(alloc);
        for (at, dir) in self.dirs.iter().enumerate() {
            let mut names = Vec::new_in(alloc);
            files.list(&join(dir, "pack", alloc), alloc, &mut names);
            for name in names.iter() {
                let Some(id) = name.strip_prefix("pack-").and_then(|rest| rest.strip_suffix(".idx")) else { continue };
                let id = Oid::parse_hex(id.as_bytes()).ok_or(Refused::Malformed(GitFile::PackName))?;
                if out.len() == MOST_PACKS {
                    return Err(Refused::TooManyPacks);
                }
                out.push((at, id));
            }
        }
        Ok(out)
    }

    fn pack_path(&self, dir: usize, id: &Oid, ext: &str, alloc: Alloc<'a>) -> AString<'a> {
        let mut out = join(&self.dirs[dir], "pack/pack-", alloc);
        out.push_str(core::str::from_utf8(&hex_str(id)).expect("hex"));
        out.push_str(ext);
        out
    }

    /// The object `oid`'s body into `out`, and its kind: loose first, then each pack. The bytes
    /// are checked to hash to `oid`.
    pub(crate) fn read(&self, oid: &Oid, files: &mut impl GitFiles, alloc: Alloc<'a>, out: &mut AVec<'a, u8>) -> Result<Kind, Refused> {
        out.clear();
        let hex = hex_str(oid);
        let hex = core::str::from_utf8(&hex).expect("hex");
        let mut file = Vec::new_in(alloc);
        for dir in self.dirs.iter() {
            let mut path = join(dir, &hex[..2], alloc);
            path.push('/');
            path.push_str(&hex[2..]);
            if files.read(&path, &mut file) {
                let mut whole = Vec::new_in(alloc);
                let (kind, body) = store::loose(&file, oid, MOST_OBJECT, alloc, &mut whole)
                    .map_err(|(at, why)| Refused::Store { file: StoreFile::Loose(*oid), at, why })?;
                out.extend_from_slice(&whole[body..]);
                return Ok(kind);
            }
        }
        for (dir, pack) in self.packs(files, alloc)?.iter() {
            file.clear();
            if !files.read(&self.pack_path(*dir, pack, ".idx", alloc), &mut file) {
                return Err(Refused::Store { file: StoreFile::Idx(*pack), at: 0, why: store::StoreWhy::IdxShort });
            }
            let idx = Idx::parse(&file, MOST_IDX_OBJECTS).map_err(|(at, why)| Refused::Store { file: StoreFile::Idx(*pack), at, why })?;
            let Ok(found) = idx.search(oid) else { continue };
            let pack_path = self.pack_path(*dir, pack, ".pack", alloc);
            return read_packed(&idx, found, *pack, &pack_path, oid, files, alloc, out);
        }
        Err(Refused::ObjectMissing(*oid))
    }

    /// git's abbreviation of `oid` (`find_unique_abbrev` in git 2.51's object-name.c): at least
    /// `ceil((msb(n) + 1) / 2)` and at least 7 hex digits, `n` the objects of every pack index,
    /// then lengthened until no other object (packed or loose) shares the prefix.
    pub(crate) fn abbrev(&self, oid: &Oid, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<usize, Refused> {
        let mut count: u64 = 0;
        let mut shared = 0;
        let mut file = Vec::new_in(alloc);
        for (dir, pack) in self.packs(files, alloc)?.iter() {
            file.clear();
            if !files.read(&self.pack_path(*dir, pack, ".idx", alloc), &mut file) {
                return Err(Refused::Store { file: StoreFile::Idx(*pack), at: 0, why: store::StoreWhy::IdxShort });
            }
            let idx = Idx::parse(&file, MOST_IDX_OBJECTS).map_err(|(at, why)| Refused::Store { file: StoreFile::Idx(*pack), at, why })?;
            count += u64::from(idx.len());
            shared = shared.max(idx.shared_hex(oid));
        }
        let hex = hex_str(oid);
        let hex_text = core::str::from_utf8(&hex).expect("hex");
        for dir in self.dirs.iter() {
            let mut names = Vec::new_in(alloc);
            files.list(&join(dir, &hex_text[..2], alloc), alloc, &mut names);
            for name in names.iter() {
                if name.len() == 38 && name.as_bytes() != &hex[2..] {
                    let same = name.bytes().zip(hex[2..].iter()).take_while(|(a, b)| a == *b).count();
                    shared = shared.max(2 + same);
                }
            }
        }
        let msb = if count == 0 { 0 } else { 63 - count.leading_zeros() as usize };
        let least = (msb + 2) / 2;
        Ok(least.max(7).max(shared + 1).min(40))
    }
}

/// The packed object at `found` in `idx`: its chain of deltas walked back to a whole object
/// (each step a ranged read of the entry), then applied forward. Iterative, capped at
/// [`MOST_DEPTH`].
#[allow(clippy::too_many_arguments)]
fn read_packed<'a>(idx: &Idx<'_>, found: u32, pack: Oid, path: &str, oid: &Oid, files: &mut impl GitFiles, alloc: Alloc<'a>, out: &mut AVec<'a, u8>) -> Result<Kind, Refused> {
    let refuse = |at: u64, why| Refused::Store { file: StoreFile::Pack(pack), at, why };
    let idx_refuse = |(at, why)| Refused::Store { file: StoreFile::Idx(pack), at, why };
    let size = match files.stat(path) {
        Some(stat) if stat.kind == FileKind::File && stat.size >= 32 => stat.size,
        _ => return Err(refuse(0, store::StoreWhy::Entry(blaze_git_fetch::PackWhy::Short))),
    };
    let pack_end = size - 20;
    let mut window = Vec::new_in(alloc);
    let mut read_entry = |at: u64, window: &mut AVec<'a, u8>| -> Result<store::EntryHead, Refused> {
        let end = idx.entry_end(at, pack_end).map_err(idx_refuse)?;
        if end - at > MOST_OBJECT * 2 + 64 {
            return Err(refuse(at, store::StoreWhy::Entry(blaze_git_fetch::PackWhy::ObjectOver { size: end - at, most: MOST_OBJECT * 2 + 64 })));
        }
        window.clear();
        if !files.read_range(path, at, end - at, window) {
            return Err(refuse(at, store::StoreWhy::Entry(blaze_git_fetch::PackWhy::Header)));
        }
        store::entry_head(window).map_err(|why| refuse(at, why))
    };
    let mut chain: AVec<'a, u64> = Vec::new_in(alloc);
    let mut at = idx.offset(found, pack_end).map_err(idx_refuse)?;
    let kind = loop {
        if chain.len() > MOST_DEPTH {
            return Err(Refused::DepthOver { pack });
        }
        let head = read_entry(at, &mut window)?;
        match head.base {
            Base::Whole(kind) => {
                store::inflate_entry(&window, &head, MOST_OBJECT, out).map_err(|why| refuse(at, why))?;
                break kind;
            }
            Base::Ofs { back } => {
                chain.push(at);
                at = match at.checked_sub(back) {
                    Some(base) if base >= 12 => base,
                    _ => return Err(refuse(at, store::StoreWhy::Entry(blaze_git_fetch::PackWhy::BaseOffset { back }))),
                };
            }
            Base::Ref { base } => {
                chain.push(at);
                let Ok(base_at) = idx.search(&base) else { return Err(refuse(at, store::StoreWhy::Entry(blaze_git_fetch::PackWhy::MissingBase { base }))) };
                at = idx.offset(base_at, pack_end).map_err(idx_refuse)?;
            }
        }
    };
    let mut delta = Vec::new_in(alloc);
    let mut next = Vec::new_in(alloc);
    while let Some(delta_at) = chain.pop() {
        let head = read_entry(delta_at, &mut window)?;
        store::inflate_entry(&window, &head, MOST_OBJECT, &mut delta).map_err(|why| refuse(delta_at, why))?;
        store::apply_delta(out, &delta, MOST_OBJECT, &mut next).map_err(|why| refuse(delta_at, why))?;
        core::mem::swap(out, &mut next);
    }
    if Oid::of(kind, out) != *oid && !planted(Plant::NoObjectHashCheck) {
        return Err(refuse(0, store::StoreWhy::Id { want: *oid }));
    }
    Ok(kind)
}

/// The commit `oid`, read.
pub(crate) fn read_commit<'a>(store: &Store<'a>, oid: &Oid, files: &mut impl GitFiles, alloc: Alloc<'a>, out: &mut AVec<'a, u8>) -> Result<(), Refused> {
    match store.read(oid, files, alloc, out)? {
        Kind::Commit => Ok(()),
        _ => Err(Refused::NotCommit(*oid)),
    }
}

/// Days since 1970-01-01 as (year, month, day) in the proleptic Gregorian calendar (the
/// civil-from-days algorithm), as git's `gmtime` gives them.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn digits(out: &mut AString<'_>, value: u64, width: usize) {
    let mut buf = [b'0'; 20];
    let mut at = buf.len();
    let mut rest = value;
    loop {
        at -= 1;
        buf[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for _ in (buf.len() - at)..width {
        out.push('0');
    }
    out.push_str(core::str::from_utf8(&buf[at..]).expect("digits"));
}

/// One `--pretty=format:` placeholder of the commit, appended to `out`, as git 2.51 prints it:
/// - `%cI`: committer date, strict ISO 8601 (pretty-formats: "committer date, strict ISO 8601
///   format"): `YYYY-MM-DDTHH:MM:SS` in the commit's own zone, then `Z` when the zone is
///   `+0000`/`-0000`, else `+HH:MM`/`-HH:MM` (date.c, `DATE_ISO8601_STRICT`);
/// - `%cs`: committer date, `YYYY-MM-DD` in the commit's zone;
/// - `%ct`: committer date, seconds since the epoch;
/// - `%H`: the commit's full id.
pub(crate) fn placeholder(out: &mut AString<'_>, which: crate::git::Placeholder, commit: &Commit<'_>, oid: &Oid) -> Result<(), Refused> {
    use crate::git::Placeholder;
    let zone_minutes = i64::from(commit.zone / 100) * 60 + i64::from(commit.zone % 100);
    let local = commit.committed + zone_minutes * 60;
    let (year, month, day) = civil(local.div_euclid(86_400));
    if !(0..=9999).contains(&year) {
        return Err(Refused::DateOutOfRange);
    }
    let seconds = local.rem_euclid(86_400) as u64;
    let date = |out: &mut AString<'_>| {
        digits(out, year as u64, 4);
        out.push('-');
        digits(out, u64::from(month), 2);
        out.push('-');
        digits(out, u64::from(day), 2);
    };
    match which {
        Placeholder::CommitterIso => {
            date(out);
            out.push('T');
            digits(out, seconds / 3600, 2);
            out.push(':');
            digits(out, seconds / 60 % 60, 2);
            out.push(':');
            digits(out, seconds % 60, 2);
            if commit.zone == 0 {
                out.push('Z');
            } else {
                out.push(if commit.zone > 0 { '+' } else { '-' });
                let zone = commit.zone.unsigned_abs();
                digits(out, u64::from(zone / 100), 2);
                out.push(':');
                digits(out, u64::from(zone % 100), 2);
            }
        }
        Placeholder::CommitterShort => date(out),
        Placeholder::CommitterSeconds => digits(out, commit.committed as u64, 1),
        Placeholder::Hash => out.push_str(core::str::from_utf8(&oid.hex()).expect("hex")),
    }
    Ok(())
}

/// A bug planted on purpose, so the property tests' controls can show their oracles catch it
/// (rule dst-not-review). Only a test build can plant one; elsewhere [`planted`] is `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    #[cfg(test)]
    None,
    /// An object read back is returned without its SHA-1 checked against its id.
    NoObjectHashCheck,
    /// An index entry's fixed 62 bytes are read before their length is checked.
    IndexEntryLengthUnchecked,
}

#[cfg(test)]
thread_local! {
    /// The bug planted in this thread's reads; set by a control, `None` otherwise.
    pub(crate) static PLANT: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.with(|it| it.get() == plant)
}

#[cfg(not(test))]
#[inline(always)]
const fn planted(_: Plant) -> bool {
    false
}

/// One entry of `.git/index` that `status` compares.
pub(crate) struct Entry<'i> {
    pub path: AString<'i>,
    pub ctime: (u32, u32),
    pub mtime: (u32, u32),
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub id: Oid,
    pub assume_valid: bool,
}

/// `.git/index` read: its entries in order, and the cache tree's root when valid.
pub(crate) struct Index<'i> {
    pub entries: AVec<'i, Entry<'i>>,
    pub root_tree: Option<Oid>,
}

fn be(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// `.git/index`, versions 2 to 4 (git's index-format documentation): the header `DIRC`, the
/// version, the count; each entry's 62 bytes of stat data, id and flags (and two more flags
/// when bit 14 is set, version 3 on); its path NUL-terminated and padded to 8 bytes (2, 3), or
/// in version 4 a varint of bytes to drop from the previous path then the rest NUL-terminated.
/// Extensions follow, each a 4-byte signature and a 4-byte size; `TREE` gives the cache tree's
/// root, the others are skipped by their size, except `link` (a split index) and `sdir` (a
/// sparse index), refused. The trailing checksum is not verified per read: git verifies it
/// itself only on `fsck`, and a hostile index can lie in its entries just as well.
pub(crate) fn parse_index<'i>(bytes: &[u8], alloc: Alloc<'i>) -> Result<Index<'i>, Refused> {
    let bad = |at: usize| Refused::IndexMalformed { at: at as u64 };
    if bytes.len() < 12 + 20 || &bytes[..4] != b"DIRC" {
        return Err(bad(0));
    }
    let version = be(bytes, 4).ok_or(bad(4))?;
    if !(2..=4).contains(&version) {
        return Err(bad(4));
    }
    let count = be(bytes, 8).ok_or(bad(8))?;
    if count > MOST_ENTRIES {
        return Err(Refused::TooManyEntries);
    }
    let end = bytes.len() - 20;
    let mut entries = Vec::new_in(alloc);
    let mut at = 12;
    let mut previous: AVec<'i, u8> = Vec::new_in(alloc);
    for _ in 0..count {
        let start = at;
        let field = |n: usize| be(bytes, start + n * 4).ok_or(bad(start));
        if start + 62 > end && !planted(Plant::IndexEntryLengthUnchecked) {
            return Err(bad(start));
        }
        let id = Oid(bytes[start + 40..start + 60].try_into().expect("20 bytes"));
        let flags = u16::from_be_bytes([bytes[start + 60], bytes[start + 61]]);
        at = start + 62;
        if flags & 0x4000 != 0 {
            if version < 3 || at + 2 > end {
                return Err(bad(start));
            }
            let extended = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
            if extended & 0x4000 != 0 {
                return Err(Refused::SparseCheckout);
            }
            at += 2;
        }
        if flags & 0x3000 != 0 {
            return Err(Refused::Conflict);
        }
        if version == 4 {
            let mut pos = at;
            let mut drop = 0u64;
            let mut byte = *bytes.get(pos).filter(|_| pos < end).ok_or(bad(pos))?;
            pos += 1;
            drop |= u64::from(byte & 0x7f);
            while byte & 0x80 != 0 {
                byte = *bytes.get(pos).filter(|_| pos < end).ok_or(bad(pos))?;
                pos += 1;
                drop = drop.checked_add(1).and_then(|d| d.checked_mul(128)).ok_or(bad(pos))? | u64::from(byte & 0x7f);
            }
            if drop > previous.len() as u64 {
                return Err(bad(at));
            }
            previous.truncate(previous.len() - drop as usize);
            let nul = bytes[pos..end].iter().position(|b| *b == 0).ok_or(bad(pos))?;
            previous.extend_from_slice(&bytes[pos..pos + nul]);
            at = pos + nul + 1;
        } else {
            let nul = bytes[at..end].iter().position(|b| *b == 0).ok_or(bad(at))?;
            previous.clear();
            previous.extend_from_slice(&bytes[at..at + nul]);
            let length = at + nul - start;
            at = start + (length + 8) / 8 * 8;
            if at > end {
                return Err(bad(start));
            }
        }
        let path = core::str::from_utf8(&previous).map_err(|_| Refused::QuotedPath)?;
        if !index_path_ok(path) {
            return Err(Refused::IndexPath { at: start as u64 });
        }
        entries.push(Entry {
            path: String::from_str_in(path, alloc),
            ctime: (field(0)?, field(1)?),
            mtime: (field(2)?, field(3)?),
            ino: field(5)?,
            mode: field(6)?,
            uid: field(7)?,
            gid: field(8)?,
            size: field(9)?,
            id,
            assume_valid: flags & 0x8000 != 0,
        });
    }
    let mut root_tree = None;
    while at < end {
        if at + 8 > end {
            return Err(bad(at));
        }
        let signature = &bytes[at..at + 4];
        let size = be(bytes, at + 4).ok_or(bad(at))? as usize;
        let data = bytes.get(at + 8..at + 8 + size).filter(|_| at + 8 + size <= end).ok_or(bad(at))?;
        match signature {
            b"link" | b"sdir" => return Err(Refused::SparseCheckout),
            b"TREE" => {
                // The root: an empty path, its entry count (-1 when invalid), a space, its
                // subtree count, a newline, then its id when valid.
                let line_end = data.iter().position(|b| *b == b'\n').ok_or(bad(at + 8))?;
                let head = core::str::from_utf8(&data[..line_end]).map_err(|_| bad(at + 8))?;
                let counts = head.strip_prefix('\0').ok_or(bad(at + 8))?;
                let (entry_count, _) = counts.split_once(' ').ok_or(bad(at + 8))?;
                if !entry_count.starts_with('-') {
                    let id = data.get(line_end + 1..line_end + 21).ok_or(bad(at + 8))?;
                    root_tree = Some(Oid(id.try_into().expect("20 bytes")));
                }
            }
            _ => {}
        }
        at += 8 + size;
    }
    Ok(Index { entries, root_tree })
}

/// Whether a repository's `config` can change what `status` reports in a way blaze does not
/// model: `filemode` set false (git then ignores the executable bit, blaze compares it), or an
/// `[include]`/`[includeIf]` section (another file, unread). Read line by line as git's config
/// syntax has it (`key = value`, keys case-insensitive; booleans `false`, `no`, `off`, `0`).
/// Other keys only make git hash more (`trustctime`, `checkStat`) or matter only for a CR,
/// which is refused anyway.
fn config_changes_status(config: &[u8]) -> bool {
    config.split(|b| *b == b'\n').any(|line| {
        let line = line.trim_ascii();
        if line.first() == Some(&b'[') {
            return line.len() >= 8 && line[1..8].eq_ignore_ascii_case(b"include");
        }
        let Some(eq) = line.iter().position(|b| *b == b'=') else { return false };
        let value = line[eq + 1..].trim_ascii();
        line[..eq].trim_ascii().eq_ignore_ascii_case(b"filemode") && [&b"false"[..], b"no", b"off", b"0"].iter().any(|v| value.eq_ignore_ascii_case(v))
    })
}

/// Whether an index entry's path is one git itself would write (git 2.51 read-cache.c
/// `verify_path`): relative, no empty, `.` or `..` component, and no `.git` component in any
/// case. Anything else would make `status` stat a file outside the worktree or inside `.git`.
fn index_path_ok(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git"))
}

/// Whether an attributes file can make the bytes `status` hashes differ from the file's own:
/// it names a filter, `ident`, an encoding or an end-of-line rule. blaze applies no attributes,
/// so a file it must hash under such rules is refused rather than guessed. (`text=auto` alone
/// changes only CRLF, so a file with no CR hashes as its raw bytes.)
fn attributes_change_bytes(attributes: &[u8]) -> bool {
    [&b"filter="[..], b"ident", b"working-tree-encoding", b"eol=", b"crlf"].iter().any(|word| attributes.windows(word.len()).any(|window| window == *word))
}

/// A path as `status --porcelain` prints it, when git would print it unquoted (no control
/// byte, `"`, `\` or byte past ASCII: `core.quotePath`'s default).
fn plain(path: &str) -> Result<&str, Refused> {
    if path.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\') { Ok(path) } else { Err(Refused::QuotedPath) }
}

/// `status --porcelain [--untracked-files=no]` of the worktree at `work` whose git folder is
/// `gitdir`, HEAD's tree being `head_tree`: one line per changed tracked file in index order
/// (` M` modified, ` D` deleted), then, when `untracked`, a refusal if any file is neither
/// tracked nor provably ignored (blaze has no ignore rules: D54).
///
/// A tracked file is clean when its stat (size, mtime, ctime, inode, device, owner) equals the
/// entry's and its mtime is older than the index file's (git's racy-git rule: an entry written
/// in the same instant as the index may hide a later change of the same size, so git hashes
/// it); else when its bytes hash as a blob to the entry's id. Staged changes: the index's cache
/// tree root must equal HEAD's tree, else refused (walking HEAD's tree is not built).
/// Cost: one index read, one stat per entry, a hash per file whose stat differs; with
/// `untracked`, a listing of every folder and a stat per name. Floor: the stats.
pub(crate) fn status<'a>(gitdir: &str, common: &str, work: &str, untracked: bool, head_tree: Oid, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<AString<'a>, Refused> {
    let index_path = join(gitdir, "index", alloc);
    let mut bytes = Vec::new_in(alloc);
    if !files.read(&index_path, &mut bytes) {
        return Err(Refused::Malformed(GitFile::Index));
    }
    let index_time = files.stat(&index_path).ok_or(Refused::Malformed(GitFile::Index))?.mtime;
    let index = parse_index(&bytes, alloc)?;
    if index.root_tree != Some(head_tree) {
        return Err(Refused::StagedUnproven);
    }
    let mut config = Vec::new_in(alloc);
    if files.read(&join(common, "config", alloc), &mut config) && config_changes_status(&config) {
        return Err(Refused::ConfigNotRead);
    }
    let mut attributes = Vec::new_in(alloc);
    let mut risky = false;
    for entry in index.entries.iter() {
        if entry.path.as_str() == ".gitattributes" || entry.path.ends_with("/.gitattributes") {
            attributes.clear();
            risky |= files.read(&join(work, &entry.path, alloc), &mut attributes) && attributes_change_bytes(&attributes);
        }
    }
    attributes.clear();
    risky |= files.read(&join(common, "info/attributes", alloc), &mut attributes) && attributes_change_bytes(&attributes);
    let mut out = String::new_in(alloc);
    let mut content = Vec::new_in(alloc);
    for entry in index.entries.iter() {
        let path = join(work, &entry.path, alloc);
        let stat = files.stat(&path);
        match entry.mode & 0o170000 {
            0o160000 => {
                // A submodule: clean only when not checked out (an empty folder or none).
                let mut names = Vec::new_in(alloc);
                files.list(&path, alloc, &mut names);
                if !names.is_empty() {
                    return Err(Refused::Submodule);
                }
                continue;
            }
            0o100000 | 0o120000 => {}
            _ => return Err(Refused::IndexMalformed { at: 0 }),
        }
        let Some(stat) = stat else {
            out.push_str(" D ");
            out.push_str(plain(&entry.path)?);
            out.push('\n');
            continue;
        };
        let want_kind = if entry.mode & 0o170000 == 0o120000 { FileKind::Symlink } else { FileKind::File };
        if stat.kind != want_kind {
            return Err(Refused::TypeChanged);
        }
        if entry.assume_valid {
            continue;
        }
        let executable = entry.mode & 0o111 != 0;
        let mode_differs = want_kind == FileKind::File && stat.executable != executable;
        let same_stat = u64::from(entry.size) == stat.size & 0xffff_ffff
            && entry.mtime == (stat.mtime.0 as u32, stat.mtime.1)
            && entry.ctime == (stat.ctime.0 as u32, stat.ctime.1)
            && entry.ino == stat.ino as u32
            && entry.uid == stat.uid
            && entry.gid == stat.gid;
        let racy = i64::from(entry.mtime.0) >= index_time.0;
        if !mode_differs && same_stat && !racy {
            continue;
        }
        let changed = if mode_differs {
            true
        } else {
            if want_kind == FileKind::Symlink {
                return Err(Refused::SymlinkNotBuilt);
            }
            content.clear();
            if !files.read(&path, &mut content) {
                return Err(Refused::TypeChanged);
            }
            if risky || content.contains(&b'\r') {
                return Err(Refused::AttributesNotApplied);
            }
            Oid::of(Kind::Blob, &content) != entry.id
        };
        if changed {
            out.push_str(" M ");
            out.push_str(plain(&entry.path)?);
            out.push('\n');
        }
    }
    if untracked {
        untracked_walk(work, &index, files, alloc)?;
    }
    Ok(out)
}

/// Every file under `work` (but `.git`) must be tracked: an untracked one would be listed by
/// git unless an ignore rule covers it, and blaze has no ignore rules, so it is refused.
/// Iterative, at most [`MOST_ENTRIES`] names.
fn untracked_walk(work: &str, index: &Index<'_>, files: &mut impl GitFiles, alloc: Alloc<'_>) -> Result<(), Refused> {
    let mut pending: AVec<'_, AString<'_>> = Vec::new_in(alloc);
    pending.push(String::new_in(alloc));
    let mut seen = 0u32;
    while let Some(rel) = pending.pop() {
        let mut names = Vec::new_in(alloc);
        files.list(&join(work, &rel, alloc), alloc, &mut names);
        for name in names.iter() {
            seen += 1;
            if seen > MOST_ENTRIES {
                return Err(Refused::TooManyEntries);
            }
            if rel.is_empty() && name.as_str() == ".git" {
                continue;
            }
            let child = join(&rel, name, alloc);
            let tracked = index.entries.binary_search_by(|entry| entry.path.as_bytes().cmp(child.as_bytes())).is_ok();
            if tracked {
                continue;
            }
            match files.stat(&join(work, &child, alloc)).map(|stat| stat.kind) {
                Some(FileKind::Dir) => {
                    let mut prefix = String::from_str_in(&child, alloc);
                    prefix.push('/');
                    let at = index.entries.partition_point(|entry| entry.path.as_bytes() < prefix.as_bytes());
                    let has_tracked = index.entries.get(at).is_some_and(|entry| entry.path.starts_with(prefix.as_str()));
                    if !has_tracked {
                        // A folder with no tracked file: git lists it as `?? dir/` if it holds any file.
                        let mut inner = Vec::new_in(alloc);
                        files.list(&join(work, &child, alloc), alloc, &mut inner);
                        if !inner.is_empty() {
                            return Err(Refused::Untracked);
                        }
                        continue;
                    }
                    pending.push(child);
                }
                Some(_) => return Err(Refused::Untracked),
                None => {}
            }
        }
    }
    Ok(())
}

/// Whether the repository has any tag: a name under `refs/tags/` or a `refs/tags/` line of
/// `packed-refs`.
pub(crate) fn has_tags(common: &str, files: &mut impl GitFiles, alloc: Alloc<'_>) -> Result<bool, Refused> {
    let mut names = Vec::new_in(alloc);
    files.list(&join(common, "refs/tags", alloc), alloc, &mut names);
    if !names.is_empty() {
        return Ok(true);
    }
    let mut packed = Vec::new_in(alloc);
    if files.read(&join(common, "packed-refs", alloc), &mut packed) {
        let text = core::str::from_utf8(&packed).map_err(|_| Refused::Malformed(GitFile::PackedRefs))?;
        return Ok(text.lines().any(|line| line.split_once(' ').is_some_and(|(_, name)| name.starts_with("refs/tags/"))));
    }
    Ok(false)
}
