//! Hermetic pinned `clang-23` compiler probe runner + content-addressed probe cache
//! (`c-probe/<64-hex>`), implementing [`blaze_build_script::c_tasks_probe::ProbeAnswerer`]
//! (rule 78, D174/D177).
//!
//! - Single answerer + single cache: on supported hosts with the pinned `clang-23` shelf unpacked
//!   (`<home>/.blaze/v2/toolchain/clang/<archive sha256>/bin/clang-23`), every probe miss is
//!   executed hermetically via `bstd::procs::hermetic::Hermetic` at `Level::Inputs` (Landlock +
//!   seccomp on Linux, Seatbelt on macOS) with `-fintegrated-cc1 --no-default-config` and only
//!   the probe's own `OUT_DIR` inputs laid in a fresh task folder.
//! - No silent answerer switching (rule 3): if the host has no clang pin in `pins/clang.json` or
//!   the shelf `clang-23` binary is missing, [`open_command_c_probes`] returns a named refusal
//!   retained as [`CProbeSetting::Unavailable`] so unrelated build scripts still run, while any
//!   script issuing a C compiler probe fails explicitly naming what is missing.
//! - Deferred binary verification: `clang-23` is NOT stream-hashed on command startup or on cache
//!   hits; SHA-256 verification against `pins/clang.json`'s `shelf_programs` runs lazily on the
//!   first cache miss and is recorded in a per-boot file-metadata stamp voucher so subsequent
//!   commands in the same boot with unchanged file metadata verify in one `stat()`.
//! - Every resolved probe's canonical answer is cached in [`CProbeStore`] (`DirStore` under
//!   `c-probe/<64-hex>` or [`MemCProbeStore`] in tests), and [`c_probe_reply_now`] revalidates
//!   `cc-probe\n<answerer>\n<clang>\n<key>` against BOTH the active `(answerer_id, clang_sha256)`
//!   and the cached canonical answer without hashing any table into every script's base key.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use std::{fs, path::{Path, PathBuf}, process, sync::Mutex};

use blaze_build_script::c_tasks_probe::{
    self, CanonicalProbe, MOUNT, OUT_DIR, ProbeAnswer, ProbeAnswerer, ProbeCache, ProbeKey, SDKROOT, Segment,
    canonical_reply_digest, decode_answer,
};
use blaze_build_script::facts;
use blaze_build_script::shelf_pins;
use blaze_build_script::toolchain;
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Arc as OwnArc, SharedAlloc, String as OwnString, Vec as OwnVec};
use blaze_pins::CLANG_JSON;
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
use bstd::procs::{
    Exit,
    hermetic::{
        Budget, Capture, ClockSpec, Fault, FilesError, FilesOp, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
        HermeticStreams, Level, Program, ReportSpec, RootFiles,
    },
};

use crate::record::{DirStore, FilesError as StoreFilesError, native_world};

type SharedStr = OwnString<SharedAlloc<'static>>;

/// Strategy id for [`HermeticClangProbe`] (hashed into [`ProbeKey`] and `cc-probe` read subjects).
const HERMETIC_STRATEGY_DOMAIN: &str = "blaze-build-script c-probe shelf clang 1";

/// Maximum captured stdout or stderr bytes for one compiler probe.
const PROBE_MOST_STREAM: u32 = 1 << 20;

/// Maximum encoded + sealed `c-probe/<64-hex>` record size accepted from disk (`2 * 1 MiB`
/// streams + binary header + 32-byte seal + margin).
pub const MAX_RECORD_BYTES: u64 = 2 * (1 << 20) + 4096;

/// Per-probe wall and CPU deadline (rule 18).
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Virtual address-space cap for `clang-23` under `RLIMIT_AS` (342 MB binary + shared libs + LLVM).
const PROBE_MEMORY_BYTES: u64 = 8 << 30;

/// Domain tag sealing the per-boot `clang-23` metadata verification voucher.
const VERIFY_VOUCHER_DOMAIN: &str = "blaze c-probe clang verified voucher 1";

/// Monotonic sequence counter for per-probe isolated task folders.
static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Persistent content-addressed store of encoded `c_tasks_probe` records (`bzcp` v1).
pub trait CProbeStore: Send + Sync {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, CProbeStoreError>;
    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), CProbeStoreError>;
}

/// Why a [`CProbeStore`] operation failed.
#[derive(Debug)]
pub enum CProbeStoreError {
    Files(StoreFilesError),
    Twice(ProbeKey),
}

impl fmt::Display for CProbeStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CProbeStoreError::Files(error) => write!(f, "the c-probe records: {error}"),
            CProbeStoreError::Twice(key) => {
                let prefix = key.0.0.get(..8).unwrap_or(&[]);
                write!(f, "another answer is already held under c-probe {prefix:02x?} (rule 21)")
            }
        }
    }
}

impl CProbeStore for DirStore {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, CProbeStoreError> {
        self.get_c_probe(&key.0, alloc).map_err(CProbeStoreError::Files)
    }

    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), CProbeStoreError> {
        self.put_c_probe(&key.0, bytes).map_err(CProbeStoreError::Files)
    }
}

/// In-memory [`CProbeStore`] for tests and isolated runs.
pub struct MemCProbeStore {
    rows: Mutex<OwnVec<(ProbeKey, OwnVec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>>,
}

impl Default for MemCProbeStore {
    fn default() -> Self {
        Self { rows: Mutex::new(OwnVec::new_in(SharedAlloc::global())) }
    }
}

impl MemCProbeStore {
    pub fn len(&self) -> usize {
        crate::host::lock(&self.rows).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replaces the stored answer for `key` (test helper for rule 23 selective-invalidation proofs).
    #[cfg(test)]
    pub fn overwrite(&self, key: ProbeKey, bytes: &[u8]) {
        let mut owned = OwnVec::with_capacity_in(bytes.len(), SharedAlloc::global());
        owned.extend_from_slice(bytes);
        let mut rows = crate::host::lock(&self.rows);
        if let Some((_, held)) = rows.iter_mut().find(|(have, _)| *have == key) {
            *held = owned;
        } else {
            rows.push((key, owned));
        }
    }
}

impl CProbeStore for MemCProbeStore {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, CProbeStoreError> {
        let rows = crate::host::lock(&self.rows);
        Ok(rows.iter().find(|(have, _)| have == key).map(|(_, bytes)| {
            let mut out = AVec::with_capacity_in(bytes.len(), alloc);
            out.extend_from_slice(bytes.as_slice());
            out
        }))
    }

    fn put(&self, key: &ProbeKey, bytes: &[u8]) -> Result<(), CProbeStoreError> {
        let mut rows = crate::host::lock(&self.rows);
        match rows.iter().find(|(have, _)| have == key) {
            Some((_, held)) if held.as_slice() == bytes => Ok(()),
            Some(_) => Err(CProbeStoreError::Twice(*key)),
            None => {
                let mut owned = OwnVec::with_capacity_in(bytes.len(), SharedAlloc::global());
                owned.extend_from_slice(bytes);
                rows.push((*key, owned));
                Ok(())
            }
        }
    }
}

/// Adapter bridging [`CProbeStore`] to [`c_tasks_probe::ProbeCache`].
pub(crate) struct CacheAdapter<'s>(pub(crate) &'s dyn CProbeStore);

impl ProbeCache for CacheAdapter<'_> {
    fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<AVec<'a, u8>>, AString<'a>> {
        self.0.get(key, alloc).map_err(|e| own_fmt(format_args!("{e}"), alloc))
    }

    fn put<'a>(&self, key: &ProbeKey, bytes: &[u8], alloc: Alloc<'a>) -> Result<(), AString<'a>> {
        self.0.put(key, bytes).map_err(|e| own_fmt(format_args!("{e}"), alloc))
    }
}

