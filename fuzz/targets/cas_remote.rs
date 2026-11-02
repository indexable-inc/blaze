//! The remote tier's protocol (`crates/blaze-cas/src/remote.rs`): the answer frames a
//! `Client` reads off a connection and hands `Store::complete`, which verifies a value before
//! keeping it; the asks a `Server` reads off a connection and answers from its store; and
//! `Answer::decode`. Both sides are driven here as `blaze_core`'s driver would drive them, the
//! connection's bytes coming from the input in pieces of the size it gives.
//!
//! The input's first byte, mod 3, says what the rest is. 0: how many keys are fetched, the
//! piece size, then the bytes the server sends back. 1: the piece size, then the bytes a
//! client sends a server whose store holds a few values. 2: the piece size, values
//! (`cas_shared::values`) a server's store holds, their answer frames and one for a key it
//! lacks, then damage to those frames on the way.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - nothing panics or hangs, and every fetch is answered once, by its tag;
//! - no allocation of a client's, a server's or the store's step is larger than
//!   `cas_shared::wire_bound` of the bytes received: a length prefix is not room made for it;
//! - an answer that decodes encodes back to the same bytes;
//! - a fetch ends as the first answer of its key says, answers coming in any order and filter
//!   frames among them: `Kept` only for a whole answer whose bytes hash to the content it names,
//!   and then the store reads those bytes; `Missing` only for a whole answer that says so;
//!   `Refused` for the rest, and for every fetch still waiting at a frame that is neither a
//!   whole filter nor a whole answer of a key still waiting; damage is never other bytes;
//! - a server sends whole frames: first a full filter its store's every key is in, then answers
//!   that decode, answering each whole ask in order as `Answer::of` its store does (its values
//!   are all short, so none is held back for a send of its own), all of them while every ask is
//!   one; it finds nothing corrupt;
//! - round trip: frames a server's store sends, undamaged, are kept as the values it holds.

#![forbid(unsafe_code)]

use std::{sync::OnceLock, time::Duration};

use blaze_cas::{
    Fetched, Key, Kind,
    remote::{Answer, Client, LONGEST_VALUE, Server},
};
use blaze_core::{Address, Completion, Conn, Done, Effect, FileName, Tag};
use blaze_fuzzer::Target;
use blaze_ids::{ContentId, fuse::Fuse8};

#[path = "cas_shared.rs"]
mod shared;

use shared::{Input, Value};

