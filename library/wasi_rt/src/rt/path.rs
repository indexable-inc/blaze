//! Preopened directories, the current directory, and how a path becomes a directory descriptor
//! and a path relative to it (wasi-libc's preopens.c, chdir.c, getcwd.c and at_fdcwd.c).
//!
//! WASI opens nothing by absolute path: every call takes a directory descriptor. The host hands
//! the program preopened directories, each with a name (fds 3 and up, read with
//! `fd_prestat_get`). A path is made absolute against the cwd, which starts as `/` and is only
//! ever text here, then resolved to the preopen whose name is its longest whole-component
//! prefix; the rest of the path is relative to that descriptor.

use core::ffi::{c_char, c_int};
use core::ptr;

use libc::{AT_FDCWD, EINVAL, ENOENT, ENOMEM, ENOTDIR, ERANGE};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::malloc::{free, malloc, realloc};
use crate::rt::mem::dup;
use crate::{EX_OSERR, EX_SOFTWARE, Global, cstr, decimal, die, fail};

/// Bytes that live in the runtime's state or the caller's argument: valid until the next path
/// resolution with the same slot, a `chdir`, or the caller's string going away.
#[derive(Clone, Copy)]
pub(crate) struct Bytes {
    ptr: *const u8,
    len: usize,
}

impl Bytes {
    pub(crate) fn of(bytes: &[u8]) -> Bytes {
        Bytes { ptr: bytes.as_ptr(), len: bytes.len() }
    }

    pub(crate) unsafe fn get<'a>(self) -> &'a [u8] {
        core::slice::from_raw_parts(self.ptr, self.len)
    }

    /// The (pointer, length) a WASI path parameter takes.
    pub(crate) fn wasi(self) -> (usize, usize) {
        (self.ptr as usize, self.len)
    }
}

struct Preopen {
    fd: c_int,
    /// The name, without leading `/` and `./` (so `/` and `.` are both empty), `malloc`ed with a
    /// NUL after it.
    name: *mut u8,
    len: usize,
}

/// A growable `malloc`ed buffer.
struct Buf {
    ptr: *mut u8,
    cap: usize,
}

impl Buf {
    const EMPTY: Buf = Buf { ptr: ptr::null_mut(), cap: 0 };

    unsafe fn reserve(&mut self, cap: usize) -> Result<*mut u8, c_int> {
        if cap > self.cap {
            let p = realloc(self.ptr.cast(), cap).cast::<u8>();
            if p.is_null() {
                return Err(ENOMEM);
            }
            self.ptr = p;
            self.cap = cap;
        }
        Ok(self.ptr)
    }
}

/// A descriptor `open` made, with the absolute path it opened: the seed's own calls
/// ([`crate::rt::seed`]) take absolute paths, so an `*at` call under a directory descriptor
/// names its path from here ([`absolute_at`]).
struct Named {
    fd: c_int,
    /// `malloc`ed, no NUL.
    path: *mut u8,
    len: usize,
}

struct State {
    preopens: *mut Preopen,
    len: usize,
    cap: usize,
    named: *mut Named,
    named_len: usize,
    named_cap: usize,
    populated: bool,
    /// The cwd, absolute, `malloc`ed with a NUL after it; null means `/`.
    cwd: *mut u8,
    cwd_len: usize,
    /// Where relative paths are made absolute; two, for the calls that take two paths.
    scratch: [Buf; 2],
}

static STATE: Global<State> = Global::new(State {
    preopens: ptr::null_mut(),
    len: 0,
    cap: 0,
    named: ptr::null_mut(),
    named_len: 0,
    named_cap: 0,
    populated: false,
    cwd: ptr::null_mut(),
    cwd_len: 0,
    scratch: [Buf::EMPTY, Buf::EMPTY],
});

unsafe fn state() -> &'static mut State {
    &mut *STATE.get()
}

unsafe fn preopens() -> &'static [Preopen] {
    let s = state();
    if s.len == 0 { &[] } else { core::slice::from_raw_parts(s.preopens, s.len) }
}

unsafe fn cwd() -> &'static [u8] {
    let s = state();
    if s.cwd.is_null() { b"/" } else { core::slice::from_raw_parts(s.cwd, s.cwd_len) }
}

/// Leading `/` and `./` removed, and `.` made empty.
fn strip_prefixes(mut path: &[u8]) -> &[u8] {
    loop {
        match path {
            [b'/', rest @ ..] => path = rest,
            [b'.', b'/', rest @ ..] => path = rest,
            [b'.'] => path = &[],
            _ => return path,
        }
    }
}

