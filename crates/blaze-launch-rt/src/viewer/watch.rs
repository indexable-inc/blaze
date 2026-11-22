//! The kernel's word that the runs directory changed or a run's process exited, as
//! descriptors the viewer's one wait polls: on macOS one kqueue holding the directory's
//! `EVFILT_VNODE` (`NOTE_WRITE`: an entry added, removed or renamed; `NOTE_DELETE` and
//! `NOTE_RENAME`: the directory itself went) and an `EVFILT_PROC` `NOTE_EXIT` per process; on
//! Linux an inotify descriptor on the directory and a pidfd per process. Nothing here waits
//! or reads a clock: [`Watch::poll_fds`] says what to poll, [`Watch::take`] reads what is
//! ready without blocking. It holds no memory of its own (a fixed table of pidfds on Linux),
//! so it is made before the viewer's thread and moved into it.

#![blaze::driver]

use core::fmt;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use blaze_sys::Errno;

/// What the kernel said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seen {
    /// An entry of the runs directory was added, removed, renamed or written and closed.
    DirChanged,
    /// The runs directory itself was deleted or moved.
    DirGone,
    /// Process `pid` exited.
    Exited(u32),
}

/// A watch call refused: which, and its errno.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WatchError {
    pub call: &'static str,
    pub errno: u16,
    pub name: &'static str,
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} refused: {} ({})", self.call, self.name, self.errno)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refused(call: &'static str, errno: Errno) -> WatchError {
    WatchError { call, errno: errno.raw(), name: errno.name() }
}

/// How a process's exit is watched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    /// Its exit will be [`Seen::Exited`].
    Watched,
    /// It is gone already.
    Gone,
}

/// The pollfd of descriptor `fd`, for its reads.
pub fn pollin(fd: i32) -> libc::pollfd {
    libc::pollfd { fd, events: libc::POLLIN, revents: 0 }
}

#[cfg(target_os = "macos")]
pub use self::macos::Watch;

#[cfg(target_os = "linux")]
pub use self::linux::Watch;

#[cfg(target_os = "macos")]
mod macos {
    use blaze_own::AVec;
    use blaze_sys::{
        Errno, Path, Timespec,
        posix::io::{CWD, Filter, Kevent, KeventFlags, NoteFlags, OpenFlags, OwnedFd, kevent, kqueue, openat},
    };

    use super::{Exit, Seen, WatchError, pollin, refused};

    /// The events one [`Watch::take`] call reads at a time.
    const EVENTS: usize = 32;

    /// The kqueue and the runs directory's descriptor it watches.
    pub struct Watch {
        kq: OwnedFd,
        /// Held open for its `EVFILT_VNODE`: the filter lasts while the descriptor does.
        _dir: OwnedFd,
    }

    impl Watch {
        /// A watch on the directory `dir`.
        pub fn new(dir: &core::ffi::CStr) -> Result<Watch, WatchError> {
            let kq = kqueue().map_err(|errno| refused("kqueue()", errno))?;
            let flags = OpenFlags::READ | OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC;
            let dir = openat(CWD, Path::new(dir), flags, 0).map_err(|errno| refused("open(the runs directory)", errno))?;
            let ident = usize::try_from(dir.fd().raw()).map_err(|_| refused("open(the runs directory)", Errno::BadF))?;
            let change = Kevent {
                ident,
                filter: Filter::VNODE,
                flags: KeventFlags::ADD | KeventFlags::CLEAR,
                fflags: NoteFlags::WRITE | NoteFlags::DELETE | NoteFlags::RENAME,
                data: 0,
                udata: 0,
            };
            kevent(kq.fd(), &[change], &mut [], Some(Timespec::ZERO))
                .map_err(|errno| refused("kevent(EVFILT_VNODE, the runs directory)", errno))?;
            Ok(Watch { kq, _dir: dir })
        }

        /// The descriptors to poll for reads: the kqueue's alone (it polls readable while it
        /// holds an event).
        pub fn poll_fds(&self, into: &mut AVec<'_, libc::pollfd>) {
            into.push(pollin(self.kq.fd().raw()));
        }

        /// Process `pid`'s exit watched: an `EVFILT_PROC` `NOTE_EXIT`, one shot.
        pub fn watch_exit(&mut self, pid: u32) -> Result<Exit, Errno> {
            let ident = usize::try_from(pid).map_err(|_| Errno::Inval)?;
            let change = Kevent {
                ident,
                filter: Filter::PROC,
                flags: KeventFlags::ADD | KeventFlags::ONESHOT,
                fflags: NoteFlags::EXIT,
                data: 0,
                udata: 0,
            };
            // With no room for events, a change that fails is the call's error.
            match kevent(self.kq.fd(), &[change], &mut [], Some(Timespec::ZERO)) {
                Ok(_) => Ok(Exit::Watched),
                Err(Errno::Srch) => Ok(Exit::Gone),
                Err(errno) => Err(errno),
            }
        }

