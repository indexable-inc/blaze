//! Incremental selective testing cache for `blaze test` (docs/v2/plan/design/test.md, "End state").
//!
//! Persists per-suite test heads (`BZTH`, `TestIndex`), postings (`BZPS`, `TestPostings`),
//! read-addressed result records (`BZTR`, `TestRun`), and the entered-function identity map
//! (`SuiteState`) in the command's CAS store (`blaze_cas::plain::Lent`), rooted under
//! `test.<suite_id>` (`store/roots.rs::keep_test_suite`).
//!
//! Soundness (rules 4, 21, 25):
//! - A test pass is kept ONLY when it ran sandboxed, harnessed, with no native C library,
//!   every entered Rust function and static was keyed (`CodeKey`), and every entered code
//!   record passed [`check_record_purity`] (no impure foreign C symbols, no raw `svc`/system-reg
//!   instructions). Any test that touches wall clock, filesystem, env, randomness, network,
//!   threads, or unknown foreign code is marked `NotKept` by name and always reruns.
//! - On a subsequent run, every previously entered function's current [`CodeKey`] is recomputed
//!   from `RunIds` via [`blaze_ladder::body_ids::instance_key`] (pure hashing, zero compilation).
//!   Unchanged tests are reused via `blaze-run::plan::Plan`'s coarse proof without running;
//!   changed functions trigger forward postings lookup (`Postings`), and undone edits hit
//!   previously filed `BZTR` passes via `Effect::Look`.

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{Lent, LentError, Namespace};
use blaze_code::record::{CodeView, Space, Symbol as CodeSymbol};
use blaze_core::Driver;
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::kind::{Kind as _, TestIndex, TestPostings, TestRun};
use blaze_ladder::body_ids::instance_key;
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
const HEAD_KEY_DOMAIN: &str = "blaze test head record 1";
const POSTINGS_KEY_DOMAIN: &str = "blaze test postings record 1";
const RESULT_SLOT_DOMAIN: &str = "blaze test result slot 1";
const TEST_ID_DOMAIN: &str = "blaze test: package target and test 2";
const ADDRESS_DOMAIN: &str = "blaze test address of traced run 1";

const STATE_MAGIC: [u8; 4] = *b"BZTS";
const STATE_VERSION: u32 = 1;

/// Typed store error from persisting incremental test cache records (rule 19).
#[derive(Debug)]
pub(crate) enum CacheError {
    Store(LentError),
    Root(RootError),
}

