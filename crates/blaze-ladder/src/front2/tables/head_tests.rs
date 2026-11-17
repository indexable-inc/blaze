//! Engine piece E2a: a frozen head is a pure function of its item (front2/tables/head.rs).
//!
//! One no_core crate (`app`, over a `core` of the sized family) holds a trait with an
//! associated type and const and a provided fn, a generic struct with a where-clause impl of it,
//! a tuple struct, an enum with constructors, an inherent impl, a where-clause fn and an
//! `impl Trait` return. Every cell of `app` (head.rs `cell_of`) is frozen:
//! - ALONE: each in a fresh store (`freeze`);
//! - FORWARD: in one store, each exported right after it is made, in key order;
//! - REVERSED: in one store, all made in reverse order, then each exported;
//!
//! and the bytes must be equal (`frozen_bytes_do_not_depend_on_what_was_made_before`). Then a
//! frozen head read back equals the store's head (`a_frozen_head_imports_as_today_s_head`), the
//! planted bug (a job-local `PTy` index exported) is caught by the same comparison
//! (`a_job_local_id_is_caught`), and the two `HeadSource`s answer alike.

use blaze_id::{ContentId, CrateItemKey, ItemKey as LibKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, ItemKind};
use blaze_typeck::PortableTys;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::head::{FreezeError, FrozenHead, FrozenHeads, FrozenHeadsOpts, HeadSource, Plant, StoreHeads, StoreHeadsOpts, export, freeze, import, reads_seen, want_bytes};
use crate::front2::by_origin;
use crate::front2::head_record::{self, HeadAt};
use crate::front2::heads::{Store, Want};
use crate::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use crate::front2::project::head_digest::want_digest_in;
use crate::front2::source::{Load, Log, NamesSource, Records};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::Sized;
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
";

const APP: &str = "#![no_std]
pub trait Area {
    type Unit;
    const SIDES: u32;
    fn area(&self) -> u32;
    fn twice(&self) -> u32 {
        0
    }
}
pub struct Rect<T> {
    pub w: T,
    pub h: T,
}
pub struct Square(pub u32);
pub enum Shape {
    Sq(Square),
    Pair(u32, u32),
    Empty,
}
impl<T> Area for Rect<T>
where
    T: Area,
{
    type Unit = T;
    const SIDES: u32 = 4;
    fn area(&self) -> u32 {
        0
    }
}
impl Area for Square {
    type Unit = u32;
    const SIDES: u32 = 4;
    fn area(&self) -> u32 {
        0
    }
}
impl<T: Area> Rect<T> {
    pub const CORNERS: u32 = 4;
    pub fn new(w: T, h: T) -> Rect<T> {
        Rect { w, h }
    }
}
pub fn total<T>(x: &T) -> u32
where
    T: Area<Unit = u32>,
{
    0
}
pub fn shaped(s: Square) -> impl Area {
    s
}
pub fn pick(s: Shape, r: &Rect<Square>) -> Shape {
    s
}
pub trait Walk {
    type Iter<'a>
    where
        Self: 'a;
    type Buf<const N: usize>;
}
pub enum Dir {
    Up,
    Down,
}
pub struct Grid<const N: usize>;
impl<const N: usize> Grid<N> {
    pub fn size(&self) -> u32 {
        0
    }
}
pub fn longer<'a, 'b: 'a>(x: &'a u32, y: &'b u32) -> &'a u32 {
    x
}
";

/// World indexes: 0 `app` (the owner), 1 `core`.
const APP_AT: usize = 0;
const CORE_AT: usize = 1;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder frozen heads crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

/// The names source a body job reads: each crate's records.
type Src = &'static Records<'static>;

