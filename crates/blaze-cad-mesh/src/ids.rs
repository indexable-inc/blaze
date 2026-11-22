//! Typed ids (rule 76): a vertex, a triangle, a part and a material role are never bare
//! integers outside a column's storage.

#![blaze::sans_io]

/// A vertex: an index into a mesh's vertex columns.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VertexId(pub u32);

/// A triangle: an index into a mesh's triangle columns.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TriId(pub u32);

/// The part a triangle belongs to (one node of an export, one rigid body of a game).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PartId(pub u32);

/// What a triangle is made of, as the CAD model names it (`steel`, `paint.hull`, ...): the
/// number is the caller's index into its own table of role names, which the exporter is handed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MaterialRole(pub u32);

impl VertexId {
    #[inline]
    pub fn index(self) -> usize {
        blaze_int::idx(self.0)
    }
}

impl TriId {
    #[inline]
    pub fn index(self) -> usize {
        blaze_int::idx(self.0)
    }
}

/// An undirected mesh edge between two vertices, held with the lesser id first, so each one has
/// one form and a sorted list of them is a set. Named `VertexPair`, not `Edge`: blaze-cad-field
/// owns `Edge` (how a boolean's edge is finished: sharp, round, chamfer), one name per concept.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VertexPair {
    lo: VertexId,
    hi: VertexId,
}

impl VertexPair {
    /// The pair of `a` and `b`, in either order.
    #[inline]
    pub fn new(a: VertexId, b: VertexId) -> VertexPair {
        if a <= b { VertexPair { lo: a, hi: b } } else { VertexPair { lo: b, hi: a } }
    }

    #[inline]
    pub fn lo(self) -> VertexId {
        self.lo
    }

    #[inline]
    pub fn hi(self) -> VertexId {
        self.hi
    }
}
