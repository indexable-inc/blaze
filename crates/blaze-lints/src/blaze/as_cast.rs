//! `as_cast` (in `blaze::edition`, allowed by default, denied in blaze's own workspace by the
//! ratchet): every `as` written in a body. `as` is one keyword for a dozen operations, some
//! lossless, some that drop high bits, flip a sign, round, saturate or invent provenance; the
//! reader can't tell which without the types. Each finding says the two types and what the
//! cast can lose, and where the operation has a name its machine-applicable fix writes that
//! name, chosen by the inferred types, never by the text, with the same behaviour:
//!
//! - the same type: the operand alone (an unsuffixed literal gets the type's suffix, `5_u64`);
//! - an integer to a wider one: `T::from(x)`; in a const context `x.widen::<T>()`; between a
//!   pointer-sized integer and a fixed one, lossless on every 32- and 64-bit target,
//!   `blaze_cast::usize_from_u32(x)` and its siblings;
//! - an integer that can lose: the width in the source's sign (`x.truncate::<T>()`, or
//!   `blaze_cast::u32_truncated_from_usize(x)` where a pointer width is on one side), then the
//!   sign (`cast_signed`, `cast_unsigned`): `x as i32` of a `u64` is
//!   `x.truncate::<u32>().cast_signed()`. These are the sites `blaze::truncating_cast` lists:
//!   wrapping is kept, as `as` had it; a `T::try_from(x)` is a change of behaviour and never
//!   written here;
//! - `bool` and `char`: `u8::from(b)`, `u32::from(c)`, `char::from(x)`, then as integers;
//! - an integer to a float: `f64::from(x)` where it is exact, `blaze_cast::f32_rounded_from_u32`
//!   and its siblings where it rounds; a float to an integer
//!   `blaze_cast::i32_saturated_from_f64` and its siblings; `f32` to `f64` `f64::from(x)`, `f64`
//!   to `f32` `blaze_cast::f32_rounded_from_f64`;
//! - pointers: `p.cast::<U>()`, `cast_mut`, `cast_const`; a reference to a pointer
//!   `core::ptr::from_ref(r)` or `from_mut`; a pointer to an integer `p.expose_provenance()`; an
//!   integer to a pointer `core::ptr::with_exposed_provenance::<T>(a)` (`as` exposes and takes
//!   exposed provenance, so these are the same operation, not `addr` and `with_addr`);
//! - a coercion written as a cast (an unsizing, a function item to a function pointer, `&mut`
//!   to `&`): a `let` that names the type in place of the cast, or the cast dropped where the
//!   declared type of a `let`, `const` or `static` forces the coercion.
//!
//! In a const context (a `const fn`, a `const`, a `static`, an enum discriminant, an inline
//! `const` block, an array's length, a const argument), where a trait's function can't be
//! called, the fix writes the const forms: `widen` for a widening, blaze-cast's const twins of
//! `From` (`blaze_cast::f64_from_f32`, `u32_from_char`) for the rest.
//!
//! A finding with no fix says why: a fieldless enum's discriminant (std has no named operation;
//! reflection would give one, but it is blaze's own syntax and the tree must build with cargo
//! too), a function's address (`FnPtr::addr` is unstable and changing on the pinned nightly), a
//! pointer cast between unsized pointees, a coercion that is not a `let`'s or an item's whole
//! value, `f16` and `f128`, a type the front end doesn't know, and a crate that does not turn
//! on `integer_widen_truncate` or depend on `blaze-cast` where the fix needs it. A cast a
//! macro writes is not found; nor is one in a type (an array type's length), outside any body.
//!
//! One cast, one finding: where `blaze::implicit_overflow` is on and reports a cast (one that
//! can lose an integer's value, which its prover can't show keeps it), that finding is the
//! cast's and this lint says nothing there (`implicit_overflow::reported_casts`). A cast the
//! prover shows keeps its value is still found here, and its fix names the operation.
//!
//! Nested casts are fixed at once: the fix of the outermost cast with a fix holds the fixed text
//! of each cast inside it, so `--fix` settles in one pass.

use std::cell::OnceCell;

use hir::HirDisplay as _;
use hir_ty::next_solver::TyKind;
use rustc_type_ir::{FloatTy, IntTy, UintTy, inherent::IntoKind as _};
use syntax::{SyntaxKind as K, TextRange, WalkEvent, view as ast};

use ast::AstNode as _;

use super::{Lint, facts};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::AsCast;

/// The bits of a `char`'s largest value, `char::MAX`.
const CHAR_BITS: u64 = 21;

/// The lossless conversions between a pointer-sized integer and a fixed one that std has no
/// `From` for (it promises 16-bit targets), as `(from, to)`: blaze-cast's `{to}_from_{from}`.
const LOSSLESS_HELPERS: &[(&str, &str)] = &[
    ("u32", "usize"),
    ("i32", "isize"),
    ("u16", "isize"),
    ("usize", "u64"),
    ("usize", "u128"),
    ("usize", "i128"),
    ("isize", "i64"),
    ("isize", "i128"),
];

