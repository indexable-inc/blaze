//! A session: what one process verified under one pinned world, the index that says who read
//! what, and what carries both to the next pin (docs/v2/plan/design/kernel.md, "Carrying a
//! session").
//!
//! - **Rows.** For an identity, the key of the entry valid at the session's pin, the hash of
//!   its value once a parent asked for it, and its height: one more than the highest step it
//!   reads, 0 where it reads no step.
//! - **Postings.** For a read's name (its kind and key, as a read-list keeps it), the rows whose
//!   entry's list names it. A read of a step's value is posted under its name as any other, and
//!   that name also says which row is the step's own ([`Session::link`]), so the readers of a
//!   row are the postings of its name.
//! - **Admission is two calls that cannot be taken apart.** [`Session::post`] writes the
//!   postings of a list and gives a [`Posted`]; [`Session::admit`] makes a row live and takes a
//!   [`Posted`], which nothing else makes. So no row is live whose postings are not written
//!   (invariant I1), and nothing outside this module inserts a row. A posting with no live row
//!   (a lookup that failed after it posted, a row dropped since) costs a check at a carry and
//!   changes no answer.
//! - **The memory is the session's**: rows, postings and names live as long as whoever holds
//!   the [`Session`] keeps it, and are freed with it. Postings are never taken out while it
//!   lives: a row that is dropped and admitted again finds its postings written
//!   (`Row::posted`), so they do not grow with the number of carries.
//! - **It is kept in the store as records, and its memory is only what was touched.**
//!   [`Session::save`] writes the rows and postings that changed since the last save into
//!   maps of the store ([`crate::trie`]: rows by identity, and for each kind of read its
//!   postings by name) and gives the key of one root record; [`Session::open`] reads that
//!   record alone. A row is read when its identity is first looked for, a posting when a
//!   carry needs its readers, so a session of a million rows that an edit reaches ten of holds
//!   those ten and their paths. Whoever holds the session keeps the root's key (32 bytes, the
//!   one thing that changes in place) and may drop the [`Session`] whenever it is saved. The
//!   store is write-once and a session is not a function of its checkout and pin (it depends
//!   on what was asked), so nothing of it is keyed by either (rule 21): every record is under
//!   the hash of its own bytes.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use blaze_cas::Key;
use blaze_ids::ContentId;

#[cfg(test)]
use crate::Seen;
#[cfg(test)]
use crate::nest::planted::{self, Bug};
use crate::store::{Get, Put};
use crate::trie::{self, Broken, Cursor, Nodes};
use crate::{KernelError, Of, Pin, Read, ReadKind, StepId, World, entry, hex, table};

/// `blaze kernel s`, then the layout's number: the root record of a saved session.
const ROOT_MAGIC: [u8; 16] = *b"blaze kernel s 2";

/// The key of a read's postings in the map of its kind: the hash of its name.
fn name_key(name: &[u8]) -> ContentId {
    ContentId::of("blaze v2 kernel session name 1", name)
}

/// A store failure under a session's records, as the error a request or a carry gives.
fn broken(broken: Broken) -> KernelError {
    KernelError::Store {
        of: Of {
            step: "session",
            // alloc: an error
            label: "the rows and postings of a saved session".to_owned(),
            identity: broken.key.0,
        },
        key: broken.key,
        why: broken.why,
    }
}

fn unreadable(key: Key, why: &str) -> KernelError {
    // alloc: an error
    broken(Broken { key, why: why.to_owned() })
}

/// An entry found valid or made under a session's pin, as its row keeps it.
#[derive(Clone)]
pub(crate) struct Known {
    pub key: Key,
    /// The hash of its value's bytes: none while only its key was needed (a value asked for at
    /// the top, which no step has read).
    pub hash: Option<ContentId>,
    /// Its reads' names and what each saw: read only by a planted bug's control.
    #[cfg(test)]
    pub reads: Vec<(Vec<u8>, Seen)>,
}

/// The live part of a row.
#[cfg_attr(test, derive(Clone))]
struct Live {
    known: Known,
    height: u32,
}

/// One identity the session admitted, now or before.
#[cfg_attr(test, derive(Clone))]
struct Row {
    /// None once a carry dropped it: it is looked up again when asked for.
    live: Option<Live>,
    /// The slots of its identity's chain whose lists' postings are written, by bit, for the
    /// first 64 slots: a list past those is checked reader by reader when it is posted.
    posted: u64,
    /// The name of the read of its value, by its place in `posts`, once a step read it.
    name: Option<u32>,
    /// Whether what the store keeps of it is read: false for a row known only as a reader in
    /// a posting that was read. Such a row is read before anything asks what it holds.
    loaded: bool,
    /// Whether it differs from what the store keeps: the next save writes it.
    dirty: bool,
}

impl Row {
    fn new(loaded: bool) -> Row {
        Row { live: None, posted: 0, name: None, loaded, dirty: false }
    }
}

/// The names of one kind of read, and the map of the store their postings are saved in.
#[cfg_attr(test, derive(Clone))]
struct Names {
    /// The names in memory, by place in `posts`.
    names: Vec<u32>,
    /// The root of the kind's postings in the store: none until a save wrote one.
    root: Option<Key>,
    /// Whether every name under `root` is in memory.
    whole: bool,
}

/// The readers of one read.
#[cfg_attr(test, derive(Clone))]
struct Post {
    /// The rows whose entry's list names the read, each once.
    readers: Vec<u32>,
    /// For a read of a step's value: that step's own row.
    row: Option<u32>,
    /// Whether the readers the store keeps for the read are among `readers`: false for a name
    /// first met in this process while its kind has postings in the store. Readers are posted
    /// to it all the same, and the store's are read in when a carry or a save needs them all.
    loaded: bool,
    /// Whether it differs from what the store keeps: the next save writes it.
    dirty: bool,
}

/// Which transition admits an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum By {
    /// A lookup found it in the store: kept by an earlier request, another process or an
    /// earlier daemon.
    Hit,
    /// It was computed now and kept.
    Keep,
    /// A cover's manifest names it ([`Session::cover`]): its own lists are not read, and the
    /// one posting written is from the cover to it (invariant I5).
    Cover,
}

/// The postings of one list of one identity are written: what [`Session::admit`] takes. Only
/// [`Session::post`] makes one.
pub(crate) struct Posted {
    row: u32,
    height: u32,
}

