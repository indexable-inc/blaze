//! refused: no method named `create` found
// A folder above the start folder is read-only by its type: an `Ancestor` has `stat`,
// `read_all` and `list`, and no method that makes, writes or removes a file.
use bstd::{Cx, fs::Ancestor, path::Path};

pub fn run(cx: &Cx, above: &Ancestor, at: &Path) {
    let _ = above.create(cx, at);
}
