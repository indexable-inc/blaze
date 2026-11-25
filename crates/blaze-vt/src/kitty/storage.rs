//! One screen's images and placements (ghostty's graphics_storage.zig): the byte cap and its
//! eviction, ids and numbers, placements at a pin or virtual, and every delete.
//!
//! The screen is not this module's type: a placement holds a [`PinId`] the screen tracks, and
//! every question about positions goes through [`GraphicsScreen`], which the screen implements.
//! Positions are plain rows and columns, so the storage is independent of how pages are kept.

use blaze_own::{ABTreeMap, AVec, Alloc, AllocFailure};
use core::ops::Bound::{Excluded, Unbounded};

use super::command::{Delete, FreeImages};
use super::image::{Generation, Image, LoadingImage, Media};
use super::num::round_u32;

/// A pin the screen tracks: it moves with its cell as the screen scrolls and reflows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PinId(pub u64);

/// A cell in the whole screen (scrollback and active area), rows counted from its first row.
/// Ordered as the stream is: by row, then column (`Pin.isBetween`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ScreenPoint {
    pub row: u64,
    pub col: u16,
}

/// A cell in the active area, 0-based.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ActivePoint {
    pub row: u16,
    pub col: u16,
}

/// The terminal's size in cells and pixels (`cols`, `rows`, `width_px`, `height_px`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

impl Grid {
    /// Pixels per cell, wide and high; 0 when the terminal has no pixel size.
    pub fn cell_px(self) -> (u32, u32) {
        let w = self.width_px.checked_div(u32::from(self.cols)).unwrap_or(0);
        let h = self.height_px.checked_div(u32::from(self.rows)).unwrap_or(0);
        (w, h)
    }
}

/// The screen could not track the cursor's pin (ghostty's `trackPin` out of memory).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrackFailed;

/// What the storage asks of the screen it belongs to. The core's screen implements it.
pub trait GraphicsScreen {
    /// The size in cells and pixels.
    fn grid(&self) -> Grid;
    /// The cursor, in the active area.
    fn cursor(&self) -> ActivePoint;
    /// Track the cursor's cell, for a placement there.
    fn track_cursor(&mut self) -> Result<PinId, TrackFailed>;
    /// Stop tracking a pin a placement held.
    fn untrack(&mut self, pin: PinId);
    /// Where a tracked pin is now.
    fn locate(&self, pin: PinId) -> ScreenPoint;
    /// The screen cell of an active-area cell, `None` outside the active area.
    fn active(&self, at: ActivePoint) -> Option<ScreenPoint>;
    /// The last row of the screen (a placement's bottom stops there, `downOverflow`).
    fn last_row(&self) -> u64;
}

/// The counter that stamps every change, shared by a terminal's screens (ghostty keeps one per
/// process; here it is passed in, rule 75).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Generations {
    last: u64,
}

impl Generations {
    /// The next stamp: strictly greater than every one before, never zero.
    pub fn next(&mut self) -> Generation {
        self.last += 1;
        Generation(self.last)
    }
}

/// Whether a placement id was sent or given by the storage for `p=0`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PlacementTag {
    Internal,
    External,
}

/// A placement's key: its image and its id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PlacementKey {
    pub image_id: u32,
    pub tag: PlacementTag,
    pub id: u32,
}

impl PlacementKey {
    pub const fn external(image_id: u32, id: u32) -> PlacementKey {
        PlacementKey { image_id, tag: PlacementTag::External, id }
    }

    pub const fn internal(image_id: u32, id: u32) -> PlacementKey {
        PlacementKey { image_id, tag: PlacementTag::Internal, id }
    }

    const fn first_of(image_id: u32) -> PlacementKey {
        PlacementKey { image_id, tag: PlacementTag::Internal, id: 0 }
    }
}

/// Where a placement is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Location {
    /// At a tracked pin: its top-left cell.
    Pin(PinId),
    /// Virtual (`U=1`): drawn where U+10EEEE placeholders name it.
    Virtual,
}

/// An image placed (`Placement`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Placement {
    pub location: Location,
    pub x_offset: u32,
    pub y_offset: u32,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub columns: u32,
    pub rows: u32,
    pub z: i32,
}

impl Placement {
    /// A placement with every size and offset zero.
    pub const fn at(location: Location) -> Placement {
        Placement {
            location,
            x_offset: 0,
            y_offset: 0,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            columns: 0,
            rows: 0,
            z: 0,
        }
    }

    /// The pixel size it is drawn at (`pixelSize`): the source rectangle, or the image, scaled
    /// to the columns and rows asked, one of them alone keeping the aspect ratio.
    pub fn pixel_size(&self, image: &Image<'_>, grid: Grid) -> PixelSize {
        let width = if self.source_width > 0 { self.source_width } else { image.width };
        let height = if self.source_height > 0 { self.source_height } else { image.height };
        if self.columns == 0 && self.rows == 0 {
            return PixelSize { width, height };
        }
        let (cell_w, cell_h) = grid.cell_px();
        if self.columns > 0 && self.rows > 0 {
            return PixelSize {
                width: cell_w.saturating_mul(self.columns),
                height: cell_h.saturating_mul(self.rows),
            };
        }
        let (wf, hf) = (f64::from(width), f64::from(height));
        if self.columns > 0 {
            let calc_width = cell_w.saturating_mul(self.columns);
            return PixelSize {
                width: calc_width,
                height: round_u32(f64::from(calc_width) * (hf / wf)),
            };
        }
        let calc_height = cell_h.saturating_mul(self.rows);
        PixelSize { width: round_u32(f64::from(calc_height) * (wf / hf)), height: calc_height }
    }

