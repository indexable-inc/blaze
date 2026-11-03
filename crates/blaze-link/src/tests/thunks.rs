//! AArch64 branches past their 128 MiB reach (`crate::thunk`): a caller and its callee with a
//! 200 MiB function between them link, each `bl` landing on a thunk that jumps to its target,
//! in a release image (direct calls) and a dev image with a jump table (calls to slot stubs,
//! past the patch room too), and in an incremental link's new code.

use blaze_codegen::{Arch, CompiledFunction, RelocKind, Relocation, Target};

use super::{Change, Disk, Image, Linked, TO, function, key, name, output};
use crate::Output;
use crate::symbols::SymKey;

const MIB: usize = 1 << 20;
/// `ret`.
const RET: [u8; 4] = [0xc0, 0x03, 0x5f, 0xd6];

fn a64(incremental: bool) -> Output {
    Output {
        arch: Arch::Aarch64,
        interpreter: Some("/lib/ld-linux-aarch64.so.1".into()),
        triple: "aarch64-unknown-linux-gnu".into(),
        ..output(incremental)
    }
}

/// `bl to; ret`.
fn caller(n: u8, to: Target) -> CompiledFunction {
    let call = Relocation { at: 0, kind: RelocKind::Call26, target: to, addend: 0 };
    let code = [[0, 0, 0, 0x94], RET].concat();
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, vec![call]) }
}

/// 200 MiB of code ending in `ret`: more than a `bl` reaches, between the callers.
fn filler(n: u8) -> CompiledFunction {
    let mut code = vec![0u8; 200 * MIB];
    let end = code.len() - 4;
    code[end..].copy_from_slice(&RET);
    CompiledFunction { arch: Arch::Aarch64, ..function(n, &code, Vec::new()) }
}

/// Where the `bl` at `at` goes.
fn bl(linked: &Linked, at: u64) -> u64 {
    let word = linked.u32_at(at);
    assert_eq!(word >> 26, 0x25, "a `bl` at {at:#x}: {word:#010x}");
    let imm = ((word & 0x03ff_ffff) << 6) as i32 >> 6;
    at.wrapping_add((i64::from(imm) * 4) as u64)
}

/// Where the thunk at `at` (`adrp x16; add x16, x16, lo12; br x16`) jumps.
fn thunk(linked: &Linked, at: u64) -> u64 {
    let (adrp, add, br) = (linked.u32_at(at), linked.u32_at(at + 4), linked.u32_at(at + 8));
    assert_eq!(adrp & 0x9f00_001f, 0x9000_0010, "`adrp x16` at {at:#x}: {adrp:#010x}");
    assert_eq!(add & 0xffc0_03ff, 0x9100_0210, "`add x16, x16, #lo12` at {at:#x}: {add:#010x}");
    assert_eq!(br, 0xd61f_0200, "`br x16` at {:#x}", at + 8);
    let pages = (((adrp >> 5) & 0x7_ffff) << 2 | (adrp >> 29) & 3) as i64;
    let pages = pages << 43 >> 43;
    let page = (at & !0xfff).wrapping_add((pages << 12) as u64);
    page + u64::from((add >> 10) & 0xfff)
}

/// The address of `n`'s slot stub, or its code without a table.
fn callee(image: &Image, linked: &Linked, n: u8, debug: &str) -> u64 {
    let Some(id) = image.symbols.find_id(&key(n).0) else { panic!("no function {n}") };
    match image.symbols.find(SymKey::Slot(id)).and_then(|it| image.state.addr_of(it)) {
        Some(stub) => stub,
        None => linked.symbol(debug),
    }
}

#[test]
fn calls_200_mib_apart_go_through_thunks() {
    for incremental in [false, true] {
        let to = if incremental { Target::Function } else { Target::Direct };
        let (one, big, three) = (caller(1, to(key(3))), filler(2), caller(3, to(key(1))));
        let mut image = Image::new(name(TO), a64(incremental)).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&one, &big, &three],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![(key(1), "one".into()), (key(2), "big".into()), (key(3), "three".into())],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap_or_else(|it| panic!("incremental {incremental}: {it}"));
        let linked = Linked::of(disk.file(TO));
        let (a, c) = (linked.symbol("one"), linked.symbol("three"));
        assert!(c - a > 200 * MIB as u64, "incremental {incremental}: 200 MiB between them");
        for (from, n, debug) in [(a, 3, "three"), (c, 1, "one")] {
            let via = bl(&linked, from);
            assert!(via.abs_diff(from) < 1 << 27, "incremental {incremental}: the `bl` at {from:#x} reaches {via:#x}");
            assert_eq!(thunk(&linked, via), callee(&image, &linked, n, debug), "incremental {incremental}: from {from:#x}");
        }
        if !incremental {
            continue;
        }
        // New code in the patch region, 200 MiB of room away from the slot stubs: a thunk of
        // its own island, in the patch region after it.
        // It calls `big`, whose slot no thunk serves yet (a thunk to `one`'s at the end of the
        // code would be in reach, and used).
        let four = caller(4, Target::Function(key(2)));
        let edit = Change {
            added: vec![&four],
            removed: vec![key(3)],
            slots: vec![(key(3), key(4))],
            debug_names: vec![(key(4), "four".into())],
            ..Change::default()
        };
        let islands = image.state.islands;
        disk.link(&mut image, edit).unwrap_or_else(|it| panic!("the edit: {it}"));
        assert_eq!(image.state.islands, islands + 1, "the edit's own island");
        let linked = Linked::of(disk.file(TO));
        let d = linked.symbol("four");
        let via = bl(&linked, d);
        assert!(via > d && via - d < 1 << 27, "the edit's thunk is after its code: {via:#x}");
        assert_eq!(thunk(&linked, via), callee(&image, &linked, 2, "big"));
    }
}

#[test]
fn an_image_with_islands_is_the_same_whatever_order_its_functions_came_in() {
    let (one, big, three) = (caller(1, Target::Direct(key(3))), filler(2), caller(3, Target::Direct(key(1))));
    let link = |added: Vec<&CompiledFunction>| {
        let mut image = Image::new(name(TO), a64(false)).unwrap();
        let mut disk = Disk::default();
        let names = vec![("_start".into(), Target::Direct(key(1)))];
        disk.link(&mut image, Change { added, names, ..Change::default() }).unwrap();
        disk
    };
    let (first, second) = (link(vec![&one, &big, &three]), link(vec![&three, &big, &one]));
    assert!(first.file(TO).same(second.file(TO)));
}
