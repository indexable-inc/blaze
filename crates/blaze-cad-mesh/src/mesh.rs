//! The mesh in columns: one vector per attribute, vertex columns all as long as `positions`
//! (the optional ones empty when absent), triangle columns all as long as `triangles`. A
//! [`MeshView`] is the borrowed form every algorithm reads; making one checks the shape (lengths,
//! indices in range, creases sorted), so no algorithm indexes out of range and none checks again.
//! Whether the surface is closed, manifold and oriented is the verifier's ([`crate::check()`]).

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::geom::P3;
use crate::ids::{VertexPair, MaterialRole, PartId, TriId, VertexId};
use crate::vecs::copied;

/// A column, as an error names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Positions,
    Normals,
    Tangents,
    Uvs,
    Colours,
    Triangles,
    Parts,
    Roles,
    Creases,
}

/// A mesh whose columns do not fit together.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeError {
    /// More entries than a 32-bit id names (`u32::MAX / 3` triangles, so every corner has one).
    TooMany { column: Column, len: usize, most: usize },
    /// A column whose length is not the one its kind needs (`want`; an optional vertex column
    /// may also be empty).
    Length { column: Column, len: usize, want: usize },
    /// A triangle's corner that names no vertex.
    IndexRange { tri: TriId, corner: u8, index: u32, vertices: u32 },
    /// A crease mark on a vertex the mesh does not have.
    CreaseRange { crease: u32, edge: VertexPair, vertices: u32 },
    /// A crease mark out of order, repeated, or from a vertex to itself: the column is a sorted
    /// set of edges.
    CreaseOrder { crease: u32, edge: VertexPair },
}

/// A mesh's columns, borrowed, as [`MeshView::new`] takes them. An optional vertex column
/// (`tangents`, `uvs`, `colours`) is empty when the mesh has none.
#[derive(Clone, Copy, Debug)]
pub struct Columns<'a> {
    pub positions: &'a [P3],
    /// One unit normal per vertex.
    pub normals: &'a [P3],
    /// `xyz` a unit tangent, `w` the handedness `+1` or `-1` (glTF's `TANGENT`).
    pub tangents: &'a [[f32; 4]],
    pub uvs: &'a [[f32; 2]],
    /// Linear RGBA, 8 bits a channel.
    pub colours: &'a [[u8; 4]],
    /// Counter-clockwise seen from outside.
    pub triangles: &'a [[u32; 3]],
    pub parts: &'a [PartId],
    pub roles: &'a [MaterialRole],
    /// Edges marked sharp by the model: a sorted set, kept by simplification and split by
    /// shading.
    pub creases: &'a [VertexPair],
}

/// A mesh, borrowed, its shape checked.
#[derive(Clone, Copy, Debug)]
pub struct MeshView<'a> {
    columns: Columns<'a>,
}

const MOST_VERTICES: usize = blaze_int::idx(u32::MAX);
const MOST_TRIANGLES: usize = blaze_int::idx(u32::MAX / 3);

