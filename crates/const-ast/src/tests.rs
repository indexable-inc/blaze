//! The views against `syntax::ast`, on every node of the files of real crates: each view casts
//! the nodes `ast`'s type of the same name casts, and each accessor finds the element `ast`'s
//! accessor of the same name finds, with the same text.

extern crate std;

use std::{
    collections::HashMap,
    format,
    path::{Path as FsPath, PathBuf},
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use syntax::{
    AstNode, Edition, FlatTree, SourceFile as AstFile, SyntaxKind, SyntaxNode,
    ast::{
        self, HasAttrs, HasGenericArgs, HasGenericParams, HasModuleItem, HasName, HasTypeBounds,
        HasVisibility,
    },
};

use crate::*;

/// A [`Tree`]'s arrays, made from a flat tree of `syntax`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Arrays {
    kinds: Vec<u16>,
    ends: Vec<u32>,
    spans: Vec<u32>,
    texts: Vec<&'static str>,
}

impl Arrays {
    /// The arrays of `tree` with its lazy nodes' parts in their places (parsing them), so an
    /// outline's arrays are a full parse's. Its text is leaked to give `texts` a lifetime.
    pub(crate) fn of(tree: &FlatTree) -> Arrays {
        let text: &'static str = String::from(tree.text()).leak();
        let mut arrays =
            Arrays { kinds: Vec::new(), ends: Vec::new(), spans: Vec::new(), texts: Vec::new() };
        arrays.push(tree, text, 0);
        arrays
    }

    /// Pushes element `at` of `tree` and its subtree, the element numbers counted in the
    /// arrays.
    fn push(&mut self, tree: &FlatTree, text: &'static str, at: u32) {
        if tree.is_lazy(at) {
            return self.push(tree.part(at), text, 0);
        }
        let index = self.kinds.len();
        let token = tree.is_token(at);
        self.kinds.push(tree.kind(at).0 | if token { TOKEN } else { 0 });
        self.ends.push(0);
        self.spans.push(if token { index as u32 } else { NO_SPAN });
        let range = tree.range(at);
        self.texts.push(match token {
            true => &text[usize::from(range.start())..usize::from(range.end())],
            false => "",
        });
        for child in tree.children_with_tokens(at) {
            self.push(tree, text, child);
        }
        self.ends[index] = self.kinds.len() as u32;
    }

    pub(crate) fn tree(&self) -> Tree<'_> {
        Tree::new(&self.kinds, &self.ends, &self.spans, &self.texts)
    }
}

/// The element number of a view or an `ast` node, to compare them by.
trait At {
    fn at(&self) -> u32;
}

impl At for SyntaxNode {
    fn at(&self) -> u32 {
        self.flat().expect("a flat tree").1
    }
}

macro_rules! at_ast {
    ($($ty:ty),*) => {$(
        impl At for $ty {
            fn at(&self) -> u32 {
                self.syntax().at()
            }
        }
    )*};
}

at_ast!(
    ast::Item,
    ast::Struct,
    ast::Enum,
    ast::Union,
    ast::Adt,
    ast::FieldList,
    ast::RecordFieldList,
    ast::RecordField,
    ast::TupleFieldList,
    ast::TupleField,
    ast::VariantList,
    ast::Variant,
    ast::ConstArg,
    ast::Name,
    ast::NameRef,
    ast::Lifetime,
    ast::Visibility,
    ast::VisibilityInner,
    ast::Attr,
    ast::Meta,
    ast::TokenTree,
    ast::Path,
    ast::PathSegment,
    ast::GenericParamList,
    ast::GenericParam,
    ast::TypeParam,
    ast::LifetimeParam,
    ast::ConstParam,
    ast::TypeBoundList,
    ast::WhereClause,
    ast::WherePred,
    ast::GenericArgList,
    ast::GenericArg,
    ast::Type,
    ast::PathType,
    ast::RefType,
    ast::PtrType,
    ast::TupleType,
    ast::ArrayType,
    ast::SliceType
);

macro_rules! at_view {
    ($($ty:ident),*) => {$(
        impl At for $ty<'_> {
            fn at(&self) -> u32 {
                self.syntax().index()
            }
        }
    )*};
}

at_view!(
    Item,
    Struct,
    Enum,
    Union,
    Adt,
    FieldList,
    RecordFieldList,
    RecordField,
    TupleFieldList,
    TupleField,
    VariantList,
    Variant,
    ConstArg,
    Name,
    NameRef,
    Lifetime,
    Visibility,
    VisibilityInner,
    Attr,
    Meta,
    TokenTree,
    Path,
    PathSegment,
    GenericParamList,
    GenericParam,
    TypeParam,
    LifetimeParam,
    ConstParam,
    TypeBoundList,
    WhereClause,
    WherePred,
    GenericArgList,
    GenericArg,
    Type,
    PathType,
    RefType,
    PtrType,
    TupleType,
    ArrayType,
    SliceType,
    SourceFile
);

impl At for ast::SourceFile {
    fn at(&self) -> u32 {
        self.syntax().at()
    }
}

fn at<T: At>(it: Option<T>) -> Option<u32> {
    it.map(|it| it.at())
}

fn all<T: At>(it: impl Iterator<Item = T>) -> Vec<u32> {
    it.map(|it| it.at()).collect()
}

