//! The requests on paths, each through the ring: every open, `statx`, `mkdir`, unlink, link
//! and rename is a request of the kernel's, none a call on the driver's thread, so no
//! effect waits on a lookup, a disk or a network file system while others are ready.
//!
//! A write at an offset, a whole file's write and a rename open what they write and sync
//! with `IORING_OP_OPENAT` into slots of the driver's half of the table, linked
//! (`IOSQE_IO_LINK`) ahead of the requests that name those slots (`IOSQE_FIXED_FILE`). The
//! driver picks the slots, not the kernel (`IORING_FILE_INDEX_ALLOC`): a link after an open
//! is written before the open runs, so it must name the slot before the kernel would say
//! which. Each slot is emptied with `IORING_OP_CLOSE` once the request ends. A kept file is
//! opened to a descriptor of its own (`ReadAll`'s `fstat` and `Truncate`'s `ftruncate` take
//! one) and put in its slot with `IORING_OP_FILES_UPDATE`; the requests on it wait for that.
//!
//! Where an open (or a link, or a clone's target) finds a directory missing and the effect
//! makes it, the request makes each directory of its path in turn (`IORING_OP_MKDIRAT`, an
//! existing one answering `EEXIST`), as `create_dir_all` does, then asks its chain again; a
//! second `ENOENT` fails it. The kernel reads every name, and writes every `statx`, in the
//! request's arena of [`ops::Paths`], which never moves and is never read as a slice.

use std::{
    ffi::CStr,
    fs::File,
    mem,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        unix::ffi::OsStrExt as _,
    },
};

use blaze_mem::RegionVec;

use super::{
    flight::{error, failed, filled, raw, spare_of},
    ops::{DIRECT, LINKS, MADE, MAKE, NO_SLOT, SECOND, WHOLE, arena_of},
    *,
};

/// Its opens, and what follows them in its chain.
const OPENING: u8 = 0;
/// What follows its opens alone, asked again after a short write.
const OPENED: u8 = 1;
/// Making the directories its path is in, one at a time.
const MAKING: u8 = 2;
/// A write at an offset's `statx`, which says whether it may bypass the page cache.
const STATTING: u8 = 3;
/// Unlinking what it made before it failed: a whole file's temporary, a clone's target.
const UNLINKING: u8 = 4;
/// A kept file's descriptor being put in its slot of the table.
const INSTALLING: u8 = 5;
/// A remove's `rmdir`, after its unlink found a directory.
const EMPTYING: u8 = 6;
/// A whole file's read whose buffer is the file's size.
pub(super) const SIZED: u8 = 1;
/// A read of a file no request keeps, open, reading into its buffer.
const READING: u8 = 7;
/// A read of a file no request keeps, waiting for room among [`LOADS`].
const QUEUED: u8 = 8;

/// The most reads of files no request keeps that hold a descriptor at once (fewer where the
/// table, sized to `RLIMIT_NOFILE`, is smaller): a batch of 50 000 opens whose reads wait
/// behind them in the ring would otherwise hold 50 000 descriptors and fail with `EMFILE`.
/// Each read is two trips through the ring of a few microseconds, so a thousand in flight is
/// far more than the submission thread and the device keep busy.
pub(super) const LOADS: usize = 1024;
/// Reads in flight past which each one's open and read go to the kernel's workers
/// (`IOSQE_ASYNC`) rather than run on the ring's one submission thread: in a batch that large
/// what is waited for is the batch, and the path lookups and page-cache copies (~3 µs a file
/// of 20 KiB) then run on every core instead of one. Below it a lone read keeps the shorter
/// path, the ~2 µs hand-off to a worker costing more than it spreads.
pub(super) const SPREAD_LEAST: usize = 64;

/// `struct statx`'s size, and the mask of what [`Kind::Stat`] asks of it: the type, the mode,
/// the modification time and the size.
const STATX_SIZE: usize = 256;
const STATX_META: u32 = 0x0001 | 0x0002 | 0x0040 | 0x0200;
const STATX_MTIME: u32 = 0x0040;
/// How a kept file is opened: for reading and appending.
const KEEP: i32 = libc::O_RDWR | libc::O_APPEND;
/// How a directory is opened to be synced.
const DIR: i32 = libc::O_RDONLY | libc::O_DIRECTORY;

/// The open of `path` with `flags` and `mode`, into `slot` of the table (no `O_CLOEXEC`: the
/// kernel refuses it for a slot, which no `exec` inherits) or, with [`NO_SLOT`], to a
/// descriptor of the driver's own.
fn open(path: *const u8, flags: i32, mode: u32, slot: u32, link: u8, user_data: u64) -> Sqe {
    let (flags, index) = match slot {
        NO_SLOT => (flags | libc::O_CLOEXEC, 0),
        slot => (flags, slot + 1),
    };
    Sqe {
        opcode: OP_OPENAT,
        flags: link,
        fd: libc::AT_FDCWD,
        addr: path as u64,
        len: mode,
        op_flags: flags as u32,
        // `file_index`: the slot plus one, 0 for none.
        splice_fd_in: index as i32,
        user_data,
        ..Sqe::default()
    }
}

/// `opcode` on the file in `slot`.
fn on_slot(opcode: u8, slot: u32, link: u8, user_data: u64) -> Sqe {
    Sqe { opcode, flags: SQE_FIXED_FILE | link, fd: slot as i32, user_data, ..Sqe::default() }
}

fn unlink(path: *const u8, flags: i32, user_data: u64) -> Sqe {
    Sqe {
        opcode: OP_UNLINKAT,
        fd: libc::AT_FDCWD,
        addr: path as u64,
        op_flags: flags as u32,
        user_data,
        ..Sqe::default()
    }
}

fn statx(path: *const u8, mask: u32, into: *const u8, user_data: u64) -> Sqe {
    Sqe {
        opcode: OP_STATX,
        fd: libc::AT_FDCWD,
        addr: path as u64,
        len: mask,
        // The `struct statx` written.
        off: into as u64,
        user_data,
        ..Sqe::default()
    }
}

/// `from` renamed over `to` (`linked` the link to the next), or with `opcode` [`OP_LINKAT`]
/// linked as it: each NUL-terminated, relative to the working directory.
fn two_names(opcode: u8, from: *const u8, to: *const u8, link: u8, user_data: u64) -> Sqe {
    Sqe {
        opcode,
        flags: link,
        fd: libc::AT_FDCWD,
        addr: from as u64,
        // The new name's directory, and the new name.
        len: libc::AT_FDCWD as u32,
        off: to as u64,
        user_data,
        ..Sqe::default()
    }
}

/// Which name of a request of `kind` the directories it makes are those of.
fn made_for(kind: Kind) -> usize {
    match kind {
        Kind::Open => 0,
        _ => 1,
    }
}

