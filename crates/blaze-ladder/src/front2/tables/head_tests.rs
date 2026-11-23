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
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::head::{FreezeError, FrozenHead, FrozenHeads, FrozenHeadsOpts, HeadSource, Plant, SeededHeads, SeededHeadsOpts, StoreHeads, StoreHeadsOpts, export, freeze, head_task, reads_seen, want_bytes};
use crate::front2::by_origin;
use crate::front2::head_record::{self, HeadAt};
use crate::front2::heads::{Store, Want};
use crate::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
use crate::front2::lifetimes::LifeOf;
use crate::front2::project::head_digest::{import_with_opaque_origins, opaque_origin, want_digest_in};
use crate::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};

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
pub(super) const CORE_AT: usize = 1;

/// The fixture's texts: `app` is [`APP`], or an edit of it.
struct Texts {
    app: &'static str,
}

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(self.app),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            _ => None,
        }
    }
}

pub(crate) fn a() -> Alloc<'static> {
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
pub(crate) type Src =&'static Records<'static>;

/// Each crate's records as `blaze check`'s body job gives them, kept for the test run.
struct Kept {
    names: AVec<'static, &'static [u8]>,
    items: AVec<'static, &'static [u8]>,
    /// Per crate: (package path, text).
    files: AVec<'static, AVec<'static, (&'static str, &'static str)>>,
    /// Per crate: (macro call, the text it made), as the engine's store serves an expansion.
    expansions: AVec<'static, AVec<'static, (LibKey, &'static str)>>,
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

    /// The texts, with no origins kept.
    fn expansion(&self, krate: usize, call: blaze_id::ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.iter().find(|(c, _)| *c == call).map(|(_, t)| *t)?;
        Some(LoadedExpansion { text, origins: None })
    }
}

/// The fixture: its records, `Sized`, and a frame for the stores (all kept for the test run).
/// Over records, as the engine's head tasks read: a key is found from the records when first
/// met, so a fresh key map is a whole one (the solved `Names` source needs every key filed
/// first, `Front2::for_program`).
fn world() -> (Src, Option<LibKey>, &'static blaze_mem::Frame) {
    world_of(APP)
}

/// [`world`] over `app` as the app's text.
pub(crate) fn world_of(app: &'static str) -> (Src, Option<LibKey>, &'static blaze_mem::Frame) {
    let g = a();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame");
    let frame: &'static blaze_mem::Frame = blaze_own::Box::leak_static(blaze_own::Box::new_in(frame, g));
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let crates = [input("app", "/app/lib.rs", &[("core", CORE_AT)]), input("core", "/core/lib.rs", &[])];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let solved = resolve_crates(&crates, sysroot, &Texts { app }, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let solved: &'static Names<'static> = blaze_own::Box::leak_static(blaze_own::Box::new_in(solved, g));
    let mut kept = Kept { names: AVec::new_in(g), items: AVec::new_in(g), files: AVec::new_in(g), expansions: AVec::new_in(g) };
    let mut crate_names: AVec<'static, &'static str> = AVec::new_in(g);
    let mut crate_keys: AVec<'static, CrateItemKey> = AVec::new_in(g);
    for c in solved.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), bump));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), bump));
        let mut files = AVec::new_in(g);
        files.extend(c.files.iter().map(|f| (f.package.as_str(), f.text)));
        kept.files.push(files);
        let mut made = AVec::new_in(g);
        // Found by its call's key (`expansion`), so the map's order is never read.
        made.extend(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, m.text))));
        kept.expansions.push(made);
        crate_names.push(c.name);
        crate_keys.push(c.key);
    }
    let log: &'static Log<'static> = blaze_own::Box::leak_static(blaze_own::Box::new_in(Log::new(crate_names.leak_static(), g), g));
    let load: &'static Kept = blaze_own::Box::leak_static(blaze_own::Box::new_in(kept, g));
    let names: Src = blaze_own::Box::leak_static(blaze_own::Box::new_in(Records::new(crate_keys.leak_static(), load, log, frame, g), g));
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
pub(crate) fn cells(names: Src) -> AVec<'static, Want> {
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
    let put = |store: &mut Store<'static>, keys: &Keys, w: Want, out: &mut AVec<'static, (Want, AVec<'static, u8>)>| {
        let bytes = export(names, keys, store, w, plant, a()).unwrap_or_else(|e| panic!("export {w:?}: {e}"));
        let mut kept = AVec::new_in(a());
        kept.extend_from_slice(&bytes);
        out.push((w, kept));
    };
    for w in order.iter().copied() {
        store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
        if each_at_once {
            put(&mut store, &keys, w, &mut out);
        }
    }
    if !each_at_once {
        for w in order.iter().copied() {
            put(&mut store, &keys, w, &mut out);
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
            let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
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
        let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        // Its meaning is the `Signature` out's digest of the same want in today's store.
        let today = want_digest_in(names, &keys, &mut store, w, a()).unwrap_or_else(|e| panic!("digest {w:?}: {e:?}"));
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
        let heads = import_with_opaque_origins(names, &keys, &store, &frozen, &mut text_of, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for (at, head) in heads.iter() {
            let filed = head_record::from_store(&store, *at).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            if let Some(what) = head_record::same(head.as_ref(), filed, store.tys.view()) {
                panic!("ORACLE: {w:?}, head {at:?} imported differs from the store's in {what}");
            }
            compared += 1;
        }
        // Into a fresh table: written again, the same record.
        let fresh = Store::new(sized, frame, a());
        let fresh_keys = Keys::new();
        let again = import_with_opaque_origins(names, &fresh_keys, &fresh, &frozen, &mut text_of, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for ((at, head), e) in again.iter().zip(frozen.heads()) {
            let bytes = head_record::encode_with_origins(head.as_ref(), fresh.tys.view(), a(), &|key| opaque_origin(names, &fresh_keys, &fresh, key)).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            assert!(bytes[..] == e.record[..], "ORACLE: {w:?}, head {at:?} read into a fresh table writes other bytes");
        }
    }
    assert!(compared > 0);
    std::println!("import: {compared} heads read back equal to the store's, in its table and a fresh one");
}

/// Step 1 (head_record.rs form 9): a frozen head carries the store's side tables a later head
/// or a body reads, so the import gives what the store gave: `Walk`'s GAT outlives clause and
/// its associated type's own const parameter, `Dir`'s fieldless-enum count, `Grid`'s impl's
/// members' frame with its const parameter, and (form 10) every fn's, ADT's, trait's and
/// impl's lifetime part, made or refused. Control: `same` sees a side row dropped (the fieldless count of a filed enum set to none).
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
        let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
        let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        let mut texts: AVec<'static, (HeadAt, &'static str)> = AVec::new_in(a());
        for e in frozen.heads() {
            if let HeadAt::Fn(k) = e.at {
                texts.push((e.at, store.fns.get(&k).unwrap_or_else(|| panic!("fn {k:?}")).text));
            }
        }
        let mut text_of = |at: HeadAt| texts.iter().find(|(t, _)| *t == at).map(|(_, s)| *s);
        let fresh = Store::new(sized, frame, a());
        let fresh_keys = Keys::new();
        let heads = import_with_opaque_origins(names, &fresh_keys, &fresh, &frozen, &mut text_of, a(), a()).unwrap_or_else(|e| panic!("import {w:?}: {e}"));
        for (at, head) in heads.iter() {
            match head {
                head_record::Head::Trait { assoc_side, life, .. } => {
                    assert!(life.is_some(), "ORACLE: {at:?}: a trait's lifetime part is not in its record");
                    for r in assoc_side.iter() {
                        rows += 1;
                        wheres += usize::from(r.where_.is_some_and(|w| !w.is_empty()));
                        consts += usize::from(r.consts.as_ref().is_some_and(|c| c.len() == 1));
                    }
                }
                head_record::Head::Adt { fieldless: f, life, .. } => {
                    assert!(life.is_some(), "ORACLE: {at:?}: an ADT's lifetime part is not in its record");
                    fieldless += usize::from(*f == Some(2));
                }
                head_record::Head::Impl { frame: m, life, .. } => {
                    let m = m.as_ref().unwrap_or_else(|| panic!("ORACLE: {at:?}: an impl's frame is not in its record"));
                    assert!(life.is_some(), "ORACLE: {at:?}: an impl's lifetime part is not in its record");
                    frames += usize::from(m.consts.len() == 1);
                }
                // Which refusal, as the store that made every cell filed it for the same fn
                // (`Store::region_gaps`; the freeze's own store is dropped with the freeze).
                head_record::Head::Fn { regions, .. } => assert!(
                    regions.is_some(),
                    "ORACLE: {at:?}: a fn's regions are not in its record (form 22); the gap: {:?}; the fn's own gap: {:?}; its text: {:?}",
                    match at {
                        HeadAt::Fn(k) => store.region_gaps.get(k),
                        _ => None,
                    },
                    match at {
                        HeadAt::Fn(k) => store.fn_gaps.get(k),
                        _ => None,
                    },
                    match at {
                        HeadAt::Fn(k) => store.fns.get(k).map(|h| h.text),
                        _ => None,
                    },
                ),
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
        let Ok(filed) = head_record::from_store(&store, at) else { continue };
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
    let mut by_freeze = FrozenHeads::new(FrozenHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a() });
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
        let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}"));
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
    let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, Want::Fn(total), frame, a(), a()).unwrap_or_else(|e| panic!("freeze total: {e}"));
    assert!(f.reads.iter().any(|w| *w == Want::Members(area)), "`total`'s reads miss `Area`: {:?}", &f.reads[..]);
    std::println!("sources: {} cells alike; {reads} reads, `total` reads `Area`", cells.len());
}

