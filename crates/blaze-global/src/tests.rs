//! The client and server in simulation: a machine's store fetching through a [`Client`] from
//! a [`Server`] on a store of its own, or from a server the test plays by hand to send what
//! no server should (damaged, truncated, reordered and oversized frames), and the outside
//! peer [`Sim::dial`] makes, for requests no client sends.

use std::{ffi::OsString, time::Duration};

use blaze_cas::{Config, Fetched, Key, Kind, Store};
use blaze_core::{
    Address, Completion, Conn, Done, Driver as _, Effect, FileName, Tag,
    sim::{Faults, Rng, Sim},
};
use blaze_format::cursor::Reader;
use blaze_ids::ContentId;

use crate::{
    BundleId, BundleSpec, Client, Download, Downloaded, Endpoint, GotBundle, Locked, Manifest, Public, Published,
    Record, Secrets, Server, Signed, Trust, client, http,
    record_key,
    sign::tests::publisher,
    tls::clear::Clears,
    wire::{self, BODY_MOST, CHUNKED_FROM, SendWindow},
};

const MINE: u64 = 1 << 40;
const CACHE: u64 = 2 << 40;
const CLIENT: u64 = 3 << 40;
const SERVER: u64 = 4 << 40;
const RAW: u64 = 5 << 40;

fn at() -> Address {
    Address::Tcp("10.0.0.1:443".parse().unwrap())
}

fn trust() -> Trust {
    Trust::new(vec![publisher(1).key()]).unwrap()
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 global test key 1", &n.to_le_bytes()))
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

fn values() -> Vec<(Key, Kind, Vec<u8>)> {
    let big = noise(3 << 20, 1);
    let mut near = big.clone();
    near[2 << 20] ^= 1;
    vec![
        (key(0), Kind::Query, b"a query's value".to_vec()),
        (key(1), Kind::Code, noise(CHUNKED_FROM - 1, 2)),
        (key(2), Kind::Run, Vec::new()),
        (key(3), Kind::Output, big),
        (key(4), Kind::Output, near),
        (key(5), Kind::Debug, noise(200 << 10, 3)),
    ]
}

fn public(key: Key, kind: Kind) -> Public {
    Public::listed(key, kind)
}

/// The cache's server on a store of its own.
struct Cache {
    store: Store,
    server: Server<Clears>,
}

/// What a server the test plays does with a request: sends `bytes`, then hangs up when
/// `hang_up`.
struct Reply {
    bytes: Vec<u8>,
    hang_up: bool,
}

fn send(bytes: Vec<u8>) -> Reply {
    Reply { bytes, hang_up: false }
}

fn send_and_hang_up(bytes: Vec<u8>) -> Reply {
    Reply { bytes, hang_up: true }
}

/// A server the test plays: `answer` makes the reply to each request, from its path and body.
struct Raw {
    listen: Tag,
    next: u64,
    conns: Vec<(Conn, Vec<u8>, Tag)>,
    accepted: usize,
    requests: Vec<(String, Vec<u8>)>,
    answer: Box<dyn FnMut(&str, &[u8]) -> Reply>,
}

impl Raw {
    fn tag(&mut self) -> Tag {
        self.next += 1;
        Tag(RAW + self.next)
    }

    fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) {
        let Completion { tag, done } = completion;
        if tag == self.listen {
            let Done::Accepted { conn } = done else { panic!("the raw listen ended: {done:?}") };
            self.accepted += 1;
            let recv = self.tag();
            effects.push(Effect::Recv { conn, most: 1 << 24, into: Vec::new(), tag: recv });
            self.conns.push((conn, Vec::new(), recv));
            return;
        }
        let Some(at) = self.conns.iter().position(|(_, _, recv)| *recv == tag) else { return };
        let Done::Read(bytes) = done else { return };
        if bytes.is_empty() {
            self.conns.remove(at);
            return;
        }
        self.conns[at].1.extend_from_slice(&bytes);
        let conn = self.conns[at].0;
        loop {
            let buffer = &self.conns[at].1;
            let Some(head) = http::request_head(buffer, |_| u64::MAX).unwrap() else { break };
            if buffer.len() < head.head + head.body {
                break;
            }
            let body = buffer[head.head..head.head + head.body].to_vec();
            self.conns[at].1.drain(..head.head + head.body);
            self.requests.push((head.path.clone(), body.clone()));
            let reply = (self.answer)(&head.path, &body);
            if !reply.bytes.is_empty() {
                let tag = self.tag();
                effects.push(Effect::Send { conn, bytes: reply.bytes, tag });
            }
            if reply.hang_up {
                let tag = self.tag();
                effects.push(Effect::Hangup { conn, tag });
                self.conns.remove(at);
                return;
            }
        }
        let recv = self.tag();
        self.conns[at].2 = recv;
        effects.push(Effect::Recv { conn, most: 1 << 24, into: Vec::new(), tag: recv });
    }
}

