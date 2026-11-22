//! An `async fn` whose poll may unwind, built under `panic = unwind` (UNWIND.md P7; ASYNC.md
//! P5, D-A5). A child of `coroutines_emit`, so it builds in that file's world (`D`, `mk_d`,
//! `twice`, `drop_d`, `drop_in_place`) with that file's helpers; the world here also declares
//! `g` and the library's two `panic_const_async_fn_resumed*` lang fns, and its `Adts` answers
//! `Panic::Unwind`.
//!
//! ```text
//! async fn a() -> u64 { let d = mk_d(); let r: u64 = twice().await; g(); r }
//! ```
//!
//! The rules:
//! - The Reference, "Panic" ([panic.unwind.destruction]): "When a panic occurs, the unwind
//!   handler \"unwinds\" Rust frames ... This means that as the panic traverses Rust frames,
//!   live objects in those frames that implement Drop will have their drop methods called."
//!   So a call made while `d` is live (`twice()`, its `into_future`, each `poll` of it, `g()`)
//!   lands in a cleanup that drops `d` exactly once; a call made before `d` exists (`mk_d()`,
//!   the call that makes it) lands in a cleanup that drops nothing: dropping `d` there would
//!   drop a value that was never made. `d` is live across the suspend, so it is kept in the
//!   state, and its drop takes the state pointer or an offset of it.
//! - The pinned library, core/src/panicking.rs:202: `panic_const_async_fn_resumed_panic =
//!   "`async fn` resumed after panicking"` (and :198, `panic_const_async_fn_resumed =
//!   "`async fn` resumed after completion"): a future whose poll unwound is poisoned, and a
//!   later poll panics with that message. So EVERY cleanup, whatever it drops, stores the tag
//!   `Panicked` (2) at the state's tag right before its `resume` (blaze-lir
//!   coroutine_unwind.rs, "Poisoning"), and `poll`'s switch sends tag 2 to a block calling
//!   `panic_const_async_fn_resumed_panic` (blaze-build walk/coroutine_panics.rs).
//! - [panic.unwind.destruction] again, with `abort`: "if the panic handler is set to abort,
//!   panicking will always terminate the process without destructors being run": under
//!   `Panic::Abort` no landing block is built.
//!
//! The first version asserted that EVERY cleanup in `poll` drops `d`; that was wrong for
//! correct LIR: the cleanup of `mk_d()` runs before `d` exists and only poisons (the build
//! printed `[v6 = param ptr, v7 = iconst i8 2, store v7, v1, 64, resume v6]` for it). This
//! version asserts per `invoke`, by its callee, what its own cleanup path must do.

use core::cell::{Cell, RefCell};
use std::{alloc::Global, println, string::String, vec::Vec as StdVec};

use blaze_own::Alloc;
use blaze_typeck::{Adt, Capture, ExprId, InferCtx, ItemKey, Needs, Outcome, PTy, PortableTys};

use super::{FxAdts, Prog, async_body, async_fn, call_await, let_u64, open_world};
use crate::layout::LayoutFacts;
use crate::{Abi, Adts, AsyncItems, BuildError, Panic, Subst};

/// The pinned core's lang fn for a poll of a `Returned` async state (panicking.rs:198).
const RESUMED: &str = "panic_const_async_fn_resumed";
/// The pinned core's lang fn for a poll of a `Panicked` async state (panicking.rs:202).
const RESUMED_PANIC: &str = "panic_const_async_fn_resumed_panic";

/// `coroutines_emit`'s `Adts`, answering `panic` as asked and the two lang fns above, and
/// noting which of the two the build asked for.
struct UnwindAdts<'r, 'a> {
    inner: &'r FxAdts<'a>,
    panic: Panic,
    resumed: ItemKey,
    resumed_panic: ItemKey,
    /// Whether the build asked for [`RESUMED`], and for [`RESUMED_PANIC`].
    asked: Cell<(bool, bool)>,
}

impl Adts for UnwindAdts<'_, '_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        Adts::adt(self.inner, key)
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        Adts::overflow_checks(self.inner)
    }

    fn ub_checks(&self) -> crate::UbChecks {
        Adts::ub_checks(self.inner)
    }

    fn facts(&self) -> &dyn LayoutFacts {
        Adts::facts(self.inner)
    }

    fn int_lit(&self, item: ItemKey, expr: ExprId) -> Option<u128> {
        Adts::int_lit(self.inner, item, expr)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        Adts::abi(self.inner, item)
    }

    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        Adts::drop_impl(self.inner, ty, ground)
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        let (returned, panicked) = self.asked.get();
        if name == RESUMED {
            self.asked.set((true, panicked));
            return Some(self.resumed);
        }
        if name == RESUMED_PANIC {
            self.asked.set((returned, true));
            return Some(self.resumed_panic);
        }
        Adts::lang_fn(self.inner, name)
    }

    fn future_poll(&self) -> Option<ItemKey> {
        Adts::future_poll(self.inner)
    }

    fn async_items(&self) -> Option<AsyncItems> {
        Adts::async_items(self.inner)
    }

    fn panic(&self) -> Panic {
        self.panic
    }
}

