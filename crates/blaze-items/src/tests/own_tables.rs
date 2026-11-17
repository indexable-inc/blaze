//! The listings a body reads of its own crate's tables, each a fact of one type, trait, name,
//! module or block (`Fact::InherentImpls`, `TraitImpls`, `TraitsNamed`, `ScopeTraitsNamed`,
//! `BlockImpls`): an item the listing holds, added or removed, changes it; an item it does not
//! hold leaves it be.

use blaze_ids::ContentId;
use hir_def::{DefWithBodyId, expr_store::Body};
use hir_expand::fixture;
use hir_ty::next_solver::SimplifiedType as HirSimplifiedType;

use super::{TestDB, edit, item, module, single};
use crate::{
    Fact, ItemId, block_impls, facts::planted, inherent_impls, key, trait_impls, value,
};

#[track_caller]
fn val(db: &TestDB, fact: Fact) -> ContentId {
    value(db, fixture::test_crate(db), &fact).unwrap_or_else(|err| panic!("{}", err.describe(db)))
}

/// One listing fact over a fixture: `fact` made from the database of `before`, `kept` texts
/// that add an item the listing does not hold, `changed` texts that add or remove one it does.
/// Every edit is written after what `fact`'s key names, so the key names the same thing in
/// each text.
struct Case {
    name: &'static str,
    before: &'static str,
    kept: &'static [&'static str],
    changed: &'static [&'static str],
    fact: fn(&TestDB) -> Fact,
}

/// The oracle: `case.fact`'s value keeps for every `kept` text and changes for every `changed`
/// one, and its key names the same fact in each. `Err` says which text broke which.
fn listing_follows_its_items(case: &Case) -> Result<(), String> {
    let (mut db, file) = single(case.before);
    let fact = (case.fact)(&db);
    let before = val(&db, fact);
    for (texts, kept) in [(case.kept, true), (case.changed, false)] {
        for (at, text) in texts.iter().enumerate() {
            edit(&mut db, file, text);
            let now = (case.fact)(&db);
            if now != fact {
                return Err(format!("{}: text {at} names the fact anew: {now:?}", case.name));
            }
            if (val(&db, fact) == before) != kept {
                let what = if kept { "changed for an item it does not list" } else { "kept" };
                return Err(format!("{}: {what} at text {at}", case.name));
            }
        }
    }
    Ok(())
}

fn adt_type(db: &TestDB, path: &str) -> HirSimplifiedType<'static> {
    match item(db, path) {
        ItemId::Adt(it) => HirSimplifiedType::Adt(it.into()),
        other => panic!("`{path}` is {other:?}"),
    }
}

/// The block of `f`'s body with items.
fn block_of_f(db: &TestDB) -> Fact {
    let ItemId::Function(f) = item(db, "f") else { panic!("`f` is no fn") };
    let body = Body::of(db, DefWithBodyId::from(f));
    let found: Vec<_> = body.blocks(db).map(|(block, _)| block).collect();
    match found[..] {
        [one] => block_impls(db, one),
        _ => panic!("{} blocks with items in `f`", found.len()),
    }
}

const INHERENT: &str = "
struct A;
struct B;
impl A { fn a(&self) {} }
fn f() {}
";

const TRAIT_IMPLS: &str = "
trait T {}
trait U {}
struct A;
struct B;
impl T for A {}
";

const NAMED: &str = "
trait T { fn m(&self); }
trait U { fn o(&self); }
";

const SCOPE: &str = "
pub trait Go { fn go(&self); }
pub trait Stay { fn stay(&self); }
pub trait Later { fn go(&self); }
mod inner {
    use crate::Go;
}
";

const BLOCK: &str = "
trait T {}
struct A;
fn f() {
    struct L;
    impl T for L {}
}
";

