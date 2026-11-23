//! Waits on descriptors and the kernel's event queues: `poll` on both systems; `kqueue` and
//! `kevent` on macOS; `eventfd`, inotify and the futex on Linux, which are
//! [`crate::linux`]'s own.

#[cfg(target_os = "macos")]
use core::ffi::c_int;

use crate::{Errno, Timespec};

// ---- poll ----

/// replaces libc::pollfd. Linux: [`crate::linux::PollFd`].
#[cfg(target_os = "linux")]
pub use crate::linux::PollFd;
/// replaces libc::POLLIN, libc::POLLPRI, libc::POLLOUT, libc::POLLERR, libc::POLLHUP,
/// libc::POLLNVAL. Linux: [`crate::linux::PollEvents`].
#[cfg(target_os = "linux")]
pub use crate::linux::PollEvents;

#[cfg(target_os = "macos")]
flag_set! {
    /// What a poll waits for and what it found (`POLL*`, poll.h: the same numbers as Linux's).
    PollEvents: i16 {
        /// `POLLIN`. replaces libc::POLLIN.
        IN = 0x01;
        /// `POLLPRI`. replaces libc::POLLPRI.
        PRI = 0x02;
        /// `POLLOUT`. replaces libc::POLLOUT.
        OUT = 0x04;
        /// `POLLERR`. replaces libc::POLLERR.
        ERR = 0x08;
        /// `POLLHUP`. replaces libc::POLLHUP.
        HUP = 0x10;
        /// `POLLNVAL`. replaces libc::POLLNVAL.
        NVAL = 0x20;
    }
}

/// `struct pollfd` (poll.h), with [`crate::linux::PollFd`]'s methods. replaces libc::pollfd.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

// poll.h, macOS arm64: `int fd; short events; short revents;`.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<PollFd>() == 8);
    assert!(align_of::<PollFd>() == 4);
    assert!(core::mem::offset_of!(PollFd, fd) == 0);
    assert!(core::mem::offset_of!(PollFd, events) == 4);
    assert!(core::mem::offset_of!(PollFd, revents) == 6);
};

#[cfg(target_os = "macos")]
impl PollFd {
    /// A poll of `fd` for `events`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(fd: crate::Fd, events: PollEvents) -> PollFd {
        PollFd { fd: fd.raw(), events: events.bits(), revents: 0 }
    }

    /// What the last poll found.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn found(&self) -> PollEvents {
        PollEvents::from_bits(self.revents)
    }
}

/// libSystem's `poll` (poll.h), not in the cold table.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) mod sys_event {
    use core::ffi::c_int;

    use crate::posix::types::nfds_t;

    unsafe extern "C" {
        /// poll.h: `int poll(struct pollfd fds[], nfds_t nfds, int timeout)`.
        pub(in crate::posix::io) fn poll(fds: *mut super::PollFd, nfds: nfds_t, timeout: c_int) -> c_int;
    }
}

/// `timeout` as `poll`'s milliseconds: rounded up, so the wait is never shorter than asked;
/// -1 (no limit) for `None`; 0 for a negative time; `c_int::MAX` (24.8 days) at most.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub(in crate::posix::io) fn poll_millis(timeout: Option<Timespec>) -> c_int {
    let Some(left) = timeout else {
        return -1;
    };
    if left.sec < 0 || left.nsec < 0 {
        return 0;
    }
    let whole = (left.sec as u64).saturating_mul(1_000);
    let part = (left.nsec as u64).saturating_add(999_999) / 1_000_000;
    let millis = whole.saturating_add(part);
    if millis > c_int::MAX as u64 { c_int::MAX } else { millis as c_int }
}

/// `poll` of `fds` for up to `timeout` (a length of time; no limit for `None`): how many
/// descriptors are ready, 0 when the time ran out; [`Errno::Intr`] when a signal ended it.
/// Linux: one [`crate::linux::ppoll`] with the thread's own mask (nanosecond-grained); macOS:
/// libSystem's `poll`, its time rounded up to milliseconds ([`poll_millis`]). replaces
/// libc::poll.
/// replaces libc::poll (blaze_sys::posix::io::poll(&mut [PollFd], Option<Timespec>), a length of time not milliseconds; both systems)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn poll(fds: &mut [PollFd], timeout: Option<Timespec>) -> Result<usize, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::ppoll(fds, timeout, None)
    }
    #[cfg(target_os = "macos")]
    {
        let Ok(count) = crate::posix::types::nfds_t::try_from(fds.len()) else {
            return Err(Errno::Inval);
        };
        // SAFETY: `fds` is `fds.len()` live `struct pollfd`s (the layout asserted above) the
        // call writes `revents` of; it keeps no pointer after it returns.
        let ready = unsafe { sys_event::poll(fds.as_mut_ptr(), count, poll_millis(timeout)) };
        super::said(ready).map(|ready| ready as usize)
    }
}

