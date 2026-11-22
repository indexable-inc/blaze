// The twin: the folders above, as `Fs::ancestors` makes them.
use bstd::{Cx, fs::{Ancestor, AncestorError, Fs}};

pub fn run(cx: &Cx, fs: &Fs) -> Result<Vec<Ancestor>, AncestorError> {
    fs.ancestors(cx)
}
