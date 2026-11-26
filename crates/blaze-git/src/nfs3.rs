//! Pure sans-IO SunRPC (RFC 5531) + MOUNTv3 + NFSv3 (RFC 1813) server over [`LazyVfs`].
//!
//! Zero OS syscalls, zero sockets, zero `std`, zero `unsafe`, explicit [`Alloc`].
//! A thin TCP driver (e.g. `bgit mount` on macOS via native `/sbin/mount_nfs`) feeds raw TCP
//! bytes into [`Nfs3Conn::push_bytes`] and drains framed RPC replies via [`Nfs3Conn::take_reply`].
//!
//! Serving both `MOUNTv3` (`prog = 100005, vers = 3`) and `NFSv3` (`prog = 100003, vers = 3`) on
//! one port lets macOS mount with `mount_nfs -o nolocks,vers=3,tcp,port=P,mountport=P 127.0.0.1:/ <dir>`
//! with zero kernel extensions, zero FUSE, zero root privileges, and zero `rpcbind`.

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

use crate::{
    storage::ObjectStore,
    vfs::{LazyVfs, ROOT_INO, VfsAttr, VfsError, VfsKind},
};

/// Maximum single SunRPC TCP record accepted (2 MiB, rule `bounded-progress`).
pub const MOST_RPC_RECORD_BYTES: usize = 2 << 20;

const RPC_CALL: u32 = 0;
const RPC_REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;

const MSG_ACCEPTED: u32 = 0;
const ACCEPT_SUCCESS: u32 = 0;
const ACCEPT_PROG_UNAVAIL: u32 = 1;
const ACCEPT_PROG_MISMATCH: u32 = 2;
const ACCEPT_PROC_UNAVAIL: u32 = 3;
const ACCEPT_GARBAGE_ARGS: u32 = 4;

const PROG_NFS: u32 = 100_003;
const PROG_MOUNT: u32 = 100_005;

// MOUNTv3 procedures
const MNT_PROC_NULL: u32 = 0;
const MNT_PROC_MNT: u32 = 1;
const MNT_PROC_UMNT: u32 = 3;
const MNT_PROC_UMNTALL: u32 = 4;
const MNT_PROC_EXPORT: u32 = 5;

// NFSv3 procedures
const NFS_PROC_NULL: u32 = 0;
const NFS_PROC_GETATTR: u32 = 1;
const NFS_PROC_SETATTR: u32 = 2;
const NFS_PROC_LOOKUP: u32 = 3;
const NFS_PROC_ACCESS: u32 = 4;
const NFS_PROC_READLINK: u32 = 5;
const NFS_PROC_READ: u32 = 6;
const NFS_PROC_WRITE: u32 = 7;
const NFS_PROC_CREATE: u32 = 8;
const NFS_PROC_MKDIR: u32 = 9;
const NFS_PROC_SYMLINK: u32 = 10;
const NFS_PROC_REMOVE: u32 = 12;
const NFS_PROC_RMDIR: u32 = 13;
const NFS_PROC_RENAME: u32 = 14;
const NFS_PROC_READDIR: u32 = 16;
const NFS_PROC_READDIRPLUS: u32 = 17;
const NFS_PROC_FSSTAT: u32 = 18;
const NFS_PROC_FSINFO: u32 = 19;
const NFS_PROC_PATHCONF: u32 = 20;
const NFS_PROC_COMMIT: u32 = 21;

// NFSv3 status codes (`nfsstat3`)
const NFS3_OK: u32 = 0;
const NFS3ERR_NOENT: u32 = 2;
const NFS3ERR_IO: u32 = 5;
const NFS3ERR_EXIST: u32 = 17;
const NFS3ERR_NOTDIR: u32 = 20;
const NFS3ERR_ISDIR: u32 = 21;
const NFS3ERR_INVAL: u32 = 22;
const NFS3ERR_NOSPC: u32 = 28;
const NFS3ERR_ROFS: u32 = 30;
const NFS3ERR_NAMETOOLONG: u32 = 63;
const NFS3ERR_NOTEMPTY: u32 = 66;
const NFS3ERR_STALE: u32 = 70;

