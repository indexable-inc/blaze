// The twin: the same folder above the start folder is read.
use bstd::{Cx, fs::Ancestor, mem::Bytes, path::Path};

pub fn run(cx: &Cx, above: &Ancestor, at: &Path, into: &mut Bytes<'_>) {
    let _ = above.read_all(cx, at, into);
}
