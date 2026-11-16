//! The request machine's simulation (drive.md, "Tests"; rules 43, 64): a fake store that counts
//! its reads, a fake checkout of toy items (a signature that may use an earlier item's, a body
//! that calls any item's signature), and a fake executor whose answers arrive in an order the
//! tape picks. The oracle: an incremental request leaves the same answer table and prints the
//! same findings as a fresh build of the same files; a repeat runs nothing for one store read; a
//! one-body edit runs one `FileItems` and one `BodyCheck`; no two runs of one key are out at
//! once; any completion order writes equal bytes; a text-less crate's missing record is the
//! typed error. Two planted bugs show the oracle catches what it claims.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_id::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::Plant;
use crate::answers::{read_last, read_page, read_root};
use crate::condition::{Condition, Space, Var};
use crate::key::{self, Key, Kind, Name, Tool};
use crate::record::{Finding, Locate, Part, PathOf};
use crate::request::{
    Answer, Ask, BatchId, Builder, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Outcome, Output, Ran,
    Request, Task, TaskFailure, Ticket, Verdict, Want, Why,
};
use crate::settled::Settled;
use crate::worker::{self, InPlace};

// --- The toy checkout -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Item {
    sig: u64,
    body: u64,
    /// An earlier item whose signature this one's reads.
    uses: Option<(usize, usize)>,
    /// Items whose signatures the body reads.
    calls: StdVec<(usize, usize)>,
    /// The cfg variables the item reads (signature and body alike), in order.
    cfg: StdVec<Var>,
}

/// A workspace member: its manifest's text is its version; it lists the files it holds.
#[derive(Clone, Debug)]
struct Member {
    version: u64,
    files: StdVec<usize>,
}

fn member_path(member: usize) -> StdVec<u8> {
    format!("m{member}/Cargo.toml").into_bytes()
}

/// The units' source, as the cli hands it: every manifest's path and content (what the layout
/// read), so an edit of any manifest re-reads the root units.
fn units_source(files: &[FileEntry<'_>]) -> ContentId {
    let mut text = StdVec::new();
    for file in files.iter().filter(|f| f.manifest) {
        text.extend_from_slice(file.path);
        text.extend_from_slice(&file.id.0);
    }
    key::digest("sim units source", &text)
}

fn member_id(member: &Member) -> ContentId {
    key::digest("sim member manifest", &member.version.to_le_bytes())
}

#[derive(Clone, Debug)]
struct World {
    files: StdVec<StdVec<Item>>,
    /// A crate with no text: a part of the units found only by this record key.
    dep: Option<Key>,
    /// A workspace: the root manifest lists these members, each a manifest of its own that
    /// lists its files and holds one finding on its own lines. Empty: the root is the package.
    members: StdVec<Member>,
    /// Item and file names to their places, so the harness's own lookups are not a scan (the
    /// linearity test steps 100,000 jobs). Names do not depend on the text, so an edit keeps it.
    items: BTreeMap<Name, (usize, usize)>,
    file_subjects: BTreeMap<ContentId, usize>,
    /// A lock, read this command: `Start` hands its `UnitGraph` job's source.
    lock: Option<Lock>,
    /// The cfg space's value count per variable (empty: one configuration).
    values: StdVec<u16>,
    /// Every body is a finding whose message is its out digest: the conditions oracle compares
    /// every body's value, not only the quarter that are findings.
    find_all: bool,
    plant: Plant,
    /// The names step's crates (empty: no `CrateScope` tasks), and the proc-macro crates they
    /// call, each by its text.
    crates: StdVec<SimCrate>,
    macros: StdVec<u64>,
    /// The module store holds none of the macro crates' modules: each `MacroModule` fails with
    /// its one finding, as the cli's lookup does (macros.rs).
    modules_absent: bool,
    /// A file whose `FileItems` task fails (a planted panic, an oversize file): its surface,
    /// and each crate reading it, fail `Waited`; the request goes on.
    failing_file: Option<usize>,
}

/// A crate of the names step: its identity (what blaze-world's `UnitInput::key` hashes), the
/// files it holds, its direct dependencies (earlier crates), which of them it glob re-exports
/// (its scope then holds theirs), the proc-macro crates it calls, and whether the names library
/// refuses it.
#[derive(Clone, Debug)]
struct SimCrate {
    identity: u64,
    files: StdVec<usize>,
    deps: StdVec<usize>,
    globs: StdVec<usize>,
    macros: StdVec<usize>,
    fails: bool,
    /// A finding its scope holds, by its words: a library and its test unit (the same files
    /// under another identity) hold the same one.
    says: Option<StdVec<u8>>,
}

fn crate_scope_name(c: usize) -> Name {
    Name::new(Kind::CrateScope, key::digest("sim crate", format!("{c}").as_bytes()))
}

fn macro_name(m: usize) -> Name {
    Name::new(Kind::MacroModule, key::digest("sim macro crate", format!("{m}").as_bytes()))
}

fn surface_name(f: usize) -> Name {
    Name::new(Kind::Surface, file_name(f).subject)
}

/// A file's surface: its items' signatures and what they use, never a body.
fn surface_source(items: &[Item]) -> ContentId {
    let sigs: StdVec<ContentId> = items.iter().map(sig_source).collect();
    key::digest("sim surface", format!("{sigs:?}").as_bytes())
}

/// A crate's identity, as the world's key: its text's identity and its dependencies' and
/// macro crates' identities, never their surfaces.
fn crate_source(world: &World, c: usize) -> ContentId {
    let krate = &world.crates[c];
    let deps: StdVec<u64> = krate.deps.iter().map(|d| world.crates[*d].identity).collect();
    let macros: StdVec<u64> = krate.macros.iter().map(|m| world.macros[*m]).collect();
    key::digest("sim crate identity", format!("{} {deps:?} {macros:?}", krate.identity).as_bytes())
}

/// Up to four crates over the world's files, each depending on earlier ones, some calling one
/// of two proc-macro crates; `fails` none.
fn draw_crates(tape: &Tape, world: &mut World) {
    let n = 1 + tape.index(4, "crates");
    world.macros = (0..1 + tape.index(2, "macro crates")).map(|m| 7_000 + m as u64).collect();
    for c in 0..n {
        let files = (0..world.files.len()).filter(|f| f % n == c).collect();
        let deps: StdVec<usize> = (0..c).filter(|_| tape.one_in(2, "dep")).collect();
        let globs = deps.iter().copied().filter(|_| tape.one_in(3, "glob")).collect();
        let macros = (0..world.macros.len()).filter(|_| tape.one_in(2, "calls macro")).collect();
        world.crates.push(SimCrate { identity: 5_000 + c as u64, files, deps, globs, macros, fails: false, says: None });
    }
}

/// The `CrateScope` task: its files' surfaces, its dependencies' scopes and its macro crates'
/// modules read; one of them failed this command fails it with `Waited`. Its out holds its own
/// surface and the scopes it glob re-exports, so a dependent's out moves only through a glob.
fn crate_scope(world: &World, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    let c = (0..world.crates.len()).find(|c| crate_scope_name(*c) == name).ok_or(Broke::Harness(format!("no crate for {name}")))?;
    if task.source != crate_source(world, c) {
        return Err(Broke::Oracle(format!("{name} ran with a source that is not its crate's identity")));
    }
    let krate = &world.crates[c];
    if krate.fails {
        return Ok(Answered::Failed(TaskFailure { name, why: Why::Refused { what: "the names library refused the crate", rule: "sim" } }));
    }
    let mut wanted: StdVec<(Name, Name)> = krate.files.iter().map(|f| (surface_name(*f), file_name(*f))).collect();
    wanted.extend(krate.deps.iter().map(|d| (crate_scope_name(*d), Name::units())));
    wanted.extend(krate.macros.iter().map(|m| (macro_name(*m), Name::units())));
    if let Some((on, _)) = wanted.iter().find(|(n, _)| task.failed.contains(n)) {
        return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: *on } }));
    }
    match reads_of(task, &wanted) {
        Err(lacking) => Ok(Answered::Need(lacking)),
        Ok(reads) => {
            let own: StdVec<ContentId> = krate.files.iter().filter_map(|f| input_out(task, surface_name(*f))).collect();
            let globbed: StdVec<ContentId> = krate.globs.iter().filter_map(|d| input_out(task, crate_scope_name(*d))).collect();
            let out = key::digest("sim crate scope out", format!("{c} {own:?} {globbed:?}").as_bytes());
            let findings = krate.says.iter().map(|words| (TWIN_RULE, 3, 7, words.clone())).collect();
            Ok(Answered::Done(Made { name, out, reads, parts: StdVec::new(), findings, paths: StdVec::new() }))
        }
    }
}

/// The rule of a sim crate's own finding ([`SimCrate::says`]).
const TWIN_RULE: u16 = 0x51D0;

fn path(file: usize) -> StdVec<u8> {
    format!("src/f{file}.rs").into_bytes()
}

fn file_name(file: usize) -> Name {
    Name::file(&path(file))
}

fn sig_name(file: usize, item: usize) -> Name {
    Name::new(Kind::Signature, key::digest("sim sig name", format!("{file}/{item}").as_bytes()))
}

fn body_name(file: usize, item: usize) -> Name {
    Name::new(Kind::BodyCheck, key::digest("sim body name", format!("{file}/{item}").as_bytes()))
}

fn dep_name() -> Name {
    Name::new(Kind::Fetch, key::digest("sim dep", b"dep"))
}

/// A toy `Cargo.lock` and what the members name of it: each entry lists the entries it depends
/// on, which are known only once its own record (its manifest) is read, as a real one's are.
#[derive(Clone, Debug)]
struct Lock {
    entries: StdVec<StdVec<usize>>,
    /// The entries the workspace's members depend on: the graph's roots.
    roots: StdVec<usize>,
    /// Entries whose `Fetch` fails this command (the network's fault: no record kept).
    failing: StdVec<usize>,
}

/// The `Fetch` name of lock entry `at`: its subject is its record key.
fn entry_name(at: usize) -> Name {
    Name::new(Kind::Fetch, key::digest("sim lock entry", format!("{at}").as_bytes()))
}

/// The `UnitGraph` job's source: the lock and the roots, as `unit_graph_task::source` digests
/// the lock and the layout's reads.
fn graph_source(lock: &Lock) -> ContentId {
    key::digest("sim unit graph source", format!("{:?} {:?}", lock.entries, lock.roots).as_bytes())
}

