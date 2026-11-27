//! On-demand self-compilation of `UnitKind::BuildScript` (`build.rs`) and `UnitKind::ProcMacro`
//! crates into `wasm64` (`memory64`) modules targeting `wasm64-blaze-wasip1` (zero `rustc`, zero
//! `toolrun.sh`, zero `wasm-lld`, zero `wasi-libc`).
//!
//! Invoked on a module-store miss by `script_modules::run` (for `RanAs::Wasm` build scripts) and
//! `macros::run` (for proc-macro crates). Compiled modules are atomically published to
//! `<store>/build-script/modules/<key>` and `<store>/proc-macro/modules/<key>` in the exact record
//! layout read by `blaze_proc_macro::{script_unit, module}`, so both the current command's wave 1
//! `Script`/`CrateScope` tasks and every subsequent command reuse them directly.

use core::cell::RefCell;
use core::fmt::Write as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_build_script::key::CMode;
use blaze_build_script::outputs::OutputsRef;
use blaze_build_script::toolchain::Mode;
use blaze_emit::{Isa, Os};
use blaze_export::wasm64::{Wasm64Opts, check_wasm64};
use blaze_fetch::{FileBytes, PackView};
use blaze_id::{ContentId, CrateItemKey, ItemKey as LibKey};
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::Grant;
use blaze_ladder::code_records::{Builder, Settings, TierName};
use blaze_ladder::crates::DirShelf;
use blaze_ladder::front2::body_host::{BodySources, Sourced};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::wasm64::{
    Wasm64BuildError, Wasm64BuildOpts, compile_items_to_wasm64, compile_roots_to_wasm64,
    default_wasm64_import, fn_symbol_of_item,
};
use blaze_ladder::run::make_one::{Maker, NamesRecords, SecondCheck};
use blaze_ladder::run::special::{MakerStart, run_roots};
use blaze_ladder::symbols::static_symbol;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_mem::{Arena, Frame, Tag};
use blaze_own::{AVec, Alloc, Bump, Erased, Vec as OwnVec};
use blaze_proc_macro::module::{Exported, sha256_hex};
use blaze_proc_macro::script::ScriptRuntime;
use blaze_proc_macro::script_sandbox::{ScriptRoot, Tree, dir_tree, mem_tree_with_submodules};
use blaze_proc_macro::script_unit::{self, Setting};
use blaze_proc_macro::spawn_route::SpawnSetting;
use blaze_scope::crate_env::{BinNames, CrateEnv, EnvFacts, SeenRoot};
use blaze_scope::crate_scope::{Kept, Ready, Solved, resolve_kept};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts, aligned};
use blaze_scope::task::{MacroBody, Par, Scratch, Serial, proc_macro_decls_key};
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, ItemKind, ProcKind};
use blaze_toolchain::{Role, ToolchainId, ToolchainView, UnitKind as TcUnitKind};
use blaze_typeck::ItemKey;
use blaze_unit_graph::{
    CfgAtom, Records as SourceRecords, StoreSources, Unit, UnitGraph, UnitId, UnitKind, implicit,
};

/// The target triple every self-compiled build script and proc-macro targets.
pub(crate) const WASM64_TARGET: &str = "wasm64-blaze-wasip1";

/// The exported global name holding `&'static [Client]` in self-compiled proc-macro modules.
const PROC_MACRO_DECLS_GLOBAL: &str = "__rustc_proc_macro_decls_blaze__";

/// Virtual reservation for the scratch arena of one on-demand wasm64 compile (costs zero physical
/// pages until touched; unmapped on return). Growing LIR/Wasm worklist structures use `heap_alloc`
/// so old buffers are freed on realloc; this frame only holds resolved scope/items records.
const COMPILE_FRAME_RESERVE: usize = 2 << 30;

/// Atomic counter for unique staging directory names during atomic module publication.
static STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Single-flight guard used ONLY when `<store>/wasm64-sysroot-v1/<tc_id>/` has missing sysroot
/// crate records on a completely cold store, preventing N parallel build-script/proc-macro
/// workers from simultaneously re-parsing `core`/`alloc`/`std` (~3.5x peak RSS reduction).
static SYSROOT_INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Immutable inputs borrowed from the coordinator for on-demand wasm64 self-compilation.
#[derive(Clone, Copy)]
pub(crate) struct CompileInputs<'a> {
    /// Decodable `UnitGraph` section bytes (`graph_payload::sections(payload)?.graph`).
    pub graph: &'a [u8],
    /// Raw `Cargo.lock` bytes, alive for the duration of the command.
    pub lock: &'a [u8],
    /// Resolved checkout root on disk; `None` in simulated test worlds (no on-disk self-compile).
    pub checkout: Option<&'a blaze_proc_macro::script_sandbox::RealRoot>,
    /// Workspace members and external path packages by lock id.
    pub members: &'a [(LockId, &'a str, blaze_world::PathKind)],
    /// Process heap for `Maker` tables (reclaimed when `Maker` drops).
    pub heap: &'static blaze_own::Heap,
}

/// Wasm64 proc-macro reactor shim compiled as crate 0 alongside the proc-macro crate (crate 1).
/// Mirrors `crates/blaze-proc-macro/guest/shim.rs` for `wasm64-blaze-wasip1` (`blaze_proc_macro_64`)
/// and overrides `wasi_rt::getenv` (via crate-0 export precedence in `collect_rust_c_exports`) so
/// every `std::env::var` call tracks individual env reads through `blaze_pm_env_var`.
const WASM64_PROC_MACRO_SHIM: &str = r#"#![allow(unsafe_code)]

use std::{ffi::CStr, mem::ManuallyDrop, panic};

unsafe extern "C" {
    #[link_name = "blaze_pm_dispatch"]
    fn dispatch(request: *const u8, len: usize) -> usize;
    #[link_name = "blaze_pm_reply"]
    fn reply(into: *mut u8);
    #[link_name = "blaze_pm_output"]
    fn output(bytes: *const u8, len: usize);
    #[link_name = "blaze_pm_env_var"]
    fn env_var(name: *const u8, len: usize, into: *mut u8, capacity: usize) -> isize;
    #[link_name = "blaze_pm_current_dir"]
    fn current_dir(into: *mut u8, capacity: usize) -> isize;
}

fn answer(ask: impl Fn(*mut u8, usize) -> isize) -> Option<Vec<u8>> {
    let mut bytes = Vec::<u8>::new();
    loop {
        let len = ask(bytes.as_mut_ptr(), bytes.capacity());
        let len = usize::try_from(len).ok()?;
        if len <= bytes.capacity() {
            unsafe { bytes.set_len(len) };
            return Some(bytes);
        }
        bytes.reserve_exact(len);
    }
}

fn enter_working_directory() -> Result<(), u32> {
    let Some(dir) = answer(|into, capacity| unsafe { current_dir(into, capacity) }) else {
        return Ok(());
    };
    let dir = String::from_utf8(dir).map_err(|_| 0u32)?;
    std::env::set_current_dir(dir)
        .map_err(|error| error.raw_os_error().map_or(0, |errno| errno as u32 & 0xFF_FFFF))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const std::ffi::c_char) -> *mut std::ffi::c_char {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let Some(mut value) =
        answer(|into, capacity| unsafe { env_var(name.as_ptr(), name.len(), into, capacity) })
    else {
        return std::ptr::null_mut();
    };
    value.push(0);
    Box::leak(value.into_boxed_slice()).as_mut_ptr().cast()
}

#[repr(C)]
struct Buffer {
    data: *mut u8,
    len: usize,
    capacity: usize,
    reserve: extern "C" fn(Buffer, usize) -> Buffer,
    drop: extern "C" fn(Buffer),
}

impl Buffer {
    fn of(bytes: Vec<u8>) -> Buffer {
        let mut bytes = ManuallyDrop::new(bytes);
        Buffer {
            data: bytes.as_mut_ptr(),
            len: bytes.len(),
            capacity: bytes.capacity(),
            reserve,
            drop: drop_buf,
        }
    }

    unsafe fn into_vec(self) -> Vec<u8> {
        let this = ManuallyDrop::new(self);
        unsafe { Vec::from_raw_parts(this.data, this.len, this.capacity) }
    }
}

extern "C" fn reserve(buffer: Buffer, additional: usize) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    bytes.reserve(additional);
    Buffer::of(bytes)
}

extern "C" fn drop_buf(buffer: Buffer) {
    std::mem::drop(unsafe { buffer.into_vec() });
}

#[repr(C)]
struct Closure {
    call: extern "C" fn(*mut u8, Buffer) -> Buffer,
    env: *mut u8,
}

#[repr(C)]
struct Config {
    input: Buffer,
    dispatch: Closure,
    force_show_panics: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Client {
    run: extern "C" fn(Config) -> Buffer,
}

extern "C" fn call(_env: *mut u8, buffer: Buffer) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    let len = unsafe { dispatch(bytes.as_ptr(), bytes.len()) };
    bytes.clear();
    bytes.reserve(len);
    unsafe {
        reply(bytes.as_mut_ptr());
        bytes.set_len(len);
    }
    Buffer::of(bytes)
}

fn panicked(message: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(10 + message.len());
    bytes.extend_from_slice(&[1, 0]);
    bytes.extend_from_slice(&(message.len() as u64).to_le_bytes());
    bytes.extend_from_slice(message.as_bytes());
    bytes
}

