//! A ground type's name as text: the one printer of `core::intrinsics::type_name` (evaluated by
//! blaze-consteval in core's `const { intrinsics::type_name::<T>() }`) and of the messages of
//! the validity assertions (`assert_inhabited` and its siblings, intrinsics/validity.rs).
//!
//! The pinned core, library/core/src/any.rs, `type_name`: "Returns the name of a type as a
//! string slice ... The returned string must not be considered to be a unique identifier of a
//! type ... the output may change between versions of the compiler". So the form is ours, and
//! it is the one a reader of a diagnostic expects: every item by its full path from its crate
//! (`core::option::Option<i32>`), its generic arguments in order (all of them: a defaulted
//! argument is printed too), no lifetimes; `&str`, `&mut T`, `*const T`, `[T; N]` with the
//! length's value, `[T]`, `(A, B)`, `(A,)`, `()`, `!`, `fn(A) -> R` (with `unsafe ` and
//! `extern "C" ` when the header says so; no `-> ()`), `dyn core::fmt::Debug + core::marker::Send`
//! with a projection as `<Item = u8>`, a fn item as its path and arguments, and a closure or an
//! async block as `<owner path>::{{closure}}`.
//!
//! Deterministic: the text is a function of the ground table and the names alone. No
//! recursion: one work stack of pieces, each node pushing its pieces in reverse. Bounded: the
//! text stops at [`MAX_TEXT`] bytes with [`TypeNameError::TooLong`] (a type of shared parts can
//! print exponentially long).

use core::alloc::Allocator;
use core::fmt::{self, Write as _};

use blaze_own::String;
use blaze_typeck::{FloatTy, IntTy, ItemKey, Mutbl, Name, PNode, PTy, PTys, UintTy};

/// The longest name printed: 1 MiB.
pub const MAX_TEXT: usize = 1 << 20;

/// What the printer reads besides the ground table: the paths of items and the text of names.
pub trait TypeNames {
    /// Gives `item`'s full path (`core::option::Option`, `my_crate::main`) to `out` and says
    /// `true`; `false` when the item has no path this world can give (`out` not called).
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool;

    /// Gives the text of `name` (an associated type's, in `dyn Iterator<Item = u8>`) to `out`
    /// and says `true`. The default: `false`, and such a `dyn` type is refused naming the name.
    fn name_text(&self, name: Name, out: &mut dyn FnMut(&str)) -> bool {
        let _ = (name, out);
        false
    }
}

/// Why a type's name was not printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeNameError {
    /// The names gave no path for this item.
    NoPath { item: ItemKey },
    /// The names gave no text for this associated type's name.
    NoName { name: Name },
    /// A node a ground type does not hold (a parameter, an alias, an opaque type, a hole), or a
    /// form not printed (a pattern type, an enum-valued const argument): named.
    NotGround { ty: PTy, what: &'static str },
    /// A type or list past the ground table.
    PastTable { ty: PTy },
    /// Past [`MAX_TEXT`] bytes.
    TooLong,
}

impl TypeNameError {
    /// The kind, as a fixed phrase (for a refusal that carries only a `&'static str`).
    pub const fn what(&self) -> &'static str {
        match self {
            TypeNameError::NoPath { .. } => "a type's name: an item the names give no path for",
            TypeNameError::NoName { .. } => "a type's name: an associated type's name the names give no text for",
            TypeNameError::NotGround { what, .. } => what,
            TypeNameError::PastTable { .. } => "a type's name: a type past the ground table",
            TypeNameError::TooLong => "a type's name longer than 1 MiB",
        }
    }
}

impl fmt::Display for TypeNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeNameError::NoPath { item } => write!(f, "the name of a type: item {:#x} has no path to print", item.0),
            TypeNameError::NoName { name } => write!(f, "the name of a type: associated type name {:#x} has no text to print", name.0),
            TypeNameError::NotGround { ty, what } => write!(f, "the name of type {}: {what}", ty.0),
            TypeNameError::PastTable { ty } => write!(f, "the name of a type: type {} is past the ground table", ty.0),
            TypeNameError::TooLong => write!(f, "the name of a type: longer than {MAX_TEXT} bytes"),
        }
    }
}

impl core::error::Error for TypeNameError {}