/// The same-sign narrowings between a pointer-sized integer and a fixed one that std's
/// `truncate` has no impl for, as `(from, to)`: blaze-cast's `{to}_truncated_from_{from}`.
const TRUNCATED_HELPERS: &[(&str, &str)] = &[
    ("usize", "u32"),
    ("isize", "i32"),
    ("u64", "usize"),
    ("i64", "isize"),
    ("u128", "usize"),
    ("i128", "isize"),
];

/// How wide an integer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Width {
    Fixed(u64),
    /// `usize` or `isize`: the target's pointer width, 32 to 64 bits on blaze's targets.
    Pointer,
}

impl Width {
    /// The fewest bits it can be.
    pub(super) fn least(self) -> u64 {
        match self {
            Width::Fixed(bits) => bits,
            Width::Pointer => 32,
        }
    }

    /// The most bits it can be.
    fn most(self) -> u64 {
        match self {
            Width::Fixed(bits) => bits,
            Width::Pointer => 64,
        }
    }
}

/// An integer type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Int {
    pub(super) signed: bool,
    pub(super) width: Width,
}

impl Int {
    const U8: Int = Int { signed: false, width: Width::Fixed(8) };
    const U32: Int = Int { signed: false, width: Width::Fixed(32) };
    const USIZE: Int = Int { signed: false, width: Width::Pointer };

    fn name(self) -> &'static str {
        match (self.signed, self.width) {
            (false, Width::Fixed(8)) => "u8",
            (false, Width::Fixed(16)) => "u16",
            (false, Width::Fixed(32)) => "u32",
            (false, Width::Fixed(64)) => "u64",
            (false, Width::Fixed(_)) => "u128",
            (false, Width::Pointer) => "usize",
            (true, Width::Fixed(8)) => "i8",
            (true, Width::Fixed(16)) => "i16",
            (true, Width::Fixed(32)) => "i32",
            (true, Width::Fixed(64)) => "i64",
            (true, Width::Fixed(_)) => "i128",
            (true, Width::Pointer) => "isize",
        }
    }

    /// Whether every value of `self` is a value of `to` on every target blaze supports.
    fn fits_in(self, to: Int) -> bool {
        match (self.signed, to.signed) {
            (true, false) => false,
            (false, true) => to.width.least() > self.width.most(),
            _ => to.width.least() >= self.width.most(),
        }
    }

    /// Whether std has `impl From<self> for to`.
    fn std_from(self, to: Int) -> bool {
        if self.signed && !to.signed {
            return false;
        }
        match (self.width, to.width) {
            (Width::Fixed(a), Width::Fixed(b)) => b > a,
            (Width::Fixed(a), Width::Pointer) => match self.signed == to.signed {
                true => a <= 16,
                false => a <= 8,
            },
            (Width::Pointer, _) => false,
        }
    }

    /// Whether core has `self.widen::<to>()`: the same sign, wider on every target.
    fn widens_to(self, to: Int) -> bool {
        self.signed == to.signed
            && match (self.width, to.width) {
                (Width::Fixed(a), Width::Fixed(b)) => b > a,
                (Width::Fixed(a), Width::Pointer) => a <= 16,
                (Width::Pointer, _) => false,
            }
    }

    /// Whether core has `self.truncate::<to>()`: the same sign, narrower on every target.
    fn truncates_to(self, to: Int) -> bool {
        self.signed == to.signed
            && match (self.width, to.width) {
                (Width::Fixed(a), Width::Fixed(b)) => b < a,
                (Width::Pointer, Width::Fixed(b)) => b <= 16,
                (_, Width::Pointer) => false,
            }
    }
}

/// What a cast's operand or target is: the one classification of what an `as` can lose, which
/// `implicit_overflow` reads too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Int(Int),
    /// A float of this many bits.
    Float(u64),
    Bool,
    Char,
    /// A raw pointer, `mutable` for `*mut`.
    RawPtr { mutable: bool },
    /// A reference, `mutable` for `&mut`.
    Ref { mutable: bool },
    /// A function item or a function pointer.
    Fn,
    /// An enum: a fieldless one's discriminant is what `as` gives.
    Enum,
    Other,
}

impl Kind {
    pub(super) fn of(ty: &hir::Type<'_>) -> Kind {
        match ty.to_next_solver().kind() {
            TyKind::Int(it) => Kind::Int(Int { signed: true, width: int_width(it) }),
            TyKind::Uint(it) => Kind::Int(Int { signed: false, width: uint_width(it) }),
            TyKind::Float(it) => Kind::Float(float_bits(it)),
            TyKind::Bool => Kind::Bool,
            TyKind::Char => Kind::Char,
            TyKind::RawPtr(..) => Kind::RawPtr { mutable: ty.is_mutable_raw_ptr() },
            TyKind::Ref(..) => Kind::Ref { mutable: ty.is_mutable_reference() },
            TyKind::FnPtr(..) | TyKind::FnDef(..) => Kind::Fn,
            _ => match ty.as_adt() {
                Some(hir::Adt::Enum(_)) => Kind::Enum,
                _ => Kind::Other,
            },
        }
    }
}

fn int_width(it: IntTy) -> Width {
    it.bit_width().map_or(Width::Pointer, Width::Fixed)
}

fn uint_width(it: UintTy) -> Width {
    it.bit_width().map_or(Width::Pointer, Width::Fixed)
}