/// The `UnitGraph` task: the reach from the roots, each entry's dependencies known once its
/// `Fetch` record is an input; the entries reached and not yet handed are asked for, level by
/// level, unless their `Fetch` failed this command (`Task::failed`), which leaves no graph.
fn unit_graph(lock: &Lock, task: &Task<'static>) -> Answered {
    let mut seen: BTreeSet<usize> = lock.roots.iter().copied().collect();
    let mut frontier: StdVec<usize> = lock.roots.clone();
    let mut reads = StdVec::new();
    let mut wants = StdVec::new();
    let mut failed = 0;
    while let Some(at) = frontier.pop() {
        let name = entry_name(at);
        match input_out(task, name) {
            Some(out) => {
                reads.push((name, out));
                for dep in &lock.entries[at] {
                    if seen.insert(*dep) {
                        frontier.push(*dep);
                    }
                }
            }
            None if task.failed.contains(&name) => failed += 1,
            None => wants.push(Want { name, home: name }),
        }
    }
    if !wants.is_empty() {
        return Answered::Need(wants);
    }
    if failed > 0 {
        return Answered::Failed(TaskFailure { name: task.name, why: Why::Refused { what: "sources not fetched", rule: "sim" } });
    }
    reads.sort();
    let out = key::digest("sim unit graph out", format!("{reads:?}").as_bytes());
    Answered::Done(Made { name: task.name, out, reads, parts: StdVec::new(), findings: StdVec::new(), paths: StdVec::new() })
}

fn sig_source(item: &Item) -> ContentId {
    key::digest("sim sig source", format!("{} {:?}", item.sig, item.uses).as_bytes())
}

fn body_source(item: &Item) -> ContentId {
    key::digest("sim body source", format!("{} {:?}", item.body, item.calls).as_bytes())
}

impl World {
    fn draw(tape: &Tape) -> World {
        let nfiles = 1 + tape.index(3, "files");
        let mut files = StdVec::new();
        let mut earlier: StdVec<(usize, usize)> = StdVec::new();
        let sizes: StdVec<usize> = (0..nfiles).map(|_| 1 + tape.index(4, "items")).collect();
        let all: StdVec<(usize, usize)> =
            sizes.iter().enumerate().flat_map(|(f, n)| (0..*n).map(move |i| (f, i))).collect();
        for (f, n) in sizes.iter().enumerate() {
            let mut items = StdVec::new();
            for i in 0..*n {
                let uses = match earlier.is_empty() || tape.one_in(2, "no use") {
                    true => None,
                    false => Some(earlier[tape.index(earlier.len(), "use")]),
                };
                let calls = (0..tape.index(3, "calls")).map(|_| all[tape.index(all.len(), "call")]).collect();
                items.push(Item { sig: tape.below(4, "sig"), body: tape.below(4, "body"), uses, calls, cfg: StdVec::new() });
                earlier.push((f, i));
            }
            files.push(items);
        }
        World::new(files)
    }

    fn new(files: StdVec<StdVec<Item>>) -> World {
        let mut items = BTreeMap::new();
        let mut file_subjects = BTreeMap::new();
        for (f, file) in files.iter().enumerate() {
            file_subjects.insert(file_name(f).subject, f);
            for i in 0..file.len() {
                items.insert(sig_name(f, i), (f, i));
                items.insert(body_name(f, i), (f, i));
            }
        }
        World {
            files,
            dep: None,
            members: StdVec::new(),
            items,
            file_subjects,
            lock: None,
            values: StdVec::new(),
            find_all: false,
            plant: Plant::None,
            crates: StdVec::new(),
            macros: StdVec::new(),
            modules_absent: false,
            failing_file: None,
        }
    }

    /// The member whose manifest's units `name` is.
    fn member_of(&self, name: Name) -> Option<usize> {
        (0..self.members.len()).find(|k| Name::manifest(&member_path(*k)) == name)
    }

    fn bytes(&self, file: usize) -> StdVec<u8> {
        format!("{:?}", self.files[file]).into_bytes()
    }

    fn id(&self, file: usize) -> ContentId {
        key::digest("sim file", &self.bytes(file))
    }

    fn file_of(&self, subject: &ContentId) -> Option<usize> {
        self.file_subjects.get(subject).copied()
    }

    fn item_of(&self, name: Name) -> Option<(usize, usize)> {
        self.items.get(&name).copied()
    }
}

// --- The fake store ---------------------------------------------------------------------------

#[derive(Default, Clone, PartialEq, Eq)]
struct Store {
    records: BTreeMap<Key, StdVec<u8>>,
    reads: u32,
}

impl Store {
    fn put(&mut self, key: Key, bytes: &[u8]) -> Result<(), Broke> {
        match self.records.get(&key) {
            Some(held) if held.as_slice() != bytes => {
                Err(Broke::Oracle(format!("two values under one key {key} (rule 21)")))
            }
            Some(_) => Ok(()),
            None => {
                self.records.insert(key, bytes.to_vec());
                Ok(())
            }
        }
    }
}

// --- The fake executor ------------------------------------------------------------------------

struct Made {
    name: Name,
    out: ContentId,
    reads: StdVec<(Name, ContentId)>,
    parts: StdVec<Part>,
    findings: StdVec<(u16, u32, u32, StdVec<u8>)>,
    /// Where each file part's file is (a units record's), for a request that opens them.
    paths: StdVec<(Name, StdVec<u8>)>,
}

/// The paths of the file parts among `parts`: the sim's files are crate roots, never a `mod` slot.
fn paths_of(world: &World, parts: &[Part]) -> StdVec<(Name, StdVec<u8>)> {
    let files = parts.iter().filter(|part| part.name.kind == Kind::FileItems);
    files.filter_map(|part| Some((part.name, path(world.file_of(&part.name.subject)?)))).collect()
}

enum Answered {
    Done(Made),
    Need(StdVec<Want>),
    Cfg(Var),
    Failed(TaskFailure),
}

fn input_out(task: &Task<'static>, name: Name) -> Option<ContentId> {
    task.inputs.iter().find(|input| input.name == name).map(|input| input.out)
}

fn reads_of(task: &Task<'static>, wanted: &[(Name, Name)]) -> Result<StdVec<(Name, ContentId)>, StdVec<Want>> {
    let mut reads = StdVec::new();
    let mut lacking = StdVec::new();
    for (name, home) in wanted {
        match input_out(task, *name) {
            Some(out) => reads.push((*name, out)),
            None => lacking.push(Want { name: *name, home: *home }),
        }
    }
    match lacking.is_empty() {
        true => {
            reads.sort();
            reads.dedup();
            Ok(reads)
        }
        false => Err(lacking),
    }
}

/// An item's out digest: its value, what it read (sorted, by base name), the cfg values read.
fn item_out(value: u64, reads: &[(Name, ContentId)], cfg: &[u16]) -> ContentId {
    key::digest("sim item out", format!("{value} {reads:?} {cfg:?}").as_bytes())
}

/// A body's finding, if it is one.
fn finding(world: &World, kind: Kind, out: &ContentId) -> Option<StdVec<u8>> {
    match kind == Kind::BodyCheck && (world.find_all || out.0[0] < 64) {
        true => Some(format!("toy finding {:02x}{:02x}{:02x}{:02x}", out.0[1], out.0[2], out.0[3], out.0[4]).into_bytes()),
        false => None,
    }
}

fn execute(world: &World, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    let done = |out, reads, parts: StdVec<Part>, findings| {
        let paths = paths_of(world, &parts);
        Ok(Answered::Done(Made { name, out, reads, parts, findings, paths }))
    };
    match name.kind {
        Kind::Units if !world.members.is_empty() && name != Name::units() => {
            // A member's manifest: its files, and one finding on its own lines.
            let k = world.member_of(name).ok_or(Broke::Harness(format!("no member for {name}")))?;
            let member = &world.members[k];
            if task.source != key::file_source(member_id(member), 0) {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its manifest's")));
            }
            let mut parts: StdVec<Part> =
                member.files.iter().map(|f| Part { name: file_name(*f), locate: Locate::File { edition: EDITION }, place: file_place(*f) }).collect();
            parts.sort_by_key(|part| part.name);
            let out = key::digest("sim member out", format!("{k} {}", member.version).as_bytes());
            let line = u32::try_from(k).unwrap_or(u32::MAX);
            let findings = std::vec![(2, line, line + 1, format!("member {k} version {}", member.version).into_bytes())];
            done(out, StdVec::new(), parts, findings)
        }
        Kind::Units => {
            let mut parts = StdVec::new();
            if world.members.is_empty() {
                for f in 0..world.files.len() {
                    parts.push(Part { name: file_name(f), locate: Locate::File { edition: EDITION }, place: file_place(f) });
                }
            } else {
                // A workspace root: each member is its manifest's units.
                for k in 0..world.members.len() {
                    parts.push(Part { name: Name::manifest(&member_path(k)), locate: Locate::File { edition: 0 }, place: 0 });
                }
            }
            if let Some(dep) = world.dep {
                parts.push(Part { name: dep_name(), locate: Locate::Key(dep), place: 0 });
            }
            // The names step: one `CrateScope` per crate by its identity, one `MacroModule` per
            // proc-macro crate by its text (the unit graph's, in the cli).
            for c in 0..world.crates.len() {
                parts.push(Part { name: crate_scope_name(c), locate: Locate::Source(crate_source(world, c)), place: 0 });
            }
            for (m, text) in world.macros.iter().enumerate() {
                let source = key::digest("sim macro text", &text.to_le_bytes());
                parts.push(Part { name: macro_name(m), locate: Locate::Source(source), place: 0 });
            }
            if !world.crates.is_empty() {
                parts.sort_by_key(|part| part.name);
            }
            let out = key::digest("sim units out", format!("{}", world.files.len()).as_bytes());
            done(out, StdVec::new(), parts, StdVec::new())
        }
        Kind::FileItems => {
            let f = world.file_of(&task.base.subject).ok_or(Broke::Harness(format!("no file for {name}")))?;
            // A file is read in the edition its home's part named, on every path to it (a
            // repeat whose home stood finds it in the file's row).
            if task.edition != EDITION || task.source != key::file_source(world.id(f), EDITION) {
                return Err(Broke::Oracle(format!("{name} ran in edition {}, not its part's {EDITION}", task.edition)));
            }
            if world.failing_file == Some(f) {
                return Ok(Answered::Failed(TaskFailure { name, why: Why::Panicked { used: 0, limit: 0 } }));
            }
            let mut parts = StdVec::new();
            for (i, item) in world.files[f].iter().enumerate() {
                let place = u32::try_from(i).unwrap_or(u32::MAX);
                parts.push(Part { name: sig_name(f, i), locate: Locate::Source(sig_source(item)), place });
                parts.push(Part { name: body_name(f, i), locate: Locate::Source(body_source(item)), place });
            }
            if !world.crates.is_empty() {
                parts.push(Part { name: surface_name(f), locate: Locate::Source(surface_source(&world.files[f])), place: 0 });
            }
            parts.sort_by_key(|part| part.name);
            let out = key::digest("sim file out", format!("{:?}", parts).as_bytes());
            // A file of an even count of items has a finding on its own text, at 1..2.
            let findings = match world.files[f].len() % 2 {
                0 => std::vec![(1, 1, 2, format!("file {f}").into_bytes())],
                _ => StdVec::new(),
            };
            done(out, StdVec::new(), parts, findings)
        }
        Kind::Signature | Kind::BodyCheck => {
            let (f, i) = world.item_of(task.base).ok_or(Broke::Harness(format!("no item for {name}")))?;
            let item = &world.files[f][i];
            // Its cfg reads first: one the condition does not fix is `Need(Cfg(variable))`.
            let mut values = StdVec::new();
            for var in &item.cfg {
                match task.condition.get(*var) {
                    Some(value) => values.push(value),
                    None => return Ok(Answered::Cfg(*var)),
                }
            }
            let (wanted, value, source): (StdVec<(Name, Name)>, u64, ContentId) = match name.kind {
                Kind::Signature => {
                    let wanted = item.uses.iter().map(|(uf, ui)| (sig_name(*uf, *ui), file_name(*uf))).collect();
                    (wanted, item.sig, sig_source(item))
                }
                _ => {
                    let wanted = item.calls.iter().map(|(cf, ci)| (sig_name(*cf, *ci), file_name(*cf))).collect();
                    (wanted, item.body, body_source(item))
                }
            };
            if source != task.source {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its text's")));
            }
            if let Some((on, _)) = wanted.iter().find(|(n, _)| task.failed.contains(n)) {
                return Ok(Answered::Failed(TaskFailure { name, why: Why::Waited { on: *on } }));
            }
            match reads_of(task, &wanted) {
                Err(lacking) => Ok(Answered::Need(lacking)),
                Ok(reads) => {
                    let out = item_out(value, &reads, &values);
                    let findings = match finding(world, name.kind, &out) {
                        Some(message) => std::vec![(7, 0, 1, message)],
                        None => StdVec::new(),
                    };
                    done(out, reads, StdVec::new(), findings)
                }
            }
        }
        Kind::UnitGraph => {
            let lock = world.lock.as_ref().ok_or(Broke::Harness(format!("{name} ran with no lock")))?;
            if task.source != graph_source(lock) {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its lock's")));
            }
            Ok(unit_graph(lock, task))
        }
        Kind::Fetch if world.lock.is_some() => {
            let lock = world.lock.as_ref().ok_or(Broke::Harness(format!("{name} ran with no lock")))?;
            let at = (0..lock.entries.len()).find(|at| entry_name(*at) == name).ok_or(Broke::Oracle(format!("{name} is no lock entry")))?;
            if task.source != name.subject {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its subject")));
            }
            match lock.failing.contains(&at) {
                true => Ok(Answered::Failed(TaskFailure { name, why: Why::Refused { what: "the network", rule: "sim" } })),
                false => done(name.subject, StdVec::new(), StdVec::new(), StdVec::new()),
            }
        }
        Kind::CrateScope => crate_scope(world, task),
        // A file's surface and a compiled module: the out is the source (the module store's
        // lookup, in the cli).
        Kind::Surface => {
            let f = world.file_of(&name.subject).ok_or(Broke::Harness(format!("no file for {name}")))?;
            if task.source != surface_source(&world.files[f]) {
                return Err(Broke::Oracle(format!("{name} ran with a source that is not its file's surface")));
            }
            done(task.source, StdVec::new(), StdVec::new(), StdVec::new())
        }
        Kind::MacroModule => match world.modules_absent {
            true => Ok(Answered::Failed(TaskFailure { name, why: Why::Refused { what: "a proc-macro module the store lacks", rule: "sim" } })),
            false => done(task.source, StdVec::new(), StdVec::new(), StdVec::new()),
        },
        kind => Err(Broke::Harness(format!("the sim has no {} task", kind.name()))),
    }
}

