//! The system calls of the Linux sandbox, each behind a function safe code can't misuse: a
//! pointer it gives the kernel is that of a borrow it holds for the call (a `&CStr`, a slice, a
//! `repr(C)` structure laid out as the kernel's, checked by size below), and a call that takes
//! only integers is safe as it is. Two stay `unsafe fn`: [`fork`], sound only in a process of
//! one thread, and [`close`], which ends a descriptor another owner may still hold. A function
//! returns what the call returned, so its caller reads `errno` right after as before.

use std::{
    ffi::{CStr, CString},
    os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd},
};

use libc::{c_int, c_long, c_uint, c_ulong, gid_t, pid_t, uid_t};

use super::{
    IfReq, MountAttr, Notif, NotifResp, SECCOMP_FILTER_FLAG_NEW_LISTENER,
    SECCOMP_IOCTL_NOTIF_RECV, SECCOMP_IOCTL_NOTIF_SEND, SECCOMP_IOCTL_NOTIF_SET_FLAGS,
    SECCOMP_SET_MODE_FILTER, SYS_MOUNT_SETATTR, SYS_PIDFD_OPEN,
    landlock::{self, PathBeneath, RulesetAttr},
};

// The sizes the kernel reads and writes through these, which the ioctl numbers encode too
// (`_IOWR('!', 0, struct seccomp_notif)` is 0x50 bytes, `seccomp_notif_resp` 0x18): a field
// out of place would have the kernel write past `Notif` or read past `NotifResp`.
const _: () = assert!(size_of::<Notif>() == 80);
const _: () = assert!(size_of::<NotifResp>() == 24);
const _: () = assert!(size_of::<MountAttr>() == 32);
const _: () = assert!(size_of::<IfReq>() == 40);
const _: () = assert!(size_of::<RulesetAttr>() == 24);
const _: () = assert!(size_of::<PathBeneath>() == 12);

pub(super) fn uid() -> uid_t {
    // SAFETY: `getuid` takes no arguments, touches no memory of this process and can't fail.
    unsafe { libc::getuid() }
}

pub(super) fn gid() -> gid_t {
    // SAFETY: `getgid` takes no arguments, touches no memory of this process and can't fail.
    unsafe { libc::getgid() }
}

pub(super) fn pid() -> pid_t {
    // SAFETY: `getpid` takes no arguments, touches no memory of this process and can't fail.
    unsafe { libc::getpid() }
}

/// `fork`: the child's pid in the parent, 0 in the child, below 0 when there is none.
///
/// # Safety
///
/// This process has one thread. The child is a copy of the calling thread alone: with another
/// thread there, a lock it held (the allocator's, stdio's, the environment's) stays held in
/// the child for good, and the child, which allocates, prints and reads the environment here,
/// deadlocks or sees what that thread left half done.
pub(super) unsafe fn fork() -> pid_t {
    // SAFETY: the caller has this process be one thread (# Safety); `fork` takes no arguments.
    unsafe { libc::fork() }
}

/// Ends this process at once with `code` (`_exit`): no `atexit` handler runs and no stdio
/// buffer copied from the process it was forked from is flushed, which would run or be
/// written a second time.
pub(super) fn exit(code: c_int) -> ! {
    // SAFETY: `_exit` takes the code by value, touches no memory of this process and never
    // returns; what it skips (destructors) only leaks.
    unsafe { libc::_exit(code) }
}

/// Makes this process the leader of a new session, with no controlling terminal.
pub(super) fn setsid() -> pid_t {
    // SAFETY: `setsid` takes no arguments and touches no memory of this process.
    unsafe { libc::setsid() }
}

/// Has the kernel kill this process when its parent ends (`PR_SET_PDEATHSIG`).
pub(super) fn die_with_parent() -> c_int {
    // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGKILL)` takes integers only and touches no memory of
    // this process.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) }
}

/// Keeps this process and what it runs from gaining privileges (`PR_SET_NO_NEW_PRIVS`).
pub(super) fn no_new_privs() -> c_int {
    // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes integers only and touches no
    // memory of this process.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }
}

/// Has the processes this one's descendants leave come to it (`PR_SET_CHILD_SUBREAPER`).
pub(super) fn child_subreaper() -> c_int {
    // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0)` takes integers only and touches no
    // memory of this process.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }
}