        /// What the kqueue holds, read without waiting, appended to `seen`; `polled` is what
        /// [`Watch::poll_fds`] pushed, after the poll.
        pub fn take(&mut self, polled: &[libc::pollfd], seen: &mut AVec<'_, Seen>) -> Result<(), WatchError> {
            if !polled.iter().any(|fd| fd.revents != 0) {
                return Ok(());
            }
            let empty = Kevent {
                ident: 0,
                filter: Filter(0),
                flags: KeventFlags::from_bits(0),
                fflags: NoteFlags::from_bits(0),
                data: 0,
                udata: 0,
            };
            let mut events = [empty; EVENTS];
            let gone = NoteFlags::DELETE.bits() | NoteFlags::RENAME.bits();
            // Bounded by what the queue holds: each round takes up to EVENTS of them, and only
            // a full round asks again.
            loop {
                let got = kevent(self.kq.fd(), &[], &mut events, Some(Timespec::ZERO))
                    .map_err(|errno| refused("kevent(the viewer's kqueue)", errno))?;
                for event in events.iter().take(got) {
                    // By value: the struct is packed.
                    let (filter, fflags, ident) = (event.filter, event.fflags, event.ident);
                    if filter == Filter::VNODE {
                        seen.push(if fflags.bits() & gone != 0 { Seen::DirGone } else { Seen::DirChanged });
                    } else if filter == Filter::PROC
                        && let Ok(pid) = u32::try_from(ident)
                    {
                        seen.push(Seen::Exited(pid));
                    }
                }
                if got < EVENTS {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use blaze_own::AVec;
    use blaze_sys::{
        Errno, Path,
        linux::{InotifyEvents, InotifyFlags, InotifyMask, OwnedFd, inotify_add_watch, inotify_init1, pidfd_open, read},
    };

    use super::{Exit, Seen, WatchError, pollin, refused};

    /// The most processes watched at once: the viewer tracks at most 64 runs.
    const MOST_EXITS: usize = 64;

    /// The inotify descriptor on the runs directory, and a pidfd per watched process.
    pub struct Watch {
        inotify: OwnedFd,
        exits: [Option<(u32, OwnedFd)>; MOST_EXITS],
    }

    impl Watch {
        /// A watch on the directory `dir`.
        pub fn new(dir: &core::ffi::CStr) -> Result<Watch, WatchError> {
            let inotify = inotify_init1(InotifyFlags::NONBLOCK | InotifyFlags::CLOSE_ON_EXEC)
                .map_err(|errno| refused("inotify_init1()", errno))?;
            let mask = InotifyMask::CREATE
                | InotifyMask::DELETE
                | InotifyMask::MOVED_TO
                | InotifyMask::MOVED_FROM
                | InotifyMask::CLOSE_WRITE
                | InotifyMask::DELETE_SELF
                | InotifyMask::MOVE_SELF
                | InotifyMask::ONLY_DIR;
            inotify_add_watch(inotify.fd(), Path::new(dir), mask)
                .map_err(|errno| refused("inotify_add_watch(the runs directory)", errno))?;
            Ok(Watch { inotify, exits: [const { None }; MOST_EXITS] })
        }

        /// The descriptors to poll for reads: the inotify descriptor, then each pidfd.
        pub fn poll_fds(&self, into: &mut AVec<'_, libc::pollfd>) {
            into.push(pollin(self.inotify.fd().raw()));
            for (_, pidfd) in self.exits.iter().flatten() {
                into.push(pollin(pidfd.fd().raw()));
            }
        }

        /// Process `pid`'s exit watched: a pidfd, readable once it has exited. [`Errno::MFile`]
        /// past [`MOST_EXITS`] processes at once.
        pub fn watch_exit(&mut self, pid: u32) -> Result<Exit, Errno> {
            let raw = i32::try_from(pid).map_err(|_| Errno::Inval)?;
            let Some(slot) = self.exits.iter_mut().find(|slot| slot.is_none()) else {
                return Err(Errno::MFile);
            };
            match pidfd_open(raw) {
                Ok(pidfd) => {
                    *slot = Some((pid, pidfd));
                    Ok(Exit::Watched)
                }
                Err(Errno::Srch) => Ok(Exit::Gone),
                Err(errno) => Err(errno),
            }
        }

        /// What is ready, read without waiting, appended to `seen`; `polled` is what
        /// [`Watch::poll_fds`] pushed, after the poll.
        pub fn take(&mut self, polled: &[libc::pollfd], seen: &mut AVec<'_, Seen>) -> Result<(), WatchError> {
            let Some((inotify, pidfds)) = polled.split_first() else {
                return Ok(());
            };
            if inotify.revents != 0 {
                let gone = InotifyMask::DELETE_SELF.bits()
                    | InotifyMask::MOVE_SELF.bits()
                    | InotifyMask::IGNORED.bits()
                    | InotifyMask::UNMOUNT.bits();
                let mut bytes = [0u8; 4096];
                // Bounded by what the descriptor holds: it is nonblocking, and says EAGAIN once
                // read empty.
                loop {
                    let got = match read(self.inotify.fd(), &mut bytes) {
                        Ok(0) | Err(Errno::Again) => break,
                        Ok(got) => got,
                        Err(errno) => return Err(refused("read(the runs directory's inotify)", errno)),
                    };
                    for event in InotifyEvents::new(bytes.get(..got).unwrap_or(&[])) {
                        // An overflow lost events: one more listing reads what they said.
                        seen.push(if event.mask.bits() & gone != 0 { Seen::DirGone } else { Seen::DirChanged });
                    }
                }
            }
            // A pidfd polls readable once its process has exited: reported, and closed. The
            // pidfds were pushed in the slots' order.
            let mut ready = pidfds.iter();
            for slot in self.exits.iter_mut().filter(|slot| slot.is_some()) {
                if ready.next().is_some_and(|fd| fd.revents != 0)
                    && let Some((pid, _closed)) = slot.take()
                {
                    seen.push(Seen::Exited(pid));
                }
            }
            Ok(())
        }
    }
}
