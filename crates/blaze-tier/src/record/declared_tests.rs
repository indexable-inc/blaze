//! The `bzcode03` declared-instances section (D106): a round trip, the bytes equal for two
//! orders of insertion, every truncation and a seeded sample of bit flips of a sealed record
//! refused by a typed error (a record is shared: its bytes are untrusted), a `bzcode03` and a
//! `bzcode07` head refused naming both numbers, and the seal refusing BY NAME a row no target uses and a target
//! with no row (so a walk trusts the section: rule 74). Each test prints the line its doc names.
//!
//! Never compiled by its writer (rule 0c): the BUILDER INTEGRATOR runs it.

extern crate std;

use std::println;

use blaze_code::record::{Space, Symbol, Target, Word};
use blaze_own::{AVec, Alloc, Vec};

use super::form::{FormError, MAGIC, encode, read, seal};
use super::{Declare, DeclaredKind, Declares, Emitted, declare};

const RET: u32 = 0xD65F_03C0;

fn target(space: Space, name: &str) -> Target {
    Target { space, symbol: Symbol::of_name(name) }
}

/// The five in-program targets of the test record and an outside one.
fn targets() -> [Target; 6] {
    [
        target(Space::Func, "callee"),
        target(Space::Static, "ANSWER"),
        target(Space::Vtable, "VTABLE"),
        target(Space::Tls, "LOCAL"),
        target(Space::Func, "getter"),
        target(Space::Outside, "puts"),
    ]
}

/// The producer's names: each in-program target's key data, as the run path would hand it.
struct Names;

impl Declares for Names {
    fn declared(&self, t: Target) -> Option<Declare<'_>> {
        let [callee, answer, vtable, local, getter, _] = targets();
        let d = |kind, item, name: &'static [u8]| Some(Declare { kind, item, name });
        match t {
            t if t == callee => d(DeclaredKind::Instance, 0x11, b"callee instance name"),
            t if t == answer => d(DeclaredKind::Static, 0x22, b""),
            t if t == vtable => d(DeclaredKind::Vtable, 0x33, b"concrete tree, trait args"),
            t if t == local => d(DeclaredKind::ThreadLocal, 0x44, b""),
            t if t == getter => d(DeclaredKind::TlsGetter, 0, b""),
            _ => None,
        }
    }
}

/// A record with the targets in `order` (indices into [`targets`], a repeat allowed), whose
/// relro holds at `8 * i` one word naming target `i` (at its first place in `order`), declared
/// through [`Names`]; the outside target's C name is kept. Every order of one set of targets is
/// the same record.
fn record(order: &[usize], alloc: Alloc<'static>) -> Emitted<'static> {
    let all = targets();
    let mut e = Emitted::new_in(alloc);
    e.code.extend_from_slice(&RET.to_le_bytes());
    e.relro.extend_from_slice(&[0; 48]);
    for (k, &i) in order.iter().enumerate() {
        e.targets.push(all[i]);
        if !order[..k].contains(&i) {
            e.words.push(Word { at: (8 * i) as u32, target: k as u32, addend: 0 });
        }
    }
    e.text.extend_from_slice(b"puts");
    e.names.push((Symbol::of_name("puts"), 0, 4));
    if let Err(error) = declare(&mut e, &Names) {
        panic!("the test record's targets are all declared: {error}");
    }
    e
}

fn bytes_of(e: Emitted<'static>, alloc: Alloc<'static>) -> AVec<'static, u8> {
    let sealed = match seal(e) {
        Ok(sealed) => sealed,
        Err(error) => panic!("the control record seals: {error}"),
    };
    let mut out: AVec<'static, u8> = Vec::new_in(alloc);
    encode(&sealed, &mut out);
    out
}