/// Shared C compiler probe configuration carried on `Script` and `script_unit::Setting`.
#[derive(Clone)]
pub struct CScriptProbes {
    pub answerer: OwnArc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>>,
    pub records: OwnArc<dyn CProbeStore, SharedAlloc<'static>>,
    /// SDK / sysroot identity hashed into every [`ProbeKey`] (`""` on Linux with no sysroot).
    pub sdk_id: OwnArc<str, SharedAlloc<'static>>,
    /// Digest of `sdk_id` (`facts::sdk_id_digest`) verified in `cc-probe` read revalidation.
    pub sdk_digest: [u8; 32],
    /// Host pinned clang archive SHA-256 (verified in `cc-probe` read revalidation).
    pub clang_sha256: [u8; 32],
}

impl blaze_own::Share for CScriptProbes {}

/// Command-level C probe state: either ready with a verified hermetic runner and CAS store, or
/// a retained refusal (`Unavailable`) that fails ONLY scripts that actually issue a C compiler
/// probe (never blocking unrelated build scripts on hosts without a clang pin or unpacked shelf).
#[derive(Clone)]
pub enum CProbeSetting {
    Ready(CScriptProbes),
    Unavailable(OwnArc<str, SharedAlloc<'static>>),
}

impl blaze_own::Share for CProbeSetting {}

impl CProbeSetting {
    pub fn ready(&self) -> Option<&CScriptProbes> {
        match self {
            CProbeSetting::Ready(probes) => Some(probes),
            CProbeSetting::Unavailable(_) => None,
        }
    }
}

/// Hermetic pinned `clang-23` compiler probe runner (`bstd::procs::hermetic::Hermetic` at
/// `Level::Inputs`).
pub struct HermeticClangProbe {
    host: SharedStr,
    clang_path: SharedStr,
    shelf_dir: SharedStr,
    shelf_digest: [u8; 32],
    sdk_dir: Option<SharedStr>,
    sdk_digest: Option<[u8; 32]>,
    work_root: SharedStr,
    deadline: Duration,
    verified: AtomicBool,
    verify_lock: bstd::sync::Lock<()>,
}

impl HermeticClangProbe {
    /// Strategy id of this answerer, derived directly from the implementation source bytes of
    /// `probe_clang.rs`, `c_tasks_probe.rs`, and the OS confinement backends (AGENTS.md rule 69)
    /// so any runner or canonicalization change automatically rolls [`ProbeKey`] and invalidates
    /// prior `cc-probe` read subjects.
    pub fn strategy_id() -> ContentId {
        let mut h = blaze_ids::Hasher::new(HERMETIC_STRATEGY_DOMAIN);
        h.part(include_bytes!("probe_clang.rs")).id(&c_tasks_probe::implementation_id());
        for part in bstd::procs::hermetic::CONFINEMENT_SOURCE {
            h.part(part);
        }
        h.finish()
    }

