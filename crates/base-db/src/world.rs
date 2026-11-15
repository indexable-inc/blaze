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
//! roots, named paths, the session's makers, and the files of no source root); index 1
//! ([`LIST_ARENA`]) is the crate list's, an open arena borrowing only the session whose one
//! input ([`AllCrates`], an ordinary input whose handle the table keeps in
//! `Table::list_input`, not a singleton, which the engine keeps in the session alone) holds
//! the list's digest ([`List`]: of each listed crate's item key and root path, not of its
//! content ID); indices `FIRST_ROOT..FIRST_ROOT + roots` are the source roots' (each root's
//! files in an open arena of its own, by root id, [`Layout`]); then a crate's index is
//! `FIRST_ROOT + roots +` its place in [`Crates::order`], every crate after the crates it
//! depends on. Each crate arena borrows its dependencies, the session and its root file's
//! root, and not the list: an entry that reads the list (a lookup by key that finds no crate,
//! `all_crates`) borrows it when it reads it. A root another crate reaches (a `#[path]`
//! module across packages) is borrowed by the entries that name or read it. A crate's
//! attribute files live in its own arena. A crate whose sources are pinned
//! ([`crate::CrateData::sealed`]) and whose dependencies are all sealed is sealed, and so is
//! the root it is rooted in. Sealing changes what a commit drops not at all: a write to a
//! sealed crate (a build script's env, a gift, a maker) drops that crate's arena and every
//! arena that borrowed from it, its dependents, never the session's. A crate's cfg, in its
//! item key and so in the list, drops the list's arena, the crate's and its dependents' and
//! what read the list; only what the session arena holds moving drops the session, and with
//! it every arena. A commit's span names each arena it marked and why ([`describe`]).
//!
//! Cost (rule 67). A commit is O(entries written) to mark, O(crates + roots) to lay the arenas
//! out again (the order, each root's arena: a vector walk, within the `Memo::next` floor of
//! O(arenas)), and O(inputs of the dropped arenas) to make them again: the floor is the
//! dropped arenas' inputs, as nothing carried is touched. A lookup is one index of a vector.
//! When no reader holds the old snapshot its table is taken over in place; while one does,
//! the table is copied (O(files + crates), a vector copy), which a resident helper pays only
//! when a reader outlives a commit.

use std::{
    alloc::{Allocator, Global},
    any::TypeId,
    io::Write as _,
};

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
    GivenFile, LocalRoots, ProcessEnv, RawFile, SourceRootId, VfsFileRoot, VfsPath, VfsRoot, VfsText,
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
    pub(crate) const GIVEN: &str = "blaze v2 given file fact 1";
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

    pub(crate) fn given(file: FileId, content: ContentId) -> Fact {
        Fact { id: of_file(GIVEN, file), digest: content }
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
    /// A gift of a [`crate::Located`] kind ([`crate::Edit::give_located`]): kept in the
    /// snapshot's table ([`Table::located`]), never a slot of the crate's arena, so its first
    /// gift marks no arena.
    pub(crate) located: bool,
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
    /// Where it was given (the caller of `Edit::set_given`): what a commit's span names as the
    /// cause when this maker marks its arena.
    pub(crate) at: &'static std::panic::Location<'static>,
}

