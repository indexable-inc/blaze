//! What an attribute is applied to: rustc's `Target`, with the names its diagnostics give it,
//! and the target of each node of the syntax tree that has attributes.
//!
//! Follows rustc's `compiler/rustc_hir/src/target.rs` (`Target`, `name`, `plural_name`,
//! `from_ast_item`, `from_assoc_item_kind`, `from_foreign_item_kind`, `from_expr`) and where
//! `compiler/rustc_ast_lowering` lowers the attributes of each node (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode},
};

use crate::cx::Cx;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum GenericParamKind {
    Type,
    Lifetime,
    Const,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MethodKind {
    /// In a `trait`, with a default body or not.
    Trait { body: bool },
    /// In an `impl Trait for Type`.
    TraitImpl,
    /// In an `impl Type`.
    Inherent,
}

/// What an attribute is applied to: rustc's `Target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    ExternCrate,
    Use,
    Static,
    Const,
    Fn,
    Closure,
    Mod,
    ForeignMod,
    GlobalAsm,
    TyAlias,
    Enum,
    Variant,
    Struct,
    Field,
    Union,
    Trait,
    TraitAlias,
    Impl { of_trait: bool },
    Expression,
    Statement,
    Arm,
    AssocConst,
    Method(MethodKind),
    AssocTy,
    ForeignFn,
    ForeignStatic,
    ForeignTy,
    GenericParam { kind: GenericParamKind, has_default: bool },
    MacroDef,
    Param,
    PatField,
    ExprField,
    WherePredicate,
    MacroCall,
    Crate,
    Delegation { mac: bool },
    ForLoop,
    While,
    Loop,
    Break,
}

impl Target {
    pub(crate) fn is_associated_item(self) -> bool {
        matches!(self, Target::AssocConst | Target::AssocTy | Target::Method(_))
    }

