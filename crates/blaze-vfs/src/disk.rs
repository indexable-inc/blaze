//! What a watcher last saw of the disk (docs/v2/plan/design/restore.md): per head, the watched
//! directory, the cursor into the system's journal of changes, and each path's [`Stamp`] (its
//! inode, size, mtime and ctime) when the head took what was there. Saved with the tree in the
//! daemon's image, so a restart asks the journal for what changed since the cursor and reads only
//! paths whose stamp moved: no scan, and no file hashed that did not change.
//!
//! The column of a restored image is read in place ([`Base`]: sorted names, their ends and their
//! stamps, decoded field by field as they are read); what changes after goes into an overlay,
//! and the next image merges both into one sorted column again.
//!
//! What changes between images is in the VFS journal too, as `SEEN` records (one per committed
//! batch, after the head record it vouches for, and one per disk in a rewrite: its overlay), so
//! a start with no image since the last edit, or after a crash, still finds every stamp
//! ([`Disks::replay`]). A stamp taken within [`window_ns`] of its file's change is never saved: a
//! later write in the same tick of the file system's clock could leave the same stamp over other
//! bytes. The name is saved as [`Stamp::UNKNOWN`] and read again once steady.
//!
//! Beside the disks, the journal keeps a machine's local facts by name ([`Marks`]: the tool's
//! hash, each placed output's stamp and content, each checkout's carry), each a `MARK` record.

use std::collections::BTreeMap;

use blaze_core::{Cursor, FileName, Names, Stamp};
use blaze_format::cursor::{self, Put as _, Reader, Wanted};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use std::alloc::Global;

use crate::section::Section;

const MAGIC: [u8; 4] = *b"BZD2";

/// How long after a file's change its stamp vouches for nothing on a file system whose stamps
/// are whole seconds (HFS+ keeps one second, FAT and some network file systems two): a second
/// write in the tick of the first can leave the stamp as it was. Taken past this, a stamp can
/// only be left by a write that moves it.
pub const COARSE_NS: u64 = 2_000_000_000;

/// The window of a stamp with a sub-second part: such a file system's clock ticks at the
/// kernel's tick or finer (Linux's coarse clock, `current_time`, ticks at a jiffy, 10 ms at
/// `HZ=100`, the slowest; APFS, NTFS and ext4 with nanosecond stamps are finer), and this is
/// twice that tick. A file system whose fine clock ticks coarser than 10 ms is not covered by
/// this rule: it must be added by name, with a window of its own, never by widening this one.
pub const FINE_NS: u64 = 20_000_000;

/// The racy window of `stamp`, git's racy rule by the file system's clock: [`COARSE_NS`] when
/// its mtime and ctime both have no sub-second part (a coarse clock, or a fine one that landed
/// on a whole second: conservative either way), else [`FINE_NS`]. The one home of the window:
/// the watcher's column, the daemon's evidence of outputs and its local facts all ask
/// [`racy`].
pub fn window_ns(stamp: &Stamp) -> u64 {
    const SECOND: u64 = 1_000_000_000;
    match stamp.mtime_ns % SECOND == 0 && stamp.ctime_ns % SECOND == 0 {
        true => COARSE_NS,
        false => FINE_NS,
    }
}

/// The bit of a saved stamp's mode that says the head chose other bytes than the disk had at
/// that stamp: a conflict ([`crate::Watched::Conflict`]) or a commit to the head by anyone but
/// its watcher. No file system's mode reaches it (`st_mode` and `stx_mode` are 16 bits; the
/// drivers' synthesized modes are too). The watcher reads the stamp without it ([`plain`]): the
/// disk was seen at that stamp, and while it stays there the head keeps its choice. The column a
/// client checks ([`crate::Column`]) hands it as [`Stamp::UNKNOWN`]: it vouches for no bytes the
/// root holds. Kept in the stamp's own bytes, so the image's column and `SEEN` records carry it
/// with no layout of their own, and a column of the layout before it has none set.
const CHOSE: u32 = 1 << 31;

/// `stamp`, saved as one the head chose against ([`CHOSE`]). A gone or unknown one is itself.
pub(crate) fn chose(stamp: Stamp) -> Stamp {
    match stamp.is_gone() || stamp.is_unknown() {
        true => stamp,
        false => Stamp { mode: stamp.mode | CHOSE, ..stamp },
    }
}

