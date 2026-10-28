use std::{io::Read, path::Path, sync::Arc, time::Duration};

use blaze_protocol::events::{Event, Function, How, Record};

use crate::{
    Graphs, Next,
    http::{self, Frame, Refused, Request, Route, Site},
    world::{Applied, World, path_of},
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
    let head = b"GET /t/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\nORIGIN:  http://127.0.0.1:1 \r\nX-Other: y";
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
    assert_eq!(program.records().len(), 1, "the last build's records, but its functions");
}

#[test]
fn the_world_is_read_over_flecs_rest_and_never_written() {
    let mut world = World::new().unwrap();
    let mut program = world.program(Path::new("/w"));
    world.apply(&mut program, 1, &began(1, "/w"));
    let name = "<alloc::vec::Vec<u8>>::push";
    let cold = fns(0, true, vec![function(0, 10, "anim::scene::render", &[1], How::Compiled { ns: 5 }),
                                 function(1, 11, name, &[], How::Stored)]);
    world.apply(&mut program, 1, &cold);
    let answer = world.rest("GET", "/query?expr=Function").unwrap();
    assert_eq!(answer.code, 200);
    let body = String::from_utf8(answer.body).unwrap();
    assert!(body.contains("anim::scene::render") && body.contains(name), "{body}");
    let answer = world.rest("GET", "/query?expr=(Calls,*)").unwrap();
    assert_eq!(answer.code, 200, "{}", String::from_utf8_lossy(&answer.body));
    assert_eq!(world.rest("PUT", "/entity/made").unwrap().code, 403);
    assert_eq!(world.rest("DELETE", "/entity/flecs").unwrap().code, 403);
    // The modules of a function's path are entities under its crate.
    let answer = world.rest("GET", "/query?expr=Module").unwrap();
    let body = String::from_utf8(answer.body).unwrap();
    assert!(body.contains("scene"), "{body}");
}

#[test]
fn a_path_is_a_name_without_its_generic_arguments() {
    assert_eq!(path_of("<alloc::vec::Vec<u8>>::push"), "alloc::vec::Vec::push");
    assert_eq!(path_of("core::ptr::drop_in_place::<anim::Scene>"), "core::ptr::drop_in_place");
    assert_eq!(
        path_of("<wgpu::Texture as core::ops::drop::Drop>::drop"),
        "<wgpu::Texture as core::ops::drop::Drop>::drop"
    );
    assert_eq!(path_of("<[u8] as core::ops::Index<core::ops::Range<usize>>>::index"),
               "<[u8] as core::ops::Index>::index");
    assert_eq!(path_of("anim::main::{closure#0}"), "anim::main::{closure#0}");
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
    let began_again = blaze_protocol::cbor::from_slice::<Record>(&message(page.next(wait)));
    assert_eq!(began_again.unwrap(), began(1, "/w"));
    done();
    assert!(matches!(page.next(Duration::ZERO), Next::Nothing), "its functions are in the snapshot");
    // A record of the build is sent as it is; one of another workspace is not, until that
    // one's build begins, which sends its program.
    let said = Record { at_ns: 2, event: Event::Unlisted { count: 3 } };
    graphs.record(Path::new("/w"), 1, said.clone());
    assert_eq!(blaze_protocol::cbor::from_slice::<Record>(&message(page.next(wait))).unwrap(), said);
    graphs.record(Path::new("/v"), 2, said.clone());
    done();
    assert!(matches!(page.next(Duration::ZERO), Next::Nothing));
    graphs.record(Path::new("/v"), 3, began(3, "/v"));
    assert!(read(&gunzip(message(page.next(wait)))).is_empty());
    // A whole object of the workspace shown sends the program anew, with the record in it.
    graphs.record(Path::new("/v"), 3, cold);
    assert_eq!(read(&gunzip(message(page.next(wait))))[0].0, "main");
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