// ---- kqueue (macOS) ----

#[cfg(target_os = "macos")]
pub use kq::*;

/// `kqueue` and `kevent` (sys/event.h): macOS's event queue.
#[cfg(target_os = "macos")]
mod kq {
    use core::ffi::{c_int, c_void};

    use crate::{
        Errno, Fd, Timespec,
        posix::io::{OwnedFd, c_count, said},
    };

    /// A kevent's filter (`EVFILT_*`, sys/event.h): which kind of event it watches. A
    /// private filter (`EVFILT_MEMORYSTATUS`, -14) is made from its number.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    #[repr(transparent)]
    pub struct Filter(pub i16);

    impl Filter {
        /// `EVFILT_READ`: a descriptor has bytes to read, or a listener a connection.
        /// replaces libc::EVFILT_READ.
        pub const READ: Filter = Filter(-1);
        /// `EVFILT_WRITE`: a descriptor has room to write. replaces libc::EVFILT_WRITE.
        pub const WRITE: Filter = Filter(-2);
        /// `EVFILT_VNODE`: a file's changes.
        /// replaces libc::EVFILT_VNODE (blaze_sys::posix::io::Filter::VNODE; macOS only)
        pub const VNODE: Filter = Filter(-4);
        /// `EVFILT_PROC`: a process's events (its exit). replaces libc::EVFILT_PROC.
        pub const PROC: Filter = Filter(-5);
        /// `EVFILT_TIMER`.
        /// replaces libc::EVFILT_TIMER (blaze_sys::posix::io::Filter::TIMER; macOS only)
        pub const TIMER: Filter = Filter(-7);
        /// `EVFILT_USER`: an event this process triggers itself ([`NoteFlags::TRIGGER`]).
        /// replaces libc::EVFILT_USER.
        pub const USER: Filter = Filter(-10);
    }

    flag_set! {
        /// A kevent's `flags` (`EV_*`, sys/event.h): what a change does, and what an event
        /// says.
        KeventFlags: u16 {
            /// `EV_ADD`. replaces libc::EV_ADD.
            ADD = 0x0001;
            /// `EV_DELETE`. replaces libc::EV_DELETE.
            DELETE = 0x0002;
            /// `EV_ENABLE`.
            /// replaces libc::EV_ENABLE (blaze_sys::posix::io::KeventFlags::ENABLE; macOS only)
            ENABLE = 0x0004;
            /// `EV_DISABLE`.
            /// replaces libc::EV_DISABLE (blaze_sys::posix::io::KeventFlags::DISABLE; macOS only)
            DISABLE = 0x0008;
            /// `EV_ONESHOT`: removed after its first event. replaces libc::EV_ONESHOT.
            ONESHOT = 0x0010;
            /// `EV_CLEAR`: its state is reset once it is reported. replaces libc::EV_CLEAR.
            CLEAR = 0x0020;
            /// `EV_RECEIPT`: a change answers with an event of its own, `EV_ERROR` set.
            /// replaces libc::EV_RECEIPT (blaze_sys::posix::io::KeventFlags::RECEIPT; macOS only)
            RECEIPT = 0x0040;
            /// `EV_DISPATCH`.
            /// replaces libc::EV_DISPATCH (blaze_sys::posix::io::KeventFlags::DISPATCH; macOS only)
            DISPATCH = 0x0080;
            /// `EV_ERROR` (an event only): the change failed; `data` is its errno. replaces
            /// libc::EV_ERROR.
            /// replaces libc::EV_ERROR (blaze_sys::posix::io::KeventFlags::ERROR; macOS only)
            ERROR = 0x4000;
            /// `EV_EOF` (an event only).
            /// replaces libc::EV_EOF (blaze_sys::posix::io::KeventFlags::EOF; macOS only)
            EOF = 0x8000;
        }
    }

