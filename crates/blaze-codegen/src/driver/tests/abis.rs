//! [`Described::abi`] carried to [`Built::abis`]: what a caller's code assumes of a function
//! beyond its address, which a running program keys a slot by beside the function's identity
//! (docs/v2/plan/design/launcher.md, "More than one changed body, and a changed signature").
//!
//! The program is that section's counterexample: `g` (2) builds a `Foo` (type 7) and hands a
//! reference to `h` (3), which hands it on to `f` (4), which reads it. `g` and `f` read the
//! layout, so it is in their code; `h` and `f` name the type in their signatures, so it is in
//! their `abi`; `h` reads nothing of it, so an edit of the layout leaves `h`'s code and key
//! as they were.

use std::collections::BTreeMap;

use super::*;

const FOO: u32 = 7;

fn passing_a_reference() -> Program {
    let mut program = Program::default();
    program.function(1, 0x11, &[2]).symbol = Some("main".to_owned());
    program.function(2, 0x22, &[3]).reads_layouts = vec![FOO];
    program.function(3, 0x33, &[4]).names_layouts = vec![FOO];
    let f = program.function(4, 0x44, &[]);
    (f.names_layouts, f.reads_layouts) = (vec![FOO], vec![FOO]);
    program
}

fn built(program: &Program, kept: Option<&Kept<u32, u32>>) -> Built {
    program.begin_build();
    let policy = policy(OptLevel::O0, Mode::Output);
    let (machine, memo, store) = (Machine::default(), Memo::new(), Store::default());
    let build = Build::new(&machine, program, &policy, &memo);
    let mut build = match kept {
        Some(kept) => build.keeping(kept),
        None => build,
    };
    build.run(&[(1, member())], &store).expect("the program builds")
}

/// Each function by its name (`f1`..), with one list's value of it.
fn named<T: Copy>(built: &Built, list: &[(InstanceKey, T)]) -> BTreeMap<String, T> {
    let names: BTreeMap<InstanceKey, &str> =
        built.debug_names.iter().map(|(key, name)| (*key, name.as_str())).collect();
    list.iter().map(|(key, value)| (names[key].to_owned(), *value)).collect()
}

/// The names of the functions whose value in `list` is another in `after` than in `before`.
fn changed<T: Copy + PartialEq>(
    before: &Built,
    after: &Built,
    list: impl Fn(&Built) -> BTreeMap<String, T>,
) -> Vec<String> {
    let (before, after) = (list(before), list(after));
    assert_eq!(before.len(), after.len(), "the two builds reach the same functions");
    (before.iter())
        .filter(|(name, value)| after.get(*name) != Some(*value))
        .map(|(name, _)| name.clone())
        .collect()
}

fn abis(built: &Built) -> BTreeMap<String, ContentId> {
    named(built, &built.abis)
}

fn identities(built: &Built) -> BTreeMap<String, ContentId> {
    named(built, &built.identities)
}

fn codes(built: &Built) -> BTreeMap<String, CodeHash> {
    named(built, &built.inputs)
}

/// The invariant: `Built::abis` holds one entry a function the walk reached, by key, in key
/// order, each its describe's `abi`.
#[test]
fn every_function_of_a_build_has_its_abi_by_key() {
    let program = passing_a_reference();
    let built = built(&program, None);
    let keys = |list: &[(InstanceKey, ContentId)]| list.iter().map(|it| it.0).collect::<Vec<_>>();
    assert_eq!(keys(&built.abis), built.inputs.iter().map(|it| it.0).collect::<Vec<_>>());
    assert!(built.abis.is_sorted_by_key(|it| it.0));
    for (id, name) in [(1, "f1"), (2, "f2"), (3, "f3"), (4, "f4")] {
        assert_eq!(abis(&built)[name], program.abi(id), "{name}");
    }
}

/// An edit of a body changes that function's code and no function's `abi` or identity: no
/// caller's assumption changed.
#[test]
fn a_body_edit_changes_no_abi() {
    let mut program = passing_a_reference();
    let before = built(&program, None);
    program.functions.get_mut(&4).unwrap().body = 0x45;
    let after = built(&program, None);
    assert_eq!(changed(&before, &after, codes), ["f4"]);
    assert_eq!(changed(&before, &after, abis), [] as [&str; 0]);
    assert_eq!(changed(&before, &after, identities), [] as [&str; 0]);
}

/// An edit of a function's signature changes its `abi` and leaves its identity as it was;
/// its caller's code changes (it is compiled against the signature), its caller's `abi` does
/// not.
#[test]
fn a_signature_edit_keeps_the_identity_and_changes_the_abi() {
    let mut program = passing_a_reference();
    let before = built(&program, None);
    program.functions.get_mut(&4).unwrap().signature = 9;
    let after = built(&program, None);
    assert_eq!(changed(&before, &after, abis), ["f4"]);
    assert_eq!(changed(&before, &after, identities), [] as [&str; 0]);
    assert_eq!(changed(&before, &after, codes), ["f3", "f4"]);
}

/// The counterexample: a layout change passed through an unchanged middle function. `h`
/// (`f3`) keeps its code, and its `abi` changes with that of every function whose signature
/// names the type; `g` (`f2`), which reads the layout and names it in no signature, changes
/// its code and keeps its `abi`. Reading "whose callers must change" off the code hashes
/// would leave `h` out, which is the shortcut the design refuses: the last assertion is that
/// control (`h`'s code is unchanged, so only `abi` tells).
#[test]
fn a_layout_change_changes_the_abi_of_every_function_that_names_the_type() {
    let mut program = passing_a_reference();
    let before = built(&program, None);
    program.layouts.insert(FOO, 1);
    let after = built(&program, None);
    assert_eq!(changed(&before, &after, abis), ["f3", "f4"]);
    assert_eq!(changed(&before, &after, identities), [] as [&str; 0]);
    let codes = changed(&before, &after, codes);
    assert_eq!(codes, ["f2", "f4"]);
    assert!(!codes.contains(&"f3".to_owned()), "the code hashes alone leave the middle out");
}

/// The same edit over a kept walk: the build after it holds the `abi`s a fresh build holds,
/// the middle function's too, though its key and code are what they were.
#[test]
fn a_kept_walk_carries_an_abi_that_changed_alone() {
    let mut program = passing_a_reference();
    let kept = Kept::new();
    let before = built(&program, Some(&kept));
    program.layouts.insert(FOO, 1);
    // What the engine sees of the edit: every describe that reads the layout.
    for id in [2, 3, 4] {
        program.touch(id);
    }
    let after = built(&program, Some(&kept));
    let fresh = built(&program.copy(), None);
    assert_eq!(after.abis, fresh.abis, "a kept walk's `abis` are a fresh build's");
    assert_eq!(changed(&before, &after, abis), ["f3", "f4"]);
}
