//! macOS host telemetry and process inspection through libSystem: Mach host counters
//! (`host_processor_info`, `host_page_size`, `host_statistics64`, `mach_timebase_info`),
//! BSD sysctl (`hw.memsize`, `vm.swapusage`, `kern.boottime`, `KERN_PROC_PID`, `KERN_PROCARGS2`,
//! `getloadavg`), libproc (`proc_listallpids`, `proc_pidinfo`, `proc_pid_rusage`, `proc_pidpath`),
//! and `kill(2)`.
//!
//! `#![no_std]`, zero dependencies beyond `core`, no allocation and no panics
//! (`#[blaze::noalloc]`, `#[blaze::nopanic]`). Reuses `crate::macos::cold` for symbols already
//! bound through the cold table (`sysctlbyname`, `mach_timebase_info`, `proc_pid_rusage`,
//! `kill`) and `crate::posix` for already-imported symbols (`mach_task_self_`, `proc_pidinfo`,
//! `getloadavg`, `__error`).

use core::ffi::{CStr, c_int, c_void};
use core::mem::{MaybeUninit, offset_of, size_of};

use crate::Errno;

// ---------------------------------------------------------------------------------------------
// Mach / XNU / BSD / libproc constants (verified against MacOSX.sdk headers)
// ---------------------------------------------------------------------------------------------

const CTL_KERN: c_int = 1;
const KERN_PROC: c_int = 14;
const KERN_PROC_PID: c_int = 1;
const KERN_PROCARGS2: c_int = 49;

const KINFO_PROC_SIZE: usize = 648;
const KINFO_PROC_STARTTIME_SEC_OFFSET: usize = 0;
const KINFO_PROC_PID_OFFSET: usize = 40;
const KINFO_PROC_COMM_OFFSET: usize = 243;
const KINFO_PROC_COMM_LEN: usize = 17;
const KINFO_PROC_PPID_OFFSET: usize = 560;
const KINFO_PROC_TDEV_OFFSET: usize = 572;

const KERN_SUCCESS: c_int = 0;
const KERN_INVALID_ADDRESS: c_int = 1;
const KERN_PROTECTION_FAILURE: c_int = 2;
const KERN_NO_SPACE: c_int = 3;
const KERN_INVALID_ARGUMENT: c_int = 4;
const KERN_RESOURCE_SHORTAGE: c_int = 6;
const KERN_NO_ACCESS: c_int = 8;

const PROCESSOR_CPU_LOAD_INFO: c_int = 2;
const CPU_STATE_MAX: usize = 4;

const HOST_VM_INFO64: c_int = 4;
/// `HOST_VM_INFO64_REV1_COUNT` (38 `natural_t` words = 152 bytes): supported on all macOS
/// versions 10.9 through 26+ and includes `internal_page_count`, `purgeable_count`,
/// `wire_count`, `compressor_page_count`, and `external_page_count`.
const HOST_VM_INFO64_REV1_COUNT: u32 = 38;

const PROC_PIDTBSDINFO: c_int = 3;
const PROC_PIDTASKINFO: c_int = 4;
const PROC_PIDVNODEPATHINFO: c_int = 9;
const RUSAGE_INFO_V4: c_int = 4;
const MAXPATHLEN: usize = 1024;

// ---------------------------------------------------------------------------------------------
// Byte-exact XNU / BSD C structs (verified against MacOSX.sdk)
// ---------------------------------------------------------------------------------------------

/// `<mach/mach_time.h>` `struct mach_timebase_info` (8 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawMachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// `<mach/vm_statistics.h>` `struct vm_statistics64` rev1 prefix (152 bytes = 38 × `u32`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawVmStatistics64Rev1 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

/// `<sys/sysctl.h>` `struct xsw_usage` (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawXswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64,
    xsu_pagesize: u32,
    xsu_encrypted: u32,
}

/// `<sys/_types/_timeval.h>` `struct timeval` on 64-bit macOS (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawTimeval {
    tv_sec: i64,
    tv_usec: i32,
    _pad: i32,
}

/// `<sys/proc_info.h>` `struct proc_bsdinfo` (136 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [u8; 16],
    pbi_name: [u8; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

/// `<sys/proc_info.h>` `struct proc_taskinfo` (96 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawProcTaskInfo {
    pti_virtual_size: u64,
    pti_resident_size: u64,
    pti_total_user: u64,
    pti_total_system: u64,
    pti_threads_user: u64,
    pti_threads_system: u64,
    pti_policy: i32,
    pti_faults: i32,
    pti_pageins: i32,
    pti_cow_faults: i32,
    pti_messages_sent: i32,
    pti_messages_received: i32,
    pti_syscalls_mach: i32,
    pti_syscalls_unix: i32,
    pti_csw: i32,
    pti_threadnum: i32,
    pti_numrunning: i32,
    pti_priority: i32,
}

/// `<sys/resource.h>` `struct rusage_info_v4` (296 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawRusageInfoV4 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
    ri_cpu_time_qos_default: u64,
    ri_cpu_time_qos_maintenance: u64,
    ri_cpu_time_qos_background: u64,
    ri_cpu_time_qos_utility: u64,
    ri_cpu_time_qos_legacy: u64,
    ri_cpu_time_qos_user_initiated: u64,
    ri_cpu_time_qos_user_interactive: u64,
    ri_billed_system_time: u64,
    ri_serviced_system_time: u64,
    ri_logical_writes: u64,
    ri_lifetime_max_phys_footprint: u64,
    ri_instructions: u64,
    ri_cycles: u64,
    ri_billed_energy: u64,
    ri_serviced_energy: u64,
    ri_interval_max_phys_footprint: u64,
    ri_runnable_time: u64,
}

/// `<sys/proc_info.h>` `struct vnode_info_path` (1176 bytes, 8-byte aligned).
///
/// `vip_vi` (`struct vnode_info`) is 152 bytes (`19 × u64`), followed immediately at offset 152
/// by `vip_path[MAXPATHLEN]` (1024 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawVnodeInfoPath {
    vip_vi: [u64; 19],
    vip_path: [u8; MAXPATHLEN],
}

