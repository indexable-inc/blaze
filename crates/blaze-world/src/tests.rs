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
use blaze_own::{AString, AVec, Alloc, Bump};
use blaze_scope::task::Serial;
use blaze_toolchain::Shelf;
use blaze_unit_graph::{CfgAtom, Resolver, StdIs, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::{BlazeEdition, Edition as Syntax, PackageEdition};

use crate::{Listed, Packages, Place, Script, Toolchain, Tree, World, WorldError, joined, open_list, world};

fn g() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn b() -> Bump<'static> {
    blaze_own::testing::bump()
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
    let (id, files) = crate::fixture::shelf(TRIPLE, std, cfg, blaze_own::testing::heap().alloc());
    let tc = Toolchain::open(&mut Memory(files.into_iter().collect()), id, TRIPLE, needs, b()).unwrap_or_else(|e| panic!("{e}"));
    StdBox::leak(StdBox::new(tc))
}

/// The crate list of `graph` over the fixture's toolchain ([`toolchain`]'s, target side only),
/// its members placed by `store`.
fn listed_or(graph: &'static UnitGraph<'static>, lock: &'static Lock<'static>, store: &mut Store) -> Result<&'static Listed<'static, StdVec<u8>>, WorldError<'static>> {
    let (id, files) = crate::fixture::shelf(TRIPLE, crate::fixture::STD, &[("target_os", Some("macos")), ("unix", None)], blaze_own::testing::heap().alloc());
    let listed = open_list(&mut Memory(files.into_iter().collect()), id, TRIPLE, graph, lock, store, b())?;
    Ok(StdBox::leak(StdBox::new(listed)))
}

fn listed(graph: &'static UnitGraph<'static>, lock: &'static Lock<'static>, store: &mut Store) -> &'static Listed<'static, StdVec<u8>> {
    listed_or(graph, lock, store).unwrap_or_else(|e| panic!("{e}"))
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
    StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(g()), left_out: AVec::new_in(g()) }))
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
    /// Where the member `app` is in the checkout.
    app_dir: &'static str,
}

impl Packages<'static> for Store {
    fn place(&mut self, package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        let name: &'static str = StdBox::leak(self.lock.package(package).name.as_str().to_string().into_boxed_str());
        self.asked.push(name.to_string());
        match name {
            "app" => Ok(Place::Checkout { dir: self.app_dir }),
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
        // util reads a file of its own by the root it sees (D96): `/package/src/more.rs`; and
        // one named by what `file!()` gives it: `/package/src/util-1.0.0/src/lib.rs`.
        (
            "util",
            "pub fn thing() {}\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! env { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! file { () => {} }\ninclude!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/more.rs\"));\ninclude!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/\", file!()));\n#[rustc_builtin_macro]\nmacro_rules! line { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! column { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! module_path { () => {} }\n  #[allow(unused)] include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/\", module_path!(), \"-\", line!(), \"-\", column!(), \".rs\"));\n",
        ),
        ("helper", "pub fn help() {}\n"),
        ("derive_it", "extern crate proc_macro;\n#[proc_macro_derive(It)]\npub fn it(_: proc_macro::TokenStream) -> proc_macro::TokenStream { loop {} }\n"),
        ("testkit", "pub fn check() {}\n"),
    ] {
        let p = pack(
            &[("src/lib.rs", text), ("src/more.rs", "pub fn more() {}\n"), ("src/util-1.0.0/src/lib.rs", "pub fn named() {}\n"), ("src/util-18-20.rs", "pub fn placed() {}\n")],
            blaze_own::testing::heap().alloc(),
        );
        packs.insert(name, StdBox::leak(p.into_boxed_slice()));
    }
    Store { lock, packs, asked: vec![], app_dir: "app" }
}

