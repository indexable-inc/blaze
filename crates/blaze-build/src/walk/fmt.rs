//! `format_args!` (`Expr::FormatArgs`): a `core::fmt::Arguments` built in a temporary, from
//! the template's bytes as read-only data and an array of `rt::Argument`s each made by a call
//! of the library's constructor for its placeholder's trait.
//!
//! The PINNED library (the toolchain shelf, library/core/src/fmt/; these forms change between
//! nightlies: `new_v1`, `new_const`, `new_v1_formatted` and `rt::Placeholder` are gone):
//! - mod.rs: `#[lang = "format_arguments"] pub struct Arguments<'a> { template: NonNull<u8>,
//!   args: NonNull<rt::Argument<'a>> }`, and
//!   `pub unsafe fn new<const N: usize, const M: usize>(template: &'a [u8; N], args: &'a
//!   [rt::Argument<'a>; M]) -> Arguments<'a>`, whose body is
//!   `Arguments { template: mem::transmute(template), args: mem::transmute(args) }`;
//!   `pub const fn from_str(s: &'static str) -> Arguments<'a>` (and `from_str_nonconst`) for a
//!   template without placeholders.
//! - mod.rs's "Internal representation": (1) "String literal representation": `template` the
//!   start of a static `str`, `args` "`len << 1 | 1`" (`as_str` returns it); (2) "Placeholders
//!   representation": `template` a byte sequence, `args` the array's address (its low bit 0
//!   by `Argument`'s `#[repr(align(2))]`). The bytes: a literal piece of up to 127 bytes is
//!   its length byte then its bytes; up to `u16::MAX` bytes, `0x80`, its length as a 16-bit
//!   little-endian, then its bytes ("Longer pieces are split into multiple pieces of max
//!   u16::MAX bytes (at utf-8 boundaries)"); "a fully default placeholder without any options
//!   is just a single byte: `0b11000000`", which formats "the next argument in the `args`
//!   array ... (starting at 0)"; "A single zero byte marks the end of the template".
//!   `fmt::write` reads exactly this (`n == 0`, `n < 0x80`, `n == 0x80`, `n == 0xC0`).
//! - rt.rs: `#[lang = "format_argument"] #[repr(align(2))] pub struct Argument<'a> { ty:
//!   ArgumentType<'a> }`, and in `impl Argument<'_>`:
//!   `pub const fn new_display<T: Display>(x: &T) -> Argument<'_>`, likewise `new_debug`
//!   (`Debug`), `new_octal`, `new_lower_hex`, `new_upper_hex`, `new_pointer`, `new_binary`,
//!   `new_lower_exp`, `new_upper_exp`; `pub const fn from_usize(x: &usize) -> Argument<'_>`
//!   for a count.
//!
//! Built: each argument evaluated once, left to right, with the shared borrow the checker
//! records on it (blaze-typeck rules/async_fmt.rs `fmt_uses_of`): a place argument is
//! borrowed where it is, a value one is spilled, and its temporary lives to the end of the
//! enclosing statement (walk/temps.rs). Then one `Argument` per placeholder, in template order
//! (an argument used twice is two of them), each the constructor's call at `[T]` with the
//! argument's reference, so every placeholder is the plain `0xC0` (the next one). Then
//! `Arguments` written field by field as `new`'s body writes it (its two transmutes are the
//! identity on these bytes): `new` is not called because its instance needs the const
//! arguments `N` and `M`, which the ground table's call seam does not make here; the fields
//! are found by NAME in the layout, never by a guessed order. A template without
//! placeholders is the string literal representation, as `from_str` makes it.
//!
//! Refused by name, never guessed: a placeholder with formatting options (fill, alignment,
//! sign, `#`, `0`, width, precision: the template's flags, width and precision fields), a
//! width or precision read from an argument (`FmtTrait::Count`, `rt::Argument::from_usize`),
//! an argument of an unsized type, and a world that does not give the template
//! ([`crate::Adts::format_template`]) or the constructors ([`crate::Adts::fmt_argument_new`],
//! [`crate::Adts::fmt_argument_adt`]).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::{RefKind, SlotKind};
use blaze_lir::{Flags, Ty};
use blaze_typeck::{Expr, ExprId, FmtTrait, FmtUse, List, Mutbl, Name, PNode};

use super::{Callee, Repr, Shape, Task, V, Walk, malformed, unsupported};
use crate::{Abi, BuildError, FmtPiece, Instance};

/// A literal piece's longest length in the one-byte form.
const SHORT_MAX: usize = 0x7F;
/// The longest literal piece; a longer one is split at char boundaries.
const PIECE_MAX: usize = u16::MAX as usize;
/// The byte of a placeholder with default options formatting the next argument.
const PLAIN_PLACEHOLDER: u8 = 0xC0;

