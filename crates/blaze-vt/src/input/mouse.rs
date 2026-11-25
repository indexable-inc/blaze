//! Mouse reporting, a port of ghostty's `src/input/mouse_encode.zig` (with `input/mouse.zig`'s
//! buttons, `terminal/mouse.zig`'s modes and the coordinate conversion of `renderer/size.zig`).
//!
//! The host gives the pointer in surface pixels and the [`Size`] of the surface, cells and
//! padding; the encoder finds the cell (or, for SGR-pixels, the terminal-space pixel), decides
//! whether the tracking mode reports the event, and writes it in the format the program asked
//! for. The last reported cell, used to send motion only when the cell changes, is the caller's
//! to keep (the `last_cell` argument), not state here.

use super::key::Mods;
use super::{BufferFull, Out};

/// What the pointer did.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MouseAction {
    #[default]
    Press,
    Release,
    Motion,
}

/// A mouse button, ghostty `input/mouse.zig` (4–7 are the wheel: up, down, left, right).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MouseButton {
    Unknown,
    Left,
    Right,
    Middle,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
    Eleven,
}

/// Which events the program asked for (DEC modes 9, 1000, 1002, 1003), ghostty
/// `terminal/mouse.zig:6`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MouseEvents {
    #[default]
    None,
    /// 9: presses of left, middle and right, no modifiers.
    X10,
    /// 1000: presses and releases.
    Normal,
    /// 1002: and motion while a button is held.
    Button,
    /// 1003: and all motion.
    Any,
}

impl MouseEvents {
    /// Whether this mode reports motion (ghostty `eventSendsMotion`).
    pub const fn sends_motion(self) -> bool {
        matches!(self, MouseEvents::Button | MouseEvents::Any)
    }
}

/// How reports are written (DEC modes 1005, 1006, 1015, 1016; none set is X10), ghostty
/// `terminal/mouse.zig:20`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum MouseFormat {
    /// `CSI M` and three bytes, coordinates to 222.
    #[default]
    X10,
    /// 1005: `CSI M`, coordinates as UTF-8.
    Utf8,
    /// 1006: `CSI < b ; x ; y M` (release `m`).
    Sgr,
    /// 1015: `CSI 32+b ; x ; y M`.
    Urxvt,
    /// 1016: SGR with terminal-space pixels.
    SgrPixels,
}

/// A pointer position in surface pixels: (0, 0) is the surface's top-left, padding included.
/// Negative and past-the-edge values are allowed (a drag out of the window).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct SurfacePos {
    pub x: f32,
    pub y: f32,
}

/// One mouse event (ghostty `mouse_encode.zig:53`).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct MouseEvent {
    pub action: MouseAction,
    /// The button; none for motion with no button pressed.
    pub button: Option<MouseButton>,
    /// Modifiers held (shift, alt and ctrl are reported).
    pub mods: Mods,
    pub pos: SurfacePos,
}

/// A width and height in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
}

/// Padding around the grid inside the surface, in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Padding {
    pub top: u32,
    pub bottom: u32,
    pub right: u32,
    pub left: u32,
}

/// The surface, cell and padding sizes that map pixels to cells (ghostty `renderer/size.zig:28`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Size {
    /// The whole surface.
    pub screen: Pixels,
    pub cell: Pixels,
    pub padding: Padding,
}

impl Size {
    /// The surface less padding (ghostty `subPadding`, saturating).
    fn terminal(self) -> Pixels {
        Pixels {
            width: self
                .screen
                .width
                .saturating_sub(self.padding.left.saturating_add(self.padding.right)),
            height: self
                .screen
                .height
                .saturating_sub(self.padding.top.saturating_add(self.padding.bottom)),
        }
    }

    /// Columns and rows, at least one each (ghostty `GridSize.update`, in f32).
    fn grid(self) -> Cell {
        let t = self.terminal();
        let cols = (t.width as f32 / self.cell.width as f32) as u16;
        let rows = (t.height as f32 / self.cell.height as f32) as u16;
        Cell { x: cols.max(1), y: rows.max(1) }
    }
}

