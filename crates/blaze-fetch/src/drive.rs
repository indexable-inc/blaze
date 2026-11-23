//! A driver's loop of the [`Machine`], once, over the IO its caller gives ([`FetchIo`]): blaze's
//! `Fetch` tasks (one machine per command, each task wanting its unit) and the example both run
//! [`step`] and differ only in their IO (rule 30). The IO owns the sockets, the store and the
//! clock; this decides nothing the machine does not, it only carries effects out and answers
//! in.

#![blaze::sans_io]

use core::net::SocketAddr;

use blaze_ids::ContentId;
use blaze_own::{AVec, Vec};

use crate::fetch::{Adopt, Ask, Effect, LocalArchive, Machine, Method, Reply, StoreFailure};
use crate::peer::HasPacket;
use crate::plan::Unit;

/// What became of a reservation ([`FetchIo::reserve`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reserved {
    /// Finished at the length the machine gave: the record is kept, whole.
    Kept,
    /// Abandoned, nothing kept: the machine failed the unit (its record did not unpack, or the
    /// store made no reservation and `write` was told why).
    Abandoned,
    /// Written, then the store did not keep it (the finish failed).
    Unkept(StoreFailure),
}

/// What woke a [`FetchIo::wait`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Woke<'b> {
    /// An answer to `ask`, its body the IO's until its next wait. No time with it: the machine
    /// tries once (D172), so nothing it does waits on when an answer came.
    Answer { ask: Ask, reply: Reply<'b> },
    /// A unicast UDP `HAS` reply from a LAN peer (`peer` is the packet source address).
    PeerReply { peer: SocketAddr, has: HasPacket },
    /// The time given passed with no answer.
    Due,
    /// Something of the caller's (a unit wanted): no answer.
    Other,
}

/// The IO a fetch is driven over.
pub trait FetchIo {
    /// The clock the machine's deadlines are on, in nanoseconds.
    fn now(&self) -> u64;
    /// Whether the store holds a record at `key`.
    fn lookup(&mut self, key: &ContentId) -> bool;
    /// Keeps `record` at `key`, whole or not at all.
    fn put(&mut self, key: &ContentId, record: &[u8]) -> Result<(), StoreFailure>;
    /// Sends one HTTPS request; its answer comes from [`FetchIo::wait`] under `ask`. A body
    /// past `most` bytes is not read ([`crate::NetFailure::TooBig`]); an answer that has not come
    /// by `deadline_ns` may be dropped (the machine fails it at that time on its own).
    #[allow(clippy::too_many_arguments)]
    fn send(&mut self, ask: Ask, method: Method, url: &str, headers: &[(&str, &str)], body: &[u8], most: u64, deadline_ns: u64);
    /// The next answer, or [`Woke::Due`] once `until_ns` has passed (`None`: no time is due,
    /// so it waits for an answer or the caller alone, and should hold a bound of its own).
    fn wait(&mut self, until_ns: Option<u64>) -> Woke<'_>;
    /// Keeps the body of the answer [`FetchIo::wait`] last gave, for `unit`'s reservation: it
    /// is handed to that reservation's `write`, then let go.
    fn keep(&mut self, unit: u32);
    /// A reservation of `room` bytes in place at `key` for `unit`. `write` is given the body
    /// kept for `unit` and the reservation's bytes (or why the store made none, after which the
    /// reservation is not finished) and gives the length to finish it at, `None` to abandon it.
    fn reserve(
        &mut self,
        unit: u32,
        key: &ContentId,
        room: u64,
        write: &mut dyn FnMut(&[u8], Result<&mut [u8], StoreFailure>) -> Option<u64>,
    ) -> Reserved;
    /// The machine's local step ([`Effect::Local`], fetch.md's place 2): reads the `.crate` of
    /// `name` `version` that another tool keeps on this machine (cargo's
    /// `<cargo home>/registry/cache/<crates.io folder>/<name>-<version>.crate`, its folders
    /// chosen by [`crate::local::crates_io_folder`] and the file by
    /// [`crate::local::archive_name`]) whole into `out`, and says what it found. Nothing found
    /// is trusted here: the machine adopts the bytes only by the lock's SHA-256. The default
    /// holds nothing, so an IO without the step goes straight to HTTPS.
    fn local_archive(&mut self, _name: &str, _version: &str, _out: &mut AVec<'_, u8>) -> LocalArchive {
        LocalArchive::Absent
    }
    /// Fire-and-forget LAN UDP `HAVE` probe for up to 32 crate SHA-256 digests. Drivers without
    /// LAN peer discovery do nothing (zero overhead).
    fn peer_probe(&mut self, _nonce: u64, _hashes: &[[u8; 32]]) {}
    /// Plain-HTTP `GET` to a LAN peer (`https_only: false`, `redirects: 0`). Drivers without
    /// LAN peer support do nothing.
    fn send_peer(&mut self, _ask: Ask, _url: &str, _most: u64, _deadline_ns: u64) {}
    /// Cancels an in-flight HTTP request (`ask`) once its racing counterpart won or its peer
    /// was banned/timed out.
    fn cancel(&mut self, _ask: Ask) {}
}

