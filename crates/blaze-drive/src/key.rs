//! Names, sources and keys (drive.md, "Names, sources and keys").
//!
//! - A [`Name`] is what a task is about: its kind and its subject (a file's path hash, an item's
//!   key, a fixed subject for the workspace's units). It is stable across edits.
//! - A source is the text the task is keyed by, known before the work: a file's content and
//!   edition, or an item's part digest in its file's record.
//! - A [`Key`] is H(tool, kind, subject, source): what the record is. It never holds another
//!   record's key; what a task read of another record is its read list (bottom.md 3).

use blaze_id::ContentId;
use blaze_ids::Hasher;

use crate::condition::{Condition, Space};

/// The kinds of task, in the order the queue runs them: a file's outline before the scopes it
/// declares, scopes before signatures, signatures before bodies, bodies before code, code
/// before tests. Discriminants are the wire's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The workspace's crates: their names, editions and roots, from the manifests' content.
    Units = 1,
    /// The dependency graph the request reaches (unit_graph_task.rs): one a command, walked
    /// level by level from the roots, each level's sources asked of `Fetch` as it is reached.
    UnitGraph = 2,
    /// A crate with no text here: its record lists its records by key (a pack's manifest).
    Fetch = 3,
    FileItems = 4,
    ModuleScope = 5,
    /// A cycle of module scopes solved as one task; each member's record is its own.
    Group = 6,
    Expansion = 7,
    Signature = 8,
    ImplBucket = 9,
    BodyCheck = 10,
    Code = 11,
    /// A test's result, whose read list is the code identities its run entered.
    Test = 12,
    /// One crate's scope (drive.md, "SPEC, the NAMES step"): every public path to its item ids,
    /// as blaze-world's names library makes it. Its source is the crate's identity
    /// (blaze-world's `UnitInput::key`); it reads its files' `Surface`s, its dependencies'
    /// `CrateScope`s and the `MacroModule`s of the proc-macro crates it calls.
    CrateScope = 13,
    /// One proc-macro crate compiled to wasm (rule 10), keyed by its whole text and the
    /// toolchain; never compiled inside a `CrateScope` task.
    MacroModule = 14,
    /// One file's surface: its items' signatures, `pub` and `use` lines and macro calls, never a
    /// body. Its source is that digest and its out is its source, so a body edit leaves it, and
    /// every `CrateScope` reading it, standing.
    Surface = 15,
    /// One file's owners (owners.rs): the crate roots whose module tree reaches it. Computed by
    /// the request machine from its parents' rows, never a task; a body reads it to know which
    /// crates' scopes it needs.
    Owners = 16,
    /// One build script's run, one per crate of the world that reads it (decision D115): named
    /// with the `CrateScope` parts, keyed by its crate's subject, never the machine. Its read
    /// list names the `Script` records whose `links` metadata it was given. Its out is its
    /// outputs record, which the crate's readers read.
    Script = 17,
}

/// How many kinds there are: the profile's table size.
pub const KINDS: usize = 17;

impl Kind {
    pub const ALL: [Kind; KINDS] = [
        Kind::Units,
        Kind::UnitGraph,
        Kind::Fetch,
        Kind::FileItems,
        Kind::ModuleScope,
        Kind::Group,
        Kind::Expansion,
        Kind::Signature,
        Kind::ImplBucket,
        Kind::BodyCheck,
        Kind::Code,
        Kind::Test,
        Kind::CrateScope,
        Kind::MacroModule,
        Kind::Surface,
        Kind::Owners,
        Kind::Script,
    ];

