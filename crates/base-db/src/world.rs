//! The driver's world and the snapshot made from it (docs/v2/plan/design/front-end-on-memo.md,
//! "base-db: the world and the snapshot").
//!
//! The world is plain owned data by stable key: files and source roots by [`FileId`] and
//! [`SourceRootId`] (kept by [`Files`]), crates by [`CrateKey`] ([`Crates`]), and the session's
//! settings, environment and root lists ([`Session`]). The snapshot ([`Snap`]) is a
//! [`blaze_memo::Memo`] whose inputs are made from the world, and a [`Table`] from each key to
//! the input that holds it. A commit ([`Db::commit`](crate::Db)) compares what was written with
//! what the world held (an equal digest marks nothing), marks the arenas holding what moved,
//! asks the memo for the next snapshot, makes the dropped arenas' inputs again from the world
//! and rebuilds those arenas' table rows.
//!
//! Arenas: index 0 is the session arena (settings, process environment, library and local
//! roots, named paths, the crate list's shape, and the files of a source root no crate is
//! rooted in); a crate's index is `1 +` its place in [`Crates::order`], every crate after the
//! crates it depends on. A source root's files live in the arena of the lowest-index crate
//! rooted in it (its owner); every other crate rooted there borrows the owner. A crate's
//! attribute files live in its own arena. Each crate arena borrows its dependencies and the
//! session; a crate whose sources are pinned ([`crate::CrateData::sealed`]) and whose
//! dependencies are all sealed is sealed.
//!
//! Cost (rule 67). A commit is O(entries written) to mark, O(crates + roots) to lay the arenas
//! out again (the order, each root's owner: a vector walk, within the `Memo::next` floor of
//! O(arenas)), and O(inputs of the dropped arenas) to make them again: the floor is the
//! dropped arenas' inputs, as nothing carried is touched. A lookup is one index of a vector.
//! When no reader holds the old snapshot its table is taken over in place; while one does,
//! the table is copied (O(files + crates), a vector copy), which a resident helper pays only
//! when a reader outlives a commit.

use std::{alloc::Global, any::TypeId, collections::BTreeSet};

use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use blaze_lint_defs::LintOptions;
use blaze_mem::Tag;
use blaze_memo::{ArenaId, Builder, Fact, Inputs, Memo};
use cfg::{CfgOptions, HashableCfgOptions};
use span::FileId;
use triomphe::Arc;

use crate::{
    AllCrates, BuiltCrateData, Crate, CrateBuilderId, CrateGraphBuilder, CrateSlot,
    CrateWorkspaceData, CratesIdMap, Env, ExtraCrateData, Files, Given, LibraryRoots,
    LocalRoots, ProcessEnv, RawFile, SourceRootId, VfsFileRoot, VfsPath, VfsRoot, VfsText,
    content::InputId,
    input::{CrateData, Dependency},
    settings::FrontSettingsData,
};

/// A crate's key: given when the world first has the crate, never reused in a session.
/// Callers keep keys across a commit, never [`Crate`] ids (an id of a dropped arena panics on
/// use).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CrateKey(pub(crate) u32);

impl CrateKey {
    /// The key as a number, for a test's or a message's eyes.
    pub fn index(self) -> u32 {
        self.0
    }
}

/// A crate's data as the world keeps it: its dependencies by key.
pub type WorldCrateData = CrateData<CrateKey>;

/// The facts the world's inputs are made with: each input's id is of its key (stable across
/// snapshots and processes), its digest of its content.
pub(crate) mod facts {
    use blaze_ids::ContentId;
    use blaze_memo::Fact;
    use span::FileId;

    use crate::{SourceRootId, content::InputId};

    pub(crate) const TEXT: &str = "blaze v2 file text fact 1";
    pub(crate) const RAW: &str = "blaze v2 raw file fact 1";
    pub(crate) const FILE_ROOT: &str = "blaze v2 file root fact 1";
    pub(crate) const ROOT: &str = "blaze v2 source root fact 1";
    pub(crate) const CRATE: &str = "blaze v2 crate fact 1";
    pub(crate) const SLOT: &str = "blaze v2 crate slot fact 1";
    pub(crate) const GIFT: &str = "blaze v2 crate gift 1";
    pub(crate) const SINGLETON: &str = "blaze v2 singleton fact 1";

    fn of_file(domain: &'static str, file: FileId) -> ContentId {
        ContentId::of(domain, &file.index().to_le_bytes())
    }

    pub(crate) fn text(file: FileId, content: ContentId) -> Fact {
        Fact { id: of_file(TEXT, file), digest: content }
    }

    pub(crate) fn raw(file: FileId, content: ContentId) -> Fact {
        Fact { id: of_file(RAW, file), digest: content }
    }

    pub(crate) fn file_root(file: FileId, root: SourceRootId) -> Fact {
        Fact {
            id: of_file(FILE_ROOT, file),
            digest: ContentId::of(FILE_ROOT, &root.0.to_le_bytes()),
        }
    }

    pub(crate) fn root(root: SourceRootId, content: ContentId) -> Fact {
        Fact { id: ContentId::of(ROOT, &root.0.to_le_bytes()), digest: content }
    }

    /// A crate's: its id of its content ID, as an image's key names a crate.
    pub(crate) fn krate(content: ContentId) -> Fact {
        Fact { id: ContentId::of(CRATE, &content.0), digest: content }
    }

    /// A gift's: of the crate's content and the kind's type name, its digest the gift's
    /// number ([`super::Gift::digest`]).
    pub(crate) fn slot(krate: ContentId, kind: &'static str, digest: ContentId) -> Fact {
        let mut bytes = Vec::with_capacity_in(32 + kind.len(), std::alloc::Global);
        bytes.extend_from_slice(&krate.0);
        bytes.extend_from_slice(kind.as_bytes());
        Fact { id: ContentId::of(SLOT, &bytes), digest }
    }

    /// A session singleton's: its id of its domain, its digest its content.
    pub(crate) fn singleton(domain: &'static str, content: InputId) -> Fact {
        Fact { id: ContentId::of(SINGLETON, domain.as_bytes()), digest: content.id() }
    }
}

/// What the world's files moved since the last commit ([`Files::marks`]).
#[derive(Default, Debug, Clone)] // clone: a commit's copy, O(what the edit wrote)
pub(crate) struct FileMarks {
    /// Files whose text, bytes, appended module or source root were written with another value.
    pub(crate) files: Vec<FileId, Global>,
    /// Source roots whose set of files or content moved, and each root a file left.
    pub(crate) roots: Vec<SourceRootId, Global>,
    /// A session datum of the files moved: a named-path bucket, an included file's source.
    pub(crate) session: bool,
    /// Rows of files read outside the VFS whose read moved.
    pub(crate) included: Vec<u32, Global>,
}

impl FileMarks {
    /// No marks, each list keeping its capacity for the next edit's.
    pub(crate) fn clear(&mut self) {
        self.files.clear();
        self.roots.clear();
        self.session = false;
        self.included.clear();
    }

    /// `self` set to `other`, in the capacity `self` has.
    pub(crate) fn set_to(&mut self, other: &FileMarks) {
        self.clear();
        self.files.extend_from_slice(&other.files);
        self.roots.extend_from_slice(&other.roots);
        self.session = other.session;
        self.included.extend_from_slice(&other.included);
    }
}

/// One thing given to one crate ([`crate::Edit::give`], [`crate::Edit::give_by`]).
#[derive(Clone)] // clone: a crate's few gifts, when the table is copied for a reader
pub(crate) struct Gift {
    pub(crate) kind: TypeId,
    pub(crate) name: &'static str,
    pub(crate) value: Given,
    pub(crate) fact: GiftFact,
}

/// What a gift's slot is read by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GiftFact {
    /// A number drawn per gift that moved ([`crate::Edit::give`]), the digest of
    /// [`facts::slot`]: two gifts are equal facts only when they are one value ([`Given`]'s
    /// equality). Of this process alone.
    Drawn(ContentId),
    /// The giver's own fact ([`crate::Edit::give_by`]): stable wherever the giver's is, so a
    /// read list naming it can be filed and read in another process.
    Given(Fact),
}

impl Gift {
    /// The fact its slot is made with in the arena of a crate whose content ID is `krate`.
    fn slot_fact(&self, krate: ContentId) -> Fact {
        match self.fact {
            GiftFact::Drawn(digest) => facts::slot(krate, self.name, digest),
            GiftFact::Given(fact) => fact,
        }
    }
}

/// What the driver has made in one arena each time that arena is made
/// ([`crate::Edit::set_given`]): inputs of a crate above base-db, set in that arena by `make`,
/// read by `fact`.
pub(crate) struct Maker {
    pub(crate) fact: Fact,
    pub(crate) make: crate::db::MakeGiven,
}

/// Puts `maker` in `makers` (sorted by fact id), in place of one of the same fact id: whether
/// anything moved, with the maker it replaced (`None`: it had none of that fact id), for undo.
/// One of an equal fact moves nothing and is dropped: the giver promises that two makers under
/// one fact make inputs no reader tells apart.
pub(crate) fn put_maker(makers: &mut Vec<Maker, Global>, maker: Maker) -> Option<Option<Maker>> {
    match makers.binary_search_by(|it| it.fact.id.cmp(&maker.fact.id)) {
        Ok(at) if makers[at].fact == maker.fact => None,
        Ok(at) => Some(Some(std::mem::replace(&mut makers[at], maker))),
        Err(at) => {
            makers.insert(at, maker);
            Some(None)
        }
    }
}

