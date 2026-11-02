#![forbid(unsafe_code)]

use std::{io::Read, path::Path, sync::Arc, time::Duration};

use blaze_ids::rng::SplitMix64;
use blaze_protocol::events::{Event, Fns, Function, How, Record, Summary};

use crate::{
    Following, Graphs, Next, Roots, State,
    http::{self, Frame, Frames, Refused, Request, Route, Site},
    kept::{self, KEPT, Kept, Kind},
    world::{Applied, Calls, World, module_path},
};

fn site() -> Site {
    Site { port: 4242, token: "0123abcd".into(), graphs: Arc::default() }
}

fn get(path: &str) -> Request {
    Request {
        method: "GET".into(),
        path: path.into(),
        host: Some("127.0.0.1:4242".into()),
        ..Request::default()
    }
}

fn websocket() -> Request {
    Request {
        origin: Some("http://127.0.0.1:4242".into()),
        upgrade: Some("websocket".into()),
        key: Some("dGhlIHNhbXBsZSBub25jZQ==".into()),
        version: Some("13".into()),
        ..get("/0123abcd/ws")
    }
}

#[test]
fn the_accept_of_rfc_6455() {
    assert_eq!(http::accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[test]
fn the_page_is_served_under_its_token() {
    let site = site();
    for path in ["/0123abcd/", "/0123abcd", "/0123abcd/index.html", "/0123abcd/?fixture=x"] {
        let route = site.route(&get(path));
        assert!(matches!(route, Ok(Route::Asset("text/html; charset=utf-8", _))), "{path}");
    }
    assert_eq!(site.route(&get("/0123abcd/nothing.js")), Err(Refused::NotFound));
    let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".to_owned();
    assert_eq!(site.route(&websocket()), Ok(Route::Websocket { accept }));
}

#[test]
fn what_is_not_the_page_is_refused() {
    let site = site();
    for path in ["/", "/index.html", "/0123abce/", "/0123abc/", "/0123abcd0/", "/../0123abcd/"] {
        assert_eq!(site.route(&get(path)), Err(Refused::Forbidden), "{path}");
    }
    // A name that leads to the machine is not enough: a site can make one lead to 127.0.0.1.
    for host in [None, Some("evil.example:4242"), Some("127.0.0.1:4243"), Some("127.0.0.1")] {
        let request = Request { host: host.map(str::to_owned), ..get("/0123abcd/") };
        assert_eq!(site.route(&request), Err(Refused::Forbidden), "{host:?}");
    }
    let other = Request { origin: Some("https://evil.example".into()), ..websocket() };
    assert_eq!(site.route(&other), Err(Refused::Forbidden));
    let none = Request { origin: None, ..websocket() };
    assert_eq!(site.route(&none), Err(Refused::Forbidden));
    let post = Request { method: "POST".into(), ..get("/0123abcd/") };
    assert_eq!(site.route(&post), Err(Refused::MethodNotAllowed));
    let old = Request { version: Some("8".into()), ..websocket() };
    assert!(matches!(site.route(&old), Err(Refused::Malformed(_))));
}

#[test]
fn a_request_head_is_read_strictly() {
    let head =
        b"GET /t/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\nORIGIN:  http://127.0.0.1:1 \r\nX-Other: y";
    let request = http::parse(head).unwrap();
    assert_eq!(request.path, "/t/");
    assert_eq!(request.origin.as_deref(), Some("http://127.0.0.1:1"));
    for (head, why) in [
        (&b"GET /t/ HTTP/1.0"[..], "a version other than HTTP/1.1"),
        (b"GET /t/", "a request line not of three words"),
        (b"GET  /t/ HTTP/1.1", "a request line not of three words"),
        (b"GET /t/ HTTP/1.1\r\nHost", "a header without a colon"),
        (b"GET /t/ HTTP/1.1\r\nHost: a\r\nhost: b", "a header given twice"),
        (b"GET /t/ HTTP/1.1\r\nHost: \xff", "a head not in UTF-8"),
    ] {
        assert_eq!(http::parse(head), Err(Refused::Malformed(why)), "{head:?}");
    }
}

fn masked(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8, 1, 2, 3, 4];
    frame.extend(payload.iter().enumerate().map(|(at, byte)| byte ^ [1, 2, 3, 4][at % 4]));
    frame
}

#[test]
fn frames_of_the_page() {
    assert_eq!(http::read_frame(&mut &masked(0x8, b"")[..]).unwrap(), Frame::Close);
    assert_eq!(http::read_frame(&mut &masked(0x1, b"hi")[..]).unwrap(), Frame::Other);
    let mut two = masked(0x9, b"ping");
    two.extend(masked(0x8, b""));
    let mut from = &two[..];
    assert_eq!(http::read_frame(&mut from).unwrap(), Frame::Other);
    assert_eq!(http::read_frame(&mut from).unwrap(), Frame::Close);
    // Unmasked, too long, or cut short: no browser's.
    assert!(http::read_frame(&mut &[0x81, 0x02, b'h', b'i'][..]).is_err());
    let long = [&[0x82, 0xff][..], &(1u64 << 20).to_be_bytes()].concat();
    assert!(http::read_frame(&mut &long[..]).is_err());
    assert!(http::read_frame(&mut &masked(0x1, b"hello")[..9]).is_err());
}

#[test]
fn frames_to_the_page() {
    for (len, head) in [(5, vec![0x82, 5]), (300, vec![0x82, 126, 1, 44])] {
        let mut out = Vec::new();
        http::write_frame(&mut out, 0x2, &vec![7; len]).unwrap();
        assert_eq!(out[..head.len()], head[..]);
        assert_eq!(out.len(), head.len() + len);
    }
    let mut out = Vec::new();
    http::write_frame(&mut out, 0x2, &vec![0; 70_000]).unwrap();
    assert_eq!(out[..10], [0x82, 127, 0, 0, 0, 0, 0, 1, 0x11, 0x70]);
}

fn function(id: u32, key: u64, name: &str, calls: &[u32], how: How) -> Function {
    Function { id, key, krate: "anim".into(), name: name.into(), calls: calls.to_vec(), how }
}

fn fns(object: u32, whole: bool, fns: Vec<Function>) -> Record {
    Record { at_ns: 1, event: Event::Functions { object, whole, functions: fns } }
}

fn began(build: u64, root: &str) -> Record {
    let event = Event::Began { build, command: "build".into(), root: root.into() };
    Record { at_ns: 0, event }
}

/// The snapshot's functions by name, with their `how` and the names of what they call.
fn read(snapshot: &[u8]) -> Vec<(String, u8, Vec<String>)> {
    let mut at = 0;
    let mut take = |n: usize| {
        at += n;
        &snapshot[at - n..at]
    };
    let u32_ = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap());
    assert_eq!(take(4), b"BLZW");
    assert_eq!(u32_(take(4)), 1);
    let len = u32_(take(4)) as usize;
    take(len);
    take(8);
    for _ in 0..u32_(take(4)) {
        let len = u32_(take(4)) as usize;
        take(len);
    }
    let mut nodes = Vec::new();
    for _ in 0..u32_(take(4)) {
        take(8 + 4);
        let how = take(1)[0];
        take(16);
        let len = u32_(take(4)) as usize;
        nodes.push((String::from_utf8(take(len).to_vec()).unwrap(), how, Vec::new()));
    }
    for _ in 0..u32_(take(4)) {
        let (from, to) = (u32_(take(4)) as usize, u32_(take(4)) as usize);
        let callee = nodes[to].0.clone();
        nodes[from].2.push(callee);
    }
    assert_eq!(at, snapshot.len());
    nodes
}

