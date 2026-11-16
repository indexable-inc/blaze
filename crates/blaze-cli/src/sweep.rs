//! The checkout's files (drive.md, "The system side"): every file's path and content id, sorted
//! by path, behind [`Sweeper`]. [`StatSweep`] lists the tree and reads and hashes only a file
//! whose [`Stamp`] does not prove it unchanged since the leaf table: one whose stamp is unknown,
//! differs from the row's, or is racy (changed within the clock's granularity of the sweep, so a
//! second change in the same tick would leave it equal). Its stamps come from a [`Stamps`]
//! strategy (rule 78): [`FromMeta`] in the binary, the simulation's own in the test.
//!
//! Today bstd's `Meta` holds a length and whether it is a directory, no stamp, so [`FromMeta`]
//! knows none and every file is read: the path behind the stamp is built and tested, and turns on
//! when `Meta`'s stamp lands (the blaze-std lane's).
//!
//! The leaf table (`.blaze/v2/leaves`) is the last answer's files: path, stamp, digest. It is
//! what `changed` and `gone` are measured against, and what a stamp is compared with.

use blaze_drive::{FileEntry, WireError, WireWhy};
use blaze_ids::ContentId;
use blaze_own::{SortedMap, Vec};
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, Meta, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;

/// What a stat says of a file, enough to prove it unchanged when every part is equal: git's
/// index fields less the mode and owner, which a content id does not depend on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub len: u64,
    /// The last change of the content, and of the inode, in ns since the epoch.
    pub mtime_ns: u64,
    pub ctime_ns: u64,
    pub ino: u64,
    pub dev: u64,
}

impl Stamp {
    /// Bytes in a leaf row.
    const BYTES: usize = 40;

    /// The stamp proves the content only when its last change is more than `granularity` before
    /// the sweep began: a change after the stat, in the same tick, would leave the stamp equal.
    pub fn settled(self, swept_at_ns: u64, granularity_ns: u64) -> bool {
        self.mtime_ns.max(self.ctime_ns).saturating_add(granularity_ns) < swept_at_ns
    }

    fn put(self, out: &mut Vec<u8, &Frame>) {
        for part in [self.len, self.mtime_ns, self.ctime_ns, self.ino, self.dev] {
            out.extend_from_slice(&part.to_le_bytes());
        }
    }

    fn get(bytes: &[u8]) -> Stamp {
        let part = |at: usize| {
            let mut word = [0; 8];
            word.copy_from_slice(&bytes[at * 8..at * 8 + 8]);
            u64::from_le_bytes(word)
        };
        Stamp { len: part(0), mtime_ns: part(1), ctime_ns: part(2), ino: part(3), dev: part(4) }
    }
}

/// Where a sweep's stamps come from, and the clock they are on.
pub trait Stamps {
    /// The stamp of the file at `path` that the listing gave `meta` for; `None` when unknown.
    fn stamp(&self, path: &str, meta: &Meta) -> Option<Stamp>;
    /// When the sweep began, on the stamps' clock.
    fn swept_at_ns(&self) -> u64;
    /// The coarsest step of the stamps' clock.
    fn granularity_ns(&self) -> u64;
}

/// The stamps bstd's `Meta` gives: none yet, so every file is read and hashed. When `Meta`
/// carries a stamp, `stamp` returns it and `swept_at_ns` is the wall clock at the sweep's start.
pub struct FromMeta;

impl Stamps for FromMeta {
    fn stamp(&self, _: &str, _: &Meta) -> Option<Stamp> {
        None
    }

    fn swept_at_ns(&self) -> u64 {
        0
    }

    fn granularity_ns(&self) -> u64 {
        // Linux's file times step with the kernel's tick (`current_time`, up to 10 ms at
        // HZ=100); APFS's in ns. The coarser, until bstd says the file system's.
        10_000_000
    }
}

/// A file's content id: the same domain as `blaze_vfs::FILE_DOMAIN` (one digest of a file in the
/// tree). blaze-vfs is not a dependency here (std, and much else); the constant belongs in
/// blaze-ids (a gap for its lane, rule 30).
pub const FILE_DOMAIN: &str = "blaze v2 vfs file 1";

/// One file of the checkout as swept.
#[derive(Clone, Copy, Debug)]
pub struct Leaf<'f> {
    /// Relative to the checkout, `/`-separated.
    pub path: &'f str,
    pub id: ContentId,
    /// Its stamp when settled ([`Stamp::settled`]): what the next sweep may trust.
    pub stamp: Option<Stamp>,
}

