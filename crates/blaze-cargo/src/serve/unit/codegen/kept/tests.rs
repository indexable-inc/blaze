//! A link's rows kept through builds' deltas against the rows made whole from each build.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use blaze_codegen::{
    BuiltId, Changes, CompiledFunction, Delta, Init, InstanceKey, Relocs, Static, StaticId,
    driver::Built,
};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use hegel::generators as gs;
use hir_ty_codegen::v2::{InputsWork, LinkInputs, Table, span_table_id};

use super::{Rows, Spans, Work};
use crate::serve::unit::codegen::{filled_id, static_row};

/// A program: functions by slot, each with how many times its body was edited (its key says
/// it) and the slots it calls; statics by slot with their value; span tables by slot with
/// their content.
#[derive(Clone, Debug, Default)]
struct Program {
    functions: BTreeMap<u8, (u8, Vec<u8>)>,
    statics: BTreeMap<u8, u8>,
    spans: BTreeMap<u8, u8>,
}

/// One edit of a program.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// The function's body: it has another key, and each caller another fill under its own.
    Body(u8),
    /// The function calls `callee` where it did not, or stops: another fill under its key.
    Calls(u8, u8),
    Remove(u8),
    Static(u8, u8),
    NoStatic(u8),
    Spans(u8, u8),
    NoSpans(u8),
    /// The program built again as it is.
    Again,
}

fn key(slot: u8, edits: u8) -> InstanceKey {
    let mut key = [slot; 32];
    key[1] = edits;
    InstanceKey(ContentId(key))
}

fn anchor(slot: u8) -> ContentId {
    ContentId([slot ^ 0x55; 32])
}

impl Program {
    fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::Body(slot) => {
                let function = self.functions.entry(slot).or_default();
                function.0 = function.0.wrapping_add(1);
            }
            Edit::Calls(slot, callee) => {
                let calls = &mut self.functions.entry(slot).or_default().1;
                match calls.iter().position(|it| *it == callee) {
                    Some(at) => {
                        calls.remove(at);
                    }
                    None => calls.push(callee),
                }
            }
            Edit::Remove(slot) => {
                self.functions.remove(&slot);
            }
            Edit::Static(slot, value) => {
                self.statics.insert(slot, value);
            }
            Edit::NoStatic(slot) => {
                self.statics.remove(&slot);
            }
            Edit::Spans(slot, value) => {
                self.spans.insert(slot, value);
            }
            Edit::NoSpans(slot) => {
                self.spans.remove(&slot);
            }
            Edit::Again => {}
        }
    }

    /// The key a call of `slot` is filled with: the function's as it is now, or of one never
    /// edited when the program has none.
    fn callee(&self, slot: u8) -> InstanceKey {
        key(slot, self.functions.get(&slot).map_or(0, |it| it.0))
    }

    fn compiled(&self) -> Vec<CompiledFunction> {
        let mut functions: Vec<CompiledFunction> = self
            .functions
            .iter()
            .map(|(slot, (edits, calls))| {
                let relocs: Vec<blaze_codegen::Relocation> = calls
                    .iter()
                    .enumerate()
                    .map(|(at, callee)| blaze_codegen::Relocation {
                        at: at as u32 * 5 + 1,
                        kind: blaze_codegen::RelocKind::PcRel32,
                        target: blaze_codegen::Target::Function(self.callee(*callee)),
                        addend: -4,
                    })
                    .collect();
                let mut code: Vec<u8> = calls.iter().flat_map(|_| [0xe8, 0, 0, 0, 0]).collect();
                code.push(0xc3);
                CompiledFunction {
                    key: key(*slot, *edits),
                    arch: blaze_codegen::Arch::X86_64,
                    align: 16,
                    code,
                    relocs: relocs.into(),
                    data: Vec::new(),
                    inlined: Vec::new(),
                    side: None,
                    unwind: None,
                    lines: Vec::new(),
                    coverage: None,
                    profile: None,
                }
            })
            .collect();
        functions.sort_unstable_by_key(|it| it.key);
        functions
    }

    fn compiled_statics(&self) -> Vec<Static> {
        let mut statics: Vec<Static> = self
            .statics
            .iter()
            .map(|(slot, value)| Static {
                item: ItemKey(ContentId([*slot; 32])),
                instance: ContentId([slot.wrapping_add(100); 32]),
                align: 4,
                init: Init::Bytes { bytes: vec![*value; 4], relocs: Relocs::new() },
                data: Vec::new(),
                mutable: true,
                thread_local: false,
                section: None,
                imports: Vec::new(),
            })
            .collect();
        statics.sort_by_key(|it| (it.item, it.instance));
        statics
    }

    fn tables(&self) -> Vec<Table> {
        let mut tables: Vec<Table> = self
            .spans
            .iter()
            .map(|(slot, value)| {
                let bytes = vec![*value; 16];
                Table { anchor: anchor(*slot), id: span_table_id(&bytes), bytes }
            })
            .collect();
        tables.sort_unstable_by_key(|it| it.anchor);
        tables
    }
}

