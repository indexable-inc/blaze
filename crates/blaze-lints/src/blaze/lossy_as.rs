//! `lossy_as` (in `blaze::edition`, allowed by default): an `as` cast that can lose value or
//! sign, or goes between a pointer and an integer, which the blaze edition removes
//! (docs/v2/plan/design/edition.md). One lint and one message shape in place of clippy's
//! `cast_possible_truncation`, `cast_sign_loss`, `cast_possible_wrap` and
//! `cast_precision_loss`, which stay available by their own names; the type reasoning of what
//! a cast can lose is here, once.
//!
//! The types are the inferred ones of the operand and the cast, aliases resolved. A cast can
//! lose when the target holds fewer of the source's values: an integer to a narrower one, a
//! signed integer to an unsigned one, an unsigned integer to a signed one with no more value
//! bits, a float to an integer, an integer with more value bits than a float's mantissa to the
//! float (`u64` to `f64`, `u32` to `f32`), a float to a narrower float, and a `char` to an
//! integer narrower than its 21 bits. `usize` and `isize` are taken as 32 to 64 bits wide, as
//! clippy takes them: `u32 as usize` is lossless, `u64 as usize` and `usize as u32` are not. A
//! raw pointer, a function pointer or a function item cast to an integer, and an integer cast
//! to a pointer, are found whatever the widths: they drop or invent provenance.
//!
//! Not found: a cast between a type and itself; a cast that can't lose (`u8 as u32`,
//! `bool as u8`, `u8 as char`, `f32 as f64`, pointer to pointer, a fieldless enum to its
//! integer); an operand whose value is known and fits the target, an integer literal (`-1 as
//! i64`, `255 as u8`) or a `const` with no generics that evaluates (`LEN as u32`, in a const
//! context or not); a cast a macro wrote; where `implicit_overflow` is on, a cast that can
//! lose an integer's value (to a narrower integer, between signed and unsigned, a float to an
//! integer), which that lint judges with the prover. There is no fix: each loss has its own conversion,
//! which the note names.

use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Expr, ExprId, Literal, UnaryOp},
    resolver::ValueNs,
};
use hir_ty::{
    consteval::{allocation_as_isize, allocation_as_usize},
    db::HirDatabase as _,
    next_solver::{GenericArgs, Ty, TyKind},
};
use rustc_type_ir::{FloatTy, IntTy, UintTy, inherent::IntoKind as _};

use super::{Finding, Lint, facts, general_facts};
use crate::check::Cx;

// The lint is `as_cast` now; this module stays for what `implicit_overflow` reads of it.
const LINT: Lint = Lint::AsCast;

/// The bits of a `char`'s largest value, `char::MAX`.
const CHAR_BITS: u64 = 21;

/// How wide an integer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Width {
    Fixed(u64),
    /// `usize` or `isize`: the target's pointer width, 32 to 64 bits.
    Pointer,
}

