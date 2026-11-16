//! The set operations of both forms against a model over the plain integer (`all` is the OR of
//! the declared flags; `complement`, `!` and `from_bits_truncate` keep only those bits), the
//! text form against bitflags' own outputs, and a planted-bug control (rule 53): a complement
//! that keeps undeclared bits, which the oracle must catch.

use std::{format, string::String, vec::Vec};

use blaze_sim::{Broke, Config, Tape};

use crate::{Flags, parser};

crate::bitflags! {
    /// Three flags, one of them two of the others, and bit 6 declared by none.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Set: u8 {
        const A = 1 << 0;
        const B = 1 << 1;
        /// Both of the above.
        const AB = Set::A.bits() | Set::B.bits();
        const C = 1 << 3;
        const HIGH = 1 << 7;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Own(u16);

crate::bitflags! {
    impl Own: u16 {
        const X = 1 << 0;
        const Y = 1 << 9;
    }
}

// The set operations work in a constant, with the test's `Ops` trait in scope.
const _: () = assert!(Set::A.union(Set::C).contains(Set::C) && Set::all().bits() == SET_ALL);

/// The declared bits of `Set`, written out: the model's `all`.
const SET_ALL: u8 = 0b1000_1011;

/// The operations the oracle checks, so a planted copy can stand in for `Set`. `bits` takes
/// `&self` as the inherent one does: a by-value `bits` in scope would be picked before the
/// inherent `const fn` on a by-value receiver (`Set::A.bits()` in a constant).
trait Ops: Copy {
    fn from_bits_retain(bits: u8) -> Self;
    fn bits(&self) -> u8;
    fn complement(self) -> Self;
}

impl Ops for Set {
    fn from_bits_retain(bits: u8) -> Set {
        Set::from_bits_retain(bits)
    }
    fn bits(&self) -> u8 {
        Set::bits(self)
    }
    fn complement(self) -> Set {
        Set::complement(self)
    }
}

/// `Set` with a complement that forgets to drop undeclared bits: the planted bug.
#[derive(Clone, Copy)]
struct KeepsUndeclared(Set);

impl Ops for KeepsUndeclared {
    fn from_bits_retain(bits: u8) -> KeepsUndeclared {
        KeepsUndeclared(Set::from_bits_retain(bits))
    }
    fn bits(&self) -> u8 {
        Set::bits(&self.0)
    }
    fn complement(self) -> KeepsUndeclared {
        KeepsUndeclared(Set::from_bits_retain(!self.0.bits()))
    }
}

fn same<T: PartialEq + core::fmt::Debug>(what: String, got: T, want: T) -> Result<(), Broke> {
    if got == want { Ok(()) } else { Err(Broke::Oracle(format!("{what}: got {got:?}, the model {want:?}"))) }
}

/// The refinement: each operation on two drawn sets is the model's on their bits.
fn case<O: Ops>(tape: &Tape) -> Result<(), Broke> {
    let (a, b) = (tape.int_in(0u8..=255, "a"), tape.int_in(0u8..=255, "b"));
    let (x, y) = (Set::from_bits_retain(a), Set::from_bits_retain(b));
    same(format!("{a:#x} | {b:#x}"), (x | y).bits(), a | b)?;
    same(format!("{a:#x} & {b:#x}"), (x & y).bits(), a & b)?;
    same(format!("{a:#x} ^ {b:#x}"), (x ^ y).bits(), a ^ b)?;
    same(format!("{a:#x} - {b:#x}"), (x - y).bits(), a & !b)?;
    same(format!("!{a:#x}"), (!x).bits(), !a & SET_ALL)?;
    same(format!("complement of {a:#x}"), O::from_bits_retain(a).complement().bits(), !a & SET_ALL)?;
    same(format!("{a:#x} contains {b:#x}"), x.contains(y), a & b == b)?;
    same(format!("{a:#x} intersects {b:#x}"), x.intersects(y), a & b != 0)?;
    same(format!("from_bits {a:#x}"), Set::from_bits(a).map(|s| s.bits()), (a & !SET_ALL == 0).then_some(a))?;
    same(format!("from_bits_truncate {a:#x}"), Set::from_bits_truncate(a).bits(), a & SET_ALL)?;
    same(format!("{a:#x} is_all"), x.is_all(), a & SET_ALL == SET_ALL)?;
    let mut z = x;
    z.insert(y);
    same(format!("insert {b:#x} into {a:#x}"), z.bits(), a | b)?;
    z.remove(y);
    same(format!("remove {b:#x}"), z.bits(), (a | b) & !b)?;
    z.set(y, true);
    z.toggle(x);
    same(format!("set {b:#x}, toggle {a:#x}"), z.bits(), (((a | b) & !b) | b) ^ a)?;
    // The iterator yields flags whose union is the set, and collecting it rebuilds the set.
    same(format!("iter of {a:#x}"), x.iter().collect::<Set>().bits(), a)?;
    let names: Vec<&str> = x.iter_names().map(|(name, _)| name).collect();
    for name in &names {
        let flag = Set::from_name(name).ok_or_else(|| Broke::Oracle(format!("no flag named {name}")))?;
        same(format!("{name} in {a:#x}"), x.contains(flag), true)?;
    }
    Ok(())
}

#[test]
fn the_set_operations_are_the_models() {
    blaze_sim::check(Config::cases(512), case::<Set>);
}

#[derive(Clone, Copy, Debug)]
enum Plant {
    Clean,
    ComplementKeepsUndeclared,
}

#[test]
fn the_oracle_catches_a_complement_that_keeps_undeclared_bits() {
    let run = |tape: &Tape, plant: Plant| match plant {
        Plant::Clean => case::<Set>(tape),
        Plant::ComplementKeepsUndeclared => case::<KeepsUndeclared>(tape),
    };
    blaze_sim::control(Config::cases(512), Plant::Clean, Plant::ComplementKeepsUndeclared, "the model", run);
}

/// The text forms bitflags 2 prints, for the cases its own tests name: the empty set, one
/// flag, a composite flag that adds no bit, and bits no flag names.
#[test]
fn debug_is_bitflags_text_form() {
    assert_eq!(format!("{:?}", Set::empty()), "Set(0x0)");
    assert_eq!(format!("{:?}", Set::A), "Set(A)");
    assert_eq!(format!("{:?}", Set::A | Set::B), "Set(A | B)");
    assert_eq!(format!("{:?}", Set::AB | Set::C), "Set(A | B | C)");
    assert_eq!(format!("{:?}", Set::from_bits_retain(0b0100_0001)), "Set(A | 0x40)");
    assert_eq!(format!("{:?}", Set::from_bits_retain(0b0100_0000)), "Set(0x40)");
    let mut text = String::new();
    parser::to_writer(&(Own::X | Own::Y), &mut text).unwrap();
    assert_eq!(text, "X | Y");
    assert_eq!(<Own as Flags>::FLAGS.len(), 2);
    assert_eq!(Own::all().bits(), 0b10_0000_0001);
    assert_eq!(Own::from_bits(1 << 4), None);
}