struct World {
    sim: Sim,
    effects: Vec<Effect>,
    mine: Store,
    client: Client<Clears>,
    cache: Option<Cache>,
    raw: Option<Raw>,
    download: Option<Download>,
    fetched: Vec<(Key, Fetched)>,
    published: Vec<Published>,
    bundles: Vec<GotBundle>,
}

impl World {
    fn bare(seed: u64) -> World {
        let mut effects = Vec::new();
        let mine = Store::open(store_config("mine", MINE), &mut effects);
        let endpoint = Endpoint { at: at(), host: "cache.example.com".into() };
        let mut world = World {
            sim: Sim::new(seed, Faults::default()),
            effects,
            mine,
            client: Client::new(endpoint, Clears, trust(), CLIENT),
            cache: None,
            raw: None,
            download: None,
            fetched: Vec::new(),
            published: Vec::new(),
            bundles: Vec::new(),
        };
        world.run();
        assert!(world.mine.ready(), "{:?}", world.mine.broken());
        world
    }

    /// With the cache's server.
    fn new(seed: u64) -> World {
        let mut world = World::bare(seed);
        let store = Store::open(store_config("cache", CACHE), &mut world.effects);
        let mut server = Server::new(at(), Clears, trust(), SERVER);
        server.start(&mut world.effects);
        world.cache = Some(Cache { store, server });
        world.run();
        assert!(world.cache().store.ready());
        world
    }

    /// With a server the test plays.
    fn raw(seed: u64, answer: impl FnMut(&str, &[u8]) -> Reply + 'static) -> World {
        let mut world = World::bare(seed);
        let listen = Tag(RAW);
        world.effects.push(Effect::Listen { at: at(), tag: listen });
        world.raw =
            Some(Raw { listen, next: 0, conns: Vec::new(), accepted: 0, requests: Vec::new(), answer: Box::new(answer) });
        world
    }

    fn cache(&mut self) -> &mut Cache {
        self.cache.as_mut().expect("a world with the cache's server")
    }

    fn requests(&self) -> &[(String, Vec<u8>)] {
        &self.raw.as_ref().expect("a raw server").requests
    }

    /// Puts `bytes` in the cache's store as `publisher` signed it.
    fn seed(&mut self, key: Key, kind: Kind, bytes: &[u8]) -> Signed {
        let signed = publisher(1).sign(Record::of(key, kind, bytes));
        let store = &mut self.cache.as_mut().expect("a world with the cache's server").store;
        store.put(key, kind, bytes.to_vec()).unwrap();
        store.put(record_key(&key), Kind::Query, signed.encode().to_vec()).unwrap();
        store.flush(&mut self.effects);
        self.run();
        signed
    }

    fn run(&mut self) {
        let mut out = Vec::new();
        loop {
            self.sim.submit(&mut self.effects);
            if self.sim.poll(&mut out, Duration::ZERO) == 0 {
                break;
            }
            for completion in out.drain(..) {
                self.dispatch(completion);
            }
            self.gather();
        }
        self.gather();
    }

    fn dispatch(&mut self, completion: Completion) {
        let tag = completion.tag;
        let effects = &mut self.effects;
        if self.client.owns(tag) {
            for answer in self.client.complete(completion, effects) {
                assert!(self.mine.owns(answer.tag), "the client answered a tag not the store's");
                self.mine.complete(answer, effects);
            }
        } else if self.mine.owns(tag) {
            self.mine.complete(completion, effects);
        } else if let Some(cache) = &mut self.cache
            && cache.store.owns(tag)
        {
            if !cache.store.complete(completion, effects).is_empty() {
                cache.server.durable(&cache.store, effects);
            }
        } else if let Some(cache) = &mut self.cache
            && cache.server.owns(tag)
        {
            cache.server.complete(&mut cache.store, completion, effects);
        } else if let Some(raw) = &mut self.raw {
            raw.complete(completion, effects);
        } else {
            panic!("a completion of no one's: {completion:?}");
        }
    }

    fn gather(&mut self) {
        let mut fetched = self.mine.fetched();
        let mut bundles = self.client.bundles();
        if let Some(download) = &mut self.download {
            bundles = download.bundles(bundles, &mut self.mine, &mut self.client, &mut self.effects);
            fetched = download.fetched(fetched);
        }
        self.fetched.extend(fetched);
        self.bundles.extend(bundles);
        self.published.extend(self.client.published());
    }

    fn fetch(&mut self, publics: &[Public]) {
        self.client.fetch_many(&mut self.mine, publics, &mut self.effects);
        self.run();
    }

    fn how(&self, key: &Key) -> &Fetched {
        let found: Vec<&Fetched> = self.fetched.iter().filter(|(it, _)| it == key).map(|(_, how)| how).collect();
        assert_eq!(found.len(), 1, "{key:?} fetched {} times: {:?}", found.len(), self.fetched);
        found[0]
    }

    fn refused(&self, key: &Key) -> &str {
        match self.how(key) {
            Fetched::Refused(why) => why,
            how => panic!("{key:?} was not refused: {how:?}"),
        }
    }

