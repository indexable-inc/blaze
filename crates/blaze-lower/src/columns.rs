//! The columns a lowering writes, the view every reader takes (of the columns in memory or of
//! a record), and the record: one blaze-format buffer whose root holds a slice per column and
//! the head, so a record is read in place.

use core::mem::offset_of;

use blaze_format_core::{FormatError, Own, Record, Slice, Writer, open_as};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::Range;

use crate::form::*;

/// The number of `u32` words in a [`Head`].
pub const HEAD_WORDS: usize = 32;

/// What a lowering is about, beyond its columns: the root of a body, or the parts of a
/// signature. Words by [`Head`]'s constants; lists are (start, len) pairs.
pub type Head = [u32; HEAD_WORDS];

/// The head's words.
pub mod head {
    /// [`crate::Mode`] as 0 (signature), 1 (body), 2 (anonymous constant).
    pub const MODE: usize = 0;
    pub const EDITION: usize = 1;
    /// The item's parser kind, as its number; `u32::MAX` for an anonymous constant.
    pub const SYNTAX: usize = 2;
    /// [`crate::machine::sig`]'s flags.
    pub const FLAGS: usize = 3;
    /// Rows of `GenericParam`.
    pub const GENERICS: usize = 4;
    /// Rows of `WherePred`.
    pub const WHERE: usize = 6;
    /// Rows of `Attr`.
    pub const ATTRS: usize = 8;
    /// Rows of `Param` (a signature); in a body, the parameters' patterns in `ids`.
    pub const PARAMS: usize = 10;
    /// The return type, a type alias's or const's or static's type.
    pub const TY: usize = 12;
    /// Rows of `Field` (struct, union) or `Variant` (enum).
    pub const FIELDS: usize = 13;
    /// `Shape` of a struct.
    pub const SHAPE: usize = 15;
    /// Rows of `Bound`: supertraits, an associated type's or a trait alias's bounds.
    pub const BOUNDS: usize = 16;
    /// An impl's trait, a path type.
    pub const TRAIT: usize = 18;
    /// An impl's self type.
    pub const SELF_TY: usize = 19;
    /// The item's name.
    pub const NAME: usize = 20;
    /// The ABI string's range (an `extern` with none: start `u32::MAX - 1`).
    pub const ABI: usize = 21;
    /// `Vis` code and path.
    pub const VIS: usize = 23;
    /// Rows of `LocalItem` (assoc entries of a trait or impl).
    pub const ITEMS: usize = 25;
    /// A body's root expression.
    pub const ROOT: usize = 27;
    /// The row of the item's `#[repr(..)]`, if it has one.
    pub const REPR: usize = 28;
    /// 1 when the item's own `cfg` is false: nothing else is lowered.
    pub const CFG_OUT: usize = 29;
}

blaze_format_core::record! {
    /// A lowering's record: a slice per column, the lang items read and the head.
    #[derive(Clone, Copy, Debug)]
    pub struct LoweredRoot: 1 {
        pub langs: u64,
        pub exprs: Slice<Node>,
        pub expr_ranges: Slice<Range>,
        pub pats: Slice<Node>,
        pub pat_ranges: Slice<Range>,
        pub stmts: Slice<Node>,
        pub types: Slice<Node>,
        pub type_ranges: Slice<Range>,
        pub paths: Slice<Node>,
        pub path_ranges: Slice<Range>,
        pub rows: Slice<Node>,
        pub row_ranges: Slice<Range>,
        pub ids: Slice<u32>,
        pub names: Slice<u64>,
        pub name_ranges: Slice<Range>,
        pub scopes: Slice<Node>,
        pub consts: Slice<Range>,
        pub unhandled: Slice<Node>,
        pub diags: Slice<Node>,
        pub cfg: Slice<Node>,
        pub head: [u32; 32],
    }
}

