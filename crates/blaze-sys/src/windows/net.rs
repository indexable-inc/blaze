//! `Win32::Networking::WinSock`: the socket calls, addresses and constants the IOCP driver's
//! sockets use, with mswsock's `AcceptEx` and `ConnectEx`. `SOCKET` and `INVALID_SOCKET` are
//! the root's ([`Socket`], [`Socket::INVALID`]).

use core::ffi::c_void;

use super::{Bool, GUID, PCSTR, Socket, WinError, io::OVERLAPPED};

/// `ADDRESS_FAMILY` (ws2def.h).
///
/// replaces windows_sys::Win32::Networking::WinSock::ADDRESS_FAMILY
pub type ADDRESS_FAMILY = u16;
/// `IPPROTO` (ws2def.h).
///
/// replaces windows_sys::Win32::Networking::WinSock::IPPROTO
pub type IPPROTO = i32;
/// `WINSOCK_SOCKET_TYPE`: `SOCK_*`.
///
/// replaces windows_sys::Win32::Networking::WinSock::WINSOCK_SOCKET_TYPE
pub type WINSOCK_SOCKET_TYPE = i32;
/// `WINSOCK_SHUTDOWN_HOW`: `SD_*`.
///
/// replaces windows_sys::Win32::Networking::WinSock::WINSOCK_SHUTDOWN_HOW
pub type WINSOCK_SHUTDOWN_HOW = i32;
/// `WSA_ERROR`: a Winsock error code (winerror.h's `WSAE*`).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSA_ERROR
pub type WSA_ERROR = i32;
/// `SEND_RECV_FLAGS`: `MSG_*`.
///
/// replaces windows_sys::Win32::Networking::WinSock::SEND_RECV_FLAGS
pub type SEND_RECV_FLAGS = i32;

/// `LPWSAOVERLAPPED_COMPLETION_ROUTINE` (winsock2.h): an APC run on completion; the driver
/// passes `None` and takes completions from the port.
///
/// replaces windows_sys::Win32::Networking::WinSock::LPWSAOVERLAPPED_COMPLETION_ROUTINE
pub type LPWSAOVERLAPPED_COMPLETION_ROUTINE = Option<
    unsafe extern "system" fn(dwerror: u32, cbtransferred: u32, lpoverlapped: *mut OVERLAPPED, dwflags: u32),
>;
/// `LPFN_CONNECTEX` (mswsock.h): `ConnectEx`, which Winsock hands out per socket through
/// `WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER, WSAID_CONNECTEX)`.
///
/// replaces windows_sys::Win32::Networking::WinSock::LPFN_CONNECTEX
pub type LPFN_CONNECTEX = Option<
    unsafe extern "system" fn(
        s: Socket,
        name: *const SOCKADDR,
        namelen: i32,
        lpsendbuffer: *const c_void,
        dwsenddatalength: u32,
        lpdwbytessent: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
    ) -> Bool,
>;

