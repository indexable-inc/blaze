//! Bodies blaze-lower makes from real text, converted by [`to_typeck`] and checked by
//! blaze-typeck's `InferCtx` against a world written by hand (`blaze_typeck::fixture`): the
//! hand-off check of the ladder's rung 7c, whose `main` is the owner's hello world.

use blaze_own::Alloc;
use blaze_typeck::fixture::{rung_7c, show};
use blaze_typeck::{InferCtx, ItemKey, Needs, Outcome, PortableTys, check};

use crate::columns::View;
use crate::form::{ExprId, TypeId};
use crate::typeck::{Refused, Types, to_typeck};
use crate::{Cfg, CfgAnswer, Edition, lower_body};

/// `_` as an inferred const in each const-argument position, lowered from text and checked.
mod infer_const;

struct NoCfg;

impl Cfg for NoCfg {
    fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
        CfgAnswer::No
    }
}

/// The bodies here write no type: a written type is a refusal naming it.
struct NoTypes;

impl Types for NoTypes {
    fn ty(&mut self, _: &View<'_>, _: TypeId, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(Refused::Form { what: "a written type (the test world resolves none)", range: Default::default() })
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Err(Refused::Form { what: "a repeat length (the test world evaluates none)", range: Default::default() })
    }

    fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(crate::typeck::no_const_arg(view, c))
    }
}

/// Rung 7c's `main`, word for word (crates/blaze-cli/tests/ladder/7c-main-io/src/main.rs).
const MAIN_7C: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}";

/// Lowers `text`, converts it and checks it as `owner` of `fx`: the record and the body's
/// expression ranges.
fn lower_and_check(
    fx: &blaze_typeck::fixture::Fx,
    owner: ItemKey,
    text: &str,
) -> (blaze_typeck::Checked<'static>, std::vec::Vec<blaze_typeck::Range>, ExprId) {
    lower_and_check_with(fx, owner, text, &mut NoTypes)
}

/// [`lower_and_check`] with written types resolved by `types`.
fn lower_and_check_with(
    fx: &blaze_typeck::fixture::Fx,
    owner: ItemKey,
    text: &str,
    types: &mut dyn Types,
) -> (blaze_typeck::Checked<'static>, std::vec::Vec<blaze_typeck::Range>, ExprId) {
    try_lower_and_check_with(fx, owner, text, types).unwrap_or_else(|e| panic!("check refused {e:?} in\n{text}"))
}

fn try_lower_and_check_with(
    fx: &blaze_typeck::fixture::Fx,
    owner: ItemKey,
    text: &str,
    types: &mut dyn Types,
) -> Result<(blaze_typeck::Checked<'static>, std::vec::Vec<blaze_typeck::Range>, ExprId), blaze_typeck::CheckError> {
    let alloc = Alloc::global();
    let lowered = match lower_body(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let view = lowered.view();
    let mut tys = PortableTys::new_in(alloc);
    let body = match to_typeck(&view, owner, &mut tys, types, alloc) {
        Ok(b) => b,
        Err(e) => panic!("to_typeck refused {e:?} in\n{text}"),
    };
    let bv = body.view(tys.view());
    let mut cx = InferCtx::new_in(alloc);
    let c = match check(&bv, fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("{text}: the world missed {:?}", n.questions),
        Err(e) => return Err(e),
    };
    Ok((c, body.expr_ranges.iter().copied().collect(), bv.root))
}

/// Every expression as `text => type`, one a line, in the record's order: a desugaring's node
/// shows the text of the construct it was made for.
fn typed_lines(fx: &blaze_typeck::fixture::Fx, text: &str, c: &blaze_typeck::Checked<'_>, ranges: &[blaze_typeck::Range]) -> std::string::String {
    let types = c.types.view();
    let mut out = std::string::String::new();
    for (i, &t) in c.expr_ty.iter().enumerate() {
        let r = ranges[i];
        let src: std::string::String = text[r.start as usize..r.end as usize].split_whitespace().collect::<std::vec::Vec<_>>().join(" ");
        out.push_str(&std::format!("{i}: {src} => {}\n", show(fx, types, t)));
    }
    out
}

/// The text lowered (`lower_body`), converted (`to_typeck`: the `?`s' lang-item paths kept as
/// `BodyPath::lang`) and checked: no diagnostic, every expression typed (none `{error}` or
/// `_`), the body's type the fn's output.
#[test]
fn rung_7c_main_from_text_checks_clean() {
    let alloc = Alloc::global();
    let lowered = match lower_body(MAIN_7C, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc) {
        Ok(l) => l,
        Err(need) => panic!("rung 7c main: needs {:?}", need.atoms),
    };
    let view = lowered.view();
    let (fx, s) = rung_7c();
    let mut tys = PortableTys::new_in(alloc);
    let body = match to_typeck(&view, s.main, &mut tys, &mut NoTypes, alloc) {
        Ok(b) => b,
        Err(e) => panic!("rung 7c main: to_typeck refused {e:?}"),
    };
    let bv = body.view(tys.view());
    let mut cx = InferCtx::new_in(alloc);
    let c = match check(&bv, &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("rung 7c main: the world missed {:?}", n.questions),
        Err(e) => panic!("rung 7c main: {e:?}"),
    };
    let types = c.types.view();
    let shown: std::vec::Vec<std::string::String> = c.expr_ty.iter().map(|&t| show(&fx, types, t)).collect();
    assert!(c.diags.is_empty(), "rung 7c main: diagnostics {:?}\ntypes {shown:?}", c.diags);
    for (i, t) in shown.iter().enumerate() {
        // `_` is an unresolved variable as `show` prints it: a whole word, never inside a name.
        let unresolved = t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')).any(|w| w == "_");
        assert!(!t.contains("{error}") && !unresolved, "rung 7c main: expression {i} is {t}\ntypes {shown:?}");
    }
    assert_eq!(shown[bv.root.0 as usize], "Result<(), Error>", "types {shown:?}");
    // Each `?` matched on `ControlFlow`'s two variants, resolved through `lang_path`.
    let cf = c.pat_defs.column::<1>().iter().filter(|d| matches!(d, blaze_typeck::Def::Ctor { adt, .. } if *adt == s.control_flow)).count();
    assert_eq!(cf, 4, "pat defs {:?}", c.pat_defs.column::<1>());
    let _: ItemKey = s.bad;
}

const IS_A: &str = "fn is_a(b: u8) -> u8 {
    match b {
        b'a' => b'A',
        _ => [b'x', b'y'][0],
    }
}";

/// `b'x'` is a `u8` literal (the reference, "Byte literals"), as an expression, an array of
/// them and a pattern: the scrutinee `u8` meets the pattern's `u8`.
#[test]
fn byte_literals_are_u8() {
    let mut world = blaze_typeck::fixture::Fx::new();
    let fx = &mut world;
    let u8_ = fx.u8();
    let is_a = fx.func("is_a", 0, &[u8_], u8_, std::vec::Vec::new());
    fx.finish();
    let (c, ranges, _) = lower_and_check(&world, is_a, IS_A);
    let lines = typed_lines(&world, IS_A, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    // The `b'a'` pattern has no expression row; a pattern that were not `u8` would be E0308
    // against the scrutinee, so no diagnostic is its check.
    let want = "\
0: b => u8
1: b'A' => u8
2: b'x' => u8
3: b'y' => u8
4: [b'x', b'y'] => [u8; 2]
5: 0 => usize
6: [b'x', b'y'][0] => u8
7: match b { b'a' => b'A', _ => [b'x', b'y'][0], } => u8
8: { match b { b'a' => b'A', _ => [b'x', b'y'][0], } } => u8
";
    assert_eq!(lines, want);
}

fn pinned_future_world() -> blaze_typeck::fixture::Fx {
    use blaze_typeck::{FnPtrHeader, LangItem, List, Name, PNode};
    use std::vec;

    let mut fx = blaze_typeck::fixture::Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, vec![], &[]);
    fx.lang(LangItem::Copy, copy);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce = fx.tr("CoerceUnsized", 2, vec![], &[]);
    fx.lang(LangItem::CoerceUnsized, coerce);
    let send = fx.tr("Send", 1, vec![], &[]);
    fx.auto(send);
    fx.lang(LangItem::Send, send);
    let future = fx.tr("Future", 1, vec![], &["Output"]);
    fx.lang(LangItem::Future, future);

    let (t, u, byte) = (fx.param(0), fx.param(1), fx.u8());
    let word = fx.strukt("Word", 0, &[("byte", byte)]);
    let word = fx.adt(word, &[]);
    let args = fx.list(&[t, byte]);
    let callback = fx.ty(PNode::FnPtr(args, FnPtrHeader::RUST));
    fx.func("make", 1, &[callback], t, vec![]);

    let boxed = fx.strukt("Box", 1, &[]);
    let pin = fx.strukt("Pin", 1, &[("pointer", t)]);
    let (box_t, box_u) = (fx.adt(boxed, &[t]), fx.adt(boxed, &[u]));
    let pointee_unsizes = fx.bound(unsize, &[t, u]);
    let box_impl = fx.imp("box_unsize", 2, box_t, Some((coerce, &[box_u])), &[], vec![pointee_unsizes]);
    fx.unsized_param(box_impl, 0);
    fx.unsized_param(box_impl, 1);
    let (pin_t, pin_u) = (fx.adt(pin, &[t]), fx.adt(pin, &[u]));
    let pointer_coerces = fx.bound(coerce, &[t, u]);
    fx.imp("pin_unsize", 2, pin_t, Some((coerce, &[pin_u])), &[], vec![pointer_coerces]);
    let concrete = fx.adt(pin, &[box_t]);
    let inherent = fx.imp("Box<T>", 1, box_t, None, &[], vec![]);
    fx.impl_fn(inherent, "pin", false, &[t], concrete);

    let principal = fx.ty(PNode::ExTrait(future, List::EMPTY));
    let auto = fx.ty(PNode::ExAuto(send));
    let mut alias = |name, term, send| {
        let output = fx.ty(PNode::ExProj { trait_: future, name: Name::of("Output"), args: List::EMPTY, term });
        let rows = [principal, output, auto];
        let obj = fx.t.dynamic(&rows[..if send { 3 } else { 2 }]).expect("future object");
        let erased = fx.adt(boxed, &[obj]);
        let target = fx.adt(pin, &[erased]);
        fx.type_alias_item(name, 0, target);
        target
    };
    let target = alias("Pinned", word, true);
    let local = alias("Local", word, false);
    let other = alias("Other", byte, true);
    fx.func("example", 0, &[], target, vec![]);
    fx.func("drop_send", 0, &[target], local, vec![]);
    fx.func("add_send", 0, &[local], target, vec![]);
    fx.func("change_output", 0, &[target], other, vec![]);
    fx.finish();
    fx
}

/// `Token` models a thread-bound value. It can be constructed while polling a Send
/// future, but the unpolled future cannot own one, and a pending future cannot save one.
fn future_witness_world() -> blaze_typeck::fixture::Fx {
    use blaze_typeck::{Def, ImplPolarity, LangItem, Mutbl};
    let mut fx = pinned_future_world();
    let send = fx.key_of("Send");
    let future = fx.key_of("Future");
    let token = fx.strukt("Token", 0, &[]);
    let token = fx.adt(token, &[]);
    let negative = fx.imp("impl !Send for Token", 0, token, Some((send, &[])), &[], std::vec![]);
    fx.set_polarity(negative, ImplPolarity::Negative);
    let unit = fx.unit();
    fx.func("token", 0, &[], token, std::vec![]);
    fx.func("consume", 0, &[token], unit, std::vec![]);
    let f = fx.param(0);
    let is_send = fx.bound(send, &[f]);
    fx.func("spawn", 1, &[f], unit, std::vec![is_send]);
    fx.func("check_spawn", 0, &[], unit, std::vec![]);
    let sync = fx.tr("Sync", 1, std::vec![], &[]);
    fx.lang(LangItem::Sync, sync);
    fx.auto(sync);
    let is_sync = fx.bound(sync, &[f]);
    let reference = fx.r(Mutbl::Not, f);
    fx.imp("Send for &T where T: Sync", 1, reference, Some((send, &[])), &[], std::vec![is_sync]);
    let borrowed = fx.r(Mutbl::Not, token);
    fx.func("observe", 0, &[borrowed], unit, std::vec![]);
    let word = fx.adt(fx.key_of("Word"), &[]);
    fx.func("word", 0, &[], word, std::vec![]);

    let pending = fx.strukt("Tick", 0, &[]);
    let tick = fx.adt(pending, &[]);
    fx.func("tick", 0, &[], tick, std::vec![]);
    fx.imp("Future for Tick", 0, tick, Some((future, &[])), &[("Output", unit)], std::vec![]);
    fx.imp("Future for Token", 0, token, Some((future, &[])), &[("Output", unit)], std::vec![]);
    let into = fx.tr("IntoFuture", 1, std::vec![], &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into);
    let held = fx.alias(into, "IntoFuture", &[f]);
    let method = fx.trait_fn(into, "into_future", true, &[f], held);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(method));
    let output = fx.alias(future, "Output", &[f]);
    let is_future = fx.bound(future, &[f]);
    fx.imp("IntoFuture for F", 1, f, Some((into, &[])), &[("IntoFuture", f), ("Output", output)], std::vec![is_future]);
    fx.finish();
    fx
}