fn hook(info: &panic::PanicHookInfo<'_>) {
    let message = match (
        info.payload().downcast_ref::<&str>(),
        info.payload().downcast_ref::<String>(),
    ) {
        (Some(it), _) => *it,
        (_, Some(it)) => it.as_str(),
        (None, None) => "a payload that is not a string",
    };
    let bytes = panicked(message);
    unsafe { output(bytes.as_ptr(), bytes.len()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn _initialize() {}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_proc_macro_alloc(len: usize) -> *mut u8 {
    let mut bytes = ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    bytes.as_mut_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_proc_macro_expand(
    decls: *const &'static [Client],
    index: u32,
    input: *mut u8,
    len: usize,
) -> u32 {
    let input = unsafe { Vec::from_raw_parts(input, len, len) };
    let decls: &'static [Client] = unsafe { *decls };
    let Some(client) = decls.get(index as usize).copied() else {
        return 1;
    };
    if let Err(errno) = enter_working_directory() {
        return 2 | (errno << 8);
    }
    panic::set_hook(Box::new(hook));
    let config = Config {
        input: Buffer::of(input),
        dispatch: Closure { call, env: std::ptr::null_mut() },
        force_show_panics: true,
    };
    let out = unsafe { (client.run)(config).into_vec() };
    unsafe { output(out.as_ptr(), out.len()) };
    0
}
"#;

#[derive(Clone, Copy)]
struct SelfCompileGrant;

impl Grant for SelfCompileGrant {}

/// Maps fetched package records from `<store>/sources/<hex>` into a `Frame`.
struct PackDir<'f> {
    folder: PathBuf,
    frame: &'f Frame,
}

impl<'f> SourceRecords<'f> for PackDir<'f> {
    fn record(&mut self, source: &blaze_fetch::SourceId<'_>) -> Option<&'f [u8]> {
        let hex = crate::fetch::key_hex(&source.key());
        let hex_str = core::str::from_utf8(&hex).ok()?;
        let path = self.folder.join(hex_str);
        let mapping = blaze_core::Mapping::map_file(&path).ok()?;
        let held: &'f blaze_core::Mapping =
            blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(mapping, self.frame));
        Some(held.bytes())
    }
}

/// Serial `Par` whose macro-expansion scratch resets per call.
struct CompilePar {
    frame: RefCell<Option<Frame>>,
}

impl CompilePar {
    fn new() -> Self {
        Self { frame: RefCell::new(Frame::new(Tag::Expand).ok()) }
    }
}

impl Par for CompilePar {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn call_scratch(&self, work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        let Ok(mut slot) = self.frame.try_borrow_mut() else {
            return Scratch::Kept;
        };
        let Some(frame) = slot.as_mut() else {
            return Scratch::Kept;
        };
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    }
}

enum CrateOrigin<'a> {
    Virtual { root: &'a str, text: &'a str },
    Checkout { pkg_dir: PathBuf, prefix: &'a str },
    Pack { view: PackView<'a>, prefix: &'a str, dir: &'a str },
    Toolchain { view: PackView<'a>, prefix: &'a str },
}

struct Wasm64Sources<'a, 'p> {
    origins: AVec<'a, CrateOrigin<'a>>,
    envs: AVec<'a, Option<CrateEnv<'a>>>,
    editions: AVec<'a, Edition>,
    proc_keys: AVec<'a, (usize, CrateItemKey)>,
    procs: Option<&'p blaze_proc_macro::hook::ProcMacros<'p>>,
    arena: &'a Arena,
}

impl<'a> Wasm64Sources<'a, '_> {
    fn read_bytes_raw(&self, krate: usize, path: &str) -> Option<&'a [u8]> {
        let origin = self.origins.get(krate)?;
        match origin {
            CrateOrigin::Virtual { root, text } => (path == *root).then_some(text.as_bytes()),
            CrateOrigin::Checkout { pkg_dir, prefix } => {
                let rel = path.strip_prefix(*prefix)?;
                let full = pkg_dir.join(rel);
                let bytes = std::fs::read(&full).ok()?;
                Some(self.arena.local().alloc_slice_copy(&bytes))
            }
            CrateOrigin::Pack { view, prefix, dir } => {
                let rel = path.strip_prefix(*prefix)?;
                let lookup = if dir.is_empty() {
                    rel.to_owned()
                } else {
                    std::format!("{dir}/{rel}")
                };
                let entry = view.find_entry(&lookup)?;
                let local = self.arena.local();
                match entry.read_in(&local).ok()? {
                    FileBytes::Borrowed(bytes) => Some(bytes),
                    FileBytes::Owned(bytes) => Some(local.alloc_slice_copy(bytes.as_slice())),
                }
            }
            CrateOrigin::Toolchain { view, prefix } => {
                let rel = path.strip_prefix(*prefix)?;
                let entry = view.find_entry(rel)?;
                let local = self.arena.local();
                match entry.read_in(&local).ok()? {
                    FileBytes::Borrowed(bytes) => Some(bytes),
                    FileBytes::Owned(bytes) => Some(local.alloc_slice_copy(bytes.as_slice())),
                }
            }
        }
    }

    fn caller_package(&self, krate: usize) -> Arc<blaze_proc_macro::sandbox::Package> {
        let files = match self.origins.get(krate) {
            Some(CrateOrigin::Checkout { pkg_dir, .. }) => {
                blaze_proc_macro::sandbox::Files::Folder(pkg_dir.clone())
            }
            _ => blaze_proc_macro::sandbox::Files::Listed(Arc::from([])),
        };
        let mut pairs: std::vec::Vec<(std::string::String, std::string::String)> =
            std::vec::Vec::new();
        if let Some(Some(env)) = self.envs.get(krate) {
            for key in [
                "CARGO_PKG_NAME",
                "CARGO_PKG_VERSION",
                "CARGO_PKG_VERSION_MAJOR",
                "CARGO_PKG_VERSION_MINOR",
                "CARGO_PKG_VERSION_PATCH",
                "CARGO_PKG_VERSION_PRE",
                "CARGO_CRATE_NAME",
                "CARGO_MANIFEST_DIR",
                "OUT_DIR",
            ] {
                if let Ok(Some(val)) = env.get(key) {
                    pairs.push((key.to_owned(), val.to_owned()));
                }
            }
        }
        let env = blaze_proc_macro::package_env::PackageEnv::set_only(pairs)
            .unwrap_or_else(|_| {
                blaze_proc_macro::package_env::PackageEnv::set_only(std::vec::Vec::new())
                    .expect("empty PackageEnv")
            });
        Arc::new(blaze_proc_macro::sandbox::Package { files, env })
    }
}

impl<'a> SourceTexts<'a> for Wasm64Sources<'a, '_> {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        let bytes = self.read_bytes_raw(krate, path)?;
        core::str::from_utf8(bytes).ok()
    }

    fn read_data(&self, krate: usize, _from: &str, path: &str) -> Option<&'a [u8]> {
        self.read_bytes_raw(krate, path)
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'a>> {
        self.envs.get(krate).and_then(|e| e.as_ref())
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        self.proc_keys.iter().find(|&&(idx, _)| idx == krate).map(|&(_, key)| key)
    }

    fn run_proc_macro<'b>(
        &self,
        krate: usize,
        def: &blaze_expand::MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
        let Some(procs) = self.procs else {
            return Err(blaze_expand::HostError {
                ask: blaze_expand::HostAsk::ProcMacro,
                why: blaze_expand::HostWhy::Refused,
            });
        };
        let edition = self.editions.get(krate).copied().unwrap_or(Edition::E2021);
        let package = self.caller_package(krate);
        procs.run(def, &call, edition, &package, alloc)
    }
}

struct KeptRecords<'a> {
    names: OwnVec<&'a [u8], Alloc<'a>>,
    items: OwnVec<&'a [u8], Alloc<'a>>,
    files: OwnVec<blaze_own::BTreeMap<&'a str, &'a str, Alloc<'a>>, Alloc<'a>>,
    tc_packs: OwnVec<Option<PackView<'a>>, Alloc<'a>>,
    arena: &'a Arena,
    expansions: OwnVec<blaze_own::BTreeMap<LibKey, &'a str, Alloc<'a>>, Alloc<'a>>,
    origins: OwnVec<blaze_own::BTreeMap<LibKey, &'a [u8], Alloc<'a>>, Alloc<'a>>,
}

impl<'a> Load<'a> for KeptRecords<'a> {
    fn names(&self, krate: usize) -> Option<&'a [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'a [u8]> {
        self.items.get(krate).copied()
    }

    fn file(
        &self,
        krate: usize,
        path: blaze_scope::package_path::PackagePath<'_>,
    ) -> Option<&'a str> {
        if let Some(text) = self.files.get(krate).and_then(|m| m.get(&path.as_str()).copied()) {
            return Some(text);
        }
        let pack = self.tc_packs.get(krate).copied().flatten()?;
        let entry = pack.find_entry(path.as_str())?;
        let local = self.arena.local();
        let bytes = match entry.read_in(&local).ok()? {
            FileBytes::Borrowed(b) => b,
            FileBytes::Owned(b) => local.alloc_slice_copy(b.as_slice()),
        };
        core::str::from_utf8(bytes).ok()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'a>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = self.origins.get(krate).and_then(|m| m.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or((&[][..], &[][..]))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn syntax_edition(e: edition::PackageEdition) -> Edition {
    match e.syntax {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

fn tc_edition(written: &str) -> Result<Edition, std::string::String> {
    match written {
        "2015" => Ok(Edition::E2015),
        "2018" => Ok(Edition::E2018),
        "2021" => Ok(Edition::E2021),
        "2024" => Ok(Edition::E2024),
        other => Err(std::format!("unsupported toolchain edition `{other}`")),
    }
}

fn wasm64_builder(id: ToolchainId) -> Builder {
    let producer: ProducerId<CodeBuilder> =
        Producers::for_tests(b"blaze-cli wasm64 self-compile 1").get::<CodeBuilder>();
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Abort),
        toolchain: id,
        producer,
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn sorted_features_match(unit: &Unit<'_>, want_sorted: &[&str]) -> bool {
    if unit.features.len() != want_sorted.len() {
        return false;
    }
    let mut got: std::vec::Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    got.sort_unstable();
    got.as_slice() == want_sorted
}

/// Typed error returned by on-demand `wasm64-blaze-wasip1` self-compilation of build scripts and
/// proc-macro crates (rules 1, 3, 19). Distinguishes Tier-2 native/C-linking/host-spawn gaps from
/// ordinary Rust compilation or store errors — never falls back to `rustc`.
#[derive(Debug)]
pub(crate) enum SelfCompileError {
    /// The build script or proc-macro uses a Tier-2 feature unsupported in the `wasm64` sandbox
    /// (e.g., an external native C/OS FFI symbol outside `wasi_snapshot_preview1_64` /
    /// `blaze_seed` / `blaze_build_script_64` / `blaze_proc_macro_64`).
    Tier2Unsupported {
        module: std::string::String,
        symbol: std::string::String,
        detail: std::string::String,
    },
    /// Any non-Tier-2 compilation, resolution, linking, validation, or publication failure.
    Failed(std::string::String),
}

impl SelfCompileError {
    pub(crate) fn is_tier2(&self) -> bool {
        matches!(self, SelfCompileError::Tier2Unsupported { .. })
    }
}

impl core::fmt::Display for SelfCompileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SelfCompileError::Tier2Unsupported { module, symbol, detail } => {
                if symbol.is_empty() {
                    write!(
                        f,
                        "Tier-2 unsupported native/host feature in `{module}` ({detail}; no rustc fallback, rules 1 & 3)"
                    )
                } else {
                    write!(
                        f,
                        "Tier-2 unsupported native C/FFI symbol `{symbol}` in `{module}` ({detail}; not provided by wasm64 sandbox and no rustc fallback, rules 1 & 3)"
                    )
                }
            }
            SelfCompileError::Failed(msg) => f.write_str(msg),
        }
    }
}

impl From<std::string::String> for SelfCompileError {
    fn from(value: std::string::String) -> Self {
        SelfCompileError::Failed(value)
    }
}

fn map_wasm64_build_error(module_name: &str, mode: CompileMode, err: Wasm64BuildError) -> SelfCompileError {
    let kind = match mode {
        CompileMode::BuildScript => "build-script",
        CompileMode::ProcMacro => "proc-macro",
    };
    match err {
        Wasm64BuildError::UnresolvedImport { name, name_len, what, .. } => {
            let sym = core::str::from_utf8(&name[..usize::from(name_len)]).unwrap_or("").to_owned();
            SelfCompileError::Tier2Unsupported {
                module: module_name.to_owned(),
                symbol: sym,
                detail: std::format!("wasm64 {kind} link: {what}"),
            }
        }
        other => SelfCompileError::Failed(std::format!("wasm64 {kind} compile failed: {other}")),
    }
}