/// The columns while they are written: plain vectors in the lowering's allocator.
pub struct Columns<'a> {
    pub(crate) exprs: AVec<'a, Node>,
    pub(crate) expr_ranges: AVec<'a, Range>,
    pub(crate) pats: AVec<'a, Node>,
    pub(crate) pat_ranges: AVec<'a, Range>,
    pub(crate) stmts: AVec<'a, Node>,
    pub(crate) types: AVec<'a, Node>,
    pub(crate) type_ranges: AVec<'a, Range>,
    pub(crate) paths: AVec<'a, Node>,
    pub(crate) path_ranges: AVec<'a, Range>,
    pub(crate) rows: AVec<'a, Node>,
    pub(crate) row_ranges: AVec<'a, Range>,
    pub(crate) ids: AVec<'a, u32>,
    pub(crate) names: AVec<'a, u64>,
    pub(crate) name_ranges: AVec<'a, Range>,
    pub(crate) scopes: AVec<'a, Node>,
    pub(crate) consts: AVec<'a, Range>,
    pub(crate) unhandled: AVec<'a, Node>,
    pub(crate) diags: AVec<'a, Node>,
    pub(crate) cfg: AVec<'a, Node>,
    pub(crate) langs: u64,
    pub(crate) head: Head,
}

impl<'a> Columns<'a> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Columns<'a> {
        Columns {
            exprs: Vec::new_in(alloc),
            expr_ranges: Vec::new_in(alloc),
            pats: Vec::new_in(alloc),
            pat_ranges: Vec::new_in(alloc),
            stmts: Vec::new_in(alloc),
            types: Vec::new_in(alloc),
            type_ranges: Vec::new_in(alloc),
            paths: Vec::new_in(alloc),
            path_ranges: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
            row_ranges: Vec::new_in(alloc),
            ids: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            name_ranges: Vec::new_in(alloc),
            scopes: Vec::new_in(alloc),
            consts: Vec::new_in(alloc),
            unhandled: Vec::new_in(alloc),
            diags: Vec::new_in(alloc),
            cfg: Vec::new_in(alloc),
            langs: 0,
            head: [NONE; HEAD_WORDS],
        }
    }

    pub fn view(&self) -> View<'_> {
        View {
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            types: &self.types,
            type_ranges: &self.type_ranges,
            paths: &self.paths,
            path_ranges: &self.path_ranges,
            rows: &self.rows,
            row_ranges: &self.row_ranges,
            ids: &self.ids,
            names: &self.names,
            name_ranges: &self.name_ranges,
            scopes: &self.scopes,
            consts: &self.consts,
            unhandled: &self.unhandled,
            diags: &self.diags,
            cfg: &self.cfg,
            langs: self.langs,
            head: self.head,
        }
    }

    pub(crate) fn to_record<'b>(&self, alloc: Alloc<'b>) -> blaze_own::Vec<u8, Alloc<'b>> {
        let mut out = Writer::new_in(<LoweredRoot as Record>::SCHEMA, Own(alloc));
        let exprs = Column::push(&mut out, &self.exprs);
        let expr_ranges = Column::push(&mut out, &self.expr_ranges);
        let pats = Column::push(&mut out, &self.pats);
        let pat_ranges = Column::push(&mut out, &self.pat_ranges);
        let stmts = Column::push(&mut out, &self.stmts);
        let types = Column::push(&mut out, &self.types);
        let type_ranges = Column::push(&mut out, &self.type_ranges);
        let paths = Column::push(&mut out, &self.paths);
        let path_ranges = Column::push(&mut out, &self.path_ranges);
        let rows = Column::push(&mut out, &self.rows);
        let row_ranges = Column::push(&mut out, &self.row_ranges);
        let ids = Column::push(&mut out, &self.ids);
        let names = Column::push(&mut out, &self.names);
        let name_ranges = Column::push(&mut out, &self.name_ranges);
        let scopes = Column::push(&mut out, &self.scopes);
        let consts = Column::push(&mut out, &self.consts);
        let unhandled = Column::push(&mut out, &self.unhandled);
        let diags = Column::push(&mut out, &self.diags);
        let cfg = Column::push(&mut out, &self.cfg);
        let at = out.next::<LoweredRoot>();
        let root = LoweredRoot {
            langs: self.langs,
            exprs: exprs.slice(at, offset_of!(LoweredRoot, exprs)),
            expr_ranges: expr_ranges.slice(at, offset_of!(LoweredRoot, expr_ranges)),
            pats: pats.slice(at, offset_of!(LoweredRoot, pats)),
            pat_ranges: pat_ranges.slice(at, offset_of!(LoweredRoot, pat_ranges)),
            stmts: stmts.slice(at, offset_of!(LoweredRoot, stmts)),
            types: types.slice(at, offset_of!(LoweredRoot, types)),
            type_ranges: type_ranges.slice(at, offset_of!(LoweredRoot, type_ranges)),
            paths: paths.slice(at, offset_of!(LoweredRoot, paths)),
            path_ranges: path_ranges.slice(at, offset_of!(LoweredRoot, path_ranges)),
            rows: rows.slice(at, offset_of!(LoweredRoot, rows)),
            row_ranges: row_ranges.slice(at, offset_of!(LoweredRoot, row_ranges)),
            ids: ids.slice(at, offset_of!(LoweredRoot, ids)),
            names: names.slice(at, offset_of!(LoweredRoot, names)),
            name_ranges: name_ranges.slice(at, offset_of!(LoweredRoot, name_ranges)),
            scopes: scopes.slice(at, offset_of!(LoweredRoot, scopes)),
            consts: consts.slice(at, offset_of!(LoweredRoot, consts)),
            unhandled: unhandled.slice(at, offset_of!(LoweredRoot, unhandled)),
            diags: diags.slice(at, offset_of!(LoweredRoot, diags)),
            cfg: cfg.slice(at, offset_of!(LoweredRoot, cfg)),
            head: self.head,
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        out.finish(at)
    }
}