#[test]
fn a_future_without_await_stores_its_captures_only() {
    use blaze_typeck::{DiagCode, PNode};
    let fx = future_witness_world();
    let owner = fx.key_of("example");
    let text = "fn example() -> Pinned { Box::pin(async { consume(token()); word() }) }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);
    let types = checked.types.view();
    let witnesses: std::vec::Vec<_> = checked.expr_ty.iter().filter_map(|&t| match types.node(t).unwrap() {
        PNode::Future(_, _, witness) => Some(types.node(witness).unwrap()),
        _ => None,
    }).collect();
    assert!(!witnesses.is_empty());
    assert!(witnesses.iter().all(|w| *w == PNode::Tuple(blaze_typeck::List::EMPTY)), "{witnesses:?}");

    let text = "fn example() -> Pinned { let x = token(); Box::pin(async move { consume(x); word() }) }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    let codes: std::vec::Vec<_> = checked.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagCode::Unsatisfied], "{:?}", checked.diags);

    // Producing a !Send value on the polling thread does not store it in the future.
    let owner = fx.key_of("check_spawn");
    let text = "fn check_spawn() { spawn(async { token() }); }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);

    // Returning a captured non-Copy binding moves it into the unpolled future,
    // even without an explicit `move`.
    let text = "fn check_spawn() { let held = token(); spawn(async { held }); }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    let codes: std::vec::Vec<_> = checked.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagCode::Unsatisfied], "{:?}", checked.diags);

    // This future only borrows Token. Sending &Token requires Token: Sync.
    let text = "fn check_spawn() { let held = token(); spawn(async { observe(&held); }); }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);

    // The async call has already taken its argument before the first poll,
    // including when the body never reads the argument.
    let text = "fn check_spawn() { let f = async |_unused| word(); spawn(f(token())); }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    let codes: std::vec::Vec<_> = checked.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [DiagCode::Unsatisfied], "{:?}", checked.diags);
}

/// The initial state can cross threads before it is polled. Consuming a capture
/// before the first await cannot make an initially !Send future Send.
#[test]
fn a_suspending_future_owns_its_move_capture_before_the_first_poll() {
    let fx = future_witness_world();
    let owner = fx.key_of("check_spawn");
    let text = "fn check_spawn() { let held = token(); spawn(async move { consume(held); tick().await; }); }";
    let (checked, _, _) = lower_and_check(&fx, owner, text);
    let codes: std::vec::Vec<_> = checked.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [blaze_typeck::DiagCode::Unsatisfied], "{:?}", checked.diags);
}

#[test]
fn boxed_coercion_replays_the_target_against_the_suspension_witness() {
    use blaze_typeck::{DiagCode, List, PNode, check_deferring, decide_stalled};
    let alloc = Alloc::global();
    let fx = future_witness_world();
    let owner = fx.key_of("example");
    for (body, pending, saved_local) in [
        ("let x = token(); consume(x); tick().await; word()", "Tick", false),
        ("let x = token(); tick().await; consume(x); word()", "Tick", true),
        ("token().await; word()", "Token", false),
    ] {
        let text = std::format!("fn example() -> Pinned {{ Box::pin(async {{ {body} }}) }}");
        let lowered = lower_body(&text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc).expect("no cfg");
        let mut tys = PortableTys::new_in(alloc);
        let body = to_typeck(&lowered.view(), owner, &mut tys, &mut NoTypes, alloc).expect("lowered body");
        let view = body.view(tys.view());
        let mut cx = InferCtx::new_in(alloc);
        let Outcome::Done(checked) = check_deferring(&view, &fx, &mut cx, &Needs::new_in(alloc), alloc).expect("defer the coercion") else {
            panic!("fixture missed a question");
        };
        assert!(checked.diags.is_empty(), "{:?}", checked.diags);
        assert_eq!(checked.stalled.len(), 1);
        let block = view.exprs.iter().position(|e| matches!(e, blaze_typeck::Expr::AsyncBlock { .. })).expect("async block");
        let block = blaze_typeck::ExprId(block as u32);
        // The flow writer supplies the pending operand and any local saved on suspension.
        let mut out = checked.types.fork_in(alloc).unwrap();
        let awaited = out.intern(PNode::Adt(fx.key_of(pending), List::EMPTY)).unwrap();
        let token = out.intern(PNode::Adt(fx.key_of("Token"), List::EMPTY)).unwrap();
        let parts = [awaited, token];
        let list = out.intern_list(&parts[..if saved_local { 2 } else { 1 }]).unwrap();
        let witness = out.intern(PNode::Tuple(list)).unwrap();
        let findings = decide_stalled(&view, &checked, out.view(), &[(block, witness)], &fx, alloc).expect("replay the coercion");
        let holds_token = saved_local || pending == "Token";
        assert_eq!(findings.len(), usize::from(holds_token), "{findings:?}");
        if holds_token {
            assert_eq!(findings[0].code, DiagCode::Unsatisfied);
            let site = findings[0].at;
            assert!(text[site.start as usize..site.end as usize].contains("Box::pin"), "{site:?}");
        } else {
            let record = checked.types.view();
            let PNode::Future(_, _, provisional) = record.node(checked.expr_ty[block.0 as usize]).unwrap() else {
                panic!("expected the block's future");
            };
            // Plant the old witness, which includes Token even after its move.
            let wrong = decide_stalled(&view, &checked, out.view(), &[(block, provisional)], &fx, alloc).unwrap();
            assert_eq!(wrong.as_slice(), &[checked.stalled[0].diag]);
            assert_eq!(wrong[0].code, DiagCode::Unsatisfied);
        }
    }
}

/// The equality on an erased future's Output types the block before its tail
/// is checked. Here the callback's field access needs that information at
/// the call, and the completed concrete future still needs to unsize.
#[test]
fn boxed_async_output_types_a_callback_in_the_body() {
    use blaze_typeck::AdjustKind;
    let fx = pinned_future_world();
    let owner = fx.key_of("example");
    let text = "fn example() -> Pinned { Box::pin(async { make(|value| value.byte) }) }";
    let (checked, ranges, _) = lower_and_check(&fx, owner, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);
    let sites: std::vec::Vec<_> = checked.adjusts.column::<0>().iter().zip(checked.adjusts.column::<1>())
        .filter_map(|(e, k)| {
            let r = ranges[e.0 as usize];
            (*k == AdjustKind::Unsize).then_some(&text[r.start as usize..r.end as usize])
        })
        .collect();
    assert_eq!(sites, ["Box::pin(async { make(|value| value.byte) })"]);
    // Changing only the field exercises the same inferred callback parameter.
    let wrong = "fn example() -> Pinned { Box::pin(async { make(|value| value.other) }) }";
    let (checked, _, _) = lower_and_check(&fx, owner, wrong);
    assert_eq!(findings(wrong, &checked), [(blaze_typeck::DiagCode::NoField, "value.other")]);
}

#[test]
fn a_boxed_future_upcast_preserves_output_and_can_drop_send() {
    use blaze_typeck::{AdjustKind, DiagCode};
    let fx = pinned_future_world();
    let text = "fn drop_send(x: Pinned) -> Local { x }";
    let (c, _, _) = lower_and_check(&fx, fx.key_of("drop_send"), text);
    assert!(c.diags.is_empty(), "{:?}", c.diags);
    assert_eq!(c.adjusts.column::<1>(), &[AdjustKind::Unsize]);
    for (name, text) in [
        ("add_send", "fn add_send(x: Local) -> Pinned { x }"),
        ("change_output", "fn change_output(x: Pinned) -> Other { x }"),
    ] {
        let (c, _, _) = lower_and_check(&fx, fx.key_of(name), text);
        assert_eq!(findings(text, &c), [(DiagCode::Mismatch, "x")]);
    }
}

const LETS: &str = "fn pick(reach: Reach, o: Option<u32>) -> u32 {
    if let Reach::Hosted = reach {
        return 1;
    }
    while let Some(y) = o {
        return y;
    }
    let Some(x) = o else { return 0 };
    x
}";

/// `let` in a condition (the hosted hello's `if let Reach::Hosted = self.reach`) and a
/// let-else (`let Some(_entered) = Entered::enter(&this) else { return .. }`): the `let` is
/// `bool`, its pattern checked against its initializer's type (`y: u32`), and the let-else's
/// `else` block `!`.
#[test]
fn let_in_a_condition_and_let_else() {
    let mut world = blaze_typeck::fixture::Fx::new();
    let fx = &mut world;
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    fx.use_path("Some", blaze_typeck::Def::Ctor { adt: option, variant: 1, kind: blaze_typeck::CtorKind::Tuple });
    let (reach, _) = fx.enum_("Reach", 0, &[("Hosted", &[]), ("Guest", &[])]);
    let u32_ = fx.u32();
    let reach_t = fx.adt(reach, &[]);
    let opt_u32 = fx.adt(option, &[u32_]);
    let pick = fx.func("pick", 0, &[reach_t, opt_u32], u32_, std::vec::Vec::new());
    fx.finish();
    let (c, ranges, _) = lower_and_check(&world, pick, LETS);
    let lines = typed_lines(&world, LETS, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    // `while let` is `loop { if let .. { body } else { break } }` (rustc
    // `lower_expr_while_in_loop_scope`): 11 the `break`, 12 its block, 13 the `if` (`()`: its
    // branches both `!`, coerced to the loop body's expected `()`), 14 the loop's body, 15 the
    // loop. A block whose statements diverge and that has no tail is `!` (4, 10, 19).
    let want = "\
0: reach => Reach
1: let Reach::Hosted = reach => bool
2: 1 => u32
3: return 1 => !
4: { return 1; } => !
5: if let Reach::Hosted = reach { return 1; } => ()
6: o => Option<u32>
7: let Some(y) = o => bool
8: y => u32
9: return y => !
10: { return y; } => !
11: while let Some(y) = o { return y; } => !
12: while let Some(y) = o { return y; } => !
13: while let Some(y) = o { return y; } => ()
14: while let Some(y) = o { return y; } => ()
15: while let Some(y) = o { return y; } => ()
16: o => Option<u32>
17: 0 => u32
18: return 0 => !
19: { return 0 } => !
20: x => u32
21: { if let Reach::Hosted = reach { return 1; } while let Some(y) = o { return y; } let Some(x) = o else { return 0 }; x } => u32
";
    assert_eq!(lines, want);
}

/// crates/blaze-std/src/out.rs's `Out::line`, word for word.
const LINE: &str = "fn line(&self, cx: &Cx, what: impl Show) -> Result<(), OutError> {
    let mut held = Held::new(self, cx);
    what.show(&mut held);
    held.take(b\"\\n\");
    held.end()
}";

/// crates/blaze-text/src/show.rs's forwarding impl's method, word for word.
const REF_SHOW: &str = "fn show<P: Put + ?Sized>(&self, out: &mut P) {
    (**self).show(out);
}";

const HELLO: &str = "fn hello(o: &Out, cx: &Cx) -> Result<(), OutError> {
    o.line(cx, \"Hello, world!\")
}";