/// `AF_UNIX` (1).
///
/// replaces windows_sys::Win32::Networking::WinSock::AF_UNIX
pub const AF_UNIX: u16 = 1u16;
/// `AF_INET` (2).
///
/// replaces windows_sys::Win32::Networking::WinSock::AF_INET
pub const AF_INET: ADDRESS_FAMILY = 2u16;
/// `AF_INET6` (23).
///
/// replaces windows_sys::Win32::Networking::WinSock::AF_INET6
pub const AF_INET6: ADDRESS_FAMILY = 23u16;
/// `IPPROTO_TCP` (6).
///
/// replaces windows_sys::Win32::Networking::WinSock::IPPROTO_TCP
pub const IPPROTO_TCP: IPPROTO = 6i32;
/// `SOCK_STREAM` (1).
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCK_STREAM
pub const SOCK_STREAM: WINSOCK_SOCKET_TYPE = 1i32;
/// `SD_BOTH` (2): shut both directions.
///
/// replaces windows_sys::Win32::Networking::WinSock::SD_BOTH
pub const SD_BOTH: WINSOCK_SHUTDOWN_HOW = 2i32;
/// `SOL_SOCKET` (`0xFFFF`).
///
/// replaces windows_sys::Win32::Networking::WinSock::SOL_SOCKET
pub const SOL_SOCKET: i32 = 65535i32;
/// `SO_UPDATE_ACCEPT_CONTEXT` (`0x700B`): gives an `AcceptEx` socket the listener's state.
///
/// replaces windows_sys::Win32::Networking::WinSock::SO_UPDATE_ACCEPT_CONTEXT
pub const SO_UPDATE_ACCEPT_CONTEXT: i32 = 28683i32;
/// `SO_UPDATE_CONNECT_CONTEXT` (`0x7010`): makes a `ConnectEx` socket a connected one.
///
/// replaces windows_sys::Win32::Networking::WinSock::SO_UPDATE_CONNECT_CONTEXT
pub const SO_UPDATE_CONNECT_CONTEXT: i32 = 28688i32;
/// `TCP_NODELAY` (1).
///
/// replaces windows_sys::Win32::Networking::WinSock::TCP_NODELAY
pub const TCP_NODELAY: i32 = 1i32;
/// `SIO_GET_EXTENSION_FUNCTION_POINTER` (`0xC8000006`).
///
/// replaces windows_sys::Win32::Networking::WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER
pub const SIO_GET_EXTENSION_FUNCTION_POINTER: u32 = 3355443206u32;
/// `WSA_FLAG_OVERLAPPED` (1).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSA_FLAG_OVERLAPPED
pub const WSA_FLAG_OVERLAPPED: u32 = 1u32;
/// `WSA_FLAG_NO_HANDLE_INHERIT` (`0x80`).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSA_FLAG_NO_HANDLE_INHERIT
pub const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 128u32;
/// `WSA_IO_PENDING` (997): an overlapped call was queued.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSA_IO_PENDING
pub const WSA_IO_PENDING: WSA_ERROR = 997i32;
/// `WSAECONNRESET` (10054).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAECONNRESET
pub const WSAECONNRESET: WSA_ERROR = 10054i32;
/// `WSAENOTCONN` (10057).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAENOTCONN
pub const WSAENOTCONN: WSA_ERROR = 10057i32;
/// `WSAVERNOTSUPPORTED` (10092): the Winsock version asked for is not there.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAVERNOTSUPPORTED
pub const WSAVERNOTSUPPORTED: WSA_ERROR = 10092i32;
/// `WSAID_CONNECTEX` (mswsock.h): `{25a207b9-ddf3-4660-8ee9-76e58c74063e}`.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAID_CONNECTEX
pub const WSAID_CONNECTEX: GUID = GUID::from_u128(0x25a207b9_ddf3_4660_8ee9_76e58c74063e);
/// `WSADESCRIPTION_LEN` (256).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSADESCRIPTION_LEN
pub const WSADESCRIPTION_LEN: u32 = 256u32;
/// `WSASYS_STATUS_LEN` (128).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSASYS_STATUS_LEN
pub const WSASYS_STATUS_LEN: u32 = 128u32;
/// `MAX_PROTOCOL_CHAIN` (7).
///
/// replaces windows_sys::Win32::Networking::WinSock::MAX_PROTOCOL_CHAIN
pub const MAX_PROTOCOL_CHAIN: u32 = 7u32;
/// `WSAPROTOCOL_LEN` (255).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAPROTOCOL_LEN
pub const WSAPROTOCOL_LEN: u32 = 255u32;

/// `SOCKADDR` (ws2def.h): the family and up to 14 bytes; what a call takes by pointer.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SOCKADDR {
    pub sa_family: ADDRESS_FAMILY,
    pub sa_data: [i8; 14],
}

/// `IN_ADDR` (inaddr.h): an IPv4 address, network order.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN_ADDR
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IN_ADDR {
    pub S_un: IN_ADDR_0,
}

/// `IN_ADDR`'s `S_un` union.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN_ADDR_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union IN_ADDR_0 {
    pub S_un_b: IN_ADDR_0_0,
    pub S_un_w: IN_ADDR_0_1,
    pub S_addr: u32,
}

/// `IN_ADDR`'s four bytes.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN_ADDR_0_0
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IN_ADDR_0_0 {
    pub s_b1: u8,
    pub s_b2: u8,
    pub s_b3: u8,
    pub s_b4: u8,
}

/// `IN_ADDR`'s two words.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN_ADDR_0_1
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IN_ADDR_0_1 {
    pub s_w1: u16,
    pub s_w2: u16,
}

/// `IN6_ADDR` (in6addr.h): an IPv6 address.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN6_ADDR
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IN6_ADDR {
    pub u: IN6_ADDR_0,
}