pub(super) fn unshare(flags: c_int) -> c_int {
    // SAFETY: `unshare` takes its flags by value and touches no memory of this process. A
    // process of more than one thread is refused `CLONE_NEWUSER` (EINVAL), not harmed.
    unsafe { libc::unshare(flags) }
}

/// `waitpid`, with the status written to `status`.
pub(super) fn wait(pid: pid_t, status: &mut c_int, options: c_int) -> pid_t {
    // SAFETY: `status` is a live, exclusive `c_int` for the call to write, and nothing else.
    unsafe { libc::waitpid(pid, status, options) }
}

pub(super) fn kill(pid: pid_t, signal: c_int) -> c_int {
    // SAFETY: `kill` takes integers only and touches no memory of this process.
    unsafe { libc::kill(pid, signal) }
}

pub(super) fn chdir(path: &CStr) -> c_int {
    // SAFETY: `path` is a nul-terminated string borrowed for the call, which only reads it.
    unsafe { libc::chdir(path.as_ptr()) }
}

/// `mount(2)`, with `None` for each string the call takes none of.
pub(super) fn mount(
    source: Option<&CStr>,
    target: &CStr,
    kind: Option<&CStr>,
    flags: c_ulong,
    data: Option<&CStr>,
) -> c_int {
    let or_null = |it: Option<&CStr>| it.map_or(std::ptr::null(), CStr::as_ptr);
    let data = data.map_or(std::ptr::null(), |it| it.as_ptr().cast::<libc::c_void>());
    // SAFETY: each pointer is null or a nul-terminated string borrowed for the call; the
    // kernel reads `data` as a string of options for every kind mounted here (tmpfs, overlay,
    // proc), or not at all for a bind or a change of propagation, and writes none of them.
    unsafe { libc::mount(or_null(source), target.as_ptr(), or_null(kind), flags, data) }
}

/// Detaches the mount at `path` (`umount2` with `MNT_DETACH`).
pub(super) fn umount_detach(path: &CStr) -> c_int {
    // SAFETY: `path` is a nul-terminated string borrowed for the call, which only reads it.
    unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) }
}

pub(super) fn pivot_root(new: &CStr, old: &CStr) -> c_long {
    // SAFETY: both are nul-terminated strings borrowed for the call, which only reads them.
    unsafe { libc::syscall(libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) }
}

/// `mount_setattr(AT_FDCWD, path, flags, attr, sizeof attr)`.
pub(super) fn mount_setattr(path: &CStr, flags: c_uint, attr: &MountAttr) -> c_long {
    // SAFETY: `path` is a nul-terminated string and `attr` a `struct mount_attr` (`repr(C)`,
    // its size checked above), both borrowed for the call, which reads `size_of` bytes of it
    // and writes neither.
    unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            libc::AT_FDCWD,
            path.as_ptr(),
            flags,
            attr as *const MountAttr,
            size_of::<MountAttr>(),
        )
    }
}

/// `open(path, flags)`: the descriptor, or `None` with `errno` set.
pub(super) fn open(path: &CStr, flags: c_int) -> Option<OwnedFd> {
    // SAFETY: `path` is a nul-terminated string borrowed for the call, which only reads it.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is >= 0, so `open` just made it, and nothing else holds it: the `OwnedFd`
    // is its one owner. A second would close it twice.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(super) fn write(fd: BorrowedFd<'_>, bytes: &[u8]) -> isize {
    // SAFETY: `bytes` is borrowed for the call, which reads `bytes.len()` bytes of it, and
    // `fd` is open while borrowed.
    unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) }
}

/// What `fstat` says of the descriptor numbered `fd`, or `None` (`errno` set) where it is not
/// one. Asking changes nothing of the descriptor, whoever owns it.
pub(super) fn stat_of(fd: c_int) -> Option<libc::stat> {
    // SAFETY: `libc::stat` is integers only, for which all-zero bytes are a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a live, exclusive `struct stat` for the call to fill, and nothing
    // else; a number that is no open descriptor is an error return.
    let done = unsafe { libc::fstat(fd, &mut stat) };
    (done == 0).then_some(stat)
}