/// A body's store filed from frozen heads alone (`seed` with no own cell, as a body's world
/// would build it): no lowering, and each cell exports the bytes a lowering store does.
/// Expected line: `filed: N cells from frozen heads, 0 lowered, bytes equal`.
#[test]
fn a_store_filed_from_frozen_heads_alone_lowers_nothing() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let alone = alone(names, sized, frame, &cells, Plant::None);
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None });
    let mut store = Store::new(sized, frame, a());
    let keys = Keys::new();
    for w in cells.iter().copied() {
        if !store.made(w) {
            let s = super::head::seed(names, None, w, &mut source, &store, &keys, a()).unwrap_or_else(|e| panic!("seed {w:?}: {e}"));
            assert_eq!(s, crate::front2::heads::Seeded::Filed, "{w:?}");
        }
        assert!(store.made(w), "ORACLE: {w:?} filed from its frozen head is not made");
    }
    assert_eq!(store.lowered, 0, "ORACLE: a store filed from frozen heads lowered");
    let mut filed: AVec<'static, (Want, AVec<'static, u8>)> = AVec::new_in(a());
    for w in cells.iter().copied() {
        let bytes = export(names, &keys, &mut store, w, Plant::None, a()).unwrap_or_else(|e| panic!("export {w:?}: {e}"));
        let mut kept = AVec::new_in(a());
        kept.extend_from_slice(&bytes);
        filed.push((w, kept));
    }
    let moved = differing(&alone, &filed);
    assert!(moved.is_empty(), "ORACLE: filed from frozen heads, these cells export other bytes than lowered: {moved:?}");
    std::println!("filed: {} cells from frozen heads, 0 lowered, bytes equal", cells.len());
}

