//! The facts a body makes: the record's `facts` (`Traced.facts`), each id by
//! [`record::fact`] with the body's own crate as its owner, sorted by id, so a reader's ask is
//! one binary search in its unit's made table (the driver lists them in the unit manifest's
//! root with their maker, `ManifestRoot::made`).
//!
//! - [`Export::ConstValue`]: a `const`'s or a `static`'s value, a variant's discriminant; for a
//!   `const fn`, what an evaluation of it reads of it (its built MIR's digest), so an evaluation
//!   that called it asks this fact and sees a change of it.
//! - [`Export::HiddenType`]: a function's opaque types' hidden types (return-position
//!   `impl Trait`, an `async fn`'s future).
//! - [`Export::Escaped`]: the digest of the items that escape the body after expansion (impls,
//!   `#[macro_export]` macros, `#[no_mangle]`, `#[export_name]`, `#[used]`, `#[link_section]`,
//!   `#[link]`).
//! - [`Export::Effects`]: the body's own effect and purity summary, what a caller's
//!   whole-program walk reads of it: its own effects and its callees by item, never its
//!   callees' effects folded in (those are the callees' own facts).
//!
//! The subject of each is the body's item key (`blaze_item_hash::values::body_key`), the subject
//! a reader's fact reference names, so a reader's id ([`crate::facts::id_of`]) and the maker's
//! are one by construction; the driver's item id (blaze-unitkey's) is not in it.
//!
//! Floor: one push per fact and one sort; a body makes a handful.

#![blaze::sans_io]

use std::alloc::{Allocator, Global};

use blaze_ids::ContentId;
use blaze_items::kinds as items;
use blaze_keyed::{
    driver::TaskFailed,
    record::{self, Fact, FactKind, Owner, Route, StableId},
};

use crate::facts;

/// One thing a body makes that another body may ask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Export {
    ConstValue,
    HiddenType,
    Escaped,
    Effects,
}

impl Export {
    pub const ALL: [Export; 4] = [Export::ConstValue, Export::HiddenType, Export::Escaped, Export::Effects];

    pub fn kind(self) -> FactKind {
        match self {
            Export::ConstValue => FactKind::ConstValue,
            Export::HiddenType => FactKind::HiddenType,
            Export::Escaped => FactKind::Escaped,
            Export::Effects => FactKind::Effects,
        }
    }

    /// The query code its id is stamped with: lane items' kind a reader's fact reference has,
    /// or this crate's own for the facts no query reads by kind yet.
    pub fn code(self) -> u16 {
        match self {
            Export::ConstValue => items::CONST_VALUE.0,
            Export::HiddenType => items::HIDDEN_TYPES.0,
            Export::Escaped => facts::ESCAPED,
            Export::Effects => facts::EFFECTS,
        }
    }

    /// The fact's id: of the body's item key `subject`, owned by its crate, and of nothing more
    /// (one fact of each kind per body).
    pub fn id(self, subject: &ContentId) -> ContentId {
        record::fact(self.kind(), Owner::of_subject(subject), self.code(), &StableId(*subject), b"")
    }
}

/// What two exports of one fact with two digests are refused as.
pub const TWO_EXPORTS: &str = "a body's made facts: one fact made with two digests";
/// What a made fact of another crate's owner, or of a kind a body does not make, is refused as.
pub const NOT_OURS: &str =
    "a body's made facts: a fact that does not route to its own unit's made table (another crate's owner, or a kind a surface holds)";

/// A body record's made facts, as they are built.
pub struct Exports {
    facts: Vec<Fact, Global>,
}

impl Default for Exports {
    fn default() -> Exports {
        Exports::new()
    }
}

impl Exports {
    pub fn new() -> Exports {
        Exports { facts: Vec::new_in(Global) }
    }

    /// Makes `what` of the body of item key `subject` with `digest`, the hash of its value's
    /// bytes.
    pub fn export(&mut self, subject: &ContentId, what: Export, digest: ContentId) {
        self.facts.push(Fact { id: what.id(subject), digest });
    }

    /// The facts as the record keeps them: sorted by id, each once, each routed to the made
    /// table of `owner`, the unit whose body makes them.
    pub fn finish<A: Allocator>(mut self, owner: Owner, alloc: A) -> Result<Vec<Fact, A>, TaskFailed> {
        self.facts.sort_unstable();
        let mut out: Vec<Fact, A> = Vec::with_capacity_in(self.facts.len(), alloc);
        for fact in self.facts {
            if record::route(&fact.id) != Ok(Route::Made(owner)) {
                return Err(TaskFailed::Refused { what: NOT_OURS });
            }
            match out.last() {
                Some(last) if last.id == fact.id => {
                    if last.digest != fact.digest {
                        return Err(TaskFailed::Refused { what: TWO_EXPORTS });
                    }
                }
                _ => out.push(fact),
            }
        }
        Ok(out)
    }
}