    /// The cells it covers (`gridSize`), rounded up; 0 when the terminal has no pixel size.
    pub fn grid_size(&self, image: &Image<'_>, grid: Grid) -> GridSize {
        if self.columns > 0 && self.rows > 0 {
            return GridSize { cols: self.columns, rows: self.rows };
        }
        let px = self.pixel_size(image, grid);
        let (cell_w, cell_h) = grid.cell_px();
        let div_ceil = |n: u32, d: u32| if d == 0 { 0 } else { n.div_ceil(d) };
        GridSize {
            cols: div_ceil(px.width.saturating_add(self.x_offset), cell_w),
            rows: div_ceil(px.height.saturating_add(self.y_offset), cell_h),
        }
    }

    /// Its rectangle on the screen (`rect`); `None` for a virtual placement. The bottom stops
    /// at the screen's last row and the right at its last column. A grid size of zero (no
    /// pixel size) is taken as one cell, where ghostty's `- 1` would wrap.
    pub fn rect(&self, image: &Image<'_>, screen: &impl GraphicsScreen) -> Option<Rect> {
        let Location::Pin(pin) = self.location else {
            return None;
        };
        let grid = screen.grid();
        let size = self.grid_size(image, grid);
        let tl = screen.locate(pin);
        let br_row =
            tl.row.saturating_add(u64::from(size.rows.saturating_sub(1))).min(screen.last_row());
        let br_col = (u32::from(tl.col).saturating_add(size.cols.saturating_sub(1)))
            .min(u32::from(grid.cols.saturating_sub(1)));
        Some(Rect { top_left: tl, bottom_right: ScreenPoint { row: br_row, col: br_col as u16 } })
    }
}

/// [`Placement::pixel_size`]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

/// [`Placement::grid_size`]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GridSize {
    pub cols: u32,
    pub rows: u32,
}

/// The cells a placement covers, both corners inclusive.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rect {
    pub top_left: ScreenPoint,
    pub bottom_right: ScreenPoint,
}

impl Rect {
    /// `Pin.isBetween`: in stream order from the top-left to the bottom-right.
    pub fn contains(&self, p: ScreenPoint) -> bool {
        self.top_left <= p && p <= self.bottom_right
    }
}

/// The byte cap of a storage; 0 disables kitty graphics on its screen.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StorageLimit(pub usize);

impl StorageLimit {
    /// ghostty's app default, 320 MB.
    pub const DEFAULT: StorageLimit = StorageLimit(320 * 1000 * 1000);
}

/// The storage could not make room or record what was asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutOfMemory;

impl From<AllocFailure> for OutOfMemory {
    fn from(_: AllocFailure) -> OutOfMemory {
        OutOfMemory
    }
}

/// The first image id given to a command that sent none (`next_image_id`).
const FIRST_IMPLICIT_ID: u32 = 2_147_483_647;

/// One screen's images and placements (`ImageStorage`).
pub struct ImageStorage<'a> {
    /// Set by every change; the renderer clears it.
    pub dirty: bool,
    /// The stamp of the last change.
    pub generation: Generation,
    pub(crate) next_image_id: u32,
    next_internal_placement_id: u32,
    images: ABTreeMap<'a, u32, Image<'a>>,
    placements: ABTreeMap<'a, PlacementKey, Placement>,
    /// A chunked transmission in progress (`m=1`).
    pub(crate) loading: Option<LoadingImage<'a>>,
    /// The media beyond direct this screen accepts.
    pub media: Media,
    total_bytes: usize,
    total_limit: StorageLimit,
    alloc: Alloc<'a>,
}