/// Traits whose members name each other's heads, as core's `Iterator` and `DoubleEndedIterator`
/// do (`fn rev(self) -> Rev<Self> where Self: DoubleEndedIterator`, `trait DoubleEndedIterator:
/// Iterator { fn next_back(&mut self) -> Option<Self::Item>; }`): no cycle in Rust (the
/// Reference, "Traits": only supertraits may not form one), so every cell is made, under both
/// `NeedsMet` strategies, to equal bytes. clap-root's `Cmd` was refused under `All` with "these
/// items need each other's heads in a cycle: [Iterator, .., Rev, ExactSizeIterator,
/// DoubleEndedIterator]"; this shape does NOT reproduce it (it passes under both), so it guards
/// the shape, not that cause. Expected line: `naming traits: N cells made alike under All and First`.
const NAMING: &str = "#![no_std]
pub struct R<T>(pub T);
pub struct S<T>(pub T);
pub trait A {
    type Item;
    fn next(&mut self) -> Self::Item;
    fn r(self) -> R<Self>
    where
        Self: B + Sized;
    fn s(self) -> S<Self>
    where
        Self: C + Sized;
    fn d(&self) -> u32
    where
        Self: D;
}
pub trait B: A {
    fn back(&mut self) -> Self::Item;
    fn rb(self) -> R<Self>
    where
        Self: Sized;
}
pub trait C: A {
    fn len(&self) -> Self::Item;
    fn cs(self) -> S<Self>
    where
        Self: B + Sized;
}
pub trait D: B + C {
    fn both(&self) -> R<Self::Item>;
}
impl<T: B> A for R<T> {
    type Item = T::Item;
    fn next(&mut self) -> T::Item {
        self.0.back()
    }
    fn r(self) -> R<Self>
    where
        Self: B,
    {
        R(self)
    }
    fn s(self) -> S<Self>
    where
        Self: C,
    {
        S(self)
    }
    fn d(&self) -> u32
    where
        Self: D,
    {
        0
    }
}
impl<T: B> B for R<T> {
    fn back(&mut self) -> T::Item {
        self.0.back()
    }
    fn rb(self) -> R<Self> {
        R(self)
    }
}
";

#[test]
fn traits_naming_each_other_are_made_under_both_strategies() {
    let (names, sized, frame) = world_of(NAMING);
    let cells = cells(names);
    let mut each: AVec<'static, AVec<'static, (Want, AVec<'static, u8>)>> = AVec::new_in(a());
    for needs in [crate::front2::heads::NeedsMet::All, crate::front2::heads::NeedsMet::First] {
        // Every cell asked in one store from the first, as a body's store asks them.
        let mut store = Store::new(sized, frame, a());
        store.needs = needs;
        let mut keys = Keys::new();
        for w in cells.iter().copied() {
            store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ORACLE: {needs:?}: ensure {w:?}: {e}"));
        }
        let mut out: AVec<'static, (Want, AVec<'static, u8>)> = AVec::new_in(a());
        for w in cells.iter().copied() {
            let bytes = export(names, &keys, &mut store, w, Plant::None, a()).unwrap_or_else(|e| panic!("{needs:?}: export {w:?}: {e}"));
            let mut kept = AVec::new_in(a());
            kept.extend_from_slice(&bytes);
            out.push((w, kept));
        }
        each.push(out);
    }
    let moved = differing(&each[0], &each[1]);
    assert!(moved.is_empty(), "ORACLE: these cells froze otherwise under All than under First: {moved:?}");
    std::println!("naming traits: {} cells made alike under All and First", cells.len());
}

