//! Scalar expressions over one row, with SQL's three-valued logic: a comparison with null is
//! null, `and` is false if any side is false, `or` true if any is true, and a filter keeps a
//! row only where its predicate is true (query.md, "Types and NULL").

use core::alloc::Allocator;
use core::cmp::Ordering;
use core::fmt::Write;

use blaze_own::{String, Vec};
use blaze_sql::query::TypeName;

use crate::datum::{Datum, Kind, Mismatch, Store};
use crate::error::{ErrorKind, Fail, Run, fail};
use crate::exec::{Ctx, Index, Row, SubKind, SubValue};
use crate::plan::{Arith, Cmp, Expr, ExprKind, Func, Loc};

fn type_error<'s, T, A: Allocator>(at: Loc, op: &'static str, it: Mismatch) -> Run<'s, T, A> {
    fail(at, ErrorKind::Type { op, left: it.left, right: it.right })
}

/// A datum's text as SQL casts it to text: numbers in their shortest form that reads back
/// (a double always with a `.` or an exponent), booleans as `true` and `false`, lists as
/// `[a, b]`. Null is the caller's.
pub fn write_text<A: Allocator + Copy>(store: &Store<A>, d: Datum, out: &mut impl Write) -> core::fmt::Result {
    match d {
        Datum::Null => Ok(()),
        Datum::Bool(b) => out.write_str(if b { "true" } else { "false" }),
        Datum::Int(n) => write!(out, "{n}"),
        Datum::UInt(n) => write!(out, "{n}"),
        Datum::Float(f) => write_float(f, out),
        Datum::Text(sym) => out.write_str(store.text(sym)),
        Datum::List(list) => {
            out.write_char('[')?;
            for (at, item) in store.items(list).iter().enumerate() {
                if at > 0 {
                    out.write_str(", ")?;
                }
                if item.is_null() {
                    out.write_str("NULL")?;
                } else {
                    write_text(store, *item, out)?;
                }
            }
            out.write_char(']')
        }
    }
}

/// A double as the shortest text that reads back to it: `2.0`, `0.1`, `1e20`.
pub fn write_float(f: f64, out: &mut impl Write) -> core::fmt::Result {
    if f.is_nan() {
        return out.write_str("nan");
    }
    if f.is_infinite() {
        return out.write_str(if f > 0.0 { "inf" } else { "-inf" });
    }
    write!(out, "{f:?}")
}

/// The integer part of `f`, toward zero (core has no `trunc`).
#[expect(blaze::as_cast, reason = "a double of magnitude below 2^52 to an integer and back is exact")]
pub(crate) fn trunc(f: f64) -> f64 {
    const EXACT: f64 = 4_503_599_627_370_496.0;
    if !f.is_finite() || f.abs() >= EXACT {
        return f;
    }
    (f as i64) as f64
}

pub(crate) fn floor(f: f64) -> f64 {
    let t = trunc(f);
    if t > f { t - 1.0 } else { t }
}

pub(crate) fn ceil(f: f64) -> f64 {
    let t = trunc(f);
    if t < f { t + 1.0 } else { t }
}

/// Rounds half away from zero, as DuckDB's `round` does.
pub(crate) fn round_half_away(f: f64) -> f64 {
    let t = trunc(f);
    let rest = f - t;
    if rest >= 0.5 {
        t + 1.0
    } else if rest <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

fn pow10(n: i64) -> f64 {
    let mut out = 1.0;
    for _ in 0..n.unsigned_abs().min(400) {
        out *= 10.0;
    }
    if n < 0 { 1.0 / out } else { out }
}

/// SQL `like`: `%` any run, `_` any one character; no escape.
pub(crate) fn like(text: &str, pattern: &str) -> bool {
    let t: &[u8] = text.as_bytes();
    let p: &[u8] = pattern.as_bytes();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    // The width of the UTF-8 character starting at `at`.
    let width = |at: usize| match t.get(at) {
        Some(b) if *b < 0x80 => 1,
        Some(b) if *b >= 0xF0 => 4,
        Some(b) if *b >= 0xE0 => 3,
        Some(_) => 2,
        None => 1,
    };
    while ti < t.len() {
        match p.get(pi) {
            Some(b'%') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(b'_') => {
                pi += 1;
                ti += width(ti);
            }
            Some(c) if t.get(ti) == Some(c) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    let next = st + width(st);
                    star = Some((sp, next));
                    pi = sp + 1;
                    ti = next;
                }
                None => return false,
            },
        }
    }
    while p.get(pi) == Some(&b'%') {
        pi += 1;
    }
    pi == p.len()
}

