//! Compiled declarative macro expanders (`macro_rules!` and `macro`)

use std::ops::ControlFlow;

use base_db::{Db, Crate};
use blaze_mem::Frame;
use span::{Edition, Span, SyntaxContext};
use syntax::{
    ast,
    view::{self, AstNode as _, HasAttrs as _},
};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    AstId, ExpandError, ExpandErrorKind, ExpandResult, HirFileId, Lookup, MacroCallId,
    attrs::{AstKeyValueMetaExt, AstPathExt, expand_cfg_attr_view},
    hygiene::{MarkCache, Transparency, apply_mark},
    span_map::InOrder,
    tt,
};

/// Old-style `macro_rules` or the new macros 2.0
#[derive(Debug, Eq, PartialEq)]
pub struct DeclarativeMacroExpander {
    pub mac: mbe::DeclarativeMacro,
    pub transparency: Transparency,
    edition: Edition,
}

impl DeclarativeMacroExpander {
    /// The expansion of `tt` as a global tree, for a caller outside a query's run.
    pub fn expand(
        &self,
        db: &Db,
        tt: &tt::TopSubtree,
        call_id: MacroCallId,
        span: Span,
    ) -> ExpandResult<(tt::TopSubtree, Option<u32>)> {
        match self.mac.err() {
            Some(_) => ExpandResult::new(
                (tt::TopSubtree::empty(tt::DelimSpan { open: span, close: span }), None),
                ExpandError::new(span, ExpandErrorKind::MacroDefinition),
            ),
            None => {
                let marks = MarkCache::new();
                let call_style = call_id.loc(db).kind.call_style();
                self.mac
                    .expand(db, tt, self.marker(db, call_id, &marks), call_style, span)
                    .map_err(Into::into)
            }
        }
    }

    /// [`Self::expand`] in `frame`, with the spans of the identifiers its `${concat(..)}`s made.
    pub fn expand_making<'f>(
        &self,
        db: &Db,
        frame: &'f Frame,
        tt: &tt::TopSubtree,
        call_id: MacroCallId,
        span: Span,
    ) -> ExpandResult<(tt::TopSubtree<&'f Frame>, Option<u32>, mbe::MadeIdents)> {
        match self.mac.err() {
            Some(_) => ExpandResult::new(
                (
                    tt::TopSubtree::empty_in(tt::DelimSpan { open: span, close: span }, frame),
                    None,
                    mbe::MadeIdents::new(),
                ),
                ExpandError::new(span, ExpandErrorKind::MacroDefinition),
            ),
            None => {
                let marks = MarkCache::new();
                let call_style = call_id.loc(db).kind.call_style();
                self.mac
                    .expand_making_in(
                        frame,
                        db,
                        tt,
                        self.marker(db, call_id, &marks),
                        call_style,
                        span,
                    )
                    .map_err(Into::into)
            }
        }
    }

    /// The hygiene `call_id`'s expansion marks each span with, `marks` keeping the contexts made.
    fn marker<'a>(
        &'a self,
        db: &'a Db,
        call_id: MacroCallId,
        marks: &'a MarkCache<SyntaxContext, SyntaxContext>,
    ) -> impl Fn(&mut Span) + Copy + 'a {
        move |s| {
            s.ctx = marks.get_or_insert_with(s.ctx, |ctx| {
                apply_mark(db, ctx, call_id.into(), self.transparency, self.edition)
            })
        }
    }
}

#[blaze_query::tracked]
impl AstId<ast::Macro> {
    /// Fetches (and compiles) the expander of this decl macro.
    #[blaze_query::tracked(returns(ref), mem_tag = Expand)]
    pub fn decl_macro_expander(
        self,
        db: &Db,
        def_crate: Crate,
    ) -> DeclarativeMacroExpander {
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let id = self;
            let map = id.file_id.span_map(db);

            let transparency = |node: view::AnyHasAttrs<'_>| {
                let mut cfg_options = None;
                expand_cfg_attr_view(
                    node.attrs(),
                    || cfg_options.get_or_insert_with(|| def_crate.cfg_options(db)),
                    |attr, _| {
                        if let view::Meta::KeyValueMeta(attr) = attr
                            && attr.path().is1("rustc_macro_transparency")
                            && let Some(value) = attr.value_string()
                        {
                            match &*value {
                                "transparent" => ControlFlow::Break(Transparency::Transparent),
                                // "semitransparent" is for old rustc versions.
                                "semiopaque" | "semitransparent" => {
                                    ControlFlow::Break(Transparency::SemiOpaque)
                                }
                                "opaque" => ControlFlow::Break(Transparency::Opaque),
                                _ => ControlFlow::Continue(()),
                            }
                        } else {
                            ControlFlow::Continue(())
                        }
                    },
                )
            };
            let ctx_edition = |ctx: SyntaxContext| {
                if ctx.is_root() {
                    def_crate.data(db).edition
                } else {
                    // UNWRAP-SAFETY: Only the root context has no outer expansion
                    let krate =
                        crate::MacroCallId::from(ctx.outer_expn(db).unwrap()).loc(db).def.krate;
                    krate.data(db).edition
                }
            };
            let edition = ctx_edition(match id.file_id {
                HirFileId::MacroFile(macro_file) => macro_file.lookup(db).ctxt,
                HirFileId::FileId(file) => SyntaxContext::root(file.edition(db)),
            });
            let (mac, transparency) = match id.to_view(db) {
                view::Macro::MacroRules(macro_rules) => (
                    match macro_rules.token_tree() {
                        Some(arg) => {
                            let tt = syntax_bridge::call_input_in(
                                arg.syntax(),
                                InOrder::new(map),
                                map.span_for_range(
                                    macro_rules.macro_rules_token().unwrap().text_range(),
                                ),
                                DocCommentDesugarMode::Mbe,
                                frame,
                            );

                            mbe::DeclarativeMacro::parse_macro_rules(&tt, ctx_edition)
                        }
                        None => mbe::DeclarativeMacro::from_err(mbe::ParseError::Expected(
                            "expected a token tree".into(),
                        )),
                    },
                    transparency(view::AnyHasAttrs::new(macro_rules))
                        .unwrap_or(Transparency::SemiOpaque),
                ),
                view::Macro::MacroDef(macro_def) => (
                    match macro_def.body() {
                        Some(body) => {
                            let span =
                                map.span_for_range(macro_def.macro_token().unwrap().text_range());
                            let args = macro_def.args().map(|args| {
                                syntax_bridge::call_input_in(
                                    args.syntax(),
                                    InOrder::new(map),
                                    span,
                                    DocCommentDesugarMode::Mbe,
                                    frame,
                                )
                            });
                            let body = syntax_bridge::call_input_in(
                                body.syntax(),
                                InOrder::new(map),
                                span,
                                DocCommentDesugarMode::Mbe,
                                frame,
                            );

                            mbe::DeclarativeMacro::parse_macro2(args.as_ref(), &body, ctx_edition)
                        }
                        None => mbe::DeclarativeMacro::from_err(mbe::ParseError::Expected(
                            "expected a token tree".into(),
                        )),
                    },
                    transparency(view::AnyHasAttrs::new(macro_def))
                        .unwrap_or(Transparency::Opaque),
                ),
            };
            DeclarativeMacroExpander { mac, transparency, edition }
        })
    }
}
