//! Reading directories (cloudlibc's dirent: opendirat.c, fdopendir.c, readdir.c, closedir.c).
//!
//! `fd_readdir` fills a buffer with packed entries, each a 24-byte header and the name, from a
//! cookie; [`readdir`] hands them out one at a time as `struct dirent`, refilling from the last
//! cookie when the buffer runs out and growing it when one entry doesn't fit.

use core::ffi::{c_char, c_int};
use core::ptr;

use libc::{DT_BLK, DT_CHR, DT_DIR, DT_LNK, DT_REG, DT_SOCK, DT_UNKNOWN, ENOENT, O_DIRECTORY, O_NONBLOCK, O_RDONLY, dirent};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::fd::close;
use crate::rt::fs::filestat_at;
use crate::rt::malloc::{free, malloc, realloc};
use crate::rt::path::Bytes;
use crate::{fail, ok};

/// libc's `DIR`, which is opaque to its users.
pub struct Dir {
    fd: c_int,
    /// Where the next `fd_readdir` starts: the `d_next` of the last entry handed out.
    cookie: u64,
    buffer: *mut u8,
    /// Bytes of `buffer` handed out so far.
    processed: usize,
    size: usize,
    /// Bytes `fd_readdir` put in `buffer`; fewer than `size` means it reached the end.
    used: usize,
    /// The entry [`readdir`] returns, reused and grown to fit each name.
    entry: *mut dirent,
    entry_size: usize,
}

const BUFFER_SIZE: usize = 4096;
const HEADER: usize = size_of::<wasip1::Dirent>();
const NAME_OFFSET: usize = core::mem::offset_of!(dirent, d_name);

/// Grows the `malloc`ed `*buf` of `*size` bytes, doubling, to at least `want`.
unsafe fn grow(buf: &mut *mut u8, size: &mut usize, want: usize) -> bool {
    if *size >= want {
        return true;
    }
    let mut new = (*size).max(1);
    while new < want {
        new *= 2;
    }
    let p = realloc((*buf).cast(), new).cast::<u8>();
    if p.is_null() {
        return false;
    }
    *buf = p;
    *size = new;
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdopendir(fd: c_int) -> *mut Dir {
    let dir = malloc(size_of::<Dir>()).cast::<Dir>();
    if dir.is_null() {
        return ptr::null_mut();
    }
    let buffer = malloc(BUFFER_SIZE).cast::<u8>();
    if buffer.is_null() {
        free(dir.cast());
        return ptr::null_mut();
    }
    // Read the first entries now: this is what tells a directory from anything else.
    let mut used = 0usize;
    if let Err(e) = ok(wasi::fd_readdir(fd, buffer as usize, BUFFER_SIZE, 0, (&raw mut used) as usize)) {
        free(buffer.cast());
        free(dir.cast());
        fail(e);
        return ptr::null_mut();
    }
    dir.write(Dir {
        fd,
        cookie: 0,
        buffer,
        processed: 0,
        size: BUFFER_SIZE,
        used,
        entry: ptr::null_mut(),
        entry_size: 0,
    });
    dir
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut Dir {
    // Through `openat`, so the descriptor's path is kept for an `*at` call under it.
    let fd = crate::rt::fs::openat_impl(libc::AT_FDCWD, path, O_RDONLY | O_NONBLOCK | O_DIRECTORY);
    if fd < 0 {
        return ptr::null_mut();
    }
    let dir = fdopendir(fd);
    if dir.is_null() {
        let e = crate::rt::errno::get();
        close(fd);
        crate::rt::errno::set(e);
    }
    dir
}

/// `S_IFMT` as a `DT_*` value, from a WASI file type.
fn dt_of(filetype: wasip1::Filetype) -> u8 {
    match filetype {
        wasip1::FILETYPE_BLOCK_DEVICE => DT_BLK,
        wasip1::FILETYPE_CHARACTER_DEVICE => DT_CHR,
        wasip1::FILETYPE_DIRECTORY => DT_DIR,
        wasip1::FILETYPE_REGULAR_FILE => DT_REG,
        wasip1::FILETYPE_SOCKET_DGRAM | wasip1::FILETYPE_SOCKET_STREAM => DT_SOCK,
        wasip1::FILETYPE_SYMBOLIC_LINK => DT_LNK,
        _ => DT_UNKNOWN,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir(dirp: *mut Dir) -> *mut dirent {
    let d = &mut *dirp;
    loop {
        let left = d.used - d.processed;
        let mut refill = left < HEADER;
        if refill && d.used < d.size {
            // The last read came back short: the end.
            return ptr::null_mut();
        }
        if !refill {
            let at = d.buffer.add(d.processed);
            let header = ptr::read_unaligned(at.cast::<wasip1::Dirent>());
            let namlen = header.d_namlen as usize;
            let entry_size = HEADER + namlen;
            if namlen == 0 {
                // No name: skip it.
                d.processed += entry_size;
                continue;
            }
            if left < entry_size {
                // Only part of the entry is here: read again from the cookie, with room for it.
                if !grow(&mut d.buffer, &mut d.size, entry_size) {
                    return ptr::null_mut();
                }
                refill = true;
            } else {
                let name = core::slice::from_raw_parts(at.add(HEADER), namlen);
                if name.contains(&0) {
                    // A name with a NUL can't be a C string: skip it.
                    d.processed += entry_size;
                    continue;
                }
                let want = (NAME_OFFSET + namlen + 1).max(size_of::<dirent>());
                let mut entry = d.entry.cast::<u8>();
                if !grow(&mut entry, &mut d.entry_size, want) {
                    return ptr::null_mut();
                }
                d.entry = entry.cast();
                let out = d.entry;
                let out_name = entry.add(NAME_OFFSET);
                ptr::copy_nonoverlapping(name.as_ptr(), out_name, namlen);
                *out_name.add(namlen) = 0;
                let mut ino = header.d_ino;
                let mut kind = header.d_type.raw();
                // A host may leave the inode 0 when it doesn't know it: look it up.
                if ino == 0 && name != b".." {
                    match filestat_at(d.fd, Bytes::of(name), false) {
                        Ok(f) => {
                            ino = f.ino;
                            // The entry may have been replaced since: take the type too.
                            kind = dt_of(f.filetype);
                        }
                        // Gone before we looked: skip it.
                        Err(ENOENT) => {
                            d.processed += entry_size;
                            continue;
                        }
                        Err(e) => {
                            fail(e);
                            return ptr::null_mut();
                        }
                    }
                }
                (*out).d_ino = ino;
                (*out).d_type = kind;
                d.cookie = header.d_next;
                d.processed += entry_size;
                return out;
            }
        }
        if refill {
            d.used = d.size;
            d.processed = d.size;
            let mut used = 0usize;
            if let Err(e) = ok(wasi::fd_readdir(d.fd, d.buffer as usize, d.size, d.cookie as i64, (&raw mut used) as usize)) {
                fail(e);
                return ptr::null_mut();
            }
            d.used = used;
            d.processed = 0;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dirfd(dirp: *mut Dir) -> c_int {
    (*dirp).fd
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn closedir(dirp: *mut Dir) -> c_int {
    let fd = (*dirp).fd;
    free((*dirp).buffer.cast());
    free((*dirp).entry.cast());
    free(dirp.cast());
    close(fd)
}
