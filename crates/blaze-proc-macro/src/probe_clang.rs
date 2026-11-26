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
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::{path::{Path, PathBuf}, process, sync::Mutex};

use blaze_build_script::c_tasks_probe::{
    self, CanonicalProbe, OUT_DIR, ProbeAnswer, ProbeAnswerer, ProbeCache, ProbeKey, SDKROOT, Segment,
    canonical_reply_digest, decode_answer,
};
use blaze_build_script::facts;
use blaze_build_script::shelf_pins;
use blaze_build_script::toolchain;
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Arc as OwnArc, Share as _, SharedAlloc, String as OwnString, Vec as OwnVec};
use blaze_pins::CLANG_JSON;
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
use bstd::procs::{
    Exit,
    hermetic::{
        Budget, Capture, ClockSpec, Fault, FilesError, FilesOp, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
        HermeticStreams, Level, Program, ReportSpec, RootFiles,
    },
};
#[cfg(not(target_os = "linux"))]
use bstd::procs::hermetic::VerifiedStamp;

use crate::record::{DirStore, FilesError as StoreFilesError};

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

/// Domain tag sealing the per-boot `clang-23` metadata verification voucher (non-Linux).
#[cfg(not(target_os = "linux"))]
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

/// Proof of verified `clang-23` held in [`HermeticClangProbe::verify_flight`]:
/// - On Linux, keeps the exact verified inode's `O_RDONLY | O_CLOEXEC` [`bstd::procs::hermetic::VerifiedExec`]
///   descriptor alive so Landlock and `execveat(..., AT_EMPTY_PATH)` execute that exact inode with
///   zero path-reopen TOCTOU.
/// - On non-Linux hosts (macOS), keeps the verified [`VerifiedStamp`] and revalidates it before
///   and after execution so a replaced binary is refused and never cached.
#[derive(Clone)]
struct VerifiedClang {
    #[cfg(target_os = "linux")]
    exec: OwnArc<bstd::procs::hermetic::VerifiedExec, SharedAlloc<'static>>,
    #[cfg(not(target_os = "linux"))]
    stamp: VerifiedStamp,
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
    io: bstd::io::Io,
    #[cfg_attr(target_os = "linux", expect(dead_code, reason = "used on non-Linux hosts without execveat"))]
    shelf_dir_cap: bstd::fs::Dir<bstd::fs::ReadWrite>,
    work_base_dir: bstd::fs::Dir<bstd::fs::ReadWrite>,
    work_rel: SharedStr,
    work_root: SharedStr,
    deadline: Duration,
    verify_flight: bstd::sync::Flight<(), VerifiedClang, OwnArc<str, SharedAlloc<'static>>, SharedAlloc<'static>>,
    #[cfg(target_os = "linux")]
    root_reader: Mutex<Option<bstd::procs::hermetic::RootReader>>,
}

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
        Self::strategy_id_for_clang_inner(
            #[cfg(target_os = "linux")]
            None,
            cx,
            host,
            clang_path,
            alloc,
        )
    }

    /// Same as [`Self::strategy_id_for_clang`], reusing a caller-owned
    /// [`bstd::procs::hermetic::RootReader`] so cancelled/stalled reads never allocate or leak a
    /// fresh async driver per probe.
    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")] reader: Option<&bstd::procs::hermetic::RootReader>,
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
        #[cfg(target_os = "linux")]
        {
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
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (cx, clang_path);
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
        Self::strategy_id_for_home_inner(
            #[cfg(target_os = "linux")]
            None,
            cx,
            home,
            host,
            alloc,
        )
    }

    /// Same as [`Self::strategy_id_for_home`], reusing a caller-owned
    /// [`bstd::procs::hermetic::RootReader`].
    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")] reader: Option<&bstd::procs::hermetic::RootReader>,
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
        Self::strategy_id_for_clang_inner(
            #[cfg(target_os = "linux")]
            reader,
            cx,
            host,
            path_str,
            alloc,
        )
        .map(Some)
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
            #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "linux")]
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
    /// - On Linux, opens `clang-23` ONCE as [`bstd::procs::hermetic::VerifiedExec`], hashes/validates
    ///   that exact inode, and retains its `OwnedFd` so Landlock and `execveat(..., AT_EMPTY_PATH)`
    ///   execute that exact verified inode with zero TOCTOU.
    /// - On macOS, verifies via `self.shelf_dir_cap` under `vcx` and retains the verified
    ///   [`VerifiedStamp`] for pre/post-exec revalidation.
    fn ensure_verified<'a>(
        &self,
        work_root: &bstd::fs::Dir<bstd::fs::ReadWrite>,
        clock: &bstd::clock::Clock,
        began: bstd::clock::Instant,
        boot: Option<blaze_core::Boot>,
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

                #[cfg(target_os = "linux")]
                {
                    let _ = (work_root, boot);
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
                }

                #[cfg(not(target_os = "linux"))]
                {
                    let voucher_name = own_fmt(format_args!("verified-{}.stamp", hex_bytes(&prog.sha256, alloc)), alloc);
                    let clang_rel = bstd::path::Path::new(prog.shelf_path)
                        .map_err(|_| fail(own_fmt(format_args!("invalid shelf path `{}`", prog.shelf_path), alloc)))?;
                    let meta = match self.shelf_dir_cap.stat(vcx, &clang_rel) {
                        Ok(Some(meta)) if !meta.dir => meta,
                        Ok(_) => {
                            return Err(fail(own_fmt(
                                format_args!(
                                    "pinned clang-23 shelf binary missing at {} (expected archive sha256 {shelf_hex} from pins/clang.json)",
                                    self.clang_path.as_str()
                                ),
                                alloc,
                            )));
                        }
                        Err(e) => {
                            return Err(fail(own_fmt(
                                format_args!("stat pinned clang-23 shelf binary at {}: {e}", self.clang_path.as_str()),
                                alloc,
                            )));
                        }
                    };
                    let stamp = stamp_of(&meta);
                    let expected_voucher = boot.map(|b| verify_voucher_digest(b, &stamp, &prog.sha256));
                    if let Some(expected) = expected_voucher
                        && let Ok(voucher_rel) = bstd::path::Path::new(voucher_name.as_str())
                        && read_voucher_bounded(work_root, vcx, &voucher_rel) == Some(expected.0)
                    {
                        return Ok(VerifiedClang { stamp });
                    }
                    let got_sha = file_sha256_bounded_on(&self.shelf_dir_cap, vcx, &clang_rel, prog.shelf_path, alloc)
                        .map_err(fail)?;
                    shelf_pins::check_member(self.host.as_str(), prog, got_sha)
                        .map_err(|e| fail(own_fmt(format_args!("{e}"), alloc)))?;
                    let after = match self.shelf_dir_cap.stat(vcx, &clang_rel) {
                        Ok(Some(m)) if !m.dir => stamp_of(&m),
                        _ => return Err(OwnArc::from_str_in("pinned clang-23 changed during SHA-256 verification", shared)),
                    };
                    if after != stamp {
                        return Err(OwnArc::from_str_in("pinned clang-23 changed during SHA-256 verification", shared));
                    }
                    if let Some(expected) = expected_voucher
                        && let Ok(voucher_rel) = bstd::path::Path::new(voucher_name.as_str())
                    {
                        let _ = work_root.publish(
                            vcx,
                            &voucher_rel,
                            bstd::fs::Content::bytes(&expected.0),
                            bstd::fs::Durable::No,
                        );
                    }
                    Ok(VerifiedClang { stamp })
                }
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

