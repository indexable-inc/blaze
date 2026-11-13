//! The control of `bstd_the_wrong_call_does_not_compile`: the right form of each call its
//! fixtures get wrong. This crate checks; each file under `wrong/`, put in its place, does not.

use std::num::NonZero;

use bstd::{
    cx::Cx,
    fs::{Dir, File, Flushed, FsError, ReadWrite, Unsynced, Written},
    mem::ByteLen,
    path::Path,
};

/// A clone is made from a flushed file.
pub fn clone_a_flushed_file(cx: &Cx, dir: &Dir<ReadWrite>, flushed: &Flushed, to: &Path) -> Result<Written, FsError> {
    flushed.clone_to(cx, dir, to)
}

/// A buffered writer is finished.
pub fn finish_a_buffered_writer(cx: &Cx, file: &File<ReadWrite>, capacity: NonZero<usize>) -> Result<ByteLen, FsError> {
    let mut writer = file.buffered(cx.frame(), capacity)?;
    let pushed = writer.push(cx, b"abc");
    let finished = writer.finish(cx);
    pushed?;
    finished
}

/// A write goes through the handle that may write.
pub fn append_through_the_writable_handle(cx: &Cx, file: &File<ReadWrite>) -> Result<(), FsError> {
    let _reader = file.read_only();
    file.append_unsynced(cx, b"x", Unsynced::No)
}

/// A path is made to hold bytes by a publish that says its durability.
pub fn publish_saying_how_durably(cx: &Cx, dir: &Dir<ReadWrite>, to: &Path) -> Result<(), FsError> {
    dir.publish_unsynced(cx, to, bstd::fs::Content::bytes(b"x"), Unsynced::No).map(|_| ())
}
