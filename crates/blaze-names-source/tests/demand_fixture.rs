//! Names by demand over real texts, judged against blaze-scope's whole-crate names.
//!
//! Three crates: `kore` (a core-like crate: no core of its own, a prelude, 40 macros each
//! writing one literal name and called once, one `$ident` macro called three times, in a file
//! module `kore/macros.rs`), `middle` (re-exports `kore::macros::*` by glob and
//! `kore::option::Option` by name), `app` (`use middle::*;`, `fn main`, and a module whose two
//! globs bring two different `dup`s: E0659).
//!
//! Every ask is answered by blaze-names-demand over [`CrateSource`] and by blaze-scope's
//! `resolve_crates` + `item_at` (the oracle); the answers must be equal. The floor of an ask is
//! the modules on its path and the one call that writes its name: the test prints both and their
//! ratio (rule 67). The control (rule 23) plants every summary `Any`: the answers stay equal and
//! every call of `kore::macros` is expanded, so the one expansion is the summaries' doing.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_mem::Frame;
use blaze_mem::tag::Tag;
use blaze_names_demand::driver::resolve;
use blaze_names_demand::{Answer, Anchor, Ask, Def, ModRef, Ns, Path, Resolver};
use blaze_names_source::{CrateFacts, CrateSource, CrateSourceOpts, SourceCounts, Summaries, Texts};
use blaze_own::{Alloc, Bump, Erased};
use blaze_scope::graph::{CrateId, Sysroot as ScopeSysroot};
use blaze_scope::names::{PathError, Why, Written};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::{Serial, key_of, root_key};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs, Sysroot, ToolchainUnit};

const KORE_LIB: &str = "#![no_core]
pub mod option {
    pub enum Option { None, Some }
}
pub mod prelude {
    pub mod rust_2021 {
        pub use crate::option::Option;
    }
}
pub mod macros;
";

/// 40 literal-name macros, each called once, and one `$ident` macro called three times: 43
/// calls in one module.
const KORE_MACROS: &str = "macro_rules! m0 { () => { pub fn made_0() {} }; } m0!();
macro_rules! m1 { () => { pub fn made_1() {} }; } m1!();
macro_rules! m2 { () => { pub fn made_2() {} }; } m2!();
macro_rules! m3 { () => { pub fn made_3() {} }; } m3!();
macro_rules! m4 { () => { pub fn made_4() {} }; } m4!();
macro_rules! m5 { () => { pub fn made_5() {} }; } m5!();
macro_rules! m6 { () => { pub fn made_6() {} }; } m6!();
macro_rules! m7 { () => { pub fn made_7() {} }; } m7!();
macro_rules! m8 { () => { pub fn made_8() {} }; } m8!();
macro_rules! m9 { () => { pub fn made_9() {} }; } m9!();
macro_rules! m10 { () => { pub fn made_10() {} }; } m10!();
macro_rules! m11 { () => { pub fn made_11() {} }; } m11!();
macro_rules! m12 { () => { pub fn made_12() {} }; } m12!();
macro_rules! m13 { () => { pub fn made_13() {} }; } m13!();
macro_rules! m14 { () => { pub fn made_14() {} }; } m14!();
macro_rules! m15 { () => { pub fn made_15() {} }; } m15!();
macro_rules! m16 { () => { pub fn made_16() {} }; } m16!();
macro_rules! m17 { () => { pub fn made_17() {} }; } m17!();
macro_rules! m18 { () => { pub fn made_18() {} }; } m18!();
macro_rules! m19 { () => { pub fn made_19() {} }; } m19!();
macro_rules! m20 { () => { pub fn made_20() {} }; } m20!();
macro_rules! m21 { () => { pub fn made_21() {} }; } m21!();
macro_rules! m22 { () => { pub fn made_22() {} }; } m22!();
macro_rules! m23 { () => { pub fn made_23() {} }; } m23!();
macro_rules! m24 { () => { pub fn made_24() {} }; } m24!();
macro_rules! m25 { () => { pub fn made_25() {} }; } m25!();
macro_rules! m26 { () => { pub fn made_26() {} }; } m26!();
macro_rules! m27 { () => { pub fn made_27() {} }; } m27!();
macro_rules! m28 { () => { pub fn made_28() {} }; } m28!();
macro_rules! m29 { () => { pub fn made_29() {} }; } m29!();
macro_rules! m30 { () => { pub fn made_30() {} }; } m30!();
macro_rules! m31 { () => { pub fn made_31() {} }; } m31!();
macro_rules! m32 { () => { pub fn made_32() {} }; } m32!();
macro_rules! m33 { () => { pub fn made_33() {} }; } m33!();
macro_rules! m34 { () => { pub fn made_34() {} }; } m34!();
macro_rules! m35 { () => { pub fn made_35() {} }; } m35!();
macro_rules! m36 { () => { pub fn made_36() {} }; } m36!();
macro_rules! m37 { () => { pub fn made_37() {} }; } m37!();
macro_rules! m38 { () => { pub fn made_38() {} }; } m38!();
macro_rules! m39 { () => { pub fn made_39() {} }; } m39!();
macro_rules! named { ($n:ident) => { pub fn $n() {} }; }
named!(alpha);
named!(beta);
named!(gamma);
";

