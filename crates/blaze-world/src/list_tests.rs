//! The world's crate list is blaze-scope's units list (crate-list-one-home, census 13 brief 5):
//! a workspace member's proc macro names its own dependencies on the macro side, or, with no
//! `wasm32-wasip1` cut on the shelf, the world carries that proc macro's typed refusal.
//!
//! The Reference, "Extern prelude": crates "provided to the compiler (as with the `--extern`
//! flag with `rustc`) are added to the extern prelude"; a proc macro is compiled for the target
//! it runs on (blaze: `wasm32-wasip1`, AGENTS.md rule 10), so the `dep` it names is `dep` built
//! for that target ("Conditional compilation": `target_arch` is "the target's CPU
//! architecture"), never the program target's `dep`. Before this piece the world's own list
//! kept every proc macro WITHOUT its dependencies (lib.rs `world`: "A proc macro's dependencies
//! are the host's build: not followed"), so `pm`'s extern prelude held no `dep`.

use std::boxed::Box as StdBox;
use std::collections::BTreeMap as StdMap;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use blaze_fetch::PackView;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::units::UnitsError;
use blaze_toolchain::{Shelf, ToolchainId};
use blaze_unit_graph::{CfgAtom, Resolver, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::{BlazeEdition, Edition as Syntax, PackageEdition};

use super::{Side, open_list, world};
use crate::{Packages, Place, Tree, World, WorldError};

#[path = "check_scope_macro_tests.rs"]
mod check_scope_macro_tests;

const TRIPLE: &str = "aarch64-apple-darwin";
const MACRO: &str = "wasm32-wasip1";
const TARGET_CFG: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];
const MACRO_CFG: &[(&str, Option<&str>)] = &[("target_arch", Some("wasm32")), ("target_family", Some("wasm")), ("target_os", Some("wasi"))];
const RUST21: PackageEdition = PackageEdition { syntax: Syntax::Edition2021, blaze: None };
const BLAZE: PackageEdition = PackageEdition { syntax: Syntax::Edition2024, blaze: Some(BlazeEdition::Blaze2026_11) };

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn b() -> blaze_own::Bump<'static> {
    blaze_own::testing::bump()
}

fn t(s: &str) -> AString<'static> {
    AString::from_str_in(s, a())
}

/// A shelf in memory, by hex name.
struct Memory(StdMap<StdString, StdVec<u8>>);

impl Shelf for Memory {
    type Error = &'static str;
    type Bytes = StdVec<u8>;
    fn place(&self) -> &str {
        "/shelf"
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, &'static str> {
        Ok(self.0.get(name).map(|b| b.len() as u64))
    }
    fn read(&mut self, name: &str, _size: u64) -> Result<StdVec<u8>, &'static str> {
        self.0.get(name).cloned().ok_or("gone")
    }
}

/// The fixture's toolchain cut for the target and, when `macro_side`, for `wasm32-wasip1`.
fn shelf(macro_side: bool) -> (ToolchainId, Memory) {
    let (id, files) = if macro_side {
        crate::fixture::shelf_with_macro_side(TRIPLE, crate::fixture::STD, TARGET_CFG, MACRO_CFG, a())
    } else {
        crate::fixture::shelf(TRIPLE, crate::fixture::STD, TARGET_CFG, a())
    };
    (id, Memory(files.into_iter().collect()))
}

/// `app` (member) depends on `dep` (registry) and on `pm`, a member proc macro depending on
/// `dep` too.
const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["dep", "pm"]

[[package]]
name = "dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "pm"
version = "0.1.0"
dependencies = ["dep"]
"#;

fn lock() -> &'static Lock<'static> {
    StdBox::leak(StdBox::new(Lock::read(LOCK.as_bytes(), a()).expect("the lock reads")))
}

fn unit(lock: &Lock<'static>, name: &str, kind: UnitKind<'static>, for_host: bool, deps: &[(&str, u32)]) -> Unit<'static> {
    let package = lock.named(name).next().expect("in the lock");
    let mut cfg = AVec::new_in(a());
    cfg.push(CfgAtom::flag("debug_assertions", a()));
    cfg.push(CfgAtom::pair("target_os", "macos", a()));
    cfg.push(CfgAtom::flag("unix", a()));
    if kind == UnitKind::ProcMacro {
        cfg.push(CfgAtom::flag("proc_macro", a()));
    }
    let mut d = AVec::new_in(a());
    for (n, u) in deps {
        d.push(UnitDep { name: t(n), unit: UnitId(*u) });
    }
    Unit {
        package,
        name: t(name),
        version: t(if lock.package(package).source_text.is_empty() { "0.1.0" } else { "1.0.0" }),
        kind,
        for_host,
        edition: RUST21,
        crate_name: t(name),
        root: t("src/lib.rs"),
        features: AVec::new_in(a()),
        cfg,
        deps: d,
        build_script: None,
        links: None,
    }
}

/// 0 `dep` for the target, 1 `dep` for the host, 2 `pm` (proc macro, on the host's `dep`),
/// 3 `app` (on the target's `dep` and on `pm`).
fn graph(lock: &Lock<'static>) -> &'static UnitGraph<'static> {
    let mut units = AVec::new_in(a());
    units.push(unit(lock, "dep", UnitKind::Lib, false, &[]));
    units.push(unit(lock, "dep", UnitKind::Lib, true, &[]));
    units.push(unit(lock, "pm", UnitKind::ProcMacro, true, &[("dep", 1)]));
    units.push(unit(lock, "app", UnitKind::Lib, false, &[("dep", 0), ("pm", 2)]));
    StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }))
}

