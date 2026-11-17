//! The offer set: which callee bodies the optimizing tier hands LLVM with one function, as
//! `available_externally` definitions it may inline (backend-next.md, "Evidence, gap and pieces",
//! piece O; decision D98's second point). Sans-IO: the call graph, its sizes and the profile's
//! heat come in through [`Graph`]; nothing here reads a file, a clock or LLVM.
//!
//! **The choice.** Best-first from the root's callees. A candidate's priority is the heat of the
//! edge that reached it (highest first), then its body's size in LIR instructions (smallest
//! first), then its instance key, then the depth it was reached at (lowest first): a total order,
//! so the same graph gives the same set whatever order [`Graph::callees`] lists edges in. A
//! candidate is taken when its size is at most [`OfferBound::one`] and the running total plus its
//! size is at most [`OfferBound::total`]; otherwise it is refused and not expanded. A taken body's
//! callees join the candidates one level deeper, up to [`OfferBound::depth`] (the root's own
//! callees are depth 1). The root is never offered, and an instance is decided once, at its first
//! (best) arrival: a later arrival is dropped, so a cycle ends by construction. A refusal is
//! final because the running total only grows. The candidates are a binary heap over the priority;
//! there is no recursion.
//!
//! **Refusals.** An edge naming an instance with no body is [`OfferError::NoBody`], never skipped:
//! an instance the profile saw called must have a plan. Each expansion's edges are sorted before
//! they are read, so which missing instance is named does not depend on the graph's edge order.
//!
//! **The key's part (D100 point 8).** [`offer_digest`] is the set's digest in the optimized
//! record's key: BLAKE3 in derive-key mode under "blaze offer set v1", over the count and each
//! row's instance and code in the set's sorted order.

use core::cmp::Ordering;
use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::tier::Callee;
use crate::{CodeId, Hex, InstanceKey, Plant};

/// One instance's body as the offer sees it: its code identity, and its size in LIR
/// instructions.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub code: CodeId,
    pub size: u32,
}

/// A call edge out of a body: the callee's instance, and how hot the profile found the edge.
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub callee: InstanceKey,
    pub heat: u64,
}

/// What the offer reads: each instance's body, and each body's call edges.
pub trait Graph {
    /// The body `instance` is compiled from, or `None` when it has none.
    fn body(&self, instance: InstanceKey) -> Option<Body>;
    /// Pushes every call edge out of `instance`'s body onto `out`, in any order.
    fn callees(&self, instance: InstanceKey, out: &mut AVec<'_, Edge>);
}

/// The offer's bounds: the largest one body taken, the most instructions taken in all, and how
/// many calls deep from the root a body may be.
#[derive(Clone, Copy, Debug)]
pub struct OfferBound {
    pub one: u32,
    pub total: u32,
    pub depth: u8,
}

/// The bounds the tier uses. Estimates, to be set from measurements on reth and tempo.
pub const OFFER: OfferBound = OfferBound { one: 400, total: 4000, depth: 3 };

/// Why no offer set was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfferError {
    /// An edge names `instance`, which has no body.
    NoBody { instance: InstanceKey },
}

impl fmt::Display for OfferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OfferError::NoBody { instance } => write!(
                f,
                "offer set: a call edge names instance {}, which has no body (offer.rs refuses an edge without a body; rule 3: never skipped)",
                Hex(&instance.0)
            ),
        }
    }
}

impl core::error::Error for OfferError {}

/// The callee bodies offered with `root`'s optimized compile, sorted by instance key, each
/// instance once. See the module's doc for the choice.
pub fn offer<'a>(
    root: InstanceKey,
    graph: &dyn Graph,
    bound: OfferBound,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, Callee>, OfferError> {
    offer_planted(root, graph, bound, alloc, Plant::None)
}

/// A candidate in the heap: the instance an edge reached, the edge's heat, its body, and the
/// depth it was reached at.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    heat: u64,
    size: u32,
    instance: InstanceKey,
    depth: u8,
    code: CodeId,
}

/// Whether `a` is taken before `b`: heat highest first, then size smallest, then instance key,
/// then depth lowest.
fn before(a: &Candidate, b: &Candidate) -> bool {
    let order = b
        .heat
        .cmp(&a.heat)
        .then(a.size.cmp(&b.size))
        .then(a.instance.cmp(&b.instance))
        .then(a.depth.cmp(&b.depth));
    order == Ordering::Less
}

