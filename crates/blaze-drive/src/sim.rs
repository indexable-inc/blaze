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
use crate::answers::{PageView, read_last, read_root};
use crate::condition::{Condition, Space, Var};
use crate::key::{self, Key, Kind, Name, Tool};
use crate::record::{Finding, Locate, Part, PathOf};
use crate::request::{
    Answer, Ask, BatchId, Builder, Command, Config, DriveError, Effect, Event, FileEntry, Gate, Outcome, Output, Ran,
    Request, Task, TaskFailure, Ticket, Verdict, Want, Why,
};
use crate::settled::Settled;
use crate::worker::{self, InPlace};

/// `test` runs its tests after its check (`Effect::StartTests`), over this harness.
mod tests_start;

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
    /// The members it depends on: its record lists their `Units` (run_scope.rs).
    deps: StdVec<usize>,
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
    key::digest("sim member manifest", format!("{} {:?}", member.version, member.deps).as_bytes())
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
    /// A file edited after the sweep and before its `FileItems` task reads it: the task reads
    /// the edited bytes, while its key holds the swept id.
    edited_at_run: Option<usize>,
    /// The control's plant handed to `verified::verify` by the fake executor.
    verify_plant: Plant,
    /// Some bodies' checks stop short: each holds a [`NOT_CHECKED_RULE`] finding, no error, as
    /// blaze-cli's `types-not-checked` (the gate cases).
    unchecked: bool,
    /// The executor's own plant for the read-list case ([`ReadBug`]).
    read_bug: ReadBug,
    /// No manifest and no file holds a finding: only bodies do (the scope cases).
    quiet: bool,
    /// `run` is given member 0 as its program package ([`Request::with_member_closure`]).
    closure: bool,
    /// The files whose bodies the running program may want; `None`: any file's.
    wants_from: Option<StdVec<usize>>,
    /// As the world a request walks from: the files its answer did not read (a narrower `run`'s
    /// closure left them out). The cli's leaf table holds only what the answer read, so each is
    /// handed as changed, as a new file is.
    unread: BTreeSet<usize>,
    /// A body resolves each call through its callee file's `Surface` first, and reads the
    /// callee's signature only when the file still holds it ([`resolved_body`]): which names it
    /// reads follows from what it read, under one key.
    resolve_calls: bool,
    /// The executor's planted bug for the resolving body ([`NamesBug`]).
    names_bug: NamesBug,
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
            edited_at_run: None,
            verify_plant: Plant::None,
            unchecked: false,
            read_bug: ReadBug::None,
            quiet: false,
            closure: false,
            unread: BTreeSet::new(),
            wants_from: None,
            resolve_calls: false,
            names_bug: NamesBug::None,
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
            // Each member it depends on, as the root lists it.
            parts.extend(member.deps.iter().map(|d| Part { name: Name::manifest(&member_path(*d)), locate: Locate::File { edition: 0 }, place: 0 }));
            parts.sort_by_key(|part| part.name);
            let out = key::digest("sim member out", format!("{k} {} {:?}", member.version, member.deps).as_bytes());
            let line = u32::try_from(k).unwrap_or(u32::MAX);
            let findings = match world.quiet {
                true => StdVec::new(),
                false => std::vec![(2, line, line + 1, format!("member {k} version {}", member.version).into_bytes())],
            };
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
            // The task reads its file now; an edit after the sweep shows here, and the bytes
            // are compared with the id the key was made from before anything is parsed.
            let mut read = world.bytes(f);
            if world.edited_at_run == Some(f) {
                read.extend_from_slice(b" edited after the sweep");
            }
            if let Err(failure) = crate::verified::verify(name, &read, world.id(f), |bytes| key::digest("sim file", bytes), world.verify_plant) {
                return Ok(Answered::Failed(failure));
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
                0 if !world.quiet => std::vec![(1, 1, 2, format!("file {f}").into_bytes())],
                _ => StdVec::new(),
            };
            done(out, StdVec::new(), parts, findings)
        }
        Kind::BodyCheck if world.resolve_calls => resolved_body(world, task),
        Kind::Signature | Kind::BodyCheck => {
            // No file holds the item: the machine ran a name no home lists (one dropped and
            // entered again, [`removals`]), its bug, not the harness's.
            let (f, i) = world.item_of(task.base).ok_or(Broke::Oracle(format!("{name} ran, and no file holds its item: a dropped name entered again")))?;
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
                    let mut findings = match finding(world, name.kind, &out) {
                        Some(message) => std::vec![(7, 0, 1, message)],
                        None => StdVec::new(),
                    };
                    // A body whose check stopped short says so (about 3 in 8 of them).
                    if world.unchecked && name.kind == Kind::BodyCheck && out.0[1] < 96 {
                        findings.push((NOT_CHECKED_RULE, 1, 2, b"not checked: the sim's checker stopped short".to_vec()));
                    }
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
        // The program's image (sim_natives.rs simulates what it reads): here it reads nothing,
        // its out its source, so the gate cases start the program from its key.
        Kind::Native if name == sim_native() => done(task.source, StdVec::new(), StdVec::new(), StdVec::new()),
        kind => Err(Broke::Harness(format!("the sim has no {} task", kind.name()))),
    }
}

/// The program `run` names in the gate cases: its image is a `Native` task of no reads.
fn sim_native() -> Name {
    Name::native(&key::digest("sim crate key", b"0"), b"aarch64-apple-darwin", b"dev")
}

// --- One request, driven ----------------------------------------------------------------------

struct Ended {
    answer: Option<Answer>,
    error: Option<DriveError>,
    /// Runs emitted, by kind (in-place re-entries not counted: the coordinator never saw them).
    runs: BTreeMap<Kind, u32>,
    /// Each task run, by name, in the order emitted.
    ran: StdVec<Name>,
    prints: StdVec<(Name, u16, u32, u32, StdVec<u8>, Option<StdVec<u8>>, u32)>,
    last: Option<StdVec<u8>>,
    /// Records handed back in `Done` outcomes.
    delivered: u32,
    /// Needs a worker answered from the settled table.
    in_place: u32,
    /// The program: started, stopped, and whether it is still running.
    started: bool,
    stopped: bool,
    /// `test`'s tests were started (`Effect::StartTests`), and how they ended when they did.
    tested: bool,
    tests_ok: Option<bool>,
    /// The machine's own count at the answer ([`Request::bookkeeping`]).
    bookkeeping: u64,
}

/// One request's setup: the workers and the command.
#[derive(Clone, Copy)]
struct Setup {
    workers: u32,
    ask: Ask<'static>,
    plant: Plant,
    /// The tool's identity (the binary's build id in blaze-cli).
    tool: &'static [u8],
    /// The kind whose producer is made from other code (D105): its tool and the whole tool move.
    moved: Option<Kind>,
    /// Whether each kind is keyed by its own tool, or (planted) by the whole one.
    split: Split,
    /// A first request hands only the manifests and opens each file where a units record says,
    /// as blaze-cli's cold run does; otherwise every file is handed at `Start`.
    opens: bool,
}

const SIM_TOOL: &[u8] = b"tool";

/// Every finding is an error in the sim (`run` fails on lints too), but the one that says its
/// item was not checked, which is no error of the program (blaze-cli's `types-not-checked`).
fn every_rule(rule: u16) -> bool {
    rule != NOT_CHECKED_RULE
}

/// The rule of a body whose check stopped short ([`World::unchecked`]).
const NOT_CHECKED_RULE: u16 = 0x0C18;

/// The sim's not-checked policy ([`Request::with_not_checked`]).
fn not_checked(rule: u16) -> bool {
    rule == NOT_CHECKED_RULE
}

