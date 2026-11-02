//! What Linux and macOS share: the channel's Unix socket with file descriptors passed by
//! `SCM_RIGHTS`, the listener and its peer checks, the asks' doorbell and bells, stack walks
//! through the system unwinder, context synchronization, randomness and the protection of an
//! access.

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::{Access, Error};

/// The most descriptors one message carries.
pub(crate) const MOST_FDS: usize = 8;

fn os(what: &str) -> Error {
    Error::new(format!("{what}: {}", std::io::Error::last_os_error()))
}

pub(crate) struct Stream {
    fd: OwnedFd,
    /// Descriptors that came with bytes already read, in order.
    fds: VecDeque<OwnedFd>,
}

impl Stream {
    fn new(fd: OwnedFd) -> Result<Stream, Error> {
        #[cfg(target_os = "macos")]
        {
            let on: c_int = 1;
            // SAFETY: `fd` is an open socket; the option value is a live `c_int` of the size
            // passed.
            let set = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const on).cast(),
                    size_of::<c_int>() as libc::socklen_t,
                )
            };
            if set != 0 {
                return Err(os("setting SO_NOSIGPIPE on the channel"));
            }
        }
        Ok(Stream { fd, fds: VecDeque::new() })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn raw(&self) -> i64 {
        i64::from(self.fd.as_raw_fd())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn from_raw(raw: i64) -> Result<Stream, Error> {
        let fd = RawFd::try_from(raw).map_err(|_| Error::new(format!("a channel fd of {raw}")))?;
        // SAFETY: a zeroed `stat` is a valid one: plain integers.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `stat` is a live, writable `libc::stat`; a closed `fd` fails with EBADF.
        if fd < 0 || unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
            return Err(os(&format!("the channel fd {fd}")));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(Error::new(format!("the channel fd {fd} is not a socket")));
        }
        // SAFETY: a test hands over a socket of a `socketpair` it made and owns nothing else
        // of it.
        Stream::new(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub(crate) fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), Error> {
        let value = match timeout {
            Some(timeout) => libc::timeval {
                tv_sec: timeout.as_secs().min(i32::MAX as u64) as libc::time_t,
                tv_usec: timeout.subsec_micros().max(u32::from(timeout.is_zero())) as libc::suseconds_t,
            },
            None => libc::timeval { tv_sec: 0, tv_usec: 0 },
        };
        for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            // SAFETY: `fd` is an open socket; `value` is a live `timeval` of the size passed.
            let set = unsafe {
                libc::setsockopt(
                    self.fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    (&raw const value).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            if set != 0 {
                return Err(os("setting the channel's timeout"));
            }
        }
        Ok(())
    }

    /// Reads what came, keeping the descriptors that came with it.
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let mut control = [0u64; 2 + MOST_FDS];
        loop {
            let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
            // SAFETY: a zeroed `msghdr` is a valid empty one; its fields are set below.
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &raw mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = size_of_val(&control) as _;
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_CMSG_CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let flags = 0;
            // SAFETY: `message` points at `iov` (into `buf`, writable for its length) and at
            // `control`, both live for the call.
            let read = unsafe { libc::recvmsg(self.fd.as_raw_fd(), &raw mut message, flags) };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                match error.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        return Err(Error::new("reading the channel timed out"));
                    }
                    _ => return Err(Error::new(format!("reading the channel: {error}"))),
                }
            }
            self.take_rights(&message)?;
            if message.msg_flags & libc::MSG_CTRUNC != 0 {
                return Err(Error::new(format!("more than {MOST_FDS} descriptors came in one message")));
            }
            return Ok(read as usize);
        }
    }

    fn take_rights(&mut self, message: &libc::msghdr) -> Result<(), Error> {
        // SAFETY: `message` was filled by `recvmsg`, whose control buffer is still live; the
        // CMSG macros walk only within `msg_controllen` of it.
        let mut header = unsafe { libc::CMSG_FIRSTHDR(message) };
        while !header.is_null() {
            // SAFETY: `header` is a control message header within the buffer (CMSG_FIRSTHDR,
            // CMSG_NXTHDR never return one outside it).
            let level = unsafe { (*header).cmsg_level };
            // SAFETY: `header` is a control message header within the buffer.
            let kind = unsafe { (*header).cmsg_type };
            // SAFETY: `header` is a control message header within the buffer.
            let len = unsafe { (*header).cmsg_len as usize };
            if level == libc::SOL_SOCKET && kind == libc::SCM_RIGHTS {
                // SAFETY: `CMSG_LEN(0)` is the header's own length, a constant.
                let data_len = len.saturating_sub(unsafe { libc::CMSG_LEN(0) } as usize);
                let count = data_len / size_of::<c_int>();
                // SAFETY: the data of an SCM_RIGHTS message is `count` descriptors, in the buffer.
                let data = unsafe { libc::CMSG_DATA(header) }.cast::<c_int>();
                for index in 0..count {
                    // SAFETY: `index < count`, in the message's data; unaligned reads, since
                    // the data is only as aligned as the header puts it.
                    let at = unsafe { data.add(index) };
                    // SAFETY: `at` is in the message data; read unaligned as above.
                    let fd = unsafe { at.read_unaligned() };
                    // SAFETY: the kernel installed `fd` in this process for this message:
                    // nothing else owns it.
                    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                    #[cfg(not(target_os = "linux"))]
                    {
                        // SAFETY: `fd` is open; FD_CLOEXEC only marks it.
                        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
                    }
                    self.fds.push_back(fd);
                }
            }
            // SAFETY: as for CMSG_FIRSTHDR.
            header = unsafe { libc::CMSG_NXTHDR(message, header) };
        }
        Ok(())
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.send(bytes, &[])
    }

    /// Sends `bytes`, with `fds` on the first of them.
    pub(crate) fn send(&mut self, mut bytes: &[u8], fds: &[RawFd]) -> Result<(), Error> {
        if fds.len() > MOST_FDS {
            return Err(Error::new(format!("{} descriptors in one message", fds.len())));
        }
        let mut control = [0u64; 2 + MOST_FDS];
        let mut fds = fds;
        while !bytes.is_empty() {
            let mut iov = libc::iovec { iov_base: bytes.as_ptr().cast_mut().cast(), iov_len: bytes.len() };
            // SAFETY: a zeroed `msghdr` is a valid empty one; its fields are set below.
            let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
            message.msg_iov = &raw mut iov;
            message.msg_iovlen = 1;
            if !fds.is_empty() {
                let data_len = size_of_val(fds) as u32;
                // SAFETY: CMSG_SPACE computes a length from a length.
                let space = unsafe { libc::CMSG_SPACE(data_len) } as usize;
                message.msg_control = control.as_mut_ptr().cast();
                message.msg_controllen = space as _;
                // SAFETY: the control buffer is `space` bytes (at most its size: `MOST_FDS`
                // descriptors), so its first header is in it.
                let header = unsafe { libc::CMSG_FIRSTHDR(&raw const message) };
                // SAFETY: `header` is the first header of the buffer, writable; its data is
                // `data_len` bytes inside `space`.
                unsafe { (*header).cmsg_level = libc::SOL_SOCKET };
                // SAFETY: `header` is the first writable header of the buffer.
                unsafe { (*header).cmsg_type = libc::SCM_RIGHTS };
                // SAFETY: CMSG_LEN computes a length from a length.
                let len = unsafe { libc::CMSG_LEN(data_len) };
                // SAFETY: `header` is the first writable header of the buffer.
                unsafe { (*header).cmsg_len = len as _ };
                // SAFETY: `header` is the first header; its data is `data_len` bytes inside `space`.
                let data = unsafe { libc::CMSG_DATA(header) };
                // SAFETY: the header data is `data_len` bytes inside the writable buffer.
                unsafe { std::ptr::copy_nonoverlapping(fds.as_ptr().cast::<u8>(), data, data_len as usize) };
            }
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_NOSIGNAL;
            #[cfg(not(target_os = "linux"))]
            let flags = 0;
            // SAFETY: `message` points at `iov` (into `bytes`) and at `control`, live for the call.
            let sent = unsafe { libc::sendmsg(self.fd.as_raw_fd(), &raw const message, flags) };
            if sent < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(Error::new(format!("writing the channel: {error}")));
            }
            fds = &[];
            bytes = &bytes[sent as usize..];
        }
        Ok(())
    }

    /// The next descriptor that came with what was read.
    pub(crate) fn take_fd(&mut self) -> Option<OwnedFd> {
        self.fds.pop_front()
    }

    /// One byte, the asks' doorbell. Any number of threads ring at once: a byte carries nothing
    /// to interleave.
    fn ring(&self) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_NOSIGNAL;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let byte = [1u8];
        loop {
            // SAFETY: one byte of a live array, to the open socket this stream owns.
            let sent = unsafe { libc::send(self.fd.as_raw_fd(), byte.as_ptr().cast(), 1, flags) };
            if sent == 1 {
                return Ok(());
            }
            if sent < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os("ringing the daemon"));
        }
    }

    /// Whether the other end wrote or hung up.
    fn spoke(&self) -> bool {
        let mut poll = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live `pollfd`; a zero timeout returns at once.
        unsafe { libc::poll(&raw mut poll, 1, 0) != 0 }
    }
}