/// A script's outputs record: cfg `has_atomics`, the env pair `VERSION` = `version` and one
/// generated file `gen.rs` holding `file`.
fn scripts(file: &str, version: &str) -> OutputsRef<'static> {
    let printed = format!("cargo:rustc-cfg=has_atomics\ncargo:rustc-env=VERSION={version}\n");
    let out_dir = "/target/debug/build/util-0123456789abcdef/out";
    let mut out = Outputs::read(printed.as_bytes(), out_dir, OutputLimits::DEFAULT, g()).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("gen.rs", file.as_bytes(), FileLimits::DEFAULT, g()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = AVec::new_in(g());
    out.encode(&mut bytes);
    let bytes: &'static [u8] = bytes.leak_static();
    OutputsRef::open(bytes).unwrap_or_else(|e| panic!("{e:?}"))
}

fn lock() -> &'static Lock<'static> {
    StdBox::leak(StdBox::new(Lock::read(LOCK.as_bytes(), g()).expect("the lock reads")))
}

fn made(order: &[u32; 9]) -> World<'static> {
    let lock = lock();
    let graph = graph(lock, order);
    let mut store = store(lock);
    let listed = listed(graph, lock, &mut store);
    world(graph, lock, listed, &mut store, checkout(), b()).unwrap_or_else(|e| panic!("{e}"))
}

fn by_name<'w>(w: &'w World<'static>, name: &str) -> StdVec<&'w crate::Crate<'static>> {
    w.crates.iter().filter(|c| c.input.name == name).collect()
}

const IN_ORDER: [u32; 9] = [0, 1, 2, 3, 4, 5, 6, 7, 8];

#[test]
fn the_inputs_hold_what_each_unit_is_given() {
    let w = made(&IN_ORDER);
    let names: StdVec<&str> = w.crates.iter().map(|c| c.input.name).collect();
    // The toolchain first (proc_macro for derive_it, bstd for blz), then the list's units: no
    // build script, no host-side helper (a dependency's proc macro is kept without its
    // dependencies), no target-side helper (unit 8: no member reaches it); the test target is a
    // crate of its own beside its library (a `check` answers for its names too).
    assert_eq!(&names[..5], ["core", "alloc", "std", "proc_macro", "bstd"]);
    let mut held: StdVec<&str> = names[5..].to_vec();
    held.sort();
    assert_eq!(held, ["app", "app", "blz", "derive_it", "testkit", "util"]);
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
    assert!(matches!(util.script, Script::Read { .. }), "its build script's record is read by its tasks");
    assert!(util.input.script.is_none(), "the world holds no script record (D115 (B))");
    assert!(util.input.cfg.contains(&("feature", Some("std"))));
    assert_eq!(util.input.features, ["std"]);
    assert!(util.input.root.ends_with("/src/lib.rs") && !util.input.root.starts_with("/0/"));
}

