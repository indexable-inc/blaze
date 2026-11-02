//! What a lazy build compiles ahead (the entry, what initializer and finalizer sections
//! name), the lazy functions' ids, history's reached set and a function's likely callees.

use blaze_codegen::{Data, Init, InstanceKey, RelocKind, Relocation, Static, Target};
use blaze_items::{CodeHash, ItemKey};

use super::{compiled, function, id, key};
use crate::{
    LazyId,
    plan::{Ahead, Function, Plan, PlanError, runs_at_start},
    session::Session,
};

fn starts(section: &str, names: &[u32], in_data: &[u32]) -> Static {
    let reloc = |n: &u32| Relocation { at: 8 * *n, kind: RelocKind::Abs64, target: Target::Function(key(*n)), addend: 0 };
    Static {
        item: ItemKey(id("blaze-lazy test static", names.first().copied().unwrap_or(0))),
        instance: id("blaze-lazy test static instance", 0),
        align: 8,
        init: Init::Bytes { bytes: vec![0; 8 * 64], relocs: names.iter().map(reloc).collect() },
        data: vec![Data {
            id: id("blaze-lazy test data", 0),
            align: 8,
            bytes: vec![0; 8 * 64],
            relocs: in_data.iter().map(reloc).collect(),
            writable: false,
        }],
        mutable: false,
        thread_local: false,
        section: Some(section.to_owned()),
        imports: Vec::new(),
    }
}

fn functions(count: u32) -> Vec<Function> {
    (0..count).map(function).collect()
}

#[test]
fn the_entry_and_what_runs_at_start_are_compiled_ahead() {
    let statics = [
        starts(".init_array", &[3], &[]),
        starts("__DATA,__mod_init_func", &[4], &[5]),
        starts(".rodata", &[6], &[]),
        starts(".CRT$XLB", &[7], &[]),
    ];
    let plan = Plan::new(&functions(10), &[key(0), key(1)], &statics).unwrap();
    let ahead: Vec<(InstanceKey, Ahead)> = plan.ahead().to_vec();
    assert_eq!(ahead, vec![
        (key(0), Ahead::Entry),
        (key(1), Ahead::Entry),
        (key(3), Ahead::Start),
        (key(4), Ahead::Start),
        (key(5), Ahead::Start),
        (key(7), Ahead::Start),
    ]);
    let lazy: Vec<InstanceKey> = plan.lazy().to_vec();
    assert_eq!(lazy, vec![key(2), key(6), key(8), key(9)], "in the walk's order, a static elsewhere names a lazy one");
    for (n, key) in lazy.iter().enumerate() {
        assert_eq!(plan.id(key), Some(LazyId(n as u32)));
        assert_eq!(plan.key(LazyId(n as u32)), *key);
    }
    assert_eq!(plan.id(&key(0)), None);
}

#[test]
fn a_function_the_walk_lists_twice_has_one_id_and_an_unknown_entry_is_an_error() {
    let mut walk = functions(3);
    walk.push(function(1));
    let plan = Plan::new(&walk, &[], &[]).unwrap();
    assert_eq!(plan.len(), 3);
    assert_eq!(Plan::new(&walk, &[key(9)], &[]).unwrap_err(), PlanError::Unknown(key(9)));
}

#[test]
fn sections_that_run_code_at_start_or_exit() {
    for yes in [
        ".init_array",
        ".init_array.00099",
        ".preinit_array",
        ".fini_array",
        ".ctors.65535",
        ".dtors",
        "__DATA,__mod_init_func",
        "__DATA,__mod_term_func",
        ".CRT$XCU",
        ".CRT$XLB",
    ] {
        assert!(runs_at_start(yes), "{yes}");
    }
    for no in [".data", ".init_arrays", ".rodata", "__DATA,__const", ".CRT", ".text.init_array"] {
        assert!(!runs_at_start(no), "{no}");
    }
}

#[test]
fn history_names_lazy_functions_by_code_hash() {
    let mut walk = functions(5);
    // Two instances of one body (other type arguments): both are reached.
    walk.push(Function { key: key(50), code: walk[2].code, item: walk[2].item });
    let plan = Plan::new(&walk, &[key(0)], &[]).unwrap();
    let mut out = Vec::new();
    plan.reached(&[walk[2].code, walk[0].code, CodeHash(id("blaze-lazy test code", 99))], &mut out);
    out.sort();
    assert_eq!(out, vec![plan.id(&key(2)).unwrap(), plan.id(&key(50)).unwrap()]);
}

#[test]
fn history_of_a_large_plan_is_every_instance_of_each_body_in_id_order() {
    let mut rng = super::Rng::new(7);
    let walk: Vec<Function> = (0..6000)
        .map(|n| Function {
            key: key(n),
            code: CodeHash(id("blaze-lazy test code", rng.below(700) as u32)),
            item: ItemKey(id("blaze-lazy test item", n)),
        })
        .collect();
    let plan = Plan::new(&walk, &[], &[]).unwrap();
    for body in [0u32, 1, 350, 699, 700] {
        let code = CodeHash(id("blaze-lazy test code", body));
        let mut out = Vec::new();
        plan.reached(&[code], &mut out);
        let expected: Vec<LazyId> =
            walk.iter().filter(|it| it.code == code).map(|it| plan.id(&it.key).unwrap()).collect();
        assert_eq!(out, expected, "body {body}");
    }
}

#[test]
fn callees_are_calls_and_tail_calls_not_addresses_taken() {
    let plan = Plan::new(&functions(6), &[key(0)], &[]).unwrap();
    let mut caller = compiled(0, &[1, 2, 1], &[3]);
    let jump = caller.code.len() as u32;
    caller.code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
    caller.relocs.push(Relocation { at: jump + 1, kind: RelocKind::PcRel32, target: Target::Direct(key(4)), addend: -4 });
    caller.relocs.push(Relocation { at: 0, kind: RelocKind::Abs64, target: Target::Function(key(5)), addend: 0 });
    let (mut out, mut seen) = (Vec::new(), plan.scratch());
    plan.callees(&caller, &mut seen, &mut out);
    let ids = |ns: &[u32]| ns.iter().map(|n| plan.id(&key(*n)).unwrap()).collect::<Vec<_>>();
    assert_eq!(out, ids(&[1, 2, 4]));
    assert!(seen.iter().all(|word| *word == 0), "the scratch is left clear");
    plan.callees(&caller, &mut seen, &mut out);
    assert_eq!(out, ids(&[1, 2, 4, 1, 2, 4]), "each call appends each callee once");
}

#[test]
fn a_session_fetches_the_entrys_callees_then_history_each_once() {
    use crate::fetch::Priority::{Callee, History};

    let walk = functions(6);
    let session: Session = super::flight::session(&walk);
    let plan = session.plan();
    let ids = |ns: &[u32]| ns.iter().map(|n| plan.id(&key(*n)).unwrap()).collect::<Vec<_>>();
    let (callee, history) = (ids(&[1, 2]), ids(&[2, 5]));
    let entry = compiled(0, &[1, 2], &[4]);
    let first = session.first(&[entry], &[walk[2].code, walk[5].code]);
    assert_eq!(first, vec![(callee[0], Callee), (callee[1], Callee), (history[1], History)]);
}