/// Where a column was written: nowhere when it is empty.
#[derive(Clone, Copy)]
struct Column {
    at: u64,
    len: usize,
}

impl Column {
    fn push<T: Record>(out: &mut Writer<Own<Alloc<'_>>>, values: &[T]) -> Column {
        match values.is_empty() {
            true => Column { at: 0, len: 0 },
            false => Column { at: out.push_slice(values), len: values.len() },
        }
    }

    fn slice<T>(self, at: u64, offset: usize) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(at + offset as u64, self.at, len),
        }
    }
}

/// Opens a lowering's record: its root verified once, each column a slice of the buffer.
pub fn open(buffer: &[u8]) -> Result<View<'_>, FormatError> {
    let root = open_as::<LoweredRoot>(buffer, <LoweredRoot as Record>::SCHEMA)?;
    Ok(View {
        exprs: root.exprs.follow(buffer)?,
        expr_ranges: root.expr_ranges.follow(buffer)?,
        pats: root.pats.follow(buffer)?,
        pat_ranges: root.pat_ranges.follow(buffer)?,
        stmts: root.stmts.follow(buffer)?,
        types: root.types.follow(buffer)?,
        type_ranges: root.type_ranges.follow(buffer)?,
        paths: root.paths.follow(buffer)?,
        path_ranges: root.path_ranges.follow(buffer)?,
        rows: root.rows.follow(buffer)?,
        row_ranges: root.row_ranges.follow(buffer)?,
        ids: root.ids.follow(buffer)?,
        names: root.names.follow(buffer)?,
        name_ranges: root.name_ranges.follow(buffer)?,
        scopes: root.scopes.follow(buffer)?,
        consts: root.consts.follow(buffer)?,
        unhandled: root.unhandled.follow(buffer)?,
        diags: root.diags.follow(buffer)?,
        cfg: root.cfg.follow(buffer)?,
        langs: root.langs,
        head: root.head,
    })
}

/// A lowering read: of the columns in memory, or of a record in place. Rows decode when
/// asked; a row that is no form of its column is a [`BadRow`].
#[derive(Clone, Copy)]
pub struct View<'b> {
    pub exprs: &'b [Node],
    pub expr_ranges: &'b [Range],
    pub pats: &'b [Node],
    pub pat_ranges: &'b [Range],
    pub stmts: &'b [Node],
    pub types: &'b [Node],
    pub type_ranges: &'b [Range],
    pub paths: &'b [Node],
    pub path_ranges: &'b [Range],
    pub rows: &'b [Node],
    pub row_ranges: &'b [Range],
    pub ids: &'b [u32],
    pub names: &'b [u64],
    pub name_ranges: &'b [Range],
    pub scopes: &'b [Node],
    pub consts: &'b [Range],
    pub unhandled: &'b [Node],
    pub diags: &'b [Node],
    pub cfg: &'b [Node],
    pub langs: u64,
    pub head: Head,
}

