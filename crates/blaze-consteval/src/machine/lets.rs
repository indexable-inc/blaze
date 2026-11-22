//! `let` in a condition: `if let`, `while let` (the lowering's `loop { if COND BODY else {
//! break } }`, blaze-lower desugar.rs `while_loop`, so it is an `if let` here) and let chains,
//! compiled into the evaluator's code as blaze-build walk/lets.rs builds them. Rules from the
//! Reference:
//! - "If expressions", "if let": "If the value of the scrutinee matches the pattern, the
//!   corresponding block will execute. Otherwise, flow proceeds to the following `else` block
//!   if it exists."
//! - "If expressions", chains of conditions (edition 2024): "Multiple condition operands can
//!   be separated with `&&`"; "each operand is evaluated from left-to-right until an operand
//!   evaluates as `false` or a `let` match fails, in which case the subsequent operands are
//!   not evaluated"; "The bindings of each pattern are put into scope to be available for the
//!   next condition operand and the consequent block."
//!
//! A condition that is a `let` or an `&&` is taken apart into its links in order, each on the
//! compile's own task stack (no recursion): a `let`'s scrutinee is compiled as a place, as a
//! `match`'s is, and its pattern tested by `Machine::pattern` with every failed test to the
//! one fail list; a `bool` link branches to the same list when false. The fail list is the
//! `else` block. An `&&` with no `let` in it gives the same result this way as through `bool`
//! short-circuiting ("Lazy boolean operators"), so no scan for a `let` is needed first. A
//! `let` anywhere else (inside parentheses, under `||`) is not a condition operand and stays
//! refused by name in `raw`.
//!
//! The pinned library needs this in constants: core's `ilog10` is `if let Some(log) =
//! self.checked_ilog10() { log } else { .. }` (core/src/num/uint_macros.rs), and the array
//! lengths `const MAX_DEC_N: usize = $T::MAX.ilog10() as usize + 1;` (core/src/fmt/num.rs) and
//! `const SIZE: usize = $unsigned::MAX.ilog10() as usize + 1;` (alloc/src/string.rs) call it.

use core::alloc::Allocator;

use blaze_typeck::{BinOp, Expr, ExprId};

use super::super::{Machine, Mode, R, malformed};
use super::{C, Cc, Code, Kind, Op};
use crate::ConstWorld;
use crate::layout::Layout;

