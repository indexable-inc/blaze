//! The `CrateScope` task: one crate's names resolved from its dependencies' records
//! (docs/v2/plan/design/drive.md, the NAMES step, "HANDED ON 07:xx").
//!
//! The world (blaze-world: every crate's `CrateInput`, its key and where its files are) is
//! made once a command from the `UnitGraph` record, the lock, the pinned toolchain on the
//! shelf under the home and the fetched sources' records. The `UnitGraph` job's record names
//! one `CrateScope` part per crate of the world ([`parts`]), its subject the crate's key: a
//! dependent names a dependency's record from the key alone.
//!
//! A task asks for its direct dependencies' and the sysroot crates' records, then runs
//! blaze-scope's `resolve_crate`; a `Need` from it asks for those records too and enters again.
//! The record is a head, then the exports record, the inner one, the names record and the
//! items record ([`sections`]); its out is the digest of the EXPORTS only, so a private change does not move a dependent. The task's key is the crate's key (an
//! identity, no text): every checkout file the resolution read is listed in its reads as that
//! file's `Surface`, so a body edit (every `Surface` out unchanged) runs no `CrateScope` task,
//! and a signature edit runs that crate's. A fetched or toolchain file is under the key.
//!
//! Interim, said in the closing note: build scripts' outputs are not given (each crate with a
//! script is resolved without them); proc macros are not run (their calls are counted as
//! unresolved by a known gap); every record lives in the project's store, the toolchain's and
//! fetched crates' too (they belong in the shared store); a path package must be a workspace
//! member (each is read under its own directory; one outside the workspace root is refused);
//! the world and the records are held in the global allocator for the command.

use core::cell::RefCell;
use std::alloc::Global;
use std::string::String;

use blaze_drive::{Input, Kind, Name, Task, TaskFailure, Want, Why};
use blaze_fetch::SourceId;
use blaze_fetch::pack::PackView;
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::Alloc;
use blaze_scope::crate_scope::{CrateScopeError, resolve_crate};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts};
use blaze_scope::prepares::Prepares;
use blaze_scope::task::Serial;
use blaze_unit_graph::{Records, StoreSources};
use blaze_world::{Fetched, Toolchain, Tree, World};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadOnly, ReadWrite};
use bstd::path::Path;
use bstd::mem::{Bytes, Frame};

use crate::CliError;
use crate::fetch::FetchEnv;
use crate::store::Store;
use crate::work::{Did, Made, held_str};

pub(crate) mod sections;

// The subject and source domains name the payload's form ([`sections::FORM`] 3): an old home's
// records (form 1: exports then inner; form 2: no tests section) are under other names, never
// found, made again.
const SUBJECT_DOMAIN: &str = "blaze-cli crate scope 3";
const SOURCE_DOMAIN: &str = "blaze-cli crate scope source 3";
const OUT_DOMAIN: &str = "blaze-cli crate scope out 1";
/// The rule a refusal of this task names.
const RULE: &str = "names.rs";

/// A crate's `CrateScope` name: its subject is its key.
pub(crate) fn name_of(key: CrateItemKey) -> Name {
    Name::new(Kind::CrateScope, ContentId::of(SUBJECT_DOMAIN, &key.0.0))
}

/// Why the names world of a command could not be made.
#[derive(Debug)]
pub enum NamesError {
    /// The unit graph record does not decode.
    Graph(blaze_unit_graph::record::RecordRefused),
    /// No lock was read this command.
    NoLock,
    /// `HOME` or `PWD` is not set: the toolchain's shelf is under the home, reached from the start.
    NoHome,
    /// The frame that holds the names step's files could not be reserved.
    Frame,
    /// The toolchain's shelf could not be reached.
    Shelf(crate::toolchain::ShelfError),
    /// The pinned toolchain could not be opened (its words).
    Toolchain(String),
    /// The fetched sources could not be opened (their words).
    Sources(String),
    /// The world could not be laid out (its words).
    World(String),
    /// The workspace's members were not matched to the lock this command (its finding is on
    /// the root's units): no member's directory is known.
    NoMembers,
    /// Two members whose directory is one: neither's files can be told apart.
    SharedDirectory { first: String, second: String, dir: String },
    /// A path package the graph reaches that is no member of the workspace (a path dependency
    /// outside the workspace root): the names step reads members' directories only. `dependent`
    /// is the first unit that depends on it, `None` when none does.
    PathNotMember { package: String, dependent: Option<String> },
}

