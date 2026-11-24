//! Pure `#![no_std]` port of `den`'s liquid multi-line selection shape & copy-pulse animation
//! (`site/client/lib/selshape.ts`: `join_rows`, `outline` per-corner radii + inner-corner fillets,
//! 300ms copy-pulse grow + staggered light sweep).

use core::alloc::Allocator;

use blaze_own::Vec as OwnVec;

use crate::error::SiteError;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Row {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fillet {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub towards: [f32; 2],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PulseState {
    pub live: bool,
    pub phase: f32,
    pub scale: f32,
    pub cx: f32,
    pub cy: f32,
}

pub const PULSE_MS: f32 = 300.0;
pub const PULSE_GROW: f32 = 0.025;
pub const PULSE_RISE: f32 = 0.18;
pub const STAGGER_MS: f32 = 40.0;
pub const ROW_IN_MS: f32 = 110.0;
pub const SWEEP_ALPHA: f32 = 0.12;

/// Cubic ease-out up to `1 + PULSE_GROW` then cubic ease-back to `1.0`.
#[inline]
pub fn pulse_scale(phase: f32) -> f32 {
    let p = if phase <= PULSE_RISE {
        let u = 1.0 - phase / PULSE_RISE;
        1.0 - u * u * u
    } else {
        let u = (1.0 - (phase - PULSE_RISE) / (1.0 - PULSE_RISE)).max(0.0);
        u * u * u
    };
    1.0 + PULSE_GROW * p
}

/// Merge fragments on one visual line, then join neighboring lines within `max_gap` seamlessly.
pub fn join_rows<A: Allocator + Copy>(
    fragments: &[Row],
    max_gap: f32,
    snap: f32,
    alloc: A,
) -> Result<OwnVec<Row, A>, SiteError> {
    let mut sorted = OwnVec::try_with_capacity_in(fragments.len(), alloc)?;
    for &f in fragments {
        sorted.try_push(f).map_err(|r| SiteError::Alloc(r.why))?;
    }
    // Insertion sort by (top, left) — fragments are nearly sorted already.
    let s = sorted.as_mut_slice();
    let mut i = 1usize;
    while i < s.len() {
        let mut j = i;
        while j > 0 {
            let (Some(&prev), Some(&cur)) = (s.get(j - 1), s.get(j)) else { break };
            if prev.top > cur.top || (prev.top == cur.top && prev.left > cur.left) {
                s.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
        i += 1;
    }

    let mut lines: OwnVec<Row, A> = OwnVec::new_in(alloc);
    for &f in sorted.as_slice() {
        let merged = if let Some(last) = lines.last_mut() {
            let f_mid = (f.top + f.bottom) * 0.5;
            let l_mid = (last.top + last.bottom) * 0.5;
            if (f_mid - l_mid).abs() < (last.bottom - last.top) * 0.5 {
                last.left = last.left.min(f.left);
                last.right = last.right.max(f.right);
                last.top = last.top.min(f.top);
                last.bottom = last.bottom.max(f.bottom);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !merged {
            lines.try_push(f).map_err(|r| SiteError::Alloc(r.why))?;
        }
    }

    let n = lines.len();
    for idx in 1..n {
        let a_bot = lines.get(idx - 1).map_or(0.0, |r| r.bottom);
        let b_top = lines.get(idx).map_or(0.0, |r| r.top);
        if b_top - a_bot > max_gap {
            continue;
        }
        let raw_mid = (a_bot + b_top) * 0.5;
        let mid = if snap > 0.0 { gpu::math::round(raw_mid * snap) / snap } else { raw_mid };
        if let Some(a) = lines.get_mut(idx - 1) {
            a.bottom = mid;
        }
        if let Some(b) = lines.get_mut(idx) {
            b.top = mid;
        }
    }
    Ok(lines)
}

#[inline]
fn touches(a: Row, b: Row) -> bool {
    (a.bottom - b.top).abs() < 0.5
}

/// Compute per-corner radii `[tl, tr, br, bl]` for each row AND concave inner-corner fillets.
pub fn outline_rows<A: Allocator + Copy>(
    rows: &[Row],
    radius: f32,
    alloc: A,
) -> Result<(OwnVec<[f32; 4], A>, OwnVec<Fillet, A>), SiteError> {
    let n = rows.len();
    let mut base = OwnVec::try_with_capacity_in(n, alloc)?;
    let mut radii = OwnVec::try_with_capacity_in(n, alloc)?;
    for &r in rows {
        let b = radius.min((r.bottom - r.top) * 0.25);
        base.try_push(b).map_err(|e| SiteError::Alloc(e.why))?;
        radii.try_push([b, b, b, b]).map_err(|e| SiteError::Alloc(e.why))?;
    }
    let mut fillets = OwnVec::new_in(alloc);

    if n >= 2 {
        for i in 0..(n - 1) {
            let Some(&a) = rows.get(i) else { continue };
            let Some(&b) = rows.get(i + 1) else { continue };
            if !touches(a, b) {
                continue;
            }
            let y = a.bottom;
            let ba = base.get(i).copied().unwrap_or(0.0);
            let bb = base.get(i + 1).copied().unwrap_or(0.0);

            // Left junction: a's bottom-left (3) against b's top-left (0)
            let dl = b.left - a.left;
            if dl.abs() <= 0.5 {
                if let Some(ra) = radii.get_mut(i) {
                    ra[3] = 0.0;
                }
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[0] = 0.0;
                }
            } else if dl > 0.0 {
                let r = bb.min(dl * 0.5);
                if let Some(ra) = radii.get_mut(i) {
                    ra[3] = ba.min(dl * 0.5);
                }
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[0] = 0.0;
                }
                fillets
                    .try_push(Fillet { x: b.left - r, y, r, towards: [-1.0, 1.0] })
                    .map_err(|e| SiteError::Alloc(e.why))?;
            } else {
                let r = ba.min((-dl) * 0.5);
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[0] = bb.min((-dl) * 0.5);
                }
                if let Some(ra) = radii.get_mut(i) {
                    ra[3] = 0.0;
                }
                fillets
                    .try_push(Fillet { x: a.left - r, y: y - r, r, towards: [-1.0, -1.0] })
                    .map_err(|e| SiteError::Alloc(e.why))?;
            }

            // Right junction: a's bottom-right (2) against b's top-right (1)
            let dr = a.right - b.right;
            if dr.abs() <= 0.5 {
                if let Some(ra) = radii.get_mut(i) {
                    ra[2] = 0.0;
                }
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[1] = 0.0;
                }
            } else if dr > 0.0 {
                let r = bb.min(dr * 0.5);
                if let Some(ra) = radii.get_mut(i) {
                    ra[2] = ba.min(dr * 0.5);
                }
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[1] = 0.0;
                }
                fillets
                    .try_push(Fillet { x: b.right, y, r, towards: [1.0, 1.0] })
                    .map_err(|e| SiteError::Alloc(e.why))?;
            } else {
                let r = ba.min((-dr) * 0.5);
                if let Some(rb) = radii.get_mut(i + 1) {
                    rb[1] = bb.min((-dr) * 0.5);
                }
                if let Some(ra) = radii.get_mut(i) {
                    ra[2] = 0.0;
                }
                fillets
                    .try_push(Fillet { x: a.right, y: y - r, r, towards: [1.0, -1.0] })
                    .map_err(|e| SiteError::Alloc(e.why))?;
            }
        }
    }

    Ok((radii, fillets))
}

/// Evaluate copy-pulse animation state `ms` milliseconds after copy.
pub fn shape_of(rows: &[Row], ms: f32, reduced: bool) -> PulseState {
    if rows.is_empty() {
        return PulseState { live: false, phase: 1.0, scale: 1.0, cx: 0.0, cy: 0.0 };
    }
    let mut left = f32::MAX;
    let mut top = f32::MAX;
    let mut right = f32::MIN;
    let mut bottom = f32::MIN;
    for &r in rows {
        left = left.min(r.left);
        top = top.min(r.top);
        right = right.max(r.right);
        bottom = bottom.max(r.bottom);
    }
    let live = !reduced && ms >= 0.0 && ms < PULSE_MS;
    let phase = if live { ms / PULSE_MS } else { 1.0 };
    PulseState {
        live,
        phase,
        scale: if live { pulse_scale(phase) } else { 1.0 },
        cx: (left + right) * 0.5,
        cy: (top + bottom) * 0.5,
    }
}
