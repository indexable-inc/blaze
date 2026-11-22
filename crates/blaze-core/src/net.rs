//! The sockets behind [`crate::Effect::Listen`] and [`crate::Effect::Connect`], for every
//! driver on Unix: made here, then read and written as each driver does (blocking on a thread,
//! through the ring, through a kqueue).
//!
//! Why unsafe: a socket made here is handed to a driver by descriptor and taken back as a std
//! stream (`from_raw_fd`, owned once), and std has no `recv` into a `Vec`'s spare room: its
//! `Read::read` needs initialized bytes, which would zero each buffer before every read (a
//! write of the whole buffer per receive). `sockaddr_storage` is viewed as the family it holds
//! only after its `ss_family` says which.

use core::ffi::c_int;
use std::{
    io::{self, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::{
            ffi::OsStrExt as _,
            fs::PermissionsExt as _,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
};

use blaze_sys::{
    Fd,
    posix::{net as sys, types::{sa_family_t, socklen_t}},
};

use crate::{Address, RECV_MOST};

pub(crate) enum Listener {
    /// `path` is the file it made and its identity, removed when it is dropped while the name
    /// is still that file; `None` for an inherited one. `user`: the one user whose connections
    /// it accepts ([`peer_is`]), this process's effective uid when it was made.
    Unix {
        listener: UnixListener,
        path: Option<(PathBuf, Made)>,
        user: u32,
    },
    /// With its options, set again on each connection it accepts.
    Tcp(TcpListener, crate::sockopt::Tcp),
}

impl Listener {
    pub(crate) fn fd(&self) -> RawFd {
        match self {
            Listener::Unix { listener, .. } => listener.as_raw_fd(),
            Listener::Tcp(listener, _) => listener.as_raw_fd(),
        }
    }

    pub(crate) fn is_unix(&self) -> bool {
        matches!(self, Listener::Unix { .. })
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self {
            Listener::Unix { listener, .. } => listener.set_nonblocking(on),
            Listener::Tcp(listener, _) => listener.set_nonblocking(on),
        }
    }

    /// The address a TCP listen was given (its port, where it asked for port 0).
    pub(crate) fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Listener::Unix { .. } => None,
            Listener::Tcp(listener, _) => listener.local_addr().ok(),
        }
    }

    /// The next connection. A Unix one is first checked to be of this listener's user
    /// ([`peer_is`]): one of another user is closed unread and said as `ConnectionAborted`,
    /// which every driver's accept loop takes as a connection gone before it was taken and
    /// goes on listening (kqueue.rs `accept`, threads.rs, the accept thread). The io_uring
    /// driver accepts on the listener's descriptor itself and takes the same decision through
    /// [`Listener::admit`] (uring/flight.rs `admitted`).
    pub(crate) fn accept(&self) -> io::Result<Socket> {
        match self {
            Listener::Unix { listener, user, .. } => {
                let stream = listener.accept()?.0;
                peer_is(Fd::new(stream.as_raw_fd()), *user)?;
                Ok(Socket::Unix(stream))
            }
            Listener::Tcp(listener, options) => {
                let (stream, _) = listener.accept()?;
                crate::sockopt::accepted(&stream, options)?;
                Ok(Socket::Tcp(stream))
            }
        }
    }

    /// A connection a driver accepted on this listener's descriptor itself (the io_uring
    /// driver's accept, uring/flight.rs `admitted`) checked as [`Listener::accept`] checks its
    /// own, by the same decision: a Unix listener's peer by [`peer_is`]; a TCP one's nothing
    /// (its options were set on the listener before its bind, and a connection inherits them).
    /// Refused: `ConnectionAborted`, and the caller closes `peer` unread.
    #[cfg(target_os = "linux")]
    pub(crate) fn admit(&self, peer: Fd) -> io::Result<()> {
        match self {
            Listener::Unix { user, .. } => peer_is(peer, *user),
            Listener::Tcp(..) => Ok(()),
        }
    }

    /// A test plays another user: no second user exists where tests run, so the listener is
    /// told another uid and this process's own connection is the foreign one.
    #[cfg(test)]
    pub(crate) fn answer_only(&mut self, uid: u32) {
        if let Listener::Unix { user, .. } = self {
            *user = uid;
        }
    }
}