fn float_bits(it: FloatTy) -> u64 {
    it.bit_width()
}

/// The bits of a float's significand, its implicit bit included: the widest integer it holds
/// exactly.
pub(super) fn mantissa(bits: u64) -> u64 {
    match bits {
        16 => 11,
        32 => 24,
        64 => 53,
        _ => 113,
    }
}

/// What a cast can lose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Loss {
    /// An integer to a narrower one of the same signedness, or an unsigned one to a narrower
    /// signed one.
    HighBits,
    /// A signed integer to an unsigned one; `narrower` when the target can be narrower.
    Sign { narrower: bool },
    /// An unsigned integer to a signed one with no more value bits.
    Wrap,
    /// A float to an integer.
    Fraction,
    /// An integer to a float whose significand holds fewer bits.
    Rounds,
    /// A float to a narrower float.
    Narrows,
    /// A `char` to an integer narrower than its 21 bits.
    CodePoint,
    /// A pointer to an integer.
    Address,
    /// An integer to a pointer.
    Provenance,
}

/// What casting a `from` to a `to` can lose; `None` when every value survives.
pub(super) fn loss(from: Kind, to: Kind) -> Option<Loss> {
    match (from, to) {
        (Kind::RawPtr { .. } | Kind::Fn, Kind::Int(_)) => Some(Loss::Address),
        (Kind::Int(_), Kind::RawPtr { .. }) => Some(Loss::Provenance),
        (Kind::Int(a), Kind::Int(b)) if a.fits_in(b) => None,
        (Kind::Int(a), Kind::Int(b)) => match (a.signed, b.signed) {
            (true, false) => Some(Loss::Sign { narrower: b.width.least() < a.width.most() }),
            (false, true) if b.width.least() < a.width.most() && a.width != b.width => {
                Some(Loss::HighBits)
            }
            (false, true) => Some(Loss::Wrap),
            _ => Some(Loss::HighBits),
        },
        (Kind::Int(a), Kind::Float(bits)) => {
            (a.width.most() - u64::from(a.signed) > mantissa(bits)).then_some(Loss::Rounds)
        }
        (Kind::Float(_), Kind::Int(_)) => Some(Loss::Fraction),
        (Kind::Float(a), Kind::Float(b)) => (b < a).then_some(Loss::Narrows),
        (Kind::Char, Kind::Int(b)) => {
            (b.width.least() - u64::from(b.signed) < CHAR_BITS).then_some(Loss::CodePoint)
        }
        _ => None,
    }
}

impl Loss {
    /// Whether it is a loss of an integer's value, which `implicit_overflow` judges with the
    /// prover where it is on: a cast to a narrower integer, between signed and unsigned, or of
    /// a float to an integer.
    pub(super) fn of_value(self) -> bool {
        matches!(self, Loss::HighBits | Loss::Sign { .. } | Loss::Wrap | Loss::Fraction)
    }

    /// What the cast does to a value it loses, after "which": `to` the target as shown.
    fn said(self, to: &str) -> String {
        // alloc: a finding's message part
        match self {
            Loss::HighBits => format!("drops the high bits of a value above `{to}::MAX`"),
            Loss::Sign { narrower: false } => {
                "turns a negative value into a large positive one".to_owned()
            }
            Loss::Sign { narrower: true } => {
                format!("turns a negative value positive and drops the high bits of one above `{to}::MAX`")
            }
            Loss::Wrap => format!("turns a value above `{to}::MAX` negative"),
            Loss::Fraction => format!(
                "drops the fraction, saturates a value outside `{to}`'s range and turns NaN into 0"
            ),
            Loss::Rounds => format!("rounds an integer too wide for `{to}`'s significand"),
            Loss::Narrows => {
                format!("rounds to the nearest `{to}` and turns a value beyond `{to}::MAX` into infinity")
            }
            Loss::CodePoint => format!("keeps only the low bits of a code point above `{to}::MAX`"),
            Loss::Address => "keeps the address and exposes the pointer's provenance".to_owned(),
            Loss::Provenance => "makes a pointer with the provenance exposed at that address".to_owned(),
        }
    }
}

/// Why a cast has no fix, said in its note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoFix {
    Unknown,
    Enum,
    FnAddress,
    UnsizedPointee,
    Coercion,
    WideFloat,
    Unwritable,
    NoFeature,
    NoBlazeCast,
    NoStd,
}

impl NoFix {
    fn said(self) -> &'static str {
        match self {
            NoFix::Unknown => "no fix: the front end doesn't know the types here",
            NoFix::Enum => {
                "no fix: std has no named operation for a fieldless enum's discriminant; match on the \
                 variants, or give the enum a `const fn` that returns its value"
            }
            NoFix::FnAddress => {
                "no fix: a function's address has no stable named operation on the pinned nightly \
                 (`FnPtr::addr` is unstable and changing)"
            }
            NoFix::UnsizedPointee => {
                "no fix: `cast` takes a sized pointee, and a cast between unsized pointees keeps the \
                 metadata in a way no named method does"
            }
            NoFix::Coercion => {
                "no fix: this cast is a coercion, written where no type is declared; name the type on \
                 a `let` (`let p: &dyn Trait = &x;`) or where the value is passed"
            }
            NoFix::WideFloat => "no fix: `f16` and `f128` have no named conversions here",
            NoFix::Unwritable => "no fix: a type here can't be written out in this module",
            NoFix::NoFeature => {
                "no fix: the fix calls core's `widen` or `truncate`, which need \
                 `#![feature(integer_widen_truncate)]` at the crate root"
            }
            NoFix::NoBlazeCast => {
                "no fix: the fix calls a function of `blaze-cast`, which this crate does not depend on \
                 (`blaze-cast.workspace = true`)"
            }
            NoFix::NoStd => {
                "no fix: a name the fix writes (`From`, a primitive type, `core`) means something else \
                 here"
            }
        }
    }
}