const CALLS_IN_MACROS: usize = 43;

const MIDDLE_LIB: &str = "pub use kore::macros::*;
pub use kore::option::Option as Opt;
";

const APP_LIB: &str = "use middle::*;
pub fn main() {}
pub mod amb {
    mod x {
        pub fn dup() {}
    }
    mod y {
        pub fn dup() {}
    }
    pub use self::x::*;
    pub use self::y::*;
}
";

const NAMES: [&str; 3] = ["kore", "middle", "app"];

/// Each crate's files, relative to its directory.
const FILES: [&[(&str, &str)]; 3] = [&[("lib.rs", KORE_LIB), ("macros.rs", KORE_MACROS)], &[("lib.rs", MIDDLE_LIB)], &[("lib.rs", APP_LIB)]];

const DEPS: [&[(&str, usize)]; 3] = [&[], &[("kore", 0)], &[("middle", 1)]];

fn key(index: usize) -> CrateItemKey {
    CrateItemKey(ContentId::of("blaze-names-source fixture crate", NAMES[index].as_bytes()))
}

fn rust_2021() -> Implicit {
    Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false }
}

struct World {
    keys: [CrateItemKey; 3],
}

impl Texts for World {
    fn read(&self, krate: CrateItemKey, path: &str) -> Option<&str> {
        let index = self.keys.iter().position(|k| *k == krate)?;
        FILES[index].iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }
}

/// The oracle's view: absolute paths `/<crate>/<file>`.
impl<'a> SourceTexts<'a> for World {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        let rel = path.strip_prefix('/')?.strip_prefix(NAMES.get(krate)?)?.strip_prefix('/')?;
        FILES[krate].iter().find(|(p, _)| *p == rel).map(|(_, t)| *t)
    }
}

/// What both sides answer, compared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Said {
    Found(ItemKey),
    Ambiguous,
    Absent,
    Other,
}

fn said_by_demand(answer: Answer) -> Said {
    match answer {
        Answer::Found { def: Def::Item(key), .. } => Said::Found(key),
        Answer::Found { def: Def::Module(m), .. } => Said::Found(m.key),
        Answer::Ambiguous { .. } => Said::Ambiguous,
        Answer::Absent { .. } => Said::Absent,
        _ => Said::Other,
    }
}

fn said_by_oracle(names: &Names<'_>, module: ItemKey, ns: Ns, name: &str, alloc: Alloc<'_>) -> Said {
    let ns = match ns {
        Ns::Type => Namespace::Types,
        Ns::Value => Namespace::Values,
        Ns::Macro => Namespace::Macros,
    };
    let segments = [name];
    match names.item_at(module, ns, Written { kind: PathKind::Plain, segments: &segments }, alloc) {
        Ok(at) => Said::Found(at.def.key),
        Err(PathError { why: Why::Ambiguous, .. }) => Said::Ambiguous,
        Err(PathError { why: Why::NotFound, .. }) => Said::Absent,
        Err(_) => Said::Other,
    }
}

struct Asked {
    said: Said,
    answer: Answer,
    source: SourceCounts,
}

/// One ask on a fresh resolver and a fresh source (so its counts are its own).
fn ask_by_demand<'w>(world: &'w World, crates: &'w [CrateFacts<'w>], summaries: Summaries, from: ModRef, name: &'w str, ns: Ns, bump: Bump<'w>) -> Asked {
    let alloc = bump.alloc();
    let absent = ToolchainUnit(u32::MAX);
    let sysroot = Sysroot { core: ToolchainUnit(0), alloc: absent, std: ToolchainUnit(0), proc_macro: absent, bstd: absent };
    let mut source = CrateSource::new(CrateSourceOpts { texts: world, crates, sysroot, summaries, alloc: bump });
    let mut resolver = Resolver::new(alloc);
    let ask = Ask { from, path: Path { anchor: Anchor::Lexical, segs: &[] }, name, ns };
    let answer = resolve(&mut resolver, &mut source, ask, alloc).unwrap_or_else(|e| panic!("the demand ask for `{name}`: {e}"));
    Asked { said: said_by_demand(answer), answer, source: source.counts() }
}