/// Puts `maker` in `makers` (sorted by fact id), in place of one of the same fact id: whether
/// anything moved, with the maker it replaced (`None`: it had none of that fact id), for undo.
/// One of an equal fact moves nothing and is dropped: the giver promises that two makers under
/// one fact make inputs no reader tells apart.
pub(crate) fn put_maker<A: Allocator>(makers: &mut Vec<Maker, A>, maker: Maker) -> Option<Option<Maker>> {
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
fn unput_maker<A: Allocator>(makers: &mut Vec<Maker, A>, id: ContentId, was: Option<Maker>) {
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
    /// Every crate, each after those it depends on: its arena's index is
    /// [`FIRST_ROOT`] + the number of roots + its place ([`Layout`]).
    pub(crate) order: Vec<CrateKey, Global>,
    by_unique: HashMap<Unique, CrateKey, Global>,
    /// Each crate attribute file's crate.
    pub(crate) attr_of: HashMap<FileId, CrateKey, Global>,
    /// Crates whose entry moved since the last commit.
    pub(crate) dirty: Vec<CrateKey, Global>,
    /// Crates given a located gift of a kind they held none of since the last commit
    /// ([`Crates::give_located`]): a write of the next snapshot's table alone, which marks no
    /// arena (under [`Plant::LocatedMarks`], each of their arenas).
    pub(crate) located: Vec<CrateKey, Global>,
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
        self.refuse_located(key, kind, name, "Edit::give");
        if self.get(key).gifts.iter().any(|it| it.kind == kind && it.value == value) {
            return false;
        }
        self.gifts_drawn += 1;
        let digest = ContentId::of(facts::GIFT, &self.gifts_drawn.to_le_bytes());
        self.put_gift(key, Gift { kind, name, value, fact: GiftFact::Drawn(digest), located: false })
    }

    /// Stops where `key` holds a gift of `kind` given as [`crate::Located`]: one kind is given
    /// one way, so a slot write and a table write never stand for one gift.
    fn refuse_located(&self, key: CrateKey, kind: TypeId, name: &'static str, by: &str) {
        if self.get(key).gifts.iter().any(|it| it.kind == kind && it.located) {
            panic!(
                "`{by}` of the kind `{name}` to the crate {key:?}, which holds a gift of that kind given \
                 as located (`Edit::give_located`): a kind is given one way"
            );
        }
    }

    /// Gives `key` `value` as the located kind `kind` ([`crate::Located`]) under the giver's
    /// `fact`: whether anything moved. The first gift of the kind marks no arena: it goes in
    /// the next snapshot's table ([`Table::located`]), and the kind's promise is that every
    /// reader computes the same value with it as without it. A gift under the fact held moves
    /// nothing. One under another fact replaces it and marks the crate, as [`Crates::take_gift`]
    /// does: a value replaced or withdrawn is one a reader may have read and that the inputs
    /// no longer give (a dependency's image whose crate moved), so whatever read it goes.
    pub(crate) fn give_located(
        &mut self,
        key: CrateKey,
        kind: TypeId,
        name: &'static str,
        fact: Fact,
        value: Given,
    ) -> bool {
        let held = self.get(key).gifts.iter().find(|it| it.kind == kind);
        match held {
            Some(held) if !held.located => panic!(
                "`Edit::give_located` of the kind `{name}` to the crate {key:?}, which holds a gift of \
                 that kind given as a slot (`Edit::give`, `Edit::give_by`): a kind is given one way"
            ),
            Some(held) if held.fact == GiftFact::Given(fact) => false,
            Some(_) => {
                self.put_gift(key, Gift { kind, name, value, fact: GiftFact::Given(fact), located: true })
            }
            None => {
                self.get_mut(key).gifts.push(Gift {
                    kind,
                    name,
                    value,
                    fact: GiftFact::Given(fact),
                    located: true,
                });
                self.undo.push(Undo::Gift(key, kind, None));
                self.located.push(key);
                true
            }
        }
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
        self.refuse_located(key, kind, name, "Edit::give_by");
        if self.get(key).gifts.iter().any(|it| it.kind == kind && it.fact == GiftFact::Given(fact)) {
            return false;
        }
        self.put_gift(key, Gift { kind, name, value, fact: GiftFact::Given(fact), located: false })
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

    /// The crate list as the session arena holds it ([`List`]). O(listed crates) hashes, made
    /// when the shape moved or the session arena is made.
    ///
    /// # Panics
    ///
    /// Where a listed crate depends on a crate not listed before it (the order keeps every
    /// crate after its dependencies, and a graph lists what its crates depend on), or a listed
    /// crate's root file is in no source root.
    pub(crate) fn list(&self, files: &Files) -> List {
        let mut by_key: HashMap<CrateKey, ContentId, Global> = HashMap::new_in(Global);
        let mut keys: Vec<(CrateKey, ContentId), Global> = Vec::new_in(Global);
        let mut hasher = Hasher::new(crate::CRATE_GRAPH_DOMAIN);
        let listed = self.order.iter().filter(|&&key| self.get(key).listed).count();
        crate::content::len(&mut hasher, listed);
        for &key in &self.order {
            let entry = self.get(key);
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
            let root = files.path_of(entry.data.root_file_id).unwrap_or_else(|| {
                panic!("the root file {:?} of the listed crate {key:?} is in no source root", entry.data.root_file_id)
            });
            hasher.id(&item_key);
            blaze_mem::scratch::with_bytes(|bytes| {
                crate::content::path_key_into(&root, bytes);
                hasher.part(&bytes[..]);
            });
        }
        List { digest: hasher.finish(), item_keys: std::sync::Arc::from(&keys[..]) }
    }
}

/// The crate list as the list's arena holds it ([`LIST_ARENA`]): the digest of [`AllCrates`],
/// of what a reader of the list can tell apart, and each listed crate's item key in order
/// (indexed by prefix for [`crate::listed_by_prefix`]).
///
/// The digest is content, the same in every database of the same crates: of each listed
/// crate, in order, its item key ([`crate::crate_item_key`]: its name, origin, what it is
/// built for, its build script runs, target, edition, crate attributes, cfg and each
/// dependency's name, flags and item key) and its root file's path (what
/// [`crate::source_root_crates`] groups it by, with the root's own content). The keys are of
/// the database: a commit marks the list's arena where the digest or the keys moved
/// ([`commit`]), which drops the runs that read the list and their borrowers alone. It is not
/// of the crate's content ID: a write of a crate's env, workspace data or lints, or a gift,
/// moves that crate's arena and its dependents' (each borrows it), not the list. A [`Crate`]
/// of the list is read from the table, made each commit, never from the arena.
#[derive(Clone)] // clone: the table's, an `Arc` and a digest
pub(crate) struct List {
    pub(crate) digest: ContentId,
    pub(crate) item_keys: std::sync::Arc<[(CrateKey, ContentId)]>,
}

/// One crate's other `OUT_DIR`s ([`crate::other_out_dirs`]): the field its [`Crate`] input is
/// made with.
pub(crate) type Dirs = Box<[Box<str>]>;

/// Each crate's [`Dirs`], by key: what a commit compares with the old table's
/// ([`Table::out_dirs`]) to mark the crates whose dirs moved, and makes their arenas with.
pub(crate) type OutDirs = HashMap<CrateKey, Dirs, Global>;

/// `env`'s `OUT_DIR` without a trailing separator; `None` for none or an empty one.
fn out_dir(env: &Env) -> Option<Box<str>> {
    let dir = env.get("OUT_DIR")?;
    let trimmed = dir.trim_end_matches(['/', '\\']);
    (!trimmed.is_empty()).then(|| Box::from(trimmed))
}

/// Every crate's other `OUT_DIR`s: for each crate of the order, the `OUT_DIR`s of the listed
/// crates whose root file is in its root file's source root, its own left out, sorted, each
/// once (what [`crate::other_out_dirs`] read off the list and each crate's variables while it
/// was a query: a reader of every crate of its root and of the list, which every arena read).
/// O(crates), once a commit.
pub(crate) fn out_dirs_of(world: &World, files: &Files) -> OutDirs {
    let crates = &world.crates;
    let mut by_root: HashMap<SourceRootId, Vec<Box<str>, Global>, Global> = HashMap::new_in(Global);
    for &key in &crates.order {
        let entry = crates.get(key);
        if !entry.listed {
            continue;
        }
        let (Some(root), Some(dir)) = (files.root_of(entry.data.root_file_id), out_dir(&entry.env)) else {
            continue;
        };
        by_root.entry(root).or_insert_with(|| Vec::new_in(Global)).push(dir);
    }
    for dirs in by_root.values_mut() {
        dirs.sort_unstable();
        dirs.dedup();
    }
    let mut out = HashMap::with_capacity_in(crates.order.len(), Global);
    for &key in &crates.order {
        let entry = crates.get(key);
        let own = out_dir(&entry.env);
        let mut dirs: Vec<Box<str>, Global> = Vec::new_in(Global);
        if let Some(of_root) = files.root_of(entry.data.root_file_id).and_then(|root| by_root.get(&root)) {
            // clone: a directory's name, once per crate of its root a commit
            dirs.extend(of_root.iter().filter(|dir| Some(*dir) != own.as_ref()).cloned());
        }
        out.insert(key, dirs.into_boxed_slice());
    }
    out
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
    /// A commit that starts a new session gives it a new id space and keeps the database's
    /// nonce (`Memo::builder` in place of `Memo::next_session`): every arena starts again at
    /// generation 0 and takes the ids the old session's arenas had, under the same nonce.
    FreshRestartsIds,
    /// A commit that marks a sealed arena starts a new session, every arena made again: what a
    /// commit did while `Memo::next` refused to drop a sealed arena.
    SealedRestartsSession,
    /// A located gift's first gift marks its crate's arena, as a slot's gift does: what the
    /// image's lang items and a crate's constants' key did before they were located.
    LocatedMarks,
    /// A moved crate list marks the session arena, not the list's own ([`LIST_ARENA`]): what
    /// a commit did while the list was a singleton of the session, which every arena borrows.
    ListMarksSession,
    /// An arena whose seal moved is carried with the seal it was made with, not made again:
    /// what a commit did before seals were compared ([`Cause::Sealed`]).
    SealCarried,
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
        !self.crates.dirty.is_empty()
            || !self.crates.located.is_empty()
            || self.crates.shape_moved
            || self.session.moved
    }

    /// Ends the open edit, its writes kept: once a commit has made its snapshot, which holds
    /// what they moved.
    pub(crate) fn forget(&mut self) {
        self.crates.undo.clear();
        self.crates.dirty.clear();
        self.crates.located.clear();
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
        crates.located.clear();
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
    /// Each file known by its content alone ([`GivenFile`]).
    pub(crate) given: Vec<Option<GivenFile>, Global>,
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
    /// Each listed crate's item key ([`crate::crate_item_key`]), in order: of the [`List`] the
    /// list's arena ([`LIST_ARENA`]) was made with, carried with the table while that arena is.
    pub(crate) item_keys: std::sync::Arc<[(CrateKey, ContentId)]>,
    /// The same crates by the first 8 bytes of their item key read little-endian
    /// ([`crate::listed_by_prefix`]), sorted by it; made with `item_keys`.
    pub(crate) by_prefix: std::sync::Arc<[(u64, CrateKey)]>,
    /// The first two listed crates found with one prefix, by key: a build with two stops
    /// ([`crate::listed_clash`]).
    pub(crate) clash: Option<[CrateKey; 2]>,
    /// The digest of that [`List`] ([`AllCrates`]); `None` before the first session.
    pub(crate) list: Option<ContentId>,
    /// The crate list's input in [`LIST_ARENA`] ([`crate::list_input`]), carried with that
    /// arena; `None` before the first session.
    pub(crate) list_input: Option<AllCrates>,
    /// Each crate's other `OUT_DIR`s, as its arena was made with them ([`out_dirs_of`]).
    pub(crate) out_dirs: OutDirs,
    /// Each crate's located gifts ([`crate::Edit::give_located`]), by key index: its kind, the
    /// giver's fact and the value. Made whole each commit from the world, a few `Arc`s a crate.
    pub(crate) located: Vec<Vec<(TypeId, Fact, Given), Global>, Global>,
}

/// `item_keys` by prefix, sorted, and the first two crates of one prefix
/// ([`Table::by_prefix`], [`Table::clash`]). O(n log n) in the listed crates, made when the
/// list is.
fn by_prefix(item_keys: &[(CrateKey, ContentId)]) -> (std::sync::Arc<[(u64, CrateKey)]>, Option<[CrateKey; 2]>) {
    let mut sorted: Vec<(u64, CrateKey), Global> = Vec::with_capacity_in(item_keys.len(), Global);
    sorted.extend(item_keys.iter().map(|&(key, item_key)| (crate::key_prefix(&item_key), key)));
    sorted.sort_unstable();
    let clash = sorted.windows(2).find(|pair| pair[0].0 == pair[1].0).map(|pair| [pair[0].1, pair[1].1]);
    (std::sync::Arc::from(&sorted[..]), clash)
}

fn put<T, A: Allocator>(rows: &mut Vec<Option<T>, A>, at: usize, value: Option<T>) {
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
                put(&mut self.given, row, None);
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
        // The list's input is put by `commit`, which knows whether its arena was carried.
        let Rows { texts, raws, given, file_roots, roots, crates, arena_files, arena_roots, list: _ } =
            rows;
        for (file, text) in texts {
            put(&mut self.texts, file.index() as usize, Some(text));
        }
        for (file, raw) in raws {
            put(&mut self.raws, file.index() as usize, Some(raw));
        }
        for (file, it) in given {
            put(&mut self.given, file.index() as usize, Some(it));
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
    given: Vec<(FileId, GivenFile), Global>,
    file_roots: Vec<(FileId, VfsFileRoot), Global>,
    roots: Vec<(SourceRootId, VfsRoot), Global>,
    /// Each crate made, with its arena's index.
    crates: HashMap<CrateKey, (Crate, u32), Global>,
    /// What was placed in each arena made, by its index: the rows a remake of it clears.
    arena_files: Vec<(u32, FileId), Global>,
    arena_roots: Vec<(u32, SourceRootId), Global>,
    /// The crate list's input, where this commit made the list's arena ([`LIST_ARENA`]).
    list: Option<AllCrates>,
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

    pub(crate) fn given(&mut self, file: FileId, given: GivenFile) {
        self.rows.given.push((file, given));
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

/// Where everything goes in a snapshot: the session at index 0, the crate list at
/// [`LIST_ARENA`], then each source root's files in an open arena of their own (indices
/// `FIRST_ROOT..FIRST_ROOT + roots`, by root id), then each crate's arena in the graph's order.
///
/// Why the list has an open arena of its own: what moves it (a crate added, taken out or
/// moved, or a listed crate's item key, which its cfg is in) is read by few: an enumeration of
/// every crate ([`crate::all_crates`]) and a crate-key lookup that found none
/// ([`crate::listed_by_prefix`]). Each run that reads it borrows it, as a root's readers do,
/// so a moved list drops those readers and their borrowers, where it dropped every arena
/// while it was a singleton of the session's (round 27: each build script's cfg written,
/// and the second `hi` crate added, dropped all 32 arenas). No crate declares it: a crate's
/// own lookups that find their crate read that crate ([`crate::listed_by_prefix`]), and its
/// root's other `OUT_DIR`s are a field of its own ([`crate::other_out_dirs`]).
///
/// Why a root's files have an arena of their own, below every crate's: a crate reads files of
/// another source root (a `#[path]` module into another package's directory) that it is found
/// to read only once its def map is made, so no crate can declare that borrow when the arenas
/// are made, and a crate cannot borrow a later crate's arena. Each root's arena is open
/// (`Inputs::open`): any crate's key may name its files, and each entry that does borrows it,
/// as each run that reads it does. An edit of a file drops its root's arena and so exactly the
/// crates rooted in that root (each declares it), the crates that read a file of that root
/// across roots (each borrowed it), and their dependents. Against the floor (the crates whose
/// def maps reach the edited file, and their dependents) the grain is the root, as a crate's
/// own files have always been read: the crates rooted beside the file's crate are dropped too.
/// A root no crate is rooted in has an arena of its own as well, so an edit there drops its
/// readers, not the session arena, whose drop drops every arena.
struct Layout {
    /// Arena index to crate (none for the session's, 0, the list's and the roots').
    at: Vec<Option<CrateKey>, Global>,
    index_of: HashMap<CrateKey, u32, Global>,
    /// How many source roots there are: their arenas are at indices
    /// `FIRST_ROOT..FIRST_ROOT + roots`.
    roots: u32,
    /// Each source root's arena's index.
    owner: HashMap<SourceRootId, u32, Global>,
    /// Each arena's roots, by index: one for a root's arena, none for any other.
    owned: Vec<Vec<SourceRootId, Global>, Global>,
    sealed: Vec<bool, Global>,
}

impl Layout {
    fn of(world: &World, files: &Files) -> Layout {
        let crates = &world.crates;
        let roots = files.roots();
        let first_crate = FIRST_ROOT + roots.len() as u32;
        let n = first_crate as usize + crates.order.len();
        let mut at = Vec::with_capacity_in(n, Global);
        at.resize(first_crate as usize, None);
        let mut index_of = HashMap::with_capacity_in(n, Global);
        let mut owner = HashMap::with_capacity_in(roots.len(), Global);
        let mut owned = Vec::with_capacity_in(n, Global);
        owned.resize_with(n, || Vec::new_in(Global));
        for (place, &root) in roots.iter().enumerate() {
            let index = FIRST_ROOT + place as u32;
            owner.insert(root, index);
            owned[index as usize].push(root);
        }
        let mut sealed = Vec::with_capacity_in(n, Global);
        sealed.resize(first_crate as usize, false);
        for (place, &key) in crates.order.iter().enumerate() {
            let index = first_crate + place as u32;
            at.push(Some(key));
            index_of.insert(key, index);
            let entry = crates.get(key);
            let deps_sealed =
                entry.data.dependencies.iter().all(|dep| sealed[index_of[&dep.crate_id] as usize]);
            let is_sealed = entry.data.sealed.is_some() && deps_sealed;
            sealed.push(is_sealed);
            // A root is sealed where a sealed crate is rooted in it: a sealed arena borrows
            // only sealed ones (`Builder::build` checks it). An edit of a file there (of an
            // unsealed crate beside it, or the sealed crate's own) drops the root's arena and
            // its borrowers, as for any root.
            if is_sealed
                && let Some(&root_at) = files.root_of(entry.data.root_file_id).and_then(|root| owner.get(&root))
            {
                sealed[root_at as usize] = true;
            }
        }
        Layout { at, index_of, roots: first_crate - FIRST_ROOT, owner, owned, sealed }
    }

    /// The index of `root`'s arena; `None` where the world has no such source root set.
    fn owner(&self, root: SourceRootId) -> Option<u32> {
        self.owner.get(&root).copied()
    }

    /// Whether arena `index` is a source root's.
    fn is_root(&self, index: u32) -> bool {
        (FIRST_ROOT..FIRST_ROOT + self.roots).contains(&index)
    }

    fn len(&self) -> u32 {
        self.at.len() as u32
    }
}

/// The index of the crate list's open arena ([`Layout`]): [`AllCrates`] alone.
pub(crate) const LIST_ARENA: u32 = 1;

/// The index of the first source root's arena ([`Layout`]).
const FIRST_ROOT: u32 = 2;

/// What [`commit`] did.
pub(crate) struct Made {
    pub(crate) snap: Snap,
    pub(crate) dropped: Box<[u32]>,
    /// Each arena the commit marked, by index, with why: what its span says ([`describe`]).
    /// Empty for a first commit, which marks nothing (it makes every arena).
    pub(crate) marked: Vec<Mark, Global>,
    /// Why the commit started a new session, carrying nothing; `None` when it carried.
    pub(crate) fresh: Option<Fresh>,
    /// The arena capacity of the new session, when it started one ([`World::arenas`]).
    pub(crate) arenas: Option<u32>,
    /// Whether the new session is of a new id space (`Memo::builder`): an index of the old one
    /// had made its last generation, so the old space can hand out no id the session needs.
    /// Every id of the snapshot is then one an earlier snapshot of the database may have had,
    /// and the caller gives the database another nonce, the identity every holder of ids
    /// outside the memo keys by ([`crate::Db::nonce`]).
    pub(crate) renewed: bool,
}

/// Why a commit started a new session (every arena made again) instead of carrying the old
/// one's arenas: what the commit's span says, so a profile can tell a rare full remake apart.
/// A new session goes on in the database's id space (`Memo::next_session`): each arena takes
/// its index's next generation, so no id of the new session is one an old arena had and a
/// holder of ids outside the memo sees every old one as dropped.
///
/// A write never starts one: a sealed arena is dropped with its borrowers as any arena is
/// (`Memo::next`, "Sealed arenas" on `Memo::changed`), so a commit of a carried snapshot
/// starts a new session only when the memo cannot carry it (the cases below but the first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fresh {
    /// The database's first commit.
    First,
    /// The world has more arenas (`needed`) than the session was made with (`capacity`).
    Outgrown { needed: u32, capacity: u32 },
    /// The arena to drop at this id has spent its generations in the database's id space
    /// (`NextRefused::GenerationsSpent`): the session is of a new id space ([`Made::renewed`]).
    GenerationsSpent(ArenaId),
    /// The last commit failed after the old snapshot was asked for its next
    /// ([`Memo::has_successor`]): a maker panicked while the next was made, and the edit was
    /// dropped. The old snapshot answers still but makes no other next.
    AfterFailedCommit,
    /// A test's planted bug ([`Plant::SealedRestartsSession`]): a sealed arena was marked.
    Planted,
}

impl std::fmt::Display for Fresh {
    /// What the commit's span records as `fresh`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fresh::First => f.write_str("first"),
            Fresh::Outgrown { needed, capacity } => {
                write!(f, "arenas outgrown: {needed} needed, the session was made with {capacity}")
            }
            Fresh::GenerationsSpent(arena) => write!(f, "generations spent at {arena:?}"),
            Fresh::AfterFailedCommit => f.write_str("after a failed commit"),
            Fresh::Planted => f.write_str("a planted bug: a sealed arena marked"),
        }
    }
}

/// An arena a commit marked: its index, what it holds, whether the old snapshot had it
/// sealed, and why it was marked (the first cause found; one arena may have more).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Mark {
    pub(crate) index: u32,
    pub(crate) who: Who,
    pub(crate) sealed: bool,
    pub(crate) cause: Cause,
}

