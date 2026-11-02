//! `AstIdMap` allows to create stable IDs for "large" syntax nodes like items
//! and macro calls.
//!
//! Specifically, it enumerates all items in a file and uses the position of an
//! item as an ID. That way, IDs don't change unless the set of items itself
//! changes.
//!
//! These IDs are tricky. If one of them invalidates, its interned ID invalidates,
//! and this can cause *a lot* to be recomputed. For example, if you invalidate the ID
//! of a struct, and that struct has an impl (any impl!) this will cause the `Self`
//! type of the impl to invalidate, which will cause the all impls queries to be
//! invalidated, which will cause every trait solve query in this crate *and* all
//! transitive reverse dependencies to be invalidated, which is pretty much the worst
//! thing that can happen incrementality wise.
//!
//! So we want these IDs to stay as stable as possible. For top-level items, we store
//! their kind and name, which should be unique, but since they can still not be, we
//! also store an index disambiguator. For nested items, we also store the ID of their
//! parent. For macro calls, we store the macro name and an index. There aren't usually
//! a lot of macro calls in item position, and invalidation in bodies is not much of
//! a problem, so this should be enough.
//!
//! What is in a fn's body (blocks with items, items in blocks, macro calls, `asm!`) is
//! numbered apart from the rest of the file, by a walk of that body alone, when the body is
//! first asked about: a dependency's file is parsed as an outline whose bodies are parsed
//! only when needed (#1294), and its map must not parse them. A body's ids are in a space of
//! their own (the body bit of [`ErasedFileAstId`]): the hash bits name the body, by slots
//! the outline gives each body from its owner's id, and the index is the node's number among
//! the nodes of its kind in the body. So a body's ids depend on its owner's id and slots and
//! on its own text only, and an outline's map and a full parse's map give the same ids.

use std::{
    any::type_name,
    cell::RefCell,
    fmt,
    hash::{BuildHasher, Hash, Hasher},
    marker::PhantomData,
    sync::{Arc, OnceLock},
};

use blaze_hash::{FixedState, HashMap};
use blaze_ids::{bits::BitArray, radix};
use blaze_mem::{Frame, Tag, frame, scratch::Scratch};
use syntax::{
    AstNode, AstPtr, FlatTree, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, ast, view::NodeId,
};

// The first index is always the root node's AstId
/// The root ast id always points to the encompassing file, using this in spans is discouraged as
/// any range relative to it will be effectively absolute, ruining the entire point of anchored
/// relative text ranges.
pub const ROOT_ERASED_FILE_AST_ID: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::Root as u32));

/// ErasedFileAstId used as the span for syntax node fixups. Any Span containing this file id is to be
/// considered fake.
/// Do not modify this, it is used by the proc-macro server.
pub const FIXUP_ERASED_FILE_AST_ID_MARKER: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::Fixup as u32));

/// [`ErasedFileAstId`] used as the span for syntax nodes that should not be mapped down to
/// macro expansion. Any `Span` containing this file id is to be considered fake.
pub const NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER: ErasedFileAstId =
    ErasedFileAstId(pack(0, 0, false, ErasedFileAstIdKind::NoDownmap as u32));

/// This is a type erased FileAstId.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErasedFileAstId(u32);

impl fmt::Debug for ErasedFileAstId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind =
            // alloc: `Debug` output, the edge
            self.kind_typed().map_or_else(|| "Unknown".to_owned(), |kind| format!("{kind:?}"));
        if f.alternate() {
            match self.in_body() {
                false => write!(f, "{kind}[{:04X}, {}]", self.hash_value(), self.index()),
                true => write!(f, "{kind}[{:04X}, {}, body]", self.hash_value(), self.index()),
            }
        } else {
            let mut f = f.debug_struct("ErasedFileAstId");
            f.field("kind", &kind)
                .field("index", &self.index())
                .field("hash", &format_args!("{:04X}", self.hash_value()));
            if self.in_body() {
                f.field("in_body", &true);
            }
            f.finish()
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
enum ErasedFileAstIdKind {
    /// This needs to not change because it's depended upon by the proc macro server.
    Fixup = 0,
    // The following are associated with `ErasedHasNameFileAstId`.
    Enum,
    Struct,
    Union,
    ExternCrate,
    MacroDef,
    MacroRules,
    Module,
    Static,
    Trait,
    // Until here associated with `ErasedHasNameFileAstId`.
    // The following are associated with `ErasedAssocItemFileAstId`.
    Variant,
    Const,
    Fn,
    MacroCall,
    TypeAlias,
    // Until here associated with `ErasedAssocItemFileAstId`.
    // Extern blocks don't really have any identifying property unfortunately.
    ExternBlock,
    // FIXME: If we store the final `UseTree` instead of the top-level `Use`, we can store its name,
    // and be way more granular for incrementality, at the expense of increased memory usage.
    // Use IDs aren't used a lot. The main thing that stores them is the def map. So everything that
    // uses the def map will be invalidated. That includes infers, and so is pretty bad, but our
    // def map incrementality story is pretty bad anyway and needs to be improved (see
    // https://rust-lang.zulipchat.com/#narrow/channel/185405-t-compiler.2Frust-analyzer/topic/.60infer.60.20queries.20and.20splitting.20.60DefMap.60).
    // So I left this as-is for now, as the def map improvement should also mitigate this.
    Use,
    /// Associated with [`ImplFileAstId`].
    Impl,
    /// Associated with [`BlockExprFileAstId`].
    BlockExpr,
    // `global_asm!()` is an item, so we need to give it an `AstId`. So we give to all inline asm
    // because incrementality is not a problem, they will always be the only item in the macro file,
    // and memory usage also not because they're rare.
    AsmExpr,
    /// Represents a fake [`ErasedFileAstId`] that should not be mapped down to macro expansion
    /// result.
    NoDownmap,
    /// Keep this last.
    Root,
}

impl ErasedFileAstIdKind {
    fn syntax_kind(self) -> Option<SyntaxKind> {
        match self {
            ErasedFileAstIdKind::Enum => Some(SyntaxKind::ENUM),
            ErasedFileAstIdKind::Struct => Some(SyntaxKind::STRUCT),
            ErasedFileAstIdKind::Union => Some(SyntaxKind::UNION),
            ErasedFileAstIdKind::ExternCrate => Some(SyntaxKind::EXTERN_CRATE),
            ErasedFileAstIdKind::MacroDef => Some(SyntaxKind::MACRO_DEF),
            ErasedFileAstIdKind::MacroRules => Some(SyntaxKind::MACRO_RULES),
            ErasedFileAstIdKind::Module => Some(SyntaxKind::MODULE),
            ErasedFileAstIdKind::Static => Some(SyntaxKind::STATIC),
            ErasedFileAstIdKind::Trait => Some(SyntaxKind::TRAIT),
            ErasedFileAstIdKind::Variant => Some(SyntaxKind::VARIANT),
            ErasedFileAstIdKind::Const => Some(SyntaxKind::CONST),
            ErasedFileAstIdKind::Fn => Some(SyntaxKind::FN),
            ErasedFileAstIdKind::MacroCall => Some(SyntaxKind::MACRO_CALL),
            ErasedFileAstIdKind::TypeAlias => Some(SyntaxKind::TYPE_ALIAS),
            ErasedFileAstIdKind::ExternBlock => Some(SyntaxKind::EXTERN_BLOCK),
            ErasedFileAstIdKind::Use => Some(SyntaxKind::USE),
            ErasedFileAstIdKind::Impl => Some(SyntaxKind::IMPL),
            ErasedFileAstIdKind::BlockExpr => Some(SyntaxKind::BLOCK_EXPR),
            ErasedFileAstIdKind::AsmExpr => Some(SyntaxKind::ASM_EXPR),
            ErasedFileAstIdKind::Fixup
            | ErasedFileAstIdKind::NoDownmap
            | ErasedFileAstIdKind::Root => None,
        }
    }
}

// First hash, then index, then whether the node is in a fn body, then kind. The kind stays
// at the same bits as before the body bit, so the markers above keep their values.
const HASH_BITS: u32 = 16;
const INDEX_BITS: u32 = 10;
const BODY_BITS: u32 = 1;
const KIND_BITS: u32 = 5;
const KIND_SHIFT: u32 = HASH_BITS + INDEX_BITS + BODY_BITS;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// How many hash values there are to name bodies by.
const SLOTS: u32 = 1 << HASH_BITS;
const _: () = assert!(ErasedFileAstIdKind::Root as u32 <= ((1 << KIND_BITS) - 1));
const _: () = assert!(HASH_BITS + INDEX_BITS + BODY_BITS + KIND_BITS == u32::BITS);
const _: () = assert!(KIND_SHIFT == 27);

#[inline]
const fn u16_hash(hash: u64) -> u16 {
    // We do basically the same as `FxHasher`. We don't use rustc-hash and truncate because the
    // higher bits have more entropy, but unlike rustc-hash we don't rotate because it rotates
    // for hashmaps that just use the low bits, but we compare all bits.
    const K: u16 = 0xecc5;
    let (part1, part2, part3, part4) =
        (hash as u16, (hash >> 16) as u16, (hash >> 32) as u16, (hash >> 48) as u16);
    part1
        .wrapping_add(part2)
        .wrapping_mul(K)
        .wrapping_add(part3)
        .wrapping_mul(K)
        .wrapping_add(part4)
        .wrapping_mul(K)
}

#[inline]
const fn pack(hash: u16, index: u32, in_body: bool, kind: u32) -> u32 {
    (hash as u32)
        | (index << HASH_BITS)
        | ((in_body as u32) << (HASH_BITS + INDEX_BITS))
        | (kind << KIND_SHIFT)
}

impl ErasedFileAstId {
    #[inline]
    fn hash_value(self) -> u16 {
        self.0 as u16
    }

    #[inline]
    fn index(self) -> u32 {
        (self.0 >> HASH_BITS) & INDEX_MASK
    }

    /// Whether the node is in a fn body, numbered by a walk of that body alone.
    #[inline]
    pub fn in_body(self) -> bool {
        (self.0 >> (HASH_BITS + INDEX_BITS)) & 1 != 0
    }

    #[inline]
    fn kind(self) -> u32 {
        self.0 >> KIND_SHIFT
    }

    fn kind_typed(self) -> Option<ErasedFileAstIdKind> {
        let kind = self.kind();
        macro_rules! kind {
            ($($kind:ident),* $(,)?) => {
                if false {
                    // Ensure we covered all variants.
                    match ErasedFileAstIdKind::Root {
                        $( ErasedFileAstIdKind::$kind => {} )*
                    }
                    unreachable!()
                }
                $( else if kind == ErasedFileAstIdKind::$kind as u32 {
                    Some(ErasedFileAstIdKind::$kind)
                } )*
                else {
                    None
                }
            };
        }
        kind!(
            Root,
            Enum,
            Struct,
            Union,
            ExternCrate,
            MacroDef,
            MacroRules,
            Module,
            Static,
            Trait,
            Variant,
            Const,
            Fn,
            MacroCall,
            TypeAlias,
            ExternBlock,
            Use,
            Impl,
            BlockExpr,
            AsmExpr,
            Fixup,
            NoDownmap,
        )
    }

    #[inline]
    pub fn is_root(self) -> bool {
        self.kind() == ErasedFileAstIdKind::Root as u32
    }

    /// Whether this can be the id of an `N`, the test [`FileAstId::downcast_unchecked`] asserts:
    /// a decoded id is checked with it before it is downcast.
    #[inline]
    pub fn may_be<N: AstIdNode>(self) -> bool {
        self.kind_typed().and_then(|kind| kind.syntax_kind()).is_some_and(|kind| N::can_cast(kind))
    }

    fn ast_id_for(
        node: syntax::view::Node<'_>,
        index_map: &mut ErasedAstIdNextIndexMap,
        parent: Option<&ErasedFileAstId>,
    ) -> Option<ErasedFileAstId> {
        // Blocks are deliberately not here - we only want to allocate a block if it contains items.
        has_name_ast_id(node, index_map)
            .or_else(|| assoc_item_ast_id(node, index_map, parent))
            .or_else(|| extern_block_ast_id(node, index_map))
            .or_else(|| use_ast_id(node, index_map))
            .or_else(|| impl_ast_id(node, index_map))
            .or_else(|| asm_expr_ast_id(node, index_map))
    }

    fn should_alloc(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
        should_alloc_has_name(kind)
            .or_else(|| should_alloc_assoc_item(kind))
            .or_else(|| {
                ast::ExternBlock::can_cast(kind).then_some(ErasedFileAstIdKind::ExternBlock)
            })
            .or_else(|| ast::Use::can_cast(kind).then_some(ErasedFileAstIdKind::Use))
            .or_else(|| ast::Impl::can_cast(kind).then_some(ErasedFileAstIdKind::Impl))
            .or_else(|| ast::AsmExpr::can_cast(kind).then_some(ErasedFileAstIdKind::AsmExpr))
    }

    #[inline]
    pub fn into_raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn from_raw(v: u32) -> Self {
        Self(v)
    }
}

pub trait AstIdNode: AstNode {}

/// `AstId` points to an AST node in a specific file.
pub struct FileAstId<N> {
    raw: ErasedFileAstId,
    _marker: PhantomData<fn() -> N>,
}

/// Traits are manually implemented because `derive` adds redundant bounds.
impl<N> Clone for FileAstId<N> {
    #[inline]
    fn clone(&self) -> FileAstId<N> {
        *self
    }
}
impl<N> Copy for FileAstId<N> {}

impl<N> PartialEq for FileAstId<N> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<N> Eq for FileAstId<N> {}
impl<N> Hash for FileAstId<N> {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.raw.hash(hasher);
    }
}

