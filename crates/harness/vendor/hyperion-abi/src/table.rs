//! The tables that cross the dlopen boundary. `Info`, `Entry` and `Step` are field for field the Zig `extern struct`s of
//! `src/api/plugin.zig` (same abi version 1). `HostApi` is the four-field header the host passes to Rust modules through
//! `hyperion_set_host`; it never grows (docs/host-diet.md section 6 and 10.4): everything else that crosses the boundary is a flecs entity
//! or component, and the shared allocator is three linked symbols (`hyperion_alloc`, `hyperion_dealloc`, `hyperion_realloc`).

use core::ffi::{c_char, c_int, c_void};

/// Same value as Zig `plugin.abi_version`. Bump on any change to `Info`, `Entry` or `Step`.
pub const ABI_VERSION: u32 = 1;

/// Version of [`HostApi`]. 3: the table shrank to `{size, abi, build_id, world}` (docs/host-diet.md 10.4); frozen for good.
pub const HOST_API_VERSION: u32 = 3;

/// Hash of `rustc -vV` of the compiler that built this crate (build.rs). Host and Rust modules must agree.
pub const BUILD_ID: u64 = match u64::from_str_radix(env!("HYPERION_BUILD_ID"), 16) {
    Ok(v) => v,
    Err(_) => 0,
};

pub type Thunk = unsafe extern "C" fn(old: *const c_void, new: *mut c_void);

/// One migration step of a component's chain (oldest first in `Entry.chain`). `new` is zeroed and `new_size` bytes long.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Step {
    pub from_hash: u64,
    pub to_hash: u64,
    pub old_size: u32,
    pub new_size: u32,
    pub thunk: Thunk,
}
unsafe impl Sync for Step {}
unsafe impl Send for Step {}

#[repr(C)]
pub struct Entry {
    pub name: *const c_char,
    pub hash: u64,
    pub chain: *const Step,
    pub n_chain: u32,
}
unsafe impl Sync for Entry {}

#[repr(C)]
pub struct Info {
    pub abi: u32,
    pub version: u32,
    pub name: *const c_char,
    pub provides: *const Entry,
    pub n_provides: u32,
    pub expects: *const Entry,
    pub n_expects: u32,
    pub requires: *const *const c_char,
    pub n_requires: u32,
}
unsafe impl Sync for Info {}

/// The host header handed to a Rust module. `size` and `abi` come first so a reader validates before trusting the rest. Frozen: no field is
/// ever appended (a new host capability is a flecs component, docs/host-diet.md section 6).
#[repr(C)]
pub struct HostApi {
    pub size: u32,
    pub abi: u32,
    pub build_id: u64,
    /// The host's flecs world (`ecs_world_t*`), alive for the life of the process.
    pub world: *mut c_void,
}
unsafe impl Sync for HostApi {}
unsafe impl Send for HostApi {}

/// Live-allocation counters of one module image's own heap (`hyperion_heap_stats` export of a module cdylib).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct HeapStats {
    pub size: u32,
    pub _pad: u32,
    /// Allocations not yet freed, and their bytes (layout sizes).
    pub allocs: i64,
    pub bytes: i64,
    /// Total allocation calls since load.
    pub calls: u64,
}

/// `hyperion_heap_stats(out) -> 0` fills `out` (`out.size` set by the caller to `size_of::<HeapStats>()`); non-zero = this image has no own heap.
pub type HeapStatsFn = unsafe extern "C" fn(out: *mut HeapStats) -> c_int;
/// `hyperion_heap_destroy() -> 0` frees every block of the image's heap in one go and makes later frees no-ops; the manager calls it after
/// `deinit` and before `dlclose`. Non-zero = no own heap.
pub type HeapDestroyFn = unsafe extern "C" fn() -> c_int;
/// `module_thread_teardown()`: the manager runs it on every thread that ran the image's code (host job-pool workers; flecs
/// workers are retired instead) before `module_unload`. It runs and forgets this thread's thread-local destructors of the image, which the shared
/// std holds (std `dylib_unload`: `prepare_unload_thread_in`). Missing export = an image built without the entry macro.
pub type ThreadTeardownFn = unsafe extern "C" fn();
/// `module_unload() -> bool`: once, last, on the reloading thread, before `dlclose`. also resets std hooks that point into the image
/// (`prepare_unload_in`). `false` = the image must stay mapped (a thread it spawned still runs, the host std does not track destructors, macOS).
pub type ModuleUnloadFn = unsafe extern "C" fn() -> bool;
/// `hyperion_take_panic(buf, cap) -> len`: 0 when the image holds no caught panic; else the full message length, `min(len, cap)` bytes copied
/// into `buf`, and the pending message cleared (`crate::panic`).
pub type TakePanicFn = unsafe extern "C" fn(buf: *mut u8, cap: usize) -> usize;

pub type SetHostFn = unsafe extern "C" fn(host: *const HostApi) -> c_int;
pub type InfoFn = unsafe extern "C" fn() -> *const Info;
/// `plugin_update(len_out) -> utf8 bytes`: the module's latest-update line, written by the `hyperion_module!` macro.
pub type UpdateFn = unsafe extern "C" fn(len: *mut usize) -> *const u8;
pub type InitFn = unsafe extern "C" fn(world: *mut c_void) -> c_int;
pub type DeinitFn = unsafe extern "C" fn(world: *mut c_void);

/// Return codes of `hyperion_set_host`.
pub const SET_HOST_OK: c_int = 0;
/// Null table, `abi` differs from [`HOST_API_VERSION`] or `size` is short of [`HostApi`].
pub const SET_HOST_BAD_API: c_int = 1;
pub const SET_HOST_BUILD_ID: c_int = 2;

/// Layout checks that must equal the Zig side (`extern struct` sizes on 64-bit targets) and the frozen host header.
/// Modules load on 64-bit hosts only; wasm32 (den-web) uses these types for layout hashes, never across images.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Step>() == 32);
    assert!(size_of::<Entry>() == 32);
    assert!(size_of::<Info>() == 64);
    assert!(core::mem::offset_of!(Info, provides) == 16);
    assert!(core::mem::offset_of!(Info, requires) == 48);
    assert!(size_of::<HostApi>() == 24);
};