impl core::fmt::Display for NamesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NamesError::Graph(refused) => write!(f, "the unit graph record at byte {}: {}", refused.at, refused.why),
            NamesError::NoLock => f.write_str("no lock was read this command"),
            NamesError::NoHome => f.write_str("HOME or PWD is not set: the toolchain's shelf is under the home, reached from the start folder"),
            NamesError::Frame => write!(f, "the frame for the names step's files ({NAMES_FRAME} bytes reserved) could not be reserved"),
            NamesError::Shelf(error) => write!(f, "{error}"),
            NamesError::Toolchain(words) => write!(f, "the pinned toolchain: {words}"),
            NamesError::Sources(words) => write!(f, "the fetched sources: {words}"),
            NamesError::World(words) => write!(f, "the crates' inputs: {words}"),
            NamesError::NoMembers => f.write_str("the workspace's members were not matched to the lock this command: no member's directory is known"),
            NamesError::SharedDirectory { first, second, dir } => {
                write!(f, "the members `{first}` and `{second}` are both in the directory `{dir}`: one directory holds one package")
            }
            NamesError::PathNotMember { package, dependent: Some(dependent) } => write!(
                f,
                "the path package `{package}`, a dependency of `{dependent}`, is no member of the workspace (a path outside the workspace root): its directory is not known to the names step"
            ),
            NamesError::PathNotMember { package, dependent: None } => {
                write!(f, "the path package `{package}` is no member of the workspace (a path outside the workspace root): its directory is not known to the names step")
            }
        }
    }
}

/// The reserve of the frame that holds what the names step read for the command: the
/// toolchain's records, the fetched sources' records and the checkout's files.
const NAMES_FRAME: usize = 4 << 30;

/// What the names step reads, through the command's own files (rule 75: the IO is the one
/// `main` was given, so a simulated command reads its simulated files): each file read whole
/// into the names frame and held to the end of the command.
struct Reader {
    cx: &'static Cx,
    /// The checkout, read-only.
    root: Dir<ReadOnly>,
    frame: &'static Frame,
}

impl Reader {
    /// The file at `path` under the checkout, held to the end of the command; `None` when
    /// there is none (or it could not be read: the reader that asked says what it lacked).
    fn read(&self, path: &str) -> Option<&'static [u8]> {
        if path.split('/').any(|part| part == ".." || part == ".") {
            return None;
        }
        let at = Path::new(path).ok()?;
        let mut bytes = Bytes::new_in(self.frame, "the names step's files");
        self.root.read_all(self.cx, &at, &mut bytes).ok()?;
        // Held to the end of the command, in the names frame.
        let held: &'static Bytes<'static> = std::boxed::Box::leak(std::boxed::Box::new(bytes));
        Some(held.as_slice())
    }
}

/// The checkout's files, as the names library asks for them.
struct ReadTree(&'static Reader);

impl Tree for ReadTree {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.0.read(path)
    }
}

/// The fetched sources' records: `<hex of the key>` in the machine's sources store
/// (`<home>/.blaze/v2/sources`, fetch.rs `SOURCES`), never the checkout, for any key the graph
/// pins: a reader over that store, with the names step's context and frame.
struct Folder(Reader);

impl Folder {
    /// The sources store opened read only beside `checkout`'s reader (its context and frame).
    fn of(checkout: &'static Reader, env: &FetchEnv<'_>) -> Result<Folder, NamesError> {
        let sources = crate::fetch::sources_dir(checkout.cx, env).map_err(|error| NamesError::Sources(error.to_string()))?;
        Ok(Folder(Reader { cx: checkout.cx, root: sources.read_only(), frame: checkout.frame }))
    }
}