/// The second wall of a Unix socket blaze listens on (the first: the folder it is made in is
/// the user's alone, bstd's private-folder check, and the socket is 0600 before its name
/// appears, [`listen`]): the connecting process's effective uid, as the kernel recorded it at
/// its connect, is `user`'s, or the connection is refused before any byte of it is read.
/// macOS: `getpeereid` (`LOCAL_PEERCRED`'s uid); Linux: `SO_PEERCRED`. A peer whose uid cannot
/// be read is refused too (rule 3: what is not proven is not let in). The refusal is
/// `ConnectionAborted`, naming both uids.
///
/// A peer may hang up before its connection is accepted. Whether its uid can still be read
/// then is the kernel's (it records the uid at the connect); where it cannot, the connection
/// is refused ([`peer_judged`]'s `Err` arm), never taken and never a panic.
fn peer_is(fd: Fd, user: u32) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let peer = sys::peer_ids(fd).map(|(uid, _)| uid);
    #[cfg(target_os = "linux")]
    let peer = sys::peer_cred(fd).map(|cred| cred.uid);
    peer_judged(peer, user)
}

/// [`peer_is`]'s decision on the peer's uid as it was read (sans-IO, so its refusal of an
/// unreadable uid is tested without a kernel that refuses the read).
fn peer_judged(peer: Result<u32, blaze_sys::Errno>, user: u32) -> io::Result<()> {
    match peer {
        Ok(uid) if uid == user => Ok(()),
        Ok(uid) => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            // alloc: an error path.
            format!("a connection of uid {uid} refused unread: this socket answers uid {user} alone (blaze-core net.rs `peer_is`)"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            // alloc: an error path.
            format!(
                "a connection refused unread: its peer's uid could not be read ({}), and this socket answers uid {user} alone (blaze-core net.rs `peer_is`)",
                io::Error::from_raw_os_error(i32::from(error.raw()))
            ),
        )),
    }
}

/// The device and inode of the socket file a listen made: its name may name another's by the
/// time the listener is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Made {
    dev: u64,
    ino: u64,
}

impl Made {
    fn of(path: &Path) -> io::Result<Made> {
        use std::os::unix::fs::MetadataExt as _;
        let it = std::fs::symlink_metadata(path)?;
        Ok(Made { dev: it.dev(), ino: it.ino() })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // A daemon whose listener goes after the next one renamed its socket into the name (the
        // threads driver ends a listen on its accept thread, after the hang-ups) leaves that
        // one's be. The look and the unlink are two calls: a rename into the name between them
        // is still lost, a window of one system call where it was the whole exit before.
        if let Listener::Unix { path: Some((path, made)), .. } = self
            && Made::of(path).is_ok_and(|now| now == *made)
        {
            _ = std::fs::remove_file(&*path);
        }
    }
}

/// A socket listening at `at`. A Unix socket is made under a temporary name, given mode 0600
/// and renamed into place, so no other user can connect in between; a name left by a daemon
/// that is gone is replaced, one another process listens on is refused.
pub(crate) fn listen(root: &Path, at: &Address, options: &crate::sockopt::Tcp) -> io::Result<Listener> {
    if !matches!(at, Address::Tcp(_)) {
        crate::sockopt::unix_refused(options)?;
    }
    match at {
        Address::Unix(name) => {
            let path = root.join(name.as_str());
            crate::fs::make_parent(&path)?;
            if path.exists() {
                if UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        // alloc: an error path.
                        format!("{}: another process listens on it", path.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            if std::os::unix::net::SocketAddr::from_pathname(&path).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    // alloc: an error path.
                    format!("{}: longer than a socket's path may be", path.display()),
                ));
            }
            let temporary = crate::fs::temporary_next_to(&path);
            let listener = bind(&temporary)?;
            let placed =
                std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                    .and_then(|()| Made::of(&temporary))
                    .and_then(|made| std::fs::rename(&temporary, &path).map(|()| made));
            let made = match placed {
                Ok(made) => made,
                Err(error) => {
                    _ = std::fs::remove_file(&temporary);
                    return Err(error);
                }
            };
            Ok(Listener::Unix { listener, path: Some((path, made)), user: blaze_sys::posix::thread::geteuid() })
        }
        Address::Inherited { fd, at } => inherited(root, *fd, at),
        Address::Tcp(address) => {
            let listener = crate::sockopt::listen_tcp(*address, options.get())?;
            // clone: the listen's options, kept to set on each connection it accepts.
            Ok(Listener::Tcp(listener, options.clone()))
        }
    }
}

