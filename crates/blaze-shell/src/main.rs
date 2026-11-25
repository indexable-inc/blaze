//! `blaze-shell`: native macOS GUI shell rendered by Metal + `blaze-ui`'s Rust-authored SDF
//! & glyph shaders + CoreText, driven by `blaze-sh`'s sans-IO structured pipeline engine,
//! O(1) CoW session forks, `blaze-vt` PTY terminal blocks, and live Rust cells via `blaze run`.

#[cfg(target_os = "macos")]
fn main() {
    macos_app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    headless_repl();
}

#[cfg(not(target_os = "macos"))]
fn headless_repl() {
    use std::io::{self, BufRead, Write};

    use blaze_own::Alloc;
    use blaze_sh::{EngineState, Value};
    use blaze_shell::host::run_command_line;

    let alloc = Alloc::global();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_owned());
    let env_owned: Vec<(String, String)> = std::env::vars().collect();
    let env_refs: Vec<(&str, &str)> = env_owned
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut state = EngineState::new(&cwd, &env_refs, alloc);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(src) = line else { break };
        let trimmed = src.trim();
        if trimmed == "exit" || trimmed == "quit" {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        let res = run_command_line(trimmed, state.fork(), false);
        for l in &res.streamed_lines {
            let _ = writeln!(stdout, "{l}");
        }
        if !matches!(res.outcome.value, Value::Nothing) {
            let mut s = String::new();
            let _ = res.outcome.value.format_cell(&mut s);
            let _ = writeln!(stdout, "{s}");
        }
        let _ = stdout.flush();
        state = res.outcome.state;
    }
}

#[cfg(target_os = "macos")]
mod macos_app {
    use std::path::PathBuf;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use blaze_own::Alloc;
    use blaze_sh::{EngineState, Value};
    use blaze_ui::shaders::ViewportUniform;
    use blaze_vt::input::key::{Key, OptionAsAlt, Platform};
    use blaze_vt::terminal::{Effect, Opts as VtOpts, Terminal};
    use objc2::rc::autoreleasepool;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEventMask,
        NSEventModifierFlags, NSEventType, NSPasteboard, NSPasteboardTypeString, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
    use objc2_metal::MTLCreateSystemDefaultDevice;
    use objc2_quartz_core::CAMetalLayer;

    use blaze_shell::blocks::{BlockBody, BlockId, BlockStore};
    use blaze_shell::editor::Editor;
    use blaze_shell::font::Fonts;
    use blaze_shell::host::{InteractiveSpawn, run_command_line};
    use blaze_shell::keys;
    use blaze_shell::layout::{LayoutInput, build_scene, layout_fullscreen_terminal};
    use blaze_shell::prompt::{GitStatusWorker, PromptInfo};
    use blaze_shell::pty::{Pty, Read as PtyRead};
    use blaze_shell::render::{FrameDraw, PIXEL_FORMAT, Renderer};
    use blaze_shell::rust_cell;
    use blaze_shell::view::{HitAction, Scene};

    const FONT_POINTS: f64 = 13.0;

    struct CliOpts {
        demo: bool,
        scroll_bottom: bool,
        screenshot: Option<PathBuf>,
    }

