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
    /// One file's owners: the crate roots whose module tree reaches it, from its parents' rows,
    /// never a task; a body reads it to know which crates' scopes it needs.
    Owners = 16,
    /// One build script's run, one per crate of the world that reads it (decision D115): named
    /// with the `CrateScope` parts, keyed by its crate's subject, never the machine. Its read
    /// list names the `Script` records whose `links` metadata it was given. Its out is its
    /// outputs record, which the crate's readers read.
    Script = 17,
    /// One crate's link table (run.md, "What a run takes from the request"): its `#[link]`
    /// lines, its extern blocks' names and the items that export a C name. A part its crate's
    /// `CrateScope` record lists, located by the table's digest, which is its out: a re-solve
    /// that leaves the table equal moves no reader. Valid by its key alone.
    Links = 18,
    /// One program's native image (the BNL2 record): named by the program ([`Name::native`]:
    /// its root crate, target and profile), made by the command from the request, never by a
    /// shared record. It reads the crate list and, for each crate of the root's closure, its
    /// `Links` and its `Script`; valid by that read list, so a repeat finds it by key and links
    /// nothing.
    Native = 19,
    /// What the command GIVES the machine (drive.md, RUNNER 19 ruling (2)): a file's content
    /// read by path ([`content_of`]) or a fact of the command ([`fact_of`]). Never a task or a
    /// record: its job is answered before the store, from the ask's facts, today's sweep or the
    /// bytes a task read this command. Its readers are kept by its watch row, whose `out` is
    /// what it was when the answer was written, so a later command dirties them only when it
    /// moved.
    Given = 20,
    /// One lint unit's findings (docs/v2/plan/design/lint.md): a file's root, its top-level
    /// items left out as holes, or one item of its outline, its children left out. A part of
    /// its file's `FileItems` record, keyed before the work by the unit's own bytes and holes,
    /// the scope its parent's text gives it, and the rule set's id (in its kind's tool); it reads
    /// nothing. Gated like a body: only the files an asked root owns are linted, so a
    /// dependency's items never are.
    Lint = 21,
}

