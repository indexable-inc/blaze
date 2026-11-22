//! What a definition can write (crate::writes), on a table of definitions whose summaries are
//! derived by hand in the comment beside each: the literal-name struct maker, `$name:ident`, `mod
//! $m`, a nested literal macro, an exported `macro_rules!` written by a transcriber, a repetition,
//! an expression-only macro, a `use` tree, a glob, and the attribute, derive, foreign-block, const
//! argument and self-recursion cases. Each row is checked as the definition's own summary and,
//! where it has one, as one call's.
//!
//! The soundness property (every name a real expansion writes is allowed by its call's summary) is
//! in blaze-names-source's tests: it needs blaze-file-items' outline of the expansion, which
//! depends on this crate.
//!
//! Expected line: `macro writes: 24 definitions, 4 calls, every summary as derived by hand`.

extern crate std;

use blaze_mem::Frame;
use blaze_mem::tag::Tag;
use blaze_own::{AVec, Alloc, Erased, Vec};

use crate::def::{BuiltinDerive, BuiltinFn, DefAt, Export, LocalInner, MacroDefinition};
use crate::tokens::{DefSite, Edition, lex};
use crate::writes::{AnyWhy, Callee, Callees, CallWrites, MacroWrites, macro_writes};
use blaze_id::{ContentId, CrateItemKey};

#[derive(Clone, Copy, Debug)]
enum Want {
    Any(AnyWhy),
    /// These names (sorted), and whether it may export.
    Only(&'static [&'static str], bool),
}

struct Row {
    name: &'static str,
    /// The `macro_rules!` body: its arms.
    body: &'static str,
    /// The definition's own summary.
    def: Want,
    /// One call's input and its summary.
    call: Option<(&'static str, Want)>,
}

const ROWS: &[Row] = &[
    // A struct with a literal name and two impls: `Meters` (the impls write no name; `get` and
    // `clone` are in impl bodies).
    Row {
        name: "meters",
        body: "() => { pub struct Meters(pub f64); impl Meters { pub fn get(&self) -> f64 { self.0 } } \
               impl Clone for Meters { fn clone(&self) -> Self { Meters(self.0) } } };",
        def: Want::Only(&["Meters"], false),
        call: None,
    },
    // `struct $name`: the name is the input's. The definition alone: Any. The call `Foo`: `Foo`.
    Row {
        name: "named",
        body: "($name:ident) => { pub struct $name; impl $name { fn new() -> Self { $name } } };",
        def: Want::Any(AnyWhy::InputNames),
        call: Some(("Foo", Want::Only(&["Foo"], false))),
    },
    // `mod $m { .. }`: the module's name is the input's; `inner` is in the module, not here.
    Row {
        name: "module",
        body: "($m:ident) => { pub mod $m { pub fn inner() {} } };",
        def: Want::Any(AnyWhy::InputNames),
        call: Some(("a", Want::Only(&["a"], false))),
    },
    // A literal `mod m` whose body exports `out`: `m` here, `out` at the root (exports).
    Row {
        name: "module_export",
        body: "() => { mod m { pub fn f() {} #[macro_export] macro_rules! out { () => {} } } };",
        def: Want::Only(&["m", "out"], true),
        call: None,
    },
    // The callee of `outer`: `made_by_inner`.
    Row { name: "inner", body: "() => { fn made_by_inner() {} };", def: Want::Only(&["made_by_inner"], false), call: None },
    // `$crate::inner!()` folded: `made_by_inner`, and its own `outer_fn`.
    Row {
        name: "outer",
        body: "() => { $crate::inner!(); fn outer_fn() {} };",
        def: Want::Only(&["made_by_inner", "outer_fn"], false),
        call: None,
    },
    // The callee takes its name from its input: `fn $n`.
    Row { name: "inner_named", body: "($n:ident) => { fn $n() {} };", def: Want::Any(AnyWhy::InputNames), call: None },
    // `$crate::inner_named!(literal_name)`: the callee's input names are the nested input's
    // literal identifiers: `literal_name`.
    Row {
        name: "outer_named",
        body: "() => { $crate::inner_named!(literal_name); };",
        def: Want::Only(&["literal_name"], false),
        call: None,
    },
    // `#[macro_export] macro_rules! made` written by the transcriber: `made`, exported.
    Row {
        name: "exporter",
        body: "() => { #[macro_export] macro_rules! made { () => {} } };",
        def: Want::Only(&["made"], true),
        call: None,
    },
    // `$(fn $f() {})*`: every name is the input's. The call `a, b`: `a`, `b`.
    Row {
        name: "fns",
        body: "($($f:ident),*) => { $(fn $f() {})* };",
        def: Want::Any(AnyWhy::InputNames),
        call: Some(("a, b", Want::Only(&["a", "b"], false))),
    },
    // An expression: no item, nothing written; a block expression: nothing.
    Row { name: "sum", body: "() => { 1 + 2 }; ($a:expr) => {{ let x = $a; x }};", def: Want::Only(&[], false), call: None },
    // `use $crate::a::{b, c as d};`: `b` and `d`.
    Row { name: "uses", body: "() => { use $crate::a::{b, c as d}; };", def: Want::Only(&["b", "d"], false), call: None },
    // `use $crate::prelude::*;`: a glob, Any.
    Row { name: "glob", body: "() => { pub use $crate::prelude::*; };", def: Want::Any(AnyWhy::Glob), call: None },
    // `$t` where an item starts: Any.
    Row { name: "item", body: "($t:item) => { $t };", def: Want::Any(AnyWhy::ItemFromInput), call: None },
    // Builtin derives write impls only: `S`.
    Row { name: "derived", body: "() => { #[derive(Debug, Clone)] #[repr(C)] struct S; };", def: Want::Only(&["S"], false), call: None },
    // A proc macro's derive may write anything.
    Row { name: "serde", body: "() => { #[derive(Serialize)] struct S; };", def: Want::Any(AnyWhy::Derive), call: None },
    // An attribute that is no inert builtin may be an attribute macro.
    Row { name: "attr", body: "() => { #[some_attr] fn f() {} };", def: Want::Any(AnyWhy::AttrMacro), call: None },
    // An attribute from the input may be an attribute macro.
    Row {
        name: "metas",
        body: "($(#[$m:meta])*) => { $(#[$m])* fn f() {} };",
        def: Want::Any(AnyWhy::AttrFromInput),
        call: None,
    },
    // A foreign block's items bind here (`ext`, `EXT`); an impl with a const argument `{ N }`
    // ends at its body, so `hidden` is not read and `after` is.
    Row {
        name: "foreign",
        body: "() => { extern \"C\" { fn ext(); static EXT: u8; } \
               impl<const N: usize> Foo<{ N }> for Bar { fn hidden() {} } fn after() {} };",
        def: Want::Only(&["EXT", "after", "ext"], false),
        call: None,
    },
    // A call this table does not hold: Any.
    Row { name: "unknown", body: "() => { other!(); };", def: Want::Any(AnyWhy::NestedCall), call: None },
    // A tt-muncher calling itself: the input's names, and the literal `one` of the nested input
    // (an over-approximation). The call `x y`: `one`, `x`, `y`.
    Row {
        name: "muncher",
        body: "(@one $n:ident) => { fn $n() {} }; ($($n:ident)*) => { $($crate::muncher!(@one $n);)* };",
        def: Want::Any(AnyWhy::InputNames),
        call: Some(("x y", Want::Only(&["one", "x", "y"], false))),
    },
];

fn at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([7; 32])), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