/// `<sys/proc_info.h>` `struct proc_vnodepathinfo` (2352 bytes, 8-byte aligned).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawProcVnodePathInfo {
    pvi_cdir: RawVnodeInfoPath,
    pvi_rdir: RawVnodeInfoPath,
}

/// `<sys/sysctl.h>` `struct kinfo_proc` on 64-bit macOS arm64 & x86_64 (648 bytes, 8-byte aligned).
///
/// Stored as an aligned raw byte buffer so fields at SDK-verified offsets can be decoded with
/// `from_ne_bytes` / slice copies without duplicating the 50-field XNU layout.
#[repr(C, align(8))]
struct RawKinfoProc([u8; KINFO_PROC_SIZE]);

// Compile-time layout assertions against MacOSX.sdk sizes and key field offsets.
const _: () = {
    assert!(size_of::<RawMachTimebaseInfo>() == 8);
    assert!(align_of::<RawMachTimebaseInfo>() == 4);
    assert!(offset_of!(RawMachTimebaseInfo, numer) == 0);
    assert!(offset_of!(RawMachTimebaseInfo, denom) == 4);

    assert!(size_of::<RawVmStatistics64Rev1>() == 152);
    assert!(size_of::<RawVmStatistics64Rev1>() == (HOST_VM_INFO64_REV1_COUNT as usize) * 4);
    assert!(offset_of!(RawVmStatistics64Rev1, wire_count) == 12);
    assert!(offset_of!(RawVmStatistics64Rev1, purgeable_count) == 88);
    assert!(offset_of!(RawVmStatistics64Rev1, compressor_page_count) == 128);
    assert!(offset_of!(RawVmStatistics64Rev1, external_page_count) == 136);
    assert!(offset_of!(RawVmStatistics64Rev1, internal_page_count) == 140);

    assert!(size_of::<RawXswUsage>() == 32);
    assert!(offset_of!(RawXswUsage, xsu_total) == 0);
    assert!(offset_of!(RawXswUsage, xsu_used) == 16);

    assert!(size_of::<RawTimeval>() == 16);
    assert!(offset_of!(RawTimeval, tv_sec) == 0);
    assert!(offset_of!(RawTimeval, tv_usec) == 8);

    assert!(size_of::<RawProcBsdInfo>() == 136);
    assert!(offset_of!(RawProcBsdInfo, pbi_pid) == 12);
    assert!(offset_of!(RawProcBsdInfo, pbi_ppid) == 16);
    assert!(offset_of!(RawProcBsdInfo, pbi_uid) == 20);
    assert!(offset_of!(RawProcBsdInfo, pbi_comm) == 48);
    assert!(offset_of!(RawProcBsdInfo, pbi_name) == 64);
    assert!(offset_of!(RawProcBsdInfo, pbi_start_tvsec) == 120);

    assert!(size_of::<RawProcTaskInfo>() == 96);
    assert!(offset_of!(RawProcTaskInfo, pti_resident_size) == 8);
    assert!(offset_of!(RawProcTaskInfo, pti_total_user) == 16);
    assert!(offset_of!(RawProcTaskInfo, pti_total_system) == 24);
    assert!(offset_of!(RawProcTaskInfo, pti_threadnum) == 84);

    assert!(size_of::<RawRusageInfoV4>() == 296);
    assert!(offset_of!(RawRusageInfoV4, ri_user_time) == 16);
    assert!(offset_of!(RawRusageInfoV4, ri_system_time) == 24);
    assert!(offset_of!(RawRusageInfoV4, ri_phys_footprint) == 72);
    assert!(offset_of!(RawRusageInfoV4, ri_diskio_bytesread) == 144);
    assert!(offset_of!(RawRusageInfoV4, ri_diskio_byteswritten) == 152);

    assert!(size_of::<RawVnodeInfoPath>() == 1176);
    assert!(offset_of!(RawVnodeInfoPath, vip_path) == 152);

    assert!(size_of::<RawProcVnodePathInfo>() == 2352);
    assert!(offset_of!(RawProcVnodePathInfo, pvi_cdir) == 0);
    assert!(offset_of!(RawProcVnodePathInfo, pvi_rdir) == 1176);

    assert!(size_of::<RawKinfoProc>() == KINFO_PROC_SIZE);
    assert!(align_of::<RawKinfoProc>() == 8);
    assert!(KINFO_PROC_STARTTIME_SEC_OFFSET + size_of::<i64>() <= KINFO_PROC_SIZE);
    assert!(KINFO_PROC_PID_OFFSET + size_of::<i32>() <= KINFO_PROC_SIZE);
    assert!(KINFO_PROC_COMM_OFFSET + KINFO_PROC_COMM_LEN <= KINFO_PROC_SIZE);
    assert!(KINFO_PROC_PPID_OFFSET + size_of::<i32>() <= KINFO_PROC_SIZE);
    assert!(KINFO_PROC_TDEV_OFFSET + size_of::<i32>() <= KINFO_PROC_SIZE);
};

// ---------------------------------------------------------------------------------------------
// Direct libSystem imports (tracked in tools/ratchets/blaze-sys-libsystem.txt)
// ---------------------------------------------------------------------------------------------

mod ffi {
    use core::ffi::{c_int, c_void};

