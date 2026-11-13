//! Record kinds as declarations: one [`Kind`] per kind of record, saying everything the driver,
//! the store and the memo need to know about it. Adding a kind, or moving where one lives, is one
//! declaration here.
//!
//! What is derived and what is written by hand: a root's schema (its layout) comes from the field
//! list `blaze_format::record!` reflects at compile time; a key's field list is hand-written in
//! each [`Kind::key`], in the order of its inputs struct, through [`KeyHasher`]. When blaze's
//! compile-time reflection can walk a struct's fields (`for const` over fields), `key` becomes
//! one generic function over `Inputs`; until then each kind's `key` is reviewed against its
//! inputs struct, field for field.

#![blaze::sans_io]

use core::{cmp::Ordering, fmt, hash, marker::PhantomData};

use blaze_ids::{ContentId, Hasher};

use crate::{
    Plant, planted,
    record::{Headed, ManifestRoot, Opaque, ProgramRoot, Sections, Traced},
    text::NameHash,
};

/// A kind's number in a record's head and the store's index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Tag(pub u16);

/// How a record's bytes are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// A blaze-format flat record, small: read by `pread` into the reading task's frame.
    Flat,
    /// A table of plain arrays (a file image, a crate surface's fact tables): mapped from its
    /// segment and read in place.
    Plain,
}

/// The smallest thing one record is about: what changes alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grain {
    /// One request (the certificate).
    Request,
    /// One source file.
    File,
    /// One crate's exported facts: a container of per-fact digests, compared per fact.
    Surface,
    /// One body: a fn, a const, a static's initializer.
    Item,
    /// One instance of a body at its type arguments.
    Instance,
    /// One macro call.
    Call,
    /// One build-script run or one test.
    Run,
    /// One unit's list of what it holds.
    Unit,
    /// One program's list of instances.
    Program,
}

/// Where a kind's records live between requests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keep {
    /// In the store only: every ask reads it.
    Disk,
    /// In the store, and in [`crate::memo::Resident`]'s table while a process stays alive.
    DiskAndResident,
}

/// What a record says about its own validity beyond its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trace {
    /// The key is all: an equal key is the record.
    Key,
    /// The record carries the reads its work made, at item grain ((fact id, digest), absence
    /// included). On a key miss the old record's list is checked against the current facts: all
    /// equal and the old result is filed under the new key, byte for byte; else the work is
    /// redone.
    Reuse,
    /// The record carries the outside reads its run made (environment, files): a hit needs an
    /// equal key and every outside read still equal.
    Outside,
}

/// One kind of record. Every constant is read at compile time: a generic over `K: Kind` folds
/// each branch on them away.
///
/// The invariant: [`Kind::key`] covers every input the record's value depends on, except what a
/// [`Trace::Reuse`] or [`Trace::Outside`] list covers (rule 21); two values under one key are a
/// typed refusal ([`crate::store::Put::Other`]).
pub trait Kind: Sized + 'static {
    const TAG: Tag;
    const NAME: &'static str;
    /// The hash domain of the key.
    const DOMAIN: &'static str;
    /// The version of what the key covers and of the root: a change to either bumps it.
    const VERSION: u16;
    const LAYOUT: Layout;
    const GRAIN: Grain;
    const TRACE: Trace;
    /// A default until the per-kind measurement of target.md, "Choosing what is resident".
    const KEEP: Keep;
    /// The record's root: the record a buffer of this kind opens to.
    type Root: Headed;
    /// What the key is computed from, all known before the work.
    type Inputs<'a>;
    fn key(inputs: &Self::Inputs<'_>) -> Key<Self>;
}

/// The key of a record of kind `K`: the 32-byte content id its inputs hash to.
pub struct Key<K> {
    id: ContentId,
    of: PhantomData<fn() -> K>,
}

impl<K> Key<K> {
    pub const fn from_id(id: ContentId) -> Key<K> {
        Key { id, of: PhantomData }
    }

    pub const fn id(&self) -> ContentId {
        self.id
    }
}

impl<K> Clone for Key<K> {
    fn clone(&self) -> Key<K> {
        *self
    }
}

impl<K> Copy for Key<K> {}

impl<K> PartialEq for Key<K> {
    fn eq(&self, other: &Key<K>) -> bool {
        self.id == other.id
    }
}

impl<K> Eq for Key<K> {}

impl<K> PartialOrd for Key<K> {
    fn partial_cmp(&self, other: &Key<K>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Key<K> {
    fn cmp(&self, other: &Key<K>) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl<K> hash::Hash for Key<K> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<K: Kind> fmt::Debug for Key<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", K::NAME, self.id)
    }
}

/// The one way a key is hashed: the kind's domain, tag and version first, then each input in
/// the order of the inputs struct, each length-prefixed or fixed-width, so two field lists never
/// hash alike.
pub struct KeyHasher(Hasher);

impl KeyHasher {
    pub fn of<K: Kind>() -> KeyHasher {
        let mut hasher = Hasher::new(K::DOMAIN);
        hasher.update(&K::TAG.0.to_le_bytes()).update(&K::VERSION.to_le_bytes());
        KeyHasher(hasher)
    }

