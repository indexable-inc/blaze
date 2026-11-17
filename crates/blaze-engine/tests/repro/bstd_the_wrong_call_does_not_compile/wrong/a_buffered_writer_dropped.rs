//! A buffered writer holds bytes: it cannot be dropped, only finished.

use std::num::NonZero;

use bstd::{
    cx::Cx,
    fs::{File, FsError, ReadWrite},
};

pub fn drop_a_buffered_writer(cx: &Cx, file: &File<ReadWrite>, capacity: NonZero<usize>) -> Result<(), FsError> {
    let mut writer = file.buffered(cx.frame(), capacity)?;
    let _ = writer.push(cx, b"abc");
    Ok(())
}