/// What `stat` says of `path`, or `lstat` when not `follow`, or `None` with `errno` set.
pub(super) fn stat(path: &CStr, follow: bool) -> Option<libc::stat> {
    // SAFETY: `libc::stat` is integers only, for which all-zero bytes are a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a nul-terminated string borrowed for the call, which only reads it,
    // and `stat` is a live, exclusive `struct stat` for the call to fill, and nothing else.
    let done = unsafe {
        match follow {
            true => libc::stat(path.as_ptr(), &mut stat),
            false => libc::lstat(path.as_ptr(), &mut stat),
        }
    };
    (done == 0).then_some(stat)
}

/// Reads from `fd` into the spare capacity of `into`, which grows by what was read: what
/// `read` returned.
pub(super) fn read(fd: BorrowedFd<'_>, into: &mut Vec<u8>) -> isize {
    let spare = into.spare_capacity_mut();
    // SAFETY: the spare capacity of `into` is exclusive for the call, which writes at most
    // `spare.len()` bytes there; `fd` is open while borrowed.
    let read = unsafe { libc::read(fd.as_raw_fd(), spare.as_mut_ptr().cast(), spare.len()) };
    if read > 0 {
        let len = into.len() + read as usize;
        // SAFETY: the kernel wrote the first `read` bytes of the spare capacity, no more than
        // it has, so the first `len` bytes of `into` are initialized and within its capacity.
        unsafe { into.set_len(len) };
    }
    read
}

/// Lists the directory `fd` is open on from where the last call left off, as `struct
/// linux_dirent64` records, into the spare capacity of `into`, which grows by what was
/// listed: what `getdents64` returned, 0 at the end.
pub(super) fn dents(fd: BorrowedFd<'_>, into: &mut Vec<u8>) -> c_long {
    let spare = into.spare_capacity_mut();
    // SAFETY: the spare capacity of `into` is exclusive for the call, which writes at most
    // `spare.len()` bytes there; `fd` is open while borrowed, and the call takes integers and
    // that pointer alone.
    let read = unsafe {
        libc::syscall(libc::SYS_getdents64, fd.as_raw_fd(), spare.as_mut_ptr(), spare.len())
    };
    if read > 0 {
        let len = into.len() + read as usize;
        // SAFETY: the kernel wrote the first `read` bytes of the spare capacity, no more than
        // it has, so the first `len` bytes of `into` are initialized and within its capacity.
        unsafe { into.set_len(len) };
    }
    read
}

/// # Safety
///
/// `fd` is this process's own, and nothing that will use or close it after holds it (an
/// `OwnedFd`, a `File`, another copy of the number): its number may be the next descriptor
/// made, which that holder would then use or close in its place.
pub(super) unsafe fn close(fd: c_int) -> c_int {
    // SAFETY: the caller owns `fd` alone (# Safety); `close` takes it by value and touches no
    // memory of this process.
    unsafe { libc::close(fd) }
}

/// A connected pair of Unix stream sockets, close-on-exec, or `None` with `errno` set.
pub(super) fn stream_pair() -> Option<[OwnedFd; 2]> {
    let mut sockets = [0; 2];
    // SAFETY: `sockets` is a live, exclusive array of the two `c_int`s the call writes.
    let made = unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, sockets.as_mut_ptr())
    };
    if made < 0 {
        return None;
    }
    // SAFETY: `socketpair` succeeded, so it just made `sockets[0]`, and nothing else holds it:
    // the `OwnedFd` is its one owner. A second would close it twice.
    let first = unsafe { OwnedFd::from_raw_fd(sockets[0]) };
    // SAFETY: as for `sockets[0]`, which is a different descriptor.
    let second = unsafe { OwnedFd::from_raw_fd(sockets[1]) };
    Some([first, second])
}

/// An IPv4 datagram socket, close-on-exec, or `None` with `errno` set.
pub(super) fn datagram_socket() -> Option<OwnedFd> {
    // SAFETY: `socket` takes integers only and touches no memory of this process.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is >= 0, so `socket` just made it, and the `OwnedFd` is its one owner.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `SIOCGIFFLAGS`: the flags of the interface `request` names, into `request.flags`.
pub(super) fn interface_flags(socket: BorrowedFd<'_>, request: &mut IfReq) -> c_int {
    // SAFETY: `request` is a live, exclusive `struct ifreq` (`repr(C)`, 40 bytes, checked
    // above) for the call, which reads its name and writes within it.
    unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS as _, request as *mut IfReq) }
}

