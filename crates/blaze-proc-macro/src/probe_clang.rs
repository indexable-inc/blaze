//! Compiler probe answerer + content-addressed probe cache (`c-probe/<64-hex>`), implementing
//! [`blaze_build_script::c_tasks_probe::ProbeAnswerer`] (rule 78, D174/D177).
//!
//! - On Linux hosts with a pinned `clang-23` shelf (`pins/clang.json`), [`open_command_c_probes`]
//!   opens [`HermeticClangProbe`]: every probe miss executes the kernel-sealed `MFD_EXEC`
//!   `clang-23` snapshot (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE`) via
//!   `execveat(..., AT_EMPTY_PATH)` inside `bstd::procs::hermetic::Hermetic` at `Level::Inputs`
//!   (Landlock + seccomp) with `-fintegrated-cc1 --no-default-config`, live pre/post-exec ELF
//!   runtime closure validation, and only the probe's own `OUT_DIR` inputs staged in a fresh
//!   task folder.
//! - On non-Linux hosts (e.g. macOS, where XNU lacks descriptor-bound `execveat` / sealed `memfd`),
//!   `spawn_route::ScriptSandbox::spawn_probe` preserves the existing `pins/answers/clang.json`
//!   pinned-table route untouched, and [`open_command_c_probes`] returns
//!   [`CProbeSetting::Unavailable`].
//! - If a Linux host has no clang pin in `pins/clang.json`, [`open_command_c_probes`] returns a
//!   named refusal retained as [`CProbeSetting::Unavailable`] so unrelated build scripts still
//!   run, while any script issuing a C compiler probe fails explicitly naming what is missing.
//! - Every resolved probe's canonical answer is cached in [`CProbeStore`] (`DirStore` under
//!   `c-probe/<64-hex>` or [`MemCProbeStore`] in tests), and [`c_probe_reply_now`] revalidates
//!   `cc-probe\n<answerer>\n<clang>\n<sdk>\n<key>` against BOTH the active `(answerer_id,
//!   clang_sha256, sdk_digest)` and the cached canonical answer without hashing any table into
//!   every script's base key.

use core::fmt::{self, Write as _};
#[cfg(target_os = "linux")]
use core::num::NonZeroUsize;
#[cfg(any(target_os = "linux", test))]
use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(target_os = "linux", test))]
use core::time::Duration;
#[cfg(any(target_os = "linux", test))]
use std::process;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use blaze_build_script::c_tasks_probe::{
    self, ProbeAnswerer, ProbeKey, canonical_reply_digest, decode_answer,
};
#[cfg(target_os = "linux")]
use blaze_build_script::c_tasks_probe::{CanonicalProbe, ProbeAnswer, Segment};
#[cfg(any(target_os = "linux", test))]
use blaze_build_script::c_tasks_probe::{OUT_DIR, SDKROOT};
use blaze_build_script::facts;
#[cfg(target_os = "linux")]
use blaze_build_script::shelf_pins;
use blaze_build_script::toolchain;
#[cfg(target_os = "linux")]
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Arc as OwnArc, SharedAlloc, String as OwnString, Vec as OwnVec};
#[cfg(target_os = "linux")]
use blaze_own::Share as _;
use blaze_pins::CLANG_JSON;
#[cfg(target_os = "linux")]
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
#[cfg(any(target_os = "linux", test))]
use bstd::procs::hermetic::ClockSpec;
#[cfg(target_os = "linux")]
use bstd::procs::{
    Exit,
    hermetic::{
        Budget, Capture, Fault, FilesError, FilesOp, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
        HermeticStreams, Level, Program, ReportSpec, RootFiles, Stop as HermeticStop,
    },
};

use crate::record::{DirStore, FilesError as StoreFilesError};

#[cfg(target_os = "linux")]
type SharedStr = OwnString<SharedAlloc<'static>>;

/// Strategy id for [`HermeticClangProbe`] (hashed into [`ProbeKey`] and `cc-probe` read subjects).
#[cfg(target_os = "linux")]
const HERMETIC_STRATEGY_DOMAIN: &str = "blaze-build-script c-probe shelf clang 1";

/// Maximum captured stdout or stderr bytes for one compiler probe.
#[cfg(target_os = "linux")]
const PROBE_MOST_STREAM: u32 = 1 << 20;

/// Maximum `OUT_DIR` input file size classified for hermetic probe execution; larger generated
/// sources stay on the ordinary compiler route so they never trip `PROBE_MOST_STREAM`.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MAX_PROBE_INPUT_BYTES: usize = 64 << 10;

/// Error prefix emitted when a probe child exceeds `PROBE_MOST_STREAM` (`Stop::Stream`), signaling
/// `spawn_probe` to route the invocation to the ordinary compiler path instead of aborting.
#[cfg(any(target_os = "linux", test))]
pub(crate) const STREAM_OVERFLOW_PREFIX: &str = "c-probe stream limit exceeded:";

/// Returns whether any `OUT_DIR` input file referenced by `args` (excluding `-o` / `--output`
/// destination paths, and normalized via `normalize_under_out_dir` so spellings like
/// `/out/./flag_check.c` match the `OUT_DIR` map key) exceeds `MAX_PROBE_INPUT_BYTES`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn has_oversized_probe_input<'a, 'b>(
    args: &[&str],
    out_dir: &str,
    input_bytes: &mut impl FnMut(&str) -> Option<&'b [u8]>,
    alloc: Alloc<'a>,
) -> bool {
    let mut skip_output = false;
    for &arg in args {
        if skip_output {
            skip_output = false;
            continue;
        }
        if matches!(arg, "-o" | "--output") {
            skip_output = true;
            continue;
        }
        if arg.starts_with("-o") || arg.starts_with("--output=") {
            continue;
        }
        let Some(rel) = c_tasks_probe::normalize_under_out_dir(arg, out_dir, alloc) else {
            continue;
        };
        if let Some(bytes) = input_bytes(rel.as_str())
            && bytes.len() > MAX_PROBE_INPUT_BYTES
        {
            return true;
        }
    }
    false
}

/// Returns whether a probe answerer error indicates stream-capture overflow (`Stop::Stream`).
#[cfg(test)]
pub(crate) fn is_stream_overflow_error(msg: &str) -> bool {
    msg.starts_with(STREAM_OVERFLOW_PREFIX)
}

/// Maximum encoded + sealed `c-probe/<64-hex>` record size accepted from disk (`2 * 1 MiB`
/// streams + binary header + 32-byte seal + margin).
pub const MAX_RECORD_BYTES: u64 = 2 * (1 << 20) + 4096;

/// Per-probe wall and CPU deadline (rule 18).
#[cfg(target_os = "linux")]
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Virtual address-space cap for `clang-23` under `RLIMIT_AS` (342 MB binary + shared libs + LLVM).
#[cfg(target_os = "linux")]
const PROBE_MEMORY_BYTES: u64 = 8 << 30;

/// Monotonic sequence counter for per-probe isolated task folders.
#[cfg(any(target_os = "linux", test))]
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
#[cfg(any(target_os = "linux", test))]
pub(crate) struct CacheAdapter<'s>(pub(crate) &'s dyn CProbeStore);

#[cfg(any(target_os = "linux", test))]
impl c_tasks_probe::ProbeCache for CacheAdapter<'_> {
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
    /// Configured host SDK directory (`""` when no SDK is active), verified against child
    /// `SDKROOT` overrides before canonicalizing a probe (rule 21).
    pub sdk_dir: OwnArc<str, SharedAlloc<'static>>,
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

/// Default strategy id for this host OS (used when no home/runtime-closure context is available
/// and by cross-platform `sweep` tests).
pub fn default_answerer_strategy_id() -> ContentId {
    #[cfg(target_os = "linux")]
    {
        HermeticClangProbe::strategy_id()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let table_id = c_tasks_probe::table_id(blaze_pins::CLANG_ANSWERS_JSON.as_bytes());
        ContentId::of("blaze-build-script c-probe non-linux table 1", &table_id.0)
    }
}

/// Proof of verified `clang-23` held in [`HermeticClangProbe::verify_flight`]: keeps the
/// kernel-sealed `MFD_EXEC` [`bstd::procs::hermetic::VerifiedExec`]
/// (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE`) alive so Landlock and
/// `execveat(..., AT_EMPTY_PATH)` execute that exact verified snapshot with zero TOCTOU.
#[cfg(target_os = "linux")]
#[derive(Clone)]
struct VerifiedClang {
    exec: OwnArc<bstd::procs::hermetic::VerifiedExec, SharedAlloc<'static>>,
}

/// Hermetic pinned `clang-23` compiler probe runner (`bstd::procs::hermetic::Hermetic` at
/// `Level::Inputs` on Linux).
#[cfg(target_os = "linux")]
pub struct HermeticClangProbe {
    host: SharedStr,
    clang_path: SharedStr,
    shelf_dir: SharedStr,
    shelf_digest: [u8; 32],
    sdk_dir: Option<SharedStr>,
    sdk_digest: Option<[u8; 32]>,
    io: bstd::io::Io,
    shelf_dir_cap: bstd::fs::Dir<bstd::fs::ReadWrite>,
    work_base_dir: bstd::fs::Dir<bstd::fs::ReadWrite>,
    work_rel: SharedStr,
    work_root: SharedStr,
    deadline: Duration,
    verify_flight: bstd::sync::Flight<(), VerifiedClang, OwnArc<str, SharedAlloc<'static>>, SharedAlloc<'static>>,
    root_reader: Mutex<Option<bstd::procs::hermetic::RootReader>>,
}

#[cfg(target_os = "linux")]
impl HermeticClangProbe {
    /// Base strategy id of this answerer, derived directly from the complete transitive
    /// implementation source bytes of `probe_clang.rs`, `spawn_route.rs`, `record.rs`,
    /// `c_tasks_probe.rs`, and every runner/launcher/confinement source file across `blaze-std`,
    /// `blaze-core`, and `blaze-sys` (AGENTS.md rule 69).
    pub fn strategy_id() -> ContentId {
        let mut h = blaze_ids::Hasher::new(HERMETIC_STRATEGY_DOMAIN);
        h.part(include_bytes!("probe_clang.rs"))
            .part(include_bytes!("spawn_route.rs"))
            .part(include_bytes!("record.rs"))
            .id(&c_tasks_probe::implementation_id());
        for group in bstd::procs::hermetic::TRANSITIVE_CONFINEMENT_SOURCE {
            for part in *group {
                h.part(part);
            }
        }
        h.finish()
    }

