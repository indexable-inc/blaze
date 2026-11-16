//! The portable type form: what a world's answer and the body's record hold (typeck.md,
//! "`TypeCtx`: what is asked of blaze-ty"). Items by key, generic parameters by index, children
//! before parents in one node column, lists of children in one side column. Becomes blaze-ty's
//! portable type table; what this crate needs of it is exactly this shape.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Malformed};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{ExprId, FloatTy, IntTy, ItemKey, List, Mutbl, Name, UintTy};
use crate::intern::ListIndex;
use blaze_hash::HashMap;

/// A type of a portable table, by its node's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PTy(pub u32);

/// One node of a portable type. Its children are `PTy`s of the same table with smaller indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PNode {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    /// Elements in the list column; `()` is the empty tuple.
    Tuple(List),
    Ref(Mutbl, PTy),
    Ptr(Mutbl, PTy),
    Array(PTy, u64),
    Slice(PTy),
    /// An ADT and its generic arguments.
    Adt(ItemKey, List),
    /// A fn item (or a ctor) and its generic arguments.
    FnDef(ItemKey, List),
    /// A fn pointer: the inputs, then the output last.
    FnPtr(List),
    /// A closure of this body, by its expression, and its signature (a `FnPtr`).
    Closure(ExprId, PTy),
    /// A generic parameter of the item, by its index in the item's generics (parents first;
    /// a trait's `Self` is index 0).
    Param(u32),
    /// `<args[0] as trait<args[1..]>>::name`.
    Alias { trait_: ItemKey, name: Name, args: List },
    /// An opaque type (`impl Trait`) and its arguments.
    Opaque(ItemKey, List),
    /// A hole (`_`): only in types written in the body.
    Infer,
    /// The error type: a type error already reported.
    Error,
}

/// A borrowed portable table.
#[derive(Clone, Copy, Debug)]
pub struct PTys<'a> {
    pub nodes: &'a [PNode],
    pub lists: &'a [PTy],
}

impl<'a> PTys<'a> {
    /// The empty table.
    pub const EMPTY: PTys<'static> = PTys { nodes: &[], lists: &[] };

    /// The node of `t`.
    pub fn node(&self, t: PTy) -> Result<PNode, CheckError> {
        match self.nodes.get(t.0 as usize) {
            Some(n) => Ok(*n),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: t.0, child: t.0 })),
        }
    }

    /// The children of a list.
    pub fn list(&self, l: List) -> Result<&'a [PTy], CheckError> {
        match self.lists.get(l.range()) {
            Some(s) => Ok(s),
            None => Err(CheckError::Malformed(Malformed::List {
                column: "types.lists",
                start: l.start,
                len: l.len,
            })),
        }
    }
}

impl ContentHash for PTys<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.nodes.len() as u64);
        for n in self.nodes {
            hash_node(n, h);
        }
        h.u64(self.lists.len() as u64);
        for t in self.lists {
            h.u32(t.0);
        }
    }
}

impl ContentHash for PTy {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.0);
    }
}

fn hash_list(l: List, h: &mut StableHasher) {
    h.u32(l.start);
    h.u32(l.len);
}

