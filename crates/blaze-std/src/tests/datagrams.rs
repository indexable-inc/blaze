//! A UDP socket ([`crate::net::Datagrams`]) keeps its `Net`'s reach and byte limit: a `Net`
//! narrowed to one bind address and one peer binds nowhere else and sends to no one else, and
//! one with a byte limit never passes it and spends it exactly, its sends and receives
//! counted together.
//!
//! The oracle looks past bstd's count: at what arrived at the two peer sockets, which belong
//! to an unlimited `Net` of the same simulated world. Each datagram the limited socket sends
//! starts with its send's index, so a peer can say which sends reached it, duplicates aside.
//! A send refused (over the limit, or to a peer not reached) must have moved nothing: no
//! datagram of its index arrives anywhere. The simulated network neither loses nor delays
//! (blaze-core's `sim::host::Datagrams { lose_one_in: 0, duplicate_one_in: 0, max_delay: 0 }`),
//! so every send that was let through arrives, and an end marker sent last by a third
//! socket says each peer has seen all.
//!
//! Plant: [`Plant::BudgetCheckedAfter`], a limit looked at once the bytes are gone: a send
//! over the limit then reaches its peer and is still said refused, which the oracle sees.
//!
//! Prints "datagrams: narrowing and the byte limit held over 150 plans" from the property.

use core::{net::SocketAddr, time::Duration};

use blaze_core::dgram::{Batch, Ecn, Options, Slots};
use blaze_sim::{Config, Tape};

use super::{Outcome, check, control, root, world};
use crate::{
    Cx, Plant,
    io::Io,
    net::{Addr, Datagrams, HostPort, Net, NetKind},
};

/// The limited socket's address, its one peer's, the peer it may not reach, and the third
/// socket that sends the end markers.
const MINE: &str = "127.0.0.1:7000";
const PEER: &str = "127.0.0.1:7001";
const OTHER: &str = "127.0.0.1:7002";
const MARKER: &str = "127.0.0.1:7003";

/// A receive's slots: four of 64 bytes, longer than any datagram the plan sends.
const SLOTS: Slots = Slots { most: 4, size: 64 };
/// The first byte of an end marker: no send has this index.
const END: u8 = 0xFF;
const MAX_STEPS: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Bits(u32);

impl Bits {
    const EMPTY: Bits = Bits(0);

    fn insert(&mut self, idx: u8) {
        if idx < 32 {
            self.0 |= 1u32 << idx;
        }
    }

    fn contains(&self, idx: u8) -> bool {
        idx < 32 && (self.0 & (1u32 << idx)) != 0
    }

    fn is_subset(&self, other: Bits) -> bool {
        (self.0 & other.0) == self.0
    }

    fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// The limited socket sends a batch of datagrams of these lengths (1 to 48) to its peer,
    /// or, where `to_other`, to the peer it may not reach.
    Send { lens: [u8; 3], count: u8, to_other: bool },
    /// The peer sends a datagram of this length to the limited socket.
    PeerSends(u8),
    /// The limited socket receives (only asked while something is there to come).
    Recv,
    /// The limited socket tries to bind at a second address its `Net` does not reach.
    BindElsewhere,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    limit: u64,
    steps: [Step; MAX_STEPS],
    len: usize,
}

fn plan(tape: &Tape) -> Plan {
    let seed = tape.next_u64("sim seed");
    let limit = tape.int_in(0..=400u64, "limit");
    let len = usize::try_from(tape.int_in(1..=MAX_STEPS as u64, "steps")).unwrap_or(1);
    let mut steps = [Step::Recv; MAX_STEPS];
    for slot in &mut steps[..len] {
        *slot = match tape.int_in(0..=5u64, "step") {
            0 | 1 => Step::Send {
                lens: [
                    tape.int_in(1..=48u64, "len a") as u8,
                    tape.int_in(1..=48u64, "len b") as u8,
                    tape.int_in(1..=48u64, "len c") as u8,
                ],
                count: tape.int_in(1..=3u64, "datagrams") as u8,
                to_other: tape.int_in(0..=4u64, "to other") == 0,
            },
            2 | 3 => Step::PeerSends(tape.int_in(1..=48u64, "peer len") as u8),
            4 => Step::Recv,
            _ => Step::BindElsewhere,
        };
    }
    Plan { seed, limit, steps, len }
}