    pub fn id(&mut self, id: &ContentId) -> &mut KeyHasher {
        self.0.id(id);
        self
    }

    pub fn opt_id(&mut self, id: Option<&ContentId>) -> &mut KeyHasher {
        match id {
            None => self.0.update(&[0]),
            Some(id) => self.0.update(&[1]).id(id),
        };
        self
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> &mut KeyHasher {
        self.0.part(bytes);
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut KeyHasher {
        self.0.update(&value.to_le_bytes());
        self
    }

    /// A list: its length, then each id.
    pub fn ids(&mut self, ids: &[ContentId]) -> &mut KeyHasher {
        self.u64(ids.len() as u64);
        ids.iter().for_each(|id| {
            self.0.id(id);
        });
        self
    }

    pub fn keys<T>(&mut self, keys: &[Key<T>]) -> &mut KeyHasher {
        self.u64(keys.len() as u64);
        keys.iter().for_each(|key| {
            self.0.id(&key.id);
        });
        self
    }

    pub fn late(&mut self, late: &[LateRead<'_>]) -> &mut KeyHasher {
        self.u64(late.len() as u64);
        for read in late {
            self.0.update(&[read.late as u8]).part(read.name);
            self.opt_id(read.value.as_ref());
        }
        self
    }

    pub fn finish<K>(&self) -> Key<K> {
        Key::from_id(self.0.finish())
    }
}

/// The compiler's own code identity: every key starts from it, so a new blaze keys apart.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ToolId(pub ContentId);

/// The digest of a unit's typed settings: cfg, features, target, edition, profile, lint levels.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Settings(pub ContentId);

/// A built-in macro whose value is read late, at expansion: its value is in the key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Late {
    Env = 1,
    OptionEnv = 2,
    Include = 3,
    IncludeStr = 4,
    IncludeBytes = 5,
}

/// One late read: which macro, of what (a variable's name, a path), and the value's digest
/// (`None`: absent, which is a value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LateRead<'a> {
    pub late: Late,
    pub name: &'a [u8],
    pub value: Option<ContentId>,
}

/// One name a body mentions and the closure of what it can resolve to in the body's universe
/// ([`crate::text::Graph::name_closure`]); `None` when nothing is found under it, so a name that
/// appears later moves the key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NameRead {
    pub name: NameHash,
    pub closure: Option<ContentId>,
}

macro_rules! kind {
    (
        $(#[$meta:meta])*
        $name:ident {
            tag: $tag:literal, version: $version:literal, layout: $layout:ident,
            grain: $grain:ident, trace: $trace:ident, keep: $keep:ident, root: $root:ty,
            inputs: $inputs:ident,
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct $name;

        impl Kind for $name {
            const TAG: Tag = Tag($tag);
            const NAME: &'static str = ::core::stringify!($name);
            const DOMAIN: &'static str = ::core::concat!("blaze-keyed ", ::core::stringify!($name), " key");
            const VERSION: u16 = $version;
            const LAYOUT: Layout = Layout::$layout;
            const GRAIN: Grain = Grain::$grain;
            const TRACE: Trace = Trace::$trace;
            const KEEP: Keep = Keep::$keep;
            type Root = $root;
            type Inputs<'a> = $inputs<'a>;
            fn key(inputs: &$inputs<'_>) -> Key<$name> {
                inputs.key()
            }
        }
    };
}

kind! {
    /// A request's certificate: what the last answer read, checked before anything else.
    Certificate {
        tag: 1, version: 1, layout: Flat, grain: Request, trace: Key, keep: Disk, root: Opaque,
        inputs: CertificateInputs,
    }
}

/// The certificate's key is its base: the request's shape and the tool (blaze-certify's
/// `certificate::base`).
pub struct CertificateInputs<'a> {
    pub tool: ToolId,
    pub base: ContentId,
    pub of: PhantomData<&'a ()>,
}

impl CertificateInputs<'_> {
    fn key(&self) -> Key<Certificate> {
        KeyHasher::of::<Certificate>().id(&self.tool.0).id(&self.base).finish()
    }
}

kind! {
    /// A file's image: tokens, the flat tree, each item's piece hash, the names, the line table.
    FileImage {
        tag: 2, version: 1, layout: Plain, grain: File, trace: Key, keep: DiskAndResident,
        root: Sections, inputs: FileInputs,
    }
}

pub struct FileInputs<'a> {
    pub tool: ToolId,
    pub edition: u16,
    pub bytes: &'a [u8],
}