/// The table's definitions by name.
struct Set<'d> {
    names: &'d [&'static str],
    defs: &'d [MacroDefinition<'d>],
}

impl<'d> Callees<'d> for Set<'d> {
    fn callee(&self, callee: Callee<'d>) -> Option<&'d MacroDefinition<'d>> {
        let name = match callee {
            Callee::Crate(name) | Callee::Bare(name) => name,
        };
        let at = self.names.iter().position(|it| *it == name)?;
        self.defs.get(at)
    }
}

fn check(what: &str, got: CallWrites, names: &[&str], want: Want) {
    match (got, want) {
        (CallWrites::Any(why), Want::Any(wanted)) => assert_eq!(why, wanted, "{what}: Any for another cause"),
        (CallWrites::Only { exports }, Want::Only(wanted, wanted_exports)) => {
            assert_eq!(names, wanted, "{what}: the names");
            assert_eq!(exports, wanted_exports, "{what}: whether it exports");
        }
        _ => panic!("{what}: {got:?} with names {names:?}, derived by hand {want:?}"),
    }
}

#[test]
fn every_summary_is_as_derived_by_hand() {
    let frame = Frame::new(Tag::Expand).expect("the test's frame");
    let erased = Erased::new(&frame);
    let alloc: Alloc<'_> = erased.handle();
    let mut names: AVec<'_, &'static str> = Vec::new_in(alloc);
    let mut defs: AVec<'_, MacroDefinition<'_>> = Vec::new_in(alloc);
    for row in ROWS {
        let body = lex(row.body, Edition::E2021, alloc).expect("the row's body lexes");
        names.push(row.name);
        defs.push(MacroDefinition::rules(body, at()));
    }
    // Builtins (crate::writes, "Builtins and proc macros"): `stringify!` writes nothing, `include!`
    // may write anything, a builtin derive writes impls only.
    let stringify = MacroDefinition::builtin_fn(BuiltinFn::Stringify, at(), alloc);
    let include = MacroDefinition::builtin_fn(BuiltinFn::Include, at(), alloc);
    let debug = MacroDefinition::builtin_derive(BuiltinDerive::Debug, at(), alloc);
    let set = Set { names: &names, defs: &defs };
    let mut out: AVec<'_, &str> = Vec::new_in(alloc);
    let mut calls = 0usize;
    for (row, def) in ROWS.iter().zip(defs.iter()) {
        let writes = macro_writes(def, &set, alloc).expect("the definition compiles");
        let got = writes.for_call(None, &mut out);
        check(row.name, got, &out, row.def);
        if let Some((input, want)) = row.call {
            let input = lex(input, Edition::E2021, alloc).expect("the call's input lexes");
            let mut call_out: AVec<'_, &str> = Vec::new_in(alloc);
            let got = writes.for_call(Some(&input), &mut call_out);
            check(row.name, got, &call_out, want);
            calls += 1;
        }
    }
    let got = MacroWrites::of(&stringify, alloc).expect("a builtin").for_call(None, &mut out);
    check("stringify", got, &out, Want::Only(&[], false));
    let got = MacroWrites::of(&include, alloc).expect("a builtin").for_call(None, &mut out);
    check("include", got, &out, Want::Any(AnyWhy::BuiltinFn(BuiltinFn::Include)));
    let got = MacroWrites::of(&debug, alloc).expect("a builtin").for_call(None, &mut out);
    check("derive(Debug)", got, &out, Want::Only(&[], false));
    // The unfolded summary of `outer` lists its call and is Any until closed.
    let open = MacroWrites::of(&defs[5], alloc).expect("compiles");
    assert_eq!(open.open_calls().len(), 1, "outer's one nested call, unfolded");
    assert_eq!(open.for_call(None, &mut out), CallWrites::Any(AnyWhy::NestedCall));
    std::println!("macro writes: {} definitions, {calls} calls, every summary as derived by hand", ROWS.len() + 3);
}