/// One operation the fix writes around the value: a function it is passed to, or a method
/// called on it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Call(String),
    Method(String),
}

/// What a fix may write at one cast: which of its names mean what it means there.
struct Writer<'a, 'c, 't> {
    cx: &'a Cx<'c, 't>,
    at: ast::Node<'t>,
    module: hir_def::ModuleId,
    /// In a const context: the const forms only.
    konst: bool,
    from_trait: OnceCell<bool>,
    blaze_cast: OnceCell<bool>,
    core: OnceCell<bool>,
}

impl<'c, 't> Writer<'_, 'c, 't> {
    /// Whether `name` is the primitive type of that name at the cast.
    fn builtin(&self, name: &str) -> bool {
        matches!(
            self.cx.resolve_name(self.at, name),
            Some(hir::PathResolution::Def(hir::ModuleDef::BuiltinType(_)))
        )
    }

    /// `{to}::from`, when `From` is core's and `to` the primitive at the cast.
    fn from_call(&self, to: &str) -> Result<Step, NoFix> {
        let db = self.cx.db();
        let from = *self.from_trait.get_or_init(|| match self.cx.resolve_name(self.at, "From") {
            Some(hir::PathResolution::Def(hir::ModuleDef::Trait(it))) => {
                facts::crate_is(db, it.module(db).krate(db), "core")
            }
            _ => false,
        });
        match from && self.builtin(to) {
            true => Ok(Step::Call(format!("{to}::from"))),
            false => Err(NoFix::NoStd),
        }
    }

    /// `blaze_cast::{name}`, when `blaze_cast` is the crate's dependency at the cast.
    fn helper(&self, name: &str) -> Result<Step, NoFix> {
        match *self.blaze_cast.get_or_init(|| self.crate_root("blaze_cast")) {
            true => Ok(Step::Call(format!("blaze_cast::{name}"))),
            false => Err(NoFix::NoBlazeCast),
        }
    }

    /// `core::ptr::{name}`, when `core` is core at the cast.
    fn ptr_fn(&self, name: &str) -> Result<Step, NoFix> {
        match *self.core.get_or_init(|| self.crate_root("core")) {
            true => Ok(Step::Call(format!("core::ptr::{name}"))),
            false => Err(NoFix::NoStd),
        }
    }

    fn crate_root(&self, name: &str) -> bool {
        let db = self.cx.db();
        match self.cx.resolve_name(self.at, name) {
            Some(hir::PathResolution::Def(hir::ModuleDef::Module(it))) => {
                it.is_crate_root(db) && facts::crate_is(db, it.krate(db), name)
            }
            _ => false,
        }
    }

    fn widen_truncate(&self) -> Result<(), NoFix> {
        match self.cx.features().is_enabled(&intern::Symbol::intern("integer_widen_truncate")) {
            true => Ok(()),
            false => Err(NoFix::NoFeature),
        }
    }

    /// `ty` as source code in the cast's module.
    fn written(&self, ty: &hir::Type<'c>) -> Result<String, NoFix> {
        let shown =
            ty.display_source_code(self.cx.db(), self.module, false).map_err(|_| NoFix::Unwritable)?;
        let bare = shown.replace("'static", "");
        if ty.contains_unknown() || bare.contains('\'') || bare.contains('{') || bare.contains("impl ") {
            return Err(NoFix::Unwritable);
        }
        Ok(shown)
    }

    /// The steps from the integer `from` to the integer `to`.
    fn int(&self, from: Int, to: Int, steps: &mut Vec<Step>) -> Result<(), NoFix> {
        if from == to {
            return Ok(());
        }
        if from.fits_in(to) {
            return self.lossless(from, to, steps);
        }
        let mid = Int { signed: from.signed, width: to.width };
        if mid != from {
            match from.fits_in(mid) {
                true => self.lossless(from, mid, steps)?,
                false => self.truncate(from, mid, steps)?,
            }
        }
        if mid.signed != to.signed {
            let sign = if to.signed { "cast_signed" } else { "cast_unsigned" };
            steps.push(Step::Method(sign.to_owned()));
        }
        Ok(())
    }

    /// The steps of a widening every value of `from` survives.
    fn lossless(&self, from: Int, to: Int, steps: &mut Vec<Step>) -> Result<(), NoFix> {
        let (f, t) = (from.name(), to.name());
        if !self.konst && from.std_from(to) {
            steps.push(self.from_call(t)?);
            return Ok(());
        }
        let helper = LOSSLESS_HELPERS.contains(&(f, t));
        if self.konst {
            if from.widens_to(to) {
                self.widen_truncate()?;
                steps.push(Step::Method(format!("widen::<{t}>")));
                return Ok(());
            }
            let unsigned = Int { signed: false, width: to.width };
            if !from.signed && to.signed && from.widens_to(unsigned) {
                self.widen_truncate()?;
                steps.push(Step::Method(format!("widen::<{}>", unsigned.name())));
                steps.push(Step::Method("cast_signed".to_owned()));
                return Ok(());
            }
        }
        match helper {
            true => {
                steps.push(self.helper(&format!("{t}_from_{f}"))?);
                Ok(())
            }
            false => Err(NoFix::NoStd),
        }
    }

    /// The steps of a same-sign narrowing that keeps the low bits.
    fn truncate(&self, from: Int, to: Int, steps: &mut Vec<Step>) -> Result<(), NoFix> {
        let (f, t) = (from.name(), to.name());
        if from.truncates_to(to) {
            self.widen_truncate()?;
            steps.push(Step::Method(format!("truncate::<{t}>")));
            return Ok(());
        }
        match TRUNCATED_HELPERS.contains(&(f, t)) {
            true => {
                steps.push(self.helper(&format!("{t}_truncated_from_{f}"))?);
                Ok(())
            }
            false => Err(NoFix::NoStd),
        }
    }

    /// The steps of the cast of a `from_ty` to a `to_ty`, which differ.
    fn steps(
        &self,
        (from, from_ty): (Kind, &hir::Type<'c>),
        (to, to_ty): (Kind, &hir::Type<'c>),
    ) -> Result<Vec<Step>, NoFix> {
        // alloc: one fix's steps
        let mut steps = Vec::new();
        match (from, to) {
            (Kind::Int(f), Kind::Int(t)) => self.int(f, t, &mut steps)?,
            (Kind::Bool, Kind::Int(t)) => match self.konst {
                false => steps.push(self.from_call(t.name())?),
                true => {
                    steps.push(self.helper("u8_from_bool")?);
                    self.int(Int::U8, t, &mut steps)?;
                }
            },
            (Kind::Char, Kind::Int(t)) => {
                let wide = !t.signed && matches!(t.width, Width::Fixed(32 | 64 | 128));
                match (self.konst, wide) {
                    (false, true) => steps.push(self.from_call(t.name())?),
                    (false, false) => {
                        steps.push(self.from_call("u32")?);
                        self.int(Int::U32, t, &mut steps)?;
                    }
                    (true, _) => {
                        steps.push(self.helper("u32_from_char")?);
                        self.int(Int::U32, t, &mut steps)?;
                    }
                }
            }
            (Kind::Int(f), Kind::Char) if f == Int::U8 => match self.konst {
                false => steps.push(self.from_call("char")?),
                true => steps.push(self.helper("char_from_u8")?),
            },
            (Kind::Int(f), Kind::Float(bits)) => {
                let t = float_name(bits)?;
                let exact = f.width.most() - u64::from(f.signed) <= mantissa(bits)
                    && f.width != Width::Pointer;
                match (exact, self.konst) {
                    (true, false) => steps.push(self.from_call(t)?),
                    (true, true) => steps.push(self.helper(&format!("{t}_from_{}", f.name()))?),
                    (false, _) => steps.push(self.helper(&format!("{t}_rounded_from_{}", f.name()))?),
                }
            }
            (Kind::Float(bits), Kind::Int(t)) => {
                let f = float_name(bits)?;
                steps.push(self.helper(&format!("{}_saturated_from_{f}", t.name()))?);
            }
            (Kind::Float(a), Kind::Float(b)) => {
                let (f, t) = (float_name(a)?, float_name(b)?);
                match (b > a, self.konst) {
                    (true, false) => steps.push(self.from_call(t)?),
                    (true, true) => steps.push(self.helper(&format!("{t}_from_{f}"))?),
                    (false, _) => steps.push(self.helper(&format!("{t}_rounded_from_{f}"))?),
                }
            }
            (Kind::RawPtr { mutable }, Kind::RawPtr { .. }) => {
                self.pointer(mutable, from_ty.remove_raw_ptr(), to_ty, &mut steps)?;
            }
            (Kind::Ref { mutable }, Kind::RawPtr { .. }) => {
                let made = if mutable { "from_mut" } else { "from_ref" };
                steps.push(self.ptr_fn(made)?);
                self.pointer(mutable, from_ty.as_reference().map(|it| it.0), to_ty, &mut steps)?;
            }
            (Kind::RawPtr { .. }, Kind::Int(t)) => {
                steps.push(Step::Method("expose_provenance".to_owned()));
                self.int(Int::USIZE, t, &mut steps)?;
            }
            (Kind::Int(f), Kind::RawPtr { mutable }) => {
                let pointee = to_ty.remove_raw_ptr().ok_or(NoFix::Unknown)?;
                let pointee = self.written(&pointee)?;
                self.int(f, Int::USIZE, &mut steps)?;
                let made = if mutable { "with_exposed_provenance_mut" } else { "with_exposed_provenance" };
                let Step::Call(path) = self.ptr_fn(made)? else { return Err(NoFix::NoStd) };
                steps.push(Step::Call(format!("{path}::<{pointee}>")));
            }
            (Kind::Fn, Kind::Int(_) | Kind::RawPtr { .. }) => return Err(NoFix::FnAddress),
            (Kind::Enum, Kind::Int(_)) => return Err(NoFix::Enum),
            (Kind::Float(16 | 128), _) | (_, Kind::Float(16 | 128)) => return Err(NoFix::WideFloat),
            _ => return Err(NoFix::Coercion),
        }
        Ok(steps)
    }

    /// The steps from a raw pointer to `from` (`mutable` for `*mut`) to the raw pointer type
    /// `to_ty`.
    fn pointer(
        &self,
        mutable: bool,
        from: Option<hir::Type<'c>>,
        to_ty: &hir::Type<'c>,
        steps: &mut Vec<Step>,
    ) -> Result<(), NoFix> {
        let db = self.cx.db();
        let (from, to) = (from.ok_or(NoFix::Unknown)?, to_ty.remove_raw_ptr().ok_or(NoFix::Unknown)?);
        if from.to_next_solver() != to.to_next_solver() {
            match (from.is_sized(db), to.is_sized(db)) {
                (_, true) => steps.push(Step::Method(format!("cast::<{}>", self.written(&to)?))),
                (true, false) => return Err(NoFix::Coercion),
                (false, false) => return Err(NoFix::UnsizedPointee),
            }
        }
        match (mutable, to_ty.is_mutable_raw_ptr()) {
            (false, true) => steps.push(Step::Method("cast_mut".to_owned())),
            (true, false) => steps.push(Step::Method("cast_const".to_owned())),
            _ => {}
        }
        Ok(())
    }
}

fn float_name(bits: u64) -> Result<&'static str, NoFix> {
    match bits {
        32 => Ok("f32"),
        64 => Ok("f64"),
        _ => Err(NoFix::WideFloat),
    }
}

