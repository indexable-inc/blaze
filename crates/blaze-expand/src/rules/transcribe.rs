//! The transcriber: an arm's template operations run on an explicit stack, each token written
//! straight into the expansion with its origin, its context made once per source context.

use blaze_mem::frame::Frame;
use blaze_own::Vec;
use parser::SyntaxKind;

use super::{
    Arm, Bound, CallError, ConcatElem, DEPTH_BOUND, DefTok, Engine, Frag, Msg, NONE, Op, RepKind,
    SITE, Sep,
    copy::{self, Copying, set_last_alone, written},
    input::View,
    linear::{NodeKind, Values},
    matcher::{Fragment, Origin, Toks},
};
use crate::{
    error::ExpandError,
    expand::Out,
    expansion::TokenOrigin,
    tokens::{Ctx, Delim, LitKind, Raw, Spacing, StreamMark, TokenKind, WholeText},
};

/// One repetition's place: rustc's `NestingState`.
#[derive(Clone, Copy, Debug)]
struct Nesting {
    idx: u32,
    hit: bool,
    at_end: bool,
}

/// Why a metavariable gave no value.
#[derive(Clone, Copy, Debug)]
enum Miss {
    /// Its repetition has no more iterations.
    Ended,
    Error(CallError),
}

/// A metavariable's value as the transcriber puts it.
#[derive(Clone, Copy, Debug)]
enum Value {
    Fragment(Fragment),
    /// What a failed match bound, as its placeholder.
    Missing(Frag),
}

/// What a sequence of operations on the stack is.
#[derive(Clone, Copy, Debug)]
enum Seq {
    Top,
    Group { delim: Delim, close: DefTok },
    Repeat { body: u32, sep: u32, kind: RepKind, counter: u32, err: Option<CallError>, initial: StreamMark, restore: StreamMark },
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    at: u32,
    end: u32,
    err: Option<CallError>,
    seq: Seq,
}

/// The transcriber's lists, kept across the arms of a call.
pub(crate) struct Scratch<'f> {
    stack: Vec<Entry, &'f Frame>,
    nesting: Vec<Nesting, &'f Frame>,
    /// The contexts made for source contexts: (stream role, the source context, this one's).
    memo: Vec<(u8, u32, Ctx), &'f Frame>,
    /// The memo's entry the last token took.
    last: usize,
    text: Vec<u8, &'f Frame>,
}

impl<'f> Scratch<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        Scratch {
            stack: Vec::new_in(frame),
            nesting: Vec::new_in(frame),
            memo: Vec::new_in(frame),
            last: 0,
            text: Vec::new_in(frame),
        }
    }

    /// The expansion back to `mark`, and the contexts made since forgotten.
    pub(crate) fn truncate(&mut self, out: &mut Out<'_, '_>, mark: StreamMark) {
        if mark.tokens >= out.exp.tokens.len() {
            return;
        }
        out.exp.tokens.truncate(mark);
        out.exp.origins.truncate(mark.tokens);
        let contexts = out.exp.tokens.contexts().len();
        self.memo.retain(|&(_, _, ctx)| ctx.node().is_none_or(|node| (node as usize) < contexts));
        self.last = 0;
        // The token now last was joined to one let go of: it is last, so `Alone`.
        if let Some(last) = mark.tokens.checked_sub(1)
            && matches!(out.exp.tokens.tokens()[last].kind, TokenKind::Ident { .. } | TokenKind::Literal { .. })
        {
            out.exp.tokens.set_spacing(last, Spacing::Alone);
        }
    }
}

/// Writes a token from `origin`, its context made once per source context.
fn push(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    kind: TokenKind,
    text: WholeText<'_>,
    origin: TokenOrigin,
) -> Result<(), ExpandError> {
    join_prev(out, origin);
    let key = copy::source_key(out, origin);
    let ctx = memo_ctx(scratch, out, origin, key)?;
    out.push_whole_ctx(kind, text, origin, ctx)
}

