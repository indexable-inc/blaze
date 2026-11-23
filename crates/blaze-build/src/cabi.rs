//! The C calling convention on AArch64: where each argument and the result of an `extern "C"`
//! call go, by the Procedure Call Standard for the Arm 64-bit Architecture (AAPCS64, release
//! 2024Q3), sections 6.8 (parameter passing, stages A to C) and 6.9 (results), and Apple's
//! arm64 variant ("Writing ARM64 code for Apple platforms"). The target is a value passed in
//! ([`Platform`]).
//!
//! # Classes ([`class_of`])
//!
//! - An integer, `bool`, `char` or pointer: [`Class::Int`]. A float: [`Class::Float`].
//! - A homogeneous floating-point aggregate (HFA, 5.9.5.1): a struct, tuple or array whose
//!   fundamental members are all one float type, one to four of them: [`Class::Hfa`].
//! - Any other struct, tuple or array: [`Class::Composite`]; one of more than 16 bytes is passed
//!   as a pointer to a caller's copy (B.4), and returned through the memory `x8` points to.
//! - A zero-sized type takes nothing ([`Class::Ignore`]), as GNU C's empty struct.
//! - Refused by name ([`CAbiError`]): a union, an enum with fields (C has neither in this
//!   form; `repr(C)` enums of fields are a later piece), an unsized type or a `repr(simd)`
//!   vector, alone or inside an aggregate (its SIMD register is not built). An `f16` and an
//!   `f128` are floats (AAPCS64 5.1.2: half and quad precision are fundamental floating-point
//!   types, one `h` or `q` view of a `v` register each), and HFA members (5.9.5.1).
//!
//! # Assignment ([`assign`])
//!
//! Stage C, in argument order, with NGRN (next general register), NSRN (next SIMD register)
//! and NSAA (next stacked argument address):
//! - C.1: a float takes `v[NSRN]` if NSRN < 8. C.2: an HFA of `n` takes `v[NSRN..NSRN+n]` if
//!   they fit; C.3: otherwise NSRN becomes 8 and it goes on the stack.
//! - C.8: an argument of 16-byte alignment rounds NGRN up to even. C.9/C.10: an integer or a
//!   composite of `w` 8-byte words takes `x[NGRN..NGRN+w]` if they fit; C.11: otherwise NGRN
//!   becomes 8 and it goes on the stack.
//! - The stack: AAPCS64 rounds NSAA to the larger of 8 and the argument's alignment and its
//!   size up to 8 (C.12 to C.16). Apple packs each argument at its natural size and alignment,
//!   and passes every variadic argument on the stack in 8-byte slots.
//! - Apple: an integer argument narrower than 32 bits is zero- or sign-extended to 32 bits by
//!   the caller ([`Ext`]); AAPCS64 leaves the bits above it unspecified.
//! - A result (6.9): an integer, float or HFA comes back in the registers it would be passed in
//!   as the first argument; a larger composite through `x8`.

use core::alloc::Allocator;
use core::fmt;

use blaze_typeck::{AdtKind, PNode, PTy};

use crate::layout::{LayoutCx, LayoutError, Layouts, Prim, Tail};

/// The target's variant of AAPCS64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    /// macOS and iOS on arm64.
    Apple,
    /// AAPCS64 as written (Linux on arm64).
    Aapcs,
}

/// What a value is to the convention.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Ignore,
    /// An integer or pointer of `size` bytes (1 to 16).
    Int { size: u8, signed: bool },
    /// A float of `size` bytes (2, 4, 8 or 16).
    Float { size: u8 },
    /// `count` (1 to 4) floats of `size` bytes each.
    Hfa { count: u8, size: u8 },
    /// Any other aggregate.
    Composite { size: u64, align: u8 },
}

/// Why a value has no place in a C call.
#[derive(Debug)]
pub enum CAbiError {
    Layout(LayoutError),
    /// A type the convention is not built for, named.
    Refused { ty: PTy, why: &'static str },
    /// More than 2^31 bytes of stacked arguments.
    TooLarge,
}

impl fmt::Display for CAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CAbiError::Layout(e) => write!(f, "a C argument's type: {e}"),
            CAbiError::Refused { ty, why } => write!(f, "type {} in an `extern \"C\"` signature: blaze does not pass {why}", ty.0),
            CAbiError::TooLarge => f.write_str("a C call's stacked arguments pass 2 GiB"),
        }
    }
}

impl core::error::Error for CAbiError {}

impl From<LayoutError> for CAbiError {
    fn from(e: LayoutError) -> CAbiError {
        CAbiError::Layout(e)
    }
}

