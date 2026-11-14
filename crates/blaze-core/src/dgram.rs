//! Batches of UDP datagrams: what an [`Effect::SendTo`](crate::Effect::SendTo) sends and an
//! [`Effect::RecvFrom`](crate::Effect::RecvFrom) receives into, and how a bound socket is set
//! up ([`Options`]).
//!
//! A [`Batch`] is one buffer of bytes and a head per run: a run is one datagram, or several of
//! one length to one peer laid end to end (Linux's segmentation offload, `UDP_SEGMENT`, on a
//! send; its receive offload, `UDP_GRO`, on a receive), the last of which may be shorter. One
//! buffer, not one per datagram: a batch is two allocations however many datagrams it holds,
//! and an asker that hands each completion's batch to its next effect allocates nothing once
//! both have grown.

use std::{
    alloc::Global,
    fmt,
    net::{IpAddr, SocketAddr},
};

use crate::{DATAGRAM_MOST, Dgram};

/// The most runs one batch carries, sent or received. Linux's `sendmmsg` and `recvmmsg` take
/// at most `UIO_MAXIOV` (1024) messages a call; 64 is what one QUIC connection's congestion
/// window sends at once on a fast path (64 datagrams of 1,200 bytes, 75 KiB), so a send of a
/// whole flight is one system call, and the stack's arrays of message headers stay small.
pub const BATCH_MOST: usize = 64;

/// The most datagrams one run carries: Linux's `UDP_MAX_SEGMENTS` (64 before Linux 6.9), the
/// most a `UDP_SEGMENT` send may cut one buffer into.
pub const SEGMENTS_MOST: usize = 64;

/// The most bytes one run carries: a coalesced receive's buffer (UDP's most less its header),
/// and the most a segmented send may hand the kernel at once.
pub const RUN_MOST: usize = 65_527;

/// A datagram's ECN codepoint (RFC 3168): the low two bits of IPv4's TOS byte or IPv6's
/// traffic class. QUIC reads them from what it receives and sets them on what it sends.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Ecn {
    /// Not ECN-capable.
    #[default]
    NotEct,
    /// ECN-capable, ECT(1).
    Ect1,
    /// ECN-capable, ECT(0).
    Ect0,
    /// Congestion experienced.
    Ce,
}

impl Ecn {
    /// The codepoint in the low two bits of `tos`.
    pub fn of_tos(tos: u8) -> Ecn {
        match tos & 0b11 {
            0b01 => Ecn::Ect1,
            0b10 => Ecn::Ect0,
            0b11 => Ecn::Ce,
            _ => Ecn::NotEct,
        }
    }

    /// The two bits, as the TOS byte or traffic class carries them.
    pub fn bits(self) -> u8 {
        match self {
            Ecn::NotEct => 0b00,
            Ecn::Ect1 => 0b01,
            Ecn::Ect0 => 0b10,
            Ecn::Ce => 0b11,
        }
    }
}

/// How a socket of [`Effect::Bind`](crate::Effect::Bind) is set up. Each option asked is set
/// or the bind fails naming it: none is skipped where the system lacks it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Options {
    /// Each datagram received says its ECN codepoint ([`Head::ecn`]), and each sent carries
    /// its head's: `IP_RECVTOS` and `IPV6_RECVTCLASS`, and a `IP_TOS` or `IPV6_TCLASS` message
    /// with each send. Without it every head received says [`Ecn::NotEct`], and a send of any
    /// other codepoint fails naming it.
    pub ecn: bool,
    /// Each datagram received says the address it arrived at ([`Head::local`]): `IP_PKTINFO`
    /// on Linux, `IP_RECVDSTADDR` on macOS, `IPV6_RECVPKTINFO` on both. For a socket bound at
    /// the unspecified address that answers from the address it was asked at.
    pub local_address: bool,
    /// Datagrams sent are never fragmented, so a path's MTU is found by what arrives (QUIC's
    /// DPLPMTUD, RFC 8899): `IP_MTU_DISCOVER` and `IPV6_MTU_DISCOVER` of `IP_PMTUDISC_PROBE` on
    /// Linux, `IP_DONTFRAG` and `IPV6_DONTFRAG` on macOS.
    pub dont_fragment: bool,
    /// The asker takes runs on a receive ([`Head::segment`] less than [`Head::len`]): on Linux
    /// `UDP_GRO`, which hands datagrams of one length from one peer up as one run. A system
    /// without it (macOS) receives each datagram as its own run, as it would with this off:
    /// the option permits runs, it does not ask for them. A receive on a socket with it must
    /// give each slot [`RUN_MOST`] bytes, as the kernel's runs fill one, or it is refused.
    pub coalesce: bool,
}

impl Options {
    /// What QUIC wants of its socket: every option on.
    pub const QUIC: Options =
        Options { ecn: true, local_address: true, dont_fragment: true, coalesce: true };
}

