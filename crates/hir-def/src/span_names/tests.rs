//! A span's name says which body it is of: no two bodies of a crate share one, or a profile adds
//! different bodies' times up as one. And naming it reads nothing into its query, or reuse would
//! depend on whether a profiler is on.

use std::{
    fmt::Write as _,
    ops::ControlFlow,
    sync::atomic::{AtomicU64, Ordering},
};

use base_db::{Crate, Db, all_crates};
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::Database as _;
use hegel::generators as gs;
use hegel::{Hegel, Settings};
use hir_expand::fixture;
use blaze_core::lock::Lock;
use std::alloc::Global;

use super::write_body_path;
use crate::{
    DefWithBodyId,
    expr_store::Body,
    nameres::{DefMap, crate_def_map, demand::scope_body_owners},
};

/// Every body of `krate`, those of items in blocks too, each once.
fn bodies(db: &Db, krate: Crate) -> HashSet<DefWithBodyId, Global> {
    let mut found = HashSet::new_in(Global);
    let mut maps: Vec<&DefMap> = vec![crate_def_map(db, krate)];
    while let Some(map) = maps.pop() {
        for (_, module) in map.modules(db) {
            for def in scope_body_owners(db, module.scope(db)) {
                maps.extend(Body::of(db, def).blocks(db).map(|(_, map)| map));
                found.insert(def);
            }
        }
    }
    found
}

/// Each body of `krate` by its span name, failing on a name two bodies share.
fn names(db: &Db, krate: Crate, text: &str) -> HashMap<String, DefWithBodyId, Global> {
    let mut named = HashMap::new_in(Global);
    for def in bodies(db, krate) {
        let mut name = String::new();
        write_body_path(db, def, &mut name);
        if let Some(other) = named.insert(name.clone(), def) {
            panic!("`{name}` names both {other:?} and {def:?} of\n{text}");
        }
    }
    named
}

const TYPES: [&str; 6] = ["u8", "u16", "i64", "[u8; 2]", "(u8, bool)", "&'static str"];

/// A crate of the shapes that once shared a name: an inherent and a trait impl of one generic
/// type for each of `types` (a bit per [`TYPES`]), `blocks` blocks in one body each declaring
/// `inner` (`nested` in a block of their own too), `unnamed` `const _` and `called` of them a
/// macro writes.
fn crate_text(types: u8, blocks: usize, nested: bool, unnamed: usize, called: usize) -> String {
    let mut text = String::from(
        "//- /lib.rs crate:shapes\npub struct NonZero<T>(T);\npub trait Swap { fn swap_bytes(self) -> Self; }\n",
    );
    for (_, ty) in TYPES.iter().enumerate().filter(|&(at, _)| types >> at & 1 == 1) {
        _ = writeln!(text, "impl NonZero<{ty}> {{ pub fn swap_bytes(self) -> Self {{ self }} }}");
        _ = writeln!(
            text,
            "impl Swap for NonZero<{ty}> {{ fn swap_bytes(self) -> Self {{ self }} }}"
        );
    }
    text.push_str("pub fn outer() {\n");
    for _ in 0..blocks {
        text.push_str(match nested {
            true => "    { { fn inner() {} } fn inner() {} }\n",
            false => "    { fn inner() {} }\n",
        });
    }
    text.push_str("}\n");
    for _ in 0..unnamed {
        text.push_str("const _: () = ();\n");
    }
    text.push_str("macro_rules! unnamed { () => { const _: () = (); }; }\n");
    for _ in 0..called {
        text.push_str("unnamed!();\n");
    }
    text
}

#[test]
fn no_two_bodies_of_a_crate_share_a_span_name() {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(48);
    Hegel::new(|tc| {
        let types = tc.draw(gs::integers::<u8>().max_value(63));
        let blocks = tc.draw(gs::integers::<usize>().max_value(4));
        let nested = tc.draw(gs::booleans());
        let unnamed = tc.draw(gs::integers::<usize>().max_value(3));
        let called = tc.draw(gs::integers::<usize>().max_value(3));
        let text = crate_text(types, blocks, nested, unnamed, called);
        let fixture = fixture::db(&text);
        let db = fixture.db();
        let named = names(db, fixture::test_crate(db), &text);
        let inner = named.keys().filter(|it| it.ends_with("}::inner")).count();
        assert_eq!(
            inner,
            blocks * (1 + usize::from(nested)),
            "a block's `inner` lost to another of\n{text}"
        );
    })
    .settings(Settings::new().test_cases(cases).database(None))
    .run();
}

const EXACT: &str = r#"
//- /lib.rs crate:shapes
pub struct NonZero<T>(T);
impl NonZero<u8> { pub fn swap_bytes(self) -> Self { self } }
impl NonZero<u16> { pub fn swap_bytes(self) -> Self { self } }
pub trait Shape { fn area(&self) -> u32 { 0 } }
impl Shape for NonZero<u16> { fn area(&self) -> u32 { 2 } }
const _: () = ();
const _: () = ();
pub enum Kind { A = 1 }
pub fn outer() {
    { fn inner() {} }
}
macro_rules! nonzero { ($t:ty) => { impl NonZero<$t> { pub fn get(self) -> Self { self } } }; }
nonzero!(u32);
nonzero!(u64);
//- /build.rs crate:custom-build:serde
fn main() {}
"#;

/// Every body of the fixture's workspace crates.
fn local_bodies(db: &Db) -> Vec<DefWithBodyId> {
    all_crates(db)
        .iter()
        .filter(|krate| krate.data(db).origin.is_local())
        .flat_map(|&krate| bodies(db, krate))
        .collect()
}

