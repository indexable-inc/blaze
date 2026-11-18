//! EACH HEAD FROZEN ONCE A REQUEST (engine_heads_once.rs, lane RH6), at the engine's seam and
//! under a race. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE SEAM: two members, `b`'s heads reading `a`'s struct. Cold, the answer equals a fresh
//! check's and the one a source a head gives (`Sources::Head`, the measure before the table),
//! every cell is frozen once (`frozen` = `cells` + the races' `twice`), and the cold read sets
//! differ only at witnessed same-crate head cutoffs. Then each edit row is compared with a fresh check:
//! a body-only edit of `a` (EARLY CUTOFF: `a`'s task reruns, `b`'s are not entered), and `a`'s
//! struct gaining a field (`b`'s readers are entered).
//!
//! THE RACE (rule 43, `blaze_sim`): two workers that both miss a cell and freeze it, interleaved
//! every way the tape draws (each a look, then a put on a miss): one entry, the first's bytes, an
//! equal second counted `twice`, other bytes refused. CONTROL: the planted "second write wins"
//! (`OncePlant::SecondWins`) is caught.

use super::engine_check_tests::{engine_opts, entries, opts};
use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::string::String as Label;

use crate::engine_heads::HeadUnits;
use crate::engine_heads_once::{Entry, HeadsOnce, OnceCounts, OnceError, OncePlant, Put, Sources};
use blaze_drive::{Kind, Name};
use blaze_engine::{CellId, Stage};
use blaze_ladder::front2::heads::Want;
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

/// The ids a request listed, by the helpers' own form.
type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
const A_LIB: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// A body-only edit: `make`'s body.
const A_LIB_BODY: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    let q = p;\n    q\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// `Point` gains a field.
const A_LIB_FIELD: &str = "pub struct Point {\n    pub x: u32,\n    pub y: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
const B_LIB: &str = "pub struct Holder {\n    pub p: a::Point,\n}\n\npub fn wrap(p: a::Point) -> Holder {\n    Holder { p }\n}\n\npub fn first(h: &Holder) -> u32 {\n    h.p.x\n}\n\npub fn again(p: a::Point) -> a::Point {\n    a::make(p)\n}\n";

fn two_members(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", A_LIB);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    io
}

/// What one request answered: the findings listed, the printed finding and note lines (sorted:
/// their order is not promised), the status and the counts.
struct Said {
    ids: Ids,
    lines: std::vec::Vec<std::string::String>,
    checked: engine_check::Checked,
}

impl Said {
    fn same_answer(&self, other: &Said) -> bool {
        self.ids == other.ids && self.lines == other.lines && self.checked.status.0 == other.checked.status.0
    }

    fn once(&self) -> OnceCounts {
        self.checked.seeds.once
    }

    fn heads(&self) -> u32 {
        self.checked.stages[Stage::Heads.index()].entered
    }
}

/// One request on 2 workers, its head tasks over `sources`.
fn request(io: &Io, fresh: bool, sources: Sources, units: HeadUnits) -> Said {
    let mut o = opts(io, 2, fresh);
    o.head_sources = sources;
    o.head_units = units;
    let before = stdout(io).len();
    let (ids, checked) = engine_opts(io, o);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().filter(|l| l.contains("error[") || l.contains("warning[") || l.starts_with("note:")).map(str::to_owned).collect();
    lines.sort_unstable();
    Said { ids, lines, checked }
}

type ReadSet = BTreeSet<CellId>;
type TaskUnit = (Name, [u8; 8]);
type UnitReads = BTreeMap<TaskUnit, std::vec::Vec<ReadSet>>;

std::thread_local! {
    // The coordinator joins the workers and takes their lists on the request thread.
    static UNIT_READS: std::cell::RefCell<Option<UnitReads>> = const { std::cell::RefCell::new(None) };
}

impl OnceCounts {
    /// Called on the coordinator before it merges this result into the stored row.
    pub(crate) fn observe_unit_reads(task: Name, unit: [u8; 8], reads: &[CellId]) {
        UNIT_READS.with_borrow_mut(|active| {
            if let Some(all) = active {
                assert!(reads.windows(2).all(|pair| pair[0] < pair[1]), "unsorted or duplicate unit reads for {task:?}, unit {:016x}", u64::from_be_bytes(unit));
                all.entry((task, unit)).or_default().push(reads.iter().copied().collect());
            }
        });
    }
}

