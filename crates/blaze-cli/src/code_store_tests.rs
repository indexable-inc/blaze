//! The code index kept across commands, on blaze-core's simulation (rules 26, 43, 64): an entry
//! put by one process is read in place by the next; a second walk of the same program makes
//! nothing (the emitter's calls counted by `RecordRun::made`), and the control that keeps
//! today's `MemoryStore` makes everything again (rule 23: the cause removed, the effect gone);
//! every input a key covers, moved alone, is a miss; a key planted without one of its inputs is
//! caught by the store's two-values refusal. The oracle is the model `entry(n)` under its key
//! (rule 8), never another store's answer.


use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, Namespace, Settings as StoreSettings, Tag};
use blaze_core::FileName;
use blaze_core::sim::{Faults, Sim};
use blaze_emit::{Isa, Os};
use blaze_ids::ContentId;
use blaze_ids::producer::{CodeBuilder, Producers};
use blaze_ladder::body_ids::{BodyId, Read, record_key};
use blaze_ladder::code_records::{
    Builder, CodeClass, CodeKey, CodeStore, KeyParts, KeyPlant, MemoryStore, PanicStrategy, RecordRun, Settings, StoreError, body_digest, code_key, code_key_planted, getter_parts, make_getter_entry,
};
use blaze_ladder::front2::body_reads::ReadsDigest;
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::symbols::fn_symbol_of_name;
use blaze_ladder::code_records::TierName;
use blaze_toolchain::ToolchainId;

use super::HomeCodeStore;
use blaze_keyed::kind::ReservedTag;
use crate::store::plain_place::{Facts, StoreAlloc, after_answer, config, open, test_cx as root_cx};

/// Process `n` of one boot over `sim`: the command's store as `open_for_command` opens it.
fn process(sim: Sim, n: u64) -> Lent<Sim, StoreAlloc> {
    let alloc = blaze_own::testing::heap().alloc();
    let cfg = config(FileName::new("store").expect("a name"), &StoreSettings::default(), Facts { boot: Some(1), nonce: 7000 + n }, alloc);
    open(sim, cfg, alloc).expect("the store opens")
}

fn back(lent: Lent<Sim, StoreAlloc>) -> Sim {
    lent.into_store().into_driver()
}

