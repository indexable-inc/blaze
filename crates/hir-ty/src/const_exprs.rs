//! Generic const expressions (`#![feature(blaze_const_exprs)]`,
//! docs/v2/plan/design/ownership.md): a const generic argument, an array length or a `where
//! const` condition that is an expression over generic parameters, `Shares<T, K, { N + M }, D>`.
//!
//! Such an argument is an anonymous constant with its owner's generic arguments
//! ([`crate::consteval::anon_const_is_dependent`]). Two of them are one constant:
//!
//! - when their arguments are concrete, by evaluation: the solver normalizes each to its value
//!   (`SolverContext::evaluate_const`), and an evaluation that overflows or panics is the
//!   error of that instantiation;
//! - when they name generic parameters, by their normal form, which is this module:
//!   [`same_expression`]. Nothing is guessed: an expression with no normal form equals only
//!   itself (the same anonymous constant with the same arguments).
//!
//! # The normal form
//!
//! An integer expression is a polynomial over atoms with integer coefficients: the sum of
//! its monomials, each a coefficient times a sorted product of atoms, like monomials merged
//! and zero ones dropped. An atom is a constant the form does not look into: a generic
//! parameter (substituted by the argument it is used with), a named constant, another
//! anonymous constant. `+`, `-` and `*` and integer literals build polynomials, so `N + M`
//! is `M + N`, `2 * (N + 1)` is `N + N + 2`, and `N + 0` is `N`. Any other operation
//! (`/`, `%`, shifts, a call, a cast) has no normal form.
//!
//! A `bool` expression is `true`, `false`, a comparison, `!e`, or `&&`/`||` of them. A
//! comparison is its difference against zero: `a == b` is `a - b == 0` with the sign that
//! makes the first coefficient positive; `a < b` is `a - b < 0`, `a > b` is `b - a < 0`, and
//! `<=`, `>=` likewise against `<= 0`; `a != b` is `!(a == b)`. `&&` and `||` are flattened,
//! sorted and deduplicated.
//!
//! The arithmetic is over the integers, not the parameter's type, so the form says two
//! expressions agree wherever both evaluate. Where one overflows the other may not: that
//! instantiation is an error when its constant is evaluated, never two different values taken
//! for one.

use std::collections::BTreeMap;

use base_db::Db;
use hir_def::{
    expr_store::ExpressionStore,
    hir::{ArithOp, BinaryOp, CmpOp, Expr, ExprId, Literal, LogicOp, Ordering, UnaryOp},
    resolver::HasResolver,
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

use crate::{
    db::AnonConstId,
    next_solver::{AliasConstKind, Const, ConstKind, GenericArgs, TyKind, default_types},
};

/// A product of atoms, each its index in the comparison's atom table, sorted.
type Monomial = Vec<u32>;

/// A polynomial: each monomial's coefficient, none zero.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Poly(BTreeMap<Monomial, i128>);

impl Poly {
    pub fn constant(value: i128) -> Poly {
        let mut poly = Poly::default();
        poly.add_term(Vec::new(), value); // alloc: the empty monomial, allocates nothing
        poly
    }

    pub fn atom(atom: u32) -> Poly {
        let mut poly = Poly::default();
        poly.add_term(vec![atom], 1); // alloc: one monomial of a normal form
        poly
    }

    fn add_term(&mut self, monomial: Monomial, coefficient: i128) -> Option<()> {
        if coefficient == 0 {
            return Some(());
        }
        let slot = self.0.entry(monomial).or_insert(0);
        *slot = slot.checked_add(coefficient)?;
        if *slot == 0 {
            self.0.retain(|_, coefficient| *coefficient != 0);
        }
        Some(())
    }

    /// `self + other`; `None` when a coefficient leaves `i128`.
    pub fn add(mut self, other: &Poly) -> Option<Poly> {
        for (monomial, &coefficient) in &other.0 {
            self.add_term(monomial.clone(), coefficient)?; // clone: a monomial of a normal form
        }
        Some(self)
    }

    /// `-self`.
    pub fn neg(mut self) -> Option<Poly> {
        for coefficient in self.0.values_mut() {
            *coefficient = coefficient.checked_neg()?;
        }
        Some(self)
    }

    /// `self * other`.
    pub fn mul(&self, other: &Poly) -> Option<Poly> {
        let mut product = Poly::default();
        for (a, &x) in &self.0 {
            for (b, &y) in &other.0 {
                let mut monomial = a.clone(); // clone: a monomial of a normal form
                monomial.extend_from_slice(b);
                monomial.sort_unstable();
                product.add_term(monomial, x.checked_mul(y)?)?;
            }
        }
        Some(product)
    }

    /// The polynomial's value when it has no atom.
    pub fn as_constant(&self) -> Option<i128> {
        match self.0.len() {
            0 => Some(0),
            1 => self.0.get(&Vec::new()).copied(), // alloc: the empty monomial, allocates nothing
            _ => None,
        }
    }
}

/// How a polynomial is compared with zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Against {
    /// `p == 0`.
    Zero,
    /// `p < 0`.
    Negative,
    /// `p <= 0`.
    NotPositive,
}