/// The hosted hello's output path, each body from its text:
///
/// `Out::line`: `what.show(&mut held)` is a method of the bound `W: Show` (a param-env
/// candidate), its own `P` inferred from the argument as `Held`, `Held: Put` by its impl;
/// `b"\n"` (`&[u8; 1]`) unsizes to `take`'s `&[u8]`.
///
/// The `&T` impl's `show`: `(**self).show(out)` probes `T` (`T: Show` from the impl's
/// where-clause, `T` `?Sized`), `P` passed through as the caller's.
///
/// `hello`: `o.line(cx, "Hello, world!")` instantiates `W = &str`; `&str: Show` holds by the
/// `&T` impl with `T = str`, which needs `str: Show` (its impl) and no `str: Sized` (the
/// impl's `?Sized`).
#[test]
fn show_through_put_on_the_hosted_output_path() {
    let (fx, s) = blaze_typeck::fixture::show_put();
    let mut all = std::string::String::new();
    for (owner, text) in [(s.line, LINE), (s.ref_show, REF_SHOW), (s.hello, HELLO)] {
        let (c, ranges, _) = lower_and_check(&fx, owner, text);
        let lines = typed_lines(&fx, text, &c, &ranges);
        assert!(c.diags.is_empty(), "{text}\ndiagnostics {:?}\n{lines}", c.diags);
        all.push_str(&lines);
        all.push_str("--\n");
    }
    // `what: impl Show` is the fn's anonymous parameter, shown `P0`; in the `&T` impl's `show`
    // `T` is `P0` and the method's own `P` is `P1`.
    let want = "\
0: Held::new => fn new
1: self => &Out
2: cx => &Cx
3: Held::new(self, cx) => Held
4: what => P0
5: held => Held
6: &mut held => &mut Held
7: what.show(&mut held) => ()
8: held => Held
9: b\"\\n\" => &[u8; 1]
10: held.take(b\"\\n\") => bool
11: held => Held
12: held.end() => Result<(), OutError>
13: { let mut held = Held::new(self, cx); what.show(&mut held); held.take(b\"\\n\"); held.end() } => Result<(), OutError>
--
0: self => &&P0
1: *self => &P0
2: **self => P0
3: (**self) => P0
4: out => &mut P1
5: (**self).show(out) => ()
6: { (**self).show(out); } => ()
--
0: o => &Out
1: cx => &Cx
2: \"Hello, world!\" => &str
3: o.line(cx, \"Hello, world!\") => Result<(), OutError>
4: { o.line(cx, \"Hello, world!\") } => Result<(), OutError>
--
";
    assert_eq!(all, want);
    let _ = s.show;
}

/// The control of the case above: with the `&T` impl's `T` implied `Sized` (no `?Sized`),
/// `&str: Show` needs `str: Sized`, which no impl gives: `hello`'s call is E0277.
#[test]
fn show_for_ref_str_needs_the_impls_question_sized() {
    let (fx, s) = blaze_typeck::fixture::show_put_with(false);
    let (c, _, _) = lower_and_check(&fx, s.hello, HELLO);
    let codes: std::vec::Vec<_> = c.diags.iter().map(|d| d.code).collect();
    assert_eq!(codes, [blaze_typeck::DiagCode::Unsatisfied], "{:?}", c.diags);
}

/// The Reference, "Trait object types": "the trait object implements the base trait, its auto
/// traits, and any supertraits of the base trait." Probe the erased pointee itself, as the
/// `Send` impl of a pointer to a future must. Changing only the object's bounds changes the
/// answer. The solver's `control_dyn_undecided_as_no` covers an unavailable principal verdict.
#[test]
fn an_erased_future_satisfies_its_listed_or_inherited_auto_trait() {
    use blaze_typeck::{DiagCode, LangItem, List, Mutbl, Name, PNode};
    use std::{vec, vec::Vec};

    let mut fx = blaze_typeck::fixture::Fx::new();
    let unit = fx.unit();
    let t = fx.param(0);
    let send = fx.tr("Send", 1, vec![], &[]);
    fx.lang(LangItem::Send, send);
    fx.auto(send);
    let send_t = fx.bound(send, &[t]);
    let future = fx.tr("Future", 1, vec![], &["Output"]);
    fx.lang(LangItem::Future, future);
    let future_t = fx.bound(future, &[t]);
    let task = fx.tr("Task", 1, vec![future_t, send_t], &[]);
    let r_t = fx.r(Mutbl::Not, t);
    // No Sized lang item in this world: require<T: ?Sized + Send>(_: &T).
    fx.func("require", 1, &[r_t], unit, vec![send_t]);

    let base = fx.ty(PNode::ExTrait(future, List::EMPTY));
    let sub = fx.ty(PNode::ExTrait(task, List::EMPTY));
    let output = fx.ty(PNode::ExProj { trait_: future, name: Name::of("Output"), args: List::EMPTY, term: unit });
    let auto = fx.ty(PNode::ExAuto(send));
    for (name, text, predicates, wanted) in [
        ("listed", "fn listed(x: &(dyn Future<Output = ()> + Send)) { require(x); }", &[base, output, auto][..], &[][..]),
        ("inherited", "fn inherited(x: &dyn Task<Output = ()>) { require(x); }", &[sub, output][..], &[][..]),
        ("absent", "fn absent(x: &dyn Future<Output = ()>) { require(x); }", &[base, output][..], &[DiagCode::Unsatisfied][..]),
    ] {
        let object = fx.t.dynamic(predicates).expect("one principal and one Output binding");
        let reference = fx.r(Mutbl::Not, object);
        let owner = fx.func(name, 0, &[reference], unit, vec![]);
        fx.finish();
        let (checked, _, _) = lower_and_check(&fx, owner, text);
        let codes: Vec<_> = checked.diags.iter().map(|diag| diag.code).collect();
        assert_eq!(codes, wanted, "{text}: {:?}", checked.diags);
    }
}

const SUM: &str = "fn sum(xs: &[u32]) -> u32 {
    let mut t = 0;
    for i in 0..xs.len() {
        t += xs[i];
    }
    for x in xs {
        t += *x;
    }
    t
}";

/// `for` over a range and over a slice, as rustc lowers it (`lower_expr_for`):
/// `match IntoIterator::into_iter(HEAD) { mut iter => loop { match Iterator::next(&mut iter)
/// { None => break, Some(PAT) => BODY } } }`, and `a..b` as `Range { start: a, end: b }`.
///
/// The range: `Range { start: 0, end: xs.len() }` is `Range<usize>` (the field types unify the
/// literal with `len`'s `usize`); `IntoIterator::into_iter` selects the blanket impl
/// (`Range<usize>: Iterator` by its impl), so `IntoIter` is `Range<usize>` itself and `next`'s
/// `Option<<Range<usize> as Iterator>::Item>` normalizes to `Option<usize>`: `i: usize`, and
/// `xs[i]` the builtin slice index (`u32`), `t += ..` fixing `t`'s literal to `u32`.
///
/// The slice: `&[u32]: IntoIterator` by the `&[T]` impl (the blanket would need
/// `&[u32]: Iterator`, which no impl gives), `IntoIter = Iter<u32>`, `Item = &u32`: `x: &u32`.
/// Every loop is `()` (a `loop` left only by a value-less `break`), the `for` too.
#[test]
fn for_over_a_range_and_a_slice() {
    let (fx, s) = blaze_typeck::fixture::loops();
    let (c, ranges, root) = lower_and_check(&fx, s.sum, SUM);
    let lines = typed_lines(&fx, SUM, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    // A desugaring's node shows the text of the `for` or the head it was made for: 11 and 28
    // the `None => break` (`!`), 13 and 30 `&mut iter`, 16 and 33 the `next` match, 17/18 and
    // 34/35 the loop's body and the loop, 21 and 38 the outer `match into_iter(..)`.
    let want = "\
0: 0 => u32
1: 0 => usize
2: xs => &[u32]
3: xs.len() => usize
4: 0..xs.len() => Range<usize>
5: t => u32
6: xs => &[u32]
7: i => usize
8: xs[i] => u32
9: t += xs[i] => ()
10: { t += xs[i]; } => ()
11: for i in 0..xs.len() { t += xs[i]; } => !
12: 0..xs.len() => Range<usize>
13: 0..xs.len() => &mut Range<usize>
14: 0..xs.len() => fn next<Range<usize>>
15: 0..xs.len() => Option<usize>
16: 0..xs.len() => ()
17: for i in 0..xs.len() { t += xs[i]; } => ()
18: for i in 0..xs.len() { t += xs[i]; } => ()
19: 0..xs.len() => fn into_iter<Range<usize>>
20: 0..xs.len() => Range<usize>
21: for i in 0..xs.len() { t += xs[i]; } => ()
22: xs => &[u32]
23: t => u32
24: x => &u32
25: *x => u32
26: t += *x => ()
27: { t += *x; } => ()
28: for x in xs { t += *x; } => !
29: xs => Iter<u32>
30: xs => &mut Iter<u32>
31: xs => fn next<Iter<u32>>
32: xs => Option<&u32>
33: xs => ()
34: for x in xs { t += *x; } => ()
35: for x in xs { t += *x; } => ()
36: xs => fn into_iter<&[u32]>
37: xs => Iter<u32>
38: for x in xs { t += *x; } => ()
39: t => u32
40: { let mut t = 0; for i in 0..xs.len() { t += xs[i]; } for x in xs { t += *x; } t } => u32
";
    assert_eq!(lines, want);
    assert_eq!(root.0, 40);
    // `next`'s arms name `Option`'s variants: `None` then `Some`, for each loop.
    let opt = c.pat_defs.column::<1>().iter().filter(|d| matches!(d, blaze_typeck::Def::Ctor { adt, .. } if *adt == s.option)).count();
    assert_eq!(opt, 4, "pat defs {:?}", c.pat_defs.column::<1>());
}

const CLOSURE_SUM: &str = "fn sum() -> u32 {
    let add = |a, b| a + b;
    let f = |x| x + 1u32;
    add(1u32, 2u32) + f(2)
}";

/// `type Callback<I, O> = &'static dyn Fn(I) -> O; struct Wrap(Callback<u8, u32>);`
/// The Reference, "Type aliases": "A type alias defines a new name for an existing type."
/// The call's input is the object's tuple argument, its result the object's FnOnce::Output.
#[test]
fn applied_alias_in_a_nongeneric_callable_field() {
    use blaze_typeck::{CheckError, LangItem, Malformed, Mutbl, Name, PNode};
    use std::vec;

    let mut fx = blaze_typeck::fixture::Fx::new();
    let (p0, p1) = (fx.param(0), fx.param(1));
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let once = fx.tr("FnOnce", 2, vec![], &["Output"]);
    fx.lang(LangItem::FnOnce, once);
    let parent = fx.bound(once, &[p0, p1]);
    let mutable = fx.tr("FnMut", 2, vec![parent], &[]);
    fx.lang(LangItem::FnMut, mutable);
    let parent = fx.bound(mutable, &[p0, p1]);
    let callable = fx.tr("Fn", 2, vec![parent], &[]);
    fx.lang(LangItem::Fn, callable);

    let object = |fx: &mut blaze_typeck::fixture::Fx, input, output| {
        let tuple = fx.tuple(&[input]);
        let args = fx.list(&[tuple]);
        let principal = fx.ty(PNode::ExTrait(callable, args));
        let binding = fx.ty(PNode::ExProj { trait_: once, name: Name::of("Output"), args, term: output });
        let dyn_ty = fx.t.dynamic(&[principal, binding]).expect("valid object bounds");
        fx.r(Mutbl::Not, dyn_ty)
    };
    let alias = object(&mut fx, p0, p1);
    let (u8_, u32_) = (fx.u8(), fx.u32());
    let field = fx.t.subst(alias, &[u8_, u32_]).expect("two alias arguments");
    assert_eq!(field, object(&mut fx, u8_, u32_));
    let wrap = fx.tuple_strukt("Wrap", 0, &[field]);
    let receiver = fx.adt(wrap, &[]);
    let run = fx.func("run", 0, &[receiver], u32_, vec![]);
    // Plant the original producer defect: both arguments still belong to the alias
    // when its unapplied RHS is filed as the field of a wrapper with zero parameters.
    let broken = fx.tuple_strukt("Broken", 0, &[alias]);
    let receiver = fx.adt(broken, &[]);
    let control = fx.func("broken", 0, &[receiver], u32_, vec![]);
    fx.finish();
    let text = "fn run(w: Wrap) -> u32 { (w.0)(1) }";
    let (checked, ranges, root) = lower_and_check(&fx, run, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);
    let types = checked.types.view();
    assert_eq!(show(&fx, types, checked.expr_ty[root.0 as usize]), "u32");
    let literal = ranges.iter().position(|r| &text[r.start as usize..r.end as usize] == "1").expect("argument");
    assert_eq!(show(&fx, types, checked.expr_ty[literal]), "u8");
    let result = try_lower_and_check_with(&fx, control, "fn broken(w: Broken) -> u32 { (w.0)(1) }", &mut NoTypes);
    assert!(matches!(result, Err(CheckError::Malformed(Malformed::Param {
        index: 0 | 1, args: 0, site: blaze_typeck::ParamSite::Field { owner, adt, field, .. },
    })) if owner == control && adt == broken && field == Name::of_index(0)));
}

/// The Reference, "Async blocks": the final expression of the block determines the
/// result of the future. The call supplies the type of the captured closure parameter.
#[test]
fn writeback_exports_a_closure_returning_an_async_block() {
    use blaze_typeck::{LangItem, PNode, UintTy};

    let mut fx = blaze_typeck::fixture::Fx::new();
    let copy = fx.tr("Copy", 1, std::vec![], &[]);
    fx.lang(LangItem::Copy, copy);
    let u8_ = fx.u8();
    fx.imp("impl Copy for u8", 0, u8_, Some((copy, &[])), &[], std::vec![]);
    let unit = fx.unit();
    let owner = fx.func("run", 0, &[], unit, std::vec![]);
    fx.finish();
    let text = "fn run() { let start = |context| async move { context }; let future = start(7u8); }";
    let (checked, ranges, _) = lower_and_check(&fx, owner, text);
    assert!(checked.diags.is_empty(), "{:?}", checked.diags);
    let types = checked.types.view();
    for expression in ["async move { context }", "start(7u8)"] {
        // An async block and its inner block share one range (the first node at it is the
        // inner block, of type `u8`: the first compile, 2026-11-18): the future is the node at
        // that range whose type is a future, and there must be exactly one.
        let at_range: std::vec::Vec<_> = ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| &text[r.start as usize..r.end as usize] == expression)
            .map(|(at, _)| types.node(checked.expr_ty[at]).expect("exported expression"))
            .collect();
        let futures: std::vec::Vec<_> = at_range.iter().filter_map(|ty| match ty { PNode::Future(_, output, _) => Some(*output), _ => None }).collect();
        assert_eq!(futures.len(), 1, "{expression}: one future among the nodes at its range: {at_range:?}");
        assert_eq!(types.node(futures[0]), Ok(PNode::Uint(UintTy::U8)));
    }
}

/// The census' first wrong answer: `a + b` with `a` still a variable (an unannotated closure
/// parameter) registers `?A: Add<?B>` ambiguous and projects `Output`, as rustc's
/// `lookup_method_in_trait` (`predicate_may_hold` only); the calls settle them. Before the fix
/// it was E0369. The world: `trait Add<Rhs> { type Output; }`, `impl Add<u32> for u32`.
#[test]
fn binop_on_a_closure_parameter_waits_for_the_call() {
    let mut world = blaze_typeck::fixture::Fx::new();
    let fx = &mut world;
    let add = fx.tr("Add", 2, std::vec::Vec::new(), &["Output"]);
    fx.lang(blaze_typeck::LangItem::Add, add);
    let u32_ = fx.u32();
    fx.imp("impl Add<u32> for u32", 0, u32_, Some((add, &[u32_])), &[("Output", u32_)], std::vec::Vec::new());
    let sum = fx.func("sum", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let (c, ranges, _) = lower_and_check(&world, sum, CLOSURE_SUM);
    let lines = typed_lines(&world, CLOSURE_SUM, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["a + b => u32", "x + 1u32 => u32", "f(2) => u32", "add(1u32, 2u32) + f(2) => u32"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
}

/// In `fn once(f: impl AsyncFnOnce(Arg) -> u8)`, field lookup needs the input
/// hint before the body is checked. The u8 describes the call future's output.
#[test]
fn async_callable_bound_types_the_parameter_and_the_future_output() {
    use blaze_typeck::{DiagCode, LangItem};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let once = fx.tr("AsyncFnOnce", 2, std::vec![], &["CallOnceFuture", "Output"]);
    fx.lang(LangItem::AsyncFnOnce, once);
    let copy = fx.tr("Copy", 1, std::vec![], &[]);
    fx.lang(LangItem::Copy, copy);
    let future = fx.tr("Future", 1, std::vec![], &["Output"]);
    fx.lang(LangItem::Future, future);
    let u8_ = fx.u8();
    let arg = fx.strukt("Arg", 0, &[("value", u8_)]);
    let arg = fx.adt(arg, &[]);
    let tuple = fx.tuple(&[arg]);
    let f = fx.param(0);
    let bound = fx.bound(once, &[f, tuple]);
    let output = fx.proj(once, &[f, tuple], "Output", u8_);
    let unit = fx.unit();
    fx.func("once", 1, &[f], unit, std::vec![bound, output]);
    let owner = fx.func("example", 0, &[], unit, std::vec![]);
    fx.finish();

    let good = "fn example() { once(async |arg| arg.value); }";
    let (c, ranges, _) = lower_and_check(&fx, owner, good);
    let lines = typed_lines(&fx, good, &c, &ranges);
    assert!(c.diags.is_empty(), "{:?}\n{lines}", c.diags);
    assert!(lines.contains("arg.value => u8"), "{lines}");

    let wrong = "fn example() { once(async |_arg| false); }";
    let (c, _, _) = lower_and_check(&fx, owner, wrong);
    assert_eq!(findings(wrong, &c), [(DiagCode::Mismatch, "false")]);
}

/// `IntoFuture::into_future(self)` and an inherent method taking `self`
/// both consume a non-Copy capture. A method taking `&self` only reads it.
#[test]
fn an_async_closure_consuming_its_await_operand_or_receiver_is_once() {
    use blaze_typeck::{CaptureKind, ClosureKind, Def, LangItem, Mutbl};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let unit = fx.unit();
    let copy = fx.tr("Copy", 1, std::vec![], &[]);
    fx.lang(LangItem::Copy, copy);
    let future = fx.tr("Future", 1, std::vec![], &["Output"]);
    fx.lang(LangItem::Future, future);
    let into = fx.tr("IntoFuture", 1, std::vec![], &["IntoFuture", "Output"]);
    fx.lang(LangItem::IntoFuture, into);
    let p = fx.param(0);
    let held = fx.alias(into, "IntoFuture", &[p]);
    let f = fx.trait_fn(into, "into_future", true, &[p], held);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(f));
    let output = fx.alias(future, "Output", &[p]);
    let bound = fx.bound(future, &[p]);
    fx.imp("impl<F: Future> IntoFuture for F", 1, p, Some((into, &[])),
        &[("IntoFuture", p), ("Output", output)], std::vec![bound]);
    let job = fx.strukt("Job", 0, &[]);
    let job = fx.adt(job, &[]);
    fx.imp("impl Future for Job", 0, job, Some((future, &[])), &[("Output", unit)], std::vec![]);
    let imp = fx.imp("impl Job", 0, job, None, &[], std::vec![]);
    fx.impl_fn(imp, "finish", true, &[job], unit);
    let borrowed = fx.r(Mutbl::Not, job);
    fx.impl_fn(imp, "observe", true, &[borrowed], unit);
    let owner = fx.func("example", 0, &[job], unit, std::vec![]);
    fx.finish();

    for (text, capture, kind) in [
        ("fn example(job: Job) { let _f = async || job.await; }", CaptureKind::ByValue, ClosureKind::FnOnce),
        ("fn example(job: Job) { let _f = async || job.finish(); }", CaptureKind::ByValue, ClosureKind::FnOnce),
        ("fn example(job: Job) { let _f = async || job.observe(); }", CaptureKind::ByRef, ClosureKind::Fn),
    ] {
        let (c, ranges, _) = lower_and_check(&fx, owner, text);
        assert!(c.diags.is_empty(), "{:?}\n{}", c.diags, typed_lines(&fx, text, &c, &ranges));
        assert_eq!(c.captures.column::<2>(), &[capture], "{text}");
        assert_eq!(c.closures.column::<1>(), &[kind], "{text}");
    }
}

const C_STRINGS: &str = "fn names() -> &'static CStr {
    let raw = cr#\"a\\b\"#;
    let byte = c\"\\xff\";
    let _ = (raw, byte);
    c\"hi\"
}";

/// The Reference, "Literal expressions", "C string literal expressions": "A C string literal
/// expression ... has type `&'static core::ffi::CStr`", raw (`cr#"..."#`) or not, and `\xNN`
/// may be any byte ("Tokens", "C string and raw C string literals"). `CStr` is found by its
/// lang item; without it the literal is the one missing-lang finding. Before the fix the
/// lowering refused every C string literal ("a C string literal").
#[test]
fn a_c_string_literal_is_a_static_cstr() {
    let mut world = blaze_typeck::fixture::Fx::new();
    let fx = &mut world;
    let cstr = fx.strukt("CStr", 0, &[]);
    fx.lang(blaze_typeck::LangItem::CStr, cstr);
    let cstr_ty = fx.adt(cstr, &[]);
    let r = fx.r(blaze_typeck::Mutbl::Not, cstr_ty);
    let names = fx.func("names", 0, &[], r, std::vec::Vec::new());
    fx.finish();
    let (c, ranges, _) = lower_and_check(&world, names, C_STRINGS);
    let lines = typed_lines(&world, C_STRINGS, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["c\"hi\" => &CStr", "cr#\"a\\b\"# => &CStr", "c\"\\xff\" => &CStr"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    // Without the lang item: one finding, at the literal.
    let mut bare = blaze_typeck::fixture::Fx::new();
    let fx = &mut bare;
    let unit = fx.unit();
    let f = fx.func("f", 0, &[], unit, std::vec::Vec::new());
    fx.finish();
    let text = "fn f() {\n    let _ = c\"hi\";\n}";
    let (c, _, _) = lower_and_check(&bare, f, text);
    assert_eq!(findings(text, &c), [(blaze_typeck::DiagCode::MissingLangItem, "c\"hi\"")]);
}

const SLASH: &str = "fn is_root(dir: &[u8]) -> bool {
    match dir {
        b\"/\" => true,
        _ => false,
    }
}";

/// The census' second wrong answer: a byte-string pattern against `&[u8]` is `&[u8]`
/// (`check_pat_lit`), not `&[u8; 1]`; before the fix it was E0308.
#[test]
fn byte_string_pattern_against_a_slice() {
    let mut world = blaze_typeck::fixture::Fx::new();
    let fx = &mut world;
    let (u8_, b) = (fx.u8(), fx.ty(blaze_typeck::PNode::Bool));
    let s = fx.ty(blaze_typeck::PNode::Slice(u8_));
    let r = fx.r(blaze_typeck::Mutbl::Not, s);
    let is_root = fx.func("is_root", 0, &[r], b, std::vec::Vec::new());
    fx.finish();
    let (c, ranges, _) = lower_and_check(&world, is_root, SLASH);
    let lines = typed_lines(&world, SLASH, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    // The control: against `&[u8; 1]` the pattern stays an array reference and checks too;
    // against `&[u16]` it is E0308.
    let mut world2 = blaze_typeck::fixture::Fx::new();
    let fx = &mut world2;
    let (u16_, b) = (fx.u16(), fx.ty(blaze_typeck::PNode::Bool));
    let s = fx.ty(blaze_typeck::PNode::Slice(u16_));
    let r = fx.r(blaze_typeck::Mutbl::Not, s);
    let is_root = fx.func("is_root", 0, &[r], b, std::vec::Vec::new());
    fx.finish();
    let (c, _, _) = lower_and_check(&world2, is_root, SLASH);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::Mismatch]);
}

/// Written types of the forms tests: a one-segment path naming a primitive.
struct PrimTypes;

impl Types for PrimTypes {
    fn ty(&mut self, v: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        use blaze_typeck::{Name, PNode, UintTy};
        let bad = Refused::Form { what: "a type the test world does not name", range: Default::default() };
        let crate::form::TypeRef::Path(p) = v.ty(t)? else { return Err(bad) };
        let path = v.path(p)?;
        let [seg] = v.segments(path)? else { return Err(bad) };
        let Some(n) = crate::form::Segment::decode(*seg)?.name else { return Err(bad) };
        let name = Name(v.name(n)?);
        let node = if name == Name::of("u32") {
            PNode::Uint(UintTy::U32)
        } else if name == Name::of("u8") {
            PNode::Uint(UintTy::U8)
        } else {
            return Err(bad);
        };
        Ok(tys.intern(node)?)
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Err(Refused::Form { what: "a repeat length (the test world evaluates none)", range: Default::default() })
    }

    fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(crate::typeck::no_const_arg(view, c))
    }
}

/// The two types the `try` block tests write, by their text: `u32` and `Result<Out, Error>`
/// of rung 7c's world.
struct Types7c {
    result: ItemKey,
    out: ItemKey,
    error: ItemKey,
}

impl Types for Types7c {
    fn ty(&mut self, v: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        use blaze_typeck::{List, PNode, UintTy};
        let bad = Refused::Form { what: "a type the test world does not name", range: Default::default() };
        let r = v.type_range(t).map_err(|_| bad)?;
        match v.text_at(r).map_err(|_| bad)? {
            "u32" => Ok(tys.intern(PNode::Uint(UintTy::U32))?),
            "Result<Out, Error>" => {
                let out = tys.intern(PNode::Adt(self.out, List::EMPTY))?;
                let error = tys.intern(PNode::Adt(self.error, List::EMPTY))?;
                let args = tys.intern_list(&[out, error])?;
                Ok(tys.intern(PNode::Adt(self.result, args))?)
            }
            _ => Err(bad),
        }
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Err(Refused::Form { what: "a repeat length (the test world evaluates none)", range: Default::default() })
    }

    fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(crate::typeck::no_const_arg(view, c))
    }
}

/// Written types by their text: each a struct or enum of the world with no arguments.
struct AdtsByText(std::vec::Vec<(&'static str, ItemKey)>);

impl Types for AdtsByText {
    fn ty(&mut self, v: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        let bad = Refused::Form { what: "a type the test world does not name", range: Default::default() };
        let r = v.type_range(t).map_err(|_| bad)?;
        let text = v.text_at(r).map_err(|_| bad)?;
        let Some(&(_, k)) = self.0.iter().find(|(n, _)| *n == text) else { return Err(bad) };
        Ok(tys.intern(blaze_typeck::PNode::Adt(k, blaze_typeck::List::EMPTY))?)
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Err(Refused::Form { what: "a repeat length (the test world evaluates none)", range: Default::default() })
    }

    fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(crate::typeck::no_const_arg(view, c))
    }
}

/// `offset_of!` (the pinned library, core/src/mem/mod.rs: "Expands to the offset in bytes of
/// a field from the beginning of the given type. The type may be a `struct`, `enum`, `union`,
/// or tuple. The field may be a nested field (`field1.field2`), but not an array index ...
/// The offset is returned as a `usize`"; `offset_of_enum`: "allows `enum` variants to be
/// traversed as if they were fields"). Its body form `builtin # offset_of(T, path)` is a
/// `usize` whose every step is a field of the type before it, `0.1` a tuple field of a tuple
/// field. A step the type lacks is E0609 at the form. Before the fix the lowering refused it
/// ("unhandled OFFSET_OF_EXPR").
#[test]
fn offset_of_is_a_usize_over_a_checked_field_path() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, u32_, usize_) = (fx.u8(), fx.u32(), fx.usize());
    let inner = fx.strukt("Inner", 0, &[("x", u8_)]);
    let inner_ty = fx.adt(inner, &[]);
    let pair = fx.tuple(&[u8_, u32_]);
    let nested = fx.tuple(&[u8_, pair]);
    let outer = fx.strukt("Outer", 0, &[("a", u32_), ("b", inner_ty), ("t", nested)]);
    let (e, _) = fx.enum_("E", 0, &[("V", &[u8_, u32_])]);
    let f = fx.func("f", 0, &[], usize_, std::vec::Vec::new());
    // A tuple struct's field is named by its digits in the world (front2's heads.rs): `p.0`
    // found it under `Name(0)` before, E0609 on every real tuple struct.
    let tup = fx.strukt("Tup", 0, &[("0", u8_)]);
    let tup_ty = fx.adt(tup, &[]);
    let g = fx.func("g", 0, &[tup_ty], u8_, std::vec::Vec::new());
    fx.finish();
    let (c, _, _) = lower_and_check(&fx, g, "fn g(p: Tup) -> u8 {\n    p.0\n}");
    assert!(c.diags.is_empty(), "`p.0` on a tuple struct: {:?}", c.diags);
    let mut types = AdtsByText(std::vec![("Outer", outer), ("E", e)]);
    let right = "fn f() -> usize {
    let a = builtin # offset_of(Outer, a);
    let b = builtin # offset_of(Outer, b.x);
    let c = builtin # offset_of(Outer, t.1.1);
    let d = builtin # offset_of(E, V.1);
    a + b + c + d
}";
    let (c, ranges, _) = lower_and_check_with(&fx, f, right, &mut types);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in [
        "builtin # offset_of(Outer, a) => usize",
        "builtin # offset_of(Outer, b.x) => usize",
        "builtin # offset_of(Outer, t.1.1) => usize",
        "builtin # offset_of(E, V.1) => usize",
    ] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn f() -> usize {
    let a = builtin # offset_of(Outer, nope);
    let b = builtin # offset_of(Outer, b.y);
    let c = builtin # offset_of(Outer, t.2);
    let d = builtin # offset_of(E, V);
    0
}";
    let (c, _, _) = lower_and_check_with(&fx, f, wrong, &mut types);
    use blaze_typeck::DiagCode::NoField;
    assert_eq!(
        findings(wrong, &c),
        [
            (NoField, "builtin # offset_of(Outer, nope)"),
            (NoField, "builtin # offset_of(Outer, b.y)"),
            (NoField, "builtin # offset_of(Outer, t.2)"),
            (NoField, "builtin # offset_of(E, V)"),
        ]
    );
}

/// Each finding as its code and the text at its item-relative range.
fn findings<'t>(text: &'t str, c: &blaze_typeck::Checked<'_>) -> std::vec::Vec<(blaze_typeck::DiagCode, &'t str)> {
    c.diags.iter().map(|d| (d.code, &text[d.at.start as usize..d.at.end as usize])).collect()
}