    /// Opens [`HermeticClangProbe`] for `host` from `<home>/.blaze/v2/toolchain/clang/<sha256>`.
    /// Does NOT stream-hash the ~342 MB `clang-23` binary on startup; verification runs lazily on
    /// the first cache miss (guarded by a per-boot file-metadata voucher).
    /// Returns a named refusal if `host` has no pin in `pins/clang.json`, no `shelf_programs` row
    /// for `clang-23`, or the shelf binary is missing on disk (rule 3: never silently switches
    /// answerers).
    pub fn open_from_shelf<'a>(
        home: &str,
        host: &str,
        sdk: Option<(&str, &str)>,
        work_root: PathBuf,
        alloc: Alloc<'a>,
    ) -> Result<(HermeticClangProbe, [u8; 32]), AString<'a>> {
        let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let shelf_hex = hex_bytes(&pin.sha256, alloc);
        let shelf_path = Path::new(home).join(crate::native_setting::SHELF_DIR).join(shelf_hex.as_str());
        let prog = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23").map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let clang_candidate = shelf_path.join(prog.shelf_path);
        if !clang_candidate.is_file() {
            return Err(own_fmt(
                format_args!(
                    "pinned clang-23 shelf binary missing at {} (expected archive sha256 {shelf_hex} from pins/clang.json)",
                    clang_candidate.display()
                ),
                alloc,
            ));
        }
        let shelf_real = canonical_utf8(&shelf_path, alloc)?;
        let clang_real = canonical_utf8(&clang_candidate, alloc)?;
        let (sdk_dir, sdk_digest) = match sdk {
            Some((dir, id)) if !dir.is_empty() => {
                let real = canonical_utf8(Path::new(dir), alloc)?;
                let dig = ContentId::of("blaze c-probe sdk root 1", id.as_bytes()).0;
                (Some(real), Some(dig))
            }
            _ => (None, None),
        };
        fs::create_dir_all(&work_root).map_err(|e| own_fmt(format_args!("{}: {e}", work_root.display()), alloc))?;
        let work_real = canonical_utf8(&work_root, alloc)?;
        let probe = HermeticClangProbe {
            host: OwnString::from_str_in(host, SharedAlloc::global()),
            clang_path: clang_real,
            shelf_dir: shelf_real,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            work_root: work_real,
            deadline: PROBE_DEADLINE,
            verified: AtomicBool::new(false),
            verify_lock: bstd::sync::Lock::new("c-probe.clang.verify", ()),
        };
        Ok((probe, pin.sha256))
    }

    /// Opens [`HermeticClangProbe`] directly from an explicit shelf folder (for differential tests).
    pub fn from_shelf_dir<'a>(
        shelf_dir: &Path,
        host: &str,
        sdk: Option<(&Path, &str)>,
        work_root: PathBuf,
        alloc: Alloc<'a>,
    ) -> Result<HermeticClangProbe, AString<'a>> {
        let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let prog = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23").map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let clang_candidate = shelf_dir.join(prog.shelf_path);
        if !clang_candidate.is_file() {
            return Err(own_fmt(format_args!("pinned clang-23 binary missing at {}", clang_candidate.display()), alloc));
        }
        let shelf_real = canonical_utf8(shelf_dir, alloc)?;
        let clang_real = canonical_utf8(&clang_candidate, alloc)?;
        let (sdk_dir, sdk_digest) = match sdk {
            Some((dir, id)) => {
                let real = canonical_utf8(dir, alloc)?;
                let dig = ContentId::of("blaze c-probe sdk root 1", id.as_bytes()).0;
                (Some(real), Some(dig))
            }
            None => (None, None),
        };
        fs::create_dir_all(&work_root).map_err(|e| own_fmt(format_args!("{}: {e}", work_root.display()), alloc))?;
        let work_real = canonical_utf8(&work_root, alloc)?;
        Ok(HermeticClangProbe {
            host: OwnString::from_str_in(host, SharedAlloc::global()),
            clang_path: clang_real,
            shelf_dir: shelf_real,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            work_root: work_real,
            deadline: PROBE_DEADLINE,
            verified: AtomicBool::new(false),
            verify_lock: bstd::sync::Lock::new("c-probe.clang.verify", ()),
        })
    }

    /// Verifies `clang-23`'s SHA-256 against `pins/clang.json` lazily on the first cache miss,
    /// strictly bounded by `clock`/`began`/`self.deadline`:
    /// - Concurrent misses park on the scheduler-aware `bstd::sync::Lock` bound to `remaining()`
    ///   (never blocking worker threads in a host `Condvar`).
    /// - Fast path checks a sealed per-boot metadata stamp voucher through deadline-aware
    ///   `bstd::fs` so repeat commands in the same boot with unchanged metadata do not re-read
    ///   342 MB.
    /// - Cold path streams `prog.shelf_path` through deadline-aware `bstd::fs` effects bound to
    ///   the remaining probe budget.
    fn ensure_verified<'a>(
        &self,
        work_io: &bstd::io::Io,
        work_root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
        clock: &bstd::clock::Clock,
        began: bstd::clock::Instant,
        boot: Option<blaze_core::Boot>,
        alloc: Alloc<'a>,
    ) -> Result<(), AString<'a>> {
        if self.verified.load(Ordering::Acquire) {
            return Ok(());
        }
        let remaining = || -> Option<Duration> {
            let elapsed = clock.now().since(began).unwrap_or(Duration::ZERO);
            self.deadline.checked_sub(elapsed).filter(|d| !d.is_zero())
        };
        let Some(rem) = remaining() else {
            return Err(own_str("c-probe clang verification timed out before lock", alloc));
        };
        let lock_cx = work_io
            .cx(rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe verify lock context stopped", alloc))?;
        let _guard = self
            .verify_lock
            .lock(&lock_cx)
            .map_err(|e| own_fmt(format_args!("c-probe clang verification timed out waiting on lock: {e}"), alloc))?;
        if self.verified.load(Ordering::Acquire) {
            return Ok(());
        }

        let prog = shelf_pins::shelf_program(CLANG_JSON, self.host.as_str(), "clang-23")
            .map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let Some(rem) = remaining() else {
            return Err(own_str("c-probe clang verification timed out before starting", alloc));
        };
        let (shelf_io, shelf_root) = native_world(Path::new(self.shelf_dir.as_str()))
            .map_err(|e| own_fmt(format_args!("c-probe shelf world: {e}"), alloc))?;
        let shelf_cx = shelf_io
            .cx(rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe verify context stopped", alloc))?;
        let clang_rel = bstd::path::Path::new(prog.shelf_path)
            .map_err(|_| own_fmt(format_args!("invalid shelf path `{}`", prog.shelf_path), alloc))?;
        let expected_voucher = match (boot, shelf_root.stat(&shelf_cx, &clang_rel)) {
            (Some(b), Ok(Some(meta))) => Some(verify_voucher_digest(b, &meta, &prog.sha256)),
            _ => None,
        };
        let voucher_name = own_fmt(format_args!("verified-{}.stamp", hex_bytes(&prog.sha256, alloc)), alloc);
        if let Some(expected) = expected_voucher
            && let Some(rem_v) = remaining()
            && let Ok(work_cx) = work_io.cx(rem_v, bstd::mem::TASK_FRAME)
            && let Ok(voucher_rel) = bstd::path::Path::new(voucher_name.as_str())
            && read_voucher_bounded(work_root, &work_cx, &voucher_rel) == Some(expected.0)
        {
            self.verified.store(true, Ordering::Release);
            return Ok(());
        }
        let Some(rem) = remaining() else {
            return Err(own_str("c-probe clang verification timed out before hashing", alloc));
        };
        let hash_cx = shelf_io
            .cx(rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe verify hash context stopped", alloc))?;
        let got_sha = file_sha256_bounded_on(&shelf_root, &hash_cx, &clang_rel, prog.shelf_path, alloc)?;
        shelf_pins::check_member(self.host.as_str(), prog, got_sha).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        if let Some(expected) = expected_voucher
            && let Some(rem_w) = remaining()
            && let Ok(pub_cx) = work_io.cx(rem_w, bstd::mem::TASK_FRAME)
            && let Ok(voucher_rel) = bstd::path::Path::new(voucher_name.as_str())
        {
            let _ = work_root.publish(
                &pub_cx,
                &voucher_rel,
                bstd::fs::Content::bytes(&expected.0),
                bstd::fs::Durable::No,
            );
        }
        self.verified.store(true, Ordering::Release);
        Ok(())
    }
}

/// Computes a per-boot file-metadata stamp digest binding `(boot_id, expected_sha256, dev, ino,
/// size, modified_ns, changed_ns)` from a deadline-bounded `bstd::fs::Meta` stat.
fn verify_voucher_digest(boot: blaze_core::Boot, meta: &bstd::fs::Meta, expected_sha256: &[u8; 32]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(VERIFY_VOUCHER_DOMAIN);
    h.part(&boot.0)
        .part(expected_sha256)
        .update(&meta.stamp.dev.to_le_bytes())
        .update(&meta.stamp.ino.to_le_bytes())
        .update(&meta.stamp.len.to_le_bytes())
        .update(&meta.stamp.modified_ns.to_le_bytes())
        .update(&meta.stamp.changed_ns.to_le_bytes());
    h.finish()
}

/// Reads an exact 32-byte verification voucher through deadline-aware `bstd::fs` into a stack
/// buffer (rejecting any missing, truncated, or oversized file without allocating).
fn read_voucher_bounded(
    dir: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    cx: &bstd::cx::Cx,
    rel: &bstd::path::Path,
) -> Option<[u8; 32]> {
    let file = dir.open_read(cx, rel).ok()?;
    if file.len(cx).ok()? != 32 {
        return None;
    }
    let mut buf = [0u8; 33];
    let n = file.read_into(&mut buf, 0).wait(cx).ok()?;
    if n != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    if let Some(src) = buf.get(..32) {
        out.copy_from_slice(src);
        Some(out)
    } else {
        None
    }
}

/// Opens the command's [`CProbeSetting`] once at the command's edge (rule 75):
/// - Uses `DirStore::build_scripts` for `c-probe/<64-hex>` records and [`HermeticClangProbe`] as
///   the sole answerer (rule 3: never silently switches to the offline table).
/// - On hosts without a clang pin (e.g. x86-64 Linux before D69) or without the unpacked shelf,
///   returns [`CProbeSetting::Unavailable`] naming why so unrelated build scripts still run and
///   only a script issuing a C compiler probe fails by name.
pub fn open_command_c_probes<'a>(
    home: &str,
    host: &str,
    sdk: Option<(&str, &str)>,
    store_dir: &Path,
    alloc: Alloc<'a>,
) -> CProbeSetting {
    let shared = SharedAlloc::global();
    let work_root = Path::new(home).join(crate::native_setting::NATIVE_WORK_DIR).join("c-probes");
    let (hermetic, clang_sha256) = match HermeticClangProbe::open_from_shelf(home, host, sdk, work_root, alloc) {
        Ok(pair) => pair,
        Err(why) => return CProbeSetting::Unavailable(OwnArc::from_str_in(why.as_str(), shared)),
    };
    let (io, root) = match native_world(store_dir) {
        Ok(pair) => pair,
        Err(e) => {
            let msg = own_fmt(format_args!("c-probe store: {e}"), alloc);
            return CProbeSetting::Unavailable(OwnArc::from_str_in(msg.as_str(), shared));
        }
    };
    let records: OwnArc<dyn CProbeStore, SharedAlloc<'static>> =
        OwnArc::new_in(DirStore::build_scripts(io.spawner(), io.boot().ok(), &root), shared);
    let sdk_str = sdk.map_or("", |(_, id)| id);
    let sdk_id: OwnArc<str, SharedAlloc<'static>> = OwnArc::from_str_in(sdk_str, shared);
    let sdk_digest = facts::sdk_id_digest(sdk_str);
    let answerer: OwnArc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> = OwnArc::new_in(hermetic, shared);
    CProbeSetting::Ready(CScriptProbes { answerer, records, sdk_id, sdk_digest, clang_sha256 })
}

/// Host's pinned clang archive SHA-256 from `pins/clang.json`, or `None` when `host` has no pin.
pub fn active_clang_pin(host: &str) -> Option<[u8; 32]> {
    toolchain::clang_pin(CLANG_JSON, host).ok().map(|pin| pin.sha256)
}

