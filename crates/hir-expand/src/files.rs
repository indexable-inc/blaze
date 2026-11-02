//! Things to wrap other things in file ids.
use std::borrow::Borrow;

use base_db::Db;
use either::Either;
use span::{AstIdNode, ErasedFileAstId, FileAstId, FileId, SyntaxContext};
use syntax::{AstNode, AstPtr, SyntaxNode, SyntaxNodePtr, TextRange, TextSize, view};

use crate::{
    EditionedFileId, HirFileId, MacroCallId, MacroKind, map_node_range_up,
    map_node_range_up_rooted, span_for_offset,
};

/// `InFile<T>` stores a value of `T` inside a particular file/syntax tree.
///
/// Typical usages are:
///
/// * `InFile<SyntaxNode>` -- syntax node in a file
/// * `InFile<ast::FnDef>` -- ast node in a file
/// * `InFile<TextSize>` -- offset in a file
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct InFileWrapper<FileKind, T> {
    pub file_id: FileKind,
    pub value: T,
}
pub type InFile<T> = InFileWrapper<HirFileId, T>;
pub type InMacroFile<T> = InFileWrapper<MacroCallId, T>;
pub type InRealFile<T> = InFileWrapper<EditionedFileId, T>;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct FilePositionWrapper<FileKind> {
    pub file_id: FileKind,
    pub offset: TextSize,
}
pub type HirFilePosition = FilePositionWrapper<HirFileId>;
pub type MacroFilePosition = FilePositionWrapper<MacroCallId>;
pub type FilePosition = FilePositionWrapper<EditionedFileId>;

impl FilePosition {
    #[inline]
    pub fn into_file_id(self, db: &Db) -> FilePositionWrapper<FileId> {
        FilePositionWrapper { file_id: self.file_id.file_id(db), offset: self.offset }
    }
}

impl From<FileRange> for HirFileRange {
    fn from(value: FileRange) -> Self {
        HirFileRange { file_id: value.file_id.into(), range: value.range }
    }
}

impl From<FilePosition> for HirFilePosition {
    fn from(value: FilePosition) -> Self {
        HirFilePosition { file_id: value.file_id.into(), offset: value.offset }
    }
}

impl HirFileRange {
    pub fn file_range(self) -> Option<FileRange> {
        Some(FileRange { file_id: self.file_id.file_id()?, range: self.range })
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct FileRangeWrapper<FileKind> {
    pub file_id: FileKind,
    pub range: TextRange,
}
pub type HirFileRange = FileRangeWrapper<HirFileId>;
pub type MacroFileRange = FileRangeWrapper<MacroCallId>;
pub type FileRange = FileRangeWrapper<EditionedFileId>;

impl FileRange {
    #[inline]
    pub fn into_file_id(self, db: &Db) -> FileRangeWrapper<FileId> {
        FileRangeWrapper { file_id: self.file_id.file_id(db), range: self.range }
    }

    #[inline]
    pub fn file_text(self, db: &Db) -> &syntax::Text {
        db.file_text(self.file_id.file_id(db)).text(db)
    }

    #[inline]
    pub fn text(self, db: &Db) -> &str {
        &self.file_text(db)[self.range]
    }
}

/// `AstId` points to an AST node in any file.
///
/// It is stable across reparses, and can be used as a query key or value.
pub type AstId<N> = crate::InFile<FileAstId<N>>;

impl<N: AstNode> AstId<N> {
    pub fn to_node(&self, db: &Db) -> N {
        self.to_ptr(db).to_node(&self.file_id.parse_or_expand(db))
    }
    pub fn to_range(&self, db: &Db) -> TextRange {
        self.to_ptr(db).text_range()
    }
    pub fn to_ptr(&self, db: &Db) -> AstPtr<N> {
        self.file_id.ast_id_map(db).get(self.value)
    }
    pub fn erase(&self) -> ErasedAstId {
        crate::InFile::new(self.file_id, self.value.erase())
    }
    #[inline]
    pub fn upcast<M: AstIdNode>(self) -> AstId<M>
    where
        N: Into<M>,
    {
        self.map(|it| it.upcast())
    }
}

impl<N: AstNode + syntax::view::Typed> AstId<N> {
    /// The node as a view of the tree the database keeps: [`Self::to_node`] with no handle.
    pub fn to_view<'db>(&self, db: &'db Db) -> N::Of<'db> {
        self.to_ptr(db).to_view(self.file_id.root(db))
    }