    flag_set! {
        /// A kevent's `fflags` (`NOTE_*`, sys/event.h): the filter's own flags.
        NoteFlags: u32 {
            /// `NOTE_DELETE` ([`Filter::VNODE`]): the file was unlinked. replaces
            /// libc::NOTE_DELETE.
            DELETE = 0x0000_0001;
            /// `NOTE_WRITE` ([`Filter::VNODE`]): the file was written; of a directory, an entry
            /// was added, removed or renamed in it. replaces libc::NOTE_WRITE.
            WRITE = 0x0000_0002;
            /// `NOTE_EXTEND` ([`Filter::VNODE`]): the file was extended in size. replaces
            /// libc::NOTE_EXTEND.
            EXTEND = 0x0000_0004;
            /// `NOTE_ATTRIB` ([`Filter::VNODE`]): the file's attributes (chmod, utimes, etc.)
            /// changed. replaces libc::NOTE_ATTRIB.
            ATTRIB = 0x0000_0008;
            /// `NOTE_LINK` ([`Filter::VNODE`]): the file's link count changed. replaces
            /// libc::NOTE_LINK.
            LINK = 0x0000_0010;
            /// `NOTE_RENAME` ([`Filter::VNODE`]): the file was renamed. replaces
            /// libc::NOTE_RENAME.
            RENAME = 0x0000_0020;
            /// `NOTE_REVOKE` ([`Filter::VNODE`]): access to the file was revoked (revoke or
            /// unmount). replaces libc::NOTE_REVOKE.
            REVOKE = 0x0000_0040;
            /// `NOTE_TRIGGER` ([`Filter::USER`]): fire the event. replaces libc::NOTE_TRIGGER.
            TRIGGER = 0x0100_0000;
            /// `NOTE_EXITSTATUS` ([`Filter::PROC`]): `data` holds the exit's wait status.
            /// replaces libc::NOTE_EXITSTATUS (blaze_sys::posix::io::NoteFlags::EXIT_STATUS; macOS only)
            EXIT_STATUS = 0x0400_0000;
            /// `NOTE_EXIT` ([`Filter::PROC`]): the process exited. replaces libc::NOTE_EXIT.
            EXIT = 0x8000_0000;
        }
    }

    /// `struct kevent` (sys/event.h), packed to 4 as the header packs it, with `udata` a
    /// number of the caller's own (the header's `void *`, as wide). replaces libc::kevent
    /// (the struct).
    #[derive(Clone, Copy, Debug)]
    #[repr(C, packed(4))]
    pub struct Kevent {
        /// What the filter watches: a descriptor, a process id, an id of the caller's.
        pub ident: usize,
        pub filter: Filter,
        pub flags: KeventFlags,
        pub fflags: NoteFlags,
        /// The filter's data: bytes ready, an exit status, an errno for `EV_ERROR`.
        pub data: isize,
        /// The caller's own, given back with each event.
        pub udata: usize,
    }

    // sys/event.h, macOS arm64 (`#pragma pack(4)`): 32 bytes, aligned 4.
    const _: () = {
        assert!(size_of::<Kevent>() == 32);
        assert!(align_of::<Kevent>() == 4);
        assert!(core::mem::offset_of!(Kevent, ident) == 0);
        assert!(core::mem::offset_of!(Kevent, filter) == 8);
        assert!(core::mem::offset_of!(Kevent, flags) == 10);
        assert!(core::mem::offset_of!(Kevent, fflags) == 12);
        assert!(core::mem::offset_of!(Kevent, data) == 16);
        assert!(core::mem::offset_of!(Kevent, udata) == 24);
    };

