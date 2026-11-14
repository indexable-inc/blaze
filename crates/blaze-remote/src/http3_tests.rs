//! The HTTP/3 bucket against an in-process server: blaze-http's own core serving the objects,
//! joined to the bucket by a [`Wire`] that is a simulated network on a virtual clock. No
//! socket, no thread, no sleep, no runtime. What this does not cover: a real UDP socket, a
//! real resolver, the machine's TLS roots, R2's public address (whether it serves HTTP/3 is
//! not known, and nothing here depends on it), and `Routed`'s split between the two
//! transports, whose HTTP/1.1 side runs only on the tokio driver.

use std::{
    alloc::Global,
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use blaze_http::h3::{
    Endpoint, Event, Identity, Range, ServerConfig, Ticket as Kept, Transmit, Transport, Trust,
    Waited, Wire,
};

use crate::{
    bucket::{Asked, Bucket, Op, Reply, Ticket},
    http3::{
        H3Bucket, TICKETS_KEPT, TICKETS_LAYOUT, TICKETS_MAGIC, decode_tickets,
        encode_tickets, origin,
    },
};

const SERVER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 4433);
const CLIENT: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 50_000);
const PUBLIC_URL: &str = "https://localhost:4433";
/// One way across the simulated network.
const LATENCY: Duration = Duration::from_millis(10);
/// The most steps of one wait on the simulated wire before the test fails as stuck.
const MOST_STEPS: usize = 100_000;
/// An ask's allowance in these tests: far past what a clean network needs.
const ALLOWED_MS: u64 = 15_000;
/// How long a certificate the test makes is valid: under the 14 days a pinned one may be.
const VALID: Duration = Duration::from_secs(13 * 86_400);

/// A DER element.
fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new_in(Global);
    out.push(tag);
    match body.len() {
        len @ 0..=127 => out.push(len as u8),
        len @ 128..=255 => out.extend_from_slice(&[0x81, len as u8]),
        len => out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]),
    }
    out.extend_from_slice(body);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

/// `unix_s` as an X.509 UTCTime (`YYMMDDHHMMSSZ`), by the civil-from-days rule.
fn utc_time(unix_s: u64) -> Vec<u8> {
    let (days, secs) = ((unix_s / 86_400) as i64, unix_s % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let text = format!(
        "{:02}{month:02}{day:02}{:02}{:02}{:02}Z",
        year % 100,
        secs / 3_600,
        secs / 60 % 60,
        secs % 60
    );
    tlv(0x17, text.as_bytes())
}

/// A self-signed ECDSA P-256 certificate for `localhost`, valid from a minute ago for
/// [`VALID`] (rustls checks it against the wall clock), and its SHA-256 to pin. Made with
/// `ring`, this crate's own signer, as rcgen is not in its graph.
fn identity() -> (Identity, [u8; 32]) {
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
    };
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let ecdsa_sha256 = tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]);
    let algorithm = seq(&[&ecdsa_sha256]);
    let common_name = tlv(0x06, &[0x55, 0x04, 0x03]);
    let name = seq(&[&tlv(0x31, &seq(&[&common_name, &tlv(0x0c, b"localhost")]))]);
    let validity = seq(&[&utc_time(now - 60), &utc_time(now - 60 + VALID.as_secs())]);
    let ec_public_key = tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]);
    let p256 = tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]);
    let point = [&[0u8][..], key.public_key().as_ref()].concat();
    let spki = seq(&[&seq(&[&ec_public_key, &p256]), &tlv(0x03, &point)]);
    let version = tlv(0xa0, &tlv(0x02, &[2]));
    let serial = tlv(0x02, &[1]);
    let tbs = seq(&[&version, &serial, &algorithm, &name, &validity, &name, &spki]);
    let signature = key.sign(&rng, &tbs).unwrap();
    let signature = [&[0u8][..], signature.as_ref()].concat();
    let cert = seq(&[&tbs, &algorithm, &tlv(0x03, &signature)]);
    let pin = blaze_sha256::digest(&cert);
    (Identity { cert_der: cert, key_der: pkcs8.as_ref().to_vec() }, pin)
}

/// How the simulated server behaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Server {
    /// Answers from its objects.
    Serving,
    /// Takes requests and never answers them.
    Holding,
    /// Is not there: every datagram to it is lost.
    Gone,
}

/// The network, the clock, and the server at the other end.
struct Sim {
    epoch: Instant,
    now: Duration,
    server: Endpoint,
    mode: Server,
    /// By path.
    objects: BTreeMap<String, Vec<u8>>,
    to_server: Vec<(Duration, Vec<u8>)>,
    to_client: Vec<(Duration, Vec<u8>)>,
    /// Each request the server took: its method, path and whether it came early.
    served: Vec<(String, String, bool)>,
}