/// `IN6_ADDR`'s `u` union.
///
/// replaces windows_sys::Win32::Networking::WinSock::IN6_ADDR_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union IN6_ADDR_0 {
    pub Byte: [u8; 16],
    pub Word: [u16; 8],
}

/// `SOCKADDR_IN` (ws2def.h): an IPv4 address and port, network order.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR_IN
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SOCKADDR_IN {
    pub sin_family: ADDRESS_FAMILY,
    pub sin_port: u16,
    pub sin_addr: IN_ADDR,
    pub sin_zero: [i8; 8],
}

/// `SCOPE_ID` (ws2def.h): an IPv6 scope, a zone and a level in one `ULONG`.
///
/// replaces windows_sys::Win32::Networking::WinSock::SCOPE_ID
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SCOPE_ID {
    pub Anonymous: SCOPE_ID_0,
}

/// `SCOPE_ID`'s union.
///
/// replaces windows_sys::Win32::Networking::WinSock::SCOPE_ID_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union SCOPE_ID_0 {
    pub Anonymous: SCOPE_ID_0_0,
    pub Value: u32,
}

/// `SCOPE_ID`'s bit fields (`Zone : 28`, `Level : 4`).
///
/// replaces windows_sys::Win32::Networking::WinSock::SCOPE_ID_0_0
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SCOPE_ID_0_0 {
    pub _bitfield: u32,
}

/// `SOCKADDR_IN6` (ws2ipdef.h): an IPv6 address, port, flow and scope.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SOCKADDR_IN6 {
    pub sin6_family: ADDRESS_FAMILY,
    pub sin6_port: u16,
    pub sin6_flowinfo: u32,
    pub sin6_addr: IN6_ADDR,
    pub Anonymous: SOCKADDR_IN6_0,
}

/// `SOCKADDR_IN6`'s scope union.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR_IN6_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union SOCKADDR_IN6_0 {
    pub sin6_scope_id: u32,
    pub sin6_scope_struct: SCOPE_ID,
}

/// `SOCKADDR_STORAGE` (ws2def.h): room for any address, 128 bytes.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SOCKADDR_STORAGE {
    pub ss_family: ADDRESS_FAMILY,
    pub __ss_pad1: [i8; 6],
    pub __ss_align: i64,
    pub __ss_pad2: [i8; 112],
}

/// `SOCKADDR_UN` (afunix.h): a Unix-domain socket's path, NUL-terminated, at most 108 bytes.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKADDR_UN
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SOCKADDR_UN {
    pub sun_family: ADDRESS_FAMILY,
    pub sun_path: [i8; 108],
}

/// `WSABUF` (ws2def.h): one buffer of a `WSASend`/`WSARecv`.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSABUF
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WSABUF {
    pub len: u32,
    /// windows-sys's `PSTR`, which the root does not declare.
    pub buf: *mut u8,
}

/// `WSADATA` (winsock2.h): what `WSAStartup` fills. On 64-bit targets the header puts the
/// two `unsigned short`s and the pointer before the strings, unlike 32-bit x86.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSADATA
#[cfg(any(target_arch = "aarch64", target_arch = "arm64ec", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WSADATA {
    pub wVersion: u16,
    pub wHighVersion: u16,
    pub iMaxSockets: u16,
    pub iMaxUdpDg: u16,
    /// windows-sys's `PSTR`, which the root does not declare.
    pub lpVendorInfo: *mut u8,
    pub szDescription: [i8; 257],
    pub szSystemStatus: [i8; 129],
}

/// `WSADATA` (winsock2.h), the 32-bit x86 order.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSADATA
#[cfg(target_arch = "x86")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WSADATA {
    pub wVersion: u16,
    pub wHighVersion: u16,
    pub szDescription: [i8; 257],
    pub szSystemStatus: [i8; 129],
    pub iMaxSockets: u16,
    pub iMaxUdpDg: u16,
    /// windows-sys's `PSTR`, which the root does not declare.
    pub lpVendorInfo: *mut u8,
}

/// `WSAPROTOCOLCHAIN` (winsock2.h).
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAPROTOCOLCHAIN
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WSAPROTOCOLCHAIN {
    pub ChainLen: i32,
    pub ChainEntries: [u32; 7],
}