/// [`graph`]'s units in the reverse order (unit `i` is [`graph`]'s `3 - i`): one graph, its
/// units numbered otherwise.
fn graph_reversed(lock: &Lock<'static>) -> &'static UnitGraph<'static> {
    let mut units = AVec::new_in(a());
    units.push(unit(lock, "app", UnitKind::Lib, false, &[("dep", 3), ("pm", 1)]));
    units.push(unit(lock, "pm", UnitKind::ProcMacro, true, &[("dep", 2)]));
    units.push(unit(lock, "dep", UnitKind::Lib, true, &[]));
    units.push(unit(lock, "dep", UnitKind::Lib, false, &[]));
    StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }))
}

/// `app` and `pm` in the checkout at their own directories; `dep` a fetched pack.
struct Places {
    lock: &'static Lock<'static>,
    dep: &'static [u8],
}

impl Packages<'static> for Places {
    fn place(&mut self, package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        match self.lock.package(package).name.as_str() {
            "app" => Ok(Place::Checkout { dir: "app" }),
            "pm" => Ok(Place::Checkout { dir: "pm" }),
            "hello" => Ok(Place::Checkout { dir: "hello" }),
            "helper" => Ok(Place::Checkout { dir: "helper" }),
            _ => Ok(Place::Pack { view: PackView::open_verified(self.dep).expect("a pack"), dir: "" }),
        }
    }
}

/// The checkout: nothing is read while the world is made.
struct NoFiles;

impl Tree for NoFiles {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

/// The world of the workspace over the fixture's shelf, cut for the macro side or not.
fn made(macro_side: bool) -> World<'static> {
    let lock = lock();
    let graph = graph(lock);
    let dep = crate::fixture::pack(&[("src/lib.rs", "pub struct Expr;\n")], a());
    let mut places = Places { lock, dep: StdBox::leak(dep.into_boxed_slice()) };
    let (id, mut shelf) = shelf(macro_side);
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
}

/// With the `wasm32-wasip1` cut: `pm`'s extern `dep` is a crate of the macro side, with that
/// target's cfg, and not the target's `dep`, which `app` keeps; `pm`'s `proc_macro` is the
/// macro side's too. Prints `crate list one home: pm's dep is the macro side's (crate <n>)`.
/// Without the fix (the world's own list): `pm` had no deps at all, so `dep_of(pm)` is `None`.
#[test]
fn a_member_proc_macro_has_its_dependency_on_the_macro_side() {
    let w = made(true);
    assert!(w.refused.is_empty(), "with the cut, nothing is refused: {:?}", w.refused);
    let pm = w.crate_of_unit[2].expect("pm is a crate") as usize;
    let app = w.crate_of_unit[3].expect("app is a crate") as usize;
    let target_dep = w.crate_of_unit[0].expect("the target's dep is a crate") as usize;
    let dep_of = |krate: usize, name: &str| w.crates[krate].input.deps.iter().find(|(n, _)| *n == name).map(|(_, at)| *at);
    let side_dep = dep_of(pm, "dep").expect("pm names `dep`");
    assert_eq!(w.crates[side_dep].input.name, "dep");
    assert_eq!(w.crates[side_dep].side, Side::Macro, "pm's `dep` is the macro side's");
    assert_ne!(side_dep, target_dep, "the macro side's `dep` is a crate of its own");
    assert!(w.crates[side_dep].input.cfg.contains(&("target_arch", Some("wasm32"))), "the macro side's cfg is wasm32-wasip1's: {:?}", w.crates[side_dep].input.cfg);
    assert!(!w.crates[target_dep].input.cfg.contains(&("target_arch", Some("wasm32"))), "the target's cfg is not wasm32's");
    assert_eq!(dep_of(app, "dep"), Some(target_dep), "app names the target's `dep`");
    let pm_proc_macro = dep_of(pm, "proc_macro").expect("pm names `proc_macro`");
    assert_eq!(w.crates[pm_proc_macro].side, Side::Macro, "pm's `proc_macro` is the macro side's, not the target toolchain's");
    std::println!("crate list one home: pm's dep is the macro side's (crate {side_dep})");
}

/// Without the cut: the world is still made; `pm` is refused by name with
/// `NoMacroToolchain`, carried in the world at `pm`'s own index, and is given no crate of
/// another side (no `dep`, and no target `proc_macro` in place of the macro side's).
/// Prints `crate list one home: pm refused: <the refusal's words>`.
#[test]
fn without_the_macro_cut_the_world_carries_the_refusal() {
    let w = made(false);
    let pm = w.crate_of_unit[2].expect("pm is still a crate") as usize;
    assert_eq!(w.refused.len(), 1, "{:?}", w.refused);
    assert_eq!(w.refusal(pm), Some(&UnitsError::NoMacroToolchain { krate: "pm", target: MACRO }), "pm's refusal names it");
    assert!(w.crates[pm].input.deps.is_empty(), "the refused proc macro is given no crate of another side: {:?}", w.crates[pm].input.deps);
    assert!(w.crates.iter().all(|c| c.side == Side::Target), "no macro-side crate without the cut");
    std::println!("crate list one home: pm refused: {}", w.refused[0].why);
}

/// `hello` alone, a workspace member of no dependency.
const HELLO: &str = r#"version = 4

[[package]]
name = "hello"
version = "0.1.0"
"#;

/// The world of `hello` alone, `edition` its edition, over the fixture's shelf whose std
/// depends on `cfg_if`.
fn hello(edition: PackageEdition) -> World<'static> {
    hello_on(edition, crate::fixture::shelf_with_std_dep)
}