impl FileInputs<'_> {
    fn key(&self) -> Key<FileImage> {
        KeyHasher::of::<FileImage>()
            .id(&self.tool.0)
            .u64(u64::from(self.edition))
            .bytes(self.bytes)
            .finish()
    }
}

kind! {
    /// A crate's surface: scopes, exports, the impl index by (trait, simplified self type),
    /// signatures, field lists, layouts and const values, each an exported fact with its own
    /// digest; and the import table of what it read of other crates, by stable id and digest.
    Surface {
        tag: 3, version: 1, layout: Plain, grain: Surface, trace: Reuse, keep: DiskAndResident,
        root: Traced, inputs: SurfaceInputs,
    }
}

/// A surface's key: its own items' pieces (bodies cut, except bodies that count as interface:
/// see target.md, "Key rules") and its dependencies' surface keys. A dependency's moved key
/// moves this one; the import table then settles it, fact by fact.
pub struct SurfaceInputs<'a> {
    pub tool: ToolId,
    pub settings: Settings,
    pub pieces: &'a [ContentId],
    pub deps: &'a [Key<Surface>],
    pub late: &'a [LateRead<'a>],
}

impl SurfaceInputs<'_> {
    fn key(&self) -> Key<Surface> {
        KeyHasher::of::<Surface>()
            .id(&self.tool.0)
            .id(&self.settings.0)
            .ids(self.pieces)
            .keys(self.deps)
            .late(self.late)
            .finish()
    }
}

kind! {
    /// A body's check: inference, MIR, the borrow-check verdict and its item-relative
    /// diagnostics, with the read list the check made.
    Body {
        tag: 4, version: 1, layout: Flat, grain: Item, trace: Reuse, keep: DiskAndResident,
        root: Traced, inputs: BodyInputs,
    }
}

/// A body's text-closure key.
pub struct BodyInputs<'a> {
    pub tool: ToolId,
    pub settings: Settings,
    /// The sysroot as one unit: a new std moves every key.
    pub sysroot: ContentId,
    /// The item's own text, piece and body, hashed with its crate's identity.
    pub text: ContentId,
    /// Each name the text mentions, sorted by name, with what it can resolve to.
    pub names: &'a [NameRead],
    /// The crate's "always" closure: glob and `as _` imports, inner attributes, unexpanded item
    /// macros ([`crate::text::Graph::always`]).
    pub always: ContentId,
    pub late: &'a [LateRead<'a>],
    /// The expansions of the macro calls in the body, by key.
    pub expansions: &'a [Key<Expansion>],
}

impl BodyInputs<'_> {
    fn key(&self) -> Key<Body> {
        let mut hasher = KeyHasher::of::<Body>();
        hasher.id(&self.tool.0).id(&self.settings.0).id(&self.sysroot).id(&self.text);
        let names = match planted(Plant::KeyLeavesOutNames) && !self.names.is_empty() {
            true => &self.names[1..],
            false => self.names,
        };
        hasher.u64(names.len() as u64);
        for read in names {
            hasher.u64(read.name.0).opt_id(read.closure.as_ref());
        }
        hasher.id(&self.always).late(self.late).keys(self.expansions).finish()
    }
}

kind! {
    /// One instance's code: a body at its type arguments, compiled.
    Instance {
        tag: 5, version: 1, layout: Flat, grain: Instance, trace: Key, keep: Disk, root: Opaque,
        inputs: InstanceInputs,
    }
}

/// An instance's key, computed once its body is checked: the digest of the body's result (not
/// its key, so a body reused under a moved key keeps its code), the type arguments' keys, the
/// codegen settings and the keys of the instances it inlines.
pub struct InstanceInputs<'a> {
    pub tool: ToolId,
    pub settings: Settings,
    pub body: ContentId,
    pub args: &'a [ContentId],
    pub inlinees: &'a [Key<Instance>],
}

impl InstanceInputs<'_> {
    fn key(&self) -> Key<Instance> {
        KeyHasher::of::<Instance>()
            .id(&self.tool.0)
            .id(&self.settings.0)
            .id(&self.body)
            .ids(self.args)
            .keys(self.inlinees)
            .finish()
    }
}

kind! {
    /// One macro call's expansion.
    Expansion {
        tag: 6, version: 1, layout: Flat, grain: Call, trace: Key, keep: DiskAndResident,
        root: Opaque, inputs: ExpansionInputs,
    }
}

/// An expansion's key. Sound with no read list because an expander runs in Wasmtime with no
/// ambient reads: what it can see is its input, this context and nothing else.
pub struct ExpansionInputs<'a> {
    pub tool: ToolId,
    /// The expander's code identity (its wasm module's content).
    pub expander: ContentId,
    pub input: ContentId,
    /// An attribute macro's own arguments.
    pub attr: Option<ContentId>,
    /// What of the call site the expander can see: edition, crate name, the def-site hygiene
    /// context.
    pub context: ContentId,
    pub of: PhantomData<&'a ()>,
}

