//! Deterministic simulation tests for the parallel LAN UDP peer-to-peer `.crate` fetch race.
//!
//! Proves:
//! 1. A fast honest LAN peer wins the race, cancels the slow WAN request via `Effect::Cancel`,
//!    verifies SHA-256, and produces a `BLZFPK03` record byte-identical to WAN.
//! 2. Blocked/dead UDP adds 0 ns delay: WAN starts at `t = 0` and succeeds cleanly.
//! 3. A Byzantine peer sending corrupted/wrong `.crate` bytes or a 3xx redirect is permanently
//!    banned (`is_peer_banned`), its remaining in-flight asks are cancelled, and WAN still
//!    succeeds cleanly with a valid `BLZFPK03` record.
//! 4. Replies from SSRF/public (`8.8.8.8`) or cloud metadata (`169.254.169.254`) IPs are
//!    rejected before any HTTP request is emitted.
//! 5. Full UDP multicast `HAVE` -> `HAS` discovery over `blaze_core::sim::Sim` drives the fetch
//!    machine end-to-end deterministically.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::time::Duration;

use blaze_ids::ContentId;
use blaze_manifest::lock::LockId;
use blaze_own::{Alloc, SortedMap, String as OwnString, Vec as OwnVec};
use blaze_tar::{Make, Writer};
use blaze_text::Writes as _;

use crate::fetch::{Ask, Effect, Limits, LocalArchive, Machine, NetFailure, Outcome, Reply};
use crate::pack::PackView;
use crate::peer::{self, DEFAULT_UDP_PORT, HAVE_MAX_BYTES, HasPacket, MULTICAST_GROUP};
use crate::plan::Unit;
use crate::unpack;

type Bytes = OwnVec<u8, Alloc<'static>>;

fn copy_bytes(src: &[u8]) -> Bytes {
    let mut out = OwnVec::new_in(blaze_own::testing::heap().alloc());
    out.extend_from_slice(src);
    out
}

/// Builds a valid `.crate` gzip+tar archive containing `Cargo.toml` and `src/lib.rs`.
fn make_crate(name: &str, version: &str, lib_rs: &[u8]) -> Bytes {
    let alloc = blaze_own::testing::heap().alloc();
    let mut tar = OwnVec::new_in(alloc);
    let mut w = Writer::new();
    let mut dir = OwnString::new_in(alloc);
    dir.str(name).str("-").str(version);
    w.entry(dir.as_str(), Make::Dir, &[], &mut tar).expect("dir");

    let mut manifest_path = OwnString::new_in(alloc);
    manifest_path.str(dir.as_str()).str("/Cargo.toml");
    let mut manifest = OwnString::new_in(alloc);
    manifest.str("[package]\nname = \"").str(name).str("\"\nversion = \"").str(version).str("\"\n");
    w.entry(manifest_path.as_str(), Make::File { exec: false }, manifest.as_str().as_bytes(), &mut tar)
        .expect("manifest");

    let mut lib_path = OwnString::new_in(alloc);
    lib_path.str(dir.as_str()).str("/src/lib.rs");
    w.entry(lib_path.as_str(), Make::File { exec: false }, lib_rs, &mut tar).expect("lib");
    w.finish(&mut tar).expect("tar end");
    let mut gz = OwnVec::new_in(alloc);
    blaze_deflate::gzip(tar.as_slice(), blaze_deflate::Level::Fast, alloc, &mut gz).expect("gzip");
    gz
}

fn make_unit<'l>(id: u32, name: &'l str, version: &str, sha256: [u8; 32]) -> Unit<'l, 'static> {
    let mut ver = OwnString::new_in(blaze_own::testing::heap().alloc());
    ver.str(version);
    Unit::Crate { package: LockId(id), name, version: ver, sha256 }
}

#[derive(Clone, Copy)]
enum PeerMode {
    /// Honest peer returning exact `.crate` bytes after `latency_ns`.
    Honest { latency_ns: u64 },
    /// Byzantine peer returning corrupted bytes (wrong SHA-256) after `latency_ns`.
    Corrupted { latency_ns: u64 },
    /// Hostile peer trying to redirect to cloud metadata (`302 Found` / `NetFailure::Refused`).
    Redirect { latency_ns: u64 },
    /// Dead UDP: peer never replies to probes.
    DeadUdp,
}

