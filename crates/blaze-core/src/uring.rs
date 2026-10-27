//! A [`Driver`] on Linux's io_uring: one ring, shared with the kernel through memory, that the
//! thread which polls fills with requests and reaps completions from, with no thread of its own
//! and one system call for a batch either way. Reads, appends, syncs and timers are the ring's;
//! a file is opened once and kept open. A truncation, which comes only in recovery, is a call
//! made where it is submitted.
//!
//! The ring is set up with the system calls themselves (`io_uring_setup`, `io_uring_enter` and
//! three `mmap`s), the layouts as `linux/io_uring.h` has them. Kernel 5.11 or later: a wait with
//! a timeout is `IORING_ENTER_EXT_ARG`.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use crate::{Completion, Done, Driver, Effect, FileName, IoError, Tag};

const OP_FSYNC: u8 = 3;
const OP_TIMEOUT: u8 = 11;
const OP_READ: u8 = 22;
const OP_WRITE: u8 = 23;

const FEAT_SINGLE_MMAP: u32 = 1 << 0;
const FEAT_EXT_ARG: u32 = 1 << 8;
const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_EXT_ARG: u32 = 1 << 3;
const OFF_SQ_RING: libc::off_t = 0;
const OFF_SQES: libc::off_t = 0x1000_0000;

/// Requests the ring holds at once; more wait in the driver until there is room.
const ENTRIES: u32 = 256;

#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    pad: u64,
}

#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    pad: u32,
    ts: u64,
}

const _: () = assert!(size_of::<Params>() == 120);
const _: () = assert!(size_of::<Sqe>() == 64);
const _: () = assert!(size_of::<Cqe>() == 16);

/// A mapping of the ring's memory, unmapped when dropped.
struct Map {
    at: *mut u8,
    len: usize,
}

