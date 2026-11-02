//! The hand-written accessors of expression views, `ast/expr_ext.rs` on views: shallow questions
//! about an expression's own syntax.

use crate::{
    SyntaxKind::{self, *},
    T, match_ast,
    view::{
        self as ast, ArithOp, AstChildren, AstNode, AstToken, BinaryOp, CmpOp, HasArgList,
        HasAttrs, LogicOp, Node, Ordering, RangeItem, RangeOp, Token, UnaryOp, support,
    },
};

impl<'t> HasAttrs<'t> for ast::Expr<'t> {}

impl<'t> ast::Expr<'t> {
    pub fn is_block_like(self) -> bool {
        matches!(
            self,
            ast::Expr::IfExpr(_)
                | ast::Expr::LoopExpr(_)
                | ast::Expr::ForExpr(_)
                | ast::Expr::WhileExpr(_)
                | ast::Expr::BlockExpr(_)
                | ast::Expr::MatchExpr(_)
        )
    }

    /// The expression inside every pair of parentheses around `self` (rustc's `peel_parens`:
    /// HIR has no parentheses); `self` when it is not parenthesized, or a `()` holds nothing.
    pub fn peel_parens(self) -> ast::Expr<'t> {
        let mut expr = self;
        while let ast::Expr::ParenExpr(paren) = expr
            && let Some(inner) = paren.expr()
        {
            expr = inner;
        }
        expr
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElseBranch<'t> {
    Block(ast::BlockExpr<'t>),
    IfExpr(ast::IfExpr<'t>),
}

impl<'t> From<ast::BlockExpr<'t>> for ElseBranch<'t> {
    fn from(block_expr: ast::BlockExpr<'t>) -> Self {
        Self::Block(block_expr)
    }
}

impl<'t> From<ast::IfExpr<'t>> for ElseBranch<'t> {
    fn from(if_expr: ast::IfExpr<'t>) -> Self {
        Self::IfExpr(if_expr)
    }
}

impl<'t> AstNode<'t> for ElseBranch<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        ast::BlockExpr::can_cast(kind) || ast::IfExpr::can_cast(kind)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        match_ast!(match syntax {
            ast::BlockExpr(it) => Some(Self::Block(it)),
            ast::IfExpr(it) => Some(Self::IfExpr(it)),
            _ => None,
        })
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            ElseBranch::Block(block_expr) => block_expr.syntax,
            ElseBranch::IfExpr(if_expr) => if_expr.syntax,
        }
    }
}

impl<'t> ast::IfExpr<'t> {
    pub fn condition(self) -> Option<ast::Expr<'t>> {
        // If the condition is a BlockExpr, check if the then body is missing.
        // If it is assume the condition is the expression that is missing instead.
        let mut exprs = support::children(self.syntax);
        let first = exprs.next();
        match first {
            Some(ast::Expr::BlockExpr(_)) => exprs.next().and(first),
            first => first,
        }
    }

    pub fn then_branch(self) -> Option<ast::BlockExpr<'t>> {
        match support::children(self.syntax).nth(1)? {
            ast::Expr::BlockExpr(block) => Some(block),
            _ => None,
        }
    }

    pub fn else_branch(self) -> Option<ElseBranch<'t>> {
        match support::children(self.syntax).nth(2)? {
            ast::Expr::BlockExpr(block) => Some(ElseBranch::Block(block)),
            ast::Expr::IfExpr(elif) => Some(ElseBranch::IfExpr(elif)),
            _ => None,
        }
    }
}

impl<'t> ast::PrefixExpr<'t> {
    pub fn op_kind(self) -> Option<UnaryOp> {
        let res = match self.op_token()?.kind() {
            T![*] => UnaryOp::Deref,
            T![!] => UnaryOp::Not,
            T![-] => UnaryOp::Neg,
            _ => return None,
        };
        Some(res)
    }

    pub fn op_token(self) -> Option<Token<'t>> {
        self.syntax
            .children_with_tokens()
            .find(|e| !ast::AnyAttr::can_cast(e.kind()) && !e.kind().is_trivia())?
            .into_token()
    }
}