/// D115 (B): the crate's task joins its script's outputs: the `rustc-cfg` atom joins the unit's
/// cfg and the record is the input's (`env!`, `include!`); the key is the world's, unmoved.
/// Expected: `test tests::a_build_scripts_cfg_joins_its_library_at_task_time ... ok`.
/// Control: drop `cfg.extend(outputs.cfgs())` in `joined`: "the joined input lacks the script's cfg".
#[test]
fn a_build_scripts_cfg_joins_its_library_at_task_time() {
    let w = made(&IN_ORDER);
    let util = by_name(&w, "util")[0];
    assert!(matches!(util.script, Script::Read { .. }));
    assert!(!util.input.cfg.contains(&("has_atomics", None)), "the world's input holds no script atom");
    let input = joined(&util.input, scripts("pub fn made() {}\n", "1"), b());
    assert!(input.cfg.contains(&("has_atomics", None)), "the joined input lacks the script's cfg");
    assert!(util.input.cfg.iter().all(|atom| input.cfg.contains(atom)), "the join dropped a unit atom");
    assert!(input.cfg.windows(2).all(|pair| pair[0] < pair[1]), "the joined cfg is not sorted and each once");
    // The record is the task's input: its generated file and env pair are what expansion reads.
    let record = input.script.unwrap_or_else(|| panic!("the joined input has no script record"));
    assert_eq!(record.file("gen.rs"), Some(blaze_build_script::outputs::OutFileRef::Inline(&b"pub fn made() {}\n"[..])));
    assert_eq!(record.env().collect::<StdVec<_>>(), [("VERSION", "1")]);
    assert_eq!(input.key, util.input.key, "the join moved the crate's key");
    // Only a crate with a script reads one.
    assert_eq!(by_name(&w, "app")[0].script, Script::None);
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

/// D115 (B): a script's outputs are a READ of the crate's task (its `Script` record and out in
/// the read list), never in the crate's key: two outputs that differ in a generated file or an
/// env value join to two inputs under ONE key; what moves is the joined input the task reads.
/// Expected: `test tests::a_crates_key_does_not_hold_its_scripts_outputs ... ok`.
#[test]
fn a_crates_key_does_not_hold_its_scripts_outputs() {
    let w = made(&IN_ORDER);
    let util = by_name(&w, "util")[0];
    let first = joined(&util.input, scripts("pub fn made() {}\n", "1"), b());
    let file = joined(&util.input, scripts("pub fn made() { }\n", "1"), b());
    let env = joined(&util.input, scripts("pub fn made() {}\n", "2"), b());
    assert_eq!(first.key, file.key);
    assert_eq!(first.key, env.key);
    assert_eq!(first.key, util.input.key);
    // What the task reads does move (control: the record differs where the inputs differ).
    assert_ne!(first.script.map(|r| r.file("gen.rs")), file.script.map(|r| r.file("gen.rs")));
    assert_ne!(first.script.map(|r| r.env().collect::<StdVec<_>>()), env.script.map(|r| r.env().collect::<StdVec<_>>()));
}

/// Ruling D115 (2): one subject function from the list key's parts; the SIDE tells a target
/// copy's run from a macro-side copy's, so a unit on both sides (a proc macro kept on the target
/// side and reached on the macro side) names two runs, and each crate its own.
/// Expected: `test tests::a_scripts_subject_is_its_crates_side ... ok`.
#[test]
fn a_scripts_subject_is_its_crates_side() {
    let subject = |side, features: &[&str]| crate::script_subject("util", "0.1.0", side, features, g());
    assert_eq!(subject(crate::Side::Target, &["std"]), subject(crate::Side::Target, &["std"]));
    assert_ne!(subject(crate::Side::Target, &["std"]), subject(crate::Side::Macro, &["std"]), "the side left the subject unchanged");
    assert_ne!(subject(crate::Side::Target, &["std"]), subject(crate::Side::Target, &[]), "the features left the subject unchanged");
    // The world names util's run by that function, its side's.
    let w = made(&IN_ORDER);
    let util = by_name(&w, "util")[0];
    assert_eq!(util.script, Script::Read { subject: crate::script_subject(util.package, util.version, util.side, util.input.features, g()) });
}

/// Two units under one crate key that name two runs are refused naming both (built by hand:
/// unreachable through `world`, whose subject is made of the key's parts).
/// Expected: `test tests::two_runs_under_one_crate_key_are_refused ... ok`.
/// Control: `one_script` returning `Ok(())` always: "two runs under one key passed".
#[test]
fn two_runs_under_one_crate_key_are_refused() {
    let target = Script::Read { subject: crate::script_subject("util", "0.1.0", crate::Side::Target, &[], g()) };
    let other = Script::Read { subject: crate::script_subject("util", "0.1.0", crate::Side::Macro, &[], g()) };
    assert!(crate::list::one_script(("util", target), ("util", target)).is_ok());
    match crate::list::one_script(("util", target), ("util_copy", other)) {
        Err(WorldError::ScriptClash { first, second }) => assert_eq!((first, second), ("util", "util_copy")),
        Err(other) => panic!("another refusal: {other}"),
        Ok(()) => panic!("two runs under one key passed"),
    }
}

/// A toolchain crate's key is its identity: the same pack on another target's cfg is another
/// crate, and a changed `std` moves `std`'s key and every unit's implicit key (rule 21). The
/// key holds the pack's digest, so a crate moves when any crate of its pack does (one library
/// pack: a toolchain bump moves them all). Control: the same inputs give equal keys.
#[test]
fn a_toolchain_crates_key_covers_its_target_and_dependencies() {
    let needs = crate::Needs { std: false, bstd: false, proc_macro: true };
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
    made(&IN_ORDER).write(&mut first);
    let orders: [[u32; 9]; 4] = [[8, 7, 6, 5, 4, 3, 2, 1, 0], [5, 0, 7, 3, 1, 8, 2, 6, 4], [4, 6, 2, 8, 0, 3, 7, 1, 5], [7, 5, 3, 1, 8, 6, 4, 2, 0]];
    for order in &orders {
        let mut other = AVec::new_in(g());
        made(order).write(&mut other);
        assert!(first == other, "order {order:?} gives other inputs");
    }
    // Control: a changed input changes the bytes.
    let mut changed = made(&IN_ORDER);
    let at = changed.crates.iter().position(|c| c.input.name == "util").unwrap_or_else(|| panic!("no util"));
    changed.crates[at].input.cfg = &[];
    let mut other = AVec::new_in(g());
    changed.write(&mut other);
    assert!(first != other);
}

#[test]
fn a_dependency_on_a_unit_not_kept_is_refused_by_name() {
    let lock = lock();
    let mut units = graph(lock, &IN_ORDER).units.clone();
    // app's library made to depend on helper's host-side unit.
    units[5].deps.push(UnitDep { name: t("helper"), unit: UnitId(2) });
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(g()), left_out: AVec::new_in(g()) }));
    let mut store = store(lock);
    match listed_or(graph, lock, &mut store) {
        Err(WorldError::Units(blaze_scope::units::UnitsError::DependencyNotKept { krate, dep, dep_for_host, .. })) => {
            assert_eq!((krate, dep, dep_for_host), ("app", "helper", true))
        }
        Err(other) => panic!("another error: {other}"),
        Ok(_) => panic!("taken"),
    }
}