// NFSv3 file types (`ftype3`)
const NF3REG: u32 = 1;
const NF3DIR: u32 = 2;
const NF3LNK: u32 = 5;

// NFSv3 ACCESS bits
const ACCESS3_READ: u32 = 0x01;
const ACCESS3_LOOKUP: u32 = 0x02;
const ACCESS3_MODIFY: u32 = 0x04;
const ACCESS3_EXTEND: u32 = 0x08;
const ACCESS3_DELETE: u32 = 0x10;
const ACCESS3_EXECUTE: u32 = 0x20;

const WRITE_VERF: [u8; 8] = *b"BGITVFS1";

/// Sans-IO TCP record-marking framer for one SunRPC connection.
pub struct Nfs3Conn<'a> {
    rx_buf: AVec<'a, u8>,
    rx_pos: usize,
    assembled: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

impl<'a> Nfs3Conn<'a> {
    pub fn new(alloc: Alloc<'a>) -> Nfs3Conn<'a> {
        Nfs3Conn {
            rx_buf: Vec::with_capacity_in(8192, alloc),
            rx_pos: 0,
            assembled: Vec::new_in(alloc),
            alloc,
        }
    }

    /// Append raw bytes read from a TCP socket.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        if self.rx_pos > 0 && self.rx_pos == self.rx_buf.len() {
            self.rx_buf.clear();
            self.rx_pos = 0;
        }
        self.rx_buf.extend_from_slice(bytes);
    }

    /// Process all complete SunRPC TCP records currently buffered, appending framed replies to
    /// `tx_out`. Returns `Err(())` only on a framing violation (e.g. oversized record).
    pub fn poll(
        &mut self,
        vfs: &mut LazyVfs<'a>,
        store: &impl ObjectStore<'a>,
        now_sec: u32,
        tx_out: &mut AVec<'a, u8>,
    ) -> Result<u32, ()> {
        let mut handled = 0u32;
        loop {
            let avail = self.rx_buf.len() - self.rx_pos;
            if avail < 4 {
                break;
            }
            let hdr = u32::from_be_bytes(
                self.rx_buf[self.rx_pos..self.rx_pos + 4].try_into().unwrap(),
            );
            let is_last = (hdr & 0x8000_0000) != 0;
            let frag_len = (hdr & 0x7fff_ffff) as usize;
            if self.assembled.len().saturating_add(frag_len) > MOST_RPC_RECORD_BYTES {
                return Err(());
            }
            if avail < 4 + frag_len {
                break;
            }
            let frag_start = self.rx_pos + 4;
            let frag_end = frag_start + frag_len;
            self.rx_pos = frag_end;

            let mut reply_body = Vec::with_capacity_in(512, self.alloc);
            if self.assembled.is_empty() && is_last {
                // Fast path: single-fragment record (99.9% of NFSv3 RPCs), zero copy!
                handle_rpc_call(
                    &self.rx_buf[frag_start..frag_end],
                    vfs,
                    store,
                    now_sec,
                    self.alloc,
                    &mut reply_body,
                );
            } else {
                self.assembled.extend_from_slice(&self.rx_buf[frag_start..frag_end]);
                if !is_last {
                    continue;
                }
                handle_rpc_call(
                    &self.assembled,
                    vfs,
                    store,
                    now_sec,
                    self.alloc,
                    &mut reply_body,
                );
                self.assembled.clear();
            }
            if !reply_body.is_empty() {
                let rm = 0x8000_0000u32 | (reply_body.len() as u32);
                tx_out.extend_from_slice(&rm.to_be_bytes());
                tx_out.extend_from_slice(&reply_body);
            }
            handled = handled.saturating_add(1);
        }

        // Compact consumed prefix when half-empty.
        if self.rx_pos > 0 {
            if self.rx_pos == self.rx_buf.len() {
                self.rx_buf.clear();
                self.rx_pos = 0;
            } else if self.rx_pos >= 65536 {
                let rem = self.rx_buf.len() - self.rx_pos;
                self.rx_buf.copy_within(self.rx_pos.., 0);
                self.rx_buf.truncate(rem);
                self.rx_pos = 0;
            }
        }
        Ok(handled)
    }
}