/// The content of one node, its children by index.
pub(crate) fn hash_node(n: &PNode, h: &mut StableHasher) {
    match *n {
        PNode::Bool => h.tag(0),
        PNode::Char => h.tag(1),
        PNode::Int(i) => {
            h.tag(2);
            h.tag(i as u8);
        }
        PNode::Uint(u) => {
            h.tag(3);
            h.tag(u as u8);
        }
        PNode::Float(f) => {
            h.tag(4);
            h.tag(f as u8);
        }
        PNode::Str => h.tag(5),
        PNode::Never => h.tag(6),
        PNode::Tuple(l) => {
            h.tag(7);
            hash_list(l, h);
        }
        PNode::Ref(m, t) => {
            h.tag(8);
            h.tag(m as u8);
            h.u32(t.0);
        }
        PNode::Ptr(m, t) => {
            h.tag(9);
            h.tag(m as u8);
            h.u32(t.0);
        }
        PNode::Array(t, n) => {
            h.tag(10);
            h.u32(t.0);
            h.u64(n);
        }
        PNode::Slice(t) => {
            h.tag(11);
            h.u32(t.0);
        }
        PNode::Adt(k, l) => {
            h.tag(12);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::FnDef(k, l) => {
            h.tag(13);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::FnPtr(l) => {
            h.tag(14);
            hash_list(l, h);
        }
        PNode::Closure(e, sig) => {
            h.tag(15);
            h.u32(e.0);
            h.u32(sig.0);
        }
        PNode::Param(i) => {
            h.tag(16);
            h.u32(i);
        }
        PNode::Alias { trait_, name, args } => {
            h.tag(17);
            h.u128(trait_.0);
            h.u64(name.0);
            hash_list(args, h);
        }
        PNode::Opaque(k, l) => {
            h.tag(18);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::Infer => h.tag(19),
        PNode::Error => h.tag(20),
    }
}

/// An owned portable table, hash-consed: equal structure is an equal `PTy`, and an equal list
/// is an equal `List`, so the table's bytes depend on structure and the order of first use
/// only (rule 25).
pub struct PortableTys<'a> {
    nodes: Vec<PNode, Alloc<'a>>,
    lists: Vec<PTy, Alloc<'a>>,
    node_index: HashMap<PNode, u32, Alloc<'a>>,
    list_index: ListIndex<'a>,
    /// Per interned list, in the order interned: where it is in `lists`.
    list_rows: Vec<List, Alloc<'a>>,
}

impl<'a> PortableTys<'a> {
    /// An empty table.
    pub fn new_in(alloc: Alloc<'a>) -> PortableTys<'a> {
        PortableTys {
            nodes: Vec::new_in(alloc),
            lists: Vec::new_in(alloc),
            node_index: HashMap::new_in(alloc),
            list_index: ListIndex::new_in(alloc),
            list_rows: Vec::new_in(alloc),
        }
    }

    /// The table, borrowed.
    pub fn view(&self) -> PTys<'_> {
        PTys { nodes: &self.nodes, lists: &self.lists }
    }

    /// The node `n`, interned. Its children must already be in the table.
    pub fn intern(&mut self, n: PNode) -> Result<PTy, CheckError> {
        if let Some(&found) = self.node_index.get(&n) {
            return Ok(PTy(found));
        }
        let at = u32::try_from(self.nodes.len())
            .map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        self.nodes.push(n);
        self.node_index.insert(n, at);
        Ok(PTy(at))
    }

    /// The list `tys`, interned.
    pub fn intern_list(&mut self, tys: &[PTy]) -> Result<List, CheckError> {
        if tys.is_empty() {
            return Ok(List::EMPTY);
        }
        let mut h = StableHasher::new("ptys.list");
        for t in tys {
            h.u32(t.0);
        }
        let hash = h.finish();
        let (lists, rows) = (&self.lists, &self.list_rows);
        if let Some(found) = self.list_index.find(hash, |id| lists.get(rows[id as usize].range()) == Some(tys)) {
            return Ok(self.list_rows[found as usize]);
        }
        let start = u32::try_from(self.lists.len())
            .map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        let len = u32::try_from(tys.len()).map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        self.list_index.push(hash)?;
        self.lists.extend_from_slice(tys);
        let list = List { start, len };
        self.list_rows.push(list);
        Ok(list)
    }

    /// The number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl<'a> PartialEq for PortableTys<'a> {
    fn eq(&self, other: &PortableTys<'a>) -> bool {
        self.nodes == other.nodes && self.lists == other.lists
    }
}

impl<'a> Eq for PortableTys<'a> {}

impl<'a> core::fmt::Debug for PortableTys<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PortableTys")
            .field("nodes", &&self.nodes[..])
            .field("lists", &&self.lists[..])
            .finish()
    }
}