impl<'p, 's, 't, A: Allocator + Copy, B: Allocator> Ctx<'p, 's, 't, A, B> {
    /// A predicate's truth: `None` for null.
    pub fn truth(&mut self, expr: &'p Expr<A>, row: &impl Row) -> Run<'s, Option<bool>, A> {
        match self.eval(expr, row)? {
            Datum::Null => Ok(None),
            Datum::Bool(b) => Ok(Some(b)),
            other => fail(expr.at, ErrorKind::Type { op: "a condition", left: other.kind(), right: Kind::Bool }),
        }
    }

    fn intern(&mut self, text: &str) -> Run<'s, Datum, A> {
        match self.store.intern(text) {
            Ok(sym) => Ok(Datum::Text(sym)),
            Err(full) => Err(self.store_full(full)),
        }
    }

    /// `d` as text, in a new buffer.
    fn text_of(&self, d: Datum) -> String<A> {
        let mut out = String::new_in(self.alloc);
        _ = write_text(&self.store, d, &mut out);
        out
    }

    fn text_arg(&self, d: Datum, at: Loc, op: &'static str) -> Run<'s, crate::datum::Sym, A> {
        match d {
            Datum::Text(sym) => Ok(sym),
            other => fail(at, ErrorKind::Type { op, left: other.kind(), right: Kind::Text }),
        }
    }

    /// Computes subquery `id` if it has not been this query.
    fn ensure_sub(&mut self, id: u32) -> Run<'s, (), A> {
        let at = id as usize;
        if matches!(self.subs.get(at), Some(Some(_))) {
            return Ok(());
        }
        let planned = self.planned;
        let Some(sub) = planned.subs.get(at) else {
            return Err(Fail::nowhere(ErrorKind::Unsupported("a subquery out of range")));
        };
        let rel = self.exec(&sub.plan)?;
        let value = match sub.kind {
            SubKind::Scalar => {
                if rel.width() != 1 || rel.rows > 1 {
                    return fail(sub.at, ErrorKind::NotScalar { rows: rel.rows, columns: rel.width() });
                }
                SubValue::Scalar(rel.get(0, 0))
            }
            SubKind::Set => {
                if rel.width() != 1 {
                    return fail(sub.at, ErrorKind::NotScalar { rows: rel.rows, columns: rel.width() });
                }
                let mut index = Index::new_in(1, self.alloc);
                let mut has_null = false;
                for row in 0..rel.rows {
                    let d = rel.get(0, row);
                    if d.is_null() {
                        has_null = true;
                        continue;
                    }
                    let hash = self.store.hash_key(&[d]);
                    if index.find_same(&self.store, hash, &[d]).is_none() {
                        index.insert(hash, &[d], 0);
                    }
                }
                SubValue::Set { index, has_null }
            }
            SubKind::Exists { keys } => {
                let width = keys as usize;
                let mut index = Index::new_in(width, self.alloc);
                let mut key = Vec::with_capacity_in(width, self.alloc);
                for row in 0..rel.rows {
                    key.clear();
                    for col in 0..width {
                        key.push(rel.get(col, row));
                    }
                    // A null inner key equals nothing.
                    if key.iter().any(|d| d.is_null()) {
                        continue;
                    }
                    let hash = self.store.hash_key(&key);
                    if index.find_same(&self.store, hash, &key).is_none() {
                        index.insert(hash, &key, 0);
                    }
                    if width == 0 {
                        break;
                    }
                }
                SubValue::Exists(index)
            }
        };
        if let Some(slot) = self.subs.get_mut(at) {
            *slot = Some(value);
        }
        Ok(())
    }

