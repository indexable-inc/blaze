//! Welding: a triangle soup (three positions a triangle, as a mesher or a file gives them) to an
//! indexed mesh, by EXACT positions: two corners are one vertex when their coordinates are equal
//! as numbers (negative zero equal to zero), found by sorting, never by hashing to a tolerance
//! (a tolerance merges what is near and splits what straddles a cell; the kernel's mesher puts
//! shared corners at bit-equal positions, so exact is the right weld for it).
//!
//! Then, in order: a triangle with two corners at one vertex is dropped (counted as degenerate);
//! each triangle is turned to start at its least vertex, keeping its orientation; of triangles
//! equal after that, one is kept (counted as duplicates); with [`Twins::Remove`], each pair of
//! triangles on the same three vertices in opposite orientations is dropped (counted as twins):
//! the faces two solids glued together share, which are inside the union.
//!
//! ORDER-FREE: the output is a function of the soup as a multiset of triangles (up to the corner
//! each starts at): vertices are numbered in position order, triangles sorted by
//! `(vertices, part, role)`. Normals are the smooth area-weighted ones of
//! [`crate::vertex_normals`]; split them with [`crate::shade()`].

#![blaze::sans_io]

use blaze_own::{Alloc, AllocFailure, Vec};

use crate::MeshLimits;
use crate::geom::{P3, plus_zeros, point_order};
use crate::ids::{MaterialRole, PartId, TriId};
use crate::mesh::{Column, Mesh, MeshVecs, ShapeError, built, empty_vecs};
use crate::shade::{ShadeError, vertex_normals};
use crate::vecs::{V, at, counting, filled, len32, push};

/// What to do with pairs of opposite triangles on the same three vertices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Twins {
    Keep,
    Remove,
}

