//! The views: `syntax::ast`'s nodes and accessors, as `const fn`s over a [`Tree`].
//!
//! Each view has the name of the `syntax::ast` node it ports, and each accessor the name of the
//! accessor it ports (`support::child` is the first child of the kind, `support::token` the first
//! token child of the kind). The traits `syntax::ast` shares accessors through (`HasName`,
//! `HasAttrs`, `HasVisibility`, `HasGenericParams`) are inherent methods here, since a const
//! fn can't yet call a trait's method.

use crate::{Children, Node, Token, Tree, kinds::*, str_eq};

/// `match $e { Some(it) => it, None => return None }`: `?`, which a const fn can't use on
/// `Option` (it is a trait's call).
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Some(it) => it,
            None => return None,
        }
    };
}

/// A view of the nodes of one kind.
macro_rules! node {
    ($(#[$meta:meta])* $name:ident = $kind:ident) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Copy)]
        pub struct $name<'a>(Node<'a>);

        impl<'a> $name<'a> {
            /// `node` as this view, if it is of its kind.
            pub const fn cast(node: Node<'a>) -> Option<$name<'a>> {
                if node.kind() == $kind { Some($name(node)) } else { None }
            }

            /// The node.
            pub const fn syntax(self) -> Node<'a> {
                self.0
            }
        }
    };
}

/// A cursor over the children of a node that cast to `$item`.
macro_rules! cursor {
    ($(#[$meta:meta])* $name:ident of $item:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy)]
        pub struct $name<'a>(Children<'a>);

        impl<'a> $name<'a> {
            /// The next one.
            #[allow(clippy::should_implement_trait)]
            pub const fn next(&mut self) -> Option<$item<'a>> {
                while let Some(node) = self.0.next() {
                    if let Some(it) = $item::cast(node) {
                        return Some(it);
                    }
                }
                None
            }

            /// How many are left.
            #[allow(clippy::should_implement_trait)]
            pub const fn count(self) -> usize {
                let mut this = self;
                let mut n = 0;
                while this.next().is_some() {
                    n += 1;
                }
                n
            }
        }

        impl<'a> Iterator for $name<'a> {
            type Item = $item<'a>;
            fn next(&mut self) -> Option<$item<'a>> {
                $name::next(self)
            }
        }
    };
}

/// `HasName::name`.
macro_rules! has_name {
    ($($name:ident),*) => {$(
        impl<'a> $name<'a> {
            /// Its name (`HasName::name`).
            pub const fn name(self) -> Option<Name<'a>> {
                match self.0.child(NAME) { Some(it) => Some(Name(it)), None => None }
            }
        }
    )*};
}

/// `HasAttrs::attrs`.
macro_rules! has_attrs {
    ($($name:ident),*) => {$(
        impl<'a> $name<'a> {
            /// Its attributes, not doc comments (`HasAttrs::attrs`).
            pub const fn attrs(self) -> Attrs<'a> {
                Attrs(self.0.children())
            }
        }
    )*};
}

/// `HasVisibility::visibility`.
macro_rules! has_visibility {
    ($($name:ident),*) => {$(
        impl<'a> $name<'a> {
            /// Its visibility (`HasVisibility::visibility`).
            pub const fn visibility(self) -> Option<Visibility<'a>> {
                match self.0.child(VISIBILITY) { Some(it) => Some(Visibility(it)), None => None }
            }
        }
    )*};
}

/// `HasGenericParams`.
macro_rules! has_generic_params {
    ($($name:ident),*) => {$(
        impl<'a> $name<'a> {
            /// Its generic parameters (`HasGenericParams::generic_param_list`).
            pub const fn generic_param_list(self) -> Option<GenericParamList<'a>> {
                match self.0.child(GENERIC_PARAM_LIST) {
                    Some(it) => Some(GenericParamList(it)),
                    None => None,
                }
            }

            /// Its where clause (`HasGenericParams::where_clause`).
            pub const fn where_clause(self) -> Option<WhereClause<'a>> {
                match self.0.child(WHERE_CLAUSE) { Some(it) => Some(WhereClause(it)), None => None }
            }
        }
    )*};
}

/// A child view of a kind: `support::child`.
macro_rules! child {
    ($node:expr, $view:ident) => {
        $view::first_child($node)
    };
}

/// `first_child` for a view of one kind.
macro_rules! first_child_of_kind {
    ($($name:ident = $kind:ident),*) => {$(
        impl<'a> $name<'a> {
            const fn first_child(node: Node<'a>) -> Option<$name<'a>> {
                match node.child($kind) { Some(it) => Some($name(it)), None => None }
            }
        }
    )*};
}

// --- Files and items ---------------------------------------------------------------------

node!(
    /// A file (`ast::SourceFile`).
    SourceFile = SOURCE_FILE
);
has_attrs!(SourceFile);

impl<'a> SourceFile<'a> {
    /// Its items (`HasModuleItem::items`).
    pub const fn items(self) -> Items<'a> {
        Items(self.0.children())
    }
}