    unsafe extern "C" {
        /// `<mach/mach_init.h>`: allocates a send right for the Mach host port; release with
        /// `mach_port_deallocate`.
        pub(super) fn mach_host_self() -> u32;
        /// `<mach/mach_port.h>`: releases a send right in `task`.
        pub(super) fn mach_port_deallocate(task: u32, name: u32) -> c_int;
        /// `<mach/mach_host.h>`: reads the host's VM page size in bytes.
        pub(super) fn host_page_size(host: u32, out_page_size: *mut usize) -> c_int;
        /// `<mach/mach_host.h>`: reads per-processor info (`PROCESSOR_CPU_LOAD_INFO`) into a
        /// kernel-allocated VM buffer released via `vm_deallocate`.
        pub(super) fn host_processor_info(
            host: u32,
            flavor: c_int,
            out_processor_count: *mut u32,
            out_processor_info: *mut *mut c_int,
            out_processor_info_cnt: *mut u32,
        ) -> c_int;
        /// `<mach/mach_host.h>`: reads 64-bit host statistics (`HOST_VM_INFO64`).
        pub(super) fn host_statistics64(
            host: u32,
            flavor: c_int,
            host_info_out: *mut c_int,
            host_info_out_cnt: *mut u32,
        ) -> c_int;
        /// `<mach/vm_map.h>`: deallocates a Mach VM region in `target_task`.
        pub(super) fn vm_deallocate(target_task: u32, address: usize, size: usize) -> c_int;
        /// `<libproc.h>`: lists all active PIDs (or returns the current count when `buffer` is null).
        pub(super) fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        /// `<libproc.h>`: resolves the executable path of `pid` into `buffer`.
        pub(super) fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        /// `<sys/sysctl.h>`: reads or writes a MIB sysctl (`KERN_PROCARGS2`, `KERN_PROC_PID`).
        pub(super) fn sysctl(
            name: *mut c_int,
            namelen: u32,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *const c_void,
            newlen: usize,
        ) -> c_int;
    }
}

// ---------------------------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------------------------

/// Owned Mach host port send right released via `mach_port_deallocate` on drop.
struct MachHostPort(u32);

impl Drop for MachHostPort {
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn drop(&mut self) {
        if self.0 != 0 {
            let task = crate::posix::thread::mach_task_self();
            // SAFETY: `self.0` is an owned send right returned by `mach_host_self` for `task`.
            let _ = unsafe { ffi::mach_port_deallocate(task, self.0) };
        }
    }
}

#[blaze::noalloc]
#[blaze::nopanic]
fn acquire_host_port() -> Result<MachHostPort, Errno> {
    // SAFETY: `mach_host_self` takes no arguments and returns a host send right (or 0 on failure).
    let raw = unsafe { ffi::mach_host_self() };
    if raw == 0 { Err(Errno::Io) } else { Ok(MachHostPort(raw)) }
}

#[blaze::noalloc]
#[blaze::nopanic]
const fn mach_kern_to_errno(kr: c_int) -> Errno {
    match kr {
        KERN_INVALID_ADDRESS => Errno::Fault,
        KERN_PROTECTION_FAILURE | KERN_NO_ACCESS => Errno::Perm,
        KERN_NO_SPACE | KERN_RESOURCE_SHORTAGE => Errno::NoMem,
        KERN_INVALID_ARGUMENT => Errno::Inval,
        _ => Errno::Io,
    }
}

#[blaze::noalloc]
#[blaze::nopanic]
fn last_errno_or(default: Errno) -> Errno {
    let err = crate::posix::signal::errno();
    if err.raw() == 0 { default } else { err }
}

#[blaze::noalloc]
#[blaze::nopanic]
fn trim_nul(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(len) => bytes.get(..len).unwrap_or(bytes),
        None => bytes,
    }
}

#[blaze::noalloc]
#[blaze::nopanic]
fn kinfo_bytes<const OFFSET: usize, const N: usize>(raw: &RawKinfoProc) -> Result<[u8; N], Errno> {
    const { assert!(OFFSET + N <= KINFO_PROC_SIZE) };
    let Some(slice) = raw.0.get(OFFSET..OFFSET + N) else {
        return Err(Errno::Io);
    };
    <[u8; N]>::try_from(slice).map_err(|_| Errno::Io)
}

#[blaze::noalloc]
#[blaze::nopanic]
fn sysctl_pod<T: Copy + Default>(name: &CStr) -> Result<T, Errno> {
    let mut val = T::default();
    let mut size = size_of::<T>();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `name` is a live NUL-terminated C string; `val` holds `size` writable bytes and
    // `newp` is null (read-only query).
    let rc = unsafe {
        crate::macos::cold::sysctlbyname(
            name.as_ptr(),
            (&raw mut val).cast::<c_void>(),
            &raw mut size,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    if size != size_of::<T>() {
        return Err(Errno::Inval);
    }
    Ok(val)
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// Per-core CPU load counters (`PROCESSOR_CPU_LOAD_INFO`), in Mach scheduler ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCoreTicks {
    pub user: u32,
    pub system: u32,
    pub idle: u32,
    pub nice: u32,
}

/// Reads per-core CPU load ticks via `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`.
///
/// Copies `min(total_cpus, out.len())` entries into `out`, frees the kernel VM buffer via
/// `vm_deallocate`, releases the host port via `mach_port_deallocate`, and returns `total_cpus`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn cpu_core_ticks(out: &mut [CpuCoreTicks]) -> Result<usize, Errno> {
    let host = acquire_host_port()?;
    let mut num_cpus: u32 = 0;
    let mut info_ptr: *mut c_int = core::ptr::null_mut();
    let mut info_cnt: u32 = 0;

    // SAFETY: `host.0` is a valid Mach host port and all out-pointers are live stack locals.
    // On `KERN_SUCCESS`, `info_ptr` points to a kernel-allocated VM buffer of `info_cnt` `c_int`s
    // deallocated below via `vm_deallocate`.
    let kr = unsafe {
        ffi::host_processor_info(
            host.0,
            PROCESSOR_CPU_LOAD_INFO,
            &raw mut num_cpus,
            &raw mut info_ptr,
            &raw mut info_cnt,
        )
    };
    if kr != KERN_SUCCESS {
        return Err(mach_kern_to_errno(kr));
    }
    if info_ptr.is_null() {
        return Err(Errno::Io);
    }

    let total_words = info_cnt as usize;
    let total_cpus = (num_cpus as usize).min(total_words / CPU_STATE_MAX);

    // SAFETY: `host_processor_info` succeeded and returned a non-null VM buffer of `total_words`
    // contiguous `c_int` values valid until `vm_deallocate` below.
    let slice = unsafe { core::slice::from_raw_parts(info_ptr, total_words) };
    for (dst, chunk) in out.iter_mut().zip(slice.chunks_exact(CPU_STATE_MAX)).take(total_cpus) {
        if let &[user, system, idle, nice] = chunk {
            *dst = CpuCoreTicks {
                user: user.cast_unsigned(),
                system: system.cast_unsigned(),
                idle: idle.cast_unsigned(),
                nice: nice.cast_unsigned(),
            };
        }
    }

    let task = crate::posix::thread::mach_task_self();
    let byte_len = total_words.saturating_mul(size_of::<c_int>());
    // SAFETY: `info_ptr` was allocated in `task` by `host_processor_info` with `byte_len` bytes,
    // and `slice` is no longer live.
    let _ = unsafe { ffi::vm_deallocate(task, info_ptr as usize, byte_len) };

    Ok(total_cpus)
}

/// Host memory and swap breakdown matching macOS Activity Monitor categories, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmMemInfo {
    pub total_bytes: u64,
    pub page_size: u64,
    pub app_bytes: u64,
    pub wired_bytes: u64,
    pub compressed_bytes: u64,
    pub cached_bytes: u64,
    pub free_bytes: u64,
    pub active_bytes: u64,
    pub inactive_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
}

