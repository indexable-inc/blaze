//! Rust's rule: a program's `main` runs in the program's own process, which ends with its
//! status (the Reference, "Crates and source files": "the main function"; the library's
//! process.rs: "`exit` ... the exit code is passed through to the underlying OS"). The
//! parent's part is the code it hands that process: a function's slot is told once before
//! its record, every later call of it gets the same slot, a symbol with no record is refused
//! by name, never answered with another's code, and a function is built only when the program
//! first calls it (run.md D124), from a body the gate left clean.
//!
//! The lazy parent's model program: `main` calls `a` and `b`, `a` calls `leaf`. An entry is its
//! declared rows ([`ToyForm`]), its record the entry's bytes; the maker counts its calls.

use std::cell::Cell;
use std::collections::BTreeSet;

use blaze_ladder::body_ids::{BodyId, Read};
use blaze_ladder::code_records::MemoryStore;
use blaze_ladder::run::special::EntryForm as LineForm;
use blaze_ladder::run::special::EntryLine;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

use super::*;
use crate::run_ids::Missing;

fn frames(bytes: &[u8]) -> std::vec::Vec<std::string::String> {
    let mut said = std::vec::Vec::new();
    let mut at = 0;
    while let Some((item, len)) = split(&bytes[at..]).expect("a frame") {
        let (_, down) = Down::decode(item).expect("a run wire message");
        said.push(match down {
            Down::Declare { pairs } => {
                let pairs: std::vec::Vec<_> = pairs.iter().map(|it| (it.symbol.0[0], it.slot.0)).collect();
                std::format!("Declare {pairs:?}")
            }
            Down::Publish { slot, record, .. } => std::format!("Publish {} {}", slot.0, record.0[0]),
            Down::Failed { why, .. } => std::format!("Failed {why:?}"),
            Down::Personality { slot } => std::format!("Personality {}", slot.0),
            Down::Native { library, words } => std::format!("Native {} {}", library.0[0], words.len()),
        });
        at += len;
    }
    said
}

fn need(need: Needed) -> std::vec::Vec<u8> {
    let mut out = std::vec::Vec::new();
    Up::Need { need, thread: blaze_launch::Thread(1) }.encode(7, &mut out).expect("a need encodes");
    out
}

/// A fixed table: symbol 1 has record 10, symbol 2 record 20, each declaring nothing.
struct Fixed;

impl Resolve for Fixed {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let n = symbol.0[0];
        Ok(match (n == 1 || n == 2) && symbol.0 == [n; 32] {
            true => Resolved::Record { key: CodeKey(blaze_ids::ContentId([n; 32])), record: RecordId([n * 10; 32]), declares: &[] },
            false => Resolved::Unknown,
        })
    }
}

/// Prints "run_parent: declared once, slot 0 published twice, unknown symbol Unresolved".
/// A parent that declared on every need, numbered a second slot for one symbol, or answered
/// an unknown symbol with a record fails it.
#[test]
fn a_symbol_is_declared_once_and_an_unknown_one_refused() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut needs = Needs::new(alloc);
    let mut inbox = need(Needed::Symbol(Symbol([1; 32])));
    inbox.extend_from_slice(&need(Needed::Slot(Slot(0))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([9; 32]))));
    let half = need(Needed::Symbol(Symbol([2; 32])));
    inbox.extend_from_slice(&half[..3]);
    let mut out = std::vec::Vec::new();
    let taken = needs.serve(&mut Fixed, &inbox, &mut out).expect("served");
    assert_eq!(taken, inbox.len() - 3, "a frame not all here is left for the next read");
    assert_eq!(frames(&out), ["Declare [(1, 0)]", "Publish 0 10", "Publish 0 10", "Failed Unresolved"]);
    assert_eq!(status_of(Ended::Exited(3)), Some(3));
    assert_eq!(status_of(Ended::Signaled(9)), Some(137));
    assert_eq!(status_of(Ended::Deadline), None);
    std::println!("run_parent: declared once, slot 0 published twice, unknown symbol Unresolved");
}

/// A `Resolve` over a fixed table that counts each symbol's asks: symbol 1's record
/// declares symbols 2 and 3; symbol 2's declares 1 (a cycle); symbol 9 is unknown.
struct Counting {
    asks: std::collections::BTreeMap<u8, u32>,
    one: [Symbol; 2],
    two: [Symbol; 1],
}