    fn kept(&self, key: &Key) -> Option<Vec<u8>> {
        self.mine.get(key).unwrap().map(|it| it.bytes.to_vec())
    }
}

fn store_config(dir: &str, tags: u64) -> Config {
    Config { dir: FileName::new(dir).unwrap(), layers: Vec::new(), tags }
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    http::write_response(status, body, false, &mut out);
    out
}

enum Item {
    Missing,
    Deferred,
    Found(Signed, Vec<u8>),
}

fn fetch_answer(items: &[(Key, Item)]) -> Vec<u8> {
    let mut window = SendWindow::default();
    let mut out = Vec::new();
    wire::header(wire::FETCH_ANSWER, &mut out);
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (key, item) in items {
        match item {
            Item::Missing => out.push(wire::MISSING),
            Item::Deferred => out.push(wire::DEFERRED),
            Item::Found(..) => out.push(wire::FOUND),
        }
        out.extend_from_slice(&key.0.0);
        if let Item::Found(signed, bytes) = item {
            out.extend_from_slice(&signed.encode());
            window.write(bytes, &mut out);
        }
    }
    out
}

fn signed_by(n: u8, key: Key, kind: Kind, bytes: &[u8]) -> Signed {
    publisher(n).sign(Record::of(key, kind, bytes))
}

#[test]
fn values_fetched_from_the_cache_are_kept_as_they_were_published() {
    for seed in 0..4 {
        let mut world = World::new(seed);
        for (key, kind, bytes) in values() {
            world.seed(key, kind, &bytes);
        }
        let mut asked: Vec<Public> = values().iter().map(|(key, kind, _)| public(*key, *kind)).collect();
        asked.push(public(key(99), Kind::Query));
        world.fetch(&asked);
        for (key, _, bytes) in values() {
            assert!(matches!(world.how(&key), Fetched::Kept(_)), "seed {seed}: {:?}", world.how(&key));
            assert_eq!(world.kept(&key), Some(bytes), "seed {seed}");
        }
        assert_eq!(world.how(&key(99)), &Fetched::Missing);
        world.fetched.clear();
        world.fetch(&asked);
        assert_eq!(world.fetched, vec![(key(99), Fetched::Missing)], "seed {seed}: what is kept is not asked again");
    }
}

#[test]
fn fetches_are_batched_and_pipelined() {
    let mut world = World::raw(10, |_, body| {
        let keys = wire::parse_fetch_ask(body).unwrap();
        let items: Vec<(Key, Item)> = keys.into_iter().map(|key| (key, Item::Missing)).collect();
        send(response(200, &fetch_answer(&items)))
    });
    let asked: Vec<Public> = (0..1000).map(|n| public(key(n), Kind::Query)).collect();
    world.fetch(&asked);
    let requests = world.requests().len();
    assert_eq!(requests, 1000usize.div_ceil(client::BATCH_KEYS), "one request a batch");
    for n in 0..1000 {
        assert_eq!(world.how(&key(n)), &Fetched::Missing);
    }
}

#[test]
fn a_large_fetch_opens_its_connections_together_and_spreads_over_them() {
    let mut world = World::raw(18, |_, body| {
        let keys = wire::parse_fetch_ask(body).unwrap();
        let items: Vec<(Key, Item)> = keys.into_iter().map(|key| (key, Item::Missing)).collect();
        send(response(200, &fetch_answer(&items)))
    });
    let count = (client::BATCH_KEYS * client::PIPELINE * 3) as u64;
    let asked: Vec<Public> = (0..count).map(|n| public(key(n), Kind::Query)).collect();
    world.client.fetch_many(&mut world.mine, &asked, &mut world.effects);
    let connects = world.effects.iter().filter(|it| matches!(it, Effect::Connect { .. })).count();
    assert_eq!(connects, 3, "as many connections as its batches fill, asked for at once");
    world.run();
    assert_eq!(world.raw.as_ref().unwrap().accepted, 3);
    assert_eq!(world.requests().len(), 12, "one request a batch");
    for n in 0..count {
        assert_eq!(world.how(&key(n)), &Fetched::Missing);
    }
}

#[test]
fn keys_a_full_response_defers_are_asked_again() {
    let bytes = b"the only value".to_vec();
    let signed = signed_by(1, key(0), Kind::Code, &bytes);
    let mut world = World::raw(11, move |_, body| {
        let keys = wire::parse_fetch_ask(body).unwrap();
        let items: Vec<(Key, Item)> = keys
            .iter()
            .enumerate()
            .map(|(at, key)| match at {
                0 if *key == signed.record.key => (*key, Item::Found(signed, bytes.clone())),
                0 => (*key, Item::Missing),
                _ => (*key, Item::Deferred),
            })
            .collect();
        send(response(200, &fetch_answer(&items)))
    });
    let asked: Vec<Public> = (0..3).map(|n| public(key(n), if n == 0 { Kind::Code } else { Kind::Query })).collect();
    world.fetch(&asked);
    assert_eq!(world.kept(&key(0)).as_deref(), Some(&b"the only value"[..]));
    assert_eq!(world.how(&key(1)), &Fetched::Missing);
    assert_eq!(world.how(&key(2)), &Fetched::Missing);
    let named: Vec<usize> = world.requests().iter().map(|(_, body)| wire::parse_fetch_ask(body).unwrap().len()).collect();
    assert_eq!(named, vec![3, 2, 1], "each deferred key is asked again, and only those");
}

