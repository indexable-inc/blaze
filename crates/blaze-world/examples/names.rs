//! A real project's names: `names <workspace dir> <fetch store folder> [--threads 1,18]
//! [--roots a,b] [--examples N] [--crates]`.
//!
//! Reads the workspace's `Cargo.lock` and its members' manifests from the checkout, every other
//! package's manifest and files from its record in blaze-fetch's store folder
//! (`records/<key>`), the toolchain from the shelf (`$HOME/.blaze/v2/toolchain`, the pinned id);
//! makes the unit graph, then [`blaze_world::world`], then resolves every crate's names with
//! `resolve_crates` once per thread count. `--roots` keeps only the named packages' libraries
//! and what they depend on (a subset closed under dependencies).
//!
//! Prints, per crate (`--crates`) and in total: modules, imports, unresolved imports, macro
//! paths, unresolved macro paths, and its time (from its first file read to the next crate's:
//! crates are resolved one after another); the unresolved by cause with a count and one place
//! each; the time of each stage; the lines read; the peak footprint.

#![feature(allocator_api)]

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use blaze_fetch::{PackView, SourceId};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{CrateInput, CrateNames, SourceTexts};
use blaze_scope::task::Loc;
use blaze_surface::{BuiltinDerive, CallShape, Path, PathKind, ScopeError};
use blaze_toolchain::Shelf;
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Records, Resolution, Resolver, StoreSources, TargetDesc, UnitGraph, UnitId, UnitKind};
use blaze_world::{Checkout, Fetched, Script, Toolchain, World};

type N = &'static str;

/// The one allocator of the run: the global heap's, named at the program's top.
fn g() -> Alloc<'static> {
    Alloc::global()
}

// ---------------------------------------------------------------------------------------------
// `--owners`: the heap's bytes by owner. Each owner allocates from its own malloc zone (macOS's
// `malloc_create_zone`, a public libmalloc call), chosen by the worker thread's current owner;
// a free or realloc finds the block's zone itself, so a block stays its first owner's. The
// zones' `size_in_use` and `max_size_in_use` are exact, with no header or counter per block.
// Without `--owners` no zone is made and every block comes from the system's default zone.