/// The items a cleanup or a finished-state block may call, by what they are.
#[derive(Clone, Copy)]
struct Keys {
    /// `fn mk_d() -> D`: the call that makes `d`.
    mk_d: ItemKey,
    /// `D`'s `Drop` fn, and the library's `drop_in_place`: either is the drop of `d`.
    drop_d: ItemKey,
    dip: ItemKey,
    /// `fn g()`: called after the suspend, with `d` in the state.
    g: ItemKey,
    /// The two lang fns of a finished state.
    resumed: ItemKey,
    resumed_panic: ItemKey,
}

/// What a build of `a`'s coroutine gives: the text of the pass's `poll` (the body), its
/// function references' callee items by place, the world's items, and whether the build asked
/// for [`RESUMED`] and [`RESUMED_PANIC`].
struct Made {
    poll: String,
    refs: StdVec<ItemKey>,
    keys: Keys,
    asked: (bool, bool),
}

/// Checks `a`'s body, then builds and splits its coroutine under `panic`.
fn build_under(panic: Panic) -> Result<Made, BuildError> {
    let mut w = open_world();
    let a = async_fn(&mut w, "a", "a_future");
    let unit = w.fx.unit();
    let never = w.fx.never();
    let g = w.fx.func("g", 0, &[], unit, StdVec::new());
    let resumed = w.fx.func(RESUMED, 0, &[], never, StdVec::new());
    let resumed_panic = w.fx.func(RESUMED_PANIC, 0, &[], never, StdVec::new());
    w.fx.finish();
    let keys = Keys { mk_d: w.mk_d, drop_d: w.drop_d, dip: w.dip, g, resumed, resumed_panic };

    let mut b = async_body();
    let lits: StdVec<(ExprId, u128)> = StdVec::new();
    let d = b.bind("d");
    let mk = b.path(&["mk_d"]);
    let made = b.call(mk, &[]);
    let s0 = b.let_(d, None, made);
    let awaited = call_await(&mut b, "twice");
    let (s1, r) = let_u64(&mut b, "r", awaited);
    let gp = b.path(&["g"]);
    let called = b.call(gp, &[]);
    let s2 = b.semi(called);
    let re = b.local(r);
    let body = b.block(&[s0, s1, s2], Some(re));
    let root = b.async_block(body, Capture::Move);
    let progs = [Prog { item: a, b: &b, root, lits: &lits }];

    let view = b.view(a, root);
    let mut infer = InferCtx::new_in(Alloc::global());
    let checked = match blaze_typeck::check(&view, &w.fx, &mut infer, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the check needed {:?}", n.questions),
        Err(error) => panic!("the check failed: {error:?}"),
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = w.fx.sig_of(a).expect("the fn's signature");
    let ordinal = crate::coroutines_in(&view).find(|c| c.1 == root).map(|c| c.0).expect("the body's root is its async block");
    let mut table = PortableTys::new_in(Alloc::global());
    let (env, output) = crate::coroutine_facts(&view, &checked, ordinal, sig.parent_count + sig.own_count, &mut table, Alloc::global())
        .unwrap_or_else(|e| panic!("the coroutine's facts: {e}"));
    let rows = [(a, ordinal, env, output)];
    let states = RefCell::new(StdVec::new());
    let inner = FxAdts { w: &w, progs: &progs, rows: &rows, table: table.view(), states: &states };
    let adts = UnwindAdts { inner: &inner, panic, resumed, resumed_panic, asked: Cell::new((false, false)) };
    let mut ground = PortableTys::new_in(Alloc::global());
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let co = crate::build_coroutine(&view, &checked, ordinal, &mut subst, &adts, None, Global)?;
    blaze_lir::verify(co.resume.func.view()).unwrap_or_else(|e| panic!("the split's poll does not verify: {e}"));
    let refs: StdVec<ItemKey> = co.resume.refs.iter().map(|it| it.item).collect();
    Ok(Made { poll: blaze_lir::print(co.resume.func.view()), refs, keys, asked: adts.asked.get() })
}

/// One printed instruction: the value it defines, its operation's name (flags cut off), and
/// its operands (after the type, when it defines a value).
#[derive(Clone, Copy)]
struct Inst<'t> {
    def: Option<&'t str>,
    op: &'t str,
    args: &'t str,
}