    /// As rustc's diagnostics name one: "function", "trait method in an impl block".
    pub(crate) fn name(self) -> &'static str {
        match self {
            Target::ExternCrate => "extern crate",
            Target::Use => "use",
            Target::Static => "static",
            Target::Const => "constant",
            Target::Fn => "function",
            Target::Closure => "closure",
            Target::Mod => "module",
            Target::ForeignMod => "foreign module",
            Target::GlobalAsm => "global asm",
            Target::TyAlias => "type alias",
            Target::Enum => "enum",
            Target::Variant => "enum variant",
            Target::Struct => "struct",
            Target::Field => "struct field",
            Target::Union => "union",
            Target::Trait => "trait",
            Target::TraitAlias => "trait alias",
            Target::Impl { .. } => "implementation block",
            Target::Expression => "expression",
            Target::Statement => "statement",
            Target::Arm => "match arm",
            Target::AssocConst => "associated const",
            Target::Method(kind) => match kind {
                MethodKind::Inherent => "inherent method",
                MethodKind::Trait { body: false } => "required trait method",
                MethodKind::Trait { body: true } => "provided trait method",
                MethodKind::TraitImpl => "trait method in an impl block",
            },
            Target::AssocTy => "associated type",
            Target::ForeignFn => "foreign function",
            Target::ForeignStatic => "foreign static item",
            Target::ForeignTy => "foreign type",
            Target::GenericParam { kind, .. } => match kind {
                GenericParamKind::Type => "type parameter",
                GenericParamKind::Lifetime => "lifetime parameter",
                GenericParamKind::Const => "const parameter",
            },
            Target::MacroDef => "macro def",
            Target::Param => "function param",
            Target::PatField => "pattern field",
            Target::ExprField => "struct field",
            Target::WherePredicate => "where predicate",
            Target::MacroCall => "macro call",
            Target::Crate => "crate",
            Target::Delegation { .. } => "delegation",
            Target::Loop => "loop",
            Target::ForLoop => "for loop",
            Target::While => "while loop",
            Target::Break => "break expression",
        }
    }

    /// As rustc's diagnostics name several: "functions", "trait impl blocks".
    pub(crate) fn plural_name(self) -> &'static str {
        match self {
            Target::ExternCrate => "extern crates",
            Target::Use => "use statements",
            Target::Static => "statics",
            Target::Const => "constants",
            Target::Fn => "functions",
            Target::Closure => "closures",
            Target::Mod => "modules",
            Target::ForeignMod => "foreign modules",
            Target::GlobalAsm => "global asms",
            Target::TyAlias => "type aliases",
            Target::Enum => "enums",
            Target::Variant => "enum variants",
            Target::Struct => "structs",
            Target::Field => "struct fields",
            Target::Union => "unions",
            Target::Trait => "traits",
            Target::TraitAlias => "trait aliases",
            Target::Impl { of_trait: false } => "inherent impl blocks",
            Target::Impl { of_trait: true } => "trait impl blocks",
            Target::Expression => "expressions",
            Target::Statement => "statements",
            Target::Arm => "match arms",
            Target::AssocConst => "associated consts",
            Target::Method(kind) => match kind {
                MethodKind::Inherent => "inherent methods",
                MethodKind::Trait { body: false } => "required trait methods",
                MethodKind::Trait { body: true } => "provided trait methods",
                MethodKind::TraitImpl => "trait methods in impl blocks",
            },
            Target::AssocTy => "associated types",
            Target::ForeignFn => "foreign functions",
            Target::ForeignStatic => "foreign statics",
            Target::ForeignTy => "foreign types",
            Target::GenericParam { kind, .. } => match kind {
                GenericParamKind::Type => "type parameters",
                GenericParamKind::Lifetime => "lifetime parameters",
                GenericParamKind::Const => "const parameters",
            },
            Target::MacroDef => "macro defs",
            Target::Param => "function params",
            Target::PatField => "pattern fields",
            Target::ExprField => "struct fields",
            Target::WherePredicate => "where predicates",
            Target::MacroCall => "macro calls",
            Target::Crate => "crates",
            Target::Delegation { .. } => "delegations",
            Target::ForLoop => "for loops",
            Target::Loop => "loops",
            Target::While => "while loops",
            Target::Break => "break expressions",
        }
    }

    /// The target of `node`, a node with attributes, as rustc lowers its attributes; `None` for
    /// a node whose attributes are another's (the lists inner attributes are written in, a
    /// function's body) or that rustc gives no attributes. `crate_root` is the crate's root
    /// file.
    pub(crate) fn of(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>, crate_root: HirFileId) -> Option<Target> {
        let syntax = node.value;
        let target = match syntax.kind() {
            SyntaxKind::SOURCE_FILE => match node.file_id == crate_root {
                true => Target::Crate,
                false => Target::Mod,
            },
            SyntaxKind::MODULE => Target::Mod,
            SyntaxKind::EXTERN_CRATE => Target::ExternCrate,
            SyntaxKind::USE => Target::Use,
            SyntaxKind::STATIC => match container(cx, node) {
                Container::Extern => Target::ForeignStatic,
                _ => Target::Static,
            },
            SyntaxKind::CONST => match container(cx, node) {
                Container::Trait | Container::Impl { .. } => Target::AssocConst,
                _ => Target::Const,
            },
            SyntaxKind::FN => {
                let body = ast::Fn::cast(syntax)?.body().is_some();
                match container(cx, node) {
                    Container::Extern => Target::ForeignFn,
                    Container::Trait => Target::Method(MethodKind::Trait { body }),
                    Container::Impl { of_trait: true } => Target::Method(MethodKind::TraitImpl),
                    Container::Impl { of_trait: false } => Target::Method(MethodKind::Inherent),
                    Container::Other => Target::Fn,
                }
            }
            SyntaxKind::TYPE_ALIAS => match container(cx, node) {
                Container::Extern => Target::ForeignTy,
                Container::Trait | Container::Impl { .. } => Target::AssocTy,
                Container::Other => Target::TyAlias,
            },
            SyntaxKind::EXTERN_BLOCK => Target::ForeignMod,
            SyntaxKind::ENUM => Target::Enum,
            SyntaxKind::VARIANT => Target::Variant,
            SyntaxKind::STRUCT => Target::Struct,
            SyntaxKind::UNION => Target::Union,
            SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD => Target::Field,
            SyntaxKind::TRAIT => match ast::Trait::cast(syntax)?.eq_token() {
                Some(_) => Target::TraitAlias,
                None => Target::Trait,
            },
            SyntaxKind::IMPL => Target::Impl { of_trait: ast::Impl::cast(syntax)?.trait_().is_some() },
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => Target::MacroDef,
            SyntaxKind::MACRO_CALL => Target::MacroCall,
            SyntaxKind::TYPE_PARAM => Target::GenericParam {
                kind: GenericParamKind::Type,
                has_default: ast::TypeParam::cast(syntax)?.default_type().is_some(),
            },
            SyntaxKind::LIFETIME_PARAM => {
                Target::GenericParam { kind: GenericParamKind::Lifetime, has_default: false }
            }
            SyntaxKind::CONST_PARAM => Target::GenericParam {
                kind: GenericParamKind::Const,
                has_default: ast::ConstParam::cast(syntax)?.default_val().is_some(),
            },
            SyntaxKind::PARAM | SyntaxKind::SELF_PARAM => Target::Param,
            SyntaxKind::RECORD_PAT_FIELD => Target::PatField,
            SyntaxKind::RECORD_EXPR_FIELD => Target::ExprField,
            SyntaxKind::WHERE_PRED => Target::WherePredicate,
            SyntaxKind::MATCH_ARM => Target::Arm,
            SyntaxKind::LET_STMT => Target::Statement,
            SyntaxKind::BLOCK_EXPR if is_fn_body(syntax) => return None,
            _ => from_expr(ast::Expr::cast(syntax)?),
        };
        Some(target)
    }
}