impl Map {
    fn new(fd: RawFd, len: usize, offset: libc::off_t) -> io::Result<Map> {
        // SAFETY: a shared mapping of the ring's file at the offset the kernel names for it.
        let at = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                offset,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { at: at.cast(), len })
    }

    /// The `T` at `offset`, which the kernel's offsets say is one.
    fn at<T>(&self, offset: u32) -> *mut T {
        debug_assert!(offset as usize + size_of::<T>() <= self.len);
        // SAFETY: within the mapping, as the debug assertion checks.
        unsafe { self.at.add(offset as usize).cast() }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// The ring: its file, the mappings and where in them the kernel's counters are.
struct Ring {
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    /// Requests written to the ring and not yet handed to the kernel.
    unsubmitted: u32,
    // Dropped after the pointers into them are no longer used.
    _maps: Vec<Map>,
    fd: OwnedFd,
}

impl Ring {
    fn new(entries: u32) -> io::Result<Ring> {
        let mut params = Params::default();
        // SAFETY: `params` is the `io_uring_params` the call reads and fills.
        let fd = unsafe {
            libc::syscall(libc::SYS_io_uring_setup, entries, &raw mut params as *mut libc::c_void)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a file descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if params.features & FEAT_EXT_ARG == 0 || params.features & FEAT_SINGLE_MMAP == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring of before Linux 5.11 (no IORING_FEAT_EXT_ARG)",
            ));
        }
        let sq_len = params.sq_off.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_len = params.cq_off.cqes as usize + params.cq_entries as usize * size_of::<Cqe>();
        let rings = Map::new(fd.as_raw_fd(), sq_len.max(cq_len), OFF_SQ_RING)?;
        let sqes =
            Map::new(fd.as_raw_fd(), params.sq_entries as usize * size_of::<Sqe>(), OFF_SQES)?;
        let (sq, cq) = (&params.sq_off, &params.cq_off);
        let array: *mut u32 = rings.at(sq.array);
        for index in 0..params.sq_entries {
            // SAFETY: the array has `sq_entries` slots; each names the request of its index.
            unsafe { array.add(index as usize).write(index) };
        }
        // SAFETY: the kernel put these counters at these offsets of the mapping.
        let (sq_mask, cq_mask) =
            unsafe { (*rings.at::<u32>(sq.ring_mask), *rings.at::<u32>(cq.ring_mask)) };
        Ok(Ring {
            sq_head: rings.at(sq.head),
            sq_tail: rings.at(sq.tail),
            sq_mask,
            sq_entries: params.sq_entries,
            sqes: sqes.at(0),
            cq_head: rings.at(cq.head),
            cq_tail: rings.at(cq.tail),
            cq_mask,
            cqes: rings.at(cq.cqes),
            unsubmitted: 0,
            _maps: vec![rings, sqes],
            fd,
        })
    }

    /// Writes `sqe` to the ring, unless it is full.
    fn push(&mut self, sqe: Sqe) -> bool {
        // SAFETY: the counters are the kernel's and this thread's; the kernel moves the head.
        let (head, tail) = unsafe {
            ((*self.sq_head).load(Ordering::Acquire), (*self.sq_tail).load(Ordering::Relaxed))
        };
        if tail.wrapping_sub(head) >= self.sq_entries {
            return false;
        }
        // SAFETY: the slot at `tail` is not the kernel's until the tail moves past it.
        unsafe {
            self.sqes.add((tail & self.sq_mask) as usize).write(sqe);
            (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release);
        }
        self.unsubmitted += 1;
        true
    }

    /// Hands the requests written to the kernel and, with `wait`, waits up to it for one
    /// completion.
    fn enter(&mut self, wait: Option<Duration>) -> io::Result<()> {
        let ts =
            wait.map(|it| Timespec { sec: it.as_secs() as i64, nsec: it.subsec_nanos() as i64 });
        let arg = GeteventsArg {
            sigmask: 0,
            sigmask_sz: 0,
            pad: 0,
            ts: ts.as_ref().map_or(0, |it| it as *const Timespec as u64),
        };
        let (min_complete, flags) = match wait {
            Some(_) => (1, ENTER_GETEVENTS | ENTER_EXT_ARG),
            None => (0, ENTER_EXT_ARG),
        };
        loop {
            // SAFETY: `arg` and the timespec it points at outlive the call.
            let entered = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_enter,
                    self.fd.as_raw_fd(),
                    self.unsubmitted,
                    min_complete,
                    flags,
                    &raw const arg,
                    size_of::<GeteventsArg>(),
                )
            };
            if entered >= 0 {
                self.unsubmitted -= (entered as u32).min(self.unsubmitted);
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The wait ended with nothing done.
                Some(libc::ETIME) => return Ok(()),
                // Completions to reap first.
                Some(libc::EBUSY | libc::EAGAIN) => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    /// Every completion the kernel has posted.
    fn reap(&mut self, mut each: impl FnMut(u64, i32)) {
        // SAFETY: the counters are the kernel's and this thread's; the kernel moves the tail.
        let (mut head, tail) = unsafe {
            ((*self.cq_head).load(Ordering::Relaxed), (*self.cq_tail).load(Ordering::Acquire))
        };
        while head != tail {
            // SAFETY: the entries between the head and the tail are posted and not reaped.
            let cqe = unsafe { &*self.cqes.add((head & self.cq_mask) as usize) };
            let (user_data, res) = (cqe.user_data, cqe.res);
            head = head.wrapping_add(1);
            // SAFETY: as above; the entry is the kernel's again once the head is past it.
            unsafe { (*self.cq_head).store(head, Ordering::Release) };
            each(user_data, res);
        }
    }
}

enum Op {
    /// Into `bytes` from `at`, `got` of `bytes.len()` read so far.
    Read {
        fd: RawFd,
        at: u64,
        bytes: Vec<u8>,
        got: usize,
    },
    /// `bytes` at the end of the file, `put` of them written so far.
    Append {
        fd: RawFd,
        bytes: Vec<u8>,
        put: usize,
    },
    Sync {
        fd: RawFd,
    },
    /// The kernel reads the time from here until it completes.
    Timer {
        after: Box<Timespec>,
    },
}

struct Pending {
    tag: Tag,
    op: Op,
}

pub struct Uring {
    start: Instant,
    root: PathBuf,
    ring: Ring,
    /// Each file opened, for reading and appending.
    files: BTreeMap<FileName, OwnedFd>,
    /// By the index in the ring's `user_data`.
    pending: Vec<Option<Pending>>,
    free: Vec<usize>,
    /// Requests for which the ring had no room.
    waiting: VecDeque<usize>,
    /// Effects done where they were submitted.
    ready: Vec<Completion>,
    in_flight: usize,
}

