//! Watching a tree on Linux (docs/v2/plan.md's table): with fanotify, one mark of the whole
//! file system, where the process has the privileges it needs, and with inotify where it does
//! not (a watch on each directory under the watched one, added as directories appear). Either
//! way the events are read off one descriptor, which the threads driver reads on a thread of
//! its own and the ring driver through the ring.
//!
//! Starting one ([`Tree::begin`]) costs what fanotify's start costs (a constant number of
//! system calls and one read of `/proc/self/mountinfo`, `crate::fanotify`'s floor) where
//! fanotify can be had. inotify watches one directory each, so its start is a walk of the
//! whole tree, one `inotify_add_watch`, open, listing and close per directory: O(directories
//! under it), the floor of any inotify watch. That walk is a [`Walk`] the caller runs where it
//! may take long (the ring driver on a thread of its own, never its loop), under a span naming
//! the directory, and bounded by [`MOST_DIRS`]: past it the start fails naming the directory and
//! the count, never a watch of part of the tree.
//!
//! A directory made in (or moved into) a watched tree needs watches of its own under inotify,
//! one per directory in it: a walk again, O(directories under it). [`Tree::take`] does not walk
//! it; it tells the directory's name at once and keeps it as fresh, and the caller hands the
//! fresh directories to whoever may take long as a [`Grow`] ([`Tree::grow`]), merging what it
//! found back with [`Tree::grown`]. The invariant: a file made under a new directory before its
//! watch exists is found by the grow's own listing, which runs after the watch is added (add,
//! then list, as the start's walk does), so every file in it is either listed or has an event
//! on the new watch; such an event, read before the grow is merged, names a watch the tables do
//! not know yet and is kept ([`PARKED`]) and taken again once it is. A flush is answered only
//! once no grow is out or fresh ([`Tree::pending`]), so it comes after what the grow finds.
//! Tested by `a_file_made_in_a_new_directory_before_its_watch_is_heard`, whose control
//! ([`GROW_DROPS_FOUND`]) drops what the walk finds. Cost on the loop: one `dup` per grow, and
//! a hash insert per directory merged.
//!
//! A watch allocates its completions' payload and nothing else once warm: directories are
//! listed with `getdents64` into a buffer kept for the next listing, the names a watch keeps are
//! spans of a column of reserved memory ([`Strings`]), the names one read of events tells are
//! gathered and deduplicated before any is made a [`FileName`] ([`Batch`]), and a tree that
//! ended leaves its tables to the next one made with them ([`Spare`]).
//!
//! Why unsafe: std has no inotify. `fs::read_dir` would cost an allocation per entry (a
//! `DirEntry` with its own path) and a `stat` per entry where `getdents64` gives the kind;
//! `fs::symlink_metadata` a `PathBuf` per name. Events and directory entries are parsed in safe
//! code: each record's length is checked against what the call returned before its name is
//! sliced.

use std::{
    ffi::CStr,
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::Path,
};

use blaze_hash::HashMap;
use blaze_mem::{RegionVec, Tag as MemTag};
use std::alloc::Global;

use crate::{Done, FileName, utf8::push_lossy};

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_EXCL_UNLINK;

/// The head of an event, before its name.
const HEAD: usize = 16;

/// The most bytes of names one [`Strings`] keeps: address space reserved once, committed as
/// names come.
const NAMES: usize = 1 << 28;

/// What one `getdents64` of a listing reads at most.
const DIRENTS: usize = 32 << 10;

/// Holes a [`Strings`] keeps before it is squeezed, at the least.
const HOLES: usize = 4 << 10;

/// The most directories one inotify watch walks and watches: past it the watch fails, naming
/// its directory and the count. A million inotify watches are about a gigabyte of the kernel's
/// memory (a watch is about 1 KiB on 64-bit Linux), more than any tree blaze is asked to watch
/// should need; a tree this large wants fanotify, whose start is constant.
pub(crate) const MOST_DIRS: usize = 1 << 20;

/// The most bytes of events an inotify watch keeps while a [`Grow`] of it is out: events of a
/// directory the grow watched, read before the grow is merged, which name a watch the tables
/// do not know yet. Past it the watch's changes are told lost (its directory, `lost: true`), so
/// whoever watches looks at the whole directory again; four reads' worth.
const PARKED: usize = 4 * BUFFER;