#[test]
fn an_edit_is_merged_into_the_program_by_key() {
    let mut world = World::new().unwrap();
    let mut program = world.program(Path::new("/w"));
    world.apply(&mut program, 1, &began(1, "/w"));
    // The calls of a batch may name functions of a later one.
    let cold = fns(0, true, vec![function(0, 10, "main", &[1, 9], How::Compiled { ns: 5 })]);
    assert_eq!(world.apply(&mut program, 1, &cold), Applied::Anew);
    let more = fns(0, true, vec![function(1, 11, "render", &[], How::Stored)]);
    assert_eq!(world.apply(&mut program, 1, &more), Applied::Told);
    assert_eq!((program.functions(), program.calls()), (2, 1));
    // An edit's patch numbers its functions anew; `render` calls into the base, id 0 of
    // the patch is `render` itself.
    world.apply(&mut program, 2, &began(2, "/w"));
    let patch = fns(1, false, vec![function(0, 11, "render", &[5], How::Compiled { ns: 9 })]);
    assert_eq!(world.apply(&mut program, 2, &patch), Applied::Told);
    let snapshot = read(&world.snapshot(&program));
    let main = ("main".into(), 0, vec!["render".into()]);
    assert_eq!(snapshot, [main, ("render".into(), 0, vec![])]);
    // A build that writes the program again starts it again.
    world.apply(&mut program, 3, &began(3, "/w"));
    let again = fns(2, true, vec![function(0, 12, "main", &[], How::Unchanged)]);
    assert_eq!(world.apply(&mut program, 3, &again), Applied::Anew);
    let names: Vec<_> = read(&world.snapshot(&program)).into_iter().map(|it| it.0).collect();
    assert_eq!(names, ["main"]);
    assert_eq!(program.told_count(), 1, "the last build's records, but its functions");
    assert_eq!(records(program.told().to_vec()), [began(3, "/w")]);
}