/// A store filed from frozen heads answers every constructor lookup of every ADT as a store
/// that lowered them: each variant's ctor fn present in one is present in the other, with an
/// equal record. The body `res` of the cost test (`match r { Ok(v) => .., Err(()) => 0 }`)
/// got E0423 when seeded: an enum's tuple-variant ctors were left out of its cell's record
/// (head_digest.rs `want_heads` searched ctors by a key origin only a struct's has).
/// Control: the fixture's enum `Shape` has tuple variants (`Sq`, `Pair`), counted apart; before
/// the fix the seeded store had none of them. Expected line: `ctors: E enum and S struct
/// ctors alike over A ADTs`.
#[test]
fn a_seeded_store_answers_every_ctor_lookup_as_lowering() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let mut lowered = Store::new(sized, frame, a());
    let mut lkeys = Keys::new();
    for w in cells.iter().copied() {
        lowered.ensure(names, &mut lkeys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
    }
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None });
    let seeded = Store::new(sized, frame, a());
    let skeys = Keys::new();
    for w in cells.iter().copied() {
        if !seeded.made(w) {
            super::head::seed(names, None, w, &mut source, &seeded, &skeys, a()).unwrap_or_else(|e| panic!("seed {w:?}: {e}"));
        }
    }
    let (mut enums, mut structs, mut adts) = (0, 0, 0);
    for (adt, row) in lowered.adts.iter() {
        let s = seeded.adts.get(adt).unwrap_or_else(|| panic!("ORACLE: ADT {adt:?} lowered but not filed from its frozen head"));
        assert_eq!(s.variants.len(), row.variants.len(), "ORACLE: ADT {adt:?}'s variants");
        adts += 1;
        for (v, sv) in row.variants.iter().zip(s.variants.iter()) {
            assert_eq!((v.key, v.name, v.ctor), (sv.key, sv.name, sv.ctor), "ORACLE: ADT {adt:?}'s variant");
            let at = HeadAt::Ctor(v.key);
            match (lowered.ctors.contains_key(&v.key), seeded.ctors.contains_key(&v.key)) {
                (false, false) => continue,
                (true, true) => {}
                (l, s) => panic!("ORACLE: ADT {adt:?} variant {:?}: ctor fn lowered {l}, seeded {s}", v.name),
            }
            let x = head_record::encode(head_record::from_store(&lowered, at).unwrap_or_else(|e| panic!("{at:?}: {e}")), lowered.tys.view(), a()).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            let y = head_record::encode(head_record::from_store(&seeded, at).unwrap_or_else(|e| panic!("{at:?}: {e}")), seeded.tys.view(), a()).unwrap_or_else(|e| panic!("{at:?}: {e}"));
            assert!(x[..] == y[..], "ORACLE: ADT {adt:?} variant {:?}: the seeded ctor fn differs from the lowered one", v.name);
            if row.kind == blaze_typeck::AdtKind::Enum {
                enums += 1;
            } else {
                structs += 1;
            }
        }
    }
    assert!(enums >= 2, "CONTROL: `Shape`'s tuple variants' ctors were not compared ({enums})");
    assert!(structs >= 1, "CONTROL: `Square`'s ctor was not compared ({structs})");
    std::println!("ctors: {enums} enum and {structs} struct ctors alike over {adts} ADTs");
}

/// The wants a cell's own item makes: an ADT's head and definition, a trait's head and
/// members; one for any other cell.
pub(super) fn own_wants(cell: Want) -> u32 {
    match cell {
        Want::Adt(_) | Want::Members(_) => 2,
        Want::Head(_) | Want::Impl(_) | Want::Fn(_) | Want::MemberFn(..) => 1,
    }
}

/// A store seeded from frozen heads lowers only each item's own wants, and freezes to the
/// bytes a store that lowered everything does. Expected line: `seeded: N cells, L wants
/// lowered (own O), unseeded U, bytes equal`.
#[test]
fn a_seeded_freeze_lowers_each_item_once() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let alone = alone(names, sized, frame, &cells, Plant::None);
    // Today's: each cell frozen alone lowers every head it needs again.
    let mut unseeded = 0u32;
    for w in cells.iter().copied() {
        unseeded += freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze {w:?}: {e}")).lowered;
    }
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None });
    let mut seeded: AVec<'static, (Want, AVec<'static, u8>)> = AVec::new_in(a());
    for w in cells.iter().copied() {
        let f = source.frozen(w).unwrap_or_else(|e| panic!("seeded {w:?}: {e}"));
        let mut kept = AVec::new_in(a());
        kept.extend_from_slice(f.bytes());
        seeded.push((w, kept));
    }
    seeded.sort_unstable_by_key(|(w, _)| want_bytes(*w));
    let moved = differing(&alone, &seeded);
    assert!(moved.is_empty(), "ORACLE: seeded from frozen heads, these cells froze to other bytes than alone: {moved:?}");
    // Every cell the source froze: the app's, and core's its items need (`Sized` and the rest).
    let made = source.cells().count();
    let own: u32 = source.cells().map(own_wants).sum();
    let app_own: u32 = cells.iter().map(|w| own_wants(*w)).sum();
    // Beyond them, one lowering per need met of a cell up the stack: two cells whose items
    // need each other's heads (core's sizedness traits: a cell is a trait's head AND members).
    assert_eq!(
        source.lowered,
        own + source.reentered,
        "ORACLE: seeded, {made} cells lowered other than each item's own wants and one per re-entry ({} needs met of a cell up the stack; unseeded: {unseeded}, the app's own {app_own})",
        source.reentered
    );
    // The control: without the seed, a dependency is lowered again by each item that needs it.
    assert!(unseeded > app_own, "the fixture's items need no other's head: unseeded {unseeded}, own {app_own}");
    std::println!(
        "seeded: {} app cells ({made} with core's), {} wants lowered (own {own}, re-entered {}), unseeded {unseeded} for the app's, bytes equal",
        cells.len(),
        source.lowered,
        source.reentered
    );
}