/// The context of this expansion for a token of `origin`, whose source key is `key`
/// ([`copy::source_key`]): made once per source context, then found in the memo.
fn memo_ctx(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    origin: TokenOrigin,
    key: (u8, u32),
) -> Result<Ctx, ExpandError> {
    // Most tokens are of the context the last one was; else the few made so far are searched.
    Ok(match scratch.memo.get(scratch.last) {
        Some(&(role, raw, ctx)) if (role, raw) == key => ctx,
        _ => match scratch.memo.iter().position(|it| (it.0, it.1) == key) {
            Some(at) => {
                scratch.last = at;
                scratch.memo[at].2
            }
            None => {
                let ctx = out.ctx_for(origin)?;
                scratch.last = scratch.memo.len();
                scratch.memo.push((key.0, key.1, ctx));
                ctx
            }
        },
    })
}

fn push_def(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    engine: &Engine<'_, '_, '_>,
    tok: DefTok,
) -> Result<(), ExpandError> {
    let def = engine.def;
    push(scratch, out, written(def.kind(tok)), def.whole(tok), tok.origin())
}

/// Copies `toks` of the view; with `alone`, a last punctuation is made `Alone`. With `range`
/// (the linear matcher's values: [`Copying::RANGES`]) as one run ([`copy::copy_frags`]), else
/// token by token: the oracle's other side. Both write equal tokens, contexts and origins.
fn push_view(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    view: &View<'_, '_>,
    toks: Toks,
    alone: bool,
    range: bool,
) -> Result<(), ExpandError> {
    if range && !toks.is_empty() {
        join_prev(out, view.origin(toks.at));
        return copy::copy_frags(out, view, 1, |_| toks, alone, |out, origin, key| {
            memo_ctx(scratch, out, origin, key)
        });
    }
    for at in toks.at..toks.end {
        push(scratch, out, written(view.kind(at)), view.whole(at), view.origin(at))?;
    }
    if alone && !toks.is_empty() {
        set_last_alone(out);
    }
    Ok(())
}

/// `toks` without the one invisible group that is all of it.
fn strip_invisible(view: &View<'_, '_>, toks: Toks) -> Toks {
    if !toks.is_empty()
        && view.kind(toks.at) == TokenKind::Open(Delim::Invisible)
        && view.close_of(toks.at) + 1 == toks.end
    {
        return Toks { at: toks.at + 1, end: toks.end - 1 };
    }
    toks
}

/// The top-level trees of `toks`, as their first tokens.
fn trees(view: &View<'_, '_>, toks: Toks) -> impl Iterator<Item = u32> {
    let mut at = toks.at;
    core::iter::from_fn(move || {
        (at < toks.end).then(|| {
            let tree = at;
            at = view.after_tree(at);
            tree
        })
    })
}

/// Whether `toks` is one tree that is a group.
fn is_group(view: &View<'_, '_>, toks: Toks) -> bool {
    !toks.is_empty()
        && matches!(view.kind(toks.at), TokenKind::Open(_))
        && view.close_of(toks.at) + 1 == toks.end
}

/// mbe's `is_macro_call`: a path of identifiers and `:` before one `!`, then a group last.
fn is_macro_call(view: &View<'_, '_>, toks: Toks) -> bool {
    let mut bang = false;
    let mut iter = trees(view, toks).peekable();
    while let Some(at) = iter.next() {
        let last = iter.peek().is_none();
        match view.kind(at) {
            TokenKind::Open(_) => return bang && last,
            TokenKind::Punct { char: '!', .. } if !bang => bang = true,
            TokenKind::Punct { char: ':', .. } if !bang => {}
            TokenKind::Ident { .. } if !bang => {}
            _ => return false,
        }
    }
    false
}

/// Which token follows a metavariable in its template, which says how an expression or a type
/// is put back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Follower {
    Angle,
    Postfix,
    Other,
}

fn follower(engine: &Engine<'_, '_, '_>, next: Option<Op>) -> Follower {
    let Some(Op::Puncts { at, len }) = next else { return Follower::Other };
    let def = engine.def;
    match (def.char(at), len) {
        // astlog-ignore: expand-hand-extent (the token after a transcribed fragment, for its spacing)
        ('<', _) => Follower::Angle,
        ('.' | '?', 1) => Follower::Postfix,
        _ => Follower::Other,
    }
}

