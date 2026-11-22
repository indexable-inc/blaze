//! The struct, enum or union a builtin derive is of, read from the item's tokens: its name,
//! generic parameters, `where` predicates, variants and fields, and the `T::Assoc` path types of
//! its field types, each as indices into the item's stream. Nothing is copied: the derive
//! writer copies the tokens it names.
//!
//! Every element is the parser's (extent.rs [`crate::extent::item`], one parse of the item):
//! a field, variant, parameter or predicate ends where the grammar ends it, never at a comma
//! counted outside `<..>`, so `T = HashMap<K, V>`, `f: u8 = g::<A, B>()` and `A = 1 << 2` are
//! each one element. Malformed input is a typed refusal ([`NotAnAdt::Syntax`]) naming the
//! element, never a silent cut.

use core::ops::Range;

use blaze_own::{AVec, Alloc, Vec};
use parser::SyntaxKind;

use crate::{
    extent::{ExtentError, Tree},
    tokens::{Delim, Raw, Spacing, TokenKind, TokenStream},
};

/// What a derive is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AdtKind {
    Struct,
    /// `default_variant` is the first variant marked `#[default]`, by index.
    Enum {
        default_variant: Option<usize>,
    },
    Union,
}

/// How a struct's or a variant's fields are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FieldsKind {
    Record,
    Tuple,
    Unit,
}

/// One field: its name token (a record field's) and its position in its variant, and the
/// tokens of its type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Field {
    pub(crate) name: Option<usize>,
    pub(crate) index: usize,
    pub(crate) ty: Range<usize>,
    /// The field declares a default value (`a: u8 = 1`, RFC 3681): `derive(Default)` takes it.
    pub(crate) defaulted: bool,
}

/// A struct's fields or an enum's variant: `fields` indexes [`Adt::fields`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Variant {
    /// The variant's name token; the type's for a struct.
    pub(crate) name: usize,
    pub(crate) kind: FieldsKind,
    pub(crate) fields: Range<usize>,
}

impl Variant {
    pub(crate) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// What a generic parameter is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ParamKind {
    Lifetime,
    /// A type parameter and its bounds, if it writes any.
    Type {
        bounds: Option<Range<usize>>,
    },
    /// A const parameter and its type.
    Const {
        ty: Range<usize>,
    },
}

/// One generic parameter: its declaration without attributes (a lifetime's with its bounds),
/// its name (a lifetime's `'` and name, two tokens) and its kind.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Param {
    pub(crate) decl: Range<usize>,
    pub(crate) name: Range<usize>,
    pub(crate) kind: ParamKind,
}

/// The item a derive is of, by token indices of its stream.
pub(crate) struct Adt<'a> {
    pub(crate) name: usize,
    pub(crate) kind: AdtKind,
    /// One for a struct, none for a union.
    pub(crate) variants: AVec<'a, Variant>,
    /// The fields of every variant, a variant's together; a union's.
    pub(crate) fields: AVec<'a, Field>,
    pub(crate) params: AVec<'a, Param>,
    /// Each `where` predicate, without its comma.
    pub(crate) predicates: AVec<'a, Range<usize>>,
    /// Each path type of a field's type with two segments or more whose first is a type
    /// parameter (`T::Assoc`, `T::A::B`): what rustc's derives bound by the trait too
    /// (rustc_builtin_macros `find_type_parameters`), in preorder.
    pub(crate) assoc: AVec<'a, Range<usize>>,
    /// `#[repr(packed)]` (`packed(N)` too) or `#[blaze::bits]`: the fields may have no
    /// address of their own, so they are copied out to be used.
    pub(crate) packed: bool,
}

impl Adt<'_> {
    pub(crate) fn is_enum(&self) -> bool {
        matches!(self.kind, AdtKind::Enum { .. })
    }

    pub(crate) fn packed_struct(&self) -> bool {
        self.packed && self.kind == AdtKind::Struct
    }

    pub(crate) fn is_empty_enum(&self) -> bool {
        self.is_enum() && self.variants.is_empty()
    }
}