fn config(workers: u32, tool: &[u8]) -> Config {
    config_moved(workers, tool, None, Split::PerKind)
}

/// Whether each kind's key holds its own producer's tool (D105), or, the planted bug, every
/// kind's the whole tool (a producer left out of its kind's key: any producer's edit reruns all).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Split {
    PerKind,
    LeftOut,
}

/// [`config`] with the producer of `moved` made from other code: the whole tool moves with it (it
/// holds every producer); each other kind's tool stays.
fn config_moved(workers: u32, tool: &[u8], moved: Option<Kind>, split: Split) -> Config {
    let mut whole_bytes = StdVec::from(tool);
    if let Some(kind) = moved {
        whole_bytes.push(kind as u8);
    }
    let whole = Tool(key::digest("sim tool", &whole_bytes));
    let tools = Kind::ALL.map(|kind| match split {
        Split::LeftOut => whole,
        Split::PerKind => {
            let domain = if moved == Some(kind) { "sim kind tool moved" } else { "sim kind tool" };
            Tool(key::digest(domain, &[tool, &[kind as u8]].concat()))
        }
    });
    Config { tool: whole, tools, workers, most_entries: 32, most_jobs: 1 << 20, is_error: every_rule }
}

const ASK: Ask<'static> = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space: Space::NONE, program: None, facts: &[] };

/// 1, 2 or 16 workers, as the tape picks.
fn workers(tape: &Tape) -> u32 {
    [1, 2, 16][tape.index(3, "workers")]
}

