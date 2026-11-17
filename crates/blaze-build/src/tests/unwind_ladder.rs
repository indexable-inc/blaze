//! Unwind ladders (walk/unwind_ladder.rs, UNWIND.md piece P6): a drop sequence carries its own
//! unwind edges, so a destructor that panics still drops the rest of the sequence.
//!
//! The Reference, "Destructors": "each variable or temporary is associated to a drop scope.
//! When control flow leaves a drop scope all variables associated to that scope are dropped in
//! reverse order of declaration"; the destructor of a type "consists of: If T: Drop, calling
//! <T as std::ops::Drop>::drop; Recursively running the destructor of all of its fields" (in
//! declaration order). "Panic" ([panic.unwind.destruction]): "as the panic traverses Rust
//! frames, live objects in those frames that implement Drop will have their drop methods
//! called". A panicking destructor partway through a sequence leaves the rest of it live, so
//! the unwind drops them.

use super::*;

/// The landing blocks of `text` (a printed LIR function) of kind `cleanup`: each block's lines.
fn cleanups(text: &str) -> Vec<Vec<&str, Global>, Global> {
    let mut out: Vec<Vec<&str, Global>, Global> = Vec::new_in(Global);
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if !line.ends_with(" cleanup:") {
            continue;
        }
        let mut body = Vec::new_in(Global);
        while let Some(l) = lines.peek().copied().filter(|l| l.starts_with("  ")) {
            body.push(l);
            lines.next();
        }
        out.push(body);
    }
    out
}

/// The value a call or invoke line passes last (`... (vK)`).
fn last_arg(line: &str) -> &str {
    line.rsplit('(').next().and_then(|a| a.split(')').next()).unwrap_or("").trim()
}

/// The `ssN` slot whose address value `v` is, from its `v = slot_addr ssN` line in `text`.
fn slot_of_value(text: &str, v: &str) -> u32 {
    let def = format!("{v} = ");
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with(def.as_str()) && l.contains("slot_addr"))
        .and_then(|l| l.split(|c: char| !c.is_alphanumeric()).find_map(|w| w.strip_prefix("ss")).and_then(|n| n.parse().ok()))
        .unwrap_or_else(|| panic!("{v} is no slot_addr:\n{text}"))
}

/// `fn main() { let a = G { x: 1 }; let b = G { x: 2 }; }` built under `panic`, checked and
/// verified (not emitted: the literal-word form refuses landing blocks).
fn two_locals(panic: crate::Panic) -> String {
    let mut w = TestWorld::new();
    w.panic = panic;
    w.func(1, "main", &[], UNIT);
    g_world(&mut w);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    let a = b.bind("a");
    let g1 = g_lit(&mut b, 1);
    stmts.push(Stmt::Let { pat: a, ty: None, init: Some(g1), else_: None });
    let second = b.bind("b");
    let g2 = g_lit(&mut b, 2);
    stmts.push(Stmt::Let { pat: second, ty: None, init: Some(g2), else_: None });
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

/// Under `Unwind`, the block end of `{ let a = ..; let b = ..; }` drops `b` by an `invoke`
/// whose one `cleanup` block drops `a` (one call, of `a`'s slot) and resumes; `a`'s own drop,
/// with nothing after it, is a plain call. Under `Abort`: no invoke, no landing block. A scope
/// exit of plain calls (today's code before P6: a panicking `b` leaks `a`) fails the invoke
/// count; a landing that dropped `b` again or nothing fails the slot check. Prints
/// "unwind_ladder: a panicking drop still drops the rest (scope: b's drop lands dropping a)".
#[test]
fn unwind_ladder_a_scope_exit_drops_the_rest_when_one_drop_panics() {
    let text = two_locals(crate::Panic::Unwind);
    let invokes: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(text.lines().filter(|l| l.contains("invoke")));
        v
    };
    assert_eq!(invokes.len(), 1, "one invoke, b's drop:\n{text}");
    let pads = cleanups(&text);
    assert_eq!(pads.len(), 1, "one cleanup block:\n{text}");
    let calls: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(pads[0].iter().copied().filter(|l| l.contains("call")));
        v
    };
    assert_eq!(calls.len(), 1, "the cleanup drops one value:\n{text}");
    assert!(pads[0].last().is_some_and(|l| l.contains("resume")), "the cleanup ends in resume:\n{text}");
    let invoked = slot_of_value(&text, last_arg(invokes[0]));
    let landed = slot_of_value(&text, last_arg(calls[0]));
    assert!(invoked > landed, "b (the later slot, {invoked}) is invoked, a ({landed}) dropped by its landing:\n{text}");
    let abort = two_locals(crate::Panic::Abort);
    assert!(!abort.contains("invoke") && !abort.contains(" cleanup:"), "no landing block under abort:\n{abort}");
    println!("unwind_ladder: a panicking drop still drops the rest (scope: b's drop lands dropping a)");
}

