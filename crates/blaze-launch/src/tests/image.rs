//! An image of placed code: rebased to a process's addresses it is what placing the records
//! there gives, byte for byte; its bytes read back as it was; and what is no image, or one of
//! another layout, is refused by name.

use hegel::TestCase;
use hegel::generators as gs;

use crate::image::{Base, Bases, Image, Input, LONGEST, StaticAt, VERSION, Why, lay};
use crate::record::{Datum, Fixup, Kind, Place, Record, To, Unwind, What};
use crate::wire::{Arch, Bind, List, Os, RecordId, StaticDef};
use crate::{CodeId, Slot, Tier};

/// A test's lists live as long as the test's process.
fn leak<T>(items: Vec<T>) -> &'static [T] {
    Box::leak(items.into_boxed_slice())
}

fn fixup(at: u32, kind: Kind, to: To, addend: i64) -> Fixup {
    Fixup { at, kind, to, addend }
}

/// A program of two statics and two functions for x86-64, every kind of absolute word in
/// it: a static that holds a function's address and an outside symbol's; a function that
/// reaches a static, an outside symbol and a datum through its pool, a function through its
/// slot's word and by its thunk, and holds its own address; a datum that holds another's
/// address and a function's; an unwind entry with a personality routine and a data area.
/// `addend` is added where an address is held.
struct Program {
    records: Vec<(RecordId, Record<'static>)>,
    statics: &'static [StaticDef<'static>],
    binds: &'static [Bind<'static>],
}

fn id(seed: u8) -> RecordId {
    RecordId([seed; 32])
}

fn program(addend: i64) -> Program {
    let holds = Record {
        arch: Arch::X86_64,
        what: What::Static { mutable: false, thread_local: false },
        align: 8,
        size: 16,
        bytes: leak((0..16).collect()),
        fixups: List::of(leak(vec![
            fixup(0, Kind::Abs64, To::Place(0), 0),
            fixup(8, Kind::Abs64, To::Place(1), addend),
        ])),
        places: List::of(leak(vec![Place::Function, Place::Extern("write")])),
        data: List::of(&[]),
        unwind: None,
        name: "app::HOLDS",
    };
    let zeroes = Record {
        arch: Arch::X86_64,
        what: What::Static { mutable: true, thread_local: false },
        align: 16,
        size: 32,
        bytes: &[],
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name: "app::ZEROES",
    };
    let first = Datum { id: [7; 32], align: 8, bytes: leak(vec![0x11; 16]), fixups: List::of(&[]), writable: false };
    let second = Datum {
        id: [8; 32],
        align: 8,
        bytes: leak(vec![0x22; 16]),
        fixups: List::of(leak(vec![
            fixup(0, Kind::Abs64, To::Data(0), addend),
            fixup(8, Kind::Abs64, To::Place(2), 0),
        ])),
        writable: false,
    };
    let reaches = Record {
        arch: Arch::X86_64,
        what: What::Code,
        align: 16,
        size: 48,
        bytes: leak(vec![0x90; 48]),
        fixups: List::of(leak(vec![
            fixup(4, Kind::GotPcRel32, To::Place(0), 0),
            fixup(12, Kind::GotPcRel32, To::Place(1), 0),
            fixup(20, Kind::GotPcRel32, To::Place(2), 0),
            fixup(28, Kind::PcRel32, To::Place(2), -4),
            fixup(32, Kind::Abs64, To::Own, addend),
            fixup(40, Kind::GotPcRel32, To::Data(1), 0),
        ])),
        places: List::of(leak(vec![Place::Static, Place::Extern("write"), Place::Function])),
        data: List::of(leak(vec![first, second])),
        unwind: Some(Unwind {
            code_align: 1,
            data_align: -8,
            return_address: 16,
            cie: &[0x0c, 0x07, 0x08],
            fde: &[0x41, 0x0e, 0x10],
            personality: Some(2),
            lsda: Some(0),
        }),
        name: "app::reaches",
    };
    let small = Record {
        arch: Arch::X86_64,
        what: What::Code,
        align: 16,
        size: 16,
        bytes: leak(vec![0xcc; 16]),
        fixups: List::of(leak(vec![fixup(0, Kind::PcRel32, To::Own, 8)])),
        places: List::of(&[]),
        // The datum the first function placed: one copy a process.
        data: List::of(leak(vec![first])),
        unwind: None,
        name: "app::small",
    };
    let statics = leak(vec![
        StaticDef { number: 0, record: id(1), refs: List::of(leak(vec![2])) },
        StaticDef { number: 1, record: id(2), refs: List::of(&[]) },
    ]);
    let bind = |slot: u32, record: u8, len: u64, refs: Vec<u32>| Bind {
        slot: Slot(slot),
        record: id(record),
        code: CodeId([record; 32]),
        tier: Tier::Zero,
        len,
        align: 16,
        refs: List::of(leak(refs)),
    };
    // The first function's pool: a cell for the static, the outside symbol and the datum.
    let binds = leak(vec![bind(0, 3, 48 + 24, vec![0, 1]), bind(1, 4, 16, vec![])]);
    Program { records: vec![(id(1), holds), (id(2), zeroes), (id(3), reaches), (id(4), small)], statics, binds }
}