/// A run's head: who it is to or from, and where its bytes are in the batch.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Head {
    /// Whom it is sent to; on a receive, who sent it.
    pub peer: SocketAddr,
    /// On a send, the address to send from (`None`: the system's choice), which must be one of
    /// the host's; on a receive, the address it arrived at, where the socket was bound with
    /// [`Options::local_address`] (else `None`).
    pub local: Option<IpAddr>,
    pub ecn: Ecn,
    /// The run's bytes in [`Batch::bytes`]: the next `len` after the runs before it.
    pub len: u32,
    /// Each datagram's length within the run, the last's at most this: `len` for a run of one
    /// datagram.
    pub segment: u32,
    /// On a receive: the datagram was longer than its slot, and its end is lost. Never set on
    /// a send.
    pub truncated: bool,
}

impl Head {
    /// How many datagrams the run carries.
    pub fn datagrams(&self) -> usize {
        match self.segment {
            0 => 0,
            segment => (self.len as usize).div_ceil(segment as usize),
        }
    }
}

/// Datagrams: one buffer of bytes and a head per run, in order.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an effect's, as `Effect` is
pub struct Batch {
    pub bytes: Vec<u8>,
    pub heads: Vec<Head>,
}

impl Default for Batch {
    fn default() -> Batch {
        Batch::new()
    }
}

impl Batch {
    /// An empty batch, which allocates nothing until a datagram is pushed.
    pub fn new() -> Batch {
        Batch { bytes: Vec::new_in(Global), heads: Vec::new_in(Global) }
    }

    /// An empty batch with room for `runs` runs of `bytes` bytes in all.
    pub fn with_capacity(runs: usize, bytes: usize) -> Batch {
        Batch { bytes: Vec::with_capacity_in(bytes, Global), heads: Vec::with_capacity_in(runs, Global) }
    }

    /// `datagram` to `peer` as a run of its own, with `ecn`, from the system's choice of
    /// address.
    pub fn push(&mut self, peer: SocketAddr, ecn: Ecn, datagram: &[u8]) {
        let len = u32::try_from(datagram.len()).expect("a datagram is shorter than 4 GiB");
        self.bytes.extend_from_slice(datagram);
        self.heads.push(Head { peer, local: None, ecn, len, segment: len, truncated: false });
    }

    /// Empties it, keeping both buffers' capacity.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.heads.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    /// How many datagrams it carries, every run's counted.
    pub fn datagrams(&self) -> usize {
        self.heads.iter().map(Head::datagrams).sum()
    }

    /// Each run's head and bytes, in order.
    pub fn runs(&self) -> impl Iterator<Item = (&Head, &[u8])> + '_ {
        let mut at = 0;
        self.heads.iter().map(move |head| {
            let end = at + head.len as usize;
            let bytes = &self.bytes[at..end];
            at = end;
            (head, bytes)
        })
    }

    /// Each datagram's head and bytes, in order, a run's cut at its segment length.
    pub fn each(&self) -> impl Iterator<Item = (&Head, &[u8])> + '_ {
        self.runs().flat_map(|(head, bytes)| {
            bytes.chunks(head.segment.max(1) as usize).map(move |datagram| (head, datagram))
        })
    }

    /// Why a send of it is refused before the system sees it, if it is: no runs, more than
    /// [`BATCH_MOST`], heads whose lengths do not add up to its bytes, a run longer than
    /// [`RUN_MOST`] or cut into more than [`SEGMENTS_MOST`], or a datagram longer than
    /// [`DATAGRAM_MOST`].
    pub fn refused(&self) -> Option<Refused> {
        if self.heads.is_empty() {
            return Some(Refused::Empty);
        }
        if self.heads.len() > BATCH_MOST {
            return Some(Refused::Runs { runs: self.heads.len() });
        }
        let total: usize = self.heads.iter().map(|it| it.len as usize).sum();
        if total != self.bytes.len() {
            return Some(Refused::Lengths { heads: total, bytes: self.bytes.len() });
        }
        for head in &self.heads {
            let len = head.segment.min(head.len) as usize;
            if len == 0 || len > DATAGRAM_MOST {
                return Some(Refused::Datagram { peer: head.peer, len });
            }
            if head.len as usize > RUN_MOST || head.datagrams() > SEGMENTS_MOST {
                return Some(Refused::Run {
                    peer: head.peer,
                    len: head.len as usize,
                    segment: head.segment as usize,
                });
            }
        }
        None
    }
}

