//! Proves that per-function build + LIR + SOTA emit inside `make_one.rs` routes transient
//! allocations through a per-worker resettable `blaze_mem::Frame` instead of `Global` heap
//! `malloc`/`free` (Rule 11 / Rule 16 / Rule 61).

use std::collections::HashMap;

use blaze_emit::{Isa, Os};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Fixture as Grant, Make, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::make_one::{Maker, NamesRecords, walk_records};
use blaze_ladder::run::{OverflowChecks, RunError};
use blaze_ladder::symbols::encode_instance;
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::{ItemKey, List, PNode, PortableTys, UintTy};

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

const DIR: &str = "/per-fn-bump/";
const ROOT: &str = "/per-fn-bump/src/main.rs";
const LANG_CRATES: [usize; 1] = [0];

const PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "legacy_receiver"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i32 {}
impl Copy for i64 {}
impl Copy for bool {}

struct Pair<A, B> {
    a: A,
    b: B,
}

enum Step {
    Add(u64),
    Mul(u64),
    Xor(u64),
    Shift(u32),
}

fn eval_step(acc: u64, step: Step) -> u64 {
    match step {
        Step::Add(x) => acc + x,
        Step::Mul(x) => acc * x,
        Step::Xor(x) => acc ^ x,
        Step::Shift(s) => acc << s,
    }
}

fn mix_loop<T: Copy>(seed: u64, n: u64, tag: T) -> Pair<u64, T> {
    let mut acc = seed ^ 0x9e37_79b9_7f4a_7c15u64;
    let mut i = 0u64;
    while i < n {
        let s = match i & 3u64 {
            0 => Step::Add(i + 7u64),
            1 => Step::Mul((i | 1u64) + 3u64),
            2 => Step::Xor(acc >> 7u32),
            _ => Step::Shift((i & 15u64) as u32),
        };
        acc = eval_step(acc, s);
        if (acc & 1u64) == 0u64 {
            acc = acc ^ (i * 17u64);
        } else {
            acc = acc + (i ^ 31u64);
        }
        i = i + 1u64;
    }
    Pair { a: acc, b: tag }
}

struct Tag0;
struct Tag1;
struct Tag2;
struct Tag3;
struct Tag4;
struct Tag5;
struct Tag6;
struct Tag7;
struct Tag8;
struct Tag9;
struct Tag10;
struct Tag11;

impl Copy for Tag0 {}
impl Copy for Tag1 {}
impl Copy for Tag2 {}
impl Copy for Tag3 {}
impl Copy for Tag4 {}
impl Copy for Tag5 {}
impl Copy for Tag6 {}
impl Copy for Tag7 {}
impl Copy for Tag8 {}
impl Copy for Tag9 {}
impl Copy for Tag10 {}
impl Copy for Tag11 {}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    let p0 = mix_loop(1u64, 8u64, Tag0);
    let p1 = mix_loop(p0.a, 8u64, Tag1);
    let p2 = mix_loop(p1.a, 8u64, Tag2);
    let p3 = mix_loop(p2.a, 8u64, Tag3);
    let p4 = mix_loop(p3.a, 8u64, Tag4);
    let p5 = mix_loop(p4.a, 8u64, Tag5);
    let p6 = mix_loop(p5.a, 8u64, Tag6);
    let p7 = mix_loop(p6.a, 8u64, Tag7);
    let p8 = mix_loop(p7.a, 8u64, Tag8);
    let p9 = mix_loop(p8.a, 8u64, Tag9);
    let p10 = mix_loop(p9.a, 8u64, Tag10);
    let p11 = mix_loop(p10.a, 8u64, Tag11);
    (p11.a & 127u64) as i32
}
"#;

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
        Some(LoadedExpansion { text, origins: None })
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_build::Panic::Abort),
        toolchain: ToolchainId([9; 32]),
        producer: Producers::for_tests(b"per-fn-bump").get::<CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024"),
    }
}

struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

fn make_fixture() -> Fixture {
    let files = [(String::from(ROOT), String::from(PROGRAM))];
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024");
    let user = UserCrate { name: "per_fn_bump", dir: DIR, root: ROOT, files: &files, edition, panic: blaze_build::Panic::Abort };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve");
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).expect("names"), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).expect("items"), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(HashMap::new());
        kept.origins.push(HashMap::new());
    }
    Fixture { kept, keys, crate_names }
}

fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn item_named<'a>(records: &'a Records<'a>, kind: ItemKind, name: &str) -> ItemKey {
    let mut found = None;
    assert!(records.each_item(0, &mut |key, _| {
        if found.is_none() && records.kind_of_item(key) == Some(kind) && records.name_of(key) == Some(name) {
            found = Some(checker_of(key));
        }
    }));
    found.unwrap_or_else(|| panic!("no item `{name}` of kind {kind:?}"))
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("per-fn-bump").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }
    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }
    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

struct CaptureGrant<'n> {
    inner: Maker<'n>,
    grant: Option<Grant>,
}

