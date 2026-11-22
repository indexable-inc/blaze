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
use crate::run_ids::{Checked, Missing};
use crate::start_list::Line;

/// The certificate the tests' lists are written and replayed under (a planted one: no answer is
/// behind it).
const LISTED: start_list::Certificate = start_list::Certificate::for_tests([0x5e; 32]);

/// The head the tests' lists are written with.
fn list_head() -> StartHead {
    StartHead { line: EntryLine { form: LineForm::Unit, symbol: CodeSymbol([0x5f; 32]) }, personality: None }
}

/// `lines` as a list under [`LISTED`] onto `bytes` (the planted writer, start_list.rs
/// `encode_lines`, so a test can name rows one by one).
pub(super) fn listed(lines: &[Line], bytes: &mut AVec<'_, u8>) {
    start_list::encode_lines(&LISTED, list_head(), lines, bytes).expect("a small list");
}

/// The list in `bytes` read back under its own certificate: today's rows.
pub(super) fn today_rows<'b>(bytes: &'b [u8], alloc: Alloc<'_>) -> TodayRows<'b> {
    let start_list::Held::Today(rows) = start_list::read(bytes, alloc).expect("the list reads").under(&LISTED) else { panic!("the list under its own certificate is not today's") };
    rows
}

/// The groups of the list in `bytes`, under its own certificate.
fn today_groups<'b>(bytes: &'b [u8], alloc: Alloc<'_>) -> std::vec::Vec<TodayGroup<'b>> {
    today_rows(bytes, alloc).groups().collect()
}

/// This run's first answers as rows (the list it writes at exit).
pub(super) fn first_lines(needs: &Needs<'_>) -> std::vec::Vec<Line> {
    needs.first().lines().collect()
}

pub(super) fn frames(bytes: &[u8]) -> std::vec::Vec<std::string::String> {
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
            Down::Positioned { slot, record, values, .. } => std::format!("Positioned {} {} {values:?}", slot.0, record.0[0]),
            Down::Failed { why, .. } => std::format!("Failed {why:?}"),
            Down::Personality { slot } => std::format!("Personality {}", slot.0),
            Down::Native { library, words } => std::format!("Native {} {}", library.0[0], words.len()),
            Down::Data { data } => {
                let data: std::vec::Vec<_> = data.iter().map(|it| (it.symbol.0[0], it.record.0[0])).collect();
                std::format!("Data {data:?}")
            }
            Down::ThreadLocals { descriptor, ender } => std::format!("ThreadLocals {} {}", descriptor.0[0], ender.0[0]),
            Down::Hooks { slot, kind } => std::format!("Hooks {} {kind:?}", slot.0),
            Down::Promote { slot, record, inlined, .. } => std::format!("Promote {} {} {}", slot.0, record.0[0], inlined.len()),
            Down::Bytes { record, at, total, part } => std::format!("Bytes {} {at} {total} {}", record.0[0], part.len()),
        });
        at += len;
    }
    said
}

pub(super) fn need(need: Needed) -> std::vec::Vec<u8> {
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
            true => Resolved::Record { key: CodeKey(blaze_ids::ContentId([n; 32])), record: RecordId([n * 10; 32]), declares: &[], data: &[], named: &[], sent: Sent::NONE },
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
    assert_eq!(status_of(Exit::Exited(3)), 3);
    assert_eq!(status_of(Exit::Signaled(9)), 137);
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
            1 => Resolved::Record { key, record: RecordId([10; 32]), declares: &self.one, data: &[], named: &[], sent: Sent::NONE },
            2 => Resolved::Record { key, record: RecordId([20; 32]), declares: &self.two, data: &[], named: &[], sent: Sent::NONE },
            3 => Resolved::Record { key, record: RecordId([30; 32]), declares: &[], data: &[], named: &[], sent: Sent::NONE },
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
    let first: std::vec::Vec<(u8, u8, u8)> = first_lines(&needs).iter().filter_map(Line::code).map(|row| (row.symbol.0[0], row.key.0.0[0], row.record.0[0])).collect();
    assert_eq!(first, [(1, 1, 10), (2, 2, 20)], "the first-answer order");

    let rows = [
        Line::Code(Row { symbol: Symbol([5; 32]), key: CodeKey(blaze_ids::ContentId([5; 32])), record: RecordId([50; 32]) }),
        Line::Code(Row { symbol: Symbol([6; 32]), key: CodeKey(blaze_ids::ContentId([6; 32])), record: RecordId([60; 32]) }),
    ];
    let mut ahead = Needs::new(alloc);
    let mut none = Counting { asks: std::collections::BTreeMap::new(), one: [Symbol([2; 32]), Symbol([3; 32])], two: [Symbol([1; 32])] };
    let mut out = std::vec::Vec::new();
    let mut list = Vec::new_in(alloc);
    listed(&rows, &mut list);
    ahead.place_ahead(&today_groups(&list, alloc), &mut out).expect("placed");
    ahead.serve(&mut none, &need(Needed::Symbol(Symbol([6; 32]))), &mut out).expect("served");
    assert_eq!(frames(&out), ["Declare [(5, 0), (6, 1)]", "Publish 0 50", "Publish 1 60", "Publish 1 60"]);
    assert!(none.asks.is_empty(), "a placed row was resolved again: {:?}", none.asks);
    assert_eq!(first_lines(&ahead), rows, "placed rows are this run's first answers, in the list's order");
    std::println!("run_parent resolve: 1, 2 and 9 asked once each; Declare before Publish; ahead rows ask nothing");
}

// The lazy parent's model.

const MAIN: u128 = 0xA001;
const A: u128 = 0xA002;
const B: u128 = 0xA003;
pub(super) const LEAF: u128 = 0xA004;

/// The statics model's root: it calls `leaf` and names static `S`.
pub(super) const SHOUT: u128 = 0xA005;
/// Two statics whose values point at each other (a cycle: one batch).
pub(super) const S: u128 = 0xA0E1;
pub(super) const T: u128 = 0xA0E2;

/// The wire model's root: it calls `echo` and `shout`, and names no static.
pub(super) const HUB: u128 = 0xA008;
/// Made on demand (`hub`'s callee), the FIRST record of its run to name `S`.
pub(super) const ECHO: u128 = 0xA007;
/// A fn touching thread-local [`V`]: its entry declares `V` (its template is a row), and its
/// record names `V`'s template and the thread-local descriptor ([`toy_named`]).
const THREADY: u128 = 0xA006;
/// The thread-local `THREADY` touches.
const V: u128 = 0xA0E3;
/// The length that marks a thread-local row in a toy entry (a static's is `u32::MAX`).
const TLS_ROW: u32 = u32::MAX - 1;

/// The thread-locals each item's entry declares.
const THREAD_LOCALS: [(u128, &[u128]); 1] = [(THREADY, &[V])];

/// A fn that makes a `&dyn` of a type: its record names vtable [`W`], whose datum its entry
/// carries (code_records.rs form 6), and calls `leaf` (the vtable's method word's instance).
pub(super) const DYN: u128 = 0xA009;
/// The vtable `DYN` names.
pub(super) const W: u128 = 0xA0E4;
/// The length that marks a vtable row in a toy entry: the vtable's id, its datum the id's bytes.
const VT_ROW: u32 = u32::MAX - 2;

/// The vtables each item's entry carries.
const VTABLES: [(u128, &[u128]); 1] = [(DYN, &[W])];

/// A toy vtable's symbol: its own domain, so it is no static's or fn's.
fn vtable_of(item: u128) -> Symbol {
    let mut bytes = std::vec::Vec::from(&b"toy vtable "[..]);
    bytes.extend_from_slice(&item.to_le_bytes());
    Symbol(blaze_blake3::hash(&bytes))
}

/// Each fn's callees.
const CALLS: [(u128, &[u128]); 9] = [(MAIN, &[A, B]), (A, &[LEAF]), (B, &[]), (LEAF, &[]), (SHOUT, &[LEAF]), (HUB, &[ECHO, SHOUT]), (ECHO, &[LEAF]), (THREADY, &[]), (DYN, &[LEAF])];

/// The statics each item's entry declares (a fn's code names them; a static's value does).
const NAMES: [(u128, &[u128]); 4] = [(SHOUT, &[S]), (S, &[T]), (T, &[S]), (ECHO, &[S])];

/// What a toy record names, the model's stand-in for a code record's `Space::Static` and
/// `Space::Tls` targets (code_records.rs `statics::each_static_named`): its static rows'
/// statics, and for a thread-local row its template and the thread-local descriptor (the
/// emitter's access sequence loads both).
fn toy_named(entry: &[u8]) -> std::vec::Vec<Symbol> {
    let mut named = std::vec::Vec::new();
    let mut at = 0;
    while at + 16 < entry.len() {
        let mut wide = [0u8; 16];
        wide.copy_from_slice(&entry[at..at + 16]);
        let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]);
        if len == u32::MAX {
            named.push(static_of(u128::from_le_bytes(wide)));
            at += 20;
            continue;
        }
        if len == TLS_ROW {
            named.push(tls_of(u128::from_le_bytes(wide)));
            named.push(Symbol(blaze_ladder::symbols::tls_desc_symbol().0));
            at += 20;
            continue;
        }
        if len == VT_ROW {
            named.push(vtable_of(u128::from_le_bytes(wide)));
            at += 20;
            continue;
        }
        at += 20 + len as usize;
    }
    named
}