/// A zero-based cell: column `x`, row `y`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Cell {
    pub x: u16,
    pub y: u16,
}

/// What the encoder needs from the terminal and the host (ghostty `mouse_encode.zig:13`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct MouseOpts {
    /// The tracking mode (9, 1000, 1002, 1003).
    pub event: MouseEvents,
    /// The report format (1005, 1006, 1015, 1016).
    pub format: MouseFormat,
    pub size: Size,
    /// Whether any button is down, this event's included. Motion outside the viewport is
    /// reported only with one down (a drag out of the surface).
    pub any_button_pressed: bool,
}

/// The longest report: SGR-pixels with two ten-digit negative coordinates.
pub const MAX_LEN: usize = 32;

/// Encodes `event` into `out` and returns the bytes, which may be none. `last_cell`, when
/// given, is the cell last reported: motion within it is dropped (except SGR-pixels), and it is
/// updated to this event's cell.
pub fn encode<'o>(
    event: &MouseEvent,
    opts: &MouseOpts,
    last_cell: Option<&mut Option<Cell>>,
    out: &'o mut [u8],
) -> Result<&'o [u8], BufferFull> {
    let mut w = Out::new(out);
    write(&mut w, event, opts, last_cell)?;
    Ok(w.finish())
}

fn write(
    w: &mut Out<'_>,
    event: &MouseEvent,
    opts: &MouseOpts,
    last_cell: Option<&mut Option<Cell>>,
) -> Result<(), BufferFull> {
    if !should_report(event, opts) {
        return Ok(());
    }

    // Releases are reported wherever they happen; anything else outside the viewport only in a
    // motion mode with a button held.
    if event.action != MouseAction::Release
        && pos_out_of_viewport(event.pos, opts.size)
        && (!opts.event.sends_motion() || !opts.any_button_pressed)
    {
        return Ok(());
    }

    let cell = pos_to_cell(event.pos, opts.size);

    // Motion only when the cell changed, unless the format reports pixels.
    if let Some(last) = last_cell {
        if event.action == MouseAction::Motion
            && opts.format != MouseFormat::SgrPixels
            && *last == Some(cell)
        {
            return Ok(());
        }
        *last = Some(cell);
    }

    let Some(code) = button_code(event, opts) else {
        return Ok(());
    };
    let final_byte = if event.action == MouseAction::Release { b'm' } else { b'M' };
    match opts.format {
        MouseFormat::X10 => {
            // One byte per coordinate, 1-based, offset by 32.
            if cell.x > 222 || cell.y > 222 {
                return Ok(());
            }
            w.bytes(b"\x1B[M")?;
            w.byte(32 + code)?;
            w.byte(33 + cell.x as u8)?;
            w.byte(33 + cell.y as u8)
        }
        MouseFormat::Utf8 => {
            w.bytes(b"\x1B[M")?;
            w.byte(32 + code)?;
            // At most 65535 + 33: never a surrogate, always a char.
            for v in [cell.x, cell.y] {
                let cp = u32::from(v) + 33;
                w.char(char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER))?;
            }
            Ok(())
        }
        MouseFormat::Sgr => {
            w.bytes(b"\x1B[<")?;
            w.dec(u32::from(code))?;
            w.byte(b';')?;
            w.dec(u32::from(cell.x) + 1)?;
            w.byte(b';')?;
            w.dec(u32::from(cell.y) + 1)?;
            w.byte(final_byte)
        }
        MouseFormat::Urxvt => {
            w.bytes(b"\x1B[")?;
            w.dec(32 + u32::from(code))?;
            w.byte(b';')?;
            w.dec(u32::from(cell.x) + 1)?;
            w.byte(b';')?;
            w.dec(u32::from(cell.y) + 1)?;
            w.byte(b'M')
        }
        MouseFormat::SgrPixels => {
            let (x, y) = pos_to_pixels(event.pos, opts.size);
            w.bytes(b"\x1B[<")?;
            w.dec(u32::from(code))?;
            w.byte(b';')?;
            w.dec_signed(x)?;
            w.byte(b';')?;
            w.dec_signed(y)?;
            w.byte(final_byte)
        }
    }
}

