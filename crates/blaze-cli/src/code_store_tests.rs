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

/// Deterministic simulation test (Rule 43) for the lease-rearm, manifest-adoption, swept-record
/// restoration, and concurrent GC protocol across 24 randomized seeds with sync failures,
/// process crashes, power cuts, a full fresh-open oracle, and 3 planted-bug controls (Rule 23).
#[test]
fn dst_lease_rearm_manifest_adoption_and_gc_protocol() {
    use core::time::Duration;
    use blaze_cas::plain::{GRACE_NS, LEASE_NS, Liveness, ROOT_LIFE_NS};
    use blaze_ladder::code_records::entry_of;
    use crate::start_list::{Certificate, ListName, Today};
    use crate::store::roots::{keep_program, keep_toolchain};

    #[derive(Clone)]
    struct SharedSim(std::rc::Rc<core::cell::RefCell<Sim>>);

    impl blaze_core::Driver for SharedSim {
        fn submit(&mut self, effects: &mut std::vec::Vec<blaze_core::Effect>) {
            self.0.borrow_mut().submit(effects);
        }
        fn poll(&mut self, out: &mut std::vec::Vec<blaze_core::Completion>, until: blaze_core::wait::Deadline) -> usize {
            self.0.borrow_mut().poll(out, until)
        }
        fn in_flight(&self) -> usize {
            self.0.borrow().in_flight()
        }
        fn now(&self) -> Duration {
            self.0.borrow().now()
        }
        fn wall_ns(&self) -> u64 {
            self.0.borrow().wall_ns()
        }
        fn boot(&self) -> Result<blaze_core::Boot, blaze_core::NoBoot> {
            self.0.borrow().boot()
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ProtocolPlant {
        None,
        SkipRearmLease,
        SkipEnsurePersisted,
        SkipLentEntries,
    }

    fn next_u64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    let run_protocol_schedule = |seed: u64, plant: ProtocolPlant| -> Result<(), std::string::String> {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let sim = std::rc::Rc::new(core::cell::RefCell::new(Sim::new(seed, Faults::default())));
        let alloc = blaze_own::testing::heap().alloc();
        let mut next_nonce = 10u64;
        let mut boot_id = 1u64;

        let open_reached = |sim: &std::rc::Rc<core::cell::RefCell<Sim>>, nonce: u64, boot: u64, durable: bool| -> Result<Lent<SharedSim, StoreAlloc>, std::string::String> {
            let mut settings = StoreSettings::default();
            settings.durable = durable;
            let mut cfg = config(FileName::new("store").expect("dir"), &settings, Facts { boot: Some(boot), nonce }, alloc);
            cfg.liveness = Liveness::Reached;
            open(SharedSim(std::rc::Rc::clone(sim)), cfg, alloc).map_err(|e| std::format!("open failed: {e}"))
        };
        let big_entry = |id: u32| -> std::vec::Vec<u8> {
            (0..4096).map(|i| (id as usize).wrapping_mul(67).wrapping_add(i).wrapping_mul(13) as u8).collect()
        };
        let ptr_key = |id: u32| CodeKey(ContentId::of("dst lease gc ptr", &id.to_le_bytes()));
        let content_key = |id: u32| CodeKey(ContentId::of("dst lease gc content", &id.to_le_bytes()));
        let base_key = CasKey(ContentId::of("dst lease gc base", &seed.to_le_bytes()));
        let ids_table = CasKey(ContentId::of("dst lease gc ids", &seed.to_le_bytes()));

        // Even seeds run durable mode (testing `fail_syncs` + full `sim.crash()` power cut + reboot);
        // odd seeds run non-durable mode (testing `processes_died()` crash + segment sealing).
        let durable = (seed % 2) == 0;
        {
            next_nonce += 1;
            let s = open_reached(&sim, next_nonce, boot_id, durable)?;
            s.put(&base_key, Namespace::LOCAL, ReservedTag::Drive.tag(), b"baseline-durable").map_err(|e| std::format!("seed base: {e}"))?;
            s.root_for("ans.base", &[base_key], ROOT_LIFE_NS).map_err(|e| std::format!("root base: {e}"))?;
            after_answer(&root_cx(), &s, 0).map_err(|e| std::format!("commit base: {e}"))?;
        }
        {
            next_nonce += 1;
            let s = open_reached(&sim, next_nonce, boot_id, durable)?;
            let mut codes = HomeCodeStore::new(&s);
            for id in 0..4u32 {
                let bytes = big_entry(id);
                codes.put(CodeClass::Program, content_key(id), &bytes).map_err(|e| std::format!("seed put {id}: {e}"))?;
                codes.point(CodeClass::Program, ptr_key(id), content_key(id)).map_err(|e| std::format!("seed point {id}: {e}"))?;
            }
            drop(codes);
            after_answer(&root_cx(), &s, 0).map_err(|e| std::format!("commit unrooted seed: {e}"))?;
        }

        // Process `build` opens, runs a check-phase commit (`finish_store_for_check`), and rearms its lease.
        next_nonce += 1;
        let lent_build = open_reached(&sim, next_nonce, boot_id, durable)?;
        let check_key = CasKey(ContentId::of("dst check rec", &seed.to_le_bytes()));
        lent_build.put(&check_key, Namespace::LOCAL, ReservedTag::Drive.tag(), b"checked-ok").map_err(|e| std::format!("put check: {e}"))?;
        lent_build.root_for("ans.check", &[check_key], ROOT_LIFE_NS).map_err(|e| std::format!("root check: {e}"))?;
        after_answer(&root_cx(), &lent_build, 0).map_err(|e| std::format!("check-phase after_answer: {e}"))?;
        if plant != ProtocolPlant::SkipRearmLease {
            lent_build.rearm_lease();
        }
        // `export_program` builds/puts `RunIds` AFTER `finish_store_for_check` rearmed the lease.
        lent_build.put(&ids_table, Namespace::LOCAL, ReservedTag::Drive.tag(), b"run-ids-bytes").map_err(|e| std::format!("put ids: {e}"))?;

        let mut codes = HomeCodeStore::new(&lent_build);
        if plant == ProtocolPlant::SkipLentEntries {
            codes.disable_lent_entries_for_test();
        }

        let mut stepped: std::vec::Vec<(u32, bool)> = std::vec::Vec::new();
        let mut next_new_id = 10u32;
        let mut gc_runs = 0u32;
        let mut mid_commits = 0u32;

        // Step initial pre-existing entries (0,1 via `pointed`+`get`, 2,3 via `Put::Same`) before GC sweeps S1.
        for id in 0..2u32 {
            let under = codes.pointed(ptr_key(id)).map_err(|e| std::format!("pointed {id}: {e}"))?.ok_or_else(|| std::format!("seed ptr {id} missing"))?;
            if under != content_key(id) {
                return Err(std::format!("seed ptr {id} pointed to wrong key"));
            }
            let got = codes.get(under).map_err(|e| std::format!("get {id}: {e}"))?.ok_or_else(|| std::format!("seed content {id} missing"))?;
            if got != big_entry(id).as_slice() {
                return Err(std::format!("seed content {id} mismatch"));
            }
            stepped.push((id, true));
        }
        for id in 2..4u32 {
            let bytes = big_entry(id);
            codes.put(CodeClass::Program, content_key(id), &bytes).map_err(|e| std::format!("put same {id}: {e}"))?;
            codes.point(CodeClass::Program, ptr_key(id), content_key(id)).map_err(|e| std::format!("point same {id}: {e}"))?;
            stepped.push((id, true));
        }

        // Randomized interleaving loop: PutNew (program & toolchain), MidCommit, ConcurrentGc.
        for step in 0..10u32 {
            let choice = next_u64(&mut rng) % 10;
            let force_commit = step == 3 && mid_commits == 0;
            let force_gc = step == 5 && gc_runs == 0;
            let force_second_commit = step == 8 && mid_commits < 2;
            if force_gc || (!force_commit && !force_second_commit && choice < 3) {
                next_nonce += 1;
                let lent_gc = open_reached(&sim, next_nonce, boot_id, durable)?;
                let _ = lent_gc.compact_foreground(32).map_err(|e| std::format!("concurrent gc step {step}: {e}"))?;
                gc_runs += 1;
            } else if force_commit || force_second_commit || choice < 6 {
                let id = next_new_id;
                next_new_id += 1;
                let class = if (id % 3) == 0 { CodeClass::Toolchain } else { CodeClass::Program };
                let bytes = big_entry(id);
                codes.put(class, content_key(id), &bytes).map_err(|e| std::format!("put new {id}: {e}"))?;
                codes.point(class, ptr_key(id), content_key(id)).map_err(|e| std::format!("point new {id}: {e}"))?;
                stepped.push((id, true));
                lent_build.commit().map_err(|e| std::format!("mid-walk commit step {step}: {e}"))?;
                mid_commits += 1;
                if lent_build.leases_len() == 0 {
                    return Err(std::format!("seed {seed} step {step}: mid-walk commit acquired 0 leases (unprotected against concurrent GC)"));
                }
            } else {
                let id = next_new_id;
                next_new_id += 1;
                let class = if (id % 3) == 0 { CodeClass::Toolchain } else { CodeClass::Program };
                let bytes = big_entry(id);
                codes.put(class, content_key(id), &bytes).map_err(|e| std::format!("put new {id}: {e}"))?;
                codes.point(class, ptr_key(id), content_key(id)).map_err(|e| std::format!("point new {id}: {e}"))?;
                stepped.push((id, true));
            }
        }

        // Ensure at least one concurrent GC followed by a mid-walk commit happened so S1 is dropped from manifest.
        {
            next_nonce += 1;
            let lent_gc = open_reached(&sim, next_nonce, boot_id, durable)?;
            let _ = lent_gc.compact_foreground(32).map_err(|e| std::format!("post-loop gc: {e}"))?;
        }
        {
            let id = next_new_id;
            next_new_id += 1;
            let bytes = big_entry(id);
            codes.put(CodeClass::Program, content_key(id), &bytes).map_err(|e| std::format!("put tail {id}: {e}"))?;
            codes.point(CodeClass::Program, ptr_key(id), content_key(id)).map_err(|e| std::format!("point tail {id}: {e}"))?;
            stepped.push((id, true));
            lent_build.commit().map_err(|e| std::format!("tail mid-walk commit: {e}"))?;
        }

        // Oracle Check A (in-build post-walk assembly): every stepped content key MUST be readable via `entry_of`.
        for &(id, _) in &stepped {
            let got = entry_of(&codes, content_key(id)).map_err(|e| std::format!("post-walk entry_of {id}: {e}"))?;
            if got != Some(big_entry(id).as_slice()) {
                return Err(std::format!("seed {seed}: post-walk MissingEntry for stepped id {id}"));
            }
        }

        // Restore any swept entries/pointers and root everything under `keep_program` + `keep_toolchain`.
        if plant != ProtocolPlant::SkipEnsurePersisted {
            let _ = codes.ensure_persisted().map_err(|e| std::format!("ensure_persisted: {e}"))?;
        }
        let walked = codes.reached();
        let tool_made = codes.toolchain_made();
        drop(codes);

        let shape = ContentId::of("dst export build shape", &seed.to_le_bytes());
        let today = Today {
            certificate: Certificate::for_tests([0x5a; 32]),
            name: ListName::of(&shape),
            tables: [ids_table, ids_table],
        };
        let (_prog_root, _) = keep_program(&lent_build, &today, None, &walked).map_err(|e| std::format!("keep_program: {e}"))?;
        let _ = keep_toolchain(&lent_build, blaze_toolchain::PINNED, &tool_made, &[]).map_err(|e| std::format!("keep_toolchain: {e}"))?;
        after_answer(&root_cx(), &lent_build, 0).map_err(|e| std::format!("closing after_answer: {e}"))?;
        drop(lent_build);

        // Inject a failed sync on durable seeds + power cut (new boot), or process death on non-durable seeds.
        if durable {
            sim.borrow_mut().fail_syncs(FileName::new("store").expect("dir"));
            next_nonce += 1;
            let failing = open_reached(&sim, next_nonce, boot_id, true)?;
            let doomed = CasKey(ContentId::of("dst doomed sync", &seed.to_le_bytes()));
            failing.put(&doomed, Namespace::LOCAL, ReservedTag::Code.tag(), b"sync-will-fail").map_err(|e| std::format!("put doomed: {e}"))?;
            if failing.commit().is_ok() {
                return Err(std::format!("seed {seed}: expected durable commit under fail_syncs to fail"));
            }
            drop(failing);
            sim.borrow_mut().crash();
            boot_id += 1;
        } else {
            next_nonce += 1;
            let sealer = open_reached(&sim, next_nonce, boot_id, false)?;
            let _ = sealer.seal_foreground().map_err(|e| std::format!("seal: {e}"))?;
            drop(sealer);
            sim.borrow_mut().processes_died();
        }

        // Post-recovery GC compaction past LEASE_NS + GRACE_NS.
        sim.borrow_mut().advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + Duration::from_secs(60));
        next_nonce += 1;
        {
            let post_gc = open_reached(&sim, next_nonce, boot_id, durable)?;
            let _ = post_gc.compact_foreground(32).map_err(|e| std::format!("post-recovery gc: {e}"))?;
        }

        // Oracle Check B (freshly opened store after GC + crash/sync-fault recovery):
        // EVERY reached pointer and content record + baseline + RunIds table must be live & exact,
        // with 0 leftover leases.
        next_nonce += 1;
        let verify = open_reached(&sim, next_nonce, boot_id, durable)?;
        if verify.leases_len() != 0 {
            return Err(std::format!("seed {seed}: leaked {} lease(s) after closing commit", verify.leases_len()));
        }
        if verify.get(&base_key).map_err(|e| std::format!("verify base: {e}"))?.map(|l| l.bytes) != Some(&b"baseline-durable"[..]) {
            return Err(std::format!("seed {seed}: durable baseline missing after crash/sync-fault"));
        }
        if verify.get(&ids_table).map_err(|e| std::format!("verify ids: {e}"))?.map(|l| l.bytes) != Some(&b"run-ids-bytes"[..]) {
            return Err(std::format!("seed {seed}: RunIds table missing after post-commit GC"));
        }
        let verify_codes = HomeCodeStore::new(&verify);
        for &(id, expect_ptr) in &stepped {
            if expect_ptr {
                let target = verify_codes
                    .pointed(ptr_key(id))
                    .map_err(|e| std::format!("fresh pointed {id}: {e}"))?
                    .ok_or_else(|| std::format!("seed {seed}: fresh store missing pointer for id {id} (hole in closing root)"))?;
                if target != content_key(id) {
                    return Err(std::format!("seed {seed}: fresh store wrong pointer target for id {id}"));
                }
            }
            let got = entry_of(&verify_codes, ptr_key(id)).map_err(|e| std::format!("fresh entry_of {id}: {e}"))?;
            if got != Some(big_entry(id).as_slice()) {
                return Err(std::format!("seed {seed}: fresh store missing/corrupt content for id {id}"));
            }
        }
        Ok(())
    };

    for seed in 1..=24u64 {
        run_protocol_schedule(seed, ProtocolPlant::None)
            .unwrap_or_else(|why| panic!("DST invariant violated on seed {seed}: {why}"));
    }

    // Planted-bug controls (Rules 23, 43): each disabled mechanism MUST be caught by the oracle.
    let caught_rearm = run_protocol_schedule(2, ProtocolPlant::SkipRearmLease);
    assert!(caught_rearm.is_err(), "control failed: oracle did not catch missing rearm_lease");

    let caught_restore = run_protocol_schedule(2, ProtocolPlant::SkipEnsurePersisted);
    assert!(caught_restore.is_err(), "control failed: oracle did not catch missing ensure_persisted (holed root)");

    let caught_cache = run_protocol_schedule(2, ProtocolPlant::SkipLentEntries);
    assert!(caught_cache.is_err(), "control failed: oracle did not catch missing lent_entries (MissingEntry)");

    std::println!(
        "dst lease+gc protocol: 24/24 seeds passed (crashes + sync failures + concurrent GC); all 3 planted controls caught"
    );
}
