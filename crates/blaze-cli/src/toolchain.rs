//! The toolchain's shelf as this binary reads it (docs/v2/plan/design/toolchain.md, section 2):
//! `<home>/.blaze/v2/toolchain/`, one file per record named by its 64-hex digest. The home and
//! the start folder are driver facts `main` passes in; neither enters a key or a record. The
//! shelf is reached read-only: under the start folder through its `Dir`, else through the one
//! [`Ancestor`] that holds it (the nearest folder above the start that is above the shelf too),
//! confined to the shelf's folder by the one prefix every read is made under and by names that
//! are only ever 64 hex digits (`blaze_toolchain::shelf`). What is opened, and when, is
//! blaze-toolchain's: the manifest by `PINNED`, a pack only when a task needs a toolchain
//! crate's text.

use blaze_core::Mapping;
use blaze_toolchain::shelf::Shelf;
use bstd::cx::Cx;
use bstd::fs::{Dir, Fs, ReadOnly};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

/// The shelf's folder beneath the home.
pub const SHELF: &str = ".blaze/v2/toolchain";

/// Why the shelf's folder could not be reached: the folder, as a person would look for it,
/// and what was said.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShelfError {
    pub place: String,
    pub said: String,
}

impl core::fmt::Display for ShelfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the toolchain's shelf {}: {}", self.place, self.said)
    }
}

impl core::error::Error for ShelfError {}

/// The size from which a shelf file is mapped read-only and read in place, not read into the
/// frame: a pack (tens of KB to tens of MB) is mapped; a smaller file (a small pack) is one
/// read into `frame`, below the cost of a mapping (a page-rounded region and a kernel map
/// entry per file).
const MAP_FROM: u64 = 64 * 1024;

/// The shelf, opened once per command that needs it; a file of [`MAP_FROM`] bytes or more is
/// mapped, a smaller one read into `frame`.
pub struct HomeShelf<'c, 'f> {
    place: String,
    dir: Dir<ReadOnly>,
    cx: &'c Cx,
    frame: &'f Frame,
}

/// A file read from the shelf: its mapping, or, under [`MAP_FROM`] bytes, its bytes in the
/// shelf's frame. Every reader borrows it (`AsRef`); none copies it whole.
pub struct Held<'f>(Kept<'f>);

enum Kept<'f> {
    Mapped(Mapping),
    Read(Bytes<'f>),
}

impl Held<'_> {
    /// Whether the file is held as its mapping.
    #[cfg(test)]
    fn mapped(&self) -> bool {
        matches!(self.0, Kept::Mapped(_))
    }
}

impl core::fmt::Debug for Held<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            Kept::Mapped(m) => write!(f, "{} bytes mapped from the shelf", m.bytes().len()),
            Kept::Read(b) => write!(f, "{} bytes from the shelf", b.len()),
        }
    }
}

impl AsRef<[u8]> for Held<'_> {
    fn as_ref(&self) -> &[u8] {
        match &self.0 {
            Kept::Mapped(m) => m.bytes(),
            Kept::Read(b) => b.as_slice(),
        }
    }
}

impl<'c, 'f> HomeShelf<'c, 'f> {
    /// The shelf under `home` (absolute, as the driver gave it), through the effect layer's
    /// home capability (`Fs::home`): the one way a command reaches a folder outside its root.
    pub fn open(cx: &'c Cx, fs: &Fs, home: &str, frame: &'f Frame) -> Result<HomeShelf<'c, 'f>, ShelfError> {
        let place = format!("{}/{SHELF}", home.trim_end_matches('/'));
        let failed = |said: String| ShelfError { place: place.clone(), said };
        let folder = Path::new(SHELF).map_err(|e| failed(format!("{e}")))?;
        let dir = fs.home(cx, home, &folder).map_err(|e| failed(format!("{e}")))?.read_only();
        Ok(HomeShelf { place, dir, cx, frame })
    }

    fn path(&self, name: &str) -> Result<Path, String> {
        Path::new(name).map_err(|e| format!("{e}"))
    }
}

impl<'c, 'f> Shelf for HomeShelf<'c, 'f> {
    type Error = String;
    type Bytes = Held<'f>;