impl Program {
    fn input(&self) -> Input<'static> {
        Input {
            arch: Arch::X86_64,
            os: Os::Linux,
            page: 4096,
            slots: 3,
            statics: self.statics,
            binds: self.binds,
            entry: Slot(0),
            inits: &[0],
        }
    }

    fn lay(&self, bases: &Bases<'_>) -> Image {
        let records = |id: &RecordId| self.records.iter().find(|it| it.0 == *id).map(|it| it.1);
        lay(&self.input(), &records, bases).unwrap_or_else(|why| panic!("{why}"))
    }
}

/// Where a drawn process has its mappings and its one outside symbol.
fn drawn(tc: &TestCase) -> (u64, u64, u64) {
    let page = |tc: &TestCase| tc.draw(gs::integers::<u64>().min_value(1).max_value(1 << 30)) * 4096;
    (page(tc), page(tc), tc.draw(gs::integers::<u64>().max_value(1 << 46)))
}

/// The image kept (laid against no process, written and read back), rebased to `bases`,
/// and with `plant` its fixes against outside symbols left out: its code and its data.
fn kept(program: &Program, bases: &Bases<'_>, plant: bool) -> (Vec<u8>, Vec<u8>) {
    let image = program.lay(&Bases::NONE);
    let mut bytes = Vec::new();
    image.encode(&mut bytes);
    let mut read = Image::decode(&bytes).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(read, image, "an image reads back as it was written");
    if plant {
        read.fixes.retain(|it| !matches!(it.base, Base::Outside(_)));
    }
    read.rebase(bases).unwrap_or_else(|why| panic!("{why}"));
    (read.area, read.data)
}

/// The refinement: an image rebased is the placement. The image the daemon keeps, made
/// against the addresses of no process, written, read back and rebased to a process's
/// mappings and symbols, holds in its code and its data exactly the bytes that placing the
/// same records at those addresses gives.
#[hegel::test]
fn an_image_rebased_is_the_placement(tc: TestCase) {
    let (code, data, write) = drawn(&tc);
    let addend = tc.draw(gs::integers::<i64>().min_value(-64).max_value(64));
    let program = program(addend);
    let outside = [write];
    let bases = Bases { code, data, outside: &outside, tls_thunk: 0, tls: &[] };
    let placed = program.lay(&bases);
    let (area, bytes) = kept(&program, &bases, false);
    assert_eq!(area, placed.area, "the code, with its mappings at {code:#x} and {data:#x}");
    assert_eq!(bytes, placed.data, "the data, with its mappings at {code:#x} and {data:#x}");
}

/// A control of [`an_image_rebased_is_the_placement`]: an image whose fixes against outside
/// symbols are left out is not the placement.
#[test]
fn the_oracle_catches_an_image_that_leaves_a_fix_out() {
    let program = program(4);
    let outside = [0x7000_1000];
    let bases = Bases { code: 0x5000_0000, data: 0x9000_0000, outside: &outside, tls_thunk: 0, tls: &[] };
    let placed = program.lay(&bases);
    assert_eq!(kept(&program, &bases, false), (placed.area.clone(), placed.data.clone())); // clone: a test's copy
    assert_ne!(kept(&program, &bases, true), (placed.area, placed.data));
}