/// `stamp` as the disk gave it: [`CHOSE`] cleared.
pub(crate) fn plain(stamp: Stamp) -> Stamp {
    match stamp.is_unknown() {
        true => stamp,
        false => Stamp { mode: stamp.mode & !CHOSE, ..stamp },
    }
}

/// Whether a saved stamp vouches that the column's root holds the bytes the disk had at it:
/// known, and not one the head chose against.
pub(crate) fn vouches(stamp: &Stamp) -> bool {
    !stamp.is_unknown() && stamp.mode & CHOSE == 0
}

/// When `stamp`'s file last changed, by the file system's clock.
pub(crate) fn changed_ns(stamp: &Stamp) -> u64 {
    stamp.mtime_ns.max(stamp.ctime_ns)
}

/// The first instant by the file system's clock at which a look finds `stamp` steady.
pub(crate) fn steady_at(stamp: &Stamp) -> u64 {
    changed_ns(stamp).saturating_add(window_ns(stamp))
}

/// Whether `stamp`, asked for at `asked_ns` or later, is within its [`window_ns`] of its
/// change: a racy stamp vouches for nothing.
pub fn racy(stamp: &Stamp, asked_ns: u64) -> bool {
    steady_at(stamp) > asked_ns
}

/// One entry of a `SEEN` record: a name and its stamp ([`Stamp::GONE`]: it and everything
/// under it are gone).
pub(crate) fn put_entry(out: &mut Vec<u8>, name: &str, stamp: Stamp) {
    out.put_bytes_u32(name.as_bytes());
    stamp.encode(out);
}

/// A `SEEN` record's payload, the one home of its layout: `head str_u32 | dir str_u32 | cursor
/// (25 B) | root: u8 flag + 32 B | count u32 | count x (name str_u32, stamp 36 B)`, the entries
/// ([`put_entry`]'s, `count` of them) in the order they were applied, `disk`'s cursor as it is
/// now, and `root` (the disk's, or none for all but the last of a split record:
/// `journal::frame_seen`).
pub(crate) fn encode_seen(
    out: &mut Vec<u8>,
    head: &str,
    disk: &Disk,
    root: Option<ContentId>,
    count: u32,
    entries: &[u8],
) {
    out.put_bytes_u32(head.as_bytes());
    out.put_bytes_u32(disk.dir.as_str().as_bytes());
    disk.cursor.encode(out);
    out.put_u8(u8::from(root.is_some()));
    out.put_bytes(&root.unwrap_or_default().0);
    out.put_u32(count);
    out.put_bytes(entries);
}

/// A restored column: `count` names sorted bytewise, each with a NUL after it as [`Names`] lays
/// names out (so a sweep of the whole column is one copy), its end past the NUL in `ends`, and
/// its stamp.
struct Base {
    section: Section,
    count: usize,
    /// Offsets in `section` of the ends, the stamps and the names.
    ends: usize,
    stamps: usize,
    names: usize,
}

impl Base {
    fn word(&self, at: usize) -> usize {
        let bytes = self.section.bytes();
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked at open")) as usize
    }

    fn start(&self, at: usize) -> usize {
        if at == 0 { 0 } else { self.word(self.ends + (at - 1) * 4) }
    }

    fn name(&self, at: usize) -> &str {
        let end = self.word(self.ends + at * 4) - 1;
        let bytes = &self.section.bytes()[self.names + self.start(at)..self.names + end];
        std::str::from_utf8(bytes).expect("checked at open")
    }

    /// Every name, as [`Names`] keeps them: one copy of the column's bytes.
    fn all(&self) -> Names {
        let mut starts = Vec::with_capacity(self.count);
        starts.extend((0..self.count).map(|at| self.start(at) as u32));
        let len = self.start(self.count);
        Names::from_checked(&self.section.bytes()[self.names..self.names + len], starts)
    }

    fn stamp(&self, at: usize) -> Stamp {
        Stamp::decode(&self.section.bytes()[self.stamps + at * Stamp::BYTES..])
            .expect("checked at open")
    }

