//! The sockets behind [`crate::Effect::Listen`] and [`crate::Effect::Connect`] on Windows:
//! Winsock's overlapped calls (`AcceptEx`, `ConnectEx`, `WSARecv`, `WSASend`), completing on
//! the port. `AF_UNIX` (Windows 10 1803 on) for [`Address::Unix`], whose socket file is under
//! the root and goes with its listener.
//!
//! A connect of a Unix socket blocks on a thread of the driver's: `ConnectEx` needs the socket
//! bound first, and Windows cannot bind an `AF_UNIX` socket to no name.

use std::{
    cell::UnsafeCell,
    io,
    net::SocketAddr,
    os::windows::io::{AsRawSocket as _, FromRawSocket as _, OwnedSocket},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use windows_sys::Win32::{
    Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNIX, AcceptEx, INVALID_SOCKET, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0, IPPROTO_TCP,
        LPFN_CONNECTEX, SD_BOTH, SIO_GET_EXTENSION_FUNCTION_POINTER, SO_UPDATE_ACCEPT_CONTEXT,
        SO_UPDATE_CONNECT_CONTEXT, SOCK_STREAM, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_IN6_0,
        SOCKADDR_STORAGE, SOCKADDR_UN, SOCKET, SOL_SOCKET, TCP_NODELAY, WSA_FLAG_NO_HANDLE_INHERIT,
        WSA_FLAG_OVERLAPPED, WSA_IO_PENDING, WSABUF, WSADATA, WSAENOTCONN, WSAGetLastError, WSAID_CONNECTEX,
        WSAIoctl, WSARecv, WSASend, WSASocketW, WSAStartup, bind, connect, getsockname, listen,
        setsockopt, shutdown,
    },
    System::IO::OVERLAPPED,
};

use crate::Address;

/// What `AcceptEx` writes of each end's address: the largest address and the 16 bytes the
/// call asks for besides.
const ADDRESS: usize = size_of::<SOCKADDR_STORAGE>() + 16;

fn last() -> io::Error {
    // SAFETY: no arguments.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// Winsock started for the process, once; never stopped, as std does.
fn started() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        // SAFETY: all zeros is a valid `WSADATA` for the call to fill.
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        // SAFETY: version 2.2 and the data to write.
        unsafe { WSAStartup(0x202, &mut data) }
    });
    match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

pub(super) fn raw(socket: &OwnedSocket) -> SOCKET {
    socket.as_raw_socket() as SOCKET
}

/// A stream socket of `family`, overlapped and kept from children.
fn stream(family: u16) -> io::Result<OwnedSocket> {
    started()?;
    let protocol = match family {
        AF_UNIX => 0,
        _ => IPPROTO_TCP,
    };
    // SAFETY: no protocol information; a socket of its own or `INVALID_SOCKET`.
    let socket = unsafe {
        WSASocketW(
            i32::from(family),
            SOCK_STREAM,
            protocol,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(last());
    }
    // SAFETY: the socket just made, owned from here.
    Ok(unsafe { OwnedSocket::from_raw_socket(socket as u64) })
}

/// An address as Winsock takes one.
pub(super) struct SockAddr {
    storage: SOCKADDR_STORAGE,
    len: i32,
    pub(super) family: u16,
}

impl SockAddr {
    fn ptr(&self) -> *const SOCKADDR {
        (&raw const self.storage).cast()
    }
}

fn sockaddr(root: &Path, at: &Address) -> io::Result<SockAddr> {
    // SAFETY: all zeros is a valid `SOCKADDR_STORAGE`.
    let mut storage: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
    let (len, family) = match at {
        Address::Unix(name) => {
            let path = std::path::absolute(root.join(name.as_str()))?;
            let text = path.to_str().ok_or_else(|| {
                // alloc: an error path.
                io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not Unicode", path.display()))
            })?;
            // SAFETY: `SOCKADDR_STORAGE` is large and aligned enough for any address.
            let un = unsafe { &mut *(&raw mut storage).cast::<SOCKADDR_UN>() };
            if text.len() >= un.sun_path.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    // alloc: an error path.
                    format!("{text}: longer than a socket's path may be"),
                ));
            }
            un.sun_family = AF_UNIX;
            for (to, &from) in un.sun_path.iter_mut().zip(text.as_bytes()) {
                *to = from as i8;
            }
            (size_of::<SOCKADDR_UN>(), AF_UNIX)
        }
        Address::Inherited { fd, at } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                // alloc: an error path.
                format!("the socket {at} passed as fd {fd}: socket activation is systemd's and launchd's, and Windows has neither"),
            ));
        }
        Address::Tcp(SocketAddr::V4(address)) => {
            // SAFETY: as above.
            let in4 = unsafe { &mut *(&raw mut storage).cast::<SOCKADDR_IN>() };
            in4.sin_family = AF_INET;
            in4.sin_port = address.port().to_be();
            in4.sin_addr = IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(address.ip().octets()) } };
            (size_of::<SOCKADDR_IN>(), AF_INET)
        }
        Address::Tcp(SocketAddr::V6(address)) => {
            // SAFETY: as above.
            let in6 = unsafe { &mut *(&raw mut storage).cast::<SOCKADDR_IN6>() };
            in6.sin6_family = AF_INET6;
            in6.sin6_port = address.port().to_be();
            in6.sin6_flowinfo = address.flowinfo();
            in6.sin6_addr = IN6_ADDR { u: IN6_ADDR_0 { Byte: address.ip().octets() } };
            in6.Anonymous = SOCKADDR_IN6_0 { sin6_scope_id: address.scope_id() };
            (size_of::<SOCKADDR_IN6>(), AF_INET6)
        }
    };
    Ok(SockAddr { storage, len: len as i32, family })
}