/// A world whose raw server answers every fetch with `items(keys)`, once: a key refused is
/// not asked of it again.
fn answered(seed: u64, items: impl Fn(&[Key]) -> Vec<(Key, Item)> + 'static) -> World {
    World::raw(seed, move |_, body| send(response(200, &fetch_answer(&items(&wire::parse_fetch_ask(body).unwrap())))))
}

#[test]
fn a_value_signed_by_a_key_not_pinned_is_refused_naming_its_key() {
    let bytes = b"bytes from anyone".to_vec();
    let mut world = answered(12, move |keys| vec![(keys[0], Item::Found(signed_by(2, keys[0], Kind::Code, &bytes), bytes.clone()))]);
    world.fetch(&[public(key(0), Kind::Code)]);
    let why = world.refused(&key(0));
    assert!(why.contains(&format!("{:x}", key(0).0)) && why.contains("does not pin"), "{why}");
    assert_eq!(world.kept(&key(0)), None);
    assert_eq!(world.requests().len(), 1, "a refused value is not asked for again");
}

#[test]
fn a_value_whose_signature_is_damaged_is_refused() {
    let bytes = b"the value".to_vec();
    let mut world = answered(13, move |keys| {
        let mut signed = signed_by(1, keys[0], Kind::Code, &bytes);
        signed.signature[0] ^= 1;
        vec![(keys[0], Item::Found(signed, bytes.clone()))]
    });
    world.fetch(&[public(key(0), Kind::Code)]);
    assert!(world.refused(&key(0)).contains("signature"), "{}", world.refused(&key(0)));
    assert_eq!(world.kept(&key(0)), None);
}

#[test]
fn a_value_that_is_not_the_one_its_signed_record_names_is_refused() {
    let mut world = answered(14, move |keys| {
        let signed = signed_by(1, keys[0], Kind::Code, b"what was published");
        vec![(keys[0], Item::Found(signed, b"what was publishes".to_vec()))]
    });
    world.fetch(&[public(key(0), Kind::Code)]);
    let why = world.refused(&key(0));
    assert!(why.contains("hash to"), "the store hashes it against its record's content: {why}");
    assert_eq!(world.kept(&key(0)), None);
}

#[test]
fn a_signed_record_of_another_key_is_not_taken_for_the_key_asked() {
    let bytes = b"another key's value".to_vec();
    let mut world = answered(15, move |keys| vec![(keys[0], Item::Found(signed_by(1, key(77), Kind::Code, &bytes), bytes.clone()))]);
    world.fetch(&[public(key(0), Kind::Code)]);
    assert!(world.refused(&key(0)).contains("a record of"), "{}", world.refused(&key(0)));
}