fn cold_request(io: &Io, sources: Sources, units: HeadUnits) -> (Said, UnitReads) {
    UNIT_READS.set(Some(UnitReads::new()));
    let said = request(io, false, sources, units);
    (said, UNIT_READS.take().expect("observing this cold request"))
}

/// The cold Job answer supplies the row size and labels. The task/unit pairs come from the
/// fixture's outlines and unit graph, independently of which lists the observer received.
struct HeadRows {
    longest: usize,
    expected: BTreeSet<TaskUnit>,
    lang_by_unit: BTreeMap<[u8; 8], CellId>,
    head_units: BTreeMap<CellId, [u8; 8]>,
    expansions: BTreeMap<TaskUnit, CellId>,
    owners: BTreeMap<Name, Label>,
    parts: BTreeMap<CellId, Label>,
}

impl HeadRows {
    fn cold(io: &Io, files: &[(&str, &str)]) -> Self {
        use crate::store::Store as _;
        use blaze_world::crate_list::CrateGraph as _;

        super::engine_answer_rows_tests::with_store(io, |cx, store, opts| {
            let root = crate::engine_answer::root_name(opts.place.checkout, &opts.tools, cx.frame());
            let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("open the cold pointer").expect("a cold answer").expect("read its pointer");
            let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("open the cold pages").expect("read the cold rows");
            let longest = kept.answer.rows.iter()
                .filter(|(name, _)| name.kind == Kind::Signature)
                .map(|(_, row)| row.reads.len())
                .max().expect("cold Signature rows");
            let mut rows = Self {
                longest, expected: BTreeSet::new(), lang_by_unit: BTreeMap::new(), head_units: BTreeMap::new(),
                expansions: BTreeMap::new(), owners: BTreeMap::new(), parts: BTreeMap::new(),
            };

            // The graph distinguishes units of the same crate. Its crate keys also let us
            // name fine cells (including absent Lang and binding lookups) by their real keys.
            let graph = kept.records[&Name::unit_graph()];
            let held = store.get(cx, graph).expect("open the unit graph").expect("the stored unit graph");
            let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(held.bytes).expect("read the unit graph") else {
                panic!("expected a whole unit graph");
            };
            let list = crate::graph_payload::sections(view.payload).expect("the graph sections").crates().expect("the crate list reads").expect("the fixture has crates");
            for at in 0..list.len() {
                let key = list.key(at);
                let scope = blaze_drive::key::crate_scope_name(&key);
                rows.owners.insert(scope, format!("{}@{:016x}", list.name(at), u64::from_be_bytes(key.prefix())));
                for &(path, text) in files {
                    if list.member(at) && list.root(at).strip_prefix("/0/") == Some(path) {
                        rows.label_member(key, path, text, opts.heap.alloc());
                    }
                }
            }
            // Only a cell the Signature row actually defines can stand for frozen bytes.
            rows.head_units.retain(|id, _| kept.answer.rows[&id.of].defines.iter().any(|(cell, _)| *cell == id.cell));
            rows
        })
    }