/// Carries out every effect `machine` has now, then, unless it is done, waits once: for an
/// answer, for its next deadline, or for at most `wait_most_ns` when none is due. The caller
/// calls it until [`Machine::is_done`] (and again after each [`Machine::want`]).
pub fn step<I: FetchIo + ?Sized>(machine: &mut Machine<'_, '_, '_>, io: &mut I, wait_most_ns: u64) {
    while let Some(effect) = machine.poll(io.now()) {
        match effect {
            Effect::Lookup { unit, key } => {
                let held = io.lookup(&key);
                machine.looked(unit, held);
            }
            Effect::Put { unit, key, record } => {
                let kept = io.put(&key, record.as_slice());
                // The heap's record goes before the machine moves on (its bytes are counted
                // against the budget until `stored`).
                drop(record);
                machine.stored(unit, kept);
            }
            Effect::Reserve { unit, key, room } => {
                let reserved = io.reserve(unit, &key, room, &mut |body, room| machine.reserved(unit, body, room));
                match reserved {
                    Reserved::Kept => machine.stored(unit, Ok(())),
                    Reserved::Abandoned => {}
                    Reserved::Unkept(failure) => machine.stored(unit, Err(failure)),
                }
            }
            Effect::Http { ask, method, url, headers, body, most, deadline_ns, https_only, .. } => {
                if https_only {
                    io.send(ask, method, url.as_str(), headers, body.as_slice(), most, deadline_ns);
                } else {
                    io.send_peer(ask, url.as_str(), most, deadline_ns);
                }
            }
            Effect::PeerProbe { nonce, hashes } => {
                io.peer_probe(nonce, hashes.as_slice());
            }
            Effect::Cancel { ask } => {
                io.cancel(ask);
            }
            Effect::Local { unit } => {
                let mut archive = Vec::new_in(machine.alloc());
                let found = match machine.unit(unit) {
                    Unit::Crate { name, version, .. } => io.local_archive(name, version.as_str(), &mut archive),
                    // Never asked: the machine asks only for a crate's archive (`looked`);
                    // answered so the unit goes on by the order rather than waiting.
                    Unit::Git { .. } => LocalArchive::Absent,
                };
                // A large record is reserved now, while the archive's bytes are still here: the
                // IO kept no answer for this unit, so `write` is given these, not its own.
                if let Adopt::Reserve { key, room } = machine.local_found(unit, found, archive.as_slice()) {
                    let reserved =
                        io.reserve(unit, &key, room, &mut |_kept, room| machine.adopt_into(unit, archive.as_slice(), room));
                    match reserved {
                        Reserved::Kept => machine.stored(unit, Ok(())),
                        Reserved::Abandoned => {}
                        Reserved::Unkept(failure) => machine.stored(unit, Err(failure)),
                    }
                }
                drop(archive);
            }
        }
    }
    if machine.is_done() {
        return;
    }
    let until = Some(machine.next_deadline().unwrap_or_else(|| io.now().saturating_add(wait_most_ns)));
    match io.wait(until) {
        Woke::Answer { ask, reply } => {
            machine.answered(ask, reply);
            if machine.reserving(ask.unit) {
                io.keep(ask.unit);
            }
        }
        Woke::PeerReply { peer, has } => {
            machine.peer_replied(peer, has, io.now());
        }
        Woke::Due | Woke::Other => {}
    }
}
