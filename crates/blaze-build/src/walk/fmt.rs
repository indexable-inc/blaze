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
//!   A placeholder with options is "a byte with the highest two bits set, followed by zero or
//!   more fields depending on the flags in the first byte": bit 0 "flags present" (a 32-bit
//!   field), bit 1 "width present" (16-bit), bit 2 "precision present" (16-bit), bit 3
//!   "arg_index present" (16-bit), bits 4 and 5 width and precision "indirect" (read from an
//!   argument), the fields in that order, little-endian; `fmt::write` reads them when
//!   `n > 0xC0`. The flags field is `FormattingOptions::flags`: bits 0..21 the fill char, 21
//!   `+`, 22 `-`, 23 `#`, 24 `0`, 25 `x?`, 26 `X?`, 27 a width is set, 28 a precision is set,
//!   29..31 the alignment (0 `<`, 1 `>`, 2 `^`, 3 not set); `FormattingOptions::new()` is
//!   `' ' as u32 | 3 << 29` (read from nightly-2026-10-27's rust-src, library/core/src/fmt/
//!   mod.rs, the `pins/toolchain` pin).
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
//! argument's reference, so every placeholder formats the next one and none writes an
//! `arg_index`: `0xC0` with default options, else its spec's flags, width and precision
//! (`parse_spec`, `encode_placeholder`) as rustc's lowering writes them. Then
//! `Arguments` written field by field as `new`'s body writes it (its two transmutes are the
//! identity on these bytes): `new` is not called because its instance needs the const
//! arguments `N` and `M`, which the ground table's call seam does not make here; the fields
//! are found by NAME in the layout, never by a guessed order. A template without
//! placeholders is the string literal representation, as `from_str` makes it.
//!
//! A width or precision read from an argument (`{:w$}`, `{:1$}`, `{:.*}`; std::fmt, "Width",
//! "Precision") is a `FmtTrait::Count` use: its `usize` argument's `rt::Argument::from_usize`
//! goes in the array after every value's, and the placeholder's width or precision field holds
//! that entry's index with its "indirect" bit set, as `fmt::write` reads it (`opt.width =
//! args.add(opt.width as usize).as_ref().as_u16()`); so the values still format in order with
//! no `arg_index`. `from_usize` itself panics past `u16::MAX` ("Formatting argument out of
//! range").
//!
//! Refused by name, never guessed: a spec rustc refuses
//! (a width or precision past `u16::MAX`, an unknown type, text after the type),
//! an argument of an unsized type, and a world that does not give the template
//! ([`crate::Adts::format_template`]) or the constructors ([`crate::Adts::fmt_argument_new`],
//! [`crate::Adts::fmt_argument_adt`]).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{Flags, Ty};
use blaze_typeck::{Expr, ExprId, FmtTrait, FmtUse, List, Mutbl, Name, PNode};

use super::{Repr, Shape, Task, V, Walk, malformed, unsupported};
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

/// `FormattingOptions::flags`' bits (library/core/src/fmt/mod.rs `mod flags`; module doc).
const FILL_BITS: u32 = (1 << 21) - 1;
const SIGN_PLUS: u32 = 1 << 21;
const SIGN_MINUS: u32 = 1 << 22;
const ALTERNATE: u32 = 1 << 23;
const ZERO_PAD: u32 = 1 << 24;
const DEBUG_LOWER_HEX: u32 = 1 << 25;
const DEBUG_UPPER_HEX: u32 = 1 << 26;
const WIDTH_SET: u32 = 1 << 27;
const PRECISION_SET: u32 = 1 << 28;
const ALIGN_BITS: u32 = 0b11 << 29;
const ALIGN_LEFT: u32 = 0;
const ALIGN_RIGHT: u32 = 1 << 29;
const ALIGN_CENTER: u32 = 2 << 29;
const ALIGN_UNKNOWN: u32 = 3 << 29;
/// `FormattingOptions::new()`'s flags: fill `' '`, alignment not set.
const DEFAULT_FLAGS: u32 = ' ' as u32 | ALIGN_UNKNOWN;

/// A placeholder's options as `fmt::write` reads them: the flags word (with its width-set and
/// precision-set bits), the width and the precision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Options {
    flags: u32,
    width: Option<Count>,
    precision: Option<Count>,
}

