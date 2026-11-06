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

/// How [`Counted`] changes its state: every call, as a counter does, or the first only, as
/// proc-macro2's detection does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Counts {
    Every,
    First,
}

/// What [`Counted`] saw: its state, its calls run ahead of their turn, and each turn's name
/// and the state it started from.
#[derive(Debug, Default)]
struct CountedState {
    count: u32,
    taken: usize,
    /// Each call run ahead: its call site, the state it ran from, and the number it gave.
    ahead: Vec<(span::Span, u32, u32)>,
    ran_ahead: usize,
    took_ahead: usize,
    turns: Vec<(String, u32)>,
}

/// `#[counted(NAME)] item`: `pub const NAME: u32 = n;` in the item's place, `n` its state as
/// the call starts, a static of a proc macro whose calls are ordered. An attribute, since an
/// attribute's call is always resolved in a round (a bang macro's path that names another
/// crate's macro is expanded at once, in collection). A call run ahead gives its turn what it gave only if
/// the state is still the one it ran from, as blaze-macro-eval's sessions do.
#[derive(Debug)]
struct Counted {
    counts: Counts,
    state: blaze_core::lock::Lock<CountedState>,
}

/// A call that panicked leaves the state as it was at the panic.
impl std::panic::RefUnwindSafe for Counted {}

impl Counted {
    /// The number a call from `count` gives, and the state it leaves.
    fn run(&self, count: u32) -> (u32, u32) {
        match self.counts {
            Counts::Every => (count, count + 1),
            Counts::First => (count, 1),
        }
    }
}

impl hir_expand::proc_macro::ProcMacroExpander for Counted {
    fn requires_session(&self) -> bool {
        true
    }

    fn speculates(&self, _: &base_db::Db, _: base_db::Crate) -> bool {
        self.state.lock().taken > 0
    }

    fn speculate(
        &self,
        _: &base_db::Db,
        _: base_db::Crate,
        call: hir_expand::proc_macro::ProcMacroCall<'_>,
    ) {
        let mut state = self.state.lock();
        let count = state.count;
        let (n, _) = self.run(count);
        state.ahead.push((call.call_site, count, n));
        state.ran_ahead += 1;
    }

    fn expand(
        &self,
        _: &base_db::Db,
        _: hir_expand::proc_macro::ProcMacroCall<'_>,
    ) -> Result<hir_expand::proc_macro::Expanded, hir_expand::proc_macro::ProcMacroExpansionError>
    {
        unreachable!("a call of `#[counted]` is always in a calling crate")
    }