impl<'b> Records<'b> for Folder {
    fn record(&mut self, source: &SourceId<'_>) -> Option<&'b [u8]> {
        let hex = crate::fetch::key_hex(&source.key());
        self.0.read(core::str::from_utf8(&hex).ok()?)
    }
}

/// One crate made this command: its payload, out, and the checkout files its resolution read.
struct Resolved {
    /// The record's payload ([`sections::write`]).
    payload: blaze_own::Vec<u8, Global>,
    out: ContentId,
    /// Each checkout file read, checkout-relative, sorted and without repeats.
    files: std::vec::Vec<String, Global>,
    /// Each checkout file looked for and not found (a `mod x;` whose file is missing, a
    /// module's other place): read as its `Surface`, absent, so creating it reruns the crate.
    absent: std::vec::Vec<String, Global>,
    /// Each unresolved path of the crate's own code, as its record's finding ([`unresolved`]).
    findings: blaze_own::Vec<Unresolved, Global>,
}

/// The rule of a `use` or macro path in a project's own crate that names nothing.
pub(crate) const UNRESOLVED: u16 = 14;

/// One unresolved path of a project crate, placed item-relative (rule 71): the range of the
/// failing segment from its item's start, and the message: the file, the item's chain digest
/// ([`crate::work::item_chain`]; 16 hex digits, or empty for a path with no place), the
/// file's edition byte, then the words' two pieces, all split by 0x1F. A body edit moves no
/// byte of it; the printer finds the item's start in the file as it is ([`placed`]).
struct Unresolved {
    at: (u32, u32),
    message: blaze_own::Vec<u8, Global>,
}

/// The pieces before the words in an [`UNRESOLVED`] finding's message.
const PLACE_PIECES: usize = 3;

/// The byte offset of `word` in `text` at or after `from`, as a whole identifier.
fn find_word(text: &str, word: &str, from: usize) -> Option<usize> {
    let ident = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let bytes = text.as_bytes();
    let mut at = from;
    while let Some(found) = text.get(at..).and_then(|rest| rest.find(word)) {
        let start = at + found;
        let end = start + word.len();
        let before = start.checked_sub(1).is_some_and(|b| ident(bytes[b]));
        let after = bytes.get(end).is_some_and(|b| ident(*b));
        if !before && !after && !word.is_empty() {
            return Some(start);
        }
        at = start + word.len().max(1);
    }
    None
}

/// Every `use` and macro path of crate `c` that resolved to nothing. The project's own (a
/// checkout file, `/0/`) are findings in `own`, sorted; the number of the others (a fetched or
/// toolchain crate's: not the project's to fix) is returned, counted in one note.
fn unresolved(c: &blaze_scope::resolve::CrateNames<'_>, own: &mut blaze_own::Vec<Unresolved, Global>) -> u32 {
    use blaze_surface::{PathKind, ScopeError};
    let project = c.files.iter().any(|file| file.path.starts_with("/0/"));
    let mut elsewhere = 0u32;
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            let (loc, path, segment) = match error {
                ScopeError::Import { site, path, segment, .. } => {
                    let fragment = site.expansion.unwrap_or(r.scope.module);
                    (c.tables.use_locs.get(&fragment).and_then(|locs| locs.get(site.index as usize).copied()), path, *segment as usize)
                }
                ScopeError::Macro { call, path, segment, .. } => (c.tables.calls.get(call).map(|src| src.loc), path, *segment as usize),
                _ => continue,
            };
            if !project {
                elsewhere += 1;
                continue;
            }
            let mut message = blaze_own::Vec::new_in(Global);
            let mut at = (0u32, 0u32);
            let file = loc.and_then(|loc| c.files.get(loc.file as usize).map(|file| (loc, *file)));
            match file.and_then(|(loc, file)| Some((loc, file, crate::work::item_holding(&file.outline, loc.offset)?))) {
                Some((_, file, item)) => {
                    let text = file.text;
                    let entry = item.range.of(text);
                    // The failing segment: the path's segments found in order in the item's text.
                    let mut from = 0usize;
                    let mut found = None;
                    for seg in path.segments.iter().take(segment + 1) {
                        found = find_word(entry, seg, from).map(|start| (start, start + seg.len()));
                        from = found.map_or(from, |(_, end)| end);
                    }
                    at = found.map_or((0, 0), |(s, e)| (s as u32, e as u32));
                    message.extend_from_slice(file.path.trim_start_matches("/0/").as_bytes());
                    message.push(0x1F);
                    let chain = crate::work::item_chain(&file.outline, item, text);
                    for shift in (0..16).rev() {
                        message.push(b"0123456789abcdef"[((chain >> (shift * 4)) & 0xF) as usize]);
                    }
                }
                None => message.push(0x1F),
            }
            message.push(0x1F);
            message.push(b'0' + c.edition.byte());
            message.push(0x1F);
            message.push(b'`');
            message.extend_from_slice(path.segments.get(segment).copied().unwrap_or("").as_bytes());
            message.extend_from_slice(b"`\x1F");
            let before = &path.segments[..segment.min(path.segments.len())];
            let head: &[u8] = match path.kind {
                PathKind::Plain | PathKind::DollarCrate(_) => b"",
                PathKind::Crate => b"crate",
                PathKind::SelfMod => b"self",
                PathKind::Super(_) => b"super",
                PathKind::Abs => b"::",
            };
            match (head.is_empty(), before.is_empty()) {
                (true, true) => message.extend_from_slice(b"this scope"),
                _ => {
                    message.push(b'`');
                    message.extend_from_slice(head);
                    for (i, seg) in before.iter().enumerate() {
                        if i > 0 || (!head.is_empty() && head != b"::") {
                            message.extend_from_slice(b"::");
                        }
                        message.extend_from_slice(seg.as_bytes());
                    }
                    message.push(b'`');
                }
            }
            own.push(Unresolved { at, message });
        }
    }
    // The scopes are a map: its order reaches the record only sorted (rule 25).
    own.sort_unstable_by(|a, b| (&a.message[..], a.at).cmp(&(&b.message[..], b.at)));
    elsewhere
}

/// Where an [`UNRESOLVED`] finding is to be drawn, read from its message alone (no file is
/// read here): its file, its item's chain and the edition the chain was made in, and the
/// words. `None` for one with no file: drawn at its crate's name. The item's start in the file
/// as it is today is found where the file is read, once for all its findings (print.rs
/// `list`: one whose item is no longer there is drawn at its crate's name too).
pub(crate) fn place_pieces(message: &[u8]) -> Result<(Option<(&str, u64, blaze_surface::Edition)>, &[u8]), CliError> {
    let mut pieces = message.splitn(PLACE_PIECES + 1, |b| *b == 0x1F);
    let (Some(path), Some(chain), Some(edition), Some(words)) = (pieces.next(), pieces.next(), pieces.next(), pieces.next()) else {
        return Err(CliError::Text { what: "an unresolved path's finding holds no place pieces" });
    };
    if path.is_empty() || chain.len() != 16 {
        return Ok((None, words));
    }
    let path = core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
    let chain = core::str::from_utf8(chain).ok().and_then(|hex| u64::from_str_radix(hex, 16).ok()).ok_or(CliError::Text { what: "an unresolved path's item digest is not hex" })?;
    let edition = edition.first().and_then(|b| b.checked_sub(b'0')).and_then(blaze_surface::Edition::from_byte).ok_or(CliError::Text { what: "an unresolved path's finding holds no edition" })?;
    Ok((Some((path, chain, edition)), words))
}