/// The `u32` at `at` of a `struct statx`.
fn word(statx: &[u8; STATX_SIZE], at: usize) -> u32 {
    u32::from_ne_bytes(statx[at..at + 4].try_into().expect("four bytes"))
}

/// `bytes`, a path, NUL-terminated at the end of `into`; where it starts.
fn push_c(into: &mut RegionVec<u8>, bytes: &[u8]) -> io::Result<u32> {
    if bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"));
    }
    let at = into.len() as u32;
    put(into, bytes)?;
    put(into, &[0])?;
    Ok(at)
}

/// `n` zero bytes at the end of `into`, for the kernel to write; where they start.
fn push_zeros(into: &mut RegionVec<u8>, n: usize) -> io::Result<u32> {
    let at = into.len() as u32;
    put(into, &[0; STATX_SIZE][..n])?;
    Ok(at)
}

/// `bytes` at the end of the arena `into`, or an error naming it where it is full.
fn put(into: &mut RegionVec<u8>, bytes: &[u8]) -> io::Result<()> {
    into.try_extend_from_slice(bytes).map_err(|error| {
        // alloc: an error path.
        io::Error::other(format!("the names of the requests on paths in flight fill their arena: {error}"))
    })
}

/// `n` in decimal at the end of `into`.
fn put_decimal(into: &mut RegionVec<u8>, mut n: u64) -> io::Result<()> {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    put(into, &digits[at..])
}

/// A name next to `path`'s, at the end of `into` NUL-terminated, that no other write of this
/// process or another uses, as [`crate::fs::temporary_next_to`] makes one.
fn push_temporary(into: &mut RegionVec<u8>, path: &Path) -> io::Result<u32> {
    let parent = path.parent().map_or(&[][..], |it| it.as_os_str().as_bytes());
    let leaf = path.file_name().map_or(&[][..], |it| it.as_bytes());
    if parent.contains(&0) || leaf.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"));
    }
    let at = into.len() as u32;
    if !parent.is_empty() {
        put(into, parent)?;
        put(into, b"/")?;
    }
    let n = TEMPORARY.fetch_add(1, Ordering::Relaxed);
    put(into, b".")?;
    put(into, leaf)?;
    put(into, b".")?;
    put_decimal(into, u64::from(std::process::id()))?;
    put(into, b"-")?;
    put_decimal(into, n)?;
    put(into, b".tmp")?;
    put(into, &[0])?;
    Ok(at)
}

/// The directory `path` is in, NUL-terminated at the end of `into`; where it starts.
fn push_dir_of(into: &mut RegionVec<u8>, path: &Path) -> io::Result<u32> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => push_c(into, parent.as_os_str().as_bytes()),
        _ => push_c(into, b"."),
    }
}

impl Uring {
    /// The request `hot`, whose names `push` puts in an arena, lending the kernel `buf`, its
    /// `cold` record given its names' starts; an error, and nothing started, where they do
    /// not fit.
    fn named(
        &mut self,
        mut hot: Hot,
        buf: Vec<u8>,
        mut cold: Cold,
        push: impl FnOnce(&Path, &mut RegionVec<u8>, &mut [u32; 5]) -> io::Result<()>,
        name: &FileName,
    ) -> io::Result<Id> {
        let arena = self.paths.begin();
        let names = self.paths.arena(arena);
        let from = names.len();
        if let Err(error) = under(&self.root, name, |path| push(path, names, &mut cold.names)) {
            names.truncate(from);
            return Err(error);
        }
        self.paths.hold(arena);
        if arena == 1 {
            hot.flags |= SECOND;
        }
        Ok(self.start(hot, buf, cold))
    }

    /// The `struct statx` the request `hot` (`id`) asked into its arena, once it completed.
    fn statx_of(&self, id: Id, hot: &Hot) -> [u8; STATX_SIZE] {
        let at = self.paths.at(arena_of(hot), self.ops.cold(id).names[4]).cast_const();
        // SAFETY: the `STATX_SIZE` bytes the request pushed at `at` and holds until it ends; its
        // `statx` has completed, so the kernel no longer writes them.
        unsafe { at.cast::<[u8; STATX_SIZE]>().read_unaligned() }
    }

    /// The name `n` of the request `id`. Each caller asks for a name its kind's push put there
    /// (0 and 1 the paths, 2 and 3 a rename's directories) or, for 4, the directory
    /// `next_parent` put there before the `made` that asks; never `write_at`'s `statx` room,
    /// which only `statx_of` reads.
    fn name_of(&self, id: Id, n: usize) -> &CStr {
        let hot = self.ops.get(id).expect("a request in flight");
        // SAFETY: as above, `names[n]` starts a NUL-terminated name in the request's arena,
        // which it holds while in flight. An index its push left unset is 0, which is still a
        // name: every request pushes a path first, so the arena's first bytes are one, kept
        // until no request holds the arena.
        unsafe { self.paths.name(arena_of(hot), self.ops.cold(id).names[n]) }
    }

    /// Why the request `id` failed with `res` doing `what` to its name `n`.
    fn failed_on(&self, id: Id, what: &str, n: usize, res: i32) -> Step {
        let name = self.name_of(id, n).to_string_lossy();
        // alloc: an error path.
        Step::Ends(Some(Done::Failed(IoError(format!("{what} {name}: {}", error(res))))))
    }

    /// Starts [`Effect::WriteAt`] `tag` of `bytes` at `at` of `file`: a `statx` first where
    /// the write may bypass the page cache, then the open into a slot linked to the write.
    pub(super) fn write_at(&mut self, tag: Tag, file: &FileName, at: u64, bytes: Vec<u8>) -> io::Result<()> {
        const SECTOR: u64 = 512;
        let len = bytes.len();
        let candidate = len >= DIRECT_LEAST && at % SECTOR == 0 && len as u64 % SECTOR == 0;
        let mut hot = Hot::new(tag, Kind::WriteAt, 0);
        hot.phase = if candidate { STATTING } else { OPENING };
        let cold = Cold { at, ..Cold::EMPTY };
        self.named(
            hot,
            bytes,
            cold,
            |path, names, at| {
                at[0] = push_c(names, path.as_os_str().as_bytes())?;
                if candidate {
                    at[4] = push_zeros(names, STATX_SIZE)?;
                }
                Ok(())
            },
            file,
        )?;
        Ok(())
    }