#[test]
fn the_world_is_read_over_flecs_rest_and_never_written() {
    let mut world = World::new().unwrap();
    let mut program = world.program(Path::new("/w"));
    world.apply(&mut program, 1, &began(1, "/w"));
    let name = "<alloc::vec::Vec<u8>>::push";
    let cold = fns(
        0,
        true,
        vec![
            function(0, 10, "anim::scene::render", &[1], How::Compiled { ns: 5 }),
            function(1, 11, name, &[], How::Stored),
            function(2, 12, "anim::main", &[0], How::Compiled { ns: 7 }),
        ],
    );
    world.apply(&mut program, 1, &cold);
    // `main` is at its crate's root, in no module: its call is of no module, and of no other crate.
    let answer = world.rest("GET", "/query?expr=Crate,(Calls,*)").unwrap();
    let body = String::from_utf8_lossy(&answer.body);
    assert!(answer.code == 200 && body.contains(r#""results":[]"#), "{body}");
    let answer = world.rest("GET", "/query?expr=Function").unwrap();
    assert_eq!(answer.code, 200);
    let body = String::from_utf8(answer.body).unwrap();
    assert!(body.contains("anim::scene::render") && body.contains(name), "{body}");
    // `render` calls `push`: `render` has it as callee, and `scene` has `Vec`.
    let answer = world.rest("GET", "/query?expr=Module,Callees").unwrap();
    let body = String::from_utf8_lossy(&answer.body);
    assert!(
        answer.code == 200 && body.contains("\"name\":\"scene\"") && body.contains("vec.Vec"),
        "{body}"
    );
    let answer = world.rest("GET", "/query?expr=Function,Callees").unwrap();
    let body = String::from_utf8_lossy(&answer.body);
    let render = r##""name":"anim::scene::render", "how":0, "ns":5, "build":1}, ["#"##;
    assert!(answer.code == 200 && body.contains(render), "{body}");
    assert_eq!(world.rest("PUT", "/entity/made").unwrap().code, 403);
    assert_eq!(world.rest("DELETE", "/entity/flecs").unwrap().code, 403);
    // The modules of a function's path are entities under its crate.
    let answer = world.rest("GET", "/query?expr=Module").unwrap();
    let body = String::from_utf8(answer.body).unwrap();
    assert!(body.contains("\"name\":\"scene\"") && body.contains("\"parent\":\"#"), "{body}");
    // What is in a program goes with it.
    world.forget(program);
    for kind in ["Function", "Module", "Crate"] {
        let answer = world.rest("GET", &format!("/query?expr={kind}")).unwrap();
        let body = String::from_utf8_lossy(&answer.body);
        assert!(body.contains(r#""results":[]"#), "{kind}: {body}");
    }
}

#[test]
fn a_function_is_in_the_path_the_page_groups_it_by() {
    let cases = [
        ("anim::scene::render", "anim::scene"),
        ("anim::main::{closure#0}", "anim"),
        ("core::ptr::drop_in_place::<anim::Thing>", "core::ptr"),
        ("<alloc::vec::Vec<T> as core::ops::drop::Drop>::drop", "alloc::vec::Vec"),
        ("<alloc::vec::Vec<u8>>::push", "alloc::vec::Vec"),
        ("<anim::Mesh>::new", "anim::Mesh"),
        ("anim::Mesh::{impl#2}::build", "anim::Mesh"),
        ("<<anim::A as anim::T>::Out as core::fmt::Debug>::fmt", "anim::A"),
        ("<(A, B) as core::clone::Clone>::clone", ""),
        ("<&mut wgpu::Device as core::fmt::Debug>::fmt", "wgpu::Device"),
        ("<&[u8] as core::fmt::Debug>::fmt", ""),
        ("<fn() -> u8 as core::fmt::Debug>::fmt", ""),
        ("<[u8] as core::ops::Index<core::ops::Range<usize>>>::index", ""),
        ("main", ""),
    ];
    for (name, path) in cases {
        assert_eq!(module_path(name).join("::"), path, "{name}");
    }
}

fn message(next: Next) -> Vec<u8> {
    match next {
        Next::Message(message) => message.to_vec(),
        Next::Nothing => panic!("nothing sent"),
        Next::Gone => panic!("hung up"),
    }
}

fn gunzip(message: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(message.as_slice()).read_to_end(&mut out).unwrap();
    out
}

#[test]
fn a_page_is_sent_the_snapshot_then_each_record() {
    let graphs = Graphs::default();
    let wait = Duration::from_secs(10);
    // What was handed to the world before is done once it answers.
    let done = || assert_eq!(graphs.rest("GET", "/world").unwrap().code, 200);
    graphs.record(Path::new("/w"), 1, began(1, "/w"));
    let cold = fns(0, true, vec![function(0, 10, "main", &[], How::Compiled { ns: 5 })]);
    graphs.record(Path::new("/w"), 1, cold.clone());
    let page = graphs.follow().unwrap();
    assert_eq!(read(&gunzip(message(page.next(wait))))[0].0, "main");
    assert_eq!(records(message(page.next(wait))), [began(1, "/w")]);
    done();
    assert!(
        matches!(page.next(Duration::ZERO), Next::Nothing),
        "its functions are in the snapshot"
    );
    // A record of the build is sent as it is; one of another workspace is not, until that
    // one's build begins, which sends its program, then the Began.
    let said = Record { at_ns: 2, event: Event::Unlisted { count: 3 } };
    graphs.record(Path::new("/w"), 1, said.clone());
    assert_eq!(records(message(page.next(wait))), [said.clone()]);
    graphs.record(Path::new("/v"), 2, said.clone());
    done();
    assert!(matches!(page.next(Duration::ZERO), Next::Nothing));
    graphs.record(Path::new("/v"), 3, began(3, "/v"));
    assert!(read(&gunzip(message(page.next(wait)))).is_empty());
    assert_eq!(records(message(page.next(wait))), [began(3, "/v")]);
    // A whole object of the workspace shown sends the program anew, with the record in it.
    graphs.record(Path::new("/v"), 3, cold);
    assert_eq!(read(&gunzip(message(page.next(wait))))[0].0, "main");
    done();
    assert!(
        matches!(page.next(Duration::ZERO), Next::Nothing),
        "its functions are in the snapshot"
    );
}

#[test]
fn until_a_page_is_asked_for_only_what_makes_the_program_is_kept() {
    let graphs = Graphs::default();
    let w = Path::new("/w");
    let said = Record { at_ns: 2, event: Event::Unlisted { count: 3 } };
    graphs.record(w, 1, began(1, "/w"));
    graphs.record(
        w,
        1,
        fns(0, true, vec![function(0, 10, "anim::old", &[], How::Compiled { ns: 5 })]),
    );
    graphs.record(w, 1, said.clone());
    graphs.record(w, 2, began(2, "/w"));
    let render = function(1, 12, "anim::scene::render", &[], How::Stored);
    graphs.record(
        w,
        2,
        fns(0, true, vec![function(0, 11, "anim::main", &[1], How::Compiled { ns: 5 })]),
    );
    graphs.record(w, 2, fns(0, true, vec![render]));
    graphs.record(w, 3, began(3, "/w"));
    graphs.record(w, 4, began(4, "/w"));
    graphs.record(
        w,
        4,
        fns(0, false, vec![function(0, 11, "anim::main", &[1], How::Compiled { ns: 7 })]),
    );
    graphs.record(w, 4, said.clone());
    let kept = graphs.kept();
    let functions = Kind::Functions { object: 0 };
    let made = [
        (2, Kind::Began),
        (2, functions),
        (2, functions),
        (4, Kind::Began),
        (4, functions),
        (4, Kind::Other),
    ];
    assert_eq!(kept, made);
    let page = graphs.follow().unwrap();
    assert!(graphs.kept().is_empty());
    let mut names: Vec<String> = read(&gunzip(message(page.next(Duration::from_secs(10)))))
        .into_iter()
        .map(|it| it.0)
        .collect();
    names.sort();
    assert_eq!(names, ["anim::main", "anim::scene::render"]);
    let wait = Duration::from_secs(10);
    // The last build's records, encoded once as they came, in one frame.
    assert_eq!(records(message(page.next(wait))), [began(4, "/w"), said]);
}

#[test]
fn the_flecs_rest_of_the_page_is_under_its_token() {
    let site = site();
    let route = site.route(&get("/0123abcd/flecs/query?expr=Function"));
    assert_eq!(route, Ok(Route::Flecs { path: "/query?expr=Function".into() }));
    assert_eq!(site.route(&get("/0123abcd0/flecs/world")), Err(Refused::Forbidden));
    let put = Request { method: "PUT".into(), ..get("/0123abcd/flecs/entity/made") };
    assert_eq!(site.route(&put), Err(Refused::MethodNotAllowed));
}

/// The records of a message: a CBOR sequence of them.
fn records(message: Vec<u8>) -> Vec<Record> {
    let (mut out, mut at) = (Vec::new(), 0);
    while at < message.len() {
        let end = at + item_len(&message[at..]);
        out.push(blaze_protocol::cbor::from_slice(&message[at..end]).unwrap());
        at = end;
    }
    out
}

/// The length of the CBOR item `bytes` begins with, of definite lengths as the encoding has.
fn item_len(bytes: &[u8]) -> usize {
    let (major, info) = (bytes[0] >> 5, bytes[0] & 31);
    let (arg, head) = match info {
        0..=23 => (u64::from(info), 1),
        24 => (u64::from(bytes[1]), 2),
        25 => (u64::from(u16::from_be_bytes([bytes[1], bytes[2]])), 3),
        26 => (u64::from(u32::from_be_bytes(bytes[1..5].try_into().unwrap())), 5),
        27 => (u64::from_be_bytes(bytes[1..9].try_into().unwrap()), 9),
        _ => panic!("an indefinite or reserved head: {info}"),
    };
    match major {
        0 | 1 | 7 => head,
        2 | 3 => head + arg as usize,
        4 | 5 => {
            let items = if major == 5 { 2 * arg } else { arg };
            let mut at = head;
            for _ in 0..items {
                at += item_len(&bytes[at..]);
            }
            at
        }
        _ => head + item_len(&bytes[head..]),
    }
}

const KRATES: [&str; 2] = ["anim", "core"];

/// The one name of function `key`: some in modules, some methods of a type.
fn name_of(key: u64) -> String {
    let krate = KRATES[key as usize % 2];
    if key % 5 == 0 {
        format!("<{krate}::T{}>::new{key}", key % 3)
    } else {
        format!("{krate}::m{}::f{key}", key % 3)
    }
}

fn summary() -> Summary {
    Summary {
        took_ns: 1,
        crates: 1,
        fns: Fns::default(),
        rebuilt: Vec::new(),
        more: 0,
        error: None,
        cache: None,
        slowest: Vec::new(),
    }
}

/// A seeded run of builds of `roots` workspaces (`/w0` and on): Begans, whole objects and
/// patches whose calls name ids of later records and of none, other records, and now and then
/// one of the build before, late.
fn seeded(seed: u64, roots: u32, n: usize) -> Vec<(u32, u64, Record)> {
    let mut rng = SplitMix64::new(seed);
    let (mut build, mut builds) = (0u64, vec![0u64; roots as usize]);
    let mut out = Vec::with_capacity(n);
    for at in 0..n as u64 {
        let root = rng.below(u64::from(roots)) as u32;
        let last = builds[root as usize];
        let roll = rng.below(16);
        if last == 0 || roll == 0 {
            build += 1;
            builds[root as usize] = build;
            out.push((root, build, began(build, &format!("/w{root}"))));
            continue;
        }
        let of = if last > 1 && rng.below(24) == 0 { last - 1 } else { last };
        let event = match roll {
            1..=8 => {
                let count = 1 + rng.below(4);
                let mut functions = Vec::new();
                for _ in 0..count {
                    let key = rng.below(24);
                    let calls = (0..rng.below(4)).map(|_| rng.below(10) as u32).collect();
                    let how =
                        if rng.below(2) == 0 { How::Compiled { ns: key + 1 } } else { How::Stored };
                    let krate = KRATES[key as usize % 2].into();
                    let id = rng.below(10) as u32;
                    functions.push(Function { id, key, krate, name: name_of(key), calls, how });
                }
                let (object, whole) = (rng.below(3) as u32, rng.below(4) == 0);
                Event::Functions { object, whole, functions }
            }
            9 => Event::Finished { ok: true, summary: summary() },
            _ => Event::Unlisted { count: roll },
        };
        out.push((root, of, Record { at_ns: at, event }));
    }
    out
}

/// The records kept until the world starts as they were kept before they were kept in runs:
/// one by one, every one scanned and retained on each Began and whole object.
#[derive(Default)]
struct OldKept(Vec<(u32, Vec<(u64, Kind, Record)>)>);

impl OldKept {
    fn record(&mut self, root: u32, build: u64, record: &Record) {
        let last = |records: &Vec<(u64, Kind, Record)>| records.last().map_or(0, |it| it.0);
        let mut at = match self.0.iter().position(|it| it.0 == root) {
            Some(at) => at,
            None => {
                if self.0.len() >= KEPT {
                    let oldest = (0..self.0.len()).min_by_key(|&it| last(&self.0[it].1));
                    if let Some(oldest) = oldest {
                        self.0.remove(oldest);
                    }
                }
                self.0.push((root, Vec::new()));
                self.0.len() - 1
            }
        };
        if matches!(record.event, Event::Began { .. }) {
            let began = self.0.remove(at);
            self.0.push(began);
            at = self.0.len() - 1;
        }
        let records = &mut self.0[at].1;
        let kind = match &record.event {
            Event::Began { .. } => Kind::Began,
            Event::Functions { object, .. } => Kind::Functions { object: *object },
            _ => Kind::Other,
        };
        match &record.event {
            Event::Began { .. } => {
                let mut said: Vec<u64> = records
                    .iter()
                    .filter(|it| matches!(it.1, Kind::Functions { .. }))
                    .map(|it| it.0)
                    .collect();
                said.dedup();
                records.retain(|it| match it.1 {
                    Kind::Functions { .. } => true,
                    Kind::Began => said.contains(&it.0),
                    Kind::Other => false,
                });
            }
            Event::Functions { whole: true, .. } => {
                if !records.iter().any(|it| it.0 == build && it.1 == kind) {
                    records.retain(|it| it.0 == build);
                }
            }
            _ => {}
        }
        records.push((build, kind, record.clone()));
    }

    fn drain(self) -> Vec<(u32, u64, Record)> {
        let records = self.0.into_iter();
        records.flat_map(|(root, it)| it.into_iter().map(move |it| (root, it.0, it.2))).collect()
    }
}

#[test]
fn records_kept_in_runs_are_those_kept_one_by_one() {
    for seed in 0..64 {
        // Past KEPT workspaces, the oldest go.
        let roots = if seed % 2 == 0 { 3 } else { 24 };
        let records = seeded(seed, roots, 400);
        let mut old = OldKept::default();
        for (root, build, record) in &records {
            old.record(*root, *build, record);
        }
        let last = old.0.last().map(|it| it.1.iter().map(|it| (it.0, it.1)).collect::<Vec<_>>());
        let (kinds, drained) = kept::round_trip(&records);
        assert_eq!(kinds, last.unwrap_or_default(), "seed {seed}");
        assert_eq!(drained, old.drain(), "seed {seed}");
    }
}

#[test]
fn a_build_kept_in_one_stream_comes_back_whole() {
    let mut records = vec![(0, 1, began(1, "/w"))];
    for at in 0..2000u32 {
        let record = if at % 3 == 0 {
            Record { at_ns: at.into(), event: Event::Unlisted { count: at.into() } }
        } else {
            let name = format!("anim::m{}::f{at}", at % 7);
            fns(at % 4, false, vec![function(at, at.into(), &name, &[at + 1, 0], How::Stored)])
        };
        records.push((0, 1, record));
    }
    let (kinds, drained) = kept::round_trip(&records);
    assert_eq!(kinds.len(), records.len());
    assert_eq!(drained, records);
}

/// Everything a page was sent, a snapshot as its bytes and a frame as its records, and how
/// many messages it took.
#[derive(Debug, PartialEq)]
enum Told {
    Snapshot(Vec<u8>),
    Record(Record),
}

fn told(page: &Following) -> (Vec<Told>, usize) {
    let (mut out, mut messages) = (Vec::new(), 0);
    while let Some(message) = page.ready() {
        messages += 1;
        if message.starts_with(&[0x1f, 0x8b]) {
            out.push(Told::Snapshot(gunzip(message.to_vec())));
        } else {
            out.extend(records(message.to_vec()).into_iter().map(Told::Record));
        }
    }
    (out, messages)
}

fn roots_of(roots: &Roots, n: u32) -> Vec<u32> {
    (0..n).map(|it| roots.intern(Path::new(&format!("/w{it}"))).unwrap()).collect()
}

#[test]
fn a_page_sent_frames_of_many_records_is_told_what_one_a_record_told_it() {
    for seed in 0..6 {
        let records = seeded(seed, 3, 300);
        let roots = Roots::default();
        let ids = roots_of(&roots, 3);
        // One a message is what the page was sent before frames held many.
        let run = |batch: usize| {
            let mut state = State::new(World::new().unwrap());
            let first = state.follow();
            let mut second = None;
            for (at, (root, build, record)) in records.iter().enumerate() {
                if at == records.len() / 2 {
                    second = Some(state.follow());
                }
                state.record(&roots, ids[*root as usize], *build, record);
                if (at + 1) % batch == 0 {
                    state.flush();
                }
            }
            state.flush();
            (told(&first), told(&second.unwrap()))
        };
        let (one, many) = (run(1), run(64));
        assert!(one.0.0.iter().any(|it| matches!(it, Told::Snapshot(_))), "seed {seed}");
        assert_eq!(one.0.0, many.0.0, "seed {seed}: the page that followed from the start");
        assert_eq!(one.1.0, many.1.0, "seed {seed}: the page that followed from halfway");
        assert!(many.0.1 < one.0.1, "seed {seed}: {} messages, not fewer than {}", many.0.1, one.0.1);
    }
}

#[test]
fn a_snapshot_is_made_again_once_its_program_changes() {
    let roots = Roots::default();
    let ids = roots_of(&roots, 3);
    let mut state = State::new(World::new().unwrap());
    let _page = state.follow();
    for (at, (root, build, record)) in seeded(7, 3, 400).iter().enumerate() {
        state.record(&roots, ids[*root as usize], *build, record);
        if at % 7 == 0
            && let Some(shown) = state.shown
        {
            let cached = state.snapshot(shown).unwrap();
            let made = state.world.snapshot(&state.programs[shown]);
            assert_eq!(gunzip(cached.to_vec()), made, "record {at}");
            assert!(Arc::ptr_eq(&cached, &state.snapshot(shown).unwrap()), "made once");
        }
    }
}

/// A program's calls as they were when each record scanned every call still waiting.
#[derive(Default)]
struct OldCalls {
    by_key: std::collections::HashMap<u64, usize>,
    keys: Vec<u64>,
    out: Vec<Vec<usize>>,
    ids: std::collections::HashMap<u32, std::collections::HashMap<u32, usize>>,
    unresolved: Vec<(u32, usize, u32)>,
}

impl OldCalls {
    fn apply(&mut self, record: &Record) {
        let (object, functions) = match &record.event {
            Event::Began { .. } => {
                self.ids.clear();
                self.unresolved.clear();
                return;
            }
            Event::Finished { .. } => {
                self.unresolved.clear();
                return;
            }
            Event::Functions { object, whole, functions } => {
                if *whole && !self.ids.contains_key(object) {
                    let said = std::mem::take(&mut self.ids);
                    *self = OldCalls::default();
                    self.ids = said.into_keys().map(|it| (it, Default::default())).collect();
                }
                (*object, functions)
            }
            _ => return,
        };
        let mut placed = Vec::new();
        for function in functions {
            let at = match self.by_key.get(&function.key) {
                Some(&at) => {
                    self.out[at].clear();
                    at
                }
                None => {
                    self.keys.push(function.key);
                    self.out.push(Vec::new());
                    self.by_key.insert(function.key, self.keys.len() - 1);
                    self.keys.len() - 1
                }
            };
            self.ids.entry(object).or_default().insert(function.id, at);
            placed.push(at);
        }
        let Some(ids) = self.ids.get(&object) else { return };
        let mut calls = Vec::new();
        for (function, &from) in functions.iter().zip(&placed) {
            for &call in &function.calls {
                match ids.get(&call) {
                    Some(&to) => calls.push((from, to)),
                    None => self.unresolved.push((object, from, call)),
                }
            }
        }
        self.unresolved.retain(|&(of, from, call)| {
            match (of == object).then(|| ids.get(&call)).flatten() {
                Some(&to) => {
                    calls.push((from, to));
                    false
                }
                None => true,
            }
        });
        for (from, to) in calls {
            if !self.out[from].contains(&to) {
                self.out[from].push(to);
            }
        }
    }

    fn calls(&self) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = (self.out.iter().enumerate())
            .flat_map(|(from, to)| to.iter().map(move |&to| (self.keys[from], self.keys[to])))
            .collect();
        out.sort_unstable();
        out
    }
}

#[test]
fn calls_waiting_in_chains_are_those_a_scan_found() {
    for seed in 0..32 {
        let mut world = World::new().unwrap();
        let mut program = world.program(Path::new("/w0"));
        let mut old = OldCalls::default();
        let mut keys = std::collections::HashMap::new();
        for (at, (_, build, record)) in seeded(seed, 1, 300).iter().enumerate() {
            world.apply(&mut program, *build, record);
            old.apply(record);
            if let Event::Functions { functions, .. } = &record.event {
                keys.extend(functions.iter().map(|it| (it.name.clone(), it.key)));
            }
            if at % 25 != 24 {
                continue;
            }
            let snapshot = read(&world.snapshot(&program));
            let keys = &keys;
            let mut calls: Vec<(u64, u64)> = (snapshot.iter())
                .flat_map(|(from, _, to)| to.iter().map(move |to| (keys[from], keys[to])))
                .collect();
            calls.sort_unstable();
            assert_eq!(calls, old.calls(), "seed {seed}, record {at}");
            assert_eq!(program.calls(), calls.len(), "seed {seed}, record {at}");
        }
    }
}

/// [`module_path`] as it was, a `Vec` at every step, over `char`s.
mod old {
    pub(super) fn module_path(name: &str) -> Vec<&str> {
        if name.starts_with('<') {
            return match closing(name, 0) {
                Some(end) => self_path(&name[1..end]),
                None => Vec::new(),
            };
        }
        let mut segments = segments(name);
        segments.pop();
        segments
    }

    fn split_path(name: &str) -> Vec<&str> {
        let (mut out, mut depth, mut start, mut last) = (Vec::new(), 0usize, 0, '\0');
        let mut chars = name.char_indices().peekable();
        while let Some((at, c)) = chars.next() {
            match c {
                '<' | '(' | '[' | '{' => depth += 1,
                '>' if last != '-' => depth = depth.saturating_sub(1),
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ':' if depth == 0 && chars.peek().is_some_and(|&(_, next)| next == ':') => {
                    out.push(&name[start..at]);
                    chars.next();
                    start = at + 2;
                }
                _ => {}
            }
            last = c;
        }
        out.push(&name[start..]);
        out
    }

    fn closing(name: &str, open: usize) -> Option<usize> {
        let (mut depth, mut last) = (0usize, '\0');
        for (at, c) in name[open..].char_indices() {
            if c == '<' {
                depth += 1;
            } else if c == '>' && last != '-' {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + at);
                }
            }
            last = c;
        }
        None
    }

    fn self_path(inner: &str) -> Vec<&str> {
        let (mut depth, mut last) = (0usize, '\0');
        for (at, c) in inner.char_indices() {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' if last != '-' => depth = depth.saturating_sub(1),
                ')' | ']' => depth = depth.saturating_sub(1),
                ' ' if depth == 0 && inner[at..].starts_with(" as ") => {
                    return type_path(&inner[..at]);
                }
                _ => {}
            }
            last = c;
        }
        type_path(inner)
    }

    fn type_path(ty: &str) -> Vec<&str> {
        let mut ty = ty.trim();
        loop {
            let before = ty;
            for prefix in ["&mut ", "&", "*const ", "*mut ", "dyn "] {
                ty = ty.strip_prefix(prefix).unwrap_or(ty).trim_start();
            }
            if let Some(rest) = ty.strip_prefix('\'') {
                ty = rest.split_once(' ').map_or(ty, |(_, rest)| rest.trim_start());
            }
            if ty == before {
                break;
            }
        }
        if ty.starts_with('<') {
            return match closing(ty, 0) {
                Some(end) => self_path(&ty[1..end]),
                None => Vec::new(),
            };
        }
        let fn_pointer =
            ty.starts_with("fn(") || ty.starts_with("unsafe ") || ty.starts_with("extern ");
        if fn_pointer || !ty.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return Vec::new();
        }
        segments(ty)
    }

    fn segments(path: &str) -> Vec<&str> {
        split_path(path)
            .into_iter()
            .filter(|it| !it.is_empty() && !it.starts_with('<') && !it.starts_with('{'))
            .map(|it| it.find('<').map_or(it, |lt| &it[..lt]))
            .collect()
    }
}

