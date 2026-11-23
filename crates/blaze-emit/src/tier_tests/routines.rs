//! The routine table as the seam's input (D197 root 6, piece (a); `blaze_tier::Names::routines`).
//! `fn(a: f128, b: f128) -> f128 { a + b }`, whose add becomes a call of compiler_builtins'
//! `__addtf3` (INTRINSICS.md 3.5), is emitted through `Tier::emit` with names that bind every
//! routine at reference place `r as u32` (as front2/builtins_shim.rs `Bound::bind` appends them)
//! and name each by its C name:
//!
//! - EQUALITY AT THE SEAM: the Reference tier's record through the seam is byte for byte the
//!   record of `emit_record` called directly with the same table, carried by `from_v1`; the
//!   direct call says it calls `__addtf3` and nothing else, and the seam's record names it.
//! - RUN: the Reference tier's record, placed by the one new placer with `__addtf3` bound to this
//!   binary's own, returns 1.0 + 2.0 = 3.0 (bits worked out by hand: quad_run_tests.rs's table).
//!   The Fast tier gets the same table (compose/fast.rs legalizes a function holding an `f128`):
//!   where it composes the function, it must return 3.0 too; where it refuses, the refusal is
//!   printed (Fast's worklist) and must not be the unbound-routine one.
//! - THE CONTROL (rule 23): the same names with no routine bound ([`RoutinePlaces::NONE`]): both
//!   tiers refuse by the one unbound-routine reason, so the table the seam hands is what each
//!   tier's call of `__addtf3` comes from.

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_inproc::exec::Executable;
use blaze_lir::legalize::SoftFloat;
use blaze_lir::{Flags, Op, Ty};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::record::{encode, from_v1, place, read, seal};
use blaze_tier::{Emitted, Machine, Named, Names, Outside, RoutinePlaces, Table, TierError, TierName};

use super::{POLL_PAGE_ROOM, TierFn, carve_poll_page, fast, reference, table};
use crate::compose::emit::POLL_PAGE;
use crate::tests::one;
use crate::{Isa, Os, emit_record};

unsafe extern "C" {
    fn __addtf3(a: f128, b: f128) -> f128;
}

/// 1.0, 2.0 and 3.0 as binary128 bits (quad_run_tests.rs's table: exponent bias 16383).
const ONE: u128 = 0x3fff << 112;
const TWO: u128 = 0x4000 << 112;
const THREE: u128 = 0x4000_8 << 108;

/// The seam's reason for an `f128` operation whose routine is not bound (tier.rs
/// `legalize_refused`).
const NOT_BOUND: &str = "an `f128` operation whose compiler_builtins routine is not bound (INTRINSICS.md P5b)";

/// The caller's names: every routine `routines` binds is the function reference at its place,
/// named by its C name; nothing else is named (the function under test names nothing itself).
struct Quad {
    routines: RoutinePlaces,
}

impl Quad {
    /// Every routine bound at place `r as u32`.
    fn all() -> Quad {
        let mut routines = RoutinePlaces::NONE;
        for r in SoftFloat::ALL {
            routines.bind(r, r as u32);
        }
        Quad { routines }
    }

    /// No routine bound: the control.
    fn none() -> Quad {
        Quad { routines: RoutinePlaces::NONE }
    }
}

impl Names for Quad {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        match of {
            Outside::Func(place) => SoftFloat::ALL.into_iter().find(|&r| self.routines.place(r) == Some(place)).map(|r| Named::C(r.symbol())),
            Outside::Static(_) | Outside::Vtable(_) | Outside::Tls(_) | Outside::TlsDesc | Outside::TlsGetter => None,
        }
    }

    fn routines(&self) -> RoutinePlaces {
        self.routines
    }
}

/// `fn(a: f128, b: f128) -> f128 { a + b }`.
fn add() -> blaze_lir::Func<impl core::alloc::Allocator + Clone> {
    one(&[Ty::F128, Ty::F128], Ty::F128, |b, v| b.binary(Op::Fadd, Flags::NONE, v[0], v[1]))
}

/// The cell emitter's target as the Reference tier derives it from `machine` (tier.rs).
fn isa(machine: Machine) -> Isa {
    let os = match machine.os {
        blaze_tier::Os::MacOs => Os::MacOs,
        blaze_tier::Os::Linux => Os::Linux,
    };
    Isa { lse: machine.features & Machine::LSE != 0, os }
}

/// `e` sealed and encoded: the bytes two records are compared by.
fn bytes(e: Emitted<'static>, what: &str) -> AVec<'static, u8> {
    let sealed = seal(e).unwrap_or_else(|error| panic!("{what}: the record does not seal: {error}"));
    let mut out: AVec<'static, u8> = Vec::new_in(Alloc::global());
    encode(&sealed, &mut out);
    out
}

/// The run's answers: the poll page and `__addtf3` by their C names, the routine through one
/// import word; anything else is unbound, by name.
struct Bound {
    page: u64,
    /// The address of the 8-byte word holding `__addtf3`'s address.
    word: u64,
}

fn is_c(target: Target, name: &str) -> bool {
    target.space == Space::Outside && target.symbol == Symbol::of_name(name)
}

impl Words for Bound {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        match is_c(target, SoftFloat::AddTf3.symbol()) {
            true => Ok(self.word),
            false => Err(ImageError::Unbound { target }),
        }
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        if is_c(target, POLL_PAGE) {
            return Ok(self.page);
        }
        match is_c(target, SoftFloat::AddTf3.symbol()) {
            true => Ok(__addtf3 as usize as u64),
            false => Err(ImageError::Unbound { target }),
        }
    }
}