/// Why the item is not one a derive can be of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotAnAdt {
    /// The item is not a struct, an enum or a union.
    WrongKind,
    /// The item, or a variant of it, has no name.
    NoName,
    /// A field, variant or parameter the parser took has a part missing.
    Malformed,
    /// The item does not parse: the element and token it stops at.
    Syntax(ExtentError),
}

impl NotAnAdt {
    pub(crate) fn message(self) -> &'static str {
        match self {
            NotAnAdt::WrongKind => "expected struct, enum or union",
            NotAnAdt::NoName => "missing name",
            NotAnAdt::Malformed => "the item does not read as a struct, an enum or a union",
            NotAnAdt::Syntax(error) => error.what.refused(),
        }
    }
}

/// Reads the derive's item, `stream`, in `alloc`.
pub(crate) fn read<'a>(stream: &TokenStream<'_>, alloc: Alloc<'a>) -> Result<Adt<'a>, NotAnAdt> {
    let tree = crate::extent::item(stream, alloc).map_err(NotAnAdt::Syntax)?;
    let r = Reader { stream, tree: &tree };
    let root = 0;
    let kind = match tree.kind(root) {
        SyntaxKind::STRUCT => AdtKind::Struct,
        SyntaxKind::ENUM => AdtKind::Enum { default_variant: None },
        SyntaxKind::UNION => AdtKind::Union,
        _ => return Err(NotAnAdt::WrongKind),
    };
    let name = tree.token(tree.child(root, SyntaxKind::NAME).ok_or(NotAnAdt::NoName)?);
    let mut adt = Adt {
        name,
        kind,
        variants: Vec::new_in(alloc),
        fields: Vec::new_in(alloc),
        params: Vec::new_in(alloc),
        predicates: Vec::new_in(alloc),
        assoc: Vec::new_in(alloc),
        packed: tree.children(root).filter(|&c| tree.kind(c) == SyntaxKind::ATTR).any(|attr| r.is_packed(attr)),
    };
    if let Some(list) = tree.child(root, SyntaxKind::GENERIC_PARAM_LIST) {
        for param in tree.children(list) {
            if let Some(param) = r.param(param)? {
                adt.params.push(param);
            }
        }
    }
    for clause in tree.children(root).filter(|&c| tree.kind(c) == SyntaxKind::WHERE_CLAUSE) {
        for predicate in tree.children(clause).filter(|&c| tree.kind(c) == SyntaxKind::WHERE_PRED) {
            adt.predicates.push(tree.range(predicate));
        }
    }
    match kind {
        AdtKind::Struct => {
            let fields = r.fields(root, &mut adt.fields, &adt.params, &mut adt.assoc)?;
            adt.variants.push(Variant { name, kind: fields, fields: 0..adt.fields.len() });
        }
        AdtKind::Union => {
            r.fields(root, &mut adt.fields, &adt.params, &mut adt.assoc)?;
        }
        AdtKind::Enum { .. } => {
            let list = tree.child(root, SyntaxKind::VARIANT_LIST).ok_or(NotAnAdt::Malformed)?;
            for variant in tree.children(list).filter(|&c| tree.kind(c) == SyntaxKind::VARIANT) {
                let name = tree.token(tree.child(variant, SyntaxKind::NAME).ok_or(NotAnAdt::NoName)?);
                let start = adt.fields.len();
                let fields = r.fields(variant, &mut adt.fields, &adt.params, &mut adt.assoc)?;
                if r.is_default(variant)
                    && let AdtKind::Enum { default_variant } = &mut adt.kind
                    && default_variant.is_none()
                {
                    *default_variant = Some(adt.variants.len());
                }
                adt.variants.push(Variant { name, kind: fields, fields: start..adt.fields.len() });
            }
        }
    }
    Ok(adt)
}