/// Each vtable row of a toy entry: its id.
fn toy_vtables(entry: &[u8]) -> std::vec::Vec<u128> {
    let mut rows = std::vec::Vec::new();
    let mut at = 0;
    while at + 16 < entry.len() {
        let mut wide = [0u8; 16];
        wide.copy_from_slice(&entry[at..at + 16]);
        let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]);
        if len == VT_ROW {
            rows.push(u128::from_le_bytes(wide));
        }
        at += match len {
            u32::MAX | TLS_ROW | VT_ROW => 20,
            len => 20 + len as usize,
        };
    }
    rows
}

/// Item `item`'s static rows onto `entry`: each the static's key and the length `u32::MAX`;
/// then its thread-local rows, each the key and [`TLS_ROW`].
fn static_rows(item: u128, entry: &mut AVec<'_, u8>) {
    for &(_, statics) in NAMES.iter().filter(|(k, _)| *k == item) {
        for s in statics.iter() {
            entry.extend_from_slice(&s.to_le_bytes());
            entry.extend_from_slice(&u32::MAX.to_le_bytes());
        }
    }
    for &(_, locals) in THREAD_LOCALS.iter().filter(|(k, _)| *k == item) {
        for v in locals.iter() {
            entry.extend_from_slice(&v.to_le_bytes());
            entry.extend_from_slice(&TLS_ROW.to_le_bytes());
        }
    }
    for &(_, tables) in VTABLES.iter().filter(|(k, _)| *k == item) {
        for w in tables.iter() {
            entry.extend_from_slice(&w.to_le_bytes());
            entry.extend_from_slice(&VT_ROW.to_le_bytes());
        }
    }
}

fn tls_of(item: u128) -> Symbol {
    Symbol(blaze_ladder::symbols::tls_symbol(ItemKey(item)).0)
}

pub(super) fn static_of(item: u128) -> Symbol {
    Symbol(blaze_ladder::symbols::static_symbol(ItemKey(item)).0)
}

/// A fn's instance name with no arguments: its key and a count of 0 (symbols.rs).
fn name_of(item: u128) -> [u8; 20] {
    let mut name = [0u8; 20];
    name[..16].copy_from_slice(&item.to_le_bytes());
    name
}

pub(super) fn symbol_of(item: u128) -> Symbol {
    Symbol(blaze_ladder::symbols::fn_symbol_of_name(&name_of(item)).0)
}

/// The model's personality routine: named before anything is published, never called (the
/// model never throws), so no fn of [`CALLS`].
const PERSONALITY: u128 = 0xA00F;

/// The run's roots: `main` alone, called directly; the model is built to unwind, so its head
/// names [`PERSONALITY`].
pub(super) struct ToyStart {
    root: u128,
    main: [u8; 20],
}

impl Start for ToyStart {
    fn head(&self) -> StartHead {
        StartHead { line: EntryLine { form: LineForm::Unit, symbol: CodeSymbol(symbol_of(self.root).0) }, personality: Some(CodeSymbol(symbol_of(PERSONALITY).0)) }
    }

    fn root_count(&self) -> usize {
        1
    }

    fn root_row(&self, _i: usize) -> (ItemKey, &[u8]) {
        (ItemKey(self.root), &self.main)
    }

    fn maker_entry(&self) -> Option<ItemKey> {
        Some(ItemKey(self.root))
    }

    fn special(&self, _symbol: &CodeSymbol, _builder: &Builder, _store: &mut dyn CodeStore, _alloc: Alloc<'_>) -> Result<Option<SpecialMade>, ParentError> {
        Ok(None)
    }
}

/// Every fn a workspace body, with a BodyCheck record that read one fact; `clean` is what the
/// gate left.
pub(super) struct ToyIds {
    reads: [Read; 1],
    clean: bool,
}

const FACT: [u8; 32] = [0xfa; 32];
const DIGEST: [u8; 32] = [0xd1; 32];

impl BodyIds for ToyIds {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        (CALLS.iter().any(|(f, _)| *f == item.0) || NAMES.iter().any(|(k, _)| *k == item.0) || item.0 == V).then(|| BodyId { key: blaze_ids::ContentId::of("run parent test body", &item.0.to_le_bytes()), reads: &self.reads })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == FACT).then_some(DIGEST)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: FACT, digest: DIGEST })
    }
}

/// Every fn a workspace body with a row: clean, or with no record, through the one gate
/// (run_ids.rs `Checked::runs`).
impl Gate for ToyIds {
    fn runnable(&self, item: ItemKey) -> Result<Runnable, NotRunnable> {
        match self.clean {
            true => Checked::Clean,
            false => Checked::Absent(Missing::NoRecord),
        }
        .runs(item)
    }
}

/// The maker: an entry is its declared rows, per row the tabled fn (16), the name's length
/// (4) and the name.
pub(super) struct ToyMaker<'t> {
    calls: &'t Cell<usize>,
    /// Whether the bodies have typed records (engine_body_record.rs): a clean front's do; a
    /// make of a body without one is the typed gap, as the run's reader answers it.
    typed: bool,
}

/// The run's reader's answer for a body with no typed record (engine_body_record.rs
/// `StoreTyped`), as the maker hands it up.
fn no_typed_record(key: ItemKey) -> RunError {
    use blaze_ladder::front2::program::Front2Refusal;
    use blaze_ladder::front2::program::lazy::LazyRefusal;
    use blaze_ladder::front2::program::lazy::typed::TypedGap;
    RunError::Front2(Front2Refusal::Lazy(LazyRefusal::Typed { function: Box::from("a toy body"), gap: TypedGap::NoRecord { key } }))
}

impl Make for ToyMaker<'_> {
    type Error = RunError;
    /// As the run's maker: handed the gate's proof (run_parent.rs `Front::Maker`).
    type Grant = Runnable;

    fn make(&mut self, _grant: &Runnable, tabled: ItemKey, _name: &[u8], entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.calls.set(self.calls.get() + 1);
        if !self.typed {
            return Err(no_typed_record(tabled));
        }
        let (_, callees) = CALLS.iter().find(|(f, _)| *f == tabled.0).ok_or(RunError::OneBody { key: tabled, why: "no such fn in the model" })?;
        for &callee in callees.iter() {
            let name = name_of(callee);
            entry.extend_from_slice(&callee.to_le_bytes());
            entry.extend_from_slice(&(name.len() as u32).to_le_bytes());
            entry.extend_from_slice(&name);
        }
        static_rows(tabled.0, entry);
        // The entry's own mark, so two leaves' records differ.
        entry.extend_from_slice(&tabled.0.to_le_bytes());
        Ok(())
    }

    /// A static's entry: its static rows and its mark (counted with the instances' makes).
    fn make_static(&mut self, _grant: &Runnable, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.calls.set(self.calls.get() + 1);
        static_rows(item.0, entry);
        entry.extend_from_slice(&item.0.to_le_bytes());
        Ok(())
    }

    /// A thread-local's template entry: its mark alone (counted with the others).
    fn make_thread_local(&mut self, _grant: &Runnable, item: ItemKey, entry: &mut AVec<'_, u8>) -> Result<(), RunError> {
        self.calls.set(self.calls.get() + 1);
        entry.extend_from_slice(&item.0.to_le_bytes());
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
            let len = u32::from_le_bytes([entry[at + 16], entry[at + 17], entry[at + 18], entry[at + 19]]);
            // A static row: its key, the length `u32::MAX`, no name.
            if len == u32::MAX {
                out.push_static(ItemKey(u128::from_le_bytes(wide)));
                at += 20;
                continue;
            }
            if len == TLS_ROW {
                out.push_thread_local(ItemKey(u128::from_le_bytes(wide)));
                at += 20;
                continue;
            }
            // A vtable is carried, not declared (its datum rides in the entry).
            if len == VT_ROW {
                at += 20;
                continue;
            }
            let len = len as usize;
            out.push(ItemKey(u128::from_le_bytes(wide)), &entry[at + 20..at + 20 + len]);
            at += 20 + len;
        }
        Ok(())
    }
}

/// The model's front end: what was made of it is counted.
pub(super) struct ToyFront<'t> {
    pub(super) calls: &'t Cell<usize>,
    pub(super) starts: &'t Cell<usize>,
    pub(super) clean: bool,
    /// The fn the run starts at: `MAIN`, or `SHOUT` for the statics model.
    pub(super) root: u128,
}

impl Publication for ToyFront<'_> {
    fn for_publication(&mut self, symbol: [u8; 32], record: &[u8], no_personality: bool, _values: &mut AVec<'_, u8>) -> Result<bool, ParentError> {
        if !no_personality {
            return Ok(false);
        }
        let view = blaze_code::record::CodeView::read(record).map_err(|why| ParentError::Decode { symbol, why })?;
        Ok(blaze_code::unwind::read(view).map_err(|why| ParentError::Unwind { symbol, why })?.lands())
    }
}

