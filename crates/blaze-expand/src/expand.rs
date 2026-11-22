//! [`expand`]: one macro call of one definition to its [`Expanded`].

use blaze_own::Alloc;

use crate::{
    builtin,
    call::Call,
    cx::{ExpandCx, HostAsk, HostError, HostWhy, Place, PlaceOf, ProcCall, ProcDiagnostic},
    def::{BuiltinAttr, LocalInner, MacroDefinition, MacroKind, ProcKind},
    derive,
    error::{ExpandError, Stream, Unbuilt},
    expansion::{Diagnostic, Expanded, Expansion, TokenOrigin, PathScope},
    global_allocator, nested, rules,
    tokens::{Ctx, CtxNode, Delim, Spacing, Text, TokenKind, Transparency},
};

/// A definition made ready to expand calls: a declarative macro compiled once into its
/// matching program ([`rules::Program`]).
pub struct Prepared<'d> {
    def: &'d MacroDefinition<'d>,
    program: Option<rules::Program<'d>>,
}

impl<'d> Prepared<'d> {
    pub fn def(&self) -> &'d MacroDefinition<'d> {
        self.def
    }
}

/// Compiles a declarative definition's rules; other kinds have nothing to prepare. A definition
/// whose streams cannot be read (unbalanced, or nested past the bound) is refused here; one whose
/// rules are wrong compiles to a program that reports that on each call.
pub fn prepare<'d>(def: &'d MacroDefinition<'_>) -> Result<Prepared<'d>, ExpandError> {
    let program = match def.kind() {
        MacroKind::Rules | MacroKind::Macro2 => Some(rules::compile(def)?),
        MacroKind::BuiltinFn(_)
        | MacroKind::BuiltinAttr(_)
        | MacroKind::BuiltinDerive(_)
        | MacroKind::ProcMacro { .. } => None,
    };
    Ok(Prepared { def, program })
}

/// Expands `call` of `def`: the expansion, or the nested calls an eager macro needs first.
/// Everything outside the tokens is asked of `cx` and recorded in the expansion.
pub fn expand<'a>(
    def: &MacroDefinition<'_>,
    call: &Call<'_>,
    cx: &impl ExpandCx,
    alloc: Alloc<'a>,
) -> Result<Expanded<'a>, ExpandError> {
    expand_prepared(&prepare(def)?, call, cx, alloc)
}

/// [`expand`] of a definition [`prepare`]d once for many calls.
pub fn expand_prepared<'a>(
    prepared: &Prepared<'_>,
    call: &Call<'_>,
    cx: &impl ExpandCx,
    alloc: Alloc<'a>,
) -> Result<Expanded<'a>, ExpandError> {
    let def = prepared.def;
    if def.body().has_own_marks() {
        return Err(ExpandError::Unsealed { stream: Stream::Def });
    }
    if let Some(params) = def.params()
        && params.has_own_marks()
    {
        return Err(ExpandError::Unsealed { stream: Stream::Params });
    }
    let checked = call.check()?;
    if let Some(shape) = def.kind().shape()
        && shape != call.shape()
    {
        return Err(ExpandError::WrongCallKind { def: shape, call: call.shape() });
    }
    let mut out = Out { exp: Expansion::new_in(alloc), def, call, alloc };
    match def.kind() {
        MacroKind::Rules | MacroKind::Macro2 => {
            let program =
                prepared.program.as_ref().expect("`prepare` compiles a declarative definition");
            rules::expand(program, &mut out, checked)?;
        }
        MacroKind::BuiltinFn(which) => {
            if let builtin::Step::Needs(calls) = builtin::expand(which, &mut out, cx)? {
                return Ok(Expanded::NeedsExpanded(calls));
            }
        }
        MacroKind::BuiltinDerive(which) => derive::expand(which, &mut out)?,
        MacroKind::BuiltinAttr(BuiltinAttr::GlobalAllocator) => global_allocator::expand(&mut out)?,
        MacroKind::BuiltinAttr(which) => {
            return Err(ExpandError::NotBuilt { what: Unbuilt::BuiltinAttr(which) });
        }
        MacroKind::ProcMacro { kind, .. } => proc_macro(kind, &mut out, cx)?,
    }
    out.finish()
}