/// Handle a single unframed SunRPC call payload and encode the unframed SunRPC reply into `out`.
pub fn handle_rpc_call<'a>(
    call: &[u8],
    vfs: &mut LazyVfs<'a>,
    store: &impl ObjectStore<'a>,
    now_sec: u32,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) {
    out.clear();
    let mut r = XdrReader::new(call);
    let Some(xid) = r.u32() else { return };
    let Some(msg_type) = r.u32() else { return };
    if msg_type != RPC_CALL {
        return;
    }
    let Some(rpcvers) = r.u32() else { return };
    let Some(prog) = r.u32() else { return };
    let Some(vers) = r.u32() else { return };
    let Some(proc_id) = r.u32() else { return };
    // Skip `cred` and `verf` (`opaque_auth = flavor:u32 + opaque`).
    if r.u32().is_none() || r.opaque().is_none() || r.u32().is_none() || r.opaque().is_none() {
        write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
        return;
    }
    if rpcvers != RPC_VERSION {
        // RPC version mismatch.
        put_u32(out, xid);
        put_u32(out, RPC_REPLY);
        put_u32(out, 1); // MSG_DENIED
        put_u32(out, 0); // RPC_MISMATCH
        put_u32(out, RPC_VERSION);
        put_u32(out, RPC_VERSION);
        return;
    }

    match prog {
        PROG_MOUNT => {
            if vers != 3 {
                write_rpc_reply_header(xid, ACCEPT_PROG_MISMATCH, out);
                put_u32(out, 3);
                put_u32(out, 3);
                return;
            }
            handle_mount3(xid, proc_id, &mut r, out);
        }
        PROG_NFS => {
            if vers != 3 {
                write_rpc_reply_header(xid, ACCEPT_PROG_MISMATCH, out);
                put_u32(out, 3);
                put_u32(out, 3);
                return;
            }
            handle_nfs3(xid, proc_id, &mut r, vfs, store, now_sec, alloc, out);
        }
        _ => {
            write_rpc_reply_header(xid, ACCEPT_PROG_UNAVAIL, out);
        }
    }
}