cursor!(
    /// Items, in order.
    Items of Item
);

/// An item of any kind (`ast::Item`). A struct rather than an enum so that a host can write
/// one, `repr(transparent)` over a [`Node`]: match on it with `as_*`.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Item<'a>(Node<'a>);

/// The kinds `ast::Item` is one of.
const fn is_item(kind: u16) -> bool {
    kind == ASM_EXPR
        || kind == CONST
        || kind == ENUM
        || kind == EXTERN_BLOCK
        || kind == EXTERN_CRATE
        || kind == FN
        || kind == IMPL
        || kind == MACRO_CALL
        || kind == MACRO_DEF
        || kind == MACRO_RULES
        || kind == MODULE
        || kind == STATIC
        || kind == STRUCT
        || kind == TRAIT
        || kind == TYPE_ALIAS
        || kind == UNION
        || kind == USE
}

/// The kinds that are `HasName`.
const fn has_name_kind(kind: u16) -> bool {
    kind == CONST
        || kind == ENUM
        || kind == FN
        || kind == MACRO_DEF
        || kind == MACRO_RULES
        || kind == MODULE
        || kind == STATIC
        || kind == STRUCT
        || kind == TRAIT
        || kind == TYPE_ALIAS
        || kind == UNION
}

/// The kinds of items that are `HasVisibility`: all but `asm!`, `extern { .. }` and macro calls.
const fn has_visibility_kind(kind: u16) -> bool {
    kind != ASM_EXPR && kind != EXTERN_BLOCK && kind != MACRO_CALL
}

/// The kinds that are `HasGenericParams`.
const fn has_generics_kind(kind: u16) -> bool {
    kind == CONST
        || kind == ENUM
        || kind == FN
        || kind == IMPL
        || kind == STRUCT
        || kind == TRAIT
        || kind == TYPE_ALIAS
        || kind == UNION
}

impl<'a> Item<'a> {
    /// `node` as an item, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<Item<'a>> {
        if is_item(node.kind()) { Some(Item(node)) } else { None }
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        self.0
    }

    /// It as a struct, if it is one.
    pub const fn as_struct(self) -> Option<Struct<'a>> {
        Struct::cast(self.0)
    }

    /// It as an enum, if it is one.
    pub const fn as_enum(self) -> Option<Enum<'a>> {
        Enum::cast(self.0)
    }

    /// It as a union, if it is one.
    pub const fn as_union(self) -> Option<Union<'a>> {
        Union::cast(self.0)
    }

    /// It as a struct, an enum or a union, if it is one (`ast::Adt`).
    pub const fn as_adt(self) -> Option<Adt<'a>> {
        Adt::cast(self.0)
    }

    /// Its attributes (`HasAttrs::attrs`; every item has them).
    pub const fn attrs(self) -> Attrs<'a> {
        Attrs(self.0.children())
    }

    /// Its name, for an item that has one (`HasName::name`).
    pub const fn name(self) -> Option<Name<'a>> {
        if !has_name_kind(self.0.kind()) {
            return None;
        }
        match self.0.child(NAME) {
            Some(it) => Some(Name(it)),
            None => None,
        }
    }

    /// Its visibility, for an item that can have one (`HasVisibility::visibility`).
    pub const fn visibility(self) -> Option<Visibility<'a>> {
        if !has_visibility_kind(self.0.kind()) {
            return None;
        }
        match self.0.child(VISIBILITY) {
            Some(it) => Some(Visibility(it)),
            None => None,
        }
    }

    /// Its generic parameters, for an item that can have them.
    pub const fn generic_param_list(self) -> Option<GenericParamList<'a>> {
        if !has_generics_kind(self.0.kind()) {
            return None;
        }
        match self.0.child(GENERIC_PARAM_LIST) {
            Some(it) => Some(GenericParamList(it)),
            None => None,
        }
    }

    /// Its where clause, for an item that can have one.
    pub const fn where_clause(self) -> Option<WhereClause<'a>> {
        if !has_generics_kind(self.0.kind()) {
            return None;
        }
        match self.0.child(WHERE_CLAUSE) {
            Some(it) => Some(WhereClause(it)),
            None => None,
        }
    }
}

