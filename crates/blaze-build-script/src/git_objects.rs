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

use crate::git::{FileKind, GitFile, GitFiles, LinkUnread, Refused, StoreFile};

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
    if !base.is_empty() && !name.is_empty() {
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
/// Other keys only make git hash more (`trustctime`, `checkStat`), matter only for a CR in a
/// file no `text` attribute covers (`core.autocrlf`; refused), or not at all on clean once
/// `text=auto` is in force (`core.eol`, `core.autocrlf`, `core.safecrlf`: see git_attributes).
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

/// A path as `status --porcelain` prints it, when git would print it unquoted (no control
/// byte, `"`, `\` or byte past ASCII: `core.quotePath`'s default).
fn plain(path: &str) -> Result<&str, Refused> {
    if path.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\') { Ok(path) } else { Err(Refused::QuotedPath) }
}

/// `status --porcelain [--untracked-files=no]` of the worktree at `work` whose git folder is
/// `gitdir`, HEAD's tree being `head_tree`: one line per changed tracked file in index order
/// (` M` modified, ` D` deleted), then, when `untracked`, the untracked files that no checkout
/// ignore rule covers. `all` keeps every file below an untracked directory; otherwise Git's
/// default `?? dir/` collapse is used. Each ignore file is read through [`GitFiles`], so editing
/// one re-asks the status fact. The machine's global ignore files are deliberately not read
/// (D54 and rule 25).
///
/// A tracked file is clean when its stat (size, mtime, ctime, inode, device, owner) equals the
/// entry's and its mtime is older than the index file's (git's racy-git rule: an entry written
/// in the same instant as the index may hide a later change of the same size, so git hashes
/// it); else when its bytes hash as a blob to the entry's id (a symlink's bytes are its target
/// text, read and never followed: git's `ce_compare_link`). Staged changes: the index's cache
/// tree root must equal HEAD's tree, else refused (walking HEAD's tree is not built).
/// Cost: one index read, one stat per entry, a hash per file whose stat differs; with
/// `untracked`, a listing of every folder and a stat per name. Floor: the stats.
pub(crate) fn status<'a>(gitdir: &str, common: &str, work: &str, untracked: bool, all: bool, head_tree: Oid, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<AString<'a>, Refused> {
    Ok(whole(gitdir, common, work, untracked, all, head_tree, files, alloc)?.text)
}

/// [`status`] whole, with what a record keeps of it for [`status_after`].
#[allow(clippy::too_many_arguments, reason = "the repository's three folders, the question, HEAD's tree, the reader and the allocator")]
pub(crate) fn whole<'a>(gitdir: &str, common: &str, work: &str, untracked: bool, all: bool, head_tree: Oid, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<Computed<'a>, Refused> {
    let prepared = prepare(gitdir, common, work, head_tree, files, alloc)?;
    let mut store: Option<Store<'a>> = None;
    let mut out = String::new_in(alloc);
    for entry in prepared.index.entries.iter() {
        tracked_line(entry, work, common, &prepared, files, &mut store, alloc, &mut out)?;
    }
    if untracked {
        let walk = Walk { work, gitdir, index: &prepared.index, all, alloc };
        let mut ignores = crate::git_ignore::Ignores::new(alloc);
        let mut found: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        let mut seen = 0u32;
        walk.untracked("", false, files, &mut ignores, &mut found, &mut seen)?;
        found.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        for path in found.iter() {
            push_line(&mut out, "?? ", path)?;
        }
    }
    Ok(Computed { text: out, text_auto: prepared.text_auto })
}

/// A status computed: its porcelain text, and whether `* text=auto` was in force.
pub(crate) struct Computed<'a> {
    pub(crate) text: AString<'a>,
    pub(crate) text_auto: bool,
}

/// What a status reads before its first entry.
struct Prepared<'a> {
    index: Index<'a>,
    /// The index file's mtime: git's racy rule compares each entry's with it.
    index_time: (i64, u32),
    text_auto: bool,
}

