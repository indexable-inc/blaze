//! The library format (write then read, every corruption refused, the perfect index) and the
//! libraries blaze was built with.

use blaze_ids::rng::SplitMix64;

use super::{
    format::{FormatError, Writer},
    *,
};
use crate::{
    Os, TlsModel,
    enc::Form,
    shape::{self, Isa, Shape},
};

fn shapes(isa: Isa) -> Vec<Shape> {
    let mut shapes = Vec::new();
    shape::each_on(isa, |shape| shapes.push(shape));
    shapes
}

/// The shapes a library of `variant` holds: its set, less what [`Variant::has`] leaves out.
fn shapes_of(variant: Variant) -> Vec<Shape> {
    shapes(variant.isa()).into_iter().filter(|it| variant.has(*it)).collect()
}

/// Stencil `i`'s made-up code: its length varies, and its last 5 bytes are a `jmp rel32`.
fn code(i: usize) -> Vec<u8> {
    let mut code = vec![i as u8; 4 + i % 7];
    code.extend_from_slice(&[0xe9, 0, 0, 0, 0]);
    code
}

/// Stencil `i`'s made-up faults: two for every fourth.
fn faults(i: usize) -> Vec<u16> {
    if i % 4 == 0 { vec![1, 4] } else { Vec::new() }
}

/// An x86-64 library of the first `n` shapes of the set, each with a `Slot0` hole, a `Next`
/// hole in its tail, every third a `Rodata` hole and every fourth two faults.
fn library(n: usize) -> (Vec<Shape>, Vec<u8>) {
    let shapes: Vec<Shape> = shapes(Isa::X86_64).into_iter().take(n).collect();
    let mut writer = Writer::new(Variant::X86_64);
    for (i, shape) in shapes.iter().enumerate() {
        let code = code(i);
        let len = code.len() as u16;
        let mut holes = vec![
            Hole { at: 0, form: Form::Abs32S, kind: HoleKind::Slot0, addend: i as i32 },
            Hole { at: len - 4, form: Form::Rel32, kind: HoleKind::Next, addend: -4 },
        ];
        if i % 3 == 0 {
            let entry = writer.rodata(&[(i % 5) as u8; 8], 8).unwrap();
            let addend = Hole::rodata_addend(entry, -4).unwrap();
            holes.push(Hole { at: 0, form: Form::Rel32, kind: HoleKind::Rodata, addend });
        }
        assert_eq!(writer.push_with_faults(*shape, &code, 5, &holes, &faults(i)), Ok(i as u32));
    }
    let bytes = writer.finish(|bytes| {
        let mut id = [0u8; 32];
        for (at, byte) in bytes.iter().enumerate() {
            id[at % 32] = id[at % 32].wrapping_mul(31).wrapping_add(*byte);
        }
        id
    });
    (shapes, bytes.unwrap())
}

#[test]
fn a_written_library_reads_back() {
    let (shapes, bytes) = library(500);
    let reader = format::Reader::new(&bytes).unwrap();
    assert_eq!(reader.variant(), Variant::X86_64);
    assert_eq!(reader.len(), 500);
    assert_eq!(reader.entries(), 5);
    for (i, shape) in shapes.iter().enumerate() {
        let index = reader.find(shape.key()).unwrap_or_else(|| panic!("{shape} not found"));
        assert_eq!(index, i as u32, "{shape}");
        assert_eq!(reader.key(index), shape.key());
        let stencil = reader.stencil(index);
        assert_eq!(stencil.code, code(i).as_slice());
        assert_eq!(stencil.tail, 5);
        assert_eq!(stencil.body(), &code(i)[..code(i).len() - 5]);
        assert_eq!(stencil.faults().collect::<Vec<_>>(), faults(i));
        let holes: Vec<Hole> = stencil.holes().collect();
        assert_eq!(holes.len(), if i % 3 == 0 { 3 } else { 2 });
        assert_eq!(holes[0], Hole { at: 0, form: Form::Abs32S, kind: HoleKind::Slot0, addend: i as i32 });
        assert_eq!(holes[1].kind, HoleKind::Next);
        if i % 3 == 0 {
            let (entry, addend) = holes[2].rodata();
            assert_eq!(addend, -4);
            let mut expected = [0u8; format::ENTRY];
            expected[..8].fill((i % 5) as u8);
            assert_eq!(reader.entry(entry), expected);
        }
    }
}

#[test]
fn writing_is_deterministic() {
    assert_eq!(library(300).1, library(300).1);
}

