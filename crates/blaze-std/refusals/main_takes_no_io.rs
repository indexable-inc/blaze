//! refused: `main` cannot take a `bstd::Io`
// D104: a `main` names each capability it uses; a whole world (`Io`) is no `Grant`, refused at
// that parameter, and `Grant`'s note names what to write instead.
#![feature(macro_attr)]

use bstd::{Cx, Io, entry::Error};

#[bstd::main]
pub fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "Hello, world!")?;
    Ok(())
}