/// A struct, an enum or a union (`ast::Adt`).
#[derive(Clone, Copy)]
pub enum Adt<'a> {
    Enum(Enum<'a>),
    Struct(Struct<'a>),
    Union(Union<'a>),
}

impl<'a> Adt<'a> {
    /// `node` as an ADT, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<Adt<'a>> {
        let kind = node.kind();
        if kind == ENUM {
            Some(Adt::Enum(Enum(node)))
        } else if kind == STRUCT {
            Some(Adt::Struct(Struct(node)))
        } else if kind == UNION {
            Some(Adt::Union(Union(node)))
        } else {
            None
        }
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        match self {
            Adt::Enum(it) => it.0,
            Adt::Struct(it) => it.0,
            Adt::Union(it) => it.0,
        }
    }

    /// Its name.
    pub const fn name(self) -> Option<Name<'a>> {
        child!(self.syntax(), Name)
    }

    /// Its attributes.
    pub const fn attrs(self) -> Attrs<'a> {
        Attrs(self.syntax().children())
    }

    /// Its visibility.
    pub const fn visibility(self) -> Option<Visibility<'a>> {
        child!(self.syntax(), Visibility)
    }

    /// Its generic parameters.
    pub const fn generic_param_list(self) -> Option<GenericParamList<'a>> {
        child!(self.syntax(), GenericParamList)
    }

    /// Its where clause.
    pub const fn where_clause(self) -> Option<WhereClause<'a>> {
        child!(self.syntax(), WhereClause)
    }
}

node!(
    /// `struct` (`ast::Struct`).
    Struct = STRUCT
);
node!(
    /// `enum` (`ast::Enum`).
    Enum = ENUM
);
node!(
    /// `union` (`ast::Union`).
    Union = UNION
);
has_name!(Struct, Enum, Union);
has_attrs!(Struct, Enum, Union);
has_visibility!(Struct, Enum, Union);
has_generic_params!(Struct, Enum, Union);

impl<'a> Struct<'a> {
    /// Its fields: `{ .. }`, `( .. )`, or none for a unit struct (`ast::Struct::field_list`).
    pub const fn field_list(self) -> Option<FieldList<'a>> {
        FieldList::first_child(self.0)
    }
}

impl<'a> Enum<'a> {
    /// Its variants (`ast::Enum::variant_list`).
    pub const fn variant_list(self) -> Option<VariantList<'a>> {
        child!(self.0, VariantList)
    }
}

impl<'a> Union<'a> {
    /// Its fields (`ast::Union::record_field_list`).
    pub const fn record_field_list(self) -> Option<RecordFieldList<'a>> {
        child!(self.0, RecordFieldList)
    }
}

// --- Fields and variants -----------------------------------------------------------------

/// `{ a: A, .. }` or `(A, ..)` (`ast::FieldList`).
#[derive(Clone, Copy)]
pub enum FieldList<'a> {
    RecordFieldList(RecordFieldList<'a>),
    TupleFieldList(TupleFieldList<'a>),
}

impl<'a> FieldList<'a> {
    /// `node` as a field list, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<FieldList<'a>> {
        let kind = node.kind();
        if kind == RECORD_FIELD_LIST {
            Some(FieldList::RecordFieldList(RecordFieldList(node)))
        } else if kind == TUPLE_FIELD_LIST {
            Some(FieldList::TupleFieldList(TupleFieldList(node)))
        } else {
            None
        }
    }

    const fn first_child(node: Node<'a>) -> Option<FieldList<'a>> {
        let mut children = node.children();
        while let Some(child) = children.next() {
            if let Some(it) = FieldList::cast(child) {
                return Some(it);
            }
        }
        None
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        match self {
            FieldList::RecordFieldList(it) => it.0,
            FieldList::TupleFieldList(it) => it.0,
        }
    }
}

node!(
    /// `{ a: A, .. }` (`ast::RecordFieldList`).
    RecordFieldList = RECORD_FIELD_LIST
);
node!(
    /// `a: A` (`ast::RecordField`).
    RecordField = RECORD_FIELD
);
node!(
    /// `(A, ..)` (`ast::TupleFieldList`).
    TupleFieldList = TUPLE_FIELD_LIST
);
node!(
    /// A field of a tuple struct (`ast::TupleField`).
    TupleField = TUPLE_FIELD
);
cursor!(
    /// Named fields, in order.
    RecordFields of RecordField
);
cursor!(
    /// Fields of a tuple struct, in order.
    TupleFields of TupleField
);
has_name!(RecordField);
has_attrs!(RecordField, TupleField);
has_visibility!(RecordField, TupleField);

impl<'a> RecordFieldList<'a> {
    /// Its fields (`ast::RecordFieldList::fields`).
    pub const fn fields(self) -> RecordFields<'a> {
        RecordFields(self.0.children())
    }
}

impl<'a> TupleFieldList<'a> {
    /// Its fields (`ast::TupleFieldList::fields`).
    pub const fn fields(self) -> TupleFields<'a> {
        TupleFields(self.0.children())
    }
}