/// Puts back what [`put_maker`] replaced under the fact id `id`: `was`, or no maker.
fn unput_maker(makers: &mut Vec<Maker, Global>, id: ContentId, was: Option<Maker>) {
    let at = makers.binary_search_by(|it| it.fact.id.cmp(&id));
    match (at, was) {
        (Ok(at), Some(was)) => makers[at] = was,
        (Ok(at), None) => _ = makers.remove(at),
        (Err(at), Some(was)) => makers.insert(at, was),
        (Err(_), None) => {}
    }
}

/// A write of the world's crates or session, with what it replaced: see [`World::rollback`].
pub(crate) enum Undo {
    /// The crate was made: undone, its key holds no crate (keys are never reused).
    Made(CrateKey),
    /// A field of the crate was set: the value it had.
    Field(CrateKey, Field),
    /// The crate's content ID moved: the one it had.
    Content(CrateKey, ContentId),
    /// The crate's attribute files were set: the list it had.
    AttrFiles(CrateKey, Box<[FileId]>),
    /// An attribute file's crate was set: the crate it had.
    AttrOf(FileId, Option<CrateKey>),
    /// A gift of the kind was put: the gift it replaced.
    Gift(CrateKey, TypeId, Option<Gift>),
    /// A maker of the fact id was put, in the crate's arena or the session's (`None`): the
    /// maker it replaced.
    Maker(Option<CrateKey>, ContentId, Option<Maker>),
    /// Whether the crate is listed moved: what it was.
    Listed(CrateKey, bool),
    /// A crate was pushed onto the order.
    OrderPushed,
    /// The crate was popped off the order.
    OrderPopped(CrateKey),
    /// The order was set whole: the one it had.
    Order(Vec<CrateKey, Global>),
    /// A crate was found by its [`Unique`] from here on.
    Unique(Unique),
    /// A session datum was set: the value it had.
    Session(SessionField),
}

/// A crate's field a writer sets, with its value: what [`Crates::set_field`] writes and an
/// [`Undo::Field`] keeps.
pub(crate) enum Field {
    Data(WorldCrateData),
    Extra(ExtraCrateData),
    Workspace(Arc<CrateWorkspaceData>),
    Cfg(CfgOptions),
    Env(Env),
    Lints(LintOptions),
}

/// A session datum a writer sets, with its value: see [`Undo::Session`].
pub(crate) enum SessionField {
    LibraryRoots(blaze_hash::HashSet<SourceRootId, Global>),
    LocalRoots(blaze_hash::HashSet<SourceRootId, Global>),
    ProcessEnv(Option<Env>),
    Settings(FrontSettingsData),
    Primary(Vec<CrateKey, Global>),
}

/// Runs each of `makers` in the arena `inputs` adds to, in fact id order.
fn run_makers(makers: &[Maker], builder: &Builder, inputs: &Inputs<'_>) {
    for maker in makers {
        (maker.make)(builder, inputs);
    }
}

/// A crate as the world keeps it.
pub(crate) struct CrateEntry {
    pub(crate) data: WorldCrateData,
    pub(crate) extra: ExtraCrateData,
    pub(crate) workspace: Arc<CrateWorkspaceData>,
    pub(crate) cfg: CfgOptions,
    pub(crate) env: Env,
    pub(crate) lints: LintOptions,
    /// Its content ID ([`Crate::content`]), of the fields above and its dependencies'.
    pub(crate) content: ContentId,
    /// The files of its crate attributes ([`crate::crate_attrs`]).
    pub(crate) attr_files: Box<[FileId]>,
    pub(crate) gifts: Vec<Gift, Global>,
    /// What the driver has made in its arena each time it is made, by fact id.
    pub(crate) makers: Vec<Maker, Global>,
    /// Whether it is in the crate graph's list ([`crate::all_crates`]).
    pub(crate) listed: bool,
    /// What a graph's walk dedups it by, when a graph made it.
    unique: Option<Unique>,
}

/// What tells two crates of a graph apart: their root file, and for a second crate on one
/// root file its data and cfg, by key.
#[derive(Clone, PartialEq, Eq, Hash)] // clone: once per crate a graph makes
pub(crate) struct Unique {
    root_file_id: FileId,
    disambiguator: Option<Box<(WorldCrateData, HashableCfgOptions)>>,
}

/// The world's crates.
#[derive(Default)]
pub(crate) struct Crates {
    /// By key; `None` for no crate (never reused).
    entries: Vec<Option<CrateEntry>, Global>,
    /// Every crate, each after those it depends on: its arena's index is 1 + its place.
    pub(crate) order: Vec<CrateKey, Global>,
    by_unique: HashMap<Unique, CrateKey, Global>,
    /// Each crate attribute file's crate.
    pub(crate) attr_of: HashMap<FileId, CrateKey, Global>,
    /// Crates whose entry moved since the last commit.
    pub(crate) dirty: Vec<CrateKey, Global>,
    /// Whether the order or the list moved since the last commit.
    pub(crate) shape_moved: bool,
    /// The builder ids of the last graph set, for [`crate::Committed::crates`].
    pub(crate) set: Option<CratesIdMap>,
    /// Gifts drawn: what each new gift's digest is of. Not put back by an undo: a number
    /// drawn once is never drawn again, so a gift of a dropped edit is no later gift's equal.
    gifts_drawn: u64,
    /// The open edit's writes of the crates and the session, each with what it replaced, the
    /// last last ([`World::rollback`]): one entry per write that moved something, the value
    /// replaced moved out, never copied, but a [`Unique`] (once per crate a graph makes).
    pub(crate) undo: Vec<Undo, Global>,
}

impl Crates {
    pub(crate) fn get(&self, key: CrateKey) -> &CrateEntry {
        match self.entries.get(key.0 as usize) {
            Some(Some(entry)) => entry,
            _ => panic!("the world has no crate {key:?}"),
        }
    }

    fn get_mut(&mut self, key: CrateKey) -> &mut CrateEntry {
        match self.entries.get_mut(key.0 as usize) {
            Some(Some(entry)) => entry,
            _ => panic!("the world has no crate {key:?}"),
        }
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = CrateKey> + '_ {
        self.order.iter().copied()
    }

    fn insert(&mut self, entry: CrateEntry) -> CrateKey {
        let key = CrateKey(u32::try_from(self.entries.len()).expect("fewer than 2^32 crates"));
        self.entries.push(Some(entry));
        self.undo.push(Undo::Made(key));
        self.order.push(key);
        self.undo.push(Undo::OrderPushed);
        self.dirty.push(key);
        self.shape_moved = true;
        key
    }

    /// Sets `key`'s listed flag, kept for undo: whether it moved.
    fn set_listed(&mut self, key: CrateKey, listed: bool) -> bool {
        let entry = self.get_mut(key);
        if entry.listed == listed {
            return false;
        }
        entry.listed = listed;
        self.undo.push(Undo::Listed(key, !listed));
        true
    }

    /// Sets `key`'s content ID, kept for undo.
    fn set_content(&mut self, key: CrateKey, content: ContentId) {
        let was = std::mem::replace(&mut self.get_mut(key).content, content);
        self.undo.push(Undo::Content(key, was));
    }

    /// Sets one of `key`'s fields to `field`'s value, kept for undo: whether it moved (an
    /// equal value writes nothing).
    pub(crate) fn put_field(&mut self, key: CrateKey, field: Field) -> bool {
        let entry = self.get_mut(key);
        let was = match field {
            Field::Data(it) if entry.data != it => Field::Data(std::mem::replace(&mut entry.data, it)),
            Field::Extra(it) if entry.extra != it => Field::Extra(std::mem::replace(&mut entry.extra, it)),
            Field::Workspace(it) if *entry.workspace != *it => {
                Field::Workspace(std::mem::replace(&mut entry.workspace, it))
            }
            Field::Cfg(it) if entry.cfg != it => Field::Cfg(std::mem::replace(&mut entry.cfg, it)),
            Field::Env(it) if entry.env != it => Field::Env(std::mem::replace(&mut entry.env, it)),
            Field::Lints(it) if entry.lints != it => Field::Lints(std::mem::replace(&mut entry.lints, it)),
            _ => return false,
        };
        self.undo.push(Undo::Field(key, was));
        true
    }

    /// The content ID of `data` and the rest, its dependencies' read from the world.
    #[allow(clippy::too_many_arguments, reason = "a crate's fields")]
    fn content_of(
        &self,
        files: &Files,
        data: &WorldCrateData,
        extra: &ExtraCrateData,
        workspace: &CrateWorkspaceData,
        cfg: &CfgOptions,
        env: &Env,
        lints: &LintOptions,
    ) -> ContentId {
        let root = files.path_of(data.root_file_id).unwrap_or_else(|| {
            panic!(
                "the root file {:?} of a crate is in no source root: set the source roots before \
                 the crates",
                data.root_file_id
            )
        });
        crate::input::keyed_content(&root, data, extra, workspace, cfg, env, lints, &|dep| {
            self.get(dep).content
        })
    }

    /// Makes a crate of `data` and the rest: put after every crate there, unlisted.
    pub(crate) fn create(
        &mut self,
        files: &Files,
        data: WorldCrateData,
        extra: ExtraCrateData,
        workspace: Arc<CrateWorkspaceData>,
        cfg: CfgOptions,
        env: Env,
    ) -> CrateKey {
        for dep in &data.dependencies {
            _ = self.get(dep.crate_id);
        }
        let lints = LintOptions::default();
        let content = self.content_of(files, &data, &extra, &workspace, &cfg, &env, &lints);
        let key = self.insert(CrateEntry {
            data,
            extra,
            workspace,
            cfg,
            env,
            lints,
            content,
            attr_files: Box::new([]),
            gifts: Vec::new_in(Global),
            makers: Vec::new_in(Global),
            listed: false,
            unique: None,
        });
        self.refresh_attrs(files, key, false);
        key
    }