impl<'w, 'l, 'g, 't, 'wk, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> Machine<'w, 'l, 'g, 't, 'wk, 'a, W, L, K> {
    /// Whether `if` condition `cond` is compiled by links: a `let`, or an `&&` chain.
    pub(super) fn cond_by_links(&self, cc: &Cc<'w, '_>, cond: ExprId) -> R<'a, bool> {
        Ok(matches!(self.expr_c(cc, cond)?, Expr::Let { .. } | Expr::Binary { op: BinOp::And, .. }))
    }

    /// `if` expression `e` whose condition `cond` is compiled by links: the links, then the
    /// consequent, then the end (`C::IfLetEnd`), which places the `else` block at the fail list.
    pub(super) fn if_let(&mut self, cc: &mut Cc<'w, '_>, e: ExprId, cond: ExprId) -> R<'a, ()> {
        let Expr::If { then, .. } = self.expr_c(cc, e)? else { return Err(malformed("an `if let` that is not an `if`")) };
        let dst = self.reg(cc, Kind::V);
        let fail = self.list(cc);
        cc.tasks.push(C::IfLetEnd { e, dst, fail });
        cc.tasks.push(C::Eval(then, Mode::Value));
        cc.tasks.push(C::CondLink { e: cond, fail });
        Ok(())
    }

    /// One task of a condition by links (`C::CondLink`, `C::CondLet`, `C::CondBool`,
    /// `C::IfLetEnd`).
    pub(super) fn let_c<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, task: C) -> R<'a, ()> {
        match task {
            C::CondLink { e, fail } => match self.expr_c(cc, e)? {
                // Left before right: the right is pushed first, so it is taken after.
                Expr::Binary { op: BinOp::And, lhs, rhs } => {
                    cc.tasks.push(C::CondLink { e: rhs, fail });
                    cc.tasks.push(C::CondLink { e: lhs, fail });
                }
                Expr::Let { pat, init } => {
                    // As a `match` scrutinee: a register local is matched through a copy unless
                    // the pattern binds by reference.
                    let mode = if !self.binds_by_ref(cc, pat) && self.reg_local(cc, init).is_some() { Mode::Value } else { Mode::Place };
                    cc.tasks.push(C::CondLet { e, fail });
                    cc.tasks.push(C::Eval(init, mode));
                }
                _ => {
                    cc.tasks.push(C::CondBool { e, fail });
                    cc.tasks.push(C::Eval(e, Mode::Value));
                }
            },
            C::CondLet { e, fail } => {
                let Expr::Let { pat, init } = self.expr_c(cc, e)? else { return Err(malformed("a condition `let` task")) };
                let s = self.pop_r(cc)?;
                let ty = self.final_ty_c(cc, init)?;
                let place = self.as_place(cc, code, s, ty)?;
                self.pattern(cc, code, pat, place, ty, fail)?;
            }
            C::CondBool { fail, .. } => {
                let g = self.pop_r(cc)?;
                let j = self.emit(code, cc, Op::Branch { cond: g, when: false, to: u32::MAX });
                self.to_list(cc, fail, j);
            }
            C::IfLetEnd { e, dst, fail } => {
                let Expr::If { else_, .. } = self.expr_c(cc, e)? else { return Err(malformed("an `if let` end task")) };
                let t = self.pop_r(cc)?;
                self.emit(code, cc, Op::Move { dst, src: t });
                let end = self.jump(code, cc);
                self.patch(code, cc, fail);
                match else_ {
                    Some(x) => {
                        cc.tasks.push(C::IfElse { e, dst, jump: end });
                        cc.tasks.push(C::Eval(x, Mode::Value));
                    }
                    None => {
                        self.emit(code, cc, Op::ImmZst { dst });
                        self.patch_one(code, end);
                        cc.rs.push(dst);
                    }
                }
            }
            _ => return Err(malformed("a task that is not a condition link's")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::string::String;

    use crate::fixture::{World, int_of};

    fn eval_int(w: &mut World, name: &str) -> Result<u128, String> {
        w.eval(name).map(int_of).map_err(|e| std::format!("{:?}", e.refusal))
    }

    /// The Reference, "If expressions", "if let": "If the value of the scrutinee matches the
    /// pattern, the corresponding block will execute. Otherwise, flow proceeds to the
    /// following `else` block if it exists." Chains of conditions: "each operand is evaluated
    /// from left-to-right until an operand evaluates as `false` or a `let` match fails, in
    /// which case the subsequent operands are not evaluated"; "The bindings of each pattern are
    /// put into scope to be available for the next condition operand and the consequent
    /// block." `while let` is the lowering's `loop { if let .. else { break } }`.
    ///
    /// `M` is the pinned library's array length `const MAX_DEC_N: usize = $T::MAX.ilog10() as
    /// usize + 1;` (core/src/fmt/num.rs) at `u8`, through a fixture `ilog10` written as core's
    /// is (core/src/num/uint_macros.rs: `if let Some(log) = self.checked_ilog10() { log } else
    /// { .. }`): 255 has three decimal digits, so `M` is 3. The fixture has no generic enums,
    /// so `Opt` stands for `Option<u32>`.
    ///
    /// Expected: `test machine::code::lets::tests::if_let_and_let_chains_in_constants ... ok`.
    /// Without the fix every constant here is refused with "`let` in a condition in a constant
    /// is not evaluated yet".
    #[test]
    fn if_let_and_let_chains_in_constants() {
        let mut w = World::new();
        w.panic_fn("panic_display", "&str");
        w.enum_("Opt", None, &[("Has", &["u32"], None), ("No", &[], None)]);
        w.const_fn(
            "checked_ilog10",
            &[("n", "u8")],
            "Opt",
            "{ if n == 0 { Opt::No } else { let mut l = 0u32; let mut m = n; while m >= 10 { m = m / 10; l += 1; } Opt::Has(l) } }",
        );
        w.const_fn(
            "ilog10",
            &[("n", "u8")],
            "u32",
            "{ if let Opt::Has(log) = checked_ilog10(n) { log } else { panic_display(\"argument of integer logarithm must be positive\") } }",
        );
        w.konst("L", "u32", "{ let x: Opt = Opt::Has(3); if let Opt::Has(v) = x { v } else { 0 } }");
        w.konst("N", "u32", "{ let x: Opt = Opt::No; if let Opt::Has(v) = x { v } else { 5 } }");
        w.konst(
            "CH",
            "u32",
            "{ let x = Opt::Has(3); let y = Opt::Has(4); if let Opt::Has(a) = x && a > 2 && let Opt::Has(b) = y { a * 10 + b } else { 0 } }",
        );
        w.konst(
            "CF",
            "u32",
            "{ let x = Opt::Has(3); let y = Opt::Has(4); if let Opt::Has(a) = x && a > 3 && let Opt::Has(b) = y { a * 10 + b } else { 9 } }",
        );
        w.konst(
            "CN",
            "u32",
            "{ let x = Opt::Has(3); let y = Opt::No; if let Opt::Has(a) = x && let Opt::Has(b) = y { a + b } else { 8 } }",
        );
        w.konst(
            "W",
            "u32",
            "{ let mut n = 0u32; let mut x = Opt::Has(3); while let Opt::Has(k) = x { n += k; x = if k == 0 { Opt::No } else { Opt::Has(k - 1) }; } n }",
        );
        w.konst("M", "usize", "ilog10(255) as usize + 1");
        w.konst("ONE", "usize", "ilog10(9) as usize + 1");
        w.build();
        assert_eq!(eval_int(&mut w, "L"), Ok(3), "`if let` whose pattern matches takes the consequent");
        assert_eq!(eval_int(&mut w, "N"), Ok(5), "`if let` whose pattern fails takes the `else`");
        assert_eq!(eval_int(&mut w, "CH"), Ok(34), "a chain whose links all hold, bindings in scope");
        assert_eq!(eval_int(&mut w, "CF"), Ok(9), "a chain whose `bool` link is false takes the `else`");
        assert_eq!(eval_int(&mut w, "CN"), Ok(8), "a chain whose second `let` fails takes the `else`");
        assert_eq!(eval_int(&mut w, "W"), Ok(6), "`while let` runs while the pattern matches: 3 + 2 + 1 + 0");
        assert_eq!(eval_int(&mut w, "M"), Ok(3), "`u8::MAX.ilog10() as usize + 1`");
        assert_eq!(eval_int(&mut w, "ONE"), Ok(1), "`9u8.ilog10() as usize + 1`");
    }
}
