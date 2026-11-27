//! Incremental selective testing cache for `blaze test` (docs/v2/plan/design/test.md, "End state").
//!
//! Persists per-suite test heads (`BZTH`, `TestIndex`), postings (`BZPS`, `TestPostings`),
//! read-addressed result records (`BZTR`, `TestRun`), and the entered-function identity map
//! (`SuiteState`) in the command's CAS store (`blaze_cas::plain::Lent`), rooted under
//! `test.<suite_id>` (`store/roots.rs::keep_test_suite`).
//!
//! Soundness (rules 4, 21, 25):
//! - A test pass is kept ONLY when it ran sandboxed, harnessed, without loading any native C
//!   library (suites with a native image that overrides no pure libc symbol load it lazily only
//!   if the test actually publishes an impure/foreign record or datum, running any C initializers
//!   lazily before entering the first foreign function), every entered Rust function and static was keyed (`CodeKey`), and every entered code record
//!   and datum passed [`check_record_purity`] / [`check_datum_purity`] (no impure foreign C
//!   symbols, no raw `svc`/system-reg instructions). Any test that touches wall clock, filesystem,
//!   env, randomness, network, threads, or unknown foreign code is marked `NotKept` by name and
//!   always reruns.
//! - On a subsequent run, every previously entered function's current [`CodeKey`] is recomputed
//!   from `RunIds` via [`blaze_ladder::body_ids::instance_key`] (pure hashing, zero compilation).
//!   Unchanged tests are reused via `blaze-run::plan::Plan`'s coarse proof without running;
//!   changed functions trigger forward postings lookup (`Postings`), and undone edits hit
//!   previously filed `BZTR` passes via `Effect::Look`.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace};
use blaze_code::datum::DatumView;
use blaze_code::record::{CodeView, Space, Symbol as CodeSymbol};
use blaze_core::Driver;
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts, Set};
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::kind::{Kind as _, TestIndex, TestPostings, TestRun};
use blaze_ladder::body_ids::{BodyIds as _, each_item, instance_key};
use blaze_ladder::code_records::{Builder, PanicStrategy, TierName, triple};
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::head::{Head, ReadDigest};
use blaze_run::plan::{Address, Last, NotKept};
use blaze_run::postings::{Ask, FnName, Postings, ReadName};
use blaze_run::result::{self, AtAddress, Slot, Verdict};
use blaze_run::TestId;
use blaze_scope::tests_record::{ShouldPanic, TestRow};
use blaze_typeck::ItemKey;

use crate::run_ids::RunIds;
use crate::run_names::TestUnit;
use crate::store::plain_place::StoreAlloc;
use crate::store::roots::{self, RootError};
use crate::test_drive::{DriveError, Filed};
use crate::test_loop::SuiteId;
use crate::test_run::{Answers, RunError};

const SUITE_ID_DOMAIN: &str = "blaze test suite identity 1";
const SUITE_PTR_DOMAIN: &str = "blaze test suite pointer 1";
const SUITE_STATE_DOMAIN: &str = "blaze test suite state 1";
const VERIFIED_PTR_DOMAIN: &str = "blaze test verified pointer 1";
const VERIFIED_STATE_DOMAIN: &str = "blaze test verified state 1";
const HEAD_KEY_DOMAIN: &str = "blaze test head record 1";
const POSTINGS_KEY_DOMAIN: &str = "blaze test postings record 1";
const RESULT_SLOT_DOMAIN: &str = "blaze test result slot 1";
const TEST_ID_DOMAIN: &str = "blaze test: package target and test 2";
const ADDRESS_DOMAIN: &str = "blaze test address of traced run 1";
const TOOLS_DIGEST_DOMAIN: &str = "blaze test tools digest 1";
const SUITE_CERT_DOMAIN: &str = "blaze test suite cert 1";
const CLOSED_CRATES_DIGEST_DOMAIN: &str = "blaze test closed crates digest 1";

const STATE_MAGIC: [u8; 4] = *b"BZTS";
/// Version 3: header adds per-suite `suite_cert` beside `Standing::root` (`cert`), `tools_digest`,
/// and the closed non-workspace crate fact table (`prefix`, `fact`, `digest`) with per-fn `closed`
/// flags so `TestCache::open` skips re-hashing unchanged suites in O(1) without opening `BodyCheck`
/// rows when another suite is edited.
const STATE_VERSION: u32 = 3;
const VERIFIED_MAGIC: [u8; 4] = *b"BZTV";
/// Version 2: splits verified `(instance_key, content_key)` pairs into closed/toolchain entries
/// keyed by `closed_crates_digest` (preserved across workspace edits) and workspace entries keyed
/// by per-suite `suite_cert`.
const VERIFIED_VERSION: u32 = 2;
/// Cap on verified `(instance_key, content_key)` pairs persisted per section (`closed` and `workspace`) per suite.
const MOST_VERIFIED_PERSISTED: usize = 65_536;
/// Cap on historical `BZTR` result keys kept per suite beyond those referenced by the current
/// `Head` (bounded MRU undo-history ring so `SuiteState` and CAS roots never grow unboundedly).
const MOST_EXTRA_RESULT_KEYS: usize = 64;

/// Per-suite invalidation digests computed from the suite's crate dependency closure (Rule 21):
/// - `suite_cert` covers every crate, file plan, workspace body row, and settled answer input in
///   the suite's closure so untouched suites hit O(1) `same_cert` without opening `BodyCheck` rows.
/// - `closed_crates_digest` covers only settled non-workspace (`Toolchain` + `Dependency`) crates
///   in the suite's closure plus toolchain/tools/builder so closed `BZTV` verifications survive
///   workspace edits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SuiteDigests {
    pub(crate) suite_cert: [u8; 32],
    pub(crate) closed_crates_digest: [u8; 32],
}

/// Precomputed workspace-wide index shared across all suites in one `blaze test` invocation so
/// [`compute_suite_digests_with_ctx`] avoids O(suites × total_files) `root.places` scans and
/// redundant `root.units` sorting while producing byte-for-byte identical [`SuiteDigests`].
pub(crate) struct SuiteDigestContext<'a> {
    settled_units: Option<AVec<'a, (blaze_drive::Name, Option<ContentId>)>>,
    /// Sorted by `(unit_subject, file_name)` for O(log F) lookup of files owned by each workspace
    /// crate's root unit.
    places_by_unit: AVec<'a, ([u8; 32], blaze_drive::Name)>,
}

impl<'a> SuiteDigestContext<'a> {
    pub(crate) fn new(
        engine_root: Option<&blaze_engine::pages::Root<'_>>,
        standing_errors: u32,
        alloc: Alloc<'a>,
    ) -> SuiteDigestContext<'a> {
        let Some(root) = engine_root.filter(|root| {
            standing_errors == 0 && root.owed_tasks.is_empty() && root.owed_cells.is_empty()
        }) else {
            return SuiteDigestContext { settled_units: None, places_by_unit: Vec::new_in(alloc) };
        };
        let mut unit_names: AVec<'_, blaze_drive::Name> = Vec::with_capacity_in(root.units.len(), alloc);
        unit_names.extend_from_slice(root.units.as_slice());
        unit_names.sort_unstable();
        unit_names.dedup();
        let mut settled_units: AVec<'a, (blaze_drive::Name, Option<ContentId>)> =
            Vec::with_capacity_in(unit_names.len(), alloc);
        for &u in unit_names.iter() {
            settled_units.push((u, root.files.get(&u).copied().flatten()));
        }
        let mut places_by_unit: AVec<'a, ([u8; 32], blaze_drive::Name)> =
            Vec::with_capacity_in(root.places.len(), alloc);
        for (&file_name, place) in root.places.iter() {
            places_by_unit.push((place.unit.subject.0, file_name));
        }
        places_by_unit.sort_unstable();
        SuiteDigestContext { settled_units: Some(settled_units), places_by_unit }
    }
}

/// Computes deterministic per-suite [`SuiteDigests`] over `unit`'s transitive crate dependency
/// closure in `list` (rules 21, 25).
#[cfg_attr(not(test), allow(dead_code, reason = "single-suite convenience wrapper over compute_suite_digests_with_ctx"))]
#[expect(clippy::too_many_arguments, reason = "computes closed and full suite closure digests from engine standing state")]
pub(crate) fn compute_suite_digests(
    unit: TestUnit<'_>,
    builder: &Builder,
    list: &blaze_world::crate_list::CrateList<'_>,
    ids: &RunIds<'_>,
    tools_digest: [u8; 32],
    standing_root: ContentId,
    standing_errors: u32,
    engine_root: Option<&blaze_engine::pages::Root<'_>>,
    alloc: Alloc<'_>,
) -> SuiteDigests {
    let ctx = SuiteDigestContext::new(engine_root, standing_errors, alloc);
    compute_suite_digests_with_ctx(unit, builder, list, ids, tools_digest, standing_root, engine_root, &ctx, alloc)
}

/// Computes deterministic per-suite [`SuiteDigests`] reusing a precomputed [`SuiteDigestContext`].
#[expect(clippy::too_many_arguments, reason = "computes closed and full suite closure digests from engine standing state")]
pub(crate) fn compute_suite_digests_with_ctx(
    unit: TestUnit<'_>,
    builder: &Builder,
    list: &blaze_world::crate_list::CrateList<'_>,
    ids: &RunIds<'_>,
    tools_digest: [u8; 32],
    standing_root: ContentId,
    engine_root: Option<&blaze_engine::pages::Root<'_>>,
    ctx: &SuiteDigestContext<'_>,
    alloc: Alloc<'_>,
) -> SuiteDigests {
    use blaze_world::crate_list::CrateGraph as _;

    let n = list.len();
    let mut visited: AVec<'_, bool> = Vec::with_capacity_in(n, alloc);
    visited.resize(n, false);
    let mut stack: AVec<'_, usize> = Vec::new_in(alloc);
    let mut closure: AVec<'_, usize> = Vec::new_in(alloc);
    if let Some(slot) = visited.get_mut(unit.at) {
        *slot = true;
        stack.push(unit.at);
    }
    while let Some(at) = stack.pop() {
        closure.push(at);
        for dep in list.deps(at) {
            if let Some(slot) = visited.get_mut(dep)
                && !*slot
            {
                *slot = true;
                stack.push(dep);
            }
        }
    }
    closure.sort_unstable_by_key(|&at| list.key(at).0);

    // 1. `closed_crates_digest`: covers toolchain, producers, tools, builder, and every non-member
    // crate in `closure` (Toolchain, Fetched, CheckoutDep). Independent of workspace edits.
    let mut closed_h = Hasher::new(CLOSED_CRATES_DIGEST_DOMAIN);
    closed_h.update(&tools_digest);
    closed_h.update(&ids.toolchain().0);
    closed_h.id(&ids.producers());
    hash_builder(&mut closed_h, builder);
    let mut closed_count = 0u64;
    for &at in closure.iter() {
        if list.member(at) {
            continue;
        }
        closed_count = closed_count.saturating_add(1);
        let key = list.key(at);
        closed_h.id(&key.0);
        match ids.crate_row_bytes(key.prefix()) {
            Some(row) => {
                closed_h.update(&[1]);
                closed_h.update(row);
            }
            None => {
                closed_h.update(&[0]);
            }
        }
    }
    closed_h.update(&closed_count.to_le_bytes());
    let closed_crates_digest = closed_h.finish().0;

    // 2. `suite_cert`: covers `closed_crates_digest`, `unit.key`, all Stage::Units rows (UnitGraph,
    // manifests, Given env!/include_str! data files), and every workspace crate in `closure`
    // (its CrateRow, all RunIds BodyRows, and every source file's plan digest + FilePlace).
    // When check had errors or unsettled owed tasks/cells (or no root), fold `standing_root`.
    let mut suite_h = Hasher::new(SUITE_CERT_DOMAIN);
    suite_h.update(&closed_crates_digest);
    suite_h.id(&unit.key.0);
    match (engine_root, ctx.settled_units.as_ref()) {
        (Some(root), Some(settled_units)) => {
            suite_h.update(&[1]);
            suite_h.update(&u64::try_from(settled_units.len()).unwrap_or(u64::MAX).to_le_bytes());
            for &(u, digest) in settled_units.iter() {
                suite_h.part(u.kind.name().as_bytes()).id(&u.subject);
                match digest {
                    Some(digest) => {
                        suite_h.update(&[1]);
                        suite_h.id(&digest);
                    }
                    None => {
                        suite_h.update(&[0]);
                    }
                }
            }

            let mut root_subjects: AVec<'_, [u8; 32]> = Vec::new_in(alloc);
            let mut fstack: AVec<'_, blaze_drive::Name> = Vec::new_in(alloc);
            let mut seen_files: Set<[u8; 32], BuildKeyHasher, Alloc<'_>> =
                Set::new(blaze_hash::SetOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None });
            let mut member_count = 0u64;
            for &at in closure.iter() {
                if !list.member(at) {
                    continue;
                }
                member_count = member_count.saturating_add(1);
                let key = list.key(at);
                let prefix = key.prefix();
                suite_h.id(&key.0);
                match ids.crate_row_bytes(prefix) {
                    Some(row) => {
                        suite_h.update(&[1]);
                        suite_h.update(row);
                    }
                    None => {
                        suite_h.update(&[0]);
                    }
                }
                let bodies = ids.crate_bodies_bytes(prefix);
                suite_h.update(&u64::try_from(bodies.len()).unwrap_or(u64::MAX).to_le_bytes());
                suite_h.update(bodies);
                if let Some(rel) = list.root(at).strip_prefix("/0/") {
                    let root_file = blaze_drive::Name::file(rel.as_bytes());
                    root_subjects.push(root_file.subject.0);
                    if seen_files.insert(root_file.subject.0) {
                        fstack.push(root_file);
                    }
                }
            }
            suite_h.update(&member_count.to_le_bytes());

            // Also include any file whose `FilePlace::unit` belongs to a workspace crate in `closure`
            // (covers macro-generated `mod` files in addition to `ids.edges`) in O(log F + owned).
            root_subjects.sort_unstable();
            root_subjects.dedup();
            for &subj in root_subjects.iter() {
                let start = ctx.places_by_unit.partition_point(|&(u, _)| u < subj);
                if let Some(tail) = ctx.places_by_unit.get(start..) {
                    for &(u, file_name) in tail {
                        if u != subj {
                            break;
                        }
                        if seen_files.insert(file_name.subject.0) {
                            fstack.push(file_name);
                        }
                    }
                }
            }
            let mut files: AVec<'_, blaze_drive::Name> = Vec::new_in(alloc);
            while let Some(file) = fstack.pop() {
                files.push(file);
                if let Some(children) = ids.children(file) {
                    for child in children {
                        if child.kind == blaze_drive::Kind::FileItems && seen_files.insert(child.subject.0) {
                            fstack.push(child);
                        }
                    }
                }
            }
            files.sort_unstable();
            files.dedup();
            suite_h.update(&u64::try_from(files.len()).unwrap_or(u64::MAX).to_le_bytes());
            for &file in files.iter() {
                suite_h.id(&file.subject);
                match root.files.get(&file).copied().flatten() {
                    Some(digest) => {
                        suite_h.update(&[1]);
                        suite_h.id(&digest);
                    }
                    None => {
                        suite_h.update(&[0]);
                    }
                }
                match root.places.get(&file) {
                    Some(place) => {
                        suite_h.update(&[1, place.edition, place.flags]);
                    }
                    None => {
                        suite_h.update(&[0]);
                    }
                }
            }
        }
        _ => {
            suite_h.update(&[0]);
            suite_h.id(&standing_root);
        }
    }
    SuiteDigests { suite_cert: suite_h.finish().0, closed_crates_digest }
}

