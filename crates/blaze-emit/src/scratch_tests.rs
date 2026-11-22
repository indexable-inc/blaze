//! The scratch pool's promises (scratch.rs): two held uses never share a register, a template's
//! temporaries are never a held register, a held register a template clobbers is refused or
//! spent, and a refusal changes nothing. The composer-level control (a template that really
//! writes a held scratch register, refused by name and wrong when the check is removed) is
//! tier_tests/directed.rs, `a_template_clobbering_a_held_scratch_is_refused`.

use blaze_tier::{ARM64, Reg, RegSet};

use crate::scratch::{Pool, ScratchError, Use};

const X16: Reg = Reg(16);
const X17: Reg = Reg(17);

fn pool() -> Pool {
    match Pool::of(&ARM64) {
        Ok(pool) => pool,
        Err(error) => panic!("arm64's description gives no pool: {}", error.what()),
    }
}

const fn plain(inputs: RegSet, result: Option<Reg>, temps: u8, clobbers: RegSet) -> Use {
    Use { inputs, result, temps, clobbers, call: false }
}

const fn call(inputs: RegSet, clobbers: RegSet) -> Use {
    Use { inputs, result: None, temps: 0, clobbers, call: true }
}

/// arm64's pool is x16 and x17: two takes give both, a third is refused, a give frees one again.
/// Expected: passes. Fails if a take can return a register already held.
#[test]
fn two_takes_never_share_a_register() {
    let mut p = pool();
    let a = p.take().expect("x16 is free");
    let b = p.take().expect("x17 is free");
    assert_ne!(a.reg(), b.reg());
    assert_eq!((a.reg(), b.reg()), (X16, X17), "lowest first");
    assert_eq!(p.take().map(|s| s.reg()), Err(ScratchError::Exhausted));
    assert!(!p.is_idle());
    p.give(a).expect("held");
    let c = p.take().expect("x16 again");
    assert_eq!(c.reg(), X16);
    p.give(b).expect("held");
    p.give(c).expect("held");
    assert!(p.is_idle());
}

/// A template's temporaries come from the free registers only: with x16 held, `TEMP0` is x17,
/// and a template naming `TEMP0` and `TEMP1` is refused (nothing changes on the refusal).
/// Expected: passes. Fails if a temporary can be a held register (the x12 class of D193).
#[test]
fn temporaries_are_never_a_held_register() {
    let mut p = pool();
    assert_eq!(p.lend(plain(RegSet::EMPTY, None, 2, RegSet::EMPTY)), Ok([Some(X16), Some(X17)]));
    let held = p.take().expect("free");
    let one = plain(RegSet::EMPTY.with(held.reg()), None, 1, RegSet::EMPTY);
    assert_eq!(p.lend(one), Ok([Some(X17), None]));
    let before = p.held();
    assert_eq!(p.lend(plain(RegSet::EMPTY.with(held.reg()), None, 2, RegSet::EMPTY)), Err(ScratchError::Exhausted));
    assert_eq!(p.held(), before, "a refusal changes nothing");
    assert_eq!(p.lend(one), Ok([Some(X17), None]), "and the pool still lends what it did");
    p.give(held).expect("held");
}

/// A pool register named as an input or a result must be held.
/// Expected: passes. Fails if a scratch register can be used without a token.
#[test]
fn an_unheld_scratch_register_is_refused() {
    let mut p = pool();
    assert_eq!(p.lend(plain(RegSet::EMPTY.with(X16), None, 0, RegSet::EMPTY)), Err(ScratchError::NotHeld(X16)));
    assert_eq!(p.lend(plain(RegSet::EMPTY, Some(X17), 0, RegSet::EMPTY)), Err(ScratchError::NotHeld(X17)));
    // A register outside the pool is not the pool's business.
    assert_eq!(p.lend(plain(RegSet::EMPTY.with(Reg(9)), Some(Reg(10)), 0, RegSet::EMPTY)), Ok([None, None]));
}

