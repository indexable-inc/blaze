//! `blaze-shell`: native macOS GUI shell library & application.
//!
//! All GPU-independent modules (`theme`, `blocks`, `prompt`, `editor`, `view`, `layout`,
//! `rust_cell`, `host`) compile and are unit-tested on every platform without requiring Metal.

pub mod blocks;
pub mod editor;
pub mod host;
pub mod icons;
pub mod layout;
pub mod prompt;
pub mod rust_cell;
pub mod svg_icons;
pub mod theme;
pub mod view;

#[cfg(target_os = "macos")]
pub mod font;
#[cfg(target_os = "macos")]
pub mod keys;
#[cfg(target_os = "macos")]
pub mod pty;
#[cfg(target_os = "macos")]
pub mod render;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use blaze_own::Alloc;
    use blaze_sh::{Bytes, EngineState, ErrorKind, PVec, Record, ShellError, Span, Table, Value};

    use crate::blocks::{BlockBody, BlockStore, TABLE_FOLD_ROWS};
    use crate::editor::Editor;
    use crate::layout::{LayoutInput, build_scene};
    use crate::prompt::{PromptInfo, abbreviate_home, read_git_head};
    use crate::rust_cell::{CELL_JSON_SENTINEL, synthesize_cell_source};
    use crate::view::{CellMetrics, HitAction};

    fn make_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("blaze-shell-test-{tag}-{nanos}"));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn layout_table_folding_cell_expansion_and_error_span() {
        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[("HOME", "/tmp")], alloc);
        let mut store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 2);

        // 1. Build a 30-row table with a nested Record cell at (0, 2).
        let mut table = Table::from_col_names(&["name", "size", "meta"], alloc);
        for i in 0..30 {
            let mut nested = Record::new(alloc);
            nested.insert_str("idx", Value::Int(i as i64));
            nested.insert_str("ok", Value::Bool(true));
            table.push_row_values(&[
                Value::string(&format!("file_{i}.rs"), alloc),
                Value::Size(Bytes((i as i64 + 1) * 1024)),
                Value::Record(nested),
            ]);
        }
        let tbl_id = store.record_block(
            "ls | sort-by size".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            0,
            3,
            0,
            BlockBody::Value(Value::Table(table)),
        );
        assert!(store.block(tbl_id).unwrap().folded, "30-row table should start folded");

        // 2. Add a Record block.
        let mut rec = Record::new(alloc);
        rec.insert_str("os", Value::string("macos", alloc));
        rec.insert_str("cores", Value::Int(12));
        store.record_block(
            "sys".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            10,
            1,
            0,
            BlockBody::Value(Value::Record(rec)),
        );

        // 3. Add an Error block with an exact span into the command line.
        let err_src = "ls | get missing_col".to_owned();
        let err = ShellError::new(
            ErrorKind::MissingColumn,
            Span::new(9, 20),
            "table has no column `missing_col`",
            alloc,
        )
        .with_help("use `select` or inspect columns with `describe`", alloc);
        store.record_block(
            err_src,
            prompt.clone(),
            state.fork(),
            state.fork(),
            20,
            1,
            1,
            BlockBody::Error { err },
        );

        let editor = Editor::new();
        let scene_folded = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1000.0,
            viewport_h: 1600.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });

        // Fold toggle hit region must be present, and every block must have a ForkBlock hit target.
        assert!(
            scene_folded
                .hit_regions
                .iter()
                .any(|h| h.action == HitAction::ToggleFold(tbl_id)),
            "expected ToggleFold hit region for >{TABLE_FOLD_ROWS} row table"
        );
        assert!(
            scene_folded
                .hit_regions
                .iter()
                .any(|h| h.action == HitAction::ForkBlock(tbl_id)),
            "expected ForkBlock hit region on block header"
        );
        assert!(
            scene_folded.hit_regions.iter().any(|h| {
                h.action == HitAction::ToggleExpandCell { block: tbl_id, row: 0, col: 2 }
            }),
            "expected ToggleExpandCell hit region on nested record cell"
        );
        assert!(
            scene_folded
                .text_runs
                .iter()
                .any(|t| t.text.contains("missing_column")),
            "expected error kind label in scene"
        );
        assert!(
            !scene_folded.over_rects.is_empty(),
            "expected span underline over_rect for error"
        );

        // Expanding the table fold must increase total layout height.
        let h_folded = scene_folded.total_content_height;
        assert!(store.toggle_fold(tbl_id));
        let scene_unfolded = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1000.0,
            viewport_h: 800.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });
        assert!(
            scene_unfolded.total_content_height > h_folded,
            "unfolding 30-row table must increase content height"
        );

        // Expanding nested cell (0, 2) must increase height further and show inline drawer.
        let h_before_expand = scene_unfolded.total_content_height;
        assert!(store.toggle_expand_cell(tbl_id, 0, 2));
        let scene_expanded = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1000.0,
            viewport_h: 800.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });
        assert!(scene_expanded.total_content_height > h_before_expand);
        assert!(scene_expanded.text_runs.iter().any(|t| t.text == "meta"));
    }

    #[test]
    fn cow_fork_from_historical_block_preserves_branch_isolation() {
        let alloc = Alloc::global();
        let s0 = EngineState::new("/tmp", &[], alloc);
        let mut store = BlockStore::new(s0.fork());
        let main_branch = store.active_branch_id();
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 0);

        let mut s1 = s0.fork();
        s1.set_var("x", Value::Int(10), false);
        let b1 = store.record_block(
            "let x = 10".to_owned(),
            prompt.clone(),
            s0.fork(),
            s1.fork(),
            1,
            1,
            0,
            BlockBody::Value(Value::Nothing),
        );
        store.set_active_state(s1.fork());

        let mut s2 = s1.fork();
        s2.set_var("x", Value::Int(99), false);
        s2.set_var("y", Value::Int(200), false);
        let _b2 = store.record_block(
            "let x = 99; let y = 200".to_owned(),
            prompt.clone(),
            s1.fork(),
            s2.fork(),
            2,
            1,
            0,
            BlockBody::Value(Value::Nothing),
        );
        store.set_active_state(s2.fork());

        // Fork from historical block `b1`: should have only [b1], `x == 10`, and no `y`!
        let fork_branch = store
            .fork_from_block(b1, Some("exp"))
            .expect("fork from block 1");
        assert_eq!(store.active_branch_id(), fork_branch);
        assert_eq!(store.active_blocks().len(), 1);
        assert_eq!(store.active_state().get_var("x").unwrap().value, Value::Int(10));
        assert!(store.active_state().get_var("y").is_none());

        // Mutating the forked branch must NOT affect `main`.
        let mut forked_mut = store.active_state().fork();
        forked_mut.set_var("z", Value::Int(7), false);
        store.set_active_state(forked_mut);

        assert!(store.switch_branch(main_branch));
        assert_eq!(store.active_blocks().len(), 2);
        assert_eq!(store.active_state().get_var("x").unwrap().value, Value::Int(99));
        assert_eq!(store.active_state().get_var("y").unwrap().value, Value::Int(200));
        assert!(store.active_state().get_var("z").is_none());
    }

    #[test]
    fn editor_editing_history_reverse_search_and_completion() {
        let alloc = Alloc::global();
        let mut state = EngineState::new("/tmp", &[], alloc);
        state.set_var("my_table", Value::List(PVec::new(alloc)), false);

        let mut ed = Editor::new();
        ed.insert_str("ls | sort-by size");
        assert_eq!(ed.text, "ls | sort-by size");

        // Kill word back (`Ctrl+W`) removes `size`.
        ed.kill_word_back();
        assert_eq!(ed.text, "ls | sort-by ");

        // Submit and test history navigation preserving draft.
        let s1 = ed.take_submitted();
        assert_eq!(s1, "ls | sort-by ");
        ed.insert_str("echo hello");
        let _ = ed.take_submitted();

        ed.insert_str("draft");
        ed.move_up();
        assert_eq!(ed.text, "echo hello");
        ed.move_up();
        assert_eq!(ed.text, "ls | sort-by");
        ed.move_down();
        assert_eq!(ed.text, "echo hello");
        ed.move_down();
        assert_eq!(ed.text, "draft");

        // Ctrl+R reverse search.
        ed.start_reverse_search();
        ed.reverse_search_push('s');
        ed.reverse_search_push('o');
        ed.reverse_search_push('r');
        ed.reverse_search_push('t');
        assert_eq!(
            ed.reverse_search.as_ref().and_then(|r| r.matched_text.as_deref()),
            Some("ls | sort-by")
        );
        assert!(ed.accept_reverse_search());
        assert_eq!(ed.text, "ls | sort-by");

        // Variable completion (`$my_` -> `$my_table`).
        ed.clear_line();
        ed.insert_str("echo $my_");
        ed.trigger_completion(&state);
        assert_eq!(ed.text, "echo $my_table");

        // Builtin completion (`sort-b` -> `sort-by `).
        ed.clear_line();
        ed.insert_str("ls | sort-b");
        ed.trigger_completion(&state);
        assert_eq!(ed.text, "ls | sort-by ");
    }

    #[test]
    fn prompt_git_head_normal_worktree_and_abbreviate_home() {
        assert_eq!(abbreviate_home("/Users/alice", "/Users/alice"), "~");
        assert_eq!(abbreviate_home("/Users/alice/dev/blaze", "/Users/alice"), "~/dev/blaze");
        assert_eq!(abbreviate_home("/Users/alice2/dev", "/Users/alice"), "/Users/alice2/dev");

        // Normal `.git/HEAD` directory.
        let repo = make_temp_dir("git-normal");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feat/blaze-shell\n").unwrap();
        let nested = repo.join("crates/blaze-shell");
        fs::create_dir_all(&nested).unwrap();
        let head = read_git_head(nested.to_str().unwrap()).expect("normal git head");
        assert_eq!(head.label, "feat/blaze-shell");
        assert!(!head.detached);

        // Detached HEAD hash.
        fs::write(repo.join(".git/HEAD"), "a1b2c3d4e5f678901234567890abcdef12345678\n").unwrap();
        let detached = read_git_head(nested.to_str().unwrap()).expect("detached head");
        assert_eq!(detached.label, "a1b2c3d");
        assert!(detached.detached);

        // Git worktree where `.git` is a file pointing to `gitdir: ...`.
        let wt_gitdir = make_temp_dir("git-wt-dir");
        fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/worktree-branch\n").unwrap();
        let wt_root = make_temp_dir("git-wt-root");
        fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", wt_gitdir.display()),
        )
        .unwrap();
        let wt_head = read_git_head(wt_root.to_str().unwrap()).expect("worktree git head");
        assert_eq!(wt_head.label, "worktree-branch");
        assert!(!wt_head.detached);

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&wt_gitdir);
        let _ = fs::remove_dir_all(&wt_root);
    }

    #[test]
    fn rust_cell_synthesizes_prior_defs_input_and_json_sentinel() {
        let prior = ["fn square(n: i64) -> i64 { n * n }", "let ignored_expr = 1;"];
        let src = synthesize_cell_source(&prior, "square(7)", "{\"a\":1}");
        assert!(src.contains("fn square(n: i64) -> i64 { n * n }"));
        assert!(!src.contains("let ignored_expr = 1;"));
        assert!(src.contains("let input: &str ="));
        assert!(src.contains(CELL_JSON_SENTINEL));
        assert!(src.contains("square(7)"));

        // Definition-only cell should emit `null` sentinel without wrapping inside closure.
        let def_src = synthesize_cell_source(&[], "struct Point { x: i64, y: i64 }", "null");
        assert!(def_src.contains("struct Point { x: i64, y: i64 }"));
        assert!(def_src.contains(&format!("{CELL_JSON_SENTINEL}null")));
    }

    #[test]
    fn vendored_svg_icons_parse_and_grid_toggles_to_table() {
        use blaze_sh::ColDisplay;
        use crate::svg_icons::{SVG_SOURCES, get_svg, icon_name_for_file};

        // Every embedded macOS folder + IntelliJ New UI dark SVG must parse into non-empty geometry.
        assert!(SVG_SOURCES.len() >= 35);
        for &(name, _) in SVG_SOURCES {
            let art = get_svg(name).unwrap_or_else(|| panic!("missing parsed svg `{name}`"));
            assert!(art.view[2] > 0.0 && art.view[3] > 0.0, "empty viewBox for `{name}`");
            assert!(!art.layers.is_empty(), "no vector layers parsed for `{name}`");
        }
        assert_eq!(icon_name_for_file("crates", Some("dir")), "folder_macos");
        assert_eq!(icon_name_for_file("target", Some("dir")), "folder_target");
        assert_eq!(icon_name_for_file(".github", Some("dir")), "folderGithub_dark");
        assert_eq!(icon_name_for_file("Cargo.toml", Some("file")), "cargo_toml");
        assert_eq!(icon_name_for_file("Cargo.lock", Some("file")), "cargo_lock");
        assert_eq!(icon_name_for_file("blaze.lock", Some("file")), "blaze_lock");
        assert_eq!(icon_name_for_file("main.rs", Some("file")), "rust_dark");
        assert_eq!(icon_name_for_file(".gitignore", Some("file")), "gitignore");

        // File table with `default_grid = true` renders compact grid by default and toggles to table.
        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[("HOME", "/tmp")], alloc);
        let mut store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 1);

        let mut table = Table::from_col_names(&["name", "type", "size", "modified"], alloc);
        table.set_col_display("name", ColDisplay::FileEntry);
        table.set_col_display("size", ColDisplay::FileSize);
        table.set_col_display("modified", ColDisplay::HumanTime);
        table.default_grid = true;
        for (name, kind, sz) in [
            ("crates", "dir", 0),
            ("docs", "dir", 0),
            ("Cargo.toml", "file", 4200),
            ("Cargo.lock", "file", 190_000),
            ("main.rs", "file", 12_400),
            ("README.md", "file", 8100),
        ] {
            table.push_row_values(&[
                Value::string(name, alloc),
                Value::string(kind, alloc),
                Value::Size(Bytes(sz)),
                Value::Date(blaze_sh::DateTime {
                    utc_ns: 1_732_500_000_000_000_000,
                    offset_secs: 0,
                }),
            ]);
        }
        let bid = store.record_block(
            "ls".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            1,
            1,
            0,
            BlockBody::Value(Value::Table(table)),
        );

        let editor = Editor::new();
        let grid_scene = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1000.0,
            viewport_h: 800.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });
        assert!(
            grid_scene
                .hit_regions
                .iter()
                .any(|h| h.action == HitAction::ToggleTableView(bid)),
            "expected ToggleTableView hit region for file table"
        );
        assert!(
            grid_scene.icon_quads.iter().any(|q| q.name == "folder_macos"),
            "expected macOS folder SVG icon in grid"
        );
        assert!(
            grid_scene.icon_quads.iter().any(|q| q.name == "rust_dark"),
            "expected IntelliJ Rust SVG icon in grid"
        );
        assert!(
            grid_scene
                .drag_sources
                .iter()
                .any(|d| d.payload.insert_text == "Cargo.lock" && d.payload.icon == Some("cargo_lock")),
            "expected Cargo.lock to be draggable from ls grid"
        );
        assert!(
            !grid_scene.text_runs.iter().any(|t| t.text == "modified"),
            "compact grid should not render table column headers"
        );

        // Toggle to table view: height grows and column headers appear.
        assert!(store.toggle_table_view(bid));
        let table_scene = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1000.0,
            viewport_h: 800.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });
        assert!(table_scene.total_content_height > grid_scene.total_content_height);
        assert!(
            table_scene.text_runs.iter().any(|t| t.text == "modified"),
            "detailed table view should render column headers"
        );
        assert!(
            table_scene
                .drag_sources
                .iter()
                .any(|d| d.payload.insert_text == "Cargo.lock"),
            "expected Cargo.lock to be draggable from ls table"
        );
    }

    #[test]
    fn undo_redo_abbreviations_ghost_completion_and_drag_drop_insert() {
        use crate::editor::{CompletionKind, parse_nuon_abbreviations};

        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[], alloc);
        let mut ed = Editor::new();

        // 1. Undo/redo (`Cmd+Z` / `Cmd+Shift+Z`) coalesces word typing and restores drops.
        for ch in "open".chars() {
            ed.insert_char(ch);
        }
        ed.insert_char(' ');
        ed.insert_dropped_token("Cargo.lock");
        assert_eq!(ed.text, "open Cargo.lock ");
        assert!(ed.undo());
        assert_eq!(ed.text, "open ");
        assert!(ed.redo());
        assert_eq!(ed.text, "open Cargo.lock ");

        // Dropped paths with spaces are auto-quoted.
        ed.insert_dropped_token("my notes.md");
        assert_eq!(ed.text, "open Cargo.lock \"my notes.md\" ");

        // 2. Command-position abbreviations (`l` -> `ls`, `ls | sb` -> `ls | sort-by`) + undo.
        ed.clear_line();
        ed.insert_str("l");
        assert!(ed.try_expand_abbreviation());
        assert_eq!(ed.text, "ls");
        assert!(ed.undo());
        assert_eq!(ed.text, "l");

        ed.set_text("ls | sb".to_owned());
        assert!(ed.try_expand_abbreviation());
        assert_eq!(ed.text, "ls | sort-by");

        // Argument position must NOT expand abbreviations (`open l` keeps `l`).
        ed.set_text("open l".to_owned());
        assert!(!ed.try_expand_abbreviation());
        assert_eq!(ed.text, "open l");

        // Parse Nushell `abbreviations.nuon` format.
        let parsed = parse_nuon_abbreviations("# comment\n{\n  \"gs\": \"git status\"\n  \"l\": \"ls\"\n}\n");
        assert_eq!(parsed, vec![("gs".to_owned(), "git status".to_owned()), ("l".to_owned(), "ls".to_owned())]);

        // 3. Fish-style inline ghost autosuggestion (word-by-word and full accept).
        ed.push_history("ls crates/blaze-sh/src | sort-by size -r".to_owned());
        ed.set_text("ls cra".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(
            ed.ghost.as_deref(),
            Some("tes/blaze-sh/src | sort-by size -r")
        );
        assert!(ed.accept_ghost_word());
        assert_eq!(ed.text, "ls crates");
        assert!(ed.accept_ghost_full());
        assert_eq!(ed.text, "ls crates/blaze-sh/src | sort-by size -r");

        // 4. Column-aware IntelliJ completion after `ls | sort-by `.
        ed.set_text("ls | sort-by ".to_owned());
        ed.refresh_assist(&state, &[], true);
        assert!(
            ed.completions
                .iter()
                .any(|c| c.label == "modified" && c.kind == CompletionKind::Column),
            "expected column-aware completion for `ls | sort-by `"
        );
    }

    #[test]
    fn vim_modal_editing_auto_pairs_tab_out_and_atuin_history() {
        use blaze_vim::{KeyCode as VK, KeyEvent as VE, Mode as VM};
        use crate::editor::{HistoryEntry, HistoryScope};

        let mut ed = Editor::new();

        // 1. IntelliJ auto-pairing: `{` -> `{|}`, typing inside, `Tab` jumps out!
        ed.insert_typed("rust ");
        ed.insert_typed("{");
        assert_eq!(ed.text, "rust {}");
        assert_eq!(ed.cursor, 6);
        ed.insert_typed("(");
        assert_eq!(ed.text, "rust {()}");
        assert_eq!(ed.cursor, 7);
        ed.insert_typed("42");
        assert_eq!(ed.text, "rust {(42)}");
        assert!(ed.tab_out());
        assert_eq!(ed.cursor, 10);
        assert!(ed.tab_out());
        assert_eq!(ed.cursor, 11);

        // Overtype closing bracket + empty-pair backspace.
        ed.clear_line();
        ed.insert_typed("[");
        assert_eq!(ed.text, "[]");
        ed.backspace();
        assert_eq!(ed.text, "");
        ed.insert_typed("(");
        ed.insert_typed(")");
        assert_eq!(ed.text, "()");
        assert_eq!(ed.cursor, 2);

        // Selection wrapping.
        ed.set_text("foo".to_owned());
        ed.select_all();
        ed.insert_typed("{");
        assert_eq!(ed.text, "{foo}");

        // 2. Embedded `blaze-vim` modal editing with shell `i|` pipeline text object + undo.
        ed.set_text("ls | take 5 | sort-by size".to_owned());
        ed.cursor = 7; // inside `take 5`
        ed.feed_vim_key(VE::key(VK::Escape));
        assert_eq!(ed.vim_mode(), VM::Normal);
        ed.feed_vim_key(VE::char('c'));
        ed.feed_vim_key(VE::char('i'));
        ed.feed_vim_key(VE::char('|'));
        assert_eq!(ed.vim_mode(), VM::Insert);
        for ch in "head".chars() {
            let mut b = [0u8; 4];
            ed.insert_typed(ch.encode_utf8(&mut b));
        }
        assert_eq!(ed.text, "ls | head | sort-by size");
        assert!(ed.undo());
        assert_eq!(ed.text, "ls | take 5 | sort-by size");

        // 3. Atuin-class Ctrl+R history palette: fuzzy search + scope cycling.
        ed.record_history_entry(HistoryEntry {
            command: "blaze test -p blaze-vim".to_owned(),
            cwd: "/Volumes/Projects/blaze".to_owned(),
            git_branch: Some("main".to_owned()),
            duration_ms: 420,
            exit_code: 0,
            timestamp_ns: 1_732_500_000_000_000_000,
            in_session: true,
        });
        ed.record_history_entry(HistoryEntry {
            command: "git status".to_owned(),
            cwd: "/tmp/other".to_owned(),
            git_branch: Some("dev".to_owned()),
            duration_ms: 12,
            exit_code: 0,
            timestamp_ns: 1_732_500_010_000_000_000,
            in_session: false,
        });
        ed.start_reverse_search_in("/Volumes/Projects/blaze");
        assert_eq!(ed.reverse_search.as_ref().unwrap().scope, HistoryScope::Global);
        ed.reverse_search_push('b');
        ed.reverse_search_push('v');
        ed.reverse_search_push('i');
        ed.reverse_search_push('m');
        assert_eq!(
            ed.reverse_search.as_ref().and_then(|r| r.matched_text.as_deref()),
            Some("blaze test -p blaze-vim")
        );
        ed.reverse_search_cycle_scope();
        assert_eq!(ed.reverse_search.as_ref().unwrap().scope, HistoryScope::Directory);
        ed.reverse_search_cycle_scope();
        assert_eq!(ed.reverse_search.as_ref().unwrap().scope, HistoryScope::Session);
    }

    #[test]
    fn path_value_renders_folder_chip_and_text_selection_extracts_lines() {
        use blaze_sh::PathKind;
        use crate::view::{TextSelection, compute_text_selection};

        let alloc = Alloc::global();
        let state = EngineState::new("/Volumes/Projects/blaze", &[], alloc);
        let mut store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/Volumes/Projects/blaze", "/Users/dev", 0, 1);

        let pval = Value::path("/Volumes/Projects/blaze", PathKind::Dir, alloc);
        store.record_block(
            "pwd".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            1,
            1,
            0,
            BlockBody::Value(pval),
        );

        let editor = Editor::new();
        let m = CellMetrics::default();
        let scene = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 960.0,
            viewport_h: 640.0,
            metrics: m,
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });

        assert!(
            scene.icon_quads.iter().any(|q| q.name == "folder_macos"),
            "pwd Value::Path should render macOS folder SVG icon"
        );
        assert!(
            scene.drag_sources.iter().any(|d| d.payload.insert_text == "/Volumes/Projects/blaze"),
            "pwd Value::Path should be draggable into the prompt"
        );

        // Universal drag selection across rendered scene text runs extracts the path text.
        let (rects, extracted) = compute_text_selection(
            &scene.text_runs,
            m,
            TextSelection { anchor: [0.0, 40.0], head: [900.0, 220.0] },
            34.0,
        );
        assert!(!rects.is_empty(), "expected highlight rects for drag selection");
        assert!(
            extracted.contains("/Volumes/Projects/blaze"),
            "expected extracted text to include path, got: {extracted:?}"
        );
    }

    #[test]
    fn word_kill_past_slashes_and_sortable_scrollable_table_and_ssh_completion() {
        // 1. `Option+Backspace` / `Ctrl+W` at `ls /Volumes/Projects/|` steps cleanly across slashes!
        let mut ed = Editor::new();
        ed.set_text("ls /Volumes/Projects/".to_owned());
        ed.kill_word_back();
        assert_eq!(ed.text, "ls /Volumes/");
        ed.kill_word_back();
        assert_eq!(ed.text, "ls /");
        ed.kill_word_back();
        assert_eq!(ed.text, "ls ");
        ed.kill_word_back();
        assert_eq!(ed.text, "");

        // 2. Context-aware SSH completion powered by sans-IO `blaze-ssh`!
        let home = make_temp_dir("ssh-comp");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(
            home.join(".ssh/config"),
            "Host blaze-vm\n  HostName 10.0.0.42\n  User builder\n  Port 2222\n",
        )
        .unwrap();
        let alloc = Alloc::global();
        let home_str = home.to_string_lossy().into_owned();
        let state = EngineState::new(&home_str, &[("HOME", &home_str)], alloc);
        ed.set_text("ssh bl".to_owned());
        ed.trigger_completion(&state);
        assert_eq!(ed.text, "ssh blaze-vm ");
        let _ = fs::remove_dir_all(&home);

        // 3. Scrollable row-grid-snapped table + clickable column header sort!
        let mut store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 1);
        let mut t = Table::from_col_names(&["name", "size"], alloc);
        for i in 0..24 {
            t.push_row_values(&[
                Value::string(&format!("f{i:02}.rs"), alloc),
                Value::Size(Bytes((i as i64 + 1) * 100)),
            ]);
        }
        let bid = store.record_block(
            "ls -l".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            1,
            1,
            0,
            BlockBody::Value(Value::Table(t)),
        );
        let s1 = build_scene(&LayoutInput {
            store: &store,
            editor: &ed,
            prompt: &prompt,
            viewport_w: 960.0,
            viewport_h: 800.0,
            metrics: CellMetrics::default(),
            scroll_y: 0.0,
            cursor_visible: true,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
        });
        assert!(!s1.scroll_regions.is_empty(), "expected scrollable table region");
        assert!(
            s1.hit_regions
                .iter()
                .any(|h| h.action == HitAction::SortTableColumn { block: bid, col: 1 }),
            "expected clickable sort header for `size`"
        );
        assert!(store.toggle_table_sort(bid, 1));
        assert_eq!(store.block(bid).unwrap().table_sort, Some((1, true)));
        assert!(store.toggle_table_sort(bid, 1));
        assert_eq!(store.block(bid).unwrap().table_sort, Some((1, false)));
    }
}