/// What a build says of each function and static, by key: the oracle's own rows, each made
/// by hashing the whole function or static.
#[derive(Clone, Default)]
struct Entries {
    id: BuiltId,
    functions: Vec<(InstanceKey, ContentId)>,
    statics: Vec<(StaticId, ContentId)>,
}

/// The keys of `now` not in `was`, those in both with another entry, and those gone.
fn changes<K: Copy + Ord>(was: &[(K, ContentId)], now: &[(K, ContentId)]) -> Changes<K> {
    let mut made = Changes::default();
    for (key, entry) in now {
        match was.binary_search_by(|it| it.0.cmp(key)) {
            Err(_) => made.added.push(*key),
            Ok(at) if was[at].1 != *entry => made.changed.push(*key),
            Ok(_) => {}
        }
    }
    for (key, _) in was {
        if now.binary_search_by(|it| it.0.cmp(key)).is_err() {
            made.removed.push(*key);
        }
    }
    made
}

/// The build of `program` as a change of the build `before` it, and its entries.
fn built(program: &Program, before: Option<&Entries>) -> (Built, Entries) {
    let functions = program.compiled();
    let statics = program.compiled_statics();
    let mut entries = Entries {
        id: BuiltId::default(),
        functions: functions.iter().map(|it| (it.key, filled_id(it))).collect(),
        statics: statics
            .iter()
            .map(|it| ((it.item, it.instance), static_row(it).unwrap().2))
            .collect(),
    };
    let mut bytes = Vec::new();
    for (key, row) in &entries.functions {
        bytes.extend_from_slice(&key.0.0);
        bytes.extend_from_slice(&row.0);
    }
    bytes.push(0xff);
    for (id, row) in &entries.statics {
        bytes.extend_from_slice(&id.0.0.0);
        bytes.extend_from_slice(&id.1.0);
        bytes.extend_from_slice(&row.0);
    }
    entries.id = BuiltId(ContentId::of("kept test built", &bytes));
    let delta = match before {
        None => Delta::default(),
        Some(was) => Delta {
            base: Some(was.id),
            functions: changes(&was.functions, &entries.functions),
            statics: changes(&was.statics, &entries.statics),
            fresh: None,
        },
    };
    let keys = || functions.iter().map(|it| it.key);
    let built = Built {
        inputs: keys().map(|key| (key, CodeHash(key.0))).collect(),
        identities: keys().map(|key| (key, key.0)).collect(),
        debug_names: keys().map(|key| (key, format!("f{:?}", key.0))).collect(),
        functions,
        statics,
        id: entries.id,
        delta,
        ..Built::default()
    };
    (built, entries)
}

/// Which bug a run plants.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plant {
    Nothing,
    /// A function the delta lists as changed keeps its row.
    SkipsChanged,
    /// A removed function's entry stays in the digest.
    KeepsRemoved,
}