/// Prints `bzcode03: the declared section round-trips; two insertion orders write equal bytes`.
/// Fails if the section is not written, read or sealed alike, if the seal keeps the producer's
/// order (rule 25), or if a row's kind, item or name comes back other than it went in.
#[test]
fn declared_section_round_trips_and_is_canonical() {
    let alloc = blaze_own::testing::heap().alloc();
    let first = bytes_of(record(&[0, 1, 2, 3, 4, 5], alloc), alloc);
    let second = bytes_of(record(&[5, 4, 3, 2, 1, 0, 0, 2], alloc), alloc);
    assert_eq!(&first[..], &second[..], "the order of insertion (and a repeated target) does not reach the bytes");
    assert_eq!(first[..8], MAGIC, "the head is the form's");
    let view = match read(&first) {
        Ok(view) => view,
        Err(error) => panic!("a sealed record reads back: {error}"),
    };
    assert_eq!(view.declared_count(), 5, "one row per in-program target, none for the outside one");
    for i in 1..view.declared_count() {
        assert!(view.declared(i - 1).symbol < view.declared(i).symbol, "rows sorted by symbol, each once");
    }
    let callee = view.declared_of(Symbol::of_name("callee"));
    assert_eq!(callee.map(|d| (d.kind, d.item, d.name)), Some((DeclaredKind::Instance, 0x11, &b"callee instance name"[..])));
    let vtable = view.declared_of(Symbol::of_name("VTABLE"));
    assert_eq!(vtable.map(|d| (d.kind, d.item, d.name)), Some((DeclaredKind::Vtable, 0x33, &b"concrete tree, trait args"[..])));
    assert_eq!(view.declared_of(Symbol::of_name("puts")), None, "an outside name is no declared instance");
    assert_eq!(view.name(Symbol::of_name("puts")), Some("puts"), "the C names keep their place in the text");
    let again = bytes_of(view.to_emitted(alloc), alloc);
    assert_eq!(&first[..], &again[..], "read, back in memory, sealed again: equal bytes");
    println!("bzcode03: the declared section round-trips; two insertion orders write equal bytes");
}

/// Prints `bzcode03: every truncation and 4096 seeded bit flips are refused by a typed error`.
/// Fails (or panics) if a prefix or a flipped record reads as a record: a shared record's bytes
/// are untrusted, and `read` is where they are checked, once.
#[test]
fn truncations_and_bit_flips_are_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let bytes = bytes_of(record(&[0, 1, 2, 3, 4, 5], alloc), alloc);
    for len in 0..bytes.len() {
        assert!(read(&bytes[..len]).is_err(), "the prefix of {len} of {} bytes is refused", bytes.len());
    }
    let mut flipped: AVec<'static, u8> = Vec::new_in(alloc);
    flipped.extend_from_slice(&bytes);
    // xorshift64 from a fixed seed: the sample is the same on every machine and every run.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..4096 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bit = (state % (bytes.len() as u64 * 8)) as usize;
        flipped[bit / 8] ^= 1 << (bit % 8);
        assert!(read(&flipped).is_err(), "the record with bit {bit} flipped is refused");
        flipped[bit / 8] ^= 1 << (bit % 8);
    }
    assert!(read(&flipped).is_ok(), "the control: the unflipped bytes read");
    println!("bzcode03: every truncation and 4096 seeded bit flips are refused by a typed error");
}

/// Prints `bzcode08: a bzcode03 and a bzcode07 record are refused naming both numbers`. Fails
/// if an old record (this form's last, or blaze-code's) is misread or refused without saying
/// which form it is (rule 26: reported and rebuilt).
#[test]
fn an_old_form_is_refused_by_both_numbers() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut bytes = bytes_of(record(&[0, 1, 2, 3, 4, 5], alloc), alloc);
    bytes[..8].copy_from_slice(b"bzcode03");
    assert_eq!(read(&bytes).err(), Some(FormError::OtherForm { found: 3, reads: 8 }));
    bytes[..8].copy_from_slice(b"bzcode07");
    assert_eq!(read(&bytes).err(), Some(FormError::OtherForm { found: 7, reads: 8 }));
    bytes[..8].copy_from_slice(b"notacode");
    assert_eq!(read(&bytes).err(), Some(FormError::Magic));
    println!("bzcode08: a bzcode03 and a bzcode07 record are refused naming both numbers");
}