impl Resolve for Counting {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        *self.asks.entry(symbol.0[0]).or_insert(0) += 1;
        let key = CodeKey(blaze_ids::ContentId([symbol.0[0]; 32]));
        Ok(match symbol.0[0] {
            1 => Resolved::Record { key, record: RecordId([10; 32]), declares: &self.one },
            2 => Resolved::Record { key, record: RecordId([20; 32]), declares: &self.two },
            3 => Resolved::Record { key, record: RecordId([30; 32]), declares: &[] },
            _ => Resolved::Unknown,
        })
    }
}

/// Asserts: (1) each symbol is resolved ONCE however many needs name it, by symbol or by
/// slot, the unknown one included; (2) a record's own slot and the slots of every symbol it
/// declares go out in one `Declare` BEFORE its `Publish`, and a symbol already declared is
/// never declared again; (3) the first-answer order is 1 then 2, each once, with its key and
/// record; (4) a start list placed ahead declares every row in one `Declare`, then publishes
/// each in order, and a need of a placed row asks the resolver nothing.
/// Prints "run_parent resolve: 1, 2 and 9 asked once each; Declare before Publish; ahead rows ask nothing".
#[test]
fn a_symbol_is_resolved_once_and_declared_before_published() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut counting = Counting { asks: std::collections::BTreeMap::new(), one: [Symbol([2; 32]), Symbol([3; 32])], two: [Symbol([1; 32])] };
    let mut needs = Needs::new(alloc);
    let mut inbox = need(Needed::Symbol(Symbol([1; 32])));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([1; 32]))));
    inbox.extend_from_slice(&need(Needed::Slot(Slot(0))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([9; 32]))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([9; 32]))));
    inbox.extend_from_slice(&need(Needed::Slot(Slot(1))));
    inbox.extend_from_slice(&need(Needed::Slot(Slot(1))));
    let mut out = std::vec::Vec::new();
    let taken = needs.serve(&mut counting, &inbox, &mut out).expect("served");
    assert_eq!(taken, inbox.len());
    assert_eq!(
        frames(&out),
        ["Declare [(1, 0), (2, 1), (3, 2)]", "Publish 0 10", "Publish 0 10", "Publish 0 10", "Failed Unresolved", "Failed Unresolved", "Publish 1 20", "Publish 1 20"],
        "a record's slots are told before it, once"
    );
    assert_eq!(counting.asks.iter().map(|(s, n)| (*s, *n)).collect::<std::vec::Vec<_>>(), [(1, 1), (2, 1), (9, 1)], "a symbol was resolved more than once, or 3 (never needed) was resolved");
    let first: std::vec::Vec<(u8, u8, u8)> = needs.first().iter().map(|row| (row.symbol.0[0], row.key.0.0[0], row.record.0[0])).collect();
    assert_eq!(first, [(1, 1, 10), (2, 2, 20)], "the first-answer order");

    let rows = [
        Row { symbol: Symbol([5; 32]), key: CodeKey(blaze_ids::ContentId([5; 32])), record: RecordId([50; 32]) },
        Row { symbol: Symbol([6; 32]), key: CodeKey(blaze_ids::ContentId([6; 32])), record: RecordId([60; 32]) },
    ];
    let mut ahead = Needs::new(alloc);
    let mut none = Counting { asks: std::collections::BTreeMap::new(), one: [Symbol([2; 32]), Symbol([3; 32])], two: [Symbol([1; 32])] };
    let mut out = std::vec::Vec::new();
    ahead.place_ahead(&rows, &mut out).expect("placed");
    ahead.serve(&mut none, &need(Needed::Symbol(Symbol([6; 32]))), &mut out).expect("served");
    assert_eq!(frames(&out), ["Declare [(5, 0), (6, 1)]", "Publish 0 50", "Publish 1 60", "Publish 1 60"]);
    assert!(none.asks.is_empty(), "a placed row was resolved again: {:?}", none.asks);
    assert_eq!(ahead.first(), &rows[..], "placed rows are this run's first answers, in the list's order");
    std::println!("run_parent resolve: 1, 2 and 9 asked once each; Declare before Publish; ahead rows ask nothing");
}

