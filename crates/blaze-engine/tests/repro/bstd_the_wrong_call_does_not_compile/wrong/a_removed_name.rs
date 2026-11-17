//! `Dir::write` decided durability for its caller and is gone: `publish` says it.

use bstd::{
    cx::Cx,
    fs::{Dir, FsError, ReadWrite},
    path::Path,
};

pub fn write_without_saying_how_durably(cx: &Cx, dir: &Dir<ReadWrite>, to: &Path) -> Result<(), FsError> {
    dir.write(cx, to, b"x")
}