#[cfg(test)]
thread_local! {
    /// A test's: this thread's starts take fanotify as refused (`EPERM`), so a privileged run
    /// tests the inotify path too.
    pub(crate) static FANOTIFY_REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A test's planted bug: nothing waits for a watch's start ([`looks_under`]'s callers), so
    /// a look asked after a watch may be answered before it is in place.
    pub(crate) static HOLDS_NOTHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A test's planted bug: a [`Grow`] made on this thread tells none of the files its walk
    /// finds, so a file made in a new directory before its watch is in place is never told.
    pub(crate) static GROW_DROPS_FOUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Where a test holds the thread that walks a tree to watch it: each walk waits at the gate
/// until the test lets one through, so a test can ask while a watch is provably not in place.
/// Nothing outside tests.
#[cfg(test)]
pub(crate) type Gate = Option<crossbeam_channel::Receiver<()>>;
#[cfg(not(test))]
pub(crate) type Gate = ();

/// Waits at `gate` for the test that holds it to let one walk through; a gate whose test let
/// go of it (dropped its end) lets every walk through. Not a wait of blaze's own: there is no
/// gate outside tests.
pub(crate) fn pass(gate: &Gate) {
    #[cfg(test)]
    if let Some(gate) = gate {
        _ = gate.recv();
    }
    #[cfg(not(test))]
    let () = *gate;
}

/// Whether `effect` looks at the disk at a name `under` says is in a directory whose watch is
/// still starting: a stat, a read, a listing or stamps. A driver whose start runs off its loop
/// holds such a look until the watch is in place ([`crate::Effect::Watch`]'s guarantee).
pub(crate) fn looks_under(effect: &crate::Effect, mut under: impl FnMut(&str) -> bool) -> bool {
    use crate::Effect;
    #[cfg(test)]
    if HOLDS_NOTHING.get() {
        return false;
    }
    match effect {
        Effect::Stat { file, .. }
        | Effect::Lstat { file, .. }
        | Effect::ReadAll { file, .. }
        | Effect::Read { file, .. }
        | Effect::ReadLink { file, .. } => under(file.as_str()),
        Effect::Entries { dir, .. } | Effect::List { dir, .. } => under(dir.as_str()),
        Effect::Stamp { files, .. } => files.iter().any(&mut under),
        _ => false,
    }
}

/// Runs `walk`, which watches directories and says how many, under a span `op` naming `dir`,
/// and says the count as a span of its own at the walk's end, inside it, named
/// `{dir}: {n} directories watched`: the probe's spans carry a name and no number
/// ([`crate::lock::Probe::io_begin`]), so a slow start names its directory and its size in the
/// profile this way. The name is made only where a probe is installed.
fn walk_span<R>(op: &'static str, dir: &str, walk: impl FnOnce() -> (R, usize)) -> R {
    crate::lock::io_wait(op, dir, || {
        let (result, watched) = walk();
        if crate::lock::installed().is_some() {
            let said = format!("{dir}: {watched} directories watched");
            crate::lock::io_wait("walked to watch with inotify", &said, || ());
        }
        result
    })
}

/// A tree watched, with fanotify or inotify.
pub(crate) struct Tree(Kind);

enum Kind {
    Fanotify(crate::fanotify::Fanotify),
    Inotify(Inotify),
}

/// What a [`Tree`] that ended leaves for the next made with it ([`Tree::reusing`]): its tables
/// and buffers, emptied, so a watch of a tree no larger than one before allocates nothing.
pub(crate) enum Spare {
    Fanotify(crate::fanotify::Room),
    Inotify(Room),
}

/// How a watch's start went ([`Tree::begin`]).
pub(crate) enum Begun {
    /// Watching already: fanotify's start, a constant number of system calls.
    Watching(Tree),
    /// inotify's: the walk that watches each directory is still to run ([`Walk::run`]).
    Walk(Walk),
}

/// An inotify watch's start still to run: the walk of its tree, which the caller runs where a
/// walk may take long. It holds the watch's tables, the directory's name among them.
pub(crate) struct Walk {
    room: Room,
    nonblocking: bool,
}

impl Walk {
    /// The watched directory, relative to the root.
    pub(crate) fn dir(&self) -> &str {
        &self.room.top
    }

    /// Watches the directory under `root` and every directory under it: a watch of the whole
    /// tree, or an error naming the directory (past [`MOST_DIRS`], or past the system's limit
    /// of inotify watches), never a watch of part of it.
    pub(crate) fn run(self, root: &Path) -> io::Result<Tree> {
        let Walk { room, nonblocking } = self;
        Ok(Tree(Kind::Inotify(Inotify::new(root, nonblocking, room)?)))
    }
}

/// Names one after another in one string, each by where it ends: a list that keeps its room.
#[derive(Default)]
pub(crate) struct Names {
    text: String,
    ends: Vec<u32>,
}

impl Names {
    fn push(&mut self, name: &str) {
        self.text.push_str(name);
        self.ends.push(self.text.len() as u32);
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    fn get(&self, at: usize) -> &str {
        let start = if at == 0 { 0 } else { self.ends[at - 1] as usize };
        &self.text[start..self.ends[at] as usize]
    }

    fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
    }
}

/// Directories an inotify watch heard made, still to be watched: a walk of each, handed to
/// whoever may take long ([`Grow::run`]), as a start's [`Walk`] is. See the module doc for the
/// invariant that loses nothing made in them meanwhile.
pub(crate) struct Grow {
    /// The watch's inotify instance, through a descriptor of its own (`F_DUPFD_CLOEXEC`): the
    /// walk adds its watches to the instance, which stays open while it does even where the
    /// watch ends meanwhile (the instance is closed with its last descriptor).
    fd: OwnedFd,
    /// The most directories the walk may watch: [`MOST_DIRS`] less those watched already.
    most: usize,
    /// What the walk fills: the watch's kept buffers, the fresh directories among them.
    into: Grown,
    #[cfg(test)]
    drops_found: bool,
}

/// What a [`Grow`] found, to merge with [`Tree::grown`]: each directory it watched with its
/// watch, and each file under them.
pub(crate) struct Grown {
    /// The directories walked, relative to the root.
    tops: Names,
    /// Each directory watched, relative to the root, and its watch, by the same index.
    dirs: Names,
    wds: Vec<i32>,
    /// The files found: made before their directory's watch, so with no event of their own.
    files: Batch,
    /// Why the walk stopped short: past [`MOST_DIRS`] or the system's limit of watches.
    error: Option<io::Error>,
}

impl Grown {
    fn new() -> Grown {
        Grown {
            tops: Names::default(),
            dirs: Names::default(),
            wds: Vec::new_in(Global),
            files: Batch::default(),
            error: None,
        }
    }

    fn clear(&mut self) {
        self.tops.clear();
        self.dirs.clear();
        self.wds.clear();
        self.files.clear();
        self.error = None;
    }
}

impl Grow {
    /// The grow not run, `why`: merged, it tells the watch's changes lost, so whoever watches
    /// looks again, and lets the watch's flushes go.
    pub(crate) fn refused(self, why: io::Error) -> Grown {
        let mut into = self.into;
        into.error = Some(why);
        into
    }

    /// Watches each fresh directory and every directory under it, with `walker`'s buffers,
    /// under `root` (its bytes): each directory added to the instance, then listed, so what is
    /// made in it before the add is listed and what is made after has an event. A directory
    /// gone before it is watched is passed over: its removal is an event of its parent's, which
    /// is watched. Past [`Grow::most`] or the system's limit the walk stops, saying why in
    /// [`Grown::error`]. Under a span naming the first directory and saying the count.
    pub(crate) fn run(self, root: &[u8], walker: &mut Walker) -> Grown {
        #[cfg(test)]
        let drops_found = self.drops_found;
        #[cfg(not(test))]
        let drops_found = false;
        let Grow { fd, most, mut into, .. } = self;
        let Grown { tops, dirs, wds, files, error } = &mut into;
        let first = if tops.is_empty() { "" } else { tops.get(0) };
        walk_span("walk a new directory to watch with inotify", first, || {
            let fd = fd.as_raw_fd();
            for at in 0..tops.len() {
                let top = tops.get(at);
                let walked = walker.walk(
                    root,
                    top,
                    |path, rel, _| {
                        if wds.len() >= most {
                            return Err(io::Error::other(format!(
                                "watching {top} with inotify: {MOST_DIRS} directories watched, \
                                 the most one watch walks (MOST_DIRS); a tree this large wants \
                                 fanotify (CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH)"
                            )));
                        }
                        // SAFETY: an inotify descriptor and a NUL-terminated path.
                        let wd = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), MASK) };
                        if wd < 0 {
                            let error = io::Error::last_os_error();
                            if error.raw_os_error() == Some(libc::ENOENT) {
                                return Ok(Visit::Skip);
                            }
                            return Err(io::Error::other(format!(
                                "watching {} with inotify, {} directories into {top}: {error}{}",
                                path.to_string_lossy(),
                                wds.len(),
                                if error.raw_os_error() == Some(libc::ENOSPC) {
                                    " (the most the system lets a user watch: \
                                     fs.inotify.max_user_watches)"
                                } else {
                                    ""
                                }
                            )));
                        }
                        dirs.push(rel);
                        wds.push(wd);
                        Ok(Visit::List(None))
                    },
                    |file| {
                        if !drops_found {
                            files.push(file);
                        }
                    },
                );
                if let Err(why) = walked {
                    *error = Some(why);
                    break;
                }
            }
            ((), wds.len())
        });
        // The walk's descriptor of the instance, closed: the watch's own stays open.
        drop(fd);
        into
    }
}