#[test]
fn a_module_path_read_in_place_is_the_one_collected_step_by_step() {
    const PIECES: [&str; 24] = [
        "anim", "core", "::", "::", "<", ">", " as ", "&", "&mut ", "dyn ", "{closure#0}", "(",
        ")", "[", "]", "fn(", "->", "'a ", "Vec", "T", ", ", " ", "é", "::<",
    ];
    let mut rng = SplitMix64::new(11);
    for _ in 0..20_000 {
        let mut name = String::new();
        for _ in 0..1 + rng.below(14) {
            name.push_str(PIECES[rng.index(PIECES.len())]);
        }
        assert_eq!(module_path(&name), old::module_path(&name), "{name:?}");
    }
}

#[test]
fn roots_are_numbered_once_each() {
    assert_eq!(Roots::place(0), (0, 0));
    assert_eq!(Roots::place(15), (0, 15));
    assert_eq!(Roots::place(16), (1, 0));
    assert_eq!(Roots::place(47), (1, 31));
    assert_eq!(Roots::place(48), (2, 0));
    assert!(Roots::place(u32::MAX).0 < crate::LEVELS);
    let roots = Roots::default();
    let paths: Vec<std::path::PathBuf> = (0..100).map(|it| format!("/w{it}").into()).collect();
    let ids: Vec<u32> = paths.iter().map(|it| roots.intern(it).unwrap()).collect();
    assert_eq!(ids, (0..100).collect::<Vec<u32>>());
    for (path, &id) in paths.iter().zip(&ids).rev() {
        assert_eq!(roots.intern(path), Some(id));
        assert_eq!(roots.get(id), Some(path.as_path()));
    }
    assert_eq!(roots.intern(Path::new("/w7/")), Some(7), "as Path compares them");
    assert_eq!(roots.get(100), None);
}

