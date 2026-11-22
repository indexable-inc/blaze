//! An irredundant sum of products (Minato, "Fast generation of prime-irredundant covers from
//! binary decision diagrams", 1993, after Morreale): the short printed form of a condition.
//!
//! [`Cover::compute`] takes a lower and an upper bound, `lower <= upper`, and finds a cover `C`
//! with `lower <= C <= upper` in which no cube can be dropped. `compute(f, f)` covers exactly
//! `f`; `compute(f & valid, f | !valid)` may use the configurations no request has as don't-care,
//! the shortest true statement a finding can make. The recursion of the paper runs on an
//! explicit stack; a cube is a persistent list of literals, so prefixing every cube of a
//! sub-cover with `x` or `!x` is one node each.

#![blaze::sans_io]

use core::fmt;

use blaze_int::{Int, idx};
use blaze_own::{AVec, Alloc, planted::Planted};

use crate::bdd::{Cond, PlantedBdd, Var};
use crate::error::CondError;
use crate::plant::CondPlant;
use crate::space::{Space, VarKind};

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
enum Frame {
    Call { l: Cond, u: Cond },
    After0 { x: u16, l0: Cond, l1: Cond, u0: Cond, u1: Cond, start: u32 },
    After1 { x: u16, l0: Cond, l1: Cond, u0: Cond, u1: Cond, start: u32, mid: u32, r0: Cond },
    After2 { x: u16, start: u32, mid: u32, end1: u32, r0: Cond, r1: Cond },
}

/// A sum of products: cubes, each a list of literals in the space's order.
pub struct Cover<'a> {
    /// Each cube's first literal, or `NONE` for the empty cube (true).
    cubes: AVec<'a, u32>,
    /// Literals: `var << 1 | positive`, and the next literal of the cube.
    lits: AVec<'a, (u32, u32)>,
    frames: AVec<'a, Frame>,
    results: AVec<'a, Cond>,
}

fn spush<T>(v: &mut AVec<'_, T>, x: T) -> Result<(), CondError> {
    v.try_push(x).map_err(|r| CondError::Alloc(r.why))
}

