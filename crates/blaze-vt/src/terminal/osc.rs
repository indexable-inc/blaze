//! What each OSC does (stream.zig:2259 `oscDispatch`, stream_terminal.zig's handler, and
//! termio/stream_handler.zig where the app acts): the screen's hyperlinks and prompt marks, the
//! title and working directory, the colours, and the effects the host acts on.

use blaze_own::planted::Planted;

use super::{Effect, Terminal};
use crate::osc::{self, Clipboard, Command, MouseShape, PromptAction, PromptKind, Redraw};
use crate::parser::Terminator;
use crate::plant::VtPlant;
use crate::screen::{PromptClick, PromptRedraw, SemanticSet};

impl<P: Planted<VtPlant>> Terminal<'_, P> {
    /// An OSC's bytes: parsed, then acted on as ghostty's stream acts on it; one ghostty
    /// ignores (or cannot parse) is counted unsupported.
    pub(super) fn osc(&mut self, data: &[u8], end: Terminator) {
        let Some(cmd) = osc::parse(data, end) else { return self.unsupported() };
        match cmd {
            Command::SemanticPrompt(sp) => self.semantic_prompt(&sp),
            Command::ChangeWindowTitle(title) => {
                // stream.zig: a title that is not UTF-8 is ignored.
                if core::str::from_utf8(title).is_err() {
                    return self.unsupported();
                }
                self.set_title(title);
            }
            Command::ClipboardContents { kind, data } => self.clipboard(kind, data),
            Command::ReportPwd(url) => self.report_pwd(url),
            Command::MouseShape(name) => match MouseShape::from_name(name) {
                Some(shape) => {
                    self.mouse_shape = shape;
                    self.effect(Effect::MouseShape(shape));
                }
                None => self.unsupported(),
            },
            Command::ColorOperation(_) | Command::KittyColorProtocol(_) => self.colors_op(&cmd),
            Command::ShowDesktopNotification { title, body } => {
                self.effect_with([title, body], |[title, body]| Effect::DesktopNotification {
                    title,
                    body,
                });
            }
            Command::HyperlinkStart { id, uri } => self.scr().start_hyperlink(uri, id),
            Command::HyperlinkEnd => self.scr().end_hyperlink(),
            Command::ConemuProgressReport(p) => self.effect(Effect::Progress(p)),
            Command::BlazeTermApi(payload) => {
                self.effect_with([payload], |[payload]| Effect::BlazeTermApi(payload));
            }
            // OSC 1 and the rest: parsed, and ignored as ghostty ignores them.
            Command::ChangeWindowIcon(_)
            | Command::ConemuSleep { .. }
            | Command::ConemuShowMessageBox(_)
            | Command::ConemuChangeTabTitle(_)
            | Command::ConemuWaitInput
            | Command::ConemuGuimacro(_)
            | Command::ConemuRunProcess(_)
            | Command::ConemuOutputEnvironmentVariable(_)
            | Command::ConemuXtermEmulation { .. }
            | Command::ConemuComment(_)
            | Command::KittyTextSizing(_)
            | Command::KittyClipboardProtocol(_)
            | Command::KittyDndProtocol(_)
            | Command::ContextSignal(_) => self.unsupported(),
        }
    }

    /// OSC 4, 5, 10–19, 104, 105, 110–119 and 21: the colours change and the queries are
    /// answered, one reply per command, whole or not at all.
    fn colors_op(&mut self, cmd: &Command<'_>) {
        let start = self.replies.len();
        let cap = self.opts.caps.replies;
        let mut fits = true;
        let replies = &mut self.replies;
        let fx = osc::run_colors(cmd, &mut self.colors, &mut |bytes: &[u8]| {
            if !fits {
                return;
            }
            if replies.len() + bytes.len() > cap || replies.try_reserve(bytes.len()).is_err() {
                fits = false;
                return;
            }
            replies.extend_from_slice(bytes);
        });
        if !fits {
            self.replies.truncate(start);
            self.stats.replies_dropped += 1;
        }
        if fx.palette {
            self.effect(Effect::PaletteChanged);
        }
    }

    /// OSC 52 (termio `clipboardContents`, the lib's decode): `?` asks to read, anything else
    /// is base64 to write; empty clears. Data that is not base64 is dropped.
    fn clipboard(&mut self, kind: u8, data: &[u8]) {
        let clipboard = Clipboard::from_kind(kind);
        if data == b"?" {
            return self.effect(Effect::ClipboardRead(clipboard));
        }
        let engine = blaze_base64::STANDARD;
        let caps = self.opts.caps;
        let bytes = &mut self.effects.bytes;
        let mark = bytes.len();
        let most = engine.decoded_len_most(data.len());
        if mark + most > caps.effect_bytes || bytes.try_reserve(most).is_err() {
            self.stats.effects_dropped += 1;
            return;
        }
        bytes.resize(mark + most, 0);
        let n = match engine.decode_into(data, &mut bytes[mark..]) {
            Ok(n) => n,
            Err(_) => {
                bytes.truncate(mark);
                return self.unsupported();
            }
        };
        bytes.truncate(mark + n);
        let contents = super::Span { start: mark as u32, len: n as u32 };
        if !self.effects.push(Effect::ClipboardWrite { clipboard, contents }, caps.effects) {
            self.effects.bytes.truncate(mark);
            self.stats.effects_dropped += 1;
        }
    }

    /// OSC 7 (termio `reportPwd`): empty clears the directory; otherwise a `file://` or
    /// `kitty-shell-cwd://` URL naming this host gives its path, percent-decoded for `file`.
    fn report_pwd(&mut self, url: &[u8]) {
        let url = &url[..url.len().min(self.opts.caps.pwd)];
        if url.is_empty() {
            self.pwd.clear();
            return self.effect(Effect::PwdChanged);
        }
        let Some((path, raw)) = local_path(url, self.opts.hostname) else {
            return self.unsupported();
        };
        self.pwd.clear();
        let mut fits = self.pwd.try_reserve(path.len()).is_ok();
        if fits {
            if raw {
                self.pwd.extend_from_slice(path);
            } else {
                let pwd = &mut self.pwd;
                let decoded = osc::url_percent_decode(path, &mut |b| {
                    if pwd.try_push(b).is_err() {
                        fits = false;
                    }
                });
                if decoded.is_err() {
                    self.pwd.clear();
                    return self.unsupported();
                }
            }
        }
        if !fits {
            self.pwd.clear();
            self.stats.unsupported += 1;
        }
        self.effect(Effect::PwdChanged);
    }

    /// OSC 133 (termio `semanticPrompt`, then Terminal.zig:1736 `semanticPrompt`).
    fn semantic_prompt(&mut self, sp: &osc::SemanticPrompt<'_>) {
        match sp.action {
            PromptAction::EndInputStartOutput => self.effect(Effect::CommandStarted),
            PromptAction::EndCommand => {
                let raw = sp.exit_code().unwrap_or(0);
                let exit_code = u8::try_from(raw).unwrap_or(1);
                self.effect(Effect::CommandFinished { exit_code });
            }
            _ => {}
        }
        match sp.action {
            PromptAction::FreshLine => self.prompt_fresh_line(),
            PromptAction::FreshLineNewPrompt | PromptAction::NewCommand => {
                self.prompt_fresh_line();
                let kind = sp.prompt_kind().unwrap_or(PromptKind::Initial);
                self.scr().set_semantic_content(prompt_set(kind));
                if let Some(r) = sp.redraw() {
                    self.flags.shell_redraws_prompt = match r {
                        Redraw::True => PromptRedraw::True,
                        Redraw::False => PromptRedraw::False,
                        Redraw::Last => PromptRedraw::Last,
                    };
                }
                // click_events wins over cl.
                let click = match (sp.click_events(), sp.cl()) {
                    (Some(e), _) => Some(PromptClick::ClickEvents(e)),
                    (None, Some(cl)) => Some(PromptClick::Cl(cl)),
                    (None, None) => None,
                };
                if let Some(c) = click {
                    self.scr().semantic_prompt.click = c;
                }
            }
            PromptAction::PromptStart => {
                let kind = sp.prompt_kind().unwrap_or(PromptKind::Initial);
                self.scr().set_semantic_content(prompt_set(kind));
            }
            PromptAction::EndPromptStartInput => {
                self.scr().set_semantic_content(SemanticSet::Input { clear_eol: false });
            }
            PromptAction::EndPromptStartInputTerminateEol => {
                self.scr().set_semantic_content(SemanticSet::Input { clear_eol: true });
            }
            PromptAction::EndInputStartOutput => {
                let s = self.scr();
                s.set_semantic_content(SemanticSet::Output);
                // fish: output starting at column 0 of a prompt row overwrites the prompt.
                if s.cursor.x == 0 && s.cursor_row().prompt != crate::page::RowPrompt::None {
                    s.cursor_row_mut().prompt = crate::page::RowPrompt::None;
                }
            }
            PromptAction::EndCommand => self.scr().set_semantic_content(SemanticSet::Output),
        }
    }

    /// OSC 133;L (Terminal.zig:1857): a CR and an index unless the cursor is at the left
    /// margin (or left of it, with left/right margins).
    fn prompt_fresh_line(&mut self) {
        let x = self.scr().cursor.x;
        let left = if x < self.region.left { 0 } else { self.region.left };
        if x == left {
            return;
        }
        self.carriage_return();
        self.index();
    }
}

