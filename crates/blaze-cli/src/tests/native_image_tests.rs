//! The `Links` and `Native` tasks of `blaze run` (native_image.rs): a crate's link table is its
//! `CrateScope` record's `links` section, and the program's image reads every closure crate's
//! table. A child of main.rs's tests: its helpers are theirs.

use blaze_drive::Kind;
use blaze_ladder::native_links::{Table, open_table};

use super::*;
use crate::native_image::WATCHED;

/// A package `app` whose binary is `src/main.rs` (the rung-3 form: no `core`, its own `main`)
/// and depends by path on `lib`, a library holding `body`. `blaze run` of it names the program
/// `src/main.rs` (main.rs `request`), so its `Native` task links `app` and `lib`.
fn two_crates(io: &Io, app: &str, lib: &str, body: &str) {
    write(
        io,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"{app}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{lib} = {{ path = \"{lib}\" }}\n\n[profile.dev]\noverflow-checks = false\n"
        ),
    );
    write(io, "src/main.rs", include_str!("../../tests/ladder/3-hello/src/main.rs"));
    write(io, &format!("{lib}/Cargo.toml"), &format!("[package]\nname = \"{lib}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"));
    write(io, &format!("{lib}/src/lib.rs"), &format!("#![feature(no_core)]\n#![no_core]\n\n{body}"));
}

/// The `Native` record made for the program whose packages include `app`, as the watch kept it.
fn native_of(app: &str) -> Option<crate::native_image::WatchedNative> {
    let mut watched = WATCHED.lock();
    let at = watched.natives.iter().position(|native| native.packages.iter().any(|(package, _)| package == app))?;
    Some(watched.natives.swap_remove(at))
}

/// A crate whose lib is `#[link(name = "z")] extern "C" { fn deflate(); }`: `blaze run` of a
/// program depending on it makes its `Links` record, and that record's payload is exactly the
/// `links` section of the crate's `CrateScope` payload it was read from (names/sections.rs),
/// a table naming the library `z` and the foreign function `deflate` (blaze-ladder
/// native_links.rs `open_table`).
///
/// A `Links` worker that copied another section, or a scope that filed no table, fails the
/// equality or the table's open.
///
/// Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
/// blaze-cli test native_image_tests`:
/// `native_image: zl's Links record is its scope's link table: link z, foreign [deflate], <n> bytes`
/// and `test tests::native_image_tests::a_link_block_is_its_crates_links_record ... ok`.
#[test]
fn a_link_block_is_its_crates_links_record() {
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    two_crates(&io, "zapp", "zl", "#[link(name = \"z\")]\nextern \"C\" {\n    fn deflate();\n}\n");
    let ran = ask(&io, (Command::Run, "run"), SMALL);
    let out = stdout(&io);
    let native = native_of("zapp").unwrap_or_else(|| panic!("no `Native` record of `zapp` was made ({:?}):\n{out}", ran.as_ref().err()));
    let (_, zl) = native.packages.iter().find(|(package, _)| package == "zl").unwrap_or_else(|| panic!("the image did not link `zl`: {:?}", native.packages));
    let made = {
        let watched = WATCHED.lock();
        watched.links.iter().find(|links| links.name == *zl).map(|links| (links.payload.clone(), links.scope.clone()))
    };
    let (payload, scope) = made.unwrap_or_else(|| panic!("no `Links` record of `zl` was made:\n{out}"));
    let section = crate::names::sections::sections(&scope).unwrap_or_else(|e| panic!("`zl`'s `CrateScope` payload: {e}")).links;
    assert_eq!(payload.as_slice(), section, "`zl`'s `Links` record is not its scope's `links` section");
    let alloc = blaze_own::testing::heap().alloc();
    let Ok(Table::Links(table)) = open_table(&payload, alloc) else { panic!("`zl`'s `Links` record is no link table") };
    let decls: std::vec::Vec<&str> = table.decls.iter().map(|decl| decl.name).collect();
    let foreign: std::vec::Vec<&str> = table.foreign.iter().copied().collect();
    assert_eq!((decls.as_slice(), foreign.as_slice()), (&["z"][..], &["deflate"][..]), "`zl`'s link table");
    std::println!("native_image: zl's Links record is its scope's link table: link z, foreign [deflate], {} bytes", payload.len());
}

/// A program of two crates, `pair` (its `src/main.rs`) and `pairlib`: `blaze run` makes one
/// `Native` record, and its reads list the `Links` of both crates, beside the unit graph.
///
/// An image that read only its root's table (A6's sim control, `Bug::RootLinksOnly`) fails at
/// `pairlib`.
///
/// Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
/// blaze-cli test native_image_tests`:
/// `native_image: one Native record of pair reads the Links of pair and pairlib (<k> Links reads)`
/// and `test tests::native_image_tests::a_two_crate_programs_image_reads_both_links ... ok`.
#[test]
fn a_two_crate_programs_image_reads_both_links() {
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    two_crates(&io, "pair", "pairlib", "pub fn f() {}\n");
    let ran = ask(&io, (Command::Run, "run"), SMALL);
    let out = stdout(&io);
    let native = native_of("pair").unwrap_or_else(|| panic!("no `Native` record of `pair` was made ({:?}):\n{out}", ran.as_ref().err()));
    assert!(native_of("pair").is_none(), "a second `Native` record of `pair` was made in one run");
    assert!(native.reads.iter().any(|(name, _)| *name == blaze_drive::Name::unit_graph()), "the image does not read the unit graph: {:?}", native.reads);
    for package in ["pair", "pairlib"] {
        let (_, links) = native.packages.iter().find(|(p, _)| p == package).unwrap_or_else(|| panic!("the image did not link `{package}`: {:?}", native.packages));
        assert!(native.reads.iter().any(|(name, _)| name == links), "the image's reads do not list `{package}`'s `Links`: {:?}", native.reads);
    }
    let links = native.reads.iter().filter(|(name, _)| name.kind == Kind::Links).count();
    std::println!("native_image: one Native record of pair reads the Links of pair and pairlib ({links} Links reads)");
}