/// Asserts the view and the `ast` node give the same.
macro_rules! same {
    ($what:expr, $view:expr, $ast:expr) => {
        assert_eq!($view, $ast, "{} differs", $what)
    };
}

/// Compares every view that casts `node` with `ast`'s.
fn compare(tree: &Tree<'_>, node: &SyntaxNode, file: &FsPath) {
    let index = node.at();
    let view = tree.node(index).expect("a node");
    let place = || format!("{} at {:?} ({:?})", file.display(), node.text_range(), node.kind());

    macro_rules! casts {
        ($($view:ident = $ast:ident),*) => {$(
            assert_eq!(
                $view::cast(view).is_some(),
                ast::$ast::can_cast(node.kind()),
                "{}: {} casts differently",
                place(),
                stringify!($view)
            );
        )*};
    }
    casts!(
        SourceFile = SourceFile,
        Item = Item,
        Struct = Struct,
        Enum = Enum,
        Union = Union,
        Adt = Adt,
        FieldList = FieldList,
        RecordFieldList = RecordFieldList,
        RecordField = RecordField,
        TupleFieldList = TupleFieldList,
        TupleField = TupleField,
        VariantList = VariantList,
        Variant = Variant,
        ConstArg = ConstArg,
        Name = Name,
        NameRef = NameRef,
        Lifetime = Lifetime,
        Visibility = Visibility,
        VisibilityInner = VisibilityInner,
        Attr = Attr,
        Meta = Meta,
        TokenTree = TokenTree,
        Path = Path,
        PathSegment = PathSegment,
        GenericParamList = GenericParamList,
        GenericParam = GenericParam,
        TypeParam = TypeParam,
        LifetimeParam = LifetimeParam,
        ConstParam = ConstParam,
        TypeBoundList = TypeBoundList,
        WhereClause = WhereClause,
        WherePred = WherePred,
        GenericArgList = GenericArgList,
        GenericArg = GenericArg,
        Type = Type,
        PathType = PathType,
        RefType = RefType,
        PtrType = PtrType,
        TupleType = TupleType,
        ArrayType = ArrayType,
        SliceType = SliceType
    );

    let w = |what: &str| format!("{}: {what}", place());

    if let (Some(v), Some(a)) = (SourceFile::cast(view), ast::SourceFile::cast(node.clone())) {
        same!(w("SourceFile::items"), all(v.items()), all(a.items()));
        same!(w("SourceFile::attrs"), all(v.attrs()), all(a.attrs()));
    }
    if let (Some(v), Some(a)) = (Item::cast(view), ast::Item::cast(node.clone())) {
        same!(w("Item::attrs"), all(v.attrs()), all(a.attrs()));
        let name = ast::AnyHasName::cast(node.clone()).and_then(|it| it.name());
        same!(w("Item::name"), at(v.name()), at(name));
        let vis = ast::AnyHasVisibility::cast(node.clone()).and_then(|it| it.visibility());
        same!(w("Item::visibility"), at(v.visibility()), at(vis));
        let generics = ast::AnyHasGenericParams::cast(node.clone());
        let (params, clause) = match generics {
            Some(it) => (it.generic_param_list(), it.where_clause()),
            None => (None, None),
        };
        same!(w("Item::generic_param_list"), at(v.generic_param_list()), at(params));
        same!(w("Item::where_clause"), at(v.where_clause()), at(clause));
        same!(w("Item::as_struct"), at(v.as_struct()), at(ast::Struct::cast(node.clone())));
        same!(w("Item::as_enum"), at(v.as_enum()), at(ast::Enum::cast(node.clone())));
        same!(w("Item::as_union"), at(v.as_union()), at(ast::Union::cast(node.clone())));
        same!(w("Item::as_adt"), at(v.as_adt()), at(ast::Adt::cast(node.clone())));
    }
    if let (Some(v), Some(a)) = (Adt::cast(view), ast::Adt::cast(node.clone())) {
        same!(w("Adt::name"), at(v.name()), at(a.name()));
        same!(w("Adt::attrs"), all(v.attrs()), all(a.attrs()));
        same!(w("Adt::visibility"), at(v.visibility()), at(a.visibility()));
        same!(w("Adt::generic_param_list"), at(v.generic_param_list()), at(a.generic_param_list()));
        same!(w("Adt::where_clause"), at(v.where_clause()), at(a.where_clause()));
    }
    macro_rules! common {
        ($v:expr, $a:expr, $ty:literal, name) => {
            same!(w(concat!($ty, "::name")), at($v.name()), at($a.name()));
        };
        ($v:expr, $a:expr, $ty:literal, attrs) => {
            same!(w(concat!($ty, "::attrs")), all($v.attrs()), all($a.attrs()));
        };
        ($v:expr, $a:expr, $ty:literal, visibility) => {
            same!(w(concat!($ty, "::visibility")), at($v.visibility()), at($a.visibility()));
        };
        ($v:expr, $a:expr, $ty:literal, generics) => {
            same!(
                w(concat!($ty, "::generic_param_list")),
                at($v.generic_param_list()),
                at($a.generic_param_list())
            );
            same!(w(concat!($ty, "::where_clause")), at($v.where_clause()), at($a.where_clause()));
        };
    }
    if let (Some(v), Some(a)) = (Struct::cast(view), ast::Struct::cast(node.clone())) {
        common!(v, a, "Struct", name);
        common!(v, a, "Struct", attrs);
        common!(v, a, "Struct", visibility);
        common!(v, a, "Struct", generics);
        same!(w("Struct::field_list"), at(v.field_list()), at(a.field_list()));
    }
    if let (Some(v), Some(a)) = (Enum::cast(view), ast::Enum::cast(node.clone())) {
        common!(v, a, "Enum", name);
        common!(v, a, "Enum", attrs);
        common!(v, a, "Enum", visibility);
        common!(v, a, "Enum", generics);
        same!(w("Enum::variant_list"), at(v.variant_list()), at(a.variant_list()));
    }
    if let (Some(v), Some(a)) = (Union::cast(view), ast::Union::cast(node.clone())) {
        common!(v, a, "Union", name);
        common!(v, a, "Union", attrs);
        common!(v, a, "Union", visibility);
        common!(v, a, "Union", generics);
        same!(w("Union::record_field_list"), at(v.record_field_list()), at(a.record_field_list()));
    }
    if let (Some(v), Some(a)) =
        (RecordFieldList::cast(view), ast::RecordFieldList::cast(node.clone()))
    {
        same!(w("RecordFieldList::fields"), all(v.fields()), all(a.fields()));
    }
    if let (Some(v), Some(a)) =
        (TupleFieldList::cast(view), ast::TupleFieldList::cast(node.clone()))
    {
        same!(w("TupleFieldList::fields"), all(v.fields()), all(a.fields()));
    }
    if let (Some(v), Some(a)) = (RecordField::cast(view), ast::RecordField::cast(node.clone())) {
        common!(v, a, "RecordField", name);
        common!(v, a, "RecordField", attrs);
        common!(v, a, "RecordField", visibility);
        same!(w("RecordField::ty"), at(v.ty()), at(a.ty()));
        same!(w("RecordField::default_val"), at(v.default_val()), at(a.default_val()));
    }
    if let (Some(v), Some(a)) = (TupleField::cast(view), ast::TupleField::cast(node.clone())) {
        common!(v, a, "TupleField", attrs);
        common!(v, a, "TupleField", visibility);
        same!(w("TupleField::ty"), at(v.ty()), at(a.ty()));
    }
    if let (Some(v), Some(a)) = (VariantList::cast(view), ast::VariantList::cast(node.clone())) {
        same!(w("VariantList::variants"), all(v.variants()), all(a.variants()));
    }
    if let (Some(v), Some(a)) = (Variant::cast(view), ast::Variant::cast(node.clone())) {
        common!(v, a, "Variant", name);
        common!(v, a, "Variant", attrs);
        common!(v, a, "Variant", visibility);
        same!(w("Variant::field_list"), at(v.field_list()), at(a.field_list()));
        same!(w("Variant::const_arg"), at(v.const_arg()), at(a.const_arg()));
    }
    if let (Some(v), Some(a)) = (Name::cast(view), ast::Name::cast(node.clone())) {
        same!(w("Name::text"), v.text(), a.text());
        assert!(v.is(a.text()), "{}", w("Name::is"));
    }
    if let (Some(v), Some(a)) = (NameRef::cast(view), ast::NameRef::cast(node.clone())) {
        same!(w("NameRef::text"), v.text(), a.text());
    }
    if let (Some(v), Some(a)) = (Lifetime::cast(view), ast::Lifetime::cast(node.clone())) {
        let text = a.lifetime_ident_token().map(|it| String::from(it.text()));
        same!(w("Lifetime::text"), Some(String::from(v.text())).filter(|it| !it.is_empty()), text);
    }
    if let (Some(v), Some(a)) = (Visibility::cast(view), ast::Visibility::cast(node.clone())) {
        same!(
            w("Visibility::visibility_inner"),
            at(v.visibility_inner()),
            at(a.visibility_inner())
        );
    }
    if let (Some(v), Some(a)) =
        (VisibilityInner::cast(view), ast::VisibilityInner::cast(node.clone()))
    {
        same!(w("VisibilityInner::path"), at(v.path()), at(a.path()));
    }
    if let (Some(v), Some(a)) = (Attr::cast(view), ast::Attr::cast(node.clone())) {
        same!(w("Attr::meta"), at(v.meta()), at(a.meta()));
        same!(w("Attr::is_inner"), v.is_inner(), a.kind().is_inner());
        same!(w("Attr::path"), at(v.path()), at(a.path()));
        same!(w("Attr::simple_name"), v.simple_name(), a.simple_name().as_deref());
        let call = a.as_simple_call();
        same!(
            w("Attr::as_simple_call"),
            v.as_simple_call().map(|(name, tt)| (String::from(name), tt.at())),
            call.map(|(name, tt)| (String::from(name.as_str()), tt.at()))
        );
    }
    if let (Some(v), Some(a)) = (Meta::cast(view), ast::Meta::cast(node.clone())) {
        same!(w("Meta::path"), at(v.path()), at(a.path()));
        same!(w("Meta::is_cfg_attr"), v.is_cfg_attr(), matches!(a, ast::Meta::CfgAttrMeta(_)));
        same!(w("Meta::is_cfg"), v.is_cfg(), matches!(a, ast::Meta::CfgMeta(_)));
        same!(w("Meta::is_key_value"), v.is_key_value(), matches!(a, ast::Meta::KeyValueMeta(_)));
        let inner = match &a {
            ast::Meta::UnsafeMeta(it) => it.meta(),
            _ => None,
        };
        same!(w("Meta::unsafe_meta"), at(v.unsafe_meta()), at(inner));
        let tt = match &a {
            ast::Meta::TokenTreeMeta(it) => it.token_tree(),
            _ => None,
        };
        same!(w("Meta::token_tree"), at(v.token_tree()), at(tt));
        same!(w("Meta::as_simple_path"), at(v.as_simple_path()), at(a.as_simple_path()));
        same!(w("Meta::as_simple_atom"), v.as_simple_atom(), a.as_simple_atom().as_deref());
        same!(w("Meta::simple_name"), v.simple_name(), a.simple_name().as_deref());
    }
    if let (Some(v), Some(a)) = (Path::cast(view), ast::Path::cast(node.clone())) {
        same!(w("Path::qualifier"), at(v.qualifier()), at(a.qualifier()));
        same!(w("Path::segment"), at(v.segment()), at(a.segment()));
        same!(
            w("Path::as_single_name_ref"),
            at(v.as_single_name_ref()),
            at(a.as_single_name_ref())
        );
    }
    if let (Some(v), Some(a)) = (PathSegment::cast(view), ast::PathSegment::cast(node.clone())) {
        same!(w("PathSegment::name_ref"), at(v.name_ref()), at(a.name_ref()));
        same!(
            w("PathSegment::generic_arg_list"),
            at(v.generic_arg_list()),
            at(a.generic_arg_list())
        );
    }
    if let (Some(v), Some(a)) =
        (GenericParamList::cast(view), ast::GenericParamList::cast(node.clone()))
    {
        same!(
            w("GenericParamList::generic_params"),
            all(v.generic_params()),
            all(a.generic_params())
        );
    }
    if let (Some(v), Some(a)) = (GenericParam::cast(view), ast::GenericParam::cast(node.clone())) {
        let kind = |v: &GenericParam<'_>| match v {
            GenericParam::ConstParam(_) => 0,
            GenericParam::LifetimeParam(_) => 1,
            GenericParam::TypeParam(_) => 2,
        };
        let ast_kind = match a {
            ast::GenericParam::ConstParam(_) => 0,
            ast::GenericParam::LifetimeParam(_) => 1,
            ast::GenericParam::TypeParam(_) => 2,
        };
        same!(w("GenericParam"), kind(&v), ast_kind);
    }
    if let (Some(v), Some(a)) = (TypeParam::cast(view), ast::TypeParam::cast(node.clone())) {
        common!(v, a, "TypeParam", name);
        common!(v, a, "TypeParam", attrs);
        same!(w("TypeParam::type_bound_list"), at(v.type_bound_list()), at(a.type_bound_list()));
        same!(w("TypeParam::default_type"), at(v.default_type()), at(a.default_type()));
    }
    if let (Some(v), Some(a)) = (LifetimeParam::cast(view), ast::LifetimeParam::cast(node.clone()))
    {
        common!(v, a, "LifetimeParam", attrs);
        same!(w("LifetimeParam::lifetime"), at(v.lifetime()), at(a.lifetime()));
        same!(
            w("LifetimeParam::type_bound_list"),
            at(v.type_bound_list()),
            at(a.type_bound_list())
        );
    }
    if let (Some(v), Some(a)) = (ConstParam::cast(view), ast::ConstParam::cast(node.clone())) {
        common!(v, a, "ConstParam", name);
        common!(v, a, "ConstParam", attrs);
        same!(w("ConstParam::ty"), at(v.ty()), at(a.ty()));
        same!(w("ConstParam::default_val"), at(v.default_val()), at(a.default_val()));
    }
    if let (Some(v), Some(a)) = (WhereClause::cast(view), ast::WhereClause::cast(node.clone())) {
        same!(w("WhereClause::predicates"), all(v.predicates()), all(a.predicates()));
    }
    if let (Some(v), Some(a)) = (WherePred::cast(view), ast::WherePred::cast(node.clone())) {
        same!(w("WherePred::ty"), at(v.ty()), at(a.ty()));
        same!(w("WherePred::lifetime"), at(v.lifetime()), at(a.lifetime()));
        same!(w("WherePred::type_bound_list"), at(v.type_bound_list()), at(a.type_bound_list()));
    }
    if let (Some(v), Some(a)) =
        (GenericArgList::cast(view), ast::GenericArgList::cast(node.clone()))
    {
        same!(w("GenericArgList::generic_args"), all(v.generic_args()), all(a.generic_args()));
    }
    if let (Some(v), Some(a)) = (GenericArg::cast(view), ast::GenericArg::cast(node.clone())) {
        let (ty, lifetime, konst) = match &a {
            ast::GenericArg::TypeArg(it) => (it.ty(), None, None),
            ast::GenericArg::LifetimeArg(it) => (None, it.lifetime(), None),
            ast::GenericArg::ConstArg(it) => (None, None, Some(it.clone())),
            ast::GenericArg::AssocTypeArg(_) => (None, None, None),
        };
        same!(w("GenericArg::as_type"), at(v.as_type()), at(ty));
        same!(w("GenericArg::as_lifetime"), at(v.as_lifetime()), at(lifetime));
        same!(w("GenericArg::as_const"), at(v.as_const()), at(konst));
    }
    if let (Some(v), Some(a)) = (Type::cast(view), ast::Type::cast(node.clone())) {
        same!(w("Type::as_path"), at(v.as_path()), at(ast::PathType::cast(a.syntax().clone())));
        same!(w("Type::as_ref"), at(v.as_ref()), at(ast::RefType::cast(a.syntax().clone())));
        same!(w("Type::as_ptr"), at(v.as_ptr()), at(ast::PtrType::cast(a.syntax().clone())));
        same!(w("Type::as_tuple"), at(v.as_tuple()), at(ast::TupleType::cast(a.syntax().clone())));
        same!(w("Type::as_array"), at(v.as_array()), at(ast::ArrayType::cast(a.syntax().clone())));
        same!(w("Type::as_slice"), at(v.as_slice()), at(ast::SliceType::cast(a.syntax().clone())));
        let tokens: Vec<_> = v.syntax().tokens().map(|it| it.text()).collect();
        let expected: Vec<_> = a
            .syntax()
            .descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .filter(|it| !it.kind().is_trivia())
            .map(|it| String::from(it.text()))
            .collect();
        same!(w("Node::tokens"), tokens, expected);
    }
    if let (Some(v), Some(a)) = (PathType::cast(view), ast::PathType::cast(node.clone())) {
        same!(w("PathType::path"), at(v.path()), at(a.path()));
    }
    if let (Some(v), Some(a)) = (RefType::cast(view), ast::RefType::cast(node.clone())) {
        same!(w("RefType::ty"), at(v.ty()), at(a.ty()));
        same!(w("RefType::lifetime"), at(v.lifetime()), at(a.lifetime()));
        same!(w("RefType::is_mut"), v.is_mut(), a.mut_token().is_some());
    }
    if let (Some(v), Some(a)) = (PtrType::cast(view), ast::PtrType::cast(node.clone())) {
        same!(w("PtrType::ty"), at(v.ty()), at(a.ty()));
        same!(w("PtrType::is_mut"), v.is_mut(), a.mut_token().is_some());
        same!(w("PtrType::is_const"), v.is_const(), a.const_token().is_some());
    }
    if let (Some(v), Some(a)) = (TupleType::cast(view), ast::TupleType::cast(node.clone())) {
        same!(w("TupleType::fields"), all(v.fields()), all(a.fields()));
        let nth: Vec<_> = (0..).map_while(|n| v.field(n)).map(|it| it.at()).collect();
        same!(w("TupleType::field"), nth, all(a.fields()));
    }
    if let (Some(v), Some(a)) = (ArrayType::cast(view), ast::ArrayType::cast(node.clone())) {
        same!(w("ArrayType::ty"), at(v.ty()), at(a.ty()));
        same!(w("ArrayType::const_arg"), at(v.const_arg()), at(a.const_arg()));
    }
    if let (Some(v), Some(a)) = (SliceType::cast(view), ast::SliceType::cast(node.clone())) {
        same!(w("SliceType::ty"), at(v.ty()), at(a.ty()));
    }
    same!(w("Node::len"), view.len(), node.flat().map(|(t, at)| t.end(at) - at).unwrap());
    same!(w("Node::children"), all(view.children().map(|it| it.index())), all(node.children()));
}

