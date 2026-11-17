//! A file whose bytes may not have reached the disk has no `clone_to`.

use bstd::{
    cx::Cx,
    fs::{Dir, FsError, ReadWrite, Written},
    path::Path,
};

pub fn clone_a_written_file(cx: &Cx, dir: &Dir<ReadWrite>, written: &Written, to: &Path) -> Result<Written, FsError> {
    written.clone_to(cx, dir, to)
}