/// What a marked arena holds: in the next snapshot's layout, or else the old one's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Who {
    Session,
    /// The crate list's arena ([`LIST_ARENA`]).
    List,
    Root(SourceRootId),
    Crate(CrateKey),
    /// No crate or root is at this index in either snapshot.
    Empty,
}

/// Why a commit marked an arena: what the `db commit` span names for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cause {
    /// A session datum or maker was written (the session arena).
    Session(SessionWrite),
    /// The crate list's digest moved ([`List`]): a crate listed, taken out or put in another
    /// place, or a listed crate's item key or root file's path moved (the session arena).
    List,
    /// A file of no source root, or the session's own file data, was written.
    SessionFiles,
    /// The crate's own entry was written, or its content ID moved with a dependency's.
    Crate(CrateKey, CrateWrite),
    /// Another crate or root (or none) is at this index than the old snapshot had.
    IndexMoved,
    /// The source root's file set was written, or a file of it (`Some`).
    Root(SourceRootId, Option<FileId>),
    /// A crate's attribute file was written.
    AttrFile(FileId),
    /// A file read outside the VFS that the crate read was written.
    Included,
    /// The arena is sealed now (`true`) or no longer (`false`), as [`Layout::of`] decides it,
    /// and the old snapshot's was not: a root a sealed crate came to be rooted in, or a crate
    /// whose dependency's seal moved. A carried arena keeps the seal it was made with, so
    /// one whose seal moved is made again ([`blaze_memo::Builder::build`] refuses a sealed
    /// arena borrowing an unsealed one).
    Sealed(bool),
}