impl At for u32 {
    fn at(&self) -> u32 {
        *self
    }
}

/// The `.rs` files under `dir`, sorted.
fn rust_files(dir: &FsPath, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|it| it.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// Real crates: the standard library's `core` and `alloc` (every kind of item, attribute and
/// generic there is), the query engine (derives and generic code) and this crate.
fn corpus() -> Vec<PathBuf> {
    let root = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for dir in
        ["library/core/src", "library/alloc/src", "crates/blaze-query/src", "crates/const-ast/src"]
    {
        rust_files(&root.join(dir), &mut files);
    }
    assert!(files.len() > 300, "the corpus is {} files", files.len());
    files
}

fn flat(node: &SyntaxNode) -> Arc<FlatTree> {
    match node.flat() {
        Some((tree, 0)) => tree.clone(),
        _ => node.to_flat().flat().expect("a flat tree").0.clone(),
    }
}

#[test]
fn the_views_are_syntax_asts() {
    let mut nodes = 0usize;
    let files = corpus();
    for file in &files {
        let text =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let root = AstFile::parse(&text, Edition::CURRENT).syntax_node();
        let root = match root.flat() {
            Some(_) => root,
            None => root.to_flat(),
        };
        let arrays = Arrays::of(&flat(&root));
        let tree = arrays.tree();
        for node in root.descendants() {
            compare(&tree, &node, file);
            nodes += 1;
        }
        let item = tree.item().map(|it| it.syntax().index());
        let first = ast::SourceFile::cast(root.clone()).and_then(|it| it.items().next());
        assert_eq!(item, at(first), "{}: Tree::item", file.display());
    }
    std::eprintln!("{} files, {nodes} nodes", files.len());
}

#[test]
fn an_outline_has_the_arrays_of_a_full_parse() {
    let mut lazies = 0usize;
    for file in corpus() {
        let text: Arc<str> = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
            .into();
        let full = AstFile::parse_shared(text.clone(), Edition::CURRENT);
        let outline = AstFile::parse_outline(text, Edition::CURRENT);
        let tree = outline.root().shared_file();
        let unparsed = tree.lazies().filter(|&at| !tree.is_parsed(at)).count();
        assert_eq!(unparsed, tree.lazies().count(), "{}: bodies parsed early", file.display());
        lazies += unparsed;
        let full = full.root().shared_file();
        assert!(Arrays::of(tree) == Arrays::of(full), "{}", file.display());
    }
    assert!(lazies > 1000, "the corpus's outlines have {lazies} lazy bodies");
}

#[test]
fn a_derive_reads_a_struct() {
    let text =
        "#[derive(Debug)] // a comment\npub struct A<'a, T: Copy = u8> { x: &'a T, y: [u8; 4] }";
    let parse = AstFile::parse(text, Edition::CURRENT);
    let arrays = Arrays::of(parse.root().shared_file());
    let tree = arrays.tree();
    let strukt = tree.item().and_then(|it| it.as_struct()).expect("a struct");
    assert!(strukt.name().is_some_and(|it| it.is("A")));
    let derive = strukt.attrs().next().expect("an attribute");
    assert_eq!(derive.simple_name(), Some("derive"));
    let (_, args) = derive.as_simple_call().expect("derive(..)");
    let args: Vec<_> = args.syntax().tokens().map(|it| it.text()).collect();
    assert_eq!(args, ["(", "Debug", ")"]);
    let Some(FieldList::RecordFieldList(fields)) = strukt.field_list() else { panic!() };
    let fields: Vec<_> = fields
        .fields()
        .map(|it| {
            let ty: Vec<_> = it.ty().unwrap().syntax().tokens().map(|it| it.text()).collect();
            (it.name().unwrap().text(), ty.concat())
        })
        .collect();
    assert_eq!(fields, [("x", String::from("&'aT")), ("y", String::from("[u8;4]"))]);
    let name = strukt.name().and_then(|it| it.token()).expect("a name token");
    assert_eq!(name.span().index(), name.index(), "the test's spans are the element numbers");
}

/// The numbers of `kinds.rs` are `SyntaxKind`'s, and its token kinds have [`TOKEN`].
#[test]
fn kinds_are_the_grammars() {
    let by_name: HashMap<String, SyntaxKind> = (0..SyntaxKind::__LAST as u16)
        .map(SyntaxKind::from)
        .map(|it| (format!("{it:?}"), it))
        .collect();
    // Tokens come first in the grammar's numbering, then nodes from `ABI` on.
    let first_node = SyntaxKind::ABI as u16;
    let path = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("src/kinds.rs");
    let text = std::fs::read_to_string(&path).expect("kinds.rs");
    let mut wrong = Vec::new();
    let mut fixed = String::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("pub(crate) const ") else {
            fixed += line;
            fixed += "\n";
            continue;
        };
        let (name, _) = rest.split_once(':').expect("NAME: u16 = ..");
        let kind = *by_name.get(name).unwrap_or_else(|| panic!("no SyntaxKind::{name}"));
        let number = kind as u16;
        // A kind is a token's when it is before `ABI` or has a fixed text: the grammar numbers
        // a few contextual keywords after its nodes.
        let token = if number < first_node || kind.fixed_text().is_some() { " | TOKEN" } else { "" };
        let want = format!("pub(crate) const {name}: u16 = {number}{token};");
        if line != want {
            wrong.push(format!("{line}  (the grammar's: {want})"));
        }
        fixed += &want;
        fixed += "\n";
    }
    if !wrong.is_empty() {
        if std::env::var_os("UPDATE_EXPECT").is_some() {
            std::fs::write(&path, fixed).expect("writing kinds.rs");
        } else {
            panic!(
                "kinds.rs is not the grammar's (UPDATE_EXPECT=1 writes it):\n{}",
                wrong.join("\n")
            );
        }
    }
}