mod owners {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicPtr, Ordering};

    /// The owners, by index: 0 is everything not tagged (the world, the graph, the counting).
    pub const NAMES: [&str; 5] = ["untagged (world, graph, counts, results' shells)", "a crate's own solve (its tables, expansions, its records)", "decoded dependency scopes (shared for the run)", "proc-macro runs (the runtime, produced tokens)", "source texts read to the heap"];
    pub const SOLVE: u8 = 1;
    pub const DECODED: u8 = 2;
    pub const PROC: u8 = 3;
    pub const SOURCE: u8 = 4;

    #[repr(C)]
    #[derive(Default)]
    pub struct Stats {
        pub blocks_in_use: u32,
        pub size_in_use: usize,
        pub max_size_in_use: usize,
        pub size_allocated: usize,
    }

    unsafe extern "C" {
        fn malloc_create_zone(start_size: usize, flags: u32) -> *mut c_void;
        fn malloc_zone_malloc(zone: *mut c_void, size: usize) -> *mut c_void;
        fn malloc_zone_memalign(zone: *mut c_void, alignment: usize, size: usize) -> *mut c_void;
        fn malloc_zone_calloc(zone: *mut c_void, count: usize, size: usize) -> *mut c_void;
        fn malloc_zone_statistics(zone: *mut c_void, stats: *mut Stats);
        fn malloc_default_zone() -> *mut c_void;
    }

    static ZONES: [AtomicPtr<c_void>; 5] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 5];

    thread_local! {
        static OWNER: Cell<u8> = const { Cell::new(0) };
    }

    /// Makes the zones (owner 0 keeps the default zone): before any owner is set.
    pub fn enable() {
        for zone in &ZONES[1..] {
            // SAFETY: `malloc_create_zone` takes a size hint and flags (0, 0) and returns a new zone.
            zone.store(unsafe { malloc_create_zone(0, 0) }, Ordering::Release);
        }
    }

    /// Sets this thread's owner, giving back the one before.
    pub fn set(owner: u8) -> u8 {
        OWNER.with(|o| o.replace(owner))
    }

    /// This thread's owner: what a thread it spawns for the same work takes.
    pub fn get() -> u8 {
        OWNER.with(Cell::get)
    }

    /// Each owner's statistics: `None` for one without its zone.
    pub fn stats() -> [Option<Stats>; 5] {
        let mut out = [None, None, None, None, None];
        for (at, zone) in ZONES.iter().enumerate() {
            // Owner 0's zone is the default one, read only once the others exist.
            let z = match at {
                // SAFETY: `malloc_default_zone` takes nothing and returns the process's zone.
                0 if !ZONES[1].load(Ordering::Acquire).is_null() => unsafe { malloc_default_zone() },
                _ => zone.load(Ordering::Acquire),
            };
            if !z.is_null() {
                let mut s = Stats::default();
                // SAFETY: `z` is a zone `malloc_create_zone` made; `s` is a `malloc_statistics_t`.
                unsafe { malloc_zone_statistics(z, &mut s) };
                out[at] = Some(s);
            }
        }
        out
    }

    pub struct ByOwner;

    fn zone() -> *mut c_void {
        let owner = OWNER.try_with(Cell::get).unwrap_or(0);
        ZONES[owner as usize].load(Ordering::Relaxed)
    }

    // SAFETY: every block comes from libmalloc (a zone or the default), aligned as asked
    // (`malloc_zone_memalign` past the 16 bytes every zone gives); `System`'s dealloc and
    // realloc call `free`/`realloc`, which find a block's zone.
    unsafe impl GlobalAlloc for ByOwner {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let z = zone();
            if z.is_null() {
                // SAFETY: the caller's layout, passed on.
                return unsafe { System.alloc(layout) };
            }
            // SAFETY: `z` is a live zone; the size and alignment are the layout's.
            unsafe { if layout.align() <= 16 { malloc_zone_malloc(z, layout.size()) } else { malloc_zone_memalign(z, layout.align(), layout.size()) }.cast() }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let z = zone();
            if z.is_null() || layout.align() > 16 {
                // SAFETY: the caller's layout, passed on.
                return unsafe { System.alloc_zeroed(layout) };
            }
            // SAFETY: `z` is a live zone; one element of the layout's size.
            unsafe { malloc_zone_calloc(z, 1, layout.size()).cast() }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` came from libmalloc above; `free` finds its zone.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as `dealloc`; `realloc` keeps the block in its zone.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
}

#[global_allocator]
static BY_OWNER: owners::ByOwner = owners::ByOwner;

const MAC_CFG: &[(&str, Option<&str>)] = &[
    ("panic", Some("unwind")),
    ("target_abi", Some("")),
    ("target_arch", Some("aarch64")),
    ("target_endian", Some("little")),
    ("target_env", Some("")),
    ("target_family", Some("unix")),
    ("target_feature", Some("aes")),
    ("target_feature", Some("crc")),
    ("target_feature", Some("dit")),
    ("target_feature", Some("dotprod")),
    ("target_feature", Some("dpb")),
    ("target_feature", Some("dpb2")),
    ("target_feature", Some("fcma")),
    ("target_feature", Some("fhm")),
    ("target_feature", Some("flagm")),
    ("target_feature", Some("fp16")),
    ("target_feature", Some("frintts")),
    ("target_feature", Some("jsconv")),
    ("target_feature", Some("lor")),
    ("target_feature", Some("lse")),
    ("target_feature", Some("neon")),
    ("target_feature", Some("paca")),
    ("target_feature", Some("pacg")),
    ("target_feature", Some("pan")),
    ("target_feature", Some("pmuv3")),
    ("target_feature", Some("ras")),
    ("target_feature", Some("rcpc")),
    ("target_feature", Some("rcpc2")),
    ("target_feature", Some("rdm")),
    ("target_feature", Some("sb")),
    ("target_feature", Some("sha2")),
    ("target_feature", Some("sha3")),
    ("target_feature", Some("ssbs")),
    ("target_feature", Some("vh")),
    ("target_has_atomic", Some("128")),
    ("target_has_atomic", Some("16")),
    ("target_has_atomic", Some("32")),
    ("target_has_atomic", Some("64")),
    ("target_has_atomic", Some("8")),
    ("target_has_atomic", Some("ptr")),
    ("target_os", Some("macos")),
    ("target_pointer_width", Some("64")),
    ("target_vendor", Some("apple")),
    ("unix", None),
];

fn mac() -> TargetDesc<'static> {
    let mut cfg = AVec::new_in(g());
    for (name, value) in MAC_CFG {
        cfg.push(match value {
            Some(v) => CfgAtom::pair(name, v, g()),
            None => CfgAtom::flag(name, g()),
        });
    }
    TargetDesc::new("aarch64-apple-darwin", cfg, g())
}

fn layout(anchor: &FsPath) -> blaze_manifest::layout::Units<'static> {
    let mut reader = Layout::new("", g()).unwrap_or_else(|e| panic!("{e}"));
    for _ in 0..64 {
        let needs = match reader.pass() {
            Ok(LayoutPass::Done(units)) => return units,
            Ok(LayoutPass::Need(needs)) => needs,
            Err(e) => panic!("workspace layout: {e}"),
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                    reader.manifest(path, text.as_deref()).unwrap_or_else(|e| panic!("{}: {e}", path.as_str()));
                }
                Need::Listing(dir) => {
                    let listed: Option<Vec<(String, bool)>> = std::fs::read_dir(anchor.join(dir.as_str())).ok().map(|listed| {
                        let mut entries: Vec<(String, bool)> =
                            listed.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir()))).collect();
                        entries.sort();
                        entries
                    });
                    let entries: Option<Vec<Entry<'_>>> = listed.as_ref().map(|l| l.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    panic!("no layout after 64 passes")
}

unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
}

/// A file mapped read-only and private for the rest of the run.
fn map(path: &FsPath) -> std::io::Result<&'static [u8]> {
    use std::os::fd::AsRawFd as _;
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: a read-only private mapping of `len` bytes of an open file the store only ever
    // replaces by a rename; never unmapped, so the slice lives as long as the process.
    let ptr = unsafe { mmap(core::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
    if ptr as isize == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: mapped above, `len` bytes, readable for the rest of the process.
    Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
}

/// The lifetime peak `phys_footprint` (`rusage_info_v4`'s `ri_lifetime_max_phys_footprint`).
fn max_footprint() -> u64 {
    let mut info = [0u64; 40];
    // SAFETY: flavor 4 writes one rusage_info_v4 (296 bytes), which the buffer outsizes.
    let ok = unsafe { proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr()) };
    if ok == 0 { info[2 + 28] } else { 0 }
}

/// The footprint now (`rusage_info_v4`'s `ri_phys_footprint`, the eighth field after the uuid).
fn now_footprint() -> u64 {
    let mut info = [0u64; 40];
    // SAFETY: as `max_footprint`.
    let ok = unsafe { proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr()) };
    if ok == 0 { info[2 + 7] } else { 0 }
}

/// blaze-fetch's store folder: one file a record, `records/<hex of the key>`.
struct Folder {
    records: PathBuf,
}

impl Records<'static> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'static [u8]> {
        let hex: String = source.key().0.iter().map(|b| format!("{b:02x}")).collect();
        let path = self.records.join(hex);
        match map(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

/// The installed toolchain's files.
struct DirShelf(String);

impl Shelf for DirShelf {
    type Error = std::io::Error;
    type Bytes = &'static [u8];
    fn place(&self) -> &str {
        &self.0
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, std::io::Error> {
        match std::fs::metadata(format!("{}/{name}", self.0)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn read(&mut self, name: &str, _size: u64) -> Result<&'static [u8], std::io::Error> {
        map(FsPath::new(&format!("{}/{name}", self.0)))
    }
}

// ---------------------------------------------------------------------------------------------
// Threads.

/// A value that crosses threads although its type is not `Send` or `Sync` only because it holds
/// `Alloc` handles: every handle in it is [`g`]'s, the process's global heap, which any thread
/// may allocate from and free to at once; nothing else in the wrapped values (the fill's task
/// closures and results) is bound to a thread (blaze-scope's examples/crates.rs, `OnGlobal`).
struct OnGlobal<T>(T);

// SAFETY: see `OnGlobal`.
unsafe impl<T> Send for OnGlobal<T> {}
// SAFETY: see `OnGlobal`.
unsafe impl<T> Sync for OnGlobal<T> {}

impl<T> std::ops::Deref for OnGlobal<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

fn par_map<T: Send>(threads: usize, n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let (f, next) = (&f, &next);
    // `--owners`: the spawned threads do the caller's work, so they allocate as its owner.
    let owner = owners::get();
    let parts: Vec<Vec<(usize, T)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.min(n.max(1)))
            .map(|_| {
                scope.spawn(move || {
                    owners::set(owner);
                    let mut mine = Vec::new();
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        if at >= n {
                            break;
                        }
                        mine.push((at, f(at)));
                    }
                    mine
                })
            })
            .collect();
        handles.into_iter().map(|it| it.join().expect("a worker thread panicked")).collect()
    });
    let mut all: Vec<Option<T>> = (0..n).map(|_| None).collect();
    for part in parts {
        for (at, value) in part {
            all[at] = Some(value);
        }
    }
    all.into_iter().map(|it| it.expect("every index is taken once")).collect()
}

struct Threads(usize);

/// `--crates`: each crate's start and end on the ready queue, in ns since `start`.
struct Clock {
    start: Instant,
    spans: Vec<(AtomicU64, AtomicU64)>,
    /// By crate index: how many times its exports record was decoded.
    decodes: Vec<AtomicU64>,
    /// The bytes of every record decoded, summed over decodes.
    decoded_bytes: AtomicU64,
    /// By crate index: how many times its record was given to a crate being resolved (each
    /// was a decode before records were decoded once a call).
    handed: Vec<AtomicU64>,
}

impl blaze_scope::crate_scope::ReadyHooks for Clock {
    fn began(&self, krate: usize, _worker: usize) {
        owners::set(owners::SOLVE);
        self.spans[krate].0.store(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn ended(&self, krate: usize, _worker: usize) {
        self.spans[krate].1.store(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        owners::set(0);
    }

    fn decoding(&self, _krate: usize) {
        owners::set(owners::DECODED);
    }

    fn decoded(&self, krate: usize, bytes: usize) {
        self.decodes[krate].fetch_add(1, Ordering::Relaxed);
        self.decoded_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn handed(&self, krate: usize) {
        self.handed[krate].fetch_add(1, Ordering::Relaxed);
    }
}

impl blaze_scope::task::Par for Threads {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        if self.0 <= 1 {
            for at in 0..n {
                out.push(f(at));
            }
            return;
        }
        let f = OnGlobal(f);
        for OnGlobal(it) in par_map(self.0, n, |i| OnGlobal((*f)(i))) {
            out.push(it);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The texts, timed: each crate's first read, and the lines and bytes read.

/// `--procs`: blaze-proc-macro's hook over a module store, and per crate the key its macro
/// crate is registered under (proc-macro crates), its edition and its package for the sandbox.
struct Procs {
    procs: blaze_proc_macro::hook::ProcMacros<'static>,
    keys: Vec<Option<blaze_id::CrateItemKey>>,
    editions: Vec<blaze_surface::Edition>,
    packages: Vec<std::sync::Arc<blaze_proc_macro::sandbox::Package>>,
    /// Per crate, its module's declared macros; `None` for a crate with no module.
    exports: Vec<Option<&'static [blaze_scope::task::ProcExport<'static>]>>,
}

struct Timed<'w> {
    inner: &'w blaze_world::Texts<'static>,
    procs: Option<&'w Procs>,
    start: Instant,
    first: Vec<AtomicU64>,
    lines: AtomicU64,
    bytes: AtomicU64,
    files: AtomicU64,
    /// `--records`: by crate, each checkout file (tree 0) its solve read, in order of reading.
    /// A fetched or toolchain file is under the crate's key, so it is not listed.
    read: Vec<std::sync::Mutex<Vec<&'static str>>>,
}

/// `--records <folder>`: each crate's two records kept between runs, one file a crate
/// (`<hex of its key>`), with its read list: each checkout file its solve read and that file's
/// surface (`blaze_file_items::Outline::surface`; an absent file: zeros), then each crate whose
/// exports record it was handed, by key, with that record's digest. A crate is found when every
/// surface and every digest is the same today: a body edit moves no surface, a private item
/// moves its own crate's surface only, and its dependents see its exports digest unchanged.
struct KeptDir<'w> {
    dir: PathBuf,
    /// The checkout's root (paths `/0/<inner>`), and whether a file's stamp may stand for it.
    root: PathBuf,
    stamps: bool,
    /// The same texts the ready queue's workers read at once (`Texts` holds a `dyn Tree` that is
    /// not marked `Sync`; it is read from every worker already): [`OnGlobal`].
    texts: OnGlobal<&'w Timed<'w>>,
    inputs: &'w [CrateInput<'static>],
    by_key: std::collections::HashMap<blaze_id::CrateItemKey, usize>,
    found: AtomicUsize,
    /// Time inside `find`, summed over workers, and the checkout files it outlined there.
    find_ns: AtomicU64,
    outlined: AtomicU64,
    /// By crate: why it was not found (`None`: found, or not asked).
    missed: Vec<std::sync::Mutex<Option<String>>>,
}

const KEPT_MAGIC: &[u8; 8] = b"BZKEPT2\0";

impl KeptDir<'_> {
    fn path(&self, krate: usize) -> PathBuf {
        let key = self.inputs[krate].key.0.0;
        self.dir.join(key.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    /// A checkout file's stamp: its length, change time and inode (`stat`, one call, no read);
    /// all zeros when absent. Equal stamps: the file is taken as unchanged, its surface not
    /// computed again (step 3's fixed cost; `--no-stamps` turns this off to measure it).
    fn stamp(&self, path: &str) -> [u8; 24] {
        use std::os::unix::fs::MetadataExt as _;
        let inner = path.strip_prefix("/0/").unwrap_or_else(|| panic!("kept record: `{path}` is not a checkout path"));
        let mut out = [0u8; 24];
        match std::fs::metadata(self.root.join(inner)) {
            Ok(m) => {
                out[..8].copy_from_slice(&m.len().to_le_bytes());
                out[8..16].copy_from_slice(&(m.ctime() as u64 ^ ((m.ctime_nsec() as u64) << 34)).to_le_bytes());
                out[16..].copy_from_slice(&m.ino().to_le_bytes());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("kept record: stat of checkout file {inner}: {e}"),
        }
        out
    }

    /// A checkout file's surface today: zeros when absent.
    fn surface(&self, krate: usize, path: &str) -> [u8; 32] {
        self.outlined.fetch_add(1, Ordering::Relaxed);
        match self.texts.inner.read(krate, path) {
            Some(text) => blaze_file_items::outline(text, self.inputs[krate].edition, g()).surface().0,
            None => [0; 32],
        }
    }

    fn miss(&self, krate: usize, why: String) -> Option<(&[u8], &[u8])> {
        *self.missed[krate].lock().unwrap_or_else(|p| panic!("crate {krate}'s miss lock was poisoned: {p}")) = Some(why);
        None
    }
}

/// A cursor over a kept file: each read names the file and the field it was after.
struct KeptRead<'b> {
    bytes: &'b [u8],
    at: usize,
    file: &'b FsPath,
}

impl<'b> KeptRead<'b> {
    fn take(&mut self, n: usize, what: &str) -> &'b [u8] {
        let Some(part) = self.bytes.get(self.at..self.at + n) else {
            panic!("kept record {}: cut short at byte {} reading {what} ({n} bytes)", self.file.display(), self.at);
        };
        self.at += n;
        part
    }

    fn u32(&mut self, what: &str) -> usize {
        u32::from_le_bytes(self.take(4, what).try_into().expect("four bytes")) as usize
    }

    fn u64(&mut self, what: &str) -> usize {
        u64::from_le_bytes(self.take(8, what).try_into().expect("eight bytes")) as usize
    }

    fn id(&mut self, what: &str) -> [u8; 32] {
        self.take(32, what).try_into().expect("32 bytes")
    }
}

impl blaze_scope::crate_scope::Kept for KeptDir<'_> {
    fn find(&self, krate: usize, exports: &dyn Fn(usize) -> Option<blaze_id::ContentId>) -> Option<(&[u8], &[u8])> {
        let start = Instant::now();
        let file = self.path(krate);
        let bytes: &'static [u8] = match std::fs::read(&file) {
            Ok(bytes) => Vec::leak(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.miss(krate, "no kept record".into()),
            Err(e) => panic!("kept record {}: {e}", file.display()),
        };
        let file: &'static FsPath = Box::leak(file.into_boxed_path());
        let mut r = KeptRead { bytes, at: 0, file };
        assert!(r.take(8, "the magic") == KEPT_MAGIC, "kept record {}: not a kept record (its magic)", file.display());
        let mut why = None;
        for _ in 0..r.u32("the file count") {
            let len = r.u32("a path's length");
            let path = std::str::from_utf8(r.take(len, "a path")).unwrap_or_else(|e| panic!("kept record {}: a path not UTF-8: {e}", file.display()));
            let stamp = r.take(24, "a stamp");
            let was = r.id("a surface");
            if why.is_none() && !(self.stamps && self.stamp(path) == stamp) && self.surface(krate, path) != was {
                why = Some(format!("the surface of {path} moved"));
            }
        }
        for _ in 0..r.u32("the read count") {
            let key = blaze_id::CrateItemKey(blaze_id::ContentId(r.id("a crate key")));
            let was = r.id("an exports digest");
            if why.is_none() {
                match self.by_key.get(&key) {
                    None => why = Some("a crate it read is not in the graph".into()),
                    Some(&g) => match exports(g) {
                        None => why = Some(format!("`{}`'s exports not written yet", self.inputs[g].name)),
                        Some(now) if now.0 != was => why = Some(format!("`{}`'s exports changed", self.inputs[g].name)),
                        Some(_) => {}
                    },
                }
            }
        }
        let len = r.u64("the exports length");
        let record = r.take(len, "the exports record");
        let len = r.u64("the inner length");
        let inner = r.take(len, "the inner record");
        self.find_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if let Some(why) = why {
            return self.miss(krate, why);
        }
        self.found.fetch_add(1, Ordering::Relaxed);
        Some((record, inner))
    }

    fn made(&self, krate: usize, record: &[u8], inner: &[u8], read: &[(usize, blaze_id::ContentId)]) {
        let files: Vec<&'static str> = self.texts.read[krate].lock().unwrap_or_else(|p| panic!("crate {krate}'s read list lock was poisoned: {p}")).clone();
        let mut out = Vec::with_capacity(64 + record.len() + inner.len());
        out.extend_from_slice(KEPT_MAGIC);
        out.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for path in &files {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            // The stamp before the text is read: a write between the two leaves an old stamp,
            // so the next run outlines the file again rather than trust it.
            out.extend_from_slice(&self.stamp(path));
            out.extend_from_slice(&self.surface(krate, path));
        }
        out.extend_from_slice(&(read.len() as u32).to_le_bytes());
        for &(g, digest) in read {
            out.extend_from_slice(&self.inputs[g].key.0.0);
            out.extend_from_slice(&digest.0);
        }
        out.extend_from_slice(&(record.len() as u64).to_le_bytes());
        out.extend_from_slice(record);
        out.extend_from_slice(&(inner.len() as u64).to_le_bytes());
        out.extend_from_slice(inner);
        let path = self.path(krate);
        let part = path.with_extension("part");
        std::fs::write(&part, &out).unwrap_or_else(|e| panic!("kept record {}: {e}", part.display()));
        std::fs::rename(&part, &path).unwrap_or_else(|e| panic!("kept record {}: {e}", path.display()));
    }
}

impl SourceTexts<'static> for Timed<'_> {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        let ns = self.start.elapsed().as_nanos() as u64 + 1;
        let _ = self.first[krate].compare_exchange(0, ns, Ordering::Relaxed, Ordering::Relaxed);
        if path.starts_with("/0/") {
            let mut read = self.read[krate].lock().unwrap_or_else(|p| panic!("crate {krate}'s read list lock was poisoned: {p}"));
            if !read.iter().any(|p| *p == path) {
                read.push(Box::leak(path.to_owned().into_boxed_str()));
            }
        }
        let before = owners::set(owners::SOURCE);
        let text = self.inner.read(krate, path);
        owners::set(before);
        let text = text?;
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
        self.lines.fetch_add(text.bytes().filter(|b| *b == b'\n').count() as u64, Ordering::Relaxed);
        Some(text)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<blaze_id::CrateItemKey> {
        self.procs?.keys[krate]
    }

    fn proc_exports(&self, krate: usize) -> Option<&'static [blaze_scope::task::ProcExport<'static>]> {
        self.procs?.exports[krate]
    }

    fn run_proc_macro<'b>(
        &self,
        krate: usize,
        def: &blaze_expand::MacroDefinition<'_>,
        call: blaze_expand::ProcCall<'_>,
        alloc: Alloc<'b>,
    ) -> Result<blaze_expand::Produced<'b>, blaze_expand::HostError> {
        let refused = blaze_expand::HostError { ask: blaze_expand::HostAsk::ProcMacro, why: blaze_expand::HostWhy::Refused };
        let Some(p) = self.procs else { return Err(refused) };
        let before = owners::set(owners::PROC);
        let produced = p.procs.run(def, &call, p.editions[krate], &p.packages[krate], alloc);
        owners::set(before);
        produced
    }
}

/// The hook over the module store `store`, writing compiled modules and records under `out`:
/// each proc-macro crate registered by its lock identity (registry checksum, a path package's
/// files, a git commit and folder: the folder from the store's module identities, as the lock
/// does not say it).
fn procs_of<P: blaze_world::Packages<'static>>(store: &FsPath, out: &FsPath, root: &FsPath, world: &World<'static>, graph: &UnitGraph<'static>, lock: &Lock<'static>, lock_text: &str, paths: &[(LockId, &'static str)], places: &mut P) -> Procs {
    use blaze_proc_macro::hook::ProcMacros;
    let runtime: &'static blaze_proc_macro::host::Runtime = Box::leak(Box::new(blaze_proc_macro::host::Runtime::new().unwrap_or_else(|e| panic!("the proc-macro runtime: {e}"))));
    let records: &'static blaze_proc_macro::record::DirStore = Box::leak(Box::new(blaze_proc_macro::record::DirStore::new(out)));
    let mut procs = ProcMacros::new(runtime, records, blaze_proc_macro::host::Limits::STANDARD);
    let mut keys = vec![None; world.crates.len()];
    let mut packages = Vec::with_capacity(world.crates.len());
    let mut macro_units = Vec::new();
    // Each crate's macros see its own package's files and nothing else, at the sandbox's one
    // virtual root that `CARGO_MANIFEST_DIR` names (proc-macro-crate reads
    // `$CARGO_MANIFEST_DIR/Cargo.toml`: jsonrpsee's `rpc`): a workspace member's folder on
    // disk, a fetched package's files read in place from its fetch record. A toolchain crate
    // calls no proc macro: an empty list.
    use blaze_proc_macro::sandbox::{Files, PACKAGE};
    let none: std::sync::Arc<[(String, &'static [u8])]> = std::sync::Arc::from(Vec::new());
    for (i, c) in world.crates.iter().enumerate() {
        let mut env = vec![
            ("CARGO_PKG_NAME".to_string(), c.package.to_string()),
            ("CARGO_PKG_VERSION".to_string(), c.version.to_string()),
            ("CARGO_CRATE_NAME".to_string(), c.input.name.to_string()),
        ];
        let files = match c.origin {
            blaze_world::Origin::Unit(uid) => {
                env.push(("CARGO_MANIFEST_DIR".to_string(), PACKAGE.to_string()));
                match places.place(graph.units[uid.0 as usize].package).unwrap_or_else(|e| panic!("{} {}: {e}", c.package, c.version)) {
                    blaze_world::Place::Checkout { dir } => Files::Folder(root.join(dir)),
                    blaze_world::Place::Pack { view, dir } => {
                        let prefix = if dir.is_empty() { String::new() } else { format!("{}/", dir.trim_matches('/')) };
                        let mut listed: Vec<(String, &'static [u8])> =
                            view.files().filter_map(|f| f.path.strip_prefix(prefix.as_str()).map(|rel| (rel.to_string(), f.bytes))).collect();
                        listed.sort_by(|a, b| a.0.cmp(&b.0));
                        Files::Listed(std::sync::Arc::from(listed))
                    }
                }
            }
            blaze_world::Origin::Toolchain(_) => Files::Listed(none.clone()),
        };
        env.sort();
        let package = blaze_proc_macro::sandbox::Package { files, env };
        packages.push(std::sync::Arc::new(package));
        let blaze_world::Origin::Unit(uid) = c.origin else { continue };
        if c.proc_macro {
            macro_units.push((i, &graph.units[uid.0 as usize]));
        }
    }
    let path_dir = |id: LockId| paths.iter().find(|(p, _)| *p == id).map(|(_, dir)| root.join(dir));
    let made = blaze_world::procs::register_macro_crates(&mut procs, store, lock, lock_text, macro_units, path_dir).unwrap_or_else(|e| panic!("{e}"));
    for (i, why) in &made.unregistered {
        println!("procs         {} {}: not registered: {why:?}", world.crates[*i].package, world.crates[*i].version);
    }
    for (i, key) in &made.keys {
        keys[*i] = Some(*key);
    }
    let registered = made.keys.len();
    let with_module = procs.crates().filter(|c| c.2).count();
    println!("procs         {registered} macro crates registered, {with_module} with a module, store {}", store.display());
    let editions = world.crates.iter().map(|c| c.input.edition).collect();
    // What each macro crate's module declares to the host: its exports, in place of its text's
    // attributes (a derive declared through `synstructure::decl_derive!` has none written).
    // Copied out once (the hook owns the records) and leaked with the rest of `--procs`.
    let with: std::collections::BTreeSet<blaze_id::CrateItemKey> = procs.crates().filter(|c| c.2).map(|c| c.0).collect();
    let exports = keys
        .iter()
        .map(|key| {
            let key = (*key).filter(|k| with.contains(k))?;
            let list: Vec<blaze_scope::task::ProcExport<'static>> = procs
                .exports(key)
                .iter()
                .map(|e| blaze_scope::task::ProcExport { kind: e.kind, name: String::leak(e.name.clone()), helpers: String::leak(e.helpers.join(", ")) })
                .collect();
            Some(&*Vec::leak(list))
        })
        .collect();
    Procs { procs, keys, editions, packages, exports }
}

// ---------------------------------------------------------------------------------------------
// Counting.

#[derive(Default, Clone)]
struct Counts {
    modules: usize,
    imports: usize,
    unresolved: usize,
    macro_paths: usize,
    macro_unresolved: usize,
}

/// Cause -> (count, one place).
type Causes = BTreeMap<String, (usize, String)>;

fn add(map: &mut Causes, cause: String, place: impl FnOnce() -> String) {
    let e = map.entry(cause).or_insert_with(|| (0, place()));
    e.0 += 1;
}

fn show_path(path: &Path<'_, N>) -> String {
    let start = match path.kind {
        PathKind::Plain => String::new(),
        PathKind::Crate => "crate::".to_string(),
        PathKind::SelfMod => "self::".to_string(),
        PathKind::Super(n) => "super::".repeat(usize::from(n)),
        PathKind::Abs => "::".to_string(),
        PathKind::DollarCrate(_) => "$crate::".to_string(),
    };
    format!("{start}{}", path.segments.iter().copied().collect::<Vec<_>>().join("::"))
}

fn at(c: &CrateNames<'static>, loc: Loc) -> String {
    c.line_of(loc).map_or_else(|| format!("{}: file {}", c.name, loc.file), |(file, line)| format!("{}:{line}", file.shown))
}

/// Facts of a crate the causes are told by.
struct Facts {
    proc_macro: bool,
    script_missing: bool,
    /// An `include!` of an `OUT_DIR` file is called in it.
    out_dir: bool,
    /// `cfg_if!` is called in it.
    cfg_if: bool,
    /// Its extern names, whether each names a proc-macro crate, and that crate.
    deps: Vec<(N, bool, usize)>,
}

/// How a name is written in a crate's text (a heuristic over its files' lines, for the causes
/// only): defined by an item with a non-builtin attribute before it and no derive (an attribute
/// proc macro's input, which the names library does not see until it is expanded), defined
/// plainly, or not written as an item at all (made by a macro or a build script).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Def {
    UnderProcAttr,
    Plain,
    NotWritten,
}

/// Attributes that are not a proc macro's.
const INERT: &[&str] = &[
    "derive", "cfg", "cfg_attr", "doc", "allow", "warn", "deny", "expect", "forbid", "inline", "must_use", "repr", "non_exhaustive", "deprecated", "test", "path",
    "macro_export", "macro_use", "track_caller", "cold", "automatically_derived", "rustfmt", "clippy", "no_mangle", "export_name", "link", "link_name", "used",
    "target_feature", "doc_cfg", "rustc_diagnostic_item", "diagnostic", "stable", "unstable", "rustc_const_stable", "rustc_const_unstable", "ignore", "should_panic",
    "serde", "error", "from", "source", "backtrace", "default", "arg", "command", "clap", "value", "rlp", "builder", "strum", "display", "with", "transparent",
];

const ITEM_WORDS: &[&str] = &["struct", "enum", "trait", "fn", "type", "const", "static", "union", "mod", "macro_rules"];

/// Each crate's defined names, made on first ask.
#[derive(Default)]
struct Defs(std::collections::HashMap<usize, std::collections::HashMap<&'static str, Def>>);

impl Defs {
    fn of(&mut self, names: &blaze_scope::resolve::Names<'static>, krate: usize, name: &str) -> Def {
        let map = self.0.entry(krate).or_insert_with(|| {
            let mut map = std::collections::HashMap::new();
            for file in names.krate(krate).files.iter() {
                let text: &'static str = file.text;
                let (mut proc_attr, mut derive) = (false, false);
                for line in text.lines() {
                    let line = line.trim_start();
                    if line.is_empty() || line.starts_with("//") {
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("#[") {
                        let attr: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':').collect();
                        let last = attr.rsplit("::").next().unwrap_or("");
                        if last == "derive" {
                            derive = true;
                        } else if !INERT.contains(&last) {
                            proc_attr = true;
                        }
                        continue;
                    }
                    let words: Vec<&'static str> = text_words(line);
                    for w in 0..words.len().saturating_sub(1) {
                        if ITEM_WORDS.contains(&words[w]) {
                            let def = if proc_attr && !derive { Def::UnderProcAttr } else { Def::Plain };
                            let e = map.entry(words[w + 1]).or_insert(def);
                            if def == Def::Plain {
                                *e = Def::Plain;
                            }
                            break;
                        }
                    }
                    proc_attr = false;
                    derive = false;
                }
            }
            map
        });
        map.get(name).copied().unwrap_or(Def::NotWritten)
    }
}

/// The identifier words of a line of a `'static` text.
fn text_words(line: &'static str) -> Vec<&'static str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).take(8).collect()
}

