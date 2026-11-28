//! Zero-`Effect`/`Done`-bridge direct stream networking backend seam ([`DirectNetBackend`]),
//! deterministic in-memory loopback backend ([`InMemoryDirectNet`]), real OS TCP loopback backend
//! ([`NativeDirectNet`]), and generational-handle handler ([`DirectNetHandler<'io, 'b, B>`])
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Mirrors `request/direct.rs` for stream networking:
//! - Zero `Effect`/`Done` enum translation on both [`InMemoryDirectNet`] and [`NativeDirectNet`].
//! - Generational driver-branded [`HandleTable`]s for [`ConnKind`] and [`ListenerKind`] with
//!   loud normal-exit leak detection (`finish_checked`) and backend socket reclamation.
//! - Small `#[inline(never)]` `exec_*` methods so monomorphization across caller closures never
//!   inflates generated code (rule 16).

use core::fmt;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};

use blaze_machine::{Handler, Perform};
use blaze_own::{Alloc, Brand, Vec};

use crate::Address;
use crate::handle::{
    Conn as ConnHandle, ConnKind, ExitKind, HandleError, HandleTable, HandleTableOpts,
    LeakedHandles, Listener, ListenerKind, TaskId,
};
use crate::request::net::{
    Accept, CloseConn, Connect, Listen, NetError, RecvInto, SendFrom, Unlisten,
};
use crate::request::{BufError, ByteLen, DmaBuf, FilledBuf};

/// Upper bound on `EINTR` retries for a single OS socket call (rule 18 `bounded-progress`).
const MAX_EINTR_RETRIES: u32 = 8;

/// Default bounded slot capacities for [`NativeDirectNet`] (rule 18 `bounded-progress`).
const DEFAULT_MAX_LISTENERS: usize = 64;
const DEFAULT_MAX_CONNS: usize = 256;

/// Direct stream networking execution seam consumed by [`DirectNetHandler`] with zero
/// `Effect`/`Done` enum translation.
pub trait DirectNetBackend {
    /// Binds a listener at `addr` (supporting ephemeral ports such as `127.0.0.1:0`), returning
    /// `(listener_token, bound_addr)`.
    fn listen(&mut self, addr: &Address) -> Result<(u64, Address), NetError>;

    /// Accepts the next queued connection on `listener`, returning `(conn_token, peer_addr)`.
    fn accept(&mut self, listener: u64) -> Result<(u64, Address), NetError>;

    /// Connects a stream socket to `addr`, returning its connection token.
    fn connect(&mut self, addr: &Address) -> Result<u64, NetError>;

    /// Writes up to `src.len()` bytes onto `conn`, returning the number of bytes accepted.
    fn send(&mut self, conn: u64, src: &[u8]) -> Result<usize, NetError>;

    /// Reads up to `dst.len()` bytes from `conn`, returning `0` on clean peer EOF.
    fn recv(&mut self, conn: u64, dst: &mut [u8]) -> Result<usize, NetError>;

    /// Closes `conn` and releases its backend resources.
    fn close_conn(&mut self, conn: u64) -> Result<(), NetError>;

    /// Closes `listener` and releases its backend resources.
    fn unlisten(&mut self, listener: u64) -> Result<(), NetError>;
}

// ---------------------------------------------------------------------------
// 1. Deterministic in-memory loopback backend (`InMemoryDirectNet<'a>`)
// ---------------------------------------------------------------------------

struct MemListener<'a> {
    token: u64,
    addr: Address,
    pending: Vec<(u64, Address), Alloc<'a>>,
}

struct MemConn<'a> {
    token: u64,
    peer: Option<u64>,
    inbox: Vec<u8, Alloc<'a>>,
    read_pos: usize,
}

/// Deterministic in-memory loopback [`DirectNetBackend`] backed by an explicit caller
/// [`Alloc<'a>`].
///
/// Supports ephemeral port binding (`127.0.0.1:0` / `[::1]:0`), FIFO accept/connect pairing,
/// configurable short-write and short-read chunk caps for testing partial TCP framing, and clean
/// peer-close EOF semantics.
pub struct InMemoryDirectNet<'a> {
    alloc: Alloc<'a>,
    listeners: Vec<MemListener<'a>, Alloc<'a>>,
    conns: Vec<MemConn<'a>, Alloc<'a>>,
    next_token: u64,
    next_ephemeral_port: u16,
    max_write_chunk: Option<usize>,
    max_read_chunk: Option<usize>,
}