/// One printed block: `blockN <kind>:` and its instructions.
struct Blk<'t> {
    name: &'t str,
    kind: &'t str,
    insts: StdVec<Inst<'t>>,
}

/// `line` (indent trimmed) read as an instruction; a trailing `!mN` or `@spN` is cut off.
fn inst(line: &str) -> Inst<'_> {
    let mut line = line.trim();
    for mark in [" !m", " @sp"] {
        if let Some(at) = line.find(mark) {
            line = &line[..at];
        }
    }
    let (def, after) = match line.split_once(" = ") {
        Some((d, a)) if d.starts_with('v') && !d.contains(' ') => (Some(d), a),
        _ => (None, line),
    };
    let (token, rest) = after.split_once(' ').unwrap_or((after, ""));
    let op = token.split('.').next().unwrap_or(token);
    let args = match def {
        Some(_) => rest.split_once(' ').map_or("", |it| it.1),
        None => rest,
    };
    Inst { def, op, args }
}

/// The printed function's blocks, in layout order.
fn blocks_of(text: &str) -> StdVec<Blk<'_>> {
    let mut blocks: StdVec<Blk<'_>> = StdVec::new();
    for line in text.lines() {
        if !line.starts_with(' ') && line.starts_with("block") && line.ends_with(':') {
            let head = &line[..line.len() - 1];
            let (name, kind) = head.split_once(' ').unwrap_or((head, ""));
            blocks.push(Blk { name, kind, insts: StdVec::new() });
            continue;
        }
        if !line.starts_with("  ") {
            continue;
        }
        if let Some(last) = blocks.last_mut() {
            last.insts.push(inst(line));
        }
    }
    blocks
}

/// The printed `ref refN = func P` rows: each function reference's name and place.
fn func_refs(text: &str) -> StdVec<(&str, usize)> {
    let mut rows = StdVec::new();
    for line in text.lines() {
        let Some((name, what)) = line.strip_prefix("ref ").and_then(|rest| rest.split_once(" = ")) else {
            continue;
        };
        if let Some(place) = what.strip_prefix("func ") {
            let place = place.trim().parse::<usize>().unwrap_or_else(|_| panic!("a function reference row with no place: {line}"));
            rows.push((name, place));
        }
    }
    rows
}

/// The block named `name`.
fn block_named<'b, 't>(blocks: &'b [Blk<'t>], name: &str, text: &str) -> &'b Blk<'t> {
    blocks.iter().find(|b| b.name == name).unwrap_or_else(|| panic!("no block {name} in poll\n{text}"))
}

/// The instruction defining `value`.
fn def_of<'t>(blocks: &[Blk<'t>], value: &str) -> Option<Inst<'t>> {
    for b in blocks {
        for i in &b.insts {
            if i.def == Some(value) {
                return Some(*i);
            }
        }
    }
    None
}

/// The callee item of the function reference `name` (`refN`).
fn callee(rows: &[(&str, usize)], refs: &[ItemKey], name: &str, text: &str) -> ItemKey {
    let place = rows.iter().find(|r| r.0 == name).map(|r| r.1).unwrap_or_else(|| panic!("no `func` row for {name}\n{text}"));
    *refs.get(place).unwrap_or_else(|| panic!("{name}'s place {place} is not among the built refs ({})", refs.len()))
}

/// The operands of `args`, split at the first `n - 1` commas (a last `(..)` list stays whole).
fn operands(args: &str, n: usize) -> StdVec<&str> {
    args.splitn(n, ", ").map(str::trim).collect()
}