/// Reads host RAM, VM page statistics (`host_statistics64(HOST_VM_INFO64)`), and swap usage
/// (`vm.swapusage`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn vm_mem_info() -> Result<VmMemInfo, Errno> {
    let total_bytes = sysctl_pod::<u64>(c"hw.memsize")?;
    let host = acquire_host_port()?;

    let mut page_size_raw: usize = 0;
    // SAFETY: `host.0` is a valid Mach host port and `page_size_raw` is a live writable local.
    let ps_kr = unsafe { ffi::host_page_size(host.0, &raw mut page_size_raw) };
    if ps_kr != KERN_SUCCESS {
        return Err(mach_kern_to_errno(ps_kr));
    }
    if page_size_raw == 0 {
        return Err(Errno::Inval);
    }
    let page_size = page_size_raw as u64;

    let mut vm = RawVmStatistics64Rev1::default();
    let mut count = HOST_VM_INFO64_REV1_COUNT;
    // SAFETY: `RawVmStatistics64Rev1` is 152 bytes = 38 `u32` words matching
    // `HOST_VM_INFO64_REV1_COUNT`; `count` is a live local updated by the call.
    let vm_kr = unsafe {
        ffi::host_statistics64(host.0, HOST_VM_INFO64, (&raw mut vm).cast::<c_int>(), &raw mut count)
    };
    if vm_kr != KERN_SUCCESS {
        return Err(mach_kern_to_errno(vm_kr));
    }

    let swap = sysctl_pod::<RawXswUsage>(c"vm.swapusage")?;

    let internal = u64::from(vm.internal_page_count);
    let purgeable = u64::from(vm.purgeable_count);
    let wired = u64::from(vm.wire_count);
    let compressed = u64::from(vm.compressor_page_count);
    let external = u64::from(vm.external_page_count);
    let free = u64::from(vm.free_count);
    let active = u64::from(vm.active_count);
    let inactive = u64::from(vm.inactive_count);

    Ok(VmMemInfo {
        total_bytes,
        page_size,
        app_bytes: internal.saturating_sub(purgeable).saturating_mul(page_size),
        wired_bytes: wired.saturating_mul(page_size),
        compressed_bytes: compressed.saturating_mul(page_size),
        cached_bytes: external.saturating_add(purgeable).saturating_mul(page_size),
        free_bytes: free.saturating_mul(page_size),
        active_bytes: active.saturating_mul(page_size),
        inactive_bytes: inactive.saturating_mul(page_size),
        swap_used_bytes: swap.xsu_used,
        swap_total_bytes: swap.xsu_total,
    })
}

/// System boot timestamp as Unix epoch seconds, from `sysctlbyname("kern.boottime")`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn boot_time_secs() -> Result<u64, Errno> {
    let boot = sysctl_pod::<RawTimeval>(c"kern.boottime")?;
    u64::try_from(boot.tv_sec).map_err(|_| Errno::Range)
}

/// System 1-, 5-, and 15-minute load averages from `getloadavg(3)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn load_average() -> Result<[f64; 3], Errno> {
    let mut avg = [0.0f64; 3];
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `avg` holds 3 writable `f64` elements matching `nelem = 3`.
    let got = unsafe { crate::posix::proc::getloadavg(avg.as_mut_ptr(), 3) };
    if got < 0 {
        return Err(last_errno_or(Errno::Io));
    }
    if got < 3 {
        return Err(Errno::Io);
    }
    Ok(avg)
}

/// Mach absolute time conversion ratio (`numer / denom` nanoseconds per tick).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachTimebase {
    pub numer: u32,
    pub denom: u32,
}

/// Reads the Mach timebase ratio via `mach_timebase_info`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mach_timebase() -> Result<MachTimebase, Errno> {
    let mut raw = RawMachTimebaseInfo::default();
    // SAFETY: `raw` is a live 8-byte `mach_timebase_info_data_t`.
    let kr = unsafe { crate::macos::cold::mach_timebase_info((&raw mut raw).cast::<c_void>()) };
    if kr != KERN_SUCCESS {
        return Err(mach_kern_to_errno(kr));
    }
    if raw.numer == 0 || raw.denom == 0 {
        return Err(Errno::Inval);
    }
    Ok(MachTimebase { numer: raw.numer, denom: raw.denom })
}

/// Converts Mach absolute ticks to nanoseconds using `u128` intermediate arithmetic.
///
/// Returns `ticks` unchanged if `tb.denom == 0`, and saturates at `u64::MAX` on overflow.
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn ticks_to_ns(ticks: u64, tb: MachTimebase) -> u64 {
    if tb.denom == 0 {
        return ticks;
    }
    let ns = ((ticks as u128) * (tb.numer as u128)) / (tb.denom as u128);
    if ns > u64::MAX as u128 { u64::MAX } else { ns as u64 }
}