impl<'a> RecordField<'a> {
    /// Its type (`ast::RecordField::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// Its default value, `= value` (`ast::RecordField::default_val`).
    pub const fn default_val(self) -> Option<ConstArg<'a>> {
        child!(self.0, ConstArg)
    }
}

impl<'a> TupleField<'a> {
    /// Its type (`ast::TupleField::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }
}

node!(
    /// `{ A, B(..), .. }` (`ast::VariantList`).
    VariantList = VARIANT_LIST
);
node!(
    /// A variant of an enum (`ast::Variant`).
    Variant = VARIANT
);
cursor!(
    /// Variants, in order.
    Variants of Variant
);
has_name!(Variant);
has_attrs!(Variant);
has_visibility!(Variant);

impl<'a> VariantList<'a> {
    /// Its variants (`ast::VariantList::variants`).
    pub const fn variants(self) -> Variants<'a> {
        Variants(self.0.children())
    }
}

impl<'a> Variant<'a> {
    /// Its fields, if it has any (`ast::Variant::field_list`).
    pub const fn field_list(self) -> Option<FieldList<'a>> {
        FieldList::first_child(self.0)
    }

    /// Its discriminant, `= value` (`ast::Variant::const_arg`).
    pub const fn const_arg(self) -> Option<ConstArg<'a>> {
        child!(self.0, ConstArg)
    }
}

node!(
    /// A constant given as an argument or a default: an expression (`ast::ConstArg`).
    ConstArg = CONST_ARG
);

// --- Names, visibility, attributes -------------------------------------------------------

node!(
    /// A name being defined (`ast::Name`).
    Name = NAME
);
node!(
    /// A name being used (`ast::NameRef`).
    NameRef = NAME_REF
);
node!(
    /// `'a` (`ast::Lifetime`).
    Lifetime = LIFETIME
);

impl<'a> Name<'a> {
    /// Its text, `r#` and all (`ast::Name::text`).
    pub const fn text(self) -> &'a str {
        match self.0.first_token() {
            Some(it) => it.text(),
            None => "",
        }
    }

    /// Its token, for its span.
    pub const fn token(self) -> Option<Token<'a>> {
        self.0.first_token()
    }
}

impl<'a> NameRef<'a> {
    /// Its text (`ast::NameRef::text`).
    pub const fn text(self) -> &'a str {
        match self.0.first_token() {
            Some(it) => it.text(),
            None => "",
        }
    }

    /// Its token, for its span.
    pub const fn token(self) -> Option<Token<'a>> {
        self.0.first_token()
    }
}

impl<'a> Lifetime<'a> {
    /// Its text, `'` and all (`ast::Lifetime::text`).
    pub const fn text(self) -> &'a str {
        match self.0.token(LIFETIME_IDENT) {
            Some(it) => it.text(),
            None => "",
        }
    }
}

node!(
    /// `pub`, `pub(crate)`, `pub(in path)` (`ast::Visibility`).
    Visibility = VISIBILITY
);
node!(
    /// `(crate)`, `(in path)` of a visibility (`ast::VisibilityInner`).
    VisibilityInner = VISIBILITY_INNER
);

impl<'a> Visibility<'a> {
    /// What is in its parentheses (`ast::Visibility::visibility_inner`).
    pub const fn visibility_inner(self) -> Option<VisibilityInner<'a>> {
        child!(self.0, VisibilityInner)
    }
}

impl<'a> VisibilityInner<'a> {
    /// The path (`ast::VisibilityInner::path`).
    pub const fn path(self) -> Option<Path<'a>> {
        child!(self.0, Path)
    }
}

node!(
    /// `#[meta]` or `#![meta]` (`ast::Attr`).
    Attr = ATTR
);
cursor!(
    /// Attributes, in order.
    Attrs of Attr
);

impl<'a> Attr<'a> {
    /// What is in its brackets (`ast::Attr::meta`).
    pub const fn meta(self) -> Option<Meta<'a>> {
        Meta::first_child(self.0)
    }

    /// Whether it is `#![..]` (`ast::Attr::kind`).
    pub const fn is_inner(self) -> bool {
        self.0.token(BANG).is_some()
    }

    /// Its path (`ast::Attr::path`).
    pub const fn path(self) -> Option<Path<'a>> {
        tri!(self.meta()).path()
    }

    /// Its name, if its path is one name or it is `cfg`/`cfg_attr` (`ast::Attr::simple_name`).
    pub const fn simple_name(self) -> Option<&'a str> {
        tri!(self.meta()).simple_name()
    }

    /// The token tree of `#[name(..)]` (`ast::Attr::as_simple_call`).
    pub const fn as_simple_call(self) -> Option<(&'a str, TokenTree<'a>)> {
        tri!(self.meta()).as_simple_call()
    }
}

