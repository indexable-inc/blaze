//! `blaze-shell`: native macOS GUI shell rendered by Metal + `blaze-ui`'s Rust-authored SDF
//! & glyph shaders + CoreText, driven by `blaze-sh`'s sans-IO structured pipeline engine,
//! O(1) CoW session forks, `blaze-vt` PTY terminal blocks, and live Rust cells via `blaze run`.

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("api") {
        std::process::exit(blaze_shell::term_server::run_cli_api(&args[2..]));
    }
    macos_app::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("api") {
        std::process::exit(blaze_shell::term_server::run_cli_api(&args[2..]));
    }
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
    use std::cell::RefCell;
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
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSBackingStoreType,
        NSCursor, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSMenu, NSMenuItem,
        NSPasteboard, NSPasteboardTypeString, NSResponder, NSView, NSWindow, NSWindowStyleMask,
        NSWindowTitleVisibility,
    };
    use objc2_core_graphics::CGColor;
    use objc2_foundation::{
        NSDate, NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    };
    use objc2_metal::MTLCreateSystemDefaultDevice;
    use objc2_quartz_core::{CAMetalLayer, CATransaction, kCAGravityTopLeft};

    use blaze_shell::agent_hub::{
        AgentHub, AgentHubAction, AgentRecord, AgentStatus, layout_agent_hub_pane,
    };
    use blaze_shell::blocks::{BlockBody, BlockId, BlockStore};
    use blaze_shell::editor::{CommandPalette, Editor, HistoryEntry, PaletteAction};
    use blaze_shell::font::Fonts;
    use blaze_shell::host::{
        InteractiveSpawn, RustCellMeta, run_command_line, run_command_line_with_pid,
    };
    use blaze_shell::image_atlas::ImageAtlas;
    use blaze_shell::keys;
    use blaze_shell::layout::{
        LayoutInput, SIDEBAR_W, TILE_GAP, TOP_BAR_H, TabBarItem, build_pane_scene,
        collect_branch_tree, draw_split_dividers, layout_command_palette,
        layout_fullscreen_terminal_pane, layout_window_top_bar, layout_workspace_sidebar,
    };
    use blaze_shell::prompt::{GitStatusWorker, PromptInfo, try_native_git_command};
    use blaze_shell::pty::{Pty, Read as PtyRead};
    use blaze_shell::render::{FrameDraw, PIXEL_FORMAT, Renderer};
    use blaze_shell::rust_cell;
    use blaze_shell::svg_icons::IconAtlas;
    use blaze_shell::theme;
    use blaze_shell::tiling::{
        CloseOutcome, FocusDir, PaneId, SplitAxis, SplitHandle, TileLayout, TileManager,
    };
    use blaze_shell::view::{
        ActiveDrag, ActivePopover, CursorTrail, DragPayload, HitAction, HitRect, HoverKind,
        OutputNav, OutputVisualMode, Scene, TextFace, TextSelection, compute_text_selection,
        extract_visual_lines, output_nav_geometry, select_at_point,
    };
    use blaze_shell::yazi::{YaziAction, YaziState, layout_yazi_pane, load_hover_preview};

    const FONT_POINTS: f64 = 13.0;
    const MIN_FONT_POINTS: f64 = 8.5;
    const MAX_FONT_POINTS: f64 = 28.0;
    const ZOOM_STEP_POINTS: f64 = 1.5;
    const CURSOR_TRAIL_SECS: f32 = 0.20;
    /// macOS `FullSizeContentView` native window resize border zone (px).
    /// Matches AppKit's actual ~4pt inner edge band (avoiding a dead inset strip) with a slightly
    /// wider corner zone so diagonal corner grabs always reach `NSThemeFrame`.
    const RESIZE_EDGE: f32 = 4.5;
    const RESIZE_CORNER: f32 = 9.0;

    #[inline]
    fn on_resize_edge(x: f32, y: f32, w: f32, h: f32) -> bool {
        if x <= RESIZE_EDGE || x >= w - RESIZE_EDGE || y <= RESIZE_EDGE || y >= h - RESIZE_EDGE {
            return true;
        }
        let corner_x = x <= RESIZE_CORNER || x >= w - RESIZE_CORNER;
        let corner_y = y <= RESIZE_CORNER || y >= h - RESIZE_CORNER;
        corner_x && corner_y
    }

    struct CliOpts {
        demo: bool,
        demo_popup: bool,
        demo_history: bool,
        demo_tiled: bool,
        scroll_bottom: bool,
        scroll_y: Option<f32>,
        cwd: Option<String>,
        cmd: Option<String>,
        screenshot: Option<PathBuf>,
    }

    fn parse_cli() -> CliOpts {
        let mut demo = false;
        let mut demo_popup = false;
        let mut demo_history = false;
        let mut demo_tiled = false;
        let mut scroll_bottom = false;
        let mut scroll_y = None;
        let mut cwd = None;
        let mut cmd = None;
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
                "--demo-tiled" => {
                    demo = true;
                    demo_tiled = true;
                }
                "--scroll-bottom" => scroll_bottom = true,
                "--scroll" => {
                    if let Some(v) = args.next() {
                        scroll_y = v.parse::<f32>().ok();
                    }
                }
                "--cwd" => {
                    if let Some(d) = args.next() {
                        cwd = Some(d);
                    }
                }
                "--cmd" => {
                    if let Some(c) = args.next() {
                        cmd = Some(c);
                    }
                }
                "--screenshot" => {
                    if let Some(p) = args.next() {
                        screenshot = Some(PathBuf::from(p));
                    }
                }
                _ => {}
            }
        }
        CliOpts {
            demo,
            demo_popup,
            demo_history,
            demo_tiled,
            scroll_bottom,
            scroll_y,
            cwd,
            cmd,
            screenshot,
        }
    }

    /// Spawn a fresh native macOS `blaze-shell` window inheriting `cwd` (`Cmd+N`).
    fn spawn_new_window(cwd: &str) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe)
                .arg("--cwd")
                .arg(cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
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
        ClosedHand,
        ResizeEW,
        ResizeNS,
    }

    fn apply_cursor(cur: &mut Option<CursorKind>, next: CursorKind) {
        if *cur == Some(next) {
            return;
        }
        *cur = Some(next);
        #[allow(deprecated)]
        let cursor = match next {
            CursorKind::Arrow => NSCursor::arrowCursor(),
            CursorKind::Pointer => NSCursor::pointingHandCursor(),
            CursorKind::IBeam => NSCursor::IBeamCursor(),
            CursorKind::ClosedHand => NSCursor::closedHandCursor(),
            CursorKind::ResizeEW => NSCursor::resizeLeftRightCursor(),
            CursorKind::ResizeNS => NSCursor::resizeUpDownCursor(),
        };
        cursor.set();
    }

    #[allow(clippy::too_many_arguments)]
    fn cursor_for_point(
        scene: &Scene,
        handles: &[SplitHandle],
        dragging_split: Option<SplitHandle>,
        alt_screen: bool,
        dragging: bool,
        selecting_text: bool,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> CursorKind {
        if let Some(sh) = dragging_split {
            return match sh.axis {
                SplitAxis::Vertical => CursorKind::ResizeEW,
                SplitAxis::Horizontal => CursorKind::ResizeNS,
            };
        }
        if x < 0.0 || y < 0.0 || x > w || y > h {
            return CursorKind::Arrow;
        }
        if dragging {
            return CursorKind::ClosedHand;
        }
        if let Some(sh) = handles.iter().find(|sh| sh.hit_rect.contains(x, y)) {
            return match sh.axis {
                SplitAxis::Vertical => CursorKind::ResizeEW,
                SplitAxis::Horizontal => CursorKind::ResizeNS,
            };
        }
        if alt_screen || selecting_text {
            return CursorKind::IBeam;
        }
        if let Some(act) = scene.hit_test(x, y) {
            return match act {
                HitAction::ResizeAgentSidebar | HitAction::ResizeTableColumn { .. } => {
                    CursorKind::ResizeEW
                }
                HitAction::FocusInlinePty(_) => CursorKind::IBeam,
                _ => CursorKind::Pointer,
            };
        }
        if scene.composer_rect.contains(x, y) {
            return CursorKind::IBeam;
        }
        if scene.drag_source_at(x, y).is_some() {
            return CursorKind::Pointer;
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
        Output {
            exit_code: i32,
            duration: Nanos,
            chunks: Vec<(i64, blaze_sh::OutputStream, Vec<u8>)>,
        },
        List(Vec<SendValue>),
        Record(Vec<(String, SendValue)>),
        Table {
            columns: Vec<String>,
            displays: Vec<ColDisplay>,
            default_grid: bool,
            hidden_mask: u64,
            initial_sort: Option<(usize, bool)>,
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
                Value::Output(o) => Self::Output {
                    exit_code: o.exit_code,
                    duration: o.duration,
                    chunks: o
                        .chunks
                        .iter()
                        .map(|ch| (ch.at.0, ch.stream, ch.bytes.as_ref().to_vec()))
                        .collect(),
                },
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
                    Self::Table {
                        columns,
                        displays,
                        default_grid: t.default_grid,
                        hidden_mask: t.hidden_mask,
                        initial_sort: t.initial_sort,
                        rows,
                    }
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
                Self::Output { exit_code, duration, chunks } => {
                    let mut pvec = blaze_sh::PVec::new(alloc);
                    for (at_ns, stream, bytes) in chunks {
                        pvec.push(blaze_sh::OutputChunk {
                            at: Nanos(at_ns),
                            stream,
                            bytes: blaze_own::Rc::from_slice_in(&bytes, alloc),
                        });
                    }
                    Value::Output(blaze_own::Rc::new_in(
                        blaze_sh::CommandOutput { exit_code, duration, chunks: pvec },
                        alloc,
                    ))
                }
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
                Self::Table { columns, displays, default_grid, hidden_mask, initial_sort, rows } => {
                    let col_refs: Vec<&str> = columns.iter().map(String::as_str).collect();
                    let mut t = Table::from_col_names(&col_refs, alloc);
                    t.default_grid = default_grid;
                    t.hidden_mask = hidden_mask;
                    t.initial_sort = initial_sort;
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
        duration_ns: u64,
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
            || first == "sleep"
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

    /// Collect all live child PTY and background-job PIDs across every pane & tab so the
    /// folder-process monitor knows which processes belong to which `blaze-shell` pane/TUI.
    fn collect_shell_pane_pids(
        wm: &TileManager<'_>,
        panes: &std::collections::HashMap<PaneId, ShellApp>,
    ) -> Vec<blaze_sysmon::ShellPanePid> {
        let mut out = Vec::new();
        let mut pane_num = 0u32;
        for tab in wm.tabs() {
            let mut leaves = blaze_own::Vec::new_in(blaze_own::Alloc::global());
            tab.root.collect_leaves(&mut leaves);
            for pid in leaves {
                pane_num += 1;
                let Some(pane) = panes.get(&pid) else {
                    continue;
                };
                if let Some(ap) = &pane.active_pty {
                    let is_alt = pane
                        .store
                        .block(ap.block_id)
                        .is_some_and(|b| {
                            matches!(
                                &b.body,
                                BlockBody::Terminal { term, running: true, .. }
                                    if matches!(term.active(), blaze_vt::terminal::Which::Alternate)
                            )
                        });
                    out.push(blaze_sysmon::ShellPanePid {
                        pid: ap.pty.pid(),
                        pane_id: u64::from(pid.0),
                        block_id: ap.block_id.0,
                        pane_number: pane_num,
                        is_alt_screen: is_alt,
                    });
                }
                for blk in pane.store.active_blocks() {
                    if let BlockBody::Running { pid: os_pid, .. } = &blk.body {
                        let p = os_pid.load(std::sync::atomic::Ordering::Relaxed);
                        if p > 0 {
                            out.push(blaze_sysmon::ShellPanePid {
                                pid: p,
                                pane_id: u64::from(pid.0),
                                block_id: blk.id.0,
                                pane_number: pane_num,
                                is_alt_screen: false,
                            });
                        }
                    }
                }
            }
        }
        out
    }

    /// Hydrate a hovered `CommandToken` into a rich `HoverKind::CommandExplain` popover using
    /// `blaze-cli-spec` (`man` page + `--help` + fish + curated specs) and `blaze-sh` builtins.
    fn hydrate_command_explain(
        source: &str,
        token_start: usize,
        token_end: usize,
        path_env: &str,
        home: Option<&str>,
    ) -> Option<HoverKind> {
        let alloc = Alloc::global();
        let tokens = blaze_cli_spec::explain::tokenize_command_spans(source, alloc);
        let tok = tokens
            .iter()
            .find(|t| t.start == token_start && t.end == token_end)?;
        let cmd = tok.cmd_base.as_str();
        let sub = tok.subcmd.as_ref().map(|s| s.as_str());
        let spec = if !cmd.is_empty() {
            let sub_for_spec = match tok.role {
                blaze_cli_spec::explain::TokenRole::Command
                | blaze_cli_spec::explain::TokenRole::Subcommand => None,
                _ => sub,
            };
            blaze_shell::cli_spec_driver::lookup_nonblocking(cmd, sub_for_spec, path_env, home)
                .or_else(|| {
                    if sub_for_spec.is_some() {
                        blaze_shell::cli_spec_driver::lookup_nonblocking(cmd, None, path_env, home)
                    } else {
                        None
                    }
                })
        } else {
            None
        };
        let man_summary = match tok.role {
            blaze_cli_spec::explain::TokenRole::Command if !cmd.is_empty() => {
                blaze_shell::cli_spec_driver::lookup_man_summary_nonblocking(cmd, None)
            }
            blaze_cli_spec::explain::TokenRole::Subcommand if !cmd.is_empty() => {
                blaze_shell::cli_spec_driver::lookup_man_summary_nonblocking(cmd, Some(tok.text.as_str()))
            }
            _ => None,
        };
        let builtin_summary = match tok.role {
            blaze_cli_spec::explain::TokenRole::Command => {
                if matches!(cmd, "top" | "btop" | "htop" | "sysmon") {
                    Some("Native Metal Activity & Apple Silicon GPU Monitor (process tree + app icons)")
                } else {
                    blaze_sh::BUILTIN_SPECS
                        .iter()
                        .find(|b| b.name == cmd)
                        .map(|b| b.summary)
                }
            }
            _ => None,
        };
        let exp = blaze_cli_spec::explain::explain_token_in(
            tok,
            spec.as_deref(),
            man_summary.as_deref(),
            builtin_summary,
            alloc,
        )?;
        Some(HoverKind::CommandExplain {
            source: source.to_owned(),
            token_start,
            token_end,
            badge: exp.badge,
            title: exp.title.as_str().to_owned(),
            summary: exp.summary.as_str().to_owned(),
            details: exp
                .details
                .iter()
                .map(|d| (d.tag.as_str().to_owned(), d.text.as_str().to_owned()))
                .collect(),
            source_note: exp.source_note.as_str().to_owned(),
        })
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
        scroll_to_block: Option<BlockId>,
        active_pty: Option<ActivePty>,
        /// Alternate-screen TUIs (`claude`, `vim`, `htop`) render inline by default so scrollback
        /// remains visible; toggle true fullscreen anytime with `Cmd+Shift+F` (or block `⤢` pill).
        pty_fullscreen: bool,
        /// When an inline PTY is running (`!pty_fullscreen`), whether keyboard & scroll focus is
        /// captured by the inline PTY (`true`) or returned to the shell composer (`false`).
        pty_focused: bool,
        /// Optional row-count override for the inline PTY box, adjusted with `Cmd+Opt+`/`-`/`0`
        /// while focused inside the inline alternate-screen terminal.
        pty_inline_rows_override: Option<u16>,
        active_sysmon: Option<BlockId>,
        sysmon_fullscreen: bool,
        last_sysmon_tick: Instant,
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
        fn new_in(initial_cwd: Option<&str>) -> Self {
            let alloc = Alloc::global();
            let cwd = initial_cwd
                .filter(|p| std::path::Path::new(p).is_dir())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "/".to_owned())
                });
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
            let _ = blaze_shell::config::ensure_default_config(&home);
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
                scroll_to_block: None,
                active_pty: None,
                pty_fullscreen: false,
                pty_focused: false,
                pty_inline_rows_override: None,
                active_sysmon: None,
                sysmon_fullscreen: false,
                last_sysmon_tick: Instant::now(),
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

        fn cwd(&self) -> &str {
            self.store.active_state().cwd.as_ref()
        }

        fn pty_accepts_input(&self) -> bool {
            self.active_pty.is_some() && (self.pty_fullscreen || self.pty_focused)
        }

        fn send_pty_focus_event(&mut self, focused: bool) {
            if let Some(ap) = &self.active_pty
                && let Some(blk) = self.store.block_mut(ap.block_id)
                && let BlockBody::Terminal { term, running: true, .. } = &mut blk.body
            {
                term.set_focused(focused);
                let seq = blaze_vt::input::focus::encode(
                    if focused {
                        blaze_vt::input::focus::FocusEvent::Gained
                    } else {
                        blaze_vt::input::focus::FocusEvent::Lost
                    },
                    blaze_vt::input::focus::FocusOpts {
                        reporting: term.mode(blaze_vt::terminal::Mode::FocusEvent),
                    },
                );
                if !seq.is_empty() {
                    ap.pty.write_all(seq);
                }
            }
        }

        fn set_pty_focus(&mut self, focused: bool) -> bool {
            if self.pty_focused == focused {
                return false;
            }
            self.pty_focused = focused;
            self.send_pty_focus_event(focused);
            true
        }

        fn process_tag(&self) -> Option<&'static str> {
            if self.yazi.is_some() {
                Some("y")
            } else if self.agent_hub.is_some() {
                Some("agent")
            } else if self.active_pty.is_some() {
                Some("tty")
            } else {
                None
            }
        }

        /// Bind `$_`, `$last`, and `$_<seq>` in `state` so any subsequent command can pipe or
        /// inspect previous outputs with zero copying (O(1) CoW persistent values!).
        /// For `__blaze_schema` meta-cards (e.g. `open Cargo.lock | schema` or `schema $lock`),
        /// binds `$_` and `$last` to the inspected subject value (`__blaze_schema_subject`) so
        /// immediately clicking or typing `.package.source` queries the inspected data!
        fn bind_output_refs(&mut self, state: &mut EngineState<'static>, val: &Value<'static>) {
            if matches!(val, Value::Nothing | Value::Error(_)) {
                return;
            }
            self.output_seq += 1;
            let effective_subject = match val {
                Value::Record(r) if blaze_shell::schema_view::is_schema_record(r) => {
                    r.get(blaze_sh::schema::SCHEMA_SUBJECT_KEY).cloned()
                }
                _ => Some(val.clone()),
            };
            if let Some(subj) = effective_subject {
                state.set_var("_", subj.clone(), false);
                state.set_var("last", subj, false);
            }
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

            // 2b. Native Metal Activity & GPU Monitor (`top` / `btop` / `htop` / `sysmon`).
            // Use `^top` to bypass and run external `/usr/bin/top` on a PTY if ever needed.
            let first_tok = trimmed.split_whitespace().next().unwrap_or("");
            if first_tok == "term"
                && !trimmed.contains('|')
                && !trimmed.contains(';')
                && !trimmed.contains("&&")
            {
                let t0 = Instant::now();
                let state_before = self.store.active_state().fork();
                let state_after = state_before.fork();
                let prompt_snap = self.prompt.clone();
                let tokens = blaze_shell::term_server::split_shell_tokens(trimmed);
                let srv = blaze_shell::term_server::start_global(&self.home);
                let res = blaze_shell::term_server::exec_term_command(
                    &srv,
                    &self.prompt.cwd,
                    tokens.get(1..).unwrap_or(&[]),
                );
                let dur_ms = t0.elapsed().as_millis() as u64;
                let dur_ns = t0.elapsed().as_nanos() as u64;
                let (exit_code, body) = match res {
                    Ok(val) => (0, BlockBody::Value(val)),
                    Err(msg) => (
                        1,
                        BlockBody::Error {
                            err: blaze_sh::ShellError::new(
                                blaze_sh::ErrorKind::External,
                                blaze_sh::Span { start: 0, end: trimmed.len() as u32 },
                                &msg,
                                Alloc::global(),
                            ),
                        },
                    ),
                };
                let id = self.store.record_block(
                    trimmed.to_owned(),
                    prompt_snap,
                    state_before,
                    state_after,
                    0,
                    dur_ms,
                    exit_code,
                    body,
                );
                if let Some(blk) = self.store.block_mut(id) {
                    blk.duration_ns = dur_ns;
                }
                self.refresh_prompt(exit_code, dur_ms);
                self.auto_scroll_bottom = true;
                self.scroll_to_block = None;
                return;
            }
            if matches!(first_tok, "top" | "btop" | "htop" | "sysmon")
                && !trimmed.contains('|')
                && !trimmed.contains(';')
                && !trimmed.contains("&&")
            {
                let mut sm = blaze_shell::sysmon_view::SysMonState::new();
                for arg in trimmed.split_whitespace().skip(1) {
                    match arg {
                        "--gpu" | "-g" => sm.engine.set_sort(blaze_sysmon::SortColumn::Gpu),
                        "--mem" | "-m" => sm.engine.set_sort(blaze_sysmon::SortColumn::Mem),
                        "--pid" | "-p" => sm.engine.set_sort(blaze_sysmon::SortColumn::Pid),
                        "--name" | "-n" => sm.engine.set_sort(blaze_sysmon::SortColumn::Name),
                        "--list" | "--flat" | "-l" => {
                            if sm.engine.tree_mode() {
                                sm.engine.toggle_tree_mode();
                            }
                        }
                        _ => {}
                    }
                }
                let snap = blaze_shell::sysmon_driver::sample_snapshot(Alloc::global());
                sm.engine.ingest(snap);
                sm.refresh_selected_cmdline();
                // If an earlier sysmon block was still live in this pane, freeze it cleanly.
                if let Some(prev_id) = self.active_sysmon.take()
                    && let Some(prev_blk) = self.store.block_mut(prev_id)
                    && let BlockBody::SysMon(prev_sm) = &mut prev_blk.body
                {
                    prev_sm.running = false;
                }
                let state_before = self.store.active_state().fork();
                let prompt_snap = self.prompt.clone();
                let bid = self.store.record_block(
                    trimmed.to_owned(),
                    prompt_snap.clone(),
                    state_before.fork(),
                    state_before,
                    now_ms(),
                    0,
                    0,
                    BlockBody::SysMon(Box::new(sm)),
                );
                self.active_sysmon = Some(bid);
                self.sysmon_fullscreen = false;
                // Schedule first delta tick quickly (~220ms) so live CPU % & GPU % appear right away!
                self.last_sysmon_tick = Instant::now() - std::time::Duration::from_millis(680);
                self.editor.record_history_entry(HistoryEntry {
                    command: trimmed.to_owned(),
                    host: prompt_snap.host,
                    cwd: prompt_snap.cwd,
                    git_branch: prompt_snap.git_branch,
                    duration_ms: 0,
                    exit_code: 0,
                    timestamp_ns: now_ns(),
                    in_session: true,
                });
                self.auto_scroll_bottom = true;
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
            let display_source = trimmed.to_owned();
            let trimmed = effective_cmd.as_str();
            let is_standalone_cd = (trimmed == "cd" || trimmed.starts_with("cd "))
                && !trimmed.contains('|')
                && !trimmed.contains(';')
                && !trimmed.contains("&&");

            let state_before = self.store.active_state().fork();
            let prompt_snap = self.prompt.clone();
            let started_ms = now_ms();

            // 4. Native `blaze-git` rich structured overrides (`git status`, `git branch`, `git stash list`)!
            let t_git = Instant::now();
            if let Some(git_val) = try_native_git_command(trimmed, &prompt_snap.cwd) {
                let elapsed = t_git.elapsed();
                let duration_ms = elapsed.as_millis() as u64;
                let duration_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
                let mut state_after = state_before.fork();
                self.bind_output_refs(&mut state_after, &git_val);
                let body = BlockBody::Value(git_val);
                let bid = self.store.record_block(
                    trimmed.to_owned(),
                    prompt_snap.clone(),
                    state_before,
                    state_after.fork(),
                    started_ms,
                    duration_ms,
                    0,
                    body,
                );
                if let Some(blk) = self.store.block_mut(bid) {
                    blk.duration_ns = duration_ns;
                }
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
                        let elapsed = t0.elapsed();
                        let duration_ms = elapsed.as_millis() as u64;
                        let duration_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
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
                            duration_ns,
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
            let res = run_command_line(trimmed, state_before.fork(), is_interactive_tty_cmd(trimmed));
            let elapsed = t0.elapsed();
            let duration_ms = elapsed.as_millis() as u64;
            let duration_ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;

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
            let new_cwd = state_after.cwd.as_ref().to_owned();
            // Automatically list the destination directory after a standalone `cd` succeeds
            // (so changing directories immediately shows the sleek compact `ls` grid!).
            let final_val = if is_standalone_cd
                && exit_code == 0
                && matches!(res.outcome.value, Value::Nothing)
            {
                let ls_res = run_command_line("ls", state_after.fork(), false);
                if ls_res.outcome.exit_code == 0 {
                    ls_res.outcome.value
                } else {
                    res.outcome.value
                }
            } else {
                res.outcome.value
            };
            self.bind_output_refs(&mut state_after, &final_val);
            let body = body_from_result(res.rust_meta, res.streamed_lines, final_val);

            let block_id = self.store.record_block(
                display_source.clone(),
                prompt_snap.clone(),
                state_before,
                state_after.fork(),
                started_ms,
                duration_ms,
                exit_code,
                body,
            );
            if let Some(blk) = self.store.block_mut(block_id) {
                blk.duration_ns = duration_ns;
            }
            self.store.set_active_state(state_after);
            if new_cwd != prompt_snap.cwd {
                let lh = self.editor.local_host.clone();
                self.editor.zoxide.record_visit(&lh, &new_cwd, now_ns());
            }
            self.editor.record_history_entry(HistoryEntry {
                command: display_source,
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
            if trimmed.starts_with("open ") && !trimmed.contains('|') && exit_code == 0 {
                self.scroll_to_block = Some(block_id);
                self.auto_scroll_bottom = false;
            } else {
                self.scroll_to_block = None;
                self.auto_scroll_bottom = true;
            }
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
                    blk.duration_ns = done.duration_ns;
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
            let (cols, rows) = if self.pty_fullscreen {
                (cols, rows)
            } else {
                let max_inline = rows.saturating_sub(3).max(12);
                (
                    cols.saturating_sub(4).max(20),
                    (rows.saturating_mul(65) / 100).max(12).min(max_inline),
                )
            };
            let px = (
                cols.saturating_mul(cell_px.0 as u16),
                rows.saturating_mul(cell_px.1 as u16),
            );

            let alloc = Alloc::global();
            match Pty::spawn(&argv_refs, &spawn.cwd, cols, rows, px) {
                Ok(pty) => {
                    let child_pid = Some(pty.pid());
                    let mut opts = VtOpts::new(cols, rows, 4 << 20);
                    opts.xtversion = b"blaze-shell";
                    opts.device.secondary = blaze_vt::reply::Da2::APP;
                    opts.cell_px = Some(blaze_vt::input::mouse::Pixels {
                        width: cell_px.0,
                        height: cell_px.1,
                    });
                    if let Ok(mut term) = Terminal::new_in(opts, alloc) {
                        term.set_focused(true);
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
                        self.pty_focused = true;
                        self.pty_inline_rows_override = None;
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
            let short_cwd = blaze_shell::prompt::abbreviate_home(cwd, &self.home);
            self.managed_agents.push(AgentRecord {
                id: format!("agent-{id_num}"),
                host: host.to_owned(),
                is_remote,
                name: format!("agent #{id_num} ({host})"),
                kind_label: if is_remote {
                    "server".to_owned()
                } else {
                    "claude".to_owned()
                },
                prompt_summary: prompt_text
                    .clone()
                    .unwrap_or_else(|| format!("{short_cwd} · interactive")),
                metric_badge: "live".to_owned(),
                cwd: cwd.to_owned(),
                pid: None,
                status: AgentStatus::Running,
                pty_session_idx: Some(id_num - 1),
                preview_lines: vec![
                    format!("host:   {host}"),
                    format!("cwd:    {short_cwd}"),
                    format!("cmd:    {display_cmd}"),
                ],
                cmdline: display_cmd.clone(),
                ..AgentRecord::default()
            });
            let spawn_cwd = if !is_remote && std::path::Path::new(cwd).is_dir() {
                cwd.to_owned()
            } else {
                self.prompt.cwd.clone()
            };
            let spawn = InteractiveSpawn {
                program,
                args,
                cwd: spawn_cwd,
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

    fn grid_for_rect(rect: HitRect, fonts: &Fonts, scale: f64) -> (u16, u16) {
        let m = fonts.metrics;
        let rw = f64::from((rect.x1 - rect.x0).max(100.0));
        let rh = f64::from((rect.y1 - rect.y0).max(80.0));
        let w = ((rw - 24.0) * scale / m.cell_w as f64).floor().max(20.0);
        let h = ((rh - 16.0) * scale / m.cell_h as f64).floor().max(6.0);
        (w as u16, h as u16)
    }

    fn sync_pty_sizes(
        panes: &mut std::collections::HashMap<PaneId, ShellApp>,
        tile_layout: &TileLayout<'_>,
        fonts: &Fonts,
        scale: f64,
    ) {
        for pl in &tile_layout.panes {
            let content_rect: HitRect = pl.rect.into();
            if let Some(pane) = panes.get_mut(&pl.pane)
                && let Some(ap) = &pane.active_pty
            {
                let (raw_cols, raw_rows) = grid_for_rect(content_rect, fonts, scale);
                let (cols, rows) = if pane.pty_fullscreen {
                    (raw_cols, raw_rows)
                } else {
                    let max_inline = raw_rows.saturating_sub(3).max(12);
                    let default_rows = (raw_rows.saturating_mul(65) / 100).max(12).min(max_inline);
                    let target_rows = pane
                        .pty_inline_rows_override
                        .map_or(default_rows, |r| r.clamp(8, max_inline));
                    (raw_cols.saturating_sub(4).max(20), target_rows)
                };
                let px = (
                    cols.saturating_mul(fonts.metrics.cell_w as u16),
                    rows.saturating_mul(fonts.metrics.cell_h as u16),
                );
                ap.pty.resize(cols, rows, px);
                if let Some(blk) = pane.store.block_mut(ap.block_id)
                    && let BlockBody::Terminal { term, .. } = &mut blk.body
                {
                    term.resize(cols, rows);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_window_scene(
        wm: &TileManager<'static>,
        panes: &mut std::collections::HashMap<PaneId, ShellApp>,
        vw: f32,
        vh: f32,
        metrics: blaze_shell::view::CellMetrics,
        cursor_vis: bool,
        frame_now: i64,
        zoom_pct: u16,
        inline_titlebar: bool,
        active_drag: Option<&ActiveDrag>,
        active_trail: Option<CursorTrail>,
        output_selection: Option<TextSelection>,
        active_popover: Option<&ActivePopover>,
        mouse_pos: Option<[f32; 2]>,
        output_nav: Option<OutputNav>,
        dragging_split: Option<SplitHandle>,
        command_palette: Option<&CommandPalette>,
        sidebar_open: bool,
    ) -> (Scene, TileLayout<'static>) {
        let sidebar_w = if sidebar_open {
            SIDEBAR_W.min(vw * 0.34).max(180.0)
        } else {
            0.0
        };
        let work_area = HitRect::new(sidebar_w, TOP_BAR_H, vw, vh.max(TOP_BAR_H + 60.0));
        let tile_layout = wm.layout(work_area.into(), TILE_GAP);
        let mut master = Scene::new();
        let is_split = tile_layout.pane_count_in_tab > 1 && !tile_layout.is_zoomed;

        // 1. Lay out and composite every visible pane in the active workspace tab!
        for pl in &tile_layout.panes {
            let Some(pane) = panes.get_mut(&pl.pane) else {
                continue;
            };
            let content_rect: HitRect = pl.rect.into();
            let pw = (content_rect.x1 - content_rect.x0).max(120.0);
            let ph = (content_rect.y1 - content_rect.y0).max(80.0);
            let sub_vh = ph + TOP_BAR_H;
            let pane_cursor_vis = cursor_vis && pl.focused;

            // Translate active window-space popover & mouse pos into pane-local space for the focused pane.
            let local_dx = content_rect.x0;
            let local_dy = content_rect.y0 - TOP_BAR_H;
            let local_popover = if pl.focused {
                active_popover.map(|p| ActivePopover {
                    anchor: HitRect::new(
                        p.anchor.x0 - local_dx,
                        p.anchor.y0 - local_dy,
                        p.anchor.x1 - local_dx,
                        p.anchor.y1 - local_dy,
                    ),
                    kind: p.kind.clone(),
                    pinned: p.pinned,
                })
            } else {
                None
            };
            let local_mouse = mouse_pos.map(|[mx, my]| [mx - local_dx, my - local_dy]);
            let local_drag = if pl.focused {
                active_drag.map(|d| ActiveDrag {
                    x: d.x - local_dx,
                    y: d.y - local_dy,
                    over_composer: d.over_composer,
                    payload: d.payload.clone(),
                })
            } else {
                None
            };
            let local_trail = if pl.focused {
                active_trail.map(|t| CursorTrail {
                    prev_rect: [
                        t.prev_rect[0] - local_dx,
                        t.prev_rect[1] - local_dy,
                        t.prev_rect[2] - local_dx,
                        t.prev_rect[3] - local_dy,
                    ],
                    progress: t.progress,
                })
            } else {
                None
            };

            let sub = if let Some(yazi) = &mut pane.yazi {
                layout_yazi_pane(yazi, &pane.store, &pane.prompt, pw, sub_vh, metrics, zoom_pct)
            } else if let Some(hub) = &mut pane.agent_hub {
                layout_agent_hub_pane(hub, &pane.store, &pane.prompt, pw, sub_vh, metrics, zoom_pct)
            } else if pane.sysmon_fullscreen
                && let Some(sid) = pane.active_sysmon
                && let Some(blk) = pane.store.block(sid)
                && let BlockBody::SysMon(state) = &blk.body
            {
                let mut sc = Scene::new();
                let avail_h = (sub_vh - TOP_BAR_H - 12.0).max(200.0);
                let _ = blaze_shell::sysmon_view::layout_sysmon_block(
                    &mut sc,
                    sid,
                    state,
                    8.0,
                    TOP_BAR_H + 6.0,
                    (pw - 8.0).max(260.0),
                    Some(avail_h),
                    metrics,
                    0,
                    true,
                );
                sc
            } else if pane.pty_fullscreen
                && pane.store.is_alternate_screen()
                && let Some(tid) = pane.store.active_running_terminal()
                && let Some(blk) = pane.store.block(tid)
                && let BlockBody::Terminal { term, .. } = &blk.body
            {
                layout_fullscreen_terminal_pane(
                    term,
                    &pane.store,
                    &pane.prompt,
                    pw,
                    sub_vh,
                    metrics,
                    pane_cursor_vis,
                    zoom_pct,
                )
            } else {
                let screen_sel = |sy: f32| {
                    if !pl.focused {
                        return None;
                    }
                    output_selection.map(|s| TextSelection {
                        anchor: [s.anchor[0] - local_dx, s.anchor[1] - sy - local_dy],
                        head: [s.head[0] - local_dx, s.head[1] - sy - local_dy],
                    })
                };
                let first = build_pane_scene(&LayoutInput {
                    store: &pane.store,
                    editor: &pane.editor,
                    prompt: &pane.prompt,
                    viewport_w: pw,
                    viewport_h: sub_vh,
                    metrics,
                    scroll_y: pane.scroll_y,
                    cursor_visible: pane_cursor_vis,
                    pty_focused: pane_cursor_vis && pane.pty_accepts_input(),
                    now_ns: frame_now,
                    zoom_pct,
                    inline_titlebar: false,
                    drag: local_drag.as_ref(),
                    cursor_trail: local_trail,
                    text_selection: screen_sel(pane.scroll_y),
                    active_popover: local_popover.as_ref(),
                    mouse_pos: local_mouse,
                    output_nav: if pl.focused { output_nav } else { None },
                });
                let max_s = (first.total_content_height - first.scroll_viewport_height).max(0.0);
                let target_scroll = if let Some(bid) = pane.scroll_to_block.take() {
                    if let Some(&(_, by)) = first.block_tops.iter().find(|(id, _)| *id == bid) {
                        (by - 8.0).clamp(0.0, max_s)
                    } else {
                        max_s
                    }
                } else if pane.auto_scroll_bottom {
                    pane.auto_scroll_bottom = false;
                    max_s
                } else {
                    pane.scroll_y.clamp(0.0, max_s)
                };
                if (target_scroll - pane.scroll_y).abs() > 0.5 {
                    pane.scroll_y = target_scroll;
                    build_pane_scene(&LayoutInput {
                        store: &pane.store,
                        editor: &pane.editor,
                        prompt: &pane.prompt,
                        viewport_w: pw,
                        viewport_h: sub_vh,
                        metrics,
                        scroll_y: pane.scroll_y,
                        cursor_visible: pane_cursor_vis,
                        pty_focused: pane_cursor_vis && pane.pty_accepts_input(),
                        now_ns: frame_now,
                        zoom_pct,
                        inline_titlebar: false,
                        drag: local_drag.as_ref(),
                        cursor_trail: local_trail,
                        text_selection: screen_sel(pane.scroll_y),
                        active_popover: local_popover.as_ref(),
                        mouse_pos: local_mouse,
                        output_nav: if pl.focused { output_nav } else { None },
                    })
                } else {
                    first
                }
            };

            // Remember each pane's own content height / viewport height for hover-scrolling!
            pane.last_scene.total_content_height = sub.total_content_height;
            pane.last_scene.scroll_viewport_height = sub.scroll_viewport_height;

            master.composite_pane(sub, content_rect, TOP_BAR_H, pl.focused, vw, vh);

            // Subtle Ghostty-style dim veil on unfocused tiled panes so focus is unmistakable.
            if is_split && !pl.focused {
                master.over_round_rect(
                    pl.rect.x0,
                    pl.rect.y0,
                    pl.rect.x1,
                    pl.rect.y1,
                    0.0,
                    theme::rgba(0x08, 0x08, 0x0a, 0.22),
                    0,
                );
            }
        }

        // 2. Crisp hairline split dividers between tiled panes.
        if !tile_layout.handles.is_empty() {
            draw_split_dividers(&mut master, &tile_layout.handles, dragging_split.map(|h| h.split));
        }

        // 3. Unified window-level titlebar with workspace tabs (`Cmd+1..9`), `+` (`Cmd+T`),
        //    zoomed badge, split buttons (`Cmd+D` / `Cmd+Shift+D`), and new window (`Cmd+N`).
        let focused_id = wm.focused_pane();
        let tab_items: Vec<TabBarItem<'_>> = wm
            .tabs()
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let p = panes.get(&tab.focused);
                let title = p.map_or("shell", |s| {
                    s.prompt
                        .project_label_override
                        .as_deref()
                        .unwrap_or(s.prompt.dir_short.as_str())
                });
                let symbol = p.and_then(|s| s.prompt.project_symbol.as_deref());
                let process_tag = p.and_then(|s| s.process_tag());
                let branches = p.map_or_else(Vec::new, |s| collect_branch_tree(&s.store));
                TabBarItem {
                    id: tab.id,
                    slot: i + 1,
                    symbol,
                    title,
                    active: i == wm.active_tab_index(),
                    pane_count: tab.root.leaf_count(),
                    zoomed: tab.zoomed,
                    process_tag,
                    branches,
                }
            })
            .collect();
        if sidebar_open {
            layout_workspace_sidebar(&mut master, &tab_items, sidebar_w, vh, metrics, mouse_pos);
        }
        if let Some(focused) = panes.get(&focused_id) {
            layout_window_top_bar(
                &mut master,
                &tab_items,
                focused_id,
                tile_layout.is_zoomed,
                tile_layout.pane_count_in_tab,
                &focused.store,
                &focused.prompt,
                vw,
                metrics,
                zoom_pct,
                inline_titlebar,
                sidebar_open,
            );
        }

        // 4. Optional `Cmd+Shift+P` Command Palette rendered above everything.
        if let Some(pal) = command_palette {
            layout_command_palette(&mut master, pal, vw, vh, metrics);
        }

        // 5. Safari/Chrome-style subtle bottom-left URL preview pill when hovering a clickable link.
        if command_palette.is_none()
            && let Some([mx, my]) = mouse_pos
            && let Some(HitAction::OpenLink { href, .. }) = master.hit_test(mx, my)
        {
            let max_chars = (((vw * 0.55) / metrics.measure("n", TextFace::UiSmall).max(5.0))
                .floor() as usize)
                .clamp(24, 96);
            let shown = blaze_shell::view::truncate_chars(&href, max_chars);
            let tw = metrics.measure(&shown, TextFace::UiSmall);
            let pw = (tw + 18.0).min(vw - 24.0);
            let ph = metrics.line_h(TextFace::UiSmall) + 8.0;
            let px0 = 10.0;
            let py0 = (vh - ph - 8.0).max(TOP_BAR_H + 8.0);
            master.overlay_round_rect(
                px0,
                py0,
                px0 + pw,
                py0 + ph,
                5.0,
                theme::rgba(0x14, 0x15, 0x1a, 0.94),
                1.0,
                theme::LINE,
                0,
            );
            master.overlay_text.push(blaze_shell::view::TextRun {
                x: px0 + 9.0,
                y: py0 + 4.0,
                text: shown,
                color: theme::INK_2,
                bold: false,
                italic: false,
                face: TextFace::UiSmall,
                clip_id: 0,
            });
        }

        (master, tile_layout)
    }

    fn auto_split_axis(tile_layout: &TileLayout<'_>) -> SplitAxis {
        tile_layout
            .panes
            .iter()
            .find(|p| p.focused)
            .map_or(SplitAxis::Vertical, |p| {
                if (p.rect.x1 - p.rect.x0) >= (p.rect.y1 - p.rect.y0) {
                    SplitAxis::Vertical
                } else {
                    SplitAxis::Horizontal
                }
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_palette_action(
        action: PaletteAction,
        wm: &mut TileManager<'static>,
        tile_layout: &TileLayout<'_>,
        panes: &mut std::collections::HashMap<PaneId, ShellApp>,
        font_points: &mut f64,
        zoom_changed: &mut bool,
        pty_sizes_dirty: &mut bool,
        sidebar_open: &mut bool,
        quit: &mut bool,
    ) {
        let cwd = panes
            .get(&wm.focused_pane())
            .map(|p| p.cwd().to_owned())
            .unwrap_or_else(|| "/".to_owned());
        match action {
            PaletteAction::NewTab => {
                let (_, new_pid) = wm.new_tab();
                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                *pty_sizes_dirty = true;
            }
            PaletteAction::ToggleSidebar => {
                *sidebar_open = !*sidebar_open;
                *pty_sizes_dirty = true;
            }
            PaletteAction::NewTiledPane => {
                let new_pid = wm.split_focused(auto_split_axis(tile_layout));
                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                *pty_sizes_dirty = true;
            }
            PaletteAction::NewWindow => spawn_new_window(&cwd),
            PaletteAction::SplitVertical => {
                let new_pid = wm.split_focused(SplitAxis::Vertical);
                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                *pty_sizes_dirty = true;
            }
            PaletteAction::SplitHorizontal => {
                let new_pid = wm.split_focused(SplitAxis::Horizontal);
                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                *pty_sizes_dirty = true;
            }
            PaletteAction::ToggleZoom => {
                if wm.toggle_zoom() {
                    *pty_sizes_dirty = true;
                }
            }
            PaletteAction::EqualizeSplits => {
                wm.equalize_splits();
                *pty_sizes_dirty = true;
            }
            PaletteAction::ClosePane => match wm.close_focused() {
                CloseOutcome::PaneClosed { closed, .. } => {
                    if let Some(mut p) = panes.remove(&closed)
                        && let Some(ap) = p.active_pty.take()
                    {
                        ap.pty.hangup();
                    }
                    *pty_sizes_dirty = true;
                }
                CloseOutcome::TabClosed { closed_panes, .. } => {
                    for pid in closed_panes {
                        if let Some(mut p) = panes.remove(&pid)
                            && let Some(ap) = p.active_pty.take()
                        {
                            ap.pty.hangup();
                        }
                    }
                    *pty_sizes_dirty = true;
                }
                CloseOutcome::LastPaneInLastTab => *quit = true,
            },
            PaletteAction::ForkBranch => {
                if let Some(p) = panes.get_mut(&wm.focused_pane()) {
                    let _ = p.store.fork_active_branch(None);
                    p.refresh_prompt(p.prompt.last_exit, p.prompt.last_duration_ms);
                }
            }
            PaletteAction::OpenYazi => {
                if let Some(p) = panes.get_mut(&wm.focused_pane()) {
                    let dir = std::path::PathBuf::from(p.cwd());
                    p.yazi = Some(YaziState::open(&dir));
                    p.agent_hub = None;
                }
            }
            PaletteAction::OpenAgentHub => {
                if let Some(p) = panes.get_mut(&wm.focused_pane()) {
                    let host = p.editor.local_host.clone();
                    let home = p.home.clone();
                    let c = p.cwd().to_owned();
                    p.agent_hub = Some(AgentHub::open(&host, &home, &c, &p.managed_agents));
                    p.yazi = None;
                }
            }
            PaletteAction::OpenHistory => {
                if let Some(p) = panes.get_mut(&wm.focused_pane()) {
                    let c = p.cwd().to_owned();
                    p.editor.start_reverse_search_in(&c);
                }
            }
            PaletteAction::ClearScreen => {
                if let Some(p) = panes.get_mut(&wm.focused_pane()) {
                    p.store.clear_active_branch();
                    p.scroll_y = 0.0;
                }
            }
            PaletteAction::ZoomIn => {
                *font_points = (*font_points + ZOOM_STEP_POINTS).min(MAX_FONT_POINTS);
                *zoom_changed = true;
            }
            PaletteAction::ZoomOut => {
                *font_points = (*font_points - ZOOM_STEP_POINTS).max(MIN_FONT_POINTS);
                *zoom_changed = true;
            }
            PaletteAction::ResetZoom => {
                *font_points = FONT_POINTS;
                *zoom_changed = true;
            }
        }
    }

    /// Install standard macOS Application and Window menus so system shortcuts
    /// (`Cmd+H` Hide, `Cmd+Opt+H` Hide Others, `Cmd+M` Minimize, `Cmd+Q` Quit,
    /// `Cmd+`` ` window cycling) behave like a first-class native macOS app.
    fn install_macos_menu(app: &NSApplication, mtm: MainThreadMarker) {
        let menubar = NSMenu::new(mtm);

        // 1. Application menu ("blaze-shell").
        let app_menu_item = NSMenuItem::new(mtm);
        let app_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("blaze-shell"));
        // SAFETY: standard AppKit responder selectors (`hide:`, `hideOtherApplications:`,
        // `unhideAllApplications:`, `terminate:`) are valid Objective-C selectors on NSApplication.
        unsafe {
            let hide_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Hide blaze-shell"),
                Some(sel!(hide:)),
                &NSString::from_str("h"),
            );
            app_menu.addItem(&hide_item);

            let hide_others = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Hide Others"),
                Some(sel!(hideOtherApplications:)),
                &NSString::from_str("h"),
            );
            hide_others.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Command | NSEventModifierFlags::Option,
            );
            app_menu.addItem(&hide_others);

            let show_all = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Show All"),
                Some(sel!(unhideAllApplications:)),
                &NSString::from_str(""),
            );
            app_menu.addItem(&show_all);

            app_menu.addItem(&NSMenuItem::separatorItem(mtm));

            let quit_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit blaze-shell"),
                Some(sel!(terminate:)),
                &NSString::from_str("q"),
            );
            app_menu.addItem(&quit_item);
        }
        app_menu_item.setSubmenu(Some(&app_menu));
        menubar.addItem(&app_menu_item);

        // 2. Window menu ("Window").
        let win_menu_item = NSMenuItem::new(mtm);
        let win_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Window"));
        // SAFETY: standard AppKit NSWindow/NSApplication selectors.
        unsafe {
            let min_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Minimize"),
                Some(sel!(performMiniaturize:)),
                &NSString::from_str("m"),
            );
            win_menu.addItem(&min_item);

            let zoom_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Zoom"),
                Some(sel!(performZoom:)),
                &NSString::from_str(""),
            );
            win_menu.addItem(&zoom_item);

            win_menu.addItem(&NSMenuItem::separatorItem(mtm));

            let front_item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Bring All to Front"),
                Some(sel!(arrangeInFront:)),
                &NSString::from_str(""),
            );
            win_menu.addItem(&front_item);
        }
        win_menu_item.setSubmenu(Some(&win_menu));
        menubar.addItem(&win_menu_item);

        app.setMainMenu(Some(&menubar));
        app.setWindowsMenu(Some(&win_menu));
    }

    enum ResizeEvent {
        WillStartLiveResize,
        FrameSize { size: NSSize, live: bool },
        DidEndLiveResize,
    }

    thread_local! {
        static RESIZE_HOOK: RefCell<Option<*mut dyn FnMut(ResizeEvent)>> = const { RefCell::new(None) };
    }

    fn dispatch_resize_event(ev: ResizeEvent) {
        let ptr = RESIZE_HOOK.with(|c| c.borrow_mut().take());
        if let Some(p) = ptr {
            // SAFETY: `p` is installed on the main thread for the dynamic extent of an AppKit
            // modal call via `with_resize_hook` and restored before returning; taking prevents
            // re-entrant double-borrows if AppKit nests another layout call.
            unsafe { (*p)(ev) };
            RESIZE_HOOK.with(|c| *c.borrow_mut() = Some(p));
        }
    }

    fn with_resize_hook<R>(f: &mut dyn FnMut(ResizeEvent), body: impl FnOnce() -> R) -> R {
        struct Reset(Option<*mut dyn FnMut(ResizeEvent)>);
        impl Drop for Reset {
            fn drop(&mut self) {
                RESIZE_HOOK.with(|c| *c.borrow_mut() = self.0);
            }
        }
        // SAFETY: `f` outlives `body()` and `Reset::drop` unconditionally clears the thread-local
        // hook before `f` can be invalidated (even across panics).
        let raw: *mut dyn FnMut(ResizeEvent) = unsafe { std::mem::transmute(f) };
        let prev = RESIZE_HOOK.with(|c| c.borrow_mut().replace(raw));
        let _guard = Reset(prev);
        body()
    }

    define_class!(
        // SAFETY: `NSView` subclass with empty ivars on the main thread only; forwards to super.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "BlazeMetalView"]
        struct BlazeMetalView;

        unsafe impl NSObjectProtocol for BlazeMetalView {}

        impl BlazeMetalView {
            #[unsafe(method(setFrameSize:))]
            fn set_frame_size(&self, size: NSSize) {
                // SAFETY: forwarding the same selector to `NSView`'s superclass implementation.
                let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
                dispatch_resize_event(ResizeEvent::FrameSize { size, live: self.inLiveResize() });
            }

            #[unsafe(method(viewWillStartLiveResize))]
            fn will_start_live_resize(&self) {
                // SAFETY: forwarding to superclass.
                let _: () = unsafe { msg_send![super(self), viewWillStartLiveResize] };
                dispatch_resize_event(ResizeEvent::WillStartLiveResize);
            }

            #[unsafe(method(viewDidEndLiveResize))]
            fn did_end_live_resize(&self) {
                // SAFETY: forwarding to superclass.
                let _: () = unsafe { msg_send![super(self), viewDidEndLiveResize] };
                dispatch_resize_event(ResizeEvent::DidEndLiveResize);
            }

            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
                true
            }
        }
    );

    impl BlazeMetalView {
        fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            // SAFETY: calling `NSView`'s designated initializer `initWithFrame:` on main thread.
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }
    }

    /// Synchronized Metal live-resize & external-resize handler (exact `den-native` protocol):
    /// updates `CAMetalLayer`'s drawable size with implicit animations disabled, reflows the full
    /// scene in place using pre-shaped text runs and path-keyed GPU images, and presents inside
    /// the active CoreAnimation transaction (`presentsWithTransaction`) so window borders and text
    /// move atomically with zero stretching, black gaps, or lag.
    #[allow(clippy::too_many_arguments)]
    fn handle_resize_event(
        ev: ResizeEvent,
        window: &NSWindow,
        layer: &CAMetalLayer,
        renderer: &Renderer,
        wm: &TileManager<'static>,
        panes: &mut std::collections::HashMap<PaneId, ShellApp>,
        fonts: &mut Fonts,
        icon_atlas: &mut IconAtlas,
        image_atlas: &mut ImageAtlas,
        size: &mut NSSize,
        scale: &mut f64,
        font_points: f64,
        anim_epoch: Instant,
        has_animated_images: &mut bool,
        tile_layout: &mut TileLayout<'static>,
        composite_scene: &mut Scene,
        pty_sizes_dirty: &mut bool,
        dirty: &mut bool,
        last_pty_sync: &mut Instant,
        sidebar_open: bool,
    ) {
        match ev {
            ResizeEvent::WillStartLiveResize => {
                layer.setPresentsWithTransaction(true);
            }
            ResizeEvent::DidEndLiveResize => {
                layer.setPresentsWithTransaction(false);
                *pty_sizes_dirty = true;
                *dirty = true;
            }
            ResizeEvent::FrameSize { size: new_size, live } => {
                if new_size.width < 40.0 || new_size.height < 40.0 || !window.isVisible() {
                    return;
                }
                let new_scale = window.backingScaleFactor().max(1.0);
                if (new_scale - *scale).abs() > f64::EPSILON {
                    *scale = new_scale;
                    *fonts = Fonts::new(font_points, *scale);
                    *icon_atlas = IconAtlas::new(*scale * (font_points / FONT_POINTS));
                    image_atlas.set_scale(*scale);
                }
                *size = new_size;
                let px_w = (size.width * *scale).round().max(1.0);
                let px_h = (size.height * *scale).round().max(1.0);
                CATransaction::begin();
                CATransaction::setDisableActions(true);
                layer.setContentsScale(*scale);
                layer.setDrawableSize(NSSize::new(px_w, px_h));
                CATransaction::commit();

                if !live {
                    layer.setPresentsWithTransaction(true);
                }

                let metrics = fonts.cell_metrics(*scale);
                let cursor_vis = window.isKeyWindow();
                let zoom_pct = ((font_points / FONT_POINTS) * 100.0).round() as u16;
                let (mut scene, next_tl) = build_window_scene(
                    wm,
                    panes,
                    size.width as f32,
                    size.height as f32,
                    metrics,
                    cursor_vis,
                    now_ns(),
                    zoom_pct,
                    true,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    sidebar_open,
                );
                *tile_layout = next_tl;

                if !live || last_pty_sync.elapsed().as_millis() >= 50 {
                    sync_pty_sizes(panes, tile_layout, fonts, *scale);
                    *last_pty_sync = Instant::now();
                    *pty_sizes_dirty = false;
                } else {
                    *pty_sizes_dirty = true;
                }

                let vh = size.height as f32;
                let glyphs = fonts.rasterize_runs(&scene.text_runs, *scale, vh);
                let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, *scale, vh);
                for (slot, bytes) in fonts.pending.drain(..) {
                    renderer.upload(slot, &bytes);
                }
                let mut icons = icon_atlas.rasterize_quads(&scene.icon_quads);
                icon_atlas.rasterize_emoji_quads_into(&scene.emoji_quads, &mut icons);
                icon_atlas.rasterize_custom_symbol_quads_into(&scene.custom_symbols, &mut icons);
                let overlay_icons = icon_atlas.rasterize_quads(&scene.overlay_icons);
                for (slot, rgba) in icon_atlas.pending.drain(..) {
                    renderer.upload_icon(slot, &rgba);
                }
                let anim_ms = anim_epoch.elapsed().as_millis() as u64;
                let (images, any_anim) = image_atlas.rasterize_quads(&scene.image_quads, anim_ms, vh);
                let (overlay_images, any_ov_anim) =
                    image_atlas.rasterize_quads(&scene.overlay_images, anim_ms, vh);
                *has_animated_images = any_anim || any_ov_anim;
                image_atlas.drain_pending_into(|slot, rgba| renderer.upload_image(slot, rgba));

                let vp = ViewportUniform {
                    size: [size.width as f32, size.height as f32],
                    dpr: *scale as f32,
                    time: 0.0,
                };
                let frame = FrameDraw {
                    vp,
                    clear: scene.clear_color,
                    clips: &scene.clips,
                    rects: &scene.rects,
                    glyphs: &glyphs,
                    icons: &icons,
                    images: &images,
                    over_rects: &scene.over_rects,
                    overlay_rects: &scene.overlay_rects,
                    overlay_icons: &overlay_icons,
                    overlay_images: &overlay_images,
                    overlay_glyphs: &overlay_glyphs,
                };
                renderer.draw(layer, &frame);
                if !live {
                    CATransaction::flush();
                    layer.setPresentsWithTransaction(false);
                }
                scene.rects.clear();
                scene.icon_quads.clear();
                scene.emoji_quads.clear();
                scene.custom_symbols.clear();
                scene.image_quads.clear();
                scene.over_rects.clear();
                scene.overlay_rects.clear();
                scene.overlay_icons.clear();
                scene.overlay_images.clear();
                scene.overlay_text.clear();
                *composite_scene = scene;
                *dirty = false;
            }
        }
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
        if cli.screenshot.is_none() {
            install_macos_menu(&app, mtm);
        }
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
        window.setMinSize(NSSize::new(420.0, 260.0));
        let view = BlazeMetalView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(init_w, init_h)));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        window.setContentView(Some(&view));

        let renderer = Renderer::new(MTLCreateSystemDefaultDevice().expect("no Metal GPU device"));
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(renderer.device()));
        layer.setPixelFormat(PIXEL_FORMAT);
        layer.setFramebufferOnly(true);
        // Disable implicit CoreAnimation stretch/fade actions and anchor contents top-left
        // (matching `den-native`) so window resizes never stretch old frames or flash white.
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // SAFETY: `kCAGravityTopLeft` is a valid QuartzCore static NSString constant.
        layer.setContentsGravity(unsafe { kCAGravityTopLeft });
        let ground = CGColor::new_srgb(
            0x10 as f64 / 255.0,
            0x10 as f64 / 255.0,
            0x11 as f64 / 255.0,
            1.0,
        );
        layer.setBackgroundColor(Some(&ground));
        CATransaction::commit();
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
        let mut image_atlas = ImageAtlas::new(scale);
        let mut size = view.bounds().size;
        layer.setContentsScale(scale);
        layer.setDrawableSize(NSSize::new(size.width * scale, size.height * scale));

        let term_home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
        let term_server = blaze_shell::term_server::start_global(&term_home);
        let mut last_term_rev = term_server.revision();

        let (mut wm, _, first_pane_id) = TileManager::new(blaze_own::Alloc::global());
        let mut panes: std::collections::HashMap<PaneId, ShellApp> = std::collections::HashMap::new();
        let mut first_pane = ShellApp::new_in(cli.cwd.as_deref());
        let (init_cols, init_rows) = grid_for(size, &fonts, scale);
        let cell_px = (fonts.metrics.cell_w as u32, fonts.metrics.cell_h as u32);
        let mut demo_popover: Option<ActivePopover> = None;
        if cli.demo {
            first_pane.populate_demo(
                init_cols,
                init_rows,
                cell_px,
                cli.scroll_bottom,
                cli.demo_popup,
                cli.demo_history,
            );
        }
        if let Some(ref startup_cmd) = cli.cmd {
            first_pane.submit_command(startup_cmd, init_cols, init_rows, cell_px, false);
            if let Some(sy) = cli.scroll_y {
                first_pane.scroll_y = sy.max(0.0);
                first_pane.auto_scroll_bottom = false;
                first_pane.scroll_to_block = None;
            } else if !cli.scroll_bottom {
                first_pane.scroll_y = 0.0;
                first_pane.auto_scroll_bottom = false;
                first_pane.scroll_to_block = None;
            }
        }
        let demo_cwd = first_pane.cwd().to_owned();
        panes.insert(first_pane_id, first_pane);

        if cli.demo_tiled {
            // Create a 3-pane SOTA BSP layout (left: shell + ls -l, top-right: yazi, bottom-right: git status)
            // plus a 2nd workspace tab so screenshots verify both tiling chrome and workspace tabs!
            let right_top_id = wm.split_focused(SplitAxis::Vertical);
            let mut right_top = ShellApp::new_in(Some(&demo_cwd));
            right_top.yazi = Some(YaziState::open(std::path::Path::new(&demo_cwd)));
            panes.insert(right_top_id, right_top);

            let right_bot_id = wm.split_focused(SplitAxis::Horizontal);
            let mut right_bot = ShellApp::new_in(Some(&demo_cwd));
            right_bot.submit_command("git status", init_cols / 2, init_rows / 2, cell_px, false);
            panes.insert(right_bot_id, right_bot);

            let (_, tab2_pane_id) = wm.new_tab();
            panes.insert(tab2_pane_id, ShellApp::new_in(Some(&demo_cwd)));
            let _ = wm.select_tab_slot(1);
            wm.focus_pane(first_pane_id);

            // Also preview the rich `site/` folder hover popover if present!
            let site_dir = std::path::Path::new(&demo_cwd).join("site");
            if site_dir.is_dir() {
                demo_popover = Some(ActivePopover {
                    anchor: HitRect::new(24.0, 210.0, 140.0, 232.0),
                    kind: load_hover_preview(&site_dir, "site/", Some("folder_macos"), 14),
                    pinned: true,
                });
            }
        }

        // Offscreen deterministic screenshot mode: render 1 frame (warming atlases) and exit.
        if let Some(shot_path) = cli.screenshot {
            let metrics = fonts.cell_metrics(scale);
            let frame_now = now_ns();
            let zoom_pct = ((font_points / FONT_POINTS) * 100.0).round() as u16;
            let (scene, _) = build_window_scene(
                &wm,
                &mut panes,
                size.width as f32,
                size.height as f32,
                metrics,
                true,
                frame_now,
                zoom_pct,
                true,
                None,
                None,
                None,
                demo_popover.as_ref(),
                None,
                None,
                None,
                None,
                false,
            );
            let vh = size.height as f32;
            let glyphs = fonts.rasterize_runs(&scene.text_runs, scale, vh);
            let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale, vh);
            for (slot, bytes) in fonts.pending.drain(..) {
                renderer.upload(slot, &bytes);
            }
            let mut icons = icon_atlas.rasterize_quads(&scene.icon_quads);
            icon_atlas.rasterize_emoji_quads_into(&scene.emoji_quads, &mut icons);
            icon_atlas.rasterize_custom_symbol_quads_into(&scene.custom_symbols, &mut icons);
            let overlay_icons = icon_atlas.rasterize_quads(&scene.overlay_icons);
            for (slot, rgba) in icon_atlas.pending.drain(..) {
                renderer.upload_icon(slot, &rgba);
            }
            let (images, _) = image_atlas.rasterize_quads(&scene.image_quads, 0, vh);
            let (overlay_images, _) = image_atlas.rasterize_quads(&scene.overlay_images, 0, vh);
            image_atlas.drain_pending_into(|slot, rgba| renderer.upload_image(slot, rgba));
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
                images: &images,
                over_rects: &scene.over_rects,
                overlay_rects: &scene.overlay_rects,
                overlay_icons: &overlay_icons,
                overlay_images: &overlay_images,
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
        let mut was_key_window = window.isKeyWindow();
        let mut pty_sizes_dirty = false;
        let mut composite_scene = Scene::new();
        let mut tile_layout = TileLayout::empty(blaze_own::Alloc::global());
        let mut dragging_split: Option<SplitHandle> = None;
        let mut dragging_agent_sidebar = false;
        let mut dragging_table_col: Option<(BlockId, usize, f32)> = None;
        let mut last_cli_spec_rev = blaze_shell::cli_spec_driver::spec_revision();
        let mut last_dust_rev = blaze_shell::dust_driver::dust_revision();
        let mut last_git_meta_rev = blaze_shell::git_meta_driver::git_meta_revision();
        let mut last_remote_rev = blaze_shell::remote_driver::remote_complete_revision();
        let mut last_app_icon_rev = blaze_shell::sysmon_driver::app_icon_revision();
        let mut last_agent_probe_rev = blaze_shell::agent_probe::probe_revision();
        let mut last_folder_proc_rev = blaze_shell::sysmon_driver::folder_proc_revision();
        let mut last_folder_poll = Instant::now() - std::time::Duration::from_secs(5);
        let mut command_palette: Option<CommandPalette> = None;
        let mut find_bar: Option<blaze_shell::find::FindBar> = None;
        let mut find_needs_scroll = false;
        let mut last_cursor_rect: Option<[f32; 4]> = None;
        let mut trail_prev: Option<[f32; 4]> = None;
        let mut trail_start: Option<Instant> = None;
        let mut last_running_tick = Instant::now();
        let mut last_anim_tick = Instant::now();
        let mut has_animated_images = false;
        let anim_epoch = Instant::now();
        let mut pending_drag: Option<PendingDrag> = None;
        let mut selecting_composer = false;
        let mut selecting_output = false;
        let mut output_selection: Option<TextSelection> = None;
        let mut output_nav: Option<OutputNav> = None;
        let mut active_popover: Option<ActivePopover> = None;
        let mut popup_engaged = false;
        let mut zoom_changed = false;
        let mut cur_cursor: Option<CursorKind> = None;
        let mut last_mouse: Option<(f32, f32)> = None;
        let mut last_hovered_link: Option<String> = None;
        let mut was_hidden = false;
        let mut last_pty_sync = Instant::now();
        let mut sidebar_open = false;

        loop {
            let mut quit = false;
            autoreleasepool(|_| {
                // SAFETY: `NSDefaultRunLoopMode` is a valid Foundation static string.
                let mode = unsafe { NSDefaultRunLoopMode };
                let until = NSDate::dateWithTimeIntervalSinceNow(if dirty { 0.0 } else { 0.008 });
                let mut events: Vec<Retained<NSEvent>> = Vec::with_capacity(8);
                {
                    let mut on_resize = |rev: ResizeEvent| {
                        handle_resize_event(
                            rev,
                            &window,
                            &layer,
                            &renderer,
                            &wm,
                            &mut panes,
                            &mut fonts,
                            &mut icon_atlas,
                            &mut image_atlas,
                            &mut size,
                            &mut scale,
                            font_points,
                            anim_epoch,
                            &mut has_animated_images,
                            &mut tile_layout,
                            &mut composite_scene,
                            &mut pty_sizes_dirty,
                            &mut dirty,
                            &mut last_pty_sync,
                            sidebar_open,
                        );
                    };
                    with_resize_hook(&mut on_resize, || {
                        if let Some(first) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                            NSEventMask::Any,
                            Some(&until),
                            mode,
                            true,
                        ) {
                            events.push(first);
                            while let Some(more) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
                                NSEventMask::Any,
                                None,
                                mode,
                                true,
                            ) {
                                events.push(more);
                            }
                        }
                    });
                }
                for ev in events {
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

                            // Native macOS application & window shortcuts ALWAYS win (never swallowed
                            // by the composer, Command Palette, Yazi, or active PTY).
                            if flags.contains(NSEventModifierFlags::Command) {
                                if !ctrl && !alt && !shift && (key == Key::KeyH || lower == "h") {
                                    app.hide(None);
                                    continue;
                                }
                                if !ctrl && !alt && !shift && (key == Key::KeyM || lower == "m") {
                                    window.miniaturize(None);
                                    continue;
                                }
                                if !ctrl && !alt && shift && (key == Key::KeyF || lower == "f") {
                                    if let Some(pane) = panes.get_mut(&wm.focused_pane()) {
                                        if pane.active_sysmon.is_some() {
                                            pane.sysmon_fullscreen = !pane.sysmon_fullscreen;
                                            dirty = true;
                                        } else if pane.active_pty.is_some() {
                                            pane.pty_fullscreen = !pane.pty_fullscreen;
                                            if pane.pty_fullscreen {
                                                pane.set_pty_focus(true);
                                            }
                                            pty_sizes_dirty = true;
                                            dirty = true;
                                        }
                                    }
                                    continue;
                                }
                                if ctrl && !alt && !shift && (key == Key::KeyF || lower == "f") {
                                    window.toggleFullScreen(None);
                                    continue;
                                }
                                if !ctrl && !alt && !shift && (key == Key::KeyQ || lower == "q") {
                                    quit = true;
                                    continue;
                                }
                            }

                            // 0. `Cmd+F` / `Cmd+G` / `Cmd+Shift+G` / `Cmd+E`: native Find in Scrollback & Output!
                            if flags.contains(NSEventModifierFlags::Command) && !ctrl && !alt {
                                if !shift && (key == Key::KeyF || lower == "f") {
                                    let initial = output_selection.and_then(|doc_sel| {
                                        let sy = panes
                                            .get(&wm.focused_pane())
                                            .map_or(0.0, |p| p.scroll_y);
                                        let screen_sel = TextSelection {
                                            anchor: [doc_sel.anchor[0], doc_sel.anchor[1] - sy],
                                            head: [doc_sel.head[0], doc_sel.head[1] - sy],
                                        };
                                        let (_, txt) = compute_text_selection(
                                            &composite_scene.text_runs,
                                            fonts.cell_metrics(scale),
                                            screen_sel,
                                            TOP_BAR_H,
                                        );
                                        let t = txt.trim();
                                        (!t.is_empty() && !t.contains('\n')).then(|| t.to_owned())
                                    });
                                    command_palette = None;
                                    active_popover = None;
                                    if let Some(fb) = &mut find_bar {
                                        if let Some(init) = initial {
                                            *fb = blaze_shell::find::FindBar::new(Some(&init));
                                            find_needs_scroll = true;
                                        }
                                    } else {
                                        find_bar = Some(blaze_shell::find::FindBar::new(initial.as_deref()));
                                        find_needs_scroll = true;
                                    }
                                    dirty = true;
                                    continue;
                                }
                                if !shift && (key == Key::KeyE || lower == "e") {
                                    if let Some(doc_sel) = output_selection {
                                        let sy = panes
                                            .get(&wm.focused_pane())
                                            .map_or(0.0, |p| p.scroll_y);
                                        let screen_sel = TextSelection {
                                            anchor: [doc_sel.anchor[0], doc_sel.anchor[1] - sy],
                                            head: [doc_sel.head[0], doc_sel.head[1] - sy],
                                        };
                                        let (_, txt) = compute_text_selection(
                                            &composite_scene.text_runs,
                                            fonts.cell_metrics(scale),
                                            screen_sel,
                                            TOP_BAR_H,
                                        );
                                        let t = txt.trim();
                                        if !t.is_empty() && !t.contains('\n') {
                                            find_bar = Some(blaze_shell::find::FindBar::new(Some(t)));
                                            find_needs_scroll = true;
                                            dirty = true;
                                            continue;
                                        }
                                    }
                                }
                                if key == Key::KeyG || lower == "g" {
                                    if let Some(fb) = &mut find_bar {
                                        if shift {
                                            fb.prev_match();
                                        } else {
                                            fb.next_match();
                                        }
                                        find_needs_scroll = true;
                                        dirty = true;
                                        continue;
                                    }
                                }
                            }

                            // 0a. Route keys into the floating `Cmd+F` Find Bar when open.
                            if let Some(fb) = &mut find_bar {
                                if key == Key::Escape {
                                    find_bar = None;
                                    dirty = true;
                                    continue;
                                }
                                if !flags.contains(NSEventModifierFlags::Command)
                                    || key == Key::Backspace
                                    || key == Key::KeyV
                                    || key == Key::ArrowLeft
                                    || key == Key::ArrowRight
                                {
                                    if matches!(key, Key::Enter | Key::NumpadEnter) {
                                        if shift {
                                            fb.prev_match();
                                        } else {
                                            fb.next_match();
                                        }
                                        find_needs_scroll = true;
                                        dirty = true;
                                        continue;
                                    } else if key == Key::Backspace {
                                        if flags.contains(NSEventModifierFlags::Command) {
                                            fb.kill_to_start();
                                        } else if alt {
                                            fb.delete_word_back();
                                        } else {
                                            fb.backspace();
                                        }
                                        find_needs_scroll = true;
                                        dirty = true;
                                        continue;
                                    } else if key == Key::Delete {
                                        fb.delete_forward();
                                        find_needs_scroll = true;
                                        dirty = true;
                                        continue;
                                    } else if key == Key::ArrowLeft {
                                        if flags.contains(NSEventModifierFlags::Command) {
                                            fb.move_home();
                                        } else {
                                            fb.move_left();
                                        }
                                        dirty = true;
                                        continue;
                                    } else if key == Key::ArrowRight {
                                        if flags.contains(NSEventModifierFlags::Command) {
                                            fb.move_end();
                                        } else {
                                            fb.move_right();
                                        }
                                        dirty = true;
                                        continue;
                                    } else if key == Key::Home {
                                        fb.move_home();
                                        dirty = true;
                                        continue;
                                    } else if key == Key::End {
                                        fb.move_end();
                                        dirty = true;
                                        continue;
                                    } else if alt && !ctrl && !flags.contains(NSEventModifierFlags::Command) && (key == Key::KeyC || lower == "c") {
                                        fb.toggle_case();
                                        find_needs_scroll = true;
                                        dirty = true;
                                        continue;
                                    } else if flags.contains(NSEventModifierFlags::Command) && key == Key::KeyV {
                                        let pb = NSPasteboard::generalPasteboard();
                                        // SAFETY: valid static AppKit pasteboard type.
                                        let t = unsafe { NSPasteboardTypeString };
                                        if let Some(s) = pb.stringForType(t) {
                                            fb.insert_str(&s.to_string());
                                            find_needs_scroll = true;
                                            dirty = true;
                                        }
                                        continue;
                                    } else if !flags.contains(NSEventModifierFlags::Command) && !ctrl && !alt {
                                        let text = keys::text_of(&ev);
                                        if !text.is_empty() && !text.chars().all(|c| c.is_control()) {
                                            fb.insert_str(&text);
                                            find_needs_scroll = true;
                                            dirty = true;
                                            continue;
                                        }
                                    }
                                }
                            }

                            // 0b. `Cmd+Shift+P`: toggle Command Palette, and route keys into it when open.
                            if flags.contains(NSEventModifierFlags::Command) && shift && !ctrl && !alt && key == Key::KeyP {
                                command_palette = if command_palette.is_some() {
                                    None
                                } else {
                                    active_popover = None;
                                    Some(CommandPalette::open())
                                };
                                dirty = true;
                                continue;
                            }
                            if let Some(pal) = &mut command_palette {
                                if key == Key::Escape {
                                    command_palette = None;
                                } else if matches!(key, Key::Enter | Key::NumpadEnter) {
                                    if let Some(act) = pal.selected_action() {
                                        command_palette = None;
                                        run_palette_action(
                                            act,
                                            &mut wm,
                                            &tile_layout,
                                            &mut panes,
                                            &mut font_points,
                                            &mut zoom_changed,
                                            &mut pty_sizes_dirty,
                                            &mut sidebar_open,
                                            &mut quit,
                                        );
                                    } else {
                                        command_palette = None;
                                    }
                                } else if key == Key::ArrowUp || (ctrl && key == Key::KeyP) {
                                    pal.move_up();
                                } else if key == Key::ArrowDown || (ctrl && key == Key::KeyN) {
                                    pal.move_down();
                                } else if key == Key::Backspace {
                                    pal.backspace();
                                } else if !flags.contains(NSEventModifierFlags::Command) && !ctrl && !alt {
                                    let text = keys::text_of(&ev);
                                    for ch in text.chars() {
                                        if !ch.is_control() {
                                            pal.push_char(ch);
                                        }
                                    }
                                }
                                dirty = true;
                                continue;
                            }

                            // Global tiling & workspace shortcuts (`Cmd+N`, `Cmd+T`, `Cmd+1..9`, `Cmd+D`,
                            // `Cmd+Shift+D`, `Cmd+W`, `Cmd+Opt+Arrow/HJKL`, `Cmd+[/]`, `Cmd+Shift+[/]`,
                            // `Cmd+Shift+Enter` zoom, `Cmd+Ctrl+Arrow` resize, `Ctrl+Tab` cycle tab).
                            if ctrl && !flags.contains(NSEventModifierFlags::Command) && key == Key::Tab {
                                let delta = if shift { -1 } else { 1 };
                                if wm.cycle_tab(delta).is_some() {
                                    active_popover = None;
                                    output_selection = None;
                                    output_nav = None;
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                }
                                continue;
                            }

                            if flags.contains(NSEventModifierFlags::Command) {
                                // 1. `Cmd+N`: auto-tile a new pane internally along the focused pane's
                                //    longer axis (BSP spiral); `Cmd+Shift+N` spawns a separate OS window.
                                if key == Key::KeyN && !ctrl && !alt {
                                    let cwd = panes
                                        .get(&wm.focused_pane())
                                        .map(|p| p.cwd().to_owned())
                                        .unwrap_or_else(|| "/".to_owned());
                                    if shift {
                                        spawn_new_window(&cwd);
                                    } else {
                                        let new_pid = wm.split_focused(auto_split_axis(&tile_layout));
                                        panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                        active_popover = None;
                                        output_selection = None;
                                        output_nav = None;
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                    }
                                    continue;
                                }
                                // 2. `Cmd+S`: toggle collapsible left workspace & fork-hierarchy sidebar.
                                if (key == Key::KeyS || lower == "s") && !ctrl && !alt && !shift {
                                    sidebar_open = !sidebar_open;
                                    active_popover = None;
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                    continue;
                                }
                                // 2b. `Cmd+T`: new workspace tab inheriting focused pane's cwd.
                                if key == Key::KeyT && !ctrl && !alt {
                                    let cwd = panes
                                        .get(&wm.focused_pane())
                                        .map(|p| p.cwd().to_owned())
                                        .unwrap_or_else(|| "/".to_owned());
                                    let (_, new_pid) = wm.new_tab();
                                    panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                    active_popover = None;
                                    output_selection = None;
                                    output_nav = None;
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                    continue;
                                }
                                // 3. `Cmd+1..9`: switch directly to workspace tab # (9 = last tab).
                                let tab_slot = match key {
                                    Key::Digit1 => Some(1),
                                    Key::Digit2 => Some(2),
                                    Key::Digit3 => Some(3),
                                    Key::Digit4 => Some(4),
                                    Key::Digit5 => Some(5),
                                    Key::Digit6 => Some(6),
                                    Key::Digit7 => Some(7),
                                    Key::Digit8 => Some(8),
                                    Key::Digit9 => Some(9),
                                    _ => None,
                                };
                                if let Some(slot) = tab_slot && !ctrl && !alt && !shift {
                                    if wm.select_tab_slot(slot).is_some() {
                                        active_popover = None;
                                        output_selection = None;
                                        output_nav = None;
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                    }
                                    continue;
                                }
                                // 4. `Cmd+D` (vertical side-by-side) / `Cmd+Shift+D` (horizontal top/bottom).
                                if key == Key::KeyD && !ctrl && !alt {
                                    let axis = if shift {
                                        SplitAxis::Horizontal
                                    } else {
                                        SplitAxis::Vertical
                                    };
                                    let cwd = panes
                                        .get(&wm.focused_pane())
                                        .map(|p| p.cwd().to_owned())
                                        .unwrap_or_else(|| "/".to_owned());
                                    let new_pid = wm.split_focused(axis);
                                    panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                    active_popover = None;
                                    output_selection = None;
                                    output_nav = None;
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                    continue;
                                }
                                // 5. `Cmd+W`: close focused pane -> collapse split -> close tab -> quit if last.
                                if key == Key::KeyW && !ctrl && !alt {
                                    match wm.close_focused() {
                                        CloseOutcome::PaneClosed { closed, .. } => {
                                            if let Some(mut p) = panes.remove(&closed)
                                                && let Some(ap) = p.active_pty.take()
                                            {
                                                ap.pty.hangup();
                                            }
                                            active_popover = None;
                                            output_selection = None;
                                            output_nav = None;
                                            pty_sizes_dirty = true;
                                            dirty = true;
                                        }
                                        CloseOutcome::TabClosed { closed_panes, .. } => {
                                            for pid in closed_panes {
                                                if let Some(mut p) = panes.remove(&pid)
                                                    && let Some(ap) = p.active_pty.take()
                                                {
                                                    ap.pty.hangup();
                                                }
                                            }
                                            active_popover = None;
                                            output_selection = None;
                                            output_nav = None;
                                            pty_sizes_dirty = true;
                                            dirty = true;
                                        }
                                        CloseOutcome::LastPaneInLastTab => {
                                            quit = true;
                                        }
                                    }
                                    continue;
                                }
                                // 6. `Cmd+Shift+Enter`: zoom / unzoom focused pane.
                                if shift && !ctrl && !alt && matches!(key, Key::Enter | Key::NumpadEnter) {
                                    if wm.toggle_zoom() {
                                        active_popover = None;
                                        output_selection = None;
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                    }
                                    continue;
                                }
                                // 7. `Cmd+Opt+Arrow` or `Cmd+Opt+H/J/K/L`: spatial directional pane focus.
                                if alt && !ctrl {
                                    let dir = match key {
                                        Key::ArrowLeft | Key::KeyH => Some(FocusDir::Left),
                                        Key::ArrowRight | Key::KeyL => Some(FocusDir::Right),
                                        Key::ArrowUp | Key::KeyK => Some(FocusDir::Up),
                                        Key::ArrowDown | Key::KeyJ => Some(FocusDir::Down),
                                        _ => None,
                                    };
                                    if let Some(d) = dir {
                                        let area = HitRect::new(
                                            0.0,
                                            TOP_BAR_H,
                                            size.width as f32,
                                            size.height as f32,
                                        );
                                        if wm.focus_direction(d, area.into(), TILE_GAP).is_some() {
                                            active_popover = None;
                                            output_selection = None;
                                            output_nav = None;
                                            dirty = true;
                                        }
                                        continue;
                                    }
                                }
                                // 8. `Cmd+Ctrl+Arrow` (resize split) & `Cmd+Ctrl+=` (equalize splits).
                                if ctrl && !alt {
                                    let dir = match key {
                                        Key::ArrowLeft | Key::KeyH => Some(FocusDir::Left),
                                        Key::ArrowRight | Key::KeyL => Some(FocusDir::Right),
                                        Key::ArrowUp | Key::KeyK => Some(FocusDir::Up),
                                        Key::ArrowDown | Key::KeyJ => Some(FocusDir::Down),
                                        _ => None,
                                    };
                                    if let Some(d) = dir {
                                        if wm.nudge_focused_split(d, 0.05) {
                                            pty_sizes_dirty = true;
                                            dirty = true;
                                        }
                                        continue;
                                    }
                                    if key == Key::Equal {
                                        wm.equalize_splits();
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                        continue;
                                    }
                                }
                                // 9. `Cmd+Shift+[` / `Cmd+Shift+]` (cycle tabs) & `Cmd+[` / `Cmd+]` (cycle panes).
                                if !ctrl && !alt && matches!(key, Key::BracketLeft | Key::BracketRight) {
                                    let delta = if key == Key::BracketLeft { -1 } else { 1 };
                                    let changed = if shift {
                                        wm.cycle_tab(delta).is_some()
                                    } else {
                                        wm.cycle_pane(delta).is_some()
                                    };
                                    if changed {
                                        active_popover = None;
                                        output_selection = None;
                                        output_nav = None;
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                    }
                                    continue;
                                }
                                // 10. `Cmd+Shift+E`: equalize splits cleanly.
                                if shift && !ctrl && !alt && key == Key::KeyE {
                                    wm.equalize_splits();
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                    continue;
                                }

                                // Font zoom (`Cmd+=`/`Cmd++`, `Cmd+-`, `Cmd+0`) ALWAYS zooms text size
                                // (including inside an inline alternate-screen PTY and in split tabs).
                                // Optional box/pane resize lives on `Cmd+Opt+=`/`-`/`0`.
                                let resize_dir: Option<i8> = if matches!(lower.as_str(), "=" | "+")
                                    || matches!(key, Key::Equal | Key::NumpadAdd)
                                {
                                    Some(1)
                                } else if lower == "-" || matches!(key, Key::Minus | Key::NumpadSubtract) {
                                    Some(-1)
                                } else if lower == "0" || matches!(key, Key::Digit0 | Key::Numpad0) {
                                    Some(0)
                                } else {
                                    None
                                };
                                if let Some(dir) = resize_dir {
                                    let focused_pid = wm.focused_pane();
                                    let inline_pty_resize = alt
                                        && panes.get(&focused_pid).is_some_and(|p| {
                                            p.active_pty.is_some() && !p.pty_fullscreen && p.pty_focused
                                        });
                                    let split_pane_resize = alt
                                        && wm.active_tab().root.leaf_count() > 1
                                        && !wm.active_tab().zoomed;

                                    if inline_pty_resize {
                                        if let Some(pane) = panes.get_mut(&focused_pid) {
                                            let pane_rect: HitRect = tile_layout
                                                .panes
                                                .iter()
                                                .find(|pl| pl.pane == focused_pid)
                                                .map(|pl| pl.rect.into())
                                                .unwrap_or_else(|| {
                                                    HitRect::new(
                                                        0.0,
                                                        TOP_BAR_H,
                                                        size.width as f32,
                                                        size.height as f32,
                                                    )
                                                });
                                            let (_, raw_rows) = grid_for_rect(pane_rect, &fonts, scale);
                                            let max_inline = raw_rows.saturating_sub(3).max(12);
                                            let default_rows =
                                                (raw_rows.saturating_mul(65) / 100).max(12).min(max_inline);
                                            let cur = pane.pty_inline_rows_override.unwrap_or(default_rows);
                                            pane.pty_inline_rows_override = match dir {
                                                1 => Some(cur.saturating_add(5).min(max_inline)),
                                                -1 => Some(cur.saturating_sub(5).max(8)),
                                                _ => None,
                                            };
                                            pty_sizes_dirty = true;
                                            dirty = true;
                                        }
                                    } else if split_pane_resize {
                                        match dir {
                                            1 => {
                                                wm.resize_focused_pane(0.06);
                                            }
                                            -1 => {
                                                wm.resize_focused_pane(-0.06);
                                            }
                                            _ => {
                                                wm.equalize_splits();
                                            }
                                        }
                                        pty_sizes_dirty = true;
                                        dirty = true;
                                    } else {
                                        match dir {
                                            1 => {
                                                font_points =
                                                    (font_points + ZOOM_STEP_POINTS).min(MAX_FONT_POINTS);
                                            }
                                            -1 => {
                                                font_points =
                                                    (font_points - ZOOM_STEP_POINTS).max(MIN_FONT_POINTS);
                                            }
                                            _ => {
                                                font_points = FONT_POINTS;
                                            }
                                        }
                                        zoom_changed = true;
                                        dirty = true;
                                    }
                                    continue;
                                }
                                if lower == "q" {
                                    quit = true;
                                    continue;
                                }
                            }

                            let shell = panes
                                .get_mut(&wm.focused_pane())
                                .expect("focused pane must exist");

                            // Per-pane macOS Command shortcuts (`Cmd+Escape`, `Cmd+K/B/A/C/X/V/Z` & line navigation).
                            if flags.contains(NSEventModifierFlags::Command) {
                                // `Cmd+Escape`: pop keyboard focus out of an inline PTY into the shell composer.
                                if !ctrl
                                    && !alt
                                    && !shift
                                    && key == Key::Escape
                                    && shell.active_pty.is_some()
                                    && !shell.pty_fullscreen
                                    && shell.pty_focused
                                {
                                    shell.set_pty_focus(false);
                                    dirty = true;
                                    continue;
                                }
                                // Navigation & editing inside the composer when no PTY accepts input.
                                if !shell.pty_accepts_input() {
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
                                } else if let Some(ap) = &shell.active_pty {
                                    // Standard macOS terminal Command line-editing shortcuts inside PTYs
                                    // (Claude Code, zsh, vim, etc.): Cmd+Backspace -> Ctrl+U,
                                    // Cmd+Left -> Ctrl+A, Cmd+Right -> Ctrl+E.
                                    match key {
                                        Key::Backspace => {
                                            ap.pty.write_all(b"\x15");
                                            continue;
                                        }
                                        Key::ArrowLeft => {
                                            ap.pty.write_all(b"\x01");
                                            continue;
                                        }
                                        Key::ArrowRight => {
                                            ap.pty.write_all(b"\x05");
                                            continue;
                                        }
                                        _ => {}
                                    }
                                }

                                match lower.as_str() {
                                    "k" if !shell.pty_fullscreen => {
                                        shell.store.clear_active_branch();
                                        shell.scroll_y = 0.0;
                                        shell.auto_scroll_bottom = true;
                                        output_selection = None;
                                        dirty = true;
                                        continue;
                                    }
                                    "b" if !shell.pty_accepts_input() => {
                                        let _ = shell.store.fork_active_branch(None);
                                        shell.refresh_prompt(shell.prompt.last_exit, shell.prompt.last_duration_ms);
                                        dirty = true;
                                        continue;
                                    }
                                    "z" if !shell.pty_accepts_input() => {
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
                                    "a" if !shell.pty_accepts_input() => {
                                        output_selection = None;
                                        shell.editor.select_all();
                                        dirty = true;
                                        continue;
                                    }
                                    "x" if !shell.pty_accepts_input() => {
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
                                                &composite_scene.text_runs,
                                                fonts.cell_metrics(scale),
                                                screen_sel,
                                                TOP_BAR_H,
                                            );
                                            if !selected.is_empty() {
                                                copy_to_pasteboard(&selected);
                                                continue;
                                            }
                                        }
                                        if !shell.pty_accepts_input() {
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
                                            if shell.pty_accepts_input()
                                                && let Some(ap) = &shell.active_pty
                                            {
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
                                    _ => {
                                        // Forward unhandled `Cmd+*` shortcuts (e.g. `Cmd+\``,
                                        // `Cmd+Ctrl+Space`, menu/window manager keys) to AppKit
                                        // instead of swallowing them or leaking stray characters
                                        // into the composer, Yazi, or PTY.
                                        app.sendEvent(&ev);
                                        continue;
                                    }
                                }
                            }

                            // 0a-sysmon. Native Activity & GPU Monitor (`top` / `btop` / `sysmon`) keyboard handling.
                            if let Some(sid) = shell.active_sysmon {
                                let vis_rows = if shell.sysmon_fullscreen {
                                    blaze_shell::sysmon_view::visible_rows_for_height(
                                        Some((size.height as f32 - TOP_BAR_H - 12.0).max(200.0)),
                                        true,
                                        false,
                                    )
                                } else {
                                    blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS
                                };
                                let mut close_sysmon = false;
                                if let Some(blk) = shell.store.block_mut(sid)
                                    && let BlockBody::SysMon(sm) = &mut blk.body
                                    && sm.running
                                {
                                    let handled = if sm.confirm_kill.is_some() {
                                        match key {
                                            Key::Escape => {
                                                sm.cancel_kill();
                                                true
                                            }
                                            Key::Enter | Key::NumpadEnter => {
                                                sm.confirm_kill_selected(false);
                                                true
                                            }
                                            _ => {
                                                let txt = keys::text_of(&ev);
                                                if txt == "y" || txt == "Y" {
                                                    sm.confirm_kill_selected(false);
                                                } else if txt == "K" {
                                                    sm.confirm_kill_selected(true);
                                                } else {
                                                    sm.cancel_kill();
                                                }
                                                true
                                            }
                                        }
                                    } else if sm.filter_focused {
                                        match key {
                                            Key::Escape => {
                                                sm.filter_focused = false;
                                                sm.clear_filter();
                                                true
                                            }
                                            Key::Enter | Key::NumpadEnter => {
                                                sm.filter_focused = false;
                                                true
                                            }
                                            Key::Backspace => {
                                                if flags.contains(NSEventModifierFlags::Command) {
                                                    sm.clear_filter();
                                                } else {
                                                    sm.pop_filter_char();
                                                }
                                                true
                                            }
                                            Key::ArrowUp => {
                                                sm.move_selection(-1, vis_rows);
                                                true
                                            }
                                            Key::ArrowDown => {
                                                sm.move_selection(1, vis_rows);
                                                true
                                            }
                                            _ if !flags.contains(NSEventModifierFlags::Command)
                                                && !ctrl
                                                && !alt =>
                                            {
                                                let txt = keys::text_of(&ev);
                                                let mut any = false;
                                                for ch in txt.chars() {
                                                    if !ch.is_control() {
                                                        sm.push_filter_char(ch);
                                                        any = true;
                                                    }
                                                }
                                                any
                                            }
                                            _ => false,
                                        }
                                    } else if !flags.contains(NSEventModifierFlags::Command) && !alt {
                                        let txt = keys::text_of(&ev);
                                        match key {
                                            Key::Escape => {
                                                if !sm.filter_input.is_empty() {
                                                    sm.clear_filter();
                                                } else {
                                                    sm.running = false;
                                                    close_sysmon = true;
                                                }
                                                true
                                            }
                                            Key::ArrowUp => {
                                                sm.move_selection(-1, vis_rows);
                                                true
                                            }
                                            Key::ArrowDown => {
                                                sm.move_selection(1, vis_rows);
                                                true
                                            }
                                            Key::ArrowLeft => {
                                                sm.collapse_or_parent(vis_rows);
                                                true
                                            }
                                            Key::ArrowRight => {
                                                sm.expand_or_child(vis_rows);
                                                true
                                            }
                                            Key::PageUp => {
                                                sm.move_selection(-(vis_rows as isize), vis_rows);
                                                true
                                            }
                                            Key::PageDown => {
                                                sm.move_selection(vis_rows as isize, vis_rows);
                                                true
                                            }
                                            Key::Space | Key::Enter | Key::NumpadEnter => {
                                                sm.toggle_selected_collapse();
                                                true
                                            }
                                            _ if ctrl && key == Key::KeyC => {
                                                sm.running = false;
                                                close_sysmon = true;
                                                true
                                            }
                                            _ if !ctrl => match txt.as_str() {
                                                "q" => {
                                                    sm.running = false;
                                                    close_sysmon = true;
                                                    true
                                                }
                                                "j" => {
                                                    sm.move_selection(1, vis_rows);
                                                    true
                                                }
                                                "k" => {
                                                    sm.move_selection(-1, vis_rows);
                                                    true
                                                }
                                                "h" => {
                                                    sm.collapse_or_parent(vis_rows);
                                                    true
                                                }
                                                "l" => {
                                                    sm.expand_or_child(vis_rows);
                                                    true
                                                }
                                                "t" => {
                                                    sm.engine.toggle_tree_mode();
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "c" => {
                                                    sm.cumulative = !sm.cumulative;
                                                    true
                                                }
                                                "p" => {
                                                    sm.engine.toggle_pause();
                                                    true
                                                }
                                                "/" => {
                                                    sm.filter_focused = true;
                                                    true
                                                }
                                                "1" => {
                                                    sm.engine.set_sort(blaze_sysmon::SortColumn::Cpu);
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "2" => {
                                                    sm.engine.set_sort(blaze_sysmon::SortColumn::Gpu);
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "3" => {
                                                    sm.engine.set_sort(blaze_sysmon::SortColumn::Mem);
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "4" => {
                                                    sm.engine.set_sort(blaze_sysmon::SortColumn::Pid);
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "5" => {
                                                    sm.engine.set_sort(blaze_sysmon::SortColumn::Name);
                                                    sm.clamp_selection(vis_rows);
                                                    true
                                                }
                                                "x" | "K" => {
                                                    sm.begin_kill_selected();
                                                    true
                                                }
                                                _ => false,
                                            },
                                            _ => false,
                                        }
                                    } else {
                                        false
                                    };
                                    if close_sysmon {
                                        shell.active_sysmon = None;
                                        shell.sysmon_fullscreen = false;
                                    }
                                    if handled {
                                        dirty = true;
                                        continue;
                                    }
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
                                    Key::PageUp => "PageUp",
                                    Key::PageDown => "PageDown",
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
                                    &composite_scene.text_runs,
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

                            // Forward directly to live PTY when an interactive external command is focused.
                            if shell.pty_accepts_input()
                                && let Some(ap) = &shell.active_pty
                            {
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
                                    let max_y = (composite_scene.input_top_y - 2.0).max(TOP_BAR_H);
                                    let vlines = extract_visual_lines(
                                        &composite_scene.text_runs,
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
                                            shell.editor.suppress_completions();
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
                                        if shift || shell.editor.is_incomplete() {
                                            shell.editor.insert_char('\n');
                                            popup_engaged = false;
                                        } else {
                                            // IntelliJ-style: when a completion item is highlighted in
                                            // the popup, Enter accepts it AND immediately executes!
                                            if popup_engaged && !shell.editor.completions.is_empty() {
                                                shell.editor.apply_selected_completion();
                                            } else {
                                                // Expand trailing command-position abbreviation (`l`, `gs`, `..`).
                                                shell.editor.try_expand_abbreviation();
                                            }
                                            let cmd = shell.editor.take_submitted();
                                            popup_engaged = false;
                                            active_popover = None;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx, true);
                                        }
                                    }
                                    Key::Tab => {
                                        if shift && !shell.editor.completions.is_empty() {
                                            // Shift+Tab cycles backward through the completion popup.
                                            popup_engaged = true;
                                            shell.editor.move_up();
                                        } else if !shell.editor.completions.is_empty() {
                                            // IntelliJ-style: Tab accepts the selected argument into
                                            // the prompt without executing so you can keep typing!
                                            shell.editor.apply_selected_completion();
                                            popup_engaged = false;
                                            shell.refresh_editor_assist(false);
                                        } else if !shift && shell.editor.tab_out() {
                                            shell.refresh_editor_assist(false);
                                        } else {
                                            let st = shell.store.active_state().fork();
                                            let cols = shell.recent_table_columns();
                                            shell.editor.trigger_completion_with_cols(&st, &cols);
                                            popup_engaged = !shell.editor.completions.is_empty();
                                            if shift && popup_engaged {
                                                shell.editor.move_up();
                                            } else if !popup_engaged {
                                                // Auto-completed uniquely or by common prefix (e.g. `/Vo` -> `/Volumes/`):
                                                // refresh inline Fish ghost immediately for the next segment!
                                                shell.refresh_editor_assist(false);
                                            }
                                        }
                                    }
                                    Key::Escape => {
                                        if !shell.editor.completions.is_empty() {
                                            shell.editor.suppress_completions();
                                            popup_engaged = false;
                                        } else {
                                            shell.editor.feed_vim_key(VimKeyEvent::key(VimKeyCode::Escape));
                                        }
                                    }
                                    Key::Backspace => {
                                        let keep_open = !shell.editor.completions.is_empty();
                                        if alt {
                                            shell.editor.kill_word_back();
                                        } else {
                                            shell.editor.backspace();
                                        }
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(keep_open);
                                    }
                                    Key::Delete => {
                                        let keep_open = !shell.editor.completions.is_empty();
                                        if alt {
                                            shell.editor.kill_word_forward();
                                        } else {
                                            shell.editor.delete_forward();
                                        }
                                        popup_engaged = false;
                                        shell.refresh_editor_assist(keep_open);
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
                                            let keep_open = !shell.editor.completions.is_empty() && text != " ";
                                            if text == " " {
                                                // Fish/Nushell inline abbreviation expansion on Space!
                                                shell.editor.try_expand_abbreviation();
                                            }
                                            shell.editor.insert_typed(&text);
                                            popup_engaged = false;
                                            shell.auto_scroll_bottom = true;
                                            // Keep Fish ghost autosuggestions live; if the popup is
                                            // already open, narrow it live as the user keeps typing!
                                            shell.refresh_editor_assist(keep_open);
                                        }
                                    }
                                }
                            }
                            dirty = true;
                            continue;
                        }
                        NSEventType::ScrollWheel => {
                            let pt = ev.locationInWindow();
                            let mx = pt.x as f32;
                            let my = (size.height - pt.y) as f32;
                            let dy = ev.scrollingDeltaY();
                            let step_px = if ev.hasPreciseScrollingDeltas() {
                                dy as f32
                            } else {
                                dy as f32 * 24.0
                            };
                            let rows_delta = (-step_px / 18.0).round() as isize;

                            // 1. Scroll active Command Palette (`Cmd+Shift+P`).
                            if let Some(pal) = &mut command_palette {
                                if pal.scroll_by(rows_delta) {
                                    dirty = true;
                                }
                                continue;
                            }
                            // 2. Scroll open `Ctrl+R` History Palette or Tab Autocomplete popup!
                            if let Some(focused) = panes.get_mut(&wm.focused_pane()) {
                                if focused.editor.reverse_search.is_some() {
                                    // Bottom-up list: scrolling up (dy > 0) reveals older entries (+scroll).
                                    let hist_delta = (step_px / 18.0).round() as isize;
                                    if focused.editor.reverse_search_scroll_by(hist_delta) {
                                        dirty = true;
                                    }
                                    continue;
                                }
                                if !focused.editor.completions.is_empty() {
                                    if focused.editor.completion_scroll_by(rows_delta) {
                                        dirty = true;
                                    }
                                    continue;
                                }
                            }

                            // 3. Scroll whichever tiled pane the pointer is currently hovering over!
                            let hovered_pl = tile_layout
                                .panes
                                .iter()
                                .find(|p| p.rect.contains(mx, my))
                                .copied();
                            let scroll_pid = hovered_pl
                                .map(|p| p.pane)
                                .unwrap_or_else(|| wm.focused_pane());
                            let pane_x0 = hovered_pl.map_or(0.0, |p| p.rect.x0);
                            let pane_w = hovered_pl.map_or(size.width as f32, |p| p.rect.x1 - p.rect.x0);
                            let Some(shell) = panes.get_mut(&scroll_pid) else {
                                continue;
                            };
                            if let Some(yazi) = &mut shell.yazi {
                                if yazi.scroll_at(mx - pane_x0, rows_delta) {
                                    dirty = true;
                                }
                            } else if let Some(sid) = shell.active_sysmon
                                && (shell.sysmon_fullscreen
                                    || matches!(
                                        composite_scene.hit_test(mx, my),
                                        Some(
                                            HitAction::SysMonSelectRow { .. }
                                                | HitAction::SysMonToggleCollapse { .. }
                                                | HitAction::SysMonSort { .. }
                                                | HitAction::SysMonToggleTree(_)
                                                | HitAction::SysMonToggleCumulative(_)
                                                | HitAction::SysMonRequestKill(_)
                                                | HitAction::SysMonTogglePause(_)
                                        )
                                    ))
                                && let Some(blk) = shell.store.block_mut(sid)
                                && let BlockBody::SysMon(sm) = &mut blk.body
                            {
                                let vis_rows = if shell.sysmon_fullscreen {
                                    blaze_shell::sysmon_view::visible_rows_for_height(
                                        Some((size.height as f32 - TOP_BAR_H - 12.0).max(200.0)),
                                        true,
                                        sm.filter_focused || !sm.filter_input.is_empty(),
                                    )
                                } else {
                                    blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS
                                };
                                sm.scroll_by(rows_delta as isize, vis_rows);
                                dirty = true;
                            } else if let Some(hub) = &mut shell.agent_hub {
                                if step_px.abs() > 0.1
                                    && hub.scroll_at(mx - pane_x0, pane_w, -step_px)
                                {
                                    dirty = true;
                                }
                            } else if let Some(ap) = &shell.active_pty
                                && (shell.pty_fullscreen
                                    || (shell.pty_focused
                                        && composite_scene
                                            .inline_pty_rect
                                            .is_some_and(|r| r.contains(mx, my))))
                                && dy.abs() > 0.5
                                && let Some(blk) = shell.store.block(ap.block_id)
                                && let BlockBody::Terminal { term, .. } = &blk.body
                                && (term.flags().mouse_event != blaze_vt::terminal::MouseEvents::None
                                    || shell.store.is_alternate_screen())
                            {
                                let up = dy > 0.0;
                                if term.flags().mouse_event != blaze_vt::terminal::MouseEvents::None {
                                    // Send true SGR / X10 mouse wheel events (`\x1b[<64;col;rowM`)
                                    // whenever the TUI (e.g. Claude Code, vim, htop) enabled mouse
                                    // reporting — never falling back to arrow keys!
                                    let pane_rect: HitRect = hovered_pl
                                        .map(|p| p.rect.into())
                                        .unwrap_or_else(|| {
                                            HitRect::new(0.0, TOP_BAR_H, size.width as f32, size.height as f32)
                                        });
                                    let target_rect = if !shell.pty_fullscreen {
                                        composite_scene.inline_pty_rect.unwrap_or(pane_rect)
                                    } else {
                                        pane_rect
                                    };
                                    let cw = (fonts.metrics.cell_w as f32 / scale as f32).max(1.0);
                                    let ch = (fonts.metrics.cell_h as f32 / scale as f32).max(1.0);
                                    let rel_x = (mx - target_rect.x0 - 10.0).max(0.0);
                                    let rel_y = (my - target_rect.y0 - 6.0).max(0.0);
                                    let cols = term.screen().cols() as u32;
                                    let rows = term.screen().rows() as u32;
                                    let msize = blaze_vt::input::mouse::Size {
                                        screen: blaze_vt::input::mouse::Pixels {
                                            width: (cols as f32 * cw).round().max(1.0) as u32,
                                            height: (rows as f32 * ch).round().max(1.0) as u32,
                                        },
                                        cell: blaze_vt::input::mouse::Pixels {
                                            width: cw.round().max(1.0) as u32,
                                            height: ch.round().max(1.0) as u32,
                                        },
                                        padding: blaze_vt::input::mouse::Padding::default(),
                                    };
                                    let mopts = term.mouse_opts(msize, false);
                                    let mev = blaze_vt::input::mouse::MouseEvent {
                                        action: blaze_vt::input::mouse::MouseAction::Press,
                                        button: Some(if up {
                                            blaze_vt::input::mouse::MouseButton::Four
                                        } else {
                                            blaze_vt::input::mouse::MouseButton::Five
                                        }),
                                        mods: blaze_vt::input::key::Mods::NONE,
                                        pos: blaze_vt::input::mouse::SurfacePos {
                                            x: rel_x,
                                            y: rel_y,
                                        },
                                    };
                                    let mut buf = [0u8; blaze_vt::input::mouse::MAX_LEN];
                                    if let Ok(encoded) =
                                        blaze_vt::input::mouse::encode(&mev, &mopts, None, &mut buf)
                                        && !encoded.is_empty()
                                    {
                                        ap.pty.write_all(encoded);
                                    }
                                } else if shell.store.is_alternate_screen()
                                    && term.mode(blaze_vt::terminal::Mode::MouseAlternateScroll)
                                {
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
                                if let Some(reg) = composite_scene.scroll_region_at(mx, my)
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
                        NSEventType::LeftMouseDown
                            if ev.windowNumber() == window.windowNumber() =>
                        {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            // Always yield native window resize edges/corners (and traffic lights
                            // on unfocused windows) directly to AppKit via `with_resize_hook` below.
                            let in_traffic_lights = y >= 0.0 && y < TOP_BAR_H && x >= 0.0 && x < 76.0;
                            let yield_to_appkit = on_resize_edge(x, y, size.width as f32, size.height as f32)
                                || (!window.isKeyWindow() && in_traffic_lights);
                            if yield_to_appkit {
                                cur_cursor = None;
                            }
                            // Clicking an unfocused window activates it AND immediately focuses the
                            // clicked pane, tab, button, or text cursor in a single click (never
                            // requiring two clicks!).
                            if !yield_to_appkit && !window.isKeyWindow() {
                                #[allow(deprecated)]
                                app.activateIgnoringOtherApps(true);
                                window.makeKeyAndOrderFront(None);
                                dirty = true;
                            }
                            // 0. If the Command Palette is open, handle row clicks or backdrop dismiss!
                            if !yield_to_appkit && let Some(pal) = &command_palette {
                                if let Some(HitAction::SelectPalette(idx)) = composite_scene.hit_test(x, y)
                                    && let Some(act) = pal.action_at(idx)
                                {
                                    command_palette = None;
                                    run_palette_action(
                                        act,
                                        &mut wm,
                                        &tile_layout,
                                        &mut panes,
                                        &mut font_points,
                                        &mut zoom_changed,
                                        &mut pty_sizes_dirty,
                                        &mut sidebar_open,
                                        &mut quit,
                                    );
                                    dirty = true;
                                    continue;
                                }
                                command_palette = None;
                                dirty = true;
                                continue;
                            }
                            // Let macOS AppKit handle native window edge/corner resize drags directly!
                            if !yield_to_appkit {
                                // 1. Grabbing a tiled split divider starts live drag resize!
                                if y >= TOP_BAR_H
                                    && let Some(&sh) = tile_layout
                                        .handles
                                        .iter()
                                        .find(|h| h.hit_rect.contains(x, y))
                                {
                                    dragging_split = Some(sh);
                                    active_popover = None;
                                    apply_cursor(
                                        &mut cur_cursor,
                                        match sh.axis {
                                            SplitAxis::Vertical => CursorKind::ResizeEW,
                                            SplitAxis::Horizontal => CursorKind::ResizeNS,
                                        },
                                    );
                                    continue;
                                }

                                // 2. Unified titlebar clicks (workspace tabs, +, split V/H, new window,
                                //    zoom, branch tabs, fork) & window drag work EVERYWHERE!
                                if y < TOP_BAR_H {
                                    if let Some(action) = composite_scene.hit_test(x, y) {
                                        output_selection = None;
                                        output_nav = None;
                                        active_popover = None;
                                        match action {
                                            HitAction::ToggleSidebar => {
                                                sidebar_open = !sidebar_open;
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                            }
                                            HitAction::SwitchTab(tid) => {
                                                if wm.select_tab_by_id(tid).is_some() {
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                            }
                                            HitAction::NewTab => {
                                                let cwd = panes
                                                    .get(&wm.focused_pane())
                                                    .map(|p| p.cwd().to_owned())
                                                    .unwrap_or_else(|| "/".to_owned());
                                                let (_, new_pid) = wm.new_tab();
                                                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                            }
                                            HitAction::CloseTab(tid) => {
                                                if let CloseOutcome::TabClosed { closed_panes, .. } =
                                                    wm.close_tab(tid)
                                                {
                                                    for pid in closed_panes {
                                                        if let Some(mut p) = panes.remove(&pid)
                                                            && let Some(ap) = p.active_pty.take()
                                                        {
                                                            ap.pty.hangup();
                                                        }
                                                    }
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                            }
                                            HitAction::SplitPane(axis) => {
                                                let cwd = panes
                                                    .get(&wm.focused_pane())
                                                    .map(|p| p.cwd().to_owned())
                                                    .unwrap_or_else(|| "/".to_owned());
                                                let new_pid = wm.split_focused(axis);
                                                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                            }
                                            HitAction::NewWindow => {
                                                let cwd = panes
                                                    .get(&wm.focused_pane())
                                                    .map(|p| p.cwd().to_owned())
                                                    .unwrap_or_else(|| "/".to_owned());
                                                spawn_new_window(&cwd);
                                            }
                                            HitAction::ToggleZoomPane(_) => {
                                                if wm.toggle_zoom() {
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                            }
                                            HitAction::SwitchBranch(bid) => {
                                                if let Some(shell) = panes.get_mut(&wm.focused_pane())
                                                    && shell.store.switch_branch(bid)
                                                {
                                                    shell.yazi = None;
                                                    shell.agent_hub = None;
                                                    shell.scroll_y = 0.0;
                                                    shell.auto_scroll_bottom = true;
                                                    shell.refresh_prompt(0, 0);
                                                    dirty = true;
                                                }
                                            }
                                            HitAction::ForkActiveBranch => {
                                                if let Some(shell) = panes.get_mut(&wm.focused_pane()) {
                                                    let _ = shell.store.fork_active_branch(None);
                                                    shell.refresh_prompt(
                                                        shell.prompt.last_exit,
                                                        shell.prompt.last_duration_ms,
                                                    );
                                                    dirty = true;
                                                }
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

                                // 3. Clicking inside the workspace sidebar or tiled panes handles
                                //    sidebar tab/fork switches, pane chrome buttons, and focus.
                                if y >= TOP_BAR_H {
                                    if let Some(action) = composite_scene.hit_test(x, y) {
                                        match action {
                                            HitAction::ToggleSidebar => {
                                                sidebar_open = !sidebar_open;
                                                active_popover = None;
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                                continue;
                                            }
                                            HitAction::SwitchTab(tid) => {
                                                if wm.select_tab_by_id(tid).is_some() {
                                                    active_popover = None;
                                                    output_selection = None;
                                                    output_nav = None;
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                                continue;
                                            }
                                            HitAction::SwitchTabBranch { tab, branch } => {
                                                let _ = wm.select_tab_by_id(tab);
                                                if let Some(shell) = panes.get_mut(&wm.focused_pane())
                                                    && shell.store.switch_branch(branch)
                                                {
                                                    shell.yazi = None;
                                                    shell.agent_hub = None;
                                                    shell.scroll_y = 0.0;
                                                    shell.auto_scroll_bottom = true;
                                                    shell.refresh_prompt(0, 0);
                                                }
                                                active_popover = None;
                                                output_selection = None;
                                                output_nav = None;
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                                continue;
                                            }
                                            HitAction::NewTab => {
                                                let cwd = panes
                                                    .get(&wm.focused_pane())
                                                    .map(|p| p.cwd().to_owned())
                                                    .unwrap_or_else(|| "/".to_owned());
                                                let (_, new_pid) = wm.new_tab();
                                                panes.insert(new_pid, ShellApp::new_in(Some(&cwd)));
                                                active_popover = None;
                                                output_selection = None;
                                                output_nav = None;
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                                continue;
                                            }
                                            HitAction::CloseTab(tid) => {
                                                if let CloseOutcome::TabClosed { closed_panes, .. } =
                                                    wm.close_tab(tid)
                                                {
                                                    for pid in closed_panes {
                                                        if let Some(mut p) = panes.remove(&pid)
                                                            && let Some(ap) = p.active_pty.take()
                                                        {
                                                            ap.pty.hangup();
                                                        }
                                                    }
                                                    active_popover = None;
                                                    output_selection = None;
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                                continue;
                                            }
                                            HitAction::ForkActiveBranch => {
                                                if let Some(shell) = panes.get_mut(&wm.focused_pane()) {
                                                    let _ = shell.store.fork_active_branch(None);
                                                    shell.refresh_prompt(
                                                        shell.prompt.last_exit,
                                                        shell.prompt.last_duration_ms,
                                                    );
                                                    dirty = true;
                                                }
                                                continue;
                                            }
                                            HitAction::ClosePane(pid) => {
                                                match wm.close_pane(pid) {
                                                    CloseOutcome::PaneClosed { closed, .. } => {
                                                        if let Some(mut p) = panes.remove(&closed)
                                                            && let Some(ap) = p.active_pty.take()
                                                        {
                                                            ap.pty.hangup();
                                                        }
                                                        active_popover = None;
                                                        output_selection = None;
                                                        pty_sizes_dirty = true;
                                                        dirty = true;
                                                    }
                                                    CloseOutcome::TabClosed { closed_panes, .. } => {
                                                        for cid in closed_panes {
                                                            if let Some(mut p) = panes.remove(&cid)
                                                                && let Some(ap) = p.active_pty.take()
                                                            {
                                                                ap.pty.hangup();
                                                            }
                                                        }
                                                        active_popover = None;
                                                        output_selection = None;
                                                        pty_sizes_dirty = true;
                                                        dirty = true;
                                                    }
                                                    CloseOutcome::LastPaneInLastTab => {}
                                                }
                                                continue;
                                            }
                                            HitAction::ToggleZoomPane(pid) => {
                                                let prev_pid = wm.focused_pane();
                                                wm.focus_pane(pid);
                                                if prev_pid != pid
                                                    && let Some(prev) = panes.get_mut(&prev_pid)
                                                {
                                                    prev.set_pty_focus(false);
                                                }
                                                if wm.toggle_zoom() {
                                                    active_popover = None;
                                                    pty_sizes_dirty = true;
                                                    dirty = true;
                                                }
                                                continue;
                                            }
                                            HitAction::FocusPane(pid) => {
                                                let prev_pid = wm.focused_pane();
                                                if wm.focus_pane(pid) {
                                                    if prev_pid != pid
                                                        && let Some(prev) = panes.get_mut(&prev_pid)
                                                    {
                                                        prev.set_pty_focus(false);
                                                    }
                                                    active_popover = None;
                                                    output_selection = None;
                                                    output_nav = None;
                                                    dirty = true;
                                                }
                                                continue;
                                            }
                                            _ => {}
                                        }
                                    }
                                    if sidebar_open
                                        && x < SIDEBAR_W.min(size.width as f32 * 0.34).max(180.0)
                                    {
                                        active_popover = None;
                                        dirty = true;
                                        continue;
                                    }
                                    if let Some(pl) =
                                        tile_layout.panes.iter().find(|p| p.rect.contains(x, y))
                                    {
                                        let prev_pid = wm.focused_pane();
                                        if wm.focus_pane(pl.pane) {
                                            if prev_pid != pl.pane
                                                && let Some(prev) = panes.get_mut(&prev_pid)
                                            {
                                                prev.set_pty_focus(false);
                                            }
                                            active_popover = None;
                                            output_selection = None;
                                            output_nav = None;
                                            dirty = true;
                                        }
                                    }
                                }
                            }
                            let shell = panes
                                .get_mut(&wm.focused_pane())
                                .expect("focused pane must exist");
                            if !yield_to_appkit
                                && shell.agent_hub.is_some()
                            {
                                if let Some(action) = composite_scene.hit_test(x, y) {
                                    match action {
                                        HitAction::ResizeAgentSidebar => {
                                            dragging_agent_sidebar = true;
                                            apply_cursor(&mut cur_cursor, CursorKind::ResizeEW);
                                        }
                                        HitAction::SelectAgent(idx) => {
                                            if let Some(hub) = &mut shell.agent_hub
                                                && hub.select_index(idx)
                                            {
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentToggleOffline => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                hub.toggle_show_offline();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentReprobeServers => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                hub.reprobe_servers();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentAttachSelected => {
                                            if let Some(AgentHubAction::EnterAgent {
                                                host,
                                                is_remote,
                                                cwd,
                                                prompt,
                                                ..
                                            }) = shell
                                                .agent_hub
                                                .as_ref()
                                                .and_then(|hub| hub.attach_selected_action())
                                            {
                                                shell.agent_hub = None;
                                                let (cols, rows) = grid_for(size, &fonts, scale);
                                                let cpx = (
                                                    fonts.metrics.cell_w as u32,
                                                    fonts.metrics.cell_h as u32,
                                                );
                                                shell.launch_agent_session(
                                                    &host, is_remote, &cwd, prompt, cols, rows,
                                                    cpx,
                                                );
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentNewTaskSelected => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                hub.start_compose_new();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentRequestKill => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                hub.begin_kill_selected();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentConfirmKill { sigkill } => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                let _ = hub.confirm_kill_selected(sigkill);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::AgentCancelKill => {
                                            if let Some(hub) = &mut shell.agent_hub {
                                                hub.cancel_kill();
                                                dirty = true;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                continue;
                            }
                            if !yield_to_appkit
                                && !(shell.pty_fullscreen && shell.store.is_alternate_screen())
                                && shell.yazi.is_none()
                                && shell.agent_hub.is_none()
                            {
                                output_nav = None;
                                if let Some(action) = composite_scene.hit_test(x, y) {
                                    output_selection = None;
                                    if !matches!(
                                        action,
                                        HitAction::FocusInlinePty(_) | HitAction::TogglePtyFullscreen
                                    ) && shell.active_pty.is_some()
                                        && !shell.pty_fullscreen
                                        && shell.set_pty_focus(false)
                                    {
                                        dirty = true;
                                    }
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
                                        HitAction::ToggleHiddenCols(block_id) => {
                                            shell.store.toggle_hidden_cols(block_id);
                                            active_popover = None;
                                            dirty = true;
                                        }
                                        HitAction::TogglePtyFullscreen => {
                                            if shell.active_sysmon.is_some() {
                                                shell.sysmon_fullscreen = !shell.sysmon_fullscreen;
                                                dirty = true;
                                            } else if shell.active_pty.is_some() {
                                                shell.pty_fullscreen = !shell.pty_fullscreen;
                                                if shell.pty_fullscreen {
                                                    shell.set_pty_focus(true);
                                                }
                                                pty_sizes_dirty = true;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::FocusInlinePty(_) => {
                                            active_popover = None;
                                            output_selection = None;
                                            shell.set_pty_focus(true);
                                            dirty = true;
                                        }
                                        HitAction::SysMonToggleTree(bid) => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.engine.toggle_tree_mode();
                                                sm.clamp_selection(blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonToggleCumulative(bid) => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.cumulative = !sm.cumulative;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonRequestKill(bid) => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.begin_kill_selected();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonTogglePause(bid) => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.engine.toggle_pause();
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonSort { block: bid, col } => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.engine.set_sort(blaze_sysmon::SortColumn::from_u8(col));
                                                sm.clamp_selection(blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonSelectRow { block: bid, row } => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                let vis = if shell.sysmon_fullscreen {
                                                    blaze_shell::sysmon_view::visible_rows_for_height(
                                                        Some((size.height as f32 - TOP_BAR_H - 12.0).max(200.0)),
                                                        true,
                                                        sm.filter_focused || !sm.filter_input.is_empty(),
                                                    )
                                                } else {
                                                    blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS
                                                };
                                                sm.select_row(row, vis);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonToggleCollapse { block: bid, pid } => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.engine.toggle_collapse(pid);
                                                sm.clamp_selection(blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SysMonClose(bid) => {
                                            if let Some(blk) = shell.store.block_mut(bid)
                                                && let BlockBody::SysMon(sm) = &mut blk.body
                                            {
                                                sm.running = false;
                                            }
                                            if shell.active_sysmon == Some(bid) {
                                                shell.active_sysmon = None;
                                                shell.sysmon_fullscreen = false;
                                            }
                                            dirty = true;
                                        }
                                        HitAction::FocusPaneBlock { pane: target_pane, block: target_block } => {
                                            active_popover = None;
                                            wm.focus_pane_anywhere(target_pane);
                                            if let Some(tp) = panes.get_mut(&target_pane) {
                                                tp.scroll_to_block = Some(target_block);
                                                tp.auto_scroll_bottom = false;
                                            }
                                            dirty = true;
                                        }
                                        HitAction::InspectProcessPid(target_pid) => {
                                            active_popover = None;
                                            let mut sm = blaze_shell::sysmon_view::SysMonState::new();
                                            let snap = blaze_shell::sysmon_driver::sample_snapshot(Alloc::global());
                                            sm.engine.ingest(snap);
                                            sm.set_filter(target_pid.to_string());
                                            if let Some(prev_id) = shell.active_sysmon.take()
                                                && let Some(prev_blk) = shell.store.block_mut(prev_id)
                                                && let BlockBody::SysMon(prev_sm) = &mut prev_blk.body
                                            {
                                                prev_sm.running = false;
                                            }
                                            let st_b = shell.store.active_state().fork();
                                            let bid = shell.store.record_block(
                                                format!("top --pid {target_pid}"),
                                                shell.prompt.clone(),
                                                st_b.fork(),
                                                st_b,
                                                now_ms(),
                                                0,
                                                0,
                                                BlockBody::SysMon(Box::new(sm)),
                                            );
                                            shell.active_sysmon = Some(bid);
                                            shell.sysmon_fullscreen = false;
                                            shell.last_sysmon_tick = Instant::now() - std::time::Duration::from_millis(680);
                                            shell.auto_scroll_bottom = true;
                                            dirty = true;
                                        }
                                        HitAction::KillProcessPid(target_pid) => {
                                            let force_sigkill = ev
                                                .modifierFlags()
                                                .contains(NSEventModifierFlags::Shift);
                                            let _ = blaze_shell::sysmon_driver::send_process_signal(
                                                target_pid,
                                                force_sigkill,
                                            );
                                            blaze_shell::sysmon_driver::notify_folder_proc_killed(
                                                target_pid,
                                            );
                                            // Optimistically remove the killed PID from both the live prompt pill
                                            // and open hover popover so it disappears instantaneously.
                                            let strip_pid = |snap: &mut blaze_shell::sysmon_driver::FolderProcSnapshot| {
                                                snap.items.retain(|it| it.pid != target_pid);
                                                snap.total_cpu_pct = snap.items.iter().map(|it| it.cpu_pct).sum();
                                                snap.total_gpu_pct = snap.items.iter().map(|it| it.gpu_pct).sum();
                                                snap.total_mem_bytes = snap
                                                    .items
                                                    .iter()
                                                    .fold(0u64, |acc, it| acc.saturating_add(it.mem_bytes));
                                                snap.is_hot = snap.total_cpu_pct >= 25.0
                                                    || snap.total_gpu_pct >= 15.0
                                                    || snap.items.iter().any(|it| it.is_hot);
                                                snap.primary_name = snap
                                                    .items
                                                    .first()
                                                    .map(|it| it.name.clone())
                                                    .unwrap_or_default();
                                            };
                                            strip_pid(&mut shell.prompt.folder_procs);
                                            if let Some(pop) = active_popover.as_mut()
                                                && let HoverKind::FolderProcs { summary } = &mut pop.kind
                                            {
                                                strip_pid(summary);
                                                if summary.items.is_empty() {
                                                    active_popover = None;
                                                }
                                            }
                                            dirty = true;
                                        }
                                        HitAction::FindToggleCase => {
                                            if let Some(fb) = &mut find_bar {
                                                fb.toggle_case();
                                                find_needs_scroll = true;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::FindNext => {
                                            if let Some(fb) = &mut find_bar {
                                                fb.next_match();
                                                find_needs_scroll = true;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::FindPrev => {
                                            if let Some(fb) = &mut find_bar {
                                                fb.prev_match();
                                                find_needs_scroll = true;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::FindClose => {
                                            find_bar = None;
                                            dirty = true;
                                        }
                                        HitAction::SortTableColumn { block, col } => {
                                            shell.store.toggle_table_sort(block, col);
                                            active_popover = None;
                                            dirty = true;
                                        }
                                        HitAction::ResizeTableColumn { block, col, col_x0 } => {
                                            dragging_table_col = Some((block, col, col_x0 as f32));
                                            active_popover = None;
                                            apply_cursor(&mut cur_cursor, CursorKind::ResizeEW);
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
                                            if let Some(reg) = composite_scene.hover_at(x, y) {
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
                                        HitAction::InsertLiveHint { replace_start, insert } => {
                                            active_popover = None;
                                            shell.editor.enter_vim_insert();
                                            shell.editor.apply_live_hint(replace_start, &insert);
                                            popup_engaged = false;
                                            shell.auto_scroll_bottom = true;
                                            shell.refresh_editor_assist(false);
                                            dirty = true;
                                        }
                                        HitAction::RunCommand(cmd) => {
                                            active_popover = None;
                                            let (cols, rows) = grid_for(size, &fonts, scale);
                                            let cpx = (
                                                fonts.metrics.cell_w as u32,
                                                fonts.metrics.cell_h as u32,
                                            );
                                            shell.submit_command(&cmd, cols, rows, cpx, true);
                                            dirty = true;
                                        }
                                        HitAction::CopyText(txt) => {
                                            copy_to_pasteboard(&txt);
                                        }
                                        HitAction::ResizeAgentSidebar => {
                                            dragging_agent_sidebar = true;
                                            apply_cursor(&mut cur_cursor, CursorKind::ResizeEW);
                                        }
                                        HitAction::SelectAgent(idx) => {
                                            if let Some(hub) = &mut shell.agent_hub
                                                && idx < hub.agents.len()
                                            {
                                                hub.selected = idx;
                                                dirty = true;
                                            }
                                        }
                                        HitAction::OpenLink { href, base_dir } => {
                                            active_popover = None;
                                            let trimmed = href.trim();
                                            if let Some(frag) = trimmed.strip_prefix('#') {
                                                let needle = frag.to_ascii_lowercase();
                                                if let Some(&(_, ay)) = composite_scene
                                                    .anchors
                                                    .iter()
                                                    .rev()
                                                    .find(|(slug, _)| slug == &needle)
                                                {
                                                    let pane_top = tile_layout
                                                        .panes
                                                        .iter()
                                                        .find(|p| p.focused)
                                                        .map_or(TOP_BAR_H, |p| p.rect.y0);
                                                    shell.scroll_y = (shell.scroll_y
                                                        + (ay - (pane_top + 16.0)))
                                                        .max(0.0);
                                                    shell.auto_scroll_bottom = false;
                                                    shell.scroll_to_block = None;
                                                    dirty = true;
                                                }
                                            } else if trimmed.starts_with("http://")
                                                || trimmed.starts_with("https://")
                                                || trimmed.starts_with("mailto:")
                                            {
                                                let _ = std::process::Command::new("/usr/bin/open")
                                                    .arg(trimmed)
                                                    .spawn();
                                            } else if !trimmed.is_empty() {
                                                let path_part =
                                                    trimmed.split('#').next().unwrap_or(trimmed);
                                                let rel = std::path::Path::new(path_part);
                                                let resolved = if rel.is_absolute() {
                                                    rel.to_path_buf()
                                                } else if !base_dir.is_empty() {
                                                    std::path::Path::new(&base_dir).join(rel)
                                                } else {
                                                    std::path::Path::new(shell.cwd()).join(rel)
                                                };
                                                let disp = resolved
                                                    .strip_prefix(shell.cwd())
                                                    .unwrap_or(&resolved)
                                                    .to_string_lossy()
                                                    .into_owned();
                                                let quoted = if disp.contains(' ') {
                                                    format!("\"{disp}\"")
                                                } else {
                                                    disp
                                                };
                                                let cmd = if resolved.is_dir() {
                                                    format!("ls -l {quoted}")
                                                } else {
                                                    format!("open {quoted}")
                                                };
                                                let (cols, rows) = grid_for(size, &fonts, scale);
                                                let cpx = (
                                                    fonts.metrics.cell_w as u32,
                                                    fonts.metrics.cell_h as u32,
                                                );
                                                shell.submit_command(&cmd, cols, rows, cpx, false);
                                                dirty = true;
                                            }
                                        }
                                        HitAction::SwitchTab(_)
                                        | HitAction::SwitchTabBranch { .. }
                                        | HitAction::ToggleSidebar
                                        | HitAction::NewTab
                                        | HitAction::CloseTab(_)
                                        | HitAction::SplitPane(_)
                                        | HitAction::NewWindow
                                        | HitAction::FocusPane(_)
                                        | HitAction::ClosePane(_)
                                        | HitAction::ToggleZoomPane(_)
                                        | HitAction::SelectPalette(_)
                                        | HitAction::AgentToggleOffline
                                        | HitAction::AgentReprobeServers
                                        | HitAction::AgentAttachSelected
                                        | HitAction::AgentNewTaskSelected
                                        | HitAction::AgentRequestKill
                                        | HitAction::AgentConfirmKill { .. }
                                        | HitAction::AgentCancelKill => {}
                                    }
                                    continue;
                                }
                                if active_popover.is_some() {
                                    active_popover = None;
                                    dirty = true;
                                }
                                if shell.active_pty.is_some()
                                    && !shell.pty_fullscreen
                                    && !composite_scene
                                        .inline_pty_rect
                                        .is_some_and(|r| r.contains(x, y))
                                    && shell.set_pty_focus(false)
                                {
                                    dirty = true;
                                }

                                // Click inside the inline composer: enter Insert mode and position/select cursor.
                                if composite_scene.composer_rect.contains(x, y) {
                                    output_selection = None;
                                    shell.editor.enter_vim_insert();
                                    let m = fonts.cell_metrics(scale);
                                    let [ox, oy] = composite_scene.composer_text_origin;
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
                                if let Some(payload) = composite_scene.drag_source_at(x, y).cloned() {
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
                                            &composite_scene.text_runs,
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
                            let shell = panes
                                .get(&wm.focused_pane())
                                .expect("focused pane must exist");
                            if in_window && !in_traffic_lights && !near_resize_edge {
                                // Smooth native hover popover tracking:
                                // - For interactive prompt menus (branches/dir/changes), keep alive while
                                //   pointer travels from anchor into the menu.
                                // - For read-only RHS file/folder previews, ONLY keep alive while still over
                                //   the hovered row so gliding up & down switches previews instantaneously!
                                let in_open_popover = active_popover.as_ref().is_some_and(|p| {
                                    if p.anchor.contains(x, y) {
                                        return true;
                                    }
                                    // Read-only tooltips and file/folder previews never use a sticky bridge:
                                    // moving off the anchor immediately switches or dismisses!
                                    if matches!(
                                        p.kind,
                                        HoverKind::FilePreview { .. }
                                            | HoverKind::DirPreview { .. }
                                            | HoverKind::FileEntry { .. }
                                            | HoverKind::CommandToken { .. }
                                            | HoverKind::CommandExplain { .. }
                                            | HoverKind::Tooltip { .. }
                                    ) {
                                        return false;
                                    }
                                    if let Some(pr) = composite_scene.popover_rect {
                                        if pr.contains(x, y) {
                                            return true;
                                        }
                                        // Narrow vertical travel bridge strictly between `p.anchor` and `pr`
                                        // (never swallowing adjacent sibling pills like `[R]` or `/src`!).
                                        let gap_x0 = p.anchor.x0 - 2.0;
                                        let gap_x1 = p.anchor.x1 + 2.0;
                                        let gap_y0 = p.anchor.y1.min(pr.y1);
                                        let gap_y1 = p.anchor.y0.max(pr.y0);
                                        if x >= gap_x0 && x <= gap_x1 && y >= gap_y0 && y <= gap_y1 {
                                            let over_other_anchor = composite_scene
                                                .hover_at(x, y)
                                                .is_some_and(|r| r.rect != p.anchor);
                                            if !over_other_anchor {
                                                return true;
                                            }
                                        }
                                    }
                                    false
                                });
                                if in_open_popover {
                                    dirty = true;
                                } else if !(shell.pty_fullscreen && shell.store.is_alternate_screen())
                                    && shell.yazi.is_none()
                                    && shell.agent_hub.is_none()
                                    && shell.editor.reverse_search.is_none()
                                    && shell.editor.completions.is_empty()
                                    && !selecting_composer
                                    && !selecting_output
                                    && pending_drag.is_none()
                                    && dragging_split.is_none()
                                    && let Some(reg) = composite_scene.hover_at(x, y)
                                {
                                    if active_popover.as_ref().is_none_or(|cur| cur.anchor != reg.rect) {
                                        let hydrated_kind = match &reg.kind {
                                            HoverKind::FileEntry { full_path, name, icon, .. } => {
                                                Some(load_hover_preview(
                                                    std::path::Path::new(full_path),
                                                    name,
                                                    *icon,
                                                    16,
                                                ))
                                            }
                                            HoverKind::CommandToken { source, token_start, token_end } => {
                                                let path_env = shell
                                                    .store
                                                    .active_state()
                                                    .get_env("PATH")
                                                    .unwrap_or("");
                                                hydrate_command_explain(
                                                    source,
                                                    *token_start,
                                                    *token_end,
                                                    path_env,
                                                    Some(&shell.home),
                                                )
                                            }
                                            other => Some(other.clone()),
                                        };
                                        if let Some(kind) = hydrated_kind {
                                            active_popover = Some(ActivePopover {
                                                anchor: reg.rect,
                                                kind,
                                                pinned: false,
                                            });
                                        } else {
                                            active_popover = None;
                                        }
                                        dirty = true;
                                    }
                                } else if active_popover.as_ref().is_some_and(|p| !p.pinned) {
                                    active_popover = None;
                                    dirty = true;
                                }

                                if sidebar_open
                                    && y >= TOP_BAR_H
                                    && x < SIDEBAR_W.min(size.width as f32 * 0.34).max(180.0)
                                {
                                    dirty = true;
                                }
                                let hovered_link = match composite_scene.hit_test(x, y) {
                                    Some(HitAction::OpenLink { href, .. }) => Some(href),
                                    _ => None,
                                };
                                if hovered_link != last_hovered_link {
                                    last_hovered_link = hovered_link;
                                    dirty = true;
                                }

                                // Keep AppKit's `NSThemeFrame` tracking areas in sync on every
                                // `MouseMoved` so native resize-border cursors never linger inset
                                // inside the window, then apply our content cursor on top.
                                app.sendEvent(&ev);
                                let next = cursor_for_point(
                                    &composite_scene,
                                    &tile_layout.handles,
                                    dragging_split,
                                    shell.pty_fullscreen,
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
                            if near_resize_edge || !in_window {
                                // Yield cursor ownership completely to AppKit on the resize border
                                // and outside the window so the first pixel back inside resets it.
                                cur_cursor = None;
                                if !in_window {
                                    last_mouse = None;
                                }
                            } else {
                                // Over traffic lights (inside window, not on resize edge): let
                                // AppKit highlight the buttons and ensure the cursor resets to Arrow.
                                app.sendEvent(&ev);
                                apply_cursor(&mut cur_cursor, CursorKind::Arrow);
                                continue;
                            }
                        }
                        NSEventType::MouseExited => {
                            cur_cursor = None;
                            last_mouse = None;
                        }
                        NSEventType::LeftMouseDragged if window.isKeyWindow() => {
                            let pt = ev.locationInWindow();
                            let x = pt.x as f32;
                            let y = (size.height - pt.y) as f32;
                            last_mouse = Some((x, y));
                            if let Some(sh) = dragging_split {
                                let ratio = match sh.axis {
                                    SplitAxis::Vertical => {
                                        let w = (sh.parent_rect.x1 - sh.parent_rect.x0).max(1.0);
                                        (x - sh.parent_rect.x0) / w
                                    }
                                    SplitAxis::Horizontal => {
                                        let h = (sh.parent_rect.y1 - sh.parent_rect.y0).max(1.0);
                                        (y - sh.parent_rect.y0) / h
                                    }
                                };
                                if wm.set_split_ratio(sh.split, ratio) {
                                    pty_sizes_dirty = true;
                                    dirty = true;
                                }
                                apply_cursor(
                                    &mut cur_cursor,
                                    match sh.axis {
                                        SplitAxis::Vertical => CursorKind::ResizeEW,
                                        SplitAxis::Horizontal => CursorKind::ResizeNS,
                                    },
                                );
                                continue;
                            }
                            if let Some((block_id, col, col_x0)) = dragging_table_col {
                                let new_w = x - col_x0 + 11.0;
                                if let Some(shell) = panes.get_mut(&wm.focused_pane())
                                    && shell.store.set_table_col_width(block_id, col, new_w)
                                {
                                    dirty = true;
                                }
                                apply_cursor(&mut cur_cursor, CursorKind::ResizeEW);
                                continue;
                            }
                            if dragging_agent_sidebar {
                                let pane_rect: HitRect = tile_layout
                                    .panes
                                    .iter()
                                    .find(|p| p.focused)
                                    .map(|p| p.rect.into())
                                    .unwrap_or_else(|| {
                                        HitRect::new(0.0, 0.0, size.width as f32, size.height as f32)
                                    });
                                let pw = (pane_rect.x1 - pane_rect.x0).max(240.0);
                                let ratio = (x - pane_rect.x0) / pw;
                                if let Some(shell) = panes.get_mut(&wm.focused_pane())
                                    && let Some(hub) = &mut shell.agent_hub
                                    && hub.set_sidebar_ratio(ratio)
                                {
                                    dirty = true;
                                }
                                apply_cursor(&mut cur_cursor, CursorKind::ResizeEW);
                                continue;
                            }
                            let shell = panes
                                .get_mut(&wm.focused_pane())
                                .expect("focused pane must exist");
                            if let Some(d) = &mut pending_drag {
                                d.x = x;
                                d.y = y;
                                if !d.active && (x - d.start_x).hypot(y - d.start_y) >= 4.0 {
                                    d.active = true;
                                }
                                if d.active {
                                    let cr = composite_scene.composer_rect;
                                    d.over_composer = cr.contains(x, y)
                                        || (y >= cr.y0 && y <= size.height as f32 && x >= cr.x0 && x <= cr.x1);
                                    apply_cursor(&mut cur_cursor, CursorKind::ClosedHand);
                                    dirty = true;
                                }
                                continue;
                            }
                            if selecting_composer {
                                let m = fonts.cell_metrics(scale);
                                let [ox, oy] = composite_scene.composer_text_origin;
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
                            if dragging_table_col.take().is_some() {
                                apply_cursor(&mut cur_cursor, CursorKind::Arrow);
                                continue;
                            }
                            if dragging_agent_sidebar {
                                dragging_agent_sidebar = false;
                                apply_cursor(&mut cur_cursor, CursorKind::Arrow);
                                continue;
                            }
                            if dragging_split.take().is_some() {
                                pty_sizes_dirty = true;
                                dirty = true;
                                let alt_scr = panes
                                    .get(&wm.focused_pane())
                                    .is_some_and(|p| p.pty_fullscreen);
                                let next = cursor_for_point(
                                    &composite_scene,
                                    &tile_layout.handles,
                                    None,
                                    alt_scr,
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
                            let shell = panes
                                .get_mut(&wm.focused_pane())
                                .expect("focused pane must exist");
                            if let Some(d) = pending_drag.take() {
                                let cr = composite_scene.composer_rect;
                                let dropped_in_prompt = cr.contains(x, y)
                                    || (y >= cr.y0 && y <= size.height as f32 && x >= cr.x0 && x <= cr.x1);
                                if d.active && dropped_in_prompt {
                                    shell.editor.enter_vim_insert();
                                    shell.editor.insert_dropped_token(&d.payload.insert_text);
                                    popup_engaged = false;
                                    shell.auto_scroll_bottom = true;
                                    shell.refresh_editor_assist(false);
                                } else if !d.active {
                                    // Simple click (without dragging) on any file/folder opens its full
                                    // structured table / expandable record / rendered preview immediately!
                                    let tok = d.payload.insert_text.trim();
                                    if !tok.is_empty() {
                                        let is_dir = d.payload.label.ends_with('/')
                                            || composite_scene.hover_at(x, y).is_some_and(|r| {
                                                matches!(
                                                    r.kind,
                                                    HoverKind::FileEntry { is_dir: true, .. }
                                                        | HoverKind::DirPreview { .. }
                                                )
                                            });
                                        let quoted = if tok.contains(' ') {
                                            format!("\"{tok}\"")
                                        } else {
                                            tok.to_owned()
                                        };
                                        let cmd = if is_dir {
                                            format!("ls -l {quoted}")
                                        } else {
                                            format!("open {quoted}")
                                        };
                                        let (cols, rows) = grid_for(size, &fonts, scale);
                                        let cpx = (
                                            fonts.metrics.cell_w as u32,
                                            fonts.metrics.cell_h as u32,
                                        );
                                        active_popover = None;
                                        shell.submit_command(&cmd, cols, rows, cpx, true);
                                        shell.auto_scroll_bottom = true;
                                    }
                                }
                                let next = cursor_for_point(
                                    &composite_scene,
                                    &tile_layout.handles,
                                    None,
                                    shell.pty_fullscreen,
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
                                    &composite_scene,
                                    &tile_layout.handles,
                                    None,
                                    shell.pty_fullscreen,
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
                                    &composite_scene,
                                    &tile_layout.handles,
                                    None,
                                    shell.pty_fullscreen,
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
                    let mut on_resize = |rev: ResizeEvent| {
                        handle_resize_event(
                            rev,
                            &window,
                            &layer,
                            &renderer,
                            &wm,
                            &mut panes,
                            &mut fonts,
                            &mut icon_atlas,
                            &mut image_atlas,
                            &mut size,
                            &mut scale,
                            font_points,
                            anim_epoch,
                            &mut has_animated_images,
                            &mut tile_layout,
                            &mut composite_scene,
                            &mut pty_sizes_dirty,
                            &mut dirty,
                            &mut last_pty_sync,
                            sidebar_open,
                        );
                    };
                    with_resize_hook(&mut on_resize, || {
                        app.sendEvent(&ev);
                    });
                }
                let mut on_resize = |rev: ResizeEvent| {
                    handle_resize_event(
                        rev,
                        &window,
                        &layer,
                        &renderer,
                        &wm,
                        &mut panes,
                        &mut fonts,
                        &mut icon_atlas,
                        &mut image_atlas,
                        &mut size,
                        &mut scale,
                        font_points,
                        anim_epoch,
                        &mut has_animated_images,
                        &mut tile_layout,
                        &mut composite_scene,
                        &mut pty_sizes_dirty,
                        &mut dirty,
                        &mut last_pty_sync,
                        sidebar_open,
                    );
                };
                with_resize_hook(&mut on_resize, || {
                    app.updateWindows();
                });
            });

            // Distinguish user-closed window from `Cmd+H` (app hidden) or `Cmd+M` (minimized to Dock)
            // so hiding/minimizing keeps the shell alive and repaints cleanly on unhide.
            let is_hidden_or_min = app.isHidden() || window.isMiniaturized();
            let closed_by_user = !window.isVisible() && !is_hidden_or_min;
            if quit || closed_by_user {
                for pane in panes.values_mut() {
                    if let Some(ap) = pane.active_pty.take() {
                        ap.pty.hangup();
                    }
                }
                break;
            }
            if is_hidden_or_min {
                was_hidden = true;
            } else if was_hidden {
                was_hidden = false;
                dirty = true;
            }

            // Poll ALL panes in the window so unfocused tiled panes stream git/jobs/PTYs live at 60fps!
            let cur_cli_rev = blaze_shell::cli_spec_driver::spec_revision();
            if cur_cli_rev != last_cli_spec_rev {
                last_cli_spec_rev = cur_cli_rev;
                if let Some(shell) = panes.get_mut(&wm.focused_pane()) {
                    if !shell.editor.text.is_empty() {
                        let was_open = !shell.editor.completions.is_empty() || popup_engaged;
                        shell.refresh_editor_assist(was_open);
                    }
                    // If the user is currently hovering a command token whose `man` / `--help`
                    // probe just completed, re-hydrate the explanation card live!
                    if let Some(pop) = &mut active_popover
                        && let HoverKind::CommandExplain { source, token_start, token_end, .. } =
                            &pop.kind
                    {
                        let path_env = shell.store.active_state().get_env("PATH").unwrap_or("");
                        if let Some(updated) = hydrate_command_explain(
                            source,
                            *token_start,
                            *token_end,
                            path_env,
                            Some(&shell.home),
                        ) {
                            pop.kind = updated;
                        }
                    }
                    dirty = true;
                }
            }
            let cur_dust_rev = blaze_shell::dust_driver::dust_revision();
            if cur_dust_rev != last_dust_rev {
                last_dust_rev = cur_dust_rev;
                dirty = true;
            }
            let cur_git_meta_rev = blaze_shell::git_meta_driver::git_meta_revision();
            if cur_git_meta_rev != last_git_meta_rev {
                last_git_meta_rev = cur_git_meta_rev;
                dirty = true;
            }
            let cur_remote_rev = blaze_shell::remote_driver::remote_complete_revision();
            if cur_remote_rev != last_remote_rev {
                last_remote_rev = cur_remote_rev;
                if let Some(shell) = panes.get_mut(&wm.focused_pane())
                    && !shell.editor.text.is_empty()
                {
                    let was_open = !shell.editor.completions.is_empty() || popup_engaged;
                    shell.refresh_editor_assist(was_open);
                    dirty = true;
                }
            }
            let cur_term_rev = term_server.revision();
            if cur_term_rev != last_term_rev {
                last_term_rev = cur_term_rev;
                for pane in panes.values_mut() {
                    pane.refresh_prompt(pane.prompt.last_exit, pane.prompt.last_duration_ms);
                }
                dirty = true;
            }
            let cur_app_icon_rev = blaze_shell::sysmon_driver::app_icon_revision();
            if cur_app_icon_rev != last_app_icon_rev {
                last_app_icon_rev = cur_app_icon_rev;
                dirty = true;
            }
            let cur_agent_probe_rev = blaze_shell::agent_probe::probe_revision();
            if cur_agent_probe_rev != last_agent_probe_rev {
                last_agent_probe_rev = cur_agent_probe_rev;
                for pane in panes.values_mut() {
                    if let Some(hub) = &mut pane.agent_hub
                        && hub.sync_reachability()
                    {
                        dirty = true;
                    }
                }
            }
            let cur_fp_rev = blaze_shell::sysmon_driver::folder_proc_revision();
            if cur_fp_rev != last_folder_proc_rev
                || last_folder_poll.elapsed() >= std::time::Duration::from_millis(1200)
            {
                last_folder_proc_rev = cur_fp_rev;
                last_folder_poll = Instant::now();
                let pane_roots = collect_shell_pane_pids(&wm, &panes);
                for pane in panes.values_mut() {
                    let next_fp = blaze_shell::sysmon_driver::poll_folder_procs(
                        &pane.prompt.cwd,
                        &pane_roots,
                    );
                    if pane.prompt.folder_procs != next_fp {
                        pane.prompt.folder_procs = next_fp;
                        dirty = true;
                    }
                }
                if let Some(pop) = &mut active_popover
                    && let HoverKind::FolderProcs { summary } = &mut pop.kind
                    && let Some(focused) = panes.get(&wm.focused_pane())
                    && focused.prompt.cwd == summary.folder
                    && *summary != focused.prompt.folder_procs
                {
                    *summary = focused.prompt.folder_procs.clone();
                    dirty = true;
                }
            }
            // Live ~900ms native telemetry sampling for any running Activity & GPU Monitor (`top`/`btop`).
            for pane in panes.values_mut() {
                if let Some(sid) = pane.active_sysmon
                    && pane.last_sysmon_tick.elapsed() >= std::time::Duration::from_millis(900)
                {
                    pane.last_sysmon_tick = Instant::now();
                    if let Some(blk) = pane.store.block_mut(sid)
                        && let BlockBody::SysMon(sm) = &mut blk.body
                        && sm.running
                    {
                        if !sm.engine.paused() {
                            let snap = blaze_shell::sysmon_driver::sample_snapshot(Alloc::global());
                            sm.engine.ingest(snap);
                            let vis = if pane.sysmon_fullscreen {
                                blaze_shell::sysmon_view::visible_rows_for_height(
                                    Some((size.height as f32 - TOP_BAR_H - 12.0).max(200.0)),
                                    true,
                                    sm.filter_focused || !sm.filter_input.is_empty(),
                                )
                            } else {
                                blaze_shell::sysmon_view::INLINE_VISIBLE_ROWS
                            };
                            sm.clamp_selection(vis);
                            dirty = true;
                        }
                    }
                }
            }
            let focused_pid = wm.focused_pane();
            let cpx = (fonts.metrics.cell_w as u32, fonts.metrics.cell_h as u32);
            let mut any_running_jobs = false;
            for (&pid, pane) in panes.iter_mut() {
                if let Some((cwd, summary)) = pane.git_worker.poll()
                    && cwd == pane.prompt.cwd
                {
                    if let Some(st) = summary {
                        pane.prompt.git_branch = Some(st.branch.clone());
                        pane.prompt.git_detached = st.detached;
                        pane.prompt.git_status = Some(st);
                    } else {
                        pane.prompt.git_status = None;
                    }
                    dirty = true;
                }

                let (cols, rows) = tile_layout
                    .panes
                    .iter()
                    .find(|pl| pl.pane == pid)
                    .map(|pl| grid_for_rect(pl.rect.into(), &fonts, scale))
                    .unwrap_or_else(|| grid_for(size, &fonts, scale));
                if pane.poll_async_jobs(cols, rows, cpx) {
                    dirty = true;
                }
                if pane.running_jobs > 0 {
                    any_running_jobs = true;
                }

                let mut pty_finished: Option<(BlockId, i32, u64)> = None;
                if let Some(ap) = &pane.active_pty {
                    let mut closed = false;
                    loop {
                        match ap.pty.read(&mut pty_buf) {
                            PtyRead::Bytes(n) => {
                                if let Some(blk) = pane.store.block_mut(ap.block_id)
                                    && let BlockBody::Terminal { term, .. } = &mut blk.body
                                {
                                    term.feed(&pty_buf[..n]);
                                    let replies = term.take_replies();
                                    if !replies.is_empty() {
                                        ap.pty.write_all(&replies);
                                    }
                                    let effects = term.take_effects();
                                    for eff in effects.iter() {
                                        match *eff {
                                            Effect::TitleChanged if pid == focused_pid => {
                                                let t = String::from_utf8_lossy(term.title()).into_owned();
                                                window.setTitle(&NSString::from_str(if t.is_empty() {
                                                    "blaze-shell"
                                                } else {
                                                    &t
                                                }));
                                            }
                                            Effect::BlazeTermApi(span) => {
                                                let (_mutated, reply) =
                                                    term_server.handle_osc_7770(effects.bytes(span));
                                                if let Some(osc_reply) = reply {
                                                    ap.pty.write_all(osc_reply.as_bytes());
                                                }
                                            }
                                            _ => {}
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
                    pane.pty_fullscreen = false;
                    pane.pty_focused = false;
                    pane.pty_inline_rows_override = None;
                    if let Some(ap) = pane.active_pty.take() {
                        let drain_deadline = Instant::now() + std::time::Duration::from_millis(15);
                        loop {
                            match ap.pty.read(&mut pty_buf) {
                                PtyRead::Bytes(n) => {
                                    if let Some(blk) = pane.store.block_mut(ap.block_id)
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
                    if let Some(blk) = pane.store.block_mut(block_id) {
                        blk.exit_code = code;
                        blk.duration_ms = dur_ms;
                        if let BlockBody::Terminal { running, exit_code, .. } = &mut blk.body {
                            *running = false;
                            *exit_code = Some(code);
                        }
                    }
                    if pid == focused_pid {
                        window.setTitle(&NSString::from_str("blaze-shell"));
                    }
                    pane.refresh_prompt(code, dur_ms);
                    pane.auto_scroll_bottom = true;
                    dirty = true;
                }
            }
            if any_running_jobs && last_running_tick.elapsed().as_millis() >= 80 {
                last_running_tick = Instant::now();
                dirty = true;
            }
            if has_animated_images
                && !is_hidden_or_min
                && last_anim_tick.elapsed().as_millis() >= 15
                && image_atlas.has_ready_animation_frame(anim_epoch.elapsed().as_millis() as u64)
            {
                last_anim_tick = Instant::now();
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
                    image_atlas.set_scale(scale);
                    layer.setContentsScale(scale);
                    zoom_changed = false;
                }
                size = new_size;
                layer.setDrawableSize(NSSize::new(size.width * scale, size.height * scale));
                pty_sizes_dirty = true;
                dirty = true;
            }
            if pty_sizes_dirty {
                let sw = if sidebar_open {
                    SIDEBAR_W.min(size.width as f32 * 0.34).max(180.0)
                } else {
                    0.0
                };
                let area = HitRect::new(sw, TOP_BAR_H, size.width as f32, size.height as f32);
                let next_tl = wm.layout(area.into(), TILE_GAP);
                sync_pty_sizes(&mut panes, &next_tl, &fonts, scale);
                pty_sizes_dirty = false;
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

            let now_key_window = window.isKeyWindow();
            if now_key_window != was_key_window {
                was_key_window = now_key_window;
                if !now_key_window {
                    cur_cursor = None;
                    last_mouse = None;
                }
                if let Some(pane) = panes.get_mut(&wm.focused_pane())
                    && pane.pty_accepts_input()
                {
                    pane.send_pty_focus_event(now_key_window);
                }
                dirty = true;
            }
            if dirty && !is_hidden_or_min {
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
                let frame_now = now_ns();
                let mouse_pt = last_mouse.map(|(mx, my)| [mx, my]);
                let (mut scene, next_tl) = build_window_scene(
                    &wm,
                    &mut panes,
                    size.width as f32,
                    size.height as f32,
                    metrics,
                    cursor_vis,
                    frame_now,
                    zoom_pct,
                    true,
                    active_drag.as_ref(),
                    active_trail,
                    output_selection,
                    active_popover.as_ref(),
                    mouse_pt,
                    output_nav,
                    dragging_split,
                    command_palette.as_ref(),
                    sidebar_open,
                );
                tile_layout = next_tl;

                if let Some(fb) = &mut find_bar {
                    let active_hit = blaze_shell::layout::apply_find_overlay(
                        &mut scene,
                        fb,
                        size.width as f32,
                        TOP_BAR_H,
                        size.height as f32,
                        metrics,
                        cursor_vis,
                        mouse_pt,
                    );
                    if find_needs_scroll {
                        find_needs_scroll = false;
                        if let Some(fm) = active_hit {
                            let delta = blaze_shell::find::scroll_delta_for_match(
                                &fm,
                                TOP_BAR_H,
                                size.height as f32,
                            );
                            if delta.abs() > 1.0
                                && let Some(pane) = panes.get_mut(&wm.focused_pane())
                            {
                                let max_scroll = (scene.total_content_height
                                    - scene.scroll_viewport_height)
                                    .max(0.0);
                                pane.auto_scroll_bottom = false;
                                pane.scroll_y = (pane.scroll_y + delta).clamp(0.0, max_scroll);
                                let (mut rescene, next_tl2) = build_window_scene(
                                    &wm,
                                    &mut panes,
                                    size.width as f32,
                                    size.height as f32,
                                    metrics,
                                    cursor_vis,
                                    frame_now,
                                    zoom_pct,
                                    true,
                                    active_drag.as_ref(),
                                    active_trail,
                                    output_selection,
                                    active_popover.as_ref(),
                                    mouse_pt,
                                    output_nav,
                                    dragging_split,
                                    command_palette.as_ref(),
                                    sidebar_open,
                                );
                                blaze_shell::layout::apply_find_overlay(
                                    &mut rescene,
                                    fb,
                                    size.width as f32,
                                    TOP_BAR_H,
                                    size.height as f32,
                                    metrics,
                                    cursor_vis,
                                    mouse_pt,
                                );
                                scene = rescene;
                                tile_layout = next_tl2;
                            }
                        }
                    }
                }

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

                let vh = size.height as f32;
                let glyphs = fonts.rasterize_runs(&scene.text_runs, scale, vh);
                let overlay_glyphs = fonts.rasterize_runs(&scene.overlay_text, scale, vh);
                for (slot, bytes) in fonts.pending.drain(..) {
                    renderer.upload(slot, &bytes);
                }
                let mut icons = icon_atlas.rasterize_quads(&scene.icon_quads);
                icon_atlas.rasterize_emoji_quads_into(&scene.emoji_quads, &mut icons);
                icon_atlas.rasterize_custom_symbol_quads_into(&scene.custom_symbols, &mut icons);
                let overlay_icons = icon_atlas.rasterize_quads(&scene.overlay_icons);
                for (slot, rgba) in icon_atlas.pending.drain(..) {
                    renderer.upload_icon(slot, &rgba);
                }
                let anim_ms = anim_epoch.elapsed().as_millis() as u64;
                let (images, any_anim) = image_atlas.rasterize_quads(&scene.image_quads, anim_ms, vh);
                let (overlay_images, any_ov_anim) =
                    image_atlas.rasterize_quads(&scene.overlay_images, anim_ms, vh);
                has_animated_images = any_anim || any_ov_anim;
                image_atlas.drain_pending_into(|slot, rgba| renderer.upload_image(slot, rgba));
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
                    images: &images,
                    over_rects: &scene.over_rects,
                    overlay_rects: &scene.overlay_rects,
                    overlay_icons: &overlay_icons,
                    overlay_images: &overlay_images,
                    overlay_glyphs: &overlay_glyphs,
                };
                renderer.draw(&layer, &frame);
                // Keep `text_runs`, hit regions, drag sources & composer geometry in `composite_scene`
                // for instant mouse hover, output text selection, and `Cmd+C` extraction!
                scene.rects.clear();
                scene.icon_quads.clear();
                scene.emoji_quads.clear();
                scene.custom_symbols.clear();
                scene.image_quads.clear();
                scene.over_rects.clear();
                scene.overlay_rects.clear();
                scene.overlay_icons.clear();
                scene.overlay_images.clear();
                scene.overlay_text.clear();
                composite_scene = scene;
                if let Some((mx, my)) = last_mouse
                    && cur_cursor.is_some()
                    && window.isKeyWindow()
                    && mx >= 0.0
                    && my >= 0.0
                    && mx <= size.width as f32
                    && my <= size.height as f32
                    && !(my < TOP_BAR_H && mx < 76.0)
                    && !on_resize_edge(mx, my, size.width as f32, size.height as f32)
                {
                    let alt_scr = panes
                        .get(&wm.focused_pane())
                        .is_some_and(|p| p.pty_fullscreen);
                    let next = cursor_for_point(
                        &composite_scene,
                        &tile_layout.handles,
                        dragging_split,
                        alt_scr,
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
        term_server.shutdown();
    }
}
