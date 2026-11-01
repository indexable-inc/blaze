//! What a watcher last saw of the disk (docs/v2/plan/design/restore.md): per head, the watched
//! directory, the cursor into the system's journal of changes, and each path's [`Stamp`] (its
//! inode, size, mtime and ctime) when the head took what was there. Saved with the tree in the
//! daemon's image, so a restart asks the journal for what changed since the cursor and reads only
//! paths whose stamp moved: no scan, and no file hashed that did not change.
//!
//! The column of a restored image is read in place ([`Base`]: sorted names, their ends and their
//! stamps, decoded field by field as they are read); what changes after goes into an overlay,
//! and the next image merges both into one sorted column again.

use blaze_core::{Cursor, FileName, Names, Stamp};
use blaze_format::cursor::{self, Put as _, Reader, Wanted};
use blaze_hash::HashMap;
use blaze_ids::ContentId;

use crate::section::Section;

const MAGIC: [u8; 4] = *b"BZD2";

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
    /// The head's root when the column last held all of it: a rescan takes from the head only
    /// what changed since (its diff), not every file. `None`: the head is looked at whole.
    pub(crate) root: Option<ContentId>,
    base: Option<Base>,
    /// Paths changed since `base`, by name under the driver's root: [`Stamp::GONE`] for one gone.
    over: HashMap<Box<str>, Stamp>,
}

impl Disk {
    pub(crate) fn new(dir: FileName) -> Disk {
        Disk { dir, cursor: Cursor::None, root: None, base: None, over: HashMap::default() }
    }

    /// What `name` was when last seen; `None` for a name not seen or gone.
    pub(crate) fn get(&self, name: &str) -> Option<Stamp> {
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

    /// `name` and every name under it gone.
    pub(crate) fn remove_under(&mut self, name: &str) {
        // alloc: per gone directory, its names under it.
        let mut gone = Vec::new();
        self.under(name, |it, _| gone.push(Box::<str>::from(it)));
        for it in gone {
            self.over.insert(it, Stamp::GONE);
        }
    }

    /// Calls `f` with every name seen at or under `name` and its stamp, in name order, merging
    /// the column and what changed since.
    pub(crate) fn under(&self, name: &str, mut f: impl FnMut(&str, Stamp)) {
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

    /// Calls `f` with every name seen directly in `dir` and its stamp, in no set order. The
    /// column's subtree of each child is skipped by a search, not walked.
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

    /// Every name seen at or under `name` into `names`, in name order, and its stamp into
    /// `stamps`. Every name the column holds was checked as it was opened or set. The whole
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
        self.under(name, |it, stamp| {
            names.push_checked(it);
            stamps.push(stamp);
        });
    }
}

/// Every watched head's [`Disk`].
#[derive(Default)]
pub(crate) struct Disks {
    pub(crate) by_head: HashMap<Box<str>, Disk>,
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

    /// The image's bytes of every disk: `MAGIC`, a count, then per disk (heads in name order)
    /// its head, its directory, its cursor, its root (a byte saying whether, then 32), its count
    /// of names, its names' length, each name's end (past its NUL), each stamp, the names, each
    /// with a NUL after it.
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
            disk.under(disk.dir.as_str(), |name, stamp| {
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
    pub(crate) fn open(section: Section) -> Result<Disks, &'static str> {
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
            let disk = Disk { dir, cursor, root, base: Some(base), over: HashMap::default() };
            disks.by_head.insert(head.into(), disk);
        }
        Ok(disks)
    }
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
        Disks::open(Section::new(map, 0..len).unwrap()).unwrap()
    }

    fn all(disk: &Disk, under: &str) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        disk.under(under, |it, stamp| out.push((it.to_owned(), stamp.ino)));
        out
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
    /// merging walk gives, name for name and stamp for stamp; so does every sweep after a change.
    #[test]
    fn a_sweep_of_a_mapped_column_is_its_names_as_they_are() {
        let mut disks = Disks::default();
        let disk = disks.of("main", &name("w"));
        // Bytewise order: `-` and `.` sort before `/`.
        let names = ["w", "w/a", "w/a-b", "w/a.c", "w/a/x.rs", "w/b/c/d.rs", "w/z"];
        for (n, it) in names.into_iter().enumerate() {
            disk.set(it, stamp(n as u64 + 1));
        }
        let walked = |disk: &Disk, under: &str| {
            let (mut names, mut stamps) = (Names::default(), Vec::new());
            disk.under(under, |it, stamp| {
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
            assert!(Disks::open(Section::new(map, 0..cut).unwrap()).is_err(), "cut at {cut}");
        }
    }
}