/// A normal form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Norm {
    Int(Poly),
    Bool(bool),
    Compare(Against, Poly),
    Not(Box<Norm>),
    /// Every one of them, sorted, no two alike, none of them an `All`.
    All(Vec<Norm>),
    /// Any one of them, likewise.
    Any(Vec<Norm>),
}

impl Norm {
    /// `p` compared `against` zero, folded when `p` is a number.
    pub fn compare(against: Against, poly: Poly) -> Option<Norm> {
        if let Some(value) = poly.as_constant() {
            return Some(Norm::Bool(match against {
                Against::Zero => value == 0,
                Against::Negative => value < 0,
                Against::NotPositive => value <= 0,
            }));
        }
        // `p == 0` and `-p == 0` are one condition: the first coefficient is made positive.
        let poly = match (against, poly.0.values().next()) {
            (Against::Zero, Some(&first)) if first < 0 => poly.neg()?,
            _ => poly,
        };
        Some(Norm::Compare(against, poly))
    }

    pub fn not(self) -> Norm {
        match self {
            Norm::Bool(value) => Norm::Bool(!value),
            Norm::Not(inner) => *inner,
            other => Norm::Not(Box::new(other)), // alloc: a node of a normal form
        }
    }

    /// `self && other` (`all`) or `self || other`.
    pub fn join(self, other: Norm, all: bool) -> Norm {
        let mut parts = Vec::new(); // alloc: the operands of a normal form
        for side in [self, other] {
            match (side, all) {
                // `true && x` is `x`; `false || x` is `x`.
                (Norm::Bool(value), _) if value == all => {}
                // `false && x` is `false`; `true || x` is `true`.
                (Norm::Bool(value), _) => return Norm::Bool(value),
                (Norm::All(inner), true) | (Norm::Any(inner), false) => parts.extend(inner),
                (side, _) => parts.push(side),
            }
        }
        parts.sort();
        parts.dedup();
        match parts.len() {
            0 => Norm::Bool(all),
            1 => parts.pop().expect("one part"),
            _ if all => Norm::All(parts),
            _ => Norm::Any(parts),
        }
    }
}

/// The atoms of one comparison: both sides' normal forms index one table, so two forms are
/// equal exactly when they are the same expression of the same constants.
#[derive(Default)]
pub struct Atoms<'db>(Vec<Const<'db>>);

impl<'db> Atoms<'db> {
    fn of(&mut self, atom: Const<'db>) -> u32 {
        let at = self.0.iter().position(|&it| it == atom).unwrap_or_else(|| {
            self.0.push(atom);
            self.0.len() - 1
        });
        at as u32
    }
}