    fn find(&self, name: &str) -> Result<usize, usize> {
        let (mut low, mut high) = (0, self.count);
        while low < high {
            let mid = (low + high) / 2;
            match self.name(mid).cmp(name) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }
}

/// One head's watched directory as last seen.
pub(crate) struct Disk {
    pub(crate) dir: FileName,
    pub(crate) cursor: Cursor,
    /// The root the column holds all of and vouches for, the head's when it was set. The
    /// invariant: every file of `root` is a name in the column, and every name whose saved
    /// stamp [`vouches`] holds in `root` the bytes the disk had at that stamp. So a rescan takes
    /// from the head only what changed since `root` (its diff), and the column a client checks
    /// pairs its stamps with `root` while it is the head's ([`crate::Vfs::column`]). Set by the
    /// watcher's commits ([`crate::Watcher`]: kept on from the head it was, or begun by a look
    /// at the whole directory) and moved by every other commit to the head, which marks what it
    /// changed ([`crate::Vfs::commit`]). `None`: neither is promised; the head is looked at
    /// whole, and the next look at the whole directory reads every file.
    pub(crate) root: Option<ContentId>,
    base: Option<Base>,
    /// Paths changed since `base`, by name under the driver's root: [`Stamp::GONE`] for one gone.
    over: HashMap<Box<str>, Stamp, Global>,
}

impl Disk {
    pub(crate) fn new(dir: FileName) -> Disk {
        Disk { dir, cursor: Cursor::None, root: None, base: None, over: HashMap::new_in(Global) }
    }

    /// What `name` was when last seen, as the disk gave it ([`plain`]); `None` for a name not
    /// seen or gone.
    pub(crate) fn get(&self, name: &str) -> Option<Stamp> {
        self.saved(name).map(plain)
    }

    /// What the column saved of `name`, [`CHOSE`] and all; `None` for a name not seen or gone.
    pub(crate) fn saved(&self, name: &str) -> Option<Stamp> {
        let stamp = match self.over.get(name) {
            Some(stamp) => *stamp,
            None => {
                let base = self.base.as_ref()?;
                base.stamp(base.find(name).ok()?)
            }
        };
        (!stamp.is_gone()).then_some(stamp)
    }

    pub(crate) fn set(&mut self, name: &str, stamp: Stamp) {
        match self.over.get_mut(name) {
            Some(it) => *it = stamp,
            None => _ = self.over.insert(name.into(), stamp),
        }
    }