/// `edits` applied to a program, each followed by a build and a link's rows brought to it by
/// the build's delta and the span tables' changes; the last step's work. `Err` names the
/// first step after which the kept rows, span tables, id or the lists of what changed are
/// not those of rows made whole from the build.
fn kept_is_whole(start: &Program, edits: &[Edit], plant: Plant) -> Result<Work, String> {
    let mut program = start.clone();
    let mut kept = Rows::default();
    kept.plant_skips_changed = plant == Plant::SkipsChanged;
    kept.plant_keeps_removed = plant == Plant::KeepsRemoved;
    let mut before: Option<Entries> = None;
    let mut tables_before: Vec<Table> = Vec::new();
    let mut serial = 0;
    let mut work = Work::default();
    let steps = std::iter::once(Edit::Again).chain(edits.iter().copied());
    for (step, edit) in steps.enumerate() {
        program.apply(edit);
        let (build, entries) = built(&program, before.as_ref());
        let tables = program.tables();
        // The front end's side: a snapshot of every table first, then what changed.
        let inputs = match step {
            0 => {
                serial += 1;
                LinkInputs {
                    serial,
                    base: None,
                    tables: tables.clone(), // clone: the test's copy of the snapshot
                    removed: Vec::new(),
                    work: InputsWork { whole: Some("the first link"), ..InputsWork::default() },
                }
            }
            _ => {
                let changed: Vec<Table> =
                    tables.iter().filter(|it| !tables_before.contains(it)).cloned().collect();
                let removed: Vec<ContentId> = tables_before
                    .iter()
                    .map(|it| it.anchor)
                    .filter(|anchor| !tables.iter().any(|it| it.anchor == *anchor))
                    .collect();
                let base = serial;
                if !changed.is_empty() || !removed.is_empty() {
                    serial += 1;
                }
                LinkInputs {
                    serial,
                    base: (serial != base).then_some(base),
                    tables: changed,
                    removed,
                    work: InputsWork::default(),
                }
            }
        };
        // What a link with nothing kept makes of the same build, and what it says changed
        // against the rows held before this step.
        let mut whole = Rows::default();
        let all = Spans::All {
            serial: inputs.serial,
            tables: tables.iter().map(|it| (it.anchor, None, &it.bytes[..])).collect(),
        };
        whole.apply(&build, all, &mut Work::default()).map_err(|it| format!("step {step}: {it}"))?;
        let was = before.clone().unwrap_or_default(); // clone: the test's copy of the entries
        let want = changes(&was.functions, &entries.functions);
        let mut want_removed: Vec<InstanceKey> =
            want.removed.iter().chain(&want.changed).copied().collect();
        let mut want_added: Vec<InstanceKey> =
            want.added.iter().chain(&want.changed).copied().collect();
        want_removed.sort_unstable();
        want_added.sort_unstable();

        work = Work::default();
        let spans = kept.spans_of(&inputs).ok_or_else(|| {
            format!("step {step} ({edit:?}): the rows hold neither the snapshot nor its base")
        })?;
        let applied = kept
            .apply(&build, spans, &mut work)
            .map_err(|it| format!("step {step} ({edit:?}): {it}"))?;
        let said = format!("after step {step} ({edit:?})");
        if step > 0 && work.whole.is_some() {
            return Err(format!("{said}: every row was made again: {:?}", work.whole));
        }
        if kept.functions != whole.functions {
            return Err(format!("{said}: the function rows kept are not the build's"));
        }
        if kept.statics != whole.statics {
            return Err(format!("{said}: the static rows kept are not the build's"));
        }
        if kept.spans != whole.spans {
            return Err(format!("{said}: the span tables kept are not the build's"));
        }
        if kept.given(build.id, &[]) != whole.given(build.id, &[]) {
            return Err(format!("{said}: the id kept is not the id of the build's rows"));
        }
        if kept.digest.nodes() != whole.digest.nodes() {
            return Err(format!("{said}: the digest kept is not canonical"));
        }
        if (applied.removed, applied.added) != (want_removed, want_added) {
            return Err(format!("{said}: the functions said gone and new are not those that changed"));
        }
        before = Some(entries);
        tables_before = tables;
    }
    Ok(work)
}

