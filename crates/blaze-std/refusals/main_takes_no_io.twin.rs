// The twin: the owner's hello, naming the one capability it uses.
#![feature(macro_attr)]

use bstd::{Cx, entry::Error, out::Out};

#[bstd::main]
pub fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, "Hello, world!")?;
    Ok(())
}