impl<'t> Front for ToyFront<'t> {
    type Start = ToyStart;
    type Ids = ToyIds;
    type Maker = ToyMaker<'t>;

    fn start(&mut self) -> Result<ToyStart, ParentError> {
        self.starts.set(self.starts.get() + 1);
        Ok(ToyStart { root: self.root, main: name_of(self.root) })
    }

    fn ids(&mut self) -> Result<ToyIds, ParentError> {
        Ok(ToyIds { reads: [Read { fact: FACT, digest: DIGEST }], clean: self.clean })
    }

    fn maker(&mut self, _start: &ToyStart, _ids: &ToyIds) -> Result<ToyMaker<'t>, ParentError> {
        // A body that is not clean has no typed record (the Bodies stage puts one per clean body).
        Ok(ToyMaker { calls: self.calls, typed: self.clean })
    }

    fn form(&self) -> &dyn Form {
        &ToyForm
    }

    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        Ok(entry)
    }

    fn statics_named(&self, entry: &[u8], out: &mut AVec<'_, Symbol>) -> Result<(), EntryError> {
        out.extend_from_slice(&toy_named(entry));
        Ok(())
    }

    /// A toy vtable's datum is its id's bytes.
    fn each_vtable(&self, _symbol: [u8; 32], entry: &[u8], each: &mut dyn FnMut(Symbol, &[u8]) -> Result<(), ParentError>) -> Result<(), ParentError> {
        for w in toy_vtables(entry) {
            each(vtable_of(w), &w.to_le_bytes())?;
        }
        Ok(())
    }
}

/// The records folder: the ids filed, and each record's bytes (the wire checker reads what a
/// published record names).
#[derive(Default)]
pub(super) struct ToyFolder {
    pub(super) held: BTreeSet<[u8; 32]>,
    bytes: std::collections::BTreeMap<[u8; 32], std::vec::Vec<u8>>,
}

/// The files control's model: a record is held once filed, as a file is.
impl Folder for &mut ToyFolder {
    fn begin(&mut self) {}

    fn file(&mut self, record: &[u8]) -> Result<RecordId, ParentError> {
        let id = blaze_blake3::hash(record);
        self.held.insert(id);
        self.bytes.insert(id, record.to_vec());
        Ok(RecordId(id))
    }

    fn kept(&mut self, _record: RecordId, _kept: Kept, _store: &mut dyn CodeStore) -> Result<(), ParentError> {
        Ok(())
    }

    fn ahead<F: Front>(&mut self, _symbol: Symbol, record: &RecordId, _code: Option<CodeKey>, _front: &F, _store: &dyn CodeStore) -> Result<bool, ParentError> {
        Ok(self.held.contains(&record.0))
    }

    fn take(&mut self) -> Sent<'_> {
        Sent::NONE
    }

    fn read<F: Front>(&self, record: &RecordId, _front: &F, _store: &dyn CodeStore, into: &mut bstd::mem::Bytes<'_>) -> Result<(), ParentError> {
        let bytes = self.bytes.get(&record.0).ok_or(ParentError::Sent { record: record.0, why: crate::run_bytes::SentWhy::NotKept })?;
        into.try_extend(bytes).map_err(|_| ParentError::Sent { record: record.0, why: crate::run_bytes::SentWhy::NoRoom { len: bytes.len() } })
    }
}

pub(super) fn builder() -> Builder {
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

    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    assert!(lazy.is_cold(), "the front end was made before any need");
    let (record, declares) = match lazy.resolve(&symbol_of(MAIN)).expect("main resolves") {
        Resolved::Record { record, declares, .. } => (record, symbols(declares)),
        Resolved::Unknown => panic!("main is unknown"),
        Resolved::Waiting => panic!("main waited for its code under `Codes::Serial`"),
    };
    assert_eq!(declares, [symbol_of(A), symbol_of(B)], "main's record declares a and b, in its entry's order");
    assert_eq!(calls.get(), 1, "the maker made more than main: the walk went ahead of what was asked");
    let first = lazy.tally();
    assert_eq!(first, Tally { reached: 1, found: 0, made: 1, unkeyed: 0 });
    drop(lazy);
    assert!(folder.held.contains(&record.0), "main's record was published without its file");
    assert_eq!(store.puts, 1, "main's entry was not put, or another was");

    let mut again = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    let second = match again.resolve(&symbol_of(MAIN)).expect("main resolves") {
        Resolved::Record { record: same, declares, .. } => {
            assert_eq!(same, record, "the found record is another than the made one");
            assert_eq!(symbols(declares), [symbol_of(A), symbol_of(B)]);
            again.tally()
        }
        Resolved::Unknown => panic!("main is unknown on the second run"),
        Resolved::Waiting => panic!("main waited for its code under `Codes::Serial`"),
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
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: false, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(MAIN)) {
        Err(ParentError::NotChecked { function, symbol, why }) => {
            assert_eq!((function, symbol, why), (ItemKey(MAIN), symbol_of(MAIN).0, NotRunnable::Checked(Checked::Absent(Missing::NoRecord))));
        }
        other => panic!("an unchecked body was answered: {other:?}"),
    }
    assert_eq!(calls.get(), 0, "an unchecked body was built");
    drop(lazy);
    // Unknown only once the walk from the roots has stepped every row and no record declares it.
    let mut clean = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    assert!(matches!(clean.resolve(&Symbol([9; 32])), Ok(Resolved::Unknown)), "an unknown symbol was answered with a record");
    assert_eq!(calls.get(), 4, "the walk from the roots did not step every row before answering Unknown");
    std::println!("lazy parent: an unchecked body refused by name, an unknown symbol Unknown");
}

/// Asserts THE ORDER (D163 part 2): a body with no typed record (one that is not clean: the
/// Bodies stage puts a record per clean body only, engine_bodies.rs) is refused by the run GATE
/// (`NotChecked`, run_ids.rs `RunIds::runnable`) before the maker, which reads the typed
/// records, is ever asked: the user meets the check's verdict, never the reader's
/// `TypedGap::NoRecord`. Control: the same maker asked past the gate answers the typed gap, so a
/// parent that made before it gated fails on the error's kind, not only on the count.
/// Prints "lazy parent: a body with no typed record is refused by the gate before the reader".
#[test]
fn a_body_with_no_typed_record_is_refused_by_the_gate_first() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: false, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(MAIN)) {
        Err(ParentError::NotChecked { function, why, .. }) => assert_eq!((function, why), (ItemKey(MAIN), NotRunnable::Checked(Checked::Absent(Missing::NoRecord)))),
        other => panic!("a body with no typed record was not refused by the gate: {other:?}"),
    }
    assert_eq!(calls.get(), 0, "the maker (the typed records' reader) was asked before the gate");
    drop(lazy);

    // Control: past the gate, the maker of a body with no typed record answers the gap.
    let grant = Checked::Clean.runs(ItemKey(MAIN)).expect("a clean body is runnable");
    let mut entry = AVec::new_in(alloc);
    let mut maker = ToyMaker { calls: &calls, typed: false };
    match maker.make(&grant, ItemKey(MAIN), &name_of(MAIN), &mut entry) {
        Err(RunError::Front2(blaze_ladder::front2::program::Front2Refusal::Lazy(blaze_ladder::front2::program::lazy::LazyRefusal::Typed { gap: blaze_ladder::front2::program::lazy::typed::TypedGap::NoRecord { key }, .. }))) => assert_eq!(key, ItemKey(MAIN)),
        other => panic!("control: the maker past the gate did not answer the typed gap: {other:?}"),
    }
    std::println!("lazy parent: a body with no typed record is refused by the gate before the reader");
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
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
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
    let mut first = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    let line = first.head().expect("the roots");
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.serve(&mut first, &need(Needed::Symbol(symbol_of(MAIN))), &mut out).expect("served");
    drop(first);
    let mut rows: std::vec::Vec<Line> = first_lines(&needs);
    // A row whose file is gone: b, with a record never filed.
    let gone = Row { symbol: symbol_of(B), key: CodeKey(blaze_ids::ContentId([0xb; 32])), record: RecordId([0xee; 32]) };
    rows.push(Line::Code(gone));
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&LISTED, line, &rows, &mut bytes).expect("two rows");
    let start_list::Held::Today(placed) = start_list::read(bytes.as_slice(), alloc).expect("a list").under(&LISTED) else { panic!("today's list read as another's") };

    starts.set(0);
    calls.set(0);
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
    let mut out = std::vec::Vec::new();
    let entry = ahead_of(Some(placed), &mut lazy, &mut needs, &mut kept, &mut out).expect("placed");
    assert_eq!(entry, line, "the list's head is not the one written");
    let kept_lines: std::vec::Vec<Line> = kept.iter().flat_map(|group| group.view().lines()).collect();
    assert_eq!(kept_lines, &rows[..1], "a row whose record's file is gone was placed");
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
    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(LEAF)).expect("leaf resolves") {
        Resolved::Record { declares, .. } => assert!(declares.is_empty(), "leaf declares nothing"),
        Resolved::Unknown => panic!("an unreached callee was answered Unknown"),
        Resolved::Waiting => panic!("leaf waited for its code under `Codes::Serial`"),
    }
    assert_eq!(calls.get(), 3, "main and a are stepped to reach leaf, then leaf is made");
    match lazy.resolve(&symbol_of(A)).expect("a, stepped ahead, is served") {
        Resolved::Record { declares, .. } => assert_eq!(symbols(declares), [symbol_of(LEAF)]),
        Resolved::Unknown => panic!("a is unknown"),
        Resolved::Waiting => panic!("a waited for its code under `Codes::Serial`"),
    }
    match lazy.resolve(&symbol_of(MAIN)).expect("main, stepped ahead, is served") {
        Resolved::Record { declares, .. } => assert_eq!(symbols(declares), [symbol_of(A), symbol_of(B)]),
        Resolved::Unknown => panic!("main is unknown"),
        Resolved::Waiting => panic!("main waited for its code under `Codes::Serial`"),
    }
    assert_eq!(calls.get(), 3, "a row stepped ahead was made again at its call");
    assert!(matches!(lazy.resolve(&symbol_of(A)), Err(ParentError::ResolvedTwice { .. })), "a served row was served again");
    assert!(matches!(lazy.resolve(&symbol_of(B)), Ok(Resolved::Record { .. })), "b, never stepped, is made at its call");
    assert_eq!(calls.get(), 4);
    assert_eq!(lazy.tally(), Tally { reached: 4, found: 0, made: 4, unkeyed: 0 });
    std::println!("lazy parent: an unreached callee walked to from the roots; rows stepped ahead served once");
}

