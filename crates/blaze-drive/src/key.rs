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

/// The kinds of task, in the order the queue runs them: a file's outline before the scopes it
/// declares, scopes before signatures, signatures before bodies, bodies before code, code
/// before tests. Discriminants are the wire's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The workspace's crates: their names, editions and roots, from the manifests' content.
    Units = 1,
    /// A crate with no text here: its record lists its records by key (a pack's manifest).
    Fetch = 2,
    FileItems = 3,
    ModuleScope = 4,
    /// A cycle of module scopes solved as one task; each member's record is its own.
    Group = 5,
    Expansion = 6,
    Signature = 7,
    ImplBucket = 8,
    BodyCheck = 9,
    Code = 10,
    /// A test's result, whose read list is the code identities its run entered.
    Test = 11,
}

/// How many kinds there are: the profile's table size.
pub const KINDS: usize = 11;

impl Kind {
    pub const ALL: [Kind; KINDS] = [
        Kind::Units,
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
    ];

    pub fn from_u8(byte: u8) -> Option<Kind> {
        Kind::ALL.get(usize::from(byte).checked_sub(1)?).copied()
    }

    /// Its index in [`Kind::ALL`].
    pub fn index(self) -> usize {
        self as usize - 1
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Units => "Units",
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

    /// The name of the workspace's units: there is one.
    pub fn units() -> Name {
        Name { kind: Kind::Units, subject: ContentId::of(UNITS_DOMAIN, b"units") }
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