// The lazy parent's model.

const MAIN: u128 = 0xA001;
const A: u128 = 0xA002;
const B: u128 = 0xA003;
const LEAF: u128 = 0xA004;

/// Each fn's callees.
const CALLS: [(u128, &[u128]); 4] = [(MAIN, &[A, B]), (A, &[LEAF]), (B, &[]), (LEAF, &[])];

/// A fn's instance name with no arguments: its key and a count of 0 (symbols.rs).
fn name_of(item: u128) -> [u8; 20] {
    let mut name = [0u8; 20];
    name[..16].copy_from_slice(&item.to_le_bytes());
    name
}

fn symbol_of(item: u128) -> Symbol {
    Symbol(blaze_ladder::symbols::fn_symbol_of_name(&name_of(item)).0)
}

/// The model's personality routine: named before anything is published, never called (the
/// model never throws), so no fn of [`CALLS`].
const PERSONALITY: u128 = 0xA00F;

/// The run's roots: `main` alone, called directly; the model is built to unwind, so its head
/// names [`PERSONALITY`].
struct ToyStart {
    main: [u8; 20],
}

impl Start for ToyStart {
    fn head(&self) -> StartHead {
        StartHead { line: EntryLine { form: LineForm::Unit, symbol: CodeSymbol(symbol_of(MAIN).0) }, personality: Some(CodeSymbol(symbol_of(PERSONALITY).0)) }
    }

    fn root_count(&self) -> usize {
        1
    }

    fn root_row(&self, _i: usize) -> (ItemKey, &[u8]) {
        (ItemKey(MAIN), &self.main)
    }

    fn maker_entry(&self) -> Option<ItemKey> {
        Some(ItemKey(MAIN))
    }

    fn special(&self, _symbol: &CodeSymbol, _builder: &Builder, _store: &mut dyn CodeStore, _alloc: Alloc<'_>) -> Result<Option<SpecialMade>, ParentError> {
        Ok(None)
    }
}

/// Every fn a workspace body, with a BodyCheck record that read one fact; `clean` is what the
/// gate left.
struct ToyIds {
    reads: [Read; 1],
    clean: bool,
}

const FACT: [u8; 32] = [0xfa; 32];
const DIGEST: [u8; 32] = [0xd1; 32];

impl BodyIds for ToyIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        CALLS.iter().any(|(f, _)| *f == item.0).then(|| BodyId { key: blaze_ids::ContentId::of("run parent test body", &item.0.to_le_bytes()), reads: &self.reads })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == FACT).then_some(DIGEST)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: FACT, digest: DIGEST })
    }
}

impl Gate for ToyIds {
    fn grain(&self, _item: ItemKey) -> Option<Grain> {
        Some(Grain::Workspace)
    }

    fn checked(&self, _item: ItemKey) -> Option<Checked> {
        Some(match self.clean {
            true => Checked::Clean,
            false => Checked::Absent(Missing::NoRecord),
        })
    }
}

/// The maker: an entry is its declared rows, per row the tabled fn (16), the name's length
/// (4) and the name.
struct ToyMaker<'t> {
    calls: &'t Cell<usize>,
}

impl Make for ToyMaker<'_> {
    type Error = RunError;

    fn make(&mut self, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.calls.set(self.calls.get() + 1);
        let (_, callees) = CALLS.iter().find(|(f, _)| *f == tabled.0).ok_or(RunError::OneBody { key: tabled, why: "no such fn in the model" })?;
        for &callee in callees.iter() {
            let name = name_of(callee);
            entry.extend_from_slice(&callee.to_le_bytes());
            entry.extend_from_slice(&(name.len() as u32).to_le_bytes());
            entry.extend_from_slice(&name);
        }
        // The entry's own mark, so two leaves' records differ.
        entry.extend_from_slice(&tabled.0.to_le_bytes());
        Ok(())
    }
}

struct ToyForm;

impl Form for ToyForm {
    fn declared(&self, entry: &[u8], out: &mut DeclaredList<'_>) -> Result<(), EntryError> {
        // The rows, then the 16-byte mark.
        let mut at = 0;
        while at + 16 < entry.len() {
            let mut wide = [0u8; 16];
            wide.copy_from_slice(&entry[at..at + 16]);
            let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]) as usize;
            out.push(ItemKey(u128::from_le_bytes(wide)), &entry[at + 20..at + 20 + len]);
            at += 20 + len;
        }
        Ok(())
    }
}