    /// What changed since the column it was opened from, sorted bytewise: a journal rewrite's
    /// `SEEN` record of this disk ([`Stamp::GONE`] for a name gone).
    pub(crate) fn overlay(&self) -> Vec<(&str, Stamp)> {
        // alloc: once a rewrite, the overlay's names in order.
        let mut out: Vec<(&str, Stamp)> = self.over.iter().map(|(it, s)| (&**it, *s)).collect();
        out.sort_unstable_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// `name` and every name under it gone.
    pub(crate) fn remove_under(&mut self, name: &str) {
        // alloc: per gone directory, its names under it.
        let mut gone = Vec::new();
        self.under(name, |it, _| gone.push(Box::<str>::from(it)));
        for it in gone {
            self.over.insert(it, Stamp::GONE);
        }
    }

    /// Calls `f` with every name seen at or under `name` and its stamp as the disk gave it
    /// ([`plain`]), in name order, merging the column and what changed since.
    pub(crate) fn under(&self, name: &str, mut f: impl FnMut(&str, Stamp)) {
        self.under_saved(name, |it, stamp| f(it, plain(stamp)));
    }

    /// [`Disk::under`] with each stamp as saved, [`CHOSE`] and all: what the image keeps and
    /// what the column a client checks hands.
    pub(crate) fn under_saved(&self, name: &str, mut f: impl FnMut(&str, Stamp)) {
        let within = |it: &str| {
            it.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        let mut changed: Vec<(&str, Stamp)> =
            // alloc: what changed since the image under `name`, once per call.
            self.over.iter().filter(|(it, _)| within(it)).map(|(it, stamp)| (&**it, *stamp)).collect();
        changed.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut changed = changed.into_iter().peekable();
        if let Some(base) = &self.base {
            let mut at = base.find(name).unwrap_or_else(|it| it);
            while at < base.count {
                let it = base.name(at);
                // `a/b` < `a/b-c` < `a/b/c`: names under `name` need not follow it at once.
                if !within(it) {
                    if it.len() > name.len() && it.as_bytes()[..name.len()] == *name.as_bytes() {
                        at += 1;
                        continue;
                    }
                    break;
                }
                while let Some(&(over, stamp)) = changed.peek().filter(|(over, _)| *over < it) {
                    if !stamp.is_gone() {
                        f(over, stamp);
                    }
                    changed.next();
                }
                match changed.peek() {
                    Some(&(over, stamp)) if over == it => {
                        if !stamp.is_gone() {
                            f(over, stamp);
                        }
                        changed.next();
                    }
                    _ => f(it, base.stamp(at)),
                }
                at += 1;
            }
        }
        for (over, stamp) in changed {
            if !stamp.is_gone() {
                f(over, stamp);
            }
        }
    }

    /// Calls `f` with every name seen directly in `dir` and its stamp as saved, [`CHOSE`] and
    /// all (the watcher takes [`vouches`] of it with the name), in no set order. The column's subtree of each child is skipped by a search,
    /// not walked.
    pub(crate) fn children(&self, dir: &str, mut f: impl FnMut(&str, Stamp)) {
        let direct = |it: &str| {
            it.strip_prefix(dir)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
        };
        for (it, stamp) in &self.over {
            if direct(it) && !stamp.is_gone() {
                f(it, *stamp);
            }
        }
        let Some(base) = &self.base else { return };
        // alloc: once per relisting.
        let prefix = format!("{dir}/");
        let mut skip = String::with_capacity(prefix.len() + 32);
        let mut at = base.find(&prefix).unwrap_or_else(|it| it);
        while at < base.count {
            let it = base.name(at);
            let Some(rest) = it.strip_prefix(prefix.as_str()) else { break };
            match rest.find('/') {
                None => {
                    if !self.over.contains_key(it) {
                        f(it, base.stamp(at));
                    }
                    at += 1;
                }
                // Everything under `prefix + child + "/"` sorts before `prefix + child + "0"`.
                Some(slash) => {
                    skip.clear();
                    skip.push_str(&it[..prefix.len() + slash]);
                    skip.push('0');
                    at = base.find(&skip).unwrap_or_else(|it| it);
                }
            }
        }
    }

    /// At most how many names, and bytes of them with their NULs, a sweep of the whole column
    /// takes: its buffers are sized once.
    pub(crate) fn size(&self) -> (usize, usize) {
        let (count, bytes) = self.base.as_ref().map_or((0, 0), |base| match base.count {
            0 => (0, 0),
            count => (count, base.start(count)),
        });
        let over = self.over.keys().map(|it| it.len() + 1).sum::<usize>();
        (count + self.over.len(), bytes + over)
    }

    /// Every name seen at or under `name` into `names`, in name order, and its stamp as saved,
    /// [`CHOSE`] and all, into `stamps` (the watcher compares [`plain`] of it with the disk's and
    /// takes [`vouches`] of it as it sweeps). Every name the column holds was checked as it was
    /// opened or set. The whole
    /// directory of a column with nothing changed since it was mapped (a restart's sweep) is
    /// one copy of its names; any other sweep of the whole directory sizes its buffers once.
    pub(crate) fn sweep(&self, name: &str, names: &mut Names, stamps: &mut Vec<Stamp>) {
        if name == self.dir.as_str() && names.is_empty() {
            let (count, bytes) = self.size();
            stamps.reserve_exact(count);
            if let Some(base) = &self.base
                && self.over.is_empty()
            {
                *names = base.all();
                stamps.extend((0..base.count).map(|at| base.stamp(at)));
                return;
            }
            *names = Names::with_capacity(count, bytes);
        }
        self.under_saved(name, |it, stamp| {
            names.push_checked(it);
            stamps.push(stamp);
        });
    }
}

/// Every watched head's [`Disk`].
#[derive(Default)]
pub(crate) struct Disks {
    pub(crate) by_head: HashMap<Box<str>, Disk, Global>,
}

impl Disks {
    /// The disk of `head` watching `dir`: kept if it watched `dir`, else a new one.
    pub(crate) fn of(&mut self, head: &str, dir: &FileName) -> &mut Disk {
        // Found before it is made: a head that has its disk allocates no key.
        if !self.by_head.get(head).is_some_and(|it| it.dir == *dir) {
            // clone: once per head, or per change of the directory it watches.
            self.by_head.insert(head.into(), Disk::new(dir.clone()));
        }
        self.by_head.get_mut(head).expect("made above")
    }

    pub(crate) fn get(&self, head: &str) -> Option<&Disk> {
        self.by_head.get(head)
    }

    /// Applies one `SEEN` record's payload ([`encode_seen`]'s layout): the head's disk watching
    /// its directory takes the record's cursor and root, then each entry in order, a
    /// [`Stamp::GONE`] one by [`Disk::remove_under`], any other by [`Disk::set`]. The whole
    /// payload is checked before any of it is applied: `None` for a malformed one (cut short, a
    /// name not under the directory, bytes past the last entry), which applies nothing.
    pub(crate) fn replay(&mut self, payload: &[u8]) -> Option<()> {
        let mut read = Reader::new(payload);
        let head = read.str_u32().ok()?;
        let dir = FileName::new(read.str_u32().ok()?)?;
        let cursor = Cursor::decode(read.take(Cursor::BYTES).ok()?)?;
        let root = match read.array::<33>().ok()? {
            [0, ..] => None,
            [1, id @ ..] => Some(ContentId(*id)),
            _ => return None,
        };
        let count = read.u32().ok()?;
        let entries = read.at();
        for _ in 0..count {
            let name = read.str_u32().ok()?;
            FileName::new(name).filter(|it| it.is_under(&dir))?;
            Stamp::decode(read.take(Stamp::BYTES).ok()?)?;
        }
        read.finish().ok()?;
        let disk = self.of(head, &dir);
        disk.cursor = cursor;
        disk.root = root;
        let mut read = Reader::starting_at(payload, entries).ok()?;
        for _ in 0..count {
            let name = read.str_u32().ok()?;
            match Stamp::decode(read.take(Stamp::BYTES).ok()?)? {
                stamp if stamp.is_gone() => disk.remove_under(name),
                stamp => disk.set(name, stamp),
            }
        }
        Some(())
    }

    /// The image's bytes of every disk: `MAGIC`, a count, then per disk (heads in name order)
    /// its head, its directory, its cursor, its root (a byte saying whether, then 32), its count
    /// of names, its names' length, each name's end (past its NUL), each stamp, the names, each
    /// with a NUL after it.
    #[expect(
        blaze::narrowed_len,
        reason = "the image holds a disk's names' length in u32 and nothing caps a disk's saved names: names past 4 GiB need a typed refusal when the image is written"
    )]
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.put_bytes(&MAGIC);
        // alloc: once per image written.
        let mut heads: Vec<&Box<str>> = self.by_head.keys().collect();
        heads.sort_unstable();
        out.put_u32(heads.len() as u32);
        // alloc: once per image written, reused per disk.
        let (mut ends, mut stamps, mut names) = (Vec::new(), Vec::new(), Vec::new());
        for head in heads {
            let disk = &self.by_head[head];
            out.put_bytes_u32(head.as_bytes());
            out.put_bytes_u32(disk.dir.as_str().as_bytes());
            disk.cursor.encode(out);
            out.put_u8(u8::from(disk.root.is_some()));
            out.put_bytes(&disk.root.unwrap_or_default().0);
            (ends.clear(), stamps.clear(), names.clear());
            let mut count = 0u32;
            disk.under_saved(disk.dir.as_str(), |name, stamp| {
                names.put_cstr(name.as_bytes());
                ends.put_u32(names.len() as u32);
                stamp.encode(&mut stamps);
                count += 1;
            });
            out.put_u32(count);
            out.put_u32(u32::try_from(names.len()).expect("under 4 GiB"));
            out.put_bytes(&ends);
            out.put_bytes(&stamps);
            out.put_bytes(&names);
        }
    }