/// A socket bound at `path`. One whose name fits but whose temporary name next to it does not is
/// bound through a short directory of this process's own under `/tmp`, made with mode 0700, whose
/// one entry links to `path`'s directory: the socket is still made in that directory, so its
/// rename into place stays atomic.
#[expect(
    blaze::tmp_path,
    reason = "the real /tmp is the policy here: the short directory a too-long socket name is bound through must \
              be under a short base, checked to be one nobody else can replace entries of, and lives only for the \
              bind; TMPDIR is the long name this exists to avoid"
)]
fn bind(path: &Path) -> io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    if std::os::unix::net::SocketAddr::from_pathname(path).is_ok() {
        return UnixListener::bind(path);
    }
    let tmp = Path::new("/tmp");
    // alloc: once per listen whose temporary name is too long, at setup.
    let short = crate::fs::temporary_next_to(&tmp.join("blaze-bind"));
    let through = || -> io::Result<UnixListener> {
        // A link above `/tmp` is followed: what matters is the directory the name is looked up
        // in, which nobody else may replace entries of.
        let is = std::fs::metadata(tmp)?;
        let user = blaze_sys::posix::thread::geteuid();
        if (is.uid() != user && is.uid() != 0)
            || (is.mode() & 0o002 != 0 && is.mode() & 0o1000 == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "another user can replace what is in it",
            ));
        }
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no directory to bind in"));
        };
        let dir = std::path::absolute(dir)?;
        std::fs::DirBuilder::new().mode(0o700).create(&short)?;
        let alias = short.join("d");
        let bound = std::os::unix::fs::symlink(&dir, &alias)
            .and_then(|()| UnixListener::bind(alias.join(name)));
        _ = std::fs::remove_dir_all(&short);
        bound
    };
    through().map_err(|error| {
        io::Error::new(
            error.kind(),
            // alloc: an error path.
            format!("binding {} through {}: {error}", path.display(), short.display()),
        )
    })
}

/// The listening socket a service manager passed as `fd`, for [`Address::Inherited`]: a
/// close-on-exec copy of it, once it is a listening stream socket bound at `at`.
fn inherited(root: &Path, fd: RawFd, at: &crate::FileName) -> io::Result<Listener> {
    // alloc: once per listen, at setup.
    let path = root.join(at.as_str());
    // alloc: an error path, every use of it.
    let refused = |what: String| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!("the socket passed as fd {fd} for {}: {what}", path.display()),
        )
    };
    let copy = blaze_sys::posix::io::dup_at_least(Fd::new(fd), 0, true).map_err(|error| {
        let error = io::Error::from_raw_os_error(i32::from(error.raw()));
        // alloc: an error path.
        refused(format!("not open ({error})"))
    })?;
    // SAFETY: `into_raw` transfers sole ownership of the new descriptor.
    let copy = unsafe { OwnedFd::from_raw_fd(copy.into_raw()) };
    let copied_fd = Fd::new(copy.as_raw_fd());
    let option = |name: c_int| -> io::Result<c_int> {
        let mut value = [0; size_of::<c_int>()];
        sys::getsockopt_bytes(copied_fd, sys::SOL_SOCKET, name, &mut value)
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))?;
        Ok(c_int::from_ne_bytes(value))
    };
    match option(sys::SO_TYPE) {
        Ok(kind) if kind == sys::SOCK_STREAM => {}
        // alloc: an error path.
        Ok(_) => return Err(refused("not a stream socket".to_owned())),
        // alloc: an error path.
        Err(error) => return Err(refused(format!("not a socket ({error})"))),
    }
    // alloc: an error path.
    if !accepting(&copy, option).map_err(|error| refused(error.to_string()))? {
        // alloc: an error path.
        return Err(refused("not listening".to_owned()));
    }
    let (storage, len) = sys::getsockname(copied_fd).map_err(|error| {
        // alloc: an error path.
        refused(io::Error::from_raw_os_error(i32::from(error.raw())).to_string())
    })?;
    if c_int::from(storage.ss_family) != sys::AF_UNIX {
        // alloc: an error path.
        return Err(refused("not a Unix socket".to_owned()));
    }
    // SAFETY: the system wrote a `sockaddr_un` (its family is AF_UNIX), which `storage` holds.
    let un = unsafe { &*(&raw const storage).cast::<sys::sockaddr_un>() };
    let named = (len as usize)
        .saturating_sub(std::mem::offset_of!(sys::sockaddr_un, sun_path))
        .min(un.sun_path.len());
    let named = &un.sun_path[..named];
    let bound = &named[..named.iter().position(|&it| it == 0).unwrap_or(named.len())];
    if !bound.iter().map(|&it| it as u8).eq(path.as_os_str().as_bytes().iter().copied()) {
        // alloc: an error path.
        let bound: Vec<u8> = bound.iter().map(|&it| it as u8).collect();
        // alloc: an error path.
        return Err(refused(format!("bound at {:?}, not there", String::from_utf8_lossy(&bound))));
    }
    Ok(Listener::Unix { listener: UnixListener::from(copy), path: None, user: blaze_sys::posix::thread::geteuid() })
}