#[test]
fn the_index_finds_every_key_of_a_full_set_and_no_other() {
    for isa in Isa::ALL {
        let variant = if isa == Isa::X86_64 { Variant::X86_64 } else { Variant::Aarch64 };
        let set = shapes_of(variant);
        let mut writer = Writer::new(variant);
        let code = [0u8; 4];
        for shape in &set {
            writer.push(*shape, &code, 0, &[]).unwrap();
        }
        let bytes = writer.finish(|_| [7; 32]).unwrap();
        let reader = format::Reader::new(&bytes).unwrap();
        assert_eq!(reader.id(), [7; 32]);
        let keys: std::collections::HashSet<u32> = set.iter().map(|it| it.key()).collect();
        for (i, shape) in set.iter().enumerate() {
            assert_eq!(reader.find(shape.key()), Some(i as u32), "{shape}");
        }
        // Shapes of the other set, and keys that are no shape.
        let other = if isa == Isa::X86_64 { Isa::Aarch64 } else { Isa::X86_64 };
        for shape in shapes(other) {
            if !keys.contains(&shape.key()) {
                assert_eq!(reader.find(shape.key()), None, "{shape}");
            }
        }
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for _ in 0..100_000 {
            let key = rng.next_u32();
            if !keys.contains(&key) {
                assert_eq!(reader.find(key), None, "{key:#x}");
            }
        }
    }
}

#[test]
fn an_empty_library_finds_nothing() {
    let bytes = Writer::new(Variant::Aarch64Lse).finish(|_| [0; 32]).unwrap();
    let reader = format::Reader::new(&bytes).unwrap();
    assert!(reader.is_empty());
    assert_eq!(reader.find(0), None);
}

#[test]
fn the_writer_refuses_what_the_reader_would() {
    let set = shapes(Isa::X86_64);
    let mut writer = Writer::new(Variant::X86_64);
    writer.push(set[0], &[0; 8], 0, &[]).unwrap();
    assert_eq!(writer.push(set[0], &[0; 8], 0, &[]), Err(FormatError::Shape(set[0].key())));
    let a64_only = shapes(Isa::Aarch64).into_iter().find(|it| !it.exists_on(Isa::X86_64)).unwrap();
    assert_eq!(writer.push(a64_only, &[0; 8], 0, &[]), Err(FormatError::Shape(a64_only.key())));
    let past = Hole { at: 6, form: Form::Abs32S, kind: HoleKind::Slot0, addend: 0 };
    assert_eq!(writer.push(set[1], &[0; 8], 0, &[past]), Err(FormatError::Bounds("a hole")));
    let place_in_number = Hole { at: 0, form: Form::Abs32S, kind: HoleKind::Next, addend: 0 };
    assert_eq!(writer.push(set[1], &[0; 8], 0, &[place_in_number]), Err(FormatError::Form(Form::Abs32S as u8)));
    assert_eq!(writer.push(set[1], &[0; 8], 9, &[]), Err(FormatError::Bounds("a stencil's code")));
    let entry = Hole { at: 0, form: Form::Rel32, kind: HoleKind::Rodata, addend: Hole::rodata_addend(3, 0).unwrap() };
    assert_eq!(writer.push(set[1], &[0; 8], 0, &[entry]), Err(FormatError::Bounds("a rodata hole's entry")));
    assert_eq!(writer.rodata(&[0; 17], 1), Err(FormatError::Bounds("a rodata entry")));
    assert_eq!(writer.push_with_faults(set[1], &[0; 8], 0, &[], &[8]), Err(FormatError::Bounds("a fault")));
    let mut a64 = Writer::new(Variant::Aarch64);
    let shape = shapes(Isa::Aarch64)[0];
    assert_eq!(a64.push_with_faults(shape, &[0; 8], 0, &[], &[2]), Err(FormatError::Bounds("a fault")));
    assert_eq!(a64.push_with_faults(shape, &[0; 8], 0, &[], &[4]), Ok(0));
}

#[test]
fn rodata_entries_are_shared_and_their_addends_round_trip() {
    let mut writer = Writer::new(Variant::X86_64);
    let a = writer.rodata(&[1, 2, 3], 4).unwrap();
    let b = writer.rodata(&[9; 16], 16).unwrap();
    assert_eq!(writer.rodata(&[1, 2, 3, 0], 4), Ok(a));
    assert_ne!(a, b);
    for (entry, addend) in [(0, 0), (1, -128), (5, 127), ((1 << 24) - 1, -4)] {
        let hole = Hole {
            at: 0,
            form: Form::Rel32,
            kind: HoleKind::Rodata,
            addend: Hole::rodata_addend(entry, addend).unwrap(),
        };
        assert_eq!(hole.rodata(), (entry, addend));
    }
    assert_eq!(Hole::rodata_addend(1 << 24, 0), None);
    assert_eq!(Hole::rodata_addend(0, 128), None);
    assert_eq!(Hole::rodata_addend(0, -129), None);
}

/// Byte offsets of the format's fields, from its module docs.
const STENCILS: usize = format::HEADER;