/// What this process verified under one pinned world, through one store and one registry of
/// steps: for an identity, the key of the entry valid at that pin and the hash of its value. A
/// value in it is not looked up again: a child asked for by many parents, in one request or in
/// many, is verified once, and a value asked for again at the top is one get of its entry,
/// with no probe, no read-list read and no read valued.
///
/// It is of one pin ([`World::pin`]). A request whose world is at another pin is refused
/// ([`KernelError::Moved`]), never answered from rows of the session's pin; a world that
/// moved while a request ran is refused the same way, the request's answer is not given, and
/// the rows are dropped. Whoever holds the session carries it to the world as it is now
/// ([`crate::Kernel::carry`]) or makes a new one.
///
/// The invariant: a session changes no answer. Dropped or cleared between any two requests,
/// every request gives the bytes it gives with it kept (`exhaustive/nested.rs`: every sequence
/// with a drop at every place; control `a_session_that_outlives_its_pin_is_caught`).
#[cfg_attr(test, derive(Clone))]
pub struct Session {
    pin: Pin,
    /// The registry of steps its rows were verified under ([`crate::Kernel::registry`]): none
    /// until its first request.
    registry: Option<ContentId>,
    rows: blaze_hash::IndexMap<ContentId, Row, Global>,
    /// How many rows are live.
    live: usize,
    posts: blaze_hash::IndexMap<Box<[u8]>, Post, Global>,
    /// For a kind of read: its names, and where the store keeps their postings.
    kinds: blaze_hash::HashMap<u16, Names, Global>,
    /// The root of the rows in the store: none until a save wrote one.
    rows_root: Option<Key>,
    /// The rows and names that differ from what the store keeps, each once.
    dirty_rows: Vec<u32>,
    dirty_posts: Vec<u32>,
    /// The nodes of the store's maps that the request or carry now running read: each is one
    /// get however many rows and postings are read under it. Empty between them.
    nodes: Nodes,
    /// The tables the request or carry now running read by part, by the key of their entry:
    /// each is one get of the store however many parts are read from it. An entry's key
    /// names one value, so what is held is never out of date. Empty between them.
    tables: blaze_hash::HashMap<Key, Box<[u8]>, Global>,
    /// The name being looked for by [`Session::link`]: one buffer, reused.
    scratch: Vec<u8>,
    /// A planted bug's postings, written a carry late.
    #[cfg(test)]
    late: Vec<(u32, u32)>,
}

impl Session {
    /// A session of `world` as it is now, with nothing verified.
    pub fn new<W: World + ?Sized>(world: &W) -> Session {
        Session {
            pin: world.pin(),
            registry: None,
            // alloc: the session's rows; empty until a value is verified
            rows: blaze_hash::IndexMap::new_in(Global),
            live: 0,
            // alloc: the session's postings; empty until a value with reads is verified
            posts: blaze_hash::IndexMap::new_in(Global),
            // alloc: the session's names by kind; empty until a value with reads is verified
            kinds: blaze_hash::HashMap::new_in(Global),
            rows_root: None,
            // alloc: none until a row changes
            dirty_rows: Vec::new(),
            // alloc: none until a posting changes
            dirty_posts: Vec::new(),
            nodes: Nodes::new(),
            // alloc: none until a part of a table is read
            tables: blaze_hash::HashMap::new_in(Global),
            // alloc: none until a step's value is read
            scratch: Vec::new(),
            #[cfg(test)]
            late: Vec::new(),
        }
    }

