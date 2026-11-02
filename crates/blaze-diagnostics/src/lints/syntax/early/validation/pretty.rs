//! rustc's pretty printing of a `where` predicate, as `deprecated_where_clause_location`'s
//! suggestion prints the predicates it moves: the types, bounds, paths and generic arguments in
//! them, with rustc's spacing. Lines are never broken: rustc's printer breaks only past 78
//! columns, which no predicate of a type alias's header reaches.
//!
//! Follows rustc's `compiler/rustc_ast_pretty/src/pprust/state.rs` (`print_type`,
//! `print_type_bounds`, `print_poly_trait_ref`, `print_path`, `print_qpath`,
//! `print_generic_args`, `print_assoc_item_constraint`, `print_generic_params`,
//! `print_fn_ret_ty`, `print_param`) and `state/item.rs` (`print_where_predicate`,
//! `print_where_bound_predicate`, `print_fn`), and the expressions of `state/expr.rs` a type
//! holds (an array's length, a const argument) (MIT or Apache-2.0).

use span::Edition;
use syntax::{
    NodeOrToken, SyntaxKind,
    utils::is_raw_identifier,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _, HasTypeBounds as _},
};

/// rustc's printing of `pred`, in `edition` (which decides whether a raw identifier keeps its
/// `r#`), appended to `out`.
pub(crate) fn where_predicate(out: &mut String, pred: ast::WherePred<'_>, edition: Edition) {
    Printer { out, edition }.where_predicate(pred);
}

/// rustc's `ty_to_string` of `ty`, appended to `out`.
pub(crate) fn ty(out: &mut String, ty: ast::Type<'_>, edition: Edition) {
    Printer { out, edition }.ty(ty);
}

/// rustc's `bounds_to_string` of `list`: the bounds joined with ` + `, appended to `out`.
pub(crate) fn bounds(out: &mut String, list: ast::TypeBoundList<'_>, edition: Edition) {
    Printer { out, edition }.type_bounds("", Some(list));
}

/// rustc's `print_generic_arg` of `arg` (`print_assoc_item_constraint` of a constraint),
/// appended to `out`.
pub(crate) fn generic_arg(out: &mut String, arg: ast::GenericArg<'_>, edition: Edition) {
    let mut printer = Printer { out, edition };
    match arg {
        ast::GenericArg::TypeArg(it) => printer.opt_ty(it.ty()),
        ast::GenericArg::LifetimeArg(it) => {
            if let Some(lifetime) = it.lifetime() {
                printer.lifetime(lifetime);
            }
        }
        ast::GenericArg::ConstArg(it) => printer.const_arg(it),
        ast::GenericArg::AssocTypeArg(it) => printer.assoc_item_constraint(it),
    }
}

/// rustc's `expr_to_string` of the expressions a type holds, appended to `out`.
pub(crate) fn expr(out: &mut String, expr: ast::Expr<'_>, edition: Edition) {
    Printer { out, edition }.expr(expr);
}

/// rustc's printing of an identifier written `text` (`Ident`'s `Display`), appended to `out`:
/// with `r#` when it is a keyword of `edition`, else without, however it is written.
pub(crate) fn push_ident(out: &mut String, text: &str, edition: Edition) {
    let bare = text.strip_prefix("r#").unwrap_or(text);
    if is_raw_identifier(bare, edition) {
        out.push_str("r#");
    }
    out.push_str(bare);
}

/// Writes into the suggestion's one buffer: nothing is allocated per node printed.
struct Printer<'a> {
    out: &'a mut String,
    edition: Edition,
}