// --- The builder ---------------------------------------------------------------------------

/// Copies element `at` of `arrays` and its subtree into `b`, element by element: a node is
/// `open` and `close` around its children, a token is `token_at` with its text and span.
fn copy(arrays: &Arrays, at: usize, b: &mut Builder<'_>) {
    let kind = arrays.kinds[at];
    if kind & TOKEN != 0 {
        b.token_at(arrays.texts[at], Span(arrays.spans[at]));
        return;
    }
    let open = b.open(Kind(kind));
    let end = arrays.ends[at] as usize;
    let mut child = at + 1;
    while child < end {
        copy(arrays, child, b);
        child = arrays.ends[child] as usize;
    }
    b.close(open);
}

/// Built arrays, owned, to compare (and, for the control, to break).
struct Rebuilt {
    kinds: Vec<u16>,
    sizes: Vec<u32>,
    spans: Vec<u32>,
    texts: Vec<String>,
}

/// Element `at` of `arrays` and its subtree, rebuilt through `Builder::over` into arrays just
/// as long.
fn rebuild(arrays: &Arrays, at: usize) -> Rebuilt {
    let n = (arrays.ends[at] as usize) - at;
    // alloc: a test's buffers, one set per item rebuilt
    let (mut kinds, mut sizes, mut spans) = (vec![0u16; n], vec![0u32; n], vec![0u32; n]);
    // alloc: as above
    let mut texts: Vec<&str> = vec![""; n];
    let built = {
        let mut b = Builder::over(&mut kinds, &mut sizes, &mut spans, &mut texts, Span::call_site());
        copy(arrays, at, &mut b);
        assert_eq!(b.len() as usize, n, "the builder wrote one element per element");
        b.finish(Entry::Items)
    };
    assert_eq!(built.entry() as u8, Entry::Items as u8);
    // alloc: owned, past the builder's buffers
    let texts = built.texts().iter().map(|text| String::from(*text)).collect();
    Rebuilt {
        kinds: built.kinds().to_vec(), // alloc: owned, so the control can break one
        sizes: built.sizes().to_vec(), // alloc: as above
        spans: built.spans().to_vec(), // alloc: as above
        texts,
    }
}