impl<N> fmt::Debug for FileAstId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileAstId::<{}>({:?})", type_name::<N>(), self.raw)
    }
}

impl<N> FileAstId<N> {
    /// The id `raw` is, read back as an id of an `N`: `raw` must have been one.
    #[inline]
    pub fn from_erased_unchecked(raw: ErasedFileAstId) -> Self {
        FileAstId { raw, _marker: PhantomData }
    }

    // Can't make this a From implementation because of coherence
    #[inline]
    pub fn upcast<M: AstIdNode>(self) -> FileAstId<M>
    where
        N: Into<M>,
    {
        FileAstId { raw: self.raw, _marker: PhantomData }
    }

    #[inline]
    pub fn downcast_unchecked<M: AstIdNode + Into<N>>(self) -> FileAstId<M> {
        debug_assert!(
            self.erase().may_be::<M>(),
            "`FileAstId::<{}>::downcast_unchecked::<{}>()` is invalid",
            std::any::type_name::<N>(),
            std::any::type_name::<M>(),
        );
        FileAstId { raw: self.raw, _marker: PhantomData }
    }

    #[inline]
    pub fn erase(self) -> ErasedFileAstId {
        self.raw
    }
}

#[derive(Hash)]
struct ErasedHasNameFileAstId<'a> {
    name: &'a str,
}

/// This holds the ast ID for variants too (they're a kind of assoc item).
#[derive(Hash)]
struct ErasedAssocItemFileAstId<'a> {
    /// Subtle: items in `extern` blocks **do not** store the ID of the extern block here.
    /// Instead this is left empty. The reason is that `ExternBlockFileAstId` is pretty unstable
    /// (it contains only an index), and extern blocks don't introduce a new scope, so storing
    /// the extern block ID will do more harm to incrementality than help.
    parent: Option<ErasedFileAstId>,
    properties: ErasedHasNameFileAstId<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ImplFileAstId<'a> {
    /// This can be `None` if the `Self` type is not a named type, or if it is inside a macro call.
    self_ty_name: Option<&'a str>,
    /// This can be `None` if this is an inherent impl, or if the trait name is inside a macro call.
    trait_name: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlockExprFileAstId {
    parent: Option<ErasedFileAstId>,
}

impl AstIdNode for ast::ExternBlock {}

fn extern_block_ast_id(
    node: syntax::view::Node<'_>,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    use syntax::view::{self as ast, AstNode};
    if ast::ExternBlock::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::ExternBlock, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::Use {}

fn use_ast_id(
    node: syntax::view::Node<'_>,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    use syntax::view::{self as ast, AstNode};
    if ast::Use::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::Use, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::AsmExpr {}

fn asm_expr_ast_id(
    node: syntax::view::Node<'_>,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    use syntax::view::{self as ast, AstNode};
    if ast::AsmExpr::can_cast(node.kind()) {
        Some(index_map.new_id(ErasedFileAstIdKind::AsmExpr, ()))
    } else {
        None
    }
}

impl AstIdNode for ast::Impl {}

fn impl_ast_id<'t>(
    node: syntax::view::Node<'t>,
    index_map: &mut ErasedAstIdNextIndexMap,
) -> Option<ErasedFileAstId> {
    use syntax::view::{self as ast, AstNode};
    let node = ast::Impl::cast(node)?;
    let type_as_name = |ty: Option<ast::Type<'t>>| match ty? {
        ast::Type::PathType(it) => Some(it.path()?.segment()?.name_ref()?),
        _ => None,
    };
    let data = ImplFileAstId {
        self_ty_name: type_as_name(node.self_ty()).map(|it| it.text()),
        trait_name: type_as_name(node.trait_()).map(|it| it.text()),
    };
    Some(index_map.new_id(ErasedFileAstIdKind::Impl, data))
}

// Blocks aren't `AstIdNode`s deliberately, because unlike other nodes, not all blocks get their own
// ast id, only if they have items. To account for that we have a different, fallible, API for blocks.
// impl !AstIdNode for ast::BlockExpr {}

fn block_expr_ast_id(
    kind: SyntaxKind,
    index_map: &mut ErasedAstIdNextIndexMap,
    parent: Option<&ErasedFileAstId>,
) -> Option<ErasedFileAstId> {
    if ast::BlockExpr::can_cast(kind) {
        Some(
            index_map.new_id(
                ErasedFileAstIdKind::BlockExpr,
                BlockExprFileAstId { parent: parent.copied() },
            ),
        )
    } else {
        None
    }
}

/// The next index of each kind and hash value, keyed by both in one word (`kind << 16 | hash`),
/// so a key hashes with one multiply.
#[derive(Default)]
struct ErasedAstIdNextIndexMap(HashMap<u32, u32>);

impl ErasedAstIdNextIndexMap {
    #[inline]
    fn new_id(&mut self, kind: ErasedFileAstIdKind, data: impl Hash) -> ErasedFileAstId {
        let hash = FixedState.hash_one(&data);
        let initial_hash = u16_hash(hash);
        // Even though 2^INDEX_BITS=1024 items with the same hash seems like a lot,
        // it could happen with macro calls or `use`s in macro-generated files. So we want
        // to handle it gracefully. We just increment the hash.
        let mut hash = initial_hash;
        let index = loop {
            match self.0.entry(((kind as u32) << HASH_BITS) | u32::from(hash)) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let i = entry.get_mut();
                    if *i < ((1 << INDEX_BITS) - 1) {
                        *i += 1;
                        break *i;
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(0);
                    break 0;
                }
            }
            hash = hash.wrapping_add(1);
            if hash == initial_hash {
                // That's 2^26=67,108,864 items!
                panic!("you have way too many items in the same file!");
            }
        };
        let kind = kind as u32;
        ErasedFileAstId(pack(hash, index, false, kind))
    }
}

macro_rules! register_enum_ast_id {
    (impl $AstIdNode:ident for $($ident:ident),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+
    };
}
register_enum_ast_id! {
    impl AstIdNode for
    Item, AnyHasGenericParams, Adt, Macro,
    AssocItem
}

macro_rules! register_has_name_ast_id {
    (impl $AstIdNode:ident for $($ident:ident = $name_method:ident),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+

        fn has_name_ast_id(
            node: syntax::view::Node<'_>,
            index_map: &mut ErasedAstIdNextIndexMap,
        ) -> Option<ErasedFileAstId> {
            use syntax::view::{self as ast, AstNode, HasName};
            $(
                if let Some(node) = ast::$ident::cast(node) {
                    let name = node.$name_method().map_or("", |it| it.text());
                    let result = ErasedHasNameFileAstId { name };
                    return Some(index_map.new_id(ErasedFileAstIdKind::$ident, result));
                }
            )*
            None
        }

        fn should_alloc_has_name(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
            $( if ast::$ident::can_cast(kind) { Some(ErasedFileAstIdKind::$ident) } else )* { None }
        }
    };
}
register_has_name_ast_id! {
    impl AstIdNode for
        Enum = name,
        Struct = name,
        Union = name,
        ExternCrate = name_ref,
        MacroDef = name,
        MacroRules = name,
        Module = name,
        Static = name,
        Trait = name
}

macro_rules! register_assoc_item_ast_id {
    (impl $AstIdNode:ident for $($ident:ident = |$it:ident| $name:expr),+ ) => {
        $(
            impl $AstIdNode for ast::$ident {}
        )+

        fn assoc_item_ast_id(
            node: syntax::view::Node<'_>,
            index_map: &mut ErasedAstIdNextIndexMap,
            parent: Option<&ErasedFileAstId>,
        ) -> Option<ErasedFileAstId> {
            use syntax::view::{self as ast, AstNode, HasName};
            $(
                if let Some($it) = ast::$ident::cast(node) {
                    let name = $name.map_or("", |it| it.text());
                    let properties = ErasedHasNameFileAstId { name };
                    let result = ErasedAssocItemFileAstId { parent: parent.copied(), properties };
                    return Some(index_map.new_id(ErasedFileAstIdKind::$ident, result));
                }
            )*
            None
        }

        fn should_alloc_assoc_item(kind: SyntaxKind) -> Option<ErasedFileAstIdKind> {
            $( if ast::$ident::can_cast(kind) { Some(ErasedFileAstIdKind::$ident) } else )* { None }
        }
    };
}
register_assoc_item_ast_id! {
    impl AstIdNode for
    Variant = |it| it.name(),
    Const = |it| it.name(),
    Fn = |it| it.name(),
    MacroCall = |it| it.path().and_then(|path| path.segment()?.name_ref()),
    TypeAlias = |it| it.name()
}

/// Maps items' nodes to `ErasedFileAstId`s and back.
///
/// The ids of what is outside fn bodies are made with the map. Each body's are made when
/// something in it is first asked about, from the tree the map keeps of it, so a map of an
/// outline parses only the bodies asked about. Which bodies were walked is not part of what
/// the map is: two maps are equal when their ids outside bodies are, and their bodies are at
/// the same places with the same text and slots, which make the same ids.
///
/// Costs: an id outside fn bodies is a 16-byte [`Entry`] in both [`Index`]es (11 to 21 bytes
/// each), 37 to 59 bytes in all, and a lookup either way is a multiply and two cache lines. A
/// body is 60 bytes until it is walked ([`Bodies`]) and 24 bytes per id after ([`BodyIds`]);
/// a lookup in one is a binary search of the bodies, then one of its ids. A lookup locks
/// nothing and writes nothing shared, but for the first one in a body, which fills its
/// `OnceLock`; a filled one is read with one acquire load.
pub struct AstIdMap {
    /// What is outside fn bodies: the root first, then in the walk's order.
    entries: Box<[Entry]>,
    /// `entries` by where they begin.
    by_start: Index,
    /// `entries` by id.
    by_id: Index,
    bodies: Bodies,
}

/// A node with an id, as words: where it begins, where it ends, its kind and its id. Plain
/// words, so that a map's entries are compared as one `[u32]` and hashed with one `write`:
/// the query engine compares maps to backdate them.
type Entry = [u32; 4];
const START: usize = 0;
const END: usize = 1;
const KIND: usize = 2;
const ID: usize = 3;
const _: () = assert!(size_of::<Entry>() == 16);

/// A node's place and kind, as the first three words of its [`Entry`].
type Key = [u32; 3];

#[inline]
fn entry(kind: SyntaxKind, range: TextRange, id: ErasedFileAstId) -> Entry {
    let [start, end, kind] = key_of(kind, range);
    [start, end, kind, id.0]
}

#[inline]
fn key_of(kind: SyntaxKind, range: TextRange) -> Key {
    [u32::from(range.start()), u32::from(range.end()), u32::from(u16::from(kind))]
}

#[inline]
fn key(ptr: SyntaxNodePtr) -> Key {
    key_of(ptr.kind(), ptr.text_range())
}

#[inline]
fn entry_ptr(entry: &Entry) -> SyntaxNodePtr {
    SyntaxNodePtr::from_parts(
        SyntaxKind::from(entry[KIND] as u16),
        TextRange::new(entry[START].into(), entry[END].into()),
    )
}