// --- One request, driven ----------------------------------------------------------------------

struct Ended {
    answer: Option<Answer>,
    error: Option<DriveError>,
    /// Runs emitted, by kind (in-place re-entries not counted: the coordinator never saw them).
    runs: BTreeMap<Kind, u32>,
    prints: StdVec<(Name, u16, u32, u32, StdVec<u8>, Option<StdVec<u8>>, u32)>,
    last: Option<StdVec<u8>>,
    /// Records handed back in `Done` outcomes.
    delivered: u32,
    /// Needs a worker answered from the settled table.
    in_place: u32,
    /// The program: started, stopped, and whether it is still running.
    started: bool,
    stopped: bool,
}

/// One request's setup: the workers and the command.
#[derive(Clone, Copy)]
struct Setup {
    workers: u32,
    ask: Ask<'static>,
    plant: Plant,
    /// The tool's identity (the binary's build id in blaze-cli).
    tool: &'static [u8],
    /// A first request hands only the manifests and opens each file where a units record says,
    /// as blaze-cli's cold run does; otherwise every file is handed at `Start`.
    opens: bool,
}

const SIM_TOOL: &[u8] = b"tool";

/// Every finding is an error in the sim: `run` fails on lints too.
fn every_rule(_: u16) -> bool {
    true
}

fn config(workers: u32, tool: &[u8]) -> Config {
    Config { tool: Tool(key::digest("sim tool", tool)), workers, most_entries: 32, most_jobs: 1 << 20, is_error: every_rule }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE };

/// 1, 2 or 16 workers, as the tape picks.
fn workers(tape: &Tape) -> u32 {
    [1, 2, 16][tape.index(3, "workers")]
}

fn setup(tape: &Tape, plant: Plant) -> Setup {
    Setup { workers: workers(tape), ask: ASK, plant, tool: SIM_TOOL, opens: tape.one_in(2, "open the files") }
}

/// The most events one request takes in the sim (rule 18): a toy build is far below it.
const MOST_EVENTS: u32 = 100_000;

fn request(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    store: &mut Store,
    last: Option<&[u8]>,
    plant: Plant,
) -> Result<Ended, Broke> {
    request_with(tape, world, before, store, last, setup(tape, plant))
}