/// A width or precision (std::fmt, "Syntax": `count := parameter | integer`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Count {
    /// Written in the spec (`{:5}`, `{:.3}`).
    Is(u16),
    /// Read from an argument (`{:w$}`, `{:1$}`, `{:.*}`): a `usize` the front end lists as a
    /// `FmtTrait::Count` use, passed as an `rt::Argument::from_usize`; the template holds its
    /// index in the `args` array with the "indirect" bit set (module doc).
    Arg,
}

/// Why a format spec has no placeholder encoding here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpecError {
    /// A width past `u16::MAX`.
    Width,
    /// A `.` without digits after it, or a precision past `u16::MAX`.
    Precision,
    /// A type other than `?`, `x?`, `X?`, `x`, `X`, `o`, `b`, `e`, `E`, `p` or none.
    Type,
}

impl SpecError {
    pub(crate) const fn what(self) -> &'static str {
        match self {
            SpecError::Width => "a format width past `u16::MAX` (a spec rustc refuses)",
            SpecError::Precision => "a format precision without digits or past `u16::MAX` (a spec rustc refuses)",
            SpecError::Type => "a format spec whose type is not `?`, `x?`, `X?`, `x`, `X`, `o`, `b`, `e`, `E`, `p` or none (a spec rustc refuses)",
        }
    }
}

/// The options of format spec `spec` (the text after a placeholder's `:`), by the grammar of
/// core::fmt's doc ("Syntax"): `[[fill]align][sign]['#']['0'][width]['.' precision]type`.
pub(crate) fn parse_spec(spec: &str) -> Result<Options, SpecError> {
    let align_of = |c: char| match c {
        '<' => Some(ALIGN_LEFT),
        '>' => Some(ALIGN_RIGHT),
        '^' => Some(ALIGN_CENTER),
        _ => None,
    };
    let mut flags = DEFAULT_FLAGS;
    let mut rest = spec;
    let mut chars = rest.chars();
    let (first, second) = (chars.next(), chars.next());
    if let (Some(fill), Some(align)) = (first, second.and_then(align_of)) {
        flags = (flags & !(FILL_BITS | ALIGN_BITS)) | fill as u32 | align;
        rest = &rest[fill.len_utf8() + 1..];
    } else if let Some(align) = first.and_then(align_of) {
        flags = (flags & !ALIGN_BITS) | align;
        rest = &rest[1..];
    }
    if let Some(r) = rest.strip_prefix('+') {
        flags |= SIGN_PLUS;
        rest = r;
    } else if let Some(r) = rest.strip_prefix('-') {
        flags |= SIGN_MINUS;
        rest = r;
    }
    if let Some(r) = rest.strip_prefix('#') {
        flags |= ALTERNATE;
        rest = r;
    }
    // A `0` is the zero flag, unless it is the count `0$` (the width read from argument 0).
    if let Some(r) = rest.strip_prefix('0')
        && !r.starts_with('$')
    {
        flags |= ZERO_PAD;
        rest = r;
    }
    let (width, r) = count(rest, SpecError::Width)?;
    rest = r;
    if width.is_some() {
        flags |= WIDTH_SET;
    }
    let precision = match rest.strip_prefix('.') {
        Some(r) => {
            // `precision := count | '*'`: `.*` takes the count from the argument before the
            // value's (which the front end resolved into its uses).
            let (precision, r) = match r.strip_prefix('*') {
                Some(r) => (Some(Count::Arg), r),
                None => count(r, SpecError::Precision)?,
            };
            rest = r;
            if precision.is_none() {
                return Err(SpecError::Precision);
            }
            flags |= PRECISION_SET;
            precision
        }
        None => None,
    };
    match rest {
        "" | "?" | "x" | "X" | "o" | "b" | "e" | "E" | "p" => {}
        "x?" => flags |= DEBUG_LOWER_HEX,
        "X?" => flags |= DEBUG_UPPER_HEX,
        _ => return Err(SpecError::Type),
    }
    Ok(Options { flags, width, precision })
}

/// The leading ASCII digits of `s`, and the rest.
fn split_digits(s: &str) -> (&str, &str) {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    s.split_at(n)
}