#[cfg(not(target_os = "linux"))]
fn stamp_of(meta: &bstd::fs::Meta) -> VerifiedStamp {
    VerifiedStamp {
        dev: meta.stamp.dev,
        ino: meta.stamp.ino,
        len: meta.stamp.len,
        modified_ns: i128::from(meta.stamp.modified_ns),
        changed_ns: i128::from(meta.stamp.changed_ns),
    }
}

/// Computes a per-boot file-metadata stamp digest binding `(boot_id, expected_sha256, dev, ino,
/// size, modified_ns, changed_ns)` from a verified executable's inode stamp.
#[cfg(not(target_os = "linux"))]
fn verify_voucher_digest(boot: blaze_core::Boot, stamp: &VerifiedStamp, expected_sha256: &[u8; 32]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(VERIFY_VOUCHER_DOMAIN);
    h.part(&boot.0)
        .part(expected_sha256)
        .update(&stamp.dev.to_le_bytes())
        .update(&stamp.ino.to_le_bytes())
        .update(&stamp.len.to_le_bytes())
        .update(&stamp.modified_ns.to_le_bytes())
        .update(&stamp.changed_ns.to_le_bytes());
    h.finish()
}

/// Reads an exact 32-byte verification voucher through deadline-aware `bstd::fs` into a stack
/// buffer (rejecting any missing, truncated, or oversized file without allocating).
#[cfg(not(target_os = "linux"))]
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