impl<'a> InMemoryDirectNet<'a> {
    /// Creates an empty deterministic in-memory loopback network in `alloc`.
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            alloc,
            listeners: Vec::new_in(alloc),
            conns: Vec::new_in(alloc),
            next_token: 1,
            next_ephemeral_port: 49_152,
            max_write_chunk: None,
            max_read_chunk: None,
        }
    }

    /// Caps each [`DirectNetBackend::send`] call to at most `max_bytes.max(1)` bytes to exercise
    /// partial-write loops and TCP framing.
    #[inline]
    pub fn with_max_write_chunk(mut self, max_bytes: usize) -> Self {
        self.max_write_chunk = Some(max_bytes.max(1));
        self
    }

    /// Caps each [`DirectNetBackend::recv`] call to at most `max_bytes.max(1)` bytes to exercise
    /// partial-read loops and TCP framing.
    #[inline]
    pub fn with_max_read_chunk(mut self, max_bytes: usize) -> Self {
        self.max_read_chunk = Some(max_bytes.max(1));
        self
    }

    /// Returns the bound address of a live listener token, if present.
    #[inline]
    pub fn local_addr(&self, listener: u64) -> Option<&Address> {
        self.listeners.as_slice().iter().find(|l| l.token == listener).map(|l| &l.addr)
    }

    /// Number of currently open connections.
    #[inline]
    pub fn open_conn_count(&self) -> usize {
        self.conns.len()
    }

    /// Number of currently open listeners.
    #[inline]
    pub fn open_listener_count(&self) -> usize {
        self.listeners.len()
    }

    fn alloc_token(&mut self) -> u64 {
        let tok = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        tok
    }

    fn alloc_ephemeral_port(&mut self, ip: std::net::IpAddr) -> Result<u16, NetError> {
        const START: u16 = 49_152;
        const SPAN: u32 = (u16::MAX as u32) - (START as u32) + 1;
        for _ in 0..SPAN {
            let port = self.next_ephemeral_port;
            self.next_ephemeral_port = if port == u16::MAX { START } else { port + 1 };
            let candidate = Address::Tcp(SocketAddr::new(ip, port));
            if !self.listeners.as_slice().iter().any(|l| l.addr == candidate) {
                return Ok(port);
            }
        }
        Err(NetError::Io {
            addr: Some(Address::Tcp(SocketAddr::new(ip, 0))),
            message: "no ephemeral port available in in-memory network".to_owned(),
        })
    }

    fn find_listener_idx(&self, token: u64) -> Option<usize> {
        self.listeners.as_slice().iter().position(|l| l.token == token)
    }

    fn find_conn_idx(&self, token: u64) -> Option<usize> {
        self.conns.as_slice().iter().position(|c| c.token == token)
    }

    fn close_conn_internal(&mut self, token: u64) -> bool {
        let Some(idx) = self.find_conn_idx(token) else {
            return false;
        };
        let removed = self.conns.remove_swap(idx);
        if let Some(peer_tok) = removed.peer
            && let Some(peer_idx) = self.find_conn_idx(peer_tok)
        {
            self.conns[peer_idx].peer = None;
        }
        true
    }
}

impl<'a> DirectNetBackend for InMemoryDirectNet<'a> {
    fn listen(&mut self, addr: &Address) -> Result<(u64, Address), NetError> {
        let bound = match addr {
            Address::Tcp(sock) => {
                if sock.port() == 0 {
                    let port = self.alloc_ephemeral_port(sock.ip())?;
                    Address::Tcp(SocketAddr::new(sock.ip(), port))
                } else {
                    if self.listeners.as_slice().iter().any(|l| &l.addr == addr) {
                        return Err(NetError::AddressInUse { addr: addr.clone() });
                    }
                    Address::Tcp(*sock)
                }
            }
            Address::Unix(_) => {
                if self.listeners.as_slice().iter().any(|l| &l.addr == addr) {
                    return Err(NetError::AddressInUse { addr: addr.clone() });
                }
                addr.clone()
            }
            Address::Inherited { .. } => {
                return Err(NetError::Io {
                    addr: Some(addr.clone()),
                    message: "inherited sockets are not supported by InMemoryDirectNet".to_owned(),
                });
            }
        };
        let token = self.alloc_token();
        self.listeners.push(MemListener {
            token,
            addr: bound.clone(),
            pending: Vec::new_in(self.alloc),
        });
        Ok((token, bound))
    }

    fn accept(&mut self, listener: u64) -> Result<(u64, Address), NetError> {
        let Some(idx) = self.find_listener_idx(listener) else {
            return Err(NetError::Closed);
        };
        if self.listeners[idx].pending.is_empty() {
            let addr = self.listeners[idx].addr.clone();
            return Err(NetError::Io {
                addr: Some(addr),
                message: "no pending connection queued on in-memory listener".to_owned(),
            });
        }
        let (server_tok, peer_addr) = self.listeners[idx].pending.remove_shift(0);
        Ok((server_tok, peer_addr))
    }

    fn connect(&mut self, addr: &Address) -> Result<u64, NetError> {
        let Some(listener_idx) = self.listeners.as_slice().iter().position(|l| &l.addr == addr)
        else {
            return Err(NetError::ConnectionRefused { addr: addr.clone() });
        };
        let peer_addr = match addr {
            Address::Tcp(sock) => {
                let port = self.alloc_ephemeral_port(sock.ip())?;
                Address::Tcp(SocketAddr::new(sock.ip(), port))
            }
            Address::Unix(_) | Address::Inherited { .. } => addr.clone(),
        };
        let client_tok = self.alloc_token();
        let server_tok = self.alloc_token();
        self.conns.push(MemConn {
            token: client_tok,
            peer: Some(server_tok),
            inbox: Vec::new_in(self.alloc),
            read_pos: 0,
        });
        self.conns.push(MemConn {
            token: server_tok,
            peer: Some(client_tok),
            inbox: Vec::new_in(self.alloc),
            read_pos: 0,
        });
        self.listeners[listener_idx].pending.push((server_tok, peer_addr));
        Ok(client_tok)
    }