/// A count at the start of `s`, if one is there, and the rest (std::fmt, "Syntax":
/// `count := parameter | integer`, `parameter := argument '$'`, `argument := integer |
/// identifier`). An identifier not followed by `$` is no count: it is the type (`{:x}`).
/// `too_big` is the error of an integer past `u16::MAX`.
fn count(s: &str, too_big: SpecError) -> Result<(Option<Count>, &str), SpecError> {
    let (digits, r) = split_digits(s);
    if !digits.is_empty() {
        if let Some(r) = r.strip_prefix('$') {
            return Ok((Some(Count::Arg), r));
        }
        return Ok((Some(Count::Is(digits.parse::<u16>().map_err(|_| too_big)?)), r));
    }
    let starts = s.chars().next().is_some_and(|c| c == '_' || c.is_alphabetic());
    let n = s.char_indices().find(|&(_, c)| !(c == '_' || c.is_alphanumeric())).map_or(s.len(), |(i, _)| i);
    if starts && let Some(r) = s[n..].strip_prefix('$') {
        return Ok((Some(Count::Arg), r));
    }
    Ok((None, s))
}

/// Appends placeholder `o` (formatting the next argument) to template `out` in the pinned
/// encoding (module doc): `0xC0` with default options, else the head byte naming the fields
/// present, then the flags, width and precision, each little-endian. A count read from an
/// argument writes the index of that argument's `rt::Argument` in the `args` array, the next
/// of `next_count` (one per count, in template order), with its "indirect" bit (4 for the
/// width, 5 for the precision).
pub(crate) fn encode_placeholder<A: Allocator>(o: Options, next_count: &mut u16, out: &mut blaze_own::Vec<u8, A>) {
    let has_flags = o.flags != DEFAULT_FLAGS;
    let indirect = |c: Option<Count>| u8::from(c == Some(Count::Arg));
    let head = PLAIN_PLACEHOLDER
        | u8::from(has_flags)
        | u8::from(o.width.is_some()) << 1
        | u8::from(o.precision.is_some()) << 2
        | indirect(o.width) << 4
        | indirect(o.precision) << 5;
    out.push(head);
    if has_flags {
        out.extend_from_slice(&o.flags.to_le_bytes());
    }
    for c in [o.width, o.precision].into_iter().flatten() {
        let field = match c {
            Count::Is(n) => n,
            Count::Arg => {
                let at = *next_count;
                *next_count += 1;
                at
            }
        };
        out.extend_from_slice(&field.to_le_bytes());
    }
}

