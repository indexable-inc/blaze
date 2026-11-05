//! The source editor on the right of the window: line-number gutter, caret-row and
//! matching-bracket highlight, Rust highlighting, Tab as four spaces. Every edit goes to the
//! blaze daemon over its API (blaze-api), the way an editor tells a language server: the text
//! is committed to the daemon's head of the checkout as a write, and the checkout built from
//! it, so a running window can take the patch. The file is written after that, for the edit to
//! stay; nothing waits for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::highlight::Highlighter;
use crate::theme::Theme;

/// What the sending thread and the window share.
#[derive(Default)]
pub struct Sending {
    /// The newest text not sent yet, when it was typed, and whether the file needs it.
    next: Option<(String, Instant, bool)>,
    /// When the text sent last was typed, and when it was sent: the patch built of it lands
    /// after both. The time between is the edit's wait for the one before to be answered.
    pub in_flight: Option<(Instant, Instant)>,
    /// What the daemon answered last, or why it could not be asked.
    pub answer: Option<Result<String, String>>,
    /// Edits sent so far.
    pub sent: u64,
    /// Why the last write of the file failed.
    pub write_error: Option<String>,
    /// Whether an edit is on its way: the file is not written yet.
    busy: bool,
}

impl Sending {
    /// Whether an edit is not sent and written yet.
    fn waits(&self) -> bool {
        self.busy || self.next.is_some()
    }
}

pub type Shared = Arc<(Mutex<Sending>, Condvar)>;