#[inline]
fn entry_id(entry: &Entry) -> ErasedFileAstId {
    ErasedFileAstId(entry[ID])
}

#[inline]
fn is_at(entry: &Entry, key: Key) -> bool {
    entry[START] == key[0] && entry[END] == key[1] && entry[KIND] == key[2]
}

/// An open-addressed table from one word of each of a map's entries (where it begins, or its
/// id) to the entry's number. Each slot is the word and the number plus one (0 for empty):
/// 8 bytes, 8 to a cache line. The table is a power of two, under 3/4 full, probed linearly
/// from a Fibonacci hash of the word (a multiply and a shift, no hasher), so a hit probes at
/// most 2.5 slots on average, nearly always in one line, and a miss stops at the first empty
/// slot. 10.7 to 21.3 bytes per entry. Built once; read by any number of threads with plain
/// loads.
#[derive(Default)]
struct Index {
    slots: Box<[[u32; 2]]>,
    /// `slots.len()` is `1 << bits`.
    bits: u32,
}

impl Index {
    /// The index of `len` entries, the `i`th's word `word(i)`.
    fn new(len: usize, word: impl Fn(usize) -> u32) -> Index {
        let size = (len + len / 3 + 1).next_power_of_two().max(2);
        let bits = size.trailing_zeros();
        // alloc: the index's one table, sized once when its map is built
        let mut slots = vec![[0u32; 2]; size].into_boxed_slice();
        for it in 0..len {
            let key = word(it);
            let mut at = home(key, bits);
            while slots[at][1] != 0 {
                at = (at + 1) & (size - 1);
            }
            slots[at] = [key, it as u32 + 1];
        }
        Index { slots, bits }
    }

    /// The first of the entries whose word is `word` that `is` accepts, each asked in the order
    /// they were added. Linear probing without removal keeps them in that order along the probe.
    #[inline]
    fn find(&self, word: u32, mut is: impl FnMut(usize) -> bool) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = home(word, self.bits);
        loop {
            let [found, number] = self.slots[at];
            if number == 0 {
                return None;
            }
            if found == word && is(number as usize - 1) {
                return Some(number as usize - 1);
            }
            at = (at + 1) & mask;
        }
    }
}

/// Where `word` is first probed for in a table of `1 << bits` slots: the top bits of its
/// product with 2^64 over the golden ratio, which spreads offsets and ids alike.
#[inline]
fn home(word: u32, bits: u32) -> usize {
    (u64::from(word).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
}

thread_local! {
    /// The sorts' scratch of the maps built on this thread, kept at the largest they grew to.
    static ENTRY_SCRATCH: RefCell<Scratch<Entry>> = const { RefCell::new(Scratch::new()) };
    static PAIR_SCRATCH: RefCell<Scratch<[u32; 2]>> = const { RefCell::new(Scratch::new()) };
    static PLACED_SCRATCH: RefCell<Scratch<Placed>> = const { RefCell::new(Scratch::new()) };
    /// The next index of each kind and hash, cleared and reused by each map built here.
    static NEXT_INDEX: RefCell<ErasedAstIdNextIndexMap> = RefCell::default();
}

/// Sorts entries by all four words, in this thread's scratch.
fn sort_entries(entries: &mut [Entry]) {
    ENTRY_SCRATCH.with_borrow_mut(|scratch| {
        radix::sort_copy_by_key_in(
            entries,
            |&[a, b, c, d]| {
                (u128::from(a) << 96) | (u128::from(b) << 64) | (u128::from(c) << 32) | u128::from(d)
            },
            scratch.take(),
        );
    });
}

/// Sorts pairs by both words, in this thread's scratch.
fn sort_pairs(pairs: &mut [[u32; 2]]) {
    PAIR_SCRATCH.with_borrow_mut(|scratch| {
        radix::sort_copy_by_key_in(pairs, |&[a, b]| radix::pack_32(a, b), scratch.take());
    });
}

/// A map's fn bodies, as columns in the order of their text. A body's ids are made by a walk of
/// it alone, when something in it is first asked about. 60 bytes per body until then.
struct Bodies {
    /// Where each begins: what the body holding a node is searched by.
    starts: Box<[u32]>,
    /// Each one's [`Row`]. With `starts`, what two maps compare and hash of their bodies: their
    /// places, slots and texts, which make the same ids; not which were walked.
    rows: Box<[Row]>,
    /// Each one's first slot and number, sorted: what the body an id names is searched by.
    by_slot: Box<[[u32; 2]]>,
    /// Each one's element in `tree`: a lazy node, walked through its part, or a fn's block.
    ats: Box<[u32]>,
    /// Each one's ids as [`BodyIds`] words, once walked.
    ids: Box<[OnceLock<Box<[u32]>>]>,
    /// The tree the bodies are elements of; `None` when there are none.
    tree: Option<Arc<FlatTree>>,
}

/// A body's end, the first of the hash values its ids have, how many it has (`INDEX_MASK + 1`
/// ids of each kind in each, [`take_slots`]), and its text's hash, low word first.
type Row = [u32; 5];
const ROW_END: usize = 0;
const ROW_FIRST_SLOT: usize = 1;
const ROW_SLOTS: usize = 2;
const ROW_TEXT_HASH: usize = 3;

/// One of [`Bodies`]' columns, allocated once at its exact length.
fn column<T>(rows: impl Iterator<Item = T>) -> Box<[T]> {
    // alloc: a column of a map, once per map built (a parse's output), at its exact length
    rows.collect()
}

/// A fn body and where it is: what [`Bodies::new`] is made of.
#[derive(Clone, Copy)]
struct Placed {
    range: TextRange,
    first_slot: u32,
    slots: u32,
    text_hash: u64,
    at: u32,
}

impl Bodies {
    /// The bodies `placed`, elements of `tree`.
    fn new(tree: &Arc<FlatTree>, placed: &mut [Placed]) -> Bodies {
        // Bodies don't overlap, so no two begin at one place.
        PLACED_SCRATCH.with_borrow_mut(|scratch| {
            radix::sort_copy_by_key_in(placed, |it| u32::from(it.range.start()), scratch.take());
        });
        let mut by_slot = column(placed.iter().zip(0u32..).map(|(it, body)| [it.first_slot, body]));
        sort_pairs(&mut by_slot);
        let row = |it: &Placed| -> Row {
            [
                u32::from(it.range.end()),
                it.first_slot,
                it.slots,
                it.text_hash as u32,
                (it.text_hash >> 32) as u32,
            ]
        };
        Bodies {
            starts: column(placed.iter().map(|it| u32::from(it.range.start()))),
            rows: column(placed.iter().map(row)),
            by_slot,
            ats: column(placed.iter().map(|it| it.at)),
            ids: column(placed.iter().map(|_| OnceLock::new())),
            // clone: one count of the tree per map with bodies, when the map is built
            tree: (!placed.is_empty()).then(|| Arc::clone(tree)),
        }
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    fn range(&self, body: usize) -> TextRange {
        TextRange::new(self.starts[body].into(), self.rows[body][ROW_END].into())
    }

    /// The body `range` is in: the last to begin at or before it, if it ends at or after it.
    #[inline]
    fn holding(&self, range: TextRange) -> Option<usize> {
        let start = u32::from(range.start());
        let body = self.starts.partition_point(|&it| it <= start).checked_sub(1)?;
        (u32::from(range.end()) <= self.rows[body][ROW_END]).then_some(body)
    }

    /// The body `id`, an id of a node in a body, is of: the one with its hash value among its
    /// slots.
    #[inline]
    fn of(&self, id: ErasedFileAstId) -> Option<usize> {
        let slot = u32::from(id.hash_value());
        let n = self.by_slot.partition_point(|it| it[0] <= slot);
        let [first, body] = *self.by_slot[..n].last()?;
        (slot < first + self.rows[body as usize][ROW_SLOTS]).then_some(body as usize)
    }

    /// The ids of body `body`, walking it if no one asked about it before.
    #[inline]
    fn ids(&self, body: usize) -> BodyIds<'_> {
        BodyIds::of(self.ids[body].get_or_init(|| {
            let tree = self.tree.as_deref().expect("a map with bodies keeps their tree");
            let (tree, at) = expanded(tree, self.ats[body]);
            let row = &self.rows[body];
            body_ids(tree, at, row[ROW_FIRST_SLOT], row[ROW_SLOTS])
        }))
    }

    /// Body `body` with the same text at `range`, element `at` of another tree: the same slots.
    fn moved(&self, body: usize, range: TextRange, at: u32) -> Placed {
        let row = &self.rows[body];
        Placed {
            range,
            first_slot: row[ROW_FIRST_SLOT],
            slots: row[ROW_SLOTS],
            text_hash: u64::from(row[ROW_TEXT_HASH]) | (u64::from(row[ROW_TEXT_HASH + 1]) << 32),
            at,
        }
    }
}

impl fmt::Debug for Bodies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries((0..self.len()).map(|body| {
                let row = &self.rows[body];
                (self.range(body), row[ROW_FIRST_SLOT], row[ROW_SLOTS])
            }))
            .finish()
    }
}

/// A body's ids, as one slice of words: its `n` [`Entry`]s sorted by where they begin, then `n`
/// pairs of an id and its entry's number, sorted by id. 24 bytes per id, and one allocation per
/// body with ids, none for one without. A lookup is a binary search of either half.
#[derive(Clone, Copy)]
struct BodyIds<'a> {
    entries: &'a [Entry],
    by_id: &'a [[u32; 2]],
}

impl<'a> BodyIds<'a> {
    fn of(words: &'a [u32]) -> BodyIds<'a> {
        let (entries, by_id) = words.split_at(words.len() / 6 * 4);
        BodyIds { entries: entries.as_chunks().0, by_id: by_id.as_chunks().0 }
    }

    #[inline]
    fn id(self, key: Key) -> Option<ErasedFileAstId> {
        let first = self.entries.partition_point(|it| it[START] < key[0]);
        self.entries[first..]
            .iter()
            .take_while(|it| it[START] == key[0])
            .find(|it| is_at(it, key))
            .map(entry_id)
    }

    #[inline]
    fn ptr(self, id: ErasedFileAstId) -> Option<SyntaxNodePtr> {
        let at = self.by_id.partition_point(|it| it[0] < id.0);
        let [found, number] = *self.by_id.get(at)?;
        (found == id.0).then(|| entry_ptr(&self.entries[number as usize]))
    }
}

impl fmt::Debug for AstIdMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AstIdMap")
            .field(
                "entries",
                // alloc: `Debug` output, the edge
                &self.entries.iter().map(|it| (entry_ptr(it), entry_id(it))).collect::<Vec<_>>(),
            )
            .field("bodies", &self.bodies)
            .finish()
    }
}

impl PartialEq for AstIdMap {
    fn eq(&self, other: &Self) -> bool {
        self.entries.as_flattened() == other.entries.as_flattened()
            && self.bodies.starts == other.bodies.starts
            && self.bodies.rows.as_flattened() == other.bodies.rows.as_flattened()
    }
}
impl Eq for AstIdMap {}