/// How many kinds there are: the profile's table size. Counted, so a new kind cannot leave it
/// stale (lint `variant-count`).
pub const KINDS: usize = core::mem::variant_count::<Kind>();

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
        Kind::Links,
        Kind::Native,
        Kind::Given,
        Kind::Lint,
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
            Kind::Links | Kind::Native => "link native code",
            Kind::Given => "read what the command gives",
            Kind::Lint => "lint",
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
        // A `Signature` (D114) is a part of its file's `FileItems` record and runs in that
        // file's body job, beside its bodies. A `Lint` unit runs in its file's lint job: the
        // file read and outlined once for all its asked units.
        matches!(self, Kind::BodyCheck | Kind::Signature | Kind::Lint)
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
            Kind::Links => "Links",
            Kind::Native => "Native",
            Kind::Given => "Given",
            Kind::Lint => "Lint",
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
    /// A `Native`'s subject is its program, its only text: what it links is its read list.
    pub fn located_by_subject(self) -> bool {
        matches!(self.kind, Kind::Fetch | Kind::Owners | Kind::Native)
    }

    /// The program the command runs, as its native image's name: the root crate's key, the
    /// target and the profile (the package and binary asked, made by the command from the
    /// request). Never held by a shared record: the crate list and the unit graph are every
    /// program's, so the name, not they, says which program (rule 21).
    pub fn native(root: &ContentId, target: &[u8], profile: &[u8]) -> Name {
        let mut h = Hasher::new(NATIVE_DOMAIN);
        h.id(root).part(target).part(profile);
        Name { kind: Kind::Native, subject: h.finish() }
    }

    /// The link table of the crate whose `CrateScope` is `scope`: one subject, so the scope's
    /// record lists it and a reader names it from the crate list alone.
    pub fn links_of(scope: Name) -> Name {
        Name { kind: Kind::Links, subject: scope.subject }
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

/// A store key's table hash: its digest's first word, as is (blaze-hash `DigestKey`). Its one
/// field is the digest, so equal keys have equal words; a table of keys on
/// `blaze_hash::DigestState` hashes nothing. The safe form of `PlainKey { UNIFORM }`, which is
/// an `unsafe impl` and this crate forbids unsafe code.
impl blaze_hash::DigestKey for Key {
    #[inline(always)]
    fn digest_word(&self) -> u64 {
        blaze_hash::digest_word(&self.0.0)
    }
}

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
const NATIVE_DOMAIN: &str = "blaze-drive native image name 1";
const FILE_NAME_DOMAIN: &str = "blaze-drive file name 1";
const FILE_SOURCE_DOMAIN: &str = "blaze-drive file source 1";
const TASK_DOMAIN: &str = "blaze-drive task key 1";
const VALUE_DOMAIN: &str = "blaze-drive value key 1";
const CONTENT_DOMAIN: &str = "blaze-drive content 1";

const FACT_DOMAIN: &str = "blaze-drive fact name 1";
const FACTS_DOMAIN: &str = "blaze-drive facts 1";

/// The name a read of the content of the file at `path` (relative to the checkout,
/// `/`-separated) is: a [`Kind::Given`] with the file's subject, so the sweep's row for the path
/// answers it. What a task that reads a file by content only (a build script's
/// `rerun-if-changed`, an `include_bytes!` target the sweep never listed) records; the read's
/// digest is the bytes' content id, the sweep's form.
pub fn content_of(path: &[u8]) -> Name {
    Name { kind: Kind::Given, subject: Name::file(path).subject }
}

/// The domain of a crate's [`Kind::CrateScope`] subject (the string kept from blaze-cli
/// names.rs, where the name was made before 2026-11-17: the same names in every home).
const CRATE_SCOPE_DOMAIN: &str = "blaze-cli crate scope 9";

/// A crate's names task: a [`Kind::CrateScope`] whose subject is the digest of the crate's key.
/// The one home of that name: the names task defines it, a body task's names reads name it.
pub fn crate_scope_name(key: &blaze_id::CrateItemKey) -> Name {
    Name::new(Kind::CrateScope, ContentId::of(CRATE_SCOPE_DOMAIN, &key.0.0))
}

/// The name of a fact the command gives (an environment variable a task reads, the target, the
/// profile): a [`Kind::Given`] whose subject is `what`'s digest. Its value is the command's
/// (each with its digest, sorted by name); never an ambient input a task reads itself.
pub fn fact_of(what: &[u8]) -> Name {
    Name { kind: Kind::Given, subject: ContentId::of(FACT_DOMAIN, what) }
}

/// The digest of the ask's facts (sorted by name): part of the certificate's key, so a moved
/// fact misses the certificate and its readers are walked.
pub fn facts_digest(facts: &[(Name, ContentId)]) -> ContentId {
    let mut h = Hasher::new(FACTS_DOMAIN);
    h.update(&(facts.len() as u64).to_le_bytes());
    for (name, value) in facts {
        h.update(&[name.kind as u8]).id(&name.subject).id(value);
    }
    h.finish()
}

const CONDITION_DOMAIN: &str = "blaze-drive condition name 1";
const SPACE_DOMAIN: &str = "blaze-drive space 1";

/// The `Owners` name of the file `file` (a `FileItems` name): what a body asks, with its file as
/// the want's home, to learn which crates own it. Its subject is the file's.
pub fn owners_of(file: Name) -> Name {
    Name::new(Kind::Owners, file.subject)
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

/// The key of a task of a file whose read NAMES follow from facts no text of its own holds (a
/// `BodyCheck`, a `Signature`): it reads the `CrateScope` of every crate its file's owners'
/// checks open, and a `CrateScope`'s name is its crate's key, so which names it reads follows
/// from WHICH crates own the file (`owners`, the file's `Owners` name, `owners_out` its out:
/// root files) and WHAT those crates are (`graph`, the `UnitGraph`'s out: the crate list, whose
/// rows hold each crate's key, made from its features, cfg and side, and each crate's edges).
/// Both
/// are folded into `task` as reads, so one key never names two read lists (rule 21). Without the
/// graph, `blaze run` then `blaze check` of one hello world (two asks, two crate lists, one
/// `Owners` out) put two shapes of one `Signature` under one key ("held CrateScope(1e39..), put
/// CrateScope(1e08..)", 2026-11-17). Every such task reads the `UnitGraph` already, so folding
/// it moves no key a moved read would not have missed. `graph` is [`ABSENT`] in a command with
/// no unit graph. The one derivation: every caller that keys such a task calls it.
pub fn owned_key(task: Key, graph: ContentId, owners: Name, owners_out: ContentId) -> Key {
    // Sorted by name, as a read list is: `UnitGraph` (2) before `Owners` (16).
    value_key(task, &[(Name::unit_graph(), graph), (owners, owners_out)])
}

/// [`value_key`] of a record that also read cells (cells.rs): the whole reads, then each cell
/// read in (name, cell) order with what it saw. With no cell read it is [`value_key`] itself.
pub fn value_key_with_cells(key: Key, reads: &[(Name, ContentId)], cells: &[crate::cells::CellRead]) -> Key {
    if cells.is_empty() {
        return value_key(key, reads);
    }
    let mut h = Hasher::new(VALUE_DOMAIN);
    h.id(&key.0);
    for (name, saw) in reads {
        h.update(&[name.kind as u8]).id(&name.subject).id(saw);
    }
    // A tag no kind byte is, so a cell read never hashes as a whole read.
    h.update(&[0xCE]);
    for read in cells {
        h.update(&[read.name.kind as u8]).id(&read.name.subject).update(&read.cell.0).id(&read.saw);
    }
    Key(h.finish())
}

/// The digest a read of a name with no record saw: an absence is a read too.
pub const ABSENT: ContentId = ContentId([0; 32]);

/// The key of bytes stored by their content (answer pages, roots).
pub fn content_key(bytes: &[u8]) -> Key {
    Key(ContentId::of(CONTENT_DOMAIN, bytes))
}

/// The digest of bytes in a domain the caller names: what an executor uses for out digests.
pub fn digest(domain: &'static str, bytes: &[u8]) -> ContentId {
    ContentId::of(domain, bytes)
}