/// Adds `fd` named `name` (a `malloc`ed string of `len` bytes we now own) to the table.
unsafe fn add(fd: c_int, name: *mut u8, len: usize) -> Result<(), c_int> {
    let full = core::slice::from_raw_parts(name, len);
    let stripped = strip_prefixes(full);
    let skip = len - stripped.len();
    ptr::copy(name.add(skip), name, stripped.len());
    *name.add(stripped.len()) = 0;
    let s = state();
    if s.len == s.cap {
        let cap = if s.cap == 0 { 4 } else { s.cap * 2 };
        let p = realloc(s.preopens.cast(), cap * size_of::<Preopen>()).cast::<Preopen>();
        if p.is_null() {
            free(name.cast());
            return Err(ENOMEM);
        }
        s.preopens = p;
        s.cap = cap;
    }
    s.preopens.add(s.len).write(Preopen { fd, name, len: stripped.len() });
    s.len += 1;
    Ok(())
}

/// Reads the host's preopens, once, before the first path is resolved or descriptor closed.
unsafe fn populate() {
    if state().populated {
        return;
    }
    state().populated = true;
    let mut n = [0; 20];
    let mut fd: c_int = 3;
    loop {
        match wasip1::fd_prestat_get(fd as u32) {
            Ok(stat) => {
                if stat.tag == wasip1::PREOPENTYPE_DIR.raw() {
                    let len = stat.u.dir.pr_name_len;
                    let name = malloc(len.saturating_add(1)).cast::<u8>();
                    if name.is_null() {
                        die(
                            &[b"wasi_rt: out of memory reading the name of preopened fd ", decimal(fd as u64, &mut n), b"\n"],
                            EX_SOFTWARE,
                        );
                    }
                    if let Err(e) = wasip1::fd_prestat_dir_name(fd as u32, name, len) {
                        die(
                            &[
                                b"wasi_rt: fd_prestat_dir_name of preopened fd ",
                                decimal(fd as u64, &mut n),
                                b" failed: ",
                                e.name().as_bytes(),
                                b"\n",
                            ],
                            EX_OSERR,
                        );
                    }
                    if add(fd, name, len).is_err() {
                        die(&[b"wasi_rt: out of memory listing preopened directories\n"], EX_SOFTWARE);
                    }
                }
            }
            Err(e) if e == wasip1::ERRNO_BADF => return,
            Err(e) => die(
                &[b"wasi_rt: fd_prestat_get of fd ", decimal(fd as u64, &mut n), b" failed: ", e.name().as_bytes(), b"\n"],
                EX_OSERR,
            ),
        }
        fd += 1;
    }
}

/// Whether `prefix` (a stripped preopen name) is a whole-component prefix of `path` (which has
/// no leading `/`).
fn prefix_matches(prefix: &[u8], path: &[u8]) -> bool {
    // An empty name (`/` or `.`) contains every relative path.
    if prefix.is_empty() && path.first() != Some(&b'/') {
        return true;
    }
    if path.len() < prefix.len() || &path[..prefix.len()] != prefix {
        return false;
    }
    let mut i = prefix.len();
    while i > 0 && prefix[i - 1] == b'/' {
        i -= 1;
    }
    matches!(path.get(i), None | Some(b'/'))
}

/// A path resolved to a preopen.
pub(crate) struct Found {
    pub(crate) fd: c_int,
    /// The matching preopen's name.
    pub(crate) prefix: Bytes,
    /// The rest of the path, relative to `fd`; `.` for the preopen itself.
    pub(crate) rel: Bytes,
}

/// The preopen that holds absolute `path`: the longest matching name, the latest added on a tie.
unsafe fn find_abspath(path: &[u8]) -> Result<Found, c_int> {
    let mut path = path;
    while let [b'/', rest @ ..] = path {
        path = rest;
    }
    let mut best: Option<&Preopen> = None;
    for pre in preopens().iter().rev() {
        if best.is_none_or(|b| pre.len > b.len)
            && prefix_matches(core::slice::from_raw_parts(pre.name, pre.len), path)
        {
            best = Some(pre);
        }
    }
    let pre = best.ok_or(ENOENT)?;
    let mut rel = &path[pre.len..];
    while let [b'/', rest @ ..] = rel {
        rel = rest;
    }
    // WASI's path calls take no empty path.
    if rel.is_empty() {
        rel = b".";
    }
    Ok(Found { fd: pre.fd, prefix: Bytes { ptr: pre.name, len: pre.len }, rel: Bytes::of(rel) })
}