impl<'a> MeshView<'a> {
    /// The columns as a view, or the first way they do not fit together.
    pub fn new(columns: Columns<'a>) -> Result<MeshView<'a>, ShapeError> {
        let n = columns.positions.len();
        let t = columns.triangles.len();
        if n > MOST_VERTICES {
            return Err(ShapeError::TooMany { column: Column::Positions, len: n, most: MOST_VERTICES });
        }
        if t > MOST_TRIANGLES {
            return Err(ShapeError::TooMany { column: Column::Triangles, len: t, most: MOST_TRIANGLES });
        }
        let need = |column: Column, len: usize, want: usize, optional: bool| -> Result<(), ShapeError> {
            if len == want || (optional && len == 0) { Ok(()) } else { Err(ShapeError::Length { column, len, want }) }
        };
        need(Column::Normals, columns.normals.len(), n, false)?;
        need(Column::Tangents, columns.tangents.len(), n, true)?;
        need(Column::Uvs, columns.uvs.len(), n, true)?;
        need(Column::Colours, columns.colours.len(), n, true)?;
        need(Column::Parts, columns.parts.len(), t, false)?;
        need(Column::Roles, columns.roles.len(), t, false)?;
        let vertices = crate::vecs::len32(n);
        for (i, tri) in columns.triangles.iter().enumerate() {
            for (corner, &index) in tri.iter().enumerate() {
                if index >= vertices {
                    return Err(ShapeError::IndexRange {
                        tri: TriId(crate::vecs::len32(i)),
                        corner: if corner == 0 { 0 } else if corner == 1 { 1 } else { 2 },
                        index,
                        vertices,
                    });
                }
            }
        }
        if columns.creases.len() > MOST_VERTICES {
            return Err(ShapeError::TooMany { column: Column::Creases, len: columns.creases.len(), most: MOST_VERTICES });
        }
        let mut last: Option<VertexPair> = None;
        for (i, &edge) in columns.creases.iter().enumerate() {
            let crease = crate::vecs::len32(i);
            if edge.hi().0 >= vertices {
                return Err(ShapeError::CreaseRange { crease, edge, vertices });
            }
            if edge.lo() == edge.hi() || last.is_some_and(|before| before >= edge) {
                return Err(ShapeError::CreaseOrder { crease, edge });
            }
            last = Some(edge);
        }
        Ok(MeshView { columns })
    }

    #[inline]
    pub fn columns(&self) -> Columns<'a> {
        self.columns
    }

    #[inline]
    pub fn positions(&self) -> &'a [P3] {
        self.columns.positions
    }

    #[inline]
    pub fn normals(&self) -> &'a [P3] {
        self.columns.normals
    }

    #[inline]
    pub fn tangents(&self) -> Option<&'a [[f32; 4]]> {
        Some(self.columns.tangents).filter(|c| !c.is_empty())
    }

    #[inline]
    pub fn uvs(&self) -> Option<&'a [[f32; 2]]> {
        Some(self.columns.uvs).filter(|c| !c.is_empty())
    }

    #[inline]
    pub fn colours(&self) -> Option<&'a [[u8; 4]]> {
        Some(self.columns.colours).filter(|c| !c.is_empty())
    }

    #[inline]
    pub fn triangles(&self) -> &'a [[u32; 3]] {
        self.columns.triangles
    }

    #[inline]
    pub fn parts(&self) -> &'a [PartId] {
        self.columns.parts
    }

    #[inline]
    pub fn roles(&self) -> &'a [MaterialRole] {
        self.columns.roles
    }

    #[inline]
    pub fn creases(&self) -> &'a [VertexPair] {
        self.columns.creases
    }

    #[inline]
    pub fn vertex_count(&self) -> u32 {
        crate::vecs::len32(self.columns.positions.len())
    }

    #[inline]
    pub fn triangle_count(&self) -> u32 {
        crate::vecs::len32(self.columns.triangles.len())
    }

    /// Triangle `t`'s corners.
    #[inline]
    pub fn tri(&self, t: TriId) -> [VertexId; 3] {
        let tri = self.columns.triangles[t.index()];
        [VertexId(tri[0]), VertexId(tri[1]), VertexId(tri[2])]
    }

    /// Triangle `t`'s corner positions.
    #[inline]
    pub fn corners(&self, t: TriId) -> [P3; 3] {
        let tri = self.columns.triangles[t.index()];
        let p = self.columns.positions;
        [p[crate::vecs::at(tri[0])], p[crate::vecs::at(tri[1])], p[crate::vecs::at(tri[2])]]
    }

    #[inline]
    pub fn position(&self, v: VertexId) -> P3 {
        self.columns.positions[v.index()]
    }
}

