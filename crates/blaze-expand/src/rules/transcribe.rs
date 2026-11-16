//! The transcriber: an arm's template operations run on an explicit stack, each token written
//! straight into the expansion with its origin, its context made once per source context.

use blaze_mem::frame::Frame;
use blaze_own::Vec;
use parser::SyntaxKind;

use super::{
    Arm, Bound, CallError, ConcatElem, DEPTH_BOUND, DefTok, Engine, Frag, Msg, NONE, Op, RepKind,
    SITE, Sep,
    input::View,
    matcher::{Binding, Bindings, Fragment, Origin, Toks},
};
use crate::{
    error::ExpandError,
    expand::Out,
    expansion::TokenOrigin,
    tokens::{Ctx, Delim, LitKind, Raw, Spacing, StreamMark, TokenKind},
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
    text: &[u8],
    origin: TokenOrigin,
) -> Result<(), ExpandError> {
    join_prev(out, origin);
    let key = match origin {
        TokenOrigin::Input(at) => (0, out.call.input().tokens()[at as usize].ctx.raw()),
        TokenOrigin::AttrArgs(at) => {
            let args = out.call.attr_args().expect("an origin of the arguments is of an attribute call");
            (1, args.tokens()[at as usize].ctx.raw())
        }
        TokenOrigin::Def(at) => (2, out.def.body().tokens()[at as usize].ctx.raw()),
        TokenOrigin::Params(at) => {
            let params = out.def.params().expect("an origin of the parameters is of `macro m(..)`");
            (3, params.tokens()[at as usize].ctx.raw())
        }
        TokenOrigin::Made(transparency) => (4 + transparency as u8, 0),
    };
    // Most tokens are of the context the last one was; else the few made so far are searched.
    let ctx = match scratch.memo.get(scratch.last) {
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
    };
    out.push_bytes_ctx(kind, text, origin, ctx)
}

/// An identifier or literal is written `Alone`, and made joint with the next token by
/// [`join_prev`] where its source had them joint.
fn written(kind: TokenKind) -> TokenKind {
    match kind {
        TokenKind::Ident { .. } | TokenKind::Literal { .. } => kind.with_spacing(Spacing::Alone),
        _ => kind,
    }
}

fn push_def(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    engine: &Engine<'_, '_, '_>,
    tok: DefTok,
) -> Result<(), ExpandError> {
    let def = engine.def;
    push(scratch, out, written(def.kind(tok)), def.bytes(tok), tok.origin())
}

/// Copies `toks` of the view; with `alone`, a last punctuation is made `Alone`.
fn push_view(
    scratch: &mut Scratch<'_>,
    out: &mut Out<'_, '_>,
    view: &View<'_, '_>,
    toks: Toks,
    alone: bool,
) -> Result<(), ExpandError> {
    for at in toks.at..toks.end {
        push(scratch, out, written(view.kind(at)), view.bytes(at), view.origin(at))?;
    }
    if alone && !toks.is_empty() {
        set_last_alone(out);
    }
    Ok(())
}

/// tt's `extend_with_tt_alone`: the last token not a close, if punctuation, made `Alone`.
fn set_last_alone(out: &mut Out<'_, '_>) {
    let tokens = out.exp.tokens.tokens();
    let last = tokens.iter().rposition(|token| !matches!(token.kind, TokenKind::Close(_)));
    if let Some(at) = last
        && matches!(tokens[at].kind, TokenKind::Punct { .. })
    {
        out.exp.tokens.set_spacing(at, Spacing::Alone);
    }
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
        ('<', _) => Follower::Angle,
        ('.' | '?', 1) => Follower::Postfix,
        _ => Follower::Other,
    }
}