/// Whether the stream socket `fd` listens, by `SO_ACCEPTCONN` (`option` reads a socket option).
#[cfg(not(target_os = "macos"))]
fn accepting(
    _fd: &OwnedFd,
    option: impl Fn(c_int) -> io::Result<c_int>,
) -> io::Result<bool> {
    Ok(option(sys::SO_ACCEPTCONN)? != 0)
}

/// Whether the stream socket `fd` listens. Apple's `getsockopt` has no `SO_ACCEPTCONN`, so it is
/// read from the socket's options as the kernel describes them (`proc_pidfdinfo`'s
/// `PROC_PIDFDSOCKETINFO`, as `lsof` reads it), at the offsets of `<sys/proc_info.h>`'s
/// `socket_fdinfo`; its type and family, read from the same description, are checked against
/// what `getsockopt` says so a layout other than this one is an error, never a wrong answer.
#[cfg(target_os = "macos")]
fn accepting(
    fd: &OwnedFd,
    option: impl Fn(c_int) -> io::Result<c_int>,
) -> io::Result<bool> {
    const PROC_PIDFDSOCKETINFO: c_int = 3;
    // `socket_fdinfo` is a 24-byte `proc_fileinfo`, then `socket_info`: a 136-byte
    // `vinfo_stat`, `soi_so` and `soi_pcb` (8 bytes each), then `soi_type`, `soi_protocol`,
    // `soi_family` (`int`s) and `soi_options` (a `short`).
    const TYPE: usize = 24 + 152;
    const FAMILY: usize = 24 + 160;
    const OPTIONS: usize = 24 + 164;
    // Larger than `socket_fdinfo` (792 bytes): the call refuses a smaller buffer only.
    let mut info = [0u64; 256];
    let size = std::mem::size_of_val(&info) as c_int;
    let pid = blaze_sys::posix::thread::getpid();
    // SAFETY: `info` is valid for writes of `size` bytes, and the call writes at most that.
    let got = unsafe {
        blaze_sys::posix::thread::proc_pidfdinfo(
            pid,
            fd.as_raw_fd(),
            PROC_PIDFDSOCKETINFO,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if got <= 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = crate::pod::bytes_of(&info);
    let int =
        |at: usize| c_int::from_ne_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
    if (got as usize) < OPTIONS + 2
        || int(TYPE) != option(sys::SO_TYPE)?
        || int(FAMILY) != sys::AF_UNIX
            && int(FAMILY) != sys::AF_INET
            && int(FAMILY) != sys::AF_INET6
    {
        return Err(io::Error::other(
            "the kernel's description of the socket is not laid out as <sys/proc_info.h> says",
        ));
    }
    let options =
        core::ffi::c_short::from_ne_bytes(bytes[OPTIONS..OPTIONS + 2].try_into().expect("2 bytes"));
    Ok(c_int::from(options) & sys::SO_ACCEPTCONN != 0)
}

fn not_connectable(at: &Address) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        // alloc: an error path.
        format!(
            "{at:?}: an inherited socket is listened on, never connected to; connect to its name"
        ),
    )
}