impl Hash for AstIdMap {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Each a `[u32]`, hashed as its length and one `write` of its bytes.
        self.entries.as_flattened().hash(state);
        self.bodies.starts.hash(state);
        self.bodies.rows.as_flattened().hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainsItems {
    Yes,
    No,
}

#[inline]
fn kind_of(tree: &FlatTree, at: u32) -> SyntaxKind {
    SyntaxKind::from(tree.kind(at).0)
}

/// Whether node `at` is a fn's body: a lazy node, or the block of a fn.
#[inline]
fn is_body(tree: &FlatTree, at: u32, kind: SyntaxKind) -> bool {
    tree.is_lazy(at)
        || (kind == SyntaxKind::BLOCK_EXPR
            && tree.parent(at).is_some_and(|it| kind_of(tree, it) == SyntaxKind::FN))
}

/// The node `at` stands for, as a node with its children: its part when it is lazy.
fn expanded(tree: &FlatTree, at: u32) -> (&FlatTree, u32) {
    match tree.is_lazy(at) {
        true => (&**tree.part(at), 0),
        false => (tree, at),
    }
}

/// The node of `kind` at `range`, not empty, at or under `at`, found by stepping down to the
/// child that holds `range`: no lazy node is entered on the way, as none holds another node.
fn find_node(tree: &FlatTree, mut at: u32, kind: SyntaxKind, range: TextRange) -> Option<u32> {
    loop {
        if tree.range(at) == range && kind_of(tree, at) == kind {
            return Some(at);
        }
        at = tree.children(at).find(|&it| tree.range(it).contains_range(range))?;
    }
}

/// What [`walk`] finds, in the order ids are made.
enum Step<P> {
    /// The block a found item is in, which holds no item found before it: its id is made
    /// before the item's.
    Block { at: u32, parent: Option<P> },
    Item { at: u32, kind: ErasedFileAstIdKind, parent: Option<P> },
    /// A fn's body, left to a walk of its own.
    Body { at: u32, parent: Option<P> },
}

/// Walks the nodes under `root` that may have ids, parents before children: by walking the
/// tree in breadth-first order parents get lower ids than children, so adding a new child
/// does not change its parent's id (adding a new function to a trait does not change the ids
/// of top-level items, which helps caching). `visit` is told of each and gives an item the
/// parent its children are told of. With `bodies_apart`, a fn's body is told of and not
/// walked into.
///
/// Blocks get an id only if they contain items. The walk keeps the stack of the blocks it is
/// under: when it enters a block it pushes `(block, No)`; items inside a block are attributed
/// to the block's container, not the block; for the first item found inside a block it makes
/// that `(block, Yes)` and tells of the block. When it leaves the block it pops it, whether
/// or not it told of it. It may seem that this tells of blocks that have no items directly but
/// have items inside other items inside them. This is true, but it doesn't matter, because
/// such blocks can't exist: the block will then contain the *outer* item.
///
/// Each layer is its elements in preorder, an item's subtree is stepped over by its end, and
/// a block is left once the walk is past its end. A lazy node has no children in the arrays,
/// so the walk never parses one.
fn walk<P: Copy>(
    frame: &Frame,
    tree: &FlatTree,
    root: u32,
    bodies_apart: bool,
    mut visit: impl FnMut(Step<P>) -> Option<P>,
) {
    let mut blocks: Vec<(u32, ContainsItems), &Frame> = Vec::new_in(frame);
    let mut curr_layer: Vec<(u32, Option<P>), &Frame> = Vec::with_capacity_in(32, frame);
    curr_layer.push((root, None));
    let mut next_layer = Vec::with_capacity_in(32, frame);
    while !curr_layer.is_empty() {
        for (layer, parent) in curr_layer.drain(..) {
            let end = tree.end(layer);
            let mut it = layer;
            while it < end {
                while blocks.last().is_some_and(|&(block, _)| tree.end(block) <= it) {
                    blocks.pop();
                }
                if tree.is_token(it) {
                    it += 1;
                    continue;
                }
                let kind = kind_of(tree, it);
                if bodies_apart && is_body(tree, it, kind) {
                    visit(Step::Body { at: it, parent });
                    it = tree.end(it);
                    continue;
                }
                if ast::BlockExpr::can_cast(kind) {
                    blocks.push((it, ContainsItems::No));
                } else if let Some(id_kind) = ErasedFileAstId::should_alloc(kind) {
                    // We don't associate items with blocks, only with items, since block IDs can be quite unstable.
                    // FIXME: Is this the correct thing to do? Macro calls might actually be more incremental if
                    // associated with blocks (not sure). Either way it's not a big deal.
                    let is_item = matches!(
                        id_kind,
                        ErasedFileAstIdKind::Enum
                            | ErasedFileAstIdKind::Struct
                            | ErasedFileAstIdKind::Union
                            | ErasedFileAstIdKind::ExternCrate
                            | ErasedFileAstIdKind::MacroDef
                            | ErasedFileAstIdKind::MacroRules
                            | ErasedFileAstIdKind::Module
                            | ErasedFileAstIdKind::Static
                            | ErasedFileAstIdKind::Trait
                            | ErasedFileAstIdKind::Const
                            | ErasedFileAstIdKind::Fn
                            | ErasedFileAstIdKind::TypeAlias
                            | ErasedFileAstIdKind::ExternBlock
                            | ErasedFileAstIdKind::Use
                            | ErasedFileAstIdKind::Impl
                    );
                    if let Some((last_block, already_allocated @ ContainsItems::No)) =
                        blocks.last_mut()
                        && (is_item
                            || (id_kind == ErasedFileAstIdKind::MacroCall && {
                                let parent = tree.parent(it);
                                parent.is_some_and(|it| kind_of(tree, it) == SyntaxKind::MACRO_EXPR)
                                    && parent.and_then(|it| tree.parent(it)).is_some_and(|it| {
                                        matches!(
                                            kind_of(tree, it),
                                            SyntaxKind::EXPR_STMT | SyntaxKind::STMT_LIST
                                        )
                                    })
                            }))
                    {
                        visit(Step::Block { at: *last_block, parent });
                        *already_allocated = ContainsItems::Yes;
                    }
                    let idx = visit(Step::Item { at: it, kind: id_kind, parent });
                    next_layer.extend(tree.children(it).map(|child| (child, idx)));
                    it = tree.end(it);
                    continue;
                }
                it += 1;
            }
            while blocks.last().is_some_and(|&(block, _)| tree.end(block) <= end) {
                blocks.pop();
            }
        }
        std::mem::swap(&mut curr_layer, &mut next_layer);
        assert!(blocks.is_empty(), "didn't leave all BlockExprs");
    }
}

/// The ids of the body at `root` of `tree`, which has `slots` hash values from `first_slot`
/// on, as [`BodyIds`] words: each node's is its number among the body's nodes of its kind, in
/// the walk's order.
fn body_ids(tree: &FlatTree, root: u32, first_slot: u32, slots: u32) -> Box<[u32]> {
    frame::task(Tag::Parse, |frame| {
        let mut next = [0u32; ErasedFileAstIdKind::Root as usize + 1];
        let mut words: Vec<u32, &Frame> = Vec::new_in(frame);
        walk::<()>(frame, tree, root, false, |step| {
            let (at, kind) = match step {
                Step::Block { at, .. } => (at, ErasedFileAstIdKind::BlockExpr),
                Step::Item { at, kind, .. } => (at, kind),
                Step::Body { .. } => unreachable!("a body's own walk leaves no body apart"),
            };
            let n = next[kind as usize];
            next[kind as usize] += 1;
            let slot = n >> INDEX_BITS;
            // Each node of a kind has a token of its own, so there are fewer than the body has
            // bytes, which its slots were counted from.
            assert!(slot < slots, "more {kind:?} nodes in a fn body than it has bytes");
            let id = pack((first_slot + slot) as u16, n & INDEX_MASK, true, kind as u32);
            words.extend_from_slice(&entry(kind_of(tree, at), tree.range(at), ErasedFileAstId(id)));
            Some(())
        });
        let n = words.len() / 4;
        sort_entries(words.as_chunks_mut::<4>().0);
        words.reserve_exact(2 * n);
        for number in 0..n {
            let id = words[4 * number + ID];
            words.extend_from_slice(&[id, number as u32]);
        }
        sort_pairs(words[4 * n..].as_chunks_mut::<2>().0);
        // alloc: the body's ids, the result, once per body the first time it is asked about, at
        // its exact length
        Box::from(&words[..])
    })
}

/// A bit per hash value a body may be named by: taken or not.
type Slots = BitArray<{ (SLOTS / 64) as usize }, u32>;

/// The first of `slots` hash values in a row none of which is in `used`, looked for from
/// `start` on, marked used.
fn take_slots(used: &mut Slots, start: u16, slots: u32) -> u32 {
    assert!(slots <= SLOTS, "a fn body too long to number its nodes ({slots} slots)");
    let mut first = u32::from(start);
    for _ in 0..2 * SLOTS {
        if first + slots > SLOTS {
            first = 0;
        }
        match (first..first + slots).rfind(|&it| used.contains(it)) {
            Some(it) => first = it + 1,
            None => {
                used.insert_range(first..first + slots);
                return first;
            }
        }
    }
    panic!("the fn bodies of one file need more than {SLOTS} slots to number their nodes")
}

/// `found`'s bodies of `tree`, each with the owner it was found in, given their slots in the
/// order they were found.
fn place_bodies(frame: &Frame, tree: &Arc<FlatTree>, found: &[(u32, ErasedFileAstId)]) -> Bodies {
    let mut used = Slots::EMPTY;
    let source = tree.source();
    let mut placed: Vec<Placed, &Frame> = Vec::with_capacity_in(found.len(), frame);
    placed.extend(found.iter().map(|&(at, owner)| {
        let range = tree.range(at);
        let slots = u32::from(range.len()) / (INDEX_MASK + 1) + 1;
        let first_slot = take_slots(&mut used, u16_hash(FixedState.hash_one(owner)), slots);
        Placed { range, first_slot, slots, text_hash: FixedState.hash_one(&source[range]), at }
    }));
    Bodies::new(tree, &mut placed)
}

/// What an item's id is made in: the parent it is told of, but none under an extern block.
fn parent_of(parent: Option<u32>, entries: &[Entry]) -> Option<ErasedFileAstId> {
    let parent = entry_id(&entries[parent? as usize]);
    // See the comment on `ErasedAssocItemFileAstId` for why is this.
    // FIXME: Technically there could be an extern block inside another item, e.g.:
    // ```
    // fn foo() {
    //     extern "C" {
    //         fn bar();
    //     }
    // }
    // ```
    // Here we want to make `foo()` the parent of `bar()`, but we make it `None`.
    // Shouldn't be a big deal though.
    (parent.kind() != ErasedFileAstIdKind::ExternBlock as u32).then_some(parent)
}

/// The tree `node` is in, shared, and its number there: [`syntax::view::Node::flat`], with the
/// count a map keeps of the tree to walk its bodies when asked. The same `(tree, at)` a
/// cursor's `flat` gives. A node in a part is in a parsed part, so nothing is parsed.
#[inline]
fn shared(node: syntax::view::Node<'_>) -> (&Arc<FlatTree>, u32) {
    let file = node.shared_file();
    let NodeId { part, at } = node.id();
    match part {
        NodeId::FILE => (file, at),
        part => (file.part(part), at),
    }
}

impl AstIdMap {
    /// The map of the file whose root is `root`. A view is made only of an item, to name it,
    /// and a fn's body is not walked into, but kept to be walked when asked about.
    pub fn from_view(root: syntax::view::Node<'_>) -> AstIdMap {
        assert!(root.parent().is_none(), "a map is of a file's root");
        let (tree, at) = shared(root);
        frame::task(Tag::Parse, |frame| {
            // Taken, so a map built inside a build (none is today) starts from an empty one.
            let mut index_map = NEXT_INDEX.take();
            index_map.0.clear();

            // Ensure we allocate the root.
            let mut entries: Vec<Entry, &Frame> = Vec::new_in(frame);
            entries.push(entry(kind_of(tree, at), tree.range(at), ROOT_ERASED_FILE_AST_ID));

            let mut found: Vec<(u32, ErasedFileAstId), &Frame> = Vec::new_in(frame);
            walk::<u32>(frame, tree, at, true, |step| match step {
                Step::Block { at, parent } => {
                    let parent = parent_of(parent, &entries);
                    let kind = kind_of(tree, at);
                    let id = block_expr_ast_id(kind, &mut index_map, parent.as_ref())
                        .expect("not a BlockExpr");
                    entries.push(entry(kind, tree.range(at), id));
                    None
                }
                Step::Item { at, kind: _, parent } => {
                    let parent = parent_of(parent, &entries);
                    // Outside bodies, so in the file's tree, not a part.
                    let item = syntax::view::Node::at(tree, NodeId { part: NodeId::FILE, at })
                        .into_node()
                        .expect("a node");
                    let id = ErasedFileAstId::ast_id_for(item, &mut index_map, parent.as_ref())
                        .expect("this node should have an ast id");
                    entries.push(entry(kind_of(tree, at), tree.range(at), id));
                    Some(entries.len() as u32 - 1)
                }
                Step::Body { at, parent } => {
                    let owner =
                        parent.map_or(ROOT_ERASED_FILE_AST_ID, |it| entry_id(&entries[it as usize]));
                    found.push((at, owner));
                    None
                }
            });
            NEXT_INDEX.set(index_map);
            let bodies = place_bodies(frame, tree, &found);
            AstIdMap::new(&entries, bodies)
        })
    }

