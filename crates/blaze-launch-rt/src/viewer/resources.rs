//! What `GET /api/resources?pid=<pid>` answers: a process's open descriptors, its IP sockets
//! and the regular files it holds open, as the one JSON object the Explore page reads
//! (`site/explore/wasm/src/model.rs`, `ingest_resources_json`):
//!
//! ```text
//! {"pid":P,"fd_count":N,"sockets":[{"proto":"TCP","local":"127.0.0.1:41937","remote":"",
//!   "state":"LISTEN","port":41937},...],"files":["/path",...]}
//! ```
//!
//! The shape and every field's text are what the viewer made of `lsof -nP -a -p <pid> -F ftPnT`
//! before it read the kernel itself: an end is `addr:port`, `*` for a wildcard address or a
//! zero port, a v6 address in brackets as `inet_ntop` writes it; `remote` is empty for an
//! unconnected socket; a socket with no TCP state (UDP) is `LISTEN` unconnected and
//! `ESTABLISHED` connected; `port` is the local port (0 for `*`); at most [`MOST_SOCKETS`]
//! sockets and [`MOST_FILES`] files, in descriptor order. The tests hold the bytes to the old
//! parser's for the same facts.
//!
//! This is the core: the facts as types, the JSON writer, the typed refusal, and the Linux
//! `/proc` readers over bytes. The calls that gather the facts are the driver's (`viewer.rs`:
//! libproc on macOS, `/proc` on Linux).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc};

/// The most sockets an answer lists (the page draws at most 48).
pub const MOST_SOCKETS: usize = 48;

/// The most file paths an answer lists.
pub const MOST_FILES: usize = 24;

/// A socket's transport.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    /// lsof's name for it, the `proto` field.
    pub fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "TCP",
            Proto::Udp => "UDP",
        }
    }
}

/// An IP address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ip {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl Ip {
    /// The wildcard (all zeros), which lsof writes `*`.
    pub fn is_unspecified(self) -> bool {
        match self {
            Ip::V4(bytes) => bytes == [0; 4],
            Ip::V6(bytes) => bytes == [0; 16],
        }
    }
}

/// One end of a socket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct End {
    pub ip: Ip,
    pub port: u16,
}

impl End {
    /// No address and no port: a remote end of a socket that is not connected.
    pub fn is_unset(self) -> bool {
        self.port == 0 && self.ip.is_unspecified()
    }
}

/// A TCP connection's state, by the names of netinet/tcp_fsm.h's `tcpstates` (lsof's `ST=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    CloseWait,
    FinWait1,
    Closing,
    LastAck,
    FinWait2,
    TimeWait,
}

impl TcpState {
    /// Darwin's `tcpsi_state` (`TCPS_*`, 0 `CLOSED` to 10 `TIME_WAIT`).
    #[cfg(any(test, target_os = "macos"))]
    pub fn from_darwin(state: i32) -> Option<TcpState> {
        Some(match state {
            0 => TcpState::Closed,
            1 => TcpState::Listen,
            2 => TcpState::SynSent,
            3 => TcpState::SynReceived,
            4 => TcpState::Established,
            5 => TcpState::CloseWait,
            6 => TcpState::FinWait1,
            7 => TcpState::Closing,
            8 => TcpState::LastAck,
            9 => TcpState::FinWait2,
            10 => TcpState::TimeWait,
            _ => return None,
        })
    }

    /// Linux's `st` column of /proc/net/tcp (include/net/tcp_states.h, 1 `ESTABLISHED` to 11
    /// `CLOSING`).
    #[cfg(any(test, target_os = "linux"))]
    pub fn from_linux(state: u8) -> Option<TcpState> {
        Some(match state {
            1 => TcpState::Established,
            2 => TcpState::SynSent,
            3 => TcpState::SynReceived,
            4 => TcpState::FinWait1,
            5 => TcpState::FinWait2,
            6 => TcpState::TimeWait,
            7 => TcpState::Closed,
            8 => TcpState::CloseWait,
            9 => TcpState::LastAck,
            10 => TcpState::Listen,
            11 => TcpState::Closing,
            _ => return None,
        })
    }

    /// Its name, the `state` field.
    pub fn name(self) -> &'static str {
        match self {
            TcpState::Closed => "CLOSED",
            TcpState::Listen => "LISTEN",
            TcpState::SynSent => "SYN_SENT",
            TcpState::SynReceived => "SYN_RCVD",
            TcpState::Established => "ESTABLISHED",
            TcpState::CloseWait => "CLOSE_WAIT",
            TcpState::FinWait1 => "FIN_WAIT_1",
            TcpState::Closing => "CLOSING",
            TcpState::LastAck => "LAST_ACK",
            TcpState::FinWait2 => "FIN_WAIT_2",
            TcpState::TimeWait => "TIME_WAIT",
        }
    }
}