    fn place(&self) -> &str {
        &self.place
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, String> {
        let path = self.path(name)?;
        let meta = self.dir.stat(self.cx, &path).map_err(|e| format!("{e}"))?;
        Ok(meta.filter(|m| !m.dir).map(|m| m.len))
    }

    fn read(&mut self, name: &str, size: u64) -> Result<Held<'f>, String> {
        let path = self.path(name)?;
        if size >= MAP_FROM {
            // The shelf's files are written once and only replaced by a rename (the toolchain's
            // install), never written in place or cut: `Dir::map_read`'s contract holds.
            let mapping = self.dir.map_read(self.cx, &path).map_err(|e| format!("{e}"))?;
            return Ok(Held(Kept::Mapped(mapping)));
        }
        let mut bytes = Bytes::new_in(self.frame, "the toolchain's shelf");
        bytes.try_reserve(usize::try_from(size).map_err(|_| "a file past this machine's address space".to_string())?).map_err(|e| format!("{e}"))?;
        self.dir.read_all(self.cx, &path, &mut bytes).map_err(|e| format!("{e}"))?;
        Ok(Held(Kept::Read(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use blaze_toolchain::shelf::{self, Why};
    use blaze_toolchain::{ToolchainId, ToolchainView, record};
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    /// A shelf beneath the start folder opens the manifest named by its id, and one it lacks is
    /// named by its digest and folder.
    #[test]
    fn the_shelf_beneath_the_start_opens_its_manifest_and_names_what_it_lacks() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let fs = io.need_fs().expect("files");
        // In the test's root frame, from its context: no allocator is reached for.
        let erased = blaze_own::Erased::new(frame);
        let manifest = record::write(&[], &[], erased.handle()).expect("a record");
        let id = ToolchainView::id_of_bytes(&manifest);
        // The simulation keeps the home beneath its one root (`Fs::home`).
        let path = Path::new(&format!("u/me/{SHELF}/{id}")).expect("a path");
        fs.root().publish(&cx, &path, Content::bytes(&manifest), Durable::No).expect("write");
        let mut home = HomeShelf::open(&cx, &fs, "/u/me", frame).expect("the shelf");
        let opened = shelf::open_manifest(&mut home, id).expect("the manifest");
        assert_eq!(opened.as_ref(), &manifest[..]);
        let other = ToolchainId([3; 32]);
        let error = shelf::open_manifest(&mut home, other).expect_err("not there");
        assert_eq!((error.why, error.place.as_str()), (Why::Absent, "/u/me/.blaze/v2/toolchain"));
    }

    /// A pack-sized shelf file is held as its mapping and takes nothing of the frame (no read
    /// of the whole file into memory); a small one is read into the frame. Without the change
    /// (every file read into the frame) the first `assert!` fails: "a file of 262144 bytes is
    /// held as its mapping".
    /// Expected: `test toolchain::tests::a_pack_sized_shelf_file_is_mapped_not_copied ... ok`.
    #[test]
    fn a_pack_sized_shelf_file_is_mapped_not_copied() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let frame = cx.frame();
        let fs = io.need_fs().expect("files");
        let large: std::vec::Vec<u8> = (0..(MAP_FROM as u32) * 4).map(|i| (i % 251) as u8).collect();
        let small = [7u8; 100];
        let (big_name, small_name) = ("a".repeat(64), "b".repeat(64));
        for (name, bytes) in [(&big_name, &large[..]), (&small_name, &small[..])] {
            let path = Path::new(&format!("u/me/{SHELF}/{name}")).expect("a path");
            fs.root().publish(&cx, &path, Content::bytes(bytes), Durable::No).expect("write");
        }
        let mut home = HomeShelf::open(&cx, &fs, "/u/me", frame).expect("the shelf");
        let before = frame.used();
        let held = home.read(&big_name, large.len() as u64).expect("the large file");
        assert!(held.mapped(), "a file of {} bytes is held as its mapping", large.len());
        assert_eq!(held.as_ref(), &large[..], "the mapping is the file's bytes");
        assert_eq!(frame.used(), before, "the large file was copied into the frame");
        let held = home.read(&small_name, small.len() as u64).expect("the small file");
        assert!(!held.mapped(), "a file of 100 bytes is read into the frame");
        assert_eq!(held.as_ref(), &small[..]);
    }
}