fn host(raw: &str) -> HostPort {
    HostPort::parse(raw).expect("a test's address")
}

fn socket(raw: &str) -> SocketAddr {
    raw.parse().expect("a test's address")
}

/// Datagrams `ours` receives until an end marker came and every index of `want` was seen:
/// the indices seen, each once. A receive that stops (the deadline: something never came)
/// is the oracle's finding.
fn drain(cx: &Cx, ours: &Datagrams, want: Bits, who: &str) -> Outcome<Bits> {
    let (mut seen, mut ended) = (Bits::EMPTY, false);
    let mut into = Batch::new();
    while !(ended && want.is_subset(seen)) {
        let batch = ours.recv(cx, into, SLOTS).map_err(|error| {
            format!("{who} waited for sends {want:?} and saw {seen:?} (end marker {ended}): {error}")
        })?;
        for (_, datagram) in batch.each() {
            match datagram.first() {
                Some(&END) => ended = true,
                Some(&index) => seen.insert(index),
                None => return Err(format!("{who} received an empty datagram")),
            }
        }
        into = batch;
    }
    Ok(seen)
}

/// The steps on `mine` (the limited, narrowed socket) and `peer`; then the oracle against
/// what `peer` and `other` received.
fn steps(plan: &Plan, cx: &Cx, net: &Net, mine: &Datagrams, peer: &Datagrams, other: &Datagrams, marker: &Datagrams) -> Outcome {
    let (mut used, mut let_through, mut refused) = (0u64, Bits::EMPTY, Bits::EMPTY);
    // The lengths the peer sent the limited socket and it has not received yet, in order.
    let mut queued = [0u64; MAX_STEPS];
    let mut q_head = 0usize;
    let mut q_tail = 0usize;
    for (index, step) in plan.steps[..plan.len].iter().enumerate() {
        let index = index as u8;
        match *step {
            Step::Send { lens, count, to_other } => {
                let to = socket(if to_other { OTHER } else { PEER });
                let mut batch = Batch::new();
                let mut bytes = 0u64;
                for &len in &lens[..count as usize] {
                    let mut datagram = [0u8; 48];
                    datagram[0] = index;
                    batch.push(to, Ecn::Ect0, &datagram[..len as usize]);
                    bytes += u64::from(len);
                }
                let fits = used + bytes <= plan.limit;
                match (mine.send(cx, batch), to_other, fits) {
                    (Ok(_), false, true) => {
                        used += bytes;
                        let_through.insert(index);
                    }
                    (Err(error), true, _) if matches!(error.kind, NetKind::PeerNotAllowed { .. }) => {
                        refused.insert(index);
                    }
                    (Err(error), false, false) if matches!(error.kind, NetKind::OverLimit(_)) => {
                        refused.insert(index);
                    }
                    (outcome, to_other, fits) => {
                        return Err(format!(
                            "send {index} of {bytes} bytes (to the other peer: {to_other}; fits the limit \
                             {} with {used} used: {fits}): {outcome:?}",
                            plan.limit
                        ));
                    }
                }
            }
            Step::PeerSends(len) => {
                let mut batch = Batch::new();
                batch.push(socket(MINE), Ecn::NotEct, &[index; 48][..len as usize]);
                peer.send(cx, batch).map_err(|error| format!("harness: the peer's send: {error}"))?;
                queued[q_tail] = u64::from(len);
                q_tail += 1;
            }
            Step::Recv if q_head == q_tail => {}
            Step::Recv => {
                let left = plan.limit - used;
                let room = left / u64::from(SLOTS.size);
                match mine.recv(cx, Batch::new(), SLOTS) {
                    Ok(batch) => {
                        let mut came = [0u64; 4];
                        let mut came_len = 0usize;
                        for (head, _) in batch.runs() {
                            if came_len < came.len() {
                                came[came_len] = u64::from(head.len);
                            }
                            came_len += 1;
                        }
                        let q_len = q_tail - q_head;
                        let matches_queued = came_len <= came.len()
                            && came_len <= q_len
                            && came[..came_len] == queued[q_head..q_head + came_len];
                        if room == 0 || came_len == 0 || came_len as u64 > room.min(4) || !matches_queued {
                            return Err(format!(
                                "a receive with {left} of the limit left (room for {room} slots) got {:?}; \
                                 queued {:?}",
                                &came[..came_len.min(came.len())],
                                &queued[q_head..q_tail]
                            ));
                        }
                        for (head, _) in batch.runs() {
                            if head.peer != socket(PEER) {
                                return Err(format!("a datagram said from {}, sent from {PEER}", head.peer));
                            }
                        }
                        used += came[..came_len].iter().sum::<u64>();
                        q_head += came_len;
                    }
                    Err(error) if room == 0 && matches!(error.kind, NetKind::OverLimit(_)) => {}
                    Err(error) => {
                        return Err(format!("a receive with {left} left (room for {room} slots): {error}"));
                    }
                }
            }
            Step::BindElsewhere => match net.bind_datagrams(cx, &host(OTHER), &Options::default()) {
                Err(error) if error.kind == NetKind::NotAllowed => {}
                Ok(stray) => {
                    let _ = stray.close(cx);
                    return Err(format!("a Net bound only at {MINE} bound at {OTHER}"));
                }
                Err(error) => return Err(format!("a bind outside the reach: {error}")),
            },
        }
        if net.limit_left() != Some(plan.limit - used) {
            return Err(format!(
                "after step {index}: the limit says {:?} left, the model {}",
                net.limit_left(),
                plan.limit - used
            ));
        }
    }
    // The end markers, from a socket of the unlimited Net, sent last.
    let mut ends = Batch::new();
    ends.push(socket(PEER), Ecn::NotEct, &[END]);
    ends.push(socket(OTHER), Ecn::NotEct, &[END]);
    marker.send(cx, ends).map_err(|error| format!("harness: the end markers: {error}"))?;
    let at_peer = drain(cx, peer, let_through, "the peer")?;
    let at_other = drain(cx, other, Bits::EMPTY, "the other peer")?;
    if let Some(moved) = (0..u8::try_from(plan.len).unwrap_or(0))
        .find(|&idx| refused.contains(idx) && (at_peer.contains(idx) || at_other.contains(idx)))
    {
        return Err(format!("send {moved} was refused, and its datagrams arrived"));
    }
    if !at_other.is_empty() {
        return Err(format!("the peer the Net does not reach received sends {at_other:?}"));
    }
    if at_peer != let_through {
        return Err(format!("the peer received sends {at_peer:?}; let through were {let_through:?}"));
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant) -> Outcome {
    let io: Io = world(plan.seed, plant);
    io.with_sim(|sim| {
        sim.datagrams(blaze_core::sim::Datagrams {
            lose_one_in: 0,
            duplicate_one_in: 0,
            max_delay: Duration::ZERO,
        })
    })
    .ok_or("harness: the world is not simulated")?;
    let cx = root(&io);
    let open = io.need_net().map_err(|error| format!("harness: {error}"))?;
    let net = open.limit(plan.limit).bind_only(&host(MINE)).peers_only(&host(PEER));
    // QUIC's marks without coalescing: a coalescing socket's slots are each RUN_MOST bytes.
    let quic = Options { ecn: true, local_address: true, ..Options::default() };
    // Each socket is linear: all are closed whatever the steps found.
    let bind = |net: &Net, at: &str| net.bind_datagrams(&cx, &host(at), &quic).map_err(|error| format!("harness: bind {at}: {error}"));
    let (mine, peer, other, marker) = (bind(&net, MINE), bind(&open, PEER), bind(&open, OTHER), bind(&open, MARKER));
    let found = match (&mine, &peer, &other, &marker) {
        (Ok(mine), Ok(peer), Ok(other), Ok(marker)) => steps(plan, &cx, &net, mine, peer, other, marker),
        _ => Err("harness: a bind failed".to_owned()),
    };
    let mut closed = Ok(());
    for socket in [mine, peer, other, marker] {
        match socket {
            Ok(socket) => {
                if let Err(error) = socket.close(&cx) {
                    closed = Err(format!("harness: close: {error}"));
                }
            }
            Err(why) => closed = Err(why),
        }
    }
    found.and(closed)
}

/// The smallest round trip, to place a hang: two sockets of an unlimited `Net` bound at fixed
/// ports, one datagram sent and received, the
/// driver's clock printed before each step. A failure names the step that did not end.
/// Prints "datagrams: one datagram crossed the simulation (clock <a> -> <b>)".
#[test]
fn one_datagram_crosses_the_simulation() {
    let io: Io = world(7, Plant::None);
    let cx = root(&io);
    let net = io.need_net().expect("a net");
    // Fixed ports: a `HostPort` has no port 0 (blaze-url refuses it as BadPort).
    let options = Options::default();
    let a = net.bind_datagrams(&cx, &host("127.0.0.1:7100"), &options).expect("the first bind");
    let b = net.bind_datagrams(&cx, &host("127.0.0.1:7101"), &options).expect("the second bind");
    if (a.local(), b.local()) != (socket("127.0.0.1:7100"), socket("127.0.0.1:7101")) {
        panic!("bound at {} and {}, asked 7100 and 7101", a.local(), b.local());
    }
    let before = io.now();
    let mut batch = Batch::new();
    batch.push(b.local(), Ecn::NotEct, b"one");
    let sent = a.send(&cx, batch);
    let after_send = io.now();
    let got = sent.as_ref().ok().map(|_| b.recv(&cx, Batch::new(), Slots::ONE));
    let after_recv = io.now();
    let closed = (a.close(&cx), b.close(&cx));
    if let Err(error) = &sent {
        panic!("the send (clock {before:?} -> {after_send:?}): {error}");
    }
    match got {
        Some(Ok(batch)) => {
            let first = batch.each().next().map(|(_head, bytes)| bytes);
            assert_eq!(first, Some(&b"one"[..]), "what arrived");
        }
        Some(Err(error)) => panic!("the receive (clock {after_send:?} -> {after_recv:?}): {error}"),
        None => unreachable!("the send failed above"),
    }
    assert!(closed.0.is_ok() && closed.1.is_ok(), "closes: {closed:?}");
    println!("datagrams: one datagram crossed the simulation (clock {before:?} -> {after_recv:?})");
}

/// The invariant: a `Net` narrowed to one bind address and one peer, with a byte limit,
/// gives a UDP socket that binds and sends nowhere else and never passes the limit, its
/// sends and receives spending it exactly, counted at the peers outside bstd's count.
/// Prints "datagrams: narrowing and the byte limit held over 150 plans".
#[test]
fn a_udp_socket_keeps_its_nets_reach_and_limit() {
    blaze_sim::check(Config::cases(150), |tape| check(plan(tape), run));
    println!("datagrams: narrowing and the byte limit held over 150 plans");
}

/// The control: with the limit looked at once the bytes are gone, a send over it reaches
/// its peer while it is said refused, and the oracle says so.
#[test]
fn the_oracle_catches_a_datagram_limit_checked_after_the_send() {
    control(Plant::BudgetCheckedAfter, plan, run);
}