/// The pinned `rt::Argument` constructor for `t`, by name (the refusal names it).
pub(crate) const fn argument_new_name(t: FmtTrait) -> &'static str {
    match t {
        FmtTrait::Display => "new_display",
        FmtTrait::Debug => "new_debug",
        FmtTrait::LowerHex => "new_lower_hex",
        FmtTrait::UpperHex => "new_upper_hex",
        FmtTrait::Octal => "new_octal",
        FmtTrait::Binary => "new_binary",
        FmtTrait::LowerExp => "new_lower_exp",
        FmtTrait::UpperExp => "new_upper_exp",
        FmtTrait::Pointer => "new_pointer",
        FmtTrait::Count => "from_usize",
    }
}

/// Appends literal `text` to template `out` in the pinned encoding (module doc), split into
/// pieces of at most `u16::MAX` bytes at char boundaries. An empty text adds nothing (a zero
/// length byte would end the template).
pub(crate) fn encode_text<A: Allocator>(text: &str, out: &mut blaze_own::Vec<u8, A>) {
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(PIECE_MAX);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (piece, tail) = rest.split_at(cut);
        if piece.len() <= SHORT_MAX {
            out.push(piece.len() as u8);
        } else {
            out.push(0x80);
            out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
        }
        out.extend_from_slice(piece.as_bytes());
        rest = tail;
    }
}

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// `format_args!` expression `e`: checks its template against its uses, then evaluates the
    /// arguments left to right; `Task::FormatArgs` builds the value from them.
    pub(super) fn format_args(&mut self, e: ExprId, args: List, uses: List) -> Result<(), BuildError> {
        let uses_of = self.body.fmt_uses.get(uses.range()).ok_or(malformed("a `format_args!`'s uses past `fmt_uses`"))?;
        if uses_of.iter().any(|u| u.trait_ == FmtTrait::Count) {
            return Err(unsupported(e, "a format placeholder whose width or precision is read from an argument (`rt::Argument::from_usize`)"));
        }
        self.fmt_template_checked(e, uses_of)?;
        let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a `format_args!`'s arguments past `expr_lists`"))?;
        self.work.push(Task::FormatArgs(e));
        for &a in list.iter().rev() {
            self.work.push(Task::Eval(a));
        }
        Ok(())
    }

    /// The template of `e`, checked against its `uses`: one placeholder per use, in order,
    /// each without formatting options. Returns whether it has a placeholder.
    fn fmt_template_checked(&self, e: ExprId, uses: &[FmtUse]) -> Result<bool, BuildError> {
        let pieces = self
            .adts
            .format_template(self.body.owner, e)
            .ok_or(unsupported(e, "`format_args!` whose template the world does not give (`Adts::format_template`)"))?;
        let mut placeholders = 0usize;
        for p in pieces.iter() {
            if let FmtPiece::Arg { plain } = *p {
                if !plain {
                    return Err(unsupported(e, "a format placeholder with formatting options (fill, alignment, sign, `#`, `0`, width or precision)"));
                }
                placeholders += 1;
            }
        }
        if placeholders != uses.len() {
            return Err(malformed("a `format_args!` template whose placeholders are not its uses"));
        }
        Ok(placeholders > 0)
    }

    /// `Task::FormatArgs(e)`: the arguments' references are the top `n` values; builds the
    /// `Arguments` in a temporary and pushes it.
    pub(super) fn format_args_done(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::FormatArgs { args, uses } = self.expr(e)? else {
            return Err(malformed("a `format_args!` task on another expression"));
        };
        let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a `format_args!`'s arguments past `expr_lists`"))?;
        let uses_of = self.body.fmt_uses.get(uses.range()).ok_or(malformed("a `format_args!`'s uses past `fmt_uses`"))?;
        let has_placeholders = self.fmt_template_checked(e, uses_of)?;
        let n = list.len();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("a `format_args!`'s arguments missing from the value stack (a build bug)"))?;
        let mut refs = blaze_own::Vec::with_capacity_in(n, self.alloc.clone());
        refs.extend_from_slice(&self.vals[base..]);
        self.vals.truncate(base);

        // `Arguments`: two pointer fields, found by name.
        let repr = self.expr_repr(e)?;
        let Repr::Agg(l) = repr else {
            return Err(unsupported(e, "a `format_args!` whose `Arguments` type is not a struct of two pointers"));
        };
        let template_off = self.named_field(l, "template").ok_or(unsupported(e, "a `core::fmt::Arguments` without its `template` field"))?;
        let args_off = self.named_field(l, "args").ok_or(unsupported(e, "a `core::fmt::Arguments` without its `args` field"))?;
        let at = self.temp(repr);

        // The template's bytes as read-only data.
        let pieces = self.adts.format_template(self.body.owner, e).ok_or(malformed("a `format_args!` template that went away"))?;
        let mut bytes = blaze_own::Vec::new_in(self.alloc.clone());
        if has_placeholders {
            for p in pieces.iter() {
                match *p {
                    FmtPiece::Text(t) => encode_text(t, &mut bytes),
                    FmtPiece::Arg { .. } => bytes.push(PLAIN_PLACEHOLDER),
                }
            }
            bytes.push(0);
        } else {
            for p in pieces.iter() {
                if let FmtPiece::Text(t) = *p {
                    bytes.extend_from_slice(t.as_bytes());
                }
            }
        }
        let data = self.b.data(&bytes, 0, &[]);
        let tp = self.b.data_addr(data);
        self.b.store(Flags::NONE, tp, at.0, at.1 + template_off, MemAttrs::aligned(3));

        if !has_placeholders {
            // "String literal representation": `args` holds `len << 1 | 1`.
            let tagged = ((bytes.len() as u128) << 1) | 1;
            let x = self.b.iconst(Ty::I64, tagged);
            self.b.store(Flags::NONE, x, at.0, at.1 + args_off, MemAttrs::aligned(3));
            self.vals.push(V::Place(at.0, at.1));
            return Ok(());
        }

        // The `rt::Argument` array, one per placeholder in template order.
        let arg_adt = self.adts.fmt_argument_adt().ok_or(BuildError::NoLangFn { expr: Some(e), name: "format_argument" })?;
        let g_arg = self.ground.intern(PNode::Adt(arg_adt, List::EMPTY)).map_err(|_| malformed("the ground table is full"))?;
        let arg_repr = self.repr(g_arg).map_err(|error| Self::ty_err(e, error))?;
        let (stride, align) = self.size_align(arg_repr);
        let count = u32::try_from(uses_of.len()).map_err(|_| malformed("more `format_args!` placeholders than fit"))?;
        let total = stride.checked_mul(count).ok_or(unsupported(e, "a `format_args!` argument array past 4 GiB"))?;
        let slot = self.b.slot(total, align, SlotKind::Temp);
        let arr = self.b.slot_addr(slot);
        for (i, u) in uses_of.iter().enumerate() {
            let a = *list.get(u.arg as usize).ok_or(malformed("a format use past its arguments"))?;
            let r = refs[u.arg as usize];
            let rt = self.adjusted_ty(a)?;
            let g_ref = self.ground_ty(rt).map_err(|error| Self::ty_err(a, error))?;
            let ref_repr = self.repr(g_ref).map_err(|error| Self::ty_err(a, error))?;
            if ref_repr != Repr::Ptr {
                return Err(unsupported(a, "a `format_args!` argument of an unsized type (its reference is wide)"));
            }
            let PNode::Ref(Mutbl::Not, t) = self.ground_node(g_ref).map_err(|error| Self::ty_err(a, error))? else {
                return Err(malformed("a `format_args!` argument without its shared borrow"));
            };
            let name = argument_new_name(u.trait_);
            let item = self.adts.fmt_argument_new(u.trait_).ok_or(BuildError::NoLangFn { expr: Some(e), name })?;
            let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
            if abi != Abi::Rust {
                return Err(unsupported(e, "an `rt::Argument` constructor not of the Rust ABI"));
            }
            let targs = self.ground.intern_list(&[t]).map_err(|_| malformed("the ground table is full"))?;
            let pass = self.pass(g_ref, ref_repr, abi, Some(item))?;
            let rpass = self.pass(g_arg, arg_repr, abi, Some(item))?;
            let place = self.ref_place(Instance { item, args: targs });
            let reference = self.b.reference(RefKind::Func, place);
            self.vals.push(r);
            self.emit_call(Callee::Direct(reference), &[ref_repr], &[pass], arg_repr, rpass)?;
            let made = self.pop()?;
            let off = i32::try_from(stride * i as u32).map_err(|_| malformed("an argument array offset past 2 GiB"))?;
            self.store(made, arg_repr, (arr, off))?;
        }
        self.b.store(Flags::NONE, arr, at.0, at.1 + args_off, MemAttrs::aligned(3));
        self.vals.push(V::Place(at.0, at.1));
        Ok(())
    }

    /// The offset of the field named `name` of struct layout `l`.
    fn named_field(&self, l: u32, name: &str) -> Option<i32> {
        let Shape::Fields { first, len } = self.layouts[l as usize].shape else { return None };
        let want = Name::of(name);
        self.layout_fields[first as usize..(first + len) as usize]
            .iter()
            .find(|f| f.0 == want)
            .and_then(|f| i32::try_from(f.1).ok())
    }
}