    /// Strategy id bound to the selected `clang-23` shelf member (`shelf_path` + member SHA-256
    /// from `pins/clang.json`) AND the host ELF runtime closure of `clang_path` under deadline
    /// `cx` (on both NixOS and standard FHS Linux hosts: resolved loader `PT_INTERP`, `nix-ld` /
    /// glibc `ld.so` or `/etc/ld.so.cache`, and transitive shared library paths and contents).
    /// Fallible: any I/O, permission, or deadline failure while reading the runtime closure is
    /// propagated loudly instead of forging a cache key (AGENTS.md rules 18, 21).
    pub fn strategy_id_for_clang<'a>(
        cx: &bstd::cx::Cx,
        host: &str,
        clang_path: &str,
        alloc: Alloc<'a>,
    ) -> Result<ContentId, AString<'a>> {
        Self::strategy_id_for_clang_inner(None, cx, host, clang_path, alloc)
    }

    /// Same as [`Self::strategy_id_for_clang`], reusing a caller-owned
    /// [`bstd::procs::hermetic::RootReader`] so cancelled/stalled reads never allocate or leak a
    /// fresh async driver per probe.
    pub fn strategy_id_for_clang_with<'a>(
        reader: &bstd::procs::hermetic::RootReader,
        cx: &bstd::cx::Cx,
        host: &str,
        clang_path: &str,
        alloc: Alloc<'a>,
    ) -> Result<ContentId, AString<'a>> {
        Self::strategy_id_for_clang_inner(Some(reader), cx, host, clang_path, alloc)
    }

    fn strategy_id_for_clang_inner<'a>(
        reader: Option<&bstd::procs::hermetic::RootReader>,
        cx: &bstd::cx::Cx,
        host: &str,
        clang_path: &str,
        alloc: Alloc<'a>,
    ) -> Result<ContentId, AString<'a>> {
        let base = Self::strategy_id();
        let prog = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23")
            .map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let mut h = blaze_ids::Hasher::new("blaze-build-script c-probe runtime closure 2");
        h.id(&base)
            .part(host.as_bytes())
            .part(prog.shelf_path.as_bytes())
            .part(&prog.sha256);
        let engaged = match reader {
            Some(r) => bstd::procs::hermetic::feed_nix_runtime_closure_with(r, cx, clang_path, alloc, &mut |chunk| {
                h.update(chunk);
            }),
            None => bstd::procs::hermetic::feed_nix_runtime_closure(cx, clang_path, alloc, &mut |chunk| {
                h.update(chunk);
            }),
        }
        .map_err(|fault| own_fmt(format_args!("c-probe runtime closure for `{clang_path}`: {fault:?}"), alloc))?;
        if engaged {
            h.part(b"linux-elf-closure-ok");
        } else {
            h.part(b"no-elf-closure");
        }
        Ok(h.finish())
    }

    /// Strategy id for the pinned `clang-23` shelf beneath `home` on `host`, bounded by `cx`,
    /// used by `sweep.rs` when revalidating recorded `cc-probe` read subjects.
    pub fn strategy_id_for_home<'a>(
        cx: &bstd::cx::Cx,
        home: &str,
        host: &str,
        alloc: Alloc<'a>,
    ) -> Result<Option<ContentId>, AString<'a>> {
        Self::strategy_id_for_home_inner(None, cx, home, host, alloc)
    }

    /// Same as [`Self::strategy_id_for_home`], reusing a caller-owned
    /// [`bstd::procs::hermetic::RootReader`].
    pub fn strategy_id_for_home_with<'a>(
        reader: &bstd::procs::hermetic::RootReader,
        cx: &bstd::cx::Cx,
        home: &str,
        host: &str,
        alloc: Alloc<'a>,
    ) -> Result<Option<ContentId>, AString<'a>> {
        Self::strategy_id_for_home_inner(Some(reader), cx, home, host, alloc)
    }

    fn strategy_id_for_home_inner<'a>(
        reader: Option<&bstd::procs::hermetic::RootReader>,
        cx: &bstd::cx::Cx,
        home: &str,
        host: &str,
        alloc: Alloc<'a>,
    ) -> Result<Option<ContentId>, AString<'a>> {
        let Ok(pin) = toolchain::clang_pin(CLANG_JSON, host) else {
            return Ok(None);
        };
        let Ok(prog) = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23") else {
            return Ok(None);
        };
        let shelf_hex = hex_bytes(&pin.sha256, alloc);
        let clang_path = Path::new(home)
            .join(crate::native_setting::SHELF_DIR)
            .join(shelf_hex.as_str())
            .join(prog.shelf_path);
        let Some(path_str) = clang_path.to_str() else {
            return Err(own_str("c-probe clang shelf path is not UTF-8", alloc));
        };
        Self::strategy_id_for_clang_inner(reader, cx, host, path_str, alloc).map(Some)
    }

    /// Opens [`HermeticClangProbe`] for `host` using already-open deadline-aware directory
    /// capabilities (`io`, `shelf_dir_cap`, `work_base_dir`) derived from the command's `bstd::fs::Fs`.
    /// Performs zero synchronous `std::fs` calls and never opens a `NativeWorld` outside a context:
    /// both work-directory creation and `clang-23` inode verification run lazily under the probe
    /// deadline on the first cache miss.
    pub fn open_from_shelf<'a>(
        home: &str,
        host: &str,
        sdk: Option<(&str, &str)>,
        io: bstd::io::Io,
        shelf_dir_cap: bstd::fs::Dir<bstd::fs::ReadWrite>,
        work_base_dir: bstd::fs::Dir<bstd::fs::ReadWrite>,
        work_rel: &str,
        work_root: PathBuf,
        alloc: Alloc<'a>,
    ) -> Result<(HermeticClangProbe, [u8; 32]), AString<'a>> {
        let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let shelf_hex = hex_bytes(&pin.sha256, alloc);
        let shelf_path = Path::new(home).join(crate::native_setting::SHELF_DIR).join(shelf_hex.as_str());
        let prog = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23").map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let clang_candidate = shelf_path.join(prog.shelf_path);
        let shelf_str = path_utf8(&shelf_path, alloc)?;
        let clang_str = path_utf8(&clang_candidate, alloc)?;
        let work_str = path_utf8(&work_root, alloc)?;
        let (sdk_dir, sdk_digest) = match sdk {
            Some((dir, id)) if !dir.is_empty() => {
                let dig = ContentId::of("blaze c-probe sdk root 1", id.as_bytes()).0;
                (Some(OwnString::from_str_in(dir, SharedAlloc::global())), Some(dig))
            }
            _ => (None, None),
        };
        let probe = HermeticClangProbe {
            host: OwnString::from_str_in(host, SharedAlloc::global()),
            clang_path: clang_str,
            shelf_dir: shelf_str,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            io,
            shelf_dir_cap,
            work_base_dir,
            work_rel: OwnString::from_str_in(work_rel, SharedAlloc::global()),
            work_root: work_str,
            deadline: PROBE_DEADLINE,
            verify_flight: bstd::sync::Flight::with_capacity_in(
                "c-probe.clang.verify",
                NonZeroUsize::MIN,
                SharedAlloc::global(),
            ),
            root_reader: Mutex::new(None),
        };
        Ok((probe, pin.sha256))
    }

    /// Opens [`HermeticClangProbe`] directly from an explicit shelf folder and pre-opened
    /// directory capabilities (for differential tests), performing zero synchronous `std::fs`
    /// calls at setup.
    pub fn from_shelf_dir<'a>(
        io: bstd::io::Io,
        shelf_dir_cap: bstd::fs::Dir<bstd::fs::ReadWrite>,
        work_base_dir: bstd::fs::Dir<bstd::fs::ReadWrite>,
        work_rel: &str,
        shelf_dir: &Path,
        host: &str,
        sdk: Option<(&Path, &str)>,
        work_root: PathBuf,
        alloc: Alloc<'a>,
    ) -> Result<HermeticClangProbe, AString<'a>> {
        let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let prog = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23").map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let clang_candidate = shelf_dir.join(prog.shelf_path);
        let shelf_str = path_utf8(shelf_dir, alloc)?;
        let clang_str = path_utf8(&clang_candidate, alloc)?;
        let work_str = path_utf8(&work_root, alloc)?;
        let (sdk_dir, sdk_digest) = match sdk {
            Some((dir, id)) => {
                let dir_str = path_utf8(dir, alloc)?;
                let dig = ContentId::of("blaze c-probe sdk root 1", id.as_bytes()).0;
                (Some(dir_str), Some(dig))
            }
            None => (None, None),
        };
        Ok(HermeticClangProbe {
            host: OwnString::from_str_in(host, SharedAlloc::global()),
            clang_path: clang_str,
            shelf_dir: shelf_str,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            io,
            shelf_dir_cap,
            work_base_dir,
            work_rel: OwnString::from_str_in(work_rel, SharedAlloc::global()),
            work_root: work_str,
            deadline: PROBE_DEADLINE,
            verify_flight: bstd::sync::Flight::with_capacity_in(
                "c-probe.clang.verify",
                NonZeroUsize::MIN,
                SharedAlloc::global(),
            ),
            root_reader: Mutex::new(None),
        })
    }

    /// Ensures the probe work directory exists through the command's already-open `bstd::fs::Dir`
    /// capability under a `Cx` strictly bounded by `self.deadline` (zero `native_world()` calls
    /// outside a context, zero `std::fs::create_dir_all`).
    fn ensure_work_dir<'a>(
        &self,
        cx: &bstd::cx::Cx,
        alloc: Alloc<'a>,
    ) -> Result<bstd::fs::Dir<bstd::fs::ReadWrite>, AString<'a>> {
        let mut dir = self.work_base_dir.share();
        for seg in self.work_rel.as_str().split('/').filter(|s| !s.is_empty()) {
            let rel = bstd::path::Path::new(seg)
                .map_err(|_| own_fmt(format_args!("invalid c-probe work dir segment `{seg}`"), alloc))?;
            dir = match dir.make_dir(cx, &rel, bstd::fs::Durable::No) {
                Ok(sub) => sub,
                Err(e) if e.kind == bstd::fs::FsKind::Exists => dir.subdir(&rel),
                Err(e) => return Err(own_fmt(format_args!("create c-probe work dir `{seg}`: {e}"), alloc)),
            };
        }
        Ok(dir)
    }

    /// Verifies `clang-23`'s presence and SHA-256 against `pins/clang.json` lazily on the first
    /// cache miss via single-flight [`bstd::sync::Flight`] strictly bounded by `self.deadline`:
    /// - No lock is ever held across filesystem waits: `Flight` drops its table lock before
    ///   running verification and parks concurrent askers on a scheduler `Signal` under their own
    ///   `&Cx` deadline.
    /// - Opens `clang-23` ONCE as a kernel-sealed `MFD_EXEC` [`bstd::procs::hermetic::VerifiedExec`]
    ///   (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE`), hashes/validates that exact
    ///   snapshot, and retains its `OwnedFd` so Landlock and `execveat(..., AT_EMPTY_PATH)`
    ///   execute that exact verified snapshot with zero TOCTOU.
    fn ensure_verified<'a>(
        &self,
        clock: &bstd::clock::Clock,
        began: bstd::clock::Instant,
        alloc: Alloc<'a>,
    ) -> Result<VerifiedClang, AString<'a>> {
        let remaining = || -> Option<Duration> {
            let elapsed = clock.now().since(began).unwrap_or(Duration::ZERO);
            self.deadline.checked_sub(elapsed).filter(|d| !d.is_zero())
        };
        let Some(rem) = remaining() else {
            return Err(own_str("c-probe clang verification timed out before starting", alloc));
        };
        let verify_cx = self
            .io
            .cx(rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe verify context stopped", alloc))?;
        let shared = SharedAlloc::global();
        self.verify_flight
            .ask(&verify_cx, (), |vcx| {
                let fail = |msg: AString<'_>| OwnArc::from_str_in(msg.as_str(), shared);
                let prog = shelf_pins::shelf_program(CLANG_JSON, self.host.as_str(), "clang-23")
                    .map_err(|e| fail(own_fmt(format_args!("{e}"), alloc)))?;
                let shelf_hex = hex_bytes(&self.shelf_digest, alloc);
                let clang_rel = bstd::path::Path::new(prog.shelf_path)
                    .map_err(|_| fail(own_fmt(format_args!("invalid shelf path `{}`", prog.shelf_path), alloc)))?;
                let mut hasher = blaze_sha256::Sha256::new();
                let exec = bstd::procs::hermetic::VerifiedExec::open_and_seal(
                    vcx,
                    &self.shelf_dir_cap,
                    &clang_rel,
                    |chunk| {
                        hasher.update(chunk);
                    },
                )
                .map_err(|e| {
                    fail(own_fmt(
                        format_args!(
                            "pinned clang-23 shelf binary missing or unreadable at {} (expected archive sha256 {shelf_hex} from pins/clang.json): {e}",
                            self.clang_path.as_str()
                        ),
                        alloc,
                    ))
                })?;
                let got_sha = hasher.finish();
                shelf_pins::check_member(self.host.as_str(), prog, got_sha)
                    .map_err(|e| fail(own_fmt(format_args!("{e}"), alloc)))?;
                Ok(VerifiedClang { exec: OwnArc::new_in(exec, shared) })
            })
            .map_err(|err| match err {
                bstd::sync::FlightError::Failed { error, .. } => own_str(error.as_ref(), alloc),
                bstd::sync::FlightError::Stopped(stopped) => {
                    own_fmt(format_args!("c-probe clang verification timed out waiting on flight: {stopped}"), alloc)
                }
                other => own_fmt(format_args!("c-probe clang verification failed: {other}"), alloc),
            })
    }
}

/// Opens the command's [`CProbeSetting`] once at the command's edge (rule 75) through the
/// command's existing deadline-aware [`bstd::fs::Fs`] capability (never opening a separate
/// `NativeWorld` outside a context):
/// - On Linux, uses `DirStore::build_scripts` for `c-probe/<64-hex>` records and
///   [`HermeticClangProbe`] (kernel-sealed `MFD_EXEC` + `execveat`).
/// - On non-Linux hosts, returns [`CProbeSetting::Unavailable`] (non-Linux hosts continue using
///   `pins/answers/clang.json` directly in `spawn_route::ScriptSandbox::spawn_probe`).
/// - On hosts without a clang pin, returns [`CProbeSetting::Unavailable`] immediately with zero
///   filesystem I/O.
pub fn open_command_c_probes<'a>(
    cx: &bstd::cx::Cx,
    fs: &bstd::fs::Fs,
    home: &str,
    host: &str,
    sdk: Option<(&str, &str)>,
    alloc: Alloc<'a>,
) -> CProbeSetting {
    let shared = SharedAlloc::global();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cx, fs, home, host, sdk, alloc);
        CProbeSetting::Unavailable(OwnArc::from_str_in(
            "hermetic live C compiler probes require Linux kernel-sealed execveat (MFD_EXEC); non-Linux hosts use pins/answers/clang.json",
            shared,
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let pin = match toolchain::clang_pin(CLANG_JSON, host) {
            Ok(pin) => pin,
            Err(e) => {
                let msg = own_fmt(format_args!("{e}"), alloc);
                return CProbeSetting::Unavailable(OwnArc::from_str_in(msg.as_str(), shared));
            }
        };
        if let Err(e) = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23") {
            let msg = own_fmt(format_args!("{e}"), alloc);
            return CProbeSetting::Unavailable(OwnArc::from_str_in(msg.as_str(), shared));
        }
        let Ok(v2_rel) = bstd::path::Path::new(".blaze/v2") else {
            return CProbeSetting::Unavailable(OwnArc::from_str_in("invalid .blaze/v2 path", shared));
        };
        let blaze_v2 = match fs.home(cx, home, &v2_rel) {
            Ok(dir) => dir,
            Err(e) => {
                let msg = own_fmt(format_args!("c-probe home capability: {e}"), alloc);
                return CProbeSetting::Unavailable(OwnArc::from_str_in(msg.as_str(), shared));
            }
        };
        let io = blaze_v2.world().share();
        let Some(home_fs) = io.fs() else {
            return CProbeSetting::Unavailable(OwnArc::from_str_in("c-probe home capability has no fs", shared));
        };
        let home_root = home_fs.root();
        // `DirStore::build_scripts` appends `build-script/records` itself, so pass the `.blaze/v2`
        // root directly (matching `sweep.rs` reading `v2/build-script/records/c-probe/<hex>`).
        let records: OwnArc<dyn CProbeStore, SharedAlloc<'static>> =
            OwnArc::new_in(DirStore::build_scripts(io.spawner(), io.boot().ok(), &blaze_v2), shared);
        let shelf_hex = hex_bytes(&pin.sha256, alloc);
        let shelf_sub_str = own_fmt(format_args!("toolchain/clang/{}", shelf_hex.as_str()), alloc);
        let Ok(shelf_sub) = bstd::path::Path::new(shelf_sub_str.as_str()) else {
            return CProbeSetting::Unavailable(OwnArc::from_str_in("invalid c-probe shelf path", shared));
        };
        let shelf_dir_cap = blaze_v2.subdir(&shelf_sub);
        let work_rel = own_fmt(format_args!("{}/c-probes", crate::native_setting::NATIVE_WORK_DIR), alloc);
        let work_root = Path::new(home).join(crate::native_setting::NATIVE_WORK_DIR).join("c-probes");
        let (hermetic, clang_sha256) = match HermeticClangProbe::open_from_shelf(
            home,
            host,
            sdk,
            io,
            shelf_dir_cap,
            home_root,
            work_rel.as_str(),
            work_root,
            alloc,
        ) {
            Ok(pair) => pair,
            Err(why) => return CProbeSetting::Unavailable(OwnArc::from_str_in(why.as_str(), shared)),
        };
        let (sdk_dir_str, sdk_str) = sdk.map_or(("", ""), |(dir, id)| (dir, id));
        let sdk_dir: OwnArc<str, SharedAlloc<'static>> = OwnArc::from_str_in(sdk_dir_str, shared);
        let sdk_id: OwnArc<str, SharedAlloc<'static>> = OwnArc::from_str_in(sdk_str, shared);
        let sdk_digest = facts::sdk_id_digest(sdk_str);
        let answerer: OwnArc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> = OwnArc::new_in(hermetic, shared);
        CProbeSetting::Ready(CScriptProbes { answerer, records, sdk_dir, sdk_id, sdk_digest, clang_sha256 })
    }
}

/// Host's pinned clang archive SHA-256 from `pins/clang.json`, or `None` when `host` has no pin.
pub fn active_clang_pin(host: &str) -> Option<[u8; 32]> {
    toolchain::clang_pin(CLANG_JSON, host).ok().map(|pin| pin.sha256)
}

/// Re-checks a recorded `cc-probe\n<answerer>\n<clang>\n<sdk>\n<key>` read subject against the
/// command's active `(answerer_id, clang_sha256, sdk_digest)` and `store`:
/// - `Ok(None)`: `subject` is not a `cc-probe` subject, or no store was provided.
/// - `Ok(Some(None))`: active answerer, clang pin, or SDK changed, or the record is genuinely
///   absent/corrupt (differs from any recorded digest, so only scripts that asked this probe re-run).
/// - `Ok(Some(Some(digest)))`: active identity matches and cached canonical probe answer hashes to
///   `digest`.
/// - `Err(why)`: reading from `store` failed with a real [`CProbeStoreError`] (e.g. I/O,
///   permission, or deadline error), propagated instead of silently downgrading to a cache miss.
pub fn c_probe_reply_now<'a>(
    subject: &str,
    active_answerer: &ContentId,
    active_clang: &[u8; 32],
    active_sdk: &[u8; 32],
    store: Option<&dyn CProbeStore>,
    alloc: Alloc<'a>,
) -> Result<Option<Option<ContentId>>, AString<'a>> {
    let Some(parsed) = facts::parse_cc_probe_subject(subject) else {
        return Ok(None);
    };
    if parsed.answerer != *active_answerer || parsed.clang != *active_clang || parsed.sdk != *active_sdk {
        return Ok(Some(None));
    }
    let Some(store) = store else {
        return Ok(None);
    };
    let Some(bytes) = store
        .get(&parsed.key, alloc)
        .map_err(|e| own_fmt(format_args!("{e}"), alloc))?
    else {
        return Ok(Some(None));
    };
    let Ok(ans) = decode_answer(bytes.as_slice(), alloc) else {
        return Ok(Some(None));
    };
    Ok(Some(Some(canonical_reply_digest(&ans))))
}

/// Bounded RAII cleanup of a per-probe task folder through deadline-aware `bstd::fs` (zero
/// unbounded `std::fs::remove_dir_all`). On the result-bearing success path [`Self::cleanup`]
/// removes the folder explicitly and propagates any cleanup error; `Drop` serves as a bounded
/// fallback on early error/unwind paths.
#[cfg(any(target_os = "linux", test))]
struct TaskDirGuard<'w> {
    io: &'w bstd::io::Io,
    work_root: &'w bstd::fs::Dir<bstd::fs::ReadWrite>,
    task_rel: bstd::path::Path,
    cleaned: bool,
}

