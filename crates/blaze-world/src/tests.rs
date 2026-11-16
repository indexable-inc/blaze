//! A small workspace made into the names library's inputs: two members (one of the blaze
//! edition), a renamed dependency, a dev-dependency that must not reach the library, a proc
//! macro whose host-side dependency is not followed, a build script's cfg joined or counted
//! missing; the same graph listed in another order gives byte-equal inputs.

use std::boxed::Box as StdBox;
use std::collections::BTreeMap as StdMap;
use std::string::{String as StdString, ToString};
use std::vec::Vec as StdVec;
use std::{format, vec};

use blaze_build_script::Outputs;
use blaze_build_script::instr::OutputLimits;
use blaze_build_script::outputs::{FileLimits, OutputsRef};
use blaze_fetch::PackView;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::task::Serial;
use blaze_toolchain::Shelf;
use blaze_unit_graph::{CfgAtom, Resolver, StdIs, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::{BlazeEdition, Edition as Syntax, PackageEdition};

use crate::{Packages, Place, Script, ScriptOutputs, Toolchain, Tree, World, WorldError, needs, world};

fn g() -> Alloc<'static> {
    Alloc::global()
}

use crate::fixture::pack;

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

const TRIPLE: &str = "aarch64-apple-darwin";

/// core, alloc, std, proc_macro (one pack) and bstd (another), all target-side.
fn toolchain(needs: crate::Needs) -> &'static Toolchain<'static, StdVec<u8>> {
    toolchain_of(needs, "#![no_std]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }", &[("target_os", Some("macos")), ("unix", None)])
}

fn toolchain_of(needs: crate::Needs, std: &str, cfg: &'static [(&'static str, Option<&'static str>)]) -> &'static Toolchain<'static, StdVec<u8>> {
    let (id, files) = crate::fixture::shelf(TRIPLE, std, cfg);
    let tc = Toolchain::open(&mut Memory(files.into_iter().collect()), id, TRIPLE, needs, g()).unwrap_or_else(|e| panic!("{e}"));
    StdBox::leak(StdBox::new(tc))
}

/// The workspace's lock: two members, four registry packages.
const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["derive_it", "testkit", "util"]

[[package]]
name = "blz"
version = "0.1.0"
dependencies = ["app"]

[[package]]
name = "derive_it"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
dependencies = ["helper"]

[[package]]
name = "helper"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "testkit"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "util"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

fn t(s: &str) -> AString<'static> {
    AString::from_str_in(s, g())
}

/// One unit as the unit graph makes it.
fn unit(lock: &Lock<'static>, name: &str, kind: UnitKind<'static>, for_host: bool, edition: PackageEdition, deps: &[(&str, u32)], build_script: Option<u32>) -> Unit<'static> {
    let package = lock.named(name).next().expect("in the lock");
    let mut cfg = AVec::new_in(g());
    cfg.push(CfgAtom::flag("debug_assertions", g()));
    cfg.push(CfgAtom::pair("target_os", "macos", g()));
    cfg.push(CfgAtom::flag("unix", g()));
    if kind == UnitKind::ProcMacro {
        cfg.push(CfgAtom::flag("proc_macro", g()));
    }
    if name == "util" {
        cfg.push(CfgAtom::pair("feature", "std", g()));
    }
    let mut features = AVec::new_in(g());
    if name == "util" {
        features.push(t("std"));
    }
    let mut d = AVec::new_in(g());
    for (n, u) in deps {
        d.push(UnitDep { name: t(n), unit: UnitId(*u) });
    }
    let crate_name = name.replace('-', "_");
    let root = match kind {
        UnitKind::BuildScript => "build.rs",
        UnitKind::Test { .. } => "src/lib.rs",
        _ => "src/lib.rs",
    };
    Unit {
        package,
        name: t(name),
        version: t(if lock.package(package).source_text.is_empty() { "0.1.0" } else { "1.0.0" }),
        kind,
        for_host,
        edition,
        crate_name: t(&crate_name),
        root: t(root),
        features,
        cfg,
        deps: d,
        build_script: build_script.map(UnitId),
        links: None,
        incomplete: false,
    }
}

const RUST21: PackageEdition = PackageEdition { syntax: Syntax::Edition2021, blaze: None };
const BLAZE: PackageEdition = PackageEdition { syntax: Syntax::Edition2024, blaze: Some(BlazeEdition::Blaze2026_11) };