/// A batch as the sim's worker runs it: each task to `Done`, `Failed` or a `Need` no row of
/// the settled table answers, re-entered in place otherwise (`worker::answer_in_place`).
fn work(world: &World, store: &Store, table: &Settled<Global>, tasks: StdVec<(Ticket, Task<'static>)>) -> Result<(StdVec<(Ticket, Answered, u32)>, u32), Broke> {
    let mut answered = StdVec::new();
    let mut resolved = 0;
    for (ticket, mut task) in tasks {
        let mut in_place = 0;
        loop {
            match execute(world, &task)? {
                Answered::Need(wants) => {
                    let mut left: Vec<Want, Alloc<'static>> = Vec::new_in(Alloc::global());
                    let before = task.inputs.len();
                    let again = worker::answer_in_place(table, &mut task, &wants, 32, &mut left);
                    for input in &task.inputs[before..] {
                        // The table's promise: a row read is a record written.
                        if !store.records.contains_key(&input.at) {
                            return Err(Broke::Oracle(format!("a need answered before its record is written: {}", input.name)));
                        }
                    }
                    if again == InPlace::Again {
                        in_place += 1;
                        resolved += 1;
                        continue;
                    }
                    answered.push((ticket, Answered::Need(left.iter().copied().collect()), in_place));
                }
                done => answered.push((ticket, done, in_place)),
            }
            break;
        }
    }
    Ok((answered, resolved))
}

fn request_with(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    store: &mut Store,
    last: Option<&[u8]>,
    setup: Setup,
) -> Result<Ended, Broke> {
    let ids: StdVec<ContentId> = (0..world.files.len()).map(|f| world.id(f)).collect();
    let paths: StdVec<StdVec<u8>> = (0..world.files.len()).map(path).collect();
    // An opening first request knows no file: each is opened where a units record says. Later
    // ones are handed every file, as the cli's leaf table holds every file the answer read.
    let handed = match setup.opens && last.is_none() {
        true => 0,
        false => world.files.len(),
    };
    let mut files: StdVec<FileEntry<'_>> = (0..handed)
        .map(|f| FileEntry {
            path: &paths[f],
            id: ids[f],
            changed: before.is_none_or(|old| old.files.get(f).is_none_or(|_| old.id(f) != ids[f])),
            manifest: false,
        })
        .collect();
    let manifest = b"Cargo.toml".to_vec();
    let root_text: &[u8] = if world.members.is_empty() { b"[package]" } else { b"[workspace]" };
    files.push(FileEntry { path: &manifest, id: key::digest("sim manifest", root_text), changed: false, manifest: true });
    let member_paths: StdVec<StdVec<u8>> = (0..world.members.len()).map(member_path).collect();
    for (k, member) in world.members.iter().enumerate() {
        let id = member_id(member);
        let changed = before.is_none_or(|old| old.members.get(k).is_none_or(|was| member_id(was) != id));
        files.push(FileEntry { path: &member_paths[k], id, changed, manifest: true });
    }
    files.sort_by(|a, b| a.path.cmp(b.path));
    // The lock and the members' dependency lines are the layout's reads too (blaze-cli reads
    // the lock with it): a change to either moves the units' source, as an edit there does.
    let units = match &world.lock {
        None => units_source(&files),
        Some(lock) => key::digest("sim units and lock", &[units_source(&files).0, graph_source(lock).0].concat()),
    };

    let mut req = Request::planted_in(config(setup.workers, setup.tool), setup.ask, setup.plant, Alloc::global());
    let table: Settled<Global> = Settled::new_in(1 << 12, Global);
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(Alloc::global());
    let mut ended = Ended {
        answer: None,
        error: None,
        runs: BTreeMap::new(),
        prints: StdVec::new(),
        last: None,
        delivered: 0,
        in_place: 0,
        started: false,
        stopped: false,
    };
    let mut gets: StdVec<Key> = StdVec::new();
    let mut batches: StdVec<(BatchId, StdVec<(Ticket, Task<'static>)>)> = StdVec::new();
    // Puts are written later, in an order the tape picks, as a store's writes complete; a
    // `Publish` completes every put before it first (the effect's contract).
    let mut puts: StdVec<(Key, StdVec<u8>)> = StdVec::new();
    // Files the machine asked to open, answered in an order the tape picks.
    let mut opens: StdVec<Name> = StdVec::new();
    // The program, once started: it wants a body now and then, and exits when the tape says.
    let mut running = false;
    let bodies: StdVec<Name> =
        (0..world.files.len()).flat_map(|f| (0..world.files[f].len()).map(move |i| body_name(f, i))).collect();
    let graph = world.lock.as_ref().map(graph_source);
    let mut first = Some(Event::Start { files: &files, gone: &[], last, units, cold: false, graph });
    // The program's exit has been stepped: a stopped program's exit is an event too, not a
    // flag the harness sets when it sees `Stop`.
    let mut exited = false;
    for _ in 0..MOST_EVENTS {
        let stepped = match first.take() {
            Some(event) => {
                exited |= matches!(event, Event::Exited { .. });
                req.step(event, &mut out)
            }
            None => {
                let program = usize::from(running) * 2;
                let total = gets.len() + batches.len() + puts.len() + opens.len() + program;
                if total == 0 {
                    return Err(Broke::Oracle(String::from("the request stalled: nothing out and no answer")));
                }
                let pick = tape.index(total, "which completes");
                let opened = gets.len() + puts.len() + batches.len();
                if (opened..opened + opens.len()).contains(&pick) {
                    let name: Name = opens.swap_remove(pick - opened);
                    let f = world.file_of(&name.subject).ok_or(Broke::Oracle(format!("{name} was opened, and it is no file")))?;
                    req.step(Event::Opened { name, id: Some(world.id(f)) }, &mut out)
                } else if pick < gets.len() {
                    let key = gets.swap_remove(pick);
                    store.reads += 1;
                    // A get sees the store's writes in flight too: one process, one store.
                    let bytes = store.records.get(&key).cloned().or_else(|| puts.iter().find(|(k, _)| *k == key).map(|(_, b)| b.clone()));
                    req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
                } else if pick < gets.len() + puts.len() {
                    let (key, bytes) = puts.swap_remove(pick - gets.len());
                    store.put(key, &bytes)?;
                    continue;
                } else if pick < gets.len() + puts.len() + batches.len() {
                    let (id, tasks) = batches.swap_remove(pick - gets.len() - puts.len());
                    let (answered, resolved) = work(world, store, &table, tasks)?;
                    ended.in_place += resolved;
                    let mut held: StdVec<(StdVec<Finding<'_>>, StdVec<PathOf<'_>>, &Made)> = StdVec::new();
                    for (_, answer, _) in &answered {
                        if let Answered::Done(made) = answer {
                            let findings = made
                                .findings
                                .iter()
                                .map(|(rule, start, end, message)| Finding { rule: *rule, start: *start, end: *end, message })
                                .collect();
                            let paths = made.paths.iter().map(|(name, path)| PathOf { name: *name, path, module: false }).collect();
                            held.push((findings, paths, made));
                        }
                    }
                    let outputs: StdVec<Output<'_>> = held
                        .iter()
                        .map(|(findings, paths, made)| Output {
                            name: made.name,
                            out: made.out,
                            reads: &made.reads,
                            parts: &made.parts,
                            paths,
                            findings,
                            payload: b"",
                        })
                        .collect();
                    ended.delivered += u32::try_from(outputs.len()).unwrap_or(u32::MAX);
                    let mut at = 0;
                    let mut results = StdVec::new();
                    for (ticket, answer, in_place) in &answered {
                        let ran = match answer {
                            Answered::Need(wants) => Ran::Need(wants),
                            Answered::Cfg(var) => Ran::Cfg(*var),
                            Answered::Failed(failure) => Ran::Failed(*failure),
                            Answered::Done(_) => {
                                at += 1;
                                Ran::Done(&outputs[at - 1..at])
                            }
                        };
                        results.push(Outcome { ticket: *ticket, ran, in_place: *in_place, ns: 1_000 });
                    }
                    req.step(Event::Ran { batch: id, results: &results }, &mut out)
                } else if tape.one_in(2, "the program exits") {
                    running = false;
                    exited = true;
                    req.step(Event::Exited { ok: true }, &mut out)
                } else {
                    let name = bodies[tape.index(bodies.len(), "the program wants")];
                    let (f, _) = world.item_of(name).ok_or(Broke::Harness(format!("no item for {name}")))?;
                    req.step(Event::Want(Want { name, home: file_name(f) }), &mut out)
                }
            }
        };
        if let Err(error) = stepped {
            ended.error = Some(error);
            return Ok(ended);
        }
        for effect in out.drain(..) {
            match effect {
                Effect::Get { key } => gets.push(key),
                Effect::Put { key, bytes } => puts.push((key, bytes.as_slice().to_vec())),
                Effect::Publish(input) => {
                    for (key, bytes) in puts.drain(..) {
                        store.put(key, &bytes)?;
                    }
                    table.publish(input).map_err(|full| Broke::Harness(format!("{full}")))?;
                }
                Effect::Unpublish(name) => table.clear(name).map_err(|full| Broke::Harness(format!("{full}")))?,
                Effect::StartProgram { .. } => {
                    oracle(!ended.started, || String::from("the program started twice"))?;
                    ended.started = true;
                    running = true;
                }
                Effect::Stop { .. } => {
                    ended.stopped = true;
                    if running {
                        running = false;
                        first = Some(Event::Exited { ok: false });
                    }
                }
                Effect::Run { batch } => {
                    let mut tasks = StdVec::new();
                    for (ticket, task) in batch.tasks.into_iter() {
                        let out_now = batches.iter().flat_map(|(_, b)| b.iter()).chain(tasks.iter());
                        if out_now.clone().any(|(_, running): &(Ticket, Task<'static>)| running.key == task.key) {
                            return Err(Broke::Oracle(format!("single flight: {} ran twice at once", task.name)));
                        }
                        *ended.runs.entry(task.name.kind).or_insert(0) += 1;
                        tasks.push((ticket, task));
                    }
                    batches.push((batch.id, tasks));
                }
                Effect::Print(print) => {
                    let file = print.file.map(|index| req.path(index).to_vec());
                    ended.prints.push((
                        print.name,
                        print.rule,
                        print.start,
                        print.end,
                        print.message.as_slice().to_vec(),
                        file,
                        print.place,
                    ));
                }
                Effect::WriteLast { bytes } => ended.last = Some(bytes.as_slice().to_vec()),
                Effect::Done(answer) => ended.answer = Some(answer),
                // Every file is handed at `Start` here, so nothing is opened.
                Effect::Open { name, path: at, module } => {
                    oracle(handed == 0, || format!("{name} was opened, and every file was handed"))?;
                    let f = world.file_of(&name.subject).ok_or(Broke::Oracle(format!("{name} was opened, and it is no file")))?;
                    oracle(at.as_slice() == path(f).as_slice() && !module, || format!("{name} was opened at another path"))?;
                    opens.push(name);
                }
            }
        }
        if let Some(answer) = ended.answer {
            // The answer is the last thing the machine says: a program it started has exited
            // first, or its exit would step an answered machine.
            oracle(!ended.started || exited, || String::from("answered while the program it started is running"))?;
            // Under `Reached` the program's exit is the verdict: work still out is abandoned.
            let abandons = setup.ask.gate == Gate::Reached && setup.ask.command != Command::Check;
            let unfinished = !gets.is_empty() || !batches.is_empty() || (!answer.profile.certified && ended.last.is_none());
            if answer.verdict == Verdict::Passed && unfinished {
                return Err(Broke::Oracle(String::from("a success reported while the check could still find an error")));
            }
            if (!gets.is_empty() || !batches.is_empty()) && !abandons {
                return Err(Broke::Oracle(String::from("answered with work still out")));
            }
            for (key, bytes) in puts.drain(..) {
                store.put(key, &bytes)?;
            }
            let made: u32 = answer.profile.kinds.iter().map(|k| k.made).sum();
            // Fewer when a run of a dropped name was answered for nothing; never more.
            oracle(made <= ended.delivered, || {
                format!("a batch result applied twice: {made} records made from {} delivered", ended.delivered)
            })?;
            return Ok(ended);
        }
    }
    Err(Broke::Harness(format!("more than {MOST_EVENTS} events")))
}

fn finished(ended: Ended) -> Result<(Answer, Ended), Broke> {
    if let Some(error) = ended.error {
        return Err(Broke::Oracle(format!("the request failed: {error}")));
    }
    let answer = ended.answer.ok_or(Broke::Harness(String::from("no answer")))?;
    Ok((answer, ended))
}

type Table = BTreeMap<Name, (Key, Key, ContentId, ContentId, Name, u32, StdVec<(Name, ContentId)>, StdVec<Name>, StdVec<Name>)>;

/// The answer table `last` names, read from `store`.
fn table(store: &Store, last: &[u8]) -> Result<Table, Broke> {
    let wire = |error| Broke::Oracle(format!("a stored answer is not one: {error}"));
    let last = read_last(last).map_err(wire)?;
    let root_bytes = store.records.get(&last.root).ok_or(Broke::Oracle(String::from("the root is not stored")))?;
    let root = read_root(root_bytes, Alloc::global()).map_err(wire)?;
    let mut table = Table::new();
    for page_key in root.pages.iter() {
        if *page_key == Key::default() {
            continue;
        }
        let bytes = store.records.get(page_key).ok_or(Broke::Oracle(String::from("a page is not stored")))?;
        for (name, row) in read_page(bytes, Alloc::global()).map_err(wire)?.iter() {
            table.insert(
                *name,
                (
                    row.key,
                    row.at,
                    row.source,
                    row.out,
                    row.home,
                    row.place,
                    row.reads.as_slice().to_vec(),
                    row.parts.as_slice().to_vec(),
                    row.readers.as_slice().to_vec(),
                ),
            );
        }
    }
    Ok(table)
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

/// A fresh build of `world` in its own store: what every incremental answer must equal.
fn fresh(tape: &Tape, world: &World) -> Result<(Store, Ended), Broke> {
    let mut store = Store::default();
    let (_, ended) = finished(request(tape, world, None, &mut store, None, Plant::None)?)?;
    Ok((store, ended))
}

fn same_as_fresh(tape: &Tape, world: &World, store: &Store, ended: &Ended) -> Result<(), Broke> {
    let (fresh_store, fresh_ended) = fresh(tape, world)?;
    let last = ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let fresh_last = fresh_ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let here = table(store, last)?;
    let there = table(&fresh_store, fresh_last)?;
    oracle(here == there, || {
        let differ: StdVec<String> = here
            .keys()
            .chain(there.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|name| here.get(name) != there.get(name))
            .take(3)
            .map(|name| format!("{name}"))
            .collect();
        format!("the answer table differs from a fresh build at {differ:?}")
    })?;
    oracle(ended.prints == fresh_ended.prints, || String::from("the findings printed differ from a fresh build"))?;
    // A file's findings are placed in its own text: from 0, never from its `mod` in its home.
    let misplaced = ended.prints.iter().find(|print| print.0.kind == Kind::FileItems && print.6 != 0);
    oracle(misplaced.is_none(), || format!("a file's finding is placed at {misplaced:?}, not in its own text"))
}

/// The edition every file is read in: not 0, so a row that loses it is seen.
const EDITION: u8 = 3;

/// Where file `f`'s part stands in its home: not 0 and not [`EDITION`], so a place taken for
/// an edition, or an edition for a place, is seen.
fn file_place(f: usize) -> u32 {
    11 * (u32::try_from(f).unwrap_or(u32::MAX) + 1)
}

/// An edit of one item's signature or body, to a value no draw makes.
fn edit(tape: &Tape, world: &World, serial: u64, body: bool) -> World {
    let mut next = world.clone();
    let f = tape.index(next.files.len(), "edit file");
    let i = tape.index(next.files[f].len(), "edit item");
    match body {
        true => next.files[f][i].body = 1_000 + serial,
        false => next.files[f][i].sig = 1_000 + serial,
    }
    next
}

// --- The properties ---------------------------------------------------------------------------

fn edits(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let mut last = first.last;
    for serial in 0..1 + tape.below(3, "edits") {
        let next = edit(tape, &world, serial, tape.one_in(2, "body or signature"));
        let (_, ended) = finished(request(tape, &next, Some(&world), &mut store, last.as_deref(), plant)?)?;
        same_as_fresh(tape, &next, &store, &ended)?;
        last = ended.last.clone();
        world = next;
    }
    Ok(())
}

#[test]
fn an_incremental_answer_equals_a_fresh_build() {
    check(SimConfig::cases(300), |tape| edits(tape, Plant::None));
}

#[test]
fn control_a_skipped_changed_read_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::SkipChangedRead, "differs from a fresh build", edits);
}

/// The real binary's first edit of a module file (2026-11-16): its row had kept its `mod`'s place
/// for an edition, and the file was read in no edition.
#[test]
fn control_an_edition_kept_as_a_place_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::EditionAsPlace, "edition", edits);
}

fn flights(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    finished(request(tape, &world, None, &mut store, None, plant)?)?;
    Ok(())
}

#[test]
fn one_flight_per_key() {
    check(SimConfig::cases(300), |tape| flights(tape, Plant::None));
}

#[test]
fn control_a_second_flight_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::DoubleFlight, "single flight", flights);
}