#[cfg(any(target_os = "linux", test))]
impl TaskDirGuard<'_> {
    fn cleanup<'a>(mut self, cx: &bstd::cx::Cx, alloc: Alloc<'a>) -> Result<(), AString<'a>> {
        let task_dir = self.work_root.subdir(&self.task_rel);
        let entries = task_dir
            .own_entries(cx)
            .map_err(|e| own_fmt(format_args!("list c-probe task dir for cleanup: {e}"), alloc))?;
        for i in 0..entries.len() {
            let p = entries
                .path(i)
                .map_err(|e| own_fmt(format_args!("c-probe cleanup entry path: {e}"), alloc))?;
            task_dir
                .remove(cx, &p, bstd::fs::Durable::No)
                .map_err(|e| own_fmt(format_args!("remove c-probe task entry `{}`: {e}", p.as_str()), alloc))?;
        }
        self.work_root
            .remove_dir(cx, &self.task_rel, bstd::fs::Durable::No)
            .map_err(|e| own_fmt(format_args!("remove c-probe task dir: {e}"), alloc))?;
        self.cleaned = true;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
impl Drop for TaskDirGuard<'_> {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
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
#[cfg(target_os = "linux")]
struct ProbeFiles;

#[cfg(target_os = "linux")]
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

/// Extracts or defaults `SOURCE_DATE_EPOCH` for the hermetic sandbox clock (`ClockSpec`), using
/// `ClockSpec::from_source_date_epoch` (the exact same parser `HermeticSpec::new` validates) so
/// leading-zero (`01`), invalid, or out-of-range values resolve to `ClockSpec::InvalidEnv` and
/// reach Clang cleanly.
#[cfg(any(target_os = "linux", test))]
fn resolve_source_date_epoch<'a>(
    owned_env: &mut AVec<'a, (AString<'a>, AString<'a>)>,
    alloc: Alloc<'a>,
) -> ClockSpec {
    if let Some((_, val)) = owned_env.iter().find(|(k, _)| k.as_str() == "SOURCE_DATE_EPOCH") {
        ClockSpec::from_source_date_epoch(val.as_str())
    } else {
        owned_env.push((own_str("SOURCE_DATE_EPOCH", alloc), own_str("0", alloc)));
        ClockSpec::Fixed { unix_secs: 0 }
    }
}

#[cfg(target_os = "linux")]
impl HermeticClangProbe {
    fn shared_root_reader<'a>(
        &self,
        cx: &bstd::cx::Cx,
        alloc: Alloc<'a>,
    ) -> Result<bstd::procs::hermetic::RootReader, AString<'a>> {
        if let Some(existing) = crate::host::lock(&self.root_reader).as_ref()
            && !existing.is_stalled()
        {
            return Ok(existing.share());
        }
        let created = bstd::procs::hermetic::RootReader::new(cx)
            .map_err(|fault| own_fmt(format_args!("c-probe root reader: {fault:?}"), alloc))?;
        let mut slot = crate::host::lock(&self.root_reader);
        if slot.as_ref().is_none_or(|r| r.is_stalled()) {
            *slot = Some(created);
        }
        match slot.as_ref() {
            Some(r) => Ok(r.share()),
            None => Err(own_str("c-probe root reader unavailable", alloc)),
        }
    }

    fn live_strategy_id<'a>(&self, cx: &bstd::cx::Cx, alloc: Alloc<'a>) -> Result<ContentId, AString<'a>> {
        let reader = self.shared_root_reader(cx, alloc)?;
        Self::strategy_id_for_clang_with(&reader, cx, self.host.as_str(), self.clang_path.as_str(), alloc)
    }
}

#[cfg(target_os = "linux")]
impl ProbeAnswerer for HermeticClangProbe {
    fn id<'a>(&self, alloc: Alloc<'a>) -> Result<ContentId, AString<'a>> {
        // Recompute the live runtime-closure identity on every lookup so a mid-command
        // `/run/current-system` switch invalidates cache hits as well as misses.
        let cx = self
            .io
            .cx(self.deadline, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe strategy id context stopped", alloc))?;
        self.live_strategy_id(&cx, alloc)
    }

    fn answer<'a, 'b>(
        &self,
        probe: &CanonicalProbe<'_, '_>,
        input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
        alloc: Alloc<'a>,
    ) -> Result<ProbeAnswer<'a>, AString<'a>> {
        let clock = self
            .io
            .need_clock()
            .map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let probe_began = clock.now();
        let remaining = || -> Option<Duration> {
            let elapsed = clock.now().since(probe_began).unwrap_or(Duration::ZERO);
            self.deadline.checked_sub(elapsed).filter(|d| !d.is_zero())
        };
        let Some(init_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted before work dir setup", alloc));
        };
        let init_cx = self
            .io
            .cx(init_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe work dir context stopped", alloc))?;
        let work_root = self.ensure_work_dir(&init_cx, alloc)?;
        let verified = self.ensure_verified(&clock, probe_began, alloc)?;

        let Some(stage_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted before staging", alloc));
        };
        let stage_cx = self
            .io
            .cx(stage_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe stage context stopped", alloc))?;
        let (dir_name, task_rel, task_dir) = make_unique_task_dir(&work_root, &clock, &stage_cx, alloc)?;
        let task_real = own_fmt(format_args!("{}/{}", self.work_root.as_str(), dir_name.as_str()), alloc);
        let guard = TaskDirGuard { io: &self.io, work_root: &work_root, task_rel, cleaned: false };

        let run_res = self.run_in_task_dir(
            probe,
            input_bytes,
            &verified,
            &clock,
            &remaining,
            &stage_cx,
            &task_dir,
            task_real.as_str(),
            alloc,
        );
        let cleanup_res = match remaining() {
            Some(cleanup_rem) => match self.io.cx(cleanup_rem, bstd::mem::TASK_FRAME) {
                Ok(cleanup_cx) => guard.cleanup(&cleanup_cx, alloc),
                Err(_) => Err(own_str("c-probe cleanup context stopped", alloc)),
            },
            None => Err(own_str("c-probe deadline exhausted before cleanup", alloc)),
        };
        finish_with_cleanup(run_res, cleanup_res, alloc)
    }
}

/// Combines a probe run outcome with its mandatory task-directory cleanup outcome. When BOTH fail,
/// the combined error deliberately does NOT start with [`STREAM_OVERFLOW_PREFIX`] so
/// `is_stream_overflow_error` never reroutes past a failed cleanup and silently drops stale files.
#[cfg(any(target_os = "linux", test))]
fn finish_with_cleanup<'a, T>(
    run_res: Result<T, AString<'a>>,
    cleanup_res: Result<(), AString<'a>>,
    alloc: Alloc<'a>,
) -> Result<T, AString<'a>> {
    match (run_res, cleanup_res) {
        (Ok(val), Ok(())) => Ok(val),
        (Ok(_), Err(clean_err)) => Err(clean_err),
        (Err(run_err), Ok(())) => Err(run_err),
        (Err(run_err), Err(clean_err)) => Err(own_fmt(
            format_args!("task-dir cleanup failed after probe error ({run_err}): {clean_err}"),
            alloc,
        )),
    }
}

#[cfg(target_os = "linux")]
impl HermeticClangProbe {
    #[expect(clippy::too_many_arguments, reason = "internal helper factoring result-bearing cleanup around probe execution")]
    fn run_in_task_dir<'a, 'b>(
        &self,
        probe: &CanonicalProbe<'_, '_>,
        input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
        verified: &VerifiedClang,
        clock: &bstd::clock::Clock,
        remaining: &dyn Fn() -> Option<Duration>,
        stage_cx: &bstd::cx::Cx,
        task_dir: &bstd::fs::Dir<bstd::fs::ReadWrite>,
        task_real: &str,
        alloc: Alloc<'a>,
    ) -> Result<ProbeAnswer<'a>, AString<'a>> {
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
            let host_file = own_fmt(format_args!("{task_real}/{}", local_name.as_str()), alloc);
            task_dir
                .publish(stage_cx, &file_rel, bstd::fs::Content::bytes(bytes), bstd::fs::Durable::No)
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
                    let host_file = own_fmt(format_args!("{task_real}/{local_name}"), alloc);
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
        // Pin Clang's builtin resource directory explicitly to the shelf since `argv[0]` carries
        // the requested compiler spelling (`cc`, `c++`, `gcc`, etc.) rather than `<shelf>/bin/clang-23`.
        owned_args.push(own_str("-resource-dir", alloc));
        owned_args.push(own_fmt(format_args!("{}/lib/clang/23", self.shelf_dir.as_str()), alloc));

        let mut has_sysroot = false;
        let sdk_str = self.sdk_dir.as_ref().map(OwnString::as_str);
        for segs in probe.exec_args.iter() {
            let rendered = render_segments(segs.as_slice(), task_real, sdk_str, placeholders.as_slice(), alloc)?;
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
                render_segments(segs.as_slice(), task_real, sdk_str, placeholders.as_slice(), alloc)?,
            ));
        }
        // Always give Clang its own confined output directory as `TMPDIR` (ambient host `TMPDIR`
        // values are intentionally excluded from `CLANG_ENV` / `ProbeKey` for machine independence).
        owned_env.push((own_str("TMPDIR", alloc), own_str(task_real, alloc)));
        let epoch_clock = resolve_source_date_epoch(&mut owned_env, alloc);
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

        let spawner = self
            .io
            .need_host()
            .and_then(|h| h.spawner())
            .map_err(|m| own_fmt(format_args!("c-probe spawner: {}", m.capability), alloc))?;
        let driver_clock = self
            .io
            .need_clock()
            .map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: driver_clock });
        let Some(run_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted during clang verification", alloc));
        };
        let cx = self
            .io
            .cx(run_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe context stopped", alloc))?;

        let exec_path = self.clang_path.as_str();
        let spec = HermeticSpec::new(HermeticOpts {
            program: Program::Stored { name: "clang-23", digest: Digest(self.shelf_digest), path: exec_path },
            args: arg_refs.as_slice(),
            env: env_refs.as_slice(),
            roots,
            mounts: mounts_vec.as_slice(),
            cwd: task_real,
            helpers: &[],
            helper_folders: &[],
            channels: &[],
            clock: epoch_clock,
            seed: 0,
            budget: Budget { cpu: run_rem, memory: PROBE_MEMORY_BYTES, steps: None },
            deadline: Deadline::after(clock.now().since_start(), run_rem),
            streams: HermeticStreams { stdout: Capture::Keep { most: PROBE_MOST_STREAM }, stderr: Capture::Keep { most: PROBE_MOST_STREAM } },
            level: Level::Inputs,
            report: ReportSpec::WholeRoots,
        })
        .map_err(|e| own_fmt(format_args!("c-probe spec refused: {:?} ({:?})", e.fault, e.rule), alloc))?;
        // Re-hash the Linux ELF runtime closure under `cx` immediately before launching to ensure
        // `/run/current-system` or host shared libraries did not switch since `probe.answerer_id`
        // was computed. `verified.exec` itself is already a kernel-sealed immutable `memfd`.
        let pre_id = self.live_strategy_id(&cx, alloc)?;
        if pre_id != probe.answerer_id {
            return Err(own_str("host runtime closure changed before probe execution (rule 21)", alloc));
        }
        let spec = spec
            .with_arg0(probe.row.program.as_str())
            .with_verified_exec(&verified.exec);

        let ran = hermetic.run(&cx, &spec, &ProbeFiles, alloc).map_err(|e| match e.fault {
            Fault::Stopped(HermeticStop::Stream { stream, most }) => own_fmt(
                format_args!("{STREAM_OVERFLOW_PREFIX} {:?} exceeded {} bytes", stream, most),
                alloc,
            ),
            _ => own_fmt(format_args!("c-probe hermetic run failed: {:?} ({:?})", e.fault, e.rule), alloc),
        })?;

        let Some(post_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted before post-exec runtime closure revalidation", alloc));
        };
        let post_cx = self
            .io
            .cx(post_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe post-exec context stopped", alloc))?;
        let post_id = self.live_strategy_id(&post_cx, alloc)?;
        if post_id != probe.answerer_id {
            return Err(own_str("host runtime closure changed during probe execution (rule 21)", alloc));
        }

        let code = match ran.exit {
            Exit::Exited(code) => code,
            Exit::Signaled(sig) => return Err(own_fmt(format_args!("c-probe clang-23 killed by signal {sig}"), alloc)),
        };

        let raw_out = core::str::from_utf8(ran.stdout.as_slice()).map_err(|_| own_str("c-probe stdout is not UTF-8", alloc))?;
        let raw_err = core::str::from_utf8(ran.stderr.as_slice()).map_err(|_| own_str("c-probe stderr is not UTF-8", alloc))?;

        // Normalize host paths back to canonical placeholders (longest match first) while escaping
        // literal `$` -> `$$` so literal `$OUT_DIR`/`$IN0.c` in Clang output round-trip verbatim
        // through `CanonicalProbe::unspell` and `<shelf>/bin` (`InstalledDir:`) never leaks `$HOME`.
        // Driver diagnostics already carry the requested compiler spelling natively via `with_arg0`,
        // so user-produced preprocessor output (e.g. `clang-23: marker`) is never rewritten.
        let requested = probe.row.program.as_str();
        let stdout = normalize_reply(raw_out, requested, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real, sdk_str, placeholders.as_slice(), alloc);
        let stderr = normalize_reply(raw_err, requested, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real, sdk_str, placeholders.as_slice(), alloc);
        Ok(ProbeAnswer { code, stdout, stderr })
    }
}

/// Stable placeholder replacing host clang shelf root paths (e.g. `InstalledDir: $CLANG_SHELF/bin`).
#[cfg(any(target_os = "linux", test))]
const CLANG_SHELF: &str = "$CLANG_SHELF";

#[cfg(any(target_os = "linux", test))]
#[expect(clippy::too_many_arguments, reason = "explicit host-root normalization arguments")]
fn normalize_reply<'a>(
    raw: &str,
    requested_program: &str,
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
    let driver_name = if requested_program.is_empty() { "clang-23" } else { requested_program };
    rev_pairs.push((clang_path, driver_name));
    rev_pairs.push((shelf_dir, CLANG_SHELF));
    rev_pairs.push((task_dir, OUT_DIR));
    if let Some(sdk) = sdk_dir {
        rev_pairs.push((sdk, SDKROOT));
    }
    rev_pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()));
    replace_pairs_escape_dollar(raw, rev_pairs.as_slice(), alloc)
}

#[cfg(target_os = "linux")]
fn render_segments<'a>(
    segs: &[Segment<'_>],
    task_dir: &str,
    sdk_dir: Option<&str>,
    placeholders: &[(AString<'a>, AString<'a>)],
    alloc: Alloc<'a>,
) -> Result<AString<'a>, AString<'a>> {
    let mut out = OwnString::new_in(alloc);
    for seg in segs {
        match seg {
            Segment::Literal(lit) => out.push_str(lit.as_str()),
            Segment::OutDir => out.push_str(task_dir),
            Segment::SdkRoot => match sdk_dir {
                Some(sdk) => out.push_str(sdk),
                None => {
                    return Err(own_str(
                        "c-probe references SDKROOT without a configured SDK root (rule 3)",
                        alloc,
                    ));
                }
            },
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
    Ok(out)
}

/// Creates a crash- and PID-reuse-safe per-probe task folder under `work_root`, mixing wall-clock
/// nanoseconds with `pid` and `PROBE_SEQ` and retrying on `FsKind::Exists` so stale directories
/// left by prior crashes never abort a valid probe.
#[cfg(any(target_os = "linux", test))]
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

#[cfg(target_os = "linux")]
fn placeholder_filename<'a>(spelled: &str, alloc: Alloc<'a>) -> AString<'a> {
    if let Some(rest) = spelled.strip_prefix(c_tasks_probe::INPUT) {
        own_fmt(format_args!("in{rest}"), alloc)
    } else if let Some(rest) = spelled.strip_prefix(c_tasks_probe::OUTPUT) {
        own_fmt(format_args!("out{rest}"), alloc)
    } else {
        own_str("file", alloc)
    }
}

#[cfg(any(target_os = "linux", test))]
fn replace_pairs_escape_dollar<'a>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'a>) -> AString<'a> {
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
            if ch == '$' {
                out.push_str("$$");
            } else {
                out.push(ch);
            }
            at += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

#[cfg(any(target_os = "linux", test))]
fn own_str<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    OwnString::from_str_in(s, alloc)
}