fn def_cause(def: Def) -> &'static str {
    match def {
        Def::UnderProcAttr => "the item is under an attribute proc macro (not expanded yet)",
        Def::NotWritten => "not written as an item in that crate's text (a macro's or a build script's output)",
        Def::Plain => "REAL MISS? written plainly",
    }
}

#[allow(clippy::too_many_arguments)]
/// One unresolved import, for the list and the table of roots.
struct Miss {
    krate: usize,
    module: String,
    place: String,
    path: String,
    /// The path's segments up to and including the failing one.
    prefix: String,
    /// The failing segment.
    seg: &'static str,
    /// The name the import would bind (its last segment; `*` for a glob).
    binds: &'static str,
    /// The crate the failing segment was looked up in.
    owner: usize,
    why: String,
    cause: String,
}

/// Each miss's root: the first unresolved name it depends on, followed through the misses of
/// the crate it was looked up in (an import there that would have bound the name, else a failed
/// glob import there), iteratively, at most `misses.len()` steps.
fn roots_of(misses: &[Miss]) -> Vec<usize> {
    let mut binds: BTreeMap<(usize, &str), usize> = BTreeMap::new();
    let mut globs: BTreeMap<usize, usize> = BTreeMap::new();
    for (i, m) in misses.iter().enumerate() {
        if m.binds == "*" {
            globs.entry(m.krate).or_insert(i);
        } else {
            binds.entry((m.krate, m.binds)).or_insert(i);
        }
    }
    let next = |i: usize| -> Option<usize> {
        let m = &misses[i];
        let by_name = binds.get(&(m.owner, m.seg)).copied().filter(|&j| j != i);
        by_name.or_else(|| globs.get(&m.owner).copied().filter(|&j| j != i && m.owner != m.krate))
    };
    (0..misses.len())
        .map(|i| {
            let mut at = i;
            for _ in 0..misses.len() {
                match next(at) {
                    Some(j) if j != i => at = j,
                    _ => break,
                }
            }
            at
        })
        .collect()
}