/// Opens the command's [`CProbeSetting`] once at the command's edge (rule 75) through the
/// command's existing deadline-aware [`bstd::fs::Fs`] capability (never opening a separate
/// `NativeWorld` outside a context):
/// - Uses `DirStore::build_scripts` for `c-probe/<64-hex>` records and [`HermeticClangProbe`] as
///   the sole answerer (rule 3: never silently switches to the offline table).
/// - On hosts without a clang pin (e.g. x86-64 Linux before D69), returns
///   [`CProbeSetting::Unavailable`] immediately with zero filesystem I/O.
pub fn open_command_c_probes<'a>(
    cx: &bstd::cx::Cx,
    fs: &bstd::fs::Fs,
    home: &str,
    host: &str,
    sdk: Option<(&str, &str)>,
    alloc: Alloc<'a>,
) -> CProbeSetting {
    let shared = SharedAlloc::global();
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
    let Ok(store_rel) = bstd::path::Path::new("build-script/records") else {
        return CProbeSetting::Unavailable(OwnArc::from_str_in("invalid c-probe records path", shared));
    };
    let store_dir = blaze_v2.subdir(&store_rel);
    let shelf_hex = hex_bytes(&pin.sha256, alloc);
    let shelf_sub_str = own_fmt(format_args!("toolchain/clang/{}", shelf_hex.as_str()), alloc);
    let Ok(shelf_sub) = bstd::path::Path::new(shelf_sub_str.as_str()) else {
        return CProbeSetting::Unavailable(OwnArc::from_str_in("invalid c-probe shelf path", shared));
    };
    let shelf_dir_cap = blaze_v2.subdir(&shelf_sub);
    let records: OwnArc<dyn CProbeStore, SharedAlloc<'static>> =
        OwnArc::new_in(DirStore::build_scripts(io.spawner(), io.boot().ok(), &store_dir), shared);
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
/// unbounded `std::fs::remove_dir_all`). On the result-bearing success path [`Self::cleanup`]
/// removes the folder explicitly and propagates any cleanup error; `Drop` serves as a bounded
/// fallback on early error/unwind paths.
struct TaskDirGuard<'w> {
    io: &'w bstd::io::Io,
    work_root: &'w bstd::fs::Dir<bstd::fs::ReadWrite>,
    task_rel: bstd::path::Path,
    cleaned: bool,
}

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