pub(crate) fn ring(stream: &Stream, _: u64) -> Result<(), Error> {
    stream.ring()
}

/// After the start the daemon sends a run process nothing, so its end speaking is its end
/// closing.
pub(crate) fn hung_up(stream: &Stream) -> bool {
    stream.spoke()
}

pub(crate) fn wait_answer(word: &AtomicU64, expected: u32, _: u64, timeout: Duration) -> bool {
    super::wait(word, expected, timeout)
}

/// Askers sleep on their slots' words, shared with the daemon, and ring on the channel.
pub(crate) struct Bells;

impl Bells {
    pub(crate) fn new(_: &super::Remote, slots: usize) -> Result<(Bells, Vec<u64>), Error> {
        Ok((Bells, vec![0; slots + 1]))
    }

    pub(crate) fn wake(&self, _: usize, word: &AtomicU64) {
        super::wake(word);
    }

    pub(crate) fn wait_ring(&self, stream: &mut Stream) -> Result<bool, Error> {
        let mut bytes = [0u8; 256];
        Ok(stream.read(&mut bytes)? != 0)
    }
}

/// The calling thread's `errno`, put back when dropped.
pub(crate) struct Errno(c_int);

impl Errno {
    pub(crate) fn keep() -> Errno {
        Errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

impl Drop for Errno {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        // SAFETY: the calling thread's `errno`, a live thread-local `int`.
        unsafe {
            *libc::__errno_location() = self.0;
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: the calling thread's `errno`, a live thread-local `int`.
            let errno = unsafe { libc::__error() };
            // SAFETY: `errno` points to the calling thread's live thread-local `int`.
            unsafe { *errno = self.0 };
        }
    }
}

pub(crate) struct Listener {
    listener: UnixListener,
    path: std::path::PathBuf,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Listener {
    /// Listens at `dir/inproc-<pid>.sock`; `dir` is made, or must already be, this user's
    /// only.
    pub(crate) fn bind(dir: &Path) -> Result<(Listener, String), Error> {
        let fail = |it: std::io::Error| Error::new(format!("{}: {it}", dir.display()));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(fail)?;
        let meta = std::fs::metadata(dir).map_err(fail)?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::new(format!(
                "{} is reachable by others (owner {}, mode {:o}): the host listens only where this user alone can",
                dir.display(),
                meta.uid(),
                meta.permissions().mode() & 0o777
            )));
        }
        let path = dir.join(format!("inproc-{}.sock", std::process::id()));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::new(format!("{}: {error}", path.display()))),
        }
        let address = path
            .to_str()
            .ok_or_else(|| Error::new(format!("{} is not UTF-8", path.display())))?
            .to_owned();
        let listener = UnixListener::bind(&path).map_err(|it| Error::new(format!("{address}: {it}")))?;
        Ok((Listener { listener, path }, address))
    }

    /// The next connection, from a process of this user, and its process ID.
    pub(crate) fn accept(&self) -> Result<(Stream, u32), Error> {
        loop {
            let (stream, _) = self.listener.accept().map_err(|it| Error::new(format!("accepting a run process: {it}")))?;
            let fd = OwnedFd::from(stream);
            match peer(&fd) {
                Ok(pid) => return Ok((Stream::new(fd)?, pid)),
                Err(error) => eprintln!("{error}"),
            }
        }
    }
}