fn count(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn holes_at(bytes: &[u8]) -> usize {
    STENCILS + count(bytes, 20) * format::STENCIL
}

fn faults_at(bytes: &[u8]) -> usize {
    holes_at(bytes) + count(bytes, 24) * format::HOLE
}

/// Where the index's slots start: after the faults and the pilots, 4-aligned.
fn slots_at(bytes: &[u8]) -> usize {
    (faults_at(bytes) + count(bytes, 48) * format::FAULT + count(bytes, 28) * 2).next_multiple_of(4)
}

fn refused(bytes: &[u8]) -> FormatError {
    format::Reader::new(bytes).expect_err("a corrupt library was read")
}

#[test]
fn every_corruption_is_refused() {
    let (_, bytes) = library(40);
    assert!(format::Reader::new(&bytes).is_ok());

    for len in [0, 7, 16, format::HEADER - 1, format::HEADER, bytes.len() / 2, bytes.len() - 1] {
        assert!(matches!(refused(&bytes[..len]), FormatError::Bounds(_)), "truncated to {len}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert_eq!(refused(&longer), FormatError::Bounds("the columns"));

    let edit = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut it = bytes.clone();
        f(&mut it);
        refused(&it)
    };
    assert_eq!(edit(&|it| it[0] = b'X'), FormatError::Magic);
    assert!(matches!(edit(&|it| it[8] ^= 1), FormatError::Version { .. }));
    assert!(matches!(edit(&|it| it[12] ^= 1), FormatError::Version { .. }));
    assert_eq!(edit(&|it| it[16] = 3), FormatError::Variant(3));
    assert_eq!(edit(&|it| it[17] = 1), FormatError::Bounds("the header's padding"));
    // A count that no longer matches the bytes.
    assert!(matches!(edit(&|it| it[20] ^= 1), FormatError::Bounds(_)));

    // The first stencil's code past the code column, and its tail past its length.
    assert_eq!(
        edit(&|it| it[STENCILS..STENCILS + 4].copy_from_slice(&u32::MAX.to_le_bytes())),
        FormatError::Bounds("a stencil's code")
    );
    assert_eq!(edit(&|it| it[STENCILS + 6] = 200), FormatError::Bounds("a stencil's tail"));
    assert_eq!(edit(&|it| it[STENCILS + 7] = 200), FormatError::Bounds("a stencil's holes"));

    // The first hole: past its stencil, of no form, of no kind, of the other architecture's
    // form, a place in a number's field, an entry past the rodata.
    let hole = holes_at(&bytes);
    assert_eq!(edit(&|it| it[hole..hole + 2].copy_from_slice(&u16::MAX.to_le_bytes())), FormatError::Bounds("a hole"));
    assert_eq!(edit(&|it| it[hole + 2] = 200), FormatError::Form(200));
    assert_eq!(edit(&|it| it[hole + 3] = 200), FormatError::Kind(200));
    assert_eq!(edit(&|it| it[hole + 2] = Form::A64MovwG0 as u8), FormatError::Form(Form::A64MovwG0 as u8));
    assert_eq!(edit(&|it| it[hole + 3] = HoleKind::Next as u8), FormatError::Form(Form::Abs32S as u8));
    // Stencil 0 has a `Rodata` hole third.
    let rodata = hole + 2 * format::HOLE;
    assert_eq!(bytes[rodata + 3], HoleKind::Rodata as u8);
    assert_eq!(
        edit(&|it| it[rodata + 4..rodata + 8].copy_from_slice(&Hole::rodata_addend(1000, 0).unwrap().to_le_bytes())),
        FormatError::Bounds("a rodata hole's entry")
    );

    // Stencil 0's faults: more than there are, and one past its code.
    assert_eq!(edit(&|it| it[STENCILS + 15] = 200), FormatError::Bounds("a stencil's faults"));
    let fault = faults_at(&bytes);
    assert_eq!(u16::from_le_bytes([bytes[fault], bytes[fault + 1]]), 1);
    assert_eq!(edit(&|it| it[fault..fault + 2].copy_from_slice(&500u16.to_le_bytes())), FormatError::Bounds("a fault"));
}

#[test]
fn a_corrupt_index_is_refused() {
    let (shapes, bytes) = library(40);
    let reader = format::Reader::new(&bytes).unwrap();
    // The slot of stencil 0's key.
    let slot = (0..count(&bytes, 32))
        .map(|it| slots_at(&bytes) + it * format::SLOT)
        .find(|at| count(&bytes, *at) == shapes[0].key() as usize)
        .unwrap();
    assert_eq!(count(&bytes, slot + 4), 0);
    assert_eq!(reader.find(shapes[0].key()), Some(0));
    let edit = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut it = bytes.clone();
        f(&mut it);
        refused(&it)
    };
    assert_eq!(
        edit(&|it| it[slot + 4..slot + 8].copy_from_slice(&40u32.to_le_bytes())),
        FormatError::Bounds("a slot's stencil")
    );
    assert_eq!(
        edit(&|it| it[slot + 4..slot + 8].copy_from_slice(&1u32.to_le_bytes())),
        FormatError::Bounds("a stencil indexed twice")
    );
    assert_eq!(edit(&|it| it[slot..slot + 4].copy_from_slice(&u32::MAX.to_le_bytes())), FormatError::Bounds("an empty slot"));
    // A key that is no shape of the set.
    assert_eq!(edit(&|it| it[slot..slot + 4].copy_from_slice(&(1u32 << 30).to_le_bytes())), FormatError::Shape(1 << 30));
    // Another shape's key, which hashes to another slot.
    let other = shapes[1].key();
    assert!(matches!(
        edit(&|it| it[slot..slot + 4].copy_from_slice(&other.to_le_bytes())),
        FormatError::Index | FormatError::Bounds(_)
    ));
}

fn target(arch: blaze_codegen::Arch, cpu: Cpu) -> Target {
    Target {
        arch,
        os: Os::Linux,
        cpu,
        pic: true,
        tls: TlsModel::ElfGd,
        probe_stack: true,
        probe_log2: 12,
    }
}

#[test]
fn each_target_has_its_variant() {
    use blaze_codegen::Arch;
    assert_eq!(Variant::of(&target(Arch::X86_64, Cpu::Baseline)), Ok(Variant::X86_64));
    assert_eq!(Variant::of(&target(Arch::Aarch64, Cpu::Baseline)), Ok(Variant::Aarch64));
    assert_eq!(Variant::of(&target(Arch::Aarch64, Cpu::AppleM1)), Ok(Variant::Aarch64Lse));
    let macos = Target { os: Os::MacOs, ..target(Arch::Aarch64, Cpu::AppleM1) };
    assert_eq!(Variant::of(&macos), Ok(Variant::Aarch64Lse));
    assert!(Variant::of(&target(Arch::X86_64, Cpu::AppleM1)).is_err());
    assert!(Variant::of(&target(Arch::Wasm32, Cpu::Baseline)).is_err());
    for variant in Variant::ALL {
        assert_eq!(Variant::from_u8(variant as u8), Some(variant));
    }
    assert_eq!(Variant::from_u8(3), None);
}

#[test]
fn the_built_libraries_hold_every_shape_of_their_set() {
    for variant in Variant::ALL {
        let library = Library::get(variant).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(library.variant(), variant);
        assert_ne!(library.id(), [0; 32]);
        let set = shapes_of(variant);
        assert_eq!(library.reader().len() as usize, set.len(), "{variant:?}");
        let a64 = variant.isa() == Isa::Aarch64;
        for shape in set {
            let stencil = library.find(shape).unwrap_or_else(|| panic!("{variant:?} has no {shape}"));
            assert!(!stencil.code.is_empty(), "{shape}");
            if a64 {
                assert_eq!(stencil.code.len() % 4, 0, "{shape}");
            }
            let len = stencil.code.len();
            let tail = usize::from(stencil.tail);
            assert!(tail == 0 || tail == if a64 { 4 } else { 5 }, "{shape}: a tail of {tail}");
            let holes: Vec<Hole> = stencil.holes().collect();
            if tail > 0 {
                let next = (len - 4) as u16;
                assert!(holes.iter().any(|it| it.kind == HoleKind::Next && it.at == next), "{shape}");
            }
            for hole in holes {
                assert!(format::fits(hole.kind, hole.form), "{shape}: {hole:?}");
                assert!(usize::from(hole.at) + usize::from(hole.form.bytes()) <= len, "{shape}: {hole:?}");
                if hole.kind == HoleKind::Rodata {
                    assert!(hole.rodata().0 < library.reader().entries(), "{shape}: {hole:?}");
                }
            }
        }
    }
}

#[test]
fn the_built_libraries_are_their_own_bytes() {
    for variant in Variant::ALL {
        let reader = format::Reader::new(variant.bytes()).unwrap();
        assert_eq!(reader.id(), Library::get(variant).unwrap().id());
        assert_eq!(reader.variant(), variant);
    }
}

#[test]
fn finding_a_stencil_allocates_nothing() {
    let library = Library::get(Variant::X86_64).unwrap();
    let set = shapes(Isa::X86_64);
    let (bytes, used) = blaze_alloc_count::measure(|| {
        let mut bytes = 0usize;
        for shape in &set {
            if let Some(stencil) = library.find(*shape) {
                bytes += stencil.body().len() + stencil.holes().len();
            }
        }
        bytes
    });
    assert!(bytes > 0);
    assert_eq!(used, blaze_alloc_count::Used::default());
}