    /// The session saved under `root` ([`Session::save`]), with nothing of it read but its
    /// root record: one get. Its rows and postings are read from `store` as requests and
    /// carries reach them. It is of the pin it was saved at: whoever opens it carries it from
    /// there.
    ///
    /// A root of another layout, or one the store does not have, is refused naming it: the
    /// holder then starts from [`Session::new`], which costs lookups and changes no answer.
    pub fn open<G: Get + ?Sized>(store: &G, root: Key) -> Result<Session, KernelError> {
        let bytes = match store.get(&root) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err(unreadable(root, "the root record of a saved session is missing")),
            // alloc: an error
            Err(why) => return Err(broken(Broken { key: root, why })),
        };
        let family = ROOT_MAGIC.len() - 1;
        match bytes.get(..ROOT_MAGIC.len()) {
            Some(magic) if magic == ROOT_MAGIC => {}
            Some(magic) if magic[..family] == ROOT_MAGIC[..family] && magic[family].is_ascii_digit() => {
                // alloc: an error
                let why = format!(
                    "its value is a saved session of layout {} (`{}`), where this blaze reads layout {}",
                    magic[family] as char,
                    String::from_utf8_lossy(magic),
                    ROOT_MAGIC[family] as char
                );
                return Err(broken(Broken { key: root, why }));
            }
            _ => return Err(unreadable(root, "its value is no saved session")),
        }
        Session::read_root(&bytes[ROOT_MAGIC.len()..])
            .ok_or_else(|| unreadable(root, "the root record of a saved session ends early or has bytes left over"))
    }

    fn read_root(bytes: &[u8]) -> Option<Session> {
        let mut input = Cursor::new(bytes);
        let pin = Pin(input.id()?);
        let registry = input.maybe_id()?;
        let live = usize::try_from(input.u64()?).ok()?;
        let rows_root = input.maybe_id()?.map(Key);
        // alloc: the session's names by kind
        let mut kinds = blaze_hash::HashMap::new_in(Global);
        for _ in 0..input.u16()? {
            let kind = input.u16()?;
            let root = Some(Key(input.id()?));
            // alloc: none until a name of the kind is read
            kinds.insert(kind, Names { names: Vec::new(), root, whole: false });
        }
        input.ended().then(|| Session {
            pin,
            registry,
            // alloc: the session's rows; empty until one is read
            rows: blaze_hash::IndexMap::new_in(Global),
            live,
            // alloc: the session's postings; empty until one is read
            posts: blaze_hash::IndexMap::new_in(Global),
            kinds,
            rows_root,
            // alloc: none until a row changes
            dirty_rows: Vec::new(),
            // alloc: none until a posting changes
            dirty_posts: Vec::new(),
            nodes: Nodes::new(),
            // alloc: none until a part of a table is read
            tables: blaze_hash::HashMap::new_in(Global),
            // alloc: none until a step's value is read
            scratch: Vec::new(),
            #[cfg(test)]
            late: Vec::new(),
        })
    }

    /// Writes what changed since the last save (the rows admitted, filled, named or dropped,
    /// and the postings that took a reader) into the store, and gives the key of the root
    /// record of the session as it is now: what [`Session::open`] takes. Writes are of the
    /// changed rows' and postings' paths (each a few nodes), never of the session.
    ///
    /// The order is the crash safety: every node before the node that names it, the root
    /// record last, and its key given only after its put. A save that stops part way has
    /// changed nothing the old root reaches, and the session still holds every change for the
    /// next save.
    pub fn save<P: Put + ?Sized>(&mut self, store: &mut P) -> Result<Key, KernelError> {
        // alloc: one save's changed postings, by kind in order (rule 25)
        let mut posts: BTreeMap<u16, Vec<(ContentId, Vec<u8>)>> = BTreeMap::new();
        for n in 0..self.dirty_posts.len() {
            let at = self.dirty_posts[n] as usize;
            self.load_post(&*store, at)?;
            let (name, post) = self.posts.get_index(at).expect("a changed name is in the session");
            let kind = entry::named(name).expect("a read's name starts with its kind").kind.0;
            // alloc: one posting's record
            let mut bytes = Vec::with_capacity(2 + name.len() + 1 + 32 + 2 + 32 * post.readers.len());
            trie::put_counted(&mut bytes, name);
            trie::put_maybe_id(&mut bytes, post.row.map(|row| self.id_of(row)));
            // alloc: one posting's readers by identity, in order (rule 25)
            let mut readers: Vec<[u8; 32]> = post.readers.iter().map(|row| self.id_of(*row).0).collect();
            readers.sort_unstable();
            readers.dedup();
            trie::put_var(&mut bytes, readers.len() as u64);
            for reader in &readers {
                bytes.extend_from_slice(reader);
            }
            posts.entry(kind).or_default().push((name_key(name), bytes));
        }
        // The nodes read for the postings above are let go: the save's own reads are of the
        // paths it writes, each once.
        self.nodes.clear();
        // alloc: one save's new roots
        let mut roots: BTreeMap<u16, Key> = BTreeMap::new();
        for (kind, names) in self.kinds.iter() {
            if let Some(root) = names.root {
                roots.insert(*kind, root);
            }
        }
        for (kind, changes) in posts {
            let had = roots.get(&kind).copied();
            if let Some(root) = trie::update(store, had, changes).map_err(broken)? {
                roots.insert(kind, root);
            }
        }
        // alloc: one save's changed rows
        let mut rows = Vec::with_capacity(self.dirty_rows.len());
        for row in &self.dirty_rows {
            let (id, it) = self.rows.get_index(*row as usize).expect("a changed row is in the session");
            let name = it.name.map(|at| &**self.posts.get_index(at as usize).expect("a row's name is in the session").0);
            // alloc: one row's record
            let mut bytes = Vec::with_capacity(2 + 66 + name.map_or(0, |name| 2 + name.len()));
            let hash = it.live.as_ref().and_then(|live| live.known.hash);
            bytes.push(u8::from(it.live.is_some()) | u8::from(hash.is_some()) << 1 | u8::from(name.is_some()) << 2);
            trie::put_var(&mut bytes, it.posted);
            if let Some(live) = &it.live {
                bytes.extend_from_slice(&live.known.key.0.0);
                trie::put_var(&mut bytes, u64::from(live.height));
                if let Some(hash) = hash {
                    bytes.extend_from_slice(&hash.0);
                }
            }
            if let Some(name) = name {
                trie::put_counted(&mut bytes, name);
            }
            rows.push((*id, bytes));
        }
        let rows_root = trie::update(store, self.rows_root, rows).map_err(broken)?;
        // alloc: the root record
        let mut bytes = Vec::with_capacity(ROOT_MAGIC.len() + 32 + 33 + 8 + 33 + 2 + 34 * roots.len());
        bytes.extend_from_slice(&ROOT_MAGIC);
        bytes.extend_from_slice(&self.pin.0.0);
        trie::put_maybe_id(&mut bytes, self.registry);
        bytes.extend_from_slice(&(self.live as u64).to_be_bytes());
        trie::put_maybe_id(&mut bytes, rows_root.map(|key| key.0));
        let kinds = u16::try_from(roots.len()).expect("a kind of read is a u16");
        bytes.extend_from_slice(&kinds.to_be_bytes());
        for (kind, root) in &roots {
            bytes.extend_from_slice(&kind.to_be_bytes());
            bytes.extend_from_slice(&root.0.0);
        }
        let root = Key(ContentId::of("blaze v2 kernel session root 1", &bytes));
        if let Err(mismatch) = store.put(root, blaze_cas::Kind::Query, bytes) {
            // alloc: an error
            let why = format!(
                "the store keeps other bytes ({:x}) under the hash of a session's root record ({:x})",
                mismatch.kept, mismatch.given
            );
            return Err(broken(Broken { key: root, why }));
        }
        // The root is in the store: what is in memory is now what the store keeps.
        self.rows_root = rows_root;
        for (kind, root) in roots {
            self.kinds.get_mut(&kind).expect("a kind with postings is in the session").root = Some(root);
        }
        for row in std::mem::take(&mut self.dirty_rows) {
            self.rows.get_index_mut(row as usize).expect("a changed row is in the session").1.dirty = false;
        }
        for at in std::mem::take(&mut self.dirty_posts) {
            self.posts.get_index_mut(at as usize).expect("a changed name is in the session").1.dirty = false;
        }
        Ok(root)
    }

    /// The identity of row `row`.
    fn id_of(&self, row: u32) -> ContentId {
        *self.rows.get_index(row as usize).expect("a reader is a row of the session").0
    }

    /// The place in `posts` of the read named `name`, made where this process had not met it.
    fn name_at(&mut self, name: &[u8]) -> usize {
        if let Some(at) = self.posts.get_index_of(name) {
            return at;
        }
        let kind = entry::named(name).expect("a read's name starts with its kind").kind.0;
        // alloc: the session's names by kind
        let names =
            self.kinds.entry(kind).or_insert_with(|| Names { names: Vec::new(), root: None, whole: true });
        // Nothing to read in where the store keeps no posting of its kind, or all are here.
        let loaded = names.root.is_none() || names.whole;
        // alloc: the session's names, one a read it ever met
        let post = Post { readers: Vec::new(), row: None, loaded, dirty: false };
        let at = self.posts.insert_full(Box::from(name), post).0;
        // alloc: the session's names by kind
        names.names.push(u32::try_from(at).expect("a session's names are counted in u32"));
        at
    }

    /// The record the store keeps for the postings of `name`, none where it keeps none.
    /// Takes the session's kinds and nodes apart from it: `name` is borrowed from its names.
    fn stored_post<G: Get + ?Sized>(
        kinds: &blaze_hash::HashMap<u16, Names, Global>,
        nodes: &mut Nodes,
        store: &G,
        name: &[u8],
    ) -> Result<Option<Vec<u8>>, KernelError> {
        let kind = entry::named(name).expect("a read's name starts with its kind").kind.0;
        let root = kinds.get(&kind).and_then(|names| names.root);
        if root.is_none() || Session::planted_open() {
            return Ok(None);
        }
        trie::get(store, nodes, root, &name_key(name)).map_err(broken)
    }

    /// Reads in the readers the store keeps for the name at `at`, where they are not yet
    /// among its readers.
    fn load_post<G: Get + ?Sized>(&mut self, store: &G, at: usize) -> Result<(), KernelError> {
        let (name, post) = self.posts.get_index(at).expect("a name by its place");
        if post.loaded {
            return Ok(());
        }
        if let Some(bytes) = Session::stored_post(&self.kinds, &mut self.nodes, store, name)? {
            self.merge_post(at, &bytes)?;
        }
        self.posts.get_index_mut(at).expect("a name by its place").1.loaded = true;
        Ok(())
    }

    /// Adds what the posting record `bytes` says to the name at `at`: its step's own row and
    /// its readers, each a row of the session from now on (read when something asks of it).
    fn merge_post(&mut self, at: usize, bytes: &[u8]) -> Result<(), KernelError> {
        let parsed = (|| {
            let mut input = Cursor::new(bytes);
            input.counted()?;
            let own = input.maybe_id()?;
            let count = usize::try_from(input.var()?).ok()?;
            let readers = input.take(count.checked_mul(32)?)?;
            input.ended().then_some((own, readers))
        })();
        let Some((own, readers)) = parsed else {
            let name = self.posts.get_index(at).expect("a name by its place").0;
            return Err(unreadable(
                Key(name_key(name)),
                "the posting record under this name's hash ends early or has bytes left over",
            ));
        };
        let own = own.map(|id| self.row_of(id));
        // alloc: the rows of one posting's readers, until they are added to it
        let rows: Vec<u32> = readers
            .chunks_exact(32)
            .map(|id| self.row_of(ContentId(id.try_into().expect("a chunk of 32 bytes"))))
            .collect();
        let post = self.posts.get_index_mut(at).expect("a name by its place").1;
        if post.row.is_none() {
            post.row = own;
        }
        let had = post.readers.len();
        for row in rows {
            if !post.readers[..had].contains(&row) {
                // alloc: the session's postings, one a reader the store keeps
                post.readers.push(row);
            }
        }
        Ok(())
    }

    /// The row of identity `id`, made as one not yet read where the session has none.
    fn row_of(&mut self, id: ContentId) -> u32 {
        // alloc: the session's rows, one an identity a posting it read names
        let (row, _) = self.rows.get_or_insert_with(id, || Row::new(false));
        u32::try_from(row).expect("a session's rows are counted in u32")
    }

    /// The row of identity `id` with what the store keeps of it read: its number.
    fn load_row<G: Get + ?Sized>(&mut self, store: &G, id: &ContentId) -> Result<u32, KernelError> {
        let at = self.row_of(*id);
        if self.rows[at as usize].loaded {
            return Ok(at);
        }
        let stored = trie::get(store, &mut self.nodes, self.rows_root, id).map_err(broken)?;
        let (live, posted, name) = match &stored {
            None => (None, 0, None),
            Some(bytes) => Session::read_row(bytes).ok_or_else(|| {
                unreadable(Key(*id), "the row record under this identity ends early or has bytes left over")
            })?,
        };
        let name = name.map(|name| {
            let name = self.name_at(name);
            self.posts.get_index_mut(name).expect("a name made just now").1.row = Some(at);
            u32::try_from(name).expect("a session's names are counted in u32")
        });
        let row = self.rows.get_index_mut(at as usize).expect("the row made above").1;
        row.live = live;
        row.posted = posted;
        row.name = name;
        row.loaded = true;
        Ok(at)
    }

    fn read_row(bytes: &[u8]) -> Option<(Option<Live>, u64, Option<&[u8]>)> {
        let mut input = Cursor::new(bytes);
        let flags = input.u8()?;
        let posted = input.var()?;
        let live = match flags & 1 != 0 {
            false => None,
            true => {
                let key = Key(input.id()?);
                let height = u32::try_from(input.var()?).ok()?;
                let hash = match flags & 2 != 0 {
                    true => Some(input.id()?),
                    false => None,
                };
                Some(Live {
                    known: Known {
                        key,
                        hash,
                        // alloc: none: a planted bug's reads are of the process that made them
                        #[cfg(test)]
                        reads: Vec::new(),
                    },
                    height,
                })
            }
        };
        let name = match flags & 4 != 0 {
            true => Some(input.counted()?),
            false => None,
        };
        (flags < 8 && input.ended()).then_some((live, posted, name))
    }

    /// Reads every posting the store keeps of `kind` that is not in memory: for a change set
    /// that says any read of the kind may differ, and for the count a refusal names.
    fn load_kind<G: Get + ?Sized>(&mut self, store: &G, kind: u16) -> Result<(), KernelError> {
        let Some(names) = self.kinds.get(&kind) else {
            return Ok(());
        };
        if names.whole {
            return Ok(());
        }
        let root = names.root.filter(|_| !Session::planted_open());
        // alloc: the kind's posting records, until each is added to the session
        let mut records: Vec<Vec<u8>> = Vec::new();
        // alloc: one posting's record, until it is added to the session
        trie::each(store, root, &mut |_, bytes| records.push(bytes.to_vec())).map_err(broken)?;
        for bytes in &records {
            let Some(name) = Cursor::new(bytes).counted() else {
                return Err(unreadable(
                    root.expect("a record was read under a root"),
                    "a posting record under this root ends before its name does",
                ));
            };
            let at = self.name_at(name);
            if !self.posts.get_index(at).expect("a name made just now").1.loaded {
                self.merge_post(at, bytes)?;
                self.posts.get_index_mut(at).expect("a name made just now").1.loaded = true;
            }
        }
        // A name in memory that the store keeps nothing for has nothing to read in.
        let names = self.kinds.get_mut(&kind).expect("the kind found above");
        names.whole = true;
        for n in 0..names.names.len() {
            let at = names.names[n] as usize;
            self.posts.get_index_mut(at).expect("a name by its place").1.loaded = true;
        }
        Ok(())
    }

    /// A planted bug: an opened session reads no posting from the store, as if none were kept.
    fn planted_open() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::OpenUnposted) {
                return true;
            }
        }
        false
    }

    /// The pin it is of.
    pub fn pin(&self) -> Pin {
        self.pin
    }

    /// How many values it holds verified.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Drops what it verified, postings too. No answer changes: each value is looked up again
    /// when asked for.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.live = 0;
        self.posts.clear();
        self.kinds.clear();
        self.rows_root = None;
        self.dirty_rows.clear();
        self.dirty_posts.clear();
        self.nodes.clear();
        self.let_go();
        #[cfg(test)]
        self.late.clear();
    }

    /// The table under the entry `key`, where the request or carry now running read it.
    pub(crate) fn table(&self, key: &Key) -> Option<&[u8]> {
        self.tables.get(key).map(|it| &**it)
    }

    /// Holds `table`, the value of the entry `key`, until the request or carry ends.
    pub(crate) fn hold_table(&mut self, key: Key, table: &[u8]) {
        // alloc: one table's bytes, until the request or carry that read a part of it ends
        self.tables.insert(key, Box::from(table));
    }

    /// Lets go of the tables it holds.
    fn let_go(&mut self) {
        // Asked at the start and the end of every request: nothing to do for most.
        if !self.tables.is_empty() {
            self.tables.clear();
        }
    }

    /// Refuses, before a request reads anything, a world that is not at the session's pin and
    /// a kernel whose registry is not the one its rows were verified under.
    pub(crate) fn enter<W: World + ?Sized>(
        &mut self,
        world: &W,
        registry: ContentId,
    ) -> Result<(), KernelError> {
        // What a request that stopped at an error left: its memory, never a wrong node.
        self.nodes.clear();
        self.let_go();
        match self.registry {
            None => self.registry = Some(registry),
            Some(had) if had != registry => {
                return Err(KernelError::Reregistered { session: had, now: registry });
            }
            Some(_) => {}
        }
        let now = world.pin();
        if now == self.pin || Session::planted_stale() {
            return Ok(());
        }
        Err(KernelError::Moved { pinned: self.pin, now })
    }

    /// Refuses a world that moved while a request ran, and drops the rows: some were verified
    /// against a world that is no longer the pinned one.
    pub(crate) fn leave<W: World + ?Sized>(&mut self, world: &W) -> Result<(), KernelError> {
        // The request or the carry is over: the nodes and the tables it read are let go.
        self.nodes.clear();
        self.let_go();
        let now = world.pin();
        if now == self.pin || Session::planted_stale() {
            return Ok(());
        }
        self.clear();
        Err(KernelError::Moved { pinned: self.pin, now })
    }

    fn planted_stale() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::StalePin) {
                return true;
            }
        }
        false
    }

    /// The live row of the identity whose ID is `id`: its number and what it keeps. A row of
    /// a saved session is read from `store` the first time its identity is looked for (the
    /// nodes on its path, once a process); every lookup starts here, so a row that is posted
    /// or admitted is one that was read.
    pub(crate) fn known<G: Get + ?Sized>(
        &mut self,
        store: &G,
        id: &ContentId,
    ) -> Result<Option<(u32, Known)>, KernelError> {
        let row = match self.rows.get_full(id) {
            Some((row, _, it)) if it.loaded => row as u32,
            None if self.rows_root.is_none() => return Ok(None),
            _ => self.load_row(store, id)?,
        };
        // clone: a key and a hash; a planted bug's reads in a test
        Ok(self.live_of(row).map(|live| (row, live.known.clone())))
    }

    /// Whether the identity whose ID is `id` was admitted, now or before a carry dropped it,
    /// with an entry that has reads: a lookup then skips its entry with no reads
    /// ([`crate::Lists::listed`]).
    pub(crate) fn listed(&self, id: &ContentId) -> bool {
        self.rows.get_full(id).is_some_and(|(_, _, row)| row.posted != 0)
    }

    /// Writes the postings of the entry the identity `id` is about to be admitted with: each
    /// read `names` names (a name as a read-list keeps it) → this identity's row. `slot` is
    /// the slot of the list in the identity's chain, none for an entry with no reads. The
    /// row's height comes with it: one more than the highest step among the reads.
    ///
    /// The first half of an admission (the module's doc): [`Session::admit`] takes what this
    /// gives. Each step among the reads was looked up or run just before, through
    /// [`Session::link`], so its row is live and its name says which row that is.
    pub(crate) fn post<'n>(
        &mut self,
        id: ContentId,
        slot: Option<u32>,
        names: impl Iterator<Item = &'n [u8]>,
        by: By,
    ) -> Posted {
        // alloc: the session's rows, one an identity it ever admitted
        let (row, _) = self.rows.get_or_insert_with(id, || Row::new(true));
        assert!(self.rows[row].loaded, "a row is looked for (`Session::known`) before it is posted");
        let bit = slot.filter(|n| *n < 64).map(|n| 1u64 << n);
        let written = bit.is_some_and(|bit| self.rows[row].posted & bit != 0);
        // Whether another list of this identity was posted before.
        let again = self.rows[row].posted != 0 || bit.is_none();
        let row = u32::try_from(row).expect("a session's rows are counted in u32");
        let unwritten = Session::planted_unposted(by);
        let mut height = 0;
        for name in names {
            let read = entry::named(name).expect("a read-list's name starts with its kind");
            let at = self.name_at(name);
            let post = self.posts.get_index_mut(at).expect("a name found or put just now").1;
            // The step whose value the read is of, whole or by part: this entry is above it.
            let step = match read.kind {
                ReadKind::STEP => post.row,
                ReadKind::PART | ReadKind::KEYS => {
                    let (step, _) = table::split(read).expect("a name the kernel wrote names its step");
                    self.scratch.clear();
                    table::name(ReadKind::STEP, step, &[], &mut self.scratch);
                    let at = self.posts.get_index_of(&*self.scratch);
                    at.and_then(|at| self.posts.get_index(at)).and_then(|(_, post)| post.row)
                }
                _ => None,
            };
            if read.kind.is_kernels() {
                let child = step.expect(
                    "a step whose value an entry read is looked up or run, and linked, before the entry is posted",
                );
                let child = self.rows[child as usize].live.as_ref().expect(
                    "a step whose value an entry read is live in the session while the entry is posted",
                );
                height = height.max(child.height + 1);
            }
            if written || unwritten {
                continue;
            }
            // A reader of a part, or of the keys, is also posted once under the step's
            // table name: what a carry dirties when the table cannot be valued at all.
            if read.kind == ReadKind::PART || read.kind == ReadKind::KEYS {
                let (step, _) = table::split(read).expect("a name the kernel wrote names its step");
                self.scratch.clear();
                table::name(ReadKind::TABLE, step, &[], &mut self.scratch);
                let scratch = std::mem::take(&mut self.scratch);
                let all = self.name_at(&scratch);
                self.scratch = scratch;
                let all_post = self.posts.get_index_mut(all).expect("a name found or put just now").1;
                // Once a list: its parts of one step are posted one after another. Once a
                // row: a row posted before (another list of its identity) is looked for.
                if all_post.readers.last() != Some(&row) && !(again && all_post.readers.contains(&row)) {
                    // alloc: the session's postings, one a table a list of a row reads
                    all_post.readers.push(row);
                    if !all_post.dirty {
                        all_post.dirty = true;
                        // alloc: the names the next save writes
                        self.dirty_posts.push(all as u32);
                    }
                }
            }
            let post = self.posts.get_index_mut(at).expect("a name found or put just now").1;
            #[cfg(test)]
            {
                if planted::is(Bug::PostLate) {
                    self.late.push((at as u32, row));
                    continue;
                }
            }
            if bit.is_some() || !post.readers.contains(&row) {
                // alloc: the session's postings, one a read of a list of a row
                post.readers.push(row);
                if !post.dirty {
                    post.dirty = true;
                    // alloc: the names the next save writes
                    self.dirty_posts.push(at as u32);
                }
            }
        }
        if let Some(bit) = bit
            && !written
            && !unwritten
            && !Session::planted_late()
        {
            self.rows.get_index_mut(row as usize).expect("the row made above").1.posted |= bit;
            self.touch(row);
        }
        Posted { row, height }
    }

    fn planted_unposted(by: By) -> bool {
        #[cfg(test)]
        {
            if by == By::Hit && planted::is(Bug::HitUnposted) {
                return true;
            }
            if by == By::Cover && planted::is(Bug::CoverUnposted) {
                return true;
            }
        }
        let _ = by;
        false
    }

    fn planted_late() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::PostLate) {
                return true;
            }
        }
        false
    }

    /// Makes the row whose postings `posted` says are written live with `known`: its number.
    /// The second half of an admission, and the only place a row becomes live.
    pub(crate) fn admit(&mut self, posted: Posted, known: Known) -> u32 {
        let row = self.rows.get_index_mut(posted.row as usize).expect("a posting is of a row").1;
        if row.live.is_none() {
            self.live += 1;
        }
        row.live = Some(Live { known, height: posted.height });
        self.touch(posted.row);
        posted.row
    }

    /// Admits the members a cover's manifest names, each with the entry key and the value hash
    /// the manifest gives, through the cover whose identity is `cover` (live in the session:
    /// its lookup found it and posted it under its covering reads). How many were admitted;
    /// a member the session holds verified already is left as it is.
    ///
    /// Invariant I5: a member admitted here has no posting of its own reads (its lists were
    /// not read: that is what the cover saves), so it is posted as a reader of the cover: the
    /// cover's row is given a name (`step`, then its identity; nothing asks for it by that
    /// name) and each member is a reader of that name. A covering read that changes reaches
    /// the cover's row, and a carry drops a cover with every reader ([`crate::nest::carry`]).
    /// Its height is one above the cover's; it takes its own when its own lookup admits it.
    pub(crate) fn cover<G: Get + ?Sized>(
        &mut self,
        store: &G,
        cover: &ContentId,
        step: StepId,
        members: &mut dyn Iterator<Item = (ContentId, Key, ContentId)>,
    ) -> Result<usize, KernelError> {
        let Some((row, _)) = self.known(store, cover)? else {
            return Ok(0);
        };
        self.link(row, step, &cover.0);
        let height = self.live_of(row).expect("a row `known` gave is live").height + 1;
        let name = self.rows.get_index(row as usize).expect("a row `known` gave").1.name;
        let at = name.expect("a row linked just above has a name") as usize;
        self.load_post(store, at)?;
        // The cover's readers before this, in order: a member is looked for in them by
        // halving, never one by one (a cover has thousands).
        // alloc: one seeding's copy of one posting's readers
        let mut had = self.posts.get_index(at).expect("the cover's name").1.readers.clone();
        had.sort_unstable();
        let unposted = Session::planted_unposted(By::Cover);
        let mut admitted = 0;
        for (id, key, hash) in members {
            if self.known(store, &id)?.is_some() {
                continue;
            }
            // alloc: the session's rows, one a member it admits
            let (member, _) = self.rows.get_or_insert_with(id, || Row::new(true));
            let member = u32::try_from(member).expect("a session's rows are counted in u32");
            if !unposted && had.binary_search(&member).is_err() {
                let post = self.posts.get_index_mut(at).expect("the cover's name").1;
                // alloc: the session's postings, one a member of a cover
                post.readers.push(member);
                if !post.dirty {
                    post.dirty = true;
                    // alloc: the names the next save writes
                    self.dirty_posts.push(at as u32);
                }
            }
            let known = Known {
                key,
                hash: Some(hash),
                // alloc: none: a planted bug's reads are of the process that read them
                #[cfg(test)]
                reads: Vec::new(),
            };
            self.admit(Posted { row: member, height }, known);
            admitted += 1;
        }
        Ok(admitted)
    }

    /// The identity of the step whose value the read `kind`, `key` is of, where a step of
    /// the session read it: how a cover's keep finds that a member's child is a member.
    pub(crate) fn named_row(&self, kind: ReadKind, key: &[u8]) -> Option<ContentId> {
        // alloc: one name, for one check at a cover's keep
        let mut name = Vec::with_capacity(2 + key.len());
        entry::name_kind(kind, &mut name);
        name.extend_from_slice(key);
        let at = self.posts.get_index_of(&*name)?;
        let row = self.posts.get_index(at)?.1.row?;
        Some(self.id_of(row))
    }

    /// Says row `row` differs from what the store keeps of it.
    fn touch(&mut self, row: u32) {
        let it = self.rows.get_index_mut(row as usize).expect("a row by its number").1;
        if !it.dirty {
            it.dirty = true;
            // alloc: the rows the next save writes
            self.dirty_rows.push(row);
        }
    }

    /// Gives the live row `row` the hash of its value, read from its entry for the first step
    /// that asked for it.
    pub(crate) fn fill(&mut self, row: u32, known: Known) {
        let Some(live) = self.live_mut(row) else {
            return;
        };
        if known.hash.is_some() {
            live.known = known;
            self.touch(row);
        }
    }

    fn live_mut(&mut self, row: u32) -> Option<&mut Live> {
        self.rows.get_index_mut(row as usize)?.1.live.as_mut()
    }

    fn live_of(&self, row: u32) -> Option<&Live> {
        self.rows.get_index(row as usize)?.1.live.as_ref()
    }

    /// Says that `row` is the value a read of the value of the step numbered `step` of the
    /// input `input` holds names: from then on the postings of that name are the row's
    /// readers, and a carry can look the row up again by the read's key. Called wherever a
    /// step is looked up or run for another (`nest::looked`, `nest::asked`), before its
    /// reader is posted. Once a row: later calls find it said.
    pub(crate) fn link(&mut self, row: u32, step: StepId, input: &[u8]) {
        if self.rows[row as usize].name.is_some() {
            return;
        }
        self.scratch.clear();
        entry::name_kind(ReadKind::STEP, &mut self.scratch);
        // alloc: the session's one buffer for a name, reused
        self.scratch.extend_from_slice(&step.0.to_be_bytes());
        // alloc: the session's one buffer for a name, reused
        self.scratch.extend_from_slice(input);
        let scratch = std::mem::take(&mut self.scratch);
        let at = self.name_at(&scratch);
        self.scratch = scratch;
        let post = self.posts.get_index_mut(at).expect("a name found or put just now").1;
        post.row = Some(row);
        let at = u32::try_from(at).expect("a session's names are counted in u32");
        if !post.dirty {
            post.dirty = true;
            // alloc: the names the next save writes
            self.dirty_posts.push(at);
        }
        self.rows.get_index_mut(row as usize).expect("a row the caller was given").1.name = Some(at);
        self.touch(row);
    }

    /// The key a read of the value of `row` names it by, once a step read it.
    pub(crate) fn asked(&self, row: u32) -> Option<&[u8]> {
        let name = self.rows.get_index(row as usize)?.1.name?;
        let (name, _) = self.posts.get_index(name as usize)?;
        Some(&name[2..])
    }

    /// The identity `row` is of, where the session has the row.
    pub(crate) fn id_at(&self, row: u32) -> Option<ContentId> {
        self.rows.get_index(row as usize).map(|(id, _)| *id)
    }

    /// The hash of the value of `row`, where it is live and a step read it.
    pub(crate) fn hash_of(&self, row: u32) -> Option<ContentId> {
        self.live_of(row)?.known.hash
    }

    pub(crate) fn is_live(&self, row: u32) -> bool {
        self.live_of(row).is_some()
    }

    /// The key of the entry of `row`, where it is live.
    pub(crate) fn key_of(&self, row: u32) -> Option<Key> {
        Some(self.live_of(row)?.known.key)
    }

    /// The place in `posts` of the read named `name`, with the readers the store keeps of it
    /// read in: none where neither this process nor the store has a posting of it.
    pub(crate) fn name_readers<G: Get + ?Sized>(
        &mut self,
        store: &G,
        name: &[u8],
    ) -> Result<Option<usize>, KernelError> {
        if let Some(at) = self.posts.get_index_of(name) {
            self.load_post(store, at)?;
            return Ok(Some(at));
        }
        // A name this process has not met: its readers are the store's.
        let Some(bytes) = Session::stored_post(&self.kinds, &mut self.nodes, store, name)? else {
            return Ok(None);
        };
        let at = self.name_at(name);
        self.merge_post(at, &bytes)?;
        self.posts.get_index_mut(at).expect("a name made just now").1.loaded = true;
        Ok(Some(at))
    }

    /// The `n`th reader of the read at `at` in `posts`, live or not: none past the last.
    pub(crate) fn reader_at(&self, at: usize, n: usize) -> Option<u32> {
        self.posts.get_index(at)?.1.readers.get(n).copied()
    }

    /// Reads in the readers the store keeps of `row`, before a carry asks for them
    /// ([`Session::reader`]): the path of one posting, where it is not in memory.
    pub(crate) fn load_readers<G: Get + ?Sized>(&mut self, store: &G, row: u32) -> Result<(), KernelError> {
        match self.rows.get_index(row as usize).and_then(|(_, it)| it.name) {
            Some(name) => self.load_post(store, name as usize),
            None => Ok(()),
        }
    }

    /// The `n`th reader of `row`, live or not: none past the last, and none for a row no step
    /// read.
    pub(crate) fn reader(&self, row: u32, n: usize) -> Option<u32> {
        let name = self.rows.get_index(row as usize)?.1.name?;
        self.posts.get_index(name as usize)?.1.readers.get(n).copied()
    }

    /// Drops the live row `row` and queues it for the carry's repair, at its height, with the
    /// hash its readers saw. A row that is not live is left: it is queued already, or dropped.
    ///
    /// A row known only as a reader is read from `store` first; a row dropped here is written
    /// by the next save as dropped, so a session opened from it does not hold it live.
    pub(crate) fn dirty<G: Get + ?Sized>(
        &mut self,
        store: &G,
        row: u32,
        queue: &mut Queue,
    ) -> Result<bool, KernelError> {
        let unread = match self.rows.get_index(row as usize) {
            Some((id, it)) if !it.loaded => Some(*id),
            Some(_) => None,
            None => return Ok(false),
        };
        if let Some(id) = unread {
            self.load_row(store, &id)?;
        }
        let it = self.rows.get_index_mut(row as usize).expect("the row found above");
        let Some(live) = it.1.live.take() else {
            return Ok(false);
        };
        if !it.1.dirty {
            it.1.dirty = true;
            // alloc: the rows the next save writes
            self.dirty_rows.push(row);
        }
        self.live -= 1;
        // alloc: one carry's queue
        queue.order.insert((live.height, row));
        // The first hash stays while the row is queued: it is what every reader not yet
        // queued was verified against.
        // alloc: one carry's queue
        queue.was.entry(row).or_insert((live.known.key, live.known.hash));
        Ok(true)
    }

    /// The start of a carry to `changed.to`: refuses a change set that is not from the
    /// session's pin, not to the world's, or not total; drops the session whole where the
    /// registry is another; else moves the pin and gives the rows `changed` reaches directly,
    /// dropped and queued. None: the session was dropped whole.
    ///
    /// Total (invariant I2): `changed` says, for every kind of read the session has a posting
    /// of, either the exact names that differ or that every one may. A kind it does not
    /// mention is a refusal naming the kind, never read as "nothing changed".
    ///
    /// The postings of each changed name are read from `store` where they are not in memory
    /// (one path a name). A store that fails part way drops the session whole: some rows were
    /// dropped and their readers not yet reached.
    pub(crate) fn seed<G: Get + ?Sized>(
        &mut self,
        store: &G,
        registry: ContentId,
        now: Pin,
        changed: &Changed,
    ) -> Result<Option<Queue>, KernelError> {
        if changed.from != self.pin {
            return Err(KernelError::Carry(CarryError::From {
                session: self.pin,
                from: changed.from,
            }));
        }
        if changed.to != now {
            return Err(KernelError::Moved { pinned: changed.to, now });
        }
        if changed.kinds.iter().any(|(kind, _)| ReadKind(*kind).is_kernels()) {
            return Err(KernelError::Carry(CarryError::Steps));
        }
        if self.registry.is_some_and(|had| had != registry) {
            self.clear();
            self.registry = Some(registry);
            self.pin = changed.to;
            return Ok(None);
        }
        self.registry = Some(registry);
        let unsaid = self
            .kinds
            .iter()
            .filter(|(kind, names)| {
                !ReadKind(**kind).is_kernels()
                    && (!names.names.is_empty() || names.root.is_some())
                    && !changed.kinds.contains_key(*kind)
            })
            .map(|(kind, _)| *kind)
            .min();
        if let Some(kind) = unsaid {
            // The count is of every name of the kind, in memory or in the store.
            self.load_kind(store, kind)?;
            let names = self.kinds[&kind].names.len();
            return Err(KernelError::Carry(CarryError::Unsaid { kind: ReadKind(kind), names }));
        }
        let mut queue = Queue::new(self.live.saturating_add(self.rows.len()));
        if let Err(error) = self.seed_rows(store, changed, &mut queue) {
            self.clear();
            return Err(error);
        }
        self.pin = changed.to;
        #[cfg(test)]
        {
            // The planted bug's postings, written now: after the seed that needed them.
            for (at, row) in std::mem::take(&mut self.late) {
                let post = self.posts.get_index_mut(at as usize).expect("a name posted late").1;
                if !post.readers.contains(&row) {
                    post.readers.push(row);
                }
            }
        }
        Ok(Some(queue))
    }

    /// Drops and queues the rows `changed` reaches directly.
    fn seed_rows<G: Get + ?Sized>(
        &mut self,
        store: &G,
        changed: &Changed,
        queue: &mut Queue,
    ) -> Result<(), KernelError> {
        // In order of kind (rule 25): the rows are numbered as they are read.
        // alloc: one carry's kinds, in order
        let mut kinds: Vec<u16> = changed.kinds.iter().map(|(kind, _)| *kind).collect();
        kinds.sort_unstable();
        for kind in kinds {
            match &changed.kinds[&kind] {
                Difference::Exact(names) => {
                    for name in names {
                        let Some(at) = self.name_readers(store, name)? else {
                            continue;
                        };
                        self.dirty_readers(store, at, queue)?;
                    }
                }
                Difference::Everything => {
                    self.load_kind(store, kind)?;
                    let names = self.kinds.get(&kind).map_or(0, |names| names.names.len());
                    for n in 0..names {
                        let at = self.kinds[&kind].names[n] as usize;
                        self.dirty_readers(store, at, queue)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Queues every live reader of the read at `at` in `posts`.
    fn dirty_readers<G: Get + ?Sized>(
        &mut self,
        store: &G,
        at: usize,
        queue: &mut Queue,
    ) -> Result<(), KernelError> {
        let readers = self.posts.get_index(at).map_or(0, |(_, post)| post.readers.len());
        for n in 0..readers {
            let row = self.posts.get_index(at).expect("a name counted just now").1.readers[n];
            self.dirty(store, row, queue)?;
        }
        Ok(())
    }

    /// The root of the postings of `kind` in the store, as the last save left it.
    #[cfg(test)]
    pub(crate) fn posts_root(&self, kind: ReadKind) -> Option<Key> {
        self.kinds.get(&kind.0).and_then(|names| names.root)
    }

    /// How many rows and how many names are in memory.
    #[cfg(test)]
    pub(crate) fn held(&self) -> (usize, usize) {
        assert_eq!(self.nodes.len(), 0, "a node of the store's maps held past a request or a carry");
        assert_eq!(self.tables.len(), 0, "a table held past a request or a carry");
        (self.rows.len(), self.posts.len())
    }

    /// Itself, for a test that goes back to an earlier state.
    #[cfg(test)]
    pub(crate) fn saved(&self) -> Session {
        // clone: a test's copy of the session
        self.clone()
    }

    /// The session `saved` gave.
    #[cfg(test)]
    pub(crate) fn restored(saved: &Session) -> Session {
        // clone: a test's copy of the session
        saved.clone()
    }
}

/// The rows a carry has still to repair: lowest first, each with the hash its readers saw
/// before the carry reached it. One carry's memory, freed when it ends.
pub(crate) struct Queue {
    order: BTreeSet<(u32, u32)>,
    was: blaze_hash::HashMap<u32, (Key, Option<ContentId>), Global>,
    /// How many rows may still be taken (rule 18): [`Popped::Spent`] past it.
    left: usize,
    /// A planted bug's rows repaired already, which it does not queue again.
    #[cfg(test)]
    done: Vec<u32>,
}

/// What [`Queue::pop`] gives.
pub(crate) enum Popped {
    /// The lowest row queued, the hash its readers saw, and the key of the entry they saw it
    /// in: what a table's parts are compared against ([`crate::table::diff`]).
    Row { row: u32, was: Option<ContentId>, key: Key },
    Empty,
    /// More rows were taken than a carry of this session may repair.
    Spent,
}

impl Queue {
    /// The bound: a carry whose heights are right repairs a row once, and one that met a row
    /// below a taller child repairs it again after that child, so a few times the session's
    /// rows is far from any carry that ends; past it the session is dropped whole
    /// ([`Dropped::Repairs`]).
    fn new(rows: usize) -> Queue {
        Queue {
            // alloc: one carry's queue; empty where nothing the session read changed
            order: BTreeSet::new(),
            // alloc: one carry's queue; empty where nothing the session read changed
            was: blaze_hash::HashMap::new_in(Global),
            left: rows.saturating_mul(8).saturating_add(64),
            #[cfg(test)]
            done: Vec::new(),
        }
    }

    /// How many rows are queued.
    pub(crate) fn len(&self) -> usize {
        self.was.len()
    }

    /// The lowest row queued (invariant I3: a row is repaired after every queued row it reads,
    /// which are lower).
    pub(crate) fn pop(&mut self) -> Popped {
        loop {
            let next = if Queue::planted_parent_first() {
                self.order.pop_last()
            } else {
                self.order.pop_first()
            };
            let Some((_, row)) = next else {
                return Popped::Empty;
            };
            // A row queued twice, at two heights, is repaired at the first.
            let Some((key, was)) = self.was.remove(&row) else {
                continue;
            };
            if self.left == 0 {
                return Popped::Spent;
            }
            self.left -= 1;
            #[cfg(test)]
            {
                if Queue::planted_parent_first() {
                    self.done.push(row);
                }
            }
            return Popped::Row { row, was, key };
        }
    }

    /// Whether a changed row's reader `row` is to be queued: always, but for a planted bug.
    pub(crate) fn takes(&self, row: u32) -> bool {
        #[cfg(test)]
        {
            if Queue::planted_parent_first() && self.done.contains(&row) {
                return false;
            }
        }
        let _ = row;
        true
    }

    fn planted_parent_first() -> bool {
        #[cfg(test)]
        {
            if planted::is(Bug::ParentFirst) {
                return true;
            }
        }
        false
    }
}

/// What differs between two pins of a world, as whoever moved the world says it: the reads a
/// carry starts from ([`crate::Kernel::carry`]).
///
/// It is total by kind of read (invariant I2). For each kind the world answers, it says
/// either the exact reads that differ ([`Changed::exact`], [`Changed::differs`]) or that any
/// read of the kind may ([`Changed::everything`]: a kind with no exact difference, such as a
/// fact valued by something that keeps no history). A read that differs is one whose
/// [`World::hash`] at `to` is not what it was at `from`: a hash for another hash, a thing
/// that appeared where nothing was ([`crate::Seen::Gone`] before) or went, and a read that
/// can or cannot be valued where before it was the other. Postings are by name, so a name
/// nothing was under is covered like any other, as long as it is said here.
///
/// The kernel cannot check that the names are all of them: that is the one thing a carry
/// trusts, and what a driver's own tests are for (`exhaustive/nested.rs` has the control: a
/// fact that appears and is left out ends stale).
pub struct Changed {
    from: Pin,
    to: Pin,
    kinds: blaze_hash::HashMap<u16, Difference, Global>,
}

enum Difference {
    /// The names (kind and key) of the reads that differ.
    Exact(Vec<Box<[u8]>>),
    Everything,
}

impl Changed {
    /// The difference between the world at `from` and at `to`, with no kind said yet.
    pub fn between(from: Pin, to: Pin) -> Changed {
        // alloc: one carry's change set
        Changed { from, to, kinds: blaze_hash::HashMap::new_in(Global) }
    }

    /// Says the difference of `kind` is exact: only the reads given to [`Changed::differs`].
    pub fn exact(&mut self, kind: ReadKind) {
        // alloc: one carry's change set
        self.kinds.entry(kind.0).or_insert_with(|| Difference::Exact(Vec::new()));
    }

    /// Says `read` differs between the two pins. Its kind's difference is exact, unless
    /// [`Changed::everything`] was said of it.
    pub fn differs(&mut self, read: Read<'_>) {
        // alloc: one carry's change set
        let kind = self.kinds.entry(read.kind.0).or_insert_with(|| Difference::Exact(Vec::new()));
        if let Difference::Exact(names) = kind {
            // alloc: one carry's change set, one name a read that differs
            let mut name = Vec::with_capacity(2 + read.key.len());
            entry::name_kind(read.kind, &mut name);
            name.extend_from_slice(read.key);
            names.push(name.into_boxed_slice());
        }
    }

    /// Says any read of `kind` may differ: every entry with a read of that kind is looked up
    /// again.
    pub fn everything(&mut self, kind: ReadKind) {
        // alloc: one carry's change set
        self.kinds.insert(kind.0, Difference::Everything);
    }

    /// Whether `read` may differ between the two pins: it was said to differ, or everything
    /// of its kind was. A read of a kind nothing was said of does not (a carry refuses a
    /// change set that leaves out a kind its session has reads of).
    pub(crate) fn may_differ(&self, read: Read<'_>) -> bool {
        match self.kinds.get(&read.kind.0) {
            None => false,
            Some(Difference::Everything) => true,
            // Linear in the reads said to differ of this kind: the change set of one carry.
            Some(Difference::Exact(names)) => names.iter().any(|name| &name[2..] == read.key),
        }
    }

    /// The pin it is from and the pin it is to.
    pub fn pins(&self) -> (Pin, Pin) {
        (self.from, self.to)
    }
}

/// Why a carry was refused before it changed anything: the session is as it was, at its pin.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CarryError {
    /// The change set is from another pin than the session's.
    From { session: Pin, from: Pin },
    /// The change set does not say what differs for a kind of read the session has: `names`
    /// reads of that kind are posted.
    Unsaid { kind: ReadKind, names: usize },
    /// The change set names reads of a step's value, which the kernel answers itself.
    Steps,
}

impl fmt::Display for CarryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CarryError::From { session, from } => write!(
                f,
                "a session of the world pinned at {:x} was given the changes from the pin {:x}: a carry starts from the session's own pin",
                session.0, from.0
            ),
            CarryError::Unsaid { kind, names } => write!(
                f,
                "the changes do not say what differs for reads of kind {} ({}), and the session's entries read {names} of them: say the reads that differ (Changed::exact, Changed::differs) or that any may (Changed::everything)",
                kind.0,
                hex(&kind.0.to_be_bytes())
            ),
            CarryError::Steps => write!(
                f,
                "the changes name reads of a kind from {} up, a step's value or a part of it: the kernel values those itself, from the reads the world answers",
                ReadKind::TABLE.0
            ),
        }
    }
}

/// Why a carry dropped everything the session had verified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dropped {
    /// The kernel's registry of steps is not the one the session was verified under: a step's
    /// code, or which steps there are, is an input of every row.
    Registry,
    /// The repair took more rows than a carry of this session may (rule 18).
    Repairs,
}

/// What a carry did ([`crate::Kernel::carry`]): the counts an edit is reported with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Carried {
    /// Set where the session was dropped whole: it holds nothing, at the new pin.
    pub dropped: Option<Dropped>,
    /// Entries the change set reached directly: one of their own reads differs.
    pub seeded: usize,
    /// Entries looked up again (found in the store, else run).
    pub repaired: usize,
    /// Of those, the entries whose value is other bytes now: their readers were queued.
    pub changed: usize,
    /// Of those, the entries whose value is the same bytes: their readers stayed verified.
    pub cut: usize,
    /// Entries dropped from the session: no step had read them (they are looked up when next
    /// asked for), or they cannot be valued at the new pin.
    pub gone: usize,
}