fn own_fmt<'a>(args: fmt::Arguments<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = OwnString::new_in(alloc);
    let _ = out.write_fmt(args);
    out
}

#[cfg(target_os = "linux")]
fn path_utf8<'a>(path: &Path, alloc: Alloc<'a>) -> Result<SharedStr, AString<'a>> {
    match path.to_str() {
        Some(s) => Ok(OwnString::from_str_in(s, SharedAlloc::global())),
        None => Err(own_fmt(format_args!("{}: path is not UTF-8", path.display()), alloc)),
    }
}

#[cfg(any(target_os = "linux", test))]
fn hex_nibble(n: u8) -> char {
    match n & 15 {
        d @ 0..=9 => char::from(b'0' + d),
        d => char::from(b'a' + (d - 10)),
    }
}

#[cfg(any(target_os = "linux", test))]
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
    #[cfg(target_os = "linux")]
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::sync::Arc;

    use blaze_build_script::c_tasks_probe::{self, ProbeAnswerer, Spawn, resolve};
    #[cfg(target_os = "linux")]
    use blaze_build_script::c_tasks_probe::{Answer, Table};
    use blaze_build_script::facts::{CcProbeSubject, cc_probe_subject, parse_cc_probe_subject};
    use blaze_build_script::toolchain::clang_pin;
    use blaze_pins::CLANG_JSON;

    #[cfg(target_os = "linux")]
    use super::HermeticClangProbe;
    use super::{
        CProbeSetting, CProbeStore as _, CacheAdapter, MemCProbeStore, c_probe_reply_now,
        open_command_c_probes, resolve_source_date_epoch,
    };

    /// Differential test (Rule 25 + D174): requires the unpacked Linux ARM64 `clang-23` shelf
    /// (`cargo test -p blaze-proc-macro --lib -- --ignored hermetic_clang_probe`). Fails hard if
    /// the shelf is absent instead of returning Ok without running assertions.
    #[cfg(target_os = "linux")]
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
        let work_name = format!("blaze-cprobe-diff-{}", std::process::id());
        let work_parent = std::env::temp_dir();
        let work = work_parent.join(&work_name);
        let (_, shelf_cap) = crate::record::native_world(&shelf).expect("shelf world");
        let (work_io, work_parent_cap) = crate::record::native_world(&work_parent).expect("work world");
        let answerer = HermeticClangProbe::from_shelf_dir(
            work_io,
            shelf_cap,
            work_parent_cap,
            &work_name,
            &shelf,
            host,
            None,
            work.clone(),
            alloc,
        )
        .expect("open hermetic probe");
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
            // Legacy `pins/answers/clang.json` rows were captured with `argv[0] = clang-23`;
            // live hermetic probes pass the requested compiler spelling (`cc`, `c++`, etc.) as
            // `argv[0]` natively, so compare stderr after translating the legacy corpus prefix.
            let legacy_stderr: std::string::String = stderr
                .as_str()
                .split_inclusive('\n')
                .map(|line| match line.strip_prefix("clang-23:") {
                    Some(rest) => format!("{}:{rest}", row.program.as_str()),
                    None => line.to_owned(),
                })
                .collect();
            assert_eq!(canonical.stdout.as_str(), stdout.as_str(), "stdout mismatch for {:?}", row.args);
            assert_eq!(canonical.stderr.as_str(), legacy_stderr.as_str(), "stderr mismatch for {:?}", row.args);
            if row.args.len() == 1 && row.args[0].as_str() == "-?" {
                let want_prefix = format!("{}: error:", row.program.as_str());
                assert!(
                    canonical.stderr.as_str().starts_with(want_prefix.as_str()),
                    "diagnostic must preserve requested driver spelling `{want_prefix}` natively via argv[0], got: {}",
                    canonical.stderr.as_str()
                );
            }

            let empty_sdk = blaze_build_script::facts::sdk_id_digest("");
            let aid = answerer.id(alloc).expect("answerer id");
            let subject = cc_probe_subject(&aid, &pin.sha256, &empty_sdk, &resolved.key, alloc);
            assert_eq!(
                parse_cc_probe_subject(subject.as_str()),
                Some(CcProbeSubject { answerer: aid, clang: pin.sha256, sdk: empty_sdk, key: resolved.key })
            );
            assert_eq!(
                c_probe_reply_now(subject.as_str(), &aid, &pin.sha256, &empty_sdk, Some(store.as_ref()), alloc),
                Ok(Some(Some(resolved.canonical_digest)))
            );
            checked += 1;
        }
        assert!(checked >= 27, "expected >= 27 Linux ARM64 rows checked, got {checked}");

        // User-produced preprocessor stdout beginning with `clang-23:` must NEVER be rewritten
        // because driver diagnostics get the requested `argv[0]` natively at launch.
        {
            let marker_src: &[u8] = b"clang-23: marker\n#pragma message \"clang\"\n";
            let marker_sha = blaze_sha256::digest(marker_src);
            let marker_out = "/target/debug/build/marker-1/out";
            let marker_path = format!("{marker_out}/1detect_compiler_family.c");
            let marker_args = ["-E", marker_path.as_str()];
            let sp_marker = Spawn {
                program: "cc",
                args: &marker_args,
                env: &[],
                out_dir: marker_out,
                sdk: "",
                mount: "/package",
                sdk_id: "",
                stdin: &[],
            };
            let dig = |rel: &str| (rel == "1detect_compiler_family.c").then_some(marker_sha);
            let mut bytes = |rel: &str| (rel == "1detect_compiler_family.c").then_some(marker_src);
            let res = resolve(&answerer, &cache, &pin.sha256, &sp_marker, dig, &mut bytes, alloc)
                .expect("marker family probe");
            assert_eq!(res.answered.code, 0);
            assert!(
                res.answered.stdout.as_str().lines().any(|line| line == "clang-23: marker"),
                "user-produced stdout line `clang-23: marker` must stay untouched, got:\n{}",
                res.answered.stdout.as_str()
            );
        }

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
            fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<blaze_ids::ContentId, blaze_own::AString<'a>> {
                Ok(blaze_ids::ContentId::of("stub-cprobe-selective-test", b"1"))
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
        let stub_id = StubAnswerer.id(alloc).expect("stub id");
        let subj_a = cc_probe_subject(&stub_id, &pin, &sdk_a, &res_a.key, alloc);
        let subj_b = cc_probe_subject(&stub_id, &pin, &sdk_a, &res_b.key, alloc);

        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc), Ok(Some(Some(res_a.canonical_digest))));
        assert_eq!(c_probe_reply_now(subj_b.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc), Ok(Some(Some(res_b.canonical_digest))));

        // Upgrading clang pin, answerer id, or macOS SDK invalidates recorded probe reads even
        // though the old record file still exists on disk.
        let new_pin = [8u8; 32];
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &new_pin, &sdk_a, Some(&store), alloc), Ok(Some(None)));
        let new_answerer = blaze_ids::ContentId::of("stub-cprobe-selective-test", b"2");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &new_answerer, &pin, &sdk_a, Some(&store), alloc), Ok(Some(None)));
        let new_sdk = blaze_build_script::facts::sdk_id_digest("macos-sdk-15.4");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &new_sdk, Some(&store), alloc), Ok(Some(None)));

        // Store read failures (I/O, permission, deadline) must propagate as Err instead of
        // silently downgrading to a cache miss in both `c_probe_reply_now` and `fact_now_with_probes`.
        struct FailingCProbeStore;
        impl super::CProbeStore for FailingCProbeStore {
            fn get<'a>(&self, key: &blaze_build_script::c_tasks_probe::ProbeKey, _alloc: blaze_own::Alloc<'a>) -> Result<Option<AVec<'a, u8>>, super::CProbeStoreError> {
                Err(super::CProbeStoreError::Twice(*key))
            }
            fn put(&self, key: &blaze_build_script::c_tasks_probe::ProbeKey, _bytes: &[u8]) -> Result<(), super::CProbeStoreError> {
                Err(super::CProbeStoreError::Twice(*key))
            }
        }
        let store_err = c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &sdk_a, Some(&FailingCProbeStore), alloc)
            .expect_err("store read failure must propagate from c_probe_reply_now");
        assert!(store_err.as_str().contains("already held"), "unexpected store error: {store_err}");
        let failing_probes = super::CScriptProbes {
            answerer: blaze_own::Arc::new_in(StubAnswerer, super::SharedAlloc::global()),
            records: blaze_own::Arc::new_in(FailingCProbeStore, super::SharedAlloc::global()),
            sdk_dir: blaze_own::Arc::from_str_in("", super::SharedAlloc::global()),
            sdk_id: blaze_own::Arc::from_str_in("macos-sdk-15.2", super::SharedAlloc::global()),
            sdk_digest: sdk_a,
            clang_sha256: pin,
        };
        let fact_store_err = crate::fact_pins::fact_now_with_probes(subj_a.as_str(), None, Some(&failing_probes), alloc)
            .expect_err("store read failure must propagate from fact_now_with_probes");
        assert!(fact_store_err.as_str().contains("already held"), "unexpected fact_now store error: {fact_store_err}");

        // Mutating ONLY probe A's cached record invalidates A and leaves B intact.
        let mut mutated = AVec::new_in(alloc);
        encode_answer(
            &ProbeAnswer { code: 1, stdout: OwnString::new_in(alloc), stderr: OwnString::from_str_in("error: unsupported", alloc) },
            &mut mutated,
        );
        store.overwrite(res_a.key, mutated.as_slice());

        assert_ne!(
            c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc),
            Ok(Some(Some(res_a.canonical_digest))),
            "mutated probe A must invalidate its asking script"
        );
        assert_eq!(
            c_probe_reply_now(subj_b.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc),
            Ok(Some(Some(res_b.canonical_digest))),
            "unrelated probe B must remain valid"
        );

        // Unsupported host returns Unavailable immediately with zero shelf I/O; on a supported host
        // with an absent shelf, setup uses the command's existing `bstd::fs::Fs` capability (zero
        // synchronous std::fs) and the first probe miss fails loudly naming the missing clang-23
        // shelf binary.
        let empty_home = std::env::temp_dir().join(format!("blaze-cprobe-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_home);
        let (tmp_io, tmp_dir) = crate::record::native_world(&std::env::temp_dir()).expect("tmp world");
        let _ = &tmp_dir;
        let tmp_fs = tmp_io.fs().expect("tmp fs");
        let tmp_cx = tmp_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("tmp cx");
        let unsupported = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "riscv64-unknown-linux-gnu", None, alloc);
        assert!(matches!(unsupported, CProbeSetting::Unavailable(_)), "unsupported host must return Unavailable");
        // Opening a native world over a non-directory file must fail loudly (ENOTDIR), while a
        // not-yet-created directory is allowed (matching `blaze-core` lazy folder creation on a
        // fresh `$HOME`) and refuses `lock_exclusive` until the directory exists.
        let not_a_dir = std::env::temp_dir().join(format!("blaze-cprobe-notdir-{}", std::process::id()));
        std::fs::write(&not_a_dir, b"file").expect("write non-dir file");
        assert!(crate::record::native_world(&not_a_dir).is_err(), "native_world over regular file must fail");
        let _ = std::fs::remove_file(&not_a_dir);
        let (lazy_io, lazy_dir) = crate::record::native_world(&empty_home).expect("native_world over not-yet-created root");
        let lazy_cx = lazy_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("lazy cx");
        let lazy_lock_path = bstd::path::Path::new("sub/.repair.lock").expect("lazy lock path");
        assert!(
            lazy_dir.lock_exclusive(&lazy_cx, &lazy_lock_path).is_err(),
            "lock_exclusive must fail with ENOENT while root directory does not exist"
        );
        std::fs::create_dir_all(&empty_home).expect("create empty home");
        let lazy_held = lazy_dir.lock_exclusive(&lazy_cx, &lazy_lock_path).expect("lock_exclusive after root created");
        lazy_held.unlock(&lazy_cx).expect("unlock lazy lock");
        let _ = std::fs::remove_dir_all(&empty_home);
        std::fs::create_dir_all(&empty_home).expect("recreate clean empty home");
        #[cfg(target_os = "linux")]
        {
            let x64_ready = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "x86_64-unknown-linux-gnu", None, alloc);
            assert!(matches!(x64_ready, CProbeSetting::Ready(_)), "x86_64-unknown-linux-gnu must be Ready");
            let missing = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "aarch64-unknown-linux-gnu", None, alloc);
            let CProbeSetting::Ready(missing_probes) = missing else {
                panic!("supported host setup must defer shelf check until first probe miss");
            };
            // Verify `open_command_c_probes` opens `DirStore::build_scripts` at `.blaze/v2` so
            // records land at `.blaze/v2/build-script/records/c-probe/<64-hex>` (matching `sweep.rs`).
            let sample_key = blaze_build_script::c_tasks_probe::ProbeKey(blaze_ids::ContentId([0xab; 32]));
            missing_probes.records.put(&sample_key, b"sample-record").expect("put sample c-probe");
            let expected_path = empty_home
                .join(".blaze/v2/build-script/records/c-probe")
                .join(super::hex_bytes(&sample_key.0.0, alloc).as_str());
            assert!(
                expected_path.is_file(),
                "c-probe store must write directly under .blaze/v2/build-script/records/c-probe/, missing {}",
                expected_path.display()
            );
            let err = resolve(&*missing_probes.answerer, &cache, &missing_probes.clang_sha256, &sp_a, dig, &mut bytes, alloc)
                .expect_err("missing shelf must fail loudly on first probe miss");
            assert!(err.as_str().contains("missing"), "unexpected missing-shelf error: {err}");
        }
        #[cfg(not(target_os = "linux"))]
        {
            let non_linux = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "aarch64-apple-darwin", None, alloc);
            assert!(matches!(non_linux, CProbeSetting::Unavailable(_)), "non-Linux hosts keep pinned-table route");
        }
        let _ = std::fs::remove_dir_all(&empty_home);

        // On Linux, `VerifiedExec::open_and_seal` copies into a kernel-sealed anonymous `memfd`
        // (`F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE`) so neither path replacement
        // nor in-place writes nor direct writes to the fd can ever alter the verified bytes.
        #[cfg(target_os = "linux")]
        {
            let pin_name = format!("blaze-cprobe-sealed-{}.bin", std::process::id());
            let pin_file = std::env::temp_dir().join(&pin_name);
            let pin_rel = bstd::path::Path::new(&pin_name).expect("pin rel");
            std::fs::write(&pin_file, b"original-verified-bytes").expect("write original");
            let mut hashed = blaze_sha256::Sha256::new();
            let vex = bstd::procs::hermetic::VerifiedExec::open_and_seal(
                &tmp_cx,
                &tmp_dir,
                &pin_rel,
                |chunk| { hashed.update(chunk); },
            )
            .expect("open_and_seal VerifiedExec");
            assert_eq!(hashed.finish(), blaze_sha256::digest(b"original-verified-bytes"));
            // In-place write AND atomic path replacement on disk cannot alter the sealed memfd.
            {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new().write(true).open(&pin_file).expect("open in-place");
                f.write_all(b"tampered-in-place!").expect("write in-place");
            }
            let replacement = std::env::temp_dir().join(format!("blaze-cprobe-sealed-{}.new", std::process::id()));
            std::fs::write(&replacement, b"tampered-replacement-bytes").expect("write replacement");
            std::fs::rename(&replacement, &pin_file).expect("atomic replace");
            let mut seen = std::vec::Vec::new();
            vex.stream_chunks(&tmp_cx, |chunk| seen.extend_from_slice(chunk)).expect("stream sealed memfd");
            assert_eq!(seen.as_slice(), b"original-verified-bytes");
            // Direct write against the sealed memfd is refused by the kernel (`F_SEAL_WRITE`).
            assert!(vex.try_write_for_test(b"evil").is_err(), "sealed memfd must reject writes");
            let _ = std::fs::remove_file(&pin_file);
        }

        // SOURCE_DATE_EPOCH parsing honors nonzero epoch, preserves invalid text for Clang, and
        // passes `HermeticSpec::new` end-to-end via `ClockSpec::InvalidEnv` (while rejecting
        // `InvalidEnv` when `SOURCE_DATE_EPOCH` is valid or absent).
        let mut env_nz = AVec::new_in(alloc);
        env_nz.push((OwnString::from_str_in("SOURCE_DATE_EPOCH", alloc), OwnString::from_str_in("1700000000", alloc)));
        assert_eq!(resolve_source_date_epoch(&mut env_nz, alloc), super::ClockSpec::Fixed { unix_secs: 1_700_000_000 });
        let mut env_default = AVec::new_in(alloc);
        assert_eq!(resolve_source_date_epoch(&mut env_default, alloc), super::ClockSpec::Fixed { unix_secs: 0 });
        assert_eq!(env_default.len(), 1);
        let mut env_bad = AVec::new_in(alloc);
        env_bad.push((OwnString::from_str_in("SOURCE_DATE_EPOCH", alloc), OwnString::from_str_in("not-a-num", alloc)));
        let bad_clock = resolve_source_date_epoch(&mut env_bad, alloc);
        assert_eq!(bad_clock, super::ClockSpec::InvalidEnv);
        assert_eq!(env_bad[0].1.as_str(), "not-a-num", "invalid SOURCE_DATE_EPOCH must be preserved for Clang");
        let spec_roots_arr = [super::Root { name: "out", kind: super::RootKind::Output }];
        let spec_mounts_arr = [super::Mount { spelling: b"/tmp/out", root: super::RootId(0) }];
        let spec_roots = super::Roots::new(&spec_roots_arr).expect("spec roots");
        let make_spec = |env: &[(&str, &str)], clock: super::ClockSpec| -> Result<(), super::Fault> {
            super::HermeticSpec::new(super::HermeticOpts {
                program: super::Program::Stored { name: "clang-23", digest: super::Digest([1; 32]), path: "/tmp/out/clang" },
                args: &["-c"],
                env,
                roots: spec_roots,
                mounts: &spec_mounts_arr,
                cwd: "/tmp/out",
                helpers: &[],
                helper_folders: &[],
                channels: &[],
                clock,
                seed: 0,
                budget: super::Budget { cpu: super::Duration::from_secs(5), memory: 1 << 28, steps: None },
                deadline: super::Deadline::after(super::Duration::ZERO, super::Duration::from_secs(5)),
                streams: super::HermeticStreams { stdout: super::Capture::Keep { most: 4096 }, stderr: super::Capture::Keep { most: 4096 } },
                level: super::Level::Inputs,
                report: super::ReportSpec::WholeRoots,
            })
            .map(|_| ())
            .map_err(|e| e.fault)
        };
        assert!(
            make_spec(&[("SOURCE_DATE_EPOCH", "not-a-num")], bad_clock).is_ok(),
            "HermeticSpec::new must accept ClockSpec::InvalidEnv when SOURCE_DATE_EPOCH is invalid"
        );
        // Leading-zero `SOURCE_DATE_EPOCH=01` must use the exact same parser as `HermeticSpec::new`
        // so it resolves to `ClockSpec::InvalidEnv` and reaches Clang instead of `ClockNotInEnv`.
        let mut env_lz = AVec::new_in(alloc);
        env_lz.push((OwnString::from_str_in("SOURCE_DATE_EPOCH", alloc), OwnString::from_str_in("01", alloc)));
        let lz_clock = resolve_source_date_epoch(&mut env_lz, alloc);
        assert_eq!(lz_clock, super::ClockSpec::InvalidEnv);
        assert!(
            make_spec(&[("SOURCE_DATE_EPOCH", "01")], lz_clock).is_ok(),
            "HermeticSpec::new must accept leading-zero SOURCE_DATE_EPOCH=01 via ClockSpec::InvalidEnv"
        );
        // Nonempty-stdin and path-qualified compiler invocations must bypass `Fact::CcProbe` and
        // stay on the ordinary compiler route (`Deferred` in check mode, `Executor` in build mode).
        let probe_args = ["-c", "-Wshadow", "-o", "/out/flag_check", "/out/flag_check.c"];
        assert!(matches!(
            crate::spawn_route::route_with_stdin("cc", &probe_args, false, blaze_build_script::CMode::Deferred, true, true),
            crate::spawn_route::Route::Fact(blaze_build_script::facts::Fact::CcProbe { .. })
        ));
        assert!(matches!(
            crate::spawn_route::route_with_stdin("cc", &probe_args, true, blaze_build_script::CMode::Deferred, true, true),
            crate::spawn_route::Route::Deferred
        ));
        assert!(matches!(
            crate::spawn_route::route_with_stdin("cc", &probe_args, true, blaze_build_script::CMode::Built, true, true),
            crate::spawn_route::Route::Executor
        ));
        assert!(matches!(
            crate::spawn_route::route_with_stdin("/opt/gcc/bin/gcc", &probe_args, false, blaze_build_script::CMode::Built, true, true),
            crate::spawn_route::Route::Executor
        ));
        assert!(matches!(
            crate::spawn_route::route_with_stdin("/missing/cc", &probe_args, false, blaze_build_script::CMode::Built, true, true),
            crate::spawn_route::Route::Executor
        ));
        assert_eq!(
            make_spec(&[("SOURCE_DATE_EPOCH", "123")], super::ClockSpec::InvalidEnv),
            Err(super::Fault::ClockNotInEnv),
            "ClockSpec::InvalidEnv must reject valid numeric SOURCE_DATE_EPOCH"
        );
        assert_eq!(
            make_spec(&[], super::ClockSpec::InvalidEnv),
            Err(super::Fault::ClockNotInEnv),
            "ClockSpec::InvalidEnv must reject missing SOURCE_DATE_EPOCH"
        );

        // Native `Dir::lock_exclusive` resolves strictly beneath the opened root descriptor and
        // refuses symbolic links in both intermediate directories and the leaf lock file.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let lock_root = std::env::temp_dir().join(format!("blaze-cprobe-locksym-{}", std::process::id()));
            let lock_outside = std::env::temp_dir().join(format!("blaze-cprobe-lockout-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&lock_root);
            let _ = std::fs::remove_dir_all(&lock_outside);
            std::fs::create_dir_all(&lock_root).expect("lock root");
            std::fs::create_dir_all(&lock_outside).expect("lock outside");
            std::os::unix::fs::symlink(&lock_outside, lock_root.join("escape-dir")).expect("symlink dir");
            std::os::unix::fs::symlink(lock_outside.join("escaped.lock"), lock_root.join("escape.lock")).expect("symlink leaf");
            let (lock_io, lock_dir) = crate::record::native_world(&lock_root).expect("lock world");
            let lock_cx = lock_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("lock cx");
            let sym_dir_path = bstd::path::Path::new("escape-dir/.repair.lock").expect("sym dir path");
            assert!(
                lock_dir.lock_exclusive(&lock_cx, &sym_dir_path).is_err(),
                "native lock_exclusive must refuse symlink intermediate directory"
            );
            let sym_leaf_path = bstd::path::Path::new("escape.lock").expect("sym leaf path");
            assert!(
                lock_dir.lock_exclusive(&lock_cx, &sym_leaf_path).is_err(),
                "native lock_exclusive must refuse symlink leaf lock file"
            );
            assert!(
                !lock_outside.join(".repair.lock").exists() && !lock_outside.join("escaped.lock").exists(),
                "refused symlink locks must never create files outside native root"
            );
            let ok_path = bstd::path::Path::new("sub/.repair.lock").expect("ok path");
            let rec_path = bstd::path::Path::new("sub/rec").expect("rec path");
            std::fs::create_dir_all(lock_root.join("sub")).expect("create sub");
            std::fs::write(lock_root.join("sub/rec"), b"corrupt-on-old-inode").expect("write old inode record");
            let held = lock_dir.lock_exclusive(&lock_cx, &ok_path).expect("legitimate lock beneath root");

            // Rename and replace the native world's root directory WHILE `held` is active:
            // protected read & publish through `held` must stay pinned to the exact opened root
            // directory inode holding the flock and NEVER touch the replacement tree.
            let moved_root = std::env::temp_dir().join(format!("blaze-cprobe-lockmoved-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&moved_root);
            std::fs::rename(&lock_root, &moved_root).expect("rename lock root while locked");
            std::fs::create_dir_all(lock_root.join("sub")).expect("recreate replacement lock root");
            std::fs::write(lock_root.join("sub/rec"), b"sentinel-on-new-inode").expect("write new inode sentinel");

            let seen = held
                .read_bounded(&lock_cx, &rec_path, 4096, false, alloc)
                .expect("read_bounded under held lock");
            let bstd::fs::BoundedRead::Whole(seen_bytes) = seen else {
                panic!("expected Whole read from locked old inode");
            };
            assert_eq!(
                seen_bytes.as_slice(),
                b"corrupt-on-old-inode",
                "locked read must stay on the locked inode rather than the replacement path"
            );
            held.publish_unsynced(&lock_cx, &rec_path, b"repaired-unsynced-on-old-inode")
                .expect("publish_unsynced under held lock");
            held.publish_synced(&lock_cx, &rec_path, b"repaired-on-old-inode")
                .expect("publish_synced under held lock");
            assert_eq!(
                std::fs::read(moved_root.join("sub/rec")).expect("read old inode"),
                b"repaired-on-old-inode",
                "locked publish must update the locked inode"
            );
            assert_eq!(
                std::fs::read(lock_root.join("sub/rec")).expect("read replacement inode"),
                b"sentinel-on-new-inode",
                "locked publish must never modify the replacement root directory"
            );
            held.unlock(&lock_cx).expect("unlock legitimate lock");

            // Any subsequent lock attempt on the stale world must also refuse after root replacement.
            let replaced_err = lock_dir
                .lock_exclusive(&lock_cx, &ok_path)
                .err()
                .expect("lock_exclusive must refuse after root directory replacement");
            assert!(
                std::format!("{replaced_err}").contains("replaced"),
                "unexpected replaced-root lock error: {replaced_err}"
            );
            let _ = std::fs::remove_dir_all(&moved_root);
            let _ = std::fs::remove_dir_all(&lock_root);
            let _ = std::fs::remove_dir_all(&lock_outside);
        }

        // Lazy verification runs through `bstd::sync::Flight` without holding any lock across
        // filesystem waits; concurrent waiters park on its Signal and honor their own deadline.
        #[cfg(target_os = "linux")]
        {
            let dummy = Arc::new(HermeticClangProbe {
                host: OwnString::from_str_in("aarch64-unknown-linux-gnu", super::SharedAlloc::global()),
                clang_path: OwnString::from_str_in("/nonexistent/clang-23", super::SharedAlloc::global()),
                shelf_dir: OwnString::from_str_in("/nonexistent", super::SharedAlloc::global()),
                shelf_digest: [0u8; 32],
                sdk_dir: None,
                sdk_digest: None,
                io: tmp_io.clone(),
                shelf_dir_cap: tmp_dir.clone(),
                work_base_dir: tmp_dir.clone(),
                work_rel: OwnString::new_in(super::SharedAlloc::global()),
                work_root: OwnString::from_str_in(&std::env::temp_dir().to_string_lossy(), super::SharedAlloc::global()),
                deadline: super::Duration::from_millis(25),
                verify_flight: bstd::sync::Flight::with_capacity_in(
                    "c-probe.clang.verify.test",
                    super::NonZeroUsize::MIN,
                    super::SharedAlloc::global(),
                ),
                root_reader: super::Mutex::new(None),
            });
            let clock = tmp_io.need_clock().expect("clock");
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = Arc::clone(&dummy);
            let holder_io = tmp_io.clone();
            let handle = std::thread::spawn(move || {
                let holder_cx = holder_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("holder cx");
                let _ = holder.verify_flight.ask(&holder_cx, (), |_vcx| {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv();
                    Err(blaze_own::Arc::from_str_in("simulated holder failure", super::SharedAlloc::global()))
                });
            });
            entered_rx.recv().expect("holder entered flight compute");
            let began = clock.now();
            let Err(err) = dummy.ensure_verified(&clock, began, alloc) else {
                panic!("must time out waiting on single-flight verification");
            };
            assert!(err.as_str().contains("timed out"), "unexpected error: {err}");
            let _ = release_tx.send(());
            handle.join().expect("holder thread finished");

            // Structural segment rendering preserves literal `$OUT_DIR`, expands real `Segment::OutDir`,
            // and explicitly refuses `Segment::SdkRoot` when no SDK is configured.
            let lit_segs = [super::Segment::Literal(OwnString::from_str_in("-DROOT=\"$OUT_DIR\"", alloc))];
            let rendered_lit = super::render_segments(&lit_segs, "/tmp/task-1", None, &[], alloc).expect("lit");
            assert_eq!(rendered_lit.as_str(), "-DROOT=\"$OUT_DIR\"");
            let real_segs = [
                super::Segment::Literal(OwnString::from_str_in("-DROOT=\"", alloc)),
                super::Segment::OutDir,
                super::Segment::Literal(OwnString::from_str_in("\"", alloc)),
            ];
            let rendered_real = super::render_segments(&real_segs, "/tmp/task-1", None, &[], alloc).expect("real");
            assert_eq!(rendered_real.as_str(), "-DROOT=\"/tmp/task-1\"");
            let unbound_sdk_segs = [super::Segment::SdkRoot];
            assert!(
                super::render_segments(&unbound_sdk_segs, "/tmp/task-1", None, &[], alloc).is_err(),
                "Segment::SdkRoot without sdk_dir must be refused"
            );
        }

        // Reply normalization scrubs `<shelf>/bin/clang-23` + `<shelf>/bin` (`InstalledDir:`) AND
        // escapes literal `$` -> `$$` so literal `$OUT_DIR`/`$IN0.c` round-trip verbatim through
        // `CanonicalProbe::unspell` while real host paths substitute cleanly.
        let placeholders = [(
            OwnString::from_str_in("$IN0.c", alloc),
            OwnString::from_str_in("/tmp/task-1/in0.c", alloc),
        )];
        let norm = super::normalize_reply(
            "clang: /home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23\nclang-23: user-produced-stdout\nInstalledDir: /home/user/.blaze/v2/toolchain/clang/abc/bin\nreal=/tmp/task-1/in0.c dir=/tmp/task-1 lit=$OUT_DIR lit2=$IN0.c\n",
            "cc",
            "/home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23",
            "/home/user/.blaze/v2/toolchain/clang/abc",
            "/tmp/task-1",
            None,
            placeholders.as_slice(),
            alloc,
        );
        // Cleanup failures on early probe errors must be combined and reported, never discarded,
        // and a combined (stream-overflow + cleanup-error) MUST NOT trigger overflow rerouting.
        let combined = super::finish_with_cleanup::<()>(
            Err(super::own_str("probe run failed", alloc)),
            Err(super::own_str("remove_dir EIO", alloc)),
            alloc,
        )
        .expect_err("combined error");
        assert!(combined.as_str().contains("probe run failed") && combined.as_str().contains("remove_dir EIO"));
        let overflow_plus_cleanup = super::finish_with_cleanup::<()>(
            Err(super::own_fmt(format_args!("{} Stdout exceeded 1048576 bytes", super::STREAM_OVERFLOW_PREFIX), alloc)),
            Err(super::own_str("remove_dir EIO", alloc)),
            alloc,
        )
        .expect_err("combined overflow + cleanup error");
        assert!(
            !super::is_stream_overflow_error(overflow_plus_cleanup.as_str()),
            "combined overflow + cleanup error must NOT be misclassified as pure stream overflow: {overflow_plus_cleanup}"
        );
        assert!(
            overflow_plus_cleanup.as_str().contains(super::STREAM_OVERFLOW_PREFIX)
                && overflow_plus_cleanup.as_str().contains("remove_dir EIO"),
            "combined error must preserve both overflow and cleanup causes: {overflow_plus_cleanup}"
        );
        // Oversized probe inputs (> 64 KiB) are caught even through non-canonical OUT_DIR spellings
        // like `/out/./...` or `/out/.//...` so no probe bypasses the bounded input cap.
        let huge = std::vec![b'a'; super::MAX_PROBE_INPUT_BYTES + 1];
        let small = b"int main(void) { return 0; }";
        let mut get_huge = |rel: &str| (rel == "1detect_compiler_family.c").then_some(huge.as_slice());
        let mut get_small = |rel: &str| (rel == "1detect_compiler_family.c").then_some(small.as_slice());
        assert!(super::has_oversized_probe_input(&["-E", "/out/1detect_compiler_family.c"], "/out", &mut get_huge, alloc));
        assert!(super::has_oversized_probe_input(&["-E", "/out/./1detect_compiler_family.c"], "/out", &mut get_huge, alloc));
        assert!(super::has_oversized_probe_input(&["-E", "/out/.//1detect_compiler_family.c"], "/out", &mut get_huge, alloc));
        assert!(!super::has_oversized_probe_input(&["-E", "/out/1detect_compiler_family.c"], "/out", &mut get_small, alloc));
        assert!(!super::has_oversized_probe_input(&["-E", "/out/./1detect_compiler_family.c"], "/out", &mut get_small, alloc));
        // An oversized existing `-o` / `--output` destination must NOT trip the input-size cap.
        let mut get_huge_out_small_in = |rel: &str| match rel {
            "flag_check" => Some(huge.as_slice()),
            "flag_check.c" => Some(small.as_slice()),
            _ => None,
        };
        assert!(!super::has_oversized_probe_input(
            &["-c", "-o", "/out/flag_check", "/out/flag_check.c"],
            "/out",
            &mut get_huge_out_small_in,
            alloc,
        ));
        assert!(!super::has_oversized_probe_input(
            &["-c", "-o/out/flag_check", "/out/flag_check.c"],
            "/out",
            &mut get_huge_out_small_in,
            alloc,
        ));
        assert!(!super::has_oversized_probe_input(
            &["-c", "--output=/out/flag_check", "/out/flag_check.c"],
            "/out",
            &mut get_huge_out_small_in,
            alloc,
        ));
        // Equivalent current_dir spellings resolve to the script mount instead of refusing probes.
        assert!(!crate::spawn_route::same_guest_dir("", "/package"));
        assert!(crate::spawn_route::same_guest_dir(".", "/package"));
        assert!(crate::spawn_route::same_guest_dir("./", "/package"));
        assert!(crate::spawn_route::same_guest_dir("/package/.", "/package"));
        assert!(crate::spawn_route::same_guest_dir("/package//sub/..", "/package"));
        assert!(crate::spawn_route::same_guest_dir("sub/..", "/package"));
        assert!(!crate::spawn_route::same_guest_dir("/package/sub", "/package"));
        assert!(!crate::spawn_route::same_guest_dir("..", "/package"));
        // Empty or relative RPATH/RUNPATH entries are rejected, while memfd `$ORIGIN` (`"/"`)
        // expands to an absolute path matching glibc's memfd runtime search path.
        #[cfg(target_os = "linux")]
        {
            let mut rbuf = [0u8; 320];
            assert!(
                bstd::procs::hermetic::validate_rpath_entry(b"", b"/", b"lib", b"aarch64", &mut rbuf).is_err(),
                "empty RPATH/RUNPATH component must be refused"
            );
            assert!(
                bstd::procs::hermetic::validate_rpath_entry(b"relative/lib", b"/", b"lib", b"aarch64", &mut rbuf).is_err(),
                "relative RPATH/RUNPATH component must be refused"
            );
            let n = bstd::procs::hermetic::validate_rpath_entry(b"$ORIGIN/../lib", b"/", b"lib", b"aarch64", &mut rbuf)
                .expect("memfd $ORIGIN entry");
            assert_eq!(&rbuf[..n], b"//../lib/");
        }
        assert!(super::is_stream_overflow_error("c-probe stream limit exceeded: Stdout exceeded 1048576 bytes"));
        assert!(!super::is_stream_overflow_error("c-probe hermetic run failed"));
        assert_eq!(
            norm.as_str(),
            "clang: cc\nclang-23: user-produced-stdout\nInstalledDir: $CLANG_SHELF/bin\nreal=$IN0.c dir=$OUT_DIR lit=$$OUT_DIR lit2=$$IN0.c\n"
        );
        let canon = c_tasks_probe::canonicalize(stub_id, &pin, &sp_a, dig, alloc).expect("canon");
        let restored = canon.unspell(norm.as_str(), &sp_a, alloc);
        assert_eq!(
            restored.as_str(),
            "clang: cc\nclang-23: user-produced-stdout\nInstalledDir: $CLANG_SHELF/bin\nreal=/out/flag_check.c dir=/out lit=$OUT_DIR lit2=$IN0.c\n"
        );

        // Stale task directory collisions recover cleanly across crash / PID reuse, and explicit
        // `TaskDirGuard::cleanup` removes entries and reports success on the result-bearing path.
        let col_root = std::env::temp_dir().join(format!("blaze-cprobe-col-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&col_root);
        std::fs::create_dir_all(&col_root).expect("col root");
        let (col_io, col_dir) = crate::record::native_world(&col_root).expect("col world");
        let col_clock = col_io.need_clock().expect("col clock");
        let col_cx = col_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("col cx");
        let (n1, rel1, d1) = super::make_unique_task_dir(&col_dir, &col_clock, &col_cx, alloc).expect("dir 1");
        let (n2, rel2, _) = super::make_unique_task_dir(&col_dir, &col_clock, &col_cx, alloc).expect("dir 2");
        assert_ne!(n1.as_str(), n2.as_str());
        let f_rel = bstd::path::Path::new("tmp.c").expect("tmp.c");
        d1.publish(&col_cx, &f_rel, bstd::fs::Content::bytes(b"int x;"), bstd::fs::Durable::No).expect("write tmp.c");
        let guard1 = super::TaskDirGuard { io: &col_io, work_root: &col_dir, task_rel: rel1, cleaned: false };
        guard1.cleanup(&col_cx, alloc).expect("explicit cleanup succeeds");
        let _ = col_dir.remove_dir(&col_cx, &rel2, bstd::fs::Durable::No);
        let _ = std::fs::remove_dir_all(&col_root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ld_so_cache_parser_resolves_custom_dirs_and_hwcaps() {
        #[cfg(target_arch = "aarch64")]
        let (target_flags, other_flags) = (0x0a03u32, 0x0303u32);
        #[cfg(target_arch = "x86_64")]
        let (target_flags, other_flags) = (0x0303u32, 0x0a03u32);
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let (target_flags, other_flags) = (0x0003u32, 0x0000u32);

        let build_cache = |compat_prefix: bool| -> std::vec::Vec<u8> {
            let mut buf = std::vec::Vec::new();
            if compat_prefix {
                buf.extend_from_slice(b"ld.so-1.7.0\0");
                buf.extend_from_slice(&1u32.to_le_bytes());
                buf.extend_from_slice(&[0u8; 12]);
                while buf.len() % 8 != 0 {
                    buf.push(0);
                }
            }
            let base_off = buf.len();
            // Ordered v2-first (matching real glibc `ldconfig`), followed by v3, v4, and baselines.
            let entries: &[(u32, u64, &[u8], &[u8])] = &[
                (target_flags, (1u64 << 62) | 0, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v2/libz.so.1"),
                (target_flags, (1u64 << 62) | 1, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v3/libz.so.1"),
                (target_flags, (1u64 << 62) | 2, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v4/libz.so.1"),
                (target_flags, 0, b"libz.so.1", b"/opt/custom/libz.so.1"),
                (target_flags, 0, b"libz.so.1", b"/usr/lib/fallback/libz.so.1"),
                (other_flags, 0, b"libz.so.1", b"/usr/lib/wrong-arch/libz.so.1"),
            ];
            let str_base = 48 + entries.len() * 24;
            let mut strings = std::vec::Vec::new();
            let mut offsets = std::vec::Vec::new();
            for &(flags, hwcap, key, val) in entries {
                let k_off = str_base + strings.len();
                strings.extend_from_slice(key);
                strings.push(0);
                let v_off = str_base + strings.len();
                strings.extend_from_slice(val);
                strings.push(0);
                offsets.push((flags, hwcap, k_off as u32, v_off as u32));
            }
            let mut subdir_offs = [0u32; 3];
            for (slot, name) in subdir_offs.iter_mut().zip([b"x86-64-v2\0".as_slice(), b"x86-64-v3\0", b"x86-64-v4\0"]) {
                *slot = (str_base + strings.len()) as u32;
                strings.extend_from_slice(name);
            }
            buf.extend_from_slice(b"glibc-ld.so.cache1.1");
            buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            buf.resize(base_off + 48, 0);
            for (flags, hwcap, k_off, v_off) in offsets {
                buf.extend_from_slice(&flags.to_le_bytes());
                buf.extend_from_slice(&k_off.to_le_bytes());
                buf.extend_from_slice(&v_off.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&hwcap.to_le_bytes());
            }
            buf.extend_from_slice(&strings);
            while buf.len() % 4 != 0 {
                buf.push(0);
            }
            // Per glibc `elf/dl-cache.h`, `extension_offset` and section `offset` are file-relative
            // (starting from byte 0 even in compat caches).
            let ext_off = buf.len() as u32;
            buf[base_off + 32..base_off + 36].copy_from_slice(&ext_off.to_le_bytes());
            let data_off = ext_off + 8 + 16;
            buf.extend_from_slice(&0xeaa4_2174u32.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&data_off.to_le_bytes());
            buf.extend_from_slice(&12u32.to_le_bytes());
            for off in subdir_offs {
                buf.extend_from_slice(&off.to_le_bytes());
            }
            buf
        };

        let new_only = build_cache(false);
        let compat = build_cache(true);
        for buf in [&new_only, &compat] {
            // Even when rows are ordered v2 -> v3 -> v4, ranking picks the highest supported ISA.
            for (level, want) in [
                (4u8, b"/opt/custom/glibc-hwcaps/x86-64-v4/libz.so.1".as_slice()),
                (3u8, b"/opt/custom/glibc-hwcaps/x86-64-v3/libz.so.1".as_slice()),
                (2u8, b"/opt/custom/glibc-hwcaps/x86-64-v2/libz.so.1".as_slice()),
                (1u8, b"/opt/custom/libz.so.1".as_slice()),
            ] {
                let caps = bstd::procs::hermetic::HostLdCaps { at_hwcap: 0, x86_64_level: level };
                let mut found: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
                let ok = bstd::procs::hermetic::parse_ld_so_cache_match(buf, b"libz.so.1", &caps, |path| {
                    found.push(path.to_vec());
                    Ok(true)
                })
                .expect("ranked cache parse");
                assert!(ok);
                assert_eq!(found, std::vec![want.to_vec()]);
            }

            // Missing highest-priority entry falls through in ranked order.
            let caps_base = bstd::procs::hermetic::HostLdCaps { at_hwcap: 0, x86_64_level: 1 };
            let mut tried: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
            let ok_fall = bstd::procs::hermetic::parse_ld_so_cache_match(buf, b"libz.so.1", &caps_base, |path| {
                tried.push(path.to_vec());
                Ok(path == b"/usr/lib/fallback/libz.so.1")
            })
            .expect("fallthrough parse");
            assert!(ok_fall);
            assert_eq!(
                tried,
                std::vec![b"/opt/custom/libz.so.1".to_vec(), b"/usr/lib/fallback/libz.so.1".to_vec()]
            );
        }
        let err = bstd::procs::hermetic::parse_ld_so_cache_match(
            b"not-an-ld-cache",
            b"libz.so.1",
            &bstd::procs::hermetic::HostLdCaps { at_hwcap: 0, x86_64_level: 1 },
            |_| Ok(true),
        )
        .expect_err("corrupt ld.so.cache must fail loudly");
        assert!(matches!(err, bstd::procs::hermetic::Fault::Host { code: 22, .. }));

        // Streaming `/etc/ld.so.preload` scanner must detect entries past 4096 bytes of comments.
        let mut preload_bytes = std::vec![b'#'; 5000];
        preload_bytes.push(b'\n');
        preload_bytes.extend_from_slice(b"  /lib/evil.so\n");
        let mut scanner = bstd::procs::hermetic::PreloadScanner::new();
        let mut saw_entry = false;
        for chunk in preload_bytes.chunks(4096) {
            if scanner.feed(chunk) {
                saw_entry = true;
                break;
            }
        }
        assert!(saw_entry, "PreloadScanner must detect preload entry past byte 4096");
        let mut clean_scanner = bstd::procs::hermetic::PreloadScanner::new();
        assert!(!clean_scanner.feed(b"# comment\n   \n# another comment\n"));

        // FHS fallback glibc-hwcaps search order must descend v4 -> v3 -> v2.
        assert_eq!(
            bstd::procs::hermetic::x86_64_fhs_hwcap_subdirs(4),
            &[
                b"glibc-hwcaps/x86-64-v4/".as_slice(),
                b"glibc-hwcaps/x86-64-v3/".as_slice(),
                b"glibc-hwcaps/x86-64-v2/".as_slice(),
            ]
        );
        assert_eq!(
            bstd::procs::hermetic::x86_64_fhs_hwcap_subdirs(3),
            &[b"glibc-hwcaps/x86-64-v3/".as_slice(), b"glibc-hwcaps/x86-64-v2/".as_slice()]
        );
        assert_eq!(bstd::procs::hermetic::x86_64_fhs_hwcap_subdirs(1), &[] as &[&[u8]]);

        // glibc standard ELF RUNPATH tokens (`$ORIGIN`, `$LIB`, `$PLATFORM`, and braced variants)
        // must expand accurately while unknown `$...` tokens fail loudly with ENOSYS.
        assert_eq!(bstd::procs::hermetic::dl_dst_lib_token(b"/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"), b"lib/x86_64-linux-gnu");
        assert_eq!(bstd::procs::hermetic::dl_dst_lib_token(b"/lib64/ld-linux-x86-64.so.2"), b"lib64");
        assert_eq!(bstd::procs::hermetic::dl_dst_lib_token(b"/nix/store/abc-glibc/lib/ld-linux-aarch64.so.1"), b"lib");
        let mut exp_buf = [0u8; 320];
        let n = bstd::procs::hermetic::expand_rpath_tokens(
            b"$ORIGIN/../$LIB/${PLATFORM}",
            b"/opt/tool/bin",
            b"lib64",
            b"x86_64",
            &mut exp_buf,
        )
        .expect("standard glibc tokens must expand");
        assert_eq!(&exp_buf[..n], b"/opt/tool/bin/../lib64/x86_64/");
        let n2 = bstd::procs::hermetic::expand_rpath_tokens(
            b"${ORIGIN}/${LIB}/$PLATFORM",
            b"/opt/tool/bin",
            b"lib/aarch64-linux-gnu",
            b"aarch64",
            &mut exp_buf,
        )
        .expect("braced glibc tokens must expand");
        assert_eq!(&exp_buf[..n2], b"/opt/tool/bin/lib/aarch64-linux-gnu/aarch64/");
        assert!(
            bstd::procs::hermetic::expand_rpath_tokens(b"$ORIGIN/$UNKNOWN", b"/opt/bin", b"lib", b"aarch64", &mut exp_buf).is_err(),
            "unknown dynamic-loader token must fail loudly"
        );
        assert!(
            bstd::procs::hermetic::expand_rpath_tokens(b"$LIB_CUSTOM", b"/opt/bin", b"lib", b"aarch64", &mut exp_buf).is_err(),
            "non-delimited $LIB_CUSTOM must not misexpand as $LIB"
        );
    }

    #[cfg(target_os = "linux")]
    fn run_reaper_core_dst(plant: bstd::procs::hermetic::ReaperPlant) -> Result<(), &'static str> {
        use bstd::procs::hermetic::{ReaperCore, RetireAction};
        const MAX_WORKERS: usize = 64;
        const MAX_PENDING: usize = 64;
        const ITEMS: usize = 110;
        for seed in 0..300u64 {
            let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
            let mut next_u64 = || -> u64 {
                rng = rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = rng;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                z ^ (z >> 31)
            };
            let mut core: ReaperCore<(usize, bool)> = ReaperCore::new(MAX_WORKERS, MAX_PENDING, plant);
            // Active simulated workers: `(item_id, permanently_wedged)`.
            let mut workers = blaze_own::Vec::new_in(blaze_own::SharedAlloc::global());
            let mut reaped = 0usize;
            let mut stuck_total = 0usize;
            let mut retired = 0usize;
            // Phase 1: Initial burst past the 64/65 cap boundary BEFORE workers complete so
            // overflow queueing (`MAX_WORKERS + 12` concurrent retirements) and transient spawn
            // failures are guaranteed to execute on every seed.
            let spawn_item = |core: &mut ReaperCore<(usize, bool)>,
                                  workers: &mut blaze_own::Vec<(usize, bool), blaze_own::SharedAlloc<'static>>,
                                  item: (usize, bool),
                                  fail_spawn: bool|
             -> Result<(), &'static str> {
                match core.retire(item) {
                    RetireAction::Spawn(first) => {
                        if fail_spawn {
                            core.on_spawn_failed(first);
                        } else {
                            workers.push(first);
                            while let Some(extra) = core.take_spawnable() {
                                workers.push(extra);
                            }
                        }
                    }
                    RetireAction::Queued => {}
                    RetireAction::SyncDrop(_) => {
                        return Err("reaper oracle: caller synchronously dropped overflowed driver");
                    }
                }
                if core.active() > MAX_WORKERS {
                    return Err("reaper oracle: active reaper workers exceeded cap");
                }
                Ok(())
            };
            for step in 0..400u32 {
                // Burst the first 76 items without completing workers to force 64 -> 65 overflow.
                let in_burst = retired < MAX_WORKERS + 12;
                let retire_now = in_burst || (retired < ITEMS && next_u64() % 3 != 0);
                if retire_now {
                    let wedged = retired == 0 || (stuck_total < 8 && next_u64() % 17 == 0);
                    if wedged {
                        stuck_total += 1;
                    }
                    let id = retired;
                    retired += 1;
                    // Inject deterministic transient thread-spawn failures (~12%) once past the
                    // first 2 workers, never on the final recovery burst.
                    let fail_spawn = retired > 2 && retired + 4 < ITEMS && next_u64() % 8 == 0;
                    spawn_item(&mut core, &mut workers, (id, wedged), fail_spawn)?;
                }
                if in_burst {
                    continue;
                }
                // Let a random non-wedged active worker complete its drop and loop for pending work.
                let mut ready_idx = None;
                let wlen = workers.len();
                if wlen > 0 {
                    let start = usize::try_from(next_u64()).unwrap_or(0) % wlen;
                    for off in 0..wlen {
                        let idx = (start + off) % wlen;
                        if let Some(&(_, wedged)) = workers.get(idx)
                            && !wedged
                        {
                            ready_idx = Some(idx);
                            break;
                        }
                    }
                }
                if let Some(idx) = ready_idx {
                    reaped += 1;
                    match core.next_after_drop() {
                        Some(next_item) => {
                            if let Some(slot) = workers.get_mut(idx) {
                                *slot = next_item;
                            }
                        }
                        None => {
                            let _ = workers.remove_swap(idx);
                        }
                    }
                }
                let _ = step;
            }
            // Recover any items queued during earlier transient spawn failures once OS thread
            // spawn succeeds again (`take_spawnable`).
            while let Some(item) = core.take_spawnable() {
                workers.push(item);
            }
            // Drain all remaining non-wedged workers and verify every non-wedged item was reaped
            // even while permanently wedged mounts remain blocked in their own workers.
            loop {
                let mut found = None;
                for i in 0..workers.len() {
                    if let Some(&(_, wedged)) = workers.get(i)
                        && !wedged
                    {
                        found = Some(i);
                        break;
                    }
                }
                let Some(idx) = found else {
                    break;
                };
                reaped += 1;
                match core.next_after_drop() {
                    Some(next_item) => {
                        if let Some(slot) = workers.get_mut(idx) {
                            *slot = next_item;
                        }
                    }
                    None => {
                        let _ = workers.remove_swap(idx);
                    }
                }
            }
            if reaped != retired - stuck_total {
                return Err("reaper oracle: non-wedged retired drivers were blocked from cleanup");
            }
            if core.pending_len() != 0 {
                return Err("reaper oracle: pending queue failed to drain despite free workers");
            }
            if core.active() != stuck_total {
                return Err("reaper oracle: active worker count diverged from wedged mount count");
            }
            if !core.has_capacity() {
                return Err("reaper oracle: drained core must report capacity available");
            }
        }
        // Admission-control reservation check: admitting `MAX_WORKERS + MAX_PENDING` live owners
        // while `pending == 0` must reserve every slot so the next admission is refused BEFORE
        // any of them wedge, and retiring all reserved owners keeps `active + pending + reserved`
        // strictly bounded at `MAX_WORKERS + MAX_PENDING`.
        let mut cap_core: ReaperCore<usize> = ReaperCore::new(MAX_WORKERS, MAX_PENDING, plant);
        for _ in 0..(MAX_WORKERS + MAX_PENDING) {
            if !cap_core.try_reserve() {
                return Err("reaper oracle: try_reserve failed before MAX_WORKERS + MAX_PENDING");
            }
        }
        if cap_core.try_reserve() || cap_core.has_capacity() {
            return Err("reaper oracle: try_reserve succeeded past MAX_WORKERS + MAX_PENDING while pending was empty");
        }
        // Releasing one clean reservation immediately re-opens capacity, then re-reserving closes it.
        cap_core.release_reservation();
        if !cap_core.try_reserve() {
            return Err("reaper oracle: release_reservation failed to restore capacity");
        }
        for i in 0..(MAX_WORKERS + MAX_PENDING) {
            let _ = cap_core.retire_reserved(i);
            if cap_core.active() + cap_core.pending_len() + cap_core.reserved() > MAX_WORKERS + MAX_PENDING {
                return Err("reaper oracle: total live + wedged count exceeded MAX_WORKERS + MAX_PENDING");
            }
        }
        if cap_core.reserved() != 0 || cap_core.has_capacity() {
            return Err("reaper oracle: retired reserved core had wrong count or stale capacity");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reaper_core_dst_survives_overflow_spawn_failures_and_wedged_mounts() {
        run_reaper_core_dst(bstd::procs::hermetic::ReaperPlant::None)
            .expect("ReaperCore DST must reap all non-wedged drivers without blocking caller");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reaper_core_dst_catches_planted_sync_drop_on_overflow() {
        let err = run_reaper_core_dst(bstd::procs::hermetic::ReaperPlant::SyncDropOnOverflow)
            .expect_err("planted SyncDropOnOverflow must be caught");
        assert!(err.contains("synchronously dropped"), "unexpected error: {err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reaper_core_dst_catches_planted_single_fifo() {
        let err = run_reaper_core_dst(bstd::procs::hermetic::ReaperPlant::SingleFifo)
            .expect_err("planted SingleFifo must be caught");
        assert!(err.contains("blocked from cleanup"), "unexpected error: {err}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_standby_reaper_dst(plant: bstd::procs::hermetic::StandbyReaperPlant) -> Result<u64, &'static str> {
        use bstd::procs::hermetic::{StandbyReaperCore, StandbyWorkerStep};
        use std::collections::VecDeque;

        const MAX_WORKERS: usize = 4;
        const MAX_PENDING: usize = 4;
        const CAP: usize = MAX_WORKERS + MAX_PENDING;
        let mut total_reaped = 0u64;

        for seed in 0u64..100 {
            let mut rng = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut core = StandbyReaperCore::<u64>::new(MAX_WORKERS, MAX_PENDING, plant);
            let mut spawning_queue: VecDeque<bool> = VecDeque::new();
            let mut live_reserved = 0usize;
            let mut live_idle_workers = 0usize;
            let mut active_drops: VecDeque<u64> = VecDeque::new();
            let mut next_id = 1u64;
            let mut retired_count = 0u64;
            let mut reaped_count = 0u64;

            let check_oracle = |c: &StandbyReaperCore<u64>, live_res: usize, live_idle: usize, active_d: &VecDeque<u64>, sp: &VecDeque<bool>| -> Result<(), &'static str> {
                if c.active() + c.pending_len() + c.reserved() > CAP {
                    return Err("standby reaper exceeded total backlog cap");
                }
                if c.active() > MAX_WORKERS {
                    return Err("standby reaper exceeded max active workers");
                }
                if c.reserved() != live_res {
                    return Err("reserved counter diverged from oracle");
                }
                if c.idle() != live_idle {
                    return Err("idle counter diverged from oracle");
                }
                if c.active() != active_d.len() {
                    return Err("active counter diverged from oracle");
                }
                if c.spawning() != sp.len() {
                    return Err("spawning counter diverged from oracle");
                }
                // Zero-stranded invariant: whenever any item is pending and active < MAX_WORKERS,
                // there MUST be at least one live idle worker or confirmed in-flight spawn to wake.
                if c.pending_len() > 0 && c.active() < MAX_WORKERS && c.idle() + c.spawning() == 0 {
                    return Err("pending item stranded with zero active/idle/spawning workers");
                }
                Ok(())
            };

            for _ in 0..80 {
                let op = next() % 6;
                match op {
                    0 | 1 => {
                        // Reserve (with injected transient spawn failures).
                        if let Ok(need_spawn) = core.begin_reserve() {
                            if need_spawn {
                                if next() % 4 == 0 {
                                    core.abort_spawn_failed();
                                } else {
                                    live_reserved += 1;
                                    spawning_queue.push_back(true);
                                }
                            } else {
                                live_reserved += 1;
                            }
                        }
                    }
                    2 => {
                        // Clean close (release reservation).
                        if live_reserved > 0 {
                            live_reserved -= 1;
                            let _ = core.release_reservation();
                        }
                    }
                    3 => {
                        // Stalled retire (zero thread::spawn possible!).
                        if live_reserved > 0 {
                            live_reserved -= 1;
                            let id = next_id;
                            next_id += 1;
                            retired_count += 1;
                            core.retire_reserved(id);
                        }
                    }
                    4 => {
                        // Start one spawned worker OR poll one idle worker.
                        if let Some(_) = spawning_queue.pop_front() {
                            core.on_worker_started();
                            live_idle_workers += 1;
                        } else if live_idle_workers > 0 {
                            match core.worker_poll() {
                                StandbyWorkerStep::Work(id) => {
                                    live_idle_workers -= 1;
                                    active_drops.push_back(id);
                                }
                                StandbyWorkerStep::Exit => {
                                    live_idle_workers -= 1;
                                }
                                StandbyWorkerStep::Wait => {}
                            }
                        }
                    }
                    _ => {
                        // Complete one active drop.
                        if let Some(_id) = active_drops.pop_front() {
                            reaped_count += 1;
                            core.worker_finished_drop();
                            live_idle_workers += 1;
                        }
                    }
                }
                check_oracle(&core, live_reserved, live_idle_workers, &active_drops, &spawning_queue)?;
            }

            // Quiesce: finish all in-flight spawns, retire or release all remaining reservations,
            // and step all workers until nothing remains.
            while spawning_queue.pop_front().is_some() {
                core.on_worker_started();
                live_idle_workers += 1;
            }
            while live_reserved > 0 {
                live_reserved -= 1;
                if next() % 2 == 0 {
                    let id = next_id;
                    next_id += 1;
                    retired_count += 1;
                    core.retire_reserved(id);
                } else {
                    let _ = core.release_reservation();
                }
                check_oracle(&core, live_reserved, live_idle_workers, &active_drops, &spawning_queue)?;
            }
            for _ in 0..256 {
                if let Some(_id) = active_drops.pop_front() {
                    reaped_count += 1;
                    core.worker_finished_drop();
                    live_idle_workers += 1;
                }
                if live_idle_workers > 0 {
                    match core.worker_poll() {
                        StandbyWorkerStep::Work(id) => {
                            live_idle_workers -= 1;
                            active_drops.push_back(id);
                        }
                        StandbyWorkerStep::Exit => {
                            live_idle_workers -= 1;
                        }
                        StandbyWorkerStep::Wait => {}
                    }
                }
                check_oracle(&core, live_reserved, live_idle_workers, &active_drops, &spawning_queue)?;
                if core.pending_len() == 0 && core.active() == 0 && core.idle() == 0 {
                    break;
                }
            }
            if reaped_count != retired_count || core.pending_len() != 0 {
                return Err("retired items stranded after quiescence");
            }
            if core.active() != 0 || core.idle() != 0 || core.reserved() != 0 || core.spawning() != 0 {
                return Err("leaked standby worker or counter after quiescence");
            }
            total_reaped += reaped_count;
        }
        Ok(total_reaped)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn standby_reaper_pool_dst_survives_interleavings_and_spawn_failures() {
        let reaped = run_standby_reaper_dst(bstd::procs::hermetic::StandbyReaperPlant::None)
            .expect("StandbyReaperCore DST must satisfy backlog, non-stranding, and quiescence invariants");
        assert!(reaped > 0, "expected retired items across 100 seeds");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn standby_reaper_pool_dst_catches_planted_stranded_item() {
        let err = run_standby_reaper_dst(bstd::procs::hermetic::StandbyReaperPlant::ExitWhileNeeded)
            .expect_err("planted ExitWhileNeeded must be caught by DST oracle");
        assert!(err.contains("stranded"), "unexpected error: {err}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_lock_runtime_dst(plant: bstd::procs::hermetic::LockPlant) -> Result<u64, &'static str> {
        use bstd::procs::hermetic::{AttemptSignal, LockBackend, LockRuntime};
        use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::collections::VecDeque;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        struct TrackedHandle {
            in_worker: StdArc<AtomicBool>,
            unlocked: StdArc<AtomicBool>,
            closed_count: StdArc<AtomicUsize>,
            unlocked_count: StdArc<AtomicUsize>,
            dropped_on_caller: StdArc<AtomicUsize>,
        }

        impl Drop for TrackedHandle {
            fn drop(&mut self) {
                if !self.in_worker.load(Ordering::Acquire) {
                    self.dropped_on_caller.fetch_add(1, Ordering::AcqRel);
                }
                if self.unlocked.load(Ordering::Acquire) {
                    self.unlocked_count.fetch_add(1, Ordering::AcqRel);
                }
                self.closed_count.fetch_add(1, Ordering::AcqRel);
            }
        }

        struct SimState {
            rng: u64,
            fail_spawn_mask: u64,
            ready_queue: VecDeque<std::boxed::Box<dyn FnOnce() + Send + 'static>>,
            parked_reapers: usize,
            trace_hash: u64,
        }

        impl SimState {
            fn next_u64(&mut self) -> u64 {
                self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
                let mut z = self.rng;
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                let out = z ^ (z >> 31);
                self.trace_hash = self.trace_hash.rotate_left(7) ^ out;
                out
            }
        }

        /// 100% single-threaded deterministic discrete-event scheduler driving the EXACT
        /// production `LockRuntime<TrackedHandle>` seam with zero `std::thread::spawn` and zero
        /// `thread::sleep`. Any `u64` seed replays the exact same interleaving bit-for-bit.
        struct SimLockBackend {
            state: StdMutex<SimState>,
            in_worker: StdArc<AtomicBool>,
            acquired_count: StdArc<AtomicUsize>,
            unlocked_count: StdArc<AtomicUsize>,
            closed_count: StdArc<AtomicUsize>,
            dropped_on_caller: StdArc<AtomicUsize>,
            rt_slot: std::sync::OnceLock<LockRuntime<TrackedHandle>>,
            self_slot: std::sync::OnceLock<blaze_own::Arc<SimLockBackend, blaze_own::SharedAlloc<'static>>>,
        }

        impl SimLockBackend {
            fn step_one(&self, pick_from_back: bool) -> bool {
                let task = {
                    let mut st = self.state.lock().expect("sim state lock");
                    if pick_from_back {
                        st.ready_queue.pop_back()
                    } else {
                        st.ready_queue.pop_front()
                    }
                };
                if let Some(f) = task {
                    self.in_worker.store(true, Ordering::Release);
                    f();
                    self.in_worker.store(false, Ordering::Release);
                    true
                } else {
                    false
                }
            }

            fn drain_all(&self) {
                while self.step_one(false) {}
            }
        }

        impl LockBackend<TrackedHandle> for SimLockBackend {
            fn open_and_try_lock(&self) -> Result<Option<TrackedHandle>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
                let r = self.state.lock().expect("sim state lock").next_u64();
                if r % 5 == 0 {
                    // Contended attempt.
                    return Ok(None);
                }
                self.acquired_count.fetch_add(1, Ordering::AcqRel);
                Ok(Some(TrackedHandle {
                    in_worker: StdArc::clone(&self.in_worker),
                    unlocked: StdArc::new(AtomicBool::new(false)),
                    closed_count: StdArc::clone(&self.closed_count),
                    unlocked_count: StdArc::clone(&self.unlocked_count),
                    dropped_on_caller: StdArc::clone(&self.dropped_on_caller),
                }))
            }

            fn unlock_and_close(&self, handle: TrackedHandle) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
                handle.unlocked.store(true, Ordering::Release);
                drop(handle);
                Ok(())
            }

            fn spawn_worker<F: FnOnce() + Send + 'static>(&self, _name: &str, f: F) -> Result<(), ()> {
                let mut st = self.state.lock().expect("sim state lock");
                if st.fail_spawn_mask > 0 && st.next_u64() % st.fail_spawn_mask == 0 {
                    return Err(());
                }
                st.ready_queue.push_back(std::boxed::Box::new(f));
                Ok(())
            }

            fn wait_signal(&self, _signal: &AttemptSignal) {
                // Deterministically decide whether this poll tick sees:
                // - 0 tasks run (stalled worker -> poll timeout -> caller may abandon `Running`),
                // - oldest queued worker run (FIFO completion), or
                // - newest queued worker run (LIFO reordering / worker finishes right before caller
                //   checks `is_stopped()`, hitting the atomic `Done(res)` retirement branch!).
                let roll = self.state.lock().expect("sim state lock").next_u64() % 4;
                match roll {
                    0 => {} // Simulate stalled worker on this tick.
                    1 => {
                        let _ = self.step_one(false);
                    }
                    2 => {
                        let _ = self.step_one(true);
                    }
                    _ => {
                        // Run two tasks to interleave unlock/reaper completions with acquire.
                        let _ = self.step_one(false);
                        let _ = self.step_one(true);
                    }
                }
            }

            fn park_reaper<'a>(
                &self,
                _cvar: &std::sync::Condvar,
                _guard: std::sync::MutexGuard<'a, bstd::procs::hermetic::StandbyReaperCore<TrackedHandle>>,
            ) -> Option<std::sync::MutexGuard<'a, bstd::procs::hermetic::StandbyReaperCore<TrackedHandle>>> {
                self.state.lock().expect("sim state lock").parked_reapers += 1;
                None
            }

            fn notify_reaper(&self, _cvar: &std::sync::Condvar, all: bool) {
                let mut st = self.state.lock().expect("sim state lock");
                let wake_count = if all { st.parked_reapers } else { st.parked_reapers.min(1) };
                st.parked_reapers -= wake_count;
                let Some(rt) = self.rt_slot.get() else {
                    return;
                };
                let Some(me) = self.self_slot.get() else {
                    return;
                };
                for _ in 0..wake_count {
                    let rt_w = blaze_own::Share::share(rt);
                    let me_w = blaze_own::Share::share(me);
                    // Waking an already-spawned parked standby reaper does NOT call `spawn_worker`.
                    st.ready_queue.push_back(std::boxed::Box::new(move || {
                        rt_w.run_standby_reaper_loop(&*me_w);
                    }));
                }
            }
        }

        let mut combined_trace = 0u64;
        for seed in 0..300u64 {
            let in_worker = StdArc::new(AtomicBool::new(false));
            let acquired_count = StdArc::new(AtomicUsize::new(0));
            let unlocked_count = StdArc::new(AtomicUsize::new(0));
            let closed_count = StdArc::new(AtomicUsize::new(0));
            let dropped_on_caller = StdArc::new(AtomicUsize::new(0));
            // Small cap (`max_workers = 2, max_pending = 4`) deterministically saturates workers,
            // queues reapers, and exercises spawn failures on every seed.
            let rt = LockRuntime::new(2, 4, plant);
            let backend = blaze_own::Arc::new_in(
                SimLockBackend {
                    state: StdMutex::new(SimState {
                        rng: seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(17),
                        fail_spawn_mask: 7,
                        ready_queue: VecDeque::new(),
                        parked_reapers: 0,
                        trace_hash: seed,
                    }),
                    in_worker: StdArc::clone(&in_worker),
                    acquired_count: StdArc::clone(&acquired_count),
                    unlocked_count: StdArc::clone(&unlocked_count),
                    closed_count: StdArc::clone(&closed_count),
                    dropped_on_caller: StdArc::clone(&dropped_on_caller),
                    rt_slot: std::sync::OnceLock::new(),
                    self_slot: std::sync::OnceLock::new(),
                },
                blaze_own::SharedAlloc::global(),
            );
            let _ = backend.rt_slot.set(blaze_own::Share::share(&rt));
            let _ = backend.self_slot.set(blaze_own::Share::share(&backend));
            let mut held = std::vec::Vec::new();
            for op in 0..24u64 {
                let choice = (seed.wrapping_add(op)) % 4;
                if choice < 2 || held.is_empty() {
                    // Half the acquires cancel after 1..=2 ticks, deterministically hitting BOTH
                    // `Running -> Abandoned` (worker runs later) AND `Done(Ok(Some(h)))` on a
                    // stopped tick (caller retires `h` immediately without blocking).
                    let cancel_after = if (seed + op) % 2 == 0 {
                        usize::try_from(1 + ((seed + op) % 2)).unwrap_or(1)
                    } else {
                        6
                    };
                    let mut polls = 0usize;
                    let mut is_stopped = || {
                        polls += 1;
                        if polls > cancel_after {
                            Some(bstd::Stopped::Cancelled { span: "lock_runtime_dst" })
                        } else {
                            None
                        }
                    };
                    let mut sleep_backoff = || Ok(());
                    if let Ok(handle) = rt.lock_exclusive(&backend, &mut is_stopped, &mut sleep_backoff) {
                        held.push(handle);
                    }
                } else if let Some(handle) = held.pop() {
                    if choice == 2 {
                        let mut polls = 0usize;
                        let mut is_stopped = || {
                            polls += 1;
                            if polls > 2 {
                                Some(bstd::Stopped::Cancelled { span: "lock_runtime_dst.unlock" })
                            } else {
                                None
                            }
                        };
                        let _ = rt.unlock(handle, &backend, &mut is_stopped);
                    } else {
                        rt.retire_handle(handle, &backend);
                    }
                }
            }
            for (idx, handle) in held.drain(..).enumerate() {
                if idx % 2 == 0 {
                    let mut polls = 0usize;
                    let mut is_stopped = || {
                        polls += 1;
                        if polls > 1 {
                            Some(bstd::Stopped::Cancelled { span: "lock_runtime_dst.drain" })
                        } else {
                            None
                        }
                    };
                    let _ = rt.unlock(handle, &backend, &mut is_stopped);
                } else {
                    rt.retire_handle(handle, &backend);
                }
            }
            // Force 100% spawn failure during quiescence (NO `pump_reapers`, NO new spawns):
            // pre-spawned standby reapers MUST still unlock, close, and exit cleanly!
            backend.state.lock().expect("sim state lock").fail_spawn_mask = 1;
            backend.drain_all();

            if dropped_on_caller.load(Ordering::Acquire) != 0 {
                return Err("lock runtime oracle: handle dropped synchronously on caller thread");
            }
            let acq = acquired_count.load(Ordering::Acquire);
            let unl = unlocked_count.load(Ordering::Acquire);
            let cls = closed_count.load(Ordering::Acquire);
            if cls != acq {
                return Err("lock runtime oracle: acquired handle was leaked without closing");
            }
            if unl != acq {
                return Err("lock runtime oracle: acquired handle was dropped without unlocking");
            }
            if rt.active_workers() != 0 {
                return Err("lock runtime oracle: active_workers counter leaked");
            }
            if rt.reaper_active() != 0
                || rt.reaper_idle() != 0
                || rt.reaper_spawning() != 0
                || rt.reaper_pending() != 0
                || rt.reaper_reserved() != 0
            {
                return Err("lock runtime oracle: reaper slot/pending/active/idle state leaked");
            }
            combined_trace ^= backend.state.lock().expect("sim state lock").trace_hash;
        }
        Ok(combined_trace)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_locked_io_handshake_dst(plant: bstd::procs::hermetic::LockedIoPlant) -> Result<(), &'static str> {
        use bstd::procs::hermetic::{AttemptSignal, LockedIoHandshake};
        use core::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex as StdMutex;

        static TEST_ACTIVE: AtomicUsize = AtomicUsize::new(0);
        for iter in 0..100usize {
            let in_flight = blaze_own::Arc::new_in(AtomicUsize::new(1), blaze_own::SharedAlloc::global());
            TEST_ACTIVE.store(1, Ordering::Release);
            let slot = blaze_own::Arc::new_in(StdMutex::new(Option::<usize>::None), blaze_own::SharedAlloc::global());
            let signal = blaze_own::Arc::new_in(AttemptSignal::new(), blaze_own::SharedAlloc::global());
            let handshake = LockedIoHandshake::new(
                blaze_own::Share::share(&in_flight),
                &TEST_ACTIVE,
                blaze_own::Share::share(&slot),
                blaze_own::Share::share(&signal),
                plant,
            );
            let mut saw_in_flight_on_unlock = false;
            let mut saw_active_on_unlock = false;
            handshake.complete_with_hook(iter, || {
                // Simulate a fast caller waking the instant `Some(res)` is published and
                // immediately invoking `FileLock::unlock` BEFORE any post-publish worker teardown.
                let taken = slot.lock().expect("slot lock").take();
                assert_eq!(taken, Some(iter));
                if in_flight.load(Ordering::Acquire) > 0 {
                    saw_in_flight_on_unlock = true;
                }
                if TEST_ACTIVE.load(Ordering::Acquire) > 0 {
                    saw_active_on_unlock = true;
                }
            });
            if saw_in_flight_on_unlock {
                return Err("locked-I/O handshake: caller unlock observed in_flight > 0 after completion");
            }
            if saw_active_on_unlock {
                return Err("locked-I/O handshake: active_workers remained > 0 after completion");
            }
            if in_flight.load(Ordering::Acquire) != 0 || TEST_ACTIVE.load(Ordering::Acquire) != 0 {
                return Err("locked-I/O handshake: counters did not return to zero");
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn locked_io_handshake_decrements_in_flight_before_publish_and_catches_planted_order() {
        run_locked_io_handshake_dst(bstd::procs::hermetic::LockedIoPlant::None)
            .expect("normal LockedIoHandshake must decrement in_flight before publishing completion");
        let err = run_locked_io_handshake_dst(bstd::procs::hermetic::LockedIoPlant::PublishBeforeDecrement)
            .expect_err("planted PublishBeforeDecrement must be caught immediately");
        assert!(err.contains("in_flight > 0"), "unexpected error: {err}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn lock_runtime_dst_survives_races_cap_overflow_and_spawn_failures() {
        let trace_a = run_lock_runtime_dst(bstd::procs::hermetic::LockPlant::None)
            .expect("deterministic LockRuntime DST must never drop on caller, leak slots, or skip unlock");
        let trace_b = run_lock_runtime_dst(bstd::procs::hermetic::LockPlant::None)
            .expect("deterministic LockRuntime DST second run must also pass");
        assert_eq!(trace_a, trace_b, "deterministic LockRuntime DST must replay identical traces across runs");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn lock_runtime_dst_catches_planted_sync_drop_on_unlock_cap() {
        let err = run_lock_runtime_dst(bstd::procs::hermetic::LockPlant::SyncDropOnUnlockCap)
            .expect_err("planted SyncDropOnUnlockCap must be caught");
        assert!(err.contains("dropped synchronously on caller thread"), "unexpected error: {err}");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn lock_runtime_dst_catches_planted_skip_abandon_unlock() {
        let err = run_lock_runtime_dst(bstd::procs::hermetic::LockPlant::SkipAbandonUnlock)
            .expect_err("planted SkipAbandonUnlock must be caught");
        assert!(err.contains("dropped without unlocking"), "unexpected error: {err}");
    }

    #[test]
    fn x86_64_linux_clang_pin_and_shelf_program_are_present() {
        let pin = super::toolchain::clang_pin(super::CLANG_JSON, "x86_64-unknown-linux-gnu")
            .expect("x86_64-unknown-linux-gnu must be pinned in pins/clang.json");
        assert_eq!(pin.name, "LLVM-23.1.4-Linux-X64.tar.xz");
        assert_eq!(pin.size, 2_017_061_664);
        let prog = super::shelf_pins::shelf_program(super::CLANG_JSON, "x86_64-unknown-linux-gnu", "clang-23")
            .expect("x86_64-unknown-linux-gnu clang-23 shelf_program must be pinned");
        assert_eq!(prog.shelf_path, "bin/clang-23");
    }

    #[test]
    fn fact_now_with_probes_propagates_answerer_id_errors() {
        use blaze_ids::ContentId;
        use blaze_own::{AString, Alloc};
        let alloc = blaze_own::testing::heap().alloc();
        struct FailingAnswerer;
        impl ProbeAnswerer for FailingAnswerer {
            fn id<'a>(&self, alloc: Alloc<'a>) -> Result<ContentId, AString<'a>> {
                Err(super::own_str("simulated nix runtime closure timeout", alloc))
            }
            fn answer<'a, 'b>(
                &self,
                _probe: &c_tasks_probe::CanonicalProbe<'_, '_>,
                _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
                alloc: Alloc<'a>,
            ) -> Result<c_tasks_probe::ProbeAnswer<'a>, AString<'a>> {
                Err(super::own_str("unused", alloc))
            }
        }
        struct EmptyStore;
        impl super::CProbeStore for EmptyStore {
            fn get<'a>(&self, _key: &c_tasks_probe::ProbeKey, _alloc: Alloc<'a>) -> Result<Option<blaze_own::AVec<'a, u8>>, super::CProbeStoreError> {
                Ok(None)
            }
            fn put(&self, _key: &c_tasks_probe::ProbeKey, _bytes: &[u8]) -> Result<(), super::CProbeStoreError> {
                Ok(())
            }
        }
        let shared = super::SharedAlloc::global();
        let probes = super::CScriptProbes {
            answerer: super::OwnArc::new_in(FailingAnswerer, shared),
            records: super::OwnArc::new_in(EmptyStore, shared),
            sdk_dir: super::OwnArc::from_str_in("", shared),
            sdk_id: super::OwnArc::from_str_in("", shared),
            sdk_digest: [0u8; 32],
            clang_sha256: [1u8; 32],
        };
        let key = c_tasks_probe::ProbeKey(ContentId::of("test", b"k"));
        let subj = blaze_build_script::facts::cc_probe_subject(
            &ContentId::of("test", b"a"),
            &[1u8; 32],
            &[0u8; 32],
            &key,
            alloc,
        );
        let err = crate::fact_pins::fact_now_with_probes(subj.as_str(), None, Some(&probes), alloc)
            .expect_err("identity failure must propagate as Err, never downgrade to Answer(None)");
        assert!(err.as_str().contains("simulated nix runtime closure timeout"), "unexpected error: {err}");
        let match_err = crate::spawn_route::fact_reply_matches_with_probes(
            subj.as_str(),
            Some(ContentId::of("test", b"reply")),
            None,
            Some(&probes),
            alloc,
        )
        .expect_err("fact_reply_matches_with_probes must also propagate identity failures");
        assert!(match_err.as_str().contains("simulated nix runtime closure timeout"), "unexpected error: {match_err}");
    }
}
