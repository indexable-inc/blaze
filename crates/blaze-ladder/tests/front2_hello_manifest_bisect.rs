//! Class Q, step two: WHICH question of `static MANIFEST`'s body asks for the bstd key that has
//! no row. A1's run of tests/front2_hello_manifest.rs (19:1x) showed `SELECTED` (`<Out as
//! Grant>::KIND`) and `PARAM_SELECTED` (`Param::new("out", <Out as Grant>::KIND)`) evaluate, and
//! only `MANIFEST` (`Manifest::new(&[Param::new("out", <Out as Grant>::KIND)])`) is refused:
//! "checking <MANIFEST>: 1 gaps   the checker key 0x6986ddff.. is in no row of the front end's
//! key map" (bstd's crate prefix in that build). So the gap is in `Manifest::new`, the borrow of
//! the array, or its coercion to `&'static [Param]`, not in the selection.
//!
//! The rules (each static below has a value): the Reference, "Static items" ("The static
//! initializer is a constant expression"); "Constant evaluation", "Constant expressions": "Array
//! expressions", "Borrow expressions" and "Calls of const functions"; "Type coercions",
//! "Unsized coercions": `&[T; n]` to `&[T]`; "Struct expressions".
//!
//! Lines (eprintln, and appended to the file `FRONT2_WORK_LIST` names), whole, on one line each:
//! - `bisect_statics`: `<NAME>: ok, <n> allocs` or `<NAME>: refused: <the whole refusal>` for
//!   SLICE, ARRAY, EMPTY, LITERAL, PLAIN, MANIFEST (each in a FRESH front, so no static's fills
//!   help another), then `BISECT: refused [..]`.
//! - `manifest_body_questions`: MANIFEST's body checked alone (as consts.rs `check_as` does),
//!   then `QUESTIONS: [..]`: the checker's questions of the round that ended in the gap
//!   (front2 reads.rs `Asked`): the one naming the key with no row says what asked for it
//!   (`FnSig`, `Predicates`, `Adt`, `ImplHeader`, `AssocItem`, ..); `KEY PROBE: ..` says whether
//!   the key is one of `Manifest`, `Param`, `Kind`, `Grant`, `Out` by path.
//! Expected once Class Q is fixed: every static `ok`, `BISECT: refused []`, and `MANIFEST body:
//! clean`. TODAY: at least PLAIN or MANIFEST refused; the lines are the diagnosis. Needs
//! `$HOME/.blaze/v2/toolchain`. NOT RUN by the writer (a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::body_reads::check_body_asked;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::reads::Asked;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "use std::grant::{Grant, Kind, Manifest, Param};
use std::out::Out;

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    Ok(())
}

static SLICE: &[Param] = &[Param::new(\"out\", Kind::Out)];
static ARRAY: [Param; 1] = [Param::new(\"out\", Kind::Out)];
static EMPTY: Manifest = Manifest::new(&[]);
static LITERAL: Manifest = Manifest { params: &[], defaults: &[] };
static PLAIN: Manifest = Manifest::new(&[Param::new(\"out\", Kind::Out)]);
static MANIFEST: Manifest = Manifest::new(&[Param::new(\"out\", <Out as Grant>::KIND)]);
";

const ORDER: [&str; 6] = ["SLICE", "ARRAY", "EMPTY", "LITERAL", "PLAIN", "MANIFEST"];

fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// The library key of `segments` read from the crate root in the value namespace (a static),
/// or in the type namespace (an ADT or a trait).
fn path_key(names: &blaze_scope::resolve::Names<'_>, ns: Namespace, segments: &[&str]) -> Option<blaze_id::ItemKey> {
    names::path(names, names.root(0), ns, Written { kind: PathKind::Plain, segments }, Alloc::global())
        .ok()
        .and_then(|f| f.whole(segments.len()))
        .map(|d| d.key)
}

/// Each static of [`MAIN`] evaluated in a front of its own.
#[test]
fn bisect_statics() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/bisect/src/main.rs".to_string(), MAIN.to_string())];
    let mut refused: Vec<&str> = Vec::new();
    for name in ORDER {
        let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
        let user = UserCrate { name: "bisect", dir: "/bisect/", root: "/bisect/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
        let mut buckets = None;
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let lib = path_key(&names, Namespace::Values, &[name]).unwrap_or_else(|| panic!("`{name}` from the crate root"));
        let k = front.keys.item(lib).unwrap_or_else(|e| panic!("{name}'s key: {e:?}"));
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let mut consts = Consts::default();
        match consts.value(&mut records, prepared, k) {
            Ok(rec) => note(&format!("{name}: ok, {} allocs", rec.data.allocs.len())),
            Err(e) => {
                note(&format!("{name}: refused: {}", format!("{e}").replace('\n', " ")));
                refused.push(name);
            }
        }
    }
    note(&format!("BISECT: refused {refused:?}"));
    assert!(refused.is_empty(), "every static has a value (the Reference, \"Static items\"); refused: {refused:?}");
}

/// MANIFEST's body checked alone, as the evaluator checks it (consts.rs `check_as`), with the
/// questions of the round that ended in the gap.
#[test]
fn manifest_body_questions() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/bisect/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "bisect", dir: "/bisect/", root: "/bisect/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let lib = path_key(&names, Namespace::Values, &["MANIFEST"]).expect("`MANIFEST` from the crate root");
    // The keys a gap's key is compared with (KEY PROBE).
    let probes: Vec<(&str, Option<blaze_typeck::ItemKey>)> = [
        ("Manifest", &["std", "grant", "Manifest"][..]),
        ("Param", &["std", "grant", "Param"][..]),
        ("Kind", &["std", "grant", "Kind"][..]),
        ("Grant", &["std", "grant", "Grant"][..]),
        ("Out", &["std", "out", "Out"][..]),
    ]
    .into_iter()
    .map(|(n, segs)| (n, path_key(&names, Namespace::Types, segs).map(checker_of)))
    .collect();
    let _ = front.keys.item(lib).expect("MANIFEST's key");
    store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("MANIFEST's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, lib).unwrap_or_else(|e| panic!("MANIFEST lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("MANIFEST's spelling");
    let scope = body.scope(&spell);
    let mut asked = Asked::new_in(blaze_own::testing::heap().alloc());
    match check_body_asked(&mut records, &body, &scope, None, Some(&mut asked)) {
        Ok(_) => note("MANIFEST body: clean"),
        Err(e) => {
            let said = format!("{e}").replace('\n', " ");
            note(&format!("MANIFEST body: refused: {said}"));
            note(&format!("QUESTIONS: {:?}", asked.questions.as_slice()));
            let hit: Vec<String> = probes.iter().filter_map(|(n, k)| k.filter(|k| said.contains(&format!("{:#034x}", k.0))).map(|_| n.to_string())).collect();
            let shown: Vec<String> = probes.iter().map(|(n, k)| format!("{n} {}", k.map_or(String::from("(no path)"), |k| format!("{:#034x}", k.0)))).collect();
            note(&format!("KEY PROBE: the gap's key is {hit:?} among [{}]", shown.join(", ")));
            panic!("MANIFEST's body checks clean (the Reference, \"Static items\"): {said}");
        }
    }
}