/// Re-checks a recorded `cc-probe\n<answerer>\n<clang>\n<sdk>\n<key>` read subject against the
/// command's active `(answerer_id, clang_sha256, sdk_digest)` and `store`:
/// - `None`: `subject` is not a `cc-probe` subject.
/// - `Some(None)`: active answerer, clang pin, or SDK changed, or cache miss/unreadable record
///   (differs from any recorded digest, so only scripts that asked this probe re-run).
/// - `Some(Some(digest))`: active identity matches and cached canonical probe answer hashes to `digest`.
pub fn c_probe_reply_now(
    subject: &str,
    active_answerer: &ContentId,
    active_clang: &[u8; 32],
    active_sdk: &[u8; 32],
    store: Option<&dyn CProbeStore>,
    alloc: Alloc<'_>,
) -> Option<Option<ContentId>> {
    let parsed = facts::parse_cc_probe_subject(subject)?;
    if parsed.answerer != *active_answerer || parsed.clang != *active_clang || parsed.sdk != *active_sdk {
        return Some(None);
    }
    let store = store?;
    let Ok(Some(bytes)) = store.get(&parsed.key, alloc) else {
        return Some(None);
    };
    let Ok(ans) = decode_answer(bytes.as_slice(), alloc) else {
        return Some(None);
    };
    Some(Some(canonical_reply_digest(&ans)))
}

/// Bounded RAII cleanup of a per-probe task folder through deadline-aware `bstd::fs` (zero
/// unbounded `std::fs::remove_dir_all`).
struct TaskDirGuard<'w> {
    io: &'w bstd::io::Io,
    work_root: &'w bstd::fs::Dir<bstd::fs::ReadWrite>,
    task_rel: bstd::path::Path,
}

impl Drop for TaskDirGuard<'_> {
    fn drop(&mut self) {
        let Ok(cx) = self.io.cx(Duration::from_secs(5), bstd::mem::TASK_FRAME) else {
            return;
        };
        let task_dir = self.work_root.subdir(&self.task_rel);
        if let Ok(entries) = task_dir.own_entries(&cx) {
            for i in 0..entries.len() {
                if let Ok(p) = entries.path(i) {
                    let _ = task_dir.remove(&cx, &p, bstd::fs::Durable::No);
                }
            }
        }
        let _ = self.work_root.remove_dir(&cx, &self.task_rel, bstd::fs::Durable::No);
    }
}

/// Minimal `RootFiles` for `ReportSpec::WholeRoots` on Tree + Output roots (no Package roots).
struct ProbeFiles;

impl RootFiles for ProbeFiles {
    fn package_files(&self, root: RootId, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Err(Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }))
    }

    fn seen(&self, _: RootId, _: RelPath<'_>) -> Result<Seen, Fault> {
        Ok(Seen::Absent)
    }

    fn read_output(&self, root_path: RelPath<'_>, _: u32, _: &mut AVec<'_, u8>) -> Result<(), Fault> {
        let _ = root_path;
        Ok(())
    }

    fn list_output(&self, _: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        Ok(())
    }

    fn clear_output(&self) -> Result<(), Fault> {
        Ok(())
    }
}

/// Extracts or defaults `SOURCE_DATE_EPOCH` so `owned_env` and `ClockSpec::Fixed` always agree.
fn resolve_source_date_epoch<'a>(
    owned_env: &mut AVec<'a, (AString<'a>, AString<'a>)>,
    alloc: Alloc<'a>,
) -> Result<u64, AString<'a>> {
    if let Some((_, val)) = owned_env.iter().find(|(k, _)| k.as_str() == "SOURCE_DATE_EPOCH") {
        val.as_str()
            .parse::<u64>()
            .map_err(|_| own_fmt(format_args!("invalid SOURCE_DATE_EPOCH `{}` for compiler probe", val.as_str()), alloc))
    } else {
        owned_env.push((own_str("SOURCE_DATE_EPOCH", alloc), own_str("0", alloc)));
        Ok(0)
    }
}

impl ProbeAnswerer for HermeticClangProbe {
    fn id(&self) -> ContentId {
        Self::strategy_id()
    }