/// rustc's `Target::from_expr`: a closure or an `async`/`gen` block is a closure, a loop of
/// each kind its own, the rest an expression; through parentheses.
fn from_expr(expr: ast::Expr<'_>) -> Target {
    match expr {
        ast::Expr::ClosureExpr(_) => Target::Closure,
        ast::Expr::BlockExpr(block) if block.async_token().is_some() || block.gen_token().is_some() => {
            Target::Closure
        }
        ast::Expr::ParenExpr(paren) => paren.expr().map_or(Target::Expression, from_expr),
        ast::Expr::ForExpr(_) => Target::ForLoop,
        ast::Expr::LoopExpr(_) => Target::Loop,
        ast::Expr::WhileExpr(_) => Target::While,
        ast::Expr::BreakExpr(_) => Target::Break,
        _ => Target::Expression,
    }
}

/// Whether `node` is the body of a function: its inner attributes are the function's.
fn is_fn_body(node: ast::Node<'_>) -> bool {
    node.parent().is_some_and(|it| it.kind() == SyntaxKind::FN)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Trait,
    Impl { of_trait: bool },
    Extern,
    Other,
}

/// What an item is written in: a trait, an impl, an `extern` block or none of them; for an
/// item a macro made, what the macro was called in.
fn container(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>) -> Container {
    let Some(parent) = node.value.parent() else { return Container::Other };
    match parent.kind() {
        SyntaxKind::ASSOC_ITEM_LIST => match parent.parent() {
            Some(it) if it.kind() == SyntaxKind::TRAIT => Container::Trait,
            Some(it) => match ast::Impl::cast(it) {
                Some(it) => Container::Impl { of_trait: it.trait_().is_some() },
                None => Container::Other,
            },
            None => Container::Other,
        },
        SyntaxKind::EXTERN_ITEM_LIST => Container::Extern,
        _ if parent.parent().is_none() => match node.file_id.macro_file() {
            Some(call) => {
                container(cx, call.loc(cx.db).to_node_view(cx.db))
            }
            None => Container::Other,
        },
        _ => Container::Other,
    }
}
