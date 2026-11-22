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

/// `struct proc_fileinfo` (sys/proc_info.h): the open-file head of every `proc_pidfdinfo`
/// answer: open flags, status, offset, `DTYPE_*` and guard flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct proc_fileinfo {
    pub fi_openflags: u32,
    pub fi_status: u32,
    pub fi_offset: i64,
    pub fi_type: i32,
    pub fi_guardflags: u32,
}

/// `struct vinfo_stat` (sys/proc_info.h): a vnode's `stat` as libproc reports it.
///
/// replaces libc::vinfo_stat
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct vinfo_stat {
    pub vst_dev: u32,
    pub vst_mode: u16,
    pub vst_nlink: u16,
    pub vst_ino: u64,
    pub vst_uid: u32,
    pub vst_gid: u32,
    pub vst_atime: i64,
    pub vst_atimensec: i64,
    pub vst_mtime: i64,
    pub vst_mtimensec: i64,
    pub vst_ctime: i64,
    pub vst_ctimensec: i64,
    pub vst_birthtime: i64,
    pub vst_birthtimensec: i64,
    pub vst_size: i64,
    pub vst_blocks: i64,
    pub vst_blksize: i32,
    pub vst_flags: u32,
    pub vst_gen: u32,
    pub vst_rdev: u32,
    pub vst_qspare: [i64; 2],
}

/// `struct vnode_info` (sys/proc_info.h): a vnode's stat, its `enum vtype` ([`VREG`] for a
/// regular file) and its file system's id (`fsid_t`, two `int32_t`s).
///
/// [`VREG`]: crate::posix::proc::VREG
///
/// replaces libc::vnode_info
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct vnode_info {
    pub vi_stat: vinfo_stat,
    pub vi_type: c_int,
    pub vi_pad: c_int,
    pub vi_fsid: [i32; 2],
}

/// `MAXPATHLEN` (sys/param.h): the bytes of a `vnode_info_path`'s path, its NUL included.
pub const PROC_VNODE_PATH_BYTES: usize = 1024;

/// `struct vnode_info_path` (sys/proc_info.h): a vnode and its path, NUL-terminated in
/// `MAXPATHLEN` bytes.
///
/// replaces libc::vnode_info_path
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct vnode_info_path {
    pub vip_vi: vnode_info,
    pub vip_path: [u8; PROC_VNODE_PATH_BYTES],
}

impl vnode_info_path {
    /// The path's bytes before its NUL (all of them if the kernel wrote none).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn path(&self) -> &[u8] {
        match self.vip_path.iter().position(|&byte| byte == 0) {
            Some(len) => self.vip_path.get(..len).unwrap_or(&self.vip_path),
            None => &self.vip_path,
        }
    }
}

/// `struct vnode_fdinfowithpath` (sys/proc_info.h): `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`'s
/// answer, a descriptor's open-file head and its vnode with path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct vnode_fdinfowithpath {
    pub pfi: proc_fileinfo,
    pub pvip: vnode_info_path,
}

/// `struct sockbuf_info` (sys/proc_info.h): one direction's socket buffer counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct sockbuf_info {
    pub sbi_cc: u32,
    pub sbi_hiwat: u32,
    pub sbi_mbcnt: u32,
    pub sbi_mbmax: u32,
    pub sbi_lowat: u32,
    pub sbi_flags: i16,
    pub sbi_timeo: i16,
}

/// `in_sockinfo.insi_v6` (sys/proc_info.h): the v6 hop limit, checksum offset, interface and
/// hops.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct in_sockinfo_v6 {
    pub in6_hlim: u8,
    pub in6_cksum: c_int,
    pub in6_ifindex: u16,
    pub in6_hops: i16,
}

/// `struct in_sockinfo` (sys/proc_info.h): an IP socket's ends. The ports are `in_port_t`s in
/// network order in the low 16 bits of an `int` (`u16::from_be` of those bits is the number);
/// each address is 16 bytes, a v6 address whole or ([`INI_IPV4`] in `insi_vflag`) a v4 one in
/// its last four.
///
/// [`INI_IPV4`]: crate::posix::proc::INI_IPV4
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct in_sockinfo {
    pub insi_fport: c_int,
    pub insi_lport: c_int,
    pub insi_gencnt: u64,
    pub insi_flags: u32,
    pub insi_flow: u32,
    pub insi_vflag: u8,
    pub insi_ip_ttl: u8,
    pub rfu_1: u32,
    pub insi_faddr: [u8; 16],
    pub insi_laddr: [u8; 16],
    pub insi_v4_tos: u8,
    pub insi_v6: in_sockinfo_v6,
}