fn setup(tape: &Tape, plant: Plant) -> Setup {
    Setup { workers: workers(tape), ask: ASK, plant, tool: SIM_TOOL, moved: None, split: Split::PerKind, opens: tape.one_in(2, "open the files") }
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
                    let mut left: Vec<Want, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
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
                // The planted checker: each signature a body read is reported at a digest of
                // its input's out and the store's record count, which moves as records are
                // written (tempo's head digest, taken from a memo that depends on what the
                // process checked before, 2026-11-17).
                Answered::Done(mut made) if world.read_bug == ReadBug::ReadsFromStoreState && made.name.kind == Kind::BodyCheck => {
                    let held = format!("{}", store.records.len());
                    for (_, saw) in made.reads.iter_mut() {
                        *saw = key::digest("sim read from store state", &[&saw.0[..], held.as_bytes()].concat());
                    }
                    answered.push((ticket, Answered::Done(made), in_place));
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
            changed: before.is_none_or(|old| old.unread.contains(&f) || old.files.get(f).is_none_or(|_| old.id(f) != ids[f])),
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

    let mut req = Request::planted_in(
        config_moved(setup.workers, setup.tool, setup.moved, setup.split),
        setup.ask,
        setup.plant,
        blaze_own::testing::heap().alloc(),
    )
    .with_not_checked(not_checked);
    if world.closure {
        req = req.with_member_closure(Name::manifest(&member_path(0)));
    }
    // Room for `one_body_reading_many_pages`'s 3,000 files, four names each.
    let table: Settled<Global> = Settled::new_in(1 << 15, Global);
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
    let mut ended = Ended {
        answer: None,
        error: None,
        runs: BTreeMap::new(),
        ran: StdVec::new(),
        prints: StdVec::new(),
        last: None,
        delivered: 0,
        in_place: 0,
        started: false,
        stopped: false,
        tested: false,
        tests_ok: None,
        bookkeeping: 0,
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
    let bodies: StdVec<Name> = (0..world.files.len())
        .filter(|f| world.wants_from.as_ref().is_none_or(|from| from.contains(f)))
        .flat_map(|f| (0..world.files[f].len()).map(move |i| body_name(f, i)))
        .collect();
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
                            cells: &[],
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
                } else if bodies.is_empty() || tape.one_in(2, "the program exits") {
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
                // The tests run to their end as the caller's suite does (blaze-cli test_drive.rs),
                // wanting nothing of the machine; how they end is the tape's.
                Effect::StartTests { .. } => {
                    oracle(!ended.tested, || String::from("the tests started twice"))?;
                    oracle(!ended.started && !running, || String::from("tests started beside a program"))?;
                    oracle(first.is_none(), || String::from("tests started with an event still to step"))?;
                    let ok = tape.one_in(2, "the tests pass");
                    ended.tested = true;
                    ended.tests_ok = Some(ok);
                    first = Some(Event::Exited { ok });
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
                        ended.ran.push(task.name);
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
            // A settled job holds no needs: they are freed as it settles (RUNNER 17).
            let kept = req.kept_needs();
            oracle(kept == 0, || format!("settled jobs keep room for {kept} needs"))?;
            // The answer is the last thing the machine says: a program it started has exited
            // first, or its exit would step an answered machine.
            oracle(!ended.started || exited, || String::from("answered while the program it started is running"))?;
            oracle(!ended.tested || exited, || String::from("answered while the tests it started are running"))?;
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
            // `Owners` records are the machine's own (owners.rs), never delivered by a batch.
            let made: u32 = Kind::ALL.iter().zip(answer.profile.kinds.iter()).filter(|(kind, _)| **kind != Kind::Owners).map(|(_, k)| k.made).sum();
            // Fewer when a run of a dropped name was answered for nothing; never more.
            oracle(made <= ended.delivered, || {
                format!("a batch result applied twice: {made} records made from {} delivered", ended.delivered)
            })?;
            ended.bookkeeping = req.bookkeeping();
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
    let root = read_root(root_bytes, blaze_own::testing::heap().alloc()).map_err(wire)?;
    let mut table = Table::new();
    for page_key in root.pages.iter() {
        if *page_key == Key::default() {
            continue;
        }
        let bytes = store.records.get(page_key).ok_or(Broke::Oracle(String::from("a page is not stored")))?;
        let page = PageView::open(bytes).map_err(|error| Broke::Oracle(format!("a stored page is not one: {error}")))?;
        for row in page.rows() {
            table.insert(
                row.name(),
                (
                    row.key(),
                    row.at(),
                    row.source(),
                    row.out(),
                    row.home(),
                    row.place(),
                    row.reads().iter().collect(),
                    row.parts().iter().collect(),
                    row.readers().iter().collect(),
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
    equal_to(store, ended, &fresh_store, &fresh_ended, false)
}

/// `ended`'s answer table and findings against a fresh build's. `within`: every row of the
/// fresh table is in this one, equal, and this one may hold more (`run`'s table keeps the rows
/// of bodies an earlier closure reached).
fn equal_to(store: &Store, ended: &Ended, fresh_store: &Store, fresh_ended: &Ended, within: bool) -> Result<(), Broke> {
    let there = equal_table(store, ended, fresh_store, fresh_ended, within)?;
    equal_prints(ended, fresh_ended, within, &there)
}

/// The answer tables alone ([`equal_to`]); the fresh one is returned.
fn equal_table(store: &Store, ended: &Ended, fresh_store: &Store, fresh_ended: &Ended, within: bool) -> Result<Table, Broke> {
    let last = ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let fresh_last = fresh_ended.last.as_deref().ok_or(Broke::Oracle(String::from("no last written")))?;
    let mut here = table(store, last)?;
    let there = table(fresh_store, fresh_last)?;
    // Within: a row a narrower request keeps also keeps its readers outside today's reach (a
    // body an earlier closure reached); each still reads it, so it stays a reader.
    if within {
        here.retain(|name, _| there.contains_key(name));
        for row in here.values_mut() {
            row.8.retain(|reader| there.contains_key(reader));
        }
    }
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
        let first = here.keys().chain(there.keys()).find(|name| here.get(name) != there.get(name));
        let rows = first.map(|name| format!("here {:?}, fresh {:?}", here.get(name), there.get(name)));
        format!("the answer table differs from a fresh build at {differ:?}; the first: {rows:?}")
    })?;
    Ok(there)
}

/// The findings printed alone ([`equal_to`]), against the fresh table `there`.
fn equal_prints(ended: &Ended, fresh_ended: &Ended, within: bool, there: &Table) -> Result<(), Broke> {
    // A narrower request prints no finding of a name it does not reach today: such an error
    // would hold a start the program never needs.
    let beyond = ended.prints.iter().find(|print| within && !there.contains_key(&print.0));
    oracle(beyond.is_none(), || format!("a finding of {:?}, which this request does not reach, was printed", beyond.map(|print| print.0)))?;
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

// --- A read list is a function of the text and the inputs ------------------------------------

/// The executor's plant for the read-list case: none, or a body that reports each signature it
/// read at a digest of what the store holds, not at the out it was handed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadBug {
    None,
    ReadsFromStoreState,
}

/// The most entries a job settles within here: a run that asks its needs, the run handed them,
/// and one more for each of two inputs re-made while it was out. A run asks each need once,
/// since what it reads follows from its text and its inputs, never from the store, so its read
/// list at a settle is the one it asked for: no entry finds a read it was not handed.
const MOST_SETTLE_ENTRIES: usize = 4;

/// The request ended with no error, and no job settled after more than [`MOST_SETTLE_ENTRIES`].
fn within_bound(what: &str, ended: &Ended) -> Result<(), Broke> {
    if let Some(error) = ended.error {
        return Err(Broke::Oracle(format!("{what}: a task's read list is not a function of its text and inputs: {error}")));
    }
    let answer = ended.answer.ok_or(Broke::Harness(format!("{what}: no answer")))?;
    let hist = answer.profile.entries_hist;
    let over: u32 = hist[MOST_SETTLE_ENTRIES + 1..].iter().sum();
    oracle(over == 0, || format!("{what}: {over} jobs settled after more than {MOST_SETTLE_ENTRIES} entries (by entries 0|1|2|3|4|5-8|9-16|more: {hist:?})"))
}

fn settles_within_bound(tape: &Tape, bug: ReadBug) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    world.read_bug = bug;
    let mut store = Store::default();
    let first = request(tape, &world, None, &mut store, None, Plant::None)?;
    within_bound("a first build", &first)?;
    let next = edit(tape, &world, 0, tape.one_in(2, "body or signature"));
    let second = request(tape, &next, Some(&world), &mut store, first.last.as_deref(), Plant::None)?;
    within_bound("an edit", &second)
}

/// Every job settles within [`MOST_SETTLE_ENTRIES`] entries, cold and after an edit.
/// Expected: "test sim::every_task_settles_within_its_bound ... ok".
#[test]
fn every_task_settles_within_its_bound() {
    check(SimConfig::cases(300), |tape| settles_within_bound(tape, ReadBug::None));
}

/// A body whose read digests follow from the store's state, not its inputs, is refused by name
/// (`ReadNotHanded`, or `Reentries` naming its last two entries), never left to loop.
/// Expected: "test sim::control_a_read_list_from_store_state_is_caught ... ok".
#[test]
fn control_a_read_list_from_store_state_is_caught() {
    control(SimConfig::cases(300), ReadBug::None, ReadBug::ReadsFromStoreState, "is not a function of its text and inputs", settles_within_bound);
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

// --- A dropped name is entered nothing again --------------------------------------------------

/// An edit that removes the last item of a file whose body called other items, and moves the
/// signature of each item that body called; every other item's use of the removed one goes
/// with it. The removed body's reads move before or after its home drops it, in the order the
/// tape completes them: blaze-cli's `records_outlive_the_packs_merge`, where a removed fn's body
/// read its file's moved `Surface` and more, and the request stopped as a cycle of that one body
/// (2026-11-17). `None`: no file has such an item.
fn removal(tape: &Tape, world: &World, serial: u64) -> Option<World> {
    let calling = |f: &usize| {
        let items = &world.files[*f];
        items.len() > 1 && items.last().is_some_and(|item| item.calls.iter().any(|call| *call != (*f, items.len() - 1)))
    };
    let candidates: StdVec<usize> = (0..world.files.len()).filter(calling).collect();
    if candidates.is_empty() {
        return None;
    }
    let f = candidates[tape.index(candidates.len(), "removal file")];
    let mut files = world.files.clone();
    let gone_at = (f, files[f].len() - 1);
    let gone = files[f].pop()?;
    for file in files.iter_mut() {
        for item in file.iter_mut() {
            item.calls.retain(|call| *call != gone_at);
            if item.uses == Some(gone_at) {
                item.uses = None;
            }
        }
    }
    let mut called: StdVec<(usize, usize)> = gone.calls.iter().copied().filter(|call| *call != gone_at).collect();
    called.sort_unstable();
    called.dedup();
    if called.len() > 1 {
        // Two reads of the dropped body move: the second dirties a job its drop settled.
        tape.reached("a removed body read two signatures that moved");
    }
    for (cf, ci) in called {
        files[cf][ci].sig = 2_000 + serial;
    }
    Some(World::new(files))
}

/// After each removal ([`removal`]) the request answers (no `NeedCycle`, no run of the removed
/// body) and its table and findings equal a fresh build's.
fn removals(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let mut last = first.last;
    for serial in 0..1 + tape.below(2, "removals") {
        let Some(next) = removal(tape, &world, serial) else { return Ok(()) };
        let ended = request(tape, &next, Some(&world), &mut store, last.as_deref(), plant)?;
        if let Some(error) = ended.error {
            return Err(Broke::Oracle(format!("a removal's request failed: {error}")));
        }
        let (_, ended) = finished(ended)?;
        same_as_fresh(tape, &next, &store, &ended)?;
        last = ended.last.clone();
        world = next;
    }
    Ok(())
}

/// The invariant: a name its home dropped is never entered again by a read of it that moves;
/// the request answers as a fresh build of the edited files does.
#[test]
fn a_removed_item_answers_as_a_fresh_build() {
    check(SimConfig::cases(300).must_reach(&["a removed body read two signatures that moved"]), |tape| removals(tape, Plant::None));
}

/// The planted bug: `dirty` enters a dropped name again (the waiter of nothing, run as a part
/// no home lists).
#[test]
fn the_oracle_catches_a_dropped_name_entered_again() {
    control(SimConfig::cases(2000), Plant::None, Plant::DirtyDropped, "a dropped name entered again", removals);
}

fn flights(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    finished(request(tape, &world, None, &mut store, None, plant)?)?;
    Ok(())
}

/// The planted kept needs (the clone's largest holder, 2026-11-16) fail the answer's oracle.
#[test]
fn control_needs_kept_past_settling_are_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::KeepSettledNeeds, "settled jobs keep room", flights);
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

/// D105: each kind's key holds its own producer's tool. With the body checker's producer moved
/// (and so the whole tool, which holds every producer: no certificate, no pointer stands), the
/// body checks run again and no other kind runs: every other record is found under its unchanged
/// key. Without the split (every kind keyed by the whole tool, the planted `Split::LeftOut`) every
/// kind runs again: the control below.
fn a_moved_kind(tape: &Tape, split: Split) -> Result<(), Broke> {
    let world = World::draw(tape);
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
    let bodies = first.runs.get(&Kind::BodyCheck).copied().unwrap_or(0);
    let moved = Setup { moved: Some(Kind::BodyCheck), split, ..setup(tape, Plant::None) };
    let (_, ended) = finished(request_with(tape, &world, Some(&world), &mut store, first.last.as_deref(), moved)?)?;
    let others: StdVec<(Kind, u32)> =
        ended.runs.iter().filter(|(kind, _)| **kind != Kind::BodyCheck).map(|(kind, n)| (*kind, *n)).collect();
    oracle(others.is_empty(), || format!("a moved body checker also ran {others:?}"))?;
    let again = ended.runs.get(&Kind::BodyCheck).copied().unwrap_or(0);
    oracle(bodies == 0 || again > 0, || format!("a moved body checker ran none of {bodies} bodies again"))
}

#[test]
fn a_moved_producer_reruns_its_own_kind_alone() {
    check(SimConfig::cases(100), |tape| a_moved_kind(tape, Split::PerKind));
}

#[test]
fn control_a_producer_left_out_of_its_kind_is_caught() {
    control(SimConfig::cases(200), Split::PerKind, Split::LeftOut, "a moved body checker also ran", a_moved_kind);
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
        // file's owners (its standing `Owners` record, read for whether a root is asked before
        // its bodies are made jobs: owners.rs), the body's key; the body's inputs stand on
        // their rows.
        let pages = answer.profile.pages;
        oracle(store.reads <= 6 + pages, || format!("a one-body edit read the store {} times ({pages} pages)", store.reads))?;
        same_as_fresh(tape, &next, &store, &ended)
    });
}

/// One body that calls the signature of each of `files - 1` other files, spread over the answer
/// table's pages, edited: its run walks its shape, asks every page its reads sit on at once,
/// and walks the shape again once, when the last page is in. The machine's bookkeeping is then
/// linear in reads plus pages. Woken by each page (`Plant::WakePerPage`, as `Job::parked` did)
/// it walks the shape once a page: reads times pages (tempo's one body edit, check-g051344-body:
/// 828 steps in 172.6 ms against 68 in 27.0 ms for a comment). Returns (bookkeeping, pages).
fn one_body_reading_many_pages(tape: &Tape, plant: Plant) -> Result<(u64, u64), Broke> {
    const FILES: usize = 3_000;
    let item = |calls: StdVec<(usize, usize)>| Item { sig: 0, body: 0, uses: None, calls, cfg: StdVec::new() };
    let files = (0..FILES).map(|f| std::vec![item(if f == 0 { (1..FILES).map(|g| (g, 0)).collect() } else { StdVec::new() })]).collect();
    let world = World::new(files);
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
    let mut next = world.clone();
    next.files[0][0].body = 1_000;
    let (answer, ended) = finished(request(tape, &next, Some(&world), &mut store, first.last.as_deref(), plant)?)?;
    let ran = |kind| ended.runs.get(&kind).copied().unwrap_or(0);
    oracle(ran(Kind::BodyCheck) == 1, || format!("the edit ran {:?}", ended.runs))?;
    let pages = u64::from(answer.profile.pages);
    oracle(pages >= 8, || format!("the reads sat on {pages} pages: too few to tell a wake per page"))?;
    let reads = (FILES - 1) as u64;
    // The whole request's count: the start's 3,000 files and the finish's table are in it too
    // (226,248 here, 74 a read and page); a wake per page walks 2,999 reads 64 times on top.
    oracle(ended.bookkeeping <= 96 * (reads + pages), || {
        format!("a one-body edit reading {reads} names on {pages} pages kept {} of bookkeeping, over 96 per read and page", ended.bookkeeping)
    })?;
    same_as_fresh(tape, &next, &store, &ended)?;
    Ok((ended.bookkeeping, pages))
}

/// Expected: "test sim::a_body_reading_many_pages_is_woken_once ... ok".
#[test]
fn a_body_reading_many_pages_is_woken_once() {
    check(SimConfig::cases(1), |tape| one_body_reading_many_pages(tape, Plant::None).map(|_| ()));
}

/// Control: woken per page, the bound is broken.
#[test]
fn control_a_wake_per_page_is_caught() {
    control(SimConfig::cases(1), Plant::None, Plant::WakePerPage, "of bookkeeping", |tape, plant| one_body_reading_many_pages(tape, plant).map(|_| ()));
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
        world.members =
            (0..3).map(|k| Member { version: 0, files: (0..world.files.len()).filter(|f| f % 3 == k).collect(), deps: StdVec::new() }).collect();
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
            let setup = Setup { workers, ask: ASK, plant: Plant::None, tool: SIM_TOOL, moved: None, split: Split::PerKind, opens };
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

/// `run` under `Beside` or `Before`: no success while an error exists or could still be found;
/// under `Before` no `StartProgram` before a check that is clean AND complete (no item it could
/// not check, run.md D124 "The gate is before the start"), and a clean complete one starts the
/// program. Then the repeat with nothing changed: it stands on the certificate, runs no task,
/// and starts exactly when the first was allowed to (the gate's answer found by key).
fn gates(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    world.unchecked = tape.one_in(2, "items not checked");
    let gate = [Gate::Beside, Gate::Before][tape.index(2, "gate")];
    let mut store = Store::default();
    let setup = Setup {
        workers: workers(tape),
        ask: Ask { command: Command::Run, args: b"", gate, space: Space::NONE, program: Some(sim_native()), facts: &[] },
        plant,
        tool: SIM_TOOL,
        moved: None,
        split: Split::PerKind,
        opens: tape.one_in(2, "open the files"),
    };
    let (answer, ended) = finished(request_with(tape, &world, None, &mut store, None, setup)?)?;
    let (_, checked) = fresh(tape, &world)?;
    let errors = checked.prints.iter().filter(|print| every_rule(print.1)).count();
    let unchecked = checked.prints.iter().filter(|print| not_checked(print.1)).count();
    // What the gate allows: under `Before` a clean and complete check; under `Beside` no error
    // (an unchecked item does not hold a start beside).
    let may_start = errors == 0 && (gate == Gate::Beside || unchecked == 0);
    let before_holds = |started: bool, when: &str| -> Result<(), Broke> {
        if gate == Gate::Before && started {
            oracle(errors == 0, || format!("{when}: `Before` started a program over errors"))?;
            oracle(unchecked == 0, || format!("{when}: `Before` started a program over {unchecked} items not checked"))?;
        }
        Ok(())
    };
    before_holds(ended.started, "the first run")?;
    // A clean (and, before, complete) check starts the program: the gate is no refusal of all.
    oracle(!may_start || ended.started, || format!("the first run: a {gate:?} check clean of errors and of unchecked items did not start the program"))?;
    if gate == Gate::Before {
        oracle(ended.started == may_start, || String::from("the first run: `Before` started a program it was not allowed to"))?;
    }
    match answer.verdict {
        Verdict::Passed | Verdict::ProgramFailed => {
            oracle(errors == 0, || format!("a {:?} verdict over {errors} errors", answer.verdict))?;
            oracle(gate == Gate::Beside || unchecked == 0, || format!("a {:?} verdict under `Before` over {unchecked} items not checked", answer.verdict))?;
        }
        Verdict::Failed { started, .. } => {
            oracle(errors > 0 || (gate == Gate::Before && unchecked > 0), || String::from("a failed verdict with no error and no item not checked"))?;
            oracle(gate == Gate::Beside || !started, || String::from("`Before` started a program over errors"))?;
            oracle(started == ended.started, || String::from("the verdict's `started` is not what happened"))?;
        }
        other => return Err(Broke::Oracle(format!("`run` answered {other:?}"))),
    }
    // The repeat, nothing changed: the certificate decides the start at its one read.
    let repeat = request_with(tape, &world, Some(&world), &mut store, ended.last.as_deref(), setup)?;
    match &repeat.error {
        // The program wanted a body the certificate stood on and the walk did not settle: owed
        // (see `commands`); the start was decided before it.
        Some(DriveError::LateWant { .. }) | None => {}
        Some(error) => return Err(Broke::Oracle(format!("the repeat stopped with an error: {error}"))),
    }
    before_holds(repeat.started, "the repeat")?;
    oracle(repeat.runs.is_empty(), || format!("the repeat ran {:?}: the gate's answer is found by key", repeat.runs))?;
    oracle(repeat.started == may_start, || format!("the repeat: started {}, and the gate allows {may_start}", repeat.started))?;
    if let Some(again) = repeat.answer {
        oracle(again.profile.certified, || String::from("the repeat did not stand on its certificate"))?;
    }
    Ok(())
}

#[test]
fn no_success_is_reported_while_an_error_could_be_found() {
    check(SimConfig::cases(300), |tape| gates(tape, Plant::None));
}

#[test]
fn control_a_success_before_the_gate_is_done_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::EarlySuccess, "could still find an error", gates);
}

/// A `run` under `Before` and one under `Beside` are two answers: neither takes the other's
/// pointer or certificate (rule 21). `check` ignores the gate, so its shape holds none.
#[test]
fn a_run_under_each_gate_has_its_own_shape() {
    let run = |gate| Ask { command: Command::Run, args: b"", gate, space: Space::NONE, program: Some(sim_native()), facts: &[] };
    assert_ne!(crate::request::shape_of(&run(Gate::Beside)), crate::request::shape_of(&run(Gate::Before)));
    let check = |gate| Ask { gate, ..ASK };
    assert_eq!(crate::request::shape_of(&check(Gate::Beside)), crate::request::shape_of(&check(Gate::Before)));
}

/// `blaze run` of a hello world whose `main` was not checked (2026-11-17: `check` exited 4 and
/// `run` ran it): under `Before` a start over an item the check could not check is caught.
#[test]
fn control_a_start_over_an_unchecked_item_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::StartOverIncomplete, "items not checked", gates);
}

