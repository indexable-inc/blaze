//! Class Q (hello runner 8, 2026-11-16 18:40): bstd's `main!` (blaze-std entry.rs) writes, inside the
//! `fn main` it makes, `static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(&[
//! $crate::grant::Param::new(stringify!(out), <Out as $crate::grant::Grant>::KIND) ]);`. The run
//! stopped: "static: constant <MANIFEST>: a body it needs: checking <MANIFEST>: 1 gaps / checker key
//! ItemKey(2263..)" (a bstd key with no row in front2's key map; port27/hello-1116/run-8s.err.txt).
//!
//! The rules. The Reference, "Static items": "The static initializer is a constant expression
//! evaluated at compile time"; "Constant evaluation", "Constant expressions": "Paths to functions
//! and constants", "Calls of const functions", "Array expressions", "Borrow expressions" (a shared
//! borrow of a value with no interior mutability) are constant expressions. "Associated Items",
//! "Associated Constants": "An associated constant definition defines a constant associated with a
//! type", so `<Out as Grant>::KIND` is the definition in the impl of `Grant` that holds for `Out`:
//! bstd grant.rs `impl Grant for Out { const KIND: Kind = Kind::Out; .. }`. "Statements", "Item
//! declarations": an item in a block is an item like a module's, with no access to the function's
//! locals. "Paths", "`$crate`": a path an expansion writes with `$crate` is in the macro's crate.
//! So every static below has a value, in the program's crate, naming bstd's items across the
//! crate boundary (the library here is the installed toolchain's bstd, the hello's own library).
//!
//! What a right answer is, layout-free: `SELECTED`'s bytes are `OUT_KIND`'s (`Kind::Out`) and not
//! `CLOCK_KIND`'s (`Kind::Clock`, `impl Grant for Clock`'s constant); `PARAM_SELECTED`'s bytes are
//! `PARAM`'s; `MANIFEST` is a `Manifest { params, defaults }` (bstd grant.rs), two slice
//! references: a 32-byte root of four words, in either field order (the Reference, "Type
//! layout": the Rust representation does not fix it). The `params` half holds a relocation to
//! an array of one `Param` and the length 1; the array's bytes are `PARAM`'s, and its `name`
//! points at the bytes `out`. The `defaults` half is the empty slice `&[]`, whose length is 0.
//!
//! Lines (eprintln, and appended to the file `FRONT2_WORK_LIST` names): one per static,
//! `<NAME>: <n> allocs, root <b> bytes, relocs [..]` or `<NAME>: refused: <the whole refusal>`;
//! then `MANIFEST STATICS: ok` when every check holds. `HELLO D104: past the manifest: <outcome>`
//! for the second test, which runs the real command's front (`run::build_stubbed`) over the
//! hello's file as run 8s had it.
//!
//! Control (the cause in): today's tree fails both, the first at `SELECTED` / `PARAM_SELECTED` /
//! `MANIFEST` refused (or wherever the gap is: the lines name it), the second with "is not
//! checked: static `static MANIFEST: ..`". Needs `$HOME/.blaze/v2/toolchain`. NOT RUN by the
//! writer (a write-only lane).

#![feature(allocator_api)]

use blaze_build::data::DataTarget;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_stubbed};
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "use std::grant::{Grant, Kind, Manifest, Param};
use std::out::Out;

fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    Ok(())
}