/// One piece of the text still to write.
#[derive(Clone, Copy)]
enum Piece {
    Ty(PTy),
    Text(&'static str),
    Path(ItemKey),
    Name(Name),
}

/// `ty` (of ground table `tys`) printed as `type_name` prints it, in `alloc`.
pub fn type_name_text<A: Allocator + Copy>(tys: PTys<'_>, ty: PTy, names: &dyn TypeNames, alloc: A) -> Result<String<A>, TypeNameError> {
    let mut out = String::new_in(alloc);
    let mut stack: Vec<Piece, A> = Vec::new_in(alloc);
    // One node's pieces in reading order, pushed onto `stack` reversed.
    let mut seq: Vec<Piece, A> = Vec::new_in(alloc);
    stack.push(Piece::Ty(ty));
    while let Some(piece) = stack.pop() {
        match piece {
            Piece::Text(t) => out.push_str(t),
            Piece::Path(item) => {
                if !names.item_path(item, &mut |p| out.push_str(p)) {
                    return Err(TypeNameError::NoPath { item });
                }
            }
            Piece::Name(name) => {
                if !names.name_text(name, &mut |p| out.push_str(p)) {
                    return Err(TypeNameError::NoName { name });
                }
            }
            Piece::Ty(t) => {
                seq.clear();
                node(tys, t, &mut out, &mut seq)?;
                for &p in seq.iter().rev() {
                    stack.push(p);
                }
            }
        }
        if out.len() > MAX_TEXT {
            return Err(TypeNameError::TooLong);
        }
    }
    Ok(out)
}

/// Writes `t`'s leaves straight into `out`, or puts its pieces in `seq` in reading order.
fn node<A: Allocator>(tys: PTys<'_>, t: PTy, out: &mut String<A>, seq: &mut Vec<Piece, A>) -> Result<(), TypeNameError> {
    let past = TypeNameError::PastTable { ty: t };
    let list = |l| tys.list(l).map_err(|_| past);
    let not = |what| Err(TypeNameError::NotGround { ty: t, what });
    let text = |seq: &mut Vec<Piece, A>, s: &'static str| seq.push(Piece::Text(s));
    match *tys.get(t).ok_or(past)? {
        PNode::Bool => out.push_str("bool"),
        PNode::Char => out.push_str("char"),
        PNode::Str => out.push_str("str"),
        PNode::Never => out.push_str("!"),
        PNode::Int(i) => out.push_str(int_name(i)),
        PNode::Uint(u) => out.push_str(uint_name(u)),
        PNode::Float(f) => out.push_str(float_name(f)),
        PNode::Tuple(l) => {
            let parts = list(l)?;
            text(seq, "(");
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    text(seq, ", ");
                }
                seq.push(Piece::Ty(p));
            }
            // A one-tuple keeps its comma: `(u8,)`.
            text(seq, if parts.len() == 1 { ",)" } else { ")" });
        }
        PNode::Record(_, l) => {
            let parts = list(l)?;
            text(seq, "record(");
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    text(seq, ", ");
                }
                seq.push(Piece::Ty(p));
            }
            text(seq, ")");
        }
        PNode::Union(l) => {
            let parts = list(l)?;
            text(seq, "union(");
            for (k, &p) in parts.iter().enumerate() {
                if k > 0 {
                    text(seq, " | ");
                }
                seq.push(Piece::Ty(p));
            }
            text(seq, ")");
        }
        PNode::Ref(m, p) => {
            text(seq, if m == Mutbl::Mut { "&mut " } else { "&" });
            seq.push(Piece::Ty(p));
        }
        PNode::Ptr(m, p) => {
            text(seq, if m == Mutbl::Mut { "*mut " } else { "*const " });
            seq.push(Piece::Ty(p));
        }
        PNode::Array(e, n) => {
            text(seq, "[");
            seq.push(Piece::Ty(e));
            text(seq, "; ");
            seq.push(Piece::Ty(n));
            text(seq, "]");
        }
        PNode::Slice(e) => {
            text(seq, "[");
            seq.push(Piece::Ty(e));
            text(seq, "]");
        }
        PNode::Adt(item, l) | PNode::FnDef(item, l) => {
            seq.push(Piece::Path(item));
            args(seq, list(l)?);
        }
        PNode::Closure { owner, .. } | PNode::Coroutine { owner, .. } => {
            seq.push(Piece::Path(owner));
            text(seq, "::{{closure}}");
        }
        PNode::FnPtr(l, header) => {
            let parts = list(l)?;
            let (&output, inputs) = parts.split_last().ok_or(past)?;
            let bits = header.bits();
            if bits & 0x80 != 0 {
                text(seq, "unsafe ");
            }
            // `FnPtrHeader::bits`'s ABI numbering (blaze-solve term.rs).
            match bits & 0x3f {
                0 => {}
                1 => text(seq, "extern \"C\" "),
                2 => text(seq, "extern \"C-unwind\" "),
                3 => text(seq, "extern \"system\" "),
                4 => text(seq, "extern \"system-unwind\" "),
                5 => text(seq, "extern \"rust-call\" "),
                _ => return not("a fn pointer of an ABI the printer does not name"),
            }
            text(seq, "fn(");
            for (k, &p) in inputs.iter().enumerate() {
                if k > 0 {
                    text(seq, ", ");
                }
                seq.push(Piece::Ty(p));
            }
            // A C-variadic pointer's `...` after its named inputs, as rustc's `type_name` prints it.
            if header.c_variadic {
                text(seq, if inputs.is_empty() { "..." } else { ", ..." });
            }
            text(seq, ")");
            let unit = matches!(tys.get(output), Some(PNode::Tuple(u)) if tys.list(*u).map_or(false, |x| x.is_empty()));
            if !unit {
                text(seq, " -> ");
                seq.push(Piece::Ty(output));
            }
        }
        PNode::Dynamic(l) => {
            text(seq, "dyn ");
            let preds = list(l)?;
            // The principal and its projections share one argument list; the auto traits
            // follow, ` + ` between each (the one order `PortableTys::dynamic` makes).
            let mut first = true;
            for &p in preds {
                match *tys.get(p).ok_or(TypeNameError::PastTable { ty: p })? {
                    PNode::ExTrait(item, al) => {
                        seq.push(Piece::Path(item));
                        let a = list(al)?;
                        // Its projections are the predicates right after it.
                        let binds = preds.iter().skip(1).any(|&q| matches!(tys.get(q), Some(PNode::ExProj { .. })));
                        if !a.is_empty() || binds {
                            text(seq, "<");
                            for (k, &x) in a.iter().enumerate() {
                                if k > 0 {
                                    text(seq, ", ");
                                }
                                seq.push(Piece::Ty(x));
                            }
                            let mut k = a.len();
                            for &q in preds.iter().skip(1) {
                                if let Some(PNode::ExProj { name, term, .. }) = tys.get(q) {
                                    if k > 0 {
                                        text(seq, ", ");
                                    }
                                    seq.push(Piece::Name(*name));
                                    text(seq, " = ");
                                    seq.push(Piece::Ty(*term));
                                    k += 1;
                                }
                            }
                            text(seq, ">");
                        }
                        first = false;
                    }
                    PNode::ExProj { .. } => {
                        // Printed in the principal's list above; with no principal there is
                        // no trait to bind it to.
                        if first {
                            return not("a `dyn` type with a projection and no principal trait");
                        }
                    }
                    PNode::ExAuto(item) => {
                        if !first {
                            text(seq, " + ");
                        }
                        seq.push(Piece::Path(item));
                        first = false;
                    }
                    _ => return not("a `dyn` type's predicate that is not a trait, a projection or an auto trait"),
                }
            }
        }
        PNode::ConstValue { ty, bits } => match *tys.get(ty).ok_or(past)? {
            PNode::Bool => out.push_str(if bits != 0 { "true" } else { "false" }),
            PNode::Uint(_) => {
                let _ = write!(out, "{bits}");
            }
            PNode::Int(i) => {
                let w = int_bits(i);
                let v = if w == 128 { bits as i128 } else { ((bits << (128 - w)) as i128) >> (128 - w) };
                let _ = write!(out, "{v}");
            }
            PNode::Char => match u32::try_from(bits).ok().and_then(char::from_u32) {
                Some(c) => {
                    let _ = write!(out, "'{}'", c.escape_debug());
                }
                None => return not("a `char` const argument that is not a char"),
            },
            _ => return not("a const argument of an enum type (`adt_const_params`)"),
        },
        PNode::Pat(..) => return not("a pattern type (`T is PAT`)"),
        PNode::Param(_) => return not("a generic parameter (the type is not ground)"),
        PNode::ConstParam(_) => return not("a const generic parameter (the type is not ground)"),
        PNode::Alias { .. } => return not("a projection not normalized (the type is not ground)"),
        PNode::Opaque(..) => return not("an opaque type not revealed (the type is not ground)"),
        PNode::Future(..) => return not("a body's own async block type (not a ground type)"),
        PNode::Infer => return not("a hole `_` (not a ground type)"),
        PNode::Error => return not("the error type"),
        PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => return not("a `dyn` predicate outside its `dyn` type"),
        // An extern type is named by its path, as an ADT with no arguments.
        PNode::Foreign(item) => seq.push(Piece::Path(item)),
    }
    Ok(())
}

