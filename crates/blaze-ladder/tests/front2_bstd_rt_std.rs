//! bstd's `mod rt` writes `extern crate std;` (blaze-std rt.rs:21; bstd is `#![no_std]`, so
//! `std` is in no extern prelude of it), and names `std::thread::ThreadId` and
//! `std::process::Termination` there. Over the installed toolchain's crates, the path
//! `std::thread::ThreadId` in `rt` must resolve over the solved names, and over the records the
//! same (front2 source.rs `Records`, `NamesSource::walk`): hello world's `blaze run` refused it
//! `NotFound` at segment 0 (2026-11-17).
//!
//! Also: the alias `rt::ThreadMark`'s row names `rt` as its module over the records, bstd's own
//! bodies `thread_mark` and `Placed::hosted_root` check clean over them, and Rust's std is in
//! the toolchain closure of a blaze-edition program (blaze-scope units.rs `toolchain_crates`).
//! All of these held on 2026-11-17 04:55 (tree a2b1388119 + this file): the run's refusal is
//! not reproduced over records the names library writes here, so its cause is in what the
//! command's own records hold for bstd, not in the resolution over records: blaze-cli
//! names/scope.rs `sysroot_given` gives a toolchain crate only the sysroot crates its shelf row
//! names, and the row (blaze-toolchain implied.rs `root_facts`) reads `extern crate` in the
//! root module only, so bstd's row names no `std` and `extern crate std;` in `rt` binds nothing.
//!
//! Prints `bstd rt: solved .. over records ..` and the `std` bindings each side holds.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use std::collections::HashMap;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_scope::names::Written;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main() {}\n";

struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

