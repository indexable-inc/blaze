//! Identical code folding through a full ELF link (docs/v2/plan/design/icf.md): codegen's
//! dedupe hands the linker one copy of equal code and renames every relocation to it, the
//! static that points at both instances included, so both pointers are one address; a function
//! whose address is significant keeps its own copy; the image is the same bytes whatever
//! order the build's pieces came in.

use blaze_codegen::dedupe::dedupe;
use blaze_codegen::{CompiledFunction, Init, InstanceKey, RelocKind, Relocation, Static, Target};
use blaze_ids::ContentId;
use blaze_items::ItemKey;

use super::{
    Change, Disk, Image, Linked, Sparse, TO, function, id, key, name, output, rel32, returns,
};

/// `lea rax, [rip + table]; ret`
const START: [u8; 8] = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3];

/// A static holding a pointer to each of `to`, in order: what a `static T: [fn() -> u32; 3]`
/// of three instances is.
fn table(to: &[u8]) -> Static {
    let relocs = to.iter().enumerate().map(|(at, n)| Relocation {
        at: 8 * at as u32,
        kind: RelocKind::Abs64,
        target: Target::Function(key(*n)),
        addend: 0,
    });
    Static {
        item: ItemKey(id(40)),
        instance: ContentId::default(),
        align: 8,
        init: Init::Bytes {
            bytes: vec![0; 8 * to.len()],
            relocs: relocs.collect::<Vec<_>>().into(),
        },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    }
}

/// `_start` (1) and three instances that return 7: 2 and 3 may fold, 4 is exported.
fn build() -> (Vec<CompiledFunction>, Vec<Static>) {
    let start = function(
        1,
        &START,
        vec![rel32(3, Target::Static { item: ItemKey(id(40)), instance: ContentId::default() })],
    );
    let functions = vec![
        start,
        function(2, &returns(7), Vec::new()),
        function(3, &returns(7), Vec::new()),
        function(4, &returns(7), Vec::new()),
    ];
    (functions, vec![table(&[2, 3, 4])])
}

fn exported(it: &InstanceKey) -> bool {
    *it == key(4)
}

/// Dedupes and links `functions` and `statics` into a fresh image: its file.
fn link(mut functions: Vec<CompiledFunction>, mut statics: Vec<Static>) -> Sparse {
    let (deduped, renamed) =
        dedupe(&mut functions, &mut statics, |it| !exported(it) && *it != key(1)).unwrap();
    assert_eq!(deduped.merged.into_iter().collect::<Vec<_>>(), [(key(3), key(2))]);
    assert_eq!(renamed, [0], "the table named 3");
    assert_eq!(deduped.bytes, returns(7).len() as u64);
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: functions.iter().collect(),
        statics: statics.iter().collect(),
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![
            (key(1), "start".into()),
            (key(2), "f2".into()),
            (key(3), "f3".into()),
            (key(4), "f4".into()),
        ],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    disk.file(TO).clone()
}

#[test]
fn equal_instances_are_one_address_and_an_exported_one_keeps_its_own() {
    let (functions, statics) = build();
    let file = link(functions, statics);
    let linked = Linked::of(&file);
    let start = linked.symbol("start");
    let at = linked.rel32(start + 3);
    let (two, three, four) = (linked.u64_at(at), linked.u64_at(at + 8), linked.u64_at(at + 16));
    assert_eq!(two, three, "the folded instance's pointer is the kept one's");
    assert_eq!(two, linked.symbol("f2"));
    assert_ne!(four, two, "an exported function is never folded");
    assert_eq!(four, linked.symbol("f4"));
    assert_eq!(linked.at(two, 6), returns(7));
    assert_eq!(linked.at(four, 6), returns(7));
}

#[test]
fn a_folded_image_is_the_same_bytes_whatever_order_its_pieces_came_in() {
    let (functions, statics) = build();
    let want = link(functions, statics);
    let (mut functions, statics) = build();
    functions.reverse();
    let got = link(functions, statics);
    assert!(want.same(&got), "folding depends on the order of the build's functions");
}