static OUT_KIND: Kind = Kind::Out;
static CLOCK_KIND: Kind = Kind::Clock;
static SELECTED: Kind = <Out as Grant>::KIND;
static PARAM: Param = Param::new(\"out\", Kind::Out);
static PARAM_SELECTED: Param = Param::new(\"out\", <Out as Grant>::KIND);
static MANIFEST: Manifest = Manifest::new(&[Param::new(\"out\", <Out as Grant>::KIND)]);
";

/// The hello's file exactly as run 8s had it (port27/hello-1116/prog8s/src/main.rs).
const HELLO: &str = "#[std::main]
fn main(cx: &Cx, out: std::out::Out) -> Result<(), Error> {
    out.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// One allocation of a record, copied out: its bytes and its relocations (offset, target).
struct Alloc1 {
    bytes: Vec<u8>,
    relocs: Vec<(u32, DataTarget)>,
}

fn le8(bytes: &[u8], at: usize) -> u64 {
    let mut v = 0u64;
    for (k, &b) in bytes[at..at + 8].iter().enumerate() {
        v |= u64::from(b) << (8 * k);
    }
    v
}

/// Every static of [`MAIN`] has its value, and the values are the ones the rules give.
#[test]
fn manifest_statics_evaluate() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/manifest/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "manifest", dir: "/manifest/", root: "/manifest/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |name: &str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    const ORDER: [&str; 6] = ["OUT_KIND", "CLOCK_KIND", "SELECTED", "PARAM", "PARAM_SELECTED", "MANIFEST"];
    let keys = ORDER.map(|n| front.keys.item(lib(n)).unwrap_or_else(|e| panic!("{n}'s key: {e:?}")));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();

    // Each static evaluated in order; a refusal is kept whole (one line) and every static is
    // still tried, so one run names every static the cause touches.
    let mut got: Vec<Result<Vec<Alloc1>, String>> = Vec::new();
    for (name, k) in ORDER.iter().zip(keys) {
        let made = match consts.value(&mut records, prepared, k) {
            Ok(rec) => Ok(rec
                .data
                .allocs
                .iter()
                .map(|a| Alloc1 { bytes: a.bytes.iter().copied().collect(), relocs: a.relocs.iter().map(|r| (r.offset, r.target)).collect() })
                .collect::<Vec<_>>()),
            Err(e) => Err(format!("{e}").replace('\n', " ")),
        };
        match &made {
            Ok(allocs) => {
                let relocs: Vec<String> = allocs[0].relocs.iter().map(|(o, t)| format!("{o}->{t:?}")).collect();
                note(&format!("{name}: {} allocs, root {} bytes, relocs [{}]", allocs.len(), allocs[0].bytes.len(), relocs.join(", ")));
            }
            Err(why) => note(&format!("{name}: refused: {why}")),
        }
        got.push(made);
    }
    let refused: Vec<String> = ORDER.iter().zip(&got).filter_map(|(n, g)| g.as_ref().err().map(|w| format!("{n}: {w}"))).collect();
    assert!(refused.is_empty(), "every static of MAIN has a value (the Reference, \"Static items\"); refused: {refused:#?}");
    let all: Vec<Vec<Alloc1>> = got.into_iter().map(|g| g.unwrap_or_default()).collect();
    let [out_kind, clock_kind, selected, param, param_selected, manifest] = &all[..] else { unreachable!() };

    // `<Out as Grant>::KIND` is the impl for `Out`'s `Kind::Out`, not another impl's constant.
    assert_eq!(selected[0].bytes, out_kind[0].bytes, "SELECTED is `Kind::Out` (impl Grant for Out)");
    assert_ne!(out_kind[0].bytes, clock_kind[0].bytes, "`Kind::Out` and `Kind::Clock` differ, so the check above can fail");
    // `Param::new("out", <Out as Grant>::KIND)` is `Param::new("out", Kind::Out)`.
    assert_eq!(param_selected[0].bytes, param[0].bytes, "PARAM_SELECTED is PARAM");
    let offsets = |a: &Alloc1| a.relocs.iter().map(|(o, _)| *o).collect::<Vec<u32>>();
    assert_eq!(offsets(&param_selected[0]), offsets(&param[0]), "PARAM_SELECTED's name pointer is where PARAM's is");

    // `Manifest { params: &[Param; 1] as &'static [Param], defaults: &[] }`: two slice
    // references of 16 bytes each, in an order the Rust representation does not fix.
    let root = &manifest[0];
    assert_eq!(root.bytes.len(), 32, "MANIFEST is a `Manifest`: two `&'static [_]` fields (params, defaults), four words");
    // The `params` half: the relocation whose target is the one-`Param` array (PARAM's bytes).
    let params_reloc = root.relocs.iter().copied().find(|(_, t)| match t {
        DataTarget::Local(n) => manifest.get(*n as usize).is_some_and(|a| a.bytes == param[0].bytes),
        _ => false,
    });
    let (at, target) = params_reloc.unwrap_or_else(|| panic!("MANIFEST's root points at an array holding PARAM's bytes; relocs at {:?}", offsets(root)));
    let half = at - at % 16;
    let len_at = half + if at % 16 == 0 { 8 } else { 0 };
    assert_eq!(le8(&root.bytes, len_at as usize), 1, "MANIFEST's `params` slice holds one `Param`");
    // The `defaults` half: `&[]`, length 0 (whichever of its two words is the length).
    let other = if half == 0 { 16usize } else { 0 };
    assert!(
        le8(&root.bytes, other) == 0 || le8(&root.bytes, other + 8) == 0,
        "MANIFEST's `defaults` is the empty slice: one of its words, the length, is 0: {:?}",
        &root.bytes[other..other + 16]
    );
    let DataTarget::Local(array) = target else { panic!("MANIFEST's array is an allocation of its own record, not {target:?}") };
    let array = &manifest[array as usize];
    assert_eq!(array.bytes, param[0].bytes, "MANIFEST's one `Param` is PARAM");
    let name = array.relocs.iter().find_map(|(_, t)| match t {
        DataTarget::Local(n) => Some(&manifest[*n as usize]),
        _ => None,
    });
    let name = name.unwrap_or_else(|| panic!("the `Param`'s name points into MANIFEST's record"));
    assert_eq!(&name.bytes[..], b"out", "`stringify!(out)` is \"out\"");
    note("MANIFEST STATICS: ok");
}

/// The real command's front (`blaze run`'s `build_stubbed`) over the hello's file: it may stop
/// at a LATER stop of the path (the entry, a closure, `guest::grant`), but never at the
/// expansion's `static MANIFEST`.
#[test]
fn hello_d104_gets_past_the_manifest() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [SourceFile { path: "src/main.rs", text: HELLO }];
    let new = NewFront { package: "hello", edition: "blaze-2026.11", shelf: Some(&shelf), overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let outcome = match build_stubbed(&names, &files, &texts, blaze_own::Alloc::global()) {
        Ok(_) => String::from("built"),
        Err(e) => format!("stopped: {e}").replace('\n', " "),
    };
    note(&format!("HELLO D104: past the manifest: {outcome}"));
    assert!(!outcome.contains("is not checked: static"), "the expansion's `static MANIFEST` is checked and evaluated: {outcome}");
}
