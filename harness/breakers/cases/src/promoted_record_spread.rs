//! sqlx's `#[sqlx::test(migrations = "..")]` passes
//! `&Migrator { migrations: Cow::Borrowed(const { &[..] }), .., ..Migrator::DEFAULT }` where a
//! `&'static Migrator` is wanted: a record literal whose other fields come from a constant, and
//! a `const { .. }` block, are constants, which rustc promotes. blaze promoted neither, so its
//! borrow checker refused every such test (E0716), and a borrow of one that outlived its
//! function pointed at a frame that was gone.

use std::borrow::Cow;

#[derive(Debug)]
pub struct Migrator {
    pub migrations: Cow<'static, [u32]>,
    pub locking: bool,
    pub table: Cow<'static, str>,
}

impl Migrator {
    pub const DEFAULT: Migrator =
        Migrator { migrations: Cow::Borrowed(&[]), locking: true, table: Cow::Borrowed("_migrations") };
}

fn wants_static(migrator: &'static Migrator) -> &'static Migrator {
    migrator
}

#[inline(never)]
fn migrator() -> &'static Migrator {
    wants_static(&Migrator { migrations: Cow::Borrowed(const { &[1, 2, 3] }), ..Migrator::DEFAULT })
}

#[inline(never)]
fn all_default<'a>() -> &'a Migrator {
    &Migrator { ..Migrator::DEFAULT }
}

/// A cell only in the field the literal writes is not copied out of the base.
#[derive(Debug)]
pub enum MaybeCell {
    Empty,
    Full(std::cell::Cell<u32>),
}

#[derive(Debug)]
pub struct State {
    pub state: MaybeCell,
    pub n: u32,
}

const FULL: State = State { state: MaybeCell::Full(std::cell::Cell::new(0)), n: 7 };

#[inline(never)]
fn cell_overwritten() -> &'static State {
    &State { state: MaybeCell::Empty, ..FULL }
}

/// A const block has its value's qualifs, not its type's.
#[inline(never)]
fn none() -> &'static Option<Box<u8>> {
    &(const { None::<Box<u8>> })
}

#[inline(never)]
fn overwritten() -> u64 {
    std::hint::black_box([u64::MAX; 64]).iter().fold(0, |sum, it| sum ^ it)
}

pub fn run() -> String {
    let (migrator, all_default) = (migrator(), all_default());
    let (state, none) = (cell_overwritten(), none());
    let sum = overwritten();
    format!("{migrator:?} {all_default:?} {state:?} {none:?} {sum}")
}
