//! Where images are kept: the store, under their key, as a large output, so its
//! content-defined chunks are shared between images that are nearly the same (a dependency
//! bumped, one instance more). The store checks a value's content hash before it hands it
//! out; [`crate::image::View::parse`] checks the rest.

use blaze_cas::{Key, Kind, Store};

use crate::PremapError;
use crate::key::ImageKey;

/// A place images are kept by key.
pub trait Shelf {
    fn get_image(&self, key: &ImageKey) -> Result<Option<&[u8]>, PremapError>;
    fn put_image(&mut self, key: &ImageKey, bytes: Vec<u8>) -> Result<(), PremapError>;
}

/// The store's key of an image: the image key is the hash of exactly what the image was
/// made from, under its own domain.
pub fn store_key(key: &ImageKey) -> Key {
    Key(key.0)
}

impl Shelf for Store {
    fn get_image(&self, key: &ImageKey) -> Result<Option<&[u8]>, PremapError> {
        let entry = self.get(&store_key(key)).map_err(|it| PremapError::of(format_args!("{it}")))?;
        match entry {
            None => Ok(None),
            Some(entry) if entry.kind != Kind::Output => Err(PremapError::of(format_args!(
                "the store keeps a {} under the dependency image {:?}",
                entry.kind.name(),
                key.0
            ))),
            Some(entry) => Ok(Some(entry.bytes)),
        }
    }

    fn put_image(&mut self, key: &ImageKey, bytes: Vec<u8>) -> Result<(), PremapError> {
        self.put(store_key(key), Kind::Output, bytes).map(|_| ()).map_err(|it| {
            PremapError::of(format_args!(
                "the store keeps other bytes under the dependency image {:?} ({:?}, given {:?}): its key \
                 misses an input",
                key.0, it.kept, it.given
            ))
        })
    }
}