impl<'a> ImageStorage<'a> {
    /// An empty storage with this byte cap and media (ghostty's defaults are
    /// [`StorageLimit::DEFAULT`] and [`Media::DIRECT`]).
    pub fn new_in(limit: StorageLimit, media: Media, alloc: Alloc<'a>) -> ImageStorage<'a> {
        ImageStorage {
            dirty: false,
            generation: Generation::default(),
            next_image_id: FIRST_IMPLICIT_ID,
            next_internal_placement_id: 0,
            images: ABTreeMap::new_in(alloc),
            placements: ABTreeMap::new_in(alloc),
            loading: None,
            media,
            total_bytes: 0,
            total_limit: limit,
            alloc,
        }
    }

    /// Whether kitty graphics are on for this screen (a non-zero limit).
    pub fn enabled(&self) -> bool {
        self.total_limit.0 != 0
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn total_limit(&self) -> StorageLimit {
        self.total_limit
    }

    pub fn images(&self) -> impl Iterator<Item = &Image<'a>> {
        self.images.values()
    }

    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    pub fn placements(&self) -> impl Iterator<Item = (&PlacementKey, &Placement)> {
        self.placements.iter()
    }

    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    pub fn placement(&self, key: PlacementKey) -> Option<&Placement> {
        self.placements.get(&key)
    }

    /// Whether a chunked transmission is in progress.
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    fn mark_mutated(&mut self, gens: &mut Generations) {
        self.dirty = true;
        self.generation = gens.next();
    }

    /// Drop everything, untracking every pin, keeping the media (ghostty's `deinit` then
    /// `self.* = .{ .image_limits = image_limits }`).
    pub fn reset(&mut self, screen: &mut impl GraphicsScreen) {
        for p in self.placements.values() {
            if let Location::Pin(pin) = p.location {
                screen.untrack(pin);
            }
        }
        *self = ImageStorage::new_in(StorageLimit::DEFAULT, self.media, self.alloc);
    }

    /// Set the byte cap (`setLimit`): 0 resets and disables; a lower one evicts.
    pub fn set_limit(
        &mut self,
        limit: StorageLimit,
        screen: &mut impl GraphicsScreen,
        gens: &mut Generations,
    ) -> Result<(), OutOfMemory> {
        if limit.0 == 0 {
            self.reset(screen);
            self.mark_mutated(gens);
        }
        if limit.0 < self.total_bytes {
            let req = self.total_bytes - limit.0;
            // Not finding enough to evict is not an error here (ghostty logs it).
            self.evict(req, screen, gens)?;
        }
        self.total_limit = limit;
        Ok(())
    }

    /// Store an image (`addImage`), replacing one of the same id, evicting to make room.
    pub fn add_image(
        &mut self,
        mut img: Image<'a>,
        screen: &mut impl GraphicsScreen,
        gens: &mut Generations,
    ) -> Result<(), OutOfMemory> {
        let len = img.data.len();
        if len > self.total_limit.0 {
            return Err(OutOfMemory);
        }
        let total = self.total_bytes + len;
        if total > self.total_limit.0 && !self.evict(total - self.total_limit.0, screen, gens)? {
            return Err(OutOfMemory);
        }
        self.mark_mutated(gens);
        img.generation = self.generation;
        let id = img.id;
        let old = self.images.try_insert(id, img).map_err(|r| OutOfMemory::from(r.why))?;
        if let Some(old) = old {
            self.total_bytes -= old.data.len();
        }
        self.total_bytes += len;
        Ok(())
    }

    /// Place an image (`addPlacement`): `placement_id` 0 gets an internal id.
    pub fn add_placement(
        &mut self,
        image_id: u32,
        placement_id: u32,
        p: Placement,
        gens: &mut Generations,
    ) -> Result<(), OutOfMemory> {
        debug_assert!(self.images.contains_key(&image_id), "placement of an image not stored");
        let key = match placement_id {
            0 => {
                let id = self.next_internal_placement_id;
                self.next_internal_placement_id = id.wrapping_add(1);
                PlacementKey::internal(image_id, id)
            }
            id => PlacementKey::external(image_id, id),
        };
        self.placements.try_insert(key, p).map_err(|r| OutOfMemory::from(r.why))?;
        self.mark_mutated(gens);
        Ok(())
    }

    pub fn image_by_id(&self, id: u32) -> Option<&Image<'a>> {
        self.images.get(&id)
    }