    fn compare(&self, op: Cmp, l: Datum, r: Datum, at: Loc) -> Run<'s, Datum, A> {
        let ordering = match self.store.compare(l, r) {
            Ok(Some(it)) => it,
            Ok(None) => return Ok(Datum::Null),
            Err(it) => return type_error(at, "a comparison", it),
        };
        // NaN equals NaN and is above every number, as a sort has it.
        Ok(Datum::Bool(match op {
            Cmp::Eq => ordering == Ordering::Equal,
            Cmp::Ne => ordering != Ordering::Equal,
            Cmp::Lt => ordering == Ordering::Less,
            Cmp::Le => ordering != Ordering::Greater,
            Cmp::Gt => ordering == Ordering::Greater,
            Cmp::Ge => ordering != Ordering::Less,
        }))
    }

    fn arith(&self, op: Arith, l: Datum, r: Datum, at: Loc) -> Run<'s, Datum, A> {
        if l.is_null() || r.is_null() {
            return Ok(Datum::Null);
        }
        let name = match op {
            Arith::Add => "+",
            Arith::Sub => "-",
            Arith::Mul => "*",
            Arith::Div => "/",
            Arith::Rem => "%",
        };
        let mismatch = Mismatch { left: l.kind(), right: r.kind() };
        if let (Some(x), Some(y)) = (l.as_i128(), r.as_i128())
            && op != Arith::Div
        {
            let n = match op {
                Arith::Add => x.checked_add(y),
                Arith::Sub => x.checked_sub(y),
                Arith::Mul => x.checked_mul(y),
                Arith::Rem if y == 0 => return Ok(Datum::Null),
                Arith::Rem => x.checked_rem(y),
                Arith::Div => None,
            };
            return match n.and_then(Datum::int) {
                Some(d) => Ok(d),
                None => fail(at, ErrorKind::Overflow(name)),
            };
        }
        let (Some(x), Some(y)) = (l.as_f64(), r.as_f64()) else {
            return type_error(at, name, mismatch);
        };
        Ok(match op {
            Arith::Add => Datum::Float(x + y),
            Arith::Sub => Datum::Float(x - y),
            Arith::Mul => Datum::Float(x * y),
            // Division by zero is null, as DuckDB has it.
            Arith::Div | Arith::Rem if y == 0.0 => Datum::Null,
            Arith::Div => Datum::Float(x / y),
            Arith::Rem => Datum::Float(x % y),
        })
    }

    /// `d` cast to `to`.
    #[expect(blaze::as_cast, reason = "a double to an integer only after checking it is in range")]
    pub(crate) fn cast(&mut self, d: Datum, to: TypeName, at: Loc) -> Run<'s, Datum, A> {
        if d.is_null() {
            return Ok(Datum::Null);
        }
        match to {
            TypeName::Text => {
                if let Datum::Text(_) = d {
                    return Ok(d);
                }
                let text = self.text_of(d);
                self.intern(text.as_str())
            }
            TypeName::Float => match d {
                Datum::Text(sym) => match self.store.text(sym).trim().parse::<f64>() {
                    Ok(f) => Ok(Datum::Float(f)),
                    Err(_) => fail(at, ErrorKind::NotANumber),
                },
                Datum::Bool(b) => Ok(Datum::Float(if b { 1.0 } else { 0.0 })),
                other => match other.as_f64() {
                    Some(f) => Ok(Datum::Float(f)),
                    None => fail(at, ErrorKind::Type { op: "a cast to double", left: other.kind(), right: Kind::Float }),
                },
            },
            TypeName::Boolean => match d {
                Datum::Bool(_) => Ok(d),
                Datum::Text(sym) => match self.store.text(sym).trim() {
                    t if t.eq_ignore_ascii_case("true") || t == "1" => Ok(Datum::Bool(true)),
                    t if t.eq_ignore_ascii_case("false") || t == "0" => Ok(Datum::Bool(false)),
                    _ => fail(at, ErrorKind::NotANumber),
                },
                other => match other.as_f64() {
                    Some(f) => Ok(Datum::Bool(f != 0.0)),
                    None => fail(at, ErrorKind::Type { op: "a cast to boolean", left: other.kind(), right: Kind::Bool }),
                },
            },
            TypeName::Int { bits, signed } => {
                let n: i128 = match d {
                    Datum::Bool(b) => i128::from(b),
                    Datum::Int(_) | Datum::UInt(_) => d.as_i128().unwrap_or(0),
                    Datum::Float(f) => {
                        let r = round_half_away(f);
                        if !r.is_finite() || r.abs() >= 1.7e38 {
                            return fail(at, ErrorKind::Overflow("a cast to an integer"));
                        }
                        r as i128
                    }
                    Datum::Text(sym) => {
                        let text = self.store.text(sym).trim();
                        match text.parse::<i128>() {
                            Ok(n) => n,
                            Err(_) => match text.parse::<f64>() {
                                Ok(f) if f.is_finite() && f.abs() < 1.7e38 => round_half_away(f) as i128,
                                _ => return fail(at, ErrorKind::NotANumber),
                            },
                        }
                    }
                    Datum::List(_) | Datum::Null => {
                        return fail(at, ErrorKind::Type { op: "a cast to an integer", left: d.kind(), right: Kind::Int });
                    }
                };
                let fits = match (bits, signed) {
                    (128, true) => true,
                    (b, true) => {
                        let half = 1i128 << (u32::from(b) - 1);
                        (-half..half).contains(&n)
                    }
                    (b, false) => n >= 0 && n < (1i128 << u32::from(b).min(127)),
                };
                match Datum::int(n).filter(|_| fits) {
                    Some(it) => Ok(it),
                    None => fail(at, ErrorKind::Overflow("a cast to an integer")),
                }
            }
        }
    }

    /// Evaluates `expr` on `row`.
    pub fn eval(&mut self, expr: &'p Expr<A>, row: &impl Row) -> Run<'s, Datum, A> {
        let at = expr.at;
        Ok(match &expr.kind {
            ExprKind::Lit(d) => *d,
            ExprKind::Col(c) => row.col(*c),
            ExprKind::Neg(operand) => match self.eval(operand, row)? {
                Datum::Null => Datum::Null,
                Datum::Float(f) => Datum::Float(-f),
                d @ (Datum::Int(_) | Datum::UInt(_)) => match Datum::int(-d.as_i128().unwrap_or(0)) {
                    Some(it) => it,
                    None => return fail(at, ErrorKind::Overflow("-")),
                },
                other => return fail(at, ErrorKind::Type { op: "-", left: other.kind(), right: Kind::Int }),
            },
            ExprKind::Not(operand) => match self.truth(operand, row)? {
                Some(b) => Datum::Bool(!b),
                None => Datum::Null,
            },
            ExprKind::And(items) => {
                let mut null = false;
                for item in items.iter() {
                    match self.truth(item, row)? {
                        Some(false) => return Ok(Datum::Bool(false)),
                        Some(true) => {}
                        None => null = true,
                    }
                }
                if null { Datum::Null } else { Datum::Bool(true) }
            }
            ExprKind::Or(items) => {
                let mut null = false;
                for item in items.iter() {
                    match self.truth(item, row)? {
                        Some(true) => return Ok(Datum::Bool(true)),
                        Some(false) => {}
                        None => null = true,
                    }
                }
                if null { Datum::Null } else { Datum::Bool(false) }
            }
            ExprKind::Arith { op, left, right } => {
                let l = self.eval(left, row)?;
                let r = self.eval(right, row)?;
                self.arith(*op, l, r, at)?
            }
            ExprKind::Concat { left, right } => {
                let l = self.eval(left, row)?;
                let r = self.eval(right, row)?;
                if l.is_null() || r.is_null() {
                    return Ok(Datum::Null);
                }
                let mut text = self.text_of(l);
                _ = write_text(&self.store, r, &mut text);
                self.intern(text.as_str())?
            }
            ExprKind::Compare { op, left, right } => {
                let l = self.eval(left, row)?;
                let r = self.eval(right, row)?;
                self.compare(*op, l, r, at)?
            }
            ExprKind::Distinct { left, right, negated } => {
                let l = self.eval(left, row)?;
                let r = self.eval(right, row)?;
                if !l.is_null() && !r.is_null() && let Err(it) = self.store.order(l, r) {
                    return type_error(at, "is distinct from", it);
                }
                let same = self.store.same(l, r);
                Datum::Bool(if *negated { same } else { !same })
            }
            ExprKind::IsNull { operand, negated } => {
                let null = self.eval(operand, row)?.is_null();
                Datum::Bool(null != *negated)
            }
            ExprKind::InList { operand, list, negated } => {
                let d = self.eval(operand, row)?;
                if d.is_null() {
                    return Ok(Datum::Null);
                }
                let mut null = false;
                let mut found = false;
                for item in list.iter() {
                    let e = self.eval(item, row)?;
                    match self.compare(Cmp::Eq, d, e, item.at)? {
                        Datum::Bool(true) => {
                            found = true;
                            break;
                        }
                        Datum::Null => null = true,
                        _ => {}
                    }
                }
                match (found, null) {
                    (true, _) => Datum::Bool(!*negated),
                    (false, true) => Datum::Null,
                    (false, false) => Datum::Bool(*negated),
                }
            }
            ExprKind::InSet { operand, sub, negated } => {
                let d = self.eval(operand, row)?;
                self.ensure_sub(*sub)?;
                let Some(Some(SubValue::Set { index, has_null })) = self.subs.get(*sub as usize) else {
                    return Err(Fail::nowhere(ErrorKind::Unsupported("an `in` subquery of another kind")));
                };
                if d.is_null() {
                    // Null in an empty set is false; else null.
                    return Ok(if index.len() == 0 && !*has_null { Datum::Bool(*negated) } else { Datum::Null });
                }
                let hash = self.store.hash_key(&[d]);
                if index.find_same(&self.store, hash, &[d]).is_some() {
                    Datum::Bool(!*negated)
                } else if *has_null {
                    Datum::Null
                } else {
                    Datum::Bool(*negated)
                }
            }
            ExprKind::Between { operand, low, high, negated } => {
                let d = self.eval(operand, row)?;
                let lo = self.eval(low, row)?;
                let hi = self.eval(high, row)?;
                let above = self.compare(Cmp::Ge, d, lo, at)?;
                let below = self.compare(Cmp::Le, d, hi, at)?;
                let both = match (above, below) {
                    (Datum::Bool(false), _) | (_, Datum::Bool(false)) => Datum::Bool(false),
                    (Datum::Bool(true), Datum::Bool(true)) => Datum::Bool(true),
                    _ => Datum::Null,
                };
                match both {
                    Datum::Bool(b) => Datum::Bool(b != *negated),
                    other => other,
                }
            }
            ExprKind::Like { operand, pattern, negated } => {
                let d = self.eval(operand, row)?;
                let p = self.eval(pattern, row)?;
                if d.is_null() || p.is_null() {
                    return Ok(Datum::Null);
                }
                let text = self.text_arg(d, at, "like")?;
                let pattern = self.text_arg(p, at, "like")?;
                Datum::Bool(like(self.store.text(text), self.store.text(pattern)) != *negated)
            }
            ExprKind::Case { operand, arms, otherwise } => {
                let subject = match operand {
                    Some(it) => Some(self.eval(it, row)?),
                    None => None,
                };
                for (when, then) in arms.iter() {
                    let hit = match subject {
                        Some(s) => {
                            let w = self.eval(when, row)?;
                            matches!(self.compare(Cmp::Eq, s, w, when.at)?, Datum::Bool(true))
                        }
                        None => self.truth(when, row)? == Some(true),
                    };
                    if hit {
                        return self.eval(then, row);
                    }
                }
                match otherwise {
                    Some(it) => self.eval(it, row)?,
                    None => Datum::Null,
                }
            }
            ExprKind::Cast { operand, to } => {
                let d = self.eval(operand, row)?;
                self.cast(d, *to, at)?
            }
            ExprKind::Call { func, args } => self.call(*func, args, row, at)?,
            ExprKind::Scalar(sub) => {
                self.ensure_sub(*sub)?;
                match self.subs.get(*sub as usize) {
                    Some(Some(SubValue::Scalar(d))) => *d,
                    _ => return Err(Fail::nowhere(ErrorKind::Unsupported("a scalar subquery of another kind"))),
                }
            }
            ExprKind::Exists { sub, keys } => {
                self.ensure_sub(*sub)?;
                let mut key = Vec::with_capacity_in(keys.len(), self.alloc);
                for k in keys.iter() {
                    key.push(self.eval(k, row)?);
                }
                // A null outer key equals no inner row.
                if key.iter().any(|d| d.is_null()) {
                    return Ok(Datum::Bool(false));
                }
                let Some(Some(SubValue::Exists(index))) = self.subs.get(*sub as usize) else {
                    return Err(Fail::nowhere(ErrorKind::Unsupported("an `exists` subquery of another kind")));
                };
                let hash = self.store.hash_key(&key);
                Datum::Bool(index.find_same(&self.store, hash, &key).is_some())
            }
        })
    }

    fn call(&mut self, func: Func, args: &'p [Expr<A>], row: &impl Row, at: Loc) -> Run<'s, Datum, A> {
        // Lazy ones first: `coalesce` stops at its first non-null.
        if func == Func::Coalesce {
            for arg in args {
                let d = self.eval(arg, row)?;
                if !d.is_null() {
                    return Ok(d);
                }
            }
            return Ok(Datum::Null);
        }
        let mut values = Vec::with_capacity_in(args.len(), self.alloc);
        for arg in args {
            values.push(self.eval(arg, row)?);
        }
        let arg = |i: usize| values.get(i).copied().unwrap_or(Datum::Null);
        Ok(match func {
            Func::Coalesce => Datum::Null,
            Func::Nullif => {
                let (a, b) = (arg(0), arg(1));
                if matches!(self.compare(Cmp::Eq, a, b, at)?, Datum::Bool(true)) { Datum::Null } else { a }
            }
            Func::Greatest | Func::Least => {
                // Nulls are skipped, as DuckDB has it; all null is null.
                let mut best = Datum::Null;
                for &d in values.iter() {
                    if d.is_null() {
                        continue;
                    }
                    if best.is_null() {
                        best = d;
                        continue;
                    }
                    let it = match self.store.order(d, best) {
                        Ok(it) => it,
                        Err(it) => return type_error(at, "greatest/least", it),
                    };
                    let better = if func == Func::Greatest { it == Ordering::Greater } else { it == Ordering::Less };
                    if better {
                        best = d;
                    }
                }
                best
            }
            Func::Round => {
                let d = arg(0);
                let places = match values.get(1) {
                    Some(p) if p.is_null() => return Ok(Datum::Null),
                    Some(p) => match p.as_i128().and_then(|it| i64::try_from(it).ok()) {
                        Some(n) => n,
                        None => return fail(at, ErrorKind::Type { op: "round", left: p.kind(), right: Kind::Int }),
                    },
                    None => 0,
                };
                match d {
                    Datum::Null => Datum::Null,
                    Datum::Int(_) | Datum::UInt(_) if places >= 0 => d,
                    Datum::Float(f) if !f.is_finite() => d,
                    other => match other.as_f64() {
                        Some(f) => {
                            let scale = pow10(places);
                            let r = round_half_away(f * scale) / scale;
                            if matches!(other, Datum::Float(_)) {
                                Datum::Float(r)
                            } else {
                                self.cast(Datum::Float(r), TypeName::Int { bits: 64, signed: true }, at)?
                            }
                        }
                        None => return fail(at, ErrorKind::Type { op: "round", left: other.kind(), right: Kind::Float }),
                    },
                }
            }
            Func::Abs | Func::Floor | Func::Ceil => match arg(0) {
                Datum::Null => Datum::Null,
                d @ (Datum::Int(_) | Datum::UInt(_)) => {
                    if func == Func::Abs {
                        match Datum::int(d.as_i128().unwrap_or(0).abs()) {
                            Some(it) => it,
                            None => return fail(at, ErrorKind::Overflow("abs")),
                        }
                    } else {
                        d
                    }
                }
                Datum::Float(f) => Datum::Float(match func {
                    Func::Abs => f.abs(),
                    Func::Floor => floor(f),
                    _ => ceil(f),
                }),
                other => return fail(at, ErrorKind::Type { op: "abs/floor/ceil", left: other.kind(), right: Kind::Float }),
            },
            Func::StartsWith | Func::EndsWith | Func::Contains => {
                let (a, b) = (arg(0), arg(1));
                if a.is_null() || b.is_null() {
                    return Ok(Datum::Null);
                }
                let (a, b) = (self.text_arg(a, at, "starts_with")?, self.text_arg(b, at, "starts_with")?);
                let (a, b) = (self.store.text(a), self.store.text(b));
                Datum::Bool(match func {
                    Func::StartsWith => a.starts_with(b),
                    Func::EndsWith => a.ends_with(b),
                    _ => a.contains(b),
                })
            }
            Func::SplitPart => {
                let (s, sep, n) = (arg(0), arg(1), arg(2));
                if s.is_null() || sep.is_null() || n.is_null() {
                    return Ok(Datum::Null);
                }
                let s = self.text_arg(s, at, "split_part")?;
                let sep = self.text_arg(sep, at, "split_part")?;
                let Some(n) = n.as_i128() else {
                    return fail(at, ErrorKind::Type { op: "split_part", left: n.kind(), right: Kind::Int });
                };
                let mut part = String::new_in(self.alloc);
                {
                    let (s, sep) = (self.store.text(s), self.store.text(sep));
                    let count = if sep.is_empty() { 1 } else { s.split(sep).count() };
                    // 1 is the first part, -1 the last; out of range is empty.
                    let index = if n > 0 { usize::try_from(n - 1).ok() } else {
                        usize::try_from(-n).ok().and_then(|it| count.checked_sub(it))
                    };
                    let found = if sep.is_empty() {
                        (index == Some(0)).then_some(s)
                    } else {
                        index.and_then(|i| s.split(sep).nth(i))
                    };
                    part.push_str(found.unwrap_or(""));
                }
                self.intern(part.as_str())?
            }
            Func::Replace => {
                let (s, from, to) = (arg(0), arg(1), arg(2));
                if s.is_null() || from.is_null() || to.is_null() {
                    return Ok(Datum::Null);
                }
                let s = self.text_arg(s, at, "replace")?;
                let from = self.text_arg(from, at, "replace")?;
                let to = self.text_arg(to, at, "replace")?;
                let mut out = String::new_in(self.alloc);
                {
                    let (s, from, to) = (self.store.text(s), self.store.text(from), self.store.text(to));
                    if from.is_empty() {
                        out.push_str(s);
                    } else {
                        let mut rest = s;
                        while let Some(i) = rest.find(from) {
                            out.push_str(rest.get(..i).unwrap_or(""));
                            out.push_str(to);
                            rest = rest.get(i + from.len()..).unwrap_or("");
                        }
                        out.push_str(rest);
                    }
                }
                self.intern(out.as_str())?
            }
            Func::Concat => {
                // `concat` skips nulls, unlike `||`.
                let mut out = String::new_in(self.alloc);
                for &d in values.iter() {
                    _ = write_text(&self.store, d, &mut out);
                }
                self.intern(out.as_str())?
            }
            Func::Length => match arg(0) {
                Datum::Null => Datum::Null,
                Datum::List(list) => Datum::from_u64(u64::try_from(self.store.items(list).len()).unwrap_or(u64::MAX)),
                d => {
                    let sym = self.text_arg(d, at, "length")?;
                    Datum::from_u64(u64::try_from(self.store.text(sym).chars().count()).unwrap_or(u64::MAX))
                }
            },
            Func::Lower | Func::Upper => match arg(0) {
                Datum::Null => Datum::Null,
                d => {
                    let sym = self.text_arg(d, at, "lower/upper")?;
                    let mut out = String::new_in(self.alloc);
                    for c in self.store.text(sym).chars() {
                        if func == Func::Lower {
                            for l in c.to_lowercase() {
                                out.push(l);
                            }
                        } else {
                            for u in c.to_uppercase() {
                                out.push(u);
                            }
                        }
                    }
                    self.intern(out.as_str())?
                }
            },
            Func::ArrayToString => {
                let (list, sep) = (arg(0), arg(1));
                if list.is_null() || sep.is_null() {
                    return Ok(Datum::Null);
                }
                let Datum::List(list) = list else {
                    return fail(at, ErrorKind::Type { op: "array_to_string", left: list.kind(), right: Kind::List });
                };
                let sep = self.text_arg(sep, at, "array_to_string")?;
                let mut out = String::new_in(self.alloc);
                let mut first = true;
                for &item in self.store.items(list) {
                    // Null elements are skipped, as DuckDB has it.
                    if item.is_null() {
                        continue;
                    }
                    if !first {
                        out.push_str(self.store.text(sep));
                    }
                    first = false;
                    _ = write_text(&self.store, item, &mut out);
                }
                self.intern(out.as_str())?
            }
            Func::Error => {
                let message = self.text_of(arg(0));
                return fail(at, ErrorKind::Raised(message));
            }
        })
    }
}