/// One cast's fix: the text that replaces `range`, and whether it can take a method call as
/// it stands; `used` once a fix around it holds it.
struct Rewrite {
    range: TextRange,
    text: String,
    postfix: bool,
    /// More parts: the type a `let` names in place of a coercion.
    extra: Option<(TextRange, String)>,
    used: bool,
}

/// One cast, as found.
struct Found<'t> {
    node: ast::Node<'t>,
    range: TextRange,
    message: String,
    note: String,
    /// The index of its rewrite in the body's list.
    fix: Option<usize>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let (found, rewrites) = find(cx);
    for it in found {
        let held = it.fix.is_some_and(|at| rewrites[at].used);
        let parts = it.fix.map(|at| &rewrites[at]).filter(|it| !it.used).map(|it| {
            let mut parts = vec![(cx.span(it.range), it.text.clone())]; // alloc: the fix's parts
            if let Some((at, text)) = &it.extra {
                parts.insert(0, (cx.span(*at), text.clone()));
            }
            parts
        });
        let note = match held {
            true => "the fix of the cast around this one writes this one too".to_owned(),
            false => it.note,
        };
        let span = cx.span(it.range);
        let diagnostic = cx.blaze(LINT, it.node, span, it.message).note(note);
        if let Some(parts) = parts {
            diagnostic.multipart_suggestion(
                "write the cast as the operation it is",
                parts,
                Applicability::MachineApplicable,
            );
        }
        diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// Every cast of the walked body, innermost first, with the rewrites of those that have one.
fn find<'t>(cx: &Cx<'_, 't>) -> (Vec<Found<'t>>, Vec<Rewrite>) {
    // alloc: one body's findings and rewrites, reported at the edge
    let (mut found, mut rewrites): (Vec<Found<'t>>, Vec<Rewrite>) = (Vec::new(), Vec::new());
    let Some(analysis) = cx.analysis() else { return (found, rewrites) };
    let db = cx.db();
    let module = hir_def::ModuleId::from(analysis.def().module(db));
    let target = cx.krate().to_display_target(db);
    let item = super::body_item(db, analysis);
    let root = cx.root();
    // One cast, one finding: where `implicit_overflow` is on and reports a cast (one that can
    // lose an integer's value and the prover can't show keeps it), the finding is its, with
    // its `try_from` fix; this lint gives way and plans no rewrite there.
    let given_way = super::implicit_overflow::reported_casts(cx);
    // alloc: the casts of one body, in the order written
    let mut casts = Vec::new();
    let mut events = root.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != root && ast::Item::cast(node).is_some() {
            events.skip_subtree();
            continue;
        }
        if let Some(cast) = ast::CastExpr::cast(node) {
            casts.push(cast);
        }
    }
    // Innermost first: a cast's operand holds only casts written after it.
    for cast in casts.into_iter().rev() {
        let node = cast.syntax();
        let (Some(operand), Some(range)) = (cast.expr(), cx.range(node)) else { continue };
        if given_way.contains(&range) {
            continue;
        }
        let (Some(from_ty), Some(to_ty)) = (cx.ty(operand), cx.ty(ast::Expr::CastExpr(cast))) else {
            // Not an expression of this body: an array type's length, a signature's.
            continue;
        };
        let from_shown = from_ty.display(db, target).to_string();
        let to_shown = to_ty.display(db, target).to_string();
        let (from, to) = (Kind::of(&from_ty), Kind::of(&to_ty));
        let lost = loss(from, to);
        let message = match lost {
            Some(lost) => format!(
                "this `as` in {item} casts `{from_shown}` to `{to_shown}`, which {}",
                lost.said(&to_shown)
            ),
            None => format!("this `as` in {item} casts `{from_shown}` to `{to_shown}`"),
        };
        let writer = Writer {
            cx,
            at: node,
            module,
            konst: in_const(root, node),
            from_trait: OnceCell::new(),
            blaze_cast: OnceCell::new(),
            core: OnceCell::new(),
        };
        let same = from_ty.to_next_solver() == to_ty.to_next_solver();
        let planned = match (from_ty.contains_unknown() || to_ty.contains_unknown(), same) {
            (true, _) => Err(NoFix::Unknown),
            (false, true) => Ok(Vec::new()),
            (false, false) => writer.steps((from, &from_ty), (to, &to_ty)),
        };
        let planned = match planned {
            Err(NoFix::Coercion) => coercion(cx, &writer, cast, &to_ty).map(Planned::Coercion),
            Err(it) => Err(it),
            Ok(steps) => Ok(Planned::Steps(steps)),
        };
        let (fix, note) = match planned
            .and_then(|it| rewrite(cx, cast, operand, from, it, &mut rewrites))
        {
            Ok(at) => {
                let note = match truncates(&rewrites[at].text) {
                    true => "the fix keeps what `as` does, wrapping included; where a value that doesn't fit \
                             is a bug, convert with `T::try_from(x)` and a typed error by hand \
                             (`blaze::truncating_cast` lists these)",
                    false => "the fix names the operation `as` does here, with the same behaviour",
                };
                (Some(at), note.to_owned())
            }
            Err(why) => (None, why.said().to_owned()),
        };
        found.push(Found { node, range, message, note, fix });
    }
    (found, rewrites)
}