/// The graph, its units listed in the order `order` gives (a permutation of 0..9).
fn graph(lock: &Lock<'static>, order: &[u32; 9]) -> &'static UnitGraph<'static> {
    // By fixed id: 0 util (lib), 1 util's build script, 2 helper (host lib), 3 derive_it
    // (proc macro), 4 testkit, 5 app (lib), 6 app's tests, 7 blz (lib), 8 helper for the
    // target (not depended on by anything held: a crate of its own, deduplicated by nothing).
    let fixed: [Unit<'static>; 9] = [
        unit(lock, "util", UnitKind::Lib, false, RUST21, &[], Some(1)),
        unit(lock, "util", UnitKind::BuildScript, true, RUST21, &[], None),
        unit(lock, "helper", UnitKind::Lib, true, RUST21, &[], None),
        unit(lock, "derive_it", UnitKind::ProcMacro, true, RUST21, &[("helper", 2)], None),
        unit(lock, "testkit", UnitKind::Lib, false, RUST21, &[], None),
        unit(lock, "app", UnitKind::Lib, false, RUST21, &[("derive_it", 3), ("u", 0)], None),
        unit(lock, "app", UnitKind::Test { bin: None }, false, RUST21, &[("app", 5), ("derive_it", 3), ("testkit", 4), ("u", 0)], None),
        unit(lock, "blz", UnitKind::Lib, false, BLAZE, &[("app", 5)], None),
        unit(lock, "helper", UnitKind::Lib, false, RUST21, &[], None),
    ];
    // `order[i]` is the fixed unit listed at position i; ids are remapped to positions.
    let mut at = [0u32; 9];
    for (i, f) in order.iter().enumerate() {
        at[*f as usize] = i as u32;
    }
    let mut units = AVec::new_in(g());
    for f in order {
        let mut u = fixed[*f as usize].clone();
        for d in u.deps.iter_mut() {
            d.unit = UnitId(at[d.unit.index()]);
        }
        u.build_script = u.build_script.map(|b| UnitId(at[b.index()]));
        units.push(u);
    }
    StdBox::leak(StdBox::new(UnitGraph { units, unavailable: AVec::new_in(g()), resolver: Resolver::V2 }))
}

/// The checkout: each member's files.
struct Files(StdMap<&'static str, &'static str>);

impl Tree for Files {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.0.get(path).map(|t| t.as_bytes())
    }
}

fn checkout() -> &'static Files {
    let mut m = StdMap::new();
    m.insert("app/src/lib.rs", "pub use u::thing;\npub mod inner;\n#[cfg(feature = \"x\")]\npub mod off;\n");
    m.insert("app/src/inner.rs", "pub fn f() {}\n");
    m.insert("blz/src/lib.rs", "pub use app::inner;\n");
    StdBox::leak(StdBox::new(Files(m)))
}

/// The store: one record per registry package, each a crate's files at its top.
struct Store {
    lock: &'static Lock<'static>,
    packs: StdMap<&'static str, &'static [u8]>,
    asked: StdVec<StdString>,
}

impl Packages<'static> for Store {
    fn place(&mut self, package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        let name: &'static str = StdBox::leak(self.lock.package(package).name.as_str().to_string().into_boxed_str());
        self.asked.push(name.to_string());
        match name {
            "app" => Ok(Place::Checkout { dir: "app" }),
            "blz" => Ok(Place::Checkout { dir: "blz" }),
            _ => match self.packs.get(name) {
                Some(bytes) => Ok(Place::Pack { view: PackView::open_verified(bytes).expect("a pack"), dir: "" }),
                None => Err(WorldError::PathNotPlaced { package: name }),
            },
        }
    }
}

fn store(lock: &'static Lock<'static>) -> Store {
    let mut packs: StdMap<&'static str, &'static [u8]> = StdMap::new();
    for (name, text) in [
        ("util", "pub fn thing() {}\n"),
        ("helper", "pub fn help() {}\n"),
        ("derive_it", "extern crate proc_macro;\n#[proc_macro_derive(It)]\npub fn it(_: proc_macro::TokenStream) -> proc_macro::TokenStream { loop {} }\n"),
        ("testkit", "pub fn check() {}\n"),
    ] {
        let p = pack(&[("src/lib.rs", text)]);
        packs.insert(name, StdBox::leak(p.into_boxed_slice()));
    }
    Store { lock, packs, asked: vec![] }
}

