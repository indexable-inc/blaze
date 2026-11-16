//! refused: no method named `create` found
// A folder granted read only cannot be written: `create` is on `Dir<ReadWrite>` only, so a
// `main` that takes `data: Dir<ReadOnly>` (bstd::grant) cannot make a file in it.
use bstd::{Cx, fs::{Dir, ReadOnly}, path::Path};

pub fn run(cx: &Cx, data: Dir<ReadOnly>, at: &Path) {
    let _ = data.create(cx, at);
}