impl<'t> ast::BinExpr<'t> {
    pub fn op_details(self) -> Option<(Token<'t>, BinaryOp)> {
        self.syntax.children_with_tokens().filter_map(|it| it.into_token()).find_map(|c| {
            #[rustfmt::skip]
            let bin_op = match c.kind() {
                T![||] => BinaryOp::LogicOp(LogicOp::Or),
                T![&&] => BinaryOp::LogicOp(LogicOp::And),

                T![==] => BinaryOp::CmpOp(CmpOp::Eq { negated: false }),
                T![!=] => BinaryOp::CmpOp(CmpOp::Eq { negated: true }),
                T![<=] => BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less,    strict: false }),
                T![>=] => BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: false }),
                T![<]  => BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Less,    strict: true }),
                T![>]  => BinaryOp::CmpOp(CmpOp::Ord { ordering: Ordering::Greater, strict: true }),

                T![+]  => BinaryOp::ArithOp(ArithOp::Add),
                T![*]  => BinaryOp::ArithOp(ArithOp::Mul),
                T![-]  => BinaryOp::ArithOp(ArithOp::Sub),
                T![/]  => BinaryOp::ArithOp(ArithOp::Div),
                T![%]  => BinaryOp::ArithOp(ArithOp::Rem),
                T![<<] => BinaryOp::ArithOp(ArithOp::Shl),
                T![>>] => BinaryOp::ArithOp(ArithOp::Shr),
                T![^]  => BinaryOp::ArithOp(ArithOp::BitXor),
                T![|]  => BinaryOp::ArithOp(ArithOp::BitOr),
                T![&]  => BinaryOp::ArithOp(ArithOp::BitAnd),

                T![=]   => BinaryOp::Assignment { op: None },
                T![+=]  => BinaryOp::Assignment { op: Some(ArithOp::Add) },
                T![*=]  => BinaryOp::Assignment { op: Some(ArithOp::Mul) },
                T![-=]  => BinaryOp::Assignment { op: Some(ArithOp::Sub) },
                T![/=]  => BinaryOp::Assignment { op: Some(ArithOp::Div) },
                T![%=]  => BinaryOp::Assignment { op: Some(ArithOp::Rem) },
                T![<<=] => BinaryOp::Assignment { op: Some(ArithOp::Shl) },
                T![>>=] => BinaryOp::Assignment { op: Some(ArithOp::Shr) },
                T![^=]  => BinaryOp::Assignment { op: Some(ArithOp::BitXor) },
                T![|=]  => BinaryOp::Assignment { op: Some(ArithOp::BitOr) },
                T![&=]  => BinaryOp::Assignment { op: Some(ArithOp::BitAnd) },

                _ => return None,
            };
            Some((c, bin_op))
        })
    }

    pub fn op_kind(self) -> Option<BinaryOp> {
        self.op_details().map(|t| t.1)
    }

    pub fn op_token(self) -> Option<Token<'t>> {
        self.op_details().map(|t| t.0)
    }

    pub fn lhs(self) -> Option<ast::Expr<'t>> {
        support::children(self.syntax).next()
    }

    pub fn rhs(self) -> Option<ast::Expr<'t>> {
        support::children(self.syntax).nth(1)
    }

    pub fn sub_exprs(self) -> (Option<ast::Expr<'t>>, Option<ast::Expr<'t>>) {
        let mut children = support::children(self.syntax);
        let first = children.next();
        let second = children.next();
        (first, second)
    }
}

impl<'t> ast::RangeExpr<'t> {
    fn op_details(self) -> Option<(usize, Token<'t>, RangeOp)> {
        self.syntax.children_with_tokens().enumerate().find_map(|(ix, child)| {
            let token = child.into_token()?;
            let bin_op = match token.kind() {
                T![..] => RangeOp::Exclusive,
                T![..=] => RangeOp::Inclusive,
                _ => return None,
            };
            Some((ix, token, bin_op))
        })
    }

    pub fn is_range_full(self) -> bool {
        support::child::<ast::Expr<'t>>(self.syntax).is_none()
    }
}