/// What is in an attribute's brackets (`ast::Meta`).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Meta<'a>(Node<'a>);

impl<'a> Meta<'a> {
    /// `node` as a meta, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<Meta<'a>> {
        let kind = node.kind();
        if kind == CFG_ATTR_META
            || kind == CFG_META
            || kind == UNSAFE_META
            || kind == PATH_META
            || kind == KEY_VALUE_META
            || kind == TOKEN_TREE_META
        {
            Some(Meta(node))
        } else {
            None
        }
    }

    const fn first_child(node: Node<'a>) -> Option<Meta<'a>> {
        let mut children = node.children();
        while let Some(child) = children.next() {
            if let Some(it) = Meta::cast(child) {
                return Some(it);
            }
        }
        None
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        self.0
    }

    /// Whether it is `cfg_attr(..)`.
    pub const fn is_cfg_attr(self) -> bool {
        self.0.kind() == CFG_ATTR_META
    }

    /// Whether it is `cfg(..)`.
    pub const fn is_cfg(self) -> bool {
        self.0.kind() == CFG_META
    }

    /// The meta of `unsafe(meta)` (`ast::UnsafeMeta::meta`).
    pub const fn unsafe_meta(self) -> Option<Meta<'a>> {
        if self.0.kind() != UNSAFE_META {
            return None;
        }
        Meta::first_child(self.0)
    }

    /// Its path (`ast::Meta::path`): none for `cfg` and `cfg_attr`.
    pub const fn path(self) -> Option<Path<'a>> {
        let kind = self.0.kind();
        if kind == CFG_ATTR_META || kind == CFG_META {
            None
        } else if kind == UNSAFE_META {
            tri!(self.unsafe_meta()).path()
        } else {
            child!(self.0, Path)
        }
    }

    /// The token tree of `path(..)` (`ast::TokenTreeMeta::token_tree`).
    pub const fn token_tree(self) -> Option<TokenTree<'a>> {
        if self.0.kind() != TOKEN_TREE_META {
            return None;
        }
        child!(self.0, TokenTree)
    }

    /// Whether it is `path = value` (`ast::KeyValueMeta`); its value is the tokens after `=`.
    pub const fn is_key_value(self) -> bool {
        self.0.kind() == KEY_VALUE_META
    }

    /// The path of `path`, alone (`ast::Meta::as_simple_path`).
    pub const fn as_simple_path(self) -> Option<Path<'a>> {
        if self.0.kind() != PATH_META {
            return None;
        }
        child!(self.0, Path)
    }

    /// The name of `name` (`ast::Meta::as_simple_atom`).
    pub const fn as_simple_atom(self) -> Option<&'a str> {
        Some(tri!(tri!(self.as_simple_path()).as_single_name_ref()).text())
    }

    /// The name and token tree of `name(..)` (`ast::Meta::as_simple_call`).
    pub const fn as_simple_call(self) -> Option<(&'a str, TokenTree<'a>)> {
        if self.0.kind() != TOKEN_TREE_META {
            return None;
        }
        let name = tri!(tri!(child!(self.0, Path)).as_single_name_ref()).text();
        Some((name, tri!(child!(self.0, TokenTree))))
    }

    /// Its name, if its path is one name, or `cfg`/`cfg_attr` (`ast::Meta::simple_name`).
    pub const fn simple_name(self) -> Option<&'a str> {
        let kind = self.0.kind();
        if kind == CFG_ATTR_META {
            return Some("cfg_attr");
        }
        if kind == CFG_META {
            return Some("cfg");
        }
        let path = tri!(self.path());
        if path.qualifier().is_some() {
            return None;
        }
        Some(tri!(tri!(path.segment()).0.first_token()).text())
    }
}

node!(
    /// `( .. )`, `[ .. ]` or `{ .. }`, of a macro call or an attribute (`ast::TokenTree`).
    TokenTree = TOKEN_TREE
);

// --- Paths -------------------------------------------------------------------------------

node!(
    /// `a::b::<C>` (`ast::Path`).
    Path = PATH
);
node!(
    /// `b::<C>` of a path (`ast::PathSegment`).
    PathSegment = PATH_SEGMENT
);
first_child_of_kind!(
    Name = NAME,
    Visibility = VISIBILITY,
    VisibilityInner = VISIBILITY_INNER,
    GenericParamList = GENERIC_PARAM_LIST,
    WhereClause = WHERE_CLAUSE,
    VariantList = VARIANT_LIST,
    RecordFieldList = RECORD_FIELD_LIST,
    ConstArg = CONST_ARG,
    TokenTree = TOKEN_TREE,
    Path = PATH,
    PathSegment = PATH_SEGMENT,
    NameRef = NAME_REF,
    GenericArgList = GENERIC_ARG_LIST,
    Lifetime = LIFETIME,
    TypeBoundList = TYPE_BOUND_LIST
);