    /// [`Self::to_in_file_node`] as a view.
    pub fn to_in_file_view<'db>(&self, db: &'db Db) -> crate::InFile<N::Of<'db>> {
        crate::InFile::new(self.file_id, self.to_view(db))
    }
}

pub type ErasedAstId = crate::InFile<ErasedFileAstId>;

impl ErasedAstId {
    pub fn to_range(&self, db: &Db) -> TextRange {
        self.to_ptr(db).text_range()
    }
    pub fn to_ptr(&self, db: &Db) -> SyntaxNodePtr {
        self.file_id.ast_id_map(db).get_erased(self.value)
    }
}

impl<FileKind, N: AstNode> InFileWrapper<FileKind, AstPtr<N>> {
    #[inline]
    pub fn upcast<M: AstNode>(self) -> InFileWrapper<FileKind, AstPtr<M>>
    where
        N: Into<M>,
    {
        self.map(|it| it.upcast())
    }
}

impl<FileKind, T> InFileWrapper<FileKind, T> {
    pub fn new(file_id: FileKind, value: T) -> Self {
        Self { file_id, value }
    }

    pub fn map<F: FnOnce(T) -> U, U>(self, f: F) -> InFileWrapper<FileKind, U> {
        InFileWrapper::new(self.file_id, f(self.value))
    }
}

impl<FileKind: Copy, T> InFileWrapper<FileKind, T> {
    pub fn with_value<U>(&self, value: U) -> InFileWrapper<FileKind, U> {
        InFileWrapper::new(self.file_id, value)
    }

    pub fn as_ref(&self) -> InFileWrapper<FileKind, &T> {
        self.with_value(&self.value)
    }

    pub fn borrow<U>(&self) -> InFileWrapper<FileKind, &U>
    where
        T: Borrow<U>,
    {
        self.with_value(self.value.borrow())
    }
}

impl<FileKind: Copy, T: Clone> InFileWrapper<FileKind, &T> {
    pub fn cloned(&self) -> InFileWrapper<FileKind, T> {
        self.with_value(self.value.clone()) // clone: what `cloned` is asked for
    }
}

impl<T> From<InMacroFile<T>> for InFile<T> {
    fn from(InMacroFile { file_id, value }: InMacroFile<T>) -> Self {
        InFile { file_id: file_id.into(), value }
    }
}

impl<T> From<InRealFile<T>> for InFile<T> {
    fn from(InRealFile { file_id, value }: InRealFile<T>) -> Self {
        InFile { file_id: file_id.into(), value }
    }
}

// region:transpose impls

impl<FileKind, T> InFileWrapper<FileKind, Option<T>> {
    pub fn transpose(self) -> Option<InFileWrapper<FileKind, T>> {
        Some(InFileWrapper::new(self.file_id, self.value?))
    }
}

impl<FileKind, L, R> InFileWrapper<FileKind, Either<L, R>> {
    pub fn transpose(self) -> Either<InFileWrapper<FileKind, L>, InFileWrapper<FileKind, R>> {
        match self.value {
            Either::Left(l) => Either::Left(InFileWrapper::new(self.file_id, l)),
            Either::Right(r) => Either::Right(InFileWrapper::new(self.file_id, r)),
        }
    }
}

// endregion:transpose impls

trait FileIdToSyntax: Copy {
    fn file_syntax(self, db: &Db) -> SyntaxNode;
    fn root(self, db: &Db) -> view::Node<'_>;
}

impl FileIdToSyntax for EditionedFileId {
    fn file_syntax(self, db: &Db) -> SyntaxNode {
        self.parse(db).syntax_node()
    }
    fn root(self, db: &Db) -> view::Node<'_> {
        self.parse(db).root()
    }
}
impl FileIdToSyntax for MacroCallId {
    fn file_syntax(self, db: &Db) -> SyntaxNode {
        self.parse_macro_expansion(db).value.0.syntax_node()
    }
    fn root(self, db: &Db) -> view::Node<'_> {
        self.parse_macro_expansion(db).value.0.root()
    }
}
impl FileIdToSyntax for HirFileId {
    fn file_syntax(self, db: &Db) -> SyntaxNode {
        self.parse_or_expand(db)
    }
    fn root(self, db: &Db) -> view::Node<'_> {
        HirFileId::root(self, db)
    }
}