#[test]
fn bstd_rt_names_std_over_records_as_over_names() {
    let g = blaze_own::Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let files: &'static [(String, String)] = Box::leak(Box::new([("/hb/src/main.rs".to_string(), MAIN.to_string())]));
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user: &'static UserCrate<'static> =
        Box::leak(Box::new(UserCrate { name: "hb", dir: "/hb/", root: "/hb/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates = Box::leak(Box::new(inputs.crates()));
    let names = Box::leak(Box::new(inputs.resolve(crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))));

    let all: Vec<&str> = names.crates().iter().map(|c| c.name).collect();
    println!("crates: {all:?}");
    let bstd = names.crates().iter().position(|c| c.name == "bstd" || c.name == "blaze_std").unwrap_or_else(|| panic!("bstd among {all:?}"));
    let c = &names.crates()[bstd];
    println!("bstd: root attrs {:?}, notes {:?}", c.root_attrs, c.notes);
    let root = c.root;
    let rt = names
        .modules(bstd)
        .find(|m| m.parent == Some(root) && (m.path == "rt" || m.path.ends_with("::rt") || m.path.ends_with("/rt") || m.path.ends_with("rt.rs")))
        .map(|m| m.key)
        .unwrap_or_else(|| panic!("bstd's `mod rt`: {:?}", names.modules(bstd).filter(|m| m.parent == Some(root)).map(|m| m.path).collect::<Vec<_>>()));
    println!("bstd rt module {rt:?}, failed {:?}", c.failed.get(&rt));
    println!("bstd rt `std` solved binding: {:?}", names.lookup(rt, Namespace::Types, "std"));

    // bstd is the owner (reader 0), as when its own bodies are checked; then every other crate.
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    let order: Vec<usize> = std::iter::once(bstd).chain((0..names.crates().len()).filter(|&i| i != bstd)).collect();
    for &i in order.iter() {
        let c = &names.crates()[i];
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        crate_names.push(c.name);
        keys.push(c.key);
    }
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static blaze_ladder::front2::source::Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(crate_names), g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(Vec::leak(keys), load, log, frame, g)));

    for segments in [&["std", "thread", "ThreadId"][..], &["std", "process", "Termination"][..]] {
        let written = Written { kind: PathKind::Plain, segments };
        let solved = names.path(rt, Namespace::Types, written, g).map(|r| (r.def.key, r.consumed));
        let over_records = NamesSource::walk(records, rt, Namespace::Types, written).map(|r| (r.def.key, r.consumed));
        println!("bstd rt {segments:?}: solved {solved:?}; over records {over_records:?}");
        let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
        println!("records filed: {errors:?}");
        assert!(solved.is_ok(), "the solved names resolve {segments:?} in bstd's rt: {solved:?}");
        assert_eq!(over_records, solved, "{segments:?} over records as over the solved names");
    }

    // The alias `rt::ThreadMark`: its row's module over the records is `rt`.
    let alias = names
        .items(bstd)
        .map(|(k, _, _)| k)
        .find(|k| names.item_text(*k).and_then(|t| t.name) == Some("ThreadMark"))
        .expect("bstd's `type ThreadMark`");
    let home = NamesSource::text_of(records, alias).map(|t| t.module);
    println!("ThreadMark {alias:?}: solved module {:?}, over records {home:?}, rt {rt:?}", names.item_text(alias).map(|t| t.module));
    assert_eq!(home, Some(rt), "the alias's row names `rt` as its module");

    // bstd's own bodies over its records: `thread_mark() -> ThreadMark` makes the alias's head
    // in `rt`, as the lazy program does for `Placed`'s row.
    let world_frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the world's frame")));
    let lang: Vec<usize> = (0..order.len()).filter(|&i| order[i] != 0).collect();
    let mut world = blaze_ladder::front2::project::ProjectWorld::from_records(records, world_frame, &lang, blaze_own::testing::heap().alloc())
        .unwrap_or_else(|e| panic!("bstd's world over records: {e}"));
    let bodies: Vec<_> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    let named = |b: &blaze_ladder::front2::project::BodyRef<'_>| match b {
        blaze_ladder::front2::project::BodyRef::Free(k) => NamesSource::name_of(records, *k).unwrap_or("?").to_string(),
        blaze_ladder::front2::project::BodyRef::Member { name, .. } => name.to_string(),
    };
    for want in ["thread_mark", "hosted_root"] {
        let Some(b) = bodies.iter().find(|b| named(b) == want) else {
            println!("body {want}: not listed");
            continue;
        };
        let v = world.check(*b, g);
        println!("body {want}: diagnostics {:?}, refused {:?}", v.diagnostics, v.refused.as_ref().map(|r| r.text()));
        if want == "thread_mark" {
            assert!(v.refused.is_none() && v.diagnostics.is_empty(), "thread_mark over records: {:?} {:?}", v.diagnostics, v.refused.as_ref().map(|r| r.text()));
        }
    }
    // The closure blaze-scope units.rs `toolchain_crates` keeps for a blaze-edition program
    // (no crate of it is Rust-std: `rust_std` false): core, alloc and bstd over the rows' edges.
    let id = blaze_toolchain::PINNED;
    let manifest = blaze_toolchain::shelf::open_manifest(&mut DirShelf::new(shelf.clone()), id).expect("the manifest");
    let view = blaze_toolchain::ToolchainView::open(manifest.as_ref(), id).expect("open_manifest checked it");
    let triple = blaze_ladder::crates::TRIPLE;
    let sys = blaze_toolchain::sysroot(&view, triple, false).expect("the target's sysroot");
    let all: Vec<_> = view.units(triple).expect("the target's units").collect();
    let mut keep = vec![false; all.len()];
    let mut todo = vec![sys.core.0 as usize, sys.alloc.0 as usize, sys.bstd.0 as usize];
    while let Some(u) = todo.pop() {
        if keep[u] {
            continue;
        }
        keep[u] = true;
        if all[u].kind != blaze_toolchain::UnitKind::ProcMacro {
            todo.extend(all[u].deps().map(|(_, d)| d as usize));
        }
    }
    let bstd_deps: Vec<&str> = all[sys.bstd.0 as usize].deps().map(|(n, _)| n).collect();
    let std_by: Vec<&str> = (0..all.len()).filter(|&u| keep[u] && all[u].deps().any(|(_, d)| d == sys.std.0)).map(|u| view.krate(all[u].krate).lib).collect();
    println!("blaze program closure: std kept {}; bstd's row edges {bstd_deps:?}; std named by {std_by:?}", keep[sys.std.0 as usize]);
    // bstd's own row names no `std` (blaze-toolchain implied.rs reads `extern crate` in the root
    // module only, and bstd writes it in `rt`): std is in the closure through the crates under
    // bstd that name it. Were it not, `extern crate std;` in `rt` would bind nothing.
    assert!(keep[sys.std.0 as usize], "Rust's std is in a blaze program's toolchain closure");
}