    fn label_member(&mut self, key: blaze_id::CrateItemKey, path: &str, text: &str, alloc: blaze_own::Alloc<'_>) {
        use blaze_scope::names_cells::{NameCell, Section, first_16};

        let scope = blaze_drive::key::crate_scope_name(&key);
        let mut part = |cell: NameCell, label: Label| {
            let id = CellId { of: scope, cell: blaze_drive::cells::Cell(cell.key()) };
            self.parts.insert(id, label);
            id
        };
        let krate = key.prefix();
        let module = blaze_scope::task::root_key(key);
        self.lang_by_unit.insert(krate, part(NameCell::Lang { krate }, "Lang".into()));
        part(NameCell::Root { krate }, "Root".into());
        part(NameCell::Incoherent { krate }, "Incoherent".into());
        part(NameCell::Module { module }, "Module(crate root)".into());
        part(NameCell::Item { key: module }, "Item(crate root)".into());
        part(NameCell::Textual { module }, "Textual(crate root)".into());
        part(NameCell::Beginning { first: first_16(module) }, "Beginning(crate root)".into());
        for section in [Section::Names, Section::Items, Section::Expansions] {
            part(NameCell::Whole { krate, section }, format!("Whole({section:?})"));
        }
        // A path can read a binding absent from this crate, including an extern crate name.
        let words = [A_LIB, B_LIB].into_iter().flat_map(|source| source.split(|c: char| !c.is_alphanumeric() && c != '_'));
        for word in words.filter(|word| !word.is_empty()).collect::<BTreeSet<_>>() {
            for ns in blaze_surface::Namespace::ALL {
                let name = blaze_scope::cell_key::cell_key(ns, word);
                part(NameCell::Binding { module, name }, format!("Binding({ns:?} {word})"));
            }
        }
        let file = Name::file(path.as_bytes());
        self.owners.insert(file, path.into());
        let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, alloc);
        for item in outline.items().iter().filter(|item| crate::signature::is_signature_entry(&outline, item)) {
            let word = crate::item_name::shown_name(item, text);
            let name = crate::signature::signature_name(file.subject, &outline, item, text);
            self.expected.insert((name, krate));
            self.owners.insert(name, format!("{path}::{word}"));
            // The top-level fixtures have no duplicate names or calls.
            let tag = format!("{:?}", item.kind());
            let key = blaze_scope::task::key_of(key, module, &tag, item.name.map_or("", |range| item.text(text, range)), 0);
            let want = match item.kind() {
                blaze_surface::ItemKind::Struct => Want::Adt(key),
                blaze_surface::ItemKind::Function => Want::Fn(key),
                blaze_surface::ItemKind::MacroCall => {
                    let expansion = NameCell::Expansion { call: key };
                    self.expansions.insert((name, krate), CellId { of: scope, cell: blaze_drive::cells::Cell(expansion.key()) });
                    part(expansion, format!("Expansion({word})"));
                    continue;
                }
                kind => panic!("unexpected fixture signature {kind:?} at {path}::{word}"),
            };
            let cell = crate::engine_heads::head_cell(want, word, None, crate::engine_tasks::WHOLE);
            self.head_units.insert(CellId { of: name, cell }, krate);
            for (cell, kind) in [
                (NameCell::Item { key }, "Item"), (NameCell::Members { owner: key }, "Members"),
                (NameCell::MemberOf { member: key }, "MemberOf"), (NameCell::VariantOf { variant: key }, "VariantOf"),
                (NameCell::Variants { owner: key }, "Variants"), (NameCell::Beginning { first: first_16(key) }, "Beginning"),
            ] {
                part(cell, format!("{kind}({word})"));
            }
        }
    }

    fn owner(&self, name: Name) -> Label {
        self.owners.get(&name).cloned().unwrap_or_else(|| format!("{:?}@{}", name.kind, name.subject))
    }

    fn describe(&self, id: CellId) -> Label {
        use crate::engine_tasks::{HEADS, SURFACE, WHOLE};
        let kind = match id.cell {
            WHOLE => "WHOLE",
            HEADS => "HEADS",
            SURFACE => "SURFACE",
            _ if self.head_units.contains_key(&id) => "Signature head cell",
            _ => self.parts.get(&id).map(Label::as_str).unwrap_or("cell"),
        };
        format!("{} {kind} [{:032x}]", self.owner(id.of), u128::from_be_bytes(id.cell.0))
    }
}

fn check_counts(rows: &HeadRows, sources: Sources, units: &UnitReads, counts: OnceCounts) {
    assert_eq!(units.keys().copied().collect::<BTreeSet<_>>(), rows.expected, "{sources:?} cold task/unit coverage");
    let lengths = units.values().flatten().map(ReadSet::len);
    let most = lengths.clone().max().expect("the fixture has head tasks");
    assert_eq!(most, counts.read_most as usize, "{sources:?} longest unit list");
    assert_eq!(lengths.clone().count(), counts.read_lists as usize, "{sources:?} unit lists counted");
    assert_eq!(lengths.map(|n| n as u64).sum::<u64>(), counts.read_cells, "{sources:?} total cells counted");
}

struct ReadDifference {
    cutoffs: ReadSet,
    witnesses: ReadSet,
    unexpected: ReadSet,
    missing: ReadSet,
}