/// `path` made absolute against the cwd, in scratch buffer `slot` when it has to be built.
unsafe fn make_absolute(path: &[u8], slot: usize) -> Result<Bytes, c_int> {
    if path.first() == Some(&b'/') {
        return Ok(Bytes::of(path));
    }
    if path == b"." || path == b"./" {
        return Ok(Bytes::of(cwd()));
    }
    let path = path.strip_prefix(b"./").unwrap_or(path);
    let cwd = cwd();
    let slash = usize::from(cwd.last() != Some(&b'/'));
    let len = cwd.len() + slash + path.len();
    let buf = state().scratch[slot].reserve(len)?;
    ptr::copy_nonoverlapping(cwd.as_ptr(), buf, cwd.len());
    if slash == 1 {
        *buf.add(cwd.len()) = b'/';
    }
    ptr::copy_nonoverlapping(path.as_ptr(), buf.add(cwd.len() + slash), path.len());
    Ok(Bytes { ptr: buf, len })
}

/// Resolves `path` (relative to the cwd or absolute) to a preopen and a path under it. `slot`
/// (0 or 1) picks the scratch buffer, so a call can hold two resolved paths. An empty path is
/// `ENOENT`, as POSIX has it.
pub(crate) unsafe fn resolve(path: *const c_char, slot: usize) -> Result<Found, c_int> {
    let path = cstr(path);
    if path.is_empty() {
        return Err(ENOENT);
    }
    populate();
    let abs = make_absolute(path, slot)?;
    find_abspath(abs.get())
}

/// The descriptor and relative path an `*at` call works on: `path` itself under `dirfd`, or,
/// for `AT_FDCWD` or an absolute path, its resolution through the preopens.
pub(crate) unsafe fn at(dirfd: c_int, path: *const c_char, slot: usize) -> Result<(c_int, Bytes), c_int> {
    let bytes = cstr(path);
    if bytes.is_empty() {
        return Err(ENOENT);
    }
    if dirfd == AT_FDCWD || bytes[0] == b'/' {
        let found = resolve(path, slot)?;
        Ok((found.fd, found.rel))
    } else {
        Ok((dirfd, Bytes::of(bytes)))
    }
}

/// The absolute path an `*at` call names, for the seed's own calls: `path` against the cwd for
/// `AT_FDCWD` or an absolute `path`, else `path` under `dirfd`'s path, a preopen's name or the
/// path `open` opened it by. In scratch buffer `slot` when it has to be built. A `dirfd` this
/// runtime has no path of (not opened by path, or closed) is `ENOTDIR`. An empty path is
/// `ENOENT`.
pub(crate) unsafe fn absolute_at(dirfd: c_int, path: *const c_char, slot: usize) -> Result<Bytes, c_int> {
    let bytes = cstr(path);
    if bytes.is_empty() {
        return Err(ENOENT);
    }
    if dirfd == AT_FDCWD || bytes[0] == b'/' {
        return make_absolute(bytes, slot);
    }
    populate();
    let (lead, base): (&[u8], &[u8]) = match preopens().iter().rev().find(|it| it.fd == dirfd) {
        Some(pre) => (b"/", core::slice::from_raw_parts(pre.name, pre.len)),
        None => match named().iter().rev().find(|it| it.fd == dirfd) {
            Some(it) => (b"", core::slice::from_raw_parts(it.path, it.len)),
            None => return Err(ENOTDIR),
        },
    };
    let bytes = bytes.strip_prefix(b"./").unwrap_or(bytes);
    // A `/` between the base and `path` unless the base, `/` and the name, already ends in one.
    let last = base.last().or(lead.last());
    let slash = usize::from(last != Some(&b'/'));
    let len = lead.len() + base.len() + slash + bytes.len();
    let buf = state().scratch[slot].reserve(len)?;
    ptr::copy_nonoverlapping(lead.as_ptr(), buf, lead.len());
    ptr::copy_nonoverlapping(base.as_ptr(), buf.add(lead.len()), base.len());
    if slash == 1 {
        *buf.add(lead.len() + base.len()) = b'/';
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(lead.len() + base.len() + slash), bytes.len());
    Ok(Bytes { ptr: buf, len })
}

unsafe fn named() -> &'static [Named] {
    let s = state();
    if s.named_len == 0 { &[] } else { core::slice::from_raw_parts(s.named, s.named_len) }
}