    /// The most recently stored image with this number (`imageByNumber`).
    pub fn image_by_number(&self, number: u32) -> Option<&Image<'a>> {
        self.images.values().filter(|i| i.number == number).max_by_key(|i| i.generation)
    }

    /// Run a delete (`delete`). A delete that removes nothing is no change.
    pub fn delete(
        &mut self,
        cmd: Delete,
        screen: &mut impl GraphicsScreen,
        gens: &mut Generations,
    ) {
        let before = (self.placements.len(), self.images.len());
        match cmd {
            Delete::All(free) => {
                self.remove_placements(screen, FreeImages::No, |_, _, _, p| {
                    p.location != Location::Virtual
                });
                if free == FreeImages::Yes {
                    let placements = &self.placements;
                    let total = &mut self.total_bytes;
                    self.images.retain(|id, img| {
                        let used = image_used(placements, *id);
                        if !used {
                            *total -= img.data.len();
                        }
                        used
                    });
                }
            }
            Delete::Id { free, image_id, placement_id } => {
                self.delete_by_id(image_id, placement_id, free, screen)
            }
            Delete::Newest { free, image_number, placement_id } => {
                if let Some(id) = self.image_by_number(image_number).map(|i| i.id) {
                    self.delete_by_id(id, placement_id, free, screen);
                }
            }
            Delete::IntersectCursor(free) => {
                let at = screen.cursor();
                self.delete_intersecting(at, free, None, screen);
            }
            Delete::IntersectCell { free, x, y } => {
                if let Some(at) = active_of(x, y) {
                    self.delete_intersecting(at, free, None, screen);
                }
            }
            Delete::IntersectCellZ { free, x, y, z } => {
                if let Some(at) = active_of(x, y) {
                    self.delete_intersecting(at, free, Some(z), screen);
                }
            }
            Delete::Column { free, x } => {
                if x > 0 {
                    let x = x - 1;
                    self.remove_placements(screen, free, |s, img, _, p| {
                        p.rect(img, s).is_some_and(|r| {
                            u32::from(r.top_left.col) <= x && u32::from(r.bottom_right.col) >= x
                        })
                    });
                }
            }
            Delete::Row { free, y } => {
                let target = match y {
                    0 => None,
                    y => u16::try_from(y - 1)
                        .ok()
                        .and_then(|row| screen.active(ActivePoint { row, col: 0 })),
                };
                if let Some(target) = target {
                    self.remove_placements(screen, free, |s, img, _, p| {
                        p.rect(img, s).is_some_and(|r| {
                            r.contains(ScreenPoint { row: target.row, col: r.top_left.col })
                        })
                    });
                }
            }
            Delete::Z { free, z } => {
                self.remove_placements(screen, free, |_, _, _, p| {
                    p.location != Location::Virtual && p.z == z
                });
            }
            Delete::Range { free, first, last } => {
                if first > 0 && last > 0 && first <= last {
                    // ghostty's test is `id >= first or id <= last`, true for every id: every
                    // placement goes (its range tests encode this; kept for parity).
                    self.remove_placements(screen, free, |_, _, k, _| {
                        k.image_id >= first || k.image_id <= last
                    });
                }
            }
            Delete::AnimationFrames(_) => {}
        }
        if (self.placements.len(), self.images.len()) != before {
            self.mark_mutated(gens);
        }
    }

    fn delete_by_id(
        &mut self,
        image_id: u32,
        placement_id: u32,
        free: FreeImages,
        screen: &mut impl GraphicsScreen,
    ) {
        if placement_id == 0 {
            let removed = remove_image_placements(&mut self.placements, image_id, screen, |_| true);
            let _ = removed;
        } else if let Some(p) =
            self.placements.remove(&PlacementKey::external(image_id, placement_id))
            && let Location::Pin(pin) = p.location
        {
            screen.untrack(pin);
        }
        if free == FreeImages::Yes {
            self.delete_if_unused(image_id);
        }
    }

    fn delete_if_unused(&mut self, image_id: u32) {
        if image_used(&self.placements, image_id) {
            return;
        }
        if let Some(img) = self.images.remove(&image_id) {
            self.total_bytes -= img.data.len();
        }
    }

    fn delete_intersecting(
        &mut self,
        at: ActivePoint,
        free: FreeImages,
        z: Option<i32>,
        screen: &mut impl GraphicsScreen,
    ) {
        let Some(target) = screen.active(at) else {
            return;
        };
        self.remove_placements(screen, free, |s, img, _, p| {
            p.rect(img, s).is_some_and(|r| r.contains(target)) && z.is_none_or(|z| p.z == z)
        });
    }

    /// Remove each placement `doomed` names, untracking its pin, image by image in id order;
    /// with `free`, an image left with no placement goes too (`deleteIfUnused`).
    fn remove_placements<S: GraphicsScreen>(
        &mut self,
        screen: &mut S,
        free: FreeImages,
        mut doomed: impl FnMut(&S, &Image<'a>, &PlacementKey, &Placement) -> bool,
    ) {
        let mut next = self.images.keys().next().copied();
        while let Some(id) = next {
            next = self.images.range((Excluded(id), Unbounded)).next().map(|(k, _)| *k);
            let Some(img) = self.images.get(&id) else { continue };
            let removed = remove_image_placements(&mut self.placements, id, screen, |args| {
                doomed(args.0, img, args.1, args.2)
            });
            if removed > 0 && free == FreeImages::Yes {
                self.delete_if_unused(id);
            }
        }
    }

    /// Free at least `req` bytes (`evictImage`): unused images first, then the oldest, each with
    /// its placements. Returns whether more than `req` was freed.
    fn evict(
        &mut self,
        req: usize,
        screen: &mut impl GraphicsScreen,
        gens: &mut Generations,
    ) -> Result<bool, OutOfMemory> {
        struct Candidate {
            id: u32,
            generation: Generation,
            used: bool,
        }
        let mut candidates: AVec<'a, Candidate> = AVec::new_in(self.alloc);
        candidates.try_reserve(self.images.len())?;
        for img in self.images.values() {
            let used = image_used(&self.placements, img.id);
            let _ = candidates.try_push(Candidate { id: img.id, generation: img.generation, used });
        }
        candidates.sort_unstable_by(|a, b| {
            a.used.cmp(&b.used).then(a.generation.cmp(&b.generation)).then(a.id.cmp(&b.id))
        });
        let mut any = false;
        let mut evicted = 0usize;
        let mut enough = false;
        for c in candidates.iter() {
            // ghostty removes these placements without untracking their pins; they are
            // untracked here so no pin outlives its placement.
            if remove_image_placements(&mut self.placements, c.id, screen, |_| true) > 0 {
                any = true;
            }
            if let Some(img) = self.images.remove(&c.id) {
                evicted += img.data.len();
                self.total_bytes -= img.data.len();
                any = true;
                if evicted > req {
                    enough = true;
                    break;
                }
            }
        }
        if any {
            self.mark_mutated(gens);
        }
        Ok(enough)
    }
}

/// Whether any placement shows this image.
fn image_used(placements: &ABTreeMap<'_, PlacementKey, Placement>, image_id: u32) -> bool {
    placements
        .range(PlacementKey::first_of(image_id)..)
        .next()
        .is_some_and(|(k, _)| k.image_id == image_id)
}

/// Remove the placements of one image `doomed` names, untracking their pins; returns how many.
fn remove_image_placements<S: GraphicsScreen>(
    placements: &mut ABTreeMap<'_, PlacementKey, Placement>,
    image_id: u32,
    screen: &mut S,
    mut doomed: impl FnMut((&S, &PlacementKey, &Placement)) -> bool,
) -> usize {
    let mut removed = 0;
    let mut from = Some(PlacementKey::first_of(image_id));
    let mut after: Option<PlacementKey> = None;
    loop {
        let found = match (from.take(), after) {
            (Some(start), _) => placements.range(start..).next(),
            (None, Some(prev)) => placements.range((Excluded(prev), Unbounded)).next(),
            (None, None) => None,
        };
        let Some((&key, &p)) = found else { break };
        if key.image_id != image_id {
            break;
        }
        after = Some(key);
        if doomed((&*screen, &key, &p)) {
            placements.remove(&key);
            if let Location::Pin(pin) = p.location {
                screen.untrack(pin);
            }
            removed += 1;
        }
    }
    removed
}