impl Tree {
    /// Starts watching `dir` under `root` and everything under it, into the tables `spare`
    /// kept when there are any: with fanotify unless the process lacks `CAP_SYS_ADMIN` or
    /// `CAP_DAC_READ_SEARCH` (the call fails with `EPERM`), whose start is a constant number of
    /// system calls; with inotify then, whose start is a [`Walk`] left to the caller. Any other
    /// failure of fanotify's is the watch's.
    pub(crate) fn begin(
        spare: Option<Spare>,
        root: &Path,
        dir: &FileName,
        nonblocking: bool,
    ) -> io::Result<Begun> {
        let (fanotify, inotify) = match spare {
            Some(Spare::Fanotify(room)) => (Some(room), None),
            Some(Spare::Inotify(room)) => (None, Some(room)),
            None => (None, None),
        };
        #[cfg(test)]
        let fanotify_refused = FANOTIFY_REFUSED.get();
        #[cfg(not(test))]
        let fanotify_refused = false;
        let started = match fanotify_refused {
            true => Err(io::Error::from_raw_os_error(libc::EPERM)),
            false => crate::fanotify::Fanotify::new(root, dir, nonblocking, fanotify),
        };
        match started {
            Ok(it) => Ok(Begun::Watching(Tree(Kind::Fanotify(it)))),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                let mut room = match inotify {
                    Some(room) => room,
                    None => Room::new()?,
                };
                room.top.clear();
                room.top.push_str(dir.as_str());
                Ok(Begun::Walk(Walk { room, nonblocking }))
            }
            Err(error) => Err(error),
        }
    }

    /// [`Tree::begin`] with its walk, if it has one, run here: for a caller on a thread of its
    /// own (the threads driver's errand).
    pub(crate) fn reusing(
        spare: Option<Spare>,
        root: &Path,
        dir: &FileName,
        nonblocking: bool,
    ) -> io::Result<Tree> {
        match Tree::begin(spare, root, dir, nonblocking)? {
            Begun::Watching(tree) => Ok(tree),
            Begun::Walk(walk) => walk.run(root),
        }
    }

    /// Stops watching, its descriptor closed; what it kept is the next tree's.
    pub(crate) fn end(self) -> Spare {
        match self.0 {
            Kind::Fanotify(it) => Spare::Fanotify(it.end()),
            Kind::Inotify(it) => Spare::Inotify(it.end()),
        }
    }

    /// The descriptor to read events from.
    pub(crate) fn raw(&self) -> RawFd {
        match &self.0 {
            Kind::Fanotify(it) => it.raw(),
            Kind::Inotify(it) => it.raw(),
        }
    }

    /// What the events read into `bytes` say: a [`Done::Changed`].
    pub(crate) fn take(&mut self, bytes: &[u8]) -> Done {
        match &mut self.0 {
            Kind::Fanotify(it) => it.take(bytes),
            Kind::Inotify(it) => it.take(bytes),
        }
    }

    /// The directories [`Tree::take`] heard made, as a [`Grow`] to run where a walk may take
    /// long; `None` where there are none, or one of this watch's grows is out already (the
    /// next goes once it is merged, so at most one is out per watch). fanotify marks the whole
    /// file system and never grows. Fails only where the instance's descriptor cannot be
    /// duplicated, naming the watch; the fresh directories are dropped, and the caller tells
    /// the watch's changes lost ([`lost`]), so whoever watches looks again.
    pub(crate) fn grow(&mut self) -> io::Result<Option<Grow>> {
        match &mut self.0 {
            Kind::Fanotify(_) => Ok(None),
            Kind::Inotify(it) => it.grow(),
        }
    }

    /// What `grown` found, merged: its directories' watches known from here, and what it found
    /// and what was kept meanwhile told, as a [`Done::Changed`].
    pub(crate) fn grown(&mut self, grown: Grown) -> Done {
        match &mut self.0 {
            Kind::Fanotify(_) => unreachable!("a fanotify watch marks the file system, never grows"),
            Kind::Inotify(it) => it.grown(grown),
        }
    }

    /// Whether a grow of this watch is out or fresh directories wait for one: a flush waits
    /// until neither, so it is answered after what the grow finds.
    pub(crate) fn pending(&self) -> bool {
        match &self.0 {
            Kind::Fanotify(_) => false,
            Kind::Inotify(it) => it.room.growing || !it.room.fresh.is_empty(),
        }
    }

    /// Runs here each [`Grow`] the watch has, merged, telling each [`Tree::grown`] to `each`:
    /// for a caller on a thread of its own (the threads driver's errand), after each take and
    /// before a flush is answered.
    pub(crate) fn grow_here(&mut self, mut each: impl FnMut(Done)) -> io::Result<()> {
        let Kind::Inotify(it) = &mut self.0 else { return Ok(()) };
        while let Some(grow) = it.grow()? {
            let mut walker = std::mem::take(&mut it.room.walker);
            let root = std::mem::take(&mut it.room.root);
            let grown = grow.run(&root, &mut walker);
            it.room.root = root;
            it.room.walker = walker;
            each(it.grown(grown));
        }
        Ok(())
    }

    /// Reads every event queued on the descriptor until it has none, handing what each read
    /// says to `each`: a flush's barrier. inotify and fanotify queue an event in the call that
    /// made the change, so once the descriptor is empty every change made before this began
    /// has been read. Nothing else may read the descriptor meanwhile; the caller holds every
    /// other reader off (the threads driver reads it on this thread only; the ring driver
    /// only once the ring's read of it has completed).
    pub(crate) fn drain(
        &mut self,
        buffer: &mut [u8],
        mut each: impl FnMut(Done),
    ) -> io::Result<()> {
        loop {
            let mut fds = [libc::pollfd { fd: self.raw(), events: libc::POLLIN, revents: 0 }];
            let ready = match crate::raw::poll(&mut fds, 0) {
                Ok(ready) => ready,
                Err(error) if error.raw_os_error() == Some(libc::EINTR) => continue,
                Err(error) => return Err(error),
            };
            if ready == 0 || fds[0].revents & libc::POLLIN == 0 {
                return Ok(());
            }
            match self.read(buffer) {
                Ok(0) => return Ok(()),
                Ok(n) => each(self.take(&buffer[..n])),
                Err(error) => match error.raw_os_error() {
                    Some(libc::EINTR) => {}
                    Some(libc::EAGAIN) => return Ok(()),
                    _ => return Err(error),
                },
            }
        }
    }

    /// One read of the queued events into `buffer`: how many bytes.
    pub(crate) fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buffer` is a live, exclusive buffer of `buffer.len()` bytes the call writes
        // at most that many of, and the descriptor is this tree's, open while `self` is
        // borrowed (closed only by `end`, which takes it).
        let n = unsafe { libc::read(self.raw(), buffer.as_mut_ptr().cast(), buffer.len()) };
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }
}

/// Where a name is in a [`Strings`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Span {
    start: u32,
    len: u32,
}

impl Span {
    /// `first` and `then`, pushed one right after the other, as one span.
    pub(crate) fn joined(first: Span, then: Span) -> Span {
        debug_assert_eq!(first.start + first.len, then.start);
        Span { start: first.start, len: first.len + then.len }
    }
}

/// Names kept for a watch's life, one after another in a column of reserved memory, tagged
/// [`MemTag::Io`]. A name let go leaves a hole; once holes are most of the column its owner
/// squeezes them out: [`Strings::keep`] each span still kept, [`Strings::squeeze`], then
/// [`Strings::moved`] each span to where its name is now.
pub(crate) struct Strings {
    bytes: RegionVec<u8>,
    holes: usize,
    /// Each name kept through a squeeze: where it started, its length, where it starts now.
    moves: Vec<(u32, u32, u32)>,
}

impl Strings {
    pub(crate) fn new() -> io::Result<Strings> {
        let bytes = RegionVec::new(NAMES, MemTag::Io).map_err(io::Error::other)?;
        // alloc: none, an empty vector, grown by the first squeeze and reused.
        Ok(Strings { bytes, holes: 0, moves: Vec::new() })
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Span {
        let start = self.bytes.len() as u32;
        self.bytes.extend_from_slice(bytes);
        Span { start, len: bytes.len() as u32 }
    }

    pub(crate) fn bytes(&self, span: Span) -> &[u8] {
        &self.bytes[span.start as usize..(span.start + span.len) as usize]
    }

    /// The name at `span`, pushed as a `str`.
    pub(crate) fn str(&self, span: Span) -> &str {
        std::str::from_utf8(self.bytes(span)).expect("pushed as a str")
    }

    /// `span`'s name is no longer kept.
    pub(crate) fn free(&mut self, span: Span) {
        self.holes += span.len as usize;
    }

    /// Whether holes are most of it, so that its owner should squeeze it.
    pub(crate) fn wasteful(&self) -> bool {
        self.holes > HOLES && self.holes * 2 > self.bytes.len()
    }

    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.holes = 0;
        self.moves.clear();
    }

    /// `span` is still kept, through the squeeze that follows.
    pub(crate) fn keep(&mut self, span: Span) {
        if span.len > 0 {
            self.moves.push((span.start, span.len, 0));
        }
    }

    /// Moves every name kept down over the holes, in order.
    pub(crate) fn squeeze(&mut self) {
        let Strings { bytes, holes, moves } = self;
        moves.sort_unstable_by_key(|it| it.0);
        let mut to = 0;
        for (start, len, now) in moves.iter_mut() {
            bytes.copy_within(*start as usize..(*start + *len) as usize, to as usize);
            *now = to;
            to += *len;
        }
        bytes.truncate(to as usize);
        *holes = 0;
    }

    /// Where `span`'s name is since the squeeze; [`Strings::keep`] said it was kept.
    pub(crate) fn moved(&self, span: Span) -> Span {
        if span.len == 0 {
            return Span::default();
        }
        let at = self
            .moves
            .binary_search_by_key(&span.start, |it| it.0)
            .expect("kept through the squeeze");
        Span { start: self.moves[at].2, len: span.len }
    }

    /// The squeeze is over.
    pub(crate) fn squeezed(&mut self) {
        self.moves.clear();
    }
}

/// The names one read of events tells, gathered and deduplicated before any is made a
/// [`FileName`], so that a batch allocates its payload and nothing else.
#[derive(Default)]
pub(crate) struct Batch {
    names: String,
    spans: Vec<(u32, u32)>,
}

impl Batch {
    /// Adds `name` where it is a [`FileName`].
    pub(crate) fn push(&mut self, name: &str) {
        if !valid(name) {
            return;
        }
        let start = self.names.len() as u32;
        self.names.push_str(name);
        self.spans.push((start, name.len() as u32));
    }

    /// Adds each name of `other`.
    fn extend(&mut self, other: &Batch) {
        for span in &other.spans {
            self.push(at(&other.names, span));
        }
    }

    fn clear(&mut self) {
        self.names.clear();
        self.spans.clear();
    }

    /// The batch as a [`Done::Changed`], each name once and sorted, emptied for the next.
    pub(crate) fn changed(&mut self, lost: bool) -> Done {
        let Batch { names, spans } = self;
        let names = names.as_str();
        spans.sort_unstable_by(|a, b| at(names, a).cmp(at(names, b)));
        spans.dedup_by(|a, b| at(names, a) == at(names, b));
        let files = spans
            .iter()
            .map(|it| FileName::new(at(names, it)).expect("checked as it was pushed"))
            .collect(); // alloc: the completion's payload, one per name changed.
        self.names.clear();
        self.spans.clear();
        Done::Changed { files, lost }
    }
}

/// A watch's changes told lost, with no names: whoever watches looks at the whole directory
/// again ([`Tree::grow`]'s failure).
pub(crate) fn lost() -> Done {
    Done::Changed { files: Vec::new_in(Global), lost: true }
}

fn at<'a>(names: &'a str, &(start, len): &(u32, u32)) -> &'a str {
    &names[start as usize..(start + len) as usize]
}

/// Whether `name` is a [`FileName`], as [`FileName::new`] says, without making one.
pub(crate) fn valid(name: &str) -> bool {
    FileName::is_valid(name)
}

/// Whether `name` is `dir` or under it, as [`FileName::is_under`] says.
pub(crate) fn is_under(name: &str, dir: &str) -> bool {
    name == dir || name.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// What a walk does with a directory it reached.
pub(crate) enum Visit {
    /// Passes over what is in it.
    Skip,
    /// Lists it: through the descriptor given, or one opened for it.
    List(Option<OwnedFd>),
}

/// A walk of a directory and every directory under it, with what it reads into kept for the
/// next walk.
#[derive(Default)]
pub(crate) struct Walker {
    /// The directories still to visit, relative to the root, one after another; each of `ends`
    /// says where one ends.
    todo: String,
    ends: Vec<u32>,
    /// The directory being visited, relative to the root, and an entry of it.
    dir: String,
    child: String,
    /// The directory being visited under the root, NUL-terminated.
    path: Vec<u8>,
    dirents: Vec<u8>,
}

impl Walker {
    /// Visits `rel` under `root`, handing `visit` its path under the root NUL-terminated, `rel`
    /// and whether it is the first, then each directory under one `visit` lists, depth first.
    /// Each other entry of a directory listed goes to `file`, named relative to the root. A
    /// directory that cannot be listed is passed over, as one gone since it was found is; the
    /// walk ends at the first error of `visit`'s.
    pub(crate) fn walk(
        &mut self,
        root: &[u8],
        rel: &str,
        mut visit: impl FnMut(&CStr, &str, bool) -> io::Result<Visit>,
        mut file: impl FnMut(&str),
    ) -> io::Result<()> {
        let Walker { todo, ends, dir, child, path, dirents } = self;
        if dirents.len() < DIRENTS {
            dirents.resize(DIRENTS, 0);
        }
        todo.clear();
        ends.clear();
        todo.push_str(rel);
        ends.push(todo.len() as u32);
        let mut first = true;
        while let Some(end) = ends.pop() {
            let start = ends.last().map_or(0, |&it| it as usize);
            dir.clear();
            dir.push_str(&todo[start..end as usize]);
            todo.truncate(start);
            path.clear();
            join(path, root, dir.as_bytes());
            path.push(0);
            let c = CStr::from_bytes_with_nul(&path[..])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
            let fd = match visit(c, dir.as_str(), std::mem::take(&mut first))? {
                Visit::Skip => continue,
                Visit::List(Some(fd)) => fd,
                Visit::List(None) => match open_dir(c) {
                    Ok(fd) => fd,
                    Err(_) => continue,
                },
            };
            list(fd.as_raw_fd(), dirents, |name, is_dir| {
                child.clear();
                if !dir.is_empty() {
                    child.push_str(dir.as_str());
                    child.push('/');
                }
                push_lossy(child, name);
                if is_dir {
                    todo.push_str(child.as_str());
                    ends.push(todo.len() as u32);
                } else {
                    file(child.as_str());
                }
            });
        }
        Ok(())
    }
}

/// `root` joined with `rel` into `into`, as [`Path::join`] joins them.
pub(crate) fn join(into: &mut Vec<u8>, root: &[u8], rel: &[u8]) {
    into.extend_from_slice(root);
    if !root.is_empty() && !root.ends_with(b"/") {
        into.push(b'/');
    }
    into.extend_from_slice(rel);
}

/// The directory at `path`, opened to list it.
fn open_dir(path: &CStr) -> io::Result<OwnedFd> {
    crate::raw::open_dir(path)
}

/// Each entry of the directory open as `fd` but `.` and `..`, with whether it is a directory
/// (a link is not followed), read in batches into `buffer`. Stops at the first error, as the
/// listing of a directory gone does.
fn list(fd: RawFd, buffer: &mut [u8], mut each: impl FnMut(&[u8], bool)) {
    /// `struct linux_dirent64`: `d_ino`, `d_off`, `d_reclen`, `d_type`, then the name.
    const NAME: usize = 19;
    loop {
        // SAFETY: an open directory and a buffer of `buffer.len()` bytes, which the call
        // writes at most.
        let got =
            unsafe { libc::syscall(libc::SYS_getdents64, fd, buffer.as_mut_ptr(), buffer.len()) };
        if got <= 0 {
            return;
        }
        let got = (got as usize).min(buffer.len());
        let mut at = 0;
        while at + NAME <= got {
            let len = u16::from_ne_bytes([buffer[at + 16], buffer[at + 17]]) as usize;
            if len <= NAME || at + len > got {
                return;
            }
            let kind = buffer[at + 18];
            let name = &buffer[at + NAME..at + len];
            let name = &name[..name.iter().position(|&it| it == 0).unwrap_or(name.len())];
            at += len;
            if name == b"." || name == b".." {
                continue;
            }
            let is_dir = match kind {
                libc::DT_DIR => true,
                libc::DT_UNKNOWN => is_dir_at(fd, name),
                _ => false,
            };
            each(name, is_dir);
        }
    }
}

/// Whether `name` in the directory open as `dir` is a directory, not followed through a link:
/// for a file system that gives no entry's type.
fn is_dir_at(dir: RawFd, name: &[u8]) -> bool {
    let mut c = [0u8; 256];
    if name.len() >= c.len() {
        return false;
    }
    c[..name.len()].copy_from_slice(name);
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: an open directory, a NUL-terminated name and a `stat` to fill.
    let got = unsafe {
        libc::fstatat(dir, c.as_ptr().cast(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    };
    // SAFETY: the call succeeded, so it filled the `stat`.
    got == 0 && unsafe { stat.assume_init_ref() }.st_mode & libc::S_IFMT == libc::S_IFDIR
}

struct Inotify {
    fd: OwnedFd,
    room: Room,
}

/// An inotify watch's tables and buffers, kept from one watch to the next.
pub(crate) struct Room {
    /// Each directory watched, by its watch, relative to the root ("" for the root), in
    /// `names`.
    dirs: HashMap<i32, Span, Global>,
    names: Strings,
    root: Vec<u8>,
    /// The watched directory. inotify watches an inode, not a name: once it is deleted or
    /// moved, nothing made at its name again is heard of, so that is told as a change of the
    /// directory itself, lost (whoever watches watches again).
    top: String,
    rel: String,
    batch: Batch,
    walker: Walker,
    /// Directories heard made and not yet handed to a [`Grow`].
    fresh: Names,
    /// Whether a [`Grow`] of this watch is out.
    growing: bool,
    /// The buffers a grow fills, kept from the last; `None` while one is out.
    spare: Option<Grown>,
    /// Events read while a grow is out that name a watch not known yet, up to [`PARKED`]
    /// bytes, taken again as the grow is merged.
    parked: Vec<u8>,
}

impl Room {
    fn new() -> io::Result<Room> {
        Ok(Room {
            dirs: HashMap::new_in(Global),
            names: Strings::new()?,
            root: Vec::new_in(Global),
            top: String::new(),
            rel: String::new(),
            batch: Batch::default(),
            walker: Walker::default(),
            fresh: Names::default(),
            growing: false,
            spare: None,
            parked: Vec::new_in(Global),
        })
    }
}

impl Inotify {
    /// Watches `room.top` under `root` and every directory under it, into `room`'s tables: a
    /// walk of the whole tree, under a span naming the directory.
    fn new(root: &Path, nonblocking: bool, room: Room) -> io::Result<Inotify> {
        let flags = libc::IN_CLOEXEC | if nonblocking { libc::IN_NONBLOCK } else { 0 };
        // SAFETY: no pointers; a descriptor of its own or -1.
        let fd = unsafe { libc::inotify_init1(flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut room = room;
        room.root.clear();
        room.root.extend_from_slice(root.as_os_str().as_bytes());
        // Taken out for the walk, which writes the tables beside it, and put back after.
        let top = std::mem::take(&mut room.top);
        let mut watch = Inotify { fd, room };
        // The wait of whoever waits for this start is this walk (rule every-wait-says-what):
        // 128 s of one, on the loop thread and in no span, ended a daemon on 2026-11-14.
        let walked = walk_span("walk to watch with inotify", &top, || {
            let walked = watch.add_tree(&top);
            (walked, watch.room.dirs.len())
        });
        watch.room.top = top;
        walked?;
        Ok(watch)
    }

    fn end(self) -> Room {
        let Inotify { fd, mut room } = self;
        drop(fd);
        room.dirs.clear();
        room.names.clear();
        // A grow still out keeps its own descriptor of the instance and its buffers; what it
        // finds is of a watch that ended, and its driver drops it.
        room.fresh.clear();
        room.growing = false;
        room.parked.clear();
        room
    }

    /// The fresh directories as a [`Grow`]: see [`Tree::grow`].
    fn grow(&mut self) -> io::Result<Option<Grow>> {
        let room = &mut self.room;
        if room.growing || room.fresh.is_empty() {
            return Ok(None);
        }
        let fd = match self.fd.try_clone() {
            Ok(fd) => fd,
            Err(error) => {
                room.fresh.clear();
                return Err(io::Error::other(format!(
                    "watching {} with inotify: its new directories cannot be walked, the \
                     descriptor to walk them with not made: {error}",
                    room.top
                )));
            }
        };
        let mut into = match room.spare.take() {
            Some(it) => it,
            None => Grown::new(),
        };
        into.clear();
        std::mem::swap(&mut into.tops, &mut room.fresh);
        room.growing = true;
        Ok(Some(Grow {
            fd,
            most: MOST_DIRS.saturating_sub(room.dirs.len()),
            into,
            #[cfg(test)]
            drops_found: GROW_DROPS_FOUND.get(),
        }))
    }

    /// `grown` merged: see [`Tree::grown`]. The events kept while it was out are taken again
    /// now that the watches they name are known; one that still names none is of a directory
    /// gone since, and is passed over as any such event is.
    fn grown(&mut self, mut grown: Grown) -> Done {
        let mut lost = false;
        {
            let Room { dirs, names, batch, top, growing, .. } = &mut self.room;
            *growing = false;
            for at in 0..grown.dirs.len() {
                let span = names.push(grown.dirs.get(at).as_bytes());
                if let Some(was) = dirs.insert(grown.wds[at], span) {
                    names.free(was);
                }
            }
            batch.extend(&grown.files);
            // Past the bound or the system's limit, part of the new tree has no watch: told
            // as lost, so whoever watches looks at it again, and, as a watch whose start stops
            // there, it hears nothing more of that part.
            if grown.error.is_some() {
                batch.push(top);
                lost = true;
            }
        }
        // Its buffers, the room of the directories it walked among them, are the next grow's.
        grown.clear();
        self.room.spare = Some(grown);
        let mut rel = std::mem::take(&mut self.room.rel);
        let parked = std::mem::take(&mut self.room.parked);
        self.events(&parked, &mut rel, &mut lost);
        self.room.parked = parked;
        self.room.parked.clear();
        if self.room.names.wasteful() {
            self.squeeze();
        }
        self.room.rel = rel;
        self.room.batch.changed(lost)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Watches `rel` and the directories under it: a start's walk. A directory made after it is
    /// a [`Grow`]'s.
    fn add_tree(&mut self, rel: &str) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        let Room { dirs, names, root, walker, .. } = &mut self.room;
        let tree = rel;
        walker.walk(
            root,
            rel,
            |path, rel, _| {
                if dirs.len() >= MOST_DIRS {
                    return Err(io::Error::other(format!(
                        "watching {tree} with inotify: {} directories watched, the most one \
                         watch walks (MOST_DIRS); a tree this large wants fanotify \
                         (CAP_SYS_ADMIN and CAP_DAC_READ_SEARCH), whose start is constant",
                        dirs.len()
                    )));
                }
                // SAFETY: an inotify descriptor and a NUL-terminated path.
                let wd = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), MASK) };
                if wd < 0 {
                    let error = io::Error::last_os_error();
                    // Gone before it was watched: its removal is an event of its parent's.
                    if error.raw_os_error() == Some(libc::ENOENT) && !dirs.is_empty() {
                        return Ok(Visit::Skip);
                    }
                    if error.raw_os_error() == Some(libc::ENOSPC) {
                        return Err(io::Error::other(format!(
                            "watching {} with inotify: {} directories watched, the most the \
                             system lets a user watch (fs.inotify.max_user_watches): {error}",
                            path.to_string_lossy(),
                            dirs.len()
                        )));
                    }
                    return Err(error);
                }
                let span = names.push(rel.as_bytes());
                if let Some(was) = dirs.insert(wd, span) {
                    names.free(was);
                }
                Ok(Visit::List(None))
            },
            |_| {},
        )
    }

    /// What the events in `bytes` say. A directory they say was made is told at once and kept
    /// as fresh, for a [`Grow`]: never walked here, which is the loop's thread on the ring.
    fn take(&mut self, bytes: &[u8]) -> Done {
        let mut rel = std::mem::take(&mut self.room.rel);
        let mut lost = false;
        self.events(bytes, &mut rel, &mut lost);
        if self.room.names.wasteful() {
            self.squeeze();
        }
        self.room.rel = rel;
        self.room.batch.changed(lost)
    }

    /// Each event in `bytes` into the batch, `rel` the room a name is made in.
    fn events(&mut self, bytes: &[u8], rel: &mut String, lost: &mut bool) {
        let mut at = 0;
        while at + HEAD <= bytes.len() {
            let word = |n: usize| {
                u32::from_ne_bytes(bytes[at + n..at + n + 4].try_into().expect("four bytes"))
            };
            let (wd, mask, len) = (word(0) as i32, word(4), word(12) as usize);
            let event = &bytes[at..(at + HEAD + len).min(bytes.len())];
            let name_bytes = &event[HEAD..];
            at += HEAD + len;
            if mask & libc::IN_Q_OVERFLOW != 0 {
                // What was dropped may be the watched directory's own end.
                self.room.batch.push(&self.room.top);
                *lost = true;
                continue;
            }
            let known = self.room.dirs.contains_key(&wd);
            // Of a directory a grow out watched: kept until the grow is merged.
            if !known && self.room.growing {
                if self.room.parked.len() + event.len() > PARKED {
                    self.room.batch.push(&self.room.top);
                    *lost = true;
                } else {
                    self.room.parked.extend_from_slice(event);
                }
                continue;
            }
            if mask & libc::IN_IGNORED != 0 {
                if let Some(span) = self.room.dirs.remove(&wd) {
                    let top = self.room.names.str(span) == self.room.top;
                    self.room.names.free(span);
                    if top {
                        self.room.batch.push(&self.room.top);
                        *lost = true;
                    }
                }
                continue;
            }
            let Some(&span) = self.room.dirs.get(&wd) else { continue };
            let dir = self.room.names.str(span);
            if mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 && dir == self.room.top {
                *lost = true;
            }
            rel.clear();
            rel.push_str(dir);
            let end = name_bytes.iter().position(|&it| it == 0).unwrap_or(name_bytes.len());
            if end > 0 {
                if !rel.is_empty() {
                    rel.push('/');
                }
                push_lossy(rel, &name_bytes[..end]);
            }
            if mask & libc::IN_ISDIR != 0 && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                self.room.fresh.push(rel);
            }
            self.room.batch.push(rel);
        }
    }

    /// The directories' names moved down over those of directories no longer watched.
    fn squeeze(&mut self) {
        let Room { dirs, names, .. } = &mut self.room;
        for span in dirs.values() {
            names.keep(*span);
        }
        names.squeeze();
        for span in dirs.values_mut() {
            *span = names.moved(*span);
        }
        names.squeezed();
    }
}

/// Room for many events at once.
pub(crate) const BUFFER: usize = 64 << 10;

#[cfg(test)]
mod tests {
    use super::*;

    /// Names pushed and let go, squeezed whenever holes are most of the column, read back as
    /// a map of owned strings beside them has them, over many seeds.
    #[test]
    fn strings_squeezed_keep_every_name_kept() {
        for seed in 0..32 {
            let mut rng = crate::sim::Rng::new(seed);
            let mut strings = Strings::new().unwrap();
            let mut ours: HashMap<u32, Span, Global> = HashMap::new_in(Global);
            let mut theirs: HashMap<u32, String, Global> = HashMap::new_in(Global);
            for step in 0..4_000u32 {
                let key = rng.below(64) as u32;
                if rng.below(3) == 0 {
                    if let Some(span) = ours.remove(&key) {
                        strings.free(span);
                    }
                    theirs.remove(&key);
                } else {
                    let name = format!("dir/{step}/{}", "x".repeat(rng.below(40) as usize));
                    if let Some(span) = ours.insert(key, strings.push(name.as_bytes())) {
                        strings.free(span);
                    }
                    theirs.insert(key, name);
                }
                if strings.wasteful() {
                    for span in ours.values() {
                        strings.keep(*span);
                    }
                    strings.squeeze();
                    for span in ours.values_mut() {
                        *span = strings.moved(*span);
                    }
                    strings.squeezed();
                    assert!(!strings.wasteful());
                }
                for (key, name) in &theirs {
                    assert_eq!(strings.str(ours[key]), name, "seed {seed} step {step}");
                }
            }
        }
    }

    /// A batch tells each valid name once, sorted, and allocates the names and their vector
    /// alone once warm.
    #[test]
    fn a_batch_is_deduplicated_before_its_names_are_made() {
        let mut batch = Batch::default();
        for name in ["src/b.rs", "src/a.rs", "src/b.rs", "", "/abs", "src/../x", "src/a.rs"] {
            batch.push(name);
        }
        let Done::Changed { files, lost } = batch.changed(true) else { unreachable!() };
        assert!(lost);
        let names: Vec<&str> = files.iter().map(FileName::as_str).collect();
        assert_eq!(names, ["src/a.rs", "src/b.rs"]);
        let done = blaze_alloc_count::assert_at_most(3, "a warm batch of two names", || {
            for name in ["src/b.rs", "src/a.rs", "src/b.rs"] {
                batch.push(name);
            }
            batch.changed(false)
        });
        assert!(matches!(done, Done::Changed { files, lost: false } if files.len() == 2));
        let empty = blaze_alloc_count::assert_at_most(0, "an empty batch", || batch.changed(false));
        assert_eq!(empty, Done::Changed { files: Vec::new(), lost: false });
    }

    /// A walk visits every directory and hands over every other entry, named as `std::fs`
    /// would name them, allocating nothing once its buffers are grown.
    #[test]
    fn a_walk_finds_what_read_dir_finds() {
        let root = crate::temp::TempDir::for_test("blaze-walk-");
        for file in ["top/a", "top/b/c", "top/b/d/e", "top/f/g"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file).unwrap();
        }
        std::os::unix::fs::symlink("b", root.join("top/link")).unwrap();
        let base = root.as_os_str().as_bytes();
        let mut walker = Walker::default();
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        walker
            .walk(
                base,
                "top",
                |path, rel, first| {
                    assert_eq!(first, rel == "top");
                    assert_eq!(path.to_bytes(), root.join(rel).as_os_str().as_bytes());
                    dirs.push(rel.to_owned());
                    Ok(Visit::List(None))
                },
                |file| files.push(file.to_owned()),
            )
            .unwrap();
        dirs.sort();
        files.sort();
        assert_eq!(dirs, ["top", "top/b", "top/b/d", "top/f"]);
        assert_eq!(files, ["top/a", "top/b/c", "top/b/d/e", "top/f/g", "top/link"]);
        let (mut visited, mut found) = (0, 0);
        blaze_alloc_count::assert_at_most(0, "a warm walk", || {
            walker.walk(
                base,
                "top",
                |_, _, _| {
                    visited += 1;
                    Ok(Visit::List(None))
                },
                |_| found += 1,
            )
        })
        .unwrap();
        assert_eq!((visited, found), (4, 5));
        std::fs::remove_dir_all(root).unwrap();
    }
}