fn prompt_set(kind: PromptKind) -> SemanticSet {
    let continuation = matches!(kind, PromptKind::Continuation | PromptKind::Secondary);
    SemanticSet::Prompt { continuation }
}

/// The path of an OSC 7 URL if it is local (termio `reportPwd`): scheme `file` or
/// `kitty-shell-cwd`, a host that is `localhost` or `hostname`. For `kitty-shell-cwd` the path
/// is raw (query and fragment kept, nothing decoded); for `file` it stops at `?` or `#` and is
/// still percent-encoded. The bool says raw.
fn local_path<'u>(url: &'u [u8], hostname: &[u8]) -> Option<(&'u [u8], bool)> {
    let (rest, raw) = if let Some(r) = url.strip_prefix(b"file://") {
        (r, false)
    } else {
        (url.strip_prefix(b"kitty-shell-cwd://")?, true)
    };
    let slash = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
    let authority = &rest[..slash];
    let path = &rest[slash..];
    let host = match authority.iter().rposition(|&b| b == b'@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    // A MAC address host (uri.zig `mac_address`) keeps its colons; otherwise one is a port.
    let host = if is_mac_address(host) {
        host
    } else {
        &host[..host.iter().position(|&b| b == b':').unwrap_or(host.len())]
    };
    if host.is_empty() || !(host == b"localhost" || (!hostname.is_empty() && host == hostname)) {
        return None;
    }
    let path = if raw {
        path
    } else {
        &path[..path.iter().position(|&b| b == b'?' || b == b'#').unwrap_or(path.len())]
    };
    Some((path, raw))
}

/// `XX:XX:XX:XX:XX:XX` in hex (uri.zig `isValidMacAddress`).
fn is_mac_address(s: &[u8]) -> bool {
    s.len() == 17
        && s.iter()
            .enumerate()
            .all(|(i, &b)| if i % 3 == 2 { b == b':' } else { b.is_ascii_hexdigit() })
}
