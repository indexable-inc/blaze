//! Aggregates and windows: an aggregate's groups in the order their first row stands, each
//! group's rows in input order; a window's partitions sorted stably by its order, each row's
//! value from its frame (query.md, "The plan IR").
//!
//! Every aggregate skips nulls; `count(*)` counts rows; over no rows `count` is 0 and every
//! other aggregate null. Integer sums are exact and refused past the 64-bit range.

use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_own::{String, Vec};

use crate::datum::{Datum, Kind};
use crate::error::{ErrorKind, Run, fail};
use crate::eval::write_text;
use crate::exec::{At, Ctx, Index, Rel};
use crate::plan::{Agg, AggFunc, Expr, FrameBound, Loc, WinFunc, Window};

/// The running state of an aggregate that needs only its values in turn.
#[derive(Clone, Copy, Debug)]
enum State {
    Count(u64),
    Sum { int: i128, float: f64, floats: bool, any: bool },
    Avg { sum: f64, n: u64 },
    /// `min`, `max`, `any_value`: the value so far.
    Pick(Datum),
    /// `arg_max`, `arg_min`: the argument at the best key so far.
    Arg { key: Datum, arg: Datum },
    Bool(Option<bool>),
}

/// An aggregate's state: running, or the rows it has seen (for `distinct`, an inner `order by`,
/// `median` and `string_agg`, which need all their values).
enum Acc<A: Allocator> {
    Run(State),
    Rows(Vec<u32, A>),
}

fn needs_rows<A: Allocator>(agg: &Agg<A>) -> bool {
    agg.distinct || !agg.order.is_empty() || matches!(agg.func, AggFunc::Median | AggFunc::StringAgg)
}

fn start(func: AggFunc) -> State {
    match func {
        AggFunc::CountRows | AggFunc::Count => State::Count(0),
        AggFunc::Sum => State::Sum { int: 0, float: 0.0, floats: false, any: false },
        AggFunc::Avg | AggFunc::Median => State::Avg { sum: 0.0, n: 0 },
        AggFunc::Min | AggFunc::Max | AggFunc::AnyValue | AggFunc::StringAgg => State::Pick(Datum::Null),
        AggFunc::ArgMax | AggFunc::ArgMin => State::Arg { key: Datum::Null, arg: Datum::Null },
        AggFunc::BoolAnd | AggFunc::BoolOr => State::Bool(None),
    }
}

impl<'p, 's, 't, A: Allocator + Copy, B: Allocator> Ctx<'p, 's, 't, A, B> {
    fn acc(&self, agg: &Agg<A>) -> Acc<A> {
        if needs_rows(agg) { Acc::Rows(Vec::new_in(self.alloc)) } else { Acc::Run(start(agg.func)) }
    }