// --- `run`'s scope (run_scope.rs) ------------------------------------------------------------

/// A workspace of up to four members over the drawn files, member 0 the program package, each
/// member depending on some later ones (no cycle, as cargo's). No manifest or file holds a
/// finding: only bodies do, so the start depends on which bodies are asked alone.
fn scope_world(tape: &Tape) -> World {
    let mut world = World::draw(tape);
    world.quiet = true;
    world.closure = true;
    let files = world.files.len();
    let n = (2 + tape.index(3, "members")).min(files);
    world.members = (0..n)
        .map(|k| Member {
            version: 0,
            files: (0..files).filter(|f| f % n == k).collect(),
            deps: (k + 1..n).filter(|_| tape.one_in(2, "a dependency")).collect(),
        })
        .collect();
    world.wants_from = Some(scope_files(&world).into_iter().collect());
    world
}

/// The members `run` asks: member 0 and each member it depends on, transitively.
fn closure_of(world: &World) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = std::vec![0];
    while let Some(k) = stack.pop() {
        if seen.insert(k) {
            stack.extend(world.members[k].deps.iter().copied());
        }
    }
    seen
}

/// The files of the members `run` asks.
fn scope_files(world: &World) -> BTreeSet<usize> {
    closure_of(world).iter().flat_map(|k| world.members[*k].files.iter().copied()).collect()
}

