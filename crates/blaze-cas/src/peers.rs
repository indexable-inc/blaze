//! A store several daemons on one machine write at once: one build's std, kept once for every
//! BLAZE_HOME on the machine (docs/v2-store.md, "A shared store").
//!
//! Each daemon writes a store of its own in the shared directory, `w-<writer>`, exactly as any
//! store is written: one writer a directory, its commits and its crash safety as they are, so
//! two daemons' commits never touch one file and no lock is taken, for a commit or a read. It
//! reads every other writer's directory as a layer behind its own. A get that misses asks
//! [`Store::look_again`]: the shared directory is listed, each other writer's head read again,
//! and a writer whose head changed is mapped afresh and checked whole before readers see it;
//! until then they read it as it was.
//!
//! A put of a key another writer's commit holds keeps nothing: the value is one entry. Two
//! writers that put one key before either has seen the other's commit each keep a copy, which
//! reads the same, since a key's bytes are the same wherever it is made. Every value read from
//! any writer's directory is checked against its hash, as in any store.
//!
//! What another writer does to its own directory while it is read here is safe by the store's
//! own rules: it appends past what its head names, replaces its index tables and head by
//! renaming, and removes a pack only once its head no longer names it, so a mapping made here
//! never sees bytes change. A file gone or replaced between reading its head and mapping it
//! means the head moved on: the head is read again, [`FAILURES_MOST`] times before the store
//! stops, naming the file.

use blaze_core::{Done, Effect, FileName};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;

use crate::{Config, FAILURES_MOST, HEAD_FILE, Head, Op, Store, Tier};

/// Tiers a store can have: a record's place keeps its tier in 8 bits ([`crate::location`]).
const TIERS_MOST: usize = 1 << 8;

/// What a writer's directory in a shared store is named, before its id.
const WRITER_PREFIX: &str = "w-";

/// A store that writes into a shared directory with others ([`Store::open_shared`]).
#[derive(Clone, Debug)]
pub struct SharedConfig {
    /// The shared directory, under the driver's root.
    pub root: FileName,
    /// Which writer this store is: its directory is `root/w-<writer>`. One process at a time
    /// may be a writer (the daemon of one BLAZE_HOME, which is one at a time already), as one
    /// daemon a store's directory.
    pub writer: ContentId,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
}

/// What a store of a shared directory knows of the other writers.
pub(crate) struct Peers {
    root: FileName,
    /// Its own directory.
    own: FileName,
    /// The tier of each other writer's directory.
    tier_of: HashMap<FileName, usize>,
    /// Failed tries of each tier since it last opened.
    tries: HashMap<usize, u32>,
    look: Option<Looking>,
    /// A look asked for while one was in flight, whose listing may have come before what the
    /// asker missed was committed.
    again: bool,
    /// Tiers whose opening failed, whose heads are read again.
    restart: Vec<usize>,
    /// The number of the last look begun, and of the last one in.
    asked: u64,
    done: u64,
}

struct Looking {
    listed: bool,
    /// The tiers whose heads it read that are not yet open as those heads say.
    waiting: HashSet<usize>,
}

impl Peers {
    fn end_look(&mut self) {
        if self.look.as_ref().is_some_and(|look| look.listed && look.waiting.is_empty()) {
            self.look = None;
            self.done = self.asked;
        }
    }
}

/// The directory of `writer` in the shared directory `root`.
fn writer_dir(root: &FileName, writer: &ContentId) -> FileName {
    // alloc: setup, once per store
    FileName::new(&format!("{root}/{WRITER_PREFIX}{writer:x}")).expect("a writer's directory stays in the shared one")
}

impl Store {
    /// A store that is writer `config.writer` of the shared directory `config.root`: what it is
    /// given goes into its own directory there, and what every other writer committed is read
    /// behind it, as of its last look ([`Store::look_again`]). It is ready once it has looked
    /// once. A shared directory, or another writer's store in it, that it cannot read stops it,
    /// naming the file; nothing is read another way.
    pub fn open_shared(config: SharedConfig, effects: &mut Vec<Effect>) -> Store {
        let own = writer_dir(&config.root, &config.writer);
        let peers = Peers {
            root: config.root,
            // clone: setup, once per store
            own: own.clone(),
            tier_of: HashMap::default(),
            tries: HashMap::default(),
            look: None,
            again: false,
            // alloc: empty, allocates nothing
            restart: Vec::new(),
            asked: 0,
            done: 0,
        };
        // alloc: empty, no layers: allocates nothing
        let mut store = Store::new(Config { dir: own, layers: Vec::new(), tags: config.tags }, Some(peers));
        store.ask_private(effects);
        store
    }