pub fn lock(shared: &Shared) -> MutexGuard<'_, Sending> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sends the newest text to the daemon, one edit at a time, and writes each to `file` once
/// the daemon has it: what typing gets ahead of the daemon is sent as one edit. A write lands
/// before the next text is sent, so the daemon never finds older text on disk than it has.
fn send_as_typed(file: PathBuf, shared: Shared) {
    loop {
        let (text, _, write) = {
            let mut sending = lock(&shared);
            loop {
                if let Some(next) = sending.next.take() {
                    sending.busy = true;
                    sending.in_flight = Some((next.1, Instant::now()));
                    break next;
                }
                sending = shared.1.wait(sending).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let answer = send(&file, &text);
        if let Err(why) = &answer {
            eprintln!("life: the edit was not built: {why}");
        }
        let written = if write { write_through(&file, &text).err() } else { None };
        let mut sending = lock(&shared);
        sending.answer = Some(answer);
        sending.sent += 1;
        sending.write_error = written;
        sending.busy = false;
    }
}

/// Commits `text` as `file` to the daemon's head of the checkout `file` is in, named as the
/// CLI names it, then builds the checkout: what the build said, or why it failed or the daemon
/// could not be asked.
#[cfg(unix)]
fn send(file: &Path, text: &str) -> Result<String, String> {
    use blaze_api::client::{self, Client};
    use blaze_api::{Level, Mode, Op, Request, Response, Targets, ViewRef};

    let file = std::path::absolute(file).map_err(|it| format!("{}: {it}", file.display()))?;
    let dir = file.parent().ok_or_else(|| format!("{} is in no directory", file.display()))?;
    let root = client::checkout(dir);
    let head = root.to_str().ok_or_else(|| format!("{} is not UTF-8", root.display()))?;
    let path = file
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| format!("{} is not a UTF-8 path under {head}", file.display()))?;
    let socket = client::socket()?;
    let mut daemon = Client::unix(&socket).map_err(|it| format!("no daemon: {it}"))?;
    let view = || ViewRef::Head(head.to_owned());
    let mut said = Vec::new();
    let mut ask =
        |request: Request, what: &str, on: &mut dyn FnMut(&Response)| -> Result<(), String> {
            said.clear();
            let ok = daemon
                .call(request, |response| match response {
                    Response::Error(error) => said.push(match &error.at {
                        Some(at) => format!("{} (at {at})", error.what),
                        None => error.what.clone(),
                    }),
                    Response::Diagnostic(it) if it.level == Level::Error => {
                        said.push(it.message.clone())
                    }
                    other => on(other),
                })
                .map_err(|it| format!("{what}: {it}"))?;
            match (ok, said.is_empty()) {
                (true, _) => Ok(()),
                (false, true) => Err(format!("{what} failed, and the daemon said nothing more")),
                (false, false) => Err(format!("{what}: {}", said.join("; "))),
            }
        };

    let mut base = None;
    ask(Request::Open { view: view() }, &format!("opening {head}"), &mut |response| {
        if let Response::Opened { root } = response {
            base = Some(*root);
        }
    })?;
    let base = base.ok_or_else(|| format!("the daemon opened {head} without saying its root"))?;
    let ops = vec![Op::Write { mode: Mode::Regular, path: path.to_owned(), bytes: text.into() }];
    ask(
        Request::Commit { ops, base, head: head.to_owned() },
        &format!("committing {path}"),
        &mut |_| {},
    )?;
    let targets = Targets {
        cargo: None,
        target: None,
        profile: "dev".to_owned(),
        features: Vec::new(),
        packages: Vec::new(),
        all_features: false,
        no_default_features: false,
    };
    let started = Instant::now();
    let mut members = 0;
    ask(Request::Build { view: view(), targets }, &format!("building {head}"), &mut |response| {
        if let Response::Built { members: built, .. } = response {
            members = *built;
        }
    })?;
    Ok(format!("ok built members={members} ms={}", started.elapsed().as_millis()))
}

#[cfg(not(unix))]
fn send(_: &Path, _: &str) -> Result<String, String> {
    Err("edits go to the daemon over its Unix socket, which this platform's client lacks"
        .to_owned())
}

/// Writes by rename, so that nothing reads a half-written file.
fn write_through(file: &Path, text: &str) -> Result<(), String> {
    let tmp = file.with_extension("rs.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, file))
        .map_err(|why| format!("could not save {}: {why}", file.display()))
}

pub struct Editor {
    /// Where the edits go; `None` when the source was not found.
    pub file: Option<PathBuf>,
    pub text: String,
    /// The text last handed to the sending thread.
    handed: String,
    /// The file as it was last read or written, to see another editor's writes by.
    on_disk: String,
    last_edit: Instant,
    last_poll: Instant,
    pub shared: Shared,
    hl: Highlighter,
    focused_once: bool,
}

impl Editor {
    pub fn new(file: Option<PathBuf>, fallback: &str) -> Editor {
        let text = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_else(|| fallback.to_owned());
        let shared = Shared::default();
        if let Some(file) = &file {
            let (file, shared) = (file.clone(), shared.clone());
            std::thread::Builder::new()
                .name("life-send".to_owned())
                .spawn(move || send_as_typed(file, shared))
                .expect("starting the thread that sends edits");
        }
        Editor {
            file,
            handed: text.clone(),
            on_disk: text.clone(),
            text,
            last_edit: Instant::now() - Duration::from_secs(10),
            last_poll: Instant::now(),
            shared,
            hl: Highlighter::default(),
            focused_once: false,
        }
    }

    /// Hands the text to the sending thread, in place of any it has not sent yet.
    fn hand(&mut self, write: bool) {
        if self.file.is_none() || self.text == self.handed {
            return;
        }
        self.handed = self.text.clone();
        let mut sending = lock(&self.shared);
        let write = write || sending.next.as_ref().is_some_and(|it| it.2);
        sending.next = Some((self.text.clone(), Instant::now(), write));
        drop(sending);
        self.shared.1.notify_one();
    }

    /// Picks up edits made outside the window (another editor), unless we are typing.
    pub fn reload(&mut self) {
        let Some(file) = &self.file else { return };
        if self.last_poll.elapsed() < Duration::from_millis(250)
            || self.last_edit.elapsed() < Duration::from_secs(1)
            || lock(&self.shared).waits()
        {
            return;
        }
        self.last_poll = Instant::now();
        if let Ok(t) = std::fs::read_to_string(file)
            && t != self.on_disk
        {
            self.on_disk = t.clone();
            if t != self.handed {
                // The daemon builds what it is sent, not what it finds on disk.
                self.text = t;
                self.hand(false);
            }
        }
    }

    fn edited(&mut self) {
        self.last_edit = Instant::now();
        self.hand(true);
        self.on_disk = self.text.clone();
    }

    pub fn show(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let m = &t.metrics;
        let id = egui::Id::unique("life-source");
        let font = egui::TextStyle::Monospace.resolve(ui.style());

        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            self.handed.clear();
            self.edited();
        }

        // Tab inserts spaces (egui would insert a literal tab).
        if ui.memory(|mem| mem.has_focus(id))
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
            && let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id)
            && let Some(r) = st.cursor.char_range()
        {
            let at = r.primary.index.0;
            let byte = self.text.char_indices().nth(at).map_or(self.text.len(), |(b, _)| b);
            self.text.insert_str(byte, &" ".repeat(m.tab_spaces));
            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(at + m.tab_spaces),
            )));
            st.store(ui.ctx(), id);
            self.edited();
        }

        let chars: Vec<char> = self.text.chars().collect();
        let cursor = egui::TextEdit::load_state(ui.ctx(), id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| r.primary.index.0);
        let cur_line = cursor.map_or(0, |c| chars.iter().take(c).filter(|&&c| c == '\n').count());
        let lines = chars.iter().filter(|&&c| c == '\n').count() + 1;
        let w = lines.to_string().len().max(m.gutter_min_digits);
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // The caret-row band goes under the gutter and the text, so it is painted first.
                let bg = ui.painter().add(egui::Shape::Noop);
                let mut g = egui::text::LayoutJob::default();
                for i in 0..lines {
                    let color = if i == cur_line { t.gutter_fg_current } else { t.gutter_fg };
                    let nl = if i + 1 < lines { "\n" } else { "" };
                    g.append(
                        &format!("{:>w$}{nl}", i + 1),
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color,
                            line_height: Some(m.line_height),
                            ..Default::default()
                        },
                    );
                }
                ui.add_space(m.gutter_left);
                let gal = ui.fonts_mut(|f| f.layout_job(g));
                let (rect, _) = ui.allocate_exact_size(gal.size(), egui::Sense::hover());
                ui.painter().galley(rect.min, gal, t.gutter_fg);
                ui.add_space(m.gutter_right);

                let hl = &mut self.hl;
                // No wrapping: one source line is one row, so the gutter lines up (long lines scroll).
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                    let job = hl.job(t, buf.as_str(), font, f32::INFINITY);
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let out = egui::TextEdit::multiline(&mut self.text)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .layouter(&mut layouter)
                    .desired_width(f32::INFINITY)
                    .desired_rows(lines.max(m.editor_min_rows))
                    .lock_focus(true)
                    .show(ui);
                if let Some(r) = out.cursor_range {
                    let origin = out.galley_pos.to_vec2();
                    let row = out.galley.pos_from_cursor(r.primary).translate(origin);
                    let full = egui::Rect::from_x_y_ranges(
                        ui.min_rect().left() - m.current_line_bleed..=ui.max_rect().right(),
                        row.top()..=row.bottom(),
                    );
                    let mut shapes =
                        vec![egui::Shape::rect_filled(full, m.current_line_radius, t.current_line)];
                    if r.primary == r.secondary {
                        let glyph = ui.fonts_mut(|f| f.glyph_width(&font, ' '));
                        if let Some((a, b)) = match_bracket(&chars, r.primary.index.0) {
                            for i in [a, b] {
                                let p = out
                                    .galley
                                    .pos_from_cursor(egui::text::CCursor::new(i))
                                    .translate(origin);
                                let cell =
                                    egui::Rect::from_min_size(p.min, egui::vec2(glyph, p.height()));
                                shapes.push(egui::Shape::rect_filled(
                                    cell,
                                    m.bracket_radius,
                                    t.bracket_match,
                                ));
                            }
                        }
                    }
                    ui.painter().set(bg, egui::Shape::Vec(shapes));
                }
                if !self.focused_once {
                    out.response.request_focus();
                    let mut st = out.state.clone();
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(0),
                    )));
                    st.store(ui.ctx(), id);
                    self.focused_once = true;
                }
                if out.response.changed() {
                    self.edited();
                }
            });
        });
    }
}

/// Index of the bracket matching the one at (or just before) `cursor`, as (this, other) char indices.
fn match_bracket(chars: &[char], cursor: usize) -> Option<(usize, usize)> {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    for at in [cursor, cursor.wrapping_sub(1)] {
        let Some(&c) = chars.get(at) else { continue };
        for (o, cl) in PAIRS {
            if c == o {
                let mut depth = 0i32;
                for (i, &d) in chars.iter().enumerate().skip(at) {
                    depth += i32::from(d == o) - i32::from(d == cl);
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            } else if c == cl {
                let mut depth = 0i32;
                for i in (0..=at).rev() {
                    depth += i32::from(chars[i] == cl) - i32::from(chars[i] == o);
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            }
        }
    }
    None
}