/// A socket listening, and the file of a Unix one's, removed with it.
pub(super) struct Listener {
    pub(super) socket: OwnedSocket,
    pub(super) family: u16,
    path: Option<PathBuf>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            _ = std::fs::remove_file(path);
        }
    }
}

impl Listener {
    /// The address a TCP listen was given (its port, where it asked for port 0).
    pub(super) fn local_addr(&self) -> Option<SocketAddr> {
        if self.family == AF_UNIX {
            return None;
        }
        // SAFETY: all zeros is a valid `SOCKADDR_STORAGE`.
        let mut storage: SOCKADDR_STORAGE = unsafe { std::mem::zeroed() };
        let mut len = size_of::<SOCKADDR_STORAGE>() as i32;
        // SAFETY: an open socket and room of the length given.
        if unsafe { getsockname(raw(&self.socket), (&raw mut storage).cast(), &mut len) } != 0 {
            return None;
        }
        match storage.ss_family {
            AF_INET => {
                // SAFETY: an IPv4 address is what the call wrote.
                let in4 = unsafe { &*(&raw const storage).cast::<SOCKADDR_IN>() };
                // SAFETY: every field of the union is the address's four bytes.
                let ip = unsafe { in4.sin_addr.S_un.S_addr }.to_ne_bytes();
                Some(SocketAddr::from((ip, u16::from_be(in4.sin_port))))
            }
            AF_INET6 => {
                // SAFETY: an IPv6 address is what the call wrote.
                let in6 = unsafe { &*(&raw const storage).cast::<SOCKADDR_IN6>() };
                // SAFETY: as above, the sixteen bytes.
                let ip = unsafe { in6.sin6_addr.u.Byte };
                Some(SocketAddr::from((ip, u16::from_be(in6.sin6_port))))
            }
            _ => None,
        }
    }
}

/// A socket listening at `at`. A Unix socket's name left by a daemon that is gone is
/// replaced; one another process listens on is refused. Who may connect to it is who the
/// directory's access list lets write there.
pub(super) fn listen(root: &Path, at: &Address) -> io::Result<Listener> {
    let address = sockaddr(root, at)?;
    let path = match at {
        Address::Unix(name) => {
            let path = root.join(name.as_str());
            crate::fs::make_parent(&path)?;
            if std::fs::symlink_metadata(&path).is_ok() {
                if connect_blocking(root, at).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        // alloc: an error path.
                        format!("{}: something listens on it already", path.display()),
                    ));
                }
                std::fs::remove_file(&path)?;
            }
            Some(path)
        }
        Address::Tcp(_) => None,
        Address::Inherited { .. } => None,
    };
    let socket = stream(address.family)?;
    // SAFETY: an open socket and an address of the length given.
    if unsafe { bind(raw(&socket), address.ptr(), address.len) } != 0 {
        return Err(last());
    }
    let listener = Listener { socket, family: address.family, path };
    // The most Winsock takes (`SOMAXCONN` of winsock2.h).
    // SAFETY: a bound socket.
    if unsafe { listen(raw(&listener.socket), i32::MAX) } != 0 {
        return Err(last());
    }
    Ok(listener)
}

/// An accept in flight: the socket it makes the connection on, and where it writes both
/// ends' addresses, inline in the request, whose entry stays where it is until its completion
/// (the kernel writes them meanwhile: `UnsafeCell`).
pub(super) struct Accepting {
    pub(super) listener: Listener,
    accepted: Option<OwnedSocket>,
    addresses: UnsafeCell<[u8; 2 * ADDRESS]>,
    pub(super) stopping: bool,
}

impl Accepting {
    pub(super) fn new(listener: Listener) -> Accepting {
        Accepting { listener, accepted: None, addresses: UnsafeCell::new([0; 2 * ADDRESS]), stopping: false }
    }