/// `prefix path\n` appended to `out` (the path plain, else refused by name).
fn push_line(out: &mut AString<'_>, prefix: &str, path: &str) -> Result<(), Refused> {
    out.push_str(prefix);
    out.push_str(plain(path)?);
    out.push('\n');
    Ok(())
}

/// The index read (its cache tree proven HEAD's), the config's check, and the attributes'
/// `text=auto`: what [`status`] reads before its first entry.
fn prepare<'a>(gitdir: &str, common: &str, work: &str, head_tree: Oid, files: &mut impl GitFiles, alloc: Alloc<'a>) -> Result<Prepared<'a>, Refused> {
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
    // Every attributes file is read; any byte-changing form but `* text=auto` at the top is
    // refused by name, so `text_auto` is the one rule in force, on every path or on none.
    let mut attributes = Vec::new_in(alloc);
    // The top files first, so that a nested file is read knowing whether `text=auto` is already
    // on every path. (The index's byte order puts `.config/.gitattributes` before the top
    // `.gitattributes`, so one pass would not do.)
    use crate::git_attributes::{AttributesFile, sets_text_auto};
    let mut text_auto = false;
    if index.entries.iter().any(|entry| entry.path.as_str() == ".gitattributes") && files.read(&join(work, ".gitattributes", alloc), &mut attributes) {
        text_auto |= sets_text_auto(&attributes, AttributesFile::Top)?;
    }
    attributes.clear();
    if files.read(&join(common, "info/attributes", alloc), &mut attributes) {
        text_auto |= sets_text_auto(&attributes, AttributesFile::Top)?;
    }
    let nested = AttributesFile::Nested { top_sets_auto: text_auto };
    for entry in index.entries.iter() {
        if entry.path.ends_with("/.gitattributes") {
            attributes.clear();
            if files.read(&join(work, &entry.path, alloc), &mut attributes) {
                // A nested file adds no path the top rule does not already cover.
                sets_text_auto(&attributes, nested)?;
            }
        }
    }
    Ok(Prepared { index, index_time, text_auto })
}

/// One tracked entry's line of the status, appended to `out` (none when it is clean): ` D` for
/// a file gone, ` M` for one whose bytes or mode differ. The object store is opened at the first
/// file whose clean asks its index blob, and kept in `store` for the next.
#[allow(clippy::too_many_arguments, reason = "the entry, the worktree, the store's folder, what was prepared, the reader, the store, the allocator and the output")]
fn tracked_line<'a>(entry: &Entry<'_>, work: &str, common: &str, prepared: &Prepared<'_>, files: &mut impl GitFiles, store: &mut Option<Store<'a>>, alloc: Alloc<'a>, out: &mut AString<'a>) -> Result<(), Refused> {
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
            return Ok(());
        }
        0o100000 | 0o120000 => {}
        _ => return Err(Refused::IndexMalformed { at: 0 }),
    }
    let Some(stat) = stat else {
        return push_line(out, " D ", &entry.path);
    };
    let want_kind = if entry.mode & 0o170000 == 0o120000 { FileKind::Symlink } else { FileKind::File };
    if stat.kind != want_kind {
        return Err(Refused::TypeChanged);
    }
    if entry.assume_valid {
        return Ok(());
    }
    let executable = entry.mode & 0o111 != 0;
    let mode_differs = want_kind == FileKind::File && stat.executable != executable;
    let same_stat = u64::from(entry.size) == stat.size & 0xffff_ffff
        && entry.mtime == (stat.mtime.0 as u32, stat.mtime.1)
        && entry.ctime == (stat.ctime.0 as u32, stat.ctime.1)
        && entry.ino == stat.ino as u32
        && entry.uid == stat.uid
        && entry.gid == stat.gid;
    let racy = i64::from(entry.mtime.0) >= prepared.index_time.0;
    if !mode_differs && same_stat && !racy {
        return Ok(());
    }
    let changed = if mode_differs {
        true
    } else {
        let mut content = Vec::new_in(alloc);
        if want_kind == FileKind::Symlink {
            // A link's blob is its target text (git's `ce_compare_link`); no attribute applies
            // to it, and it is never followed.
            files.read_link(&path, &mut content).map_err(|why| match why {
                LinkUnread::NotALink => Refused::TypeChanged,
                LinkUnread::Unreadable | LinkUnread::ReaderCannot => Refused::LinkUnread(why),
            })?;
        } else {
            if !files.read(&path, &mut content) {
                return Err(Refused::TypeChanged);
            }
            if prepared.text_auto {
                // git convert.c `crlf_to_git` under CRLF_AUTO: a text file with CR LF pairs is
                // converted unless its index blob is text holding CR LF already.
                if crate::git_attributes::auto_may_convert(&content) {
                    let opened = match store.take() {
                        Some(opened) => opened,
                        None => Store::open(common, files, alloc)?,
                    };
                    let mut blob = Vec::new_in(alloc);
                    let kind = opened.read(&entry.id, files, alloc, &mut blob)?;
                    *store = Some(opened);
                    // `has_crlf_in_index` reads a blob; another kind reads as none.
                    if kind != Kind::Blob || !crate::git_attributes::blob_has_crlf(&blob) {
                        // Text under `auto` has no lone CR: every CR starts a CR LF pair.
                        content.retain(|b| *b != b'\r');
                    }
                }
            } else if content.contains(&b'\r') {
                return Err(Refused::AttributesNotApplied);
            }
        }
        Oid::of(Kind::Blob, &content) != entry.id
    };
    if changed {
        push_line(out, " M ", &entry.path)?;
    }
    Ok(())
}