impl<'a> Path<'a> {
    /// The path before the last `::` (`ast::Path::qualifier`).
    pub const fn qualifier(self) -> Option<Path<'a>> {
        child!(self.0, Path)
    }

    /// The last segment (`ast::Path::segment`).
    pub const fn segment(self) -> Option<PathSegment<'a>> {
        child!(self.0, PathSegment)
    }

    /// Its one segment's name, if it has no qualifier (`ast::Path::as_single_name_ref`).
    pub const fn as_single_name_ref(self) -> Option<NameRef<'a>> {
        if self.qualifier().is_some() {
            return None;
        }
        tri!(self.segment()).name_ref()
    }
}

impl<'a> PathSegment<'a> {
    /// Its name (`ast::PathSegment::name_ref`).
    pub const fn name_ref(self) -> Option<NameRef<'a>> {
        child!(self.0, NameRef)
    }

    /// Its generic arguments (`ast::PathSegment::generic_arg_list`).
    pub const fn generic_arg_list(self) -> Option<GenericArgList<'a>> {
        child!(self.0, GenericArgList)
    }
}

// --- Generics ----------------------------------------------------------------------------

node!(
    /// `<T, 'a, const N: usize>` (`ast::GenericParamList`).
    GenericParamList = GENERIC_PARAM_LIST
);
node!(
    /// `T: Bound = Default` (`ast::TypeParam`).
    TypeParam = TYPE_PARAM
);
node!(
    /// `'a: 'b` (`ast::LifetimeParam`).
    LifetimeParam = LIFETIME_PARAM
);
node!(
    /// `const N: usize = 1` (`ast::ConstParam`).
    ConstParam = CONST_PARAM
);
node!(
    /// `: A + B` of a parameter or a predicate (`ast::TypeBoundList`).
    TypeBoundList = TYPE_BOUND_LIST
);
node!(
    /// `where T: A, ..` (`ast::WhereClause`).
    WhereClause = WHERE_CLAUSE
);
node!(
    /// `T: A` of a where clause (`ast::WherePred`).
    WherePred = WHERE_PRED
);
cursor!(
    /// Generic parameters, in order.
    GenericParams of GenericParam
);
cursor!(
    /// Where predicates, in order.
    WherePreds of WherePred
);
has_name!(TypeParam, ConstParam);
has_attrs!(TypeParam, ConstParam, LifetimeParam);

/// A generic parameter (`ast::GenericParam`).
#[derive(Clone, Copy)]
pub enum GenericParam<'a> {
    ConstParam(ConstParam<'a>),
    LifetimeParam(LifetimeParam<'a>),
    TypeParam(TypeParam<'a>),
}

impl<'a> GenericParam<'a> {
    /// `node` as a generic parameter, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<GenericParam<'a>> {
        let kind = node.kind();
        if kind == CONST_PARAM {
            Some(GenericParam::ConstParam(ConstParam(node)))
        } else if kind == LIFETIME_PARAM {
            Some(GenericParam::LifetimeParam(LifetimeParam(node)))
        } else if kind == TYPE_PARAM {
            Some(GenericParam::TypeParam(TypeParam(node)))
        } else {
            None
        }
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        match self {
            GenericParam::ConstParam(it) => it.0,
            GenericParam::LifetimeParam(it) => it.0,
            GenericParam::TypeParam(it) => it.0,
        }
    }
}

impl<'a> GenericParamList<'a> {
    /// Its parameters (`ast::GenericParamList::generic_params`).
    pub const fn generic_params(self) -> GenericParams<'a> {
        GenericParams(self.0.children())
    }
}

impl<'a> TypeParam<'a> {
    /// Its bounds (`HasTypeBounds::type_bound_list`).
    pub const fn type_bound_list(self) -> Option<TypeBoundList<'a>> {
        child!(self.0, TypeBoundList)
    }

    /// Its default (`ast::TypeParam::default_type`).
    pub const fn default_type(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }
}

impl<'a> LifetimeParam<'a> {
    /// The lifetime (`ast::LifetimeParam::lifetime`).
    pub const fn lifetime(self) -> Option<Lifetime<'a>> {
        child!(self.0, Lifetime)
    }

    /// Its bounds (`HasTypeBounds::type_bound_list`).
    pub const fn type_bound_list(self) -> Option<TypeBoundList<'a>> {
        child!(self.0, TypeBoundList)
    }
}

impl<'a> ConstParam<'a> {
    /// Its type (`ast::ConstParam::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// Its default (`ast::ConstParam::default_val`).
    pub const fn default_val(self) -> Option<ConstArg<'a>> {
        child!(self.0, ConstArg)
    }
}