/// The names step's state for a command: the world, each crate by subject, and what the
/// tasks made (a task that asks for its files' `Surface`s answers from here when entered again).
pub struct Names<'f> {
    world: World<'f>,
    /// (subject, crate index), sorted.
    by_subject: std::vec::Vec<(ContentId, usize), Global>,
    /// (key, crate index), sorted.
    by_key: std::vec::Vec<(CrateItemKey, usize), Global>,
    made: std::vec::Vec<(ContentId, Resolved), Global>,
    /// The subjects of the crates whose records may hold nothing of a project (target.md,
    /// "Namespaces"): the toolchain's, and a registry archive's known by its lock checksum.
    /// Whether a record is public also needs every name it reads public: the store's check.
    public: std::vec::Vec<ContentId, Global>,
    /// The command's one set of prepared macro definitions, shared by every crate it resolves.
    prepares: &'f Prepares<'f>,
    /// Crates whose names were resolved this command, and the unresolved paths among them.
    pub resolved: u32,
    pub unresolved: u64,
}

/// A checkout file a package's names read that no units record and no file's outline lists
/// (the machine's `DriveError::NoPart`, which names the file by its digest alone).
#[derive(Debug)]
pub struct Unlisted {
    pub package: String,
    /// The file's path (its slot's: `x.rs` for `x/mod.rs` too), checkout-relative.
    pub path: String,
}

impl core::fmt::Display for Unlisted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the names of package `{}` read `{}`, which no manifest's targets and no file's `mod` lines list (a module under a macro, an attribute or a `#[path]` the outline does not follow): its items cannot be checked",
            self.package, self.path
        )
    }
}

impl Names<'_> {
    /// The package whose names read the checkout file `file` names, and the file's path: the
    /// first in subject order. For an error that would name the file by its digest.
    pub(crate) fn reader_of(&self, file: Name) -> Option<Unlisted> {
        for (subject, resolved) in self.made.iter() {
            let Some(path) = resolved.files.iter().find(|path| Name::file(path.as_bytes()) == file) else { continue };
            let at = self.by_subject.binary_search_by_key(subject, |(subject, _)| *subject).ok()?;
            return Some(Unlisted { package: String::from(self.world.crates[self.by_subject[at].1].package), path: path.clone() });
        }
        None
    }
}