/// Whether `path` is `region` or under it (`""`: everything).
fn within(region: &str, path: &str) -> bool {
    region.is_empty() || path == region || (path.len() > region.len() && path.as_bytes()[region.len()] == b'/' && path.starts_with(region))
}

/// The untracked walk of one status computation: the worktree, the git folder (its
/// `info/exclude`), the index and the mode (`all`: every file; else git's default collapse).
struct Walk<'w, 'a> {
    work: &'w str,
    gitdir: &'w str,
    index: &'w Index<'a>,
    all: bool,
    alloc: Alloc<'a>,
}

impl<'a> Walk<'_, 'a> {
    /// Whether the index tracks `path` itself.
    fn tracked(&self, path: &str) -> bool {
        self.index.entries.binary_search_by(|entry| entry.path.as_bytes().cmp(path.as_bytes())).is_ok()
    }

    /// Whether the index tracks a file under the folder `dir` (`""`: the root).
    fn tracked_below(&self, dir: &str) -> bool {
        if dir.is_empty() {
            return !self.index.entries.is_empty();
        }
        // The paths under `dir/` are together in byte order: the first path not before `dir/`
        // (compared without making it) is under it, or none is.
        let d = dir.as_bytes();
        let at = self.index.entries.partition_point(|entry| {
            let path = entry.path.as_bytes();
            let n = path.len().min(d.len());
            match path[..n].cmp(&d[..n]) {
                core::cmp::Ordering::Less => true,
                core::cmp::Ordering::Greater => false,
                // `path` is `dir` or a prefix of it: before `dir/`.
                core::cmp::Ordering::Equal if path.len() <= d.len() => true,
                core::cmp::Ordering::Equal => path[d.len()] < b'/',
            }
        });
        self.index.entries.get(at).is_some_and(|entry| within(dir, &entry.path) && entry.path.len() > dir.len())
    }

    /// The untracked files under `rel` (`""`: the root), in porcelain form, pushed to `found` in
    /// walk order: a file by its path; in the default mode a folder the index tracks nothing
    /// under, holding at least one file no rule ignores, as `dir/` (git's collapse: the highest
    /// such folder, never one with a tracked file below). With `first`, nothing is pushed and
    /// the walk ends at the first such file: whether there is one. Bounded by [`MOST_ENTRIES`]
    /// names; ignore files are read once each per walk (`ignores`).
    #[allow(clippy::too_many_arguments, reason = "the walk's bounded state")]
    fn untracked(&self, rel: &str, first: bool, files: &mut impl GitFiles, ignores: &mut crate::git_ignore::Ignores<'a>, found: &mut AVec<'a, AString<'a>>, seen: &mut u32) -> Result<bool, Refused> {
        let alloc = self.alloc;
        let mut names = Vec::new_in(alloc);
        files.list(&join(self.work, rel, alloc), alloc, &mut names);
        let mut any = false;
        for name in names.iter() {
            *seen += 1;
            if *seen > MOST_ENTRIES {
                return Err(Refused::TooManyEntries);
            }
            if rel.is_empty() && name.as_str() == ".git" {
                continue;
            }
            let child = join(rel, name, alloc);
            if self.tracked(&child) {
                continue;
            }
            if self.one(&child, first, files, ignores, found, seen)? {
                any = true;
                if first {
                    return Ok(true);
                }
            }
        }
        Ok(any)
    }

    /// One untracked name `child` (not tracked itself): a folder walked (or collapsed), a file
    /// kept unless a rule ignores it. Whether it holds or is an untracked file.
    #[allow(clippy::too_many_arguments, reason = "the walk's bounded state")]
    fn one(&self, child: &str, first: bool, files: &mut impl GitFiles, ignores: &mut crate::git_ignore::Ignores<'a>, found: &mut AVec<'a, AString<'a>>, seen: &mut u32) -> Result<bool, Refused> {
        let alloc = self.alloc;
        match files.stat(&join(self.work, child, alloc)).map(|stat| stat.kind) {
            Some(FileKind::Dir) => {
                if ignores.ignored(self.work, self.gitdir, child, true, files) {
                    return Ok(false);
                }
                if first {
                    return self.untracked(child, true, files, ignores, found, seen);
                }
                if !self.all && !self.tracked_below(child) {
                    let holds = self.untracked(child, true, files, ignores, found, seen)?;
                    if holds {
                        let mut dir = String::from_str_in(child, alloc);
                        dir.push('/');
                        found.push(dir);
                    }
                    return Ok(holds);
                }
                self.untracked(child, false, files, ignores, found, seen)
            }
            Some(_) => {
                if ignores.ignored(self.work, self.gitdir, child, false, files) {
                    return Ok(false);
                }
                if !first {
                    found.push(String::from_str_in(child, alloc));
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

pub use after::{After, CoveredSince, Gen, Made, StatusAnswer, StatusPlant, WholeWhy};
pub(crate) use after::status_after;

/// The status after a change (E3, 2026-11-20): from the previous answer and the journal's
/// changed paths, byte-equal to [`whole`]. Its entry is git.rs `status_after`.
mod after {
    use super::*;

/// A place in a journal of changes: which journal (its identity, as bytes: the volume's event
/// store, a USN journal, the simulation's) and how far into it. Opaque here; blaze-cli
/// journal_sweep.rs makes it from the journal's cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gen {
    pub journal: [u8; 24],
    pub at: u64,
}

impl Gen {
    /// Whether `later` is a place of this journal, at or after this one.
    pub fn reaches(self, later: Gen) -> bool {
        self.journal == later.journal && self.at <= later.at
    }
}

/// Proof that `changed` names every path of the checkout that changed since the journal's place
/// `since` (rule 74): made by the journal's replay alone (blaze-cli journal_sweep.rs, where its
/// `judge` accepted the replay from `since`), its names and its lost subtrees, each relative to
/// the checkout, sorted. Not a boolean: [`status_after`] takes it, and with none computes the
/// status whole, saying so ([`WholeWhy::NotCovered`]).
#[derive(Clone, Copy, Debug)]
pub struct CoveredSince<'c> {
    since: Gen,
    changed: &'c [&'c str],
    /// A folder under which every untracked name is ignored all through the window
    /// ([`CoveredSince::ignoring`]), relative as `changed` is.
    ignored: Option<&'c str>,
}

impl<'c> CoveredSince<'c> {
    /// The replay's proof: from `since`, the paths `changed` (sorted, each once). At git.rs
    /// `status_after` they are relative to the served tree's root; [`status_after`] here takes
    /// them relative to the worktree (the entry rebases them, [`CoveredSince::rebased`]). Only a
    /// journal's replay makes one.
    pub fn replayed(since: Gen, changed: &'c [&'c str]) -> Self {
        CoveredSince { since, changed, ignored: None }
    }

    /// This proof, with `folder` known to ignore every untracked name beneath it from `since` to
    /// the place now: its own ignore file read exactly `*` before the replay, and not named by
    /// it (blaze-cli journal_sweep.rs `CheckoutChanges::ignored_whole` makes the claim; were the
    /// file named, the status is whole, [`WholeWhy::RulesChanged`]). A `*` in a folder's own
    /// ignore file outranks every outer rule for the names in it, and the walk does not enter a
    /// folder so ignored, so no untracked name under `folder` has a line, then or now: a region
    /// whose untracked unit is under it is not walked, and lists and reads nothing. A tracked
    /// name there is the index's, as anywhere.
    pub fn ignoring(self, folder: &'c str) -> Self {
        CoveredSince { ignored: Some(folder), ..self }
    }

    /// This proof rebased to the worktree `work` (git.rs `status_after`): `changed`, the paths
    /// the entry rebased, and the ignored folder rebased the same way, dropped when it is not
    /// beneath the worktree (no name of it is then in the status).
    pub fn rebased<'d>(&self, work: &str, changed: &'d [&'d str]) -> CoveredSince<'d>
    where
        'c: 'd,
    {
        let ignored = self.ignored.and_then(|folder| match work.is_empty() {
            true => Some(folder),
            false => folder.strip_prefix(work).and_then(|rest| rest.strip_prefix('/')),
        });
        CoveredSince { since: self.since, changed, ignored }
    }

    pub fn since(&self) -> Gen {
        self.since
    }

    pub fn changed(&self) -> &'c [&'c str] {
        self.changed
    }

    /// The folder [`CoveredSince::ignoring`] named, if any.
    pub fn ignored(&self) -> Option<&'c str> {
        self.ignored
    }
}

