//! A placed output whose stamp the store took inside its racy window, vouched for once that
//! window has passed, between requests: what the next request's certificate check would
//! otherwise do on its own path (docs/v2/plan/design/certificate.md, "State", gap 2).
//!
//! The store looks at a file right after its rename onto the path (`blaze_cas::PlacedStamp`),
//! and the rename moves the inode's change time to that instant: every look it takes is
//! inside its window ([`blaze_vfs::racy`]), so the evidence keeps none of them and the first
//! check after a build stats, hashes and stats again every output the build placed. On the
//! gauntlet that is five linked programs of 140-160 MB, 66-83 ms each: 365 ms of the second
//! no-op after a cold build (2026-11-15, `request certificate: hash an output no stamp vouches
//! for`). Done here instead, the same three steps once the window has passed: the file looked
//! at whole (it must be the stamp the store saw, and steady by the look's time), read and
//! hashed (it must hold the bytes placed), looked at again (it must not have moved while it
//! was read). Then the stamp vouches for the placed bytes as a check's would, and the jobs'
//! record and evidence keep it with the steady look's time. Anything else drops the vouch, and
//! the next check hashes the output as before: never a condition, never a fallback.
//!
//! No IO, no clock: the driver asks the looks and the read and tells their outcomes. The read's
//! hash is no step of the loop's ([`Hashes`]): a worker's, told back as the read is.

use std::alloc::Allocator;

use blaze_core::Stamp;
use blaze_graph::pool::Class;
use blaze_ids::ContentId;
use crossbeam_channel::{Receiver, Sender};

use super::super::settle::Compress;

/// The hashes of vouches' reads out on the worker pool, each told back by its vouch's serial
/// over a channel the loop takes from as it turns ([`super::Jobs::vouch_hashed`]). Until
/// 2026-11-15 the loop hashed each read itself: after every build each placed program, 20 ms
/// a 20 MB one on the gauntlet, two to four of them, and a request that came in meanwhile
/// (the next build's client asking which blaze answers) waited behind them, 85 ms.
pub(super) struct Hashes {
    on: Compress,
    /// How many are out.
    out: usize,
    /// A hash made, or `None` for one a panic on its worker lost.
    send: Sender<(u32, Option<ContentId>)>,
    recv: Receiver<(u32, Option<ContentId>)>,
}

impl Hashes {
    /// Hashed where asked ([`Compress::Inline`]): the simulation's daemons, which run on one
    /// thread, until the daemon says the pool ([`Hashes::on`]).
    pub(super) fn new() -> Hashes {
        let (send, recv) = crossbeam_channel::unbounded();
        Hashes { on: Compress::Inline, out: 0, send, recv }
    }

    pub(super) fn on(&mut self, on: Compress) {
        self.on = on;
    }

    /// The bytes vouch `serial` read, hashed as the output's key is made with `wanted` the
    /// key placed ([`super::certify::content_of`]), under `span`, the vouch's; told back
    /// through [`Hashes::back`].
    pub(super) fn hash<A: Allocator + Send + 'static>(
        &mut self,
        serial: u32,
        bytes: Vec<u8, A>,
        wanted: ContentId,
        span: Option<tracing::Span>,
    ) {
        self.out += 1;
        // clone: a channel's sender, once a hash
        let send = self.send.clone();
        let work = move || {
            let _hash = span.map(|span| {
                tracing::info_span!(target: "blaze", parent: &span, "hash the placed output", bytes = bytes.len()).entered()
            });
            super::certify::content_of(&bytes, Some(wanted))
        };
        match self.on {
            Compress::Inline => _ = send.send((serial, Some(work()))),
            // A panic fails this one vouch: told as no hash, it is dropped and the next check
            // hashes the output as before.
            Compress::Pool => blaze_graph::exec::shared().submit_as(
                None,
                Class::Background,
                work,
                move |done| _ = send.send((serial, done.ok())),
            ),
        }
    }

    /// Whether a hash is out.
    pub(super) fn out(&self) -> bool {
        self.out != 0
    }

    /// A hash told back, if one was: its vouch's serial and the key, `None` where a panic
    /// lost it.
    pub(super) fn back(&mut self) -> Option<(u32, Option<ContentId>)> {
        let back = self.recv.try_recv().ok()?;
        self.out -= 1;
        Some(back)
    }
}

/// A bug planted here, for a test's control. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Plant {
    None,
    /// The first look is taken as the placement's own was, inside the window, and kept.
    RacyLook,
    /// What was read is kept with no second look.
    NoRelook,
    /// The stamp is kept on the first look alone: nothing is read.
    NoRead,
}

