//! The kept link images' budget: which images [`super::Paths`] keeps between links once what
//! they hold passes it. Sans IO: told what each kept image weighs and when it was linked, it
//! hands back the paths to let go; the loop drops them.
//!
//! An image is kept so the next link of its path patches it rather than laying it out whole,
//! for the daemon's life unless it is let go here. Past the budget the least recently linked go
//! first (by the link they were kept after, a number the loop gives out in the order it takes
//! links back, never an address or a map's order), and never one a run maps now (a dev
//! executable's running program, whose patches go into the memory it maps). A path whose image
//! was let go has none kept: its next link lays it out whole, which the loop says as it posts
//! that link ([`super::Paths::take_images`]). When the live images alone weigh more than the
//! budget, every other image is let go and the live ones stay over it: the pass says so.

#![forbid(unsafe_code)]

/// The part of the daemon's limit the kept images may hold together: a 32nd (1.47 GiB under
/// coord-111's 47 GiB). They are a cache, there only so a relink patches rather than lays out
/// whole, and what they hold is Link's at rest; the links running now are Link's too and need
/// room under the same [`blaze_graph::admission::fair_share`] (32 of 304 parts, 4.95 GiB at 47).
/// coord-111 (memcap5): with that whole share as the budget the kept images reached it, Link
/// held 4.99 GiB of regions and 2.69 of heap with every link done, and the footprint the 48 GB
/// cap stopped at was 47.28.
pub(super) const LIMIT_PART: u64 = 32;

/// What the kept images may hold together under a limit of `limit` bytes ([`LIMIT_PART`]).
pub(super) const fn budget(limit: u64) -> u64 {
    limit / LIMIT_PART
}

/// What the kept images weigh, by path id, and the order they were linked in.
pub(super) struct KeptImages {
    /// Of each path, its kept image's bytes ([`super::Kept::held`]); 0 when none is kept.
    bytes: Vec<u64>,
    /// Of each path, the link its image was last kept after: the order "least recently
    /// linked" goes by. Stays as it was while the image is taken out, so an image a compile
    /// gave back without linking it keeps its place.
    linked: Vec<u64>,
    /// Whether the path's image was let go past the budget and no link of it was posted since.
    let_go: Vec<bool>,
    /// The last link number given.
    links: u64,
    /// The kept images' bytes together.
    held: u64,
}

/// One kept image as an eviction pass sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Row {
    pub id: u32,
    pub bytes: u64,
    pub linked: u64,
    pub live: bool,
}

/// What an eviction pass decided: the paths to let go, least recently linked first, and what
/// the kept images hold once they are gone; over the budget only when the live ones alone are.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Evictions {
    pub ids: Vec<u32>,
    pub held: u64,
}

impl KeptImages {
    pub(super) fn new() -> KeptImages {
        KeptImages {
            bytes: Vec::new(),  // alloc: none until the first path
            linked: Vec::new(), // alloc: none until the first path
            let_go: Vec::new(), // alloc: none until the first path
            links: 0,
            held: 0,
        }
    }

    /// A path interned: none kept at it.
    pub(super) fn path(&mut self) {
        self.bytes.push(0);
        self.linked.push(0);
        self.let_go.push(false);
    }

    /// The image at `id` kept, weighing `bytes`: after a link of it when `relinked`, else as
    /// it was taken (a compile that gave way), keeping its place in the order.
    pub(super) fn keep(&mut self, id: u32, bytes: u64, relinked: bool) {
        let at = id as usize;
        self.held = self.held - self.bytes[at] + bytes;
        self.bytes[at] = bytes;
        if relinked {
            self.links += 1;
            self.linked[at] = self.links;
        }
        self.let_go[at] = false;
    }

    /// The image at `id` taken out for a link (or let go): it weighs nothing here until kept
    /// again. Whether it was let go past the budget since its last link: said once, by the
    /// first take after.
    pub(super) fn take(&mut self, id: u32) -> bool {
        let at = id as usize;
        self.held -= std::mem::take(&mut self.bytes[at]);
        std::mem::take(&mut self.let_go[at])
    }

    /// The image at `id` let go past the budget, and the bytes it weighed: its next link is
    /// told so.
    pub(super) fn let_go(&mut self, id: u32) -> u64 {
        let bytes = self.bytes[id as usize];
        self.take(id);
        self.let_go[id as usize] = true;
        bytes
    }

