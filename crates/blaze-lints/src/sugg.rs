//! An expression as clippy writes it back in a suggestion, with the parentheses it needs where
//! it goes: clippy's `Sugg`.
//!
//! Follows clippy's `clippy_utils/src/sugg.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0), over rust-analyzer's syntax
//! rather than HIR: what lowering turns into another kind (`?`, `.await`, ranges, parentheses)
//! is taken as the kind it lowers to.

use std::{borrow::Cow, fmt};

use syntax::view as ast;

use ast::{AstNode as _, RangeItem as _};

/// clippy's `AssocOp`: the operator of a binary expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    /// A binary operator, as written: `==`, `+`, `&&`.
    Binary(String),
    Assign,
    /// `+=` and the like, as written.
    AssignOp(String),
    Cast,
    /// `..` or `..=`.
    Range(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UnOp {
    Not,
    Neg,
}

impl UnOp {
    fn as_str(&self) -> &'static str {
        match self {
            UnOp::Not => "!",
            UnOp::Neg => "-",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sugg {
    /// One that never needs parentheses: `1337`, `[0; 42]`.
    NonParen(String),
    MaybeParen(String),
    BinOp(Op, String, String),
    UnOp(UnOp, Box<Sugg>),
}

impl fmt::Display for Sugg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sugg::NonParen(it) | Sugg::MaybeParen(it) => it.fmt(f),
            Sugg::BinOp(op, lhs, rhs) => binop_to_string(op, lhs, rhs).fmt(f),
            Sugg::UnOp(op, inner) => {
                write!(f, "{}{}", op.as_str(), inner.clone().maybe_inner_paren())
            }
        }
    }
}

fn binop_to_string(op: &Op, lhs: &str, rhs: &str) -> String {
    match op {
        Op::Binary(op) | Op::AssignOp(op) => format!("{lhs} {op} {rhs}"),
        Op::Assign => format!("{lhs} = {rhs}"),
        Op::Cast => format!("{lhs} as {rhs}"),
        Op::Range(limits) => format!("{lhs}{limits}{rhs}"),
    }
}

/// Whether `sugg` is in parentheses as a whole.
pub(crate) fn has_enclosing_paren(sugg: &str) -> bool {
    let mut chars = sugg.chars();
    if chars.next() != Some('(') {
        return false;
    }
    let mut depth = 1;
    for c in &mut chars {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
    }
    chars.next().is_none()
}

impl Sugg {
    /// clippy's `Sugg::hir_with_context` for an expression written in the context it is
    /// suggested in; `snippet` is the source of a node, and `same_type` whether a `!` or `-`
    /// expression has the type of its operand.
    pub(crate) fn of<'t>(
        expr: ast::Expr<'t>,
        snippet: &impl Fn(ast::Node<'t>) -> Cow<'t, str>,
        same_type: &impl Fn(ast::PrefixExpr<'t>) -> bool,
    ) -> Sugg {
        if let ast::Expr::PrefixExpr(prefix) = expr
            && let Some(op) = prefix.op_kind()
            && let Some(inner) = prefix.expr()
        {
            let op = match op {
                ast::UnaryOp::Not => Some(UnOp::Not),
                ast::UnaryOp::Neg => Some(UnOp::Neg),
                ast::UnaryOp::Deref => None,
            };
            if let Some(op) = op
                && same_type(prefix)
            {
                return Sugg::UnOp(op, Box::new(Sugg::of(inner, snippet, same_type)));
            }
        }
        Sugg::from_snippet(expr, snippet)
    }

    /// clippy's `Sugg::hir_from_snippet`.
    pub(crate) fn from_snippet<'t>(
        expr: ast::Expr<'t>,
        snippet: &impl Fn(ast::Node<'t>) -> Cow<'t, str>,
    ) -> Sugg {
        let text = || snippet(expr.syntax()).into_owned();
        let of = |it: Option<ast::Expr<'t>>| {
            it.map(|it| snippet(it.syntax()).into_owned()).unwrap_or_default()
        };
        match expr {
            ast::Expr::RangeExpr(range) => {
                let limits = match range.op_kind() {
                    Some(ast::RangeOp::Inclusive) => "..=",
                    _ => "..",
                };
                Sugg::BinOp(Op::Range(limits.to_owned()), of(range.start()), of(range.end()))
            }
            ast::Expr::ParenExpr(paren) => match paren.expr() {
                Some(
                    inner @ (ast::Expr::BinExpr(_)
                    | ast::Expr::CastExpr(_)
                    | ast::Expr::RangeExpr(_)),
                ) => Sugg::from_snippet(inner, snippet),
                Some(
                    ast::Expr::RefExpr(_)
                    | ast::Expr::IfExpr(_)
                    | ast::Expr::LetExpr(_)
                    | ast::Expr::ClosureExpr(_)
                    | ast::Expr::PrefixExpr(_)
                    | ast::Expr::MatchExpr(_)
                    | ast::Expr::ForExpr(_),
                ) => Sugg::MaybeParen(text()),
                _ => Sugg::NonParen(text()),
            },
            ast::Expr::RefExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::LetExpr(_)
            | ast::Expr::ClosureExpr(_)
            | ast::Expr::PrefixExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::ForExpr(_) => Sugg::MaybeParen(text()),
            ast::Expr::BinExpr(bin) => {
                let op = bin.op_token().map(|it| it.text().to_owned()).unwrap_or_default();
                let op = match bin.op_kind() {
                    Some(ast::BinaryOp::Assignment { op: None }) => Op::Assign,
                    Some(ast::BinaryOp::Assignment { op: Some(_) }) => Op::AssignOp(op),
                    _ => Op::Binary(op),
                };
                Sugg::BinOp(op, of(bin.lhs()), of(bin.rhs()))
            }
            ast::Expr::CastExpr(cast) => {
                let ty = cast.ty().map(|it| snippet(it.syntax()).into_owned()).unwrap_or_default();
                Sugg::BinOp(Op::Cast, of(cast.expr()), ty)
            }
            _ => Sugg::NonParen(text()),
        }
    }