impl Sim {
    fn new(objects: &[(&str, Vec<u8>)]) -> (Sim, [u8; 32]) {
        let epoch = Instant::now();
        let (identity, pin) = identity();
        let config = ServerConfig { identity, transport: Transport::default(), requests: true };
        let server = Endpoint::new(epoch, Some(config), None).unwrap();
        let objects = objects.iter().map(|(name, bytes)| (format!("/{name}"), bytes.clone())).collect();
        let sim = Sim {
            epoch,
            now: Duration::ZERO,
            server,
            mode: Server::Serving,
            objects,
            to_server: Vec::new_in(Global),
            to_client: Vec::new_in(Global),
            served: Vec::new_in(Global),
        };
        (sim, pin)
    }

    /// The server's work at `now`: its clock, its requests answered, its datagrams sent.
    fn serve(&mut self) {
        self.server.advance_clock(self.now);
        while let Some(event) = self.server.next_event() {
            let Event::Request { request, served } = event else { continue };
            self.served.push((served.method.clone(), served.path.clone(), served.early));
            if self.mode == Server::Holding {
                continue;
            }
            let Some(object) = self.objects.get(&served.path) else {
                self.server.respond(request, 404, &[], &[]).unwrap();
                continue;
            };
            let etag = format!("\"v{}\"", object.len());
            let len = object.len().to_string();
            if served.method == "HEAD" {
                self.server.respond(request, 200, &[("content-length", &len), ("etag", &etag)], &[]).unwrap();
                continue;
            }
            match served.range.as_deref().map(|it| Range::parse(it).expect("one closed range")) {
                Some(range) => {
                    let end = (range.at + range.len).min(object.len() as u64) as usize;
                    let body = &object[range.at as usize..end];
                    let length = body.len().to_string();
                    let headers = [("content-length", length.as_str()), ("etag", &etag)];
                    self.server.respond(request, 206, &headers, body).unwrap();
                }
                None => {
                    let headers = [("content-length", len.as_str()), ("etag", &etag)];
                    self.server.respond(request, 200, &headers, object).unwrap();
                }
            }
        }
        while let Some(transmit) = self.server.next_transmit() {
            assert_eq!(transmit.to, CLIENT, "the server sends only to its client");
            self.to_client.push((self.now + LATENCY, transmit.bytes));
        }
    }

    /// The datagrams for the client landed by now.
    fn landed(&mut self, received: &mut dyn FnMut(SocketAddr, &[u8])) -> bool {
        let now = self.now;
        let mut any = false;
        self.to_client.retain(|(at, bytes)| {
            if *at > now {
                return true;
            }
            received(SERVER, bytes);
            any = true;
            false
        });
        any
    }
}

impl Wire for Sim {
    fn epoch(&self) -> Instant {
        self.epoch
    }

    fn now(&mut self) -> Duration {
        self.now
    }

    fn send(&mut self, transmit: Transmit) {
        assert_eq!(transmit.to, SERVER, "the bucket sends only to its origin");
        if self.mode != Server::Gone {
            self.to_server.push((self.now + LATENCY, transmit.bytes));
        }
    }

    fn wait(&mut self, until: Duration, received: &mut dyn FnMut(SocketAddr, &[u8])) -> Waited {
        for _ in 0..MOST_STEPS {
            self.serve();
            if self.landed(received) {
                return Waited::Received;
            }
            let next = [
                self.to_server.iter().map(|it| it.0).min(),
                self.to_client.iter().map(|it| it.0).min(),
                self.server.next_deadline(),
            ]
            .into_iter()
            .flatten()
            .min();
            let Some(next) = next.filter(|it| *it <= until) else {
                self.now = self.now.max(until);
                self.serve();
                return if self.landed(received) { Waited::Received } else { Waited::Deadline };
            };
            self.now = self.now.max(next);
            self.server.advance_clock(self.now);
            let now = self.now;
            let mut due = Vec::new_in(Global);
            self.to_server.retain(|(at, bytes)| {
                if *at > now {
                    return true;
                }
                due.push(bytes.clone());
                false
            });
            for bytes in due {
                self.server.received(CLIENT, &bytes);
            }
        }
        panic!("the simulated wire took {MOST_STEPS} steps in one wait by {:?}", self.now);
    }
}