/// Each damaged response a raw server sends: every key it answers is kept with exactly its
/// value or refused, at least one is refused saying what is wrong and naming its key, and the
/// connection is not asked anything again.
#[test]
fn damaged_truncated_reordered_and_oversized_frames_are_refused() {
    type Damage = fn(&[Key], Vec<u8>) -> Reply;
    fn value(key: &Key) -> Vec<u8> {
        noise(100, key.0.0[0].into())
    }
    let good = |keys: &[Key]| -> Vec<u8> {
        let items: Vec<(Key, Item)> =
            keys.iter().map(|key| (*key, Item::Found(signed_by(1, *key, Kind::Code, &value(key)), value(key)))).collect();
        fetch_answer(&items)
    };
    let cases: Vec<(&str, Damage, &str)> = vec![
        ("a magic of another body", |_, mut body| { body[0] = b'X'; send(response(200, &body)) }, "magic"),
        ("another version", |_, mut body| { body[6] = 9; send(response(200, &body)) }, "version 9"),
        ("cut short", |_, body| send(response(200, &body[..body.len() - 7])), "truncated"),
        ("answers in another order", |keys, _| {
            let mut reversed = keys.to_vec();
            reversed.reverse();
            let items: Vec<(Key, Item)> = reversed.into_iter().map(|key| (key, Item::Missing)).collect();
            send(response(200, &fetch_answer(&items)))
        }, "in place of"),
        ("fewer answers", |keys, _| {
            let items: Vec<(Key, Item)> = keys[1..].iter().map(|key| (*key, Item::Missing)).collect();
            send(response(200, &fetch_answer(&items)))
        }, "answers for"),
        ("an unknown answer", |_, mut body| { body[12] = 7; send(response(200, &body)) }, "unknown answer 7"),
        ("a body longer than allowed", |_, _| {
            send(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", BODY_MOST + 1).into_bytes())
        }, "more than"),
        ("chunked", |_, _| send(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec()), "chunked"),
        ("not HTTP", |_, _| send(b"SSH-2.0-OpenSSH\r\n\r\n".to_vec()), "HTTP/1.1"),
        ("a hang-up", |_, _| send_and_hang_up(Vec::new()), "hung up"),
        ("a hang-up mid-response", |_, body| {
            let mut bytes = response(200, &body);
            bytes.truncate(bytes.len() / 2);
            send_and_hang_up(bytes)
        }, "hung up"),
        ("an error status", |_, _| send(response(503, b"the cache's store is not open yet")), "503"),
        ("a count of pieces past the body", |_, mut body| {
            let at = 12 + 33 + crate::sign::SIGNED_LEN;
            body[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            send(response(200, &body))
        }, "truncated"),
        ("a piece longer than allowed", |_, mut body| {
            let at = 12 + 33 + crate::sign::SIGNED_LEN + 4 + 1;
            body[at..at + 4].copy_from_slice(&((wire::PIECE_MOST + 1) as u32).to_le_bytes());
            send(response(200, &body))
        }, "allowed"),
    ];
    for (n, (what, damage, says)) in cases.into_iter().enumerate() {
        let mut world = World::raw(100 + n as u64, move |_, body| {
            let keys = wire::parse_fetch_ask(body).unwrap();
            damage(&keys, good(&keys))
        });
        let asked: Vec<Public> = (0..3).map(|n| public(key(n), Kind::Code)).collect();
        world.fetch(&asked);
        let mut refused = 0;
        for public in &asked {
            let key = public.key();
            match world.how(&key) {
                Fetched::Refused(why) => {
                    refused += 1;
                    assert!(why.contains(says), "{what}: {why:?} does not say {says:?}");
                    assert!(why.contains(&format!("{:x}", key.0)), "{what}: {why:?} does not name the key");
                    assert_eq!(world.kept(&key), None, "{what}");
                }
                Fetched::Kept(_) => assert_eq!(world.kept(&key), Some(value(&key)), "{what}"),
                Fetched::Missing => panic!("{what}: {key:?} answered missing"),
            }
        }
        assert!(refused > 0, "{what}: nothing was refused");
        assert_eq!(world.requests().len(), 1, "{what}: nothing is asked again");
    }
}

/// A response with a bit flipped anywhere in its body, or cut anywhere: each key is kept with
/// exactly its value, or not kept; never kept with other bytes.
#[test]
fn no_damage_makes_a_value_kept_with_other_bytes() {
    let values: Vec<(Key, Vec<u8>)> = (0..4).map(|n| (key(n), noise(if n == 3 { 3 * CHUNKED_FROM } else { 50 }, n))).collect();
    let good = {
        let values = values.clone();
        let items: Vec<(Key, Item)> = values
            .iter()
            .map(|(key, bytes)| (*key, Item::Found(signed_by(1, *key, Kind::Code, bytes), bytes.clone())))
            .collect();
        fetch_answer(&items)
    };
    let mut rng = Rng::new(7);
    for round in 0..60 {
        let mut body = good.clone();
        let cut = round % 3 == 2;
        if cut {
            body.truncate(rng.below(body.len() as u64) as usize);
        } else {
            let at = rng.below(body.len() as u64) as usize;
            body[at] ^= 1 << rng.below(8);
        }
        let mut world = World::raw(200 + round as u64, move |_, _| send(response(200, &body)));
        let asked: Vec<Public> = values.iter().map(|(key, _)| public(*key, Kind::Code)).collect();
        world.fetch(&asked);
        for (key, bytes) in &values {
            match world.how(key) {
                Fetched::Kept(_) => assert_eq!(world.kept(key).as_ref(), Some(bytes), "round {round}"),
                Fetched::Refused(_) | Fetched::Missing => assert_eq!(world.kept(key), None, "round {round}"),
            }
        }
    }
}

#[test]
fn a_cache_that_cannot_be_reached_refuses_what_was_asked() {
    let mut world = World::bare(16);
    world.fetch(&[public(key(0), Kind::Code)]);
    assert!(world.refused(&key(0)).contains("could not connect"), "{}", world.refused(&key(0)));
}

#[test]
#[should_panic(expected = "not shown public")]
fn the_client_is_asked_for_no_key_not_shown_public() {
    let mut world = World::bare(17);
    world.mine.fetch(&key(0), &mut world.client, &mut world.effects);
}

#[test]
fn what_the_builders_publish_any_machine_fetches() {
    let mut world = World::new(18);
    let mut secrets = Secrets::new();
    secrets.publisher(&publisher(1));
    let mut tags = Vec::new();
    for (key, kind, bytes) in values() {
        tags.push(world.client.publish(&public(key, kind), bytes, &publisher(1), &secrets, &mut world.effects).unwrap());
    }
    world.run();
    let mut answered: Vec<Tag> = world.published.iter().map(|it| it.tag).collect();
    answered.sort_unstable();
    tags.sort_unstable();
    assert_eq!(answered, tags, "each publish answered once");
    assert!(world.published.iter().all(|it| it.result.is_ok()), "{:?}", world.published);
    let asked: Vec<Public> = values().iter().map(|(key, kind, _)| public(*key, *kind)).collect();
    world.fetch(&asked);
    for (key, _, bytes) in values() {
        assert_eq!(world.kept(&key), Some(bytes));
    }
}

#[test]
fn the_cache_refuses_a_publish_no_pinned_key_signed_and_keeps_one_value_a_key() {
    let mut world = World::new(19);
    let secrets = Secrets::new();
    let value = b"a value".to_vec();
    world.client.publish(&public(key(0), Kind::Code), value.clone(), &publisher(2), &secrets, &mut world.effects).unwrap();
    world.client.publish(&public(key(1), Kind::Code), value.clone(), &publisher(1), &secrets, &mut world.effects).unwrap();
    world.run();
    world.client.publish(&public(key(1), Kind::Code), b"another".to_vec(), &publisher(1), &secrets, &mut world.effects).unwrap();
    world.run();
    let result = |key: Key, n: usize| world.published.iter().filter(|it| it.key == key).nth(n).unwrap().result.clone();
    assert!(result(key(0), 0).unwrap_err().contains("does not pin"));
    assert_eq!(result(key(1), 0), Ok(()));
    assert!(result(key(1), 1).unwrap_err().contains("another value"));
    assert!(!world.cache().store.has(&key(0)));
    assert_eq!(world.cache().store.get(&key(1)).unwrap().unwrap().bytes, value);
}

#[test]
fn a_value_holding_a_secret_is_never_published() {
    let mut world = World::raw(20, |_, _| panic!("nothing is sent"));
    let token = "ghp_0123456789abcdefghij";
    let mut secrets = Secrets::new();
    secrets.publisher(&publisher(1));
    secrets.env([(OsString::from("GITHUB_TOKEN"), OsString::from(token))], &[]);
    let seed_hex = crate::sign::hex(&[1; 32]);
    let holding: Vec<Vec<u8>> = vec![
        format!("token={token}\n").into_bytes(),
        [b"seed:".as_slice(), &[1; 32]].concat(),
        format!("export SEED={seed_hex}").into_bytes(),
    ];
    for (n, bytes) in holding.into_iter().enumerate() {
        let refused = world
            .client
            .publish(&public(key(n as u64), Kind::Output), bytes, &publisher(1), &secrets, &mut world.effects)
            .unwrap_err();
        assert_eq!(refused.key, key(n as u64));
        assert!(!refused.why.contains(token) && !refused.why.contains(&seed_hex), "{refused}");
        assert!(refused.why.contains("GITHUB_TOKEN") || refused.why.contains("seed"), "{refused}");
    }
    world.run();
    assert!(world.requests().is_empty() && world.published.is_empty());
}

/// Requests no client sends, from the outside peer.
#[test]
fn the_server_refuses_requests_that_are_not_ones() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (b"GET /v1/fetch HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec(), "405"),
        (b"POST /v2/everything HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec(), "404"),
        (format!("POST /v1/fetch HTTP/1.1\r\nContent-Length: {}\r\n\r\n", 1 << 30).into_bytes(), "413"),
        (b"POST /v1/publish HTTP/1.1\r\n\r\n".to_vec(), "411"),
        (b"POST /v1/fetch HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc".to_vec(), "400"),
        (b"POST /v1/publish HTTP/1.1\r\nContent-Length: 4\r\n\r\nBLZG".to_vec(), "400"),
    ];
    for (n, (request, status)) in cases.into_iter().enumerate() {
        let mut world = World::new(300 + n as u64);
        let conn = world.sim.dial(&at()).unwrap();
        world.run();
        world.sim.peer_send(conn, &request);
        world.run();
        let answer = world.sim.peer_received(conn);
        let head = http::response_head(&answer, u64::MAX).unwrap().unwrap();
        assert_eq!(head.status.to_string(), status, "{}", String::from_utf8_lossy(&answer));
        assert!(head.close && world.sim.peer_hung_up_on(conn), "a refused request ends its connection");
    }
}

#[test]
fn the_server_refuses_a_published_value_that_is_not_its_record() {
    let mut world = World::new(21);
    let signed = signed_by(1, key(0), Kind::Code, b"what was signed");
    let mut body = Vec::new();
    wire::header(wire::PUBLISH_ASK, &mut body);
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&signed.encode());
    SendWindow::default().write(b"what was sent!!", &mut body);
    let mut request = Vec::new();
    http::write_request("cache.example.com", client::PUBLISH_PATH, &body, &mut request);
    let conn = world.sim.dial(&at()).unwrap();
    world.run();
    world.sim.peer_send(conn, &request);
    world.run();
    let answer = world.sim.peer_received(conn);
    let head = http::response_head(&answer, u64::MAX).unwrap().unwrap();
    assert_eq!(head.status, 200);
    let statuses = wire::parse_publish_answer(&answer[head.head..], 1).unwrap();
    assert!(statuses[0].1.as_ref().unwrap_err().contains("hash to"), "{statuses:?}");
    assert!(!world.cache().store.has(&key(0)) && !world.cache().store.has(&record_key(&key(0))));
}

fn lock() -> &'static str {
    r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = ["serde"]

[[package]]
name = "serde"
version = "1.0.219"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5f0e2c6ed6606019b4e29e69dbaba95b11854410e5347d525002456dbbb786b6"

[[package]]
name = "libc"
version = "0.2.170"
source = "sparse+https://index.crates.io/"
checksum = "875b3680cb2f8f71bdcf9a30f38d48282f5d3c95cbf9b3fa57269bb5d5c06828"

[[package]]
name = "winapi"
version = "0.3.9"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5c839a674fcd7a98952e593242ea400abe93992746761e38641405d28b00f419"

[[package]]
name = "forked"
version = "1.0.0"
source = "git+https://github.com/someone/forked?rev=abc#abc"

[[package]]
name = "internal"
version = "2.0.0"
source = "registry+https://registry.example.com/index"
checksum = "1111111111111111111111111111111111111111111111111111111111111111"
"#
}