/// `struct tcp_sockinfo` (sys/proc_info.h): a TCP socket's [`in_sockinfo`] and its connection
/// state (`TCPS_*`, 0 `CLOSED` to 10 `TIME_WAIT`, netinet/tcp_fsm.h).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct tcp_sockinfo {
    pub tcpsi_ini: in_sockinfo,
    pub tcpsi_state: c_int,
    pub tcpsi_timer: [c_int; 4],
    pub tcpsi_mss: c_int,
    pub tcpsi_flags: u32,
    pub rfu_1: u32,
    pub tcpsi_tp: u64,
}

/// `socket_info.soi_proto` (sys/proc_info.h): the family's part, which `soi_kind` names. Only
/// the IP forms are spelled out; the others (unix, kernel control, ...) are its bytes.
#[derive(Clone, Copy)]
#[repr(C)]
pub union soi_proto {
    pub pri_in: in_sockinfo,
    pub pri_tcp: tcp_sockinfo,
    pub pri_bytes: [u64; 66],
}

/// `struct socket_info` (sys/proc_info.h): a socket's stat, kernel addresses, type, protocol,
/// family, options and state, its buffers, and `soi_kind` naming the `soi_proto` part
/// ([`SOCKINFO_IN`], [`SOCKINFO_TCP`], ...).
///
/// [`SOCKINFO_IN`]: crate::posix::proc::SOCKINFO_IN
/// [`SOCKINFO_TCP`]: crate::posix::proc::SOCKINFO_TCP
#[derive(Clone, Copy)]
#[repr(C)]
pub struct socket_info {
    pub soi_stat: vinfo_stat,
    pub soi_so: u64,
    pub soi_pcb: u64,
    pub soi_type: c_int,
    pub soi_protocol: c_int,
    pub soi_family: c_int,
    pub soi_options: i16,
    pub soi_linger: i16,
    pub soi_state: i16,
    pub soi_qlen: i16,
    pub soi_incqlen: i16,
    pub soi_qlimit: i16,
    pub soi_timeo: i16,
    pub soi_error: u16,
    pub soi_oobmark: u32,
    pub soi_rcv: sockbuf_info,
    pub soi_snd: sockbuf_info,
    pub soi_kind: c_int,
    pub rfu_1: u32,
    pub soi_proto: soi_proto,
}

impl socket_info {
    /// The TCP part, for a socket whose `soi_kind` is [`SOCKINFO_TCP`].
    ///
    /// [`SOCKINFO_TCP`]: crate::posix::proc::SOCKINFO_TCP
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn tcp(&self) -> Option<&tcp_sockinfo> {
        if self.soi_kind != crate::posix::proc::SOCKINFO_TCP {
            return None;
        }
        // SAFETY: `soi_kind` says the kernel wrote a `tcp_sockinfo` here, and every field of it
        // is an integer or an array of them, so any initialized bytes are a valid value; the
        // whole union is initialized (zeroed before the call, see `proc_fd_socket_info`).
        Some(unsafe { &self.soi_proto.pri_tcp })
    }

    /// The IP part (of a TCP socket too), for a socket whose `soi_kind` is [`SOCKINFO_IN`] or
    /// [`SOCKINFO_TCP`].
    ///
    /// [`SOCKINFO_IN`]: crate::posix::proc::SOCKINFO_IN
    /// [`SOCKINFO_TCP`]: crate::posix::proc::SOCKINFO_TCP
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn inet(&self) -> Option<&in_sockinfo> {
        if let Some(tcp) = self.tcp() {
            return Some(&tcp.tcpsi_ini);
        }
        if self.soi_kind != crate::posix::proc::SOCKINFO_IN {
            return None;
        }
        // SAFETY: as `tcp`: `soi_kind` says an `in_sockinfo` is here, all integers, and the
        // union is initialized.
        Some(unsafe { &self.soi_proto.pri_in })
    }
}

/// `struct socket_fdinfo` (sys/proc_info.h): `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`'s answer,
/// a descriptor's open-file head and its socket.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct socket_fdinfo {
    pub pfi: proc_fileinfo,
    pub psi: socket_info,
}

