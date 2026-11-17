//! UDP's system calls for the host's datagram effects ([`crate::Effect::Bind`],
//! [`crate::Effect::SendTo`], [`crate::Effect::RecvFrom`]): a socket's options, and a batch
//! sent or received in one call.
//!
//! - Linux: `sendmmsg` of one message per run, a run of several datagrams cut by the kernel
//!   (`UDP_SEGMENT`); `recvmmsg` into the batch's slots, a slot filled by a coalesced run where
//!   the socket asked for `UDP_GRO`.
//! - macOS: `sendmsg_x` and `recvmsg_x`, which send or receive many datagrams in one call as
//!   `sendmmsg` and `recvmmsg` do. They are private (xnu's `bsd/sys/socket_private.h`, in
//!   libSystem since macOS 10.11 and what Network.framework's own UDP path is built on); the
//!   public calls move one datagram each, so a QUIC flight of 64 datagrams would be 64 calls.
//!   Their symbols are looked up by a test (`the_private_batch_calls_are_there`) so a system
//!   that drops them fails the gate, not a build. A run of several datagrams is sent as its
//!   datagrams, one message each, as macOS has no segmentation offload.
//!
//! Why unsafe: the message headers, their control messages and the addresses they point at
//! are the kernel's ABI, filled and read through raw pointers into buffers this module owns
//! for the length of the one call; a receive's bytes land in the batch's spare capacity and
//! are counted as initialized only up to what the kernel said it wrote.

use std::{
    io,
    mem::{self, MaybeUninit},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket},
    os::fd::{AsRawFd as _, RawFd},
    ptr,
};

use crate::dgram::{BATCH_MOST, Batch, Ecn, Head, Options, Slots};

/// The bytes each message's control messages may take: a traffic class, a source address
/// (an `in6_pktinfo`, 20 bytes) and a segment size, each with its header, with room to spare.
/// Every message's own, zeroed per call.
const CONTROL: usize = 128;

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct Control([u8; CONTROL]);

impl Control {
    const ZERO: Control = Control([0; CONTROL]);
}

/// The messages one call sends or receives: [`BATCH_MOST`], as a batch's runs are.
const MESSAGES: usize = BATCH_MOST;

/// `at`, bound, nonblocking and close-on-exec, with each of `options` set. An option the
/// system refuses fails the bind, naming it.
pub(crate) fn bind(at: SocketAddr, options: Options) -> Result<UdpSocket, (&'static str, io::Error)> {
    // std's sockets are made close-on-exec.
    let socket = UdpSocket::bind(at).map_err(|error| ("the bind", error))?;
    socket.set_nonblocking(true).map_err(|error| ("O_NONBLOCK", error))?;
    let fd = socket.as_raw_fd();
    let v6 = at.is_ipv6();
    if options.ecn {
        match v6 {
            false => set(fd, libc::IPPROTO_IP, libc::IP_RECVTOS, 1, "IP_RECVTOS")?,
            true => set(fd, libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS, 1, "IPV6_RECVTCLASS")?,
        }
    }
    if options.local_address {
        match v6 {
            #[cfg(target_os = "linux")]
            false => set(fd, libc::IPPROTO_IP, libc::IP_PKTINFO, 1, "IP_PKTINFO")?,
            #[cfg(target_os = "macos")]
            false => set(fd, libc::IPPROTO_IP, libc::IP_RECVDSTADDR, 1, "IP_RECVDSTADDR")?,
            true => set(fd, libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO, 1, "IPV6_RECVPKTINFO")?,
        }
    }
    if options.dont_fragment {
        #[cfg(target_os = "linux")]
        match v6 {
            false => set(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                libc::IP_PMTUDISC_PROBE,
                "IP_MTU_DISCOVER",
            )?,
            true => set(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_MTU_DISCOVER,
                libc::IPV6_PMTUDISC_PROBE,
                "IPV6_MTU_DISCOVER",
            )?,
        }
        #[cfg(target_os = "macos")]
        match v6 {
            false => set(fd, libc::IPPROTO_IP, libc::IP_DONTFRAG, 1, "IP_DONTFRAG")?,
            true => set(fd, libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG, 1, "IPV6_DONTFRAG")?,
        }
    }
    // macOS has no receive offload: `coalesce` permits runs, and none come.
    #[cfg(target_os = "linux")]
    if options.coalesce {
        set(fd, libc::SOL_UDP, libc::UDP_GRO, 1, "UDP_GRO")?;
    }
    Ok(socket)
}