fn built(name: &str, _: &str) -> Option<Vec<String>> {
    match name {
        "winapi" => None,
        "serde" => Some(vec!["std".into(), "derive".into(), "std".into()]),
        _ => Some(Vec::new()),
    }
}

#[test]
fn a_bundle_holds_the_crates_io_packages_a_build_builds() {
    let locked = Locked::parse(lock(), built).unwrap();
    let names: Vec<&str> = locked.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["libc", "serde"], "workspace, git, other registries and packages not built are not in it");
    assert_eq!(locked[1].features, ["derive", "std"]);
    assert_eq!(locked[1].checksum[..2], [0x5f, 0x0e]);
    let no_checksum = lock().replace("checksum = \"875b3680cb2f8f71bdcf9a30f38d48282f5d3c95cbf9b3fa57269bb5d5c06828\"\n", "");
    assert!(Locked::parse(&no_checksum, built).unwrap_err().contains("libc 0.2.170 of crates.io has no checksum"));
    let bad = lock().replace("5f0e2c6e", "5f0e2c6z");
    assert!(Locked::parse(&bad, built).unwrap_err().contains("not 64 hex digits"));
    assert!(Locked::parse("[[package]\n", built).unwrap_err().contains("not TOML"));
}

fn spec() -> BundleSpec {
    BundleSpec {
        packages: Locked::parse(lock(), built).unwrap(),
        toolchain: ContentId([1; 32]),
        target: "x86_64-unknown-linux-gnu".into(),
        settings: ContentId([2; 32]),
    }
}