/// `WSAPROTOCOL_INFOW` (winsock2.h): a provider's description; `WSASocketW` takes it or null.
///
/// replaces windows_sys::Win32::Networking::WinSock::WSAPROTOCOL_INFOW
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WSAPROTOCOL_INFOW {
    pub dwServiceFlags1: u32,
    pub dwServiceFlags2: u32,
    pub dwServiceFlags3: u32,
    pub dwServiceFlags4: u32,
    pub dwProviderFlags: u32,
    pub ProviderId: GUID,
    pub dwCatalogEntryId: u32,
    pub ProtocolChain: WSAPROTOCOLCHAIN,
    pub iVersion: i32,
    pub iAddressFamily: i32,
    pub iMaxSockAddr: i32,
    pub iMinSockAddr: i32,
    pub iSocketType: i32,
    pub iProtocol: i32,
    pub iProtocolMaxOffset: i32,
    pub iNetworkByteOrder: i32,
    pub iSecurityScheme: i32,
    pub dwMessageSize: u32,
    pub dwProviderReserved: u32,
    pub szProtocol: [u16; 256],
}

// ws2def.h, inaddr.h, in6addr.h, ws2ipdef.h, afunix.h: the same on every Windows target.
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<SOCKADDR>() == 16 && align_of::<SOCKADDR>() == 2);
    assert!(offset_of!(SOCKADDR, sa_data) == 2);
    assert!(size_of::<IN_ADDR>() == 4 && align_of::<IN_ADDR>() == 4);
    assert!(size_of::<IN_ADDR_0_0>() == 4 && size_of::<IN_ADDR_0_1>() == 4);
    assert!(size_of::<IN6_ADDR>() == 16 && align_of::<IN6_ADDR>() == 2);
    assert!(size_of::<SOCKADDR_IN>() == 16 && align_of::<SOCKADDR_IN>() == 4);
    assert!(offset_of!(SOCKADDR_IN, sin_family) == 0);
    assert!(offset_of!(SOCKADDR_IN, sin_port) == 2);
    assert!(offset_of!(SOCKADDR_IN, sin_addr) == 4);
    assert!(offset_of!(SOCKADDR_IN, sin_zero) == 8);
    assert!(size_of::<SCOPE_ID>() == 4 && align_of::<SCOPE_ID>() == 4);
    assert!(size_of::<SOCKADDR_IN6>() == 28 && align_of::<SOCKADDR_IN6>() == 4);
    assert!(offset_of!(SOCKADDR_IN6, sin6_port) == 2);
    assert!(offset_of!(SOCKADDR_IN6, sin6_flowinfo) == 4);
    assert!(offset_of!(SOCKADDR_IN6, sin6_addr) == 8);
    assert!(offset_of!(SOCKADDR_IN6, Anonymous) == 24);
    assert!(size_of::<SOCKADDR_STORAGE>() == 128 && align_of::<SOCKADDR_STORAGE>() == 8);
    assert!(offset_of!(SOCKADDR_STORAGE, ss_family) == 0);
    assert!(size_of::<SOCKADDR_UN>() == 110 && align_of::<SOCKADDR_UN>() == 2);
    assert!(offset_of!(SOCKADDR_UN, sun_path) == 2);
    assert!(size_of::<WSAPROTOCOLCHAIN>() == 32);
    assert!(size_of::<WSAPROTOCOL_INFOW>() == 628 && align_of::<WSAPROTOCOL_INFOW>() == 4);
    assert!(offset_of!(WSAPROTOCOL_INFOW, ProviderId) == 20);
    assert!(offset_of!(WSAPROTOCOL_INFOW, ProtocolChain) == 40);
    assert!(offset_of!(WSAPROTOCOL_INFOW, szProtocol) == 116);
};

// winsock2.h and ws2def.h, x86-64 and arm64 Windows (LLP64, 8-byte pointers): identical.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<WSABUF>() == 16 && align_of::<WSABUF>() == 8);
    assert!(offset_of!(WSABUF, len) == 0);
    assert!(offset_of!(WSABUF, buf) == 8);
    assert!(size_of::<WSADATA>() == 408 && align_of::<WSADATA>() == 8);
    assert!(offset_of!(WSADATA, wVersion) == 0);
    assert!(offset_of!(WSADATA, wHighVersion) == 2);
    assert!(offset_of!(WSADATA, lpVendorInfo) == 8);
    assert!(offset_of!(WSADATA, szDescription) == 16);
    assert!(offset_of!(WSADATA, szSystemStatus) == 273);
};