/// Whether the tracking mode reports this event at all (ghostty `mouse_encode.zig:174`).
fn should_report(event: &MouseEvent, opts: &MouseOpts) -> bool {
    match opts.event {
        MouseEvents::None => false,
        MouseEvents::X10 => {
            event.action == MouseAction::Press
                && matches!(
                    event.button,
                    Some(MouseButton::Left | MouseButton::Middle | MouseButton::Right)
                )
        }
        MouseEvents::Normal => event.action != MouseAction::Motion,
        MouseEvents::Button => event.button.is_some(),
        MouseEvents::Any => true,
    }
}

/// The button byte before its format's offset (ghostty `mouse_encode.zig:197`): none for a
/// button no format can name (unknown, 10, 11).
fn button_code(event: &MouseEvent, opts: &MouseOpts) -> Option<u8> {
    let mut acc = match event.button {
        // Motion with no button.
        None => 3,
        // Legacy formats cannot say which button was released.
        Some(_)
            if event.action == MouseAction::Release
                && !matches!(opts.format, MouseFormat::Sgr | MouseFormat::SgrPixels) =>
        {
            3
        }
        Some(b) => match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Four => 64,
            MouseButton::Five => 65,
            MouseButton::Six => 66,
            MouseButton::Seven => 67,
            MouseButton::Eight => 128,
            MouseButton::Nine => 129,
            MouseButton::Unknown | MouseButton::Ten | MouseButton::Eleven => return None,
        },
    };
    // X10 mode carries no modifiers.
    if opts.event != MouseEvents::X10 {
        if event.mods.shift() {
            acc += 4;
        }
        if event.mods.alt() {
            acc += 8;
        }
        if event.mods.ctrl() {
            acc += 16;
        }
    }
    if event.action == MouseAction::Motion {
        acc += 32;
    }
    Some(acc)
}

/// Outside the surface (negative or past its size; exactly at the edge is inside).
fn pos_out_of_viewport(pos: SurfacePos, size: Size) -> bool {
    let max_x = size.screen.width as f32;
    let max_y = size.screen.height as f32;
    pos.x < 0.0 || pos.y < 0.0 || pos.x > max_x || pos.y > max_y
}

/// The cell under a surface position, clamped to the grid (ghostty `Coordinate.convert(.grid)`).
fn pos_to_cell(pos: SurfacePos, size: Size) -> Cell {
    let term_x = f64::from(pos.x) - f64::from(size.padding.left);
    let term_y = f64::from(pos.y) - f64::from(size.padding.top);
    let grid = size.grid();
    let col = (term_x.max(0.0) / f64::from(size.cell.width)) as u16;
    let row = (term_y.max(0.0) / f64::from(size.cell.height)) as u16;
    Cell { x: col.min(grid.x - 1), y: row.min(grid.y - 1) }
}

/// Terminal-space pixels, rounded half away from zero, not clamped (ghostty
/// `mouse_encode.zig:271`).
fn pos_to_pixels(pos: SurfacePos, size: Size) -> (i32, i32) {
    let x = f64::from(pos.x) - f64::from(size.padding.left);
    let y = f64::from(pos.y) - f64::from(size.padding.top);
    (round(x), round(y))
}

