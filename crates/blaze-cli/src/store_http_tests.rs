//! The origin over HTTP is the folder origin's equal: the same signed origin, published into a
//! folder of bstd's simulated world and served from it over the simulated network by a tiny
//! server with range support, gives byte-equal records through `FolderOrigin` and through
//! `HttpOrigin` (blaze-h1-net on bstd's `Net`). A server that lies (one payload byte flipped in
//! every segment it serves) gives no record: the read path's digest chain refuses each, and
//! nothing wrong is taken.

use std::{alloc::Global, collections::BTreeMap, format, string::String, vec::Vec as StdVec};

use blaze_cas::plain::{Namespace, seal_segment};
use blaze_ids::ContentId;
use blaze_keyed::{exec::RecordRef, kind::Tag};
use blaze_own::Erased;
use blaze_remote::read::{Answer, Limits, Reader, fetch, publish};
use blaze_remote::trust::Signer;
use bstd::{
    Io,
    cx::Cx,
    fs::{Content, Dir, Durable, ReadWrite},
    mem::TASK_FRAME,
    net::{Addr, Conn, HostPort, Listener},
    path::Path,
};
use core::time::Duration;

use super::{FolderOrigin, HttpAt, HttpOrigin, OneKey};

const AT: (&str, u16) = ("127.0.0.1", 8090);

/// The records, and the origin's objects by name.
fn origin(signer: &Signer) -> (BTreeMap<ContentId, StdVec<u8>>, BTreeMap<String, StdVec<u8>>) {
    let mut truth = BTreeMap::new();
    for i in 0..40u64 {
        truth.insert(ContentId::of("http origin test", &i.to_le_bytes()), std::vec![i as u8; 1 + (i as usize * 37) % 300]);
    }
    let refs: StdVec<(RecordRef, &[u8])> = truth.iter().map(|(k, v)| (RecordRef { tag: Tag(3), key: *k }, v.as_slice())).collect();
    let mut segment = StdVec::new();
    seal_segment(Namespace::PUBLIC, &refs, &mut segment).expect("seal");
    let global = Erased::new(Global);
    let sealed = publish(Namespace::PUBLIC, 1, signer, &[segment.as_slice()], 4, global.handle()).expect("publish");
    let objects = sealed.iter().map(|o| (o.object.name().as_str().to_owned(), o.bytes.as_slice().to_vec())).collect();
    (truth, objects)
}

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

/// Serves `folder`'s files under `/o/` with ranges, one request a connection, until no client
/// comes for a minute of the world's clock; a liar flips the first byte of every `seg-` reply.
fn serve(cx: &Cx, clock: &bstd::clock::Clock, listener: &mut Listener, folder: &Dir<ReadWrite>, lies: bool) -> Result<u32, String> {
    let mut served = 0;
    for _ in 0..256 {
        let waiting = cx.until(clock.after(Duration::from_secs(60)));
        let Ok(conn) = listener.accept(&waiting) else { return Ok(served) };
        let answered = answer(cx, &conn, folder, lies);
        let _ = conn.finish(cx);
        answered?;
        served += 1;
    }
    Err(String::from("harness: more than 256 requests"))
}

fn answer(cx: &Cx, conn: &Conn, folder: &Dir<ReadWrite>, lies: bool) -> Result<(), String> {
    let (target, first, last) = request(cx, conn)?;
    let name = target.strip_prefix("/o/").ok_or("harness: a target outside /o/")?;
    let whole = match folder.open_read(cx, &Path::new(name).map_err(|e| format!("harness: {e}"))?) {
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
            if lies && name.starts_with("seg-") {
                body[0] ^= 0x5a;
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

/// Every record of the origin read through the folder and through HTTP: the bytes each found.
fn both(lies: bool) -> (StdVec<Option<StdVec<u8>>>, StdVec<Option<StdVec<u8>>>, BTreeMap<ContentId, StdVec<u8>>) {
    let signer = Signer::from_seed(&[9; 32]);
    let (truth, objects) = origin(&signer);
    let io = Io::sim(11);
    let cx = io.cx(Duration::from_secs(3600), TASK_FRAME).expect("a root");
    let fs = io.need_fs().expect("files");
    let net = io.need_net().expect("a network");
    let root = fs.root();
    for (name, bytes) in &objects {
        root.publish(&cx, &Path::new(&format!("origin/{name}")).expect("a path"), Content::bytes(bytes), Durable::No).expect("an object");
    }
    let folder = fs.open_dir(&cx, &Path::new("origin").expect("a path")).expect("the origin folder");
    let check = OneKey { keys: [signer.public()] };
    let global = Erased::new(Global);
    let keys: StdVec<ContentId> = truth.keys().copied().collect();
    let clock = io.need_clock().expect("a clock");
    let mut now = || u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    let read = |reader: &Reader<'_>| (0..keys.len()).map(|i| matches!(reader.answer(i), Answer::Found { .. }).then(|| reader.found(i).unwrap_or(&[]).to_vec())).collect::<StdVec<_>>();
    let mut by_folder = Reader::new_in(Namespace::PUBLIC, &check, Limits::DEFAULT, global.handle());
    fetch(&mut by_folder, &mut FolderOrigin { cx: &cx, folder: &folder }, &mut now, &keys, global.handle()).expect("a batch");
    let folder_found = read(&by_folder);
    let addr = Addr::Tcp(HostPort::new(AT.0, AT.1).expect("an address"));
    let mut listener = net.listen(&cx, &addr).expect("a listener");
    let at = HttpAt::of(&format!("http://{}:{}/o/", AT.0, AT.1), &net).expect("an http origin");
    let mut by_http = Reader::new_in(Namespace::PUBLIC, &check, Limits::DEFAULT, global.handle());
    let served = cx
        .scope_with(|s| {
            let server = s.may_overlap("origin server", TASK_FRAME, |cx| serve(cx, &clock, &mut listener, &folder, lies));
            let fetched = fetch(&mut by_http, &mut HttpOrigin::new(s.cx(), &at), &mut now, &keys, global.handle());
            (fetched, server.join(s.cx()))
        })
        .expect("the scope");
    let _ = listener.close(&cx);
    served.0.expect("a batch");
    let requests = served.1.expect("the server");
    std::eprintln!("http origin: {requests} ranged GETs served (lies: {lies})");
    (folder_found, read(&by_http), truth)
}

#[test]
fn the_same_records_come_byte_equal_through_the_folder_and_over_http() {
    let (folder, http, truth) = both(false);
    let found = http.iter().filter(|it| it.is_some()).count();
    std::eprintln!("folder and http: {found} of {} records found, byte-equal: {}", truth.len(), folder == http);
    assert_eq!(found, truth.len(), "every record is found over http");
    assert_eq!(folder, http, "the folder and http give equal bytes");
    assert!(http.iter().zip(truth.values()).all(|(got, want)| got.as_deref() == Some(want.as_slice())));
}

#[test]
fn a_lying_server_is_refused_by_the_digest_chain() {
    let (_, http, truth) = both(true);
    let found = http.iter().filter(|it| it.is_some()).count();
    let wrong = http.iter().zip(truth.values()).filter(|(got, want)| got.as_deref().is_some_and(|got| got != want.as_slice())).count();
    std::eprintln!("lying server: {found} of {} records taken, {wrong} wrong", truth.len());
    assert_eq!(wrong, 0, "nothing a liar sent is taken");
    assert!(found < truth.len(), "the flipped payloads are refused");
}