    /// Puts `added`, crates just made, into the list after the crates there.
    ///
    /// # Panics
    ///
    /// When one is listed already.
    pub(crate) fn add(&mut self, added: &[CrateKey]) {
        for &key in added {
            assert!(!self.get(key).listed, "`Edit::add_crates`: the crate {key:?} is in the crate graph already");
            self.set_listed(key, true);
        }
        self.shape_moved |= !added.is_empty();
    }

    /// Lists exactly `keys`.
    pub(crate) fn set_list(&mut self, keys: &[CrateKey]) {
        let keep: HashSet<CrateKey, Global> = keys.iter().copied().collect();
        for at in 0..self.order.len() {
            let key = self.order[at];
            if self.set_listed(key, keep.contains(&key)) {
                self.shape_moved = true;
            }
        }
    }

    /// Sets one of a crate's fields, then its content ID and every dependent's whose moved
    /// with it. Marks what moved; an equal value writes nothing.
    pub(crate) fn set_field(&mut self, files: &Files, key: CrateKey, field: Field) {
        let attrs = matches!(&field, Field::Data(data) if data.crate_attrs != self.get(key).data.crate_attrs);
        let deps = matches!(&field, Field::Data(data) if data.dependencies != self.get(key).data.dependencies);
        if !self.put_field(key, field) {
            return;
        }
        self.dirty.push(key);
        if attrs {
            self.refresh_attrs(files, key, false);
        }
        if deps {
            // A dependency gained may be placed after the crate: the walk below, and every
            // snapshot's layout, read the order as each crate after its dependencies.
            self.settle_order(key);
        }
        self.refresh_contents(files, key);
    }

    /// Makes `key`'s content ID again, and in order every dependent's whose moved.
    fn refresh_contents(&mut self, files: &Files, key: CrateKey) {
        let mut moved: HashSet<CrateKey, Global> = HashSet::new_in(Global);
        let start = self.order.iter().position(|it| *it == key).expect("a crate of the order");
        self.refresh_from(files, start, &mut moved, &|it, _| it == key);
    }

    /// Makes again, from place `start` of the order on, the content ID of each crate `first`
    /// names and of each that depends on a crate of `moved`, adding each whose ID moved to
    /// `moved`. The order has each crate after its dependencies, so one walk sees every
    /// dependency's new ID before its dependents. Every crate of the world is kept so, listed
    /// or not: a crate left out of the graph comes back with the ID its fields and its
    /// dependencies' IDs give (rule keys-cover-every-input).
    fn refresh_from(
        &mut self,
        files: &Files,
        start: usize,
        moved: &mut HashSet<CrateKey, Global>,
        first: &dyn Fn(CrateKey, &CrateEntry) -> bool,
    ) {
        for at in start..self.order.len() {
            let it = self.order[at];
            let entry = self.get(it);
            if !first(it, entry) && !entry.data.dependencies.iter().any(|dep| moved.contains(&dep.crate_id)) {
                continue;
            }
            // A crate out of the list whose root file is in no source root has no ID to make
            // (its root's path is part of it): it is made again when the root it comes back
            // with is set ([`Crates::refresh_rooted`]). A listed one has its root, or this panics.
            if !entry.listed && files.root_of(entry.data.root_file_id).is_none() {
                continue;
            }
            let content = self.content_of(
                files,
                &entry.data,
                &entry.extra,
                &entry.workspace,
                &entry.cfg,
                &entry.env,
                &entry.lints,
            );
            if content != entry.content {
                self.set_content(it, content);
                self.dirty.push(it);
                moved.insert(it);
                self.shape_moved |= self.get(it).listed;
            }
        }
    }

    /// Makes again the content ID of each crate whose root file is in one of `roots`, and of
    /// every dependent whose ID moved with it: a crate's ID is of its root file's path, which
    /// a write of a source root moves (a file renamed, a root set again). O(crates) to find
    /// them, then a hash a crate whose root moved; nothing when no root was written.
    pub(crate) fn refresh_rooted(&mut self, files: &Files, roots: &[SourceRootId]) {
        if roots.is_empty() {
            return;
        }
        let mut moved: HashSet<CrateKey, Global> = HashSet::new_in(Global);
        let rooted = |_: CrateKey, entry: &CrateEntry| {
            files.root_of(entry.data.root_file_id).is_some_and(|root| roots.contains(&root))
        };
        self.refresh_from(files, 0, &mut moved, &rooted);
    }

    /// Puts the order back to each crate after its dependencies once `key`'s data moved: a
    /// dependency it gained may be placed after it. Nothing moves when every dependency of
    /// `key` is before it (the common case, one pass over its dependencies); otherwise a
    /// depth-first walk in the old order, so crates keep their places where they can.
    ///
    /// # Panics
    ///
    /// When `key` now depends on itself through its dependencies, naming both: a crate graph
    /// has no cycle.
    fn settle_order(&mut self, key: CrateKey) {
        let place = |order: &[CrateKey], it: CrateKey| order.iter().position(|at| *at == it);
        let at = place(&self.order, key).expect("a crate of the order");
        let deps = &self.get(key).data.dependencies;
        if deps.iter().all(|dep| place(&self.order[..at], dep.crate_id).is_some()) {
            return;
        }
        // Each crate walked: `false` while its dependencies are walked, `true` once placed.
        let mut placed: HashMap<CrateKey, bool, Global> = HashMap::with_capacity_in(self.order.len(), Global);
        let mut sorted: Vec<CrateKey, Global> = Vec::with_capacity_in(self.order.len(), Global);
        let mut stack: Vec<(CrateKey, usize), Global> = Vec::new_in(Global);
        for &root in &self.order {
            if placed.contains_key(&root) {
                continue;
            }
            placed.insert(root, false);
            stack.push((root, 0));
            while let Some(top) = stack.last_mut() {
                let krate = top.0;
                match self.get(krate).data.dependencies.get(top.1) {
                    Some(dep) => {
                        top.1 += 1;
                        let dep = dep.crate_id;
                        match placed.get(&dep) {
                            Some(true) => {}
                            Some(false) => panic!(
                                "`Edit::set_crate_data`: the crate {key:?} now depends on itself \
                                 through {dep:?} and {krate:?}: a crate graph has no cycle"
                            ),
                            None => {
                                placed.insert(dep, false);
                                stack.push((dep, 0));
                            }
                        }
                    }
                    None => {
                        placed.insert(krate, true);
                        sorted.push(krate);
                        stack.pop();
                    }
                }
            }
        }
        let was = std::mem::replace(&mut self.order, sorted);
        self.undo.push(Undo::Order(was));
        self.shape_moved = true;
    }

    /// Writes `key`'s attribute files from its data's attributes (none when `gone`).
    fn refresh_attrs(&mut self, files: &Files, key: CrateKey, gone: bool) {
        let entry = self.get(key);
        let attrs: &[Box<str>] = if gone { &[] } else { &entry.data.crate_attrs };
        let made = files.set_crate_attr_files(key, attrs, &entry.attr_files);
        if *made != *entry.attr_files {
            // The old list is taken out of the entry before `attr_of` is written: the entry
            // gets `made` below.
            let old = std::mem::take(&mut self.get_mut(key).attr_files);
            for &file in old.iter() {
                let was = self.attr_of.remove(&file);
                self.undo.push(Undo::AttrOf(file, was));
            }
            for &file in made.iter() {
                let was = self.attr_of.insert(file, key);
                self.undo.push(Undo::AttrOf(file, was));
            }
            self.get_mut(key).attr_files = made;
            self.undo.push(Undo::AttrFiles(key, old));
            self.dirty.push(key);
        }
        let entry = self.get(key);
        files.set_crate_attr_root(&entry.attr_files, files.root_of(entry.data.root_file_id));
    }

    /// Puts each crate's attribute files in its root file's root again: a root file's root
    /// moves by a write of the files, not of the crate. O(crates with attributes) a commit.
    fn settle_attr_roots(&self, files: &Files) {
        for &key in &self.order {
            let entry = self.get(key);
            if !entry.attr_files.is_empty() {
                files.set_crate_attr_root(&entry.attr_files, files.root_of(entry.data.root_file_id));
            }
        }
    }

    /// Gives `key` `value` as kind `K`, its slot read by a fact drawn for it: whether anything
    /// moved. Two [`Given`] are equal only when they are one value.
    pub(crate) fn give(&mut self, key: CrateKey, kind: TypeId, name: &'static str, value: Given) -> bool {
        if self.get(key).gifts.iter().any(|it| it.kind == kind && it.value == value) {
            return false;
        }
        self.gifts_drawn += 1;
        let digest = ContentId::of(facts::GIFT, &self.gifts_drawn.to_le_bytes());
        self.put_gift(key, Gift { kind, name, value, fact: GiftFact::Drawn(digest) })
    }

    /// Gives `key` `value` as kind `K`, its slot read by the giver's `fact`: whether anything
    /// moved. A gift under the fact the slot holds moves nothing, whatever its value (the
    /// giver's promise: a reader of the slot sees no difference), and the slot keeps the value
    /// it has, so the world and the snapshot hold one value.
    pub(crate) fn give_by(
        &mut self,
        key: CrateKey,
        kind: TypeId,
        name: &'static str,
        fact: Fact,
        value: Given,
    ) -> bool {
        if self.get(key).gifts.iter().any(|it| it.kind == kind && it.fact == GiftFact::Given(fact)) {
            return false;
        }
        self.put_gift(key, Gift { kind, name, value, fact: GiftFact::Given(fact) })
    }

