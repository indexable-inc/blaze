// A whole file mapped read-only: its bytes read in place, nothing copied to the heap.

use blaze_core::{Done, Effect, Extent, Mapping};

use super::{Dir, FsError, FsOp, Rights, call, unexpected};
use crate::{cx::Cx, path::Path};

impl<P: Rights> Dir<P> {
    /// The whole file at `path` mapped read-only and shared ([`Effect::Map`] with
    /// [`Extent::Whole`]): its bytes as the file holds them, read in place for as long as any
    /// clone of the [`Mapping`] lives, never copied into a frame or the heap. For a large plain
    /// array read whole and kept (a toolchain pack, a record): a small file is read with
    /// [`Dir::read_sized`] into the task's frame instead. A missing file, or one the driver
    /// cannot map, is the driver's error, named with the file. A read: any rights may ask it.
    ///
    /// The caller's contract is [`Effect::Map`]'s: while the mapping lives, the file's bytes
    /// below its length when mapped are never written again and it is never cut (a shelf's or a
    /// store's file is only ever replaced by renaming another over it).
    pub fn map_read(&self, cx: &Cx, path: &Path) -> Result<Mapping, FsError> {
        let file = self.beneath(path);
        // A missing file is `NotFound`, as `open_read` says it, on every driver: the
        // simulation would map a missing file as empty.
        self.found(cx, &file)?;
        // clone: the effect's name, a refcount.
        let named = file.clone();
        match call(&self.io, cx, FsOp::Read, &file, |tag| Effect::Map { file: named, extent: Extent::Whole, tag })? {
            Done::Mapped(mapping) => Ok(mapping),
            done => Err(unexpected(FsOp::Read, &file, done)),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec::Vec;

    use crate::fs::{Content, Durable};
    use crate::io::{Io, Limits, SimWorld};
    use crate::mem::TASK_FRAME;
    use crate::path::Path;
    use crate::test::BUDGET;

    /// A file mapped is its bytes, and mapping it takes nothing of the task's frame (the bytes
    /// are the mapping's, not a copy); a missing file is an error naming it, never empty bytes.
    /// Expected: `test fs::map_read::tests::a_mapped_file_is_its_bytes_and_takes_no_frame ... ok`.
    #[test]
    fn a_mapped_file_is_its_bytes_and_takes_no_frame() {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let fs = io.need_fs().expect("files");
        // alloc: the test's file, once.
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let path = Path::new("shelf/pack").expect("a path");
        fs.root().publish(&cx, &path, Content::bytes(&data), Durable::No).expect("write");
        let before = cx.frame().used();
        let mapped = fs.root().map_read(&cx, &path).expect("the file mapped");
        assert_eq!(mapped.bytes(), &data[..], "the mapping is the file's bytes");
        assert_eq!(cx.frame().used(), before, "mapping a file took bytes of the task's frame");
        let missing = Path::new("shelf/none").expect("a path");
        let error = fs.root().map_read(&cx, &missing).expect_err("a missing file is not mapped");
        assert!(format!("{error}").contains("none"), "the error names the file: {error}");
    }
}