fn run_setup(tape: &Tape, plant: Plant) -> Setup {
    let ask = Ask { command: Command::Run, args: b"", gate: Gate::Before, space: Space::NONE, program: Some(sim_native()), facts: &[] };
    Setup { workers: workers(tape), ask, plant, tool: SIM_TOOL, moved: None, split: Split::PerKind, opens: tape.one_in(2, "open the files") }
}

/// The run made a job of no body outside `files` (its closure's), checked every body of `new`
/// (those no earlier request made) before the start, and started exactly when the closure's
/// bodies hold no error (`errors`).
fn reached_checked(world: &World, ended: &Ended, files: &BTreeSet<usize>, new: &BTreeSet<usize>, errors: usize, when: &str) -> Result<(), Broke> {
    for name in ended.ran.iter().filter(|name| name.kind == Kind::BodyCheck) {
        let (f, _) = world.item_of(*name).ok_or(Broke::Harness(format!("no item for {name}")))?;
        oracle(files.contains(&f), || format!("{when}: {name} of file {f}, which no member of the program's closure holds, was a job"))?;
    }
    for f in new.iter() {
        for i in 0..world.files[*f].len() {
            oracle(ended.ran.contains(&body_name(*f, i)), || format!("{when}: a body the program reaches ({f}/{i}) was not checked before the start"))?;
        }
    }
    oracle(ended.started == (errors == 0), || format!("{when}: started {} over {errors} errors in the bodies the program reaches", ended.started))
}

/// `run` under `Before` with the program package's closure: a body of a member the program
/// does not depend on is never a job and its error does not hold the start; an error in the
/// closure does; a manifest edit that makes a member reached checks its bodies before the start.
fn scopes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = scope_world(tape);
    // Each file's body errors, from the whole check: a body's finding does not depend on the
    // members, so it holds for the edited world too.
    let (_, checked) = fresh(tape, &world)?;
    let mut errors: BTreeMap<usize, usize> = BTreeMap::new();
    for print in checked.prints.iter().filter(|print| print.0.kind == Kind::BodyCheck && every_rule(print.1)) {
        let (f, _) = world.item_of(print.0).ok_or(Broke::Harness(format!("no item for {}", print.0)))?;
        *errors.entry(f).or_insert(0) += 1;
    }
    let errors_in = |files: &BTreeSet<usize>| files.iter().map(|f| errors.get(f).copied().unwrap_or(0)).sum::<usize>();
    let mut store = Store::default();
    let (_, first) = finished(request_with(tape, &world, None, &mut store, None, run_setup(tape, plant))?)?;
    let files = scope_files(&world);
    reached_checked(&world, &first, &files, &files, errors_in(&files), "the first run")?;
    // The package's manifest now depends on a member it did not reach.
    let reached = closure_of(&world);
    let unreached: StdVec<usize> = (0..world.members.len()).filter(|k| !reached.contains(k)).collect();
    if unreached.is_empty() {
        return Ok(());
    }
    let mut next = world.clone();
    next.members[0].deps.push(unreached[tape.index(unreached.len(), "the member reached anew")]);
    next.members[0].version += 1;
    let now = scope_files(&next);
    next.wants_from = Some(now.iter().copied().collect());
    let added: BTreeSet<usize> = now.difference(&files).copied().collect();
    let (_, again) = finished(request_with(tape, &next, Some(&world), &mut store, first.last.as_deref(), run_setup(tape, plant))?)?;
    reached_checked(&next, &again, &now, &added, errors_in(&now), "the run after the edit")
}