    /// The map of `entries`, the root's first, and `bodies`.
    fn new(entries: &[Entry], bodies: Bodies) -> AstIdMap {
        // alloc: the map's entries, its result, once per map built, at their exact length
        let entries: Box<[Entry]> = Box::from(entries);
        let by_start = Index::new(entries.len(), |it| entries[it][START]);
        let by_id = Index::new(entries.len(), |it| entries[it][ID]);
        AstIdMap { entries, by_start, by_id, bodies }
    }

    /// The ids of the nodes of `node`, a node of the tree this map is of, as nodes of
    /// `relative`: the same node, the root of a tree of its own. What a body is lowered from
    /// when it is lowered without the rest of its file, so that the map stays the same while
    /// the text around `node` moves it. The first entry is `node` itself.
    ///
    /// Either tree may be an outline or a full parse. A fn body in `node` is not walked: the
    /// map made keeps it, with its slots, to be walked in `relative` when asked about, so
    /// lowering one body of an item parses no other.
    pub fn relative_to_view(
        &self,
        node: syntax::view::Node<'_>,
        relative: syntax::view::Node<'_>,
    ) -> AstIdMap {
        self.relative(shared(node), shared(relative), &[])
    }

    /// [`AstIdMap::relative_to_view`] for a `relative` that is `node` with each node of `elided`
    /// (nodes under `node`, in this map's tree, sorted by where they begin and disjoint)
    /// replaced by an empty node of its kind: nothing in them has an id or a body in the map
    /// made, and what follows each is moved back by the lengths elided before it. What an
    /// item's signature or a file's outline is lowered from, bodies left out, so that the map
    /// stays the same while the bodies change.
    pub fn relative_without_view(
        &self,
        node: syntax::view::Node<'_>,
        relative: syntax::view::Node<'_>,
        elided: &[SyntaxNodePtr],
    ) -> AstIdMap {
        self.relative(shared(node), shared(relative), elided)
    }

    /// What [`AstIdMap::relative_without_view`] is, of the trees `node` and `relative` are in
    /// and their numbers there.
    fn relative(
        &self,
        (tree, at): (&Arc<FlatTree>, u32),
        (relative_tree, relative_at): (&Arc<FlatTree>, u32),
        elided: &[SyntaxNodePtr],
    ) -> AstIdMap {
        let (tree, at) = expanded(tree, at);
        let (start, relative_range) = (tree.range(at).start(), relative_tree.range(relative_at));
        debug_assert!(
            elided.windows(2).all(|it| it[0].text_range().end() <= it[1].text_range().start()),
            "elided nodes are sorted and disjoint"
        );
        frame::task(Tag::Parse, |frame| {
            // Where each elided node ends, and the lengths elided up to and with it: an offset
            // moves back by the lengths of every elided node ending at or before it.
            let mut cut = Vec::with_capacity_in(elided.len(), frame);
            let mut removed = TextSize::new(0);
            for ptr in elided {
                removed += ptr.text_range().len();
                cut.push((ptr.text_range().end(), removed));
            }
            debug_assert!(
                tree.kind(at) == relative_tree.kind(relative_at)
                    && tree.range(at).len() - removed == relative_range.len(),
                "not the same node"
            );
            // Each end is moved on its own: a node holding an elided one keeps its start and
            // loses the elided length from its end.
            let shift = |offset: TextSize| match cut.partition_point(|&(end, _)| end <= offset) {
                0 => offset,
                ends => offset - cut[ends - 1].1,
            };
            let moved = |range: TextRange| {
                TextRange::new(shift(range.start()), shift(range.end())) - start
                    + relative_range.start()
            };
            // Under a node in a body, everything is that body's.
            let in_body = self.bodies.holding(tree.range(at)).is_some();
            let mut entries: Vec<Entry, &Frame> = Vec::new_in(frame);
            let mut placed: Vec<Placed, &Frame> = Vec::new_in(frame);
            let end = tree.end(at);
            let mut it = at;
            // The next elided node: the walk meets them in the order they begin.
            let mut next = 0;
            while it < end {
                if tree.is_token(it) {
                    it += 1;
                    continue;
                }
                let kind = kind_of(tree, it);
                let range = tree.range(it);
                if elided.get(next).is_some_and(|ptr| ptr.kind() == kind && ptr.text_range() == range)
                {
                    next += 1;
                    it = tree.end(it);
                    continue;
                }
                if !in_body && is_body(tree, it, kind) {
                    let body = self
                        .bodies
                        .holding(range)
                        .filter(|&body| self.bodies.range(body) == range)
                        .expect("a fn body of the node is one of the map's");
                    let range = moved(range);
                    let relative_at = find_node(relative_tree, relative_at, kind, range)
                        .expect("the relative node has the body");
                    placed.push(self.bodies.moved(body, range, relative_at));
                    it = tree.end(it);
                    continue;
                }
                // `node` itself first, a file's root too, whose id no kind allocates.
                let may_have_id = it == at
                    || ast::BlockExpr::can_cast(kind)
                    || ErasedFileAstId::should_alloc(kind).is_some();
                if may_have_id && let Some(id) = self.id_at(key_of(kind, range)) {
                    entries.push(entry(kind, moved(range), id));
                }
                it += 1;
            }
            assert_eq!(next, elided.len(), "every elided node is a node under the one mapped");
            AstIdMap::new(&entries, Bodies::new(relative_tree, &mut placed))
        })
    }

    /// The map's entries outside fn bodies, `[start, end, kind, id]`: the root first, then in
    /// the walk's order, breadth first (an item before what is in it), not sorted by start.
    /// What an edit's unchanged text is compared by as one slice.
    pub fn entries(&self) -> &[[u32; 4]] {
        &self.entries
    }

    /// The root node.
    pub fn root(&self) -> SyntaxNodePtr {
        entry_ptr(&self.entries[0])
    }

    /// [`AstIdMap::ast_id_for_ptr`] of a view of an `N`.
    pub fn ast_id_of_view<N: AstIdNode + syntax::view::Typed>(
        &self,
        item: N::Of<'_>,
    ) -> FileAstId<N> {
        self.ast_id_for_ptr(AstPtr::of(item))
    }

    /// [`AstIdMap::ast_id_for_ptr_for_block`] of a view: blocks may not be allocated (if they
    /// have no items).
    pub fn ast_id_for_block_view(
        &self,
        block: syntax::view::BlockExpr<'_>,
    ) -> Option<FileAstId<ast::BlockExpr>> {
        self.ast_id_for_ptr_for_block(AstPtr::of(block))
    }

    pub fn ast_id_for_ptr<N: AstIdNode>(&self, ptr: AstPtr<N>) -> FileAstId<N> {
        let ptr = ptr.syntax_node_ptr();
        FileAstId { raw: self.erased_ast_id(ptr), _marker: PhantomData }
    }

    /// Blocks may not be allocated (if they have no items), so they have a different API.
    pub fn ast_id_for_ptr_for_block(
        &self,
        ptr: AstPtr<ast::BlockExpr>,
    ) -> Option<FileAstId<ast::BlockExpr>> {
        let ptr = ptr.syntax_node_ptr();
        self.try_erased_ast_id(ptr).map(|raw| FileAstId { raw, _marker: PhantomData })
    }

    fn erased_ast_id(&self, ptr: SyntaxNodePtr) -> ErasedFileAstId {
        self.try_erased_ast_id(ptr).unwrap_or_else(|| {
            panic!(
                "Can't find SyntaxNodePtr {:?} in AstIdMap:\n{:?}",
                ptr,
                // alloc: a panic's message
                self.entries.iter().map(entry_id).collect::<Vec<_>>(),
            )
        })
    }

    /// The id of the node at `ptr`; of a node in a fn body, after walking that body if no one
    /// asked about it before.
    fn try_erased_ast_id(&self, ptr: SyntaxNodePtr) -> Option<ErasedFileAstId> {
        self.id_at(key(ptr))
    }

    #[inline]
    fn id_at(&self, key: Key) -> Option<ErasedFileAstId> {
        if let Some(it) = self.by_start.find(key[0], |it| is_at(&self.entries[it], key)) {
            return Some(entry_id(&self.entries[it]));
        }
        let range = TextRange::new(key[0].into(), key[1].into());
        self.bodies.ids(self.bodies.holding(range)?).id(key)
    }

    // Don't bound on `AstIdNode` here, because `BlockExpr`s are also valid here (`ast::BlockExpr`
    // doesn't always have a matching `FileAstId`, but a `FileAstId<ast::BlockExpr>` always has
    // a matching node).
    pub fn get<N: AstNode>(&self, id: FileAstId<N>) -> AstPtr<N> {
        let ptr = self.get_erased(id.raw);
        AstPtr::try_from_raw(ptr)
            .unwrap_or_else(|| panic!("AstIdMap node mismatch with node `{ptr:?}`"))
    }

    /// The nodes this map has an id of: every item of the file, and the blocks with items,
    /// those in fn bodies too (which walks every body no one asked about before).
    pub fn ptrs(&self) -> impl Iterator<Item = SyntaxNodePtr> + '_ {
        let bodies = (0..self.bodies.len())
            .flat_map(move |body| self.bodies.ids(body).entries.iter().map(entry_ptr));
        self.entries.iter().map(entry_ptr).chain(bodies)
    }

    /// The node of `id`, when this map has it: a map [`AstIdMap::relative_to_view`] a node has
    /// only the ids of what is in that node. The id of a node in a fn body walks that body if no
    /// one asked about it before.
    pub fn try_get_erased(&self, id: ErasedFileAstId) -> Option<SyntaxNodePtr> {
        if id.in_body() {
            return self.bodies.ids(self.bodies.of(id)?).ptr(id);
        }
        self.by_id.find(id.0, |_| true).map(|it| entry_ptr(&self.entries[it]))
    }

    pub fn get_erased(&self, id: ErasedFileAstId) -> SyntaxNodePtr {
        self.try_get_erased(id).unwrap_or_else(|| {
            panic!(
                "Can't find ast id {:?} in AstIdMap:\n{:?}\n{:?}",
                id,
                // alloc: a panic's message
                self.entries.iter().map(|it| (entry_ptr(it), entry_id(it))).collect::<Vec<_>>(),
                self.bodies,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        hash::BuildHasher,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use blaze_hash::{FixedState, HashMap, HashSet};
    use blaze_ids::rng::SplitMix64;
    use syntax::{
        AstNode, AstPtr, Edition, Parse, SourceFile, SyntaxKind, SyntaxNode, SyntaxNodePtr,
        TextRange, TextSize,
        ast::{self, HasName as _},
        view::{self, AstNode as _, HasName as _},
    };

    use super::ErasedFileAstId;
    use crate::AstIdMap;

    #[test]
    fn check_all_nodes() {
        let syntax = SourceFile::parse(
            r#"
extern crate foo;
fn foo() {
    union U {}
}
struct S;
macro_rules! m {}
macro m2() {}
trait Trait {}
impl Trait for S {}
impl S {}
impl m!() {}
impl m2!() for m!() {}
type T = i32;
enum E {
    V1(),
    V2 {},
    V3,
}
struct S; // duplicate
extern "C" {
    static S: i32;
}
static mut S: i32 = 0;
const FOO: i32 = 0;
        "#,
            Edition::CURRENT,
        );
        let ast_id_map = AstIdMap::from_view(syntax.root());
        for node in syntax.root().descendants() {
            if !matches!(
                node.kind(),
                SyntaxKind::EXTERN_CRATE
                    | SyntaxKind::FN
                    | SyntaxKind::UNION
                    | SyntaxKind::STRUCT
                    | SyntaxKind::MACRO_RULES
                    | SyntaxKind::MACRO_DEF
                    | SyntaxKind::MACRO_CALL
                    | SyntaxKind::TRAIT
                    | SyntaxKind::IMPL
                    | SyntaxKind::TYPE_ALIAS
                    | SyntaxKind::ENUM
                    | SyntaxKind::VARIANT
                    | SyntaxKind::EXTERN_BLOCK
                    | SyntaxKind::STATIC
                    | SyntaxKind::CONST
            ) {
                continue;
            }
            let ptr = view::ptr(node);
            let ast_id = ast_id_map.erased_ast_id(ptr);
            let turn_back = ast_id_map.get_erased(ast_id);
            assert_eq!(ptr, turn_back);
        }
    }

    #[test]
    fn different_names_get_different_hashes() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {}
fn bar() {}
        "#,
            Edition::CURRENT,
        );
        let ast_id_map = AstIdMap::from_view(syntax.root());
        let fns = syntax.root().descendants().filter_map(view::Fn::cast).collect::<Vec<_>>();
        let [foo_fn, bar_fn] = fns.as_slice() else {
            panic!("not exactly 2 functions");
        };
        let foo_fn_id = ast_id_map.ast_id_of_view::<ast::Fn>(*foo_fn);
        let bar_fn_id = ast_id_map.ast_id_of_view::<ast::Fn>(*bar_fn);
        assert_ne!(foo_fn_id.raw.hash_value(), bar_fn_id.raw.hash_value(), "hashes are equal");
    }

    #[test]
    fn different_parents_get_different_hashes() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {
    m!();
}
fn bar() {
    m!();
}
        "#,
            Edition::CURRENT,
        );
        let ast_id_map = AstIdMap::from_view(syntax.root());
        let macro_calls =
            syntax.root().descendants().filter_map(view::MacroCall::cast).collect::<Vec<_>>();
        let [macro_call_foo, macro_call_bar] = macro_calls.as_slice() else {
            panic!("not exactly 2 macro calls");
        };
        let macro_call_foo_id = ast_id_map.ast_id_of_view::<ast::MacroCall>(*macro_call_foo);
        let macro_call_bar_id = ast_id_map.ast_id_of_view::<ast::MacroCall>(*macro_call_bar);
        assert_ne!(
            macro_call_foo_id.raw.hash_value(),
            macro_call_bar_id.raw.hash_value(),
            "hashes are equal"
        );
    }