/// A proc macro's call, run by the context.
fn proc_macro(
    kind: ProcKind,
    out: &mut Out<'_, '_>,
    cx: &impl ExpandCx,
) -> Result<(), ExpandError> {
    // The call's file, for `Span::file` (D96: the same answer `file!()` gets here); a read of
    // the run only if the macro asks it.
    let file = match cx.place(PlaceOf::File) {
        Ok(Place::Text(file)) => Ok(file),
        Ok(Place::Number(_)) => Err(HostError { ask: HostAsk::Place, why: HostWhy::Unreadable }),
        Err(error) => Err(error),
    };
    let call = ProcCall { kind, input: out.call.input(), attr_args: out.call.attr_args(), file };
    let produced = cx.run_proc_macro(out.def, call, out.alloc)?;
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
    // The host's output is untrusted (a wasm module's): every origin and text is checked
    // before it is used, so none can index out of bounds.
    for origin in produced.origins.iter() {
        proc_origin(out.call, *origin)?;
    }
    for (token, origin) in produced.tokens.tokens().iter().zip(produced.origins.iter()) {
        out.push(token.kind, produced.tokens.text_of(token), *origin)?;
    }
    let len = crate::tokens::index(produced.text.len());
    let rebase = |out: &mut Out<'_, '_>, text: Text| -> Result<Text, ExpandError> {
        let s = text.get(&produced.text).ok_or(ExpandError::ProcMacroText { text, len })?;
        out.note(s)
    };
    for diagnostic in produced.diagnostics.iter() {
        let diagnostic = match *diagnostic {
            ProcDiagnostic::Emitted { level, message, at } => {
                if let Some(origin) = at {
                    proc_origin(out.call, origin)?;
                }
                Diagnostic::ProcMacro { level, message: rebase(out, message)?, at }
            }
            ProcDiagnostic::Panicked { message } => {
                Diagnostic::ProcMacroPanicked { message: rebase(out, message)? }
            }
        };
        out.diagnose(diagnostic);
    }
    out.exp.reads.extend(&produced.reads)?;
    Ok(())
}

/// Whether a proc macro may give `origin`: a token of its input or its attribute's arguments,
/// or one made; never the definition's.
fn proc_origin(call: &Call<'_>, origin: TokenOrigin) -> Result<(), ExpandError> {
    let given = match origin {
        TokenOrigin::Input(i) => (i as usize) < call.input().len(),
        TokenOrigin::AttrArgs(i) => call.attr_args().is_some_and(|args| (i as usize) < args.len()),
        TokenOrigin::Def(_) | TokenOrigin::Params(_) => false,
        TokenOrigin::Made(_) => true,
    };
    match given {
        true => Ok(()),
        false => Err(ExpandError::ProcMacroOrigin { origin }),
    }
}

/// An expansion being written: each token pushed with its origin, its context made from it.
/// `'c` borrows the definition and the call; `'a` is the allocator the expansion is written in.
pub(crate) struct Out<'c, 'a> {
    pub(crate) exp: Expansion<'a>,
    pub(crate) def: &'c MacroDefinition<'c>,
    pub(crate) call: &'c Call<'c>,
    pub(crate) alloc: Alloc<'a>,
}