const CASES: &[Case] = &[
    Case {
        name: "the inherent impls of A",
        before: INHERENT,
        kept: &["
struct A;
struct B;
impl A { fn a(&self) {} }
fn f() {}
impl B { fn b(&self) {} }
"],
        changed: &[
            "
struct A;
struct B;
impl A { fn a(&self) {} }
fn f() {}
impl A { fn c(&self) {} }
",
            // One in a body's block is A's too.
            "
struct A;
struct B;
impl A { fn a(&self) {} }
fn f() { impl A {} }
",
            "
struct A;
struct B;
fn f() {}
",
        ],
        fact: |db| inherent_impls(db, &adt_type(db, "A")),
    },
    Case {
        name: "the impls of T",
        before: TRAIT_IMPLS,
        kept: &["
trait T {}
trait U {}
struct A;
struct B;
impl T for A {}
impl U for A {}
"],
        changed: &[
            "
trait T {}
trait U {}
struct A;
struct B;
impl T for A {}
impl T for B {}
",
            "
trait T {}
trait U {}
struct A;
struct B;
",
        ],
        fact: |db| match item(db, "T") {
            ItemId::Trait(it) => trait_impls(db, it),
            other => panic!("`T` is {other:?}"),
        },
    },
    Case {
        name: "the traits with an item named m",
        before: NAMED,
        kept: &["
trait T { fn m(&self); }
trait U { fn o(&self); }
trait W { fn o(&self); }
"],
        changed: &[
            "
trait T { fn m(&self); }
trait U { fn o(&self); }
trait V { fn m(&self) -> u32; }
",
            "
trait T { fn m(&self) -> u32; }
trait U { fn o(&self); }
",
        ],
        fact: |_| Fact::TraitsNamed(key::name_hash("m")),
    },
    Case {
        name: "the traits in inner's scope with an item named go",
        before: SCOPE,
        kept: &["
pub trait Go { fn go(&self); }
pub trait Stay { fn stay(&self); }
pub trait Later { fn go(&self); }
mod inner {
    use crate::Go;
    use crate::Stay;
}
"],
        changed: &[
            "
pub trait Go { fn go(&self); }
pub trait Stay { fn stay(&self); }
pub trait Later { fn go(&self); }
mod inner {
    use crate::Go;
    use crate::Later;
}
",
            "
pub trait Go { fn go(&self); }
pub trait Stay { fn stay(&self); }
pub trait Later { fn go(&self); }
mod inner {
}
",
        ],
        fact: |db| {
            let inner = key::module_key(db, module(db, "inner"));
            Fact::ScopeTraitsNamed { scope: inner, name: key::name_hash("go") }
        },
    },
    Case {
        name: "the impls of f's block",
        before: BLOCK,
        kept: &["
trait T {}
struct A;
fn f() {
    struct L;
    impl T for L {}
}
impl T for A {}
"],
        changed: &[
            "
trait T {}
struct A;
fn f() {
    struct L;
    impl T for L {}
    impl L {}
}
",
            "
trait T {}
struct A;
fn f() {
    struct L;
}
",
        ],
        fact: block_of_f,
    },
];

/// The invariant: each listing fact's value changes when an item it lists is added, removed
/// or changed, and keeps when an item it does not list is added.
#[test]
fn a_listing_changes_for_its_items_alone() {
    for case in CASES {
        if let Err(err) = listing_follows_its_items(case) {
            panic!("{err}");
        }
    }
}

/// Sets [`planted::LISTING_IGNORED`] while it lives.
struct Planted;

impl Planted {
    fn set() -> Planted {
        planted::LISTING_IGNORED.set(true);
        Planted
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        planted::LISTING_IGNORED.set(false);
    }
}

/// The control: with the listings hashing nothing of what they list, the oracle refuses every
/// case, so `a_listing_changes_for_its_items_alone` passing is evidence.
#[test]
fn the_oracle_catches_a_listing_that_ignores_what_it_lists() {
    let _planted = Planted::set();
    for case in CASES {
        let caught = listing_follows_its_items(case).is_err();
        assert!(caught, "{} passed with its listing ignored", case.name);
    }
}
