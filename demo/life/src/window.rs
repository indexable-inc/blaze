//! The window: a readout at the top, the running Game of Life on the left with its tiers under
//! it, and the editor on the right, in the Islands Dark theme. blaze-live applies each patch
//! on a thread of its own the moment it arrives; the next step of the simulation runs the new
//! rules from main.rs.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use blaze_live::{Live, Outcome};

use crate::editor::{self, Editor};
use crate::theme::{self, Theme};
use crate::tiers::TiersPanel;
use crate::{H, Life, W};

/// How long one frame steps the simulation for when `step_ms()` is 0.
const FLAT_OUT: Duration = Duration::from_millis(10);

/// How the daemon's last answer went: whether it built, and what it said.
#[derive(Default)]
struct BuildStatus {
    ok: bool,
    text: String,
}

impl BuildStatus {
    fn of(answer: Option<&Result<String, String>>) -> BuildStatus {
        match answer {
            None => BuildStatus { ok: true, text: String::new() },
            Some(Ok(said)) => BuildStatus { ok: true, text: said.clone() },
            Some(Err(why)) => BuildStatus { ok: false, text: why.clone() },
        }
    }
}

struct App {
    life: Life,
    /// What the thread that applies patches says, and for an applied one the time from the
    /// keystroke to the new code in place and the part of it the edit waited to be sent.
    applied: Receiver<(Outcome, Option<(Duration, Duration)>)>,
    /// Whether live edits are on, or why not.
    live_note: String,
    /// The last patch outcome, and keystroke-to-running time for an applied one, with the
    /// part of it the edit waited in the editor for the one before.
    outcome: Option<(String, bool)>,
    edit_to_running: Option<(Duration, Duration)>,
    patches: u64,
    editor: Editor,
    build: BuildStatus,
    theme: &'static Theme,
    fps_at: Instant,
    fps_frames: u64,
    frames: u64,
    fps: f64,
    /// The generation when `fps_at` was, and generations a second since.
    fps_generation: u64,
    generations: f64,
    tiers: TiersPanel,
}

/// Islands Dark: lighter window behind, darker rounded islands separated by a small gap.
fn island(t: &Theme, inner_v: i8, inner_h: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(t.island_bg)
        .corner_radius(t.metrics.island_radius)
        .inner_margin(egui::Margin::symmetric(inner_h, inner_v))
        .outer_margin(egui::Margin::same(t.metrics.island_gap))
}

fn apply_theme(ctx: &egui::Context, t: &Theme) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = t.window_bg;
    v.window_fill = t.island_bg;
    v.extreme_bg_color = t.island_bg;
    v.override_text_color = Some(t.fg);
    v.selection.bg_fill = t.selection;
    v.selection.stroke = egui::Stroke::NONE;
    v.text_cursor.stroke = egui::Stroke::new(t.metrics.caret_width, t.caret);
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(t.metrics.font_size));
    });
}

impl App {
    fn header(&self, ui: &mut egui::Ui) {
        let t = self.theme;
        ui.horizontal_wrapped(|ui| {
            match (&self.outcome, self.edit_to_running) {
                (Some((_, true)), Some((total, queued))) => {
                    let ms = |it: Duration| it.as_secs_f64() * 1e3;
                    ui.heading(
                        egui::RichText::new(format!("edit to running: {:.1} ms", ms(total)))
                            .color(t.function),
                    );
                    // Where the time of this one edit went, adding up to it: waiting in the
                    // editor for the edit before to be answered, then the daemon's build, the
                    // patch's way here and its switch.
                    let daemon = total.saturating_sub(queued);
                    ui.colored_label(
                        t.comment,
                        format!(
                            "(in the editor {:.1} ms, build and switch {:.1} ms)",
                            ms(queued),
                            ms(daemon)
                        ),
                    );
                }
                (Some((_, true)), None) => {
                    ui.heading(egui::RichText::new("edit applied").color(t.function));
                }
                (Some((what, false)), _) => {
                    ui.heading(egui::RichText::new(what).color(t.error));
                }
                _ => {
                    ui.heading(egui::RichText::new("edit the code on the right").color(t.fg));
                }
            }
            let note = match &self.outcome {
                Some((what, true)) => format!("   {what}"),
                _ => format!("   {}", self.live_note),
            };
            ui.colored_label(t.comment, note);
        });
        let build = if self.build.text.is_empty() {
            String::new()
        } else if self.build.ok {
            format!("   |   {}", self.build.text.lines().last().unwrap_or(""))
        } else {
            "   |   the last build failed".to_owned()
        };
        ui.colored_label(
            t.number,
            format!(
                "{:.0} fps   |   {:.0} generations/s   |   generation {}   |   {} edits sent, {} patches applied{build}",
                self.fps,
                self.generations,
                self.life.generation,
                editor::lock(&self.editor.shared).sent,
                self.patches
            ),
        );
    }