/// Prints `bzcode03: the seal refuses an unused row and an undeclared target by symbol`. Fails
/// if a record whose section and targets disagree seals: a walk must trust the section.
#[test]
fn the_seal_refuses_a_section_that_disagrees_with_the_targets() {
    let alloc = blaze_own::testing::heap().alloc();
    // The control.
    assert!(seal(record(&[0, 1, 2, 3, 4, 5], alloc)).is_ok(), "the control seals");
    // A row no target uses: the callee's row stays, its target goes.
    let mut e = record(&[0, 1, 2, 3, 4, 5], alloc);
    e.targets[0] = target(Space::Func, "another callee");
    let err = seal(e).err();
    assert!(
        matches!(err, Some(FormError::DeclaredUnused { symbol, kind: DeclaredKind::Instance }) if symbol == Symbol::of_name("callee"))
            || matches!(err, Some(FormError::DeclaredMissing { symbol, space: Space::Func }) if symbol == Symbol::of_name("another callee")),
        "the disagreement is refused by name, got {err:?}"
    );
    // Only the unused row: an extra row for a symbol no target is.
    let mut e = record(&[0, 1, 2, 3, 4, 5], alloc);
    e.declared.push(super::DeclaredRow { symbol: Symbol::of_name("nobody"), kind: DeclaredKind::Static, item: 7, at: 0, len: 0 });
    assert_eq!(seal(e).err(), Some(FormError::DeclaredUnused { symbol: Symbol::of_name("nobody"), kind: DeclaredKind::Static }));
    // Only the missing row: a target added after the declare.
    let mut e = record(&[0, 1, 2, 3, 4, 5], alloc);
    e.targets.push(target(Space::Static, "UNDECLARED"));
    assert_eq!(seal(e).err(), Some(FormError::DeclaredMissing { symbol: Symbol::of_name("UNDECLARED"), space: Space::Static }));
    // A row of the wrong space for its target.
    let mut e = record(&[0, 1, 2, 3, 4, 5], alloc);
    for r in e.declared.iter_mut() {
        if r.symbol == Symbol::of_name("ANSWER") {
            r.kind = DeclaredKind::TlsGetter;
            r.item = 0;
        }
    }
    let err = seal(e).err();
    assert!(
        matches!(err, Some(FormError::DeclaredUnused { kind: DeclaredKind::TlsGetter, .. }) | Some(FormError::DeclaredSpace { .. })),
        "a static declared as a getter is refused, got {err:?}"
    );
    // Two different rows of one symbol.
    let mut e = record(&[0, 1, 2, 3, 4, 5], alloc);
    e.declared.push(super::DeclaredRow { symbol: Symbol::of_name("ANSWER"), kind: DeclaredKind::Static, item: 0x99, at: 0, len: 0 });
    assert_eq!(seal(e).err(), Some(FormError::DeclaredTwice { symbol: Symbol::of_name("ANSWER") }));
    // A producer with no row for a target: `declare` refuses it by name.
    struct Nobody;
    impl Declares for Nobody {
        fn declared(&self, _: Target) -> Option<Declare<'_>> {
            None
        }
    }
    let mut e = Emitted::new_in(alloc);
    e.code.extend_from_slice(&RET.to_le_bytes());
    e.targets.push(target(Space::Func, "callee"));
    assert_eq!(declare(&mut e, &Nobody).err(), Some(FormError::DeclaredMissing { symbol: Symbol::of_name("callee"), space: Space::Func }));
    println!("bzcode03: the seal refuses an unused row and an undeclared target by symbol");
}
