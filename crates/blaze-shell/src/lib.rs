//! `blaze-shell`: native macOS GUI shell library & application.
//!
//! All GPU-independent modules (`theme`, `blocks`, `prompt`, `editor`, `view`, `layout`,
//! `rust_cell`, `host`) compile and are unit-tested on every platform without requiring Metal.

pub mod blocks;
pub mod editor;
pub mod host;
pub mod layout;
pub mod prompt;
pub mod rust_cell;
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
                .any(|t| t.text.contains("error[missing_column]")),
            "expected error kind badge in scene"
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
        });
        assert!(scene_expanded.total_content_height > h_before_expand);
        assert!(scene_expanded.text_runs.iter().any(|t| t.text.contains("row 0.meta")));
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
}
