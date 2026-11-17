//! One link step of a kept program: what the driver's `Front::link` runs for each request
//! that changed a program's rows. The image's kept state (its record, [`Image::keep`]) is
//! verified and opened, or a new image made when none is kept; the change is staged and made
//! the image; the state is kept again. No image lives between requests: the record does.
//!
//! **Whose part is whose.** The change is built by the caller, which reads the front end's
//! records (blaze-command's `Front::link`):
//! - a row that came or moved (`now`) is a function in `added`, made by the same
//!   `code::link_rows` a whole link uses, given only those rows (their code records and plans,
//!   and their callees' plans for the places);
//! - a row that went or moved (`was`) is its codegen key in `removed` (a moved row is both:
//!   the old code out, the new in under the same key, as a patch of an edited body is);
//! - a static likewise in `statics` and `removed_statics`, its names in `names` and
//!   `unnamed`, and the span tables of its items in `spans`.
//!
//! Nothing here reads a key of the front end's or the driver's: the linker's change is in its
//! own terms, so the link has no dependency on either.
//!
//! **What it gives of the file: a patch, never the file.** The program's bytes are a store
//! file the caller makes by cloning the last state's file and applying [`Stepped::patch`]
//! ([`Patch::edits`], at offsets, sans-IO here); a first build's patch is the whole file
//! ([`crate::PatchBase::Empty`]). The patch is what the commit knows it wrote
//! ([`Image::patch`]): the pieces placed or moved, their jump-table cells, and the blocks of
//! the writer's tables, parts and headers that differ from the last state's. No two files are
//! diffed.
//!
//! **What it reads of the previous image: only the kept state.** Every byte of the last file
//! is in it: the regions' bytes (opened in place from the mapping), the writer's parts and the
//! debug sections ([`crate::file`]'s `FileBytes`, which the kept digest is computed from), so
//! the commit compares each table it made against the last state's bytes and never reads the
//! previous file. The patch's [`Patch::base`] is that file's digest, kept with the state: the
//! caller may check the clone it patches against it.
//!
//! **The floor, and the cost against it.** Bytes written: in proportion to the change (a body
//! edit's code, cell, and a few blocks of tables), and no more for a larger image. Work: the
//! open and the keep are one pass each over the state's sections (`keep.rs`), `O(state)`
//! against a floor of the change; and as the writer's tables but `.rela.dyn` and the ELF
//! symbol table (whose slots and places are history, kept with the state) are not kept, the
//! first stage after each open makes them whole and compares them block by block with the last
//! state's (`O(tables)` read, only changed blocks written); a Mach-O image's page hashes are
//! not kept either, so each step hashes every page it signs (`O(image)`).

use std::alloc::Allocator;
use std::fmt;

use blaze_core::{FileName, Mapping};

use crate::{Change, Image, KeepError, LinkError, Output, Patch, verify};

/// What a step gives: the patch that makes this step's file from the last one's, and the
/// image's state kept.
pub struct Stepped<A: Allocator> {
    pub patch: Patch,
    pub kept: Vec<u8, A>,
}

/// Why a step stopped: the kept state did not open (never relinked whole instead), or the
/// change was refused.
#[derive(Debug)]
pub enum StepError {
    Keep(KeepError),
    Link(LinkError),
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepError::Keep(error) => write!(f, "{error}"),
            StepError::Link(error) => write!(f, "{error}"),
        }
    }
}

/// Links `change` into the image `program` keeps in `kept` (`None`: its first link), written
/// to `to` with `output`, and keeps its state in `alloc`. An image opened from `kept` is laid
/// out, so its patch is never full: only a first link's is.
pub fn step<A: Allocator>(
    program: &str,
    to: FileName,
    output: Output,
    kept: Option<Mapping>,
    change: Change<'_>,
    alloc: A,
) -> Result<Stepped<A>, StepError> {
    let mut image = match kept {
        Some(bytes) => {
            let verified = verify(program, bytes).map_err(StepError::Keep)?;
            Image::open(to, output, &verified).map_err(StepError::Keep)?
        }
        None => Image::new(to, output).map_err(StepError::Link)?,
    };
    let staged = image.stage(change).map_err(StepError::Link)?;
    let patch = image.patch(staged).map_err(StepError::Link)?;
    let kept = image.keep(alloc).map_err(StepError::Keep)?;
    Ok(Stepped { patch, kept })
}