/// Ensures the build-script unit `package version features` has a compiled `wasm64` module in
/// `<store>/build-script/modules/` and returns its `wasm-sha256` hex digest.
pub(crate) fn compile_build_script(
    store: &Path,
    inputs: CompileInputs<'_>,
    package: &str,
    version: &str,
    features: &[&str],
) -> Result<std::string::String, SelfCompileError> {
    let Some(checkout) = inputs.checkout else {
        return Err(SelfCompileError::Failed(std::format!(
            "cannot self-compile build script {package} {version}: no on-disk checkout"
        )));
    };
    let frame = Frame::with_reserve(COMPILE_FRAME_RESERVE, Tag::Other)
        .map_err(|_| SelfCompileError::Failed(std::string::String::from("failed to reserve wasm64 compile frame")))?;
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let decoded = blaze_unit_graph::record::decode(inputs.graph, alloc)
        .map_err(|e| SelfCompileError::Failed(std::format!("unit graph decode failed at byte {}: {}", e.at, e.why)))?;
    let lock = Lock::read(inputs.lock, alloc)
        .map_err(|e| SelfCompileError::Failed(std::format!("Cargo.lock parse failed: {e}")))?;
    let mut want_sorted: std::vec::Vec<&str> = features.to_vec();
    want_sorted.sort_unstable();
    let Some(unit_idx) = decoded.graph.units.iter().position(|u| {
        u.kind == UnitKind::BuildScript
            && u.name.as_str() == package
            && u.version.as_str() == version
            && sorted_features_match(u, &want_sorted)
    }) else {
        return Err(SelfCompileError::Failed(std::format!(
            "unit graph has no BuildScript unit for {package} {version} with features [{}]",
            want_sorted.join(",")
        )));
    };
    let script_id = UnitId(unit_idx as u32);
    ensure_build_script_unit(store, &decoded.graph, &lock, checkout, inputs.members, inputs.heap, script_id, 0)
}

fn path_pkg_files_digest(
    checkout: &blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    pkg_id: LockId,
) -> Option<std::string::String> {
    let (_, dir, _) = members.iter().find(|(id, _, _)| *id == pkg_id)?;
    let pkg_dir = if dir.is_empty() {
        checkout.path().to_path_buf()
    } else {
        checkout.path().join(dir)
    };
    let mut files = blaze_proc_macro::module::package_files(&pkg_dir).ok()?;
    Some(blaze_proc_macro::module::files_digest(&mut files))
}

/// Recursively compiles `script_id` (and any build-script dependencies of its build-dependencies)
/// into `<store>/build-script/modules/<key>` and returns its `wasm-sha256` digest.
#[expect(clippy::too_many_arguments, reason = "recursive build-script compilation over store, graph, lock, checkout, members, heap, unit, and depth")]
fn ensure_build_script_unit(
    store: &Path,
    graph: &UnitGraph<'_>,
    lock: &Lock<'_>,
    checkout: &blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    heap: &'static blaze_own::Heap,
    script_id: UnitId,
    depth: u8,
) -> Result<std::string::String, SelfCompileError> {
    if depth > 8 {
        return Err(SelfCompileError::Failed(std::string::String::from(
            "build-script self-compilation recursion limit exceeded",
        )));
    }
    let unit = &graph.units[script_id.index()];
    let mut feats: std::vec::Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    feats.sort_unstable();
    let files_digest = members
        .iter()
        .find(|(id, _, _)| *id == unit.package)
        .map(|(_, dir, _)| {
            let pkg_dir = if dir.is_empty() {
                checkout.path().to_path_buf()
            } else {
                checkout.path().join(dir)
            };
            script_unit::script_files_digest(&pkg_dir, unit.root.as_str()).map_err(|e| {
                SelfCompileError::Failed(std::format!("reading build script files of {}: {e}", unit.name.as_str()))
            })
        })
        .transpose()?;
    let deps_digest = script_unit::script_deps_digest(graph, lock, script_id, |dep_pkg| {
        path_pkg_files_digest(checkout, members, dep_pkg)
    });

    // Fast path: already compiled and cached on disk.
    if let Ok(Some(record_dir)) = script_unit::module_of_files_and_deps(
        store,
        unit.name.as_str(),
        unit.version.as_str(),
        &feats,
        files_digest.as_deref(),
        deps_digest.as_deref(),
    ) && let Ok(digest) = std::fs::read_to_string(record_dir.join("wasm-sha256"))
    {
        return Ok(digest.trim().to_owned());
    }

    let wasm = compile_unit_to_wasm64(
        store,
        graph,
        lock,
        checkout,
        members,
        heap,
        script_id,
        CompileMode::BuildScript,
        depth,
    )?
    .wasm;

    let id = blaze_toolchain::PINNED;
    let mut identity = std::format!(
        "blaze build-script module 3\nname {}\nversion {}\nfeatures {}\n",
        unit.name.as_str(),
        unit.version.as_str(),
        feats.join(","),
    );
    if let Some(hex) = &files_digest {
        let _ = writeln!(identity, "files {hex}");
    }
    if let Some(hex) = &deps_digest {
        let _ = writeln!(identity, "deps {hex}");
    }
    let _ = writeln!(
        identity,
        "toolchain {id}\nwasm-target {WASM64_TARGET}\n{}\nbuilder blaze-wasm64",
        script_unit::shim_line()
    );
    let key = sha256_hex(identity.as_bytes());
    let wasm_sha256 = sha256_hex(&wasm);
    publish_build_script_module(store, &key, &identity, &wasm, &wasm_sha256)?;
    Ok(wasm_sha256)
}

/// Ensures a dependency proc-macro unit (e.g. `rustversion` used by `encoding_rs/build.rs`) is
/// compiled to `wasm64` and published in `<store>/proc-macro/modules/<key>`, returning its runner
/// crate key and loaded `ModuleRecord`.
#[expect(clippy::too_many_arguments, reason = "mirrors compile_unit_to_wasm64 inputs")]
fn ensure_proc_macro_unit(
    store: &Path,
    graph: &UnitGraph<'_>,
    lock: &Lock<'_>,
    checkout: &blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    heap: &'static blaze_own::Heap,
    pm_id: UnitId,
    depth: u8,
) -> Result<(CrateItemKey, blaze_proc_macro::module::ModuleRecord, std::string::String, std::string::String), SelfCompileError> {
    if depth > 8 {
        return Err(SelfCompileError::Failed(std::string::String::from(
            "proc-macro dependency self-compilation recursion limit exceeded",
        )));
    }
    let unit = &graph.units[pm_id.index()];
    let locked = blaze_proc_macro::module::LockedPackages::of(lock);
    let folders = blaze_world::procs::git_folders(store)
        .map_err(|e| std::format!("listing proc-macro store git folders: {e}"))?;
    let identity = blaze_world::procs::identify(lock, &locked, &folders, unit, |pkg_id| {
        let Some((_, dir, _)) = members.iter().find(|(id, _, _)| *id == pkg_id) else {
            return Ok(None);
        };
        let pkg_dir = if dir.is_empty() {
            checkout.path().to_path_buf()
        } else {
            checkout.path().join(dir)
        };
        let mut files = blaze_proc_macro::module::package_files(&pkg_dir)
            .map_err(|e| blaze_world::procs::ProcsError::Files { at: pkg_dir, error: e })?;
        Ok(Some(blaze_proc_macro::module::files_digest(&mut files)))
    })
    .map_err(|e| std::format!("identifying proc-macro dep {} {}: {e}", unit.name.as_str(), unit.version.as_str()))?
    .map_err(|e| std::format!("proc-macro dep {} {} not identified: {e}", unit.name.as_str(), unit.version.as_str()))?;

    let (mod_key, id_text, existing) = identity.with_id(|id| {
        (
            blaze_proc_macro::module::module_key(id),
            blaze_proc_macro::module::identity(id),
            blaze_proc_macro::module::read_module(store, id),
        )
    });
    let pm_key = blaze_proc_macro::hook::crate_key(&mod_key);
    let record = match existing {
        Ok(rec) => rec,
        Err(blaze_proc_macro::module::ModuleError::Missing { .. }) => {
            let built = compile_unit_to_wasm64(
                store,
                graph,
                lock,
                checkout,
                members,
                heap,
                pm_id,
                CompileMode::ProcMacro,
                depth,
            )?;
            let wasm_sha256 = sha256_hex(&built.wasm);
            publish_proc_macro_module(
                store,
                &mod_key,
                &id_text,
                &built.wasm,
                &wasm_sha256,
                &built.macros,
                PROC_MACRO_DECLS_GLOBAL,
            )?;
            identity
                .with_id(|id| blaze_proc_macro::module::read_module(store, id))
                .map_err(|e| {
                    std::format!(
                        "reading newly published proc-macro dep {} {}: {e}",
                        identity.package, identity.version
                    )
                })?
        }
        Err(e) => {
            return Err(SelfCompileError::Failed(std::format!(
                "reading proc-macro dep {} {}: {e}",
                identity.package, identity.version
            )));
        }
    };
    Ok((pm_key, record, identity.package, identity.version))
}

