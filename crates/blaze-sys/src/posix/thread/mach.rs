//! macOS: the Mach calls and libproc calls the profilers and the meters read: ports,
//! `thread_info` and `task_info` (mach/thread_info.h, mach/task_info.h), `mach_task_self`, and
//! libproc's `proc_pidinfo`, `proc_pidfdinfo` and `proc_pid_rusage` (libproc.h,
//! sys/proc_info.h, sys/resource.h). All in libSystem.
//!
//! The flavor numbers (`THREAD_BASIC_INFO`, `MACH_TASK_BASIC_INFO`, `PROC_PIDTASKINFO`, their
//! counts) and `kern_return_t` are [`crate::posix::proc`]'s; `time_value_t` is
//! [`crate::posix::io`]'s.

use core::ffi::{c_int, c_uint, c_void};

use crate::Errno;
use crate::posix::io::TimeValue as time_value_t;
use crate::posix::signal::errno;

/// `mach_port_t` (mach/port.h): a name of a port right in this task, a `natural_t`.
///
/// replaces libc::mach_port_t
pub type mach_port_t = c_uint;

/// `thread_act_t` (mach/mach_types.h): a thread's port.
///
/// replaces libc::thread_act_t
pub type thread_act_t = mach_port_t;

/// `thread_flavor_t` (mach/thread_info.h): which `thread_info` answer is asked, a `natural_t`.
///
/// replaces libc::thread_flavor_t
pub type thread_flavor_t = c_uint;

/// `mach_msg_type_number_t` (mach/message.h): a count of `integer_t`s, a `natural_t`.
///
/// replaces libc::mach_msg_type_number_t
pub type mach_msg_type_number_t = c_uint;

/// `KERN_SUCCESS` (mach/kern_return.h): a Mach call's success.
///
/// replaces libc::KERN_SUCCESS
pub const KERN_SUCCESS: c_int = 0;

/// `struct thread_basic_info` (mach/thread_info.h): a thread's user and system time (to the
/// microsecond), CPU share (of `TH_USAGE_SCALE`), policy, run state, flags, suspensions and
/// sleep.
///
/// replaces libc::thread_basic_info
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct thread_basic_info {
    pub user_time: time_value_t,
    pub system_time: time_value_t,
    pub cpu_usage: c_int,
    pub policy: c_int,
    pub run_state: c_int,
    pub flags: c_int,
    pub suspend_count: c_int,
    pub sleep_time: c_int,
}

// mach/thread_info.h, macOS arm64: ten `integer_t`s (`THREAD_BASIC_INFO_COUNT` 10).
const _: () = {
    assert!(size_of::<thread_basic_info>() == 40);
    assert!(align_of::<thread_basic_info>() == 4);
    assert!(core::mem::offset_of!(thread_basic_info, user_time) == 0);
    assert!(core::mem::offset_of!(thread_basic_info, system_time) == 8);
    assert!(core::mem::offset_of!(thread_basic_info, cpu_usage) == 16);
    assert!(core::mem::offset_of!(thread_basic_info, run_state) == 24);
};

/// `struct mach_task_basic_info` (mach/task_info.h, `#pragma pack(4)`): the task's virtual and
/// resident sizes and its peak, the times of its ended threads, its policy and suspensions.
///
/// replaces libc::mach_task_basic_info
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, packed(4))]
pub struct mach_task_basic_info {
    pub virtual_size: u64,
    pub resident_size: u64,
    pub resident_size_max: u64,
    pub user_time: time_value_t,
    pub system_time: time_value_t,
    pub policy: c_int,
    pub suspend_count: c_int,
}

// mach/task_info.h, macOS arm64: twelve `natural_t`s (`MACH_TASK_BASIC_INFO_COUNT` 12).
const _: () = {
    assert!(size_of::<mach_task_basic_info>() == 48);
    assert!(align_of::<mach_task_basic_info>() == 4);
    assert!(core::mem::offset_of!(mach_task_basic_info, virtual_size) == 0);
    assert!(core::mem::offset_of!(mach_task_basic_info, resident_size) == 8);
    assert!(core::mem::offset_of!(mach_task_basic_info, resident_size_max) == 16);
    assert!(core::mem::offset_of!(mach_task_basic_info, user_time) == 24);
    assert!(core::mem::offset_of!(mach_task_basic_info, policy) == 40);
};