fn read_difference(rows: &HeadRows, head: &UnitReads, (task, unit): TaskUnit, h: &ReadSet, j: &ReadSet) -> ReadDifference {
    // reads_from stops at HeadAt::Cell for another entry in the same crate. The fixture's
    // head_cell includes the unit prefix. Neither an Item lookup nor a file HEADS read qualifies.
    let cutoffs: ReadSet = j.iter().copied().filter(|id| id.of != task && rows.head_units.get(id) == Some(&unit)).collect();
    let unexpected = j.difference(h).filter(|id| !cutoffs.contains(*id)).copied().collect();
    let mut missing = ReadSet::new();
    let mut witnesses = ReadSet::new();
    for id in h.difference(j) {
        let mut covered = false;
        for &cutoff in &cutoffs {
            // Sources::Head already follows the freeze transitively. Only look through an
            // actual Job edge, in its unit. If the producer ran twice, require the witness
            // in both observations rather than borrowing a read peculiar to another entry.
            let producer = &head[&(cutoff.of, unit)];
            if producer.iter().all(|reads| reads.contains(id)) {
                witnesses.insert(cutoff);
                covered = true;
            }
        }
        if !covered {
            missing.insert(*id);
        }
    }
    ReadDifference { cutoffs, witnesses, unexpected, missing }
}

fn compare_unit_reads(rows: &HeadRows, head: &UnitReads, job: &UnitReads, head_most: u32, job_most: u32) {
    println!("head reads: longest stored Job row {}, longest single unit: Head {head_most}, Job {job_most}", rows.longest);
    let mut tasks = BTreeSet::new();
    let mut with_cutoff = BTreeSet::new();
    let mut compared = 0;
    let mut cut_entries = 0;
    let mut largest = 0;
    let mut largest_at = Label::from("no Head-only reads");
    let mut violations = std::vec::Vec::new();
    for (&(task, unit), job_lists) in job {
        tasks.insert(task);
        let head_lists = &head[&(task, unit)];
        let name = rows.owner(task);
        let prefix = u64::from_be_bytes(unit);
        assert_eq!(head_lists.len(), job_lists.len(), "{name}, unit {prefix:016x}: number of entries");
        for (entry, (h, j)) in head_lists.iter().zip(job_lists).enumerate() {
            compared += 1;
            let at = format!("{name}, unit {prefix:016x}, entry {}", entry + 1);
            // Every head in this fixture uses the unit's receiver pointers, even when
            // another job froze it. A same-crate cutoff cannot cover this direct read.
            let lang = rows.lang_by_unit[&unit];
            if !j.contains(&lang) {
                violations.push(format!("{at}: missing direct read of {}", rows.describe(lang)));
            }
            let diff = read_difference(rows, head, (task, unit), h, j);
            if !diff.cutoffs.is_empty() {
                with_cutoff.insert(task);
                cut_entries += 1;
            }
            for &id in &diff.cutoffs {
                let side = if h.contains(&id) { "shared" } else { "Job only" };
                println!("head task {at}: {side} cutoff {}", rows.describe(id));
            }
            let omitted = h.difference(j).count();
            if omitted > largest {
                largest = omitted;
                let behind: std::vec::Vec<_> = diff.witnesses.iter().map(|id| rows.describe(*id)).collect();
                largest_at = format!("{at}, behind {behind:?}, {} uncovered", diff.missing.len());
            }
            for id in diff.unexpected {
                violations.push(format!("{at}: unexpected Job-only {}", rows.describe(id)));
            }
            for id in diff.missing {
                violations.push(format!("{at}: missing {} outside any same-crate Job cutoff", rows.describe(id)));
            }
        }
    }
    println!("head reads: {} tasks compared ({} task/unit pairs, {compared} entries), {} tasks with a cutoff ({cut_entries} entries), largest Head-only set {largest}: {largest_at}", tasks.len(), job.len(), with_cutoff.len());
    assert!(violations.is_empty(), "cold head read violations:\n{}", violations.join("\n"));
}

/// The row after an edit: the incremental answer against a fresh check's (rule 21: no row is
/// fast by being stale), printed with its counts.
fn row(io: &Io, name: &str) -> Said {
    let now = request(io, false, Sources::Job, HeadUnits::Each);
    let fresh = request(io, true, Sources::Job, HeadUnits::Each);
    let o = now.once();
    println!(
        "row {name}: heads {} entered; cells {}, frozen {}, twice {}, found {}; reads a head task: mean {}, most {}; equal a fresh check: {}; {}",
        now.heads(),
        o.cells,
        o.frozen,
        o.twice,
        o.found,
        o.read_mean(),
        o.read_most,
        now.same_answer(&fresh),
        entries(&now.checked)
    );
    assert!(now.same_answer(&fresh), "row {name}: the answer after the edit is not a fresh check's: ids {:?} against {:?}, lines {:?} against {:?}", now.ids, fresh.ids, now.lines, fresh.lines);
    now
}