/// A stream that takes at most 7 bytes a write.
struct Dribble(Vec<u8>);

impl std::io::Write for Dribble {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = bytes.len().min(7);
        self.0.extend_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn frames_written_together_are_the_frames_written_alone() {
    let lens = [0usize, 5, 125, 126, 300, 0xffff, 0x1_0000, 70_000, 3, 300_000, 1];
    let payloads: Vec<Vec<u8>> =
        lens.iter().map(|&len| (0..len).map(|it| it as u8).collect()).collect();
    let mut alone = Vec::new();
    for it in &payloads {
        http::write_frame(&mut alone, 0x2, it).unwrap();
    }
    let mut together = Vec::new();
    let mut dribbled = Dribble(Vec::new());
    let mut frames = Frames::default();
    for it in &payloads {
        frames.push(&mut together, 0x2, it).unwrap();
    }
    frames.flush(&mut together).unwrap();
    for it in &payloads {
        frames.push(&mut dribbled, 0x2, it).unwrap();
    }
    frames.flush(&mut dribbled).unwrap();
    assert_eq!(together, alone);
    assert_eq!(dribbled.0, alone);
}

/// The calls of every node chained through one list are those of a list a node, as they were
/// kept before: in the order made, each node's cleared alone or all at once.
#[test]
fn calls_chained_in_one_list_are_those_of_a_list_a_node() {
    for seed in 0..64 {
        let mut rng = SplitMix64::new(seed);
        let (mut calls, mut model) = (Calls::new(), Vec::<Vec<u32>>::new());
        for step in 0..3000 {
            match rng.below(16) {
                0 if !model.is_empty() => {
                    let at = rng.index(model.len());
                    assert_eq!(calls.clear_node(at as u32), model[at].len(), "seed {seed}");
                    model[at].clear();
                }
                1 if rng.below(16) == 0 => {
                    calls.clear();
                    model.clear();
                }
                2 | 3 => {
                    calls.push_node();
                    model.push(Vec::new());
                }
                _ if !model.is_empty() => {
                    let (at, to) = (rng.index(model.len()), rng.index(model.len()) as u32);
                    assert_eq!(calls.contains(at as u32, to), model[at].contains(&to));
                    if !model[at].contains(&to) {
                        calls.push(at as u32, to);
                        model[at].push(to);
                    }
                }
                _ => {}
            }
            if step % 50 != 49 {
                continue;
            }
            assert_eq!(calls.nodes(), model.len(), "seed {seed}, step {step}");
            for (at, to) in model.iter().enumerate() {
                let chained: Vec<u32> = calls.of(at as u32).collect();
                assert_eq!(chained, *to, "seed {seed}, step {step}, node {at}");
            }
        }
    }
}

/// A build kept like the one before, until a page is asked for, allocates nothing: the runs
/// the next Began and whole object drop, the encoder's stream and the record's CBOR are reused.
#[test]
fn a_build_kept_like_the_last_allocates_nothing() {
    // The same Began each build, so each build's stream is the same bytes.
    let began = began(1, "/w");
    let calls = vec![
        function(0, 10, "anim::main", &[1], How::Stored),
        function(1, 11, "anim::scene::render", &[], How::Stored),
    ];
    let whole = fns(0, true, calls);
    let other = Record { at_ns: 2, event: Event::Unlisted { count: 3 } };
    let keep = |kept: &mut Kept, build: u64| {
        for record in [&began, &whole, &other, &other] {
            kept.keep(0, build, record);
        }
    };
    let mut kept = Kept::default();
    for build in 1..5 {
        keep(&mut kept, build);
    }
    blaze_alloc_count::assert_at_most(0, "a build kept like the last", || keep(&mut kept, 5));
    let five = [Kind::Began, Kind::Functions { object: 0 }, Kind::Other, Kind::Other];
    assert_eq!(kept.kinds(0), five.map(|kind| (5u64, kind)));
    let mut drained = Vec::new();
    kept.drain(|root, build, record| drained.push((root, build, record))).expect("kept whole");
    let records = [&began, &whole, &other, &other].map(|it| (0u32, 5u64, it.clone()));
    assert_eq!(drained, records);
}

/// A record kept that does not come back whole is the drain's error, naming its build, never
/// a world made from the records around it.
#[test]
fn a_torn_record_kept_is_an_error() {
    let other = Record { at_ns: 2, event: Event::Unlisted { count: 3 } };
    let mut kept = Kept::default();
    kept.keep(4, 7, &began(1, "/w"));
    kept.keep(4, 7, &other);
    kept.tear();
    let Err(unkept) = kept.drain(|_, _, _| {}) else { panic!("a torn record drained") };
    assert_eq!((unkept.root, unkept.build), (4, 7));
    assert!(matches!(unkept.why, kept::Why::EndsEarly), "{unkept}");
}

/// A build told every page like the one before allocates nothing once warm: the world tells
/// each record into its reused buffer, and the pages' frame is encoded into its own, once for
/// every page. Flecs's own allocations are its C heap's, which this does not count; the frame
/// sent at the flush is one allocation, and not measured.
#[test]
fn a_build_told_every_page_like_the_last_allocates_nothing() {
    let roots = Roots::default();
    let root = roots_of(&roots, 1)[0];
    let mut state = State::new(World::new().unwrap());
    let pages = [state.follow(), state.follow()];
    for page in &pages {
        told(page);
    }
    let began = began(1, "/w0");
    let calls = vec![
        function(0, 10, "anim::main", &[1], How::Stored),
        function(1, 11, "anim::scene::render", &[0], How::Compiled { ns: 4 }),
    ];
    let said = fns(0, false, calls);
    let mut round = vec![began, said];
    round.extend((0..8).map(|count| Record { at_ns: 2, event: Event::Unlisted { count } }));
    round.push(Record { at_ns: 3, event: Event::Finished { ok: true, summary: summary() } });
    for build in 1..6 {
        let tell = |state: &mut State| {
            for record in &round {
                state.record(&roots, root, build, record);
            }
        };
        if build < 5 {
            tell(&mut state);
        } else {
            blaze_alloc_count::assert_at_most(0, "a build told every page", || tell(&mut state));
        }
        state.flush();
        for page in &pages {
            let (sent, _) = told(page);
            if build > 1 {
                let records: Vec<Told> = round.iter().cloned().map(Told::Record).collect();
                assert_eq!(sent, records, "build {build}");
            }
        }
    }
    assert_eq!((state.programs[0].functions(), state.programs[0].calls()), (2, 2));
}

/// The maps of the ids of each object, kept from build to build, name what a map a build made
/// anew named: a call to an id an object's earlier build named waits, as before.
#[test]
fn ids_kept_from_build_to_build_are_each_builds_own() {
    let mut world = World::new().unwrap();
    let mut program = world.program(Path::new("/w"));
    world.apply(&mut program, 1, &began(1, "/w"));
    let first = vec![
        function(0, 10, "main", &[], How::Stored),
        function(1, 11, "render", &[], How::Stored),
    ];
    world.apply(&mut program, 1, &fns(0, true, first));
    world.apply(&mut program, 2, &began(2, "/w"));
    // Id 1 of object 0 was `render` in the build before, not in this one.
    let main = vec![function(0, 10, "main", &[1], How::Stored)];
    world.apply(&mut program, 2, &fns(0, false, main));
    assert_eq!(program.calls(), 0);
    let render = vec![function(1, 12, "draw", &[], How::Stored)];
    world.apply(&mut program, 2, &fns(0, false, render));
    let snapshot = read(&world.snapshot(&program));
    let names: Vec<_> = snapshot.iter().map(|it| (it.0.as_str(), it.2.clone())).collect();
    assert_eq!(names, [("main", vec!["draw".into()]), ("render", vec![]), ("draw", vec![])]);
}
