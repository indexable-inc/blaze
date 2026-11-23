//! Head records (front2/head_record.rs; docs/v2/plan/design/drive.md, "End state", piece 10):
//! every head the store makes for a small crate, and for every rung's crate, is encoded,
//! decoded and compared equal to the one lowered, then encoded again byte for byte. Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_region_rules.rs does.
//!
//! How equality is taken: the head is decoded into the store's own type table, which is
//! hash-consed (equal structure is an equal `PTy`), and `head_record::same` compares it with
//! the lowered head field by field, a type by its `PTy` and a list by its elements. Then the
//! decoded head is encoded again (byte-equal), and decoded into a FRESH table and encoded from
//! there (byte-equal: the bytes hold structure, never the store's interner order).
//!
//! Prints, when right (the counts include the library heads the crate's heads needed):
//! - `front2_head_records: <n> heads round-trip equal (fn <a>, adt <b>, ctor <c>, trait <d>,
//!   impl <e>, alias <f>, opaque <g>; statics <h>; lifetime parts <i>); every cut refused`
//! - `front2_head_records: two heads differing in one lifetime have different digests`
//! - `front2_head_records: <n> heads of <r> rungs round-trip equal (<s> rungs not of a blaze
//!   edition left out)`

#![feature(allocator_api)]

#[allow(dead_code)]
mod rungs;

use std::collections::BTreeMap;

use blaze_id::ItemKey as LibKey;
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{
    HeadAt, HeadRecordError, HeadRef, decode_retaining as decode, digest, encode, encode_with_origins, from_store, heads_in, reader_digest, regions_digest,
    same,
};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::Paths;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{ItemDetail, Namespace, PathKind};
use blaze_ladder::front2::lifetimes::LifeOf;
use blaze_ladder::front2::rows::stable_tys;

const EDITION: &str = "blaze-2026.11";