/// What the request's `Start` is made of.
pub trait Sweeper {
    /// Every file under `root` that is part of the checkout, sorted by path, into `out`; `last`
    /// is the leaf table, whose digests a file proved unchanged keeps.
    fn sweep<'f>(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        last: &Leaves<'_>,
        frame: &'f Frame,
        out: &mut Vec<Leaf<'f>, &'f Frame>,
    ) -> Result<(), CliError>;
}

/// Directories that are never the checkout's: blaze's own, cargo's output, git's.
const NOT_CHECKOUT: [&str; 3] = [".blaze/", "target/", ".git/"];

/// A bug the sweep's test plants, to show the test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A racy stamp is trusted: a change in the tick of the last sweep is missed.
    RacyTrusted,
}

/// List the tree; read and hash only the files whose stamps do not prove them unchanged.
pub struct StatSweep<S> {
    pub stamps: S,
    pub plant: Plant,
}

impl<S: Stamps> Sweeper for StatSweep<S> {
    fn sweep<'f>(
        &self,
        cx: &Cx,
        root: &Dir<ReadWrite>,
        last: &Leaves<'_>,
        frame: &'f Frame,
        out: &mut Vec<Leaf<'f>, &'f Frame>,
    ) -> Result<(), CliError> {
        let cx = cx.span("sweep: list, then read and hash each file its stamp does not prove unchanged");
        let swept_at = self.stamps.swept_at_ns();
        let granularity = self.stamps.granularity_ns();
        let listed = root.list(&cx, frame)?;
        let mut bytes = Bytes::new_in(frame, "the sweep's file");
        for (path, meta) in listed.iter() {
            let text = path.as_str();
            if meta.dir || NOT_CHECKOUT.iter().any(|skip| text.starts_with(skip)) {
                continue;
            }
            let seen = self.stamps.stamp(text, meta);
            let stamp = match self.plant {
                Plant::None => seen.filter(|stamp| stamp.settled(swept_at, granularity)),
                Plant::RacyTrusted => seen,
            };
            let id = match (stamp, last.row(text.as_bytes())) {
                (Some(now), Some((id, Some(was)))) if now == was => id,
                _ => {
                    bytes.clear();
                    root.read_all(&cx, path, &mut bytes)?;
                    ContentId::of(FILE_DOMAIN, bytes.as_slice())
                }
            };
            // The path outlives the listing: copied into the command's frame, freed with it.
            let kept: &'f mut [u8] = Vec::from_slice_in(text.as_bytes(), frame).leak();
            let path = core::str::from_utf8(kept).map_err(|_| CliError::Text { what: "a path bstd listed is not UTF-8" })?;
            out.push(Leaf { path, id, stamp });
        }
        out.sort_unstable_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(())
    }
}

/// A manifest: the units' source.
pub fn is_manifest(path: &str) -> bool {
    path == "Cargo.toml" || path == "Cargo.lock" || path.ends_with("/Cargo.toml")
}

const LEAVES: &str = ".blaze/v2/leaves";
/// Rows, sorted by path: `len u32, path, digest [32], known u8, stamp [40]` (zeros when not
/// known). BCL1 had no stamp.
const LEAVES_MAGIC: &[u8; 4] = b"BCL2";

/// The last answer's files: path to content id and its settled stamp.
pub struct Leaves<'f> {
    rows: SortedMap<&'f [u8], (ContentId, Option<Stamp>), &'f Frame>,
}