/// Transcribes `arm`'s template with `bindings` into `out`: `Ok(None)` when it is written,
/// `Ok(Some(error))` for the template's first error (what was written is the caller's to
/// truncate).
pub(crate) fn transcribe<'f, B: Copying>(
    engine: &mut Engine<'_, '_, 'f>,
    arm: &Arm,
    view: &View<'_, 'f>,
    bindings: &B,
    out: &mut Out<'_, '_>,
    scratch: &mut Scratch<'f>,
) -> Result<Result<Option<CallError>, ExpandError>, Bound> {
    scratch.stack.clear();
    scratch.nesting.clear();
    scratch.stack.push(Entry { at: arm.ops, end: arm.ops_end, err: None, seq: Seq::Top });
    let program = engine.program;
    loop {
        engine.budget.step()?;
        let top = scratch.stack.len() - 1;
        let (at, end) = (scratch.stack[top].at, scratch.stack[top].end);
        if at < end {
            let op = program.ops[at as usize];
            scratch.stack[top].at += 1;
            let e = match op {
                Op::Tok(tok) => match push_def(scratch, out, engine, tok) {
                    Ok(()) => None,
                    Err(error) => return Ok(Err(error)),
                },
                Op::Crate(tok) => {
                    let kind = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
                    if let Err(error) = push(scratch, out, kind, WholeText::of("$crate"), tok.origin()) {
                        return Ok(Err(error));
                    }
                    None
                }
                Op::Puncts { at, len } => {
                    for i in 0..u32::from(len) {
                        if let Err(error) = push_def(scratch, out, engine, DefTok(at.0 + i)) {
                            return Ok(Err(error));
                        }
                    }
                    None
                }
                Op::Group { delim, open, close, end } => {
                    if let Err(error) = push(scratch, out, TokenKind::Open(delim), WholeText::of(""), open.origin()) {
                        return Ok(Err(error));
                    }
                    scratch.stack[top].at = end;
                    let seq = Seq::Group { delim, close };
                    push_entry(scratch, Entry { at: at + 1, end, err: None, seq })?;
                    continue;
                }
                Op::Repeat { end, sep, kind } => {
                    // `$($x)*` over the linear matcher's values, every iteration a token run:
                    // one copy ([`tt_run`], [`repeat_run`]); every other repetition runs on the
                    // stack below. Not past the stack's bound: there the stack refuses it.
                    if B::RANGES
                        && sep == NONE
                        && end == at + 2
                        && scratch.stack.len() < DEPTH_BOUND
                        && let Op::Var { binder, .. } = program.ops[(at + 1) as usize]
                        && let Some((node, len)) = tt_run(bindings, &scratch.nesting, binder)
                    {
                        scratch.stack[top].at = end;
                        if let Err(error) = repeat_run(engine, view, bindings, scratch, out, node, len)? {
                            return Ok(Err(error));
                        }
                        continue;
                    }
                    scratch.stack[top].at = end;
                    scratch.nesting.push(Nesting { idx: 0, hit: false, at_end: false });
                    let mark = out.exp.tokens.mark();
                    let seq = Seq::Repeat {
                        body: at + 1,
                        sep,
                        kind,
                        counter: 0,
                        err: None,
                        initial: mark,
                        restore: mark,
                    };
                    push_entry(scratch, Entry { at: at + 1, end, err: None, seq })?;
                    continue;
                }
                Op::Var { binder, name, .. } => {
                    let next = (at + 1 < end).then(|| program.ops[(at + 1) as usize]);
                    let follower = follower(engine, next);
                    match var(engine, view, bindings, scratch, out, binder, name, follower) {
                        Ok(e) => e,
                        Err(error) => return Ok(Err(error)),
                    }
                }
                Op::Ignore { binder, name } => {
                    match get_fragment(bindings, &mut scratch.nesting, binder, name) {
                        Err(Miss::Error(e)) => Some(e),
                        Ok(_) | Err(Miss::Ended) => None,
                    }
                }
                Op::Index { depth } => {
                    let nesting = &scratch.nesting;
                    let index = (nesting.len() as u32)
                        .checked_sub(1 + depth)
                        .map_or(0, |at| nesting[at as usize].idx);
                    if let Err(error) = integer(scratch, out, index) {
                        return Ok(Err(error));
                    }
                    None
                }
                Op::Len { .. } => {
                    if let Err(error) = integer(scratch, out, 0) {
                        return Ok(Err(error));
                    }
                    None
                }
                Op::Count { binder, name, depth } => {
                    let Some(mut binding) = bindings.root(binder) else {
                        let e = CallError::Binding(SITE, Msg::Unresolved(name));
                        scratch.stack[top].err = scratch.stack[top].err.or(Some(e));
                        continue;
                    };
                    let mut ended = false;
                    for state in scratch.nesting.iter_mut() {
                        state.hit = true;
                        match bindings.kind(binding) {
                            NodeKind::Fragment(_) | NodeKind::Missing(_) => break,
                            NodeKind::Nested(len) => match state.idx < len {
                                true => binding = bindings.child(binding, state.idx),
                                false => {
                                    state.at_end = true;
                                    ended = true;
                                    break;
                                }
                            },
                            NodeKind::Empty => {
                                state.at_end = true;
                                break;
                            }
                        }
                    }
                    let frame = *scratch.stack.allocator();
                    if !ended
                        && let Err(error) = integer(scratch, out, count(bindings, binding, 0, depth, frame))
                    {
                        return Ok(Err(error));
                    }
                    None
                }
                Op::Concat { start, len, func } => {
                    match concat(engine, view, bindings, scratch, out, start, len, func) {
                        Ok(e) => e,
                        Err(error) => return Ok(Err(error)),
                    }
                }
            };
            scratch.stack[top].err = scratch.stack[top].err.or(e);
            continue;
        }
        // The sequence on top is done.
        let done = scratch.stack.pop().expect("a sequence is on the stack");
        match done.seq {
            Seq::Top => return Ok(Ok(done.err)),
            Seq::Group { delim, close } => {
                if let Err(error) = push(scratch, out, TokenKind::Close(delim), WholeText::of(""), close.origin()) {
                    return Ok(Err(error));
                }
                let parent = scratch.stack.last_mut().expect("a group is inside a sequence");
                parent.err = parent.err.or(done.err);
            }
            Seq::Repeat { body, sep, kind, mut counter, mut err, initial, mut restore } => {
                let state = scratch.nesting.last_mut().expect("a repetition has its nesting");
                let mut finished = state.at_end || !state.hit;
                if !finished {
                    state.idx += 1;
                    state.hit = false;
                    restore = out.exp.tokens.mark();
                    counter += 1;
                    if counter == 65536 {
                        scratch.truncate(out, initial);
                        err = Some(CallError::LimitExceeded);
                        finished = true;
                    } else if done.err.is_some() {
                        err = err.or(done.err);
                    } else if sep != NONE
                        && let Err(error) = separator(engine, scratch, out, program.seps[sep as usize])
                    {
                        return Ok(Err(error));
                    }
                }
                if !finished {
                    let seq = Seq::Repeat { body, sep, kind, counter, err, initial, restore };
                    scratch.stack.push(Entry { at: body, end: done.end, err: None, seq });
                    continue;
                }
                scratch.truncate(out, restore);
                scratch.nesting.pop();
                if kind == RepKind::OneOrMore && counter == 0 && err.is_none() {
                    err = Some(CallError::UnexpectedToken(SITE));
                }
                let parent = scratch.stack.last_mut().expect("a repetition is inside a sequence");
                parent.err = parent.err.or(err);
            }
        }
    }
}