/// Random histories over one store in which requests reach different subsets: `check` and
/// `run` (its closure narrower, each its own pointer), edits of a body or a signature, a member
/// reached anew or left, a removal, and now and then a file that fails its task. Every request
/// that answers equals a fresh build of its world under its own ask (`run`'s within it), and
/// every answer a job takes from its own old row for a read the walk did not enter is held to
/// that read's row as it runs (request.rs `taken_read`; debug builds and this simulation only).
/// A planted bug is caught: an old row's answer taken for a read the walk DID enter. The other,
/// a dirtied reader not run again keeping its row unmarked, waits on a known bug (below).
///
/// Each history is of one kind: with failing tasks, or with removals; its closure only grows.
/// Three known bugs, found by these histories and reported 2026-11-17 by lane DR, keep the
/// rest out, each an ignored test: a failing task among removals runs a body on old text
/// (`known_bug_a_failing_task_among_removals`); a shrunk closure keeps and prints the findings
/// it no longer reaches (`known_bug_a_shrunk_closure_prints_what_it_left`; after a shrink a
/// `run`'s table is held to the fresh one and its prints are not); a part a shrunk closure
/// held keeps its old row (`known_bug_a_part_held_by_a_shrunk_closure_keeps_its_old_row`).
fn histories(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mix = match tape.one_in(2, "a history with failing tasks, not removals") {
        true => Mix { removals: false, shrink: false, ..Mix::ALL },
        false => Mix { fails: false, shrink: false, ..Mix::ALL },
    };
    histories_of(tape, plant, mix)
}

/// What a history may hold ([`histories`]).
#[derive(Clone, Copy)]
struct Mix {
    run: bool,
    check: bool,
    fails: bool,
    removals: bool,
    members: bool,
    /// A member's dependency dropped: `run`'s closure shrinks.
    shrink: bool,
}

impl Mix {
    const ALL: Mix = Mix { run: true, check: true, fails: true, removals: true, members: true, shrink: true };
}

fn histories_of(tape: &Tape, plant: Plant, mix: Mix) -> Result<(), Broke> {
    let mut world = scope_world(tape);
    let mut store = Store::default();
    // Each pointer's last answer and the world it answered: `check`'s, then `run`'s.
    let mut lasts: [Option<(StdVec<u8>, World)>; 2] = [None, None];
    // A member's dependency was dropped in this history: `run`'s closure shrank once.
    let mut shrunk = false;
    for serial in 0..2 + tape.below(10, "requests") {
        let mut next = world.clone();
        match tape.index(5, "edit") {
            0 => next = edit(tape, &world, serial, true),
            1 => next = edit(tape, &world, serial, false),
            2 | 3 if !mix.members => {}
            3 if !mix.shrink => {}
            4.. if !mix.removals => {}
            2 => {
                let reached = closure_of(&world);
                let unreached: StdVec<usize> = (0..world.members.len()).filter(|k| !reached.contains(k)).collect();
                if !unreached.is_empty() {
                    next.members[0].deps.push(unreached[tape.index(unreached.len(), "the member reached anew")]);
                    next.members[0].version += 1;
                }
            }
            3 => {
                if next.members[0].deps.pop().is_some() {
                    next.members[0].version += 1;
                    shrunk = true;
                }
            }
            _ => {
                // The removal's text, in this world (its members, its crates).
                if let Some(removed) = removal(tape, &world, serial) {
                    next.files = removed.files;
                    next.items = removed.items;
                    next.file_subjects = removed.file_subjects;
                }
            }
        }
        // The program wants no body: a `run` whose certificate stands (an edit outside its
        // closure) holds no `now` rows, and a want then fails `LateWant` (request.rs `wanted`,
        // reported to the coordinator 2026-11-17); these histories are about the walk.
        next.wants_from = Some(StdVec::new());
        let run = match (mix.run, mix.check) {
            (true, true) => tape.one_in(2, "run, not check"),
            (run, _) => run,
        };
        let pointer = usize::from(run);
        let setup = match run {
            true => run_setup(tape, plant),
            false => setup(tape, plant),
        };
        let mut asked = next.clone();
        let fails = mix.fails && tape.one_in(4, "a file fails its task");
        if fails {
            asked.failing_file = Some(tape.index(asked.files.len(), "the failing file"));
        }
        let (last, before) = match &lasts[pointer] {
            Some((last, before)) => (Some(last.as_slice()), Some(before)),
            None => (None, None),
        };
        let (_, ended) = finished(request_with(tape, &asked, before, &mut store, last, setup)?)?;
        world = next;
        // A failed task leaves no last answer: the next request walks from the one before.
        if fails || ended.last.is_none() {
            continue;
        }
        let mut fresh_store = Store::default();
        let (_, fresh_ended) = finished(request_with(tape, &asked, None, &mut fresh_store, None, Setup { plant: Plant::None, ..setup })?)?;
        let there = equal_table(&store, &ended, &fresh_store, &fresh_ended, run)?;
        // A shrunk closure's prints are the known bug's
        // (`known_bug_a_shrunk_closure_prints_what_it_left`): its table is held to the fresh one.
        if !(run && shrunk) {
            equal_prints(&ended, &fresh_ended, run, &there)?;
        }
        let mut read = world.clone();
        if run {
            let reached = scope_files(&world);
            read.unread = (0..world.files.len()).filter(|f| !reached.contains(f)).collect();
        }
        lasts[pointer] = ended.last.clone().map(|last| (last, read));
    }
    Ok(())
}

/// Expected: "test sim::histories_over_narrower_and_wider_requests_answer_as_fresh_builds ... ok".
#[test]
fn histories_over_narrower_and_wider_requests_answer_as_fresh_builds() {
    check(SimConfig::cases(300), |tape| histories(tape, Plant::None));
}

/// A known bug (lane DR, 2026-11-17): `check` histories with failing tasks AND removals run a
/// `BodyCheck` with a source that is not its text's (seed 16 of 300, before the floor too).
#[test]
#[ignore = "known bug: a failing task in a history with removals runs a body on old text"]
fn known_bug_a_failing_task_among_removals() {
    check(SimConfig::cases(300), |tape| histories_of(tape, Plant::None, Mix { run: false, fails: true, removals: true, members: false, shrink: false, check: true }));
}

/// A known bug (lane DR, 2026-11-17): `run` after its closure shrank prints the finding of a
/// body it no longer reaches (request.rs `finish`: old findings merged unless dropped).
#[test]
#[ignore = "known bug: a shrunk closure keeps and prints the findings it no longer reaches"]
fn known_bug_a_shrunk_closure_prints_what_it_left() {
    check(SimConfig::cases(300), |tape| histories_of(tape, Plant::None, Mix { check: false, fails: false, removals: false, ..Mix::ALL }));
}

/// A known bug (lane DR, 2026-11-17): a `run` whose closure shrank re-reads an edited file it no
/// longer reaches; its parts are held and dropped unmade (request.rs `release`), keeping their
/// old rows, and when the closure grows again the file's row stands and they are never entered:
/// a `Signature` keeps its old source and its readers stand on it. The same invariant as
/// [`Request::taken_read`]'s, broken by held parts rather than dirtied readers.
#[test]
#[ignore = "known bug: a part held by a shrunk closure keeps its old row"]
fn known_bug_a_part_held_by_a_shrunk_closure_keeps_its_old_row() {
    check(SimConfig::cases(300), |tape| histories_of(tape, Plant::None, Mix { check: false, fails: false, removals: false, ..Mix::ALL }));
}

/// The dirtied reader `run` leaves unrun is reachable only after its closure shrank (one that
/// only grows runs every reader it ever reached), which the known bug above blocks: until it is
/// fixed this control cannot show its plant caught.
#[test]
#[ignore = "blocked by known_bug_a_part_held_by_a_shrunk_closure_keeps_its_old_row"]
fn control_a_dirtied_reader_left_unmarked_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::StaleReaderKept, "", |tape, plant| {
        histories_of(tape, plant, Mix { fails: false, removals: false, ..Mix::ALL })
    });
}

