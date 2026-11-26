//! Hermetic pinned `clang-23` compiler probe runner + content-addressed probe cache
//! (`c-probe/<64-hex>`), implementing [`blaze_build_script::c_tasks_probe::ProbeAnswerer`]
//! (rule 78, D174/D177).
//!
//! - Single answerer + single cache: when the host's pinned `clang-23` is unpacked on the shelf
//!   (`<home>/.blaze/v2/toolchain/clang/<archive sha256>/bin/clang-23`), its SHA-256 is verified
//!   against `pins/clang.json`'s `shelf_programs` row and every probe miss is executed hermetically
//!   via `bstd::procs::hermetic::Hermetic` at `Level::Inputs` (Landlock + seccomp on Linux,
//!   Seatbelt on macOS) with `-fintegrated-cc1 --no-default-config` and only the probe's own
//!   `OUT_DIR` inputs laid in a fresh task folder.
//! - Offline zero-download fallback mode: when no shelf is unpacked locally, `OwnedTableAnswerer`
//!   answers from the checked-in `pins/answers/clang.json` corpus under a distinct strategy id so
//!   its records never collide with hermetic shelf records.
//! - Every resolved probe's canonical answer is cached in [`CProbeStore`] (`DirStore` under
//!   `c-probe/<64-hex>` or [`MemCProbeStore`] in tests), and [`c_probe_reply_now`] validates
//!   `cc-probe\n<64-hex>` per-probe reads directly from that store without hashing any table into
//!   every script's base key.

use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::{fs, io::{self, Read as _}, path::{Path, PathBuf}, process, sync::Mutex};

use blaze_build_script::c_tasks_probe::{
    self, CanonicalProbe, MOUNT, OUT_DIR, ProbeAnswer, ProbeAnswerer, ProbeCache, ProbeKey, SDKROOT, Table, TableAnswerer,
    canonical_reply_digest, decode_answer,
};
use blaze_build_script::facts;
use blaze_build_script::shelf_pins;
use blaze_build_script::toolchain;
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, Arc as OwnArc, SharedAlloc, String as OwnString, Vec as OwnVec};

type SharedStr = OwnString<SharedAlloc<'static>>;
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

/// Strategy id for [`HermeticClangProbe`] (hashed into [`ProbeKey`]).
const HERMETIC_STRATEGY_DOMAIN: &str = "blaze-build-script c-probe shelf clang 1";

/// Maximum captured stdout or stderr bytes for one compiler probe.
const PROBE_MOST_STREAM: u32 = 1 << 20;

/// Per-probe wall and CPU deadline (rule 18).
const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Virtual address-space cap for `clang-23` under `RLIMIT_AS` (342 MB binary + shared libs + LLVM).
const PROBE_MEMORY_BYTES: u64 = 8 << 30;

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
}

impl blaze_own::Share for CScriptProbes {}

/// Owned offline table answerer wrapping `pins/answers/clang.json` (used only when the host's
/// pinned `clang-23` shelf is not unpacked locally).
pub struct OwnedTableAnswerer;

impl ProbeAnswerer for OwnedTableAnswerer {
    fn id(&self) -> ContentId {
        TableAnswerer::strategy_id()
    }

    fn answer<'a, 'b>(
        &self,
        probe: &CanonicalProbe<'_, '_>,
        input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
        alloc: Alloc<'a>,
    ) -> Result<ProbeAnswer<'a>, AString<'a>> {
        let table = Table::parse(blaze_pins::CLANG_ANSWERS_JSON.as_bytes(), alloc).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let answerer = TableAnswerer::new(&table);
        answerer.answer(probe, input_bytes, alloc)
    }
}

/// Hermetic pinned `clang-23` compiler probe runner (`bstd::procs::hermetic::Hermetic` at
/// `Level::Inputs`).
pub struct HermeticClangProbe {
    clang_path: SharedStr,
    shelf_dir: SharedStr,
    shelf_digest: [u8; 32],
    sdk_dir: Option<SharedStr>,
    sdk_digest: Option<[u8; 32]>,
    work_root: PathBuf,
    deadline: Duration,
}

impl HermeticClangProbe {
    /// Strategy id of this answerer (hashed into [`ProbeKey`]).
    pub fn strategy_id() -> ContentId {
        ContentId::of(HERMETIC_STRATEGY_DOMAIN, b"v1")
    }