fn push_entry(scratch: &mut Scratch<'_>, entry: Entry) -> Result<(), Bound> {
    scratch.stack.push(entry);
    match scratch.stack.len() > DEPTH_BOUND {
        true => Err(Bound(crate::error::RulesLimit::Depth)),
        false => Ok(()),
    }
}

/// A `$($x)*` (one metavariable, no separator) whose iterations the copy writes as one run:
/// `$x`'s node at the repetition and its `len` iterations, when `$x` is repeated at every
/// repetition around it and at this one, `1 <= len < 65536`, and each iteration's value is a
/// non-empty token run written as it is (`Fragment::Tokens` of `Raw` or `Ast` origin: what `tt`,
/// `ident`, `lifetime`, `literal`, `pat`, `meta` bind). Reads only: `None` leaves every other case
/// (no iteration, the iteration limit, a value put in a group or parenthesized, a binder not
/// repeated here, a name no binder declares) to the stack machine, which decides it as before.
fn tt_run<B: Values>(bindings: &B, nesting: &[Nesting], binder: u32) -> Option<(B::Node, u32)> {
    let mut node = bindings.root(binder)?;
    for state in nesting {
        match bindings.kind(node) {
            NodeKind::Nested(len) if state.idx < len => node = bindings.child(node, state.idx),
            _ => return None,
        }
    }
    let NodeKind::Nested(len) = bindings.kind(node) else { return None };
    if len == 0 || len >= 65536 {
        return None;
    }
    for idx in 0..len {
        match bindings.kind(bindings.child(node, idx)) {
            NodeKind::Fragment(Fragment::Tokens { toks, origin: Origin::Raw | Origin::Ast })
                if !toks.is_empty() => {}
            _ => return None,
        }
    }
    Some((node, len))
}