/// A held register in a template's clobbers: refused, even as an input, unless the template is a
/// call that reads it (an indirect call's `blr x16`); then it is spent, and reading it again is
/// refused until it is given back and taken again.
/// Expected: passes. Fails if a clobber of a held register passes unseen.
#[test]
fn a_clobbered_held_register_is_refused_or_spent() {
    let mut p = pool();
    let s = p.take().expect("free");
    let clobbers = RegSet::EMPTY.with(X16).with(X17);
    let read = RegSet::EMPTY.with(s.reg());
    assert_eq!(p.lend(plain(RegSet::EMPTY, None, 0, clobbers)), Err(ScratchError::Clobbered(X16)));
    assert_eq!(p.lend(plain(read, None, 0, clobbers)), Err(ScratchError::Clobbered(X16)), "not a call: no stated order");
    assert_eq!(p.lend(call(RegSet::EMPTY, clobbers)), Err(ScratchError::Clobbered(X16)), "a call not reading it");
    assert_eq!(p.lend(call(read, clobbers)), Ok([None, None]));
    assert_eq!(p.lend(plain(RegSet::EMPTY.with(s.reg()), None, 0, RegSet::EMPTY)), Err(ScratchError::Spent(X16)));
    // Written fresh, it is a value again.
    assert_eq!(p.lend(plain(RegSet::EMPTY, Some(s.reg()), 0, RegSet::EMPTY)), Ok([None, None]));
    assert_eq!(p.lend(plain(RegSet::EMPTY.with(s.reg()), None, 0, RegSet::EMPTY)), Ok([None, None]));
    p.give(s).expect("held");
    assert!(p.is_idle());
}

/// The Reference tier's pool (T3 step 1, `Pool::over` and `REFERENCE_SCRATCH`): x9 to x17,
/// taken lowest first and never twice, each as the encoder's number; a template clobbering a
/// held one is refused (the class of the x12 bug of 2026-11-21: the overflow flag held in x12
/// while the far store wrote x12); a set of one register gives no pool (the control: a pool
/// that accepted it could hand one register to a held use and a temporary at once).
/// Expected: passes. Fails if a take returns a held register or a clobbered hold is accepted.
#[test]
fn the_reference_pool_holds_x9_to_x17() {
    let mut p = match Pool::over(crate::scratch::REFERENCE_SCRATCH) {
        Ok(pool) => pool,
        Err(error) => panic!("the Reference tier's set gives no pool: {}", error.what()),
    };
    let mut held = std::vec::Vec::new();
    for n in 9u8..=17 {
        let s = p.take().expect("a register of x9..x17 is free");
        assert_eq!(s.reg(), Reg(n), "lowest first");
        assert_eq!(s.enc(), u32::from(n), "the encoder's number");
        held.push(s);
    }
    assert_eq!(p.take().map(|s| s.reg()), Err(ScratchError::Exhausted));
    let x12 = held.remove(3);
    assert_eq!(x12.reg(), Reg(12));
    let far_store = plain(RegSet::EMPTY, None, 0, RegSet::EMPTY.with(Reg(12)));
    assert_eq!(p.lend(far_store), Err(ScratchError::Clobbered(Reg(12))), "a template writing the held x12");
    p.give(x12).expect("held");
    assert_eq!(p.lend(far_store), Ok([None, None]), "x12 given back: the template may write it");
    for s in held {
        p.give(s).expect("held");
    }
    assert!(p.is_idle());
    assert!(matches!(Pool::over(RegSet::EMPTY.with(Reg(9))), Err(ScratchError::Description(_))));
}

/// A description with one scratch register, or one the allocator may also hand out, gives no
/// pool.
/// Expected: passes.
#[test]
fn a_description_without_two_private_scratch_registers_is_refused() {
    let one = blaze_tier::RegDesc { scratch: RegSet::EMPTY.with(X16), ..ARM64 };
    assert!(matches!(Pool::of(&one), Err(ScratchError::Description(_))));
    let shared = blaze_tier::RegDesc { allocatable: ARM64.allocatable.with(X16), ..ARM64 };
    assert!(matches!(Pool::of(&shared), Err(ScratchError::Description(_))));
}