    /// The disks of an image's section, read in place: every length, end and name is checked
    /// once here (bounds, order, UTF-8), so reads after never fail. The error is what is wrong
    /// with the section.
    pub(crate) fn open(section: &Section) -> Result<Disks, &'static str> {
        fn cut(_: cursor::Error) -> &'static str {
            "cut short"
        }
        fn text<'a>(read: &mut Reader<'a>) -> Result<&'a str, &'static str> {
            read.str_u32().map_err(|error| match error.wanted {
                Wanted::Utf8 { .. } => "a name that is not UTF-8",
                _ => "cut short",
            })
        }
        let bytes = section.bytes();
        let mut read = Reader::new(bytes);
        if read.array::<4>().ok() != Some(&MAGIC) {
            return Err("no magic");
        }
        let mut disks = Disks::default();
        for _ in 0..read.u32().map_err(cut)? {
            let head = text(&mut read)?;
            let dir = FileName::new(text(&mut read)?).ok_or("a directory that is no name")?;
            let cursor = Cursor::decode(read.remaining()).ok_or("a bad cursor")?;
            read.skip(Cursor::BYTES).map_err(cut)?;
            let root = match read.array::<33>().map_err(cut)? {
                [0, ..] => None,
                [1, id @ ..] => Some(ContentId(*id)),
                _ => return Err("a bad root"),
            };
            let count = read.u32().map_err(cut)? as usize;
            let names_len = read.u32().map_err(cut)? as usize;
            let ends = read.at();
            let stamps = ends + count * 4;
            let names = stamps + count * Stamp::BYTES;
            read.skip(names + names_len - ends).map_err(cut)?;
            // clone: an `Arc` of the mapping, once per disk opened.
            let base = Base { section: section.clone(), count, ends, stamps, names };
            let mut last = 0;
            for n in 0..count {
                let stop = base.word(ends + n * 4);
                if stop <= last || stop > names_len || bytes[names + stop - 1] != 0 {
                    return Err("a name's end out of order or not after its NUL");
                }
                let name = std::str::from_utf8(&bytes[names + last..names + stop - 1])
                    .map_err(|_| "a name that is not UTF-8")?;
                if name.contains('\0')
                    || FileName::new(name).is_none_or(|it| !it.is_under(&dir))
                    || (n > 0 && base.name(n - 1) >= name)
                {
                    return Err("names out of order or outside the directory");
                }
                last = stop;
            }
            let disk = Disk { dir, cursor, root, base: Some(base), over: HashMap::new_in(Global) };
            disks.by_head.insert(head.into(), disk);
        }
        Ok(disks)
    }
}