    fn send(&mut self, conn: u64, src: &[u8]) -> Result<usize, NetError> {
        let Some(idx) = self.find_conn_idx(conn) else {
            return Err(NetError::Closed);
        };
        let Some(peer_tok) = self.conns[idx].peer else {
            return Err(NetError::BrokenPipe {
                message: "peer closed in-memory connection".to_owned(),
            });
        };
        let Some(peer_idx) = self.find_conn_idx(peer_tok) else {
            return Err(NetError::BrokenPipe {
                message: "peer closed in-memory connection".to_owned(),
            });
        };
        if src.is_empty() {
            return Ok(0);
        }
        let mut n = src.len();
        if let Some(cap) = self.max_write_chunk {
            n = n.min(cap);
        }
        self.conns[peer_idx].inbox.extend_from_slice(&src[..n]);
        Ok(n)
    }

    fn recv(&mut self, conn: u64, dst: &mut [u8]) -> Result<usize, NetError> {
        let Some(idx) = self.find_conn_idx(conn) else {
            return Err(NetError::Closed);
        };
        let avail = self.conns[idx].inbox.len().saturating_sub(self.conns[idx].read_pos);
        if avail == 0 {
            if self.conns[idx].peer.is_none() {
                return Ok(0);
            }
            return Err(NetError::Io {
                addr: None,
                message: "in-memory connection has no buffered bytes ready to read".to_owned(),
            });
        }
        if dst.is_empty() {
            return Ok(0);
        }
        let mut n = avail.min(dst.len());
        if let Some(cap) = self.max_read_chunk {
            n = n.min(cap);
        }
        let entry = &mut self.conns[idx];
        let start = entry.read_pos;
        dst[..n].copy_from_slice(&entry.inbox.as_slice()[start..start + n]);
        entry.read_pos += n;
        if entry.read_pos == entry.inbox.len() {
            entry.inbox.clear();
            entry.read_pos = 0;
        }
        Ok(n)
    }

    fn close_conn(&mut self, conn: u64) -> Result<(), NetError> {
        if self.close_conn_internal(conn) { Ok(()) } else { Err(NetError::Closed) }
    }

