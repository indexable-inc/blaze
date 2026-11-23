//! An E0425 after a written `std` says what `std` named when it is bstd (piece std-is-bstd-note;
//! wiring /Volumes/Projects/blaze-int-1112-notes/wiring/blaze-ladder/std-is-bstd-note-1.md; rule 19).
//!
//! Under a blaze edition `std` is bstd and Rust's `std` has no name (blaze-unit-graph
//! implicit.rs: "blaze edition (lib, bin, test) | yes | by `extern crate alloc` | bstd; Rust's
//! `std` none"; "There is no opt-in to Rust's `std` under the edition"). bstd's `sync` has no
//! `OnceLock` (crates/blaze-std/src/sync.rs: its `Once` is Rust std's `OnceLock` as a value), so
//! `std::sync::OnceLock::<u8>::new()` is E0425 at segment 2, and the finding must say that `std`
//! here is bstd, under edition blaze-2026.11. Under edition 2024 `std` is Rust's: the same path
//! resolves (the pinned library's std/src/sync/mod.rs: `pub use self::once_lock::OnceLock;`),
//! and a path missing there too (`std::sync::NOT_THERE`) is E0425 with no note.
//!
//! What it prints when it passes: `test std_is_bstd_note ... ok`, and in the notes one line per
//! (edition, body):
//! `blaze-2026.11 once_lock: diagnostics [.. "`OnceLock` (segment 2) not found on the module ..
//! (note: `std` here names bstd, under edition blaze-2026.11; that edition gives Rust's `std` no
//! name)" ..]`, the same for `not_there` (`NOT_THERE`); `2024 once_lock: <anything without the
//! note>`; `2024 not_there: diagnostics [.. UnresolvedPath .. "`NOT_THERE` (segment 2) not found
//! on the module .." ..]` with no note.
//! Control: without the wiring (world_path_miss.rs answering `PathPrefix::Module` for every
//! module), `blaze-2026.11 once_lock` says "`OnceLock` (segment 2) not found on the module .."
//! with no note, and the test fails with "blaze-2026.11 once_lock: no note naming bstd".
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_e0425_values.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main() {}
fn once_lock() -> u8 {
    let _cell = std::sync::OnceLock::<u8>::new();
    0
}
fn not_there() -> u8 { std::sync::NOT_THERE }
";

/// The note's words (front2/path_miss_shown.rs `prefix_shown`, `PathPrefix::Bstd`).
const NOTE: &str = "`std` here names bstd, under edition blaze-2026.11";

/// What each body of `MAIN` said when checked as a crate of `edition`: (body, said).
fn said_under(edition: &str) -> Vec<(&'static str, String)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/note/src/main.rs".to_string(), MAIN.to_string())];
    let package = edition::PackageEdition::of_manifest(edition).unwrap_or_else(|e| panic!("edition {edition}: {e:?}"));
    let user = UserCrate { name: "note", dir: "/note/", root: "/note/src/main.rs", files: &files, edition: package, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut out = Vec::new();
    let mut bodies = Vec::new();
    for name in ["once_lock", "not_there"] {
        let path: &[&str] = &[name];
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: path }, alloc)
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("{edition}: `{name}` not found from the crate's root"));
        store.ensure(&names, &mut front.keys, Want::Fn(key)).unwrap_or_else(|e| panic!("{edition} {name}: no head: {e:?}"));
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name, b)),
            Err(e) => out.push((name, format!("does not lower: {e}"))),
        }
    }
    for (name, body) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        out.push((*name, said));
    }
    out
}

#[test]
fn std_is_bstd_note() {
    let mut notes: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (edition, body, said) in said_under("blaze-2026.11").into_iter().map(|(b, s)| ("blaze-2026.11", b, s)).chain(said_under("2024").into_iter().map(|(b, s)| ("2024", b, s))) {
        notes.push(format!("{edition} {body}: {said}"));
        let unresolved = said.contains("UnresolvedPath");
        let noted = said.contains(NOTE);
        match (edition, body) {
            // Under the blaze edition both paths miss after `std`, and both say what it named.
            ("blaze-2026.11", _) if !(unresolved && noted) => failed.push(format!("{edition} {body}: no note naming bstd")),
            // Rust's `std`: never the note; a path Rust's std lacks is still E0425.
            ("2024", _) if noted => failed.push(format!("{edition} {body}: a note under Rust's std")),
            ("2024", "not_there") if !unresolved => failed.push(format!("{edition} {body}: not E0425")),
            _ => {}
        }
    }
    println!("{}", notes.join("\n"));
    assert!(failed.is_empty(), "{failed:?}\n{}", notes.join("\n"));
}
