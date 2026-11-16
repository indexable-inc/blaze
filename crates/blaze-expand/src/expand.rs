//! [`expand`]: one macro call of one definition to its [`Expanded`].

use core::alloc::Allocator;

use mbe::DeclarativeMacro;

use crate::{
    builtin,
    call::Call,
    cx::{ExpandCx, ProcCall},
    declarative,
    def::{LocalInner, MacroDef, MacroKind, ProcKind},
    derive,
    error::{ExpandError, Stream, Unbuilt},
    expansion::{Diagnostic, Expanded, Expansion, Origin, PathScope},
    nested,
    tokens::{Ctx, CtxNode, Delim, Spacing, Text, TokenKind, Transparency},
};

/// A definition made ready to expand calls: a declarative macro's rules parsed once.
pub struct Prepared<'d, A: Allocator> {
    def: &'d MacroDef<A>,
    rules: Option<DeclarativeMacro>,
}

impl<'d, A: Allocator> Prepared<'d, A> {
    pub fn def(&self) -> &'d MacroDef<A> {
        self.def
    }
}

/// Parses a declarative definition's rules; other kinds have nothing to prepare.
pub fn prepare<A: Allocator>(def: &MacroDef<A>) -> Result<Prepared<'_, A>, ExpandError> {
    let rules = match def.kind() {
        MacroKind::Rules | MacroKind::Macro2 => Some(declarative::parse(def)?),
        MacroKind::BuiltinFn(_)
        | MacroKind::BuiltinAttr(_)
        | MacroKind::BuiltinDerive(_)
        | MacroKind::ProcMacro { .. } => None,
    };
    Ok(Prepared { def, rules })
}

/// Expands `call` of `def`: the expansion, or the nested calls an eager macro needs first.
/// Everything outside the tokens is asked of `cx` and recorded in the expansion.
pub fn expand<A: Allocator + Clone>(
    def: &MacroDef<A>,
    call: &Call<'_, A>,
    cx: &impl ExpandCx,
    alloc: A,
) -> Result<Expanded<A>, ExpandError> {
    expand_prepared(&prepare(def)?, call, cx, alloc)
}

/// [`expand`] of a definition [`prepare`]d once for many calls.
pub fn expand_prepared<A: Allocator + Clone>(
    prepared: &Prepared<'_, A>,
    call: &Call<'_, A>,
    cx: &impl ExpandCx,
    alloc: A,
) -> Result<Expanded<A>, ExpandError> {
    let def = prepared.def;
    if def.body().has_own_marks() {
        return Err(ExpandError::Unsealed { stream: Stream::Def });
    }
    if let Some(params) = def.params()
        && params.has_own_marks()
    {
        return Err(ExpandError::Unsealed { stream: Stream::Params });
    }
    call.check()?;
    if let Some(shape) = def.kind().shape()
        && shape != call.shape()
    {
        return Err(ExpandError::WrongCallKind { def: shape, call: call.shape() });
    }
    let mut out = Out { exp: Expansion::new_in(alloc.clone()), def, call, alloc };
    match def.kind() {
        MacroKind::Rules | MacroKind::Macro2 => {
            let rules = prepared.rules.as_ref().expect("`prepare` parses a declarative definition");
            declarative::expand(rules, &mut out)?;
        }
        MacroKind::BuiltinFn(which) => {
            if let builtin::Step::Needs(calls) = builtin::expand(which, &mut out, cx)? {
                return Ok(Expanded::NeedsExpanded(calls));
            }
        }
        MacroKind::BuiltinDerive(which) => derive::expand(which, &mut out)?,
        MacroKind::BuiltinAttr(which) => {
            return Err(ExpandError::NotBuilt { what: Unbuilt::BuiltinAttr(which) });
        }
        MacroKind::ProcMacro { kind, .. } => proc_macro(kind, &mut out, cx)?,
    }
    out.finish()
}

/// A proc macro's call, run by the context.
fn proc_macro<A: Allocator + Clone>(
    kind: ProcKind,
    out: &mut Out<'_, A>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    let call = ProcCall { kind, input: out.call.input(), attr_args: out.call.attr_args() };
    let produced = cx.run_proc_macro(out.def, call, out.alloc.clone())?;
    let (tokens, origins) = (produced.tokens.len(), produced.origins.len());
    if tokens != origins {
        return Err(ExpandError::ProcMacroOutput {
            tokens: crate::tokens::index(tokens),
            origins: crate::tokens::index(origins),
        });
    }
    if let Some(token) = produced.tokens.unbalanced() {
        return Err(ExpandError::Unbalanced { stream: Stream::Produced, token });
    }
    for (token, origin) in produced.tokens.tokens().iter().zip(produced.origins.iter()) {
        out.push(token.kind, produced.tokens.text_of(token), *origin)?;
    }
    out.exp.reads.extend(&produced.reads)?;
    Ok(())
}