/// `record` read and placed by the one new placer: its code where it runs, then in the data its
/// relro part 8-aligned and the import word after it.
fn placed(record: &[u8], tier: TierName) -> Executable {
    let name = tier.as_str();
    let view = read(record).unwrap_or_else(|error| panic!("the {name} tier's record does not read back: {error}"));
    let relro_len = view.relro().len();
    let align = 1usize << view.relro_align();
    let symbol = Symbol::of_name("blaze-emit tier_tests::routines: the f128 add");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(view.code().len(), POLL_PAGE_ROOM + relro_len + align + 16, |code, data| {
        let (page, rest) = carve_poll_page(data);
        let skip = (align - rest.as_ptr() as usize % align) % align;
        let word_at = skip + relro_len.next_multiple_of(8);
        rest[word_at..word_at + 8].copy_from_slice(&(__addtf3 as usize as u64).to_le_bytes());
        let word = rest[word_at..].as_ptr() as u64;
        let relro = &mut rest[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = Bound { page, word };
        done = place(&view, symbol, at, &mut words, code, relro);
    })
    .expect("the code maps");
    done.unwrap_or_else(|error| panic!("the {name} tier's record does not place: {error}"));
    exec
}

/// `exec` called on 1.0 and 2.0: the sum's bits.
fn sum_of_one_and_two(exec: &Executable) -> u128 {
    // SAFETY: a placed C function of two quads (q0, q1) returning one (q0), its import word this
    // binary's `__addtf3`; mapped while `exec` lives.
    let f = unsafe { core::mem::transmute::<usize, extern "C" fn(f128, f128) -> f128>(exec.code_address() as usize) };
    f(f128::from_bits(ONE), f128::from_bits(TWO)).to_bits()
}

/// Whether `error` is the seam's unbound-routine refusal by `tier`.
fn unbound(error: &TierError, tier: TierName) -> bool {
    matches!(error, TierError::Refused { tier: by, inst: None, what } if *by == tier && *what == NOT_BOUND)
}

/// EQUALITY AT THE SEAM, and the control.
///
/// Prints: `the seam's routine table: the Reference record equals emit_record's (N bytes),
/// calling __addtf3; with no table both tiers refuse by name`.
#[test]
fn the_seam_hands_reference_the_routine_table() {
    let machine = table().head().machine;
    let func = add();
    let view = func.view();
    let names = Quad::all();
    let seam = reference(view, &names, machine, Alloc::global()).unwrap_or_else(|error| panic!("the Reference tier refused the f128 add with every routine bound: {error}"));
    let seam = bytes(seam, "through the seam");
    let verified = blaze_lir::verify(view).expect("the test's function is LIR");
    let (record, called) = emit_record(verified, &names, &names.routines, isa(machine), Alloc::global())
        .unwrap_or_else(|error| panic!("emit_record refused the f128 add with every routine bound: {error:?}"));
    assert_eq!(called.iter().collect::<std::vec::Vec<_>>(), [SoftFloat::AddTf3], "the add calls __addtf3 only: {called:?}");
    let direct = from_v1(record, Alloc::global()).unwrap_or_else(|error| panic!("from_v1 refused emit_record's record: {error:?}"));
    let direct = bytes(direct, "emit_record directly");
    assert!(seam == direct, "the seam's record ({} bytes) differs from emit_record's ({} bytes)", seam.len(), direct.len());
    let read_back = read(&seam).unwrap_or_else(|error| panic!("the seam's record does not read back: {error}"));
    let names_addtf3 = (0..read_back.targets()).any(|i| is_c(read_back.target(i), SoftFloat::AddTf3.symbol()));
    assert!(names_addtf3, "the seam's record names no __addtf3 target");

    // THE CONTROL: no table, no call: each tier refuses by the one reason.
    let rows: [(TierName, TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];
    for (tier, emit) in rows {
        match emit(view, &Quad::none(), machine, Alloc::global()) {
            Err(error) if unbound(&error, tier) => {}
            Err(error) => panic!("the {} tier, no routine bound: the wrong refusal: {error}", tier.as_str()),
            Ok(_) => panic!("the {} tier emitted an f128 add with no routine bound", tier.as_str()),
        }
    }
    println!(
        "the seam's routine table: the Reference record equals emit_record's ({} bytes), calling __addtf3; with no table both tiers refuse by name",
        seam.len()
    );
}

/// RUN: each tier's record of the f128 add, with every routine bound, returns 3.0, or (the Fast
/// tier only) is refused by a reason other than an unbound routine, printed for the worklist.
///
/// Prints: `f128 add through the seam: Reference 3.0` then `Fast 3.0` or `Fast refused: ..`.
#[test]
fn an_f128_add_calls_its_routine_through_the_seam() {
    let machine = table().head().machine;
    let func = add();
    let view = func.view();
    let names = Quad::all();
    let rows: [(TierName, TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];
    for (tier, emit) in rows {
        let name = tier.as_str();
        match emit(view, &names, machine, Alloc::global()) {
            Ok(emitted) => {
                let record = bytes(emitted, name);
                let exec = placed(&record, tier);
                let got = sum_of_one_and_two(&exec);
                assert_eq!(got, THREE, "the {name} tier's 1.0 + 2.0: {got:#034x}");
                println!("f128 add through the seam: {name} 3.0");
            }
            Err(error) if tier == TierName::Reference => panic!("the Reference tier refused the f128 add with every routine bound: {error}"),
            Err(error) if unbound(&error, tier) => panic!("the {name} tier was not handed the routine table: {error}"),
            Err(error) => println!("f128 add through the seam: {name} refused: {error}"),
        }
    }
}