pub(crate) enum Socket {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Socket {
    /// A connected socket the driver got as a bare descriptor (an accept of kqueue's: the
    /// ring's accepts go into its table of slots, so only that driver makes one this way).
    #[cfg(target_os = "macos")]
    pub(crate) fn from_fd(fd: OwnedFd, unix: bool) -> Socket {
        use std::os::fd::IntoRawFd as _;
        let raw = fd.into_raw_fd();
        match unix {
            // SAFETY: `raw` came from `fd.into_raw_fd()`, a connected socket of the kind `unix`
            // names, so it is owned once, here; a wrong kind would make std's stream calls fail,
            // not UB.
            true => Socket::Unix(unsafe { UnixStream::from_raw_fd(raw) }),
            // SAFETY: as above, a TCP socket.
            false => Socket::Tcp(unsafe { TcpStream::from_raw_fd(raw) }),
        }
    }

    pub(crate) fn fd(&self) -> RawFd {
        match self {
            Socket::Unix(stream) => stream.as_raw_fd(),
            Socket::Tcp(stream) => stream.as_raw_fd(),
        }
    }

    pub(crate) fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self {
            Socket::Unix(stream) => stream.set_nonblocking(on),
            Socket::Tcp(stream) => stream.set_nonblocking(on),
        }
    }

    /// At most `most` bytes, and at most [`RECV_MOST`], appended to `into`, read straight into
    /// its spare room: `into` grows to `into.len() + most.min(RECV_MOST)` at most and never
    /// shrinks. 0 once the peer hung up.
    pub(crate) fn read_into(&self, into: &mut Vec<u8>, most: usize) -> io::Result<usize> {
        let most = most.min(RECV_MOST);
        // alloc: none once `into` has room for `most` more, as a reused buffer has.
        into.reserve(most);
        let room = into.spare_capacity_mut().as_mut_ptr().cast::<u8>();
        loop {
            #[cfg(target_os = "macos")]
            // SAFETY: `room` has `most` writable bytes. recv initializes only the returned count.
            let got = unsafe { sys::c::recv(self.fd(), room.cast(), most, 0) };
            #[cfg(not(target_os = "macos"))]
            // SAFETY: the socket's open descriptor and `most` bytes of `into`'s spare room
            // (reserved above), which the call only writes.
            let got = unsafe { libc::recv(self.fd(), room.cast(), most, 0) };
            if let Ok(got) = usize::try_from(got) {
                // SAFETY: the call wrote `got` bytes, at most `most`, from the start of the
                // spare room, right after `into`'s length.
                unsafe { into.set_len(into.len() + got) };
                return Ok(got);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                // The peer reset it: as good as hung up, for a reader.
                io::ErrorKind::ConnectionReset => return Ok(0),
                _ => return Err(error),
            }
        }
    }

    /// One write, as much as the socket takes: the kqueue driver's, which writes when the
    /// socket is ready. The other drivers write all of it ([`Socket::write_all`]).
    #[cfg(target_os = "macos")]
    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Unix(stream) => (&*stream).write(bytes),
            Socket::Tcp(stream) => (&*stream).write(bytes),
        }
    }

    pub(crate) fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Socket::Unix(stream) => (&*stream).write_all(bytes),
            Socket::Tcp(stream) => (&*stream).write_all(bytes),
        }
    }

    /// Both directions shut: a read in flight on it ends with nothing.
    pub(crate) fn shutdown(&self) -> io::Result<()> {
        let shut = match self {
            Socket::Unix(stream) => stream.shutdown(std::net::Shutdown::Both),
            Socket::Tcp(stream) => stream.shutdown(std::net::Shutdown::Both),
        };
        match shut {
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            other => other,
        }
    }
}

