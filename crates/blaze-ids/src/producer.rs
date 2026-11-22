//! A producer's identity (decision D105, docs/v2/plan/design/producer-id.md): the code that makes
//! a kind of value, as a digest the program's entry computes once and hands down. Never a
//! version string a person bumps: a result filed by a key holds the id of the code that made it
//! (rule 52), so other code is another key and an older record is never read as a fresh answer
//! (rule 21).
//!
//! - [`Row`]: one per producer, and [`Row::Whole`] for every value no row is split out for yet.
//! - [`ProducerId<P>`]: one producer's id, typed by its marker ([`Expander`], [`Lowering`],
//!   [`BodyChecker`], [`Names`], [`CodeBuilder`]): a key that asks for the expander's id does not
//!   take the lowering's (rule 76).
//! - [`Producers`]: the table, made once at the entry ([`Producers::of_build`]: every row from
//!   the binary's id while every producing function is in the entry crate) and passed down by
//!   reference; tests make theirs with [`Producers::for_tests`]. There is no default and
//!   no global (rule 75).

use core::{fmt, hash, marker::PhantomData};

use crate::{ContentId, Hasher};

/// One producer: a row of [`Producers`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Row {
    /// blaze-expand: what a macro call expands to.
    Expander = 0,
    /// blaze-lower and the parser under it: a body's, a signature's and an anonymous
    /// constant's lowered record.
    Lowering = 1,
    /// The body checker (blaze-typeck, blaze-solve, front2) and the mapping of its verdict to
    /// findings: a body's check record.
    BodyChecker = 2,
    /// The names solver (blaze-scope, blaze-world): a crate's scope.
    Names = 3,
    /// The code builder: a code record.
    CodeBuilder = 4,
    /// The whole binary: every value whose producer has no row of its own yet.
    Whole = 5,
}

impl Row {
    /// How many rows there are.
    pub const COUNT: usize = 6;

    /// Every row, in table order.
    pub const ALL: [Row; Row::COUNT] =
        [Row::Expander, Row::Lowering, Row::BodyChecker, Row::Names, Row::CodeBuilder, Row::Whole];

    /// The rows a crate's `[package.metadata.blaze] producer` list may name (blaze-producers):
    /// all but [`Row::Whole`].
    pub const FROM_SOURCE: [Row; Row::COUNT - 1] =
        [Row::Expander, Row::Lowering, Row::BodyChecker, Row::Names, Row::CodeBuilder];

    /// The row's name: what a manifest's `[package.metadata.blaze] producer` list says.
    pub const fn name(self) -> &'static str {
        match self {
            Row::Expander => "expander",
            Row::Lowering => "lowering",
            Row::BodyChecker => "body-checker",
            Row::Names => "names",
            Row::CodeBuilder => "code-builder",
            Row::Whole => "whole",
        }
    }

    /// The row named `name`, if one is.
    pub fn named(name: &str) -> Option<Row> {
        Row::ALL.into_iter().find(|row| row.name() == name)
    }

    /// The row's place in the table.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// A producer's marker: names its [`Row`]. Sealed: the rows are this file's.
pub trait Producer: sealed::Sealed {
    const ROW: Row;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! markers {
    ($($(#[$doc:meta])* $name:ident => $row:ident;)*) => {$(
        $(#[$doc])*
        #[derive(Debug)]
        pub enum $name {}
        impl sealed::Sealed for $name {}
        impl Producer for $name {
            const ROW: Row = Row::$row;
        }
    )*};
}

markers! {
    /// The expander's marker ([`Row::Expander`]).
    Expander => Expander;
    /// The lowering's marker ([`Row::Lowering`]).
    Lowering => Lowering;
    /// The body checker's marker ([`Row::BodyChecker`]).
    BodyChecker => BodyChecker;
    /// The names solver's marker ([`Row::Names`]).
    Names => Names;
    /// The code builder's marker ([`Row::CodeBuilder`]).
    CodeBuilder => CodeBuilder;
    /// The whole binary's marker ([`Row::Whole`]).
    Whole => Whole;
}

/// The id of producer `P`: the digest of its code, from [`Producers::get`] alone.
pub struct ProducerId<P> {
    id: ContentId,
    of: PhantomData<fn() -> P>,
}

impl<P> ProducerId<P> {
    /// The digest a key mixes in.
    pub fn id(&self) -> &ContentId {
        &self.id
    }
}

// By hand: a derive would ask `P` (an empty marker) for each trait.
impl<P> Clone for ProducerId<P> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<P> Copy for ProducerId<P> {}
impl<P> PartialEq for ProducerId<P> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl<P> Eq for ProducerId<P> {}
impl<P> hash::Hash for ProducerId<P> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
impl<P: Producer> fmt::Debug for ProducerId<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProducerId({}: {:?})", P::ROW.name(), self.id)
    }
}

const BUILD_DOMAIN: &str = "blaze producer of the build";
const TEST_DOMAIN: &str = "blaze producer for a test";
const TABLE_DOMAIN: &str = "blaze producer table";

/// Every producer's id, one per [`Row`]. Made once at the entry and passed down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Producers {
    rows: [ContentId; Row::COUNT],
}