#[allow(private_bounds)]
impl<FileId: FileIdToSyntax, T> InFileWrapper<FileId, T> {
    pub fn file_syntax(&self, db: &Db) -> SyntaxNode {
        FileIdToSyntax::file_syntax(self.file_id, db)
    }

    /// The file's root as a view of the parse the database keeps: [`Self::file_syntax`] with
    /// no handle.
    pub fn root<'db>(&self, db: &'db Db) -> view::Node<'db> {
        FileIdToSyntax::root(self.file_id, db)
    }
}

#[allow(private_bounds)]
impl<FileId: FileIdToSyntax, N: AstNode> InFileWrapper<FileId, AstPtr<N>> {
    pub fn to_node(&self, db: &Db) -> N {
        self.value.to_node(&self.file_syntax(db))
    }
}

#[allow(private_bounds)]
impl<FileId: FileIdToSyntax, N: AstNode + view::Typed> InFileWrapper<FileId, AstPtr<N>> {
    /// The node as a view: [`Self::to_node`] with no handle.
    pub fn to_view<'db>(&self, db: &'db Db) -> N::Of<'db> {
        self.value.to_view(self.root(db))
    }
}

impl<FileId: Copy, N: AstNode> InFileWrapper<FileId, N> {
    pub fn syntax(&self) -> InFileWrapper<FileId, &SyntaxNode> {
        self.with_value(self.value.syntax())
    }
}

impl<FileId: Copy, N: AstNode> InFileWrapper<FileId, &N> {
    // unfortunately `syntax` collides with the impl above, because `&_` is fundamental
    pub fn syntax_ref(&self) -> InFileWrapper<FileId, &SyntaxNode> {
        self.with_value(self.value.syntax())
    }
}

/// The views' [`InFileWrapper::syntax`] and the node's file range: named `_view`, as
/// a type may be both a handle and a view in coherence's eyes.
impl<'t, FileId: Copy, N: view::AstNode<'t>> InFileWrapper<FileId, N> {
    pub fn syntax_view(&self) -> InFileWrapper<FileId, view::Node<'t>> {
        self.with_value(view::AstNode::syntax(&self.value))
    }

    pub fn node_file_range_view(&self) -> FileRangeWrapper<FileId> {
        FileRangeWrapper { file_id: self.file_id, range: view::AstNode::syntax(&self.value).text_range() }
    }
}

// region:specific impls
impl<SN: Borrow<SyntaxNode>> InFile<SN> {
    pub fn kind(&self) -> parser::SyntaxKind {
        self.value.borrow().kind()
    }

    pub fn text_range(&self) -> TextRange {
        self.value.borrow().text_range()
    }
}

/// Walks and mappings of a node view: named `_view`, as `impl<SN: Borrow<SyntaxNode>> InFile<SN>`
/// may overlap `InFile<view::Node>` in coherence's eyes.
impl<'db> InFile<view::Node<'db>> {
    pub fn parent_ancestors_with_macros_view(
        self,
        db: &'db Db,
    ) -> impl Iterator<Item = InFile<view::Node<'db>>> + 'db {
        std::iter::successors(parent_with_macros(db, self), move |&node| {
            parent_with_macros(db, node)
        })
    }

    pub fn ancestors_with_macros_view(
        self,
        db: &'db Db,
    ) -> impl Iterator<Item = InFile<view::Node<'db>>> + 'db {
        std::iter::successors(Some(self), move |&node| parent_with_macros(db, node))
    }

    /// Falls back to the macro call range if the node cannot be mapped up fully.
    pub fn original_file_range_rooted_view(self, db: &Db) -> FileRange {
        self.map(|it| it.text_range()).original_node_file_range_rooted(db)
    }

    /// Falls back to the macro call range if the node cannot be mapped up fully.
    pub fn original_file_range_with_macro_call_input_view(self, db: &Db) -> FileRange {
        self.map(|it| it.text_range()).original_node_file_range_with_macro_call_input(db)
    }