#[test]
fn control_an_old_answer_taken_for_an_entered_read_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::RecordedForEntered, "", histories);
}

#[test]
fn a_run_checks_the_program_packages_closure_alone() {
    check(SimConfig::cases(300), |tape| scopes(tape, Plant::None));
}

#[test]
fn control_a_reached_member_left_out_is_caught() {
    control(SimConfig::cases(2000), Plant::None, Plant::ReachedSkipped, "the program reaches", scopes);
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
        // `test` names the image too: under `Plant::TestStartsProgram` it starts one.
        let program = (!matches!(command, Command::Check)).then(sim_native);
        let setup = Setup { ask: Ask { command, args: b"", gate: Gate::Beside, space: Space::NONE, program, facts: &[] }, ..setup(tape, plant) };
        let ended = request_with(tape, &world, before.as_ref(), &mut store, last.as_deref(), setup)?;
        match &ended.error {
            Some(DriveError::LateWant { .. }) => return Ok(()),
            Some(error) => return Err(Broke::Oracle(format!("`{command:?}` stopped with an error: {error}"))),
            None => {}
        }
        let (answer, ended) = finished(ended)?;
        // `test` starts no program (a package's `main` is not its tests): it runs its tests after
        // its check (`Effect::StartTests`, sim/tests_start.rs), and its answer is theirs.
        if command == Command::Test {
            oracle(!ended.started, || String::from("`test` started a program: `main` is not a test"))?;
            match answer.verdict {
                Verdict::Passed => oracle(ended.tests_ok == Some(true), || String::from("`test` passed and its tests did not"))?,
                Verdict::ProgramFailed => oracle(ended.tests_ok == Some(false), || String::from("`test` failed as its tests and they passed or did not run"))?,
                Verdict::Failed { started: false, .. } => oracle(!ended.tested, || String::from("`test` failed its check and its tests ran"))?,
                other => return Err(Broke::Oracle(format!("`test` answered {other:?}"))),
            }
        }
        // Only `test` runs tests.
        oracle(command == Command::Test || !ended.tested, || format!("`{command:?}` started tests"))?;
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
    let mut req = Request::planted_in(config(16, SIM_TOOL), ask, world.plant, blaze_own::testing::heap().alloc());
    let mut store: BTreeMap<Key, StdVec<u8>> = BTreeMap::new();
    let mut todo: std::collections::VecDeque<Effect<'static>> = std::collections::VecDeque::new();
    let mut out: Vec<Effect<'static>, Alloc<'static>> = Vec::new_in(blaze_own::testing::heap().alloc());
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
                    .map(|(m, findings)| Output { name: m.name, out: m.out, reads: &m.reads, cells: &[], parts: &m.parts, paths: &[], findings, payload: b"" })
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
    let ask = Ask { command: Command::Check, args: b"", gate: Gate::Beside, space, program: None, facts: &[] };
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
        let run = fifo(&world, Ask { command: Command::Check, args: b"", gate: Gate::Beside, space, program: None, facts: &[] })?;
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

/// A request in which one task fails puts every other record it settled (a request with
/// findings is the normal state while a person edits: the coordinator's rule, 2026-11-16), and
/// the repeat, nothing changed, finds them all: its found and standing jobs are at least the
/// first's made ones, and it makes none but what failed.
fn a_failure_keeps_the_rest(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = names_world(tape);
    if world.crates.len() < 2 {
        return Ok(());
    }
    world.crates[0].fails = true;
    let mut store = Store::default();
    let (answer, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let made: u32 = answer.profile.kinds.iter().map(|k| k.made).sum();
    oracle(answer.failed > 0, || "the planted crate did not fail".into())?;
    let (again, _) = finished(request(tape, &world, Some(&world), &mut store, first.last.as_deref(), plant)?)?;
    let kept: u32 = again.profile.kinds.iter().map(|k| k.found + k.standing).sum();
    // An `Owners` row with no answer to stand on is computed again from its parents' rows (its
    // key holds no text its parents follow from, owners.rs): no task, so not counted.
    let owners_made = again.profile.kinds[Kind::Owners.index()].made;
    let remade: u32 = again.profile.kinds.iter().map(|k| k.made).sum::<u32>() - owners_made;
    let made = made - answer.profile.kinds[Kind::Owners.index()].made;
    oracle(kept >= made && remade == 0, || {
        format!("a request with a failure put {} records of {made} made; the repeat found or stood {kept} and made {remade}", answer.profile.puts)
    })
}

#[test]
fn a_request_with_a_failure_puts_every_settled_record() {
    check(SimConfig::cases(200), |tape| a_failure_keeps_the_rest(tape, Plant::None));
}

#[test]
fn control_puts_dropped_on_a_failure_are_caught() {
    control(SimConfig::cases(500), Plant::None, Plant::DropPutsOnFailure, "the repeat found or stood", a_failure_keeps_the_rest);
}

/// An edit lands between the sweep and a file's task (the coordinator's ruling: verified
/// bytes). The task compares what it read with the id its key holds, fails `Changed`, and puts
/// nothing: no record of the file is under the swept id, and the request is not certified.
fn an_edit_after_the_sweep(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::draw(tape);
    if world.files.is_empty() {
        return Ok(());
    }
    world.edited_at_run = Some(0);
    world.verify_plant = plant;
    let mut store = Store::default();
    let (answer, ended) = finished(request(tape, &world, None, &mut store, None, Plant::None)?)?;
    let file = file_name(0);
    let filed = store.records.values().any(|bytes| matches!(crate::record::open(bytes), Ok(crate::record::Stored::Whole(view)) if view.name == file));
    oracle(!filed, || format!("a record of {file} was put under the swept id though its bytes changed before the read"))?;
    oracle(answer.failed > 0 && ended.last.is_none(), || format!("{} failed and last written {}: the changed file must leave the answer uncertified", answer.failed, ended.last.is_some()))
}

#[test]
fn a_file_changed_after_the_sweep_puts_nothing() {
    check(SimConfig::cases(200), |tape| an_edit_after_the_sweep(tape, Plant::None));
}

#[test]
fn control_a_skipped_content_compare_is_caught() {
    control(SimConfig::cases(300), Plant::None, Plant::SkipContentCompare, "under the swept id", an_edit_after_the_sweep);
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
        let mut gone = Vec::new_in(blaze_own::testing::heap().alloc());
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

// --- What a task reads follows from its key and its answers (request.rs `Walk`) ---------------

/// The rule of a resolving body's call to an item its callee's file no longer holds: rustc's
/// "cannot find function" at the call, the caller's own error.
const NO_ITEM_RULE: u16 = 0x51D1;

/// The executor's plant for the resolving body: none, or (`HiddenRead`) a body whose call missed
/// ALSO reads its callee file's first signature, so its read names follow from an input that is
/// neither in its key nor among its answers (one builder plants it, the other does not).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NamesBug {
    None,
    HiddenRead,
}

/// A body that resolves its calls (blaze-cli's body check, in small): it reads the `Surface` of
/// each file it calls into first; then, for each call, the callee's `Signature` when the file
/// still holds the item, else nothing more and a [`NO_ITEM_RULE`] finding. Whether the file holds
/// the item follows from its surface (the surface's out digests every signature in the file), so
/// its read names are a function of its key and its answers, and a removed callee changes them
/// under one key: the caller's text, and so its key, stands.
fn resolved_body(world: &World, task: &Task<'static>) -> Result<Answered, Broke> {
    let name = task.name;
    let (f, i) = world.item_of(task.base).ok_or(Broke::Oracle(format!("{name} ran, and no file holds its item: a dropped name entered again")))?;
    let item = &world.files[f][i];
    if body_source(item) != task.source {
        return Err(Broke::Oracle(format!("{name} ran with a source that is not its text's")));
    }
    let callee_files: BTreeSet<usize> = item.calls.iter().map(|(cf, _)| *cf).collect();
    let mut wanted: StdVec<(Name, Name)> = callee_files.iter().map(|cf| (surface_name(*cf), file_name(*cf))).collect();
    let failed = |wanted: &[(Name, Name)]| wanted.iter().find(|(n, _)| task.failed.contains(n)).map(|(on, _)| Answered::Failed(TaskFailure { name, why: Why::Waited { on: *on } }));
    if let Some(failure) = failed(&wanted) {
        return Ok(failure);
    }
    // The surfaces first: which signatures there are follows from them.
    if let Err(lacking) = reads_of(task, &wanted) {
        return Ok(Answered::Need(lacking));
    }
    let mut missing = StdVec::new();
    for (cf, ci) in item.calls.iter().copied() {
        match ci < world.files[cf].len() {
            true => wanted.push((sig_name(cf, ci), file_name(cf))),
            false => {
                missing.push((cf, ci));
                if world.names_bug == NamesBug::HiddenRead {
                    wanted.push((sig_name(cf, 0), file_name(cf)));
                }
            }
        }
    }
    missing.sort_unstable();
    missing.dedup();
    if let Some(failure) = failed(&wanted) {
        return Ok(failure);
    }
    let reads = match reads_of(task, &wanted) {
        Err(lacking) => return Ok(Answered::Need(lacking)),
        Ok(reads) => reads,
    };
    let out = key::digest("sim resolved body out", format!("{} {reads:?} {missing:?}", item.body).as_bytes());
    let findings = missing.iter().map(|(cf, ci)| (NO_ITEM_RULE, 0, 1, format!("no item {ci} in file {cf}").into_bytes())).collect();
    Ok(Answered::Done(Made { name, out, reads, parts: StdVec::new(), findings, paths: StdVec::new() }))
}

/// A world of crates (so each file has a `Surface`) whose bodies resolve their calls.
fn resolving_world(tape: &Tape, bug: NamesBug) -> World {
    let mut world = names_world(tape);
    world.resolve_calls = true;
    world.names_bug = bug;
    world
}

/// The last item of a file removed while a body other than its own still calls it, the calls
/// left as written (blaze-cli's "member removed" case: `f` removed, `h` still calls it). A
/// signature that used it no longer does (its text moves, a key of its own). `None`: no item is
/// so called.
fn callee_removal(tape: &Tape, world: &World) -> Option<World> {
    let called = |f: &usize| {
        let gone = (*f, world.files[*f].len() - 1);
        world.files[*f].len() > 1
            && world.files.iter().enumerate().any(|(g, items)| items.iter().enumerate().any(|(j, item)| (g, j) != gone && item.calls.contains(&gone)))
    };
    let candidates: StdVec<usize> = (0..world.files.len()).filter(called).collect();
    if candidates.is_empty() {
        return None;
    }
    let f = candidates[tape.index(candidates.len(), "removed callee's file")];
    let mut files = world.files.clone();
    let gone = (f, files[f].len() - 1);
    files[f].pop()?;
    for item in files.iter_mut().flatten() {
        if item.uses == Some(gone) {
            item.uses = None;
        }
    }
    let mut next = World::new(files);
    next.crates = world.crates.clone();
    next.macros = world.macros.clone();
    next.resolve_calls = world.resolve_calls;
    next.names_bug = world.names_bug;
    Some(next)
}

/// One store, three requests: the world, its callee removed, the callee back. After the
/// removal the request answers (no two values under one key: the callers' keys stood while
/// their read names moved with their surfaces' answers), equals a fresh build and reports the
/// missing callee; with it back the answer equals a fresh build again, and no body is checked:
/// the first records are found through the shapes, by today's answers.
fn a_removed_callee(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = resolving_world(tape, NamesBug::None);
    let Some(removed) = callee_removal(tape, &world) else { return Ok(()) };
    tape.reached("a caller's key stood while its callee went");
    let mut store = Store::default();
    let (_, first) = finished(request(tape, &world, None, &mut store, None, plant)?)?;
    let gone = request(tape, &removed, Some(&world), &mut store, first.last.as_deref(), plant)?;
    if let Some(error) = gone.error {
        return Err(Broke::Oracle(format!("the removal's request failed: {error}")));
    }
    let (_, gone) = finished(gone)?;
    same_as_fresh(tape, &removed, &store, &gone)?;
    oracle(gone.prints.iter().any(|print| print.1 == NO_ITEM_RULE), || String::from("no caller reported its removed callee"))?;
    let (_, back) = finished(request(tape, &world, Some(&removed), &mut store, gone.last.as_deref(), plant)?)?;
    same_as_fresh(tape, &world, &store, &back)?;
    oracle(runs(&back, Kind::BodyCheck) == 0, || format!("the callee put back checked bodies again: {:?}", back.runs))
}

/// The invariant: a removed callee is its callers' finding, the request answers as a fresh
/// build does, and putting it back finds the first answer in the records.
#[test]
fn a_removed_callee_answers_as_a_fresh_build_and_back_from_records() {
    check(SimConfig::cases(300).must_reach(&["a caller's key stood while its callee went"]), |tape| a_removed_callee(tape, Plant::None));
}

/// The cause, removed: each record's shape put at its task key (the drive before `Walk`) puts a
/// second shape under a caller's key when its callee goes, refused as two values.
#[test]
fn control_a_shape_at_the_task_key_is_two_values() {
    control(SimConfig::cases(2000), Plant::None, Plant::ShapeAtTaskKey, "two values under one key", a_removed_callee);
}

/// Two builders of one history (the world, then its callee removed), each in its own store, as
/// two CI machines fill one shared cache: every drive record of one, put into the other's
/// store, is equal bytes where both hold a key (rule 25). Under [`NamesBug::HiddenRead`] the
/// second builder's caller reads one more name though every answer it shares with the first's
/// was the same: two records under one key that are two VALUES (rule 21), and the put is refused.
fn two_builders(tape: &Tape, bug: NamesBug) -> Result<(), Broke> {
    let world = resolving_world(tape, NamesBug::None);
    let Some(removed) = callee_removal(tape, &world) else { return Ok(()) };
    let build = |tape: &Tape, bug: NamesBug| -> Result<Store, Broke> {
        let (mut one, mut two) = (world.clone(), removed.clone());
        one.names_bug = bug;
        two.names_bug = bug;
        let mut store = Store::default();
        let (_, first) = finished(request(tape, &one, None, &mut store, None, Plant::None)?)?;
        finished(request(tape, &two, Some(&one), &mut store, first.last.as_deref(), Plant::None)?)?;
        Ok(store)
    };
    let mut first = build(tape, NamesBug::None)?;
    let second = build(tape, bug)?;
    for (key, bytes) in second.records.iter().filter(|(_, bytes)| crate::record::open(bytes).is_ok()) {
        first.put(*key, bytes)?;
    }
    Ok(())
}

#[test]
fn two_builders_of_one_history_write_equal_records() {
    check(SimConfig::cases(300), |tape| two_builders(tape, NamesBug::None));
}

/// The refusal stays where it belongs: read names that follow from no answer are two values.
#[test]
fn control_read_names_from_a_hidden_input_are_two_values() {
    control(SimConfig::cases(2000), NamesBug::None, NamesBug::HiddenRead, "two values under one key", two_builders);
}