/// The most bytes one mark holds: a mark is a fact (a stamp and a hash), not a store, and the
/// journal is rewritten whole with every mark in it.
pub const MARK_MOST: usize = 1 << 20;

/// A machine's local facts by name, each the bytes of its last `MARK` record: kept in name order,
/// so a rewrite frames them in one order and a prefix is one range.
#[derive(Default)]
pub(crate) struct Marks {
    // alloc: the daemon's lifetime; a few marks per checkout and output, ordered by name.
    by_name: BTreeMap<Box<str>, Box<[u8]>>,
}

impl Marks {
    /// Applies one `MARK` record's payload (`name str_u32 | bytes to the record's end`, empty
    /// bytes: the name has no mark); `None` for a malformed one, which applies nothing.
    pub(crate) fn replay(&mut self, payload: &[u8]) -> Option<()> {
        let mut read = Reader::new(payload);
        let name = read.str_u32().ok()?;
        let bytes = read.take_rest();
        (bytes.len() <= MARK_MOST).then_some(())?;
        self.set(name, bytes);
        Some(())
    }

    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        self.by_name.get(name).map(|it| &**it)
    }

    /// `name`'s mark is `bytes` from now; empty bytes remove it.
    pub(crate) fn set(&mut self, name: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            self.by_name.remove(name);
        } else {
            // alloc: once per mark set, the value it keeps.
            self.by_name.insert(name.into(), bytes.into());
        }
    }

    /// Every mark whose name starts with `prefix`, in name order.
    pub(crate) fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a [u8])> {
        self.by_name
            .range::<str, _>((std::ops::Bound::Included(prefix), std::ops::Bound::Unbounded))
            .take_while(move |(name, _)| name.starts_with(prefix))
            .map(|(name, bytes)| (&**name, &**bytes))
    }

    /// Every mark in name order: a journal rewrite frames them all.
    pub(crate) fn all(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.by_name.iter().map(|(name, bytes)| (&**name, &**bytes))
    }
}