    /// The stage of the compiler this kind is, as a person reads it in a note: kinds of one
    /// stage share the words.
    pub fn stage(self) -> &'static str {
        match self {
            Kind::Units | Kind::UnitGraph => "read the manifests",
            // Its sources in the store, checked against the lock; not their code read.
            Kind::Fetch => "fetch the dependencies' sources",
            Kind::FileItems | Kind::Surface | Kind::Owners => "parse",
            Kind::ModuleScope | Kind::Group | Kind::CrateScope => "resolve names",
            Kind::MacroModule => "compile procedural macros",
            Kind::Expansion => "expand macros",
            Kind::Signature | Kind::ImplBucket | Kind::BodyCheck => "check types",
            Kind::Code => "generate code",
            Kind::Test => "run tests",
            Kind::Script => "run build scripts",
        }
    }

    pub fn from_u8(byte: u8) -> Option<Kind> {
        Kind::ALL.get(usize::from(byte).checked_sub(1)?).copied()
    }

    /// Whether a batch of this kind is one home's ready tasks, whole, rather than sized by time:
    /// a runner sets up once per home and batch (a body check builds its file's front-end
    /// reader once for all the file's bodies), so a batch cut by the per-task mean (which
    /// includes that setup) would pay the setup once a task.
    pub const fn batches_by_home(self) -> bool {
        matches!(self, Kind::BodyCheck)
    }

    /// Its index in [`Kind::ALL`].
    pub fn index(self) -> usize {
        self as usize - 1
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Units => "Units",
            Kind::UnitGraph => "UnitGraph",
            Kind::Fetch => "Fetch",
            Kind::FileItems => "FileItems",
            Kind::ModuleScope => "ModuleScope",
            Kind::Group => "Group",
            Kind::Expansion => "Expansion",
            Kind::Signature => "Signature",
            Kind::ImplBucket => "ImplBucket",
            Kind::BodyCheck => "BodyCheck",
            Kind::Code => "Code",
            Kind::Test => "Test",
            Kind::CrateScope => "CrateScope",
            Kind::MacroModule => "MacroModule",
            Kind::Surface => "Surface",
            Kind::Owners => "Owners",
            Kind::Script => "Script",
        }
    }
}

/// What a task is about. Ordered by kind first, so a sorted queue of names is the run order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Name {
    pub kind: Kind,
    pub subject: ContentId,
}

impl Name {
    pub const fn new(kind: Kind, subject: ContentId) -> Name {
        Name { kind, subject }
    }

    /// The workspace root's units: which manifests are its units (drive.md, "Units"). There is
    /// one; its source is the root manifest and the checkout's manifest paths.
    pub fn units() -> Name {
        Name { kind: Kind::Units, subject: ContentId::of(UNITS_DOMAIN, b"units") }
    }

    /// The request's dependency graph: one a command, so its subject is a constant; its source
    /// is `unit_graph_task::source` (the lock, the layout's reads, the target, the ask).
    pub fn unit_graph() -> Name {
        Name { kind: Kind::UnitGraph, subject: ContentId::of(UNITS_DOMAIN, b"unit graph") }
    }

    /// Whether the name's subject is its source too, known from the name alone: a `Fetch`
    /// name's subject is its unit's record key (fetch_task.rs). A need of one is located by it.
    pub fn located_by_subject(self) -> bool {
        matches!(self.kind, Kind::Fetch | Kind::Owners)
    }

    /// The units of the manifest at `path` (its package): a `Units` name whose subject is the
    /// manifest file's, so its source is that file's content and its findings land on its lines.
    pub fn manifest(path: &[u8]) -> Name {
        Name { kind: Kind::Units, subject: Name::file(path).subject }
    }

    /// The file a name's text is: a `FileItems` name's own file, a manifest's units' manifest,
    /// the root units' `Cargo.toml`; `None` for an item (its home's file is its file).
    pub fn text_file(self) -> Option<Name> {
        match self.kind {
            Kind::FileItems => Some(self),
            Kind::Units if self == Name::units() => Some(Name::file(b"Cargo.toml")),
            Kind::Units => Some(Name { kind: Kind::FileItems, subject: self.subject }),
            _ => None,
        }
    }

    /// The `FileItems` name of the file at `path`, relative to the checkout.
    pub fn file(path: &[u8]) -> Name {
        Name { kind: Kind::FileItems, subject: ContentId::of(FILE_NAME_DOMAIN, path) }
    }
}

