//! A plain-text summary: one line per part, in scene order, fields separated by a tab:
//!
//! ```text
//! "name"  parent="name"|-  triangles=N  vertices=N  roles=a,b  mass=x|-  lods=N
//! ```
//!
//! Names are quoted and escaped as Rust strings are (blaze-text's `escaped`), so a tab or a
//! newline in a name cannot break the line; a role past the scene's materials is written `#id`;
//! the mass is the shortest form that reads back as the same `f64`; every line ends in `\n`.
//! Written with blaze-text, never `core::fmt`.

#![blaze::sans_io]

use blaze_int::idx;
use blaze_own::planted::{NoPlant, Planted};
use blaze_own::{Alloc, AllocFailure, Vec};
use blaze_text::Writes;

use crate::plant::SummaryPlant;
use crate::scene::Scene;
use crate::sink::Bytes;

#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct SummaryOpts<'a, 's> {
    pub alloc: Alloc<'a>,
    pub scratch: Alloc<'s>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SummaryError {
    /// A parent that is not an earlier part.
    ParentOrder { part: usize, parent: u32 },
    Alloc(AllocFailure),
}

impl From<AllocFailure> for SummaryError {
    fn from(e: AllocFailure) -> SummaryError {
        SummaryError::Alloc(e)
    }
}

/// The summary as UTF-8 text in `opts.alloc`.
pub fn summary<'a>(scene: &Scene<'_>, opts: SummaryOpts<'a, '_>) -> Result<Vec<u8, Alloc<'a>>, SummaryError> {
    summary_planted(scene, opts, NoPlant)
}

/// [`summary`] with a planted bug, for the tests' controls.
pub fn summary_planted<'a>(
    scene: &Scene<'_>,
    opts: SummaryOpts<'a, '_>,
    plant: impl Planted<SummaryPlant>,
) -> Result<Vec<u8, Alloc<'a>>, SummaryError> {
    let mut out = Bytes::new(opts.alloc);
    let mut roles: Vec<u32, Alloc<'_>> = Vec::new_in(opts.scratch);
    for (i, part) in scene.parts.iter().enumerate() {
        if let Some(p) = part.parent
            && idx(p) >= i
        {
            return Err(SummaryError::ParentOrder { part: i, parent: p });
        }
        if part.mesh.is_none() && plant.has(SummaryPlant::SkipGroups) {
            continue;
        }
        out.escaped(part.name);
        out.str("\tparent=");
        match part.parent {
            Some(p) => {
                out.escaped(scene.parts[idx(p)].name);
            }
            None => {
                out.str("-");
            }
        }
        let (triangles, vertices) = match &part.mesh {
            Some(view) => (view.triangle_count(), view.vertex_count()),
            None => (0, 0),
        };
        out.str("\ttriangles=").int(triangles).str("\tvertices=").int(vertices).str("\troles=");
        roles.clear();
        if let Some(view) = &part.mesh {
            for role in view.roles() {
                roles.try_push(role.0).map_err(|r| r.why)?;
            }
        }
        roles.sort_unstable();
        roles.dedup();
        for (k, &role) in roles.iter().enumerate() {
            if k > 0 {
                out.str(",");
            }
            match scene.materials.get(idx(role)) {
                Some(m) => {
                    out.str(m.name);
                }
                None => {
                    out.str("#").int(role);
                }
            }
        }
        out.str("\tmass=");
        match part.mass {
            Some(m) => {
                out.float(m.mass);
            }
            None => {
                out.str("-");
            }
        }
        out.str("\tlods=").int(part.lods.len()).str("\n");
    }
    out.finish().map_err(SummaryError::Alloc)
}