/// [`tt_run`]'s repetition written as the stack machine writes it: its steps charged (per
/// iteration one for `$x` and one for the iteration's end, and the same for the iteration that
/// finds the end, which writes nothing), each repetition around it marked reached (as `$x`'s
/// lookup marks it), then every iteration's fragment copied in one run, each made `Alone` at its
/// end as [`var`] writes it. No iteration is written and then truncated.
fn repeat_run<B: Values>(
    engine: &mut Engine<'_, '_, '_>,
    view: &View<'_, '_>,
    bindings: &B,
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    node: B::Node,
    len: u32,
) -> Result<Result<(), ExpandError>, Bound> {
    for _ in 0..2 * (len + 1) {
        engine.budget.step()?;
    }
    for state in scratch.nesting.iter_mut() {
        state.hit = true;
    }
    let frag = |idx: u32| match bindings.kind(bindings.child(node, idx)) {
        NodeKind::Fragment(Fragment::Tokens { toks, .. }) => toks,
        other => unreachable!("`tt_run` checked every iteration's value, found {other:?}"),
    };
    join_prev(out, view.origin(frag(0).at));
    Ok(copy::copy_frags(out, view, len, frag, true, |out, origin, key| {
        memo_ctx(scratch, out, origin, key)
    }))
}

fn separator(
    engine: &Engine<'_, '_, '_>,
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    sep: Sep,
) -> Result<(), ExpandError> {
    match sep {
        Sep::Ident(tok) | Sep::Literal(tok) => push_def(scratch, out, engine, tok),
        Sep::Puncts { at, len } => {
            for i in 0..u32::from(len) {
                push_def(scratch, out, engine, DefTok(at.0 + i))?;
            }
            Ok(())
        }
        Sep::Lifetime { quote, name } => {
            push_def(scratch, out, engine, quote)?;
            push_def(scratch, out, engine, name)
        }
    }
}

/// An integer literal made at the call site.
fn integer(scratch: &mut Scratch<'_>, out: &mut Out<'_, '_>, value: u32) -> Result<(), ExpandError> {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut rest = value;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let kind = TokenKind::Literal { kind: LitKind::Integer, suffix_len: 0, spacing: Spacing::Alone };
    let digits = core::str::from_utf8(&digits[at..]).expect("ASCII digits are a whole `str`");
    push(scratch, out, kind, WholeText::of(digits), SITE)
}

/// `${count(..)}` of a binding at `depth`.
fn count<B: Values>(bindings: &B, binding: B::Node, depth: u32, max: u32, frame: &Frame) -> u32 {
    let mut stack: Vec<(B::Node, u32, u32, u32), &Frame> = Vec::new_in(frame);
    let mut total = 0u32;
    match bindings.kind(binding) {
        NodeKind::Nested(len) => match depth == max {
            true => return len,
            false => stack.push((binding, depth, 0, len)),
        },
        NodeKind::Empty => return 0,
        NodeKind::Fragment(_) | NodeKind::Missing(_) => return 1,
    }
    while let Some((node, d, idx, len)) = stack.last_mut() {
        if *idx >= *len {
            stack.pop();
            continue;
        }
        let cur = *idx;
        *idx = cur + 1;
        let child = bindings.child(*node, cur);
        let next_d = *d + 1;
        match bindings.kind(child) {
            NodeKind::Nested(n) => match next_d == max {
                true => total += n,
                false => stack.push((child, next_d, 0, n)),
            },
            NodeKind::Empty => {}
            NodeKind::Fragment(_) | NodeKind::Missing(_) => total += 1,
        }
    }
    total
}

