//! The mesh's content digest: BLAKE3 (key-derivation mode, context `blaze-cad-mesh 1 mesh`) over
//! the mesh's canonical byte form, the key every stage after it is cached under (rule 21: it
//! covers every column).
//!
//! The canonical byte form, column by column in the order `positions, normals, tangents, uvs,
//! colours, triangles, parts, roles, creases`: the column's entry count as a little-endian `u64`,
//! then each entry's values in order, each `f32` as its IEEE bits and each integer as itself,
//! little-endian, `u8`s as bytes. Nothing of the machine is in it: the same mesh gives the same
//! bytes on every host.
//!
//! [`MeshDigest::of_unordered`] is the digest of the mesh as a SET of triangles: each corner
//! stands for all its vertex's attributes, each triangle is turned to start at its least corner
//! (keeping its orientation) and the triangles are sorted, so renumbering vertices and reordering
//! triangles leave it unchanged. Vertices no triangle uses are not in it.

#![blaze::sans_io]

use blaze_blake3::{Hasher, context_key};
use blaze_own::{Alloc, AllocFailure};

use crate::mesh::MeshView;
use crate::vecs::{V, at, push};

/// A mesh's content digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MeshDigest(pub [u8; 32]);

const MESH: &str = "blaze-cad-mesh 1 mesh";
const UNORDERED: &str = "blaze-cad-mesh 1 unordered mesh";
const CORNER: &str = "blaze-cad-mesh 1 corner";
const TRIANGLE: &str = "blaze-cad-mesh 1 triangle";

/// A hasher fed through a fixed buffer, so each value is not its own call.
struct Feed {
    hasher: Hasher,
    buf: [u8; 256],
    len: usize,
}

impl Feed {
    fn new(context: &str) -> Feed {
        Feed { hasher: Hasher::new_derive_key(context), buf: [0; 256], len: 0 }
    }

    fn keyed(key: &[u8; 32]) -> Feed {
        Feed { hasher: Hasher::new_from_context_key(key), buf: [0; 256], len: 0 }
    }

    fn bytes(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > self.buf.len() {
            self.hasher.update(&self.buf[..self.len]);
            self.len = 0;
        }
        if bytes.len() > self.buf.len() {
            self.hasher.update(bytes);
        } else {
            self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
    }

    fn u32(&mut self, x: u32) {
        self.bytes(&x.to_le_bytes());
    }

    fn u64(&mut self, x: u64) {
        self.bytes(&x.to_le_bytes());
    }

    fn f32s(&mut self, xs: &[f32]) {
        for x in xs {
            self.u32(x.to_bits());
        }
    }

    fn finish(mut self) -> [u8; 32] {
        self.hasher.update(&self.buf[..self.len]);
        self.len = 0;
        self.hasher.finalize()
    }
}

fn count(len: usize) -> u64 {
    blaze_int::Int::widen_to::<u64>(crate::vecs::len32(len))
}

impl MeshDigest {
    /// The digest of `view`'s canonical byte form.
    pub fn of(view: &MeshView<'_>) -> MeshDigest {
        let c = view.columns();
        let mut f = Feed::new(MESH);
        f.u64(count(c.positions.len()));
        for p in c.positions {
            f.f32s(p);
        }
        f.u64(count(c.normals.len()));
        for n in c.normals {
            f.f32s(n);
        }
        f.u64(count(c.tangents.len()));
        for t in c.tangents {
            f.f32s(t);
        }
        f.u64(count(c.uvs.len()));
        for uv in c.uvs {
            f.f32s(uv);
        }
        f.u64(count(c.colours.len()));
        for colour in c.colours {
            f.bytes(colour);
        }
        f.u64(count(c.triangles.len()));
        for tri in c.triangles {
            f.u32(tri[0]);
            f.u32(tri[1]);
            f.u32(tri[2]);
        }
        f.u64(count(c.parts.len()));
        for part in c.parts {
            f.u32(part.0);
        }
        f.u64(count(c.roles.len()));
        for role in c.roles {
            f.u32(role.0);
        }
        f.u64(count(c.creases.len()));
        for edge in c.creases {
            f.u32(edge.lo().0);
            f.u32(edge.hi().0);
        }
        MeshDigest(f.finish())
    }

    /// The digest of `view` as a set of triangles (see the module's documentation): equal for two
    /// meshes that differ only in vertex numbering, triangle order and the corner each triangle
    /// starts at.
    pub fn of_unordered(view: &MeshView<'_>, scratch: Alloc<'_>) -> Result<MeshDigest, AllocFailure> {
        let corner_key = context_key(CORNER);
        let tri_key = context_key(TRIANGLE);
        let mut vertex_hash: V<'_, [u8; 32]> = blaze_own::Vec::try_with_capacity_in(view.positions().len(), scratch)?;
        for v in 0..view.positions().len() {
            let mut f = Feed::keyed(&corner_key);
            f.f32s(&view.positions()[v]);
            f.f32s(&view.normals()[v]);
            if let Some(t) = view.tangents() {
                f.f32s(&t[v]);
            }
            if let Some(uv) = view.uvs() {
                f.f32s(&uv[v]);
            }
            if let Some(colour) = view.colours() {
                f.bytes(&colour[v]);
            }
            push(&mut vertex_hash, f.finish())?;
        }
        let mut tris: V<'_, [u8; 32]> = blaze_own::Vec::try_with_capacity_in(view.triangles().len(), scratch)?;
        for (t, tri) in view.triangles().iter().enumerate() {
            let h = [vertex_hash[at(tri[0])], vertex_hash[at(tri[1])], vertex_hash[at(tri[2])]];
            let first = if h[0] <= h[1] && h[0] <= h[2] {
                0
            } else if h[1] <= h[2] {
                1
            } else {
                2
            };
            let mut f = Feed::keyed(&tri_key);
            for k in 0..3 {
                f.bytes(&h[(first + k) % 3]);
            }
            f.u32(view.parts()[t].0);
            f.u32(view.roles()[t].0);
            push(&mut tris, f.finish())?;
        }
        tris.sort_unstable();
        let mut creases: V<'_, [[u8; 32]; 2]> = blaze_own::Vec::try_with_capacity_in(view.creases().len(), scratch)?;
        for edge in view.creases() {
            let (a, b) = (vertex_hash[edge.lo().index()], vertex_hash[edge.hi().index()]);
            push(&mut creases, if a <= b { [a, b] } else { [b, a] })?;
        }
        creases.sort_unstable();
        let mut f = Feed::new(UNORDERED);
        f.u64(count(tris.len()));
        for h in tris.iter() {
            f.bytes(h);
        }
        f.u64(count(creases.len()));
        for pair in creases.iter() {
            f.bytes(&pair[0]);
            f.bytes(&pair[1]);
        }
        Ok(MeshDigest(f.finish()))
    }
}