/// A `MARK` record's payload, the one home of its layout.
pub(crate) fn encode_mark(out: &mut Vec<u8>, name: &str, bytes: &[u8]) {
    out.put_bytes_u32(name.as_bytes());
    out.put_bytes(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    fn stamp(n: u64) -> Stamp {
        Stamp { ino: n, size: n, mtime_ns: n, ctime_ns: n, mode: blaze_core::S_IFREG | 0o644 }
    }

    fn reopen(disks: &Disks) -> Disks {
        let mut bytes = Vec::new();
        disks.encode(&mut bytes);
        let len = bytes.len();
        let map = blaze_core::Mapping::copy(&bytes);
        Disks::open(&Section::new(map, 0..len).unwrap()).unwrap()
    }

    fn all(disk: &Disk, under: &str) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        disk.under(under, |it, stamp| out.push((it.to_owned(), stamp.ino)));
        out
    }

    /// The invariant: a [`crate::Column`] hands each name at or under its directory once, in
    /// name order, with the stamp last set for it, whether it is in the image's column, set
    /// since, or both; a name removed since, and a name outside the directory, is not handed.
    /// Against a map kept beside it. Control: the overlay alone (the image's column left out)
    /// misses names the model holds, and the oracle says so.
    #[test]
    fn a_column_hands_every_name_once_in_order_with_its_stamp() {
        let mut disks = Disks::default();
        let mut model = BTreeMap::new();
        let disk = disks.of("main", &name("w"));
        for (n, it) in ["w", "w/a", "w/b", "w/b/c", "w/b-d", "w/z"].into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
            model.insert(it.to_owned(), n as u64 + 1);
        }
        disk.set("x/out", stamp(99));
        let mut disks = reopen(&disks);
        let disk = disks.by_head.get_mut("main").unwrap();
        disk.root = Some(ContentId([7; 32]));
        // After the image: one name changed, one added, one subtree gone.
        disk.set("w/a", stamp(20));
        disk.set("w/b/e", stamp(21));
        disk.remove_under("w/b-d");
        model.insert("w/a".to_owned(), 20);
        model.insert("w/b/e".to_owned(), 21);
        model.remove("w/b-d");
        let disk = &disks.by_head["main"];
        let handed = |column: &crate::Column<'_>| {
            let mut out = Vec::new();
            column.each(|it, stamp| out.push((it.to_owned(), stamp.ino)));
            out
        };
        let column = crate::Column { dir: disk.dir.as_str(), root: disk.root, disk };
        let wanted: Vec<(String, u64)> = model.iter().map(|(it, n)| (it.clone(), *n)).collect();
        assert_eq!(handed(&column), wanted);
        assert_eq!(column.root, Some(ContentId([7; 32])));
        assert!(column.len() >= wanted.len(), "len is at least the names handed");

        let mut overlay = Disk::new(name("w"));
        for (it, stamp) in disk.overlay() {
            overlay.set(it, stamp);
        }
        let planted = crate::Column { dir: "w", root: None, disk: &overlay };
        assert_ne!(handed(&planted), wanted, "the control: the overlay alone is caught");
    }

    #[test]
    fn a_column_reopens_in_place_and_merges_what_changed_after() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        disk.cursor = Cursor::Sim { journal: 1, next: 9 };
        disk.root = Some(ContentId([7; 32]));
        for (n, it) in ["w/a", "w/a/x.rs", "w/a-b", "w/b.rs", "w/z/y"].into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        disks.of("other", &name("o")).set("o/f", stamp(9));
        let mut disks = reopen(&disks);
        assert_eq!(disks.by_head["other"].root, None);
        let disk = disks.by_head.get_mut("main").unwrap();
        assert_eq!(disk.cursor, Cursor::Sim { journal: 1, next: 9 });
        assert_eq!(disk.root, Some(ContentId([7; 32])));
        assert_eq!(disk.get("w/b.rs"), Some(stamp(4)));
        assert_eq!(all(disk, "w/a"), [("w/a".into(), 1), ("w/a/x.rs".into(), 2)]);
        disk.set("w/a/n.rs", stamp(7));
        disk.set("w/b.rs", stamp(8));
        disk.remove_under("w/z");
        assert_eq!(disk.get("w/z/y"), None);
        let names: Vec<(String, u64)> = all(disk, "w");
        let want = [("w/a", 1), ("w/a-b", 3), ("w/a/n.rs", 7), ("w/a/x.rs", 2), ("w/b.rs", 8)];
        assert_eq!(names, want.map(|(it, n)| (it.to_owned(), n)));
        let disks = reopen(&disks);
        assert_eq!(all(&disks.by_head["main"], "w"), want.map(|(it, n)| (it.to_owned(), n)));
    }

    /// A restart's sweep of a mapped column is one copy of its names, and gives what the
    /// merging walk gives, name for name and stamp for stamp as saved (a stamp the head chose
    /// against keeps its [`CHOSE`]); so does every sweep after a change.
    #[test]
    fn a_sweep_of_a_mapped_column_is_its_names_as_they_are() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        // Bytewise order: `-` and `.` sort before `/`.
        let names = ["w", "w/a", "w/a-b", "w/a.c", "w/a/x.rs", "w/b/c/d.rs", "w/z"];
        for (n, it) in names.into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        disk.set("w/a.c", chose(stamp(4)));
        let walked = |disk: &Disk, under: &str| {
            let (mut names, mut stamps) = (Names::default(), Vec::new());
            disk.under_saved(under, |it, stamp| {
                names.push_checked(it);
                stamps.push(stamp);
            });
            (names, stamps)
        };
        let swept = |disk: &Disk, under: &str| {
            let (mut names, mut stamps) = (Names::default(), Vec::new());
            disk.sweep(under, &mut names, &mut stamps);
            (names, stamps)
        };
        let mut disk = reopen(&disks).by_head.remove("main").unwrap();
        assert!(disk.over.is_empty());
        assert_eq!(disk.size(), (names.len(), names.iter().map(|it| it.len() + 1).sum()));
        let whole = swept(&disk, "w");
        assert_eq!(whole, walked(&disk, "w"));
        assert!(whole.1.iter().any(|it| !vouches(it)), "the chosen stamp is swept as saved");
        assert_eq!(whole.0.iter().collect::<Vec<_>>(), names);
        assert_eq!(swept(&disk, "w/a"), walked(&disk, "w/a"));
        disk.set("w/a/new.rs", stamp(9));
        disk.remove_under("w/b");
        let changed = swept(&disk, "w");
        assert_eq!(changed, walked(&disk, "w"));
        assert!(changed.0.iter().any(|it| it == "w/a/new.rs"));
        assert!(!changed.0.iter().any(|it| it.starts_with("w/b")));
        let empty = reopen(&Disks::default());
        assert!(empty.by_head.is_empty());
    }

    #[test]
    fn children_are_the_names_directly_in_a_directory() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        let names = ["w/a", "w/a/x/deep.rs", "w/a/y.rs", "w/a-b", "w/a.c", "w/a0", "w/b/c", "w/z"];
        for (n, it) in names.into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        let children = |disk: &Disk, dir: &str| {
            let mut out = Vec::new();
            disk.children(dir, |it, stamp| out.push((it.to_owned(), stamp.ino)));
            out.sort_unstable();
            out
        };
        let want = |names: &[(&str, u64)]| -> Vec<(String, u64)> {
            names.iter().map(|&(it, n)| (it.to_owned(), n)).collect()
        };
        let reopened = reopen(&disks);
        for disk in [&disks.by_head["main"], &reopened.by_head["main"]] {
            assert_eq!(children(disk, "w/a"), want(&[("w/a/y.rs", 3)]));
            assert_eq!(
                children(disk, "w"),
                want(&[("w/a", 1), ("w/a-b", 4), ("w/a.c", 5), ("w/a0", 6), ("w/z", 8)])
            );
        }
        // What changed since the column is merged: a new name, and one gone.
        let mut disk = reopen(&disks).by_head.remove("main").unwrap();
        disk.set("w/new", stamp(9));
        disk.remove_under("w/a0");
        assert_eq!(
            children(&disk, "w"),
            want(&[("w/a", 1), ("w/a-b", 4), ("w/a.c", 5), ("w/new", 9), ("w/z", 8)])
        );
    }

    #[test]
    fn a_bad_section_is_refused_naming_why() {
        let mut disks = Disks::default();
        disks.of("main", &name("w")).set("w/a", stamp(1));
        let mut bytes = Vec::new();
        disks.encode(&mut bytes);
        for cut in 0..bytes.len() {
            let map = blaze_core::Mapping::copy(&bytes[..cut]);
            assert!(Disks::open(&Section::new(map, 0..cut).unwrap()).is_err(), "cut at {cut}");
        }
    }
}