/// A mesh's columns, owned, as [`Mesh::new`] takes them and [`Mesh::into_vecs`] gives them back.
#[derive(Debug)]
pub struct MeshVecs<'m> {
    pub positions: Vec<P3, Alloc<'m>>,
    pub normals: Vec<P3, Alloc<'m>>,
    pub tangents: Vec<[f32; 4], Alloc<'m>>,
    pub uvs: Vec<[f32; 2], Alloc<'m>>,
    pub colours: Vec<[u8; 4], Alloc<'m>>,
    pub triangles: Vec<[u32; 3], Alloc<'m>>,
    pub parts: Vec<PartId, Alloc<'m>>,
    pub roles: Vec<MaterialRole, Alloc<'m>>,
    pub creases: Vec<VertexPair, Alloc<'m>>,
}

/// Every column empty, in `alloc`.
pub(crate) fn empty_vecs(alloc: Alloc<'_>) -> MeshVecs<'_> {
    MeshVecs {
        positions: Vec::new_in(alloc),
        normals: Vec::new_in(alloc),
        tangents: Vec::new_in(alloc),
        uvs: Vec::new_in(alloc),
        colours: Vec::new_in(alloc),
        triangles: Vec::new_in(alloc),
        parts: Vec::new_in(alloc),
        roles: Vec::new_in(alloc),
        creases: Vec::new_in(alloc),
    }
}

/// A mesh, owned, its shape checked: the columns in the allocator it was handed.
#[derive(Debug)]
pub struct Mesh<'m> {
    vecs: MeshVecs<'m>,
}

impl<'m> Mesh<'m> {
    /// The columns as a mesh, or the first way they do not fit together.
    pub fn new(vecs: MeshVecs<'m>) -> Result<Mesh<'m>, ShapeError> {
        MeshView::new(columns_of(&vecs))?;
        Ok(Mesh { vecs })
    }

    #[inline]
    pub fn view(&self) -> MeshView<'_> {
        // Checked when the mesh was made, and a mesh's columns change only through `new`.
        MeshView { columns: columns_of(&self.vecs) }
    }

    pub fn into_vecs(self) -> MeshVecs<'m> {
        self.vecs
    }

    /// A copy in `alloc`.
    pub fn try_clone_in<'n>(&self, alloc: Alloc<'n>) -> Result<Mesh<'n>, AllocFailure> {
        let v = &self.vecs;
        Ok(Mesh {
            vecs: MeshVecs {
                positions: copied(&v.positions, alloc)?,
                normals: copied(&v.normals, alloc)?,
                tangents: copied(&v.tangents, alloc)?,
                uvs: copied(&v.uvs, alloc)?,
                colours: copied(&v.colours, alloc)?,
                triangles: copied(&v.triangles, alloc)?,
                parts: copied(&v.parts, alloc)?,
                roles: copied(&v.roles, alloc)?,
                creases: copied(&v.creases, alloc)?,
            },
        })
    }
}

/// A copy of `view`'s columns in `alloc`.
pub(crate) fn vecs_of<'n>(view: &MeshView<'_>, alloc: Alloc<'n>) -> Result<MeshVecs<'n>, AllocFailure> {
    let c = view.columns;
    Ok(MeshVecs {
        positions: copied(c.positions, alloc)?,
        normals: copied(c.normals, alloc)?,
        tangents: copied(c.tangents, alloc)?,
        uvs: copied(c.uvs, alloc)?,
        colours: copied(c.colours, alloc)?,
        triangles: copied(c.triangles, alloc)?,
        parts: copied(c.parts, alloc)?,
        roles: copied(c.roles, alloc)?,
        creases: copied(c.creases, alloc)?,
    })
}

fn columns_of<'v>(v: &'v MeshVecs<'_>) -> Columns<'v> {
    Columns {
        positions: &v.positions,
        normals: &v.normals,
        tangents: &v.tangents,
        uvs: &v.uvs,
        colours: &v.colours,
        triangles: &v.triangles,
        parts: &v.parts,
        roles: &v.roles,
        creases: &v.creases,
    }
}

/// A mesh this crate built: a shape error there is a bug in the algorithm that built it, named
/// as such by the caller's error.
pub(crate) fn built(vecs: MeshVecs<'_>) -> Result<Mesh<'_>, ShapeError> {
    Mesh::new(vecs)
}
