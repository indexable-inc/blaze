//! D189 step 4, proof (3): the whole-crate differential of the impl table, one row per impl
//! (trait, self head, where written), compared under `BlockScopes::Modules` and `Bodies`, with
//! every body the `Bodies` solve held given its `BodyScope`. The crate has nested blocks, a
//! block-local struct with an impl, `const _: () = { impl .. }`, a block `macro_rules!`, a block
//! `use`, and shadowing at two depths. The two tables must be equal row for row, and every
//! held body must be solved (no refusal). Each record must also write the same bytes when built
//! twice (rule 25).
//!
//! Two controls (rule 23):
//! - drop one body's record, and its impls are only under `Modules`;
//! - file the body impls through the crate's names alone (blaze-scope `OwnNames`) instead of
//!   through the body's record (`BodyWorld`), and the block-local heads (the shadowing `Outer`
//!   among them) are filed otherwise.
//!
//! Each control must make the differential non-empty, or the test proves nothing.
//!
//! The same differential runs on a real crate as the command resolves it: blaze-scope
//! crate_scope.rs `block_scopes_differential`, which `blaze check --names-oracle
//! --block-scopes=bodies` runs once per crate (blaze-cli names/scope.rs). Needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`. Prints the verdicts first:
//! `impls differential: AGREE ..`, `impls control: ..`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_own::AVec;
use blaze_scope::body_scope::{BodyScope, BodyScopeReadError, BodyScopeView, ImplHeaders, ImplRow, bodies_impl_table, body_scope, impl_diff, modules_impl_table};
use blaze_scope::items_record::buckets::Filing;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::items_record::buckets::OwnNames;
use blaze_scope::items_record::impl_filing;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, Names, ReachPlant, SourceTexts, resolve_crates};
use blaze_scope::surface::BlockScopes;
use blaze_scope::task::{Par, Serial};

