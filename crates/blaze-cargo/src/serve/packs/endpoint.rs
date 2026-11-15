//! The read-only endpoint peers read this daemon's store through (docs/remote-cache.md,
//! "Packs", "Peers"): `blaze_pack::peer`'s rules as a state machine on the daemon's own
//! driver. Lane packs-1112.
//!
//! Off unless `<root>/packs.json` has `serve`. It owns no socket, thread or clock: it pushes
//! [`blaze_core::Effect`]s (`Listen` on the one TCP address the user wrote, `Recv`, `Send`,
//! `Hangup`, a `Timer` per connection) under tags of its own and takes their completions
//! from the daemon's loop ([`Endpoint::complete`]), which lends it the store to read. So no
//! thread waits in the kernel for it (AGENTS.md rule 44: its waits are the driver's, each an
//! effect in flight with a deadline of its own), and it runs under the simulation as the
//! daemon does.
//!
//! Bounds: at most [`CONNS_MOST`] connections at once (one more is hung up on); a request's
//! head is at most `blaze_pack::peer::HEAD_MOST` bytes; a connection has [`WITHIN`] from its
//! accept to its answer's last byte, then it is hung up; an answer is at most
//! `blaze_pack::peer::RANGE_MOST` bytes of a record, or the statements of one `has`. A
//! request not signed by a listed reader is answered 403 before the store is read.
//!
//! What it costs the loop: a `has` is an index look and one signature a key it has; a record
//! is one verified read (`Store::get`) and a copy of at most 4 MiB. A long record read by
//! ranges is verified whole for each range: measured nowhere yet, and the place to look if
//! a peer's reads show in the loop's turns.

use std::time::Duration;

use blaze_cas::Store;
use blaze_core::{Address, Completion, Conn, Done, Effect, Tag, Tags};
use blaze_pack::{
    Record,
    peer::{self, Asked, Statement},
};

use super::{Config, Ed25519, admitted, id_of, key_of, sign};

/// How long a connection has from its accept to its answer's end.
const WITHIN: Duration = Duration::from_secs(10);
/// Connections served at once.
const CONNS_MOST: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Recv,
    Send,
    /// The connection's deadline.
    Timer,
    Hangup,
}

pub(crate) struct Endpoint {
    rules: peer::Endpoint,
    /// This machine's Ed25519 secret, which signs what it states, and its public half.
    seed: [u8; 32],
    signer: [u8; 32],
    tags: Tags,
    listen: Option<Tag>,
    /// The connections being served: a few, searched in place.
    conns: Vec<Conn>,
    ops: Vec<(Tag, Conn, Op)>,
    /// Why it stopped listening, for the daemon to say.
    failed: Option<String>,
}

impl Endpoint {
    /// The endpoint the settings ask for, listening from the effects pushed here: none when
    /// they ask for none. One asked for that the settings do not allow, or whose key cannot
    /// be read, is the error.
    pub(crate) fn asked(tags: u64, effects: &mut Vec<Effect>) -> Result<Option<Endpoint>, String> {
        let Some(config) = Config::read()? else { return Ok(None) };
        let Some(serve) = &config.serve else { return Ok(None) };
        let seed = config.seed()?.ok_or_else(|| {
            "`serve` needs `key_file`: the file holding this machine's Ed25519 secret, which signs what it states".to_owned()
        })?;
        Endpoint::new(&serve.bind, serve.readers.clone(), seed, tags, effects).map(Some) // clone: once a daemon
    }