/// A connection made to `at`, blocking until it is.
pub(crate) fn connect(root: &Path, at: &Address, options: &crate::sockopt::Tcp) -> io::Result<Socket> {
    if !matches!(at, Address::Tcp(_)) {
        crate::sockopt::unix_refused(options)?;
    }
    match at {
        Address::Unix(name) => {
            let stream = blaze_mem::scratch::with_path(|path| {
                path.push(root);
                path.push(name.as_str());
                UnixStream::connect(&*path)
            })?;
            Ok(Socket::Unix(stream))
        }
        Address::Inherited { .. } => Err(not_connectable(at)),
        Address::Tcp(address) => Ok(Socket::Tcp(crate::sockopt::connect_tcp(*address, options)?)),
    }
}

/// `at` as the system names a socket's address, for a driver that connects by itself.
pub(crate) struct SockAddr {
    pub(crate) storage: sys::sockaddr_storage,
    pub(crate) len: socklen_t,
    pub(crate) domain: c_int,
}

pub(crate) fn sockaddr(root: &Path, at: &Address) -> io::Result<SockAddr> {
    let mut storage = sys::sockaddr_storage::ZERO;
    let (len, domain) = match at {
        Address::Unix(name) => blaze_mem::scratch::with_path(|path| {
            path.push(root);
            path.push(name.as_str());
            let bytes = path.as_os_str().as_bytes();
            // SAFETY: `sockaddr_storage` is large and aligned enough for any address.
            let un = unsafe { &mut *(&raw mut storage).cast::<sys::sockaddr_un>() };
            if bytes.len() >= un.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    // alloc: an error path.
                    format!("{}: longer than a socket's path may be", path.display()),
                ));
            }
            un.sun_family = sys::AF_UNIX as sa_family_t;
            for (to, &from) in un.sun_path.iter_mut().zip(bytes) {
                *to = from as core::ffi::c_char;
            }
            let len = std::mem::offset_of!(sys::sockaddr_un, sun_path) + bytes.len() + 1;
            Ok((len, sys::AF_UNIX))
        })?,
        Address::Inherited { .. } => return Err(not_connectable(at)),
        Address::Tcp(SocketAddr::V4(address)) => {
            // SAFETY: as above.
            let in4 = unsafe { &mut *(&raw mut storage).cast::<sys::sockaddr_in>() };
            in4.sin_family = sys::AF_INET as sa_family_t;
            in4.sin_port = address.port().to_be();
            in4.sin_addr = sys::in_addr { s_addr: u32::from_ne_bytes(address.ip().octets()) };
            (size_of::<sys::sockaddr_in>(), sys::AF_INET)
        }
        Address::Tcp(SocketAddr::V6(address)) => {
            // SAFETY: as above.
            let in6 = unsafe { &mut *(&raw mut storage).cast::<sys::sockaddr_in6>() };
            in6.sin6_family = sys::AF_INET6 as sa_family_t;
            in6.sin6_port = address.port().to_be();
            in6.sin6_flowinfo = address.flowinfo();
            in6.sin6_addr = sys::in6_addr { s6_addr: address.ip().octets() };
            in6.sin6_scope_id = address.scope_id();
            (size_of::<sys::sockaddr_in6>(), sys::AF_INET6)
        }
    };
    set_len(&mut storage, len);
    Ok(SockAddr { storage, len: len as socklen_t, domain })
}

/// Every address starts with its length on Apple's systems.
#[cfg(target_os = "macos")]
fn set_len(storage: &mut sys::sockaddr_storage, len: usize) {
    storage.ss_len = len as u8;
}

#[cfg(not(target_os = "macos"))]
fn set_len(_: &mut sys::sockaddr_storage, _: usize) {}

/// A socket of `domain`, of the kind `connect` takes, not yet connected.
pub(crate) fn stream_socket(domain: c_int) -> io::Result<OwnedFd> {
    let fd = sys::socket(domain, sys::SOCK_STREAM, 0)
        .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))?;
    // SAFETY: the call returned a descriptor of its own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd.raw()) };
    set_cloexec(fd.as_raw_fd())?;
    Ok(fd)
}

pub(crate) fn set_cloexec(fd: RawFd) -> io::Result<()> {
    blaze_sys::posix::io::set_close_on_exec(Fd::new(fd), true)
        .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
}

pub(crate) mod udp;

#[cfg(test)]
mod tests;