/// How many of `o`'s width and precision are read from an argument.
const fn counts_of(o: Options) -> usize {
    matches!(o.width, Some(Count::Arg)) as usize + matches!(o.precision, Some(Count::Arg)) as usize
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// `format_args!` expression `e`: checks its template against its uses, then evaluates the
    /// arguments left to right; `Task::FormatArgs` builds the value from them.
    pub(super) fn format_args(&mut self, e: ExprId, args: List, uses: List) -> Result<(), BuildError> {
        let uses_of = self.body.fmt_uses.get(uses.range()).ok_or(malformed("a `format_args!`'s uses past `fmt_uses`"))?;
        self.fmt_template_checked(e, uses_of)?;
        let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a `format_args!`'s arguments past `expr_lists`"))?;
        self.work.push(Task::FormatArgs(e));
        for &a in list.iter().rev() {
            self.work.push(Task::Eval(a));
        }
        Ok(())
    }

    /// The template of `e`, checked against its `uses`: per placeholder, in order, a
    /// `FmtTrait::Count` use for each count it reads from an argument (its width's, then its
    /// precision's: blaze-lower's `format_args`), then its value's use; each spec one this
    /// encodes. Returns whether it has a placeholder.
    fn fmt_template_checked(&self, e: ExprId, uses: &[FmtUse]) -> Result<bool, BuildError> {
        let pieces = self
            .adts
            .format_template(self.body.owner, e)
            .ok_or(unsupported(e, "`format_args!` whose template the world does not give (`Adts::format_template`)"))?;
        let mismatch = || malformed("a `format_args!` template whose placeholders and counts are not its uses");
        let mut at = 0usize;
        let mut placeholders = 0usize;
        for p in pieces.iter() {
            if let FmtPiece::Arg { spec } = *p {
                let o = parse_spec(spec).map_err(|why| unsupported(e, why.what()))?;
                let counts = uses.get(at..at + counts_of(o)).ok_or_else(mismatch)?;
                at += counts.len();
                let value = uses.get(at).ok_or_else(mismatch)?;
                at += 1;
                if counts.iter().any(|u| u.trait_ != FmtTrait::Count) || value.trait_ == FmtTrait::Count {
                    return Err(mismatch());
                }
                placeholders += 1;
            }
        }
        if at != uses.len() {
            return Err(mismatch());
        }
        // Every index the template holds (a count's) is a 16-bit field.
        if uses.len() > usize::from(u16::MAX) {
            return Err(unsupported(e, "a `format_args!` with more than `u16::MAX` placeholders and counts"));
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
        let mut refs = blaze_own::Vec::with_capacity_in(n, self.alloc);
        refs.extend_from_slice(&self.vals[base..]);
        self.truncate_vals(base);

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
        let mut bytes = blaze_own::Vec::new_in(self.alloc);
        // The `args` array: the values' `rt::Argument`s in placeholder order, so each
        // placeholder formats "the next argument" and none writes an `arg_index`; then the
        // counts' `from_usize`s, in template order, each named by its index (`fmt_template_checked`
        // bounded every index by `u16::MAX`).
        let values = uses_of.iter().filter(|u| u.trait_ != FmtTrait::Count).count();
        let mut next_count = values as u16;
        if has_placeholders {
            for p in pieces.iter() {
                match *p {
                    FmtPiece::Text(t) => encode_text(t, &mut bytes),
                    FmtPiece::Arg { spec } => {
                        let o = parse_spec(spec).map_err(|why| unsupported(e, why.what()))?;
                        encode_placeholder(o, &mut next_count, &mut bytes);
                    }
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
            self.push_val(V::Place(at.0, at.1), None);
            return Ok(());
        }

        // The `rt::Argument` array: one per placeholder in template order, then one per count.
        let in_order = uses_of.iter().filter(|u| u.trait_ != FmtTrait::Count).chain(uses_of.iter().filter(|u| u.trait_ == FmtTrait::Count));
        let arg_adt = self.adts.fmt_argument_adt().ok_or(BuildError::NoLangFn { expr: Some(e), name: "format_argument" })?;
        let g_arg = self.ground.intern(PNode::Adt(arg_adt, List::EMPTY)).map_err(|_| malformed("the ground table is full"))?;
        let arg_repr = self.repr(g_arg).map_err(|error| Self::ty_err(e, error))?;
        let (stride, align) = self.size_align(arg_repr);
        let count = u32::try_from(uses_of.len()).map_err(|_| malformed("more `format_args!` placeholders than fit"))?;
        let total = stride.checked_mul(count).ok_or(unsupported(e, "a `format_args!` argument array past 4 GiB"))?;
        let slot = self.b.slot(total, align, SlotKind::Temp);
        let arr = self.b.slot_addr(slot);
        for (i, u) in in_order.enumerate() {
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
            // `new_display::<T>(x: &T)` and the other traits' are generic; `from_usize(x:
            // &usize)` is not.
            let targs = match u.trait_ {
                FmtTrait::Count => List::EMPTY,
                _ => self.ground.intern_list(&[t]).map_err(|_| malformed("the ground table is full"))?,
            };
            let pass = self.pass(g_ref, ref_repr, abi, Some(item))?;
            let rpass = self.pass(g_arg, arg_repr, abi, Some(item))?;
            self.push_val(r, None);
            self.call_instance(Some(e), Instance { item, args: targs }, &[ref_repr], &[pass], None, arg_repr, rpass)?;
            let made = self.pop()?;
            let off = i32::try_from(stride * i as u32).map_err(|_| malformed("an argument array offset past 2 GiB"))?;
            self.store(made, arg_repr, (arr, off))?;
        }
        self.b.store(Flags::NONE, arr, at.0, at.1 + args_off, MemAttrs::aligned(3));
        self.push_val(V::Place(at.0, at.1), None);
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

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::{SpecError, encode_placeholder, parse_spec};

    fn encoded(spec: &str) -> Result<blaze_own::Vec<u8, Global>, SpecError> {
        let mut out = blaze_own::Vec::new_in(Global);
        // Counts read from arguments take array entries from 1 on (one value before them).
        let mut next_count = 1;
        encode_placeholder(parse_spec(spec)?, &mut next_count, &mut out);
        Ok(out)
    }

    /// Each spec's placeholder bytes as rustc's lowering writes them and `fmt::write` reads
    /// them (module doc). `{:#018x}` (core's io `decode_repr`): head `0xC3` (flags and width
    /// present), flags `' ' | # | 0 | width-set | align unset` = `0x6980_0020`, width 18. A
    /// build that drops the width, writes the flags without the width-set bit, or orders the
    /// fields otherwise fails it; it prints nothing on a pass.
    #[test]
    fn placeholder_options_are_encoded_as_the_pinned_library_reads_them() {
        assert_eq!(encoded("").expect("encodes").as_slice(), &[0xC0]);
        assert_eq!(encoded("?").expect("encodes").as_slice(), &[0xC0]);
        assert_eq!(encoded("#018x").expect("encodes").as_slice(), &[0xC3, 0x20, 0x00, 0x80, 0x69, 18, 0]);
        // Fill `*`, centred, width 10, precision 3: `0x2A | 2 << 29 | 1 << 27 | 1 << 28`.
        assert_eq!(encoded("*^10.3").expect("encodes").as_slice(), &[0xC7, 0x2A, 0x00, 0x00, 0x58, 10, 0, 3, 0]);
        // `+` alone: flags only.
        assert_eq!(encoded("+").expect("encodes").as_slice(), &[0xC1, 0x20, 0x00, 0x20, 0x60]);
        // `x?`: the debug-lower-hex bit, 1 << 25.
        assert_eq!(encoded("x?").expect("encodes").as_slice(), &[0xC1, 0x20, 0x00, 0x00, 0x62]);
        // A width of 300 takes both bytes.
        assert_eq!(encoded("<300").expect("encodes").as_slice(), &[0xC3, 0x20, 0x00, 0x00, 0x08, 0x2C, 0x01]);
        // Counts read from arguments (std::fmt, "Width", "Precision"): head bit 4 (width
        // indirect) or 5 (precision indirect), the field the array index (1, then 2).
        // `{:1$}`: flags ' ' | width-set | align unset = 0x6800_0020, width at entry 1.
        assert_eq!(encoded("1$").expect("encodes").as_slice(), &[0xD3, 0x20, 0x00, 0x00, 0x68, 1, 0]);
        // `{:w$}` and `{:0$}` (a count, not the zero flag) encode alike.
        assert_eq!(encoded("w$").expect("encodes").as_slice(), &[0xD3, 0x20, 0x00, 0x00, 0x68, 1, 0]);
        assert_eq!(encoded("0$").expect("encodes").as_slice(), &[0xD3, 0x20, 0x00, 0x00, 0x68, 1, 0]);
        // `{:.*}`: flags ' ' | precision-set | align unset = 0x7000_0020.
        assert_eq!(encoded(".*").expect("encodes").as_slice(), &[0xE5, 0x20, 0x00, 0x00, 0x70, 1, 0]);
        // `{:>w$.p$x}`: both indirect, entries 1 and 2; `0x20 | 1 << 29 | 1 << 27 | 1 << 28`.
        assert_eq!(encoded(">w$.p$x").expect("encodes").as_slice(), &[0xF7, 0x20, 0x00, 0x00, 0x38, 1, 0, 2, 0]);
        // `{:08.3}`: the zero flag stays the flag before a plain width.
        assert_eq!(encoded("08.3").expect("encodes").as_slice(), &[0xC7, 0x20, 0x00, 0x00, 0x79, 8, 0, 3, 0]);
        // An identifier without `$` is the type, not a count.
        assert_eq!(encoded("x").expect("encodes").as_slice(), &[0xC0]);
        assert_eq!(encoded("70000").err(), Some(SpecError::Width));
        assert_eq!(encoded("5.").err(), Some(SpecError::Precision));
        assert_eq!(encoded("5z").err(), Some(SpecError::Type));
    }
}