/// A 1-based cell from a delete, in the active area; `None` for 0 or past `u16`.
fn active_of(x: u32, y: u32) -> Option<ActivePoint> {
    if x == 0 || y == 0 {
        return None;
    }
    Some(ActivePoint { col: u16::try_from(x - 1).ok()?, row: u16::try_from(y - 1).ok()? })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::vec::Vec;

    pub(crate) fn alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    /// A screen of `rows` x `cols` with no scrollback: tracked pins stay where they were put.
    pub(crate) struct FakeScreen {
        pub grid: Grid,
        pub cursor: ActivePoint,
        pub pins: Vec<(PinId, ScreenPoint)>,
        pub next_pin: u64,
        pub refuse_track: bool,
    }

    impl FakeScreen {
        pub(crate) fn new(cols: u16, rows: u16) -> FakeScreen {
            FakeScreen {
                grid: Grid { cols, rows, width_px: 0, height_px: 0 },
                cursor: ActivePoint { row: 0, col: 0 },
                pins: Vec::new(),
                next_pin: 0,
                refuse_track: false,
            }
        }

        pub(crate) fn px(mut self, width_px: u32, height_px: u32) -> FakeScreen {
            self.grid.width_px = width_px;
            self.grid.height_px = height_px;
            self
        }

        /// `trackPin(&t, .{ .x, .y })`
        pub(crate) fn track(&mut self, col: u16, row: u16) -> PinId {
            let id = PinId(self.next_pin);
            self.next_pin += 1;
            self.pins.push((id, ScreenPoint { row: u64::from(row), col }));
            id
        }

        pub(crate) fn tracked(&self) -> usize {
            self.pins.len()
        }
    }

    impl GraphicsScreen for FakeScreen {
        fn grid(&self) -> Grid {
            self.grid
        }
        fn cursor(&self) -> ActivePoint {
            self.cursor
        }
        fn track_cursor(&mut self) -> Result<PinId, TrackFailed> {
            if self.refuse_track {
                return Err(TrackFailed);
            }
            let c = self.cursor;
            Ok(self.track(c.col, c.row))
        }
        fn untrack(&mut self, pin: PinId) {
            let at = self
                .pins
                .iter()
                .position(|(p, _)| *p == pin)
                .expect("untrack of a pin not tracked");
            self.pins.remove(at);
        }
        fn locate(&self, pin: PinId) -> ScreenPoint {
            self.pins.iter().find(|(p, _)| *p == pin).expect("locate of a pin not tracked").1
        }
        fn active(&self, at: ActivePoint) -> Option<ScreenPoint> {
            (at.row < self.grid.rows && at.col < self.grid.cols)
                .then_some(ScreenPoint { row: u64::from(at.row), col: at.col })
        }
        fn last_row(&self) -> u64 {
            u64::from(self.grid.rows) - 1
        }
    }

    pub(crate) fn image(id: u32, width: u32, height: u32) -> Image<'static> {
        Image::empty_in(id, width, height, alloc())
    }

    fn pin_at(t: &mut FakeScreen, col: u16, row: u16) -> Placement {
        Placement::at(Location::Pin(t.track(col, row)))
    }

    struct Fixture {
        t: FakeScreen,
        s: ImageStorage<'static>,
        g: Generations,
    }

    impl Fixture {
        fn new(cols: u16, rows: u16, px: (u32, u32)) -> Fixture {
            Fixture {
                t: FakeScreen::new(cols, rows).px(px.0, px.1),
                s: ImageStorage::new_in(StorageLimit::DEFAULT, Media::DIRECT, alloc()),
                g: Generations::default(),
            }
        }
        fn add(&mut self, img: Image<'static>) {
            self.s.add_image(img, &mut self.t, &mut self.g).unwrap();
        }
        fn place(&mut self, image_id: u32, placement_id: u32, col: u16, row: u16) {
            let p = pin_at(&mut self.t, col, row);
            self.s.add_placement(image_id, placement_id, p, &mut self.g).unwrap();
        }
        fn delete(&mut self, d: Delete) {
            self.s.delete(d, &mut self.t, &mut self.g);
        }
        /// Images 1, 2, 3; placements (1, 1) and (2, 1) at (1, 1).
        fn three_two() -> Fixture {
            let mut f = Fixture::new(3, 3, (0, 0));
            for id in 1..=3 {
                f.add(image(id, 0, 0));
            }
            f.place(1, 1, 1, 1);
            f.place(2, 1, 1, 1);
            f.s.dirty = false;
            f
        }
        /// 100x100 cells of one pixel; image 1 is 50x50 and image 2 25x25; placements (1, 1) at
        /// (0, 0) and (1, 2) at (25, 25).
        fn hundred() -> Fixture {
            let mut f = Fixture::new(100, 100, (100, 100));
            f.add(image(1, 50, 50));
            f.add(image(2, 25, 25));
            f.place(1, 1, 0, 0);
            f.place(1, 2, 25, 25);
            f.s.dirty = false;
            f
        }
    }

    use FreeImages::{No, Yes};

    // ghostty graphics_storage.zig:914 "storage: add placement with zero placement id"
    #[test]
    fn storage_add_placement_with_zero_placement_id() {
        let mut f = Fixture::new(100, 100, (100, 100));
        f.add(image(1, 50, 50));
        f.add(image(2, 25, 25));
        f.place(1, 0, 25, 25);
        f.place(1, 0, 25, 25);
        assert_eq!(f.s.placement_count(), 2);
        assert_eq!(f.s.image_count(), 2);
        assert!(f.s.placement(PlacementKey::internal(1, 0)).is_some());
        assert!(f.s.placement(PlacementKey::internal(1, 1)).is_some());
    }

    // ghostty graphics_storage.zig:943 "storage: delete all placements and images"
    #[test]
    fn storage_delete_all_placements_and_images() {
        let mut f = Fixture::three_two();
        f.delete(Delete::All(Yes));
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (0, 0, 0));
    }

    // ghostty graphics_storage.zig:966 "storage: delete all placements and images preserves limit"
    #[test]
    fn storage_delete_all_placements_and_images_preserves_limit() {
        let mut f = Fixture::new(3, 3, (0, 0));
        f.s.set_limit(StorageLimit(5000), &mut f.t, &mut f.g).unwrap();
        for id in 1..=3 {
            f.add(image(id, 0, 0));
        }
        f.place(1, 1, 1, 1);
        f.place(2, 1, 1, 1);
        f.s.dirty = false;
        f.delete(Delete::All(Yes));
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (0, 0, 0));
        assert_eq!(f.s.total_limit(), StorageLimit(5000));
    }

    // ghostty graphics_storage.zig:991 "storage: delete all placements"
    #[test]
    fn storage_delete_all_placements() {
        let mut f = Fixture::three_two();
        f.delete(Delete::All(No));
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (0, 3, 0));
    }

    // ghostty graphics_storage.zig:1014 "storage: delete all placements by image id"
    #[test]
    fn storage_delete_all_placements_by_image_id() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Id { free: No, image_id: 2, placement_id: 0 });
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 3, 1));
    }

    // ghostty graphics_storage.zig:1037 "storage: delete all placements by image id and unused images"
    #[test]
    fn storage_delete_all_placements_by_image_id_and_unused_images() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Id { free: Yes, image_id: 2, placement_id: 0 });
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 2, 1));
    }

    // ghostty graphics_storage.zig:1060 "storage: delete placement by specific id"
    #[test]
    fn storage_delete_placement_by_specific_id() {
        let mut f = Fixture::new(3, 3, (0, 0));
        for id in 1..=3 {
            f.add(image(id, 0, 0));
        }
        f.place(1, 1, 1, 1);
        f.place(1, 2, 1, 1);
        f.place(2, 1, 1, 1);
        f.s.dirty = false;
        f.delete(Delete::Id { free: Yes, image_id: 1, placement_id: 2 });
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (2, 3, 2));
    }

    // ghostty graphics_storage.zig:1088 "storage: delete intersecting cursor"
    #[test]
    fn storage_delete_intersecting_cursor() {
        let mut f = Fixture::hundred();
        f.t.cursor = ActivePoint { row: 12, col: 12 };
        f.delete(Delete::IntersectCursor(No));
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 2)).is_some());
    }

    // ghostty graphics_storage.zig:1120 "storage: delete intersecting cursor plus unused"
    #[test]
    fn storage_delete_intersecting_cursor_plus_unused() {
        let mut f = Fixture::hundred();
        f.t.cursor = ActivePoint { row: 12, col: 12 };
        f.delete(Delete::IntersectCursor(Yes));
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 2)).is_some());
    }

    // ghostty graphics_storage.zig:1152 "storage: delete intersecting cursor hits multiple"
    #[test]
    fn storage_delete_intersecting_cursor_hits_multiple() {
        let mut f = Fixture::hundred();
        f.t.cursor = ActivePoint { row: 26, col: 26 };
        f.delete(Delete::IntersectCursor(Yes));
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (0, 1, 0));
    }

    // ghostty graphics_storage.zig:1178 "storage: delete by column"
    #[test]
    fn storage_delete_by_column() {
        let mut f = Fixture::hundred();
        f.delete(Delete::Column { free: No, x: 60 });
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 1)).is_some());
    }

    // ghostty graphics_storage.zig:1211 "storage: delete by column 1x1"
    #[test]
    fn storage_delete_by_column_1x1() {
        let mut f = Fixture::new(100, 100, (100, 100));
        f.add(image(1, 1, 1));
        f.place(1, 1, 0, 0);
        f.place(1, 2, 1, 0);
        f.place(1, 3, 2, 0);
        f.delete(Delete::Column { free: No, x: 2 });
        assert_eq!((f.s.placement_count(), f.s.image_count()), (2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 1)).is_some());
        assert!(f.s.placement(PlacementKey::external(1, 3)).is_some());
    }

    // ghostty graphics_storage.zig:1244 "storage: delete by row"
    #[test]
    fn storage_delete_by_row() {
        let mut f = Fixture::hundred();
        f.delete(Delete::Row { free: No, y: 60 });
        assert!(f.s.dirty);
        assert_eq!((f.s.placement_count(), f.s.image_count(), f.t.tracked()), (1, 2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 1)).is_some());
    }

    // ghostty graphics_storage.zig:1277 "storage: delete by row 1x1"
    #[test]
    fn storage_delete_by_row_1x1() {
        let mut f = Fixture::new(100, 100, (100, 100));
        f.add(image(1, 1, 1));
        f.place(1, 1, 0, 0);
        f.place(1, 2, 0, 1);
        f.place(1, 3, 0, 2);
        f.delete(Delete::Row { free: No, y: 2 });
        assert_eq!((f.s.placement_count(), f.s.image_count()), (2, 1));
        assert!(f.s.placement(PlacementKey::external(1, 1)).is_some());
        assert!(f.s.placement(PlacementKey::external(1, 3)).is_some());
    }

    // ghostty graphics_storage.zig:1310 "storage: delete images by range 1"
    #[test]
    fn storage_delete_images_by_range_1() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Range { free: No, first: 1, last: 2 });
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (3, 0, 0));
    }

    // ghostty graphics_storage.zig:1335 "storage: delete images by range 2"
    #[test]
    fn storage_delete_images_by_range_2() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Range { free: Yes, first: 1, last: 2 });
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (1, 0, 0));
    }

    // ghostty graphics_storage.zig:1360 "storage: delete images by range 3"
    #[test]
    fn storage_delete_images_by_range_3() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Range { free: No, first: 1, last: 1 });
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (3, 0, 0));
    }

    // ghostty graphics_storage.zig:1385 "storage: delete images by range 4"
    #[test]
    fn storage_delete_images_by_range_4() {
        let mut f = Fixture::three_two();
        f.delete(Delete::Range { free: Yes, first: 1, last: 1 });
        assert!(f.s.dirty);
        assert_eq!((f.s.image_count(), f.s.placement_count(), f.t.tracked()), (1, 0, 0));
    }

    // ghostty graphics_storage.zig:1410 "storage: aspect ratio calculation when only columns or rows specified"
    #[test]
    fn storage_aspect_ratio_calculation_when_only_columns_or_rows_specified() {
        let grid = Grid { cols: 100, rows: 100, width_px: 1000, height_px: 2000 };
        let img = image(1, 16, 9);
        let p = Placement { columns: 10, rows: 0, ..Placement::at(Location::Virtual) };
        assert_eq!(p.pixel_size(&img, grid), PixelSize { width: 100, height: 56 });
        let img = image(2, 16, 9);
        let p = Placement { columns: 0, rows: 5, ..Placement::at(Location::Virtual) };
        assert_eq!(p.pixel_size(&img, grid), PixelSize { width: 178, height: 100 });
    }

    // ghostty graphics_storage.zig:1456 "storage: generation stamps on image add and replace"
    #[test]
    fn storage_generation_stamps_on_image_add_and_replace() {
        let mut f = Fixture::new(3, 3, (0, 0));
        assert_eq!(f.s.generation, Generation(0));
        f.add(image(1, 1, 1));
        let gen1 = f.s.generation;
        assert!(gen1 > Generation(0));
        assert_eq!(f.s.image_by_id(1).unwrap().generation, gen1);
        f.add(image(2, 1, 1));
        let gen2 = f.s.generation;
        assert!(gen2 > gen1);
        assert_eq!(f.s.image_by_id(2).unwrap().generation, gen2);
        f.add(image(1, 1, 1));
        let gen3 = f.s.generation;
        assert!(gen3 > gen2);
        assert_eq!(f.s.image_by_id(1).unwrap().generation, gen3);
        assert_eq!(f.s.image_by_id(2).unwrap().generation, gen2);
    }

    // ghostty graphics_storage.zig:1493 "storage: generation bumps on placement and delete"
    #[test]
    fn storage_generation_bumps_on_placement_and_delete() {
        let mut f = Fixture::new(3, 3, (0, 0));
        f.add(image(1, 0, 0));
        let gen_add = f.s.generation;
        f.place(1, 1, 1, 1);
        let gen_place = f.s.generation;
        assert!(gen_place > gen_add);
        let _ = f.s.image_by_id(1);
        let _ = f.s.image_by_number(1);
        assert_eq!(f.s.generation, gen_place);
        f.delete(Delete::All(Yes));
        assert!(f.s.generation > gen_place);
    }

    // ghostty graphics_storage.zig:1517 "storage: generation bumps when setLimit evicts or disables"
    #[test]
    fn storage_generation_bumps_when_set_limit_evicts_or_disables() {
        let mut f = Fixture::new(3, 3, (0, 0));
        let mut img = image(1, 1, 1);
        img.data.try_extend_from_slice(b"1234").unwrap();
        f.add(img);
        let gen_add = f.s.generation;
        f.s.dirty = false;
        f.s.set_limit(StorageLimit(1), &mut f.t, &mut f.g).unwrap();
        assert!(f.s.dirty);
        assert!(f.s.generation > gen_add);
        assert_eq!(f.s.image_count(), 0);
        let gen_evict = f.s.generation;
        f.s.dirty = false;
        f.s.set_limit(StorageLimit(0), &mut f.t, &mut f.g).unwrap();
        assert!(f.s.dirty);
        assert!(f.s.generation > gen_evict);
        assert!(!f.s.enabled());
    }

    // ghostty graphics_storage.zig:1545 "storage: imageByNumber returns most recently transmitted"
    #[test]
    fn storage_image_by_number_returns_most_recently_transmitted() {
        let mut f = Fixture::new(3, 3, (0, 0));
        f.add(Image { number: 7, ..image(1, 0, 0) });
        f.add(Image { number: 7, ..image(2, 0, 0) });
        assert_eq!(f.s.image_by_number(7).unwrap().id, 2);
        f.add(Image { number: 7, ..image(1, 0, 0) });
        assert_eq!(f.s.image_by_number(7).unwrap().id, 1);
    }

    // ghostty graphics_storage.zig:1565 "storage: nextGeneration is unique and monotonic"
    #[test]
    fn storage_next_generation_is_unique_and_monotonic() {
        let mut g = Generations::default();
        let a = g.next();
        let b = g.next();
        assert!(b > a);
        assert!(a > Generation(0));
    }

    // ghostty graphics_storage.zig:1573 "storage: no-op delete does not mark a mutation"
    #[test]
    fn storage_no_op_delete_does_not_mark_a_mutation() {
        let mut f = Fixture::new(3, 3, (0, 0));
        f.delete(Delete::All(Yes));
        assert!(!f.s.dirty);
        assert_eq!(f.s.generation, Generation(0));
        f.add(image(1, 0, 0));
        f.place(1, 1, 1, 1);
        let stamp = f.s.generation;
        f.s.dirty = false;
        f.delete(Delete::Id { free: No, image_id: 42, placement_id: 0 });
        assert!(!f.s.dirty);
        assert_eq!(f.s.generation, stamp);
        f.delete(Delete::Id { free: No, image_id: 1, placement_id: 0 });
        assert!(f.s.dirty);
        assert!(f.s.generation > stamp);
    }

    fn bytes_image(id: u32, n: usize) -> Image<'static> {
        let mut img = image(id, 1, 1);
        img.data.try_extend_from_slice(&std::vec![0u8; n]).unwrap();
        img
    }

    /// Inventory §9 "Limits": the byte cap evicts unused images first, then the oldest; an
    /// image over the whole cap is refused.
    #[test]
    fn storage_byte_cap_and_eviction_order() {
        let mut f = Fixture::new(3, 3, (0, 0));
        f.s.set_limit(StorageLimit(10), &mut f.t, &mut f.g).unwrap();
        f.add(bytes_image(1, 4));
        f.add(bytes_image(2, 4));
        f.place(1, 1, 0, 0);
        // 4 + 4 + 4 > 10: the unused image 2 goes, though image 1 is older.
        f.add(bytes_image(3, 4));
        assert!(f.s.image_by_id(2).is_none());
        assert!(f.s.image_by_id(1).is_some());
        assert_eq!(f.s.total_bytes(), 8);
        // Then the oldest unused (3), still short of room... and the used image 1 with its pin.
        f.add(bytes_image(4, 9));
        assert_eq!(f.s.image_count(), 1);
        assert_eq!(f.t.tracked(), 0);
        assert_eq!(f.s.total_bytes(), 9);
        assert_eq!(f.s.add_image(bytes_image(5, 11), &mut f.t, &mut f.g), Err(OutOfMemory));
        // Replacing an image counts its old bytes out.
        f.add(bytes_image(4, 2));
        assert_eq!(f.s.total_bytes(), 2);
    }

    #[test]
    fn storage_delete_z_and_cell_z_and_virtual() {
        let mut f = Fixture::hundred();
        let virt = Placement { z: 5, ..Placement::at(Location::Virtual) };
        f.s.add_placement(2, 9, virt, &mut f.g).unwrap();
        // d=z skips virtual placements.
        f.delete(Delete::Z { free: No, z: 5 });
        assert_eq!(f.s.placement_count(), 3);
        // d=a skips virtual placements too.
        f.delete(Delete::All(No));
        assert_eq!(f.s.placement_count(), 1);
        // d=i with placement 0 takes virtual ones.
        f.delete(Delete::Id { free: Yes, image_id: 2, placement_id: 0 });
        assert_eq!((f.s.placement_count(), f.s.image_count()), (0, 1));
        // d=q matches the z-index of a placement over the cell.
        let mut f = Fixture::hundred();
        f.delete(Delete::IntersectCellZ { free: No, x: 1, y: 1, z: 1 });
        assert_eq!(f.s.placement_count(), 2);
        f.delete(Delete::IntersectCellZ { free: No, x: 1, y: 1, z: 0 });
        assert_eq!(f.s.placement_count(), 1);
        // d=p at a 1-based cell; 0 does nothing.
        f.delete(Delete::IntersectCell { free: No, x: 0, y: 30 });
        assert_eq!(f.s.placement_count(), 1);
        f.delete(Delete::IntersectCell { free: No, x: 30, y: 30 });
        assert_eq!(f.s.placement_count(), 0);
        // d=n by number.
        let mut f = Fixture::new(3, 3, (0, 0));
        f.add(Image { number: 4, ..image(1, 0, 0) });
        f.place(1, 0, 0, 0);
        f.delete(Delete::Newest { free: Yes, image_number: 4, placement_id: 0 });
        assert_eq!((f.s.placement_count(), f.s.image_count()), (0, 0));
    }

    #[test]
    fn storage_rect_stops_at_screen_edges() {
        let mut t = FakeScreen::new(10, 5).px(10, 5);
        let img = image(1, 30, 30);
        let p = Placement::at(Location::Pin(t.track(8, 3)));
        let r = p.rect(&img, &t).unwrap();
        assert_eq!(r.top_left, ScreenPoint { row: 3, col: 8 });
        assert_eq!(r.bottom_right, ScreenPoint { row: 4, col: 9 });
        assert!(Placement::at(Location::Virtual).rect(&img, &t).is_none());
    }
}