/// `trait Make { fn make() -> Self; }`, `impl Make for u32`, in scope, and `fn give() -> u32`.
fn make_world() -> (blaze_typeck::fixture::Fx, ItemKey) {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let make = fx.tr("Make", 1, std::vec::Vec::new(), &[]);
    let (u32_, self_) = (fx.u32(), fx.param(0));
    fx.trait_fn(make, "make", false, &[], self_);
    fx.imp("impl Make for u32", 0, u32_, Some((make, &[])), &[], std::vec::Vec::new());
    fx.use_trait(make);
    fx.func("give", 0, &[], u32_, std::vec::Vec::new());
    (fx, make)
}

/// Qualified paths (the Reference, "Qualified paths"): `<T as Tr>::x` is `Tr`'s own item `x`
/// with `Self = T`, and holds only where `T: Tr` does; `<T>::x` is `x` looked up on `T` as
/// `T::x` is. The wrong programs: an item `Tr` lacks is E0576 at the path, a non-trait in the
/// trait's place is E0404, and a self type without the impl is E0277.
#[test]
fn qualified_path_names_the_traits_item() {
    let (mut fx, _) = make_world();
    let u32_ = fx.u32();
    let pick = fx.func("pick", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let right = "fn pick() -> u32 {
    let a = <u32 as Make>::make();
    let b = <u32>::make();
    a
}";
    let (c, ranges, _) = lower_and_check_with(&fx, pick, right, &mut PrimTypes);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["<u32 as Make>::make() => u32", "<u32>::make() => u32"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn pick() -> u32 {
    let a = <u32 as Make>::nothing();
    let b = <u32 as give>::make();
    let c = <u8 as Make>::make();
    0
}";
    let (c, _, _) = lower_and_check_with(&fx, pick, wrong, &mut PrimTypes);
    let found = findings(wrong, &c);
    use blaze_typeck::DiagCode::{NoAssocItem, NotATrait, Unsatisfied};
    assert!(found.contains(&(NoAssocItem, "<u32 as Make>::nothing")), "{found:?}");
    assert!(found.contains(&(NotATrait, "<u32 as give>::make")), "{found:?}");
    assert!(found.iter().any(|&(code, _)| code == Unsatisfied), "{found:?}");
}

/// `try { STMTS TAIL }` (the Unstable Book, `try_blocks`): a block whose value is
/// `Try::from_output(TAIL)` and whose `?`s break to the block with
/// `FromResidual::from_residual(r)` instead of returning from the fn. So inside `bad`, whose
/// return type `u32` is no `FromResidual`, a `?` in a `try` block of type
/// `Result<Out, Error>` is right; the wrong program gives the block the type `u32`, which is
/// no `Try`: E0277 at the `?`'s operand's `branch` call. World: rung 7c.
#[test]
fn try_block_is_the_target_of_its_questions() {
    let (fx, s) = rung_7c();
    let mut types = Types7c { result: s.result, out: fx.key_of("Out"), error: fx.key_of("Error") };
    let right = "fn bad(io: Io) -> u32 {\n    let r: Result<Out, Error> = try { io.need_out()? };\n    0\n}";
    let (c, ranges, _) = lower_and_check_with(&fx, s.bad, right, &mut types);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("try { io.need_out()? } => Result<Out, Error>"), "{lines}");
    let wrong = "fn bad(io: Io) -> u32 {\n    let r: u32 = try { io.need_out()? };\n    0\n}";
    let (c, _, _) = lower_and_check_with(&fx, s.bad, wrong, &mut types);
    let got = findings(wrong, &c);
    assert!(
        !got.is_empty() && got.iter().all(|(code, _)| *code == blaze_typeck::DiagCode::Unsatisfied),
        "{got:?}"
    );
}

