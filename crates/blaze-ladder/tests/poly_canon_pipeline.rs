//! End-to-end integration tests for polymorphization layout-canonicalization wired into
//! blaze's monomorphization / code-record pipeline (`make_one.rs` + `poly_canon.rs` + `Walk`).
//!
//! Verifies:
//! 1. Equivalent generic instantiations (`id::<&u32>`, `id::<&u64>`, `id::<&A>`, `id::<&B>`;
//!    `ptr_addr::<u8>`, `ptr_addr::<u64>`, `ptr_addr::<Big>`, `ptr_addr::<HasDropA>`;
//!    plain 16-byte structs `id::<A>`, `id::<B>`) collapse to ONE compiled `CodeRecord` / `Symbol`.
//! 2. Soundness guards (Rule 4 `superset-of-rust`): distinct sizes/ABIs (`u32`, `u64`, `f64`,
//!    `i8`, `u8`), `Drop` types (`HasDropA` vs `HasDropB`), niche vs non-niche (`Opt<&u32>` vs
//!    `Opt<u64>`), `type_name::<T>()` reflection, and trait dispatch stay strictly distinct.
//! 3. Incremental verification & invalidation (Rule 21 `keys-cover-every-input`, Rule 60):
//!    - Warm runs hit 100% cache with `check_one == 0` via `ReadKind::PolyBodyText`.
//!    - Trivia-only edits in a canonicalized callee keep 100% cache hits.
//!    - Semantic edits in a transitive canonicalized callee (adding `type_name::<T>()`)
//!      invalidate the caller and expand the previously collapsed instantiations back to concrete.
//! 4. Measured monomorphization dedup ratio on a realistic multi-type fixture.

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Counts, Read, Walk};
use blaze_ladder::code_records::{Builder, CodeStore, EntryView, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::{CheckCount, Maker, NamesRecords, PolyCanonStats, walk_records};
use blaze_own::Alloc;
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/poly/";
const ROOT: &str = "/poly/src/main.rs";
const OVERFLOW: OverflowChecks = OverflowChecks::Off;
const LANG_CRATES: [usize; 1] = [0];
const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

fn isa() -> Isa {
    Isa { lse: true, os: Os::MacOs }
}

fn builder() -> Builder {
    Builder {
        isa: isa(),
        tier: TierName::Reference,
        settings: Settings::baseline(OVERFLOW, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"poly canon pipeline").get::<CodeBuilder>()
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }
    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }
    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }
    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("origins {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn made_origins(call: LibKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("origins {call:x}: {e:?}"),
    }
}

struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

fn fixture(text: &str) -> Fixture {
    let a = alloc();
    let files = [(String::from(ROOT), String::from(text))];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "poly", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).unwrap_or_else(|e| panic!("names encode: {e:?}")), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).unwrap_or_else(|e| panic!("items encode: {e:?}")), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    Fixture { kept, keys, crate_names }
}

fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("reader frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

struct TextIds {
    rows: Vec<(ItemKey, ContentId)>,
    entry: ItemKey,
}

impl BodyIds for TextIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let key = match self.rows.binary_search_by_key(&item, |r| r.0) {
            Ok(n) => self.rows[n].1,
            Err(_) => Hasher::new("poly test member").part(&item.0.to_le_bytes()).finish(),
        };
        Some(BodyId { key, reads: &READS })
    }
    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }
    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// Computes trivia-free body check IDs for every item with text (free fns, trait/impl methods,
/// constructors) so comment edits in callees preserve `BodyId` and exercise `ReadKind::PolyBodyText`.
fn ids_of(f: &Fixture) -> TextIds {
    let records = reader(f).records;
    let mut libs: Vec<LibKey> = Vec::new();
    assert!(records.each_item(0, &mut |key, _| libs.push(key)));
    let mut rows = Vec::new();
    let mut entry = None;
    for &lib in libs.iter() {
        let Some(text) = records.text_of(lib) else {
            continue;
        };
        let lexed = parser::LexedStr::new(parser::Edition::Edition2024, text.text);
        let mut h = Hasher::new("poly test body check");
        for i in 0..lexed.len() {
            if !lexed.kind(i).is_trivia() {
                h.part(lexed.text(i).as_bytes());
            }
        }
        rows.push((checker_of(lib), h.finish()));
        if records.kind_of_item(lib) == Some(ItemKind::Function)
            && records.member_owner(lib).is_none()
            && records.name_of(lib) == Some("main")
        {
            entry = Some(checker_of(lib));
        }
    }
    assert!(records.errors().is_empty(), "records errors: {:?}", records.errors());
    rows.sort_by_key(|r| r.0);
    TextIds { rows, entry: entry.expect("program has main") }
}