/// Whether `built` is element `at` of `arrays` and its subtree: a node's kind, a token as
/// [`TOKEN`] (its kind is the host's to find), each size `ends[i] - i`, each span and text.
/// The first difference is named by the built element's number.
fn same_tree(built: &Rebuilt, arrays: &Arrays, at: usize) -> Result<(), String> {
    let n = (arrays.ends[at] as usize) - at;
    if built.kinds.len() != n {
        return Err(format!("built {} elements where the tree has {n}", built.kinds.len()));
    }
    for i in 0..n {
        let j = at + i;
        let token = arrays.kinds[j] & TOKEN != 0;
        let kind = if token { TOKEN } else { arrays.kinds[j] };
        let size = arrays.ends[j] - j as u32;
        let span = arrays.spans[j];
        let text = arrays.texts[j];
        let differs = |what: &str, got: String, want: String| {
            Err(format!("element {i} (the tree's {j}): its {what} is {got}, the tree's {want}"))
        };
        if built.kinds[i] != kind {
            return differs("kind", built.kinds[i].to_string(), kind.to_string());
        }
        if built.sizes[i] != size {
            return differs("size", built.sizes[i].to_string(), size.to_string());
        }
        if built.spans[i] != span {
            return differs("span", built.spans[i].to_string(), span.to_string());
        }
        if built.texts[i] != text {
            return differs("text", format!("{:?}", built.texts[i]), format!("{text:?}"));
        }
    }
    Ok(())
}