/// `<T as Tr>::Assoc::x` (the Reference, "Qualified paths"): `x` looked up on the projection
/// `<T as Tr>::Assoc` as on any type. The world: `trait Has { type Item; }`,
/// `impl Has for u32 { type Item = u8; }`, `impl u8 { fn zero() -> u8; }`. The wrong program
/// names an associated type `Has` lacks: E0576.
#[test]
fn qualified_path_through_an_associated_type() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let has = fx.tr("Has", 1, std::vec::Vec::new(), &["Item"]);
    let (u8_, u32_) = (fx.u8(), fx.u32());
    fx.imp("impl Has for u32", 0, u32_, Some((has, &[])), &[("Item", u8_)], std::vec::Vec::new());
    let inh = fx.imp("impl u8", 0, u8_, None, &[], std::vec::Vec::new());
    fx.impl_fn(inh, "zero", false, &[], u8_);
    let z = fx.func("z", 0, &[], u8_, std::vec::Vec::new());
    fx.finish();
    let right = "fn z() -> u8 {\n    <u32 as Has>::Item::zero()\n}";
    let (c, ranges, _) = lower_and_check_with(&fx, z, right, &mut PrimTypes);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("<u32 as Has>::Item::zero() => u8"), "{lines}");
    let wrong = "fn z() -> u8 {\n    <u32 as Has>::Nope::zero()\n}";
    let (c, _, _) = lower_and_check_with(&fx, z, wrong, &mut PrimTypes);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::NoAssocItem, "<u32 as Has>::Nope::zero")]);
}

/// `_ = e` (the Reference, "Destructuring assignments"): `let _ = e;`, the value dropped, `()`.
/// `_` anywhere else is an error at the `_`.
#[test]
fn underscore_on_the_left_of_an_assignment() {
    let (mut fx, _) = make_world();
    let (unit, u32_) = (fx.unit(), fx.u32());
    let drop_it = fx.func("drop_it", 0, &[], unit, std::vec::Vec::new());
    let tail = fx.func("tail", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let right = "fn drop_it() {\n    _ = give();\n}";
    let (c, ranges, _) = lower_and_check(&fx, drop_it, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["_ => u32", "_ = give() => ()"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn tail() -> u32 {\n    _\n}";
    let (c, _, _) = lower_and_check(&fx, tail, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::UnderscoreExpr, "_")]);
}

/// The finding for `_` outside an assignment's left side says what refuses it: the Reference,
/// "Underscore expressions", [expr.placeholder.lhs-assignment-only] ("They may only appear in the
/// left-hand side of an assignment."), as its `because`. Before diag-names-something-1 it was
/// E0000 with no type, no name and no reason (std census 3: `check: Diags: E0000 found None,
/// expected None,` on 46 of 49 rungs, from `panic_unwind::imp::panic`'s `[core::ptr::null(); _]`).
/// Prints `underscore finding: E0000 UnderscoreExpr Some(Reference { rule:
/// "expr.placeholder.lhs-assignment-only", says: "They may only appear in the left-hand side of an
/// assignment." })` once, for the tail. (A repeat count `_` is an inferred const, no expression:
/// typeck_tests/infer_const.rs.)
#[test]
fn underscore_finding_names_its_rule() {
    let (mut fx, _) = make_world();
    let u32_ = fx.u32();
    let tail = fx.func("tail", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let says = |text: &str, f| {
        let (c, _, _) = lower_and_check(&fx, f, text);
        let all: std::vec::Vec<blaze_typeck::Diag> = c.diags.iter().copied().collect();
        let [d] = all.as_slice() else { panic!("one finding wanted in {text:?}, got {all:?}") };
        assert_eq!(d.code, blaze_typeck::DiagCode::UnderscoreExpr, "{text:?}");
        assert!(
            matches!(d.because, Some(blaze_typeck::Because::Reference { rule: "expr.placeholder.lhs-assignment-only", .. })),
            "the finding for {text:?} says nothing: {d:?}"
        );
        std::println!("underscore finding: {} {:?} {:?}", d.code.code(), d.code, d.because);
    };
    says("fn tail() -> u32 {\n    _\n}", tail);
}

/// Destructuring assignments (the Reference, "Assignment expressions", "Destructuring
/// assignments": "If the left operand is a destructuring assignee expression ... desugared to
/// a pattern matching followed by sequential assignment"; the assignee's forms: a place, `_`,
/// a tuple, a slice, a tuple struct, a struct, `..` among them, nested). Each typed through its
/// `let`: the places take the right side's parts. And "Compound assignment expressions":
/// "Unlike other place operands, the assigned place operand must be a place expression", so
/// `(a, b) += (1, 2)` is E0067 at the tuple. Before the fix the lowering refused every one.
#[test]
fn destructuring_assignment_types_each_place() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (unit, u8_, u32_, usize_) = (fx.unit(), fx.u8(), fx.u32(), fx.usize());
    let s = fx.strukt("S", 0, &[("f", u32_), ("g", u8_)]);
    let s_ty = fx.adt(s, &[]);
    let (pair, _) = fx.enum_("E", 0, &[("P", &[u32_, u8_])]);
    let pair_ty = fx.adt(pair, &[]); // `enum E { P(u32, u8) }`
    let arr = fx.array(u32_, 2);
    let t = fx.tuple(&[u32_, u8_]);
    let tt = fx.tuple(&[u32_, u32_]);
    let f = fx.func("f", 0, &[u32_, u32_, arr, s_ty, t, pair_ty, u8_, tt, usize_, u8_], unit, std::vec::Vec::new());
    fx.finish();
    let right = "fn f(mut a: u32, mut b: u32, arr: [u32; 2], s: S, t: (u32, u8), p: Pair, mut c: u8, mut q: (u32, u32), i: usize, mut g: u8) {
    (a, b) = (b, a);
    [a, b] = arr;
    S { f: a, .. } = s;
    S { g, .. } = s;
    (a, _) = t;
    ((a, c), [b, ..]) = (t, arr);
    E::P(b, c) = p;
    (q.0, q.1) = (1, 2);
    let mut v = arr;
    (v[i], a) = (3, 4);
}";
    let (c, ranges, _) = lower_and_check(&fx, f, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["(a, b) = (b, a) => ()", "[a, b] = arr => ()", "S { f: a, .. } = s => ()", "S { g, .. } = s => ()", "(q.0, q.1) = (1, 2) => ()", "v[i] => u32", "q.0 => u32"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn f(mut a: u32, mut b: u32, arr: [u32; 2], s: S, t: (u32, u8), p: Pair, mut c: u8, mut q: (u32, u32), i: usize, mut g: u8) {
    (a, b) += (1, 2);
    (c, b) = t;
}";
    let (c, _, _) = lower_and_check(&fx, f, wrong);
    let found = findings(wrong, &c);
    assert!(found.contains(&(blaze_typeck::DiagCode::InvalidCompoundLhs, "(a, b)")), "{found:?}");
    assert!(found.iter().any(|&(code, at)| code == blaze_typeck::DiagCode::Mismatch && at.contains('b')), "{found:?}");
}

/// A `..` in a struct, tuple-struct or tuple pattern, under a reference scrutinee (the
/// Reference, "Patterns", "Rest patterns": "The rest pattern (the `..` token) acts as a
/// variable-length pattern which matches zero or more elements that haven't been matched
/// already before and after"; it has no type of its own, being no value; "Binding modes": "If
/// a binding pattern does not explicitly have `ref`, `ref mut`, or `mut`, then it uses the
/// default binding mode": `match &x { S { .. } => .. }` dereferences the scrutinee). bstd's
/// `match &self.reach { Reach::Shared { .. } => .. }` (io.rs `guest_grant`). Before the fix the
/// `..` node, in no list and so never reached, kept `{error}`: "UnknownInCleanBody { at:
/// PatTy(..) }".
#[test]
fn a_rest_pattern_under_a_reference_is_a_hole() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, u32_) = (fx.u8(), fx.u32());
    let s = fx.strukt("S", 0, &[("f", u32_), ("g", u8_)]);
    let s_ty = fx.adt(s, &[]);
    let s_ref = fx.r(blaze_typeck::Mutbl::Not, s_ty);
    let (e, _) = fx.enum_("E", 0, &[("P", &[u32_, u8_])]);
    let e_ty = fx.adt(e, &[]);
    let e_ref = fx.r(blaze_typeck::Mutbl::Not, e_ty);
    let t = fx.tuple(&[u32_, u8_]);
    let t_ref = fx.r(blaze_typeck::Mutbl::Not, t);
    let f = fx.func("f", 0, &[s_ref, e_ref, t_ref], u32_, std::vec::Vec::new());
    fx.finish();
    let text = "fn f(s: &S, p: &E, t: &(u32, u8)) -> u32 {
    let a = match s { S { .. } => 1 };
    let b = match p { E::P(..) => 2 };
    let c = match t { (x, ..) => *x };
    a + b + c
}";
    let (c, ranges, _) = lower_and_check(&fx, f, text);
    let lines = typed_lines(&fx, text, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("*x => u32"), "{lines}");
}