/// The fixture's shelf, as one of its makers gives it.
type ShelfOf = fn(&'static str, &str, &'static [(&'static str, Option<&'static str>)], Alloc<'_>) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>);

/// The world of `hello` alone, `edition` its edition, over the shelf `shelf_of` makes.
fn hello_on(edition: PackageEdition, shelf_of: ShelfOf) -> World<'static> {
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(HELLO.as_bytes(), a()).expect("the lock reads")));
    let mut member = unit(lock, "hello", UnitKind::Lib, false, &[]);
    member.edition = edition;
    let mut units = AVec::new_in(a());
    units.push(member);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }));
    let mut places = Places { lock, dep: &[] };
    let (id, files) = shelf_of(TRIPLE, crate::fixture::STD, TARGET_CFG, a());
    let mut shelf = Memory(files.into_iter().collect());
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
}

/// A crate's implicit sysroot dependency is a fact of its own text, an edge of its shelf row
/// (blaze_toolchain::implied; the Rust Reference, "Extern prelude": `std` is added "as long as
/// the `no_std` attribute is not specified in the crate root"). So std is opened iff a crate
/// of the closure names it:
/// - a blaze-edition program over a shelf whose bstd closure is all `#![no_std]` leaves std
///   (and std's `cfg_if`) unopened;
/// - the same program over a shelf where bstd depends on `blaze_ids`, whose root uses std's
///   prelude (`Vec<u64>`; row `std`, edge `("std", 2)`), opens std and cfg_if, and blaze_ids's
///   world crate holds the edge to the world's std (what its `Vec` resolves through);
/// - a Rust 2021 program opens std by its own need.
/// CONTROL: -4's rule (std always) fails the first assertion; the rule before -4 (std only
/// for a Rust-edition unit, no edges) fails the second (blaze_ids `Vec` NotFound on the real
/// run, recpipe5/time5.sh). Prints
/// `sysroot edge: no_std closure leaves std unopened (<a> toolchain crates); blaze_ids opens std (<b>)`.
#[test]
fn std_is_opened_iff_a_crate_of_the_closure_names_it() {
    let named = |w: &World<'static>, name: &str| w.crates.iter().any(|c| c.input.name == name);
    let at = |w: &World<'static>, name: &str| w.crates.iter().position(|c| c.input.name == name);
    let toolchain = |w: &World<'static>| w.crates.iter().filter(|c| matches!(c.origin, crate::Origin::Toolchain(_))).count();
    let blaze = hello(BLAZE);
    assert!(named(&blaze, "bstd"), "a blaze-edition program opens bstd");
    assert!(!named(&blaze, "std"), "no crate of a no_std closure names std: it stays unopened");
    assert!(!named(&blaze, "cfg_if"), "nor std's own dependency");
    let ids = hello_on(BLAZE, crate::fixture::shelf_with_bstd_naming_std);
    assert!(named(&ids, "std") && named(&ids, "cfg_if"), "blaze_ids names std: std and its closure are opened");
    let std_at = at(&ids, "std").expect("std is opened");
    let ids_at = at(&ids, "blaze_ids").expect("blaze_ids is opened");
    assert!(ids.crates[ids_at].input.deps.iter().any(|(name, d)| *name == "std" && *d == std_at), "blaze_ids's `std` is the world's std");
    let rust = hello(RUST21);
    assert!(named(&rust, "std") && named(&rust, "cfg_if"), "a Rust 2021 program opens std by its own need");
    std::println!("sysroot edge: no_std closure leaves std unopened ({} toolchain crates); blaze_ids opens std ({})", toolchain(&blaze), toolchain(&ids));
}