/// A float layout's size in bytes, `None` for any other primitive.
fn float_size(p: Prim) -> Option<u8> {
    match p {
        Prim::F16 => Some(2),
        Prim::F32 => Some(4),
        Prim::F64 => Some(8),
        Prim::F128 => Some(16),
        _ => None,
    }
}

/// The class of ground type `ty`.
pub fn class_of<A: Allocator + Copy>(ls: &mut Layouts<A>, ty: PTy, cx: &mut LayoutCx<'_, '_>, scratch: A) -> Result<Class, CAbiError> {
    let id = ls.of(ty, cx)?;
    let l = *ls.get(id);
    let refused = |why| CAbiError::Refused { ty, why };
    if l.tail != Tail::Sized {
        return Err(refused("an unsized value"));
    }
    if l.is_zst() {
        return Ok(Class::Ignore);
    }
    // Every type met by value, by an explicit stack: unions and enums of fields refused; the
    // fundamental members counted, each a scalar layout (a newtype of one included).
    let mut stack: Vec<PTy, A> = Vec::new_in(scratch);
    let mut parts: Vec<PTy, A> = Vec::new_in(scratch);
    stack.push(ty);
    let mut float: Option<u8> = None;
    let mut members = 0u64;
    let mut homogeneous = true;
    while let Some(t) = stack.pop() {
        let tid = ls.of(t, cx)?;
        let tl = *ls.get(tid);
        if tl.is_zst() {
            continue;
        }
        // A transparent value has its field's ABI, including when it is a member of an HFA.
        let t = tl.call_ty;
        let node = cx.ground.view().get(t).copied();
        // Its `repr`: answered, as the layout above read it; an unmade head refuses by name.
        let simd = match node {
            Some(PNode::Adt(key, _)) => cx.facts.repr(key).map_err(|m| LayoutError { ty: t, why: m.head() })?.simd,
            _ => false,
        };
        match node {
            // A short vector (layout/simd.rs) is a fundamental type of its own (AAPCS64 5.9):
            // passed in ONE SIMD register, never as an HFA of its lanes or in `x` registers.
            Some(PNode::Adt(..)) if simd => {
                return Err(refused("a `repr(simd)` vector (AAPCS64 passes it in one SIMD register, which blaze's C calls do not build yet)"));
            }
            Some(PNode::Adt(key, _)) => match cx.facts.adt(key).map(|a| a.kind) {
                Some(AdtKind::Union) => return Err(refused("a union")),
                Some(AdtKind::Enum) if tl.scalar.is_none() => return Err(refused("an enum with fields")),
                _ => {}
            },
            _ => {}
        }
        if let Some(s) = tl.scalar {
            members += 1;
            match (float_size(s.prim), float) {
                (Some(f), None) if members == 1 => float = Some(f),
                (Some(f), Some(g)) if f == g => {}
                _ => homogeneous = false,
            }
            continue;
        }
        parts.clear();
        ls.field_types(t, cx, &mut parts)?;
        match (node, parts.first().copied()) {
            (Some(PNode::Array(_, n)), Some(e)) => {
                let n = cx.ground.view().value_u64(n).ok_or(refused("an array whose length is not a value"))?;
                // More than four elements is no HFA: push at most five to say so.
                for _ in 0..n.min(5) {
                    stack.push(e);
                }
            }
            // A wide pointer: two non-float members.
            (_, None) => {
                members += 2;
                homogeneous = false;
            }
            _ => {
                for &p in parts.iter().rev() {
                    stack.push(p);
                }
            }
        }
        if members > 4 {
            homogeneous = false;
        }
    }
    // A scalar layout is the scalar's class, as C passes a one-member struct's member.
    match (l.scalar, float, homogeneous) {
        (Some(_), Some(size), _) => Ok(Class::Float { size }),
        (Some(s), None, _) => Ok(Class::Int { size: l.size as u8, signed: matches!(s.prim, Prim::Int { signed: true, .. }) }),
        (None, Some(size), true) if (1..=4).contains(&members) => Ok(Class::Hfa { count: members as u8, size }),
        (None, ..) => Ok(Class::Composite { size: l.size, align: l.align }),
    }
}

/// How an integer narrower than 32 bits is widened by the caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ext {
    None,
    Zext32,
    Sext32,
}

/// Where a value goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// General registers `x[first..first + count]`.
    Gpr { first: u8, count: u8 },
    /// SIMD registers `v[first..first + count]`, each holding one float of the class.
    Fpr { first: u8, count: u8 },
    /// `size` bytes at `offset` from the stack pointer at the call.
    Stack { offset: u32, size: u32 },
}

/// Where an argument goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgLoc {
    Ignore,
    Direct { at: Place, ext: Ext },
    /// The caller copies the value to its memory and passes the copy's address at `at`.
    Indirect { at: Place },
}