/// Whether a fix's text keeps the low bits or the bits of another sign of a value that may not
/// fit.
fn truncates(text: &str) -> bool {
    ["truncate::<", "cast_signed()", "cast_unsigned()", "_truncated_from_"].iter().any(|it| text.contains(it))
}

/// What a fix writes: steps around the operand, or a coercion's rewrite.
enum Planned {
    Steps(Vec<Step>),
    Coercion(Coerce),
}

/// How a coercion written as a cast is written without it.
enum Coerce {
    /// The operand alone: a declared type forces the coercion.
    Drop,
    /// The operand alone, with the type named after the `let`'s pattern, at this offset.
    Let(TextRange, String),
}

/// A coercion's rewrite: the cast dropped where the `let`, `const` or `static` it is the whole
/// value of declares its type, or the type moved onto a `let` with an untyped name.
fn coercion<'c, 't>(
    cx: &Cx<'c, 't>,
    writer: &Writer<'_, 'c, 't>,
    cast: ast::CastExpr<'t>,
    to_ty: &hir::Type<'c>,
) -> Result<Coerce, NoFix> {
    let node = cast.syntax();
    let parent = node.parent().ok_or(NoFix::Coercion)?;
    if let Some(stmt) = ast::LetStmt::cast(parent)
        && stmt.initializer().map(|it| it.syntax()) == Some(node)
    {
        if let Some(ty) = stmt.ty() {
            let inferred = ty.syntax().descendants().any(|it| it.kind() == K::INFER_TYPE);
            return if inferred { Err(NoFix::Coercion) } else { Ok(Coerce::Drop) };
        }
        let Some(ast::Pat::IdentPat(pat)) = stmt.pat() else { return Err(NoFix::Coercion) };
        if pat.pat().is_some() {
            return Err(NoFix::Coercion);
        }
        let end = cx.range(pat.syntax()).ok_or(NoFix::Coercion)?.end();
        return Ok(Coerce::Let(TextRange::empty(end), format!(": {}", writer.written(to_ty)?)));
    }
    if matches!(parent.kind(), K::CONST | K::STATIC) {
        return Ok(Coerce::Drop);
    }
    Err(NoFix::Coercion)
}