/// One head task per cell of the app, in forward and in reversed order, each order over one
/// shared seeded source: every task's result (name, key, out, bytes, defines, reads) is equal
/// across the orders; each item is lowered once (the source's lowerings are its cells' own
/// wants plus one per re-entry, as `a_seeded_freeze_lowers_each_item_once` states); `out` is the
/// meaning of `bytes`, `defines` that one cell, `reads` sorted and free of the task's own cell.
/// Control: another tool gives another key, the same out. Expected line:
/// `head tasks: N cells, forward and reversed equal, lowered L (own O, re-entered R) in each`.
#[test]
fn head_tasks_are_equal_in_any_order() {
    let (names, sized, frame) = world();
    let cells = cells(names);
    let tool = blaze_drive::key::Tool(ContentId::of("blaze-ladder head task test", b"head"));
    let run = |order: &[Want]| {
        let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None });
        let mut done: AVec<'static, (Want, super::head::HeadDone<'static>)> = AVec::new_in(a());
        for w in order.iter().copied() {
            let d = head_task(names, &mut source, w, tool, a());
            if let Some(e) = &d.failed {
                panic!("head task of {w:?} failed: {e}");
            }
            done.push((w, d));
        }
        done.sort_unstable_by_key(|(w, _)| want_bytes(*w));
        let own: u32 = source.cells().map(own_wants).sum();
        (done, source.lowered, own, source.reentered)
    };
    let mut reversed: AVec<'static, Want> = AVec::new_in(a());
    reversed.extend(cells.iter().rev().copied());
    let (fwd, lowered_f, own_f, re_f) = run(&cells[..]);
    let (rev, lowered_r, own_r, re_r) = run(&reversed[..]);
    assert_eq!(lowered_f, own_f + re_f, "ORACLE: forward, an item was lowered more than once");
    assert_eq!(lowered_r, own_r + re_r, "ORACLE: reversed, an item was lowered more than once");
    assert_eq!(fwd.len(), rev.len());
    for ((w, f), (_, r)) in fwd.iter().zip(rev.iter()) {
        assert_eq!((f.name, f.key, f.out), (r.name, r.key, r.out), "ORACLE: {w:?}'s name, key or out differs by order");
        assert!(f.bytes[..] == r.bytes[..], "ORACLE: {w:?}'s bytes differ by order");
        assert!(f.defines[..] == r.defines[..] && f.reads[..] == r.reads[..], "ORACLE: {w:?}'s defines or reads differ by order");
        let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("{w:?}: {e}"));
        assert_eq!(frozen.meaning(), f.out, "{w:?}: out is the bytes' meaning");
        assert_eq!(f.name, super::head::cell_id(*w).of, "{w:?}: named as its cell");
        assert!(f.defines[..] == [(super::head::cell_id(*w).cell, f.out)], "{w:?}: defines its one cell");
        assert!(f.reads.windows(2).all(|p| p[0].0 < p[1].0), "{w:?}: reads sorted, one row a cell");
        assert!(f.reads.iter().all(|(c, _)| *c != super::head::cell_id(*w)), "{w:?}: reads its own cell");
    }
    // Control: the key is the tool's too; the out is not.
    let other = blaze_drive::key::Tool(ContentId::of("blaze-ladder head task test", b"another"));
    let mut source = SeededHeads::new(SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None });
    let (w, f) = &fwd[0];
    let d = head_task(names, &mut source, *w, other, a());
    assert!(d.key != f.key && d.out == f.out, "another tool: another key, the same out");
    std::println!(
        "head tasks: {} cells, forward and reversed equal, lowered {lowered_f} (own {own_f}, re-entered {re_f}) forward, {lowered_r} (own {own_r}, re-entered {re_r}) reversed",
        cells.len()
    );
}

/// Step 1 (head_record.rs form 10): a REAL edit of only a lifetime bound moves the head's
/// meaning, frozen and today's (`want_digest_in`, a `Signature`'s out) alike; a comment written
/// inside the signature moves neither. Control: the comment edit does change the record's bytes
/// (the fn text's digest), so the equal meaning is the reader digest's doing.
/// Expected line: `lifetimes: `longer` moves with `'b: 'a` (bounds 1 -> 0), not with a comment`.
#[test]
fn a_lifetime_bound_alone_moves_the_head() {
    let base = longer_of(APP);
    let unbound = longer_of(edited(APP, "'b: 'a>", "'b>"));
    let commented = longer_of(edited(APP, "y: &'b u32)", "/* the other */ y: &'b u32)"));
    assert_eq!(base.frozen, base.today, "a frozen head means other than today's");
    assert_ne!(base.frozen, unbound.frozen, "ORACLE: dropping `'b: 'a` left the frozen head's meaning");
    assert_ne!(base.today, unbound.today, "ORACLE: dropping `'b: 'a` left the `Signature`'s digest");
    assert_eq!(base.frozen, commented.frozen, "ORACLE: a comment in the signature moved the frozen head");
    assert_eq!(base.today, commented.today, "ORACLE: a comment in the signature moved the `Signature`'s digest");
    assert!(base.bytes[..] != commented.bytes[..], "CONTROL: the comment edit changed nothing the record holds");
    assert_eq!((base.bounds, unbound.bounds), (1, 0), "`longer`'s declared bounds");
    std::println!("lifetimes: `longer` moves with `'b: 'a` (bounds {} -> {}), not with a comment", base.bounds, unbound.bounds);
}

/// `longer`'s cell in a world over `app`: its frozen meaning, today's digest, its frozen
/// bytes, and how many lifetime bounds its lifetime part declares.
struct Longer {
    frozen: ContentId,
    today: ContentId,
    bytes: AVec<'static, u8>,
    bounds: usize,
}

fn longer_of(app: &'static str) -> Longer {
    let (names, sized, frame) = world_of(app);
    let mut found = None;
    NamesSource::each_item(names, APP_AT, &mut |key, _| {
        if NamesSource::name_of(names, key) == Some("longer") {
            found = Some(key);
        }
    });
    let lib = found.expect("`longer`");
    let w = Want::Fn(lib);
    let f = freeze(names, sized, crate::front2::receiver::Pointers::NOT_LOOKED_UP, w, frame, a(), a()).unwrap_or_else(|e| panic!("freeze `longer`: {e}"));
    let frozen = FrozenHead::read(&f.bytes).unwrap_or_else(|e| panic!("read `longer`: {e}")).meaning();
    let mut store = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure `longer`: {e}"));
    let today = want_digest_in(names, &keys, &mut store, w, a()).unwrap_or_else(|e| panic!("digest `longer`: {e:?}"));
    let bounds = match store.lives_of(HeadAt::Fn(checker_of(lib))) {
        Some(LifeOf::Fn(Ok(l))) => l.bounds.len(),
        other => panic!("ORACLE: `longer`'s lifetime part: {other:?}"),
    };
    let mut bytes = AVec::new_in(a());
    bytes.extend_from_slice(&f.bytes);
    Longer { frozen, today, bytes, bounds }
}