    /// Looks again at what the other writers of its shared directory have committed: a get that
    /// missed asks this before it takes the value to be none, and gets again once
    /// [`Store::looked`] reaches the number this returns. Asked while a look is in flight, it is
    /// the next look, which begins when that one ends. A store not opened shared has no other
    /// writer: its look is in already. A store that stopped looks no more ([`Store::broken`]).
    pub fn look_again(&mut self, effects: &mut Vec<Effect>) -> u64 {
        let Some(peers) = &mut self.peers else { return 0 };
        if peers.look.is_some() || peers.done == 0 {
            peers.again = true;
            return peers.asked + 1;
        }
        self.start_look(effects)
    }

    /// The number of the last look in ([`Store::look_again`]).
    pub fn looked(&self) -> u64 {
        self.peers.as_ref().map_or(0, |it| it.done)
    }

    /// Whether tier `at` is another writer's directory of a shared store.
    pub(crate) fn is_peer(&self, at: usize) -> bool {
        at != 0 && self.peers.is_some()
    }

    /// The directory [`Effect::Private`] checks: the shared one, with every writer's in it.
    pub(crate) fn private_dir(&self) -> &FileName {
        self.peers.as_ref().map_or(&self.config.dir, |it| &it.root)
    }

    /// Whether a shared store can answer gets: its own directory open and its first look in.
    pub(crate) fn peers_ready(&self) -> Option<bool> {
        self.peers.as_ref().map(|it| self.tiers[0].opened && it.done > 0)
    }

    fn start_look(&mut self, effects: &mut Vec<Effect>) -> u64 {
        let tag = self.tags.ask(Op::Peers);
        let peers = self.peers.as_mut().expect("a look is of a shared store");
        peers.asked += 1;
        peers.look = Some(Looking { listed: false, waiting: HashSet::default() });
        // clone: once per look, the effect owns the name
        effects.push(Effect::Entries { dir: peers.root.clone(), tag });
        peers.asked
    }

    fn ask_head(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let file = self.tiers[at].file(HEAD_FILE);
        let tag = self.tags.ask(Op::Head { tier: at });
        effects.push(Effect::ReadAll { file, tag });
    }

    /// The head of tier `at` of a shared store, read.
    pub(crate) fn peer_head(&mut self, at: usize, bytes: Vec<u8>, effects: &mut Vec<Effect>) {
        if at == 0 {
            let fresh = bytes.is_empty();
            self.read_head(0, bytes, effects);
            if self.broken.is_some() {
                return;
            }
            if fresh {
                // Its directory holds a head from its opening on, so a listing of the shared
                // directory is never of nothing, and the other writers open it as a store that
                // holds nothing yet.
                let file = self.tiers[0].file(HEAD_FILE);
                let tag = self.tags.ask(Op::Announce);
                effects.push(Effect::Write { file, bytes: Head::default().encode(), tag });
            } else {
                self.start_look(effects);
            }
            return;
        }
        let head = match bytes.is_empty() {
            // A writer whose directory is made and whose head is not written yet holds nothing.
            true => Head::default(),
            false => match Head::decode(&bytes) {
                Ok(head) => head,
                Err(why) => {
                    let file = self.tiers[at].file(HEAD_FILE);
                    return self.fail(file, why);
                }
            },
        };
        let tier = &self.tiers[at];
        if tier.opened && tier.head == head {
            return self.peer_settled(at);
        }
        if tier.opened {
            // clone: once per other writer's head that changed
            let dir = tier.dir.clone();
            let before = std::mem::replace(&mut self.tiers[at], Tier::new(dir));
            // alloc: once per other writer's head that changed, held until it is open afresh
            self.tiers[at].before = Some(Box::new(before));
        }
        self.read_head(at, bytes, effects);
    }