    fn answer<'a, 'b>(
        &self,
        probe: &CanonicalProbe<'_, '_>,
        input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
        alloc: Alloc<'a>,
    ) -> Result<ProbeAnswer<'a>, AString<'a>> {
        let (world, work_root) = native_world(Path::new(self.work_root.as_str()))
            .map_err(|e| own_fmt(format_args!("c-probe work world: {e}"), alloc))?;
        let clock = world.need_clock().map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let probe_began = clock.now();
        let remaining = || -> Option<Duration> {
            let elapsed = clock.now().since(probe_began).unwrap_or(Duration::ZERO);
            self.deadline.checked_sub(elapsed).filter(|d| !d.is_zero())
        };
        self.ensure_verified(&world, &work_root, &clock, probe_began, world.boot().ok(), alloc)?;

        let Some(stage_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted before staging", alloc));
        };
        let stage_cx = world
            .cx(stage_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe stage context stopped", alloc))?;
        let (dir_name, task_rel, task_dir) = make_unique_task_dir(&work_root, &clock, &stage_cx, alloc)?;
        let task_real = own_fmt(format_args!("{}/{}", self.work_root.as_str(), dir_name.as_str()), alloc);
        let _guard = TaskDirGuard { io: &world, work_root: &work_root, task_rel };

        // Materialize ONLY the probe's own OUT_DIR input files (`$IN<n><ext>` -> `<task>/in<n><ext>`)
        // through deadline-aware `bstd::fs`.
        let mut placeholders: AVec<'a, (AString<'a>, AString<'a>)> = OwnVec::new_in(alloc);
        for (spelled, rel) in probe.input_rels.iter() {
            let Some(bytes) = input_bytes(rel.as_str()) else {
                return Err(own_fmt(format_args!("c-probe input `{}` missing from OUT_DIR", rel.as_str()), alloc));
            };
            let local_name = placeholder_filename(spelled.as_str(), alloc);
            let file_rel = bstd::path::Path::new(local_name.as_str())
                .map_err(|_| own_fmt(format_args!("invalid probe input `{}`", local_name.as_str()), alloc))?;
            let host_file = own_fmt(format_args!("{}/{}", task_real.as_str(), local_name.as_str()), alloc);
            task_dir
                .publish(&stage_cx, &file_rel, bstd::fs::Content::bytes(bytes), bstd::fs::Durable::No)
                .map_err(|e| own_fmt(format_args!("write probe input {host_file}: {e}"), alloc))?;
            placeholders.push((own_str(spelled.as_str(), alloc), host_file));
        }

        // Collect output placeholders strictly from structural `Segment::Placeholder` nodes so
        // literal arguments containing `$OUT` are never mistaken for output files.
        for segs in probe.exec_args.iter() {
            for seg in segs.iter() {
                if let Segment::Placeholder(spelled) = seg
                    && spelled.as_str().starts_with(c_tasks_probe::OUTPUT)
                    && !placeholders.iter().any(|(k, _)| k.as_str() == spelled.as_str())
                {
                    let local_name = placeholder_filename(spelled.as_str(), alloc);
                    let host_file = own_fmt(format_args!("{}/{local_name}", task_real.as_str()), alloc);
                    placeholders.push((own_str(spelled.as_str(), alloc), host_file));
                }
            }
        }

        // Build canonical argv executed by pinned `clang-23` from structural segments so literal
        // sentinel text like `-DROOT="$OUT_DIR"` is preserved verbatim.
        let mut owned_args: AVec<'a, AString<'a>> = OwnVec::new_in(alloc);
        if matches!(probe.row.program.as_str(), "c++" | "clang++" | "g++") {
            owned_args.push(own_str("--driver-mode=g++", alloc));
        }
        owned_args.push(own_str("-fintegrated-cc1", alloc));
        owned_args.push(own_str("--no-default-config", alloc));

        let mut has_sysroot = false;
        let sdk_str = self.sdk_dir.as_ref().map(OwnString::as_str);
        for segs in probe.exec_args.iter() {
            let rendered = render_segments(segs.as_slice(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc);
            has_sysroot |= rendered.as_str().starts_with("-isysroot") || rendered.as_str().starts_with("--sysroot");
            owned_args.push(rendered);
        }
        if !has_sysroot && let Some(sdk) = sdk_str {
            owned_args.push(own_str("-isysroot", alloc));
            owned_args.push(own_str(sdk, alloc));
        }
        let mut arg_refs: AVec<'a, &str> = OwnVec::with_capacity_in(owned_args.len(), alloc);
        for arg in owned_args.iter() {
            arg_refs.push(arg.as_str());
        }

        // Build strictly ascending environment (`CLANG_ENV` + synchronized `SOURCE_DATE_EPOCH`).
        let mut owned_env: AVec<'a, (AString<'a>, AString<'a>)> = OwnVec::new_in(alloc);
        for (k, segs) in probe.exec_env.iter() {
            owned_env.push((
                own_str(k.as_str(), alloc),
                render_segments(segs.as_slice(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc),
            ));
        }
        let epoch_secs = resolve_source_date_epoch(&mut owned_env, alloc)?;
        owned_env.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        owned_env.dedup_by(|a, b| a.0.as_str() == b.0.as_str());
        let mut env_refs: AVec<'a, (&str, &str)> = OwnVec::with_capacity_in(owned_env.len(), alloc);
        for (k, v) in owned_env.iter() {
            env_refs.push((k.as_str(), v.as_str()));
        }

        // Build hermetic roots: shelf (Tree), optional sdk (Tree), task dir (Output).
        let mut roots_vec: AVec<'a, Root<'_>> = OwnVec::new_in(alloc);
        let mut mounts_vec: AVec<'a, Mount<'_>> = OwnVec::new_in(alloc);
        roots_vec.push(Root { name: "clang shelf", kind: RootKind::Tree { tree: Digest(self.shelf_digest) } });
        mounts_vec.push(Mount { spelling: self.shelf_dir.as_bytes(), root: RootId(0) });
        if let (Some(sdk_dir), Some(sdk_digest)) = (sdk_str, self.sdk_digest) {
            let idx = u8::try_from(roots_vec.len()).map_err(|_| own_str("too many roots", alloc))?;
            roots_vec.push(Root { name: "sdk", kind: RootKind::Tree { tree: Digest(sdk_digest) } });
            mounts_vec.push(Mount { spelling: sdk_dir.as_bytes(), root: RootId(idx) });
        }
        let out_idx = u8::try_from(roots_vec.len()).map_err(|_| own_str("too many roots", alloc))?;
        roots_vec.push(Root { name: "c-probe output", kind: RootKind::Output });
        mounts_vec.push(Mount { spelling: task_real.as_bytes(), root: RootId(out_idx) });
        let roots = Roots::new(roots_vec.as_slice()).map_err(|_| own_str("c-probe roots refused", alloc))?;

        let spawner = world.need_host().and_then(|h| h.spawner()).map_err(|m| own_fmt(format_args!("c-probe spawner: {}", m.capability), alloc))?;
        let driver_clock = world.need_clock().map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: driver_clock });
        let elapsed = clock.now().since(probe_began).unwrap_or(Duration::ZERO);
        let remaining = self
            .deadline
            .checked_sub(elapsed)
            .filter(|d| !d.is_zero())
            .ok_or_else(|| own_str("c-probe deadline exhausted during clang verification", alloc))?;
        let cx = world.cx(remaining, bstd::mem::TASK_FRAME).map_err(|_| own_str("c-probe context stopped", alloc))?;

        let spec = HermeticSpec::new(HermeticOpts {
            program: Program::Stored { name: "clang-23", digest: Digest(self.shelf_digest), path: self.clang_path.as_str() },
            args: arg_refs.as_slice(),
            env: env_refs.as_slice(),
            roots,
            mounts: mounts_vec.as_slice(),
            cwd: task_real.as_str(),
            helpers: &[],
            helper_folders: &[],
            channels: &[],
            clock: ClockSpec::Fixed { unix_secs: epoch_secs },
            seed: 0,
            budget: Budget { cpu: remaining, memory: PROBE_MEMORY_BYTES, steps: None },
            deadline: Deadline::after(clock.now().since_start(), remaining),
            streams: HermeticStreams { stdout: Capture::Keep { most: PROBE_MOST_STREAM }, stderr: Capture::Keep { most: PROBE_MOST_STREAM } },
            level: Level::Inputs,
            report: ReportSpec::WholeRoots,
        })
        .map_err(|e| own_fmt(format_args!("c-probe spec refused: {:?} ({:?})", e.fault, e.rule), alloc))?;

        let ran = hermetic
            .run(&cx, &spec, &ProbeFiles, alloc)
            .map_err(|e| own_fmt(format_args!("c-probe hermetic run failed: {:?} ({:?})", e.fault, e.rule), alloc))?;

        let code = match ran.exit {
            Exit::Exited(code) => code,
            Exit::Signaled(sig) => return Err(own_fmt(format_args!("c-probe clang-23 killed by signal {sig}"), alloc)),
        };

        let raw_out = core::str::from_utf8(ran.stdout.as_slice()).map_err(|_| own_str("c-probe stdout is not UTF-8", alloc))?;
        let raw_err = core::str::from_utf8(ran.stderr.as_slice()).map_err(|_| own_str("c-probe stderr is not UTF-8", alloc))?;

        // Normalize host paths back to canonical placeholders (longest match first), including the
        // entire clang shelf root (`<shelf>/bin` -> `$CLANG_SHELF/bin`) so `-v` output like
        // `InstalledDir: <shelf>/bin` never leaks the machine's home directory into cached replies.
        let stdout = normalize_reply(raw_out, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc);
        let stderr = normalize_reply(raw_err, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc);
        Ok(ProbeAnswer { code, stdout, stderr })
    }
}

/// Stable placeholder replacing host clang shelf root paths (e.g. `InstalledDir: $CLANG_SHELF/bin`).
const CLANG_SHELF: &str = "$CLANG_SHELF";

fn normalize_reply<'a>(
    raw: &str,
    clang_path: &str,
    shelf_dir: &str,
    task_dir: &str,
    sdk_dir: Option<&str>,
    placeholders: &[(AString<'a>, AString<'a>)],
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut rev_pairs: AVec<'a, (&str, &str)> = OwnVec::new_in(alloc);
    for (spelled, host_file) in placeholders.iter() {
        rev_pairs.push((host_file.as_str(), spelled.as_str()));
    }
    rev_pairs.push((clang_path, "clang-23"));
    rev_pairs.push((shelf_dir, CLANG_SHELF));
    rev_pairs.push((task_dir, OUT_DIR));
    if let Some(sdk) = sdk_dir {
        rev_pairs.push((sdk, SDKROOT));
    }
    rev_pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()));
    replace_pairs(raw, rev_pairs.as_slice(), alloc)
}

fn render_segments<'a>(
    segs: &[Segment<'_>],
    task_dir: &str,
    sdk_dir: Option<&str>,
    placeholders: &[(AString<'a>, AString<'a>)],
    alloc: Alloc<'a>,
) -> AString<'a> {
    let mut out = OwnString::new_in(alloc);
    for seg in segs {
        match seg {
            Segment::Literal(lit) => out.push_str(lit.as_str()),
            Segment::OutDir => out.push_str(task_dir),
            Segment::SdkRoot => {
                if let Some(sdk) = sdk_dir {
                    out.push_str(sdk);
                } else {
                    out.push_str(SDKROOT);
                }
            }
            Segment::Mount => out.push_str("/package"),
            Segment::Placeholder(spelled) => {
                if let Some((_, host_file)) = placeholders.iter().find(|(k, _)| k.as_str() == spelled.as_str()) {
                    out.push_str(host_file.as_str());
                } else {
                    out.push_str(spelled.as_str());
                }
            }
        }
    }
    out
}