struct Reader<'s, 't> {
    stream: &'s TokenStream<'s>,
    tree: &'s Tree<'t>,
}

impl Reader<'_, '_> {
    /// The keyword or plain name `word` at `at` (not `r#word`).
    fn word(&self, at: usize, word: &str) -> bool {
        matches!(self.stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Ident { raw: Raw::No, .. }))
            && self.stream.text_at(at) == word
    }

    /// The node's first child node (not token) that is none of `not`: a field's or a const
    /// parameter's type.
    fn other_child(&self, node: usize, not: &[SyntaxKind]) -> Option<usize> {
        let t = self.tree;
        t.children(node).find(|&c| !t.is_token(c) && !not.contains(&t.kind(c)))
    }

    /// The fields of `owner` (the item, or a variant): its record or tuple field list, or none;
    /// each field type's path types bounded as rustc's derives bound them to `assoc`
    /// ([`Reader::assoc_types`]), `params` read already.
    fn fields(
        &self,
        owner: usize,
        fields: &mut AVec<'_, Field>,
        params: &[Param],
        assoc: &mut AVec<'_, Range<usize>>,
    ) -> Result<FieldsKind, NotAnAdt> {
        let t = self.tree;
        let (list, kind) = match (t.child(owner, SyntaxKind::RECORD_FIELD_LIST), t.child(owner, SyntaxKind::TUPLE_FIELD_LIST)) {
            (Some(list), _) => (list, FieldsKind::Record),
            (None, Some(list)) => (list, FieldsKind::Tuple),
            (None, None) => return Ok(FieldsKind::Unit),
        };
        let first = fields.len();
        for field in t.children(list).filter(|&c| matches!(t.kind(c), SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD)) {
            let name = match kind {
                FieldsKind::Record => Some(t.token(t.child(field, SyntaxKind::NAME).ok_or(NotAnAdt::NoName)?)),
                FieldsKind::Tuple | FieldsKind::Unit => None,
            };
            let ty = self
                .other_child(field, &[SyntaxKind::ATTR, SyntaxKind::VISIBILITY, SyntaxKind::MUT_RESTRICTION, SyntaxKind::NAME, SyntaxKind::CONST_ARG])
                .ok_or(NotAnAdt::Malformed)?;
            let defaulted = t.child(field, SyntaxKind::CONST_ARG).is_some();
            fields.push(Field { name, index: fields.len() - first, ty: t.range(ty), defaulted });
            self.assoc_types(ty, params, assoc);
        }
        Ok(kind)
    }

    /// One child of a generic parameter list: a parameter, or `None` for its `<`, `>` and
    /// commas.
    fn param(&self, node: usize) -> Result<Option<Param>, NotAnAdt> {
        let t = self.tree;
        let kind = t.kind(node);
        if !matches!(kind, SyntaxKind::LIFETIME_PARAM | SyntaxKind::TYPE_PARAM | SyntaxKind::CONST_PARAM) {
            return Ok(None);
        }
        let decl_start = t.children(node).find(|&c| t.kind(c) != SyntaxKind::ATTR).ok_or(NotAnAdt::Malformed)?;
        let decl = t.range(decl_start).start..t.range(node).end;
        let named = |child: SyntaxKind| t.child(node, child).map(|c| t.range(c)).ok_or(NotAnAdt::Malformed);
        let param = match kind {
            SyntaxKind::LIFETIME_PARAM => Param { decl, name: named(SyntaxKind::LIFETIME)?, kind: ParamKind::Lifetime },
            SyntaxKind::TYPE_PARAM => {
                let bounds = t.child(node, SyntaxKind::TYPE_BOUND_LIST).map(|c| t.range(c)).filter(|r| !r.is_empty());
                Param { decl, name: named(SyntaxKind::NAME)?, kind: ParamKind::Type { bounds } }
            }
            _ => {
                let ty = self.other_child(node, &[SyntaxKind::ATTR, SyntaxKind::NAME, SyntaxKind::CONST_ARG]).ok_or(NotAnAdt::Malformed)?;
                Param { decl, name: named(SyntaxKind::NAME)?, kind: ParamKind::Const { ty: t.range(ty) } }
            }
        };
        Ok(Some(param))
    }

    /// The inside of the attribute node `attr` (`#[inside]`), as tokens: its meta node,
    /// without the invisible groups that are all of it (`#[$attr]` of a `$attr:meta`
    /// fragment); empty when the attribute holds no meta.
    fn attr_inside(&self, attr: usize) -> Range<usize> {
        crate::extent::meta_of(self.stream, self.tree, attr)
    }

    /// `#[default]` among the variant's attributes.
    fn is_default(&self, variant: usize) -> bool {
        let t = self.tree;
        t.children(variant).filter(|&c| t.kind(c) == SyntaxKind::ATTR).any(|attr| {
            let inside = self.attr_inside(attr);
            inside.len() == 1 && self.word(inside.start, "default")
        })
    }

    /// `repr(.. packed ..)` or `blaze::bits`, the attribute node `attr`.
    fn is_packed(&self, attr: usize) -> bool {
        let inside = self.attr_inside(attr);
        let at = inside.start;
        let open = matches!(self.stream.tokens().get(at + 1).map(|t| t.kind), Some(TokenKind::Open(Delim::Paren)));
        if self.word(at, "repr") && open {
            return (at + 2..inside.end).any(|it| self.word(it, "packed"));
        }
        let path_sep = matches!(self.stream.tokens().get(at + 1).map(|t| t.kind), Some(TokenKind::Punct { char: ':', spacing: Spacing::Joint }))
            && matches!(self.stream.tokens().get(at + 2).map(|t| t.kind), Some(TokenKind::Punct { char: ':', .. }));
        self.word(at, "blaze") && path_sep && self.word(at + 3, "bits")
    }

    /// Each path type of the fields' types that rustc's `find_type_parameters` bounds (its
    /// `visit_ty`): two segments or more, the first a type parameter's name written alone (not
    /// `::T`, not `<T as Tr>`), not a trait of a bound and not the trait of a qualified path.
    fn assoc_types(&self, ty: usize, params: &[Param], assoc: &mut AVec<'_, Range<usize>>) {
        let t = self.tree;
        let is_type_param = |at: usize| {
            params.iter().any(|p| matches!(p.kind, ParamKind::Type { .. }) && self.stream.text_at(p.name.start) == self.stream.text_at(at))
        };
        if !params.iter().any(|p| matches!(p.kind, ParamKind::Type { .. })) {
            return;
        }
        for node in core::iter::once(ty).chain(t.descendants(ty)) {
            if t.kind(node) != SyntaxKind::PATH_TYPE || self.trait_position(node) {
                continue;
            }
            let Some(mut path) = t.child(node, SyntaxKind::PATH) else { continue };
            let mut segments = 1;
            while let Some(qualifier) = t.child(path, SyntaxKind::PATH) {
                (path, segments) = (qualifier, segments + 1);
            }
            let first = t.child(path, SyntaxKind::PATH_SEGMENT).and_then(|segment| t.children(segment).next());
            let named = first.is_some_and(|f| t.kind(f) == SyntaxKind::NAME_REF && is_type_param(t.token(f)));
            if segments >= 2 && named {
                assoc.push(t.range(node));
            }
        }
    }

    /// Whether the path type `node` stands where a trait does: a bound's, or after `as` in a
    /// qualified path's `<T as Tr>`.
    fn trait_position(&self, node: usize) -> bool {
        let t = self.tree;
        match t.parent(node).map(|p| (p, t.kind(p))) {
            Some((_, SyntaxKind::TYPE_BOUND)) => true,
            Some((segment, SyntaxKind::PATH_SEGMENT)) => {
                t.children(segment).take_while(|&c| c != node).any(|c| t.kind(c) == SyntaxKind::AS_KW)
            }
            _ => false,
        }
    }
}