/// The process at the other end of `fd`, when it is this user's.
fn peer(fd: &OwnedFd) -> Result<u32, Error> {
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    #[cfg(target_os = "linux")]
    {
        // SAFETY: a zeroed `ucred` is a valid one.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `fd` is a connected socket; `cred` and `len` are live and writable.
        let got = unsafe {
            libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &raw mut len)
        };
        if got != 0 {
            return Err(os("reading a run process's credentials"));
        }
        if cred.uid != me {
            return Err(Error::new(format!("a connection from process {} of user {}, refused", cred.pid, cred.uid)));
        }
        Ok(cred.pid as u32)
    }
    #[cfg(target_os = "macos")]
    {
        const SOL_LOCAL: c_int = 0;
        const LOCAL_PEERPID: c_int = 2;
        let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
        // SAFETY: `fd` is a connected Unix socket; `uid` and `gid` are live and writable.
        if unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) } != 0 {
            return Err(os("reading a run process's user"));
        }
        if uid != me {
            return Err(Error::new(format!("a connection from user {uid}, refused")));
        }
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: as above; `pid` and `len` are live and writable.
        let got = unsafe { libc::getsockopt(fd.as_raw_fd(), SOL_LOCAL, LOCAL_PEERPID, (&raw mut pid).cast(), &raw mut len) };
        if got != 0 {
            return Err(os("reading a run process's process"));
        }
        Ok(pid as u32)
    }
}