    /// Puts `gift` in `key`'s slot of its kind and marks the crate: a slot that held a gift
    /// before is a write, counted by kind ([`crate::input::count_given`]).
    fn put_gift(&mut self, key: CrateKey, gift: Gift) -> bool {
        let kind = gift.kind;
        let entry = self.get_mut(key);
        let was = match entry.gifts.iter_mut().find(|it| it.kind == gift.kind) {
            Some(held) => {
                crate::input::count_given(gift.name);
                Some(std::mem::replace(held, gift))
            }
            None => {
                entry.gifts.push(gift);
                None
            }
        };
        self.undo.push(Undo::Gift(key, kind, was));
        self.dirty.push(key);
        true
    }

    /// Takes `key`'s gift of kind `kind` away: from the next commit its arena has no slot of
    /// the kind. Whether anything moved: a crate that holds none moves nothing.
    pub(crate) fn take_gift(&mut self, key: CrateKey, kind: TypeId) -> bool {
        let entry = self.get_mut(key);
        let Some(at) = entry.gifts.iter().position(|it| it.kind == kind) else { return false };
        let was = entry.gifts.remove(at);
        self.undo.push(Undo::Gift(key, kind, Some(was)));
        self.dirty.push(key);
        true
    }

    /// The gift of kind `kind` that `key` holds in the world, with this edit's writes.
    pub(crate) fn gift(&self, key: CrateKey, kind: TypeId) -> Option<&Given> {
        self.get(key).gifts.iter().find(|it| it.kind == kind).map(|it| &it.value)
    }

    /// Has `key`'s arena run `maker` each time it is made ([`crate::Edit::set_given`]): whether
    /// anything moved.
    pub(crate) fn set_given(&mut self, key: CrateKey, maker: Maker) -> bool {
        let id = maker.fact.id;
        let Some(was) = put_maker(&mut self.get_mut(key).makers, maker) else { return false };
        self.undo.push(Undo::Maker(Some(key), id, was));
        self.dirty.push(key);
        true
    }

    /// Sets the graph's crates, writing only what differs: a crate is found again by its
    /// [`Unique`], its fields compared, its content ID made again only when a field or a
    /// dependency's moved. Crates of the last graph that this one lacks leave the list.
    pub(crate) fn set_graph(&mut self, files: &Files, graph: CrateGraphBuilder) -> CratesIdMap {
        let ids: Vec<CrateBuilderId, Global> = {
            let mut ids = Vec::new_in(Global);
            ids.extend(graph.iter());
            ids
        };
        let len = ids.iter().map(|id| id.into_raw().into_u32() as usize + 1).max().unwrap_or(0);
        let mut walk = Walk {
            graph: &graph,
            done: {
                let mut done = Vec::with_capacity_in(len, Global);
                done.resize(len, None);
                done
            },
            order: Vec::with_capacity_in(len, Global),
            root_files: HashSet::with_capacity_in(len, Global),
            map: HashMap::with_capacity_in(len, Global),
            moved: HashSet::new_in(Global),
        };
        for &id in &ids {
            walk.go(self, files, id);
        }
        let Walk { order: walked, map, mut moved, .. } = walk;
        let listed = walked.len();
        let walked_set: HashSet<CrateKey, Global> = walked.iter().copied().collect();
        // The crates the graph no longer has leave the list and lose their attribute files.
        let mut rest = Vec::new_in(Global);
        for &key in &self.order {
            if !walked_set.contains(&key) {
                rest.push(key);
            }
        }
        for &key in &rest {
            if self.set_listed(key, false) {
                self.shape_moved = true;
                self.refresh_attrs(files, key, true);
            }
        }
        for &key in &walked {
            if self.set_listed(key, true) {
                self.shape_moved = true;
            }
        }
        let mut order = walked;
        order.extend(rest);
        if order != self.order {
            let was = std::mem::replace(&mut self.order, order);
            self.undo.push(Undo::Order(was));
            self.shape_moved = true;
        }
        // The crates left out keep IDs of their dependencies' IDs: each whose dependency's ID
        // moved in the walk is made again, so it comes back with the ID a fresh world gives.
        // They are after every walked crate (none of those depends on one of them), in their
        // old order. Nothing to do, and no walk, when no ID moved.
        if !moved.is_empty() {
            self.refresh_from(files, listed, &mut moved, &|_, _| false);
        }
        map
    }

    /// Each crate's content ID, in order, of the listed: what the crate list's shape digest is.
    pub(crate) fn list_content(&self) -> ContentId {
        let mut hasher = Hasher::new(crate::CRATE_GRAPH_DOMAIN);
        let listed: Vec<ContentId, Global> = {
            let mut listed = Vec::new_in(Global);
            listed.extend(self.order.iter().map(|&key| self.get(key)).filter(|it| it.listed).map(|it| it.content));
            listed
        };
        crate::content::len(&mut hasher, listed.len());
        for content in &listed {
            hasher.id(content);
        }
        hasher.finish()
    }
}

/// [`Crates::set_graph`]'s walk, each crate after its dependencies.
struct Walk<'g> {
    graph: &'g CrateGraphBuilder,
    done: Vec<Option<CrateKey>, Global>,
    order: Vec<CrateKey, Global>,
    root_files: HashSet<FileId, Global>,
    map: CratesIdMap,
    /// Crates whose content ID moved in this walk.
    moved: HashSet<CrateKey, Global>,
}

impl Walk<'_> {
    fn go(&mut self, crates: &mut Crates, files: &Files, source: CrateBuilderId) -> CrateKey {
        let at = source.into_raw().into_u32() as usize;
        if let Some(done) = self.done[at] {
            return done;
        }
        let krate = &self.graph[source];
        // Each dependency walked first; its key is then in `done`, read in place: an unchanged
        // crate builds no list of them.
        for dep in &krate.basic.dependencies {
            self.go(crates, files, dep.crate_id);
        }
        let done = &self.done;
        let key_of = |id: CrateBuilderId| {
            done[id.into_raw().into_u32() as usize].expect("a dependency is walked before its dependent")
        };
        let root_file_id = krate.basic.root_file_id;
        // A second crate on one root file is keyed by its data, built here once.
        let second = (!self.root_files.insert(root_file_id)).then(|| keyed_data(&krate.basic, &key_of));
        let unique = Unique {
            root_file_id,
            disambiguator: second
                .as_ref()
                // clone: a second crate's data, kept as its key (setup)
                .map(|data| Box::new_in((data.clone(), krate.cfg_options.to_hashable()), Global)),
        };
        let deps_moved = krate.basic.dependencies.iter().any(|dep| self.moved.contains(&key_of(dep.crate_id)));
        let workspace = krate.workspace().clone(); // clone: an `Arc`, once per crate (setup)
        let key = match crates.by_unique.get(&unique).copied() {
            Some(key) => {
                let entry = crates.get(key);
                // Built only when it differs from the data held: compared field by field.
                let data = match second {
                    Some(data) => (entry.data != data).then_some(data),
                    None => (!same_data(&entry.data, &krate.basic, &key_of))
                        .then(|| keyed_data(&krate.basic, &key_of)),
                };
                let extra = (entry.extra != krate.extra).then(|| krate.extra.clone()); // clone: a field that moved (setup)
                let cfg = (entry.cfg != krate.cfg_options).then(|| krate.cfg_options.clone()); // clone: as above
                let env = (entry.env != krate.env).then(|| krate.env.clone()); // clone: as above
                let mut set = false;
                let fields = [data.map(Field::Data), extra.map(Field::Extra), cfg.map(Field::Cfg), env.map(Field::Env)];
                for field in fields.into_iter().flatten() {
                    set |= crates.put_field(key, field);
                }
                set |= crates.put_field(key, Field::Workspace(workspace));
                if set {
                    crates.dirty.push(key);
                }
                if set || deps_moved {
                    let entry = crates.get(key);
                    let content = crates.content_of(
                        files,
                        &entry.data,
                        &entry.extra,
                        &entry.workspace,
                        &entry.cfg,
                        &entry.env,
                        &entry.lints,
                    );
                    if content != entry.content {
                        crates.set_content(key, content);
                        crates.dirty.push(key);
                        crates.shape_moved = true;
                        self.moved.insert(key);
                    }
                }
                key
            }
            None => {
                let data = second.unwrap_or_else(|| keyed_data(&krate.basic, &key_of));
                let lints = LintOptions::default();
                let content = crates.content_of(
                    files,
                    &data,
                    &krate.extra,
                    &workspace,
                    &krate.cfg_options,
                    &krate.env,
                    &lints,
                );
                let key = crates.insert(CrateEntry {
                    data,
                    extra: krate.extra.clone(), // clone: a new crate's fields (setup)
                    workspace,
                    cfg: krate.cfg_options.clone(), // clone: as above
                    env: krate.env.clone(), // clone: as above
                    lints,
                    content,
                    attr_files: Box::new([]),
                    gifts: Vec::new_in(Global),
                    makers: Vec::new_in(Global),
                    listed: true,
                    unique: Some(unique.clone()), // clone: the key, kept with the entry (setup)
                });
                // A new crate is placed by the walk, not after the crates there.
                if let Some(popped) = crates.order.pop() {
                    crates.undo.push(Undo::OrderPopped(popped));
                }
                // clone: the key, kept for undo (setup)
                crates.undo.push(Undo::Unique(unique.clone()));
                crates.by_unique.insert(unique, key);
                self.moved.insert(key);
                key
            }
        };
        crates.refresh_attrs(files, key, false);
        self.order.push(key);
        self.map.insert(source, key);
        self.done[at] = Some(key);
        key
    }
}

