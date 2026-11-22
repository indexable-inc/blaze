// The twin: the same folder granted read and write makes the file.
use bstd::{Cx, fs::{Dir, ReadWrite}, path::Path};

pub fn run(cx: &Cx, data: Dir<ReadWrite>, at: &Path) {
    let _ = data.create(cx, at);
}