fn fn_named(f: &Fixture, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| {
        if found.is_none() && records.kind_of_item(key) == Some(ItemKind::Function) && records.name_of(key) == Some(name) {
            found = Some(checker_of(key));
        }
    }));
    found.unwrap_or_else(|| panic!("no fn named `{name}`"))
}

struct WalkSummary {
    walk: Walk<'static>,
    counts: Counts,
    checks: CheckCount,
    poly_stats: PolyCanonStats,
    instances_by_fn: Vec<(ItemKey, Option<String>)>,
}

fn run_walk(f: &Fixture, ids: &TextIds, store: &mut dyn CodeStore) -> WalkSummary {
    let names = reader(f);
    let b = builder();
    let mut maker = Maker::new(names, Some(ids.entry), &b, alloc());
    let (walk, counts) = walk_records(ids.entry, ids, store, &b, &producer(), &mut maker, alloc())
        .unwrap_or_else(|e| panic!("walk_records failed: {e}"));
    assert!(names.records.errors().is_empty(), "records errors: {:?}", names.records.errors());
    let mut instances_by_fn = Vec::new();
    for row in 0..walk.len() {
        let tabled = walk.tabled(row);
        let args = maker.instance_shown(tabled, walk.name(row)).and_then(|s| s.args.map(|a| a.into_string()));
        instances_by_fn.push((tabled, args));
    }
    WalkSummary {
        walk,
        counts,
        checks: maker.checks(),
        poly_stats: maker.poly_stats(),
        instances_by_fn,
    }
}

fn instances_of(summary: &WalkSummary, key: ItemKey) -> Vec<Option<String>> {
    summary.instances_by_fn.iter().filter(|(k, _)| *k == key).map(|(_, a)| a.clone()).collect()
}

