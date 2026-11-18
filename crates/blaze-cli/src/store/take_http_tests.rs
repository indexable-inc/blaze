//! The take over HTTP (store/take.rs, store.rs `NamedOrigin::published`): the origin named as
//! `http://127.0.0.1:8091/o/` is joined to the set's folder (`/o/public/<toolchain>/<target>`),
//! and a simulated server serving the simulation's files with ranges answers each ranged GET.
//! A home that takes the set over HTTP holds the same records, byte for byte, as one that takes
//! it from the folder, with the same counts; a server that flips a payload byte is refused by
//! name and nothing is rooted. No network: bstd's simulated one, one lane playing both ends
//! (`HttpOrigin::read_with`, bstd's begin/finish split).

use std::{format, string::String};

use blaze_cas::Key as CasKey;
use blaze_remote::read::{Object, Origin, OriginError};
use blaze_remote::trust::Signer;
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::net::{Addr, Conn, HostPort, Listener};
use bstd::path::Path;

use super::super::published_e2e_tests::{SEED, TARGET, TOOLCHAIN, developer_home, set_name};
use super::super::{CommandLent, HttpOrigin, NamedOrigin, OriginAt};
use super::tests::{named_by_root, rooted, set_id, world};
use super::{How, SegmentWhy, TakeError, TakePlant, Taken, take_from};

const AT: (&str, u16) = ("127.0.0.1", 8091);

/// The request's target and range (`bytes=a-b`), from its head.
fn request(cx: &Cx, conn: &Conn) -> Result<(String, u64, u64), String> {
    let mut got = cx.bytes();
    for _ in 0..8 {
        if let Some(end) = got.as_slice().windows(4).position(|it| it == b"\r\n\r\n") {
            let head = core::str::from_utf8(&got.as_slice()[..end]).map_err(|e| format!("harness: {e}"))?;
            let target = head.lines().next().and_then(|l| l.split(' ').nth(1)).ok_or("harness: no target")?.to_owned();
            let range = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case("range")).map(|(_, v)| v.trim()))
                .and_then(|v| v.strip_prefix("bytes="))
                .and_then(|v| v.split_once('-'))
                .ok_or("harness: no range")?;
            let first = range.0.parse::<u64>().map_err(|e| format!("harness: {e}"))?;
            let last = range.1.parse::<u64>().map_err(|e| format!("harness: {e}"))?;
            return Ok((target, first, last));
        }
        conn.recv(cx, 4096, &mut got).map_err(|e| format!("harness: receive: {e}"))?;
    }
    Err(String::from("harness: no whole request"))
}

/// One request answered from the simulation's files (the target's path beneath `root`), with
/// ranges; a liar flips the first byte of every `seg-` reply past the segment's header.
fn answer(cx: &Cx, conn: &Conn, root: &Dir<ReadWrite>, lies: bool) -> Result<(), String> {
    let (target, first, last) = request(cx, conn)?;
    let name = target.strip_prefix('/').ok_or("harness: a target that is not a path")?;
    let whole = match root.open_read(cx, &Path::new(name).map_err(|e| format!("harness: {e}"))?) {
        Ok(file) => {
            let mut bytes = cx.bytes();
            file.read_all(cx, &mut bytes).map_err(|e| format!("harness: {e}"))?;
            Some(bytes.as_slice().to_vec())
        }
        Err(_) => None,
    };
    let wire = match whole {
        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        Some(whole) if first >= whole.len() as u64 => {
            format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n\r\n", whole.len()).into_bytes()
        }
        Some(whole) => {
            let end = (last + 1).min(whole.len() as u64);
            let mut body = whole[first as usize..end as usize].to_vec();
            // Past the 128-byte header: a payload's byte (blaze-cas `HEADER`).
            if lies && name.rsplit('/').next().is_some_and(|it| it.starts_with("seg-")) && body.len() > 200 {
                body[200] ^= 0x5a;
            }
            let mut wire = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {first}-{}/{}\r\nContent-Length: {}\r\n\r\n",
                end - 1,
                whole.len(),
                body.len()
            )
            .into_bytes();
            wire.extend_from_slice(&body);
            wire
        }
    };
    conn.send_all(cx, &wire).map_err(|e| format!("harness: send: {e}"))
}

/// `HttpOrigin` with the test's server answering each ask between its send and its read.
struct Served<'t> {
    origin: HttpOrigin<'t>,
    listener: &'t mut Listener,
    root: &'t Dir<ReadWrite>,
    lies: bool,
    served: u32,
    failed: Option<String>,
}