impl<'a> Cover<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Cover<'a> {
        Cover {
            cubes: AVec::new_in(alloc),
            lits: AVec::new_in(alloc),
            frames: AVec::new_in(alloc),
            results: AVec::new_in(alloc),
        }
    }

    /// How many cubes.
    pub fn len(&self) -> usize {
        self.cubes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cubes.is_empty()
    }

    /// Cube `at`'s literals, `(variable, positive)`, in the space's order.
    pub fn cube(&self, at: usize) -> impl Iterator<Item = (Var, bool)> + '_ {
        let mut next = self.cubes[at];
        core::iter::from_fn(move || {
            if next == NONE {
                return None;
            }
            let (lit, after) = self.lits[idx(next)];
            next = after;
            Some((Var::new((lit >> 1).narrow_or_stop::<u16>()), lit & 1 == 1))
        })
    }

    fn pop(&mut self) -> Cond {
        let Some(r) = self.results.pop() else { unreachable!("a cover's step leaves its result") };
        r
    }

    fn prefix(&mut self, cubes: core::ops::Range<u32>, x: u16, positive: bool) -> Result<(), CondError> {
        for c in cubes {
            let head = self.cubes[idx(c)];
            let at = self.lits.len().narrow_or_stop::<u32>();
            spush(&mut self.lits, (u32::from(x) << 1 | u32::from(positive), head))?;
            self.cubes[idx(c)] = at;
        }
        Ok(())
    }

    /// Finds an irredundant cover between `lower` and `upper` (cleared first), in at most `most`
    /// steps and cubes; returns the cover's function.
    pub fn compute(
        &mut self,
        bdd: &mut PlantedBdd<'_, impl Planted<CondPlant>>,
        lower: Cond,
        upper: Cond,
        most: u32,
    ) -> Result<Cond, CondError> {
        self.cubes.clear();
        self.lits.clear();
        self.frames.clear();
        self.results.clear();
        let mut steps = 0u32;
        spush(&mut self.frames, Frame::Call { l: lower, u: upper })?;
        while let Some(frame) = self.frames.pop() {
            match frame {
                Frame::Call { l, u } => {
                    steps += 1;
                    if steps > most || self.cubes.len() >= idx(most) {
                        return Err(CondError::CoverCap { most });
                    }
                    if l.is_false() {
                        spush(&mut self.results, Cond::FALSE)?;
                    } else if u.is_true() {
                        spush(&mut self.cubes, NONE)?;
                        spush(&mut self.results, Cond::TRUE)?;
                    } else {
                        let x = bdd.level_of(l).min(bdd.level_of(u));
                        let (l0, l1) = bdd.cofactors(l, x);
                        let (u0, u1) = bdd.cofactors(u, x);
                        let start = self.cubes.len().narrow_or_stop::<u32>();
                        let a = bdd.and(l0, u1.not())?;
                        spush(&mut self.frames, Frame::After0 { x, l0, l1, u0, u1, start })?;
                        spush(&mut self.frames, Frame::Call { l: a, u: u0 })?;
                    }
                }
                Frame::After0 { x, l0, l1, u0, u1, start } => {
                    let r0 = self.pop();
                    let mid = self.cubes.len().narrow_or_stop::<u32>();
                    let b = bdd.and(l1, u0.not())?;
                    spush(&mut self.frames, Frame::After1 { x, l0, l1, u0, u1, start, mid, r0 })?;
                    spush(&mut self.frames, Frame::Call { l: b, u: u1 })?;
                }
                Frame::After1 { x, l0, l1, u0, u1, start, mid, r0 } => {
                    let r1 = self.pop();
                    let end1 = self.cubes.len().narrow_or_stop::<u32>();
                    let a = bdd.and(l0, r0.not())?;
                    let b = bdd.and(l1, r1.not())?;
                    let ls = bdd.or(a, b)?;
                    let us = bdd.and(u0, u1)?;
                    spush(&mut self.frames, Frame::After2 { x, start, mid, end1, r0, r1 })?;
                    spush(&mut self.frames, Frame::Call { l: ls, u: us })?;
                }
                Frame::After2 { x, start, mid, end1, r0, r1 } => {
                    let rs = self.pop();
                    self.prefix(start..mid, x, false)?;
                    self.prefix(mid..end1, x, true)?;
                    let xv = bdd.var(Var::new(x))?;
                    let split = bdd.ite(xv, r1, r0)?;
                    let r = bdd.or(split, rs)?;
                    spush(&mut self.results, r)?;
                }
            }
        }
        Ok(self.pop())
    }

    /// The cover in Rust's cfg syntax: `false`, `true`, a literal, `all(..)`, `any(..)`. A
    /// feature of `home` is `feature = "x"`, another package's `feature = "dep/x"`.
    pub fn write_cfg(&self, space: &Space<'_>, home: &str, out: &mut impl fmt::Write) -> fmt::Result {
        match self.cubes.len() {
            0 => return out.write_str("false"),
            1 => return self.write_cube(0, space, home, out),
            _ => {}
        }
        out.write_str("any(")?;
        for at in 0..self.cubes.len() {
            if at > 0 {
                out.write_str(", ")?;
            }
            self.write_cube(at, space, home, out)?;
        }
        out.write_str(")")
    }

    fn write_cube(&self, at: usize, space: &Space<'_>, home: &str, out: &mut impl fmt::Write) -> fmt::Result {
        let n = self.cube(at).count();
        match n {
            0 => return out.write_str("true"),
            1 => {}
            _ => out.write_str("all(")?,
        }
        for (i, (var, positive)) in self.cube(at).enumerate() {
            if i > 0 {
                out.write_str(", ")?;
            }
            if !positive {
                out.write_str("not(")?;
            }
            write_atom(space, var, home, out)?;
            if !positive {
                out.write_str(")")?;
            }
        }
        if n > 1 {
            out.write_str(")")?;
        }
        Ok(())
    }
}

/// A variable as a cfg atom.
pub fn write_atom(space: &Space<'_>, var: Var, home: &str, out: &mut impl fmt::Write) -> fmt::Result {
    let name = space.name(var);
    if space.kind(var) == VarKind::Feature {
        let full = name.trim_start_matches("feature:");
        let short = full.strip_prefix(home).and_then(|r| r.strip_prefix('/')).unwrap_or(full);
        return write!(out, "feature = \"{short}\"");
    }
    let atom = name.trim_start_matches("cfg:");
    match atom.split_once('=') {
        Some((key, value)) => write!(out, "{key} = {value}"),
        None => out.write_str(atom),
    }
}
