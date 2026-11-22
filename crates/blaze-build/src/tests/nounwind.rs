//! Nounwind bodies (walk/nounwind.rs): in a body whose ABI does not permit unwinding, a call
//! that may unwind lands on the body's one `terminate` block, which calls core's
//! `panic_cannot_unwind`; under `panic = abort`, and in a Rust fn, the call stays plain.
//!
//! The Reference, "Functions", the unwinding table: "panic=unwind | non-unwinding | abort", and
//! "The \"non-unwinding\" ABI category refers to all other ABI strings, including \"C\"";
//! [items.fn.extern.abort]: "With panic=unwind, when a panic is turned into an abort by a
//! non-unwinding ABI boundary, either no destructors (Drop calls) will run, or all destructors
//! up until the ABI boundary will run." The pinned library, core/src/panicking.rs:346-351:
//! `#[lang = "panic_cannot_unwind"] #[rustc_nounwind] fn panic_cannot_unwind() -> !`
//! ("Panics because we cannot unwind out of a function").

use super::*;

/// The `panic_cannot_unwind` lang fn of this world.
const CANNOT_UNWIND: ItemKey = ItemKey(140);

/// `[extern "C"] fn main() { [let a = G { x: 1 };] f(); }` built under `panic`, checked and
/// verified (not emitted): its LIR text and the build.
fn one_call(panic: crate::Panic, c_abi: bool, local: bool) -> (String, Built<Global>) {
    let mut w = TestWorld::new();
    w.panic = panic;
    w.func(1, "main", &[], UNIT);
    if c_abi {
        w.c_abi.push(MAIN);
    }
    g_world(&mut w);
    w.func(140, "panic_cannot_unwind", &[], PNode::Never);
    w.cannot_unwind = Some(CANNOT_UNWIND);
    w.func(33, "f", &[], UNIT);
    let mut b = BodyBuf::new();
    let mut stmts: Vec<Stmt, Global> = Vec::new_in(Global);
    if local {
        let a = b.bind("a");
        let g1 = g_lit(&mut b, 1);
        stmts.push(Stmt::Let { pat: a, ty: None, init: Some(g1), else_: None });
    }
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
    (blaze_lir::print(built.func.view()), built)
}

/// The words of `line` split at every character that is not alphanumeric.
fn words(line: &str) -> impl Iterator<Item = &str> + '_ {
    line.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty())
}

/// Under `Unwind`, `extern "C" fn main() { f(); }` (and the same with a live `G` local) has
/// one `invoke` for `f()`, and with the local one for its drop, whose unwind block is the one `terminate` block; that
/// block makes one call, of `panic_cannot_unwind`, and ends in `unreachable`; no `cleanup`
/// block is built (no destructor runs: [items.fn.extern.abort]); the function's header says
/// `nounwind`. Under `Abort` the call is plain (no invoke, no landing block). A Rust fn's call
/// with nothing live is plain under `Unwind`, and the Rust fn is not `nounwind`. A build that
/// left the extern body's call plain, built a cleanup there, or landed elsewhere fails.
/// Prints "nounwind: an extern \"C\" body's call lands on terminate".
#[test]
fn an_extern_c_bodys_call_lands_on_terminate() {
    for local in [false, true] {
        let (text, built) = one_call(crate::Panic::Unwind, true, local);
        let header = text.lines().next().unwrap_or("");
        assert!(words(header).any(|w| w == "nounwind"), "the extern \"C\" body is nounwind (local {local}):\n{text}");
        let invokes: Vec<&str, Global> = {
            let mut v = Vec::new_in(Global);
            v.extend(text.lines().filter(|l| words(l).any(|w| w == "invoke")));
            v
        };
        // With the local, its drop at the block's end lands on terminate too (walk/unwind_ladder.rs `drop_call`).
        assert_eq!(invokes.len(), 1 + usize::from(local), "f()'s invoke, and a's drop's (local {local}):\n{text}");
        assert!(!text.lines().any(|l| l.ends_with(" cleanup:")), "no cleanup in a nounwind body (local {local}):\n{text}");
        let terminates: Vec<&str, Global> = {
            let mut v = Vec::new_in(Global);
            v.extend(text.lines().filter(|l| l.ends_with(" terminate:")));
            v
        };
        assert_eq!(terminates.len(), 1, "one terminate block (local {local}):\n{text}");
        let name = terminates[0].split(' ').next().unwrap_or("");
        assert!(invokes.iter().all(|i| words(i).any(|w| w == name)), "every invoke's unwind edge is {name} (local {local}):\n{text}");
        let body: Vec<&str, Global> = {
            let mut v = Vec::new_in(Global);
            v.extend(text.lines().skip_while(|l| !l.ends_with(" terminate:")).skip(1).take_while(|l| l.starts_with("  ")));
            v
        };
        let n = built
            .refs
            .iter()
            .position(|r| r.item == CANNOT_UNWIND)
            .unwrap_or_else(|| panic!("no reference to panic_cannot_unwind (local {local}):\n{text}"));
        let reference = format!("ref{n}");
        let calls: Vec<&&str, Global> = {
            let mut v = Vec::new_in(Global);
            v.extend(body.iter().filter(|l| words(l).any(|w| w == "call")));
            v
        };
        assert_eq!(calls.len(), 1, "the terminate block makes one call (local {local}):\n{text}");
        assert!(words(calls[0]).any(|w| w == reference), "it calls panic_cannot_unwind, {reference} (local {local}):\n{text}");
        assert!(body.last().is_some_and(|l| l.contains("unreachable")), "the terminate block ends in unreachable (local {local}):\n{text}");
    }
    let (abort, _) = one_call(crate::Panic::Abort, true, false);
    assert!(!abort.contains("invoke") && !abort.contains(" terminate:"), "a plain call under abort:\n{abort}");
    let (rust, _) = one_call(crate::Panic::Unwind, false, false);
    assert!(!rust.contains("invoke") && !rust.contains(" terminate:"), "a Rust fn's call with nothing live is plain:\n{rust}");
    assert!(!words(rust.lines().next().unwrap_or("")).any(|w| w == "nounwind"), "a Rust fn is not nounwind:\n{rust}");
    println!("nounwind: an extern \"C\" body's call lands on terminate");
}
