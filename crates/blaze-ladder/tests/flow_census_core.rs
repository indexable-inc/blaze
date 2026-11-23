//! The flow census over REAL PROJECT CODE (rule 7): the toolchain's `core` (or `alloc`, `std`:
//! `FLOW_CENSUS_CRATE`) from the installed shelf, every fn body with a text checked by the
//! engine's own move and borrow check, `ProjectWorld::check_borrows` (front2/borrows.rs), as
//! tests/flow_census.rs checks bstd's: the same path, report and gate line
//! (tests/flow_census_shared/mod.rs). Every body here compiles under rustc, so every finding is
//! false and every refusal is a gap of the engine.
//!
//! The program is a plain Rust probe (`fn main() {}`, edition 2024) whose crates are the
//! toolchain's, its names solved into one frame of 32 GiB of address space (committed only as
//! used), as tests/front2_core_from_prim.rs does.
//!
//! Environment: `FLOW_CENSUS_CRATE` (default `core`), `FLOW_CENSUS_CAP` (default 3000 bodies,
//! taken in key order), `FLOW_CENSUS_OUT` (a file the report is written to). Needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`. Expected time, a debug build: the toolchain's
//! names solved in under a minute, then about 50 to 100 ms a body (core's bodies are larger and
//! more generic than bstd's 18 ms), so 3 to 6 minutes at the default cap.

#![feature(allocator_api)]

mod flow_census_shared;

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::refusal::SourceFile;
use blaze_mem::Frame;
use blaze_own::Erased;

const MAIN: &str = "fn main() {}\n";

#[test]
fn flow_census_core() {
    let start = Instant::now();
    let want = std::env::var("FLOW_CENSUS_CRATE").ok();
    let cap: usize = std::env::var("FLOW_CENSUS_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(3000);
    let out_path = std::env::var("FLOW_CENSUS_OUT").ok();
    // The test's one allocator, made at its top: a frame kept for the process (the store and
    // the front end take `Alloc<'static>`), as tests/front2_core_from_prim.rs keeps it.
    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/probe/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "probe", dir: "/probe/", root: "/probe/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let cname = want.as_deref().unwrap_or("core");
    let krate = crates.iter().position(|c| c.name == cname).unwrap_or_else(|| panic!("no crate named `{cname}` among the probe's crates (FLOW_CENSUS_CRATE)"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut world = ProjectWorld::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
    let report = flow_census_shared::census(&names, &mut world, krate, cname, cap, frame, a, start);
    flow_census_shared::finish(&report, out_path.as_deref());
}