/// `from` with its one `old` written as `new`, kept for the test run.
fn edited(from: &'static str, old: &str, new: &str) -> &'static str {
    let at = from.find(old).unwrap_or_else(|| panic!("`{old}` is not in the fixture"));
    let mut v: AVec<'static, u8> = AVec::new_in(a());
    v.extend_from_slice(from[..at].as_bytes());
    v.extend_from_slice(new.as_bytes());
    v.extend_from_slice(from[at + old.len()..].as_bytes());
    core::str::from_utf8(v.leak_static()).expect("an edit of UTF-8 by UTF-8")
}

/// D148 across stores (tempo: 9,115 bodies refused `[NoHead]`, every callee whose head came
/// from a record): every head of a kind that holds regions reaches another store laid or
/// refused by name, never absent. Oracle: a free fn `f(&u8) -> &u8`, an inherent `S::m`, a
/// trait's declared `Tr::g` and its impl's for `u8`, each frozen to its cell's record and
/// filed into a store holding nothing else (no text made into heads there), read the same
/// signature column as the store that made them; and a head whose regions its maker refused
/// softly (planted: `Unlaid::NoContainer`, a kind past the first five bytes) or never made (planted: refused, then not filed)
/// reads `Refused` with its kind's name in the other store, as in its maker. Control (rule
/// 23): the old filer (the decoded heads to `file_heads` alone, no recorded regions) reads
/// both planted heads as `NoHead`, the tempo answer.
#[test]
fn every_head_reaches_another_store_laid_or_refused_by_name() {
    use super::head::{RecordedRegions, file_frozen};
    use blaze_typeck::region_facts::UnreadWhy;
    use blaze_typeck::regions::Region;
    const APP: &str = "pub fn f(x: &u8) -> &u8 { x }
pub struct S { b: u8 }
impl S {
    pub fn m(&self) -> &u8 { &self.b }
}
pub trait Tr {
    fn g(&self) -> &u8;
}
impl Tr for u8 {
    fn g(&self) -> &u8 { self }
}
pub fn soft(x: &u8) -> &u8 { x }
pub fn unmade(x: &u8) -> &u8 { x }
";
    let (names, sized, frame) = world_of(APP);
    let named = |name: &str| {
        let mut found = None;
        NamesSource::each_item(names, APP_AT, &mut |key, _| {
            if NamesSource::name_of(names, key) == Some(name) {
                found = Some(key);
            }
        });
        found.unwrap_or_else(|| panic!("`{name}` in the fixture"))
    };
    let sig = |store: &Store<'_>, k: blaze_typeck::ItemKey| -> std::vec::Vec<Region> {
        let filed = store.regions.made(k).unwrap_or_else(|| panic!("ORACLE: {k:?}'s regions: {}", store.regions.unread(k)));
        filed.regions().sig.as_ref().unwrap_or_else(|| panic!("ORACLE: {k:?} has no signature column")).as_slice().to_vec()
    };
    // Each fn cell made in its own store, frozen, and filed into a store of nothing else.
    let mut laid = 0;
    for w in cells(names).iter().copied() {
        if !matches!(w, Want::Fn(_) | Want::MemberFn(..) | Want::Members(_)) {
            continue;
        }
        if let Want::Fn(lib) = w
            && (lib == named("soft") || lib == named("unmade"))
        {
            continue;
        }
        let mut made = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        made.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure {w:?}: {e}"));
        let bytes = export(names, &keys, &mut made, w, Plant::None, a()).unwrap_or_else(|e| panic!("export {w:?}: {e}"));
        let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read {w:?}: {e}"));
        let other = Store::new(sized, frame, a());
        let other_keys = Keys::new();
        file_frozen(names, w, frozen, &other, &other_keys, a()).unwrap_or_else(|e| panic!("file {w:?}: {e}"));
        for e in frozen.heads() {
            let HeadAt::Fn(k) = e.at else { continue };
            assert_eq!(e.recorded, RecordedRegions::Laid, "ORACLE: {w:?}'s fn {k:?} is not recorded laid");
            assert_eq!(sig(&other, k), sig(&made, k), "ORACLE: {w:?}'s fn {k:?} reads another column filed from its record");
            laid += 1;
        }
    }
    // `f`, `S::m`, `Tr::g` (the trait's) and `<u8 as Tr>::g`.
    assert!(laid >= 4, "ORACLE: {laid} fn heads crossed stores, fewer than the four callees");
    // The planted heads: a soft refusal and regions never made.
    for (name, plant) in [("soft", true), ("unmade", false)] {
        let lib = named(name);
        let (w, k) = (Want::Fn(lib), checker_of(lib));
        let mut made = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        made.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure '{name}': {e}"));
        if plant {
            made.regions.file_unlaid(k, head_record::Unlaid::NoContainer, head_record::Unlaid::NoContainer.name());
        } else {
            made.regions.file_refused(k);
        }
        let want_kind = if plant { head_record::Unlaid::NoContainer } else { head_record::Unlaid::NotMade };
        let bytes = export(names, &keys, &mut made, w, Plant::None, a()).unwrap_or_else(|e| panic!("export '{name}': {e}"));
        let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read '{name}': {e}"));
        let other = Store::new(sized, frame, a());
        file_frozen(names, w, frozen, &other, &Keys::new(), a()).unwrap_or_else(|e| panic!("file '{name}': {e}"));
        assert!(other.fns.contains_key(&k), "ORACLE: '{name}'s head was not filed");
        assert_eq!(other.regions.unread(k).why, UnreadWhy::Refused { what: want_kind.name() }, "ORACLE: `{name}` reached the other store without its refusal's name");
        // CONTROL: the old filer, the heads alone.
        let old = Store::new(sized, frame, a());
        let mut text_of = |at: HeadAt| match at {
            HeadAt::Fn(_) => names.seeded_text_of(lib).map(|t| t.text),
            _ => None,
        };
        let heads = import_with_opaque_origins(names, &Keys::new(), &old, &frozen, &mut text_of, a(), a()).unwrap_or_else(|e| panic!("import `{name}`: {e}"));
        old.file_heads(w, heads).unwrap_or_else(|e| panic!("file `{name}`'s heads alone: {e:?}"));
        assert_eq!(old.regions.unread(k).why, UnreadWhy::NoHead, "CONTROL: the heads alone already carried `{name}`'s refusal, so the recorded regions were not the cause");
    }
}