    /// Folds one row's values (`vals`, the aggregate's arguments) into `state`.
    fn update(&self, state: &mut State, func: AggFunc, vals: &[Datum], at: Loc) -> Run<'s, (), A> {
        let first = vals.first().copied().unwrap_or(Datum::Null);
        if func == AggFunc::CountRows {
            if let State::Count(n) = state {
                *n += 1;
            }
            return Ok(());
        }
        if first.is_null() {
            return Ok(());
        }
        match state {
            State::Count(n) => *n += 1,
            State::Sum { int, float, floats, any } => {
                *any = true;
                match first {
                    Datum::Float(f) => {
                        *floats = true;
                        *float += f;
                    }
                    d => match d.as_i128() {
                        Some(n) => match int.checked_add(n) {
                            Some(it) => *int = it,
                            None => return fail(at, ErrorKind::Overflow("sum")),
                        },
                        None => return fail(at, ErrorKind::Type { op: "sum", left: d.kind(), right: Kind::Int }),
                    },
                }
            }
            State::Avg { sum, n } => match first.as_f64() {
                Some(f) => {
                    *sum += f;
                    *n += 1;
                }
                None => return fail(at, ErrorKind::Type { op: "avg", left: first.kind(), right: Kind::Float }),
            },
            State::Pick(best) => {
                let better = match func {
                    AggFunc::AnyValue | AggFunc::StringAgg => best.is_null(),
                    _ if best.is_null() => true,
                    _ => {
                        let it = self.store.order(first, *best).map_err(|it| crate::error::Fail {
                            at: Some(at),
                            kind: ErrorKind::Type { op: "min/max", left: it.left, right: it.right },
                        })?;
                        if func == AggFunc::Max { it == Ordering::Greater } else { it == Ordering::Less }
                    }
                };
                if better {
                    *best = first;
                }
            }
            State::Arg { key, arg } => {
                // `arg_max(arg, key)`: rows of a null key are skipped; the first best wins.
                let k = vals.get(1).copied().unwrap_or(Datum::Null);
                if k.is_null() {
                    return Ok(());
                }
                let better = key.is_null() || {
                    let it = self.store.order(k, *key).map_err(|it| crate::error::Fail {
                        at: Some(at),
                        kind: ErrorKind::Type { op: "arg_max/arg_min", left: it.left, right: it.right },
                    })?;
                    if func == AggFunc::ArgMax { it == Ordering::Greater } else { it == Ordering::Less }
                };
                if better {
                    *key = k;
                    *arg = first;
                }
            }
            State::Bool(b) => match first {
                Datum::Bool(v) => {
                    *b = Some(match (*b, func) {
                        (None, _) => v,
                        (Some(x), AggFunc::BoolAnd) => x && v,
                        (Some(x), _) => x || v,
                    });
                }
                other => return fail(at, ErrorKind::Type { op: "bool_and/bool_or", left: other.kind(), right: Kind::Bool }),
            },
        }
        Ok(())
    }

    fn result(state: State, at: Loc) -> Run<'s, Datum, A> {
        Ok(match state {
            State::Count(n) => Datum::from_u64(n),
            State::Sum { any: false, .. } => Datum::Null,
            State::Sum { int, float, floats, .. } => {
                if floats {
                    let mut f = float;
                    if int != 0 {
                        f += Datum::int(int).and_then(Datum::as_f64).unwrap_or(0.0);
                    }
                    Datum::Float(f)
                } else {
                    match Datum::int(int) {
                        Some(it) => it,
                        None => return fail(at, ErrorKind::Overflow("sum")),
                    }
                }
            }
            State::Avg { n: 0, .. } => Datum::Null,
            State::Avg { sum, n } => Datum::Float(sum / Datum::from_u64(n).as_f64().unwrap_or(1.0)),
            State::Pick(d) => d,
            State::Arg { arg, .. } => arg,
            State::Bool(b) => b.map_or(Datum::Null, Datum::Bool),
        })
    }

    /// The arguments of `agg` on `row`, or `None` where its `filter` drops the row.
    fn args_of(&mut self, agg: &'p Agg<A>, rel: &Rel<A>, row: usize, vals: &mut Vec<Datum, A>) -> Run<'s, bool, A> {
        let at = At { rel, row };
        if let Some(filter) = &agg.filter
            && self.truth(filter, &at)? != Some(true)
        {
            return Ok(false);
        }
        vals.clear();
        for arg in agg.args.iter() {
            vals.push(self.eval(arg, &at)?);
        }
        Ok(true)
    }

    fn feed(&mut self, acc: &mut Acc<A>, agg: &'p Agg<A>, rel: &Rel<A>, row: u32, vals: &mut Vec<Datum, A>) -> Run<'s, (), A> {
        if !self.args_of(agg, rel, row as usize, vals)? {
            return Ok(());
        }
        match acc {
            Acc::Run(state) => self.update(state, agg.func, vals, agg.at),
            Acc::Rows(rows) => {
                rows.push(row);
                Ok(())
            }
        }
    }

    fn finish(&mut self, acc: &Acc<A>, agg: &'p Agg<A>, rel: &Rel<A>) -> Run<'s, Datum, A> {
        let rows = match acc {
            Acc::Run(state) => return Self::result(*state, agg.at),
            Acc::Rows(rows) => rows,
        };
        let mut order = Vec::from_slice_in(rows, self.alloc);
        self.sort_rows(rel, &mut order, &agg.order)?;
        // Each row's arguments, in the aggregate's order.
        let width = agg.args.len().max(1);
        let mut values = Vec::with_capacity_in(order.len() * width, self.alloc);
        let mut vals = Vec::new_in(self.alloc);
        for &row in order.iter() {
            vals.clear();
            let at = At { rel, row: row as usize };
            for arg in agg.args.iter() {
                vals.push(self.eval(arg, &at)?);
            }
            vals.resize(width, Datum::Null);
            values.extend_from_slice(&vals);
        }
        let mut kept = Vec::with_capacity_in(order.len(), self.alloc);
        if agg.distinct {
            let mut seen = Index::new_in(1, self.alloc);
            for i in 0..order.len() {
                let d = values.get(i * width).copied().unwrap_or(Datum::Null);
                let hash = self.store.hash_key(&[d]);
                if seen.find_same(&self.store, hash, &[d]).is_none() {
                    seen.insert(hash, &[d], 0);
                    kept.push(i);
                }
            }
        } else {
            for i in 0..order.len() {
                kept.push(i);
            }
        }
        let row_vals = |i: usize| values.get(i * width..i * width + width).unwrap_or(&[]);
        match agg.func {
            AggFunc::StringAgg => {
                let mut out = String::new_in(self.alloc);
                let mut any = false;
                for &i in kept.iter() {
                    let vals = row_vals(i);
                    let d = vals.first().copied().unwrap_or(Datum::Null);
                    if d.is_null() {
                        continue;
                    }
                    if any {
                        match vals.get(1) {
                            Some(Datum::Text(sep)) => out.push_str(self.store.text(*sep)),
                            Some(Datum::Null) => {}
                            Some(other) => {
                                return fail(agg.at, ErrorKind::Type { op: "string_agg", left: other.kind(), right: Kind::Text });
                            }
                            None => out.push(','),
                        }
                    }
                    any = true;
                    _ = write_text(&self.store, d, &mut out);
                }
                if !any {
                    return Ok(Datum::Null);
                }
                match self.store.intern(out.as_str()) {
                    Ok(sym) => Ok(Datum::Text(sym)),
                    Err(full) => Err(self.store_full(full)),
                }
            }
            AggFunc::Median => {
                let mut nums = Vec::with_capacity_in(kept.len(), self.alloc);
                for &i in kept.iter() {
                    let d = row_vals(i).first().copied().unwrap_or(Datum::Null);
                    if d.is_null() {
                        continue;
                    }
                    match d.as_f64() {
                        Some(f) => nums.push(f),
                        None => return fail(agg.at, ErrorKind::Type { op: "median", left: d.kind(), right: Kind::Float }),
                    }
                }
                if nums.is_empty() {
                    return Ok(Datum::Null);
                }
                nums.sort_unstable_by(f64::total_cmp);
                let n = nums.len();
                let mid = nums.get(n / 2).copied().unwrap_or(0.0);
                // An even count is the mean of the middle two, as `quantile_cont(0.5)`.
                Ok(Datum::Float(if n % 2 == 0 {
                    let low = nums.get(n / 2 - 1).copied().unwrap_or(0.0);
                    low + (mid - low) / 2.0
                } else {
                    mid
                }))
            }
            func => {
                let mut state = start(func);
                for &i in kept.iter() {
                    self.update(&mut state, func, row_vals(i), agg.at)?;
                }
                Self::result(state, agg.at)
            }
        }
    }

    /// The key datums of `exprs` on `row`.
    fn key_of(&mut self, exprs: &'p [Expr<A>], rel: &Rel<A>, row: usize, key: &mut Vec<Datum, A>) -> Run<'s, (), A> {
        key.clear();
        for expr in exprs {
            key.push(self.eval(expr, &At { rel, row })?);
        }
        Ok(())
    }

    /// Rows grouped by `keys` (nulls one group): each group's rows in input order, groups in
    /// order of first appearance, as offsets into one array.
    fn groups(&mut self, rel: &Rel<A>, keys: &'p [Expr<A>]) -> Run<'s, (Vec<u32, A>, Vec<u32, A>), A> {
        let mut index = Index::new_in(keys.len(), self.alloc);
        let mut group_of = Vec::with_capacity_in(rel.rows, self.alloc);
        let mut sizes: Vec<u32, A> = Vec::new_in(self.alloc);
        let mut key = Vec::new_in(self.alloc);
        for row in 0..rel.rows {
            self.key_of(keys, rel, row, &mut key)?;
            let hash = self.store.hash_key(&key);
            let group = match index.find_same(&self.store, hash, &key) {
                Some(entry) => index.row(entry),
                None => {
                    let group = u32::try_from(sizes.len()).unwrap_or(u32::MAX);
                    index.insert(hash, &key, group);
                    sizes.push(0);
                    group
                }
            };
            if let Some(size) = sizes.get_mut(group as usize) {
                *size += 1;
            }
            group_of.push(group);
        }
        // Offsets, then each row into its group's next slot.
        let mut starts = Vec::with_capacity_in(sizes.len() + 1, self.alloc);
        let mut total = 0u32;
        starts.push(0);
        for size in sizes.iter() {
            total += size;
            starts.push(total);
        }
        let mut fill = Vec::from_slice_in(&starts, self.alloc);
        let mut rows = Vec::from_elem_in(0u32, rel.rows, self.alloc);
        for (row, &group) in group_of.iter().enumerate() {
            if let Some(slot) = fill.get_mut(group as usize) {
                if let Some(out) = rows.get_mut(*slot as usize) {
                    *out = u32::try_from(row).unwrap_or(u32::MAX);
                }
                *slot += 1;
            }
        }
        Ok((starts, rows))
    }

    /// `group by keys` with `aggs`: the keys' columns, then the aggregates'.
    pub(crate) fn aggregate(&mut self, rel: &Rel<A>, keys: &'p [Expr<A>], aggs: &'p [Agg<A>]) -> Run<'s, Rel<A>, A> {
        let mut out = Rel::new_in(keys.len() + aggs.len(), self.alloc);
        let (starts, rows) = if keys.is_empty() {
            // One group, even of no rows.
            let mut starts = Vec::new_in(self.alloc);
            starts.push(0);
            starts.push(u32::try_from(rel.rows).unwrap_or(u32::MAX));
            let mut rows = Vec::with_capacity_in(rel.rows, self.alloc);
            for row in 0..rel.rows {
                rows.push(u32::try_from(row).unwrap_or(u32::MAX));
            }
            (starts, rows)
        } else {
            self.groups(rel, keys)?
        };
        let mut vals = Vec::new_in(self.alloc);
        let mut row_out = Vec::with_capacity_in(out.width(), self.alloc);
        let mut key = Vec::new_in(self.alloc);
        for g in 0..starts.len().saturating_sub(1) {
            let (lo, hi) = (starts.get(g).copied().unwrap_or(0), starts.get(g + 1).copied().unwrap_or(0));
            let members = rows.get(lo as usize..hi as usize).unwrap_or(&[]);
            row_out.clear();
            if let Some(&first) = members.first()
                && !keys.is_empty()
            {
                self.key_of(keys, rel, first as usize, &mut key)?;
                row_out.extend_from_slice(&key);
            }
            for agg in aggs {
                let mut acc = self.acc(agg);
                for &row in members {
                    self.feed(&mut acc, agg, rel, row, &mut vals)?;
                }
                let d = self.finish(&acc, agg, rel)?;
                row_out.push(d);
            }
            out.push(&row_out);
        }
        Ok(out)
    }

    /// `rel` with one column a window, each row's value in its partition and frame.
    pub(crate) fn window(&mut self, rel: Rel<A>, windows: &'p [Window<A>]) -> Run<'s, Rel<A>, A> {
        let mut out = rel;
        for window in windows {
            let column = self.window_column(&out, window)?;
            out.cols.push(column);
        }
        Ok(out)
    }

    fn window_column(&mut self, rel: &Rel<A>, window: &'p Window<A>) -> Run<'s, Vec<Datum, A>, A> {
        let mut column = Vec::from_elem_in(Datum::Null, rel.rows, self.alloc);
        let (starts, mut rows) = self.groups(rel, &window.partition)?;
        let mut order_key = Vec::new_in(self.alloc);
        let mut vals = Vec::new_in(self.alloc);
        let order_exprs = &window.order;
        for p in 0..starts.len().saturating_sub(1) {
            let (lo, hi) = (starts.get(p).copied().unwrap_or(0) as usize, starts.get(p + 1).copied().unwrap_or(0) as usize);
            let Some(part) = rows.get_mut(lo..hi) else { continue };
            self.sort_rows(rel, part, order_exprs)?;
            let part: &[u32] = part;
            let n = part.len();
            // Each position's order key, for peers.
            let width = order_exprs.len();
            order_key.clear();
            for &row in part {
                for key in order_exprs.iter() {
                    order_key.push(self.eval(&key.expr, &At { rel, row: row as usize })?);
                }
            }
            let store = &self.store;
            let peers = |i: usize, j: usize| -> bool {
                let a = order_key.get(i * width..i * width + width).unwrap_or(&[]);
                let b = order_key.get(j * width..j * width + width).unwrap_or(&[]);
                a.iter().zip(b).all(|(x, y)| store.same(*x, *y))
            };
            // The first and one past the last position of each position's peers.
            let mut peer_start = Vec::with_capacity_in(n, self.alloc);
            for i in 0..n {
                let s = if i > 0 && peers(i - 1, i) { peer_start.get(i - 1).copied().unwrap_or(i) } else { i };
                peer_start.push(s);
            }
            let mut peer_end = Vec::from_elem_in(n, n, self.alloc);
            for i in (0..n).rev() {
                let e = if i + 1 < n && peers(i, i + 1) { peer_end.get(i + 1).copied().unwrap_or(i + 1) } else { i + 1 };
                if let Some(slot) = peer_end.get_mut(i) {
                    *slot = e;
                }
            }
            let mut put = |i: usize, d: Datum| {
                if let Some(slot) = part.get(i).and_then(|row| column.get_mut(*row as usize)) {
                    *slot = d;
                }
            };
            match &window.func {
                WinFunc::RowNumber => {
                    for i in 0..n {
                        put(i, Datum::from_u64(u64::try_from(i + 1).unwrap_or(u64::MAX)));
                    }
                }
                WinFunc::Rank => {
                    for i in 0..n {
                        let s = peer_start.get(i).copied().unwrap_or(i);
                        put(i, Datum::from_u64(u64::try_from(s + 1).unwrap_or(u64::MAX)));
                    }
                }
                WinFunc::DenseRank => {
                    let mut rank = 0u64;
                    for i in 0..n {
                        if peer_start.get(i).copied() == Some(i) {
                            rank += 1;
                        }
                        put(i, Datum::from_u64(rank));
                    }
                }
                WinFunc::Shift { expr, offset, default } => {
                    for i in 0..n {
                        let target = i64::try_from(i).ok().and_then(|it| it.checked_add(*offset));
                        let target = target.and_then(|t| usize::try_from(t).ok()).filter(|t| *t < n);
                        let d = match (target, default) {
                            (Some(t), _) => {
                                let row = part.get(t).copied().unwrap_or(0) as usize;
                                self.eval(expr, &At { rel, row })?
                            }
                            (None, Some(default)) => {
                                let row = part.get(i).copied().unwrap_or(0) as usize;
                                self.eval(default, &At { rel, row })?
                            }
                            (None, None) => Datum::Null,
                        };
                        put(i, d);
                    }
                }
                WinFunc::Agg(agg) => {
                    let frame = window.frame;
                    let lo_of = |i: usize| match frame.start {
                        FrameBound::Unbounded => 0,
                        FrameBound::Rows(k) => offset(i, k, n),
                        FrameBound::Current if frame.rows => i,
                        FrameBound::Current => peer_start.get(i).copied().unwrap_or(i),
                    };
                    let hi_of = |i: usize| match frame.end {
                        FrameBound::Unbounded => n,
                        FrameBound::Rows(k) => offset(i, k, n).saturating_add(1).min(n),
                        FrameBound::Current if frame.rows => i + 1,
                        FrameBound::Current => peer_end.get(i).copied().unwrap_or(i + 1),
                    };
                    if frame.start == FrameBound::Unbounded && frame.end == FrameBound::Unbounded {
                        let mut acc = self.acc(agg);
                        for &row in part {
                            self.feed(&mut acc, agg, rel, row, &mut vals)?;
                        }
                        let d = self.finish(&acc, agg, rel)?;
                        for i in 0..n {
                            put(i, d);
                        }
                    } else if frame.start == FrameBound::Unbounded {
                        // A growing frame: fed once a row.
                        let mut acc = self.acc(agg);
                        let mut fed = 0usize;
                        for i in 0..n {
                            let hi = hi_of(i);
                            while fed < hi {
                                let row = part.get(fed).copied().unwrap_or(0);
                                self.feed(&mut acc, agg, rel, row, &mut vals)?;
                                fed += 1;
                            }
                            let d = self.finish(&acc, agg, rel)?;
                            put(i, d);
                        }
                    } else {
                        for i in 0..n {
                            let (lo, hi) = (lo_of(i), hi_of(i));
                            let mut acc = self.acc(agg);
                            for &row in part.get(lo..hi.max(lo)).unwrap_or(&[]) {
                                self.feed(&mut acc, agg, rel, row, &mut vals)?;
                            }
                            let d = self.finish(&acc, agg, rel)?;
                            put(i, d);
                        }
                    }
                }
            }
        }
        Ok(column)
    }
}

/// Position `i + k`, clamped to `0..=n`.
fn offset(i: usize, k: i64, n: usize) -> usize {
    let i = i64::try_from(i).unwrap_or(i64::MAX);
    let t = i.saturating_add(k).max(0);
    usize::try_from(t).unwrap_or(usize::MAX).min(n)
}
