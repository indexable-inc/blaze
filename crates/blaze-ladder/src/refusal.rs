//! What `blaze run` takes in and what it refuses with: the program's files ([`SourceFile`]),
//! a refusal as a finding ([`Refusal`] under a [`RefusalRule`]), and the plain Rust words for
//! each syntax node a refusal names ([`syntax_name`]). The front end ([`crate::front`]) makes
//! the refusals; blaze-cli's findings path and the runner read the same ones (one home).

use std::alloc::Global;
use std::boxed::Box;

use parser::SyntaxKind;
use parser::SyntaxKind::*;

/// One source file of the program.
pub struct SourceFile<'t> {
    /// Relative to the package root: "src/main.rs".
    pub path: &'t str,
    pub text: &'t str,
}

/// The rule a [`Refusal`] is under: the ladder's own ids (`ladder:` in a report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum RefusalRule {
    /// A construct this build of blaze does not take yet: `ladder:unsupported`.
    Unsupported = 1,
    /// Text the lexer or the parser refuses: `ladder:syntax`.
    Syntax = 2,
    /// A program the front end refuses for what it says (an unresolved name, a duplicate):
    /// `ladder:program`.
    Program = 3,
    /// Advice, not a refusal: `#[blaze::x]` names no attribute this build knows (blaze-surface's
    /// `tool_attrs::KNOWN`); the attribute stays inert and the program runs:
    /// `ladder:unknown-blaze-attribute`. Its level is the printer's, by this rule (rule 81).
    UnknownBlazeAttribute = 4,
}

impl RefusalRule {
    /// The rule's name in a report.
    pub fn name(self) -> &'static str {
        match self {
            RefusalRule::Unsupported => "ladder:unsupported",
            RefusalRule::Syntax => "ladder:syntax",
            RefusalRule::Program => "ladder:program",
            RefusalRule::UnknownBlazeAttribute => "ladder:unknown-blaze-attribute",
        }
    }

    /// Whether a finding of this rule is advice (the program still runs) rather than a refusal.
    pub fn is_advice(self) -> bool {
        self == RefusalRule::UnknownBlazeAttribute
    }
}

/// One refusal as a finding, the shape blaze-drive's `Print` takes: its file, the enclosing
/// item's first byte (`place`), its byte range relative to `place`, its rule and its words,
/// with no `line:col` (rule 71: positions are byte ranges until printed). For a construct,
/// `construct` is its plain Rust name ([`syntax_name`]) and `message` the sentence about it;
/// `secondary` is a range the construct applies to, relative to `place` too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Index into the program's files; `None` for a refusal of no file (no root).
    pub file: Option<u32>,
    pub rule: RefusalRule,
    /// The enclosing item's first byte in its file; 0 where none encloses it.
    pub place: u32,
    pub start: u32,
    /// Where the construct ends; equal to `start` where only its first byte is known.
    pub end: u32,
    pub secondary: Option<(u32, u32)>,
    pub construct: Option<&'static str>,
    pub message: Box<str, Global>,
}

/// The sentence a refused construct gets.
pub const NOT_YET: &str = "is not supported by this build of blaze yet";

