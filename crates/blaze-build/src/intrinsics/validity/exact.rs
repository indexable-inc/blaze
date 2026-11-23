//! The exact judgement of the three validity assertions, by the layout records the builder
//! already has (`Layouts`: never a second layout).
//!
//! The rules (the Rust Reference, "Behavior considered undefined", producing an invalid value:
//! "A value in a `bool` other than `false` (0) or `true` (1)"; "A discriminant in an `enum` not
//! included in the type definition"; "A value in a `char` which is a surrogate or above
//! `char::MAX`"; "A `!` (all values are invalid for this type)"; "A null `fn` pointer"; "A
//! reference or `Box<T>` that is dangling, misaligned, or points to an invalid value"; "Invalid
//! metadata in a wide reference, `Box<T>`, or raw pointer"; "Invalid values for a type with a
//! custom definition of invalid values. In the standard library, this affects `NonNull<T>` and
//! `NonZero*`"):
//!
//! - **inhabited**: a struct, tuple, closure or pattern type is inhabited iff every field is; an
//!   array of length 0 always is, a longer one iff its element is; a union always is (its value
//!   is any bytes); an enum iff SOME variant has every field inhabited (none: an empty enum is
//!   not); a leaf (a scalar, a pointer, `!`, a fn item) by its layout record's own word. Decided
//!   by an and-or evaluation over the type graph (a work stack, each type once), not by the
//!   enum row's flag, which a niche-encoded enum sets `false` whatever its variants hold.
//! - **the fill** (`assert_zero_valid`: every byte 0; `assert_mem_uninitialized_valid`: every
//!   byte 0x01): the value whose every byte is the fill must be a valid value. A leaf scalar
//!   (`bool`, `char`, an integer, a float, a fn pointer) must hold the fill in its record's
//!   valid range; a reference or raw pointer the same for its address and metadata, a `dyn`
//!   pointee's metadata (neither 0 nor 0x0101..01 is a vtable) never; a struct with
//!   `#[rustc_layout_scalar_valid_range_*]` (`NonNull`) and a pattern type (`NonZero`'s inner
//!   `u8 is 1..`) by their narrowed scalar; a struct, tuple or closure by every field; a
//!   non-empty array by its element (an array of length 0 always passes); a union always; an
//!   enum by THE variant the fill selects through its layout's tag encoding
//!   (`Layouts::variant_of_tag` at the fill's value of the tag's width: a direct tag must equal
//!   some discriminant, a niche value names its variant or the untagged one) and every field of
//!   that variant; no variant selected (an empty enum, a direct tag no discriminant equals):
//!   refused.
//! - **0x01 only**: a reference is also refused when its pointee has an alignment above 1 (the
//!   address 0x0101..01 is odd) or any size (a sized pointee of size > 0, a slice tail of
//!   element size > 0 at length 0x0101..01: not dereferenceable).
//!
//! The strict form. The pinned core says the assertions "do not *guarantee* to ever panic, and
//! should only be called if an assertion failure will imply language UB in the following code"
//! (library/core/src/intrinsics/mod.rs:753-782); `mem::uninitialized` writes 0x01 to every byte
//! after the assertion "as an imperfect mitigation for old code that uses this function on bool,
//! nonnull, and noundef types" (library/core/src/mem/mod.rs:801-805). So the uninitialized
//! assertion judges the 0x01-filled value, exactly, and NOT truly uninitialized memory (the
//! strict form, which would refuse every integer): the bytes the program then holds are 0x01,
//! and a panic is due exactly when that value is invalid.
//!
//! Not judged here, refused by name: a coroutine's state (`Future`, `Coroutine`: which state the
//! fill selects is the coroutine pass's encoding, not read here) and an unsized type (the
//! assertions' `T` is sized). `Box<T>` under the 0x01 fill is judged by its `NonNull` field's
//! scalar alone (non-null): the layout records do not say which ADT is the `owned_box` lang
//! item, so its pointee's alignment and size are not read (the Reference makes it UB; the
//! assertion "does not guarantee to ever panic").

use core::alloc::Allocator;
use core::ops::Range;

use blaze_typeck::{Adt, AdtKind, PNode, PTy};

use super::Check;
use crate::layout::{Fields, Layout, LayoutCx, LayoutError, Layouts, Refusal, Tail, Variants};

/// Whether `ty` (ground) passes `check`: `true` builds nothing, `false` builds the panic.
pub fn permits<A: Allocator + Clone>(check: Check, ty: PTy, layouts: &mut Layouts<A>, cx: &mut LayoutCx<'_, '_>) -> Result<bool, LayoutError> {
    layouts.of(ty, cx)?;
    match check {
        Check::Inhabited => inhabited(ty, layouts, cx),
        Check::Zero | Check::Ones => filled(check, ty, layouts, cx),
    }
}