    /// Attempts to map the syntax node back up its macro calls.
    pub fn original_file_range_opt_view(self, db: &Db) -> Option<(FileRange, SyntaxContext)> {
        self.map(|it| it.text_range()).original_node_file_range_opt(db)
    }

    pub fn original_syntax_node_rooted_view(
        self,
        db: &'db Db,
    ) -> Option<InRealFile<view::Node<'db>>> {
        // This kind of upmapping can only be achieved in attribute expanded files,
        // as we don't have node inputs otherwise and therefore can't find an `N` node in the input
        let file_id = match self.file_id {
            HirFileId::FileId(file_id) => {
                return Some(InRealFile { file_id, value: self.value });
            }
            HirFileId::MacroFile(m)
                if matches!(m.kind(db), MacroKind::Attr | MacroKind::AttrBuiltIn) =>
            {
                m
            }
            _ => return None,
        };

        let FileRange { file_id: editioned_file_id, range } = map_node_range_up_rooted(
            db,
            file_id.expansion_span_map(db),
            self.value.text_range(),
        )?;

        let kind = self.value.kind();
        let value = editioned_file_id
            .parse(db)
            .root()
            .covering_element(range)
            .ancestors()
            .take_while(|it| it.text_range() == range)
            .find(|it| it.kind() == kind)?;
        Some(InRealFile::new(editioned_file_id, value))
    }
}

/// The parent of `node`: past the root of a macro file, the parent of the item of its call, as
/// `ancestors_with_macros` steps.
fn parent_with_macros<'db>(
    db: &'db Db,
    node: InFile<view::Node<'db>>,
) -> Option<InFile<view::Node<'db>>> {
    match node.value.parent() {
        Some(parent) => Some(node.with_value(parent)),
        None => {
            // A call that is its own file's root (`panic!` expands to the bare call
            // `panic_2021!(..)`) is in the expansion of the call above it.
            let item = node.file_id.macro_file()?.loc(db).to_node_item_view(db);
            parent_with_macros(db, item.map(|it| view::AstNode::syntax(&it)))
        }
    }
}

/// A token view mapped up its macro calls.
impl InFile<view::Token<'_>> {
    /// Falls back to the macro call range if the node cannot be mapped up fully.
    pub fn original_file_range(self, db: &Db) -> FileRange {
        token_original_file_range(db, self.map(|it| it.text_range()))
    }

    /// Attempts to map the syntax node back up its macro calls.
    pub fn original_file_range_opt(self, db: &Db) -> Option<FileRange> {
        token_original_file_range_opt(db, self.map(|it| it.text_range()))
    }
}

/// Where a token at `token` comes from: its file range, else the whole macro call's.
fn token_original_file_range(db: &Db, token: InFile<TextRange>) -> FileRange {
    match token.file_id {
        HirFileId::FileId(file_id) => FileRange { file_id, range: token.value },
        HirFileId::MacroFile(mac_file) => {
            let (range, ctxt) =
                span_for_offset(db, mac_file.expansion_span_map(db), token.value.start());

            // FIXME: Figure out an API that makes proper use of ctx, this only exists to
            // keep pre-token map rewrite behaviour.
            if ctxt.is_root() {
                return range;
            }

            // Fall back to whole macro call.
            let loc = mac_file.loc(db);
            loc.kind.original_call_range(db, loc.krate)
        }
    }
}

/// Where a token at `token` comes from, if it maps up fully.
fn token_original_file_range_opt(db: &Db, token: InFile<TextRange>) -> Option<FileRange> {
    match token.file_id {
        HirFileId::FileId(file_id) => Some(FileRange { file_id, range: token.value }),
        HirFileId::MacroFile(mac_file) => {
            let (range, ctxt) =
                span_for_offset(db, mac_file.expansion_span_map(db), token.value.start());

            // FIXME: Figure out an API that makes proper use of ctx, this only exists to
            // keep pre-token map rewrite behaviour.
            if ctxt.is_root() { Some(range) } else { None }
        }
    }
}

impl InMacroFile<TextSize> {
    pub fn original_file_range(self, db: &Db) -> (FileRange, SyntaxContext) {
        span_for_offset(db, self.file_id.expansion_span_map(db), self.value)
    }
}