    fn footer(&self, ui: &mut egui::Ui) {
        let t = self.theme;
        let write_error = editor::lock(&self.editor.shared).write_error.clone();
        if let Some(why) = write_error {
            ui.colored_label(t.error, why);
        } else if self.editor.file.is_none() {
            ui.colored_label(
                t.comment,
                "no source to edit: start the window from demo/life, or set BLAZE_LIFE_FILE",
            );
        } else if self.build.ok || self.build.text.is_empty() {
            let file =
                self.editor.file.as_ref().map(|f| f.display().to_string()).unwrap_or_default();
            ui.colored_label(
                t.comment,
                format!("no errors   ·   built as you type, saved to {file}"),
            );
        } else {
            ui.colored_label(t.error, "the edit was not built; the window keeps the old code:");
            egui::ScrollArea::vertical().id_salt("errs").show(ui, |ui| {
                ui.monospace(&self.build.text);
            });
        }
    }

    fn grid(&self, ui: &mut egui::Ui) {
        let t = self.theme;
        let avail = ui.available_size();
        let side = (avail.x / W as f32).min(avail.y / H as f32).max(1.0);
        let size = egui::vec2(side * W as f32, side * H as f32);
        let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
        let rect = egui::Rect::from_center_size(rect.center(), size);
        let painter = ui.painter_at(rect);
        for y in 0..H {
            for x in 0..W {
                let age = self.life.age[y * W + x];
                let color = if age > 0 {
                    let [_, r, g, b] = crate::color(age).to_be_bytes();
                    egui::Color32::from_rgb(r, g, b)
                } else {
                    t.dead_cell
                };
                let min = rect.min + egui::vec2(x as f32, y as f32) * side;
                painter.rect_filled(
                    egui::Rect::from_min_size(min, egui::vec2(side - 1.0, side - 1.0)),
                    0.0,
                    color,
                );
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok((outcome, took)) = self.applied.try_recv() {
            let applied = matches!(outcome, Outcome::Applied { .. });
            if applied {
                self.patches += 1;
                self.edit_to_running = took;
            }
            self.outcome = Some((outcome.to_string(), applied));
        }
        let flat_out = crate::step_ms() == 0;
        if flat_out {
            let until = Instant::now() + FLAT_OUT;
            while Instant::now() < until {
                self.life.step();
            }
            self.life.next_step = Instant::now();
        } else if Instant::now() >= self.life.next_step {
            self.life.step();
            self.life.next_step = Instant::now() + Duration::from_millis(crate::step_ms());
        }
        self.editor.reload();
        self.build = BuildStatus::of(editor::lock(&self.editor.shared).answer.as_ref());
        self.tiers.refresh();
        self.frames += 1;
        if self.fps_at.elapsed() > Duration::from_millis(500) {
            let seconds = self.fps_at.elapsed().as_secs_f64();
            self.fps = (self.frames - self.fps_frames) as f64 / seconds;
            self.generations = (self.life.generation - self.fps_generation) as f64 / seconds;
            self.fps_frames = self.frames;
            self.fps_generation = self.life.generation;
            self.fps_at = Instant::now();
        }
        if flat_out || self.tiers.lit() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let t = self.theme;
        let m = &t.metrics;
        apply_theme(root.ctx(), t);

        egui::Panel::top("head")
            .frame(island(t, m.header_pad_v, m.header_pad_h))
            .show(root, |ui| self.header(ui));

        egui::Panel::right("editor")
            .frame(island(t, 0, 0))
            .default_size(m.editor_panel_width)
            .resizable(true)
            .show(root, |ui| {
                // Fixed-height footer so the editor never jumps when an error appears or clears.
                egui::Panel::bottom("err")
                    .frame(
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(m.footer_pad_h, m.footer_pad_v)),
                    )
                    .exact_size(m.footer_height)
                    .show(ui, |ui| self.footer(ui));
                egui::Frame::new().inner_margin(egui::Margin::symmetric(0, m.editor_pad_v)).show(
                    ui,
                    |ui| {
                        self.editor.show(ui, t);
                    },
                );
            });

        egui::Panel::bottom("tiers")
            .frame(island(t, m.tiers_pad, m.tiers_pad))
            .exact_size(m.tiers_height)
            .show(root, |ui| self.tiers.show(ui, t));

        egui::CentralPanel::default()
            .frame(island(t, m.life_pad, m.life_pad))
            .show(root, |ui| self.grid(ui));
    }
}

/// The file to edit: `BLAZE_LIFE_FILE`, else src/main.rs here (the launcher starts the window
/// in the crate's copy), else the crate this was built from, if it is still there.
fn source_file() -> Option<PathBuf> {
    if let Some(file) = std::env::var_os("BLAZE_LIFE_FILE") {
        return Some(file.into());
    }
    [
        PathBuf::from("src/main.rs"),
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .map(|p| std::path::absolute(&p).unwrap_or(p))
}

/// `BLAZE_LIFE_HEADLESS=1`: the same program without a window, for harness/live_latency.py to
/// time edits of the demo by (its executable is the window's, eframe and all). Each patch is
/// applied as it lands, as in the window; what `step_ms()` returns is printed whenever it
/// changes, with the time it was seen.
fn headless(mut life: Life) -> eframe::Result {
    use std::io::Write as _;
    let at_us = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| it.as_micros())
    };
    let live = match Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    live.apply_as_they_come(move |outcome| {
        println!("outcome: {outcome} at_us={}", at_us());
        let _ = std::io::stdout().flush();
    });
    println!("ready");
    let mut seen = None;
    loop {
        let now = crate::step_ms();
        if seen != Some(now) {
            println!("answer {now} at_us={}", at_us());
            let _ = std::io::stdout().flush();
            seen = Some(now);
        }
        if Instant::now() >= life.next_step {
            life.step();
            life.next_step = Instant::now() + Duration::from_millis(now.min(1000));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn run(life: Life) -> eframe::Result {
    if std::env::var_os("BLAZE_LIFE_HEADLESS").is_some() {
        return headless(life);
    }
    let (live, live_note) = match Live::start() {
        Ok(live) => (Some(live), "live edits on".to_owned()),
        Err(why) => (None, format!("live edits off: {why}")),
    };
    let tiering_off = if live.is_none() {
        Some("tiering: off, as live edits are".to_owned())
    } else if std::env::var_os("BLAZE_TIER").is_some_and(|it| it == "off") {
        Some("tiering: off (BLAZE_TIER=off)".to_owned())
    } else {
        None
    };
    eprintln!("life: {live_note}");
    // Not include_str!("main.rs"): that would make every edit of main.rs an edit of this file too.
    let editor = Editor::new(
        source_file(),
        "// src/main.rs was not found next to this window, or where it was built.\n",
    );
    let (tell, applied) = channel();
    let shared = editor.shared.clone();
    let app = App {
        life,
        applied,
        live_note,
        outcome: None,
        edit_to_running: None,
        patches: 0,
        editor,
        build: BuildStatus::default(),
        theme: &theme::ISLANDS_DARK,
        fps_at: Instant::now(),
        fps_frames: 0,
        frames: 0,
        fps: 0.0,
        fps_generation: 0,
        generations: 0.0,
        tiers: TiersPanel::new(tiering_off),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(theme::ISLANDS_DARK.metrics.window_size)
            .with_title("blaze: Game of Life, live"),
        ..Default::default()
    };
    eframe::run_native(
        "blaze life",
        options,
        Box::new(move |creation| {
            let ctx = creation.egui_ctx.clone();
            if let Some(live) = live {
                // A step of the simulation that is under way when the rules change finishes
                // with some cells by the old ones, as an edit that lands mid-call does.
                live.apply_as_they_come(move |outcome| {
                    // Timed here, the moment the new code is in place, as the readout says.
                    let took = match outcome {
                        Outcome::Applied { .. } => editor::lock(&shared)
                            .in_flight
                            .take()
                            .map(|(typed, sent)| (typed.elapsed(), sent - typed)),
                        _ => None,
                    };
                    match took {
                        Some((took, queued)) => eprintln!(
                            "life: {outcome}; edit to running {took:.2?}, of it {queued:.2?} in the editor"
                        ),
                        None => eprintln!("life: {outcome}"),
                    }
                    let _ = tell.send((outcome, took));
                    ctx.request_repaint();
                });
            }
            Ok(Box::new(app))
        }),
    )
}