    /// `kqueue`: a new event queue (closed on exec by the system). replaces libc::kqueue.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn kqueue() -> Result<OwnedFd, Errno> {
        // SAFETY: `int kqueue(void)`: no arguments, no memory.
        let kq = said(unsafe { crate::macos::cold::kqueue() })?;
        // SAFETY: the call returned a new descriptor to this call alone.
        Ok(unsafe { OwnedFd::from_raw(kq) })
    }

    /// `kevent`: `changes` applied to `kq`, then up to `events.len()` events written into
    /// `events`, waiting up to `timeout` (a length of time; no limit for `None`, none for
    /// [`Timespec::ZERO`]): how many were written. [`Errno::Intr`] when a signal ended the
    /// wait. A change that fails is an event with [`KeventFlags::ERROR`] where `events` has
    /// room, else the call's error. replaces libc::kevent (the call).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn kevent(kq: Fd, changes: &[Kevent], events: &mut [Kevent], timeout: Option<Timespec>) -> Result<usize, Errno> {
        let (changed, room) = (c_count(changes.len())?, c_count(events.len())?);
        let timeout = timeout.as_ref().map_or(core::ptr::null(), |it| core::ptr::from_ref(it).cast::<c_void>());
        // SAFETY: `changes` is `changed` live `struct kevent`s the call reads and `events`
        // `room` it writes (the layout asserted above); the timespec, when given, is a live
        // `struct timespec` (two 64-bit integers, as `Timespec`); nothing is kept after the call.
        let written = unsafe {
            crate::macos::cold::kevent(
                kq.raw(),
                changes.as_ptr().cast::<c_void>(),
                changed,
                events.as_mut_ptr().cast::<c_void>(),
                room,
                timeout,
            )
        };
        said(written).map(|written: c_int| written as usize)
    }

    /// [`kevent`] into room not yet initialized: the events the call wrote, the first ones of
    /// `events`, as initialized `Kevent`s. The kqueue driver's receive into a reused buffer's
    /// spare slots, with no zeroing first.
    /// replaces libc::kevent (the call, into uninitialized event slots: blaze_sys::posix::io::kevent_uninit(kq, changes, &mut [MaybeUninit<Kevent>], timeout) -> Result<&mut [Kevent], Errno>)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn kevent_uninit<'a>(
        kq: Fd,
        changes: &[Kevent],
        events: &'a mut [core::mem::MaybeUninit<Kevent>],
        timeout: Option<Timespec>,
    ) -> Result<&'a mut [Kevent], Errno> {
        let (changed, room) = (c_count(changes.len())?, c_count(events.len())?);
        let timeout = timeout.as_ref().map_or(core::ptr::null(), |it| core::ptr::from_ref(it).cast::<c_void>());
        // SAFETY: as `kevent`'s; the call only writes `events`, never reads it.
        let written = unsafe {
            crate::macos::cold::kevent(
                kq.raw(),
                changes.as_ptr().cast::<c_void>(),
                changed,
                events.as_mut_ptr().cast::<c_void>(),
                room,
                timeout,
            )
        };
        let written: c_int = said(written)?;
        let written = (written as usize).min(events.len());
        // SAFETY: the call wrote `written` whole `struct kevent`s from the start of `events`
        // (at most its room), so those slots are initialized; the borrow is `events`'s.
        Ok(unsafe { core::slice::from_raw_parts_mut(events.as_mut_ptr().cast::<Kevent>(), written) })
    }

    #[cfg(test)]
    mod uninit_tests {
        use super::{Kevent, kevent_uninit, kqueue};
        use crate::Timespec;
        use core::mem::MaybeUninit;

        /// An empty kqueue looked at with no wait writes no event into uninitialized room.
        #[test]
        fn an_empty_queue_fills_nothing() {
            let kq = kqueue().unwrap();
            let mut room = [const { MaybeUninit::<Kevent>::uninit() }; 4];
            assert_eq!(kevent_uninit(kq.fd(), &[], &mut room, Some(Timespec::ZERO)).map(|it| it.len()), Ok(0));
        }
    }
}

// ---- Linux: eventfd, inotify, the futex ----

