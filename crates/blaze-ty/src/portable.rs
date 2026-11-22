//! The portable form: types as a record holds them, meaningful in any process.
//!
//! A [`PTys`] is a table of rows `(code, a, b)`, `code` the class in its top two bits and the
//! kind below, children before parents, each child column a row number of the table, items as
//! their `ItemKey`s in a side column, const values in a scalar column, lists as rows over a
//! pool of row numbers. It is hash-consed, so a type is one row however often it is shared,
//! and rows are numbered in first-use order, so one type exported alone into an empty table
//! gives the same bytes in every process: equal types, equal bytes.
//!
//! Inference variables have no meaning outside their task: they export as holes (the
//! variable kinds with number 0), which an import hands back to the caller to fill.
//! This is the one definition: blaze-typeck's `portable.rs` is to move onto it.

use blaze_id::ItemKey;
use blaze_int::Int;
use blaze_own::{Alloc, Vec};

use crate::hash;
use crate::ids::{Arg, Def, Node, TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY, VarKind, ix, row};
use crate::index::{Index, Probe};
use crate::kind::{self, Role, ct, re, ty};
use crate::memo::Memo;
use crate::tys::Tys;

/// A row of a portable table.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PTy(pub u32);

/// Why bytes or a table did not import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportError {
    /// The bytes are cut short or have the wrong header.
    Truncated { at: usize },
    BadMagic,
    /// A row whose kind is not one of its class.
    BadKind { row: u32 },
    /// A row whose child is not an earlier row of an allowed class.
    BadChild { row: u32, child: u32 },
    /// A row's item or scalar number past its column.
    BadIndex { row: u32 },
    /// A list over pool words past the pool.
    BadList { row: u32 },
    /// A row twice: the bytes are not canonical.
    Duplicate { row: u32 },
    /// The row asked for is past the table.
    NoRow { row: u32 },
}

impl core::fmt::Display for ImportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ImportError::Truncated { at } => write!(f, "portable types cut short at byte {at}"),
            ImportError::BadMagic => write!(f, "not portable types (bad header)"),
            ImportError::BadKind { row } => write!(f, "row {row} has a kind not of its class"),
            ImportError::BadChild { row, child } => {
                write!(f, "row {row} has child {child}, not an earlier row of the class it needs")
            }
            ImportError::BadIndex { row } => write!(f, "row {row} names an item or value past its column"),
            ImportError::BadList { row } => write!(f, "list row {row} runs past the pool"),
            ImportError::Duplicate { row } => write!(f, "row {row} repeats an earlier row"),
            ImportError::NoRow { row } => write!(f, "row {row} is past the table"),
        }
    }
}

impl core::error::Error for ImportError {}

const MAGIC: u32 = 0x3159_5442; // "BTY1"
/// A hole's number.
const HOLE: u32 = 0;

#[inline(always)]
fn code(tag: u32, k: u8) -> u8 {
    // `tag` is at most 3 and kinds of a class are under 64.
    (tag.truncate_to::<u8>() << 6) | (k & 0x3f)
}

#[inline(always)]
fn tag_of(code: u8) -> u32 {
    u32::from(code >> 6)
}

/// A portable table (see the module).
pub struct PTys<'b> {
    code: Vec<u8, Alloc<'b>>,
    a: Vec<u32, Alloc<'b>>,
    b: Vec<u32, Alloc<'b>>,
    pool: Vec<u32, Alloc<'b>>,
    items: Vec<ItemKey, Alloc<'b>>,
    scalars: Vec<u128, Alloc<'b>>,
    index: Index<'b>,
    item_index: Index<'b>,
    scalar_index: Index<'b>,
    memo: Memo<'b>,
    stack: Vec<(Node, bool), Alloc<'b>>,
    out: Vec<u32, Alloc<'b>>,
}

fn row_hash(code: u8, a: u32, b: u32, pool: &[u32]) -> u64 {
    if tag_of(code) == TAG_LIST {
        let (s, l) = (ix(a), ix(b));
        hash::words(pool[s..s + l].iter().copied())
    } else {
        hash::row(code, a, b)
    }
}