/// Whether `held` is the data [`keyed_data`] of `basic` would build, compared field by field
/// without building it: an unchanged crate of a graph set again allocates nothing. Both are
/// destructured whole, so a field added to [`CrateData`] or [`Dependency`] is compared here
/// or this fails to compile (rule keys-cover-every-input).
fn same_data(
    held: &WorldCrateData,
    basic: &crate::CrateDataBuilder,
    key_of: &dyn Fn(CrateBuilderId) -> CrateKey,
) -> bool {
    let CrateData {
        root_file_id,
        edition,
        dependencies,
        origin,
        crate_attrs,
        target_kind,
        manifest_kind,
        crate_types,
        proc_macro_cwd,
        sealed,
    } = held;
    let same_dep = |held: &Dependency<CrateKey>, dep: &crate::DependencyBuilder| {
        let Dependency { crate_id, name, prelude, sysroot, private } = held;
        *crate_id == key_of(dep.crate_id)
            && *name == dep.name
            && *prelude == dep.prelude
            && *sysroot == dep.sysroot
            && *private == dep.private
    };
    *root_file_id == basic.root_file_id
        && *edition == basic.edition
        && dependencies.len() == basic.dependencies.len()
        && dependencies.iter().zip(&basic.dependencies).all(|(held, dep)| same_dep(held, dep))
        && *origin == basic.origin
        && *crate_attrs == basic.crate_attrs
        && *target_kind == basic.target_kind
        && *manifest_kind == basic.manifest_kind
        && *crate_types == basic.crate_types
        && *proc_macro_cwd == basic.proc_macro_cwd
        && *sealed == basic.sealed
}

/// `basic` with each dependency's builder id made its key by `key_of`.
fn keyed_data(basic: &crate::CrateDataBuilder, key_of: &dyn Fn(CrateBuilderId) -> CrateKey) -> WorldCrateData {
    CrateData {
        root_file_id: basic.root_file_id,
        edition: basic.edition,
        dependencies: {
            let mut out = Vec::with_capacity_in(basic.dependencies.len(), Global);
            out.extend(basic.dependencies.iter().map(|dep| Dependency {
                crate_id: key_of(dep.crate_id),
                name: dep.name.clone(), // clone: as above
                prelude: dep.prelude,
                sysroot: dep.sysroot,
                private: dep.private,
            }));
            out
        },
        origin: basic.origin.clone(), // clone: as above
        crate_attrs: basic.crate_attrs.clone(), // clone: as above
        target_kind: basic.target_kind,
        manifest_kind: basic.manifest_kind,
        crate_types: basic.crate_types,
        proc_macro_cwd: basic.proc_macro_cwd.clone(), // clone: an `Arc`
        sealed: basic.sealed,
    }
}

/// `data` with its dependencies' [`Crate`]s `deps`, in its order: the data a [`Crate`] input
/// is made with.
fn built_data(data: &WorldCrateData, deps: &[Crate]) -> BuiltCrateData {
    CrateData {
        root_file_id: data.root_file_id,
        edition: data.edition,
        dependencies: {
            let mut out = Vec::with_capacity_in(data.dependencies.len(), Global);
            out.extend(data.dependencies.iter().zip(deps).map(|(dep, &crate_id)| Dependency {
                crate_id,
                name: dep.name.clone(), // clone: as above
                prelude: dep.prelude,
                sysroot: dep.sysroot,
                private: dep.private,
            }));
            out
        },
        origin: data.origin.clone(), // clone: as above
        crate_attrs: data.crate_attrs.clone(), // clone: as above
        target_kind: data.target_kind,
        manifest_kind: data.manifest_kind,
        crate_types: data.crate_types,
        proc_macro_cwd: data.proc_macro_cwd.clone(), // clone: an `Arc`
        sealed: data.sealed,
    }
}

/// The session's data: what the session arena's singletons are made of.
pub(crate) struct Session {
    pub(crate) library_roots: blaze_hash::HashSet<SourceRootId, Global>,
    pub(crate) local_roots: blaze_hash::HashSet<SourceRootId, Global>,
    pub(crate) process_env: Option<Env>,
    pub(crate) settings: FrontSettingsData,
    /// The crates of the request's primary packages, sorted ([`crate::PrimaryCrates`]).
    pub(crate) primary: Vec<CrateKey, Global>,
    /// What the driver has made in the session arena each time it is made, by fact id
    /// ([`crate::Edit::set_given`] of no crate).
    pub(crate) makers: Vec<Maker, Global>,
    /// Whether any of the above moved since the last commit.
    pub(crate) moved: bool,
}

/// A bug the tests plant to show their oracle catches it; [`Plant::None`] everywhere else.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// A commit marks no arena for what was written: every arena is carried.
    EditMarksNothing,
    /// A crate arena made again takes the crate's variables as they were when it was first
    /// made, not as the world holds them now.
    RemakeReusesStale,
}

/// The world: everything but the files ([`Files`], shared with the snapshot's readers).
pub(crate) struct World {
    pub(crate) crates: Crates,
    pub(crate) session: Session,
    /// The session's fact boundaries, marked on each session's first builder.
    pub(crate) boundaries: fn(&mut Builder),
    pub(crate) log: Option<std::sync::Arc<blaze_memo::testing::RunLog, Global>>,
    pub(crate) flight: std::sync::Arc<dyn blaze_memo::Flight>,
    /// Whether each session keeps read lists: chosen by the database's maker ([`Db::new`],
    /// [`Db::logged`], [`Db::for_tests_with_reads`]), and the same for every session.
    ///
    /// [`Db::new`]: crate::Db::new
    /// [`Db::logged`]: crate::Db::logged
    /// [`Db::for_tests_with_reads`]: crate::Db::for_tests_with_reads
    pub(crate) reads: blaze_memo::Reads,
    /// The arena capacity of the session: past it a commit starts a new session.
    pub(crate) arenas: u32,
    pub(crate) plant: Plant,
    /// Under [`Plant::RemakeReusesStale`]: each crate's variables when its arena was first made.
    stale: HashMap<CrateKey, Env, Global>,
}

impl World {
    pub(crate) fn new(
        boundaries: fn(&mut Builder),
        log: Option<std::sync::Arc<blaze_memo::testing::RunLog, Global>>,
        flight: std::sync::Arc<dyn blaze_memo::Flight>,
        reads: blaze_memo::Reads,
    ) -> World {
        World {
            crates: Crates::default(),
            session: Session {
                library_roots: blaze_hash::HashSet::new_in(Global),
                local_roots: blaze_hash::HashSet::new_in(Global),
                process_env: None,
                settings: FrontSettingsData::from_env(),
                primary: Vec::new_in(Global),
                makers: Vec::new_in(Global),
                moved: true,
            },
            boundaries,
            log,
            flight,
            reads,
            arenas: 0,
            plant: Plant::None,
            stale: HashMap::new_in(Global),
        }
    }

    /// Whether the world has a write no commit took: a crate, the graph's shape or the
    /// session moved. The files' are [`Files::has_marks`].
    pub(crate) fn moved(&self) -> bool {
        !self.crates.dirty.is_empty() || self.crates.shape_moved || self.session.moved
    }

    /// Ends the open edit, its writes kept: once a commit has made its snapshot, which holds
    /// what they moved.
    pub(crate) fn forget(&mut self) {
        self.crates.undo.clear();
        self.crates.dirty.clear();
        self.crates.shape_moved = false;
        self.session.moved = false;
    }

    /// Puts back every write of the open edit, the last first: the crates and the session as
    /// the edit found them, nothing marked. A crate the edit made holds its key no more (the
    /// key is not given again). O(the edit's writes).
    pub(crate) fn rollback(&mut self) {
        let crates = &mut self.crates;
        while let Some(undo) = crates.undo.pop() {
            match undo {
                Undo::Made(key) => crates.entries[key.0 as usize] = None,
                Undo::Field(key, field) => {
                    let entry = crates.get_mut(key);
                    match field {
                        Field::Data(it) => entry.data = it,
                        Field::Extra(it) => entry.extra = it,
                        Field::Workspace(it) => entry.workspace = it,
                        Field::Cfg(it) => entry.cfg = it,
                        Field::Env(it) => entry.env = it,
                        Field::Lints(it) => entry.lints = it,
                    }
                }
                Undo::Content(key, content) => crates.get_mut(key).content = content,
                Undo::AttrFiles(key, files) => crates.get_mut(key).attr_files = files,
                Undo::AttrOf(file, Some(key)) => _ = crates.attr_of.insert(file, key),
                Undo::AttrOf(file, None) => _ = crates.attr_of.remove(&file),
                Undo::Gift(key, kind, was) => {
                    let gifts = &mut crates.get_mut(key).gifts;
                    let at = gifts.iter().position(|it| it.kind == kind);
                    match (at, was) {
                        (Some(at), Some(was)) => gifts[at] = was,
                        (Some(at), None) => _ = gifts.remove(at),
                        (None, Some(was)) => gifts.push(was),
                        (None, None) => {}
                    }
                }
                Undo::Maker(Some(key), id, was) => unput_maker(&mut crates.get_mut(key).makers, id, was),
                Undo::Maker(None, id, was) => unput_maker(&mut self.session.makers, id, was),
                Undo::Listed(key, listed) => crates.get_mut(key).listed = listed,
                Undo::OrderPushed => _ = crates.order.pop(),
                Undo::OrderPopped(key) => crates.order.push(key),
                Undo::Order(order) => crates.order = order,
                Undo::Unique(unique) => _ = crates.by_unique.remove(&unique),
                Undo::Session(field) => {
                    let session = &mut self.session;
                    match field {
                        SessionField::LibraryRoots(it) => session.library_roots = it,
                        SessionField::LocalRoots(it) => session.local_roots = it,
                        SessionField::ProcessEnv(it) => session.process_env = it,
                        SessionField::Settings(it) => session.settings = it,
                        SessionField::Primary(it) => session.primary = it,
                    }
                }
            }
        }
        crates.dirty.clear();
        crates.shape_moved = false;
        self.session.moved = false;
    }
}