impl<'a> Out<'_, 'a> {
    /// Appends a token from `origin`: a token of the call's input keeps its context; one of the
    /// definition, or made, gets this expansion's mark.
    pub(crate) fn push(
        &mut self,
        kind: TokenKind,
        text: &str,
        origin: TokenOrigin,
    ) -> Result<(), ExpandError> {
        let ctx = self.ctx_for(origin)?;
        self.exp.tokens.push(kind, text, ctx)?;
        self.exp.origins.push(origin);
        Ok(())
    }


    /// [`Out::push`] with the context already made for `origin` ([`Out::ctx_for`]), of a text
    /// proved whole ([`crate::tokens::WholeText`]): a token's text copied with no UTF-8 check.
    pub(crate) fn push_whole_ctx(
        &mut self,
        kind: TokenKind,
        text: crate::tokens::WholeText<'_>,
        origin: TokenOrigin,
        ctx: Ctx,
    ) -> Result<(), ExpandError> {
        self.exp.tokens.push_whole(kind, text, ctx)?;
        self.exp.origins.push(origin);
        Ok(())
    }

    /// The context of this expansion a token from `origin` gets.
    pub(crate) fn ctx_for(&mut self, origin: TokenOrigin) -> Result<Ctx, ExpandError> {
        let (def, call) = (self.def, self.call);
        let foreign = ExpandError::ForeignOrigin { origin };
        let tokens = &mut self.exp.tokens;
        let ctx = match origin {
            TokenOrigin::Input(at) => {
                let input = call.input();
                let token = input.tokens().get(at as usize).ok_or(foreign)?;
                tokens.import_ctx(input, token.ctx)?
            }
            TokenOrigin::AttrArgs(at) => {
                let args = call.attr_args().ok_or(foreign)?;
                let token = args.tokens().get(at as usize).ok_or(foreign)?;
                tokens.import_ctx(args, token.ctx)?
            }
            TokenOrigin::Def(at) => {
                let body = def.body();
                let token = body.tokens().get(at as usize).ok_or(foreign)?;
                let parent = tokens.import_ctx(body, token.ctx)?;
                tokens.context(CtxNode { parent, mark: def.mark(def.transparency()) })?
            }
            TokenOrigin::Params(at) => {
                let params = def.params().ok_or(foreign)?;
                let token = params.tokens().get(at as usize).ok_or(foreign)?;
                let parent = tokens.import_ctx(params, token.ctx)?;
                tokens.context(CtxNode { parent, mark: def.mark(def.transparency()) })?
            }
            // rustc's `def_site`: the root, marked opaque; `call_site` and `mixed_site`: the
            // call's own context, marked.
            TokenOrigin::Made(Transparency::Opaque) => {
                let parent = Ctx::root(def.edition());
                tokens.context(CtxNode { parent, mark: def.mark(Transparency::Opaque) })?
            }
            TokenOrigin::Made(transparency) => {
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
        self.push(kind, text, TokenOrigin::Made(transparency))
    }

    /// Writes `code`, made with `transparency`: a run of letters, digits, `_` and `$` is an
    /// identifier (`$crate` one), each other character a punct, joined to the next when that is
    /// a punct too, and a bracket a group's delimiter. Spaces only separate. `code` is ASCII.
    pub(crate) fn made_code(
        &mut self,
        code: &str,
        transparency: Transparency,
    ) -> Result<(), ExpandError> {
        let bytes = code.as_bytes();
        let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
        let delim = |byte: u8| b"()[]{}".contains(&byte);
        let mut at = 0;
        while let Some(&byte) = bytes.get(at) {
            if byte == b' ' {
                at += 1;
                continue;
            }
            if word(byte) {
                let start = at;
                while bytes.get(at).is_some_and(|&it| word(it)) {
                    at += 1;
                }
                let ident = TokenKind::Ident { raw: crate::tokens::Raw::No, spacing: Spacing::Alone };
                self.made(ident, &code[start..at], transparency)?;
                continue;
            }
            let kind = match byte {
                b'(' => TokenKind::Open(Delim::Paren),
                b')' => TokenKind::Close(Delim::Paren),
                b'[' => TokenKind::Open(Delim::Bracket),
                b']' => TokenKind::Close(Delim::Bracket),
                b'{' => TokenKind::Open(Delim::Brace),
                b'}' => TokenKind::Close(Delim::Brace),
                _ => {
                    let joined = bytes.get(at + 1).is_some_and(|&next| {
                        next.is_ascii_punctuation() && !word(next) && !delim(next)
                    });
                    let spacing = if joined { Spacing::Joint } else { Spacing::Alone };
                    TokenKind::Punct { char: char::from(byte), spacing }
                }
            };
            self.made(kind, "", transparency)?;
            at += 1;
        }
        Ok(())
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

    /// The expansion: with an error ([`Diagnostic::is_error`]), no tokens (the call's error is all it makes); else
    /// its nested calls listed.
    fn finish(mut self) -> Result<Expanded<'a>, ExpandError> {
        if self.exp.diagnostics.iter().any(Diagnostic::is_error) {
            self.exp.tokens.clear();
            self.exp.origins.clear();
            self.exp.holes.clear();
            return Ok(Expanded::Expansion(self.exp));
        }
        let mut calls = nested::find_calls(&self.exp.tokens, self.alloc)?;
        if self.def.at().local_inner == LocalInner::Yes {
            for call in calls.iter_mut() {
                let single = call.path.end - call.path.start == 1;
                let from_def = matches!(self.exp.origins[call.path.start as usize], TokenOrigin::Def(_));
                if single && from_def {
                    call.scope = PathScope::DefCrate;
                }
            }
        }
        self.exp.nested = calls;
        Ok(Expanded::Expansion(self.exp))
    }
}