#[allow(clashing_extern_declarations, reason = "blaze-sys's own typed signatures; macos_sample.rs declares `mach_task_self_` with the same type")]
unsafe extern "C" {
    /// `mach_task_self_` (mach/mach_init.h): this task's port, set by libSystem before `main`
    /// and never changed.
    ///
    /// replaces libc::mach_task_self_ (read by libc::mach_task_self)
    pub safe static mach_task_self_: mach_port_t;

    /// `thread_info` (mach/thread_act.h): the `flavor` answer about `thread` into `info`, of
    /// `*count` `integer_t`s in and written back out; a `kern_return_t`.
    ///
    /// replaces libc::thread_info
    pub fn thread_info(thread: thread_act_t, flavor: thread_flavor_t, info: *mut c_int, count: *mut mach_msg_type_number_t) -> c_int;

    /// `task_info` (mach/task.h): the `flavor` answer (a `task_flavor_t`) about `task` into
    /// `info`, of `*count` `integer_t`s in and out; a `kern_return_t`.
    ///
    /// replaces libc::task_info
    pub fn task_info(task: mach_port_t, flavor: c_uint, info: *mut c_int, count: *mut mach_msg_type_number_t) -> c_int;

    /// `proc_pidinfo` (libproc.h): the `flavor` answer (`PROC_PIDTASKINFO`, ...) about process
    /// `pid` into `buffer` of `size` bytes; the bytes written, or 0 or less and `errno`.
    ///
    /// replaces libc::proc_pidinfo
    pub fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, size: c_int) -> c_int;

    /// `proc_pidfdinfo` (libproc.h): the `flavor` answer about descriptor `fd` of process
    /// `pid` into `buffer` of `size` bytes; the bytes written, or 0 or less and `errno`.
    ///
    /// replaces libc::proc_pidfdinfo
    pub fn proc_pidfdinfo(pid: c_int, fd: c_int, flavor: c_int, buffer: *mut c_void, size: c_int) -> c_int;
}

/// `mach_task_self()` (mach/mach_init.h): this task's port, `mach_task_self_`. Cannot fail.
///
/// replaces libc::mach_task_self
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mach_task_self() -> mach_port_t {
    mach_task_self_
}

/// `struct proc_taskinfo` (sys/proc_info.h): a process's sizes, times (Mach absolute units) of
/// its ended and live threads, and its counts of faults, messages, system calls, switches and
/// threads.
///
/// replaces libc::proc_taskinfo
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct proc_taskinfo {
    pub pti_virtual_size: u64,
    pub pti_resident_size: u64,
    pub pti_total_user: u64,
    pub pti_total_system: u64,
    pub pti_threads_user: u64,
    pub pti_threads_system: u64,
    pub pti_policy: i32,
    pub pti_faults: i32,
    pub pti_pageins: i32,
    pub pti_cow_faults: i32,
    pub pti_messages_sent: i32,
    pub pti_messages_received: i32,
    pub pti_syscalls_mach: i32,
    pub pti_syscalls_unix: i32,
    pub pti_csw: i32,
    pub pti_threadnum: i32,
    pub pti_numrunning: i32,
    pub pti_priority: i32,
}

// sys/proc_info.h, macOS arm64.
const _: () = {
    assert!(size_of::<proc_taskinfo>() == 96);
    assert!(align_of::<proc_taskinfo>() == 8);
    assert!(core::mem::offset_of!(proc_taskinfo, pti_resident_size) == 8);
    assert!(core::mem::offset_of!(proc_taskinfo, pti_total_user) == 16);
    assert!(core::mem::offset_of!(proc_taskinfo, pti_total_system) == 24);
    assert!(core::mem::offset_of!(proc_taskinfo, pti_policy) == 48);
    assert!(core::mem::offset_of!(proc_taskinfo, pti_threadnum) == 84);
};

/// `struct proc_fdinfo` (sys/proc_info.h): one open descriptor of a process and its kind
/// (`PROX_FDTYPE_*`), as `proc_pidinfo(PROC_PIDLISTFDS)` lists them.
///
/// replaces libc::proc_fdinfo
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct proc_fdinfo {
    pub proc_fd: i32,
    pub proc_fdtype: u32,
}

// sys/proc_info.h, macOS arm64.
const _: () = {
    assert!(size_of::<proc_fdinfo>() == 8);
    assert!(align_of::<proc_fdinfo>() == 4);
    assert!(core::mem::offset_of!(proc_fdinfo, proc_fdtype) == 4);
};

/// `rusage_info_t` (sys/resource.h): what `proc_pid_rusage` writes into, a `void *`.
///
/// replaces libc::rusage_info_t
pub type rusage_info_t = *mut c_void;

/// `RUSAGE_INFO_V4` (sys/resource.h): the flavor of [`rusage_info_v4`].
///
/// replaces libc::RUSAGE_INFO_V4
pub const RUSAGE_INFO_V4: c_int = 4;