    fn unlisten(&mut self, listener: u64) -> Result<(), NetError> {
        let Some(idx) = self.find_listener_idx(listener) else {
            return Err(NetError::Closed);
        };
        let removed = self.listeners.remove_swap(idx);
        for &(server_tok, _) in removed.pending.as_slice() {
            _ = self.close_conn_internal(server_tok);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 2. Real OS TCP loopback backend (`NativeDirectNet`) — zero `Effect`/`Done`
// ---------------------------------------------------------------------------

/// Zero-`Effect`/`Done`-bridge [`DirectNetBackend`] backed directly by OS
/// `std::net::{TcpListener, TcpStream}` sockets inside bounded slot tables.
#[derive(Debug)]
pub struct NativeDirectNet {
    listeners: std::vec::Vec<Option<(u64, TcpListener)>>,
    conns: std::vec::Vec<Option<(u64, TcpStream)>>,
    max_listeners: usize,
    max_conns: usize,
    next_token: u64,
}

impl Default for NativeDirectNet {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeDirectNet {
    /// Creates a native OS TCP backend with default bounded slot limits.
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_LISTENERS, DEFAULT_MAX_CONNS)
    }

    /// Creates a native OS TCP backend with explicit listener and connection table caps.
    pub fn with_limits(max_listeners: usize, max_conns: usize) -> Self {
        Self {
            listeners: std::vec::Vec::new(),
            conns: std::vec::Vec::new(),
            max_listeners,
            max_conns,
            next_token: 1,
        }
    }

    /// Number of currently open OS connections in this backend.
    #[inline]
    pub fn open_conn_count(&self) -> usize {
        self.conns.iter().filter(|slot| slot.is_some()).count()
    }

    /// Number of currently open OS listeners in this backend.
    #[inline]
    pub fn open_listener_count(&self) -> usize {
        self.listeners.iter().filter(|slot| slot.is_some()).count()
    }

    fn alloc_token(&mut self) -> u64 {
        let tok = self.next_token;
        self.next_token = self.next_token.wrapping_add(1);
        tok
    }

    fn require_tcp(addr: &Address) -> Result<SocketAddr, NetError> {
        match addr {
            Address::Tcp(sock) => Ok(*sock),
            Address::Unix(_) | Address::Inherited { .. } => Err(NetError::Io {
                addr: Some(addr.clone()),
                // alloc: an error path.
                message: format!("NativeDirectNet supports TCP addresses only, got {addr}"),
            }),
        }
    }

    fn map_io_err(addr: Option<Address>, err: std::io::Error) -> NetError {
        match err.kind() {
            std::io::ErrorKind::ConnectionRefused => match addr {
                Some(addr) => NetError::ConnectionRefused { addr },
                None => NetError::Io {
                    addr: None,
                    // alloc: an error path.
                    message: format!("{err}"),
                },
            },
            std::io::ErrorKind::AddrInUse => match addr {
                Some(addr) => NetError::AddressInUse { addr },
                None => NetError::Io {
                    addr: None,
                    // alloc: an error path.
                    message: format!("{err}"),
                },
            },
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted => {
                // alloc: an error path.
                NetError::BrokenPipe { message: format!("{err}") }
            }
            _ => NetError::Io {
                addr,
                // alloc: an error path.
                message: format!("{err}"),
            },
        }
    }

    fn insert_listener(&mut self, listener: TcpListener) -> Result<u64, NetError> {
        let tok = self.alloc_token();
        if let Some(slot) = self.listeners.iter_mut().find(|s| s.is_none()) {
            *slot = Some((tok, listener));
            return Ok(tok);
        }
        if self.listeners.len() >= self.max_listeners {
            return Err(NetError::Io {
                addr: None,
                // alloc: an error path.
                message: format!("native listener table full at cap {}", self.max_listeners),
            });
        }
        self.listeners.push(Some((tok, listener)));
        Ok(tok)
    }

    fn insert_conn(&mut self, stream: TcpStream) -> Result<u64, NetError> {
        let tok = self.alloc_token();
        if let Some(slot) = self.conns.iter_mut().find(|s| s.is_none()) {
            *slot = Some((tok, stream));
            return Ok(tok);
        }
        if self.conns.len() >= self.max_conns {
            return Err(NetError::Io {
                addr: None,
                // alloc: an error path.
                message: format!("native connection table full at cap {}", self.max_conns),
            });
        }
        self.conns.push(Some((tok, stream)));
        Ok(tok)
    }

    fn listener_ref(&self, token: u64) -> Result<&TcpListener, NetError> {
        self.listeners
            .iter()
            .find_map(|slot| match slot {
                Some((tok, listener)) if *tok == token => Some(listener),
                _ => None,
            })
            .ok_or(NetError::Closed)
    }

    fn conn_mut(&mut self, token: u64) -> Result<&mut TcpStream, NetError> {
        self.conns
            .iter_mut()
            .find_map(|slot| match slot {
                Some((tok, stream)) if *tok == token => Some(stream),
                _ => None,
            })
            .ok_or(NetError::Closed)
    }
}

impl DirectNetBackend for NativeDirectNet {
    #[inline(never)]
    fn listen(&mut self, addr: &Address) -> Result<(u64, Address), NetError> {
        let sock = Self::require_tcp(addr)?;
        let listener =
            TcpListener::bind(sock).map_err(|e| Self::map_io_err(Some(addr.clone()), e))?;
        let bound = listener
            .local_addr()
            .map(Address::Tcp)
            .map_err(|e| Self::map_io_err(Some(addr.clone()), e))?;
        let token = self.insert_listener(listener)?;
        Ok((token, bound))
    }

    #[inline(never)]
    fn accept(&mut self, listener: u64) -> Result<(u64, Address), NetError> {
        let mut last_err = None;
        for _ in 0..MAX_EINTR_RETRIES {
            let sock = self.listener_ref(listener)?;
            match sock.accept() {
                Ok((stream, peer)) => {
                    _ = stream.set_nodelay(true);
                    let tok = self.insert_conn(stream)?;
                    return Ok((tok, Address::Tcp(peer)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    last_err = Some(e);
                }
                Err(e) => return Err(Self::map_io_err(None, e)),
            }
        }
        Err(Self::map_io_err(
            None,
            last_err.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::Interrupted)),
        ))
    }

    #[inline(never)]
    fn connect(&mut self, addr: &Address) -> Result<u64, NetError> {
        let sock = Self::require_tcp(addr)?;
        let stream =
            TcpStream::connect(sock).map_err(|e| Self::map_io_err(Some(addr.clone()), e))?;
        _ = stream.set_nodelay(true);
        self.insert_conn(stream)
    }

    #[inline(never)]
    fn send(&mut self, conn: u64, src: &[u8]) -> Result<usize, NetError> {
        let stream = self.conn_mut(conn)?;
        let mut last_err = None;
        for _ in 0..MAX_EINTR_RETRIES {
            match stream.write(src) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    last_err = Some(e);
                }
                Err(e) => return Err(Self::map_io_err(None, e)),
            }
        }
        Err(Self::map_io_err(
            None,
            last_err.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::Interrupted)),
        ))
    }

    #[inline(never)]
    fn recv(&mut self, conn: u64, dst: &mut [u8]) -> Result<usize, NetError> {
        let stream = self.conn_mut(conn)?;
        let mut last_err = None;
        for _ in 0..MAX_EINTR_RETRIES {
            match stream.read(dst) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    last_err = Some(e);
                }
                Err(e) => return Err(Self::map_io_err(None, e)),
            }
        }
        Err(Self::map_io_err(
            None,
            last_err.unwrap_or_else(|| std::io::Error::from(std::io::ErrorKind::Interrupted)),
        ))
    }

    #[inline(never)]
    fn close_conn(&mut self, conn: u64) -> Result<(), NetError> {
        let slot = self
            .conns
            .iter_mut()
            .find(|s| matches!(s, Some((tok, _)) if *tok == conn))
            .ok_or(NetError::Closed)?;
        *slot = None;
        Ok(())
    }

    #[inline(never)]
    fn unlisten(&mut self, listener: u64) -> Result<(), NetError> {
        let slot = self
            .listeners
            .iter_mut()
            .find(|s| matches!(s, Some((tok, _)) if *tok == listener))
            .ok_or(NetError::Closed)?;
        *slot = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 3. Direct zero-`Effect`/`Done` handler (`DirectNetHandler<'io, 'b, B>`)
// ---------------------------------------------------------------------------

/// Handler-level error for [`DirectNetHandler`] (handle leak on normal exit or table capacity hit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirectNetHandlerError {
    LeakedHandles(LeakedHandles),
    TableFull(HandleError),
}