/// The fixture's world, handed whole to the one thread that files on a small stack.
struct Handed {
    names: Src,
    sized: Option<LibKey>,
    frame: &'static blaze_mem::Frame,
    bytes: std::vec::Vec<u8>,
    want: Want,
}

// SAFETY: `Records` and `Frame` are not `Sync` (cells, a bump cursor) because two threads must
// not use one at once. The test thread hands the world to exactly one spawned thread and then
// only joins it: nothing of the world is touched on the test thread until the join returns,
// so every use is from one thread at a time, ordered by the join. Neither holds a reference
// into a thread-local.
unsafe impl Send for Handed {}

/// The stack a decoded head costs. blaze-cli `a_future_that_stops_being_send_reaches_its_caller`
/// overflowed a 2 MiB stack 145 frames deep, through head_record `decode_retaining`,
/// `import_with_opaque_origins`, `file_frozen` and `seed`: every frame held heads with their
/// clauses (`Preds`) and regions (`FiledRegions`) inline, ~1.4 KB a copy, and a debug frame
/// keeps a copy per temporary. Oracle: a fn cell holding every optional part a fn's record
/// has (where-clauses, laid regions, an opaque with its bounds and laid regions) is read and
/// filed into a fresh store on a thread of 256 KiB, and every part is filed there. Control
/// (rule 23): on the test's own stack the record is checked to hold every part, so the small
/// thread is not passed by a record with little to decode. The sizes themselves are held by
/// `head_record::HEAD_MOST` and `FILED_REGIONS_MOST`, the inline form refused beside them.
#[test]
fn a_head_with_every_part_files_on_a_small_stack() {
    use super::head::{RecordedRegions, file_frozen};
    const APP: &str = "#![no_std]
pub trait Tr {}
impl Tr for u8 {}
pub fn every<'a, T: Tr>(x: &'a T, y: &'a u8) -> impl Tr + 'a
where
    T: 'a,
{
    *y
}
";
    const STACK: usize = 256 << 10;
    let (names, sized, frame) = world_of(APP);
    let mut lib = None;
    NamesSource::each_item(names, APP_AT, &mut |key, _| {
        if NamesSource::name_of(names, key) == Some("every") {
            lib = Some(key);
        }
    });
    let want = Want::Fn(lib.expect("`every` in the fixture"));
    let mut made = Store::new(sized, frame, a());
    let mut keys = Keys::new();
    made.ensure(names, &mut keys, want).unwrap_or_else(|e| panic!("ensure `every`: {e}"));
    let bytes = export(names, &keys, &mut made, want, Plant::None, a()).unwrap_or_else(|e| panic!("export `every`: {e}"));
    // CONTROL: the record holds a fn and its opaque, each with its clauses and laid regions.
    let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read `every`: {e}"));
    let (mut fns, mut opaques) = (0, 0);
    for e in frozen.heads() {
        let k = head_record::regions_key(e.at);
        match e.at {
            HeadAt::Fn(_) => fns += 1,
            HeadAt::Opaque(_) => opaques += 1,
            other => panic!("CONTROL: `every`'s cell holds a {other:?}"),
        }
        assert_eq!(e.recorded, RecordedRegions::Laid, "CONTROL: {:?}'s regions are not in the record", e.at);
        assert!(made.preds.get(&k).is_some(), "CONTROL: {:?} has no clauses to record", e.at);
    }
    assert_eq!((fns, opaques), (1, 1), "CONTROL: `every`'s cell is not one fn and its opaque");
    let handed = Handed { names, sized, frame, bytes: bytes.to_vec(), want };
    let small = std::thread::Builder::new()
        .name(std::string::String::from("head-on-a-small-stack"))
        .stack_size(STACK)
        .spawn(move || {
            let h = handed;
            let mut kept: AVec<'static, u8> = AVec::new_in(a());
            kept.extend_from_slice(&h.bytes);
            let frozen = FrozenHead::read(&kept).unwrap_or_else(|e| panic!("read `every` on the small stack: {e}"));
            let other = Store::new(h.sized, h.frame, a());
            file_frozen(h.names, h.want, frozen, &other, &Keys::new(), a()).unwrap_or_else(|e| panic!("file `every` on the small stack: {e}"));
            let mut filed = 0;
            for e in frozen.heads() {
                let k = head_record::regions_key(e.at);
                assert!(other.regions.made(k).is_some(), "ORACLE: {:?}'s regions were not filed: {}", e.at, other.regions.unread(k));
                assert!(other.preds.get(&k).is_some(), "ORACLE: {:?}'s clauses were not filed", e.at);
                if let HeadAt::Fn(_) = e.at {
                    assert!(other.fns.contains_key(&k), "ORACLE: {:?}'s head was not filed", e.at);
                }
                filed += 1;
            }
            filed
        })
        .unwrap_or_else(|e| panic!("spawn a thread of {STACK} bytes: {e}"));
    let filed = small.join().unwrap_or_else(|_| panic!("ORACLE: filing `every` on a {STACK}-byte stack failed"));
    assert_eq!(filed, 2, "ORACLE: the small stack filed {filed} heads, not the fn and its opaque");
}