/// A status answer as the next command starts from it ([`status_after`]): two facts that are
/// stored apart (blaze-cli engine_script_reads.rs), never as one record:
/// - what a script observes: the porcelain `text` (byte-equal to the whole computation) for the
///   question `untracked`/`all`, kept as the answer record (the git answer's digest preimage,
///   [`crate::git::Answer::digest_bytes`]) under a key made from the cell's digest;
/// - its coverage: the `text=auto` it was computed under, HEAD's `tree` it was compared with,
///   and the journal's `place` (taken before any file was read), a small record hashed into no
///   cell.
#[derive(Debug, PartialEq, Eq)]
pub struct StatusAnswer<'a> {
    pub text: AString<'a>,
    pub untracked: bool,
    pub all: bool,
    pub text_auto: bool,
    pub tree: Oid,
    pub place: Gen,
}

/// Why a status was computed whole: each case the change-driven answer does not cover, decided
/// before any fact is read and printed in the profile (rule 3: never a quiet fallback).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WholeWhy {
    /// No previous answer: the first command, an older record's form, or its records missing
    /// (a lapsed or never written coverage root).
    NoPrevious,
    /// The previous answer is of another question (`-uno`, `-uall`).
    OtherQuestion,
    /// The journal's replay does not start at the previous answer's place (no replay, a reset,
    /// a lost history, another journal, a replay from a later checkpoint).
    NotCovered,
    /// A path under a git folder changed: the index, HEAD, a ref, the config (a stage, a
    /// commit, a checkout).
    GitFolder,
    /// An ignore or attributes file changed: every path's rule may have moved.
    RulesChanged,
    /// The worktree's folder itself was named (a lost history of all of it).
    WorkNamed,
    /// The git folder is outside the worktree (a linked worktree's, a separate `--git-dir`):
    /// the worktree's journal does not see the index, HEAD or the refs change.
    GitFolderOutside,
}