/// What of the session an edit wrote first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SessionWrite {
    /// A datum ([`SessionField`]), by name.
    Field(&'static str),
    /// A maker, given at this place ([`crate::Edit::set_given`]).
    Given(&'static std::panic::Location<'static>),
    /// Moved with no write logged: a new world's session, never made yet.
    Unlogged,
}

/// What of a crate an edit wrote first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CrateWrite {
    Made,
    /// A field ([`Field`]), by name.
    Field(&'static str),
    /// Its content ID alone: a dependency's moved, or its root file's path.
    Content,
    AttrFiles,
    /// A gift of the kind named.
    Gift(&'static str),
    /// A maker, given at this place ([`crate::Edit::set_given`]).
    Given(&'static std::panic::Location<'static>),
    /// Listed (`true`) or taken out of the list.
    Listed(bool),
    /// The other `OUT_DIR`s of its source root moved ([`OutDirs`]): a crate of its root
    /// added, taken out, or given another `OUT_DIR`.
    OutDirs,
    /// Marked with no write logged.
    Unlogged,
}

impl Field {
    fn name(&self) -> &'static str {
        match self {
            Field::Data(_) => "data",
            Field::Extra(_) => "extra",
            Field::Workspace(_) => "workspace",
            Field::Cfg(_) => "cfg",
            Field::Env(_) => "env",
            Field::Lints(_) => "lints",
        }
    }
}

