//! Profile-guided function order through a full ELF link (docs/v2/plan/design/code-layout.md):
//! a profile's hot functions first in call order and the cold ones last, the same bytes
//! whatever order the profile and cold set came in, and an incremental link moving no
//! function for a profile.

use blaze_codegen::{CompiledFunction, InstanceKey, Target};

use super::{Change, Disk, Image, TO, function, key, name, output, rel32, returns};
use crate::Heat;

/// `_start` (1) calling 2 to 7 in turn, each returning its number.
fn program() -> Vec<CompiledFunction> {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for n in 2..=7u8 {
        relocs.push(rel32(code.len() as u32 + 1, Target::Direct(key(n))));
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]);
    }
    code.push(0xc3);
    let mut out = vec![function(1, &code, relocs)];
    out.extend((2..=7).map(|n| function(n, &returns(n), Vec::new())));
    out
}

fn change<'a>(
    functions: &'a [CompiledFunction],
    heat: Option<&'a Heat>,
    cold: Vec<InstanceKey>,
) -> Change<'a> {
    Change {
        added: functions.iter().collect(),
        names: vec![("_start".into(), Target::Direct(key(1)))],
        heat,
        cold,
        ..Change::default()
    }
}

/// A fresh image of `change`: its functions' keys in address order, and its file.
fn link(change: Change<'_>) -> (Vec<InstanceKey>, Vec<u8>) {
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let staged = image.stage(change).unwrap();
    let bytes = image.bytes(staged).unwrap();
    (image.functions().into_iter().map(|it| it.0).collect(), bytes)
}

/// 6 runs most and calls 3, which runs nearly as much: 3 right after 6, both before the rest.
fn heat(reversed: bool) -> Heat {
    let mut functions = vec![(key(6), 100), (key(3), 80), (key(7), 1)];
    let mut calls = vec![(key(6), key(3), 80), (key(1), key(7), 1)];
    if reversed {
        functions.reverse();
        calls.reverse();
    }
    Heat::new(functions, calls)
}

/// The profile's functions first, 3 after its caller 6; then the rest by content; the cold
/// ones the profile never saw last; and 7, which it saw run, hot though marked cold.
#[test]
fn a_profile_packs_hot_functions_first_and_cold_last() {
    let functions = program();
    let heat = heat(false);
    let (order, _) = link(change(&functions, Some(&heat), vec![key(5), key(2), key(7)]));
    assert_eq!(order, [key(6), key(3), key(1), key(7), key(4), key(2), key(5)]);
    let (plain, _) = link(change(&functions, None, Vec::new()));
    assert_eq!(plain, (1..=7).map(key).collect::<Vec<_>>(), "without either, by content as before");
}

/// The same functions, profile and cold set give the same file whatever order they came in.
#[test]
fn the_same_tree_and_profile_give_the_same_bytes() {
    let functions = program();
    let mut backwards = program();
    backwards.reverse();
    let (forward, reverse) = (heat(false), heat(true));
    assert_eq!(forward.id(), reverse.id());
    let (_, one) = link(change(&functions, Some(&forward), vec![key(5), key(2)]));
    let (_, two) = link(change(&backwards, Some(&reverse), vec![key(2), key(5), key(2)]));
    assert!(one == two, "two links of one tree and profile differ");
    let (_, plain) = link(change(&functions, None, Vec::new()));
    assert!(one != plain, "the profile changed nothing");
}

/// An incremental link places new code in the patch region: a profile it is given moves no
/// function the image has.
#[test]
fn an_incremental_link_moves_nothing_for_a_profile() {
    let functions = program();
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, change(&functions, None, Vec::new())).unwrap();
    let before = image.functions();
    let heat = heat(false);
    disk.link(&mut image, Change { heat: Some(&heat), cold: vec![key(1)], ..Change::default() })
        .unwrap();
    assert_eq!(image.functions(), before);
}
