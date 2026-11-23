//! The flow census (docs/v2/plan/design/flow.md, "The census"): every fn body of bstd checked by
//! THE ENGINE'S OWN move and borrow check, `ProjectWorld::check_borrows` (front2/borrows.rs), the
//! function `blaze check`'s body tasks call (blaze-cli body_types.rs): its types, then, when they
//! were checked clean, its facts read from the heads, its own flow and each closure's and async
//! block's body as a flow of its own (borrows_nested.rs), each move and borrow checked. One path
//! (rule 30): the census makes no fact of its own (the stand-ins it had, lifetimes read from a
//! `'` in a head's text and closures refused because its facts said they were not checked, are
//! gone). bstd compiles under the pinned compiler, so every finding is a bug of the engine.
//!
//! The report and its gate line (`gate: bodies B, whole W, refused R, false findings F`) are
//! tests/flow_census_shared/mod.rs's, shared with tests/flow_census_core.rs (the toolchain's
//! `core`). With blaze-cli's `BORROWS_ARE_ERRORS` true, R + F > 0 fails this test.
//!
//! `FLOW_CENSUS_OUT` names the output file; `FLOW_CENSUS_CAP` caps the bodies (key order).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. About a minute in a debug
//! build (the census before it: 52 s for 2,898 bodies).

#![feature(allocator_api)]

mod flow_census_shared;

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::ProjectWorld;
use blaze_ladder::refusal::SourceFile;
use blaze_mem::Frame;
use blaze_own::Erased;

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

#[test]
fn flow_census() {
    let start = Instant::now();
    let cap: usize = std::env::var("FLOW_CENSUS_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let out_path = std::env::var("FLOW_CENSUS_OUT").ok();
    // The test's one allocator, made at its top: a frame kept for the process (the store and
    // the front end take `Alloc<'static>`), as tests/front2_core_from_prim.rs keeps it; 32 GiB of
    // address space, committed only as used.
    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc_ = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc_).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let cname = if crates.iter().any(|c| c.name == "bstd") { "bstd" } else { "blaze_std" };
    let krate = crates.iter().position(|c| c.name == cname).expect("bstd is a crate");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let mut world = ProjectWorld::new(&names, frame, &sources, a).unwrap_or_else(|e| panic!("front2: {e}"));
    let report = flow_census_shared::census(&names, &mut world, krate, cname, cap, frame, a, start);
    flow_census_shared::finish(&report, out_path.as_deref());
}
