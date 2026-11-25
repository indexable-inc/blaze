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
    use blaze_sh::{
        Bytes, ColDisplay, DateTime, EngineState, ErrorKind, Nanos, PathKind, Record, ShellError,
        Span, Table, Value,
    };
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
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use blaze_own::Alloc;
    use blaze_sh::{
        Bytes, ColDisplay, DateTime, EngineState, ErrorKind, Nanos, PathKind, Record, ShellError,
        Span, Table, Value,
    };
    use blaze_ui::shaders::ViewportUniform;
    use blaze_vim::{
        KeyCode as VimKeyCode, KeyEvent as VimKeyEvent, Mode as VimMode, Mods as VimMods,
        Outcome as VimOutcome,
    };
    use blaze_vt::input::key::{Key, OptionAsAlt, Platform};
    use blaze_vt::terminal::{Effect, Opts as VtOpts, Terminal};
    use objc2::rc::autoreleasepool;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor, NSEventMask,
        NSEventModifierFlags, NSEventType, NSPasteboard, NSPasteboardTypeString, NSWindow,
        NSWindowStyleMask, NSWindowTitleVisibility,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};
    use objc2_metal::MTLCreateSystemDefaultDevice;
    use objc2_quartz_core::CAMetalLayer;

    use blaze_shell::agent_hub::{AgentHub, AgentHubAction, AgentRecord, AgentStatus, layout_agent_hub};
    use blaze_shell::blocks::{BlockBody, BlockId, BlockStore};
    use blaze_shell::editor::{Editor, HistoryEntry};
    use blaze_shell::font::Fonts;
    use blaze_shell::host::{
        InteractiveSpawn, RustCellMeta, run_command_line, run_command_line_with_pid,
    };
    use blaze_shell::keys;
    use blaze_shell::layout::{LayoutInput, TOP_BAR_H, build_scene, layout_fullscreen_terminal};
    use blaze_shell::prompt::{GitStatusWorker, PromptInfo, try_native_git_command};
    use blaze_shell::pty::{Pty, Read as PtyRead};
    use blaze_shell::render::{FrameDraw, PIXEL_FORMAT, Renderer};
    use blaze_shell::rust_cell;
    use blaze_shell::svg_icons::IconAtlas;
    use blaze_shell::view::{
        ActiveDrag, ActivePopover, CursorTrail, DragPayload, HitAction, HitRect, HoverKind,
        OutputNav, OutputVisualMode, Scene, TextSelection, compute_text_selection,
        extract_visual_lines, output_nav_geometry, select_at_point,
    };
    use blaze_shell::yazi::{YaziAction, YaziState, layout_yazi_view, load_file_preview_lines};

    const FONT_POINTS: f64 = 13.0;
    const MIN_FONT_POINTS: f64 = 8.5;
    const MAX_FONT_POINTS: f64 = 28.0;
    const ZOOM_STEP_POINTS: f64 = 1.5;
    const CURSOR_TRAIL_SECS: f32 = 0.20;
    /// macOS `FullSizeContentView` native window resize border zone (px).
    const RESIZE_EDGE: f32 = 8.0;

    #[inline]
    fn on_resize_edge(x: f32, y: f32, w: f32, h: f32) -> bool {
        x <= RESIZE_EDGE || x >= w - RESIZE_EDGE || y >= h - RESIZE_EDGE || y <= 4.0
    }

    struct CliOpts {
        demo: bool,
        demo_popup: bool,
        demo_history: bool,
        scroll_bottom: bool,
        screenshot: Option<PathBuf>,
    }

    fn parse_cli() -> CliOpts {
        let mut demo = false;
        let mut demo_popup = false;
        let mut demo_history = false;
        let mut scroll_bottom = false;
        let mut screenshot = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--demo" => demo = true,
                "--demo-popup" => {
                    demo = true;
                    demo_popup = true;
                }
                "--demo-history" => {
                    demo = true;
                    demo_history = true;
                }
                "--scroll-bottom" => scroll_bottom = true,
                "--screenshot" => {
                    if let Some(p) = args.next() {
                        screenshot = Some(PathBuf::from(p));
                    }
                }
                _ => {}
            }
        }
        CliOpts { demo, demo_popup, demo_history, scroll_bottom, screenshot }
    }

    struct PendingDrag {
        start_x: f32,
        start_y: f32,
        x: f32,
        y: f32,
        active: bool,
        over_composer: bool,
        payload: DragPayload,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CursorKind {
        Arrow,
        Pointer,
        IBeam,
        OpenHand,
        ClosedHand,
    }

    fn apply_cursor(cur: &mut CursorKind, next: CursorKind) {
        if *cur == next {
            return;
        }
        *cur = next;
        let cursor = match next {
            CursorKind::Arrow => NSCursor::arrowCursor(),
            CursorKind::Pointer => NSCursor::pointingHandCursor(),
            CursorKind::IBeam => NSCursor::IBeamCursor(),
            CursorKind::OpenHand => NSCursor::openHandCursor(),
            CursorKind::ClosedHand => NSCursor::closedHandCursor(),
        };
        cursor.set();
    }

    fn cursor_for_point(
        scene: &Scene,
        alt_screen: bool,
        dragging: bool,
        selecting_text: bool,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> CursorKind {
        if x < 0.0 || y < 0.0 || x > w || y > h {
            return CursorKind::Arrow;
        }
        if dragging {
            return CursorKind::ClosedHand;
        }
        if alt_screen || selecting_text {
            return CursorKind::IBeam;
        }
        if scene.hit_test(x, y).is_some() {
            return CursorKind::Pointer;
        }
        if scene.composer_rect.contains(x, y) {
            return CursorKind::IBeam;
        }
        if scene.drag_source_at(x, y).is_some() {
            return CursorKind::OpenHand;
        }
        if y >= TOP_BAR_H
            && scene.text_runs.iter().any(|r| {
                !r.text.is_empty()
                    && y >= r.y - 1.0
                    && y <= r.y + 18.0
                    && x >= r.x
                    && x <= r.x + r.text.chars().count() as f32 * 9.0
            })
        {
            return CursorKind::IBeam;
        }
        CursorKind::Arrow
    }

    struct ActivePty {
        block_id: BlockId,
        pty: Pty,
        started: Instant,
    }

    /// Lossless `Send` mirror of `blaze_sh::Value` so background command threads can return
    /// typed tables, records, paths, sizes, dates, and spanned errors to the UI thread safely.
    enum SendValue {
        Nothing,
        Bool(bool),
        Int(i64),
        Float(f64),
        Size(Bytes),
        Duration(Nanos),
        Date(DateTime),
        Range(blaze_sh::RangeVal),
        String(String),
        Path(String, PathKind),
        Binary(Vec<u8>),
        List(Vec<SendValue>),
        Record(Vec<(String, SendValue)>),
        Table {
            columns: Vec<String>,
            displays: Vec<ColDisplay>,
            default_grid: bool,
            rows: Vec<Vec<SendValue>>,
        },
        Error {
            kind: ErrorKind,
            span: Span,
            message: String,
            help: Option<String>,
        },
    }

    impl SendValue {
        fn from_value(v: &Value<'_>) -> Self {
            match v {
                Value::Nothing => Self::Nothing,
                Value::Bool(b) => Self::Bool(*b),
                Value::Int(n) => Self::Int(*n),
                Value::Float(f) => Self::Float(*f),
                Value::Size(b) => Self::Size(*b),
                Value::Duration(d) => Self::Duration(*d),
                Value::Date(dt) => Self::Date(*dt),
                Value::String(s) => Self::String(s.as_ref().to_owned()),
                Value::Path { path, kind } => Self::Path(path.as_ref().to_owned(), *kind),
                Value::Binary(b) => Self::Binary(b.as_ref().to_vec()),
                Value::List(xs) => Self::List(xs.iter().map(Self::from_value).collect()),
                Value::Record(rec) => Self::Record(
                    rec.iter()
                        .map(|(k, val)| (k.to_owned(), Self::from_value(val)))
                        .collect(),
                ),
                Value::Table(t) => {
                    let columns = t.columns.iter().map(|c| c.as_ref().to_owned()).collect();
                    let displays = t.col_display.iter().copied().collect();
                    let mut rows = Vec::with_capacity(t.nrows());
                    for r in 0..t.nrows() {
                        let mut row = Vec::with_capacity(t.ncols());
                        for c in 0..t.ncols() {
                            row.push(
                                t.cell(r, c)
                                    .map(Self::from_value)
                                    .unwrap_or(Self::Nothing),
                            );
                        }
                        rows.push(row);
                    }
                    Self::Table { columns, displays, default_grid: t.default_grid, rows }
                }
                Value::Error(e) => Self::Error {
                    kind: e.kind,
                    span: e.span,
                    message: e.message.as_str().to_owned(),
                    help: e.help.as_ref().map(|h| h.as_str().to_owned()),
                },
                Value::Range(r) => Self::Range(*r),
                Value::Closure(_) | Value::CellPath(_) => {
                    let mut s = String::new();
                    let _ = v.format_cell(&mut s);
                    Self::String(s)
                }
            }
        }

        fn into_value(self, alloc: Alloc<'static>) -> Value<'static> {
            match self {
                Self::Nothing => Value::Nothing,
                Self::Bool(b) => Value::Bool(b),
                Self::Int(n) => Value::Int(n),
                Self::Float(f) => Value::Float(f),
                Self::Size(b) => Value::Size(b),
                Self::Duration(d) => Value::Duration(d),
                Self::Date(dt) => Value::Date(dt),
                Self::Range(r) => Value::Range(r),
                Self::String(s) => Value::string(&s, alloc),
                Self::Path(p, kind) => Value::path(&p, kind, alloc),
                Self::Binary(b) => Value::binary(&b, alloc),
                Self::List(xs) => {
                    let mut out = blaze_sh::PVec::new(alloc);
                    for x in xs {
                        out.push(x.into_value(alloc));
                    }
                    Value::List(out)
                }
                Self::Record(entries) => {
                    let mut rec = Record::new(alloc);
                    for (k, v) in entries {
                        rec.insert_str(&k, v.into_value(alloc));
                    }
                    Value::Record(rec)
                }
                Self::Table { columns, displays, default_grid, rows } => {
                    let col_refs: Vec<&str> = columns.iter().map(String::as_str).collect();
                    let mut t = Table::from_col_names(&col_refs, alloc);
                    t.default_grid = default_grid;
                    for (col, disp) in columns.iter().zip(displays.into_iter()) {
                        t.set_col_display(col, disp);
                    }
                    for row in rows {
                        let vals: Vec<Value<'static>> =
                            row.into_iter().map(|v| v.into_value(alloc)).collect();
                        t.push_row_values(&vals);
                    }
                    Value::Table(t)
                }
                Self::Error { kind, span, message, help } => {
                    let mut err = ShellError::new(kind, span, &message, alloc);
                    if let Some(h) = help {
                        err = err.with_help(&h, alloc);
                    }
                    Value::Error(blaze_own::Rc::new_in(err, alloc))
                }
            }
        }
    }

    struct AsyncJobDone {
        block_id: BlockId,
        source: String,
        host: String,
        cwd: String,
        branch: Option<String>,
        duration_ms: u64,
        exit_code: i32,
        streamed_lines: Vec<String>,
        rust_meta: Option<RustCellMeta>,
        interactive_spawn: Option<InteractiveSpawn>,
        value: SendValue,
        new_rust_cell: Option<String>,
    }

    /// True for full-screen TUI or interactive REPL programs that genuinely need a foreground
    /// keyboard-attached PTY rather than non-blocking background execution.
    fn is_interactive_tty_cmd(trimmed: &str) -> bool {
        if trimmed.contains('|') {
            return false;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(raw_cmd) = parts.next() else {
            return false;
        };
        let cmd = raw_cmd.trim_start_matches('^').rsplit('/').next().unwrap_or(raw_cmd);
        let has_args = parts.next().is_some();
        match cmd {
            "vim" | "nvim" | "vi" | "hx" | "nano" | "emacs" | "less" | "more" | "man" | "top"
            | "htop" | "btop" | "ssh" | "tmux" | "screen" | "fzf" | "lazygit" | "gitui"
            | "tig" | "claude" | "codex" | "bash" | "zsh" | "fish" | "sh" | "sudo" | "watch"
            | "yazi" | "ranger" | "k9s" => true,
            "python" | "python3" | "node" | "bun" | "irb" | "sqlite3" | "psql" | "redis-cli" => {
                !has_args
            }
            _ => false,
        }
    }

    /// True for commands that may take noticeable time (`rust { ... }`, `:rs`, external CLI
    /// binaries like `git`, `blaze`, `cargo`, `sleep`, etc.) so they run asynchronously while
    /// the next prompt line appears immediately!
    fn should_run_async(trimmed: &str) -> bool {
        if is_interactive_tty_cmd(trimmed) {
            return false;
        }
        if trimmed.starts_with("rust") || trimmed.starts_with(":rs") {
            return true;
        }
        let first = trimmed
            .split(['|', ';', '\n'])
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        if first.is_empty()
            || first.starts_with('^')
            || first.starts_with("./")
            || first.starts_with('/')
        {
            return true;
        }
        // Fast state-mutating or instant in-process builtins stay synchronous (<1ms) so
        // `cd`, `let`, `def`, `alias`, `ls`, `pwd`, `sys`, `$_` pipelines update immediately.
        let instant_or_stateful = matches!(
            first,
            "let" | "mut" | "def" | "alias" | "cd" | "pwd" | "ls" | "sys" | "env" | "echo"
                | "print" | "clear" | "help" | "fork" | "open" | "which" | "if" | "for"
                | "while" | "match" | "try" | "ps"
        ) || first.starts_with('$')
            || first.starts_with('[')
            || first.starts_with('{')
            || first.starts_with('(')
            || first.starts_with('"')
            || first.starts_with('\'')
            || first.chars().next().is_some_and(|c| c.is_ascii_digit());
        !instant_or_stateful && !blaze_sh::is_builtin(first)
    }

    fn copy_to_pasteboard(text: &str) {
        if text.is_empty() {
            return;
        }
        let pb = NSPasteboard::generalPasteboard();
        // SAFETY: valid static AppKit pasteboard type.
        let t = unsafe { NSPasteboardTypeString };
        pb.clearContents();
        pb.setString_forType(&NSString::from_str(text), t);
    }

    fn to_vim_key(key: Key, text: &str, ctrl: bool, alt: bool, shift: bool) -> Option<VimKeyEvent> {
        let mods = VimMods { ctrl, alt, shift };
        let code = match key {
            Key::Escape => VimKeyCode::Escape,
            Key::Enter | Key::NumpadEnter => VimKeyCode::Enter,
            Key::Backspace => VimKeyCode::Backspace,
            Key::Delete => VimKeyCode::Delete,
            Key::ArrowLeft => VimKeyCode::Left,
            Key::ArrowRight => VimKeyCode::Right,
            Key::ArrowUp => VimKeyCode::Up,
            Key::ArrowDown => VimKeyCode::Down,
            Key::Home => VimKeyCode::Home,
            Key::End => VimKeyCode::End,
            Key::Tab => VimKeyCode::Tab,
            _ => {
                if ctrl {
                    let ch = match key {
                        Key::KeyR => 'r',
                        Key::BracketLeft => '[',
                        _ => return None,
                    };
                    VimKeyCode::Char(ch)
                } else {
                    let ch = text.chars().next()?;
                    if ch.is_control() {
                        return None;
                    }
                    VimKeyCode::Char(ch)
                }
            }
        };
        Some(VimKeyEvent { code, mods })
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
        yazi: Option<YaziState>,
        agent_hub: Option<AgentHub>,
        managed_agents: Vec<AgentRecord>,
        last_scene: Scene,
        job_tx: mpsc::Sender<AsyncJobDone>,
        job_rx: mpsc::Receiver<AsyncJobDone>,
        running_jobs: usize,
        output_seq: u64,
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
            let mut editor = Editor::new();
            editor.load_user_abbreviations(&home);
            editor.load_persistent_history(&home);
            let local_host = editor.local_host.clone();
            editor.zoxide.record_visit(&local_host, &cwd, now_ns());
            let (job_tx, job_rx) = mpsc::channel();

            Self {
                store: BlockStore::new(state),
                editor,
                prompt,
                home,
                git_worker,
                scroll_y: 0.0,
                auto_scroll_bottom: false,
                active_pty: None,
                yazi: None,
                agent_hub: None,
                managed_agents: Vec::new(),
                last_scene: Scene::new(),
                job_tx,
                job_rx,
                running_jobs: 0,
                output_seq: 0,
            }
        }

        /// Bind `$_`, `$last`, and `$_<seq>` in `state` so any subsequent command can pipe or
        /// inspect previous outputs with zero copying (O(1) CoW persistent values!).
        fn bind_output_refs(&mut self, state: &mut EngineState<'static>, val: &Value<'static>) {
            if matches!(val, Value::Nothing | Value::Error(_)) {
                return;
            }
            self.output_seq += 1;
            state.set_var("_", val.clone(), false);
            state.set_var("last", val.clone(), false);
            let numbered = format!("_{}", self.output_seq);
            state.set_var(&numbered, val.clone(), false);
        }

        fn refresh_prompt(&mut self, last_exit: i32, last_duration_ms: u64) {
            let cwd = self.store.active_state().cwd.as_ref().to_owned();
            let prev_status = if self.prompt.cwd == cwd {
                self.prompt.git_status.clone()
            } else {
                None
            };
            self.prompt = PromptInfo::from_cwd(&cwd, &self.home, last_exit, last_duration_ms);
            if let Some(st) = prev_status {
                self.prompt.git_branch = Some(st.branch.clone());
                self.prompt.git_detached = st.detached;
                self.prompt.git_status = Some(st);
            }
            self.git_worker.request(&cwd);
        }

        fn recent_table_columns(&self) -> Vec<String> {
            for blk in self.store.active_blocks().iter().rev() {
                if let BlockBody::Value(Value::Table(t)) = &blk.body {
                    return t.columns.iter().map(|c| c.as_ref().to_owned()).collect();
                }
            }
            Vec::new()
        }

        fn refresh_editor_assist(&mut self, open_popup: bool) {
            let st = self.store.active_state().fork();
            let cols = self.recent_table_columns();
            self.editor.refresh_assist(&st, &cols, open_popup);
        }

        fn submit_command(
            &mut self,
            src: &str,
            term_cols: u16,
            term_rows: u16,
            cell_px: (u32, u32),
            allow_async: bool,
        ) {
            let trimmed = src.trim();
            if trimmed.is_empty() {
                return;
            }
            if trimmed == "clear" {
                self.store.clear_active_branch();
                self.scroll_y = 0.0;
                return;
            }

            // 1. Native `y` / `y <dir>`: built-in Yazi-class 3-column GPU file manager with previews!
            if trimmed == "y" || trimmed.starts_with("y ") {
                let arg = trimmed.strip_prefix("y").unwrap_or("").trim();
                let target_dir = if arg.is_empty() {
                    PathBuf::from(&self.prompt.cwd)
                } else {
                    let clean = arg.trim_matches(|c| c == '"' || c == '\'');
                    let p = if let Some(rest) = clean.strip_prefix("~/") {
                        PathBuf::from(&self.home).join(rest)
                    } else if clean.starts_with('/') {
                        PathBuf::from(clean)
                    } else {
                        PathBuf::from(&self.prompt.cwd).join(clean)
                    };
                    if p.is_dir() { p } else { PathBuf::from(&self.prompt.cwd) }
                };
                self.yazi = Some(YaziState::open(&target_dir));
                return;
            }

            // 2. Native `agent`: unified multi-server interactive Agent Hub / prompt launcher!
            if trimmed == "agent" || trimmed.starts_with("agent ") {
                let arg = trimmed.strip_prefix("agent").unwrap_or("").trim();
                if arg.is_empty() {
                    let local_host = self.editor.local_host.clone();
                    let home = self.home.clone();
                    let cwd = self.prompt.cwd.clone();
                    self.agent_hub = Some(AgentHub::open(&local_host, &home, &cwd, &self.managed_agents));
                    return;
                }
                // `agent "<prompt>"` or `agent --host <srv> "<prompt>"`: launch an interactive agent session!
                let (target_host, is_remote, prompt_text) = if let Some(rest) = arg.strip_prefix("--host ") {
                    let mut parts = rest.trim().splitn(2, char::is_whitespace);
                    let h = parts.next().unwrap_or("").to_owned();
                    let p = parts.next().unwrap_or("").trim().trim_matches(|c| c == '"' || c == '\'').to_owned();
                    (h, true, p)
                } else {
                    (
                        self.editor.local_host.clone(),
                        false,
                        arg.trim_matches(|c| c == '"' || c == '\'').to_owned(),
                    )
                };
                self.launch_agent_session(
                    &target_host,
                    is_remote,
                    &self.prompt.cwd.clone(),
                    if prompt_text.is_empty() { None } else { Some(prompt_text) },
                    term_cols,
                    term_rows,
                    cell_px,
                );
                return;
            }

            // 3. Cross-host Zoxide CRDT `cd` / `z`: fuzzy directory jump + remote `host:path` SSH!
            let mut effective_cmd = trimmed.to_owned();
            if let Some(raw_arg) = trimmed.strip_prefix("cd ").or_else(|| trimmed.strip_prefix("z ")) {
                let arg = raw_arg.trim().trim_matches(|c| c == '"' || c == '\'');
                if !arg.is_empty() && arg != "-" && arg != "~" && !arg.starts_with("~/") {
                    // Explicit cross-host jump `host:/path` -> open interactive SSH right into that dir!
                    if let Some((host, rpath)) = arg.split_once(':')
                        && !host.is_empty()
                        && !host.contains('/')
                        && !rpath.is_empty()
                    {
                        let state_before = self.store.active_state().fork();
                        let prompt_snap = self.prompt.clone();
                        let remote_cmd = format!("cd {} && exec ${{SHELL:-/bin/sh}} -l", rpath);
                        let spawn = InteractiveSpawn {
                            program: "ssh".to_owned(),
                            args: vec!["-t".to_owned(), host.to_owned(), remote_cmd],
                            cwd: self.prompt.cwd.clone(),
                            env: Vec::new(),
                        };
                        self.spawn_pty_block(
                            None,
                            trimmed.to_owned(),
                            prompt_snap,
                            state_before.fork(),
                            state_before,
                            now_ms(),
                            spawn,
                            term_cols,
                            term_rows,
                            cell_px,
                        );
                        return;
                    }
                    let local_candidate = if arg.starts_with('/') {
                        PathBuf::from(arg)
                    } else {
                        PathBuf::from(&self.prompt.cwd).join(arg)
                    };
                    if !local_candidate.is_dir() {
                        let local_host = self.editor.local_host.clone();
                        if let Some(best) = self
                            .editor
                            .zoxide
                            .query(arg, &local_host, now_ns(), 1)
                            .into_iter()
                            .next()
                        {
                            if best.is_local {
                                effective_cmd = if best.path.contains(' ') {
                                    format!("cd \"{}\"", best.path)
                                } else {
                                    format!("cd {}", best.path)
                                };
                            } else {
                                let state_before = self.store.active_state().fork();
                                let prompt_snap = self.prompt.clone();
                                let remote_cmd = format!("cd {} && exec ${{SHELL:-/bin/sh}} -l", best.path);
                                let spawn = InteractiveSpawn {
                                    program: "ssh".to_owned(),
                                    args: vec!["-t".to_owned(), best.host.clone(), remote_cmd],
                                    cwd: self.prompt.cwd.clone(),
                                    env: Vec::new(),
                                };
                                self.spawn_pty_block(
                                    None,
                                    format!("cd {}:{}", best.host, best.path),
                                    prompt_snap,
                                    state_before.fork(),
                                    state_before,
                                    now_ms(),
                                    spawn,
                                    term_cols,
                                    term_rows,
                                    cell_px,
                                );
                                return;
                            }
                        } else if let Some(found) = blaze_shell::zoxide::shallow_find_dir(
                            std::path::Path::new(&self.prompt.cwd),
                            arg,
                        ) {
                            let p = found.to_string_lossy().into_owned();
                            effective_cmd = if p.contains(' ') {
                                format!("cd \"{p}\"")
                            } else {
                                format!("cd {p}")
                            };
                        }
                    }
                }
            }
            let trimmed = effective_cmd.as_str();

            let state_before = self.store.active_state().fork();
            let prompt_snap = self.prompt.clone();
            let started_ms = now_ms();

            // 4. Native `blaze-git` rich structured overrides (`git status`, `git branch`, `git stash list`)!
            let t_git = Instant::now();
            if let Some(git_val) = try_native_git_command(trimmed, &prompt_snap.cwd) {
                let duration_ms = t_git.elapsed().as_millis() as u64;
                let mut state_after = state_before.fork();
                self.bind_output_refs(&mut state_after, &git_val);
                let body = BlockBody::Value(git_val);
                self.store.record_block(
                    trimmed.to_owned(),
                    prompt_snap.clone(),
                    state_before,
                    state_after.fork(),
                    started_ms,
                    duration_ms,
                    0,
                    body,
                );
                self.store.set_active_state(state_after);
                self.editor.record_history_entry(HistoryEntry {
                    command: trimmed.to_owned(),
                    host: prompt_snap.host,
                    cwd: prompt_snap.cwd,
                    git_branch: prompt_snap.git_branch,
                    duration_ms,
                    exit_code: 0,
                    timestamp_ns: now_ns(),
                    in_session: true,
                });
                self.refresh_prompt(0, duration_ms);
                self.auto_scroll_bottom = true;
                return;
            }

            // Non-blocking background execution for `rust { ... }` cells and external commands:
            // immediately record a live `Running` block and return control to the next prompt line!
            if allow_async && should_run_async(trimmed) {
                let label = if trimmed.starts_with("rust") || trimmed.starts_with(":rs") {
                    "blaze run…".to_owned()
                } else {
                    "running…".to_owned()
                };
                let pid_slot = Arc::new(AtomicU32::new(0));
                let block_id = self.store.record_block(
                    trimmed.to_owned(),
                    prompt_snap.clone(),
                    state_before.fork(),
                    state_before.fork(),
                    started_ms,
                    0,
                    0,
                    BlockBody::Running { label, pid: Arc::clone(&pid_slot) },
                );
                self.running_jobs += 1;
                self.auto_scroll_bottom = true;

                let tx = self.job_tx.clone();
                let src_owned = trimmed.to_owned();
                let host = prompt_snap.host.clone();
                let cwd = prompt_snap.cwd.clone();
                let branch = prompt_snap.git_branch.clone();
                // Snapshot lightweight cwd/env/vars/rust_cells to run on worker thread.
                let env_owned: Vec<(String, String)> = {
                    let mut out = Vec::new();
                    state_before.env.for_each(&mut |k, v| {
                        out.push((k.as_ref().to_owned(), v.as_ref().to_owned()));
                    });
                    out
                };
                let prior_rust_cells: Vec<String> = state_before
                    .rust_cells
                    .iter()
                    .map(|c| c.code.as_ref().to_owned())
                    .collect();
                let _ = std::thread::Builder::new()
                    .name("blaze-shell-job".to_owned())
                    .spawn(move || {
                        let t0 = Instant::now();
                        let alloc = Alloc::global();
                        let env_refs: Vec<(&str, &str)> = env_owned
                            .iter()
                            .map(|(k, v)| (k.as_str(), v.as_str()))
                            .collect();
                        let mut st = EngineState::new(&cwd, &env_refs, alloc);
                        for cell_src in prior_rust_cells {
                            st.push_rust_cell(&cell_src);
                        }
                        let prior_count = st.rust_cells.len();
                        // Non-interactive background CLI commands run captured (`allow_interactive_pty = false`)
                        // so they publish their OS PID immediately, never hijack the prompt, and update their
                        // single block in-place when complete!
                        let res = run_command_line_with_pid(&src_owned, st, false, Some(&pid_slot));
                        let duration_ms = t0.elapsed().as_millis() as u64;
                        let new_rust_cell = if res.outcome.state.rust_cells.len() > prior_count {
                            res.outcome.state.rust_cells.last().map(|c| c.code.as_ref().to_owned())
                        } else {
                            None
                        };
                        let value = SendValue::from_value(&res.outcome.value);
                        let _ = tx.send(AsyncJobDone {
                            block_id,
                            source: src_owned,
                            host,
                            cwd,
                            branch,
                            duration_ms,
                            exit_code: res.outcome.exit_code,
                            streamed_lines: res.streamed_lines,
                            rust_meta: res.rust_meta,
                            interactive_spawn: res.interactive_spawn,
                            value,
                            new_rust_cell,
                        });
                    });
                return;
            }

            let t0 = Instant::now();
            let res = run_command_line(trimmed, state_before.fork(), true);
            let duration_ms = t0.elapsed().as_millis() as u64;

            if let Some(spawn) = res.interactive_spawn {
                self.spawn_pty_block(
                    None,
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
            let mut state_after = res.outcome.state;
            self.bind_output_refs(&mut state_after, &res.outcome.value);
            let body = body_from_result(res.rust_meta, res.streamed_lines, res.outcome.value);

            let block_id = self.store.record_block(
                trimmed.to_owned(),
                prompt_snap.clone(),
                state_before,
                state_after.fork(),
                started_ms,
                duration_ms,
                exit_code,
                body,
            );
            self.store.set_active_state(state_after);
            let new_cwd = self.store.active_state().cwd.as_ref().to_owned();
            if new_cwd != prompt_snap.cwd {
                let lh = self.editor.local_host.clone();
                self.editor.zoxide.record_visit(&lh, &new_cwd, now_ns());
            }
            self.editor.record_history_entry(HistoryEntry {
                command: trimmed.to_owned(),
                host: prompt_snap.host,
                cwd: prompt_snap.cwd,
                git_branch: prompt_snap.git_branch,
                duration_ms,
                exit_code,
                timestamp_ns: now_ns(),
                in_session: true,
            });

            if let Some(label) = res.forked_label {
                let _ = self.store.fork_from_block(block_id, Some(&label));
            }
            self.refresh_prompt(exit_code, duration_ms);
            self.auto_scroll_bottom = true;
        }

        fn poll_async_jobs(&mut self, cols: u16, rows: u16, cell_px: (u32, u32)) -> bool {
            let mut updated = false;
            while let Ok(done) = self.job_rx.try_recv() {
                self.running_jobs = self.running_jobs.saturating_sub(1);
                updated = true;

                if let Some(spawn) = done.interactive_spawn {
                    // Upgrade the existing `Running` block in-place to a live PTY (never duplicate!).
                    let state_before = self
                        .store
                        .block(done.block_id)
                        .map(|b| b.state_before.fork())
                        .unwrap_or_else(|| self.store.active_state().fork());
                    let prompt_snap = self.prompt.clone();
                    self.spawn_pty_block(
                        Some(done.block_id),
                        done.source,
                        prompt_snap,
                        state_before.fork(),
                        state_before,
                        now_ms(),
                        spawn,
                        cols,
                        rows,
                        cell_px,
                    );
                    continue;
                }

                let alloc = Alloc::global();
                let exit_code = done.exit_code;
                let val = done.value.into_value(alloc);
                let val_for_ref = val.clone();
                let body = body_from_result(done.rust_meta, done.streamed_lines, val);
                let mut active = self.store.active_state().fork();
                self.bind_output_refs(&mut active, &val_for_ref);
                if let Some(cell_code) = done.new_rust_cell {
                    active.push_rust_cell(&cell_code);
                }
                self.store.set_active_state(active.fork());
                if let Some(blk) = self.store.block_mut(done.block_id) {
                    blk.duration_ms = done.duration_ms;
                    blk.exit_code = exit_code;
                    blk.state_after = active;
                    blk.body = body;
                }
                self.editor.record_history_entry(HistoryEntry {
                    command: done.source,
                    host: done.host,
                    cwd: done.cwd,
                    git_branch: done.branch,
                    duration_ms: done.duration_ms,
                    exit_code,
                    timestamp_ns: now_ns(),
                    in_session: true,
                });
                self.refresh_prompt(exit_code, done.duration_ms);
            }
            updated
        }

        #[allow(clippy::too_many_arguments)]
        fn spawn_pty_block(
            &mut self,
            existing_block: Option<BlockId>,
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
                    let child_pid = Some(pty.pid());
                    let mut opts = VtOpts::new(cols, rows, 4 << 20);
                    opts.cell_px = Some(blaze_vt::input::mouse::Pixels {
                        width: cell_px.0,
                        height: cell_px.1,
                    });
                    if let Ok(term) = Terminal::new_in(opts, alloc) {
                        let body = BlockBody::Terminal {
                            term,
                            pid: child_pid,
                            running: true,
                            exit_code: None,
                        };
                        let block_id = if let Some(id) = existing_block
                            && let Some(blk) = self.store.block_mut(id)
                        {
                            blk.body = body;
                            blk.state_after = state_after.fork();
                            id
                        } else {
                            self.store.record_block(
                                source,
                                prompt_snap,
                                state_before,
                                state_after.fork(),
                                started_ms,
                                0,
                                0,
                                body,
                            )
                        };
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
                    if let Some(id) = existing_block
                        && let Some(blk) = self.store.block_mut(id)
                    {
                        blk.exit_code = 1;
                        blk.body = BlockBody::Error { err };
                    } else {
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
                    }
                    self.refresh_prompt(1, 0);
                    self.auto_scroll_bottom = true;
                }
            }
        }

        fn populate_demo(
            &mut self,
            cols: u16,
            rows: u16,
            cell_px: (u32, u32),
            scroll_bottom: bool,
            show_popup: bool,
            show_history: bool,
        ) {
            // Compute real live `blaze-git` Starship status synchronously before demo commands.
            self.prompt.refresh_git_sync();

            self.submit_command("pwd", cols, rows, cell_px, false);
            self.submit_command("ls", cols, rows, cell_px, false);
            self.submit_command("$_ | sort-by size -r | take 4", cols, rows, cell_px, false);
            self.submit_command("sys", cols, rows, cell_px, false);

            // Demonstrate O(1) CoW session fork in the top branch bar.
            let _ = self.store.fork_active_branch(Some("experiment"));
            if rust_cell::find_blaze_binary(None).is_some() {
                self.submit_command("rust { (1..=10).sum::<i64>() }", cols, rows, cell_px, false);
            }
            self.submit_command("ls | get nonexistent_col", cols, rows, cell_px, false);
            self.prompt.refresh_git_sync();

            if show_history {
                let cwd = self.prompt.cwd.clone();
                self.editor.start_reverse_search_in(&cwd);
                for ch in "ls".chars() {
                    self.editor.reverse_search_push(ch);
                }
            } else if show_popup {
                self.editor.set_text_initial("open Ca".to_owned());
                self.refresh_editor_assist(true);
            } else {
                self.editor.set_text_initial("$_ | where size".to_owned());
                self.refresh_editor_assist(false);
            }
            self.auto_scroll_bottom = scroll_bottom;
            self.scroll_y = 0.0;
        }

        #[allow(clippy::too_many_arguments)]
        fn launch_agent_session(
            &mut self,
            host: &str,
            is_remote: bool,
            cwd: &str,
            prompt_text: Option<String>,
            cols: u16,
            rows: u16,
            cell_px: (u32, u32),
        ) {
            let state_before = self.store.active_state().fork();
            let prompt_snap = self.prompt.clone();
            let (program, args, display_cmd) = if is_remote {
                let remote_cmd = match &prompt_text {
                    Some(p) => {
                        let esc = p.replace('\'', "'\\''");
                        format!("claude '{esc}' || exec ${{SHELL:-/bin/sh}} -l")
                    }
                    None => "claude || exec ${SHELL:-/bin/sh} -l".to_owned(),
                };
                (
                    "ssh".to_owned(),
                    vec!["-t".to_owned(), host.to_owned(), remote_cmd],
                    match &prompt_text {
                        Some(p) => format!("agent --host {host} \"{p}\""),
                        None => format!("agent --host {host}"),
                    },
                )
            } else {
                let mut a = Vec::new();
                if let Some(p) = &prompt_text {
                    a.push(p.clone());
                }
                (
                    "claude".to_owned(),
                    a,
                    match &prompt_text {
                        Some(p) => format!("agent \"{p}\""),
                        None => "agent".to_owned(),
                    },
                )
            };
            let id_num = self.managed_agents.len() + 1;
            self.managed_agents.push(AgentRecord {
                id: format!("agent-{id_num}"),
                host: host.to_owned(),
                is_remote,
                name: format!("agent #{id_num} ({host})"),
                prompt_summary: prompt_text
                    .clone()
                    .unwrap_or_else(|| "interactive session".to_owned()),
                cwd: cwd.to_owned(),
                pid: None,
                status: AgentStatus::Running,
                pty_session_idx: Some(id_num - 1),
                preview_lines: vec![
                    format!("host:   {host}"),
                    format!("cwd:    {cwd}"),
                    format!("cmd:    {display_cmd}"),
                ],
            });
            let spawn = InteractiveSpawn {
                program,
                args,
                cwd: self.prompt.cwd.clone(),
                env: Vec::new(),
            };
            self.spawn_pty_block(
                None,
                display_cmd,
                prompt_snap,
                state_before.fork(),
                state_before,
                now_ms(),
                spawn,
                cols,
                rows,
                cell_px,
            );
            if let (Some(ap), Some(rec)) = (&self.active_pty, self.managed_agents.last_mut()) {
                rec.pid = Some(ap.pty.pid());
            }
        }
    }

    fn body_from_result(
        rust_meta: Option<RustCellMeta>,
        streamed_lines: Vec<String>,
        value: Value<'static>,
    ) -> BlockBody {
        if let Some(meta) = rust_meta {
            if let Value::Error(e) = &value {
                BlockBody::Error { err: (**e).clone() }
            } else {
                BlockBody::RustCell {
                    code: meta.code,
                    compile_ms: meta.compile_ms,
                    run_ms: meta.run_ms,
                    stdout: meta.stdout,
                    value,
                }
            }
        } else if let Value::Error(e) = &value {
            BlockBody::Error { err: (**e).clone() }
        } else if !streamed_lines.is_empty() {
            BlockBody::StreamAndValue { lines: streamed_lines, value }
        } else {
            BlockBody::Value(value)
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn now_ns() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    }

    fn grid_for(size: NSSize, fonts: &Fonts, scale: f64) -> (u16, u16) {
        let m = fonts.metrics;
        // Account for horizontal padding + unified `TOP_BAR_H` so full-screen TUIs (`claude`, `vim`)
        // never clip below the bottom edge or collide with session tabs!
        let w = ((size.width - 28.0) * scale / m.cell_w as f64)
            .floor()
            .max(20.0);
        let h = ((size.height - f64::from(TOP_BAR_H) - 20.0) * scale / m.cell_h as f64)
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
            std::env::set_var("CLICOLOR", "1");
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
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
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
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        window.setMovableByWindowBackground(false);
        window.setAcceptsMouseMovedEvents(true);
        let bg_color = objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(
            0x10 as f64 / 255.0,
            0x10 as f64 / 255.0,
            0x11 as f64 / 255.0,
            1.0,
        );
        window.setBackgroundColor(Some(&bg_color));
        // SAFETY: `NSAppearanceNameDarkAqua` is a valid static AppKit constant.
        let dark_name = unsafe { objc2_app_kit::NSAppearanceNameDarkAqua };
        if let Some(app_dark) = objc2_app_kit::NSAppearance::appearanceNamed(dark_name) {
            use objc2_app_kit::NSAppearanceCustomization as _;
            window.setAppearance(Some(&app_dark));
        }
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
        let mut font_points: f64 = FONT_POINTS;
        let mut fonts = Fonts::new(font_points, scale);
        let mut icon_atlas = IconAtlas::new(scale);
        let mut size = view.bounds().size;
        layer.setContentsScale(scale);
        layer.setDrawableSize(NSSize::new(size.width * scale, size.height * scale));

        let mut shell = ShellApp::new();
        let (init_cols, init_rows) = grid_for(size, &fonts, scale);
        let cell_px = (fonts.metrics.cell_w as u32, fonts.metrics.cell_h as u32);
        if cli.demo {
            shell.populate_demo(
                init_cols,
                init_rows,
                cell_px,
                cli.scroll_bottom,
                cli.demo_popup,
                cli.demo_history,
            );
        }

        // Offscreen deterministic screenshot mode: render 1 frame (warming atlases) and exit.
        if let Some(shot_path) = cli.screenshot {
            let metrics = fonts.cell_metrics(scale);
            let frame_now = now_ns();
            let zoom_pct = ((font_points / FONT_POINTS) * 100.0).round() as u16;
            let mut scene = build_scene(&LayoutInput {
                store: &shell.store,
                editor: &shell.editor,
                prompt: &shell.prompt,
                viewport_w: size.width as f32,
                viewport_h: size.height as f32,
                metrics,
                scroll_y: shell.scroll_y,
                cursor_visible: true,
                now_ns: frame_now,
                zoom_pct,
                inline_titlebar: true,
                drag: None,
                cursor_trail: None,
                text_selection: None,
                active_popover: None,
                mouse_pos: None,
                output_nav: None,
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
                    now_ns: frame_now,
                    zoom_pct,
                    inline_titlebar: true,
                    drag: None,
                    cursor_trail: None,
                    text_selection: None,
                    active_popover: None,
                    mouse_pos: None,
                    output_nav: None,
                });
            }
            let glyphs = fonts.rasterize_runs(&scene.text_runs, scale);
            let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale);
            for (slot, bytes) in fonts.pending.drain(..) {
                renderer.upload(slot, &bytes);
            }
            let icons = icon_atlas.rasterize_quads(&scene.icon_quads);
            let overlay_icons = icon_atlas.rasterize_quads(&scene.overlay_icons);
            for (slot, rgba) in icon_atlas.pending.drain(..) {
                renderer.upload_icon(slot, &rgba);
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
                icons: &icons,
                over_rects: &scene.over_rects,
                overlay_rects: &scene.overlay_rects,
                overlay_icons: &overlay_icons,
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
        let mut last_cursor_rect: Option<[f32; 4]> = None;
        let mut trail_prev: Option<[f32; 4]> = None;
        let mut trail_start: Option<Instant> = None;
        let mut last_running_tick = Instant::now();
        let mut pending_drag: Option<PendingDrag> = None;
        let mut selecting_composer = false;
        let mut selecting_output = false;
        let mut output_selection: Option<TextSelection> = None;
        let mut output_nav: Option<OutputNav> = None;
        let mut active_popover: Option<ActivePopover> = None;
        let mut popup_engaged = false;
        let mut zoom_changed = false;
        let mut cur_cursor = CursorKind::Arrow;
        let mut last_mouse: Option<(f32, f32)> = None;

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
                            let key = keys::key_of(ev.keyCode());
                            let shift = flags.contains(NSEventModifierFlags::Shift);
                            let ctrl = flags.contains(NSEventModifierFlags::Control);
                            let alt = flags.contains(NSEventModifierFlags::Option);
                            let chars = ev
                                .charactersIgnoringModifiers()
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            let lower = chars.to_ascii_lowercase();

                            // Global macOS Command shortcuts (`Cmd+Q/W/K/B/A/C/X/V/Z`, zoom `Cmd+`/`Cmd-`/`Cmd+0`,
                            // and natural line navigation `Cmd+Left/Right/Backspace`).
                            if flags.contains(NSEventModifierFlags::Command) {
                                // Zoom (`Cmd+=`/`Cmd++`, `Cmd+-`, `Cmd+0`).
                                if matches!(lower.as_str(), "=" | "+")
                                    || matches!(key, Key::Equal | Key::NumpadAdd)
                                {
                                    font_points = (font_points + ZOOM_STEP_POINTS).min(MAX_FONT_POINTS);
                                    zoom_changed = true;
                                    dirty = true;
                                    continue;
                                }
                                if lower == "-" || matches!(key, Key::Minus | Key::NumpadSubtract) {
                                    font_points = (font_points - ZOOM_STEP_POINTS).max(MIN_FONT_POINTS);
                                    zoom_changed = true;
                                    dirty = true;
                                    continue;
                                }
                                if lower == "0" || matches!(key, Key::Digit0 | Key::Numpad0) {
                                    font_points = FONT_POINTS;
                                    zoom_changed = true;
                                    dirty = true;
                                    continue;
                                }

                                // Navigation & editing inside the composer when no PTY is running.
                                if shell.active_pty.is_none() {
                                    match key {
                                        Key::ArrowLeft => {
                                            shell.editor.move_home(shift);
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                            continue;
                                        }
                                        Key::ArrowRight => {
                                            shell.editor.move_end(shift);
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                            continue;
                                        }
                                        Key::Backspace => {
                                            shell.editor.kill_to_line_start();
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                            continue;
                                        }
                                        _ => {}
                                    }
                                }

                                match lower.as_str() {
                                    "q" | "w" => {
                                        quit = true;
                                        continue;
                                    }
                                    "k" if shell.active_pty.is_none() => {
                                        shell.store.clear_active_branch();
                                        shell.scroll_y = 0.0;
                                        output_selection = None;
                                        dirty = true;
                                        continue;
                                    }
                                    "b" if shell.active_pty.is_none() => {
                                        let _ = shell.store.fork_active_branch(None);
                                        shell.refresh_prompt(shell.prompt.last_exit, shell.prompt.last_duration_ms);
                                        dirty = true;
                                        continue;
                                    }
                                    "z" if shell.active_pty.is_none() => {
                                        if shift {
                                            shell.editor.redo();
                                        } else {
                                            shell.editor.undo();
                                        }
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                        dirty = true;
                                        continue;
                                    }
                                    "a" if shell.active_pty.is_none() => {
                                        output_selection = None;
                                        shell.editor.select_all();
                                        dirty = true;
                                        continue;
                                    }
                                    "x" if shell.active_pty.is_none() => {
                                        if let Some(cut) = shell.editor.cut_selection() {
                                            copy_to_pasteboard(&cut);
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                        }
                                        continue;
                                    }
                                    "c" => {
                                        // Copy selected output/scrollback text first, else composer selection, else full line!
                                        if let Some(doc_sel) = output_selection {
                                            let screen_sel = TextSelection {
                                                anchor: [doc_sel.anchor[0], doc_sel.anchor[1] - shell.scroll_y],
                                                head: [doc_sel.head[0], doc_sel.head[1] - shell.scroll_y],
                                            };
                                            let (_, selected) = compute_text_selection(
                                                &shell.last_scene.text_runs,
                                                fonts.cell_metrics(scale),
                                                screen_sel,
                                                TOP_BAR_H,
                                            );
                                            if !selected.is_empty() {
                                                copy_to_pasteboard(&selected);
                                                continue;
                                            }
                                        }
                                        if shell.active_pty.is_none() {
                                            let copy_text = shell
                                                .editor
                                                .selected_text()
                                                .map(str::to_owned)
                                                .unwrap_or_else(|| shell.editor.text.clone());
                                            copy_to_pasteboard(&copy_text);
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
                                                popup_engaged = false;
                                                shell.refresh_editor_assist(false);
                                                dirty = true;
                                            }
                                        }
                                        continue;
                                    }
                                    _ => {}
                                }
                            }

                            // 0a. Native `y` (Yazi 3-column file manager) keyboard handling.
                            if let Some(yazi) = &mut shell.yazi {
                                let text = keys::text_of(&ev);
                                let kname = match key {
                                    Key::Escape => "Escape",
                                    Key::Enter | Key::NumpadEnter => "Enter",
                                    Key::Backspace => "Backspace",
                                    Key::ArrowUp => "Up",
                                    Key::ArrowDown => "Down",
                                    Key::ArrowLeft => "Left",
                                    Key::ArrowRight => "Right",
                                    _ => "",
                                };
                                let ch = if !ctrl && !alt { text.chars().next() } else { None };
                                let act = yazi.handle_key(kname, ch);
                                match act {
                                    YaziAction::Continue => {}
                                    YaziAction::ExitCancel => {
                                        shell.yazi = None;
                                    }
                                    YaziAction::ExitAndCd(dir) => {
                                        shell.yazi = None;
                                        let dir_s = dir.to_string_lossy().into_owned();
                                        if dir_s != shell.prompt.cwd {
                                            let cmd = if dir_s.contains(' ') {
                                                format!("cd \"{dir_s}\"")
                                            } else {
                                                format!("cd {dir_s}")
                                            };
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx, false);
                                        }
                                    }
                                    YaziAction::InsertPath(p) => {
                                        shell.yazi = None;
                                        shell.editor.enter_vim_insert();
                                        shell.editor.insert_dropped_token(&p);
                                        shell.refresh_editor_assist(false);
                                    }
                                }
                                dirty = true;
                                continue;
                            }

                            // 0b. Native `agent` multi-server interactive fleet hub keyboard handling.
                            if let Some(hub) = &mut shell.agent_hub {
                                let text = keys::text_of(&ev);
                                let kname = match key {
                                    Key::Escape => "Escape",
                                    Key::Enter | Key::NumpadEnter => "Enter",
                                    Key::Backspace => "Backspace",
                                    Key::ArrowUp => "Up",
                                    Key::ArrowDown => "Down",
                                    _ => "",
                                };
                                let ch = if !ctrl && !alt { text.chars().next() } else { None };
                                match hub.handle_key(kname, ch) {
                                    AgentHubAction::Continue => {}
                                    AgentHubAction::Exit => {
                                        shell.agent_hub = None;
                                    }
                                    AgentHubAction::EnterAgent {
                                        host,
                                        is_remote,
                                        cwd,
                                        prompt,
                                        ..
                                    } => {
                                        shell.agent_hub = None;
                                        let (cols, rows) = grid_for(size, &fonts, scale);
                                        let cpx = (
                                            fonts.metrics.cell_w as u32,
                                            fonts.metrics.cell_h as u32,
                                        );
                                        shell.launch_agent_session(
                                            &host, is_remote, &cwd, prompt, cols, rows, cpx,
                                        );
                                    }
                                }
                                dirty = true;
                                continue;
                            }

                            // 0c. Vim Scrollback / Output Navigation Mode (`h`/`j`/`k`/`l`, `v`/`V`, `y` over outputs!).
                            if let Some(mut nav) = output_nav {
                                let text = keys::text_of(&ev);
                                let m = fonts.cell_metrics(scale);
                                let vlines = extract_visual_lines(
                                    &shell.last_scene.text_runs,
                                    m,
                                    TOP_BAR_H,
                                    size.height as f32,
                                );
                                if vlines.is_empty() {
                                    output_nav = None;
                                    dirty = true;
                                    continue;
                                }
                                nav.line = nav.line.min(vlines.len() - 1);
                                let max_col = vlines[nav.line].chars.len().saturating_sub(1);
                                nav.col = nav.col.min(max_col);
                                let ch = if !ctrl && !alt { text.chars().next() } else { None };

                                if nav.pending_g {
                                    nav.pending_g = false;
                                    if ch == Some('g') {
                                        nav.line = 0;
                                        nav.col = 0;
                                        output_nav = Some(nav);
                                        dirty = true;
                                        continue;
                                    }
                                }

                                let mut keep = true;
                                match (key, ch) {
                                    (Key::Escape, _) => {
                                        if nav.visual.is_some() {
                                            nav.visual = None;
                                        } else {
                                            keep = false;
                                        }
                                    }
                                    (_, Some('i' | 'a' | 'q')) => {
                                        keep = false;
                                        if ch != Some('q') {
                                            shell.editor.enter_vim_insert();
                                        }
                                    }
                                    (Key::ArrowLeft, _) | (_, Some('h')) => {
                                        nav.col = nav.col.saturating_sub(1);
                                    }
                                    (Key::ArrowRight, _) | (_, Some('l')) => {
                                        nav.col = (nav.col + 1).min(max_col);
                                    }
                                    (Key::ArrowUp, _) | (_, Some('k')) => {
                                        if nav.line > 0 {
                                            nav.line -= 1;
                                        } else if shell.scroll_y > 0.0 {
                                            shell.scroll_y = (shell.scroll_y - m.ch * 3.0).max(0.0);
                                            shell.auto_scroll_bottom = false;
                                        }
                                    }
                                    (Key::ArrowDown, _) | (_, Some('j')) => {
                                        if nav.line + 1 < vlines.len() {
                                            nav.line += 1;
                                        } else if nav.visual.is_none() {
                                            // Pressing `j` past the bottom line returns smoothly to the prompt!
                                            keep = false;
                                        }
                                    }
                                    (_, Some('0')) => nav.col = 0,
                                    (_, Some('$')) => nav.col = max_col,
                                    (_, Some('g')) => nav.pending_g = true,
                                    (_, Some('G')) => nav.line = vlines.len() - 1,
                                    (_, Some('w')) => {
                                        let row = &vlines[nav.line].chars;
                                        let mut c = nav.col;
                                        while c < row.len() && !row[c].1.is_whitespace() {
                                            c += 1;
                                        }
                                        while c < row.len() && row[c].1.is_whitespace() {
                                            c += 1;
                                        }
                                        nav.col = c.min(max_col);
                                    }
                                    (_, Some('b')) => {
                                        let row = &vlines[nav.line].chars;
                                        let mut c = nav.col.saturating_sub(1);
                                        while c > 0 && row[c].1.is_whitespace() {
                                            c -= 1;
                                        }
                                        while c > 0 && !row[c - 1].1.is_whitespace() {
                                            c -= 1;
                                        }
                                        nav.col = c;
                                    }
                                    (_, Some('v')) => {
                                        nav.visual = match nav.visual {
                                            Some((_, _, OutputVisualMode::Char)) => None,
                                            _ => Some((nav.line, nav.col, OutputVisualMode::Char)),
                                        };
                                    }
                                    (_, Some('V')) => {
                                        nav.visual = match nav.visual {
                                            Some((_, _, OutputVisualMode::Line)) => None,
                                            _ => Some((nav.line, nav.col, OutputVisualMode::Line)),
                                        };
                                    }
                                    (_, Some('y')) | (Key::Enter, _) => {
                                        let (_, _, yanked) = output_nav_geometry(&vlines, m, nav);
                                        if !yanked.is_empty() {
                                            copy_to_pasteboard(&yanked);
                                        }
                                        nav.visual = None;
                                        keep = false;
                                    }
                                    _ => {}
                                }
                                output_nav = if keep { Some(nav) } else { None };
                                dirty = true;
                                continue;
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
                                dirty = true;
                                continue;
                            }

                            // Interactive shell editor key handling.
                            output_selection = None;
                            if active_popover.is_some() {
                                active_popover = None;
                                if key == Key::Escape {
                                    dirty = true;
                                    continue;
                                }
                            }
                            let text = keys::text_of(&ev);

                            if shell.editor.reverse_search.is_some() {
                                // Atuin-class `Ctrl+R` fuzzy history palette.
                                if ctrl && key == Key::KeyR {
                                    shell.editor.reverse_search_cycle_scope();
                                } else if key == Key::Escape || (ctrl && key == Key::KeyC) {
                                    shell.editor.cancel_reverse_search();
                                } else if key == Key::ArrowUp || (ctrl && key == Key::KeyP) {
                                    shell.editor.reverse_search_up();
                                } else if key == Key::ArrowDown || (ctrl && key == Key::KeyN) {
                                    shell.editor.reverse_search_down();
                                } else if key == Key::Tab
                                    || key == Key::ArrowRight
                                    || (ctrl && key == Key::KeyF)
                                {
                                    shell.editor.accept_reverse_search();
                                    popup_engaged = false;
                                    shell.refresh_editor_assist(false);
                                } else if key == Key::Enter || key == Key::NumpadEnter {
                                    if shell.editor.accept_reverse_search() {
                                        let cmd = shell.editor.take_submitted();
                                        popup_engaged = false;
                                        let (cols, rows) = grid_for(size, &fonts, scale);
                                        let cpx = (
                                            fonts.metrics.cell_w as u32,
                                            fonts.metrics.cell_h as u32,
                                        );
                                        shell.submit_command(&cmd, cols, rows, cpx, true);
                                    }
                                } else if alt
                                    && matches!(
                                        key,
                                        Key::Digit1
                                            | Key::Digit2
                                            | Key::Digit3
                                            | Key::Digit4
                                            | Key::Digit5
                                            | Key::Digit6
                                            | Key::Digit7
                                            | Key::Digit8
                                            | Key::Digit9
                                    )
                                {
                                    let offset = match key {
                                        Key::Digit1 => 1,
                                        Key::Digit2 => 2,
                                        Key::Digit3 => 3,
                                        Key::Digit4 => 4,
                                        Key::Digit5 => 5,
                                        Key::Digit6 => 6,
                                        Key::Digit7 => 7,
                                        Key::Digit8 => 8,
                                        Key::Digit9 => 9,
                                        _ => 0,
                                    };
                                    let target = shell
                                        .editor
                                        .reverse_search
                                        .as_ref()
                                        .map(|rs| rs.selected + offset);
                                    if let Some(idx) = target
                                        && let Some(cmd) = shell.editor.take_reverse_search_index(idx)
                                    {
                                        shell.editor.set_text(cmd);
                                        shell.editor.enter_vim_insert();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                } else if key == Key::Backspace {
                                    shell.editor.reverse_search_backspace();
                                } else if !ctrl && !alt && !text.is_empty() {
                                    for ch in text.chars() {
                                        if !ch.is_control() {
                                            shell.editor.reverse_search_push(ch);
                                        }
                                    }
                                }
                            } else if shell.editor.vim_mode() != VimMode::Insert {
                                // `blaze-vim` sans-IO Normal / Visual / VisualLine modal editing
                                // (with shell-aware `i|`/`a|`, `i/`/`a/`, `i,`/`a,` text objects).
                                // Pressing `k` / `Up` on line 0 of the prompt (or `Ctrl+G`) smoothly
                                // enters Scrollback Output Navigation (`h`/`j`/`k`/`l`, `v`/`V`, `y`)!
                                let (cline, _) = shell.editor.cursor_line_col();
                                if ctrl && key == Key::KeyL {
                                    shell.store.clear_active_branch();
                                    shell.scroll_y = 0.0;
                                    output_selection = None;
                                } else if shell.editor.vim_mode() == VimMode::Normal
                                    && cline == 0
                                    && ((key == Key::ArrowUp && !ctrl && !alt)
                                        || (!ctrl && !alt && text == "k")
                                        || (ctrl && key == Key::KeyG))
                                {
                                    let m = fonts.cell_metrics(scale);
                                    let max_y = (shell.last_scene.input_top_y - 2.0).max(TOP_BAR_H);
                                    let vlines = extract_visual_lines(
                                        &shell.last_scene.text_runs,
                                        m,
                                        TOP_BAR_H,
                                        max_y,
                                    );
                                    if !vlines.is_empty() {
                                        output_nav = Some(OutputNav {
                                            line: vlines.len() - 1,
                                            col: 0,
                                            visual: None,
                                            pending_g: false,
                                        });
                                    }
                                } else if ctrl && key == Key::KeyP {
                                    shell.editor.move_up();
                                } else if ctrl && key == Key::KeyN {
                                    shell.editor.move_down();
                                } else if ctrl && key == Key::KeyC {
                                    if shell.editor.vim_mode() == VimMode::Normal {
                                        shell.editor.clear_line();
                                        shell.editor.enter_vim_insert();
                                    } else {
                                        shell.editor.feed_vim_key(VimKeyEvent::key(VimKeyCode::Escape));
                                    }
                                } else if let Some(vev) = to_vim_key(key, &text, ctrl, alt, shift) {
                                    match shell.editor.feed_vim_key(vev) {
                                        VimOutcome::NormalEnter => {
                                            shell.editor.try_expand_abbreviation();
                                            let cmd = shell.editor.take_submitted();
                                            popup_engaged = false;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx, true);
                                        }
                                        VimOutcome::Handled { mutated, yanked } => {
                                            if yanked {
                                                copy_to_pasteboard(shell.editor.vim_register_text());
                                            }
                                            if mutated {
                                                shell.auto_scroll_bottom = true;
                                            }
                                        }
                                        VimOutcome::PassThrough => {}
                                    }
                                }
                            } else if ctrl {
                                match key {
                                    Key::KeyR => {
                                        let cwd = shell.prompt.cwd.clone();
                                        let host = shell.prompt.host.clone();
                                        shell.editor.start_reverse_search_for(&host, &cwd);
                                    }
                                    Key::KeyL => {
                                        shell.store.clear_active_branch();
                                        shell.scroll_y = 0.0;
                                        output_selection = None;
                                        output_nav = None;
                                    }
                                    Key::Space => {
                                        let st = shell.store.active_state().fork();
                                        let cols = shell.recent_table_columns();
                                        shell.editor.trigger_completion_with_cols(&st, &cols);
                                        popup_engaged = !shell.editor.completions.is_empty();
                                    }
                                    Key::KeyP => {
                                        if !shell.editor.completions.is_empty() {
                                            popup_engaged = true;
                                        }
                                        shell.editor.move_up();
                                        if shell.editor.completions.is_empty() {
                                            shell.refresh_editor_assist(false);
                                        }
                                    }
                                    Key::KeyN => {
                                        if !shell.editor.completions.is_empty() {
                                            popup_engaged = true;
                                        }
                                        shell.editor.move_down();
                                        if shell.editor.completions.is_empty() {
                                            shell.refresh_editor_assist(false);
                                        }
                                    }
                                    Key::BracketLeft => {
                                        if !shell.editor.completions.is_empty() {
                                            shell.editor.dismiss_completions();
                                            popup_engaged = false;
                                        } else {
                                            shell.editor.feed_vim_key(VimKeyEvent::key(VimKeyCode::Escape));
                                        }
                                    }
                                    Key::KeyC => {
                                        shell.editor.dismiss_completions();
                                        shell.editor.clear_line();
                                        popup_engaged = false;
                                    }
                                    Key::KeyU => {
                                        shell.editor.clear_line();
                                        popup_engaged = false;
                                    }
                                    Key::KeyK => {
                                        shell.editor.kill_to_end();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::KeyW => {
                                        shell.editor.kill_word_back();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::KeyA => {
                                        shell.editor.move_home(shift);
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::KeyE => {
                                        shell.editor.move_end(shift);
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::KeyF => {
                                        shell.editor.move_right(shift);
                                        shell.editor.dismiss_completions();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::KeyB => {
                                        shell.editor.move_left(shift);
                                        shell.editor.dismiss_completions();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    _ => {}
                                }
                            } else {
                                match key {
                                    Key::Enter | Key::NumpadEnter => {
                                        if popup_engaged && !shell.editor.completions.is_empty() {
                                            shell.editor.apply_selected_completion();
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                        } else if shift || shell.editor.is_incomplete() {
                                            shell.editor.insert_char('\n');
                                            popup_engaged = false;
                                        } else {
                                            // Expand trailing command-position abbreviation (`l`, `gs`, `..`) before running.
                                            shell.editor.try_expand_abbreviation();
                                            let cmd = shell.editor.take_submitted();
                                            popup_engaged = false;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx, true);
                                        }
                                    }
                                    Key::Tab => {
                                        if !shell.editor.completions.is_empty() && !popup_engaged {
                                            shell.editor.apply_selected_completion();
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                        } else if shell.editor.completions.is_empty()
                                            && shell.editor.tab_out()
                                        {
                                            shell.refresh_editor_assist(false);
                                        } else {
                                            let st = shell.store.active_state().fork();
                                            let cols = shell.recent_table_columns();
                                            shell.editor.trigger_completion_with_cols(&st, &cols);
                                            popup_engaged = !shell.editor.completions.is_empty();
                                        }
                                    }
                                    Key::Escape => {
                                        if !shell.editor.completions.is_empty() {
                                            shell.editor.dismiss_completions();
                                            popup_engaged = false;
                                        } else {
                                            shell.editor.feed_vim_key(VimKeyEvent::key(VimKeyCode::Escape));
                                        }
                                    }
                                    Key::Backspace => {
                                        if alt {
                                            shell.editor.kill_word_back();
                                        } else {
                                            shell.editor.backspace();
                                        }
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::Delete => {
                                        if alt {
                                            shell.editor.kill_word_forward();
                                        } else {
                                            shell.editor.delete_forward();
                                        }
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::ArrowLeft => {
                                        if alt {
                                            shell.editor.move_word_left(shift);
                                        } else {
                                            shell.editor.move_left(shift);
                                        }
                                        shell.editor.dismiss_completions();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::ArrowRight => {
                                        if alt {
                                            shell.editor.move_word_right(shift);
                                        } else {
                                            shell.editor.move_right(shift);
                                        }
                                        shell.editor.dismiss_completions();
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::ArrowUp => {
                                        if !shell.editor.completions.is_empty() {
                                            popup_engaged = true;
                                        }
                                        shell.editor.move_up();
                                        if shell.editor.completions.is_empty() {
                                            shell.refresh_editor_assist(false);
                                        }
                                    }
                                    Key::ArrowDown => {
                                        if !shell.editor.completions.is_empty() {
                                            popup_engaged = true;
                                        }
                                        shell.editor.move_down();
                                        if shell.editor.completions.is_empty() {
                                            shell.refresh_editor_assist(false);
                                        }
                                    }
                                    Key::Home => {
                                        shell.editor.move_home(shift);
                                        shell.refresh_editor_assist(false);
                                    }
                                    Key::End => {
                                        shell.editor.move_end(shift);
                                        shell.refresh_editor_assist(false);
                                    }
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
                                            if text == " " {
                                                // Fish/Nushell inline abbreviation expansion on Space!
                                                shell.editor.try_expand_abbreviation();
                                            }
                                            shell.editor.insert_typed(&text);
                                            popup_engaged = false;
                                            shell.auto_scroll_bottom = true;
                                            // Keep Fish ghost autosuggestions live, never auto-pop noisy menu!
                                            shell.refresh_editor_assist(false);
                                        }
                                    }
                                }
                            }
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
                                let step_y = if ev.hasPreciseScrollingDeltas() {
                                    dy as f32
                                } else {
                                    dy as f32 * 24.0
                                };
                                let step_x = if ev.hasPreciseScrollingDeltas() {
                                    ev.scrollingDeltaX() as f32
                                } else {
                                    ev.scrollingDeltaX() as f32 * 24.0
                                };
                                let pt = ev.locationInWindow();
                                let mx = pt.x as f32;
                                let my = (size.height - pt.y) as f32;
                                if let Some(reg) = shell.last_scene.scroll_region_at(mx, my)
                                    && shell.store.scroll_table(
                                        reg.block,
                                        step_y,
                                        step_x,
                                        reg.max_scroll_y,
                                        reg.max_scroll_x,
                                    )
                                {
                                    active_popover = None;
                                    dirty = true;
                                } else {
                                    let max_s = (shell.last_scene.total_content_height
                                        - shell.last_scene.scroll_viewport_height)
                                        .max(0.0);
                                    shell.scroll_y = (shell.scroll_y - step_y).clamp(0.0, max_s);
                                    shell.auto_scroll_bottom = false;
                                    active_popover = None;
                                    dirty = true;
                                }
                            }
                        }
                        NSEventType::LeftMouseDown if window.isKeyWindow() => {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            // Let macOS AppKit handle native window edge/corner resize drags directly!
                            if !on_resize_edge(x, y, size.width as f32, size.height as f32) {
                                // Unified titlebar clicks (branch tabs, fork, zoom) & window drag work
                                // EVERYWHERE — including inside full-screen PTY (`claude`), `y`, and `agent`!
                                if y < TOP_BAR_H {
                                    if let Some(action) = shell.last_scene.hit_test(x, y) {
                                        output_selection = None;
                                        output_nav = None;
                                        match action {
                                            HitAction::SwitchBranch(bid) => {
                                                if shell.store.switch_branch(bid) {
                                                    shell.yazi = None;
                                                    shell.agent_hub = None;
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
                                            HitAction::ResetZoom => {
                                                font_points = FONT_POINTS;
                                                zoom_changed = true;
                                                dirty = true;
                                            }
                                            _ => {}
                                        }
                                        continue;
                                    }
                                    if x > 74.0 {
                                        window.performWindowDragWithEvent(&ev);
                                        continue;
                                    }
                                }
                            }
                            if !on_resize_edge(x, y, size.width as f32, size.height as f32)
                                && !shell.store.is_alternate_screen()
                                && shell.yazi.is_none()
                                && shell.agent_hub.is_none()
                            {
                                output_nav = None;
                                if let Some(action) = shell.last_scene.hit_test(x, y) {
                                    output_selection = None;
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
                                        HitAction::ToggleTableView(block_id) => {
                                            shell.store.toggle_table_view(block_id);
                                            active_popover = None;
                                            dirty = true;
                                        }
                                        HitAction::SortTableColumn { block, col } => {
                                            shell.store.toggle_table_sort(block, col);
                                            active_popover = None;
                                            dirty = true;
                                        }
                                        HitAction::ToggleExpandCell { block, row, col } => {
                                            shell.store.toggle_expand_cell(block, row, col);
                                            dirty = true;
                                        }
                                        HitAction::SelectCompletion(idx) => {
                                            shell.editor.apply_completion(idx);
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                        }
                                        HitAction::SelectHistory(idx) => {
                                            if let Some(cmd) =
                                                shell.editor.take_reverse_search_index(idx)
                                            {
                                                shell.editor.set_text(cmd);
                                                shell.editor.enter_vim_insert();
                                                popup_engaged = false;
                                                shell.refresh_editor_assist(false);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::CycleHistoryScope => {
                                            shell.editor.reverse_search_cycle_scope();
                                            dirty = true;
                                        }
                                        HitAction::ResetZoom => {
                                            font_points = FONT_POINTS;
                                            zoom_changed = true;
                                            dirty = true;
                                        }
                                        HitAction::OpenBranchPicker => {
                                            if let Some(reg) = shell.last_scene.hover_at(x, y) {
                                                let already_pinned = active_popover
                                                    .as_ref()
                                                    .is_some_and(|p| p.pinned && p.anchor == reg.rect);
                                                active_popover = if already_pinned {
                                                    None
                                                } else {
                                                    Some(ActivePopover {
                                                        anchor: reg.rect,
                                                        kind: reg.kind.clone(),
                                                        pinned: true,
                                                    })
                                                };
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SwitchGitBranch(branch) => {
                                            active_popover = None;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            let cmd = format!("git switch {branch}");
                                            shell.submit_command(&cmd, cols, rows, cpx, true);
                                            dirty = true;
                                        }
                                        HitAction::ChangeDir(dir) => {
                                            active_popover = None;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            let cmd = if dir.contains(' ') {
                                                format!("cd \"{dir}\"")
                                            } else {
                                                format!("cd {dir}")
                                            };
                                            shell.submit_command(&cmd, cols, rows, cpx, false);
                                            dirty = true;
                                        }
                                        HitAction::InsertToken(tok) => {
                                            active_popover = None;
                                            shell.editor.enter_vim_insert();
                                            shell.editor.insert_dropped_token(&tok);
                                            popup_engaged = false;
                                            shell.auto_scroll_bottom = true;
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                        }
                                    }
                                    continue;
                                }
                                if active_popover.is_some() {
                                    active_popover = None;
                                    dirty = true;
                                }

                                // Click inside the inline composer: enter Insert mode and position/select cursor.
                                if shell.last_scene.composer_rect.contains(x, y) {
                                    output_selection = None;
                                    shell.editor.enter_vim_insert();
                                    let m = fonts.cell_metrics(scale);
                                    let [ox, oy] = shell.last_scene.composer_text_origin;
                                    let line_step = (m.ch + 2.0).max(1.0);
                                    let line = ((y - oy).max(0.0) / line_step).floor() as usize;
                                    let col = (((x - ox).max(0.0) + m.cw * 0.45) / m.cw).floor() as usize;
                                    let clicks = ev.clickCount();
                                    let shift = ev.modifierFlags().contains(NSEventModifierFlags::Shift);
                                    if clicks >= 3 {
                                        shell.editor.select_all();
                                    } else if clicks == 2 {
                                        shell.editor.select_word_at(line, col);
                                    } else {
                                        shell.editor.set_cursor_at_line_col(line, col, shift);
                                        selecting_composer = true;
                                    }
                                    shell.editor.dismiss_completions();
                                    popup_engaged = false;
                                    dirty = true;
                                    continue;
                                }

                                // Press on any draggable file/folder/crate cell (e.g. `Cargo.lock`): arm drag-to-hotbar.
                                if let Some(payload) = shell.last_scene.drag_source_at(x, y).cloned() {
                                    pending_drag = Some(PendingDrag {
                                        start_x: x,
                                        start_y: y,
                                        x,
                                        y,
                                        active: false,
                                        over_composer: false,
                                        payload,
                                    });
                                    continue;
                                }

                                // Empty inline titlebar drag moves the macOS window natively.
                                if y < TOP_BAR_H && x > 74.0 {
                                    window.performWindowDragWithEvent(&ev);
                                    continue;
                                }

                                // Drag-select (or double/triple click) any rendered block/terminal output text!
                                if y >= TOP_BAR_H {
                                    shell.editor.selection_anchor = None;
                                    let m = fonts.cell_metrics(scale);
                                    let clicks = ev.clickCount();
                                    if clicks >= 2 {
                                        if let Some(sel) = select_at_point(
                                            &shell.last_scene.text_runs,
                                            m,
                                            x,
                                            y,
                                            clicks >= 3,
                                            TOP_BAR_H,
                                        ) {
                                            output_selection = Some(TextSelection {
                                                anchor: [sel.anchor[0], sel.anchor[1] + shell.scroll_y],
                                                head: [sel.head[0], sel.head[1] + shell.scroll_y],
                                            });
                                        } else {
                                            output_selection = None;
                                        }
                                        selecting_output = false;
                                    } else {
                                        output_selection = Some(TextSelection {
                                            anchor: [x, y + shell.scroll_y],
                                            head: [x, y + shell.scroll_y],
                                        });
                                        selecting_output = true;
                                    }
                                    dirty = true;
                                    continue;
                                }
                            }
                        }
                        NSEventType::MouseMoved if window.isKeyWindow() => {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            last_mouse = Some((x, y));
                            let in_traffic_lights = y >= 0.0 && y < TOP_BAR_H && x >= 0.0 && x < 76.0;
                            let near_resize_edge = on_resize_edge(x, y, size.width as f32, size.height as f32);
                            let in_window = x >= 0.0 && y >= 0.0 && x <= size.width as f32 && y <= size.height as f32;
                            if in_window && !in_traffic_lights && !near_resize_edge {
                                // Smooth native hover popover tracking:
                                // - For interactive prompt menus (branches/dir/changes), keep alive while
                                //   pointer travels from anchor into the menu.
                                // - For read-only RHS file previews, ONLY keep alive while still over the
                                //   hovered file row so gliding up & down switches previews instantaneously!
                                let in_open_popover = active_popover.as_ref().is_some_and(|p| {
                                    if p.anchor.contains(x, y) {
                                        return true;
                                    }
                                    if matches!(p.kind, HoverKind::FilePreview { .. } | HoverKind::FileEntry { .. }) {
                                        return false;
                                    }
                                    if let Some(pr) = shell.last_scene.popover_rect {
                                        if pr.contains(x, y) {
                                            return true;
                                        }
                                        let bx0 = p.anchor.x0.min(pr.x0) - 4.0;
                                        let bx1 = p.anchor.x1.max(pr.x1) + 4.0;
                                        let by0 = p.anchor.y0.min(pr.y0);
                                        let by1 = p.anchor.y1.max(pr.y1);
                                        if HitRect::new(bx0, by0, bx1, by1).contains(x, y) {
                                            return true;
                                        }
                                    }
                                    false
                                });
                                if in_open_popover {
                                    dirty = true;
                                } else if !shell.store.is_alternate_screen()
                                    && shell.yazi.is_none()
                                    && shell.agent_hub.is_none()
                                    && shell.editor.reverse_search.is_none()
                                    && shell.editor.completions.is_empty()
                                    && !selecting_composer
                                    && !selecting_output
                                    && pending_drag.is_none()
                                    && let Some(reg) = shell.last_scene.hover_at(x, y)
                                {
                                    if active_popover.as_ref().is_none_or(|cur| !cur.pinned && cur.anchor != reg.rect) {
                                        let hydrated_kind = match &reg.kind {
                                            HoverKind::FileEntry { full_path, name, icon, .. } => {
                                                let (meta, lines, truncated) = load_file_preview_lines(
                                                    std::path::Path::new(full_path),
                                                    16,
                                                );
                                                HoverKind::FilePreview {
                                                    full_path: full_path.clone(),
                                                    name: name.clone(),
                                                    icon: *icon,
                                                    meta,
                                                    lines,
                                                    truncated,
                                                }
                                            }
                                            other => other.clone(),
                                        };
                                        active_popover = Some(ActivePopover {
                                            anchor: reg.rect,
                                            kind: hydrated_kind,
                                            pinned: false,
                                        });
                                        dirty = true;
                                    }
                                } else if active_popover.as_ref().is_some_and(|p| !p.pinned) {
                                    active_popover = None;
                                    dirty = true;
                                }

                                let next = cursor_for_point(
                                    &shell.last_scene,
                                    shell.store.is_alternate_screen(),
                                    pending_drag.as_ref().is_some_and(|d| d.active),
                                    selecting_composer || selecting_output,
                                    x,
                                    y,
                                    size.width as f32,
                                    size.height as f32,
                                );
                                apply_cursor(&mut cur_cursor, next);
                                continue;
                            }
                            if active_popover.as_ref().is_some_and(|p| !p.pinned) {
                                active_popover = None;
                                dirty = true;
                            }
                            // Only reset to Arrow when strictly outside resize edges (otherwise let AppKit show resize arrows!).
                            if !near_resize_edge {
                                apply_cursor(&mut cur_cursor, CursorKind::Arrow);
                            }
                        }
                        NSEventType::LeftMouseDragged if window.isKeyWindow() => {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            last_mouse = Some((x, y));
                            if let Some(d) = &mut pending_drag {
                                d.x = x;
                                d.y = y;
                                if !d.active && (x - d.start_x).hypot(y - d.start_y) >= 4.0 {
                                    d.active = true;
                                }
                                if d.active {
                                    let cr = shell.last_scene.composer_rect;
                                    d.over_composer = cr.contains(x, y)
                                        || (y >= cr.y0 && y <= size.height as f32 && x >= cr.x0 && x <= cr.x1);
                                    apply_cursor(&mut cur_cursor, CursorKind::ClosedHand);
                                    dirty = true;
                                }
                                continue;
                            }
                            if selecting_composer {
                                let m = fonts.cell_metrics(scale);
                                let [ox, oy] = shell.last_scene.composer_text_origin;
                                let line_step = (m.ch + 2.0).max(1.0);
                                let line = ((y - oy).max(0.0) / line_step).floor() as usize;
                                let col = (((x - ox).max(0.0) + m.cw * 0.45) / m.cw).floor() as usize;
                                shell.editor.set_cursor_at_line_col(line, col, true);
                                apply_cursor(&mut cur_cursor, CursorKind::IBeam);
                                dirty = true;
                                continue;
                            }
                            if selecting_output {
                                if let Some(sel) = &mut output_selection {
                                    sel.head = [x, y + shell.scroll_y];
                                }
                                apply_cursor(&mut cur_cursor, CursorKind::IBeam);
                                dirty = true;
                                continue;
                            }
                        }
                        NSEventType::LeftMouseUp => {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            last_mouse = Some((x, y));
                            if let Some(d) = pending_drag.take() {
                                let cr = shell.last_scene.composer_rect;
                                let dropped_in_prompt = cr.contains(x, y)
                                    || (y >= cr.y0 && y <= size.height as f32 && x >= cr.x0 && x <= cr.x1);
                                if d.active && dropped_in_prompt {
                                    shell.editor.enter_vim_insert();
                                    shell.editor.insert_dropped_token(&d.payload.insert_text);
                                    popup_engaged = false;
                                    shell.auto_scroll_bottom = true;
                                    shell.refresh_editor_assist(false);
                                }
                                let next = cursor_for_point(
                                    &shell.last_scene,
                                    shell.store.is_alternate_screen(),
                                    false,
                                    false,
                                    x,
                                    y,
                                    size.width as f32,
                                    size.height as f32,
                                );
                                apply_cursor(&mut cur_cursor, next);
                                dirty = true;
                                continue;
                            }
                            if selecting_composer {
                                selecting_composer = false;
                                let next = cursor_for_point(
                                    &shell.last_scene,
                                    shell.store.is_alternate_screen(),
                                    false,
                                    false,
                                    x,
                                    y,
                                    size.width as f32,
                                    size.height as f32,
                                );
                                apply_cursor(&mut cur_cursor, next);
                                continue;
                            }
                            if selecting_output {
                                selecting_output = false;
                                if let Some(sel) = output_selection
                                    && (sel.head[0] - sel.anchor[0]).hypot(sel.head[1] - sel.anchor[1]) < 3.0
                                {
                                    output_selection = None;
                                    dirty = true;
                                }
                                let next = cursor_for_point(
                                    &shell.last_scene,
                                    shell.store.is_alternate_screen(),
                                    false,
                                    false,
                                    x,
                                    y,
                                    size.width as f32,
                                    size.height as f32,
                                );
                                apply_cursor(&mut cur_cursor, next);
                                continue;
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

            // Poll asynchronous `blaze-git` Starship status worker (single atomic check when idle).
            if let Some((cwd, summary)) = shell.git_worker.poll()
                && cwd == shell.prompt.cwd
            {
                if let Some(st) = summary {
                    shell.prompt.git_branch = Some(st.branch.clone());
                    shell.prompt.git_detached = st.detached;
                    shell.prompt.git_status = Some(st);
                } else {
                    shell.prompt.git_status = None;
                }
                dirty = true;
            }

            // Poll non-blocking background command jobs (`rust { ... }`, external commands).
            {
                let (cols, rows) = grid_for(size, &fonts, scale);
                let cpx = (fonts.metrics.cell_w as u32, fonts.metrics.cell_h as u32);
                if shell.poll_async_jobs(cols, rows, cpx) {
                    dirty = true;
                }
            }
            if shell.running_jobs > 0 && last_running_tick.elapsed().as_millis() >= 80 {
                last_running_tick = Instant::now();
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
                // Drain any remaining unread bytes from the PTY master before closing/dropping it
                // so fast commands never lose their final output!
                if let Some(ap) = shell.active_pty.take() {
                    let drain_deadline = Instant::now() + std::time::Duration::from_millis(15);
                    loop {
                        match ap.pty.read(&mut pty_buf) {
                            PtyRead::Bytes(n) => {
                                if let Some(blk) = shell.store.block_mut(ap.block_id)
                                    && let BlockBody::Terminal { term, .. } = &mut blk.body
                                {
                                    term.feed(&pty_buf[..n]);
                                }
                            }
                            PtyRead::Closed => break,
                            PtyRead::Nothing => {
                                if Instant::now() >= drain_deadline {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                        }
                    }
                }
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

            // Handle window resize, backing scale changes, and `Cmd+`/`Cmd-`/`Cmd+0` zoom.
            let new_size = view.bounds().size;
            let new_scale = window.backingScaleFactor().max(1.0);
            if new_size != size || (new_scale - scale).abs() > f64::EPSILON || zoom_changed {
                if (new_scale - scale).abs() > f64::EPSILON || zoom_changed {
                    scale = new_scale;
                    fonts = Fonts::new(font_points, scale);
                    icon_atlas = IconAtlas::new(scale * (font_points / FONT_POINTS));
                    layer.setContentsScale(scale);
                    zoom_changed = false;
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

            // Ghostty-style eased cursor sweep trail (`DURATION = 0.20s`, `EaseOutCubic`).
            let mut active_trail: Option<CursorTrail> = None;
            if let (Some(prev), Some(t0)) = (trail_prev, trail_start) {
                let p = t0.elapsed().as_secs_f32() / CURSOR_TRAIL_SECS;
                if p < 1.0 {
                    active_trail = Some(CursorTrail { prev_rect: prev, progress: p });
                    dirty = true;
                } else {
                    trail_start = None;
                    trail_prev = None;
                    dirty = true;
                }
            }

            if dirty {
                let metrics = fonts.cell_metrics(scale);
                // Steady non-blinking cursor (matches Ghostty `cursor-style-blink = false`).
                let cursor_vis = window.isKeyWindow();
                let zoom_pct = ((font_points / FONT_POINTS) * 100.0).round() as u16;
                let active_drag = pending_drag.as_ref().and_then(|d| {
                    if d.active {
                        Some(ActiveDrag {
                            x: d.x,
                            y: d.y,
                            over_composer: d.over_composer,
                            payload: d.payload.clone(),
                        })
                    } else {
                        None
                    }
                });
                let mut scene = if let Some(yazi) = &mut shell.yazi {
                    layout_yazi_view(
                        yazi,
                        &shell.store,
                        &shell.prompt,
                        size.width as f32,
                        size.height as f32,
                        metrics,
                        zoom_pct,
                        true,
                    )
                } else if let Some(hub) = &shell.agent_hub {
                    layout_agent_hub(
                        hub,
                        &shell.store,
                        &shell.prompt,
                        size.width as f32,
                        size.height as f32,
                        metrics,
                        zoom_pct,
                        true,
                    )
                } else if shell.store.is_alternate_screen()
                    && let Some(tid) = shell.store.active_running_terminal()
                    && let Some(blk) = shell.store.block(tid)
                    && let BlockBody::Terminal { term, .. } = &blk.body
                {
                    layout_fullscreen_terminal(
                        term,
                        &shell.store,
                        &shell.prompt,
                        size.width as f32,
                        size.height as f32,
                        metrics,
                        cursor_vis,
                        zoom_pct,
                        true,
                    )
                } else {
                    let frame_now = now_ns();
                    let screen_sel = |sy: f32| {
                        output_selection.map(|s| TextSelection {
                            anchor: [s.anchor[0], s.anchor[1] - sy],
                            head: [s.head[0], s.head[1] - sy],
                        })
                    };
                    let mouse_pt = last_mouse.map(|(mx, my)| [mx, my]);
                    let first = build_scene(&LayoutInput {
                        store: &shell.store,
                        editor: &shell.editor,
                        prompt: &shell.prompt,
                        viewport_w: size.width as f32,
                        viewport_h: size.height as f32,
                        metrics,
                        scroll_y: shell.scroll_y,
                        cursor_visible: cursor_vis,
                        now_ns: frame_now,
                        zoom_pct,
                        inline_titlebar: true,
                        drag: active_drag.as_ref(),
                        cursor_trail: active_trail,
                        text_selection: screen_sel(shell.scroll_y),
                        active_popover: active_popover.as_ref(),
                        mouse_pos: mouse_pt,
                        output_nav,
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
                            now_ns: frame_now,
                            zoom_pct,
                            inline_titlebar: true,
                            drag: active_drag.as_ref(),
                            cursor_trail: active_trail,
                            text_selection: screen_sel(shell.scroll_y),
                            active_popover: active_popover.as_ref(),
                            mouse_pos: mouse_pt,
                            output_nav,
                        })
                    } else {
                        first
                    }
                };

                // Detect significant cursor jump to trigger Ghostty eased sweep trail!
                if let (Some(prev), Some(curr)) = (last_cursor_rect, scene.cursor_rect) {
                    let dx = (curr[0] + curr[2] * 0.5) - (prev[0] + prev[2] * 0.5);
                    let dy = (curr[1] + curr[3] * 0.5) - (prev[1] + prev[3] * 0.5);
                    if dx.hypot(dy) > 10.0 {
                        trail_prev = Some(prev);
                        trail_start = Some(Instant::now());
                    }
                }
                last_cursor_rect = scene.cursor_rect;

                let glyphs = fonts.rasterize_runs(&scene.text_runs, scale);
                let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale);
                for (slot, bytes) in fonts.pending.drain(..) {
                    renderer.upload(slot, &bytes);
                }
                let icons = icon_atlas.rasterize_quads(&scene.icon_quads);
                let overlay_icons = icon_atlas.rasterize_quads(&scene.overlay_icons);
                for (slot, rgba) in icon_atlas.pending.drain(..) {
                    renderer.upload_icon(slot, &rgba);
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
                    icons: &icons,
                    over_rects: &scene.over_rects,
                    overlay_rects: &scene.overlay_rects,
                    overlay_icons: &overlay_icons,
                    overlay_glyphs: &overlay_glyphs,
                };
                renderer.draw(&layer, &frame);
                // Keep `text_runs`, hit regions, drag sources & composer geometry in `last_scene`
                // for instant mouse hover, output text selection, and `Cmd+C` extraction!
                scene.rects.clear();
                scene.icon_quads.clear();
                scene.over_rects.clear();
                scene.overlay_rects.clear();
                scene.overlay_icons.clear();
                scene.overlay_text.clear();
                shell.last_scene = scene;
                if let Some((mx, my)) = last_mouse
                    && window.isKeyWindow()
                    && !on_resize_edge(mx, my, size.width as f32, size.height as f32)
                {
                    let next = cursor_for_point(
                        &shell.last_scene,
                        shell.store.is_alternate_screen(),
                        pending_drag.as_ref().is_some_and(|d| d.active),
                        selecting_composer || selecting_output,
                        mx,
                        my,
                        size.width as f32,
                        size.height as f32,
                    );
                    apply_cursor(&mut cur_cursor, next);
                }
                dirty = trail_start.is_some();
            }
        }
    }
}