impl fmt::Display for DirectNetHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DirectNetHandlerError::LeakedHandles(l) => write!(f, "{l}"),
            DirectNetHandlerError::TableFull(e) => write!(f, "{e}"),
        }
    }
}

/// Zero-`Effect`/`Done`-bridge `blaze_machine::Handler` driving all 7 typed stream network
/// requests (`Listen`, `Accept`, `Connect`, `SendFrom`, `RecvInto`, `CloseConn`, `Unlisten`)
/// directly over any [`DirectNetBackend`] with branded generational handle tables and loud leak
/// detection.
pub struct DirectNetHandler<'io, 'b, B: DirectNetBackend> {
    backend: &'b mut B,
    task: TaskId,
    alloc: Alloc<'io>,
    conns: HandleTable<'io, ConnKind, u64>,
    listeners: HandleTable<'io, ListenerKind, u64>,
    listener_addrs: Vec<(u64, Address), Alloc<'io>>,
}

impl<'io, 'b, B: DirectNetBackend> DirectNetHandler<'io, 'b, B> {
    /// Creates a `DirectNetHandler` bound to `backend`, branded with `brand`, and backed by
    /// `alloc`.
    pub fn new(
        backend: &'b mut B,
        task: TaskId,
        brand: Brand<'io>,
        alloc: Alloc<'io>,
    ) -> Self {
        let conns = HandleTable::new(HandleTableOpts { brand, max_slots: 256, alloc });
        let listeners = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        Self {
            backend,
            task,
            alloc,
            conns,
            listeners,
            listener_addrs: Vec::new_in(alloc),
        }
    }

    #[inline]
    pub fn backend(&self) -> &B {
        self.backend
    }

    #[inline]
    pub fn backend_mut(&mut self) -> &mut B {
        self.backend
    }

    /// Returns the bound [`Address`] of a live [`Listener`] handle (useful after binding an
    /// ephemeral port such as `127.0.0.1:0`).
    pub fn listener_addr(&self, listener: &Listener<'io>) -> Result<&Address, NetError> {
        let &token = self.listeners.get(listener.id()).map_err(NetError::StaleHandle)?;
        self.listener_addrs
            .as_slice()
            .iter()
            .find(|(tok, _)| *tok == token)
            .map(|(_, addr)| addr)
            .ok_or(NetError::Closed)
    }

    /// Reclaims any open connection and listener handles for this task and closes their backend
    /// sockets; fails loudly with [`DirectNetHandlerError::LeakedHandles`] if any handle remained
    /// open on [`ExitKind::Normal`].
    pub fn finish_task(&mut self, exit: ExitKind) -> Result<u32, DirectNetHandlerError> {
        let mut leaked_conns: Vec<u64, Alloc<'io>> = Vec::new_in(self.alloc);
        let mut leaked_listeners: Vec<u64, Alloc<'io>> = Vec::new_in(self.alloc);

        let conn_count = self
            .conns
            .reclaim_task(self.task, ExitKind::Cancelled, |tok| leaked_conns.push(tok))
            .expect("cancelled reclaim is infallible");
        let listener_count = self
            .listeners
            .reclaim_task(self.task, ExitKind::Cancelled, |tok| leaked_listeners.push(tok))
            .expect("cancelled reclaim is infallible");

        for &tok in leaked_conns.as_slice() {
            _ = self.backend.close_conn(tok);
        }
        for &tok in leaked_listeners.as_slice() {
            self.remove_listener_addr(tok);
            _ = self.backend.unlisten(tok);
        }

        let total = conn_count + listener_count;
        match exit {
            ExitKind::Cancelled => Ok(total),
            ExitKind::Normal if total == 0 => Ok(0),
            ExitKind::Normal => Err(DirectNetHandlerError::LeakedHandles(LeakedHandles {
                task: self.task,
                count: total,
            })),
        }
    }

    /// Asserts normal task completion with zero leaked handles (`finish_task(ExitKind::Normal)`).
    #[inline]
    pub fn finish_checked(&mut self) -> Result<(), DirectNetHandlerError> {
        self.finish_task(ExitKind::Normal).map(|_| ())
    }

    fn remove_listener_addr(&mut self, token: u64) {
        if let Some(pos) = self.listener_addrs.as_slice().iter().position(|(t, _)| *t == token) {
            _ = self.listener_addrs.remove_swap(pos);
        }
    }