/// The snapshot: the memo, the table of its inputs, and its commit's number.
pub(crate) struct Snap {
    pub(crate) memo: Memo,
    pub(crate) table: Table,
    /// Commits before this snapshot: what a file read outside the VFS is versioned by
    /// ([`Files::included_file`]).
    pub(crate) seq: u64,
}

/// From each key to the input of the snapshot that holds it.
#[derive(Clone, Default)] // clone: when a reader holds the old snapshot through a commit
pub(crate) struct Table {
    pub(crate) texts: Vec<Option<VfsText>, Global>,
    pub(crate) raws: Vec<Option<RawFile>, Global>,
    pub(crate) file_roots: Vec<Option<VfsFileRoot>, Global>,
    pub(crate) roots: Vec<Option<VfsRoot>, Global>,
    pub(crate) crates: Vec<Option<Crate>, Global>,
    /// Arena index to the crate it holds (none for the session's, 0).
    pub(crate) at_index: Vec<Option<CrateKey>, Global>,
    /// Arena index to whether it was sealed.
    pub(crate) sealed: Vec<bool, Global>,
    /// What was placed in each arena: the rows a remake of it clears.
    pub(crate) arena_files: Vec<Vec<FileId, Global>, Global>,
    pub(crate) arena_roots: Vec<Vec<SourceRootId, Global>, Global>,
    /// The listed crates, in order ([`crate::all_crates`]).
    pub(crate) all: std::sync::Arc<[Crate]>,
    /// Each source root's listed crates, in order ([`crate::source_root_crates`]).
    pub(crate) root_crates: HashMap<SourceRootId, std::sync::Arc<[Crate]>, Global>,
    /// The primary crates, sorted.
    pub(crate) primary: std::sync::Arc<[Crate]>,
    /// Each listed crate's item key ([`crate::crate_item_key`]), in order
    /// ([`crate::crate_item_keys`]): session-level, made from the world when the session arena
    /// is (a listed crate written, added or taken out), carried with the table otherwise.
    pub(crate) item_keys: std::sync::Arc<[(CrateKey, ContentId)]>,
}

fn put<T>(rows: &mut Vec<Option<T>, Global>, at: usize, value: Option<T>) {
    if rows.len() <= at {
        if value.is_none() {
            return;
        }
        rows.resize_with(at + 1, || None);
    }
    rows[at] = value;
}

impl Table {
    /// The crate whose arena `arena` is: what [`Crate::key`] answers.
    pub(crate) fn key_of(&self, arena: ArenaId) -> CrateKey {
        match self.at_index.get(arena.index() as usize) {
            Some(Some(key)) => *key,
            _ => panic!("the arena {arena:?} holds no crate"),
        }
    }

    /// Clears every row placed in arena `index`.
    fn clear(&mut self, index: u32) {
        let at = index as usize;
        if let Some(files) = self.arena_files.get_mut(at) {
            for file in std::mem::replace(files, Vec::new_in(Global)) {
                let row = file.index() as usize;
                put(&mut self.texts, row, None);
                put(&mut self.raws, row, None);
                put(&mut self.file_roots, row, None);
            }
        }
        if let Some(roots) = self.arena_roots.get_mut(at) {
            for root in std::mem::replace(roots, Vec::new_in(Global)) {
                put(&mut self.roots, root.0 as usize, None);
            }
        }
        if let Some(slot) = self.at_index.get_mut(at)
            && let Some(key) = slot.take()
        {
            put(&mut self.crates, key.0 as usize, None);
        }
    }

    /// Puts `rows`, made for the arenas a commit made again, in the table, whose rows of those
    /// arenas are cleared: nothing here fails, so a commit applies its rows whole or not at all.
    fn apply(&mut self, rows: Rows) {
        let Rows { texts, raws, file_roots, roots, crates, arena_files, arena_roots } = rows;
        for (file, text) in texts {
            put(&mut self.texts, file.index() as usize, Some(text));
        }
        for (file, raw) in raws {
            put(&mut self.raws, file.index() as usize, Some(raw));
        }
        for (file, root) in file_roots {
            put(&mut self.file_roots, file.index() as usize, Some(root));
        }
        for (id, root) in roots {
            put(&mut self.roots, id.0 as usize, Some(root));
        }
        fn grow(table: &mut Table, arena: u32) {
            let at = arena as usize;
            if table.arena_files.len() <= at {
                table.arena_files.resize_with(at + 1, || Vec::new_in(Global));
                table.arena_roots.resize_with(at + 1, || Vec::new_in(Global));
            }
        }
        for (arena, file) in arena_files {
            grow(self, arena);
            self.arena_files[arena as usize].push(file);
        }
        for (arena, root) in arena_roots {
            grow(self, arena);
            self.arena_roots[arena as usize].push(root);
        }
        for (key, (krate, index)) in crates {
            put(&mut self.crates, key.0 as usize, Some(krate));
            self.at_index[index as usize] = Some(key);
        }
    }
}

/// The rows of the arenas a commit makes again, made beside the old snapshot's table and put in
/// it only once the next snapshot is made ([`Table::apply`]): a maker that panics leaves the
/// table as it was.
#[derive(Default)]
pub(crate) struct Rows {
    texts: Vec<(FileId, VfsText), Global>,
    raws: Vec<(FileId, RawFile), Global>,
    file_roots: Vec<(FileId, VfsFileRoot), Global>,
    roots: Vec<(SourceRootId, VfsRoot), Global>,
    /// Each crate made, with its arena's index.
    crates: HashMap<CrateKey, (Crate, u32), Global>,
    /// What was placed in each arena made, by its index: the rows a remake of it clears.
    arena_files: Vec<(u32, FileId), Global>,
    arena_roots: Vec<(u32, SourceRootId), Global>,
}

impl Rows {
    fn placed(&mut self, index: u32) -> Placed<'_> {
        Placed { rows: self, arena: index }
    }
}

/// Rows placed in one arena as it is made ([`Files::make_root`] and the rest).
pub(crate) struct Placed<'t> {
    rows: &'t mut Rows,
    arena: u32,
}

impl Placed<'_> {
    pub(crate) fn text(&mut self, file: FileId, text: VfsText) {
        self.rows.texts.push((file, text));
        self.rows.arena_files.push((self.arena, file));
    }

    pub(crate) fn raw(&mut self, file: FileId, raw: RawFile) {
        self.rows.raws.push((file, raw));
        self.rows.arena_files.push((self.arena, file));
    }

    pub(crate) fn file_root(&mut self, file: FileId, root: VfsFileRoot) {
        self.rows.file_roots.push((file, root));
        self.rows.arena_files.push((self.arena, file));
    }

    pub(crate) fn root(&mut self, id: SourceRootId, root: VfsRoot) {
        self.rows.roots.push((id, root));
        self.rows.arena_roots.push((self.arena, id));
    }
}

/// Where everything goes in a snapshot: each crate's index, each root's owner.
struct Layout {
    /// Arena index to crate (index 0 is none).
    at: Vec<Option<CrateKey>, Global>,
    index_of: HashMap<CrateKey, u32, Global>,
    /// Each source root's owner's index (0: the session).
    owner: HashMap<SourceRootId, u32, Global>,
    /// Each arena's roots, by index.
    owned: Vec<Vec<SourceRootId, Global>, Global>,
    sealed: Vec<bool, Global>,
}

impl Layout {
    fn of(world: &World, files: &Files) -> Layout {
        let crates = &world.crates;
        let n = crates.order.len() + 1;
        let mut at = Vec::with_capacity_in(n, Global);
        at.push(None);
        let mut index_of = HashMap::with_capacity_in(n, Global);
        let mut owner = HashMap::new_in(Global);
        let mut sealed = Vec::with_capacity_in(n, Global);
        sealed.push(false);
        for (place, &key) in crates.order.iter().enumerate() {
            let index = place as u32 + 1;
            at.push(Some(key));
            index_of.insert(key, index);
            let entry = crates.get(key);
            if let Some(root) = files.root_of(entry.data.root_file_id) {
                owner.entry(root).or_insert(index);
            }
            let deps_sealed =
                entry.data.dependencies.iter().all(|dep| sealed[index_of[&dep.crate_id] as usize]);
            sealed.push(entry.data.sealed.is_some() && deps_sealed);
        }
        let mut owned = Vec::with_capacity_in(n, Global);
        owned.resize_with(n, || Vec::new_in(Global));
        for root in files.roots() {
            let index = owner.get(&root).copied().unwrap_or(0);
            owned[index as usize].push(root);
        }
        Layout { at, index_of, owner, owned, sealed }
    }