    fn parse_cli() -> CliOpts {
        let mut demo = false;
        let mut scroll_bottom = false;
        let mut screenshot = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => demo = true,
                "--scroll-bottom" => scroll_bottom = true,
                "--screenshot" => {
                    if let Some(p) = args.next() {
                        screenshot = Some(PathBuf::from(p));
                    }
                }
                _ => {}
            }
        }
        CliOpts { demo, scroll_bottom, screenshot }
    }

    struct ActivePty {
        block_id: BlockId,
        pty: Pty,
        started: Instant,
    }

    struct ShellApp {
        store: BlockStore,
        editor: Editor,
        prompt: PromptInfo,
        home: String,
        git_worker: GitStatusWorker,
        scroll_y: f32,
        auto_scroll_bottom: bool,
        active_pty: Option<ActivePty>,
        last_scene: Scene,
    }

    impl ShellApp {
        fn new() -> Self {
            let alloc = Alloc::global();
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_owned());
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
            let env_owned: Vec<(String, String)> = std::env::vars().collect();
            let env_refs: Vec<(&str, &str)> = env_owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let state = EngineState::new(&cwd, &env_refs, alloc);
            let prompt = PromptInfo::from_cwd(&cwd, &home, 0, 0);
            let git_worker = GitStatusWorker::spawn();
            git_worker.request(&cwd);

            Self {
                store: BlockStore::new(state),
                editor: Editor::new(),
                prompt,
                home,
                git_worker,
                scroll_y: 0.0,
                auto_scroll_bottom: false,
                active_pty: None,
                last_scene: Scene::new(),
            }
        }

        fn refresh_prompt(&mut self, last_exit: i32, last_duration_ms: u64) {
            let cwd = self.store.active_state().cwd.as_ref().to_owned();
            let prev_dirty = if self.prompt.cwd == cwd { self.prompt.git_dirty } else { None };
            self.prompt = PromptInfo::from_cwd(&cwd, &self.home, last_exit, last_duration_ms);
            self.prompt.git_dirty = prev_dirty;
            self.git_worker.request(&cwd);
        }

        fn submit_command(&mut self, src: &str, term_cols: u16, term_rows: u16, cell_px: (u32, u32)) {
            let trimmed = src.trim();
            if trimmed.is_empty() {
                return;
            }
            if trimmed == "clear" {
                self.store.clear_active_branch();
                self.scroll_y = 0.0;
                return;
            }

            let state_before = self.store.active_state().fork();
            let prompt_snap = self.prompt.clone();
            let started_ms = now_ms();
            let t0 = Instant::now();

            let res = run_command_line(trimmed, state_before.fork(), true);
            let duration_ms = t0.elapsed().as_millis() as u64;

            if let Some(spawn) = res.interactive_spawn {
                self.spawn_pty_block(
                    trimmed.to_owned(),
                    prompt_snap,
                    state_before,
                    res.outcome.state,
                    started_ms,
                    spawn,
                    term_cols,
                    term_rows,
                    cell_px,
                );
                return;
            }

            let exit_code = res.outcome.exit_code;
            let state_after = res.outcome.state;
            let body = if let Some(meta) = res.rust_meta {
                if let Value::Error(e) = &res.outcome.value {
                    BlockBody::Error { err: (**e).clone() }
                } else {
                    BlockBody::RustCell {
                        code: meta.code,
                        compile_ms: meta.compile_ms,
                        run_ms: meta.run_ms,
                        stdout: meta.stdout,
                        value: res.outcome.value,
                    }
                }
            } else if let Value::Error(e) = &res.outcome.value {
                BlockBody::Error { err: (**e).clone() }
            } else if !res.streamed_lines.is_empty() {
                BlockBody::StreamAndValue {
                    lines: res.streamed_lines,
                    value: res.outcome.value,
                }
            } else {
                BlockBody::Value(res.outcome.value)
            };

            let block_id = self.store.record_block(
                trimmed.to_owned(),
                prompt_snap,
                state_before,
                state_after.fork(),
                started_ms,
                duration_ms,
                exit_code,
                body,
            );
            self.store.set_active_state(state_after);

            if let Some(label) = res.forked_label {
                let _ = self.store.fork_from_block(block_id, Some(&label));
            }
            self.refresh_prompt(exit_code, duration_ms);
            self.auto_scroll_bottom = true;
        }

        #[allow(clippy::too_many_arguments)]
        fn spawn_pty_block(
            &mut self,
            source: String,
            prompt_snap: PromptInfo,
            state_before: EngineState<'static>,
            state_after: EngineState<'static>,
            started_ms: u64,
            spawn: InteractiveSpawn,
            cols: u16,
            rows: u16,
            cell_px: (u32, u32),
        ) {
            let mut argv_owned = Vec::with_capacity(1 + spawn.args.len());
            argv_owned.push(spawn.program.clone());
            argv_owned.extend(spawn.args.iter().cloned());
            let argv_refs: Vec<&str> = argv_owned.iter().map(String::as_str).collect();
            let px = (
                cols.saturating_mul(cell_px.0 as u16),
                rows.saturating_mul(cell_px.1 as u16),
            );

            let alloc = Alloc::global();
            match Pty::spawn(&argv_refs, &spawn.cwd, cols, rows, px) {
                Ok(pty) => {
                    let mut opts = VtOpts::new(cols, rows, 4 << 20);
                    opts.cell_px = Some(blaze_vt::input::mouse::Pixels {
                        width: cell_px.0,
                        height: cell_px.1,
                    });
                    if let Ok(term) = Terminal::new_in(opts, alloc) {
                        let block_id = self.store.record_block(
                            source,
                            prompt_snap,
                            state_before,
                            state_after.fork(),
                            started_ms,
                            0,
                            0,
                            BlockBody::Terminal { term, running: true, exit_code: None },
                        );
                        self.store.set_active_state(state_after);
                        self.active_pty = Some(ActivePty {
                            block_id,
                            pty,
                            started: Instant::now(),
                        });
                        self.auto_scroll_bottom = true;
                    }
                }
                Err(msg) => {
                    let err = blaze_sh::ShellError::new(
                        blaze_sh::ErrorKind::External,
                        blaze_sh::Span::ZERO,
                        &format!("failed to spawn `{}` on PTY: {msg}", spawn.program),
                        alloc,
                    );
                    self.store.record_block(
                        source,
                        prompt_snap,
                        state_before,
                        state_after,
                        started_ms,
                        0,
                        1,
                        BlockBody::Error { err },
                    );
                    self.refresh_prompt(1, 0);
                    self.auto_scroll_bottom = true;
                }
            }
        }

        fn populate_demo(&mut self, cols: u16, rows: u16, cell_px: (u32, u32), scroll_bottom: bool) {
            self.submit_command("ls | sort-by size -r | take 6", cols, rows, cell_px);
            self.submit_command("sys", cols, rows, cell_px);
            self.submit_command("let x = 21", cols, rows, cell_px);

            // Demonstrate O(1) CoW session fork in the top branch bar.
            let _ = self.store.fork_active_branch(Some("experiment"));
            self.submit_command(
                "[[crate lines status]; [blaze-sh 1840 true] [blaze-vt 3120 true] [blaze-shell 2290 true]] | sort-by lines -r",
                cols,
                rows,
                cell_px,
            );
            if rust_cell::find_blaze_binary(None).is_some() {
                self.submit_command("rust { (1..=10).sum::<i64>() }", cols, rows, cell_px);
            }
            self.submit_command("ls | get nonexistent_col", cols, rows, cell_px);
            self.editor.set_text("ls | where size > 1KB | sort-by size -r".to_owned());
            self.auto_scroll_bottom = scroll_bottom;
            self.scroll_y = 0.0;
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn grid_for(size: NSSize, fonts: &Fonts, scale: f64) -> (u16, u16) {
        let m = fonts.metrics;
        let w = ((size.width - 36.0) * scale / m.cell_w as f64)
            .floor()
            .max(20.0);
        let h = ((size.height - 140.0) * scale / m.cell_h as f64)
            .floor()
            .max(8.0);
        (w as u16, h as u16)
    }

    pub fn run() {
        let cli = parse_cli();

        // SAFETY: single-threaded startup before any worker thread is spawned.
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            std::env::set_var("TERM_PROGRAM", "blaze-shell");
        }

        let mtm = MainThreadMarker::new().expect("blaze-shell must run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(if cli.screenshot.is_some() {
            NSApplicationActivationPolicy::Accessory
        } else {
            NSApplicationActivationPolicy::Regular
        });
        app.finishLaunching();

        let init_w = 1140.0;
        let init_h = 820.0;
        let rect = NSRect::new(NSPoint::new(160.0, 120.0), NSSize::new(init_w, init_h));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: valid content rect, style mask, and buffered backing store on main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: `window` stays alive for the duration of `run()`.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("blaze-shell"));
        let view = window.contentView().expect("window contentView");

        let renderer = Renderer::new(MTLCreateSystemDefaultDevice().expect("no Metal GPU device"));
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(renderer.device()));
        layer.setPixelFormat(PIXEL_FORMAT);
        layer.setFramebufferOnly(true);
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);

        if cli.screenshot.is_none() {
            window.center();
            window.makeKeyAndOrderFront(None);
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }

        let mut scale = window.backingScaleFactor().max(1.0);
        let mut fonts = Fonts::new(FONT_POINTS, scale);
        let mut size = view.bounds().size;
        layer.setContentsScale(scale);
        layer.setDrawableSize(NSSize::new(size.width * scale, size.height * scale));

        let mut shell = ShellApp::new();
        let (init_cols, init_rows) = grid_for(size, &fonts, scale);
        let cell_px = (fonts.metrics.cell_w as u32, fonts.metrics.cell_h as u32);
        if cli.demo {
            shell.populate_demo(init_cols, init_rows, cell_px, cli.scroll_bottom);
        }

        // Offscreen deterministic screenshot mode: render 2 frames (warming glyph atlas) and exit.
        if let Some(shot_path) = cli.screenshot {
            let metrics = fonts.cell_metrics(scale);
            let mut scene = build_scene(&LayoutInput {
                store: &shell.store,
                editor: &shell.editor,
                prompt: &shell.prompt,
                viewport_w: size.width as f32,
                viewport_h: size.height as f32,
                metrics,
                scroll_y: shell.scroll_y,
                cursor_visible: true,
            });
            let max_s = (scene.total_content_height - scene.scroll_viewport_height).max(0.0);
            if shell.auto_scroll_bottom && max_s > 0.0 {
                shell.scroll_y = max_s;
                scene = build_scene(&LayoutInput {
                    store: &shell.store,
                    editor: &shell.editor,
                    prompt: &shell.prompt,
                    viewport_w: size.width as f32,
                    viewport_h: size.height as f32,
                    metrics,
                    scroll_y: shell.scroll_y,
                    cursor_visible: true,
                });
            }
            let glyphs = fonts.rasterize_runs(&scene.text_runs, scale);
            let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale);
            for (slot, bytes) in fonts.pending.drain(..) {
                renderer.upload(slot, &bytes);
            }
            let vp = ViewportUniform {
                size: [size.width as f32, size.height as f32],
                dpr: scale as f32,
                time: 0.0,
            };
            let frame = FrameDraw {
                vp,
                clear: scene.clear_color,
                clips: &scene.clips,
                rects: &scene.rects,
                glyphs: &glyphs,
                over_rects: &scene.over_rects,
                overlay_rects: &scene.overlay_rects,
                overlay_glyphs: &overlay_glyphs,
            };
            let w_px = (size.width * scale).round().max(1.0) as usize;
            let h_px = (size.height * scale).round().max(1.0) as usize;
            if let Err(e) = renderer.render_to_png(w_px, h_px, &frame, &shot_path) {
                eprintln!("blaze-shell screenshot failed: {e}");
                std::process::exit(1);
            }
            return;
        }

        let mut pty_buf = vec![0u8; 1 << 16];
        let mut keybuf = [0u8; 128];
        let mut dirty = true;
        let mut blink_on = true;
        let mut last_blink = Instant::now();

        loop {
            let mut quit = false;
            autoreleasepool(|_| {
                // SAFETY: `NSDefaultRunLoopMode` is a valid Foundation static string.
                let mode = unsafe { NSDefaultRunLoopMode };
                let until = NSDate::dateWithTimeIntervalSinceNow(if dirty { 0.0 } else { 0.008 });
                while let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    Some(&until),
                    mode,
                    true,
                ) {
                    match ev.r#type() {
                        NSEventType::KeyDown if window.isKeyWindow() => {
                            let flags = ev.modifierFlags();
                            let chars = ev
                                .charactersIgnoringModifiers()
                                .map(|s| s.to_string())
                                .unwrap_or_default();

                            // Global Command shortcuts.
                            if flags.contains(NSEventModifierFlags::Command) {
                                match chars.as_str() {
                                    "q" | "w" => {
                                        quit = true;
                                        continue;
                                    }
                                    "k" if shell.active_pty.is_none() => {
                                        shell.store.clear_active_branch();
                                        shell.scroll_y = 0.0;
                                        dirty = true;
                                        continue;
                                    }
                                    "b" if shell.active_pty.is_none() => {
                                        let _ = shell.store.fork_active_branch(None);
                                        shell.refresh_prompt(shell.prompt.last_exit, shell.prompt.last_duration_ms);
                                        dirty = true;
                                        continue;
                                    }
                                    "a" if shell.active_pty.is_none() => {
                                        shell.editor.select_all();
                                        dirty = true;
                                        continue;
                                    }
                                    "c" if shell.active_pty.is_none() => {
                                        let copy_text = shell
                                            .editor
                                            .selected_text()
                                            .map(str::to_owned)
                                            .unwrap_or_else(|| shell.editor.text.clone());
                                        if !copy_text.is_empty() {
                                            let pb = NSPasteboard::generalPasteboard();
                                            // SAFETY: valid static AppKit pasteboard type.
                                            let t = unsafe { NSPasteboardTypeString };
                                            pb.clearContents();
                                            pb.setString_forType(&NSString::from_str(&copy_text), t);
                                        }
                                        continue;
                                    }
                                    "v" => {
                                        let pb = NSPasteboard::generalPasteboard();
                                        // SAFETY: valid static AppKit pasteboard type.
                                        let t = unsafe { NSPasteboardTypeString };
                                        if let Some(s) = pb.stringForType(t) {
                                            let pasted = s.to_string();
                                            if let Some(ap) = &shell.active_pty {
                                                if let Some(blk) = shell.store.block(ap.block_id)
                                                    && let BlockBody::Terminal { term, .. } = &blk.body
                                                {
                                                    let mut data = pasted.into_bytes();
                                                    let p = blaze_vt::input::paste::encode(
                                                        &mut data,
                                                        term.paste_opts(),
                                                    );
                                                    let mut out = Vec::new();
                                                    out.extend_from_slice(p.prefix);
                                                    out.extend_from_slice(p.data);
                                                    out.extend_from_slice(p.suffix);
                                                    ap.pty.write_all(&out);
                                                }
                                            } else {
                                                shell.editor.insert_str(&pasted);
                                                dirty = true;
                                            }
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }

                            // Forward directly to live PTY when an interactive external command is running.
                            if let Some(ap) = &shell.active_pty {
                                if let Some(blk) = shell.store.block(ap.block_id)
                                    && let BlockBody::Terminal { term, .. } = &blk.body
                                {
                                    let text = keys::text_of(&ev);
                                    let kev = keys::event(&ev, &text);
                                    let eo = term.input_opts(Platform::MacOs, OptionAsAlt::False);
                                    if let Ok(bytes) =
                                        blaze_vt::input::key::encode(&kev, &eo, &mut keybuf)
                                    {
                                        ap.pty.write_all(bytes);
                                    }
                                }
                                blink_on = true;
                                last_blink = Instant::now();
                                dirty = true;
                                continue;
                            }

                            // Interactive shell editor key handling.
                            let key = keys::key_of(ev.keyCode());
                            let shift = flags.contains(NSEventModifierFlags::Shift);
                            let ctrl = flags.contains(NSEventModifierFlags::Control);
                            let alt = flags.contains(NSEventModifierFlags::Option);
                            let text = keys::text_of(&ev);

                            if shell.editor.reverse_search.is_some() {
                                if ctrl && key == Key::KeyR {
                                    shell.editor.reverse_search_next();
                                } else if key == Key::Escape || (ctrl && key == Key::KeyC) {
                                    shell.editor.cancel_reverse_search();
                                } else if key == Key::Enter || key == Key::NumpadEnter {
                                    shell.editor.accept_reverse_search();
                                } else if key == Key::Backspace {
                                    shell.editor.reverse_search_backspace();
                                } else if !ctrl && !text.is_empty() {
                                    for ch in text.chars() {
                                        shell.editor.reverse_search_push(ch);
                                    }
                                }
                            } else if ctrl {
                                match key {
                                    Key::KeyR => shell.editor.start_reverse_search(),
                                    Key::KeyC => {
                                        shell.editor.dismiss_completions();
                                        shell.editor.clear_line();
                                    }
                                    Key::KeyU => shell.editor.clear_line(),
                                    Key::KeyK => shell.editor.kill_to_end(),
                                    Key::KeyW => shell.editor.kill_word_back(),
                                    Key::KeyA => shell.editor.move_home(shift),
                                    Key::KeyE => shell.editor.move_end(shift),
                                    _ => {}
                                }
                            } else {
                                match key {
                                    Key::Enter | Key::NumpadEnter => {
                                        if !shell.editor.completions.is_empty() {
                                            shell.editor.apply_selected_completion();
                                        } else if shift || shell.editor.is_incomplete() {
                                            shell.editor.insert_char('\n');
                                        } else {
                                            let cmd = shell.editor.take_submitted();
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx);
                                        }
                                    }
                                    Key::Tab => {
                                        let st = shell.store.active_state().fork();
                                        shell.editor.trigger_completion(&st);
                                    }
                                    Key::Escape => shell.editor.dismiss_completions(),
                                    Key::Backspace => shell.editor.backspace(),
                                    Key::Delete => shell.editor.delete_forward(),
                                    Key::ArrowLeft => {
                                        if alt {
                                            shell.editor.move_word_left(shift);
                                        } else {
                                            shell.editor.move_left(shift);
                                        }
                                    }
                                    Key::ArrowRight => {
                                        if alt {
                                            shell.editor.move_word_right(shift);
                                        } else {
                                            shell.editor.move_right(shift);
                                        }
                                    }
                                    Key::ArrowUp => shell.editor.move_up(),
                                    Key::ArrowDown => shell.editor.move_down(),
                                    Key::Home => shell.editor.move_home(shift),
                                    Key::End => shell.editor.move_end(shift),
                                    Key::PageUp => {
                                        shell.scroll_y = (shell.scroll_y
                                            - shell.last_scene.scroll_viewport_height * 0.75)
                                            .max(0.0);
                                        shell.auto_scroll_bottom = false;
                                    }
                                    Key::PageDown => {
                                        let max_s = (shell.last_scene.total_content_height
                                            - shell.last_scene.scroll_viewport_height)
                                            .max(0.0);
                                        shell.scroll_y = (shell.scroll_y
                                            + shell.last_scene.scroll_viewport_height * 0.75)
                                            .min(max_s);
                                        shell.auto_scroll_bottom = false;
                                    }
                                    _ => {
                                        if !text.is_empty() {
                                            shell.editor.insert_str(&text);
                                        }
                                    }
                                }
                            }
                            blink_on = true;
                            last_blink = Instant::now();
                            dirty = true;
                            continue;
                        }
                        NSEventType::ScrollWheel => {
                            let dy = ev.scrollingDeltaY();
                            if shell.store.is_alternate_screen() {
                                if let Some(ap) = &shell.active_pty
                                    && dy.abs() > 0.5
                                    && let Some(blk) = shell.store.block(ap.block_id)
                                    && let BlockBody::Terminal { term, .. } = &blk.body
                                {
                                    let up = dy > 0.0;
                                    let seq: &[u8] = if term.mode(blaze_vt::terminal::Mode::CursorKeys)
                                    {
                                        if up { b"\x1bOA" } else { b"\x1bOB" }
                                    } else if up {
                                        b"\x1b[A"
                                    } else {
                                        b"\x1b[B"
                                    };
                                    ap.pty.write_all(seq);
                                }
                            } else {
                                let step = if ev.hasPreciseScrollingDeltas() {
                                    dy as f32
                                } else {
                                    dy as f32 * 24.0
                                };
                                let max_s = (shell.last_scene.total_content_height
                                    - shell.last_scene.scroll_viewport_height)
                                    .max(0.0);
                                shell.scroll_y = (shell.scroll_y - step).clamp(0.0, max_s);
                                shell.auto_scroll_bottom = false;
                                dirty = true;
                            }
                        }
                        NSEventType::LeftMouseDown if window.isKeyWindow() => {
                            if !shell.store.is_alternate_screen() {
                                let pt = ev.locationInWindow();
                                let x = pt.x as f32;
                                let y = (size.height - pt.y) as f32;
                                if let Some(action) = shell.last_scene.hit_test(x, y) {
                                    match action {
                                        HitAction::SwitchBranch(bid) => {
                                            if shell.store.switch_branch(bid) {
                                                shell.scroll_y = 0.0;
                                                shell.auto_scroll_bottom = true;
                                                shell.refresh_prompt(0, 0);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::ForkActiveBranch => {
                                            let _ = shell.store.fork_active_branch(None);
                                            shell.refresh_prompt(
                                                shell.prompt.last_exit,
                                                shell.prompt.last_duration_ms,
                                            );
                                            dirty = true;
                                        }
                                        HitAction::ForkBlock(block_id) => {
                                            if shell.store.fork_from_block(block_id, None).is_some() {
                                                shell.auto_scroll_bottom = true;
                                                shell.refresh_prompt(0, 0);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::ToggleFold(block_id) => {
                                            shell.store.toggle_fold(block_id);
                                            dirty = true;
                                        }
                                        HitAction::ToggleExpandCell { block, row, col } => {
                                            shell.store.toggle_expand_cell(block, row, col);
                                            dirty = true;
                                        }
                                        HitAction::SelectCompletion(idx) => {
                                            shell.editor.apply_completion(idx);
                                            dirty = true;
                                        }
                                    }
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                    app.sendEvent(&ev);
                }
                app.updateWindows();
            });

            if quit || !window.isVisible() {
                if let Some(ap) = shell.active_pty.take() {
                    ap.pty.hangup();
                }
                break;
            }

            // Poll asynchronous git status worker (single atomic check when idle).
            if let Some((cwd, git_dirty)) = shell.git_worker.poll()
                && cwd == shell.prompt.cwd
            {
                shell.prompt.git_dirty = Some(git_dirty);
                dirty = true;
            }

            // Pump live PTY output into its `blaze-vt` terminal block.
            let mut pty_finished: Option<(BlockId, i32, u64)> = None;
            if let Some(ap) = &shell.active_pty {
                let mut closed = false;
                loop {
                    match ap.pty.read(&mut pty_buf) {
                        PtyRead::Bytes(n) => {
                            if let Some(blk) = shell.store.block_mut(ap.block_id)
                                && let BlockBody::Terminal { term, .. } = &mut blk.body
                            {
                                term.feed(&pty_buf[..n]);
                                let replies = term.take_replies();
                                if !replies.is_empty() {
                                    ap.pty.write_all(&replies);
                                }
                                let effects = term.take_effects();
                                for eff in effects.iter() {
                                    if let Effect::TitleChanged = eff {
                                        let t = String::from_utf8_lossy(term.title()).into_owned();
                                        window.setTitle(&NSString::from_str(if t.is_empty() {
                                            "blaze-shell"
                                        } else {
                                            &t
                                        }));
                                    }
                                }
                            }
                            dirty = true;
                        }
                        PtyRead::Nothing => break,
                        PtyRead::Closed => {
                            closed = true;
                            break;
                        }
                    }
                }
                if let Some(code) = ap.pty.try_wait() {
                    pty_finished = Some((ap.block_id, code, ap.started.elapsed().as_millis() as u64));
                } else if closed {
                    pty_finished = Some((ap.block_id, 0, ap.started.elapsed().as_millis() as u64));
                }
            }
            if let Some((block_id, code, dur_ms)) = pty_finished {
                shell.active_pty = None;
                if let Some(blk) = shell.store.block_mut(block_id) {
                    blk.exit_code = code;
                    blk.duration_ms = dur_ms;
                    if let BlockBody::Terminal { running, exit_code, .. } = &mut blk.body {
                        *running = false;
                        *exit_code = Some(code);
                    }
                }
                window.setTitle(&NSString::from_str("blaze-shell"));
                shell.refresh_prompt(code, dur_ms);
                shell.auto_scroll_bottom = true;
                dirty = true;
            }

            // Handle window resize and backing scale changes.
            let new_size = view.bounds().size;
            let new_scale = window.backingScaleFactor().max(1.0);
            if new_size != size || (new_scale - scale).abs() > f64::EPSILON {
                if (new_scale - scale).abs() > f64::EPSILON {
                    fonts = Fonts::new(FONT_POINTS, new_scale);
                    scale = new_scale;
                    layer.setContentsScale(scale);
                }
                size = new_size;
                layer.setDrawableSize(NSSize::new(size.width * scale, size.height * scale));
                if let Some(ap) = &shell.active_pty {
                    let (cols, rows) = grid_for(size, &fonts, scale);
                    let px = (
                        cols.saturating_mul(fonts.metrics.cell_w as u16),
                        rows.saturating_mul(fonts.metrics.cell_h as u16),
                    );
                    ap.pty.resize(cols, rows, px);
                    if let Some(blk) = shell.store.block_mut(ap.block_id)
                        && let BlockBody::Terminal { term, .. } = &mut blk.body
                    {
                        term.resize(cols, rows);
                    }
                }
                dirty = true;
            }

            if last_blink.elapsed().as_millis() > 550 {
                blink_on = !blink_on;
                last_blink = Instant::now();
                dirty = true;
            }

            if dirty {
                let metrics = fonts.cell_metrics(scale);
                let cursor_vis = blink_on && window.isKeyWindow();
                let mut scene = if shell.store.is_alternate_screen()
                    && let Some(tid) = shell.store.active_running_terminal()
                    && let Some(blk) = shell.store.block(tid)
                    && let BlockBody::Terminal { term, .. } = &blk.body
                {
                    layout_fullscreen_terminal(
                        term,
                        size.width as f32,
                        size.height as f32,
                        metrics,
                        cursor_vis,
                    )
                } else {
                    let first = build_scene(&LayoutInput {
                        store: &shell.store,
                        editor: &shell.editor,
                        prompt: &shell.prompt,
                        viewport_w: size.width as f32,
                        viewport_h: size.height as f32,
                        metrics,
                        scroll_y: shell.scroll_y,
                        cursor_visible: cursor_vis,
                    });
                    let max_s = (first.total_content_height - first.scroll_viewport_height).max(0.0);
                    let target_scroll = if shell.auto_scroll_bottom {
                        shell.auto_scroll_bottom = false;
                        max_s
                    } else {
                        shell.scroll_y.clamp(0.0, max_s)
                    };
                    if (target_scroll - shell.scroll_y).abs() > 0.5 {
                        shell.scroll_y = target_scroll;
                        build_scene(&LayoutInput {
                            store: &shell.store,
                            editor: &shell.editor,
                            prompt: &shell.prompt,
                            viewport_w: size.width as f32,
                            viewport_h: size.height as f32,
                            metrics,
                            scroll_y: shell.scroll_y,
                            cursor_visible: cursor_vis,
                        })
                    } else {
                        first
                    }
                };

                let glyphs = fonts.rasterize_runs(&scene.text_runs, scale);
                let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale);
                for (slot, bytes) in fonts.pending.drain(..) {
                    renderer.upload(slot, &bytes);
                }
                let vp = ViewportUniform {
                    size: [size.width as f32, size.height as f32],
                    dpr: scale as f32,
                    time: 0.0,
                };
                let frame = FrameDraw {
                    vp,
                    clear: scene.clear_color,
                    clips: &scene.clips,
                    rects: &scene.rects,
                    glyphs: &glyphs,
                    over_rects: &scene.over_rects,
                    overlay_rects: &scene.overlay_rects,
                    overlay_glyphs: &overlay_glyphs,
                };
                renderer.draw(&layer, &frame);
                // Keep only hit regions + scroll metadata in `last_scene` for instant mouse handling.
                scene.rects.clear();
                scene.text_runs.clear();
                scene.over_rects.clear();
                scene.overlay_rects.clear();
                scene.overlay_text.clear();
                shell.last_scene = scene;
                dirty = false;
            }
        }
    }
}