/// A metavariable's value at the repetitions' current iterations.
fn get_fragment<B: Values>(
    bindings: &B,
    nesting: &mut [Nesting],
    binder: u32,
    name: DefTok,
) -> Result<Value, Miss> {
    let at = name.origin();
    let Some(mut b) = bindings.root(binder) else {
        return Err(Miss::Error(CallError::Binding(at, Msg::Unresolved(name))));
    };
    let innermost = nesting.len().wrapping_sub(1);
    for (depth, state) in nesting.iter_mut().enumerate() {
        state.hit = true;
        b = match bindings.kind(b) {
            NodeKind::Fragment(_) => break,
            NodeKind::Missing(_) => {
                state.at_end = true;
                break;
            }
            NodeKind::Nested(len) => match state.idx < len {
                true => bindings.child(b, state.idx),
                false => {
                    state.at_end = true;
                    return Err(match depth == innermost {
                        true => Miss::Ended,
                        false => Miss::Error(CallError::Binding(at, Msg::NestedNotFound(name))),
                    });
                }
            },
            NodeKind::Empty => {
                state.at_end = true;
                return Err(match depth == innermost {
                    true => Miss::Ended,
                    false => Miss::Error(CallError::Binding(at, Msg::EmptyNotFound(name))),
                });
            }
        };
    }
    match bindings.kind(b) {
        NodeKind::Fragment(fragment) => Ok(Value::Fragment(fragment)),
        NodeKind::Missing(kind) => Ok(Value::Missing(kind)),
        NodeKind::Nested(_) => Err(Miss::Error(CallError::Binding(at, Msg::FoundNested(name)))),
        NodeKind::Empty => Err(Miss::Error(CallError::Binding(at, Msg::FoundEmpty(name)))),
    }
}