/// `SIOCSIFFLAGS`: sets `request.flags` on the interface `request` names.
pub(super) fn set_interface_flags(socket: BorrowedFd<'_>, request: &IfReq) -> c_int {
    // SAFETY: `request` is a `struct ifreq` (`repr(C)`, 40 bytes, checked above) borrowed for
    // the call, which only reads it.
    unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS as _, request as *const IfReq) }
}

/// The Landlock ABI version (`landlock_create_ruleset(NULL, 0, VERSION)`), or below 1 with
/// `errno` set.
pub(super) fn landlock_abi() -> c_long {
    // SAFETY: with `LANDLOCK_CREATE_RULESET_VERSION` the attribute is null and the size 0, as
    // landlock_create_ruleset(2) asks: the kernel reads no memory of this process.
    unsafe {
        libc::syscall(
            landlock::CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            landlock::CREATE_RULESET_VERSION,
        )
    }
}

/// A Landlock ruleset of the first `size` bytes of `attr` (what this kernel's ABI knows), or
/// `None` with `errno` set.
pub(super) fn landlock_ruleset(attr: &RulesetAttr, size: usize) -> Option<OwnedFd> {
    assert!(size <= size_of::<RulesetAttr>(), "a ruleset attribute of {size} bytes");
    // SAFETY: `attr` is a `struct landlock_ruleset_attr` (`repr(C)`, checked above) borrowed
    // for the call, which reads its first `size` bytes, no more than it has (asserted).
    let fd = unsafe { libc::syscall(landlock::CREATE_RULESET, attr as *const RulesetAttr, size, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is >= 0, so `landlock_create_ruleset` just made it, and the `OwnedFd` is
    // its one owner.
    Some(unsafe { OwnedFd::from_raw_fd(fd as c_int) })
}

pub(super) fn landlock_add_rule(ruleset: BorrowedFd<'_>, rule: &PathBeneath) -> c_long {
    // SAFETY: `rule` is a `struct landlock_path_beneath_attr` (`repr(C, packed)`, 12 bytes,
    // checked above) borrowed for the call, which only reads it; `ruleset` is open while
    // borrowed.
    unsafe {
        libc::syscall(
            landlock::ADD_RULE,
            ruleset.as_raw_fd(),
            landlock::RULE_PATH_BENEATH,
            rule as *const PathBeneath,
            0u32,
        )
    }
}

pub(super) fn landlock_restrict_self(ruleset: BorrowedFd<'_>) -> c_long {
    // SAFETY: `landlock_restrict_self` takes integers only and touches no memory of this
    // process; `ruleset` is open while borrowed.
    unsafe { libc::syscall(landlock::RESTRICT_SELF, ruleset.as_raw_fd(), 0u32) }
}

/// Installs `program` as this process's seccomp filter with a listener: the listener's
/// descriptor, or `None` with `errno` set.
pub(super) fn install_filter(program: &[libc::sock_filter]) -> Option<OwnedFd> {
    let prog = libc::sock_fprog { len: program.len() as u16, filter: program.as_ptr().cast_mut() };
    // SAFETY: `prog` points at `program`, borrowed for the call, with a length no longer than
    // it; the kernel copies the instructions in and writes nothing through the pointer.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog as *const libc::sock_fprog,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is >= 0, so `seccomp` just made the listener, and the `OwnedFd` is its one
    // owner.
    Some(unsafe { OwnedFd::from_raw_fd(fd as c_int) })
}

/// Room for one control message of a descriptor, aligned as a `cmsghdr` must be: the kernel
/// and the `CMSG_*` helpers read and write its header fields in place.
#[repr(C, align(8))]
struct Control([u8; 64]);

const _: () = assert!(align_of::<libc::cmsghdr>() <= align_of::<Control>());

/// A `msghdr` of the one buffer `iov` and the first `controllen` bytes of `space`.
fn message(iov: &mut libc::iovec, space: &mut Control, controllen: usize) -> libc::msghdr {
    // SAFETY: `msghdr` is integers and raw pointers, for which all-zero bytes (null pointers,
    // zero lengths) are a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.0.as_mut_ptr().cast();
    msg.msg_controllen = controllen as _;
    msg
}

/// Sends the descriptor `fd` over `socket` (`SCM_RIGHTS`): what `sendmsg` returned.
pub(super) fn send_descriptor(socket: c_int, fd: c_int) -> isize {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut space = Control([0; 64]);
    // SAFETY: `CMSG_SPACE` is arithmetic on its argument and reads no memory.
    let room = unsafe { libc::CMSG_SPACE(size_of::<c_int>() as u32) } as usize;
    let msg = message(&mut iov, &mut space, room);
    // SAFETY: `msg`'s control buffer is `space`, `room` (24 bytes on 64-bit, 16 on 32-bit) of
    // its 64, at least one `cmsghdr` long, so the first header is at its start and not null.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    // SAFETY: `cmsg` is the start of `space`, live, aligned for a `cmsghdr` (`Control`) and
    // that long, and nothing else reads or writes `space` until `sendmsg`.
    unsafe { (*cmsg).cmsg_level = libc::SOL_SOCKET };
    // SAFETY: as just above.
    unsafe { (*cmsg).cmsg_type = libc::SCM_RIGHTS };
    // SAFETY: `CMSG_LEN` is arithmetic on its argument and reads no memory.
    let len = unsafe { libc::CMSG_LEN(size_of::<c_int>() as u32) };
    // SAFETY: as for `cmsg_level` above.
    unsafe { (*cmsg).cmsg_len = len as _ };
    // SAFETY: `CMSG_DATA` is `cmsg` plus the aligned header length, inside `space`.
    let data = unsafe { libc::CMSG_DATA(cmsg) };
    // SAFETY: `data` is the header's aligned size into `space` (16 bytes on 64-bit, 12 on
    // 32-bit), so the `c_int` ends by 20 of its 64; the data need not be aligned for a
    // `c_int`, so the write is unaligned.
    unsafe { std::ptr::write_unaligned(data.cast::<c_int>(), fd) };
    // SAFETY: `msg` points at `iov`, `byte` and `space`, all live and unborrowed for the call,
    // which only reads them.
    unsafe { libc::sendmsg(socket, &msg, 0) }
}

/// The descriptor sent over `socket` (close-on-exec), or `None` when the other end closed
/// first or sent none.
pub(super) fn received_descriptor(socket: BorrowedFd<'_>) -> Option<OwnedFd> {
    let socket = socket.as_raw_fd();
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut space = Control([0; 64]);
    let mut msg = message(&mut iov, &mut space, 64);
    loop {
        // SAFETY: `msg` points at `iov`, `byte` and `space`, live and exclusive for the call,
        // which writes at most `iov_len` (1) and `msg_controllen` (64) bytes into them.
        let read = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if read <= 0 {
            return None;
        }
        break;
    }
    // SAFETY: `CMSG_FIRSTHDR` reads `msg`'s control fields, which the kernel left within
    // `space`, and gives its start or null.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    // SAFETY: not null, so `cmsg` is the start of `space`, aligned for a `cmsghdr`
    // (`Control`), and the kernel wrote the header there.
    if cmsg.is_null() || unsafe { (*cmsg).cmsg_type } != libc::SCM_RIGHTS {
        return None;
    }
    // SAFETY: `CMSG_DATA` is `cmsg` plus the aligned header length, inside `space`.
    let data = unsafe { libc::CMSG_DATA(cmsg) };
    // SAFETY: `data` is the header's aligned size into `space` (16 bytes on 64-bit, 12 on
    // 32-bit), so the `c_int` read ends by 20 of its 64 initialized bytes; the data need not
    // be aligned for a `c_int`.
    let fd = unsafe { std::ptr::read_unaligned(data.cast::<c_int>()) };
    // SAFETY: an `SCM_RIGHTS` message's descriptor is one the kernel just installed in this
    // process for this receive, so nothing else holds it: the `OwnedFd` is its one owner.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `SECCOMP_IOCTL_NOTIF_SET_FLAGS` on `listener`.
pub(super) fn set_notif_flags(listener: c_int, flags: c_ulong) -> c_int {
    // SAFETY: the flags are the argument itself, not a pointer: the kernel reads no memory
    // of this process.
    unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SET_FLAGS as _, flags) }
}

/// Takes a notification from `listener` into `notif`.
pub(super) fn notif_recv(listener: c_int, notif: &mut Notif) -> c_int {
    // SAFETY: `notif` is a live, exclusive `struct seccomp_notif` (`repr(C)`, 80 bytes, the
    // size the ioctl number names, checked above) for the call to fill.
    unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_RECV as _, notif as *mut Notif) }
}

