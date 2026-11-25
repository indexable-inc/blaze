//! The parser's events to the terminal's operations (stream.zig `execute` 1057, `csiDispatch`
//! 1089, `escDispatch` 2423; stream_terminal.zig for what each does and replies).

use blaze_own::planted::Planted;

use super::ops::{EraseDisplay, EraseLine};
use super::{Effect, Mode, StatusDisplay, Terminal};
use crate::dcs::DcsCommand;
use crate::parser::{Csi, Dcs, Esc, Sink, Terminator};
use crate::plant::VtPlant;
use crate::reply;
use crate::screen::{Charset, CursorShape, KittySet, ProtectedMode, Slot};

/// "def1": no parameter is 1, one is itself, more is invalid (stream.zig).
fn def1(csi: &Csi<'_>) -> Option<usize> {
    match csi.params {
        [] => Some(1),
        [p] => Some(usize::from(*p)),
        _ => None,
    }
}

impl<P: Planted<VtPlant>> Sink for Terminal<'_, P> {
    fn print(&mut self, c: char) {
        Terminal::print(self, u32::from(c));
    }

    fn execute(&mut self, c0: u8) {
        match c0 {
            0x00..=0x02 => {}
            0x05 => {
                let e = self.opts.enquiry;
                self.reply(|o| reply::enquiry(o, e));
            }
            0x07 => self.effect(Effect::Bell),
            0x08 => self.cursor_left(1),
            0x09 => self.tabs(1, false),
            0x0A..=0x0C => self.linefeed(),
            0x0D => self.carriage_return(),
            0x0E => self.invoke_charset(false, Slot::G1, false),
            0x0F => self.invoke_charset(false, Slot::G0, false),
            _ => self.unsupported(),
        }
    }

    fn esc(&mut self, esc: &Esc<'_>) {
        let none = esc.intermediates.is_empty();
        match esc.final_byte {
            b'B' | b'A' | b'0' => {
                let set = match esc.final_byte {
                    b'B' => Charset::Ascii,
                    b'A' => Charset::British,
                    _ => Charset::DecSpecial,
                };
                let slot = match esc.intermediates {
                    [b'('] => Slot::G0,
                    [b')'] => Slot::G1,
                    [b'*'] => Slot::G2,
                    [b'+'] => Slot::G3,
                    _ => return self.unsupported(),
                };
                self.scr().charset.sets[slot as usize] = set;
            }
            b'7' if none => self.save_cursor(),
            b'8' if none => self.restore_cursor(),
            b'8' if esc.intermediates == b"#" => self.decaln(),
            b'D' if none => self.index(),
            b'E' if none => {
                self.index();
                self.carriage_return();
            }
            b'H' if none => self.tab_set(),
            b'M' if none => self.reverse_index(),
            b'N' if none => self.invoke_charset(false, Slot::G2, true),
            b'O' if none => self.invoke_charset(false, Slot::G3, true),
            b'V' if none => self.set_protected_mode(ProtectedMode::Iso),
            b'W' if none => self.set_protected_mode(ProtectedMode::Off),
            b'Z' if none => self.device_attributes(0),
            b'c' if none => self.full_reset(),
            b'n' if none => self.invoke_charset(false, Slot::G2, false),
            b'o' if none => self.invoke_charset(false, Slot::G3, false),
            b'~' if none => self.invoke_charset(true, Slot::G1, false),
            b'}' if none => self.invoke_charset(true, Slot::G2, false),
            b'|' if none => self.invoke_charset(true, Slot::G3, false),
            b'=' if none => self.set_mode(Mode::KeypadKeys, true),
            b'>' if none => self.set_mode(Mode::KeypadKeys, false),
            b'\\' => {}
            _ => self.unsupported(),
        }
    }

    fn csi(&mut self, csi: &Csi<'_>) {
        self.csi_dispatch(csi);
    }

    fn osc_start(&mut self) {
        self.osc.start();
    }

    fn osc_put(&mut self, b: u8) {
        // ghostty captures at most `MAX_BUF` bytes after the number's `;` but for the unbounded
        // commands: past that (and the longest number, `5522;`) the OSC is invalid, so the
        // capture stops there rather than at the cap.
        let len = self.osc.bytes.len();
        if len >= crate::osc::MAX_BUF + 5 && !crate::osc::unbounded(&self.osc.bytes) {
            self.osc.overflow = true;
            return;
        }
        self.osc.put(&[b]);
    }

    fn osc_end(&mut self, end: Terminator) {
        if self.osc.overflow {
            self.stats.strings_truncated += 1;
            return;
        }
        let buf = core::mem::replace(&mut self.osc.bytes, blaze_own::Vec::new_in(self.alloc));
        self.osc(&buf, end);
        self.osc.bytes = buf;
        self.osc.bytes.clear();
    }

    fn dcs_hook(&mut self, dcs: &Dcs<'_>) {
        if let Some(DcsCommand::Tmux(n)) = self.dcs.hook(dcs)
            && !self.effects.tmux(n, &self.opts.caps)
        {
            self.stats.effects_dropped += 1;
        }
    }

    fn dcs_put(&mut self, b: u8) {
        if let Some(DcsCommand::Tmux(n)) = self.dcs.put(b)
            && !self.effects.tmux(n, &self.opts.caps)
        {
            self.stats.effects_dropped += 1;
        }
    }

    fn dcs_unhook(&mut self) {
        match self.dcs.unhook() {
            None => {}
            Some(DcsCommand::Tmux(n)) => {
                if !self.effects.tmux(n, &self.opts.caps) {
                    self.stats.effects_dropped += 1;
                }
            }
            Some(DcsCommand::XtGetTcap(req)) => {
                super::reply_into(
                    &mut self.replies,
                    self.opts.caps.replies,
                    &mut self.stats,
                    |o| reply::xtgettcap(o, &req),
                );
            }
            Some(DcsCommand::Decrqss(req)) => self.decrqss(req),
        }
    }

    fn apc_start(&mut self) {
        self.apc.start();
    }

    fn apc_put(&mut self, bytes: &[u8]) {
        self.apc.put(bytes);
    }

    fn apc_end(&mut self) {
        let (apc, mut screen, which) = self.kitty_split();
        let out = apc.end(which, &mut screen);
        self.kitty_outcome(out.reply, out.cursor, out.waits_on_host);
    }
}