/// A triangle soup: corners counter-clockwise from outside, with each triangle's part and role.
#[derive(Clone, Copy, Debug)]
pub struct Soup<'s> {
    pub corners: &'s [[P3; 3]],
    pub parts: &'s [PartId],
    pub roles: &'s [MaterialRole],
}

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct WeldOpts<'a, 's> {
    pub twins: Twins,
    pub limits: MeshLimits,
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

/// The welded mesh and what was dropped.
#[derive(Debug)]
pub struct Welded<'a> {
    pub mesh: Mesh<'a>,
    pub degenerate: u32,
    pub duplicates: u32,
    /// Pairs dropped (two triangles each).
    pub twins: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum WeldError {
    Length { column: Column, len: usize, want: usize },
    TooManyTriangles { triangles: usize, most: u32 },
    NonFinite { tri: TriId, corner: u8, at: P3 },
    Normals(ShadeError),
    Alloc(AllocFailure),
    /// The mesh built here did not fit together: a bug in `weld`.
    Built(ShapeError),
}

impl From<AllocFailure> for WeldError {
    fn from(e: AllocFailure) -> WeldError {
        WeldError::Alloc(e)
    }
}

pub fn weld<'a>(soup: Soup<'_>, opts: WeldOpts<'a, '_>) -> Result<Welded<'a>, WeldError> {
    let scratch = opts.scratch;
    let t = soup.corners.len();
    if t > blaze_int::idx(opts.limits.most_triangles) {
        return Err(WeldError::TooManyTriangles { triangles: t, most: opts.limits.most_triangles });
    }
    if soup.parts.len() != t {
        return Err(WeldError::Length { column: Column::Parts, len: soup.parts.len(), want: t });
    }
    if soup.roles.len() != t {
        return Err(WeldError::Length { column: Column::Roles, len: soup.roles.len(), want: t });
    }
    for (i, tri) in soup.corners.iter().enumerate() {
        for (k, p) in tri.iter().enumerate() {
            if !p.iter().all(|x| x.is_finite()) {
                let corner = if k == 0 { 0 } else if k == 1 { 1 } else { 2 };
                return Err(WeldError::NonFinite { tri: TriId(len32(i)), corner, at: *p });
            }
        }
    }
    let corner_at = |c: u32| -> P3 { plus_zeros(soup.corners[at(c / 3)][at(c % 3)]) };
    let mut order = counting(len32(t * 3), scratch)?;
    order.sort_unstable_by(|&a, &b| point_order(corner_at(a), corner_at(b)).then(a.cmp(&b)));
    let mut vertex_of = filled(t * 3, 0u32, scratch)?;
    let mut positions: V<'_, P3> = Vec::new_in(scratch);
    for &c in order.iter() {
        let p = corner_at(c);
        if positions.last().is_none_or(|&q| point_order(p, q).is_ne()) {
            push(&mut positions, p)?;
        }
        vertex_of[at(c)] = len32(positions.len()) - 1;
    }
    let mut degenerate = 0u32;
    let mut keyed: V<'_, ([u32; 3], PartId, MaterialRole)> = Vec::try_with_capacity_in(t, scratch)?;
    for i in 0..t {
        let v = [vertex_of[i * 3], vertex_of[i * 3 + 1], vertex_of[i * 3 + 2]];
        if v[0] == v[1] || v[1] == v[2] || v[0] == v[2] {
            degenerate += 1;
            continue;
        }
        push(&mut keyed, (least_first(v), soup.parts[i], soup.roles[i]))?;
    }
    keyed.sort_unstable();
    let before = keyed.len();
    keyed.dedup_by(|b, a| a.0 == b.0);
    let duplicates = len32(before - keyed.len());
    let mut keep = filled(keyed.len(), true, scratch)?;
    let mut twins = 0u32;
    if opts.twins == Twins::Remove {
        let mut sets: V<'_, ([u32; 3], u32)> = Vec::try_with_capacity_in(keyed.len(), scratch)?;
        for (i, k) in keyed.iter().enumerate() {
            let mut s = k.0;
            s.sort_unstable();
            push(&mut sets, (s, len32(i)))?;
        }
        sets.sort_unstable();
        // Duplicates are gone, so a set holds at most its two orientations.
        for pair in sets.windows(2) {
            if pair[0].0 == pair[1].0 {
                keep[at(pair[0].1)] = false;
                keep[at(pair[1].1)] = false;
                twins += 1;
            }
        }
    }
    let mut used = filled(positions.len(), u32::MAX, scratch)?;
    for (i, k) in keyed.iter().enumerate() {
        if keep[i] {
            for &v in &k.0 {
                used[at(v)] = 0;
            }
        }
    }
    let mut out: MeshVecs<'a> = empty_vecs(opts.alloc);
    for (v, slot) in used.iter_mut().enumerate() {
        if *slot == 0 {
            *slot = len32(out.positions.len());
            push(&mut out.positions, positions[v])?;
            push(&mut out.normals, [0.0; 3])?;
        }
    }
    for (i, k) in keyed.iter().enumerate() {
        if keep[i] {
            push(&mut out.triangles, [used[at(k.0[0])], used[at(k.0[1])], used[at(k.0[2])]])?;
            push(&mut out.parts, k.1)?;
            push(&mut out.roles, k.2)?;
        }
    }
    let mesh = built(out).map_err(WeldError::Built)?;
    let normals = vertex_normals(&mesh.view(), opts.alloc).map_err(WeldError::Normals)?;
    let mut vecs = mesh.into_vecs();
    vecs.normals = normals;
    Ok(Welded { mesh: built(vecs).map_err(WeldError::Built)?, degenerate, duplicates, twins })
}

/// The triangle turned to start at its least vertex, keeping its orientation.
#[inline]
pub(crate) fn least_first(v: [u32; 3]) -> [u32; 3] {
    if v[0] < v[1] && v[0] < v[2] {
        v
    } else if v[1] < v[2] {
        [v[1], v[2], v[0]]
    } else {
        [v[2], v[0], v[1]]
    }
}
