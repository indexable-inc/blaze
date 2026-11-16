//! A program in the capability style under plain rustc: `main` takes its world, and
//! `#[bstd::main]` makes the process's `main` that hands it over (bstd::entry).

#![feature(macro_attr)]

use bstd::{Cx, Io, entry::Error};

#[bstd::main]
fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    let out = io.need_out()?;
    out.line(cx, "hello from bstd")?;
    Ok(())
}