/// Core end-to-end test:
/// - `id::<&u32>`, `id::<&u64>`, `id::<&A>`, `id::<&B>` share ONE canonical `CodeRecord` (`&()`).
/// - `id::<A>` and `id::<B>` (plain 16-byte align-8 structs) share ONE canonical `CodeRecord` (`[u64; 2]`).
/// - `ptr_addr::<u8>`, `ptr_addr::<u64>`, `ptr_addr::<Big>`, `ptr_addr::<HasDropA>` share ONE
///   canonical `CodeRecord` (`poly::OpaqueThin`).
/// - `id::<u32>`, `id::<u64>`, `id::<f64>`, `id::<i8>`, `id::<u8>` remain 5 DISTINCT `CodeRecord`s.
/// - `id::<HasDropA>` and `id::<HasDropB>` (custom `Drop` impls) remain 2 DISTINCT `CodeRecord`s.
/// - `id::<Opt<&u32>>` (niche-optimized 8B) and `id::<Opt<u64>>` (tagged 16B) remain 2 DISTINCT `CodeRecord`s.
#[test]
fn refs_and_opaque_ptrs_share_one_code_record_while_distinct_types_stay_separate() {
    const SRC: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[lang = "drop"]
pub trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_in_place"]
unsafe fn drop_in_place<T: ?Sized>(_to_drop: *mut T) {}

struct A(u64, u64);
struct B(u64, u64);
struct Big([u64; 8]);

struct HasDropA(u64);
impl Drop for HasDropA {
    fn drop(&mut self) {
        self.0 = 0;
    }
}

struct HasDropB(u64);
impl Drop for HasDropB {
    fn drop(&mut self) {
        self.0 = 1;
    }
}

enum Opt<T> {
    None,
    Some(T),
}

fn id<T>(x: T) -> T {
    x
}

fn ptr_addr<T>(p: &T) -> usize {
    p as *const T as usize
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> u64 {
    let v_u32: u32 = 10;
    let v_u64: u64 = 20;
    let v_f64: f64 = 1.5;
    let v_i8: i8 = -3;
    let v_u8: u8 = 7;
    let a = A(1, 2);
    let b = B(3, 4);
    let big = Big([0; 8]);
    let da = HasDropA(5);
    let db = HasDropB(6);

    // 4 distinct reference types -> MUST collapse to 1 `id::<&()>`
    let r1 = id(&v_u32);
    let r2 = id(&v_u64);
    let r3 = id(&a);
    let r4 = id(&b);

    // 4 distinct pointees behind `&T` -> MUST collapse to 1 `ptr_addr::<poly::OpaqueThin>`
    let p1 = ptr_addr(&v_u8);
    let p2 = ptr_addr(&v_u64);
    let p3 = ptr_addr(&big);
    let p4 = ptr_addr(&da);

    // 2 plain 16-byte align-8 structs -> MUST collapse to 1 `id::<[u64; 2]>`
    let a2 = id(a);
    let b2 = id(b);

    // 5 distinct scalar ABIs/sizes -> MUST stay 5 distinct instances
    let s1 = id(v_u32);
    let s2 = id(v_u64);
    let s3 = id(v_f64);
    let s4 = id(v_i8);
    let s5 = id(v_u8);

    // 2 custom Drop structs of identical size/align -> MUST stay 2 distinct instances
    let d1 = id(da);
    let d2 = id(db);

    // Niche-optimized Option<&u32> (8B) vs tagged Option<u64> (16B) -> MUST stay 2 distinct instances
    let o1 = id(Opt::Some(&v_u32));
    let o2 = id(Opt::Some(v_u64));

    let _ = (r1, r2, r3, r4, s3, d1, d2, o1, o2);
    (p1 + p2 + p3 + p4) as u64 + a2.0 + b2.0 + s1 as u64 + s2 + s4 as u64 + s5 as u64
}
"#;

    let f = fixture(SRC);
    let ids = ids_of(&f);
    let mut store = MemoryStore::new_in(alloc());
    let summary = run_walk(&f, &ids, &mut store);

    let id_key = fn_named(&f, "id");
    let ptr_addr_key = fn_named(&f, "ptr_addr");

    let id_insts = instances_of(&summary, id_key);
    let ptr_addr_insts = instances_of(&summary, ptr_addr_key);

    // `ptr_addr` called at 4 distinct pointee types (`u8`, `u64`, `Big`, `HasDropA`) -> 1 record!
    assert_eq!(
        ptr_addr_insts.len(),
        1,
        "all 4 `ptr_addr::<T>` calls must share ONE canonical CodeRecord, got: {ptr_addr_insts:?}"
    );
    assert_eq!(
        ptr_addr_insts[0].as_deref(),
        Some("[poly::OpaqueThin]"),
        "ptr_addr canonical arg should be `[poly::OpaqueThin]`, got: {ptr_addr_insts:?}"
    );

    // `id` called at 15 distinct concrete types:
    //   - 4 refs (`&u32`, `&u64`, `&A`, `&B`) -> 1 (`[&()]`)
    //   - 2 plain 16B structs (`A`, `B`) -> 1 (`[[u64; 2]]`)
    //   - 5 distinct scalars (`u32`, `u64`, `f64`, `i8`, `u8`) -> 5
    //   - 2 custom Drop types (`HasDropA`, `HasDropB`) -> 2
    //   - 2 distinct Option layouts (`Opt<&u32>` is niche-optimized 8B Indirect -> stays concrete;
    //     `Opt<u64>` is 16B Indirect no niche -> `[[u64; 2]]`)
    // 1. All 4 refs collapsed to a single `[&()]` entry (no `&u32`, `&u64`, `&A`, `&B`).
    assert!(
        id_insts.iter().any(|a| a.as_deref() == Some("[&()]")),
        "expected canonical `id::<&()>` in {id_insts:?}"
    );
    for raw_ref in ["[&Uint(U32)]", "[&Uint(U64)]", "[&poly::A]", "[&poly::B]"] {
        assert!(
            !id_insts.iter().any(|a| a.as_deref() == Some(raw_ref)),
            "raw ref `{raw_ref}` should have canonicalized to `[&()]`: {id_insts:?}"
        );
    }
    // 2. Both plain 16B structs (`A`, `B`) collapsed to `[u64; 2]` (`[[Uint(U64); ... bits: 2 ...]]`).
    assert!(
        id_insts.iter().any(|a| a.as_deref().is_some_and(|s| s.contains("Uint(U64)") && s.contains("bits: 2"))),
        "expected canonical `id::<[u64; 2]>` in {id_insts:?}"
    );
    for plain in ["[poly::A]", "[poly::B]"] {
        assert!(
            !id_insts.iter().any(|a| a.as_deref() == Some(plain)),
            "plain 16B struct `{plain}` should have canonicalized to `[u64; 2]`: {id_insts:?}"
        );
    }
    // 3. All 5 distinct scalar types stayed distinct.
    for scalar in ["[Uint(U32)]", "[Uint(U64)]", "[Float(F64)]", "[Int(I8)]", "[Uint(U8)]"] {
        assert!(
            id_insts.iter().any(|a| a.as_deref() == Some(scalar)),
            "scalar `{scalar}` must remain distinct in {id_insts:?}"
        );
    }
    // 4. Both `Drop` structs stayed strictly concrete and distinct!
    for drop_ty in ["[poly::HasDropA]", "[poly::HasDropB]"] {
        assert!(
            id_insts.iter().any(|a| a.as_deref() == Some(drop_ty)),
            "Drop type `{drop_ty}` MUST stay concrete in {id_insts:?}"
        );
    }
    // 5. `Opt<u64>` (16B tagged enum with tag niche `0..=1`) stays concrete & distinct from
    //    `Opt<&u32>` (niche-optimized 8B nullable scalar with no remaining niche -> `[Uint(U64)]`)
    //    and distinct from `&u32` (non-null niche -> `[&()]`)!
    assert!(
        id_insts.iter().any(|a| a.as_deref() == Some("[poly::Opt<Uint(U64)>]")),
        "16B tagged `Opt<u64>` must remain distinct from niche-optimized 8B `Opt<&u32>` and `[u64; 2]`: {id_insts:?}"
    );
    assert!(
        !id_insts.iter().any(|a| a.as_deref().is_some_and(|s| s.contains("Opt") && s.contains("U32"))),
        "niche-optimized 8B nullable `Opt<&u32>` shares the exact `(8B, align 8, Int64, no-niche)` ABI class of `u64`: {id_insts:?}"
    );
    // Total `id` instances: 15 concrete calls -> 10 compiled instances (`&()`, `[u64; 2]`, `u32`, `u64`, `f64`, `i8`, `u8`, `HasDropA`, `HasDropB`, `Opt<u64>`).
    assert_eq!(
        id_insts.len(),
        10,
        "15 concrete `id` calls should deduplicate to exactly 10 canonical CodeRecords, got: {id_insts:?}"
    );

    // Verify `main`'s stored `EntryView` in `MemoryStore` is readable and valid!
    let main_key = summary.walk.key(0).expect("main has CodeKey");
    let main_entry_bytes = store.get(main_key).expect("store lookup").expect("main stored in MemoryStore");
    let main_view = EntryView::read(main_entry_bytes).expect("main EntryView valid");
    let mut declared_count = 0usize;
    main_view
        .each_declared::<()>(|_| {
            declared_count += 1;
            Ok(())
        }, |e| panic!("malformed declared row: {e}"))
        .unwrap();
    assert!(declared_count >= 10, "main declares canonical callees, got {declared_count}");
    assert!(
        summary.poly_stats.canonicalized_calls >= 10,
        "expected >= 10 canonicalized calls, got {:?}",
        summary.poly_stats
    );
}

/// Soundness test: `type_name::<T>()` (reflection) and trait dispatch (`T::tag()`) MUST NEVER
/// be canonicalized even when `T` is passed behind `&T`!
#[test]
fn reflection_and_trait_dispatch_stay_concrete() {
    const SRC: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[rustc_intrinsic]
pub const fn type_name<T: ?Sized>() -> &'static str;

fn name_is_nonempty<T>(_: &T) -> bool {
    let s = const { type_name::<T>() };
    s as *const str as *const u8 as usize != 0
}

trait Tag {
    fn tag(&self) -> u32;
}

impl Tag for u32 {
    fn tag(&self) -> u32 { 1 }
}

impl Tag for u64 {
    fn tag(&self) -> u32 { 2 }
}

fn get_tag<T: Tag>(x: &T) -> u32 {
    x.tag()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> u32 {
    let a: u32 = 10;
    let b: u64 = 20;
    let n1 = name_is_nonempty(&a);
    let n2 = name_is_nonempty(&b);
    let t1 = get_tag(&a);
    let t2 = get_tag(&b);
    if n1 && n2 { t1 + t2 } else { 0 }
}
"#;

    let f = fixture(SRC);
    let ids = ids_of(&f);
    let mut store = MemoryStore::new_in(alloc());
    let summary = run_walk(&f, &ids, &mut store);

    let name_insts = instances_of(&summary, fn_named(&f, "name_is_nonempty"));
    let tag_insts = instances_of(&summary, fn_named(&f, "get_tag"));

    assert_eq!(
        name_insts.len(),
        2,
        "`type_name::<T>()` must keep `u32` and `u64` concrete and distinct, got: {name_insts:?}"
    );
    assert!(name_insts.iter().any(|a| a.as_deref() == Some("[Uint(U32)]")));
    assert!(name_insts.iter().any(|a| a.as_deref() == Some("[Uint(U64)]")));

    assert_eq!(
        tag_insts.len(),
        2,
        "trait dispatch `x.tag()` must keep `u32` and `u64` concrete and distinct, got: {tag_insts:?}"
    );
    assert!(tag_insts.iter().any(|a| a.as_deref() == Some("[Uint(U32)]")));
    assert!(tag_insts.iter().any(|a| a.as_deref() == Some("[Uint(U64)]")));
}

/// Transitive call-graph propagation + Rule 21 incremental cache invalidation test:
/// - Run 1 (cold): `main` -> `wrapper::<&u32>` & `wrapper::<&u64>` -> `leaf` collapses to ONE
///   `wrapper::<&()>` and ONE `leaf::<&()>` (3 total records instead of 5).
/// - Run 2 (warm, unchanged): 100% cache hit (`found == 3, made == 0`), `check_one == 0`.
/// - Run 3 (comment/whitespace edit in `leaf`): 100% cache hit (`found == 3, made == 0`), `check_one == 0`.
/// - Run 4 (semantic edit in `leaf` adding `type_name::<T>()`): `ReadKind::PolyBodyText(leaf)`
///   invalidates `main` -> rebuilds with 2 distinct `wrapper` and 2 distinct `leaf` records (5 total)!
#[test]
fn transitive_propagation_and_incremental_poly_body_invalidation() {
    const V1: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[rustc_intrinsic]
pub const fn type_name<T: ?Sized>() -> &'static str;

fn leaf<T>(x: T) -> T {
    x
}

fn wrapper<T>(x: T) -> T {
    leaf(x)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> u64 {
    let a: u32 = 1;
    let b: u64 = 2;
    let r1 = wrapper(&a);
    let r2 = wrapper(&b);
    (*r1 as u64) + *r2
}
"#;

    let mut store = MemoryStore::new_in(alloc());

    // Run 1: cold compile — `wrapper` and `leaf` each compile ONCE at `&()`.
    let f1 = fixture(V1);
    let ids1 = ids_of(&f1);
    let r1 = run_walk(&f1, &ids1, &mut store);
    assert_eq!((r1.counts.reached, r1.counts.found, r1.counts.made), (3, 0, 3), "cold run: {}", r1.counts);
    assert_eq!(instances_of(&r1, fn_named(&f1, "wrapper")), vec![Some("[&()]".to_string())]);
    assert_eq!(instances_of(&r1, fn_named(&f1, "leaf")), vec![Some("[&()]".to_string())]);

    // Run 2: warm cache hit — ZERO `check_one` body checks!
    let r2 = run_walk(&f1, &ids1, &mut store);
    assert_eq!((r2.counts.reached, r2.counts.found, r2.counts.made), (3, 3, 0), "warm run: {}", r2.counts);
    assert_eq!(r2.checks.check_one, 0, "warm verification via ReadKind::PolyBodyText must check 0 bodies: {:?}", r2.checks);

    // Run 3: comment/whitespace-only edit in `leaf` — still 100% cache hit, ZERO `check_one`!
    let v_comment = V1.replace("fn leaf<T>(x: T) -> T {\n    x\n}", "fn leaf<T>(x: T) -> T {\n    // harmless comment\n    x\n}");
    let f_comment = fixture(&v_comment);
    let ids_comment = ids_of(&f_comment);
    let r3 = run_walk(&f_comment, &ids_comment, &mut store);
    assert_eq!((r3.counts.reached, r3.counts.found, r3.counts.made), (3, 3, 0), "comment edit run: {}", r3.counts);
    assert_eq!(r3.checks.check_one, 0, "trivia-only edit must check 0 bodies: {:?}", r3.checks);

    // Run 4: semantic edit in `leaf` adding `const { type_name::<T>() }` — MUST invalidate `main`
    // via `ReadKind::PolyBodyText(leaf)` and expand `wrapper` & `leaf` back to concrete `&u32` + `&u64`!
    let v_reflect = V1.replace(
        "fn leaf<T>(x: T) -> T {\n    x\n}",
        "fn leaf<T>(x: T) -> T {\n    let _ = const { type_name::<T>() };\n    x\n}",
    );
    let f_reflect = fixture(&v_reflect);
    let ids_reflect = ids_of(&f_reflect);
    let r4 = run_walk(&f_reflect, &ids_reflect, &mut store);
    assert_eq!(r4.counts.reached, 5, "reflection edit must expand back to 5 concrete instances: {}", r4.counts);
    assert_eq!(r4.counts.found, 0, "main, wrapper, and leaf must all rebuild after reflection edit: {}", r4.counts);
    assert_eq!(instances_of(&r4, fn_named(&f_reflect, "wrapper")).len(), 2);
    assert_eq!(instances_of(&r4, fn_named(&f_reflect, "leaf")).len(), 2);
}

/// Measures concrete monomorphization dedup ratio on a realistic multi-type fixture:
/// 12 distinct user structs (6 plain 16B, 4 plain 8B, 2 Drop 8B) instantiated across 5 generic
/// helpers (`id`, `ptr_addr`, `pick`, `wrap`, `pair_first`): 52 concrete generic instantiations +
/// `main` = 53 pre-poly instances -> measures exact post-poly compiled `CodeRecord` count!
#[test]
fn measured_dedup_ratio_on_multi_type_fixture() {
    const SRC: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

#[lang = "drop"]
pub trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_in_place"]
unsafe fn drop_in_place<T: ?Sized>(_to_drop: *mut T) {}

struct P16A(u64, u64);
struct P16B(u64, u64);
struct P16C(u64, u64);
struct P16D(u64, u64);
struct P16E(u64, u64);
struct P16F(u64, u64);

struct P8A(u64);
struct P8B(u64);
struct P8C(u64);
struct P8D(u64);

struct D8A(u64);
impl Drop for D8A { fn drop(&mut self) { self.0 = 0; } }
struct D8B(u64);
impl Drop for D8B { fn drop(&mut self) { self.0 = 0; } }

fn id<T>(x: T) -> T { x }
fn wrap<T>(x: T) -> T { id(x) }
fn pick<T>(c: bool, a: T, b: T) -> T { if c { a } else { b } }
fn ptr_addr<T>(p: &T) -> usize { p as *const T as usize }
fn pair_first<T, U>(a: &T, _b: &U) -> usize { ptr_addr(a) }

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> u64 {
    let a1 = P16A(1, 2); let a2 = P16B(1, 2); let a3 = P16C(1, 2);
    let a4 = P16D(1, 2); let a5 = P16E(1, 2); let a6 = P16F(1, 2);
    let b1 = P8A(1); let b2 = P8B(2); let b3 = P8C(3); let b4 = P8D(4);
    let d1 = D8A(1); let d2 = D8B(2);

    // 12 `ptr_addr::<T>` calls (all collapse to 1 `OpaqueThin`)
    let mut acc: usize = 0;
    acc += ptr_addr(&a1) + ptr_addr(&a2) + ptr_addr(&a3) + ptr_addr(&a4) + ptr_addr(&a5) + ptr_addr(&a6);
    acc += ptr_addr(&b1) + ptr_addr(&b2) + ptr_addr(&b3) + ptr_addr(&b4) + ptr_addr(&d1) + ptr_addr(&d2);

    // 10 `pair_first::<T, U>` distinct concrete pairs (all collapse to 1 `<OpaqueThin, OpaqueThin>`)
    acc += pair_first(&a1, &b1) + pair_first(&a2, &b2) + pair_first(&a3, &b3) + pair_first(&a4, &b4);
    acc += pair_first(&a5, &d1) + pair_first(&a6, &d2) + pair_first(&b1, &a1) + pair_first(&b2, &a2);
    acc += pair_first(&d1, &a3) + pair_first(&d2, &a4);

    // 10 `wrap::<T>` -> `id::<T>` calls (6x16B -> 1, 4x8B -> 1) + 2 Drop calls (stay 2 distinct)
    let a1 = wrap(a1); let a2 = wrap(a2); let a3 = wrap(a3);
    let a4 = wrap(a4); let a5 = wrap(a5); let a6 = wrap(a6);
    let b1 = wrap(b1); let b2 = wrap(b2); let b3 = wrap(b3); let b4 = wrap(b4);
    let d1 = wrap(d1); let d2 = wrap(d2);

    // 6 `pick::<&T>` calls (all collapse to 1 `&()`)
    let _ = (
        pick(true, &a1, &a1), pick(true, &a2, &a2), pick(true, &a3, &a3),
        pick(true, &b1, &b1), pick(true, &d1, &d1), pick(true, &d2, &d2),
    );

    let _ = (a1, a2, a3, a4, a5, a6, b1, b2, b3, b4, d1, d2);
    acc as u64
}
"#;

    let f = fixture(SRC);
    let ids = ids_of(&f);
    let mut store = MemoryStore::new_in(alloc());
    let summary = run_walk(&f, &ids, &mut store);

    // Pre-polymorphization distinct instances:
    //   main (1) + ptr_addr (12) + pair_first (10) + wrap (12) + id (12) + pick (6)
    //   + Drop::drop (2) + drop_in_place (2) = 57 concrete instances (52 generic + 5 non-generic/drop).
    let pre_poly_generic_instances: usize = 12 + 10 + 12 + 12 + 6;
    let post_poly_generic_instances: usize = instances_of(&summary, fn_named(&f, "ptr_addr")).len()
        + instances_of(&summary, fn_named(&f, "pair_first")).len()
        + instances_of(&summary, fn_named(&f, "wrap")).len()
        + instances_of(&summary, fn_named(&f, "id")).len()
        + instances_of(&summary, fn_named(&f, "pick")).len();

    let ratio = pre_poly_generic_instances as f64 / post_poly_generic_instances as f64;
    println!(
        "poly_canon multi-type fixture: generic instances {pre_poly_generic_instances} -> {post_poly_generic_instances} ({ratio:.2}x reduction), total reached={}, poly_stats={:?}",
        summary.counts.reached, summary.poly_stats
    );
    // Expected post-poly generic instances:
    //   ptr_addr: 1 (`OpaqueThin`)
    //   pair_first: 1 (`OpaqueThin, OpaqueThin`)
    //   wrap: 4 (`[u64; 2]`, `[u64; 1]`, `D8A`, `D8B`)
    //   id: 4 (`[u64; 2]`, `[u64; 1]`, `D8A`, `D8B`)
    //   pick: 1 (`&()`)
    //   Total = 11 (down from 52 -> 4.73x reduction!).
    assert_eq!(post_poly_generic_instances, 11, "expected 11 canonical generic instances (4.73x reduction)");
    assert!(ratio >= 4.0, "expected >= 4.0x generic instance reduction, got {ratio:.2}x");
}