impl core::fmt::Display for WholeWhy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            WholeWhy::NoPrevious => "no previous answer of this form",
            WholeWhy::OtherQuestion => "the previous answer is of another question",
            WholeWhy::NotCovered => "the journal's replay does not start at the previous answer's place",
            WholeWhy::GitFolder => "a path under a git folder changed (the index, HEAD, a ref, the config)",
            WholeWhy::RulesChanged => "an ignore or attributes file changed",
            WholeWhy::WorkNamed => "the worktree's folder itself was named",
            WholeWhy::GitFolderOutside => "the git folder is outside the worktree, where its journal does not see it",
        })
    }
}

/// How a status after a change was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Made {
    /// From the previous answer: `regions` subtrees named by the journal recomputed.
    Incremental { regions: u32 },
    /// Whole, for the reason given.
    Whole(WholeWhy),
}

/// What [`status_after`] answers: the status now, at the place `covered` reached, and how.
#[derive(Debug)]
pub struct After<'a> {
    pub answer: StatusAnswer<'a>,
    pub made: Made,
}

/// The decision [`status_after`] makes before any fact is read: the regions to recompute (each
/// worktree-relative, none under another, sorted), or why the status is computed whole.
#[allow(clippy::too_many_arguments, reason = "the previous answer, its proof, the question, the repository's folders, the place now, the allocator and the plant")]
fn decide<'a>(
    previous: &StatusAnswer<'_>,
    covered: Option<&CoveredSince<'_>>,
    untracked: bool,
    all: bool,
    gitdir: &str,
    common: &str,
    work: &str,
    now: Gen,
    alloc: Alloc<'a>,
    plant: impl blaze_own::planted::Planted<StatusPlant>,
) -> Result<AVec<'a, AString<'a>>, WholeWhy> {
    if previous.untracked != untracked || (previous.all != all && untracked) {
        return Err(WholeWhy::OtherQuestion);
    }
    let covered = covered.ok_or(WholeWhy::NotCovered)?;
    // The replay must start exactly at the previous answer's place: a later start (a journal
    // checkpoint) hides the changes between, which a certificate proved left the answer's text
    // alone but not its `text=auto` (an attributes edit that changes no line).
    let reached = previous.place == covered.since && covered.since.reaches(now);
    if !reached && !plant.has(StatusPlant::CoverageIgnored) {
        return Err(WholeWhy::NotCovered);
    }
    // The journal is the worktree's: a git folder outside it (a linked worktree's, a separate
    // `--git-dir`) changes unseen.
    let inside = |folder: &str| match work.is_empty() {
        // The reader's namespace is the worktree: a relative folder is in it.
        true => !folder.is_empty() && !folder.starts_with('/') && !folder.starts_with("../") && folder != "..",
        false => folder.len() > work.len() && within(work, folder),
    };
    if !inside(gitdir) || !inside(common) {
        return Err(WholeWhy::GitFolderOutside);
    }
    let mut regions: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for &rel in covered.changed {
        if rel.is_empty() {
            return Err(WholeWhy::WorkNamed);
        }
        let full = join(work, rel, alloc);
        if within(gitdir, &full) || within(common, &full) || rel.split('/').any(|part| part == ".git") {
            return Err(WholeWhy::GitFolder);
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if name == ".gitignore" || name == ".gitattributes" {
            return Err(WholeWhy::RulesChanged);
        }
        regions.push(String::from_str_in(rel, alloc));
    }
    Ok(outermost(regions, alloc))
}

/// `paths` sorted, each kept unless it is under another (`a/c` is under `a`, though `a-b`
/// sorts between them).
fn outermost<'a>(mut paths: AVec<'a, AString<'a>>, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    paths.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut kept: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for path in paths {
        if !kept.iter().any(|outer| within(outer, &path)) {
            kept.push(path);
        }
    }
    kept
}