/// Answers a notification on `listener` with `resp`.
pub(super) fn notif_send(listener: c_int, resp: &NotifResp) -> c_int {
    // SAFETY: `resp` is a `struct seccomp_notif_resp` (`repr(C)`, 24 bytes, the size the
    // ioctl number names, checked above) borrowed for the call, which only reads it.
    unsafe { libc::ioctl(listener, SECCOMP_IOCTL_NOTIF_SEND as _, resp as *const NotifResp) }
}

/// A pidfd of `pid`, or `None` with `errno` set.
pub(super) fn pidfd_open(pid: pid_t) -> Option<OwnedFd> {
    // SAFETY: `pidfd_open` takes integers only and touches no memory of this process.
    let fd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid, 0) } as c_int;
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is >= 0, so `pidfd_open` just made it, and the `OwnedFd` is its one owner.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(super) fn poll(fds: &mut [libc::pollfd], timeout: c_int) -> c_int {
    // SAFETY: `fds` is live and exclusive for the call, which reads and writes `fds.len()`
    // entries of it and no more.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) }
}

/// Reads `pid`'s memory at `remote` (its addresses, which are never dereferenced here) into
/// the spare capacity of `into`, which grows by what was read: what `process_vm_readv`
/// returned.
pub(super) fn read_memory_into(pid: pid_t, remote: &[libc::iovec], into: &mut Vec<u8>) -> isize {
    let spare = into.spare_capacity_mut();
    let local = libc::iovec { iov_base: spare.as_mut_ptr().cast(), iov_len: spare.len() };
    // SAFETY: `local` is the spare capacity of `into`, exclusive for the call, which writes at
    // most `spare.len()` bytes there. `remote` only names addresses of the other process,
    // which the kernel checks against that process's mappings.
    let read = unsafe {
        libc::process_vm_readv(pid, &local, 1, remote.as_ptr(), remote.len() as _, 0)
    };
    if read > 0 {
        let len = into.len() + read as usize;
        // SAFETY: the kernel wrote the first `read` bytes of the spare capacity, no more than
        // it has, so the first `len` bytes of `into` are initialized and within its capacity.
        unsafe { into.set_len(len) };
    }
    read
}

