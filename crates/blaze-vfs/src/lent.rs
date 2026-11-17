//! Bytes lent from a mapped file with no copy: a range of a [`Mapping`] as [`Bytes`] that hold a
//! count of it. What a shipped tree's texts are mounted from (blaze-cargo's
//! `toolchain::library::texts`): the tree then lends its files' bytes from file pages, not the
//! heap, as it does a settled image's ([`crate::Vfs::settle`]).

use std::{ops::Range, sync::Arc};

use blaze_core::Mapping;

use crate::Bytes;

/// `range` of `mapping`'s bytes, one count of it and no copy; `None` when it is not inside them.
pub fn lent(mapping: &Arc<Mapping>, range: Range<usize>) -> Option<Bytes> {
    let bytes: *const [u8] = mapping.bytes().get(range)?;
    // clone: a count of the mapping, which the bytes hold while they lend it
    let owner: Arc<Mapping> = Arc::clone(mapping);
    // SAFETY: the bytes are a range of `mapping`'s, which stay at one address, unwritten, for as
    // long as any clone of it lives: a file mapped read-only that is only ever replaced by
    // renaming another over it (`Effect::Map`'s contract, which `Mapping::map_file` takes), or
    // bytes it copied and never writes. The `Bytes` hold a count of `mapping` for as long as they
    // lend them.
    Some(unsafe { Bytes::held(bytes, owner) })
}