#[test]
fn a_bundle_is_named_by_everything_its_build_is_a_function_of() {
    let id = BundleId::of(&spec());
    let mut reordered = spec();
    reordered.packages.reverse();
    reordered.packages[0].features.reverse();
    assert_eq!(BundleId::of(&reordered), id);
    let changes: [fn(&mut BundleSpec); 6] = [
        |it| it.toolchain = ContentId([9; 32]),
        |it| it.settings = ContentId([9; 32]),
        |it| it.target = "aarch64-apple-darwin".into(),
        |it| it.packages[0].features.push("extra".into()),
        |it| it.packages[0].checksum[0] ^= 1,
        |it| _ = it.packages.pop(),
    ];
    for change in changes {
        let mut changed = spec();
        change(&mut changed);
        assert_ne!(BundleId::of(&changed), id);
    }
}

fn manifest(entries: &[(Key, Kind, Vec<u8>)]) -> Manifest {
    let records = entries.iter().map(|(key, kind, bytes)| (public(*key, *kind), Record::of(*key, *kind, bytes)));
    Manifest::sign(BundleId::of(&spec()), records, &publisher(1))
}

#[test]
fn a_manifest_round_trips_and_every_part_of_it_is_signed() {
    let manifest = manifest(&values());
    let mut bytes = Vec::new();
    manifest.encode(&mut bytes);
    let decoded = Manifest::decode(&mut Reader::new(&bytes)).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(decoded.verify(&trust()), Ok(()));
    assert!(decoded.entries.windows(2).all(|pair| pair[0].key < pair[1].key));
    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 1;
        if let Ok(it) = Manifest::decode(&mut Reader::new(&damaged)) {
            assert!(it.verify(&trust()).is_err(), "byte {at}");
        }
    }
    let head = bytes.len() - 4 - crate::sign::RECORD_LEN * manifest.entries.len();
    let mut huge = bytes[..head].to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(Manifest::decode(&mut Reader::new(&huge)).unwrap_err().contains("more than"));
    let mut many = bytes[..head].to_vec();
    many.extend_from_slice(&1000u32.to_le_bytes());
    assert!(Manifest::decode(&mut Reader::new(&many)).unwrap_err().contains("truncated"));
    let other = Manifest::sign(manifest.id, Vec::new(), &publisher(2));
    assert!(other.verify(&trust()).unwrap_err().contains("does not pin"));
}

