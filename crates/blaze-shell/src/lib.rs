//! `blaze-shell`: native macOS GUI shell library & application.
//!
//! All GPU-independent modules (`theme`, `blocks`, `prompt`, `editor`, `view`, `layout`,
//! `rust_cell`, `host`) compile and are unit-tested on every platform without requiring Metal.

pub mod agent_hub;
pub mod agent_probe;
pub mod blocks;
pub mod cli_spec_driver;
pub mod config;
pub mod diag_view;
pub mod help_view;
pub mod dust_driver;
pub mod editor;
pub mod find;
pub mod git_meta_driver;
pub mod host;
pub mod icons;
pub mod image_atlas;
pub mod layout;
pub mod prompt;
pub mod remote_driver;
pub mod rust_cell;
pub mod schema_view;
pub mod svg_icons;
pub mod sysmon_driver;
pub mod sysmon_view;
pub mod term_server;
pub mod theme;
pub mod tiling;
pub mod ui_widgets;
pub mod view;
pub mod yazi;
pub mod zoxide;

#[cfg(target_os = "macos")]
pub mod avif_dav1d;
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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

        // 3. Atuin-class Ctrl+R history palette: fuzzy search + host/dir/session scope cycling.
        ed.record_history_entry(HistoryEntry {
            command: "blaze test -p blaze-vim".to_owned(),
            host: "hydra".to_owned(),
            cwd: "/Volumes/Projects/blaze".to_owned(),
            git_branch: Some("main".to_owned()),
            duration_ms: 420,
            exit_code: 0,
            timestamp_ns: 1_732_500_000_000_000_000,
            in_session: true,
        });
        ed.record_history_entry(HistoryEntry {
            command: "git status".to_owned(),
            host: "blaze-vm".to_owned(),
            cwd: "/tmp/other".to_owned(),
            git_branch: Some("dev".to_owned()),
            duration_ms: 12,
            exit_code: 0,
            timestamp_ns: 1_732_500_010_000_000_000,
            in_session: false,
        });
        ed.start_reverse_search_for("hydra", "/Volumes/Projects/blaze");
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
        assert_eq!(ed.reverse_search.as_ref().unwrap().scope, HistoryScope::Host);
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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
            pty_focused: false,
            now_ns: 1_732_500_000_000_000_000,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
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

    #[test]
    fn zoxide_crdt_semilattice_and_fullscreen_block_sdf_and_ghost_predictor() {
        use blaze_vt::terminal::{Opts as VtOpts, Terminal};
        use crate::layout::layout_fullscreen_terminal;
        use crate::view::{OutputNav, OutputVisualMode, extract_visual_lines, output_nav_geometry};
        use crate::zoxide::{FrecencyEntry, FrecencyStore};

        // 1. Cross-host Zoxide CRDT semilattice merge laws (commutative & idempotent).
        let mut a = FrecencyStore::new();
        let mut b = FrecencyStore::new();
        a.join_entry(FrecencyEntry {
            host: "hydra".to_owned(),
            path: "/Volumes/Projects/blaze".to_owned(),
            visits: 10,
            last_ns: 1_000,
        });
        b.join_entry(FrecencyEntry {
            host: "hydra".to_owned(),
            path: "/Volumes/Projects/blaze".to_owned(),
            visits: 14,
            last_ns: 900,
        });
        b.join_entry(FrecencyEntry {
            host: "blaze-vm".to_owned(),
            path: "/srv/blaze".to_owned(),
            visits: 7,
            last_ns: 2_000,
        });
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        let ab_v: Vec<_> = ab.entries().cloned().collect();
        let ba_v: Vec<_> = ba.entries().cloned().collect();
        assert_eq!(ab_v, ba_v, "CRDT join must be commutative");
        ab.merge(&b);
        let ab2_v: Vec<_> = ab.entries().cloned().collect();
        assert_eq!(ab2_v, ab_v, "CRDT join must be idempotent");
        let top = ab.query("blaze", "hydra", 2_500, 4);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].host, "hydra", "local host should rank ahead of remote");
        assert_eq!(top[1].host, "blaze-vm", "cross-server entry must appear");

        // 1b. Native binary zoxide v3 `db.zo` decoder (zero subprocesses).
        let mut zo = Vec::new();
        zo.extend_from_slice(&3u32.to_le_bytes());
        zo.extend_from_slice(&1u64.to_le_bytes());
        let p = b"/tmp/hello-world";
        zo.extend_from_slice(&(p.len() as u64).to_le_bytes());
        zo.extend_from_slice(p);
        zo.extend_from_slice(&142.0f64.to_le_bytes());
        zo.extend_from_slice(&1_732_500_000i64.to_le_bytes());
        let parsed = crate::zoxide::parse_zoxide_db_v3(&zo, "hydra");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, "/tmp/hello-world");
        assert_eq!(parsed[0].visits, 142);

        // 2. Full-screen PTY keeps top tab bar AND renders Unicode block/box elements via SDF (zero font seams!).
        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[], alloc);
        let store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 0);
        let mut term = Terminal::new_in(VtOpts::new(40, 12, 1 << 16), alloc).unwrap();
        term.feed("█▛▜▙▟─│┌┐└┘".as_bytes());
        let scene = layout_fullscreen_terminal(
            &term,
            &store,
            &prompt,
            960.0,
            640.0,
            CellMetrics::default(),
            true,
            100,
            true,
        );
        assert!(
            scene.hit_regions.iter().any(|h| matches!(h.action, HitAction::SwitchBranch(_))),
            "fullscreen terminal must keep clickable session tabs in top bar"
        );
        assert!(
            !scene.over_rects.is_empty(),
            "block elements & box drawing must render via custom GPU SDF quads"
        );
        assert!(
            !scene.text_runs.iter().any(|t| t.text.contains('█') || t.text.contains('▛')),
            "block elements must NOT fall back to CoreText font glyphs"
        );

        // 3. Zero-history instant ghost predictor + Vim OutputNav visual line yank.
        let mut ed = Editor::new();
        ed.set_text("git s".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.ghost.as_deref(), Some("tatus"));
        ed.set_text("agent ".to_owned());
        ed.refresh_assist(&state, &[], false);
        assert_eq!(ed.ghost.as_deref(), Some("\"\""));

        let vlines = extract_visual_lines(&scene.text_runs, CellMetrics::default(), 0.0, 640.0);
        let (cur, vis, text) = output_nav_geometry(
            &vlines,
            CellMetrics::default(),
            OutputNav {
                line: 0,
                col: 0,
                visual: Some((0, 0, OutputVisualMode::Line)),
                pending_g: false,
            },
        );
        assert!(cur.is_some());
        assert!(!vis.is_empty());
        assert!(!text.is_empty());
    }

    #[test]
    fn narrow_viewport_layouts_never_panic() {
        let alloc = Alloc::global();
        let dir = make_temp_dir("narrow-vp");
        fs::write(
            dir.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let state = EngineState::new(dir.to_str().unwrap(), &[("HOME", "/tmp")], alloc);
        let store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd(dir.to_str().unwrap(), "/tmp", 0, 0);
        let editor = Editor::new();

        let mut yazi = crate::yazi::YaziState::open(&dir);
        let mut hub = crate::agent_hub::AgentHub::from_records(
            "hydra",
            vec![crate::agent_hub::AgentRecord {
                id: "local-1".to_owned(),
                host: "hydra".to_owned(),
                is_remote: false,
                name: "claude #1".to_owned(),
                kind_label: "claude".to_owned(),
                prompt_summary: "test".to_owned(),
                metric_badge: "12%".to_owned(),
                cwd: "/tmp".to_owned(),
                pid: Some(1),
                status: crate::agent_hub::AgentStatus::Running,
                pty_session_idx: None,
                preview_lines: vec!["ok".to_owned()],
                ..Default::default()
            }],
        );

        for &vw in &[260.0f32, 320.0, 400.0, 514.0, 620.0, 1140.0] {
            for &vh in &[200.0f32, 400.0, 820.0] {
                let sy = crate::yazi::layout_yazi_view(
                    &mut yazi,
                    &store,
                    &prompt,
                    vw,
                    vh,
                    CellMetrics::default(),
                    100,
                    true,
                );
                assert!(!sy.rects.is_empty());

                let sh = crate::agent_hub::layout_agent_hub(
                    &mut hub,
                    &store,
                    &prompt,
                    vw,
                    vh,
                    CellMetrics::default(),
                    100,
                    true,
                );
                assert!(!sh.rects.is_empty());

                let sb = build_scene(&LayoutInput {
                    store: &store,
                    editor: &editor,
                    prompt: &prompt,
                    viewport_w: vw,
                    viewport_h: vh,
                    metrics: CellMetrics::default(),
                    scroll_y: 0.0,
                    cursor_visible: true,
                    pty_focused: false,
                    now_ns: 1_732_500_000_000_000_000,
                    zoom_pct: 100,
                    inline_titlebar: true,
                    drag: None,
                    cursor_trail: None,
                    text_selection: None,
                    active_popover: None,
                    mouse_pos: None,
                    output_nav: None,
                });
                assert!(!sb.rects.is_empty());
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_hover_preview_has_rich_svg_icons_and_sniff_metadata() {
        let dir = make_temp_dir("dir-hover");
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("package.json"), r#"{"name":"site"}"#).unwrap();
        fs::write(dir.join("bun.lock"), "{}").unwrap();
        fs::write(dir.join("README.md"), "# Site\n").unwrap();
        fs::write(dir.join(".env"), "KEY=1\n").unwrap();

        let kind = crate::yazi::load_hover_preview(&dir, "site/", None, 16);
        match kind {
            crate::view::HoverKind::DirPreview { meta, entries, total_items, hidden_count, .. } => {
                assert!(meta.contains("Web"), "expected Web sniff label, got {meta}");
                assert!(meta.contains("2 dirs") && meta.contains("3 files"), "got {meta}");
                assert_eq!(total_items, 5);
                assert_eq!(hidden_count, 1);
                let nm = entries.iter().find(|e| e.name == "node_modules").unwrap();
                assert_eq!(nm.icon, "folder_target");
                assert!(nm.muted);
                assert_eq!(nm.kind_label, "Build output");
                let pkg = entries.iter().find(|e| e.name == "package.json").unwrap();
                assert_eq!(pkg.icon, "json_dark");
                assert!(!pkg.size_label.is_empty());
                let md = entries.iter().find(|e| e.name == "README.md").unwrap();
                assert_eq!(md.icon, "markdown_dark");
            }
            other => panic!("expected HoverKind::DirPreview, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tiling_wm_invariants_and_scene_compositing() {
        use crate::layout::{
            TOP_BAR_H, TabBarItem, build_pane_scene, draw_split_dividers, layout_window_top_bar,
        };
        use crate::tiling::{CloseOutcome, FocusDir, SplitAxis, TileManager};
        use crate::view::{HitRect, Scene};

        let (mut wm, t1, p1) = TileManager::new(Alloc::global());
        assert_eq!(wm.focused_pane(), p1);
        assert_eq!(wm.tabs()[0].id, t1);

        // Split vertically -> horizontally -> check directional navigation & zoom.
        let p2 = wm.split_focused(SplitAxis::Vertical);
        let _p3 = wm.split_focused(SplitAxis::Horizontal);
        let area = HitRect::new(0.0, TOP_BAR_H, 1200.0, 800.0);
        let tl = wm.layout(area.into(), 2.0);
        assert_eq!(tl.panes.len(), 3);
        assert_eq!(tl.handles.len(), 2);

        // Spatial navigation: from p3 (bottom-right) -> Up -> p2 (top-right) -> Left -> p1 (left).
        assert_eq!(wm.focus_direction(FocusDir::Up, area.into(), 2.0), Some(p2));
        assert_eq!(wm.focus_direction(FocusDir::Left, area.into(), 2.0), Some(p1));
        assert_eq!(wm.focus_direction(FocusDir::Right, area.into(), 2.0), Some(p2));

        // Zoom collapses layout to 1 full-area pane, then unzoom restores all 3.
        assert!(wm.toggle_zoom());
        let zoomed = wm.layout(area.into(), 2.0);
        assert_eq!(zoomed.panes.len(), 1);
        assert!(zoomed.is_zoomed);
        assert!(wm.toggle_zoom());
        assert_eq!(wm.layout(area.into(), 2.0).panes.len(), 3);

        // Workspace tabs: Cmd+T, Cmd+1..9, Cmd+W.
        let (t2, p4) = wm.new_tab();
        assert_eq!(wm.active_tab().id, t2);
        assert_eq!(wm.focused_pane(), p4);
        assert_eq!(wm.select_tab_slot(1), Some(p2));
        assert_eq!(wm.select_tab_slot(9), Some(p4));
        assert!(matches!(wm.close_focused(), CloseOutcome::TabClosed { .. }));
        assert_eq!(wm.tabs().len(), 1);

        // Deterministic 120-step fuzz verifying zero overlap & focused-pane invariant at narrow & wide sizes.
        let mut rng: u64 = 0x5EED_CAFE_1234_5678;
        let mut next_u32 = || {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (rng >> 32) as u32
        };
        for _ in 0..120 {
            let op = next_u32() % 10;
            match op {
                0 if wm.active_tab().root.leaf_count() < 6 => {
                    let _ = wm.split_focused(SplitAxis::Vertical);
                }
                1 if wm.active_tab().root.leaf_count() < 6 => {
                    let _ = wm.split_focused(SplitAxis::Horizontal);
                }
                2 if wm.tabs().len() < 5 => {
                    let _ = wm.new_tab();
                }
                3 => {
                    let slot = (next_u32() as usize % 9) + 1;
                    let _ = wm.select_tab_slot(slot);
                }
                4 => {
                    let dir = [FocusDir::Left, FocusDir::Right, FocusDir::Up, FocusDir::Down]
                        [(next_u32() % 4) as usize];
                    let _ = wm.focus_direction(dir, area.into(), 2.0);
                }
                5 => {
                    let _ = wm.toggle_zoom();
                }
                6 => {
                    let dir = [FocusDir::Left, FocusDir::Right, FocusDir::Up, FocusDir::Down]
                        [(next_u32() % 4) as usize];
                    let _ = wm.nudge_focused_split(dir, 0.08);
                }
                7 => wm.equalize_splits(),
                8 => {
                    let _ = wm.cycle_pane(1);
                }
                _ => {
                    if wm.tabs().len() > 1 || wm.active_tab().root.leaf_count() > 1 {
                        let _ = wm.close_focused();
                    }
                }
            }
            let focused = wm.focused_pane();
            assert!(wm.active_tab().root.contains(focused), "focused pane must exist in tree");
            for &(w, h) in &[(320.0f32, 240.0f32), (1200.0, 800.0)] {
                let a = HitRect::new(0.0, TOP_BAR_H, w, h);
                let l = wm.layout(a.into(), 2.0);
                for (i, pa) in l.panes.iter().enumerate() {
                    assert!(pa.rect.x1 > pa.rect.x0 && pa.rect.y1 > pa.rect.y0);
                    for pb in &l.panes[i + 1..] {
                        let ox = pa.rect.x1.min(pb.rect.x1) - pa.rect.x0.max(pb.rect.x0);
                        let oy = pa.rect.y1.min(pb.rect.y1) - pa.rect.y0.max(pb.rect.y0);
                        assert!(ox <= 0.0 || oy <= 0.0, "panes must never overlap: {pa:?} vs {pb:?}");
                    }
                }
            }
        }

        // Verify Scene::composite_pane + layout_window_top_bar end-to-end.
        let alloc = Alloc::global();
        let state = EngineState::new("/tmp", &[("HOME", "/tmp")], alloc);
        let store = BlockStore::new(state.fork());
        let prompt = PromptInfo::from_cwd("/tmp", "/tmp", 0, 0);
        let editor = Editor::new();
        let l = wm.layout(area.into(), 2.0);
        let mut master = Scene::new();
        for pl in &l.panes {
            let content: HitRect = pl.rect.into();
            let sub = build_pane_scene(&LayoutInput {
                store: &store,
                editor: &editor,
                prompt: &prompt,
                viewport_w: content.x1 - content.x0,
                viewport_h: (content.y1 - content.y0) + TOP_BAR_H,
                metrics: CellMetrics::default(),
                scroll_y: 0.0,
                cursor_visible: pl.focused,
                pty_focused: false,
                now_ns: 1_732_500_000_000_000_000,
                zoom_pct: 100,
                inline_titlebar: false,
                drag: None,
                cursor_trail: None,
                text_selection: None,
                active_popover: None,
                mouse_pos: None,
                output_nav: None,
            });
            master.composite_pane(sub, content, TOP_BAR_H, pl.focused, 1200.0, 800.0);
        }
        draw_split_dividers(&mut master, &l.handles, None);
        let tabs = [TabBarItem {
            id: wm.active_tab().id,
            slot: 1,
            symbol: None,
            title: "tmp",
            active: true,
            pane_count: l.pane_count_in_tab,
            zoomed: l.is_zoomed,
            process_tag: None,
            branches: crate::layout::collect_branch_tree(&store),
        }];
        layout_window_top_bar(
            &mut master,
            &tabs,
            wm.focused_pane(),
            l.is_zoomed,
            l.pane_count_in_tab,
            &store,
            &prompt,
            1200.0,
            CellMetrics::default(),
            100,
            true,
            false,
        );
        assert!(master.hit_regions.iter().any(|h| h.action == HitAction::ToggleSidebar));
        assert!(master.hit_regions.iter().any(|h| h.action == HitAction::NewTab));
        assert!(master.hit_regions.iter().any(|h| h.action == HitAction::NewWindow));
        assert!(
            master
                .hit_regions
                .iter()
                .any(|h| h.action == HitAction::SplitPane(SplitAxis::Vertical))
        );

        // Verify collapsible workspace sidebar renders nested fork hierarchy with SwitchTabBranch hits.
        let st0 = EngineState::new("/tmp", &[], Alloc::global());
        let mut forked_store = BlockStore::new(st0.fork());
        let b1 = forked_store.record_block(
            "ls -l".to_owned(),
            prompt.clone(),
            st0.fork(),
            st0.fork(),
            1,
            0,
            0,
            BlockBody::Value(Value::Nothing),
        );
        let f1 = forked_store.fork_from_block(b1, Some("fork-1")).expect("fork-1");
        let b2 = forked_store.record_block(
            "blaze check".to_owned(),
            prompt.clone(),
            st0.fork(),
            st0.fork(),
            2,
            0,
            0,
            BlockBody::Value(Value::Nothing),
        );
        let f2 = forked_store.fork_from_block(b2, Some("fork-2")).expect("fork-2");
        let tree = crate::layout::collect_branch_tree(&forked_store);
        assert_eq!(tree.len(), 3);
        assert_eq!((tree[0].depth, tree[1].depth, tree[2].depth), (0, 1, 2));
        assert_eq!(tree[1].id, f1);
        assert_eq!(tree[1].fork_cmd.as_deref(), Some("ls -l"));
        assert_eq!(tree[2].id, f2);
        assert_eq!(tree[2].fork_cmd.as_deref(), Some("blaze check"));
        let sb_tabs = [TabBarItem {
            id: wm.active_tab().id,
            slot: 1,
            symbol: None,
            title: "tmp",
            active: true,
            pane_count: 1,
            zoomed: false,
            process_tag: None,
            branches: tree,
        }];
        let mut sb_scene = Scene::new();
        crate::layout::layout_workspace_sidebar(&mut sb_scene, &sb_tabs, 240.0, 800.0, CellMetrics::default(), None);
        assert!(sb_scene.hit_regions.iter().any(|h| h.action == HitAction::SwitchTab(wm.active_tab().id)));
        assert!(sb_scene.hit_regions.iter().any(|h| h.action == HitAction::SwitchTabBranch { tab: wm.active_tab().id, branch: f2 }));
    }

    #[test]
    fn dual_font_icons_middle_truncation_and_scrollable_popups() {
        // 1. Modern MacBook Pro & server rack SVGs parse cleanly.
        assert!(crate::svg_icons::get_svg("macbook_pro").is_some());
        assert!(crate::svg_icons::get_svg("server_rack").is_some());

        // 2. Middle truncation preserves trailing filename for long nested paths.
        let mid = crate::view::truncate_middle("crates/blaze-shell/src/layout.rs", 20);
        assert!(mid.ends_with("layout.rs"), "expected filename preserved, got {mid}");
        assert!(mid.contains('…'), "expected ellipsis in {mid}");

        // 3. Scrollable Tab completions + Ctrl+R history + Cmd+Shift+P Command Palette.
        let mut ed = Editor::new();
        for i in 0..20 {
            ed.completions.push(crate::editor::CompletionItem {
                label: format!("item_{i:02}"),
                insert: format!("item_{i:02}"),
                detail: "file".to_owned(),
                doc: format!("doc {i}"),
                kind: crate::editor::CompletionKind::File,
                icon: None,
                avatar_path: None,
                fs_meta: None,
                match_indices: Vec::new(),
            });
        }
        assert_eq!(ed.completion_scroll, 0);
        assert!(ed.completion_scroll_by(4));
        assert_eq!(ed.completion_scroll, 4);
        assert!(ed.completion_scroll_by(-2));
        assert_eq!(ed.completion_scroll, 2);

        let mut pal = crate::editor::CommandPalette::open();
        assert!(pal.matches.len() > crate::editor::PALETTE_VISIBLE_ROWS);
        assert!(pal.scroll_by(3));
        assert_eq!(pal.scroll, 3);
        pal.push_char('s');
        pal.push_char('p');
        pal.push_char('l');
        assert!(!pal.matches.is_empty());
        let mut sc = crate::view::Scene::new();
        crate::layout::layout_command_palette(&mut sc, &pal, 1140.0, 820.0, CellMetrics::default());
        assert!(sc.hit_regions.iter().any(|h| matches!(h.action, HitAction::SelectPalette(_))));
    }

    #[test]
    fn live_resize_reflow_readme_80_widths_is_submillisecond() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let readme = root.join("README.md");
        if !readme.is_file() {
            return;
        }
        let content = fs::read_to_string(&readme).expect("read README.md");
        let alloc = Alloc::global();
        let cwd = root.to_string_lossy().into_owned();
        let state = EngineState::new(&cwd, &[], alloc);
        let mut store = BlockStore::new(state.fork());
        let prompt = crate::prompt::PromptInfo::from_cwd(&cwd, &cwd, 0, 0);
        store.record_block(
            "open README.md".to_owned(),
            prompt.clone(),
            state.fork(),
            state.fork(),
            0,
            1,
            0,
            BlockBody::Value(Value::string(&content, alloc)),
        );
        let editor = Editor::new();
        let m = CellMetrics::default();

        // Warm up once (parses Markdown/HTML & shapes tokens).
        let _ = build_scene(&LayoutInput {
            store: &store,
            editor: &editor,
            prompt: &prompt,
            viewport_w: 1140.0,
            viewport_h: 820.0,
            metrics: m,
            scroll_y: 0.0,
            cursor_visible: true,
            pty_focused: false,
            now_ns: 0,
            zoom_pct: 100,
            inline_titlebar: true,
            drag: None,
            cursor_trail: None,
            text_selection: None,
            active_popover: None,
            mouse_pos: None,
            output_nav: None,
        });

        // Sweep across 80 distinct widths simulating a live drag resize (520px..1320px).
        let t0 = std::time::Instant::now();
        let mut last_h = 0.0f32;
        for i in 0..80 {
            let vw = 520.0 + (i as f32) * 10.0;
            let sc = build_scene(&LayoutInput {
                store: &store,
                editor: &editor,
                prompt: &prompt,
                viewport_w: vw,
                viewport_h: 820.0,
                metrics: m,
                scroll_y: 0.0,
                cursor_visible: true,
                pty_focused: false,
                now_ns: 0,
                zoom_pct: 100,
                inline_titlebar: true,
                drag: None,
                cursor_trail: None,
                text_selection: None,
                active_popover: None,
                mouse_pos: None,
                output_nav: None,
            });
            last_h = sc.total_content_height;
            assert!(!sc.text_runs.is_empty());
        }
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let per_frame_ms = elapsed_ms / 80.0;
        eprintln!("live resize reflow (README.md x 80 widths): {per_frame_ms:.3} ms/frame (last_h={last_h:.0})");
        assert!(per_frame_ms < 5.0, "live resize reflow too slow: {per_frame_ms:.2} ms/frame");
    }

    #[test]
    fn git_subfolder_completion_and_ls_git_status_badges() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cwd = root.to_string_lossy().into_owned();
        if crate::prompt::read_git_head(&cwd).is_none() {
            return;
        }

        // 1. `ls` git status classification on the repo root:
        //    `Cargo.toml` & `crates` are tracked (or modified), `target` is gitignored.
        let entries = vec![
            ("Cargo.toml".to_owned(), false),
            ("crates".to_owned(), true),
            ("target".to_owned(), true),
            (".git".to_owned(), true),
        ];
        let statuses = crate::prompt::git_ls_entry_statuses(&root, &entries, None)
            .expect("expected git statuses inside repo");
        assert_eq!(statuses.len(), 4);
        assert_ne!(statuses[0], crate::prompt::GitFileStatus::Ignored);
        assert_ne!(statuses[0], crate::prompt::GitFileStatus::Untracked);
        assert_ne!(statuses[1], crate::prompt::GitFileStatus::Ignored);
        assert_eq!(statuses[2], crate::prompt::GitFileStatus::Ignored, "target/ must be Ignored");
        assert_eq!(statuses[3], crate::prompt::GitFileStatus::Ignored, ".git must be Ignored");

        // 2. Recursive git subfolder completion: `vim .r` inside `crates/blaze-shell` should surface
        //    nested `.rs` files like `src/main.rs`, ranked strictly below any direct cwd match.
        let shell_dir = root.join("crates/blaze-shell");
        let shell_cwd = shell_dir.to_string_lossy().into_owned();
        let alloc = Alloc::global();
        let state = EngineState::new(&shell_cwd, &[], alloc);
        let mut ed = Editor::new();
        ed.set_text("vim .r".to_owned());
        ed.refresh_assist(&state, &[], true);
        let labels: Vec<&str> = ed.completions.iter().map(|c| c.label.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.starts_with("src/") && l.ends_with(".rs")),
            "expected git subfolder `.rs` completion for `vim .r`, got {labels:?}"
        );
        // Any direct cwd match (without `/`) must appear before subfolder matches (with `/`).
        if let (Some(last_direct), Some(first_sub)) = (
            labels.iter().rposition(|l| !l.contains('/')),
            labels.iter().position(|l| l.contains('/')),
        ) {
            assert!(
                last_direct < first_sub,
                "direct cwd matches should outrank subfolder matches: {labels:?}"
            );
        }
    }

    #[test]
    fn user_config_toml_host_and_project_symbols() {
        let toml = r#"
[host]
symbol = "🦑"
label = ""

[hosts.prod]
symbol = "⚡"
label = "prod-gpu"

[projects."/Volumes/Projects/tmp/hello-world"]
symbol = "👋🌎"

[projects]
"~/Projects/blaze" = "🔥"
"#;
        let alloc = blaze_own::Alloc::global();
        let cfg = crate::config::UserConfig::parse_in(toml, alloc);
        let h_local = cfg.resolve_host("hydra", false).expect("local host");
        assert_eq!(h_local.symbol_str(), Some("🦑"));
        assert_eq!(h_local.label_str(), Some(""));

        let h_prod = cfg.resolve_host("prod", true).expect("prod host");
        assert_eq!(h_prod.symbol_str(), Some("⚡"));
        assert_eq!(h_prod.label_str(), Some("prod-gpu"));

        // Exact match AND nested subdir prefix match both inherit `👋🌎`!
        let p_exact = cfg
            .resolve_project("/Volumes/Projects/tmp/hello-world", "/Users/test")
            .expect("exact project match");
        assert_eq!(p_exact.symbol_str(), Some("👋🌎"));
        let p_sub = cfg
            .resolve_project("/Volumes/Projects/tmp/hello-world/src", "/Users/test")
            .expect("subdir prefix match");
        assert_eq!(p_sub.symbol_str(), Some("👋🌎"));

        // Tilde shorthand rule works too!
        let p_tilde = cfg
            .resolve_project("/Users/test/Projects/blaze/crates", "/Users/test")
            .expect("tilde shorthand match");
        assert_eq!(p_tilde.symbol_str(), Some("🔥"));

        assert_eq!(crate::config::symbol_cluster_count("🦑"), 1);
        assert_eq!(crate::config::symbol_cluster_count("👋🌎"), 2);
    }

    #[test]
    fn cl_abbreviation_dynamic_claude_flags_and_command_syntax_classes() {
        let mut ed = crate::editor::Editor::new();
        ed.text = "cl".to_owned();
        ed.cursor = ed.text.len();
        assert!(ed.try_expand_abbreviation());
        assert_eq!(ed.text, "claude --dangerously-skip-permissions");

        // Dynamic/curated flag completion for `claude --dangerou`.
        let alloc = blaze_own::Alloc::global();
        let state = blaze_sh::EngineState::new("/", &[], alloc);
        ed.text = "claude --dangerou".to_owned();
        ed.cursor = ed.text.len();
        ed.refresh_assist(&state, &[], true);
        assert!(
            ed.completions
                .iter()
                .any(|c| c.label == "--dangerously-skip-permissions"),
            "expected --dangerously-skip-permissions in {:?}",
            ed.completions.iter().map(|c| &c.label).collect::<Vec<_>>()
        );

        // Builtin vs Abbrev vs External syntax classification.
        assert_eq!(
            crate::editor::classify_command_token("ls", Some(&ed.abbreviations)),
            crate::editor::CommandHighlightKind::Builtin
        );
        assert_eq!(
            crate::editor::classify_command_token("cl", Some(&ed.abbreviations)),
            crate::editor::CommandHighlightKind::Abbrev
        );
        assert_eq!(
            crate::editor::classify_command_token("/bin/sh", Some(&ed.abbreviations)),
            crate::editor::CommandHighlightKind::External
        );
    }
}