/// Each workspace member's lock entry and directory, anchor-relative, from the layout the
/// command read: the graph job's when it was made, else the members matched to the lock;
/// `None` when the members were not matched to a lock this command.
pub(crate) fn member_dirs<'l>(laid: Option<&'l crate::units::Laid<'_>>) -> Option<&'l [(LockId, blaze_unit_graph::Text<'static>)]> {
    let laid = laid?;
    match (&laid.graph, &laid.parts) {
        (Some(graph), _) => Some(graph.member_dirs()),
        (None, Some(parts)) => Some(parts.member_dirs()),
        (None, None) => None,
    }
}

/// The names world of a command, made from the graph record `graph_record`; `members` is each
/// workspace member's lock entry and directory, anchor-relative ([`member_dirs`]).
pub(crate) fn world<'f>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    graph_record: &[u8],
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
    env: &FetchEnv<'_>,
) -> Result<Names<'f>, NamesError> {
    let lock: &'f Lock<'f> = lock.ok_or(NamesError::NoLock)?;
    let members = members.ok_or(NamesError::NoMembers)?;
    // The command's allocator for the world and the records: the global one, held to the end.
    let alloc: Alloc<'f> = Alloc::global();
    let Some(home) = env.home else { return Err(NamesError::NoHome) };
    let decoded = blaze_unit_graph::record::decode(graph_record, Alloc::global()).map_err(NamesError::Graph)?;
    let decoded: &'static blaze_unit_graph::record::Decoded<'static> = std::boxed::Box::leak(std::boxed::Box::new(decoded));
    let graph = &decoded.graph;
    let needs = blaze_world::needs(graph);
    // The files read for the names step live to the end of the command: a context of the
    // command's (its deadline) and a frame of their own, both held to the end.
    let cx: &'static Cx = std::boxed::Box::leak(std::boxed::Box::new(cx.until(cx.deadline())));
    let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::with_reserve(NAMES_FRAME, bstd::mem::Tag::Other).map_err(|_| NamesError::Frame)?));
    let mut shelf = crate::toolchain::HomeShelf::open(cx, &env.fs, home, frame).map_err(NamesError::Shelf)?;
    let toolchain = Toolchain::open(&mut shelf, env.toolchain, crate::graph::TRIPLE, needs, Alloc::global())
        .map_err(|e| NamesError::Toolchain(std::format!("{e}")))?;
    let toolchain: &'f Toolchain<'f, crate::toolchain::Held<'static>> = std::boxed::Box::leak(std::boxed::Box::new(toolchain));
    let reader: &'static Reader = std::boxed::Box::leak(std::boxed::Box::new(Reader { cx, root: root.read_only(), frame }));
    let sources = StoreSources::new(lock, Folder::of(reader, env)?, PackView::open_source_put, Alloc::global())
        .map_err(|e| NamesError::Sources(std::format!("{e}")))?;
    let sources = std::boxed::Box::leak(std::boxed::Box::new(sources));
    // Every path package the graph reaches is a member, placed at its own directory (the
    // layout's, anchor-relative: `""` for the root's own package), so its root file and its
    // `mod x;` files are read under it. A lock may name path packages no unit reaches: they are
    // never placed. Two members in one directory, or a reached path package that is no member
    // (a path dependency outside the workspace root), is refused by name: nothing is guessed.
    let mut by_dir: blaze_own::Vec<(&str, &str), &Frame> = blaze_own::Vec::with_capacity_in(members.len(), frame);
    for (id, dir) in members.iter() {
        by_dir.push((dir.as_str(), lock.package(*id).name.as_str()));
    }
    by_dir.sort_unstable();
    if let Some(pair) = by_dir.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        return Err(NamesError::SharedDirectory { first: String::from(pair[0].1), second: String::from(pair[1].1), dir: String::from(pair[0].0) });
    }
    let reached = |name: &str| graph.units.iter().any(|unit| unit.name.as_str().bytes().map(|b| if b == b'-' { b'_' } else { b }).eq(name.bytes().map(|b| if b == b'-' { b'_' } else { b })));
    let mut paths: std::vec::Vec<(LockId, &'f str), Global> = std::vec::Vec::new();
    for at in 0..lock.packages.len() {
        let id = LockId(at as u32);
        let package = lock.package(id);
        if !matches!(package.source, LockSource::Path) || !reached(package.name.as_str()) {
            continue;
        }
        let Some((_, dir)) = members.iter().find(|(member, _)| *member == id) else {
            // The first unit that depends on a unit of this package, by the graph's order.
            let dependent = graph
                .units
                .iter()
                .find(|unit| unit.deps.iter().any(|dep| graph.units[dep.unit.0 as usize].package == id))
                .map(|unit| String::from(unit.name.as_str()));
            return Err(NamesError::PathNotMember { package: String::from(package.name.as_str()), dependent });
        };
        // Held to the end of the command, in the names frame, as the files read under it are.
        let held: &'static blaze_own::String<&'static Frame> = blaze_own::Box::leak(blaze_own::Box::new_in(blaze_own::String::from_str_in(dir.as_str(), frame), frame));
        paths.push((id, held.as_str()));
    }
    let paths: &'f [(LockId, &'f str)] = std::vec::Vec::leak(paths);
    let checkout: &'f ReadTree = std::boxed::Box::leak(std::boxed::Box::new(ReadTree(reader)));
    let mut packages = Fetched { store: sources, paths, lock, alloc: Alloc::global() };
    let world = blaze_world::world(graph, lock, toolchain, &mut packages, checkout, None, Alloc::global())
        .map_err(|e| NamesError::World(std::format!("{e}")))?;
    let mut by_subject = std::vec::Vec::new();
    let mut by_key = std::vec::Vec::new();
    for (at, krate) in world.crates.iter().enumerate() {
        by_subject.push((name_of(krate.input.key).subject, at));
        by_key.push((krate.input.key, at));
    }
    by_subject.sort_unstable();
    by_key.sort_unstable();
    // A registry package with a checksum in the lock: its archive is known by content, so its
    // crate's records are keyed by nothing of this project. A path or git package, a member,
    // or a registry package with no checksum stays the project's.
    let from_archive = |unit: blaze_unit_graph::UnitId| {
        let package = lock.package(graph.units[unit.0 as usize].package);
        matches!(package.source, LockSource::Registry { .. }) && matches!(package.checksum, blaze_manifest::lock::Checksum::Sha256(_))
    };
    let mut public = std::vec::Vec::new_in(Global);
    public.extend(
        world
            .crates
            .iter()
            .filter(|krate| match krate.origin {
                blaze_world::Origin::Toolchain(_) => true,
                blaze_world::Origin::Unit(unit) => from_archive(unit),
            })
            .map(|krate| name_of(krate.input.key).subject),
    );
    Ok(Names { world, by_subject, by_key, made: std::vec::Vec::new(), public, prepares: Prepares::new_in(alloc), resolved: 0, unresolved: 0 })
}

/// Tells `store` the world's public crates (the toolchain's and checksummed registry
/// archives'), whose records hold nothing of a project when every name they read is public
/// too: they go to the machine store (remote-read.md, "One chain of stores"). Called wherever
/// a world is made.
pub(crate) fn tell_public(store: &mut dyn Store, names: &Names<'_>) {
    store.public_crates(&names.public);
}