/// A block whose last statement diverges, written by a macro, as an arm and as a `-> !` body
/// (the Reference, "Block expressions": "the type of the block is the type of the final
/// operand, or `()` if the final operand is omitted", and "Never type": "Expressions of type
/// `!` can be coerced into any other type"; a block whose statements diverge is itself `!`:
/// core's `panic_2021` is `({ $crate::panicking::panic_fmt(..); })`). bstd's `None =>
/// panic!(..)` arms (cx.rs `cancellation`) and alloc's `fn capacity_overflow() -> ! {
/// panic!(..); }`: "DiagWithoutPlace { code: Mismatch, name: None }". A CONTROL: written by
/// hand these check clean (this harness expands no local macro: "to_typeck refused Form {
/// what: \"a macro call\" .. }"), so the census class is in the expansion's lowering, not here.
#[test]
fn a_macro_block_that_diverges_is_never() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u32_, never) = (fx.u32(), fx.never());
    fx.func("stop", 0, &[], never, std::vec::Vec::new());
    let f = fx.func("f", 0, &[u32_], u32_, std::vec::Vec::new());
    let g = fx.func("g", 0, &[], never, std::vec::Vec::new());
    fx.finish();
    let plain = "fn f(x: u32) -> u32 {
    match x { 0 => 1, _ => { stop(); } }
}";
    let (c, ranges, _) = lower_and_check(&fx, f, plain);
    assert!(c.diags.is_empty(), "plain: {:?}\n{}", c.diags, typed_lines(&fx, plain, &c, &ranges));
    let body = "fn g() -> ! {
    { stop(); };
}";
    let (c, ranges, _) = lower_and_check(&fx, g, body);
    assert!(c.diags.is_empty(), "body: {:?}\n{}", c.diags, typed_lines(&fx, body, &c, &ranges));
}

/// A block's final operand is the last statement LEFT after cfg stripping, not the last one
/// written. The Reference, "Conditional compilation": "If any predicate is false, the form is
/// removed from the source code"; "Block expressions": "The syntax for a block is {, then any
/// inner attributes, then any number of statements, then an optional expression, called the
/// final operand, and finally a }" and "When a block contains a final operand, the block has
/// the type and value of that final operand"; "Statements": a block-like expression statement
/// that omits its `;` "must be type ()" only while it IS a statement. tempo's
/// zones/node dev.rs `write_owner_only` is this shape (`#[cfg(unix)] { .. }` then
/// `#[cfg(not(unix))] { .. }` last). `all()` is true and `any()` false whatever the cfg set.
/// Expected: both bodies check with no diagnostic; cargo prints
/// "test typeck_tests::the_final_operand_is_chosen_after_cfg_stripping ... ok".
/// A wrong fix fails it: the `{ 1 }` kept as a statement is a mismatch (`1` against `()`, and
/// the block `()` against `u32`); the stripped `{ () }` kept as the tail is a mismatch too.
#[test]
fn the_final_operand_is_chosen_after_cfg_stripping() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let u32_ = fx.u32();
    let f = fx.func("f", 0, &[], u32_, std::vec::Vec::new());
    let g = fx.func("g", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let blocks = "fn f() -> u32 {
    #[cfg(all())]
    {
        1
    }

    #[cfg(any())]
    {
        ()
    }
}";
    let (c, ranges, _) = lower_and_check(&fx, f, blocks);
    assert!(c.diags.is_empty(), "blocks: {:?}\n{}", c.diags, typed_lines(&fx, blocks, &c, &ranges));
    // A block-like statement still followed by a statement that stays is not the final operand:
    // `{ 1; }` is `()`, and the kept `2` is the tail.
    let kept = "fn g() -> u32 {
    #[cfg(all())]
    {
        1;
    }
    #[cfg(any())]
    {
        ()
    }
    2
}";
    let (c, ranges, _) = lower_and_check(&fx, g, kept);
    assert!(c.diags.is_empty(), "kept: {:?}\n{}", c.diags, typed_lines(&fx, kept, &c, &ranges));
}

/// A closure called with the wrong number of arguments (the Reference, "Call expressions": a
/// closure operand's call is `Fn::call(&f, (args..))`, its arguments one `Args` tuple; "Closure
/// types": the closure implements the `Fn*` traits with its parameters as `Args`): E0057, the
/// Fn-trait call's error, as a `F: Fn(u32)` parameter's. Before the fix it was E0061.
#[test]
fn a_closure_called_with_the_wrong_count_is_e0057() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let unit = fx.unit();
    let g = fx.func("g", 0, &[], unit, std::vec::Vec::new());
    fx.finish();
    let right = "fn g() {\n    let f = |x| x;\n    f(1u32);\n}";
    let (c, ranges, _) = lower_and_check(&fx, g, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    let wrong = "fn g() {\n    let f = |x| x;\n    f(1u32);\n    f(1u32, 2u32);\n}";
    let (c, _, _) = lower_and_check(&fx, g, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::FnTraitArgCount, "f(1u32, 2u32)")]);
}

/// `extern "rust-call"` (the `unboxed_closures` feature; core's `extern "rust-call" fn
/// call(&self, args: Args)` with `Args: Tuple`): the last parameter must be `Tuple`; a tuple is,
/// `u32` is E0277 at that parameter ("functions with the \"rust-call\" ABI must take a single
/// non-self tuple argument"). Before the fix nothing was checked.
#[test]
fn a_rust_call_fn_takes_its_arguments_as_one_tuple() {
    use blaze_typeck::{Abi, FnPtrHeader, Safety};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let tuple = fx.tr("Tuple", 1, std::vec::Vec::new(), &[]);
    fx.lang(blaze_typeck::LangItem::Tuple, tuple);
    let (unit, u32_) = (fx.unit(), fx.u32());
    let pair = fx.tuple(&[u32_, u32_]);
    let good = fx.func("good", 0, &[pair], unit, std::vec::Vec::new());
    let bad = fx.func("bad", 0, &[u32_], unit, std::vec::Vec::new());
    let header = FnPtrHeader { abi: Abi::RustCall, safety: Safety::Safe, c_variadic: false };
    fx.set_header(good, header);
    fx.set_header(bad, header);
    fx.finish();
    let right = "extern \"rust-call\" fn good(args: (u32, u32)) {}";
    let (c, _, _) = lower_and_check(&fx, good, right);
    assert!(c.diags.is_empty(), "diagnostics {:?}", c.diags);
    let wrong = "extern \"rust-call\" fn bad(args: u32) {}";
    let (c, _, _) = lower_and_check(&fx, bad, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::Unsatisfied, "args")]);
}

/// Raw borrows (the Reference, "Raw borrow operators"): `&raw const p` is `*const T` and
/// `&raw mut p` is `*mut T` for a place `p: T`; the operand must be a place (E0745).
#[test]
fn raw_borrow_makes_a_pointer() {
    use blaze_typeck::{Mutbl, PNode};
    let (mut fx, _) = make_world();
    let u32_ = fx.u32();
    let ptr = fx.ty(PNode::Ptr(Mutbl::Not, u32_));
    let addr = fx.func("addr", 0, &[u32_], ptr, std::vec::Vec::new());
    fx.finish();
    let right = "fn addr(x: u32) -> *const u32 {\n    let p = &raw mut x;\n    &raw const x\n}";
    let (c, ranges, _) = lower_and_check(&fx, addr, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["&raw mut x => *mut u32", "&raw const x => *const u32"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn addr(x: u32) -> *const u32 {\n    &raw const give()\n}";
    let (c, _, _) = lower_and_check(&fx, addr, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::RawBorrowOfTemporary, "give()")]);
}

/// Range patterns (the Reference, "Range patterns"): each end a literal, a negated literal or
/// a constant's path, of an integer, float or `char` type (E0029 otherwise), both ends and the
/// scrutinee one type (E0308 otherwise); the pattern peels references as any non-reference
/// pattern does; `lo..`, `..=hi` and `lo..hi` are the same rule.
#[test]
fn range_patterns() {
    use blaze_typeck::{Mutbl, PNode};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, b) = (fx.u8(), fx.ty(PNode::Bool));
    let ru8 = fx.r(Mutbl::Not, u8_);
    let s = fx.ty(PNode::Str);
    let rs = fx.r(Mutbl::Not, s);
    let hex = fx.func("hex", 0, &[ru8], b, std::vec::Vec::new());
    let word = fx.func("word", 0, &[rs], b, std::vec::Vec::new());
    fx.finish();
    let right = "fn hex(b: &u8) -> bool {
    match b {
        b'0'..=b'9' | b'a'..=b'f' => true,
        200.. => true,
        ..=b' ' => true,
        10..20 => true,
        _ => false,
    }
}";
    let (c, ranges, _) = lower_and_check(&fx, hex, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    let wrong = "fn hex(b: &u8) -> bool {\n    match b {\n        0u16..=9 => true,\n        _ => false,\n    }\n}";
    let (c, _, _) = lower_and_check(&fx, hex, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::Mismatch, "0u16")]);
    let wrong = "fn word(s: &str) -> bool {\n    match s {\n        \"a\"..=\"z\" => true,\n        _ => false,\n    }\n}";
    let (c, _, _) = lower_and_check(&fx, word, wrong);
    use blaze_typeck::DiagCode::RangePatType;
    assert_eq!(findings(wrong, &c), [(RangePatType, "\"a\""), (RangePatType, "\"z\"")]);
}

/// Slice patterns with `..` (the Reference, "Slice patterns"): against `[T; N]`, the `k`
/// elements beside `..` must be at most `N` (exactly `N` with no `..`; E0023 otherwise) and
/// `x @ ..` is `[T; N - k]`; against `[T]`, any number, and `x @ ..` is `[T]`; through a
/// reference by the default binding mode, so `tail @ ..` of a `&[u8]` binds `&[u8]`.
#[test]
fn slice_patterns_with_rest() {
    use blaze_typeck::{Mutbl, PNode};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let u8_ = fx.u8();
    let sl = fx.ty(PNode::Slice(u8_));
    let rsl = fx.r(Mutbl::Not, sl);
    let arr = fx.array(u8_, 4);
    let ends = fx.func("ends", 0, &[rsl, arr], u8_, std::vec::Vec::new());
    fx.finish();
    let right = "fn ends(s: &[u8], a: [u8; 4]) -> u8 {
    let [x, rest @ ..] = a;
    let r = rest;
    match s {
        [first, .., last] => *first + *last,
        [head, tail @ ..] => {
            let t = tail;
            *head
        }
        [] => x,
    }
}";
    let (c, ranges, _) = lower_and_check(&fx, ends, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    for want in ["rest => [u8; 3]", "tail => &[u8]", "*first + *last => u8"] {
        assert!(lines.contains(want), "no `{want}` in\n{lines}");
    }
    let wrong = "fn ends(s: &[u8], a: [u8; 4]) -> u8 {\n    let [p, q, r, t, u, ..] = a;\n    p\n}";
    let (c, _, _) = lower_and_check(&fx, ends, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::PatternArity, "[p, q, r, t, u, ..]")]);
}