    pub(crate) fn new(
        bind: &str,
        readers: Vec<[u8; 32]>,
        seed: [u8; 32],
        tags: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<Endpoint, String> {
        let rules = peer::Endpoint::new(bind, readers).map_err(|refused| refused.to_string())?;
        // The key is checked before anything listens: a statement is signed on every ask.
        let (signer, _) = sign(&seed, b"")?;
        let mut tags = Tags::new(tags);
        let listen = tags.next();
        effects.push(Effect::Listen { at: Address::Tcp(rules.bind()), tag: listen });
        Ok(Endpoint {
            rules,
            seed,
            signer,
            tags,
            listen: Some(listen),
            conns: Vec::new(), // alloc: empty, allocates nothing
            ops: Vec::new(),
            failed: None,
        })
    }

    pub(crate) fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Why it no longer listens, taken once.
    pub(crate) fn take_failed(&mut self) -> Option<String> {
        self.failed.take()
    }

    fn ask(&mut self, conn: Conn, op: Op) -> Tag {
        let tag = self.tags.next();
        self.ops.push((tag, conn, op));
        tag
    }

    fn hang_up(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        if let Some(at) = self.conns.iter().position(|it| *it == conn) {
            self.conns.swap_remove(at);
            // What is still in flight on it completes into nothing.
            self.ops.retain(|(_, of, _)| *of != conn);
            let tag = self.ask(conn, Op::Hangup);
            effects.push(Effect::Hangup { conn, tag });
        }
    }

    /// Takes a completion of its own, answering from `store`.
    pub(crate) fn complete(&mut self, completion: Completion, store: &Store, effects: &mut Vec<Effect>) {
        let Completion { tag, done } = completion;
        if self.listen == Some(tag) {
            match done {
                Done::Accepted { conn } if self.conns.len() >= CONNS_MOST => {
                    let tag = self.tags.next();
                    effects.push(Effect::Hangup { conn, tag });
                }
                Done::Accepted { conn } => {
                    self.conns.push(conn);
                    let tag = self.ask(conn, Op::Recv);
                    // alloc: a request's head, once a connection
                    effects.push(Effect::Recv { conn, most: peer::HEAD_MOST + 1, into: Vec::with_capacity(1024), tag });
                    let tag = self.ask(conn, Op::Timer);
                    effects.push(Effect::Timer { after: WITHIN, tag });
                }
                other => {
                    self.listen = None;
                    // alloc: error path
                    self.failed = Some(format!("serving the store on {} ended: {other:?}", self.rules.bind()));
                }
            }
            return;
        }
        let Some(at) = self.ops.iter().position(|(it, _, _)| *it == tag) else { return };
        let (_, conn, op) = self.ops.swap_remove(at);
        match (op, done) {
            (Op::Recv, Done::Read(head)) if !head.is_empty() => {
                let whole = head.windows(4).position(|it| it == b"\r\n\r\n").map(|end| end + 4);
                match whole {
                    None if head.len() <= peer::HEAD_MOST => {
                        let most = peer::HEAD_MOST + 1 - head.len();
                        let tag = self.ask(conn, Op::Recv);
                        effects.push(Effect::Recv { conn, most, into: head, tag });
                    }
                    // Whole, or longer than a head may be (which `answer` says).
                    _ => {
                        let _span = tracing::info_span!("pack peer request").entered();
                        let bytes = self.answer(&head[..whole.unwrap_or(head.len())], store);
                        let tag = self.ask(conn, Op::Send);
                        effects.push(Effect::Send { conn, bytes, tag });
                    }
                }
            }
            // Answered, hung up on by the caller, out of time, or broken: the connection ends.
            (Op::Recv | Op::Send | Op::Timer, _) => self.hang_up(conn, effects),
            (Op::Hangup, _) => {}
        }
    }

    /// The whole response to the request whose head is `head`.
    fn answer(&self, head: &[u8], store: &Store) -> Vec<u8> {
        let respond = |status: u16, part: Option<(u64, u64, u64)>, body: &[u8]| {
            let mut out = peer::response_head(status, body.len() as u64, part);
            out.extend_from_slice(body);
            out
        };
        let request = match peer::asked(head) {
            Ok(request) => request,
            Err((status, why)) => return respond(status, None, why.as_bytes()),
        };
        if !self.rules.lets(&request, &Ed25519) {
            return respond(403, None, b"this request is not signed by a key that may read this store\n");
        }
        // Never a pin or the daemon's image: neither is a record a build asks a peer for.
        let travels = admitted(true);
        match request.asked {
            Asked::Has { keys } => {
                let mut each = Vec::with_capacity(keys.len()); // alloc: once a request, at most `HAS_MOST`
                for key in &keys {
                    // A value that does not read is not stated: the asker builds it.
                    let Ok(Some(entry)) = store.get(&key_of(key)) else { continue };
                    if !travels.contains(&entry.kind.code()) {
                        continue;
                    }
                    let record = Record {
                        key: id_of(&entry.key),
                        kind: entry.kind.code(),
                        len: entry.bytes.len() as u64,
                        content: blaze_pack::Id(entry.content.0),
                    };
                    let Ok((_, signature)) = sign(&self.seed, &Statement::message(&record, &self.signer)) else {
                        continue;
                    };
                    each.push(Statement { record, signer: self.signer, signature }.encode());
                }
                respond(200, None, &peer::statements(&each))
            }
            Asked::Record { key, range } => match store.get(&key_of(&key)) {
                Ok(Some(entry)) if travels.contains(&entry.kind.code()) => {
                    let len = entry.bytes.len() as u64;
                    match peer::slice(len, range) {
                        Ok((from, to)) => respond(
                            if range.is_some() { 206 } else { 200 },
                            range.map(|_| (from, to, len)),
                            &entry.bytes[from as usize..to as usize],
                        ),
                        Err((status, why)) => respond(status, None, why.as_bytes()),
                    }
                }
                _ => respond(404, None, b""),
            },
        }
    }
}