/// An expansion being written: each token pushed with its origin, its context made from it.
pub(crate) struct Out<'c, A: Allocator + Clone> {
    pub(crate) exp: Expansion<A>,
    pub(crate) def: &'c MacroDef<A>,
    pub(crate) call: &'c Call<'c, A>,
    pub(crate) alloc: A,
}

impl<A: Allocator + Clone> Out<'_, A> {
    /// Appends a token from `origin`: a token of the call's input keeps its context; one of the
    /// definition, or made, gets this expansion's mark.
    pub(crate) fn push(
        &mut self,
        kind: TokenKind,
        text: &str,
        origin: Origin,
    ) -> Result<(), ExpandError> {
        let ctx = self.ctx_for(origin)?;
        self.exp.tokens.push(kind, text, ctx)?;
        self.exp.origins.push(origin);
        Ok(())
    }

    fn ctx_for(&mut self, origin: Origin) -> Result<Ctx, ExpandError> {
        let (def, call) = (self.def, self.call);
        let foreign = ExpandError::ForeignOrigin { origin };
        let tokens = &mut self.exp.tokens;
        let ctx = match origin {
            Origin::Input(at) => {
                let input = call.input();
                let token = input.tokens().get(at as usize).ok_or(foreign)?;
                tokens.import_ctx(input, token.ctx)?
            }
            Origin::AttrArgs(at) => {
                let args = call.attr_args().ok_or(foreign)?;
                let token = args.tokens().get(at as usize).ok_or(foreign)?;
                tokens.import_ctx(args, token.ctx)?
            }
            Origin::Def(at) => {
                let body = def.body();
                let token = body.tokens().get(at as usize).ok_or(foreign)?;
                let parent = tokens.import_ctx(body, token.ctx)?;
                tokens.context(CtxNode { parent, mark: def.mark(def.transparency()) })?
            }
            Origin::Params(at) => {
                let params = def.params().ok_or(foreign)?;
                let token = params.tokens().get(at as usize).ok_or(foreign)?;
                let parent = tokens.import_ctx(params, token.ctx)?;
                tokens.context(CtxNode { parent, mark: def.mark(def.transparency()) })?
            }
            // rustc's `def_site`: the root, marked opaque; `call_site` and `mixed_site`: the
            // call's own context, marked.
            Origin::Made(Transparency::Opaque) => {
                let parent = Ctx::root(def.edition());
                tokens.context(CtxNode { parent, mark: def.mark(Transparency::Opaque) })?
            }
            Origin::Made(transparency) => {
                let parent = tokens.import_ctx(call.input(), call.site.ctx)?;
                tokens.context(CtxNode { parent, mark: def.mark(transparency) })?
            }
        };
        Ok(ctx)
    }

    /// A token the expansion makes.
    pub(crate) fn made(
        &mut self,
        kind: TokenKind,
        text: &str,
        transparency: Transparency,
    ) -> Result<(), ExpandError> {
        self.push(kind, text, Origin::Made(transparency))
    }

    /// `#[automatically_derived]`, made def-site.
    pub(crate) fn automatically_derived(&mut self) -> Result<(), ExpandError> {
        let opaque = Transparency::Opaque;
        self.made(TokenKind::Punct { char: '#', spacing: Spacing::Alone }, "", opaque)?;
        self.made(TokenKind::Open(Delim::Bracket), "", opaque)?;
        let ident = TokenKind::Ident { raw: crate::tokens::Raw::No, spacing: Spacing::Alone };
        self.made(ident, "automatically_derived", opaque)?;
        self.made(TokenKind::Close(Delim::Bracket), "", opaque)
    }

    pub(crate) fn note(&mut self, text: &str) -> Result<Text, ExpandError> {
        Ok(self.exp.note(text)?)
    }

    pub(crate) fn note_display(
        &mut self,
        value: &impl core::fmt::Display,
    ) -> Result<Text, ExpandError> {
        Ok(self.exp.note_display(value)?)
    }

    pub(crate) fn diagnose(&mut self, diagnostic: Diagnostic) {
        self.exp.diagnostics.push(diagnostic);
    }

    /// The expansion: with a diagnostic, no tokens (the call's error is all it makes); else
    /// its nested calls listed.
    fn finish(mut self) -> Result<Expanded<A>, ExpandError> {
        if !self.exp.diagnostics.is_empty() {
            self.exp.tokens.clear();
            self.exp.origins.clear();
            self.exp.holes.clear();
            return Ok(Expanded::Expansion(self.exp));
        }
        let mut calls = nested::find_calls(&self.exp.tokens, self.alloc.clone())?;
        if self.def.at().local_inner == LocalInner::Yes {
            for call in calls.iter_mut() {
                let single = call.path.end - call.path.start == 1;
                let from_def = matches!(self.exp.origins[call.path.start as usize], Origin::Def(_));
                if single && from_def {
                    call.scope = PathScope::DefCrate;
                }
            }
        }
        self.exp.nested = calls;
        Ok(Expanded::Expansion(self.exp))
    }
}