/// One record for every served library: cfg `has_atomics`, an env pair and one generated file.
struct Scripts(OutputsRef<'static>);

impl ScriptOutputs<'static> for Scripts {
    fn served(&self, _lib: UnitId) -> Option<crate::Served<'static>> {
        Some(crate::Served { outputs: self.0, cfg: &[("has_atomics", None)], digest: self.0.crate_digest() })
    }
}

/// [`Scripts`] whose record's generated file `gen.rs` holds `file`, and whose env pair
/// `VERSION` is `version`.
fn scripts(file: &str, version: &str) -> &'static Scripts {
    let printed = format!("cargo:rustc-cfg=has_atomics\ncargo:rustc-env=VERSION={version}\n");
    let out_dir = "/target/debug/build/util-0123456789abcdef/out";
    let mut out = Outputs::read(printed.as_bytes(), out_dir, OutputLimits::DEFAULT, g()).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("gen.rs", file.as_bytes(), FileLimits::DEFAULT, g()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = AVec::new_in(g());
    out.encode(&mut bytes);
    let bytes: &'static [u8] = bytes.leak();
    StdBox::leak(StdBox::new(Scripts(OutputsRef::open(bytes).unwrap_or_else(|e| panic!("{e:?}")))))
}

fn lock() -> &'static Lock<'static> {
    StdBox::leak(StdBox::new(Lock::read(LOCK.as_bytes(), g()).expect("the lock reads")))
}

fn made(order: &[u32; 9], scripts: Option<&dyn ScriptOutputs<'static>>) -> World<'static> {
    let lock = lock();
    let graph = graph(lock, order);
    let tc = toolchain(needs(graph));
    let mut store = store(lock);
    world(graph, lock, tc, &mut store, checkout(), scripts, g()).unwrap_or_else(|e| panic!("{e}"))
}

fn by_name<'w>(w: &'w World<'static>, name: &str) -> StdVec<&'w crate::Crate<'static>> {
    w.crates.iter().filter(|c| c.input.name == name).collect()
}