/// What the lazy parent builds for (run_parent.rs `builder_for`), with a fixed toolchain id.
fn builder() -> Builder {
    Builder {
        isa: Isa { os: Os::MacOs, lse: true },
        tier: TierName::Reference,
        settings: Settings { overflow_checks: OverflowChecks::On, debug_assertions: false, panic: PanicStrategy::Unwind },
        toolchain: ToolchainId([7; 32]),
        producer: Producers::for_tests(b"code store test builder").get::<CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

/// The text of model fn `n`.
fn text(n: u32) -> std::string::String {
    std::format!("fn f{n}() -> u32 {{ {n} }}")
}

/// The digest of what model fn `n`'s check read, version `v` (a callee's signature that moved
/// is another version).
fn reads(n: u32, v: u8) -> ReadsDigest {
    let mut bytes = [0u8; 5];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    bytes[4] = v;
    ReadsDigest(ContentId::of("code store test reads", &bytes))
}

/// Model instance `n`'s own parts of its key.
fn parts(n: u32) -> KeyParts {
    KeyParts { symbol: fn_symbol_of_name(&n.to_le_bytes()), body: body_digest(&text(n), Some(reads(n, 0))), ground: ContentId::of("code store test ground", &n.to_le_bytes()) }
}

/// Instance `n`'s one entry (rule 21): 1 to 300 bytes.
fn entry(n: u32) -> std::vec::Vec<u8> {
    (0..1 + (n as usize * 41) % 300).map(|i| (n as usize).wrapping_mul(151).wrapping_add(i) as u8).collect()
}

/// An entry put by one process is read in place by the next, through the commit after the
/// answer; a key never put is a miss; other bytes under a committed key are `TwoValues`, equal
/// bytes no write; a record of another tag under a code key is refused, never read as code.
/// Prints "code store: 64 entries read in place in the next process; other bytes refused; another tag refused".
#[test]
fn entries_reach_the_next_process_and_are_read_in_place() {
    let lent = process(Sim::new(1, Faults::default()), 1);
    let mut store = HomeCodeStore::new(&lent);
    for n in 0..64 {
        store.put(CodeClass::Program,code_key(&parts(n), &builder()), &entry(n)).expect("a put");
        assert_eq!(store.get(code_key(&parts(n), &builder())).expect("a get"), Some(&entry(n)[..]), "entry {n} from the arena");
    }
    drop(store);
    after_answer(&root_cx(), &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 2);
    let mut store = HomeCodeStore::new(&lent);
    for n in 0..64 {
        assert_eq!(store.get(code_key(&parts(n), &builder())).expect("a get"), Some(&entry(n)[..]), "entry {n} in the next process");
    }
    let counts = lent.counts();
    assert_eq!((counts.from_segments, counts.from_puts), (64, 0), "{counts:?}");
    assert_eq!(store.get(code_key(&parts(5000), &builder())).expect("a get"), None);
    let key = code_key(&parts(7), &builder());
    match store.put(CodeClass::Program,key, &entry(8)) {
        Err(StoreError::TwoValues { key: refused }) => assert_eq!(refused, key),
        other => panic!("other bytes under a committed code key were not refused: {other:?}"),
    }
    store.put(CodeClass::Program,key, &entry(7)).expect("equal bytes under a held key are no write");
    // A record of the drive's (or any other) tag under a key the code index asks.
    let other = CodeKey(ContentId::of("code store test other tag", b"x"));
    lent.put(&CasKey(other.0), Namespace::LOCAL, Tag(1), b"not a code entry").expect("a put of another tag");
    assert_ne!(Tag(1), ReservedTag::Code.tag());
    match store.get(other) {
        Err(StoreError::Failed { key, .. }) => assert_eq!(key, other),
        found => panic!("a record of another tag was read as a code entry: {found:?}"),
    }
    std::println!("code store: 64 entries read in place in the next process; other bytes refused; another tag refused");
}

/// `RecordRun`'s walk over the two program parts made from no text (the thread-local getter and
/// ender, each emitted by `make_entry` through `emit_record_measured`): the second process over
/// the home's store makes nothing and finds both, and what it finds equals a fresh emission
/// (rule 25). The control: a `MemoryStore` made fresh in each process,
/// which makes both again.
/// Prints "code store walk: first made 2 found 0; second made 0 found 2; control (memory store) second made 2 found 0".
#[test]
fn a_second_walk_over_the_home_store_emits_nothing() {
    let heap = blaze_own::testing::heap();
    let walk = |store: &mut dyn CodeStore| -> (usize, usize) {
        let mut run = RecordRun::new(store, builder()).expect("the builder names the emitting tier");
        run.getter(heap.alloc()).expect("the getter's entry");
        run.ender(heap.alloc()).expect("the ender's entry");
        (run.made(), run.found())
    };
    let lent = process(Sim::new(2, Faults::default()), 1);
    let first = walk(&mut HomeCodeStore::new(&lent));
    after_answer(&root_cx(), &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 2);
    let second = walk(&mut HomeCodeStore::new(&lent));
    let kept = HomeCodeStore::new(&lent);
    let made = make_getter_entry(builder().isa, heap.alloc()).expect("the getter emits");
    assert_eq!(kept.get(code_key(&getter_parts(), &builder())).expect("a get"), Some(&made[..]), "the kept getter entry is not the bytes a fresh emission writes");
    let control_first = walk(&mut MemoryStore::new_in(heap.alloc()));
    let control = walk(&mut MemoryStore::new_in(heap.alloc()));
    std::println!(
        "code store walk: first made {} found {}; second made {} found {}; control (memory store) second made {} found {}",
        first.0, first.1, second.0, second.1, control.0, control.1
    );
    assert_eq!(first, (2, 0));
    assert_eq!(second, (0, 2), "the second process emitted again over the home's store");
    assert_eq!((control_first, control), ((2, 0), (2, 0)), "the control kept code across processes: the cause is not the store");
}

/// THE STALE-ANSWER TEST. The base entry is committed; then each input `code_key` covers is
/// moved alone and the entry under the moved key must be a miss in the next process: the
/// body's text, the check's read list (a callee's signature, an ADT's definition, an impl
/// bucket), the ground facts (a projection normalized otherwise), the symbol (other ground
/// arguments), overflow checks, debug assertions, the panic strategy, the tier, the toolchain,
/// the target's LSE and its OS. A comment added to the body is NOT an input (its tokens are
/// trivia) and must be found. Then `record_key`'s inputs (D106): the BodyCheck key, a read's
/// digest, a crate fact's digest, and the code builder's producer id (D105).
/// Prints "code store stale: code_key 11 inputs moved, 11 misses, comment found; record_key 4 inputs moved, 4 misses".
#[test]
fn every_covered_input_moved_alone_is_a_miss() {
    let base_parts = parts(1);
    let base = builder();
    let mut moved: std::vec::Vec<(&str, CodeKey)> = std::vec::Vec::new();
    let with_parts = |p: KeyParts| code_key(&p, &base);
    let with_builder = |b: Builder| code_key(&base_parts, &b);
    moved.push(("the body's text", with_parts(KeyParts { body: body_digest("fn f1() -> u32 { 2 }", Some(reads(1, 0))), ..base_parts })));
    moved.push(("a callee's signature (the check's read list)", with_parts(KeyParts { body: body_digest(&text(1), Some(reads(1, 1))), ..base_parts })));
    moved.push(("the ground facts", with_parts(KeyParts { ground: ContentId::of("code store test ground", b"moved"), ..base_parts })));
    moved.push(("the symbol", with_parts(KeyParts { symbol: fn_symbol_of_name(b"other ground arguments"), ..base_parts })));
    moved.push(("overflow checks", with_builder(Builder { settings: Settings { overflow_checks: OverflowChecks::Off, ..base.settings }, ..base })));
    moved.push(("debug assertions", with_builder(Builder { settings: Settings { debug_assertions: true, ..base.settings }, ..base })));
    moved.push(("the panic strategy", with_builder(Builder { settings: Settings { panic: PanicStrategy::Abort, ..base.settings }, ..base })));
    moved.push(("the tier", with_builder(Builder { tier: TierName::Optimized, ..base })));
    moved.push(("the toolchain", with_builder(Builder { toolchain: ToolchainId([8; 32]), ..base })));
    moved.push(("the target's LSE", with_builder(Builder { isa: Isa { lse: false, ..base.isa }, ..base })));
    moved.push(("the target's OS", with_builder(Builder { isa: Isa { os: Os::Linux, ..base.isa }, ..base })));
    let comment = with_parts(KeyParts { body: body_digest("fn f1() -> u32 { /* why */ 1 }", Some(reads(1, 0))), ..base_parts });

    // record_key (body_ids.rs): one BodyCheck read and one crate fact.
    let producers = Producers::for_tests(b"code store stale");
    let producer = producers.get::<CodeBuilder>();
    let read = Read { fact: [1; 32], digest: [2; 32] };
    let fact = Read { fact: [3; 32], digest: [4; 32] };
    let symbol = fn_symbol_of_name(b"record key instance");
    let body_key = ContentId::of("code store test body check", b"1");
    let record = |key: ContentId, read: Read, fact: Read, producer: &blaze_ids::producer::ProducerId<CodeBuilder>| {
        let reads = [read];
        record_key(symbol, &BodyId { key, reads: &reads }, &[fact], &base, producer)
    };
    let record_base = record(body_key, read, fact, &producer);
    let other_producers = Producers::for_tests(b"code store stale: another code builder");
    let mut record_moved: std::vec::Vec<(&str, CodeKey)> = std::vec::Vec::new();
    record_moved.push(("the BodyCheck key", record(ContentId::of("code store test body check", b"2"), read, fact, &producer)));
    record_moved.push(("a read's digest", record(body_key, Read { digest: [5; 32], ..read }, fact, &producer)));
    record_moved.push(("a crate fact's digest", record(body_key, read, Read { digest: [6; 32], ..fact }, &producer)));
    record_moved.push(("the code builder's producer id", record(body_key, read, fact, &other_producers.get::<CodeBuilder>())));

    let lent = process(Sim::new(3, Faults::default()), 1);
    let mut store = HomeCodeStore::new(&lent);
    store.put(CodeClass::Program,code_key(&base_parts, &base), &entry(1)).expect("the base entry");
    store.put(CodeClass::Program,record_base, &entry(2)).expect("the record_key base entry");
    drop(store);
    after_answer(&root_cx(), &lent, 0).expect("the commit after the answer");
    let lent = process(back(lent), 2);
    let store = HomeCodeStore::new(&lent);
    assert_eq!(store.get(code_key(&base_parts, &base)).expect("a get"), Some(&entry(1)[..]), "the base entry is not found: the misses below prove nothing");
    assert_eq!(store.get(record_base).expect("a get"), Some(&entry(2)[..]), "the record_key base entry is not found");
    let mut misses = 0;
    for (input, key) in moved.iter() {
        assert_eq!(store.get(*key).expect("a get"), None, "{input} moved and the old entry was found: stale code");
        misses += 1;
    }
    assert_eq!(store.get(comment).expect("a get"), Some(&entry(1)[..]), "a comment moved the key: trivia is in it");
    let mut record_misses = 0;
    for (input, key) in record_moved.iter() {
        assert_eq!(store.get(*key).expect("a get"), None, "record_key: {input} moved and the old entry was found: stale code");
        record_misses += 1;
    }
    std::println!(
        "code store stale: code_key {} inputs moved, {misses} misses, comment found; record_key {} inputs moved, {record_misses} misses",
        moved.len(),
        record_moved.len()
    );
}

/// A key planted without one of its inputs: a dev entry and a release entry (overflow checks on
/// and off) or a fast and an optimized one fall under ONE key, and the second put, in the next
/// process, is the store's `TwoValues` (rule 21), never served or overwritten. Without the
/// plant the two keys differ and both puts stand.
/// Prints "code store planted: WithoutOverflow caught by TwoValues; WithoutTier caught by TwoValues; unplanted both stand".
#[test]
fn a_key_without_an_input_is_caught_by_the_two_values_refusal() {
    let base = builder();
    let release = Builder { settings: Settings { overflow_checks: OverflowChecks::Off, ..base.settings }, ..base };
    let optimized = Builder { tier: TierName::Optimized, ..base };
    let caught = |plant: KeyPlant, other: Builder, seed: u64| -> bool {
        let lent = process(Sim::new(seed, Faults::default()), 1);
        let mut store = HomeCodeStore::new(&lent);
        store.put(CodeClass::Program,code_key_planted(&parts(1), &base, plant), &entry(1)).expect("the first entry");
        drop(store);
        after_answer(&root_cx(), &lent, 0).expect("the commit after the answer");
        let lent = process(back(lent), 2);
        let mut store = HomeCodeStore::new(&lent);
        match store.put(CodeClass::Program,code_key_planted(&parts(1), &other, plant), &entry(2)) {
            Err(StoreError::TwoValues { .. }) => true,
            Ok(()) => false,
            Err(failed) => panic!("the store failed otherwise: {failed}"),
        }
    };
    assert!(caught(KeyPlant::WithoutOverflow, release, 10), "a key without the overflow setting was not caught");
    assert!(caught(KeyPlant::WithoutTier, optimized, 11), "a key without the tier was not caught");
    assert!(!caught(KeyPlant::None, release, 12), "the full key refused a release entry beside a dev one");
    assert!(!caught(KeyPlant::None, optimized, 13), "the full key refused an optimized entry beside a fast one");
    std::println!("code store planted: WithoutOverflow caught by TwoValues; WithoutTier caught by TwoValues; unplanted both stand");
}