/// blaze-toolchain implied.rs `crate_facts` over the INSTALLED shelf's packs, every target-side
/// library of the host target, as the cutter will read them at the next cut: no crate's files
/// are refused (a `#[path]` into stdarch, a module file the pack lacks), bstd now asks for
/// `std` (its `extern crate std;` in `mod rt`), and the rows with the edges this adds still
/// form no cycle. Read only: nothing is cut or written. Prints `crate facts on the shelf: N
/// libraries read, edges gained [..]`.
#[test]
fn the_shelfs_crates_read_with_every_module_file() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf_dir = format!("{home}/.blaze/v2/toolchain");
    let mut shelf = DirShelf::new(shelf_dir.clone());
    let id = blaze_toolchain::PINNED;
    let manifest = blaze_toolchain::shelf::open_manifest(&mut shelf, id).expect("the manifest");
    let view = blaze_toolchain::ToolchainView::open(manifest.as_ref(), id).expect("open_manifest checked it");
    let triple = blaze_ladder::crates::TRIPLE;
    let sys = blaze_toolchain::sysroot(&view, triple, false).expect("the target's sysroot");
    let units = blaze_toolchain::SysrootUnits { core: sys.core.0, alloc: sys.alloc.0, std: sys.std.0 };
    let target_cfg: Vec<(&str, Option<&str>)> = view.cfg(triple).expect("the target's cfg").collect();
    let all: Vec<_> = view.units(triple).expect("the target's units").collect();
    let mut deps: Vec<Vec<u32>> = all.iter().map(|u| u.deps().map(|(_, d)| d).collect()).collect();
    let (mut read, mut gained, mut refused) = (0, Vec::new(), Vec::new());
    for (at, row) in all.iter().enumerate() {
        if row.kind != blaze_toolchain::UnitKind::Lib || row.for_host {
            continue;
        }
        let krate = view.krate(row.krate);
        let pack = blaze_toolchain::shelf::open_pack(&mut shelf, id, &krate).unwrap_or_else(|e| panic!("{}: {e}", krate.name));
        let pack = blaze_fetch::PackView::open_verified(pack.as_ref()).unwrap_or_else(|e| panic!("{}: {e:?}", krate.name));
        let root = if krate.dir.is_empty() || row.root.starts_with(krate.dir) { row.root.to_string() } else { format!("{}/{}", krate.dir, row.root) };
        let mut cfg = target_cfg.clone();
        cfg.extend(row.features().map(|f| ("feature", Some(f))));
        let file = |p: &str| pack.find(p).and_then(|f| std::str::from_utf8(f.bytes).ok());
        match blaze_toolchain::implied::crate_facts(&root, file, &cfg, blaze_own::Alloc::global()) {
            Ok(facts) => {
                read += 1;
                let explicit: Vec<&str> = row.deps().map(|(n, _)| n).collect();
                for (edge, unit) in blaze_toolchain::implied::implied_edges(&facts, units, |n| explicit.contains(&n)).into_iter().flatten() {
                    gained.push(format!("{} +{edge}", krate.lib));
                    deps[at].push(unit);
                }
            }
            Err(e) => refused.push(format!("{} {}: {e}", krate.name, krate.version)),
        }
    }
    println!("crate facts on the shelf: {read} libraries read, edges gained {gained:?}");
    assert!(refused.is_empty(), "refused: {refused:#?}");
    let lib = view.krate(all[sys.bstd.0 as usize].krate).lib;
    assert!(
        deps[sys.bstd.0 as usize].contains(&sys.std.0),
        "bstd ({lib}) reaches std (explicit or implied): gained {gained:?}"
    );
    let slices: Vec<&[u32]> = deps.iter().map(Vec::as_slice).collect();
    let cycle = blaze_toolchain::implied::first_in_cycle(&slices, blaze_own::Alloc::global()).expect("memory");
    assert_eq!(cycle.map(|u| view.krate(all[u].krate).lib), None, "the gained edges make a cycle");
}