#[test]
fn an_image_lists_what_a_launcher_keeps_of_what_it_placed() {
    let image = program(0).lay(&Bases::NONE);
    assert_eq!(image.outside, [("write".to_owned(), false)]);
    assert_eq!(image.statics.len(), 2);
    assert!(matches!(image.statics[0], Some(StaticAt::Data(0))));
    // One copy of the datum both functions name, and the second datum.
    assert_eq!(image.datums.iter().map(|it| it.0[0]).collect::<Vec<_>>(), [7, 8]);
    let names: Vec<&str> = image.bodies.iter().map(|it| &it.name[..]).collect();
    assert_eq!(names, ["app::reaches", "app::small"]);
    assert_eq!((image.bodies[0].at, image.bodies[0].len), (0, 72));
    assert_eq!((image.bodies[1].at, image.bodies[1].len), (80, 16));
    assert!(image.bodies[0].frame.is_some() && image.bodies[1].frame.is_none());
    assert_eq!(image.area.len(), 96);
}

/// A thread-local on AArch64 Mach-O: the pool's descriptor holds the launcher's thunk and
/// what names the static to it, which an image moves as it moves an address.
#[test]
fn a_thread_local_is_rebased_through_its_descriptor() {
    let local = Record {
        arch: Arch::Aarch64,
        what: What::Static { mutable: true, thread_local: true },
        align: 8,
        size: 8,
        bytes: leak(vec![9; 8]),
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name: "app::LOCAL",
    };
    // `adrp x0, cell; ldr x0, [x0, cell's offset in its page]`.
    let reads = Record {
        arch: Arch::Aarch64,
        what: What::Code,
        align: 16,
        size: 8,
        bytes: leak([0x9000_0000u32, 0xf940_0000].iter().flat_map(|it| it.to_le_bytes()).collect()),
        fixups: List::of(leak(vec![
            fixup(0, Kind::TlvPage21, To::Place(0), 0),
            fixup(4, Kind::TlvPageOffset12, To::Place(0), 0),
        ])),
        places: List::of(leak(vec![Place::Static])),
        data: List::of(&[]),
        unwind: None,
        name: "app::reads",
    };
    let statics = leak(vec![StaticDef { number: 0, record: id(1), refs: List::of(&[]) }]);
    let binds = leak(vec![Bind {
        slot: Slot(0),
        record: id(2),
        code: CodeId([2; 32]),
        tier: Tier::Zero,
        len: 8 + 32,
        align: 16,
        refs: List::of(leak(vec![0])),
    }]);
    let input = Input {
        arch: Arch::Aarch64,
        os: Os::Macos,
        page: 16384,
        slots: 1,
        statics,
        binds,
        entry: Slot(0),
        inits: &[],
    };
    let records = |of: &RecordId| if *of == id(1) { Some(local) } else { (*of == id(2)).then_some(reads) };
    let tls = [0xabc0_0000];
    let bases = Bases { code: 0x1_0000_0000, data: 0x2_0000_0000, outside: &[], tls_thunk: 0x7777_0000, tls: &tls };
    let placed = lay(&input, &records, &bases).unwrap_or_else(|why| panic!("{why}"));
    let mut image = lay(&input, &records, &Bases::NONE).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(image.tls.len(), 1);
    assert_eq!((image.tls[0].size, image.tls[0].init_len), (8, 8));
    image.rebase(&bases).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(image.area, placed.area);
    assert_eq!(image.data, placed.data);
    // The descriptor: the thunk, then what names the static.
    assert_eq!(image.area[16..24], 0x7777_0000u64.to_le_bytes());
    assert_eq!(image.area[24..32], 0xabc0_0000u64.to_le_bytes());
}