/// Asserts (run.md, "Statics in the run"): a record that names static `S`, whose value points
/// at `T`, whose value points back at `S`, is answered with BOTH statics' data, each made once
/// and filed, `S` first, never as declared slots; the cycle is one batch and neither is sent
/// again (`leaf`, asked later, brings none); a static's symbol is no need (`Unknown`). On the
/// wire the order is `Declare`, `Data`, `Publish`, and the record that brought data is a
/// start-list group, its data before its row (start_list.rs version 4). A second run over the same store finds all three and makes nothing.
/// Controls: a parent that sent a static per record, or as a slot, or after the `Publish`, fails.
/// Prints "lazy parent: statics told once with the first record, Declare Data Publish; second run found 3".
#[test]
fn a_records_statics_are_told_once_before_its_publish() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));

    let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: SHOUT }, &mut folder, &mut store, builder(), alloc);
    let (declares, data) = match lazy.resolve(&symbol_of(SHOUT)).expect("shout resolves") {
        Resolved::Record { declares, data, .. } => (symbols(declares), data.to_vec()),
        Resolved::Unknown => panic!("shout is unknown"),
        Resolved::Waiting => panic!("shout waited for its code under `Codes::Serial`"),
    };
    assert_eq!(declares, [symbol_of(LEAF)], "a static was declared as a slot");
    let told: std::vec::Vec<Symbol> = data.iter().map(|d| d.symbol).collect();
    assert_eq!(told, [static_of(S), static_of(T)], "the record's statics, its own first, the cycle once");
    assert_eq!(calls.get(), 3, "shout, S and T each made once");
    assert_eq!(lazy.tally(), Tally { reached: 3, found: 0, made: 3, unkeyed: 0 });
    assert!(matches!(lazy.resolve(&static_of(S)), Ok(Resolved::Unknown)), "a static's symbol was answered as a need");
    match lazy.resolve(&symbol_of(LEAF)).expect("leaf resolves") {
        Resolved::Record { data, .. } => assert!(data.is_empty(), "a static told already was sent again"),
        Resolved::Unknown => panic!("leaf is unknown"),
        Resolved::Waiting => panic!("leaf waited for its code under `Codes::Serial`"),
    }
    drop(lazy);
    for d in data.iter() {
        assert!(folder.held.contains(&d.record.0), "a static's datum was told without its file");
    }

    let mut again = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: SHOUT }, &mut folder, &mut store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.serve(&mut again, &need(Needed::Symbol(symbol_of(SHOUT))), &mut out).expect("served");
    let said = frames(&out);
    let kinds: std::vec::Vec<&str> = said.iter().map(|frame| frame.split(' ').next().unwrap_or("")).collect();
    assert_eq!(kinds, ["Declare", "Data", "Publish"], "{said:?}");
    let datum = |s: u128| data.iter().find(|d| d.symbol == static_of(s)).map(|d| (d.symbol.0[0], d.record.0[0])).expect("told");
    assert_eq!(said[1], std::format!("Data {:?}", [datum(S), datum(T)]), "the found data are the made data");
    let codes: std::vec::Vec<Symbol> = first_lines(&needs).iter().filter_map(Line::code).map(|row| row.symbol).collect();
    let kept: std::vec::Vec<Symbol> = first_lines(&needs).iter().filter_map(|line| match line {
        Line::Datum(datum) => Some(datum.symbol),
        Line::Code(_) | Line::Named(_) | Line::Values(_) => None,
    }).collect();
    assert_eq!((codes, kept), (std::vec![symbol_of(SHOUT)], std::vec![static_of(S), static_of(T)]), "the record that brought data is not a start-list group with its data before it");
    assert_eq!(again.tally(), Tally { reached: 3, found: 3, made: 0, unkeyed: 0 });
    assert_eq!(calls.get(), 4, "the second run made a static or an instance its store holds");
    std::println!("lazy parent: statics told once with the first record, Declare Data Publish; second run found {}", again.tally().found);
}

/// Symbols 1 and 2 each name static 0x51; 1 is answered first and brings its datum, 2 brings
/// none (sent already); 3 names no static; 4 names static 0x77, which nothing sends.
struct TwoNamers {
    told: [Datum; 1],
    s: [Symbol; 1],
    rogue: [Symbol; 1],
}

impl Resolve for TwoNamers {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        let n = symbol.0[0];
        let key = CodeKey(blaze_ids::ContentId([n; 32]));
        let record = RecordId([n * 10; 32]);
        Ok(match n {
            1 => Resolved::Record { key, record, declares: &[], data: &self.told, named: &self.s, sent: Sent::NONE },
            2 => Resolved::Record { key, record, declares: &[], data: &[], named: &self.s, sent: Sent::NONE },
            3 => Resolved::Record { key, record, declares: &[], data: &[], named: &[], sent: Sent::NONE },
            4 => Resolved::Record { key, record, declares: &[], data: &[], named: &self.rogue, sent: Sent::NONE },
            _ => Resolved::Unknown,
        })
    }
}

/// Asserts (run.md, "Statics in the run"; start_list.rs version 4): (1) every first answer is
/// a start-list group in order, a record naming a static an EARLIER answer sent included: the
/// datum once, in its first namer's group, before that namer's row; the later namer's group
/// names it and carries no datum; `Data` precedes the first namer's `Publish`. (2) A record
/// naming a static nothing sent is refused by name (`StaticUnsent`) and NO `Publish` of it is
/// written: `Needs::publish` is the one writer. CONTROLS: record a group's datum from `data`
/// rather than the batch sent, and symbol 2's group holds the datum again, failing (1);
/// publish without checking `named` and a `Publish` of record 40 follows, failing (2).
/// Prints "run_parent: a record naming a static sent before is a group after the datum's; one never sent is refused".
#[test]
fn a_record_naming_a_static_told_before_is_a_group_after_its_datum() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut namers = TwoNamers { told: [Datum { symbol: Symbol([0x51; 32]), record: RecordId([0x52; 32]) }], s: [Symbol([0x51; 32])], rogue: [Symbol([0x77; 32])] };
    let mut needs = Needs::new(alloc);
    let mut inbox = need(Needed::Symbol(Symbol([1; 32])));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([2; 32]))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([3; 32]))));
    let mut out = std::vec::Vec::new();
    needs.serve(&mut namers, &inbox, &mut out).expect("served");
    let said = frames(&out);
    let kinds: std::vec::Vec<&str> = said.iter().map(|frame| frame.split(' ').next().unwrap_or("")).collect();
    assert_eq!(kinds, ["Declare", "Data", "Publish", "Declare", "Publish", "Declare", "Publish"], "{said:?}");
    let (one, two, three) = (Symbol([1; 32]), Symbol([2; 32]), Symbol([3; 32]));
    let row = |symbol: Symbol| {
        let n = symbol.0[0];
        Line::Code(Row { symbol, key: CodeKey(blaze_ids::ContentId([n; 32])), record: RecordId([n * 10; 32]) })
    };
    assert_eq!(
        first_lines(&needs),
        &[Line::Datum(namers.told[0]), Line::Named(namers.s[0]), row(one), Line::Named(namers.s[0]), row(two), row(three)][..],
        "the first answers are not each a group in order, the datum once before its first namer"
    );

    let mut rogue = std::vec::Vec::new();
    let refused = needs.serve(&mut namers, &need(Needed::Symbol(Symbol([4; 32]))), &mut rogue);
    assert!(matches!(refused, Err(ParentError::StaticUnsent { record, symbol, touches_tls: false, .. }) if record == [40; 32] && symbol == [0x77; 32]), "{refused:?}");
    assert!(!frames(&rogue).iter().any(|frame| frame.starts_with("Publish")), "a record naming an unsent static was published: {:?}", frames(&rogue));
    std::println!("run_parent: a record naming a static sent before is a group after the datum's; one never sent is refused");
}

