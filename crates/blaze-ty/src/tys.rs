//! The arena: one per type-checking task, in columns.
//!
//! Each class (types and predicates, regions, consts) is a set of rows `(kind: u8, a: u32,
//! b: u32)` with a `flags: u16` column computed at intern time, and types have a `markers: u32`
//! column for the solver's marker-trait cache. Lists of arguments are `(start, len)` into one
//! pool of `Arg` words. Items are a column of `ItemKey`s (`Def` is a row of it); const values a
//! column of `u128`s. Every class is hash-consed: a row is interned once, so equal types are
//! equal ids, and `==` on a `Ty` is type equality (up to inference variables).
//!
//! Memory per distinct type: 1 + 4 + 4 + 2 + 4 = 15 bytes of columns and about 5 bytes of
//! index (4-byte slot and 1-byte tag at 7/8 load at most), against rustc's 48-byte
//! `WithCachedTypeInfo<TyKind>` behind a pointer in a sharded set. No hash column (a deviation
//! from the brief's layout): a grow recomputes each row's hash in one multiply, which costs
//! less than 8 bytes a row of memory held for the whole task.
//!
//! The intern floor is one hash (one multiply), one 16-wide tag compare and one row compare;
//! a hit reads three columns at one row. A miss adds four pushes and the flags of at most two
//! children.

use blaze_id::ItemKey;
use blaze_int::Cast;
use blaze_own::{Alloc, Vec};

use crate::flags::Flags;
use crate::hash;
use crate::ids::{
    Arg, Const, Def, FloatTy, IntTy, List, MOST_ROWS, Mutability, Node, Pred, Region, TAG_CONST, TAG_REGION, TAG_TY, Ty,
    UintTy, Universe, ix, row,
};
use crate::index::{Index, Probe};
use crate::kind::{self, Desc, Role, ct, re, ty};

#[cold]
#[inline(never)]
fn full(column: &'static str) -> ! {
    panic!(
        "blaze-ty: the {column} column of a task's arena is full at {MOST_ROWS} rows (rule 18): a task this large is a bug in what feeds it"
    )
}

/// One class's rows.
pub(crate) struct Rows<'a> {
    pub(crate) kind: Vec<u8, Alloc<'a>>,
    pub(crate) a: Vec<u32, Alloc<'a>>,
    pub(crate) b: Vec<u32, Alloc<'a>>,
    pub(crate) flags: Vec<Flags, Alloc<'a>>,
    index: Index<'a>,
    name: &'static str,
}

impl<'a> Rows<'a> {
    fn with_capacity_in(rows: usize, name: &'static str, alloc: Alloc<'a>) -> Rows<'a> {
        Rows {
            kind: Vec::with_capacity_in(rows, alloc),
            a: Vec::with_capacity_in(rows, alloc),
            b: Vec::with_capacity_in(rows, alloc),
            flags: Vec::with_capacity_in(rows, alloc),
            index: Index::with_capacity_in(rows, alloc),
            name,
        }
    }

    #[inline(always)]
    fn find(&self, kind: u8, a: u32, b: u32) -> Probe {
        let h = hash::row(kind, a, b);
        self.index.probe(h, |id| {
            let i = ix(id);
            self.a[i] == a && self.b[i] == b && self.kind[i] == kind
        })
    }

    #[inline]
    fn push(&mut self, v: crate::index::Vacant, kind: u8, a: u32, b: u32, flags: Flags) -> u32 {
        let Some(id) = row(self.kind.len()) else { full(self.name) };
        self.kind.push(kind);
        self.a.push(a);
        self.b.push(b);
        self.flags.push(flags);
        let (k, ca, cb) = (&self.kind, &self.a, &self.b);
        self.index.fill(v, id, |r| {
            let i = ix(r);
            hash::row(k[i], ca[i], cb[i])
        });
        id
    }

    pub(crate) fn len(&self) -> usize {
        self.kind.len()
    }

    fn bytes(&self) -> usize {
        self.kind.capacity() + self.a.capacity() * 4 + self.b.capacity() * 4 + self.flags.capacity() * 2
            + self.index.bytes()
    }
}

/// The four kinds of alias.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum AliasKind {
    /// `<T as Trait>::Assoc`.
    Projection,
    /// `Type::Assoc` of an inherent impl.
    Inherent,
    /// `impl Trait`.
    Opaque,
    /// A type alias (lazy `type A = ..`).
    Free,
}

impl AliasKind {
    const fn offset(self) -> u8 {
        match self {
            AliasKind::Projection => 0,
            AliasKind::Inherent => 1,
            AliasKind::Opaque => 2,
            AliasKind::Free => 3,
        }
    }

    /// The alias kind of a type-arena kind and whether it is rigid.
    pub const fn of_kind(k: u8) -> Option<(AliasKind, bool)> {
        if !kind::is_alias(k) {
            return None;
        }
        let rigid = k >= ty::RIGID_PROJECTION;
        let off = if rigid { k - ty::RIGID_PROJECTION } else { k - ty::PROJECTION };
        let kind = match off {
            0 => AliasKind::Projection,
            1 => AliasKind::Inherent,
            2 => AliasKind::Opaque,
            _ => AliasKind::Free,
        };
        Some((kind, rigid))
    }