pub(crate) fn connect(address: &str) -> Result<Stream, Error> {
    let stream = UnixStream::connect(address).map_err(|it| Error::new(format!("connecting to the daemon at {address}: {it}")))?;
    Stream::new(OwnedFd::from(stream))
}

pub(crate) fn random(out: &mut [u8; 16]) -> Result<(), Error> {
    // SAFETY: `out` is 16 writable bytes, within getentropy's limit of 256.
    if unsafe { libc::getentropy(out.as_mut_ptr().cast(), out.len()) } != 0 {
        return Err(os("getentropy"));
    }
    Ok(())
}

/// The process's global scope: what `dlsym(RTLD_DEFAULT, ..)` searches, the executable and
/// the libraries the system loader loaded for it, in its order.
pub(crate) struct Scope {
    /// The name being looked up, NUL-terminated; reused.
    name: Vec<u8>,
}

impl Scope {
    pub(crate) fn new() -> Scope {
        Scope { name: Vec::new() }
    }

    pub(crate) fn find(&mut self, name: &str) -> Result<Option<u64>, Error> {
        if name.is_empty() || name.as_bytes().contains(&0) {
            return Err(Error::new(format!("a lookup of {name:?}: not a C name")));
        }
        self.name.clear();
        self.name.reserve(name.len() + 1);
        self.name.extend_from_slice(name.as_bytes());
        self.name.push(0);
        // SAFETY: `self.name` is NUL-terminated with no NUL before its end (checked above) and
        // lives for the call; RTLD_DEFAULT asks the loader's global scope, which it only reads.
        let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, self.name.as_ptr().cast()) };
        Ok((!found.is_null()).then_some(found as u64))
    }
}

pub(crate) fn protection(access: Access) -> c_int {
    match access {
        Access::Execute => libc::PROT_READ | libc::PROT_EXEC,
        Access::Read => libc::PROT_READ,
        Access::Copy | Access::Shared => libc::PROT_READ | libc::PROT_WRITE,
    }
}

pub(crate) fn serialize() {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: `isb` only synchronizes this core's context; it reads and writes no memory.
    unsafe {
        std::arch::asm!("isb", options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `cpuid` of leaf 0 serializes the core and writes only the registers named;
    // `rbx`, which LLVM reserves, is saved in a scratch register and put back.
    unsafe {
        std::arch::asm!(
            "mov {keep}, rbx",
            "cpuid",
            "mov rbx, {keep}",
            keep = out(reg) _,
            inout("eax") 0u32 => _,
            inout("ecx") 0u32 => _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
}

const URC_NO_REASON: c_int = 0;
const URC_END_OF_STACK: c_int = 5;

unsafe extern "C" {
    fn _Unwind_Backtrace(
        trace: extern "C" fn(context: *mut c_void, state: *mut c_void) -> c_int,
        state: *mut c_void,
    ) -> c_int;
    fn _Unwind_GetIP(context: *mut c_void) -> usize;
}

struct Walk<'a> {
    out: &'a mut [u64],
    len: usize,
    full: bool,
}

extern "C" fn step(context: *mut c_void, state: *mut c_void) -> c_int {
    // SAFETY: `state` is the `Walk` `backtrace` passed, live and borrowed by nothing else
    // for the walk.
    let walk = unsafe { &mut *state.cast::<Walk<'_>>() };
    // SAFETY: `context` is the unwinder's context of the frame it is at.
    let ip = unsafe { _Unwind_GetIP(context) } as u64;
    if walk.len == walk.out.len() {
        walk.full = true;
        return URC_END_OF_STACK;
    }
    walk.out[walk.len] = ip;
    walk.len += 1;
    URC_NO_REASON
}

/// The calling thread's return addresses, through the unwinder that runs its panics, so it
/// sees every frame they would: code placed in a code memory registers its unwind info before
/// its slot is stored.
pub(crate) fn backtrace(out: &mut [u64]) -> Option<usize> {
    let mut walk = Walk { out, len: 0, full: false };
    // SAFETY: `step` matches the callback's signature and is given `walk`, live for the call.
    let result = unsafe { _Unwind_Backtrace(step, (&raw mut walk).cast()) };
    (result == URC_END_OF_STACK && !walk.full).then_some(walk.len)
}