/// THE WIRE INVARIANT (run.md, "Statics in the run"; the child's `StaticNotTold`): reading the
/// parent's messages in order, every `Publish` names a record whose every static (what
/// [`toy_named`] reads from its bytes in `folder`) an EARLIER `Data` sent, and no static is sent
/// twice. The publishes checked, or the first breach, named.
pub(super) fn statics_before_publish(bytes: &[u8], folder: &ToyFolder) -> Result<usize, std::string::String> {
    let mut sent: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut published = 0;
    let mut at = 0;
    while let Some((item, len)) = split(&bytes[at..]).map_err(|why| std::format!("a frame at {at}: {why:?}"))? {
        let (_, down) = Down::decode(item).map_err(|why| std::format!("a message at {at}: {why:?}"))?;
        match down {
            Down::Data { data } => {
                for datum in data.iter() {
                    if !sent.insert(datum.symbol.0) {
                        return Err(std::format!("static {:02x?} sent twice", &datum.symbol.0[..6]));
                    }
                }
            }
            // The thread-local descriptor: the child makes it when told (wire/thread_locals.rs).
            Down::ThreadLocals { descriptor, .. } => {
                if !sent.insert(descriptor.0) {
                    return Err(std::format!("the thread-local descriptor {:02x?} told twice", &descriptor.0[..6]));
                }
            }
            Down::Publish { record, .. } | Down::Positioned { record, .. } => {
                let entry = folder.bytes.get(&record.0).ok_or_else(|| std::format!("record {:02x?} published and never filed", &record.0[..6]))?;
                for static_ in toy_named(entry) {
                    if !sent.contains(&static_.0) {
                        return Err(std::format!("record {:02x?} published before a `Data` sent static {:02x?}", &record.0[..6], &static_.0[..6]));
                    }
                }
                published += 1;
            }
            _ => {}
        }
        at += len;
    }
    Ok(published)
}

/// The wire checker's planted bug: a `Publish` of `shout`'s record before the `Data` of `S`
/// and `T` is caught, and the same messages in the right order pass. A checker that never
/// looked fails the first assert.
/// Prints "wire checker: a Publish before its Data is caught".
#[test]
fn the_wire_checker_catches_a_publish_before_its_data() {
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let mut folder = ToyFolder::default();
    let mut entry: AVec<'_, u8> = Vec::new_in(erased.handle());
    static_rows(SHOUT, &mut entry);
    entry.extend_from_slice(&SHOUT.to_le_bytes());
    let record = (&mut folder).file(&entry).expect("filed");
    let data = [Datum { symbol: static_of(S), record: RecordId([1; 32]) }, Datum { symbol: static_of(T), record: RecordId([2; 32]) }];
    let publish = |out: &mut std::vec::Vec<u8>| Down::Publish { slot: Slot(0), tier: Tier::Zero, record }.encode(0, out).expect("Publish encodes");
    let send = |out: &mut std::vec::Vec<u8>| Down::Data { data: List::of(&data) }.encode(1, out).expect("Data encodes");
    let mut wrong = std::vec::Vec::new();
    publish(&mut wrong);
    send(&mut wrong);
    assert!(statics_before_publish(&wrong, &folder).is_err(), "a Publish before its Data passed the checker");
    let mut right = std::vec::Vec::new();
    send(&mut right);
    publish(&mut right);
    assert_eq!(statics_before_publish(&right, &folder), Ok(1));
    std::println!("wire checker: a Publish before its Data is caught");
}

/// Asserts the wire invariant ([`statics_before_publish`]) over every path a record reaches
/// the child by, in one lazy parent over the toy program:
/// - the personality named at the start (`Needs::name_personality`), and a need of it that
///   resolves to nothing (`Failed`);
/// - the root (`hub`, no static);
/// - a record made ON DEMAND that is the first to name `S` (`echo`): `S` and `T` (a static in a
///   static, a cycle) go in one `Data` before it;
/// - a second record naming `S` (`shout`), with no `Data`, and the same record published AGAIN;
/// - the next run's start-list groups placed ahead (`Needs::place_ahead`): the list holds all
///   four first answers, `echo`'s group with the data of `S` and `T` before its row, so the
///   replay's `Data` goes before `echo`'s and `shout`'s `Publish`;
/// - a record touching a thread-local (`thready`, the hello world's old stop): the template of
///   `V` goes in a `Data` and the descriptor in one `ThreadLocals`, both before its `Publish`;
///   a second record touching `V` (the same record again) sends neither again.
/// CONTROLS: the planted bug in `the_wire_checker_catches_a_publish_before_its_data`; and run
/// 3's messages with the `ThreadLocals` cut out (a parent that never told the descriptor) fail
/// the checker, naming the descriptor.
/// Prints "wire: every Publish stands on its statics; 5 + 7 + 1 publishes checked; the descriptor told once, before its first namer".
#[test]
fn every_publish_stands_on_its_statics_on_the_wire() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));

    let mut out = std::vec::Vec::new();
    let mut needs = Needs::new(alloc);
    let first: std::vec::Vec<Line> = {
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB }, &mut folder, &mut store, builder(), alloc);
        let head = lazy.head().expect("the head");
        needs.name_personality(head.personality, &mut out).expect("the personality named");
        let mut inbox = need(Needed::Symbol(symbol_of(HUB)));
        for item in [ECHO, SHOUT, SHOUT, LEAF, PERSONALITY] {
            inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(item))));
        }
        needs.serve(&mut lazy, &inbox, &mut out).expect("served");
        first_lines(&needs)
    };
    let said = frames(&out);
    assert!(said.iter().any(|frame| frame.starts_with("Personality")) && said.iter().any(|frame| frame.starts_with("Failed")), "{said:?}");
    assert_eq!(statics_before_publish(&out, &folder), Ok(5), "run 1: {said:?}");
    let rows: std::vec::Vec<Symbol> = first.iter().filter_map(Line::code).map(|row| row.symbol).collect();
    assert_eq!(rows, [symbol_of(HUB), symbol_of(ECHO), symbol_of(SHOUT), symbol_of(LEAF)], "the start list lost a first answer, a namer of a static or not");

    let mut again = std::vec::Vec::new();
    let mut list = Vec::new_in(alloc);
    listed(&first, &mut list);
    {
        let groups = today_groups(&list, alloc);
        let mut needs = Needs::new(alloc);
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB }, &mut folder, &mut store, builder(), alloc);
        needs.place_ahead(&groups, &mut again).expect("placed");
        lazy.placed(&groups);
        let mut inbox = need(Needed::Symbol(symbol_of(HUB)));
        inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(ECHO))));
        inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(SHOUT))));
        needs.serve(&mut lazy, &inbox, &mut again).expect("served from the list");
    }
    // The four groups placed (echo's with the `Data` of S and T), then three needs of placed
    // rows published again.
    assert_eq!(statics_before_publish(&again, &folder), Ok(7), "run 2: {:?}", frames(&again));

    let mut thready = std::vec::Vec::new();
    {
        let mut needs = Needs::new(alloc);
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: THREADY }, &mut folder, &mut store, builder(), alloc);
        needs.serve(&mut lazy, &need(Needed::Symbol(symbol_of(THREADY))), &mut thready).expect("a record touching a thread-local served");
    }
    let said = frames(&thready);
    let kinds: std::vec::Vec<&str> = said.iter().map(|frame| frame.split(' ').next().unwrap_or("")).collect();
    assert_eq!(kinds, ["Declare", "Data", "ThreadLocals", "Publish"], "run 3: {said:?}");
    assert_eq!(statics_before_publish(&thready, &folder), Ok(1), "run 3: {said:?}");
    // The control: the same messages with the `ThreadLocals` cut out.
    let tls = blaze_ladder::symbols::tls_desc_symbol().0;
    let mut untold = std::vec::Vec::new();
    let mut at = 0;
    while let Some((item, len)) = split(&thready[at..]).expect("run 3's frames") {
        if !matches!(Down::decode(item), Ok((_, Down::ThreadLocals { .. }))) {
            untold.extend_from_slice(&thready[at..at + len]);
        }
        at += len;
    }
    let breach = statics_before_publish(&untold, &folder).expect_err("a Publish naming the descriptor, never told, passed the checker");
    assert!(breach.contains(&std::format!("{:02x?}", &tls[..6])), "the breach does not name the descriptor: {breach}");
    std::println!("wire: every Publish stands on its statics; 5 + 7 + 1 publishes checked; the descriptor told once, before its first namer");
}