/// Records the rewrite of `cast` as planned, the fixed text of the casts inside its operand
/// spliced in, and gives its index.
fn rewrite<'t>(
    cx: &Cx<'_, 't>,
    cast: ast::CastExpr<'t>,
    operand: ast::Expr<'t>,
    from: Kind,
    planned: Planned,
    rewrites: &mut Vec<Rewrite>,
) -> Result<usize, NoFix> {
    let node = cast.syntax();
    let range = cx.range(node).ok_or(NoFix::Unwritable)?;
    let at = cx.range(operand.syntax()).ok_or(NoFix::Unwritable)?;
    let held = postfix_operand(operand, rewrites, at);
    let (text, literal) = operand_text(cx, operand, at, from, rewrites);
    let (steps, extra) = match planned {
        Planned::Steps(steps) => (steps, None),
        Planned::Coercion(Coerce::Drop) => (Vec::new(), None),
        Planned::Coercion(Coerce::Let(at, ty)) => (Vec::new(), Some((at, ty))),
    };
    let operand_postfix = held || literal;
    let postfix = operand_postfix || !steps.is_empty();
    let text = render(text, operand_postfix, &steps);
    // Parentheses around the cast go with it when what replaces it can take a method call.
    let range = match node.parent().and_then(ast::ParenExpr::cast) {
        Some(paren) if postfix && extra.is_none() => cx.range(paren.syntax()).unwrap_or(range),
        _ => range,
    };
    rewrites.push(Rewrite { range, text, postfix, extra, used: false });
    Ok(rewrites.len() - 1)
}