fn handle_mount3(xid: u32, proc_id: u32, r: &mut XdrReader<'_>, out: &mut AVec<'_, u8>) {
    match proc_id {
        MNT_PROC_NULL => {
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
        }
        MNT_PROC_MNT => {
            if r.opaque().is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, 0); // MNT3_OK
            put_fh(out, ROOT_INO);
            // auth_flavors: [AUTH_UNIX (1), AUTH_NONE (0)]
            put_u32(out, 2);
            put_u32(out, 1);
            put_u32(out, 0);
        }
        MNT_PROC_UMNT | MNT_PROC_UMNTALL => {
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
        }
        MNT_PROC_EXPORT => {
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, 1); // opt_next = Some
            put_opaque(out, b"/"); // ex_dir
            put_u32(out, 0); // ex_groups = None (world/localhost)
            put_u32(out, 0); // next = None
        }
        _ => {
            write_rpc_reply_header(xid, ACCEPT_PROC_UNAVAIL, out);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_nfs3<'a>(
    xid: u32,
    proc_id: u32,
    r: &mut XdrReader<'_>,
    vfs: &mut LazyVfs<'a>,
    store: &impl ObjectStore<'a>,
    now_sec: u32,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) {
    match proc_id {
        NFS_PROC_NULL => {
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
        }

        NFS_PROC_GETATTR => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.getattr(ino, store) {
                Ok(attr) => {
                    put_u32(out, NFS3_OK);
                    put_fattr3(out, attr);
                }
                Err(e) => put_u32(out, vfs_err_to_nfs3(e)),
            }
        }

        NFS_PROC_SETATTR => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(sattr) = r.sattr3(now_sec) else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            // guard: sattrguard3 (bool + optional nfstime3)
            let Some(check_guard) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if check_guard != 0 && (r.u32().is_none() || r.u32().is_none()) {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.setattr(ino, sattr.mode, sattr.size, sattr.mtime_sec, now_sec, store) {
                Ok(attr) => {
                    put_u32(out, NFS3_OK);
                    put_wcc_data(out, Some(attr));
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(ino, store).ok());
                }
            }
        }

        NFS_PROC_LOOKUP => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.lookup(dir_ino, name, store) {
                Ok((child_ino, child_attr)) => {
                    put_u32(out, NFS3_OK);
                    put_fh(out, child_ino);
                    put_post_op_attr(out, Some(child_attr));
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_ACCESS => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(want) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.getattr(ino, store) {
                Ok(attr) => {
                    let mut allowed = ACCESS3_READ | ACCESS3_LOOKUP;
                    if attr.kind == VfsKind::Dir || (attr.mode & 0o111 != 0) {
                        allowed |= ACCESS3_EXECUTE;
                    }
                    if !vfs.opts().read_only {
                        allowed |= ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE;
                    }
                    put_u32(out, NFS3_OK);
                    put_post_op_attr(out, Some(attr));
                    put_u32(out, want & allowed);
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, None);
                }
            }
        }

        NFS_PROC_READLINK => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            let mut target = Vec::new_in(alloc);
            match vfs.readlink(ino, store, &mut target) {
                Ok(()) => {
                    put_u32(out, NFS3_OK);
                    put_post_op_attr(out, vfs.getattr(ino, store).ok());
                    put_opaque(out, &target);
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, vfs.getattr(ino, store).ok());
                }
            }
        }

        NFS_PROC_READ => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(offset) = r.u64() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(count) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let clamped = (count as usize).min(1024 * 1024);
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            let mut data = Vec::new_in(alloc);
            match vfs.read(ino, offset, clamped, store, &mut data) {
                Ok(eof) => {
                    put_u32(out, NFS3_OK);
                    put_post_op_attr(out, vfs.getattr(ino, store).ok());
                    put_u32(out, data.len() as u32);
                    put_u32(out, u32::from(eof));
                    put_opaque(out, &data);
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, vfs.getattr(ino, store).ok());
                }
            }
        }

        NFS_PROC_WRITE => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(offset) = r.u64() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(count) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(_stable) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(data) = r.opaque() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let slice = &data[..data.len().min(count as usize)];
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.write(ino, offset, slice, now_sec, store) {
                Ok(attr) => {
                    put_u32(out, NFS3_OK);
                    put_wcc_data(out, Some(attr));
                    put_u32(out, slice.len() as u32);
                    put_u32(out, 2); // FILE_SYNC
                    out.extend_from_slice(&WRITE_VERF);
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(ino, store).ok());
                }
            }
        }

        NFS_PROC_CREATE => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(how) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let (mode, exclusive) = match how {
                0 => {
                    // UNCHECKED
                    let Some(s) = r.sattr3(now_sec) else {
                        write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                        return;
                    };
                    (s.mode.unwrap_or(0o644), false)
                }
                1 => {
                    // GUARDED
                    let Some(s) = r.sattr3(now_sec) else {
                        write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                        return;
                    };
                    (s.mode.unwrap_or(0o644), true)
                }
                2 => {
                    // EXCLUSIVE (8-byte createverf3)
                    if r.fixed(8).is_none() {
                        write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                        return;
                    }
                    (0o644, true)
                }
                _ => {
                    write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                    return;
                }
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.create(dir_ino, name, mode, exclusive, now_sec, store) {
                Ok((child_ino, child_attr)) => {
                    put_u32(out, NFS3_OK);
                    put_u32(out, 1); // post_op_fh3 present
                    put_fh(out, child_ino);
                    put_post_op_attr(out, Some(child_attr));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_MKDIR => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if r.sattr3(now_sec).is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.mkdir(dir_ino, name, now_sec, store) {
                Ok((child_ino, child_attr)) => {
                    put_u32(out, NFS3_OK);
                    put_u32(out, 1);
                    put_fh(out, child_ino);
                    put_post_op_attr(out, Some(child_attr));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_SYMLINK => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if r.sattr3(now_sec).is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            let Some(target) = r.opaque() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.symlink(dir_ino, name, target, now_sec, store) {
                Ok((child_ino, child_attr)) => {
                    put_u32(out, NFS3_OK);
                    put_u32(out, 1);
                    put_fh(out, child_ino);
                    put_post_op_attr(out, Some(child_attr));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_REMOVE => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.remove(dir_ino, name, now_sec, store) {
                Ok(()) => {
                    put_u32(out, NFS3_OK);
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_RMDIR => {
            let Some((dir_ino, name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.rmdir(dir_ino, name, now_sec, store) {
                Ok(()) => {
                    put_u32(out, NFS3_OK);
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_RENAME => {
            let Some((from_dir, from_name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some((to_dir, to_name)) = r.diropargs() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            match vfs.rename(from_dir, from_name, to_dir, to_name, now_sec, store) {
                Ok(()) => {
                    put_u32(out, NFS3_OK);
                    put_wcc_data(out, vfs.getattr(from_dir, store).ok());
                    put_wcc_data(out, vfs.getattr(to_dir, store).ok());
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_wcc_data(out, vfs.getattr(from_dir, store).ok());
                    put_wcc_data(out, vfs.getattr(to_dir, store).ok());
                }
            }
        }

        NFS_PROC_READDIR => {
            let Some(dir_ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(cookie) = r.u64() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if r.fixed(8).is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            let Some(count) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            let max_entries = ((count as usize) / 48).clamp(1, 2048);
            match vfs.readdir(dir_ino, cookie, max_entries, store) {
                Ok((entries, mut eof)) => {
                    put_u32(out, NFS3_OK);
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                    out.extend_from_slice(&[0u8; 8]); // cookieverf
                    let start_len = out.len();
                    let budget = (count as usize).saturating_sub(128);
                    for (idx, e) in entries.iter().enumerate() {
                        let est = 4 + 8 + 4 + pad4(e.name.len()) + 8;
                        if idx > 0 && (out.len() - start_len) + est > budget {
                            eof = false;
                            break;
                        }
                        put_u32(out, 1); // value_follows
                        put_u64(out, e.ino);
                        put_opaque(out, e.name.as_bytes());
                        put_u64(out, e.cookie);
                    }
                    put_u32(out, 0); // end of list
                    put_u32(out, u32::from(eof));
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_READDIRPLUS => {
            let Some(dir_ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(cookie) = r.u64() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if r.fixed(8).is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            let Some(_dircount) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            let Some(maxcount) = r.u32() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            let max_entries = ((maxcount as usize) / 140).clamp(1, 512);
            match vfs.readdir(dir_ino, cookie, max_entries, store) {
                Ok((entries, mut eof)) => {
                    put_u32(out, NFS3_OK);
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                    out.extend_from_slice(&[0u8; 8]); // cookieverf
                    let start_len = out.len();
                    let budget = (maxcount as usize).saturating_sub(256);
                    for (idx, e) in entries.iter().enumerate() {
                        let est = 4 + 8 + 4 + pad4(e.name.len()) + 8 + 88 + 16;
                        if idx > 0 && (out.len() - start_len) + est > budget {
                            eof = false;
                            break;
                        }
                        put_u32(out, 1); // value_follows
                        put_u64(out, e.ino);
                        put_opaque(out, e.name.as_bytes());
                        put_u64(out, e.cookie);
                        put_post_op_attr(out, Some(e.attr));
                        put_u32(out, 1); // name_handle present
                        put_fh(out, e.ino);
                    }
                    put_u32(out, 0);
                    put_u32(out, u32::from(eof));
                }
                Err(e) => {
                    put_u32(out, vfs_err_to_nfs3(e));
                    put_post_op_attr(out, vfs.getattr(dir_ino, store).ok());
                }
            }
        }

        NFS_PROC_FSSTAT => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, NFS3_OK);
            put_post_op_attr(out, vfs.getattr(ino, store).ok());
            let total_bytes: u64 = 1u64 << 40; // 1 TiB virtual capacity
            let free_bytes: u64 = if vfs.opts().read_only { 0 } else { total_bytes / 2 };
            put_u64(out, total_bytes);
            put_u64(out, free_bytes);
            put_u64(out, free_bytes);
            put_u64(out, vfs.opts().limits.most_inodes as u64);
            put_u64(out, (vfs.opts().limits.most_inodes as u64).saturating_sub(vfs.stats().total_inodes));
            put_u64(out, (vfs.opts().limits.most_inodes as u64).saturating_sub(vfs.stats().total_inodes));
            put_u32(out, 0); // invarsec
        }

        NFS_PROC_FSINFO => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, NFS3_OK);
            put_post_op_attr(out, vfs.getattr(ino, store).ok());
            put_u32(out, 131_072); // rtmax
            put_u32(out, 65_536); // rtpref
            put_u32(out, 4096); // rtmult
            put_u32(out, 131_072); // wtmax
            put_u32(out, 65_536); // wtpref
            put_u32(out, 4096); // wtmult
            put_u32(out, 65_536); // dtpref
            put_u64(out, vfs.opts().limits.most_file_bytes as u64); // maxfilesize
            put_u32(out, 1); // time_delta.seconds
            put_u32(out, 0); // time_delta.nseconds
            put_u32(out, 0x001a); // FSF3_SYMLINK | FSF3_HOMOGENEOUS | FSF3_CANSETTIME
        }

        NFS_PROC_PATHCONF => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, NFS3_OK);
            put_post_op_attr(out, vfs.getattr(ino, store).ok());
            put_u32(out, 32_767); // linkmax
            put_u32(out, 255); // name_max
            put_u32(out, 1); // no_trunc = true
            put_u32(out, 1); // chown_restricted = true
            put_u32(out, 0); // case_insensitive = false
            put_u32(out, 1); // case_preserving = true
        }

        NFS_PROC_COMMIT => {
            let Some(ino) = r.fh() else {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            };
            if r.u64().is_none() || r.u32().is_none() {
                write_rpc_reply_header(xid, ACCEPT_GARBAGE_ARGS, out);
                return;
            }
            write_rpc_reply_header(xid, ACCEPT_SUCCESS, out);
            put_u32(out, NFS3_OK);
            put_wcc_data(out, vfs.getattr(ino, store).ok());
            out.extend_from_slice(&WRITE_VERF);
        }

        _ => {
            write_rpc_reply_header(xid, ACCEPT_PROC_UNAVAIL, out);
        }
    }
}

fn vfs_err_to_nfs3(e: VfsError) -> u32 {
    match e {
        VfsError::NoEnt => NFS3ERR_NOENT,
        VfsError::Exist => NFS3ERR_EXIST,
        VfsError::NotDir => NFS3ERR_NOTDIR,
        VfsError::IsDir => NFS3ERR_ISDIR,
        VfsError::NotEmpty => NFS3ERR_NOTEMPTY,
        VfsError::ReadOnly => NFS3ERR_ROFS,
        VfsError::Inval => NFS3ERR_INVAL,
        VfsError::NameTooLong => NFS3ERR_NAMETOOLONG,
        VfsError::NoSpc => NFS3ERR_NOSPC,
        VfsError::Io(_) => NFS3ERR_IO,
    }
}

// ---------------------------------------------------------------------------
// XDR Reader & Writer Primitives (RFC 4506)
// ---------------------------------------------------------------------------

struct ParsedSattr3 {
    mode: Option<u32>,
    size: Option<u64>,
    mtime_sec: Option<u32>,
}

struct XdrReader<'b> {
    buf: &'b [u8],
    at: usize,
}

impl<'b> XdrReader<'b> {
    fn new(buf: &'b [u8]) -> XdrReader<'b> {
        XdrReader { buf, at: 0 }
    }

    fn u32(&mut self) -> Option<u32> {
        if self.at + 4 > self.buf.len() {
            return None;
        }
        let v = u32::from_be_bytes(self.buf[self.at..self.at + 4].try_into().unwrap());
        self.at += 4;
        Some(v)
    }

    fn u64(&mut self) -> Option<u64> {
        if self.at + 8 > self.buf.len() {
            return None;
        }
        let v = u64::from_be_bytes(self.buf[self.at..self.at + 8].try_into().unwrap());
        self.at += 8;
        Some(v)
    }

    fn fixed(&mut self, len: usize) -> Option<&'b [u8]> {
        let padded = pad4(len);
        if self.at + padded > self.buf.len() {
            return None;
        }
        let s = &self.buf[self.at..self.at + len];
        self.at += padded;
        Some(s)
    }

    fn opaque(&mut self) -> Option<&'b [u8]> {
        let len = self.u32()? as usize;
        self.fixed(len)
    }

    fn fh(&mut self) -> Option<u64> {
        let raw = self.opaque()?;
        if raw.len() != 8 {
            return None;
        }
        Some(u64::from_be_bytes(raw.try_into().unwrap()))
    }

    fn diropargs(&mut self) -> Option<(u64, &'b str)> {
        let dir_ino = self.fh()?;
        let name_bytes = self.opaque()?;
        let name = core::str::from_utf8(name_bytes).ok()?;
        Some((dir_ino, name))
    }

    fn sattr3(&mut self, now_sec: u32) -> Option<ParsedSattr3> {
        // mode
        let mode = if self.u32()? != 0 { Some(self.u32()?) } else { None };
        // uid
        if self.u32()? != 0 {
            self.u32()?;
        }
        // gid
        if self.u32()? != 0 {
            self.u32()?;
        }
        // size
        let size = if self.u32()? != 0 { Some(self.u64()?) } else { None };
        // atime (0=DONT_CHANGE, 1=SET_TO_SERVER_TIME, 2=SET_TO_CLIENT_TIME)
        match self.u32()? {
            0 | 1 => {}
            2 => {
                self.u32()?;
                self.u32()?;
            }
            _ => return None,
        }
        // mtime
        let mtime_sec = match self.u32()? {
            0 => None,
            1 => Some(now_sec),
            2 => {
                let sec = self.u32()?;
                let _nsec = self.u32()?;
                Some(sec)
            }
            _ => return None,
        };
        Some(ParsedSattr3 { mode, size, mtime_sec })
    }
}

#[inline]
const fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

#[inline]
fn put_u32(out: &mut AVec<'_, u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[inline]
fn put_u64(out: &mut AVec<'_, u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_opaque(out: &mut AVec<'_, u8>, data: &[u8]) {
    put_u32(out, data.len() as u32);
    out.extend_from_slice(data);
    let pad = (4 - (data.len() & 3)) & 3;
    for _ in 0..pad {
        out.push(0);
    }
}

fn put_fh(out: &mut AVec<'_, u8>, ino: u64) {
    put_u32(out, 8);
    out.extend_from_slice(&ino.to_be_bytes());
}

fn write_rpc_reply_header(xid: u32, accept_stat: u32, out: &mut AVec<'_, u8>) {
    put_u32(out, xid);
    put_u32(out, RPC_REPLY);
    put_u32(out, MSG_ACCEPTED);
    put_u32(out, 0); // AUTH_NONE flavor
    put_u32(out, 0); // AUTH_NONE len
    put_u32(out, accept_stat);
}

fn put_fattr3(out: &mut AVec<'_, u8>, a: VfsAttr) {
    let ftype = match a.kind {
        VfsKind::File => NF3REG,
        VfsKind::Dir => NF3DIR,
        VfsKind::Symlink => NF3LNK,
    };
    let used = a.size.div_ceil(4096).saturating_mul(4096);
    put_u32(out, ftype);
    put_u32(out, a.mode & 0o7777);
    put_u32(out, a.nlink);
    put_u32(out, a.uid);
    put_u32(out, a.gid);
    put_u64(out, a.size);
    put_u64(out, used);
    put_u32(out, 0); // rdev.specdata1
    put_u32(out, 0); // rdev.specdata2
    put_u64(out, 1); // fsid
    put_u64(out, a.ino); // fileid
    put_u32(out, a.mtime_sec); // atime.sec
    put_u32(out, 0); // atime.nsec
    put_u32(out, a.mtime_sec); // mtime.sec
    put_u32(out, 0); // mtime.nsec
    put_u32(out, a.ctime_sec); // ctime.sec
    put_u32(out, 0); // ctime.nsec
}

fn put_post_op_attr(out: &mut AVec<'_, u8>, attr: Option<VfsAttr>) {
    match attr {
        Some(a) => {
            put_u32(out, 1);
            put_fattr3(out, a);
        }
        None => put_u32(out, 0),
    }
}

fn put_wcc_data(out: &mut AVec<'_, u8>, post: Option<VfsAttr>) {
    put_u32(out, 0); // pre_op_attr = None
    put_post_op_attr(out, post);
}

// Keep unused error code referenced for completeness.
const _: u32 = NFS3ERR_STALE;