const TARGET: Target = Target {
    name: "cas_remote",
    check,
    seeds,
    dictionary: &[
        b"BLZASK\x03\x00",
        b"BLZANS\x03\x00",
        b"BLZANS\x03\x00\x02\x01",
        b"\x00\x00\x00\x40\x00\x00\x00\x00",
        b"\x52\x00\x00\x40\x00\x00\x00\x00",
        b"\x29\x00\x00\x00\x00\x00\x00\x00",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

/// The store's tags are `0..2^32` (`cas_shared::config`); the client's and server's follow.
const CLIENT_TAGS: u64 = 1 << 32;
const SERVER_TAGS: u64 = 2 << 32;
/// An answer's bytes before its value: header, which, key, kind, content hash, length.
const VALUE_HEADER: u64 = 8 + 1 + 32 + 1 + 32 + 8;
const ASK_LEN: usize = 8 + 32;
const ASK_HEADER: &[u8; 8] = b"BLZASK\x03\x00";
const ANSWER_HEADER: &[u8; 8] = b"BLZANS\x03\x00";
/// The byte after an answer frame's header that says it is a filter.
const FILTER: u8 = 2;
const FETCHES_MOST: u8 = 8;
const RECEIVES_MOST: usize = 4096;

fn check(data: &[u8]) {
    if let Ok(answer) = Answer::decode(data) {
        assert!(answer.encode() == data, "an answer that decodes encodes back to the same bytes");
    }
    let mut input = Input::new(data);
    match input.u8() % 3 {
        0 => {
            let keys: Vec<Key> = (0..=u32::from(input.u8() % FETCHES_MOST)).map(shared::key).collect();
            let piece = piece(&mut input);
            fetch_all(&keys, input.rest(), piece);
        }
        1 => {
            let piece = piece(&mut input);
            serve(input.rest(), piece);
        }
        _ => {
            let piece = piece(&mut input);
            let (values, settles) = shared::values(&mut input);
            let (keys, frames) = frames_of(&values, settles);
            let mut stream = frames.clone();
            shared::damage(&mut stream, &mut input, true);
            let fetched = fetch_all(&keys, &stream, piece);
            if stream == frames {
                for (value, (key, fetched)) in values.iter().zip(&fetched) {
                    assert!(
                        *key == value.key && matches!(fetched, Fetched::Kept(_)),
                        "round trip: a value the server holds is kept, not {fetched:?}"
                    );
                }
                assert!(
                    matches!(fetched.last(), Some((_, Fetched::Missing))),
                    "round trip: a key the server lacks is missing, not {:?}",
                    fetched.last()
                );
            }
        }
    }
}

/// The most bytes one receive gives: the input's, all it asks for when 0.
fn piece(input: &mut Input<'_>) -> usize {
    match input.u16() {
        0 => usize::MAX,
        it => usize::from(it),
    }
}

fn address() -> Address {
    Address::Unix(FileName::new("remote.sock").expect("a socket's name"))
}

/// Measures one step's largest allocation against what `received` bytes allow.
fn bounded<T>(received: usize, what: &str, step: impl FnOnce() -> T) -> T {
    let most = shared::wire_bound(received);
    let (largest, out) = blaze_fuzz::largest_allocation(step);
    assert!(
        largest <= most,
        "bounded: {what} asked for {largest} bytes at once with {received} bytes received, \
         more than the {most} those bytes imply"
    );
    out
}

/// How a fetch must end, by its frame.
#[derive(Debug)]
enum Expect {
    Kept(Vec<u8>),
    Missing,
    Refused,
}

/// How each fetch of `keys` (all different) ends when the server sends `stream`: its frames
/// read in order, a filter taken, an answer matched to the fetch of its key; the fetches still
/// waiting at the first frame that is neither (or at the end) are refused.
fn expected(keys: &[Key], stream: &[u8]) -> Vec<Expect> {
    let mut ends: Vec<Option<Expect>> = keys.iter().map(|_| None).collect();
    let mut at = 0;
    while let Some(body) = frame_at(stream, at) {
        at += 8 + body.len();
        if body.get(8) == Some(&FILTER) {
            match filter_in(body) {
                Some(_) => continue,
                None => break,
            }
        }
        let (key, expect) = match Answer::decode(body) {
            Ok(Answer::Missing { key }) => (key, Expect::Missing),
            Ok(Answer::Value { key, content, bytes, .. }) => match shared::content_of(&bytes) == content {
                true => (key, Expect::Kept(bytes)),
                false => (key, Expect::Refused),
            },
            Err(_) => break,
        };
        let Some(waiting) = keys.iter().position(|it| *it == key).filter(|&it| ends[it].is_none()) else { break };
        ends[waiting] = Some(expect);
    }
    ends.into_iter().map(|it| it.unwrap_or(Expect::Refused)).collect()
}

/// A filter frame's body, as a client takes it: whether it replaces the filters before it,
/// and the filter; `None` for one it refuses.
fn filter_in(body: &[u8]) -> Option<(bool, Fuse8)> {
    let rest = body.strip_prefix(ANSWER_HEADER)?;
    let (&[which, replaces], rest) = rest.split_first_chunk::<2>()?;
    if which != FILTER || replaces > 1 {
        return None;
    }
    let (_epoch, rest) = rest.split_first_chunk::<8>()?;
    Some((replaces == 1, Fuse8::decode(rest).ok()?))
}

/// What a filter holds of `key`: its first eight bytes, as a `u64`.
fn word(key: &Key) -> u64 {
    u64::from_le_bytes(*key.0.0.first_chunk().expect("a key is 32 bytes"))
}

/// The body of the frame at `at`, when it is all there and not longer than an answer may be.
fn frame_at(stream: &[u8], at: usize) -> Option<&[u8]> {
    let prefix = stream.get(at..at.checked_add(8)?)?;
    let len = u64::from_le_bytes(prefix.try_into().expect("8 bytes"));
    if len > LONGEST_VALUE + VALUE_HEADER {
        return None;
    }
    stream.get(at + 8..(at + 8).checked_add(usize::try_from(len).ok()?)?)
}

/// Fetches `keys` into an empty store through a client whose connection receives `stream` in
/// pieces of at most `piece` bytes (and at most [`RECEIVES_MOST`] of them, so a long stream in
/// small pieces is not a hang), checking each fetch ends as its frame says; how each ended.
fn fetch_all(keys: &[Key], stream: &[u8], piece: usize) -> Vec<(Key, Fetched)> {
    let piece = piece.max(stream.len().div_ceil(RECEIVES_MOST));
    let mut sim = shared::sim();
    let mut store = shared::open(&mut sim);
    let mut client = Client::new(address(), CLIENT_TAGS);
    let mut effects = Vec::new();
    let tags: Vec<Tag> = keys
        .iter()
        .map(|key| store.fetch(key, &mut client, &mut effects).expect("an empty store fetches a key it lacks"))
        .collect();
    let (mut at, mut answered, mut theirs) = (0, Vec::new(), Vec::new());
    while !effects.is_empty() {
        for effect in std::mem::take(&mut effects) {
            let tag = effect.tag();
            if !client.owns(tag) {
                theirs.push(effect);
                continue;
            }
            let done = match effect {
                Effect::Connect { .. } => Done::Connected { conn: Conn(1) },
                Effect::Send { mut bytes, .. } => {
                    bytes.clear();
                    Done::Sent(bytes)
                }
                Effect::Recv { most, mut into, .. } => {
                    let end = stream.len().min(at + most.min(piece));
                    into.extend_from_slice(&stream[at..end]);
                    at = end;
                    Done::Read(into)
                }
                Effect::Hangup { .. } => continue,
                other => panic!("a client asks to connect, send, receive and hang up, not {other:?}"),
            };
            let mut answers = Vec::new();
            bounded(at, "a client's step", || client.complete(Completion { tag, done }, &mut effects, &mut answers));
            for answer in answers {
                assert!(
                    tags.contains(&answer.tag) && !answered.contains(&answer.tag),
                    "a client answers each fetch once, by its tag: {:?}",
                    answer.tag
                );
                answered.push(answer.tag);
                bounded(at, "the store's take of an answer", || store.complete(answer, &mut effects));
            }
        }
        shared::drive(&mut sim, &mut store, &mut theirs);
    }
    assert_eq!(answered.len(), tags.len(), "every fetch is answered by the time the connection is done");
    let mut fetched = store.fetched();
    fetched.sort_by_key(|(key, _)| keys.iter().position(|it| it == key));
    assert_eq!(
        fetched.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
        keys,
        "the store says how each fetch went, once"
    );
    for ((key, fetched), expect) in fetched.iter().zip(expected(keys, stream)) {
        match (fetched, &expect) {
            (Fetched::Kept(_), Expect::Kept(bytes)) => {
                let entry = store.get(key).expect("a value kept is whole").expect("a value kept is there");
                assert!(entry.bytes == bytes.as_slice(), "a value kept is the bytes sent");
            }
            (Fetched::Missing, Expect::Missing) | (Fetched::Refused(_), Expect::Refused) => {}
            (fetched, expect) => panic!("a fetch of {:x} ended {fetched:?}; its frame says {expect:?}", key.0),
        }
    }
    fetched
}

/// What the servers' store holds in mode 1.
fn served() -> Vec<Value> {
    [(Kind::Query, b"a served query's result".repeat(4)), (Kind::Code, vec![0xc3; 2000]), (Kind::Image, Vec::new())]
        .into_iter()
        .zip(0..)
        .map(|((kind, bytes), n)| Value { key: shared::key(n), kind, bytes })
        .collect()
}

/// The files of a store of [`served`], written once; each run's server opens a store of them.
fn served_files() -> &'static [(FileName, Vec<u8>)] {
    static FILES: OnceLock<Vec<(FileName, Vec<u8>)>> = OnceLock::new();
    FILES.get_or_init(|| {
        let mut sim = shared::write(&served(), 0);
        let files = shared::files(&mut sim);
        let with_bytes = |(file, _): (FileName, u64)| {
            let bytes = sim.contents(&file);
            (file, bytes)
        };
        files.into_iter().map(with_bytes).collect()
    })
}

/// Serves `asks`, received in pieces of at most `piece` bytes, from a store of [`served`],
/// checking what it sends.
fn serve(asks: &[u8], piece: usize) {
    let mut sim = shared::sim();
    for (file, bytes) in served_files() {
        sim.change(file, bytes);
    }
    let mut store = shared::open(&mut sim);
    let mut server = Server::new(address(), SERVER_TAGS).expect("a server listens on a unix socket");
    let mut effects = Vec::new();
    server.refresh(&mut store, &mut effects);
    server.start(&mut effects);
    let (mut at, mut sent, mut corrupt, mut parked) = (0, Vec::new(), Vec::new(), None);
    loop {
        if effects.is_empty() {
            let Some((tag, into)) = parked.take() else { break };
            effects.push(Effect::Recv { conn: Conn(7), most: 0, into, tag });
        }
        for effect in std::mem::take(&mut effects) {
            let tag = effect.tag();
            assert!(server.owns(tag), "a server asks with its own tags");
            let done = match effect {
                Effect::Listen { .. } => Done::Accepted { conn: Conn(7) },
                Effect::Send { mut bytes, .. } => {
                    sent.extend_from_slice(&bytes);
                    bytes.clear();
                    Done::Sent(bytes)
                }
                Effect::Recv { most, into, .. } if at == asks.len() && parked.is_none() && most > 0 => {
                    parked = Some((tag, into));
                    continue;
                }
                Effect::Recv { most, mut into, .. } => {
                    let end = asks.len().min(at + most.min(piece));
                    into.extend_from_slice(&asks[at..end]);
                    at = end;
                    Done::Read(into)
                }
                Effect::Hangup { .. } | Effect::Unlisten { .. } => continue,
                other => panic!("a server asks to listen, send, receive and hang up, not {other:?}"),
            };
            corrupt.extend(bounded(at, "a server's step", || server.complete(Completion { tag, done }, &store, &mut effects)));
        }
    }
    assert!(corrupt.is_empty(), "a server of a whole store finds nothing corrupt: {corrupt:?}");
    let first = frame_at(&sent, 0).expect("a server sends a peer its filter first");
    let Some((true, filter)) = filter_in(first) else { panic!("a server's first frame is a full filter") };
    assert!(
        served().iter().all(|value| filter.contains(word(&value.key))),
        "a server's filter holds every key its store does"
    );
    let mut answers = Vec::new();
    let mut from = 8 + first.len();
    while from < sent.len() {
        let body = frame_at(&sent, from).expect("a server sends whole frames");
        answers.push(Answer::decode(body).expect("a server sends answers that decode"));
        from += 8 + body.len();
    }
    let blocks: Vec<&[u8]> = asks.chunks_exact(ASK_LEN).collect();
    let asked: Vec<Answer> = blocks
        .iter()
        .take_while(|it| it[..8] == ASK_HEADER[..])
        .map(|it| Answer::of(&store, &Key(ContentId(it[8..].try_into().expect("32 bytes")))).expect("a whole store"))
        .collect();
    assert!(
        answers.len() <= asked.len() && answers[..] == asked[..answers.len()],
        "a server answers the whole asks in order as Answer::of its store does"
    );
    if asked.len() == blocks.len() {
        assert_eq!(answers.len(), asked.len(), "a server answers every ask while every ask is one");
    }
}

/// A server's store of `values`, settled `settles` times: the keys a client fetches (each
/// value's, then one it lacks) and the frames the server sends for them.
fn frames_of(values: &[Value], settles: u8) -> (Vec<Key>, Vec<u8>) {
    let mut sim = shared::write(values, settles);
    let store = shared::open(&mut sim);
    let keys: Vec<Key> = values.iter().map(|it| it.key).chain([shared::key(u32::MAX - 1)]).collect();
    let frames = keys.iter().flat_map(|key| frame(&Answer::of(&store, key).expect("a whole store"))).collect();
    (keys, frames)
}

fn frame(answer: &Answer) -> Vec<u8> {
    let body = answer.encode();
    [(body.len() as u64).to_le_bytes().as_slice(), &body].concat()
}

fn value(n: u32, kind: Kind, bytes: &[u8]) -> Answer {
    Answer::Value { key: shared::key(n), kind, content: shared::content_of(bytes), bytes: bytes.to_vec() }
}

/// A filter frame of `keys`, as a server sends one.
fn filter_frame(replaces: bool, keys: &[Key]) -> Vec<u8> {
    let mut body = [&ANSWER_HEADER[..], &[FILTER, u8::from(replaces)], &0u64.to_le_bytes()].concat();
    Fuse8::of(keys.iter().map(word).collect()).encode(&mut body);
    [(body.len() as u64).to_le_bytes().as_slice(), &body].concat()
}

fn ask(key: Key) -> Vec<u8> {
    [&ASK_HEADER[..], &key.0.0].concat()
}

/// Streams of whole, damaged and too-long answers; asks of what a server holds and lacks, and
/// asks then bytes that are not one; values sent whole and damaged.
fn seeds() -> Vec<Vec<u8>> {
    let missing = frame(&Answer::Missing { key: shared::key(0) });
    let one = frame(&value(0, Kind::Query, b"one"));
    let two = frame(&value(1, Kind::Code, b"two"));
    let lying = frame(&Answer::Value {
        key: shared::key(0),
        kind: Kind::Query,
        content: shared::content_of(b"not these"),
        bytes: b"these".to_vec(),
    });
    let full = filter_frame(true, &[shared::key(0), shared::key(1)]);
    let mut neither = filter_frame(false, &[shared::key(0)]);
    neither[17] = 7;
    let client = |fetches: u8, piece: u16, stream: &[u8]| [&[0, fetches - 1][..], &piece.to_le_bytes(), stream].concat();
    let mut out = vec![
        client(1, 0, &missing),
        client(1, 0, &one),
        client(2, 0, &[one.clone(), two.clone()].concat()),
        client(2, 3, &[one.clone(), two.clone()].concat()),
        client(2, 0, &[two.clone(), one.clone()].concat()),
        client(1, 0, &lying),
        client(1, 0, &one[..one.len() - 1]),
        client(1, 0, &[one.clone(), vec![0; 8]].concat()),
        client(1, 0, &(LONGEST_VALUE + VALUE_HEADER + 1).to_le_bytes()),
        client(1, 0, &(1u64 << 30).to_le_bytes()),
        client(1, 0, &[(1u64 << 30).to_le_bytes().as_slice(), &[0; 64]].concat()),
        client(1, 0, b""),
        client(2, 0, &[full.clone(), two.clone(), filter_frame(false, &[shared::key(2)]), one.clone()].concat()),
        client(2, 5, &[full, one.clone(), neither, two.clone()].concat()),
        client(1, 0, &[one.clone(), one.clone()].concat()),
    ];
    let asks: Vec<u8> = [0, 1, 2, u32::MAX - 1].into_iter().flat_map(|n| ask(shared::key(n))).collect();
    out.push([&[1, 0, 0][..], &asks].concat());
    out.push([&[1, 7, 0][..], &asks].concat());
    out.push([&[1, 0, 0][..], &asks, b"not an ask at all, forty bytes of it...."].concat());
    out.push([&[1, 0, 0][..], &asks[..ASK_LEN + 5]].concat());
    for values in shared::sample_inputs() {
        out.push([&[2, 0, 0][..], &values].concat());
        out.push([&[2, 0, 0x10][..], &values, &[0, 0x30, 0, 0, 0, 0x01]].concat());
        out.push([&[2, 0, 0][..], &values, &[1, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]].concat());
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