/// A fn whose signature names an ADT filed from its RECORD lays the column a store that
/// lowered the ADT from text lays (D148, D155). `V<u8>` is `V<u8, G>`: a store lowering `V`
/// from text makes `G` on the way (its default), one filing `V`'s frozen head (every engine
/// store: a dependency's, std's `Vec<u8>` is `Vec<u8, Global>`) never names `G`, and before
/// D155 the column's walk met `G` uncounted (the tempo `[NoHead]` of every `Vec`-taking
/// toolchain or dependency fn). `G`'s count is a fact of its text, read from its crate's items
/// record through the names, so no head of `G` is needed. Oracle: filed from `V`'s record, `d`
/// lays the column the text store lays. Controls (rule 23): `G` is unmade in the seeded store
/// before `d` is made, so the oracle is not met by a store that never lacked it; and with the
/// names answering no count for `G` (`Records::plant_no_lifetimes`) the same seeded store
/// refuses `d`'s column by name (`Unlaid::Counts`).
#[test]
fn a_fn_over_a_recorded_adt_lays_its_defaults_counts() {
    const APP: &str = "pub struct G;
pub struct V<T, A = G> { t: T, a: A }
pub fn d(v: &V<u8>) -> &u8 { &v.t }
";
    let (names, sized, frame) = world_of(APP);
    let named = |name: &str| {
        let mut found = None;
        NamesSource::each_item(names, APP_AT, &mut |key, _| {
            if NamesSource::name_of(names, key) == Some(name) {
                found = Some(key);
            }
        });
        found.unwrap_or_else(|| panic!("`{name}` in the fixture"))
    };
    let (g, v, d) = (named("G"), named("V"), named("d"));
    let (w, k) = (Want::Fn(d), checker_of(d));
    let sig = |store: &Store<'_>| -> std::vec::Vec<blaze_typeck::regions::Region> {
        let filed = store.regions.made(k).unwrap_or_else(|| panic!("ORACLE: `d`'s regions: {}", store.regions.unread(k)));
        filed.regions().sig.as_ref().unwrap_or_else(|| panic!("ORACLE: `d` has no signature column")).as_slice().to_vec()
    };
    // The text store: `d` made from nothing, `V` and its default lowered from text.
    let mut text = Store::new(sized, frame, a());
    let mut text_keys = Keys::new();
    text.ensure(names, &mut text_keys, w).unwrap_or_else(|e| panic!("ensure `d`: {e}"));
    assert!(text.made(text.need(g)), "CONTROL: the text store did not make `G`, so it is not the default's maker");
    // The seeded store: `V`'s frozen head filed first, then `d` made.
    let mut maker = Store::new(sized, frame, a());
    let mut maker_keys = Keys::new();
    maker.ensure(names, &mut maker_keys, Want::Adt(v)).unwrap_or_else(|e| panic!("ensure `V`: {e}"));
    let bytes = export(names, &maker_keys, &mut maker, Want::Adt(v), Plant::None, a()).unwrap_or_else(|e| panic!("export `V`: {e}"));
    let seeded_store = |planted: bool| -> Store<'static> {
        let frozen = FrozenHead::read(&bytes).unwrap_or_else(|e| panic!("read `V`: {e}"));
        let mut seeded = Store::new(sized, frame, a());
        let mut seeded_keys = Keys::new();
        super::head::file_frozen(names, Want::Adt(v), frozen, &seeded, &seeded_keys, a()).unwrap_or_else(|e| panic!("file `V`: {e}"));
        assert!(!seeded.made(seeded.need(g)), "CONTROL: filing `V`'s record made `G`: the cause is absent and the oracle proves nothing");
        if planted {
            names.plant_no_lifetimes(g);
        }
        seeded.ensure(names, &mut seeded_keys, w).unwrap_or_else(|e| panic!("ensure `d` over `V`'s record: {e}"));
        seeded
    };
    let seeded = seeded_store(false);
    assert_eq!(sig(&seeded), sig(&text), "ORACLE: `d` over `V`'s record lays another column than over `V`'s text");
    // CONTROL: the names answer no count for `G`; `d`'s column is refused by name.
    let planted = seeded_store(true);
    assert!(planted.regions.made(k).is_none(), "CONTROL: with `G`'s count unanswered `d`'s column was laid");
    let kind = planted.region_gaps.get(&k).map(crate::front2::heads::RegionGap::unlaid);
    assert_eq!(kind, Some(head_record::Unlaid::Counts), "CONTROL: `d`'s refusal is not `Counts`: {:?}", planted.region_gaps.get(&k));
}