/// The module's program under `Unwind`, read per `invoke` of the split's `poll` by its callee:
/// - `mk_d()` (before `d` exists) and the drop of `d` itself: the cleanup path drops nothing;
/// - any other call (made while `d` is live: `twice()`, its `into_future` and `poll`, `g()`):
///   the cleanup path makes exactly one call, the drop of `d` (`drop_d` or `drop_in_place`),
///   whose argument is the state pointer or `ptr_add` of it, and that drop comes before the
///   poisoning store;
/// - every cleanup path is a chain of `cleanup` blocks joined by `jump`, makes no other call,
///   and ends in `store <iconst 2>, <state>, <tag offset>` then `resume`; the state and the tag
///   offset are read from `poll`'s head (`load <tag ty> <state>, <offset>`, the switch's
///   scrutinee); every `cleanup` block of `poll` lies on some invoke's path;
/// - there is a `mk_d` invoke and a `g` invoke, and at least two invokes with `d` live;
/// - `poll`'s switch sends tag 2 to a block calling `panic_const_async_fn_resumed_panic` and
///   tag 1 to one calling `panic_const_async_fn_resumed`, each then `unreachable`, and the build
///   asked for both;
/// - under `Abort`: no `invoke` and no `cleanup` block.
///
/// Fails without the piece: the build refuses with `BuildError::NoCleanup` (walk/unwind.rs) or
/// the pass refuses the landing block; a pass that resumed without poisoning fails the store
/// assertion; a cleanup that dropped `d` before it exists (or twice) fails the drop count; a
/// `poll` that traps at tag 2 fails the resumed-after-panicking assertion.
///
/// Prints "async unwind: L cleanups drop `d` once from the state, P only poison (`mk_d` and
/// the drop of `d`), each stores tag 2 at offset T before resume; poll at tag 2 calls
/// `panic_const_async_fn_resumed_panic`; none under abort" (L at least 2, P at least 1).
#[test]
fn an_async_fn_with_a_drop_live_across_an_await_builds_under_unwind() {
    let made = build_under(Panic::Unwind).unwrap_or_else(|e| panic!("the coroutine of `a` under `panic = unwind` failed: {e}"));
    let text = made.poll.as_str();
    let keys = made.keys;
    let blocks = blocks_of(text);
    let rows = func_refs(text);
    let refs = made.refs.as_slice();

    // The head: the tag's load and the switch on it.
    let head = blocks.first().unwrap_or_else(|| panic!("poll has no blocks\n{text}"));
    let switch = head.insts.last().filter(|i| i.op == "switch").unwrap_or_else(|| panic!("poll's head ends in a switch on the tag\n{text}"));
    let scrutinee = operands(switch.args, 2)[0];
    let load = def_of(&blocks, scrutinee).filter(|i| i.op == "load").unwrap_or_else(|| panic!("the switch's scrutinee {scrutinee} is a load of the tag\n{text}"));
    let tag_at = operands(load.args, 2);
    let (state, tag_offset) = (tag_at[0], tag_at[1]);

    // A finished state: tag 1 and tag 2 each go to a block calling its lang fn.
    let cases = switch.args.split_once('[').map_or("", |it| it.1).trim_end_matches(']');
    let mut finished = [(1u64, keys.resumed, RESUMED, false), (2u64, keys.resumed_panic, RESUMED_PANIC, false)];
    for case in cases.split("0x").skip(1) {
        let (hex, to) = case.split_once(':').unwrap_or_else(|| panic!("a switch case without a target: {case}\n{text}"));
        let tag = u64::from_str_radix(hex.trim(), 16).unwrap_or_else(|_| panic!("a switch case that is not hex: {hex}\n{text}"));
        let target = to.trim().split('(').next().unwrap_or("").trim();
        for want in finished.iter_mut().filter(|it| it.0 == tag) {
            let b = block_named(&blocks, target, text);
            let call = b.insts.iter().find(|i| i.op == "call").unwrap_or_else(|| panic!("poll at tag {tag} goes to {target}, which calls nothing\n{text}"));
            let who = callee(&rows, refs, operands(call.args, 3)[0], text);
            assert!(who == want.1, "poll at tag {tag} goes to {target}, which must call `{}`\n{text}", want.2);
            assert!(b.insts.last().is_some_and(|i| i.op == "unreachable"), "{target} never returns after `{}`\n{text}", want.2);
            want.3 = true;
        }
    }
    for want in finished.iter() {
        assert!(want.3, "poll's switch has no case for tag {} (`{}`)\n{text}", want.0, want.2);
    }

    // Each invoke's own cleanup path.
    let (mut live, mut poison_only, mut mk_seen, mut g_seen) = (0usize, 0usize, false, false);
    let mut covered: StdVec<&str> = StdVec::new();
    for b in &blocks {
        for i in b.insts.iter().filter(|i| i.op == "invoke") {
            let o = operands(i.args, 5);
            assert!(o.len() == 5, "an invoke with fewer than five fields: {} ({})\n{text}", i.args, b.name);
            let (who, landing) = (callee(&rows, refs, o[0], text), o[3]);
            let is_drop = who == keys.drop_d || who == keys.dip;
            mk_seen |= who == keys.mk_d;
            g_seen |= who == keys.g;
            // `d` is not live at the call that makes it, nor at its own drop.
            let drops_wanted = usize::from(!(who == keys.mk_d || is_drop));
            let mut chain: StdVec<&str> = StdVec::new();
            let mut next = Some(landing);
            while let Some(name) = next {
                assert!(!chain.contains(&name), "the cleanup path of {} loops back to {name}\n{text}", b.name);
                chain.push(name);
                let c = block_named(&blocks, name, text);
                assert_eq!(c.kind, "cleanup", "{name}, on the cleanup path of {}'s invoke, is a cleanup block\n{text}", b.name);
                let last = c.insts.last().unwrap_or_else(|| panic!("{name} is empty\n{text}"));
                next = match last.op {
                    "jump" => Some(last.args.split('(').next().unwrap_or("").trim()),
                    "resume" => None,
                    other => panic!("a cleanup path that ends {name} in `{other}`: this test reads straight cleanup paths, each ending in `resume`\n{text}"),
                };
            }
            let path: StdVec<Inst<'_>> = chain.iter().flat_map(|n| block_named(&blocks, n, text).insts.iter().copied()).collect();
            let mut drops = 0usize;
            for (k, step) in path.iter().enumerate().filter(|it| it.1.op == "call") {
                let co = operands(step.args, 3);
                let target = callee(&rows, refs, co[0], text);
                assert!(target == keys.drop_d || target == keys.dip, "a cleanup calls only the drop of `d`: step {k} of {chain:?}\n{text}");
                let arg = co.get(2).copied().unwrap_or("").trim_start_matches('(').trim_end_matches(')').trim();
                let from_state = arg == state
                    || def_of(&blocks, arg).is_some_and(|d| d.op == "ptr_add" && operands(d.args, 2)[0] == state);
                assert!(from_state, "the cleanup drops `d` at the state pointer {state} or an offset of it, not {arg}: {chain:?}\n{text}");
                drops += 1;
            }
            assert_eq!(drops, drops_wanted, "the cleanup of {}'s invoke drops `d` {drops_wanted} time(s): {chain:?}\n{text}", b.name);
            let n = path.len();
            assert!(n >= 2 && path[n - 1].op == "resume", "the cleanup path {chain:?} ends in `resume`\n{text}");
            let store = path[n - 2];
            assert_eq!(store.op, "store", "the cleanup path {chain:?} stores the tag right before `resume`\n{text}");
            let so = operands(store.args, 3);
            assert!(so.len() == 3 && so[1] == state && so[2] == tag_offset, "the tag is stored at {state}, offset {tag_offset}: {}\n{text}", store.args);
            let tag = def_of(&blocks, so[0]).unwrap_or_else(|| panic!("the stored tag {} has no definition\n{text}", so[0]));
            assert!(tag.op == "iconst" && tag.args == "2", "the tag stored before `resume` is `Panicked` (2), not `{} {}`\n{text}", tag.op, tag.args);
            match drops_wanted {
                0 => poison_only += 1,
                _ => live += 1,
            }
            covered.extend(chain.iter().copied());
        }
    }
    for b in blocks.iter().filter(|b| b.kind == "cleanup") {
        assert!(covered.contains(&b.name), "the cleanup block {} lies on no invoke's cleanup path\n{text}", b.name);
    }
    assert!(mk_seen, "`mk_d()` may unwind, so it is an invoke whose cleanup poisons\n{text}");
    assert!(g_seen, "`g()` (after the suspend, `d` in the state) is an invoke\n{text}");
    assert!(live >= 2, "calls before and after the suspend with `d` live each land in a cleanup dropping it: {live}\n{text}");
    assert_eq!(made.asked, (true, true), "the build asks for `{RESUMED}` and `{RESUMED_PANIC}` (returned, panicked)");

    let abort = build_under(Panic::Abort).unwrap_or_else(|e| panic!("the coroutine of `a` under `panic = abort` failed: {e}"));
    let abort_blocks = blocks_of(abort.poll.as_str());
    let landed = abort_blocks.iter().any(|b| b.kind == "cleanup" || b.insts.iter().any(|i| i.op == "invoke"));
    assert!(!landed, "no landing block under abort:\n{}", abort.poll);
    println!(
        "async unwind: {live} cleanups drop `d` once from the state, {poison_only} only poison (`mk_d` and the drop of `d`), each stores tag 2 at offset {tag_offset} before resume; poll at tag 2 calls `{RESUMED_PANIC}`; none under abort"
    );
}
