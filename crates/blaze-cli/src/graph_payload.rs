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
/// `BZG2`: adds the optional resolved `wasm64-blaze-wasip1` host-unit graph section after `list`
/// (`graph | list | wasm_graph | graph_len(u64) | list_len(u64) | BZG2`).
pub(crate) const MAGIC: &[u8; 4] = b"BZG2";
const TRAILER: usize = 20;

/// A `UnitGraph` payload's three parts, borrowed from its bytes.
#[derive(Clone, Copy)]
pub(crate) struct Sections<'b> {
    /// The unit graph record (blaze-unit-graph `record::decode`'s input).
    pub(crate) graph: &'b [u8],
    /// The crate list's bytes; empty when the world could not be made (the record's finding
    /// says why), so no crate has a row.
    pub(crate) list: &'b [u8],
    /// The resolved `wasm64-blaze-wasip1` host-unit graph record (`record::decode`'s input);
    /// empty when the graph has no build scripts / proc macros or no wasm64 host description.
    pub(crate) wasm_graph: &'b [u8],
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

/// `graph` then `list` then `wasm_graph` then the trailer, into `alloc` (the producer seals its
/// record in place: [`seal`]).
#[cfg(test)]
pub(crate) fn join<'a>(graph: &[u8], list: &[u8], wasm_graph: &[u8], alloc: Alloc<'a>) -> Vec<u8, Alloc<'a>> {
    let mut out = Vec::with_capacity_in(graph.len() + list.len() + wasm_graph.len() + TRAILER, alloc);
    out.extend_from_slice(graph);
    seal(&mut out, list, wasm_graph);
    out
}

/// The domain of a `UnitGraph` record's out: the digest of its whole payload, so a reader of
/// the crate list or wasm64 host graph is moved when any section moves, not only when the host
/// graph does (rule 21).
const OUT_DOMAIN: &str = "blaze-cli unit graph payload out 2";

/// Appends `list`, `wasm_graph`, and the trailer to `payload`, which holds the graph record
/// alone: [`join`] in place, in the record's own allocator. Returns the record's out
/// ([`OUT_DOMAIN`]).
pub(crate) fn seal<A: core::alloc::Allocator>(payload: &mut Vec<u8, A>, list: &[u8], wasm_graph: &[u8]) -> blaze_ids::ContentId {
    let graph = payload.len();
    let list_len = list.len();
    payload.extend_from_slice(list);
    payload.extend_from_slice(wasm_graph);
    payload.extend_from_slice(&(u64::try_from(graph).unwrap_or(u64::MAX)).to_le_bytes());
    payload.extend_from_slice(&(u64::try_from(list_len).unwrap_or(u64::MAX)).to_le_bytes());
    payload.extend_from_slice(MAGIC);
    blaze_ids::ContentId::of(OUT_DOMAIN, payload.as_slice())
}

/// The three parts of `payload`.
pub(crate) fn sections(payload: &[u8]) -> Result<Sections<'_>, PayloadError> {
    let Some(at) = payload.len().checked_sub(TRAILER) else { return Err(PayloadError::Form("shorter than its trailer")) };
    if payload.get(at + 16..) != Some(MAGIC.as_slice()) {
        return Err(PayloadError::Form("no trailer mark"));
    }
    let mut g = [0; 8];
    let mut l = [0; 8];
    let (Some(g_bytes), Some(l_bytes)) = (payload.get(at..at + 8), payload.get(at + 8..at + 16)) else {
        return Err(PayloadError::Form("shorter than its trailer"));
    };
    g.copy_from_slice(g_bytes);
    l.copy_from_slice(l_bytes);
    let graph = usize::try_from(u64::from_le_bytes(g)).map_err(|_| PayloadError::Form("a graph length past the address space"))?;
    let list_len = usize::try_from(u64::from_le_bytes(l)).map_err(|_| PayloadError::Form("a list length past the address space"))?;
    let Some(list_end) = graph.checked_add(list_len) else {
        return Err(PayloadError::Form("graph + list length overflow"));
    };
    if list_end > at {
        return Err(PayloadError::Form("a graph/list length past the payload"));
    }
    let (Some(graph_slice), Some(list_slice), Some(wasm_slice)) = (payload.get(..graph), payload.get(graph..list_end), payload.get(list_end..at)) else {
        return Err(PayloadError::Form("a graph/list length past the payload"));
    };
    Ok(Sections { graph: graph_slice, list: list_slice, wasm_graph: wasm_slice })
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
        let (graph, list, wasm) = (b"graph bytes".as_slice(), b"list".as_slice(), b"wasm64 graph".as_slice());
        // The test's one frame, made at its top: its memory is the OS's, not the global heap.
        let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let joined = join(graph, list, wasm, erased.handle());
        let read = sections(&joined).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((read.graph, read.list, read.wasm_graph), (graph, list, wasm));
        let older = sections(graph).err().expect("the older form is refused");
        std::println!("graph payload: {} + {} + {} bytes read back; the older form refused: {older}", graph.len(), list.len(), wasm.len());
    }
}