#[cfg(test)]
thread_local! {
    pub(super) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(plant: Plant) -> bool {
    let _ = plant;
    false
}

/// One placed output being vouched for.
#[derive(Clone, Debug)]
pub(super) struct Vouch {
    /// The output's path id in the jobs' record.
    pub(super) path: u32,
    /// The key of the bytes placed there.
    pub(super) content: ContentId,
    /// The store's look at the file after its rename.
    pub(super) placed: Stamp,
    state: State,
}

#[derive(Clone, Copy, Debug)]
enum State {
    /// Waits for this instant by the file system's clock: the first at which a look at the
    /// file as placed is steady.
    Due(u64),
    /// The first look is asked, the file system's clock read as it was.
    Looking { asked_ns: u64 },
    /// The file is read; `first` is the first look, steady as asked at `looked_ns`.
    Reading { first: Stamp, looked_ns: u64 },
    /// The second look is asked.
    Relooking { first: Stamp, looked_ns: u64 },
}

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Next {
    /// Look at the file: its whole stamp, `None` where it is gone or no regular file
    /// ([`Vouch::looked`]).
    Look,
    /// Read the file whole and hash it as the output's key is made ([`Vouch::read`]).
    Read,
    /// The file holds the placed bytes under `stamp`, steady as looked at at `looked_ns`.
    Vouched { stamp: Stamp, looked_ns: u64 },
    /// Nothing is vouched for, and why: the next check hashes the output.
    Dropped(&'static str),
}

impl Vouch {
    /// The vouch of the output at path id `path`, placed with the bytes of key `content` and
    /// looked at by the store as `placed` at `looked_ns`; `None` where that look is steady
    /// already and vouches on its own.
    pub(super) fn of(path: u32, content: ContentId, placed: Stamp, looked_ns: u64) -> Option<Vouch> {
        if !blaze_vfs::racy(&placed, looked_ns) {
            return None;
        }
        Some(Vouch { path, content, placed, state: State::Due(blaze_vfs::steady_at(&placed)) })
    }

    /// The instant by the file system's clock from which it may begin; `None` once begun.
    pub(super) fn due_ns(&self) -> Option<u64> {
        match self.state {
            State::Due(at) => Some(at),
            _ => None,
        }
    }

    /// Begins at `now_ns` by the file system's clock: the first look, if it is due.
    pub(super) fn begin(&mut self, now_ns: u64) -> Option<Next> {
        match self.state {
            State::Due(at) if now_ns >= at || planted(Plant::RacyLook) => {
                self.state = State::Looking { asked_ns: now_ns };
                Some(Next::Look)
            }
            _ => None,
        }
    }

    /// Back to waiting for its instant, nothing seen kept: what a vouch whose read came in as
    /// a request began does, so its hash does not hold that request up on the loop.
    pub(super) fn rewind(&mut self) {
        self.state = State::Due(blaze_vfs::steady_at(&self.placed));
    }

    /// A look asked by [`Next::Look`] saw `stamp`.
    pub(super) fn looked(&mut self, stamp: Option<Stamp>) -> Next {
        match (self.state, stamp) {
            (State::Looking { .. }, None) => Next::Dropped("the output is gone, or no regular file"),
            (State::Looking { .. }, Some(stamp)) if stamp != self.placed => {
                Next::Dropped("the output is not the file the store placed: it was written or replaced since")
            }
            (State::Looking { asked_ns }, Some(stamp))
                if blaze_vfs::racy(&stamp, asked_ns) && !planted(Plant::RacyLook) =>
            {
                Next::Dropped("the look was still inside the stamp's racy window")
            }
            (State::Looking { asked_ns }, Some(first)) if planted(Plant::NoRead) => {
                Next::Vouched { stamp: first, looked_ns: asked_ns }
            }
            (State::Looking { asked_ns }, Some(first)) => {
                self.state = State::Reading { first, looked_ns: asked_ns };
                Next::Read
            }
            (State::Relooking { first, looked_ns }, Some(second)) if second == first => {
                Next::Vouched { stamp: first, looked_ns }
            }
            (State::Relooking { .. }, _) => Next::Dropped("the output moved while it was read"),
            _ => Next::Dropped("a look came out of turn"),
        }
    }

    /// The read asked by [`Next::Read`] hashed to `content`; `None` where it failed.
    pub(super) fn read(&mut self, content: Option<ContentId>) -> Next {
        match self.state {
            State::Reading { first, looked_ns } if content == Some(self.content) => {
                if planted(Plant::NoRelook) {
                    return Next::Vouched { stamp: first, looked_ns };
                }
                self.state = State::Relooking { first, looked_ns };
                Next::Look
            }
            State::Reading { .. } if content.is_none() => Next::Dropped("the output could not be read"),
            State::Reading { .. } => Next::Dropped("the output holds other bytes than the store placed"),
            _ => Next::Dropped("a read came out of turn"),
        }
    }
}

#[cfg(test)]
mod tests;