#[link(name = "ws2_32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `WSAGetLastError` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSAGetLastError
    pub fn WSAGetLastError() -> WSA_ERROR;
    /// `WSAStartup` (winsock2.h): returns the error itself, not through `WSAGetLastError`.
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSAStartup
    pub fn WSAStartup(wversionrequested: u16, lpwsadata: *mut WSADATA) -> i32;
    /// `WSASocketW` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSASocketW
    pub fn WSASocketW(
        af: i32,
        r#type: i32,
        protocol: i32,
        lpprotocolinfo: *const WSAPROTOCOL_INFOW,
        g: u32,
        dwflags: u32,
    ) -> Socket;
    /// `WSAIoctl` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSAIoctl
    pub fn WSAIoctl(
        s: Socket,
        dwiocontrolcode: u32,
        lpvinbuffer: *const c_void,
        cbinbuffer: u32,
        lpvoutbuffer: *mut c_void,
        cboutbuffer: u32,
        lpcbbytesreturned: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
        lpcompletionroutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    /// `WSARecv` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSARecv
    pub fn WSARecv(
        s: Socket,
        lpbuffers: *const WSABUF,
        dwbuffercount: u32,
        lpnumberofbytesrecvd: *mut u32,
        lpflags: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
        lpcompletionroutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    /// `WSASend` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::WSASend
    pub fn WSASend(
        s: Socket,
        lpbuffers: *const WSABUF,
        dwbuffercount: u32,
        lpnumberofbytessent: *mut u32,
        dwflags: u32,
        lpoverlapped: *mut OVERLAPPED,
        lpcompletionroutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;
    /// `bind` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::bind
    pub fn bind(s: Socket, name: *const SOCKADDR, namelen: i32) -> i32;
    /// `connect` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::connect
    pub fn connect(s: Socket, name: *const SOCKADDR, namelen: i32) -> i32;
    /// `getsockname` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::getsockname
    pub fn getsockname(s: Socket, name: *mut SOCKADDR, namelen: *mut i32) -> i32;
    /// `listen` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::listen
    pub fn listen(s: Socket, backlog: i32) -> i32;
    /// `setsockopt` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::setsockopt
    pub fn setsockopt(s: Socket, level: i32, optname: i32, optval: PCSTR, optlen: i32) -> i32;
    /// `shutdown` (winsock2.h).
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::shutdown
    pub fn shutdown(s: Socket, how: WINSOCK_SHUTDOWN_HOW) -> i32;
}

#[link(name = "mswsock", kind = "raw-dylib")]
unsafe extern "system" {
    /// `AcceptEx` (mswsock.h): an overlapped accept into a socket made beforehand.
    ///
    /// replaces windows_sys::Win32::Networking::WinSock::AcceptEx
    pub fn AcceptEx(
        slistensocket: Socket,
        sacceptsocket: Socket,
        lpoutputbuffer: *mut c_void,
        dwreceivedatalength: u32,
        dwlocaladdresslength: u32,
        dwremoteaddresslength: u32,
        lpdwbytesreceived: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
    ) -> Bool;
}

/// Winsock 2.2, the version [`wsa_startup`] asks for (`MAKEWORD(2, 2)`).
const WINSOCK_2_2: u16 = 0x0202;

/// `WSAStartup` for Winsock 2.2: what it reported (`WSADATA`), or its error. A process may
/// start Winsock more than once; each start is counted and none is undone here (blaze never
/// calls `WSACleanup`: the process's exit ends Winsock). [`WSAVERNOTSUPPORTED`] when the
/// system gave another version.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wsa_startup() -> Result<WSADATA, WinError> {
    // SAFETY: `WSADATA` is integers, byte arrays and a raw pointer: all zeros is a valid value,
    // and the call overwrites it.
    let mut data: WSADATA = unsafe { core::mem::zeroed() };
    // SAFETY: a version number and a live `WSADATA` the call writes; it keeps no pointer.
    let code = unsafe { WSAStartup(WINSOCK_2_2, &raw mut data) };
    if code != 0 {
        return Err(WinError(code as u32));
    }
    if data.wVersion != WINSOCK_2_2 {
        return Err(WinError(WSAVERNOTSUPPORTED as u32));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_ex_s_id_is_mswsock_h_s() {
        assert_eq!(WSAID_CONNECTEX, GUID::from_u128(0x25a207b9_ddf3_4660_8ee9_76e58c74063e));
        assert_eq!(SIO_GET_EXTENSION_FUNCTION_POINTER, 0xC800_0006);
    }
}