fn at<T: Copy>(column: &'static str, all: &[T], i: u32) -> Result<T, BadRow> {
    all.get(i as usize).copied().ok_or(BadRow { column, tag: u16::MAX })
}

fn slice<'b, T>(column: &'static str, all: &'b [T], l: List) -> Result<&'b [T], BadRow> {
    let start = l.start as usize;
    all.get(start..start + l.len as usize).ok_or(BadRow { column, tag: u16::MAX })
}

impl<'b> View<'b> {
    pub fn expr(&self, e: ExprId) -> Result<Expr, BadRow> {
        Expr::decode(at("exprs", self.exprs, e.0)?)
    }

    pub fn expr_range(&self, e: ExprId) -> Range {
        self.expr_ranges.get(e.0 as usize).copied().unwrap_or_default()
    }

    pub fn pat(&self, p: PatId) -> Result<Pat, BadRow> {
        Pat::decode(at("pats", self.pats, p.0)?)
    }

    pub fn stmt(&self, s: StmtId) -> Result<Stmt, BadRow> {
        Stmt::decode(at("stmts", self.stmts, s.0)?)
    }

    pub fn ty(&self, t: TypeId) -> Result<TypeRef, BadRow> {
        TypeRef::decode(at("types", self.types, t.0)?)
    }

    pub fn path(&self, p: PathIdx) -> Result<PathRef, BadRow> {
        PathRef::decode(at("paths", self.paths, p.0)?)
    }

    pub fn row(&self, r: RowIdx) -> Result<Node, BadRow> {
        at("rows", self.rows, r.0)
    }

    pub fn rows(&self, l: List) -> Result<&'b [Node], BadRow> {
        slice("rows", self.rows, l)
    }

    pub fn ids(&self, l: List) -> Result<&'b [u32], BadRow> {
        slice("ids", self.ids, l)
    }

    pub fn segments(&self, p: PathRef) -> Result<&'b [Node], BadRow> {
        self.rows(p.segments)
    }

    /// A name's hash, blaze-typeck's `Name`.
    pub fn name(&self, n: NameIdx) -> Result<u64, BadRow> {
        at("names", self.names, n.0)
    }

    /// A name as written in `text`, the item's text (`r#` kept).
    pub fn name_text<'t>(&self, text: &'t str, n: NameIdx) -> &'t str {
        self.name_ranges.get(n.0 as usize).and_then(|r| text.get(r.start as usize..r.end as usize)).unwrap_or("")
    }

    pub fn const_range(&self, c: ConstIdx) -> Result<Range, BadRow> {
        at("consts", self.consts, c.0)
    }

    pub fn scope(&self, s: ScopeId) -> Result<Scope, BadRow> {
        Scope::decode(at("scopes", self.scopes, s.0)?)
    }

    pub fn unhandled(&self) -> impl Iterator<Item = Unhandled> + 'b {
        self.unhandled.iter().map(|n| Unhandled::decode(*n))
    }

    pub fn diags(&self) -> impl Iterator<Item = Result<Diag, BadRow>> + 'b {
        self.diags.iter().map(|n| Diag::decode(*n))
    }

    pub fn cfg_reads(&self) -> impl Iterator<Item = CfgRead> + 'b {
        self.cfg.iter().map(|n| CfgRead::decode(*n))
    }

    /// The lang items the body's desugarings name.
    pub fn langs(&self) -> impl Iterator<Item = LangItem> + 'b {
        let set = self.langs;
        LangItem::ALL.into_iter().filter(move |l| set & l.bit() != 0)
    }

    pub fn list(&self, word: usize) -> List {
        List { start: self.head[word], len: self.head[word + 1] }
    }

    pub fn head_opt(&self, word: usize) -> Option<u32> {
        unopt(self.head[word])
    }

    /// A body's root expression and its parameters' patterns.
    pub fn body(&self) -> Option<(ExprId, &'b [u32])> {
        let root = self.head_opt(head::ROOT)?;
        let params = self.ids(self.list(head::PARAMS)).unwrap_or(&[]);
        Some((ExprId(root), params))
    }

    /// Whether the item's own `cfg` turned it off.
    pub fn cfg_out(&self) -> bool {
        self.head[head::CFG_OUT] == 1
    }
}