#[test]
fn a_cold_build_is_one_download() {
    let mut world = World::new(22);
    let secrets = Secrets::new();
    for (key, kind, bytes) in values() {
        world.client.publish(&public(key, kind), bytes, &publisher(1), &secrets, &mut world.effects).unwrap();
    }
    world.run();
    let manifest = manifest(&values());
    let id = manifest.id;
    world.client.publish_bundle(manifest.clone(), &mut world.effects);
    world.run();
    assert!(world.published.iter().all(|it| it.result.is_ok()), "{:?}", world.published);
    assert!(world.published.iter().any(|it| it.key == Key(id.0)));

    world.mine.put(key(0), Kind::Query, values()[0].2.clone()).unwrap();
    world.download = Some(Download::start(id, &mut world.client, &mut world.effects));
    world.run();
    let bytes: u64 = values()[1..].iter().map(|it| it.2.len() as u64).sum();
    match world.download.take().unwrap().done() {
        Some(Ok(Downloaded::Kept { listed, fetched, bytes: got, tickets })) => {
            assert_eq!((*listed, *fetched, *got), (values().len(), values().len() - 1, bytes));
            assert_eq!(tickets.len(), values().len() - 1);
        }
        other => panic!("{other:?}"),
    }
    for (key, _, bytes) in values() {
        assert_eq!(world.kept(&key), Some(bytes));
    }
    assert!(world.fetched.is_empty(), "the download's fetches are its own: {:?}", world.fetched);

    let other = BundleId(ContentId([7; 32]));
    world.download = Some(Download::start(other, &mut world.client, &mut world.effects));
    world.run();
    assert_eq!(world.download.take().unwrap().done(), Some(&Ok(Downloaded::NoBundle)));
}

#[test]
fn the_cache_keeps_no_manifest_listing_what_it_lacks() {
    let mut world = World::new(23);
    let manifest = manifest(&values());
    world.client.publish_bundle(manifest.clone(), &mut world.effects);
    world.run();
    let why = world.published[0].result.clone().unwrap_err();
    assert!(why.contains("no value of"), "{why}");
    assert!(!world.cache().store.has(&manifest.id.manifest_key()));
}

#[test]
fn a_manifest_no_pinned_key_signed_is_refused_and_nothing_is_fetched() {
    let unsigned = Manifest::sign(BundleId::of(&spec()), values().iter().map(|(key, kind, bytes)| (public(*key, *kind), Record::of(*key, *kind, bytes))), &publisher(2));
    let mut world = World::raw(24, move |path, _| {
        assert_eq!(path, client::BUNDLE_PATH, "nothing it lists is fetched");
        let mut body = Vec::new();
        wire::header(wire::BUNDLE_ANSWER, &mut body);
        body.push(wire::FOUND);
        unsigned.encode(&mut body);
        send(response(200, &body))
    });
    world.download = Some(Download::start(BundleId::of(&spec()), &mut world.client, &mut world.effects));
    world.run();
    let done = world.download.take().unwrap();
    let why = done.done().unwrap().clone().unwrap_err();
    assert!(why.contains("does not pin"), "{why}");
    assert_eq!(world.requests().len(), 1);
}

#[test]
fn a_value_a_manifest_lists_is_checked_against_its_listed_record() {
    let served = values();
    let listed = manifest(&served);
    let id = listed.id;
    let mut world = World::raw(25, move |path, body| {
        let mut out = Vec::new();
        if path == client::BUNDLE_PATH {
            wire::header(wire::BUNDLE_ANSWER, &mut out);
            out.push(wire::FOUND);
            listed.encode(&mut out);
        } else {
            let keys = wire::parse_fetch_ask(body).unwrap();
            let items: Vec<(Key, Item)> = keys
                .iter()
                .map(|key| {
                    let (_, kind, bytes) = served.iter().find(|it| it.0 == *key).unwrap();
                    let mut other = bytes.clone();
                    other.push(1);
                    let signed = signed_by(1, *key, *kind, &other);
                    (*key, Item::Found(signed, other))
                })
                .collect();
            out = fetch_answer(&items);
        }
        send(response(200, &out))
    });
    world.download = Some(Download::start(id, &mut world.client, &mut world.effects));
    world.run();
    let why = world.download.take().unwrap().done().unwrap().clone().unwrap_err();
    assert!(why.contains("not the one its bundle's manifest lists"), "{why}");
    assert!(why.contains(&format!("{} of its {} values were not kept", values().len(), values().len())), "{why}");
    for (key, _, _) in values() {
        assert_eq!(world.kept(&key), None);
    }
}