/// `<a, b>` after a path, nothing for no arguments.
fn args<A: Allocator>(seq: &mut Vec<Piece, A>, list: &[PTy]) {
    if list.is_empty() {
        return;
    }
    seq.push(Piece::Text("<"));
    for (k, &p) in list.iter().enumerate() {
        if k > 0 {
            seq.push(Piece::Text(", "));
        }
        seq.push(Piece::Ty(p));
    }
    seq.push(Piece::Text(">"));
}

const fn int_name(i: IntTy) -> &'static str {
    match i {
        IntTy::Isize => "isize",
        IntTy::I8 => "i8",
        IntTy::I16 => "i16",
        IntTy::I32 => "i32",
        IntTy::I64 => "i64",
        IntTy::I128 => "i128",
    }
}

/// The width of a signed integer type on the hosts in scope (64-bit `isize`).
const fn int_bits(i: IntTy) -> u32 {
    match i {
        IntTy::I8 => 8,
        IntTy::I16 => 16,
        IntTy::I32 => 32,
        IntTy::Isize | IntTy::I64 => 64,
        IntTy::I128 => 128,
    }
}

const fn uint_name(u: UintTy) -> &'static str {
    match u {
        UintTy::Usize => "usize",
        UintTy::U8 => "u8",
        UintTy::U16 => "u16",
        UintTy::U32 => "u32",
        UintTy::U64 => "u64",
        UintTy::U128 => "u128",
    }
}

const fn float_name(f: FloatTy) -> &'static str {
    match f {
        FloatTy::F16 => "f16",
        FloatTy::F32 => "f32",
        FloatTy::F64 => "f64",
        FloatTy::F128 => "f128",
    }
}

#[cfg(test)]
mod tests;
