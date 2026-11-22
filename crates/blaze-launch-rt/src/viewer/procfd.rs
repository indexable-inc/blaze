//! The facts `/api/resources` answers, asked of the kernel in this process: on macOS libproc
//! (`proc_pidinfo(PROC_PIDLISTFDS)` sized by its own first call, then `proc_pidfdinfo` per
//! descriptor, `PROC_PIDFDSOCKETINFO` for a socket and `PROC_PIDFDVNODEPATHINFO` for a vnode),
//! on Linux `/proc/<pid>/fd` and `/proc/<pid>/net/{tcp,tcp6,udp,udp6}`. No process is
//! started. A refusal is the call, the pid and the errno ([`ResourcesError`]); a descriptor
//! closed between the list and its own call is not one (it is no longer open), and nothing
//! else is passed over.

#![blaze::driver]

use blaze_own::Alloc;
use blaze_sys::Errno;

use super::resources::{Refused, Resources, ResourcesError};

/// `call` refused for process `pid` with `errno`.
fn refused(call: &'static str, pid: u32, errno: Errno) -> ResourcesError {
    let refused = match errno {
        Errno::Srch | Errno::NoEnt => Refused::Gone,
        Errno::Perm | Errno::Acces => Refused::Denied,
        _ => Refused::Failed,
    };
    ResourcesError { call, pid, errno: errno.raw(), name: errno.name(), refused }
}