/// Asserts (run.md, "Statics in the run"; code_records.rs `VtableDatum`): a record naming a
/// vtable (`dyn`'s, `W`) is published only after a `Data` carrying the vtable's datum, the datum
/// filed in the records folder under its content, and its method word's instance (`leaf`)
/// declared in the same `Declare`; the namer's start-list group is the datum, the name, its row; a second need of the record
/// sends the datum no more. CONTROL: the same messages with the `Data` cut out (a parent that
/// never sent the vtable) fail the wire checker, naming the vtable.
/// Prints "wire: a vtable's datum is sent before its first namer, once; a cut `Data` is caught".
#[test]
fn a_record_naming_a_vtable_stands_on_its_datum() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut out = std::vec::Vec::new();
    let first = {
        let mut needs = Needs::new(alloc);
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: DYN }, &mut folder, &mut store, builder(), alloc);
        let mut inbox = need(Needed::Symbol(symbol_of(DYN)));
        inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(DYN))));
        needs.serve(&mut lazy, &inbox, &mut out).expect("a record naming a vtable served");
        first_lines(&needs).iter().map(|line| match line {
            Line::Datum(datum) => ('d', datum.symbol),
            Line::Named(symbol) => ('n', *symbol),
            Line::Code(row) => ('c', row.symbol),
            Line::Values(_) => ('v', Symbol([0; 32])),
        }).collect::<std::vec::Vec<_>>()
    };
    let said = frames(&out);
    let kinds: std::vec::Vec<&str> = said.iter().map(|frame| frame.split(' ').next().unwrap_or("")).collect();
    assert_eq!(kinds, ["Declare", "Data", "Publish", "Publish"], "{said:?}");
    assert_eq!(first, [('d', vtable_of(W)), ('n', vtable_of(W)), ('c', symbol_of(DYN))], "a record naming a vtable is not a group with the vtable's datum before its row");
    let mut sent = std::vec::Vec::new();
    let mut at = 0;
    while let Some((item, len)) = split(&out[at..]).expect("the frames") {
        match Down::decode(item).expect("a message") {
            (_, Down::Declare { pairs }) => assert!(pairs.iter().any(|it| it.symbol == symbol_of(LEAF)), "the method word's instance was not declared: {said:?}"),
            (_, Down::Data { data }) => sent.extend(data.iter().map(|it| (it.symbol, it.record))),
            _ => {}
        }
        at += len;
    }
    let datum = W.to_le_bytes();
    assert_eq!(sent, [(vtable_of(W), RecordId(blaze_blake3::hash(&datum)))], "the Data is not the vtable's datum, filed by its content");
    assert!(folder.held.contains(&blaze_blake3::hash(&datum)), "the vtable's datum was sent and never filed");
    assert_eq!(statics_before_publish(&out, &folder), Ok(2), "{said:?}");
    // The control: the `Data` cut out.
    let mut cut = std::vec::Vec::new();
    let mut at = 0;
    while let Some((item, len)) = split(&out[at..]).expect("the frames") {
        if !matches!(Down::decode(item), Ok((_, Down::Data { .. }))) {
            cut.extend_from_slice(&out[at..at + len]);
        }
        at += len;
    }
    let breach = statics_before_publish(&cut, &folder).expect_err("a Publish naming a vtable never sent passed the checker");
    assert!(breach.contains(&std::format!("{:02x?}", &vtable_of(W).0[..6])), "the breach does not name the vtable: {breach}");
    std::println!("wire: a vtable's datum is sent before its first namer, once; a cut `Data` is caught");
}

/// The model's start with the head's routine chosen: `routine` false is a program built to
/// unwind whose lang crates declare no `#[lang = "eh_personality"]` fn.
struct Headed {
    toy: ToyStart,
    routine: bool,
}

impl Start for Headed {
    fn head(&self) -> StartHead {
        let head = self.toy.head();
        StartHead { personality: head.personality.filter(|_| self.routine), ..head }
    }

    fn root_count(&self) -> usize {
        self.toy.root_count()
    }

    fn root_row(&self, i: usize) -> (ItemKey, &[u8]) {
        self.toy.root_row(i)
    }

    fn maker_entry(&self) -> Option<ItemKey> {
        self.toy.maker_entry()
    }

    fn special(&self, symbol: &CodeSymbol, builder: &Builder, store: &mut dyn CodeStore, alloc: Alloc<'_>) -> Result<Option<SpecialMade>, ParentError> {
        self.toy.special(symbol, builder, store, alloc)
    }
}

/// The model's front end where `a`'s record lands (its entry ends with `a`'s mark): `a` holds a
/// value whose drop unwinding must run across its call of `leaf`. `main` lands nowhere.
struct Landing<'t> {
    toy: ToyFront<'t>,
    routine: bool,
}

impl Publication for Landing<'_> {
    fn for_publication(&mut self, _symbol: [u8; 32], record: &[u8], no_personality: bool, _values: &mut AVec<'_, u8>) -> Result<bool, ParentError> {
        Ok(no_personality && record.ends_with(&A.to_le_bytes()))
    }
}

impl<'t> Front for Landing<'t> {
    type Start = Headed;
    type Ids = ToyIds;
    type Maker = ToyMaker<'t>;

    fn start(&mut self) -> Result<Headed, ParentError> {
        Ok(Headed { toy: self.toy.start()?, routine: self.routine })
    }

    fn ids(&mut self) -> Result<ToyIds, ParentError> {
        self.toy.ids()
    }

    fn maker(&mut self, start: &Headed, ids: &ToyIds) -> Result<ToyMaker<'t>, ParentError> {
        self.toy.maker(&start.toy, ids)
    }

    fn form(&self) -> &dyn Form {
        &ToyForm
    }

    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        Ok(entry)
    }

    fn statics_named(&self, entry: &[u8], out: &mut AVec<'_, Symbol>) -> Result<(), EntryError> {
        self.toy.statics_named(entry, out)
    }

    fn each_vtable(&self, symbol: [u8; 32], entry: &[u8], each: &mut dyn FnMut(Symbol, &[u8]) -> Result<(), ParentError>) -> Result<(), ParentError> {
        self.toy.each_vtable(symbol, entry, each)
    }

    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        (item.0 == A).then(|| Box::from("toy::a"))
    }
}

/// Rule 4 (blaze-ladder personality.rs): a program built to unwind that declares no personality
/// routine runs while no record lands. Asserts: with no routine, `main` (no row lands) resolves
/// to its record, and `a` (a row lands) is refused as `NoPersonality` naming `a` by key and path,
/// its record never filed; control: the same model with a routine named resolves `a`. A parent
/// that refused at the start fails the first; one that published `a`'s record fails the second.
/// Prints "lazy parent: no routine, `main` runs and `a` is refused: <error>".
#[test]
fn a_record_that_lands_with_no_routine_is_refused_naming_its_fn() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let toy = || ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN };

    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let mut lazy = Lazy::new(&cx, Landing { toy: toy(), routine: false }, &mut folder, &mut store, builder(), alloc);
    match lazy.resolve(&symbol_of(MAIN)) {
        Ok(Resolved::Record { .. }) => {}
        Ok(Resolved::Unknown) => panic!("`main` lands nowhere and was answered Unknown"),
        Ok(Resolved::Waiting) => panic!("`main` waited for its code under `Codes::Serial`"),
        Err(why) => panic!("`main` lands nowhere and was refused: {why}"),
    }
    let refused = match lazy.resolve(&symbol_of(A)) {
        Err(why @ ParentError::NoPersonality { .. }) => why,
        Err(other) => panic!("`a` was refused for another cause: {other}"),
        Ok(_) => panic!("`a`'s record lands and was answered with no routine named"),
    };
    let ParentError::NoPersonality { function, path, symbol } = &refused else { unreachable!("matched above") };
    assert_eq!((*function, path.as_deref(), *symbol), (Some(ItemKey(A)), Some("toy::a"), symbol_of(A).0));
    let shown = std::format!("{refused}");
    assert!(shown.contains("fn `toy::a`") && shown.contains("eh_personality"), "{shown}");
    drop(lazy);
    assert_eq!(folder.held.len(), 1, "a record past `main`'s was filed: `a`'s, which was refused");

    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let mut named = Lazy::new(&cx, Landing { toy: toy(), routine: true }, &mut folder, &mut store, builder(), alloc);
    named.resolve(&symbol_of(MAIN)).expect("`main` resolves with a routine");
    assert!(matches!(named.resolve(&symbol_of(A)), Ok(Resolved::Record { .. })), "`a` was refused with a routine named");
    std::println!("lazy parent: no routine, `main` runs and `a` is refused: {shown}");
}

/// A sink that writes each event down as its `Debug` text, and says whether it wants paths.
struct Recorder {
    seen: std::cell::RefCell<std::vec::Vec<std::string::String>>,
    paths: bool,
}

impl blaze_screen::Watch for Recorder {
    fn see(&self, step: blaze_screen::Step<'_>) {
        self.seen.borrow_mut().push(std::format!("{step:?}"));
    }

    fn paths(&self) -> bool {
        self.paths
    }
}

/// The model's front end with a sink: everything is `ToyFront`'s; a fn's path is `toy::<key>`
/// and each one named is counted.
struct Watched<'t> {
    toy: ToyFront<'t>,
    watch: &'t Recorder,
    named: &'t Cell<usize>,
}

fn toy_path(item: u128) -> std::string::String {
    std::format!("toy::{item:#x}")
}

impl Publication for Watched<'_> {
    fn for_publication(&mut self, symbol: [u8; 32], record: &[u8], no_personality: bool, values: &mut AVec<'_, u8>) -> Result<bool, ParentError> {
        self.toy.for_publication(symbol, record, no_personality, values)
    }
}

impl<'t> Front for Watched<'t> {
    type Start = ToyStart;
    type Ids = ToyIds;
    type Maker = ToyMaker<'t>;