/// Creates a crash- and PID-reuse-safe per-probe task folder under `work_root`, mixing wall-clock
/// nanoseconds with `pid` and `PROBE_SEQ` and retrying on `FsKind::Exists` so stale directories
/// left by prior crashes never abort a valid probe.
fn make_unique_task_dir<'a>(
    work_root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    clock: &bstd::clock::Clock,
    cx: &bstd::cx::Cx,
    alloc: Alloc<'a>,
) -> Result<(AString<'a>, bstd::path::Path, bstd::fs::Dir<bstd::fs::ReadWrite>), AString<'a>> {
    let wall = clock.wall().as_ns();
    let pid = process::id();
    for _ in 0..32u32 {
        let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir_name = own_fmt(format_args!("p-{pid}-{wall:x}-{seq}"), alloc);
        let task_rel = bstd::path::Path::new(dir_name.as_str())
            .map_err(|_| own_fmt(format_args!("invalid c-probe task dir `{}`", dir_name.as_str()), alloc))?;
        match work_root.make_dir(cx, &task_rel, bstd::fs::Durable::No) {
            Ok(dir) => return Ok((dir_name, task_rel, dir)),
            Err(e) if e.kind == bstd::fs::FsKind::Exists => continue,
            Err(e) => return Err(own_fmt(format_args!("create probe dir: {e}"), alloc)),
        }
    }
    Err(own_str("create probe dir: exhausted unique task directory attempts", alloc))
}

fn placeholder_filename<'a>(spelled: &str, alloc: Alloc<'a>) -> AString<'a> {
    if let Some(rest) = spelled.strip_prefix(c_tasks_probe::INPUT) {
        own_fmt(format_args!("in{rest}"), alloc)
    } else if let Some(rest) = spelled.strip_prefix(c_tasks_probe::OUTPUT) {
        own_fmt(format_args!("out{rest}"), alloc)
    } else {
        own_str("file", alloc)
    }
}

fn replace_pairs<'a>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = OwnString::new_in(alloc);
    let mut at = 0usize;
    'scan: while let Some(rest) = text.get(at..) {
        if rest.is_empty() {
            break;
        }
        for (from, to) in pairs.iter().copied() {
            if !from.is_empty() && rest.starts_with(from) {
                out.push_str(to);
                at += from.len();
                continue 'scan;
            }
        }
        if let Some(ch) = rest.chars().next() {
            out.push(ch);
            at += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn own_str<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    OwnString::from_str_in(s, alloc)
}

fn own_fmt<'a>(args: fmt::Arguments<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = OwnString::new_in(alloc);
    let _ = out.write_fmt(args);
    out
}

fn canonical_utf8<'a>(path: &Path, alloc: Alloc<'a>) -> Result<SharedStr, AString<'a>> {
    let real = fs::canonicalize(path).map_err(|e| own_fmt(format_args!("{}: {e}", path.display()), alloc))?;
    match real.to_str() {
        Some(s) => Ok(OwnString::from_str_in(s, SharedAlloc::global())),
        None => Err(own_fmt(format_args!("{}: path is not UTF-8", real.display()), alloc)),
    }
}

fn file_sha256_bounded_on<'a>(
    root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
    cx: &bstd::cx::Cx,
    rel: &bstd::path::Path,
    label: &str,
    alloc: Alloc<'a>,
) -> Result<[u8; 32], AString<'a>> {
    let file = root.open_read(cx, rel).map_err(|e| own_fmt(format_args!("{label}: {e}"), alloc))?;
    let mut hasher = blaze_sha256::Sha256::new();
    let mut buf = [0u8; 64 << 10];
    let mut offset = 0u64;
    loop {
        let n = file.read_into(&mut buf, offset).wait(cx).map_err(|e| own_fmt(format_args!("{label}: {e}"), alloc))?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
        let step = u64::try_from(n).map_err(|_| own_str("c-probe verify offset overflow", alloc))?;
        offset = offset.checked_add(step).ok_or_else(|| own_str("c-probe verify offset overflow", alloc))?;
    }
    Ok(hasher.finish())
}

fn hex_nibble(n: u8) -> char {
    match n & 15 {
        d @ 0..=9 => char::from(b'0' + d),
        d => char::from(b'a' + (d - 10)),
    }
}