/// `$name`.
#[expect(clippy::too_many_arguments, reason = "the transcriber's state, passed as its parts")]
fn var<B: Copying>(
    engine: &Engine<'_, '_, '_>,
    view: &View<'_, '_>,
    bindings: &B,
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    binder: u32,
    name: DefTok,
    follower: Follower,
) -> Result<Option<CallError>, ExpandError> {
    let id = name.origin();
    let value = match get_fragment(bindings, &mut scratch.nesting, binder, name) {
        Ok(value) => value,
        Err(Miss::Ended) => return Ok(None),
        Err(Miss::Error(CallError::Binding(_, Msg::Unresolved(_)))) => {
            // A name no binder declares is written as it is, `$` and the name.
            let dollar = TokenKind::Punct { char: '$', spacing: Spacing::Alone };
            push(scratch, out, dollar, WholeText::of(""), id)?;
            let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
            push(scratch, out, ident, engine.def.whole(name), id)?;
            return Ok(None);
        }
        Err(Miss::Error(error)) => return Ok(Some(error)),
    };
    let invisible = |scratch: &mut Scratch<'_>, out: &mut Out<'_, '_>, open: bool| {
        let kind = if open { TokenKind::Open(Delim::Invisible) } else { TokenKind::Close(Delim::Invisible) };
        push(scratch, out, kind, WholeText::of(""), id)
    };
    let paren = |scratch: &mut Scratch<'_>, out: &mut Out<'_, '_>, open: bool| {
        let kind = if open { TokenKind::Open(Delim::Paren) } else { TokenKind::Close(Delim::Paren) };
        push(scratch, out, kind, WholeText::of(""), id)
    };
    match value {
        Value::Missing(kind) => {
            let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
            match kind {
                Frag::Stmt => push(scratch, out, TokenKind::Punct { char: ';', spacing: Spacing::Alone }, WholeText::of(""), id)?,
                Frag::Block => {
                    push(scratch, out, TokenKind::Open(Delim::Brace), WholeText::of(""), id)?;
                    push(scratch, out, TokenKind::Close(Delim::Brace), WholeText::of(""), id)?;
                }
                Frag::Meta | Frag::Item | Frag::Tt | Frag::Vis => {}
                Frag::Lifetime => {
                    push(scratch, out, TokenKind::Punct { char: '\'', spacing: Spacing::Joint }, WholeText::of(""), id)?;
                    push(scratch, out, ident, WholeText::of("missing"), id)?;
                    set_last_alone(out);
                }
                Frag::Path
                | Frag::Ty
                | Frag::Pat
                | Frag::PatParam
                | Frag::Expr
                | Frag::Expr2021
                | Frag::Ident
                | Frag::Literal => push(scratch, out, ident, WholeText::of("missing"), id)?,
            }
        }
        Value::Fragment(Fragment::Tokens { toks, origin: Origin::Grouped }) => {
            invisible(scratch, out, true)?;
            push_view(scratch, out, view, strip_invisible(view, toks), true, B::RANGES)?;
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Tokens { toks, origin: Origin::Raw | Origin::Ast }) => {
            push_view(scratch, out, view, toks, true, B::RANGES)?;
        }
        // `&$t` of `A + B` is `&(A + B)`, `&$p` of `A | B` is `&(A | B)`: one operand, as an
        // expression is below.
        Value::Fragment(Fragment::Tokens { toks, origin: Origin::Parenthesized }) => {
            invisible(scratch, out, true)?;
            paren(scratch, out, true)?;
            push_view(scratch, out, view, strip_invisible(view, toks), true, B::RANGES)?;
            paren(scratch, out, false)?;
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Expr(toks)) => {
            let sub = strip_invisible(view, toks);
            let len = sub.end - sub.at;
            let negative_literal = len == 2
                && view.char(sub.at) == Some('-')
                && matches!(view.kind(sub.at + 1), TokenKind::Literal { .. });
            let is_leaf = len == 1 && !matches!(view.kind(sub.at), TokenKind::Open(_));
            let whole = negative_literal && follower != Follower::Postfix;
            let parenthesized =
                !(is_leaf || is_group(view, sub) || is_macro_call(view, sub) || whole);
            if !whole {
                invisible(scratch, out, true)?;
            }
            if parenthesized {
                paren(scratch, out, true)?;
            }
            push_view(scratch, out, view, sub, true, B::RANGES)?;
            if parenthesized {
                paren(scratch, out, false)?;
            }
            if !whole {
                invisible(scratch, out, false)?;
            }
        }
        Value::Fragment(Fragment::Path(toks)) => {
            invisible(scratch, out, true)?;
            // `Vec<T>` put where a path is needs its turbofish (`Vec::<T>`).
            let mut prev_ident = false;
            for tree in trees(view, toks) {
                let end = view.after_tree(tree);
                // astlog-ignore: expand-hand-extent (a `::` the transcription writes before a `<`)
                if prev_ident && end == tree + 1 && view.char(tree) == Some('<') {
                    push(scratch, out, TokenKind::Punct { char: ':', spacing: Spacing::Joint }, WholeText::of(""), SITE)?;
                    push(scratch, out, TokenKind::Punct { char: ':', spacing: Spacing::Alone }, WholeText::of(""), SITE)?;
                }
                prev_ident = end == tree + 1 && matches!(view.kind(tree), TokenKind::Ident { .. });
                push_view(scratch, out, view, Toks { at: tree, end }, false, B::RANGES)?;
            }
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Ty(toks)) if follower == Follower::Angle => {
            paren(scratch, out, true)?;
            push_view(scratch, out, view, toks, true, B::RANGES)?;
            paren(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Ty(toks)) => {
            invisible(scratch, out, true)?;
            push_view(scratch, out, view, toks, true, B::RANGES)?;
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Empty) => {}
    }
    Ok(None)
}