    /// Opens [`HermeticClangProbe`] if the host's pinned `clang-23` binary is unpacked under
    /// `<home>/.blaze/v2/toolchain/clang/<archive sha256>/bin/clang-23` and passes its
    /// `pins/clang.json` `shelf_programs` SHA-256 check. Returns `Ok(None)` when the shelf binary
    /// is not unpacked on disk yet; returns `Err` on pin mismatch or malformed path.
    pub fn open_from_shelf<'a>(
        home: &str,
        host: &str,
        sdk: Option<(&str, &str)>,
        work_root: PathBuf,
        alloc: Alloc<'a>,
    ) -> Result<Option<HermeticClangProbe>, AString<'a>> {
        let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
        let shelf_hex = hex_bytes(&pin.sha256, alloc);
        let shelf_path = Path::new(home).join(crate::native_setting::SHELF_DIR).join(shelf_hex.as_str());
        let Ok(prog) = shelf_pins::shelf_program(CLANG_JSON, host, "clang-23") else {
            return Ok(None);
        };
        let clang_candidate = shelf_path.join(prog.shelf_path);
        if !clang_candidate.is_file() {
            return Ok(None);
        }
        let got_sha = file_sha256(&clang_candidate).map_err(|e| own_fmt(format_args!("{}: {e}", clang_candidate.display()), alloc))?;
        shelf_pins::check_member(host, prog, got_sha).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
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
        Ok(Some(HermeticClangProbe {
            clang_path: clang_real,
            shelf_dir: shelf_real,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            work_root,
            deadline: PROBE_DEADLINE,
        }))
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
        let got_sha = file_sha256(&clang_candidate).map_err(|e| own_fmt(format_args!("{}: {e}", clang_candidate.display()), alloc))?;
        shelf_pins::check_member(host, prog, got_sha).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
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
        Ok(HermeticClangProbe {
            clang_path: clang_real,
            shelf_dir: shelf_real,
            shelf_digest: pin.sha256,
            sdk_dir,
            sdk_digest,
            work_root,
            deadline: PROBE_DEADLINE,
        })
    }
}

/// Opens the command's [`CScriptProbes`] once at the command's edge (rule 75):
/// - Uses `DirStore::build_scripts` for `c-probe/<64-hex>` records.
/// - Uses [`HermeticClangProbe`] as the sole answerer whenever the pinned shelf `clang-23` is
///   unpacked and verified on disk; otherwise uses [`OwnedTableAnswerer`] for zero-download
///   offline checks.
pub fn open_command_c_probes<'a>(
    home: &str,
    host: &str,
    sdk: Option<(&str, &str)>,
    store_dir: &Path,
    alloc: Alloc<'a>,
) -> Result<CScriptProbes, AString<'a>> {
    let (io, root) = native_world(store_dir).map_err(|e| own_fmt(format_args!("{e}"), alloc))?;
    let shared = SharedAlloc::global();
    let records: OwnArc<dyn CProbeStore, SharedAlloc<'static>> = OwnArc::new_in(DirStore::build_scripts(io.spawner(), io.boot().ok(), &root), shared);
    let sdk_id: OwnArc<str, SharedAlloc<'static>> = OwnArc::from_str_in(sdk.map_or("", |(_, id)| id), shared);
    let work_root = Path::new(home).join(crate::native_setting::NATIVE_WORK_DIR).join("c-probes");
    let answerer: OwnArc<dyn ProbeAnswerer + Send + Sync, SharedAlloc<'static>> = match HermeticClangProbe::open_from_shelf(home, host, sdk, work_root, alloc)? {
        Some(hermetic) => OwnArc::new_in(hermetic, shared),
        None => OwnArc::new_in(OwnedTableAnswerer, shared),
    };
    Ok(CScriptProbes { answerer, records, sdk_id })
}