/// Each crate's records as `blaze check`'s body job gives them, kept for the test run.
struct Kept {
    names: AVec<'static, &'static [u8]>,
    items: AVec<'static, &'static [u8]>,
    /// Per crate: (package path, text).
    files: AVec<'static, AVec<'static, (&'static str, &'static str)>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.iter().find(|(p, _)| *p == path.as_str()).map(|(_, t)| *t)
    }

    fn expansion(&self, _krate: usize, _call: blaze_id::ItemKey) -> Option<&'static str> {
        None
    }

    fn expansion_origins(&self, _krate: usize, _call: blaze_id::ItemKey) -> Option<&'static [u8]> {
        None
    }
}

/// The fixture: its records, `Sized`, and a frame for the stores (all kept for the test run).
/// Over records, as the engine's head tasks read: a key is found from the records when first
/// met, so a fresh key map is a whole one (the solved `Names` source needs every key filed
/// first, `Front2::for_program`).
fn world() -> (Src, Option<LibKey>, &'static blaze_mem::Frame) {
    let crates = [input("app", "/app/lib.rs", &[("core", CORE_AT)]), input("core", "/core/lib.rs", &[])];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let solved = resolve_crates(&crates, sysroot, &Texts, &Serial, a()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let solved: &'static Names<'static> = blaze_own::Box::leak(blaze_own::Box::new_in(solved, a()));
    let g = a();
    let mut kept = Kept { names: AVec::new_in(g), items: AVec::new_in(g), files: AVec::new_in(g) };
    let mut crate_names: AVec<'static, &'static str> = AVec::new_in(g);
    let mut crate_keys: AVec<'static, CrateItemKey> = AVec::new_in(g);
    for c in solved.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), g));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), g));
        let mut files = AVec::new_in(g);
        files.extend(c.files.iter().map(|f| (f.package.as_str(), f.text)));
        kept.files.push(files);
        crate_names.push(c.name);
        crate_keys.push(c.key);
    }
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame");
    let frame: &'static blaze_mem::Frame = blaze_own::Box::leak(blaze_own::Box::new_in(frame, g));
    let log: &'static Log<'static> = blaze_own::Box::leak(blaze_own::Box::new_in(Log::new(crate_names.leak(), g), g));
    let load: &'static Kept = blaze_own::Box::leak(blaze_own::Box::new_in(kept, g));
    let names: Src = blaze_own::Box::leak(blaze_own::Box::new_in(Records::new(crate_keys.leak(), load, log, frame, g), g));
    let mut sized = None;
    NamesSource::each_lang(names, CORE_AT, &mut |name, key| {
        if name == "sized" {
            sized = Some(key);
        }
    });
    assert!(sized.is_some(), "core's `Sized`");
    (names, sized, frame)
}

/// Every cell of `app`: each item's whole want, and each impl fn's own; sorted by key.
fn cells(names: Src) -> AVec<'static, Want> {
    let mut out: AVec<'static, Want> = AVec::new_in(a());
    NamesSource::each_item(names, APP_AT, &mut |key, _| {
        let Ok(kind) = by_origin::kind_of_lib(names, key) else { return };
        let Ok(want) = by_origin::item_want(names, kind) else { return };
        out.push(want);
    });
    let impls: AVec<'static, LibKey> = {
        let mut v = AVec::new_in(a());
        v.extend(out.iter().filter_map(|w| if let Want::Impl(k) = w { Some(*k) } else { None }));
        v
    };
    for lib in impls.iter().copied() {
        NamesSource::each_member(names, lib, &mut |m| {
            if m.kind == ItemKind::Function && m.written_by.is_none() {
                if let Some(name) = m.name {
                    out.push(Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, name)));
                }
            }
        });
    }
    out.sort_unstable_by_key(|w| want_bytes(*w));
    out.dedup();
    out
}