    fn owner(&self, root: SourceRootId) -> u32 {
        self.owner.get(&root).copied().unwrap_or(0)
    }

    fn len(&self) -> u32 {
        self.at.len() as u32
    }
}

/// What [`commit`] did.
pub(crate) struct Made {
    pub(crate) snap: Snap,
    pub(crate) dropped: Box<[u32]>,
    /// Why the commit started a new session, carrying nothing; `None` when it carried.
    pub(crate) fresh: Option<Fresh>,
    /// The arena capacity of the new session, when it started one ([`World::arenas`]).
    pub(crate) arenas: Option<u32>,
}

/// Why a commit started a new session (every arena made again) instead of carrying the old
/// one's arenas: what the commit's span says, so a profile can tell a rare full remake apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fresh {
    /// The database's first commit.
    First,
    /// The world has more arenas than the session was made with.
    Outgrown,
    /// A sealed arena was marked (`Memo::next` refuses to drop one).
    SealedMarked,
    /// An arena to drop has spent its generations (`NextRefused::GenerationsSpent`).
    GenerationsSpent,
    /// The last commit failed after the old snapshot was asked for its next
    /// ([`Memo::has_successor`]): a maker panicked while the next was made, and the edit was
    /// dropped. The old snapshot answers still but makes no other next.
    AfterFailedCommit,
}

impl Fresh {
    /// Its name, as the commit's span records it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Fresh::First => "first",
            Fresh::Outgrown => "arenas outgrown",
            Fresh::SealedMarked => "a sealed arena marked",
            Fresh::GenerationsSpent => "generations spent",
            Fresh::AfterFailedCommit => "after a failed commit",
        }
    }
}

/// The arenas a commit marks: what moved since `old`, by where it was and where it goes.
#[allow(clippy::too_many_arguments, reason = "what moved, each kind on its own")]
fn marked(
    world: &World,
    files: &Files,
    layout: &Layout,
    old: &Table,
    marks: FileMarks,
    crates_dirty: &[CrateKey],
    session_moved: bool,
    shape_moved: bool,
) -> BTreeSet<u32> {
    let mut marked: BTreeSet<u32> = BTreeSet::new();
    if world.plant == Plant::EditMarksNothing {
        return marked;
    }
    if session_moved || shape_moved || marks.session {
        marked.insert(0);
    }
    // A crate that moved to another index is made again at both.
    let span = old.at_index.len().max(layout.at.len());
    for index in 1..span as u32 {
        let was = old.at_index.get(index as usize).copied().flatten();
        let now = layout.at.get(index as usize).copied().flatten();
        if was != now {
            marked.insert(index);
        }
    }
    let old_index =
        |key: CrateKey| old.at_index.iter().position(|it| *it == Some(key)).map(|it| it as u32);
    for &key in crates_dirty {
        marked.extend(old_index(key));
        marked.extend(layout.index_of.get(&key).copied());
    }
    let old_owner = |root: SourceRootId| -> u32 {
        old.arena_roots.iter().position(|roots| roots.contains(&root)).map_or(0, |it| it as u32)
    };
    let mut roots: Vec<SourceRootId, Global> = marks.roots;
    for &file in &marks.files {
        if let Some(&krate) = world.crates.attr_of.get(&file) {
            marked.extend(layout.index_of.get(&krate).copied());
            continue;
        }
        match files.root_of(file) {
            Some(root) => roots.push(root),
            // A file of no root lives in the session arena.
            None => _ = marked.insert(0),
        }
    }
    // A root whose owner moved is placed again in both.
    for root in files.roots() {
        if old_owner(root) != layout.owner(root) {
            roots.push(root);
        }
    }
    for root in roots {
        marked.insert(old_owner(root));
        marked.insert(layout.owner(root));
    }
    for row in marks.included {
        for key in files.included_readers(row) {
            marked.extend(layout.index_of.get(&key).copied());
        }
    }
    marked
}

/// A commit's first step, before it asks whether anything moved: makes again the content ID of
/// each crate rooted in a source root whose file set the edit wrote, and of its dependents
/// whose IDs moved with it ([`Crates::refresh_rooted`]: an ID is of its root file's path), and
/// puts each crate's attribute files in its root file's root again
/// ([`Crates::settle_attr_roots`]). Its writes are the edit's, undone with it.
pub(crate) fn settle(world: &mut World, files: &Files) {
    let written = files.roots_written();
    world.crates.refresh_rooted(files, &written);
    world.crates.settle_attr_roots(files);
}

/// The next snapshot after `old` from the world as it is now ([`settle`]d). Nothing of the
/// world or of `old` is written until the next snapshot is made and every row and list of its
/// table is: only then is `old`'s table taken over (in place when no reader holds it, else
/// copied), its dropped arenas' rows cleared and the new rows put, none of which fails. So a
/// maker that panics leaves the world and `old` whole; the caller drops the edit. The marks,
/// the dirty crates and the moved flags are the caller's to clear once it holds the snapshot.
pub(crate) fn commit(
    world: &mut World,
    files: &Files,
    mut old: Option<&mut std::sync::Arc<Snap, Global>>,
) -> Made {
    let marks = files.marks();
    let layout = Layout::of(world, files);
    let seq = old.as_ref().map_or(0, |it| it.seq + 1);

    // The carried arenas, or none: a first commit, an arena capacity outgrown, a sealed arena
    // that moved (`Memo::next` refuses to drop one), or an old snapshot a failed commit asked
    // for its next already, starts a new session.
    let carried: Result<_, Fresh> = match old.as_deref() {
        None => Err(Fresh::First),
        Some(_) if layout.len() > world.arenas => Err(Fresh::Outgrown),
        Some(old) if old.memo.has_successor() => Err(Fresh::AfterFailedCommit),
        Some(old) => {
            let marked = marked(
                world,
                files,
                &layout,
                &old.table,
                marks,
                &world.crates.dirty,
                world.session.moved,
                world.crates.shape_moved,
            );
            let hits_sealed =
                marked.iter().any(|&index| old.table.sealed.get(index as usize).copied().unwrap_or(false));
            match hits_sealed {
                true => Err(Fresh::SealedMarked),
                false => {
                    let mut ids: Vec<ArenaId, Global> = Vec::new_in(Global);
                    ids.extend(old.memo.arenas().filter(|it| marked.contains(&it.index())));
                    match old.memo.next(&ids) {
                        // An arena to drop has spent its generations
                        // (`NextRefused::GenerationsSpent`): the session ends here and a fresh
                        // one is made below, as when a sealed arena is marked. Nothing of the
                        // old session is carried, so no id of an earlier making can be read.
                        // Rare (2^generation bits remakes of one index), so an event names the
                        // arena, and the commit's span says the session is fresh.
                        Err(refused @ blaze_memo::NextRefused::GenerationsSpent { .. }) => {
                            tracing::info!(
                                target: "blaze",
                                "a commit starts a new session: {refused}"
                            );
                            Err(Fresh::GenerationsSpent)
                        }
                        Ok(next) => {
                            let mut dropped: Vec<u32, Global> = Vec::new_in(Global);
                            dropped.extend_from_slice(next.dropped());
                            Ok((next.builder, dropped))
                        }
                    }
                }
            }
        }
    };
    let fresh = carried.as_ref().err().copied();
    let (mut builder, dropped, arenas) = match carried {
        Ok((builder, dropped)) => (builder, dropped, None),
        Err(_) => {
            let capacity = (2 * layout.len()).clamp(64, blaze_memo::MAX_ARENAS);
            let config = blaze_memo::Config {
                arenas: capacity,
                reads: world.reads,
                flight: std::sync::Arc::clone(&world.flight),
                cap_bytes: u64::MAX,
                plant: blaze_memo::Plant::None,
            };
            let mut builder = Memo::builder(config);
            (world.boundaries)(&mut builder);
            if let Some(log) = &world.log {
                builder.log_runs(std::sync::Arc::clone(log));
            }
            let mut dropped = Vec::new_in(Global);
            dropped.extend(0..layout.len());
            (builder, dropped, Some(capacity))
        }
    };
    // The table carried rows are read from while the arenas are made: the old one, when the
    // session goes on.
    let base: Option<&Table> = match (&old, fresh) {
        (Some(old), None) => Some(&old.table),
        _ => None,
    };
    // Which arenas are carried, up to the last the old table or the layout has a row of.
    let tracked = base
        .map_or(0, |it| it.at_index.len().max(it.arena_files.len()))
        .max(layout.at.len());
    let mut carried_at: Vec<bool, Global> = Vec::with_capacity_in(tracked, Global);
    carried_at.extend((0..tracked as u32).map(|index| builder.has(index)));

    let mut rows = Rows::default();
    for index in 0..layout.len() {
        if builder.has(index) {
            continue;
        }
        remake(world, files, &layout, &mut builder, &mut rows, base, index);
    }
    let memo = builder.build();

    // The lists, read from the rows made and the carried table before anything is written:
    // O(crates).
    let made = |key: CrateKey| -> Option<Crate> {
        rows.crates
            .get(&key)
            .map(|it| it.0)
            .or_else(|| base.and_then(|it| it.crates.get(key.0 as usize).copied().flatten()))
    };
    let crate_of = |key: CrateKey| -> Crate {
        made(key).unwrap_or_else(|| panic!("the crate {key:?} of the order was made in no arena"))
    };
    let crates = &world.crates;
    let mut all = Vec::new_in(Global);
    let mut root_crates: HashMap<SourceRootId, Vec<Crate, Global>, Global> = HashMap::new_in(Global);
    for &key in &crates.order {
        let entry = crates.get(key);
        if !entry.listed {
            continue;
        }
        let krate = crate_of(key);
        all.push(krate);
        if let Some(root) = files.root_of(entry.data.root_file_id) {
            root_crates.entry(root).or_insert_with(|| Vec::new_in(Global)).push(krate);
        }
    }
    let all: std::sync::Arc<[Crate]> = all.into_iter().collect();
    let root_crates: HashMap<SourceRootId, std::sync::Arc<[Crate]>, Global> =
        root_crates.into_iter().map(|(root, crates)| (root, crates.into_iter().collect())).collect();
    let mut primary: Vec<Crate, Global> = Vec::new_in(Global);
    primary.extend(world.session.primary.iter().filter_map(|&key| made(key)));
    primary.sort_unstable();
    let primary: std::sync::Arc<[Crate]> = primary.into_iter().collect();

    // From here on nothing fails. The old table taken over, or copied while a reader holds
    // the old snapshot through this commit; a new session's starts empty.
    let mut table = match (old.take(), fresh) {
        (Some(old), None) => match std::sync::Arc::get_mut(old) {
            // The caller drops the old snapshot once the new one is made.
            Some(snap) => std::mem::take(&mut snap.table),
            // clone: a reader holds the old snapshot through this commit
            None => old.table.clone(),
        },
        _ => Table::default(),
    };
    // Rows of every arena not carried are cleared first: a remade arena may place what another
    // held.
    let tracked = tracked.max(table.at_index.len()).max(table.arena_files.len());
    for index in 0..tracked as u32 {
        if !carried_at.get(index as usize).copied().unwrap_or(false) {
            table.clear(index);
        }
    }
    table.at_index.resize(layout.at.len(), None);
    table.sealed.clone_from(&layout.sealed);
    table.apply(rows);
    table.all = all;
    table.root_crates = root_crates;
    table.primary = primary;
    // The session list of item keys, made again with the session arena: a listed crate
    // written, added or taken out moves the shape (`Crates::shape_moved`), which makes the
    // session arena again, so a carried session arena has the carried table's list. Made from
    // the world as this commit has it, so a rolled-back edit leaves no trace in it.
    if !carried_at.first().copied().unwrap_or(false) {
        table.item_keys = item_keys_of(&world.crates);
    }

    Made { snap: Snap { memo, table, seq }, dropped: dropped.into_boxed_slice(), fresh, arenas }
}