/// For every cold task, unit and entry, each Job-only read must be a same-crate head cutoff,
/// and each Head-only read must occur in the transitive Head reads of a cutoff present in Job.
/// As engine_heads_once.rs states, “A head read at its head task's cell (a head of the same
/// crate) ends there: its cell's digest is its frozen bytes' meaning.”
/// “So when anything a read head was made from moves, either that head's bytes move (its
/// cell's digest moves and the reader is entered at the next wave) or they do not, and then the
/// reader's bytes cannot move either (early cutoff).”
#[test]
fn each_head_is_frozen_once_and_its_readers_follow_its_meaning() {
    // Before: a source a head (the measure's baseline), on a world of its own.
    let before_io = two_members(23);
    let (before, head_units) = cold_request(&before_io, Sources::Head, HeadUnits::Each);
    let io = two_members(23);
    let (cold, job_units) = cold_request(&io, Sources::Job, HeadUnits::Each);
    let rows = HeadRows::cold(&io, &[("a/src/lib.rs", A_LIB), ("b/src/lib.rs", B_LIB)]);
    let fresh = request(&io, true, Sources::Job, HeadUnits::Each);
    assert!(cold.same_answer(&fresh), "cold: the table's answer is not a fresh check's: {:?} against {:?}", cold.lines, fresh.lines);
    assert!(cold.same_answer(&before), "cold: the table's answer is not a source a head's: {:?} against {:?}", cold.lines, before.lines);
    let o = cold.once();
    assert!(o.cells > 0, "cold: the table holds no cell: {o:?}");
    // The floor: each cell frozen once; a race between two jobs that both missed a cell is the
    // only second freeze, and it is counted.
    assert_eq!(o.frozen, o.cells + o.twice, "ORACLE: a cell was frozen more than once outside a race: {o:?}");
    assert_eq!(o.reads_unequal, 0, "two freezes of one cell kept other reads: {o:?}");
    // (`Checked` holds no profile text: the table's profile line is read on the real run.)
    let b = before.once();
    println!(
        "heads once before (a source a head): reads a head task mean {}, most {} (cells), lowered {}, constants evaluated {}; after (the table): mean {}, most {}, lowered {}, constants evaluated {} (distinct {})",
        b.read_mean(),
        b.read_most,
        b.lowered,
        b.evaluated,
        o.read_mean(),
        o.read_most,
        o.lowered,
        o.evaluated,
        o.consts
    );
    check_counts(&rows, Sources::Head, &head_units, b);
    check_counts(&rows, Sources::Job, &job_units, o);
    compare_unit_reads(&rows, &head_units, &job_units, b.read_most, o.read_most);
    assert!(o.read_mean() <= b.read_mean(), "ORACLE: the table's read lists are longer in the mean than a source a head's: {o:?} against {b:?}");
    assert!(o.lowered <= b.lowered, "ORACLE: the table lowered more than a source a head: {o:?} against {b:?}");
    println!("heads once cold: {o:?}; {}", entries(&cold.checked));

    // A body-only edit in `a`: NO head task is entered, `make`'s own neither (lane CX17: a
    // fn's head key holds its signature's text, not its body's; before it the head reran and
    // froze equal bytes, the EARLY CUTOFF this row used to show. OPEN, lane CX19: an edit that
    // moves a head's key and not its meaning, to show the cutoff again).
    write(&io, "a/src/lib.rs", A_LIB_BODY);
    let body = row(&io, "a body-only edit in a");
    assert_eq!(body.heads(), 0, "ORACLE: a body-only edit entered a head task");

    // `Point` gains a field: its head moves, and `b`'s heads that read it are entered.
    write(&io, "a/src/lib.rs", A_LIB_FIELD);
    let field = row(&io, "a's struct gains a field");
    assert!(field.heads() >= 2, "ORACLE: `Point` gained a field and no head task of `b` that reads it was entered ({} entered)", field.heads());
}

