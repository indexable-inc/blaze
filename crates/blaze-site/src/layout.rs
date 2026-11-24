//! Pure `#![no_std]`, zero-recursion, zero-unsafe port of the Blaze front page's typography &
//! block layout engine (`site/client/lib/page/layout.ts`) + text selection & Cmd+F search helpers.

use core::alloc::Allocator;

use blaze_own::{String as OwnString, Vec as OwnVec};
use blaze_ui::slug::SlugFontTable;
use blaze_ui::types::Rect;

use crate::error::SiteError;
use crate::model::{Block, ChipKind, PageModel, TipKind, Word};
use crate::palette::{ImageName, RoleName, Token};

pub const NARROW: f32 = 560.0;
pub const MAX_COL_WIDTH: f32 = 640.0;
pub const PAD: f32 = 24.0;
pub const NOTE_RULE: f32 = 40.0;
pub const CHIP_X: f32 = 0.42;
pub const CHIP_Y: f32 = 0.14;
pub const UNDERLINE_AT: f32 = 3.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    Box,
    Button,
    Chip,
    Bar,
    Rule,
    Dot,
    Underline,
    Divider,
    PartPlate,
    LinkPlate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub rect: Rect,
    pub r: f32,
    pub fill: Token,
    pub alpha: f32,
    pub ring: Option<Token>,
    pub hollow: bool,
    pub kind: ShapeKind,
    pub link: Option<u32>,
    pub part: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pic {
    pub rect: Rect,
    pub image: ImageName,
    pub ink: Option<Token>,
    pub link: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedGlyph {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub glyph: u32,
    pub role: RoleName,
    pub link: Option<u32>,
    pub part: Option<u32>,
    pub char_idx: Option<u32>,
    /// `Some(0)` = "Copy", `Some(1)` = "Copied".
    pub button: Option<u8>,
    pub scramble: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharBox {
    pub x0: f32,
    pub x1: f32,
    pub top: f32,
    pub bottom: f32,
    pub ch: char,
    /// Optional separator emitted before this char when copying (`'\n'` or `' '`).
    pub before: Option<char>,
    pub block: u32,
    pub item: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CharRange {
    pub from: usize,
    pub to: usize,
}

pub struct NoteRect<A: Allocator + Copy> {
    pub id: OwnString<A>,
    pub rect: Rect,
}

/// Complete laid-out page in CSS pixels from column top-left.
pub struct PageLayout<A: Allocator + Copy> {
    pub width: f32,
    pub height: f32,
    pub shapes: OwnVec<Shape, A>,
    pub pics: OwnVec<Pic, A>,
    pub glyphs: OwnVec<PlacedGlyph, A>,
    pub chars: OwnVec<CharBox, A>,
    pub links: OwnVec<OwnVec<Rect, A>, A>,
    pub parts: OwnVec<Option<Rect>, A>,
    pub button: Option<Rect>,
    pub command: Option<Rect>,
    pub notes: OwnVec<NoteRect<A>, A>,
}

impl<A: Allocator + Copy> PageLayout<A> {
    pub fn empty_in(alloc: A) -> Self {
        Self {
            width: MAX_COL_WIDTH,
            height: 800.0,
            shapes: OwnVec::new_in(alloc),
            pics: OwnVec::new_in(alloc),
            glyphs: OwnVec::new_in(alloc),
            chars: OwnVec::new_in(alloc),
            links: OwnVec::new_in(alloc),
            parts: OwnVec::new_in(alloc),
            button: None,
            command: None,
            notes: OwnVec::new_in(alloc),
        }
    }

    pub fn note_rect(&self, id: &str) -> Option<Rect> {
        for n in self.notes.as_slice() {
            if n.id.as_str() == id {
                return Some(n.rect);
            }
        }
        None
    }
}

/// Laid-out floating hover card in card-local CSS pixels.
pub struct TipLayout<A: Allocator + Copy> {
    pub w: f32,
    pub h: f32,
    pub shapes: OwnVec<Shape, A>,
    pub pics: OwnVec<Pic, A>,
    pub glyphs: OwnVec<PlacedGlyph, A>,
    pub link_rects: OwnVec<(u32, Rect), A>,
}

#[derive(Clone, Copy)]
struct LayoutCtx<'a, A: Allocator + Copy> {
    narrow: bool,
    title_size: f32,
    table: &'a SlugFontTable<A>,
}

impl<'a, A: Allocator + Copy> LayoutCtx<'a, A> {
    fn new(win: Viewport, table: &'a SlugFontTable<A>) -> Self {
        let narrow = win.width <= NARROW;
        let title_size = (0.052 * win.width).clamp(32.0, 40.0);
        Self { narrow, title_size, table }
    }

    fn size(&self, role: RoleName) -> f32 {
        match role {
            RoleName::Title => self.title_size,
            RoleName::TitleMark => 0.62 * self.title_size,
            _ => {
                let s = role.spec();
                if self.narrow {
                    s.narrow.unwrap_or(s.size)
                } else {
                    s.size
                }
            }
        }
    }

    fn metrics(&self, role: RoleName, size: f32) -> (f32, f32, f32) {
        let spec = role.spec();
        let (asc, desc) = match self.table.find_instance(spec.family, spec.wght, spec.opsz) {
            Some(inst) => (inst.ascent, inst.descent),
            None => (0.92, -0.24),
        };
        (
            gpu::math::round(asc * size),
            gpu::math::round((-desc) * size),
            spec.lead * size,
        )
    }
}

#[derive(Clone, Copy)]
struct WordBox {
    word_idx: usize,
    size: f32,
    lead: f32,
    body: f32,
    trail: f32,
    gap: f32,
    brk: bool,
    x: f32,
}

impl WordBox {
    #[inline]
    fn span(&self) -> f32 {
        self.lead + self.body + self.trail
    }
}

fn measure_boxes<A: Allocator + Copy>(
    ctx: LayoutCtx<'_, A>,
    words: &[Word<A>],
    alloc: A,
) -> Result<OwnVec<WordBox, A>, SiteError> {
    let mut out = OwnVec::try_with_capacity_in(words.len(), alloc)?;
    for (i, w) in words.iter().enumerate() {
        let size = ctx.size(w.role);
        let prev = if i > 0 { words.get(i - 1) } else { None };
        let next = words.get(i + 1);
        let in_chip = |other: Option<&Word<A>>| -> bool {
            match (w.chip, other.and_then(|o| o.chip)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        };
        let gap = w.space * size;
        let lead = if w.chip.is_some() && !in_chip(prev) { CHIP_X * size } else { 0.0 };
        let trail = if w.chip.is_some() && !in_chip(next) { CHIP_X * size } else { 0.0 };
        let brk = gap > 0.0 && next.is_some() && !in_chip(next) && w.image.is_none();
        out.try_push(WordBox { word_idx: i, size, lead, body: w.adv * size, trail, gap, brk, x: 0.0 })
            .map_err(|r| SiteError::Alloc(r.why))?;
    }
    Ok(out)
}

fn place_line(line: &mut [WordBox]) -> f32 {
    let mut x = 0.0f32;
    for b in line.iter_mut() {
        b.x = x;
        x += b.span() + b.gap;
    }
    match line.last() {
        Some(last) => last.x + last.span(),
        None => 0.0,
    }
}

/// Greedy line breaking matching `layout.ts::breakLines` (never breaks inside a chip or after an inline icon).
fn break_lines<A: Allocator + Copy>(
    bs: &[WordBox],
    max_w: f32,
    alloc: A,
) -> Result<OwnVec<OwnVec<WordBox, A>, A>, SiteError> {
    let mut lines: OwnVec<OwnVec<WordBox, A>, A> = OwnVec::new_in(alloc);
    let mut line: OwnVec<WordBox, A> = OwnVec::new_in(alloc);
    let mut unit: OwnVec<WordBox, A> = OwnVec::new_in(alloc);
    let mut x = 0.0f32;
    let mut gap = 0.0f32;

    for (i, &b) in bs.iter().enumerate() {
        unit.try_push(b).map_err(|r| SiteError::Alloc(r.why))?;
        if b.brk || i + 1 == bs.len() {
            let mut uw = 0.0f32;
            for (ui, ub) in unit.as_slice().iter().enumerate() {
                uw += ub.span() + if ui + 1 < unit.len() { ub.gap } else { 0.0 };
            }
            if !line.is_empty() && x + gap + uw > max_w + 0.01 {
                let mut finished = OwnVec::new_in(alloc);
                core::mem::swap(&mut finished, &mut line);
                place_line(finished.as_mut_slice());
                lines.try_push(finished).map_err(|r| SiteError::Alloc(r.why))?;
                x = 0.0;
            }
            if !line.is_empty() {
                x += gap;
            }
            for &ub in unit.as_slice() {
                line.try_push(ub).map_err(|r| SiteError::Alloc(r.why))?;
            }
            x += uw;
            gap = unit.last().map_or(0.0, |u| u.gap);
            unit.clear();
        }
    }
    if !line.is_empty() {
        place_line(line.as_mut_slice());
        lines.try_push(line).map_err(|r| SiteError::Alloc(r.why))?;
    }
    Ok(lines)
}

/// `text-wrap: balance` bisection to quarter-pixel precision.
fn balance_width<A: Allocator + Copy>(bs: &[WordBox], max_w: f32, alloc: A) -> Result<f32, SiteError> {
    let count = break_lines(bs, max_w, alloc)?.len();
    if count <= 1 {
        return Ok(max_w);
    }
    let mut lo = 0.0f32;
    let mut hi = max_w;
    while hi - lo > 0.25 {
        let mid = (lo + hi) * 0.5;
        if break_lines(bs, mid, alloc)?.len() <= count {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(hi)
}

fn vertical_metrics<A: Allocator + Copy>(
    ctx: LayoutCtx<'_, A>,
    words: &[Word<A>],
    line: &[WordBox],
    base: RoleName,
    override_lh: Option<f32>,
) -> (f32, f32) {
    let s = ctx.size(base);
    let (a, d, lh) = ctx.metrics(base, s);
    let half = (override_lh.unwrap_or(lh) - a - d) * 0.5;
    let mut above = a + half;
    let mut below = d + half;
    for b in line {
        let Some(w) = words.get(b.word_idx) else { continue };
        if w.image.is_some() {
            above = above.max(w.rise * b.size);
            below = below.max(w.drop * b.size);
            continue;
        }
        let (wa, wd, wlh) = ctx.metrics(w.role, b.size);
        let h = (wlh - wa - wd) * 0.5;
        above = above.max(wa + h);
        below = below.max(wd + h);
    }
    (above, below)
}

#[derive(Clone, Copy)]
struct EmitOpts {
    final_line: bool,
    base_size: f32,
    button: Option<u8>,
    tail_to: Option<f32>,
}

struct LayoutOut<'a, A: Allocator + Copy> {
    alloc: A,
    ctx: LayoutCtx<'a, A>,
    shapes: OwnVec<Shape, A>,
    pics: OwnVec<Pic, A>,
    glyphs: OwnVec<PlacedGlyph, A>,
    chars: OwnVec<CharBox, A>,
    links: OwnVec<OwnVec<Rect, A>, A>,
    parts: OwnVec<Option<Rect>, A>,
    block: u32,
    item: Option<u32>,
    sep: Option<char>,
}

impl<'a, A: Allocator + Copy> LayoutOut<'a, A> {
    fn new(ctx: LayoutCtx<'a, A>, link_count: usize, alloc: A) -> Result<Self, SiteError> {
        let mut links = OwnVec::try_with_capacity_in(link_count, alloc)?;
        for _ in 0..link_count {
            links.try_push(OwnVec::new_in(alloc)).map_err(|r| SiteError::Alloc(r.why))?;
        }
        Ok(Self {
            alloc,
            ctx,
            shapes: OwnVec::new_in(alloc),
            pics: OwnVec::new_in(alloc),
            glyphs: OwnVec::new_in(alloc),
            chars: OwnVec::new_in(alloc),
            links,
            parts: OwnVec::new_in(alloc),
            block: 0,
            item: None,
            sep: None,
        })
    }

    fn row(&mut self, sep: char) {
        self.sep = Some(sep);
    }

    fn push_char(&mut self, x0: f32, x1: f32, top: f32, bottom: f32, ch: char) -> Result<u32, SiteError> {
        let before = if !self.chars.is_empty() { self.sep.take() } else { self.sep = None; None };
        let idx = u32::try_from(self.chars.len()).unwrap_or(0);
        self.chars
            .try_push(CharBox { x0, x1, top, bottom, ch, before, block: self.block, item: self.item })
            .map_err(|r| SiteError::Alloc(r.why))?;
        Ok(idx)
    }

    fn emit_line(
        &mut self,
        words: &[Word<A>],
        line: &[WordBox],
        left: f32,
        top: f32,
        above: f32,
        below: f32,
        e: EmitOpts,
    ) -> Result<(), SiteError> {
        let baseline = top + above;
        let bottom = baseline + below;

        for (i, b) in line.iter().enumerate() {
            let Some(w) = words.get(b.word_idx) else { continue };
            let x = left + b.x + b.lead;
            if let Some(img) = w.image {
                let ink = if img.is_mask() {
                    let sp = w.role.spec();
                    Some(if w.link.is_some() { sp.link.map_or(sp.ink, |l| l.ink) } else { sp.ink })
                } else {
                    None
                };
                self.pics
                    .try_push(Pic {
                        rect: Rect::new(x, baseline - w.rise * b.size, b.body, (w.rise + w.drop) * b.size),
                        image: img,
                        ink,
                        link: w.link,
                    })
                    .map_err(|r| SiteError::Alloc(r.why))?;
            } else {
                let y = baseline - w.raise * e.base_size;
                for (k, ch) in w.text.as_str().chars().enumerate() {
                    let gx = x + w.xs.get(k).copied().unwrap_or(0.0) * b.size;
                    let gx1 = x + w.xs.get(k + 1).copied().unwrap_or(w.adv) * b.size;
                    let char_idx = if !w.deco && e.button.is_none() {
                        Some(self.push_char(gx, gx1, top, bottom, ch)?)
                    } else {
                        None
                    };
                    let g = w.glyphs.get(k).copied().unwrap_or(-1);
                    if g < 0 {
                        continue;
                    }
                    let ku = u32::try_from(k).unwrap_or(u32::MAX);
                    let is_scr = w.scramble.as_slice().iter().any(|&s| s == ku);
                    self.glyphs
                        .try_push(PlacedGlyph {
                            x: gx,
                            y,
                            size: b.size,
                            glyph: u32::try_from(g).unwrap_or(0),
                            role: w.role,
                            link: w.link,
                            part: w.part,
                            char_idx,
                            button: e.button,
                            scramble: is_scr,
                        })
                        .map_err(|r| SiteError::Alloc(r.why))?;
                }
            }
            let is_last = i + 1 == line.len();
            if b.gap > 0.0 && !w.deco && w.image.is_none() && e.button.is_none() && !(is_last && e.final_line) {
                let x0 = left + b.x + b.span();
                let x1 = if is_last { e.tail_to.unwrap_or(x0 + b.gap) } else { x0 + b.gap };
                let _ = self.push_char(x0, x1, top, bottom, ' ')?;
            }
        }

        // Chips & badges
        let mut i = 0usize;
        while i < line.len() {
            let chip = line.get(i).and_then(|b| words.get(b.word_idx)).and_then(|w| w.chip);
            let Some(cid) = chip else {
                i += 1;
                continue;
            };
            let mut j = i;
            while j + 1 < line.len()
                && line.get(j + 1).and_then(|b| words.get(b.word_idx)).and_then(|w| w.chip) == Some(cid)
            {
                j += 1;
            }
            let (Some(&bi), Some(&bj)) = (line.get(i), line.get(j)) else {
                i = j + 1;
                continue;
            };
            let mut first_b = bi;
            for k in i..=j {
                if let Some(b) = line.get(k) {
                    if let Some(w) = words.get(b.word_idx) {
                        if w.image.is_none() && !w.deco {
                            first_b = *b;
                            break;
                        }
                    }
                }
            }
            let Some(fw) = words.get(first_b.word_idx) else {
                i = j + 1;
                continue;
            };
            let (ma, md, _) = self.ctx.metrics(fw.role, first_b.size);
            let x0 = left + bi.x;
            let x1 = left + bj.x + bj.span();
            let h = ma + md + 2.0 * CHIP_Y * first_b.size;
            let is_badge = fw.chip_kind == Some(ChipKind::Badge);
            let shape = if is_badge {
                Shape {
                    rect: Rect::new(x0, baseline - ma - CHIP_Y * first_b.size, x1 - x0, h),
                    r: h * 0.5,
                    fill: Token::Bg,
                    alpha: 1.0,
                    ring: Some(Token::Line2),
                    hollow: false,
                    kind: ShapeKind::Chip,
                    link: fw.link,
                    part: None,
                }
            } else {
                Shape {
                    rect: Rect::new(x0, baseline - ma - CHIP_Y * first_b.size, x1 - x0, h),
                    r: 6.0,
                    fill: Token::STint,
                    alpha: 1.0,
                    ring: None,
                    hollow: false,
                    kind: ShapeKind::Chip,
                    link: fw.link,
                    part: None,
                }
            };
            self.shapes.try_push(shape).map_err(|r| SiteError::Alloc(r.why))?;
            i = j + 1;
        }

        // Link hit boxes
        let mut i = 0usize;
        while i < line.len() {
            let link = line.get(i).and_then(|b| words.get(b.word_idx)).and_then(|w| w.link);
            let Some(lid) = link else { i += 1; continue; };
            let mut j = i;
            while j + 1 < line.len()
                && line.get(j + 1).and_then(|b| words.get(b.word_idx)).and_then(|w| w.link) == Some(lid)
            {
                j += 1;
            }
            if let (Some(&bi), Some(&bj), Some(list)) = (
                line.get(i),
                line.get(j),
                self.links.get_mut(usize::try_from(lid).unwrap_or(usize::MAX)),
            ) {
                let r = Rect::new(left + bi.x, top, bj.x + bj.span() - bi.x, bottom - top);
                list.try_push(r).map_err(|e| SiteError::Alloc(e.why))?;
            }
            i = j + 1;
        }

        // Link underlines
        let mut i = 0usize;
        while i < line.len() {
            let key = |idx: usize| -> Option<u32> {
                let b = line.get(idx)?;
                let w = words.get(b.word_idx)?;
                if w.image.is_none() && w.role.spec().link.is_some_and(|l| l.underline) {
                    w.link
                } else {
                    None
                }
            };
            let Some(lid) = key(i) else { i += 1; continue; };
            let mut j = i;
            while j + 1 < line.len() && key(j + 1) == Some(lid) {
                j += 1;
            }
            if let (Some(&bi), Some(&bj)) = (line.get(i), line.get(j)) {
                let x0 = left + bi.x + bi.lead;
                let x1 = left + bj.x + bj.lead + bj.body;
                self.shapes
                    .try_push(Shape {
                        rect: Rect::new(x0, baseline + UNDERLINE_AT, x1 - x0, 1.0),
                        r: 0.0,
                        fill: Token::Line2,
                        alpha: 1.0,
                        ring: None,
                        hollow: false,
                        kind: ShapeKind::Underline,
                        link: Some(lid),
                        part: None,
                    })
                    .map_err(|r| SiteError::Alloc(r.why))?;
            }
            i = j + 1;
        }

        // Hover plates for links inside Rust code chips (`Cx`, `Out`, `Dir`, `ReadOnly`)
        let mut i = 0usize;
        while i < line.len() {
            let key = |idx: usize| -> Option<u32> {
                let b = line.get(idx)?;
                let w = words.get(b.word_idx)?;
                if w.chip.is_some() && w.image.is_none() { w.link } else { None }
            };
            let Some(lid) = key(i) else { i += 1; continue; };
            let mut j = i;
            while j + 1 < line.len() && key(j + 1) == Some(lid) {
                j += 1;
            }
            if let (Some(&bi), Some(&bj)) = (line.get(i), line.get(j)) {
                if let Some(w0) = words.get(bi.word_idx) {
                    let (ma, md, _) = self.ctx.metrics(w0.role, bi.size);
                    let x0 = left + bi.x + bi.lead;
                    let x1 = left + bj.x + bj.lead + bj.body;
                    self.shapes
                        .try_push(Shape {
                            rect: Rect::new(x0 - 2.0, baseline - ma - 2.0, x1 - x0 + 4.0, ma + md + 4.0),
                            r: 4.0,
                            fill: Token::STint,
                            alpha: 1.0,
                            ring: Some(Token::Line2),
                            hollow: false,
                            kind: ShapeKind::LinkPlate,
                            link: Some(lid),
                            part: None,
                        })
                        .map_err(|r| SiteError::Alloc(r.why))?;
                }
            }
            i = j + 1;
        }

        // Command part hit rects + hover plates
        let mut i = 0usize;
        while i < line.len() {
            let pid_opt = line.get(i).and_then(|b| words.get(b.word_idx)).and_then(|w| w.part);
            let Some(pid) = pid_opt else { i += 1; continue; };
            let mut j = i;
            while j + 1 < line.len()
                && line.get(j + 1).and_then(|b| words.get(b.word_idx)).and_then(|w| w.part) == Some(pid)
            {
                j += 1;
            }
            if let (Some(&bi), Some(&bj)) = (line.get(i), line.get(j)) {
                if let Some(w0) = words.get(bi.word_idx) {
                    let (ma, md, _) = self.ctx.metrics(w0.role, bi.size);
                    let x0 = left + bi.x + bi.lead;
                    let x1 = left + bj.x + bj.lead + bj.body;
                    let pbox = Rect::new(x0, baseline - ma, x1 - x0, ma + md);
                    let pid_u = usize::try_from(pid).unwrap_or(0);
                    while self.parts.len() <= pid_u {
                        self.parts.try_push(None).map_err(|r| SiteError::Alloc(r.why))?;
                    }
                    if let Some(slot) = self.parts.get_mut(pid_u) {
                        *slot = Some(pbox);
                    }
                    self.shapes
                        .try_push(Shape {
                            rect: Rect::new(pbox.x - 3.0, pbox.y - 3.0, pbox.w + 6.0, pbox.h + 6.0),
                            r: 7.0,
                            fill: Token::STint,
                            alpha: 1.0,
                            ring: None,
                            hollow: false,
                            kind: ShapeKind::PartPlate,
                            link: None,
                            part: Some(pid),
                        })
                        .map_err(|r| SiteError::Alloc(r.why))?;
                }
            }
            i = j + 1;
        }

        Ok(())
    }

    fn para(
        &mut self,
        words: &[Word<A>],
        base: RoleName,
        left: f32,
        width: f32,
        top: f32,
        balanced: bool,
    ) -> Result<f32, SiteError> {
        let bs = measure_boxes(self.ctx, words, self.alloc)?;
        let max_w = if balanced { balance_width(bs.as_slice(), width, self.alloc)? } else { width };
        let lines = break_lines(bs.as_slice(), max_w, self.alloc)?;
        let mut y = top;
        let n = lines.len();
        for (i, line) in lines.as_slice().iter().enumerate() {
            let (above, below) = vertical_metrics(self.ctx, words, line.as_slice(), base, None);
            self.emit_line(
                words,
                line.as_slice(),
                left,
                y,
                above,
                below,
                EmitOpts { final_line: i + 1 == n, base_size: self.ctx.size(base), button: None, tail_to: None },
            )?;
            y += above + below;
        }
        Ok(y)
    }
}

fn layout_flex<A: Allocator + Copy>(
    out: &mut LayoutOut<'_, A>,
    words: &[Word<A>],
    left: f32,
    right: Option<f32>,
    top: f32,
    width: f32,
    gap: f32,
    wrap: bool,
    row_h: Option<f32>,
    base: RoleName,
) -> Result<f32, SiteError> {
    let alloc = out.alloc;
    // Group consecutive words sharing a link
    let mut groups: OwnVec<(usize, usize), A> = OwnVec::new_in(alloc);
    let mut i = 0usize;
    while i < words.len() {
        let mut j = i + 1;
        if let Some(lid) = words.get(i).and_then(|w| w.link) {
            while j < words.len() && words.get(j).and_then(|w| w.link) == Some(lid) {
                j += 1;
            }
        }
        groups.try_push((i, j)).map_err(|r| SiteError::Alloc(r.why))?;
        i = j;
    }

    struct FlexItem<A: Allocator + Copy> {
        w_start: usize,
        w_end: usize,
        bs: OwnVec<WordBox, A>,
        w: f32,
        above: f32,
        below: f32,
    }

    let mut items: OwnVec<FlexItem<A>, A> = OwnVec::new_in(alloc);
    for &(gs, ge) in groups.as_slice() {
        let slice = words.get(gs..ge).unwrap_or(&[]);
        let mut bs = measure_boxes(out.ctx, slice, alloc)?;
        let w = place_line(bs.as_mut_slice());
        let (above, below) = vertical_metrics(out.ctx, slice, bs.as_slice(), base, None);
        items
            .try_push(FlexItem { w_start: gs, w_end: ge, bs, w, above, below })
            .map_err(|r| SiteError::Alloc(r.why))?;
    }

    // Partition into rows
    let mut row_ranges: OwnVec<(usize, usize), A> = OwnVec::new_in(alloc);
    let mut rs = 0usize;
    let mut rx = 0.0f32;
    for (idx, it) in items.as_slice().iter().enumerate() {
        if idx > rs && wrap && rx + gap + it.w > width + 0.01 {
            row_ranges.try_push((rs, idx)).map_err(|r| SiteError::Alloc(r.why))?;
            rs = idx;
            rx = it.w;
        } else {
            if idx > rs {
                rx += gap;
            }
            rx += it.w;
        }
    }
    if rs < items.len() {
        row_ranges.try_push((rs, items.len())).map_err(|r| SiteError::Alloc(r.why))?;
    }

    let mut y = top;
    let nr = row_ranges.len();
    for (r_idx, &(a, b)) in row_ranges.as_slice().iter().enumerate() {
        let mut max_h = 0.0f32;
        let mut total_w = 0.0f32;
        for idx in a..b {
            if let Some(it) = items.get(idx) {
                max_h = max_h.max(it.above + it.below);
                if idx > a {
                    total_w += gap;
                }
                total_w += it.w;
            }
        }
        let rh = row_h.unwrap_or(max_h);
        let mut at_x = match right {
            Some(r) => r - total_w,
            None => left,
        };
        for idx in a..b {
            let Some(it) = items.get(idx) else { continue };
            let lh = it.above + it.below;
            let next_x = at_x + it.w + gap;
            let final_line = r_idx + 1 == nr && idx + 1 == b;
            let tail_to = if final_line {
                None
            } else if idx + 1 < b {
                Some(next_x)
            } else {
                Some(at_x + it.w + it.bs.last().map_or(0.0, |wb| wb.gap))
            };
            let slice = words.get(it.w_start..it.w_end).unwrap_or(&[]);
            out.emit_line(
                slice,
                it.bs.as_slice(),
                at_x,
                y + (rh - lh) * 0.5,
                it.above,
                it.below,
                EmitOpts { final_line, base_size: out.ctx.size(base), button: None, tail_to },
            )?;
            at_x = next_x;
        }
        y += rh + if r_idx + 1 < nr { gap } else { 0.0 };
    }
    Ok(y)
}

/// Lay out the entire front page at column `width` for viewport `win`.
pub fn layout_page<A: Allocator + Copy>(
    model: &PageModel<A>,
    width: f32,
    table: &SlugFontTable<A>,
    win: Viewport,
    alloc: A,
) -> Result<PageLayout<A>, SiteError> {
    let ctx = LayoutCtx::new(win, table);
    let mut out = LayoutOut::new(ctx, model.links.len(), alloc)?;
    let cw = (width - 2.0 * PAD).max(160.0);
    let right = PAD + cw;
    let mut y = 0.0f32;
    let mut pend = 0.0f32;
    let mut button: Option<Rect> = None;
    let mut command: Option<Rect> = None;
    let mut notes: OwnVec<NoteRect<A>, A> = OwnVec::new_in(alloc);

    let mut prev_was_label = false;
    let mut prev_was_command = false;

    for (bi, block) in model.blocks.as_slice().iter().enumerate() {
        out.block = u32::try_from(bi).unwrap_or(0);
        out.item = None;
        out.row('\n');

        let cur_is_label = matches!(block, Block::Label { .. });
        let cur_is_command = matches!(block, Block::Command { .. });

        match block {
            Block::Bar { name, links, backed } => {
                let mut nbs = measure_boxes(ctx, name.as_slice(), alloc)?;
                place_line(nbs.as_mut_slice());
                let (na, nb) = vertical_metrics(ctx, name.as_slice(), nbs.as_slice(), RoleName::Wordmark, Some(72.0));
                out.emit_line(
                    name.as_slice(),
                    nbs.as_slice(),
                    PAD,
                    y,
                    na,
                    nb,
                    EmitOpts { final_line: true, base_size: ctx.size(RoleName::Wordmark), button: None, tail_to: None },
                )?;

                if ctx.narrow {
                    out.row('\n');
                    let _ = layout_flex(&mut out, links.as_slice(), PAD, Some(right), y, cw, 16.0, false, Some(72.0), RoleName::Bar)?;
                    out.row('\n');
                    let mut bbs = measure_boxes(ctx, backed.as_slice(), alloc)?;
                    place_line(bbs.as_mut_slice());
                    let (ba, bb) = vertical_metrics(ctx, backed.as_slice(), bbs.as_slice(), RoleName::Backed, None);
                    out.emit_line(
                        backed.as_slice(),
                        bbs.as_slice(),
                        PAD,
                        y + 52.0,
                        ba,
                        bb,
                        EmitOpts { final_line: true, base_size: ctx.size(RoleName::Backed), button: None, tail_to: None },
                    )?;
                } else {
                    out.row('\n');
                    let mut bbs = measure_boxes(ctx, backed.as_slice(), alloc)?;
                    place_line(bbs.as_mut_slice());
                    let (ba, bb) = vertical_metrics(ctx, backed.as_slice(), bbs.as_slice(), RoleName::Backed, None);
                    out.shapes
                        .try_push(Shape {
                            rect: Rect::new(88.0, y + 29.0, 1.0, 14.0),
                            r: 0.0,
                            fill: Token::Line2,
                            alpha: 1.0,
                            ring: None,
                            hollow: false,
                            kind: ShapeKind::Divider,
                            link: None,
                            part: None,
                        })
                        .map_err(|r| SiteError::Alloc(r.why))?;
                    out.emit_line(
                        backed.as_slice(),
                        bbs.as_slice(),
                        88.0 + 15.0,
                        y + (72.0 - ba - bb) * 0.5,
                        ba,
                        bb,
                        EmitOpts { final_line: true, base_size: ctx.size(RoleName::Backed), button: None, tail_to: None },
                    )?;
                    out.row('\n');
                    let _ = layout_flex(&mut out, links.as_slice(), PAD, Some(right), y, cw, 22.0, false, Some(72.0), RoleName::Bar)?;
                }
                y += 72.0;
            }
            Block::Title { words } => {
                let m = if ctx.narrow { 56.0 } else { (0.06 * win.height).clamp(32.0, 80.0) };
                y += pend.max(m);
                pend = 0.0;
                y = out.para(words.as_slice(), RoleName::Title, PAD, cw, y, true)?;
                pend = pend.max(22.0);
            }
            Block::Label { words } => {
                y += pend.max(30.0);
                pend = 0.0;
                y = out.para(words.as_slice(), RoleName::Label, PAD, cw, y, false)?;
            }
            Block::Command { words, button: btn_words, copied: cpd_words, .. } => {
                let m = if prev_was_label { 10.0 } else { 28.0 };
                y += pend.max(m);
                pend = 0.0;
                let h = if ctx.narrow { 52.0 } else { 60.0 };
                let cmd_r = Rect::new(PAD, y, cw, h);
                command = Some(cmd_r);
                out.shapes
                    .try_push(Shape {
                        rect: cmd_r,
                        r: 14.0,
                        fill: Token::Bg2,
                        alpha: 1.0,
                        ring: Some(Token::Line),
                        hollow: false,
                        kind: ShapeKind::Box,
                        link: None,
                        part: None,
                    })
                    .map_err(|r| SiteError::Alloc(r.why))?;

                let pad = if ctx.narrow { 10.0 } else { 14.0 };
                let mut l0 = measure_boxes(ctx, btn_words.as_slice(), alloc)?;
                let mut l1 = measure_boxes(ctx, cpd_words.as_slice(), alloc)?;
                let w0 = place_line(l0.as_mut_slice());
                let w1 = place_line(l1.as_mut_slice());
                let bw = w0.max(w1) + 2.0 * pad;
                let bh = if ctx.narrow { 34.0 } else { 36.0 };
                let btn_r = Rect::new(right - if ctx.narrow { 7.0 } else { 10.0 } - bw, y + (h - bh) * 0.5, bw, bh);
                button = Some(btn_r);
                out.shapes
                    .try_push(Shape {
                        rect: btn_r,
                        r: 9.0,
                        fill: Token::STint,
                        alpha: 1.0,
                        ring: None,
                        hollow: false,
                        kind: ShapeKind::Button,
                        link: None,
                        part: None,
                    })
                    .map_err(|r| SiteError::Alloc(r.why))?;

                let (a0, b0) = vertical_metrics(ctx, btn_words.as_slice(), l0.as_slice(), RoleName::Button, None);
                out.emit_line(
                    btn_words.as_slice(),
                    l0.as_slice(),
                    btn_r.x + (bw - w0) * 0.5,
                    btn_r.y + (bh - a0 - b0) * 0.5,
                    a0,
                    b0,
                    EmitOpts { final_line: true, base_size: ctx.size(RoleName::Button), button: Some(0), tail_to: None },
                )?;
                let (a1, b1) = vertical_metrics(ctx, cpd_words.as_slice(), l1.as_slice(), RoleName::Button, None);
                out.emit_line(
                    cpd_words.as_slice(),
                    l1.as_slice(),
                    btn_r.x + (bw - w1) * 0.5,
                    btn_r.y + (bh - a1 - b1) * 0.5,
                    a1,
                    b1,
                    EmitOpts { final_line: true, base_size: ctx.size(RoleName::Button), button: Some(1), tail_to: None },
                )?;

                let mut cbs = measure_boxes(ctx, words.as_slice(), alloc)?;
                place_line(cbs.as_mut_slice());
                let (ca, cb) = vertical_metrics(ctx, words.as_slice(), cbs.as_slice(), RoleName::Cmd, None);
                out.emit_line(
                    words.as_slice(),
                    cbs.as_slice(),
                    PAD + if ctx.narrow { 14.0 } else { 22.0 },
                    y + (h - ca - cb) * 0.5,
                    ca,
                    cb,
                    EmitOpts { final_line: true, base_size: ctx.size(RoleName::Cmd), button: None, tail_to: None },
                )?;
                y += h;
            }
            Block::List { items } => {
                y += pend.max(if prev_was_command { 36.0 } else { 0.0 });
                pend = 0.0;
                layout_item_list(model, items.as_slice(), false, PAD, right, &mut y, &mut pend, &mut out, alloc)?;
                out.item = None;
                pend = pend.max(20.0);
            }
            Block::Soon { label, items } => {
                y += pend.max(16.0);
                pend = 0.0;
                y = out.para(label.as_slice(), RoleName::Label, PAD, cw, y, false)?;
                y += pend.max(12.0);
                pend = 0.0;
                layout_item_list(model, items.as_slice(), true, PAD, right, &mut y, &mut pend, &mut out, alloc)?;
                out.item = None;
                pend = pend.max(20.0);
            }
            Block::Sign { image, closing, words, contact } => {
                if !closing.is_empty() {
                    y += pend.max(28.0);
                    pend = 0.0;
                    out.row('\n');
                    y = out.para(closing.as_slice(), RoleName::Sub, PAD, cw, y, false)? + 6.0;
                }
                y += pend.max(14.0);
                pend = 0.0;
                let w = 202.0f32.min(cw);
                let h = w / image.aspect();
                out.pics
                    .try_push(Pic { rect: Rect::new(PAD - 2.0, y, w, h), image: *image, ink: Some(Token::Ink1), link: None })
                    .map_err(|r| SiteError::Alloc(r.why))?;
                y += h + 4.0;
                out.row('\n');
                y = out.para(words.as_slice(), RoleName::Sign, PAD, cw, y, false)?;
                if !contact.is_empty() {
                    out.row('\n');
                    y = out.para(contact.as_slice(), RoleName::Sign, PAD, cw, y + 2.0, false)?;
                }
            }
            Block::Notes { notes: block_notes } => {
                y += pend.max(64.0);
                pend = 0.0;
                out.shapes
                    .try_push(Shape {
                        rect: Rect::new(PAD, y, NOTE_RULE, 1.0),
                        r: 0.0,
                        fill: Token::Line2,
                        alpha: 1.0,
                        ring: None,
                        hollow: false,
                        kind: ShapeKind::Rule,
                        link: None,
                        part: None,
                    })
                    .map_err(|r| SiteError::Alloc(r.why))?;
                y += 15.0;
                for (ni, n) in block_notes.as_slice().iter().enumerate() {
                    if ni > 0 {
                        y += 4.0;
                    }
                    out.item = Some(u32::try_from(ni + 1).unwrap_or(1));
                    out.row('\n');
                    let ny0 = y;
                    y = out.para(n.words.as_slice(), RoleName::Note, PAD, cw, y, false)?;
                    let mut id_copy = OwnString::new_in(alloc);
                    id_copy.try_push_str(n.id.as_str())?;
                    notes
                        .try_push(NoteRect { id: id_copy, rect: Rect::new(PAD, ny0, cw, y - ny0) })
                        .map_err(|r| SiteError::Alloc(r.why))?;
                }
                out.item = None;
            }
            Block::Footer { links } => {
                y += pend.max(48.0);
                pend = 0.0;
                let base = links.first().map_or(RoleName::Bar, |w| w.role);
                y = layout_flex(&mut out, links.as_slice(), PAD, None, y, cw, 22.0, true, None, base)?;
            }
        }

        prev_was_label = cur_is_label;
        prev_was_command = cur_is_command;
    }

    y += pend + 72.0;
    Ok(PageLayout {
        width,
        height: gpu::math::ceil(y),
        shapes: out.shapes,
        pics: out.pics,
        glyphs: out.glyphs,
        chars: out.chars,
        links: out.links,
        parts: out.parts,
        button,
        command,
        notes,
    })
}

/// Iterative DFS over nested bullet list items (zero recursion!).
fn layout_item_list<A: Allocator + Copy>(
    model: &PageModel<A>,
    roots: &[usize],
    ring_top: bool,
    start_x: f32,
    right: f32,
    y: &mut f32,
    pend: &mut f32,
    out: &mut LayoutOut<'_, A>,
    alloc: A,
) -> Result<(), SiteError> {
    enum Step {
        Enter { idx: usize, level: u8, x: f32, ring: bool },
        Exit { level: u8 },
    }
    let mut stack: OwnVec<Step, A> = OwnVec::new_in(alloc);
    let mut ri = roots.len();
    while ri > 0 {
        ri -= 1;
        if let Some(&idx) = roots.get(ri) {
            stack
                .try_push(Step::Enter { idx, level: 0, x: start_x, ring: ring_top })
                .map_err(|r| SiteError::Alloc(r.why))?;
        }
    }

    let mut item_counter: u32 = 0;
    while let Some(step) = stack.pop() {
        match step {
            Step::Exit { level } => {
                *pend = (*pend).max(if level == 0 { 14.0 } else { 4.0 });
            }
            Step::Enter { idx, level, x, ring } => {
                let Some(it) = model.items_arena.get(idx) else { continue };
                let base = if level == 0 {
                    RoleName::Item
                } else if !it.children.is_empty() {
                    RoleName::SubHead
                } else {
                    RoleName::Sub
                };
                let s = out.ctx.size(base);
                let tx = x + 1.15 * s + 4.0;
                *y += *pend;
                *pend = 0.0;
                out.item = Some(item_counter);
                item_counter = item_counter.saturating_add(1);
                out.row('\n');
                let line_top = *y;

                let dot = if level == 0 && ring {
                    Shape {
                        rect: Rect::new(tx - 0.95 * s - 0.5, line_top + 0.66 * s - 0.5, 6.0, 6.0),
                        r: 3.0,
                        fill: Token::Ink4,
                        alpha: 1.0,
                        ring: None,
                        hollow: true,
                        kind: ShapeKind::Dot,
                        link: None,
                        part: None,
                    }
                } else if level == 0 {
                    Shape {
                        rect: Rect::new(tx - 0.95 * s, line_top + 0.66 * s, 5.0, 5.0),
                        r: 2.5,
                        fill: Token::Ink4,
                        alpha: 1.0,
                        ring: None,
                        hollow: false,
                        kind: ShapeKind::Dot,
                        link: None,
                        part: None,
                    }
                } else if level == 1 {
                    Shape {
                        rect: Rect::new(tx - 1.05 * s, line_top + 0.6 * s, 4.0, 4.0),
                        r: 2.0,
                        fill: Token::Ink4,
                        alpha: 1.0,
                        ring: None,
                        hollow: true,
                        kind: ShapeKind::Dot,
                        link: None,
                        part: None,
                    }
                } else {
                    Shape {
                        rect: Rect::new(tx - 1.0 * s, line_top + 0.62 * s, 4.0, 4.0),
                        r: 0.5,
                        fill: Token::Ink4,
                        alpha: 1.0,
                        ring: None,
                        hollow: false,
                        kind: ShapeKind::Dot,
                        link: None,
                        part: None,
                    }
                };
                out.shapes.try_push(dot).map_err(|r| SiteError::Alloc(r.why))?;

                *y = out.para(it.words.as_slice(), base, tx, (right - tx).max(40.0), *y, false)?;

                // Push Exit, then children in reverse so first child pops first!
                stack.try_push(Step::Exit { level }).map_err(|r| SiteError::Alloc(r.why))?;
                if !it.children.is_empty() {
                    *pend = (*pend).max(4.0);
                    let mut ci = it.children.len();
                    while ci > 0 {
                        ci -= 1;
                        if let Some(&ch_idx) = it.children.get(ci) {
                            stack
                                .try_push(Step::Enter {
                                    idx: ch_idx,
                                    level: level.saturating_add(1),
                                    x: tx,
                                    ring: false,
                                })
                                .map_err(|r| SiteError::Alloc(r.why))?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Lay out a floating hover card (`TipLayout`) matching `site/client/lib/page/layout.ts::layoutTip`.
pub fn layout_tip<A: Allocator + Copy + 'static>(
    model: &PageModel<A>,
    key: &str,
    column: f32,
    table: &SlugFontTable<A>,
    alloc: A,
) -> Result<Option<TipLayout<A>>, SiteError> {
    let Some(entry) = model.find_tip(key) else {
        return Ok(None);
    };
    let ctx = LayoutCtx::new(Viewport { width: 10_000.0, height: 0.0 }, table);
    let mut out = LayoutOut::new(ctx, model.links.len(), alloc)?;
    // Card background plate at shapes[0]
    out.shapes
        .try_push(Shape {
            rect: Rect::ZERO,
            r: 11.0,
            fill: Token::Bg3,
            alpha: 1.0,
            ring: Some(Token::Line2),
            hollow: false,
            kind: ShapeKind::Box,
            link: None,
            part: None,
        })
        .map_err(|r| SiteError::Alloc(r.why))?;

    let max_inner = (288.0f32.min(column - 16.0) - 26.0).max(80.0);
    let inner: f32;
    let mut y = 10.0f32;

    match &entry.kind {
        TipKind::Text { title, body } => {
            let tw = match title {
                Some(tws) => {
                    let mut bs = measure_boxes(ctx, tws.as_slice(), alloc)?;
                    place_line(bs.as_mut_slice())
                }
                None => 0.0,
            };
            let mut bbs = measure_boxes(ctx, body.as_slice(), alloc)?;
            let bw = place_line(bbs.as_mut_slice());
            inner = max_inner.min(tw.max(bw));
            if let Some(tws) = title {
                y = out.para(tws.as_slice(), RoleName::TipTitle, 13.0, inner, y, true)? + 3.0;
            }
            y = out.para(body.as_slice(), RoleName::Tip, 13.0, inner, y, true)?;
        }
        TipKind::Rows { rows } => {
            let mut max_says = 0.0f32;
            let mut max_does = 0.0f32;
            for r in rows.as_slice() {
                let mut sb = measure_boxes(ctx, r.says.as_slice(), alloc)?;
                let mut db = measure_boxes(ctx, r.does.as_slice(), alloc)?;
                max_says = max_says.max(place_line(sb.as_mut_slice()));
                max_does = max_does.max(place_line(db.as_mut_slice()));
            }
            let gap = if max_says > 0.0 { 14.0 } else { 0.0 };
            inner = max_inner.min(max_says + gap + max_does);
            let dw = (inner - max_says - gap).max(20.0);

            for (ri, r) in rows.as_slice().iter().enumerate() {
                if ri > 0 {
                    y += 5.0;
                }
                let sb = measure_boxes(ctx, r.says.as_slice(), alloc)?;
                let db = measure_boxes(ctx, r.does.as_slice(), alloc)?;
                let sl = break_lines(sb.as_slice(), max_says.max(1.0), alloc)?;
                let d_bal = balance_width(db.as_slice(), dw, alloc)?;
                let dl = break_lines(db.as_slice(), d_bal, alloc)?;

                let first_s_above = sl
                    .first()
                    .map_or(0.0, |l| vertical_metrics(ctx, r.says.as_slice(), l.as_slice(), RoleName::Tip, None).0);
                let first_d_above = dl
                    .first()
                    .map_or(0.0, |l| vertical_metrics(ctx, r.does.as_slice(), l.as_slice(), RoleName::Tip, None).0);
                let first_above = first_s_above.max(first_d_above);

                let mut bottom = y;
                // Left column (`says`)
                let mut ly = y + first_above - first_s_above;
                for (k, l) in sl.as_slice().iter().enumerate() {
                    let (a, b) = vertical_metrics(ctx, r.says.as_slice(), l.as_slice(), RoleName::Tip, None);
                    out.emit_line(
                        r.says.as_slice(),
                        l.as_slice(),
                        13.0,
                        ly,
                        a,
                        b,
                        EmitOpts { final_line: k + 1 == sl.len(), base_size: ctx.size(RoleName::Tip), button: None, tail_to: None },
                    )?;
                    ly += a + b;
                }
                bottom = bottom.max(ly);
                // Right column (`does`)
                let mut ry = y + first_above - first_d_above;
                for (k, l) in dl.as_slice().iter().enumerate() {
                    let (a, b) = vertical_metrics(ctx, r.does.as_slice(), l.as_slice(), RoleName::Tip, None);
                    out.emit_line(
                        r.does.as_slice(),
                        l.as_slice(),
                        13.0 + max_says + gap,
                        ry,
                        a,
                        b,
                        EmitOpts { final_line: k + 1 == dl.len(), base_size: ctx.size(RoleName::Tip), button: None, tail_to: None },
                    )?;
                    ry += a + b;
                }
                bottom = bottom.max(ry);
                y = bottom;
            }
        }
    }

    let w = inner + 26.0;
    let h = y + 11.0;
    if let Some(card) = out.shapes.get_mut(0) {
        card.rect = Rect::new(0.0, 0.0, w, h);
    }
    let mut link_rects = OwnVec::new_in(alloc);
    for (lid, list) in out.links.as_slice().iter().enumerate() {
        for &r in list.as_slice() {
            link_rects
                .try_push((u32::try_from(lid).unwrap_or(0), r))
                .map_err(|e| SiteError::Alloc(e.why))?;
        }
    }
    Ok(Some(TipLayout { w, h, shapes: out.shapes, pics: out.pics, glyphs: out.glyphs, link_rects }))
}

// =============================================================================
// Selection, Copy Text, and Case-Insensitive `find_matches`
// =============================================================================

/// Case-insensitive substring search across soft-wrapped lines (matching `input.ts::findMatches`).
pub fn find_matches<A: Allocator + Copy>(
    layout: &PageLayout<A>,
    query: &str,
    alloc: A,
) -> Result<OwnVec<CharRange, A>, SiteError> {
    let mut q_low = OwnString::new_in(alloc);
    for ch in query.chars() {
        for lc in ch.to_lowercase() {
            q_low.try_push(lc)?;
        }
    }
    let mut out = OwnVec::new_in(alloc);
    if q_low.is_empty() {
        return Ok(out);
    }

    let mut flat_chars: OwnVec<char, A> = OwnVec::new_in(alloc);
    let mut owner: OwnVec<usize, A> = OwnVec::new_in(alloc);
    for (i, c) in layout.chars.as_slice().iter().enumerate() {
        if i > 0 && c.before.is_some() {
            flat_chars.try_push(' ').map_err(|r| SiteError::Alloc(r.why))?;
            owner.try_push(i).map_err(|r| SiteError::Alloc(r.why))?;
        }
        let mut pushed = false;
        for lc in c.ch.to_lowercase() {
            flat_chars.try_push(lc).map_err(|r| SiteError::Alloc(r.why))?;
            owner.try_push(i).map_err(|r| SiteError::Alloc(r.why))?;
            pushed = true;
            break;
        }
        if !pushed {
            flat_chars.try_push(c.ch).map_err(|r| SiteError::Alloc(r.why))?;
            owner.try_push(i).map_err(|r| SiteError::Alloc(r.why))?;
        }
    }

    let mut needle: OwnVec<char, A> = OwnVec::new_in(alloc);
    for ch in q_low.as_str().chars() {
        needle.try_push(ch).map_err(|r| SiteError::Alloc(r.why))?;
    }
    let nlen = needle.len();
    let flen = flat_chars.len();
    if nlen == 0 || flen < nlen {
        return Ok(out);
    }

    let mut pos = 0usize;
    while pos + nlen <= flen {
        let mut matched = true;
        for k in 0..nlen {
            if flat_chars.get(pos + k) != needle.get(k) {
                matched = false;
                break;
            }
        }
        if matched {
            let from = owner.get(pos).copied().unwrap_or(0);
            let to = owner.get(pos + nlen - 1).copied().unwrap_or(from).saturating_add(1);
            out.try_push(CharRange { from, to }).map_err(|r| SiteError::Alloc(r.why))?;
            pos += nlen;
        } else {
            pos += 1;
        }
    }
    Ok(out)
}

/// Copy plain text for `chars[from..to]`, inserting block newlines / wrap spaces cleanly.
pub fn copy_text<A: Allocator + Copy>(
    layout: &PageLayout<A>,
    from: usize,
    to: usize,
    out: &mut OwnString<A>,
) -> Result<(), SiteError> {
    out.clear();
    let hi = to.min(layout.chars.len());
    let lo = from.min(hi);
    for i in lo..hi {
        if let Some(c) = layout.chars.get(i) {
            if i > lo {
                if let Some(sep) = c.before {
                    out.try_push(sep)?;
                }
            }
            out.try_push(c.ch)?;
        }
    }
    Ok(())
}

/// Hit-test a stage-local CSS point `(x, y)` to the nearest character insertion index `0..=chars.len()`.
pub fn char_at_point<A: Allocator + Copy>(layout: &PageLayout<A>, x: f32, y: f32) -> usize {
    let chars = layout.chars.as_slice();
    if chars.is_empty() {
        return 0;
    }
    let mut best_idx = 0usize;
    let mut best_dist = f32::MAX;
    for (i, c) in chars.iter().enumerate() {
        let dy = if y < c.top {
            c.top - y
        } else if y > c.bottom {
            y - c.bottom
        } else {
            0.0
        };
        let cx = (c.x0 + c.x1) * 0.5;
        let dx = (x - cx).abs();
        let score = dy * 1000.0 + dx;
        if score < best_dist {
            best_dist = score;
            best_idx = if x > cx && dy == 0.0 { i + 1 } else { i };
        }
    }
    best_idx.min(chars.len())
}

/// Word boundary range for double-click selection around `idx`.
pub fn word_range<A: Allocator + Copy>(layout: &PageLayout<A>, idx: usize) -> CharRange {
    let chars = layout.chars.as_slice();
    if chars.is_empty() {
        return CharRange { from: 0, to: 0 };
    }
    let i = idx.min(chars.len() - 1);
    let Some(ci) = chars.get(i) else {
        return CharRange { from: 0, to: 0 };
    };
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '-';
    let kind = is_word(ci.ch);
    let blk = ci.block;
    let mut from = i;
    while from > 0 {
        let Some(prev) = chars.get(from - 1) else { break };
        if prev.block == blk && is_word(prev.ch) == kind {
            from -= 1;
        } else {
            break;
        }
    }
    let mut to = i + 1;
    while to < chars.len() {
        let Some(next) = chars.get(to) else { break };
        if next.block == blk && is_word(next.ch) == kind {
            to += 1;
        } else {
            break;
        }
    }
    CharRange { from, to }
}

/// Block/item range for triple-click selection around `idx`.
pub fn block_item_range<A: Allocator + Copy>(layout: &PageLayout<A>, idx: usize) -> CharRange {
    let chars = layout.chars.as_slice();
    if chars.is_empty() {
        return CharRange { from: 0, to: 0 };
    }
    let i = idx.min(chars.len() - 1);
    let Some(ci) = chars.get(i) else {
        return CharRange { from: 0, to: 0 };
    };
    let blk = ci.block;
    let it = ci.item;
    let mut from = i;
    while from > 0 {
        let Some(prev) = chars.get(from - 1) else { break };
        if prev.block == blk && prev.item == it {
            from -= 1;
        } else {
            break;
        }
    }
    let mut to = i + 1;
    while to < chars.len() {
        let Some(next) = chars.get(to) else { break };
        if next.block == blk && next.item == it {
            to += 1;
        } else {
            break;
        }
    }
    CharRange { from, to }
}