    fn start(&mut self) -> Result<ToyStart, ParentError> {
        self.toy.start()
    }

    fn ids(&mut self) -> Result<ToyIds, ParentError> {
        self.toy.ids()
    }

    fn maker(&mut self, start: &ToyStart, ids: &ToyIds) -> Result<ToyMaker<'t>, ParentError> {
        self.toy.maker(start, ids)
    }

    fn form(&self) -> &dyn Form {
        self.toy.form()
    }

    fn record_of<'e>(&self, entry: &'e [u8]) -> Result<&'e [u8], EntryError> {
        self.toy.record_of(entry)
    }

    fn statics_named(&self, entry: &[u8], out: &mut AVec<'_, Symbol>) -> Result<(), EntryError> {
        self.toy.statics_named(entry, out)
    }

    fn each_vtable(&self, symbol: [u8; 32], entry: &[u8], each: &mut dyn FnMut(Symbol, &[u8]) -> Result<(), ParentError>) -> Result<(), ParentError> {
        self.toy.each_vtable(symbol, entry, each)
    }

    fn path_of(&mut self, item: ItemKey) -> Option<Box<str>> {
        self.named.set(self.named.get() + 1);
        Some(toy_path(item.0).into_boxed_str())
    }

    fn watch(&self) -> &dyn blaze_screen::Watch {
        self.watch
    }
}

/// Asserts: a recorded run of the model emits, for each first call, the fn being answered
/// (with its path when the sink wants paths) and then how it was answered, in the order the
/// program asks: `main` made, then `a` made; a second run over the same store finds `main`.
/// A sink that wants no paths gets `None` and the front is never asked to name one (the
/// control: a parent that named paths for a sink that shows nothing pays a lookup per call).
/// The loop's events ride the same sink through `Resolve::watch`.
/// Prints "run events: first run [..]; second run [..]".
#[test]
fn a_recorded_run_emits_each_first_call_and_how_it_was_answered() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts, named) = (Cell::new(0), Cell::new(0), Cell::new(0));
    let toy = || ToyFront { calls: &calls, starts: &starts, clean: true, root: MAIN };
    let resolving = |item: u128| std::format!("Resolving {{ path: Some({:?}) }}", toy_path(item));

    let first = Recorder { seen: std::cell::RefCell::new(std::vec::Vec::new()), paths: true };
    let mut lazy = Lazy::new(&cx, Watched { toy: toy(), watch: &first, named: &named }, &mut folder, &mut store, builder(), alloc);
    assert!(first.seen.borrow().is_empty(), "an event before any need: {:?}", first.seen.borrow());
    lazy.resolve(&symbol_of(MAIN)).expect("main resolves");
    lazy.resolve(&symbol_of(A)).expect("a resolves");
    Resolve::watch(&lazy).see(blaze_screen::Step::Answers);
    drop(lazy);
    let made = "Answered(Made)".to_string();
    assert_eq!(*first.seen.borrow(), [resolving(MAIN), made.clone(), resolving(A), made, "Answers".to_string()]);
    assert_eq!(named.get(), 2, "each fn stepped is named once");

    let second = Recorder { seen: std::cell::RefCell::new(std::vec::Vec::new()), paths: false };
    let mut again = Lazy::new(&cx, Watched { toy: toy(), watch: &second, named: &named }, &mut folder, &mut store, builder(), alloc);
    again.resolve(&symbol_of(MAIN)).expect("main resolves on the second run");
    drop(again);
    assert_eq!(*second.seen.borrow(), ["Resolving { path: None }".to_string(), "Answered(Found)".to_string()]);
    assert_eq!(named.get(), 2, "a sink that wants no paths had one named");
    std::println!("run events: first run {:?}; second run {:?}", first.seen.borrow(), second.seen.borrow());
}

/// Asserts: (1) the program's native library is the run's first frame, its two words numbered
/// slots 0 and 1 in word order, and the personality's `Declare` after it tells its own pair
/// alone (slot 2), so no word is declared twice; (2) the control: told after a slot was
/// numbered, it is refused by name ([`ParentError::NativeLate`]) and nothing is written.
/// Prints "native: the library told first, its words slots 0 and 1; told late refused".
#[test]
fn the_native_library_is_the_first_frame() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let words = [Symbol([7; 32]), Symbol([8; 32])];
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.tell_native(RecordId([5; 32]), &words, &mut out).expect("told first");
    needs.name_personality(Some(CodeSymbol([9; 32])), &mut out).expect("the personality named");
    let said = frames(&out);
    assert_eq!(said, ["Native 5 2", "Declare [(9, 2)]", "Personality 2"], "the library was not the first frame: {said:?}");
    let (_, first) = Down::decode(split(&out).expect("a frame").expect("one frame").0).expect("a run wire message");
    let Down::Native { words: told, .. } = first else { panic!("the first frame is no Native") };
    let pairs: std::vec::Vec<_> = told.iter().map(|it| (it.symbol, it.slot.0)).collect();
    assert_eq!(pairs, [(words[0], 0), (words[1], 1)], "the words were not numbered in word order");

    let mut late = Needs::new(alloc);
    let mut before = std::vec::Vec::new();
    late.name_personality(Some(CodeSymbol([9; 32])), &mut before).expect("the personality named");
    let mut after = std::vec::Vec::new();
    let refused = late.tell_native(RecordId([5; 32]), &words, &mut after);
    assert!(matches!(refused, Err(ParentError::NativeLate { slots: 1 })), "a late library was not refused: {refused:?}");
    assert!(after.is_empty(), "a refused library wrote a frame");
    std::println!("native: the library told first, its words slots 0 and 1; told late refused");
}

/// The toy's maker as a code worker keeps it: made for one entry and start (test_drive.rs
/// `MadeFor`).
struct ToyKept<'t> {
    maker: ToyMaker<'t>,
    entry: Option<ItemKey>,
    start: blaze_ladder::run::special::MakerStart,
}

impl crate::test_drive::MadeFor for ToyKept<'_> {
    fn made_for(&self) -> (Option<ItemKey>, blaze_ladder::run::special::MakerStart) {
        (self.entry, self.start)
    }
}

/// One code worker's makers over the toy: the fake beside the command's `SuiteMakers` at the
/// seam's one trait (test_drive.rs `KeptMakers`, rule 78), its makers kept by the command's
/// `Makers`. `panics`: how many makes left panic before they make (rule 20's plant).
struct ToyMakers<'t> {
    calls: &'t Cell<usize>,
    kept: crate::test_drive::Makers<ToyKept<'t>>,
    panics: usize,
    forgot: usize,
}

impl crate::test_drive::KeptMakers for ToyMakers<'_> {
    fn make(&mut self, job: &crate::test_drive::Job<'static>, entry: &mut AVec<'_, u8>) -> Result<(), crate::test_drive::Unmade> {
        if self.panics > 0 {
            self.panics -= 1;
            panic!("a planted panic in a toy make");
        }
        let calls = self.calls;
        let kept = self.kept.of(job, || Ok(ToyKept { maker: ToyMaker { calls, typed: true }, entry: job.entry, start: job.start }))?;
        job.order.make(&mut kept.maker, &job.proof, job.name.as_slice(), entry)
    }

    fn forget(&mut self, suite: crate::test_loop::SuiteId) {
        self.forgot += 1;
        self.kept.forget(suite);
    }

    /// A toy maker has no program to name or place by: the crossed refusal keeps its fn's name.
    fn shown(&mut self, job: &crate::test_drive::Job<'static>, refusal: &mut crate::run_parent::CodeRefusal) {
        let _ = (job, refusal);
    }
}

fn toy_makers(calls: &Cell<usize>, plant: crate::test_drive::MakersPlant, panics: usize) -> ToyMakers<'_> {
    ToyMakers { calls, kept: crate::test_drive::Makers::new(blaze_own::testing::heap().alloc(), plant), panics, forgot: 0 }
}

/// A make of the toy's fn `item` for a test whose roots start at `entry`, as the relay hands it
/// to a worker (test_drive.rs `Job`).
fn toy_job(item: u128, entry: u128) -> crate::test_drive::Job<'static> {
    let heap = blaze_own::testing::heap();
    let mut name = blaze_engine::SVec::with_capacity_in(20, heap.shared());
    name.extend_from_slice(&name_of(item));
    crate::test_drive::Job {
        suite: crate::test_loop::SuiteId(0),
        order: blaze_ladder::body_ids::Order { tabled: ItemKey(item), kind: blaze_ladder::body_ids::RowKind::Instance },
        name,
        proof: Checked::Clean.runs(ItemKey(item)).expect("a clean body runs"),
        entry: Some(ItemKey(entry)),
        start: blaze_ladder::run::special::MakerStart::ByRule,
    }
}