/// Returns the estimated number of active processes via `proc_listallpids(NULL, 0)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn count_all_pids() -> Result<usize, Errno> {
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `proc_listallpids(NULL, 0)` queries the current process count without writing.
    let count = unsafe { ffi::proc_listallpids(core::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(last_errno_or(Errno::Io));
    }
    Ok(count as usize)
}

/// Fills `out` with active process IDs via `proc_listallpids` and returns the count written.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn list_all_pids(out: &mut [i32]) -> Result<usize, Errno> {
    if out.is_empty() {
        return Ok(0);
    }
    let max_pids = (c_int::MAX as usize) / size_of::<i32>();
    let cap = out.len().min(max_pids);
    let buf_bytes = (cap * size_of::<i32>()) as c_int;
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `out` has at least `cap` writable `i32` slots (`buf_bytes` bytes).
    let count = unsafe { ffi::proc_listallpids(out.as_mut_ptr().cast::<c_void>(), buf_bytes) };
    if count < 0 {
        return Err(last_errno_or(Errno::Io));
    }
    Ok((count as usize).min(cap))
}

/// BSD process metadata from `proc_pidinfo(PROC_PIDTBSDINFO)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcBsdInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub start_time_secs: u64,
    pub comm: [u8; 16],
    pub name: [u8; 32],
}

impl ProcBsdInfo {
    /// Returns NUL-trimmed `self.name` if non-empty, otherwise NUL-trimmed `self.comm`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn name_bytes(&self) -> &[u8] {
        let name = trim_nul(&self.name);
        if !name.is_empty() { name } else { trim_nul(&self.comm) }
    }
}

/// Reads BSD process metadata for `pid` via `proc_pidinfo(PROC_PIDTBSDINFO = 3)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_bsd_info(pid: i32) -> Result<ProcBsdInfo, Errno> {
    const SIZE: c_int = size_of::<RawProcBsdInfo>() as c_int;
    let mut raw = MaybeUninit::<RawProcBsdInfo>::zeroed();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `raw` points to 136 writable bytes matching `PROC_PIDTBSDINFO`.
    let said = unsafe {
        crate::posix::thread::proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, raw.as_mut_ptr().cast::<c_void>(), SIZE)
    };
    if said <= 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    if said != SIZE {
        return Err(Errno::Inval);
    }
    // SAFETY: `proc_pidinfo` wrote all 136 bytes of `RawProcBsdInfo` (all integer fields).
    let raw = unsafe { raw.assume_init() };
    Ok(ProcBsdInfo {
        pid: raw.pbi_pid,
        ppid: raw.pbi_ppid,
        uid: raw.pbi_uid,
        start_time_secs: raw.pbi_start_tvsec,
        comm: raw.pbi_comm,
        name: raw.pbi_name,
    })
}

/// Basic process identity from `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])`.
///
/// Unlike `proc_bsd_info`, `KERN_PROC_PID` is world-readable on macOS for cross-UID / root
/// processes (such as `/usr/bin/login`), keeping the process tree intact across login wrappers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcKinfoBasic {
    pub pid: u32,
    pub ppid: u32,
    pub comm: [u8; 17],
    pub start_time_secs: u64,
    pub tty_dev: i32,
}

impl ProcKinfoBasic {
    /// Process command name trimmed at the first NUL byte (`kp_proc.p_comm`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn name_bytes(&self) -> &[u8] {
        trim_nul(&self.comm)
    }
}

/// Reads basic process identity for `pid` via `sysctl([CTL_KERN, KERN_PROC, KERN_PROC_PID, pid])`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_kinfo_basic(pid: i32) -> Result<ProcKinfoBasic, Errno> {
    if pid <= 0 {
        return Err(Errno::Inval);
    }
    let mut mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid];
    let mut raw = RawKinfoProc([0u8; KINFO_PROC_SIZE]);
    let mut len = KINFO_PROC_SIZE;
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `mib` has 4 `c_int` entries; `raw.0` holds `KINFO_PROC_SIZE` (648) writable,
    // 8-byte-aligned bytes matching `sizeof(struct kinfo_proc)`; `newp` is null.
    let rc = unsafe {
        ffi::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            raw.0.as_mut_ptr().cast::<c_void>(),
            &raw mut len,
            core::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(last_errno_or(Errno::Io));
    }
    if len < KINFO_PROC_SIZE {
        return Err(Errno::Srch);
    }
    let tv_sec = i64::from_ne_bytes(kinfo_bytes::<KINFO_PROC_STARTTIME_SEC_OFFSET, 8>(&raw)?);
    let raw_pid = i32::from_ne_bytes(kinfo_bytes::<KINFO_PROC_PID_OFFSET, 4>(&raw)?);
    let comm = kinfo_bytes::<KINFO_PROC_COMM_OFFSET, KINFO_PROC_COMM_LEN>(&raw)?;
    let raw_ppid = i32::from_ne_bytes(kinfo_bytes::<KINFO_PROC_PPID_OFFSET, 4>(&raw)?);
    let tty_dev = i32::from_ne_bytes(kinfo_bytes::<KINFO_PROC_TDEV_OFFSET, 4>(&raw)?);
    let Ok(pid) = u32::try_from(raw_pid) else {
        return Err(Errno::Io);
    };
    let Ok(ppid) = u32::try_from(raw_ppid) else {
        return Err(Errno::Io);
    };
    let start_time_secs = u64::try_from(tv_sec).unwrap_or(0);
    Ok(ProcKinfoBasic { pid, ppid, comm, start_time_secs, tty_dev })
}

/// Process task counters from `proc_pidinfo(PROC_PIDTASKINFO)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcTaskInfo {
    pub total_user: u64,
    pub total_system: u64,
    pub resident_size: u64,
    pub threads: u32,
}