#[test]
fn an_absent_source_is_an_error_naming_the_package() {
    let lock = lock();
    let graph = graph(lock, &IN_ORDER);
    let mut store = store(lock);
    store.packs.remove("testkit");
    let listed = listed(graph, lock, &mut store);
    match world(graph, lock, listed, &mut store, checkout(), b()) {
        Err(e) => assert!(format!("{e}").contains("testkit"), "{e}"),
        Ok(_) => panic!("taken"),
    }
}

#[test]
fn the_world_resolves() {
    let w = made(&IN_ORDER);
    let inputs = w.inputs(g());
    let names = w.resolve(&inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let app = by_name(&w, "app")[0];
    let at = w.crates.iter().position(|c| core::ptr::eq(c, app)).expect("app");
    let c = names.krate(at);
    // Its root, `inner` from its file, `off` cfg'd out.
    assert_eq!(c.tree.modules.len(), 2, "{:?}", c.tree.notes);
    let errors: usize = c.done.values().map(|r| r.scope.errors.len()).sum();
    assert_eq!(errors, 0, "app's imports all resolve");
}

/// D96 P2 and P3: the command's texts answer each crate's environment (before, `Texts` had no
/// `crate_env` and every `env!` in the command was refused). What a crate SEES as its root never
/// depends on the workspace: a dependency (registry, git, toolchain) sees the one root
/// `/package`; a member sees `/workspace/<member dir>`. The cargo book, "Environment variables
/// Cargo sets for crates": "CARGO_MANIFEST_DIR — The directory containing the manifest of your
/// package." Prints `D96 P2: util sees /package, app sees /workspace/app`.
#[test]
fn a_dependency_sees_the_one_package_root_and_a_member_its_workspace_dir() {
    use blaze_scope::resolve::SourceTexts;
    let w = made(&IN_ORDER);
    let dir = |name: &str| {
        let at = w.crates.iter().position(|c| c.input.name == name).unwrap_or_else(|| panic!("no crate `{name}`"));
        let env = w.texts.crate_env(at).unwrap_or_else(|| panic!("`{name}`: the command's texts give no crate environment"));
        (env.get("CARGO_MANIFEST_DIR"), env.get("CARGO_MANIFEST_PATH"))
    };
    assert_eq!(dir("util"), (Ok(Some("/package")), Ok(Some("/package/Cargo.toml"))));
    assert_eq!(dir("core").0, Ok(Some("/package")), "a toolchain crate is a dependency too");
    assert_eq!(dir("app"), (Ok(Some("/workspace/app")), Ok(Some("/workspace/app/Cargo.toml"))));
    std::println!("D96 P2: util sees /package, app sees /workspace/app");
}

/// D96 P4: `include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/more.rs"))` in a dependency
/// reads its own `src/more.rs`: an absolute path under the root the crate sees is served from
/// its package, a tracked read (before, any absolute written path was refused as leaving the
/// root). The cargo book: "CARGO_MANIFEST_DIR — The directory containing the manifest of your
/// package"; core `macros/mod.rs`, `include!`: "Parses a file as an expression or an item
/// according to the context." Prints `D96 P4: util read /package/src/more.rs`.
#[test]
fn an_include_under_the_crates_own_root_is_served() {
    let w = made(&IN_ORDER);
    let inputs = w.inputs(g());
    let names = w.resolve(&inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let at = w.crates.iter().position(|c| c.input.name == "util").expect("util");
    let c = names.krate(at);
    let made: StdVec<&str> = c.expansions.values().filter_map(|e| e.made.map(|m| m.text)).collect();
    assert!(made.contains(&"pub fn more( ) { }"), "util's include of its own file by CARGO_MANIFEST_DIR: {made:?}; notes: {:?}", c.notes);
    std::println!("D96 P4: util read /package/src/more.rs");
}

/// D96 (4): `file!()` in a dependency names its file `<name>-<version>/<path>` (here
/// `util-1.0.0/src/lib.rs`), the same in every workspace and saying which crate; util includes
/// `concat!(env!("CARGO_MANIFEST_DIR"), "/src/", file!())`, and only that answer reads its
/// `src/util-1.0.0/src/lib.rs` (`pub fn named`). Before, the item host answered the
/// package-relative `src/lib.rs`, which names `/package/src/src/lib.rs`: no such file. core
/// `macros/mod.rs`, `file!`: "Expands to the file name in which it was invoked." Prints
/// `D96 file!: util-1.0.0/src/lib.rs`.
#[test]
fn file_in_a_dependency_is_its_name_version_and_path() {
    let w = made(&IN_ORDER);
    let inputs = w.inputs(g());
    let names = w.resolve(&inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let at = w.crates.iter().position(|c| c.input.name == "util").expect("util");
    let c = names.krate(at);
    let made: StdVec<&str> = c.expansions.values().filter_map(|e| e.made.map(|m| m.text)).collect();
    assert!(made.contains(&"pub fn named( ) { }"), "util's include by file!(): {made:?}; notes: {:?}", c.notes);
    std::println!("D96 file!: util-1.0.0/src/lib.rs");
}

/// `module_path!()`, `line!()` and `column!()` in eager input at item position have real
/// answers (before, refused: the call lost every name it declared). util's line 18 is
/// `  #[allow(unused)] include!(concat!(.., module_path!(), "-", line!(), "-", column!(), ".rs"))`:
/// core `macros/mod.rs`: `module_path!` "expands to a string that represents the current
/// module path" (util's root: `util`); `line!`/`column!` give "the first macro invocation
/// leading up to the expansion" (the `include!`, whose path starts at character 20: the
/// attribute and the indent are not the call), "1-based". Only that answer reads
/// `src/util-18-20.rs` (`pub fn placed`). Prints `D96 place: util-18-20.rs`.
#[test]
fn module_path_line_and_column_in_eager_input_are_the_call_s() {
    let w = made(&IN_ORDER);
    let inputs = w.inputs(g());
    let names = w.resolve(&inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let at = w.crates.iter().position(|c| c.input.name == "util").expect("util");
    let c = names.krate(at);
    let made: StdVec<&str> = c.expansions.values().filter_map(|e| e.made.map(|m| m.text)).collect();
    assert!(made.contains(&"pub fn placed( ) { }"), "util's include by module_path!/line!/column!: {made:?}; notes: {:?}", c.notes);
    std::println!("D96 place: util-18-20.rs");
}

/// D96 P3, rule 21: a member sees `/workspace/<its dir>` (`env!("CARGO_MANIFEST_DIR")`,
/// `file!()`), so its directory is an input of everything made for it and must be in its crate
/// key: one member moved from `app` to `crates/app` gets another key, and its dependent `blz`
/// too; a dependency (`util`) keeps its key. Prints `D96 P3: a moved member is another key`.
#[test]
fn a_members_directory_is_in_its_crate_key() {
    let lock = lock();
    let graph = graph(lock, &IN_ORDER);
    let mut here = store(lock);
    let mut moved = store(lock);
    moved.app_dir = "crates/app";
    let (listed_here, listed_moved) = (listed(graph, lock, &mut here), listed(graph, lock, &mut moved));
    let a = world(graph, lock, listed_here, &mut here, checkout(), b()).unwrap_or_else(|e| panic!("{e}"));
    let b = world(graph, lock, listed_moved, &mut moved, checkout(), b()).unwrap_or_else(|e| panic!("{e}"));
    let key = |w: &World<'static>, name: &str| by_name(w, name)[0].input.key;
    assert_ne!(key(&a, "app"), key(&b, "app"), "`app` moved to crates/app kept its key: its directory is not in it");
    assert_ne!(key(&a, "blz"), key(&b, "blz"), "`blz` depends on the moved `app`");
    assert_eq!(key(&a, "util"), key(&b, "util"), "a dependency's key does not hold where a member is");
    std::println!("D96 P3: a moved member is another key");
}

/// D96 P1: a dependency's scope and items records are a function of the dependency, not of the
/// workspace using it. The second workspace has no proc macro (derive_it is a plain library
/// there), so it keeps one toolchain crate fewer (`proc_macro`): bstd and every fetched package
/// sit at another tree number. The same crate under the same key must still write byte-equal
/// names and items records (before D96 the items record's file rows held `/<tree>/...`).
/// Prints `D96 P1: <n> crates' records equal in two workspaces`.
#[test]
fn a_dependencys_records_are_equal_in_two_workspaces() {
    let first = made(&IN_ORDER);
    let lock = lock();
    let mut units = graph(lock, &IN_ORDER).units.clone();
    units[3].kind = UnitKind::Lib;
    units[3].for_host = false;
    // A target-side library depends on the target's helper, not the host's.
    units[3].deps[0].unit = UnitId(8);
    let other: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(g()), left_out: AVec::new_in(g()) }));
    let mut store = store(lock);
    let listed = listed(other, lock, &mut store);
    let second = world(other, lock, listed, &mut store, checkout(), b()).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(first.toolchain_crates, second.toolchain_crates, "the two workspaces keep the same toolchain crates");

    let (a_inputs, b_inputs) = (first.inputs(g()), second.inputs(g()));
    let a = first.resolve(&a_inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let b = second.resolve(&b_inputs, &Serial, b()).unwrap_or_else(|e| panic!("{e}"));
    let mut equal = 0;
    for name in ["util", "testkit", "bstd", "std", "core"] {
        let at = |w: &World<'static>| w.crates.iter().position(|c| c.input.name == name).unwrap_or_else(|| panic!("no crate `{name}`"));
        let (i, j) = (at(&first), at(&second));
        assert_eq!(first.crates[i].input.key, second.crates[j].input.key, "`{name}`: another key");
        let (x, y) = (a.krate(i), b.krate(j));
        let names = |n| blaze_scope::names_record::encode(n, g()).unwrap_or_else(|e| panic!("`{name}`'s names record: {e:?}"));
        let items = |n| blaze_scope::items_record::encode(n, g()).unwrap_or_else(|e| panic!("`{name}`'s items record: {e:?}"));
        assert!(names(x).as_slice() == names(y).as_slice(), "`{name}`: the names records differ between the two workspaces");
        assert!(
            items(x).as_slice() == items(y).as_slice(),
            "`{name}`: the items records differ between the two workspaces (its files at {:?} and {:?})",
            first.crates[i].input.root,
            second.crates[j].input.root
        );
        equal += 1;
    }
    std::println!("D96 P1: {equal} crates' records equal in two workspaces");
}