#[test]
fn what_is_no_image_of_this_layout_is_refused_by_name() {
    let image = program(0).lay(&Bases::NONE);
    let mut bytes = Vec::new();
    image.encode(&mut bytes);
    let mut other = bytes.clone(); // clone: a test's copy
    other[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
    let why = Image::decode(&other).unwrap_err().to_string();
    assert!(why.contains(&format!("layout {}", VERSION + 1)), "{why}");
    let why = Image::decode(&bytes[..bytes.len() - 3]).unwrap_err().to_string();
    assert!(why.contains("the image ends inside"), "{why}");
    let mut longer = bytes.clone(); // clone: as above
    longer.push(0);
    let why = Image::decode(&longer).unwrap_err().to_string();
    assert!(why.contains("1 bytes after the image"), "{why}");
    let refused = Image::decode(b"not an image at all").unwrap_err();
    assert_eq!(refused.why, Why::Layout);
    assert!(refused.what.contains("no image of placed code"), "{refused}");
}

/// A function that reaches a datum relative to itself, with no word between, cannot be
/// moved apart from the data: there is no image of it, and the refusal names it.
#[test]
fn code_relative_to_what_is_outside_the_mapping_has_no_image() {
    let datum = Datum { id: [5; 32], align: 8, bytes: leak(vec![1; 8]), fixups: List::of(&[]), writable: false };
    let record = Record {
        arch: Arch::X86_64,
        what: What::Code,
        align: 16,
        size: 16,
        bytes: leak(vec![0x90; 16]),
        fixups: List::of(leak(vec![fixup(4, Kind::PcRel32, To::Data(0), 0)])),
        places: List::of(&[]),
        data: List::of(leak(vec![datum])),
        unwind: None,
        name: "app::near",
    };
    let binds = leak(vec![Bind {
        slot: Slot(0),
        record: id(1),
        code: CodeId([1; 32]),
        tier: Tier::Zero,
        len: 16,
        align: 16,
        refs: List::of(&[]),
    }]);
    let input = Input {
        arch: Arch::X86_64,
        os: Os::Linux,
        page: 4096,
        slots: 1,
        statics: &[],
        binds,
        entry: Slot(0),
        inits: &[],
    };
    let records = |of: &RecordId| (*of == id(1)).then_some(record);
    let refused = lay(&input, &records, &Bases::NONE).unwrap_err();
    assert_eq!(refused.why, Why::Relative);
    assert!(refused.what.contains("app::near"), "{refused}");
}

/// A program of one static `record` and no function, for a launcher of `arch`.
fn of_one_static(arch: Arch, record: Record<'static>) -> Result<Image, crate::image::ImageError> {
    let statics = leak(vec![StaticDef { number: 0, record: id(1), refs: List::of(&[]) }]);
    let input =
        Input { arch, os: Os::Linux, page: 4096, slots: 1, statics, binds: &[], entry: Slot(0), inits: &[] };
    let records = |of: &RecordId| (*of == id(1)).then_some(record);
    lay(&input, &records, &Bases::NONE)
}

fn zeroes(arch: Arch, align: u32, size: u64, thread_local: bool, name: &'static str) -> Record<'static> {
    Record {
        arch,
        what: What::Static { mutable: true, thread_local },
        align,
        size,
        bytes: &[],
        fixups: List::of(&[]),
        places: List::of(&[]),
        data: List::of(&[]),
        unwind: None,
        name,
    }
}

/// Each case in which a program has no image is refused by its name, naming what of the
/// program: a static aligned past a page, a thread-local where the launcher has none, more
/// bytes than a message carries. (Code relative to data is the test above; bytes that are
/// no image, [`what_is_no_image_of_this_layout_is_refused_by_name`].)
#[test]
fn each_case_of_no_image_is_refused_by_its_name() {
    let refused = of_one_static(Arch::X86_64, zeroes(Arch::X86_64, 8192, 8, false, "app::WIDE")).unwrap_err();
    assert_eq!((refused.why, refused.why.name()), (Why::Alignment, "aligned past a page"));
    assert!(refused.what.contains("app::WIDE"), "{refused}");
    let refused = of_one_static(Arch::X86_64, zeroes(Arch::X86_64, 8, 8, true, "app::LOCAL")).unwrap_err();
    assert_eq!(refused.why, Why::ThreadLocal);
    assert!(refused.what.contains("app::LOCAL"), "{refused}");
    let refused =
        of_one_static(Arch::X86_64, zeroes(Arch::X86_64, 8, LONGEST as u64 + 1, false, "app::LONG")).unwrap_err();
    assert_eq!(refused.why, Why::Long);
    // And a static a page wide, of a page: an image.
    assert!(of_one_static(Arch::X86_64, zeroes(Arch::X86_64, 4096, 4096, false, "app::PAGE")).is_ok());
    // A record that was not given is no case of the program's: the input's fault.
    let input = Input {
        arch: Arch::X86_64,
        os: Os::Linux,
        page: 4096,
        slots: 1,
        statics: leak(vec![StaticDef { number: 0, record: id(9), refs: List::of(&[]) }]),
        binds: &[],
        entry: Slot(0),
        inits: &[],
    };
    let none = |_: &RecordId| -> Option<Record<'static>> { None };
    assert_eq!(lay(&input, &none, &Bases::NONE).unwrap_err().why, Why::Input);
}
