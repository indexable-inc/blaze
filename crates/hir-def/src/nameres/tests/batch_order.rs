//! A def map is the same whatever order the database runs a batch of its tasks in: the plane
//! schedules a round's expansions and a crate's dependencies as it likes.

use super::*;
use base_db::{RunOrder, set_run_order};

/// The def map of the fixture's test crate, with its diagnostics, collected with every batch
/// run in `order`.
fn collected_in(order: RunOrder, #[rust_analyzer::rust_fixture] ra_fixture: &str) -> String {
    let previous = set_run_order(order);
    let db = fixture::db(ra_fixture).into_db();
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let diagnostics: Vec<String> =
        def_map.diagnostics(&db).iter().map(|it| format!("{:?}", it.kind)).collect();
    let records = super::uses::render_records(&db, def_map);
    let used = super::uses::render_used(&db, krate);
    let root_scope = def_map.module(&db, def_map.root).scope(&db);
    let traits_used = super::uses::render_used_with_traits(
        &db,
        krate,
        &crate::resolver::HasResolver::resolver(def_map.root, &db),
        root_scope.traits(),
    );
    let scopes: Vec<String> = def_map
        .modules(&db)
        .map(|(_, it)| {
            let scope = it.scope(&db);
            let globs = scope.glob_ambiguities().iter().map(|it| it.name.as_str().to_owned());
            let shadowed = scope.shadowed_imports().iter().map(|it| it.name.as_str().to_owned());
            format!("{:?} {:?}", globs.collect::<Vec<_>>(), shadowed.collect::<Vec<_>>())
        })
        .collect();
    let collected = format!(
        "{}\n{diagnostics:#?}\n{records}\n{used}\n{traits_used}\n{scopes:#?}",
        def_map.dump(&db)
    );
    set_run_order(previous);
    collected
}

fn check_same_in_any_order(#[rust_analyzer::rust_fixture] ra_fixture: &str, has: &[&str]) {
    let read = collected_in(RunOrder::LeftToReaders, ra_fixture);
    for item in has {
        assert!(read.contains(item), "`{item}` is not in the def map:\n{read}");
    }
    let orders = [
        RunOrder::InOrder,
        RunOrder::Reversed,
        RunOrder::Strided(2),
        RunOrder::Strided(3),
        RunOrder::Strided(7),
    ];
    for order in orders {
        let batched = collected_in(order, ra_fixture);
        assert_eq!(read, batched, "the def map differs with its batches run {order:?}");
    }
}

/// Each round's expansions define the macros the next round calls, in many modules at once.
const ROUNDS: &str = r#"
//- /lib.rs
macro_rules! items {
    ($($name:ident)*) => { $(pub struct $name;)* };
}

macro_rules! first {
    ($m:ident) => {
        macro_rules! second {
            ($n:ident) => {
                macro_rules! third {
                    ($o:ident) => { pub mod $o { items!(Three); } };
                }
                pub mod $n { items!(Two); }
            };
        }
        pub mod $m { items!(One); }
    };
}

first!(one);
second!(two);
third!(three);

macro_rules! module {
    ($name:ident, $($item:ident)*) => { pub mod $name { items!($($item)*); } };
}

module!(a, A1 A2 A3);
module!(b, B1 B2 B3);
module!(c, C1 C2 C3);
module!(d, D1 D2 D3);
module!(e, E1 E2 E3);
module!(f, F1 F2 F3);
module!(g, G1 G2 G3);
module!(h, H1 H2 H3);

pub use a::*;
pub use h::H1 as Last;

mod nested;

missing!();

//- /nested.rs
module!(i, I1);
module!(j, J1);
second!(k);
"#;

#[test]
fn rounds_of_macro_rules_collect_the_same_in_any_order() {
    check_same_in_any_order(
        ROUNDS,
        &[
            "crate::one",
            "crate::two",
            "crate::three",
            "Three",
            "crate::h",
            "Last",
            "crate::nested::k",
            "J1",
        ],
    );
}