const IN_ORDER: [u32; 9] = [0, 1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn the_inputs_hold_what_each_unit_is_given() {
    let w = made(&IN_ORDER, None);
    let names: StdVec<&str> = w.crates.iter().map(|c| c.input.name).collect();
    // The toolchain first (proc_macro for derive_it, bstd for blz), then the held units: no
    // build script, no host-side helper (a proc macro's dependency is not followed); the test
    // target is a crate of its own beside its library (a `check` answers for its names too).
    assert_eq!(&names[..5], ["core", "alloc", "std", "proc_macro", "bstd"]);
    let mut held: StdVec<&str> = names[5..].to_vec();
    held.sort();
    assert_eq!(held, ["app", "app", "blz", "derive_it", "helper", "testkit", "util"]);
    assert_eq!(w.crate_of_unit[1], None, "a build script is no crate");
    assert_eq!(w.crate_of_unit[2], None, "the proc macro's host-side helper is no crate");
    let (lib, test) = (w.crate_of_unit[5].expect("the library is a crate"), w.crate_of_unit[6].expect("a test target is a crate"));
    assert_ne!(lib, test);
    let tested: StdVec<&str> = w.crates[test as usize].input.deps.iter().map(|(n, _)| *n).collect();
    assert!(tested.contains(&"app") && tested.contains(&"testkit"), "the test target reaches its library and its dev-dependency: {tested:?}");

    let app = &w.crates[lib as usize];
    let deps: StdVec<(&str, &str)> = app.input.deps.iter().map(|(n, i)| (*n, w.crates[*i].input.name)).collect();
    // The rename is the extern name; the dev-dependency is not there.
    assert_eq!(deps, [("derive_it", "derive_it"), ("u", "util")]);
    assert_eq!(app.input.implicit.std, StdIs::Rust);
    assert_eq!(app.input.root, "/0/app/src/lib.rs");
    assert_eq!(app.input.shown_from, "/0/app/");
    assert_eq!(app.input.cfg, [("debug_assertions", None), ("target_os", Some("macos")), ("unix", None)]);

    let blz = by_name(&w, "blz")[0];
    assert!(matches!(blz.input.implicit.std, StdIs::Bstd(_)), "a blaze-edition member's `std` is bstd");
    assert_eq!(w.sysroot.bstd.map(|c| w.crates[c.0 as usize].input.name), Some("bstd"));
    assert_eq!(w.sysroot.std.map(|c| w.crates[c.0 as usize].input.name), Some("std"));

    let pm = by_name(&w, "derive_it")[0];
    assert!(pm.proc_macro && pm.input.implicit.proc_macro);
    let deps: StdVec<(&str, &str)> = pm.input.deps.iter().map(|(n, i)| (*n, w.crates[*i].input.name)).collect();
    assert_eq!(deps, [("proc_macro", "proc_macro")], "a proc macro gets `proc_macro`, not its host-side dependencies");
    assert!(pm.input.cfg.contains(&("proc_macro", None)));

    let util = by_name(&w, "util")[0];
    assert_eq!(util.script, Script::Missing, "its build script's outputs were not given");
    assert!(util.input.cfg.contains(&("feature", Some("std"))));
    assert_eq!(util.input.features, ["std"]);
    assert!(util.input.root.ends_with("/src/lib.rs") && !util.input.root.starts_with("/0/"));
}

#[test]
fn a_build_scripts_cfg_joins_its_library() {
    let w = made(&IN_ORDER, Some(scripts("pub fn made() {}\n", "1")));
    let util = by_name(&w, "util")[0];
    assert_eq!(util.script, Script::Joined);
    assert!(util.input.cfg.contains(&("has_atomics", None)));
    // The record is the crate's input: its generated file and env pair are what expansion reads.
    let record = util.input.script.unwrap_or_else(|| panic!("util's input has no script record"));
    assert_eq!(record.file("gen.rs"), Some(&b"pub fn made() {}\n"[..]));
    assert_eq!(record.env().collect::<StdVec<_>>(), [("VERSION", "1")]);
    // Only a unit a script serves takes a record.
    assert!(by_name(&w, "app")[0].input.script.is_none());
    // Control: without the outputs the atom is not there, and the key differs (rule 21).
    let bare = made(&IN_ORDER, None);
    let bare_util = by_name(&bare, "util")[0];
    assert!(!bare_util.input.cfg.contains(&("has_atomics", None)));
    assert!(bare_util.input.script.is_none());
    assert_ne!(util.input.key, bare_util.input.key);
}

/// std's build script (library/std/build.rs:80) prints `cargo:rustc-cfg=backtrace_in_libstd` on
/// every target, and std's backtrace module (library/backtrace/src/symbolize/gimli.rs:21..26)
/// picks `mod mystd` or `extern crate std as mystd` by it. The Reference, "Conditional
/// compilation", "Set configuration options": options are set by the compiler from the build's
/// inputs; a build script's `rustc-cfg` is such an input. Until the script runs, the interim
/// table gives std, and only std, the atom (toolchain.rs `STD_SCRIPT_CFG`).
#[test]
fn std_carries_its_build_script_cfg() {
    let tc = toolchain(crate::Needs { std: true, bstd: false, proc_macro: false });
    let std = &tc.crates()[tc.std().unwrap_or_else(|| panic!("no std"))];
    assert!(std.cfg.contains(&("backtrace_in_libstd", None)), "std's cfg: {:?}", std.cfg);
    let core = &tc.crates()[tc.core().unwrap_or_else(|| panic!("no core"))];
    assert!(!core.cfg.contains(&("backtrace_in_libstd", None)), "core's cfg: {:?}", core.cfg);
}

/// Two records equal in cfgs that differ only in one generated file's bytes, or only in one env
/// value, give the crate two keys: a changed file under an unchanged key would be two values
/// under one key (rule 21). The control: equal records give one key.
#[test]
fn a_crates_key_covers_its_scripts_files_and_env() {
    let key = |file: &str, version: &str| by_name(&made(&IN_ORDER, Some(scripts(file, version))), "util")[0].input.key;
    let first = key("pub fn made() {}\n", "1");
    assert_eq!(first, key("pub fn made() {}\n", "1"));
    assert_ne!(first, key("pub fn made() { }\n", "1"), "a changed generated file left the key unchanged");
    assert_ne!(first, key("pub fn made() {}\n", "2"), "a changed env value left the key unchanged");
}

/// A toolchain crate's key is its identity: the same pack on another target's cfg is another
/// crate, and a changed `std` moves `std`'s key and every unit's implicit key (rule 21). The
/// key holds the pack's digest, so a crate moves when any crate of its pack does (one library
/// pack: a toolchain bump moves them all). Control: the same inputs give equal keys.
#[test]
fn a_toolchain_crates_key_covers_its_target_and_dependencies() {
    let needs = crate::Needs { std: true, bstd: false, proc_macro: true };
    let std = "#![no_std]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }";
    let mac: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];
    let linux: &[(&str, Option<&str>)] = &[("target_os", Some("linux")), ("unix", None)];
    let key = |tc: &Toolchain<'static, StdVec<u8>>, role: Option<usize>| tc.crates()[role.expect("the role")].key;
    let (a, same, other_std, other_target) = (toolchain_of(needs, std, mac), toolchain_of(needs, std, mac), toolchain_of(needs, "#![no_std]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} pub fn added() {} }", mac), toolchain_of(needs, std, linux));
    assert_eq!(key(a, a.std()), key(same, same.std()));
    assert_ne!(key(a, a.std()), key(other_std, other_std.std()), "a changed std left its key unchanged");
    assert_ne!(key(a, a.core()), key(other_target, other_target.core()), "another target's cfg left core's key unchanged");
    let rust = PackageEdition { syntax: Syntax::Edition2021, blaze: None };
    let implicit = |tc| crate::implicit_key(tc, blaze_unit_graph::implicit(rust, &UnitKind::Lib)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(implicit(a), implicit(same));
    assert_ne!(implicit(a), implicit(other_std), "a changed std left a unit's implicit key unchanged");
}

#[test]
fn the_same_graph_in_any_order_gives_equal_bytes() {
    let mut first = AVec::new_in(g());
    made(&IN_ORDER, None).write(&mut first);
    let orders: [[u32; 9]; 4] = [[8, 7, 6, 5, 4, 3, 2, 1, 0], [5, 0, 7, 3, 1, 8, 2, 6, 4], [4, 6, 2, 8, 0, 3, 7, 1, 5], [7, 5, 3, 1, 8, 6, 4, 2, 0]];
    for order in &orders {
        let mut other = AVec::new_in(g());
        made(order, None).write(&mut other);
        assert!(first == other, "order {order:?} gives other inputs");
    }
    // Control: a changed input changes the bytes.
    let mut joined = AVec::new_in(g());
    made(&IN_ORDER, Some(scripts("pub fn made() {}\n", "1"))).write(&mut joined);
    assert!(first != joined);
}

#[test]
fn a_dependency_on_a_unit_not_held_is_refused_by_name() {
    let lock = lock();
    let mut units = graph(lock, &IN_ORDER).units.clone();
    // app's library made to depend on helper's host-side unit.
    units[5].deps.push(UnitDep { name: t("helper"), unit: UnitId(2) });
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, unavailable: AVec::new_in(g()), resolver: Resolver::V2 }));
    let tc = toolchain(needs(graph));
    let mut store = store(lock);
    match world(graph, lock, tc, &mut store, checkout(), None, g()) {
        Err(WorldError::DependencyNotHeld { krate, dep, for_host, .. }) => assert_eq!((krate, dep, for_host), ("app", "helper", true)),
        Err(other) => panic!("another error: {other}"),
        Ok(_) => panic!("taken"),
    }
}

#[test]
fn an_absent_source_is_an_error_naming_the_package() {
    let lock = lock();
    let graph = graph(lock, &IN_ORDER);
    let tc = toolchain(needs(graph));
    let mut store = store(lock);
    store.packs.remove("testkit");
    match world(graph, lock, tc, &mut store, checkout(), None, g()) {
        Err(e) => assert!(format!("{e}").contains("testkit"), "{e}"),
        Ok(_) => panic!("taken"),
    }
}

#[test]
fn the_world_resolves() {
    let w = made(&IN_ORDER, None);
    let inputs = w.inputs(g());
    let names = w.resolve(&inputs, &Serial, g()).unwrap_or_else(|e| panic!("{e}"));
    let app = by_name(&w, "app")[0];
    let at = w.crates.iter().position(|c| core::ptr::eq(c, app)).expect("app");
    let c = names.krate(at);
    // Its root, `inner` from its file, `off` cfg'd out.
    assert_eq!(c.tree.modules.len(), 2, "{:?}", c.tree.notes);
    let errors: usize = c.done.values().map(|r| r.scope.errors.len()).sum();
    assert_eq!(errors, 0, "app's imports all resolve");
}