    /// Its type-arena kind.
    pub const fn kind(self, rigid: bool) -> u8 {
        if rigid { ty::RIGID_PROJECTION + self.offset() } else { ty::PROJECTION + self.offset() }
    }
}

/// A fn pointer's header: ABI, safety, C variadics, and how many late-bound variables its
/// binder has. The ABI's numbering is the front end's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FnHeader(pub u32);

impl FnHeader {
    /// The safe, non-variadic Rust ABI with no bound variables.
    pub const RUST: FnHeader = FnHeader(0);

    pub const fn new(abi: u8, is_unsafe: bool, c_variadic: bool, bound_vars: u16) -> FnHeader {
        let mut bits = Cast::<u8, u32>::widen(abi);
        if is_unsafe {
            bits |= 1 << 8;
        }
        if c_variadic {
            bits |= 1 << 9;
        }
        FnHeader(bits | Cast::<u16, u32>::widen(bound_vars) << 16)
    }

    pub const fn abi(self) -> u8 {
        Cast::<u32, u8>::truncate(self.0)
    }

    pub const fn is_unsafe(self) -> bool {
        self.0 & (1 << 8) != 0
    }

    pub const fn c_variadic(self) -> bool {
        self.0 & (1 << 9) != 0
    }

    /// How many late-bound variables the signature's binder has.
    pub const fn bound_vars(self) -> u16 {
        Cast::<u32, u16>::truncate(self.0 >> 16)
    }
}

/// A type, read back (rustc_type_ir/src/ty_kind.rs, `TyKind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TyKind {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Adt(Def, List),
    Foreign(Def),
    Array(Ty, Const),
    Slice(Ty),
    RawPtr(Ty, Mutability),
    Ref(Region, Ty, Mutability),
    FnDef(Def, List),
    /// The signature (inputs, then the output) under the binder, and the header.
    FnPtr(List, FnHeader),
    /// The predicates (`EX_*` rows) and the region.
    Dynamic(List, Region),
    Closure(Def, List),
    CoroutineClosure(Def, List),
    Coroutine(Def, List),
    CoroutineWitness(Def, List),
    Tuple(List),
    Alias { kind: AliasKind, rigid: bool, def: Def, args: List },
    Param(u32),
    Bound(u32, u32),
    Canonical(u32),
    Placeholder(Universe, u32),
    TyVar(u32),
    IntVar(u32),
    FloatVar(u32),
    Error,
    /// The row is a predicate or a `dyn` predicate.
    NotAType,
}

/// A region, read back (rustc_type_ir/src/region_kind.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    EarlyParam(u32),
    Bound(u32, u32),
    LateParam(Def, u32),
    Static,
    Var(u32),
    Placeholder(Universe, u32),
    Erased,
    Error,
    Canonical(u32),
}

/// A const, read back (rustc_type_ir/src/const_kind.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstKind {
    Param(u32),
    Var(u32),
    Bound(u32, u32),
    Placeholder(Universe, u32),
    Value(Ty, u128),
    Unevaluated(Def, List),
    Error,
    Canonical(u32),
}

/// A predicate, read back (rustc_type_ir/src/predicate_kind.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PredKind {
    Trait(Def, List),
    NotTrait(Def, List),
    Projection(Arg, Arg),
    RegionOutlives(Region, Region),
    TypeOutlives(Ty, Region),
    WellFormed(Arg),
    ConstHasType(Const, Ty),
    ConstEvaluatable(Const),
    Subtype(Ty, Ty),
    Coerce(Ty, Ty),
    AliasEq(Arg, Arg),
    AliasSub(Ty, Ty),
    DynCompatible(Def),
    NormalizesTo(Arg, Arg),
    ConstEquate(Const, Const),
    Ambiguous,
    /// `dyn` predicates.
    ExTrait(Def, List),
    ExProjection(Def, List),
    ExAuto(Def),
    /// The row is a type.
    NotAPred,
}

/// The arena (see the module).
pub struct Tys<'a> {
    alloc: Alloc<'a>,
    pub(crate) ty: Rows<'a>,
    markers: Vec<u32, Alloc<'a>>,
    pub(crate) re: Rows<'a>,
    pub(crate) ct: Rows<'a>,
    pub(crate) list_start: Vec<u32, Alloc<'a>>,
    pub(crate) list_len: Vec<u32, Alloc<'a>>,
    pub(crate) list_flags: Vec<Flags, Alloc<'a>>,
    pub(crate) pool: Vec<Arg, Alloc<'a>>,
    list_index: Index<'a>,
    items: Vec<ItemKey, Alloc<'a>>,
    item_index: Index<'a>,
    scalars: Vec<u128, Alloc<'a>>,
    scalar_index: Index<'a>,
}

#[inline(always)]
fn item_hash(k: &ItemKey) -> u64 {
    // Bytes 0..8 are the crate's prefix, shared by every item of a crate: hash 8..24.
    let b = &k.0.0;
    let mut lo = [0u8; 8];
    lo.copy_from_slice(&b[8..16]);
    let mut hi = [0u8; 8];
    hi.copy_from_slice(&b[16..24]);
    hash::fold_mul(u64::from_le_bytes(lo) ^ 0x9e37_79b9_7f4a_7c15, u64::from_le_bytes(hi) | 1)
}