#[hegel::composite]
fn edit(tc: &hegel::TestCase) -> Edit {
    let slot = 1 + tc.draw(gs::integers::<u8>().max_value(5));
    let other = 1 + tc.draw(gs::integers::<u8>().max_value(5));
    let value = tc.draw(gs::integers::<u8>().max_value(2));
    match tc.draw(gs::integers::<u8>().max_value(8)) {
        0 | 1 => Edit::Body(slot),
        2 | 3 => Edit::Calls(slot, other),
        4 => Edit::Remove(slot),
        5 => Edit::Static(slot, value),
        6 => Edit::NoStatic(slot),
        7 => Edit::Spans(slot, value),
        _ => match value {
            0 => Edit::NoSpans(slot),
            _ => Edit::Again,
        },
    }
}

/// The refinement: for any program and sequence of edits (a body edited, a call added or
/// dropped, a function removed, a static or a span table changed or gone), the rows, span
/// tables and id a link keeps by applying each build's delta are those of rows made whole
/// from the build, and what it says changed is what did.
#[hegel::test]
fn the_rows_kept_by_deltas_are_the_rows_made_whole(tc: hegel::TestCase) {
    let start: Vec<Edit> = tc.draw(gs::vecs(edit()).max_size(8));
    let edits: Vec<Edit> = tc.draw(gs::vecs(edit()).max_size(10));
    let mut program = Program::default();
    for edit in start {
        program.apply(edit);
    }
    if let Err(why) = kept_is_whole(&program, &edits, Plant::Nothing) {
        panic!("{why}");
    }
}

/// `main` (3) calls `call` (2), which calls `log` (1).
fn chain() -> Program {
    let mut program = Program::default();
    for edit in [Edit::Calls(3, 2), Edit::Calls(2, 1), Edit::Body(1)] {
        program.apply(edit);
    }
    program
}

/// A control of [`the_rows_kept_by_deltas_are_the_rows_made_whole`]: `log` is edited, so
/// `call` is filled with its new key under its own. With a link that keeps the row of a
/// function the delta lists as changed, `call`'s row is its old fill's, and the oracle says
/// at which step.
#[test]
fn the_oracle_catches_a_row_not_refreshed_for_a_caller() {
    let edits = [Edit::Body(1)];
    kept_is_whole(&chain(), &edits, Plant::Nothing).unwrap();
    let why = kept_is_whole(&chain(), &edits, Plant::SkipsChanged)
        .expect_err("the oracle missed a caller left on its old fill");
    assert!(why.contains("after step 1") && why.contains("function rows"), "{why}");
}

/// A control of [`the_rows_kept_by_deltas_are_the_rows_made_whole`]: with a link that drops
/// a removed function's row and leaves its entry in the digest, the id still says the
/// function, and the oracle says at which step.
#[test]
fn the_oracle_catches_a_given_id_not_updated_for_a_removed_function() {
    let edits = [Edit::Remove(3)];
    kept_is_whole(&chain(), &edits, Plant::Nothing).unwrap();
    let why = kept_is_whole(&chain(), &edits, Plant::KeepsRemoved)
        .expect_err("the oracle missed an id that still says a removed function");
    assert!(why.contains("after step 1") && why.contains("the id kept"), "{why}");
}

/// A link the image refuses takes its rows back: they, and their id, are what they were.
#[test]
fn rows_taken_back_are_the_rows_before() {
    let mut program = chain();
    let (first, entries) = built(&program, None);
    let mut kept = Rows::default();
    let all = Spans::All { serial: 1, tables: Vec::new() };
    kept.apply(&first, all, &mut Work::default()).unwrap();
    let (functions, given) = (kept.functions.clone(), kept.given(first.id, &[]));
    program.apply(Edit::Body(1));
    program.apply(Edit::Remove(3));
    let (second, _) = built(&program, Some(&entries));
    let applied = kept.apply(&second, Spans::Same, &mut Work::default()).unwrap();
    assert_ne!(kept.functions, functions);
    kept.undo(applied.undo);
    assert_eq!(kept.functions, functions);
    assert_eq!((kept.built, kept.given(first.id, &[])), (Some(first.id), given));
    assert_eq!(kept.takes(&second), Ok(false), "the next link's delta is against them still");
}