struct SimEvent {
    at_ns: u64,
    kind: SimEventKind,
}

enum SimEventKind {
    UdpHas { peer: SocketAddr, has: HasPacket },
    HttpReply { ask: Ask, status: u16, body: Bytes, failed: Option<NetFailure> },
}

fn run_race<'p, 'l>(
    units: &'p [Unit<'l, 'static>],
    archives: &[&[u8]],
    peer_addr: SocketAddr,
    peer_mode: PeerMode,
    wan_latency_ns: u64,
) -> (
    Machine<'p, 'l, 'static>,
    SortedMap<ContentId, Bytes, Alloc<'static>>,
    OwnVec<Ask, Alloc<'static>>,
    u64,
) {
    let alloc = blaze_own::testing::heap().alloc();
    let mut limits = Limits::REAL;
    limits.reserve_from = u64::MAX; // keep in-heap Put simple for inspection
    let mut machine = Machine::new(units, limits, alloc);
    let mut store = SortedMap::<ContentId, Bytes, Alloc<'static>>::new_in(alloc);
    let mut events = OwnVec::<SimEvent, Alloc<'static>>::new_in(alloc);
    let mut cancelled = OwnVec::<Ask, Alloc<'static>>::new_in(alloc);
    let mut now = 0u64;

    for _ in 0..10_000 {
        while let Some(effect) = machine.poll(now) {
            match effect {
                Effect::Lookup { unit, key } => {
                    machine.looked(unit, store.contains_key(&key));
                }
                Effect::Local { unit } => {
                    let _ = machine.local_found(unit, LocalArchive::Absent, &[]);
                }
                Effect::Put { unit, key, record } => {
                    store.insert(key, copy_bytes(record.as_slice()));
                    machine.stored(unit, Ok(()));
                }
                Effect::Reserve { .. } => unreachable!("reserve_from is u64::MAX in this test"),
                Effect::Cancel { ask } => {
                    cancelled.push(ask);
                    events.retain(|ev| !matches!(ev.kind, SimEventKind::HttpReply { ask: a, .. } if a == ask));
                }
                Effect::PeerProbe { nonce, hashes } => {
                    if !matches!(peer_mode, PeerMode::DeadUdp) {
                        let mask = (1u32 << hashes.len()) - 1;
                        events.push(SimEvent {
                            at_ns: now + 1_000_000, // 1 ms LAN RTT
                            kind: SimEventKind::UdpHas {
                                peer: peer_addr,
                                has: HasPacket { tcp_port: peer_addr.port(), nonce, mask, active_transfers: 0 },
                            },
                        });
                    }
                }
                Effect::Http { ask, url, https_only, redirects, .. } => {
                    let idx = usize::try_from(ask.unit).expect("unit index");
                    if https_only {
                        assert!(url.as_str().starts_with(crate::fetch::CRATES));
                        events.push(SimEvent {
                            at_ns: now + wan_latency_ns,
                            kind: SimEventKind::HttpReply {
                                ask,
                                status: 200,
                                body: copy_bytes(archives[idx]),
                                failed: None,
                            },
                        });
                    } else {
                        assert_eq!(redirects, 0, "LAN peer requests must forbid redirects");
                        assert!(url.as_str().starts_with("http://"));
                        match peer_mode {
                            PeerMode::Honest { latency_ns } => {
                                events.push(SimEvent {
                                    at_ns: now + latency_ns,
                                    kind: SimEventKind::HttpReply {
                                        ask,
                                        status: 200,
                                        body: copy_bytes(archives[idx]),
                                        failed: None,
                                    },
                                });
                            }
                            PeerMode::Corrupted { latency_ns } => {
                                let mut bad = copy_bytes(archives[idx]);
                                bad[0] ^= 0xff;
                                events.push(SimEvent {
                                    at_ns: now + latency_ns,
                                    kind: SimEventKind::HttpReply { ask, status: 200, body: bad, failed: None },
                                });
                            }
                            PeerMode::Redirect { latency_ns } => {
                                events.push(SimEvent {
                                    at_ns: now + latency_ns,
                                    kind: SimEventKind::HttpReply {
                                        ask,
                                        status: 302,
                                        body: OwnVec::new_in(alloc),
                                        failed: None,
                                    },
                                });
                            }
                            PeerMode::DeadUdp => unreachable!("no peer HTTP without HAS reply"),
                        }
                    }
                }
            }
        }
        if machine.is_done() {
            break;
        }
        let next_ev = events.iter().map(|e| e.at_ns).min();
        let next = match (next_ev, machine.next_deadline()) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => panic!("fetch machine stalled without completing"),
        };
        now = now.max(next);
        let mut i = 0;
        while i < events.len() {
            if events[i].at_ns <= now {
                let ev = events.remove_shift(i);
                match ev.kind {
                    SimEventKind::UdpHas { peer, has } => machine.peer_replied(peer, has, now),
                    SimEventKind::HttpReply { ask, status, body, failed } => {
                        let reply = match failed {
                            Some(f) => Reply::Failed(f),
                            None => Reply::Body { status, bytes: body.as_slice() },
                        };
                        machine.answered(ask, reply);
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    assert!(machine.is_done());
    (machine, store, cancelled, now)
}

#[test]
fn fast_lan_peer_wins_cancels_wan_and_verifies_sha256() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = make_crate("alpha", "1.2.3", b"pub fn alpha() -> u32 { 42 }\n");
    let sha = blaze_sha256::digest(bytes.as_slice());
    let units = [make_unit(0, "alpha", "1.2.3", sha)];
    let peer = SocketAddr::from(([192, 168, 1, 20], 41730));

    let (machine, store, cancelled, finished_ns) =
        run_race(&units, &[bytes.as_slice()], peer, PeerMode::Honest { latency_ns: 2_000_000 }, 500_000_000);

    assert_eq!(machine.peer_wins(), 1, "LAN peer should win the race");
    assert_eq!(finished_ns, 3_000_000, "finished in 1ms UDP + 2ms LAN HTTP, not 500ms WAN");
    assert_eq!(cancelled.len(), 1, "slow WAN request must be cancelled once peer wins");
    assert!(matches!(machine.outcome(0), Outcome::Fetched { .. }));

    // Record stored must match WAN's BLZFPK03 byte-for-byte.
    let (wan_record, _) =
        unpack::crate_record("alpha", "1.2.3", sha, bytes.as_slice(), &Limits::REAL.unpack, alloc).expect("unpack");
    let stored = store.get(&units[0].key()).expect("stored record");
    assert_eq!(stored.as_slice(), wan_record.as_slice());
    assert!(PackView::open_source(&units[0].source(), stored.as_slice()).is_ok());
}

#[test]
fn dead_udp_adds_zero_delay_and_wan_succeeds() {
    let bytes = make_crate("beta", "0.1.0", b"pub fn beta() {}\n");
    let sha = blaze_sha256::digest(bytes.as_slice());
    let units = [make_unit(0, "beta", "0.1.0", sha)];
    let peer = SocketAddr::from(([192, 168, 1, 20], 41730));

    let wan_ms = 50_000_000;
    let (machine, store, cancelled, finished_ns) = run_race(&units, &[bytes.as_slice()], peer, PeerMode::DeadUdp, wan_ms);

    assert_eq!(machine.peer_wins(), 0);
    assert_eq!(finished_ns, wan_ms, "dead UDP must add 0 ns delay to WAN fetch");
    assert!(cancelled.is_empty());
    assert!(matches!(machine.outcome(0), Outcome::Fetched { .. }));
    assert!(store.contains_key(&units[0].key()));
}

#[test]
fn byzantine_peer_corrupted_bytes_and_redirect_are_banned_and_wan_succeeds() {
    let b0 = make_crate("gamma", "1.0.0", b"pub fn gamma() {}\n");
    let b1 = make_crate("delta", "2.0.0", b"pub fn delta() {}\n");
    let units = [
        make_unit(0, "gamma", "1.0.0", blaze_sha256::digest(b0.as_slice())),
        make_unit(1, "delta", "2.0.0", blaze_sha256::digest(b1.as_slice())),
    ];
    let peer = SocketAddr::from(([10, 0, 0, 99], 41730));

    // Corrupted `.crate` bytes -> peer permanently banned, WAN succeeds with valid BLZFPK03.
    let (machine, store, _cancelled, finished_ns) = run_race(
        &units,
        &[b0.as_slice(), b1.as_slice()],
        peer,
        PeerMode::Corrupted { latency_ns: 2_000_000 },
        80_000_000,
    );
    assert!(machine.is_peer_banned(peer.ip()), "corrupted peer must be permanently banned");
    assert_eq!(machine.peer_wins(), 0);
    assert_eq!(finished_ns, 80_000_000, "WAN succeeds on schedule despite Byzantine peer");
    for (i, u) in units.iter().enumerate() {
        let idx = u32::try_from(i).expect("unit index");
        assert!(matches!(machine.outcome(idx), Outcome::Fetched { .. }));
        let rec = store.get(&u.key()).expect("valid BLZFPK03 stored");
        assert!(PackView::open_source(&u.source(), rec.as_slice()).is_ok());
    }

    // Redirecting peer -> permanently banned, WAN still succeeds cleanly.
    let (machine_r, store_r, _, _) = run_race(
        &units,
        &[b0.as_slice(), b1.as_slice()],
        peer,
        PeerMode::Redirect { latency_ns: 2_000_000 },
        80_000_000,
    );
    assert!(machine_r.is_peer_banned(peer.ip()), "redirecting peer must be permanently banned");
    assert_eq!(machine_r.peer_wins(), 0);
    assert_eq!(store_r.len(), 2);
}

#[test]
fn ssrf_public_and_metadata_peer_replies_are_rejected() {
    let bytes = make_crate("epsilon", "0.5.0", b"pub fn eps() {}\n");
    let sha = blaze_sha256::digest(bytes.as_slice());
    let units = [make_unit(0, "epsilon", "0.5.0", sha)];

    for evil_ip in [
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    ] {
        let evil = SocketAddr::new(evil_ip, 80);
        let (machine, store, _, finished_ns) = run_race(
            &units,
            &[bytes.as_slice()],
            evil,
            PeerMode::Honest { latency_ns: 1_000_000 },
            40_000_000,
        );
        assert_eq!(machine.rejected_peers(), 1, "evil IP {evil_ip} must be rejected");
        assert_eq!(machine.peer_wins(), 0, "no HTTP request may be sent to {evil_ip}");
        assert_eq!(finished_ns, 40_000_000, "WAN completes unaffected");
        assert!(store.contains_key(&units[0].key()));
    }
}

/// End-to-end DST using `blaze_core::sim::Sim` UDP multicast fan-out (`239.255.77.77:41729`)
/// to prove `HAVE` -> `HAS` discovery works over the simulated UDP host.
#[test]
fn sim_udp_multicast_have_has_drives_fetch_machine() {
    use blaze_core::dgram::{Batch, Ecn, Options, Slots};
    use blaze_core::sim::{Faults, Sim};
    use blaze_core::sockopt::{Ask, MulticastV4, UdpOptions};
    use blaze_core::{Done, Driver as _, Effect as CoreEffect, Tag};

    let bytes = make_crate("zeta", "0.9.0", b"pub fn zeta() -> bool { true }\n");
    let sha = blaze_sha256::digest(bytes.as_slice());
    let units = [make_unit(0, "zeta", "0.9.0", sha)];

    let mut sim = Sim::new(77, Faults::default());
    let peer_udp_opts = Options {
        tuning: UdpOptions {
            reuse_address: Ask::Must(true),
            reuse_port: Ask::Must(true),
            multicast_join: Ask::Must(MulticastV4 {
                group: MULTICAST_GROUP,
                interface: Ipv4Addr::UNSPECIFIED,
            }),
            ..UdpOptions::DEFAULT
        },
        ..Options::default()
    };
    let client_udp_opts = Options {
        tuning: UdpOptions {
            broadcast: Ask::Must(true),
            multicast_ttl: Ask::Must(1),
            ..UdpOptions::DEFAULT
        },
        ..Options::default()
    };

    let mut effects = [
        CoreEffect::Bind {
            at: SocketAddr::from(([192, 168, 10, 5], DEFAULT_UDP_PORT)),
            options: peer_udp_opts,
            tag: Tag(1),
        },
        CoreEffect::Bind {
            at: SocketAddr::from(([192, 168, 10, 2], 0)),
            options: client_udp_opts,
            tag: Tag(2),
        },
    ]
    .into_iter()
    .collect();
    let alloc = blaze_own::testing::heap().alloc();
    let mut answers = OwnVec::new_in(alloc);
    blaze_core::run_until_idle(&mut sim, &mut effects, |done, _| answers.push(done)).expect("bind");
    answers.sort_by_key(|c| c.tag);
    let Done::Bound { sock: peer_sock, .. } = answers[0].done else { panic!("peer bind") };
    let Done::Bound { sock: client_sock, .. } = answers[1].done else { panic!("client bind") };

    let mut machine = Machine::new(&units, Limits::REAL, alloc);
    let mut probe_payload = None;
    while let Some(eff) = machine.poll(0) {
        match eff {
            Effect::Lookup { unit, .. } => machine.looked(unit, false),
            Effect::Local { unit } => {
                let _ = machine.local_found(unit, LocalArchive::Absent, &[]);
            }
            Effect::PeerProbe { nonce, hashes } => {
                let mut wire = [0u8; HAVE_MAX_BYTES];
                let len = peer::encode_have(nonce, hashes.as_slice(), &mut wire).expect("encode HAVE");
                probe_payload = Some(copy_bytes(&wire[..len]));
            }
            Effect::Http { .. } | Effect::Cancel { .. } | Effect::Put { .. } | Effect::Reserve { .. } => {}
        }
    }
    let probe = probe_payload.expect("machine emitted PeerProbe");

    // Client multicasts HAVE, peer receives it, matches SHA-256, and unicasts HAS back.
    let mut batch = Batch::new();
    batch.push(SocketAddr::from((MULTICAST_GROUP, DEFAULT_UDP_PORT)), Ecn::NotEct, probe.as_slice());
    let mut effects = [
        CoreEffect::RecvFrom { sock: peer_sock, into: Batch::new(), slots: Slots::ONE, tag: Tag(10) },
        CoreEffect::RecvFrom { sock: client_sock, into: Batch::new(), slots: Slots::ONE, tag: Tag(11) },
        CoreEffect::SendTo { sock: client_sock, batch, tag: Tag(12) },
    ]
    .into_iter()
    .collect();
    let mut client_got_has = None;
    let mut done_buf = Default::default();
    for _ in 0..100 {
        sim.submit(&mut effects);
        if client_got_has.is_some() || sim.in_flight() == 0 {
            break;
        }
        let until = blaze_core::wait::Deadline::after(sim.now(), Duration::from_secs(1));
        sim.poll(&mut done_buf, until);
        for c in done_buf.drain(..) {
            match (c.tag, c.done) {
                (Tag(10), Done::Datagrams(b)) => {
                    let (head, raw) = b.each().next().expect("HAVE datagram");
                    let have = peer::decode_have(raw).expect("valid HAVE");
                    let mut mask = 0u32;
                    for (idx, h) in have.iter() {
                        if h == sha {
                            mask |= 1u32 << idx;
                        }
                    }
                    assert_eq!(mask, 1);
                    let reply = peer::encode_has(HasPacket {
                        tcp_port: 41731,
                        nonce: have.nonce,
                        mask,
                        active_transfers: 0,
                    });
                    let mut out = Batch::new();
                    out.push(head.peer, Ecn::NotEct, &reply);
                    effects.push(CoreEffect::SendTo { sock: peer_sock, batch: out, tag: Tag(13) });
                }
                (Tag(11), Done::Datagrams(b)) => {
                    let (head, raw) = b.each().next().expect("HAS datagram");
                    let has = peer::decode_has(raw).expect("valid HAS");
                    client_got_has = Some((head.peer, has));
                }
                _ => {}
            }
        }
    }

    let (from_peer, has) = client_got_has.expect("client received unicast HAS from simulated peer");
    assert_eq!(from_peer.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 10, 5)));
    assert_eq!(has.tcp_port, 41731);

    // Feeding HAS into Machine immediately emits a plain-HTTP GET to the peer's IP:41731.
    machine.peer_replied(from_peer, has, 1_000_000);
    let eff = machine.poll(1_000_000).expect("peer HTTP effect");
    match eff {
        Effect::Http { url, https_only, redirects, .. } => {
            assert!(!https_only);
            assert_eq!(redirects, 0);
            assert!(url.as_str().starts_with("http://192.168.10.5:41731/v1/crate/"));
        }
        other => panic!("expected peer Effect::Http, got {other:?}"),
    }
}