/// check_scope.rs: an owner's bodies are checked against ONE side. `app` sees the target's
/// crates and `pm` for its macros only; `pm` (a proc macro, checked on the macro side, rule 10)
/// sees its side's crates alone; each has ONE `core` among its lang crates, its own side's.
/// CONTROL: the rule before (every owner seeded with the target sysroot, every dependency
/// followed, lang crates by name) gives `pm` both sides' cores, the class of pm-small's "the
/// lang item `AsyncGenFinished` is declared twice" (2026-11-16 19:51). Prints `check scope:
/// app's core is crate <a>, pm's crate <b>; the old rule gave pm <n> cores`.
#[test]
fn an_owner_sees_one_side() {
    use crate::check_scope::{Seen, check_scope};
    let w = made(true);
    let pm = w.crate_of_unit[2].expect("pm is a crate") as usize;
    let app = w.crate_of_unit[3].expect("app is a crate") as usize;
    let cores_of = |owner: usize| -> StdVec<usize> {
        let scope = check_scope(&w, owner, false, a()).unwrap_or_else(|e| panic!("{e}"));
        let readers = scope.readers();
        scope.lang_places().map(|p| readers[p].0).filter(|at| w.crates[*at].input.name == "core").collect()
    };
    let app_scope = check_scope(&w, app, false, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(app_scope.readers()[0], (app, Seen::Owner), "the owner is first");
    assert!(app_scope.readers().contains(&(pm, Seen::MacrosOnly)), "app names pm for its macros only: {:?}", app_scope.readers());
    assert!(app_scope.crates().all(|at| at == pm || w.crates[at].side == Side::Target), "app sees no macro-side crate: {:?}", app_scope.readers());
    let pm_scope = check_scope(&w, pm, false, a()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pm_scope.side(), Side::Macro, "a proc macro's bodies are checked on the macro side");
    assert!(pm_scope.crates().skip(1).all(|at| w.crates[at].side == Side::Macro), "pm sees the macro side alone: {:?}", pm_scope.readers());
    let (app_cores, pm_cores) = (cores_of(app), cores_of(pm));
    assert_eq!(app_cores, [w.sysroot.core.expect("the target's core").0 as usize], "app's one core is the target's");
    assert_eq!(pm_cores.len(), 1, "pm has one core: {pm_cores:?}");
    assert_eq!(w.crates[pm_cores[0]].side, Side::Macro, "pm's core is the macro side's");
    // CONTROL: the rule before crate-list-one-home-3 and this, for `pm`.
    let mut old: StdVec<usize> = StdVec::from([pm]);
    old.extend([w.sysroot.core, w.sysroot.alloc, w.sysroot.std, w.sysroot.bstd].into_iter().flatten().map(|id| id.0 as usize).filter(|at| *at != pm));
    let mut next = 0;
    while next < old.len() {
        let at = old[next];
        next += 1;
        for (_, dep) in w.crates[at].input.deps.iter() {
            if !old.contains(dep) {
                old.push(*dep);
            }
        }
    }
    let old_cores = old.iter().filter(|at| w.crates[**at].input.name == "core").count();
    assert_eq!(old_cores, 2, "the old rule joined both sides' cores for pm");
    std::println!("check scope: app's core is crate {}, pm's crate {}; the old rule gave pm {old_cores} cores", app_cores[0], pm_cores[0]);
}

/// The workspace of check_scope.rs's sides: `app` (member) depends on `helper` (member) and on
/// `pm` (member proc macro), and has a build script depending on `bdep` (registry); `helper`
/// depends on `reg` (registry); `pm` depends on `helper` and `reg`. tempo's shape:
/// `tempo-precompiles-macros` is a member proc macro on `syn` and `alloy`, both also built for
/// the target.
const SIDES: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["bdep", "helper", "pm"]

[[package]]
name = "bdep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "helper"
version = "0.1.0"
dependencies = ["reg"]

[[package]]
name = "pm"
version = "0.1.0"
dependencies = ["helper", "reg"]

[[package]]
name = "reg"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

/// [`SIDES`]' units as blaze-unit-graph makes them with tests asked (`blaze check`), resolver
/// 2: 0 `reg` for the target with feature `f` (which `app` turns on), 1 `reg` for the host
/// without it, 2 `helper` for the target, 3 `helper` for the host, 4 `pm` (proc macro, on the
/// host's `helper` and `reg`, its build script unit 9), 5 `pm`'s library test harness (a HOST
/// unit on the host's `helper` and `reg`, as the proc macro: resolve.rs `Kind::TestLib`), 6
/// `app` (on the target's `helper` and on `pm`, its build script unit 7), 7 `app`'s build
/// script (on the host's `bdep`), 8 `bdep` for the host, 9 `pm`'s build script.
fn sides_world() -> World<'static> {
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(SIDES.as_bytes(), a()).expect("the lock reads")));
    let mut units = AVec::new_in(a());
    let mut reg = unit(lock, "reg", UnitKind::Lib, false, &[]);
    reg.features.push(t("f"));
    reg.cfg.push(CfgAtom::pair("feature", "f", a()));
    units.push(reg);
    units.push(unit(lock, "reg", UnitKind::Lib, true, &[]));
    units.push(unit(lock, "helper", UnitKind::Lib, false, &[("reg", 0)]));
    units.push(unit(lock, "helper", UnitKind::Lib, true, &[("reg", 1)]));
    let mut pm = unit(lock, "pm", UnitKind::ProcMacro, true, &[("helper", 3), ("reg", 1)]);
    pm.build_script = Some(UnitId(9));
    units.push(pm);
    let mut harness = unit(lock, "pm", UnitKind::Test { bin: None }, true, &[("helper", 3), ("reg", 1)]);
    harness.cfg.push(CfgAtom::flag("test", a()));
    harness.build_script = Some(UnitId(9));
    units.push(harness);
    let mut app = unit(lock, "app", UnitKind::Lib, false, &[("helper", 2), ("pm", 4)]);
    app.build_script = Some(UnitId(7));
    units.push(app);
    let mut script = unit(lock, "app", UnitKind::BuildScript, true, &[("bdep", 8)]);
    script.root = t("build.rs");
    units.push(script);
    units.push(unit(lock, "bdep", UnitKind::Lib, true, &[]));
    let mut pm_script = unit(lock, "pm", UnitKind::BuildScript, true, &[]);
    pm_script.root = t("build.rs");
    units.push(pm_script);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }));
    let reg = crate::fixture::pack(&[("src/lib.rs", "pub struct Expr;\n")], a());
    let mut places = Places { lock, dep: StdBox::leak(reg.into_boxed_slice()) };
    let (id, mut shelf) = shelf(true);
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
}

/// The crate `helper` of `side` in `w`.
fn helpers_of(w: &World<'static>, side: Side) -> usize {
    (0..w.crates.len()).find(|c| w.crates[*c].input.name == "helper" && w.crates[*c].side == side).unwrap_or_else(|| panic!("no helper on {side:?}"))
}