/// [`offer`] with a safeguard taken out for the tests' planted-bug control:
/// [`Plant::IgnoreTotal`] takes a body whatever the running total.
pub(crate) fn offer_planted<'a>(
    root: InstanceKey,
    graph: &dyn Graph,
    bound: OfferBound,
    alloc: Alloc<'a>,
    plant: Plant,
) -> Result<AVec<'a, Callee>, OfferError> {
    // `taken` is kept sorted by instance (it is the answer); `refused` sorted too. Both are as
    // long as the instances decided, which the bounds keep small, so a shifting insert is
    // cheaper than a hash map whose seed the caller would have to hand in.
    let mut taken: AVec<'a, Callee> = Vec::new_in(alloc);
    let mut refused: AVec<'a, InstanceKey> = Vec::new_in(alloc);
    let mut heap: AVec<'a, Candidate> = Vec::new_in(alloc);
    let mut edges: AVec<'a, Edge> = Vec::new_in(alloc);
    let mut running: u64 = 0;
    if bound.depth >= 1 {
        expand(root, 1, graph, &mut edges, &mut heap)?;
    }
    while let Some(next) = pop(&mut heap) {
        if next.instance == root {
            continue;
        }
        let at_taken = taken.binary_search_by(|row| row.instance.cmp(&next.instance));
        if at_taken.is_ok() {
            continue;
        }
        let at_refused = match refused.binary_search(&next.instance) {
            Ok(_) => continue,
            Err(at) => at,
        };
        let size = u64::from(next.size);
        let under_total = plant == Plant::IgnoreTotal || running + size <= u64::from(bound.total);
        if next.size > bound.one || !under_total {
            refused.insert_shift(at_refused, next.instance);
            continue;
        }
        let at = match at_taken {
            Ok(at) | Err(at) => at,
        };
        taken.insert_shift(at, Callee { instance: next.instance, code: next.code });
        running += size;
        if next.depth < bound.depth {
            expand(next.instance, next.depth + 1, graph, &mut edges, &mut heap)?;
        }
    }
    Ok(taken)
}

/// Pushes every callee of `instance` onto the heap at `depth`, its edges sorted first so the
/// first missing body named is the same in any edge order.
fn expand(
    instance: InstanceKey,
    depth: u8,
    graph: &dyn Graph,
    edges: &mut AVec<'_, Edge>,
    heap: &mut AVec<'_, Candidate>,
) -> Result<(), OfferError> {
    edges.clear();
    graph.callees(instance, edges);
    edges.sort_unstable_by(|a, b| a.callee.cmp(&b.callee).then(b.heat.cmp(&a.heat)));
    for edge in edges.iter() {
        let Some(body) = graph.body(edge.callee) else {
            return Err(OfferError::NoBody { instance: edge.callee });
        };
        push(
            heap,
            Candidate {
                heat: edge.heat,
                size: body.size,
                instance: edge.callee,
                depth,
                code: body.code,
            },
        );
    }
    Ok(())
}

/// Adds `candidate` to the binary heap whose best is at index 0.
fn push(heap: &mut AVec<'_, Candidate>, candidate: Candidate) {
    heap.push(candidate);
    let mut at = heap.len() - 1;
    while at > 0 {
        let parent = (at - 1) / 2;
        if !before(&heap[at], &heap[parent]) {
            break;
        }
        heap.swap(at, parent);
        at = parent;
    }
}

/// Takes the best candidate off the binary heap.
fn pop(heap: &mut AVec<'_, Candidate>) -> Option<Candidate> {
    let last = heap.pop()?;
    if heap.is_empty() {
        return Some(last);
    }
    let best = core::mem::replace(&mut heap[0], last);
    let len = heap.len();
    let mut at = 0;
    loop {
        let left = 2 * at + 1;
        let right = left + 1;
        let mut best_child = at;
        if left < len && before(&heap[left], &heap[best_child]) {
            best_child = left;
        }
        if right < len && before(&heap[right], &heap[best_child]) {
            best_child = right;
        }
        if best_child == at {
            break;
        }
        heap.swap(at, best_child);
        at = best_child;
    }
    Some(best)
}

/// The offer set's digest, the part of the optimized record's key that names it (D100 point 8):
/// BLAKE3 in derive-key mode under "blaze offer set v1", over the count as u32 little-endian and
/// then each row's instance bytes and code bytes, in the order given. [`offer`] gives rows sorted
/// by instance, so one graph gives one digest in any order. Refuses nothing.
pub fn offer_digest(offer: &[Callee]) -> [u8; 32] {
    let mut hasher = blaze_blake3::Hasher::new_derive_key("blaze offer set v1");
    // 2^32 rows of 64 bytes would be 256 GiB of offer: no set reaching here is that long, so
    // the count always fits; it is never clipped.
    let count = u32::try_from(offer.len()).expect("an offer set has fewer than 2^32 rows");
    hasher.update(&count.to_le_bytes());
    for row in offer {
        hasher.update(&row.instance.0);
        hasher.update(&row.code.0);
    }
    hasher.finalize()
}