/// Each cell's bytes made in ONE store: in `order`; each exported right after it is made
/// (`each_at_once`), or all made first.
fn in_one_store(names: Src, sized: Option<LibKey>, frame: &'static blaze_mem::Frame, order: &[Want], each_at_once: bool, plant: Plant) -> AVec<'static, (Want, AVec<'static, u8>)> {
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    let mut out: AVec<'static, (Want, AVec<'static, u8>)> = AVec::new_in(a());
    let put = |store: &Store<'static>, keys: &Keys, w: Want, out: &mut AVec<'static, (Want, AVec<'static, u8>)>| {
        let bytes = export(names, keys, store, w, plant, a()).unwrap_or_else(|e| panic!("export {w:?}: {e}"));
        let mut kept = AVec::new_in(a());
        kept.extend_from_slice(&bytes);
        out.push((w, kept));
    };
    for w in order.iter().copied() {
        store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
        if each_at_once {
            put(&store, &keys, w, &mut out);
        }
    }
    if !each_at_once {
        for w in order.iter().copied() {
            put(&store, &keys, w, &mut out);
        }
    }
    out.sort_unstable_by_key(|(w, _)| want_bytes(*w));
    out
}

/// Each cell's bytes made alone, each in a fresh store: through `freeze`, or (a planted run)
/// through a fresh store and `export` with the plant.
fn alone(names: Src, sized: Option<LibKey>, frame: &'static blaze_mem::Frame, order: &[Want], plant: Plant) -> AVec<'static, (Want, AVec<'static, u8>)> {
    let mut out: AVec<'static, (Want, AVec<'static, u8>)> = AVec::new_in(a());
    for w in order.iter().copied() {
        let mut kept = AVec::new_in(a());
        if plant == Plant::None {
            let f = freeze(names, sized, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
            kept.extend_from_slice(&f.bytes);
        } else {
            let one = in_one_store(names, sized, frame, &[w], true, plant);
            kept.extend_from_slice(&one[0].1);
        }
        out.push((w, kept));
    }
    out.sort_unstable_by_key(|(w, _)| want_bytes(*w));
    out
}

/// The cells whose bytes differ between `x` and `y` (both sorted, over the same cells).
fn differing(x: &[(Want, AVec<'static, u8>)], y: &[(Want, AVec<'static, u8>)]) -> AVec<'static, Want> {
    assert_eq!(x.len(), y.len());
    let mut out = AVec::new_in(a());
    for ((wx, bx), (wy, by)) in x.iter().zip(y.iter()) {
        assert_eq!(wx, wy);
        if bx[..] != by[..] {
            out.push(*wx);
        }
    }
    out
}

/// Expected line: `frozen heads: N cells, H heads (fn F, adt A, ctor C, trait T, impl I,
/// opaque O), forward = reversed = alone`.
#[test]
fn frozen_bytes_do_not_depend_on_what_was_made_before() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let mut reversed: AVec<'static, Want> = AVec::new_in(a());
    reversed.extend(cells.iter().rev().copied());

    let alone = alone(names, sized, frame, &cells, Plant::None);
    let forward = in_one_store(names, sized, frame, &cells, true, Plant::None);
    let backward = in_one_store(names, sized, frame, &reversed, false, Plant::None);
    let moved = differing(&alone, &forward);
    assert!(moved.is_empty(), "ORACLE: made forward in one store, these cells froze to other bytes than alone: {moved:?}");
    let moved = differing(&alone, &backward);
    assert!(moved.is_empty(), "ORACLE: made in reverse in one store, these cells froze to other bytes than alone: {moved:?}");

    // The fixture holds every kind the piece names.
    let mut kinds = [0u32; 8];
    let mut heads = 0;
    for (w, bytes) in alone.iter() {
        let frozen = FrozenHead::read(bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        assert_eq!(frozen.want(), *w);
        for e in frozen.heads() {
            heads += 1;
            kinds[match e.at {
                HeadAt::Fn(_) => 0,
                HeadAt::Adt(_) => 1,
                HeadAt::Ctor(_) => 2,
                HeadAt::Trait(_) => 3,
                HeadAt::Impl(_) => 4,
                HeadAt::Opaque(_) => 5,
                HeadAt::Alias(_) => 6,
                HeadAt::TraitAlias(_) => 7,
            }] += 1;
        }
    }
    assert!(kinds[..6].iter().all(|n| *n > 0), "a head kind is missing from the fixture: {kinds:?}");
    assert!(cells.iter().any(|w| matches!(w, Want::MemberFn(..))), "no impl fn cell: {cells:?}");
    std::println!(
        "frozen heads: {} cells, {heads} heads (fn {}, adt {}, ctor {}, trait {}, impl {}, opaque {}), forward = reversed = alone",
        cells.len(),
        kinds[0],
        kinds[1],
        kinds[2],
        kinds[3],
        kinds[4],
        kinds[5]
    );
}