impl Uring {
    /// A driver whose files are under `root`, or why this kernel has no ring for it.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Uring> {
        Ok(Uring {
            start: Instant::now(),
            root: root.into(),
            ring: Ring::new(ENTRIES)?,
            files: BTreeMap::new(),
            pending: Vec::new(),
            free: Vec::new(),
            waiting: VecDeque::new(),
            ready: Vec::new(),
            in_flight: 0,
        })
    }

    /// The file, opened once; `make` makes it if missing, with its directories.
    fn file(&mut self, name: &FileName, make: bool) -> io::Result<RawFd> {
        if let Some(fd) = self.files.get(name) {
            return Ok(fd.as_raw_fd());
        }
        let path = self.root.join(name.as_str());
        if make && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let fd = open(&path, make)?;
        let raw = fd.as_raw_fd();
        self.files.insert(name.clone(), fd);
        Ok(raw)
    }

    fn start(&mut self, tag: Tag, op: Op) {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                self.pending.push(None);
                self.pending.len() - 1
            }
        };
        self.pending[index] = Some(Pending { tag, op });
        self.queue(index);
    }

    /// Writes the request of `pending[index]` as it now stands to the ring, or keeps it for
    /// when there is room.
    fn queue(&mut self, index: usize) {
        if !self.waiting.is_empty() || !self.ring.push(self.sqe(index)) {
            self.waiting.push_back(index);
        }
    }

    fn sqe(&self, index: usize) -> Sqe {
        let pending = self.pending[index].as_ref().expect("a request in flight");
        let user_data = index as u64;
        match &pending.op {
            Op::Read { fd, at, bytes, got } => Sqe {
                opcode: OP_READ,
                fd: *fd,
                off: at + *got as u64,
                // SAFETY of the kernel's write: the buffer stays where it is until completion.
                addr: bytes[*got..].as_ptr() as u64,
                len: (bytes.len() - got) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Append { fd, bytes, put } => Sqe {
                opcode: OP_WRITE,
                fd: *fd,
                // The file's position, which is its end: it is open for appending.
                off: u64::MAX,
                addr: bytes[*put..].as_ptr() as u64,
                len: (bytes.len() - put) as u32,
                user_data,
                ..Sqe::default()
            },
            Op::Sync { fd } => Sqe { opcode: OP_FSYNC, fd: *fd, user_data, ..Sqe::default() },
            Op::Timer { after } => Sqe {
                opcode: OP_TIMEOUT,
                fd: -1,
                addr: &**after as *const Timespec as u64,
                len: 1,
                user_data,
                ..Sqe::default()
            },
        }
    }

    fn flush_waiting(&mut self) {
        while let Some(&index) = self.waiting.front() {
            let sqe = self.sqe(index);
            if !self.ring.push(sqe) {
                break;
            }
            self.waiting.pop_front();
        }
    }

    fn submit_ring(&mut self) {
        if self.ring.unsubmitted > 0
            && let Err(error) = self.ring.enter(None)
        {
            self.fail_unsubmitted(error);
        }
    }

    /// The ring takes no more requests: every one in flight fails. Their slots are not used
    /// again, as the kernel may still post a completion of one.
    fn fail_unsubmitted(&mut self, error: io::Error) {
        for pending in self.pending.iter_mut().filter_map(Option::take) {
            let done = Done::Failed(IoError(error.to_string()));
            self.ready.push(Completion { tag: pending.tag, done });
        }
        self.waiting.clear();
        self.ring.unsubmitted = 0;
    }

    /// What became of the request at `index`: its completion, or `None` when the rest of it
    /// was asked for again.
    fn completed(&mut self, index: usize, res: i32) -> Option<Completion> {
        let pending = self.pending[index].as_mut()?;
        let failed =
            |res: i32| Done::Failed(IoError(io::Error::from_raw_os_error(-res).to_string()));
        let done = match &mut pending.op {
            Op::Timer { .. } if res == -libc::ETIME || res == 0 => Done::Fired,
            _ if res < 0 => failed(res),
            Op::Read { bytes, got, .. } => {
                *got += res as usize;
                if res > 0 && *got < bytes.len() {
                    self.queue(index);
                    return None;
                }
                let mut bytes = std::mem::take(bytes);
                bytes.truncate(*got);
                Done::Read(bytes)
            }
            Op::Append { bytes, put, .. } => {
                *put += res as usize;
                if *put < bytes.len() {
                    if res == 0 {
                        failed(-libc::EIO)
                    } else {
                        self.queue(index);
                        return None;
                    }
                } else {
                    Done::Appended
                }
            }
            Op::Sync { .. } => Done::Synced,
            Op::Timer { .. } => failed(res),
        };
        let pending = self.pending[index].take().expect("in flight");
        self.free.push(index);
        Some(Completion { tag: pending.tag, done })
    }

    fn reap(&mut self, out: &mut Vec<Completion>) -> usize {
        let mut posted = Vec::new();
        self.ring.reap(|user_data, res| posted.push((user_data as usize, res)));
        let mut count = 0;
        for (index, res) in posted {
            if let Some(completion) = self.completed(index, res) {
                out.push(completion);
                count += 1;
            }
        }
        self.flush_waiting();
        self.submit_ring();
        count
    }
}