/// Sans-IO test helper computing per-suite [`SuiteDigests`] over `ids` when no full `CrateList`
/// or engine `Root` is present (used by `test_drive.rs` zero-spawn unit tests).
#[cfg(test)]
pub(crate) fn test_suite_digests(
    unit_key: blaze_id::CrateItemKey,
    builder: &Builder,
    ids: &RunIds<'_>,
    tools_digest: [u8; 32],
) -> SuiteDigests {
    let mut closed_h = Hasher::new(CLOSED_CRATES_DIGEST_DOMAIN);
    closed_h.update(&tools_digest);
    closed_h.update(&ids.toolchain().0);
    closed_h.id(&ids.producers());
    hash_builder(&mut closed_h, builder);
    let closed_crates_digest = closed_h.finish().0;

    let mut suite_h = Hasher::new(SUITE_CERT_DOMAIN);
    suite_h.update(&closed_crates_digest);
    suite_h.id(&unit_key.0);
    if let Some(row) = ids.crate_row_bytes(unit_key.prefix()) {
        suite_h.update(&[1]);
        suite_h.update(row);
    } else {
        suite_h.update(&[0]);
    }
    let bodies = ids.crate_bodies_bytes(unit_key.prefix());
    suite_h.update(&u64::try_from(bodies.len()).unwrap_or(u64::MAX).to_le_bytes());
    suite_h.update(bodies);
    SuiteDigests { suite_cert: suite_h.finish().0, closed_crates_digest }
}

/// Computes a deterministic 32-byte digest over all engine tool identities so `SuiteState` can
/// prove that `RunIds::producers` (check-kind tools) is unchanged without re-hashing every fn.
pub(crate) fn tools_digest(tools: &[blaze_drive::Tool; blaze_drive::key::KINDS]) -> [u8; 32] {
    let mut h = Hasher::new(TOOLS_DIGEST_DOMAIN);
    for tool in tools {
        h.id(&tool.0);
    }
    h.finish().0
}

/// Typed store error from persisting or reading incremental test cache records (rule 19).
#[derive(Debug)]
pub(crate) enum CacheError {
    Store(LentError),
    Root(RootError),
    Corrupt(&'static str),
}

impl core::fmt::Display for CacheError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CacheError::Store(e) => core::fmt::Display::fmt(e, f),
            CacheError::Root(e) => core::fmt::Display::fmt(e, f),
            CacheError::Corrupt(what) => f.write_str(what),
        }
    }
}

/// One Rust function or static instance entered during a test run.
pub(crate) struct EnteredFn<'a> {
    pub symbol: FnName,
    pub code_key: [u8; 32],
    pub tabled: ItemKey,
    pub name: AVec<'a, u8>,
}

/// Computes the stable suite identity covering unit settings, package, target, and code builder.
pub(crate) fn suite_id(unit: TestUnit<'_>, builder: &Builder) -> ContentId {
    let mut h = Hasher::new(SUITE_ID_DOMAIN);
    h.part(unit.package.as_bytes())
        .part(unit.target.as_bytes())
        .id(&unit.key.0)
        .part(&[u8::from(unit.tests.harnessed())]);
    hash_builder(&mut h, builder);
    h.finish()
}

/// Computes the stable suite identity for a workspace `[[bin]]` stub served via `CARGO_BIN_EXE_*`
/// so its `SuiteCache::verified` (`BZTV`) set persists across runs under today's `Certificate`.
pub(crate) fn bin_cache_id(package: &str, bin_name: &str, key: blaze_id::CrateItemKey, builder: &Builder) -> ContentId {
    let mut h = Hasher::new(SUITE_ID_DOMAIN);
    h.part(package.as_bytes())
        .part(b"bin-exe:")
        .part(bin_name.as_bytes())
        .id(&key.0)
        .part(&[2]);
    hash_builder(&mut h, builder);
    h.finish()
}

/// Loads previously verified `(instance_key, content_key)` pairs for an arbitrary suite/binary
/// identity `id` if and only if they were recorded under today's exact `certificate`.
/// Returns `Ok((0, 0))` only on a genuine cold miss (no pointer or certificate mismatch); store
/// I/O errors, dangling pointers, wrong tags, and corrupt `BZTV` payloads are returned as typed
/// errors.
pub(crate) fn load_verified_by_id<D: Driver>(
    lent: &Lent<D, StoreAlloc>,
    id: ContentId,
    certificate: &crate::start_list::Certificate,
    into_closed: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    into_workspace: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
) -> Result<(usize, usize), DriveError> {
    let store_err = |e: LentError| DriveError::CacheStore { what: "reading binary stub verified cache", why: CacheError::Store(e) };
    let corrupt = |what: &'static str| DriveError::CacheStore { what: "reading binary stub verified cache", why: CacheError::Corrupt(what) };
    let ptr_key = verified_ptr_key(id);
    let Some(state_key) = lent.pointed(TestIndex::TAG, &ptr_key).map_err(store_err)? else {
        return Ok((0, 0));
    };
    let Some(lend) = lent.get(&state_key).map_err(store_err)? else {
        return Err(corrupt("verified cache pointer names a missing record"));
    };
    if lend.tag != TestIndex::TAG {
        return Err(corrupt("verified cache record has unexpected store tag"));
    }
    let cert = *certificate.bytes();
    match decode_verified_status(lend.bytes, cert, cert, into_closed, into_workspace) {
        VerifiedDecode::Loaded(closed, ws) => Ok((closed, ws)),
        VerifiedDecode::StaleCertificate => Ok((0, 0)),
        VerifiedDecode::Corrupt => Err(corrupt("malformed BZTV verified cache payload")),
    }
}

/// Persists and pins verified `(instance_key, content_key)` pairs for a workspace binary identity
/// `id` under today's exact `certificate`.
pub(crate) fn save_verified_by_id<D: Driver>(
    lent: &Lent<D, StoreAlloc>,
    id: ContentId,
    certificate: &crate::start_list::Certificate,
    closed_pairs: impl Iterator<Item = ([u8; 32], [u8; 32])>,
    workspace_pairs: impl Iterator<Item = ([u8; 32], [u8; 32])>,
    alloc: Alloc<'_>,
) -> Result<(), DriveError> {
    let store_err = |e: LentError| DriveError::CacheStore { what: "writing binary stub verified cache", why: CacheError::Store(e) };
    let root_err = |e: RootError| DriveError::CacheStore { what: "pinning binary stub verified cache", why: CacheError::Root(e) };
    let mut closed_list: AVec<'_, ([u8; 32], [u8; 32])> = Vec::new_in(alloc);
    for pair in closed_pairs {
        if closed_list.len() >= MOST_VERIFIED_PERSISTED {
            break;
        }
        closed_list.push(pair);
    }
    let mut ws_list: AVec<'_, ([u8; 32], [u8; 32])> = Vec::new_in(alloc);
    for pair in workspace_pairs {
        if ws_list.len() >= MOST_VERIFIED_PERSISTED {
            break;
        }
        ws_list.push(pair);
    }
    if closed_list.is_empty() && ws_list.is_empty() {
        return Ok(());
    }
    closed_list.sort_unstable();
    closed_list.dedup();
    ws_list.sort_unstable();
    ws_list.dedup();
    let cert = *certificate.bytes();
    let mut vbytes: AVec<'_, u8> = Vec::new_in(alloc);
    encode_verified(cert, cert, closed_list.as_slice(), ws_list.as_slice(), &mut vbytes);
    let vstate_key = CasKey(ContentId::of(VERIFIED_STATE_DOMAIN, vbytes.as_slice()));
    let vptr_key = verified_ptr_key(id);
    lent.put(&vstate_key, Namespace::LOCAL, TestIndex::TAG, vbytes.as_slice()).map_err(store_err)?;
    lent.point(TestIndex::TAG, &vptr_key, &vstate_key).map_err(store_err)?;
    roots::keep_test_suite(lent, &id, &[vptr_key, vstate_key]).map_err(root_err)?;
    Ok(())
}

/// Computes the stable base [`TestId`] for one test row, covering every input that affects its
/// verdict independently of its code (rule 21).
pub(crate) fn test_id(unit: TestUnit<'_>, row: TestRow<'_>, builder: &Builder, args: &[&str]) -> TestId {
    let mut h = Hasher::new(TEST_ID_DOMAIN);
    h.part(unit.package.as_bytes())
        .part(unit.target.as_bytes())
        .id(&unit.key.0)
        .id(&row.key.0)
        .part(&[u8::from(unit.tests.harnessed())]);
    match row.should_panic {
        ShouldPanic::No => {
            h.part(&[0]);
        }
        ShouldPanic::Yes => {
            h.part(&[1]);
        }
        ShouldPanic::WithMessage(text) => {
            h.part(&[2]);
            h.part(text.as_bytes());
        }
    }
    hash_builder(&mut h, builder);
    if !unit.tests.harnessed() {
        for arg in args {
            h.part(arg.as_bytes());
        }
    }
    TestId(h.finish().0)
}

