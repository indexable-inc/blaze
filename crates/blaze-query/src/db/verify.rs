//! The claim-free verify walk: a row of an earlier view is checked by its edges in read order,
//! depth first, with no flight and no lock, marking each row it proves unchanged with a
//! compare-and-swap of its `verified` word. It gives up (and the caller takes the row's flight)
//! at the first edge that changed, or that needs a run, or a row that is running, provisional
//! or untracked: those are the claimed path's. A final row of a converged cycle is walked like
//! any other: its head's edges are what the whole cycle read outside itself over every round,
//! and a member's one edge is its head (`function::finalize`). A row whose kind did not change,
//! or whose input summary shares no bit with the fields written since it was verified, is
//! proved without its edges; a walked row's summary grows by its dependencies' current ones.
//!
//! The walk reads headers untyped: every function row starts with its [`Header`], and the
//! ingredient's vtable gives the row size. With a hook set it gives up too, so that every
//! validation is reported by the claimed path, which may call the hook.

use std::{ptr::NonNull, sync::atomic::Ordering};

use super::{
    counts,
    ingredient::{self, Kind},
    local::{self, Walk},
    row::{DURABILITY, Header, PROVISIONAL, RUNNING, UNTRACKED, VALUE, VIEW_MASK, header_at, span},
    storage::{FIELD, NodeRow, Storage, color, key_parts, or},
};

/// Edges ahead of the one checked whose colors are prefetched.
const AHEAD: u64 = 4;

/// Verifies the row of `header` (whose word was `word`, of an earlier view, its node `node`)
/// without claiming anything: `true` when it and every row below it it had to look at were
/// marked verified in this view, `false` to take the claimed path.
pub(crate) fn unclaimed(storage: &Storage, header: &Header, word: u32, node: u32) -> bool {
    if storage.has_hook() {
        return false;
    }
    let meta = header.meta.load(Ordering::Relaxed);
    if word & (RUNNING | PROVISIONAL) != 0 || meta & UNTRACKED != 0 || word & VIEW_MASK == 0 {
        return false;
    }
    if storage.unchanged_since(meta as u8 & DURABILITY as u8, word & VIEW_MASK) {
        return mark(storage, header, word, storage.node(node));
    }
    let row = storage.node(node);
    if storage.inputs_unchanged(row, word & VIEW_MASK) {
        return mark(storage, header, word, row);
    }
    local::with(|local| {
        let base = local.walk.len();
        let proved = walk(storage, &mut local.walk, base, header, word, node);
        local.walk.truncate(base);
        proved
    })
}

fn entry(header: &Header, word: u32, node: u32) -> Walk {
    let edges = header.edges.load(Ordering::Acquire);
    let start = span::start(edges);
    Walk {
        header: std::ptr::from_ref(header),
        node,
        verified: word & VIEW_MASK,
        word,
        next: start,
        end: start + span::len(edges),
        summary: [0; 2],
    }
}

fn walk(
    storage: &Storage,
    stack: &mut Vec<Walk>,
    base: usize,
    header: &Header,
    word: u32,
    node: u32,
) -> bool {
    let view = storage.view_bits();
    let depth = storage.nodes_made() as usize;
    // alloc: none once warm: the walk stack grows to its high-water depth once per thread
    stack.push(entry(header, word, node));
    loop {
        let top = stack.last_mut().expect("the walk's root is on its stack");
        if top.next == top.end {
            let done = *top;
            // SAFETY: the header is in a row page of `storage` (pushed from one), which lives
            // as long as the storage, borrowed for this walk.
            let header = unsafe { &*done.header };
            let row = storage.node(done.node);
            // The summary goes in before the mark publishes the row in this view.
            row.grow_summary(done.summary);
            if !mark(storage, header, done.word, row) {
                return false;
            }
            stack.pop();
            if stack.len() == base {
                return true;
            }
            let parent = stack.last_mut().expect("a walked row's reader");
            if header.changed.load(Ordering::Relaxed) > parent.verified {
                return false;
            }
            parent.summary = or(parent.summary, row.summary());
            continue;
        }
        let at = top.next;
        top.next += 1;
        let verified = top.verified;
        if at + AHEAD < top.end {
            let ahead = storage.edge(at + AHEAD) as usize;
            storage.nodes.prefetch(ahead);
        }
        let dep = storage.edge(at);
        let row = storage.node(dep);
        let seen = row.color();
        let painted = (seen >> 32) as u32;
        let proved = if painted >= FIELD || painted == view {
            seen as u32 <= verified
        } else {
            let Some(header) = function_header(storage, row) else { return false };
            let word = header.verified.load(Ordering::Acquire);
            let meta = header.meta.load(Ordering::Relaxed);
            if word & (RUNNING | PROVISIONAL) != 0 || meta & UNTRACKED != 0 || word & VIEW_MASK == 0
            {
                return false;
            }
            if word & VIEW_MASK == view {
                header.changed.load(Ordering::Relaxed) <= verified
            } else if storage.unchanged_since(meta as u8 & DURABILITY as u8, word & VIEW_MASK)
                || storage.inputs_unchanged(row, word & VIEW_MASK)
            {
                mark(storage, header, word, row)
                    && header.changed.load(Ordering::Relaxed) <= verified
            } else {
                if stack.len() - base > depth {
                    return false;
                }
                // alloc: none once warm: the walk stack grows to its high-water depth once per thread
                stack.push(entry(header, word, dep));
                continue;
            }
        };
        if !proved {
            return false;
        }
        let top = stack.last_mut().expect("the walked row");
        top.summary = or(top.summary, row.summary());
    }
}

/// The header of the function row whose node is `node`, if its page was made.
pub(crate) fn function_header<'s>(storage: &'s Storage, node: &NodeRow) -> Option<&'s Header> {
    let (index, id, _) = key_parts(node.key());
    let vtable = ingredient::vtable(index)?;
    if vtable.kind != Kind::Function {
        return None;
    }
    let dir = storage.dir(index);
    if dir.is_null() {
        return None;
    }
    // SAFETY: a published directory is readable whole for `MAX_PAGES` entries
    // (`Pages::dir`), and `id.page() < MAX_PAGES` (masked, `Id::page`).
    let page = NonNull::new(unsafe { &*dir.add(id.page()) }.load(Ordering::Acquire))?;
    // SAFETY: the page is the function's, made with rows of its vtable's `row_size`; the
    // header lives as long as `storage`.
    Some(unsafe { header_at(page, id.slot(), vtable.row_size) })
}

/// Marks a row verified in this view, keeping whether it has its value: `false` when another
/// task changed its word meanwhile.
fn mark(storage: &Storage, header: &Header, word: u32, node: &NodeRow) -> bool {
    let view = storage.view_bits();
    let to = view | (word & VALUE);
    match header.verified.compare_exchange(word, to, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            node.paint(color(view, header.changed.load(Ordering::Relaxed)));
            counts::count(key_parts(node.key()).0, counts::VERIFIED);
            // A row proved current is query work ended, as one verified under its flight is:
            // what the stall watchdog reads (`function::completed`).
            super::function::complete_one();
            true
        }
        Err(now) => now == to,
    }
}
