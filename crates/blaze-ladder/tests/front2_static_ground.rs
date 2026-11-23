//! A static whose fields' types are grounded through a projection (front2/const_project.rs,
//! consts.rs `LayoutFacts::normalize` of `ConstsWorld`), evaluated by front2's constant
//! evaluator with the real core. The case met on the hello world's walk: alloc's
//! `static STATIC_INNER_SLICE: SliceArcInnerForStatic` (library/alloc/src/rcs/arc.rs:
//! `#[repr(C, align(16))] struct SliceArcInnerForStatic { inner: ArcInner<[u8; 1]> }`, whose
//! `strong`/`weak` are `Atomic<usize>`, and core's
//! `pub struct Atomic<T: AtomicPrimitive> { v: UnsafeCell<T::Storage> }`,
//! library/core/src/sync/atomic.rs) was "left out: .. a field type with no ground form is not
//! evaluated yet" (port27/front2/list-AI.txt, 24 functions).
//!
//! The Reference, "Static items": "The static initializer is a constant expression evaluated
//! at compile time"; "Type layout", "The `C` Representation": "each field ... is placed at the
//! next offset that is a multiple of its alignment ... the size of the struct is the current
//! offset rounded up to the nearest multiple of the struct's alignment"; "The alignment
//! modifiers": `align(16)` raises the struct's alignment to 16. "Associated Items",
//! "Associated types": `<u32 as Prim>::Storage` is the impl's `type Storage = Al8<u32>`.
//!
//! Lines it prints (and asserts):
//!   `HELD: 32 bytes, align 16: strong 20 at 0, weak 15 at 8, data 7 at 16`
//!   `ARC_LIKE: 32 bytes, align 16: strong 1 at 0, weak 1 at 8, data 0 at 16`
//!   `BAD: refused: <the checker's diagnostic, or the named projection>`
//!
//! A wrong fix fails here: answering the projection with anything but `Al8<u32>` moves `weak`
//! (to 4 with `u32`'s alignment) or refuses again; the old refusal's text ("a field type with
//! no ground form") for `BAD` fails the third check, which wants the refusal NAMED.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_consts.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records};
use blaze_ladder::front2::consts::{ConstFail, Consts};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

pub trait Prim {
    type Storage;
}

#[repr(C, align(8))]
pub struct Al8<T>(T);

impl Prim for u32 {
    type Storage = Al8<u32>;
}

#[repr(C)]
pub struct Atom<T: Prim> {
    v: T::Storage,
}

impl Atom<u32> {
    pub const fn new(v: u32) -> Atom<u32> {
        Atom { v: Al8(v) }
    }
}

#[repr(C)]
pub struct Inner<T> {
    strong: Atom<u32>,
    weak: Atom<u32>,
    data: T,
}

#[repr(C, align(16))]
pub struct Holder {
    inner: Inner<[u8; 1]>,
}

static HELD: Holder = Holder { inner: Inner { strong: Atom::new(20), weak: Atom { v: Al8(15) }, data: [7] } };

#[repr(C)]
pub struct ArcLike<T> {
    strong: core::sync::atomic::AtomicUsize,
    weak: core::sync::atomic::AtomicUsize,
    data: T,
}

#[repr(C, align(16))]
pub struct ForStatic {
    inner: ArcLike<[u8; 1]>,
}

static ARC_LIKE: ForStatic = ForStatic {
    inner: ArcLike { strong: core::sync::atomic::AtomicUsize::new(1), weak: core::sync::atomic::AtomicUsize::new(1), data: [0] },
};

pub trait Tr {
    type S;
}

impl Tr for u16 {
    type S = u8;
}

pub struct W<T: Tr> {
    v: T::S,
}

static BAD: W<u8> = W { v: 3 };
";

/// A line of the outcome, as it happens: appended to the file `FRONT2_WORK_LIST` names, since a
/// passing (or hung) test's output is not shown by the lane's check script.
fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// The little-endian integer of `n` bytes at `at`.
fn le(bytes: &[u8], at: usize, n: usize) -> u64 {
    let mut v = 0u64;
    for (k, &b) in bytes[at..at + n].iter().enumerate() {
        v |= u64::from(b) << (8 * k);
    }
    v
}

#[test]
fn static_fields_ground_through_projections() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/statics/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "statics", dir: "/statics/", root: "/statics/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let (held, arc_like, bad) = (lib("HELD"), lib("ARC_LIKE"), lib("BAD"));
    let held = front.keys.item(held).expect("HELD's key");
    let arc_like = front.keys.item(arc_like).expect("ARC_LIKE's key");
    let bad = front.keys.item(bad).expect("BAD's key");
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();

    // `Atom<u32>`'s one field is `<u32 as Prim>::Storage` = `Al8<u32>`: size 8, align 8. So
    // `Inner<[u8; 1]>` (repr C) is strong at 0, weak at 8, data at 16, size 24; `Holder`'s
    // `align(16)` makes it 32 bytes at alignment 16 (log2 4).
    let rec = consts.value(&mut records, prepared, held).unwrap_or_else(|e| panic!("HELD: {e}"));
    let root = rec.data.allocs.first().expect("HELD's record has its root allocation");
    let b = &root.bytes[..];
    let line = format!("HELD: {} bytes, align {}: strong {} at 0, weak {} at 8, data {} at 16", b.len(), 1u32 << root.align, le(b, 0, 4), le(b, 8, 4), b[16]);
    note(&line);
    assert_eq!(line, "HELD: 32 bytes, align 16: strong 20 at 0, weak 15 at 8, data 7 at 16");
    assert!(root.relocs.is_empty(), "HELD holds no pointer");

    // The library's shape: `AtomicUsize` = `Atomic<usize>`, its field
    // `UnsafeCell<<usize as AtomicPrimitive>::Storage>` (`Align8<usize>` on a 64-bit target),
    // made by `AtomicUsize::new`'s `transmute`.
    let rec = consts.value(&mut records, prepared, arc_like).unwrap_or_else(|e| panic!("ARC_LIKE: {e}"));
    let root = rec.data.allocs.first().expect("ARC_LIKE's record has its root allocation");
    let b = &root.bytes[..];
    let line = format!("ARC_LIKE: {} bytes, align {}: strong {} at 0, weak {} at 8, data {} at 16", b.len(), 1u32 << root.align, le(b, 0, 8), le(b, 8, 8), b[16]);
    note(&line);
    assert_eq!(line, "ARC_LIKE: 32 bytes, align 16: strong 1 at 0, weak 1 at 8, data 0 at 16");

    // `W<u8>`'s field is `<u8 as Tr>::S`, and no impl of `Tr` holds for `u8`: that type has
    // no ground form (rustc refuses the program, E0277). Refused by name: the checker's E0277
    // on the static's body, or the evaluator's projection refusal naming `Tr`'s `S` at `u8`;
    // never the old unnamed "a field type with no ground form".
    match consts.value(&mut records, prepared, bad) {
        Ok(_) => panic!("BAD has no value: `u8: Tr` does not hold"),
        Err(e @ ConstFail::Check { error: CheckFail::Diags { .. }, .. }) => {
            let ConstFail::Check { error: CheckFail::Diags { diags, .. }, .. } = &e else { unreachable!() };
            assert!(diags.iter().any(|(code, _, _)| code.code() == "E0277"), "BAD: the checker's diagnostics hold no E0277: {e}");
            note(&format!("BAD: refused: {e}"));
        }
        Err(e @ (ConstFail::Diag { .. } | ConstFail::Projection { .. })) => note(&format!("BAD: refused: {e}")),
        Err(other) => panic!("BAD: refused, but not by name: {other}"),
    }
}