/// Re-checks a recorded `cc-probe\n<64-hex>` read subject against `store`:
/// - `None`: `subject` is not a `cc-probe` subject.
/// - `Some(Some(digest))`: cached canonical probe answer exists and hashes to `digest`.
/// - `Some(None)`: cache miss or unreadable record (differs from any recorded digest, so only
///   scripts that asked this probe re-run).
pub fn c_probe_reply_now(subject: &str, store: Option<&dyn CProbeStore>, alloc: Alloc<'_>) -> Option<Option<ContentId>> {
    let key = facts::parse_cc_probe_subject(subject)?;
    let store = store?;
    let Ok(Some(bytes)) = store.get(&key, alloc) else {
        return Some(None);
    };
    let Ok(ans) = decode_answer(bytes.as_slice(), alloc) else {
        return Some(None);
    };
    Some(Some(canonical_reply_digest(&ans)))
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
        let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir_name = own_fmt(format_args!("p-{}-{seq}", process::id()), alloc);
        let task_path = self.work_root.join(dir_name.as_str());
        fs::create_dir_all(&task_path).map_err(|e| own_fmt(format_args!("create probe dir: {e}"), alloc))?;
        let task_real = canonical_utf8(&task_path, alloc)?;
        let _guard = TempDirGuard(task_path);

        // Materialize ONLY the probe's own OUT_DIR input files (`$IN<n><ext>` -> `<task>/in<n><ext>`).
        let mut placeholders: AVec<'a, (AString<'a>, AString<'a>)> = OwnVec::new_in(alloc);
        for (spelled, rel) in probe.input_rels.iter() {
            let Some(bytes) = input_bytes(rel.as_str()) else {
                return Err(own_fmt(format_args!("c-probe input `{}` missing from OUT_DIR", rel.as_str()), alloc));
            };
            let local_name = placeholder_filename(spelled.as_str(), alloc);
            let host_file = own_fmt(format_args!("{}/{local_name}", task_real.as_str()), alloc);
            fs::write(host_file.as_str(), bytes).map_err(|e| own_fmt(format_args!("write probe input {host_file}: {e}"), alloc))?;
            placeholders.push((own_str(spelled.as_str(), alloc), host_file));
        }

        // Collect output placeholders (`$OUT<n><ext>` -> `<task>/out<n><ext>`).
        for arg in probe.row.args.iter() {
            let text = arg.as_str();
            if text.starts_with(c_tasks_probe::OUTPUT) && !placeholders.iter().any(|(k, _)| k.as_str() == text) {
                let local_name = placeholder_filename(text, alloc);
                let host_file = own_fmt(format_args!("{}/{local_name}", task_real.as_str()), alloc);
                placeholders.push((own_str(text, alloc), host_file));
            }
        }

        // Build canonical argv executed by pinned `clang-23`.
        let mut owned_args: AVec<'a, AString<'a>> = OwnVec::new_in(alloc);
        if matches!(probe.row.program.as_str(), "c++" | "clang++" | "g++") {
            owned_args.push(own_str("--driver-mode=g++", alloc));
        }
        owned_args.push(own_str("-fintegrated-cc1", alloc));
        owned_args.push(own_str("--no-default-config", alloc));

        let mut has_sysroot = false;
        let sdk_str = self.sdk_dir.as_ref().map(OwnString::as_str);
        for arg in probe.row.args.iter() {
            let s = arg.as_str();
            has_sysroot |= s.starts_with("-isysroot") || s.starts_with("--sysroot");
            if let Some((_, host_file)) = placeholders.iter().find(|(k, _)| k.as_str() == s) {
                owned_args.push(own_str(host_file.as_str(), alloc));
            } else {
                owned_args.push(expand_roots(s, task_real.as_str(), sdk_str, alloc));
            }
        }
        if !has_sysroot && let Some(sdk) = sdk_str {
            owned_args.push(own_str("-isysroot", alloc));
            owned_args.push(own_str(sdk, alloc));
        }
        let mut arg_refs: AVec<'a, &str> = OwnVec::with_capacity_in(owned_args.len(), alloc);
        for arg in owned_args.iter() {
            arg_refs.push(arg.as_str());
        }

        // Build strictly ascending environment (`CLANG_ENV` + `SOURCE_DATE_EPOCH=0`).
        let mut owned_env: AVec<'a, (AString<'a>, AString<'a>)> = OwnVec::new_in(alloc);
        for (k, v) in probe.row.env.iter() {
            owned_env.push((own_str(k.as_str(), alloc), expand_roots(v.as_str(), task_real.as_str(), sdk_str, alloc)));
        }
        if !owned_env.iter().any(|(k, _)| k.as_str() == "SOURCE_DATE_EPOCH") {
            owned_env.push((own_str("SOURCE_DATE_EPOCH", alloc), own_str("0", alloc)));
        }
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

        let (world, _) = native_world(Path::new(task_real.as_str())).map_err(|e| own_fmt(format_args!("c-probe world: {e}"), alloc))?;
        let spawner = world.need_host().and_then(|h| h.spawner()).map_err(|m| own_fmt(format_args!("c-probe spawner: {}", m.capability), alloc))?;
        let clock = world.need_clock().map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let driver_clock = world.need_clock().map_err(|m| own_fmt(format_args!("c-probe clock: {}", m.capability), alloc))?;
        let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: driver_clock });
        let cx = world.cx(self.deadline, bstd::mem::TASK_FRAME).map_err(|_| own_str("c-probe context stopped", alloc))?;

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
            clock: ClockSpec::Fixed { unix_secs: 0 },
            seed: 0,
            budget: Budget { cpu: self.deadline, memory: PROBE_MEMORY_BYTES, steps: None },
            deadline: Deadline::after(clock.now().since_start(), self.deadline),
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

        // Normalize host paths back to canonical placeholders (longest match first).
        let mut rev_pairs: AVec<'a, (&str, &str)> = OwnVec::new_in(alloc);
        for (spelled, host_file) in placeholders.iter() {
            rev_pairs.push((host_file.as_str(), spelled.as_str()));
        }
        rev_pairs.push((self.clang_path.as_str(), "clang-23"));
        rev_pairs.push((task_real.as_str(), OUT_DIR));
        if let Some(sdk_dir) = sdk_str {
            rev_pairs.push((sdk_dir, SDKROOT));
        }
        rev_pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()));

        let stdout = replace_pairs(raw_out, rev_pairs.as_slice(), alloc);
        let stderr = replace_pairs(raw_err, rev_pairs.as_slice(), alloc);
        Ok(ProbeAnswer { code, stdout, stderr })
    }
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