impl HermeticClangProbe {
    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        {
            let reader = self.shared_root_reader(cx, alloc)?;
            Self::strategy_id_for_clang_with(&reader, cx, self.host.as_str(), self.clang_path.as_str(), alloc)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::strategy_id_for_clang(cx, self.host.as_str(), self.clang_path.as_str(), alloc)
        }
    }
}

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
        let verified = self.ensure_verified(&work_root, &clock, probe_began, self.io.boot().ok(), alloc)?;

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
            let rendered = render_segments(segs.as_slice(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc)?;
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
                render_segments(segs.as_slice(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc)?,
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
            budget: Budget { cpu: run_rem, memory: PROBE_MEMORY_BYTES, steps: None },
            deadline: Deadline::after(clock.now().since_start(), run_rem),
            streams: HermeticStreams { stdout: Capture::Keep { most: PROBE_MOST_STREAM }, stderr: Capture::Keep { most: PROBE_MOST_STREAM } },
            level: Level::Inputs,
            report: ReportSpec::WholeRoots,
        })
        .map_err(|e| own_fmt(format_args!("c-probe spec refused: {:?} ({:?})", e.fault, e.rule), alloc))?;
        #[cfg(target_os = "linux")]
        let spec = {
            // Re-hash the Nix runtime closure under `cx` immediately before launching to ensure
            // `/run/current-system` did not switch since `probe.answerer_id` was computed.
            // `verified.exec` itself is already a kernel-sealed immutable `memfd`.
            let pre_id = self.live_strategy_id(&cx, alloc)?;
            if pre_id != probe.answerer_id {
                return Err(own_str("host Nix runtime closure changed before probe execution (rule 21)", alloc));
            }
            spec.with_verified_exec(&verified.exec)
        };

        #[cfg(not(target_os = "linux"))]
        {
            let prog = shelf_pins::shelf_program(CLANG_JSON, self.host.as_str(), "clang-23")
                .map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
            let clang_rel = bstd::path::Path::new(prog.shelf_path)
                .map_err(|_| own_fmt(format_args!("invalid shelf path `{}`", prog.shelf_path), alloc))?;
            let cur = self
                .shelf_dir_cap
                .stat(&cx, &clang_rel)
                .map_err(|e| own_fmt(format_args!("pre-exec stat clang-23: {e}"), alloc))?
                .filter(|m| !m.dir)
                .map(|m| stamp_of(&m));
            if cur != Some(verified.stamp) {
                return Err(own_str("pinned clang-23 inode changed before probe execution (rule 21)", alloc));
            }
        }

        let ran = hermetic
            .run(&cx, &spec, &ProbeFiles, alloc)
            .map_err(|e| own_fmt(format_args!("c-probe hermetic run failed: {:?} ({:?})", e.fault, e.rule), alloc))?;

        #[cfg(target_os = "linux")]
        {
            let Some(post_rem) = remaining() else {
                return Err(own_str("c-probe deadline exhausted before post-exec runtime closure revalidation", alloc));
            };
            let post_cx = self
                .io
                .cx(post_rem, bstd::mem::TASK_FRAME)
                .map_err(|_| own_str("c-probe post-exec context stopped", alloc))?;
            let post_id = self.live_strategy_id(&post_cx, alloc)?;
            if post_id != probe.answerer_id {
                return Err(own_str("host Nix runtime closure changed during probe execution (rule 21)", alloc));
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            let Some(post_rem) = remaining() else {
                return Err(own_str("c-probe deadline exhausted before post-exec inode revalidation", alloc));
            };
            let post_cx = self
                .io
                .cx(post_rem, bstd::mem::TASK_FRAME)
                .map_err(|_| own_str("c-probe post-exec context stopped", alloc))?;
            let prog = shelf_pins::shelf_program(CLANG_JSON, self.host.as_str(), "clang-23")
                .map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
            let clang_rel = bstd::path::Path::new(prog.shelf_path)
                .map_err(|_| own_fmt(format_args!("invalid shelf path `{}`", prog.shelf_path), alloc))?;
            let cur = self
                .shelf_dir_cap
                .stat(&post_cx, &clang_rel)
                .map_err(|e| own_fmt(format_args!("post-exec stat clang-23: {e}"), alloc))?
                .filter(|m| !m.dir)
                .map(|m| stamp_of(&m));
            if cur != Some(verified.stamp) {
                return Err(own_str("pinned clang-23 inode changed during probe execution (rule 21)", alloc));
            }
        }

        let code = match ran.exit {
            Exit::Exited(code) => code,
            Exit::Signaled(sig) => return Err(own_fmt(format_args!("c-probe clang-23 killed by signal {sig}"), alloc)),
        };

        let raw_out = core::str::from_utf8(ran.stdout.as_slice()).map_err(|_| own_str("c-probe stdout is not UTF-8", alloc))?;
        let raw_err = core::str::from_utf8(ran.stderr.as_slice()).map_err(|_| own_str("c-probe stderr is not UTF-8", alloc))?;

        // Normalize host paths back to canonical placeholders (longest match first) while escaping
        // literal `$` -> `$$` so literal `$OUT_DIR`/`$IN0.c` in Clang output round-trip verbatim
        // through `CanonicalProbe::unspell`, and `<shelf>/bin` (`InstalledDir:`) never leaks `$HOME`.
        let stdout = normalize_reply(raw_out, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc);
        let stderr = normalize_reply(raw_err, self.clang_path.as_str(), self.shelf_dir.as_str(), task_real.as_str(), sdk_str, placeholders.as_slice(), alloc);
        let Some(cleanup_rem) = remaining() else {
            return Err(own_str("c-probe deadline exhausted before cleanup", alloc));
        };
        let cleanup_cx = self
            .io
            .cx(cleanup_rem, bstd::mem::TASK_FRAME)
            .map_err(|_| own_str("c-probe cleanup context stopped", alloc))?;
        guard.cleanup(&cleanup_cx, alloc)?;
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
    replace_pairs_escape_dollar(raw, rev_pairs.as_slice(), alloc)
}

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

