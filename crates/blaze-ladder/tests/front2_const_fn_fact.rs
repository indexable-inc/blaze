//! A call in a constant is answered from front2's facts (front2/const_fn_fact.rs `fn_fact`):
//! "not a `const fn`" only for a callee whose head was read and has no `const`; a missing fact
//! is `FnKind::Missing(fact)`, which blaze-consteval refuses as the typed, pending
//! `Refusal::FnFact` (its own test: blaze-consteval tests/fn_fact.rs, through the evaluator).
//!
//! The Reference, "Constant evaluation", "Const functions" and the list of constant
//! expressions: "Calls of const functions and const methods" and tuple struct and tuple variant
//! constructors may be called in a constant context; any other call is an error. And
//! "Statements", "Item declarations": a static declared in a block is a static like a module's.
//!
//! The body-static programs are each a plain `fn main()` (D104 allows it) declaring ONE static
//! in its body, as protobuf's generated `default_instance` does (`static instance: M = M {
//! special_fields: crate::SpecialFields::new(), .. }`), built by the real command's front
//! (`build_stubbed`). The ctor and gap cases call `fn_fact` over a FRESH store, which holds no
//! ctor row and no signature row: the ctors are answered from their keys' facts alone.
//! Expected lines (stderr, and the file `FRONT2_WORK_LIST` names):
//!   "tuple ctor: ok"
//!   "other crate's const fn: ok"
//!   "not const: refused, naming three"
//!   "ctor rows absent: struct P and variant E::A answered as ctors"
//!   "gap: Missing(Origin): it has no library item, no member origin and no signature row"
//! Needs `$HOME/.blaze/v2/toolchain`. NEVER RUN by its writer.

use blaze_consteval::{Fact, FnKind};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::const_fn_fact::fn_fact;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::source::NamesSource;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_stubbed};
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::ItemKey;

fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

fn shelf() -> String {
    let home = std::env::var("HOME").expect("HOME is set");
    format!("{home}/.blaze/v2/toolchain")
}

/// The real command's front over one `src/main.rs`: `Ok(())` or the refusal, on one line.
fn build(text: &str) -> Result<(), String> {
    let shelf = shelf();
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [SourceFile { path: "src/main.rs", text }];
    let new = NewFront { package: "fnfact", edition: "blaze-2026.11", shelf: Some(&shelf), overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    build_stubbed(&names, &files, &texts).map(|_| ()).map_err(|e| format!("{e}").replace('\n', " "))
}

/// `P(3)` in a body static: a tuple struct's constructor is const.
#[test]
fn body_static_calls_a_tuple_ctor() {
    let got = build("struct P(u8);\n\nfn main() {\n    static L: P = P(3);\n    let _kept = &L;\n}\n");
    note(&format!("tuple ctor: {}", got.as_ref().map_or_else(|e| e.as_str(), |()| "ok")));
    assert_eq!(got, Ok(()), "a tuple struct's ctor is const");
}

/// `std::grant::Param::new(..)`: an inherent `pub const fn` of an impl in ANOTHER crate (bstd),
/// the shape of protobuf's `SpecialFields::new()` and alloc's `Vec::new()`.
#[test]
fn body_static_calls_another_crates_const_fn() {
    let got = build("fn main() {\n    static L: std::grant::Param = std::grant::Param::new(\"out\", std::grant::Kind::Out);\n    let _kept = &L;\n}\n");
    note(&format!("other crate's const fn: {}", got.as_ref().map_or_else(|e| e.as_str(), |()| "ok")));
    assert_eq!(got, Ok(()), "`Param::new` is a `const fn` of bstd");
}

/// Control, the old form: `three()`'s head was read and has no `const`: refused as not const,
/// naming it by path (item_shown.rs), never by a bare key.
#[test]
fn body_static_calls_a_fn_that_is_not_const() {
    let got = build("fn three() -> u8 {\n    3\n}\n\nfn main() {\n    static L: u8 = three();\n    let _kept = &L;\n}\n");
    let Err(e) = got else { panic!("a call of a fn that is not `const` in a static's initializer is an error") };
    assert!(e.contains("which is not a `const fn`"), "refused as not const: {e}");
    assert!(e.contains("three"), "the refusal names the callee by its path: {e}");
    note("not const: refused, naming three");
}

/// Over a fresh store (no ctor rows, no signature rows): a tuple struct's ctor and an enum's
/// tuple variant are ctors from their keys' facts; a key nothing files is the gap
/// `Missing(Origin)`, never `NotConst`. Without const_fn_fact.rs's no-row arms the struct's
/// ctor (a `Ctor` member with no row) and the variant (an item with no text) were `NotConst`.
#[test]
fn ctors_without_rows_and_a_missing_fact() {
    let shelf = shelf();
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let text = "struct P(u8);\n\nenum E {\n    A(u8),\n}\n\nfn main() {}\n";
    let files = [SourceFile { path: "src/main.rs", text }];
    let new = NewFront { package: "fnfact", edition: "blaze-2026.11", shelf: Some(&shelf), overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files).unwrap_or_else(|e| panic!("front2: {e}"));
    let store = Store::new(front.index.lang("sized"), front.frame);
    let heap = blaze_own::testing::heap();
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Types, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let (p, e) = (lib("P"), lib("E"));
    let alloc: Alloc<'_> = heap.alloc();

    // `P(..)`: the struct's `Ctor` member, as heads.rs files it, with no ctor row.
    let pk = front.keys.item(p).expect("P's key");
    let ctor = front.keys.member(pk, MemberKind::Ctor, "").expect("P's ctor key");
    assert!(store.ctors.get(&ctor).is_none(), "the fresh store holds no ctor row for P");
    assert_eq!(fn_fact(&names, &front.keys, &front.index, &store, ctor, alloc), FnKind::Ctor { adt: pk, variant: 0 });

    // `E::A(..)`: the variant's own key, with no ctor row.
    let a = NamesSource::variants(&names, e).and_then(|vs| vs.iter().find(|v| v.name == "A")).map(|v| v.key).expect("E::A in the names library");
    let ak = front.keys.item(a).expect("E::A's key");
    assert!(store.ctors.get(&ak).is_none(), "the fresh store holds no ctor row for E::A");
    assert_eq!(fn_fact(&names, &front.keys, &front.index, &store, ak, alloc), FnKind::Ctor { adt: checker_of(e), variant: 0 });
    note("ctor rows absent: struct P and variant E::A answered as ctors");

    // A key in no crate's prefix and in no row: nothing files it.
    let nobody = ItemKey(0xffff_ffff_ffff_ffff_0000_0000_0000_0001);
    let got = fn_fact(&names, &front.keys, &front.index, &store, nobody, alloc);
    assert_eq!(got, FnKind::Missing(Fact::Origin), "a key nothing files is the gap Origin, never `NotConst`");
    note(&format!("gap: {got:?}: {}", Fact::Origin));
}
