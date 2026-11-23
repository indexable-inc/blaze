//! `blaze check`'s TYPES stage entry (front2/project.rs) on a copy of the owner's hello world
//! with one more body that is a type error: the Reference, "Literal expressions": a string
//! literal has type `&'static str`, so `let x: u8 = "a";` is a mismatch (E0308). His `main`
//! must come back clean with its read list's digest; the bad body with exactly one E0308 whose
//! range is item-relative and covers the literal. Prints each part's cost: names, the world,
//! the first body, the next. Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use std::time::Instant;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;

const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs` as written, plus a body with a type error.
const MAIN: &str = "fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}

fn bad() {
    let x: u8 = \"a\";
}
";

#[test]
fn own_bodies_checked_with_item_relative_diagnostics() {
    let start = Instant::now();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let t_names = start.elapsed();

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let t = Instant::now();
    let mut world = ProjectWorld::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let t_world = t.elapsed();
    let bodies = world.bodies(Alloc::global());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let listed: Vec<(String, Option<u32>)> = bodies.iter().map(|(b, at)| (name_of(b), at.map(|a| a.start))).collect();
    println!("bodies: {listed:?}");
    let main = bodies.iter().find(|(b, _)| name_of(b) == "main").map(|(b, _)| *b).expect("`main` listed");
    let bad = bodies.iter().find(|(b, _)| name_of(b) == "bad").map(|(b, _)| *b).expect("`bad` listed");

    let t = Instant::now();
    let m = world.check(main, Alloc::global());
    let t_first = t.elapsed();
    let t = Instant::now();
    let b = world.check(bad, Alloc::global());
    let t_next = t.elapsed();
    let t = Instant::now();
    let again = world.check(main, Alloc::global());
    let t_again = t.elapsed();
    println!("COST: names {t_names:?}; world {t_world:?}; first body (main) {t_first:?}; next body (bad) {t_next:?}; main again {t_again:?}");
    println!("main: {m:?}");
    println!("bad: {b:?}");
    assert!(m.is_clean(), "main: {m:?}");
    assert!(m.reads.is_some() && m.reads == again.reads, "main's read digest: {:?} then {:?}", m.reads, again.reads);
    assert!(b.gaps.is_empty() && b.refused.is_none(), "bad: {b:?}");
    assert_eq!(b.diagnostics.len(), 1, "bad: {b:?}");
    let d = &b.diagnostics[0];
    assert_eq!(d.code, "E0308", "bad: {b:?}");
    let item = names.item_text(match bad { BodyRef::Free(k) => k, BodyRef::Member { container, .. } => container }).expect("bad's text").text;
    let shown = &item[d.start as usize..d.end as usize];
    println!("bad's E0308 covers {shown:?} of its item text");
    assert!(shown.contains("\"a\""), "the range {}..{} of {item:?} is {shown:?}", d.start, d.end);
}