fn set(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
    what: &'static str,
) -> Result<(), (&'static str, io::Error)> {
    // SAFETY: an int option of an open socket, its value on the stack for the call.
    let done = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&raw const value).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    match done {
        0 => Ok(()),
        _ => Err((what, io::Error::last_os_error())),
    }
}

/// How far a send of a batch has got: the next datagram to send is at byte `offset` of run
/// `run`, which starts at byte `byte` of the batch's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct At {
    pub(crate) run: usize,
    pub(crate) byte: usize,
    pub(crate) offset: usize,
}

impl At {
    pub(crate) fn done(&self, batch: &Batch) -> bool {
        self.run >= batch.heads.len()
    }
}

/// `peer` written as the system names it, for a socket of the family `v6` says: a v4 peer of a
/// v6 socket is its v4-mapped address.
fn write_addr(peer: SocketAddr, v6: bool, storage: &mut libc::sockaddr_storage) -> libc::socklen_t {
    let peer = match (peer, v6) {
        (SocketAddr::V4(it), true) => {
            SocketAddr::V6(SocketAddrV6::new(it.ip().to_ipv6_mapped(), it.port(), 0, 0))
        }
        (peer, _) => peer,
    };
    // SAFETY: all zeros is a valid `sockaddr_storage`.
    *storage = unsafe { mem::zeroed() };
    let len = match peer {
        SocketAddr::V4(address) => {
            // SAFETY: `sockaddr_storage` is large and aligned enough for any address.
            let in4 = unsafe { &mut *(&raw mut *storage).cast::<libc::sockaddr_in>() };
            in4.sin_family = libc::AF_INET as libc::sa_family_t;
            in4.sin_port = address.port().to_be();
            in4.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(address.ip().octets()) };
            size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(address) => {
            // SAFETY: as above.
            let in6 = unsafe { &mut *(&raw mut *storage).cast::<libc::sockaddr_in6>() };
            in6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            in6.sin6_port = address.port().to_be();
            in6.sin6_flowinfo = address.flowinfo();
            in6.sin6_addr = libc::in6_addr { s6_addr: address.ip().octets() };
            in6.sin6_scope_id = address.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
    };
    super::set_len(storage, len);
    len as libc::socklen_t
}

/// The address the system wrote, a v4-mapped one said as v4; `None` for a family that is
/// neither.
fn read_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: the family says it is a `sockaddr_in`.
            let in4 = unsafe { &*(&raw const *storage).cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(in4.sin_addr.s_addr.to_ne_bytes());
            Some(SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(in4.sin_port))))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says it is a `sockaddr_in6`.
            let in6 = unsafe { &*(&raw const *storage).cast::<libc::sockaddr_in6>() };
            let ip = Ipv6Addr::from(in6.sin6_addr.s6_addr);
            let port = u16::from_be(in6.sin6_port);
            Some(match ip.to_ipv4_mapped() {
                Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, port)),
                None => SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    port,
                    in6.sin6_flowinfo,
                    in6.sin6_scope_id,
                )),
            })
        }
        _ => None,
    }
}

/// One control message to send.
enum Cmsg {
    Int { level: libc::c_int, kind: libc::c_int, value: libc::c_int },
    #[cfg(target_os = "linux")]
    U16 { level: libc::c_int, kind: libc::c_int, value: u16 },
    #[cfg(target_os = "linux")]
    Pktinfo4(libc::in_pktinfo),
    Pktinfo6(libc::in6_pktinfo),
}

impl Cmsg {
    fn raw(&self) -> (libc::c_int, libc::c_int, *const u8, usize) {
        match self {
            Cmsg::Int { level, kind, value } => {
                (*level, *kind, (&raw const *value).cast(), size_of::<libc::c_int>())
            }
            #[cfg(target_os = "linux")]
            Cmsg::U16 { level, kind, value } => {
                (*level, *kind, (&raw const *value).cast(), size_of::<u16>())
            }
            #[cfg(target_os = "linux")]
            Cmsg::Pktinfo4(info) => (
                libc::IPPROTO_IP,
                libc::IP_PKTINFO,
                (&raw const *info).cast(),
                size_of::<libc::in_pktinfo>(),
            ),
            Cmsg::Pktinfo6(info) => (
                libc::IPPROTO_IPV6,
                libc::IPV6_PKTINFO,
                (&raw const *info).cast(),
                size_of::<libc::in6_pktinfo>(),
            ),
        }
    }
}