/// `t`'s node in the ground table.
fn node_of(t: PTy, cx: &LayoutCx<'_, '_>) -> Result<PNode, LayoutError> {
    cx.ground.view().get(t).copied().ok_or(LayoutError { ty: t, why: Refusal::Malformed("a type past the ground table") })
}

/// Every scalar the record holds (one, or both halves of a pair) holds the fill.
fn scalars_hold(check: Check, l: &Layout) -> bool {
    l.scalar.is_none_or(|s| check.holds(s)) && l.pair.is_none_or(|(a, b)| check.holds(a) && check.holds(b))
}

/// The narrowed scalar (a valid-range struct's or a pattern type's: the one scalar, or a
/// pair's first half) holds the fill; the rest is its fields'.
fn narrowed_holds(check: Check, l: &Layout) -> bool {
    l.scalar.is_none_or(|s| check.holds(s)) && l.pair.is_none_or(|(a, _)| check.holds(a))
}

/// The fill checks, over a work list of the types held by value (no recursion).
fn filled<A: Allocator + Clone>(check: Check, ty: PTy, layouts: &mut Layouts<A>, cx: &mut LayoutCx<'_, '_>) -> Result<bool, LayoutError> {
    let facts = cx.facts;
    let mut todo: Vec<PTy, A> = Vec::new_in(layouts.alloc());
    let mut parts: Vec<PTy, A> = Vec::new_in(layouts.alloc());
    todo.push(ty);
    while let Some(t) = todo.pop() {
        let id = layouts.of(t, cx)?;
        let l = *layouts.get(id);
        let refuse = |why| LayoutError { ty: t, why };
        // `None`: every type it holds by value; `Some(r)`: those of `field_types`'s `r` only.
        let held: Option<Range<usize>> = match node_of(t, cx)? {
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Never | PNode::FnDef(..) | PNode::FnPtr(..) => {
                if l.uninhabited || !scalars_hold(check, &l) {
                    return Ok(false);
                }
                continue;
            }
            node @ (PNode::Ref(_, pointee) | PNode::Ptr(_, pointee)) => {
                if !scalars_hold(check, &l) {
                    return Ok(false);
                }
                let pid = layouts.of(pointee, cx)?;
                let p = *layouts.get(pid);
                if p.tail == Tail::Dyn {
                    return Ok(false);
                }
                if check == Check::Ones && matches!(node, PNode::Ref(..)) {
                    let empty = match p.tail {
                        Tail::Sized => p.size == 0,
                        Tail::Slice { stride } => p.size == 0 && stride == 0,
                        Tail::Dyn => false,
                    };
                    if p.align > 0 || !empty {
                        return Ok(false);
                    }
                }
                continue;
            }
            PNode::Pat(..) => {
                if !narrowed_holds(check, &l) {
                    return Ok(false);
                }
                continue;
            }
            PNode::Adt(key, _) => {
                let adt = facts.adt(key).ok_or(refuse(Refusal::NoAdt(key)))?;
                match adt.kind {
                    AdtKind::Union => continue,
                    AdtKind::Struct => {
                        let range = facts.valid_range(key).map_err(|m| refuse(m.head()))?;
                        if (range.start.is_some() || range.end.is_some()) && !narrowed_holds(check, &l) {
                            return Ok(false);
                        }
                        None
                    }
                    AdtKind::Enum => {
                        let v = match l.variants {
                            Variants::Single { index } => Some(index),
                            Variants::Multiple { tag, .. } => layouts.variant_of_tag(id, check.fill(tag.prim.bits())),
                        };
                        // No variant: an empty enum, or a direct tag no discriminant equals.
                        let Some(variant) = v.and_then(|v| adt.variants.get(v as usize)) else { return Ok(false) };
                        Some(variant.fields.range())
                    }
                }
            }
            PNode::Array(..) => match l.fields {
                Fields::Array { count: 0, .. } => continue,
                _ => None,
            },
            PNode::Tuple(_) | PNode::Record(..) | PNode::Closure { .. } => None,
            PNode::Union(ul) => {
                if ul.len == 0 {
                    return Ok(false);
                }
                if ul.len == 1 {
                    None
                } else {
                    let v = match l.variants {
                        Variants::Single { index } => Some(index),
                        Variants::Multiple { tag, .. } => layouts.variant_of_tag(id, check.fill(tag.prim.bits())),
                    };
                    let Some(v) = v.and_then(|v| usize::try_from(v).ok()) else { return Ok(false) };
                    let Some(end) = v.checked_add(1) else { return Ok(false) };
                    Some(v..end)
                }
            }
            PNode::Future(..) | PNode::Coroutine { .. } => {
                return Err(refuse(Refusal::Unsupported(
                    "the zero or 0x01 validity of a coroutine's state (which state the fill selects is the coroutine pass's encoding)",
                )));
            }
            PNode::Str | PNode::Slice(_) | PNode::Dynamic(_) => {
                return Err(refuse(Refusal::Unsupported("the zero or 0x01 validity of an unsized type (the assertion's `T` is sized)")));
            }
            _ => return Err(refuse(Refusal::Malformed("a validity assertion's type the layout lays out but no rule names"))),
        };
        parts.clear();
        layouts.field_types(t, cx, &mut parts)?;
        let fields = match held {
            None => &parts[..],
            Some(r) => parts.get(r).ok_or(refuse(Refusal::Malformed("a variant's fields past their column")))?,
        };
        todo.extend_from_slice(fields);
    }
    Ok(true)
}