/// Expected line: `import: N heads read back equal to the store's, in its table and a fresh one`.
#[test]
fn a_frozen_head_imports_as_today_s_head() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    // Today's head: every cell made in one store, as a body job's store holds them.
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    for w in cells.iter().copied() {
        store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
    }
    let mut compared = 0;
    for w in cells.iter().copied() {
        let f = freeze(names, sized, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        // Its meaning is the `Signature` out's digest of the same want in today's store.
        let today = want_digest_in(names, &keys, &store, w, a()).unwrap_or_else(|e| panic!("digest {w:?}: {e:?}"));
        assert_eq!(frozen.meaning(), today, "{w:?}: the frozen head means other than the store's");
        // The fn texts, as the body has them.
        let mut texts: AVec<'static, (HeadAt, &'static str)> = AVec::new_in(a());
        for e in frozen.heads() {
            if let HeadAt::Fn(k) = e.at {
                let head = store.fns.get(&k).unwrap_or_else(|| panic!("{w:?}: fn {k:?} not in the store"));
                texts.push((e.at, head.text));
            }
        }
        let mut text_of = |at: HeadAt| texts.iter().find(|(t, _)| *t == at).map(|(_, s)| *s);
        // Into the store's own table: equal to the head filed there, term for term.
        let heads = import(&frozen, &mut text_of, &mut store.tys, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for (at, head) in heads.iter() {
            let filed = head_record::from_store(&store, *at, None, None).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            if let Some(what) = head_record::same(head.as_ref(), filed, store.tys.view()) {
                panic!("ORACLE: {w:?}, head {at:?} imported differs from the store's in {what}");
            }
            compared += 1;
        }
        // Into a fresh table: written again, the same record.
        let mut fresh = PortableTys::new_in(a());
        let again = import(&frozen, &mut text_of, &mut fresh, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for ((at, head), e) in again.iter().zip(frozen.heads()) {
            let bytes = head_record::encode(head.as_ref(), fresh.view(), a()).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            assert!(bytes[..] == e.record[..], "ORACLE: {w:?}, head {at:?} read into a fresh table writes other bytes");
        }
    }
    assert!(compared > 0);
    std::println!("import: {compared} heads read back equal to the store's, in its table and a fresh one");
}

/// Step 1 (head_record.rs form 9): a frozen head carries the store's side tables a later head
/// or a body reads, so the import gives what the store gave: `Walk`'s GAT outlives clause and
/// its associated type's own const parameter, `Dir`'s fieldless-enum count, `Grid`'s impl's
/// members' frame with its const parameter. The lifetime parts stay out of the form: every
/// fn's and ADT's decoded lifetime part is `None` (`longer`'s `'b: 'a` included); what a body
/// reads of them is the borrow check's `BorrowHead` row (borrow_reads.rs), not the head.
/// Control: `same` sees a side row dropped (the fieldless count of a filed enum set to none).
/// Expected line: `side tables: assoc rows R (where W, consts C), fieldless F, frames M`.
#[test]
fn a_frozen_head_carries_the_store_s_side_tables() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    for w in cells.iter().copied() {
        store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
    }
    let (mut rows, mut wheres, mut consts, mut fieldless, mut frames) = (0, 0, 0, 0, 0);
    for w in cells.iter().copied() {
        let f = freeze(names, sized, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        let mut texts: AVec<'static, (HeadAt, &'static str)> = AVec::new_in(a());
        for e in frozen.heads() {
            if let HeadAt::Fn(k) = e.at {
                texts.push((e.at, store.fns.get(&k).unwrap_or_else(|| panic!("fn {k:?}")).text));
            }
        }
        let mut text_of = |at: HeadAt| texts.iter().find(|(t, _)| *t == at).map(|(_, s)| *s);
        let mut fresh = PortableTys::new_in(a());
        let heads = import(&frozen, &mut text_of, &mut fresh, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for (at, head) in heads.iter() {
            match head {
                head_record::Head::Trait { assoc_side, .. } => {
                    for r in assoc_side.iter() {
                        rows += 1;
                        wheres += usize::from(r.where_.is_some_and(|w| !w.is_empty()));
                        consts += usize::from(r.consts.as_ref().is_some_and(|c| c.len() == 1));
                    }
                }
                head_record::Head::Adt { fieldless: f, life, .. } => {
                    assert!(life.is_none(), "{at:?}: an ADT's lifetime part is in the form");
                    fieldless += usize::from(*f == Some(2));
                }
                head_record::Head::Impl { frame: m, .. } => {
                    let m = m.as_ref().unwrap_or_else(|| panic!("ORACLE: {at:?}: an impl's frame is not in its record"));
                    frames += usize::from(m.consts.len() == 1);
                }
                head_record::Head::Fn { life, .. } => assert!(life.is_none(), "{at:?}: a fn's lifetime part is in the form"),
                _ => {}
            }
        }
    }
    assert!(wheres >= 1, "ORACLE: `Walk::Iter`'s `where Self: 'a` is not in the frozen trait ({rows} rows)");
    assert!(consts >= 1, "ORACLE: `Walk::Buf`'s `const N` is not in the frozen trait ({rows} rows)");
    assert!(fieldless >= 1, "ORACLE: `Dir`'s fieldless count is not in the frozen ADT");
    assert!(frames >= 1, "ORACLE: `Grid`'s impl frame with `const N` is not in the frozen impl");
    // Control: a filed enum with its fieldless count dropped is told apart by `same`.
    let mut caught = false;
    for at in head_record::heads_in(&store, a()).iter().copied() {
        let Ok(filed) = head_record::from_store(&store, at, None, None) else { continue };
        if let head_record::HeadRef::Adt { fieldless: Some(_), .. } = filed {
            let mut planted = filed;
            if let head_record::HeadRef::Adt { fieldless, .. } = &mut planted {
                *fieldless = None;
            }
            assert_eq!(head_record::same(filed, planted, store.tys.view()), Some("a fieldless enum's variant count"), "CONTROL: a dropped side row passed");
            caught = true;
        }
    }
    assert!(caught, "CONTROL: no fieldless enum filed");
    std::println!("side tables: assoc rows {rows} (where {wheres}, consts {consts}), fieldless {fieldless}, frames {frames}");
}

/// Step 5 (the engine's cell names): each cell's `CellId` is its item's `Signature`, part 0;
/// a fact about a constructor is a read of its ADT's cell, and about a trait's member fn, of
/// its trait's. Control: the constructor's own key names another `Signature` than the cell's,
/// so a read named by the fact's key would miss the edit of its ADT.
/// Expected line: `cells: N named; constructors C, trait fns T read their container's cell`.
#[test]
fn a_fact_reads_the_cell_of_its_item() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    for w in cells.iter().copied() {
        store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
        let id = super::head::cell_id(w);
        let item = match w {
            Want::MemberFn(_, f) => f,
            w => checker_of(w.key()),
        };
        assert_eq!(id.of, super::head::signature_name(item.0.to_be_bytes()), "{w:?}");
        assert_eq!(id.cell.0, [0; 16], "{w:?}: a head cell is one part");
    }
    let mut ctors = 0;
    for (&k, sig) in store.ctors.iter() {
        let cell = super::head::fact_cell(names, &keys, k).unwrap_or_else(|e| panic!("ctor {k:?}: {e}"));
        assert!(matches!(cell, Want::Adt(lib) if checker_of(lib) == sig.adt), "ORACLE: ctor {k:?} reads {cell:?}, not its ADT's cell");
        assert_ne!(super::head::cell_id(cell).of, super::head::signature_name(k.0.to_be_bytes()), "CONTROL: a ctor's own key names its ADT's cell");
        ctors += 1;
    }
    let mut trait_fns = 0;
    for w in cells.iter().copied() {
        let Want::Members(t) = w else { continue };
        let Some(head) = store.maps.traits.get(&t) else { continue };
        for &(_, f) in head.fns.iter() {
            let cell = super::head::fact_cell(names, &keys, f).unwrap_or_else(|e| panic!("trait fn {f:?}: {e}"));
            assert_eq!(cell, Want::Members(t), "ORACLE: trait fn {f:?} reads {cell:?}");
            trait_fns += 1;
        }
    }
    assert!(ctors > 0 && trait_fns > 0, "the fixture has constructors ({ctors}) and trait fns ({trait_fns})");
    std::println!("cells: {} named; constructors {ctors}, trait fns {trait_fns} read their container's cell", cells.len());
}