impl core::fmt::Display for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = &self.subject.0;
        write!(f, "{}({:02x}{:02x}{:02x}{:02x}{:02x}{:02x})", self.kind.name(), b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

/// Where a record is in the store: a task key, a value key, a page, a certificate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Key(pub ContentId);

impl core::fmt::Display for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in &self.0.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The tool every key holds: blaze's build and its pinned nightly, as one digest the caller
/// computes (the binary's own content id). Two tools never share a record.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Tool(pub ContentId);

const UNITS_DOMAIN: &str = "blaze-drive units name 1";
const FILE_NAME_DOMAIN: &str = "blaze-drive file name 1";
const FILE_SOURCE_DOMAIN: &str = "blaze-drive file source 1";
const TASK_DOMAIN: &str = "blaze-drive task key 1";
const VALUE_DOMAIN: &str = "blaze-drive value key 1";
const CERT_DOMAIN: &str = "blaze-drive certificate 1";
const CONTENT_DOMAIN: &str = "blaze-drive content 1";

const CONDITION_DOMAIN: &str = "blaze-drive condition name 1";
const SPACE_DOMAIN: &str = "blaze-drive space 1";

/// The `Owners` name of the file `file` (a `FileItems` name): what a body asks, with its file as
/// the want's home, to learn which crates own it (owners.rs).
pub fn owners_of(file: Name) -> Name {
    crate::owners::of(file)
}

/// The job of `base` under `condition` (drive.md, "One check covers every configuration"): a
/// name of its own, so its record, row and readers are an ordinary job's. Under
/// [`Condition::ANY`] it is `base` itself: a request with no cfg reads costs nothing.
pub fn at_condition(base: Name, condition: &Condition) -> Name {
    if condition.atoms().is_empty() {
        return base;
    }
    let mut h = Hasher::new(CONDITION_DOMAIN);
    h.update(&[base.kind as u8]).id(&base.subject);
    for (var, value) in condition.atoms() {
        h.update(&var.to_le_bytes()).update(&value.to_le_bytes());
    }
    Name { kind: base.kind, subject: h.finish() }
}

/// The name a split record reads the request's space under: a split's forks are the values
/// the space allows, so it is read-addressed by the space's digest ([`space_digest`]).
pub fn space_name() -> Name {
    Name { kind: Kind::Units, subject: ContentId::of(SPACE_DOMAIN, b"space") }
}

/// What a read of [`space_name`] sees: every variable's value count and every implication.
pub fn space_digest(space: &Space<'_>) -> ContentId {
    let mut h = Hasher::new(SPACE_DOMAIN);
    h.update(&(space.values.len() as u64).to_le_bytes());
    for count in space.values {
        h.update(&count.to_le_bytes());
    }
    for (a, b) in space.implications {
        h.update(&a.to_le_bytes()).update(&b.to_le_bytes());
    }
    h.finish()
}

/// A `FileItems` source: the file's bytes (their content id) and the edition it is read in.
pub fn file_source(content: ContentId, edition: u8) -> ContentId {
    let mut h = Hasher::new(FILE_SOURCE_DOMAIN);
    h.id(&content).update(&[edition]);
    h.finish()
}

/// A task's key: what its record is, before the work.
pub fn task_key(tool: Tool, name: Name, source: &ContentId) -> Key {
    let mut h = Hasher::new(TASK_DOMAIN);
    h.id(&tool.0).update(&[name.kind as u8]).id(&name.subject).id(source);
    Key(h.finish())
}

/// A read-addressed record's value key: its task key and the digest each read saw, in the read
/// list's order (sorted by name). An absent name's digest is [`ABSENT`].
pub fn value_key(key: Key, reads: &[(Name, ContentId)]) -> Key {
    let mut h = Hasher::new(VALUE_DOMAIN);
    h.id(&key.0);
    for (name, saw) in reads {
        h.update(&[name.kind as u8]).id(&name.subject).id(saw);
    }
    Key(h.finish())
}

/// The digest a read of a name with no record saw: an absence is a read too.
pub const ABSENT: ContentId = ContentId([0; 32]);

/// The certificate key of a request: the tool, the command and its arguments, and every file of
/// the checkout by path and content (sorted by path).
pub fn cert_key<'a>(tool: Tool, command: &[u8], files: impl Iterator<Item = (&'a [u8], &'a ContentId)>) -> Key {
    let mut h = Hasher::new(CERT_DOMAIN);
    h.id(&tool.0).part(command);
    for (path, id) in files {
        h.part(path).id(id);
    }
    Key(h.finish())
}

/// The key of bytes stored by their content (answer pages, roots).
pub fn content_key(bytes: &[u8]) -> Key {
    Key(ContentId::of(CONTENT_DOMAIN, bytes))
}

/// The digest of bytes in a domain the caller names: what an executor uses for out digests.
pub fn digest(domain: &'static str, bytes: &[u8]) -> ContentId {
    ContentId::of(domain, bytes)
}