/// The counter: one function's body edited among `N` hashes that function and its one
/// caller, and the digest's paths of the two: the rows hashed and the bytes hashed among
/// 10,000 functions are those among 100, within the digest's few nodes more.
#[test]
fn a_body_edit_among_many_hashes_the_edit() {
    let mut costs = Vec::new();
    for count in [100u32, 10_000] {
        let mut program = chain();
        // The rest of the program: functions that call nothing, in slots of their own.
        let mut rest = Vec::new();
        for at in 0..count {
            let mut key = [0xee; 32];
            key[4..8].copy_from_slice(&at.to_be_bytes());
            rest.push(key);
        }
        let (mut first, _) = built(&program, None);
        let filler = first.functions[0].clone(); // clone: the test's template of a function
        for key in &rest {
            let mut function = filler.clone(); // clone: one more function of the program
            function.key = InstanceKey(ContentId(*key));
            function.relocs = Vec::<blaze_codegen::Relocation>::new().into();
            function.code = vec![0xc3];
            first.functions.push(function);
        }
        first.functions.sort_unstable_by_key(|it| it.key);
        first.inputs = first.functions.iter().map(|it| (it.key, CodeHash(it.key.0))).collect();
        first.identities = first.functions.iter().map(|it| (it.key, it.key.0)).collect();
        let entries = Entries {
            id: first.id,
            functions: first.functions.iter().map(|it| (it.key, filled_id(it))).collect(),
            statics: Vec::new(),
        };
        let mut kept = Rows::default();
        let all = Spans::All { serial: 1, tables: Vec::new() };
        kept.apply(&first, all, &mut Work::default()).unwrap();

        program.apply(Edit::Body(1));
        let (mut second, _) = built(&program, None);
        let edited: Vec<CompiledFunction> = second.functions.clone(); // clone: the edited three
        second.functions = first.functions.iter().filter(|it| it.key.0.0[0] == 0xee).cloned().collect();
        second.functions.extend(edited);
        second.functions.sort_unstable_by_key(|it| it.key);
        second.inputs = second.functions.iter().map(|it| (it.key, CodeHash(it.key.0))).collect();
        second.identities = second.functions.iter().map(|it| (it.key, it.key.0)).collect();
        let now: Vec<(InstanceKey, ContentId)> =
            second.functions.iter().map(|it| (it.key, filled_id(it))).collect();
        second.id = BuiltId(ContentId::of("kept test built", &count.to_le_bytes()));
        second.delta = Delta {
            base: Some(first.id),
            functions: changes(&entries.functions, &now),
            statics: Changes::default(),
            fresh: None,
        };
        let mut work = Work::default();
        let applied = kept.apply(&second, Spans::Same, &mut work).unwrap();
        assert_eq!(work.whole, None, "{count} functions: taken as the build's delta");
        assert_eq!(work.rows, 2, "{count} functions: the new `log` and `call` filled with it");
        assert_eq!((applied.removed.len(), applied.added.len()), (2, 2));
        assert!(work.digest_bytes <= 3 * 64 * 40, "{count} functions: {work:?}");
        let mut whole = Rows::default();
        let all = Spans::All { serial: 1, tables: Vec::new() };
        whole.apply(&second, all, &mut Work::default()).unwrap();
        assert_eq!(kept.given(second.id, &[]), whole.given(second.id, &[]));
        costs.push((work.rows, work.row_bytes));
    }
    assert_eq!(costs[0], costs[1], "the same rows and code hashed among 100 as among 10,000");
}