impl core::fmt::Display for CacheError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CacheError::Store(e) => core::fmt::Display::fmt(e, f),
            CacheError::Root(e) => core::fmt::Display::fmt(e, f),
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
        if target.space == Space::Outside {
            let Some(name) = view.name(target.symbol) else {
                return Err(NotKept::Foreign);
            };
            classify_outside_symbol(name)?;
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

fn classify_outside_symbol(raw: &str) -> Result<(), NotKept> {
    match raw {
        // Wall clock, monotonic time reads, and sleeps.
        "clock_gettime"
        | "gettimeofday"
        | "time"
        | "mach_absolute_time"
        | "mach_continuous_time"
        | "mach_approximate_time"
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

        // Provably pure memory, allocator, IEEE-754 math, compiler-rt builtins, single-process sync/TLS, and unwind hooks.
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
        | "strcmp"
        | "strncmp"
        | "__memcpy_chk"
        | "__memmove_chk"
        | "__memset_chk"
        | "malloc"
        | "calloc"
        | "realloc"
        | "free"
        | "posix_memalign"
        | "aligned_alloc"
        | "memalign"
        | "malloc_usable_size"
        | "malloc_size"
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
        | "floor"
        | "ceil"
        | "trunc"
        | "round"
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
        | "floorf"
        | "ceilf"
        | "truncf"
        | "roundf"
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
        | "__udivti3"
        | "__divti3"
        | "__modti3"
        | "__umodti3"
        | "__multi3"
        | "__muloti4"
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
        | "__floatsitf"
        | "__floatditf"
        | "__floattitf"
        | "__floatunsitf"
        | "__floatunditf"
        | "__floatuntitf"
        | "__extendsftf2"
        | "__extenddftf2"
        | "__trunctfsf2"
        | "__trunctfdf2"
        | "__powisf2"
        | "__powidf2"
        | "__powitf2"
        | "_tlv_bootstrap"
        | "_tlv_atexit"
        | "__cxa_thread_atexit_impl"
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
        | "pthread_mutexattr_destroy"
        | "pthread_rwlock_init"
        | "pthread_rwlock_rdlock"
        | "pthread_rwlock_wrlock"
        | "pthread_rwlock_unlock"
        | "pthread_rwlock_destroy"
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

struct FnEntry<'a> {
    symbol: FnName,
    old_key: [u8; 32],
    now_key: Option<[u8; 32]>,
    tabled: ItemKey,
    name: AVec<'a, u8>,
}

struct SuiteSlot<'a, 'm> {
    id: ContentId,
    head_bytes: Option<&'m [u8]>,
    postings_bytes: Option<&'m [u8]>,
    fns: AVec<'a, FnEntry<'a>>,
    result_keys: AVec<'a, CasKey>,
    new_head: Option<(CasKey, AVec<'a, u8>)>,
    new_postings: Option<CasKey>,
    dirty: bool,
}

/// CAS-backed incremental cache for all suites in one `blaze test` invocation.
pub(crate) struct TestCache<'a, 'm, D: Driver> {
    lent: &'m Lent<D, StoreAlloc>,
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
        suite_specs: &[(TestUnit<'_>, Builder)],
        ids: &RunIds<'_>,
        alloc: Alloc<'a>,
    ) -> (TestCache<'a, 'm, D>, AVec<'a, Head<'m>>, AVec<'a, Postings<'m>>, AVec<'a, Ask>) {
        let mut suites = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut heads = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut postings_list = Vec::with_capacity_in(suite_specs.len(), alloc);
        let mut moved = Vec::new_in(alloc);

        for (unit, builder) in suite_specs {
            let sid = suite_id(*unit, builder);
            let mut slot = SuiteSlot {
                id: sid,
                head_bytes: None,
                postings_bytes: None,
                fns: Vec::new_in(alloc),
                result_keys: Vec::new_in(alloc),
                new_head: None,
                new_postings: None,
                dirty: false,
            };
            let mut head = Head::EMPTY;
            let mut postings = Postings::EMPTY;

            if let Some(loaded) = load_suite_state(lent, sid, alloc)
                && let Ok(opened_head) = Head::open(loaded.head_bytes)
                && let Ok(opened_postings) = Postings::open(loaded.postings_bytes)
            {
                slot.head_bytes = Some(loaded.head_bytes);
                slot.postings_bytes = Some(loaded.postings_bytes);
                slot.result_keys = loaded.result_keys;
                for mut f in loaded.fns {
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
        (TestCache { lent, suites, owners: Vec::new_in(alloc), alloc }, heads, postings_list, moved)
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

    /// Records the walked function metadata observed during a test run so [`Answers::answer`] and
    /// [`Self::commit`] know every entered function's identity and `instance_name`.
    pub(crate) fn record_entered(&mut self, suite: SuiteId, entered: &[EnteredFn<'a>]) {
        let Ok(idx) = usize::try_from(suite.0) else { return };
        let Some(slot) = self.suites.get_mut(idx) else { return };
        for e in entered {
            match slot.fns.binary_search_by_key(&e.symbol, |row| row.symbol) {
                Ok(at) => {
                    if let Some(entry) = slot.fns.get_mut(at) {
                        entry.now_key = Some(e.code_key);
                        entry.old_key = e.code_key;
                        entry.tabled = e.tabled;
                        entry.name.clear();
                        entry.name.extend_from_slice(e.name.as_slice());
                    }
                }
                Err(at) => {
                    let mut name = Vec::with_capacity_in(e.name.len(), self.alloc);
                    name.extend_from_slice(e.name.as_slice());
                    slot.fns.insert_shift(at, FnEntry {
                        symbol: e.symbol,
                        old_key: e.code_key,
                        now_key: Some(e.code_key),
                        tabled: e.tabled,
                        name,
                    });
                }
            }
        }
    }

    /// Answers `Effect::Look`: recomputes `test`'s address from its previous head row's asks
    /// answered with today's code identities, and checks whether a `BZTR` pass exists there.
    pub(crate) fn look(&self, suite: SuiteId, test: TestId) -> Result<bool, DriveError> {
        let Ok(idx) = usize::try_from(suite.0) else { return Ok(false) };
        let Some(slot) = self.suites.get(idx) else { return Ok(false) };
        let Some(head_bytes) = slot.head_bytes else { return Ok(false) };
        let Ok(head) = Head::open(head_bytes) else { return Ok(false) };
        let Some(row) = head.row(test) else { return Ok(false) };
        let mut pairs: AVec<'_, (FnName, [u8; 32])> = Vec::new_in(self.alloc);
        for ask in head.asks(row) {
            match ask {
                Ask::Fn(name) => {
                    let Ok(at) = slot.fns.binary_search_by_key(&name, |f| f.symbol) else {
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

    /// Commits every dirty suite's `SuiteState`, pointer, and root into `Lent` (flushed to disk
    /// by the command's single post-answer commit).
    pub(crate) fn commit(mut self) -> Result<(), DriveError> {
        for slot in self.suites.iter_mut() {
            if !slot.dirty {
                continue;
            }
            let (head_key, head_bytes_slice): (CasKey, &[u8]) = match &slot.new_head {
                Some((key, bytes)) => (*key, bytes.as_slice()),
                None => continue,
            };
            let Some(postings_key) = slot.new_postings else { continue };
            // Keep only `FnEntry` rows still referenced by the new head, and collect live result keys.
            let mut live_fns: AVec<'_, FnName> = Vec::new_in(self.alloc);
            let mut live_results: AVec<'_, CasKey> = Vec::new_in(self.alloc);
            live_results.extend_from_slice(slot.result_keys.as_slice());
            if let Ok(head) = Head::open(head_bytes_slice) {
                for row in head.rows() {
                    if let Last::Passed { address } | Last::Unsettled { address } = row.kept.last {
                        live_results.push(result_slot_key(address, Slot::Pass));
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
            live_results.sort_unstable();
            live_results.dedup();

            // Encode SuiteState deterministically.
            let mut state_bytes: AVec<'_, u8> = Vec::new_in(self.alloc);
            state_bytes.extend_from_slice(&STATE_MAGIC);
            state_bytes.extend_from_slice(&STATE_VERSION.to_le_bytes());
            state_bytes.extend_from_slice(&head_key.0.0);
            state_bytes.extend_from_slice(&postings_key.0.0);
            let kept_fns: AVec<'_, usize> = {
                let mut v = Vec::new_in(self.alloc);
                for (i, f) in slot.fns.iter().enumerate() {
                    if live_fns.binary_search(&f.symbol).is_ok() && f.now_key.is_some() {
                        v.push(i);
                    }
                }
                v
            };
            let fn_count = u32::try_from(kept_fns.len()).unwrap_or(u32::MAX);
            let res_count = u32::try_from(live_results.len()).unwrap_or(u32::MAX);
            state_bytes.extend_from_slice(&fn_count.to_le_bytes());
            state_bytes.extend_from_slice(&res_count.to_le_bytes());
            for &i in kept_fns.iter() {
                let Some(f) = slot.fns.get(i) else { continue };
                let Some(key) = f.now_key else { continue };
                state_bytes.extend_from_slice(&f.symbol.0);
                state_bytes.extend_from_slice(&key);
                state_bytes.extend_from_slice(&f.tabled.0.to_le_bytes());
                let nlen = u32::try_from(f.name.len()).unwrap_or(u32::MAX);
                state_bytes.extend_from_slice(&nlen.to_le_bytes());
                state_bytes.extend_from_slice(f.name.as_slice());
            }
            for rk in live_results.iter() {
                state_bytes.extend_from_slice(&rk.0.0);
            }

            let state_key = CasKey(ContentId::of(SUITE_STATE_DOMAIN, state_bytes.as_slice()));
            let ptr_key = suite_ptr_key(slot.id);
            let store_err = |e: LentError| DriveError::CacheStore { what: "writing test suite cache", why: CacheError::Store(e) };
            let root_err = |e: RootError| DriveError::CacheStore { what: "pinning test suite root", why: CacheError::Root(e) };
            self.lent.put(&state_key, Namespace::LOCAL, TestIndex::TAG, state_bytes.as_slice()).map_err(store_err)?;
            self.lent.point(TestIndex::TAG, &ptr_key, &state_key).map_err(store_err)?;

            let mut pinned: AVec<'_, CasKey> = Vec::with_capacity_in(4 + live_results.len(), self.alloc);
            pinned.push(ptr_key);
            pinned.push(state_key);
            pinned.push(head_key);
            pinned.push(postings_key);
            pinned.extend_from_slice(live_results.as_slice());
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
            if let Ok(at) = slot.fns.binary_search_by_key(&name, |f| f.symbol)
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
    fn file(&mut self, suite: SuiteId, _test: TestId, address: Address, slot: Slot, bytes: &[u8]) -> Result<(), DriveError> {
        let key = result_slot_key(address, slot);
        let store_err = |e: LentError| DriveError::CacheStore { what: "writing test result record", why: CacheError::Store(e) };
        self.lent.put(&key, Namespace::LOCAL, TestRun::TAG, bytes).map_err(store_err)?;
        if let Some(s) = usize::try_from(suite.0).ok().and_then(|i| self.suites.get_mut(i)) {
            s.result_keys.push(key);
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
    head_bytes: &'m [u8],
    postings_bytes: &'m [u8],
    fns: AVec<'a, FnEntry<'a>>,
    result_keys: AVec<'a, CasKey>,
}

fn load_suite_state<'a, 'm, D: Driver>(lent: &'m Lent<D, StoreAlloc>, suite: ContentId, alloc: Alloc<'a>) -> Option<LoadedState<'a, 'm>> {
    let ptr_key = suite_ptr_key(suite);
    let state_key = lent.pointed(TestIndex::TAG, &ptr_key).ok()??;
    let lend = lent.get(&state_key).ok()??;
    if lend.tag != TestIndex::TAG {
        return None;
    }
    let read_u32 = |buf: &[u8], off: usize| -> Option<u32> {
        let slice: [u8; 4] = buf.get(off..off.checked_add(4)?)?.try_into().ok()?;
        Some(u32::from_le_bytes(slice))
    };
    let read_u128 = |buf: &[u8], off: usize| -> Option<u128> {
        let slice: [u8; 16] = buf.get(off..off.checked_add(16)?)?.try_into().ok()?;
        Some(u128::from_le_bytes(slice))
    };
    let read_32 = |buf: &[u8], off: usize| -> Option<[u8; 32]> {
        buf.get(off..off.checked_add(32)?)?.try_into().ok()
    };

    let bytes = lend.bytes;
    let header_len = 4 + 4 + 32 + 32 + 4 + 4;
    if bytes.len() < header_len || bytes.get(0..4) != Some(&STATE_MAGIC) {
        return None;
    }
    let version = read_u32(bytes, 4)?;
    if version != STATE_VERSION {
        return None;
    }
    let head_key = CasKey(ContentId(read_32(bytes, 8)?));
    let postings_key = CasKey(ContentId(read_32(bytes, 40)?));
    let fn_count = usize::try_from(read_u32(bytes, 72)?).ok()?;
    let res_count = usize::try_from(read_u32(bytes, 76)?).ok()?;

    let mut at = header_len;
    let mut fns = Vec::with_capacity_in(fn_count, alloc);
    for _ in 0..fn_count {
        let fixed = 32 + 32 + 16 + 4;
        if bytes.len().checked_sub(at)? < fixed {
            return None;
        }
        let symbol = FnName(read_32(bytes, at)?);
        let old_key = read_32(bytes, at.checked_add(32)?)?;
        let tabled = ItemKey(read_u128(bytes, at.checked_add(64)?)?);
        let nlen = usize::try_from(read_u32(bytes, at.checked_add(80)?)?).ok()?;
        at = at.checked_add(fixed)?;
        let end = at.checked_add(nlen)?;
        let name_slice = bytes.get(at..end)?;
        let mut name = Vec::with_capacity_in(nlen, alloc);
        name.extend_from_slice(name_slice);
        at = end;
        fns.push(FnEntry { symbol, old_key, now_key: None, tabled, name });
    }
    let rem_needed = res_count.checked_mul(32)?;
    if bytes.len().checked_sub(at)? != rem_needed {
        return None;
    }
    let mut result_keys = Vec::with_capacity_in(res_count, alloc);
    for _ in 0..res_count {
        let k = CasKey(ContentId(read_32(bytes, at)?));
        at = at.checked_add(32)?;
        result_keys.push(k);
    }

    let head_lend = lent.get(&head_key).ok()??;
    if head_lend.tag != TestIndex::TAG {
        return None;
    }
    let postings_lend = lent.get(&postings_key).ok()??;
    if postings_lend.tag != TestPostings::TAG {
        return None;
    }
    Some(LoadedState { head_bytes: head_lend.bytes, postings_bytes: postings_lend.bytes, fns, result_keys })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outside_symbol_classification_separates_pure_and_impure() {
        for pure in ["memcpy", "malloc", "free", "sin", "blaze_poll_page", "__udivti3", "_Unwind_Resume", "__error", "_tlv_bootstrap"] {
            assert_eq!(classify_outside_symbol(pure), Ok(()), "{pure} should be pure");
        }
        for clock in ["clock_gettime", "gettimeofday", "mach_absolute_time", "nanosleep", "dispatch_time"] {
            assert_eq!(classify_outside_symbol(clock), Err(NotKept::Clock), "{clock} should be Clock");
        }
        for net in ["socket", "connect", "getaddrinfo", "sendto"] {
            assert_eq!(classify_outside_symbol(net), Err(NotKept::Network), "{net} should be Network");
        }
        assert_eq!(classify_outside_symbol("pthread_create"), Err(NotKept::Threads));
        for foreign in ["open", "openat", "stat", "getenv", "arc4random_buf", "getrandom", "posix_spawn", "write", "custom_ffi_fn"] {
            assert_eq!(classify_outside_symbol(foreign), Err(NotKept::Foreign), "{foreign} should be Foreign");
        }
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
}