/// A type's state in [`inhabited`]'s evaluation.
const UNSEEN: u8 = 0;
const OPEN: u8 = 1;
const YES: u8 = 2;
const NO: u8 = 3;

/// What decides whether a type is inhabited.
#[derive(Clone, Copy)]
enum Form<'w> {
    /// Every type it holds by value.
    All,
    /// At least one member of a tagged union (`PNode::Union`).
    Any,
    /// Some variant with every field.
    AnyVariant(Adt<'w>),
    /// Always (a union, an empty array).
    Yes,
    /// The layout record's own word (a leaf).
    Record,
}

/// The and-or evaluation of inhabitedness: a post-order walk on a work stack, each type
/// decided once (`state`, by the type's ground index).
fn inhabited<A: Allocator + Clone>(ty: PTy, layouts: &mut Layouts<A>, cx: &mut LayoutCx<'_, '_>) -> Result<bool, LayoutError> {
    let facts = cx.facts;
    let mut state: Vec<u8, A> = Vec::new_in(layouts.alloc());
    let mut stack: Vec<PTy, A> = Vec::new_in(layouts.alloc());
    let mut parts: Vec<PTy, A> = Vec::new_in(layouts.alloc());
    stack.push(ty);
    while let Some(&t) = stack.last() {
        let at = t.0 as usize;
        if state.len() <= at {
            state.resize(at + 1, UNSEEN);
        }
        if state[at] == YES || state[at] == NO {
            stack.pop();
            continue;
        }
        let id = layouts.of(t, cx)?;
        let l = *layouts.get(id);
        let form = match node_of(t, cx)? {
            PNode::Adt(key, _) => {
                let adt = facts.adt(key).ok_or(LayoutError { ty: t, why: Refusal::NoAdt(key) })?;
                match adt.kind {
                    AdtKind::Union => Form::Yes,
                    AdtKind::Struct => Form::All,
                    AdtKind::Enum => Form::AnyVariant(adt),
                }
            }
            PNode::Array(..) => match l.fields {
                Fields::Array { count: 0, .. } => Form::Yes,
                _ => Form::All,
            },
            PNode::Tuple(_) | PNode::Record(..) | PNode::Closure { .. } | PNode::Coroutine { .. } | PNode::Pat(..) => Form::All,
            PNode::Union(_) => Form::Any,
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Ref(..)
            | PNode::Ptr(..)
            | PNode::Slice(_)
            | PNode::FnDef(..)
            | PNode::FnPtr(..)
            | PNode::Dynamic(_) => Form::Record,
            _ => return Err(LayoutError { ty: t, why: Refusal::Malformed("a validity assertion's type the layout lays out but no rule names") }),
        };
        parts.clear();
        if matches!(form, Form::All | Form::Any | Form::AnyVariant(_)) {
            layouts.field_types(t, cx, &mut parts)?;
        }
        if state[at] == UNSEEN {
            state[at] = OPEN;
            let before = stack.len();
            for &p in parts.iter() {
                let pi = p.0 as usize;
                if state.len() <= pi {
                    state.resize(pi + 1, UNSEEN);
                }
                match state[pi] {
                    UNSEEN => stack.push(p),
                    OPEN => return Err(LayoutError { ty: p, why: Refusal::Recursive }),
                    _ => {}
                }
            }
            if stack.len() > before {
                continue;
            }
        }
        let all_yes = |fs: &[PTy]| fs.iter().all(|p| state[p.0 as usize] == YES);
        let yes = match form {
            Form::All => all_yes(&parts),
            Form::Any => parts.iter().any(|p| usize::try_from(p.0).ok().and_then(|i| state.get(i)) == Some(&YES)),
            Form::Yes => true,
            Form::Record => !l.uninhabited,
            Form::AnyVariant(adt) => {
                let mut any = false;
                for v in adt.variants {
                    let fs = parts.get(v.fields.range()).ok_or(LayoutError { ty: t, why: Refusal::Malformed("a variant's fields past their column") })?;
                    any |= all_yes(fs);
                }
                any
            }
        };
        state[at] = if yes { YES } else { NO };
        stack.pop();
    }
    Ok(state.get(ty.0 as usize) == Some(&YES))
}

#[cfg(test)]
mod tests;