/// `struct rusage_info_v4` (sys/resource.h): [`crate::macos::RusageInfoV4`], whose layout
/// macos.rs's tests check.
///
/// replaces libc::rusage_info_v4
pub type rusage_info_v4 = crate::macos::RusageInfoV4;

/// `proc_pid_rusage` (libproc.h): the `flavor` (`RUSAGE_INFO_V4`) usage of process `pid`
/// into `buffer` (the header's `rusage_info_t *`, the struct itself); 0, or -1 and `errno`.
/// Through the cold table. [`crate::macos::process_usage`] is this process's, typed.
///
/// # Safety
///
/// `buffer` is writable for the struct `flavor` names.
///
/// replaces libc::proc_pid_rusage
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut rusage_info_t) -> c_int {
    // SAFETY: the caller's contract; the cold entry takes the buffer as `*mut c_void`.
    unsafe { crate::macos::cold::proc_pid_rusage(pid, flavor, buffer.cast()) }
}

/// Process `pid`'s task answer: `proc_pidinfo(PROC_PIDTASKINFO)`. [`Errno::Srch`] for no such
/// process, [`Errno::Perm`] for one of another user; a short answer is refused as
/// [`Errno::Inval`]. Used by the launcher's and the profiler's memory and time readings.
///
/// replaces libc::proc_pidinfo(.., libc::PROC_PIDTASKINFO, ..) (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_task_info(pid: c_int) -> Result<proc_taskinfo, Errno> {
    let mut info = proc_taskinfo::default();
    const SIZE: c_int = size_of::<proc_taskinfo>() as c_int;
    // SAFETY: `info` is a live `proc_taskinfo` of `SIZE` bytes the call writes at most `SIZE`
    // of.
    let said = unsafe {
        proc_pidinfo(pid, crate::posix::proc::PROC_PIDTASKINFO as c_int, 0, (&raw mut info).cast(), SIZE)
    };
    if said <= 0 {
        return Err(errno());
    }
    if said < SIZE {
        return Err(Errno::Inval);
    }
    Ok(info)
}

/// Thread `thread`'s basic answer: `thread_info(THREAD_BASIC_INFO)`; the Mach error otherwise
/// (`KERN_INVALID_ARGUMENT` for a port that is no thread). Used by the profilers' per-thread
/// times.
///
/// replaces libc::thread_info(.., libc::THREAD_BASIC_INFO, ..) (the typed form)
#[cfg(target_arch = "aarch64")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn thread_basic_info_of(thread: thread_act_t) -> Result<thread_basic_info, crate::macos_sample::Kern> {
    let zero = time_value_t { seconds: 0, microseconds: 0 };
    let mut info = thread_basic_info {
        user_time: zero,
        system_time: zero,
        cpu_usage: 0,
        policy: 0,
        run_state: 0,
        flags: 0,
        suspend_count: 0,
        sleep_time: 0,
    };
    let mut count = crate::posix::proc::THREAD_BASIC_INFO_COUNT as mach_msg_type_number_t;
    // SAFETY: `info` is a live `thread_basic_info`, `count` `integer_t`s long (its size / 4,
    // asserted above), which the call writes at most `count` of; `count` is a live local.
    let said = unsafe {
        thread_info(thread, crate::posix::proc::THREAD_BASIC_INFO as thread_flavor_t, (&raw mut info).cast(), &raw mut count)
    };
    if said != KERN_SUCCESS {
        return Err(crate::macos_sample::Kern(said));
    }
    Ok(info)
}

/// This task's basic answer: `task_info(mach_task_self(), MACH_TASK_BASIC_INFO)`; the Mach
/// error otherwise. Used by the harness's resident-size reading.
///
/// replaces libc::task_info(.., libc::MACH_TASK_BASIC_INFO, ..) (the typed form)
#[cfg(target_arch = "aarch64")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn task_basic_info_self() -> Result<mach_task_basic_info, crate::macos_sample::Kern> {
    let zero = time_value_t { seconds: 0, microseconds: 0 };
    let mut info = mach_task_basic_info {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        user_time: zero,
        system_time: zero,
        policy: 0,
        suspend_count: 0,
    };
    let mut count = crate::posix::proc::MACH_TASK_BASIC_INFO_COUNT as mach_msg_type_number_t;
    // SAFETY: `info` is a live `mach_task_basic_info`, `count` `natural_t`s long (its size / 4,
    // asserted above; aligned to 4 as the kernel writes it), written at most `count` of.
    let said = unsafe {
        task_info(mach_task_self(), crate::posix::proc::MACH_TASK_BASIC_INFO as c_uint, (&raw mut info).cast(), &raw mut count)
    };
    if said != KERN_SUCCESS {
        return Err(crate::macos_sample::Kern(said));
    }
    Ok(info)
}