/// Inline constants (the Reference, "Const blocks"): `const { .. }` is its block's type, typed
/// in the enclosing body's inference with the expectation it is under.
#[test]
fn const_block_is_its_blocks_type() {
    let mut fx = blaze_typeck::fixture::Fx::new();
    let u32_ = fx.u32();
    let k = fx.func("k", 0, &[], u32_, std::vec::Vec::new());
    fx.finish();
    let right = "fn k() -> u32 {\n    const { 1 }\n}";
    let (c, ranges, _) = lower_and_check(&fx, k, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("const { 1 } => u32"), "{lines}");
    let wrong = "fn k() -> u32 {\n    const { 1u8 }\n}";
    let (c, _, _) = lower_and_check(&fx, k, wrong);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::Mismatch]);
}

/// Literal patterns (the Reference, "Literal patterns", and rustc's `check_pat_lit`): a
/// literal pattern has its literal's type, so a string literal is `&str` and a byte literal
/// `u8`; a non-reference literal (a number, `b'x'`, `'c'`, `true`) peels the scrutinee's
/// references by the default binding mode, a string or byte-string literal (itself a
/// reference) peels none, so `"a"` against `&&str` is E0308; an `&` pattern takes one
/// reference off by hand.
#[test]
fn literal_patterns_and_references() {
    use blaze_typeck::{Mutbl, PNode};
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, u32_) = (fx.u8(), fx.u32());
    let s = fx.ty(PNode::Str);
    let rs = fx.r(Mutbl::Not, s);
    let rrs = fx.r(Mutbl::Not, rs);
    let ru8 = fx.r(Mutbl::Not, u8_);
    let lits = fx.func("lits", 0, &[rs, ru8], u32_, std::vec::Vec::new());
    let deep = fx.func("deep", 0, &[rrs], u32_, std::vec::Vec::new());
    fx.finish();
    let right = "fn lits(s: &str, b: &u8) -> u32 {
    let n = match s {
        \"a\" => 1,
        _ => 2,
    };
    match b {
        b'a' => n,
        &b'b' => 2,
        _ => 3,
    }
}";
    let (c, ranges, _) = lower_and_check(&fx, lits, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    let wrong = "fn deep(s: &&str) -> u32 {\n    match s {\n        \"a\" => 1,\n        _ => 2,\n    }\n}";
    let (c, _, _) = lower_and_check(&fx, deep, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::Mismatch, "\"a\"")]);
}

/// `::x` is a global path (the Reference, "Paths"): it starts at the root rustc names
/// `{{root}}`, which the world resolves (the crate root in 2015, the extern prelude after).
#[test]
fn global_path_starts_at_the_root() {
    let alloc = Alloc::global();
    let text = "fn f() -> u32 {\n    ::give()\n}";
    let lowered = lower_body(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc).expect("lowers");
    let view = lowered.view();
    let mut tys = PortableTys::new_in(alloc);
    let body = to_typeck(&view, ItemKey(1), &mut tys, &mut NoTypes, alloc).expect("converts");
    let names: std::vec::Vec<blaze_typeck::Name> = body.segments.iter().map(|s| s.name).collect();
    assert_eq!(names, [blaze_typeck::Name::of("{{root}}"), blaze_typeck::Name::of("give")]);
}

/// The const-generics world: `fn n<const N: usize>() -> [u8; N]`, `fn takes<const N: usize>(a:
/// [u8; N]) -> [u8; N]`, `struct Buf<const N: usize> { b: [u8; N] }`, and the bodies' owners.
fn const_world() -> (blaze_typeck::fixture::Fx, [ItemKey; 6]) {
    use blaze_typeck::PNode;
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, usize_) = (fx.u8(), fx.usize());
    let n0 = fx.ty(PNode::ConstParam(0));
    let arr_n = fx.ty(PNode::Array(u8_, n0));
    let n = fx.func("n", 1, &[], arr_n, std::vec::Vec::new());
    fx.const_param(n, 0, usize_);
    let takes = fx.func("takes", 1, &[arr_n], arr_n, std::vec::Vec::new());
    fx.const_param(takes, 0, usize_);
    let buf = fx.strukt("Buf", 1, &[("b", arr_n)]);
    fx.const_param(buf, 0, usize_);
    let arr3 = fx.array(u8_, 3);
    let main3 = fx.func("main3", 0, &[], arr3, std::vec::Vec::new());
    let unit = fx.unit();
    let main = fx.func("main", 0, &[], unit, std::vec::Vec::new());
    // `fn f<const N: usize>(a: [u8; N]) -> usize`: `N` names its own const parameter.
    let f = fx.func("f", 1, &[arr_n], usize_, std::vec::Vec::new());
    fx.const_param(f, 0, usize_);
    fx.use_path("N", blaze_typeck::Def::TyParam(0));
    fx.finish();
    (fx, [n, takes, buf, main3, main, f])
}

/// Explicit const arguments (the Reference, "Generic arguments" and "Const generics"): a
/// literal for a const parameter is a value of the parameter's type, so `n::<3>()` is
/// `[u8; 3]`. The type of a const argument must match its parameter's (E0308 for
/// `n::<true>()`), and a type given for a const parameter is E0747 (`n::<u8>()`).
#[test]
fn an_explicit_const_argument_is_a_value_of_its_parameters_type() {
    let (fx, [_, _, _, main3, _, _]) = const_world();
    let right = "fn main3() -> [u8; 3] {\n    n::<3>()\n}";
    let (c, ranges, _) = lower_and_check(&fx, main3, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("n::<3>() => [u8; 3]"), "{lines}");
    // The control: `::<4>` against the `[u8; 3]` return type is E0308 at the call.
    let four = "fn main3() -> [u8; 3] {\n    n::<4>()\n}";
    let (c, _, _) = lower_and_check(&fx, main3, four);
    assert_eq!(findings(four, &c), [(blaze_typeck::DiagCode::Mismatch, "n::<4>()")]);
    let wrong = "fn main3() -> [u8; 3] {\n    n::<true>()\n}";
    let (c, _, _) = lower_and_check(&fx, main3, wrong);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::Mismatch]);
    let kind = "fn main3() -> [u8; 3] {\n    n::<u8>()\n}";
    let (c, _, _) = lower_and_check_with(&fx, main3, kind, &mut PrimTypes);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::GenericArgKind]);
}

/// A const parameter left to inference takes its value from an argument (the Reference,
/// "Const generics": const arguments may be inferred): `takes(a)` with `a: [u8; 3]` is
/// `[u8; 3]`; with a two-element array against the `[u8; 3]` return type it is E0308.
#[test]
fn a_const_argument_is_inferred_from_an_array() {
    let (fx, [_, _, _, main3, _, _]) = const_world();
    let right = "fn main3() -> [u8; 3] {\n    let a = [1u8, 2, 3];\n    takes(a)\n}";
    let (c, ranges, _) = lower_and_check(&fx, main3, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("takes(a) => [u8; 3]"), "{lines}");
    let wrong = "fn main3() -> [u8; 3] {\n    let a = [1u8, 2];\n    takes(a)\n}";
    let (c, _, _) = lower_and_check(&fx, main3, wrong);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::Mismatch]);
}

/// A struct generic over a const (the Reference, "Const generics"): `Buf::<2> { b: [1u8, 2] }`
/// checks, its field `[u8; N]` at `N = 2`; `Buf::<3>` with a two-element field is E0308.
#[test]
fn a_struct_generic_over_a_const() {
    let (fx, [_, _, _, _, main, _]) = const_world();
    let right = "fn main() {\n    let x = Buf::<2> { b: [1u8, 2] };\n}";
    let (c, ranges, _) = lower_and_check(&fx, main, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    let wrong = "fn main() {\n    let x = Buf::<3> { b: [1u8, 2] };\n}";
    let (c, _, _) = lower_and_check(&fx, main, wrong);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<std::vec::Vec<_>>(), [blaze_typeck::DiagCode::Mismatch]);
}

/// A const parameter used as a value in its item's body is of its declared type (the
/// Reference, "Const generics"): `N` in `fn f<const N: usize>(a: [u8; N]) -> usize { N }` is
/// `usize`; returned where a `u8` is expected it is E0308.
#[test]
fn a_const_parameter_is_a_value_in_its_body() {
    let (fx, [_, _, _, _, _, f]) = const_world();
    let right = "fn f(a: [u8; N]) -> usize {\n    N\n}";
    let (c, ranges, _) = lower_and_check(&fx, f, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("N => usize"), "{lines}");
    let wrong = "fn f(a: [u8; N]) -> usize {\n    let b: u8 = N;\n    N\n}";
    let (c, _, _) = lower_and_check_with(&fx, f, wrong, &mut PrimTypes);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::Mismatch, "N")]);
}

/// A const argument the caller cannot value does not stop the body: it is a hole with its gap
/// listed (`TypeckBody::const_gaps`: why, where, and the constant's own text by its range),
/// and the rest of the body is converted and checked. `NoTypes` evaluates no constant
/// (`no_const_arg`), so `{ 1 + 2 }` is `ConstGapWhy::NoEvaluator`; a negated literal with no
/// suffix is `NegativeNoSuffix`. The hole is a const variable the declared `[u8; 3]` settles
/// (the Reference, "Const generics": a const argument may be inferred): no diagnostic, the
/// call typed `[u8; 3]`. Before, `to_typeck` refused the whole body ("a const argument that
/// is no literal"): the `expect` below fails on that.
///
/// Prints: `test typeck_tests::a_const_argument_with_no_value_is_a_gap_and_the_body_goes_on ... ok`
#[test]
fn a_const_argument_with_no_value_is_a_gap_and_the_body_goes_on() {
    use crate::typeck::{ConstGapWhy, ConstSite};
    /// `NoTypes`, as a caller that reads the gaps (`Types::takes_const_gaps`).
    struct GapsRead;
    impl Types for GapsRead {
        fn ty(&mut self, view: &View<'_>, t: TypeId, tys: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            NoTypes.ty(view, t, tys)
        }
        fn repeat_len(&mut self, view: &View<'_>, len: ExprId) -> Result<u64, Refused> {
            NoTypes.repeat_len(view, len)
        }
        fn const_arg(&mut self, view: &View<'_>, c: crate::ConstIdx, tys: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
            NoTypes.const_arg(view, c, tys)
        }
        fn takes_const_gaps(&self) -> bool {
            true
        }
    }
    // A caller that does not read the gaps still gets the refusal, by name: never a hole.
    {
        let text = "fn main3() -> [u8; 3] {\n    n::<{ 1 + 2 }>()\n}";
        let alloc = Alloc::global();
        let lowered = lower_body(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc).unwrap_or_else(|need| panic!("needs {:?}", need.atoms));
        let view = lowered.view();
        let mut tys = PortableTys::new_in(alloc);
        match to_typeck(&view, ItemKey(0), &mut tys, &mut NoTypes, alloc) {
            Err(Refused::Form { what, .. }) => assert_eq!(what, crate::typeck::NO_CONST_ARG),
            Err(e) => panic!("another refusal: {e:?}"),
            Ok(_) => panic!("a caller that reads no gap was handed a hole"),
        }
    }
    let (fx, [_, _, _, main3, _, _]) = const_world();
    let gaps_of = |text: &'static str| {
        let alloc = Alloc::global();
        let lowered = match lower_body(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc) {
            Ok(l) => l,
            Err(need) => panic!("{text}: needs {:?}", need.atoms),
        };
        let view = lowered.view();
        let mut tys = PortableTys::new_in(alloc);
        let body = to_typeck(&view, main3, &mut tys, &mut GapsRead, alloc).unwrap_or_else(|e| panic!("a const gap is no refusal of the body: {e:?}"));
        body.const_gaps.iter().map(|g| (g.why, g.site, &text[g.range.start as usize..g.range.end as usize])).collect::<std::vec::Vec<_>>()
    };
    let block = "fn main3() -> [u8; 3] {\n    n::<{ 1 + 2 }>()\n}";
    assert_eq!(gaps_of(block), [(ConstGapWhy::NoEvaluator, ConstSite::Arg, "{ 1 + 2 }")]);
    let (c, ranges, _) = lower_and_check_with(&fx, main3, block, &mut GapsRead);
    let lines = typed_lines(&fx, block, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("n::<{ 1 + 2 }>() => [u8; 3]"), "{lines}");
    let negative = "fn main3() -> [u8; 3] {\n    n::<-1>()\n}";
    assert_eq!(gaps_of(negative), [(ConstGapWhy::NegativeNoSuffix, ConstSite::Arg, "-1")]);
    // A literal is still a value, with no gap: the control.
    assert_eq!(gaps_of("fn main3() -> [u8; 3] {\n    n::<3>()\n}"), []);
}

