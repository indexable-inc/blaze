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

const TRIPLE: &str = "aarch64-apple-darwin";
const MACRO: &str = "wasm32-wasip1";
const TARGET_CFG: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];
const MACRO_CFG: &[(&str, Option<&str>)] = &[("target_arch", Some("wasm32")), ("target_family", Some("wasm")), ("target_os", Some("wasi"))];
const RUST21: PackageEdition = PackageEdition { syntax: Syntax::Edition2021, blaze: None };
const BLAZE: PackageEdition = PackageEdition { syntax: Syntax::Edition2024, blaze: Some(BlazeEdition::Blaze2026_11) };

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
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
        incomplete: false,
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
    StdBox::leak(StdBox::new(UnitGraph { units, unavailable: AVec::new_in(a()), resolver: Resolver::V2 }))
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
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, a()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut places, &NoFiles, None, a()).unwrap_or_else(|e| panic!("{e}"))
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
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(HELLO.as_bytes(), a()).expect("the lock reads")));
    let mut member = unit(lock, "hello", UnitKind::Lib, false, &[]);
    member.edition = edition;
    let mut units = AVec::new_in(a());
    units.push(member);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, unavailable: AVec::new_in(a()), resolver: Resolver::V2 }));
    let mut places = Places { lock, dep: &[] };
    let (id, files) = crate::fixture::shelf_with_std_dep(TRIPLE, crate::fixture::STD, TARGET_CFG, a());
    let mut shelf = Memory(files.into_iter().collect());
    let listed = open_list(&mut shelf, id, TRIPLE, graph, lock, &mut places, a()).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    world(graph, lock, listed, &mut places, &NoFiles, None, a()).unwrap_or_else(|e| panic!("{e}"))
}

/// Every toolchain crate's implicit is Rust's, bstd's included, so a blaze-edition program's
/// world holds Rust's std and std's own dependency (`cfg_if`, from the fixture's shelf) beside
/// bstd. The world is made only when the list's toolchain crates (std's closure always) are all
/// opened: the strict match by unit. Prints
/// `crate list one home: a blaze-edition world opens Rust std for its toolchain (<n> toolchain crates)`.
/// Without -4 (std opened only for a Rust-edition unit): `hello(BLAZE)` is refused, "the
/// toolchain crate `…` of the crate list was not opened from the shelf", and on the real run
/// bstd's closure lost `std` (blaze_ids `Vec` NotFound, recpipe5/time5.sh).
#[test]
fn a_blaze_edition_world_opens_rust_std_for_the_toolchain() {
    let named = |w: &World<'static>, name: &str| w.crates.iter().any(|c| c.input.name == name);
    let toolchain = |w: &World<'static>| w.crates.iter().filter(|c| matches!(c.origin, crate::Origin::Toolchain(_))).count();
    let blaze = hello(BLAZE);
    assert!(named(&blaze, "blaze-std") || named(&blaze, "bstd"), "a blaze-edition program opens bstd");
    assert!(named(&blaze, "std"), "bstd's implicit is Rust's std: it is opened");
    assert!(named(&blaze, "cfg_if"), "std's dependency is opened with it");
    let rust = hello(RUST21);
    assert!(named(&rust, "std") && named(&rust, "cfg_if"), "under Rust 2021 std and its cfg_if are opened");
    std::println!("crate list one home: a blaze-edition world opens Rust std for its toolchain ({} toolchain crates)", toolchain(&blaze));
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