/// Reads the link at `path` into the capacity of the empty `into`, whose length becomes the
/// link's when it fits with room to spare: what `readlink` returned. A link as long as the
/// room may be cut short, so its length is left 0 then.
pub(super) fn readlink_into(path: &CStr, into: &mut Vec<u8>) -> isize {
    into.clear();
    let room = into.capacity();
    // SAFETY: `path` is a nul-terminated string borrowed for the call, and `into`'s buffer is
    // exclusive for it, which writes at most `room` bytes there.
    let read = unsafe { libc::readlink(path.as_ptr(), into.as_mut_ptr().cast(), room) };
    if read >= 0 && (read as usize) < room {
        // SAFETY: the kernel wrote the first `read` bytes, fewer than the capacity.
        unsafe { into.set_len(read as usize) };
    }
    read
}

/// Runs `argv[0]` with `argv` and `env` in place of this process. It returns only where it
/// couldn't, with `errno` set.
pub(super) fn execve(argv: &[CString], env: &[CString]) {
    let mut args: Vec<*const libc::c_char> = Vec::with_capacity(argv.len() + 1);
    args.extend(argv.iter().map(|it| it.as_ptr()));
    args.push(std::ptr::null());
    let mut envp: Vec<*const libc::c_char> = Vec::with_capacity(env.len() + 1);
    envp.extend(env.iter().map(|it| it.as_ptr()));
    envp.push(std::ptr::null());
    // SAFETY: `args` and `envp` end with null and each other entry is a nul-terminated string
    // of `argv` or `env`, borrowed for the call; `args[0]` is the program's path, or null with
    // no `argv`, which the kernel refuses (EFAULT). Nothing is written through them.
    unsafe { libc::execve(args[0], args.as_ptr(), envp.as_ptr()) };
}