const USER: &str = "pub trait Show { fn show(&self) -> u8; }
pub struct Outer;
macro_rules! make_impl { ($t:ident) => { impl Show for $t { fn show(&self) -> u8 { 3 } } } }
pub fn nested() -> u8 {
    struct A;
    impl Show for A { fn show(&self) -> u8 { 1 } }
    let inner = {
        struct B;
        impl B { fn b(&self) -> u8 { 2 } }
        let innermost = {
            struct C;
            make_impl!(C);
            C.show()
        };
        innermost + B.b()
    };
    inner + A.show()
}
const _: () = { impl Outer { pub fn outer(&self) -> u8 { 4 } } };
pub fn block_macro() -> u8 {
    macro_rules! local_impl { ($t:ident) => { impl Show for $t { fn show(&self) -> u8 { 5 } } } }
    struct D;
    local_impl!(D);
    D.show()
}
pub fn block_use() -> u8 {
    use Show as S2;
    struct E;
    impl S2 for E { fn show(&self) -> u8 { 6 } }
    E.show()
}
pub fn shadowing() -> u8 {
    struct Outer;
    impl Outer { fn inner(&self) -> u8 { 7 } }
    let deeper = {
        struct Outer;
        impl Show for Outer { fn show(&self) -> u8 { 8 } }
        Outer.show()
    };
    deeper + Outer.inner()
}
macro_rules! check { ($c:expr) => { if !$c { extern crate alloc; let message = alloc::format!(\"{}\", 1); let _ = message; } } }
macro_rules! check_eq { ($a:expr, $b:expr) => {{ let left = $a; check!(left == $b); }} }
pub fn block_extern() -> u8 {
    check_eq!(1u8, 1u8);
    9
}
pub fn main() {}
";

/// The serial solve with D189's seam set to `Bodies`.
struct Bodies;

impl Par for Bodies {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn block_scopes(&self) -> BlockScopes {
        BlockScopes::Bodies
    }
}

#[test]
fn the_impl_table_is_the_same_under_both_strategies() {
    let alloc = blaze_own::testing::heap().alloc();
    let bump = blaze_own::testing::bump();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let files: &'static [(String, String)] = Box::leak(vec![("/user/src/main.rs".to_string(), USER.to_string())].into_boxed_slice());
    let user: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let texts = &inputs.texts;
    let modules: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates under Modules: {e}"))));
    let bodies: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    let (c_mod, c_bod) = (modules.crates()[0], bodies.crates()[0]);

    // Every held body solved as its own; a refusal is a verdict, never skipped.
    let deps = BodyDeps::of_names(bodies, 0, alloc);
    let prepares = Prepares::new_in(bump);
    let mut scopes: AVec<'static, BodyScope<'static>> = AVec::new_in(alloc);
    let mut refused = Vec::new();
    let mut deterministic = true;
    for held in c_bod.bodies.iter() {
        match body_scope(c_bod, &deps, &crates[0], 0, *held, texts, prepares, bump) {
            Ok(scope) => {
                let again = body_scope(c_bod, &deps, &crates[0], 0, *held, texts, prepares, bump).unwrap_or_else(|e| panic!("body_scope again: {e}"));
                let (one, two) = (scope.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")), again.write(alloc).unwrap_or_else(|e| panic!("write again: {e:?}")));
                deterministic &= one.as_slice() == two.as_slice() && scope.key == again.key;
                scopes.push(scope);
            }
            Err(e) => refused.push(format!("{e}")),
        }
    }

    let rows_mod = modules_impl_table(c_mod, alloc).unwrap_or_else(|e| panic!("the Modules impl table: {e:?}"));
    let rows_bod = bodies_impl_table(c_bod, &scopes, alloc).unwrap_or_else(|e| panic!("the Bodies impl table: {e:?}"));
    let diff = impl_diff(&rows_mod, &rows_bod, alloc);
    let in_bodies: usize = scopes.iter().map(|s| s.solved.tables.impls.len()).sum();
    println!(
        "impls differential: {}: modules {} bodies {} (from bodies {in_bodies}) equal {} only-modules {} only-bodies {} differ {}; held {} solved {} refused {}; records deterministic {deterministic}",
        if diff.is_empty() && refused.is_empty() { "AGREE" } else { "DIFFER" },
        rows_mod.len(),
        rows_bod.len(),
        diff.equal,
        diff.only_modules.len(),
        diff.only_bodies.len(),
        diff.differ.len(),
        c_bod.bodies.len(),
        scopes.len(),
        refused.len()
    );

    // Control 1: the record of the body with the most impls dropped (its rows leave the table).
    let biggest = scopes.iter().max_by_key(|s| s.solved.tables.impls.len()).map(|s| s.solved.tables.impls.iter().map(|i| i.key).collect::<Vec<_>>()).unwrap_or_default();
    let dropped: Vec<ImplRow> = rows_bod.iter().filter(|r| !biggest.contains(&r.key)).copied().collect();
    let dropped_diff = impl_diff(&rows_mod, &dropped, alloc);
    // Control 2: the body impls filed through the crate's names alone.
    let own = OwnNames(c_bod);
    let mut blind: Vec<ImplRow> = modules_impl_table(c_bod, alloc).unwrap_or_else(|e| panic!("the crate's own impls: {e:?}")).iter().copied().collect();
    for s in scopes.iter() {
        for i in s.solved.tables.impls.iter() {
            let filing = impl_filing(&own, c_bod.key, c_bod.edition, i, alloc).unwrap_or_else(|e| panic!("an impl's filing: {e:?}"));
            blind.push(ImplRow { key: i.key, module: i.module, filing });
        }
    }
    blind.sort_unstable_by_key(|r| r.key);
    let blind_diff = impl_diff(&rows_mod, &blind, alloc);
    println!("impls control: one body dropped: only-modules {}; filed without the body's record: differ {}", dropped_diff.only_modules.len(), blind_diff.differ.len());
    let headers = ImplHeaders::of(c_bod, &scopes, alloc);
    for (m, b) in blind_diff.differ.iter().take(4) {
        let header = headers.of_key(m.key).map_or_else(|| "an impl with no header".to_string(), |h| format!("`{h}`"));
        let filed = |f: &Filing| match f {
            Filing::At(_) => "filed".to_string(),
            Filing::Not(why) => format!("not filed ({why:?})"),
        };
        let same = m.filing == b.filing;
        println!("impls control: blind: {header}: modules {}; without the record {}{}", filed(&m.filing), filed(&b.filing), if same { "" } else { " (another bucket or none)" });
    }

    // The records read back in place (rule 26): each view answers every declared name as its
    // builder does. Controls: another body's record, a record cut short, an old version.
    let mut round_trips = 0usize;
    let mut first_bytes: Option<&'static [u8]> = None;
    for s in scopes.iter() {
        let bytes = blaze_scope::resolve::aligned(&s.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")), bump);
        let view = BodyScopeView::read(bytes, alloc).unwrap_or_else(|e| panic!("a written body scope reads back: {e}"));
        assert_eq!(s.differs_from(&view), None, "a body scope read back answers as its builder");
        round_trips += 1;
        first_bytes.get_or_insert(bytes);
    }
    let first = first_bytes.expect("at least one body scope");
    let other = scopes.last().map(|s| BodyScopeView::read(first, alloc).map(|v| s.differs_from(&v)));
    let short = BodyScopeView::read(&first[..first.len() - 1], alloc).err();
    let mut old = first.to_vec();
    old[4..8].copy_from_slice(&1u32.to_le_bytes());
    let old_version = BodyScopeView::read(blaze_scope::resolve::aligned(&old, bump), alloc).err();
    println!("impls records: {round_trips} read back equal; control: another body's {other:?}, cut short {short:?}, version 1 {old_version:?}");
    if scopes.len() > 1 {
        assert!(matches!(other, Some(Ok(Some(_)))), "another body's record must differ, or the round trip proves nothing");
    }
    assert!(short.is_some(), "a record cut short is refused, never read");
    assert_eq!(old_version, Some(BodyScopeReadError::Version { found: 1 }), "an old record is refused by its version");

    // proptest's `prop_assert!` (sugar.rs:756) through `prop_assert_eq!`'s block: the
    // `extern crate alloc;` of a block an EXPANSION wrote binds `alloc` in that block, where the
    // `alloc::format!` of its `let` initializer resolves. The twin: the block one out (the
    // `{ let left = ..; }` block `check_eq!` wrote) does not bind it, so the same path written
    // there is not found.
    let ty = blaze_surface::Namespace::Types;
    let binds = |s: &BodyScope<'_>, m: blaze_id::ItemKey| s.blocks.iter().find(|b| b.module == m).map(|b| b.scope.get(ty, "alloc").is_some());
    let mut extern_blocks = 0usize;
    let mut twin_found = 0usize;
    for s in scopes.iter() {
        for b in s.blocks.iter().filter(|b| b.declared.iter().any(|&(ns, name, _)| ns == ty && name == "alloc")) {
            extern_blocks += 1;
            assert_eq!(binds(s, b.module), Some(true), "a block's `extern crate alloc;` binds `alloc` in that block");
            if let Some(out) = b.parent.and_then(|p| binds(s, p)) {
                twin_found += usize::from(out);
            }
        }
    }
    println!("impls block extern: {extern_blocks} block(s) an expansion wrote bind `alloc`; the block one out binds it in {twin_found}");
    assert!(extern_blocks >= 1, "the `if` block `check!` writes inside `check_eq!`'s block is a block of `block_extern`'s record, binding `alloc`");
    assert_eq!(twin_found, 0, "the twin: one block out, `alloc` is not bound (Rust: not found)");

    assert!(refused.is_empty(), "every held body is solved: {refused:?}");
    assert!(diff.is_empty(), "the impl tables differ: {diff:?}");
    assert!(in_bodies >= 8, "the eight impls in bodies (A, B, C, D, E, two `Outer`s, the `const _` one) are in the Bodies table: {in_bodies}");
    assert!(deterministic, "two builders of one body's key write equal bytes (rule 25)");
    assert!(!scopes.is_empty() && !dropped_diff.only_modules.is_empty(), "control 1: a dropped body's impls must be missing, or this test proves nothing");
    assert!(!blind_diff.is_empty(), "control 2: without the body's record the block-local heads must be filed otherwise, or this test proves nothing");
}

/// Glob imports a body's block solve meets (tempo, 21 Nov 04:10: every one of the 1,790 refused
/// bodies was "the one-group solve is refused: GlobSource"; serde_core's modules are all written
/// by `crate_root!()`, each opening with `use crate::lib::*;`, and libc's root holds `pub use
/// unix::*;` of a module `cfg_if!` wrote). Each body below writes an impl whose trait or type is
/// reached through a glob:
/// - `made_glob`: its module globs `crate::lib`, a module an expansion wrote outside the body
///   (the tempo case; no `mod` line names it);
/// - `root_glob`: the root's own `pub use self::lib::*;` of that module (libc's root);
/// - `super_glob`: a block's `use super::*;` (the crate root, from `over_lib`);
/// - `enum_glob`: a block's `use Kind::*;` of an enum outside the body;
/// - `sibling_glob`: an inner block's `use m::*;` of a `mod` its outer block declares (a module
///   of the body's own solve);
/// - `glob_cycle`: `a` and `a::b` glob each other inside one block (a cycle in the body's group).
const GLOBS: &str = "pub trait Show { fn show(&self) -> u8; }
macro_rules! make_lib { () => { pub mod lib { pub trait LibTr {} pub struct Lib; } } }
make_lib!();
pub use self::lib::*;
pub enum Kind { Alpha, Beta }
pub mod over_lib {
    use crate::lib::*;
    pub fn made_glob() -> u8 { struct L; impl LibTr for L {} let _ = Lib; 1 }
    pub fn super_glob() -> u8 { use super::*; struct S; impl Show for S { fn show(&self) -> u8 { 2 } } S.show() }
}
pub fn root_glob() -> u8 { struct R; impl LibTr for R {} 3 }
pub fn enum_glob() -> u8 { use Kind::*; struct K; impl Show for K { fn show(&self) -> u8 { match Alpha { Alpha => 4, Beta => 0 } } } K.show() }
pub fn sibling_glob() -> u8 {
    mod m { pub struct M; pub trait MTr {} }
    let inner = { use m::*; impl MTr for M {} 5 };
    inner
}
pub fn glob_cycle() -> u8 {
    mod a { pub mod b { pub use super::*; pub struct B; } pub use self::b::*; pub struct A; }
    use a::*;
    impl Show for B { fn show(&self) -> u8 { 6 } }
    impl Show for A { fn show(&self) -> u8 { 7 } }
    B.show() + A.show()
}
pub fn main() {}
";

/// The planted bug (rule 23): the same texts, with a body solve placing no finished module
/// ([`ReachPlant::Unplaced`]), as before the fix. The glob bodies must be refused again.
struct Unplaced<'t>(&'t blaze_ladder::crates::Texts<'t>);

impl<'t> SourceTexts<'t> for Unplaced<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        self.0.read(krate, path)
    }

    fn reach_plant(&self) -> ReachPlant {
        ReachPlant::Unplaced
    }
}

/// `text` as the user crate over the installed toolchain, its names solved under both
/// strategies: (crates, the `Modules` names, the `Bodies` names, the texts).
fn both_strategies(text: &'static str) -> (&'static [CrateInput<'static>], &'static Names<'static>, &'static Names<'static>, &'static blaze_ladder::crates::Texts<'static>) {
    let bump = blaze_own::testing::bump();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let files: &'static [(String, String)] = Box::leak(vec![("/user/src/main.rs".to_string(), text.to_string())].into_boxed_slice());
    let user: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let texts = &inputs.texts;
    let modules: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates under Modules: {e}"))));
    let bodies: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    (crates, modules, bodies, texts)
}

#[test]
fn a_body_s_globs_reach_modules_an_expansion_wrote_enums_and_its_own_modules() {
    let alloc = blaze_own::testing::heap().alloc();
    let bump = blaze_own::testing::bump();
    let (crates, modules, bodies, texts) = both_strategies(GLOBS);
    let (c_mod, c_bod) = (modules.crates()[0], bodies.crates()[0]);
    let deps = BodyDeps::of_names(bodies, 0, alloc);
    let prepares = Prepares::new_in(bump);
    let mut scopes: AVec<'static, BodyScope<'static>> = AVec::new_in(alloc);
    let mut refused: Vec<String> = Vec::new();
    // Each fixture body by the struct only it declares.
    let marks = ["struct L;", "struct R;", "struct S;", "struct K;", "mod m ", "mod a "];
    let mut solved_marks: Vec<&str> = Vec::new();
    for held in c_bod.bodies.iter() {
        match body_scope(c_bod, &deps, &crates[0], 0, *held, texts, prepares, bump) {
            Ok(scope) => {
                solved_marks.extend(marks.iter().copied().filter(|m| held.src.text.contains(m)));
                scopes.push(scope);
            }
            Err(e) => refused.push(format!("{e}")),
        }
    }
    let rows_mod = modules_impl_table(c_mod, alloc).unwrap_or_else(|e| panic!("the Modules impl table: {e:?}"));
    let rows_bod = bodies_impl_table(c_bod, &scopes, alloc).unwrap_or_else(|e| panic!("the Bodies impl table: {e:?}"));
    let diff = impl_diff(&rows_mod, &rows_bod, alloc);
    let headers = ImplHeaders::of(c_bod, &scopes, alloc);
    let in_bodies: Vec<String> = scopes.iter().flat_map(|s| s.impls.iter().map(|i| i.header.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    println!(
        "globs differential: {}: held {} solved {} refused {}; modules {} bodies {} only-modules {} only-bodies {} differ {}; impls in bodies {in_bodies:?}",
        if diff.is_empty() && refused.is_empty() { "AGREE" } else { "DIFFER" },
        c_bod.bodies.len(),
        scopes.len(),
        refused.len(),
        rows_mod.len(),
        rows_bod.len(),
        diff.only_modules.len(),
        diff.only_bodies.len(),
        diff.differ.len()
    );
    for r in &refused {
        println!("globs differential: refused: {r}");
    }
    for r in diff.only_modules.iter() {
        println!("globs differential: only under Modules: {}", headers.of_key(r.key).map_or_else(|| "an impl with no header".to_string(), |h| h.to_string()));
    }

    // The control: the same bodies, no finished module placed. The bodies whose globs reach a
    // module `make_lib!()` wrote (`made_glob`, `root_glob`) are refused at that glob again.
    let unplaced = Unplaced(texts);
    let mut planted: Vec<String> = Vec::new();
    for held in c_bod.bodies.iter() {
        if let Err(e) = body_scope(c_bod, &deps, &crates[0], 0, *held, &unplaced, prepares, bump) {
            planted.push(format!("{e}"));
        }
    }
    let planted_glob = planted.iter().filter(|e| e.contains("GlobSource")).count();
    println!("globs control: no module placed: refused {} ({planted_glob} at a glob source)", planted.len());
    for e in planted.iter().take(3) {
        println!("globs control: refused: {e}");
    }

    assert!(refused.is_empty(), "no body is refused at a glob (the tempo refusal `GlobSource`): {refused:?}");
    assert!(planted_glob >= 1, "control: with no module placed a body's glob of `lib` is refused (`GlobSource`), or this test proves nothing: {planted:?}");
    for m in marks {
        assert!(solved_marks.contains(&m), "the body declaring `{m}` is held and solved as its own: {solved_marks:?}");
    }
    for want in ["LibTr for L", "LibTr for R", "Show for S", "Show for K", "MTr for M", "Show for B", "Show for A"] {
        assert!(in_bodies.iter().any(|h| h.ends_with(want)), "`impl {want}` is written in its body's record: {in_bodies:?}");
    }
    assert!(diff.is_empty(), "the impl tables differ: {diff:?}");

    // The enum glob is the block's own edge, its source the enum; the twin: a block with no
    // `use Kind::*;` (`root_glob`'s) has no edge of an enum.
    let enum_edges = |s: &BodyScope<'_>| s.blocks.iter().flat_map(|b| b.scope.globs.iter()).filter(|g| matches!(g.source.kind, blaze_scope::DefKind::Enum)).count();
    let with_kind = scopes.iter().find(|s| s.locals.iter().any(|l| l.name == "K")).map(enum_edges);
    let without = scopes.iter().find(|s| s.locals.iter().any(|l| l.name == "R")).map(enum_edges);
    println!("globs enum: block edges of an enum in `enum_glob` {with_kind:?}, in `root_glob` {without:?}");
    assert_eq!(with_kind, Some(1), "`use Kind::*;` is one glob edge of `enum_glob`'s block, its source the enum");
    assert_eq!(without, Some(0), "the twin: `root_glob`'s block globs no enum");
}