#[test]
fn a_span_name_says_which_impl_const_block_and_build_script() {
    let fixture = fixture::db(EXACT);
    let db = fixture.db();
    let mut all = HashSet::new_in(Global);
    for &krate in all_crates(db).iter() {
        if krate.data(db).origin.is_local() {
            all.extend(names(db, krate, EXACT).into_keys());
        }
    }
    for name in [
        "shapes::<impl#0>::swap_bytes",
        "shapes::<impl#1>::swap_bytes",
        "shapes::<impl#2>::area",
        "shapes::Shape::area",
        "shapes::_#0",
        "shapes::_#1",
        "shapes::Kind::A",
        "build_script[serde]::main",
    ] {
        assert!(all.contains(name), "no body is named `{name}`; the names are {all:?}");
    }
    assert!(
        all.iter().any(|it| it.starts_with("shapes::outer::{block#") && it.ends_with("}::inner")),
        "a block's item is not under its body; the names are {all:?}"
    );
    let by_macro = all
        .iter()
        .filter(|it| it.starts_with("shapes::<impl#0 in macro call ") && it.ends_with(">::get"));
    assert_eq!(
        by_macro.count(),
        2,
        "each `nonzero!` impl is named by its call; the names are {all:?}"
    );
}

/// A macro that calls itself writes each inner call at ordinal 0 of its own output, so its
/// bodies are named by the whole chain of calls: the two `get`s are two names.
#[test]
fn bodies_of_nested_macro_calls_are_named_by_the_outer_call() {
    let text = r#"
//- /lib.rs crate:nested
pub struct NonZero<T>(T);
macro_rules! nonzero {
    ($t:ty, outer) => { nonzero!($t); };
    ($t:ty) => { impl NonZero<$t> { pub fn get(self) -> Self { self } } };
}
nonzero!(u32, outer);
nonzero!(u64, outer);
"#;
    let fixture = fixture::db(text);
    let db = fixture.db();
    let named = names(db, fixture::test_crate(db), text);
    let gets = named.keys().filter(|it| it.ends_with(">::get")).count();
    assert_eq!(gets, 2, "the names are {:?}", named.keys().collect::<Vec<_>>());
}

/// The `function` each span was given, by a subscriber that records every span.
struct Named {
    next: AtomicU64,
    functions: Lock<Vec<String>>,
}

impl Default for Named {
    fn default() -> Named {
        Named {
            next: AtomicU64::new(0),
            functions: Lock::new("test.def.span_names.functions", Vec::new()),
        }
    }
}

struct Functions<'a>(&'a Lock<Vec<String>>);

impl tracing::field::Visit for Functions<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "function" {
            self.0.lock().push(value.to_owned());
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl tracing::Subscriber for &'static Named {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(self.next.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Functions(&self.functions));
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// What a query that opens `def`'s body span and reads its body read, each read as
/// `ingredient(id) kind`. `traced` only keys the run: the test asks once with no subscriber and
/// once under one that records, and so names, the span.
#[blaze_query::tracked(returns(ref))]
fn reads_beside_a_named_span(db: &Db, def: DefWithBodyId, traced: bool) -> Vec<String> {
    _ = traced;
    let ((), reads) = blaze_query::db::capture_reads(db, || {
        let _p = crate::body_span!(db, def, "span_names_reads_query").entered();
        _ = Body::of(db, def);
    });
    let Some(reads) = reads else { panic!("{def:?}: a span and a body read are shareable") };
    let mut out = Vec::new();
    reads.for_each_read(db.storage(), |read| {
        out.push(format!("{:?} {:?}", read.key, read.kind));
        ControlFlow::<()>::Continue(())
    });
    out
}

#[test]
fn naming_a_span_reads_nothing_into_its_query() {
    let fixture = fixture::db(EXACT);
    let db = fixture.db();
    let named: &'static Named = Box::leak(Box::default());
    for def in local_bodies(db) {
        let plain = reads_beside_a_named_span(db, def, false).clone();
        let traced = tracing::subscriber::with_default(named, || {
            reads_beside_a_named_span(db, def, true).clone()
        });
        assert_eq!(plain, traced, "naming {def:?}'s span changed what its query read");
    }
    let functions = named.functions.lock();
    for name in ["shapes::<impl#0>::swap_bytes", "shapes::_#1", "build_script[serde]::main"] {
        assert!(functions.iter().any(|it| it == name), "no span was named `{name}`: {functions:?}");
    }
}

/// A crate's label tells apart the builds of one package a profile would otherwise add up as
/// one: a library, its `cfg(test)` copy, a binary's, a proc macro and a build script.
#[test]
fn a_crate_label_tells_a_library_from_its_test_copy() {
    use base_db::TargetKind as K;
    let lib = K::Lib { is_proc_macro: false };
    let kinds = [
        (lib, lib),
        (K::Test, lib),
        (K::Test, K::Bin),
        (K::Test, K::Test),
        (K::Bin, K::Bin),
        (K::Lib { is_proc_macro: true }, K::Lib { is_proc_macro: true }),
        (K::BuildScript, K::BuildScript),
    ];
    let labels: Vec<&str> =
        kinds.iter().map(|&(kind, manifest)| super::kind_label(kind, manifest)).collect();
    assert_eq!(
        labels,
        ["lib", "lib-test", "bin-test", "test", "bin", "proc-macro", "build-script"]
    );
}