impl<'t> RangeItem<'t> for ast::RangeExpr<'t> {
    type Bound = ast::Expr<'t>;

    fn start(&self) -> Option<ast::Expr<'t>> {
        let op_ix = self.op_details()?.0;
        self.syntax
            .children_with_tokens()
            .take(op_ix)
            .find_map(|it| ast::Expr::cast(it.into_node()?))
    }

    fn end(&self) -> Option<ast::Expr<'t>> {
        let op_ix = self.op_details()?.0;
        self.syntax
            .children_with_tokens()
            .skip(op_ix + 1)
            .find_map(|it| ast::Expr::cast(it.into_node()?))
    }

    fn op_token(&self) -> Option<Token<'t>> {
        self.op_details().map(|t| t.1)
    }

    fn op_kind(&self) -> Option<RangeOp> {
        self.op_details().map(|t| t.2)
    }
}

impl<'t> ast::IndexExpr<'t> {
    pub fn base(self) -> Option<ast::Expr<'t>> {
        support::children(self.syntax).next()
    }

    pub fn index(self) -> Option<ast::Expr<'t>> {
        support::children(self.syntax).nth(1)
    }
}

impl<'t> ast::UnsafeBinderCastExpr<'t> {
    /// Whether it is `builtin # wrap_binder(..)`, not `builtin # unwrap_binder(..)`.
    pub fn is_wrap(self) -> bool {
        self.wrap_binder_token().is_some()
    }
}

#[derive(Clone)]
pub enum ArrayExprKind<'t> {
    Repeat { initializer: Option<ast::Expr<'t>>, repeat: Option<ast::Expr<'t>> },
    ElementList(AstChildren<'t, ast::Expr<'t>>),
}