    fn expand_in(
        &self,
        _: &base_db::Db,
        _: base_db::Crate,
        call: hir_expand::proc_macro::ProcMacroCall<'_>,
    ) -> Result<hir_expand::proc_macro::Expanded, hir_expand::proc_macro::ProcMacroExpansionError>
    {
        let Some(crate::tt::TtElement::Leaf(crate::tt::Leaf::Ident(name))) =
            call.attrs.and_then(|it| it.iter().next())
        else {
            return Err(hir_expand::proc_macro::ProcMacroExpansionError::Panic(
                "#[counted]: one identifier".into(),
            ));
        };
        let name = name.clone();
        let mut state = self.state.lock();
        state.taken += 1;
        let count = state.count;
        let ahead = state
            .ahead
            .iter()
            .position(|it| it.0 == call.call_site)
            .map(|at| state.ahead.swap_remove(at));
        let n = match ahead {
            Some((_, from, n)) if from == count => {
                state.took_ahead += 1;
                n
            }
            _ => self.run(count).0,
        };
        state.count = self.run(count).1;
        state.turns.push((name.sym.as_str().to_owned(), n));
        let call_site = call.call_site;
        let tree = hir_expand::builtin::quote::quote! { call_site => pub const #name: u32 = #n; };
        Ok(hir_expand::proc_macro::Expanded {
            tree,
            reads: hir_expand::proc_macro::ExpansionReads::Unknown(
                "a test's proc macro, which traces nothing of what it read",
            ),
            state: hir_expand::proc_macro::StateUse::Session(
                "`#[counted]` gives the count the calls before it left",
            ),
        })
    }

    // dyn: `ProcMacroExpander::eq_dyn`'s signature, compared when a crate's macros are set
    fn eq_dyn(&self, other: &dyn hir_expand::proc_macro::ProcMacroExpander) -> bool {
        std::ptr::eq(self as *const Self as *const (), other as *const _ as *const ())
    }
}

/// Six calls of `#[counted]` in one round, beside other macros, with each batch run in `order`:
/// what each call's turn was given, and how many calls ran ahead and how many turns took one.
fn counted_in(order: RunOrder, counts: Counts) -> (Vec<(String, u32)>, usize, usize) {
    let previous = set_run_order(order);
    let counted = std::sync::Arc::new(Counted {
        counts,
        state: blaze_core::lock::Lock::new("test.def.batch_order.counted", CountedState::default()),
    });
    let extra = vec![(
        "#[proc_macro_attribute]\npub fn counted(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n".to_owned(),
        hir_expand::proc_macro::ProcMacro {
            name: intern::Symbol::intern("counted"),
            kind: hir_expand::proc_macro::ProcMacroKind::Attr,
            expander: counted.clone(),
            disabled: false,
        },
    )];
    let db = fixture::db_with_proc_macros(
        r#"
//- proc_macros: counted, identity
//- /lib.rs
macro_rules! items { ($($name:ident)*) => { $(pub struct $name;)* }; }
#[proc_macros::counted(A)]
pub struct HostA;
items!(X Y);
#[proc_macros::counted(B)]
pub struct HostB;
#[proc_macros::identity]
pub struct Free;
#[proc_macros::counted(C)]
pub struct HostC;
mod inner {
    #[proc_macros::counted(D)]
    pub struct HostD;
}
#[proc_macros::counted(E)]
pub struct HostE;
#[proc_macros::counted(F)]
pub struct HostF;
"#,
        extra,
    )
    .into_db();
    let krate = db.fetch_test_crate();
    let dump = crate_def_map(&db, krate).dump(&db);
    for item in ["A", "B", "C", "D", "E", "F", "Free", "X"] {
        assert!(dump.contains(item), "`{item}` is not in the def map:\n{dump}");
    }
    set_run_order(previous);
    let state = counted.state.lock();
    (state.turns.clone(), state.ran_ahead, state.took_ahead)
}

/// A macro whose calls are ordered gets them in the round's order whatever order the batch
/// runs its tasks in, and a call run ahead of its turn is taken only from the state its turn
/// has: a counter counts in order, and a macro that changes its state on its first call only
/// has each later call taken from its run ahead.
#[test]
fn ordered_calls_take_their_runs_ahead_only_from_their_turns_state() {
    let orders = [
        RunOrder::LeftToReaders,
        RunOrder::InOrder,
        RunOrder::Reversed,
        RunOrder::Strided(2),
        RunOrder::Strided(3),
    ];
    for order in orders {
        let (turns, _, _) = counted_in(order, Counts::Every);
        let names: Vec<&str> = turns.iter().map(|it| it.0.as_str()).collect();
        // `D` is in a module, collected after the root's calls of its round.
        let numbers: Vec<u32> = turns.iter().map(|it| it.1).collect();
        assert_eq!(
            numbers,
            [0, 1, 2, 3, 4, 5],
            "{order:?}: the counter counts its turns in order: {turns:?}"
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            ["A", "B", "C", "D", "E", "F"],
            "{order:?}: every call has one turn: {turns:?}"
        );

        let (turns, ran_ahead, took_ahead) = counted_in(order, Counts::First);
        let numbers: Vec<u32> = turns.iter().map(|it| it.1).collect();
        assert_eq!(
            numbers,
            [0, 1, 1, 1, 1, 1],
            "{order:?}: only the first call starts from nothing: {turns:?}"
        );
        if order == RunOrder::LeftToReaders {
            assert_eq!((ran_ahead, took_ahead), (0, 0), "no batch ran, so no call ran ahead");
        } else {
            assert!(
                took_ahead > 0 && took_ahead == ran_ahead,
                "{order:?}: every run ahead is taken: {ran_ahead} {took_ahead}"
            );
        }
    }
    // Both the counter's names in their turns' order and the order the front end collects
    // them in are the same in every order.
    let reference = counted_in(RunOrder::LeftToReaders, Counts::Every).0;
    for order in orders {
        assert_eq!(counted_in(order, Counts::Every).0, reference, "{order:?}");
    }
}