/// replaces libc::eventfd. Linux: [`crate::linux::eventfd2`].
#[cfg(target_os = "linux")]
pub use crate::linux::eventfd2;
/// replaces libc::EFD_CLOEXEC (`EventFlags::CLOSE_ON_EXEC`), libc::EFD_NONBLOCK
/// (`EventFlags::NONBLOCK`). Linux: [`crate::linux::EventFlags`].
/// replaces libc::EFD_NONBLOCK (blaze_sys::posix::io::EventFlags::NONBLOCK; Linux only)
/// replaces libc::EFD_SEMAPHORE (blaze_sys::posix::io::EventFlags::SEMAPHORE; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::EventFlags;
/// replaces libc::inotify_init1. Linux: [`crate::linux::inotify_init1`].
#[cfg(target_os = "linux")]
pub use crate::linux::inotify_init1;
/// replaces libc::inotify_add_watch. Linux: [`crate::linux::inotify_add_watch`].
#[cfg(target_os = "linux")]
pub use crate::linux::inotify_add_watch;
/// replaces libc::inotify_rm_watch. Linux: [`crate::linux::inotify_rm_watch`].
#[cfg(target_os = "linux")]
pub use crate::linux::inotify_rm_watch;
/// replaces libc::IN_CLOEXEC (`InotifyFlags::CLOSE_ON_EXEC`), libc::IN_NONBLOCK
/// (`InotifyFlags::NONBLOCK`). Linux: [`crate::linux::InotifyFlags`].
/// replaces libc::IN_NONBLOCK (blaze_sys::posix::io::InotifyFlags::NONBLOCK; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::InotifyFlags;
/// replaces libc::IN_CREATE, libc::IN_MOVED_TO, libc::IN_MOVED_FROM, libc::IN_DELETE,
/// libc::IN_DELETE_SELF, libc::IN_MOVE_SELF, libc::IN_MODIFY, libc::IN_CLOSE_WRITE,
/// libc::IN_ATTRIB, libc::IN_EXCL_UNLINK (`EXCLUDE_UNLINKED`), libc::IN_Q_OVERFLOW
/// (`OVERFLOW`), libc::IN_IGNORED, libc::IN_ISDIR (`IS_DIR`). Linux:
/// [`crate::linux::InotifyMask`].
/// replaces libc::IN_ACCESS (blaze_sys::posix::io::InotifyMask::ACCESS; Linux only)
/// replaces libc::IN_MODIFY (blaze_sys::posix::io::InotifyMask::MODIFY; Linux only)
/// replaces libc::IN_ATTRIB (blaze_sys::posix::io::InotifyMask::ATTRIB; Linux only)
/// replaces libc::IN_CLOSE_WRITE (blaze_sys::posix::io::InotifyMask::CLOSE_WRITE; Linux only)
/// replaces libc::IN_CLOSE_NOWRITE (blaze_sys::posix::io::InotifyMask::CLOSE_NO_WRITE; Linux only)
/// replaces libc::IN_OPEN (blaze_sys::posix::io::InotifyMask::OPEN; Linux only)
/// replaces libc::IN_DELETE_SELF (blaze_sys::posix::io::InotifyMask::DELETE_SELF; Linux only)
/// replaces libc::IN_MOVE_SELF (blaze_sys::posix::io::InotifyMask::MOVE_SELF; Linux only)
/// replaces libc::IN_UNMOUNT (blaze_sys::posix::io::InotifyMask::UNMOUNT; Linux only)
/// replaces libc::IN_Q_OVERFLOW (blaze_sys::posix::io::InotifyMask::OVERFLOW; Linux only)
/// replaces libc::IN_IGNORED (blaze_sys::posix::io::InotifyMask::IGNORED; Linux only)
/// replaces libc::IN_ONLYDIR (blaze_sys::posix::io::InotifyMask::ONLY_DIR; Linux only)
/// replaces libc::IN_DONT_FOLLOW (blaze_sys::posix::io::InotifyMask::NO_FOLLOW; Linux only)
/// replaces libc::IN_EXCL_UNLINK (blaze_sys::posix::io::InotifyMask::EXCLUDE_UNLINKED; Linux only)
/// replaces libc::IN_MASK_CREATE (blaze_sys::posix::io::InotifyMask::MASK_CREATE; Linux only)
/// replaces libc::IN_MASK_ADD (blaze_sys::posix::io::InotifyMask::MASK_ADD; Linux only)
/// replaces libc::IN_ISDIR (blaze_sys::posix::io::InotifyMask::IS_DIR; Linux only)
/// replaces libc::IN_ONESHOT (blaze_sys::posix::io::InotifyMask::ONE_SHOT; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::InotifyMask;
/// replaces libc::inotify_event (read by walking a read's bytes). Linux:
/// [`crate::linux::InotifyEvents`].
#[cfg(target_os = "linux")]
pub use crate::linux::{InotifyEvent, InotifyEvents, Watch};
/// replaces libc::FUTEX_WAIT (a wait of any bit, `bitset` [`FUTEX_ANY`], with an absolute
/// `CLOCK_MONOTONIC` time instead of a relative one),
/// libc::FUTEX_WAIT_BITSET with libc::FUTEX_PRIVATE_FLAG (the private wait; a
/// shared one, without the flag, with a relative time: [`crate::linux::futex_wait_shared`]). Linux:
/// [`crate::linux::futex_wait_bitset`].
/// replaces libc::FUTEX_WAIT_BITSET (with futex: blaze_sys::posix::io::futex_wait_bitset, always private; Linux only)
/// replaces libc::FUTEX_PRIVATE_FLAG (the futex calls futex_wait_bitset and futex_wake are the private ones; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::futex_wait_bitset;
/// replaces libc::FUTEX_WAKE with libc::FUTEX_PRIVATE_FLAG (the private wake; a shared one is
/// [`crate::linux::futex_wake_shared`]). Linux: [`crate::linux::futex_wake`].
#[cfg(target_os = "linux")]
pub use crate::linux::futex_wake;
/// `FUTEX_BITSET_MATCH_ANY`. Linux: [`crate::linux::FUTEX_ANY`].
#[cfg(target_os = "linux")]
pub use crate::linux::FUTEX_ANY;