fn bucket(sim: Sim, pin: [u8; 32], kept: &[Kept]) -> H3Bucket<Sim> {
    H3Bucket::new(sim, PUBLIC_URL, SERVER, Trust::Pinned(vec![pin]), kept).unwrap()
}

/// Sends `ops` and waits for every answer, by ticket.
fn ask_all(bucket: &mut H3Bucket<Sim>, ops: Vec<Op>, allowed_ms: u64) -> BTreeMap<Ticket, Reply> {
    let now = bucket.now_ms();
    let mut asked = Vec::new_in(Global);
    asked.extend(ops.into_iter().enumerate().map(|(i, op)| Asked {
        ticket: Ticket(i as u64),
        op,
        deadline_ms: now + allowed_ms,
    }));
    let count = asked.len();
    bucket.send(asked);
    let mut out = BTreeMap::new();
    while bucket.in_flight() > 0 {
        out.extend(bucket.wait());
    }
    assert_eq!(out.len(), count, "each ask answered once");
    out
}

fn objects() -> Vec<(&'static str, Vec<u8>)> {
    let segment: Vec<u8> = (0..100_000u32).map(|it| (it * 31 % 251) as u8).collect();
    vec![("public/t/tc/segments/aa", segment), ("public/t/tc/roots/main", b"root".to_vec())]
}

#[test]
fn reads_over_http3_are_answered_as_over_http1() {
    let objects = objects();
    let (sim, pin) = Sim::new(&objects);
    let mut bucket = bucket(sim, pin, &[]);
    let segment = &objects[0].1;
    let replies = ask_all(
        &mut bucket,
        vec![
            Op::Get { name: objects[0].0.to_owned(), range: Some((1_000, 70_000)), most: 1 << 20 },
            Op::Get { name: objects[0].0.to_owned(), range: None, most: 1 << 20 },
            Op::Head { name: objects[1].0.to_owned() },
            Op::Get { name: "public/t/tc/segments/missing".to_owned(), range: None, most: 1 << 20 },
        ],
        ALLOWED_MS,
    );
    let etag = format!("\"v{}\"", segment.len());
    assert_eq!(replies[&Ticket(0)], Reply::Got { bytes: segment[1_000..71_000].to_vec(), etag: etag.clone() });
    assert_eq!(replies[&Ticket(1)], Reply::Got { bytes: segment.clone(), etag });
    assert_eq!(replies[&Ticket(2)], Reply::Exists { len: 4 });
    assert_eq!(replies[&Ticket(3)], Reply::Missing);
    let counts = bucket.counts();
    assert_eq!((counts.requests, counts.received), (4, 70_000 + segment.len() as u64));
}

#[test]
fn a_body_past_what_was_asked_is_a_failure_naming_the_object() {
    let objects = objects();
    let (sim, pin) = Sim::new(&objects);
    let mut bucket = bucket(sim, pin, &[]);
    let name = objects[0].0;
    let replies = ask_all(&mut bucket, vec![Op::Get { name: name.to_owned(), range: None, most: 10 }], ALLOWED_MS);
    let Reply::Failed { why } = &replies[&Ticket(0)] else { panic!("{replies:?}") };
    assert!(why.contains(name), "{why}");
}

#[test]
fn a_second_process_reads_in_0rtt_with_the_tickets_the_first_kept() {
    let objects = objects();
    let (sim, pin) = Sim::new(&objects);
    let mut first = bucket(sim, pin, &[]);
    ask_all(&mut first, vec![Op::Head { name: objects[1].0.to_owned() }], ALLOWED_MS);
    let tickets = first.take_tickets();
    assert!(!tickets.is_empty(), "the server sent a ticket");
    let now_s = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let file = encode_tickets(&tickets);
    let kept = decode_tickets(&file, now_s).unwrap();
    assert_eq!(kept.as_slice(), tickets.as_slice(), "the tickets file round-trips");

    let mut sim = first.into_wire();
    sim.served.clear();
    let mut second = bucket(sim, pin, &kept);
    let range = Some((0, 4));
    let replies = ask_all(&mut second, vec![Op::Get { name: objects[1].0.to_owned(), range, most: 4 }], ALLOWED_MS);
    assert_eq!(replies[&Ticket(0)], Reply::Got { bytes: b"root".to_vec(), etag: "\"v4\"".to_owned() });
    let served = &second.wire().served;
    assert_eq!(served.len(), 1, "{served:?}");
    assert!(served[0].2, "the resumed read came as early data: {served:?}");
}