#[test]
fn exported_macros_of_a_dependency_collect_the_same_in_any_order() {
    check_same_in_any_order(
        r#"
//- /main.rs crate:main deps:lib
use lib::*;

lib::define!(made);
made!(Made1);
made!(Made2);
lib::structs!(S1 S2 S3);
mod m {
    lib::structs!(M1);
    crate::made!(M2);
}

//- /lib.rs crate:lib
#[macro_export]
macro_rules! structs {
    ($($name:ident)*) => { $(pub struct $name;)* };
}

#[macro_export]
macro_rules! define {
    ($name:ident) => {
        #[macro_export]
        macro_rules! $name { ($item:ident) => { $crate::structs!($item); }; }
    };
}
"#,
        &["Made1", "Made2", "S3", "crate::m", "M1", "M2"],
    );
}

/// The dependencies' def maps are collected at once, sharing a dependency of their own, and
/// the extern prelude keeps the order of the dependencies.
#[test]
fn dependencies_collect_the_same_in_any_order() {
    check_same_in_any_order(
        r#"
//- /main.rs crate:main deps:a,b,c,d
pub use a::A;
pub use b::*;
pub use c::C;
pub use d::from_base as d_base;

//- /a.rs crate:a deps:base
pub struct A;
base::made!(FromA);

//- /b.rs crate:b deps:base,a
pub use a::A as BA;
pub struct B;

//- /c.rs crate:c deps:base
pub struct C;
pub use base::Base;

//- /d.rs crate:d deps:base
pub mod from_base { pub use base::*; }

//- /base.rs crate:base
pub struct Base;
#[macro_export]
macro_rules! made {
    ($name:ident) => { pub struct $name; };
}
"#,
        &["A", "BA", "B", "C", "d_base"],
    );
}

#[test]
fn attribute_and_derive_macros_collect_the_same_in_any_order() {
    check_same_in_any_order(
        r#"
//- proc_macros: identity, derive_identity
//- minicore: derive, clone, copy
//- /lib.rs
macro_rules! made {
    ($name:ident) => {
        #[proc_macros::identity]
        #[derive(proc_macros::DeriveIdentity, Clone, Copy)]
        pub struct $name;
    };
}

#[proc_macros::identity]
pub struct Plain;

#[proc_macros::identity]
pub mod inner {
    #[proc_macros::identity]
    pub fn f() {}
}

made!(X1);
made!(X2);
made!(X3);
made!(X4);

#[derive(Clone)]
pub enum Derived { A, B }
"#,
        &["Plain", "crate::inner", "X1", "X4", "Derived"],
    );
}

/// What the imports and macro calls resolved through, as the dependencies and expansions of a
/// round come in any order: globs read through globs, two globs that disagree, `#[macro_use]`.
#[test]
fn name_records_are_the_same_in_any_order() {
    check_same_in_any_order(
        r#"
//- /main.rs crate:main deps:a,b
#[macro_use]
extern crate a;
pub use a::*;
pub use b::*;
pub use inner::*;
made!(Made);
mod inner {
    pub use super::Made as Again;
    pub use crate::outer::*;
}
mod outer { pub struct Out; }
pub struct Shared;
pub use a::Shared;
fn f(_: Again, _: Out, _: Same) { both(); }
mod traits { pub trait Tr {} }
mod reex { pub use crate::traits::Tr as _; }
use reex::*;

//- /a.rs crate:a
pub fn both() {}
pub struct Same;
pub struct Shared;
#[macro_export]
macro_rules! made {
    ($name:ident) => { pub struct $name; };
}

//- /b.rs crate:b
pub fn both() {}
pub use crate::Same as Other;
pub struct Same;
"#,
        &[
            "Made",
            "Again",
            "super::Made as Again Other",
            "crate::outer::* Other [Out]",
            "ambiguous both Values",
            "use made Macros",
            "- _ : type (glob)",
            "reex::* Other [_]",
            "crate::traits::Tr as _ Other",
        ],
    );
}
