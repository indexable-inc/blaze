//! The startup image (store-next.md, item d; Relax's image record, bottom.md 15): for one
//! program, the code records `blaze run` placed before `main` was reached, in the order it
//! placed them, so the next run of the same program reads them in one pass, in segment and
//! offset order, before the child asks for any. It is a hint and never a proof: each code
//! record it names is found by its own key as any is, and a key the store no longer holds is
//! asked for as before. A wrong image costs reads, never a wrong program.
//!
//! Keyed by the program's manifest (the key of the record that lists the program's items and
//! their code records, run.md): [`StartupImage::key_for`]. Stored under
//! [`Namespace::LOCAL`] with blaze-keyed's `ReservedTag::Image`, as any record; a new manifest is a new key, so an
//! edit never reads the image of another program. Bytes, little-endian, deterministic (the
//! order is the placement order, which the program's own run fixes):
//!
//! - 0..4 `BLSI`, 4..8 format (1), 8..40 the manifest's key, 40..44 rows `n`, 44..48 zero;
//! - `n` rows of 40: 0..4 the slot, 4..8 zero, 8..40 the code record's key.
//!
//! What `blaze-run`'s `Parent` does with it (the driver's side, not built here): at start, get
//! [`StartupImage::key_for`] of the manifest; when found, [`StartupImage::read_order`] the rows
//! against the store's [`super::Plain::locate`] and read each placed record into the child's
//! code region before the child's first ask, the slot table's rows filled from the image's
//! slots (the Welcome's "known slot → record" list, run.md); after `main` is reached, put the
//! image of what this run placed when it differs from the one read, committed with the run's
//! other records after the answer.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Allocator;

use blaze_ids::ContentId;
use super::core::Placed;
use crate::Key;

const MAGIC: [u8; 4] = *b"BLSI";
const FORMAT: u32 = 1;
const HEADER: usize = 48;
const ROW: usize = 40;

/// One placement: the child's slot and the code record placed in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub slot: u32,
    pub code: ContentId,
}

/// The code records one run placed before `main`, in order, for the program `manifest` names.
pub struct StartupImage<A: Allocator> {
    pub manifest: ContentId,
    pub placed: Vec<Placement, A>,
}

// By hand: a derive would ask the allocator for `PartialEq` and `Debug`, which a handle lacks.
impl<A: Allocator> PartialEq for StartupImage<A> {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest && self.placed[..] == other.placed[..]
    }
}

impl<A: Allocator> std::fmt::Debug for StartupImage<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartupImage").field("manifest", &self.manifest).field("placed", &&self.placed[..]).finish()
    }
}

/// Why bytes are not a startup image: what is wrong and the byte it is at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// Shorter than its header or than the rows it says it holds.
    Short { len: u64, wanted: u64 },
    /// Another magic: not a startup image.
    Magic,
    /// Another format of it: refused naming both (rule 26: the image is rebuilt by the next
    /// run, never migrated).
    Format { found: u32, wanted: u32 },
    /// A word a writer keeps zero is not, at this byte.
    NotZero { at: u64 },
    /// It is the image of another program than the one asked.
    Manifest { found: ContentId, wanted: ContentId },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Short { len, wanted } => write!(f, "a startup image of {len} bytes, which needs {wanted}"),
            ImageError::Magic => write!(f, "a record under the startup image's tag that does not start with `BLSI`"),
            ImageError::Format { found, wanted } => write!(f, "a startup image of format {found}, not {wanted}: the next run writes it again"),
            ImageError::NotZero { at } => write!(f, "a startup image whose zero word at byte {at} is not zero"),
            ImageError::Manifest { found, wanted } => write!(f, "the startup image under the key of manifest {wanted:x} names manifest {found:x}"),
        }
    }
}

impl<A: Allocator> StartupImage<A> {
    /// The key of the image of the program `manifest` names.
    pub fn key_for(manifest: &ContentId) -> Key {
        Key(ContentId::of("blaze startup image", &manifest.0))
    }

    /// Its bytes, into `out` (cleared first): the caller's buffer, in its allocator.
    pub fn encode_into<B: Allocator>(&self, out: &mut Vec<u8, B>) {
        out.clear();
        out.reserve(HEADER + ROW * self.placed.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&self.manifest.0);
        out.extend_from_slice(&(self.placed.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        for it in &self.placed {
            out.extend_from_slice(&it.slot.to_le_bytes());
            out.extend_from_slice(&[0u8; 4]);
            out.extend_from_slice(&it.code.0);
        }
    }

    /// The image `bytes` hold for the program `manifest` names, its rows in `alloc`.
    pub fn decode_in(bytes: &[u8], manifest: &ContentId, alloc: A) -> Result<StartupImage<A>, ImageError> {
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        if bytes.len() < HEADER {
            return Err(ImageError::Short { len: bytes.len() as u64, wanted: HEADER as u64 });
        }
        if bytes[0..4] != MAGIC {
            return Err(ImageError::Magic);
        }
        if u32_at(4) != FORMAT {
            return Err(ImageError::Format { found: u32_at(4), wanted: FORMAT });
        }
        let found = ContentId(bytes[8..40].try_into().expect("32 bytes"));
        if found != *manifest {
            return Err(ImageError::Manifest { found, wanted: *manifest });
        }
        if u32_at(44) != 0 {
            return Err(ImageError::NotZero { at: 44 });
        }
        let n = u32_at(40) as usize;
        let wanted = HEADER as u64 + ROW as u64 * n as u64;
        if bytes.len() as u64 != wanted {
            return Err(ImageError::Short { len: bytes.len() as u64, wanted });
        }
        let mut placed = Vec::with_capacity_in(n, alloc);
        for i in 0..n {
            let at = HEADER + ROW * i;
            if u32_at(at + 4) != 0 {
                return Err(ImageError::NotZero { at: (at + 4) as u64 });
            }
            placed.push(Placement { slot: u32_at(at), code: ContentId(bytes[at + 8..at + 40].try_into().expect("32 bytes")) });
        }
        Ok(StartupImage { manifest: *manifest, placed })
    }

    /// The order to read the image's records in: each row the store places (`locate`, as
    /// [`super::Plain::locate`]), by segment and then by offset, so a cold read is one pass
    /// over each segment's pages, ascending; the row's index into [`StartupImage::placed`]
    /// beside each. Rows the store does not hold are left out: the child asks for them as it
    /// would with no image. Into `out`, cleared first (the caller's frame).
    pub fn read_order<B: Allocator>(&self, mut locate: impl FnMut(&Key) -> Option<Placed>, out: &mut Vec<(u32, Placed), B>) {
        out.clear();
        for (i, it) in self.placed.iter().enumerate() {
            if let Some(placed) = locate(&Key(it.code)) {
                out.push((i as u32, placed));
            }
        }
        // Equal places (a record placed in two slots) keep the image's order: the row's index
        // breaks the tie, so the unstable sort (no buffer of its own) is deterministic.
        out.sort_unstable_by(|(i, a), (j, b)| a.segment.0.cmp(&b.segment.0).then(a.at.cmp(&b.at)).then(i.cmp(j)));
    }
}