/// The drop glue of `struct P(G, G)` (key 10; with `own_drop`, its own `Drop` impl `H_DROP`)
/// built under `panic`, verified: its LIR text.
fn glue_of_p(panic: crate::Panic, own_drop: bool) -> String {
    let mut w = TestWorld::new();
    w.panic = panic;
    let g = g_world(&mut w);
    w.strukt(10, "P", &[("0", g), ("1", g)]);
    if own_drop {
        w.drops.push((ItemKey(10), H_DROP));
    }
    let mut b = BodyBuf::new();
    let p = b.bind("to_drop");
    b.params.push(p);
    let root = b.block(&[], None);
    let view = b.view(DROP_IN_PLACE, root);
    let mut cx = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        other => panic!("the check did not finish: {:?}", other.err()),
    };
    let mut ground = PortableTys::new_in(Alloc::global());
    let pt = ground.intern(PNode::Adt(ItemKey(10), List::EMPTY)).expect("intern");
    let sig = w.sig(DROP_IN_PLACE).expect("a signature");
    let mut subst = Subst { args: &[pt], ground: &mut ground, scratch: Alloc::global() };
    let built = crate::build_drop_glue(&view, &checked, &sig, &mut subst, &TestAdts { world: &w, body: &b }, Global)
        .unwrap_or_else(|e| panic!("{e}"));
    blaze_lir::verify(built.func.view()).unwrap_or_else(|e| panic!("does not verify: {e}"));
    blaze_lir::print(built.func.view())
}

/// Under `Unwind`, the glue of `struct P(G, G)`: the first field's drop is an `invoke` whose
/// one `cleanup` block drops the second field (one call, another address than the invoke's)
/// and resumes; the second field's drop is a plain call. With `P`'s own `Drop`: the impl's call
/// is an invoke too, whose landing drops both fields (two calls); two invokes in all. Under
/// `Abort`: no invoke. Glue of plain calls (a panicking first field leaks the second) fails.
/// Prints "unwind_ladder: a panicking drop still drops the rest (glue: field 0 lands dropping
/// field 1; P's Drop lands dropping both)".
#[test]
fn unwind_ladder_drop_glue_drops_the_remaining_fields_when_one_panics() {
    let text = glue_of_p(crate::Panic::Unwind, false);
    let invokes: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(text.lines().filter(|l| l.contains("invoke")));
        v
    };
    assert_eq!(invokes.len(), 1, "one invoke, field 0's drop:\n{text}");
    let pads = cleanups(&text);
    assert_eq!(pads.len(), 1, "one cleanup block:\n{text}");
    let calls: Vec<&str, Global> = {
        let mut v = Vec::new_in(Global);
        v.extend(pads[0].iter().copied().filter(|l| l.contains("call")));
        v
    };
    assert_eq!(calls.len(), 1, "the cleanup drops field 1:\n{text}");
    assert_ne!(last_arg(calls[0]), last_arg(invokes[0]), "the landing drops the other field, not the one that panicked:\n{text}");
    assert!(pads[0].last().is_some_and(|l| l.contains("resume")), "the cleanup ends in resume:\n{text}");
    let with_impl = glue_of_p(crate::Panic::Unwind, true);
    assert_eq!(with_impl.lines().filter(|l| l.contains("invoke")).count(), 2, "P's Drop and field 0 invoked:\n{with_impl}");
    let impl_pads = cleanups(&with_impl);
    let mut sizes: Vec<usize, Global> = Vec::new_in(Global);
    sizes.extend(impl_pads.iter().map(|pad| pad.iter().filter(|l| l.contains("call")).count()));
    sizes.sort_unstable();
    assert_eq!(sizes.as_slice(), &[1, 2], "P's Drop lands dropping both fields, field 0 dropping field 1:\n{with_impl}");
    let abort = glue_of_p(crate::Panic::Abort, true);
    assert!(!abort.contains("invoke") && !abort.contains(" cleanup:"), "no landing block under abort:\n{abort}");
    println!("unwind_ladder: a panicking drop still drops the rest (glue: field 0 lands dropping field 1; P's Drop lands dropping both)");
}