    /// The entries lent are the root's, then each item's outside bodies in the walk's order,
    /// with the ids lookups give.
    #[test]
    fn entries_are_lent_root_first_in_the_walks_order() {
        let text: Arc<str> = "struct A;\nimpl T for A { fn b() { struct C; } }\nfn d() {}\n".into();
        let file = full_of(&text);
        let map = AstIdMap::from_view(file.root());
        let entries = map.entries();
        assert_eq!(super::entry_ptr(&entries[0]), view::ptr(file.root()));
        let kinds: Vec<SyntaxKind> = entries.iter().map(|it| super::entry_ptr(it).kind()).collect();
        let want = [
            SyntaxKind::SOURCE_FILE,
            SyntaxKind::STRUCT,
            SyntaxKind::IMPL,
            SyntaxKind::FN,
            SyntaxKind::FN,
        ];
        assert_eq!(kinds, want, "`b` is under the impl, so after `d`; `C` is in a body");
        for entry in entries {
            let ptr = super::entry_ptr(entry);
            assert_eq!(map.try_erased_ast_id(ptr), Some(super::entry_id(entry)));
        }
    }

    #[test]
    fn blocks_with_no_items_have_no_id() {
        let syntax = SourceFile::parse(
            r#"
fn foo() {
    let foo = 1;
    bar(foo);
}
        "#,
            Edition::CURRENT,
        );
        let ast_id_map = AstIdMap::from_view(syntax.root());
        let block = syntax.root().descendants().find_map(view::BlockExpr::cast).expect("no block");
        assert!(ast_id_map.ast_id_for_block_view(block).is_none());
    }

    fn outline_of(text: &Arc<str>) -> Parse<SourceFile> {
        SourceFile::parse_outline(text.clone(), Edition::CURRENT)
    }

    fn full_of(text: &Arc<str>) -> Parse<SourceFile> {
        SourceFile::parse_shared(text.clone(), Edition::CURRENT)
    }

    fn count(ids: &[(SyntaxNodePtr, ErasedFileAstId)], kind: SyntaxKind) -> usize {
        ids.iter().filter(|(ptr, _)| ptr.kind() == kind).count()
    }