/// Each listed crate's item key ([`crate::crate_item_key`]), in the graph's order, each
/// dependency's read from those before it. O(listed crates) hashes, once per making of the
/// session arena, which already makes every crate's arena again.
///
/// # Panics
///
/// Where a listed crate depends on a crate not listed before it: the order keeps every crate
/// after its dependencies, and a graph lists what its crates depend on.
fn item_keys_of(crates: &Crates) -> std::sync::Arc<[(CrateKey, ContentId)]> {
    let mut by_key: HashMap<CrateKey, ContentId, Global> = HashMap::new_in(Global);
    let mut keys: Vec<(CrateKey, ContentId), Global> = Vec::new_in(Global);
    for &key in &crates.order {
        let entry = crates.get(key);
        if !entry.listed {
            continue;
        }
        let item_key = crate::crate_item_key(&entry.data, &entry.extra, &entry.cfg, &|dep: CrateKey| {
            *by_key.get(&dep).unwrap_or_else(|| {
                panic!("the listed crate {key:?} depends on {dep:?}, which is not listed before it")
            })
        });
        by_key.insert(key, item_key);
        keys.push((key, item_key));
    }
    std::sync::Arc::from(&keys[..])
}

/// Makes arena `index`'s inputs from the world, its rows into `rows`; a dependency carried
/// from the old session is read from `base`, its table.
fn remake(
    world: &mut World,
    files: &Files,
    layout: &Layout,
    builder: &mut Builder,
    rows: &mut Rows,
    base: Option<&Table>,
    index: u32,
) {
    if index == 0 {
        let inputs = builder.arena(0, Tag::Query);
        make_session(world, files, &inputs);
        let mut placed = rows.placed(0);
        for &root in &layout.owned[0] {
            files.make_root(&inputs, root, &mut placed);
        }
        files.make_rootless(&inputs, &|file| world.crates.attr_of.contains_key(&file), &mut placed);
        // `inputs` (a borrow of the builder) ends above: the driver's makers are given the
        // builder and the arena's inputs again ([`Builder::inputs`]).
        make_given(builder, 0, &world.session.makers);
        return;
    }
    let key = layout.at[index as usize].expect("a crate arena");
    let entry = world.crates.get(key);
    let session = builder.id(0).expect("the session arena is made first");
    let mut borrows: Vec<ArenaId, Global> = Vec::new_in(Global);
    borrows.push(session);
    for dep in &entry.data.dependencies {
        let at = layout.index_of[&dep.crate_id];
        borrows.push(builder.id(at).expect("a dependency's arena is made before it"));
    }
    if let Some(root) = files.root_of(entry.data.root_file_id) {
        let owner = layout.owner(root);
        if owner != index && owner != 0 {
            borrows.push(builder.id(owner).expect("a root's owner is made before the crates rooted there"));
        }
    }
    let deps: Vec<Crate, Global> = {
        let mut deps = Vec::new_in(Global);
        deps.extend(entry.data.dependencies.iter().map(|dep| {
            let made = rows.crates.get(&dep.crate_id).map(|it| it.0);
            made.or_else(|| base.and_then(|it| it.crates.get(dep.crate_id.0 as usize).copied().flatten()))
                .expect("a dependency's crate is made before it")
        }));
        deps
    };
    let stale_env = match world.plant {
        Plant::RemakeReusesStale => Some(
            world.stale.entry(key).or_insert_with(|| entry.env.clone()).clone(), // clone: a planted bug's copy, tests only
        ),
        _ => None,
    };
    let entry = world.crates.get(key);
    let inputs = builder.arena(index, Tag::Query);
    for id in borrows {
        inputs.borrows(id);
    }
    if layout.sealed[index as usize] {
        inputs.seal();
    }
    let mut placed = rows.placed(index);
    for &root in &layout.owned[index as usize] {
        files.make_root(&inputs, root, &mut placed);
    }
    files.make_files(&inputs, &entry.attr_files, &mut placed);
    let mut slots = Vec::with_capacity_in(entry.gifts.len(), Global);
    for gift in &entry.gifts {
        let fact = gift.slot_fact(entry.content);
        // clone: the gift's `Arc`, once per making of its arena
        slots.push((gift.kind, CrateSlot::new(&inputs, fact, gift.value.clone())));
    }
    let data = built_data(&entry.data, &deps);
    let krate = Crate::new(
        &inputs,
        facts::krate(entry.content),
        data,
        entry.extra.clone(), // clone: the crate's fields, once per making of its arena
        Arc::clone(&entry.workspace),
        entry.cfg.clone(), // clone: as above
        stale_env.unwrap_or_else(|| entry.env.clone()), // clone: as above
        InputId(entry.content),
        entry.attr_files.clone(), // clone: as above
        entry.lints.clone(), // clone: as above
        slots,
    );
    make_given(builder, index, &entry.makers);
    rows.crates.insert(key, (krate, index));
}

/// Runs `makers` in arena `index`, just added to `builder` with the world's own inputs.
fn make_given(builder: &Builder, index: u32, makers: &[Maker]) {
    if makers.is_empty() {
        return;
    }
    let id = builder.id(index).expect("the arena was just added");
    run_makers(makers, builder, &builder.inputs(id));
}

/// The session arena's singletons.
fn make_session(world: &World, files: &Files, inputs: &Inputs<'_>) {
    let session = &world.session;
    let roots_content = |domain: &'static str, roots: &blaze_hash::HashSet<SourceRootId, Global>| {
        let mut sorted: Vec<u32, Global> = Vec::new_in(Global);
        sorted.extend(roots.iter().map(|it| it.0));
        sorted.sort_unstable();
        let mut hasher = Hasher::new(domain);
        crate::content::len(&mut hasher, sorted.len());
        for root in sorted {
            hasher.update(&root.to_le_bytes());
        }
        InputId(hasher.finish())
    };
    const LIBRARY: &str = "blaze v2 library roots 1";
    const LOCAL: &str = "blaze v2 local roots 1";
    const ENV: &str = "blaze v2 process env 1";
    const LIST: &str = "blaze v2 crate list 1";
    let library = roots_content(LIBRARY, &session.library_roots);
    // clone: the root sets, once per making of the session arena
    LibraryRoots::set(inputs, facts::singleton(LIBRARY, library), session.library_roots.clone());
    let local = roots_content(LOCAL, &session.local_roots);
    LocalRoots::set(inputs, facts::singleton(LOCAL, local), session.local_roots.clone());
    if let Some(env) = &session.process_env {
        let content = InputId(env.content());
        // clone: the process's variables, once per making of the session arena
        ProcessEnv::set(inputs, facts::singleton(ENV, content), env.clone(), content);
    }
    let list = InputId(world.crates.list_content());
    AllCrates::set(inputs, facts::singleton(LIST, list), list);
    session.settings.set_in(inputs);
    files.make_session(inputs);
}

/// The fact of a file read outside the VFS ([`crate::IncludedFile`]): of its path, its digest
/// its read's content.
pub(crate) fn included_fact(path: &[u8], content: ContentId) -> Fact {
    Fact { id: ContentId::of(crate::files::INCLUDED_PATH_DOMAIN, path), digest: content }
}