/// `${concat(..)}`: one identifier made at the expression.
#[expect(clippy::too_many_arguments, reason = "the transcriber's state, passed as its parts")]
fn concat<'f, B: Values>(
    engine: &Engine<'_, '_, 'f>,
    view: &View<'_, '_>,
    bindings: &B,
    scratch: &mut Scratch<'f>,
    out: &mut Out<'_, '_>,
    start: u32,
    len: u32,
    func: DefTok,
) -> Result<Option<CallError>, ExpandError> {
    let def = engine.def;
    let mut err = None;
    let mut text = core::mem::replace(&mut scratch.text, Vec::new_in(engine.frame));
    text.clear();
    let push_text = |text: &mut Vec<u8, &Frame>, s: &str| text.extend_from_slice(s.as_bytes());
    for &elem in &engine.program.concat[start as usize..(start + len) as usize] {
        match elem {
            ConcatElem::Ident(tok) => push_text(&mut text, def.text(tok)),
            ConcatElem::Literal(tok) => push_text(&mut text, unsuffixed(def.kind(tok), def.text(tok))),
            ConcatElem::Var { binder, name } => {
                let value = match get_fragment(bindings, &mut scratch.nesting, binder, name) {
                    Ok(value) => value,
                    Err(Miss::Ended) => continue,
                    Err(Miss::Error(e)) => {
                        err = err.or(Some(e));
                        continue;
                    }
                };
                // The value's first tree, if it is its only one.
                let single = match value {
                    Value::Fragment(Fragment::Tokens { toks, .. }) => {
                        let mut it = trees(view, toks);
                        match (it.next(), it.next()) {
                            (Some(at), None) => Some((view.kind(at), view.text(at))),
                            _ => None,
                        }
                    }
                    Value::Missing(kind) => match kind {
                        Frag::Path
                        | Frag::Ty
                        | Frag::Pat
                        | Frag::PatParam
                        | Frag::Expr
                        | Frag::Expr2021
                        | Frag::Ident
                        | Frag::Literal => Some((TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, "missing")),
                        _ => None,
                    },
                    Value::Fragment(_) => None,
                };
                match single {
                    Some((TokenKind::Ident { .. }, s)) => push_text(&mut text, s),
                    Some((kind @ TokenKind::Literal { .. }, s)) => push_text(&mut text, unsuffixed(kind, s)),
                    _ => {
                        let msg = Msg::Static(
                            "metavariables of `${concat(..)}` must be of type `ident`, `literal` or `tt`",
                        );
                        err = err.or(Some(CallError::Binding(name.origin(), msg)));
                    }
                }
            }
        }
    }
    let mut s = core::str::from_utf8(&text).expect("pushed whole `str`s");
    if !rustc_lexer::is_ident(s) {
        let msg = Msg::Static("`${concat(..)}` is not generating a valid identifier");
        err = err.or(Some(CallError::Binding(func.origin(), msg)));
        s = "__ra_concat_dummy";
    }
    let raw = match SyntaxKind::from_keyword(s, parser::Edition::LATEST).is_some() {
        true => Raw::Yes,
        false => Raw::No,
    };
    let kind = TokenKind::Ident { raw, spacing: Spacing::Alone };
    let result = push(scratch, out, kind, WholeText::of(s), func.origin());
    scratch.text = text;
    result?;
    Ok(err)
}

/// A literal's text without its suffix.
fn unsuffixed(kind: TokenKind, text: &str) -> &str {
    match kind {
        TokenKind::Literal { suffix_len, .. } => &text[..text.len() - suffix_len as usize],
        _ => text,
    }
}

/// Before a token from `next` is written: the last token written, an identifier or literal, is
/// made joint with it (`JointHidden`) where [`copy::joint`] says the source had them joint, else
/// `Alone`. Decided at each write, so a token written again after a truncation
/// ([`Scratch::truncate`]) decides it again.
fn join_prev(out: &mut Out<'_, '_>, next: TokenOrigin) {
    let Some(prev) = out.exp.tokens.len().checked_sub(1) else { return };
    let kind = out.exp.tokens.tokens()[prev].kind;
    if !matches!(kind, TokenKind::Ident { .. } | TokenKind::Literal { .. }) {
        return;
    }
    let a = out.exp.origins[prev];
    let spacing = match copy::joint(out, a, next) {
        true => Spacing::JointHidden,
        false => Spacing::Alone,
    };
    if kind.spacing() != spacing {
        out.exp.tokens.set_spacing(prev, spacing);
    }
}