impl InFile<TextRange> {
    pub fn original_node_file_range(self, db: &Db) -> (FileRange, SyntaxContext) {
        match self.file_id {
            HirFileId::FileId(file_id) => {
                (FileRange { file_id, range: self.value }, SyntaxContext::root(file_id.edition(db)))
            }
            HirFileId::MacroFile(mac_file) => {
                match map_node_range_up(db, mac_file.expansion_span_map(db), self.value) {
                    Some(it) => it,
                    None => {
                        let loc = mac_file.loc(db);
                        (
                            loc.kind.original_call_range(db, loc.krate),
                            SyntaxContext::root(loc.def.edition),
                        )
                    }
                }
            }
        }
    }

    pub fn original_node_file_range_rooted(self, db: &Db) -> FileRange {
        match self.file_id {
            HirFileId::FileId(file_id) => FileRange { file_id, range: self.value },
            HirFileId::MacroFile(mac_file) => {
                match map_node_range_up_rooted(db, mac_file.expansion_span_map(db), self.value) {
                    Some(it) => it,
                    _ => {
                        let loc = mac_file.loc(db);
                        loc.kind.original_call_range(db, loc.krate)
                    }
                }
            }
        }
    }

    pub fn original_node_file_range_with_macro_call_input(
        self,
        db: &Db,
    ) -> FileRange {
        match self.file_id {
            HirFileId::FileId(file_id) => FileRange { file_id, range: self.value },
            HirFileId::MacroFile(mac_file) => {
                match map_node_range_up_rooted(db, mac_file.expansion_span_map(db), self.value) {
                    Some(it) => it,
                    _ => {
                        let loc = mac_file.loc(db);
                        loc.kind.original_call_range_with_input(db)
                    }
                }
            }
        }
    }

    pub fn original_node_file_range_opt(
        self,
        db: &Db,
    ) -> Option<(FileRange, SyntaxContext)> {
        match self.file_id {
            HirFileId::FileId(file_id) => Some((
                FileRange { file_id, range: self.value },
                SyntaxContext::root(file_id.edition(db)),
            )),
            HirFileId::MacroFile(mac_file) => {
                map_node_range_up(db, mac_file.expansion_span_map(db), self.value)
            }
        }
    }

    pub fn original_node_file_range_rooted_opt(self, db: &Db) -> Option<FileRange> {
        match self.file_id {
            HirFileId::FileId(file_id) => Some(FileRange { file_id, range: self.value }),
            HirFileId::MacroFile(mac_file) => {
                map_node_range_up_rooted(db, mac_file.expansion_span_map(db), self.value)
            }
        }
    }
}

/// Maps a node view up to the real file, in attribute expansions only.
impl<'db, N: view::AstNode<'db>> InFile<N> {
    pub fn original_ast_node_rooted_view(self, db: &'db Db) -> Option<InRealFile<N>> {
        // This kind of upmapping can only be achieved in attribute expanded files,
        // as we don't have node inputs otherwise and therefore can't find an `N` node in the input
        let file_id = match self.file_id {
            HirFileId::FileId(file_id) => {
                return Some(InRealFile { file_id, value: self.value });
            }
            HirFileId::MacroFile(m) => m,
        };
        if !matches!(file_id.kind(db), MacroKind::Attr | MacroKind::AttrBuiltIn) {
            return None;
        }

        let FileRange { file_id: editioned_file_id, range } = map_node_range_up_rooted(
            db,
            file_id.expansion_span_map(db),
            view::AstNode::syntax(&self.value).text_range(),
        )?;

        // FIXME: This heuristic is brittle and with the right macro may select completely unrelated nodes?
        let anc = editioned_file_id.parse(db).root().covering_element(range);
        let value = anc.ancestors().find_map(N::cast)?;
        Some(InRealFile::new(editioned_file_id, value))
    }
}

impl<T> InFile<T> {
    pub fn into_real_file(self) -> Result<InRealFile<T>, InFile<T>> {
        match self.file_id {
            HirFileId::FileId(file_id) => Ok(InRealFile { file_id, value: self.value }),
            HirFileId::MacroFile(_) => Err(self),
        }
    }
}