/// The `CrateScope` parts the graph's record names: one per crate of the world, by subject,
/// its source the same key's digest.
pub(crate) fn parts(names: &Names<'_>) -> std::vec::Vec<(Name, ContentId), Global> {
    let mut parts = std::vec::Vec::with_capacity(names.world.crates.len());
    for krate in names.world.crates.iter() {
        parts.push((name_of(krate.input.key), ContentId::of(SOURCE_DOMAIN, &krate.input.key.0.0)));
    }
    parts.sort_unstable_by_key(|(name, _)| name.subject);
    parts.dedup_by_key(|(name, _)| name.subject);
    parts
}

/// The world's texts, listing each checkout file read (tree 0: `/0/<path>`) and each looked
/// for and not found.
struct Recording<'r, 'w> {
    inner: &'r blaze_world::Texts<'w>,
    read: RefCell<std::vec::Vec<String, Global>>,
    absent: RefCell<std::vec::Vec<String, Global>>,
}

impl<'w> SourceTexts<'w> for Recording<'_, 'w> {
    fn read(&self, krate: usize, path: &str) -> Option<&'w str> {
        let text = self.inner.read(krate, path);
        // A file there is read as its `Surface`; a probe that found nothing (a module's other
        // place, `m.rs` beside `m/mod.rs`, a missing `mod x;`) as its `Surface`'s absence.
        if let Some(inner) = path.strip_prefix("/0/") {
            match text {
                Some(_) => self.read.borrow_mut().push(String::from(inner)),
                None => self.absent.borrow_mut().push(String::from(inner)),
            }
        }
        text
    }
}


/// The surface name of a checkout file.
/// The name a file the resolver read is listed by: the slot `a/x.rs` for `a/x/mod.rs` found
/// through `mod x;`, else the path itself. A `mod.rs` file is a slot's exactly when the
/// resolve also looked at that slot (`probed`): `mod x;` looks at `a/x.rs` before
/// `a/x/mod.rs`, while a crate root (`[lib] path = "mod.rs"`) and a `#[path]` naming a `mod.rs`
/// file are opened by their own path and listed by it (units.rs, work.rs `module_file`).
/// (Before 2026-11-16 every `mod.rs` was mapped: `hir-ty-codegen`'s root, `crates/
/// hir-ty-codegen/mod.rs`, was asked for as `crates/hir-ty-codegen.rs`, which nothing lists.)
/// Owed: the texts interface saying how it reached a file, so this is told, not inferred; a
/// resolver that looked at `x/mod.rs` first would end in `Unlisted`, by name, never quietly.
fn slot_of(path: String, probed: impl Fn(&str) -> bool) -> String {
    match path.strip_suffix("/mod.rs") {
        Some(stem) => {
            let mut slot = String::with_capacity(stem.len() + 3);
            slot.push_str(stem);
            slot.push_str(".rs");
            if probed(&slot) { slot } else { path }
        }
        None => path,
    }
}

fn surface(path: &str) -> Name {
    Name::new(Kind::Surface, Name::file(path.as_bytes()).subject)
}