impl<'f> Leaves<'f> {
    /// The table, or an empty one when there is none (a first build).
    pub fn read(cx: &Cx, root: &Dir<ReadWrite>, frame: &'f Frame) -> Result<Leaves<'f>, CliError> {
        let mut rows = SortedMap::new_in(frame);
        let path = Path::new(LEAVES)?;
        if root.stat(cx, &path)?.is_none() {
            return Ok(Leaves { rows });
        }
        let mut bytes = Bytes::new_in(frame, "the leaf table");
        root.read_all(cx, &path, &mut bytes)?;
        let kept: &'f [u8] = Vec::from_slice_in(bytes.as_slice(), frame).leak();
        let short = |at: usize, why| CliError::Leaves(WireError { what: "leaf table", at: u32::try_from(at).unwrap_or(u32::MAX), why });
        if kept.get(..4) != Some(LEAVES_MAGIC.as_slice()) {
            return Err(short(0, WireWhy::Magic));
        }
        let mut at = 4;
        while at < kept.len() {
            let len = kept.get(at..at + 4).ok_or(short(at, WireWhy::Short))?;
            let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
            let row = 4 + len + 32 + 1 + Stamp::BYTES;
            let whole = kept.get(at..at + row).ok_or(short(at, WireWhy::Short))?;
            let path = &whole[4..4 + len];
            let mut digest = [0; 32];
            digest.copy_from_slice(&whole[4 + len..36 + len]);
            let stamp = match whole[36 + len] {
                0 => None,
                1 => Some(Stamp::get(&whole[37 + len..])),
                tag => return Err(short(at + 36 + len, WireWhy::Tag(tag))),
            };
            if rows.last_key_value().is_some_and(|(last, _)| *last >= path) {
                return Err(short(at, WireWhy::Unsorted));
            }
            rows.insert(path, (ContentId(digest), stamp));
            at += row;
        }
        Ok(Leaves { rows })
    }

    pub fn get(&self, path: &[u8]) -> Option<ContentId> {
        self.rows.get(path).map(|(id, _)| *id)
    }

    /// The row of `path`: its digest, and its stamp when it was settled.
    pub fn row(&self, path: &[u8]) -> Option<(ContentId, Option<Stamp>)> {
        self.rows.get(path).copied()
    }

    /// The paths the table holds that `files` (sorted) does not.
    pub fn gone<'a>(&self, files: &[FileEntry<'a>], out: &mut Vec<&'f [u8], &Frame>) {
        for (path, _) in self.rows.iter() {
            if files.binary_search_by(|file| file.path.cmp(path)).is_err() {
                out.push(*path);
            }
        }
    }

    /// The table of `leaves` (sorted, as swept), replacing the old one.
    pub fn write(cx: &Cx, root: &Dir<ReadWrite>, leaves: &[Leaf<'_>], frame: &Frame) -> Result<(), CliError> {
        let mut bytes = Vec::new_in(frame);
        bytes.extend_from_slice(LEAVES_MAGIC);
        for leaf in leaves {
            let len = u32::try_from(leaf.path.len()).map_err(|_| CliError::Text { what: "a path past 4 GiB" })?;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend_from_slice(leaf.path.as_bytes());
            bytes.extend_from_slice(&leaf.id.0);
            match leaf.stamp {
                Some(stamp) => {
                    bytes.push(1);
                    stamp.put(&mut bytes);
                }
                None => bytes.extend_from_slice(&[0; 1 + Stamp::BYTES]),
            }
        }
        root.publish(cx, &Path::new(LEAVES)?, Content::bytes(&bytes), Durable::No)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! The sweep on bstd's simulated file system, its stamps from a model of the file times
    //! (what `Meta`'s stamp will give): files written, edited in place at the same length, and
    //! swept as the clock moves in steps of under, about and past a tick. Oracle: every swept id
    //! is the hash of the file's content now; and a sweep of a tree unchanged since a sweep in
    //! which every stamp had settled reads no byte (the simulated disk's `read` count).
    //! Planted: [`Plant::RacyTrusted`], a change in the tick of the last sweep missed.

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::string::{String, ToString};

    use blaze_sim::{Broke, Config, Tape, check, control};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// The model's clock step: file times are whole ticks, as on a file system that keeps them
    /// coarse.
    const TICK: u64 = 1_000;

    const PATHS: [&str; 6] = ["Cargo.toml", "README", "src/a.rs", "src/b/c.rs", "src/d.rs", "src/main.rs"];

    /// The stamps a file system would give: each file's last write, in whole ticks.
    struct Model {
        now: Cell<u64>,
        mtimes: RefCell<BTreeMap<String, u64>>,
    }

    impl Stamps for &Model {
        fn stamp(&self, path: &str, meta: &Meta) -> Option<Stamp> {
            let mtime_ns = *self.mtimes.borrow().get(path)?;
            Some(Stamp { len: meta.len, mtime_ns, ctime_ns: mtime_ns, ino: 7, dev: 1 })
        }

        fn swept_at_ns(&self) -> u64 {
            self.now.get()
        }

        fn granularity_ns(&self) -> u64 {
            TICK
        }
    }

    fn harness<E: core::fmt::Display>(what: &str) -> impl Fn(E) -> Broke + '_ {
        move |error| Broke::Harness(format!("{what}: {error}"))
    }

    fn sweeps(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let io = Io::simulated(SimWorld::new(tape.next_u64("sim seed"), Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(harness("a root context"))?;
        let fs = io.need_fs().map_err(harness("the file system"))?;
        let root = fs.open_dir(&cx, &Path::new("data").map_err(harness("a path"))?).map_err(harness("open"))?;
        let model = Model { now: Cell::new(10 * TICK), mtimes: RefCell::new(BTreeMap::new()) };
        let mut content: BTreeMap<&str, std::vec::Vec<u8>> = BTreeMap::new();
        let write = |path: &'static str, bytes: &[u8]| -> Result<(), Broke> {
            let at = Path::new(path).map_err(harness("a path"))?;
            root.publish(&cx, &at, Content::bytes(bytes), Durable::No).map_err(harness("a write"))?;
            let now = model.now.get();
            model.mtimes.borrow_mut().insert(path.to_string(), now - now % TICK);
            Ok(())
        };
        for path in PATHS.iter().take(1 + tape.index(PATHS.len(), "files")) {
            let bytes = tape.bytes(1..=16, "a file's content");
            write(path, &bytes)?;
            content.insert(path, bytes);
        }
        let sweep = |round: &str, content: &BTreeMap<&str, std::vec::Vec<u8>>| -> Result<u64, Broke> {
            let frame = cx.frame();
            let last = Leaves::read(&cx, &root, frame).map_err(harness("the leaf table"))?;
            let before = io.disk().ok_or(Broke::Harness("the world is not simulated".to_string()))?.read;
            let mut swept = Vec::new_in(frame);
            StatSweep { stamps: &model, plant }
                .sweep(&cx, &root, &last, frame, &mut swept)
                .map_err(harness("the sweep"))?;
            let after = io.disk().ok_or(Broke::Harness("the world is not simulated".to_string()))?.read;
            for leaf in swept.iter() {
                let now = content.get(leaf.path).ok_or(Broke::Harness(format!("{} was never written", leaf.path)))?;
                if leaf.id != ContentId::of(FILE_DOMAIN, now) {
                    return Err(Broke::Oracle(format!("{round}: {} swept with a stale digest", leaf.path)));
                }
            }
            Leaves::write(&cx, &root, &swept, frame).map_err(harness("the leaf table's write"))?;
            Ok(after - before)
        };
        sweep("the first sweep", &content)?;
        for _ in 0..1 + tape.index(3, "rounds") {
            let step = [0, TICK / 2, 3 * TICK][tape.index(3, "the clock's step")];
            model.now.set(model.now.get() + step);
            let paths: std::vec::Vec<&str> = content.keys().copied().collect();
            for path in paths {
                if tape.bool("edit this file") {
                    let len = content[path].len();
                    let mut bytes = tape.bytes(len..=len, "its new content, at its length");
                    bytes.resize(len, 0);
                    let at = PATHS.iter().find(|p| **p == path).ok_or(Broke::Harness(format!("{path} is no path")))?;
                    write(at, &bytes)?;
                    content.insert(at, bytes);
                }
            }
            sweep("a round's sweep", &content)?;
        }
        // Past every write's tick: this sweep settles every stamp, and the next reads nothing.
        model.now.set(model.now.get() + 3 * TICK);
        sweep("the settling sweep", &content)?;
        model.now.set(model.now.get() + 3 * TICK);
        match sweep("the unchanged tree's sweep", &content)? {
            0 => Ok(()),
            read => Err(Broke::Oracle(format!("an unchanged tree's sweep read {read} bytes"))),
        }
    }

    #[test]
    fn a_sweep_reads_only_what_its_stamps_do_not_prove_unchanged() {
        check(Config::cases(200), |tape| sweeps(tape, Plant::None));
    }

    #[test]
    fn the_oracle_catches_a_racy_stamp_trusted() {
        control(Config::cases(2000), Plant::None, Plant::RacyTrusted, "stale digest", sweeps);
    }

    /// Today's stamps (`FromMeta`, none known): every file is read, so nothing is missed.
    #[test]
    fn without_stamps_every_file_is_read() {
        assert_eq!(FromMeta.stamp("src/main.rs", &Meta { len: 3, dir: false }), None);
    }
}