impl<'a> WhereClause<'a> {
    /// Its predicates (`ast::WhereClause::predicates`).
    pub const fn predicates(self) -> WherePreds<'a> {
        WherePreds(self.0.children())
    }
}

impl<'a> WherePred<'a> {
    /// The type bounded (`ast::WherePred::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// The lifetime bounded (`ast::WherePred::lifetime`).
    pub const fn lifetime(self) -> Option<Lifetime<'a>> {
        child!(self.0, Lifetime)
    }

    /// Its bounds (`HasTypeBounds::type_bound_list`).
    pub const fn type_bound_list(self) -> Option<TypeBoundList<'a>> {
        child!(self.0, TypeBoundList)
    }
}

node!(
    /// `<A, 'b, C = D>` of a path segment (`ast::GenericArgList`).
    GenericArgList = GENERIC_ARG_LIST
);
cursor!(
    /// Generic arguments, in order.
    GenericArgs of GenericArg
);

/// A generic argument (`ast::GenericArg`): a type, a lifetime, a constant or an associated
/// type's binding.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct GenericArg<'a>(Node<'a>);

impl<'a> GenericArg<'a> {
    /// `node` as a generic argument, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<GenericArg<'a>> {
        let kind = node.kind();
        if kind == TYPE_ARG || kind == LIFETIME_ARG || kind == CONST_ARG || kind == ASSOC_TYPE_ARG {
            Some(GenericArg(node))
        } else {
            None
        }
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        self.0
    }

    /// The type of a type argument (`ast::TypeArg::ty`).
    pub const fn as_type(self) -> Option<Type<'a>> {
        if self.0.kind() != TYPE_ARG {
            return None;
        }
        Type::first_child(self.0)
    }

    /// The lifetime of a lifetime argument (`ast::LifetimeArg::lifetime`).
    pub const fn as_lifetime(self) -> Option<Lifetime<'a>> {
        if self.0.kind() != LIFETIME_ARG {
            return None;
        }
        child!(self.0, Lifetime)
    }

    /// A constant argument (`ast::ConstArg`).
    pub const fn as_const(self) -> Option<ConstArg<'a>> {
        ConstArg::cast(self.0)
    }
}

impl<'a> GenericArgList<'a> {
    /// Its arguments (`ast::GenericArgList::generic_args`).
    pub const fn generic_args(self) -> GenericArgs<'a> {
        GenericArgs(self.0.children())
    }
}

// --- Types -------------------------------------------------------------------------------

/// A type (`ast::Type`): match on it with `as_*`; the kinds without a view of their own
/// (`dyn`, `impl`, `fn`, `for<..>`, `!`, `_`, a macro, a pattern type, parentheses) are read
/// through [`Type::syntax`].
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Type<'a>(Node<'a>);

/// The kinds `ast::Type` is one of.
const fn is_type(kind: u16) -> bool {
    kind == ARRAY_TYPE
        || kind == DYN_TRAIT_TYPE
        || kind == FN_PTR_TYPE
        || kind == FOR_TYPE
        || kind == IMPL_TRAIT_TYPE
        || kind == INFER_TYPE
        || kind == MACRO_TYPE
        || kind == NEVER_TYPE
        || kind == PAREN_TYPE
        || kind == PATH_TYPE
        || kind == PATTERN_TYPE
        || kind == PTR_TYPE
        || kind == REF_TYPE
        || kind == SLICE_TYPE
        || kind == TUPLE_TYPE
}

impl<'a> Type<'a> {
    /// `node` as a type, if it is one.
    pub const fn cast(node: Node<'a>) -> Option<Type<'a>> {
        if is_type(node.kind()) { Some(Type(node)) } else { None }
    }

    const fn first_child(node: Node<'a>) -> Option<Type<'a>> {
        let mut children = node.children();
        while let Some(child) = children.next() {
            if let Some(it) = Type::cast(child) {
                return Some(it);
            }
        }
        None
    }

    const fn nth_child(node: Node<'a>, n: usize) -> Option<Type<'a>> {
        let mut children = node.children();
        let mut seen = 0;
        while let Some(child) = children.next() {
            if let Some(it) = Type::cast(child) {
                if seen == n {
                    return Some(it);
                }
                seen += 1;
            }
        }
        None
    }

    /// The node.
    pub const fn syntax(self) -> Node<'a> {
        self.0
    }

    /// `a::B<C>` (`ast::PathType`).
    pub const fn as_path(self) -> Option<PathType<'a>> {
        PathType::cast(self.0)
    }

    /// `&'a mut T` (`ast::RefType`).
    pub const fn as_ref(self) -> Option<RefType<'a>> {
        RefType::cast(self.0)
    }