/// One `CrateScope` task.
#[expect(clippy::too_many_arguments, reason = "a task's edge: each argument one handle or frame of it")]
pub(crate) fn run<'f, 'b>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    store: &mut dyn Store,
    names: &mut Option<Names<'f>>,
    lock: Option<&'f Lock<'static>>,
    members: Option<&[(LockId, blaze_unit_graph::Text<'_>)]>,
    env: &FetchEnv<'_>,
    task: &Task<'_>,
    frame: &Frame,
    held: &'b Frame,
) -> Result<Did<'b>, CliError> {
    let cx = cx.span("crate scope: resolve one crate's names");
    let name = task.name;
    let refused = |what: &'static str| TaskFailure { name, why: Why::Refused { what, rule: RULE } };
    if names.is_none() {
        let Some(graph) = task.inputs.iter().find(|input: &&Input| input.name == Name::unit_graph()) else {
            let mut wants = blaze_own::Vec::with_capacity_in(1, held);
            wants.push(Want { name: Name::unit_graph(), home: Name::unit_graph() });
            return Ok(Did::Need(wants));
        };
        let read = crate::work::payload(&cx, store, graph, frame)?;
        match world(&cx, root, read.payload(), lock, members, env) {
            Ok(made) => {
                tell_public(store, &made);
                *names = Some(made);
            }
            Err(error) => {
                let words = bstd::mem::format_in!(held, "names not resolved: {error}");
                return Ok(Did::FailedSaying(refused("the crates' inputs could not be made"), held_str(words.as_str(), held)));
            }
        }
    }
    let names = names.as_mut().expect("made above");
    let Ok(at) = names.by_subject.binary_search_by_key(&name.subject, |(subject, _)| *subject).map(|i| names.by_subject[i].1) else {
        return Ok(Did::FailedSaying(refused("a crate the world does not hold"), "a `CrateScope` part names a crate this command's world does not hold"));
    };
    // Any need that failed this command (a dependency's names, a file's surface whose outline
    // failed): this crate fails `Waited` on it, alone (rule 20); asking again would loop.
    if let Some(failed) = task.failed.first() {
        return Ok(Did::Failed(TaskFailure { name, why: Why::Waited { on: *failed } }));
    }
    let crates = &names.world.crates;
    let input = crates[at].input;
    // The records it reads, by crate index: direct dependencies, the sysroot crates, then each
    // a `Need` named (any `CrateScope` input beyond those).
    let mut local: std::vec::Vec<usize, Global> = std::vec::Vec::new();
    let place = |global: usize, local: &mut std::vec::Vec<usize, Global>| match local.iter().position(|g| *g == global) {
        Some(p) => p,
        None => {
            local.push(global);
            local.len() - 1
        }
    };
    let mut own_deps: std::vec::Vec<(&'f str, usize), Global> = std::vec::Vec::with_capacity(input.deps.len());
    for &(dep, index) in input.deps {
        own_deps.push((dep, place(index, &mut local)));
    }
    let sysroot = names.world.sysroot;
    let all = [sysroot.core, sysroot.alloc, sysroot.std, sysroot.bstd];
    // A toolchain crate is given only the sysroot crates below it (its dependencies' closure),
    // or `core` and `alloc` would each wait on the other; any other crate is given all of them.
    let below = {
        let mut seen: std::vec::Vec<bool, Global> = std::vec::Vec::new();
        seen.resize(crates.len(), false);
        let mut stack: std::vec::Vec<usize, Global> = std::vec::Vec::new();
        stack.extend(input.deps.iter().map(|(_, index)| *index));
        while let Some(g) = stack.pop() {
            if !core::mem::replace(&mut seen[g], true) {
                stack.extend(crates[g].input.deps.iter().map(|(_, index)| *index));
            }
        }
        seen
    };
    let toolchain = at < names.world.toolchain_crates as usize;
    let mut mapped = [None; 4];
    for (slot, s) in mapped.iter_mut().zip(all) {
        *slot = s.filter(|s| s.index() != at && (!toolchain || below[s.index()])).map(|s| place(s.index(), &mut local));
    }
    for input in task.inputs.iter().filter(|input| input.name.kind == Kind::CrateScope) {
        if let Ok(i) = names.by_subject.binary_search_by_key(&input.name.subject, |(subject, _)| *subject) {
            place(names.by_subject[i].1, &mut local);
        }
    }
    let have = |g: usize| task.inputs.iter().find(|input| input.name == name_of(crates[g].input.key));
    let missing: std::vec::Vec<usize, Global> = local.iter().copied().filter(|g| have(*g).is_none()).collect();
    if !missing.is_empty() {
        let mut wants = blaze_own::Vec::with_capacity_in(missing.len(), held);
        wants.extend(missing.iter().map(|g| Want { name: name_of(crates[*g].input.key), home: Name::unit_graph() }));
        return Ok(Did::Need(wants));
    }
    let made_at = names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject);
    if made_at.is_err() {
        let mut deps: std::vec::Vec<&'static [u8], Global> = std::vec::Vec::with_capacity(local.len());
        for g in local.iter() {
            let read = crate::work::payload(&cx, store, have(*g).expect("checked above"), frame)?;
            let record = match sections::sections(read.payload()) {
                Ok(parts) => parts.exports,
                Err(error) => {
                    let words = bstd::mem::format_in!(held, "crate `{}`: the record of its dependency `{}`: {error}", input.name, crates[*g].input.name);
                    return Ok(Did::FailedSaying(refused("a dependency's record"), held_str(words.as_str(), held)));
                }
            };
            #[cfg(test)]
            sections::watch(crates[*g].input.name, read.payload());
            deps.push(blaze_scope::resolve::aligned(record, Alloc::global()));
        }
        let me = local.len();
        let own = |s: Option<CrateId>| s.filter(|s| s.index() == at).map(|_| CrateId(me as u32));
        let ids = mapped.map(|p| p.map(|p| CrateId(p as u32)));
        let local_sysroot = Sysroot { core: ids[0].or(own(all[0])), alloc: ids[1].or(own(all[1])), std: ids[2].or(own(all[2])), bstd: ids[3].or(own(all[3])) };
        let own_deps: &'f [(&'f str, usize)] = std::vec::Vec::leak(own_deps);
        let local_input = CrateInput { deps: own_deps, ..input };
        let texts = Recording { inner: &names.world.texts, read: RefCell::new(std::vec::Vec::new()), absent: RefCell::new(std::vec::Vec::new()) };
        let done = {
            let _cx = cx.span("crate scope: resolve_crate");
            resolve_crate(&local_input, &deps, local_sysroot, &texts, at, &Serial, names.prepares, Alloc::global())
        };
        let done = match done {
            Ok(done) => done,
            Err(CrateScopeError::Need(keys)) => {
                let mut wants = blaze_own::Vec::with_capacity_in(keys.len(), held);
                for key in keys.iter() {
                    let Ok(i) = names.by_key.binary_search_by_key(key, |(k, _)| *k) else {
                        return Ok(Did::FailedSaying(refused("a needed crate the world does not hold"), "the resolution needs a crate's record this command's world does not hold"));
                    };
                    wants.push(Want { name: name_of(crates[names.by_key[i].1].input.key), home: Name::unit_graph() });
                }
                return Ok(Did::Need(wants));
            }
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: its names could not be resolved: {error:?}", input.name);
                return Ok(Did::FailedSaying(refused("the crate's names could not be resolved"), held_str(words.as_str(), held)));
            }
        };
        let mut findings = blaze_own::Vec::new_in(Global);
        let unresolved = done.names.notes.len() as u32 + unresolved(&done.names, &mut findings);
        // The names and items records, filed in the same record (drive.md, "End state", piece
        // 3): a body is checked by reading them, never by solving the crate graph again.
        let names_record = match blaze_scope::names_record::encode(done.names, Alloc::global()) {
            Ok(bytes) => bytes,
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: its names record could not be written: {error:?}", input.name);
                return Ok(Did::FailedSaying(refused("the crate's names record could not be written"), held_str(words.as_str(), held)));
            }
        };
        let items_record = match blaze_scope::items_record::encode(done.names, Alloc::global()) {
            Ok(bytes) => bytes,
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: its items record could not be written: {error:?}", input.name);
                return Ok(Did::FailedSaying(refused("the crate's items record could not be written"), held_str(words.as_str(), held)));
            }
        };
        // A test unit's tests, from attributes alone (test.md, "The tests record", piece 6): the
        // fifth section. A unit without `test` in its cfg has none, and its section is empty.
        let tests_record = match blaze_scope::tests_record::encode(done.names, Alloc::global()) {
            Ok(bytes) => Some(bytes),
            Err(blaze_scope::tests_record::TestsRecordError::NotATestUnit { .. }) => None,
            Err(error) => {
                let words = bstd::mem::format_in!(held, "crate `{}`: its tests record could not be written: {error}", input.name);
                return Ok(Did::FailedSaying(refused("the crate's tests record could not be written"), held_str(words.as_str(), held)));
            }
        };
        let tests: &[u8] = match &tests_record {
            Some(bytes) => &bytes[..],
            None => &[],
        };
        let payload = sections::write(unresolved, [&done.record[..], &done.inner[..], &names_record[..], &items_record[..], tests]);
        #[cfg(test)]
        sections::watch_tests(input.name, &payload, tests);
        // The drive names a module file by its slot (`x.rs` for `x/mod.rs` too, sweep.rs): a
        // read of either place is a read of the slot's `Surface`.
        let (mut read, mut lacked) = (texts.read.into_inner(), texts.absent.into_inner());
        read.sort_unstable();
        lacked.sort_unstable();
        let (mut files, mut absent) = (read.clone(), lacked.clone());
        let looked = |slot: &str| [&read, &lacked].iter().any(|list| list.binary_search_by(|path| path.as_str().cmp(slot)).is_ok());
        for path in files.iter_mut().chain(absent.iter_mut()) {
            *path = slot_of(core::mem::take(path), looked);
        }
        files.sort_unstable();
        files.dedup();
        absent.sort_unstable();
        absent.dedup();
        absent.retain(|path| files.binary_search(path).is_err());
        names.resolved += 1;
        names.unresolved += u64::from(unresolved);
        let resolved = Resolved { out: ContentId::of(OUT_DOMAIN, &done.record), payload, files, absent, findings };
        let i = names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject).unwrap_err();
        names.made.insert(i, (name.subject, resolved));
    }
    let i = names.made.binary_search_by_key(&name.subject, |(subject, _)| *subject).expect("made above");
    let resolved = &names.made[i].1;
    // Each checkout file it read is read as its `Surface`: ask for those not given yet, with
    // the dependencies' records again, so the entry after holds them all.
    let unread: std::vec::Vec<&String, Global> = resolved.files.iter().filter(|path| !task.inputs.iter().any(|input| input.name == surface(path))).collect();
    if !unread.is_empty() {
        let mut wants = blaze_own::Vec::with_capacity_in(unread.len() + local.len(), held);
        wants.extend(unread.iter().map(|path| Want { name: surface(path), home: Name::file(path.as_bytes()) }));
        wants.extend(local.iter().map(|g| Want { name: name_of(crates[*g].input.key), home: Name::unit_graph() }));
        return Ok(Did::Need(wants));
    }
    let mut made = Made::new_in(name, held);
    made.out = resolved.out;
    made.payload.extend_from_slice(&resolved.payload);
    for finding in resolved.findings.iter() {
        let words = core::str::from_utf8(&finding.message).map_err(|_| CliError::Text { what: "a path's segment is not UTF-8" })?;
        made.find(UNRESOLVED, finding.at, &[words]);
    }
    for path in resolved.files.iter() {
        let input = task.inputs.iter().find(|input| input.name == surface(path)).expect("checked above");
        made.reads.push((input.name, input.out));
    }
    for path in resolved.absent.iter() {
        made.reads.push((surface(path), blaze_drive::key::ABSENT));
    }
    for g in local.iter() {
        let input = have(*g).expect("checked above");
        made.reads.push((input.name, input.out));
    }
    Ok(Did::Done(made))
}