impl Make for CaptureGrant<'_> {
    type Error = RunError;
    type Grant = Grant;
    fn make(&mut self, grant: &Grant, tabled: ItemKey, name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.grant = Some(*grant);
        self.inner.make(grant, tabled, name, entry)
    }
    fn make_static(&mut self, grant: &Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.grant = Some(*grant);
        self.inner.make_static(grant, item, entry)
    }
    fn make_thread_local(&mut self, grant: &Grant, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.grant = Some(*grant);
        self.inner.make_thread_local(grant, item, entry)
    }
    fn verify(&mut self, tabled: ItemKey, name: &[u8], entry: &[u8]) -> Result<Option<blaze_ladder::code_records::reads::Verdict>, RunError> {
        self.inner.verify(tabled, name, entry)
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"per-fn-bump").get::<CodeBuilder>()
}

#[test]
fn per_fn_make_uses_bump_frame_not_global_heap() {
    let f = make_fixture();
    let names = reader(&f);
    let main_key = item_named(names.records, ItemKind::Function, "main");
    let mix_key = item_named(names.records, ItemKind::Function, "mix_loop");
    let tag_names = ["Tag0", "Tag1", "Tag2", "Tag3", "Tag4", "Tag5", "Tag6", "Tag7", "Tag8", "Tag9", "Tag10", "Tag11"];
    let tag_keys: Vec<ItemKey> = tag_names.iter().map(|n| item_named(names.records, ItemKind::Struct, n)).collect();

    let a = alloc();
    let b = builder();
    let mut store = MemoryStore::new_in(a);
    let mut cap = CaptureGrant { inner: Maker::new(names, Some(main_key), &b, a), grant: None };
    // Full warmup walk constructs LazyProgram, checks `main`/`mix_loop`/`eval_step`, and captures a `Grant`.
    let _ = walk_records(main_key, &EveryItem, &mut store, &b, &producer(), &mut cap, a).expect("warmup walk");
    let grant = cap.grant.expect("walk captured grant");
    let mut maker = cap.inner;

    // Pre-encode instance names for `mix_loop::<Tag0..Tag11>` + `u8/u16/u32/u64` outside measurement.
    let mut ground = PortableTys::new_in(a);
    let mut encoded_names: Vec<Vec<u8>> = Vec::new();
    for &tag in &tag_keys {
        let ty = ground.intern(PNode::Adt(tag, List::EMPTY)).expect("intern tag");
        let list = ground.intern_list(&[ty]).expect("intern list");
        let mut buf: AVec<'_, u8> = blaze_own::Vec::new_in(a);
        encode_instance(mix_key, list, ground.view(), a, &mut buf).expect("encode");
        encoded_names.push(buf.as_slice().to_vec());
    }
    for uint in [UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64] {
        let ty = ground.intern(PNode::Uint(uint)).expect("intern uint");
        let list = ground.intern_list(&[ty]).expect("intern list");
        let mut buf: AVec<'_, u8> = blaze_own::Vec::new_in(a);
        encode_instance(mix_key, list, ground.view(), a, &mut buf).expect("encode");
        encoded_names.push(buf.as_slice().to_vec());
    }

    let mut out: AVec<'_, u8> = blaze_own::Vec::with_capacity_in(64 << 10, a);
    // Warm up one more call so reusable per-worker buffers are at steady-state capacity.
    maker.make(&grant, mix_key, &encoded_names[0], &mut out).expect("warmup make");
    assert!(!out.is_empty());

    // Measure 15 subsequent `maker.make` calls (pure per-fn build + LIR opt + SOTA emit + record seal).
    let measured_fns = encoded_names.len() - 1;
    let mut first_entry: Vec<u8> = Vec::new();
    let (_, used) = blaze_alloc_count::measure(|| {
        for (idx, inst_name) in encoded_names[1..].iter().enumerate() {
            out.clear();
            maker.make(&grant, mix_key, inst_name, &mut out).expect("measured make");
            assert!(!out.is_empty());
            if idx == 0 {
                first_entry = out.as_slice().to_vec();
            }
        }
    });

    let allocs_per_fn = used.allocations as f64 / measured_fns as f64;
    let bytes_per_fn = used.bytes as f64 / measured_fns as f64;
    eprintln!(
        "per-fn make ({measured_fns} fns): total={} allocs ({} B), per-fn={allocs_per_fn:.1} allocs ({bytes_per_fn:.0} B)",
        used.allocations, used.bytes
    );

    // Re-making an already-seen instance after multiple O(1) `fn_frame.reset()` cycles must
    // produce byte-identical record output and stay far under the >95% reduction budget.
    let (_, one) = blaze_alloc_count::measure(|| {
        out.clear();
        maker.make(&grant, mix_key, &encoded_names[1], &mut out).expect("repeat make");
    });
    assert_eq!(out.as_slice(), first_entry.as_slice(), "O(1) fn_frame.reset() must preserve byte-identical record output");
    assert!(
        one.allocations < 200 && allocs_per_fn < 200.0 && bytes_per_fn < 20_000.0,
        "expected >95% reduction from 6,858.7 allocs/fn (1,038,540 B/fn) baseline, got {allocs_per_fn:.1} allocs/fn ({bytes_per_fn:.0} B/fn); repeat single fn: {} allocs ({} B)",
        one.allocations,
        one.bytes
    );
}
