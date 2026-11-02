//! What v1's splice (`blaze-splice`, `blaze-in-place`) held true of an executable it patched,
//! as it holds of an image blaze-link keeps: the file a client runs is never written in place,
//! a crashed commit is not what the next one builds on, a static keeps its place and bytes
//! through the relinks after the one that added it, and names intern as a map would.

use blaze_codegen::{CompiledFunction, Init, Relocs, Static, Target};
use blaze_core::{Completion, Effect, FileName};
use blaze_hash::{HashMap, HashSet};
use blaze_items::ItemKey;

use super::{
    Change, Disk, Image, Linked, Sparse, TO, edit, first, function, id, key, name, output, program,
    rel32, returns,
};
use crate::LinkError;
use crate::symbols::Symbols;

const NEXT: &str = "out/app.blaze-next";

/// What a commit's effect does to which file, as far as a client running the image can tell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Touch {
    Writes,
    Removes,
    ClonedInto,
    RenamedOnto,
}

fn touched(effect: &Effect) -> Option<(Touch, &FileName)> {
    match effect {
        Effect::WriteAt { file, .. } | Effect::Write { file, .. } | Effect::Truncate { file, .. } => {
            Some((Touch::Writes, file))
        }
        Effect::Remove { file, .. } => Some((Touch::Removes, file)),
        Effect::Clone { to, .. } => Some((Touch::ClonedInto, to)),
        Effect::Rename { to, .. } => Some((Touch::RenamedOnto, to)),
        _ => None,
    }
}

/// [`Disk::link`], noting each file an effect touches and holding that the image's file is as
/// the last commit left it until the rename puts the new one there.
fn link_watching(disk: &mut Disk, image: &mut Image, change: Change<'_>) -> (Result<u64, LinkError>, Vec<(Touch, FileName)>) {
    let staged = image.stage(change).unwrap();
    let mut effects = Vec::new();
    image.commit(staged, &mut effects).unwrap();
    let before = disk.files.get(&name(TO)).cloned();
    let mut seen = Vec::new();
    loop {
        let effect = effects.pop().expect("the commit ended or asked for more");
        let tag = effect.tag();
        assert!(image.owns(tag), "{effect:?} is not the image's");
        let touch = touched(&effect).map(|(touch, file)| (touch, file.clone()));
        let done = disk.done(&effect);
        if let Some(it) = &touch {
            seen.push(it.clone());
        }
        if touch.as_ref().is_none_or(|(touch, file)| *touch != Touch::RenamedOnto || *file != name(TO)) {
            let now = disk.files.get(&name(TO));
            let same = match (&before, now) {
                (Some(before), Some(now)) => before.same(now),
                (None, None) => true,
                _ => false,
            };
            assert!(same, "{effect:?} changed {TO} before the rename");
        }
        let mut more = Vec::new();
        if let Some(result) = image.complete(&Completion { tag, done }, &mut more) {
            return (result, seen);
        }
        effects.extend(more);
    }
}

/// v1 wrote the executable in place, so a client waited for the write to end before starting
/// it (Linux refuses to run a file open for writing). blaze-link's commit writes only
/// `<image>.blaze-next` and renames it over the image: a client starting the image at any
/// moment runs the last commit's file or the new one, whole, and has nothing to wait for.
#[test]
fn a_commit_never_writes_the_images_file_in_place() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    for change in [first(&start, &g), edit(&g2)] {
        let (result, seen) = link_watching(&mut disk, &mut image, change);
        result.unwrap();
        let on_image: Vec<Touch> = seen.iter().filter(|(_, file)| *file == name(TO)).map(|it| it.0).collect();
        assert_eq!(on_image, [Touch::RenamedOnto], "the image's file is only ever renamed onto: {seen:?}");
        assert!(seen.iter().any(|(touch, file)| *touch == Touch::Writes && *file == name(NEXT)), "{seen:?}");
        assert!(!disk.files.contains_key(&name(NEXT)));
    }
    let linked = Linked::of(disk.file(TO));
    assert_eq!(linked.through_stub(linked.rel32(linked.symbol("start") + 1)), linked.symbol("g2"));
}

/// v1's journal put back what the last splice wrote over before the next began, so a splice
/// cut short left nothing behind. A commit cut short (a crash before its rename) leaves the
/// image's file as it was and a `<image>.blaze-next` of any bytes: the next commit removes
/// that before it clones the image, and makes the file a commit from a clean disk makes.
#[test]
fn a_commit_cut_short_is_not_what_the_next_one_builds_on() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let (mut crashed, mut clean) = (Disk::default(), Disk::default());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut twin = Image::new(name(TO), output(true)).unwrap();
    crashed.link(&mut image, first(&start, &g)).unwrap();
    clean.link(&mut twin, first(&start, &g)).unwrap();

    let mut left = Sparse::default();
    left.write(0, &[0xcc; 3 * 4096 + 17]);
    crashed.files.insert(name(NEXT), left);
    let (result, seen) = link_watching(&mut crashed, &mut image, edit(&g2));
    assert_eq!(result, clean.link(&mut twin, edit(&g2)));
    let on_next: Vec<Touch> = seen.iter().filter(|(_, file)| *file == name(NEXT)).map(|it| it.0).collect();
    assert_eq!(on_next.first(), Some(&Touch::Removes), "the stale next file goes first: {seen:?}");
    assert!(on_next.contains(&Touch::ClonedInto), "an incremental commit clones the image: {seen:?}");
    assert!(!crashed.files.contains_key(&name(NEXT)));
    assert!(crashed.file(TO).same(clean.file(TO)), "the commit after a crash writes what a clean one does");
}

