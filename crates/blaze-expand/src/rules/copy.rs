//! Range copies for the transcriber: a fragment's tokens written as one run, and `$($x)*` over
//! a run of token trees as one run of fragments, instead of one stack entry per iteration and one
//! context lookup per token.
//!
//! The strategy is a type parameter at one seam ([`Copying`]): over the linear matcher's
//! [`Flat`] the transcriber copies runs; over the position-set matcher's [`Bindings`] (the exact
//! mode, and the oracle's other side) it writes token by token as before. The two write equal
//! tokens, spacings, contexts and origins; the oracle (`super::rules_oracle`) compares them on
//! every arm, and [`CopyPlant::DropLastToken`] is the planted bug it must report.
//!
//! What a run does per token, and why it equals the token-by-token writer
//! (`transcribe::push`):
//! - its context: looked up (the transcriber's memo, else made) only when the token's source
//!   context differs from the token before's in the run; the token-by-token writer finds the
//!   same context in its memo then, so the same contexts are made in the same order;
//! - its kind: [`written`] (an identifier or literal `Alone`); the token before, when an
//!   identifier or literal, is made `JointHidden` exactly when [`joint`] says so, which is
//!   `transcribe::join_prev`'s decision (the run's first token is joined by the caller);
//! - after each fragment, [`set_last_alone`], as the token-by-token writer does after each.

use core::sync::atomic::{AtomicU8, Ordering};

use super::{
    input::View,
    linear::{Flat, Values},
    matcher::{Bindings, Toks},
};
use crate::{
    error::ExpandError,
    expand::Out,
    expansion::TokenOrigin,
    tokens::{Ctx, Spacing, TokenKind},
};

/// How the transcriber writes a match's fragments: the seam between the two matchers' values
/// (`strategy-is-a-type-parameter`).
pub(crate) trait Copying: Values {
    /// Fragments are copied as runs ([`copy_frags`]); else token by token.
    const RANGES: bool;
}

impl Copying for Flat<'_, '_> {
    const RANGES: bool = true;
}

impl Copying for Bindings<'_> {
    const RANGES: bool = false;
}

/// A bug planted in the range copy, for the oracle's control: with one planted, the oracle
/// must count arms different.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum CopyPlant {
    Off = 0,
    /// Each range copy leaves out its last token.
    DropLastToken = 1,
}

static PLANT: AtomicU8 = AtomicU8::new(CopyPlant::Off as u8);

/// Plants `plant` in the range copy for the process (the oracle's control; a test binary of
/// its own sets it, as no other expansion of that process may see it).
#[doc(hidden)]
pub fn rules_copy_plant(plant: CopyPlant) {
    PLANT.store(plant as u8, Ordering::Relaxed);
}

/// What a token's context is looked up by: its stream's role and its context there, as the
/// transcriber's memo keys it.
pub(super) fn source_key(out: &Out<'_, '_>, origin: TokenOrigin) -> (u8, u32) {
    match origin {
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
    }
}

/// An identifier or literal is written `Alone`, and made joint with the next token where its
/// source had them joint ([`joint`]).
pub(super) fn written(kind: TokenKind) -> TokenKind {
    match kind {
        TokenKind::Ident { .. } | TokenKind::Literal { .. } => kind.with_spacing(Spacing::Alone),
        _ => kind,
    }
}

/// Whether a written identifier or literal from `a` is joint with the token from `b` written
/// after it: both of one stream, `b` right after `a` there, `a` not `Alone` in it, and both of
/// one edition. A fragment's last token, joint with the `,` after it in the call (`[u8, u16]`),
/// is not joint with the `as` the template writes after it (`$Src as`).
pub(super) fn joint(out: &Out<'_, '_>, a: TokenOrigin, b: TokenOrigin) -> bool {
    let (stream, i, j) = match (a, b) {
        (TokenOrigin::Input(i), TokenOrigin::Input(j)) => (out.call.input(), i, j),
        (TokenOrigin::AttrArgs(i), TokenOrigin::AttrArgs(j)) => match out.call.attr_args() {
            Some(args) => (args, i, j),
            None => return false,
        },
        (TokenOrigin::Def(i), TokenOrigin::Def(j)) => (out.def.body(), i, j),
        (TokenOrigin::Params(i), TokenOrigin::Params(j)) => match out.def.params() {
            Some(params) => (params, i, j),
            None => return false,
        },
        _ => return false,
    };
    if i.checked_add(1) != Some(j) {
        return false;
    }
    let (ti, tj) = (stream.tokens()[i as usize], stream.tokens()[j as usize]);
    ti.kind.spacing() != Spacing::Alone
        && (ti.ctx == tj.ctx || stream.edition_of(ti.ctx) == stream.edition_of(tj.ctx))
}

/// tt's `extend_with_tt_alone`: the last token written that is not a close, if punctuation,
/// made `Alone`.
pub(super) fn set_last_alone(out: &mut Out<'_, '_>) {
    let tokens = out.exp.tokens.tokens();
    let last = tokens.iter().rposition(|token| !matches!(token.kind, TokenKind::Close(_)));
    if let Some(at) = last
        && matches!(tokens[at].kind, TokenKind::Punct { .. })
    {
        out.exp.tokens.set_spacing(at, Spacing::Alone);
    }
}

/// Writes the fragments `frag(0)..frag(count)` of `view`, each a non-empty range, one after
/// another as one run; with `alone`, [`set_last_alone`] after each. The caller has joined the
/// first token to what was written before (`transcribe::join_prev`). `ctx_of` is the
/// transcriber's memo: the context for a token of `origin`, whose [`source_key`] is given.
pub(super) fn copy_frags(
    out: &mut Out<'_, '_>,
    view: &View<'_, '_>,
    count: u32,
    frag: impl Fn(u32) -> Toks,
    alone: bool,
    mut ctx_of: impl FnMut(&mut Out<'_, '_>, TokenOrigin, (u8, u32)) -> Result<Ctx, ExpandError>,
) -> Result<(), ExpandError> {
    let plant = PLANT.load(Ordering::Relaxed) == CopyPlant::DropLastToken as u8;
    // The context of the run so far: its source key, and the context made for it.
    let mut cur: Option<((u8, u32), Ctx)> = None;
    // The origin of the token written last in this run, when an identifier or literal.
    let mut prev: Option<TokenOrigin> = None;
    for f in 0..count {
        let toks = frag(f);
        let end = match plant && f + 1 == count {
            true => toks.end - 1,
            false => toks.end,
        };
        let n = end.saturating_sub(toks.at) as usize;
        out.exp.tokens.reserve(n, 0);
        out.exp.origins.reserve(n);
        for at in toks.at..end {
            let origin = view.origin(at);
            if let Some(before) = prev
                && joint(out, before, origin)
            {
                let last = out.exp.tokens.len() - 1;
                out.exp.tokens.set_spacing(last, Spacing::JointHidden);
            }
            let this = source_key(out, origin);
            let ctx = match cur {
                Some((key, ctx)) if key == this => ctx,
                _ => {
                    let ctx = ctx_of(out, origin, this)?;
                    cur = Some((this, ctx));
                    ctx
                }
            };
            let kind = written(view.kind(at));
            out.push_whole_ctx(kind, view.whole(at), origin, ctx)?;
            prev = match kind {
                TokenKind::Ident { .. } | TokenKind::Literal { .. } => Some(origin),
                _ => None,
            };
        }
        if alone {
            set_last_alone(out);
        }
    }
    Ok(())
}