/// The model's front end: what was made of it is counted.
struct ToyFront<'t> {
    calls: &'t Cell<usize>,
    starts: &'t Cell<usize>,
    clean: bool,
}

impl<'t> Front for ToyFront<'t> {
    type Start = ToyStart;
    type Ids = ToyIds;
    type Maker = ToyMaker<'t>;

    fn start(&mut self) -> Result<ToyStart, ParentError> {
        self.starts.set(self.starts.get() + 1);
        Ok(ToyStart { main: name_of(MAIN) })
    }

    fn ids(&mut self) -> Result<ToyIds, ParentError> {
        Ok(ToyIds { reads: [Read { fact: FACT, digest: DIGEST }], clean: self.clean })
    }

    fn maker(&mut self, _start: &ToyStart) -> Result<ToyMaker<'t>, ParentError> {
        Ok(ToyMaker { calls: self.calls })
    }

    fn form(&self) -> &dyn Form {
        &ToyForm
    }

    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        Ok(entry)
    }
}

/// The records folder: the ids filed.
#[derive(Default)]
struct ToyFolder {
    held: BTreeSet<[u8; 32]>,
}

impl Folder for &mut ToyFolder {
    fn holds(&self, record: &RecordId) -> Result<bool, ParentError> {
        Ok(self.held.contains(&record.0))
    }

    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError> {
        let id = blaze_blake3::hash(record);
        self.held.insert(id);
        Ok(RecordId(id))
    }
}

fn builder() -> Builder {
    Builder {
        isa: Isa { os: Os::MacOs, lse: true },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::On, blaze_ladder::run::Panic::Unwind),
        toolchain: blaze_toolchain::PINNED,
        producer: blaze_ids::producer::Producers::for_tests(b"run parent test builder").get::<CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn symbols(declares: &[Symbol]) -> std::vec::Vec<Symbol> {
    declares.to_vec()
}

/// Asserts: (1) the first run's resolve of `main` makes ONE instance (the maker called once),
/// files its record, and declares `a` and `b` without making them; (2) a second run over the
/// same store finds `main`'s entry and makes nothing; (3) the closing line counts each. A
/// parent that walked ahead (made `a`, `b` or `leaf` before they are asked) fails (1); one that
/// keyed nothing fails (2).
/// Prints "lazy parent: first run made 1 of 4 reached-or-declared, second run found 1 made 0".
#[test]
fn the_entry_makes_one_instance_and_a_second_run_makes_none() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));

    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    assert!(lazy.is_cold(), "the front end was made before any need");
    let (record, declares) = match lazy.resolve(&symbol_of(MAIN)).expect("main resolves") {
        Resolved::Record { record, declares, .. } => (record, symbols(declares)),
        Resolved::Unknown => panic!("main is unknown"),
    };
    assert_eq!(declares, [symbol_of(A), symbol_of(B)], "main's record declares a and b, in its entry's order");
    assert_eq!(calls.get(), 1, "the maker made more than main: the walk went ahead of what was asked");
    let first = lazy.tally();
    assert_eq!(first, Tally { reached: 1, found: 0, made: 1, unkeyed: 0 });
    drop(lazy);
    assert!(folder.held.contains(&record.0), "main's record was published without its file");
    assert_eq!(store.puts, 1, "main's entry was not put, or another was");

    let mut again = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    let second = match again.resolve(&symbol_of(MAIN)).expect("main resolves") {
        Resolved::Record { record: same, declares, .. } => {
            assert_eq!(same, record, "the found record is another than the made one");
            assert_eq!(symbols(declares), [symbol_of(A), symbol_of(B)]);
            again.tally()
        }
        Resolved::Unknown => panic!("main is unknown on the second run"),
    };
    assert_eq!(second, Tally { reached: 1, found: 1, made: 0, unkeyed: 0 });
    assert_eq!(calls.get(), 1, "the second run made an instance its store holds");
    std::println!("lazy parent: first run {first}; second run {second}");
}