    /// Starts [`Effect::Write`] `tag` of `bytes` over `file`: the chain that opens its
    /// directory, makes its temporary next to it, writes, syncs and renames it over the file
    /// and syncs the directory.
    pub(super) fn write(&mut self, tag: Tag, file: FileName, bytes: Vec<u8>) -> io::Result<()> {
        let mut hot = Hot::new(tag, Kind::Write, 0);
        hot.flags |= MAKE;
        let push = |path: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_temporary(names, path)?;
            at[1] = push_c(names, path.as_os_str().as_bytes())?;
            at[2] = push_dir_of(names, path)?;
            Ok(())
        };
        let id = self.named(hot, bytes, Cold::EMPTY, push, &file)?;
        match self.ops.contains(id) {
            true => self.ops.cold_mut(id).file = Some(file),
            // Refused as it was queued.
            false => self.forget(&file),
        }
        Ok(())
    }

    /// Starts [`Effect::Rename`] `tag` of `from` over `to`, after the opens of the directory
    /// of each and linked to their syncs.
    pub(super) fn rename(&mut self, tag: Tag, from: &FileName, to: &FileName) -> io::Result<()> {
        self.forget(from);
        self.forget(to);
        let mut hot = Hot::new(tag, Kind::Rename, 0);
        hot.flags |= MAKE;
        let root = Arc::clone(&self.root); // clone: a reference count.
        let push = |source: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_c(names, source.as_os_str().as_bytes())?;
            at[3] = push_dir_of(names, source)?;
            under(&root, to, |target| {
                at[1] = push_c(names, target.as_os_str().as_bytes())?;
                match source.parent() == target.parent() {
                    true => at[2] = at[3],
                    false => at[2] = push_dir_of(names, target)?,
                }
                Ok(())
            })
        };
        self.named(hot, ops::NO_BYTES, Cold::EMPTY, push, from)?;
        Ok(())
    }

    /// Starts [`Effect::Remove`] `tag` of `file`.
    pub(super) fn remove(&mut self, tag: Tag, file: &FileName) -> io::Result<()> {
        self.forget(file);
        let push = |path: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_c(names, path.as_os_str().as_bytes())?;
            Ok(())
        };
        self.named(Hot::new(tag, Kind::Remove, 0), ops::NO_BYTES, Cold::EMPTY, push, file)?;
        Ok(())
    }

    /// Starts [`Effect::Link`] `tag` (with `kind` [`Kind::Link`]) or [`Effect::Clone`] (with
    /// [`Kind::Clone`]) of `from` as `to`.
    pub(super) fn copy(&mut self, tag: Tag, kind: Kind, from: &FileName, to: &FileName) -> io::Result<()> {
        self.forget(to);
        let mut hot = Hot::new(tag, kind, 0);
        hot.flags |= MAKE;
        let root = Arc::clone(&self.root); // clone: a reference count.
        let push = |source: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_c(names, source.as_os_str().as_bytes())?;
            under(&root, to, |target| {
                at[1] = push_c(names, target.as_os_str().as_bytes())?;
                Ok(())
            })
        };
        self.named(hot, ops::NO_BYTES, Cold::EMPTY, push, from)?;
        Ok(())
    }

    /// Starts [`Effect::Map`] (with `kind` [`Kind::Map`]) or [`Effect::Stat`] (with
    /// [`Kind::Stat`]) `tag` of `file`.
    pub(super) fn look(&mut self, tag: Tag, kind: Kind, file: &FileName) -> io::Result<()> {
        let push = |path: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_c(names, path.as_os_str().as_bytes())?;
            if kind == Kind::Stat {
                at[4] = push_zeros(names, STATX_SIZE)?;
            }
            Ok(())
        };
        self.named(Hot::new(tag, kind, 0), ops::NO_BYTES, Cold::EMPTY, push, file)?;
        Ok(())
    }

    /// Starts [`Effect::Read`] (`len`) or, with `None`, [`Effect::ReadAll`] `tag` of `file`,
    /// which no request keeps: a read of the file at its name now, open only while it reads.
    pub(super) fn load(&mut self, tag: Tag, file: &FileName, at: u64, len: Option<usize>) -> io::Result<()> {
        let mut hot = Hot::new(tag, Kind::Load, 0);
        hot.phase = QUEUED;
        let buf = match len {
            // alloc: the read's result, handed to its asker.
            Some(len) => Vec::with_capacity(len),
            None => {
                hot.flags |= WHOLE;
                ops::NO_BYTES
            }
        };
        let push = |path: &Path, names: &mut RegionVec<u8>, starts: &mut [u32; 5]| {
            starts[0] = push_c(names, path.as_os_str().as_bytes())?;
            Ok(())
        };
        self.named(hot, buf, Cold { at, ..Cold::EMPTY }, push, file)?;
        Ok(())
    }

    /// The most reads of files no request keeps that may hold a descriptor at once.
    pub(super) fn loads_most(&self) -> usize {
        (self.table.ours as usize / 2).clamp(1, LOADS)
    }

    /// Whether the request `id` may be asked of the kernel as far as the reads holding
    /// descriptors go: a read not yet among them joins them where there is room, or waits, in
    /// the order asked, for one to end.
    pub(super) fn admitted(&mut self, id: Id) -> bool {
        let hot = self.ops.get(id).expect("a request in flight");
        if hot.kind != Kind::Load || hot.phase != QUEUED {
            return true;
        }
        if self.loading >= self.loads_most() {
            // alloc: none until more reads wait than the ring holds, then amortized growth.
            self.load_waiting.push_back(id);
            return false;
        }
        self.loading += 1;
        self.ops.get_mut(id).expect("in flight").phase = OPENING;
        true
    }

    /// A read that held a descriptor ended: the one that has waited longest takes its room.
    fn unload(&mut self) {
        self.loading -= 1;
        while let Some(id) = self.load_waiting.pop_front() {
            if self.ops.contains(id) {
                self.queue(id);
                return;
            }
        }
    }

    /// The read of a file no request keeps `id` (`hot`): its open completed with `res`, or
    /// its read did.
    fn loaded(&mut self, id: Id, hot: Hot, res: i32) -> Step {
        let whole = hot.flags & WHOLE != 0;
        if hot.phase == OPENING {
            // A file that is not there reads as empty, as a journal not yet written does.
            if res == -libc::ENOENT && whole {
                return Step::Ends(Some(Done::Read(ops::NO_BYTES)));
            }
            if res < 0 {
                return self.failed_on(id, "opening", 0, res);
            }
            // SAFETY: the open's completion: a descriptor of the driver's, owned by no other.
            let fd = unsafe { OwnedFd::from_raw_fd(res) };
            // Its size as the file now open has it, so a rename over the name meanwhile never
            // sizes the read of the file it replaced. No ring op `fstat`s a descriptor at 6.0.
            if whole {
                match size_of_file(fd.as_raw_fd()) {
                    // alloc: the read's result, handed to its asker.
                    Ok(size) => *self.ops.buf_mut(id) = Vec::with_capacity(size as usize),
                    Err(error) => return Step::Ends(Some(Done::Failed(IoError(error.to_string())))), // alloc: an error.
                }
            }
            self.ops.cold_mut(id).fds[0] = Some(fd);
            if self.ops.buf(id).capacity() == 0 {
                return Step::Ends(Some(Done::Read(mem::take(self.ops.buf_mut(id)))));
            }
            self.ops.get_mut(id).expect("in flight").phase = READING;
            return Step::Again;
        }
        if res < 0 {
            return Step::Ends(failed(res));
        }
        let bytes = self.ops.buf_mut(id);
        if res > 0 && filled(bytes, res) {
            return Step::Again;
        }
        Step::Ends(Some(Done::Read(mem::take(bytes))))
    }

    /// Starts the open of the kept file `file`, named `name`, made if missing with `make`.
    pub(super) fn open(&mut self, file: Handle<Kept>, name: &FileName, make: bool) -> io::Result<()> {
        let mut hot = Hot::new(NOBODYS, Kind::Open, file.to_bits());
        if make {
            hot.flags |= MAKE;
        }
        let push = |path: &Path, names: &mut RegionVec<u8>, at: &mut [u32; 5]| {
            at[0] = push_c(names, path.as_os_str().as_bytes())?;
            Ok(())
        };
        self.named(hot, ops::NO_BYTES, Cold::EMPTY, push, name)?;
        Ok(())
    }

    /// The slots of the table every request whose last link ended let go are emptied through
    /// the ring: those of kept files and connections dropped since ([`Fixed`]'s drop).
    pub(super) fn drain_closing(&mut self) {
        loop {
            let slot = self.table.closing.borrow_mut().pop();
            let Some(slot) = slot else { return };
            self.close(slot);
        }
    }

    fn close(&mut self, slot: u32) {
        self.closes += 1;
        self.start(Hot::new(NOBODYS, Kind::Close, u64::from(slot)), ops::NO_BYTES, Cold::EMPTY);
    }

    /// Whether the request `id` on a kept file may be asked of the kernel: its file open, and a
    /// whole file's read sized. One that waits for its file is asked once it is open
    /// ([`Uring::wake`]); a truncate is done here, on the open descriptor, and ends.
    pub(super) fn prepared(&mut self, id: Id) -> bool {
        let hot = *self.ops.get(id).expect("a request in flight");
        if !matches!(hot.kind, Kind::Read | Kind::Append | Kind::Sync | Kind::Truncate) {
            return true;
        }
        let kept = self.kept.get(Handle::from_bits(hot.of)).expect("held by its request");
        if !kept.open {
            // alloc: none past the capacity made with the driver, a request each at most.
            self.on_open.push(id);
            return false;
        }
        let fd = kept.fixed.fd();
        let done = match hot.kind {
            // No io_uring op cuts a file below Linux 6.9 (`IORING_OP_FTRUNCATE`): one
            // `ftruncate` of the descriptor already open, here.
            Kind::Truncate => {
                let len = self.ops.cold(id).at;
                // SAFETY: an open descriptor; no pointers.
                match unsafe { libc::ftruncate(fd, len as libc::off_t) } {
                    0 => Done::Truncated,
                    // alloc: an error.
                    _ => Done::Failed(IoError(io::Error::last_os_error().to_string())),
                }
            }
            Kind::Read if hot.flags & WHOLE != 0 && hot.phase != SIZED => {
                self.ops.get_mut(id).expect("in flight").phase = SIZED;
                match size_of_file(fd) {
                    Ok(0) => Done::Read(mem::take(self.ops.buf_mut(id))),
                    Ok(size) => {
                        // alloc: the read's result, handed to its asker.
                        *self.ops.buf_mut(id) = Vec::with_capacity(size as usize);
                        return true;
                    }
                    Err(error) => Done::Failed(IoError(error.to_string())), // alloc: an error.
                }
            }
            _ => return true,
        };
        self.ready.push(Completion { tag: hot.tag, done });
        self.end(id);
        false
    }

    /// Whether the request `id` holds the slots of the table its opens need, given them if it
    /// needs them and they are free. One that finds too few waits for the next emptied, the
    /// unnamed kept files let go to make room; one that no request in flight or file kept
    /// could make room for fails, naming why.
    pub(super) fn slotted(&mut self, id: Id) -> bool {
        let hot = self.ops.get(id).expect("a request in flight");
        let cold = self.ops.cold(id);
        let need = match (hot.kind, hot.phase) {
            (Kind::WriteAt, OPENING) | (Kind::Open, INSTALLING) => 1,
            (Kind::Write, OPENING) => 2,
            (Kind::Rename, OPENING) => 1 + usize::from(cold.names[3] != cold.names[2]),
            _ => 0,
        };
        if need == 0 || cold.slots[0] != NO_SLOT {
            return true;
        }
        if self.table.free.borrow().len() < need {
            self.evict();
        }
        let mut free = self.table.free.borrow_mut();
        if free.len() >= need {
            let cold = self.ops.cold_mut(id);
            for slot in &mut cold.slots[..need] {
                *slot = free.pop().expect("counted");
            }
            drop(free);
            self.slots_held += need;
            return true;
        }
        drop(free);
        if self.closes == 0 && self.slots_held == 0 && self.kept.is_empty() {
            // alloc: an error.
            let why = format!(
                "every one of the {} slots of the ring's table of files the driver keeps is held by a \
                 connection",
                self.table.ours
            );
            self.refused_request(id, &why);
            return false;
        }
        // alloc: none past the capacity made with the driver, a request each at most.
        self.slot_waiting.push_back(id);
        false
    }

    /// Every kept file no request uses let go, and every other unnamed so it goes once its
    /// last request ends: their slots emptied through the ring.
    fn evict(&mut self) {
        for (_, file) in self.names.drain() {
            let kept = self.kept.get_mut(file).expect("named files are kept");
            kept.named = false;
            if kept.users == 0 {
                self.kept.remove(file);
            }
        }
        self.drain_closing();
    }

    /// The slots emptied, for the requests that waited for them, in the order they asked.
    fn unslot(&mut self) {
        for _ in 0..self.slot_waiting.len() {
            if self.table.free.borrow().is_empty() {
                return;
            }
            let Some(id) = self.slot_waiting.pop_front() else { return };
            if self.ops.contains(id) {
                self.queue(id);
            }
        }
    }

    /// The request `id` ends failed with `why` before it was asked of the kernel; a kept
    /// file's open with every request that waits for it.
    fn refused_request(&mut self, id: Id, why: &str) {
        let hot = *self.ops.get(id).expect("a request in flight");
        if hot.kind == Kind::Open {
            self.unkept(Handle::from_bits(hot.of), -libc::ENFILE, why);
        } else {
            // alloc: an error.
            self.ready.push(Completion { tag: hot.tag, done: Done::Failed(IoError(why.to_owned())) });
        }
        self.end(id);
    }

    /// The SQEs the request on a path `id` is, as it now stands, into `sqes`. How many.
    pub(super) fn path_sqes(&self, id: Id, sqes: &mut [Sqe; LINKS]) -> usize {
        let hot = self.ops.get(id).expect("a request in flight");
        let (cold, buf) = (self.ops.cold(id), self.ops.buf(id));
        let name = |n: usize| self.paths.at(arena_of(hot), cold.names[n]).cast_const();
        let ud = |link: usize| data(id, link);
        let done = hot.done as usize;
        // What of its buffer is still to go: where it starts and how long it is.
        let (rest, left) = (buf[done..].as_ptr() as u64, (buf.len() - done).min(u32::MAX as usize) as u32);
        // Every name, buffer and `statx` below stays where it is until the request's last
        // completion: the arenas and the buffer column are reserved memory that never moves,
        // and what a request holds in them stays until it ends (`Uring::end`).
        match (hot.kind, hot.phase) {
            (_, MAKING) => {
                sqes[0] = Sqe {
                    opcode: OP_MKDIRAT,
                    fd: libc::AT_FDCWD,
                    addr: name(4) as u64,
                    len: 0o777,
                    user_data: ud(0),
                    ..Sqe::default()
                };
                1
            }
            (Kind::Close, _) => {
                // `file_index` is the slot plus one; `fd` must be 0 with it.
                sqes[0] = Sqe { opcode: OP_CLOSE, fd: 0, splice_fd_in: hot.of as i32 + 1, user_data: ud(0), ..Sqe::default() };
                1
            }
            (Kind::WriteAt, STATTING) => {
                sqes[0] = statx(name(0), STATX_DIOALIGN, name(4), ud(0));
                1
            }
            (Kind::WriteAt, _) => {
                let slot = cold.slots[0];
                let write = if hot.flags & DIRECT != 0 {
                    debug_assert_ne!(hot.buffer, ops::NO_BUFFER, "a registered buffer, given before it is queued");
                    let piece = (buf.len() - done).min(DIRECT_PIECE);
                    let to = self.registered.at(hot.buffer);
                    // SAFETY: the buffer is this request's alone (lent by `Lent` until it ends),
                    // `DIRECT_PIECE` long, and the kernel reads it only for this request, which
                    // it is not given before this; `piece` is at most what is left of `buf`.
                    unsafe { ptr::copy_nonoverlapping(buf[done..].as_ptr(), to, piece) };
                    Sqe {
                        opcode: OP_WRITE_FIXED,
                        flags: SQE_FIXED_FILE,
                        fd: slot as i32,
                        off: cold.at + hot.done,
                        addr: to as u64,
                        len: piece as u32,
                        buf_index: hot.buffer,
                        user_data: ud(1),
                        ..Sqe::default()
                    }
                } else {
                    let off = cold.at + hot.done;
                    Sqe { off, addr: rest, len: left, ..on_slot(OP_WRITE, slot, 0, ud(1)) }
                };
                if hot.phase == OPENED {
                    sqes[0] = write;
                    return 1;
                }
                let flags = libc::O_WRONLY | if hot.flags & DIRECT != 0 { libc::O_DIRECT } else { 0 };
                // An empty write only opens the file, which must be there.
                let more = !buf.is_empty();
                sqes[0] = open(name(0), flags, 0, slot, if more { SQE_IO_LINK } else { 0 }, ud(0));
                if !more {
                    return 1;
                }
                sqes[1] = write;
                2
            }
            (Kind::Write, UNLINKING) | (Kind::Clone, UNLINKING) => {
                sqes[0] = unlink(name(if hot.kind == Kind::Write { 0 } else { 1 }), 0, ud(0));
                1
            }
            (Kind::Write, _) => {
                let [temporary, dir] = cold.slots;
                let mut n = 0;
                if hot.phase == OPENING {
                    sqes[0] = open(name(2), DIR, 0, dir, SQE_IO_LINK, ud(0));
                    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
                    sqes[1] = open(name(0), flags, 0o644, temporary, SQE_IO_LINK, ud(1));
                    n = 2;
                }
                sqes[n] = Sqe { off: hot.done, addr: rest, len: left, ..on_slot(OP_WRITE, temporary, SQE_IO_LINK, ud(2)) };
                sqes[n + 1] = on_slot(OP_FSYNC, temporary, SQE_IO_LINK, ud(3));
                sqes[n + 2] = two_names(OP_RENAMEAT, name(0), name(1), SQE_IO_LINK, ud(4));
                sqes[n + 3] = on_slot(OP_FSYNC, dir, 0, ud(5));
                n + 4
            }
            (Kind::Rename, _) => {
                let [to_dir, from_dir] = cold.slots;
                let two = cold.names[3] != cold.names[2];
                sqes[0] = open(name(2), DIR, 0, to_dir, SQE_IO_LINK, ud(0));
                let mut n = 1;
                if two {
                    sqes[n] = open(name(3), DIR, 0, from_dir, SQE_IO_LINK, ud(1));
                    n += 1;
                }
                sqes[n] = two_names(OP_RENAMEAT, name(0), name(1), SQE_IO_LINK, ud(2));
                sqes[n + 1] = on_slot(OP_FSYNC, to_dir, if two { SQE_IO_LINK } else { 0 }, ud(3));
                n += 2;
                if two {
                    sqes[n] = on_slot(OP_FSYNC, from_dir, 0, ud(4));
                    n += 1;
                }
                n
            }
            (Kind::Open, INSTALLING) => {
                // The descriptor, in the arena, put in the slot.
                sqes[0] = Sqe {
                    opcode: OP_FILES_UPDATE,
                    fd: -1,
                    addr: name(4) as u64,
                    len: 1,
                    off: u64::from(cold.slots[0]),
                    user_data: ud(0),
                    ..Sqe::default()
                };
                1
            }
            (Kind::Open, _) => {
                let flags = KEEP | if hot.flags & MAKE != 0 { libc::O_CREAT } else { 0 };
                sqes[0] = open(name(0), flags, 0o644, NO_SLOT, 0, ud(0));
                1
            }
            (Kind::Remove, phase) => {
                sqes[0] = unlink(name(0), if phase == EMPTYING { libc::AT_REMOVEDIR } else { 0 }, ud(0));
                1
            }
            (Kind::Link, _) => {
                sqes[0] = two_names(OP_LINKAT, name(0), name(1), 0, ud(0));
                1
            }
            (Kind::Clone, _) => {
                sqes[0] = open(name(0), libc::O_RDONLY, 0, NO_SLOT, SQE_IO_LINK, ud(0));
                let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
                sqes[1] = open(name(1), flags, 0o644, NO_SLOT, 0, ud(1));
                2
            }
            (Kind::Map, _) => {
                sqes[0] = open(name(0), libc::O_RDONLY, 0, NO_SLOT, 0, ud(0));
                1
            }
            (Kind::Stat, _) => {
                sqes[0] = statx(name(0), STATX_META, name(4), ud(0));
                1
            }
            (Kind::Load, phase) => {
                let spread = if self.loading >= SPREAD_LEAST { SQE_ASYNC } else { 0 };
                sqes[0] = match phase {
                    OPENING => open(name(0), libc::O_RDONLY, 0, NO_SLOT, spread, ud(0)),
                    _ => {
                        let (addr, len) = spare_of(buf, buf.capacity());
                        let (fd, off) = (raw(&cold.fds[0]), cold.at + buf.len() as u64);
                        Sqe { opcode: OP_READ, flags: spread, fd, off, addr, len, user_data: ud(0), ..Sqe::default() }
                    }
                };
                1
            }
            (kind, _) => unreachable!("{kind:?} is not a request on a path"),
        }
    }

    /// The result `res` of link `link` of the request on a path `id` (or a close); once every
    /// link asked is in, what they mean for it.
    pub(super) fn path_step(&mut self, id: Id, link: usize, res: i32) -> Step {
        let hot = *self.ops.get(id).expect("a request in flight");
        self.ops.cold_mut(id).results[link] = res;
        if hot.links != 0 {
            return Step::Held;
        }
        let results = mem::take(&mut self.ops.cold_mut(id).results);
        if hot.phase == MAKING {
            return self.made(id, results[0]);
        }
        match hot.kind {
            Kind::Close => {
                self.closes -= 1;
                let slot = hot.of as u32;
                if slot < self.table.ours {
                    self.table.free.borrow_mut().push(slot);
                }
                self.unslot();
                Step::Ends(None)
            }
            Kind::WriteAt => self.wrote_at(id, hot, results),
            Kind::Write => self.wrote(id, hot, results),
            Kind::Rename => self.renamed(id, hot, results),
            Kind::Open => self.opened(id, hot, results[0]),
            Kind::Remove => match (results[0], hot.phase) {
                (0, _) => Step::Ends(Some(Done::Removed)),
                (res, _) if res == -libc::ENOENT => Step::Ends(Some(Done::Removed)),
                // Linux's `unlink` of a directory.
                (res, OPENING) if res == -libc::EISDIR => {
                    self.ops.get_mut(id).expect("in flight").phase = EMPTYING;
                    Step::Again
                }
                (res, _) => Step::Ends(failed(res)),
            },
            Kind::Link => match results[0] {
                0 => Step::Ends(Some(Done::Linked)),
                res if res == -libc::ENOENT && hot.flags & MADE == 0 => {
                    self.make_parents(id).unwrap_or_else(|| Step::Ends(failed(res)))
                }
                res => Step::Ends(failed(res)),
            },
            Kind::Clone => self.cloned(id, hot, results),
            Kind::Map => self.mapped(id, results[0]),
            Kind::Stat => self.statted(id, hot, results[0]),
            Kind::Load => self.loaded(id, hot, results[0]),
            kind => unreachable!("{kind:?} is not a request on a path"),
        }
    }

    /// Whether `res` of the open of slot `n` of the request `id` opened it, noted where it did.
    fn opened_slot(&mut self, id: Id, n: usize, res: i32) -> bool {
        let opened = res >= 0;
        self.ops.cold_mut(id).opened[n] = opened;
        opened
    }

    fn wrote_at(&mut self, id: Id, hot: Hot, results: [i32; LINKS]) -> Step {
        if hot.phase == STATTING {
            let res = results[0];
            if res < 0 {
                return self.failed_on(id, "opening", 0, res);
            }
            let (at, len) = (self.ops.cold(id).at, self.ops.buf(id).len() as u64);
            let statx = self.statx_of(id, &hot);
            // `stx_dio_offset_align`, where the mask says it is filled in.
            let align = u64::from(if word(&statx, 0) & STATX_DIOALIGN != 0 { word(&statx, 156) } else { 0 });
            let hot = self.ops.get_mut(id).expect("in flight");
            if align != 0 && DIRECT_PIECE as u64 % align == 0 && at % align == 0 && len % align == 0 {
                hot.flags |= DIRECT;
            }
            hot.phase = OPENING;
            return Step::Again;
        }
        if hot.phase == OPENING && !self.opened_slot(id, 0, results[0]) {
            let what = match hot.flags & DIRECT {
                0 => "opening",
                _ => "opening, to bypass the page cache its file system says it takes,",
            };
            return self.failed_on(id, what, 0, results[0]);
        }
        let len = self.ops.buf(id).len();
        if len == 0 {
            return Step::Ends(Some(Done::Written));
        }
        let res = results[1];
        if res < 0 {
            return Step::Ends(failed(res));
        }
        let put = self.progress(id, res);
        self.ops.get_mut(id).expect("in flight").phase = OPENED;
        match (put < len, res) {
            (false, _) => Step::Ends(Some(Done::Written)),
            (true, 0) => Step::Ends(failed(-libc::EIO)),
            (true, _) => Step::Again,
        }
    }

    fn wrote(&mut self, id: Id, hot: Hot, results: [i32; LINKS]) -> Step {
        if hot.phase == UNLINKING {
            // The temporary is gone, or never renamed: the old file is as it was.
            return Step::Ends(failed(-(self.ops.cold(id).at as i32)));
        }
        if hot.phase == OPENING {
            let dir = results[0];
            if !self.opened_slot(id, 1, dir) {
                if dir == -libc::ENOENT && hot.flags & MADE == 0 {
                    if let Some(step) = self.make_parents(id) {
                        return step;
                    }
                }
                return self.failed_on(id, "opening", 2, dir);
            }
            if !self.opened_slot(id, 0, results[1]) {
                return self.failed_on(id, "making", 0, results[1]);
            }
        }
        let [_, _, wrote, synced, renamed, dir] = results;
        let len = self.ops.buf(id).len();
        let put = hot.done as usize;
        // A short write breaks the chain; the rest is asked again, with the links after it.
        if wrote > 0 && put + (wrote as usize) < len {
            self.progress(id, wrote);
            self.ops.get_mut(id).expect("in flight").phase = OPENED;
            return Step::Again;
        }
        let wrote = if wrote == 0 && put < len { -libc::EIO } else { wrote };
        if let Some(res) = [wrote, synced, renamed].into_iter().find(|it| *it < 0) {
            self.ops.get_mut(id).expect("in flight").phase = UNLINKING;
            self.ops.cold_mut(id).at = u64::from(res.unsigned_abs());
            return Step::Again;
        }
        if dir < 0 { Step::Ends(failed(dir)) } else { Step::Ends(Some(Done::Written)) }
    }

    fn renamed(&mut self, id: Id, hot: Hot, results: [i32; LINKS]) -> Step {
        let two = {
            let cold = self.ops.cold(id);
            cold.names[3] != cold.names[2]
        };
        if !self.opened_slot(id, 0, results[0]) {
            if results[0] == -libc::ENOENT && hot.flags & MADE == 0 {
                if let Some(step) = self.make_parents(id) {
                    return step;
                }
            }
            return self.failed_on(id, "opening", 2, results[0]);
        }
        if two && !self.opened_slot(id, 1, results[1]) {
            return self.failed_on(id, "opening", 3, results[1]);
        }
        match results[2..].iter().find(|it| **it < 0) {
            Some(&res) => Step::Ends(failed(res)),
            None => Step::Ends(Some(Done::Renamed)),
        }
    }

    /// The open of the kept file `hot.of` completed with `res`.
    fn opened(&mut self, id: Id, hot: Hot, res: i32) -> Step {
        let file: Handle<Kept> = Handle::from_bits(hot.of);
        if hot.phase == INSTALLING {
            if res < 0 {
                // alloc: an error.
                let why = format!("putting {} in the ring's table of files: {}", self.name_of(id, 0).to_string_lossy(), error(res));
                self.unkept(file, res, &why);
                return Step::Ends(None);
            }
            let cold = self.ops.cold_mut(id);
            let (fd, slot) = (cold.fds[0].take(), mem::replace(&mut cold.slots[0], NO_SLOT));
            self.slots_held -= 1;
            let kept = self.kept.get_mut(file).expect("held by its open");
            kept.fixed.fd = fd;
            kept.fixed.slot = Some(slot);
            kept.open = true;
            self.wake(file, None, false);
            return Step::Ends(None);
        }
        if res >= 0 {
            // SAFETY: the open's completion: a descriptor of the driver's, owned by no other.
            let fd = unsafe { OwnedFd::from_raw_fd(res) };
            // The descriptor the kernel reads as an `__s32`, at an offset it may read as one.
            let names = self.paths.arena(arena_of(&hot));
            let pad = names.len().next_multiple_of(4) - names.len();
            let at = (names.len() + pad) as u32;
            let at = put(names, &[0; 3][..pad]).and_then(|()| put(names, &res.to_ne_bytes())).map(|()| at);
            let cold = self.ops.cold_mut(id);
            cold.fds[0] = Some(fd);
            match at {
                Ok(at) => cold.names[4] = at,
                Err(error) => {
                    // alloc: an error.
                    let why = error.to_string();
                    self.unkept(file, -libc::ENOMEM, &why);
                    return Step::Ends(None);
                }
            }
            self.ops.get_mut(id).expect("in flight").phase = INSTALLING;
            return Step::Again;
        }
        let make = self.kept.get(file).expect("held by its open").make;
        if res == -libc::ENOENT && hot.flags & MAKE == 0 && make {
            // A read asked before the first request that makes it: the reads fail as the
            // file is missing, and it is opened again to be made.
            // alloc: an error.
            let why = format!("opening {}: {}", self.name_of(id, 0).to_string_lossy(), error(res));
            self.wake(file, Some((res, &why)), true);
            self.ops.get_mut(id).expect("in flight").flags |= MAKE;
            return Step::Again;
        }
        if res == -libc::ENOENT && hot.flags & MAKE != 0 && hot.flags & MADE == 0 {
            if let Some(step) = self.make_parents(id) {
                return step;
            }
        }
        // alloc: an error.
        let why = format!("opening {}: {}", self.name_of(id, 0).to_string_lossy(), error(res));
        self.unkept(file, res, &why);
        Step::Ends(None)
    }

    /// The kept file `file` failed to open with `res`: every request waiting for it ends
    /// with `why` (a whole file's read of a missing one, empty), and it is no longer named,
    /// so the next request on its name opens it again.
    fn unkept(&mut self, file: Handle<Kept>, res: i32, why: &str) {
        self.wake(file, Some((res, why)), false);
        self.names.retain(|_, it| *it != file);
        if let Some(kept) = self.kept.get_mut(file) {
            kept.named = false;
        }
    }

    /// The requests waiting for the kept file `file`, in the order they asked: asked now it is
    /// open, or, where it `failed`, ended with why; with `reads`, only its reads.
    fn wake(&mut self, file: Handle<Kept>, failed: Option<(i32, &str)>, reads: bool) {
        let mut at = 0;
        while at < self.on_open.len() {
            let id = self.on_open[at];
            let Some(hot) = self.ops.get(id) else {
                self.on_open.remove(at);
                continue;
            };
            if hot.of != file.to_bits() || (reads && hot.kind != Kind::Read) {
                at += 1;
                continue;
            }
            self.on_open.remove(at);
            let (tag, whole) = (hot.tag, hot.flags & WHOLE != 0);
            let Some((res, why)) = failed else {
                self.queue(id);
                continue;
            };
            let done = match whole && res == -libc::ENOENT {
                true => Done::Read(mem::take(self.ops.buf_mut(id))),
                false => Done::Failed(IoError(why.to_owned())), // alloc: clone: an error.
            };
            self.ready.push(Completion { tag, done });
            self.end(id);
        }
    }

    fn cloned(&mut self, id: Id, hot: Hot, results: [i32; LINKS]) -> Step {
        if hot.phase == UNLINKING {
            let res = -(self.ops.cold(id).at as i32);
            let (from, to) = (self.name_of(id, 0).to_string_lossy(), self.name_of(id, 1).to_string_lossy());
            let why = match matches!(-res, libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL) {
                // alloc: an error path.
                true => format!(
                    "the file system of {to} cannot share blocks with {from} ({}); blaze does not copy instead",
                    error(res)
                ),
                false => error(res),
            };
            return Step::Ends(Some(Done::Failed(IoError(why))));
        }
        let [from, to, ..] = results;
        // SAFETY: each open's completion, where it opened: a descriptor of the driver's.
        let [source, target] = [from, to].map(|it| (it >= 0).then(|| unsafe { OwnedFd::from_raw_fd(it) }));
        // A failed first open cancels the second: nothing was made.
        let Some(source) = source else {
            return self.failed_on(id, "opening", 0, from);
        };
        let Some(target) = target else {
            drop(source);
            if to == -libc::ENOENT && hot.flags & MADE == 0 {
                if let Some(step) = self.make_parents(id) {
                    return step;
                }
            }
            return self.failed_on(id, "making", 1, to);
        };
        /// `_IOW(0x94, 9, int)` of `linux/fs.h`.
        const FICLONE: libc::c_ulong = 0x4004_9409;
        // No io_uring op shares blocks: one `FICLONE` of the two files the ring opened, here.
        // SAFETY: two open descriptors; the call reads the first's blocks into the second.
        let cloned = unsafe { libc::ioctl(target.as_raw_fd(), FICLONE as _, source.as_raw_fd()) };
        if cloned == 0 {
            return Step::Ends(Some(Done::Cloned));
        }
        let errno = io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        drop((source, target));
        self.ops.get_mut(id).expect("in flight").phase = UNLINKING;
        self.ops.cold_mut(id).at = errno as u64;
        Step::Again
    }

    fn mapped(&mut self, id: Id, res: i32) -> Step {
        if res < 0 {
            return self.failed_on(id, "opening", 0, res);
        }
        // SAFETY: the open's completion: a descriptor of the driver's, owned by no other.
        let file = File::from(unsafe { OwnedFd::from_raw_fd(res) });
        // SAFETY: `Effect::Map`'s contract: while any `Mapping` of the file lives, no effect
        // writes, appends to or truncates it in place; the store and the image replace a file
        // only by renaming another over it.
        let done = match unsafe { crate::fs::Mapped::map(&file) } {
            Ok(mapped) => Done::Mapped(crate::Mapping::file(mapped)),
            Err(error) => Done::Failed(IoError(error.to_string())), // alloc: an error.
        };
        Step::Ends(Some(done))
    }

    fn statted(&mut self, id: Id, hot: Hot, res: i32) -> Step {
        if res == -libc::ENOENT {
            return Step::Ends(Some(Done::Stat(None)));
        }
        if res < 0 {
            return Step::Ends(failed(res));
        }
        let statx = self.statx_of(id, &hot);
        let wide = |at: usize| u64::from(word(&statx, at)) | u64::from(word(&statx, at + 4)) << 32;
        let (mask, mode, size) = (word(&statx, 0), word(&statx, 28) & 0xffff, wide(40));
        let (seconds, nanos) = (wide(112) as i64, word(&statx, 120));
        let modified_ns = match (mask & STATX_MTIME != 0, u64::try_from(seconds)) {
            (true, Ok(seconds)) => seconds.saturating_mul(1_000_000_000).saturating_add(u64::from(nanos)),
            _ => 0,
        };
        let dir = mode & libc::S_IFMT == libc::S_IFDIR;
        Step::Ends(Some(Done::Stat(Some(crate::Meta { len: size, dir, modified_ns }))))
    }

    /// The request `id`, whose path's directory was missing, makes the directories its path
    /// is in, from the first; `None` where its path is in none, so its `ENOENT` stands.
    fn make_parents(&mut self, id: Id) -> Option<Step> {
        let hot = self.ops.get_mut(id).expect("in flight");
        hot.flags |= MADE;
        hot.phase = MAKING;
        // Past a leading `/`: the first directory ends at the next.
        self.ops.cold_mut(id).at = 1;
        match self.next_parent(id) {
            Ok(true) => Some(Step::Again),
            Ok(false) => {
                self.ops.get_mut(id).expect("in flight").phase = OPENING;
                None
            }
            Err(error) => Some(Step::Ends(Some(Done::Failed(IoError(error.to_string()))))), // alloc: an error.
        }
    }

    /// A directory of the request `id`'s path made with `res`.
    fn made(&mut self, id: Id, res: i32) -> Step {
        if res < 0 && res != -libc::EEXIST {
            return self.failed_on(id, "making", 4, res);
        }
        match self.next_parent(id) {
            Ok(true) => Step::Again,
            Ok(false) => {
                // Every directory made: the request from its start.
                self.ops.get_mut(id).expect("in flight").phase = OPENING;
                Step::Again
            }
            Err(error) => Step::Ends(Some(Done::Failed(IoError(error.to_string())))), // alloc: an error.
        }
    }

    /// The next directory of the request `id`'s path to make, past [`Cold::at`] of it, put
    /// in its arena as its fifth name; whether there is one.
    fn next_parent(&mut self, id: Id) -> io::Result<bool> {
        let hot = *self.ops.get(id).expect("in flight");
        let arena = arena_of(&hot);
        let (path, from) = {
            let cold = self.ops.cold(id);
            (cold.names[made_for(hot.kind)], cold.at as usize)
        };
        let pushed = scratch::with_bytes(|dir| -> io::Result<Option<(u32, usize)>> {
            // SAFETY: `path` is the request's name `made_for` its kind, a path its push put
            // NUL-terminated in `arena`, which the request holds while in flight.
            let path = unsafe { self.paths.name(arena, path) }.to_bytes();
            let Some(end) = path.get(from..).and_then(|rest| rest.iter().position(|&it| it == b'/')) else {
                return Ok(None);
            };
            dir.extend_from_slice(&path[..from + end]);
            let at = push_c(self.paths.arena(arena), dir)?;
            Ok(Some((at, from + end + 1)))
        })?;
        let Some((at, next)) = pushed else { return Ok(false) };
        let cold = self.ops.cold_mut(id);
        cold.names[4] = at;
        cold.at = next as u64;
        Ok(true)
    }

    /// What the request on a path `hot` held let go as it ends: its names, each slot its
    /// opens filled emptied through the ring (one not filled free again at once), its kept
    /// file's use, the name a whole file's write makes another file, its registered buffer.
    pub(super) fn path_end(&mut self, hot: &Hot, mut cold: Cold) {
        self.paths.release(arena_of(hot));
        let mut freed = false;
        for (slot, opened) in cold.slots.into_iter().zip(cold.opened) {
            if slot == NO_SLOT {
                continue;
            }
            self.slots_held -= 1;
            match opened {
                true => self.close(slot),
                false => {
                    self.table.free.borrow_mut().push(slot);
                    freed = true;
                }
            }
        }
        if freed {
            self.unslot();
        }
        match hot.kind {
            Kind::Open => unkeep(&mut self.kept, Handle::from_bits(hot.of)),
            Kind::Write => {
                if let Some(file) = cold.file.take() {
                    self.forget(&file);
                }
            }
            Kind::WriteAt => {
                self.direct_writes += u64::from(hot.flags & DIRECT != 0 && hot.phase == OPENED);
                if hot.buffer != ops::NO_BUFFER {
                    self.unbuffer(hot.buffer);
                }
            }
            Kind::Load if hot.phase != QUEUED => {
                // Its descriptor closed before the next read takes its room.
                drop(cold.fds[0].take());
                self.unload();
            }
            _ => {}
        }
    }
}