impl<'t> ast::ArrayExpr<'t> {
    pub fn kind(self) -> ArrayExprKind<'t> {
        if self.is_repeat() {
            ArrayExprKind::Repeat {
                initializer: support::children(self.syntax).next(),
                repeat: support::children(self.syntax).nth(1),
            }
        } else {
            ArrayExprKind::ElementList(support::children(self.syntax))
        }
    }

    fn is_repeat(self) -> bool {
        self.semicolon_token().is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiteralKind<'t> {
    String(ast::String<'t>),
    ByteString(ast::ByteString<'t>),
    CString(ast::CString<'t>),
    IntNumber(ast::IntNumber<'t>),
    FloatNumber(ast::FloatNumber<'t>),
    Char(ast::Char<'t>),
    Byte(ast::Byte<'t>),
    Bool(bool),
}

impl<'t> ast::Literal<'t> {
    pub fn token(self) -> Token<'t> {
        self.syntax
            .children_with_tokens()
            .find(|e| !ast::AnyAttr::can_cast(e.kind()) && !e.kind().is_trivia())
            .and_then(|e| e.into_token())
            .expect("a literal holds its token")
    }

    /// # Panics
    ///
    /// When the literal's token is not a literal, which the parser never makes.
    pub fn kind(self) -> LiteralKind<'t> {
        let token = self.token();

        match_ast!(match token {
            ast::IntNumber(t) => LiteralKind::IntNumber(t),
            ast::FloatNumber(t) => LiteralKind::FloatNumber(t),
            ast::String(t) => LiteralKind::String(t),
            ast::ByteString(t) => LiteralKind::ByteString(t),
            ast::CString(t) => LiteralKind::CString(t),
            ast::Char(t) => LiteralKind::Char(t),
            ast::Byte(t) => LiteralKind::Byte(t),
            _ => match token.kind() {
                T![true] => LiteralKind::Bool(true),
                T![false] => LiteralKind::Bool(false),
                kind => panic!("a literal at {:?} holds a {kind:?}", token.text_range()),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockModifier<'t> {
    Async(Token<'t>),
    Unsafe(Token<'t>),
    Try { try_token: Token<'t>, bikeshed_token: Option<Token<'t>>, result_type: Option<ast::Type<'t>> },
    Const(Token<'t>),
    AsyncGen(Token<'t>),
    Gen(Token<'t>),
    Label(ast::Label<'t>),
}

impl<'t> ast::BlockExpr<'t> {
    pub fn modifier(self) -> Option<BlockModifier<'t>> {
        self.gen_token()
            .map(|v| {
                if self.async_token().is_some() {
                    BlockModifier::AsyncGen(v)
                } else {
                    BlockModifier::Gen(v)
                }
            })
            .or_else(|| self.async_token().map(BlockModifier::Async))
            .or_else(|| self.unsafe_token().map(BlockModifier::Unsafe))
            .or_else(|| {
                let modifier = self.try_block_modifier()?;
                let try_token = modifier.try_token()?;
                let bikeshed_token = modifier.bikeshed_token();
                let result_type = modifier.ty();
                Some(BlockModifier::Try { try_token, bikeshed_token, result_type })
            })
            .or_else(|| self.const_token().map(BlockModifier::Const))
            .or_else(|| self.label().map(BlockModifier::Label))
    }

    /// false if the block is an intrinsic part of the syntax and can't be
    /// replaced with arbitrary expression.
    ///
    /// ```not_rust
    /// fn foo() { not_stand_alone }
    /// const FOO: () = { stand_alone };
    /// ```
    pub fn is_standalone(self) -> bool {
        let parent = match self.syntax.parent() {
            Some(it) => it,
            None => return true,
        };
        match parent.kind() {
            FOR_EXPR | IF_EXPR => parent
                .children()
                .find(|it| ast::Expr::can_cast(it.kind()))
                .is_none_or(|it| it == self.syntax),
            LET_ELSE | FN | WHILE_EXPR | LOOP_EXPR | CONST_BLOCK_PAT => false,
            _ => true,
        }
    }
}

impl<'t> ast::RecordExprField<'t> {
    pub fn parent_record_lit(self) -> ast::RecordExpr<'t> {
        self.syntax
            .ancestors()
            .find_map(ast::RecordExpr::cast)
            .expect("a RecordExprField is always nested in a RecordExpr")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallableExpr<'t> {
    Call(ast::CallExpr<'t>),
    MethodCall(ast::MethodCallExpr<'t>),
}

impl<'t> HasAttrs<'t> for CallableExpr<'t> {}
impl<'t> HasArgList<'t> for CallableExpr<'t> {}

impl<'t> AstNode<'t> for CallableExpr<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        ast::CallExpr::can_cast(kind) || ast::MethodCallExpr::can_cast(kind)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        if let Some(it) = ast::CallExpr::cast(syntax) {
            Some(Self::Call(it))
        } else {
            ast::MethodCallExpr::cast(syntax).map(Self::MethodCall)
        }
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            Self::Call(it) => it.syntax,
            Self::MethodCall(it) => it.syntax,
        }
    }
}

impl<'t> ast::MacroDef<'t> {
    fn tts(self) -> (Option<ast::TokenTree<'t>>, Option<ast::TokenTree<'t>>) {
        let mut types = support::children(self.syntax);
        let first = types.next();
        let second = types.next();
        (first, second)
    }

    pub fn args(self) -> Option<ast::TokenTree<'t>> {
        match self.tts() {
            (Some(args), Some(_)) => Some(args),
            _ => None,
        }
    }

    pub fn body(self) -> Option<ast::TokenTree<'t>> {
        match self.tts() {
            (Some(body), None) | (_, Some(body)) => Some(body),
            _ => None,
        }
    }
}

impl<'t> ast::ClosureExpr<'t> {
    #[inline]
    pub fn body(self) -> Option<ast::Expr<'t>> {
        support::child(self.syntax)
    }
}

impl<'t> ast::Const<'t> {
    #[inline]
    pub fn body(self) -> Option<ast::Expr<'t>> {
        support::child(self.syntax)
    }
}

impl<'t> ast::Fn<'t> {
    #[inline]
    pub fn body(self) -> Option<ast::BlockExpr<'t>> {
        support::child(self.syntax)
    }
}

impl<'t> ast::Static<'t> {
    #[inline]
    pub fn body(self) -> Option<ast::Expr<'t>> {
        support::child(self.syntax)
    }
}

impl<'t> ast::FormatArgsExpr<'t> {
    #[inline]
    pub fn args(self) -> AstChildren<'t, ast::FormatArgsArg<'t>> {
        support::children(self.syntax)
    }
}

impl<'t> ast::ArgList<'t> {
    #[inline]
    pub fn args(self) -> AstChildren<'t, ast::Expr<'t>> {
        support::children(self.syntax)
    }
}