fn own_str<'a>(s: &str, alloc: Alloc<'a>) -> AString<'a> {
    OwnString::from_str_in(s, alloc)
}

fn own_fmt<'a>(args: fmt::Arguments<'_>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = OwnString::new_in(alloc);
    let _ = out.write_fmt(args);
    out
}

fn path_utf8<'a>(path: &Path, alloc: Alloc<'a>) -> Result<SharedStr, AString<'a>> {
    match path.to_str() {
        Some(s) => Ok(OwnString::from_str_in(s, SharedAlloc::global())),
        None => Err(own_fmt(format_args!("{}: path is not UTF-8", path.display()), alloc)),
    }
}

#[cfg(not(target_os = "linux"))]
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
            assert_eq!(canonical.stdout.as_str(), stdout.as_str(), "stdout mismatch for {:?}", row.args);
            assert_eq!(canonical.stderr.as_str(), stderr.as_str(), "stderr mismatch for {:?}", row.args);

            let empty_sdk = blaze_build_script::facts::sdk_id_digest("");
            let aid = answerer.id(alloc).expect("answerer id");
            let subject = cc_probe_subject(&aid, &pin.sha256, &empty_sdk, &resolved.key, alloc);
            assert_eq!(
                parse_cc_probe_subject(subject.as_str()),
                Some(CcProbeSubject { answerer: aid, clang: pin.sha256, sdk: empty_sdk, key: resolved.key })
            );
            assert_eq!(
                c_probe_reply_now(subject.as_str(), &aid, &pin.sha256, &empty_sdk, Some(store.as_ref()), alloc),
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

        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc), Some(Some(res_a.canonical_digest)));
        assert_eq!(c_probe_reply_now(subj_b.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc), Some(Some(res_b.canonical_digest)));

        // Upgrading clang pin, answerer id, or macOS SDK invalidates recorded probe reads even
        // though the old record file still exists on disk.
        let new_pin = [8u8; 32];
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &new_pin, &sdk_a, Some(&store), alloc), Some(None));
        let new_answerer = blaze_ids::ContentId::of("stub-cprobe-selective-test", b"2");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &new_answerer, &pin, &sdk_a, Some(&store), alloc), Some(None));
        let new_sdk = blaze_build_script::facts::sdk_id_digest("macos-sdk-15.4");
        assert_eq!(c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &new_sdk, Some(&store), alloc), Some(None));

        // Mutating ONLY probe A's cached record invalidates A and leaves B intact.
        let mut mutated = AVec::new_in(alloc);
        encode_answer(
            &ProbeAnswer { code: 1, stdout: OwnString::new_in(alloc), stderr: OwnString::from_str_in("error: unsupported", alloc) },
            &mut mutated,
        );
        store.overwrite(res_a.key, mutated.as_slice());

        assert_ne!(
            c_probe_reply_now(subj_a.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc),
            Some(Some(res_a.canonical_digest)),
            "mutated probe A must invalidate its asking script"
        );
        assert_eq!(
            c_probe_reply_now(subj_b.as_str(), &stub_id, &pin, &sdk_a, Some(&store), alloc),
            Some(Some(res_b.canonical_digest)),
            "unrelated probe B must remain valid"
        );

        // Unsupported host returns Unavailable immediately with zero shelf I/O; on a supported host
        // with an absent shelf, setup uses the command's existing `bstd::fs::Fs` capability (zero
        // synchronous std::fs) and the first probe miss fails loudly naming the missing clang-23
        // shelf binary.
        let empty_home = std::env::temp_dir().join(format!("blaze-cprobe-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty_home);
        let (tmp_io, tmp_dir) = crate::record::native_world(&std::env::temp_dir()).expect("tmp world");
        let tmp_fs = tmp_io.fs().expect("tmp fs");
        let tmp_cx = tmp_io.cx(super::Duration::from_secs(5), bstd::mem::TASK_FRAME).expect("tmp cx");
        let unsupported = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "x86_64-unknown-linux-gnu", None, alloc);
        assert!(matches!(unsupported, CProbeSetting::Unavailable(_)), "unsupported host must return Unavailable");
        std::fs::create_dir_all(&empty_home).expect("create empty home");
        let missing = open_command_c_probes(&tmp_cx, &tmp_fs, &empty_home.to_string_lossy(), "aarch64-unknown-linux-gnu", None, alloc);
        let CProbeSetting::Ready(missing_probes) = missing else {
            panic!("supported host setup must defer shelf check until first probe miss");
        };
        let err = resolve(&*missing_probes.answerer, &cache, &missing_probes.clang_sha256, &sp_a, dig, &mut bytes, alloc)
            .expect_err("missing shelf must fail loudly on first probe miss");
        assert!(err.as_str().contains("missing"), "unexpected missing-shelf error: {err}");
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

        // Lazy verification runs through `bstd::sync::Flight` without holding any lock across
        // filesystem waits; concurrent waiters park on its Signal and honor their own deadline.
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
            #[cfg(target_os = "linux")]
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
        let Err(err) = dummy.ensure_verified(&tmp_dir, &clock, began, None, alloc) else {
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

        // Reply normalization scrubs `<shelf>/bin/clang-23` + `<shelf>/bin` (`InstalledDir:`) AND
        // escapes literal `$` -> `$$` so literal `$OUT_DIR`/`$IN0.c` round-trip verbatim through
        // `CanonicalProbe::unspell` while real host paths substitute cleanly.
        let placeholders = [(
            OwnString::from_str_in("$IN0.c", alloc),
            OwnString::from_str_in("/tmp/task-1/in0.c", alloc),
        )];
        let norm = super::normalize_reply(
            "clang: /home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23\nInstalledDir: /home/user/.blaze/v2/toolchain/clang/abc/bin\nreal=/tmp/task-1/in0.c dir=/tmp/task-1 lit=$OUT_DIR lit2=$IN0.c\n",
            "/home/user/.blaze/v2/toolchain/clang/abc/bin/clang-23",
            "/home/user/.blaze/v2/toolchain/clang/abc",
            "/tmp/task-1",
            None,
            placeholders.as_slice(),
            alloc,
        );
        assert_eq!(
            norm.as_str(),
            "clang: clang-23\nInstalledDir: $CLANG_SHELF/bin\nreal=$IN0.c dir=$OUT_DIR lit=$$OUT_DIR lit2=$$IN0.c\n"
        );
        let canon = c_tasks_probe::canonicalize(stub_id, &pin, &sp_a, dig, alloc).expect("canon");
        let restored = canon.unspell(norm.as_str(), &sp_a, alloc);
        assert_eq!(
            restored.as_str(),
            "clang: clang-23\nInstalledDir: $CLANG_SHELF/bin\nreal=/out/flag_check.c dir=/out lit=$OUT_DIR lit2=$IN0.c\n"
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

        let entries: &[(u32, &[u8], &[u8])] = &[
            (target_flags, b"libz.so.1", b"/opt/custom/glibc-hwcaps/v3/libz.so.1"),
            (target_flags, b"libz.so.1", b"/opt/custom/libz.so.1"),
            (other_flags, b"libz.so.1", b"/usr/lib/wrong-arch/libz.so.1"),
            (target_flags, b"libc.so.6", b"/usr/lib/libc.so.6"),
        ];
        let str_base = 48 + entries.len() * 24;
        let mut strings = std::vec::Vec::new();
        let mut offsets = std::vec::Vec::new();
        for &(flags, key, val) in entries {
            let k_off = str_base + strings.len();
            strings.extend_from_slice(key);
            strings.push(0);
            let v_off = str_base + strings.len();
            strings.extend_from_slice(val);
            strings.push(0);
            offsets.push((flags, k_off as u32, v_off as u32));
        }
        let mut new_only = std::vec::Vec::new();
        new_only.extend_from_slice(b"glibc-ld.so.cache1.1");
        new_only.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        new_only.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        new_only.resize(48, 0);
        for (flags, k_off, v_off) in offsets {
            new_only.extend_from_slice(&flags.to_le_bytes());
            new_only.extend_from_slice(&k_off.to_le_bytes());
            new_only.extend_from_slice(&v_off.to_le_bytes());
            new_only.extend_from_slice(&0u32.to_le_bytes());
            new_only.extend_from_slice(&0u64.to_le_bytes());
        }
        new_only.extend_from_slice(&strings);

        let mut compat = std::vec::Vec::new();
        compat.extend_from_slice(b"ld.so-1.7.0\0");
        compat.extend_from_slice(&1u32.to_le_bytes());
        compat.extend_from_slice(&[0u8; 12]);
        while compat.len() % 8 != 0 {
            compat.push(0);
        }
        compat.extend_from_slice(&new_only);

        for buf in [&new_only, &compat] {
            let mut found: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
            let ok = bstd::procs::hermetic::parse_ld_so_cache_matches(buf, b"libz.so.1", |path| {
                found.push(path.to_vec());
                Ok(true)
            })
            .expect("valid ld.so.cache must parse");
            assert!(ok);
            assert_eq!(
                found,
                std::vec![
                    b"/opt/custom/glibc-hwcaps/v3/libz.so.1".to_vec(),
                    b"/opt/custom/libz.so.1".to_vec(),
                ]
            );
        }
        let err = bstd::procs::hermetic::parse_ld_so_cache_matches(b"not-an-ld-cache", b"libz.so.1", |_| Ok(true))
            .expect_err("corrupt ld.so.cache must fail loudly");
        assert!(matches!(err, bstd::procs::hermetic::Fault::Host { code: 22, .. }));
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