/// The refinement: a tree copied element by element through `Builder::over` (open, token_at,
/// close) has the kinds, sizes (`ends[i] - i`), spans and texts of the tree it was copied
/// from, for every item of every file of the corpus `the_views_are_syntax_asts` walks: the
/// builder can write every tree the parser makes, and sizes are relative, so an item rebuilt
/// alone is the same run of elements it is inside its file. The control is
/// `the_rebuild_check_catches_a_size_off_by_one`.
#[test]
fn every_item_rebuilt_is_the_parsers() {
    let mut items = 0usize;
    let mut elements = 0usize;
    for file in corpus() {
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let root = AstFile::parse(&text, Edition::CURRENT).syntax_node();
        let root = match root.flat() {
            Some(_) => root,
            None => root.to_flat(),
        };
        let arrays = Arrays::of(&flat(&root));
        let mut at = 1usize;
        while at < arrays.kinds.len() {
            if arrays.kinds[at] & TOKEN == 0 {
                let rebuilt = rebuild(&arrays, at);
                if let Err(e) = same_tree(&rebuilt, &arrays, at) {
                    panic!("{}: the item at element {at}: {e}", file.display());
                }
                items += 1;
                elements += rebuilt.kinds.len();
            }
            at = arrays.ends[at] as usize;
        }
    }
    assert!(items > 1_000, "the corpus has {items} items");
    std::eprintln!("{items} items, {elements} elements rebuilt");
}