/// Asserts: a workspace body whose check is `Absent` today is refused by name
/// (`NotChecked` naming `main`) and the maker is never called; an unknown symbol is `Unknown`.
/// A parent that built an unchecked body, or answered an unknown symbol with a record, fails it.
/// Prints "lazy parent: an unchecked body refused by name, an unknown symbol Unknown".
#[test]
fn an_absent_workspace_body_is_refused_and_an_unknown_symbol_is_unknown() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: false }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(MAIN)) {
        Err(ParentError::NotChecked { function, symbol, why }) => {
            assert_eq!((function, symbol, why), (ItemKey(MAIN), symbol_of(MAIN).0, Some(Checked::Absent(Missing::NoRecord))));
        }
        other => panic!("an unchecked body was answered: {other:?}"),
    }
    assert_eq!(calls.get(), 0, "an unchecked body was built");
    drop(lazy);
    // Unknown only once the walk from the roots has stepped every row and no record declares it.
    let mut clean = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    assert!(matches!(clean.resolve(&Symbol([9; 32])), Ok(Resolved::Unknown)), "an unknown symbol was answered with a record");
    assert_eq!(calls.get(), 4, "the walk from the roots did not step every row before answering Unknown");
    std::println!("lazy parent: an unchecked body refused by name, an unknown symbol Unknown");
}

/// Asserts (UNWIND.md 3.3, piece P3): with no start list the head comes from the roots, and the
/// personality routine is named before anything else is written: its slot's `Declare`, then
/// `Personality` naming that slot, then `main`'s answer under the next slot. Control: a head
/// naming no routine (built `panic = "abort"`) writes nothing.
/// Prints "lazy parent: the personality named before the first publish; none for abort".
#[test]
fn the_personality_is_named_before_the_first_publish() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut kept: AVec<'_, Row> = Vec::new_in(alloc);
    let mut out = std::vec::Vec::new();
    let head = ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut out).expect("the roots");
    assert_eq!(head.personality, Some(CodeSymbol(symbol_of(PERSONALITY).0)), "the roots' head names no routine");
    needs.serve(&mut lazy, &need(Needed::Symbol(symbol_of(MAIN))), &mut out).expect("served");
    let said = frames(&out);
    assert_eq!(said[..2], [std::format!("Declare [({}, 0)]", symbol_of(PERSONALITY).0[0]), std::string::String::from("Personality 0")], "the personality was not named first: {said:?}");
    let published = said.iter().position(|frame| frame.starts_with("Publish")).expect("main was published");
    assert!(published > 1, "a record was published before the personality was named: {said:?}");
    assert!(said[published].starts_with("Publish 1 "), "main was not published under the slot after the routine's: {said:?}");
    let mut abort = Needs::new(alloc);
    let mut nothing = std::vec::Vec::new();
    abort.name_personality(None, &mut nothing).expect("nothing to name");
    assert!(nothing.is_empty(), "a program built to abort was told a personality: {:?}", frames(&nothing));
    std::println!("lazy parent: the personality named before the first publish; none for abort");
}

