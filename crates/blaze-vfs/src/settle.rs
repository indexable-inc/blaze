//! A settle: once an image the VFS wrote is durable and mapped, the live tree reads every node
//! that image holds from the mapping and lets its own copy go ([`Vfs::settle`]). File texts
//! are then the content store's, paged in when read (the image names each by its ID and holds
//! none); only what changed since the image is the tree's own.
//!
//! The image is opened as a restart opens it ([`crate::image::open`]: its spine and its tree
//! section checked, no text read), and a generation of that base plus the live nodes
//! the image lacks (found by content ID) is published as a collection publishes one: readers
//! move to it at once, and the old table, with the heap bytes only it held, is freed once no
//! reader is in it ([`crate::store::Writer::replace`]). Nodes take the image's numbers, so the
//! heads, the open views and the journal's sets are renumbered as [`Vfs::collect`] renumbers
//! them. No unsafe code is added: the base is borrowed from the image the next table holds, as
//! a restore's is.

use std::fmt;

use blaze_ids::ContentId;
use blaze_image::Corrupt;

use crate::{
    ImageUse, Vfs,
    image::{self, ImageFile, Opened},
    tree::{NodeId, Onto},
};

/// What [`Vfs::settle`] did: the image's node count, and the bytes of the files whose own copy
/// the tree let go for the image's (freed once no reader and no other holder of them is left).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settled {
    pub nodes: u64,
    pub freed: u64,
}

/// Why [`Vfs::settle`] did not settle; the tree is then as it was.
#[derive(Debug)]
pub enum Unsettled {
    /// The image `image` is not one the journal holds durable (its base, the image a rewrite
    /// in flight moves it to, or the next it starts from): only staged, of another tree, or
    /// of an older history. `base` is the image the journal starts from.
    NotDurable { image: ContentId, base: Option<ContentId> },
    /// The image could not be opened as a VFS tree this tree wrote.
    Image(ImageUse),
    /// A file the image lacks, which the tree held from an earlier image, failed its check.
    Corrupt(Corrupt),
}

impl fmt::Display for Unsettled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unsettled::NotDurable { image, base } => {
                write!(f, "the VFS settles only on a durable image it wrote, and image {image:x}")?;
                f.write_str(" is not one (the journal starts from ")?;
                match base {
                    Some(base) => write!(f, "image {base:x})"),
                    None => f.write_str("no image)"),
                }
            }
            Unsettled::Image(why) => write!(f, "the VFS could not settle on the image: {why:?}"),
            Unsettled::Corrupt(corrupt) => {
                let lacks = "a file it lacks is corrupt";
                write!(f, "the VFS could not settle on the image, {lacks}: {corrupt}")
            }
        }
    }
}

impl std::error::Error for Unsettled {}

/// A planted bug, for the settle tests' controls. Never set outside a test.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The heads and open views keep their old numbers (step 4 skipped).
    KeepsItsNumbers,
    /// Nothing is settled and `Ok` is answered (steps 3 to 5 skipped).
    FreesNothing,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PLANT: std::cell::Cell<Plant> = const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted() -> Plant {
    PLANT.with(std::cell::Cell::get)
}

impl Vfs {
    /// Settles the tree on `image`, the file of an image this VFS wrote ([`Vfs::write_image`])
    /// and was told is durable ([`Vfs::image_durable`]), as mapped, with the store it put the
    /// image's texts in: every node it holds is read from it (its texts from the store) from
    /// now on, and the tree's own copy of each is let go. Heads, views and every root keep
    /// their content; readers pinned before read on as they did. Refused, with the tree
    /// unchanged, for an image the journal does not hold durable.
    pub fn settle(&mut self, image: ImageFile) -> Result<Settled, Unsettled> {
        let Opened { tree: based, name, nodes, .. } = image::open(image).map_err(Unsettled::Image)?;
        if self.journal.durable_image(&name) != Some(nodes) {
            return Err(Unsettled::NotDurable { image: name, base: self.journal.based_on() });
        }
        #[cfg(test)]
        let (frees_nothing, keeps) =
            (planted() == Plant::FreesNothing, planted() == Plant::KeepsItsNumbers);
        #[cfg(not(test))]
        let (frees_nothing, keeps) = (false, false);
        if frees_nothing {
            return Ok(Settled { nodes, freed: 0 });
        }
        let Onto { renumbered, freed } = self.tree.onto(&based)?;
        // The opened tree's own table goes; the next generation holds the image.
        drop(based);
        let renumber = |node: &mut NodeId| {
            *node = renumbered[node.0 as usize].expect("a settle keeps every node");
        };
        if !keeps {
            self.heads.values_mut().for_each(renumber);
            self.views.iter_mut().flatten().for_each(renumber);
        }
        self.journal.onto_image(&name, &renumbered, self.tree.len());
        Ok(Settled { nodes, freed })
    }
}

#[cfg(test)]
mod tests;