/// Control: the comparison sees a job-local id. With the plant on, each fn head's record is
/// followed by its output's index in the store's table; forward and alone then differ.
/// Expected line: `planted: N of M cells caught`.
#[test]
fn a_job_local_id_is_caught() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let forward = in_one_store(names, sized, frame, &cells, true, Plant::JobLocalId);
    let alone = alone(names, sized, frame, &cells, Plant::JobLocalId);
    let caught = differing(&alone, &forward);
    assert!(!caught.is_empty(), "ORACLE BLIND: a job-local id was exported and every cell froze alike");
    std::println!("planted: {} of {} cells caught", caught.len(), cells.len());
}

/// The two sources a body's view reads heads through answer alike, and a freeze's read list
/// is answered by them. Expected line: `sources: N cells alike; R reads, `total` reads `Area``.
#[test]
fn the_two_sources_answer_alike() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    let mut by_store = StoreHeads::new(StoreHeadsOpts { names, keys: &mut keys, store: &mut store, alloc: a() });
    let mut by_freeze = FrozenHeads::new(FrozenHeadsOpts { names, sized, frame, scratch: a(), alloc: a() });
    let mut reads = 0;
    for w in cells.iter().copied() {
        let x: AVec<'static, u8> = {
            let f = by_store.frozen(w).unwrap_or_else(|e| panic!("store {w:?}: {e}"));
            let mut v = AVec::new_in(a());
            v.extend_from_slice(f.bytes());
            v
        };
        let y = by_freeze.frozen(w).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        assert!(x[..] == y.bytes()[..], "ORACLE: {w:?} froze otherwise from the store than alone");
        let f = freeze(names, sized, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        let seen = reads_seen(&f.reads, &mut by_freeze, a()).unwrap_or_else(|e: FreezeError| panic!("reads of {w:?}: {e}"));
        reads += seen.len();
    }
    // `total`'s where-clause reads the trait.
    let mut total = None;
    let mut area = None;
    NamesSource::each_item(names, APP_AT, &mut |key, _| match NamesSource::name_of(names, key) {
        Some("total") => total = Some(key),
        Some("Area") => area = Some(key),
        _ => {}
    });
    let (total, area) = (total.expect("`total`"), area.expect("`Area`"));
    let f = freeze(names, sized, Want::Fn(total), frame, a(), a()).unwrap_or_else(|e| panic!("freeze total: {e}"));
    assert!(f.reads.iter().any(|w| *w == Want::Members(area)), "`total`'s reads miss `Area`: {:?}", &f.reads[..]);
    std::println!("sources: {} cells alike; {reads} reads, `total` reads `Area`", cells.len());
}