/// Process `pid`'s descriptors, sockets and open files (none asked for pid 0).
pub fn gather(pid: u32, alloc: Alloc<'_>) -> Result<Resources<'_>, ResourcesError> {
    let mut facts = Resources::new_in(pid, alloc);
    if pid == 0 {
        return Ok(facts);
    }
    let raw = i32::try_from(pid).map_err(|_| refused("the query's pid", pid, Errno::Srch))?;
    gather_os(raw, &mut facts, alloc)?;
    Ok(facts)
}

#[cfg(target_os = "macos")]
fn gather_os<'a>(raw: i32, facts: &mut Resources<'a>, alloc: Alloc<'a>) -> Result<(), ResourcesError> {
    use blaze_own::AVec;
    use blaze_sys::posix::{
        net::{AF_INET, AF_INET6, IPPROTO_TCP},
        proc::{INI_IPV4, PROX_FDTYPE_SOCKET, PROX_FDTYPE_VNODE, VREG},
        thread::{proc_fd_list_capacity, proc_fd_socket_info, proc_fd_vnode_path, proc_fdinfo, proc_list_fds, socket_info},
    };

    use super::resources::{End, Ip, Proto, Socket, TcpState};

    /// `IPPROTO_UDP` (netinet/in.h).
    const IPPROTO_UDP: i32 = 17;

    /// An `in_port_t` in network order in the low 16 bits of an `int`.
    fn port(raw: i32) -> u16 {
        u16::try_from(raw & 0xFFFF).map(u16::from_be).unwrap_or(0)
    }

    /// The socket `info` is, if it is a TCP or UDP one over IP.
    fn socket(info: &socket_info) -> Option<Socket> {
        if info.soi_family != AF_INET && info.soi_family != AF_INET6 {
            return None;
        }
        let proto = match info.soi_protocol {
            IPPROTO_TCP => Proto::Tcp,
            IPPROTO_UDP => Proto::Udp,
            _ => return None,
        };
        let inet = info.inet()?;
        let v4 = inet.insi_vflag & INI_IPV4 != 0;
        let ip = |bytes: [u8; 16]| match bytes {
            [.., a, b, c, d] if v4 => Ip::V4([a, b, c, d]),
            bytes => Ip::V6(bytes),
        };
        let state = match proto {
            Proto::Tcp => info.tcp().and_then(|tcp| TcpState::from_darwin(tcp.tcpsi_state)),
            Proto::Udp => None,
        };
        Some(Socket {
            proto,
            local: End { ip: ip(inet.insi_laddr), port: port(inet.insi_lport) },
            remote: End { ip: ip(inet.insi_faddr), port: port(inet.insi_fport) },
            state,
        })
    }

    let pid = facts.pid;
    let listed = "proc_pidinfo(PROC_PIDLISTFDS)";
    // The list's room from libproc's own first answer (the table and 20 spare); a descriptor
    // opened between the two calls past the spare is not listed, as with any snapshot.
    let room = proc_fd_list_capacity(raw).map_err(|errno| refused(listed, pid, errno))?;
    let mut fds = AVec::from_elem_in(proc_fdinfo::default(), room, alloc);
    let got = proc_list_fds(raw, &mut fds).map_err(|errno| refused(listed, pid, errno))?;
    fds.truncate(got);
    for fd in fds.iter() {
        facts.fd_count = facts.fd_count.saturating_add(1);
        match fd.proc_fdtype {
            PROX_FDTYPE_SOCKET if facts.wants_socket() => match proc_fd_socket_info(raw, fd.proc_fd) {
                Ok(info) => {
                    if let Some(socket) = socket(&info.psi) {
                        facts.push_socket(socket);
                    }
                }
                Err(Errno::BadF) => {}
                Err(errno) => return Err(refused("proc_pidfdinfo(PROC_PIDFDSOCKETINFO)", pid, errno)),
            },
            PROX_FDTYPE_VNODE if facts.wants_file() => match proc_fd_vnode_path(raw, fd.proc_fd) {
                Ok(info) => {
                    if info.pvip.vip_vi.vi_type == VREG {
                        facts.push_file(info.pvip.path());
                    }
                }
                Err(Errno::BadF) => {}
                Err(errno) => return Err(refused("proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)", pid, errno)),
            },
            _ => {}
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn gather_os<'a>(raw: i32, facts: &mut Resources<'a>, alloc: Alloc<'a>) -> Result<(), ResourcesError> {
    use core::fmt::Write as _;
    use std::{fs, io, os::unix::ffi::OsStrExt};

    use blaze_own::{AString, AVec};

    use super::resources::{FdLink, Proto, fd_link, proc_net_sockets};

    /// The most bytes of one /proc/net table read (a table line is 150 bytes: 27k sockets).
    const MOST_TABLE_BYTES: usize = 4 << 20;

    fn io_refused(call: &'static str, pid: u32, error: &io::Error) -> ResourcesError {
        let errno = error.raw_os_error().and_then(|raw| u16::try_from(raw).ok()).map_or(Errno::Inval, Errno::from_raw);
        refused(call, pid, errno)
    }

    let pid = facts.pid;
    let mut path = AString::new_in(alloc);
    let _ = write!(path, "/proc/{raw}/fd");
    let listing = "opendir(/proc/<pid>/fd)";
    let entries = fs::read_dir(path.as_str()).map_err(|error| io_refused(listing, pid, &error))?;
    let mut links: AVec<'_, (u32, AVec<'_, u8>)> = AVec::new_in(alloc);
    for entry in entries {
        let entry = entry.map_err(|error| io_refused(listing, pid, &error))?;
        let Some(fd) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        match fs::read_link(entry.path()) {
            Ok(target) => links.push((fd, AVec::from_slice_in(target.as_os_str().as_bytes(), alloc))),
            // Closed since the listing: no longer open.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_refused("readlink(/proc/<pid>/fd/<n>)", pid, &error)),
        }
    }
    links.sort_unstable_by_key(|&(fd, _)| fd);
    let mut wanted: AVec<'_, u64> = AVec::new_in(alloc);
    for (_, target) in links.iter() {
        if let FdLink::Socket(inode) = fd_link(target) {
            wanted.push(inode);
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    let mut sockets = AVec::new_in(alloc);
    if !wanted.is_empty() {
        let tables: [(&str, &'static str, Proto, bool); 4] = [
            ("tcp", "read(/proc/<pid>/net/tcp)", Proto::Tcp, false),
            ("tcp6", "read(/proc/<pid>/net/tcp6)", Proto::Tcp, true),
            ("udp", "read(/proc/<pid>/net/udp)", Proto::Udp, false),
            ("udp6", "read(/proc/<pid>/net/udp6)", Proto::Udp, true),
        ];
        for (table, call, proto, v6) in tables {
            path.clear();
            let _ = write!(path, "/proc/{raw}/net/{table}");
            let bytes = match super::read_bounded(path.as_str(), MOST_TABLE_BYTES, alloc) {
                Ok(bytes) => bytes,
                // A kernel without IPv6 has no `6` tables: no socket is in them.
                Err(error) if error.kind() == io::ErrorKind::NotFound && v6 => continue,
                Err(error) => return Err(io_refused(call, pid, &error)),
            };
            proc_net_sockets(&bytes, proto, v6, &wanted, &mut sockets).map_err(|_| refused(call, pid, Errno::Inval))?;
        }
    }
    sockets.sort_unstable_by_key(|&(inode, _)| inode);
    for (_, target) in links.iter() {
        facts.add_linux_fd(target, &sockets);
    }
    Ok(())
}