/// Transcribes `arm`'s template with `bindings` into `out`: `Ok(None)` when it is written,
/// `Ok(Some(error))` for the template's first error (what was written is the caller's to
/// truncate).
pub(crate) fn transcribe<'f>(
    engine: &mut Engine<'_, '_, 'f>,
    arm: &Arm,
    view: &View<'_, 'f>,
    bindings: &Bindings<'f>,
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
                    if let Err(error) = push(scratch, out, kind, b"$crate", tok.origin()) {
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
                    if let Err(error) = push(scratch, out, TokenKind::Open(delim), b"", open.origin()) {
                        return Ok(Err(error));
                    }
                    scratch.stack[top].at = end;
                    let seq = Seq::Group { delim, close };
                    push_entry(scratch, Entry { at: at + 1, end, err: None, seq })?;
                    continue;
                }
                Op::Repeat { end, sep, kind } => {
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
                        match binding {
                            Binding::Fragment(_) | Binding::Missing(_) => break,
                            Binding::Nested { start, len } => match state.idx < len {
                                true => binding = bindings.child(start, state.idx),
                                false => {
                                    state.at_end = true;
                                    ended = true;
                                    break;
                                }
                            },
                            Binding::Empty => {
                                state.at_end = true;
                                break;
                            }
                        }
                    }
                    if !ended
                        && let Err(error) = integer(scratch, out, count(bindings, binding, 0, depth))
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
                if let Err(error) = push(scratch, out, TokenKind::Close(delim), b"",close.origin()) {
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
    push(scratch, out, kind, &digits[at..], SITE)
}

/// `${count(..)}` of a binding at `depth`.
fn count(bindings: &Bindings<'_>, binding: Binding, depth: u32, max: u32) -> u32 {
    match binding {
        Binding::Nested { start, len } => match depth == max {
            true => len,
            false => (0..len).map(|idx| count(bindings, bindings.child(start, idx), depth + 1, max)).sum(),
        },
        Binding::Empty => 0,
        Binding::Fragment(_) | Binding::Missing(_) => 1,
    }
}

/// A metavariable's value at the repetitions' current iterations.
fn get_fragment(
    bindings: &Bindings<'_>,
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
        b = match b {
            Binding::Fragment(_) => break,
            Binding::Missing(_) => {
                state.at_end = true;
                break;
            }
            Binding::Nested { start, len } => match state.idx < len {
                true => bindings.child(start, state.idx),
                false => {
                    state.at_end = true;
                    return Err(match depth == innermost {
                        true => Miss::Ended,
                        false => Miss::Error(CallError::Binding(at, Msg::NestedNotFound(name))),
                    });
                }
            },
            Binding::Empty => {
                state.at_end = true;
                return Err(match depth == innermost {
                    true => Miss::Ended,
                    false => Miss::Error(CallError::Binding(at, Msg::EmptyNotFound(name))),
                });
            }
        };
    }
    match b {
        Binding::Fragment(f) => Ok(Value::Fragment(bindings.fragments[f as usize])),
        Binding::Missing(kind) => Ok(Value::Missing(kind)),
        Binding::Nested { .. } => Err(Miss::Error(CallError::Binding(at, Msg::FoundNested(name)))),
        Binding::Empty => Err(Miss::Error(CallError::Binding(at, Msg::FoundEmpty(name)))),
    }
}

/// `$name`.
#[expect(clippy::too_many_arguments, reason = "the transcriber's state, passed as its parts")]
fn var(
    engine: &Engine<'_, '_, '_>,
    view: &View<'_, '_>,
    bindings: &Bindings<'_>,
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
            push(scratch, out, dollar, b"",id)?;
            let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
            push(scratch, out, ident, engine.def.bytes(name), id)?;
            return Ok(None);
        }
        Err(Miss::Error(error)) => return Ok(Some(error)),
    };
    let invisible = |scratch: &mut Scratch<'_>, out: &mut Out<'_, '_>, open: bool| {
        let kind = if open { TokenKind::Open(Delim::Invisible) } else { TokenKind::Close(Delim::Invisible) };
        push(scratch, out, kind, b"",id)
    };
    let paren = |scratch: &mut Scratch<'_>, out: &mut Out<'_, '_>, open: bool| {
        let kind = if open { TokenKind::Open(Delim::Paren) } else { TokenKind::Close(Delim::Paren) };
        push(scratch, out, kind, b"",id)
    };
    match value {
        Value::Missing(kind) => {
            let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
            match kind {
                Frag::Stmt => push(scratch, out, TokenKind::Punct { char: ';', spacing: Spacing::Alone }, b"",id)?,
                Frag::Block => {
                    push(scratch, out, TokenKind::Open(Delim::Brace), b"",id)?;
                    push(scratch, out, TokenKind::Close(Delim::Brace), b"",id)?;
                }
                Frag::Meta | Frag::Item | Frag::Tt | Frag::Vis => {}
                Frag::Lifetime => {
                    push(scratch, out, TokenKind::Punct { char: '\'', spacing: Spacing::Joint }, b"",id)?;
                    push(scratch, out, ident, b"missing", id)?;
                    set_last_alone(out);
                }
                Frag::Path
                | Frag::Ty
                | Frag::Pat
                | Frag::PatParam
                | Frag::Expr
                | Frag::Expr2021
                | Frag::Ident
                | Frag::Literal => push(scratch, out, ident, b"missing", id)?,
            }
        }
        Value::Fragment(Fragment::Tokens { toks, origin: Origin::Grouped }) => {
            invisible(scratch, out, true)?;
            push_view(scratch, out, view, strip_invisible(view, toks), true)?;
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Tokens { toks, origin: Origin::Raw | Origin::Ast }) => {
            push_view(scratch, out, view, toks, true)?;
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
            push_view(scratch, out, view, sub, true)?;
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
                if prev_ident && end == tree + 1 && view.char(tree) == Some('<') {
                    push(scratch, out, TokenKind::Punct { char: ':', spacing: Spacing::Joint }, b"",SITE)?;
                    push(scratch, out, TokenKind::Punct { char: ':', spacing: Spacing::Alone }, b"",SITE)?;
                }
                prev_ident = end == tree + 1 && matches!(view.kind(tree), TokenKind::Ident { .. });
                push_view(scratch, out, view, Toks { at: tree, end }, false)?;
            }
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Ty(toks)) if follower == Follower::Angle => {
            paren(scratch, out, true)?;
            push_view(scratch, out, view, toks, true)?;
            paren(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Ty(toks)) => {
            invisible(scratch, out, true)?;
            push_view(scratch, out, view, toks, true)?;
            invisible(scratch, out, false)?;
        }
        Value::Fragment(Fragment::Empty) => {}
    }
    Ok(None)
}