    /// Asks for the next connection, completing through `overlapped`.
    ///
    /// # Safety
    ///
    /// `overlapped` is a request's [`super::sys::Head::overlapped`], made ready for this step,
    /// and the request's entry (its head and this `Accepting` with its address buffer and accepted socket) stays where it is, alive, until the
    /// step's completion is dequeued: the kernel writes them after this returns, so freeing or
    /// moving either first is a write to freed memory.
    pub(super) unsafe fn accept(&mut self, overlapped: *mut OVERLAPPED) -> io::Result<()> {
        let accepted = stream(self.listener.family)?;
        let mut got = 0;
        // SAFETY: two open sockets, the address buffer of the lengths given, and an
        // `OVERLAPPED`, all in the request's entry, which is kept until the completion is
        // taken (`Iocp::requests`).
        let ok = unsafe {
            AcceptEx(
                raw(&self.listener.socket),
                raw(&accepted),
                self.addresses.get().cast(),
                0,
                ADDRESS as u32,
                ADDRESS as u32,
                &mut got,
                overlapped,
            )
        };
        self.accepted = Some(accepted);
        pending(ok != 0)
    }

    /// The connection the completed accept made.
    pub(super) fn take(&mut self) -> io::Result<OwnedSocket> {
        let accepted = self.accepted.take().expect("an accept completed");
        let listener = raw(&self.listener.socket);
        // So the socket is the listener's in every call that asks (shutdown, its names).
        set(&accepted, SOL_SOCKET, SO_UPDATE_ACCEPT_CONTEXT, &listener)?;
        if self.listener.family != AF_UNIX {
            nodelay(&accepted)?;
        }
        Ok(accepted)
    }
}

/// Ok where an overlapped call is in flight: it returned true, or false with
/// `WSA_IO_PENDING`. Either way its completion comes to the port.
fn pending(ok: bool) -> io::Result<()> {
    if ok {
        return Ok(());
    }
    let error = last();
    match error.raw_os_error() {
        Some(WSA_IO_PENDING) => Ok(()),
        _ => Err(error),
    }
}

fn set<T>(socket: &OwnedSocket, level: i32, name: i32, value: &T) -> io::Result<()> {
    // SAFETY: an open socket and a value of the size given.
    let set = unsafe {
        setsockopt(raw(socket), level, name, (value as *const T).cast(), size_of::<T>() as i32)
    };
    match set {
        0 => Ok(()),
        _ => Err(last()),
    }
}

fn nodelay(socket: &OwnedSocket) -> io::Result<()> {
    set(socket, IPPROTO_TCP, TCP_NODELAY, &1i32)
}

/// A connect: the address, inline in the request, whose entry stays where it is until the
/// connect's completion, and the socket, once it is asked for.
pub(super) struct Connecting {
    socket: Option<OwnedSocket>,
    address: SockAddr,
}

impl Connecting {
    /// A TCP connect to `at`, to be asked for once the request holding it is in place.
    pub(super) fn new(root: &Path, at: &Address) -> io::Result<Connecting> {
        Ok(Connecting { socket: None, address: sockaddr(root, at)? })
    }

    /// The socket the connect is on, once asked for: what a cancel names.
    pub(super) fn raw(&self) -> SOCKET {
        self.socket.as_ref().map_or(INVALID_SOCKET, raw)
    }

    /// Asks for the connect, through `overlapped`.
    ///
    /// # Safety
    ///
    /// `overlapped` is a request's [`super::sys::Head::overlapped`], made ready for this step,
    /// and the request's entry (its head and this `Connecting` with its address and socket) stays where it is, alive, until the
    /// step's completion is dequeued: the kernel writes them after this returns, so freeing or
    /// moving either first is a write to freed memory.
    pub(super) unsafe fn start(
        &mut self,
        associate: impl FnOnce(&OwnedSocket) -> io::Result<()>,
        overlapped: *mut OVERLAPPED,
    ) -> io::Result<()> {
        let family = self.address.family;
        let socket = stream(family)?;
        // `ConnectEx` takes only a bound socket: bound to any address of its family.
        let any = match family {
            AF_INET6 => SocketAddr::from(([0u16; 8], 0)),
            _ => SocketAddr::from(([0u8; 4], 0)),
        };
        let any = sockaddr(Path::new(""), &Address::Tcp(any))?;
        // SAFETY: an open socket and an address of the length given.
        if unsafe { bind(raw(&socket), any.ptr(), any.len) } != 0 {
            return Err(last());
        }
        associate(&socket)?;
        let connect_ex = connect_ex(&socket)?;
        // SAFETY: a bound socket, an address and an `OVERLAPPED` in the request's entry, which
        // is kept until the completion is taken (`Iocp::requests`), and no data to send.
        let ok = unsafe {
            connect_ex(
                raw(&socket),
                self.address.ptr(),
                self.address.len,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                overlapped,
            )
        };
        self.socket = Some(socket);
        pending(ok != 0)
    }

