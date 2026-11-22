//! The record form ([`crate::emit_record`]; docs/v2/plan/design/run.md, "End state", piece 5):
//! no literal address word is left in the code, the record round-trips through
//! `CodeRecord::seal` and `CodeView::read`, two builds give equal bytes, and the code runs
//! equal to the literal-word form's.
//!
//! [`holds`] is also called by `tests::emitted_for` for every function the crate's tests emit
//! in the literal-word form, so each of them is checked in the record form too.
//!
//! The lines the tests print:
//!
//! ```text
//! record form: 7 functions, 5 sites, 1 relro word, 0 literal words; each round-trips and builds to equal bytes twice
//! both forms run equal: a loop 5050, a static and data 147 then 149, 4 calls, a function's address, a memcpy of 200 bytes
//! record form refusals: no symbol, a C name for a vtable, an unaligned address; a C function keeps its name
//! tracked call: equal record bytes, caller placed at 2:5 then 5:5
//! ```

use blaze_code::record::{CodeView, OWN_CODE, PositionField, PositionPart, PositionRow, Sealed, SiteKind, Space, Symbol, Target};
use blaze_lir::{
    Flags, Op, Ty, View,
    builder::{DataPosition, DataReloc, MemAttrs, Param, Variadic},
    func::{Conv, DataId, RefKind},
};
use blaze_own::{Alloc, Vec};

use crate::{
    BULK_INLINE_MOST, EmitError, Isa, Named, Names, Outside, RecordEmitError,
    bulk_tests::bulk,
    pass::sites::UNALIGNED_ADDRESS,
    record::emit_record_up_to,
    reloc_tests::reader,
    tests::{ISA, callee, caller_of, one, sum_func},
};

/// The symbol the tests give `of`: the hash of its `Debug` text.
fn symbol(of: Outside) -> Symbol {
    Symbol::of_name(&format!("{of:?}"))
}

/// Every reference named by its place's symbol.
struct ByPlace;