/// Whether the constants `a` and `b` have one normal form (the module's doc). `false` when
/// either has none: then only identity makes them equal, which the caller has checked.
pub fn same_expression<'db>(db: &'db Db, a: Const<'db>, b: Const<'db>) -> bool {
    // Nothing to normalize unless one side is an expression: the common case of two plain
    // constants costs two matches.
    let is_expression = |it: Const<'db>| {
        matches!(it.kind(), ConstKind::Alias(_, alias) if matches!(alias.kind, AliasConstKind::Anon { .. }))
    };
    if !is_expression(a) && !is_expression(b) {
        return false;
    }
    let mut atoms = Atoms::default();
    match (norm_of_const(db, &mut atoms, a), norm_of_const(db, &mut atoms, b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The normal form of `ct`: a number's, an anonymous constant's expression's with its
/// arguments, or the constant as one atom.
fn norm_of_const<'db>(db: &'db Db, atoms: &mut Atoms<'db>, ct: Const<'db>) -> Option<Norm> {
    match ct.kind() {
        ConstKind::Value(value) => {
            let leaf = value.value.inner().to_leaf();
            if value.ty == default_types().types.bool {
                return Some(Norm::Bool(leaf.to_uint_unchecked() != 0));
            }
            // A number that is an integer of at most 64 bits is itself; anything else is an
            // atom, equal only to the same value.
            match u64::try_from(leaf.to_uint_unchecked()) {
                Ok(number) if matches!(value.ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) => {
                    Some(Norm::Int(Poly::constant(i128::from(number))))
                }
                _ => Some(Norm::Int(Poly::atom(atoms.of(ct)))),
            }
        }
        ConstKind::Alias(_, alias) => match alias.kind {
            AliasConstKind::Anon { def_id } => {
                norm_of_anon(db, atoms, def_id.0, alias.args).or_else(|| {
                    // No normal form: the constant itself, with its arguments.
                    Some(Norm::Int(Poly::atom(atoms.of(ct))))
                })
            }
            _ => Some(Norm::Int(Poly::atom(atoms.of(ct)))),
        },
        ConstKind::Param(_) | ConstKind::Placeholder(_) => {
            Some(Norm::Int(Poly::atom(atoms.of(ct))))
        }
        ConstKind::Infer(_) | ConstKind::Bound(..) | ConstKind::Error(_) | ConstKind::Expr(_) => {
            None
        }
    }
}

/// The normal form of the anonymous constant `anon` used with `args`, `None` when its
/// expression has an operation the form does not have.
fn norm_of_anon<'db>(
    db: &'db Db,
    atoms: &mut Atoms<'db>,
    anon: AnonConstId<'db>,
    args: GenericArgs<'db>,
) -> Option<Norm> {
    let loc = anon.loc(db);
    let cx = Cx {
        db,
        store: ExpressionStore::of(db, loc.owner),
        resolver: loc.owner.resolver(db),
        generics: crate::generics::generics(db, loc.owner.generic_def(db)),
        args,
    };
    cx.norm(atoms, loc.expr, 0)
}

struct Cx<'db> {
    db: &'db Db,
    store: &'db ExpressionStore,
    resolver: hir_def::resolver::Resolver<'db>,
    generics: crate::generics::Generics<'db>,
    args: GenericArgs<'db>,
}

/// How deep an expression may nest before it has no normal form: a bound on the walk.
const DEPTH: u32 = 64;