/// `@round` then `@intFromFloat`: half away from zero (core has no `f64::round`).
fn round(v: f64) -> i32 {
    // ghostty traps past i32; this saturates (NaN is 0).
    let v = v.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
    let t = v as i64;
    let frac = v - t as f64;
    let r = if frac >= 0.5 {
        t + 1
    } else if frac <= -0.5 {
        t - 1
    } else {
        t
    };
    r.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn test_size() -> Size {
        Size {
            screen: Pixels { width: 1_000, height: 1_000 },
            cell: Pixels { width: 1, height: 1 },
            padding: Padding::default(),
        }
    }

    fn opts(event: MouseEvents, format: MouseFormat) -> MouseOpts {
        MouseOpts { event, format, size: test_size(), any_button_pressed: false }
    }

    fn ev(button: Option<MouseButton>, action: MouseAction, x: f32, y: f32) -> MouseEvent {
        MouseEvent { action, button, mods: Mods::NONE, pos: SurfacePos { x, y } }
    }

    fn enc(event: &MouseEvent, opts: &MouseOpts, last: &mut Option<Cell>) -> Vec<u8> {
        let mut buf = [0u8; MAX_LEN];
        encode(event, opts, Some(last), &mut buf).unwrap().to_vec()
    }

    const ACTIONS: [MouseAction; 3] =
        [MouseAction::Press, MouseAction::Release, MouseAction::Motion];
    const LEFT: Option<MouseButton> = Some(MouseButton::Left);

    // ghostty mouse_encode.zig:292 "shouldReport: none mode never reports"
    #[test]
    fn should_report_none_mode_never_reports() {
        for a in ACTIONS {
            assert!(!should_report(
                &ev(LEFT, a, 0.0, 0.0),
                &opts(MouseEvents::None, MouseFormat::X10)
            ));
        }
    }

    // ghostty mouse_encode.zig:302 "shouldReport: x10 reports only left/middle/right press"
    #[test]
    fn should_report_x10_reports_only_left_middle_right_press() {
        let o = opts(MouseEvents::X10, MouseFormat::X10);
        for b in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
            assert!(should_report(&ev(Some(b), MouseAction::Press, 0.0, 0.0), &o));
        }
        assert!(!should_report(&ev(LEFT, MouseAction::Release, 0.0, 0.0), &o));
        assert!(!should_report(&ev(LEFT, MouseAction::Motion, 0.0, 0.0), &o));
        assert!(!should_report(&ev(Some(MouseButton::Four), MouseAction::Press, 0.0, 0.0), &o));
        assert!(!should_report(&ev(None, MouseAction::Press, 0.0, 0.0), &o));
    }

    // ghostty mouse_encode.zig:337 "shouldReport: normal reports press and release but not motion"
    #[test]
    fn should_report_normal_reports_press_and_release_but_not_motion() {
        let o = opts(MouseEvents::Normal, MouseFormat::X10);
        assert!(should_report(&ev(LEFT, MouseAction::Press, 0.0, 0.0), &o));
        assert!(should_report(&ev(LEFT, MouseAction::Release, 0.0, 0.0), &o));
        assert!(!should_report(&ev(LEFT, MouseAction::Motion, 0.0, 0.0), &o));
    }

    // ghostty mouse_encode.zig:355 "shouldReport: button mode requires a button"
    #[test]
    fn should_report_button_mode_requires_a_button() {
        let o = opts(MouseEvents::Button, MouseFormat::X10);
        for a in ACTIONS {
            assert!(should_report(&ev(LEFT, a, 0.0, 0.0), &o));
            assert!(!should_report(&ev(None, a, 0.0, 0.0), &o));
        }
    }

    // ghostty mouse_encode.zig:374 "shouldReport: any mode reports everything"
    #[test]
    fn should_report_any_mode_reports_everything() {
        let o = opts(MouseEvents::Any, MouseFormat::X10);
        for a in ACTIONS {
            assert!(should_report(&ev(LEFT, a, 0.0, 0.0), &o));
        }
        assert!(should_report(&ev(None, MouseAction::Motion, 0.0, 0.0), &o));
    }

    // ghostty mouse_encode.zig:390 "x10 press left"
    #[test]
    fn x10_press_left() {
        let mut e = ev(LEFT, MouseAction::Press, 0.0, 0.0);
        e.mods = Mods::SHIFT | Mods::ALT | Mods::CTRL;
        let mut last = None;
        assert_eq!(
            enc(&e, &opts(MouseEvents::X10, MouseFormat::X10), &mut last),
            [0x1B, b'[', b'M', 32, 33, 33]
        );
    }

    // ghostty mouse_encode.zig:416 "x10 ignores release"
    #[test]
    fn x10_ignores_release() {
        let mut last = None;
        assert!(
            enc(
                &ev(LEFT, MouseAction::Release, 0.0, 0.0),
                &opts(MouseEvents::X10, MouseFormat::X10),
                &mut last
            )
            .is_empty()
        );
    }

    // ghostty mouse_encode.zig:433 "normal ignores motion"
    #[test]
    fn normal_ignores_motion() {
        let mut last = None;
        assert!(
            enc(
                &ev(LEFT, MouseAction::Motion, 0.0, 0.0),
                &opts(MouseEvents::Normal, MouseFormat::Sgr),
                &mut last
            )
            .is_empty()
        );
    }

    // ghostty mouse_encode.zig:450 "button mode requires button"
    #[test]
    fn button_mode_requires_button() {
        let mut last = None;
        assert!(
            enc(
                &ev(None, MouseAction::Motion, 0.0, 0.0),
                &opts(MouseEvents::Button, MouseFormat::Sgr),
                &mut last
            )
            .is_empty()
        );
    }

    // ghostty mouse_encode.zig:467 "sgr release keeps button identity"
    #[test]
    fn sgr_release_keeps_button_identity() {
        let mut last = None;
        let e = ev(Some(MouseButton::Right), MouseAction::Release, 4.0, 5.0);
        assert_eq!(enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last), b"\x1B[<2;5;6m");
    }

    // ghostty mouse_encode.zig:485 "sgr motion with no button"
    #[test]
    fn sgr_motion_with_no_button() {
        let mut last = None;
        let e = ev(None, MouseAction::Motion, 1.0, 2.0);
        assert_eq!(enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last), b"\x1B[<35;2;3M");
    }

    // ghostty mouse_encode.zig:503 "urxvt with modifiers"
    #[test]
    fn urxvt_with_modifiers() {
        let mut last = None;
        let mut e = ev(LEFT, MouseAction::Press, 2.0, 3.0);
        e.mods = Mods::SHIFT | Mods::ALT | Mods::CTRL;
        assert_eq!(
            enc(&e, &opts(MouseEvents::Any, MouseFormat::Urxvt), &mut last),
            b"\x1B[60;3;4M"
        );
    }

    // ghostty mouse_encode.zig:522 "utf8 encodes large coordinates"
    #[test]
    fn utf8_encodes_large_coordinates() {
        let mut last = None;
        let out = enc(
            &ev(LEFT, MouseAction::Press, 300.0, 400.0),
            &opts(MouseEvents::Any, MouseFormat::Utf8),
            &mut last,
        );
        assert_eq!(out[..4], [0x1B, b'[', b'M', 32]);
        let cps: Vec<u32> =
            core::str::from_utf8(&out[4..]).unwrap().chars().map(u32::from).collect();
        assert_eq!(cps, [333, 433]);
    }

    // ghostty mouse_encode.zig:547 "x10 coordinate limit"
    #[test]
    fn x10_coordinate_limit() {
        let mut last = None;
        assert!(
            enc(
                &ev(LEFT, MouseAction::Press, 223.0, 0.0),
                &opts(MouseEvents::X10, MouseFormat::X10),
                &mut last
            )
            .is_empty()
        );
    }

    // ghostty mouse_encode.zig:565 "sgr wheel button mappings"
    #[test]
    fn sgr_wheel_button_mappings() {
        for (b, code) in [
            (MouseButton::Four, 64),
            (MouseButton::Five, 65),
            (MouseButton::Six, 66),
            (MouseButton::Seven, 67),
        ] {
            let mut last = None;
            let out = enc(
                &ev(Some(b), MouseAction::Press, 0.0, 0.0),
                &opts(MouseEvents::Any, MouseFormat::Sgr),
                &mut last,
            );
            assert_eq!(out, std::format!("\x1B[<{code};1;1M").as_bytes());
        }
    }

    // ghostty mouse_encode.zig:597 "urxvt release uses legacy button 3 encoding"
    #[test]
    fn urxvt_release_uses_legacy_button_3_encoding() {
        let mut last = None;
        let e = ev(Some(MouseButton::Right), MouseAction::Release, 2.0, 3.0);
        assert_eq!(
            enc(&e, &opts(MouseEvents::Any, MouseFormat::Urxvt), &mut last),
            b"\x1B[35;3;4M"
        );
    }

    // ghostty mouse_encode.zig:615 "unsupported button is ignored"
    #[test]
    fn unsupported_button_is_ignored() {
        let mut last = None;
        let e = ev(Some(MouseButton::Ten), MouseAction::Press, 1.0, 1.0);
        assert!(enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last).is_empty());
    }

    // ghostty mouse_encode.zig:633 "sgr pixels uses terminal-space cursor coordinates"
    #[test]
    fn sgr_pixels_uses_terminal_space_cursor_coordinates() {
        let mut last = None;
        let e = ev(LEFT, MouseAction::Press, 10.0, 20.0);
        assert_eq!(
            enc(&e, &opts(MouseEvents::Any, MouseFormat::SgrPixels), &mut last),
            b"\x1B[<0;10;20M"
        );
    }

    // ghostty mouse_encode.zig:651 "sgr pixels release keeps button identity"
    #[test]
    fn sgr_pixels_release_keeps_button_identity() {
        let mut last = None;
        let e = ev(Some(MouseButton::Right), MouseAction::Release, 10.0, 20.0);
        assert_eq!(
            enc(&e, &opts(MouseEvents::Any, MouseFormat::SgrPixels), &mut last),
            b"\x1B[<2;10;20m"
        );
    }

    // ghostty mouse_encode.zig:669 "position exactly at viewport boundary is encoded in final cell"
    #[test]
    fn position_exactly_at_viewport_boundary_is_encoded_in_final_cell() {
        let mut o = opts(MouseEvents::Any, MouseFormat::Sgr);
        o.size = Size {
            screen: Pixels { width: 10, height: 10 },
            cell: Pixels { width: 2, height: 2 },
            padding: Padding::default(),
        };
        let mut last = None;
        assert_eq!(enc(&ev(LEFT, MouseAction::Press, 10.0, 10.0), &o, &mut last), b"\x1B[<0;5;5M");
    }

    // ghostty mouse_encode.zig:693 "outside viewport motion with no pressed button is ignored"
    #[test]
    fn outside_viewport_motion_with_no_pressed_button_is_ignored() {
        let mut last = None;
        let e = ev(LEFT, MouseAction::Motion, -1.0, -1.0);
        assert!(enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last).is_empty());
    }

    // ghostty mouse_encode.zig:712 "outside viewport motion with pressed button is reported"
    #[test]
    fn outside_viewport_motion_with_pressed_button_is_reported() {
        let mut last = None;
        let mut o = opts(MouseEvents::Any, MouseFormat::Sgr);
        o.any_button_pressed = true;
        assert_eq!(
            enc(&ev(LEFT, MouseAction::Motion, -1.0, -1.0), &o, &mut last),
            b"\x1B[<32;1;1M"
        );
    }

    // ghostty mouse_encode.zig:731 "motion is deduped by last cell except sgr pixels"
    #[test]
    fn motion_is_deduped_by_last_cell_except_sgr_pixels() {
        let mut last = None;
        let e = ev(LEFT, MouseAction::Motion, 5.0, 6.0);
        assert!(!enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last).is_empty());
        assert!(enc(&e, &opts(MouseEvents::Any, MouseFormat::Sgr), &mut last).is_empty());
        assert!(!enc(&e, &opts(MouseEvents::Any, MouseFormat::SgrPixels), &mut last).is_empty());
    }

    // Beyond ghostty's tests.

    #[test]
    fn padding_shifts_cells_and_pixels() {
        let mut o = opts(MouseEvents::Any, MouseFormat::Sgr);
        o.size = Size {
            screen: Pixels { width: 110, height: 110 },
            cell: Pixels { width: 10, height: 10 },
            padding: Padding { top: 5, bottom: 5, right: 5, left: 5 },
        };
        let mut last = None;
        // (24, 4) is terminal (19, -1): cell (1, 0), the negative row clamped.
        assert_eq!(enc(&ev(LEFT, MouseAction::Press, 24.0, 4.0), &o, &mut last), b"\x1B[<0;2;1M");
        o.format = MouseFormat::SgrPixels;
        assert_eq!(enc(&ev(LEFT, MouseAction::Press, 24.0, 4.0), &o, &mut last), b"\x1B[<0;19;-1M");
        // Rounding half away from zero.
        assert_eq!(enc(&ev(LEFT, MouseAction::Press, 5.5, 4.5), &o, &mut last), b"\x1B[<0;1;-1M");
        // Past the grid: the last cell.
        o.format = MouseFormat::Sgr;
        assert_eq!(
            enc(&ev(LEFT, MouseAction::Press, 109.0, 109.0), &o, &mut last),
            b"\x1B[<0;10;10M"
        );
    }

    #[test]
    fn no_last_cell_never_dedupes() {
        let e = ev(LEFT, MouseAction::Motion, 5.0, 6.0);
        let o = opts(MouseEvents::Any, MouseFormat::Sgr);
        let mut buf = [0u8; MAX_LEN];
        for _ in 0..2 {
            assert_eq!(encode(&e, &o, None, &mut buf).unwrap(), b"\x1B[<32;6;7M");
        }
    }

    #[test]
    fn every_report_fits_max_len() {
        blaze_sim::check(blaze_sim::Config::cases(2000), |tape| {
            const BUTTONS: [Option<MouseButton>; 6] = [
                None,
                LEFT,
                Some(MouseButton::Right),
                Some(MouseButton::Four),
                Some(MouseButton::Nine),
                Some(MouseButton::Eleven),
            ];
            const EVENTS: [MouseEvents; 5] = [
                MouseEvents::None,
                MouseEvents::X10,
                MouseEvents::Normal,
                MouseEvents::Button,
                MouseEvents::Any,
            ];
            const FORMATS: [MouseFormat; 5] = [
                MouseFormat::X10,
                MouseFormat::Utf8,
                MouseFormat::Sgr,
                MouseFormat::Urxvt,
                MouseFormat::SgrPixels,
            ];
            let coord = |t: &blaze_sim::Tape, l| match t.below(3, l) {
                0 => f32::MAX,
                1 => -f32::MAX,
                _ => t.int_in(-100_000..=100_000i32, l) as f32,
            };
            let e = MouseEvent {
                action: *tape.pick(&ACTIONS, "action").unwrap_or(&MouseAction::Press),
                button: *tape.pick(&BUTTONS, "button").unwrap_or(&None),
                mods: Mods::from_bits(tape.int_in(0..=0x3FFu16, "mods")),
                pos: SurfacePos { x: coord(tape, "x"), y: coord(tape, "y") },
            };
            let o = MouseOpts {
                event: *tape.pick(&EVENTS, "event").unwrap_or(&MouseEvents::Any),
                format: *tape.pick(&FORMATS, "format").unwrap_or(&MouseFormat::Sgr),
                size: Size {
                    screen: Pixels {
                        width: tape.int_in(0..=u32::MAX, "w"),
                        height: tape.int_in(0..=u32::MAX, "h"),
                    },
                    cell: Pixels {
                        width: tape.int_in(1..=40u32, "cw"),
                        height: tape.int_in(1..=40u32, "ch"),
                    },
                    padding: Padding {
                        top: tape.int_in(0..=50u32, "pt"),
                        bottom: tape.int_in(0..=50u32, "pb"),
                        right: tape.int_in(0..=50u32, "pr"),
                        left: tape.int_in(0..=50u32, "pl"),
                    },
                },
                any_button_pressed: tape.coin("held"),
            };
            let mut buf = [0u8; MAX_LEN];
            assert!(encode(&e, &o, None, &mut buf).is_ok(), "{e:?} {o:?}");
        });
    }
}