impl Names for ByPlace {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        Some(Named::Symbol(symbol(of)))
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// No reference has a name.
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// Every reference is the C name `callee_c`.
struct AllC;

impl Names for AllC {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        Some(Named::C("callee_c"))
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// `func` in the record form, sealed, and its bytes of instructions.
fn sealed_of(func: View<'_>, names: &dyn Names, isa: Isa) -> Result<(Sealed<'static>, u32), RecordEmitError> {
    let (record, insts, _) = emit_record_up_to(blaze_lir::verify(func).expect("the test's function is LIR"), names, &crate::RoutinePlaces::NONE, isa, BULK_INLINE_MOST, Alloc::global())?;
    let sealed = record.seal().unwrap_or_else(|why| panic!("the emitted record does not seal: {why}\n{}", blaze_lir::print(func)));
    Ok((sealed, insts))
}

fn encode(sealed: &Sealed<'static>) -> Vec<u8, Alloc<'static>> {
    let mut out = Vec::new_in(Alloc::global());
    sealed.encode(&mut out);
    out
}

fn inst(code: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([code[at], code[at + 1], code[at + 2], code[at + 3]])
}

/// `func` built in the record form holds the piece's promises, or the test stops naming the
/// one broken; the record's (sites, relro words). `None` where the record form refuses by name
/// what the literal-word form emits (an address at an unaligned offset of its data row).
///
/// - Decoded, the instructions hold no load of a literal word (the A64 "load register
///   (literal)" class: bits 29..27 `011`, bits 25..24 `00`); every `adrp` is the first
///   instruction of a site row, emitted with a zero page and followed by the `ldr` (and, for
///   a call, `blr x16`) or `add` its kind says, with a zero offset; every `adr` reaches the
///   function's own data after the instructions.
/// - After the instructions the code holds only the function's data rows that hold no
///   address (at most their bytes and padding): no site and no word names a byte there, so
///   nothing fills an address in.
/// - `CodeView::read` of the sealed record's bytes gives back its code, relro part, targets,
///   sites and words.
/// - A second build encodes to equal bytes.
pub(crate) fn holds(func: View<'_>, isa: Isa) -> Option<(usize, usize)> {
    let text = || blaze_lir::print(func);
    let (sealed, insts) = match sealed_of(func, &ByPlace, isa) {
        Ok(it) => it,
        Err(RecordEmitError::Emit(EmitError::Data { why, .. })) if why == UNALIGNED_ADDRESS => return None,
        Err(why) => panic!("the record form refuses what the literal-word form emits: {why}\n{}", text()),
    };
    let bytes = encode(&sealed);
    let view = CodeView::read(&bytes).unwrap_or_else(|why| panic!("the record's bytes do not read: {why}\n{}", text()));
    let code = view.code();
    let insts = insts as usize;
    assert!(insts % 4 == 0 && insts <= code.len(), "{insts} bytes of instructions in {} of code", code.len());

    let (mut at, mut site) = (0, 0);
    while at < insts {
        let word = inst(code, at);
        assert_ne!(word & 0x3b00_0000, 0x1800_0000, "a load of a literal word at {at}\n{}", text());
        if word & 0x9f00_0000 == 0x1000_0000 {
            // A near `adr`, or the far own-data form's sum (pass/reach.rs).
            let to = crate::record_check::adr_reaches(code, insts, at, word);
            assert!(
                to >= insts as i64 && to <= code.len() as i64,
                "the `adr` at {at} reaches {to}, not the data after the {insts} bytes of instructions\n{}",
                text()
            );
        }
        if word & 0x9f00_0000 != 0x9000_0000 {
            at += 4;
            continue;
        }
        assert!(site < view.sites() && view.site(site).at as usize == at, "an `adrp` at {at} that no site row names\n{}", text());
        let row = view.site(site);
        let rd = word & 31;
        assert_eq!(word, 0x9000_0000 | rd, "the `adrp` at {at} is emitted with a zero page");
        let second = inst(code, at + 4);
        match row.kind {
            SiteKind::Relro => {
                assert_eq!(second, 0x9100_0000 | (rd << 5) | rd, "`add x{rd}, x{rd}, #0` after the `adrp` at {at}");
                assert!((row.addend as usize) < view.relro().len(), "the relro site at {at} addresses a byte of the relro part");
            }
            SiteKind::Load | SiteKind::Call => {
                assert_eq!(second, 0xf940_0000 | (rd << 5) | rd, "`ldr x{rd}, [x{rd}]` after the `adrp` at {at}");
                assert!((row.target as usize) < view.targets(), "the site at {at} names a target row");
            }
        }
        if row.kind == SiteKind::Call {
            assert_eq!(rd, 16, "a call site loads x16");
            assert_eq!(inst(code, at + 8), 0xd63f_0200, "`blr x16` ends the call site at {at}");
        }
        site += 1;
        at += row.kind.len() as usize;
    }
    assert_eq!(site, view.sites(), "a site row with no `adrp` in the instructions\n{}", text());

    let mut most = 0;
    for row in 0..func.datas.len() as u32 {
        if func.data_relocs_of(DataId(row)).is_empty() {
            // A row is padded to its own alignment (pass/sites.rs: `8.max(1 << align)`), 16 at
            // the least here.
            most += func.data_bytes_of(DataId(row)).len() + 15.max((1usize << func.datas[row as usize].align) - 1);
        }
    }
    assert!(
        code.len() - insts <= most,
        "{} bytes follow the instructions, more than the data rows holding no address ({most} with padding)\n{}",
        code.len() - insts,
        text()
    );

    let record = sealed.record();
    assert_eq!(code, &record.code[..], "the code round-trips");
    assert_eq!(view.relro(), &record.relro[..], "the relro part round-trips");
    assert_eq!(view.targets(), record.targets.len());
    assert!((0..view.targets()).all(|i| view.target(i) == record.targets[i]), "the targets round-trip");
    assert!(record.targets.windows(2).all(|pair| pair[0] < pair[1]), "the targets are sorted by (space, symbol), each once");
    assert_eq!(view.sites(), record.sites.len());
    assert!((0..view.sites()).all(|i| view.site(i) == record.sites[i]), "the sites round-trip");
    assert_eq!(view.words(), record.words.len());
    assert!((0..view.words()).all(|i| view.word(i) == record.words[i]), "the words round-trip");
    assert_eq!(view.positions(), record.positions.len());
    assert!((0..view.positions()).all(|i| view.position(i) == record.positions[i]), "the source position fields round-trip");

    let (again, _) = sealed_of(func, &ByPlace, isa).expect("the second build emits as the first did");
    assert_eq!(&bytes[..], &encode(&again)[..], "two builds give equal bytes\n{}", text());
    Some((view.sites(), view.words()))
}

/// At the emitter boundary the hidden last argument of a tracked call is a pointer to a
/// `Location` (blaze-build intrinsics.rs `location`). Plant the current coordinates in
/// the input bytes: copying those numbers would make the two builds below differ.
#[test]
fn moving_the_untracked_caller_preserves_the_record() {
    use blaze_code::{
        image::ImageError,
        place::{At, Words, place_positions},
    };

    const ITEM: u128 = 0x1234_5678_9abc_def0_ffe0_ddc0_bba0_9988;
    const CALLER: &str = "fn caller() -> u32 {\n    tracked()\n}\n";
    let coordinates = |source: &str, at: usize| {
        let before = &source[..at];
        let line = 1 + before.bytes().filter(|&b| b == b'\n').count() as u32;
        let col = 1 + before.rsplit('\n').next().unwrap().chars().count() as u32;
        (line, col)
    };
    let build = |source: &str| {
        let start = source.find("fn caller").unwrap();
        let call = source.find("tracked()").unwrap();
        let site = (call - start) as u32;
        let (line, col) = coordinates(source, call);
        let func = one(&[], Ty::I32, |b, _| {
            let filename = b"src/main.rs\0";
            let path = b.data(filename, 0, &[]);
            let mut bytes = [0u8; 24];
            bytes[8..16].copy_from_slice(&((filename.len() - 1) as u64).to_le_bytes());
            bytes[16..20].copy_from_slice(&line.to_le_bytes());
            bytes[20..24].copy_from_slice(&col.to_le_bytes());
            let fields = [
                DataPosition { offset: 16, item: ITEM, site, field: blaze_lir::PositionField::Line },
                DataPosition { offset: 20, item: ITEM, site, field: blaze_lir::PositionField::Col },
            ];
            let location = b.data_with_positions(&bytes, 3, &[DataReloc { offset: 0, target: path, addend: 0 }], &fields);
            let ptr = b.data_addr(location);
            let tracked = b.reference(RefKind::Func, 0);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
            b.call(Flags::NONE, tracked, sig, &[ptr])
        });
        let (sealed, _) = sealed_of(func.view(), &ByPlace, ISA).expect("the untracked caller emits a record");
        encode(&sealed)
    };
    let moved = format!("\n\n\n{CALLER}");
    let bytes = build(CALLER);
    assert_eq!(&bytes[..], &build(&moved)[..], "adding three lines above the caller preserves every stored byte");
    let view = CodeView::read(&bytes).expect("the stored caller reads");
    let site = CALLER.find("tracked()").unwrap() as u32;
    assert_eq!(view.positions(), 2);
    assert_eq!(view.position(0), PositionRow { part: PositionPart::Relro, at: 16, item: ITEM, site, field: PositionField::Line });
    assert_eq!(view.position(1), PositionRow { part: PositionPart::Relro, at: 20, item: ITEM, site, field: PositionField::Col });
    assert_eq!(&view.relro()[16..24], &[0; 8], "the record excludes the planted coordinates");

    struct Tracked;
    impl Words for Tracked {
        fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
            assert!(call);
            assert_eq!(target, Target { space: Space::Func, symbol: symbol(Outside::Func(0)) });
            Ok(0x3000)
        }
        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            panic!("the filename belongs to the caller's code, unexpected import {target:?}")
        }
    }
    let placed = |source: &str| {
        let mut code = vec![0; view.code().len()];
        let mut relro = vec![0; view.relro().len()];
        let at = At { code: 0x1000, relro: 0x2000 };
        place_positions(&view, Symbol::of_name("caller"), at, &mut Tracked, &mut code, &mut relro, |_, row| {
            assert_eq!(row.item, ITEM);
            let call = source.find("fn caller").unwrap() + row.site as usize;
            Ok(row.field.value(coordinates(source, call)))
        }).expect("the caller places against the current text");
        (inst(&relro, 16), inst(&relro, 20))
    };
    assert_eq!(placed(CALLER), (2, 5));
    assert_eq!(placed(&moved), (5, 5));
    println!("tracked call: equal record bytes, caller placed at 2:5 then 5:5");
}

/// The piece's test (run.md, "Pieces", 5): prints
/// `record form: 7 functions, 5 sites, 1 relro word, 0 literal words; each round-trips and builds to equal bytes twice`.
#[test]
fn no_literal_word_is_left_in_a_record() {
    let address = one(&[], Ty::PTR, |b, _| {
        let reference = b.reference(RefKind::Func, 0);
        b.func_addr(reference)
    });
    let own = one(&[], Ty::I8, |b, _| {
        let row = b.data(&[7, 8, 9], 0, &[]);
        let at = b.data_addr(row);
        b.load(Ty::I8, Flags::NONE, at, 1, MemAttrs::default())
    });
    // (what, the function, its sites, its relro words)
    let samples = [
        ("a loop", sum_func(), 0, 0),
        ("a callee", callee(), 0, 0),
        ("a caller", caller_of(Ty::I32).0, 1, 0),
        ("a function's address", address, 1, 0),
        ("a static and data holding an address", reader(), 2, 1),
        ("a memcpy of 200 bytes", bulk(Op::Memcpy, Some(200)), 1, 0),
        ("own data holding no address", own, 0, 0),
    ];
    let (mut sites, mut words) = (0, 0);
    for (what, func, want_sites, want_words) in samples.iter() {
        let got = holds(func.view(), ISA).unwrap_or_else(|| panic!("{what}: refused"));
        assert_eq!(got, (*want_sites, *want_words), "{what}: (sites, relro words)");
        sites += got.0;
        words += got.1;
    }

    // The data row holding an address is in the relro part; the text it names stays in the
    // code, named by the record's own code byte, never by an address.
    let (sealed, insts) = sealed_of(reader().view(), &ByPlace, ISA).expect("emits");
    let record = sealed.record();
    assert_eq!(record.relro.len(), 8, "the table row is the relro part");
    assert_eq!(&record.relro[..], &[0u8; 8], "its word is zero until the image binds it");
    assert_eq!(record.words[0].target, OWN_CODE);
    let text_at = record.words[0].addend as usize - 1;
    assert!(text_at >= insts as usize, "the text row follows the instructions");
    assert_eq!(&record.code[text_at..text_at + 3], b"hi!", "the word names the text row plus 1");
    assert_eq!(record.targets[..], [Target { space: Space::Static, symbol: symbol(Outside::Static(0)) }]);
    let kinds = [record.sites[0].kind, record.sites[1].kind];
    assert_eq!(kinds, [SiteKind::Load, SiteKind::Relro], "the static's load, then the table's address");

    // A routine is named by its C name, kept for `dlsym`.
    let (sealed, _) = sealed_of(bulk(Op::Memcpy, Some(200)).view(), &ByPlace, ISA).expect("emits");
    let bytes = encode(&sealed);
    let view = CodeView::read(&bytes).expect("reads");
    assert_eq!(view.target(0).space, Space::Outside);
    assert_eq!(view.name(view.target(0).symbol), Some("memcpy"));
    assert_eq!(view.site(0).kind, SiteKind::Call);

    println!(
        "record form: {} functions, {sites} sites, {words} relro word, 0 literal words; each round-trips and builds to equal bytes twice",
        samples.len()
    );
}

/// What the record form refuses is refused by name, never emitted as a literal word: prints
/// `record form refusals: no symbol, a C name for a vtable, an unaligned address; a C function keeps its name`.
#[test]
fn the_record_form_refuses_by_name() {
    let caller = caller_of(Ty::I32).0;
    let refused = |names: &dyn Names, func: View<'_>| sealed_of(func, names, ISA).map(|_| ()).expect_err("refused");
    assert_eq!(refused(&Nameless, caller.view()), RecordEmitError::NoSymbol { of: Outside::Func(0) });

    let vtable = one(&[], Ty::PTR, |b, _| {
        let reference = b.reference(RefKind::Vtable, 0);
        b.data_addr(reference)
    });
    assert_eq!(refused(&AllC, vtable.view()), RecordEmitError::CName { of: Outside::Vtable(0) });

    // An address 4 bytes into its row: no aligned word of the relro part can hold it.
    let packed = one(&[], Ty::PTR, |b, _| {
        let reference = b.reference(RefKind::Func, 0);
        let row = b.data(&[0; 12], 2, &[DataReloc { offset: 4, target: reference, addend: 0 }]);
        b.data_addr(row)
    });
    let why = RecordEmitError::Emit(EmitError::Data { row: 0, why: UNALIGNED_ADDRESS });
    assert_eq!(refused(&ByPlace, packed.view()), why);
    assert_eq!(holds(packed.view(), ISA), None, "the refusal `holds` passes over is this one");

    // A function with a C name is an outside target whose name the record keeps.
    let (sealed, _) = sealed_of(caller.view(), &AllC, ISA).expect("emits");
    let bytes = encode(&sealed);
    let view = CodeView::read(&bytes).expect("reads");
    assert_eq!(view.targets(), 1);
    assert_eq!(view.target(0), Target { space: Space::Outside, symbol: Symbol::of_name("callee_c") });
    assert_eq!(view.name(view.target(0).symbol), Some("callee_c"));
    println!("record form refusals: no symbol, a C name for a vtable, an unaligned address; a C function keeps its name");
}

#[cfg(target_arch = "aarch64")]
mod runs {
    use core::sync::atomic::{AtomicU64, Ordering};