/// Reads task counters for `pid` via `proc_pidinfo(PROC_PIDTASKINFO = 4)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_task_info(pid: i32) -> Result<ProcTaskInfo, Errno> {
    const SIZE: c_int = size_of::<RawProcTaskInfo>() as c_int;
    let mut raw = RawProcTaskInfo::default();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `raw` is a live 96-byte `RawProcTaskInfo` matching `PROC_PIDTASKINFO`.
    let said = unsafe {
        crate::posix::thread::proc_pidinfo(pid, PROC_PIDTASKINFO, 0, (&raw mut raw).cast::<c_void>(), SIZE)
    };
    if said <= 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    if said != SIZE {
        return Err(Errno::Inval);
    }
    let threads = u32::try_from(raw.pti_threadnum).unwrap_or(0);
    Ok(ProcTaskInfo {
        total_user: raw.pti_total_user,
        total_system: raw.pti_total_system,
        resident_size: raw.pti_resident_size,
        threads,
    })
}

/// Process resource usage from `proc_pid_rusage(RUSAGE_INFO_V4)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcRusage {
    pub phys_footprint: u64,
    pub user_time: u64,
    pub system_time: u64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
}

/// Reads resource usage for `pid` via `proc_pid_rusage(RUSAGE_INFO_V4 = 4)`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_rusage_v4(pid: i32) -> Result<ProcRusage, Errno> {
    let mut raw = MaybeUninit::<RawRusageInfoV4>::zeroed();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `raw` points to 296 writable bytes matching `RUSAGE_INFO_V4`.
    let rc = unsafe { crate::macos::cold::proc_pid_rusage(pid, RUSAGE_INFO_V4, raw.as_mut_ptr().cast::<c_void>()) };
    if rc != 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    // SAFETY: `proc_pid_rusage` returned 0 and populated `RawRusageInfoV4` (all integer fields).
    let raw = unsafe { raw.assume_init() };
    Ok(ProcRusage {
        phys_footprint: raw.ri_phys_footprint,
        user_time: raw.ri_user_time,
        system_time: raw.ri_system_time,
        disk_read_bytes: raw.ri_diskio_bytesread,
        disk_write_bytes: raw.ri_diskio_byteswritten,
    })
}

/// Resolves the executable path of `pid` into `buf` via `proc_pidpath`, returning the byte
/// length written (excluding any trailing NUL).
///
/// XNU requires `buf.len() >= 1024` (`PROC_PIDPATHINFO_SIZE`); 4096 (`PROC_PIDPATHINFO_MAXSIZE`)
/// avoids `ENOMEM` for long paths.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_exe_path(pid: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    if buf.is_empty() {
        return Err(Errno::NoMem);
    }
    let buf_len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `buf` has `buf_len` writable bytes for `proc_pidpath`.
    let said = unsafe { ffi::proc_pidpath(pid, buf.as_mut_ptr().cast::<c_void>(), buf_len) };
    if said <= 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    Ok((said as usize).min(buf.len()))
}

/// Reads the current working directory of `pid` via `proc_pidinfo(PROC_PIDVNODEPATHINFO = 9)`,
/// copies the NUL-trimmed `pvi_cdir.vip_path` into `buf`, and returns its byte length.
///
/// Returns [`Errno::Range`] if `buf` is smaller than the path length.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_cwd(pid: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    const SIZE: c_int = size_of::<RawProcVnodePathInfo>() as c_int;
    let mut raw = MaybeUninit::<RawProcVnodePathInfo>::zeroed();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `raw` points to 2352 writable bytes matching `PROC_PIDVNODEPATHINFO`.
    let said = unsafe {
        crate::posix::thread::proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, raw.as_mut_ptr().cast::<c_void>(), SIZE)
    };
    if said <= 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    if said != SIZE {
        return Err(Errno::Inval);
    }
    // SAFETY: `proc_pidinfo` wrote all 2352 bytes of `RawProcVnodePathInfo`.
    let raw = unsafe { raw.assume_init() };
    let path = trim_nul(&raw.pvi_cdir.vip_path);
    let Some(dst) = buf.get_mut(..path.len()) else {
        return Err(Errno::Range);
    };
    dst.copy_from_slice(path);
    Ok(path.len())
}

/// Queries the required byte length for `sysctl([CTL_KERN, KERN_PROCARGS2, pid])`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_args2_len(pid: i32) -> Result<usize, Errno> {
    if pid <= 0 {
        return Err(Errno::Inval);
    }
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut needed: usize = 0;
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `mib` has 3 `c_int` entries; `oldp = NULL` queries the required byte length.
    let rc = unsafe {
        ffi::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            core::ptr::null_mut(),
            &raw mut needed,
            core::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    Ok(needed)
}

/// Reads the raw `KERN_PROCARGS2` buffer for `pid` into `buf` via
/// `sysctl([CTL_KERN, KERN_PROCARGS2, pid])`, returning the number of bytes written.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_args2(pid: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    if pid <= 0 {
        return Err(Errno::Inval);
    }
    if buf.is_empty() {
        return Err(Errno::NoMem);
    }
    let mut mib = [CTL_KERN, KERN_PROCARGS2, pid];
    let mut size: usize = buf.len();
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `mib` has 3 `c_int` entries; `buf` holds `size` writable bytes; `newp` is null.
    let rc = unsafe {
        ffi::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast::<c_void>(),
            &raw mut size,
            core::ptr::null(),
            0,
        )
    };
    if rc != 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    Ok(size.min(buf.len()))
}