impl Origin for Served<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        let Served { origin, listener, root, lies, served, failed } = self;
        let mut outcome = Ok(());
        let read = origin.read_with(object, at, into, deadline_ns, |cx| {
            outcome = (|| {
                let conn = listener.accept(cx).map_err(|e| format!("harness: accept: {e}"))?;
                let answered = answer(cx, &conn, root, *lies);
                let finished = conn.finish(cx);
                answered?;
                finished.map_err(|e| format!("harness: the server's finish: {e}"))
            })();
        });
        match outcome {
            Ok(()) => *served += 1,
            Err(why) => *failed = Some(why),
        }
        read
    }
}

/// The set taken into `lent` over HTTP from the origin `http://127.0.0.1:8091/o/` (A's key
/// named), the server a liar when `lies`: the take, and the requests the server answered.
fn over_http(world: &super::tests::World, cx: &Cx, lent: &CommandLent, lies: bool) -> (Result<Taken, TakeError>, u32) {
    let io = &world.io;
    let fs = io.need_fs().expect("files");
    let net = io.need_net().expect("a network");
    let hex = TOOLCHAIN.hex();
    let key = Signer::from_seed(&SEED).public().to_string();
    let base = format!("http://{}:{}/o/", AT.0, AT.1);
    let named = NamedOrigin::published(cx, &fs, Some(&net), &base, Some(key.as_str()), set_name(&hex), lent).expect("the published origin over http");
    let OriginAt::Http(at) = &named.at else { panic!("an http:// origin was not read over HTTP") };
    // The join: the base, then the set's folder; each object's name is added per ask.
    assert_eq!(at.base.as_str(), format!("http://{}:{}/o/public/{}/{TARGET}", AT.0, AT.1, hex.as_str()), "the origin's base joined to the set's folder");
    let addr = Addr::Tcp(HostPort::new(AT.0, AT.1).expect("an address"));
    let mut listener = net.listen(cx, &addr).expect("a listener");
    let clock = io.need_clock().expect("a clock");
    let root = fs.root();
    let mut served = Served { origin: HttpOrigin::new(cx, at, &clock), listener: &mut listener, root: &root, lies, served: 0, failed: None };
    let taken = take_from(&mut served, named.check.named_keys(), set_id(), lent, TOOLCHAIN, &clock, TakePlant::NONE);
    let (requests, failed) = (served.served, served.failed.take());
    let _ = listener.close(cx);
    assert_eq!(failed, None, "the test's server");
    (taken, requests)
}

#[test]
fn a_set_taken_over_http_is_byte_equal_to_the_set_taken_from_the_folder() {
    let world = world(71);
    let cx = world.io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    // B from the folder, through the command's own path (`NamedOrigin::published`, `take`).
    let (_b, lent_b) = developer_home(&world.io, &cx, "/b", 6);
    let by_folder = {
        let fs = world.io.need_fs().expect("files");
        let hex = TOOLCHAIN.hex();
        let key = Signer::from_seed(&SEED).public().to_string();
        let named = NamedOrigin::published(&cx, &fs, None, "o", Some(key.as_str()), set_name(&hex), &lent_b).expect("the published folder origin");
        let clock = world.io.need_clock().expect("a clock");
        super::take(&cx, &named, set_id(), &lent_b, TOOLCHAIN, &clock, TakePlant::NONE).expect("B takes from the folder")
    };
    // C over HTTP.
    let (_c, lent_c) = developer_home(&world.io, &cx, "/c", 7);
    let (by_http, requests) = over_http(&world, &cx, &lent_c, false);
    let by_http = by_http.expect("C takes over HTTP");
    std::eprintln!("folder: {by_folder}\nhttp:   {by_http} ({requests} ranged GETs)");
    assert_eq!(requests, by_http.reads, "one ranged GET a read");
    let counts = |taken: &Taken| (taken.how, taken.generation, taken.records, taken.bytes, taken.already, taken.reads, taken.head_reads);
    assert_eq!(counts(&by_http), counts(&by_folder), "the counts over HTTP and from the folder");
    assert_eq!(by_http.how, How::Took);
    assert_eq!(named_by_root(&lent_c, &world.records), named_by_root(&lent_b, &world.records), "the roots");
    for (key, want) in &world.records.public {
        let b = lent_b.get(&CasKey(*key)).expect("a get").expect("B holds it");
        let c = lent_c.get(&CasKey(*key)).expect("a get").expect("C holds it");
        assert_eq!((b.bytes, b.tag, b.namespace), (c.bytes, c.tag, c.namespace), "a record over HTTP and from the folder");
        assert_eq!(c.bytes, want.as_slice());
    }
}

#[test]
fn a_server_that_flips_a_payload_byte_is_refused_and_nothing_is_rooted() {
    let world = world(73);
    let cx = world.io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let (_c, lent) = developer_home(&world.io, &cx, "/c", 7);
    let (taken, _) = over_http(&world, &cx, &lent, true);
    assert!(matches!(taken, Err(TakeError::Segment { why: SegmentWhy::Check(_), .. })), "a liar's segment: {taken:?}");
    assert!(!rooted(&lent), "a liar's set was rooted");
}