/// Compiles the proc-macro crate named by `identity` to `wasm64-blaze-wasip1`, atomically
/// publishes `<store>/proc-macro/modules/<key>`, and returns its `wasm-sha256` digest.
pub(crate) fn compile_proc_macro(
    store: &Path,
    inputs: CompileInputs<'_>,
    identity: &blaze_world::procs::MacroIdentity,
) -> Result<std::string::String, SelfCompileError> {
    let Some(checkout) = inputs.checkout else {
        return Err(SelfCompileError::Failed(std::format!(
            "cannot self-compile proc-macro {} {}: no on-disk checkout",
            identity.package, identity.version
        )));
    };
    let frame = Frame::with_reserve(COMPILE_FRAME_RESERVE, Tag::Other)
        .map_err(|_| SelfCompileError::Failed(std::string::String::from("failed to reserve wasm64 compile frame")))?;
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let decoded = blaze_unit_graph::record::decode(inputs.graph, alloc)
        .map_err(|e| SelfCompileError::Failed(std::format!("unit graph decode failed at byte {}: {}", e.at, e.why)))?;
    let lock = Lock::read(inputs.lock, alloc)
        .map_err(|e| SelfCompileError::Failed(std::format!("Cargo.lock parse failed: {e}")))?;
    let mut want_sorted: std::vec::Vec<std::string::String> =
        identity.with_id(|id| id.features.iter().map(|s| (*s).to_owned()).collect());
    want_sorted.sort_unstable();
    let want_refs: std::vec::Vec<&str> = want_sorted.iter().map(std::string::String::as_str).collect();
    let Some(unit_idx) = decoded.graph.units.iter().position(|u| {
        u.kind == UnitKind::ProcMacro
            && u.name.as_str() == identity.package.as_str()
            && u.version.as_str() == identity.version.as_str()
            && sorted_features_match(u, &want_refs)
    }) else {
        return Err(SelfCompileError::Failed(std::format!(
            "unit graph has no ProcMacro unit for {} {} with features [{}]",
            identity.package,
            identity.version,
            want_refs.join(",")
        )));
    };
    let pm_id = UnitId(unit_idx as u32);
    let built = compile_unit_to_wasm64(
        store,
        &decoded.graph,
        &lock,
        checkout,
        inputs.members,
        inputs.heap,
        pm_id,
        CompileMode::ProcMacro,
        0,
    )?;

    let (key, id_text) = identity.with_id(|id| {
        (blaze_proc_macro::module::module_key(id), blaze_proc_macro::module::identity(id))
    });
    let wasm_sha256 = sha256_hex(&built.wasm);
    publish_proc_macro_module(
        store,
        &key,
        &id_text,
        &built.wasm,
        &wasm_sha256,
        &built.macros,
        PROC_MACRO_DECLS_GLOBAL,
    )?;
    Ok(wasm_sha256)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompileMode {
    BuildScript,
    ProcMacro,
}

struct CompiledWasm64 {
    wasm: std::vec::Vec<u8>,
    macros: std::vec::Vec<Exported>,
}

/// Runs a dependency crate's build script on `wasm64-blaze-wasip1` so crates like `proc-macro2`
/// under `serde_derive` receive their exact `rustc-cfg` atoms and `OUT_DIR` files during compilation.
#[expect(clippy::too_many_arguments, reason = "dependency build-script execution on wasm64")]
fn run_dep_build_script(
    store: &Path,
    graph: &UnitGraph<'_>,
    lock: &Lock<'_>,
    checkout: &blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    heap: &'static blaze_own::Heap,
    bs_id: UnitId,
    tc_cfg: &[(std::string::String, Option<std::string::String>)],
    tc_names: &[std::string::String],
    depth: u8,
) -> Result<std::vec::Vec<u8>, SelfCompileError> {
    // Ensure the build script's own wasm64 module exists first.
    let _ = ensure_build_script_unit(store, graph, lock, checkout, members, heap, bs_id, depth + 1)?;
    let frame = Frame::with_reserve(128 << 20, Tag::Other)
        .map_err(|_| SelfCompileError::Failed(std::string::String::from("failed to reserve dep build-script frame")))?;
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let unit = &graph.units[bs_id.index()];

    let (tree, root) = if let Some((_, dir, _)) = members.iter().find(|(id, _, _)| *id == unit.package)
    {
        let sr = ScriptRoot::member(dir)
            .map_err(|e| std::format!("member root for {}: {e}", unit.name.as_str()))?;
        (dir_tree(checkout), sr)
    } else {
        let pack_dir = PackDir { folder: store.join("sources"), frame: &frame };
        let mut sources = StoreSources::new(
            lock,
            pack_dir,
            PackView::open_source_put,
            alloc,
        )
        .map_err(|e| std::format!("opening fetched sources for dep build script: {e}"))?;
        let (view, dir) = sources
            .files(unit.package)
            .map_err(|e| std::format!("fetched files for {}: {e}", unit.name.as_str()))?;
        let git = match &lock.packages[unit.package.index()].source {
            LockSource::Git { commit, .. } => {
                blaze_build_script::git::Oid::parse_hex(commit.as_bytes())
            }
            _ => None,
        };
        let mut decoded: OwnVec<(&str, FileBytes<'_, &Frame>), &Frame> =
            OwnVec::with_capacity_in(view.len(), &frame);
        let mut submodules: OwnVec<(&str, [u8; 20]), &Frame> = OwnVec::new_in(&frame);
        for entry in view.entries() {
            if entry.mode == blaze_fetch::Mode::Submodule {
                if let Some(commit) = entry.submodule_commit() {
                    submodules.push((entry.path, commit));
                }
                continue;
            }
            let bytes = entry
                .read_in(&frame)
                .map_err(|e| std::format!("decode {} in {}: {}", entry.path, unit.name.as_str(), blaze_text::Shown(&e)))?;
            decoded.push((entry.path, bytes));
        }
        (
            mem_tree_with_submodules(
                decoded.iter().map(|(p, b)| (*p, b.as_slice())),
                submodules.iter().copied(),
                dir.as_str(),
                git,
            ),
            ScriptRoot::package(),
        )
    };

    let rt = ScriptRuntime::new(store).map_err(|e| std::format!("ScriptRuntime init: {e}"))?;
    let shipped = tc_names.iter().map(|s| s.as_str());
    let rows = blaze_world::script_setting::pinned_table(WASM64_TARGET, shipped, alloc);
    let mut table: std::vec::Vec<blaze_build_script::probe::Row> = rows.as_slice().to_vec();
    table.sort_unstable_by_key(|r| r.key);
    let files_digest = members
        .iter()
        .find(|(id, _, _)| *id == unit.package)
        .and_then(|(_, dir, _)| {
            let pkg_dir = if dir.is_empty() {
                checkout.path().to_path_buf()
            } else {
                checkout.path().join(dir)
            };
            script_unit::script_files_digest(&pkg_dir, unit.root.as_str()).ok()
        });
    let deps_digest = script_unit::script_deps_digest(graph, lock, bs_id, |dep_pkg| {
        path_pkg_files_digest(checkout, members, dep_pkg)
    });
    let setting = Setting {
        store,
        target: WASM64_TARGET,
        host: crate::graph::TRIPLE,
        table: Arc::new(table),
        probes: None,
        c_probes: None,
        c_mode: CMode::Deferred,
        mode: Mode::Check,
        native: None,
        spawn_setting: Arc::new(SpawnSetting::off()),
        files_digest: files_digest.as_deref(),
        deps_digest: deps_digest.as_deref(),
    };
    let mut side_cfg: AVec<'_, CfgAtom<'_>> = OwnVec::with_capacity_in(tc_cfg.len(), alloc);
    for (k, v) in tc_cfg {
        side_cfg.push(match v {
            Some(val) => CfgAtom::pair(k.as_str(), val.as_str(), alloc),
            None => CfgAtom::flag(k.as_str(), alloc),
        });
    }
    let prepared = script_unit::prepare_viewing_on(
        &rt,
        &setting,
        graph,
        bs_id,
        Some(side_cfg.as_slice()),
        tree,
        root,
        &[],
        blaze_proc_macro::script_sandbox::dep_mount::DepViews::none(),
        alloc,
    )
    .map_err(|e| std::format!("prepare dep build script {}: {e}", unit.name.as_str()))?;
    let ran = rt
        .run(&prepared.loaded, prepared.script, &prepared.env, alloc)
        .map_err(|e| std::format!("run dep build script {}: {e}", unit.name.as_str()))?;
    let mut encoded: AVec<'_, u8> = OwnVec::new_in(alloc);
    ran.outputs.encode(&mut encoded);
    Ok(encoded.as_slice().to_vec())
}

#[expect(
    clippy::too_many_arguments,
    reason = "whole-program wasm64 compilation over store, graph, lock, checkout, members, root unit, mode, and recursion depth"
)]
fn compile_unit_to_wasm64(
    store: &Path,
    graph: &UnitGraph<'_>,
    lock: &Lock<'_>,
    checkout: &blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    heap: &'static blaze_own::Heap,
    root_unit: UnitId,
    mode: CompileMode,
    depth: u8,
) -> Result<CompiledWasm64, SelfCompileError> {
    // Collect `root_unit` + transitive `UnitGraph` dependency closure in deterministic sorted
    // order (Rule 25: independent of workspace unit-index ordering) up front so any dependency
    // proc-macro runtime outlives `compile_unit_in`'s `Frame` (zero leaks).
    let mut unit_order: std::vec::Vec<UnitId> = std::vec::Vec::new();
    let mut unit_to_pos: HashMap<UnitId, usize> = HashMap::new();
    let mut stack = std::vec![root_unit];
    while let Some(uid) = stack.pop() {
        if unit_to_pos.contains_key(&uid) {
            continue;
        }
        let pos = unit_order.len();
        unit_to_pos.insert(uid, pos);
        unit_order.push(uid);
        let u = &graph.units[uid.index()];
        let mut child_deps: std::vec::Vec<(&str, &str, &str, &str, u8, UnitId)> = std::vec::Vec::new();
        for dep in u.deps.iter() {
            let du = &graph.units[dep.unit.index()];
            if matches!(du.kind, UnitKind::Lib | UnitKind::ProcMacro) {
                let kind_ord = match du.kind {
                    UnitKind::Lib => 0u8,
                    UnitKind::ProcMacro => 1u8,
                    _ => 2u8,
                };
                child_deps.push((
                    dep.name.as_str(),
                    du.name.as_str(),
                    du.version.as_str(),
                    du.crate_name.as_str(),
                    kind_ord,
                    dep.unit,
                ));
            }
        }
        child_deps.sort_unstable_by(|a, b| (a.0, a.1, a.2, a.3, a.4).cmp(&(b.0, b.1, b.2, b.3, b.4)));
        for (_, _, _, _, _, dep_unit) in child_deps.into_iter().rev() {
            stack.push(dep_unit);
        }
    }

    // Ensure any dependency proc-macro crates (e.g. `rustversion` used by `encoding_rs/build.rs`)
    // are compiled to `wasm64` and loaded into a per-compile `ProcMacros` runner.
    let mut dep_pm_records: std::vec::Vec<(
        UnitId,
        CrateItemKey,
        blaze_proc_macro::module::ModuleRecord,
        std::string::String,
        std::string::String,
    )> = std::vec::Vec::new();
    for &uid in &unit_order {
        if uid == root_unit {
            continue;
        }
        if graph.units[uid.index()].kind == UnitKind::ProcMacro {
            let (pm_key, record, pkg, ver) = ensure_proc_macro_unit(
                store,
                graph,
                lock,
                checkout,
                members,
                heap,
                uid,
                depth + 1,
            )?;
            dep_pm_records.push((uid, pm_key, record, pkg, ver));
        }
    }
    let pm_rt = if dep_pm_records.is_empty() {
        None
    } else {
        Some(
            blaze_proc_macro::host::Runtime::new(heap)
                .map_err(|e| std::format!("creating wasm64 dep proc-macro runtime: {e}"))?,
        )
    };
    let pm_dir_store = pm_rt.as_ref().map(|_| {
        blaze_proc_macro::record::DirStore::new(&store.join(crate::proc_runner::EXPANSIONS))
    });
    let mut pm_hook = match (pm_rt.as_ref(), pm_dir_store.as_ref()) {
        (Some(rt), Some(ds)) => Some(blaze_proc_macro::hook::ProcMacros::new(
            rt,
            ds,
            blaze_proc_macro::host::Limits::STANDARD,
        )),
        _ => None,
    };
    let mut dep_pm_keys_by_unit: HashMap<UnitId, CrateItemKey> = HashMap::new();
    if let Some(ref mut hook) = pm_hook {
        for (uid, pm_key, record, pkg, ver) in dep_pm_records {
            hook.add_crate(pm_key, &pkg, &ver, Ok(record));
            dep_pm_keys_by_unit.insert(uid, pm_key);
        }
    }

    let frame = Frame::with_reserve(COMPILE_FRAME_RESERVE, Tag::Other)
        .map_err(|_| SelfCompileError::Failed(std::string::String::from("failed to reserve wasm64 compile frame")))?;
    let infer_frame = Frame::with_reserve(COMPILE_FRAME_RESERVE, Tag::Infer)
        .map_err(|_| SelfCompileError::Failed(std::string::String::from("failed to reserve wasm64 infer frame")))?;
    let arena = Arena::new(Tag::Other);
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let result = compile_unit_in(
        store,
        graph,
        lock,
        checkout,
        members,
        heap,
        root_unit,
        &unit_order,
        &unit_to_pos,
        pm_hook.as_ref(),
        &dep_pm_keys_by_unit,
        mode,
        depth,
        &frame,
        &infer_frame,
        &arena,
        bump,
    )?;
    if let Some(ref hook) = pm_hook {
        let _ = hook.keep_all();
    }
    Ok(result)
}