/// check_scope.rs `check_side`: every owner of [`SIDES`] has a scope on the side it is built
/// for, and every crate of it is that side's (a proc macro aside, seen for its macros):
/// - `pm` (a member proc macro) on the macro side, over the macro side's `helper` and `reg`;
/// - `pm`'s library test harness on the macro side too, a host unit of the graph as the proc
///   macro is (cargo builds it for the host with host features); its row says Macro, its cfg
///   is the side's (no host `target_os`, with `test`), and its build-script run is the macro
///   side's (`pm`'s subject on Macro), as the proc macro's;
/// - feature `f` of `reg`, which `app` turns on, is on the target side's `reg` only: the macro
///   side's one copy of `reg` (shared by `pm`, the harness and `helper`) has no `f`;
/// - `helper`, built for both sides: two crates under two keys, each an owner checked on its
///   side over its side's `reg`;
/// - `app` on the target side, `pm` seen for its macros only; its build script and the
///   build-dependency `bdep` are no crate of the world (no owner).
/// Prints `check scope sides: <n> owners, harness on Macro (its row Target), helper's owners on
/// [<sides>]`.
#[test]
fn every_owner_is_checked_on_the_side_it_is_built_for() {
    use crate::check_scope::{Seen, check_scope, check_side};
    let w = sides_world();
    assert!(w.refused.is_empty(), "{:?}", w.refused);
    let at = |unit: u32| w.crate_of_unit[unit as usize].unwrap_or_else(|| panic!("unit {unit} is a target-side crate")) as usize;
    let (helper, pm, harness, app) = (at(2), at(4), at(5), at(6));
    // Every owner: a scope whose crates are all its check side's, a proc macro aside; one core.
    let mut sides_of_helper: StdVec<Side> = StdVec::new();
    for owner in 0..w.crates.len() {
        if matches!(w.crates[owner].origin, crate::Origin::Toolchain(_)) {
            continue;
        }
        let scope = check_scope(&w, owner, false, a()).unwrap_or_else(|e| panic!("owner `{}`: {e}", w.crates[owner].input.name));
        let side = scope.side();
        for &(seen, how) in scope.readers() {
            // The owner's own row may say another side (the harness); a linked crate may not.
            assert!(!matches!(how, Seen::Linked { .. }) || w.crates[seen].side == side, "owner `{}` on {side:?} sees `{}` of {:?}", w.crates[owner].input.name, w.crates[seen].input.name, w.crates[seen].side);
        }
        let cores = scope.lang_places().filter(|p| w.crates[scope.readers()[*p].0].input.name == "core").count();
        assert_eq!(cores, 1, "owner `{}` has one core", w.crates[owner].input.name);
        if w.crates[owner].input.name == "helper" {
            sides_of_helper.push(side);
        }
    }
    let side_of = |owner: usize| check_side(&w, owner).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(side_of(pm), Side::Macro, "a member proc macro is checked on the macro side");
    assert_eq!(side_of(harness), Side::Macro, "its library test harness is the same code, built for the macro side");
    assert_eq!(w.crates[harness].side, Side::Macro, "the harness's row says Macro");
    assert_eq!(w.crates[pm].side, Side::Macro, "the proc macro's row says Macro");
    assert!(w.crates[harness].input.deps.iter().all(|(_, d)| w.crates[*d].side == Side::Macro), "the list points the harness's edges at the macro side");
    // The member proc macro and its harness: the side's cfg, and the side's build-script run.
    for (code, test) in [(pm, false), (harness, true)] {
        let c = &w.crates[code];
        assert!(!c.input.cfg.contains(&("target_os", Some("macos"))), "`{}` has the macro side's cfg, not the host unit's: {:?}", c.input.name, c.input.cfg);
        assert_eq!(c.input.cfg.contains(&("test", None)), test, "`test` is the harness's only: {:?}", c.input.cfg);
        assert!(c.input.cfg.contains(&("proc_macro", None)) != test, "`proc_macro` is the proc macro's: {:?}", c.input.cfg);
        let subject = crate::script_subject("pm", "0.1.0", Side::Macro, &[], a());
        assert_eq!(c.script, crate::Script::Read { subject }, "`{}` reads pm's build script run on the macro side", c.input.name);
    }
    // Resolver 2: `app`'s feature `f` of `reg` is the target side's only; the macro side has
    // one `reg`, shared by every crate of it, without `f`.
    let regs: StdVec<usize> = (0..w.crates.len()).filter(|c| w.crates[*c].input.name == "reg").collect();
    assert_eq!(regs.len(), 2, "one `reg` per side: {regs:?}");
    for &r in regs.iter() {
        let has_f = w.crates[r].input.features.contains(&"f") && w.crates[r].input.cfg.contains(&("feature", Some("f")));
        assert_eq!(has_f, w.crates[r].side == Side::Target, "`reg` on {:?}: feature `f` iff the target's", w.crates[r].side);
    }
    let side_reg = regs.iter().copied().find(|r| w.crates[*r].side == Side::Macro).expect("the macro side's reg");
    for asker in [pm, harness, helpers_of(&w, Side::Macro)] {
        assert!(w.crates[asker].input.deps.iter().any(|(n, d)| *n == "reg" && *d == side_reg), "`{}` on the macro side asks the one macro-side `reg`", w.crates[asker].input.name);
    }
    assert_eq!(side_of(helper), Side::Target);
    assert_eq!(side_of(app), Side::Target);
    let app_scope = check_scope(&w, app, false, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(app_scope.readers().contains(&(pm, Seen::MacrosOnly)), "app names pm for its macros only");
    assert!(app_scope.readers().contains(&(helper, Seen::Linked { toolchain: false })), "app links the target's helper");
    // `helper` is two crates under two keys, one a side.
    let helpers: StdVec<usize> = (0..w.crates.len()).filter(|c| w.crates[*c].input.name == "helper").collect();
    assert!(helpers.iter().any(|c| w.crates[*c].side == Side::Target) && helpers.iter().any(|c| w.crates[*c].side == Side::Macro), "helper is built for both sides");
    let mut keys: StdVec<[u8; 32]> = helpers.iter().map(|c| w.crates[*c].input.key.0.0).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), helpers.len(), "each side's helper has a key of its own");
    assert!(sides_of_helper.contains(&Side::Target) && sides_of_helper.contains(&Side::Macro), "helper is checked once per side: {sides_of_helper:?}");
    // The build script reads its run on the target side; it and its build-dependency are no crate.
    assert!(matches!(w.crates[app].script, crate::Script::Read { .. }), "app reads its script's run");
    assert!(w.crate_of_unit[7].is_none() && w.crate_of_unit[8].is_none(), "a build script and its build-dependency are no crate of the world");
    assert!(!w.crates.iter().any(|c| c.input.name == "bdep"), "bdep is no owner");
    let owners = w.crates.iter().filter(|c| !matches!(c.origin, crate::Origin::Toolchain(_))).count();
    std::println!("check scope sides: {owners} owners, harness on Macro (its row Target), helper's owners on {sides_of_helper:?}");
}

