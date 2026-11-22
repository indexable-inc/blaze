//! A program that says what it takes (bstd::grant): standard output, and one folder read only.
//! Started with no run spec it is refused, saying the grant and its fix; started with one on
//! file descriptor 3 (`blaze run` writes it, or `3< spec.json` by hand), it prints the
//! folder's `hello.txt`.

#![feature(macro_attr)]

use bstd::{
    Cx,
    entry::Error,
    fs::{Dir, ReadOnly},
    out::Out,
    path::Path,
};

#[bstd::main]
fn main(cx: &Cx, out: Out, data: Dir<ReadOnly>) -> Result<(), Error> {
    let at = Path::new("hello.txt")?;
    let mut held = cx.bytes();
    data.read_all(cx, &at, &mut held)?;
    let text = core::str::from_utf8(held.as_slice()).map_err(|_| Error::new("hello.txt is not UTF-8"))?;
    out.line(cx, text.trim_end())?;
    Ok(())
}