#[expect(
    clippy::too_many_arguments,
    reason = "whole-program wasm64 compilation over arena-backed frames"
)]
fn compile_unit_in<'a>(
    store: &Path,
    graph: &'a UnitGraph<'a>,
    lock: &'a Lock<'a>,
    checkout: &'a blaze_proc_macro::script_sandbox::RealRoot,
    members: &[(LockId, &str, blaze_world::PathKind)],
    heap: &'static blaze_own::Heap,
    root_unit: UnitId,
    unit_order: &[UnitId],
    unit_to_pos: &HashMap<UnitId, usize>,
    pm_hook: Option<&'a blaze_proc_macro::hook::ProcMacros<'a>>,
    dep_pm_keys_by_unit: &HashMap<UnitId, CrateItemKey>,
    mode: CompileMode,
    depth: u8,
    frame: &'a Frame,
    infer_frame: &'a Frame,
    arena: &'a Arena,
    bump: Bump<'a>,
) -> Result<CompiledWasm64, SelfCompileError> {
    let alloc = bump.alloc();

    // 1. Open pinned `wasm64-blaze-wasip1` toolchain from `<store>/toolchain`.
    let tc_id = blaze_toolchain::PINNED;
    let shelf_path = store.join("toolchain");
    let mut shelf = DirShelf::new(shelf_path.to_string_lossy().into_owned());
    let manifest_file = blaze_toolchain::shelf::open_manifest(&mut shelf, tc_id)
        .map_err(|e| std::format!("open toolchain manifest {tc_id}: {e}"))?;
    let manifest_bytes: &[u8] = bump.alloc_slice_copy(manifest_file.as_ref());
    let tc_view = ToolchainView::open(manifest_bytes, tc_id)
        .map_err(|e| std::format!("invalid toolchain manifest {tc_id}: {e}"))?;
    let sys = blaze_toolchain::sysroot(&tc_view, WASM64_TARGET, false)
        .ok_or_else(|| std::format!("pinned toolchain {tc_id} lacks target `{WASM64_TARGET}`"))?;
    let all_tc: std::vec::Vec<_> = tc_view
        .units(WASM64_TARGET)
        .ok_or_else(|| std::format!("toolchain {tc_id} has no units for `{WASM64_TARGET}`"))?
        .collect();

    let mut keep_tc = std::vec![false; all_tc.len()];
    let mut tc_todo = std::vec![
        sys.core.0 as usize,
        sys.alloc.0 as usize,
        sys.std.0 as usize,
        sys.proc_macro.0 as usize,
        sys.bstd.0 as usize,
    ];
    while let Some(u) = tc_todo.pop() {
        if keep_tc[u] {
            continue;
        }
        keep_tc[u] = true;
        if all_tc[u].kind != TcUnitKind::ProcMacro {
            tc_todo.extend(all_tc[u].deps().map(|(_, d)| d as usize));
        }
    }

    let tc_cfg_owned: std::vec::Vec<(std::string::String, Option<std::string::String>)> = tc_view
        .cfg(WASM64_TARGET)
        .ok_or_else(|| std::format!("toolchain {tc_id} has no cfg for `{WASM64_TARGET}`"))?
        .map(|(n, v)| (n.to_owned(), v.map(str::to_owned)))
        .collect();

    struct TcUnit<'a> {
        name: &'a str,
        edition: Edition,
        root: &'a str,
        prefix: &'a str,
        features: &'a [&'a str],
        raw_deps: std::vec::Vec<(&'a str, usize)>,
        pack: PackView<'a>,
        key: CrateItemKey,
        role: Role,
        proc_macro: bool,
    }

    let mut tc_at = std::vec![usize::MAX; all_tc.len()];
    let mut tc_units: std::vec::Vec<TcUnit<'_>> = std::vec::Vec::new();
    let mut tc_names: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    for (u, row) in all_tc.iter().enumerate() {
        let is_pm = row.kind == TcUnitKind::ProcMacro;
        if !keep_tc[u] || !(is_pm || (row.kind == TcUnitKind::Lib && !row.for_host)) {
            continue;
        }
        let krate = tc_view.krate(row.krate);
        let ed = tc_edition(krate.edition)?;
        let shelf_file = blaze_toolchain::shelf::open_pack(&mut shelf, tc_id, &krate)
            .map_err(|e| std::format!("open toolchain pack `{}`: {e}", krate.name))?;
        let held_file: &'a blaze_ladder::crates::ShelfFile =
            blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(shelf_file, frame));
        let pack = PackView::open_verified(held_file.as_ref())
            .map_err(|_| std::format!("invalid toolchain pack `{}`", krate.name))?;
        let prefix = bump.alloc_str(&std::format!("/toolchain/{u}/"));
        let root_str = if krate.dir.is_empty() || row.root.starts_with(krate.dir) {
            std::format!("{prefix}{}", row.root)
        } else {
            std::format!("{prefix}{}/{}", krate.dir, row.root)
        };
        let root = bump.alloc_str(&root_str);
        let mut feats: OwnVec<&str, Bump<'_>> = OwnVec::new_in(bump);
        for f in row.features() {
            feats.push(bump.alloc_str(f));
        }
        let mut key_bytes = krate.pack.to_vec();
        for f in row.features() {
            key_bytes.extend_from_slice(f.as_bytes());
            key_bytes.push(0);
        }
        let key = CrateItemKey(ContentId::of("blaze wasm64 tc unit", &key_bytes));
        tc_at[u] = tc_units.len();
        tc_names.push(krate.lib.to_owned());
        tc_units.push(TcUnit {
            name: bump.alloc_str(krate.lib),
            edition: ed,
            root,
            prefix,
            features: feats.into_bump_slice(),
            raw_deps: if is_pm {
                std::vec::Vec::new()
            } else {
                row.deps().map(|(n, d)| (&*bump.alloc_str(n), d as usize)).collect()
            },
            pack,
            key,
            role: krate.role,
            proc_macro: is_pm,
        });
    }

    // 2. Run build scripts for crates in `unit_order` on `wasm64-blaze-wasip1` (e.g., `proc-macro2`
    //    under `serde_derive`, or `rustversion`'s own `build.rs` when compiling `rustversion`) so
    //    their `rustc-cfg` atoms and `OUT_DIR` are joined into each CrateInput.
    let mut dep_script_bytes: HashMap<UnitId, &[u8]> = HashMap::new();
    for &uid in unit_order {
        if uid == root_unit && mode == CompileMode::BuildScript {
            continue;
        }
        if let Some(bs_id) = graph.units[uid.index()].build_script {
            let encoded = run_dep_build_script(
                store,
                graph,
                lock,
                checkout,
                members,
                heap,
                bs_id,
                &tc_cfg_owned,
                &tc_names,
                depth,
            )?;
            let slice: &[u8] = bump.alloc_slice_copy(&encoded);
            dep_script_bytes.insert(uid, slice);
        }
    }

    // 4. Open fetched package packs for non-member crates in `unit_order`.
    let pack_dir = PackDir { folder: store.join("sources"), frame };
    let mut store_sources =
        StoreSources::new(lock, pack_dir, PackView::open_source_put, alloc)
            .map_err(|e| std::format!("opening fetched sources store: {e}"))?;

    // Layout of `CrateInput`s:
    // - BuildScript mode: crate 0 = root_unit, crates 1..unit_order.len() = deps, then tc_units.
    // - ProcMacro mode: crate 0 = root_unit (so `BucketIndex` links crate 0 even with `proc_macro=true`!),
    //   crates 1..unit_order.len() = deps, crate `shim_idx = unit_order.len()` = WASM64_PROC_MACRO_SHIM
    //   (strictly before `tc_base` so shim's `getenv` overrides `wasi_rt::getenv`), then tc_units.
    let unit_base: usize = 0;
    let shim_idx: Option<usize> = (mode == CompileMode::ProcMacro).then_some(unit_order.len());
    let tc_base: usize = unit_order.len() + usize::from(mode == CompileMode::ProcMacro);
    let total_crates = tc_base + tc_units.len();

    let mut crates: AVec<'_, CrateInput<'_>> = OwnVec::with_capacity_in(total_crates, alloc);
    let mut origins: AVec<'_, CrateOrigin<'_>> = OwnVec::with_capacity_in(total_crates, alloc);
    let mut envs: AVec<'_, Option<CrateEnv<'_>>> = OwnVec::with_capacity_in(total_crates, alloc);
    let mut editions: AVec<'_, Edition> = OwnVec::with_capacity_in(total_crates, alloc);
    let mut proc_keys: AVec<'_, (usize, CrateItemKey)> = OwnVec::new_in(alloc);

    // Base wasm64 cfg atoms slice (with `debug_assertions` matching dev profile).
    let mut base_cfg: AVec<'_, (&str, Option<&str>)> =
        OwnVec::with_capacity_in(tc_cfg_owned.len() + 2, alloc);
    for (n, v) in &tc_cfg_owned {
        base_cfg.push((&*bump.alloc_str(n), v.as_deref().map(|s| &*bump.alloc_str(s))));
    }
    base_cfg.push(("debug_assertions", None));

    let pm_tc_crate_idx = tc_units
        .iter()
        .position(|u| u.name == "proc_macro")
        .map(|i| tc_base + i);

    // UnitGraph crates (`root_unit` at index 0 and its transitive dependencies).
    for (pos, &uid) in unit_order.iter().enumerate() {
        let u = &graph.units[uid.index()];
        let krate_idx = unit_base + pos;
        let is_root = uid == root_unit;
        let (origin, prefix_str, seen_root) =
            if let Some((_, dir, _)) = members.iter().find(|(id, _, _)| *id == u.package) {
                let pfx: &'a str = if dir.is_empty() {
                    bump.alloc_str("/checkout/")
                } else {
                    bump.alloc_str(&std::format!("/checkout/{dir}/"))
                };
                let seen = if dir.is_empty() {
                    SeenRoot::Member { dir: "" }
                } else {
                    SeenRoot::Member { dir: bump.alloc_str(dir) }
                };
                let pkg_dir = if dir.is_empty() {
                    checkout.path().to_path_buf()
                } else {
                    checkout.path().join(dir)
                };
                (CrateOrigin::Checkout { pkg_dir, prefix: pfx }, pfx, seen)
            } else {
                let (view, dir) = store_sources.files(u.package).map_err(|e| {
                    std::format!("fetched sources missing for {} {}: {e}", u.name.as_str(), u.version.as_str())
                })?;
                let pfx: &'a str = bump.alloc_str(&std::format!("/pack/{krate_idx}/"));
                let dir_s: &'a str = bump.alloc_str(dir.as_str());
                (CrateOrigin::Pack { view, prefix: pfx, dir: dir_s }, pfx, SeenRoot::Package)
            };

        let root_path = bump.alloc_str(&std::format!("{prefix_str}{}", u.root.as_str()));
        let mut feats: OwnVec<&str, Bump<'_>> = OwnVec::with_capacity_in(u.features.len(), bump);
        for f in u.features.iter() {
            feats.push(&*bump.alloc_str(f.as_str()));
        }
        let feats_slice: &[&str] = feats.into_bump_slice();

        let script_ref: Option<OutputsRef<'_>> = dep_script_bytes.get(&uid).and_then(|bytes| {
            OutputsRef::open(bytes).ok()
        });

        let mut cfg: OwnVec<(&str, Option<&str>), Bump<'_>> =
            OwnVec::with_capacity_in(base_cfg.len() + feats_slice.len() + 8, bump);
        cfg.extend_from_slice(base_cfg.as_slice());
        for &f in feats_slice {
            cfg.push(("feature", Some(f)));
        }
        if let Some(ref outputs) = script_ref {
            for (k, v) in outputs.cfgs() {
                cfg.push((&*bump.alloc_str(k), v.map(|s| &*bump.alloc_str(s))));
            }
        }
        cfg.sort_unstable();
        cfg.dedup();

        let mut deps: OwnVec<(&str, usize), Bump<'_>> = OwnVec::with_capacity_in(u.deps.len() + 1, bump);
        for dep in u.deps.iter() {
            if let Some(&dep_pos) = unit_to_pos.get(&dep.unit) {
                deps.push((bump.alloc_str(dep.name.as_str()), unit_base + dep_pos));
            }
        }
        if let Some(pm_idx) = pm_tc_crate_idx
            && !deps.iter().any(|(n, _)| *n == "proc_macro")
        {
            deps.push(("proc_macro", pm_idx));
        }

        let unit_implicit = if is_root && mode == CompileMode::BuildScript {
            implicit(u.edition, &UnitKind::Lib).binary()
        } else {
            implicit(u.edition, &u.kind)
        };

        // Workspace-independent deterministic crate key (Rule 25): never hashes host `UnitId(uid.0)`,
        // so compiling the same build script or proc-macro from any workspace emits byte-identical
        // `.wasm` output.
        let mut key_buf = std::vec::Vec::new();
        key_buf.extend_from_slice(u.name.as_str().as_bytes());
        key_buf.push(0);
        key_buf.extend_from_slice(u.version.as_str().as_bytes());
        key_buf.push(0);
        key_buf.extend_from_slice(u.crate_name.as_str().as_bytes());
        key_buf.push(0);
        let kind_byte: u8 = match u.kind {
            UnitKind::Lib => 1,
            UnitKind::ProcMacro => 2,
            UnitKind::BuildScript => 3,
            UnitKind::Bin(_) => 4,
            UnitKind::Test { .. } => 5,
            UnitKind::Example(_) => 6,
            UnitKind::Bench(_) => 7,
            _ => 8,
        };
        key_buf.push(kind_byte);
        let mut sorted_feats: std::vec::Vec<&str> = feats_slice.to_vec();
        sorted_feats.sort_unstable();
        sorted_feats.dedup();
        for f in sorted_feats {
            key_buf.push(0);
            key_buf.extend_from_slice(f.as_bytes());
        }
        let crate_key = CrateItemKey(ContentId::of("blaze wasm64 unit 2", &key_buf));

        let env = CrateEnv::of(
            EnvFacts {
                crate_name: bump.alloc_str(u.crate_name.as_str()),
                package: Some((bump.alloc_str(u.name.as_str()), bump.alloc_str(u.version.as_str()))),
                bin: None,
                seen: seen_root,
                read_folder: prefix_str,
                build_script: u.build_script.is_some(),
                script: script_ref,
                test_or_bench: false,
                bin_exes: BinNames::NONE,
                meta: graph.meta_of(u.package).map(|m| m.vars()),
            },
            alloc,
        )
        .ok();

        let unit_edition = syntax_edition(u.edition);
        crates.push(CrateInput {
            name: bump.alloc_str(u.crate_name.as_str()),
            key: crate_key,
            edition: unit_edition,
            implicit: unit_implicit,
            root: root_path,
            shown_from: prefix_str,
            features: feats_slice,
            deps: deps.into_bump_slice(),
            cfg: cfg.into_bump_slice(),
            script: script_ref,
        });
        origins.push(origin);
        envs.push(env);
        editions.push(unit_edition);
        if let Some(&pm_key) = dep_pm_keys_by_unit.get(&uid) {
            proc_keys.push((krate_idx, pm_key));
        }
    }

    // Synthesized wasm64 proc-macro reactor shim (strictly before `tc_base` so its `#[no_mangle]`
    // `getenv` overrides `wasi_rt::getenv` by crate-index precedence).
    if mode == CompileMode::ProcMacro {
        let shim_root = "/virtual/shim/src/lib.rs";
        let shim_key = CrateItemKey(ContentId::of(
            "blaze wasm64 pm shim",
            WASM64_PROC_MACRO_SHIM.as_bytes(),
        ));
        let mut shim_cfg: OwnVec<(&str, Option<&str>), Bump<'_>> =
            OwnVec::with_capacity_in(base_cfg.len(), bump);
        shim_cfg.extend_from_slice(base_cfg.as_slice());
        let mut shim_deps: OwnVec<(&str, usize), Bump<'_>> = OwnVec::with_capacity_in(1, bump);
        if let Some(pm_idx) = pm_tc_crate_idx {
            shim_deps.push(("proc_macro", pm_idx));
        }
        let ed24 = edition::PackageEdition::of_manifest("2024").expect("2024");
        crates.push(CrateInput {
            name: "blaze_wasm64_pm_shim",
            key: shim_key,
            edition: Edition::E2024,
            implicit: implicit(ed24, &UnitKind::Lib),
            root: shim_root,
            shown_from: "/virtual/shim/",
            features: &[],
            deps: shim_deps.into_bump_slice(),
            cfg: shim_cfg.into_bump_slice(),
            script: None,
        });
        origins.push(CrateOrigin::Virtual { root: shim_root, text: WASM64_PROC_MACRO_SHIM });
        envs.push(None);
        editions.push(Edition::E2024);
    }

    // Toolchain crates (`wasm64-blaze-wasip1`).
    let rust24 = edition::PackageEdition::of_manifest("2024").expect("2024");
    let tc_lib_implicit = implicit(rust24, &UnitKind::Lib);
    let tc_pm_implicit = implicit(rust24, &UnitKind::ProcMacro);
    for u in &tc_units {
        let mut cfg: OwnVec<(&str, Option<&str>), Bump<'_>> =
            OwnVec::with_capacity_in(tc_cfg_owned.len() + u.features.len(), bump);
        for (n, v) in &tc_cfg_owned {
            cfg.push((&*bump.alloc_str(n), v.as_deref().map(|s| &*bump.alloc_str(s))));
        }
        for &f in u.features {
            cfg.push(("feature", Some(f)));
        }
        cfg.sort_unstable();
        cfg.dedup();
        let mut deps: OwnVec<(&str, usize), Bump<'_>> = OwnVec::with_capacity_in(u.raw_deps.len(), bump);
        for &(name, raw_d) in &u.raw_deps {
            let mapped = tc_at[raw_d];
            if mapped == usize::MAX {
                return Err(SelfCompileError::Failed(std::format!(
                    "toolchain crate `{}` depends on unkept toolchain unit {raw_d}",
                    u.name
                )));
            }
            deps.push((name, tc_base + mapped));
        }
        crates.push(CrateInput {
            name: u.name,
            key: u.key,
            edition: u.edition,
            implicit: if u.proc_macro { tc_pm_implicit } else { tc_lib_implicit },
            root: u.root,
            shown_from: u.prefix,
            features: u.features,
            deps: deps.into_bump_slice(),
            cfg: cfg.into_bump_slice(),
            script: None,
        });
        origins.push(CrateOrigin::Toolchain { view: u.pack, prefix: u.prefix });
        envs.push(None);
        editions.push(u.edition);
    }

    let role_id = |r: Role| {
        tc_units
            .iter()
            .position(|u| u.role == r)
            .map(|i| CrateId((tc_base + i) as u32))
    };
    let sysroot = Sysroot {
        core: role_id(Role::Core),
        alloc: role_id(Role::Alloc),
        std: role_id(Role::Std),
        bstd: role_id(Role::Bstd),
    };

    // 5. Resolve all crates together with `blaze-scope`, reusing persisted disk records for
    //    `wasm64-blaze-wasip1` sysroot crates (`krate >= tc_base`) under
    //    `<store>/wasm64-sysroot-v1/<tc_id>/` (Rule 51: content-addressed records on disk, page
    //    cache only, zero in-memory cross-command cache).
    let sources: &'a Wasm64Sources<'a, 'a> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        Wasm64Sources {
            origins,
            envs,
            editions,
            proc_keys,
            procs: pm_hook,
            arena,
        },
        frame,
    ));
    let par = CompilePar::new();
    let sysroot_dir = store.join("wasm64-sysroot-v1").join(std::format!("{tc_id}"));
    let mut sysroot_kept = Wasm64SysrootKept::open(&sysroot_dir, tc_base, crates.as_slice(), bump);
    let sysroot_guard = if sysroot_kept.has_missing() {
        let guard = SYSROOT_INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        sysroot_kept = Wasm64SysrootKept::open(&sysroot_dir, tc_base, crates.as_slice(), bump);
        if sysroot_kept.has_missing() { Some(guard) } else { None }
    } else {
        None
    };
    let ready = Ready::serial(core::time::Duration::from_secs(120));
    let (solved, _) = resolve_kept(crates.as_slice(), sysroot, sources, &par, &ready, &sysroot_kept, bump)
        .map_err(|(at, e)| std::format!("blaze-scope resolve failed on `{}`: {e:?}", crates[at].name))?;

    // Extract proc-macro exports in declaration order (matching `_DECLS`) if in ProcMacro mode.
    let mut exported_macros: std::vec::Vec<Exported> = std::vec::Vec::new();
    if mode == CompileMode::ProcMacro
        && let Some(Solved::Solved(pm_scope)) = solved.first()
    {
        let pm_crate = pm_scope.names;
        if let Some((_, root_decls)) = pm_crate.decls.get(&pm_crate.root) {
            let mut idx = 0u32;
            for decl in root_decls.items.iter() {
                if let Some(src) = pm_crate.tables.macros.get(&decl.def.key)
                    && let MacroBody::Proc { kind, helpers } = src.body
                {
                    let helper_list: std::vec::Vec<std::string::String> = helpers
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect();
                    exported_macros.push(Exported {
                        index: idx,
                        kind: match kind {
                            blaze_surface::ProcKind::Derive => ProcKind::Derive,
                            blaze_surface::ProcKind::Attr => ProcKind::Attr,
                            blaze_surface::ProcKind::FnLike => ProcKind::FnLike,
                        },
                        name: src.name.to_owned(),
                        helpers: helper_list,
                    });
                    idx += 1;
                }
            }
        }
    }

    // 6. Collect names & items records into `bump`, atomically persist newly-solved sysroot
    //    records, and construct `blaze-ladder` `Records`.
    let fresh_sysroot = sysroot_kept.take_fresh();
    let mut kept = KeptRecords {
        names: OwnVec::with_capacity_in(crates.len(), alloc),
        items: OwnVec::with_capacity_in(crates.len(), alloc),
        files: OwnVec::with_capacity_in(crates.len(), alloc),
        tc_packs: OwnVec::with_capacity_in(crates.len(), alloc),
        arena,
        expansions: OwnVec::with_capacity_in(crates.len(), alloc),
        origins: OwnVec::with_capacity_in(crates.len(), alloc),
    };
    for (krate, entry) in solved.iter().enumerate() {
        let tc_pack = (krate >= tc_base).then(|| tc_units[krate - tc_base].pack);
        kept.tc_packs.push(tc_pack);
        match entry {
            Solved::Solved(scope) => {
                let c = scope.names;
                if let Some(fresh) = fresh_sysroot.get(&krate) {
                    kept.names.push(aligned(&fresh.names, bump));
                    kept.items.push(aligned(&fresh.items, bump));
                } else {
                    let nr = names_record::encode(c, alloc)
                        .map_err(|e| std::format!("names_record encode for `{}`: {e:?}", c.name))?;
                    kept.names.push(aligned(&nr, bump));
                    let ir = items_record::encode(c, alloc)
                        .map_err(|e| std::format!("items_record encode for `{}`: {e:?}", c.name))?;
                    kept.items.push(aligned(&ir, bump));
                }
                let mut f_map = blaze_own::BTreeMap::new_in(alloc);
                for f in c.files.iter() {
                    f_map.insert(&*bump.alloc_str(f.package.as_str()), f.text);
                }
                kept.files.push(f_map);
                let mut e_map = blaze_own::BTreeMap::new_in(alloc);
                let mut o_map = blaze_own::BTreeMap::new_in(alloc);
                for (call, e) in c.expansions.iter() {
                    if let Some(m) = e.made.as_ref() {
                        e_map.insert(*call, m.text);
                        let bytes: &'a [u8] = match &m.origins {
                            Ok(b) => bump.alloc_slice_copy(b),
                            Err(_) => &[],
                        };
                        o_map.insert(*call, bytes);
                    }
                }
                if krate >= tc_base
                    && let Some(fresh) = fresh_sysroot.get(&krate)
                {
                    let exp_bytes = encode_expansions_table(&e_map, &o_map);
                    let key_hex = hex_content_id(crates[krate].key.0);
                    let _ = publish_sysroot_crate(&sysroot_dir, &key_hex, fresh, &exp_bytes);
                }
                kept.expansions.push(e_map);
                kept.origins.push(o_map);
            }
            Solved::Found { .. } => {
                let cached = sysroot_kept.loaded_at(krate).ok_or_else(|| {
                    std::format!("cached sysroot crate `{}` missing loaded records", crates[krate].name)
                })?;
                kept.names.push(cached.names);
                kept.items.push(cached.items);
                // Toolchain files are served directly from `tc_packs[krate]` (zero extra copy).
                kept.files.push(blaze_own::BTreeMap::new_in(alloc));
                let mut e_map = blaze_own::BTreeMap::new_in(alloc);
                let mut o_map = blaze_own::BTreeMap::new_in(alloc);
                decode_expansions_table(cached.expansions, &mut e_map, &mut o_map)?;
                kept.expansions.push(e_map);
                kept.origins.push(o_map);
            }
        }
    }
    drop(sysroot_guard);

    let keys: &'a [CrateItemKey] = {
        let mut v: OwnVec<CrateItemKey, Bump<'a>> = OwnVec::with_capacity_in(crates.len(), bump);
        v.extend(crates.iter().map(|c| c.key));
        v.into_bump_slice()
    };
    let crate_names: &'a [&'a str] = {
        let mut v: OwnVec<&'a str, Bump<'a>> = OwnVec::with_capacity_in(crates.len(), bump);
        v.extend(crates.iter().map(|c| c.name));
        v.into_bump_slice()
    };
    let pos_of = |name: &str| crate_names.iter().position(|&n| n == name);
    let lang_crates: &'a [usize] = {
        let mut v: OwnVec<usize, Bump<'a>> = OwnVec::new_in(bump);
        for idx in [
            Some(0),
            pos_of("core"),
            pos_of("alloc"),
            pos_of("std"),
            pos_of("proc_macro"),
            pos_of("panic_abort"),
            pos_of("bstd"),
            pos_of("blaze_std"),
        ]
        .into_iter()
        .flatten()
        {
            if !v.as_slice().contains(&idx) {
                v.push(idx);
            }
        }
        v.into_bump_slice()
    };

    let load: &'a KeptRecords<'a> =
        blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(kept, frame));
    let log: &'a Log<'a> =
        blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Log::new(crate_names, alloc), frame));
    let records: &'a Records<'a> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(
        Records::new(keys, load, log, infer_frame, alloc),
        frame,
    ));
    let module_name = graph.units[root_unit.index()].name.as_str();
    let pm_crate_key = crates.first().map(|c| c.key);
    let body_sources: &'a Sourced<'a, Wasm64Sources<'a, 'a>> =
        blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(Sourced(sources), frame));
    let wasm = emit_wasm64_from_records(
        records,
        infer_frame,
        lang_crates,
        body_sources,
        tc_id,
        mode,
        shim_idx,
        module_name,
        pm_crate_key,
        alloc,
        heap.alloc(),
    )?;
    Ok(CompiledWasm64 { wasm, macros: exported_macros })
}