impl<P: Planted<VtPlant>> Terminal<'_, P> {
    /// SGR (stream.zig:1381): every attribute onto the cursor's pen, interned in the screen's
    /// style set (the cursor holds one reference).
    fn sgr(&mut self, csi: &Csi<'_>) {
        let mut st = self.scr().cursor.style;
        crate::sgr::apply_all(csi, &mut st);
        self.scr().set_cursor_style(st);
    }

    /// DECRQSS (stream_handler.zig `dcsCommand`): the setting from the screen shown.
    fn decrqss(&mut self, req: crate::dcs::Decrqss) {
        let region = self.region;
        let lr = self.modes.get(Mode::EnableLeftRightMargin);
        let blink = self.modes.get(Mode::CursorBlinking);
        let s = self.screen();
        let style = s.cursor.style;
        let shape = s.cursor.shape;
        let state = reply::DecrqssState {
            style: &style,
            top: region.top as u32,
            bottom: region.bottom as u32,
            left: region.left as u32,
            right: region.right as u32,
            left_right_margins: lr,
            cursor_shape: shape,
            cursor_blink: blink,
        };
        self.reply(|o| reply::decrqss(o, req, &state));
    }

    /// DA1 (0), DA2 (1), DA3 (2).
    fn device_attributes(&mut self, which: u8) {
        let d = self.opts.device;
        match which {
            0 => self.reply(|o| reply::da1(o, d.primary)),
            1 => self.reply(|o| reply::da2(o, d.secondary)),
            _ => {
                if let Some(id) = d.tertiary {
                    self.reply(|o| reply::da3(o, id));
                }
            }
        }
    }

    fn csi_dispatch(&mut self, csi: &Csi<'_>) {
        let none = csi.intermediates.is_empty();
        let inter = csi.intermediates;
        let params = csi.params;
        match csi.final_byte {
            b'A' | b'k' if none => match def1(csi) {
                Some(n) => self.cursor_up(n),
                None => self.unsupported(),
            },
            b'B' if none => match def1(csi) {
                Some(n) => self.cursor_down(n),
                None => self.unsupported(),
            },
            b'C' if none => match def1(csi) {
                Some(n) => self.cursor_right(n),
                None => self.unsupported(),
            },
            b'D' | b'j' if none => match def1(csi) {
                Some(n) => self.cursor_left(n),
                None => self.unsupported(),
            },
            b'E' if none => match def1(csi) {
                Some(n) => {
                    self.cursor_down(n);
                    self.carriage_return();
                }
                None => self.unsupported(),
            },
            b'F' if none => match def1(csi) {
                Some(n) => {
                    self.cursor_up(n);
                    self.carriage_return();
                }
                None => self.unsupported(),
            },
            b'G' | b'`' if none => match def1(csi) {
                Some(n) => {
                    let (row, _) = self.cursor_pos_relative();
                    self.set_cursor_pos(row, n);
                }
                None => self.unsupported(),
            },
            b'H' | b'f' if none => match params {
                [] => self.set_cursor_pos(1, 1),
                [r] => self.set_cursor_pos(usize::from(*r), 1),
                [r, c] => self.set_cursor_pos(usize::from(*r), usize::from(*c)),
                _ => self.unsupported(),
            },
            b'I' if none => match def1(csi) {
                Some(n) => self.tabs(n, false),
                None => self.unsupported(),
            },
            b'J' => {
                let protected = match inter {
                    [] => false,
                    [b'?'] => true,
                    _ => return self.unsupported(),
                };
                let mode = match params {
                    [] | [0] => EraseDisplay::Below,
                    [1] => EraseDisplay::Above,
                    [2] => EraseDisplay::Complete,
                    [3] => EraseDisplay::Scrollback,
                    [22] => EraseDisplay::ScrollComplete,
                    _ => return self.unsupported(),
                };
                self.erase_display(mode, protected);
            }
            b'K' => {
                let protected = match inter {
                    [] => false,
                    [b'?'] => true,
                    _ => return self.unsupported(),
                };
                let mode = match params {
                    [] | [0] => EraseLine::Right,
                    [1] => EraseLine::Left,
                    [2] => EraseLine::Complete,
                    _ => return self.unsupported(),
                };
                self.erase_line(mode, protected);
            }
            b'L' if none => match def1(csi) {
                Some(n) => self.insert_lines(n),
                None => self.unsupported(),
            },
            b'M' if none => match def1(csi) {
                Some(n) => self.delete_lines(n),
                None => self.unsupported(),
            },
            b'P' if none => match def1(csi) {
                Some(n) => self.delete_chars(n),
                None => self.unsupported(),
            },
            b'S' if none => match def1(csi) {
                Some(n) => self.scroll_up(n),
                None => self.unsupported(),
            },
            b'T' if none => match def1(csi) {
                Some(n) => self.scroll_down(n),
                None => self.unsupported(),
            },
            b'W' => match (inter, params) {
                ([], [] | [0]) => self.tab_set(),
                ([], [2]) => self.tab_clear_current(),
                ([], [5]) => self.tab_clear_all(),
                ([b'?'], [5]) => self.tab_reset(),
                _ => self.unsupported(),
            },
            b'X' if none => match def1(csi) {
                Some(n) => self.erase_chars(n),
                None => self.unsupported(),
            },
            b'Z' if none => match def1(csi) {
                Some(n) => self.tabs(n, true),
                None => self.unsupported(),
            },
            b'a' if none => match def1(csi) {
                Some(n) => {
                    let (row, col) = self.cursor_pos_relative();
                    self.set_cursor_pos(row, col.saturating_add(n));
                }
                None => self.unsupported(),
            },
            b'b' if none => match def1(csi) {
                Some(n) => self.print_repeat(n),
                None => self.unsupported(),
            },
            b'c' => match inter {
                [] => self.device_attributes(0),
                [b'>'] => self.device_attributes(1),
                [b'='] => self.device_attributes(2),
                _ => self.unsupported(),
            },
            b'd' if none => match def1(csi) {
                Some(n) => {
                    let (_, col) = self.cursor_pos_relative();
                    self.set_cursor_pos(n, col);
                }
                None => self.unsupported(),
            },
            b'e' if none => match def1(csi) {
                Some(n) => {
                    let (row, col) = self.cursor_pos_relative();
                    self.set_cursor_pos(row.saturating_add(n), col);
                }
                None => self.unsupported(),
            },
            b'g' if none => match params {
                [0] => self.tab_clear_current(),
                [3] => self.tab_clear_all(),
                _ => self.unsupported(),
            },
            b'h' | b'l' => {
                let on = csi.final_byte == b'h';
                let ansi = match inter {
                    [] => true,
                    [b'?'] => false,
                    _ => return self.unsupported(),
                };
                for &p in params {
                    match Mode::from_int(p, ansi) {
                        Some(m) => self.set_mode(m, on),
                        None => self.unsupported(),
                    }
                }
            }
            b'm' => match inter {
                [] => self.sgr(csi),
                [b'>'] => self.xtmodkeys(params),
                _ => self.unsupported(),
            },
            b'n' => self.device_status(inter, params),
            b'p' => match (inter, params) {
                ([b'?', b'$'], [n]) => self.request_mode(*n),
                _ => self.unsupported(),
            },
            b'q' => match inter {
                [b' '] => self.decscusr(params),
                [b'"'] => match params {
                    [] | [0] | [2] => self.set_protected_mode(ProtectedMode::Off),
                    [1] => self.set_protected_mode(ProtectedMode::Dec),
                    _ => self.unsupported(),
                },
                [b'>'] => {
                    let v = self.opts.xtversion;
                    if v.len() <= 256 {
                        self.reply(|o| reply::xtversion(o, v));
                    }
                }
                _ => self.unsupported(),
            },
            b'r' => match (inter, params) {
                ([], []) => self.set_top_bottom_margin(0, 0),
                ([], [t]) => self.set_top_bottom_margin(usize::from(*t), 0),
                ([], [t, b]) => self.set_top_bottom_margin(usize::from(*t), usize::from(*b)),
                ([b'?'], _) => {
                    for &p in params {
                        match Mode::from_int(p, false) {
                            Some(m) => {
                                let v = self.modes.restore(m);
                                self.set_mode(m, v);
                            }
                            None => self.unsupported(),
                        }
                    }
                }
                _ => self.unsupported(),
            },
            b's' => match (inter, params) {
                ([], []) => {
                    if self.modes.get(Mode::EnableLeftRightMargin) {
                        self.set_left_right_margin(0, 0);
                    } else {
                        self.save_cursor();
                    }
                }
                ([], [l]) => self.set_left_right_margin(usize::from(*l), 0),
                ([], [l, r]) => self.set_left_right_margin(usize::from(*l), usize::from(*r)),
                ([b'?'], _) => {
                    for &p in params {
                        match Mode::from_int(p, false) {
                            Some(m) => self.modes.save(m),
                            None => self.unsupported(),
                        }
                    }
                }
                ([b'>'], [] | [0]) => self.flags.mouse_shift_capture = Some(false),
                ([b'>'], [1]) => self.flags.mouse_shift_capture = Some(true),
                _ => self.unsupported(),
            },
            b't' if none => self.xtwinops(params),
            b'u' => match inter {
                [] => self.restore_cursor(),
                [b'?'] => {
                    let f = self.scr().kitty.current().bits();
                    self.reply(|o| reply::kitty_keyboard(o, f));
                }
                [b'>'] => match params {
                    [f] if *f > 31 => self.unsupported(),
                    [f] => self.scr().kitty.push(*f as u8),
                    _ => self.scr().kitty.push(0),
                },
                [b'<'] => {
                    let n = if let [n] = params { usize::from(*n) } else { 1 };
                    self.scr().kitty.pop(n);
                }
                [b'='] => {
                    let flags = params.first().copied().unwrap_or(0);
                    if flags > 31 {
                        return self.unsupported();
                    }
                    let how = match params.get(1).copied().unwrap_or(1) {
                        1 => KittySet::Set,
                        2 => KittySet::Or,
                        3 => KittySet::Not,
                        _ => return self.unsupported(),
                    };
                    self.scr().kitty.set(how, flags as u8);
                }
                _ => self.unsupported(),
            },
            b'@' if none => match params {
                [] => self.insert_blanks(1),
                [p] => self.insert_blanks(usize::from(*p).max(1)),
                _ => self.unsupported(),
            },
            b'}' => match (inter, params) {
                ([b'$'], [0]) => self.status_display = StatusDisplay::Main,
                ([b'$'], [1]) => self.status_display = StatusDisplay::StatusLine,
                _ => self.unsupported(),
            },
            _ => self.unsupported(),
        }
    }

    /// XTMODKEYS `CSI > Pp ; Pv m` (S:1738): only `4;2` turns modifyOtherKeys 2 on.
    fn xtmodkeys(&mut self, params: &[u16]) {
        if params.len() > 2 {
            return self.unsupported();
        }
        let numeric = match params {
            [] => false,
            [p, rest @ ..] => match p {
                0..=2 => false,
                4 => rest == [2],
                _ => return self.unsupported(),
            },
        };
        self.flags.modify_other_keys_2 = numeric;
    }

    /// DSR (S:1804).
    fn device_status(&mut self, inter: &[u8], params: &[u16]) {
        match inter {
            [] | [b'?', ..] => {
                let question = match inter {
                    [] => false,
                    [b'?'] => true,
                    _ => return self.unsupported(),
                };
                match (params, question) {
                    ([5], false) => self.reply(reply::operating_status),
                    ([6], false) => {
                        let c = self.scr().cursor;
                        let r = reply::CursorReport {
                            x: c.x as u32,
                            y: c.y as u32,
                            origin: self.modes.get(Mode::Origin),
                            left: self.region.left as u32,
                            top: self.region.top as u32,
                        };
                        self.reply(|o| reply::cursor_position(o, r));
                    }
                    ([996], true) => self.report_color_scheme(),
                    _ => self.unsupported(),
                }
            }
            [b'>'] => self.flags.modify_other_keys_2 = false,
            _ => self.unsupported(),
        }
    }

    /// DECRQM `CSI ? n $ p`: `CSI ? n ; 1|2|0 $ y` (modes.zig `getReport`).
    fn request_mode(&mut self, n: u16) {
        let state = match Mode::from_int(n, false) {
            Some(m) if self.modes.get(m) => reply::ReportState::Set,
            Some(_) => reply::ReportState::Reset,
            None => reply::ReportState::NotRecognized,
        };
        self.reply(|o| reply::mode_report(o, reply::ModeReport { mode: n, ansi: false, state }));
    }

    /// DECSCUSR (S:1903; stream_terminal.zig `cursor_style`).
    fn decscusr(&mut self, params: &[u16]) {
        let (shape, blink) = match params {
            [] | [0] => (CursorShape::Block, false),
            [1] => (CursorShape::Block, true),
            [2] => (CursorShape::Block, false),
            [3] => (CursorShape::Underline, true),
            [4] => (CursorShape::Underline, false),
            [5] => (CursorShape::Bar, true),
            [6] => (CursorShape::Bar, false),
            _ => return self.unsupported(),
        };
        self.modes.set(Mode::CursorBlinking, blink);
        self.scr().cursor.shape = shape;
    }

    /// XTWINOPS: the reports 14, 16, 18, 21 and the title stack 22/23 (S:2070).
    fn xtwinops(&mut self, params: &[u16]) {
        match params {
            [14] => self.size_report(reply::SizeReport::Csi14),
            [16] => self.size_report(reply::SizeReport::Csi16),
            [18] => self.size_report(reply::SizeReport::Csi18),
            [21] => {
                let title = core::mem::replace(&mut self.title, blaze_own::Vec::new_in(self.alloc));
                self.reply(|o| reply::title_report(o, &title));
                self.title = title;
            }
            [22, 0 | 2] | [22, 0 | 2, _] => self.title_push(),
            [23, 0 | 2] | [23, 0 | 2, _] => self.title_pop(),
            _ => self.unsupported(),
        }
    }

    /// CSI 22 t: the title onto the stack, the oldest dropped past the cap.
    fn title_push(&mut self) {
        if self.opts.caps.title_stack == 0 {
            self.stats.titles_evicted += 1;
            return;
        }
        if self.title_stack.len() >= self.opts.caps.title_stack {
            self.title_stack.remove_shift(0);
            self.stats.titles_evicted += 1;
        }
        let mut t = blaze_own::Vec::new_in(self.alloc);
        if t.try_reserve(self.title.len()).is_err() || self.title_stack.try_reserve(1).is_err() {
            self.stats.titles_evicted += 1;
            return;
        }
        t.extend_from_slice(&self.title);
        self.title_stack.push(t);
    }

    /// CSI 23 t: the title from the stack, if any.
    fn title_pop(&mut self) {
        if let Some(t) = self.title_stack.pop() {
            self.title = t;
            self.effect(Effect::TitleChanged);
        }
    }
}
