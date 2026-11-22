//! The Mach and libproc numbers the tree reads (macOS): `kern_return_t`, `natural_t`, the
//! QoS classes, the `thread_info`/`task_info` flavors and their counts, `proc_pidinfo`'s
//! flavors and the superpage flag. The Mach types and calls themselves (`mach_port_t`,
//! `thread_info`, `task_info`, `proc_pidinfo` and their typed forms) are in
//! `crate::posix::thread`'s Mach module.

use core::ffi::{c_int, c_uint};

/// `kern_return_t` (mach/kern_return.h): a Mach call's result, `KERN_SUCCESS` (0) or an error.
/// replaces libc::kern_return_t
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type kern_return_t = c_int;

/// `natural_t` (mach/arm/vm_types.h): Mach's 32-bit word, what thread states are counted in.
/// replaces libc::natural_t
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type natural_t = c_uint;

/// `qos_class_t` (sys/qos.h): a thread's quality-of-service class, as the `unsigned int` the
/// calls take and write. libc's is an enum, which a value the kernel writes and the enum lacks
/// would make undefined; the classes are the constants below (`QOS_CLASS_UTILITY` for
/// `libc::qos_class_t::QOS_CLASS_UTILITY`).
/// replaces libc::qos_class_t
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type qos_class_t = c_uint;

/// `QOS_CLASS_USER_INTERACTIVE` (sys/qos.h).
pub const QOS_CLASS_USER_INTERACTIVE: qos_class_t = 0x21;
/// `QOS_CLASS_USER_INITIATED` (sys/qos.h).
pub const QOS_CLASS_USER_INITIATED: qos_class_t = 0x19;
/// `QOS_CLASS_DEFAULT` (sys/qos.h).
pub const QOS_CLASS_DEFAULT: qos_class_t = 0x15;
/// `QOS_CLASS_UTILITY` (sys/qos.h).
pub const QOS_CLASS_UTILITY: qos_class_t = 0x11;
/// `QOS_CLASS_BACKGROUND` (sys/qos.h).
pub const QOS_CLASS_BACKGROUND: qos_class_t = 0x09;
/// `QOS_CLASS_UNSPECIFIED` (sys/qos.h).
pub const QOS_CLASS_UNSPECIFIED: qos_class_t = 0x00;

/// `THREAD_BASIC_INFO` (mach/thread_info.h): `thread_info`'s flavor for a
/// `thread_basic_info`.
/// replaces libc::THREAD_BASIC_INFO
pub const THREAD_BASIC_INFO: c_int = 3;

/// `THREAD_BASIC_INFO_COUNT` (mach/thread_info.h): its size in `integer_t`s, 10.
/// replaces libc::THREAD_BASIC_INFO_COUNT
pub const THREAD_BASIC_INFO_COUNT: c_uint = 10;

/// `TH_STATE_RUNNING` (mach/thread_info.h): `thread_basic_info.run_state` of a thread on a
/// core.
/// replaces libc::TH_STATE_RUNNING
pub const TH_STATE_RUNNING: c_int = 1;

/// `MACH_TASK_BASIC_INFO` (mach/task_info.h): `task_info`'s flavor for a
/// `mach_task_basic_info`.
/// replaces libc::MACH_TASK_BASIC_INFO
pub const MACH_TASK_BASIC_INFO: c_uint = 20;

/// `MACH_TASK_BASIC_INFO_COUNT` (mach/task_info.h): its size in `natural_t`s, 12.
/// replaces libc::MACH_TASK_BASIC_INFO_COUNT
pub const MACH_TASK_BASIC_INFO_COUNT: c_uint = 12;

/// `PROC_PIDLISTFDS` (sys/proc_info.h): `proc_pidinfo`'s flavor for the open descriptors.
/// replaces libc::PROC_PIDLISTFDS
pub const PROC_PIDLISTFDS: c_int = 1;

/// `PROC_PIDTASKINFO` (sys/proc_info.h): `proc_pidinfo`'s flavor for a `proc_taskinfo`.
/// replaces libc::PROC_PIDTASKINFO
pub const PROC_PIDTASKINFO: c_int = 4;

/// `PROC_PIDFDVNODEPATHINFO` (sys/proc_info.h): `proc_pidfdinfo`'s flavor for a
/// `vnode_fdinfowithpath`, a file descriptor's vnode and its path.
pub const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `PROC_PIDFDSOCKETINFO` (sys/proc_info.h): `proc_pidfdinfo`'s flavor for a `socket_fdinfo`.
pub const PROC_PIDFDSOCKETINFO: c_int = 3;

/// `PROX_FDTYPE_VNODE` (sys/proc_info.h): a `proc_fdinfo.proc_fdtype` of a file, directory
/// or device. replaces libc::PROX_FDTYPE_VNODE
pub const PROX_FDTYPE_VNODE: c_uint = 1;

/// `PROX_FDTYPE_SOCKET` (sys/proc_info.h): a `proc_fdinfo.proc_fdtype` of a socket.
/// replaces libc::PROX_FDTYPE_SOCKET
pub const PROX_FDTYPE_SOCKET: c_uint = 2;

/// `SOCKINFO_IN` (sys/proc_info.h): a `socket_info.soi_kind` whose `soi_proto` is an
/// `in_sockinfo` (an IP socket other than TCP: UDP, raw).
pub const SOCKINFO_IN: c_int = 1;

/// `SOCKINFO_TCP` (sys/proc_info.h): a `socket_info.soi_kind` whose `soi_proto` is a
/// `tcp_sockinfo` (an `in_sockinfo`, then the connection's state).
pub const SOCKINFO_TCP: c_int = 2;

/// `INI_IPV4` (sys/proc_info.h): `in_sockinfo.insi_vflag`'s bit for a v4 address (in the last
/// four bytes of its 16).
pub const INI_IPV4: u8 = 0x1;

/// `INI_IPV6` (sys/proc_info.h): `in_sockinfo.insi_vflag`'s bit for a v6 address.
pub const INI_IPV6: u8 = 0x2;

/// `VREG` (sys/vnode.h, `enum vtype`): `vnode_info.vi_type` of a regular file.
pub const VREG: c_int = 1;

/// `VM_FLAGS_SUPERPAGE_SIZE_2MB` (mach/vm_statistics.h): `mach_vm_allocate` with 2 MiB pages
/// (x86-64 alone; arm64 refuses it).
/// replaces libc::VM_FLAGS_SUPERPAGE_SIZE_2MB
pub const VM_FLAGS_SUPERPAGE_SIZE_2MB: c_int = 2 << 16;