/// One IP socket of the process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Socket {
    pub proto: Proto,
    pub local: End,
    pub remote: End,
    /// The TCP state; `None` for UDP.
    pub state: Option<TcpState>,
}

impl Socket {
    /// The `state` field: the TCP state's name, or for a socket with none `LISTEN` when it is
    /// not connected and `ESTABLISHED` when it is.
    pub fn state_name(&self) -> &'static str {
        match self.state {
            Some(state) => state.name(),
            None if self.remote.is_unset() => "LISTEN",
            None => "ESTABLISHED",
        }
    }
}

/// A process's descriptors as the page reads them.
pub struct Resources<'a> {
    pub pid: u32,
    /// Every open descriptor.
    pub fd_count: u32,
    pub sockets: AVec<'a, Socket>,
    /// The paths of the regular files open, as the kernel gave them (bytes; written lossily
    /// as UTF-8).
    pub files: AVec<'a, AVec<'a, u8>>,
    alloc: Alloc<'a>,
}

impl<'a> Resources<'a> {
    /// No descriptor yet, of `pid` (0: the answer for no process, all empty).
    pub fn new_in(pid: u32, alloc: Alloc<'a>) -> Resources<'a> {
        Resources { pid, fd_count: 0, sockets: AVec::new_in(alloc), files: AVec::new_in(alloc), alloc }
    }

    /// Whether another socket would be listed.
    pub fn wants_socket(&self) -> bool {
        self.sockets.len() < MOST_SOCKETS
    }

    /// Whether another file would be listed.
    pub fn wants_file(&self) -> bool {
        self.files.len() < MOST_FILES
    }

    /// `socket` listed, if under [`MOST_SOCKETS`].
    pub fn push_socket(&mut self, socket: Socket) {
        if self.wants_socket() {
            self.sockets.push(socket);
        }
    }

    /// `path` listed, if non-empty and under [`MOST_FILES`].
    pub fn push_file(&mut self, path: &[u8]) {
        if self.wants_file() && !path.is_empty() {
            self.files.push(AVec::from_slice_in(path, self.alloc));
        }
    }

    /// The answer, appended to `out`.
    pub fn write_json(&self, out: &mut AString<'_>) {
        let _ = write!(out, r#"{{"pid":{},"fd_count":{},"sockets":["#, self.pid, self.fd_count);
        for (at, socket) in self.sockets.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            out.push_str(r#"{"proto":""#);
            out.push_str(socket.proto.name());
            out.push_str(r#"","local":""#);
            write_end(out, socket.local);
            out.push_str(r#"","remote":""#);
            if !socket.remote.is_unset() {
                write_end(out, socket.remote);
            }
            out.push_str(r#"","state":""#);
            out.push_str(socket.state_name());
            let _ = write!(out, r#"","port":{}}}"#, socket.local.port);
        }
        out.push_str(r#"],"files":["#);
        for (at, file) in self.files.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            out.push('"');
            push_json_bytes(out, file);
            out.push('"');
        }
        out.push_str("]}");
    }

    /// One `/proc/<pid>/fd/<n>` link's target counted (Linux): a socket listed from
    /// `sockets` (the process's IP sockets by inode, sorted by inode; a socket of another kind
    /// is in none of the tables and is counted only), an absolute path listed as a file
    /// (`/dev/...` too: the link says no more), anything else (`pipe:[..]`, `anon_inode:..`)
    /// counted only.
    #[cfg(any(test, target_os = "linux"))]
    pub fn add_linux_fd(&mut self, target: &[u8], sockets: &[(u64, Socket)]) {
        self.fd_count = self.fd_count.saturating_add(1);
        match fd_link(target) {
            FdLink::Socket(inode) => {
                if let Ok(at) = sockets.binary_search_by_key(&inode, |&(of, _)| of)
                    && let Some(&(_, socket)) = sockets.get(at)
                {
                    self.push_socket(socket);
                }
            }
            FdLink::Path(path) => self.push_file(path),
            FdLink::Other => {}
        }
    }
}

/// `addr:port` as lsof writes it: `*` for a wildcard address or a zero port, a v6 address in
/// brackets.
fn write_end(out: &mut AString<'_>, end: End) {
    if end.ip.is_unspecified() {
        out.push('*');
    } else {
        match end.ip {
            Ip::V4(bytes) => write_v4(out, bytes),
            Ip::V6(bytes) => {
                out.push('[');
                write_v6(out, bytes);
                out.push(']');
            }
        }
    }
    out.push(':');
    if end.port == 0 {
        out.push('*');
    } else {
        let _ = write!(out, "{}", end.port);
    }
}

fn write_v4(out: &mut AString<'_>, [a, b, c, d]: [u8; 4]) {
    let _ = write!(out, "{a}.{b}.{c}.{d}");
}

/// A v6 address as BSD's `inet_ntop` writes it (RFC 5952): lowercase hex words, the longest
/// run of two or more zero words (the first of equals) as `::`, and a v4-mapped
/// (`::ffff:a.b.c.d`) or v4-compatible (`::a.b.c.d`) address with its last four bytes dotted.
fn write_v6(out: &mut AString<'_>, bytes: [u8; 16]) {
    let mut words = [0u16; 8];
    for (word, pair) in words.iter_mut().zip(bytes.chunks_exact(2)) {
        if let [high, low] = *pair {
            *word = u16::from_be_bytes([high, low]);
        }
    }
    let (mut best_at, mut best_len) = (0usize, 0usize);
    let mut at = 0;
    while at < words.len() {
        let zeros = words.iter().skip(at).take_while(|&&word| word == 0).count();
        if zeros > best_len {
            (best_at, best_len) = (at, zeros);
        }
        at += zeros.max(1);
    }
    if best_len < 2 {
        best_len = 0;
    }
    let mapped = best_at == 0 && (best_len == 6 || (best_len == 5 && words.get(5) == Some(&0xffff)));
    for (at, word) in words.iter().enumerate() {
        if best_len > 0 && at >= best_at && at < best_at + best_len {
            if at == best_at {
                out.push(':');
            }
            continue;
        }
        if at != 0 {
            out.push(':');
        }
        if at == 6 && mapped {
            if let [.., a, b, c, d] = bytes {
                write_v4(out, [a, b, c, d]);
            }
            return;
        }
        let _ = write!(out, "{word:x}");
    }
    if best_len > 0 && best_at + best_len == words.len() {
        out.push(':');
    }
}

/// `text` as the inside of a JSON string: `"`, `\` and the control characters escaped as the
/// viewer has always written them.
pub fn push_json_text(out: &mut AString<'_>, text: &str) {
    for ch in text.chars() {
        match ch {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            ch if u32::from(ch) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
}

/// `bytes` as the inside of a JSON string, read as UTF-8 with each invalid sequence one
/// U+FFFD (what `String::from_utf8_lossy` makes).
pub fn push_json_bytes(out: &mut AString<'_>, bytes: &[u8]) {
    for chunk in bytes.utf8_chunks() {
        push_json_text(out, chunk.valid());
        if !chunk.invalid().is_empty() {
            out.push('\u{FFFD}');
        }
    }
}

/// Why the facts could not be read: the call that refused, of which process, and its errno.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResourcesError {
    /// The call, as `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` or `readlink(/proc/<pid>/fd/<n>)`.
    pub call: &'static str,
    pub pid: u32,
    pub errno: u16,
    /// The errno's C name (`ESRCH`).
    pub name: &'static str,
    pub refused: Refused,
}

/// What kind of refusal, for the HTTP status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The query was malformed.
    BadQuery,
    /// No such process (it exited).
    Gone,
    /// The process is another user's.
    Denied,
    /// Any other errno.
    Failed,
}

impl ResourcesError {
    /// The HTTP status it is answered with: a non-2xx the page keeps its last answer on.
    pub fn status(&self) -> &'static str {
        match self.refused {
            Refused::BadQuery => "400 Bad Request",
            Refused::Gone => "404 Not Found",
            Refused::Denied => "403 Forbidden",
            Refused::Failed => "500 Internal Server Error",
        }
    }

    /// `{"error":{"call":"..","pid":P,"errno":E,"name":".."}}`, appended to `out`.
    pub fn write_json(&self, out: &mut AString<'_>) {
        out.push_str(r#"{"error":{"call":""#);
        push_json_text(out, self.call);
        let _ = write!(out, r#"","pid":{},"errno":{},"name":""#, self.pid, self.errno);
        push_json_text(out, self.name);
        out.push_str(r#""}}"#);
    }
}

/// The `pid` of a `/api/resources` query: 0 when the query names none (the answer for no
/// process); a refusal for a `pid` that is no number.
pub fn query_pid(query: &str) -> Result<u32, ResourcesError> {
    let mut pid = 0;
    for pair in query.split('&') {
        if let Some(("pid", value)) = pair.split_once('=') {
            pid = value.parse::<u32>().map_err(|_| ResourcesError {
                call: "the query's pid",
                pid: 0,
                errno: 22,
                name: "EINVAL",
                refused: Refused::BadQuery,
            })?;
        }
    }
    Ok(pid)
}

/// What a `/proc/<pid>/fd/<n>` link names. (This and the `/proc` readers below are Linux's
/// driver's; the tests run them on every host.)
#[cfg(any(test, target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FdLink<'b> {
    /// `socket:[<inode>]`.
    Socket(u64),
    /// An absolute path.
    Path(&'b [u8]),
    /// `pipe:[..]`, `anon_inode:..` and the like.
    Other,
}

/// What `target`, a descriptor link's target, names.
#[cfg(any(test, target_os = "linux"))]
pub fn fd_link(target: &[u8]) -> FdLink<'_> {
    if let Some(inode) = target.strip_prefix(b"socket:[").and_then(|rest| rest.strip_suffix(b"]")) {
        return match decimal(inode) {
            Some(inode) => FdLink::Socket(inode),
            None => FdLink::Other,
        };
    }
    if target.first() == Some(&b'/') {
        return FdLink::Path(target);
    }
    FdLink::Other
}

/// A /proc/net table line that is not what the kernel writes.
#[cfg(any(test, target_os = "linux"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcNetError {
    /// Its line, 1 the header.
    pub line: usize,
    /// The column that did not parse.
    pub column: &'static str,
}

/// The sockets of one `/proc/<pid>/net/{tcp,tcp6,udp,udp6}` table whose inode is in `wanted`
/// (sorted), appended to `into` as `(inode, socket)`. `v6` for the `6` tables (32 hex digits
/// an address). The kernel writes each address as native-endian 32-bit words in hex
/// (`%08X` of the `__be32`), so a word's bytes are its `to_ne_bytes`; the ports are plain hex.
#[cfg(any(test, target_os = "linux"))]
pub fn proc_net_sockets(
    table: &[u8],
    proto: Proto,
    v6: bool,
    wanted: &[u64],
    into: &mut AVec<'_, (u64, Socket)>,
) -> Result<(), ProcNetError> {
    for (at, line) in table.split(|&byte| byte == b'\n').enumerate().skip(1) {
        let line_no = at + 1;
        let mut columns = line.split(|byte| byte.is_ascii_whitespace()).filter(|column| !column.is_empty());
        let Some(_slot) = columns.next() else {
            continue;
        };
        let bad = |column| ProcNetError { line: line_no, column };
        let local = columns.next().and_then(|it| end(it, v6)).ok_or(bad("local_address"))?;
        let remote = columns.next().and_then(|it| end(it, v6)).ok_or(bad("rem_address"))?;
        let state = columns.next().and_then(hex_u8).ok_or(bad("st"))?;
        // tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout, then the inode.
        let inode = columns.nth(5).and_then(decimal).ok_or(bad("inode"))?;
        if wanted.binary_search(&inode).is_err() {
            continue;
        }
        let state = match proto {
            Proto::Tcp => Some(TcpState::from_linux(state).ok_or(bad("st"))?),
            Proto::Udp => None,
        };
        into.push((inode, Socket { proto, local, remote, state }));
    }
    Ok(())
}

/// `ADDR:PORT` of a /proc/net table.
#[cfg(any(test, target_os = "linux"))]
fn end(column: &[u8], v6: bool) -> Option<End> {
    let colon = column.iter().position(|&byte| byte == b':')?;
    let (addr, port) = column.split_at(colon);
    let port = u16::try_from(hex(port.get(1..)?)?).ok()?;
    let ip = if v6 {
        let mut bytes = [0u8; 16];
        if addr.len() != 32 {
            return None;
        }
        for (into, word) in bytes.chunks_exact_mut(4).zip(addr.chunks_exact(8)) {
            into.copy_from_slice(&u32::try_from(hex(word)?).ok()?.to_ne_bytes());
        }
        Ip::V6(bytes)
    } else {
        if addr.len() != 8 {
            return None;
        }
        Ip::V4(u32::try_from(hex(addr)?).ok()?.to_ne_bytes())
    };
    Some(End { ip, port })
}

#[cfg(any(test, target_os = "linux"))]
fn hex_u8(column: &[u8]) -> Option<u8> {
    u8::try_from(hex(column)?).ok()
}

/// Up to 16 hex digits, at least one.
#[cfg(any(test, target_os = "linux"))]
fn hex(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    digits.iter().try_fold(0u64, |value, &digit| Some(value << 4 | u64::from(char::from(digit).to_digit(16)?)))
}

/// Decimal digits, at least one, that fit a `u64`.
#[cfg(any(test, target_os = "linux"))]
fn decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |value, &digit| {
        value.checked_mul(10)?.checked_add(u64::from(char::from(digit).to_digit(10)?))
    })
}

#[cfg(test)]
#[path = "resources_tests.rs"]
mod tests;