/// The path of a porcelain line (`XY path`, a collapsed folder with its `/`).
fn line_path(line: &str) -> &str {
    line.get(3..).unwrap_or_default()
}

/// The status of the worktree `work` now, from the `previous` answer and the paths the journal
/// says changed since it (`covered`), sans-IO through `files` (E3, 2026-11-20). Byte-equal to
/// [`whole`]'s text: the previous lines outside every region the journal named are kept, and
/// each region is computed as the whole computation computes it (its tracked entries, and its
/// untracked unit: in the default mode the highest folder above it the index tracks nothing
/// under, so the collapse is the whole's), then the lines are put in the whole's order (tracked
/// by path, untracked by path). Where the change-driven answer is not covered the decision is
/// made before any fact is read, typed ([`WholeWhy`]), and the status computed whole.
///
/// Cost after a change: one index read and its stat; per region one index lookup, a stat per
/// tracked entry in it (a hash where the stat differs), and in an untracked unit the walk of
/// that unit alone; ignore files only for an untracked path, once each. No walk of the tree,
/// and no read of HEAD or the commit: `head_tree` (which may read a pack) is asked only for a
/// whole computation; after a change the previous answer's tree stands, as no path under the
/// git folder changed since it (else the decision is [`WholeWhy::GitFolder`]).
#[allow(clippy::too_many_arguments, reason = "the previous answer, its proof, the question, the repository's folders, HEAD's tree, the place now, the reader and the allocator")]
pub(crate) fn status_after<'a, F: GitFiles>(
    previous: Option<&StatusAnswer<'_>>,
    covered: Option<&CoveredSince<'_>>,
    untracked: bool,
    all: bool,
    gitdir: &str,
    common: &str,
    work: &str,
    head_tree: impl FnOnce(&mut F, Alloc<'a>) -> Result<Oid, Refused>,
    now: Gen,
    files: &mut F,
    alloc: Alloc<'a>,
    plant: impl blaze_own::planted::Planted<StatusPlant>,
) -> Result<After<'a>, Refused> {
    let all = all && untracked;
    // The folder the proof says ignores every untracked name beneath it (`CoveredSince::ignoring`).
    let ignored = covered.and_then(|covered| covered.ignored());
    let decided = match previous {
        Some(previous) => decide(previous, covered, untracked, all, gitdir, common, work, now, alloc, plant).map(|regions| (previous, regions)),
        None => Err(WholeWhy::NoPrevious),
    };
    let (previous, regions) = match decided {
        Ok(decided) => decided,
        Err(why) => {
            let tree = head_tree(files, alloc)?;
            let computed = whole(gitdir, common, work, untracked, all, tree, files, alloc)?;
            return Ok(After { answer: StatusAnswer { text: computed.text, untracked, all, text_auto: computed.text_auto, tree, place: now }, made: Made::Whole(why) });
        }
    };
    let head_tree = previous.tree;
    // The index and its time: the racy rule's. Config and attributes are as the previous
    // answer read them (a change to any is a whole computation, above).
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
    let prepared = Prepared { index, index_time, text_auto: previous.text_auto };
    let walk = Walk { work, gitdir, index: &prepared.index, all, alloc };
    // Each region's untracked unit: itself in `-uall`; else the highest folder on its way the
    // index tracks nothing under (the whole's collapse), or itself.
    let mut units: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for region in regions.iter() {
        let mut unit = region.as_str();
        if !all && !plant.has(StatusPlant::NoCollapseUnit) {
            let ends = region.match_indices('/').map(|(at, _)| at).chain(core::iter::once(region.len()));
            for end in ends {
                let folder = &region[..end];
                if !walk.tracked(folder) && !walk.tracked_below(folder) {
                    unit = folder;
                    break;
                }
            }
        }
        units.push(String::from_str_in(unit, alloc));
    }
    let units = outermost(units, alloc);
    let keep_stale = plant.has(StatusPlant::KeepRevertedLine);
    let mut tracked: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut kept_untracked: AVec<'_, &str> = Vec::new_in(alloc);
    for line in previous.text.split_terminator('\n') {
        let path = line_path(line);
        let bare = path.strip_suffix('/').unwrap_or(path);
        if line.starts_with("?? ") {
            if !units.iter().any(|unit| within(unit, bare)) {
                kept_untracked.push(line);
            }
        } else if !regions.iter().any(|region| within(region, path)) || keep_stale {
            let mut kept = String::from_str_in(line, alloc);
            kept.push('\n');
            tracked.push(kept);
        }
    }
    // Each region's tracked entries, as the whole computes them.
    let mut store: Option<Store<'a>> = None;
    for region in regions.iter() {
        let from = prepared.index.entries.partition_point(|entry| entry.path.as_bytes() < region.as_bytes());
        for entry in prepared.index.entries[from..].iter().take_while(|entry| within(region, &entry.path)) {
            let mut line = String::new_in(alloc);
            tracked_line(entry, work, common, &prepared, files, &mut store, alloc, &mut line)?;
            // With the line kept above (a planted bug), the region's own is not added twice.
            let twice = keep_stale && tracked.iter().any(|it| it.as_str() == line.as_str());
            if !line.is_empty() && !twice {
                tracked.push(line);
            }
        }
    }
    tracked.sort_by(|a, b| line_path(a).as_bytes().cmp(line_path(b).as_bytes()));
    // Each unit's untracked lines, as the whole computes them: the unit's folders above it
    // proven unignored (the whole's walk reaches it only through them), then the unit itself.
    let mut found: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    if untracked {
        let mut ignores = crate::git_ignore::Ignores::new(alloc);
        let mut seen = 0u32;
        for unit in units.iter() {
            // A unit in a folder that ignored every untracked name all through the window (the
            // proof's, `CoveredSince::ignoring`: blaze's own, written by every command) has no
            // untracked line, then or now: neither listed nor its ignore files read.
            if ignored.is_some_and(|folder| !folder.is_empty() && within(folder, unit)) {
                continue;
            }
            // A tracked unit (an edited tracked file) has no untracked line: asked first, from
            // the index alone, so its edit reads no ignore file.
            if walk.tracked(unit) {
                continue;
            }
            let hidden = unit.match_indices('/').any(|(end, _)| ignores.ignored(work, gitdir, &unit[..end], true, files));
            if hidden {
                continue;
            }
            walk.one(unit, false, files, &mut ignores, &mut found, &mut seen)?;
        }
    }
    let mut out = String::new_in(alloc);
    for line in tracked.iter() {
        out.push_str(line);
    }
    let mut lines: AVec<'a, &str> = Vec::new_in(alloc);
    lines.extend(kept_untracked.iter().copied().map(line_path));
    lines.extend(found.iter().map(|path| path.as_str()));
    lines.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    lines.dedup();
    for path in lines.iter() {
        push_line(&mut out, "?? ", path)?;
    }
    let regions = u32::try_from(regions.len()).unwrap_or(u32::MAX);
    Ok(After { answer: StatusAnswer { text: out, untracked, all, text_auto: prepared.text_auto, tree: head_tree, place: now }, made: Made::Incremental { regions } })
}

/// The bugs a control plants in [`status_after`] (blaze_own::planted, D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusPlant {
    /// The status as it is.
    None,
    /// A previous tracked line in a named region is kept: a reverted edit keeps its ` M`.
    KeepRevertedLine,
    /// A region's untracked unit is the region itself: a new file in a folder already shown
    /// as `?? dir/` adds its own line.
    NoCollapseUnit,
    /// Lost coverage is taken as covered: the change-driven answer from a place the journal
    /// does not reach.
    CoverageIgnored,
    /// The proof claims a folder ignores every untracked name (`CoveredSince::ignoring`)
    /// without its ignore file read: planted where the claim is made (the tests' model), as
    /// blaze-cli journal_sweep.rs would be with its read skipped.
    IgnoringUnchecked,
}

impl blaze_own::planted::Plants for StatusPlant {}
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