/// Why a batch's send is refused before the system sees it ([`Batch::refused`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    Empty,
    Runs { runs: usize },
    Lengths { heads: usize, bytes: usize },
    Datagram { peer: SocketAddr, len: usize },
    Run { peer: SocketAddr, len: usize, segment: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refused::Empty => f.write_str("a batch of no datagrams: a send carries at least one"),
            Refused::Runs { runs } => {
                write!(f, "a batch of {runs} runs: one carries at most BATCH_MOST, {BATCH_MOST}")
            }
            Refused::Lengths { heads, bytes } => {
                write!(f, "a batch whose heads say {heads} bytes and whose buffer holds {bytes}")
            }
            Refused::Datagram { peer, len } => write!(
                f,
                "a datagram of {len} bytes to {peer}: one is 1 to DATAGRAM_MOST ({DATAGRAM_MOST}) \
                 bytes"
            ),
            Refused::Run { peer, len, segment } => write!(
                f,
                "a run of {len} bytes in segments of {segment} to {peer}: a run is at most \
                 RUN_MOST ({RUN_MOST}) bytes and SEGMENTS_MOST ({SEGMENTS_MOST}) datagrams"
            ),
        }
    }
}

/// The slots one [`Effect::RecvFrom`](crate::Effect::RecvFrom) receives into: at most `most`
/// runs (1 to [`BATCH_MOST`]), each of at most `size` bytes (1 to [`RUN_MOST`]). The batch's
/// bytes are grown once to `most * size` and kept.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Slots {
    pub most: u16,
    pub size: u32,
}

impl Slots {
    /// One datagram of any length: what a receive that wants one at a time asks.
    pub const ONE: Slots = Slots { most: 1, size: DATAGRAM_MOST as u32 };

    /// Why a receive into these is refused, if it is.
    pub fn refused(&self, coalesce: bool) -> Option<String> {
        let (most, size) = (self.most as usize, self.size as usize);
        if most == 0 || most > BATCH_MOST {
            return Some(format!(
                "a receive of {most} slots: one has 1 to BATCH_MOST ({BATCH_MOST})"
            ));
        }
        if size == 0 || size > RUN_MOST {
            return Some(format!(
                "a receive into slots of {size} bytes: a slot is 1 to RUN_MOST ({RUN_MOST})"
            ));
        }
        if coalesce && size < RUN_MOST {
            return Some(format!(
                "a receive into slots of {size} bytes on a socket bound to coalesce: each slot \
                 is RUN_MOST ({RUN_MOST}) bytes, as a coalesced run fills one"
            ));
        }
        None
    }
}

/// What failed of a datagram socket's effect: which socket, at which address, with which
/// peer where it was about one, the system's error number where the system said it, and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct DgramError {
    pub sock: Dgram,
    /// The address it is bound at, where it is bound.
    pub local: Option<SocketAddr>,
    pub peer: Option<SocketAddr>,
    /// `errno`, where the system refused it.
    pub errno: Option<i32>,
    pub why: String,
}

impl fmt::Display for DgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.sock)?;
        if let Some(local) = self.local {
            write!(f, " at {local}")?;
        }
        if let Some(peer) = self.peer {
            write!(f, " with {peer}")?;
        }
        write!(f, ": {}", self.why)?;
        if let Some(errno) = self.errno {
            write!(f, " (errno {errno})")?;
        }
        Ok(())
    }
}

/// Why a name did not resolve ([`Effect::Resolve`](crate::Effect::Resolve)).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct Unresolved {
    pub name: String,
    pub port: u16,
    pub why: String,
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resolving {}:{}: {}", self.name, self.port, self.why)
    }
}

/// The longest a name of [`Effect::Resolve`](crate::Effect::Resolve) may be: DNS's most for a
/// whole name (RFC 1035 section 2.3.4), less its trailing dot.
pub const NAME_MOST: usize = 253;

/// How long a native driver lets `getaddrinfo` take before the resolve fails naming the
/// name: the system resolver's own default is five seconds a try and two tries a server
/// (`resolv.conf`'s `timeout` and `attempts`), so a resolve that has not ended in this has no
/// answer coming.
pub const RESOLVE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_s_datagrams_are_its_runs_cut_at_their_segment() {
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut batch = Batch::new();
        batch.push(peer, Ecn::Ect0, b"one");
        batch.bytes.extend_from_slice(b"aabbc");
        batch.heads.push(Head {
            peer,
            local: None,
            ecn: Ecn::NotEct,
            len: 5,
            segment: 2,
            truncated: false,
        });
        let each: Vec<&[u8]> = batch.each().map(|(_, it)| it).collect();
        assert_eq!(each, [&b"one"[..], b"aa", b"bb", b"c"]);
        assert_eq!(batch.datagrams(), 4);
        assert_eq!(batch.refused(), None);
        batch.bytes.push(0);
        assert_eq!(batch.refused(), Some(Refused::Lengths { heads: 8, bytes: 9 }));
    }

    #[test]
    fn ecn_is_the_low_two_bits() {
        for ecn in [Ecn::NotEct, Ecn::Ect1, Ecn::Ect0, Ecn::Ce] {
            assert_eq!(Ecn::of_tos(0b1010_1000 | ecn.bits()), ecn);
        }
    }
}