// sys/proc_info.h, macOS arm64 (and x86-64: no field here differs). The offsets are the ones
// blaze-core's socket reader and lsof read.
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<proc_fileinfo>() == 24);
    assert!(offset_of!(proc_fileinfo, fi_offset) == 8);
    assert!(offset_of!(proc_fileinfo, fi_type) == 16);
    assert!(size_of::<vinfo_stat>() == 136);
    assert!(align_of::<vinfo_stat>() == 8);
    assert!(offset_of!(vinfo_stat, vst_ino) == 8);
    assert!(offset_of!(vinfo_stat, vst_size) == 88);
    assert!(offset_of!(vinfo_stat, vst_qspare) == 120);
    assert!(size_of::<vnode_info>() == 152);
    assert!(offset_of!(vnode_info, vi_type) == 136);
    assert!(offset_of!(vnode_info, vi_fsid) == 144);
    assert!(size_of::<vnode_info_path>() == 1176);
    assert!(offset_of!(vnode_info_path, vip_path) == 152);
    assert!(size_of::<vnode_fdinfowithpath>() == 1200);
    assert!(offset_of!(vnode_fdinfowithpath, pvip) == 24);
    assert!(size_of::<sockbuf_info>() == 24);
    assert!(size_of::<in_sockinfo_v6>() == 12);
    assert!(size_of::<in_sockinfo>() == 80);
    assert!(align_of::<in_sockinfo>() == 8);
    assert!(offset_of!(in_sockinfo, insi_lport) == 4);
    assert!(offset_of!(in_sockinfo, insi_gencnt) == 8);
    assert!(offset_of!(in_sockinfo, insi_vflag) == 24);
    assert!(offset_of!(in_sockinfo, rfu_1) == 28);
    assert!(offset_of!(in_sockinfo, insi_faddr) == 32);
    assert!(offset_of!(in_sockinfo, insi_laddr) == 48);
    assert!(offset_of!(in_sockinfo, insi_v4_tos) == 64);
    assert!(offset_of!(in_sockinfo, insi_v6) == 68);
    assert!(size_of::<tcp_sockinfo>() == 120);
    assert!(offset_of!(tcp_sockinfo, tcpsi_state) == 80);
    assert!(offset_of!(tcp_sockinfo, tcpsi_tp) == 112);
    assert!(size_of::<soi_proto>() == 528);
    assert!(size_of::<socket_info>() == 768);
    assert!(offset_of!(socket_info, soi_so) == 136);
    assert!(offset_of!(socket_info, soi_type) == 152);
    assert!(offset_of!(socket_info, soi_protocol) == 156);
    assert!(offset_of!(socket_info, soi_family) == 160);
    assert!(offset_of!(socket_info, soi_options) == 164);
    assert!(offset_of!(socket_info, soi_oobmark) == 180);
    assert!(offset_of!(socket_info, soi_rcv) == 184);
    assert!(offset_of!(socket_info, soi_snd) == 208);
    assert!(offset_of!(socket_info, soi_kind) == 232);
    assert!(offset_of!(socket_info, soi_proto) == 240);
    assert!(size_of::<socket_fdinfo>() == 792);
    assert!(offset_of!(socket_fdinfo, psi) == 24);
};

/// The error of a libproc call that answered 0 or less: `errno`, which the caller cleared
/// before the call, or [`Errno::Inval`] if the call set none (a refusal libproc did not name).
#[blaze::noalloc]
#[blaze::nopanic]
fn libproc_failed() -> Errno {
    let error = errno();
    if error.raw() == 0 { Errno::Inval } else { error }
}

/// How many [`proc_fdinfo`] entries to hold process `pid`'s descriptor list now:
/// `proc_pidinfo(PROC_PIDLISTFDS)` with no buffer, which answers the table's length plus 20
/// entries of slack, in bytes. [`Errno::Srch`] for no such process (or one that has exited),
/// [`Errno::Perm`] for one of another user.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_fd_list_capacity(pid: c_int) -> Result<usize, Errno> {
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: a null buffer of size 0 asks for the size alone; nothing is written.
    let said = unsafe {
        proc_pidinfo(pid, crate::posix::proc::PROC_PIDLISTFDS, 0, core::ptr::null_mut(), 0)
    };
    if said <= 0 {
        return Err(libproc_failed());
    }
    usize::try_from(said / FD_ENTRY_BYTES).map_err(|_| Errno::Inval)
}