impl SessionField {
    fn name(&self) -> &'static str {
        match self {
            SessionField::LibraryRoots(_) => "library roots",
            SessionField::LocalRoots(_) => "local roots",
            SessionField::ProcessEnv(_) => "process env",
            SessionField::Settings(_) => "settings",
            SessionField::Primary(_) => "primary crates",
        }
    }
}

/// The open edit's first write of the session and of each crate, read from the undo log in
/// the order written: O(the edit's writes), once a commit that carries.
struct Writes {
    session: Option<SessionWrite>,
    crates: HashMap<CrateKey, CrateWrite, Global>,
}

impl Writes {
    fn of(world: &World) -> Writes {
        let crates = &world.crates;
        let mut writes = Writes { session: None, crates: HashMap::new_in(Global) };
        let maker_at = |makers: &[Maker], id: ContentId, was: &Option<Maker>| {
            let now = makers.binary_search_by(|it| it.fact.id.cmp(&id)).ok().map(|at| makers[at].at);
            now.or_else(|| was.as_ref().map(|it| it.at))
        };
        for undo in &crates.undo {
            let (key, write) = match undo {
                Undo::Made(key) => (*key, CrateWrite::Made),
                Undo::Field(key, was) => (*key, CrateWrite::Field(was.name())),
                Undo::Content(key, _) => (*key, CrateWrite::Content),
                Undo::AttrFiles(key, _) => (*key, CrateWrite::AttrFiles),
                Undo::Gift(key, kind, was) => {
                    let now = crates.get(*key).gifts.iter().find(|it| it.kind == *kind);
                    // A located kind's first gift marks nothing ([`Crates::give_located`]): it is
                    // no cause, and naming it would hide the write of a bundle given in the same
                    // edit that did mark (a dependency's image beside its lang items).
                    if was.is_none() && now.is_some_and(|it| it.located) && world.plant != Plant::LocatedMarks {
                        continue;
                    }
                    let name = now.map(|it| it.name).or_else(|| was.as_ref().map(|it| it.name)).unwrap_or("a gift");
                    (*key, CrateWrite::Gift(name))
                }
                Undo::Maker(Some(key), id, was) => match maker_at(&crates.get(*key).makers, *id, was) {
                    Some(at) => (*key, CrateWrite::Given(at)),
                    None => (*key, CrateWrite::Unlogged),
                },
                Undo::Listed(key, was) => (*key, CrateWrite::Listed(!*was)),
                Undo::Maker(None, id, was) => {
                    let write = maker_at(&world.session.makers, *id, was).map_or(SessionWrite::Unlogged, SessionWrite::Given);
                    writes.session.get_or_insert(write);
                    continue;
                }
                // Not of the session arena: each crate whose flag moved is given it.
                Undo::Session(SessionField::Primary(_)) => continue,
                Undo::Session(field) => {
                    writes.session.get_or_insert(SessionWrite::Field(field.name()));
                    continue;
                }
                Undo::AttrOf(..)
                | Undo::OrderPushed
                | Undo::OrderPopped(_)
                | Undo::Order(_)
                | Undo::Unique(_) => continue,
            };
            writes.crates.entry(key).or_insert(write);
        }
        writes
    }
}

/// Arena indices marked, each with the cause found first: sorted by index, one per index, once
/// [`marked`] returns it.
type Marked = Vec<(u32, Cause), Global>;

fn mark(marked: &mut Marked, index: Option<u32>, cause: Cause) {
    if let Some(index) = index {
        marked.push((index, cause));
    }
}