fn open(path: &Path, make: bool) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
    let flags =
        libc::O_RDWR | libc::O_APPEND | libc::O_CLOEXEC | if make { libc::O_CREAT } else { 0 };
    // SAFETY: a NUL-terminated path; the descriptor returned is this driver's.
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn size_of_file(fd: RawFd) -> io::Result<u64> {
    // SAFETY: `stat` is written by the call.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor and a `stat` to write.
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.st_size as u64)
}

impl Driver for Uring {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            self.in_flight += 1;
            let tag = effect.tag();
            let started = match effect {
                Effect::ReadAll { file, .. } => match self.file(&file, false) {
                    Ok(fd) => size_of_file(fd).map(|size| {
                        let bytes = vec![0; size as usize];
                        match size {
                            0 => Some(Done::Read(bytes)),
                            _ => {
                                self.start(tag, Op::Read { fd, at: 0, bytes, got: 0 });
                                None
                            }
                        }
                    }),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        Ok(Some(Done::Read(Vec::new())))
                    }
                    Err(error) => Err(error),
                },
                Effect::Read { file, at, len, .. } => self.file(&file, false).map(|fd| {
                    self.start(tag, Op::Read { fd, at, bytes: vec![0; len], got: 0 });
                    None
                }),
                Effect::Append { file, bytes, .. } => {
                    self.file(&file, true).map(|fd| match bytes.is_empty() {
                        true => Some(Done::Appended),
                        false => {
                            self.start(tag, Op::Append { fd, bytes, put: 0 });
                            None
                        }
                    })
                }
                Effect::Sync { file, .. } => self.file(&file, true).map(|fd| {
                    self.start(tag, Op::Sync { fd });
                    None
                }),
                Effect::Truncate { file, len, .. } => self.file(&file, true).and_then(|fd| {
                    // SAFETY: an open descriptor.
                    match unsafe { libc::ftruncate(fd, len as libc::off_t) } {
                        0 => Ok(Some(Done::Truncated)),
                        _ => Err(io::Error::last_os_error()),
                    }
                }),
                Effect::Timer { after, .. } => {
                    let after = Box::new(Timespec {
                        sec: after.as_secs() as i64,
                        nsec: after.subsec_nanos() as i64,
                    });
                    self.start(tag, Op::Timer { after });
                    Ok(None)
                }
            };
            match started {
                Ok(None) => {}
                Ok(Some(done)) => self.ready.push(Completion { tag, done }),
                Err(error) => self
                    .ready
                    .push(Completion { tag, done: Done::Failed(IoError(error.to_string())) }),
            }
        }
        self.submit_ring();
    }

    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize {
        let mut count = self.ready.len();
        out.append(&mut self.ready);
        count += self.reap(out);
        let in_ring = self.in_flight - count;
        if count == 0 && in_ring > 0 {
            let deadline = Instant::now() + wait;
            while count == 0 {
                let left = deadline.saturating_duration_since(Instant::now());
                if let Err(error) = self.ring.enter(Some(left)) {
                    self.fail_unsubmitted(error);
                    count += self.ready.len();
                    out.append(&mut self.ready);
                    break;
                }
                count += self.reap(out);
                if left.is_zero() {
                    break;
                }
            }
        }
        self.in_flight -= count;
        count
    }

    fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileName, run_until_idle, threads::Threads};

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The same effects on the ring and on threads end the same.
    #[test]
    fn as_threads_do() {
        let file = FileName::new("vfs/journal").unwrap();
        let script = |driver: &mut dyn Driver| {
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"one".to_vec(), tag: Tag(1) }];
            let mut seen = Vec::new();
            run_until_idle(driver, &mut effects, |done, effects| {
                seen.push(done.done.clone());
                match done.tag {
                    Tag(1) => effects.push(Effect::Sync { file: file.clone(), tag: Tag(2) }),
                    Tag(2) => {
                        effects.push(Effect::Timer { after: Duration::from_millis(1), tag: Tag(3) })
                    }
                    Tag(3) => effects.push(Effect::Read {
                        file: file.clone(),
                        at: 1,
                        len: 10,
                        tag: Tag(4),
                    }),
                    Tag(4) => {
                        effects.push(Effect::Truncate { file: file.clone(), len: 2, tag: Tag(5) })
                    }
                    Tag(5) => effects.push(Effect::ReadAll { file: file.clone(), tag: Tag(6) }),
                    Tag(6) => effects.push(Effect::ReadAll {
                        file: FileName::new("missing").unwrap(),
                        tag: Tag(7),
                    }),
                    Tag(7) => effects.push(Effect::Read {
                        file: FileName::new("missing").unwrap(),
                        at: 0,
                        len: 1,
                        tag: Tag(8),
                    }),
                    _ => {}
                }
            });
            seen
        };
        let (ours, theirs) = (dir("uring"), dir("uring-threads"));
        let ring = script(&mut Uring::new(&ours).unwrap());
        let threads = script(&mut Threads::new(&theirs, 2));
        assert_eq!(&ring[..7], &threads[..7]);
        assert_eq!(
            &ring[..7],
            &[
                Done::Appended,
                Done::Synced,
                Done::Fired,
                Done::Read(b"ne".to_vec()),
                Done::Truncated,
                Done::Read(b"on".to_vec()),
                Done::Read(Vec::new()),
            ]
        );
        assert!(matches!((&ring[7], &threads[7]), (Done::Failed(_), Done::Failed(_))));
        for it in [ours, theirs] {
            std::fs::remove_dir_all(it).unwrap();
        }
    }

    /// More requests than the ring holds, all in flight at once, each to its own file, with
    /// timers among them that fire in their order.
    #[test]
    fn more_than_the_ring_holds() {
        let root = dir("uring-many");
        let mut driver = Uring::new(&root).unwrap();
        let count = ENTRIES as u64 * 4;
        let mut effects: Vec<Effect> = (0..count)
            .map(|n| Effect::Append {
                file: FileName::new(&format!("f/{}", n % 64)).unwrap(),
                bytes: n.to_le_bytes().to_vec(),
                tag: Tag(n),
            })
            .collect();
        effects.extend((0..3).map(|n| Effect::Timer {
            after: Duration::from_millis(5 * (3 - n)),
            tag: Tag(count + n),
        }));
        let mut appended = 0;
        let mut fired = Vec::new();
        run_until_idle(&mut driver, &mut effects, |done, _| match done.done {
            Done::Appended => appended += 1,
            Done::Fired => fired.push(done.tag.0 - count),
            other => panic!("{other:?}"),
        });
        assert_eq!((appended, fired), (count, vec![2, 1, 0]));
        let mut total = 0;
        for n in 0..64 {
            total += std::fs::read(root.join(format!("f/{n}"))).unwrap().len();
        }
        assert_eq!(total as u64, count * 8);
        drop(driver);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_wait_ends_at_its_time() {
        let mut driver = Uring::new(dir("uring-wait")).unwrap();
        let mut effects = vec![Effect::Timer { after: Duration::from_secs(60), tag: Tag(1) }];
        driver.submit(&mut effects);
        let began = Instant::now();
        let mut out = Vec::new();
        assert_eq!(driver.poll(&mut out, Duration::from_millis(20)), 0);
        let waited = began.elapsed();
        assert!(
            waited >= Duration::from_millis(19) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert_eq!(driver.in_flight(), 1);
    }
}