    /// The kept images' bytes together.
    pub(super) fn held(&self) -> u64 {
        self.held
    }

    /// A kept image's row, `live` as its image says now.
    pub(super) fn row(&self, id: u32, live: bool) -> Row {
        Row { id, bytes: self.bytes[id as usize], linked: self.linked[id as usize], live }
    }
}

/// Which of `rows` (every kept image, in any order) to let go so the rest hold at most
/// `budget` bytes: the least recently linked first, by `linked` and then `id` (each path's
/// number), never a live one, and none while they fit.
pub(super) fn evictions(rows: &mut [Row], budget: u64) -> Evictions {
    let mut held = rows.iter().map(|it| it.bytes).sum::<u64>();
    // alloc: none while the kept images fit, else the few to let go
    let mut ids = Vec::new();
    if held <= budget {
        return Evictions { ids, held };
    }
    rows.sort_unstable_by_key(|it| (it.linked, it.id));
    for row in rows.iter().filter(|it| !it.live) {
        if held <= budget {
            break;
        }
        held -= row.bytes;
        ids.push(row.id);
    }
    Evictions { ids, held }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u32, bytes: u64, linked: u64, live: bool) -> Row {
        Row { id, bytes, linked, live }
    }

    /// Past the budget the least recently linked images go first, a live one never, and once
    /// the rest fit none more; under it none goes. Ties of link order go by path id, so the
    /// decision never depends on the rows' order.
    #[test]
    fn kept_images_past_the_budget_go_least_recently_linked_first_and_never_a_live_one() {
        // Path 3 is the oldest but live; then 1, then 0 and 4 linked together, then 2.
        let mut rows = [
            row(2, 30, 9, false),
            row(0, 20, 5, false),
            row(3, 50, 1, true),
            row(4, 10, 5, false),
            row(1, 40, 2, false),
        ];
        assert_eq!(
            evictions(&mut rows.clone(), 150),
            Evictions { ids: vec![], held: 150 },
            "under the budget"
        );
        // 150 held, 85 allowed: 1 (40) leaves 110, 0 (20) leaves 90, 4 (10) leaves 80.
        assert_eq!(evictions(&mut rows, 85), Evictions { ids: vec![1, 0, 4], held: 80 });
        // The live image alone is over: every other goes, it stays.
        let mut rows = [row(2, 30, 9, false), row(3, 50, 1, true), row(1, 40, 2, false)];
        assert_eq!(evictions(&mut rows, 10), Evictions { ids: vec![1, 2], held: 50 });

        // The columns: a relink moves an image to the back of the order, a compile that gave
        // way keeps its place, and a take after a let-go says so once.
        let mut kept = KeptImages::new();
        for _ in 0..3 {
            kept.path();
        }
        kept.keep(0, 10, true);
        kept.keep(1, 20, true);
        kept.keep(2, 30, true);
        kept.take(0);
        kept.keep(0, 15, true);
        kept.take(1);
        kept.keep(1, 20, false);
        assert_eq!(kept.held(), 65);
        let mut rows = [kept.row(0, false), kept.row(1, false), kept.row(2, false)];
        let evicted = evictions(&mut rows, 40);
        assert_eq!(evicted, Evictions { ids: vec![1, 2], held: 15 });
        let weighed: Vec<u64> = evicted.ids.iter().map(|&id| kept.let_go(id)).collect(); // alloc: a test's two
        assert_eq!(weighed, [20, 30]);
        assert_eq!(kept.held(), 15);
        assert!(kept.take(1), "the next link of a path let go is told so");
        assert!(!kept.take(1), "once");
        assert!(!kept.take(0), "a path kept within the budget was not let go");
    }

    /// The kept images' budget leaves the links running now room under Link's fair share of the
    /// limit: a 32nd of the limit, under a third of that share. coord-111's 47 GiB gives
    /// 1.47 GiB, where the whole share (4.95) let Link reach 7.68 GiB with no link running.
    #[test]
    fn the_kept_images_budget_leaves_the_running_links_most_of_links_share() {
        const GIB: u64 = 1 << 30;
        let limit = 47 * GIB;
        let share = blaze_graph::admission::fair_share(blaze_mem::Tag::Link, limit);
        assert_eq!(budget(limit), limit / 32);
        assert!(budget(limit) * 3 < share, "{} of a share of {share}", budget(limit));
    }
}