/// The arenas a commit marks, each with why: what moved since `old`, by where it was and
/// where it goes. `list_moved`: the crate list's digest differs from `old`'s ([`List`]);
/// `out_dirs_moved`: the crates whose other `OUT_DIR`s differ from `old`'s ([`OutDirs`]).
fn marked(
    world: &World,
    files: &Files,
    layout: &Layout,
    old: &Table,
    marks: FileMarks,
    list_moved: bool,
    out_dirs_moved: &[CrateKey],
) -> Marked {
    let mut marked: Marked = Vec::new_in(Global);
    if world.plant == Plant::EditMarksNothing {
        return marked;
    }
    let writes = Writes::of(world);
    let old_index =
        |key: CrateKey| old.at_index.iter().position(|it| *it == Some(key)).map(|it| it as u32);
    if world.session.moved {
        mark(&mut marked, Some(0), Cause::Session(writes.session.unwrap_or(SessionWrite::Unlogged)));
    }
    if list_moved {
        let at = if world.plant == Plant::ListMarksSession { 0 } else { LIST_ARENA };
        mark(&mut marked, Some(at), Cause::List);
    }
    // A located gift given for the first time marks nothing ([`Crates::give_located`]).
    if world.plant == Plant::LocatedMarks {
        for &key in &world.crates.located {
            let cause = Cause::Crate(key, writes.crates.get(&key).copied().unwrap_or(CrateWrite::Unlogged));
            mark(&mut marked, old_index(key), cause);
        }
    }
    // A crate whose root's other `OUT_DIR`s moved ([`crate::other_out_dirs`], a field of its
    // own): its arena alone, and its borrowers with it.
    for &key in out_dirs_moved {
        mark(&mut marked, old_index(key), Cause::Crate(key, CrateWrite::OutDirs));
    }
    if marks.session {
        mark(&mut marked, Some(0), Cause::SessionFiles);
    }
    for &key in &world.crates.dirty {
        let cause = Cause::Crate(key, writes.crates.get(&key).copied().unwrap_or(CrateWrite::Unlogged));
        mark(&mut marked, old_index(key), cause);
        mark(&mut marked, layout.index_of.get(&key).copied(), cause);
    }
    // A crate that moved to another index is made again at both.
    let span = old.at_index.len().max(layout.at.len());
    for index in 1..span as u32 {
        let was = old.at_index.get(index as usize).copied().flatten();
        let now = layout.at.get(index as usize).copied().flatten();
        if was != now {
            mark(&mut marked, Some(index), Cause::IndexMoved);
        }
    }
    // An arena whose seal moved is made again with its new seal: the memo checks a sealed
    // arena's borrows once, when the snapshot is built, against each arena's seal as made.
    // A root's seal moves with no write of the root (a sealed crate rooted in it listed, or
    // one there unsealed), which nothing above marks; while the crate list was the session's,
    // a crate listed dropped every arena and hid it.
    if world.plant != Plant::SealCarried {
        let span = old.sealed.len().max(layout.sealed.len());
        for index in 1..span as u32 {
            let was = old.sealed.get(index as usize).copied().unwrap_or(false);
            let now = layout.sealed.get(index as usize).copied().unwrap_or(false);
            if was != now {
                mark(&mut marked, Some(index), Cause::Sealed(now));
            }
        }
    }
    // The arena a root was placed in: its own (`Layout`); none for a root the old snapshot did
    // not have.
    let old_owner = |root: SourceRootId| -> Option<u32> {
        old.arena_roots.iter().position(|roots| roots.contains(&root)).map(|it| it as u32)
    };
    let mut roots: Vec<(SourceRootId, Cause), Global> = Vec::new_in(Global);
    roots.extend(marks.roots.iter().map(|&root| (root, Cause::Root(root, None))));
    for &file in &marks.files {
        if let Some(&krate) = world.crates.attr_of.get(&file) {
            mark(&mut marked, layout.index_of.get(&krate).copied(), Cause::AttrFile(file));
            continue;
        }
        match files.root_of(file) {
            Some(root) => roots.push((root, Cause::Root(root, Some(file)))),
            // A file of no root lives in the session arena.
            None => mark(&mut marked, Some(0), Cause::SessionFiles),
        }
    }
    // A root whose arena moved to another index is placed again at both.
    for root in files.roots() {
        if old_owner(root) != layout.owner(root) {
            roots.push((root, Cause::IndexMoved));
        }
    }
    for (root, cause) in roots {
        mark(&mut marked, old_owner(root), cause);
        mark(&mut marked, layout.owner(root), cause);
    }
    for row in marks.included {
        for key in files.included_readers(row) {
            mark(&mut marked, layout.index_of.get(&key).copied(), Cause::Included);
        }
    }
    // By index, the cause found first kept (a stable sort).
    marked.sort_by_key(|it| it.0);
    marked.dedup_by_key(|it| it.0);
    marked
}

/// The marks of `marked`, each with what its arena holds and whether `old` had it sealed.
fn marks_of(marked: &Marked, layout: &Layout, old: &Table) -> Vec<Mark, Global> {
    let mut marks = Vec::with_capacity_in(marked.len(), Global);
    for &(index, cause) in marked {
        let at = index as usize;
        let who = if index == 0 {
            Who::Session
        } else if index == LIST_ARENA {
            Who::List
        } else if let Some(Some(key)) = layout.at.get(at) {
            Who::Crate(*key)
        } else if let Some(&root) = layout.owned.get(at).and_then(|it| it.first()) {
            Who::Root(root)
        } else if let Some(Some(key)) = old.at_index.get(at) {
            Who::Crate(*key)
        } else if let Some(&root) = old.arena_roots.get(at).and_then(|it| it.first()) {
            Who::Root(root)
        } else {
            Who::Empty
        };
        let sealed = old.sealed.get(at).copied().unwrap_or(false);
        marks.push(Mark { index, who, sealed, cause });
    }
    marks
}

/// How many marks [`describe`] names; the rest it counts.
pub(crate) const MARKS_NAMED: usize = 8;

/// What a commit's span records as `causes`: the first [`MARKS_NAMED`] marks, each as
/// `index who [sealed]: cause`, joined by `; `, then how many more; and when the commit
/// started a new session, what forced it, with the cause of the arena it names.
pub(crate) fn describe(
    world: &World,
    marks: &[Mark],
    fresh: Option<Fresh>,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    for (at, it) in marks.iter().take(MARKS_NAMED).enumerate() {
        if at > 0 {
            out.write_all(b"; ")?;
        }
        write!(out, "{} ", it.index)?;
        match it.who {
            Who::Session => out.write_all(b"session")?,
            Who::List => out.write_all(b"crate list")?,
            Who::Root(root) => write!(out, "root {}", root.0)?,
            Who::Crate(key) => write_crate(world, key, out)?,
            Who::Empty => out.write_all(b"empty")?,
        }
        if it.sealed {
            out.write_all(b" sealed")?;
        }
        out.write_all(b": ")?;
        write_cause(world, it.cause, out)?;
    }
    if marks.len() > MARKS_NAMED {
        write!(out, "; and {} more", marks.len() - MARKS_NAMED)?;
    }
    if let Some(fresh) = fresh {
        if !marks.is_empty() {
            out.write_all(b"; ")?;
        }
        write!(out, "fresh: {fresh}")?;
        if let Fresh::GenerationsSpent(arena) = fresh
            && let Some(it) = marks.iter().find(|it| it.index == arena.index())
        {
            out.write_all(b", marked for ")?;
            write_cause(world, it.cause, out)?;
        }
    }
    Ok(())
}