/// Sends signal `sig` to process `pid` via `kill(2)`.
///
/// Refuses `pid <= 0` with [`Errno::Inval`] so callers cannot accidentally signal the caller's
/// process group or every process.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill_pid(pid: i32, sig: i32) -> Result<(), Errno> {
    if pid <= 0 {
        return Err(Errno::Inval);
    }
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `kill(2)` takes two integers and touches no caller memory.
    let rc = unsafe { crate::macos::cold::kill(pid, sig) };
    if rc != 0 {
        return Err(last_errno_or(Errno::Inval));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_macos_sdk() {
        assert_eq!(size_of::<RawMachTimebaseInfo>(), 8);
        assert_eq!(size_of::<RawVmStatistics64Rev1>(), 152);
        assert_eq!(size_of::<RawXswUsage>(), 32);
        assert_eq!(size_of::<RawTimeval>(), 16);
        assert_eq!(size_of::<RawProcBsdInfo>(), 136);
        assert_eq!(size_of::<RawProcTaskInfo>(), 96);
        assert_eq!(size_of::<RawRusageInfoV4>(), 296);
        assert_eq!(size_of::<RawVnodeInfoPath>(), 1176);
        assert_eq!(size_of::<RawProcVnodePathInfo>(), 2352);
        assert_eq!(size_of::<RawKinfoProc>(), 648);
        assert_eq!(align_of::<RawKinfoProc>(), 8);

        // Cross-check against libc oracle where libc exposes the struct.
        assert_eq!(size_of::<RawTimeval>(), size_of::<libc::timeval>());
        assert_eq!(offset_of!(RawTimeval, tv_sec), offset_of!(libc::timeval, tv_sec));
        assert_eq!(offset_of!(RawTimeval, tv_usec), offset_of!(libc::timeval, tv_usec));
        assert_eq!(size_of::<RawProcBsdInfo>(), size_of::<libc::proc_bsdinfo>());
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_pid), offset_of!(libc::proc_bsdinfo, pbi_pid));
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_ppid), offset_of!(libc::proc_bsdinfo, pbi_ppid));
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_uid), offset_of!(libc::proc_bsdinfo, pbi_uid));
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_comm), offset_of!(libc::proc_bsdinfo, pbi_comm));
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_name), offset_of!(libc::proc_bsdinfo, pbi_name));
        assert_eq!(offset_of!(RawProcBsdInfo, pbi_start_tvsec), offset_of!(libc::proc_bsdinfo, pbi_start_tvsec));
        assert_eq!(size_of::<RawProcTaskInfo>(), size_of::<libc::proc_taskinfo>());
        assert_eq!(offset_of!(RawProcTaskInfo, pti_resident_size), offset_of!(libc::proc_taskinfo, pti_resident_size));
        assert_eq!(offset_of!(RawProcTaskInfo, pti_total_user), offset_of!(libc::proc_taskinfo, pti_total_user));
        assert_eq!(offset_of!(RawProcTaskInfo, pti_total_system), offset_of!(libc::proc_taskinfo, pti_total_system));
        assert_eq!(offset_of!(RawProcTaskInfo, pti_threadnum), offset_of!(libc::proc_taskinfo, pti_threadnum));
        assert_eq!(size_of::<RawRusageInfoV4>(), size_of::<libc::rusage_info_v4>());
        assert_eq!(offset_of!(RawRusageInfoV4, ri_user_time), offset_of!(libc::rusage_info_v4, ri_user_time));
        assert_eq!(offset_of!(RawRusageInfoV4, ri_system_time), offset_of!(libc::rusage_info_v4, ri_system_time));
        assert_eq!(offset_of!(RawRusageInfoV4, ri_phys_footprint), offset_of!(libc::rusage_info_v4, ri_phys_footprint));
        assert_eq!(offset_of!(RawRusageInfoV4, ri_diskio_bytesread), offset_of!(libc::rusage_info_v4, ri_diskio_bytesread));
        assert_eq!(
            offset_of!(RawRusageInfoV4, ri_diskio_byteswritten),
            offset_of!(libc::rusage_info_v4, ri_diskio_byteswritten)
        );
        assert_eq!(size_of::<RawVnodeInfoPath>(), size_of::<libc::vnode_info_path>());
        assert_eq!(offset_of!(RawVnodeInfoPath, vip_path), offset_of!(libc::vnode_info_path, vip_path));
        assert_eq!(size_of::<RawProcVnodePathInfo>(), size_of::<libc::proc_vnodepathinfo>());
        assert_eq!(offset_of!(RawProcVnodePathInfo, pvi_cdir), offset_of!(libc::proc_vnodepathinfo, pvi_cdir));
        assert_eq!(offset_of!(RawProcVnodePathInfo, pvi_rdir), offset_of!(libc::proc_vnodepathinfo, pvi_rdir));
    }

    #[test]
    fn cpu_core_ticks_reads_live_cores() {
        let total = cpu_core_ticks(&mut []).expect("query cpu count");
        assert!(total > 0, "expected at least 1 CPU core");

        let mut buf = vec![CpuCoreTicks::default(); total];
        let got = cpu_core_ticks(&mut buf).expect("read per-core ticks");
        assert_eq!(got, total);
        let any_nonzero = buf.iter().any(|c| c.user > 0 || c.system > 0 || c.idle > 0);
        assert!(any_nonzero, "expected nonzero tick counters across cores: {buf:?}");

        // Truncated output slice still returns total_cpus and populates prefix.
        let mut small = [CpuCoreTicks::default(); 1];
        let got_small = cpu_core_ticks(&mut small).expect("read truncated core ticks");
        assert_eq!(got_small, total);
        assert!(small[0].user > 0 || small[0].system > 0 || small[0].idle > 0);
    }

    #[test]
    fn vm_mem_info_reads_live_host_memory() {
        let mem = vm_mem_info().expect("read vm_mem_info");
        assert!(mem.total_bytes >= 512 * 1024 * 1024, "unexpected total_bytes: {}", mem.total_bytes);
        assert!(mem.page_size == 4096 || mem.page_size == 16384, "unexpected page_size: {}", mem.page_size);
        assert!(mem.wired_bytes > 0, "expected nonzero wired_bytes");
        assert!(mem.app_bytes > 0 || mem.active_bytes > 0, "expected active memory: {mem:?}");
        assert!(mem.swap_used_bytes <= mem.swap_total_bytes || mem.swap_total_bytes == 0);
    }

    #[test]
    fn boot_time_and_load_average_answer() {
        let boot = boot_time_secs().expect("read boot_time_secs");
        // After 2020-01-01T00:00:00Z.
        assert!(boot > 1_577_836_800, "unexpected boot_time_secs: {boot}");

        let load = load_average().expect("read load_average");
        for v in load {
            assert!(v.is_finite() && v >= 0.0, "unexpected load average: {load:?}");
        }
    }

    #[test]
    fn mach_timebase_and_ticks_to_ns() {
        let tb = mach_timebase().expect("read mach_timebase");
        assert!(tb.numer > 0 && tb.denom > 0, "invalid timebase: {tb:?}");

        assert_eq!(ticks_to_ns(0, tb), 0);
        assert_eq!(ticks_to_ns(12345, MachTimebase { numer: 1, denom: 1 }), 12345);
        assert_eq!(ticks_to_ns(300, MachTimebase { numer: 125, denom: 3 }), 12_500);
        assert_eq!(ticks_to_ns(999, MachTimebase { numer: 125, denom: 0 }), 999);
        assert_eq!(ticks_to_ns(u64::MAX, MachTimebase { numer: 125, denom: 3 }), u64::MAX);
    }

    #[test]
    fn list_all_pids_contains_self() {
        let self_pid = std::process::id() as i32;
        let est = count_all_pids().expect("count_all_pids");
        assert!(est > 0, "expected nonzero process count");

        let mut pids = vec![0i32; est.saturating_add(128)];
        let n = list_all_pids(&mut pids).expect("list_all_pids");
        assert!(n > 0 && n <= pids.len());
        assert!(pids[..n].contains(&self_pid), "expected self pid {self_pid} in {n} listed pids");
        assert_eq!(list_all_pids(&mut []).expect("empty list_all_pids"), 0);
    }

    #[test]
    fn proc_bsd_task_rusage_exe_cwd_args2_on_self() {
        let self_pid = std::process::id() as i32;

        // proc_bsd_info
        let bsd = proc_bsd_info(self_pid).expect("proc_bsd_info(self)");
        assert_eq!(bsd.pid, self_pid as u32);
        assert!(bsd.start_time_secs > 1_577_836_800);
        assert!(!bsd.name_bytes().is_empty(), "expected non-empty process name");

        // name_bytes fallback from empty name to comm
        let mut fallback = bsd;
        fallback.name = [0u8; 32];
        fallback.comm = *b"fallback_comm\0\0\0";
        assert_eq!(fallback.name_bytes(), b"fallback_comm");

        // proc_task_info
        let task = proc_task_info(self_pid).expect("proc_task_info(self)");
        assert!(task.resident_size > 0, "expected nonzero resident_size");
        assert!(task.threads >= 1, "expected at least 1 thread");

        // proc_rusage_v4
        let ru = proc_rusage_v4(self_pid).expect("proc_rusage_v4(self)");
        assert!(ru.phys_footprint > 0, "expected nonzero phys_footprint");
        assert!(ru.user_time > 0 || ru.system_time > 0, "expected nonzero CPU time: {ru:?}");

        // proc_exe_path
        let mut exe_buf = [0u8; 4096];
        let exe_len = proc_exe_path(self_pid, &mut exe_buf).expect("proc_exe_path(self)");
        assert!(exe_len > 0);
        let exe = core::str::from_utf8(&exe_buf[..exe_len]).expect("UTF-8 exe path");
        assert!(exe.starts_with('/'), "expected absolute exe path, got {exe:?}");
        assert_eq!(proc_exe_path(self_pid, &mut []), Err(Errno::NoMem));

        // proc_cwd
        let mut cwd_buf = [0u8; 1024];
        let cwd_len = proc_cwd(self_pid, &mut cwd_buf).expect("proc_cwd(self)");
        assert!(cwd_len > 0);
        let cwd = core::str::from_utf8(&cwd_buf[..cwd_len]).expect("UTF-8 cwd");
        assert!(cwd.starts_with('/'), "expected absolute cwd, got {cwd:?}");
        let mut tiny = [0u8; 1];
        if cwd_len > 1 {
            assert_eq!(proc_cwd(self_pid, &mut tiny), Err(Errno::Range));
        }

        // proc_args2_len & proc_args2
        let needed = proc_args2_len(self_pid).expect("proc_args2_len(self)");
        assert!(needed >= size_of::<i32>(), "expected args2 len >= 4, got {needed}");
        let mut args_buf = vec![0u8; needed.max(8192)];
        let got = proc_args2(self_pid, &mut args_buf).expect("proc_args2(self)");
        assert!(got >= size_of::<i32>(), "expected args2 bytes >= 4, got {got}");
        let argc = i32::from_ne_bytes([args_buf[0], args_buf[1], args_buf[2], args_buf[3]]);
        assert!(argc >= 1, "expected argc >= 1, got {argc}");
        assert_eq!(proc_args2_len(0), Err(Errno::Inval));
        assert_eq!(proc_args2(0, &mut args_buf), Err(Errno::Inval));
        assert_eq!(proc_args2(self_pid, &mut []), Err(Errno::NoMem));
    }

    #[test]
    fn kill_pid_guards_nonpositive_and_probes_self() {
        assert_eq!(kill_pid(0, 0), Err(Errno::Inval));
        assert_eq!(kill_pid(-1, 0), Err(Errno::Inval));
        assert_eq!(kill_pid(i32::MIN, 0), Err(Errno::Inval));
        // Signal 0 checks permission and process existence without sending a signal.
        let self_pid = std::process::id() as i32;
        assert_eq!(kill_pid(self_pid, 0), Ok(()));
    }

    #[test]
    fn proc_kinfo_basic_reads_launchd_and_matches_self_bsd_info() {
        assert_eq!(proc_kinfo_basic(0), Err(Errno::Inval));
        assert_eq!(proc_kinfo_basic(-1), Err(Errno::Inval));

        let launchd = proc_kinfo_basic(1).expect("proc_kinfo_basic(1) without root");
        assert_eq!(launchd.pid, 1);
        assert_eq!(launchd.ppid, 0);
        assert_eq!(launchd.name_bytes(), b"launchd");

        let self_pid = std::process::id() as i32;
        let kinfo = proc_kinfo_basic(self_pid).expect("proc_kinfo_basic(self)");
        let bsd = proc_bsd_info(self_pid).expect("proc_bsd_info(self)");
        assert_eq!(kinfo.pid, bsd.pid);
        assert_eq!(kinfo.ppid, bsd.ppid);
        assert_eq!(kinfo.start_time_secs, bsd.start_time_secs);
        assert!(!kinfo.name_bytes().is_empty(), "expected non-empty kinfo comm");
    }
}