/// The bytes of one [`proc_fdinfo`] (the const block above holds it to `size_of`), as the
/// `int` libproc counts in.
const FD_ENTRY_BYTES: c_int = 8;
const _: () = assert!(size_of::<proc_fdinfo>() == 8);

/// Process `pid`'s open descriptors into `into`: `proc_pidinfo(PROC_PIDLISTFDS)`; how many it
/// wrote. Size `into` from [`proc_fd_list_capacity`]: descriptors opened between the two calls
/// past its slack are not listed (the kernel writes what fits), so an answer that fills `into`
/// may be short of the table. Errors as [`proc_fd_list_capacity`]'s; a process with no open
/// descriptor answers 0.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_list_fds(pid: c_int, into: &mut [proc_fdinfo]) -> Result<usize, Errno> {
    // Whole entries, at most `c_int::MAX` bytes of them.
    let entries = c_int::try_from(into.len()).unwrap_or(c_int::MAX).min(c_int::MAX / FD_ENTRY_BYTES);
    let bytes = entries * FD_ENTRY_BYTES;
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `into` is live and writable for `bytes` bytes (`entries <= into.len()` whole
    // `proc_fdinfo`s, all-integer `repr(C)` values any bytes make valid); the call writes at
    // most `bytes`.
    let said = unsafe {
        proc_pidinfo(pid, crate::posix::proc::PROC_PIDLISTFDS, 0, into.as_mut_ptr().cast(), bytes)
    };
    if said < 0 {
        return Err(libproc_failed());
    }
    if said == 0 {
        let error = errno();
        return if error.raw() == 0 { Ok(0) } else { Err(error) };
    }
    usize::try_from(said.min(bytes) / FD_ENTRY_BYTES).map_err(|_| Errno::Inval)
}

/// Descriptor `fd` of process `pid` as a socket: `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`.
/// [`Errno::BadF`] for a descriptor closed since it was listed (or no socket), [`Errno::Srch`]
/// for a process that has exited, [`Errno::Perm`] for one of another user; a short answer is
/// refused as [`Errno::Inval`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_fd_socket_info(pid: c_int, fd: c_int) -> Result<socket_fdinfo, Errno> {
    // `size_of::<socket_fdinfo>()`, which the const block above holds to 792.
    const SIZE: c_int = 792;
    // SAFETY: `socket_fdinfo` is all integers, arrays of them and a union of such; all zero
    // bytes are a valid value, and the zeroing initializes the union's every byte.
    let mut info: socket_fdinfo = unsafe { core::mem::zeroed() };
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `info` is a live `socket_fdinfo` of `SIZE` bytes the call writes at most `SIZE`
    // of.
    let said = unsafe {
        proc_pidfdinfo(pid, fd, crate::posix::proc::PROC_PIDFDSOCKETINFO, (&raw mut info).cast(), SIZE)
    };
    if said <= 0 {
        return Err(libproc_failed());
    }
    if said < SIZE {
        return Err(Errno::Inval);
    }
    Ok(info)
}

/// Descriptor `fd` of process `pid` as a vnode with its path:
/// `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`. Errors as [`proc_fd_socket_info`]'s.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn proc_fd_vnode_path(pid: c_int, fd: c_int) -> Result<vnode_fdinfowithpath, Errno> {
    // `size_of::<vnode_fdinfowithpath>()`, which the const block above holds to 1200.
    const SIZE: c_int = 1200;
    // SAFETY: `vnode_fdinfowithpath` is all integers and arrays of them; all zero bytes are a
    // valid value.
    let mut info: vnode_fdinfowithpath = unsafe { core::mem::zeroed() };
    crate::posix::signal::set_errno(Errno::from_raw(0));
    // SAFETY: `info` is a live `vnode_fdinfowithpath` of `SIZE` bytes the call writes at most
    // `SIZE` of.
    let said = unsafe {
        proc_pidfdinfo(pid, fd, crate::posix::proc::PROC_PIDFDVNODEPATHINFO, (&raw mut info).cast(), SIZE)
    };
    if said <= 0 {
        return Err(libproc_failed());
    }
    if said < SIZE {
        return Err(Errno::Inval);
    }
    Ok(info)
}

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