#[allow(clippy::too_many_arguments, reason = "an example's counting pass")]
#[allow(clippy::too_many_arguments, reason = "the counting's inputs and its three outputs")]
fn count(names: &blaze_scope::resolve::Names<'static>, inners: Option<&[blaze_scope::crate_scope::CrateScope<'static>]>, here: usize, defs: &mut Defs, f: &Facts, pkg: &str, causes: &mut Causes, misses: &mut Vec<Miss>) -> Counts {
    let c = names.krate(here);
    let mut n = Counts { modules: c.tree.modules.len() + c.made.len(), ..Counts::default() };
    for (_, d) in c.decls.values() {
        n.imports += d.uses.len();
        n.macro_paths += d.calls.len();
    }
    for e in c.expansions.values() {
        n.imports += e.decls.uses.len();
        n.macro_paths += e.decls.calls.len();
    }
    for m in c.made.values() {
        n.imports += m.decls.uses.len();
        n.macro_paths += m.decls.calls.len();
    }
    let use_loc = |fragment, index: u32| c.tables.use_locs.get(&fragment).and_then(|locs: &AVec<'_, Loc>| locs.get(index as usize).copied());
    for r in c.done.values() {
        let mpath = c.path_of(r.scope.module).unwrap_or_default().to_string();
        for error in r.scope.errors.iter() {
            match error {
                ScopeError::Import { site, path, segment, why } => {
                    n.unresolved += 1;
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    let place = use_loc(fragment, site.index).map_or_else(|| mpath.clone(), |loc| at(c, loc));
                    let seg = path.segments.get(*segment as usize).copied().unwrap_or("");
                    // The crate the missing name is looked for in: the crate of what the path
                    // before it names, followed through every alias (`extern crate
                    // librocksdb_sys as ffi`: `ffi::x` is looked for in librocksdb-sys); a first
                    // segment is looked for here.
                    let before = &path.segments[..(*segment as usize).min(path.segments.len())];
                    let at_owner = match before.is_empty() {
                        true => None,
                        false => names.item_at(r.scope.module, blaze_surface::Namespace::Types, blaze_scope::names::Written { kind: path.kind, segments: before }, Alloc::global()).ok(),
                    };
                    let owner = at_owner.as_ref().map_or(here, |it| it.krate);
                    // Only now, the lookup having failed: another crate's inner record says
                    // whether the name is there and private.
                    let why = match (inners, &at_owner) {
                        (Some(inners), Some(it)) if owner != here => blaze_scope::crate_scope::why_reported(*why, &inners[owner].inner, it.def.key, seg, Alloc::global())
                            .unwrap_or_else(|e| panic!("crate {owner}'s inner record (asked for `{seg}` of `{}`): {e:?}", show_path(path))),
                        _ => *why,
                    };
                    let cause = if f.proc_macro {
                        format!("in a proc-macro crate (its host-side dependencies are not held): import {why:?}")
                    } else if f.out_dir && f.script_missing && owner == here {
                        format!("crate includes an OUT_DIR file its build script writes (not run): import `{seg}` {why:?}")
                    } else if f.cfg_if && *segment == 0 && owner == here {
                        format!("crate calls cfg_if!: first segment `{seg}` {why:?}")
                    } else {
                        let def = defs.of(names, owner, seg);
                        let whose = if owner == here { "this crate" } else { "a dependency" };
                        format!("{} [{whose}]: `{seg}` of `{}` ({why:?})", def_cause(def), show_path(path))
                    };
                    if !f.proc_macro {
                        let end = (*segment as usize + 1).min(path.segments.len());
                        misses.push(Miss {
                            krate: here,
                            module: mpath.clone(),
                            place: place.clone(),
                            path: show_path(path),
                            prefix: path.segments[..end].join("::"),
                            seg,
                            binds: path.segments.last().copied().unwrap_or(""),
                            owner,
                            why: format!("{why:?}"),
                            cause: cause.clone(),
                        });
                    }
                    add(causes, format!("[{pkg}] {cause}"), || format!("{place}: use {}", show_path(path)));
                }
                ScopeError::Macro { call, path, why, .. } => {
                    n.macro_unresolved += 1;
                    let src = c.tables.calls.get(call);
                    let place = src.map_or_else(|| mpath.clone(), |src| at(c, src.loc));
                    let last = path.segments.last().copied().unwrap_or("");
                    let first = path.segments.first().copied().unwrap_or("");
                    let shape = src.map(|it| it.kind);
                    let from_proc = f.deps.iter().any(|(name, pm, _)| *pm && *name == first);
                    let cause = if f.proc_macro {
                        format!("in a proc-macro crate (its host-side dependencies are not held): macro {why:?}")
                    } else if from_proc || (matches!(shape, Some(CallShape::Derive | CallShape::Attr)) && !BuiltinDerive::ALL.into_iter().any(|it| it.name() == last)) {
                        format!("a proc macro's (not expanded yet): {} `{last}`", match shape {
                            Some(CallShape::Derive) => "derive",
                            Some(CallShape::Attr) => "attribute",
                            _ => "macro",
                        })
                    } else if last == "cfg_if" && f.cfg_if {
                        "cfg_if! itself not in scope where it is called".to_string()
                    } else if path.segments.len() > 1 && f.deps.iter().any(|(name, _, _)| *name == first) {
                        format!("names a dependency's macro not found there: `{last}` in `{first}::..` ({why:?})")
                    } else {
                        format!("REAL MISS? macro {why:?}: `{last}`")
                    };
                    add(causes, format!("[{pkg}] {cause}"), || format!("{place}: {}!", show_path(path)));
                }
                ScopeError::ExternCrate { name } => {
                    let cause = if f.cfg_if { "extern crate written in a crate that calls cfg_if!".to_string() } else { format!("REAL MISS? extern crate `{name}` not found") };
                    add(causes, format!("[{pkg}] {cause}"), || format!("{mpath}: extern crate {name}"));
                }
                _ => {}
            }
        }
    }
    n
}

/// Strip `[pkg] ` to the cause's kind, for the totals.
fn kind_of(cause: &str) -> &str {
    let rest = cause.split_once("] ").map_or(cause, |(_, r)| r);
    let rest = rest.split_once(": `").map_or(rest, |(k, _)| k);
    rest.split_once(" `").map_or(rest, |(k, _)| k)
}

// ---------------------------------------------------------------------------------------------
// The subset.

/// The graph's units in the closure of `roots`' library units, by held edges; ids remapped.
fn subset(graph: &UnitGraph<'static>, roots: &[String]) -> &'static UnitGraph<'static> {
    let units = &graph.units;
    let mut keep = vec![false; units.len()];
    let mut todo: Vec<usize> = units.iter().enumerate().filter(|(_, u)| u.kind == UnitKind::Lib && !u.for_host && roots.iter().any(|r| r == u.name.as_str())).map(|(i, _)| i).collect();
    while let Some(u) = todo.pop() {
        if keep[u] {
            continue;
        }
        keep[u] = true;
        if units[u].kind != UnitKind::ProcMacro {
            todo.extend(units[u].deps.iter().map(|d| d.unit.index()));
        }
    }
    let mut at = vec![u32::MAX; units.len()];
    let mut kept = AVec::new_in(g());
    for (i, u) in units.iter().enumerate().filter(|(i, _)| keep[*i]) {
        at[i] = kept.len() as u32;
        kept.push(u.clone());
    }
    for u in kept.iter_mut() {
        if u.kind == UnitKind::ProcMacro {
            u.deps.clear();
        }
        for d in u.deps.iter_mut() {
            d.unit = UnitId(at[d.unit.index()]);
        }
        u.build_script = u.build_script.map(|_| UnitId(u32::MAX));
    }
    Box::leak(Box::new(UnitGraph { units: kept, unavailable: AVec::new_in(g()), resolver: graph.resolver }))
}

/// `--owners`: each owner's heap bytes now and at its own peak; the default zone's are the
/// footprint's rest (with mapped pages written and the runtime's own mappings).
fn print_owners() {
    println!("  heap by owner (MB in use now / at the owner's peak / blocks now):");
    for (at, s) in owners::stats().iter().enumerate() {
        if let Some(s) = s {
            println!("    {:>9.1} / {:>9.1} / {:>10}  {}", s.size_in_use as f64 / 1e6, s.max_size_in_use as f64 / 1e6, s.blocks_in_use, owners::NAMES[at]);
        }
    }
    println!("    footprint now {:.1} MB (the rest: the default zone's untagged blocks, written mappings, the macro runtime's mappings)", now_footprint() as f64 / 1e6);
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(store)) = (args.get(1).map(PathBuf::from), args.get(2).map(PathBuf::from)) else {
        eprintln!("usage: names <workspace dir> <blaze-fetch store folder> [--threads 1,18] [--roots a,b] [--crates] [--tests: each member's library also as a test unit, `test` on, dev-dependencies in scope]");
        std::process::exit(2);
    };
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let threads: Vec<usize> = flag("--threads").unwrap_or_else(|| "1,18".into()).split(',').map(|t| t.parse().expect("a thread count")).collect();
    let roots: Option<Vec<String>> = flag("--roots").map(|r| r.split(',').map(str::to_string).collect());
    let per_crate = args.iter().any(|a| a == "--crates");
    let by_owner = args.iter().any(|a| a == "--owners");
    if by_owner {
        owners::enable();
    }

    // The graph.
    let t0 = Instant::now();
    let lock_bytes: &'static [u8] = Box::leak(std::fs::read(root.join("Cargo.lock")).expect("Cargo.lock").into_boxed_slice());
    let lock: &'static Lock<'static> = Box::leak(Box::new(Lock::read(lock_bytes, g()).unwrap_or_else(|e| panic!("Cargo.lock: {e}"))));
    let ws = layout(&root);
    let written = ws.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, ws.root_edition).unwrap_or_else(|_| panic!("unknown resolver {written:?}"));
    let mut roots_ids = AVec::new_in(g());
    let mut members = Vec::new();
    let mut paths: Vec<(LockId, &'static str)> = Vec::new();
    for m in ws.members.iter() {
        let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) else {
            println!("member {} is not in Cargo.lock as a path package", m.name.as_str());
            continue;
        };
        roots_ids.push(id);
        members.push((id, Package::from_member(m, g())));
        paths.push((id, Box::leak(m.dir.as_str().to_string().into_boxed_str())));
    }
    let ask: &'static Ask<'static> = Box::leak(Box::new(Ask { roots: roots_ids, features: AVec::new_in(g()), all_features: false, no_default_features: false, tests: args.iter().any(|a| a == "--tests") }));
    let target: &'static TargetDesc<'static> = Box::leak(Box::new(mac()));
    let mut res = Resolution::new(lock, target, target, ask, resolver, g());
    for (id, p) in members {
        res.member(id, p).unwrap_or_else(|e| panic!("{e}"));
    }
    let sources: &'static mut StoreSources<'static, 'static, 'static, Folder> =
        Box::leak(Box::new(StoreSources::new(lock, Folder { records: store.join("records") }, PackView::open_source_put, g()).unwrap_or_else(|e| panic!("{e}"))));
    let whole: &'static UnitGraph<'static> = Box::leak(Box::new(loop {
        match res.pass().unwrap_or_else(|e| panic!("{e}")) {
            Pass::Done(graph) => break graph,
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let facts = sources.package(*id).unwrap_or_else(|e| panic!("{e}"));
                    res.package(*id, Some(facts)).unwrap_or_else(|e| panic!("{e}"));
                }
            }
        }
    }));
    let graph_time = t0.elapsed();
    let graph = match &roots {
        Some(r) => subset(whole, r),
        None => whole,
    };

    // The toolchain and the world.
    let t1 = Instant::now();
    let home = std::env::var("HOME").expect("HOME");
    let needs = blaze_world::needs(graph);
    let tc: &'static Toolchain<'static, &'static [u8]> = Box::leak(Box::new(
        Toolchain::open(&mut DirShelf(format!("{home}/.blaze/v2/toolchain")), blaze_toolchain::PINNED, "aarch64-apple-darwin", needs, g()).unwrap_or_else(|e| panic!("{e}")),
    ));
    let tc_time = t1.elapsed();
    let checkout: &'static Checkout = Box::leak(Box::new(Checkout::new(root.clone())));
    let t2 = Instant::now();
    let mut packages = Fetched { store: sources, paths: &paths, lock, alloc: g() };
    // `--scripts <dir>`: the build scripts' outputs records blaze-proc-macro's `tempo_scripts
    // export` wrote, `<name>@<version>@<host|target>.outputs`, each by the unit it serves.
    let records: Option<&'static blaze_world::ScriptRecords<'static>> = flag("--scripts").map(|dir| {
        let mut given: Vec<(UnitId, &'static [u8])> = Vec::new();
        for (at, unit) in graph.units.iter().enumerate() {
            if unit.build_script.is_none() || !matches!(unit.kind, UnitKind::Lib | UnitKind::ProcMacro) {
                continue;
            }
            let side = if unit.for_host { "host" } else { "target" };
            let path = PathBuf::from(&dir).join(format!("{}@{}@{side}.outputs", unit.name.as_str(), unit.version.as_str()));
            match std::fs::read(&path) {
                Ok(bytes) => given.push((UnitId(at as u32), Box::leak(bytes.into_boxed_slice()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("{}: {e}", path.display()),
            }
        }
        let records = blaze_world::ScriptRecords::new(given, g()).unwrap_or_else(|e| panic!("{e}"));
        println!("scripts       {} units given their build script's outputs from {dir}", records.len());
        &*Box::leak(Box::new(records))
    });
    let scripts = records.as_ref().map(|r| r as &dyn blaze_world::ScriptOutputs<'static>);
    let world: &'static World<'static> = Box::leak(Box::new(blaze_world::world(graph, lock, tc, &mut packages, checkout, scripts, g()).unwrap_or_else(|e| panic!("{e}"))));
    let world_time = t2.elapsed();
    for used in world.corrections.iter() {
        println!("{used}");
    }
    // `--procs <module store> --procs-out <dir>`: proc macros run through blaze-proc-macro's hook.
    let procs: Option<&'static Procs> = flag("--procs").map(|store| {
        let out = flag("--procs-out").unwrap_or_else(|| panic!("--procs needs --procs-out <dir>"));
        let lock_text = std::str::from_utf8(lock_bytes).expect("Cargo.lock is UTF-8");
        let procs: &'static Procs = Box::leak(Box::new(procs_of(FsPath::new(&store), FsPath::new(&out), &root, world, graph, lock, lock_text, &paths, &mut packages)));
        // Every module compiled or loaded from its compiled record before resolution starts:
        // no worker waits on a compile. With `--records` a module is loaded by the first call
        // a solved crate makes ([`Runtime::load`]): a found crate calls nothing, so a repeat
        // with nothing changed loads none (r19-b: 697.8 ms of a 1069.7 ms run, 0 crates solved).
        if flag("--records").is_some() {
            return procs;
        }
        let workers = threads.iter().copied().max().unwrap_or(1);
        let started = std::time::Instant::now();
        let compiled = procs.procs.load_all(workers).unwrap_or_else(|e| panic!("the proc-macro modules: {e}"));
        println!("procs         {} modules ready on {workers} workers in {:.1} ms, {compiled} compiled", procs.procs.crates().filter(|c| c.2).count(), started.elapsed().as_secs_f64() * 1e3);
        procs
    });
    let mut bytes = AVec::new_in(g());
    world.write(&mut bytes);
    let digest = blaze_id::ContentId::of("blaze world example inputs", &bytes);
    let held_units = world.crate_of_unit.iter().filter(|c| c.is_some()).count();
    let missing = world.crates.iter().filter(|c| c.script == Script::Missing).count();
    let pms = world.crates.iter().filter(|c| c.proc_macro).count();
    println!("workspace     {}", root.display());
    println!("graph         {} units ({} of them crates of the names set), {:.1} ms", graph.units.len(), held_units, ms(graph_time));
    println!(
        "world         {} crates ({} toolchain, {} proc macros, {} resolved without their build script's outputs), toolchain {:.1} ms, inputs {:.1} ms, inputs digest {:02x}{:02x}{:02x}{:02x}",
        world.crates.len(),
        world.toolchain_crates,
        pms,
        missing,
        ms(tc_time),
        ms(world_time),
        digest.0[0],
        digest.0[1],
        digest.0[2],
        digest.0[3]
    );
    if records.is_some() {
        for c in world.crates.iter().filter(|c| c.script == Script::Missing) {
            println!("  without     {} {} ({})", c.package, c.version, c.input.name);
        }
    }

    let shuffle = args.iter().any(|a| a == "--shuffle");
    let kept_dir: Option<PathBuf> = flag("--records").map(PathBuf::from);
    if let Some(dir) = &kept_dir {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("--records {}: {e}", dir.display()));
    }
    let mut first_records: Option<Vec<(Vec<u8>, Vec<u8>)>> = None;
    for (run, &t) in threads.iter().enumerate() {
        let texts = Timed {
            inner: &world.texts,
            procs,
            start: Instant::now(),
            first: (0..world.crates.len()).map(|_| AtomicU64::new(0)).collect(),
            lines: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            files: AtomicU64::new(0),
            read: (0..world.crates.len()).map(|_| std::sync::Mutex::new(Vec::new())).collect(),
        };
        let texts: &'static Timed<'static> = Box::leak(Box::new(texts));
        let inputs: &'static [CrateInput<'static>] = world.inputs(g()).leak();
        if by_owner {
            println!("threads {t}: before the resolve: footprint {:.1} MB, its peak so far {:.1} MB", now_footprint() as f64 / 1e6, max_footprint() as f64 / 1e6);
            print_owners();
        }
        let start = Instant::now();
        // `--crates`: crate by crate through records on a ready queue of `t` workers, each
        // crate's time from its start to its end.
        let clock = Clock {
            start,
            spans: (0..world.crates.len()).map(|_| (AtomicU64::new(0), AtomicU64::new(0))).collect(),
            decodes: (0..world.crates.len()).map(|_| AtomicU64::new(0)).collect(),
            decoded_bytes: AtomicU64::new(0),
            handed: (0..world.crates.len()).map(|_| AtomicU64::new(0)).collect(),
        };
        let mut by_records = None;
        // `--shuffle`: after the first thread count, ready crates are taken in a shuffled order
        // (a fixed permutation per run): the records must not change.
        let rank: Option<Vec<u32>> = (shuffle && run > 0).then(|| {
            let mut r: Vec<u32> = (0..world.crates.len() as u32).collect();
            let mut s = 0x9e37_79b9_7f4a_7c15u64 ^ run as u64;
            for i in (1..r.len()).rev() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                r.swap(i, (s % (i as u64 + 1)) as usize);
            }
            r
        });
        // `--records <folder>`: found or solved, crate by crate; the counts below need every
        // crate's tables, so this run prints what was found and solved and the times only.
        if let Some(dir) = kept_dir.as_ref() {
            let kept = KeptDir {
                dir: dir.clone(),
                root: root.clone(),
                stamps: !args.iter().any(|a| a == "--no-stamps"),
                texts: OnGlobal(texts),
                inputs,
                by_key: inputs.iter().enumerate().map(|(i, c)| (c.key, i)).collect(),
                found: AtomicUsize::new(0),
                find_ns: AtomicU64::new(0),
                outlined: AtomicU64::new(0),
                missed: (0..inputs.len()).map(|_| std::sync::Mutex::new(None)).collect(),
            };
            let ready = blaze_scope::crate_scope::Ready { workers: t, rank: rank.as_deref(), wait: Duration::from_secs(1200), hooks: &clock };
            let all = blaze_scope::crate_scope::resolve_kept(inputs, world.sysroot, texts, &Threads(t), &ready, &kept, g());
            let elapsed = start.elapsed();
            let (all, needs) = match all {
                Ok(all) => all,
                Err((at, e)) => {
                    println!("threads {t}: no names after {:.1} ms: crate `{}`: {e:?}", ms(elapsed), inputs[at].name);
                    continue;
                }
            };
            let solved: Vec<usize> = (0..all.len()).filter(|&i| matches!(all[i], blaze_scope::crate_scope::Solved::Solved(_))).collect();
            let work: u64 = clock.spans.iter().map(|(b, e)| e.load(Ordering::Relaxed).saturating_sub(b.load(Ordering::Relaxed))).sum();
            let solve_work: u64 = solved.iter().map(|&i| clock.spans[i].1.load(Ordering::Relaxed).saturating_sub(clock.spans[i].0.load(Ordering::Relaxed))).sum();
            println!(
                "\nthreads {t}: records {}: {} crates in {:.1} ms: found {}, solved {}; work {:.1} ms (solving {:.1} ms; finding {:.1} ms, {} checkout files outlined); files read by solves {}; Need re-entries {needs}; peak footprint so far {:.1} MB",
                dir.display(),
                all.len(),
                ms(elapsed),
                kept.found.load(Ordering::Relaxed),
                solved.len(),
                work as f64 / 1e6,
                solve_work as f64 / 1e6,
                kept.find_ns.load(Ordering::Relaxed) as f64 / 1e6,
                kept.outlined.load(Ordering::Relaxed),
                texts.files.load(Ordering::Relaxed),
                max_footprint() as f64 / 1e6
            );
            let mut whys: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for &i in &solved {
                let why = kept.missed[i].lock().unwrap_or_else(|p| panic!("miss lock poisoned: {p}")).clone().unwrap_or_else(|| "not asked".into());
                let kind = why.split('`').next().unwrap_or("").trim().to_string();
                whys.entry(if kind.is_empty() { why } else { kind }).or_default().push(inputs[i].name);
            }
            for (why, crates) in &whys {
                println!("  solved because {why}: {} (e.g. {})", crates.len(), crates.iter().take(12).copied().collect::<Vec<_>>().join(", "));
            }
            continue;
        }
        let names = if per_crate {
            let ready = blaze_scope::crate_scope::Ready { workers: t, rank: rank.as_deref(), wait: Duration::from_secs(1200), hooks: &clock };
            match blaze_scope::crate_scope::resolve_by_records(inputs, world.sysroot, texts, &Threads(t), &ready, g()) {
                Ok((all, needs)) => {
                    let each: Vec<&'static CrateNames<'static>> = all.iter().map(|c| c.names).collect();
                    let names = blaze_scope::resolve::Names::of_crates(inputs, world.sysroot, &each, g()).map_err(|e| e.to_string());
                    by_records = Some((all, needs));
                    names
                }
                Err((at, e)) => Err(format!("crate `{}`: {e:?}", inputs[at].name)),
            }
        } else {
            blaze_scope::resolve::resolve_crates(inputs, world.sysroot, texts, &Threads(t), g()).map_err(|e| e.to_string())
        };
        let elapsed = start.elapsed();
        let names = match names {
            Ok(names) => names,
            Err(e) => {
                println!("threads {t}: no names after {:.1} ms: {e}", ms(elapsed));
                continue;
            }
        };
        if let Some(p) = procs {
            let findings = p.procs.take_findings();
            let mut by_kind: BTreeMap<String, (usize, String)> = BTreeMap::new();
            for f in &findings {
                let kind = format!("{f:?}").split([' ', '{', '(']).next().unwrap_or("").to_string();
                by_kind.entry(kind).or_insert_with(|| (0, f.to_string())).0 += 1;
            }
            println!("proc findings {} in all", findings.len());
            for (kind, (n, first)) in &by_kind {
                println!("  {n:>6}  {kind}: e.g. {}", first.chars().take(200).collect::<String>());
            }
        }
        {
            // The library's notes, by kind (an expansion's by its note's kind): what was refused.
            let mut by_kind: BTreeMap<String, (usize, String)> = BTreeMap::new();
            for (i, c) in world.crates.iter().enumerate() {
                for note in names.notes(i) {
                    let text = format!("{note:?}");
                    let kind = match text.find("note: ") {
                        Some(at) => format!("Expand {}", text[at + 6..].split([' ', '{', '(']).next().unwrap_or("")),
                        None => text.split([' ', '{', '(']).next().unwrap_or("").to_string(),
                    };
                    by_kind.entry(kind).or_insert_with(|| (0, format!("[{}] {}", c.package, text.chars().take(300).collect::<String>()))).0 += 1;
                }
            }
            println!("notes by kind:");
            for (kind, (n, first)) in &by_kind {
                println!("  {n:>6}  {kind}: e.g. {first}");
            }
            // `--notes-of a,b`: every note of these packages, whole.
            if let Some(of) = flag("--notes-of") {
                for (i, c) in world.crates.iter().enumerate() {
                    if of.split(',').any(|p| p == c.package) {
                        for note in names.notes(i) {
                            println!("  note [{}] {note:?}", c.package);
                        }
                    }
                }
            }
        }
        let end_ns = texts.start.elapsed().as_nanos() as u64;
        // Each crate's time: from its first read to the next first read after it.
        let mut firsts: Vec<(u64, usize)> = texts.first.iter().enumerate().map(|(i, f)| (f.load(Ordering::Relaxed), i)).filter(|(f, _)| *f != 0).collect();
        firsts.sort();
        let mut took = vec![0u64; world.crates.len()];
        for w in 0..firsts.len() {
            let next = firsts.get(w + 1).map_or(end_ns, |it| it.0);
            took[firsts[w].1] = next - firsts[w].0;
        }
        if by_records.is_some() {
            for (i, (b, e)) in clock.spans.iter().enumerate() {
                took[i] = e.load(Ordering::Relaxed).saturating_sub(b.load(Ordering::Relaxed));
            }
        }
        let mut total = Counts::default();
        // Inside proc-macro crates: not asked (their host-side dependencies are not held: the
        // bootstrap tool compiles them today), so not counted as unresolved.
        let mut in_macro_crates = Counts::default();
        let mut causes = Causes::new();
        let mut misses: Vec<Miss> = Vec::new();
        let mut defs = Defs::default();
        let mut rows = Vec::new();
        // Each crate: whether it includes an `OUT_DIR` file its build script cannot write yet.
        let mut waits: Vec<bool> = Vec::with_capacity(world.crates.len());
        for (i, c) in world.crates.iter().enumerate() {
            let cn = names.krate(i);
            let mut facts = Facts { proc_macro: c.proc_macro, script_missing: c.script == Script::Missing, out_dir: false, cfg_if: false, deps: Vec::new() };
            for call in cn.tables.calls.values() {
                facts.out_dir |= call.name == "include" && call.input.contains("OUT_DIR");
                facts.cfg_if |= call.name == "cfg_if";
            }
            facts.deps = c.input.deps.iter().map(|(n, d)| (*n, world.crates[*d].proc_macro, *d)).collect();
            let tag = if (i as u32) < world.toolchain_crates { format!("toolchain {}", c.package) } else { c.package.to_string() };
            waits.push(facts.out_dir && facts.script_missing);
            let n = count(&names, by_records.as_ref().map(|(all, _)| &all[..]), i, &mut defs, &facts, &tag, &mut causes, &mut misses);
            let into = if c.proc_macro { &mut in_macro_crates } else { &mut total };
            into.modules += n.modules;
            into.imports += n.imports;
            into.unresolved += n.unresolved;
            into.macro_paths += n.macro_paths;
            into.macro_unresolved += n.macro_unresolved;
            rows.push((took[i], c.package, c.version, n));
        }
        println!("\nthreads {t}: {} crates resolved in {:.1} ms; files read {}, {} lines, {:.1} MB; peak footprint so far {:.1} MB", world.crates.len(), ms(elapsed), texts.files.load(Ordering::Relaxed), texts.lines.load(Ordering::Relaxed), texts.bytes.load(Ordering::Relaxed) as f64 / 1e6, max_footprint() as f64 / 1e6);
        if by_owner {
            print_owners();
        }
        println!("  total: modules {}, imports {}, unresolved imports {}, macro paths {}, unresolved macro paths {}", total.modules, total.imports, total.unresolved, total.macro_paths, total.macro_unresolved);
        println!(
            "  not asked: inside macro crates: modules {}, imports {} ({} not found), macro paths {} ({} not found)",
            in_macro_crates.modules, in_macro_crates.imports, in_macro_crates.unresolved, in_macro_crates.macro_paths, in_macro_crates.macro_unresolved
        );
        if let Some((all, needs)) = &by_records {
            // The work, and the floor of the wall time at any worker count: the longest chain
            // of crates each waiting on the one before (its dependencies and the sysroot crates
            // before it in the graph's order, as the ready queue has it).
            let order = names.graph().order();
            let mut pos = vec![0usize; order.len()];
            for (p, id) in order.iter().enumerate() {
                pos[id.index()] = p;
            }
            let sys: Vec<usize> = world.sysroot.all().into_iter().flatten().map(|s| s.index()).collect();
            let mut path = vec![0u64; order.len()];
            let mut via = vec![usize::MAX; order.len()];
            for id in order {
                let at = id.index();
                let waits = inputs[at].deps.iter().map(|&(_, d)| d).chain(sys.iter().copied().filter(|&s| s != at && pos[s] < pos[at]));
                let best = waits.max_by_key(|&d| (path[d], usize::MAX - d));
                path[at] = took[at] + best.map_or(0, |d| path[d]);
                via[at] = best.unwrap_or(usize::MAX);
            }
            let sum: u64 = took.iter().sum();
            let end = (0..order.len()).max_by_key(|&i| path[i]).unwrap_or(0);
            let mut chain = Vec::new();
            let mut at = end;
            while at != usize::MAX {
                chain.push(format!("{} {:.0}", world.crates[at].package, took[at] as f64 / 1e6));
                at = via[at];
            }
            chain.reverse();
            println!("  ready queue ({t} workers): wall {:.1} ms; work (sum of per-crate times) {:.1} ms; critical path {:.1} ms over {} crates; Need re-entries {needs}", ms(elapsed), sum as f64 / 1e6, path[end] as f64 / 1e6, chain.len());
            println!("  critical path (crate ms): {}", chain.join(" > "));
            let decodes: u64 = clock.decodes.iter().map(|d| d.load(Ordering::Relaxed)).sum();
            let core = world.sysroot.core.map_or(0, |c| clock.decodes[c.index()].load(Ordering::Relaxed));
            let most = (0..clock.decodes.len()).max_by_key(|&i| clock.decodes[i].load(Ordering::Relaxed)).unwrap_or(0);
            let handed: u64 = clock.handed.iter().map(|d| d.load(Ordering::Relaxed)).sum();
            let core_handed = world.sysroot.core.map_or(0, |c| clock.handed[c.index()].load(Ordering::Relaxed));
            println!(
                "  record decodes: {decodes} over {} records, {:.1} MB decoded; core's {core}; most: {} {}; records handed to readers {handed} (core's {core_handed})",
                clock.decodes.len(),
                clock.decoded_bytes.load(Ordering::Relaxed) as f64 / 1e6,
                world.crates[most].package,
                clock.decodes[most].load(Ordering::Relaxed)
            );
            let (exports, inner): (usize, usize) = all.iter().fold((0, 0), |(e, i), c| (e + c.record.len(), i + c.inner.len()));
            println!("  records: exports {exports} bytes, inner {inner} bytes");
            let mut sizes: Vec<(usize, usize, usize)> = all.iter().enumerate().map(|(i, c)| (c.record.len() + c.inner.len(), c.record.len(), i)).collect();
            sizes.sort_by(|a, b| b.cmp(a));
            println!("  ten largest (exports + inner): {}", sizes.iter().take(10).map(|(_, e, i)| format!("{} {}+{}", world.crates[*i].package, e, all[*i].inner.len())).collect::<Vec<_>>().join(", "));
            // Busy workers over time: the mean of in-flight crates (work / wall).
            println!("  mean busy workers {:.2} of {t}", sum as f64 / 1e6 / ms(elapsed));
            let bytes: Vec<(Vec<u8>, Vec<u8>)> = all.iter().map(|c| (c.record.to_vec(), c.inner.to_vec())).collect();
            match &first_records {
                None => first_records = Some(bytes),
                Some(before) => {
                    let unequal: Vec<String> = before
                        .iter()
                        .zip(bytes.iter())
                        .enumerate()
                        .filter(|(_, (a, b))| a != b)
                        .map(|(i, (a, b))| format!("{} (exports {}, inner {})", world.crates[i].package, if a.0 == b.0 { "equal" } else { "DIFFER" }, if a.1 == b.1 { "equal" } else { "DIFFER" }))
                        .collect();
                    println!("  records unequal to the first run's ({} workers{}): {} of {} {:?}", t, if rank.is_some() { ", shuffled" } else { "" }, unequal.len(), bytes.len(), unequal);
                }
            }
        }
        let mut by_time = rows.clone();
        by_time.sort_by(|a, b| b.0.cmp(&a.0));
        println!("  slowest crates:");
        for (ns, pkg, ver, n) in by_time.iter().take(15) {
            println!("    {:>9.1} ms  {pkg} {ver}: modules {}, imports {}, unresolved {}, macro paths {}, unresolved {}", *ns as f64 / 1e6, n.modules, n.imports, n.unresolved, n.macro_paths, n.macro_unresolved);
        }
        // The solver's counters of the slowest crates (blaze-scope `FillStats`): where a fill's
        // time went, counted in the sans-IO core.
        let mut order: Vec<usize> = (0..world.crates.len()).collect();
        order.sort_by(|a, b| took[*b].cmp(&took[*a]));
        println!("  solver counters (slowest crates):");
        for &i in order.iter().take(5) {
            let r = names.krate(i).report;
            let f = r.fill;
            println!(
                "    {} {}: steps {}, reads {}, expansions {}, groups {} (largest {}), passes {}, relax rounds {} (queued {}), plans {} (imports {}, globs {}), lookups {}, agains {} (spared {}), touches {} (parks {}), member nodes {}, stalls {} (items {}, edges {}), events {}, wakes {}",
                world.crates[i].package, world.crates[i].version, r.steps, r.reads, r.expansions, r.groups, r.largest, f.passes, f.relax_rounds, f.relax_queued, f.plans, f.plan_imports, f.plan_globs, f.lookups, f.agains, f.agains_spared, f.touches, f.parks, f.member_nodes, f.stalls, f.stall_items, f.stall_edges, f.events, f.wakes
            );
        }
        if per_crate {
            println!("  per crate:");
            for (ns, pkg, ver, n) in &rows {
                println!("    {pkg} {ver}: {:.1} ms, modules {}, imports {}, unresolved {}, macro paths {}, unresolved {}", *ns as f64 / 1e6, n.modules, n.imports, n.unresolved, n.macro_paths, n.macro_unresolved);
            }
        }
        let mut kinds: BTreeMap<String, (usize, String)> = BTreeMap::new();
        for (cause, (n, place)) in &causes {
            let side = if cause.starts_with("[toolchain ") { "toolchain crates: " } else { "project's crates: " };
            let e = kinds.entry(format!("{side}{}", kind_of(cause))).or_insert_with(|| (0, format!("{cause}: {place}")));
            e.0 += n;
        }
        let mut kinds: Vec<_> = kinds.into_iter().collect();
        kinds.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        println!("  unresolved by cause:");
        for (kind, (n, example)) in &kinds {
            println!("    {n:>7}  {kind}\n             e.g. {example}");
        }
        let mut detail: Vec<_> = causes.iter().filter(|(c, _)| c.contains("REAL MISS")).collect();
        detail.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        println!("  possible real misses, largest first:");
        for (cause, (n, place)) in detail.iter().take(40) {
            println!("    {n:>7}  {cause}\n             at {place}");
        }
        // Every unresolved import (outside proc-macro crates), with its root: the first
        // unresolved name it depends on, followed through the crates it was looked up in.
        let roots = roots_of(&misses);
        let pkg = |k: usize| world.crates[k].package;
        println!("  unresolved imports ({}), crate | module | path | failing segment | reason | root:", misses.len());
        for (m, &r) in misses.iter().zip(&roots) {
            let root = &misses[r];
            println!(
                "    miss {} | {} | {} | {} | {} | {}  root: {} `{}` ({})  at {}",
                pkg(m.krate),
                m.module,
                m.path,
                m.seg,
                m.why,
                m.cause,
                pkg(root.owner),
                root.prefix,
                pkg(root.krate),
                m.place
            );
        }
        // Roots by (crate looked in, path up to the failing segment's module).
        let mut by_root: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();
        for &r in &roots {
            let root = &misses[r];
            // A first segment that failed is its own root (`addr2line`), else its module.
            let module = root.prefix.rsplit_once("::").map_or(root.prefix.as_str(), |(m, _)| m).to_string();
            let e = by_root.entry((pkg(root.owner).to_string(), module)).or_insert_with(|| (0, format!("{} in {}: {}", root.path, pkg(root.krate), root.cause)));
            e.0 += 1;
        }
        // Two columns, from the root's cause: a miss waits on a build script when its root is
        // looked up in (or written in) a crate that includes an `OUT_DIR` file its script cannot
        // write yet; the crate looked in is the one the path names, aliases followed (`count`).
        let waiting_root = |r: usize| {
            let root = &misses[r];
            waits[root.owner] || waits[root.krate]
        };
        let waiting = roots.iter().filter(|&&r| waiting_root(r)).count();
        let mut named: Vec<&str> = (0..world.crates.len()).filter(|&i| waits[i]).map(|i| world.crates[i].package).collect();
        named.sort_unstable();
        named.dedup();
        println!(
            "  unresolved imports in two columns: ours to fix {} | waiting on a build script that cannot run yet {} (named: {})",
            misses.len() - waiting,
            waiting,
            named.join(", ")
        );
        let mut by_root: Vec<_> = by_root.into_iter().collect();
        by_root.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        println!("  roots ({}), by imports taken down:", by_root.len());
        for ((owner, module), (n, example)) in &by_root {
            println!("    root {n:>5}  {owner} `{module}`  e.g. {example}");
        }
    }
    let (files, bytes) = checkout.read_so_far();
    println!("\ncheckout read {files} files, {:.1} MB; peak footprint {:.1} MB; total {:.1} ms", bytes as f64 / 1e6, max_footprint() as f64 / 1e6, ms(t0.elapsed()));
}