impl Producers {
    /// Every row from the running binary's build id (bstd's `Host::build_id`), each under its
    /// row's name. A rebuilt blaze has new ids for every producer, so nothing is stale and nothing
    /// is shared between builds. The one constructor outside tests while every producing function
    /// is in the entry crate (blaze-cli `producers.rs` says why the source rows of piece 2 went).
    pub fn of_build(build_id: &[u8]) -> Producers {
        Producers { rows: Row::ALL.map(|row| row_digest(BUILD_DOMAIN, row, build_id)) }
    }

    /// A fixed table for a test: every row from `seed`. Two tests with one seed agree; nothing
    /// outside test code calls it (the producer ratchet counts a call outside test code).
    pub fn for_tests(seed: &[u8]) -> Producers {
        Producers { rows: Row::ALL.map(|row| row_digest(TEST_DOMAIN, row, seed)) }
    }

    /// This table with `row` alone made from `seed`: the causal test's one moved producer.
    pub fn planted_for_tests(self, row: Row, seed: &[u8]) -> Producers {
        let mut rows = self.rows;
        rows[row.index()] = row_digest(TEST_DOMAIN, row, seed);
        Producers { rows }
    }

    /// Producer `P`'s id.
    pub fn get<P: Producer>(&self) -> ProducerId<P> {
        ProducerId { id: self.rows[P::ROW.index()], of: PhantomData }
    }

    /// The id of `row`, untyped: for a table that maps another key space onto rows (the drive's
    /// kinds), never for a key of one producer (take [`Producers::get`] there).
    pub fn row(&self, row: Row) -> &ContentId {
        &self.rows[row.index()]
    }

    /// The whole table as one digest, rows in order: for a key that every producer's change
    /// must move (piece 1's drive tool).
    pub fn digest(&self) -> ContentId {
        let mut hasher = Hasher::new(TABLE_DOMAIN);
        for id in &self.rows {
            hasher.id(id);
        }
        hasher.finish()
    }
}

fn row_digest(domain: &'static str, row: Row, bytes: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(domain);
    hasher.part(row.name().as_bytes()).part(bytes);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The causal claim (rule 23): one producer's id moved moves exactly its row and the table's
    /// digest; every other row stays. Without the plant (the same seed) nothing moves.
    #[test]
    fn one_moved_producer_moves_its_row_alone() {
        let base = Producers::for_tests(b"test");
        for moved in Row::ALL {
            let planted = base.planted_for_tests(moved, b"older");
            for row in Row::ALL {
                assert_eq!(planted.row(row) == base.row(row), row != moved, "row {row:?} with {moved:?} moved");
            }
            assert_ne!(planted.digest(), base.digest());
        }
        assert_eq!(Producers::for_tests(b"test"), base);
        std::println!("producers: each of {} rows moves alone", Row::COUNT);
    }

    /// Typed ids read their own row; another build id moves every row (rule 21: the build's
    /// code made every record).
    #[test]
    fn typed_ids_and_every_row_of_the_build() {
        let table = Producers::for_tests(b"test");
        assert_eq!(table.get::<Expander>().id(), table.row(Row::Expander));
        assert_eq!(table.get::<BodyChecker>().id(), table.row(Row::BodyChecker));
        let (one, other) = (Producers::of_build(b"one"), Producers::of_build(b"other"));
        for row in Row::ALL {
            assert_ne!(one.row(row), other.row(row), "row {row:?} kept across builds");
            assert_eq!(Row::named(row.name()), Some(row));
        }
        assert_eq!(Producers::of_build(b"one"), one);
        std::println!("producers: typed ids hold; another build moves all {} rows", Row::COUNT);
    }
}