/// check_scope.rs `OtherSide` stays for a real error: an edge from the target's `app` to the
/// macro side's `helper` (no proc macro between) is refused naming the owner and the edge's two
/// crates. CONTROL: the world as made gives `app` its scope.
/// Prints `check scope: a crossing edge: <the refusal's words>`.
#[test]
fn an_edge_across_sides_is_refused_naming_it() {
    use crate::check_scope::{ScopeError, check_scope};
    let mut w = sides_world();
    let app = w.crate_of_unit[6].expect("app is a crate") as usize;
    assert!(check_scope(&w, app, false, a()).is_ok(), "CONTROL: app's scope as made");
    let side_helper = (0..w.crates.len()).find(|c| w.crates[*c].input.name == "helper" && w.crates[*c].side == Side::Macro).expect("the macro side's helper");
    let mut deps: StdVec<(&'static str, usize)> = w.crates[app].input.deps.to_vec();
    deps.push(("side_helper", side_helper));
    w.crates[app].input.deps = StdBox::leak(deps.into_boxed_slice());
    let refused = check_scope(&w, app, false, a()).err().expect("a crossing edge is refused");
    assert_eq!(refused, ScopeError::OtherSide { owner: "app", from: "app", krate: "helper", side: Side::Target });
    let words = std::format!("{refused}");
    assert!(words.contains("`app` -> `helper`"), "{words}");
    std::println!("check scope: a crossing edge: {words}");
}

/// crate_list.rs: the crate list written from a world reads back EQUAL to it (every field of
/// every crate found by its key, each dependency's name and key, the sysroot), and every
/// owner's check scope over the list names the crates its scope over the world names, seen
/// alike: for the hello program and for a workspace with a macro side. CONTROL: a list whose
/// first row holds the second row's root (a row of another crate) differs. Prints
/// `crate list: <what>: <n> crates, <b> bytes, 0 differences; control: <k> differences`.
#[test]
fn the_crate_list_reads_back_as_its_world() {
    use crate::check_scope::{Seen, check_scope, check_scope_in};
    use crate::crate_list::{CrateList, HEADER_LEN, ROW, differences, encode};
    for (what, w) in [("hello", hello(BLAZE)), ("macro side", made(true))] {
        let bytes = encode(&w, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
        let list = CrateList::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
        let differ = differences(&w, &list);
        assert_eq!(differ, 0, "{what}: the list differs from its world");
        for owner in 0..w.crates.len() {
            let row = list.find(w.crates[owner].input.key).expect("every crate has its row");
            let by_world = check_scope(&w, owner, false, a()).ok().map(|s| {
                let mut v: StdVec<([u8; 32], Seen)> = s.readers().iter().map(|(at, how)| (w.crates[*at].input.key.0.0, *how)).collect();
                v.sort_unstable_by_key(|(k, _)| *k);
                v
            });
            let by_list = check_scope_in(&list, row, false, a()).ok().map(|s| {
                let mut v: StdVec<([u8; 32], Seen)> = s.readers().iter().map(|(r, how)| (list.key(*r).0.0, *how)).collect();
                v.sort_unstable_by_key(|(k, _)| *k);
                v
            });
            assert_eq!(by_world, by_list, "{what}: owner {} sees other crates over the list", w.crates[owner].input.name);
        }
        // CONTROL: row 0's root range replaced by another row's, for each other row in turn: the
        // most differences any of them makes. Not "row 1's": which crates are rows 0 and 1 moves
        // with their keys (it did when the fixture's `core` gained items, 2026-11-17), and two
        // rows can name one root (one crate on the host side and on the macro side), which
        // differs in nothing.
        let r0 = HEADER_LEN + 40;
        let own: [u8; 8] = bytes[r0..r0 + 8].try_into().expect("8 bytes");
        let mut control = 0;
        for row in 1..w.crates.len() {
            let at = HEADER_LEN + row * ROW + 40;
            let other: [u8; 8] = bytes[at..at + 8].try_into().expect("8 bytes");
            if other == own {
                continue;
            }
            let mut planted = bytes.to_vec();
            planted[r0..r0 + 8].copy_from_slice(&other);
            control = control.max(differences(&w, &CrateList::open(&planted).unwrap_or_else(|e| panic!("{e}"))));
        }
        assert!(control > 0, "{what}: CONTROL: a row holding another crate's root must differ");
        std::println!("crate list: {what}: {} crates, {} bytes, {differ} differences; control: {control} differences", w.crates.len(), bytes.len());
    }
}

/// crate_list.rs, version 2: the tree table names each tree's pack as the world placed it, in
/// `Texts::packs` order (tree `k` is pack `k - 1`): a toolchain tree by its shelf digest (the
/// pack's own footer), the fetched `dep` by its sources-store key (the lock's fetch plan's unit,
/// `SourceId::key`), each with its files' listing BLAKE3; and every crate's `shown_from` names a
/// tree of the table. CONTROL: one byte of a tree's files changed differs. Prints
/// `crate list trees: <t> trees (<s> shelf, <f> fetched), dep is tree <k>; control: <n> differences`.
#[test]
fn the_tree_table_names_each_trees_pack() {
    use crate::PackOrigin;
    use crate::crate_list::{CrateGraph as _, CrateList, HEADER_LEN, ROW, TREE, differences, encode};
    let w = made(true);
    let bytes = encode(&w, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    let list = CrateList::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(differences(&w, &list), 0, "the list differs from its world");
    assert_eq!(list.trees(), w.texts.packs().len(), "one tree per pack");
    assert_eq!(list.tree(0), None, "the checkout is no row of the table");
    assert_eq!(list.tree(list.trees() + 1), None, "past the table is no row");
    let (mut shelf, mut fetched) = (0, 0);
    for (k, pack) in w.texts.packs().iter().enumerate() {
        let row = list.tree(k + 1).expect("every tree has its row");
        assert_eq!(row.files, pack.tree().0, "tree {}: its files are the pack's", k + 1);
        match row.origin {
            PackOrigin::Shelf { .. } => shelf += 1,
            PackOrigin::Source { .. } => fetched += 1,
        }
    }
    let dep = w.crate_of_unit[0].expect("the target's dep is a crate") as usize;
    let shown = w.crates[dep].input.shown_from;
    let k: usize = shown.trim_start_matches('/').split('/').next().and_then(|n| n.parse().ok()).expect("dep's shown_from names a tree");
    let lock = lock();
    let dep_package = lock.named("dep").next().expect("in the lock");
    let plan = blaze_fetch::plan::plan(lock, a()).unwrap_or_else(|_| panic!("the lock plans"));
    let want = plan.units.iter().find(|u| matches!(u, blaze_fetch::Unit::Crate { package, .. } if *package == dep_package)).expect("dep is a unit").key();
    assert_eq!(list.tree(k).map(|row| row.origin), Some(PackOrigin::Source { key: want }), "dep's tree is its fetched record, by key");
    for c in w.crates.iter() {
        let k: usize = c.input.shown_from.trim_start_matches('/').split('/').next().and_then(|n| n.parse().ok()).expect("a tree number");
        assert!(k == 0 || list.tree(k).is_some(), "`{}` is shown from tree {k}, which the table names", c.input.name);
    }
    // CONTROL: the last byte of the first tree's files.
    let mut planted = bytes.to_vec();
    let at = HEADER_LEN + list.len() * ROW + bytes_of_deps(&list) + TREE - 1;
    planted[at] ^= 1;
    let control = differences(&w, &CrateList::open(&planted).unwrap_or_else(|e| panic!("{e}")));
    assert!(control > 0, "CONTROL: a tree of other files must differ");
    std::println!("crate list trees: {} trees ({shelf} shelf, {fetched} fetched), dep is tree {k}; control: {control} differences", list.trees());
}

/// crate_list.rs version 6, rule 25: the rows are in the world's order (the toolchain's crates
/// in its manifest's order, then by crate key; two crates under one key are refused, so no tie
/// is left to break), a function of the crates' contents alone: one graph whose units are
/// numbered in the reverse order makes a world of the same crates at the same indices, and its
/// list is the same bytes. Nothing of a map's order or of this machine enters (the world's
/// tables are vectors sorted by key; the list writes rows, then each table, in those orders).
/// CONTROL: the list of the other fixture world (the cut's macro side on) differs. Prints
/// `crate list order: <n> crates, <b> bytes, equal over both unit orders; control differs`.
#[test]
fn the_list_is_the_same_bytes_whatever_the_units_order() {
    use crate::crate_list::{CrateGraph as _, CrateList, encode};
    let made_from = |reversed: bool, macro_side: bool| {
        let lock = lock();
        let graph = if reversed { graph_reversed(lock) } else { graph(lock) };
        let dep = crate::fixture::pack(&[("src/lib.rs", "pub struct Expr;\n")], a());
        let mut places = Places { lock, dep: StdBox::leak(dep.into_boxed_slice()) };
        let (id, mut shelf) = shelf(macro_side);
        let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
        let listed = StdBox::leak(StdBox::new(listed));
        world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"))
    };
    let (forward, backward) = (made_from(false, true), made_from(true, true));
    let one = encode(&forward, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    let two = encode(&backward, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    let list = CrateList::open(&one).unwrap_or_else(|e| panic!("{e}"));
    for r in 0..list.len() {
        assert_eq!(list.key(r), forward.crates[r].input.key, "row {r} is the world's crate {r}");
        assert_eq!(list.find(forward.crates[r].input.key), Some(r), "row {r} is found by its key");
    }
    assert_eq!(forward.crates.len(), backward.crates.len(), "one graph, one set of crates");
    for at in 0..forward.crates.len() {
        assert_eq!(forward.crates[at].input.key, backward.crates[at].input.key, "crate {at}: the same crate over both unit orders");
    }
    assert!(one.as_slice() == two.as_slice(), "the list's bytes differ over the units' order");
    let other = encode(&made_from(false, false), &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    assert!(other.as_slice() != one.as_slice(), "CONTROL: another world's list must differ");
    std::println!("crate list order: {} crates, {} bytes, equal over both unit orders; control differs", list.len(), one.len());
}

/// crate_list.rs version 6: each row's `TEST` flag is its world crate's cfg (`cfg_test`: `test`
/// with no value), row `r` the world's crate `r`. CONTROL (rule 23): row 0's flag flipped makes
/// the list differ from its world, so `differences` sees the flag (blaze-cli names.rs
/// `Names::differences_from` refuses a world made by need on it). Prints
/// `crate list test flags: <t> of <n> rows test; control: <k> differences`.
#[test]
fn a_flipped_test_flag_differs_from_its_world() {
    use crate::crate_list::{CrateList, FLAGS_AT, HEADER_LEN, TEST, cfg_test, differences, encode};
    let w = made(true);
    let bytes = encode(&w, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    let list = CrateList::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(differences(&w, &list), 0, "the list differs from its world");
    for at in 0..w.crates.len() {
        assert_eq!(list.test(at), cfg_test(&w, at), "row {at}: its test flag is its crate's cfg");
    }
    let tests = (0..w.crates.len()).filter(|at| list.test(*at)).count();
    let mut planted = bytes.to_vec();
    planted[HEADER_LEN + FLAGS_AT] ^= TEST;
    let control = differences(&w, &CrateList::open(&planted).unwrap_or_else(|e| panic!("{e}")));
    assert!(control > 0, "CONTROL: a row whose test flag is flipped must differ from its world");
    std::println!("crate list test flags: {tests} of {} rows test; control: {control} differences", w.crates.len());
}

/// The bytes of a list's dependency table: its entries counted over every row.
fn bytes_of_deps(list: &crate::crate_list::CrateList<'_>) -> usize {
    use crate::crate_list::CrateGraph as _;
    (0..list.len()).map(|r| list.deps_named(r).count()).sum::<usize>() * crate::crate_list::DEP
}

/// A list of an older version is refused by name, never read as this form. Prints
/// `crate list: an old list: <the refusal's words>`.
#[test]
fn an_old_crate_list_is_refused_by_name() {
    use crate::crate_list::{CrateList, ListError, VERSION, encode};
    let w = hello(BLAZE);
    let mut bytes = encode(&w, &|_| None, a()).unwrap_or_else(|e| panic!("{e}")).to_vec();
    // Version 4: each crate's row carries its package's metadata variables (lane RE3).
    // Version 5: each crate's row carries its package's version, for diagnostics.
    // Version 6: rows in the world's order with a by-key table, the test and public flags and
    // the planners' tables (engine.md item 2): a version-5 list is refused, never read (D160).
    // Version 7: each tree row carries its pack kind.
    // Version 8: each row carries its package binaries (`CARGO_BIN_EXE_<name>`).
    assert_eq!(VERSION, 8);
    bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
    let refused = CrateList::open(&bytes).err().expect("a version-2 list is refused");
    assert_eq!(refused, ListError::Version { found: 2 });
    let words = std::format!("{refused}");
    assert!(words.contains("version 2, not 8"), "{words}");
    for old in 1..VERSION {
        bytes[4..6].copy_from_slice(&old.to_le_bytes());
        assert_eq!(CrateList::open(&bytes).err(), Some(ListError::Version { found: old }), "a version-{old} list is refused by name");
    }
    std::println!("crate list: an old list: {words}");
}

/// An external checkout path package (`Place::Path`, e.g. `[patch.crates-io]` or an excluded path
/// dependency) lives in tree 0 (`/0/<dir>/...`) so `FileItems` and edits invalidate cleanly, yet
/// has `member == false` and `SeenRoot::Package` (not checked as a workspace member) and round-trips
/// through `CrateList` with zero differences.
#[test]
fn external_checkout_path_package_is_tree_zero_non_member() {
    use crate::crate_list::{CrateList, differences, encode};
    const PATCH_LOCK: &str = "\
version = 4

[[package]]
name = \"app\"
version = \"0.1.0\"
dependencies = [\"patched\"]

[[package]]
name = \"patched\"
version = \"0.1.0\"
";
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(PATCH_LOCK.as_bytes(), a()).expect("lock")));
    let mut units = AVec::new_in(a());
    units.push(unit(lock, "patched", UnitKind::Lib, false, &[]));
    units.push(unit(lock, "app", UnitKind::Lib, false, &[("patched", 0)]));
    let graph: &'static UnitGraph<'static> =
        StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }));
    struct PatchPlaces {
        app: LockId,
        patched: LockId,
    }
    impl Packages<'static> for PatchPlaces {
        fn place(&mut self, id: LockId) -> Result<Place<'static>, WorldError<'static>> {
            if id == self.app {
                Ok(Place::Checkout { dir: "crates/app" })
            } else if id == self.patched {
                Ok(Place::Path { dir: "vendor/patched" })
            } else {
                Err(WorldError::PathNotPlaced { package: "unknown" })
            }
        }
    }
    let mut places = PatchPlaces { app: lock.named("app").next().expect("app"), patched: lock.named("patched").next().expect("patched") };
    let (id, mut shelf) = shelf(false);
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, b()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    let w = world(graph, lock, listed, &mut places, &NoFiles, b()).unwrap_or_else(|e| panic!("{e}"));
    let app_at = w.crates.iter().position(|c| c.input.name == "app").expect("app crate");
    let patched_at = w.crates.iter().position(|c| c.input.name == "patched").expect("patched crate");
    assert!(w.crates[app_at].member, "workspace member must have member == true");
    assert!(!w.crates[patched_at].member, "external checkout path package must have member == false");
    assert_eq!(w.crates[app_at].input.root, "/0/crates/app/src/lib.rs");
    assert_eq!(w.crates[patched_at].input.root, "/0/vendor/patched/src/lib.rs");
    let bytes = encode(&w, &|_| None, a()).unwrap_or_else(|e| panic!("{e}"));
    let list = CrateList::open(&bytes).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(differences(&w, &list), 0);
    assert!(list.member(app_at));
    assert!(!list.member(patched_at));
    assert_eq!(list.root(patched_at), "/0/vendor/patched/src/lib.rs");
    assert_eq!(list.env_facts(patched_at).seen, blaze_scope::crate_env::SeenRoot::Package);
}