fn expand_roots<'a>(text: &str, task_dir: &str, sdk_dir: Option<&str>, alloc: Alloc<'a>) -> AString<'a> {
    match sdk_dir {
        Some(sdk) => replace_pairs(text, &[(OUT_DIR, task_dir), (MOUNT, "/package"), (SDKROOT, sdk)], alloc),
        None => replace_pairs(text, &[(OUT_DIR, task_dir), (MOUNT, "/package")], alloc),
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

fn file_sha256(path: &Path) -> Result<[u8; 32], io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = blaze_sha256::Sha256::new();
    let mut buf = [0u8; 64 << 10];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(chunk) = buf.get(..n) {
            hasher.update(chunk);
        }
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
    use blaze_build_script::facts::{cc_probe_subject, parse_cc_probe_subject};
    use blaze_build_script::toolchain::clang_pin;
    use blaze_pins::CLANG_JSON;

    use super::{CProbeStore as _, CacheAdapter, HermeticClangProbe, MemCProbeStore, c_probe_reply_now};

    /// Differential test (Rule 25 + D174): when the host's pinned `clang-23` shelf is unpacked on
    /// disk, every Linux ARM64 row in `pins/answers/clang.json` is re-answered hermetically under
    /// Landlock (`Level::Inputs`) and asserted byte-equal on `(code, stdout, stderr)`, AND novel
    /// flags (`-Wshadow`, `-Wuninitialized`, `-ffile-prefix-map=/workspace/vendored/aws-lc-sys=`)
    /// are answered and cached without touching `clang.json`.
    #[test]
    fn hermetic_clang_probe_matches_linux_arm64_corpus_and_answers_novel_flags() {
        let host = "aarch64-unknown-linux-gnu";
        if !cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            return;
        }
        let Ok(home) = std::env::var("HOME") else { return };
        let alloc = blaze_own::testing::heap().alloc();
        let pin = clang_pin(CLANG_JSON, host).expect("linux arm64 pin");
        let pin_hex = super::hex_bytes(&pin.sha256, alloc);
        let shelf = Path::new(&home).join(crate::native_setting::SHELF_DIR).join(pin_hex.as_str());
        if !shelf.join("bin/clang-23").is_file() {
            eprintln!("skipped: {} not unpacked", shelf.display());
            return;
        }
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

            let subject = cc_probe_subject(&resolved.key, alloc);
            assert_eq!(parse_cc_probe_subject(subject.as_str()), Some(resolved.key));
            assert_eq!(c_probe_reply_now(subject.as_str(), Some(store.as_ref()), alloc), Some(Some(resolved.canonical_digest)));
            checked += 1;
        }
        assert!(checked >= 27, "expected >= 27 Linux ARM64 rows checked, got {checked}");

        for extra_flag in ["-Wshadow", "-Wuninitialized", "-ffile-prefix-map=/workspace/vendored/aws-lc-sys="] {
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
                env: &[],
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
        let _ = answerer.id();
    }

    /// Rule 23 selective invalidation: changing one cached `c-probe` record moves ONLY the
    /// per-probe read digest of that probe, leaving unrelated scripts' probes untouched.
    #[test]
    fn selective_c_probe_invalidation_only_moves_asking_probe() {
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
        let subj_a = cc_probe_subject(&res_a.key, alloc);
        let subj_b = cc_probe_subject(&res_b.key, alloc);

        assert_eq!(c_probe_reply_now(subj_a.as_str(), Some(&store), alloc), Some(Some(res_a.canonical_digest)));
        assert_eq!(c_probe_reply_now(subj_b.as_str(), Some(&store), alloc), Some(Some(res_b.canonical_digest)));

        let mut mutated = AVec::new_in(alloc);
        encode_answer(
            &ProbeAnswer { code: 1, stdout: OwnString::new_in(alloc), stderr: OwnString::from_str_in("error: unsupported", alloc) },
            &mut mutated,
        );
        store.overwrite(res_a.key, mutated.as_slice());

        assert_ne!(
            c_probe_reply_now(subj_a.as_str(), Some(&store), alloc),
            Some(Some(res_a.canonical_digest)),
            "mutated probe A must invalidate its asking script"
        );
        assert_eq!(
            c_probe_reply_now(subj_b.as_str(), Some(&store), alloc),
            Some(Some(res_b.canonical_digest)),
            "unrelated probe B must remain valid"
        );
    }
}
