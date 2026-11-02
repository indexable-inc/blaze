use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::CodeHash;

use super::*;
use crate::{
    Arch, Data, Line, RelocKind, Relocation, Relocs, Target,
    driver::reach::{Node, Reach},
};

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn id(n: u8) -> ContentId {
    ContentId([n + 100; 32])
}

fn ran(n: u8) -> ContentId {
    ContentId([n + 200; 32])
}

fn relocs(targets: &[Target]) -> Relocs {
    let rows = targets.iter().enumerate().map(|(at, target)| Relocation {
        at: 8 * at as u32,
        kind: RelocKind::Abs64,
        target: target.clone(),
        addend: 0,
    });
    rows.collect::<Vec<_>>().into()
}

/// The code of `key` naming `targets`, with `data` naming `in_data` and lines of `lines`.
fn compiled(key: InstanceKey, targets: &[Target], in_data: &[Target], lines: &[SpanRef]) -> CompiledFunction {
    let data = (!in_data.is_empty()).then(|| Data {
        id: ContentId([77; 32]),
        align: 8,
        bytes: vec![0; 8 * in_data.len()],
        relocs: relocs(in_data),
        writable: false,
    });
    CompiledFunction {
        key,
        arch: Arch::X86_64,
        align: 16,
        code: vec![0; 8 * targets.len().max(1)],
        relocs: relocs(targets),
        data: data.into_iter().collect(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: lines.iter().enumerate().map(|(at, span)| Line { at: at as u32, span: *span }).collect(),
        coverage: None,
        profile: None,
    }
}

/// Functions 0, 1 and 2 by key, roots 0 and 2: 0 calls 1 through its slot, 2 calls nothing.
fn built() -> Built {
    let calls = [Target::Function(key(1))];
    let none: [Target; 0] = [];
    let marks = [ContentId([9; 32]); 2];
    let refs: [&[Target]; 3] = [&calls, &none, &none];
    let reach = Reach::new((0..3).map(|n| (key(n), Node { marks, refs: refs[n as usize] })), std::iter::empty());
    Built {
        functions: (0..3).map(|n| compiled(key(n), refs[n as usize], &[], &[])).collect(),
        roots: vec![key(0), key(2)],
        inputs: (0..3).map(|n| (key(n), CodeHash(ran(n)))).collect(),
        versions: (0..3).map(|n| (id(n), Ok(ran(n)))).collect(),
        reach,
        ..Built::default()
    }
}

/// `bound` as the table keeps it for a function naming no static and no span table.
fn plain(bound: ContentId) -> ContentId {
    placed(bound, &[], &[], &SpanIds::new(&[])).unwrap()
}

fn slots(key: &InstanceKey) -> Option<u32> {
    match key.0.0[0] {
        0 => Some(2),
        1 => Some(0),
        _ => None,
    }
}

fn tests() -> Vec<(String, usize)> {
    vec![("calls".to_owned(), 0), ("alone".to_owned(), 1)]
}

#[test]
fn a_function_s_id_is_its_identity_with_its_settings() {
    let (identity, other) = (ContentId([1; 32]), ContentId([2; 32]));
    let (debug, release) = (Settings(ContentId([3; 32])), Settings(ContentId([4; 32])));
    assert_eq!(fn_id(identity, debug), fn_id(identity, debug));
    assert_ne!(fn_id(identity, debug), fn_id(identity, release));
    assert_ne!(fn_id(identity, debug), fn_id(other, debug));
}

#[test]
fn a_bound_is_of_the_items_its_references_bind_to() {
    let items = |key: &InstanceKey| (key.0.0[0] < 3).then(|| ItemKey(ContentId([key.0.0[0] + 50; 32])));
    let calls = |callee: u8| [Target::Function(key(callee)), Target::Extern("memcpy".to_owned())];
    let of = |refs: &[Target]| bound(&Ok(ran(0)), refs, items).unwrap();
    // Instance 1 and instance 2 here are of one item: a callee's own change is not the caller's.
    let same = |key: &InstanceKey| Some(ItemKey(ContentId([key.0.0[0].min(1); 32])));
    assert_eq!(
        bound(&Ok(ran(0)), &calls(1), same).unwrap(),
        bound(&Ok(ran(0)), &[Target::Direct(key(2)), Target::Extern("memcpy".to_owned())], same).unwrap()
    );
    assert_ne!(of(&calls(1)), of(&calls(2)), "another item called");
    assert_ne!(of(&calls(1)), of(&calls(1)[..1]), "a reference more");
    assert_ne!(of(&calls(1)), bound(&Ok(ran(1)), &calls(1), items).unwrap(), "its own code");
    assert_eq!(of(&[Target::Coverage(0)]), of(&[position(4, 9)]));
    assert_eq!(bound(&Err("no body".to_owned()), &calls(1), items), Err("no body".to_owned()));
    assert!(bound(&Ok(ran(0)), &calls(5), items).unwrap_err().contains("no instance of"));
}

fn static_id(n: u8) -> (ItemKey, ContentId) {
    (ItemKey(ContentId([n + 30; 32])), ContentId::default())
}

fn static_ref(n: u8) -> Target {
    let (item, instance) = static_id(n);
    Target::Static { item, instance }
}

fn span(anchor: u8, index: u32) -> SpanRef {
    SpanRef { anchor: ContentId([anchor; 32]), index }
}

fn position(anchor: u8, index: u32) -> Target {
    Target::Position(span(anchor, index))
}

fn statics_of(values: &[(u8, u8, Vec<Target>)]) -> StaticBounds {
    let bind = |key: &InstanceKey| (key.0.0[0] < 3).then(|| id(key.0.0[0]));
    StaticBounds::new(values.iter().map(|(n, value, refs)| (static_id(*n), ContentId([*value; 32]), refs.iter().collect())), bind)
}

fn bound_of(statics: &StaticBounds, n: u8) -> ContentId {
    let (item, instance) = static_id(n);
    statics.of(&item, &instance).unwrap().0.clone().unwrap()
}

#[test]
fn a_static_s_bound_is_its_value_and_those_it_reaches() {
    // 0 names 1, 1 names 2 and a position; 3 names nothing.
    let of = |value_of_2: u8| {
        statics_of(&[
            (0, 1, vec![static_ref(1)]),
            (1, 1, vec![static_ref(2), position(8, 3)]),
            (2, value_of_2, vec![Target::Function(key(1))]),
            (3, 1, vec![]),
        ])
    };
    let (before, after) = (of(1), of(2));
    for n in 0..3 {
        assert_ne!(bound_of(&before, n), bound_of(&after, n), "static {n} reaches the edited one");
    }
    assert_eq!(bound_of(&before, 3), bound_of(&after, 3));
    let (item, instance) = static_id(0);
    assert_eq!(before.of(&item, &instance).unwrap().1, [ContentId([8; 32])]);
    let (item, instance) = static_id(3);
    assert!(before.of(&item, &instance).unwrap().1.is_empty());
}

#[test]
fn a_cycle_of_statics_is_hashed_once() {
    let of = |value_of_1: u8| {
        statics_of(&[
            (0, 1, vec![static_ref(1), position(4, 0)]),
            (1, value_of_1, vec![static_ref(0), position(5, 0)]),
            (2, 1, vec![static_ref(0)]),
        ])
    };
    let (before, after) = (of(1), of(2));
    assert_eq!(bound_of(&before, 0), bound_of(&before, 1), "one cycle, one bound");
    for n in 0..3 {
        assert_ne!(bound_of(&before, n), bound_of(&after, n));
    }
    let (item, instance) = static_id(2);
    assert_eq!(before.of(&item, &instance).unwrap().1, [ContentId([4; 32]), ContentId([5; 32])]);
}

#[test]
fn a_static_naming_what_the_build_lacks_has_no_bound() {
    let statics = statics_of(&[(0, 1, vec![Target::Function(key(7))]), (1, 1, vec![static_ref(0)]), (2, 1, vec![static_ref(9)])]);
    for n in 0..3 {
        let (item, instance) = static_id(n);
        assert!(statics.of(&item, &instance).unwrap().0.is_err(), "static {n}");
    }
    let (item, instance) = static_id(9);
    assert!(statics.of(&item, &instance).is_none());
}

#[test]
fn a_function_names_its_positions_data_lines_and_statics_sorted_once() {
    let statics = statics_of(&[(0, 1, vec![position(6, 0), position(2, 1)])]);
    let function = compiled(
        key(0),
        &[position(9, 0), static_ref(0), Target::Function(key(1))],
        &[position(2, 4), static_ref(0)],
        &[span(9, 2), span(1, 0)],
    );
    let (mut anchors, mut named) = (vec![ContentId([200; 32])], vec![]);
    named_by(&function, &statics, &mut anchors, &mut named).unwrap();
    assert_eq!(anchors, [1, 2, 6, 9].map(|it| ContentId([it; 32])), "cleared, then each once in order");
    let (item, instance) = static_id(0);
    assert_eq!(named, [(item, instance, bound_of(&statics, 0))]);

    let missing = compiled(key(0), &[static_ref(5)], &[], &[]);
    assert!(named_by(&missing, &statics, &mut anchors, &mut named).unwrap_err().contains("none of"));
}

#[test]
fn placed_is_the_bound_with_the_statics_and_tables_it_names() {
    let tables = |moved: u8| {
        (0..100u8).map(|n| (ContentId([n; 32]), vec![n, if n == 7 { moved } else { 0 }])).collect::<Vec<_>>()
    };
    let (before, after) = (SpanIds::new(&tables(0)), SpanIds::new(&tables(1)));
    let bound = ContentId([1; 32]);
    let names = |anchors: &[u8]| anchors.iter().map(|it| ContentId([*it; 32])).collect::<Vec<_>>();
    let at = |anchors: &[u8], spans: &SpanIds| placed(bound, &[], &names(anchors), spans);
    assert_ne!(at(&[3, 7], &before), at(&[3, 7], &after), "a table it names moved");
    assert_eq!(at(&[3, 8], &before), at(&[3, 8], &after));
    assert_eq!(at(&[], &before), at(&[], &after));
    assert_ne!(at(&[], &before), placed(ContentId([2; 32]), &[], &[], &before), "its own bound");
    let (item, instance) = static_id(0);
    assert_ne!(
        placed(bound, &[(item, instance, ContentId([1; 32]))], &[], &before),
        placed(bound, &[(item, instance, ContentId([2; 32]))], &[], &before),
        "a static's value"
    );
    assert_eq!(at(&[3, 200], &before), Err(ContentId([200; 32])));
    for n in 0..100u8 {
        assert_eq!(before.get(&ContentId([n; 32])), Some(ContentId::of(SPAN_TABLE_DOMAIN, &[n, 0])));
    }
    assert_eq!(SpanIds::new(&[]).get(&ContentId([1; 32])), None);
}

#[test]
fn a_binary_records_what_it_ran_where_it_has_slots() {
    assert!(!records(Mode::Output));
    assert!(records(Mode::Live));
    assert!(records(Mode::Tiered));
}

#[test]
fn a_table_has_every_function_each_slot_s_and_each_test_s() {
    let beside = ContentId([7; 32]);
    let table = ExecTable::new(&built(), &[], 4, slots, beside, tests()).unwrap();
    assert_eq!(table.fns, [(id(0), plain(ran(0))), (id(1), plain(ran(1))), (id(2), plain(ran(2)))]);
    assert_eq!(table.slot_fn, [1, ExecTable::EMPTY, 0, ExecTable::EMPTY]);
    assert_eq!(table.beside, beside);
    assert_eq!(table.tests, [("calls".to_owned(), vec![0, 1]), ("alone".to_owned(), vec![2])]);
}

/// Functions 0 to 4 and statics 0 to 2: 0 names table 1 by a position, 1 by its data, 2 by a
/// line, 3 is merged away into 0, 4 names tables 2 and 1 and reads static 1, which names
/// static 0; 5 reads static 2. Every function is a root.
fn placing(span_table_1: u8, static_0: u8) -> (Built, Vec<(ContentId, Vec<u8>)>) {
    let none: [Target; 0] = [];
    let marks = [ContentId([9; 32]); 2];
    let reach = Reach::new((0..6).map(|n| (key(n), Node { marks, refs: &none })), std::iter::empty());
    let functions = vec![
        compiled(key(0), &[position(1, 0)], &[], &[]),
        compiled(key(1), &[], &[position(1, 3)], &[]),
        compiled(key(2), &[], &[], &[span(1, 2)]),
        compiled(key(4), &[position(2, 0), position(1, 1), static_ref(1)], &[], &[]),
        compiled(key(5), &[static_ref(2)], &[], &[]),
    ];
    let value = |byte: u8, targets: &[Target]| Static {
        item: static_id(0).0,
        instance: ContentId::default(),
        align: 8,
        init: Init::Bytes { bytes: vec![byte; 8 * targets.len().max(1)], relocs: relocs(targets) },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    };
    let statics = vec![
        Static { item: static_id(0).0, ..value(static_0, &[]) },
        Static { item: static_id(1).0, ..value(1, &[static_ref(0)]) },
        Static { item: static_id(2).0, ..value(1, &[]) },
    ];
    let mut deduped = crate::dedupe::Deduped::default();
    deduped.merged.insert(key(3), key(0));
    let built = Built {
        functions,
        statics,
        roots: (0..6).map(key).collect(),
        inputs: (0..6).map(|n| (key(n), CodeHash(ran(n)))).collect(),
        versions: (0..6).map(|n| (id(n), Ok(ran(n)))).collect(),
        deduped,
        reach,
        ..Built::default()
    };
    let spans = vec![(ContentId([1; 32]), vec![span_table_1]), (ContentId([2; 32]), vec![0])];
    (built, spans)
}

fn no_tests() -> Vec<(String, usize)> {
    Vec::new()
}

fn placed_of(span_table_1: u8, static_0: u8) -> Vec<ContentId> {
    let (built, spans) = placing(span_table_1, static_0);
    let table = ExecTable::new(&built, &spans, 0, |_| None, ContentId::default(), no_tests()).unwrap();
    table.fns.iter().map(|it| it.1).collect()
}

#[test]
fn a_moved_table_changes_exactly_the_functions_naming_it() {
    let (before, moved) = (placed_of(0, 0), placed_of(1, 0));
    for n in [0, 1, 2, 3, 4] {
        assert_ne!(before[n], moved[n], "function {n} names table 1");
    }
    assert_eq!(before[5], moved[5], "function 5 names no table");
}

#[test]
fn a_static_s_value_changes_the_functions_reading_it_at_any_depth() {
    let (before, edited) = (placed_of(0, 0), placed_of(0, 1));
    assert_ne!(before[4], edited[4], "function 4 reads static 1, which names static 0");
    for n in [0, 1, 2, 3, 5] {
        assert_eq!(before[n], edited[n], "function {n} reads no static reaching static 0");
    }
}

#[test]
fn the_order_a_function_names_its_tables_in_is_not_in_its_bound() {
    let (mut built, spans) = placing(0, 0);
    built.functions[0] = compiled(key(0), &[position(1, 0), position(2, 0)], &[], &[]);
    built.functions[1] = compiled(key(1), &[position(2, 4)], &[position(1, 3)], &[]);
    built.versions[1].1 = Ok(ran(0));
    let table = ExecTable::new(&built, &spans, 0, |_| None, ContentId::default(), no_tests()).unwrap();
    assert_eq!(table.fns[0].1, table.fns[1].1);
}

#[test]
fn a_table_is_in_identity_order_and_says_each_place_in_it() {
    // 1000 functions whose `FnId`s run against their keys: function `n` is `999 - n`th.
    let numbered = |n: u32, byte: usize| {
        let mut bytes = [0u8; 32];
        bytes[byte..byte + 4].copy_from_slice(&n.to_be_bytes());
        ContentId(bytes)
    };
    let all: Vec<Target> = (0..1000).map(|n| Target::Direct(InstanceKey(numbered(n, 0)))).collect();
    let one = [Target::Function(InstanceKey(numbered(1, 0)))];
    let none: [Target; 0] = [];
    let marks = [ContentId([9; 32]); 2];
    let refs_of = |n: u32| match n {
        0 => &one[..],
        999 => &all[..],
        _ => &none[..],
    };
    let reach = Reach::new(
        (0..1000).map(|n| (InstanceKey(numbered(n, 0)), Node { marks, refs: refs_of(n) })),
        std::iter::empty(),
    );
    let built = Built {
        functions: (0..1000).map(|n| compiled(InstanceKey(numbered(n, 0)), refs_of(n), &[], &[])).collect(),
        roots: vec![InstanceKey(numbered(0, 0)), InstanceKey(numbered(999, 0))],
        inputs: (0..1000).map(|n| (InstanceKey(numbered(n, 0)), CodeHash(numbered(n, 8)))).collect(),
        versions: (0..1000).map(|n| (numbered(999 - n, 4), Ok(numbered(n, 8)))).collect(),
        reach,
        ..Built::default()
    };
    let slot_of = |key: &InstanceKey| (key.0 == numbered(0, 0)).then_some(1);
    let tests = [("few".to_owned(), 0), ("every".to_owned(), 1)];
    let table = ExecTable::new(&built, &[], 2, slot_of, ContentId::default(), tests).unwrap();
    let ids: Vec<ContentId> = table.fns.iter().map(|it| it.0).collect();
    assert_eq!(ids, (0..1000).map(|n| numbered(n, 4)).collect::<Vec<_>>());
    assert_eq!(table.fns[999].1, plain(numbered(0, 8)), "function 0 is last");
    assert_eq!(table.slot_fn, [ExecTable::EMPTY, 999]);
    // A sort for the test reaching two of 1000, the bits for the one reaching all.
    assert_eq!(table.tests[0].1, [998, 999]);
    assert_eq!(table.tests[1].1, (0..1000).collect::<Vec<u32>>());
}

#[test]
fn a_table_is_refused_when_it_would_not_say_what_ran() {
    let zero = ContentId::default();

    let mut unbound = built();
    unbound.versions[1].1 = Err("no body".to_owned());
    assert_eq!(
        ExecTable::new(&unbound, &[], 4, slots, zero, tests()),
        Err(ExecError::Unbound { function: key(1), why: "no body".to_owned() })
    );

    let mut twice = built();
    twice.versions[2].0 = id(0);
    assert_eq!(
        ExecTable::new(&twice, &[], 4, slots, zero, tests()),
        Err(ExecError::Twice { function: key(2), other: key(0) })
    );

    assert_eq!(
        ExecTable::new(&built(), &[], 2, slots, zero, tests()),
        Err(ExecError::PastTable { function: key(0), slot: 2, slots: 2 })
    );

    assert_eq!(
        ExecTable::new(&built(), &[], 4, |key| (key.0.0[0] < 2).then_some(1), zero, tests()),
        Err(ExecError::TwoInSlot { slot: 1, function: key(1), other: key(0) })
    );

    assert_eq!(
        ExecTable::new(&built(), &[], 4, slots, zero, [("gone".to_owned(), 2)]),
        Err(ExecError::NoRoot { test: "gone".to_owned(), root: 2 })
    );

    let mut later = built();
    later.functions.remove(2);
    assert_eq!(
        ExecTable::new(&later, &[], 4, slots, zero, tests()),
        Err(ExecError::NotCompiled { function: key(2) })
    );

    let (unplaced, mut spans) = placing(0, 0);
    spans.remove(1);
    assert_eq!(
        ExecTable::new(&unplaced, &spans, 0, |_| None, zero, no_tests()),
        Err(ExecError::NoSpans { function: key(4), anchor: ContentId([2; 32]) })
    );

    let (mut lacking, spans) = placing(0, 0);
    lacking.statics.remove(0);
    let error = ExecTable::new(&lacking, &spans, 0, |_| None, zero, no_tests()).unwrap_err();
    assert!(matches!(&error, ExecError::Unbound { function, .. } if *function == key(4)), "{error}");
}