#[expect(clippy::too_many_arguments, reason = "ties NamesRecords<'n> lifetime cleanly to &records")]
fn emit_wasm64_from_records<'n>(
    records: &'n Records<'n>,
    infer_frame: &'n Frame,
    lang_crates: &'n [usize],
    body_sources: &'n dyn BodySources,
    tc_id: ToolchainId,
    mode: CompileMode,
    shim_idx: Option<usize>,
    module_name: &str,
    pm_crate_key: Option<CrateItemKey>,
    alloc: Alloc<'n>,
    heap_alloc: Alloc<'static>,
) -> Result<std::vec::Vec<u8>, SelfCompileError> {
    let nr = NamesRecords { records, frame: infer_frame, lang_crates, body_sources: Some(body_sources) };
    let builder = wasm64_builder(tc_id);

    // 7. Single-pass LIR -> Wasm64 compilation! Use `heap_alloc` for the growing worklist,
    //    `Vec<OwnedFunc>`, and emitted wasm buffer so reallocations free old backing storage
    //    instead of accumulating inside the bump-only scratch frame.
    let mut wasm_out: AVec<'static, u8> = OwnVec::new_in(heap_alloc);
    match mode {
        CompileMode::BuildScript => {
            let roots = run_roots(nr.records, nr.lang_crates, builder.settings.panic, &[], nr.frame, heap_alloc, alloc)
                .map_err(|e| std::format!("build-script run_roots failed: {e}"))?;
            let maker_entry = match roots.shim() {
                Some(shim) => Some(shim.main.tabled),
                None => (!roots.is_empty()).then(|| roots.root(0).tabled),
            };
            let mut maker = Maker::<SelfCompileGrant, SecondCheck>::of_roots(
                nr,
                maker_entry,
                &roots,
                SecondCheck,
                &builder,
                Some(crate::run_parent::command_make_watch()),
                heap_alloc,
            );
            let opts = Wasm64BuildOpts {
                module_name,
                start_symbol: Some(roots.entry),
                exports: &[],
                export_globals: &[],
                include_blaze_host_v1: false,
                opts: Wasm64Opts::USER_PROGRAM,
            };
            compile_roots_to_wasm64(
                &roots,
                |_| Some(SelfCompileGrant),
                &mut maker,
                &opts,
                default_wasm64_import,
                &mut wasm_out,
                heap_alloc,
            )
            .map_err(|e| map_wasm64_build_error(module_name, CompileMode::BuildScript, e))?;
        }
        CompileMode::ProcMacro => {
            let shim_crate = shim_idx
                .ok_or_else(|| std::string::String::from("missing proc-macro shim crate index"))?;
            let find_fn = |name: &str| -> Result<(LibKey, ItemKey), std::string::String> {
                let mut found = None;
                records.each_item(shim_crate, &mut |key, _| {
                    if found.is_none()
                        && records.kind_of_item(key) == Some(ItemKind::Function)
                        && records.name_of(key) == Some(name)
                    {
                        found = Some((key, checker_of(key)));
                    }
                });
                found.ok_or_else(|| std::format!("proc-macro shim missing fn `{name}`"))
            };
            let (_, init_item) = find_fn("_initialize")?;
            let (_, alloc_item) = find_fn("blaze_proc_macro_alloc")?;
            let (expand_lib, expand_item) = find_fn("blaze_proc_macro_expand")?;
            let pm_key = pm_crate_key.ok_or_else(|| std::string::String::from("missing proc-macro crate key"))?;
            let decls_lib_key = proc_macro_decls_key(pm_key);
            let decls_item = checker_of(decls_lib_key);

            let init_sym = fn_symbol_of_item(init_item, alloc)
                .map_err(|e| std::format!("symbol for _initialize: {e}"))?;
            let alloc_sym = fn_symbol_of_item(alloc_item, alloc)
                .map_err(|e| std::format!("symbol for blaze_proc_macro_alloc: {e}"))?;
            let expand_sym = fn_symbol_of_item(expand_item, alloc)
                .map_err(|e| std::format!("symbol for blaze_proc_macro_expand: {e}"))?;
            let decls_sym = static_symbol(decls_item);

            let mut maker = Maker::<SelfCompileGrant, SecondCheck>::of_start(
                nr,
                Some(expand_item),
                MakerStart::Direct(expand_lib),
                SecondCheck,
                &builder,
                Some(crate::run_parent::command_make_watch()),
                heap_alloc,
            );
            let exports = [
                (init_sym, "_initialize"),
                (alloc_sym, "blaze_proc_macro_alloc"),
                (expand_sym, "blaze_proc_macro_expand"),
            ];
            let export_globals = [(PROC_MACRO_DECLS_GLOBAL, decls_sym)];
            let opts = Wasm64BuildOpts {
                module_name,
                start_symbol: None,
                exports: &exports,
                export_globals: &export_globals,
                include_blaze_host_v1: false,
                opts: Wasm64Opts::USER_PROGRAM,
            };
            compile_items_to_wasm64(
                &[init_item, alloc_item, expand_item],
                &[decls_item],
                |_| Some(SelfCompileGrant),
                &mut maker,
                &opts,
                default_wasm64_import,
                &mut wasm_out,
                heap_alloc,
            )
            .map_err(|e| map_wasm64_build_error(module_name, CompileMode::ProcMacro, e))?;
        }
    }

    // Verify the emitted wasm64 module before publishing.
    check_wasm64(wasm_out.as_slice())
        .map_err(|e| std::format!("emitted wasm64 failed validation: {e}"))?;

    Ok(wasm_out.as_slice().to_vec())
}