/// `symbol` resolved under `Codes::Pooled` through the real relay and the real pool's channels
/// (test_drive.rs `CodeRelay`, `CodePool`, `Lanes`), its one code worker's turn driven inline
/// (`WorkerLane::step`, the same make and hand-back as the worker's loop): each ask handed on, made,
/// pumped back and answered, until the symbol's record comes. Its id and declares.
#[expect(clippy::too_many_arguments, reason = "the seam's pieces, each handed in")]
fn resolve_pooled(
    lazy: &mut Lazy<'_, '_, ToyFront<'_>, &mut ToyFolder>,
    symbol: &Symbol,
    code: &crate::test_drive::CodeRelay<'_>,
    lane: crate::test_drive::WorkerLane<'_, '_>,
    makers: &mut dyn crate::test_drive::KeptMakers,
    cx: &bstd::Cx,
    clock: &bstd::clock::Clock,
    heap: &'static blaze_own::Heap,
) -> Result<([u8; 32], std::vec::Vec<Symbol>), ParentError> {
    use blaze_engine::code_pool::Child;
    use blaze_engine::code_pool::relay::Reply;

    let child = Child(0);
    let mut scratch: AVec<'static, u8> = blaze_own::Vec::new_in(heap.alloc());
    for _ in 0..8 {
        match lazy.resolve(symbol)? {
            Resolved::Record { record, declares, .. } => return Ok((record.0, symbols(declares))),
            Resolved::Unknown => panic!("the symbol is unknown under `Codes::Pooled`"),
            Resolved::Waiting => {}
        }
        let ask = lazy.asked().expect("a waiting row names its make");
        let mut name = blaze_engine::SVec::with_capacity_in(ask.name.len(), heap.shared());
        name.extend_from_slice(ask.name);
        let job = crate::test_drive::Job {
            suite: crate::test_loop::SuiteId(0),
            order: ask.order,
            name,
            proof: ask.proof,
            entry: Some(ItemKey(MAIN)),
            start: blaze_ladder::run::special::MakerStart::ByRule,
        };
        code.ask(child, ask.key, job).expect("the relay takes the ask");
        assert!(lane.step(cx, clock, makers, &mut scratch, heap).expect("the worker's turn"), "the relay handed the worker no make");
        code.pump().expect("the relay takes the made entry back");
        let came = code
            .answer(child, |reply| match reply {
                Reply::Made(entry) => lazy.made(entry),
                Reply::Refused(why) => Err(lazy.refused(why.for_asker())),
                Reply::Late { .. } => panic!("the one child's ask was late"),
            })
            .expect("the relay answers");
        came.expect("the ask was answered in its turn")?;
    }
    panic!("the symbol did not resolve in eight makes");
}

/// THE SEAM TEST of D194 (rule 78): `Codes::Serial` and `Codes::Pooled` over one toy give equal
/// records. Asserts, for `main` resolved once each way in a fresh store: (1) equal record ids
/// and declares; (2) equal record bytes filed; (3) equal tallies, puts and makes. Pooled goes
/// through the real relay, the real pool's channels and the worker's make (`make_one`), with the
/// fake `KeptMakers`; the control of the makers is `a_maker_of_another_start_is_refused`.
/// Prints "seam: serial and pooled equal: record ..., N bytes filed, M makes".
#[test]
fn pooled_codes_equal_serial() {
    use crate::test_drive::{CodePool, CodeRelay, Lanes, MakersPlant};

    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let clock = io.need_clock().expect("the simulated world's clock");
    let heap = blaze_own::testing::heap();
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();

    // Serial: the make in the test's own turn.
    let mut serial_store = MemoryStore::new_in(alloc);
    let mut serial_folder = ToyFolder::default();
    let (serial_calls, serial_starts) = (Cell::new(0), Cell::new(0));
    let mut serial = Lazy::new(&cx, ToyFront { calls: &serial_calls, starts: &serial_starts, clean: true, root: MAIN }, &mut serial_folder, &mut serial_store, builder(), alloc);
    let (serial_record, serial_declares) = match serial.resolve(&symbol_of(MAIN)).expect("main resolves serially") {
        Resolved::Record { record, declares, .. } => (record.0, symbols(declares)),
        Resolved::Unknown => panic!("main is unknown"),
        Resolved::Waiting => panic!("main waited under `Codes::Serial`"),
    };
    let serial_tally = serial.tally();
    drop(serial);

    // Pooled: the make relayed to a worker.
    let lanes = Lanes::new(core::num::NonZeroUsize::MIN, heap);
    let pool = CodePool { lanes: &lanes, count: 1, cx: &cx, clock: &clock, procs: crate::job_texts::Procs::None };
    let now = || blaze_run::Nanos(0);
    let code = CodeRelay::new(1, &pool, &now, heap.shared(), 0x5eed).expect("a relay for one child");
    let worker_calls = Cell::new(0);
    let mut makers = toy_makers(&worker_calls, MakersPlant::None, 0);
    let mut pooled_store = MemoryStore::new_in(alloc);
    let mut pooled_folder = ToyFolder::default();
    let (pooled_calls, pooled_starts) = (Cell::new(0), Cell::new(0));
    let mut pooled = Lazy::new(&cx, ToyFront { calls: &pooled_calls, starts: &pooled_starts, clean: true, root: MAIN }, &mut pooled_folder, &mut pooled_store, builder(), alloc);
    pooled.make_by(Codes::Pooled);
    let (pooled_record, pooled_declares) = resolve_pooled(&mut pooled, &symbol_of(MAIN), &code, lanes.lane(0), &mut makers, &cx, &clock, heap).expect("main resolves pooled");
    let pooled_tally = pooled.tally();
    drop(pooled);
    lanes.close();

    assert_eq!(pooled_record, serial_record, "the pooled make filed another record than the serial one");
    assert_eq!(pooled_declares, serial_declares, "the pooled record declares other symbols");
    assert_eq!(pooled_folder.bytes, serial_folder.bytes, "the records filed differ in their bytes");
    assert_eq!(pooled_tally, serial_tally, "the two runs counted differently");
    assert_eq!(pooled_store.puts, serial_store.puts, "the two runs put a different number of entries");
    assert_eq!(pooled_calls.get(), 0, "the pooled parent made an instance in its own turn");
    assert_eq!(worker_calls.get(), serial_calls.get(), "the worker made a different number of instances than the serial turn");
    let bytes: usize = serial_folder.bytes.values().map(|record| record.len()).sum();
    std::println!("seam: serial and pooled equal: record {}, {bytes} bytes filed, {} makes", Hex(&serial_record), worker_calls.get());
}

/// THE SEAM TEST's control (rule 78, a planted bug): a worker whose makers are found by suite
/// alone (`MakersPlant::BySuiteAlone`) hands a make of a test starting at `a` the maker made for
/// `main`'s; `Makers::of` refuses it by name, typed, and never makes with it. Asserts the clean
/// makers make both, and the planted ones refuse the second with the words of `ANOTHER_START`.
/// Prints "seam control: a maker of another start refused".
#[test]
fn a_maker_of_another_start_is_refused() {
    use crate::test_drive::{MakersPlant, make_one};

    let heap = blaze_own::testing::heap();
    let mut scratch: AVec<'static, u8> = blaze_own::Vec::new_in(heap.alloc());
    for plant in [MakersPlant::None, MakersPlant::BySuiteAlone] {
        let calls = Cell::new(0);
        let mut makers = toy_makers(&calls, plant, 0);
        make_one(&mut makers, &toy_job(MAIN, MAIN), &mut scratch, heap.shared()).expect("main's make");
        let second = make_one(&mut makers, &toy_job(A, A), &mut scratch, heap.shared());
        match plant {
            MakersPlant::None => {
                second.expect("a make of another test's roots gets a maker of its own");
                assert_eq!(calls.get(), 2, "the clean makers did not make both");
            }
            MakersPlant::BySuiteAlone => {
                let why = second.expect_err("the planted makers handed a maker of another start, and it was used");
                assert!(why.walk.as_str().contains("another entry or start"), "the refusal does not name the start: {why}");
                assert!(why.whole, "the refusal's words were cut");
                assert_eq!(calls.get(), 1, "the maker of another start made the instance");
            }
        }
    }
    std::println!("seam control: a maker of another start refused");
}

/// Rule 20 on a code worker: a make that panics fails alone, typed, naming that it panicked;
/// the worker forgets the suite's makers; its next make works. Asserts the refusal's words, one
/// forget, and the next make's entry. Prints "code worker: a panicked make failed alone".
#[test]
fn a_panicking_make_fails_alone_and_the_next_one_makes() {
    use crate::test_drive::{MakersPlant, make_one};

    let heap = blaze_own::testing::heap();
    let mut scratch: AVec<'static, u8> = blaze_own::Vec::new_in(heap.alloc());
    let calls = Cell::new(0);
    let mut makers = toy_makers(&calls, MakersPlant::None, 1);
    let panicked = make_one(&mut makers, &toy_job(MAIN, MAIN), &mut scratch, heap.shared()).expect_err("a planted panic made an entry");
    assert!(panicked.walk.as_str().contains("panicked on its code worker"), "the refusal does not say the make panicked: {panicked}");
    assert_eq!(makers.forgot, 1, "the worker kept a suite's makers past a panic in them");
    let made = make_one(&mut makers, &toy_job(MAIN, MAIN), &mut scratch, heap.shared()).expect("the make after the panic");
    assert!(!made.is_empty(), "the make after the panic made nothing");
    assert_eq!(calls.get(), 1, "the panicked make reached the maker");
    std::println!("code worker: a panicked make failed alone");
}