/// The literal forms a const argument may take (the Reference, "Generic arguments"): its
/// value is read from its text, a negative one in two's complement at its suffix's width.
#[test]
fn const_literals_read_their_values() {
    let v = |t: &str| crate::typeck::const_literal_bits(t);
    assert_eq!(v("3"), Some(3));
    assert_eq!(v("0x1_0u8"), Some(16));
    assert_eq!(v("-1i8"), Some(0xff));
    assert_eq!(v("true"), Some(1));
    assert_eq!(v("'a'"), Some(97));
    // Refused: out of the suffix's range, a negative unsigned, an escape, a block.
    assert_eq!(v("256u8"), None);
    assert_eq!(v("-1u8"), None);
    assert_eq!(v("'\\n'"), None);
    assert_eq!(v("{ 3 }"), None);
}

/// A call argument that is an integer literal, or a `-` of one, has its value as written in
/// `call_arg_ints` (a `#[rustc_legacy_const_generics]` callee's const argument; blaze-typeck
/// rules/legacy_const.rs): hex, `_` and a suffix read, the `-` left to the checker, the entry
/// at the literal's own id, ascending by id across nested calls; a literal past `u128` and a
/// literal that is no call argument are not recorded.
/// Prints `test typeck_tests::call_arguments_that_are_integer_literals_keep_their_values ... ok`.
#[test]
fn call_arguments_that_are_integer_literals_keep_their_values() {
    let text = "fn main() {
    let x = 5;
    f(x, 0x1f);
    f(x, -3);
    f(x, 1_000u16);
    f(g(7), (2));
    f(x, 340282366920938463463374607431768211456);
}";
    let alloc = Alloc::global();
    let lowered = match lower_body(text, Edition::E2024, &mut NoCfg, &mut crate::NoModule, alloc) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let view = lowered.view();
    let mut tys = PortableTys::new_in(alloc);
    let body = match to_typeck(&view, ItemKey(1), &mut tys, &mut NoTypes, alloc) {
        Ok(b) => b,
        Err(e) => panic!("to_typeck refused {e:?} in\n{text}"),
    };
    let values: std::vec::Vec<u128> = body.call_arg_ints.iter().map(|&(_, v)| v).collect();
    assert_eq!(values, [31, 3, 1000, 7, 2], "the values, in id order");
    assert!(body.call_arg_ints.windows(2).all(|w| w[0].0 < w[1].0), "ascending by id: {:?}", &body.call_arg_ints[..]);
    for &(e, v) in body.call_arg_ints.iter() {
        assert!(matches!(view.expr(ExprId(e.0)), Ok(crate::form::Expr::Lit(_))), "the entry for {v} is at a literal, not its `-` or parentheses");
    }
}

/// The world for an impl generic over a const: `trait Tr<U> {}`,
/// `impl<const N: usize> Tr<[u8; N]> for [u16; N]`, `fn pick<T: Tr<U>, U>(t: T) -> U`,
/// `fn need<T: Tr<U>, U>(t: T, u: U)`, and `main4() -> [u8; 4]`, `main() -> ()`.
fn const_impl_world() -> (blaze_typeck::fixture::Fx, [ItemKey; 2]) {
    use blaze_typeck::PNode;
    let mut fx = blaze_typeck::fixture::Fx::new();
    let (u8_, u16_, usize_, unit) = (fx.u8(), fx.u16(), fx.usize(), fx.unit());
    let tr = fx.tr("Tr", 2, std::vec::Vec::new(), &[]);
    let n0 = fx.ty(PNode::ConstParam(0));
    let (a8, a16) = (fx.ty(PNode::Array(u8_, n0)), fx.ty(PNode::Array(u16_, n0)));
    let imp = fx.imp("impl<const N: usize> Tr<[u8; N]> for [u16; N]", 1, a16, Some((tr, &[a8])), &[], std::vec::Vec::new());
    fx.const_param(imp, 0, usize_);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let b = fx.bound(tr, &[p0, p1]);
    fx.func("pick", 2, &[p0], p1, std::vec![b]);
    let b = fx.bound(tr, &[p0, p1]);
    fx.func("need", 2, &[p0, p1], unit, std::vec![b]);
    let arr4 = fx.array(u8_, 4);
    let main4 = fx.func("main4", 0, &[], arr4, std::vec::Vec::new());
    let main = fx.func("main", 0, &[], unit, std::vec::Vec::new());
    fx.finish();
    (fx, [main4, main])
}

/// An impl's const parameter is a const to be inferred when the impl is matched (the
/// Reference, "Const generics": a const parameter is a value; "Trait and lifetime bounds"):
/// `impl<const N: usize> Tr<[u8; N]> for [u16; N]` matched for `[u16; 4]` binds `N = 4`, so
/// `pick([1u16, 2, 3, 4])` is `[u8; 4]`.
#[test]
fn an_impls_const_parameter_is_bound_by_selection() {
    let (fx, [main4, _]) = const_impl_world();
    let right = "fn main4() -> [u8; 4] {\n    pick([1u16, 2, 3, 4])\n}";
    let (c, ranges, _) = lower_and_check(&fx, main4, right);
    let lines = typed_lines(&fx, right, &c, &ranges);
    assert!(c.diags.is_empty(), "diagnostics {:?}\n{lines}", c.diags);
    assert!(lines.contains("pick([1u16, 2, 3, 4]) => [u8; 4]"), "{lines}");
}

/// Both uses of the impl's `N` take one value. The first argument fixes `T = [u16; 3]`,
/// so the sole impl fixes `U = [u8; 3]`. The second argument must coerce to that formal
/// (the Reference, "Type coercions", "Coercion sites"). Its written length is four.
#[test]
fn an_impls_const_parameter_takes_one_value() {
    let (fx, [_, main]) = const_impl_world();
    let wrong = "fn main() {\n    need([1u16, 2, 3], [1u8, 2, 3, 4])\n}";
    let (c, _, _) = lower_and_check(&fx, main, wrong);
    assert_eq!(findings(wrong, &c), [(blaze_typeck::DiagCode::Mismatch, "[1u8, 2, 3, 4]")]);
    let diag = &c.diags[0];
    assert_eq!(show(&fx, c.types.view(), diag.expected.expect("the second formal")), "[u8; 3]");
    assert_eq!(show(&fx, c.types.view(), diag.found.expect("the second argument")), "[u8; 4]");
    let right = "fn main() {\n    need([1u16, 2, 3], [1u8, 2, 3])\n}";
    let (c, _, _) = lower_and_check(&fx, main, right);
    assert!(c.diags.is_empty(), "diagnostics {:?}", c.diags);
}

/// `impl<X> Tr<X> for (X,)` uses one type in both places. Checking the tuple first
/// determines the other formal. Checking the trait argument first leaves Self unknown.
#[test]
fn an_impls_type_parameter_takes_one_type() {
    use blaze_typeck::DiagCode::{Mismatch, Unsatisfied};
    use std::vec::Vec;

    let mut fx = blaze_typeck::fixture::Fx::new();
    let tr = fx.tr("Tr", 2, Vec::new(), &[]);
    let (a, b, unit) = (fx.param(0), fx.param(1), fx.unit());
    let tuple = fx.tuple(&[a]);
    fx.imp("impl<X> Tr<X> for (X,)", 1, tuple, Some((tr, &[a])), &[], Vec::new());
    let bound = fx.bound(tr, &[a, b]);
    fx.func("need", 2, &[a, b], unit, std::vec![bound]);
    fx.func("rhs_first", 2, &[b, a], unit, std::vec![bound]);
    let main = fx.func("main", 0, &[], unit, Vec::new());
    fx.finish();

    let text = "fn main() { need((1u8,), false) }";
    let (c, _, _) = lower_and_check(&fx, main, text);
    assert_eq!(findings(text, &c), [(Mismatch, "false")]);

    // Committing the impl while Self is unknown would demand a (bool,) argument here.
    // Both formals instead take their arguments' types, leaving (u8,): Tr<bool> unmet.
    let text = "fn main() { rhs_first(false, (1u8,)) }";
    let (c, _, _) = lower_and_check(&fx, main, text);
    assert_eq!(c.diags.iter().map(|d| d.code).collect::<Vec<_>>(), [Unsatisfied]);
}

/// `pattern_type!(T is PAT)`, the pinned library's built-in (core/src/pat.rs), read from its
/// input text and lowered to blaze-typeck's canonical node (typeck.md, "Pattern types"). The
/// forms are core's own: num/niche_types.rs (`Nanoseconds(u32 is 0..=999_999_999)`,
/// `NonZeroU32Inner(u32 is 1..)`, `NonZeroI8Inner(i8 is ..0 | 1..)`, `UsizeNoHighBit(usize is
/// 0..=HALF_USIZE)`, `U32NotAllOnes(u32 is 0..u32::MAX)`, `NonZeroCharInner(char is
/// '\u{1}'..='\u{10ffff}')`) and ptr/non_null.rs (`*const T is !null`). One set of values is one
/// node; a form outside these is a refusal naming it.
mod pattern_types {
    use blaze_own::Alloc;
    use blaze_typeck::{IntTy, Mutbl, PNode, PTy, PortableTys, UintTy};

    use crate::typeck::{Refused, parse_pattern_type, pattern_type};

    fn at() -> blaze_surface::Range {
        blaze_surface::Range::new(0, 1)
    }

    /// The node and its ranges' bits for `text` over `base`, paths evaluated by `consts`.
    fn lower(tys: &mut PortableTys<'_>, base: PNode, text: &str) -> Result<(PTy, std::vec::Vec<u128>), Refused> {
        let base = tys.intern(base)?;
        let read = parse_pattern_type(text, at())?;
        let mut eval = |p: &str| match p {
            "HALF_USIZE" => Ok(u128::from(u64::MAX >> 1)),
            "u32::MAX" => Ok(u128::from(u32::MAX)),
            _ => Err(Refused::Form { what: "an unknown constant", range: at() }),
        };
        let t = pattern_type(tys, base, &read, 64, at(), &mut eval)?;
        let PNode::Pat(_, l) = tys.view().node(t)? else { panic!("not a pattern type") };
        let mut bits = std::vec::Vec::new();
        for &v in tys.view().list(l)? {
            let PNode::ConstValue { bits: b, .. } = tys.view().node(v)? else { panic!("a range end is not a value") };
            bits.push(b);
        }
        Ok((t, bits))
    }

    #[test]
    fn cores_forms_lower_to_canonical_ranges() {
        let mut tys = PortableTys::new_in(Alloc::global());
        let t = &mut tys;
        let u32_ = PNode::Uint(UintTy::U32);
        assert_eq!(lower(t, u32_, "u32 is 0..=999_999_999").expect("lowers").1, [0, 999_999_999]);
        let (one_up, r) = lower(t, u32_, "u32 is 1..").expect("lowers");
        assert_eq!(r, [1, 0xffff_ffff]);
        let (written_out, _) = lower(t, u32_, "u32 is 1..=4_294_967_295u32").expect("lowers");
        assert_eq!(one_up, written_out, "one set of values is one node");
        assert_eq!(lower(t, u32_, "u32 is 0..u32::MAX").expect("lowers").1, [0, 0xffff_fffe]);
        assert_eq!(lower(t, PNode::Int(IntTy::I8), "i8 is ..0 | 1..").expect("lowers").1, [1, 0xff]);
        assert_eq!(lower(t, PNode::Int(IntTy::I32), "i32 is ..-1 | 0..").expect("lowers").1, [0, 0xffff_fffe]);
        assert_eq!(lower(t, PNode::Uint(UintTy::Usize), "usize is 0..=HALF_USIZE").expect("lowers").1, [0, u128::from(u64::MAX >> 1)]);
        assert_eq!(lower(t, PNode::Char, r"char is '\u{1}'..='\u{10ffff}'").expect("lowers").1, [1, 0x10_ffff]);
        let param = t.intern(PNode::Param(0)).expect("interns");
        assert_eq!(lower(t, PNode::Ptr(Mutbl::Not, param), "*const T is !null").expect("lowers").1, [1, u128::from(u64::MAX)]);
    }

    #[test]
    fn forms_outside_cores_are_refused_by_name() {
        let mut tys = PortableTys::new_in(Alloc::global());
        let t = &mut tys;
        let what = |r: Result<(PTy, std::vec::Vec<u128>), Refused>| match r {
            Err(Refused::Form { what, .. }) => what,
            other => panic!("not a refusal: {other:?}"),
        };
        let u32_ = PNode::Uint(UintTy::U32);
        assert_eq!(what(lower(t, u32_, "u32 is Some(_)")), "a pattern in a pattern type that is not a range, a literal or `!null`");
        assert_eq!(what(lower(t, u32_, "u32 is 1..Some(_)")), "a range end in a pattern type that is not a literal or a path");
        assert_eq!(what(lower(t, u32_, "u32 is 5..2")), "an empty range in a pattern type");
        assert_eq!(what(lower(t, u32_, "u32 is !null")), "`!null` on a type that is not a raw pointer");
        assert_eq!(what(lower(t, u32_, "u32 is -1..")), "a negative end in a pattern type over an unsigned base");
        assert_eq!(what(lower(t, PNode::Bool, "bool is true")), "a range pattern type over a type that is not an integer or `char`");
    }
}
