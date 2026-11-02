//! Patterns, with rustc's `parse_pat_with_range_pat` span: the pattern's first token to its
//! last; an or-pattern starts at its leading `|` when it has one.

use syntax::T;
use syntax::view::{self as ra, AstNode, HasName as _};

use super::Lower;
use crate::ast::{self, DUMMY_NODE_ID, ThinVec};
use crate::span::{Ident, Span, Spanned, sym};

impl Lower<'_> {
    pub(crate) fn pat(&self, pat: ra::Pat<'_>) -> Box<ast::Pat> {
        Box::new(self.pat_val(pat))
    }

    pub(crate) fn pat_val(&self, pat: ra::Pat<'_>) -> ast::Pat {
        let span = self.nsp(pat.syntax());
        let kind = match pat {
            ra::Pat::IdentPat(p) => {
                let by_ref = match p.ref_token() {
                    Some(_) => ast::ByRef::Yes(
                        ast::Pinnedness::Not,
                        if p.mut_token().is_some() {
                            ast::Mutability::Mut
                        } else {
                            ast::Mutability::Not
                        },
                    ),
                    None => ast::ByRef::No,
                };
                let mutbl = if p.ref_token().is_none() && p.mut_token().is_some() {
                    ast::Mutability::Mut
                } else {
                    ast::Mutability::Not
                };
                let ident = match p.name() {
                    Some(n) => self.name(n),
                    None => {
                        self.unsupported(span, "a binding without a name");
                        Ident::new(sym::empty, span)
                    }
                };
                let sub = p.pat().map(|sub| self.pat(sub));
                ast::PatKind::Ident(ast::BindingMode(by_ref, mutbl), ident, sub)
            }
            ra::Pat::WildcardPat(_) => ast::PatKind::Wild,
            ra::Pat::RestPat(_) => ast::PatKind::Rest,
            ra::Pat::LiteralPat(_) | ra::Pat::ConstBlockPat(_) => {
                ast::PatKind::Expr(self.pat_expr(pat))
            }
            ra::Pat::PathPat(p) => match p.path() {
                Some(path) => {
                    let (qself, path) = self.path(path);
                    ast::PatKind::Path(qself, path)
                }
                None => self.pat_kind_err(span),
            },
            ra::Pat::RangePat(p) => {
                use syntax::view::RangeItem as _;
                let op = p
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .find(|t| matches!(t.kind(), T![..] | T![..=] | T![...]));
                let Some(op) = op else {
                    return self.pat_err(span);
                };
                let end = match op.kind() {
                    T![..=] => ast::RangeEnd::Included(ast::RangeSyntax::DotDotEq),
                    T![...] => ast::RangeEnd::Included(ast::RangeSyntax::DotDotDot),
                    _ => ast::RangeEnd::Excluded,
                };
                let re = Spanned {
                    node: end,
                    span: self.tsp(op),
                };
                let start = p.start().map(|s| self.pat_expr(s));
                let hi = p.end().map(|e| self.pat_expr(e));
                ast::PatKind::Range(start, hi, re)
            }
            ra::Pat::RecordPat(p) => {
                let (qself, path) = match p.path() {
                    Some(path) => self.path(path),
                    None => (None, self.err_path(span)),
                };
                let mut fields = ThinVec::new();
                let mut rest = ast::PatFieldsRest::None;
                if let Some(list) = p.record_pat_field_list() {
                    for f in list.fields() {
                        fields.push(self.pat_field(f));
                    }
                    if let Some(r) = list.rest_pat() {
                        rest = ast::PatFieldsRest::Rest(self.nsp(r.syntax()));
                    }
                }
                ast::PatKind::Struct(qself, path, fields, rest)
            }
            ra::Pat::TupleStructPat(p) => {
                let (qself, path) = match p.path() {
                    Some(path) => self.path(path),
                    None => (None, self.err_path(span)),
                };
                ast::PatKind::TupleStruct(
                    qself,
                    path,
                    p.fields().map(|f| self.pat_val(f)).collect(),
                )
            }
            ra::Pat::TuplePat(p) => {
                let fields: ThinVec<ast::Pat> = p.fields().map(|f| self.pat_val(f)).collect();
                // `(p)` is a parenthesized pattern; `(p,)` and `(..)` are tuples.
                let trailing_comma = p.syntax().child_token(T![,]).is_some();
                if fields.len() == 1
                    && !trailing_comma
                    && !matches!(fields[0].kind, ast::PatKind::Rest)
                {
                    ast::PatKind::Paren(Box::new(
                        fields
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| self.pat_err(span)),
                    ))
                } else {
                    ast::PatKind::Tuple(fields)
                }
            }
            ra::Pat::ParenPat(p) => match p.pat() {
                Some(inner) => ast::PatKind::Paren(self.pat(inner)),
                None => self.pat_kind_err(span),
            },
            ra::Pat::SlicePat(p) => {
                ast::PatKind::Slice(p.pats().map(|f| self.pat_val(f)).collect())
            }
            ra::Pat::RefPat(p) => {
                let mutbl = if p.mut_token().is_some() {
                    ast::Mutability::Mut
                } else {
                    ast::Mutability::Not
                };
                match p.pat() {
                    Some(inner) => ast::PatKind::Ref(self.pat(inner), ast::Pinnedness::Not, mutbl),
                    None => self.pat_kind_err(span),
                }
            }
            ra::Pat::OrPat(p) => ast::PatKind::Or(p.pats().map(|f| self.pat_val(f)).collect()),
            ra::Pat::GuardPat(p) => match (p.pat(), p.guard()) {
                (Some(inner), Some(guard)) => ast::PatKind::Guard(self.pat(inner), self.guard(guard, span)),
                _ => self.pat_kind_err(span),
            },
            ra::Pat::MacroPat(p) => match p.macro_call() {
                Some(mc) => ast::PatKind::MacCall(self.mac_call(mc)),
                None => self.pat_kind_err(span),
            },
            ra::Pat::BoxPat(_) => {
                self.unsupported(span, "a `box` pattern");
                ast::PatKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
            }
            ra::Pat::DerefPat(_) | ra::Pat::CfgPredPat(_) | ra::Pat::NotNull(_) => {
                self.unsupported(span, "a builtin pattern");
                ast::PatKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
            }
        };
        ast::Pat {
            id: DUMMY_NODE_ID,
            kind,
            span,
        }
    }

    fn pat_err(&self, span: Span) -> ast::Pat {
        ast::Pat {
            id: DUMMY_NODE_ID,
            kind: self.pat_kind_err(span),
            span,
        }
    }

    fn pat_kind_err(&self, span: Span) -> ast::PatKind {
        self.unsupported(span, "a pattern rust-analyzer did not parse");
        ast::PatKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
    }

    /// A literal, path or `const` block in pattern position as the expression rustc parses it to
    /// (`parse_literal_maybe_minus`, `parse_pat_range_end`).
    fn pat_expr(&self, pat: ra::Pat<'_>) -> Box<ast::Expr> {
        let span = self.nsp(pat.syntax());
        let kind = match pat {
            ra::Pat::LiteralPat(p) => {
                let lit = match p.literal() {
                    Some(l) => self.lit_expr(l),
                    None => return self.expr_err(span),
                };
                match p.minus_token() {
                    Some(_) => ast::ExprKind::Unary(ast::UnOp::Neg, lit),
                    None => return lit,
                }
            }
            ra::Pat::PathPat(p) => match p.path() {
                Some(path) => {
                    let (qself, path) = self.path(path);
                    ast::ExprKind::Path(qself, path)
                }
                None => return self.expr_err(span),
            },
            ra::Pat::ConstBlockPat(p) => match p.block_expr() {
                Some(b) => {
                    let block = self.block_expr_inner(b);
                    let value = Box::new(ast::Expr {
                        id: DUMMY_NODE_ID,
                        kind: ast::ExprKind::Block(block, None),
                        span: self.nsp(b.syntax()),
                        attrs: ThinVec::new(),
                        tokens: None,
                    });
                    ast::ExprKind::ConstBlock(ast::AnonConst {
                        id: DUMMY_NODE_ID,
                        value,
                    })
                }
                None => return self.expr_err(span),
            },
            ra::Pat::IdentPat(p)
                if p.ref_token().is_none() && p.mut_token().is_none() && p.pat().is_none() =>
            {
                // `X..=Y` with plain names: rust-analyzer keeps them as bindings.
                match p.name() {
                    Some(n) => ast::ExprKind::Path(None, ast::Path::from_ident(self.name(n))),
                    None => return self.expr_err(span),
                }
            }
            _ => {
                self.unsupported(span, "a range pattern bound rustc would reject");
                return self.expr_err(span);
            }
        };
        Box::new(ast::Expr {
            id: DUMMY_NODE_ID,
            kind,
            span,
            attrs: ThinVec::new(),
            tokens: None,
        })
    }

    /// rustc's `parse_pat_field`: from the field name (after attributes) to the end of the
    /// pattern.
    fn pat_field(&self, f: ra::RecordPatField<'_>) -> ast::PatField {
        let span = self.nsp(f.syntax());
        let attrs = self.outer_attrs(f.syntax());
        let (ident, pat, is_shorthand) = match (f.name_ref(), f.pat()) {
            (Some(name), Some(pat)) if f.colon_token().is_some() => {
                (self.name_ref(name), self.pat(pat), false)
            }
            (_, Some(pat)) => {
                let pat = self.pat(pat);
                let ident = match &pat.kind {
                    ast::PatKind::Ident(_, ident, None) => *ident,
                    _ => {
                        self.unsupported(span, "a shorthand field pattern rustc would reject");
                        Ident::new(sym::empty, span)
                    }
                };
                (ident, pat, true)
            }
            _ => {
                self.unsupported(span, "a field pattern rust-analyzer did not parse");
                (
                    Ident::new(sym::empty, span),
                    Box::new(self.pat_err(span)),
                    false,
                )
            }
        };
        ast::PatField {
            ident,
            pat,
            is_shorthand,
            attrs,
            id: DUMMY_NODE_ID,
            span,
            is_placeholder: false,
        }
    }
}