/// The control messages a run's head asks for, at most three: its codepoint where it has
/// one, its source address where it names one, and (Linux) its segment size where it is cut.
fn cmsgs(head: &Head, peer_v4: bool, out: &mut [Option<Cmsg>; 3]) {
    *out = [None, None, None];
    if head.ecn != Ecn::NotEct {
        let value = libc::c_int::from(head.ecn.bits());
        out[0] = Some(match peer_v4 {
            true => Cmsg::Int { level: libc::IPPROTO_IP, kind: libc::IP_TOS, value },
            false => Cmsg::Int { level: libc::IPPROTO_IPV6, kind: libc::IPV6_TCLASS, value },
        });
    }
    match head.local {
        #[cfg(target_os = "linux")]
        Some(IpAddr::V4(ip)) => {
            // SAFETY: all zeros is a valid `in_pktinfo`: no interface, the address below.
            let mut info: libc::in_pktinfo = unsafe { mem::zeroed() };
            info.ipi_spec_dst = libc::in_addr { s_addr: u32::from_ne_bytes(ip.octets()) };
            out[1] = Some(Cmsg::Pktinfo4(info));
        }
        // macOS sends from a v4 address by `IP_SENDSRCADDR`, which a socket bound at one
        // address does not need: the asker names its source only on a socket bound at the
        // unspecified address, refused on macOS by `send` before it gets here.
        #[cfg(not(target_os = "linux"))]
        Some(IpAddr::V4(_)) => {}
        Some(IpAddr::V6(ip)) => {
            // SAFETY: all zeros is a valid `in6_pktinfo`.
            let mut info: libc::in6_pktinfo = unsafe { mem::zeroed() };
            info.ipi6_addr = libc::in6_addr { s6_addr: ip.octets() };
            out[1] = Some(Cmsg::Pktinfo6(info));
        }
        None => {}
    }
    #[cfg(target_os = "linux")]
    if head.segment < head.len {
        let value = u16::try_from(head.segment).expect("a segment is at most DATAGRAM_MOST");
        out[2] = Some(Cmsg::U16 { level: libc::SOL_UDP, kind: libc::UDP_SEGMENT, value });
    }
}

/// `msg`'s control messages written into `control`: none leaves it without.
fn encode(msg: &mut libc::msghdr, control: &mut Control, items: &[Option<Cmsg>; 3]) {
    if items.iter().all(Option::is_none) {
        msg.msg_control = ptr::null_mut();
        msg.msg_controllen = 0;
        return;
    }
    *control = Control::ZERO;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = CONTROL as _;
    let mut used = 0usize;
    // SAFETY: `msg` points at `control`, zeroed and `CONTROL` long, for the length of this
    // function; each header is written only where `CMSG_FIRSTHDR` or `CMSG_NXTHDR` said it
    // fits, and its data, at most an `in6_pktinfo`, fits its `CMSG_SPACE` within `CONTROL`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        for item in items.iter().flatten() {
            assert!(!cmsg.is_null(), "a datagram's control messages fit CONTROL, {CONTROL} bytes");
            let (level, kind, data, len) = item.raw();
            (*cmsg).cmsg_level = level;
            (*cmsg).cmsg_type = kind;
            (*cmsg).cmsg_len = libc::CMSG_LEN(len as libc::c_uint) as _;
            ptr::copy_nonoverlapping(data, libc::CMSG_DATA(cmsg), len);
            used += libc::CMSG_SPACE(len as libc::c_uint) as usize;
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    msg.msg_controllen = used as _;
}

/// What a received message's control messages said: its codepoint, the address it arrived at,
/// and (Linux) its run's segment size.
#[derive(Default)]
struct Said {
    ecn: Option<Ecn>,
    local: Option<IpAddr>,
    segment: Option<u32>,
}