#[test]
fn a_repeat_runs_nothing_for_one_read() {
    check(SimConfig::cases(200), |tape| {
        let world = World::draw(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        store.reads = 0;
        let (answer, again) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.runs.is_empty(), || format!("a repeat ran {:?}", again.runs))?;
        oracle(store.reads == 1 && answer.profile.gets == 1, || format!("a repeat read the store {} times", store.reads))?;
        oracle(answer.profile.certified, || String::from("a repeat did not stand on its certificate"))?;
        oracle(again.prints == first.prints, || String::from("a repeat printed other findings"))
    });
}

/// The tool's identity is in every key: a binary built from other code shares no record and no
/// certificate with the one before it (it runs every task again), and the same identity does.
#[test]
fn another_tool_shares_no_record_and_no_certificate() {
    check(SimConfig::cases(100), |tape| {
        let world = World::draw(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let held: BTreeSet<Key> = store.records.keys().copied().collect();
        let other = Setup { tool: b"another build", ..setup(tape, Plant::None) };
        let mut alone = Store::default();
        finished(request_with(tape, &world, None, &mut alone, None, Setup { tool: b"another build", ..setup(tape, Plant::None) })?)?;
        let shared = alone.records.keys().filter(|key| held.contains(key)).count();
        oracle(shared == 0, || format!("another tool's build wrote {shared} records under this tool's keys"))?;
        let (answer, again) = finished(request_with(tape, &world, Some(&world), &mut store, first.last.as_deref(), other)?)?;
        oracle(!answer.profile.certified, || String::from("another tool stood on this tool's certificate"))?;
        // It made every record its own fresh build makes (each under its own keys, none of this
        // tool's); a run's count may differ, `last`'s shapes saving a `Need` round.
        let missing = alone.records.keys().filter(|key| !store.records.contains_key(key)).count();
        oracle(missing == 0, || format!("another tool, given this tool's last answer, left {missing} of its records unmade ({:?})", again.runs))?;
        let (same, repeat) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(same.profile.certified && repeat.runs.is_empty(), || format!("the same tool ran {:?}", repeat.runs))
    });
}

#[test]
fn a_one_body_edit_runs_one_file_and_one_body() {
    check(SimConfig::cases(300), |tape| {
        let world = World::draw(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let next = edit(tape, &world, 0, true);
        store.reads = 0;
        let (answer, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        let ran = |kind| ended.runs.get(&kind).copied().unwrap_or(0);
        oracle(ran(Kind::FileItems) == 1 && ran(Kind::BodyCheck) == 1 && ended.runs.len() == 2, || {
            format!("a one-body edit ran {:?}", ended.runs)
        })?;
        // drive.md's count: certificate, old certificate, root, the pages, the file's key, the
        // body's key; the body's inputs stand on their rows.
        let pages = answer.profile.pages;
        oracle(store.reads <= 5 + pages, || format!("a one-body edit read the store {} times ({pages} pages)", store.reads))?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

#[test]
fn any_completion_order_writes_equal_bytes() {
    check(SimConfig::cases(200), |tape| {
        let world = World::draw(tape);
        let (one, _) = fresh(tape, &world)?;
        let (two, _) = fresh(tape, &world)?;
        oracle(one.records == two.records, || String::from("two orders of one build wrote other bytes"))
    });
}

#[test]
fn a_missing_record_of_a_textless_crate_is_the_typed_error() {
    check(SimConfig::cases(50), |tape| {
        let mut world = World::draw(tape);
        let dep = Key(key::digest("sim dep key", &tape.next_u64("dep").to_le_bytes()));
        world.dep = Some(dep);
        let mut store = Store::default();
        let ended = request(tape, &world, None, &mut store, None, Plant::None)?;
        match ended.error {
            Some(DriveError::Missing { name, key, builder: Builder::Pack { krate } }) => {
                oracle(name == dep_name() && key == dep && krate == Name::units(), || {
                    format!("the missing record named {name} {key} {krate}")
                })
            }
            other => Err(Broke::Oracle(format!("a missing dependency record ended as {other:?}"))),
        }
    });
}

/// Each member's finding is printed once, on its own manifest, saying its version.
fn members_print_on_their_manifests(world: &World, ended: &Ended) -> Result<(), Broke> {
    for (k, member) in world.members.iter().enumerate() {
        let message = format!("member {k} version {}", member.version).into_bytes();
        let on = ended.prints.iter().filter(|p| p.4 == message).collect::<StdVec<_>>();
        oracle(on.len() == 1 && on[0].5.as_deref() == Some(member_path(k).as_slice()) && on[0].0 == Name::manifest(&member_path(k)), || {
            format!("member {k}'s finding printed as {:?}", on.iter().map(|p| (p.0, p.5.clone())).collect::<StdVec<_>>())
        })?;
    }
    Ok(())
}

#[test]
fn a_member_manifest_edit_rereads_that_member_alone() {
    check(SimConfig::cases(300), |tape| {
        let mut world = World::draw(tape);
        world.members = (0..3).map(|k| Member { version: 0, files: (0..world.files.len()).filter(|f| f % 3 == k).collect() }).collect();
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        members_print_on_their_manifests(&world, &first)?;
        let mut next = world.clone();
        let k = tape.index(3, "edit member");
        next.members[k].version += 1;
        let (_, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        // The root units (the layout read moved) and that member's: no file, no other member.
        oracle(ended.runs.len() == 1 && ended.runs.get(&Kind::Units) == Some(&2), || {
            format!("an edit of member {k}'s manifest ran {:?}", ended.runs)
        })?;
        members_print_on_their_manifests(&next, &ended)?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

// --- The unit graph and its fetches -----------------------------------------------------------

/// A lock of up to eight entries, each depending only on later ones (as a real lock's graph is
/// acyclic), with the roots the tape picks; none failing.
fn draw_lock(tape: &Tape) -> Lock {
    let n = 1 + tape.index(8, "lock entries");
    let entries = (0..n).map(|at| (at + 1..n).filter(|_| tape.one_in(3, "depends")).collect()).collect();
    let roots = (0..n).filter(|_| tape.one_in(4, "a root")).collect();
    Lock { entries, roots, failing: StdVec::new() }
}

/// The entries the roots reach.
fn reach(lock: &Lock) -> BTreeSet<usize> {
    let mut seen: BTreeSet<usize> = lock.roots.iter().copied().collect();
    let mut frontier = lock.roots.clone();
    while let Some(at) = frontier.pop() {
        for dep in &lock.entries[at] {
            if seen.insert(*dep) {
                frontier.push(*dep);
            }
        }
    }
    seen
}

fn runs(ended: &Ended, kind: Kind) -> u32 {
    ended.runs.get(&kind).copied().unwrap_or(0)
}

/// A lock entry nothing reaches is never fetched: a lock with no roots (hello world's stale
/// lock, whose 177 entries are bstd's, which the toolchain gives) fetches nothing at all.
#[test]
fn a_lock_fetches_only_what_the_roots_reach() {
    check(SimConfig::cases(300), |tape| {
        let mut world = World::draw(tape);
        let lock = draw_lock(tape);
        let reached = reach(&lock);
        world.lock = Some(lock);
        let mut store = Store::default();
        let (answer, ended) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        oracle(answer.failed == 0, || format!("{} tasks failed", answer.failed))?;
        oracle(runs(&ended, Kind::Fetch) as usize == reached.len(), || {
            format!("{} Fetch runs for a reach of {} ({:?})", runs(&ended, Kind::Fetch), reached.len(), world.lock)
        })?;
        let mut none = world.clone();
        if let Some(lock) = none.lock.as_mut() {
            lock.roots.clear();
        }
        let (_, bare) = finished(request(tape, &none, None, &mut Store::default(), None, Plant::None)?)?;
        oracle(runs(&bare, Kind::Fetch) == 0 && runs(&bare, Kind::UnitGraph) == 1, || format!("a lock no root reaches ran {:?}", bare.runs))
    });
}

/// A member's manifest edit that makes one more entry reachable fetches exactly that one.
#[test]
fn an_edit_reaching_one_more_entry_fetches_one() {
    check(SimConfig::cases(300), |tape| {
        let mut world = World::draw(tape);
        world.lock = Some(draw_lock(tape));
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let mut next = world.clone();
        let Some(lock) = next.lock.as_mut() else { return Err(Broke::Harness(String::from("no lock"))) };
        let unreached: StdVec<usize> = (0..lock.entries.len()).filter(|at| !reach(lock).contains(at)).collect();
        // A root added: an unreached entry whose own dependencies are reached (or none), else a
        // new entry with none.
        let reached = reach(lock);
        let pick = unreached.iter().copied().find(|at| lock.entries[*at].iter().all(|dep| reached.contains(dep)));
        let added = match pick {
            Some(at) => at,
            None => {
                lock.entries.push(StdVec::new());
                lock.entries.len() - 1
            }
        };
        lock.roots.push(added);
        let (answer, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(answer.failed == 0 && runs(&ended, Kind::Fetch) == 1, || format!("reaching entry {added} ran {:?}", ended.runs))
    });
}

/// A `Fetch` that failed leaves no graph record: the graph task is told (`Task::failed`), asks
/// no more for it, and fails; the next request asks for the graph and that entry again.
#[test]
fn a_failed_fetch_leaves_no_graph_and_the_next_request_asks_again() {
    check(SimConfig::cases(300), |tape| {
        let mut world = World::draw(tape);
        let mut lock = draw_lock(tape);
        lock.roots.push(0);
        let reached: StdVec<usize> = reach(&lock).into_iter().collect();
        let failing = reached[tape.index(reached.len(), "failing")];
        lock.failing.push(failing);
        world.lock = Some(lock);
        let mut store = Store::default();
        let (answer, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        oracle(answer.failed >= 2 && runs(&first, Kind::UnitGraph) >= 1, || format!("a failed fetch ended with {} failed, ran {:?}", answer.failed, first.runs))?;
        let mut next = world.clone();
        if let Some(lock) = next.lock.as_mut() {
            lock.failing.clear();
        }
        let (again, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.failed == 0 && runs(&ended, Kind::UnitGraph) >= 1 && runs(&ended, Kind::Fetch) >= 1, || {
            format!("the request after a failed fetch ran {:?} with {} failed", ended.runs, again.failed)
        })
    });
}

// --- Many workers -----------------------------------------------------------------------------

#[test]
fn one_two_and_sixteen_workers_write_equal_bytes() {
    check(SimConfig::cases(200), |tape| {
        let world = World::draw(tape);
        let mut answers = StdVec::new();
        let opens = tape.one_in(2, "open the files");
        for workers in [1, 2, 16] {
            let mut store = Store::default();
            let setup = Setup { workers, ask: ASK, plant: Plant::None, tool: SIM_TOOL, opens };
            let (_, ended) = finished(request_with(tape, &world, None, &mut store, None, setup)?)?;
            answers.push((store.records, ended.prints));
        }
        oracle(answers[0] == answers[1] && answers[1] == answers[2], || {
            String::from("1, 2 and 16 workers wrote other records or printed other findings")
        })
    });
}

#[test]
fn control_a_batch_applied_twice_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::BatchTwice, "a batch result applied twice", flights);
}

#[test]
fn control_a_need_answered_before_its_record_is_written_is_caught() {
    control(SimConfig::cases(4000), Plant::None, Plant::PublishBeforePut, "a need answered before its record is written", edits);
}

// --- The gate ---------------------------------------------------------------------------------

/// `run` under `Beside` or `Before`: no success while an error exists or could still be found.
fn gates(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let gate = [Gate::Beside, Gate::Before][tape.index(2, "gate")];
    let mut store = Store::default();
    let setup = Setup {
        workers: workers(tape),
        ask: Ask { command: Command::Run, args: b"", gate, space: Space::NONE },
        plant,
        tool: SIM_TOOL,
        opens: tape.one_in(2, "open the files"),
    };
    let (answer, ended) = finished(request_with(tape, &world, None, &mut store, None, setup)?)?;
    let (_, checked) = fresh(tape, &world)?;
    let errors = checked.prints.len();
    match answer.verdict {
        Verdict::Passed | Verdict::ProgramFailed => {
            oracle(errors == 0, || format!("a {:?} verdict over {errors} errors", answer.verdict))
        }
        Verdict::Failed { started, .. } => {
            oracle(errors > 0, || String::from("a failed verdict with no error"))?;
            oracle(gate == Gate::Beside || !started, || String::from("`Before` started a program over errors"))?;
            oracle(started == ended.started, || String::from("the verdict's `started` is not what happened"))
        }
        other => Err(Broke::Oracle(format!("`run` answered {other:?}"))),
    }
}

#[test]
fn no_success_is_reported_while_an_error_could_be_found() {
    check(SimConfig::cases(300), |tape| gates(tape, Plant::None));
}

#[test]
fn control_a_success_before_the_gate_is_done_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::EarlySuccess, "could still find an error", gates);
}

/// Commands and edits in any order on one store, each walking forward from the one before, as
/// a person types them: `check`, `run` and `test` (each its own certificate), a body or a
/// signature edited between. Every request ends answered or with a typed error, never with a
/// step after its answer (`blaze run` after `blaze check`, 2026-11-16: the walk stood on the
/// check's error, stopped the program it had started and answered before its exit).
///
/// The one typed error allowed is `LateWant` (drive.md, "The gate"): after a forward walk or a
/// certificate, a stood item is settled by the answer but not walked, and a want of it once
/// the walk is done is refused. Owed: answer it from the answer's own rows; until then the
/// real program wants nothing (`Code` is not built).
fn commands(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    let mut before: Option<World> = None;
    let mut store = Store::default();
    let mut last: Option<StdVec<u8>> = None;
    for serial in 0..2 + tape.below(5, "steps") {
        if before.is_some() && tape.one_in(3, "an edit") {
            world = edit(tape, &world, serial, tape.one_in(2, "body or signature"));
            continue;
        }
        let command = [Command::Check, Command::Run, Command::Test][tape.index(3, "command")];
        let setup = Setup { ask: Ask { command, args: b"", gate: Gate::Beside, space: Space::NONE }, ..setup(tape, plant) };
        let ended = request_with(tape, &world, before.as_ref(), &mut store, last.as_deref(), setup)?;
        match &ended.error {
            Some(DriveError::LateWant { .. }) => return Ok(()),
            Some(error) => return Err(Broke::Oracle(format!("`{command:?}` stopped with an error: {error}"))),
            None => {}
        }
        let (answer, ended) = finished(ended)?;
        // `test` is its check alone until tests run on this path: it starts no program (a
        // package's `main` is not its tests) and never answers as a program's pass or failure.
        if command == Command::Test {
            oracle(!ended.started, || String::from("`test` started a program: `main` is not a test"))?;
            oracle(matches!(answer.verdict, Verdict::Checked | Verdict::Failed { started: false, .. }), || {
                format!("`test` answered {:?}: its answer is its check's alone", answer.verdict)
            })?;
        }
        if ended.last.is_some() {
            last = ended.last;
        }
        before = Some(world.clone());
    }
    Ok(())
}

/// `test` starting the package's program (what `blaze test` did until 2026-11-16: the check,
/// then `src/main.rs` run as if `main` were the tests) is caught by the commands property.
#[test]
fn control_a_test_that_starts_a_program_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::TestStartsProgram, "`test` started a program", commands);
}

#[test]
fn any_sequence_of_commands_and_edits_answers() {
    check(SimConfig::cases(300), |tape| commands(tape, Plant::None));
}

#[test]
fn control_an_answer_while_the_program_runs_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::AnswerWhileRunning, "answered while the program", commands);
}

// --- Linear bookkeeping (parallel.md change 3) ------------------------------------------------

/// `files` files of `items` items: each signature uses the one before it in its file (a chain
/// of `Need` rounds), each body calls two signatures elsewhere.
fn wide(files: usize, items: usize) -> World {
    let world = (0..files)
        .map(|f| {
            (0..items)
                .map(|i| Item {
                    sig: (f * items + i) as u64 % 4,
                    body: 0,
                    uses: (i > 0).then(|| (f, i - 1)),
                    calls: (1..=2).map(|k| ((f * 7 + k) % files, (i * 3 + k) % items)).collect(),
                    cfg: StdVec::new(),
                })
                .collect()
        })
        .collect();
    World::new(world)
}

/// What a first-in-first-out run of one cold `check` saw.
struct Fifo {
    /// Records made.
    jobs: u64,
    /// Parts listed plus reads.
    edges: u64,
    /// The machine's own count.
    bookkeeping: u64,
    /// Each finding printed: its item, its condition, its message.
    prints: StdVec<(Name, Condition, StdVec<u8>)>,
}

fn counted(world: &World) -> Result<(u64, u64, u64), Broke> {
    let run = fifo(world, ASK)?;
    Ok((run.jobs, run.edges, run.bookkeeping))
}

/// One cold `check` of `world` under `ask`, its effects answered first in, first out.
fn fifo(world: &World, ask: Ask<'_>) -> Result<Fifo, Broke> {
    let ids: StdVec<ContentId> = (0..world.files.len()).map(|f| world.id(f)).collect();
    let paths: StdVec<StdVec<u8>> = (0..world.files.len()).map(path).collect();
    let mut files: StdVec<FileEntry<'_>> = (0..world.files.len())
        .map(|f| FileEntry { path: &paths[f], id: ids[f], changed: true, manifest: false })
        .collect();
    let manifest = b"Cargo.toml".to_vec();
    files.push(FileEntry { path: &manifest, id: key::digest("sim manifest", b"[package]"), changed: false, manifest: true });
    files.sort_by(|a, b| a.path.cmp(b.path));
    let mut req = Request::planted_in(config(16, SIM_TOOL), ask, world.plant, Alloc::global());
    let mut store: BTreeMap<Key, StdVec<u8>> = BTreeMap::new();
    let mut todo: std::collections::VecDeque<Effect<'static>> = std::collections::VecDeque::new();
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(Alloc::global());
    let (mut jobs, mut edges) = (0u64, 0u64);
    let mut prints = StdVec::new();
    let units = units_source(&files);
    req.step(Event::Start { files: &files, gone: &[], last: None, units, cold: false, graph: None }, &mut out).map_err(|e| Broke::Oracle(format!("{e}")))?;
    loop {
        todo.extend(out.drain(..));
        let effect = todo.pop_front().ok_or(Broke::Oracle(String::from("the request stalled")))?;
        let stepped = match effect {
            Effect::Get { key } => {
                let bytes = store.get(&key).cloned();
                req.step(Event::Got { key, bytes: bytes.as_deref() }, &mut out)
            }
            Effect::Put { key, bytes } => {
                store.insert(key, bytes.as_slice().to_vec());
                continue;
            }
            Effect::Run { batch } => {
                let mut answered = StdVec::new();
                for (ticket, task) in batch.tasks.iter() {
                    answered.push((*ticket, execute(world, task)?));
                }
                let made: StdVec<&Made> = answered
                    .iter()
                    .filter_map(|(_, a)| match a {
                        Answered::Done(made) => Some(made),
                        Answered::Need(_) | Answered::Cfg(_) | Answered::Failed(_) => None,
                    })
                    .collect();
                let held: StdVec<StdVec<Finding<'_>>> = made
                    .iter()
                    .map(|m| m.findings.iter().map(|(rule, start, end, message)| Finding { rule: *rule, start: *start, end: *end, message }).collect())
                    .collect();
                let outputs: StdVec<Output<'_>> = made
                    .iter()
                    .zip(held.iter())
                    .map(|(m, findings)| Output { name: m.name, out: m.out, reads: &m.reads, parts: &m.parts, paths: &[], findings, payload: b"" })
                    .collect();
                let mut at = 0;
                let mut results = StdVec::new();
                for (ticket, answer) in &answered {
                    let ran = match answer {
                        Answered::Need(wants) => Ran::Need(wants),
                        Answered::Cfg(var) => Ran::Cfg(*var),
                        Answered::Failed(failure) => Ran::Failed(*failure),
                        Answered::Done(m) => {
                            jobs += 1;
                            edges += (m.parts.len() + m.reads.len()) as u64;
                            at += 1;
                            Ran::Done(&outputs[at - 1..at])
                        }
                    };
                    results.push(Outcome { ticket: *ticket, ran, in_place: 0, ns: 1_000 });
                }
                req.step(Event::Ran { batch: batch.id, results: &results }, &mut out)
            }
            Effect::Print(print) => {
                prints.push((print.name, print.condition, print.message.as_slice().to_vec()));
                continue;
            }
            Effect::Done(_) => return Ok(Fifo { jobs, edges, bookkeeping: req.bookkeeping(), prints }),
            _ => continue,
        };
        stepped.map_err(|e| Broke::Oracle(format!("{e}")))?;
    }
}

/// The machine's bookkeeping per job and edge stays flat from 2,000 jobs to 100,000: no step
/// costs in proportion to the jobs (parallel.md change 3). A count, not a time.
#[test]
fn bookkeeping_is_linear_in_jobs_and_edges() {
    check(SimConfig::cases(1), |tape| {
        let small_files = 100 + tape.index(20, "small world's files");
        let (small_jobs, small_edges, small) = counted(&wide(small_files, 10))?;
        let (large_jobs, large_edges, large) = counted(&wide(1_000, 50))?;
        oracle(large_jobs >= 100_000, || format!("the large world made {large_jobs} jobs, not 100,000"))?;
        let per_small = small as f64 / (small_jobs + small_edges) as f64;
        let per_large = large as f64 / (large_jobs + large_edges) as f64;
        oracle(per_large <= 2.0 * per_small && per_large <= 64.0, || {
            format!(
                "bookkeeping per job and edge grew from {per_small:.1} ({small_jobs} jobs) to {per_large:.1} ({large_jobs} jobs)"
            )
        })
    });
}

// --- Conditions through the request machine (drive.md, "One check covers every configuration") --

/// A toy checkout whose items read cfg variables: one to three variables of two or three values,
/// each item reading up to two of them; every body a finding (its value compared whole).
fn draw_cfg(tape: &Tape, plant: Plant) -> World {
    let mut world = World::draw(tape);
    let vars = 1 + tape.index(3, "cfg variables");
    world.values = (0..vars).map(|_| 2 + tape.index(2, "values") as u16).collect();
    for file in world.files.iter_mut() {
        for item in file.iter_mut() {
            item.cfg = (0..tape.index(3, "cfg reads")).map(|_| tape.index(vars, "cfg variable") as Var).collect();
        }
    }
    world.find_all = true;
    world.plant = plant;
    world
}

/// One full configuration directly: an item's signature, then its body, by the toy semantics.
fn eval_sig(world: &World, f: usize, i: usize, config: &[u16]) -> ContentId {
    let item = &world.files[f][i];
    let mut reads: StdVec<(Name, ContentId)> =
        item.uses.iter().map(|(uf, ui)| (sig_name(*uf, *ui), eval_sig(world, *uf, *ui, config))).collect();
    reads.sort();
    reads.dedup();
    let values: StdVec<u16> = item.cfg.iter().map(|var| config[*var as usize]).collect();
    item_out(item.sig, &reads, &values)
}

fn eval_body(world: &World, f: usize, i: usize, config: &[u16]) -> ContentId {
    let item = &world.files[f][i];
    let mut reads: StdVec<(Name, ContentId)> =
        item.calls.iter().map(|(cf, ci)| (sig_name(*cf, *ci), eval_sig(world, *cf, *ci, config))).collect();
    reads.sort();
    reads.dedup();
    let values: StdVec<u16> = item.cfg.iter().map(|var| config[*var as usize]).collect();
    item_out(item.body, &reads, &values)
}

/// The oracle (bottom.md 11): checking once under the whole space, forking on each unfixed read
/// and storing each record under what it read, prints exactly the findings of checking every
/// full configuration on its own: for each configuration, the findings whose condition it
/// implies are its findings, each once.
fn whole_space_equals_each(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = draw_cfg(tape, plant);
    let space = Space { values: &world.values, implications: &[] };
    let ask = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space };
    let run = fifo(&world, ask)?;
    let mut configs: StdVec<StdVec<u16>> = std::vec![StdVec::new()];
    for count in &world.values {
        configs = configs.into_iter().flat_map(|c| (0..*count).map(move |v| [c.as_slice(), &[v]].concat())).collect();
    }
    for config in &configs {
        let mut full = Condition::ANY;
        for (var, value) in config.iter().enumerate() {
            full = full.with(var as Var, *value).ok().flatten().ok_or(Broke::Harness(String::from("a full configuration")))?;
        }
        let mut each: StdVec<(Name, StdVec<u8>)> = StdVec::new();
        for f in 0..world.files.len() {
            // A file's own finding holds under every configuration (`execute`'s FileItems).
            if world.files[f].len() % 2 == 0 {
                each.push((file_name(f), format!("file {f}").into_bytes()));
            }
            for i in 0..world.files[f].len() {
                if let Some(message) = finding(&world, Kind::BodyCheck, &eval_body(&world, f, i, config)) {
                    each.push((body_name(f, i), message));
                }
            }
        }
        let mut once: StdVec<(Name, StdVec<u8>)> = run
            .prints
            .iter()
            .filter(|(_, condition, _)| full.implies(condition))
            .map(|(name, _, message)| (*name, message.clone()))
            .collect();
        each.sort();
        once.sort();
        oracle(each == once, || {
            format!(
                "under {config:?} the whole space's check printed {} findings, the configuration's own {}: they differ",
                once.len(),
                each.len()
            )
        })?;
    }
    Ok(())
}

#[test]
fn the_whole_space_equals_each_configuration_through_the_request() {
    check(SimConfig::cases(300), |tape| whole_space_equals_each(tape, Plant::None));
}

#[test]
fn control_an_asker_served_a_fork_it_does_not_imply_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::RanUnderCondition, "they differ", whole_space_equals_each);
}

/// An item reading three variables of five or six values spans 125 to 216 configurations, past
/// the cap of 64: its answer is the typed finding, not a sample.
#[test]
fn an_item_past_the_cap_is_a_finding() {
    check(SimConfig::cases(20), |tape| {
        let mut world = wide(1, 1);
        world.values = (0..3).map(|_| 5 + tape.index(2, "values past the cap") as u16).collect();
        world.files[0][0].cfg = std::vec![0, 1, 2];
        let space = Space { values: &world.values, implications: &[] };
        let run = fifo(&world, Ask { command: Command::Check, args: b"", gate: Gate::Beside, space })?;
        oracle(run.prints.iter().any(|(_, _, message)| message.windows(7).any(|w| w == b"past 64")), || {
            format!("no over-cap finding among {} printed", run.prints.len())
        })
    });
}

// --- The names step: `CrateScope` and `MacroModule` (drive.md, "SPEC, the NAMES step") -------

fn names_world(tape: &Tape) -> World {
    let mut world = World::draw(tape);
    draw_crates(tape, &mut world);
    world
}

/// The crates whose `CrateScope` reruns when `changed`'s scope moves: each direct dependent of
/// a moved scope reruns, and its own scope moves only where it glob re-exports a moved one.
fn reruns_after(world: &World, changed: usize) -> BTreeSet<usize> {
    let mut moved: BTreeSet<usize> = [changed].into_iter().collect();
    let mut rerun = moved.clone();
    for (c, krate) in world.crates.iter().enumerate().skip(changed + 1) {
        if krate.deps.iter().any(|d| moved.contains(d)) {
            rerun.insert(c);
        }
        if krate.globs.iter().any(|d| moved.contains(d)) {
            moved.insert(c);
        }
    }
    rerun
}

/// Every crate's scope is made once, after its dependencies' (a `Need` re-enters it), in any
/// completion order with equal bytes; each proc-macro crate's module is made once for all its
/// callers; a repeat runs nothing.
#[test]
fn crate_scopes_resolve_once_in_dependency_order_in_any_completion_order() {
    check(SimConfig::cases(200), |tape| {
        let world = names_world(tape);
        let (one, first) = fresh(tape, &world)?;
        let (two, _) = fresh(tape, &world)?;
        oracle(one.records == two.records, || String::from("two completion orders of the names step wrote other bytes"))?;
        // A run is an entry: a crate is entered once, or twice when it asks all its needs at
        // once and is re-entered with them; never more.
        let scopes = runs(&first, Kind::CrateScope) as usize;
        oracle((world.crates.len()..=2 * world.crates.len()).contains(&scopes) && runs(&first, Kind::MacroModule) as usize == world.macros.len(), || {
            format!("{} crates and {} macro crates ran {:?}", world.crates.len(), world.macros.len(), first.runs)
        })?;
        let mut store = one;
        let (_, again) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.runs.is_empty(), || format!("a repeat ran {:?}", again.runs))
    });
}