/// Every head kind the store makes: a fn with generics, lifetimes and a where clause; a struct,
/// an enum with discriminants, a union, a tuple struct (its constructor); a trait with an
/// associated type, an associated const and a default method; an inherent and a trait impl; a
/// type alias; a const and a static; `impl Trait` in argument and return position (an opaque
/// head); `dyn Trait` with a lifetime bound; a const generic. `pick_x` and `pick_y` differ only
/// in the lifetime of their output (the planted difference).
const TEXT: &str = "
use core::fmt::Debug;
pub trait Shape { type Out; const SIDES: u32; fn area(&self) -> u32; fn twice(&self) -> u32 { self.area() * 2 } }
pub struct Sq<'a, T> { side: &'a T }
pub enum E { A = 1, B = 5, C }
pub union U { a: u32, b: f32 }
pub struct Tup(u8, u16);
pub struct Arr<const N: usize> { a: [u8; N] }
impl<'a, T> Sq<'a, T> { pub fn get(&self) -> &'a T { self.side } }
impl<'a> Shape for Sq<'a, u32> { type Out = u32; const SIDES: u32 = 4; fn area(&self) -> u32 { *self.side * *self.side } }
pub type Pair<T> = (T, T);
pub const LIMIT: usize = 3;
pub static NAME: &str = \"n\";
pub fn pick<'a, 'b: 'a, T>(x: &'a T, _y: &'b T) -> &'a T where T: Clone { x }
pub fn show(x: impl Debug) -> impl Debug { x }
pub fn boxed<'a>(x: &'a (dyn Debug + 'a)) -> &'a (dyn Debug + 'a) { x }
pub fn first<const N: usize>(a: Arr<N>) -> u8 { a.a[0] }
pub fn pair(p: Pair<u8>, e: E, u: U, t: Tup) -> u8 { let _ = (e, u, t); p.0 }
pub fn pick_x<'x, 'y>(p: &'x u8, q: &'y u8) -> &'x u8 { let _ = q; p }
pub fn pick_y<'x, 'y>(p: &'x u8, q: &'y u8) -> &'y u8 { let _ = p; q }
fn main() {}
";

/// What one crate's round trip saw.
#[derive(Default)]
struct Seen {
    by_kind: BTreeMap<&'static str, usize>,
    statics: usize,
    lives: usize,
    /// Heads refused by name (`HeadRecordError::Unencoded`), and items whose head the store
    /// refused: each with its item.
    refused: Vec<String>,
}

impl Seen {
    fn heads(&self) -> usize {
        self.by_kind.values().sum()
    }
    fn of(&self, kind: &str) -> usize {
        self.by_kind.get(kind).copied().unwrap_or(0)
    }
}

fn kind(at: HeadAt) -> &'static str {
    match at {
        HeadAt::Fn(_) => "fn",
        HeadAt::Adt(_) => "adt",
        HeadAt::Ctor(_) => "ctor",
        HeadAt::Trait(_) => "trait",
        HeadAt::Impl(_) => "impl",
        HeadAt::Alias(_) => "alias",
        HeadAt::TraitAlias(_) => "trait alias",
        HeadAt::Opaque(_) => "opaque",
    }
}

/// The toolchain on its shelf.
fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Makes every head of crate 0's items (`rel` its files by path under its directory), then
/// round-trips every head in the store; with `cuts`, every strict prefix of every record too.
fn round_trip_all(what: &str, rel: &[(String, String)], edition: &str, cuts: bool) -> Seen {
    let tc = toolchain();
    let files: Vec<(String, String)> = rel.iter().map(|(p, t)| (format!("/krate/{p}"), t.clone())).collect();
    let edition = edition::PackageEdition::of_manifest(edition).expect("a known edition");
    let user = UserCrate { name: "krate", dir: "/krate/", root: "/krate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("{what}: the crates: {e}"));
    let crates = inputs.crates();
    // The test's one heap handle, named once (the ambient-heap ratchet counts each line naming it).
    let g = Alloc::global();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{what}: resolve_crates: {e}"));
    let sources: Vec<SourceFile<'_>> = rel.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, g).unwrap_or_else(|e| panic!("{what}: front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, g);
    let mut seen = Seen::default();

    // Every item of the crate, asked for by its kind.
    let mut items: Vec<LibKey> = names.items(0).map(|(k, _, _)| k).collect();
    items.sort();
    for k in items {
        let wants = match (Paths { names: &names }).detail(k) {
            Some(ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => vec![Want::Head(k), Want::Adt(k)],
            Some(ItemDetail::Trait) => vec![Want::Head(k), Want::Members(k)],
            Some(ItemDetail::TraitAlias | ItemDetail::TypeAlias) => vec![Want::Head(k)],
            Some(ItemDetail::Impl) => vec![Want::Impl(k)],
            Some(ItemDetail::Fn { .. } | ItemDetail::Const { .. } | ItemDetail::Static { .. }) => vec![Want::Fn(k)],
            _ => vec![],
        };
        for want in wants {
            if let Err(e) = store.ensure(&names, &mut front.keys, want) {
                seen.refused.push(format!("{what}: the store refused {want:?}: {e:?}"));
            }
        }
    }

    // The lifetime parts of every fn and ADT, made (or refused) in the store: a record holds them.
    let ats: Vec<HeadAt> = heads_in(&store, g).iter().copied().collect();
    let parts: Vec<HeadAt> = ats.iter().copied().filter(|at| matches!(at, HeadAt::Fn(_) | HeadAt::Adt(_))).collect();
    store.make_lives(&names, &front.keys, &parts);

    let origin = |key| blaze_ladder::front2::project::head_digest::opaque_origin(&names, &front.keys, &store, key);
    for at in ats {
        let made_life = matches!(store.lives_of(at), Some(LifeOf::Fn(Ok(_)) | LifeOf::Adt(Ok(_))));
        let made = from_store(&store, at).and_then(|h| Ok((encode_with_origins(h, store.tys.view(), g, &origin)?, h.text())));
        let (bytes, text) = match made {
            Ok(made) => made,
            Err(e @ HeadRecordError::Unencoded { .. }) => {
                seen.refused.push(format!("{what}: {at:?}: {e}"));
                continue;
            }
            Err(e) => panic!("{what}: {at:?}: not encoded: {e}"),
        };
        let decoded = decode(&bytes, text, &store.tys, g).unwrap_or_else(|e| panic!("{what}: {at:?}: not decoded: {e}"));
        let lowered = from_store(&store, at).expect("encoded above");
        if let Some(part) = same(lowered, decoded.as_ref(), store.tys.view()) {
            panic!("{what}: {at:?}: the decoded head differs from the lowered one in {part}");
        }
        let again = decoded.encode(store.tys.view(), g).unwrap_or_else(|e| panic!("{what}: {at:?}: decoded, not encoded again: {e}"));
        assert!(bytes[..] == again[..], "{what}: {at:?}: encoded again, the bytes differ");
        let fresh = stable_tys(g).expect("a fresh stable table");
        let elsewhere = decode(&bytes, text, &fresh, g).unwrap_or_else(|e| panic!("{what}: {at:?}: not decoded into a fresh table: {e}"));
        let from_fresh = elsewhere.encode(fresh.view(), g).unwrap_or_else(|e| panic!("{what}: {at:?}: not encoded from a fresh table: {e}"));
        assert!(bytes[..] == from_fresh[..], "{what}: {at:?}: encoded from a fresh table, the bytes differ");
        if cuts {
            for cut in 0..bytes.len() {
                let scratch = stable_tys(g).expect("a fresh stable table");
                match decode(&bytes[..cut], text, &scratch, g) {
                    Err(HeadRecordError::Short { .. }) => {}
                    Ok(_) => panic!("{what}: {at:?}: cut at {cut} of {} bytes, decoded", bytes.len()),
                    Err(e) => panic!("{what}: {at:?}: cut at {cut} of {} bytes, refused but not as short: {e}", bytes.len()),
                }
            }
        }
        *seen.by_kind.entry(kind(at)).or_default() += 1;
        if let HeadAt::Fn(k) = at {
            seen.statics += usize::from(store.statics.contains_key(&k));
        }
        seen.lives += usize::from(made_life);
    }
    seen
}

/// Every head of the small crate round-trips; every kind is met at least once; every strict
/// prefix of every record is refused as short, never decoded and never a panic. A writer that
/// drops a field (a variant's discriminant, a bound's arguments, the lifetime part), writes an
/// interner index instead of structure (the fresh-table bytes differ), or reads past its bytes,
/// fails here.
#[test]
fn every_head_round_trips_equal() {
    let seen = round_trip_all("the small crate", &[("src/main.rs".to_string(), TEXT.to_string())], EDITION, true);
    assert!(seen.refused.is_empty(), "refused: {:#?}", seen.refused);
    for (k, least) in [("fn", 12), ("adt", 5), ("ctor", 1), ("trait", 1), ("impl", 2), ("alias", 1), ("opaque", 1)] {
        assert!(seen.of(k) >= least, "at least {least} {k} heads: {}", seen.of(k));
    }
    assert!(seen.statics >= 1, "the static's head");
    assert!(seen.lives >= 2, "a fn's and an ADT's lifetime part");
    println!(
        "front2_head_records: {} heads round-trip equal (fn {}, adt {}, ctor {}, trait {}, impl {}, alias {}, opaque {}; statics {}; lifetime parts {}); every cut refused",
        seen.heads(),
        seen.of("fn"),
        seen.of("adt"),
        seen.of("ctor"),
        seen.of("trait"),
        seen.of("impl"),
        seen.of("alias"),
        seen.of("opaque"),
        seen.statics,
        seen.lives
    );
}

/// The planted difference: `pick_x` and `pick_y` have the same erased types (`&u8, &u8 ->
/// &u8`); only their regions differ (the Reference, "Lifetime elision" does not apply: both
/// name their output's lifetime, `'x` and `'y` are other parameters). `pick_x`'s head with its
/// own regions (head_record.rs form 22, D148) and with `pick_y`'s must have different record
/// digests and regions digests but ONE erased digest, and the same head encoded twice the same
/// digests. A record or a regions digest that leaves the regions out fails.
#[test]
fn one_lifetime_moves_the_digest() {
    let tc = toolchain();
    let files = [("/krate/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "krate", dir: "/krate/", root: "/krate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut key = |n: &str| {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let lib = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("`{n}`: no head: {e:?}"));
        checker_of(lib)
    };
    let (kx, ky) = (key("pick_x"), key("pick_y"));
    let regions_x = store.regions.made(kx).expect("`pick_x`: its regions filed with its head");
    let regions_y = store.regions.made(ky).expect("`pick_y`: its regions filed with its head");
    let (hx, hy) = (&store.fns[&kx], &store.fns[&ky]);
    assert!(hx.inputs[..] == hy.inputs[..] && hx.output == hy.output, "the two fns' erased types are equal: only a lifetime differs");
    assert!(!regions_x.regions().same(regions_y.regions()), "the two fns' regions differ");
    let Ok(HeadRef::Fn { key, head, preds, static_, misses, .. }) = from_store(&store, HeadAt::Fn(kx)) else {
        panic!("`pick_x`: a fn head")
    };
    // `pick_y`'s regions lie over `pick_x`'s signature: the erased types are one `PTy`.
    let own = HeadRef::Fn { key, head, preds, static_, regions: Some(regions_x), misses };
    let planted = HeadRef::Fn { key, head, preds, static_, regions: Some(regions_y), misses };
    let tys = store.tys.view();
    let d = |h| digest(&encode(h, tys, Alloc::global()).unwrap_or_else(|e| panic!("`pick_x`: not encoded: {e}")));
    let erased = |h| reader_digest(h, tys, Alloc::global()).unwrap_or_else(|e| panic!("`pick_x`: no erased digest: {e}"));
    let regions = |h| regions_digest(h, tys, Alloc::global()).unwrap_or_else(|e| panic!("`pick_x`: no regions digest: {e}"));
    assert_eq!(d(own), d(own), "one head, two encodings, one digest");
    assert_eq!(regions(own), regions(own), "one head, two regions digests");
    assert_ne!(d(own), d(planted), "a head whose regions differ has another record digest");
    assert_ne!(regions(own), regions(planted), "a head whose regions differ has another regions digest");
    assert_eq!(erased(own), erased(planted), "a lifetime-only difference moved the erased digest");
    println!("front2_head_records: two heads differing in one lifetime have different digests");
}

/// Every whole `front2/x-*` rung of a blaze edition (crates/blaze-cli/tests/ladder/front2/):
/// every head its crate's items make round-trips, as above (no cuts: the small crate has
/// them). A rung of edition 2024 builds without the toolchain's libraries (its own `no_core`
/// lang items), which this harness does not set up: it is counted and left out, by name on
/// standard error. Items the store refuses are not this test's subject (front2_rungs.rs
/// reports them): they are counted on standard error; a head refused by the RECORD fails.
#[test]
#[ignore = "a measurer's run, not a check: it solves std once per rung (83 rungs) and outlasts lanecheck's 900 s in a debug build; run it with --ignored"]
fn every_head_of_the_rungs_round_trips_equal() {
    let dir = rungs::ladder().join("front2");
    let mut rung_names: Vec<String> = std::fs::read_dir(&dir)
        .expect("the ladder's front2 directory")
        .map(|e| e.expect("a directory entry").file_name().into_string().expect("a UTF-8 rung name"))
        .filter(|n| (n.starts_with("x-") || n.starts_with("x0-")) && ["Cargo.toml", "expected", "src/main.rs"].iter().all(|f| dir.join(n).join(f).is_file()))
        .collect();
    rung_names.sort();
    let (mut heads, mut ran, mut left_out) = (0, 0, 0);
    for rung in &rung_names {
        let at = dir.join(rung);
        let manifest = std::fs::read_to_string(at.join("Cargo.toml")).expect("a rung's Cargo.toml");
        let line = manifest.lines().find_map(|l| l.trim().strip_prefix("edition")).expect("an edition line");
        let edition = line.trim().trim_start_matches('=').trim().trim_matches('"').to_string();
        if !edition.starts_with("blaze-") {
            eprintln!("front2_head_records: {rung} is of edition {edition}: left out");
            left_out += 1;
            continue;
        }
        let seen = round_trip_all(rung, &rungs::sources(&at), &edition, false);
        let by_record: Vec<&String> = seen.refused.iter().filter(|r| !r.contains("the store refused")).collect();
        assert!(by_record.is_empty(), "{rung}: heads the record refused: {by_record:#?}");
        if !seen.refused.is_empty() {
            eprintln!("front2_head_records: {rung}: {} items the store refused", seen.refused.len());
        }
        heads += seen.heads();
        ran += 1;
    }
    assert!(ran > 0, "at least one rung of a blaze edition");
    println!("front2_head_records: {heads} heads of {ran} rungs round-trip equal ({left_out} rungs not of a blaze edition left out)");
}