    #[inline(never)]
    fn exec_listen(
        &mut self,
        req: Listen<'io>,
    ) -> Result<Result<Listener<'io>, NetError>, DirectNetHandlerError> {
        let Listen { addr, _brand: _ } = req;
        let (token, bound_addr) = match self.backend.listen(&addr) {
            Ok(pair) => pair,
            Err(e) => return Ok(Err(e)),
        };
        let id = match self.listeners.insert(self.task, token) {
            Ok(id) => id,
            Err(e) => {
                _ = self.backend.unlisten(token);
                return Err(DirectNetHandlerError::TableFull(e));
            }
        };
        self.listener_addrs.push((token, bound_addr));
        Ok(Ok(Listener::from_id(id)))
    }

    #[inline(never)]
    fn exec_accept(
        &mut self,
        req: Accept<'io>,
    ) -> Result<Result<ConnHandle<'io>, NetError>, DirectNetHandlerError> {
        let Accept { listener } = req;
        let token = match self.listeners.get(listener) {
            Ok(&tok) => tok,
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        let (conn_tok, _peer_addr) = match self.backend.accept(token) {
            Ok(pair) => pair,
            Err(e) => return Ok(Err(e)),
        };
        let id = match self.conns.insert(self.task, conn_tok) {
            Ok(id) => id,
            Err(e) => {
                _ = self.backend.close_conn(conn_tok);
                return Err(DirectNetHandlerError::TableFull(e));
            }
        };
        Ok(Ok(ConnHandle::from_id(id)))
    }

    #[inline(never)]
    fn exec_connect(
        &mut self,
        req: Connect<'io>,
    ) -> Result<Result<ConnHandle<'io>, NetError>, DirectNetHandlerError> {
        let Connect { addr, _brand: _ } = req;
        let conn_tok = match self.backend.connect(&addr) {
            Ok(tok) => tok,
            Err(e) => return Ok(Err(e)),
        };
        let id = match self.conns.insert(self.task, conn_tok) {
            Ok(id) => id,
            Err(e) => {
                _ = self.backend.close_conn(conn_tok);
                return Err(DirectNetHandlerError::TableFull(e));
            }
        };
        Ok(Ok(ConnHandle::from_id(id)))
    }

    #[inline(never)]
    fn exec_send_from<'a>(
        &mut self,
        req: SendFrom<'io, 'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, NetError>>, DirectNetHandlerError> {
        let SendFrom { conn, buf } = req;
        let token = match self.conns.get(conn) {
            Ok(&tok) => tok,
            Err(e) => return Ok(Err(BufError { buf, error: NetError::StaleHandle(e) })),
        };
        let total = buf.len();
        if total == 0 {
            return match self.backend.send(token, &[]) {
                Ok(_) => Ok(Ok(buf)),
                Err(error) => Ok(Err(BufError { buf, error })),
            };
        }
        let mut sent = 0usize;
        while sent < total {
            match self.backend.send(token, &buf.as_slice()[sent..]) {
                Ok(0) => {
                    return Ok(Err(BufError {
                        buf,
                        error: NetError::BrokenPipe {
                            message: "connection closed during send (wrote 0 bytes)".to_owned(),
                        },
                    }));
                }
                Ok(n) => {
                    sent = sent.saturating_add(n).min(total);
                }
                Err(error) => return Ok(Err(BufError { buf, error })),
            }
        }
        Ok(Ok(buf))
    }

    #[inline(never)]
    fn exec_recv_into<'a>(
        &mut self,
        req: RecvInto<'io, 'a>,
    ) -> Result<Result<FilledBuf<'a>, BufError<'a, NetError>>, DirectNetHandlerError> {
        let RecvInto { conn, mut buf } = req;
        let token = match self.conns.get(conn) {
            Ok(&tok) => tok,
            Err(e) => return Ok(Err(BufError { buf, error: NetError::StaleHandle(e) })),
        };
        let pre_sized = !buf.is_empty();
        let want = if pre_sized { buf.len() } else { buf.capacity() };
        if !pre_sized {
            buf.bytes.resize(want, 0);
        }
        match self.backend.recv(token, buf.as_mut_slice()) {
            Ok(n) => {
                let filled = n.min(want);
                if !pre_sized {
                    buf.bytes.truncate(filled);
                }
                Ok(Ok(FilledBuf { buf, filled: ByteLen(filled as u32) }))
            }
            Err(error) => {
                if !pre_sized {
                    buf.bytes.clear();
                }
                Ok(Err(BufError { buf, error }))
            }
        }
    }

    #[inline(never)]
    fn exec_close_conn(
        &mut self,
        req: CloseConn<'io>,
    ) -> Result<Result<(), NetError>, DirectNetHandlerError> {
        let CloseConn { conn } = req;
        let token = match self.conns.remove(conn, self.task) {
            Ok(tok) => tok,
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        Ok(self.backend.close_conn(token))
    }

    #[inline(never)]
    fn exec_unlisten(
        &mut self,
        req: Unlisten<'io>,
    ) -> Result<Result<(), NetError>, DirectNetHandlerError> {
        let Unlisten { listener } = req;
        let token = match self.listeners.remove(listener, self.task) {
            Ok(tok) => tok,
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        self.remove_listener_addr(token);
        Ok(self.backend.unlisten(token))
    }
}

macro_rules! impl_direct_net_handler {
    ([$($lt:lifetime),*], $req:ty, $ans:ty, $method:ident) => {
        impl<'io, $($lt,)* 'b, B: DirectNetBackend> Handler<$req> for DirectNetHandler<'io, 'b, B> {
            type Error = DirectNetHandlerError;

            fn perform<Out>(
                &mut self,
                request: $req,
                deliver: impl FnOnce(Perform<$ans>) -> Out,
            ) -> Result<Out, Self::Error> {
                let ans = self.$method(request)?;
                Ok(deliver(Perform::Answered(ans)))
            }
        }
    };
}

impl_direct_net_handler!([], Listen<'io>, Result<Listener<'io>, NetError>, exec_listen);
impl_direct_net_handler!([], Accept<'io>, Result<ConnHandle<'io>, NetError>, exec_accept);
impl_direct_net_handler!([], Connect<'io>, Result<ConnHandle<'io>, NetError>, exec_connect);
impl_direct_net_handler!(
    ['a],
    SendFrom<'io, 'a>,
    Result<DmaBuf<'a>, BufError<'a, NetError>>,
    exec_send_from
);
impl_direct_net_handler!(
    ['a],
    RecvInto<'io, 'a>,
    Result<FilledBuf<'a>, BufError<'a, NetError>>,
    exec_recv_into
);
impl_direct_net_handler!([], CloseConn<'io>, Result<(), NetError>, exec_close_conn);
impl_direct_net_handler!([], Unlisten<'io>, Result<(), NetError>, exec_unlisten);

#[cfg(test)]
mod tests {
    use blaze_machine::{Handler, Perform};
    use blaze_own::testing::heap;
    use blaze_own::{Alloc, with_brand};

    use super::{
        DirectNetBackend, DirectNetHandler, DirectNetHandlerError, InMemoryDirectNet,
        NativeDirectNet,
    };
    use crate::Address;
    use crate::handle::{Conn as ConnHandle, HandleError, LeakedHandles, TaskId};
    use crate::request::net::{
        Accept, CloseConn, Connect, ConnectOpts, Listen, ListenOpts, NetError, RecvInto,
        RecvIntoOpts, SendFrom, SendFromOpts,
    };
    use crate::request::{ByteLen, DmaBuf};

    fn recv_exact<'io, B: DirectNetBackend>(
        handler: &mut DirectNetHandler<'io, '_, B>,
        conn: &ConnHandle<'io>,
        expected_len: usize,
        alloc: Alloc<'static>,
    ) -> std::vec::Vec<u8> {
        let mut out = std::vec::Vec::with_capacity(expected_len);
        let mut buf = DmaBuf::with_capacity_in(ByteLen(64), alloc);
        while out.len() < expected_len {
            let remaining = (expected_len - out.len()).min(64);
            buf.resize_zero(remaining);
            let filled = handler
                .perform(conn.recv_into(RecvIntoOpts { buf }), |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap()
                .unwrap();
            let n = filled.filled.0 as usize;
            assert!(n > 0, "unexpected EOF after {} of {expected_len} bytes", out.len());
            out.extend_from_slice(&filled.buf.as_slice()[..n]);
            buf = filled.buf;
        }
        buf.release();
        out
    }

    fn run_echo_framing_sequence<B: DirectNetBackend>(
        backend: &mut B,
        request_payload: &[u8],
        reply_payload: &[u8],
        alloc: Alloc<'static>,
    ) -> (std::vec::Vec<u8>, std::vec::Vec<u8>) {
        let mut server_got = std::vec::Vec::new();
        let mut client_got = std::vec::Vec::new();

        with_brand(|brand| {
            let mut handler = DirectNetHandler::new(backend, TaskId(801), brand, alloc);

            let listener = handler
                .perform(
                    Listen::new(ListenOpts {
                        addr: Address::Tcp("127.0.0.1:0".parse().unwrap()),
                    }),
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap()
                .unwrap();
            let bound_addr = handler.listener_addr(&listener).unwrap().clone();
            match &bound_addr {
                Address::Tcp(sock) => assert_ne!(sock.port(), 0, "ephemeral port must be non-zero"),
                Address::Unix(_) | Address::Inherited { .. } => {
                    panic!("expected TCP bound address, got {bound_addr}")
                }
            }

            let client = handler
                .perform(Connect::new(ConnectOpts { addr: bound_addr }), |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap()
                .unwrap();
            let server = handler
                .perform(listener.accept(), |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap()
                .unwrap();

            // Client -> Server
            let sent_req = handler
                .perform(
                    client.send_from(SendFromOpts {
                        buf: DmaBuf::from_slice_in(request_payload, alloc),
                    }),
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap()
                .unwrap();
            sent_req.release();

            server_got = recv_exact(&mut handler, &server, request_payload.len(), alloc);

            // Server -> Client reply
            let sent_reply = handler
                .perform(
                    server.send_from(SendFromOpts {
                        buf: DmaBuf::from_slice_in(reply_payload, alloc),
                    }),
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap()
                .unwrap();
            sent_reply.release();

            client_got = recv_exact(&mut handler, &client, reply_payload.len(), alloc);

            // Close client first and verify server observes clean EOF (0 bytes).
            assert_eq!(
                handler
                    .perform(client.close(), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .unwrap(),
                Ok(())
            );
            let eof = handler
                .perform(
                    server.recv_into(RecvIntoOpts {
                        buf: DmaBuf::with_capacity_in(ByteLen(16), alloc),
                    }),
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap()
                .unwrap();
            assert_eq!(eof.filled, ByteLen(0));
            assert!(eof.buf.is_empty());
            eof.buf.release();

            // Close server and unlisten.
            assert_eq!(
                handler
                    .perform(server.close(), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .unwrap(),
                Ok(())
            );
            assert_eq!(
                handler
                    .perform(listener.unlisten(), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .unwrap(),
                Ok(())
            );
            assert_eq!(handler.finish_checked(), Ok(()));
        });

        (server_got, client_got)
    }

    #[test]
    fn direct_net_handler_native_matches_in_memory_byte_for_byte() {
        let h = heap();
        let request_payload: std::vec::Vec<u8> =
            (0..257usize).map(|i| ((i.wrapping_mul(37).wrapping_add(11)) & 0xFF) as u8).collect();
        let reply_payload: std::vec::Vec<u8> =
            (0..193usize).map(|i| ((i.wrapping_mul(53).wrapping_add(29)) & 0xFF) as u8).collect();

        // Exercise short-write and short-read chunk caps in InMemoryDirectNet.
        let mut in_mem = InMemoryDirectNet::new(h.alloc())
            .with_max_write_chunk(13)
            .with_max_read_chunk(19);
        let mut native = NativeDirectNet::new();

        let (mem_server_got, mem_client_got) =
            run_echo_framing_sequence(&mut in_mem, &request_payload, &reply_payload, h.alloc());
        let (nat_server_got, nat_client_got) =
            run_echo_framing_sequence(&mut native, &request_payload, &reply_payload, h.alloc());

        assert_eq!(mem_server_got, request_payload);
        assert_eq!(nat_server_got, request_payload);
        assert_eq!(mem_server_got, nat_server_got);

        assert_eq!(mem_client_got, reply_payload);
        assert_eq!(nat_client_got, reply_payload);
        assert_eq!(mem_client_got, nat_client_got);

        assert_eq!(in_mem.open_conn_count(), 0);
        assert_eq!(in_mem.open_listener_count(), 0);
        assert_eq!(native.open_conn_count(), 0);
        assert_eq!(native.open_listener_count(), 0);
    }

    #[test]
    fn direct_net_handler_catches_leaked_conn_and_stale_handle() {
        let h = heap();
        let mut in_mem = InMemoryDirectNet::new(h.alloc());

        with_brand(|brand| {
            let mut handler = DirectNetHandler::new(&mut in_mem, TaskId(802), brand, h.alloc());

            let listener = handler
                .perform(
                    Listen::new(ListenOpts {
                        addr: Address::Tcp("127.0.0.1:0".parse().unwrap()),
                    }),
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap()
                .unwrap();
            let bound_addr = handler.listener_addr(&listener).unwrap().clone();

            let client = handler
                .perform(Connect::new(ConnectOpts { addr: bound_addr }), |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap()
                .unwrap();
            let server = handler
                .perform(listener.accept(), |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap()
                .unwrap();

            // Close `client` cleanly, then verify stale handle reuse is rejected across Send/Recv/Close.
            let stale_conn = client.id();
            assert_eq!(
                handler
                    .perform(client.close(), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .unwrap(),
                Ok(())
            );

            let stale_send = handler
                .perform(
                    SendFrom {
                        conn: stale_conn,
                        buf: DmaBuf::from_slice_in(b"stale", h.alloc()),
                    },
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap();
            match stale_send {
                Err(be) => {
                    assert!(matches!(be.error, NetError::StaleHandle(HandleError::Stale { .. })));
                    be.buf.release();
                }
                Ok(buf) => {
                    buf.release();
                    panic!("stale conn send must fail with StaleHandle");
                }
            }

            let stale_recv = handler
                .perform(
                    RecvInto {
                        conn: stale_conn,
                        buf: DmaBuf::with_capacity_in(ByteLen(16), h.alloc()),
                    },
                    |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    },
                )
                .unwrap();
            match stale_recv {
                Err(be) => {
                    assert!(matches!(be.error, NetError::StaleHandle(HandleError::Stale { .. })));
                    be.buf.release();
                }
                Ok(filled) => {
                    filled.buf.release();
                    panic!("stale conn recv must fail with StaleHandle");
                }
            }

            let stale_close = handler
                .perform(CloseConn { conn: stale_conn }, |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap();
            assert!(matches!(stale_close, Err(NetError::StaleHandle(HandleError::Stale { .. }))));

            // Close `listener` cleanly and verify stale Accept is rejected.
            let stale_listener = listener.id();
            assert_eq!(
                handler
                    .perform(listener.unlisten(), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .unwrap(),
                Ok(())
            );
            let stale_accept = handler
                .perform(Accept { listener: stale_listener }, |p| match p {
                    Perform::Answered(res) => res,
                    _ => unreachable!(),
                })
                .unwrap();
            assert!(matches!(stale_accept, Err(NetError::StaleHandle(HandleError::Stale { .. }))));

            // Deliberately leak `server` without calling `server.close()`:
            // `finish_checked` must fail loudly with `LeakedHandles` AND reclaim the backend conn.
            _ = server.into_id();
            assert_eq!(
                handler.finish_checked(),
                Err(DirectNetHandlerError::LeakedHandles(LeakedHandles {
                    task: TaskId(802),
                    count: 1,
                }))
            );
        });

        assert_eq!(in_mem.open_conn_count(), 0);
        assert_eq!(in_mem.open_listener_count(), 0);
    }
}