    use blaze_code::record::{CodeView, OWN_CODE, OWN_RELRO, SiteKind, Space, Target};
    use blaze_inproc::exec::Executable;
    use blaze_lir::{Op, Ty, View, func::RefKind};

    use super::{ByPlace, encode, inst, sealed_of, symbol};
    use crate::{
        Outside, RelocTo, Routine, Via,
        bulk_tests::bulk,
        reloc_tests::reader,
        tests::{ISA, callee, caller_of, emitted, one, run, sum_func},
    };

    unsafe extern "C" {
        fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
    }

    /// The tests' binder: `func` in the record form, mapped with an import word per target of
    /// the record (target `i`'s at data byte `8 * i`) and the relro part after them (16-aligned),
    /// each import word and relro word filled from `home` (a target and, for a C name, its text:
    /// the same map the literal-word form's words are filled from), and each site's `adrp` and
    /// `ldr`/`add` completed to reach its word, the `adrp`'s 4 GiB reach checked.
    fn bound(func: View<'_>, home: &dyn Fn(Target, Option<&str>) -> u64) -> Executable {
        let (sealed, _) = sealed_of(func, &ByPlace, ISA).expect("the record form emits");
        let bytes = encode(&sealed);
        let view = CodeView::read(&bytes).expect("the record reads");
        assert_eq!(view.positions(), 0, "this test binder has no source-position resolver");
        let targets = view.targets();
        let relro_at = (8 * targets).next_multiple_of(16);
        Executable::written(view.code().len(), relro_at + view.relro().len(), |code, data| {
            let (code_at, data_at) = (code.as_ptr() as u64, data.as_ptr() as u64);
            assert!(data.is_empty() || data_at % 16 == 0, "the data is 16-aligned");
            code.copy_from_slice(view.code());
            let value = |i: usize| {
                let target = view.target(i);
                home(target, view.name(target.symbol))
            };
            for i in 0..targets {
                data[8 * i..8 * i + 8].copy_from_slice(&value(i).to_le_bytes());
            }
            data[relro_at..].copy_from_slice(view.relro());
            for i in 0..view.words() {
                let word = view.word(i);
                let to = match word.target {
                    OWN_CODE => code_at,
                    OWN_RELRO => data_at + relro_at as u64,
                    target => value(target as usize),
                };
                let at = relro_at + word.at as usize;
                data[at..at + 8].copy_from_slice(&to.wrapping_add_signed(word.addend).to_le_bytes());
            }
            for i in 0..view.sites() {
                let site = view.site(i);
                let at = site.at as usize;
                let pc = code_at + u64::from(site.at);
                let to = match site.kind {
                    SiteKind::Relro => data_at + relro_at as u64 + u64::from(site.addend),
                    SiteKind::Load | SiteKind::Call => data_at + 8 * u64::from(site.target),
                };
                let pages = (to >> 12) as i64 - (pc >> 12) as i64;
                assert!((-(1i64 << 20)..1i64 << 20).contains(&pages), "the site at {at} is past `adrp`'s 4 GiB");
                let imm = pages as u32;
                let first = inst(code, at) | ((imm & 3) << 29) | (((imm >> 2) & 0x7_ffff) << 5);
                let low = (to & 0xfff) as u32;
                let second = match site.kind {
                    SiteKind::Relro => inst(code, at + 4) | (low << 10),
                    SiteKind::Load | SiteKind::Call => {
                        assert_eq!(low % 8, 0, "an import word is 8-aligned");
                        inst(code, at + 4) | ((low >> 3) << 10)
                    }
                };
                code[at..at + 4].copy_from_slice(&first.to_le_bytes());
                code[at + 4..at + 8].copy_from_slice(&second.to_le_bytes());
            }
        })
        .expect("the code maps")
    }