impl Printer<'_> {
    fn word(&mut self, text: &str) {
        self.out.push_str(text);
    }

    fn where_predicate(&mut self, pred: ast::WherePred<'_>) {
        self.outer_attrs(pred.syntax());
        if let Some(binder) = pred.for_binder() {
            self.formal_generic_params(binder);
        }
        match (pred.lifetime(), pred.ty()) {
            (Some(lifetime), _) => {
                self.lifetime(lifetime);
                self.word(":");
                self.lifetime_bounds(" ", pred.type_bound_list());
            }
            (None, Some(ty)) => {
                self.ty(ty);
                self.word(":");
                self.type_bounds(" ", pred.type_bound_list());
            }
            (None, None) => self.source(pred.syntax()),
        }
    }

    /// `print_outer_attributes`: each attribute as written, then a space.
    fn outer_attrs(&mut self, node: ast::Node<'_>) {
        for attr in node.children().filter(|it| it.kind() == SyntaxKind::ATTR) {
            self.source(attr);
            self.word(" ");
        }
    }

    fn ident(&mut self, text: &str) {
        push_ident(self.out, text, self.edition);
    }

    fn lifetime(&mut self, lifetime: ast::Lifetime<'_>) {
        self.word(lifetime.text());
    }

    /// The bounds of `list` joined with ` + `, after `prefix` when there is one.
    fn lifetime_bounds(&mut self, prefix: &str, list: Option<ast::TypeBoundList<'_>>) {
        let Some(list) = list else { return };
        for (i, bound) in list.bounds().enumerate() {
            self.word(if i == 0 { prefix } else { " + " });
            match bound.lifetime() {
                Some(lifetime) => self.lifetime(lifetime),
                None => self.source(bound.syntax()),
            }
        }
    }

    /// The bounds of `list` joined with ` + `, after `prefix` when there is one.
    fn type_bounds(&mut self, prefix: &str, list: Option<ast::TypeBoundList<'_>>) {
        let Some(list) = list else { return };
        for (i, bound) in list.bounds().enumerate() {
            self.word(if i == 0 { prefix } else { " + " });
            self.type_bound(bound);
        }
    }

    fn type_bound(&mut self, bound: ast::TypeBound<'_>) {
        if let Some(lifetime) = bound.lifetime() {
            self.lifetime(lifetime);
            return;
        }
        if let Some(args) = bound.use_bound_generic_args() {
            self.word("use<");
            for (i, arg) in args.use_bound_generic_args().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                match arg {
                    ast::UseBoundGenericArg::Lifetime(it) => self.lifetime(it),
                    ast::UseBoundGenericArg::NameRef(it) => self.ident(it.text()),
                }
            }
            self.word(">");
            return;
        }
        if let Some(binder) = bound.for_binder() {
            self.formal_generic_params(binder);
        }
        if bound.l_brack_token().is_some() || bound.tilde_token().is_some() {
            self.word("[const] ");
        } else if bound.const_token().is_some() {
            self.word("const ");
        }
        if bound.async_token().is_some() {
            self.word("async ");
        }
        if bound.question_mark_token().is_some() {
            self.word("?");
        }
        match bound.ty() {
            Some(ty) => self.ty(ty),
            None => self.source(bound.syntax()),
        }
    }

    /// `print_formal_generic_params`: `for<..> `.
    fn formal_generic_params(&mut self, binder: ast::ForBinder<'_>) {
        let Some(list) = binder.generic_param_list() else { return };
        let mut params = list.generic_params().peekable();
        if params.peek().is_none() {
            return;
        }
        self.word("for");
        self.generic_params(params);
        self.word(" ");
    }

    fn generic_params<'t>(&mut self, params: impl Iterator<Item = ast::GenericParam<'t>>) {
        self.word("<");
        for (i, param) in params.enumerate() {
            if i != 0 {
                self.word(", ");
            }
            self.outer_attrs(param.syntax());
            match param {
                ast::GenericParam::LifetimeParam(it) => {
                    if let Some(lifetime) = it.lifetime() {
                        self.lifetime(lifetime);
                    }
                    self.lifetime_bounds(": ", it.type_bound_list());
                }
                ast::GenericParam::TypeParam(it) => {
                    if let Some(name) = it.name() {
                        self.ident(name.text());
                    }
                    self.type_bounds(": ", it.type_bound_list());
                    if let Some(default) = it.default_type() {
                        self.word(" = ");
                        self.ty(default);
                    }
                }
                ast::GenericParam::ConstParam(it) => {
                    self.word("const ");
                    if let Some(name) = it.name() {
                        self.ident(name.text());
                    }
                    self.word(" : ");
                    if let Some(ty) = it.ty() {
                        self.ty(ty);
                    }
                    if let Some(default) = it.default_val() {
                        self.word(" = ");
                        self.const_arg(default);
                    }
                }
            }
        }
        self.word(">");
    }

    fn ty(&mut self, ty: ast::Type<'_>) {
        match ty {
            ast::Type::SliceType(it) => {
                self.word("[");
                self.opt_ty(it.ty());
                self.word("]");
            }
            ast::Type::PtrType(it) => {
                self.word("*");
                self.word(if it.mut_token().is_some() { "mut " } else { "const " });
                self.opt_ty(it.ty());
            }
            ast::Type::RefType(it) => {
                self.word("&");
                if let Some(lifetime) = it.lifetime() {
                    self.lifetime(lifetime);
                    self.word(" ");
                }
                if it.mut_token().is_some() {
                    self.word("mut ");
                }
                self.opt_ty(it.ty());
            }
            ast::Type::NeverType(_) => self.word("!"),
            ast::Type::TupleType(it) => {
                self.word("(");
                let mut count = 0;
                for field in it.fields() {
                    if count != 0 {
                        self.word(", ");
                    }
                    self.ty(field);
                    count += 1;
                }
                if count == 1 {
                    self.word(",");
                }
                self.word(")");
            }
            ast::Type::ParenType(it) => {
                self.word("(");
                self.opt_ty(it.ty());
                self.word(")");
            }
            ast::Type::FnPtrType(it) => self.fn_ptr(it),
            ast::Type::ForType(it) => {
                if let Some(binder) = it.for_binder() {
                    self.formal_generic_params(binder);
                }
                self.opt_ty(it.ty());
            }
            ast::Type::PathType(it) => match it.path() {
                Some(path) => self.path(path, false),
                None => self.source(it.syntax()),
            },
            ast::Type::DynTraitType(it) => {
                if it.dyn_token().is_some() {
                    self.word("dyn ");
                }
                self.bound_list(it.type_bound_list());
            }
            ast::Type::ImplTraitType(it) => {
                self.word("impl ");
                self.bound_list(it.type_bound_list());
            }
            ast::Type::ArrayType(it) => {
                self.word("[");
                self.opt_ty(it.ty());
                self.word("; ");
                if let Some(len) = it.const_arg() {
                    self.const_arg(len);
                }
                self.word("]");
            }
            ast::Type::InferType(_) => self.word("_"),
            ast::Type::PatternType(it) => {
                self.opt_ty(it.ty());
                self.word(" is ");
                if let Some(pat) = it.pat() {
                    self.source(pat.syntax());
                }
            }
            ast::Type::MacroType(it) => self.source(it.syntax()),
            ast::Type::UnsafeBinderType(it) => {
                self.word("unsafe");
                if let Some(list) = it.generic_param_list() {
                    self.generic_params(list.generic_params());
                }
                self.word(" ");
                self.opt_ty(it.ty());
            }
        }
    }

    fn opt_ty(&mut self, ty: Option<ast::Type<'_>>) {
        if let Some(ty) = ty {
            self.ty(ty);
        }
    }

    fn bound_list(&mut self, list: Option<ast::TypeBoundList<'_>>) {
        self.type_bounds("", list);
    }

    /// `print_ty_fn` of a function pointer: `unsafe extern "C" fn(u8) -> u8`.
    fn fn_ptr(&mut self, it: ast::FnPtrType<'_>) {
        if it.unsafe_token().is_some() {
            self.word("unsafe ");
        }
        if let Some(abi) = it.abi() {
            self.word("extern ");
            if let Some(string) = abi.string_token() {
                self.word(string.text());
                self.word(" ");
            }
        }
        self.word("fn(");
        if let Some(params) = it.param_list() {
            for (i, param) in params.params().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                self.param(param);
            }
        }
        self.word(")");
        self.ret_type(it.ret_type());
    }

    fn param(&mut self, param: ast::Param<'_>) {
        self.outer_attrs(param.syntax());
        if let Some(pat) = param.pat() {
            self.source(pat.syntax());
            self.word(": ");
        }
        match (param.ty(), param.dotdotdot_token()) {
            (Some(ty), _) => self.ty(ty),
            (None, Some(_)) => self.word("..."),
            (None, None) => {}
        }
    }

    fn ret_type(&mut self, ret: Option<ast::RetType<'_>>) {
        if let Some(ty) = ret.and_then(|it| it.ty()) {
            self.word(" -> ");
            self.ty(ty);
        }
    }

    /// `print_path`, or `print_qpath` when its first segment is `<T as Trait>`;
    /// `colons_before_params` prints `::<..>` (in an expression) rather than `<..>`.
    fn path(&mut self, path: ast::Path<'_>, colons_before_params: bool) {
        for (i, segment) in path.segments().enumerate() {
            if let Some(ast::PathSegmentKind::Type { type_ref, trait_ref }) = segment.kind() {
                self.word("<");
                self.opt_ty(type_ref);
                if let Some(trait_) = trait_ref.and_then(|it| it.path()) {
                    self.word(" as ");
                    self.path(trait_, false);
                }
                self.word(">");
                continue;
            }
            if i > 0 || segment.coloncolon_token().is_some() {
                self.word("::");
            }
            match segment.name_ref() {
                Some(name) => self.ident(name.text()),
                None => self.source(segment.syntax()),
            }
            if let Some(args) = segment.generic_arg_list() {
                self.generic_arg_list(args, colons_before_params);
            } else if let Some(args) = segment.parenthesized_arg_list() {
                if colons_before_params {
                    self.word("::");
                }
                self.word("(");
                for (i, arg) in args.type_args().enumerate() {
                    if i != 0 {
                        self.word(", ");
                    }
                    self.opt_ty(arg.ty());
                }
                self.word(")");
                self.ret_type(segment.ret_type());
            } else if segment.return_type_syntax().is_some() {
                if colons_before_params {
                    self.word("::");
                }
                self.word("(..)");
            }
        }
    }

    fn generic_arg_list(&mut self, args: ast::GenericArgList<'_>, colons_before_params: bool) {
        if colons_before_params {
            self.word("::");
        }
        self.word("<");
        for (i, arg) in args.generic_args().enumerate() {
            if i != 0 {
                self.word(", ");
            }
            match arg {
                ast::GenericArg::TypeArg(it) => self.opt_ty(it.ty()),
                ast::GenericArg::LifetimeArg(it) => {
                    if let Some(lifetime) = it.lifetime() {
                        self.lifetime(lifetime);
                    }
                }
                ast::GenericArg::ConstArg(it) => self.const_arg(it),
                ast::GenericArg::AssocTypeArg(it) => self.assoc_item_constraint(it),
            }
        }
        self.word(">");
    }

    /// `print_assoc_item_constraint`: `Item = u8`, and `Item : Copy` with the space rustc
    /// puts before the colon.
    fn assoc_item_constraint(&mut self, it: ast::AssocTypeArg<'_>) {
        if let Some(name) = it.name_ref() {
            self.ident(name.text());
        }
        if let Some(args) = it.generic_arg_list() {
            self.generic_arg_list(args, false);
        } else if it.return_type_syntax().is_some() {
            self.word("(..)");
        } else if let Some(params) = it.param_list() {
            self.word("(");
            for (i, param) in params.params().enumerate() {
                if i != 0 {
                    self.word(", ");
                }
                self.param(param);
            }
            self.word(")");
            self.ret_type(it.ret_type());
        }
        self.word(" ");
        if it.eq_token().is_some() {
            self.word("= ");
            match (it.ty(), it.const_arg()) {
                (Some(ty), _) => self.ty(ty),
                (None, Some(ct)) => self.const_arg(ct),
                (None, None) => {}
            }
        } else {
            self.type_bounds(": ", it.type_bound_list());
        }
    }

    fn const_arg(&mut self, arg: ast::ConstArg<'_>) {
        match arg.expr() {
            Some(expr) => self.expr(expr),
            None => self.source(arg.syntax()),
        }
    }

    /// `print_expr` of the expressions a type holds: literals, paths, blocks of one
    /// expression, parentheses, unary and binary operators; any other expression is printed
    /// as written, its white space collapsed.
    fn expr(&mut self, expr: ast::Expr<'_>) {
        match expr {
            ast::Expr::Literal(it) => self.source(it.syntax()),
            ast::Expr::PathExpr(it) => match it.path() {
                Some(path) => self.path(path, true),
                None => self.source(it.syntax()),
            },
            ast::Expr::UnderscoreExpr(_) => self.word("_"),
            ast::Expr::ParenExpr(it) => {
                self.word("(");
                if let Some(inner) = it.expr() {
                    self.expr(inner);
                }
                self.word(")");
            }
            ast::Expr::PrefixExpr(it) => match (it.op_token(), it.expr()) {
                (Some(op), Some(inner)) => {
                    self.word(op.text());
                    self.expr(inner);
                }
                _ => self.source(it.syntax()),
            },
            ast::Expr::BinExpr(it) => match (it.lhs(), it.op_token(), it.rhs()) {
                (Some(lhs), Some(op), Some(rhs)) => {
                    self.expr(lhs);
                    self.word(" ");
                    self.word(op.text());
                    self.word(" ");
                    self.expr(rhs);
                }
                _ => self.source(it.syntax()),
            },
            ast::Expr::BlockExpr(it)
                if it.modifier().is_none() && it.statements().next().is_none() =>
            {
                match it.tail_expr() {
                    Some(tail) => {
                        self.word("{ ");
                        self.expr(tail);
                        self.word(" }");
                    }
                    None => self.word("{}"),
                }
            }
            _ => self.source(expr.syntax()),
        }
    }

    /// `node` as written, without its comments, each run of white space one space.
    fn source(&mut self, node: ast::Node<'_>) {
        let mut space = false;
        for token in node.descendants_with_tokens().filter_map(NodeOrToken::into_token) {
            match token.kind() {
                SyntaxKind::WHITESPACE => space = true,
                SyntaxKind::COMMENT => {}
                _ => {
                    if space && !self.out.is_empty() && !self.out.ends_with(' ') {
                        self.word(" ");
                    }
                    space = false;
                    self.word(token.text());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use syntax::SourceFile;

    use super::*;

    fn printed(predicate: &str) -> String {
        let text = format!("fn f() where {predicate} {{}}");
        let parse = SourceFile::parse(&text, Edition::Edition2021);
        let pred = parse.root().descendants().find_map(ast::WherePred::cast).unwrap();
        let mut out = String::new();
        where_predicate(&mut out, pred, Edition::Edition2021);
        out
    }

    #[test]
    fn an_identifier_is_printed_raw_where_a_keyword() {
        let mut out = String::new();
        push_ident(&mut out, "r#async", Edition::Edition2018);
        out.push(' ');
        push_ident(&mut out, "r#async", Edition::Edition2015);
        out.push(' ');
        push_ident(&mut out, "r#foo", Edition::Edition2021);
        assert_eq!(out, "r#async async foo");
    }

    #[test]
    fn printing_allocates_only_the_buffers_growth() {
        let text = "fn f() where for<'a> &'a T: Iterator<Item = [u8; N + 1]> + ?Sized + 'static {}";
        let parse = SourceFile::parse(text, Edition::Edition2021);
        let pred = parse.root().descendants().find_map(ast::WherePred::cast).unwrap();
        let mut out = String::with_capacity(256);
        blaze_alloc_count::assert_at_most(0, "a where predicate printed into a buffer with room", || {
            where_predicate(&mut out, pred, Edition::Edition2021)
        });
        assert_eq!(out, "for<'a> &'a T: Iterator<Item = [u8; N + 1]> + ?Sized + 'static");
    }

    #[test]
    fn a_predicate_is_printed_with_rustcs_spacing() {
        assert_eq!(printed("u32:Copy"), "u32: Copy");
        assert_eq!(printed("T :  Clone+'static"), "T: Clone + 'static");
        assert_eq!(printed("'a:'b+'c"), "'a: 'b + 'c");
        assert_eq!(printed("for<'a> &'a T: ?Sized"), "for<'a> &'a T: ?Sized");
        assert_eq!(printed("Vec::<T>: Iterator<Item=u8>"), "Vec<T>: Iterator<Item = u8>");
        assert_eq!(printed("T: Iterator<Item: Copy>"), "T: Iterator<Item : Copy>");
        assert_eq!(printed("<T as Tr>::A: Fn(u8)->u8"), "<T as Tr>::A: Fn(u8) -> u8");
        assert_eq!(printed("(u8,): Sized"), "(u8,): Sized");
        assert_eq!(printed("[u8; N+1]: Sized"), "[u8; N + 1]: Sized");
        assert_eq!(printed("*mut dyn Tr: Sized"), "*mut dyn Tr: Sized");
        assert_eq!(printed("T:"), "T:");
    }
}