/// A body edit leaves every surface, so no `CrateScope` and no `MacroModule` runs.
#[test]
fn a_body_edit_runs_no_crate_scope() {
    check(SimConfig::cases(200), |tape| {
        let world = names_world(tape);
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let next = edit(tape, &world, 0, true);
        let (_, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(runs(&ended, Kind::CrateScope) == 0 && runs(&ended, Kind::Surface) == 0 && runs(&ended, Kind::MacroModule) == 0, || {
            format!("a body edit ran {:?}", ended.runs)
        })?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

/// A crate and its twin (a library and its test unit: the same files under another identity,
/// each its own `CrateScope`) hold the same finding, and a third crate one of other words. The
/// answer prints the twins' finding once and counts it once, made fresh and stood on a repeat
/// (the certificate's findings); the other finding is untouched.
fn twins(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = names_world(tape);
    let words = b"no `nope` in `crate`".to_vec();
    world.crates[0].says = Some(words.clone());
    let twin = SimCrate { identity: 9_000, ..world.crates[0].clone() };
    let other = SimCrate { identity: 9_001, says: Some(b"no `gone` in `crate`".to_vec()), ..world.crates[0].clone() };
    world.crates.push(twin);
    world.crates.push(other);
    let mut store = Store::default();
    let (answer, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let once = |ended: &Ended, answer: &Answer, when: &str| {
        let twins = ended.prints.iter().filter(|p| p.4 == words).count();
        let others = ended.prints.iter().filter(|p| p.1 == TWIN_RULE).count() - twins;
        oracle(twins == 1 && others == 1, || format!("{when}: the twins' finding printed {twins} times, the other crate's {others}"))?;
        oracle(answer.findings as usize == ended.prints.len(), || format!("{when}: {} errors counted over {} printed", answer.findings, ended.prints.len()))
    };
    once(&first, &answer, "fresh")?;
    let (answer, again) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), plant)?)?;
    oracle(again.runs.is_empty(), || format!("a repeat ran {:?}", again.runs))?;
    once(&again, &answer, "a repeat")
}

#[test]
fn one_finding_held_by_two_names_prints_and_counts_once() {
    check(SimConfig::cases(200), |tape| twins(tape, Plant::None));
}

#[test]
fn control_a_finding_printed_once_a_name_is_caught() {
    control(SimConfig::cases(200), Plant::None, Plant::PrintEachName, "the twins' finding printed 2 times", twins);
}

/// An added `pub fn` in a crate reruns that crate's scope and its direct dependents' (their
/// read of its scope fails), and past them only through a glob re-export: the unit of rerun is
/// the whole dependent crate.
#[test]
fn an_added_pub_fn_reruns_its_crate_and_the_dependents_reading_it() {
    check(SimConfig::cases(200), |tape| {
        let world = names_world(tape);
        let f = tape.index(world.files.len(), "edited file");
        let c = f % world.crates.len();
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let mut files = world.files.clone();
        files[f].push(Item { sig: 9_000, body: 0, uses: None, calls: StdVec::new(), cfg: StdVec::new() });
        let mut next = World::new(files);
        next.crates = world.crates.clone();
        next.macros = world.macros.clone();
        let (answer, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        let expected = reruns_after(&world, c);
        oracle(answer.failed == 0 && runs(&ended, Kind::CrateScope) as usize == expected.len() && runs(&ended, Kind::Surface) == 1, || {
            format!("a pub fn in crate {c} (file {f}) ran {:?}, expected {} CrateScope ({expected:?}) of {:?}", ended.runs, expected.len(), world.crates)
        })?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

/// A module store without the macro crates' modules: each `MacroModule` fails once (one
/// finding a macro crate) and keeps no record, so the command after the store is filled looks
/// each up exactly once, and a repeat after that looks none up.
#[test]
fn an_absent_module_is_asked_again_and_a_present_one_never() {
    check(SimConfig::cases(200), |tape| {
        let mut world = names_world(tape);
        if world.macros.is_empty() {
            return Ok(());
        }
        world.modules_absent = true;
        let mut store = Store::default();
        let (answer, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        let looked = runs(&first, Kind::MacroModule) as usize;
        oracle(looked == world.macros.len() && answer.failed as usize >= world.macros.len(), || {
            format!("with no modules, {} macro crates ran {looked} lookups and {} failed", world.macros.len(), answer.failed)
        })?;
        let mut filled = world.clone();
        filled.modules_absent = false;
        let (again, second) = finished(request(tape, &filled, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.failed == 0 && runs(&second, Kind::MacroModule) as usize == world.macros.len(), || {
            format!("after the store was filled: {} failed, ran {:?}", again.failed, second.runs)
        })?;
        let (_, third) = finished(request(tape, &filled, Some(&filled), &mut store, second.last.as_deref(), Plant::None)?)?;
        oracle(runs(&third, Kind::MacroModule) == 0, || format!("a repeat looked modules up: {:?}", third.runs))
    });
}

/// A file whose own task fails (a planted panic) fails alone: its surface, which a crate's
/// `CrateScope` wants, settles failed `Waited` on it, and so does each crate reading it; the
/// request answers (no `NoPart`), and the next command, with the file fixed, matches a fresh
/// build.
#[test]
fn a_failed_file_fails_its_surface_and_its_readers_not_the_request() {
    check(SimConfig::cases(200), |tape| {
        let mut world = names_world(tape);
        if world.files.is_empty() {
            return Ok(());
        }
        world.failing_file = Some(0);
        // File 0 is crate 0's (`draw_crates`): it and each crate reaching it fail.
        let mut reaching: BTreeSet<usize> = [0].into_iter().collect();
        for (c, krate) in world.crates.iter().enumerate().skip(1) {
            if krate.deps.iter().any(|d| reaching.contains(d)) {
                reaching.insert(c);
            }
        }
        let mut store = Store::default();
        let (answer, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        // The file, its surface, and the crates; items using the file's signatures fail too.
        let expected = 2 + reaching.len();
        oracle(answer.failed as usize >= expected, || format!("{} failed, expected at least {expected} (crates {reaching:?})", answer.failed))?;
        let mut fixed = world.clone();
        fixed.failing_file = None;
        let (again, ended) = finished(request(tape, &fixed, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        oracle(again.failed == 0, || format!("after the fix {} failed", again.failed))?;
        same_as_fresh(tape, &fixed, &store, &ended)
    });
}

/// A crate the names library refuses fails alone; each crate reaching it fails `Waited` and
/// keeps no record, so the next command, with the crate fixed, makes all of them.
#[test]
fn a_failed_crate_leaves_its_dependents_without_a_record() {
    check(SimConfig::cases(200), |tape| {
        let mut world = names_world(tape);
        if world.crates.len() < 2 {
            return Ok(());
        }
        world.crates[1].deps = std::vec![0];
        world.crates[0].fails = true;
        let mut reaching: BTreeSet<usize> = [0].into_iter().collect();
        for (c, krate) in world.crates.iter().enumerate().skip(1) {
            if krate.deps.iter().any(|d| reaching.contains(d)) {
                reaching.insert(c);
            }
        }
        let mut store = Store::default();
        let (answer, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
        oracle(answer.failed as usize == reaching.len(), || format!("{} failed, expected {} ({reaching:?})", answer.failed, reaching.len()))?;
        let mut fixed = world.clone();
        fixed.crates[0].fails = false;
        let (again, ended) = finished(request(tape, &fixed, Some(&world), &mut store, first.last.as_deref(), Plant::None)?)?;
        let scopes = runs(&ended, Kind::CrateScope) as usize;
        // The failed crates kept nothing, so each runs again (a crate the failed command never
        // reached runs now too: at most two entries a crate in all).
        oracle(again.failed == 0 && (reaching.len()..=2 * world.crates.len()).contains(&scopes), || {
            format!("after the fix ran {:?}, expected {} to {} CrateScope entries", ended.runs, reaching.len(), 2 * world.crates.len())
        })?;
        same_as_fresh(tape, &fixed, &store, &ended)
    });
}

// --- One pointer a (tool, shape) (pointers.rs) -------------------------------------------------

use crate::pointers::{self, PointerName};

/// The checkout's pointer folder as blaze-cli keeps it: a write removes what
/// `pointers::evicted` names; the stamp is the count of writes (a file's modified time).
#[derive(Default)]
struct Pointers {
    held: BTreeMap<PointerName, (u64, StdVec<u8>)>,
    writes: u64,
}

impl Pointers {
    fn write(&mut self, name: PointerName, bytes: StdVec<u8>, plant: pointers::Plant) {
        let mut held: StdVec<(PointerName, u64)> = self.held.iter().map(|(name, (at, _))| (*name, *at)).collect();
        let mut gone = Vec::new_in(Alloc::global());
        pointers::evicted(&mut held, name, plant, &mut gone);
        for name in gone.iter() {
            self.held.remove(name);
        }
        self.writes += 1;
        self.held.insert(name, (self.writes, bytes));
    }
}

/// One command of the build `tool`: it reads its own pointer, answers and writes it. A command
/// with no pointer has no leaf table either (they are one file): every file is new to it.
fn command(
    tape: &Tape,
    world: &World,
    before: Option<&World>,
    store: &mut Store,
    kept: &mut Pointers,
    (tool, ask): (&'static [u8], Ask<'static>),
    plant: pointers::Plant,
) -> Result<(Answer, Ended), Broke> {
    let name = PointerName::planted(config(1, tool).tool, &crate::request::shape_of(&ask), plant);
    let last = kept.held.get(&name).map(|(_, bytes)| bytes.clone());
    let before = if last.is_some() { before } else { None };
    let (answer, ended) = finished(request_with(tape, world, before, store, last.as_deref(), Setup { tool, ask, ..setup(tape, Plant::None) })?)?;
    if let Some(bytes) = &ended.last {
        kept.write(name, bytes.clone(), plant);
    }
    Ok((answer, ended))
}

/// read-addressed.md, piece 1: tool A, another (a second build, or A's command of another
/// shape), an edit, tool A: A walks forward from its own last answer and runs the one file and
/// the one body, never from the units.
fn own_pointer(tape: &Tape, plant: pointers::Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    let mut kept = Pointers::default();
    let other: (&'static [u8], Ask<'static>) = match tape.one_in(2, "the other is a shape") {
        true => (SIM_TOOL, Ask { args: b"--another-shape", ..ASK }),
        false => (b"another build", ASK),
    };
    command(tape, &world, None, &mut store, &mut kept, (SIM_TOOL, ASK), plant)?;
    command(tape, &world, Some(&world), &mut store, &mut kept, other, plant)?;
    let next = edit(tape, &world, 0, true);
    let (answer, ended) = command(tape, &next, Some(&world), &mut store, &mut kept, (SIM_TOOL, ASK), plant)?;
    oracle(answer.profile.forward, || format!("after another command, the first walked from the units: ran {:?}", ended.runs))?;
    let ran = |kind| ended.runs.get(&kind).copied().unwrap_or(0);
    oracle(ran(Kind::FileItems) == 1 && ran(Kind::BodyCheck) == 1 && ended.runs.len() == 2, || format!("a one-body edit after another command ran {:?}", ended.runs))
}

#[test]
fn each_tool_and_shape_walks_forward_from_its_own_pointer() {
    check(SimConfig::cases(200), |tape| own_pointer(tape, pointers::Plant::None));
}

#[test]
fn control_one_pointer_for_every_tool_and_shape_is_caught() {
    control(SimConfig::cases(200), pointers::Plant::None, pointers::Plant::OneName, "the first walked from the units", own_pointer);
}

const BUILDS: [&[u8]; 10] = [b"b0", b"b1", b"b2", b"b3", b"b4", b"b5", b"b6", b"b7", b"b8", b"b9"];

/// More builds than `pointers::KEPT` in one checkout: the folder holds the `KEPT` written
/// last; the newest repeats on its certificate; one whose pointer went answers again in full,
/// with the findings it printed before.
fn kept_pointers(tape: &Tape, plant: pointers::Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    let mut kept = Pointers::default();
    let mut first = None;
    for tool in BUILDS {
        let (_, ended) = command(tape, &world, Some(&world), &mut store, &mut kept, (tool, ASK), plant)?;
        first.get_or_insert(ended);
    }
    let shape = crate::request::shape_of(&ASK);
    let want: BTreeSet<PointerName> = BUILDS[BUILDS.len() - pointers::KEPT..].iter().map(|tool| PointerName::of(config(1, tool).tool, &shape)).collect();
    let held: BTreeSet<PointerName> = kept.held.keys().copied().collect();
    oracle(held == want, || format!("the folder holds {} pointers, {} of the {} written last", held.len(), held.intersection(&want).count(), pointers::KEPT))?;
    let (newest, repeat) = command(tape, &world, Some(&world), &mut store, &mut kept, (BUILDS[BUILDS.len() - 1], ASK), plant)?;
    oracle(newest.profile.certified && repeat.runs.is_empty(), || format!("the newest build's repeat ran {:?}", repeat.runs))?;
    let (oldest, again) = command(tape, &world, Some(&world), &mut store, &mut kept, (BUILDS[0], ASK), plant)?;
    let first = first.expect("ten builds ran");
    oracle(!oldest.profile.forward && again.prints == first.prints, || String::from("a build whose pointer went printed other findings, or walked forward from no pointer"))
}

#[test]
fn a_checkout_keeps_the_pointers_written_last() {
    check(SimConfig::cases(60), |tape| kept_pointers(tape, pointers::Plant::None));
}

#[test]
fn control_evicting_the_newest_pointer_is_caught() {
    control(SimConfig::cases(60), pointers::Plant::None, pointers::Plant::EvictNewest, "written last", kept_pointers);
}