/// The asks: (from module, name, namespace, outlines on its path, calls that must expand).
fn asks() -> [(ModRef, &'static str, Ns, usize, usize); 6] {
    let app = ModRef { krate: key(2), key: root_key(key(2)) };
    let amb = ModRef { krate: key(2), key: key_of(key(2), root_key(key(2)), "mod", "amb", 0) };
    [
        // `app::main`: the app's root alone.
        (app, "main", Ns::Value, 1, 0),
        // Through two globs: app, middle, kore's root (for `macros`), kore::macros; one call.
        (app, "made_7", Ns::Value, 4, 1),
        // A `$ident` macro's name, through the same globs: one call.
        (app, "alpha", Ns::Value, 4, 1),
        // A name re-exported by `use .. as`: app, middle, kore, kore::option.
        (app, "Opt", Ns::Type, 4, 0),
        // The standard prelude: app, middle, kore::macros (the globs, every call there may not
        // write `Option`), then kore, kore::prelude, kore::prelude::rust_2021, kore::option.
        (app, "Option", Ns::Type, 7, 0),
        // E0659: two globs, two `dup`s.
        (amb, "dup", Ns::Value, 3, 0),
    ]
}

fn crates() -> [CrateFacts<'static>; 3] {
    let facts = |index: usize| CrateFacts {
        name: NAMES[index],
        key: key(index),
        edition: Edition::E2021,
        implicit: rust_2021(),
        root: "lib.rs",
        deps: DEPS[index],
        features: &[],
    };
    [facts(0), facts(1), facts(2)]
}

fn oracle<'a>(world: &World, alloc: Bump<'a>) -> Names<'a> {
    let input = |index: usize, root: &'static str| CrateInput {
        name: NAMES[index],
        key: key(index),
        edition: Edition::E2021,
        implicit: rust_2021(),
        root,
        shown_from: "/",
        features: &[],
        deps: DEPS[index],
        cfg: &[],
        script: None,
    };
    let inputs = [input(0, "/kore/lib.rs"), input(1, "/middle/lib.rs"), input(2, "/app/lib.rs")];
    let sysroot = ScopeSysroot { core: Some(CrateId(0)), std: Some(CrateId(0)), ..ScopeSysroot::default() };
    resolve_crates(&inputs, sysroot, world, &Serial, alloc).unwrap_or_else(|e| panic!("the oracle's whole-crate names: {e}"))
}

/// Rule: every ask answered by demand equals blaze-scope's whole-crate answer; `app::main`
/// outlines one module and expands nothing; a name one macro call writes expands that call
/// alone, of 43 in its module. `app::amb::dup` is ambiguous on both sides (E0659).
///
/// Expected: "1 passed", printing per ask
/// `ask <name>: demand <said> == oracle <said>; outlines <n> (floor <f>, ratio <r>), expansions <n> (floor <f>), callee asks <n>`
/// and `E0659: app::amb::dup is ambiguous (<why>)`.
#[test]
fn demand_answers_equal_the_whole_crate_names_at_the_floor() {
    let frame = Frame::new(Tag::Expand).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let world = World { keys: [key(0), key(1), key(2)] };
    let crates = crates();
    let names = oracle(&world, bump);
    assert_eq!(names.root(2), root_key(key(2)), "the oracle's root key is blaze-scope's `root_key`");
    for (from, name, ns, outline_floor, expansion_floor) in asks() {
        let asked = ask_by_demand(&world, &crates, Summaries::Read, from, name, ns, bump);
        let want = said_by_oracle(&names, from.key, ns, name, alloc);
        let c = asked.source;
        std::println!(
            "ask {name}: demand {:?} == oracle {want:?}; outlines {} (floor {outline_floor}, ratio {:.2}), expansions {} (floor {expansion_floor}), callee asks {}",
            asked.said,
            c.outlines,
            c.outlines as f64 / outline_floor as f64,
            c.expansions,
            c.callee_asks,
        );
        assert_eq!(asked.said, want, "`{name}` from {from:?}: the demand answer {:?}", asked.answer);
        assert_eq!(c.expansions, expansion_floor, "`{name}` expands only the calls that may write it");
        if let Answer::Ambiguous { why, .. } = asked.answer {
            std::println!("E0659: app::amb::{name} is ambiguous ({why:?})");
        }
    }
    let dup = ask_by_demand(&world, &crates, Summaries::Read, asks()[5].0, "dup", Ns::Value, bump);
    assert!(matches!(dup.answer, Answer::Ambiguous { .. }), "E0659: `dup` from app::amb is ambiguous, not {:?}", dup.answer);
}

/// The control (rule 23): every summary planted `Writes::Any`. The answers stay equal to the
/// oracle's, and a name looked up through `kore::macros` now expands every one of its 43 calls:
/// the single expansion above is the summaries' doing.
///
/// Expected: "1 passed", printing `control <name>: expansions <n> (every call: 43)`.
#[test]
fn control_every_summary_any_expands_every_call() {
    let frame = Frame::new(Tag::Expand).unwrap_or_else(|e| panic!("the test's frame: {e:?}"));
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let world = World { keys: [key(0), key(1), key(2)] };
    let crates = crates();
    let names = oracle(&world, bump);
    for (from, name, ns, _, _) in asks() {
        let asked = ask_by_demand(&world, &crates, Summaries::AllAny, from, name, ns, bump);
        let want = said_by_oracle(&names, from.key, ns, name, alloc);
        std::println!("control {name}: expansions {} (every call: {CALLS_IN_MACROS})", asked.source.expansions);
        assert_eq!(asked.said, want, "control: `{name}` from {from:?}: the demand answer {:?}", asked.answer);
        if matches!(name, "made_7" | "alpha") {
            assert_eq!(asked.source.expansions, CALLS_IN_MACROS, "control: `{name}` expands every call of kore::macros");
        }
    }
}