fn decode(msg: &libc::msghdr) -> Said {
    let mut said = Said::default();
    if msg.msg_control.is_null() || (msg.msg_controllen as usize) < size_of::<libc::cmsghdr>() {
        return said;
    }
    // SAFETY: the kernel wrote `msg_controllen` bytes of control messages into the buffer
    // `msg_control` points at, which this module owns; each header is read where
    // `CMSG_FIRSTHDR` or `CMSG_NXTHDR` found it within them, and its data no longer than its
    // `cmsg_len` says (unaligned reads: the data is aligned only to the header's alignment).
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let (level, kind) = ((*cmsg).cmsg_level, (*cmsg).cmsg_type);
            let data = libc::CMSG_DATA(cmsg);
            let len = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
            // The codepoint: a byte (Linux's IP_TOS, macOS's IP_RECVTOS) or an int.
            let byte_or_int = || match len {
                1 => u8::from(*data),
                _ => ptr::read_unaligned(data.cast::<libc::c_int>()) as u8,
            };
            match (level, kind) {
                (libc::IPPROTO_IP, libc::IP_TOS) => said.ecn = Some(Ecn::of_tos(byte_or_int())),
                #[cfg(target_os = "macos")]
                (libc::IPPROTO_IP, libc::IP_RECVTOS) => {
                    said.ecn = Some(Ecn::of_tos(byte_or_int()));
                }
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) => {
                    said.ecn = Some(Ecn::of_tos(byte_or_int()));
                }
                #[cfg(target_os = "linux")]
                (libc::IPPROTO_IP, libc::IP_PKTINFO) => {
                    let info = ptr::read_unaligned(data.cast::<libc::in_pktinfo>());
                    said.local = Some(IpAddr::V4(Ipv4Addr::from(info.ipi_addr.s_addr.to_ne_bytes())));
                }
                #[cfg(target_os = "macos")]
                (libc::IPPROTO_IP, libc::IP_RECVDSTADDR) => {
                    let addr = ptr::read_unaligned(data.cast::<libc::in_addr>());
                    said.local = Some(IpAddr::V4(Ipv4Addr::from(addr.s_addr.to_ne_bytes())));
                }
                (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => {
                    let info = ptr::read_unaligned(data.cast::<libc::in6_pktinfo>());
                    let ip = Ipv6Addr::from(info.ipi6_addr.s6_addr);
                    said.local = Some(match ip.to_ipv4_mapped() {
                        Some(v4) => IpAddr::V4(v4),
                        None => IpAddr::V6(ip),
                    });
                }
                #[cfg(target_os = "linux")]
                (libc::SOL_UDP, libc::UDP_GRO) => {
                    let size = ptr::read_unaligned(data.cast::<libc::c_int>());
                    said.segment = u32::try_from(size).ok();
                }
                _ => {}
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    said
}

/// The messages of one send: their headers, the iovec and address each points at, and their
/// control messages. On the stack of the call, never moved while the kernel reads it.
struct Messages {
    msgs: [libc::msghdr; MESSAGES],
    iovs: [libc::iovec; MESSAGES],
    names: [libc::sockaddr_storage; MESSAGES],
    controls: [Control; MESSAGES],
    /// Where each message's datagrams end: the [`At`] after it.
    after: [At; MESSAGES],
    /// Of a receive: each message's length and flags, as the kernel said them.
    got: [(usize, libc::c_int); MESSAGES],
    len: usize,
}

impl Messages {
    fn new() -> Messages {
        // SAFETY: all zeros is a valid message header, iovec and address: null pointers and
        // zero lengths, filled before the kernel sees one.
        unsafe {
            Messages {
                msgs: mem::zeroed(),
                iovs: mem::zeroed(),
                names: mem::zeroed(),
                controls: [Control::ZERO; MESSAGES],
                after: [At::default(); MESSAGES],
                got: [(0, 0); MESSAGES],
                len: 0,
            }
        }
    }

    /// The messages of `batch` from `at`, at most [`MESSAGES`]: one per run where `whole`
    /// (the kernel cuts it), else one per datagram.
    fn fill(&mut self, batch: &Batch, mut at: At, v6: bool, whole: bool) {
        self.len = 0;
        let mut items = [None, None, None];
        while self.len < MESSAGES && !at.done(batch) {
            let head = &batch.heads[at.run];
            let run_len = head.len as usize;
            let (start, len) = match whole {
                true => (at.byte, run_len),
                false => {
                    let len = (head.segment as usize).min(run_len - at.offset);
                    (at.byte + at.offset, len)
                }
            };
            let n = self.len;
            let namelen = write_addr(head.peer, v6, &mut self.names[n]);
            self.iovs[n] = libc::iovec {
                iov_base: batch.bytes[start..start + len].as_ptr().cast_mut().cast(),
                iov_len: len,
            };
            let mut one = *head;
            if !whole {
                one.len = len as u32;
                one.segment = len as u32;
            }
            cmsgs(&one, head.peer.is_ipv4(), &mut items);
            let msg = &mut self.msgs[n];
            // SAFETY: all zeros is a valid header; every field the kernel reads is set below.
            *msg = unsafe { mem::zeroed() };
            msg.msg_name = (&raw mut self.names[n]).cast();
            msg.msg_namelen = namelen;
            msg.msg_iov = &raw mut self.iovs[n];
            msg.msg_iovlen = 1;
            encode(msg, &mut self.controls[n], &items);
            at.offset += len;
            if at.offset >= run_len {
                at = At { run: at.run + 1, byte: at.byte + run_len, offset: 0 };
            }
            self.after[n] = at;
            self.len += 1;
        }
    }
}

/// One call's send of `batch` from `at` on `socket`: how far it got, at least one message
/// further; an error the system said for the first message (`WouldBlock` where the socket has
/// no room yet). `v6` is the socket's family.
pub(crate) fn send(socket: &UdpSocket, v6: bool, batch: &Batch, at: At) -> io::Result<At> {
    // On the stack (some 25 KiB): a host thread's or the driver's, either far larger.
    let mut messages = Messages::new();
    send_into(socket, v6, batch, at, &mut messages)
}

#[cfg(target_os = "linux")]
fn send_into(
    socket: &UdpSocket,
    v6: bool,
    batch: &Batch,
    at: At,
    messages: &mut Messages,
) -> io::Result<At> {
    messages.fill(batch, at, v6, true);
    // SAFETY: an `mmsghdr` is a `msghdr` and the count the kernel writes; all zeros is valid.
    let mut mmsgs: [libc::mmsghdr; MESSAGES] = unsafe { mem::zeroed() };
    for (to, from) in mmsgs.iter_mut().zip(&messages.msgs[..messages.len]) {
        // A copy of a header whose pointers point into `messages`.
        to.msg_hdr = *from;
    }
    loop {
        // SAFETY: `messages.len` headers, each pointing at its address, iovec and control in
        // `messages` and at the batch's bytes, all alive and unmoved across the call.
        let sent = unsafe {
            libc::sendmmsg(socket.as_raw_fd(), mmsgs.as_mut_ptr(), messages.len as _, 0 as _)
        };
        match sent {
            n if n > 0 => return Ok(messages.after[n as usize - 1]),
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// xnu's `struct msghdr_x` (`bsd/sys/socket_private.h`): a `msghdr` and the bytes moved.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MsghdrX {
    msg_name: *mut libc::c_void,
    msg_namelen: libc::socklen_t,
    msg_iov: *mut libc::iovec,
    msg_iovlen: libc::c_int,
    msg_control: *mut libc::c_void,
    msg_controllen: libc::socklen_t,
    msg_flags: libc::c_int,
    msg_datalen: libc::size_t,
}

// Private to libSystem (rule macos-private-apis): many datagrams in one call, where the public
// `sendmsg` and `recvmsg` move one. Declared in xnu's `bsd/sys/socket_private.h`; the symbols
// are checked by `tests::the_private_batch_calls_are_there`.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sendmsg_x(s: libc::c_int, msgp: *const MsghdrX, cnt: libc::c_uint, flags: libc::c_int)
    -> isize;
    fn recvmsg_x(s: libc::c_int, msgp: *const MsghdrX, cnt: libc::c_uint, flags: libc::c_int)
    -> isize;
}

#[cfg(target_os = "macos")]
fn to_x(msg: &libc::msghdr) -> MsghdrX {
    MsghdrX {
        msg_name: msg.msg_name,
        msg_namelen: msg.msg_namelen,
        msg_iov: msg.msg_iov,
        msg_iovlen: msg.msg_iovlen,
        msg_control: msg.msg_control,
        msg_controllen: msg.msg_controllen,
        msg_flags: 0,
        msg_datalen: 0,
    }
}

#[cfg(target_os = "macos")]
fn send_into(
    socket: &UdpSocket,
    v6: bool,
    batch: &Batch,
    at: At,
    messages: &mut Messages,
) -> io::Result<At> {
    if !v6 && batch.heads[at.run..].iter().any(|it| matches!(it.local, Some(IpAddr::V4(_)))) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "a v4 source address named per datagram: macOS's IP_SENDSRCADDR is not built",
        ));
    }
    messages.fill(batch, at, v6, false);
    let mut xs = [MsghdrX {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: ptr::null_mut(),
        msg_iovlen: 0,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
        msg_datalen: 0,
    }; MESSAGES];
    for (to, from) in xs.iter_mut().zip(&messages.msgs[..messages.len]) {
        *to = to_x(from);
    }
    loop {
        // SAFETY: as Linux's `sendmmsg` above: `messages.len` headers into `messages` and the
        // batch, alive and unmoved across the call.
        let sent =
            unsafe { sendmsg_x(socket.as_raw_fd(), xs.as_ptr(), messages.len as libc::c_uint, 0) };
        match sent {
            n if n > 0 => return Ok(messages.after[n as usize - 1]),
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// One call's receive into `batch` (cleared first) of at most `slots`: the runs there now, at
/// least one, or `WouldBlock` where none is. Each head says its peer, its codepoint where the
/// socket asked for them, the address it arrived at where it asked, and whether the slot cut
/// it short.
pub(crate) fn recv(socket: &UdpSocket, batch: &mut Batch, slots: Slots) -> io::Result<usize> {
    let (most, size) = (usize::from(slots.most).min(MESSAGES), slots.size as usize);
    batch.clear();
    batch.bytes.reserve(most * size);
    batch.heads.reserve(most);
    let mut messages = Messages::new();
    let base = batch.bytes.spare_capacity_mut().as_mut_ptr();
    for n in 0..most {
        // SAFETY: slot `n` is within the `most * size` bytes reserved above.
        let slot: *mut MaybeUninit<u8> = unsafe { base.add(n * size) };
        messages.iovs[n] = libc::iovec { iov_base: slot.cast(), iov_len: size };
        let msg = &mut messages.msgs[n];
        // SAFETY: all zeros is a valid header.
        *msg = unsafe { mem::zeroed() };
        msg.msg_name = (&raw mut messages.names[n]).cast();
        msg.msg_namelen = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &raw mut messages.iovs[n];
        msg.msg_iovlen = 1;
        msg.msg_control = messages.controls[n].0.as_mut_ptr().cast();
        msg.msg_controllen = CONTROL as _;
    }
    let got = recv_into(socket, &mut messages, most)?;
    // Each message's bytes moved down to follow the last's, so the batch's bytes are its runs
    // end to end; a move to a lower address, which `ptr::copy` makes whole however they
    // overlap.
    let mut end = 0usize;
    for (n, &(len, flags)) in messages.got[..got].iter().enumerate() {
        let Some(peer) = read_addr(&messages.names[n]) else { continue };
        let said = decode(&messages.msgs[n]);
        let len = len.min(size);
        // SAFETY: the kernel wrote `len` bytes at slot `n`; `end` is at most its start.
        unsafe {
            ptr::copy(base.add(n * size), base.add(end), len);
        }
        end += len;
        let len = len as u32;
        batch.heads.push(Head {
            peer,
            local: said.local,
            ecn: said.ecn.unwrap_or(Ecn::NotEct),
            len,
            segment: said.segment.filter(|&it| it > 0 && it < len).unwrap_or(len),
            truncated: flags & libc::MSG_TRUNC != 0,
        });
    }
    // SAFETY: the first `end` bytes were written by the kernel and moved down above.
    unsafe { batch.bytes.set_len(end) };
    Ok(batch.heads.len())
}

/// The kernel's receive into `messages`' first `most`: each message's length and flags.
#[cfg(target_os = "linux")]
fn recv_into(
    socket: &UdpSocket,
    messages: &mut Messages,
    most: usize,
) -> io::Result<usize> {
    // SAFETY: as in `send_into`.
    let mut mmsgs: [libc::mmsghdr; MESSAGES] = unsafe { mem::zeroed() };
    for (to, from) in mmsgs.iter_mut().zip(&messages.msgs[..most]) {
        // A copy of a header whose pointers point into `messages` and the batch.
        to.msg_hdr = *from;
    }
    loop {
        // SAFETY: `most` headers, each pointing at its slot of the batch's reserved bytes and
        // at its address and control buffers in `messages`, alive and unmoved across the call.
        let got = unsafe {
            libc::recvmmsg(
                socket.as_raw_fd(),
                mmsgs.as_mut_ptr(),
                most as _,
                0 as _,
                ptr::null_mut(),
            )
        };
        match got {
            n if n >= 0 => {
                let n = n as usize;
                for (at, mmsg) in mmsgs[..n].iter().enumerate() {
                    // The kernel's view of the header: its address and control lengths.
                    messages.msgs[at] = mmsg.msg_hdr;
                    messages.got[at] = (mmsg.msg_len as usize, mmsg.msg_hdr.msg_flags);
                }
                return Ok(n);
            }
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn recv_into(
    socket: &UdpSocket,
    messages: &mut Messages,
    most: usize,
) -> io::Result<usize> {
    let mut xs = [MsghdrX {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: ptr::null_mut(),
        msg_iovlen: 0,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
        msg_datalen: 0,
    }; MESSAGES];
    for (to, from) in xs.iter_mut().zip(&messages.msgs[..most]) {
        *to = to_x(from);
    }
    loop {
        // SAFETY: as Linux's `recvmmsg`.
        let got =
            unsafe { recvmsg_x(socket.as_raw_fd(), xs.as_ptr(), most as libc::c_uint, 0) };
        match got {
            n if n >= 0 => {
                let n = n as usize;
                for (at, x) in xs[..n].iter().enumerate() {
                    let msg = &mut messages.msgs[at];
                    msg.msg_namelen = x.msg_namelen;
                    msg.msg_controllen = x.msg_controllen;
                    msg.msg_flags = x.msg_flags;
                    messages.got[at] = (x.msg_datalen, x.msg_flags);
                }
                return Ok(n);
            }
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The private calls this module makes on macOS are in libSystem (rule macos-private-apis):
    /// a system that drops them fails here, by name.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_private_batch_calls_are_there() {
        for name in [c"sendmsg_x", c"recvmsg_x"] {
            // SAFETY: a lookup of a NUL-terminated name in the process's own images.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(!found.is_null(), "{name:?} is not in libSystem");
        }
    }

    /// A batch of runs sent on loopback arrives as its datagrams, each with its bytes, its
    /// codepoint and the address it arrived at.
    #[test]
    fn a_batch_crosses_loopback_with_its_codepoints_and_addresses() {
        let at: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let options = Options { ecn: true, local_address: true, ..Options::default() };
        let (send_sock, recv_sock) = (bind(at, options).unwrap(), bind(at, options).unwrap());
        let to = recv_sock.local_addr().unwrap();
        let mut batch = Batch::new();
        for n in 0..8u8 {
            batch.push(to, Ecn::Ect0, &[n; 100]);
        }
        let mut at = At::default();
        while !at.done(&batch) {
            at = send(&send_sock, false, &batch, at).unwrap();
        }
        let mut got = Batch::new();
        let mut seen = 0;
        for _ in 0..1000 {
            match recv(&recv_sock, &mut got, Slots { most: 8, size: 2048 }) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                    continue;
                }
                Err(error) => panic!("receiving on {to}: {error}"),
            }
            for (head, bytes) in got.each() {
                assert_eq!(bytes, [seen as u8; 100]);
                assert_eq!(head.ecn, Ecn::Ect0, "the codepoint sent");
                assert_eq!(head.local, Some(to.ip()), "the address it arrived at");
                assert!(!head.truncated);
                seen += 1;
            }
            if seen == 8 {
                return;
            }
        }
        panic!("{seen} of 8 datagrams arrived");
    }
}