fn item_hash(k: &ItemKey) -> u64 {
    let b = &k.0.0;
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[8..16]);
    hash::key(u64::from_le_bytes(w))
}

impl<'b> PTys<'b> {
    pub fn new_in(alloc: Alloc<'b>) -> PTys<'b> {
        PTys {
            code: Vec::new_in(alloc),
            a: Vec::new_in(alloc),
            b: Vec::new_in(alloc),
            pool: Vec::new_in(alloc),
            items: Vec::new_in(alloc),
            scalars: Vec::new_in(alloc),
            index: Index::with_capacity_in(16, alloc),
            item_index: Index::with_capacity_in(16, alloc),
            scalar_index: Index::with_capacity_in(16, alloc),
            memo: Memo::new_in(alloc),
            stack: Vec::new_in(alloc),
            out: Vec::new_in(alloc),
        }
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// A row: its class, kind and columns.
    pub fn row(&self, p: PTy) -> Option<(u32, u8, u32, u32)> {
        let i = ix(p.0);
        let c = *self.code.get(i)?;
        Some((tag_of(c), c & 0x3f, self.a[i], self.b[i]))
    }

    /// A list row's elements.
    pub fn list(&self, p: PTy) -> &[u32] {
        match self.row(p) {
            Some((TAG_LIST, _, s, l)) => &self.pool[ix(s)..ix(s) + ix(l)],
            _ => &[],
        }
    }

    pub fn item(&self, i: u32) -> Option<ItemKey> {
        self.items.get(ix(i)).copied()
    }

    fn intern_item(&mut self, key: ItemKey) -> u32 {
        let h = item_hash(&key);
        let items = &self.items;
        match self.item_index.probe(h, |id| items[ix(id)] == key) {
            Probe::Found(id) => id,
            Probe::Vacant(v) => {
                let Some(id) = row(self.items.len()) else { panic!("blaze-ty: a portable table is full (rule 18)") };
                self.items.push(key);
                let items = &self.items;
                self.item_index.fill(v, id, |r| item_hash(&items[ix(r)]));
                id
            }
        }
    }

    fn intern_scalar(&mut self, v: u128) -> u32 {
        let h = hash::wide(v);
        let s = &self.scalars;
        match self.scalar_index.probe(h, |id| s[ix(id)] == v) {
            Probe::Found(id) => id,
            Probe::Vacant(slot) => {
                let Some(id) = row(self.scalars.len()) else { panic!("blaze-ty: a portable table is full (rule 18)") };
                self.scalars.push(v);
                let s = &self.scalars;
                self.scalar_index.fill(slot, id, |r| hash::wide(s[ix(r)]));
                id
            }
        }
    }

    /// Interns a row; a list's elements are the pool's words from `a` on, `b` of them, just
    /// pushed (dropped again if the list is there already).
    fn intern_row(&mut self, c: u8, a: u32, b: u32) -> Result<u32, u32> {
        let h = row_hash(c, a, b, &self.pool);
        let (codes, ca, cb, pool) = (&self.code, &self.a, &self.b, &self.pool);
        let list = tag_of(c) == TAG_LIST;
        let probe = self.index.probe(h, |id| {
            let i = ix(id);
            if codes[i] != c {
                return false;
            }
            if list {
                let (s, l) = (ix(ca[i]), ix(cb[i]));
                l == ix(b) && pool[s..s + l] == pool[ix(a)..ix(a) + ix(b)]
            } else {
                ca[i] == a && cb[i] == b
            }
        });
        match probe {
            Probe::Found(id) => Err(id),
            Probe::Vacant(v) => {
                let Some(id) = row(self.code.len()) else { panic!("blaze-ty: a portable table is full (rule 18)") };
                self.code.push(c);
                self.a.push(a);
                self.b.push(b);
                let (codes, ca, cb, pool) = (&self.code, &self.a, &self.b, &self.pool);
                self.index.fill(v, id, |r| row_hash(codes[ix(r)], ca[ix(r)], cb[ix(r)], pool));
                Ok(id)
            }
        }
    }

    /// Exports a node of an arena (resolve its variables first: unresolved ones become
    /// holes).
    pub fn export(&mut self, tys: &Tys<'_>, n: Node) -> PTy {
        self.memo.clear();
        self.stack.clear();
        self.out.clear();
        self.stack.push((n, false));
        while let Some((n, exit)) = self.stack.pop() {
            if !exit {
                if let Some(p) = self.memo.get(u64::from(n.bits())) {
                    self.out.push(p);
                    continue;
                }
                self.stack.push((n, true));
                if n.tag() == TAG_LIST {
                    for el in tys.list(n.list()).iter().rev() {
                        self.stack.push((el.node(), false));
                    }
                } else {
                    let (_, a, b) = tys.raw(n);
                    let d = tys.desc(n);
                    if d.b == Role::Child {
                        self.stack.push((Node(b), false));
                    }
                    if d.a == Role::Child {
                        self.stack.push((Node(a), false));
                    }
                }
                continue;
            }
            let p = if n.tag() == TAG_LIST {
                let len = tys.list_len(n.list());
                let base = self.out.len() - ix(len);
                let Some(start) = row(self.pool.len()) else { panic!("blaze-ty: a portable table is full (rule 18)") };
                self.pool.extend_from_slice(&self.out[base..]);
                self.out.truncate(base);
                match self.intern_row(code(TAG_LIST, 0), start, len) {
                    Ok(id) => id,
                    Err(id) => {
                        self.pool.truncate(ix(start));
                        id
                    }
                }
            } else {
                let (k, a, b) = tys.raw(n);
                let d = tys.desc(n);
                let children = usize::from(d.a == Role::Child) + usize::from(d.b == Role::Child);
                let base = self.out.len() - children;
                let mut next = base;
                let mut col = |this: &mut PTys<'b>, role: Role, x: u32| -> u32 {
                    match role {
                        Role::Child => {
                            next += 1;
                            this.out[next - 1]
                        }
                        Role::Def => this.intern_item(tys.item_key(Def(x))),
                        Role::Scalar => this.intern_scalar(tys.scalar_value(x)),
                        Role::Raw => x,
                        Role::None => 0,
                    }
                };
                let hole = matches!(
                    (n.tag(), k),
                    (TAG_TY, ty::TY_VAR | ty::INT_VAR | ty::FLOAT_VAR) | (TAG_REGION, re::VAR) | (TAG_CONST, ct::VAR)
                );
                let pa = if hole { HOLE } else { col(self, d.a, a) };
                let pb = col(self, d.b, b);
                self.out.truncate(base);
                match self.intern_row(code(n.tag(), k), pa, pb) {
                    Ok(id) | Err(id) => id,
                }
            };
            self.memo.insert(u64::from(n.bits()), p);
            self.out.push(p);
        }
        PTy(self.out.pop().unwrap_or(0))
    }

    /// Imports every row into an arena, in one forward pass, checking each; `map` gets the
    /// arena node of each row. `hole` gives the node for a hole of a variable kind.
    pub fn import(
        &self,
        tys: &mut Tys<'_>,
        hole: &mut dyn FnMut(&mut Tys<'_>, VarKind) -> Node,
        map: &mut Vec<u32, Alloc<'_>>,
    ) -> Result<(), ImportError> {
        map.clear();
        for i in 0..self.code.len() {
            let Some(r) = row(i) else { return Err(ImportError::NoRow { row: u32::MAX }) };
            let (c, a, b) = (self.code[i], self.a[i], self.b[i]);
            let tag = tag_of(c);
            let k = c & 0x3f;
            let node = if tag == TAG_LIST {
                let (s, l) = (ix(a), ix(b));
                if s + l > self.pool.len() {
                    return Err(ImportError::BadList { row: r });
                }
                let m = tys.list_mark();
                for &el in &self.pool[s..s + l] {
                    let child = self.child(map, r, el, (1 << TAG_TY) | (1 << TAG_REGION) | (1 << TAG_CONST))?;
                    tys.list_push(Arg(child.bits()));
                }
                tys.list_finish(m).node()
            } else {
                if !kind::valid(tag, k) {
                    return Err(ImportError::BadKind { row: r });
                }
                let hole_kind = match (tag, k) {
                    (TAG_TY, ty::TY_VAR) => Some(VarKind::Ty),
                    (TAG_TY, ty::INT_VAR) => Some(VarKind::Int),
                    (TAG_TY, ty::FLOAT_VAR) => Some(VarKind::Float),
                    (TAG_REGION, re::VAR) => Some(VarKind::Region),
                    (TAG_CONST, ct::VAR) => Some(VarKind::Const),
                    _ => None,
                };
                if let Some(hk) = hole_kind {
                    hole(tys, hk)
                } else {
                    let d = match tag {
                        TAG_TY => kind::ty_d(k),
                        TAG_REGION => kind::re_d(k),
                        _ => kind::ct_d(k),
                    };
                    if tag == TAG_TY
                        && ((k == ty::INT || k == ty::UINT) && a >= 6 || k == ty::FLOAT && a >= 4)
                    {
                        return Err(ImportError::BadKind { row: r });
                    }
                    let mut col = |role: Role, x: u32, second: bool| -> Result<u32, ImportError> {
                        Ok(match role {
                            Role::Child => self.child(map, r, x, kind::child_classes(tag, k, second))?.bits(),
                            Role::Def => tys.def(self.item(x).ok_or(ImportError::BadIndex { row: r })?).index(),
                            Role::Scalar => {
                                let v = *self.scalars.get(ix(x)).ok_or(ImportError::BadIndex { row: r })?;
                                tys.scalar(v)
                            }
                            Role::Raw => x,
                            Role::None => 0,
                        })
                    };
                    let na = col(d.a, a, false)?;
                    let nb = col(d.b, b, true)?;
                    tys.intern_node(tag, k, na, nb)
                }
            };
            map.push(node.bits());
        }
        Ok(())
    }

    /// The arena node of an earlier row of an allowed class.
    fn child(&self, map: &[u32], r: u32, child: u32, allowed: u8) -> Result<Node, ImportError> {
        let Some(&bits) = map.get(ix(child)) else { return Err(ImportError::BadChild { row: r, child }) };
        let node = Node(bits);
        if allowed & (1 << node.tag()) == 0 {
            return Err(ImportError::BadChild { row: r, child });
        }
        Ok(node)
    }

    /// The node an import gave a row.
    pub fn node_of(map: &[u32], p: PTy) -> Result<Node, ImportError> {
        match map.get(ix(p.0)) {
            Some(&bits) => Ok(Node(bits)),
            None => Err(ImportError::NoRow { row: p.0 }),
        }
    }

    /// The table as bytes, little-endian: header, then each column.
    pub fn to_bytes(&self, out: &mut Vec<u8, Alloc<'_>>) {
        let len = |n: usize| -> u32 {
            match row(n) {
                Some(x) => x,
                None => panic!("blaze-ty: a portable table is full (rule 18)"),
            }
        };
        for w in [MAGIC, len(self.code.len()), len(self.pool.len()), len(self.items.len()), len(self.scalars.len())] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.extend_from_slice(&self.code);
        for col in [&self.a, &self.b, &self.pool] {
            for w in col.iter() {
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
        for k in self.items.iter() {
            out.extend_from_slice(&k.0.0);
        }
        for s in self.scalars.iter() {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }

    /// A table from [`PTys::to_bytes`]'s bytes. Rows are checked when imported; here, that the
    /// columns are whole and no row repeats.
    pub fn from_bytes(bytes: &[u8], alloc: Alloc<'b>) -> Result<PTys<'b>, ImportError> {
        let mut rd = Reader { bytes, at: 0 };
        if rd.word()? != MAGIC {
            return Err(ImportError::BadMagic);
        }
        let rows = ix(rd.word()?);
        let pool = ix(rd.word()?);
        let items = ix(rd.word()?);
        let scalars = ix(rd.word()?);
        let mut t = PTys::new_in(alloc);
        t.code.extend_from_slice(rd.take(rows)?);
        for _ in 0..rows {
            t.a.push(rd.word()?);
        }
        for _ in 0..rows {
            t.b.push(rd.word()?);
        }
        for _ in 0..pool {
            t.pool.push(rd.word()?);
        }
        for _ in 0..items {
            let s = rd.take(32)?;
            let mut k = [0u8; 32];
            k.copy_from_slice(s);
            t.items.push(ItemKey(blaze_id::ContentId(k)));
        }
        for _ in 0..scalars {
            let s = rd.take(16)?;
            let mut v = [0u8; 16];
            v.copy_from_slice(s);
            t.scalars.push(u128::from_le_bytes(v));
        }
        // Rebuild the indexes; a repeat means the bytes were not written by `to_bytes`.
        for i in 0..rows {
            let Some(r) = row(i) else { return Err(ImportError::NoRow { row: u32::MAX }) };
            let (c, a, b) = (t.code[i], t.a[i], t.b[i]);
            if tag_of(c) == TAG_LIST && ix(a) + ix(b) > t.pool.len() {
                return Err(ImportError::BadList { row: r });
            }
            let h = row_hash(c, a, b, &t.pool);
            let (codes, ca, cb, tp) = (&t.code, &t.a, &t.b, &t.pool);
            let list = tag_of(c) == TAG_LIST;
            let probe = t.index.probe(h, |id| {
                let j = ix(id);
                codes[j] == c
                    && if list {
                        let (s, l) = (ix(ca[j]), ix(cb[j]));
                        l == ix(b) && tp[s..s + l] == tp[ix(a)..ix(a) + ix(b)]
                    } else {
                        ca[j] == a && cb[j] == b
                    }
            });
            match probe {
                Probe::Found(_) => return Err(ImportError::Duplicate { row: r }),
                Probe::Vacant(v) => {
                    let (codes, ca, cb, tp) = (&t.code, &t.a, &t.b, &t.pool);
                    t.index.fill(v, r, |x| row_hash(codes[ix(x)], ca[ix(x)], cb[ix(x)], tp));
                }
            }
        }
        for i in 0..items {
            let Some(r) = row(i) else { return Err(ImportError::NoRow { row: u32::MAX }) };
            let it = &t.items;
            match t.item_index.probe(item_hash(&it[i]), |id| it[ix(id)] == it[i]) {
                Probe::Found(_) => return Err(ImportError::Duplicate { row: r }),
                Probe::Vacant(v) => {
                    let it = &t.items;
                    t.item_index.fill(v, r, |x| item_hash(&it[ix(x)]));
                }
            }
        }
        for i in 0..scalars {
            let Some(r) = row(i) else { return Err(ImportError::NoRow { row: u32::MAX }) };
            let sc = &t.scalars;
            match t.scalar_index.probe(hash::wide(sc[i]), |id| sc[ix(id)] == sc[i]) {
                Probe::Found(_) => return Err(ImportError::Duplicate { row: r }),
                Probe::Vacant(v) => {
                    let sc = &t.scalars;
                    t.scalar_index.fill(v, r, |x| hash::wide(sc[ix(x)]));
                }
            }
        }
        if rd.at != bytes.len() {
            return Err(ImportError::Truncated { at: rd.at });
        }
        Ok(t)
    }
}

/// Reads [`PTys::to_bytes`]'s bytes front to back.
struct Reader<'r> {
    bytes: &'r [u8],
    at: usize,
}

impl<'r> Reader<'r> {
    fn take(&mut self, n: usize) -> Result<&'r [u8], ImportError> {
        let end = self.at.checked_add(n).ok_or(ImportError::Truncated { at: self.at })?;
        let s = self.bytes.get(self.at..end).ok_or(ImportError::Truncated { at: self.at })?;
        self.at = end;
        Ok(s)
    }

    fn word(&mut self) -> Result<u32, ImportError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
}