fn hash_builder(h: &mut Hasher, builder: &Builder) {
    let panic: u8 = match builder.settings.panic {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
    };
    let tier: u8 = match builder.tier {
        TierName::Reference => 0,
        TierName::Fast => 1,
        TierName::Optimized => 2,
        TierName::Sota => 3,
    };
    h.part(triple(builder.isa).as_bytes())
        .part(&[u8::from(builder.isa.lse), tier, u8::from(builder.settings.overflow_checks.on()), u8::from(builder.settings.debug_assertions), panic])
        .part(&builder.toolchain.0)
        .id(builder.producer.id());
}

fn suite_ptr_key(suite: ContentId) -> CasKey {
    CasKey(ContentId::of(SUITE_PTR_DOMAIN, &suite.0))
}

fn verified_ptr_key(suite: ContentId) -> CasKey {
    CasKey(ContentId::of(VERIFIED_PTR_DOMAIN, &suite.0))
}

fn result_slot_key(address: Address, slot: Slot) -> CasKey {
    let mut h = Hasher::new(RESULT_SLOT_DOMAIN);
    h.update(&address.0);
    h.update(&[match slot {
        Slot::Pass => 0,
        Slot::Failure => 1,
    }]);
    CasKey(h.finish())
}

/// Computes `H(test, asks, answers)` deterministically over sorted asks (rule 25).
fn compute_address(test: TestId, asks: &[(FnName, [u8; 32])]) -> Address {
    let mut h = Hasher::new(ADDRESS_DOMAIN);
    h.update(&test.0);
    let len = u64::try_from(asks.len()).unwrap_or(u64::MAX);
    h.update(&len.to_le_bytes());
    for (name, key) in asks {
        h.update(&name.0);
        h.update(key);
    }
    Address(h.finish().0)
}

/// Checks whether a compiled `CodeRecord`'s machine code and external C targets are provably pure
/// (no wall clock, filesystem, environment, randomness, network, threads, process spawn, or
/// unknown foreign calls). Any impure or unknown target returns the specific [`NotKept`] reason.
pub(crate) fn check_record_purity(bytes: &[u8]) -> Result<(), NotKept> {
    let Ok(view) = CodeView::read(bytes) else {
        return Err(NotKept::Untraced);
    };
    for i in 0..view.targets() {
        let target = view.target(i);
        match target.space {
            // Weak extern symbol presence depends on the host libc/runtime version, so a test can
            // branch on whether the weak address is null even for allowlisted names.
            Space::WeakOutside => return Err(NotKept::Untraced),
            Space::Outside => {
                let Some(name) = view.name(target.symbol) else {
                    return Err(NotKept::Foreign);
                };
                classify_outside_symbol(name)?;
            }
            _ => {}
        }
    }
    // Check 4-byte-aligned AArch64 instruction words for raw `svc` syscalls or impure `mrs`
    // system-register reads. Trailing read-only literal data at the end of `view.code()` need not
    // be 4-byte-length-aligned, so `chunks_exact(4)` covers every executable instruction word.
    if cfg!(target_arch = "aarch64") {
        let (chunks, _) = view.code().as_chunks::<4>();
        for &chunk in chunks {
            let inst = u32::from_le_bytes(chunk);
            // `svc #imm`: bits [31:21] == 0b11010100000, bits [4:0] == 0b00001
            if (inst & 0xFFE0_001F) == 0xD400_0001 {
                return Err(NotKept::Foreign);
            }
            // `mrs Xt, sysreg`: bits [31:20] == 0xD53
            if (inst & 0xFFF0_0000) == 0xD530_0000 {
                let sysreg = inst & 0xFFFF_FFE0;
                match sysreg {
                    // Thread pointers (`tpidr_el0` on Linux, `tpidrro_el0` on macOS) and arithmetic/FP flags.
                    0xD53B_D040 | 0xD53B_D060 | 0xD53B_4200 | 0xD53B_4400 | 0xD53B_4420 => {}
                    // Architectural timers (`cntfrq_el0`, `cntpct_el0`, `cntvct_el0`, `cntpctss_el0`, `cntvctss_el0`).
                    0xD53B_E000 | 0xD53B_E020 | 0xD53B_E040 | 0xD53B_E0A0 | 0xD53B_E0C0 => {
                        return Err(NotKept::Clock);
                    }
                    _ => return Err(NotKept::Foreign),
                }
            }
        }
    }
    Ok(())
}

