//! Narrowing never widens: a handle narrowed to read-only has no call that writes.

use bstd::{
    cx::Cx,
    fs::{File, FsError, ReadWrite, Unsynced},
};

pub fn append_through_the_narrowed_handle(cx: &Cx, file: &File<ReadWrite>) -> Result<(), FsError> {
    let narrowed = file.read_only();
    narrowed.append_unsynced(cx, b"x", Unsynced::No)
}