    /// `*const T` (`ast::PtrType`).
    pub const fn as_ptr(self) -> Option<PtrType<'a>> {
        PtrType::cast(self.0)
    }

    /// `(A, B)` (`ast::TupleType`).
    pub const fn as_tuple(self) -> Option<TupleType<'a>> {
        TupleType::cast(self.0)
    }

    /// `[T; N]` (`ast::ArrayType`).
    pub const fn as_array(self) -> Option<ArrayType<'a>> {
        ArrayType::cast(self.0)
    }

    /// `[T]` (`ast::SliceType`).
    pub const fn as_slice(self) -> Option<SliceType<'a>> {
        SliceType::cast(self.0)
    }
}

node!(
    /// `a::B<C>` (`ast::PathType`).
    PathType = PATH_TYPE
);
node!(
    /// `&'a mut T` (`ast::RefType`).
    RefType = REF_TYPE
);
node!(
    /// `*const T`, `*mut T` (`ast::PtrType`).
    PtrType = PTR_TYPE
);
node!(
    /// `(A, B)` (`ast::TupleType`).
    TupleType = TUPLE_TYPE
);
node!(
    /// `[T; N]` (`ast::ArrayType`).
    ArrayType = ARRAY_TYPE
);
node!(
    /// `[T]` (`ast::SliceType`).
    SliceType = SLICE_TYPE
);

/// The types of a tuple type, in order.
#[derive(Clone, Copy)]
pub struct TupleTypeFields<'a>(Children<'a>);

impl<'a> TupleTypeFields<'a> {
    /// The next one.
    #[allow(clippy::should_implement_trait)]
    pub const fn next(&mut self) -> Option<Type<'a>> {
        while let Some(node) = self.0.next() {
            if let Some(it) = Type::cast(node) {
                return Some(it);
            }
        }
        None
    }
}

impl<'a> Iterator for TupleTypeFields<'a> {
    type Item = Type<'a>;
    fn next(&mut self) -> Option<Type<'a>> {
        TupleTypeFields::next(self)
    }
}

impl<'a> PathType<'a> {
    /// The path (`ast::PathType::path`).
    pub const fn path(self) -> Option<Path<'a>> {
        child!(self.0, Path)
    }
}

impl<'a> RefType<'a> {
    /// The type referred to (`ast::RefType::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// The lifetime (`ast::RefType::lifetime`).
    pub const fn lifetime(self) -> Option<Lifetime<'a>> {
        child!(self.0, Lifetime)
    }

    /// Whether it is `&mut` (`ast::RefType::mut_token`).
    pub const fn is_mut(self) -> bool {
        self.0.token(MUT_KW).is_some()
    }
}

impl<'a> PtrType<'a> {
    /// The type pointed to (`ast::PtrType::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// Whether it is `*mut` (`ast::PtrType::mut_token`).
    pub const fn is_mut(self) -> bool {
        self.0.token(MUT_KW).is_some()
    }

    /// Whether it is `*const` (`ast::PtrType::const_token`).
    pub const fn is_const(self) -> bool {
        self.0.token(CONST_KW).is_some()
    }
}

impl<'a> TupleType<'a> {
    /// Its types (`ast::TupleType::fields`).
    pub const fn fields(self) -> TupleTypeFields<'a> {
        TupleTypeFields(self.0.children())
    }

    /// Its `n`th type, from 0.
    pub const fn field(self, n: usize) -> Option<Type<'a>> {
        Type::nth_child(self.0, n)
    }
}

impl<'a> ArrayType<'a> {
    /// The element type (`ast::ArrayType::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }

    /// The length (`ast::ArrayType::const_arg`).
    pub const fn const_arg(self) -> Option<ConstArg<'a>> {
        child!(self.0, ConstArg)
    }
}

impl<'a> SliceType<'a> {
    /// The element type (`ast::SliceType::ty`).
    pub const fn ty(self) -> Option<Type<'a>> {
        Type::first_child(self.0)
    }
}

// --- Text --------------------------------------------------------------------------------

impl<'a> Name<'a> {
    /// Whether its text is `text`.
    pub const fn is(self, text: &str) -> bool {
        str_eq(self.text(), text)
    }
}

impl<'a> NameRef<'a> {
    /// Whether its text is `text`.
    pub const fn is(self, text: &str) -> bool {
        str_eq(self.text(), text)
    }
}

impl<'a> Tree<'a> {
    /// The node numbered `at`, if `at` is a node of the tree: for a host or a memo that keeps
    /// views by number.
    pub const fn node(&'a self, at: u32) -> Option<Node<'a>> {
        if at < self.len() && self.kind(at) & crate::TOKEN == 0 {
            Some(Node { tree: self, at })
        } else {
            None
        }
    }
}