/// The planted bug for `every_item_rebuilt_is_the_parsers`: a rebuilt tree with one size one
/// too big is not the parser's, and the compare names that element.
#[test]
fn the_rebuild_check_catches_a_size_off_by_one() {
    let text = "pub struct A<T> { x: T, y: [u8; 4] }";
    let arrays = Arrays::of(AstFile::parse(text, Edition::CURRENT).root().shared_file());
    let at = 1;
    assert_eq!(arrays.kinds[at], kinds::STRUCT, "element 1 is the struct");
    let mut rebuilt = rebuild(&arrays, at);
    assert_eq!(same_tree(&rebuilt, &arrays, at), Ok(()), "the rebuild before the bug");
    let broken = rebuilt.kinds.len() / 2;
    rebuilt.sizes[broken] += 1;
    let error = same_tree(&rebuilt, &arrays, at).expect_err("an off-by-one size");
    assert!(error.starts_with(&format!("element {broken} ")), "{error}");
    assert!(error.contains("its size is"), "{error}");
}

/// The invariant: a splice is one element, `SPLICE` with the input's number as its kind, size
/// 1, and the spliced element's number as its span; a token's splice inside `wrap` is the
/// wrapping nodes, outer to inner, each holding the rest. The control is the same splices of
/// input 0, whose kinds differ.
#[test]
fn a_splice_is_one_element_naming_the_input() {
    let text = "struct Point { x: i32 }";
    let arrays = Arrays::of(AstFile::parse(text, Edition::CURRENT).root().shared_file());
    let spliced = |input: u32| {
        let tree = Tree { input, ..arrays.tree() };
        let strukt = tree.item().and_then(|it| it.as_struct()).expect("a struct");
        let name = strukt.name().expect("a name");
        let token = name.token().expect("a name token");
        let (mut kinds, mut sizes, mut spans) = ([0u16; 5], [0u32; 5], [0u32; 5]);
        let mut texts = [""; 5];
        let mut b = Builder::over(&mut kinds, &mut sizes, &mut spans, &mut texts, Span::mixed_site());
        strukt.splice(&mut b, &[]);
        b.tok(token);
        name.splice(&mut b, &[Kind::PATH, Kind::PATH_SEGMENT]);
        let built = b.finish(Entry::Items);
        let out = (
            built.kinds().to_vec(), // alloc: compared below
            built.sizes().to_vec(), // alloc: as above
            built.spans().to_vec(), // alloc: as above
            built.texts().iter().all(|it| it.is_empty()),
        );
        (out, strukt.syntax().index(), token.index())
    };
    let ((kinds, sizes, spans, no_texts), strukt, token) = spliced(3);
    assert_eq!(kinds, [SPLICE | 3, SPLICE | 3, kinds::PATH, kinds::PATH_SEGMENT, SPLICE | 3]);
    assert_eq!(sizes, [1, 1, 3, 2, 1]);
    assert_eq!(spans, [strukt, token, NO_SPAN, NO_SPAN, token]);
    assert!(no_texts, "a splice and a node have no text");
    let ((control, ..), ..) = spliced(0);
    assert_eq!(control[0], SPLICE, "input 0's splice");
    assert_ne!(control, kinds, "the input's number is in the kind");
}