    /// The connection, once the connect has completed.
    pub(super) fn take(self) -> io::Result<OwnedSocket> {
        let socket = self.socket.expect("a connect completed was asked for");
        // So the socket is connected in every call that asks (shutdown, its names).
        // SAFETY: a connected socket; the option takes no value.
        if unsafe { setsockopt(raw(&socket), SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, std::ptr::null(), 0) } != 0 {
            return Err(last());
        }
        nodelay(&socket)?;
        Ok(socket)
    }
}

type ConnectEx = unsafe extern "system" fn(
    SOCKET,
    *const SOCKADDR,
    i32,
    *const core::ffi::c_void,
    u32,
    *mut u32,
    *mut OVERLAPPED,
) -> windows_sys::core::BOOL;

/// `ConnectEx`, which Winsock hands out per socket.
fn connect_ex(socket: &OwnedSocket) -> io::Result<ConnectEx> {
    let mut function: LPFN_CONNECTEX = None;
    let mut got = 0;
    // SAFETY: an open socket, the GUID to read, and room for the pointer; synchronous.
    let asked = unsafe {
        WSAIoctl(
            raw(socket),
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&raw const WSAID_CONNECTEX).cast(),
            size_of_val(&WSAID_CONNECTEX) as u32,
            (&raw mut function).cast(),
            size_of::<LPFN_CONNECTEX>() as u32,
            &mut got,
            std::ptr::null_mut(),
            None,
        )
    };
    if asked != 0 {
        return Err(last());
    }
    function.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "Winsock has no ConnectEx"))
}

/// A connection made to `at`, blocking until it is: a Unix socket's, on a thread of the
/// driver's (see the module's comment), and the check of a listen whether its name is taken.
pub(super) fn connect_blocking(root: &Path, at: &Address) -> io::Result<OwnedSocket> {
    let address = sockaddr(root, at)?;
    let socket = stream(address.family)?;
    // SAFETY: an open socket and an address of the length given.
    if unsafe { connect(raw(&socket), address.ptr(), address.len) } != 0 {
        return Err(last());
    }
    if address.family != AF_UNIX {
        nodelay(&socket)?;
    }
    Ok(socket)
}

/// A receive into the `len` bytes at `into` (which need not be initialized: the kernel only
/// writes them), completing through `overlapped`; they, and `flags` (which Winsock may write as
/// it completes), are the request's, kept until its completion is taken.
///
/// # Safety
///
/// `into` is `len` writable bytes and `flags` a writable `u32`, both the request's, and
/// `overlapped` its head's, made ready for this step; all three stay where they are, alive and
/// unread by anything else, until the completion is dequeued, as Winsock writes them after
/// this returns. `socket` is open.
pub(super) unsafe fn recv(
    socket: SOCKET,
    into: *mut u8,
    len: usize,
    flags: *mut u32,
    overlapped: *mut OVERLAPPED,
) -> io::Result<()> {
    let buffer = WSABUF { len: len.min(u32::MAX as usize) as u32, buf: into };
    // SAFETY: the request's flags, which nothing reads or writes while no receive is in flight.
    unsafe { flags.write(0) };
    // SAFETY: an open socket; Winsock takes the `WSABUF` before it returns, and the bytes it
    // names, the flags and the `OVERLAPPED` are the request's, kept until its completion.
    let ok = unsafe { WSARecv(socket, &buffer, 1, std::ptr::null_mut(), flags, overlapped, None) };
    pending(ok == 0)
}

/// A send of `bytes`, completing through `overlapped`; both outlive the request.
///
/// # Safety
///
/// `bytes` and `overlapped` (the request's head, made ready for this step) stay where they
/// are, alive and unwritten, until the completion is dequeued: Winsock reads the bytes after
/// this returns, past the borrow. `socket` is open.
pub(super) unsafe fn send(socket: SOCKET, bytes: &[u8], overlapped: *mut OVERLAPPED) -> io::Result<()> {
    let buffer = WSABUF { len: bytes.len().min(u32::MAX as usize) as u32, buf: bytes.as_ptr().cast_mut() };
    // SAFETY: as in `recv`; the bytes are only read.
    let ok = unsafe { WSASend(socket, &buffer, 1, std::ptr::null_mut(), 0, overlapped, None) };
    pending(ok == 0)
}

/// Both directions shut: a receive in flight on it ends with nothing.
pub(super) fn hang_up(socket: &OwnedSocket) -> io::Result<()> {
    // SAFETY: an open socket.
    if unsafe { shutdown(raw(socket), SD_BOTH) } == 0 {
        return Ok(());
    }
    let error = last();
    match error.raw_os_error() {
        Some(WSAENOTCONN) => Ok(()),
        _ => Err(error),
    }
}