fn static_of(n: u8, size: usize, align: u32, value: u8) -> Static {
    Static {
        item: ItemKey(id(n)),
        instance: id(n + 100),
        align,
        init: Init::Bytes { bytes: vec![value; size], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: true,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

/// Function `f`: `lea rax, [rip + static n]; ret`.
fn reads(f: u8, n: u8) -> CompiledFunction {
    let target = Target::Static { item: ItemKey(id(n)), instance: id(n + 100) };
    function(f, &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3], vec![rel32(3, target)])
}

/// v1's `Lasting`: a static new since the base keeps one place in every patch after the one it
/// came in, and the next new one gets a place of its own, aligned. A relink's change is a
/// delta, so a static added once is placed once: later relinks that add code reading it, add
/// another static or edit other functions leave it where it is, with its bytes.
#[test]
fn a_static_added_by_a_relink_keeps_its_place_and_bytes_through_later_relinks() {
    let (start, g) = program(Target::Function);
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();

    let (h, a) = (reads(4, 20), static_of(20, 4, 4, 0xaa));
    let added = Change {
        added: vec![&h],
        statics: vec![&a],
        debug_names: vec![(key(4), "h".into())],
        ..Change::default()
    };
    disk.link(&mut image, added).unwrap();
    let one = Linked::of(disk.file(TO));
    let at = one.rel32(one.symbol("h") + 3);
    assert_eq!(at % 4, 0);
    assert_eq!(one.at(at, 4), [0xaa; 4]);

    let (k, k2, b) = (reads(5, 20), reads(6, 21), static_of(21, 9, 8, 0xbb));
    let more = Change {
        added: vec![&k, &k2],
        statics: vec![&b],
        debug_names: vec![(key(5), "k".into()), (key(6), "k2".into())],
        ..Change::default()
    };
    disk.link(&mut image, more).unwrap();
    let two = Linked::of(disk.file(TO));
    assert_eq!(two.symbol("h"), one.symbol("h"));
    assert_eq!(two.rel32(two.symbol("h") + 3), at, "what read it still does, where it was");
    assert_eq!(two.rel32(two.symbol("k") + 3), at, "new code reads it where it is");
    assert_eq!(two.at(at, 4), [0xaa; 4]);
    let other = two.rel32(two.symbol("k2") + 3);
    assert_eq!(other % 8, 0);
    assert!(other + 9 <= at || at + 4 <= other, "{other:#x} overlaps {at:#x}");
    assert_eq!(two.at(other, 9), [0xbb; 9]);

    let g2 = function(3, &returns(2), Vec::new());
    disk.link(&mut image, edit(&g2)).unwrap();
    let three = Linked::of(disk.file(TO));
    assert_eq!(three.rel32(three.symbol("k") + 3), at);
    assert_eq!(three.rel32(three.symbol("k2") + 3), other);
    assert_eq!((three.at(at, 4), three.at(other, 9)), (&[0xaa; 4][..], &[0xbb; 9][..]));
}

/// v1's `names::Names`, a table of an executable's symbol names, as `symbols.rs` holds them:
/// mangled names sharing prefixes and lengths, many repeated, and the empty name, each one id
/// that reads back as its bytes.
#[test]
fn mangled_names_sharing_prefixes_intern_as_a_map_would() {
    let names: Vec<String> = (0..5000).map(|it| format!("_ZN5crate{}{}E", it % 97, "x".repeat(it % 13))).collect();
    let mut symbols = Symbols::default();
    let mut model = HashMap::<&str, _>::default();
    let mut ids = HashSet::default();
    for name in &names {
        let id = symbols.name(name);
        match model.get(name.as_str()) {
            Some(was) => assert_eq!(id, *was, "{name}"),
            None => {
                assert!(ids.insert(id), "{name} was given another name's id");
                model.insert(name.as_str(), id);
            }
        }
    }
    assert_eq!(model.len(), 97 * 13);
    for (name, id) in &model {
        assert_eq!(symbols.str(*id), *name);
        assert_eq!(symbols.find_name(name), Some(*id));
    }
    assert_eq!(symbols.find_name("_ZN5crate0E_not"), None);
    assert_eq!(symbols.find_name(""), None);
    let empty = symbols.name("");
    assert!(!ids.contains(&empty));
    assert_eq!((symbols.str(empty), symbols.find_name("")), ("", Some(empty)));
}