    /// Tier `at` of a shared store is open as its head last said: the look waiting for it has it.
    pub(crate) fn peer_settled(&mut self, at: usize) {
        let peers = self.peers.as_mut().expect("a peer is of a shared store");
        peers.tries.remove(&at);
        if let Some(look) = &mut peers.look {
            look.waiting.remove(&at);
        }
        peers.end_look();
    }

    /// Tier `at` of a shared store could not be opened as its head said: what it was before is
    /// kept for readers, and its head is read again, at most [`FAILURES_MOST`] times running.
    pub(crate) fn peer_failed(&mut self, at: usize, file: FileName, why: String) {
        let peers = self.peers.as_mut().expect("a peer is of a shared store");
        let tries = peers.tries.entry(at).or_default();
        *tries += 1;
        if *tries >= FAILURES_MOST {
            return self.fail(file, why);
        }
        peers.restart.push(at);
        // The mappings of the opening given up are let go of as they come.
        self.tags.ops.retain(|_, op| !matches!(op, Op::Map { tier, .. } if *tier == at));
        let tier = &mut self.tiers[at];
        *tier = match tier.before.take() {
            Some(before) => *before,
            // clone: an error path, a peer opened afresh
            None => Tier::new(tier.dir.clone()),
        };
    }

    /// The listing of the shared directory came: each other writer's head is read.
    pub(crate) fn peers_listed(&mut self, done: Done, effects: &mut Vec<Effect>) {
        let entries = match done {
            Done::Entries(entries) => entries,
            done => {
                // clone: the name of what failed, on an error path
                let root = self.peers.as_ref().expect("a listing is of a shared store").root.clone();
                if self.retry(&root, &done) {
                    let tag = self.tags.ask(Op::Peers);
                    effects.push(Effect::Entries { dir: root, tag });
                }
                return;
            }
        };
        self.failures = 0;
        for (dir, stamp) in entries {
            let peers = self.peers.as_mut().expect("a listing is of a shared store");
            let name = dir.as_str().rsplit('/').next().unwrap_or_default();
            if !stamp.is_dir() || !name.starts_with(WRITER_PREFIX) || dir == peers.own {
                continue;
            }
            let at = match peers.tier_of.get(&dir) {
                Some(&at) => at,
                None if self.tiers.len() >= TIERS_MOST => {
                    // clone: the error that breaks the store
                    let root = peers.root.clone();
                    // alloc: the error that breaks the store
                    return self.fail(root, format!("it holds more than {} writers' stores", TIERS_MOST - 1));
                }
                None => {
                    let at = self.tiers.len();
                    // clone: once per writer found, the map and the tier each own its name
                    peers.tier_of.insert(dir.clone(), at);
                    self.tiers.push(Tier::new(dir));
                    at
                }
            };
            if let Some(look) = &mut peers.look {
                look.waiting.insert(at);
            }
            self.ask_head(at, effects);
        }
        let peers = self.peers.as_mut().expect("a listing is of a shared store");
        if let Some(look) = &mut peers.look {
            look.listed = true;
        }
        peers.end_look();
    }

    /// Its own empty head, written at its first opening, came: it looks.
    pub(crate) fn announced(&mut self, done: Done, effects: &mut Vec<Effect>) {
        if matches!(done, Done::Written) {
            self.failures = 0;
            self.start_look(effects);
            return;
        }
        let file = self.tiers[0].file(HEAD_FILE);
        if self.retry(&file, &done) {
            let tag = self.tags.ask(Op::Announce);
            effects.push(Effect::Write { file, bytes: Head::default().encode(), tag });
        }
    }

    /// What a completion left to ask of the other writers: heads to read again after a failed
    /// opening, and a look asked for while one was in flight.
    pub(crate) fn peers_ask(&mut self, effects: &mut Vec<Effect>) {
        if self.broken.is_some() {
            return;
        }
        let Some(peers) = &mut self.peers else { return };
        let restart = std::mem::take(&mut peers.restart);
        let again = peers.look.is_none() && peers.done > 0 && std::mem::take(&mut peers.again);
        for at in restart {
            self.ask_head(at, effects);
        }
        if again {
            self.start_look(effects);
        }
    }

    /// Why a tier failed to open: a shared store's other writer is read again, else it stops.
    pub(crate) fn tier_failed(&mut self, at: usize, file: FileName, why: String) {
        match self.is_peer(at) {
            true => self.peer_failed(at, file, why),
            false => self.fail(file, why),
        }
    }
}