impl<'a> Tys<'a> {
    /// An arena with the preset rows (`Ty::BOOL` .. the floats, `Region::STATIC`, `ERASED`,
    /// `ERROR`, `Const::ERROR`, `List::EMPTY`), sized for a typical body.
    pub fn new_in(alloc: Alloc<'a>) -> Tys<'a> {
        Tys::with_capacity_in(256, alloc)
    }

    /// An arena with room for about `types` types before a column grows.
    pub fn with_capacity_in(types: usize, alloc: Alloc<'a>) -> Tys<'a> {
        let types = types.max(64);
        let mut t = Tys {
            alloc,
            ty: Rows::with_capacity_in(types, "type", alloc),
            markers: Vec::with_capacity_in(types, alloc),
            re: Rows::with_capacity_in(types / 4, "region", alloc),
            ct: Rows::with_capacity_in(16, "const", alloc),
            list_start: Vec::with_capacity_in(types / 2, alloc),
            list_len: Vec::with_capacity_in(types / 2, alloc),
            list_flags: Vec::with_capacity_in(types / 2, alloc),
            pool: Vec::with_capacity_in(types, alloc),
            list_index: Index::with_capacity_in(types / 2, alloc),
            items: Vec::with_capacity_in(types / 4, alloc),
            item_index: Index::with_capacity_in(types / 4, alloc),
            scalars: Vec::with_capacity_in(16, alloc),
            scalar_index: Index::with_capacity_in(16, alloc),
        };
        // The empty list is row 0 and never in the index: `list_finish` answers it first.
        t.list_start.push(0);
        t.list_len.push(0);
        t.list_flags.push(Flags::default());
        let presets = [
            t.intern_ty(ty::BOOL, 0, 0),
            t.intern_ty(ty::CHAR, 0, 0),
            t.intern_ty(ty::STR, 0, 0),
            t.intern_ty(ty::NEVER, 0, 0),
            t.intern_ty(ty::ERROR, 0, 0),
            t.intern_ty(ty::TUPLE, List::EMPTY.node().bits(), 0),
        ];
        debug_assert!(presets == [Ty::BOOL, Ty::CHAR, Ty::STR, Ty::NEVER, Ty::ERROR, Ty::UNIT]);
        for c in 0..6 {
            t.intern_ty(ty::INT, c, 0);
        }
        for c in 0..6 {
            t.intern_ty(ty::UINT, c, 0);
        }
        for c in 0..4 {
            t.intern_ty(ty::FLOAT, c, 0);
        }
        debug_assert!(t.ty.len() == ix(Ty::PRESET));
        t.intern_re(re::STATIC, 0, 0);
        t.intern_re(re::ERASED, 0, 0);
        t.intern_re(re::ERROR, 0, 0);
        debug_assert!(t.re.len() == ix(Region::PRESET));
        t.intern_ct(ct::ERROR, 0, 0);
        debug_assert!(t.ct.len() == ix(Const::PRESET));
        t
    }

    /// The allocator every column is in.
    pub fn allocator(&self) -> Alloc<'a> {
        self.alloc
    }

    // ---- interning -----------------------------------------------------------------------

    /// The flags of a row from its kind's descriptor and its children's flags.
    #[inline]
    fn row_flags(&self, d: Desc, bound: bool, a: u32, b: u32) -> Flags {
        let mut f = if bound { Flags::bound(d.flags, a) } else { Flags::of(d.flags) };
        if d.a == Role::Child {
            f = f.with_child(self.node_flags(Node(a)), d.binder);
        }
        if d.b == Role::Child {
            f = f.with_child(self.node_flags(Node(b)), d.binder);
        }
        f
    }

    /// Interns a type-arena row. The caller gives children of the classes the kind says.
    #[inline]
    pub(crate) fn intern_ty(&mut self, kind: u8, a: u32, b: u32) -> Ty {
        match self.ty.find(kind, a, b) {
            Probe::Found(id) => Ty(id),
            Probe::Vacant(v) => {
                let f = self.row_flags(kind::ty_d(kind), kind == ty::BOUND, a, b);
                let id = self.ty.push(v, kind, a, b, f);
                self.markers.push(0);
                Ty(id)
            }
        }
    }

    #[inline]
    pub(crate) fn intern_re(&mut self, kind: u8, a: u32, b: u32) -> Region {
        match self.re.find(kind, a, b) {
            Probe::Found(id) => Region(id),
            Probe::Vacant(v) => {
                let f = self.row_flags(kind::re_d(kind), kind == re::BOUND, a, b);
                Region(self.re.push(v, kind, a, b, f))
            }
        }
    }

    #[inline]
    pub(crate) fn intern_ct(&mut self, kind: u8, a: u32, b: u32) -> Const {
        match self.ct.find(kind, a, b) {
            Probe::Found(id) => Const(id),
            Probe::Vacant(v) => {
                let f = self.row_flags(kind::ct_d(kind), kind == ct::BOUND, a, b);
                Const(self.ct.push(v, kind, a, b, f))
            }
        }
    }

    /// Interns a row of the class `tag` (not a list).
    #[inline]
    pub(crate) fn intern_node(&mut self, tag: u32, kind: u8, a: u32, b: u32) -> Node {
        match tag {
            TAG_TY => self.intern_ty(kind, a, b).node(),
            TAG_REGION => self.intern_re(kind, a, b).node(),
            _ => self.intern_ct(kind, a, b).node(),
        }
    }

    /// Where a list being built in the pool starts: push its elements with
    /// [`Tys::list_push`], then intern it with [`Tys::list_finish`].
    #[inline(always)]
    pub fn list_mark(&self) -> usize {
        self.pool.len()
    }

    #[inline(always)]
    pub fn list_push(&mut self, a: Arg) {
        self.pool.push(a);
    }

    /// Interns the elements pushed since `mark`; a list already in the arena gives its id and
    /// drops the pushed copy.
    pub fn list_finish(&mut self, mark: usize) -> List {
        let len = self.pool.len() - mark;
        if len == 0 {
            return List::EMPTY;
        }
        let probe = {
            let words = &self.pool[mark..];
            let h = hash::words(words.iter().map(|a| a.0));
            let (pool, starts, lens) = (&self.pool, &self.list_start, &self.list_len);
            self.list_index.probe(h, |id| {
                let i = ix(id);
                let (s, l) = (ix(starts[i]), ix(lens[i]));
                l == len && pool[s..s + l] == *words
            })
        };
        match probe {
            Probe::Found(id) => {
                self.pool.truncate(mark);
                List(id)
            }
            Probe::Vacant(v) => {
                let Some(id) = row(self.list_start.len()) else { full("list") };
                let Some(start) = row(mark) else { full("list pool") };
                let Some(n) = row(len) else { full("list pool") };
                let mut f = Flags::default();
                for i in mark..self.pool.len() {
                    f = f.with_child(self.node_flags(self.pool[i].node()), false);
                }
                self.list_start.push(start);
                self.list_len.push(n);
                self.list_flags.push(f);
                let (pool, starts, lens) = (&self.pool, &self.list_start, &self.list_len);
                self.list_index.fill(v, id, |r| {
                    let i = ix(r);
                    let (s, l) = (ix(starts[i]), ix(lens[i]));
                    hash::words(pool[s..s + l].iter().map(|a| a.0))
                });
                List(id)
            }
        }
    }

    /// Interns a list of arguments.
    pub fn mk_list(&mut self, args: &[Arg]) -> List {
        let m = self.list_mark();
        self.pool.extend_from_slice(args);
        self.list_finish(m)
    }

    /// Interns a list of types.
    pub fn mk_ty_list(&mut self, tys: &[Ty]) -> List {
        let m = self.list_mark();
        for t in tys {
            self.pool.push(t.arg());
        }
        self.list_finish(m)
    }

    /// The item of a key, interned.
    pub fn def(&mut self, key: ItemKey) -> Def {
        let h = item_hash(&key);
        let items = &self.items;
        match self.item_index.probe(h, |id| items[ix(id)] == key) {
            Probe::Found(id) => Def(id),
            Probe::Vacant(v) => {
                let Some(id) = row(self.items.len()) else { full("item") };
                self.items.push(key);
                let items = &self.items;
                self.item_index.fill(v, id, |r| item_hash(&items[ix(r)]));
                Def(id)
            }
        }
    }

    /// The key of an item.
    #[inline]
    pub fn item_key(&self, d: Def) -> ItemKey {
        self.items[ix(d.0)]
    }

    /// The scalar column's index of a value, interned.
    pub(crate) fn scalar(&mut self, v: u128) -> u32 {
        let h = hash::wide(v);
        let scalars = &self.scalars;
        match self.scalar_index.probe(h, |id| scalars[ix(id)] == v) {
            Probe::Found(id) => id,
            Probe::Vacant(slot) => {
                let Some(id) = row(self.scalars.len()) else { full("scalar") };
                self.scalars.push(v);
                let scalars = &self.scalars;
                self.scalar_index.fill(slot, id, |r| hash::wide(scalars[ix(r)]));
                id
            }
        }
    }

    pub(crate) fn scalar_value(&self, i: u32) -> u128 {
        self.scalars[ix(i)]
    }

    // ---- reading -------------------------------------------------------------------------

    /// A node's flags.
    #[inline(always)]
    pub fn node_flags(&self, n: Node) -> Flags {
        let i = ix(n.index());
        match n.tag() {
            TAG_TY => self.ty.flags[i],
            TAG_REGION => self.re.flags[i],
            TAG_CONST => self.ct.flags[i],
            _ => self.list_flags[i],
        }
    }

    #[inline(always)]
    pub fn flags(&self, t: Ty) -> Flags {
        self.ty.flags[ix(t.0)]
    }

    /// A row of a node's class: `(kind, a, b)`; a list's is `(0, start, len)`.
    #[inline(always)]
    pub fn raw(&self, n: Node) -> (u8, u32, u32) {
        let i = ix(n.index());
        match n.tag() {
            TAG_TY => (self.ty.kind[i], self.ty.a[i], self.ty.b[i]),
            TAG_REGION => (self.re.kind[i], self.re.a[i], self.re.b[i]),
            TAG_CONST => (self.ct.kind[i], self.ct.a[i], self.ct.b[i]),
            _ => (0, self.list_start[i], self.list_len[i]),
        }
    }

    /// The kind code of a type-arena row.
    #[inline(always)]
    pub fn kind_of(&self, t: Ty) -> u8 {
        self.ty.kind[ix(t.0)]
    }

    #[inline(always)]
    pub(crate) fn re_kind_of(&self, r: Region) -> u8 {
        self.re.kind[ix(r.0)]
    }

    #[inline(always)]
    pub(crate) fn ct_kind_of(&self, c: Const) -> u8 {
        self.ct.kind[ix(c.0)]
    }

    /// The descriptor of a node's kind (a list's: no columns).
    #[inline(always)]
    pub(crate) fn desc(&self, n: Node) -> Desc {
        let i = ix(n.index());
        match n.tag() {
            TAG_TY => kind::ty_d(self.ty.kind[i]),
            TAG_REGION => kind::re_d(self.re.kind[i]),
            TAG_CONST => kind::ct_d(self.ct.kind[i]),
            _ => kind::ct_d(u8::MAX),
        }
    }

    /// A list's elements.
    #[inline(always)]
    pub fn list(&self, l: List) -> &[Arg] {
        let i = ix(l.0);
        let s = ix(self.list_start[i]);
        &self.pool[s..s + ix(self.list_len[i])]
    }

    #[inline(always)]
    pub fn list_len(&self, l: List) -> u32 {
        self.list_len[ix(l.0)]
    }

    /// Element `i` of a list.
    #[inline(always)]
    pub fn list_get(&self, l: List, i: u32) -> Option<Arg> {
        self.list(l).get(ix(i)).copied()
    }

    /// The solver's marker bits of a type (`flags::markers`).
    #[inline(always)]
    pub fn markers(&self, t: Ty) -> u32 {
        self.markers[ix(t.0)]
    }

    /// Adds marker bits to a type.
    #[inline(always)]
    pub fn set_markers(&mut self, t: Ty, bits: u32) {
        self.markers[ix(t.0)] |= bits;
    }

    /// How many types (and predicates) the arena holds.
    pub fn type_count(&self) -> usize {
        self.ty.len()
    }

    /// Bytes the arena holds, every column and index.
    pub fn bytes(&self) -> usize {
        self.ty.bytes()
            + self.markers.capacity() * 4
            + self.re.bytes()
            + self.ct.bytes()
            + (self.list_start.capacity() + self.list_len.capacity() + self.pool.capacity()) * 4
            + self.list_flags.capacity() * 2
            + self.list_index.bytes()
            + self.items.capacity() * 32
            + self.item_index.bytes()
            + self.scalars.capacity() * 16
            + self.scalar_index.bytes()
    }

    /// A type, read back.
    pub fn ty_kind(&self, t: Ty) -> TyKind {
        let i = ix(t.0);
        let (k, a, b) = (self.ty.kind[i], self.ty.a[i], self.ty.b[i]);
        let na = Node(a);
        let nb = Node(b);
        match k {
            ty::BOOL => TyKind::Bool,
            ty::CHAR => TyKind::Char,
            ty::INT => match IntTy::of_code(a) {
                Some(x) => TyKind::Int(x),
                None => TyKind::NotAType,
            },
            ty::UINT => match UintTy::of_code(a) {
                Some(x) => TyKind::Uint(x),
                None => TyKind::NotAType,
            },
            ty::FLOAT => match FloatTy::of_code(a) {
                Some(x) => TyKind::Float(x),
                None => TyKind::NotAType,
            },
            ty::STR => TyKind::Str,
            ty::NEVER => TyKind::Never,
            ty::ADT => TyKind::Adt(Def(a), nb.list()),
            ty::FOREIGN => TyKind::Foreign(Def(a)),
            ty::ARRAY => TyKind::Array(na.ty(), nb.konst()),
            ty::SLICE => TyKind::Slice(na.ty()),
            ty::PTR => TyKind::RawPtr(na.ty(), Mutability::Not),
            ty::PTR_MUT => TyKind::RawPtr(na.ty(), Mutability::Mut),
            ty::REF => TyKind::Ref(nb.region(), na.ty(), Mutability::Not),
            ty::REF_MUT => TyKind::Ref(nb.region(), na.ty(), Mutability::Mut),
            ty::FN_DEF => TyKind::FnDef(Def(a), nb.list()),
            ty::FN_PTR => TyKind::FnPtr(na.list(), FnHeader(b)),
            ty::DYNAMIC => TyKind::Dynamic(na.list(), nb.region()),
            ty::CLOSURE => TyKind::Closure(Def(a), nb.list()),
            ty::CORO_CLOSURE => TyKind::CoroutineClosure(Def(a), nb.list()),
            ty::COROUTINE => TyKind::Coroutine(Def(a), nb.list()),
            ty::CORO_WITNESS => TyKind::CoroutineWitness(Def(a), nb.list()),
            ty::TUPLE => TyKind::Tuple(na.list()),
            ty::PARAM => TyKind::Param(a),
            ty::BOUND => TyKind::Bound(a, b),
            ty::CANONICAL => TyKind::Canonical(a),
            ty::PLACEHOLDER => TyKind::Placeholder(Universe(a), b),
            ty::TY_VAR => TyKind::TyVar(a),
            ty::INT_VAR => TyKind::IntVar(a),
            ty::FLOAT_VAR => TyKind::FloatVar(a),
            ty::ERROR => TyKind::Error,
            _ => match AliasKind::of_kind(k) {
                Some((kind, rigid)) => TyKind::Alias { kind, rigid, def: Def(a), args: nb.list() },
                None => TyKind::NotAType,
            },
        }
    }

    /// A region, read back.
    pub fn region_kind(&self, r: Region) -> RegionKind {
        let i = ix(r.0);
        let (k, a, b) = (self.re.kind[i], self.re.a[i], self.re.b[i]);
        match k {
            re::EARLY_PARAM => RegionKind::EarlyParam(a),
            re::BOUND => RegionKind::Bound(a, b),
            re::LATE_PARAM => RegionKind::LateParam(Def(a), b),
            re::STATIC => RegionKind::Static,
            re::VAR => RegionKind::Var(a),
            re::PLACEHOLDER => RegionKind::Placeholder(Universe(a), b),
            re::ERASED => RegionKind::Erased,
            re::CANONICAL => RegionKind::Canonical(a),
            _ => RegionKind::Error,
        }
    }

    /// A const, read back.
    pub fn const_kind(&self, c: Const) -> ConstKind {
        let i = ix(c.0);
        let (k, a, b) = (self.ct.kind[i], self.ct.a[i], self.ct.b[i]);
        match k {
            ct::PARAM => ConstKind::Param(a),
            ct::VAR => ConstKind::Var(a),
            ct::BOUND => ConstKind::Bound(a, b),
            ct::PLACEHOLDER => ConstKind::Placeholder(Universe(a), b),
            ct::VALUE => ConstKind::Value(Node(a).ty(), self.scalar_value(b)),
            ct::UNEVALUATED => ConstKind::Unevaluated(Def(a), Node(b).list()),
            ct::CANONICAL => ConstKind::Canonical(a),
            _ => ConstKind::Error,
        }
    }

    /// A predicate, read back.
    pub fn pred_kind(&self, p: Pred) -> PredKind {
        let i = ix(p.0);
        let (k, a, b) = (self.ty.kind[i], self.ty.a[i], self.ty.b[i]);
        let (na, nb) = (Node(a), Node(b));
        let (aa, ab) = (Arg(a), Arg(b));
        match k {
            ty::P_TRAIT => PredKind::Trait(Def(a), nb.list()),
            ty::P_NOT_TRAIT => PredKind::NotTrait(Def(a), nb.list()),
            ty::P_PROJECTION => PredKind::Projection(aa, ab),
            ty::P_REGION_OUTLIVES => PredKind::RegionOutlives(na.region(), nb.region()),
            ty::P_TYPE_OUTLIVES => PredKind::TypeOutlives(na.ty(), nb.region()),
            ty::P_WELL_FORMED => PredKind::WellFormed(aa),
            ty::P_CONST_HAS_TYPE => PredKind::ConstHasType(na.konst(), nb.ty()),
            ty::P_CONST_EVALUATABLE => PredKind::ConstEvaluatable(na.konst()),
            ty::P_SUBTYPE => PredKind::Subtype(na.ty(), nb.ty()),
            ty::P_COERCE => PredKind::Coerce(na.ty(), nb.ty()),
            ty::P_ALIAS_EQ => PredKind::AliasEq(aa, ab),
            ty::P_ALIAS_SUB => PredKind::AliasSub(na.ty(), nb.ty()),
            ty::P_DYN_COMPATIBLE => PredKind::DynCompatible(Def(a)),
            ty::P_NORMALIZES_TO => PredKind::NormalizesTo(aa, ab),
            ty::P_CONST_EQUATE => PredKind::ConstEquate(na.konst(), nb.konst()),
            ty::P_AMBIGUOUS => PredKind::Ambiguous,
            ty::EX_TRAIT => PredKind::ExTrait(Def(a), nb.list()),
            ty::EX_PROJECTION => PredKind::ExProjection(Def(a), nb.list()),
            ty::EX_AUTO => PredKind::ExAuto(Def(a)),
            _ => PredKind::NotAPred,
        }
    }

    /// The predicate a type-arena row is, if it is one.
    pub fn as_pred(&self, t: Ty) -> Option<Pred> {
        if kind::is_pred(self.kind_of(t)) { Some(Pred(t.0)) } else { None }
    }

    // ---- types ---------------------------------------------------------------------------

    pub fn adt(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::ADT, def.0, args.node().bits())
    }

