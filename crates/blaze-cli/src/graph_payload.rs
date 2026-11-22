//! The `UnitGraph` record's payload: the unit graph record, then the crate list (blaze-world
//! crate_list.rs: one row a crate of the world, sorted by key, read in place), then a trailer
//! (the graph's length, `u64` little endian, and [`MAGIC`]). The world is made ONCE, by the
//! `UnitGraph` task that already makes it to name the `CrateScope` parts (work.rs); every other
//! reader of the crates opens the list (D114 rule 1, the coordinator's ruling 2026-11-16
//! 21:1x), and a reader of the units takes [`Sections::graph`].

use blaze_own::{Alloc, Vec};
use blaze_world::crate_list::{CrateList, ListError};

/// The trailer's mark: a payload without it is of another form (an older binary's, which its
/// producer id already keeps apart: this is a check of the form, not a migration).
pub(crate) const MAGIC: &[u8; 4] = b"BZGP";
const TRAILER: usize = 12;

/// A `UnitGraph` payload's two parts, borrowed from its bytes.
#[derive(Clone, Copy)]
pub(crate) struct Sections<'b> {
    /// The unit graph record (blaze-unit-graph `record::decode`'s input).
    pub(crate) graph: &'b [u8],
    /// The crate list's bytes; empty when the world could not be made (the record's finding
    /// says why), so no crate has a row.
    pub(crate) list: &'b [u8],
}

/// Why a payload is not of this form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// No trailer: what was wrong, by name.
    Form(&'static str),
    /// The list's own check refused it.
    List(ListError),
}

impl core::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PayloadError::Form(what) => write!(f, "a `UnitGraph` payload not of this form (graph_payload.rs): {what}"),
            PayloadError::List(error) => write!(f, "the `UnitGraph` payload's crate list (graph_payload.rs): {error}"),
        }
    }
}

/// `graph` then `list` then the trailer, into `alloc` (the producer seals its record in place:
/// [`seal`]).
#[cfg(test)]
pub(crate) fn join<'a>(graph: &[u8], list: &[u8], alloc: Alloc<'a>) -> Vec<u8, Alloc<'a>> {
    let mut out = Vec::with_capacity_in(graph.len() + list.len() + TRAILER, alloc);
    out.extend_from_slice(graph);
    seal(&mut out, list);
    out
}

/// The domain of a `UnitGraph` record's out: the digest of its whole payload, so a reader of
/// the crate list is moved when a row moves, not only when the graph does (rule 21).
const OUT_DOMAIN: &str = "blaze-cli unit graph payload out 1";

/// Appends `list` and the trailer to `payload`, which holds the graph record alone: [`join`]
/// in place, in the record's own allocator. Returns the record's out ([`OUT_DOMAIN`]).
pub(crate) fn seal<A: core::alloc::Allocator>(payload: &mut Vec<u8, A>, list: &[u8]) -> blaze_ids::ContentId {
    let graph = payload.len();
    payload.extend_from_slice(list);
    payload.extend_from_slice(&(graph as u64).to_le_bytes());
    payload.extend_from_slice(MAGIC);
    blaze_ids::ContentId::of(OUT_DOMAIN, payload.as_slice())
}

/// The two parts of `payload`.
pub(crate) fn sections(payload: &[u8]) -> Result<Sections<'_>, PayloadError> {
    let Some(at) = payload.len().checked_sub(TRAILER) else { return Err(PayloadError::Form("shorter than its trailer")) };
    if &payload[at + 8..] != MAGIC {
        return Err(PayloadError::Form("no trailer mark"));
    }
    let mut n = [0; 8];
    n.copy_from_slice(&payload[at..at + 8]);
    let graph = usize::try_from(u64::from_le_bytes(n)).map_err(|_| PayloadError::Form("a graph length past the address space"))?;
    if graph > at {
        return Err(PayloadError::Form("a graph length past the payload"));
    }
    Ok(Sections { graph: &payload[..graph], list: &payload[graph..at] })
}

impl<'b> Sections<'b> {
    /// The crate list, checked whole once; `None` when the payload holds none.
    pub(crate) fn crates(&self) -> Result<Option<CrateList<'b>>, PayloadError> {
        if self.list.is_empty() {
            return Ok(None);
        }
        CrateList::open(self.list).map(Some).map_err(PayloadError::List)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two parts read back as written, and a payload of the older form (no trailer) is
    /// refused by name, never read as a graph with a list. Prints `graph payload: <g> + <l>
    /// bytes read back; the older form refused: <error>`.
    #[test]
    fn a_payload_reads_back_its_graph_and_list() {
        let (graph, list) = (b"graph bytes".as_slice(), b"list".as_slice());
        // The test's one frame, made at its top: its memory is the OS's, not the global heap.
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let joined = join(graph, list, erased.handle());
        let read = sections(&joined).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((read.graph, read.list), (graph, list));
        let older = sections(graph).err().expect("the older form is refused");
        std::println!("graph payload: {} + {} bytes read back; the older form refused: {older}", graph.len(), list.len());
    }
}
