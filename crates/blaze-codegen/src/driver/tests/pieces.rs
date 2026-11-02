//! [`Built::digests`]: one of each function, static and debug name of the build, sorted by
//! what it is, each its piece's own, and the same whether the piece was compiled or read back.

use super::*;

/// Each list is the build's pieces in their order, each with the digest its piece has.
fn check(built: &Built) {
    let pieces = &built.digests;
    let keys: Vec<InstanceKey> = built.functions.iter().map(|it| it.key).collect();
    let digested: Vec<InstanceKey> = pieces.functions.iter().map(|it| it.0).collect();
    assert_eq!(digested, keys, "one digest a function the build kept, by key");
    let code_of = |key: InstanceKey| built.inputs.iter().find(|it| it.0 == key).expect("a walked instance").1;
    for (function, (_, digest)) in built.functions.iter().zip(&pieces.functions) {
        assert_eq!(*digest, digests::function_digest(code_of(function.key), &function.inlined));
    }

    assert_eq!(pieces.statics.len(), built.statics.len());
    for (value, (id, digest)) in built.statics.iter().zip(&pieces.statics) {
        assert_eq!(*id, (value.item, value.instance));
        assert_eq!(*digest, static_definition(value));
    }
    assert!(pieces.statics.is_sorted_by(|a, b| a.0 < b.0));

    assert_eq!(pieces.debug_names.len(), built.debug_names.len());
    for ((key, name), (named, digest)) in built.debug_names.iter().zip(&pieces.debug_names) {
        assert_eq!(key, named);
        assert_eq!(*digest, digests::debug_name_digest(name));
    }
    assert!(pieces.debug_names.is_sorted_by(|a, b| a.0 < b.0));
}

#[test]
fn a_build_gives_a_digest_of_each_piece_and_a_hit_the_same() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = program();
    program.statics.insert(7, (0xaa, None));
    program.statics.insert(8, (0xbb, Some(".init_array".to_owned())));
    program.functions.get_mut(&4).unwrap().statics = vec![7, 8];
    let mut store = Store::default();
    let cold = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    check(&cold);
    assert_eq!(cold.digests.functions.len(), 5);
    assert_eq!(cold.digests.statics.len(), 2);
    store.keep(&cold.puts);

    let warm_machine = Machine::default();
    let warm = build(&program, &warm_machine, &policy, &Memo::new(), &store).unwrap();
    assert_eq!(warm_machine.statics.load(std::sync::atomic::Ordering::Relaxed), 0, "every static read back");
    check(&warm);
    assert_eq!(warm.digests, cold.digests, "a piece read back has its compile's digest");

    program.statics.get_mut(&7).unwrap().0 = 0xac;
    let edited = build(&program, &Machine::default(), &policy, &Memo::new(), &store).unwrap();
    check(&edited);
    let of = |built: &Built, id: u32| built.digests.statics.iter().find(|it| it.0.0 == static_item(id)).unwrap().1;
    assert_ne!(of(&edited, 7), of(&cold, 7), "a static's value is in its digest");
    assert_eq!(of(&edited, 8), of(&cold, 8));
}

#[test]
fn a_function_dedupe_merged_away_has_no_digest_but_its_debug_name_has() {
    let policy = policy(OptLevel::O0, Mode::Output);
    let mut program = Program::default();
    program.function(1, 0x11, &[2, 3]).symbol = Some("main".to_owned());
    program.function(2, 0x77, &[]);
    program.function(3, 0x77, &[]);
    let built = build(&program, &Machine::default(), &policy, &Memo::new(), &Store::default()).unwrap();
    assert_eq!(built.deduped.merged.len(), 1);
    check(&built);
    assert_eq!(built.digests.functions.len(), 2);
    assert_eq!(built.digests.debug_names.len(), 3);
}