fn publish_build_script_module(
    store: &Path,
    key: &str,
    identity: &str,
    wasm: &[u8],
    wasm_sha256: &str,
) -> Result<(), std::string::String> {
    let base = store.join("build-script/modules");
    let stage_root = store.join("build-script/.stage");
    std::fs::create_dir_all(&base)
        .map_err(|e| std::format!("create {}: {e}", base.display()))?;
    std::fs::create_dir_all(&stage_root)
        .map_err(|e| std::format!("create {}: {e}", stage_root.display()))?;
    let final_dir = base.join(key);
    if final_dir.join("wasm-sha256").is_file() {
        return Ok(());
    }
    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = stage_root.join(std::format!("{key}.{}.{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| std::format!("create {}: {e}", stage.display()))?;
    std::fs::write(stage.join("identity"), identity)
        .map_err(|e| std::format!("write identity: {e}"))?;
    std::fs::write(stage.join("module.wasm"), wasm)
        .map_err(|e| std::format!("write module.wasm: {e}"))?;
    std::fs::write(stage.join("wasm-sha256"), std::format!("{wasm_sha256}\n"))
        .map_err(|e| std::format!("write wasm-sha256: {e}"))?;
    if let Err(err) = std::fs::rename(&stage, &final_dir) {
        if final_dir.join("wasm-sha256").is_file() {
            let _ = std::fs::remove_dir_all(&stage);
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(&stage);
        return Err(std::format!("publish {}: {err}", final_dir.display()));
    }
    Ok(())
}

fn publish_proc_macro_module(
    store: &Path,
    key: &str,
    identity: &str,
    wasm: &[u8],
    wasm_sha256: &str,
    macros: &[Exported],
    decls_global: &str,
) -> Result<(), std::string::String> {
    let base = store.join("proc-macro/modules");
    let stage_root = store.join("proc-macro/.stage");
    std::fs::create_dir_all(&base)
        .map_err(|e| std::format!("create {}: {e}", base.display()))?;
    std::fs::create_dir_all(&stage_root)
        .map_err(|e| std::format!("create {}: {e}", stage_root.display()))?;
    let final_dir = base.join(key);
    if final_dir.join("wasm-sha256").is_file() {
        return Ok(());
    }
    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = stage_root.join(std::format!("{key}.{}.{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| std::format!("create {}: {e}", stage.display()))?;
    let mut macros_text = std::string::String::new();
    for m in macros {
        let kind_word = match m.kind {
            ProcKind::Derive => "derive",
            ProcKind::Attr => "attr",
            ProcKind::FnLike => "bang",
        };
        if m.helpers.is_empty() {
            let _ = writeln!(macros_text, "{} {kind_word} {}", m.index, m.name);
        } else {
            let _ = writeln!(
                macros_text,
                "{} {kind_word} {} {}",
                m.index,
                m.name,
                m.helpers.join(",")
            );
        }
    }
    std::fs::write(stage.join("identity"), identity)
        .map_err(|e| std::format!("write identity: {e}"))?;
    std::fs::write(stage.join("module.wasm"), wasm)
        .map_err(|e| std::format!("write module.wasm: {e}"))?;
    std::fs::write(stage.join("wasm-sha256"), std::format!("{wasm_sha256}\n"))
        .map_err(|e| std::format!("write wasm-sha256: {e}"))?;
    std::fs::write(stage.join("macros"), macros_text)
        .map_err(|e| std::format!("write macros: {e}"))?;
    std::fs::write(stage.join("decls"), std::format!("{decls_global}\n"))
        .map_err(|e| std::format!("write decls: {e}"))?;
    if let Err(err) = std::fs::rename(&stage, &final_dir) {
        if final_dir.join("wasm-sha256").is_file() {
            let _ = std::fs::remove_dir_all(&stage);
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(&stage);
        return Err(std::format!("publish {}: {err}", final_dir.display()));
    }
    Ok(())
}

struct CachedSysrootCrate<'a> {
    exports: &'a [u8],
    inner: &'a [u8],
    read: &'a [u8],
    names: &'a [u8],
    items: &'a [u8],
    expansions: &'a [u8],
}

#[derive(Default)]
struct FreshSysrootCrate {
    exports: std::vec::Vec<u8>,
    inner: std::vec::Vec<u8>,
    read: std::vec::Vec<u8>,
    names: std::vec::Vec<u8>,
    items: std::vec::Vec<u8>,
}

/// Persisted disk record store for resolved `wasm64-blaze-wasip1` sysroot crates
/// (`<store>/wasm64-sysroot-v1/<tc_id>/<crate_key_hex>/`). Follows Rule 51: content-addressed
/// records written once via atomic staging-dir rename, read in place via the kernel page cache,
/// and verified by `resolve_kept`'s read-list check (`blaze_scope::asked::check`).
struct Wasm64SysrootKept<'a> {
    tc_base: usize,
    loaded: std::vec::Vec<Option<CachedSysrootCrate<'a>>>,
    fresh: std::sync::Mutex<HashMap<usize, FreshSysrootCrate>>,
}

impl<'a> Wasm64SysrootKept<'a> {
    fn open(sysroot_dir: &Path, tc_base: usize, crates: &[CrateInput<'_>], bump: Bump<'a>) -> Self {
        let mut loaded = std::vec::Vec::with_capacity(crates.len());
        for (krate, input) in crates.iter().enumerate() {
            if krate < tc_base {
                loaded.push(None);
                continue;
            }
            let dir = sysroot_dir.join(hex_content_id(input.key.0));
            let rec = (|| -> Option<CachedSysrootCrate<'a>> {
                let exports = std::fs::read(dir.join("exports.bin")).ok()?;
                let inner = std::fs::read(dir.join("inner.bin")).ok()?;
                let read = std::fs::read(dir.join("read.bin")).ok()?;
                let names = std::fs::read(dir.join("names.bin")).ok()?;
                let items = std::fs::read(dir.join("items.bin")).ok()?;
                let expansions = std::fs::read(dir.join("expansions.bin")).ok()?;
                Some(CachedSysrootCrate {
                    exports: aligned(&exports, bump),
                    inner: aligned(&inner, bump),
                    read: aligned(&read, bump),
                    names: aligned(&names, bump),
                    items: aligned(&items, bump),
                    expansions: bump.alloc_slice_copy(&expansions),
                })
            })();
            loaded.push(rec);
        }
        Self { tc_base, loaded, fresh: std::sync::Mutex::new(HashMap::new()) }
    }

    fn has_missing(&self) -> bool {
        self.loaded.iter().skip(self.tc_base).any(Option::is_none)
    }

    fn loaded_at(&self, krate: usize) -> Option<&CachedSysrootCrate<'a>> {
        self.loaded.get(krate)?.as_ref()
    }

    fn take_fresh(&self) -> HashMap<usize, FreshSysrootCrate> {
        let mut guard = self.fresh.lock().unwrap_or_else(|p| p.into_inner());
        core::mem::take(&mut *guard)
    }
}

impl Kept for Wasm64SysrootKept<'_> {
    fn find(&self, krate: usize) -> Option<(&[u8], &[u8], &[u8])> {
        let c = self.loaded.get(krate)?.as_ref()?;
        Some((c.exports, c.inner, c.read))
    }

    fn made(&self, krate: usize, record: &[u8], inner: &[u8], read: &[u8]) {
        if krate < self.tc_base {
            return;
        }
        let mut guard = self.fresh.lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard.entry(krate).or_default();
        entry.exports = record.to_vec();
        entry.inner = inner.to_vec();
        entry.read = read.to_vec();
    }

    fn keeps_names(&self) -> bool {
        true
    }

    fn names(&self, krate: usize, names: &[u8]) {
        if krate < self.tc_base {
            return;
        }
        let mut guard = self.fresh.lock().unwrap_or_else(|p| p.into_inner());
        guard.entry(krate).or_default().names = names.to_vec();
    }

    fn keeps_items(&self) -> bool {
        true
    }

    fn items(&self, krate: usize, items: &[u8]) {
        if krate < self.tc_base {
            return;
        }
        let mut guard = self.fresh.lock().unwrap_or_else(|p| p.into_inner());
        guard.entry(krate).or_default().items = items.to_vec();
    }
}

fn hex_content_id(id: ContentId) -> std::string::String {
    let mut s = std::string::String::with_capacity(64);
    for b in id.0 {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn encode_expansions_table(
    expansions: &blaze_own::BTreeMap<LibKey, &str, Alloc<'_>>,
    origins: &blaze_own::BTreeMap<LibKey, &[u8], Alloc<'_>>,
) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::new();
    let count = expansions.len() as u32;
    out.extend_from_slice(&count.to_le_bytes());
    for (&call, &text) in expansions.iter() {
        let orig = origins.get(&call).copied().unwrap_or(&[]);
        out.extend_from_slice(&call.0.0);
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(&(orig.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(orig);
    }
    out
}

fn decode_expansions_table<'a>(
    bytes: &'a [u8],
    expansions: &mut blaze_own::BTreeMap<LibKey, &'a str, Alloc<'a>>,
    origins: &mut blaze_own::BTreeMap<LibKey, &'a [u8], Alloc<'a>>,
) -> Result<(), std::string::String> {
    if bytes.len() < 4 {
        return Err(std::string::String::from("truncated wasm64 sysroot expansions table"));
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let mut at = 4usize;
    for _ in 0..count {
        if at + 40 > bytes.len() {
            return Err(std::string::String::from("truncated wasm64 sysroot expansion entry header"));
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[at..at + 32]);
        let key = LibKey(ContentId(id));
        let tlen = u32::from_le_bytes([bytes[at + 32], bytes[at + 33], bytes[at + 34], bytes[at + 35]]) as usize;
        let olen = u32::from_le_bytes([bytes[at + 36], bytes[at + 37], bytes[at + 38], bytes[at + 39]]) as usize;
        at += 40;
        if at + tlen + olen > bytes.len() {
            return Err(std::string::String::from("truncated wasm64 sysroot expansion entry payload"));
        }
        let text = core::str::from_utf8(&bytes[at..at + tlen])
            .map_err(|_| std::string::String::from("non-utf8 wasm64 sysroot expansion text"))?;
        let orig = &bytes[at + tlen..at + tlen + olen];
        at += tlen + olen;
        expansions.insert(key, text);
        origins.insert(key, orig);
    }
    Ok(())
}

fn publish_sysroot_crate(
    sysroot_dir: &Path,
    key_hex: &str,
    fresh: &FreshSysrootCrate,
    expansions: &[u8],
) -> Result<(), std::string::String> {
    if fresh.exports.is_empty() || fresh.names.is_empty() || fresh.items.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(sysroot_dir)
        .map_err(|e| std::format!("create {}: {e}", sysroot_dir.display()))?;
    let stage_root = sysroot_dir.parent().unwrap_or(sysroot_dir).join(".stage");
    std::fs::create_dir_all(&stage_root)
        .map_err(|e| std::format!("create {}: {e}", stage_root.display()))?;
    let final_dir = sysroot_dir.join(key_hex);
    if final_dir.join("expansions.bin").is_file() {
        return Ok(());
    }
    let seq = STAGE_SEQ.fetch_add(1, Ordering::Relaxed);
    let stage = stage_root.join(std::format!("{key_hex}.{}.{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)
        .map_err(|e| std::format!("create {}: {e}", stage.display()))?;
    std::fs::write(stage.join("exports.bin"), &fresh.exports)
        .map_err(|e| std::format!("write exports.bin: {e}"))?;
    std::fs::write(stage.join("inner.bin"), &fresh.inner)
        .map_err(|e| std::format!("write inner.bin: {e}"))?;
    std::fs::write(stage.join("read.bin"), &fresh.read)
        .map_err(|e| std::format!("write read.bin: {e}"))?;
    std::fs::write(stage.join("names.bin"), &fresh.names)
        .map_err(|e| std::format!("write names.bin: {e}"))?;
    std::fs::write(stage.join("items.bin"), &fresh.items)
        .map_err(|e| std::format!("write items.bin: {e}"))?;
    std::fs::write(stage.join("expansions.bin"), expansions)
        .map_err(|e| std::format!("write expansions.bin: {e}"))?;
    if std::fs::rename(&stage, &final_dir).is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    Ok(())
}