/// `open` made `fd` by the absolute `path`: kept for [`absolute_at`] until `fd` is closed.
pub(crate) unsafe fn opened(fd: c_int, path: Bytes) -> Result<(), c_int> {
    let path = path.get();
    let copy = malloc(path.len().max(1)).cast::<u8>();
    if copy.is_null() {
        return Err(ENOMEM);
    }
    ptr::copy_nonoverlapping(path.as_ptr(), copy, path.len());
    let s = state();
    if s.named_len == s.named_cap {
        let cap = if s.named_cap == 0 { 16 } else { s.named_cap * 2 };
        let p = realloc(s.named.cast(), cap * size_of::<Named>()).cast::<Named>();
        if p.is_null() {
            free(copy.cast());
            return Err(ENOMEM);
        }
        s.named = p;
        s.named_cap = cap;
    }
    s.named.add(s.named_len).write(Named { fd, path: copy, len: path.len() });
    s.named_len += 1;
    Ok(())
}

/// Lets a program name a descriptor as a preopen itself.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasilibc_register_preopened_fd(fd: c_int, path: *const c_char) -> c_int {
    populate();
    let bytes = cstr(path);
    let name = dup(bytes).cast::<u8>();
    if name.is_null() {
        return -1;
    }
    match add(fd, name, bytes.len()) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

/// Called before `fd` is closed: the preopens must be read first, so the host's list is seen
/// whole.
pub(crate) unsafe fn before_close() {
    populate();
}

/// `fd` is closed: it is no longer a preopen (wasi-libc keeps it, so a later descriptor that
/// reuses the number would be taken for the directory).
pub(crate) unsafe fn closed(fd: c_int) {
    let s = state();
    let mut i = 0;
    while i < s.named_len {
        let it = s.named.add(i);
        if (*it).fd == fd {
            free((*it).path.cast());
            ptr::copy(s.named.add(i + 1), it, s.named_len - i - 1);
            s.named_len -= 1;
        } else {
            i += 1;
        }
    }
    let mut i = 0;
    while i < s.len {
        let pre = s.preopens.add(i);
        if (*pre).fd == fd {
            free((*pre).name.cast());
            ptr::copy(s.preopens.add(i + 1), pre, s.len - i - 1);
            s.len -= 1;
        } else {
            i += 1;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    let cwd = cwd();
    if buf.is_null() {
        return dup(cwd);
    }
    if size == 0 {
        fail(EINVAL);
        return ptr::null_mut();
    }
    if size < cwd.len() + 1 {
        fail(ERANGE);
        return ptr::null_mut();
    }
    ptr::copy_nonoverlapping(cwd.as_ptr(), buf.cast::<u8>(), cwd.len());
    *buf.add(cwd.len()) = 0;
    buf
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn chdir(path: *const c_char) -> c_int {
    let found = match resolve(path, 0) {
        Ok(found) => found,
        Err(e) => return fail(e),
    };
    let mut stat = core::mem::MaybeUninit::<wasip1::Filestat>::uninit();
    let (rel_ptr, rel_len) = found.rel.wasi();
    let r = wasi::path_filestat_get(
        found.fd,
        wasip1::LOOKUPFLAGS_SYMLINK_FOLLOW as i32,
        rel_ptr,
        rel_len,
        stat.as_mut_ptr() as usize,
    );
    if r != 0 {
        return fail(r);
    }
    if stat.assume_init().filetype != wasip1::FILETYPE_DIRECTORY {
        return fail(ENOTDIR);
    }
    // The new cwd is "/" + prefix, then "/" + rel unless rel is "." (and no "/" when prefix is
    // empty). Built before the old one is freed: prefix or rel may point into it.
    let prefix = found.prefix.get();
    let rel = found.rel.get();
    let with_rel = rel != b".";
    let mid = usize::from(with_rel && !prefix.is_empty());
    let len = 1 + prefix.len() + mid + if with_rel { rel.len() } else { 0 };
    let new = malloc(len + 1).cast::<u8>();
    if new.is_null() {
        return -1;
    }
    *new = b'/';
    ptr::copy_nonoverlapping(prefix.as_ptr(), new.add(1), prefix.len());
    if mid == 1 {
        *new.add(1 + prefix.len()) = b'/';
    }
    if with_rel {
        ptr::copy_nonoverlapping(rel.as_ptr(), new.add(1 + prefix.len() + mid), rel.len());
    }
    *new.add(len) = 0;
    let s = state();
    let old = s.cwd;
    s.cwd = new;
    s.cwd_len = len;
    free(old.cast());
    0
}