impl Width {
    /// The fewest bits it can be.
    fn least(self) -> u64 {
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

/// What a cast's operand or target is, as far as what a cast loses goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Int { signed: bool, width: Width },
    /// A float of this many bits.
    Float(u64),
    Bool,
    Char,
    /// A raw pointer, a function pointer or a function item.
    Pointer,
}

impl Kind {
    pub(super) fn of(ty: Ty<'_>) -> Option<Kind> {
        Some(match ty.kind() {
            TyKind::Int(it) => Kind::Int { signed: true, width: int_width(it) },
            TyKind::Uint(it) => Kind::Int { signed: false, width: uint_width(it) },
            TyKind::Float(it) => Kind::Float(float_bits(it)),
            TyKind::Bool => Kind::Bool,
            TyKind::Char => Kind::Char,
            TyKind::RawPtr(..) | TyKind::FnPtr(..) | TyKind::FnDef(..) => Kind::Pointer,
            _ => return None,
        })
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
fn mantissa(bits: u64) -> u64 {
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
    /// A signed integer to an unsigned one; `same` when they are as wide, `narrower` when the
    /// target can be narrower.
    Sign { same: bool, narrower: bool },
    /// An unsigned integer to a signed one with no more value bits; `same` when as wide.
    Wrap { same: bool },
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
    let same_width = |a: Width, b: Width| a == b;
    match (from, to) {
        (Kind::Pointer, Kind::Int { .. }) => Some(Loss::Address),
        (Kind::Int { .. }, Kind::Pointer) => Some(Loss::Provenance),
        (Kind::Int { signed: true, width: a }, Kind::Int { signed: false, width: b }) => {
            Some(Loss::Sign { same: same_width(a, b), narrower: b.least() < a.most() })
        }
        (Kind::Int { signed: false, width: a }, Kind::Int { signed: true, width: b }) => {
            if b.least() > a.most() {
                None
            } else if b.least() < a.most() && !same_width(a, b) {
                Some(Loss::HighBits)
            } else {
                Some(Loss::Wrap { same: same_width(a, b) })
            }
        }
        (Kind::Int { width: a, .. }, Kind::Int { width: b, .. }) => {
            (b.least() < a.most()).then_some(Loss::HighBits)
        }
        (Kind::Int { signed, width }, Kind::Float(bits)) => {
            (width.most() - u64::from(signed) > mantissa(bits)).then_some(Loss::Rounds)
        }
        (Kind::Float(_), Kind::Int { .. }) => Some(Loss::Fraction),
        (Kind::Float(a), Kind::Float(b)) => (b < a).then_some(Loss::Narrows),
        (Kind::Char, Kind::Int { signed, width }) => {
            (width.least() - u64::from(signed) < CHAR_BITS).then_some(Loss::CodePoint)
        }
        _ => None,
    }
}

impl Loss {
    /// Whether it is a loss of an integer's value, which `implicit_overflow` judges where it
    /// is on: a cast to a narrower integer, between signed and unsigned, or of a float to an
    /// integer.
    pub(super) fn of_value(self) -> bool {
        matches!(self, Loss::HighBits | Loss::Sign { .. } | Loss::Wrap { .. } | Loss::Fraction)
    }

    /// What the cast does to a value it loses, after "which": `to` the target as shown.
    fn said(self, to: &str) -> String {
        // alloc: a finding's message part
        match self {
            Loss::HighBits => format!("drops the high bits of a value above `{to}::MAX`"),
            Loss::Sign { narrower: false, .. } => {
                "turns a negative value into a large positive one".to_owned()
            }
            Loss::Sign { narrower: true, .. } => {
                format!("turns a negative value positive and drops the high bits of one above `{to}::MAX`")
            }
            Loss::Wrap { .. } => format!("turns a value above `{to}::MAX` negative"),
            Loss::Fraction => format!(
                "drops the fraction, saturates a value outside `{to}`'s range and turns NaN into 0"
            ),
            Loss::Rounds => format!("rounds an integer too wide for `{to}`'s significand"),
            Loss::Narrows => {
                format!("rounds to the nearest `{to}` and turns a value beyond `{to}::MAX` into infinity")
            }
            Loss::CodePoint => format!("keeps only the low bits of a code point above `{to}::MAX`"),
            Loss::Address => {
                "keeps the address and drops the pointer's provenance".to_owned()
            }
            Loss::Provenance => "makes a pointer with no provenance".to_owned(),
        }
    }

    /// What to write instead: `to` the target as shown, `to_kind` what it is.
    fn instead(self, to: &str, to_kind: Kind) -> String {
        // alloc: a finding's note
        match self {
            Loss::HighBits | Loss::Sign { same: false, .. } | Loss::Wrap { same: false } => {
                format!("`{to}::try_from(x)` returns an error for a value that doesn't fit")
            }
            Loss::Sign { same: true, .. } => format!(
                "`x.cast_unsigned()` when the bits are meant, `{to}::try_from(x)` when the value is"
            ),
            Loss::Wrap { same: true } => format!(
                "`x.cast_signed()` when the bits are meant, `{to}::try_from(x)` when the value is"
            ),
            Loss::Fraction => format!(
                "round as meant (`trunc`, `round`, `floor`, `ceil`) and check the value against `{to}::MIN` and \
                 `{to}::MAX` first: std has no checked conversion from a float to an integer"
            ),
            Loss::Rounds => {
                let held = match to_kind {
                    Kind::Float(16) => "`u8` or `i8`",
                    Kind::Float(32) => "`u16`, `i16` or narrower",
                    Kind::Float(64) => "`u32`, `i32` or narrower",
                    _ => "`u64`, `i64` or narrower",
                };
                format!(
                    "`{to}::from(x)` takes an integer it holds exactly ({held}); a wider one rounds, so say \
                     where the rounding is meant"
                )
            }
            Loss::Narrows => {
                "keep the wider float, or say where the rounding is meant".to_owned()
            }
            Loss::CodePoint => {
                format!("`{to}::try_from(u32::from(c))` returns an error for a code point that doesn't fit")
            }
            Loss::Address => {
                "`ptr.addr()` for the address alone, `ptr.expose_provenance()` when the integer becomes a \
                 pointer again"
                    .to_owned()
            }
            Loss::Provenance => {
                "`ptr.with_addr(addr)` from a pointer into the same allocation, or \
                 `core::ptr::with_exposed_provenance(addr)` for an address `expose_provenance` gave"
                    .to_owned()
            }
        }
    }
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let file = analysis.tree().1;
    let target = cx.krate().to_display_target(db);
    // `implicit_overflow` judges the casts that can lose an integer's value, with the prover.
    let yields = cx.on_blaze(Lint::ImplicitOverflow);
    for (id, expr) in body.exprs() {
        let Expr::Cast { expr: operand, .. } = expr else { continue };
        let (from_ty, to_ty) = (infer.expr_ty(operand), infer.expr_ty(id));
        if from_ty == to_ty {
            continue;
        }
        let (Some(from), Some(to)) = (Kind::of(from_ty), Kind::of(to_ty)) else { continue };
        let Some(lost) = loss(from, to) else { continue };
        if yields && lost.of_value() {
            continue;
        }
        if known(cx, def, body, operand, from).is_some_and(|value| fits(value, to)) {
            continue;
        }
        if !analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let from_shown = analysis.ty(from_ty).display(db, target).to_string();
        let to_shown = analysis.ty(to_ty).display(db, target).to_string();
        let message = format!(
            "this `as` in {} casts `{from_shown}` to `{to_shown}`, which {}",
            super::body_item(db, analysis),
            lost.said(&to_shown)
        );
        facts::push(&mut found, LINT, range, message, Some(lost.instead(&to_shown, to)));
    }
    found
}

/// The value of the integer `operand`, of kind `from`, when it is known without running the
/// code: an integer literal, maybe negated, or a `const` with no generics that evaluates.
pub(super) fn known(
    cx: &Cx<'_, '_>,
    def: DefWithBodyId,
    body: &Body,
    operand: ExprId,
    from: Kind,
) -> Option<i128> {
    let Kind::Int { signed, .. } = from else { return None };
    match body.expr(operand) {
        Expr::Literal(literal) => literal_value(literal),
        Expr::UnaryOp { expr, op: UnaryOp::Neg } => match body.expr(expr) {
            Expr::Literal(literal) => literal_value(literal)?.checked_neg(),
            _ => None,
        },
        Expr::Path(_) => {
            let db = cx.db();
            let ValueNs::ConstId(konst) = general_facts::value_of(db, def, body, operand)? else {
                return None;
            };
            if hir_ty::generics::generics(db, konst.into()).iter(true).next().is_some() {
                return None;
            }
            let value = db.const_eval(konst, GenericArgs::empty(), None).ok()?;
            match signed {
                true => Some(allocation_as_isize(value)),
                false => i128::try_from(allocation_as_usize(value)).ok(),
            }
        }
        _ => None,
    }
}

/// The value of an integer literal, if `i128` holds it.
fn literal_value(literal: &Literal) -> Option<i128> {
    match *literal {
        Literal::Int(value, _) => Some(value),
        Literal::Uint(value, _) => i128::try_from(value).ok(),
        _ => None,
    }
}

/// Whether `value` survives a cast to `to`, on every pointer width.
pub(super) fn fits(value: i128, to: Kind) -> bool {
    match to {
        Kind::Int { signed, width } => {
            let bits = width.least();
            if bits >= 128 {
                return signed || value >= 0;
            }
            match signed {
                true => {
                    let half = 1i128 << (bits - 1);
                    (-half..half).contains(&value)
                }
                false => (0..(1i128 << bits)).contains(&value),
            }
        }
        Kind::Float(bits) => value.unsigned_abs() <= 1u128 << mantissa(bits),
        Kind::Bool | Kind::Char | Kind::Pointer => false,
    }
}