/// `${concat(..)}`: one identifier made at the expression.
#[expect(clippy::too_many_arguments, reason = "the transcriber's state, passed as its parts")]
fn concat<'f>(
    engine: &Engine<'_, '_, 'f>,
    view: &View<'_, '_>,
    bindings: &Bindings<'_>,
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
    let mut push_text = |text: &mut Vec<u8, &Frame>, s: &str| text.extend_from_slice(s.as_bytes());
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
    let result = push(scratch, out, kind, s.as_bytes(), func.origin());
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
/// made joint with it (`JointHidden`) where it followed that token in the stream both were
/// read from and the source had them joint, else `Alone`. A fragment's last token, joint with
/// the `,` after it in the call (`[u8, u16]`), is not joint with the `as` the template writes
/// after it (`$Src as`). Decided at each write, so a token written again after a truncation
/// ([`Scratch::truncate`]) decides it again.
fn join_prev(out: &mut Out<'_, '_>, next: TokenOrigin) {
    let Some(prev) = out.exp.tokens.len().checked_sub(1) else { return };
    let kind = out.exp.tokens.tokens()[prev].kind;
    if !matches!(kind, TokenKind::Ident { .. } | TokenKind::Literal { .. }) {
        return;
    }
    let a = out.exp.origins[prev];
    let stream = match (a, next) {
        (TokenOrigin::Input(_), TokenOrigin::Input(_)) => Some(out.call.input()),
        (TokenOrigin::AttrArgs(_), TokenOrigin::AttrArgs(_)) => out.call.attr_args(),
        (TokenOrigin::Def(_), TokenOrigin::Def(_)) => Some(out.def.body()),
        (TokenOrigin::Params(_), TokenOrigin::Params(_)) => out.def.params(),
        _ => None,
    };
    let joint = match (stream, source_index(a), source_index(next)) {
        (Some(stream), Some(i), Some(j)) if i.checked_add(1) == Some(j) => {
            let (ti, tj) = (stream.tokens()[i as usize], stream.tokens()[j as usize]);
            ti.kind.spacing() != Spacing::Alone
                && stream.edition_of(ti.ctx) == stream.edition_of(tj.ctx)
        }
        _ => false,
    };
    let spacing = if joint { Spacing::JointHidden } else { Spacing::Alone };
    if kind.spacing() != spacing {
        out.exp.tokens.set_spacing(prev, spacing);
    }
}

fn source_index(origin: TokenOrigin) -> Option<u32> {
    match origin {
        TokenOrigin::Input(at) | TokenOrigin::AttrArgs(at) | TokenOrigin::Def(at) | TokenOrigin::Params(at) => Some(at),
        TokenOrigin::Made(_) => None,
    }
}