/// The words a Rust programmer uses for a syntax node of `kind`: what a refusal names. `None`
/// for a token kind; every node kind has an entry (`every_node_kind_has_a_plain_name`).
pub fn syntax_name(kind: SyntaxKind) -> Option<&'static str> {
    Some(match kind {
        ABI => "an `extern \"ABI\"` qualifier",
        ARG_LIST => "an argument list",
        ARRAY_EXPR => "an array expression `[a, b]`",
        ARRAY_TYPE => "an array type `[T; N]`",
        ASM_CLOBBER_ABI | ASM_CONST | ASM_DIR_SPEC | ASM_EXPR | ASM_LABEL | ASM_OPERAND_EXPR | ASM_OPERAND_NAMED
        | ASM_OPTION | ASM_OPTIONS | ASM_REG_OPERAND | ASM_REG_SPEC | ASM_SYM => "inline assembly `asm!`",
        ASSOC_ITEM_LIST => "a trait's or impl's item list",
        ASSOC_TYPE_ARG => "an associated type argument or bound",
        ATTR => "an attribute `#[..]`",
        AWAIT_EXPR => "`.await`",
        BECOME_EXPR => "a `become` tail call",
        BIN_EXPR => "a binary operator",
        BLOCK_EXPR => "a block `{ .. }`",
        BOX_PAT => "a `box` pattern",
        BREAK_EXPR => "`break`",
        CALL_EXPR => "a call",
        CAST_EXPR => "an `as` cast",
        CFG_ATOM | CFG_ATTR_META | CFG_COMPOSITE | CFG_META | CFG_PRED_EXPR | CFG_PRED_PAT => "a `cfg` condition",
        CLOSURE_EXPR => "a closure",
        CONST => "a `const` item",
        CONST_ARG => "a const generic argument",
        CONST_BLOCK_PAT => "a `const { .. }` pattern",
        CONST_PARAM => "a const generic parameter",
        CONTINUE_EXPR => "`continue`",
        DEREF_PAT => "a `deref!` pattern",
        DOC_COMMENT => "a doc comment",
        DYN_TRAIT_TYPE => "a trait object type `dyn Trait`",
        ENUM => "an `enum`",
        EXPR_STMT => "an expression statement",
        EXTERN_BLOCK => "an `extern` block",
        EXTERN_CRATE => "`extern crate`",
        EXTERN_ITEM_LIST => "an `extern` block's item list",
        FIELD_EXPR => "a field access `x.f`",
        FN => "a fn",
        FN_PTR_TYPE => "a fn pointer type `fn(..)`",
        FORMAT_ARGS_ARG | FORMAT_ARGS_EXPR => "`format_args!`",
        FOR_BINDER => "a `for<'a>` binder",
        FOR_EXPR => "a `for` loop",
        FOR_TYPE => "a higher-ranked type `for<'a> ..`",
        GENERIC_ARG_LIST => "a generic argument list `<..>`",
        GENERIC_PARAM_LIST => "a generic parameter list `<..>`",
        GUARD_PAT => "a pattern with a guard",
        IDENT_PAT => "a binding pattern",
        IF_EXPR => "`if`",
        IMPL => "an `impl` block",
        IMPL_RESTRICTION => "an `impl` restriction",
        IMPL_TRAIT_TYPE => "an `impl Trait` type",
        INCLUDE_BYTES_EXPR => "`include_bytes!`",
        INDEX_EXPR => "indexing `a[i]`",
        INFER_TYPE => "an inferred type `_`",
        ITEM_LIST => "an inline module",
        KEY_VALUE_META => "an attribute's `key = value`",
        LABEL | LIFETIME => "a label or lifetime",
        LET_ELSE => "`let .. else`",
        LET_EXPR => "`if let` or `while let`",
        LET_STMT => "`let`",
        LIFETIME_ARG => "a lifetime argument",
        LIFETIME_PARAM => "a lifetime parameter",
        LITERAL => "a literal",
        LITERAL_PAT => "a literal pattern",
        LOOP_EXPR => "`loop`",
        MACRO_CALL | MACRO_EXPR | MACRO_ITEMS | MACRO_PAT | MACRO_STMTS | MACRO_TYPE => "a macro call `name!(..)`",
        MACRO_DEF => "a `macro` definition",
        MACRO_RULES => "`macro_rules!`",
        MATCH_ARM => "a `match` arm",
        MATCH_ARM_LIST => "a `match`'s arm list",
        MATCH_EXPR => "`match`",
        MATCH_GUARD => "a `match` guard `if ..`",
        METHOD_CALL_EXPR => "a method call `x.f()`",
        MODULE => "a module `mod`",
        MUT_RESTRICTION => "a `mut` restriction",
        NAME | NAME_REF => "a name",
        NEVER_TYPE => "the never type `!`",
        NOT_NULL => "a `!null` pattern type",
        OFFSET_OF_EXPR => "`offset_of!`",
        OR_PAT => "an or-pattern `a | b`",
        PARAM => "a parameter",
        PARAM_LIST => "a parameter list",
        PARENTHESIZED_ARG_LIST => "a parenthesized argument list `Fn(A) -> B`",
        PAREN_EXPR => "a parenthesized expression",
        PAREN_PAT => "a parenthesized pattern",
        PAREN_TYPE => "a parenthesized type",
        PATH | PATH_EXPR | PATH_SEGMENT => "a path",
        PATH_META => "an attribute's path",
        PATH_PAT => "a path pattern",
        PATH_TYPE => "a type path",
        PATTERN_TYPE => "a pattern type `T is ..`",
        PREFIX_EXPR => "a prefix operator (`-`, `!`, `*`)",
        PTR_TYPE => "a raw pointer type `*const T`",
        RANGE_EXPR => "a range `a..b`",
        RANGE_PAT => "a range pattern `a..=b`",
        RECORD_EXPR => "a struct literal `S { .. }`",
        RECORD_EXPR_FIELD => "a struct literal's field",
        RECORD_EXPR_FIELD_LIST => "a struct literal's field list",
        RECORD_FIELD => "a struct's field",
        RECORD_FIELD_LIST => "a struct's field list",
        RECORD_PAT => "a struct pattern `S { .. }`",
        RECORD_PAT_FIELD => "a struct pattern's field",
        RECORD_PAT_FIELD_LIST => "a struct pattern's field list",
        REF_EXPR => "a borrow `&x`",
        REF_PAT => "a reference pattern `&p`",
        REF_TYPE => "a reference type `&T`",
        RENAME => "a rename `as name`",
        REST_PAT => "a `..` rest pattern",
        RETURN_EXPR => "`return`",
        RETURN_TYPE_SYNTAX => "return type notation `f(..)`",
        RET_TYPE => "a return type",
        SELF_PARAM => "a `self` parameter",
        SLICE_PAT => "a slice pattern `[a, b]`",
        SLICE_TYPE => "a slice type `[T]`",
        SOURCE_FILE => "a source file",
        STATIC => "a `static`",
        STMT_LIST => "a block's statement list",
        STRUCT => "a `struct`",
        TOKEN_TREE => "a macro's input",
        TOKEN_TREE_META => "an attribute's argument list",
        TRAIT => "a `trait`",
        TRY_BLOCK_MODIFIER => "a `try` block",
        TRY_EXPR => "the `?` operator",
        TUPLE_EXPR => "a tuple `(a, b)`",
        TUPLE_FIELD => "a tuple struct's field",
        TUPLE_FIELD_LIST => "a tuple struct's field list",
        TUPLE_PAT => "a tuple pattern `(a, b)`",
        TUPLE_STRUCT_PAT => "a tuple struct or variant pattern `S(..)`",
        TUPLE_TYPE => "a tuple type `(A, B)`",
        TYPE_ALIAS => "a `type` alias or associated type",
        TYPE_ANCHOR => "a qualified path `<T as Trait>::..`",
        TYPE_ARG => "a type argument",
        TYPE_BOUND => "a trait bound",
        TYPE_BOUND_LIST => "a list of trait bounds",
        TYPE_PARAM => "a type parameter",
        UNDERSCORE_EXPR => "`_` as an expression",
        UNION => "a `union`",
        UNSAFE_META => "an `unsafe(..)` attribute",
        USE => "a `use` item",
        USE_BOUND_GENERIC_ARGS => "a `use<..>` bound",
        USE_TREE | USE_TREE_LIST => "a `use` item",
        VARIANT => "an enum variant",
        VARIANT_LIST => "an enum's variant list",
        VISIBILITY | VISIBILITY_INNER => "a visibility `pub`",
        WHERE_CLAUSE | WHERE_PRED => "a `where` clause",
        WHILE_EXPR => "`while`",
        WILDCARD_PAT => "a wildcard pattern `_`",
        YEET_EXPR => "`do yeet`",
        YIELD_EXPR => "`yield`",
        UNSAFE_BINDER_CAST_EXPR => "an `unsafe<..>` binder cast",
        UNSAFE_BINDER_TYPE => "an `unsafe<..>` binder type",
        SPEC_CLAUSE => "a specification clause (`requires`, `ensures`)",
        UNION_TYPE => "an error union type",
        RECORD_TYPE => "a record type",
        _ => return None,
    })
}