fn hex_bytes<'a>(bytes: &[u8; 32], alloc: Alloc<'a>) -> AString<'a> {
    let mut out = OwnString::with_capacity_in(64, alloc);
    for byte in bytes {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 15));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use blaze_build_script::c_tasks_probe::{self, Answer, ProbeAnswerer, Spawn, Table, resolve};
    use blaze_build_script::facts::{CcProbeSubject, cc_probe_subject, parse_cc_probe_subject};
    use blaze_build_script::toolchain::clang_pin;
    use blaze_pins::CLANG_JSON;

    use super::{
        CProbeSetting, CProbeStore as _, CacheAdapter, HermeticClangProbe, MemCProbeStore, c_probe_reply_now,
        open_command_c_probes, resolve_source_date_epoch,
    };

    /// Differential test (Rule 25 + D174): requires the unpacked Linux ARM64 `clang-23` shelf
    /// (`cargo test -p blaze-proc-macro --lib -- --ignored hermetic_clang_probe`). Fails hard if
    /// the shelf is absent instead of returning Ok without running assertions.
    #[test]
    #[ignore = "requires unpacked Linux ARM64 clang-23 shelf; run with --ignored"]
    fn hermetic_clang_probe_matches_linux_arm64_corpus_and_answers_novel_flags() {
        assert!(cfg!(all(target_os = "linux", target_arch = "aarch64")), "differential test requires Linux ARM64");
        let host = "aarch64-unknown-linux-gnu";
        let home = std::env::var("HOME").expect("HOME must be set");
        let alloc = blaze_own::testing::heap().alloc();
        let pin = clang_pin(CLANG_JSON, host).expect("linux arm64 pin");
        let pin_hex = super::hex_bytes(&pin.sha256, alloc);
        let shelf = Path::new(&home).join(crate::native_setting::SHELF_DIR).join(pin_hex.as_str());
        assert!(shelf.join("bin/clang-23").is_file(), "expected unpacked shelf binary at {}", shelf.join("bin/clang-23").display());
        let work = std::env::temp_dir().join(format!("blaze-cprobe-diff-{}", std::process::id()));
        let answerer = HermeticClangProbe::from_shelf_dir(&shelf, host, None, work.clone(), alloc).expect("open hermetic probe");
        let store = Arc::new(MemCProbeStore::default());
        let cache = CacheAdapter(store.as_ref());

        let raw_json = blaze_pins::CLANG_ANSWERS_JSON;
        let table = Table::parse(raw_json.as_bytes(), alloc).expect("table parses");
        let flag_src: &[u8] = b"int main(void) { return 0; }";
        let flag_sha = blaze_sha256::digest(flag_src);
        let flag_hex = super::hex_bytes(&flag_sha, alloc);
        let family_src: &[u8] = b"#ifdef __clang__\n#pragma message \"clang\"\n#endif\n\n#ifdef __GNUC__\n#pragma message \"gcc\"\n#endif\n\n#ifdef __EMSCRIPTEN__\n#pragma message \"emscripten\"\n#endif\n\n#ifdef __VXWORKS__\n#pragma message \"VxWorks\"\n#endif\n";
        let family_sha = blaze_sha256::digest(family_src);
        let family_hex = super::hex_bytes(&family_sha, alloc);

        let mut checked = 0usize;
        for row in table.rows() {
            if row.clang.as_str() != pin_hex.as_str() {
                continue;
            }
            let Answer::Known { code, stdout, stderr } = &row.answer else { continue };
            let out_dir = "/target/debug/build/diff-1/out";
            let mut spawned_args: std::vec::Vec<std::string::String> = std::vec::Vec::new();
            let mut file_map: std::vec::Vec<(std::string::String, [u8; 32], &[u8])> = std::vec::Vec::new();
            for arg in row.args.iter() {
                let s = arg.as_str();
                if let Some((_, sha_hex)) = row.files.iter().find(|(k, _)| k.as_str() == s) {
                    let bytes: &[u8] = if sha_hex.as_str() == flag_hex.as_str() {
                        flag_src
                    } else if sha_hex.as_str() == family_hex.as_str() {
                        family_src
                    } else {
                        panic!("unknown source sha {sha_hex}");
                    };
                    let rel = super::placeholder_filename(s, alloc).as_str().to_owned();
                    let sha = blaze_sha256::digest(bytes);
                    spawned_args.push(format!("{out_dir}/{rel}"));
                    file_map.push((rel, sha, bytes));
                } else if s.starts_with(c_tasks_probe::OUTPUT) {
                    let rel = super::placeholder_filename(s, alloc);
                    spawned_args.push(format!("{out_dir}/{rel}"));
                } else {
                    spawned_args.push(s.to_owned());
                }
            }
            let arg_refs: std::vec::Vec<&str> = spawned_args.iter().map(std::string::String::as_str).collect();
            let env_refs: std::vec::Vec<(&str, &str)> = row.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let sp = Spawn {
                program: row.program.as_str(),
                args: &arg_refs,
                env: &env_refs,
                out_dir,
                sdk: "",
                mount: "/package",
                sdk_id: "",
                stdin: &[],
            };
            let file_dig = |rel: &str| file_map.iter().find(|(r, _, _)| r == rel).map(|(_, d, _)| *d);
            let mut file_bytes = |rel: &str| file_map.iter().find(|(r, _, _)| r == rel).map(|(_, _, b)| *b);
            let resolved = resolve(&answerer, &cache, &pin.sha256, &sp, file_dig, &mut file_bytes, alloc)
                .unwrap_or_else(|e| panic!("hermetic probe failed for {:?}: {e}", row.args));

            let raw_cached = store.get(&resolved.key, alloc).expect("read cache").expect("cached");
            let canonical = c_tasks_probe::decode_answer(raw_cached.as_slice(), alloc).expect("decode");
            assert_eq!(canonical.code, *code, "code mismatch for {:?}", row.args);
            assert_eq!(canonical.stdout.as_str(), stdout.as_str(), "stdout mismatch for {:?}", row.args);
            assert_eq!(canonical.stderr.as_str(), stderr.as_str(), "stderr mismatch for {:?}", row.args);

            let empty_sdk = blaze_build_script::facts::sdk_id_digest("");
            let subject = cc_probe_subject(&answerer.id(), &pin.sha256, &empty_sdk, &resolved.key, alloc);
            assert_eq!(
                parse_cc_probe_subject(subject.as_str()),
                Some(CcProbeSubject { answerer: answerer.id(), clang: pin.sha256, sdk: empty_sdk, key: resolved.key })
            );
            assert_eq!(
                c_probe_reply_now(subject.as_str(), &answerer.id(), &pin.sha256, &empty_sdk, Some(store.as_ref()), alloc),
                Some(Some(resolved.canonical_digest))
            );
            checked += 1;
        }
        assert!(checked >= 27, "expected >= 27 Linux ARM64 rows checked, got {checked}");

        // Novel flags + nonzero SOURCE_DATE_EPOCH (proves ClockSpec::Fixed agrees with env!).
        for (extra_flag, env) in [
            ("-Wshadow", &[("SOURCE_DATE_EPOCH", "1700000000")][..]),
            ("-Wuninitialized", &[][..]),
            ("-ffile-prefix-map=/workspace/vendored/aws-lc-sys=", &[][..]),
        ] {
            let out_dir = "/target/debug/build/novel-1/out";
            let args = [
                "-O0",
                "-ffunction-sections",
                "-fdata-sections",
                "-fPIC",
                "--target=aarch64-unknown-linux-gnu",
                "-Wall",
                "-Wextra",
                extra_flag,
                "-Wno-unused-command-line-argument",
                "-o",
                "/target/debug/build/novel-1/out/flag_check",
                "-c",
                "/target/debug/build/novel-1/out/flag_check.c",
            ];
            let sp = Spawn {
                program: "cc",
                args: &args,
                env,
                out_dir,
                sdk: "",
                mount: "/workspace/vendored/aws-lc-sys",
                sdk_id: "",
                stdin: &[],
            };
            let file_dig = |rel: &str| (rel == "flag_check.c").then_some(flag_sha);
            let mut file_bytes = |rel: &str| (rel == "flag_check.c").then_some(flag_src);
            let resolved = resolve(&answerer, &cache, &pin.sha256, &sp, file_dig, &mut file_bytes, alloc)
                .unwrap_or_else(|e| panic!("novel probe {extra_flag} failed: {e}"));
            assert_eq!(resolved.answered.code, 0, "{extra_flag}: {:?}", resolved.answered);
            assert_eq!(resolved.answered.stderr.as_str(), "", "{extra_flag}: {:?}", resolved.answered);
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Rule 23 selective invalidation + active `(answerer_id, clang_sha256)` revalidation +
    /// missing-shelf refusal + nonzero `SOURCE_DATE_EPOCH` parsing (runs on every host).
    #[test]
    fn selective_c_probe_invalidation_and_active_identity_checks() {
        use blaze_build_script::c_tasks_probe::{ProbeAnswer, encode_answer};
        use blaze_own::{AVec, String as OwnString};

        struct StubAnswerer;
        impl ProbeAnswerer for StubAnswerer {
            fn id(&self) -> blaze_ids::ContentId {
                blaze_ids::ContentId::of("stub-cprobe-selective-test", b"1")
            }
            fn answer<'a, 'b>(
                &self,
                probe: &blaze_build_script::c_tasks_probe::CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: blaze_own::Alloc<'a>,
            ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
                Ok(ProbeAnswer {
                    code: 0,
                    stdout: OwnString::new_in(alloc),
                    stderr: OwnString::from_str_in(probe.row.args.last().map_or("", |s| s.as_str()), alloc),
                })
            }
        }

        let alloc = blaze_own::testing::heap().alloc();
        let store = MemCProbeStore::default();
        let cache = CacheAdapter(&store);
        let pin = [7u8; 32];
        let src = b"int main(void) { return 0; }";
        let sha = blaze_sha256::digest(src);
        let dig = |r: &str| (r == "flag_check.c").then_some(sha);
        let mut bytes = |r: &str| (r == "flag_check.c").then_some(src.as_slice());

        let args_a = ["-c", "-Wshadow", "/out/flag_check.c", "-o", "/out/flag_check.o"];
        let args_b = ["-c", "-Wuninitialized", "/out/flag_check.c", "-o", "/out/flag_check.o"];
        let sp_a = Spawn { program: "cc", args: &args_a, env: &[], out_dir: "/out", sdk: "", mount: "/package", sdk_id: "", stdin: &[] };
        let sp_b = Spawn { program: "cc", args: &args_b, env: &[], out_dir: "/out", sdk: "", mount: "/package", sdk_id: "", stdin: &[] };

        let res_a = resolve(&StubAnswerer, &cache, &pin, &sp_a, dig, &mut bytes, alloc).expect("probe A");
        let res_b = resolve(&StubAnswerer, &cache, &pin, &sp_b, dig, &mut bytes, alloc).expect("probe B");
        let sdk_a = blaze_build_script::facts::sdk_id_digest("macos-sdk-15.2");
        let subj_a = cc_probe_subject(&StubAnswerer.id(), &pin, &sdk_a, &res_a.key, alloc);
        let subj_b = cc_probe_subject(&StubAnswerer.id(), &pin, &sdk_a, &res_b.key, alloc);

        assert_eq!(c_probe_reply_now(subj_a.as_str(), &StubAnswerer.id(), &pin, &sdk_a, Some(&store), alloc), Some(Some(res_a.canonical_digest)));
        assert_eq!(c_probe_reply_now(subj_b.as_str(), &StubAnswerer.id(), &pin, &sdk_a, Some(&store), alloc), Some(Some(res_b.canonical_digest)));

        // Upgrading clang pin, answerer id, or macOS SDK invalidates recorded probe reads even
        // though the old record file still exists on disk.
        let new_pin = [8u8; 32];
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &StubAnswerer.id(), &new_pin, &sdk_a, Some(&store), alloc), Some(None));
        let new_answerer = blaze_ids::ContentId::of("stub-cprobe-selective-test", b"2");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &new_answerer, &pin, &sdk_a, Some(&store), alloc), Some(None));
        let new_sdk = blaze_build_script::facts::sdk_id_digest("macos-sdk-15.4");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &StubAnswerer.id(), &pin, &new_sdk, Some(&store), alloc), Some(None));

        // Mutating ONLY probe A's cached record invalidates A and leaves B intact.
        let mut mutated = AVec::new_in(alloc);
        encode_answer(
            &ProbeAnswer { code: 1, stdout: OwnString::new_in(alloc), stderr: OwnString::from_str_in("error: unsupported", alloc) },
            &mut mutated,
        );
        store.overwrite(res_a.key, mutated.as_slice());

        assert_ne!(
            c_probe_reply_now(subj_a.as_str(), &StubAnswerer.id(), &pin, &sdk_a, Some(&store), alloc),
            Some(Some(res_a.canonical_digest)),
            "mutated probe A must invalidate its asking script"
        );
        assert_eq!(
            c_probe_reply_now(subj_b.as_str(), &StubAnswerer.id(), &pin, &sdk_a, Some(&store), alloc),
            Some(Some(res_b.canonical_digest)),
            "unrelated probe B must remain valid"
        );

        // Missing shelf / unsupported host returns Unavailable (never silently switches answerers).
        let empty_home = std::env::temp_dir().join(format!("blaze-cprobe-empty-{}", std::process::id()));
        let missing = open_command_c_probes(&empty_home.to_string_lossy(), "aarch64-unknown-linux-gnu", None, &empty_home, alloc);
        assert!(matches!(missing, CProbeSetting::Unavailable(_)), "missing shelf must return Unavailable");
        let unsupported = open_command_c_probes(&empty_home.to_string_lossy(), "x86_64-unknown-linux-gnu", None, &empty_home, alloc);
        assert!(matches!(unsupported, CProbeSetting::Unavailable(_)), "unsupported host must return Unavailable");

        // SOURCE_DATE_EPOCH parsing honors nonzero epoch and rejects invalid text.
        let mut env_nz = AVec::new_in(alloc);
        env_nz.push((OwnString::from_str_in("SOURCE_DATE_EPOCH", alloc), OwnString::from_str_in("1700000000", alloc)));
        assert_eq!(resolve_source_date_epoch(&mut env_nz, alloc).expect("nonzero epoch"), 1_700_000_000);
        let mut env_default = AVec::new_in(alloc);
        assert_eq!(resolve_source_date_epoch(&mut env_default, alloc).expect("default epoch"), 0);
        assert_eq!(env_default.len(), 1);
        let mut env_bad = AVec::new_in(alloc);
        env_bad.push((OwnString::from_str_in("SOURCE_DATE_EPOCH", alloc), OwnString::from_str_in("not-a-num", alloc)));
        assert!(resolve_source_date_epoch(&mut env_bad, alloc).is_err());

        // Lazy verification parks on the scheduler-aware `bstd::sync::Lock` and honors the probe
        // deadline when another worker holds the lock.
        let dummy = HermeticClangProbe {
            host: OwnString::from_str_in("aarch64-unknown-linux-gnu", super::SharedAlloc::global()),
            clang_path: OwnString::from_str_in("/nonexistent/clang-23", super::SharedAlloc::global()),
            shelf_dir: OwnString::from_str_in("/nonexistent", super::SharedAlloc::global()),
            shelf_digest: [0u8; 32],
            sdk_dir: None,
            sdk_digest: None,
            work_root: OwnString::from_str_in(&std::env::temp_dir().to_string_lossy(), super::SharedAlloc::global()),
            deadline: super::Duration::from_millis(25),
            verified: super::AtomicBool::new(false),
            verify_lock: bstd::sync::Lock::new("c-probe.clang.verify.test", ()),
        };
        let (io, work_root) = super::native_world(&std::env::temp_dir()).expect("native world");
        let clock = io.need_clock().expect("clock");
        let _held = dummy.verify_lock.try_lock().expect("hold verify lock");
        let began = clock.now();
        let err = dummy
            .ensure_verified(&io, &work_root, &clock, began, None, alloc)
            .expect_err("must time out waiting on scheduler lock");
        assert!(err.as_str().contains("timed out"), "unexpected error: {err}");

        // Structural segment rendering preserves literal `$OUT_DIR` while expanding real `Segment::OutDir`.
        let lit_segs = [super::Segment::Literal(OwnString::from_str_in("-DROOT=\"$OUT_DIR\"", alloc))];
        let rendered_lit = super::render_segments(&lit_segs, "/tmp/task-1", None, &[], alloc);
        assert_eq!(rendered_lit.as_str(), "-DROOT=\"$OUT_DIR\"");
        let real_segs = [
            super::Segment::Literal(OwnString::from_str_in("-DROOT=\"", alloc)),
            super::Segment::OutDir,
            super::Segment::Literal(OwnString::from_str_in("\"", alloc)),
        ];
        let rendered_real = super::render_segments(&real_segs, "/tmp/task-1", None, &[], alloc);
        assert_eq!(rendered_real.as_str(), "-DROOT=\"/tmp/task-1\"");

        // Reply normalization scrubs BOTH `<shelf>/bin/clang-23` AND `<shelf>/bin` (`InstalledDir:`).
        let norm = super::normalize_reply(
            "clang: /home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23\nInstalledDir: /home/user/.blaze/v2/toolchain/clang/abc/bin\n",
            "/home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23",
            "/home/user/.blaze/v2/toolchain/clang/abc",
            "/tmp/task-1",
            None,
            &[],
            alloc,
        );
        assert_eq!(norm.as_str(), "clang: clang-23\nInstalledDir: $CLANG_SHELF/bin\n");

        // Stale task directory collisions recover cleanly across crash / PID reuse.
        let col_root = std::env::temp_dir().join(format!("blaze-cprobe-col-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&col_root);
        std::fs::create_dir_all(&col_root).expect("col root");
        let (col_io, col_dir) = super::native_world(&col_root).expect("col world");
        let col_clock = col_io.need_clock().expect("col clock");
        let col_cx = col_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("col cx");
        let (n1, rel1, _) = super::make_unique_task_dir(&col_dir, &col_clock, &col_cx, alloc).expect("dir 1");
        let (n2, rel2, _) = super::make_unique_task_dir(&col_dir, &col_clock, &col_cx, alloc).expect("dir 2");
        assert_ne!(n1.as_str(), n2.as_str());
        let _ = col_dir.remove_dir(&col_cx, &rel1, bstd::fs::Durable::No);
        let _ = col_dir.remove_dir(&col_cx, &rel2, bstd::fs::Durable::No);
        let _ = std::fs::remove_dir_all(&col_root);
    }
}