/// Asserts: (1) a start list under today's certificate gives the entry line and places its
/// rows with NO front end made (the roots never asked for, the `Option` still `None`), and a
/// need of a placed row asks nothing; (2) a row whose record's file is gone is not placed, and
/// its need is resolved like a first call (the control: without the file check it would be
/// published from a missing file); (3) a callee of a placed record is reached by stepping the
/// placed row (found, not made) and made alone.
/// Prints "lazy parent: today's list placed cold; a row without its file resolved; a placed row's callee made alone".
#[test]
fn a_list_under_todays_certificate_starts_cold() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    // A first run: main resolved, its row the list's.
    let mut first = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    let line = first.head().expect("the roots");
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.serve(&mut first, &need(Needed::Symbol(symbol_of(MAIN))), &mut out).expect("served");
    drop(first);
    let mut rows: std::vec::Vec<Row> = needs.first().to_vec();
    // A row whose file is gone: b, with a record never filed.
    let gone = Row { symbol: symbol_of(B), key: CodeKey(blaze_ids::ContentId([0xb; 32])), record: RecordId([0xee; 32]) };
    rows.push(gone);
    let today = start_list::Certificate([1; 32]);
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode(today, line, &rows, &mut bytes).expect("two rows");
    let start_list::Held::Today(placed) = start_list::read(bytes.as_slice(), alloc).expect("a list").under(today) else { panic!("today's list read as another's") };

    starts.set(0);
    calls.set(0);
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut kept: AVec<'_, Row> = Vec::new_in(alloc);
    let mut out = std::vec::Vec::new();
    let entry = ahead_of(Some(placed), &mut lazy, &mut needs, &mut kept, &mut out).expect("placed");
    assert_eq!(entry, line, "the list's head is not the one written");
    assert_eq!(kept.as_slice(), &rows[..1], "a row whose record's file is gone was placed");
    // The routine named from the list's head, first: its slot declared, then `Personality`,
    // before the placed row's `Publish`.
    let said = frames(&out);
    assert_eq!(said[..2], [std::format!("Declare [({}, 0)]", symbol_of(PERSONALITY).0[0]), std::string::String::from("Personality 0")], "the personality was not named first: {said:?}");
    assert!(said[2..].iter().any(|frame| frame.starts_with("Publish")), "the placed row was not published: {said:?}");
    needs.serve(&mut lazy, &need(Needed::Symbol(symbol_of(MAIN))), &mut out).expect("served");
    assert!(lazy.is_cold(), "a list under today's certificate made the front end");
    assert_eq!(starts.get(), 0);

    // b's file is gone: resolved like a first call.
    needs.serve(&mut lazy, &need(Needed::Symbol(symbol_of(B))), &mut out).expect("served");
    assert!(!lazy.is_cold(), "a row without its file was answered from the list");
    assert_eq!(calls.get(), 1, "b was not made alone (main is found by stepping its placed row)");
    // a, a callee of placed main: made alone.
    needs.serve(&mut lazy, &need(Needed::Symbol(symbol_of(A))), &mut out).expect("served");
    assert_eq!(calls.get(), 2, "a was not made, or leaf was made before it was asked");
    assert_eq!(lazy.tally(), Tally { reached: 3, found: 1, made: 2, unkeyed: 0 });
    std::println!("lazy parent: today's list placed cold; a row without its file resolved; a placed row's callee made alone");
}

/// Asserts (answer (c)): a callee no placed row leads to (`leaf`, asked first, as when the start
/// list's cap broke the chain) is reached by walking forward from the roots (main, then a, which
/// declares it), not answered `Unknown`; each row stepped ahead is served once at its own first
/// call from what its step made (no second make), a third ask of it is `ResolvedTwice`, and a
/// row the walk did not step (b) is made at its call. A parent that answered leaf `Unknown`, or
/// refused a stepped-ahead row as resolved twice, fails it.
/// Prints "lazy parent: an unreached callee walked to from the roots; rows stepped ahead served once".
#[test]
fn an_unreached_callee_is_walked_to_from_the_roots() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(LEAF)).expect("leaf resolves") {
        Resolved::Record { declares, .. } => assert!(declares.is_empty(), "leaf declares nothing"),
        Resolved::Unknown => panic!("an unreached callee was answered Unknown"),
    }
    assert_eq!(calls.get(), 3, "main and a are stepped to reach leaf, then leaf is made");
    match lazy.resolve(&symbol_of(A)).expect("a, stepped ahead, is served") {
        Resolved::Record { declares, .. } => assert_eq!(symbols(declares), [symbol_of(LEAF)]),
        Resolved::Unknown => panic!("a is unknown"),
    }
    match lazy.resolve(&symbol_of(MAIN)).expect("main, stepped ahead, is served") {
        Resolved::Record { declares, .. } => assert_eq!(symbols(declares), [symbol_of(A), symbol_of(B)]),
        Resolved::Unknown => panic!("main is unknown"),
    }
    assert_eq!(calls.get(), 3, "a row stepped ahead was made again at its call");
    assert!(matches!(lazy.resolve(&symbol_of(A)), Err(ParentError::ResolvedTwice { .. })), "a served row was served again");
    assert!(matches!(lazy.resolve(&symbol_of(B)), Ok(Resolved::Record { .. })), "b, never stepped, is made at its call");
    assert_eq!(calls.get(), 4);
    assert_eq!(lazy.tally(), Tally { reached: 4, found: 0, made: 4, unkeyed: 0 });
    std::println!("lazy parent: an unreached callee walked to from the roots; rows stepped ahead served once");
}