    pub(crate) fn maybe_paren(self) -> Sugg {
        match self {
            Sugg::NonParen(_) => self,
            Sugg::MaybeParen(sugg) => match has_enclosing_paren(&sugg) {
                true => Sugg::MaybeParen(sugg),
                false => Sugg::NonParen(format!("({sugg})")),
            },
            Sugg::BinOp(op, lhs, rhs) => {
                Sugg::NonParen(format!("({})", binop_to_string(&op, &lhs, &rhs)))
            }
            Sugg::UnOp(op, inner) => {
                Sugg::NonParen(format!("({}{})", op.as_str(), inner.maybe_inner_paren()))
            }
        }
    }

    fn starts_with_unary_op(&self) -> bool {
        match self {
            Sugg::UnOp(..) => true,
            Sugg::BinOp(..) => false,
            Sugg::MaybeParen(it) | Sugg::NonParen(it) => it.starts_with(['*', '!', '-', '&']),
        }
    }

    fn maybe_inner_paren(self) -> Sugg {
        match self.starts_with_unary_op() {
            true => self,
            false => self.maybe_paren(),
        }
    }

    pub(crate) fn make_return(self) -> Sugg {
        Sugg::NonParen(format!("return {self}"))
    }

    pub(crate) fn blockify(self) -> Sugg {
        Sugg::NonParen(format!("{{ {self} }}"))
    }

    pub(crate) fn deref(self) -> Sugg {
        make_unop("*", self)
    }
}

/// clippy's `make_unop`.
pub(crate) fn make_unop(op: &str, expr: Sugg) -> Sugg {
    Sugg::MaybeParen(format!("{op}{}", expr.maybe_inner_paren()))
}

impl std::ops::Not for Sugg {
    type Output = Sugg;

    fn not(self) -> Sugg {
        match self {
            Sugg::BinOp(Op::Binary(op), lhs, rhs) => {
                let to = match op.as_str() {
                    "==" => "!=",
                    "!=" => "==",
                    "<" => ">=",
                    ">=" => "<",
                    ">" => "<=",
                    "<=" => ">",
                    _ => return make_unop("!", Sugg::BinOp(Op::Binary(op), lhs, rhs)),
                };
                Sugg::BinOp(Op::Binary(to.to_owned()), lhs, rhs)
            }
            Sugg::UnOp(UnOp::Not, expr) => *expr,
            other => make_unop("!", other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ast::AstNode as _;

    fn sugg(text: &str) -> Sugg {
        let parse =
            syntax::SourceFile::parse(&format!("fn f() {{ {text}; }}"), span::Edition::CURRENT);
        let expr = parse
            .root()
            .descendants()
            .find_map(ast::ExprStmt::cast)
            .and_then(|it| it.expr())
            .unwrap();
        Sugg::of(expr, &|it| Cow::Borrowed(it.text()), &|_| true)
    }

    #[test]
    fn as_clippy_writes_them() {
        assert_eq!((!sugg("a == b")).to_string(), "a != b");
        assert_eq!((!sugg("a && b")).to_string(), "!(a && b)");
        assert_eq!((!sugg("x")).to_string(), "!x");
        assert_eq!((!sugg("!x")).to_string(), "x");
        assert_eq!((!sugg("x.y()").maybe_paren()).to_string(), "!x.y()");
        assert_eq!((!!sugg("x").maybe_paren()).to_string(), "!!x");
        assert_eq!(sugg("a as u8").maybe_paren().to_string(), "(a as u8)");
        assert!(has_enclosing_paren("(a)"));
        assert!(!has_enclosing_paren("(a)(b)"));
    }
}