    fn of(space: Space, of: Outside) -> Target {
        Target { space, symbol: symbol(of) }
    }

    /// Both instantiations run equal (rule 78): a sample of the crate's execution tests through
    /// the literal-word form, its words filled as placement fills them, and through the record
    /// form, its import table bound from the same addresses. Prints
    /// `both forms run equal: a loop 5050, a static and data 147 then 149, 4 calls, a function's address, a memcpy of 200 bytes`.
    #[test]
    fn both_forms_run_equal() {
        // A loop: no outside reference in either form.
        let sum = sum_func();
        let old = run::place(&emitted(sum.view(), &[]).expect("emits"), |_| unreachable!());
        let new = bound(sum.view(), &|target, _| panic!("an unexpected target {target:?}"));
        let sums = (run::call(&old, [100, 0, 0, 0]), run::call(&new, [100, 0, 0, 0]));
        assert_eq!(sums, (5050, 5050));

        // A static read and written, and data holding the address of other data.
        let reads = reader();
        let (old_cell, new_cell) = (AtomicU64::new(40), AtomicU64::new(40));
        let old = run::place_all(&emitted(reads.view(), &[]).expect("emits"), |_| unreachable!(), |to| match to {
            RelocTo::Static(0) => old_cell.as_ptr() as u64,
            other => panic!("an unexpected relocation {other:?}"),
        });
        let new = bound(reads.view(), &|target, _| {
            assert_eq!(target, of(Space::Static, Outside::Static(0)));
            new_cell.as_ptr() as u64
        });
        for want in [147, 149] {
            assert_eq!((run::call(&old, [0; 4]), run::call(&new, [0; 4])), (want, want), "`i` plus the static after += 2");
        }
        assert_eq!((old_cell.load(Ordering::Relaxed), new_cell.load(Ordering::Relaxed)), (44, 44), "both stored to their static");

        // A call: the literal word holds the callee's address; the import word does.
        let callee = callee();
        let old_callee = run::place(&emitted(callee.view(), &[]).expect("emits"), |_| unreachable!());
        let new_callee = bound(callee.view(), &|target, _| panic!("an unexpected target {target:?}"));
        let caller = caller_of(Ty::I32).0;
        let old = run::place(&emitted(caller.view(), &[Via::Address]).expect("emits"), |_| old_callee.code_address());
        let new = bound(caller.view(), &|target, _| {
            assert_eq!(target, of(Space::Func, Outside::Func(0)));
            new_callee.code_address()
        });
        let cases = [(5u32, 2u8), (5, 0xfe), (0xffff_ffff, 0x80), (1000, 0x7f)];
        for (x, y) in cases {
            let args = [u64::from(x) | 0xabcd << 40, u64::from(y) | 0xff00, 0, 0];
            let want = x.wrapping_mul(3).wrapping_add(y as i8 as i32 as u32).wrapping_add(1);
            assert_eq!((run::call(&old, args) as u32, run::call(&new, args) as u32), (want, want), "{x:#x}, {y:#x}");
        }

        // A function's address as a value.
        let address = one(&[], Ty::PTR, |b, _| {
            let reference = b.reference(RefKind::Func, 0);
            b.func_addr(reference)
        });
        let old = run::place(&emitted(address.view(), &[Via::Address]).expect("emits"), |_| 0x1234_5678);
        let new = bound(address.view(), &|_, _| 0x1234_5678);
        assert_eq!((run::call(&old, [0; 4]), run::call(&new, [0; 4])), (0x1234_5678, 0x1234_5678));

        // A system routine: bound by its C name.
        let copy = bulk(Op::Memcpy, Some(200));
        let old = run::place_all(&emitted(copy.view(), &[]).expect("emits"), |_| unreachable!(), |to| match to {
            RelocTo::Routine(Routine::Memcpy) => memcpy as usize as u64,
            other => panic!("an unexpected relocation {other:?}"),
        });
        let new = bound(copy.view(), &|target, name| {
            assert_eq!((target.space, name), (Space::Outside, Some("memcpy")));
            memcpy as usize as u64
        });
        let from: [u8; 256] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        let (mut by_old, mut by_new) = ([0u8; 256], [0u8; 256]);
        run::call(&old, [by_old.as_mut_ptr() as u64, from.as_ptr() as u64, 0, 0]);
        run::call(&new, [by_new.as_mut_ptr() as u64, from.as_ptr() as u64, 0, 0]);
        assert_eq!(by_old, by_new);
        assert_eq!(by_new[..200], from[..200]);
        assert!(by_new[200..].iter().all(|&byte| byte == 0), "exactly 200 bytes are written");

        println!("both forms run equal: a loop 5050, a static and data 147 then 149, 4 calls, a function's address, a memcpy of 200 bytes");
    }
}