impl<'db> Cx<'db> {
    fn norm(&self, atoms: &mut Atoms<'db>, expr: ExprId, depth: u32) -> Option<Norm> {
        if depth > DEPTH {
            return None;
        }
        match self.store.expr(expr) {
            Expr::Block { statements, tail: Some(tail), .. } if statements.is_empty() => {
                self.norm(atoms, tail, depth + 1)
            }
            Expr::Literal(Literal::Bool(value)) => Some(Norm::Bool(*value)),
            Expr::Literal(Literal::Int(value, _)) => Some(Norm::Int(Poly::constant(*value))),
            // A `line!()` or `column!()` is a placeholder in the store, its value where it is
            // written (`body_store::placed_literal`): no normal form of its own.
            Expr::Literal(Literal::Uint(..))
                if crate::body_store::calls_place_macro(self.db, self.store) =>
            {
                None
            }
            Expr::Literal(Literal::Uint(value, _)) => {
                Some(Norm::Int(Poly::constant(i128::try_from(*value).ok()?)))
            }
            Expr::Path(path) => {
                let named = crate::consteval::path_to_const(
                    self.db,
                    &self.resolver,
                    &|| &self.generics,
                    None,
                    path,
                    self.store.expr_path_hygiene(expr),
                )
                .ok()?;
                // A parameter stands for the argument the constant is used with.
                let named = match named.kind() {
                    ConstKind::Param(param) => {
                        self.args.as_slice().get(param.index as usize)?.konst()?
                    }
                    _ => named,
                };
                norm_of_const(self.db, atoms, named)
            }
            Expr::UnaryOp { expr, op: UnaryOp::Not } => match self.norm(atoms, expr, depth + 1)? {
                Norm::Int(_) => None,
                condition => Some(condition.not()),
            },
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => {
                Some(Norm::Int(self.int(atoms, expr, depth)?.neg()?))
            }
            Expr::BinaryOp { lhs, rhs, op: Some(op) } => match op {
                BinaryOp::ArithOp(ArithOp::Add) => {
                    let (a, b) = (self.int(atoms, lhs, depth)?, self.int(atoms, rhs, depth)?);
                    Some(Norm::Int(a.add(&b)?))
                }
                BinaryOp::ArithOp(ArithOp::Sub) => {
                    let (a, b) = (self.int(atoms, lhs, depth)?, self.int(atoms, rhs, depth)?);
                    Some(Norm::Int(a.add(&b.neg()?)?))
                }
                BinaryOp::ArithOp(ArithOp::Mul) => {
                    let (a, b) = (self.int(atoms, lhs, depth)?, self.int(atoms, rhs, depth)?);
                    Some(Norm::Int(a.mul(&b)?))
                }
                BinaryOp::CmpOp(cmp) => {
                    let (a, b) = (self.int(atoms, lhs, depth)?, self.int(atoms, rhs, depth)?);
                    let a_minus_b = a.clone().add(&b.clone().neg()?)?; // clone: a normal form's sides
                    match cmp {
                        CmpOp::Eq { negated } => {
                            let equal = Norm::compare(Against::Zero, a_minus_b)?;
                            Some(if negated { equal.not() } else { equal })
                        }
                        CmpOp::Ord { ordering, strict } => {
                            let against =
                                if strict { Against::Negative } else { Against::NotPositive };
                            let difference = match ordering {
                                Ordering::Less => a_minus_b,
                                Ordering::Greater => b.add(&a.neg()?)?,
                            };
                            Norm::compare(against, difference)
                        }
                    }
                }
                BinaryOp::LogicOp(logic) => {
                    let (a, b) = (self.bool(atoms, lhs, depth)?, self.bool(atoms, rhs, depth)?);
                    Some(a.join(b, matches!(logic, LogicOp::And)))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn int(&self, atoms: &mut Atoms<'db>, expr: ExprId, depth: u32) -> Option<Poly> {
        match self.norm(atoms, expr, depth + 1)? {
            Norm::Int(poly) => Some(poly),
            _ => None,
        }
    }

    fn bool(&self, atoms: &mut Atoms<'db>, expr: ExprId, depth: u32) -> Option<Norm> {
        match self.norm(atoms, expr, depth + 1)? {
            Norm::Int(_) => None,
            condition => Some(condition),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n() -> Poly {
        Poly::atom(0)
    }
    fn m() -> Poly {
        Poly::atom(1)
    }
    fn k(value: i128) -> Poly {
        Poly::constant(value)
    }

    /// `+` and `*` commute and associate, constants fold, and `x - x` is nothing.
    #[test]
    fn sums_and_products_have_one_form() {
        assert_eq!(n().add(&m()), m().add(&n()));
        assert_eq!(n().mul(&m()), m().mul(&n()));
        assert_eq!(n().add(&m()).unwrap().add(&k(1)), k(1).add(&m()).unwrap().add(&n()));
        // 2 * (N + 1) == N + N + 2
        let twice = k(2).mul(&n().add(&k(1)).unwrap()).unwrap();
        assert_eq!(twice, n().add(&n()).unwrap().add(&k(2)).unwrap());
        // N + 0 == N, N * 1 == N, N - N == 0
        assert_eq!(n().add(&k(0)).unwrap(), n());
        assert_eq!(n().mul(&k(1)).unwrap(), n());
        assert_eq!(n().add(&n().neg().unwrap()).unwrap().as_constant(), Some(0));
        // (N + M) * (N + M) == N*N + 2*N*M + M*M
        let sum = n().add(&m()).unwrap();
        let square = sum.mul(&sum).unwrap();
        let expanded = n()
            .mul(&n())
            .unwrap()
            .add(&k(2).mul(&n()).unwrap().mul(&m()).unwrap())
            .unwrap()
            .add(&m().mul(&m()).unwrap())
            .unwrap();
        assert_eq!(square, expanded);
    }

    #[test]
    fn different_expressions_differ() {
        assert_ne!(n().add(&m()).unwrap(), n().mul(&m()).unwrap());
        assert_ne!(n().add(&k(1)).unwrap(), n());
        assert_ne!(n(), m());
    }

    /// A coefficient that leaves `i128` has no normal form, never a wrapped one.
    #[test]
    fn a_coefficient_out_of_range_has_no_form() {
        assert_eq!(k(i128::MAX).add(&k(1)), None);
        assert_eq!(k(i128::MAX).mul(&k(2)), None);
        assert_eq!(k(i128::MIN).neg(), None);
    }

    /// `A + B == N` is `N == B + A`; `a < b` is `b > a`; a comparison of numbers folds.
    #[test]
    fn comparisons_are_differences_against_zero() {
        let (a, b, total) = (Poly::atom(0), Poly::atom(1), Poly::atom(2));
        let sum = a.clone().add(&b).unwrap();
        let left = Norm::compare(Against::Zero, sum.clone().add(&total.clone().neg().unwrap()).unwrap());
        let right = Norm::compare(Against::Zero, total.add(&sum.neg().unwrap()).unwrap());
        assert_eq!(left, right);
        let less = Norm::compare(Against::Negative, a.clone().add(&b.clone().neg().unwrap()).unwrap());
        let more = Norm::compare(Against::NotPositive, a.add(&b.neg().unwrap()).unwrap());
        assert_ne!(less, more);
        assert_eq!(Norm::compare(Against::Zero, k(0)), Some(Norm::Bool(true)));
        assert_eq!(Norm::compare(Against::Negative, k(3)), Some(Norm::Bool(false)));
    }

    /// `&&` and `||` are flattened, sorted and deduplicated, and fold with a known side.
    #[test]
    fn conjunctions_have_one_form() {
        let p = Norm::compare(Against::Negative, n()).unwrap();
        let q = Norm::compare(Against::Zero, m()).unwrap();
        assert_eq!(p.clone().join(q.clone(), true), q.clone().join(p.clone(), true));
        assert_eq!(p.clone().join(p.clone(), true), p);
        assert_eq!(p.clone().join(Norm::Bool(true), true), p);
        assert_eq!(p.clone().join(Norm::Bool(false), true), Norm::Bool(false));
        assert_eq!(p.clone().join(Norm::Bool(true), false), Norm::Bool(true));
        assert_ne!(p.clone().join(q.clone(), true), p.clone().join(q, false));
        assert_eq!(p.clone().not().not(), p);
    }
}