/// Where the result comes back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetLoc {
    Ignore,
    Direct(Place),
    /// The caller passes the address of the result's memory in `x8`.
    Indirect,
}

struct State {
    ngrn: u8,
    nsrn: u8,
    nsaa: u64,
}

impl State {
    fn stack(&mut self, size: u64, align: u8, platform: Platform, variadic: bool) -> Result<Place, CAbiError> {
        let (size, align) = match (platform, variadic) {
            (Platform::Apple, false) => (size, 1u64 << align),
            _ => (size.div_ceil(8) * 8, (1u64 << align).max(8)),
        };
        let at = self.nsaa.div_ceil(align) * align;
        self.nsaa = at + size;
        if self.nsaa > i32::MAX as u64 {
            return Err(CAbiError::TooLarge);
        }
        Ok(Place::Stack { offset: at as u32, size: size as u32 })
    }

    fn gprs(&mut self, words: u8, align16: bool) -> Option<Place> {
        if align16 {
            self.ngrn = self.ngrn.div_ceil(2) * 2;
        }
        if self.ngrn + words <= 8 {
            let first = self.ngrn;
            self.ngrn += words;
            Some(Place::Gpr { first, count: words })
        } else {
            self.ngrn = 8;
            None
        }
    }
}

/// Where each argument of classes `args` goes, into `out`; the arguments from index
/// `variadic_from` on are the `...` of a variadic call.
pub fn assign<A: Allocator>(args: &[Class], variadic_from: Option<usize>, platform: Platform, out: &mut Vec<ArgLoc, A>) -> Result<(), CAbiError> {
    let mut s = State { ngrn: 0, nsrn: 0, nsaa: 0 };
    for (i, &c) in args.iter().enumerate() {
        let variadic = variadic_from.is_some_and(|v| i >= v);
        let on_stack = variadic && platform == Platform::Apple;
        let loc = match c {
            Class::Ignore => ArgLoc::Ignore,
            Class::Float { size } => {
                let at = if !on_stack && s.nsrn < 8 {
                    s.nsrn += 1;
                    Place::Fpr { first: s.nsrn - 1, count: 1 }
                } else {
                    s.nsrn = 8;
                    s.stack(u64::from(size), size.trailing_zeros() as u8, platform, variadic)?
                };
                ArgLoc::Direct { at, ext: Ext::None }
            }
            Class::Hfa { count, size } => {
                let at = if !on_stack && s.nsrn + count <= 8 {
                    s.nsrn += count;
                    Place::Fpr { first: s.nsrn - count, count }
                } else {
                    s.nsrn = 8;
                    s.stack(u64::from(count) * u64::from(size), size.trailing_zeros() as u8, platform, variadic)?
                };
                ArgLoc::Direct { at, ext: Ext::None }
            }
            Class::Int { size, signed } => {
                let ext = match (platform, size < 4, signed) {
                    (Platform::Apple, true, true) => Ext::Sext32,
                    (Platform::Apple, true, false) => Ext::Zext32,
                    _ => Ext::None,
                };
                let words = size.div_ceil(8);
                let at = match (on_stack, s.gprs(words, size == 16)) {
                    (false, Some(at)) => at,
                    _ => s.stack(u64::from(size), size.trailing_zeros() as u8, platform, variadic)?,
                };
                ArgLoc::Direct { at, ext }
            }
            Class::Composite { size, .. } if size > 16 => {
                let at = match (on_stack, s.gprs(1, false)) {
                    (false, Some(at)) => at,
                    _ => s.stack(8, 3, platform, variadic)?,
                };
                ArgLoc::Indirect { at }
            }
            Class::Composite { size, align } => {
                let words = size.div_ceil(8) as u8;
                let at = match (on_stack, s.gprs(words, align == 4)) {
                    (false, Some(at)) => at,
                    _ => s.stack(size, align, platform, variadic)?,
                };
                ArgLoc::Direct { at, ext: Ext::None }
            }
        };
        out.push(loc);
    }
    Ok(())
}

/// Where a result of class `c` comes back (6.9).
pub fn ret(c: Class) -> RetLoc {
    match c {
        Class::Ignore => RetLoc::Ignore,
        Class::Int { size, .. } => RetLoc::Direct(Place::Gpr { first: 0, count: size.div_ceil(8) }),
        Class::Float { .. } => RetLoc::Direct(Place::Fpr { first: 0, count: 1 }),
        Class::Hfa { count, .. } => RetLoc::Direct(Place::Fpr { first: 0, count }),
        Class::Composite { size, .. } if size > 16 => RetLoc::Indirect,
        Class::Composite { size, .. } => RetLoc::Direct(Place::Gpr { first: 0, count: size.div_ceil(8) as u8 }),
    }
}

#[cfg(test)]
mod tests;