#[test]
fn a_read_not_answered_by_its_deadline_is_timed_out() {
    let objects = objects();
    let (mut sim, pin) = Sim::new(&objects);
    sim.mode = Server::Holding;
    let mut bucket = bucket(sim, pin, &[]);
    let allowed_ms = 500;
    let replies = ask_all(&mut bucket, vec![Op::Head { name: objects[1].0.to_owned() }], allowed_ms);
    assert_eq!(replies[&Ticket(0)], Reply::TimedOut);
    let now = bucket.wire().now;
    assert!(now >= Duration::from_millis(allowed_ms), "answered at {now:?}, before its deadline");
}

#[test]
fn a_host_that_does_not_answer_over_udp_fails_naming_the_host_how_and_the_setting() {
    let objects = objects();
    let (mut sim, pin) = Sim::new(&objects);
    sim.mode = Server::Gone;
    let mut bucket = bucket(sim, pin, &[]);
    let name = objects[1].0;
    // Past the QUIC idle timeout, so the handshake's failure answers it, not its deadline.
    let replies = ask_all(&mut bucket, vec![Op::Head { name: name.to_owned() }], 60_000);
    let Reply::Failed { why } = &replies[&Ticket(0)] else { panic!("{replies:?}") };
    for part in [name, "localhost", "handshake timed out", "bucket.transport (BLAZE_CACHE_TRANSPORT)", "\"h1\""] {
        assert!(why.contains(part), "{part:?} not in {why}");
    }
}

#[test]
fn a_signed_request_is_never_sent_over_http3() {
    let objects = objects();
    let (sim, pin) = Sim::new(&objects);
    let mut bucket = bucket(sim, pin, &[]);
    let put = Op::Put { name: "public/t/tc/roots/main".to_owned(), bytes: b"x".to_vec(), cond: crate::bucket::Cond::Any };
    let private = Op::Get { name: "0123456789abcdef/t/tc/roots/main".to_owned(), range: None, most: 10 };
    let replies = ask_all(&mut bucket, vec![put, private], ALLOWED_MS);
    for reply in replies.values() {
        let Reply::Failed { why } = reply else { panic!("{replies:?}") };
        assert!(why.contains("HTTP/1.1"), "{why}");
    }
    assert!(bucket.wire().served.is_empty(), "nothing reached the server");
}

/// The invariant: a tickets file decodes to the tickets encoded, at most [`TICKETS_KEPT`] of
/// them, without those past their age; a file of another layout, cut short, or with bytes
/// past its last ticket is refused, naming the file.
#[test]
fn the_tickets_file_round_trips_and_refuses_what_it_is_not() {
    let ticket = |i: u64, lifetime_s: u32| Kept {
        server_name: "localhost".to_owned(),
        issued_unix_s: 1_000 + i,
        lifetime_s,
        bytes: vec![i as u8; 40],
    };
    let tickets: Vec<Kept> = (0..TICKETS_KEPT as u64 + 2).map(|i| ticket(i, 3_600)).collect();
    let file = encode_tickets(&tickets);
    let kept = decode_tickets(&file, 1_000).unwrap();
    assert_eq!(kept.as_slice(), &tickets[2..], "the newest are kept");
    assert!(decode_tickets(&file, 1_000 + 3_600 + TICKETS_KEPT as u64 + 2).unwrap().is_empty(), "all are past their lifetime");

    let mut other = file.to_vec();
    other[TICKETS_MAGIC.len()] = TICKETS_LAYOUT + 1;
    let why = decode_tickets(&other, 1_000).unwrap_err();
    assert!(why.contains("remote-tickets.bin") && why.contains(&format!("layout {}", TICKETS_LAYOUT + 1)), "{why}");
    assert!(decode_tickets(&file[..file.len() - 1], 1_000).unwrap_err().contains("cut short"));
    let mut longer = file.to_vec();
    longer.push(0);
    assert!(decode_tickets(&longer, 1_000).unwrap_err().contains("past its last ticket"));
    assert!(decode_tickets(b"not a tickets file", 1_000).is_err());
}

#[test]
fn the_origin_of_a_public_address() {
    assert_eq!(origin("https://cache.example").unwrap(), ("cache.example".to_owned(), "cache.example".to_owned(), 443));
    assert_eq!(origin("https://localhost:4433/").unwrap(), ("localhost".to_owned(), "localhost:4433".to_owned(), 4433));
    assert_eq!(origin("https://[::1]:8443").unwrap(), ("::1".to_owned(), "[::1]:8443".to_owned(), 8443));
    for refused in ["http://cache.example", "https://", "https://a/b", "https://u@a", "https://a:x", "https://[::1"] {
        assert!(origin(refused).is_err(), "{refused}");
    }
}