/// Checks whether a static, thread-local, or vtable [`DatumView`] references only provably pure
/// external C symbols (no function pointers to foreign C code or addresses of foreign C globals).
pub(crate) fn check_datum_purity(bytes: &[u8]) -> Result<(), NotKept> {
    let Ok(view) = DatumView::read(bytes) else {
        return Err(NotKept::Untraced);
    };
    for i in 0..view.targets() {
        let target = view.target(i);
        match target.space {
            // Weak extern symbol presence depends on the host libc/runtime version, so a test can
            // branch on whether the weak address is null even for allowlisted names.
            Space::WeakOutside => return Err(NotKept::Untraced),
            Space::Outside => {
                let Some(name) = view.name(target.symbol) else {
                    return Err(NotKept::Foreign);
                };
                classify_outside_symbol(name)?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn classify_outside_symbol(raw: &str) -> Result<(), NotKept> {
    match raw {
        // Wall-clock reads, ambiguous clock-id reads, and sleeps.
        // Note: `clock_gettime` and `clock_gettime_nsec_np` accept `CLOCK_REALTIME` (wall clock)
        // as well as monotonic clocks; because this static checker does not inspect the `clk_id`
        // argument, they stay `NotKept::Clock` (rule 21). Strictly monotonic symbols
        // (`mach_absolute_time`, `mach_continuous_time`, `mach_approximate_time`,
        // `mach_timebase_info`) are classified `Ok(())` below, matching `blaze-run/src/oscall.rs`
        // (`Kind::ClockMonotonic => Rule::Own`, D81).
        "clock_gettime"
        | "gettimeofday"
        | "time"
        | "clock_gettime_nsec_np"
        | "nanosleep"
        | "clock_nanosleep"
        | "mach_wait_until"
        | "usleep"
        | "sleep"
        | "dispatch_time" => Err(NotKept::Clock),

        // Thread spawning.
        "pthread_create" | "pthread_spawn" => Err(NotKept::Threads),

        // Networking.
        "socket"
        | "socketpair"
        | "connect"
        | "bind"
        | "listen"
        | "accept"
        | "accept4"
        | "sendto"
        | "recvfrom"
        | "send"
        | "recv"
        | "sendmsg"
        | "recvmsg"
        | "getaddrinfo" => Err(NotKept::Network),

        // Blaze runtime safepoint poll page (emitted on loop back-edges for deadline checks).
        "blaze_poll_page" => Ok(()),

        // Strictly monotonic process-relative clocks (`Instant::now()`, `oscall.rs` `Rule::Own`, D81).
        "mach_absolute_time"
        | "mach_continuous_time"
        | "mach_approximate_time"
        | "mach_timebase_info" => Ok(()),

        // Provably pure memory, allocator, IEEE-754 math, compiler-rt builtins, single-process
        // sync/TLS/scheduling, and unwind hooks.
        // Note: `write`/`writev`/`fstat`/`isatty`/`mmap`/`sysconf` are intentionally NOT listed
        // because they can target arbitrary fds or observe host state without dynamic interception.
        "memcpy"
        | "memmove"
        | "memset"
        | "memcmp"
        | "bcmp"
        | "bzero"
        | "__bzero"
        | "strlen"
        | "strnlen"
        | "memchr"
        | "memrchr"
        | "memmem"
        | "strcmp"
        | "strncmp"
        | "strchr"
        | "strrchr"
        | "strstr"
        | "memset_pattern16"
        | "memset_pattern8"
        | "memset_pattern4"
        | "__memcpy_chk"
        | "__memmove_chk"
        | "__memset_chk"
        | "malloc"
        | "calloc"
        | "realloc"
        | "reallocf"
        | "valloc"
        | "free"
        | "posix_memalign"
        | "aligned_alloc"
        | "memalign"
        | "malloc_usable_size"
        | "malloc_size"
        | "malloc_good_size"
        | "fabs"
        | "fabsf"
        | "sin"
        | "cos"
        | "tan"
        | "asin"
        | "acos"
        | "atan"
        | "atan2"
        | "sinh"
        | "cosh"
        | "tanh"
        | "asinh"
        | "acosh"
        | "atanh"
        | "exp"
        | "exp2"
        | "expm1"
        | "log"
        | "log2"
        | "log10"
        | "log1p"
        | "pow"
        | "sqrt"
        | "cbrt"
        | "hypot"
        | "fma"
        | "fmod"
        | "fmax"
        | "fmin"
        | "fdim"
        | "floor"
        | "ceil"
        | "trunc"
        | "round"
        | "roundeven"
        | "rint"
        | "nearbyint"
        | "copysign"
        | "ldexp"
        | "frexp"
        | "modf"
        | "remainder"
        | "remquo"
        | "scalbn"
        | "scalbln"
        | "erf"
        | "erfc"
        | "tgamma"
        | "lgamma"
        | "lgamma_r"
        | "ilogb"
        | "logb"
        | "nextafter"
        | "sincos"
        | "__sincos_stret"
        | "sinf"
        | "cosf"
        | "tanf"
        | "asinf"
        | "acosf"
        | "atanf"
        | "atan2f"
        | "sinhf"
        | "coshf"
        | "tanhf"
        | "asinhf"
        | "acoshf"
        | "atanhf"
        | "expf"
        | "exp2f"
        | "expm1f"
        | "logf"
        | "log2f"
        | "log10f"
        | "log1pf"
        | "powf"
        | "sqrtf"
        | "cbrtf"
        | "hypotf"
        | "fmaf"
        | "fmodf"
        | "fmaxf"
        | "fminf"
        | "fdimf"
        | "floorf"
        | "ceilf"
        | "truncf"
        | "roundf"
        | "roundevenf"
        | "rintf"
        | "nearbyintf"
        | "copysignf"
        | "ldexpf"
        | "frexpf"
        | "modff"
        | "remainderf"
        | "remquof"
        | "scalbnf"
        | "scalblnf"
        | "erff"
        | "erfcf"
        | "tgammaf"
        | "lgammaf"
        | "lgammaf_r"
        | "ilogbf"
        | "logbf"
        | "nextafterf"
        | "sincosf"
        | "__sincosf_stret"
        | "__udivti3"
        | "__divti3"
        | "__modti3"
        | "__umodti3"
        | "__udivmodti4"
        | "__divmodti4"
        | "__multi3"
        | "__muloti4"
        | "__mulodi4"
        | "__mulosi4"
        | "__ashlti3"
        | "__ashrti3"
        | "__lshrti3"
        | "__addtf3"
        | "__subtf3"
        | "__multf3"
        | "__divtf3"
        | "__eqtf2"
        | "__netf2"
        | "__lttf2"
        | "__letf2"
        | "__gttf2"
        | "__getf2"
        | "__unordtf2"
        | "__fixtfsi"
        | "__fixtfdi"
        | "__fixtfti"
        | "__fixunstfsi"
        | "__fixunstfdi"
        | "__fixunstfti"
        | "__fixsfti"
        | "__fixdfti"
        | "__fixunssfti"
        | "__fixunsdfti"
        | "__floatsitf"
        | "__floatditf"
        | "__floattitf"
        | "__floatunsitf"
        | "__floatunditf"
        | "__floatuntitf"
        | "__floattisf"
        | "__floattidf"
        | "__floatuntisf"
        | "__floatuntidf"
        | "__extendsftf2"
        | "__extenddftf2"
        | "__trunctfsf2"
        | "__trunctfdf2"
        | "__extendhfsf2"
        | "__truncsfhf2"
        | "__truncdfhf2"
        | "__gnu_h2f_ieee"
        | "__gnu_f2h_ieee"
        | "__powisf2"
        | "__powidf2"
        | "__powitf2"
        | "__clzsi2"
        | "__clzdi2"
        | "__clzti2"
        | "__ctzsi2"
        | "__ctzdi2"
        | "__ctzti2"
        | "__popcountsi2"
        | "__popcountdi2"
        | "__popcountti2"
        | "__bswapsi2"
        | "__bswapdi2"
        | "__chkstk"
        | "___chkstk_darwin"
        | "__rust_probestack"
        | "_tlv_bootstrap"
        | "_tlv_atexit"
        | "__cxa_thread_atexit_impl"
        | "sched_yield"
        | "pthread_yield_np"
        | "pthread_equal"
        | "pthread_key_create"
        | "pthread_getspecific"
        | "pthread_setspecific"
        | "pthread_key_delete"
        | "pthread_mutex_init"
        | "pthread_mutex_lock"
        | "pthread_mutex_trylock"
        | "pthread_mutex_unlock"
        | "pthread_mutex_destroy"
        | "pthread_mutexattr_init"
        | "pthread_mutexattr_settype"
        | "pthread_mutexattr_gettype"
        | "pthread_mutexattr_destroy"
        | "pthread_rwlock_init"
        | "pthread_rwlock_rdlock"
        | "pthread_rwlock_wrlock"
        | "pthread_rwlock_unlock"
        | "pthread_rwlock_destroy"
        | "pthread_rwlockattr_init"
        | "pthread_rwlockattr_destroy"
        | "pthread_cond_init"
        | "pthread_cond_wait"
        | "pthread_cond_signal"
        | "pthread_cond_broadcast"
        | "pthread_cond_destroy"
        | "pthread_condattr_init"
        | "pthread_condattr_destroy"
        | "pthread_once"
        | "pthread_self"
        | "pthread_attr_init"
        | "pthread_attr_destroy"
        | "pthread_attr_getstack"
        | "pthread_attr_getguardsize"
        | "pthread_attr_setstacksize"
        | "pthread_get_stackaddr_np"
        | "pthread_get_stacksize_np"
        | "pthread_getattr_np"
        | "os_unfair_lock_lock"
        | "os_unfair_lock_trylock"
        | "os_unfair_lock_unlock"
        | "os_unfair_lock_assert_owner"
        | "os_unfair_lock_assert_not_owner"
        | "os_sync_wait_on_address"
        | "os_sync_wake_by_address_any"
        | "os_sync_wake_by_address_all"
        | "dispatch_release"
        | "dispatch_semaphore_create"
        | "dispatch_semaphore_signal"
        | "dispatch_semaphore_wait"
        | "_Unwind_Backtrace"
        | "_Unwind_Resume"
        | "_Unwind_RaiseException"
        | "_Unwind_DeleteException"
        | "_Unwind_GetCFA"
        | "_Unwind_GetDataRelBase"
        | "_Unwind_GetIP"
        | "_Unwind_GetIPInfo"
        | "_Unwind_GetLanguageSpecificData"
        | "_Unwind_GetRegionStart"
        | "_Unwind_GetTextRelBase"
        | "_Unwind_SetGR"
        | "_Unwind_SetIP"
        | "abort"
        | "exit"
        | "_exit"
        | "__assert_rtn"
        | "__assert_fail"
        | "__error"
        | "__errno_location"
        | "mach_error_string"
        | "gai_strerror"
        | "strerror_r"
        | "__xpg_strerror_r"
        | "sigaddset"
        | "sigemptyset" => Ok(()),

        _ => Err(NotKept::Foreign),
    }
}

/// Closed non-workspace crate fact persisted in `SuiteState`: `(crate_prefix, fact, digest)`.
type ClosedCrate = ([u8; 8], [u8; 32], [u8; 32]);

struct FnEntry<'a> {
    symbol: FnName,
    old_key: [u8; 32],
    now_key: Option<[u8; 32]>,
    tabled: ItemKey,
    name: AVec<'a, u8>,
    /// True iff `tabled` and every item referenced in `name` belong exclusively to `Toolchain` or
    /// `Dependency` crates (zero `Workspace` items), allowing `TestCache::open` to skip re-hashing
    /// when all recorded `closed_crates` facts and `tools_digest` are unchanged.
    closed: bool,
}

struct SuiteSlot<'a, 'm> {
    id: ContentId,
    suite_cert: [u8; 32],
    closed_crates_digest: [u8; 32],
    head_bytes: Option<&'m [u8]>,
    postings_bytes: Option<&'m [u8]>,
    /// Existing pinned keys loaded for this suite (ptr, state, head, postings, results) so a
    /// verified-only commit preserves them under `test.<id>`.
    loaded_root_keys: AVec<'a, CasKey>,
    fns: AVec<'a, FnEntry<'a>>,
    fn_index: Map<[u8; 32], usize, BuildKeyHasher, Alloc<'a>>,
    /// Historical result keys loaded from previous `SuiteState` (oldest..newest).
    result_keys: AVec<'a, CasKey>,
    /// Result keys newly filed during this command invocation.
    new_result_keys: AVec<'a, CasKey>,
    new_head: Option<(CasKey, AVec<'a, u8>)>,
    new_postings: Option<CasKey>,
    dirty: bool,
    /// Existing persisted `(verified_ptr, verified_state)` keys if already in CAS for today's
    /// closed or full suite digest.
    loaded_verified_keys: Option<(CasKey, CasKey)>,
    /// Newly verified `(closed_pairs, workspace_pairs)` to persist under today's
    /// `(suite_cert, closed_crates_digest)` on [`TestCache::commit`].
    verified_to_save: Option<(AVec<'a, ([u8; 32], [u8; 32])>, AVec<'a, ([u8; 32], [u8; 32])>)>,
}

/// CAS-backed incremental cache for all suites in one `blaze test` invocation.
pub(crate) struct TestCache<'a, 'm, D: Driver> {
    lent: &'m Lent<D, StoreAlloc>,
    cert: [u8; 32],
    tools_digest: [u8; 32],
    suites: AVec<'a, SuiteSlot<'a, 'm>>,
    /// Sorted `(TestId, SuiteId)` mapping for resolving the owning suite when the shared
    /// inventory pool (`SuiteId(u32::MAX)`) calls `Answers::answer`.
    owners: AVec<'a, (TestId, SuiteId)>,
    alloc: Alloc<'a>,
}

impl<'a, 'm, D: Driver> TestCache<'a, 'm, D> {
    /// Opens previously persisted heads, postings, and entered-function identity maps for each
    /// suite, computes which functions moved against today's `ids`, and returns the cache along
    /// with the per-suite opened [`Head`]s, [`Postings`]s, and combined `moved` ask list.
    pub(crate) fn open(
        lent: &'m Lent<D, StoreAlloc>,
        suite_specs: &[crate::test_drive::SuiteSpec<'_>],
        ids: &RunIds<'_>,
        certificate: &crate::start_list::Certificate,
        tools_digest: [u8; 32],
        alloc: Alloc<'a>,
    ) -> (TestCache<'a, 'm, D>, AVec<'a, Head<'m>>, AVec<'a, Postings<'m>>, AVec<'a, Ask>) {
        let cert = *certificate.bytes();
        let mut suites = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut heads = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut postings_list = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut moved = Vec::new_in(alloc);

        for spec in suite_specs {
            let unit = &spec.unit;
            let builder = &spec.builder;
            let sid = suite_id(*unit, builder);
            let mut slot = SuiteSlot {
                id: sid,
                suite_cert: spec.digests.suite_cert,
                closed_crates_digest: spec.digests.closed_crates_digest,
                head_bytes: None,
                postings_bytes: None,
                loaded_root_keys: Vec::new_in(alloc),
                fns: Vec::new_in(alloc),
                fn_index: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: None }),
                result_keys: Vec::new_in(alloc),
                new_result_keys: Vec::new_in(alloc),
                new_head: None,
                new_postings: None,
                dirty: false,
                loaded_verified_keys: None,
                verified_to_save: None,
            };
            let mut head = Head::EMPTY;
            let mut postings = Postings::EMPTY;

            if let Some(loaded) = load_suite_state(lent, sid, alloc)
                && let Ok(opened_head) = Head::open(loaded.head_bytes)
                && let Ok(opened_postings) = Postings::open(loaded.postings_bytes)
            {
                slot.head_bytes = Some(loaded.head_bytes);
                slot.postings_bytes = Some(loaded.postings_bytes);
                slot.loaded_root_keys.push(loaded.ptr_key);
                slot.loaded_root_keys.push(loaded.state_key);
                slot.loaded_root_keys.push(loaded.head_key);
                slot.loaded_root_keys.push(loaded.postings_key);
                slot.loaded_root_keys.extend_from_slice(loaded.result_keys.as_slice());
                slot.result_keys = loaded.result_keys;
                // Fast path 1: exact Standing::root certificate OR per-suite closure `suite_cert`
                // and check tools digest match -> every crate, file, and body in this suite's
                // closure is identical (zero `instance_key` hashing and zero `BodyCheck` opens).
                let same_cert = loaded.tools_digest == tools_digest
                    && (loaded.cert == cert || loaded.suite_cert == spec.digests.suite_cert);
                // Fast path 2: on edits inside this suite's closure, skip re-hashing pure
                // toolchain/dependency fns whose generic arguments touch no workspace crate
                // whenever their crate facts stand.
                let closed_ok = !same_cert
                    && loaded.tools_digest == tools_digest
                    && closed_crates_unchanged(loaded.closed_crates.as_slice(), ids);
                for mut f in loaded.fns {
                    if same_cert || (closed_ok && f.closed) {
                        f.now_key = Some(f.old_key);
                    } else {
                        let now = instance_key(
                            CodeSymbol(f.symbol.0),
                            f.tabled,
                            f.name.as_slice(),
                            ids,
                            builder,
                            &builder.producer,
                            alloc,
                        )
                        .map(|k| k.0.0);
                        f.now_key = now;
                        if now != Some(f.old_key) {
                            moved.push(Ask::Fn(f.symbol));
                        }
                    }
                    let at = slot.fns.len();
                    slot.fn_index.insert(f.symbol.0, at);
                    slot.fns.push(f);
                }
                head = opened_head;
                postings = opened_postings;
            }
            suites.push(slot);
            heads.push(head);
            postings_list.push(postings);
        }

        moved.sort_unstable();
        moved.dedup();
        (TestCache { lent, cert, tools_digest, suites, owners: Vec::new_in(alloc), alloc }, heads, postings_list, moved)
    }

    /// Registers the `(TestId, SuiteId)` ownership table for the shared inventory pool.
    pub(crate) fn set_owners(&mut self, asked: &[blaze_run::plan::Asked<'_>], owners: &[SuiteId]) {
        self.owners.clear();
        for (one, &suite) in asked.iter().zip(owners.iter()) {
            self.owners.push((one.test, suite));
        }
        self.owners.sort_unstable_by_key(|&(id, _)| id);
    }

    fn resolve_suite(&self, suite: SuiteId, test: TestId) -> SuiteId {
        if suite.0 == u32::MAX
            && let Ok(at) = self.owners.binary_search_by_key(&test, |&(id, _)| id)
            && let Some(&(_, owner)) = self.owners.get(at)
        {
            return owner;
        }
        suite
    }

    /// Loads previously verified `(instance_key, content_key)` pairs for `suite`:
    /// - Closed/toolchain entries load whenever `closed_crates_digest` stands (surviving workspace
    ///   edits so `std`/`core`/`alloc`/dep callees skip `verify_instance` in O(1)).
    /// - Workspace entries load only when the suite's full `suite_cert` also stands.
    /// Returns `(closed_loaded, workspace_loaded)`.
    pub(crate) fn load_verified(
        &mut self,
        suite: SuiteId,
        into_closed: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
        into_workspace: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    ) -> (usize, usize) {
        let Ok(idx) = usize::try_from(suite.0) else { return (0, 0) };
        let Some(slot) = self.suites.get_mut(idx) else { return (0, 0) };
        let ptr_key = verified_ptr_key(slot.id);
        let Ok(Some(state_key)) = self.lent.pointed(TestIndex::TAG, &ptr_key) else { return (0, 0) };
        let Ok(Some(lend)) = self.lent.get(&state_key) else { return (0, 0) };
        if lend.tag != TestIndex::TAG {
            return (0, 0);
        }
        let Some(loaded) = decode_verified(
            lend.bytes,
            slot.suite_cert,
            slot.closed_crates_digest,
            into_closed,
            into_workspace,
        ) else {
            return (0, 0);
        };
        slot.loaded_verified_keys = Some((ptr_key, state_key));
        loaded
    }

    /// Schedules the full sets of closed and workspace verified `(instance_key, content_key)`
    /// pairs for `suite` to be persisted under today's `(suite_cert, closed_crates_digest)` on
    /// [`Self::commit`].
    pub(crate) fn record_verified(
        &mut self,
        suite: SuiteId,
        closed_pairs: impl Iterator<Item = ([u8; 32], [u8; 32])>,
        workspace_pairs: impl Iterator<Item = ([u8; 32], [u8; 32])>,
    ) {
        let Ok(idx) = usize::try_from(suite.0) else { return };
        let Some(slot) = self.suites.get_mut(idx) else { return };
        let mut closed = Vec::new_in(self.alloc);
        for pair in closed_pairs {
            if closed.len() >= MOST_VERIFIED_PERSISTED {
                break;
            }
            closed.push(pair);
        }
        let mut workspace = Vec::new_in(self.alloc);
        for pair in workspace_pairs {
            if workspace.len() >= MOST_VERIFIED_PERSISTED {
                break;
            }
            workspace.push(pair);
        }
        if !closed.is_empty() || !workspace.is_empty() {
            slot.verified_to_save = Some((closed, workspace));
        }
    }

    /// Records the walked function metadata observed during a test run so [`Answers::answer`] and
    /// [`Self::commit`] know every entered function's identity and `instance_name`.
    pub(crate) fn record_entered(&mut self, suite: SuiteId, entered: &[EnteredFn<'a>]) {
        let Ok(idx) = usize::try_from(suite.0) else { return };
        let Some(slot) = self.suites.get_mut(idx) else { return };
        for e in entered {
            if let Some(&at) = slot.fn_index.get(&e.symbol.0) {
                if let Some(entry) = slot.fns.get_mut(at) {
                    entry.now_key = Some(e.code_key);
                    entry.old_key = e.code_key;
                    entry.tabled = e.tabled;
                    entry.name.clear();
                    entry.name.extend_from_slice(e.name.as_slice());
                }
            } else {
                let mut name = Vec::with_capacity_in(e.name.len(), self.alloc);
                name.extend_from_slice(e.name.as_slice());
                let at = slot.fns.len();
                slot.fn_index.insert(e.symbol.0, at);
                slot.fns.push(FnEntry {
                    symbol: e.symbol,
                    old_key: e.code_key,
                    now_key: Some(e.code_key),
                    tabled: e.tabled,
                    name,
                    closed: false,
                });
            }
        }
    }

    /// Answers `Effect::Look`: recomputes `test`'s address from its previous head row's asks
    /// answered with today's code identities, and checks whether a `BZTR` pass exists there.
    pub(crate) fn look(&self, suite: SuiteId, test: TestId) -> Result<bool, DriveError> {
        let suite = self.resolve_suite(suite, test);
        let Ok(idx) = usize::try_from(suite.0) else { return Ok(false) };
        let Some(slot) = self.suites.get(idx) else { return Ok(false) };
        let Some(head_bytes) = slot.head_bytes else { return Ok(false) };
        let Ok(head) = Head::open(head_bytes) else { return Ok(false) };
        let Some(row) = head.row(test) else { return Ok(false) };
        let mut pairs: AVec<'_, (FnName, [u8; 32])> = Vec::new_in(self.alloc);
        for ask in head.asks(row) {
            match ask {
                Ask::Fn(name) => {
                    let Some(&at) = slot.fn_index.get(&name.0) else {
                        return Ok(false);
                    };
                    let Some(now) = slot.fns.get(at).and_then(|f| f.now_key) else {
                        return Ok(false);
                    };
                    pairs.push((name, now));
                }
                Ask::Read(_) => return Ok(false),
            }
        }
        if pairs.is_empty() {
            return Ok(false);
        }
        pairs.sort_unstable_by_key(|(name, _)| *name);
        pairs.dedup_by_key(|(name, _)| *name);
        let address = compute_address(test, &pairs);
        let key = result_slot_key(address, Slot::Pass);
        match self.lent.get(&key) {
            Ok(Some(lend)) if lend.tag == TestRun::TAG => Ok(matches!(result::open(lend.bytes), Ok(Verdict::Pass(_)))),
            Ok(_) => Ok(false),
            Err(_) => Ok(false),
        }
    }

    /// Commits every dirty suite's `SuiteState`, verified-entry cache, pointer, and root into
    /// `Lent` (flushed to disk by the command's single post-answer commit).
    pub(crate) fn commit(mut self, ids: &RunIds<'_>) -> Result<(), DriveError> {
        let store_err = |e: LentError| DriveError::CacheStore { what: "writing test suite cache", why: CacheError::Store(e) };
        let root_err = |e: RootError| DriveError::CacheStore { what: "pinning test suite root", why: CacheError::Root(e) };
        for slot in self.suites.iter_mut() {
            let mut verified_keys = slot.loaded_verified_keys;
            let mut verified_changed = false;
            if let Some((mut closed_pairs, mut ws_pairs)) = slot.verified_to_save.take()
                && (!closed_pairs.is_empty() || !ws_pairs.is_empty())
            {
                closed_pairs.sort_unstable();
                closed_pairs.dedup();
                ws_pairs.sort_unstable();
                ws_pairs.dedup();
                let mut vbytes: AVec<'_, u8> = Vec::new_in(self.alloc);
                encode_verified(
                    slot.suite_cert,
                    slot.closed_crates_digest,
                    closed_pairs.as_slice(),
                    ws_pairs.as_slice(),
                    &mut vbytes,
                );
                let vstate_key = CasKey(ContentId::of(VERIFIED_STATE_DOMAIN, vbytes.as_slice()));
                let vptr_key = verified_ptr_key(slot.id);
                self.lent.put(&vstate_key, Namespace::LOCAL, TestIndex::TAG, vbytes.as_slice()).map_err(store_err)?;
                self.lent.point(TestIndex::TAG, &vptr_key, &vstate_key).map_err(store_err)?;
                verified_keys = Some((vptr_key, vstate_key));
                verified_changed = true;
            }

            if !slot.dirty {
                if verified_changed
                    && let Some((vptr_key, vstate_key)) = verified_keys
                {
                    let mut pinned: AVec<'_, CasKey> = Vec::with_capacity_in(slot.loaded_root_keys.len() + 2, self.alloc);
                    pinned.extend_from_slice(slot.loaded_root_keys.as_slice());
                    pinned.push(vptr_key);
                    pinned.push(vstate_key);
                    roots::keep_test_suite(self.lent, &slot.id, pinned.as_slice()).map_err(root_err)?;
                }
                continue;
            }
            let (head_key, head_bytes_slice): (CasKey, &[u8]) = match &slot.new_head {
                Some((key, bytes)) => (*key, bytes.as_slice()),
                None => continue,
            };
            let Some(postings_key) = slot.new_postings else { continue };
            // Keep only `FnEntry` rows still referenced by the new head, and collect live head
            // result keys (both `Last::Passed` and `Last::Unsettled` stay pinned for undo lookups).
            let mut live_fns: AVec<'_, FnName> = Vec::new_in(self.alloc);
            let mut head_results: AVec<'_, CasKey> = Vec::new_in(self.alloc);
            if let Ok(head) = Head::open(head_bytes_slice) {
                for row in head.rows() {
                    if let Last::Passed { address } | Last::Unsettled { address } = row.kept.last {
                        head_results.push(result_slot_key(address, Slot::Pass));
                    }
                    for ask in head.asks(row) {
                        if let Ask::Fn(name) = ask {
                            live_fns.push(name);
                        }
                    }
                }
            }
            live_fns.sort_unstable();
            live_fns.dedup();
            // Prune historical `BZTR` pins to all current head rows plus a bounded MRU undo ring.
            let live_results = prune_result_keys(
                slot.result_keys.as_slice(),
                slot.new_result_keys.as_slice(),
                head_results.as_slice(),
                MOST_EXTRA_RESULT_KEYS,
                self.alloc,
            );

            let mut kept_fns: AVec<'_, usize> = Vec::new_in(self.alloc);
            for (i, f) in slot.fns.iter().enumerate() {
                if live_fns.binary_search(&f.symbol).is_ok() && f.now_key.is_some() {
                    kept_fns.push(i);
                }
            }
            kept_fns.sort_unstable_by_key(|&i| slot.fns.get(i).map(|f| f.symbol));

            let mut closed_crates: AVec<'_, ClosedCrate> = Vec::new_in(self.alloc);
            let mut kept_flags: AVec<'_, (usize, bool)> = Vec::with_capacity_in(kept_fns.len(), self.alloc);
            for &i in kept_fns.iter() {
                let Some(f) = slot.fns.get(i) else { continue };
                let closed = classify_closed_entry(f.tabled, f.name.as_slice(), ids, &mut closed_crates);
                kept_flags.push((i, closed));
            }
            closed_crates.sort_unstable();
            closed_crates.dedup();

            // Encode SuiteState deterministically (sorted by `symbol`, rule 25).
            let mut state_bytes: AVec<'_, u8> = Vec::new_in(self.alloc);
            encode_suite_state(
                self.cert,
                slot.suite_cert,
                self.tools_digest,
                head_key,
                postings_key,
                slot.fns.as_slice(),
                kept_flags.as_slice(),
                closed_crates.as_slice(),
                live_results.as_slice(),
                &mut state_bytes,
            );

            let state_key = CasKey(ContentId::of(SUITE_STATE_DOMAIN, state_bytes.as_slice()));
            let ptr_key = suite_ptr_key(slot.id);
            self.lent.put(&state_key, Namespace::LOCAL, TestIndex::TAG, state_bytes.as_slice()).map_err(store_err)?;
            self.lent.point(TestIndex::TAG, &ptr_key, &state_key).map_err(store_err)?;

            let mut pinned: AVec<'_, CasKey> = Vec::with_capacity_in(6 + live_results.len(), self.alloc);
            pinned.push(ptr_key);
            pinned.push(state_key);
            pinned.push(head_key);
            pinned.push(postings_key);
            pinned.extend_from_slice(live_results.as_slice());
            if let Some((vptr_key, vstate_key)) = verified_keys {
                pinned.push(vptr_key);
                pinned.push(vstate_key);
            }
            roots::keep_test_suite(self.lent, &slot.id, pinned.as_slice()).map_err(root_err)?;
        }
        Ok(())
    }
}

impl<D: Driver> Answers for TestCache<'_, '_, D> {
    fn answer<'s>(
        &'s mut self,
        suite: SuiteId,
        test: TestId,
        entered: &[FnName],
        _reads: &[ReadName],
        _digests: &mut AVec<'_, (ReadName, ReadDigest)>,
    ) -> Result<(Address, AtAddress<'s>), RunError> {
        let suite = self.resolve_suite(suite, test);
        let Some(slot) = usize::try_from(suite.0).ok().and_then(|i| self.suites.get(i)) else {
            return Ok((Address(ContentId::of(ADDRESS_DOMAIN, &test.0).0), AtAddress::EMPTY));
        };
        let mut pairs: AVec<'_, (FnName, [u8; 32])> = Vec::with_capacity_in(entered.len(), self.alloc);
        for &name in entered {
            if let Some(&at) = slot.fn_index.get(&name.0)
                && let Some(key) = slot.fns.get(at).and_then(|f| f.now_key)
            {
                pairs.push((name, key));
            }
        }
        pairs.sort_unstable_by_key(|(name, _)| *name);
        pairs.dedup_by_key(|(name, _)| *name);
        let address = compute_address(test, &pairs);

        let pass = match self.lent.get(&result_slot_key(address, Slot::Pass)) {
            Ok(Some(lend)) if lend.tag == TestRun::TAG => match result::open(lend.bytes) {
                Ok(Verdict::Pass(passed)) => Some(passed),
                _ => None,
            },
            _ => None,
        };
        let failure = match self.lent.get(&result_slot_key(address, Slot::Failure)) {
            Ok(Some(lend)) if lend.tag == TestRun::TAG => match result::open(lend.bytes) {
                Ok(Verdict::Fail(failed)) => Some(failed),
                _ => None,
            },
            _ => None,
        };
        Ok((address, AtAddress { pass, failure }))
    }
}

impl<D: Driver> Filed for TestCache<'_, '_, D> {
    fn file(&mut self, suite: SuiteId, test: TestId, address: Address, slot: Slot, bytes: &[u8]) -> Result<(), DriveError> {
        let suite = self.resolve_suite(suite, test);
        let key = result_slot_key(address, slot);
        let store_err = |e: LentError| DriveError::CacheStore { what: "writing test result record", why: CacheError::Store(e) };
        self.lent.put(&key, Namespace::LOCAL, TestRun::TAG, bytes).map_err(store_err)?;
        if let Some(s) = usize::try_from(suite.0).ok().and_then(|i| self.suites.get_mut(i)) {
            s.new_result_keys.push(key);
            s.dirty = true;
        }
        Ok(())
    }

    fn flaky(&mut self, _suite: SuiteId, _test: TestId) -> Result<(), DriveError> {
        Ok(())
    }

    fn head(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError> {
        let key = CasKey(ContentId::of(HEAD_KEY_DOMAIN, bytes));
        let store_err = |e: LentError| DriveError::CacheStore { what: "writing test head record", why: CacheError::Store(e) };
        self.lent.put(&key, Namespace::LOCAL, TestIndex::TAG, bytes).map_err(store_err)?;
        if let Some(s) = usize::try_from(suite.0).ok().and_then(|i| self.suites.get_mut(i)) {
            let mut copy = Vec::with_capacity_in(bytes.len(), self.alloc);
            copy.extend_from_slice(bytes);
            s.new_head = Some((key, copy));
            s.dirty = true;
        }
        Ok(())
    }

    fn postings(&mut self, suite: SuiteId, bytes: &[u8]) -> Result<(), DriveError> {
        let key = CasKey(ContentId::of(POSTINGS_KEY_DOMAIN, bytes));
        let store_err = |e: LentError| DriveError::CacheStore { what: "writing test postings record", why: CacheError::Store(e) };
        self.lent.put(&key, Namespace::LOCAL, TestPostings::TAG, bytes).map_err(store_err)?;
        if let Some(s) = usize::try_from(suite.0).ok().and_then(|i| self.suites.get_mut(i)) {
            s.new_postings = Some(key);
            s.dirty = true;
        }
        Ok(())
    }
}

struct LoadedState<'a, 'm> {
    ptr_key: CasKey,
    state_key: CasKey,
    cert: [u8; 32],
    suite_cert: [u8; 32],
    tools_digest: [u8; 32],
    head_key: CasKey,
    postings_key: CasKey,
    head_bytes: &'m [u8],
    postings_bytes: &'m [u8],
    fns: AVec<'a, FnEntry<'a>>,
    closed_crates: AVec<'a, ClosedCrate>,
    result_keys: AVec<'a, CasKey>,
}

/// Decoded payload of a `SuiteState` v3 record (before resolving `head_key`/`postings_key` in CAS).
struct DecodedSuiteState<'a> {
    cert: [u8; 32],
    suite_cert: [u8; 32],
    tools_digest: [u8; 32],
    head_key: CasKey,
    postings_key: CasKey,
    fns: AVec<'a, FnEntry<'a>>,
    closed_crates: AVec<'a, ClosedCrate>,
    result_keys: AVec<'a, CasKey>,
}

/// Checks whether a walked function `(tabled, name)` touches ONLY `Toolchain` and `Dependency`
/// crates (zero `Workspace` items in either `tabled` or any generic argument in `name`). When true,
/// appends every referenced crate's `(prefix, fact, digest)` to `crates_out`.
fn classify_closed_entry(
    tabled: ItemKey,
    name: &[u8],
    ids: &RunIds<'_>,
    crates_out: &mut AVec<'_, ClosedCrate>,
) -> bool {
    let check_item = |item: ItemKey| -> Option<ClosedCrate> {
        match ids.grain(item)? {
            crate::run_ids::Grain::Toolchain | crate::run_ids::Grain::Dependency => {
                let read = ids.crate_fact(item)?;
                let b = item.0.to_be_bytes();
                let prefix = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
                Some((prefix, read.fact, read.digest))
            }
            crate::run_ids::Grain::Workspace => None,
        }
    };
    let Some(head_crate) = check_item(tabled) else { return false };
    let start = crates_out.len();
    crates_out.push(head_crate);
    let mut all_closed = true;
    let parsed = each_item(name, |item| {
        if all_closed {
            match check_item(item) {
                Some(entry) => crates_out.push(entry),
                None => all_closed = false,
            }
        }
    });
    if parsed.is_err() || !all_closed {
        crates_out.truncate(start);
        return false;
    }
    true
}

/// Returns true iff every non-workspace crate recorded in `closed_crates` still has a `Toolchain`
/// or `Dependency` grain and the exact same `Read { fact, digest }` in today's `ids`.
fn closed_crates_unchanged(closed_crates: &[ClosedCrate], ids: &RunIds<'_>) -> bool {
    for &(prefix, fact, digest) in closed_crates {
        let probe = ItemKey(u128::from_be_bytes([
            prefix[0], prefix[1], prefix[2], prefix[3],
            prefix[4], prefix[5], prefix[6], prefix[7],
            0, 0, 0, 0, 0, 0, 0, 0,
        ]));
        let is_non_workspace = matches!(
            ids.grain(probe),
            Some(crate::run_ids::Grain::Toolchain | crate::run_ids::Grain::Dependency)
        );
        if !is_non_workspace {
            return false;
        }
        let Some(read) = ids.crate_fact(probe) else { return false };
        if read.fact != fact || read.digest != digest {
            return false;
        }
    }
    true
}

/// Builds the bounded set of `BZTR` result keys to persist and pin for a suite:
/// - Every `Slot::Pass` key referenced by the current `Head` (`Last::Passed | Last::Unsettled`) is
///   always retained (never pruned).
/// - Up to `cap` additional historical keys (from `prior` and `newly_filed`) are retained in MRU
///   order (`oldest..newest`) so undo-edits and flakiness checks stay hot while CAS pins remain
///   strictly bounded on long-lived workspaces.
///
/// Output layout: `[history_tail (oldest..newest)..., head_sorted...]`, zero duplicates,
/// deterministic regardless of parallel test completion order (rule 25).
fn prune_result_keys<'a>(
    prior: &[CasKey],
    newly_filed: &[CasKey],
    head_results: &[CasKey],
    cap: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, CasKey> {
    let mut head_sorted: AVec<'_, CasKey> = Vec::with_capacity_in(head_results.len(), alloc);
    head_sorted.extend_from_slice(head_results);
    head_sorted.sort_unstable();
    head_sorted.dedup();

    let mut new_sorted: AVec<'_, CasKey> = Vec::with_capacity_in(newly_filed.len(), alloc);
    new_sorted.extend_from_slice(newly_filed);
    new_sorted.sort_unstable();
    new_sorted.dedup();

    let mut history: AVec<'_, CasKey> = Vec::new_in(alloc);
    let mut seen: Set<[u8; 32], BuildKeyHasher, Alloc<'a>> = Set::new(blaze_hash::SetOpts {
        hasher: BuildKeyHasher::default(),
        alloc,
        capacity: Some(prior.len().saturating_add(new_sorted.len())),
    });
    for &k in prior {
        if head_sorted.binary_search(&k).is_err()
            && new_sorted.binary_search(&k).is_err()
            && seen.insert(k.0.0)
        {
            history.push(k);
        }
    }
    for &k in new_sorted.iter() {
        if head_sorted.binary_search(&k).is_err() && seen.insert(k.0.0) {
            history.push(k);
        }
    }
    let drop_n = history.len().saturating_sub(cap);
    let tail = history.get(drop_n..).unwrap_or(&[]);

    let mut out: AVec<'a, CasKey> = Vec::with_capacity_in(tail.len().saturating_add(head_sorted.len()), alloc);
    out.extend_from_slice(tail);
    out.extend_from_slice(head_sorted.as_slice());
    out
}

#[expect(clippy::too_many_arguments, reason = "deterministic SuiteState v3 encoder")]
fn encode_suite_state(
    cert: [u8; 32],
    suite_cert: [u8; 32],
    tools_digest: [u8; 32],
    head_key: CasKey,
    postings_key: CasKey,
    fns: &[FnEntry<'_>],
    kept_flags: &[(usize, bool)],
    closed_crates: &[ClosedCrate],
    live_results: &[CasKey],
    out: &mut AVec<'_, u8>,
) {
    out.extend_from_slice(&STATE_MAGIC);
    out.extend_from_slice(&STATE_VERSION.to_le_bytes());
    out.extend_from_slice(&cert);
    out.extend_from_slice(&suite_cert);
    out.extend_from_slice(&tools_digest);
    out.extend_from_slice(&head_key.0.0);
    out.extend_from_slice(&postings_key.0.0);
    let fn_count = u32::try_from(kept_flags.len()).unwrap_or(u32::MAX);
    let res_count = u32::try_from(live_results.len()).unwrap_or(u32::MAX);
    let crate_count = u32::try_from(closed_crates.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&fn_count.to_le_bytes());
    out.extend_from_slice(&res_count.to_le_bytes());
    out.extend_from_slice(&crate_count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for &(i, closed) in kept_flags {
        let Some(f) = fns.get(i) else { continue };
        let Some(key) = f.now_key else { continue };
        out.extend_from_slice(&f.symbol.0);
        out.extend_from_slice(&key);
        out.extend_from_slice(&f.tabled.0.to_le_bytes());
        out.push(u8::from(closed));
        out.extend_from_slice(&[0u8; 3]);
        let nlen = u32::try_from(f.name.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(f.name.as_slice());
    }
    for &(prefix, fact, digest) in closed_crates {
        out.extend_from_slice(&prefix);
        out.extend_from_slice(&fact);
        out.extend_from_slice(&digest);
    }
    for rk in live_results {
        out.extend_from_slice(&rk.0.0);
    }
}

fn decode_suite_state<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Option<DecodedSuiteState<'a>> {
    let read_u32 = |buf: &[u8], off: usize| -> Option<u32> {
        let slice: [u8; 4] = buf.get(off..off.checked_add(4)?)?.try_into().ok()?;
        Some(u32::from_le_bytes(slice))
    };
    let read_u128 = |buf: &[u8], off: usize| -> Option<u128> {
        let slice: [u8; 16] = buf.get(off..off.checked_add(16)?)?.try_into().ok()?;
        Some(u128::from_le_bytes(slice))
    };
    let read_8 = |buf: &[u8], off: usize| -> Option<[u8; 8]> {
        buf.get(off..off.checked_add(8)?)?.try_into().ok()
    };
    let read_32 = |buf: &[u8], off: usize| -> Option<[u8; 32]> {
        buf.get(off..off.checked_add(32)?)?.try_into().ok()
    };

    let header_len = 4 + 4 + 32 + 32 + 32 + 32 + 32 + 4 + 4 + 4 + 4;
    if bytes.len() < header_len || bytes.get(0..4) != Some(&STATE_MAGIC) {
        return None;
    }
    let version = read_u32(bytes, 4)?;
    if version != STATE_VERSION {
        return None;
    }
    let cert = read_32(bytes, 8)?;
    let suite_cert = read_32(bytes, 40)?;
    let tools_digest = read_32(bytes, 72)?;
    let head_key = CasKey(ContentId(read_32(bytes, 104)?));
    let postings_key = CasKey(ContentId(read_32(bytes, 136)?));
    let fn_count = usize::try_from(read_u32(bytes, 168)?).ok()?;
    let res_count = usize::try_from(read_u32(bytes, 172)?).ok()?;
    let crate_count = usize::try_from(read_u32(bytes, 176)?).ok()?;
    if read_u32(bytes, 180)? != 0 {
        return None;
    }

    let mut at = header_len;
    let fixed_fn = 32 + 32 + 16 + 4 + 4;
    if fn_count > bytes.len().checked_sub(at)? / fixed_fn {
        return None;
    }
    let mut fns = Vec::with_capacity_in(fn_count, alloc);
    for _ in 0..fn_count {
        if bytes.len().checked_sub(at)? < fixed_fn {
            return None;
        }
        let symbol = FnName(read_32(bytes, at)?);
        let old_key = read_32(bytes, at.checked_add(32)?)?;
        let tabled = ItemKey(read_u128(bytes, at.checked_add(64)?)?);
        let closed_byte = *bytes.get(at.checked_add(80)?)?;
        if closed_byte > 1 || bytes.get(at.checked_add(81)?..at.checked_add(84)?) != Some(&[0u8; 3]) {
            return None;
        }
        let nlen = usize::try_from(read_u32(bytes, at.checked_add(84)?)?).ok()?;
        at = at.checked_add(fixed_fn)?;
        let end = at.checked_add(nlen)?;
        let name_slice = bytes.get(at..end)?;
        let mut name = Vec::with_capacity_in(nlen, alloc);
        name.extend_from_slice(name_slice);
        at = end;
        fns.push(FnEntry { symbol, old_key, now_key: None, tabled, name, closed: closed_byte == 1 });
    }

    let crates_needed = crate_count.checked_mul(72)?;
    let res_needed = res_count.checked_mul(32)?;
    if bytes.len().checked_sub(at)? != crates_needed.checked_add(res_needed)? {
        return None;
    }
    let mut closed_crates = Vec::with_capacity_in(crate_count, alloc);
    for _ in 0..crate_count {
        let prefix = read_8(bytes, at)?;
        let fact = read_32(bytes, at.checked_add(8)?)?;
        let digest = read_32(bytes, at.checked_add(40)?)?;
        at = at.checked_add(72)?;
        closed_crates.push((prefix, fact, digest));
    }

    let mut result_keys = Vec::with_capacity_in(res_count, alloc);
    for _ in 0..res_count {
        let k = CasKey(ContentId(read_32(bytes, at)?));
        at = at.checked_add(32)?;
        result_keys.push(k);
    }

    Some(DecodedSuiteState { cert, suite_cert, tools_digest, head_key, postings_key, fns, closed_crates, result_keys })
}

pub(crate) fn encode_verified(
    suite_cert: [u8; 32],
    closed_crates_digest: [u8; 32],
    closed_pairs: &[([u8; 32], [u8; 32])],
    workspace_pairs: &[([u8; 32], [u8; 32])],
    out: &mut AVec<'_, u8>,
) {
    let closed_count = closed_pairs.len().min(MOST_VERIFIED_PERSISTED);
    let workspace_count = workspace_pairs.len().min(MOST_VERIFIED_PERSISTED);
    let header_len = 4 + 4 + 32 + 32 + 4 + 4;
    out.reserve(header_len + (closed_count + workspace_count) * 64);
    out.extend_from_slice(&VERIFIED_MAGIC);
    out.extend_from_slice(&VERIFIED_VERSION.to_le_bytes());
    out.extend_from_slice(&suite_cert);
    out.extend_from_slice(&closed_crates_digest);
    out.extend_from_slice(&u32::try_from(closed_count).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&u32::try_from(workspace_count).unwrap_or(u32::MAX).to_le_bytes());
    for &(key, under) in closed_pairs.iter().take(closed_count) {
        out.extend_from_slice(&key);
        out.extend_from_slice(&under);
    }
    for &(key, under) in workspace_pairs.iter().take(workspace_count) {
        out.extend_from_slice(&key);
        out.extend_from_slice(&under);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VerifiedDecode {
    Loaded(usize, usize),
    StaleCertificate,
    Corrupt,
}

pub(crate) fn decode_verified_status(
    bytes: &[u8],
    suite_cert: [u8; 32],
    closed_crates_digest: [u8; 32],
    into_closed: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    into_workspace: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
) -> VerifiedDecode {
    let header_len = 4 + 4 + 32 + 32 + 4 + 4;
    if bytes.len() < header_len || bytes.get(0..4) != Some(&VERIFIED_MAGIC) {
        return VerifiedDecode::Corrupt;
    }
    let Some(version_bytes) = bytes.get(4..8).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return VerifiedDecode::Corrupt;
    };
    if u32::from_le_bytes(version_bytes) != VERIFIED_VERSION {
        return VerifiedDecode::Corrupt;
    }
    let Some(stored_suite_cert) = bytes.get(8..40) else {
        return VerifiedDecode::Corrupt;
    };
    let Some(stored_closed_digest) = bytes.get(40..72) else {
        return VerifiedDecode::Corrupt;
    };
    let Some(closed_count_bytes) = bytes.get(72..76).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return VerifiedDecode::Corrupt;
    };
    let Some(ws_count_bytes) = bytes.get(76..80).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return VerifiedDecode::Corrupt;
    };
    let Ok(closed_count) = usize::try_from(u32::from_le_bytes(closed_count_bytes)) else {
        return VerifiedDecode::Corrupt;
    };
    let Ok(workspace_count) = usize::try_from(u32::from_le_bytes(ws_count_bytes)) else {
        return VerifiedDecode::Corrupt;
    };
    let Some(total_pairs) = closed_count.checked_add(workspace_count) else {
        return VerifiedDecode::Corrupt;
    };
    let Some(payload) = total_pairs.checked_mul(64) else {
        return VerifiedDecode::Corrupt;
    };
    if bytes.len().checked_sub(header_len) != Some(payload) {
        return VerifiedDecode::Corrupt;
    }
    if stored_closed_digest != closed_crates_digest.as_slice() {
        return VerifiedDecode::StaleCertificate;
    }
    let mut at = header_len;
    let mut loaded_closed = 0usize;
    for _ in 0..closed_count {
        let Some(key) = bytes.get(at..at + 32).and_then(|s| <[u8; 32]>::try_from(s).ok()) else {
            return VerifiedDecode::Corrupt;
        };
        let Some(under) = bytes.get(at + 32..at + 64).and_then(|s| <[u8; 32]>::try_from(s).ok()) else {
            return VerifiedDecode::Corrupt;
        };
        at += 64;
        if into_closed.insert((key, under)) {
            loaded_closed += 1;
        }
    }
    let mut loaded_workspace = 0usize;
    if stored_suite_cert == suite_cert.as_slice() {
        for _ in 0..workspace_count {
            let Some(key) = bytes.get(at..at + 32).and_then(|s| <[u8; 32]>::try_from(s).ok()) else {
                return VerifiedDecode::Corrupt;
            };
            let Some(under) = bytes.get(at + 32..at + 64).and_then(|s| <[u8; 32]>::try_from(s).ok()) else {
                return VerifiedDecode::Corrupt;
            };
            at += 64;
            if into_workspace.insert((key, under)) {
                loaded_workspace += 1;
            }
        }
    }
    VerifiedDecode::Loaded(loaded_closed, loaded_workspace)
}

fn decode_verified(
    bytes: &[u8],
    suite_cert: [u8; 32],
    closed_crates_digest: [u8; 32],
    into_closed: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
    into_workspace: &mut Set<([u8; 32], [u8; 32]), FixedState, Alloc<'static>>,
) -> Option<(usize, usize)> {
    match decode_verified_status(bytes, suite_cert, closed_crates_digest, into_closed, into_workspace) {
        VerifiedDecode::Loaded(closed, ws) => Some((closed, ws)),
        VerifiedDecode::StaleCertificate | VerifiedDecode::Corrupt => None,
    }
}

fn load_suite_state<'a, 'm, D: Driver>(lent: &'m Lent<D, StoreAlloc>, suite: ContentId, alloc: Alloc<'a>) -> Option<LoadedState<'a, 'm>> {
    let ptr_key = suite_ptr_key(suite);
    let state_key = lent.pointed(TestIndex::TAG, &ptr_key).ok()??;
    let lend = lent.get(&state_key).ok()??;
    if lend.tag != TestIndex::TAG {
        return None;
    }
    let decoded = decode_suite_state(lend.bytes, alloc)?;
    let head_lend = lent.get(&decoded.head_key).ok()??;
    if head_lend.tag != TestIndex::TAG {
        return None;
    }
    let postings_lend = lent.get(&decoded.postings_key).ok()??;
    if postings_lend.tag != TestPostings::TAG {
        return None;
    }
    Some(LoadedState {
        ptr_key,
        state_key,
        cert: decoded.cert,
        suite_cert: decoded.suite_cert,
        tools_digest: decoded.tools_digest,
        head_key: decoded.head_key,
        postings_key: decoded.postings_key,
        head_bytes: head_lend.bytes,
        postings_bytes: postings_lend.bytes,
        fns: decoded.fns,
        closed_crates: decoded.closed_crates,
        result_keys: decoded.result_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_symbol_classification_separates_pure_and_impure() {
        for pure in [
            "memcpy",
            "malloc",
            "free",
            "sin",
            "asinh",
            "roundeven",
            "memset_pattern16",
            "blaze_poll_page",
            "mach_absolute_time",
            "mach_continuous_time",
            "mach_approximate_time",
            "mach_timebase_info",
            "sched_yield",
            "pthread_yield_np",
            "os_sync_wait_on_address",
            "___chkstk_darwin",
            "__udivti3",
            "_Unwind_Resume",
            "__error",
            "_tlv_bootstrap",
        ] {
            assert_eq!(classify_outside_symbol(pure), Ok(()), "{pure} should be pure");
        }
        for clock in ["clock_gettime", "clock_gettime_nsec_np", "gettimeofday", "time", "nanosleep", "dispatch_time"] {
            assert_eq!(classify_outside_symbol(clock), Err(NotKept::Clock), "{clock} should be Clock");
        }
        for net in ["socket", "connect", "getaddrinfo", "sendto"] {
            assert_eq!(classify_outside_symbol(net), Err(NotKept::Network), "{net} should be Network");
        }
        assert_eq!(classify_outside_symbol("pthread_create"), Err(NotKept::Threads));
        for foreign in [
            "open",
            "openat",
            "stat",
            "fstat",
            "isatty",
            "getenv",
            "arc4random_buf",
            "getrandom",
            "posix_spawn",
            "write",
            "writev",
            "mmap",
            "sysconf",
            "custom_ffi_fn",
        ] {
            assert_eq!(classify_outside_symbol(foreign), Err(NotKept::Foreign), "{foreign} should be Foreign");
        }
    }

    #[test]
    fn prune_result_keys_keeps_all_head_rows_and_caps_undo_history() {
        let alloc = blaze_own::testing::heap().alloc();
        let mk_key = |v: u16| -> CasKey {
            let mut b = [0u8; 32];
            let le = v.to_le_bytes();
            b[0] = le[0];
            b[1] = le[1];
            CasKey(ContentId(b))
        };

        // 80 prior historical keys (0..80) + 20 newly filed keys (80..100) + 3 live head keys (200..203).
        let mut prior: AVec<'_, CasKey> = Vec::new_in(alloc);
        for i in 0u16..80 {
            prior.push(mk_key(i));
        }
        let mut newly_a: AVec<'_, CasKey> = Vec::new_in(alloc);
        for i in 80u16..100 {
            newly_a.push(mk_key(i));
        }
        let mut newly_b: AVec<'_, CasKey> = Vec::new_in(alloc);
        for i in (80u16..100).rev() {
            newly_b.push(mk_key(i));
        }
        let head = [mk_key(202), mk_key(200), mk_key(201)];

        let pruned_a = prune_result_keys(prior.as_slice(), newly_a.as_slice(), &head, MOST_EXTRA_RESULT_KEYS, alloc);
        let pruned_b = prune_result_keys(prior.as_slice(), newly_b.as_slice(), &head, MOST_EXTRA_RESULT_KEYS, alloc);
        // Rule 25: parallel completion order of `newly_filed` must produce identical output.
        assert_eq!(pruned_a.as_slice(), pruned_b.as_slice());
        assert_eq!(pruned_a.len(), MOST_EXTRA_RESULT_KEYS + head.len());
        // Oldest 36 keys (0..36) evicted; newest 64 (36..100) retained; all 3 head keys retained.
        assert!(!pruned_a.as_slice().contains(&mk_key(0)));
        assert!(!pruned_a.as_slice().contains(&mk_key(35)));
        assert!(pruned_a.as_slice().contains(&mk_key(36)));
        assert!(pruned_a.as_slice().contains(&mk_key(99)));
        for h in head {
            assert!(pruned_a.as_slice().contains(&h), "every head key must remain pinned");
        }
        // Even if Head has more than cap rows (e.g., 100 live tests), NONE of the head rows are pruned.
        let mut big_head: AVec<'_, CasKey> = Vec::new_in(alloc);
        for i in 500u16..600 {
            big_head.push(mk_key(i));
        }
        let pruned_big = prune_result_keys(prior.as_slice(), &[], big_head.as_slice(), MOST_EXTRA_RESULT_KEYS, alloc);
        assert_eq!(pruned_big.len(), MOST_EXTRA_RESULT_KEYS + big_head.len());
        for h in big_head.iter() {
            assert!(pruned_big.as_slice().contains(h));
        }
    }

    #[test]
    fn suite_state_v3_round_trips_cert_suite_cert_tools_closed_flags_and_closed_crates() {
        let alloc = blaze_own::testing::heap().alloc();
        let cert = [0x11u8; 32];
        let suite_cert = [0x12u8; 32];
        let tools = [0x22u8; 32];
        let head_key = CasKey(ContentId([0x33u8; 32]));
        let postings_key = CasKey(ContentId([0x44u8; 32]));
        let mut name0 = Vec::new_in(alloc);
        name0.extend_from_slice(&[1, 2, 3, 4]);
        let mut name1 = Vec::new_in(alloc);
        name1.extend_from_slice(&[5, 6, 7, 8, 9]);
        let mut fns = Vec::new_in(alloc);
        fns.push(FnEntry {
            symbol: FnName([0x55; 32]),
            old_key: [0x66; 32],
            now_key: Some([0x66; 32]),
            tabled: ItemKey(0x1234),
            name: name0,
            closed: true,
        });
        fns.push(FnEntry {
            symbol: FnName([0x77; 32]),
            old_key: [0x88; 32],
            now_key: Some([0x88; 32]),
            tabled: ItemKey(0x5678),
            name: name1,
            closed: false,
        });
        let kept_flags = [(0usize, true), (1usize, false)];
        let closed_crates = [([1u8; 8], [2u8; 32], [3u8; 32])];
        let results = [CasKey(ContentId([0x99u8; 32]))];

        let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
        encode_suite_state(
            cert,
            suite_cert,
            tools,
            head_key,
            postings_key,
            fns.as_slice(),
            &kept_flags,
            &closed_crates,
            &results,
            &mut bytes,
        );

        let decoded = decode_suite_state(bytes.as_slice(), alloc);
        assert!(decoded.is_some(), "valid SuiteState v3 must decode");
        if let Some(d) = decoded {
            assert_eq!(d.cert, cert);
            assert_eq!(d.suite_cert, suite_cert);
            assert_eq!(d.tools_digest, tools);
            assert_eq!(d.head_key, head_key);
            assert_eq!(d.postings_key, postings_key);
            assert_eq!(d.fns.len(), 2);
            assert_eq!(d.fns.first().map(|f| (f.symbol, f.old_key, f.closed)), Some((FnName([0x55; 32]), [0x66; 32], true)));
            assert_eq!(d.fns.get(1).map(|f| (f.symbol, f.old_key, f.closed)), Some((FnName([0x77; 32]), [0x88; 32], false)));
            assert_eq!(d.closed_crates.as_slice(), &closed_crates);
            assert_eq!(d.result_keys.as_slice(), &results);
        }

        // Planted control: v2 version byte must be rejected cleanly so old homes rebuild.
        let mut v2 = Vec::new_in(alloc);
        v2.extend_from_slice(bytes.as_slice());
        if let Some(ver) = v2.get_mut(4..8) {
            ver.copy_from_slice(&2u32.to_le_bytes());
        }
        assert!(decode_suite_state(v2.as_slice(), alloc).is_none());
    }

    #[test]
    fn check_record_purity_rejects_bad_bytes_and_aarch64_impure_instructions() {
        assert_eq!(check_record_purity(b"not a code record"), Err(NotKept::Untraced));

        let alloc = blaze_own::testing::heap().alloc();
        let build_record = |code: &[u8], outside: Option<&str>| {
            let mut rec = blaze_code::record::CodeRecord::new_in(alloc);
            rec.code.extend_from_slice(code);
            if let Some(name) = outside {
                let _ = rec.outside(name);
            }
            let sealed = rec.seal().expect("seal synthetic CodeRecord");
            let mut out = Vec::new_in(alloc);
            sealed.encode(&mut out);
            out
        };

        // Pure `ret` (0xd65f03c0) + 3 trailing rodata bytes (non-4-aligned total length).
        let pure_with_rodata = [0xc0, 0x03, 0x5f, 0xd6, b'a', b'b', b'c'];
        assert_eq!(check_record_purity(&build_record(&pure_with_rodata, None)), Ok(()));
        assert_eq!(check_record_purity(&build_record(&pure_with_rodata, Some("blaze_poll_page"))), Ok(()));
        assert_eq!(check_record_purity(&build_record(&pure_with_rodata, Some("clock_gettime"))), Err(NotKept::Clock));
        assert_eq!(check_record_purity(&build_record(&pure_with_rodata, Some("open"))), Err(NotKept::Foreign));

        if cfg!(target_arch = "aarch64") {
            // `mrs x11, tpidrro_el0` (macOS TLS) + `mrs x11, tpidr_el0` (Linux TLS) + `ret`
            let mut tls_code = Vec::new_in(alloc);
            tls_code.extend_from_slice(&0xD53B_D06Bu32.to_le_bytes());
            tls_code.extend_from_slice(&0xD53B_D04Bu32.to_le_bytes());
            tls_code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
            assert_eq!(check_record_purity(&build_record(tls_code.as_slice(), None)), Ok(()));

            // `mrs x0, cntvct_el0` -> Clock
            assert_eq!(check_record_purity(&build_record(&0xD53B_E040u32.to_le_bytes(), None)), Err(NotKept::Clock));
            // `mrs x0, rndr` -> Foreign
            assert_eq!(check_record_purity(&build_record(&0xD53B_2400u32.to_le_bytes(), None)), Err(NotKept::Foreign));
            // `svc #0x80` -> Foreign
            assert_eq!(check_record_purity(&build_record(&0xD400_1001u32.to_le_bytes(), None)), Err(NotKept::Foreign));
        }
    }

    #[test]
    fn verified_cache_v2_preserves_closed_across_workspace_edit_and_invalidates_on_closed_change() {
        let alloc = blaze_own::testing::heap().alloc();
        let suite_a = [0xaa; 32];
        let suite_edited = [0xbb; 32];
        let closed_a = [0xcc; 32];
        let closed_moved = [0xdd; 32];
        let closed_pairs = [([1u8; 32], [2u8; 32]), ([3u8; 32], [4u8; 32])];
        let ws_pairs = [([5u8; 32], [6u8; 32])];
        let mut bytes = Vec::new_in(alloc);
        encode_verified(suite_a, closed_a, &closed_pairs, &ws_pairs, &mut bytes);

        // 1. Exact match: both closed and workspace verified pairs load.
        let mut loaded_closed = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        let mut loaded_ws = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        assert_eq!(decode_verified(bytes.as_slice(), suite_a, closed_a, &mut loaded_closed, &mut loaded_ws), Some((2, 1)));
        assert!(loaded_closed.contains(&([1u8; 32], [2u8; 32])));
        assert!(loaded_closed.contains(&([3u8; 32], [4u8; 32])));
        assert!(loaded_ws.contains(&([5u8; 32], [6u8; 32])));

        // 2. Workspace edit (`suite_cert` moved, `closed_crates_digest` unchanged):
        // all closed/toolchain pairs are preserved in O(1); workspace pairs are invalidated.
        let mut after_ws_closed = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        let mut after_ws_ws = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        assert_eq!(
            decode_verified(bytes.as_slice(), suite_edited, closed_a, &mut after_ws_closed, &mut after_ws_ws),
            Some((2, 0)),
        );
        assert!(after_ws_closed.contains(&([1u8; 32], [2u8; 32])));
        assert!(after_ws_closed.contains(&([3u8; 32], [4u8; 32])));
        assert!(after_ws_ws.is_empty(), "workspace edit must invalidate workspace verified entries");

        // 3. Planted control: changing `closed_crates_digest` invalidates BOTH closed and workspace entries.
        let mut stale_closed = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        let mut stale_ws = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        assert_eq!(
            decode_verified(bytes.as_slice(), suite_a, closed_moved, &mut stale_closed, &mut stale_ws),
            None,
        );
        assert!(stale_closed.is_empty());
        assert!(stale_ws.is_empty());
    }

    #[test]
    fn decode_verified_refuses_previous_v1_layout() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut raw = Vec::new_in(alloc);
        raw.extend_from_slice(&VERIFIED_MAGIC);
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&[0xaa; 32]);
        raw.extend_from_slice(&0u32.to_le_bytes());
        let mut closed = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        let mut ws = Set::new(blaze_hash::SetOpts { hasher: FixedState, alloc, capacity: None });
        assert!(
            decode_verified(raw.as_slice(), [0xaa; 32], [0xcc; 32], &mut closed, &mut ws).is_none(),
            "v1 BZTV must be rejected so v2 rebuilds cleanly",
        );
    }

    #[test]
    fn check_datum_purity_separates_pure_and_foreign_datums() {
        assert_eq!(check_datum_purity(b"not a datum"), Err(NotKept::Untraced));

        let alloc = blaze_own::testing::heap().alloc();
        let build_datum = |outside: Option<&str>| {
            let mut d = blaze_code::datum::Datum::new_in(blaze_code::datum::Place::ReadOnly, 8, alloc);
            d.bytes.extend_from_slice(&[0u8; 8]);
            if let Some(name) = outside {
                let target = d.outside(name);
                d.words.push(blaze_code::record::Word { at: 0, target, addend: 0 });
            }
            let sealed = d.seal().expect("seal synthetic Datum");
            let mut out = Vec::new_in(alloc);
            sealed.encode(&mut out);
            out
        };

        assert_eq!(check_datum_purity(&build_datum(None)), Ok(()));
        assert_eq!(check_datum_purity(&build_datum(Some("memcpy"))), Ok(()));
        assert_eq!(check_datum_purity(&build_datum(Some("clock_gettime"))), Err(NotKept::Clock));
        assert_eq!(check_datum_purity(&build_datum(Some("secp256k1_ecdsa_verify"))), Err(NotKept::Foreign));
    }

    #[test]
    fn check_purity_rejects_weak_outside_even_when_allowlisted() {
        let alloc = blaze_own::testing::heap().alloc();
        // Pure `ret` (0xd65f03c0).
        let pure_code = 0xD65F_03C0u32.to_le_bytes();
        let build_record = |name: &str, weak: bool| {
            let mut rec = blaze_code::record::CodeRecord::new_in(alloc);
            rec.code.extend_from_slice(&pure_code);
            if weak {
                let _ = rec.weak_outside(name);
            } else {
                let _ = rec.outside(name);
            }
            let sealed = rec.seal().expect("seal synthetic CodeRecord");
            let mut out = Vec::new_in(alloc);
            sealed.encode(&mut out);
            out
        };
        let build_datum = |name: &str, weak: bool| {
            let mut d = blaze_code::datum::Datum::new_in(blaze_code::datum::Place::ReadOnly, 8, alloc);
            d.bytes.extend_from_slice(&[0u8; 8]);
            let target = if weak { d.weak_outside(name) } else { d.outside(name) };
            d.words.push(blaze_code::record::Word { at: 0, target, addend: 0 });
            let sealed = d.seal().expect("seal synthetic Datum");
            let mut out = Vec::new_in(alloc);
            sealed.encode(&mut out);
            out
        };

        // Allowlisted symbols remain cacheable under strong `Space::Outside`,
        // but MUST be rejected as unkeyed (`NotKept::Untraced`) under `Space::WeakOutside`.
        for name in ["memcpy", "blaze_poll_page", "__cxa_thread_atexit_impl"] {
            assert_eq!(
                check_record_purity(&build_record(name, false)),
                Ok(()),
                "control: strong allowlisted symbol {name} must stay cacheable in CodeRecord",
            );
            assert_eq!(
                check_record_purity(&build_record(name, true)),
                Err(NotKept::Untraced),
                "weak allowlisted symbol {name} must be rejected as unkeyed in CodeRecord",
            );
            assert_eq!(
                check_datum_purity(&build_datum(name, false)),
                Ok(()),
                "control: strong allowlisted symbol {name} must stay cacheable in Datum",
            );
            assert_eq!(
                check_datum_purity(&build_datum(name, true)),
                Err(NotKept::Untraced),
                "weak allowlisted symbol {name} must be rejected as unkeyed in Datum",
            );
        }
    }

    #[test]
    fn decode_state_refuses_previous_v1_layout() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut raw = Vec::new_in(alloc);
        raw.extend_from_slice(&STATE_MAGIC);
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        assert!(decode_suite_state(&raw, alloc).is_none(), "v1 SuiteState must be rejected so v3 rebuilds cleanly");
    }

    #[test]
    fn decode_state_refuses_previous_v2_layout() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut raw = Vec::new_in(alloc);
        raw.extend_from_slice(&STATE_MAGIC);
        raw.extend_from_slice(&2u32.to_le_bytes());
        // Full 152-byte v2 header with 0 fns, 0 results, 0 closed crates.
        raw.extend_from_slice(&[0u8; 32 * 4 + 4 * 4]);
        assert!(decode_suite_state(&raw, alloc).is_none(), "v2 SuiteState must be rejected so v3 rebuilds cleanly");
    }
}