impl ExpansionInputs<'_> {
    fn key(&self) -> Key<Expansion> {
        KeyHasher::of::<Expansion>()
            .id(&self.tool.0)
            .id(&self.expander)
            .id(&self.input)
            .opt_id(self.attr.as_ref())
            .id(&self.context)
            .finish()
    }
}

kind! {
    /// One build-script run: its outputs and the outside reads it made.
    ScriptRun {
        tag: 7, version: 1, layout: Flat, grain: Run, trace: Outside, keep: Disk, root: Traced,
        inputs: ScriptInputs,
    }
}

pub struct ScriptInputs<'a> {
    pub tool: ToolId,
    /// The script's program.
    pub script: Key<ProgramManifest>,
    pub package: ContentId,
    /// The environment cargo defines for a script (declared, not traced).
    pub env: ContentId,
    pub settings: Settings,
    pub of: PhantomData<&'a ()>,
}

impl ScriptInputs<'_> {
    fn key(&self) -> Key<ScriptRun> {
        KeyHasher::of::<ScriptRun>()
            .id(&self.tool.0)
            .id(&self.script.id())
            .id(&self.package)
            .id(&self.env)
            .id(&self.settings.0)
            .finish()
    }
}

kind! {
    /// One test's result, with the outside reads it made.
    TestRun {
        tag: 8, version: 1, layout: Flat, grain: Run, trace: Outside, keep: Disk, root: Traced,
        inputs: TestInputs,
    }
}

/// A test's key: what its program reaches from the test's entry (the digest of the reachable
/// instances' keys), the test's path, and the run's settings.
pub struct TestInputs<'a> {
    pub tool: ToolId,
    pub reach: ContentId,
    pub test: &'a [u8],
    pub settings: Settings,
}

impl TestInputs<'_> {
    fn key(&self) -> Key<TestRun> {
        KeyHasher::of::<TestRun>()
            .id(&self.tool.0)
            .id(&self.reach)
            .bytes(self.test)
            .id(&self.settings.0)
            .finish()
    }
}

kind! {
    /// One unit's list: its surface's, bodies' and instances' keys, by item. Written after every
    /// record it names.
    UnitManifest {
        tag: 9, version: 1, layout: Flat, grain: Unit, trace: Key, keep: DiskAndResident,
        root: ManifestRoot, inputs: UnitInputs,
    }
}

/// A unit manifest's key: its files' keys, its late inputs and its dependencies' manifests' keys.
pub struct UnitInputs<'a> {
    pub tool: ToolId,
    pub settings: Settings,
    pub files: &'a [Key<FileImage>],
    pub late: &'a [LateRead<'a>],
    pub deps: &'a [Key<UnitManifest>],
}

impl UnitInputs<'_> {
    fn key(&self) -> Key<UnitManifest> {
        let mut hasher = KeyHasher::of::<UnitManifest>();
        hasher.id(&self.tool.0).id(&self.settings.0).keys(self.files);
        if !planted(Plant::KeyLeavesOutLateInputs) {
            hasher.late(self.late);
        }
        hasher.keys(self.deps).finish()
    }
}

kind! {
    /// One program: the instances it is made of, by key, and its image's digest.
    ProgramManifest {
        tag: 10, version: 1, layout: Flat, grain: Program, trace: Key, keep: Disk,
        root: ProgramRoot, inputs: ProgramInputs,
    }
}

/// A program's key: the link settings and its instances' keys, sorted and deduplicated.
pub struct ProgramInputs<'a> {
    pub tool: ToolId,
    pub link: ContentId,
    pub instances: &'a [Key<Instance>],
}

impl ProgramInputs<'_> {
    fn key(&self) -> Key<ProgramManifest> {
        KeyHasher::of::<ProgramManifest>()
            .id(&self.tool.0)
            .id(&self.link)
            .keys(self.instances)
            .finish()
    }
}

/// Every kind's tag, for a host that dispatches an untyped ask to `Store::read::<K>`.
pub const TAGS: [(Tag, &str); 10] = [
    (Certificate::TAG, Certificate::NAME),
    (FileImage::TAG, FileImage::NAME),
    (Surface::TAG, Surface::NAME),
    (Body::TAG, Body::NAME),
    (Instance::TAG, Instance::NAME),
    (Expansion::TAG, Expansion::NAME),
    (ScriptRun::TAG, ScriptRun::NAME),
    (TestRun::TAG, TestRun::NAME),
    (UnitManifest::TAG, UnitManifest::NAME),
    (ProgramManifest::TAG, ProgramManifest::NAME),
];
