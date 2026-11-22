//! A program in the capability style under plain rustc: `main` takes its world, and
//! `#[bstd::main]` makes the process's `main` that hands it over (bstd::entry).

#![feature(macro_attr)]

use bstd::{Cx, entry::Error, out::Out};

#[bstd::main]
fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, "hello from bstd")?;
    Ok(())
}