    pub fn foreign(&mut self, def: Def) -> Ty {
        self.intern_ty(ty::FOREIGN, def.0, 0)
    }

    pub fn array(&mut self, elem: Ty, len: Const) -> Ty {
        self.intern_ty(ty::ARRAY, elem.node().bits(), len.node().bits())
    }

    /// `[elem; n]` with a `usize` length.
    pub fn array_n(&mut self, elem: Ty, n: u64) -> Ty {
        let len = self.const_value(Ty::uint(UintTy::Usize), u128::from(n));
        self.array(elem, len)
    }

    pub fn slice(&mut self, elem: Ty) -> Ty {
        self.intern_ty(ty::SLICE, elem.node().bits(), 0)
    }

    pub fn ptr(&mut self, pointee: Ty, m: Mutability) -> Ty {
        let k = match m {
            Mutability::Not => ty::PTR,
            Mutability::Mut => ty::PTR_MUT,
        };
        self.intern_ty(k, pointee.node().bits(), 0)
    }

    pub fn reference(&mut self, r: Region, pointee: Ty, m: Mutability) -> Ty {
        let k = match m {
            Mutability::Not => ty::REF,
            Mutability::Mut => ty::REF_MUT,
        };
        self.intern_ty(k, pointee.node().bits(), r.node().bits())
    }