    /// The lazy bodies of `parse`'s tree that are parsed.
    fn parsed(parse: &Parse<SourceFile>) -> Vec<syntax::TextRange> {
        let (tree, _) = parse.root().flat();
        tree.lazies().filter(|&it| tree.is_parsed(it)).map(|it| tree.range(it)).collect()
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
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

    /// The parser's test data and the `syntax` crate's own sources.
    fn corpus() -> Vec<(PathBuf, Arc<str>)> {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut paths = Vec::new();
        for dir in ["parser/test_data/parser", "syntax/src"] {
            rust_files(&crates.join(dir), &mut paths);
        }
        assert!(paths.len() > 300, "the corpus is {} files", paths.len());
        read_all(paths)
    }

    /// Each of `paths` with its text.
    fn read_all(paths: Vec<PathBuf>) -> Vec<(PathBuf, Arc<str>)> {
        paths
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                (path, Arc::from(text))
            })
            .collect()
    }

    /// What the views are checked on: every file of the parser's test data and `span`'s own
    /// sources (`src/*.rs`).
    fn view_corpus() -> Vec<(PathBuf, Arc<str>)> {
        let span = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut paths = Vec::new();
        rust_files(&span.join("../parser/test_data"), &mut paths);
        assert!(paths.len() > 500, "the parser's test data is {} files", paths.len());
        let src = span.join("src");
        let mut own: Vec<PathBuf> = std::fs::read_dir(&src)
            .unwrap_or_else(|e| panic!("{}: {e}", src.display()))
            .map(|it| it.expect("a directory entry").path())
            .filter(|it| it.is_file() && it.extension().is_some_and(|it| it == "rs"))
            .collect();
        own.sort();
        assert!(own.len() >= 5, "span's sources are {} files", own.len());
        paths.extend(own);
        read_all(paths)
    }

    /// Each node a map has an id of, with it.
    fn ids(map: &AstIdMap) -> Vec<(SyntaxNodePtr, ErasedFileAstId)> {
        map.ptrs().map(|ptr| (ptr, map.erased_ast_id(ptr))).collect()
    }

    #[test]
    fn an_outline_has_the_ids_of_a_full_parse() {
        for (path, text) in corpus() {
            let path = path.display();
            let full = full_of(&text);
            let full_map = AstIdMap::from_view(full.root());
            let want = ids(&full_map);
            let distinct: HashSet<_> = want.iter().map(|&(_, id)| id).collect();
            assert_eq!(distinct.len(), want.len(), "{path}: two nodes have one id");
            let with_id: HashSet<_> = want.iter().map(|&(ptr, _)| ptr).collect();
            for node in full.root().descendants() {
                let ptr = view::ptr(node);
                if ErasedFileAstId::should_alloc(node.kind()).is_some() {
                    assert!(with_id.contains(&ptr), "{path}: {node:?} has no id");
                }
                if full_map.try_erased_ast_id(ptr).is_some() {
                    assert!(with_id.contains(&ptr), "{path}: `ptrs` misses {node:?}");
                }
            }

            for reverse in [false, true] {
                let outline = outline_of(&text);
                let map = AstIdMap::from_view(outline.root());
                assert!(parsed(&outline).is_empty(), "{path}: making the map parsed a body");
                assert!(map == full_map, "{path}: the outline's map is not the full parse's");
                assert_eq!(FixedState.hash_one(&map), FixedState.hash_one(&full_map));
                let mut order: Vec<_> = want.iter().copied().collect();
                if reverse {
                    order.reverse();
                }
                for (i, (ptr, id)) in order.into_iter().enumerate() {
                    match i % 2 {
                        0 => assert_eq!(map.try_erased_ast_id(ptr), Some(id), "{path}: {ptr:?}"),
                        _ => assert_eq!(map.try_get_erased(id), Some(ptr), "{path}: {id:?}"),
                    }
                }
                assert!(map == full_map, "{path}: walking bodies changed the map");
            }

            let outline = outline_of(&text);
            let map = AstIdMap::from_view(outline.root());
            std::thread::scope(|scope| {
                for thread in 0..4 {
                    let (map, want) = (&map, &want);
                    scope.spawn(move || {
                        let skip = thread * want.len() / 4;
                        for &(ptr, id) in want.iter().cycle().skip(skip).take(want.len()) {
                            assert_eq!(map.try_erased_ast_id(ptr), Some(id));
                            assert_eq!(map.try_get_erased(id), Some(ptr));
                        }
                    });
                }
            });
            let got: HashMap<_, _> = ids(&map).into_iter().collect();
            assert!(got == want.into_iter().collect::<HashMap<_, _>>(), "{path}: `ptrs` differ");
        }
    }

    #[test]
    fn a_body_is_walked_when_asked_about() {
        let text: Arc<str> =
            "fn a() { m!(); struct S; }\nfn b() { n!(); }\nimpl T { fn c() { fn d() { o!() } } }\n"
                .into();
        let full = full_of(&text);
        let call = full
            .root()
            .descendants()
            .filter_map(view::MacroCall::cast)
            .find(|it| it.path().is_some_and(|it| it.syntax().text() == "n"))
            .expect("`n!()`");
        let body = full
            .root()
            .descendants()
            .filter_map(view::Fn::cast)
            .find(|it| it.name().is_some_and(|it| it.syntax().text() == "b"))
            .and_then(|it| it.body())
            .expect("`b`'s body")
            .syntax()
            .text_range();
        let call_ptr = AstPtr::<ast::MacroCall>::of(call);

        let outline = outline_of(&text);
        let map = AstIdMap::from_view(outline.root());
        let (tree, _) = outline.root().flat();
        assert_eq!(tree.lazies().count(), 3, "the bodies of `a`, `b` and `c`");
        assert!(parsed(&outline).is_empty());
        let id = map.ast_id_for_ptr(call_ptr);
        assert!(id.erase().in_body());
        assert_eq!(parsed(&outline), [body], "asking for `n!()` parses `b`'s body alone");
        assert_eq!(map.get(id), call_ptr);

        // An id from another map of the same text finds its body by its slots.
        let again = outline_of(&text);
        let map = AstIdMap::from_view(again.root());
        assert_eq!(map.get(id), call_ptr);
        assert_eq!(parsed(&again), [body]);
    }

    #[test]
    fn many_bodies_with_the_same_macros_have_distinct_ids() {
        let text: String =
            (0..5000).map(|i| format!("fn f{i}() {{ a!(); a!(); {{ b!(); }} }}\n")).collect();
        let text: Arc<str> = text.into();
        let full_map = AstIdMap::from_view(full_of(&text).root());
        let want = ids(&full_map);
        assert_eq!(count(&want, SyntaxKind::FN), 5000);
        assert_eq!(count(&want, SyntaxKind::MACRO_CALL), 3 * 5000);
        let distinct: HashSet<_> = want.iter().map(|&(_, id)| id).collect();
        assert_eq!(distinct.len(), want.len());
        let map = AstIdMap::from_view(outline_of(&text).root());
        for &(ptr, id) in want.iter().rev() {
            assert_eq!(map.try_get_erased(id), Some(ptr));
        }
    }

    #[test]
    fn relative_to_an_outline_or_a_full_parse() {
        let text: Arc<str> =
            "struct A;\nimpl T for A {\n    fn c() { m!(); fn d() { n!() } }\n    fn e() {}\n}\n"
                .into();
        let full_map = AstIdMap::from_view(full_of(&text).root());
        for file in [outline_of(&text), full_of(&text)] {
            let map = AstIdMap::from_view(file.root());
            let anchor =
                file.root().children().find(|it| it.kind() == SyntaxKind::IMPL).expect("the impl");
            let (start, range) = (anchor.text_range().start(), anchor.text_range());
            let anchor_text: Arc<str> = Arc::from(&text[range]);
            for relative_root in [outline_of(&anchor_text), full_of(&anchor_text)] {
                let relative = relative_root
                    .root()
                    .children()
                    .find(|it| it.kind() == SyntaxKind::IMPL)
                    .expect("the impl");
                let relative_map = map.relative_to_view(anchor, relative);
                assert_eq!(relative_map.root().text_range(), relative.text_range());
                let inside = ids(&full_map)
                    .into_iter()
                    .filter(|(ptr, _)| range.contains_range(ptr.text_range()))
                    .collect::<Vec<_>>();
                assert_eq!(count(&inside, SyntaxKind::IMPL), 1);
                assert_eq!(count(&inside, SyntaxKind::FN), 3, "`c`, `d` and `e`");
                assert_eq!(count(&inside, SyntaxKind::MACRO_CALL), 2);
                for (ptr, id) in inside {
                    let moved = SyntaxNodePtr::from_parts(ptr.kind(), ptr.text_range() - start);
                    assert_eq!(relative_map.try_erased_ast_id(moved), Some(id));
                    assert_eq!(relative_map.try_get_erased(id), Some(moved));
                }
            }
            assert!(parsed(&file).is_empty(), "the file's bodies are walked in the relative trees");
        }
    }

    /// `anchor` as a tree of its own with each node of `elided` under it (sorted and disjoint)
    /// replaced by an empty node of its kind, as hir-def's `SignatureSyntax` makes one.
    fn without_all<'t>(anchor: view::Node<'t>, elided: &[view::Node<'t>]) -> Arc<syntax::FlatTree> {
        Arc::new(anchor.subtree_without(elided))
    }

    /// With the node of `elided_kind` under the first node of `kind` left out, the map has only
    /// that node, with its id in the file, at its range in the tree without it, and no body.
    fn only_the_item_without(text: &str, kind: SyntaxKind, elided_kind: SyntaxKind) {
        let text: Arc<str> = text.into();
        for file in [outline_of(&text), full_of(&text)] {
            let map = AstIdMap::from_view(file.root());
            let anchor = file.root().descendants().find(|it| it.kind() == kind).expect("the item");
            let elided = anchor
                .children()
                .find(|it| it.kind() == elided_kind)
                .expect("the node to leave out");
            let tree = without_all(anchor, &[elided]);
            let relative = view::Node::root(&tree);
            let relative_map = map.relative_without_view(anchor, relative, &[view::ptr(elided)]);
            assert_eq!(relative_map.root(), view::ptr(relative));
            assert_eq!(ids(&relative_map).len(), 1, "{kind:?} alone");
            assert_eq!(relative_map.bodies.len(), 0);
            assert_eq!(
                relative_map.try_erased_ast_id(view::ptr(relative)),
                Some(map.erased_ast_id(view::ptr(anchor)))
            );
        }
    }

    #[test]
    fn relative_without_a_fn_body() {
        only_the_item_without("fn f() { struct A; }\nstruct B;\n", SyntaxKind::FN, SyntaxKind::BLOCK_EXPR);
    }

    #[test]
    fn relative_without_an_impls_items() {
        only_the_item_without(
            "struct S;\nimpl T for S { fn a() {} fn b() { x!(); } }\n",
            SyntaxKind::IMPL,
            SyntaxKind::ASSOC_ITEM_LIST,
        );
    }

    /// What follows the elided node in the item (the `;`) moves back, and the item's end with it.
    #[test]
    fn relative_without_a_const_body() {
        only_the_item_without(
            "const C: u8 = { struct D; 1 };\n",
            SyntaxKind::CONST,
            SyntaxKind::BLOCK_EXPR,
        );
    }

    /// With nothing left out, it is the map `relative_to_view` makes.
    #[test]
    fn relative_to_is_relative_without_nothing() {
        let text: Arc<str> = "struct A;\nimpl T for A {\n    fn c() { m!(); fn d() { n!() } }\n}\n".into();
        let file = full_of(&text);
        let map = AstIdMap::from_view(file.root());
        let anchor =
            file.root().children().find(|it| it.kind() == SyntaxKind::IMPL).expect("the impl");
        let tree = Arc::new(anchor.subtree());
        let relative = view::Node::root(&tree);
        assert_eq!(
            ids(&map.relative_to_view(anchor, relative)),
            ids(&map.relative_without_view(anchor, relative, &[]))
        );
    }

    /// With every fn body of a file left out (a file's outline, as hir-def's `file_outline`
    /// makes it), the map has every item outside the bodies, each with its id in the file, at
    /// its range in the tree without them, and no body.
    #[test]
    fn relative_without_every_body() {
        let text: Arc<str> =
            "fn f() { struct A; }\nstruct B;\nfn g() { m!(); }\nimpl T for B {\n    fn h() { 1 }\n}\nconst C: u8 = 2;\n"
                .into();
        for file in [outline_of(&text), full_of(&text)] {
            let root = file.root();
            let map = AstIdMap::from_view(root);
            let bodies: Vec<view::Node<'_>> = root
                .descendants()
                .filter(|it| {
                    it.kind() == SyntaxKind::BLOCK_EXPR
                        && it.parent().is_some_and(|it| it.kind() == SyntaxKind::FN)
                })
                .collect();
            assert_eq!(bodies.len(), 3);
            let tree = without_all(root, &bodies);
            let relative = view::Node::root(&tree);
            let ptrs: Vec<SyntaxNodePtr> = bodies.iter().map(|&it| view::ptr(it)).collect();
            let relative_map = map.relative_without_view(root, relative, &ptrs);
            assert_eq!(relative_map.bodies.len(), 0);

            let shift = |offset: TextSize| {
                let cut = bodies
                    .iter()
                    .filter(|it| it.text_range().end() <= offset)
                    .fold(0, |cut, it| cut + u32::from(it.text_range().len()));
                offset - TextSize::new(cut)
            };
            let mut kept = 0;
            for node in root.descendants() {
                if bodies.iter().any(|it| it.text_range().contains_range(node.text_range())) {
                    continue;
                }
                let Some(id) = map.try_erased_ast_id(view::ptr(node)) else { continue };
                let range = node.text_range();
                let moved = TextRange::new(shift(range.start()), shift(range.end()));
                let in_relative = SyntaxNodePtr::from_parts(node.kind(), moved);
                assert_eq!(relative_map.try_get_erased(id), Some(in_relative), "{node:?}");
                assert_eq!(relative_map.try_erased_ast_id(in_relative), Some(id));
                let found = view::try_to_node(in_relative, relative);
                assert!(found.is_some(), "{node:?} is in the outline");
                kept += 1;
            }
            assert_eq!(ids(&relative_map).len(), kept, "only what is outside the bodies");
            assert!(kept >= 7, "the file, its items, the impl's fn: {kept}");
        }
    }

    /// What a map keeps, read from its columns and not its indexes: each node it has an id
    /// of, with the id (walking every body).
    fn stored(map: &AstIdMap) -> Vec<(SyntaxNodePtr, ErasedFileAstId)> {
        let pair = |it: &super::Entry| (super::entry_ptr(it), super::entry_id(it));
        let bodies = (0..map.bodies.len())
            .flat_map(|body| map.bodies.ids(body).entries.iter().map(pair).collect::<Vec<_>>());
        map.entries.iter().map(pair).chain(bodies).collect()
    }

    /// Looking an id up by node and a node up by id allocates nothing, once a body's ids are
    /// made (the first lookup in a body walks it).
    #[test]
    fn lookups_allocate_nothing_once_bodies_are_walked() {
        for (path, text) in corpus() {
            let full = full_of(&text);
            let outline = outline_of(&text);
            for map in [AstIdMap::from_view(full.root()), AstIdMap::from_view(outline.root())] {
                let ids: Vec<ErasedFileAstId> = stored(&map).into_iter().map(|(_, id)| id).collect();
                let ptrs: Vec<SyntaxNodePtr> = full.root().descendants().map(view::ptr).collect();
                let look = || {
                    for &ptr in &ptrs {
                        std::hint::black_box(map.try_erased_ast_id(ptr));
                    }
                    for &id in &ids {
                        std::hint::black_box(map.try_get_erased(id));
                    }
                };
                look();
                let what = format!("{}: ast id lookups", path.display());
                blaze_alloc_count::assert_at_most(0, &what, look);
            }
        }
    }

    /// A warm build of a map allocates its result and nothing else: the entries and two
    /// indexes, and the five body columns of a map with bodies.
    #[test]
    fn a_warm_build_allocates_only_its_map() {
        for (path, text) in corpus() {
            for parse in [full_of(&text), outline_of(&text)] {
                let map = AstIdMap::from_view(parse.root());
                let columns = if map.bodies.len() == 0 { 3 } else { 8 };
                let what = format!("{}: a warm map build", path.display());
                blaze_alloc_count::assert_at_most(columns, &what, || {
                    std::hint::black_box(AstIdMap::from_view(parse.root()));
                });
            }
        }
    }

    /// A warm walk of a body allocates its ids and nothing else.
    #[test]
    fn a_warm_body_walk_allocates_only_its_ids() {
        for (path, text) in corpus() {
            let map = AstIdMap::from_view(outline_of(&text).root());
            let bodies = &map.bodies;
            let Some(tree) = bodies.tree.as_deref() else { continue };
            for body in 0..bodies.len() {
                let (tree, at) = super::expanded(tree, bodies.ats[body]);
                let row = bodies.rows[body];
                let walk = || {
                    std::hint::black_box(super::body_ids(
                        tree,
                        at,
                        row[super::ROW_FIRST_SLOT],
                        row[super::ROW_SLOTS],
                    ));
                };
                walk();
                let what = format!("{}: a warm walk of body {body}", path.display());
                blaze_alloc_count::assert_at_most(1, &what, walk);
            }
        }
    }

    #[test]
    fn an_index_finds_what_a_linear_search_does() {
        let mut rng = SplitMix64::new(1);
        for len in [0usize, 1, 2, 3, 7, 8, 9, 100, 1000, 5000] {
            // Few distinct keys, so most are shared by many entries.
            let keys: Vec<u32> =
                (0..len).map(|_| rng.below(len as u64 / 2 + 1) as u32).collect();
            let index = super::Index::new(len, |it| keys[it]);
            assert!(index.slots.len() * 3 > len * 4, "{len} entries fill the table past 3/4");
            for key in 0..len as u32 + 10 {
                let want: Vec<usize> = (0..len).filter(|&it| keys[it] == key).collect();
                let mut asked = Vec::new();
                assert_eq!(
                    index.find(key, |it| {
                        asked.push(it);
                        false
                    }),
                    None
                );
                assert_eq!(asked, want, "{len} entries, key {key}: asked in the order added");
                assert_eq!(index.find(key, |_| true), want.first().copied());
                assert_eq!(index.find(key, |it| Some(&it) == want.last()), want.last().copied());
            }
        }
        assert_eq!(super::Index::default().find(0, |_| true), None);
    }

    /// The indexes give what hash maps of the stored ids give (the layout before, which kept
    /// `HashMap`s by node and by id), for every node of a file and for ids no node has.
    #[test]
    fn lookups_agree_with_maps_of_the_stored_ids() {
        for (path, text) in corpus() {
            let path = path.display();
            let full = full_of(&text);
            let outline = outline_of(&text);
            for map in [AstIdMap::from_view(full.root()), AstIdMap::from_view(outline.root())] {
                let stored = stored(&map);
                let by_ptr: HashMap<_, _> = stored.iter().copied().collect();
                let by_id: HashMap<_, _> = stored.iter().map(|&(ptr, id)| (id, ptr)).collect();
                assert_eq!(by_ptr.len(), stored.len(), "{path}: a node is kept twice");
                assert_eq!(by_id.len(), stored.len(), "{path}: an id is kept twice");
                for node in full.root().descendants() {
                    let ptr = view::ptr(node);
                    let want = by_ptr.get(&ptr).copied();
                    assert_eq!(map.try_erased_ast_id(ptr), want, "{path}: {ptr:?}");
                }
                for &(_, id) in &stored {
                    let others = [
                        id,
                        ErasedFileAstId(id.0 ^ (1 << super::HASH_BITS)),
                        ErasedFileAstId(id.0.wrapping_add(1)),
                    ];
                    for other in others {
                        let want = by_id.get(&other).copied();
                        assert_eq!(map.try_get_erased(other), want, "{path}: {other:?}");
                    }
                }
            }
        }
    }

    /// A body's ids are one slice: entries sorted by where they begin, then each id with its
    /// entry's number, sorted by id.
    #[test]
    fn a_body_keeps_its_ids_sorted_both_ways() {
        for (path, text) in corpus() {
            let path = path.display();
            let map = AstIdMap::from_view(outline_of(&text).root());
            for body in 0..map.bodies.len() {
                let ids = map.bodies.ids(body);
                assert_eq!(ids.entries.len(), ids.by_id.len(), "{path}");
                assert!(ids.entries.is_sorted(), "{path}: entries out of order");
                assert!(ids.by_id.is_sorted(), "{path}: ids out of order");
                for &[id, number] in ids.by_id {
                    assert_eq!(ids.entries[number as usize][super::ID], id, "{path}");
                }
                let range = map.bodies.range(body);
                for entry in ids.entries {
                    let ptr = super::entry_ptr(entry);
                    assert!(range.contains_range(ptr.text_range()), "{path}: {ptr:?}");
                    assert!(super::entry_id(entry).in_body(), "{path}: {ptr:?}");
                }
            }
        }
    }

    /// Equal maps hash the same, and an item more is another map.
    #[test]
    fn equality_and_hash_follow_the_ids() {
        for (path, text) in corpus().into_iter().take(60) {
            let path = path.display();
            let a = AstIdMap::from_view(full_of(&text).root());
            let b = AstIdMap::from_view(outline_of(&text).root());
            assert!(a == b, "{path}");
            assert_eq!(FixedState.hash_one(&a), FixedState.hash_one(&b), "{path}");
            let more: Arc<str> = format!("{text}\nstruct BlazeOneMore;\n").into();
            let c = AstIdMap::from_view(full_of(&more).root());
            assert!(a != c, "{path}: another item, the same map");
        }
    }

    /// The next index is keyed by `kind << 16 | hash` in one word, and numbers nodes as the key
    /// `(kind, hash)` did, also past 1024 nodes of one kind and hash.
    #[test]
    fn packed_index_keys_number_as_pairs_did() {
        use std::collections::hash_map::Entry;

        use super::{ErasedAstIdNextIndexMap, ErasedFileAstIdKind as Kind, INDEX_BITS, pack};

        #[derive(Default)]
        struct Pairs(HashMap<(Kind, u16), u32>);

        impl Pairs {
            fn new_id(&mut self, kind: Kind, data: u64) -> ErasedFileAstId {
                let initial_hash = super::u16_hash(FixedState.hash_one(data));
                let mut hash = initial_hash;
                let index = loop {
                    match self.0.entry((kind, hash)) {
                        Entry::Occupied(mut entry) => {
                            let i = entry.get_mut();
                            if *i < ((1 << INDEX_BITS) - 1) {
                                *i += 1;
                                break *i;
                            }
                        }
                        Entry::Vacant(entry) => {
                            entry.insert(0);
                            break 0;
                        }
                    }
                    hash = hash.wrapping_add(1);
                    assert_ne!(hash, initial_hash);
                };
                ErasedFileAstId(pack(hash, index, false, kind as u32))
            }
        }

        let kinds = [Kind::Fn, Kind::Struct, Kind::MacroCall, Kind::Use, Kind::Impl, Kind::Root];
        let (mut packed, mut pairs) = (ErasedAstIdNextIndexMap::default(), Pairs::default());
        let mut rng = SplitMix64::new(2);
        for _ in 0..60_000 {
            let kind = kinds[rng.index(kinds.len())];
            let data = rng.below(4);
            assert_eq!(packed.new_id(kind, data), pairs.new_id(kind, data));
        }
    }

    /// A full parse and an outline of `text`.
    fn parses(text: &Arc<str>) -> [Parse<SourceFile>; 2] {
        [full_of(text), outline_of(text)]
    }

    /// The id the handles name `node` by, made in `index_map` under `parent`: what
    /// `ast_id_for` did before it read views, the reference the views are checked against.
    fn handle_id(
        node: &SyntaxNode,
        index_map: &mut super::ErasedAstIdNextIndexMap,
        parent: Option<ErasedFileAstId>,
    ) -> ErasedFileAstId {
        use super::{
            BlockExprFileAstId, ErasedAssocItemFileAstId, ErasedFileAstIdKind as Kind,
            ErasedHasNameFileAstId, ImplFileAstId,
        };
        let text = |it: Option<String>| it.unwrap_or_default();
        let has_name = || {
            let name = ast::AnyHasName::cast(node.clone()).and_then(|it| it.name());
            text(name.map(|it| it.text().to_owned()))
        };
        let kind = match node.kind() {
            SyntaxKind::BLOCK_EXPR => Kind::BlockExpr,
            kind => ErasedFileAstId::should_alloc(kind).expect("a node with an id"),
        };
        match kind {
            Kind::Enum
            | Kind::Struct
            | Kind::Union
            | Kind::MacroDef
            | Kind::MacroRules
            | Kind::Module
            | Kind::Static
            | Kind::Trait => index_map.new_id(kind, ErasedHasNameFileAstId { name: &has_name() }),
            Kind::ExternCrate => {
                let name = ast::ExternCrate::cast(node.clone()).and_then(|it| it.name_ref());
                let name = text(name.map(|it| it.text().to_owned()));
                index_map.new_id(kind, ErasedHasNameFileAstId { name: &name })
            }
            Kind::Variant | Kind::Const | Kind::Fn | Kind::TypeAlias | Kind::MacroCall => {
                let name = match kind {
                    Kind::MacroCall => {
                        let call = ast::MacroCall::cast(node.clone()).expect("a macro call");
                        let name = call.path().and_then(|path| path.segment()?.name_ref());
                        text(name.map(|it| it.text().to_owned()))
                    }
                    _ => has_name(),
                };
                let properties = ErasedHasNameFileAstId { name: &name };
                index_map.new_id(kind, ErasedAssocItemFileAstId { parent, properties })
            }
            Kind::ExternBlock | Kind::Use | Kind::AsmExpr => index_map.new_id(kind, ()),
            Kind::Impl => {
                let item = ast::Impl::cast(node.clone()).expect("an impl");
                let type_name = |ty: Option<ast::Type>| match ty? {
                    ast::Type::PathType(it) => {
                        Some(it.path()?.segment()?.name_ref()?.text().to_owned())
                    }
                    _ => None,
                };
                let (self_ty, trait_) = (type_name(item.self_ty()), type_name(item.trait_()));
                let data = ImplFileAstId {
                    self_ty_name: self_ty.as_deref(),
                    trait_name: trait_.as_deref(),
                };
                index_map.new_id(kind, data)
            }
            Kind::BlockExpr => index_map.new_id(kind, BlockExprFileAstId { parent }),
            Kind::Fixup | Kind::NoDownmap | Kind::Root => panic!("{kind:?} is no node's"),
        }
    }

    /// The parent an entry's id is made under, as the walk tells it: the nearest item it is in
    /// (a block is none), none at the top or under an extern block.
    fn parent_in(map: &AstIdMap, node: &SyntaxNode) -> Option<ErasedFileAstId> {
        let parent = node
            .ancestors()
            .skip(1)
            .find(|it| ErasedFileAstId::should_alloc(it.kind()).is_some())?;
        let id = map.erased_ast_id(SyntaxNodePtr::new(&parent));
        (id.kind() != super::ErasedFileAstIdKind::ExternBlock as u32).then_some(id)
    }

    /// An outline's map is the full parse's, every entry and id, the bodies' too; and naming
    /// each item through a view gives the id its handle names it by, replaying the entries in
    /// the order the walk made their ids. The handles are the reference here until they go.
    #[test]
    fn a_views_map_names_items_as_the_handles_did() {
        for (path, text) in view_corpus() {
            let path = path.display();
            let want = AstIdMap::from_view(full_of(&text).root());
            for parse in parses(&text) {
                let got = AstIdMap::from_view(parse.root());
                assert!(got == want, "{path}: an outline's map is not the full parse's");
                assert_eq!(FixedState.hash_one(&got), FixedState.hash_one(&want), "{path}");
                assert_eq!(got.entries, want.entries, "{path}: the entries");
                assert_eq!(stored(&got), stored(&want), "{path}: the ids, the bodies' too");

                let node = parse.syntax_node();
                let mut index_map = super::ErasedAstIdNextIndexMap::default();
                for entry in &got.entries[1..] {
                    let ptr = super::entry_ptr(entry);
                    let item = ptr.to_node(&node);
                    let id = handle_id(&item, &mut index_map, parent_in(&got, &item));
                    assert_eq!(id, super::entry_id(entry), "{path}: {ptr:?} is named otherwise");
                }
            }
        }
    }

    /// `stored` as a map from node to id, checked to keep each node once.
    fn by_node(
        ids: Vec<(SyntaxNodePtr, ErasedFileAstId)>,
        what: &str,
    ) -> HashMap<SyntaxNodePtr, ErasedFileAstId> {
        let len = ids.len();
        let map: HashMap<_, _> = ids.into_iter().collect();
        assert_eq!(map.len(), len, "{what}: a node is kept twice");
        map
    }

    /// Each item's map `relative_to_view` makes, of the item as a tree of its own, has the ids
    /// the file's map has of the item and the nodes under it, the bodies' too, and no other,
    /// each at its range in the item's tree; and the map `relative_without_view` makes, of it
    /// with its fn bodies left out, has those outside the bodies, each moved back by the
    /// lengths left out before it; in full parses and outlines.
    #[test]
    fn relative_maps_have_the_ids_under_their_node_moved() {
        for (path, text) in view_corpus() {
            let path = path.display();
            let full = full_of(&text);
            let full_root = full.root();
            let all = stored(&AstIdMap::from_view(full_root));
            for parse in parses(&text) {
                let root = parse.root();
                let map = AstIdMap::from_view(root);
                // In the order they begin, none in another: what `relative_without_view` elides.
                let bodies: Vec<SyntaxNodePtr> = (0..map.bodies.len())
                    .map(|body| {
                        let tree = map.bodies.tree.as_deref().expect("a map with bodies keeps them");
                        let kind = super::kind_of(tree, map.bodies.ats[body]);
                        SyntaxNodePtr::from_parts(kind, map.bodies.range(body))
                    })
                    .collect();
                for entry in &map.entries[1..] {
                    let ptr = super::entry_ptr(entry);
                    if ptr.kind() == SyntaxKind::BLOCK_EXPR {
                        continue;
                    }
                    let anchor = view::to_node(ptr, root);
                    assert_eq!(view::ptr(anchor), ptr, "{path}");
                    let range = ptr.text_range();
                    let is_anchor = |up: view::Node<'_>| view::ptr(up) == ptr;
                    let under = |it: SyntaxNodePtr| {
                        range.contains_range(it.text_range())
                            && view::to_node(it, full_root).ancestors().any(is_anchor)
                    };

                    let tree = Arc::new(anchor.subtree());
                    let got = map.relative_to_view(anchor, view::Node::root(&tree));
                    let want = all.iter().filter(|&&(it, _)| under(it)).map(|&(it, id)| {
                        (SyntaxNodePtr::from_parts(it.kind(), it.text_range() - range.start()), id)
                    });
                    let what = format!("{path}: relative to {ptr:?}");
                    let want = by_node(want.collect(), &what);
                    assert!(by_node(stored(&got), &what) == want, "{what}");

                    let elided: Vec<SyntaxNodePtr> = bodies
                        .iter()
                        .copied()
                        .filter(|it| range.contains_range(it.text_range()) && it.text_range() != range)
                        .collect();
                    if elided.is_empty() {
                        continue;
                    }
                    let nodes: Vec<view::Node<'_>> =
                        elided.iter().map(|&it| view::to_node(it, root)).collect();
                    let tree = without_all(anchor, &nodes);
                    let got = map.relative_without_view(anchor, view::Node::root(&tree), &elided);
                    let shift = |offset: TextSize| {
                        elided
                            .iter()
                            .filter(|it| it.text_range().end() <= offset)
                            .fold(offset - range.start(), |at, it| at - it.text_range().len())
                    };
                    let in_elided = |it: SyntaxNodePtr| {
                        elided.iter().any(|e| e.text_range().contains_range(it.text_range()))
                    };
                    let want = all
                        .iter()
                        .filter(|&&(it, _)| under(it) && !in_elided(it))
                        .map(|&(it, id)| {
                            let (start, end) = (it.text_range().start(), it.text_range().end());
                            let moved = TextRange::new(shift(start), shift(end));
                            (SyntaxNodePtr::from_parts(it.kind(), moved), id)
                        });
                    let what = format!("{path}: {ptr:?} without its bodies");
                    assert!(got.bodies.len() == 0, "{what}: a body is kept");
                    let want = by_node(want.collect(), &what);
                    assert!(by_node(stored(&got), &what) == want, "{what}");
                }
            }
        }
    }

    /// A warm `from_view` allocates its result and nothing else, over span's own sources and
    /// the parser's test data.
    #[test]
    fn a_warm_view_build_allocates_only_its_map() {
        for (path, text) in view_corpus() {
            for parse in parses(&text) {
                let map = AstIdMap::from_view(parse.root());
                let columns = if map.bodies.len() == 0 { 3 } else { 8 };
                let what = format!("{}: a warm map build from a view", path.display());
                blaze_alloc_count::assert_at_most(columns, &what, || {
                    std::hint::black_box(AstIdMap::from_view(parse.root()));
                });
            }
        }
    }
}