/// Whether the operand, as fixed, can take a method call without parentheses.
fn postfix_operand(operand: ast::Expr<'_>, rewrites: &[Rewrite], at: TextRange) -> bool {
    if let Some(it) = rewrites.iter().find(|it| !it.used && it.range == at) {
        return it.postfix;
    }
    let block_like = matches!(
        operand,
        ast::Expr::BlockExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::LoopExpr(_)
            | ast::Expr::WhileExpr(_)
            | ast::Expr::ForExpr(_)
    );
    !block_like && operand.precedence() >= ast::prec::ExprPrecedence::Postfix
}

/// The operand's text with the fixes of the casts inside it spliced in (each marked used), an
/// unsuffixed literal given its type's suffix; and whether that text is the literal.
fn operand_text(
    cx: &Cx<'_, '_>,
    operand: ast::Expr<'_>,
    at: TextRange,
    from: Kind,
    rewrites: &mut [Rewrite],
) -> (String, bool) {
    let suffix = match from {
        Kind::Int(it) => Some(it.name()),
        Kind::Float(32) => Some("f32"),
        Kind::Float(64) => Some("f64"),
        _ => None,
    };
    let literal = match operand {
        ast::Expr::Literal(it) => Some(it),
        ast::Expr::PrefixExpr(it) => match it.expr() {
            Some(ast::Expr::Literal(it)) => Some(it),
            _ => None,
        },
        _ => None,
    };
    let unsuffixed = literal.and_then(|it| match it.kind() {
        ast::LiteralKind::IntNumber(n) if n.suffix().is_none() => Some(it),
        ast::LiteralKind::FloatNumber(n) if n.suffix().is_none() => Some(it),
        _ => None,
    });
    if let (Some(literal), Some(suffix)) = (unsuffixed, suffix)
        && let Some(lit_at) = cx.range(literal.syntax())
    {
        let text = cx.snippet(at);
        let start = usize::from(lit_at.start() - at.start());
        let end = usize::from(lit_at.end() - at.start());
        let digits = &text[start..end];
        let dot = if digits.ends_with('.') { "0" } else { "" };
        let text = format!("{}{digits}{dot}_{suffix}{}", &text[..start], &text[end..]);
        return (text, matches!(operand, ast::Expr::Literal(_)));
    }
    // alloc: the operand's fixed text
    let mut text = String::new();
    let mut from_at = at.start();
    let mut inside: Vec<&mut Rewrite> =
        rewrites.iter_mut().filter(|it| !it.used && at.contains_range(it.range)).collect();
    inside.sort_by_key(|it| it.range.start());
    for it in inside {
        text.push_str(cx.snippet(TextRange::new(from_at, it.range.start())));
        text.push_str(&it.text);
        from_at = it.range.end();
        it.used = true;
    }
    text.push_str(cx.snippet(TextRange::new(from_at, at.end())));
    (text, false)
}

/// `operand` with `steps` written around it; a method on an operand that can't take one as it
/// stands (`postfix` false) gets parentheses.
fn render(operand: String, postfix: bool, steps: &[Step]) -> String {
    let mut out = operand;
    let mut postfix = postfix;
    for step in steps {
        out = match step {
            Step::Call(path) => format!("{path}({out})"),
            Step::Method(name) if postfix => format!("{out}.{name}()"),
            Step::Method(name) => format!("({out}).{name}()"),
        };
        postfix = true;
    }
    out
}

/// Whether `node` is in a const context of the body at `root`: a `const fn`, a `const`, a
/// `static`, an enum discriminant, an inline `const` block, an array repeat's length or a
/// const argument. A closure inside one counts too: the const forms are right everywhere,
/// so erring toward them is safe.
fn in_const(root: ast::Node<'_>, node: ast::Node<'_>) -> bool {
    match root.kind() {
        K::CONST | K::STATIC | K::VARIANT => return true,
        K::FN if ast::Fn::cast(root).and_then(|it| it.const_token()).is_some() => return true,
        _ => {}
    }
    let mut child = node;
    for parent in std::iter::successors(node.parent(), |it| it.parent()) {
        if parent == root {
            break;
        }
        match parent.kind() {
            K::CONST_ARG | K::ARRAY_TYPE => return true,
            K::BLOCK_EXPR if ast::BlockExpr::cast(parent).and_then(|it| it.const_token()).is_some() => {
                return true;
            }
            K::ARRAY_EXPR => {
                if let Some(array) = ast::ArrayExpr::cast(parent)
                    && let ast::ArrayExprKind::Repeat { repeat: Some(repeat), .. } = array.kind()
                    && repeat.syntax() == child
                {
                    return true;
                }
            }
            _ => {}
        }
        child = parent;
    }
    false
}

#[cfg(test)]
#[path = "as_cast_tests.rs"]
mod tests;