    pub fn fn_def(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::FN_DEF, def.0, args.node().bits())
    }

    /// A fn pointer: `sig` is the inputs then the output, under the pointer's binder.
    pub fn fn_ptr(&mut self, sig: List, header: FnHeader) -> Ty {
        self.intern_ty(ty::FN_PTR, sig.node().bits(), header.0)
    }

    /// `dyn ..`: `preds` are `EX_*` rows (`ex_trait`, `ex_projection`, `ex_auto`), principal
    /// first, in the order the front end sorts them.
    pub fn dynamic(&mut self, preds: List, r: Region) -> Ty {
        self.intern_ty(ty::DYNAMIC, preds.node().bits(), r.node().bits())
    }

    pub fn closure(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::CLOSURE, def.0, args.node().bits())
    }

    pub fn coroutine_closure(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::CORO_CLOSURE, def.0, args.node().bits())
    }

    pub fn coroutine(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::COROUTINE, def.0, args.node().bits())
    }

    pub fn coroutine_witness(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::CORO_WITNESS, def.0, args.node().bits())
    }

    /// A tuple of the list's types (the list must hold types only).
    pub fn tuple(&mut self, fields: List) -> Ty {
        self.intern_ty(ty::TUPLE, fields.node().bits(), 0)
    }

    pub fn tuple_of(&mut self, fields: &[Ty]) -> Ty {
        let l = self.mk_ty_list(fields);
        self.tuple(l)
    }

    /// An alias not known to be rigid.
    pub fn alias(&mut self, kind: AliasKind, def: Def, args: List) -> Ty {
        self.intern_ty(kind.kind(false), def.0, args.node().bits())
    }

    /// An alias known to be rigid (normalizing it gives itself).
    pub fn rigid_alias(&mut self, kind: AliasKind, def: Def, args: List) -> Ty {
        self.intern_ty(kind.kind(true), def.0, args.node().bits())
    }

    pub fn param(&mut self, index: u32) -> Ty {
        self.intern_ty(ty::PARAM, index, 0)
    }

    /// A bound type variable `debruijn` binders up.
    pub fn bound(&mut self, debruijn: u32, var: u32) -> Ty {
        self.intern_ty(ty::BOUND, debruijn, var)
    }

    pub fn placeholder(&mut self, u: Universe, var: u32) -> Ty {
        self.intern_ty(ty::PLACEHOLDER, u.0, var)
    }

    pub(crate) fn canonical_ty(&mut self, var: u32) -> Ty {
        self.intern_ty(ty::CANONICAL, var, 0)
    }

    pub(crate) fn ty_var(&mut self, v: u32) -> Ty {
        self.intern_ty(ty::TY_VAR, v, 0)
    }

    pub(crate) fn int_var(&mut self, v: u32) -> Ty {
        self.intern_ty(ty::INT_VAR, v, 0)
    }

    pub(crate) fn float_var(&mut self, v: u32) -> Ty {
        self.intern_ty(ty::FLOAT_VAR, v, 0)
    }

    // ---- `dyn` predicates ----------------------------------------------------------------

    /// The principal trait of a `dyn`: its arguments without `Self`.
    pub fn ex_trait(&mut self, def: Def, args: List) -> Ty {
        self.intern_ty(ty::EX_TRAIT, def.0, args.node().bits())
    }

    /// `Assoc = term` of a `dyn`: the args without `Self`, then the term last.
    pub fn ex_projection(&mut self, def: Def, args_then_term: List) -> Ty {
        self.intern_ty(ty::EX_PROJECTION, def.0, args_then_term.node().bits())
    }

    pub fn ex_auto(&mut self, def: Def) -> Ty {
        self.intern_ty(ty::EX_AUTO, def.0, 0)
    }

    // ---- regions -------------------------------------------------------------------------

    pub fn re_early_param(&mut self, index: u32) -> Region {
        self.intern_re(re::EARLY_PARAM, index, 0)
    }

    pub fn re_bound(&mut self, debruijn: u32, var: u32) -> Region {
        self.intern_re(re::BOUND, debruijn, var)
    }

    pub fn re_late_param(&mut self, scope: Def, var: u32) -> Region {
        self.intern_re(re::LATE_PARAM, scope.0, var)
    }

    pub fn re_placeholder(&mut self, u: Universe, var: u32) -> Region {
        self.intern_re(re::PLACEHOLDER, u.0, var)
    }

    pub(crate) fn re_var(&mut self, v: u32) -> Region {
        self.intern_re(re::VAR, v, 0)
    }

    pub(crate) fn re_canonical(&mut self, var: u32) -> Region {
        self.intern_re(re::CANONICAL, var, 0)
    }

    // ---- consts --------------------------------------------------------------------------

    pub fn const_param(&mut self, index: u32) -> Const {
        self.intern_ct(ct::PARAM, index, 0)
    }

    pub fn const_bound(&mut self, debruijn: u32, var: u32) -> Const {
        self.intern_ct(ct::BOUND, debruijn, var)
    }

    pub fn const_placeholder(&mut self, u: Universe, var: u32) -> Const {
        self.intern_ct(ct::PLACEHOLDER, u.0, var)
    }

    /// A value of a type: its bits, as the front end lays out a scalar (or the id of a
    /// value tree the front end keeps).
    pub fn const_value(&mut self, t: Ty, bits: u128) -> Const {
        let s = self.scalar(bits);
        self.intern_ct(ct::VALUE, t.node().bits(), s)
    }

    pub fn const_unevaluated(&mut self, def: Def, args: List) -> Const {
        self.intern_ct(ct::UNEVALUATED, def.0, args.node().bits())
    }

    pub(crate) fn const_var(&mut self, v: u32) -> Const {
        self.intern_ct(ct::VAR, v, 0)
    }

    pub(crate) fn const_canonical(&mut self, var: u32) -> Const {
        self.intern_ct(ct::CANONICAL, var, 0)
    }

    // ---- predicates ----------------------------------------------------------------------

    fn pred(&mut self, kind: u8, a: u32, b: u32) -> Pred {
        Pred(self.intern_ty(kind, a, b).0)
    }

    /// `args[0]: Trait<args[1..]>`.
    pub fn pred_trait(&mut self, def: Def, args: List) -> Pred {
        self.pred(ty::P_TRAIT, def.0, args.node().bits())
    }

    pub fn pred_not_trait(&mut self, def: Def, args: List) -> Pred {
        self.pred(ty::P_NOT_TRAIT, def.0, args.node().bits())
    }

    /// `alias == term` as a clause (rustc's `ProjectionPredicate`); `alias` is an alias type
    /// or an unevaluated const.
    pub fn pred_projection(&mut self, alias: Arg, term: Arg) -> Pred {
        self.pred(ty::P_PROJECTION, alias.0, term.0)
    }

    /// `longer: shorter`.
    pub fn pred_region_outlives(&mut self, longer: Region, shorter: Region) -> Pred {
        self.pred(ty::P_REGION_OUTLIVES, longer.node().bits(), shorter.node().bits())
    }

    pub fn pred_type_outlives(&mut self, t: Ty, r: Region) -> Pred {
        self.pred(ty::P_TYPE_OUTLIVES, t.node().bits(), r.node().bits())
    }

    pub fn pred_well_formed(&mut self, a: Arg) -> Pred {
        self.pred(ty::P_WELL_FORMED, a.0, 0)
    }

    pub fn pred_const_has_type(&mut self, c: Const, t: Ty) -> Pred {
        self.pred(ty::P_CONST_HAS_TYPE, c.node().bits(), t.node().bits())
    }

    pub fn pred_const_evaluatable(&mut self, c: Const) -> Pred {
        self.pred(ty::P_CONST_EVALUATABLE, c.node().bits(), 0)
    }

    /// `a <: b`.
    pub fn pred_subtype(&mut self, a: Ty, b: Ty) -> Pred {
        self.pred(ty::P_SUBTYPE, a.node().bits(), b.node().bits())
    }

    pub fn pred_coerce(&mut self, a: Ty, b: Ty) -> Pred {
        self.pred(ty::P_COERCE, a.node().bits(), b.node().bits())
    }

    pub fn pred_alias_eq(&mut self, a: Arg, b: Arg) -> Pred {
        self.pred(ty::P_ALIAS_EQ, a.0, b.0)
    }

    pub fn pred_alias_sub(&mut self, a: Ty, b: Ty) -> Pred {
        self.pred(ty::P_ALIAS_SUB, a.node().bits(), b.node().bits())
    }

    pub fn pred_dyn_compatible(&mut self, def: Def) -> Pred {
        self.pred(ty::P_DYN_COMPATIBLE, def.0, 0)
    }

    pub fn pred_normalizes_to(&mut self, alias: Arg, term: Arg) -> Pred {
        self.pred(ty::P_NORMALIZES_TO, alias.0, term.0)
    }

    pub fn pred_const_equate(&mut self, a: Const, b: Const) -> Pred {
        self.pred(ty::P_CONST_EQUATE, a.node().bits(), b.node().bits())
    }

    pub fn pred_ambiguous(&mut self) -> Pred {
        self.pred(ty::P_AMBIGUOUS, 0, 0)
    }
}
