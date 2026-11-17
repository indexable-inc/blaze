//! Cleanup paths (walk/unwind.rs): a call that may unwind while locals need drop is an
//! `invoke` whose `cleanup` block drops them, last declared first, then `resume`s; under
//! `panic = abort` no landing block is built.
//!
//! The Reference, "Panic" ([panic.unwind.destruction]): "as the panic traverses Rust frames,
//! live objects in those frames that implement Drop will have their drop methods called. Thus,
//! when normal execution resumes, no-longer-accessible objects will have been \"cleaned up\"
//! just as if they had gone out of scope normally"; and "if the panic handler is set to abort,
//! panicking will always terminate the process without destructors being run". "Destructors":
//! a scope's variables drop "in reverse order of declaration".

use super::*;

/// `fn main() { let a = G { x: 1 }; let b = G { x: 2 }; f(); }` built under `panic`; checked
/// and verified, not emitted (`emits` is the literal-word form, which refuses landing blocks).
fn two_locals_and_a_call(panic: crate::Panic) -> String {
    let mut w = TestWorld::new();
    w.panic = panic;
    w.func(1, "main", &[], UNIT);
    g_world(&mut w);
    w.func(33, "f", &[], UNIT);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let a = b.bind("a");
    let g1 = g_lit(&mut b, 1);
    stmts.push(Stmt::Let { pat: a, ty: None, init: Some(g1), else_: None });
    let second = b.bind("b");
    let g2 = g_lit(&mut b, 2);
    stmts.push(Stmt::Let { pat: second, ty: None, init: Some(g2), else_: None });
    let call = b.call("f", &[]);
    stmts.push(Stmt::Expr { expr: call, semi: true });
    let root = b.block(&stmts, None);
    let view = b.view(MAIN, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.sig(MAIN).expect("a signature");
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
        .unwrap_or_else(|e| panic!("the build failed: {e}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    blaze_lir::print(built.func.view())
}

/// The numbers of the `ssN` slot words in `line` (the printer's slot form), in order.
fn slots_in(line: &str) -> impl Iterator<Item = u32> + '_ {
    line.split(|c: char| !c.is_alphanumeric()).filter_map(|w| w.strip_prefix("ss")).filter_map(|n| n.parse().ok())
}

/// Under `Unwind`, `f()` is an `invoke` whose one `cleanup` block makes exactly two calls (the
/// drops of `b` then `a`: their slots in descending order, the slots being made in the order
/// of the `let`s) and ends in `resume`; under `Abort` there is no `invoke` and no landing
/// block. A cleanup that dropped in declaration order, dropped one local, or was built under
/// abort fails. Prints "cleanup: f() lands in a cleanup dropping slots [S_b, S_a], then resumes;
/// none under abort".
#[test]
fn a_call_that_may_unwind_drops_both_locals_in_reverse_order() {
    let text = two_locals_and_a_call(crate::Panic::Unwind);
    // f()'s, and b's drop at the block's end (its ladder lands dropping a: tests/unwind_ladder.rs).
    assert_eq!(text.lines().filter(|l| l.contains("invoke")).count(), 2, "two invokes, f() and b's drop:\n{text}");
    let mut lines = text.lines().skip_while(|l| !l.ends_with(" cleanup:"));
    assert!(lines.next().is_some(), "no cleanup block:\n{text}");
    let body: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(lines.take_while(|l| l.starts_with("  ")));
        v
    };
    assert_eq!(body.iter().filter(|l| l.contains("call")).count(), 2, "two drops in the cleanup:\n{text}");
    assert!(body.last().is_some_and(|l| l.contains("resume")), "the cleanup ends in resume:\n{text}");
    // Each drop's argument (`call refN, sN, (vK)`) traced to its `vK = slot_addr ssM` line,
    // wherever it is (the cleanup reuses addresses computed before the invoke).
    let mut dropped: Vec<u32, Global> = Vec::new_in(Global);
    for line in body.iter().filter(|l| l.contains("call")) {
        let arg = line.rsplit('(').next().and_then(|a| a.split(')').next()).unwrap_or("").trim();
        let def = format!("{arg} = ");
        let slot = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with(def.as_str()) && l.contains("slot_addr"))
            .and_then(|l| slots_in(l).next())
            .unwrap_or_else(|| panic!("the drop's argument {arg} is no slot_addr: {body:?}\n{text}"));
        dropped.push(slot);
    }
    assert_eq!(dropped.len(), 2, "the cleanup addresses two slots: {body:?}\n{text}");
    assert!(dropped[0] > dropped[1], "b (the later slot) drops first:\n{text}");
    let abort = two_locals_and_a_call(crate::Panic::Abort);
    assert!(!abort.contains("invoke") && !abort.contains(" cleanup:"), "no landing block under abort:\n{abort}");
    println!("cleanup: f() lands in a cleanup dropping slots {dropped:?}, then resumes; none under abort");
}

/// `fn main(i: usize) -> u32 { let g = G { x: 1 }; let a = [7u32, 8, 9]; a[i] }` under
/// `Unwind`: the bounds check's `panic_bounds_check` may unwind (core's `panic_bounds_check`
/// panics, and a panic unwinds past `g`), so it is an `invoke` whose cleanup drops `g` (one
/// call) and resumes. A bounds check called with a plain `call` (no landing block: `g` never
/// dropped on that path) fails. Prints "bounds: the out-of-range call lands in a cleanup with
/// 1 drop, then resumes".
#[test]
fn an_index_out_of_range_drops_the_live_values_when_it_unwinds() {
    let mut w = TestWorld::new();
    w.panic = crate::Panic::Unwind;
    w.func(1, "main", &[USIZE], U32);
    g_world(&mut w);
    w.bounds_fn();
    let mut b = BodyBuf::new();
    let i = b.bind("i");
    b.params.push(i);
    let g = b.bind("g");
    let g1 = g_lit(&mut b, 1);
    let a = b.bind("a");
    let e7 = b.int(7, SU32);
    let e8 = b.int(8, None);
    let e9 = b.int(9, None);
    let arr = b.array(&[e7, e8, e9]);
    let a_e = b.local(a);
    let i_e = b.local(i);
    let ai = b.index(a_e, i_e);
    let stmts = [Stmt::Let { pat: g, ty: None, init: Some(g1), else_: None }, Stmt::Let { pat: a, ty: None, init: Some(arr), else_: None }];
    let root = b.block(&stmts, Some(ai));
    let view = b.view(MAIN, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.sig(MAIN).expect("a signature");
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
        .unwrap_or_else(|e| panic!("the build failed: {e}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    let text = blaze_lir::print(built.func.view());
    assert_eq!(text.lines().filter(|l| l.contains("invoke")).count(), 1, "one invoke, for panic_bounds_check:\n{text}");
    let mut lines = text.lines().skip_while(|l| !l.ends_with(" cleanup:"));
    assert!(lines.next().is_some(), "no cleanup block:\n{text}");
    let body: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(lines.take_while(|l| l.starts_with("  ")));
        v
    };
    let drops = body.iter().filter(|l| l.contains("call")).count();
    assert_eq!(drops, 1, "the cleanup drops `g`:\n{text}");
    assert!(body.last().is_some_and(|l| l.contains("resume")), "the cleanup ends in resume:\n{text}");
    println!("bounds: the out-of-range call lands in a cleanup with {drops} drop, then resumes");
}