/// A crate's display name, or its key where it has none or is gone.
fn write_crate(world: &World, key: CrateKey, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    match world.crates.entries.get(key.0 as usize) {
        Some(Some(entry)) => match &entry.extra.display_name {
            Some(name) => out.write_all(name.canonical_name().as_str().as_bytes()),
            None => write!(out, "crate {}", key.0),
        },
        _ => write!(out, "crate {} (gone)", key.0),
    }
}

fn write_cause(world: &World, cause: Cause, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    match cause {
        Cause::Session(SessionWrite::Field(field)) => write!(out, "session {field} written"),
        Cause::Session(SessionWrite::Given(at)) => write!(out, "session maker given at {at}"),
        Cause::Session(SessionWrite::Unlogged) => out.write_all(b"session moved"),
        Cause::List => out.write_all(b"the crate list moved"),
        Cause::SessionFiles => out.write_all(b"a session file written"),
        Cause::Crate(key, write) => {
            write_crate(world, key, out)?;
            match write {
                CrateWrite::Made => out.write_all(b" made"),
                CrateWrite::Field(field) => write!(out, " {field} written"),
                CrateWrite::Content => out.write_all(b" content moved (a dependency's, or its root path)"),
                CrateWrite::AttrFiles => out.write_all(b" attribute files set"),
                CrateWrite::Gift(kind) => write!(out, " given {kind}"),
                CrateWrite::Given(at) => write!(out, " maker given at {at}"),
                CrateWrite::Listed(true) => out.write_all(b" listed"),
                CrateWrite::Listed(false) => out.write_all(b" taken out of the list"),
                CrateWrite::OutDirs => out.write_all(b" its root's other OUT_DIRs moved"),
                CrateWrite::Unlogged => out.write_all(b" marked"),
            }
        }
        Cause::IndexMoved => out.write_all(b"index moved"),
        Cause::Root(root, None) => write!(out, "root {} written", root.0),
        Cause::Root(root, Some(file)) => write!(out, "file {} of root {} written", file.index(), root.0),
        Cause::AttrFile(file) => write!(out, "attribute file {} written", file.index()),
        Cause::Included => out.write_all(b"a file read outside the VFS written"),
        Cause::Sealed(true) => out.write_all(b"sealed now"),
        Cause::Sealed(false) => out.write_all(b"no longer sealed"),
    }
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
    // The crate list, made again only where the shape moved (a listed crate's content ID, the
    // list or the order); else the old table's, which the session arena was made with.
    let list: Option<List> = match (&old, world.crates.shape_moved) {
        (Some(old), false) => old.table.list.map(|digest| List {
            digest,
            item_keys: std::sync::Arc::clone(&old.table.item_keys),
        }),
        _ => None,
    };
    let list = list.unwrap_or_else(|| world.crates.list(files));
    // Each crate's other `OUT_DIR`s, and those that moved since the old table's.
    let out_dirs = out_dirs_of(world, files);
    let mut out_dirs_moved: Vec<CrateKey, Global> = Vec::new_in(Global);
    if let Some(old) = old.as_deref() {
        out_dirs_moved.extend(
            world.crates.order.iter().copied().filter(|key| old.table.out_dirs.get(key) != out_dirs.get(key)),
        );
    }

    // The carried arenas, or none: a first commit, an arena capacity outgrown, or an old
    // snapshot a failed commit asked for its next already, starts a new session. A write never
    // does: a sealed arena marked is dropped with its borrowers, as any arena is.
    let mut marked_list: Vec<Mark, Global> = Vec::new_in(Global);
    let carried: Result<_, Fresh> = match old.as_deref() {
        None => Err(Fresh::First),
        Some(_) if layout.len() > world.arenas => {
            Err(Fresh::Outgrown { needed: layout.len(), capacity: world.arenas })
        }
        Some(old) if old.memo.has_successor() => Err(Fresh::AfterFailedCommit),
        Some(old) => {
            // The digest is content; the item keys' list also names each crate by its key.
            let list_moved = old.table.list != Some(list.digest) || old.table.item_keys[..] != list.item_keys[..];
            let marked = marked(world, files, &layout, &old.table, marks, list_moved, &out_dirs_moved);
            marked_list = marks_of(&marked, &layout, &old.table);
            let mut ids: Vec<ArenaId, Global> = Vec::new_in(Global);
            ids.extend(old.memo.arenas().filter(|it| marked.binary_search_by_key(&it.index(), |m| m.0).is_ok()));
            let planted = world.plant == Plant::SealedRestartsSession && marked_list.iter().any(|it| it.sealed);
            if planted {
                Err(Fresh::Planted)
            } else {
                match old.memo.next(&ids) {
                    // An arena to drop has spent its generations in the database's id space
                    // (`NextRefused::GenerationsSpent`): the session ends here and one of a
                    // new id space is made below, under another nonce (`Made::renewed`). Rare
                    // (2^generation bits makings of one index in the database's life), so the
                    // session's making says it.
                    Err(blaze_memo::NextRefused::GenerationsSpent { arena }) => {
                        Err(Fresh::GenerationsSpent(arena))
                    }
                    Ok(next) => {
                        let mut dropped: Vec<u32, Global> = Vec::new_in(Global);
                        dropped.extend_from_slice(next.dropped());
                        Ok((next.builder, dropped))
                    }
                }
            }
        }
    };
    let fresh = carried.as_ref().err().copied();
    let mut renewed = false;
    let (mut builder, dropped, arenas) = match carried {
        Ok((builder, dropped)) => (builder, dropped, None),
        Err(_) => {
            let capacity = (2 * layout.len()).clamp(64, blaze_memo::MAX_ARENAS);
            let config = || blaze_memo::Config {
                arenas: capacity,
                reads: world.reads,
                flight: std::sync::Arc::clone(&world.flight),
                cap_bytes: u64::MAX,
                plant: blaze_memo::Plant::None,
            };
            // A new session of this database goes on in its id space, so no arena it makes
            // takes an id an old arena had. Only the first commit, and one after an index spent
            // its generations in the space, start a new id space (the caller gives the
            // database another nonce for the second, `Made::renewed`).
            let mut builder = match old.as_deref() {
                None => Memo::builder(config()),
                Some(_) if world.plant == Plant::FreshRestartsIds => Memo::builder(config()),
                Some(old) => match old.memo.next_session(config()) {
                    Ok(builder) => builder,
                    Err(refused @ blaze_memo::NextRefused::GenerationsSpent { .. }) => {
                        tracing::info!(
                            target: "blaze",
                            "a commit starts a new id space, and the database another nonce: {refused}"
                        );
                        renewed = true;
                        Memo::builder(config())
                    }
                },
            };
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
    // The dropped arenas made again from the world: their inputs, slots and the makers given
    // them run (`make_given`). Its own span, apart from the old snapshot's release (`db commit
    // release`), so a slow commit says which it was.
    let making = tracing::info_span!(target: "blaze", "db commit make", arenas = tracing::field::Empty).entered();
    let mut remade = 0_u64;
    for index in 0..layout.len() {
        if builder.has(index) {
            continue;
        }
        remade += 1;
        remake(world, files, &layout, &mut builder, &mut rows, base, index, list.digest, &out_dirs);
    }
    making.record("arenas", remade);
    let memo = builder.build();
    drop(making);

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
    let made_list = rows.list;
    table.apply(rows);
    table.all = all;
    table.root_crates = root_crates;
    table.primary = primary;
    // The list of item keys, its index by prefix and its digest, put with the list's arena
    // they were made into: a list whose digest or keys moved marks that arena, so a carried
    // one has the carried table's. Made from the world as this commit has it, so a
    // rolled-back edit leaves no trace in it.
    if !carried_at.get(LIST_ARENA as usize).copied().unwrap_or(false) {
        (table.by_prefix, table.clash) = by_prefix(&list.item_keys);
        table.item_keys = list.item_keys;
        table.list = Some(list.digest);
        // The arena was made in this commit, and with it its one input.
        table.list_input = Some(made_list.unwrap_or_else(|| {
            panic!("base-db: the crate list's arena ({LIST_ARENA}) was made, and no crate-list input with it")
        }));
    }
    table.out_dirs = out_dirs;
    // Every crate's located gifts, made whole: a few `Arc`s a crate.
    table.located.clear();
    for &key in &world.crates.order {
        let at = key.0 as usize;
        if table.located.len() <= at {
            table.located.resize_with(at + 1, || Vec::new_in(Global));
        }
        let row = &mut table.located[at];
        row.extend(world.crates.get(key).gifts.iter().filter(|it| it.located).map(|it| {
            let GiftFact::Given(fact) = it.fact else {
                unreachable!("a located gift is given by its giver's fact (`Crates::give_located`)")
            };
            // clone: the gift's `Arc`, once per located gift a commit
            (it.kind, fact, it.value.clone())
        }));
    }

    Made {
        snap: Snap { memo, table, seq },
        dropped: dropped.into_boxed_slice(),
        marked: marked_list,
        fresh,
        arenas,
        renewed,
    }
}

/// Makes arena `index`'s inputs from the world, its rows into `rows`; a dependency carried
/// from the old session is read from `base`, its table. `list`: the crate list's digest
/// ([`List`]), what the list arena's [`AllCrates`] holds; `out_dirs`: each crate's other
/// `OUT_DIR`s.
#[allow(clippy::too_many_arguments, reason = "what one arena is made from")]
fn remake(
    world: &mut World,
    files: &Files,
    layout: &Layout,
    builder: &mut Builder,
    rows: &mut Rows,
    base: Option<&Table>,
    index: u32,
    list: ContentId,
    out_dirs: &OutDirs,
) {
    if index == LIST_ARENA {
        // The crate list: open to every key, borrowing only the session ([`Layout`]); never
        // sealed, as a sealed crate's lookup that finds no crate reads it and seals nothing.
        let session = builder.id(0).expect("the session arena is made first");
        let inputs = builder.arena(LIST_ARENA, Tag::Query);
        inputs.borrows(session);
        inputs.open();
        // An input of the list's arena, its handle kept in the table (`Table::list_input`):
        // not a singleton, which the engine keeps in the session arena alone.
        const LIST: &str = "blaze v2 crate list 1";
        let list = InputId(list);
        rows.list = Some(AllCrates::new(&inputs, facts::singleton(LIST, list), list));
        return;
    }
    if index == 0 {
        let inputs = builder.arena(0, Tag::Query);
        make_session(world, files, &inputs);
        // Every source root has an arena of its own ([`Layout`]): the session holds the files
        // of none.
        let mut placed = rows.placed(0);
        files.make_rootless(&inputs, &|file| world.crates.attr_of.contains_key(&file), &mut placed);
        // `inputs` (a borrow of the builder) ends above: the driver's makers are given the
        // builder and the arena's inputs again ([`Builder::inputs`]).
        make_given(builder, 0, &world.session.makers);
        return;
    }
    if layout.is_root(index) {
        // A source root's files: open to every crate's keys, borrowing only the session
        // ([`Layout`]).
        let session = builder.id(0).expect("the session arena is made first");
        let inputs = builder.arena(index, Tag::Query);
        inputs.borrows(session);
        inputs.open();
        if layout.sealed[index as usize] {
            inputs.seal();
        }
        let mut placed = rows.placed(index);
        for &root in &layout.owned[index as usize] {
            files.make_root(&inputs, root, &mut placed);
        }
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
    // The arena of its root file's source root, which its def map reads before anything else;
    // another root's files it reads are borrowed when read ([`Layout`]).
    if let Some(owner) = files.root_of(entry.data.root_file_id).and_then(|root| layout.owner(root)) {
        borrows.push(builder.id(owner).expect("a root's arena is made before every crate's"));
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
    files.make_files(&inputs, &entry.attr_files, &mut placed);
    let mut slots = Vec::with_capacity_in(entry.gifts.len(), Global);
    // A located gift is the table's ([`Table::located`]), no slot of the arena.
    for gift in entry.gifts.iter().filter(|it| !it.located) {
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
        // clone: its directories' names, once per making of its arena
        out_dirs.get(&key).cloned().unwrap_or_default(),
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

/// The session arena's singletons. The crate list is the list arena's ([`LIST_ARENA`]).
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
    session.settings.set_in(inputs);
    files.make_session(inputs);
}

/// The fact of a file read outside the VFS ([`crate::IncludedFile`]): of its path, its digest
/// its read's content.
pub(crate) fn included_fact(path: &[u8], content: ContentId) -> Fact {
    Fact { id: ContentId::of(crate::files::INCLUDED_PATH_DOMAIN, path), digest: content }
}