/// The node kinds of the grammar (from `ABI`, without the contextual keywords it numbers after
/// its nodes) that `kinds.rs` or `kinds!(..)` lacks, and the names in `kinds!(..)` that are no
/// node kind.
fn kinds_not_public(kinds_rs: &str, names: &[&str]) -> Vec<String> {
    let nodes: Vec<String> = (SyntaxKind::ABI as u16..SyntaxKind::__LAST as u16)
        .map(SyntaxKind::from)
        .filter(|it| it.fixed_text().is_none())
        .map(|it| format!("{it:?}"))
        .collect();
    let in_file: Vec<&str> = kinds_rs
        .lines()
        .filter_map(|line| line.strip_prefix("pub(crate) const "))
        .filter(|rest| !rest.contains("| TOKEN"))
        .filter_map(|rest| rest.split_once(':').map(|(name, _)| name))
        .collect();
    let mut wrong = Vec::new();
    for node in &nodes {
        if !in_file.contains(&node.as_str()) {
            wrong.push(format!("{node} has no line in kinds.rs"));
        }
        if !names.contains(&node.as_str()) {
            wrong.push(format!("{node} is not in kinds!(..)"));
        }
    }
    for name in names {
        if !nodes.iter().any(|it| it == name) {
            wrong.push(format!("kinds!(..) names {name}, which is no node kind"));
        }
    }
    wrong
}

/// The invariant: every node kind of the grammar is a `Kind` constant (so a macro can open
/// any node the parser makes) and has its number in `kinds.rs`, and `Kind` names nothing
/// else. The control: the list without one name is caught, naming it.
#[test]
fn every_node_kind_is_public() {
    let path = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("src/kinds.rs");
    let text = std::fs::read_to_string(&path).expect("kinds.rs");
    let wrong = kinds_not_public(&text, crate::builder::KIND_NAMES);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    let short: Vec<&str> =
        crate::builder::KIND_NAMES.iter().copied().filter(|it| *it != "IMPL").collect();
    assert_eq!(kinds_not_public(&text, &short), ["IMPL is not in kinds!(..)"]);
}

/// The invariant: `close` closes only the innermost open node.
#[test]
#[should_panic(expected = "const_ast::Builder::close: the node is not the innermost open one")]
fn a_close_out_of_order_panics() {
    let (mut kinds, mut sizes, mut spans) = ([0u16; 4], [0u32; 4], [0u32; 4]);
    let mut texts = [""; 4];
    let mut b = Builder::over(&mut kinds, &mut sizes, &mut spans, &mut texts, Span::call_site());
    let outer = b.open(Kind::IMPL);
    let _inner = b.open(Kind::ASSOC_ITEM_LIST);
    b.close(outer);
}

/// The invariant: a fixed builder never writes past its arrays, and says how many they hold.
#[test]
#[should_panic(expected = "const_ast::Builder: the output has more than 2 elements")]
fn a_full_fixed_builder_panics_naming_its_size() {
    let (mut kinds, mut sizes, mut spans) = ([0u16; 2], [0u32; 2], [0u32; 2]);
    let mut texts = [""; 2];
    let mut b = Builder::over(&mut kinds, &mut sizes, &mut spans, &mut texts, Span::call_site());
    b.token("a");
    b.token("b");
    b.token("c");
}