/// CONTROL (rule 23). This call writes a macro definition, so its task reads the expansion
/// record and freezes no head. Removing that Job read leaves no cutoff that can cover it.
#[test]
fn the_cutoff_oracle_catches_a_dropped_expansion_read() {
    const LIB: &str = "macro_rules! later { () => { macro_rules! nothing { () => {}; } }; }\nlater!();\n";
    let fixture = || {
        let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
        write(&io, "Cargo.toml", A_MANIFEST);
        write(&io, "src/lib.rs", LIB);
        io
    };
    let (baseline, head) = cold_request(&fixture(), Sources::Head, HeadUnits::Each);
    for units in [HeadUnits::Each, HeadUnits::NoExpansionReads] {
        let io = fixture();
        let (answer, job) = cold_request(&io, Sources::Job, units);
        let rows = HeadRows::cold(&io, &[("src/lib.rs", LIB)]);
        check_counts(&rows, Sources::Head, &head, baseline.once());
        check_counts(&rows, Sources::Job, &job, answer.once());
        for (&task_unit, lists) in &job {
            let baseline = &head[&task_unit];
            let expansion = rows.expansions[&task_unit];
            assert_eq!(baseline.len(), lists.len(), "control entries for {task_unit:?}");
            for (h, j) in baseline.iter().zip(lists) {
                assert!(h.contains(&expansion), "the call reads its expansion in {task_unit:?}");
                let diff = read_difference(&rows, &head, task_unit, h, j);
                assert!(diff.unexpected.is_empty() && diff.cutoffs.is_empty(), "{task_unit:?}: {units:?} adds a read or reaches another head");
                if units == HeadUnits::NoExpansionReads {
                    assert_eq!(diff.missing, ReadSet::from([expansion]), "the oracle must identify the dropped expansion");
                    println!("cutoff control caught missing {}", rows.describe(expansion));
                } else {
                    assert!(diff.missing.is_empty(), "{task_unit:?}: the rule misses {:?}", diff.missing);
                }
            }
        }
    }
}

// THE RACE.

/// One cell of the race, and its two freezes' bytes.
fn cell() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("blaze-cli heads once race", b"the cell")))
}

const ONE: &[u8] = b"the first freeze's bytes";
const OTHER: &[u8] = b"another freeze's bytes!!";

fn entry(bytes: &'static [u8]) -> Entry {
    Entry { want: cell(), member: false, bytes, out: blaze_ids::ContentId::of("blaze-cli heads once race", bytes), reads: &[], heads: &[], consts: &[], record: None }
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Two workers, each a look then (on a miss) a put of its freeze, interleaved as the tape draws;
/// the second's bytes equal or other. The oracle over the table after both.
fn race(tape: &Tape, plant: OncePlant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let table = HeadsOnce::new(heap, plant, Sources::Job);
    let other = tape.bool("the second freeze made other bytes");
    let bytes = [ONE, if other { OTHER } else { ONE }];
    // Each worker's next step: 0 look, 1 put, 2 done.
    let mut step = [0u8; 2];
    let mut missed = [false; 2];
    let mut puts: std::vec::Vec<(usize, Result<Put, OnceError>)> = std::vec::Vec::new();
    while step != [2, 2] {
        let w = match (step[0] < 2, step[1] < 2) {
            (true, true) => usize::from(tape.bool("the second worker steps now")),
            (true, false) => 0,
            _ => 1,
        };
        match step[w] {
            0 => {
                missed[w] = table.look(cell()).is_none();
                step[w] = if missed[w] { 1 } else { 2 };
            }
            _ => {
                puts.push((w, table.put(entry(bytes[w]))));
                step[w] = 2;
            }
        }
    }
    let counts = table.counts();
    let oracle = |what: std::string::String| Err(Broke::Oracle(what));
    if counts.cells != 1 {
        return oracle(std::format!("the table holds {} entries of one cell", counts.cells));
    }
    let Some((first, _)) = puts.first().copied() else {
        return Err(Broke::Harness(std::string::String::from("no worker put")));
    };
    let held = table.look(cell()).map(|e| e.bytes);
    if held != Some(bytes[first]) {
        return oracle(std::string::String::from("the table holds other bytes than the first freeze's"));
    }
    match puts.as_slice() {
        [(_, Ok(Put::Kept))] if counts.twice == 0 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Ok(Put::Twice))] if !other && counts.twice == 1 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Err(OnceError::Unequal { .. }))] if other && counts.twice == 0 => Ok(()),
        seen => oracle(std::format!("the puts answered {seen:?} with twice {} (other bytes: {other})", counts.twice)),
    }
}

/// Expected: passes over every interleaving the tape draws.
#[test]
fn two_freezes_of_one_cell_leave_one_entry() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| race(tape, OncePlant::Rule, heap));
}

/// CONTROL (rule 23): a second write that wins is caught by the same oracle.
#[test]
fn the_race_oracle_catches_a_second_write_that_wins() {
    let heap = heap();
    control(Config::cases(2000), OncePlant::Rule, OncePlant::SecondWins, "other bytes than the first freeze's", |tape, plant| race(tape, plant, heap));
}
