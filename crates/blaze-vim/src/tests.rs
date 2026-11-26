//! Comprehensive unit tests and `blaze-sim` property tests for `blaze-vim`.

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, check};

use crate::{
    BuiltinObjects, ChainObjects, CursorShape, KeyCode, KeyEvent, Mode, MotionKind, ObjectScope,
    OwnedBuf, ShellObjects, Span, TextBuffer, TextObjectResolver, VimMachine, VimOpts,
};

fn test_alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn feed_str<B: TextBuffer, O: TextObjectResolver>(
    vim: &mut VimMachine<'_>,
    buf: &mut B,
    objs: &O,
    seq: &str,
) {
    let mut chars = seq.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            vim.feed(KeyEvent::key(KeyCode::Escape), buf, objs);
        } else if ch == '\x12' {
            vim.feed(KeyEvent::ctrl('r'), buf, objs);
        } else if vim.mode() == Mode::Insert {
            let ev = KeyEvent::char(ch);
            vim.feed(ev, buf, objs);
            let mut tmp = [0u8; 4];
            let s = ch.encode_utf8(&mut tmp);
            let pos = vim.cursor();
            buf.replace_range(pos, pos, s);
            vim.set_cursor(buf.text(), pos + s.len());
        } else {
            vim.feed(KeyEvent::char(ch), buf, objs);
        }
    }
}

#[test]
fn standard_motions_operators_text_objects_undo_and_dot_repeat() {
    let alloc = test_alloc();
    let mut buf = OwnedBuf::from_str_in("echo \"hello world\" | sort-by (size)", alloc);
    let mut vim = VimMachine::new(VimOpts::default(), alloc);

    // `ci"` changes inside quotes even when cursor starts before the quote!
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "ci\"blaze\x1b");
    assert_eq!(buf.as_str(), "echo \"blaze\" | sort-by (size)");
    assert_eq!(vim.mode(), Mode::Normal);
    assert_eq!(vim.cursor_shape(), CursorShape::Block);

    // `u` undoes cleanly, `Ctrl+R` redoes cleanly.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "u");
    assert_eq!(buf.as_str(), "echo \"hello world\" | sort-by (size)");
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "\x12");
    assert_eq!(buf.as_str(), "echo \"blaze\" | sort-by (size)");

    // `f(` jumps to `(`, `ci(` changes inside parens, then `0` jumps home.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "f(ci(modified\x1b");
    assert_eq!(buf.as_str(), "echo \"blaze\" | sort-by (modified)");

    // `da(` deletes balanced parens and their contents.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "da(");
    assert_eq!(buf.as_str(), "echo \"blaze\" | sort-by ");

    // `.` dot-repeat replays last change on another word!
    let mut b2 = OwnedBuf::from_str_in("alpha beta gamma", alloc);
    let mut v2 = VimMachine::new(VimOpts::default(), alloc);
    feed_str(&mut v2, &mut b2, &BuiltinObjects, "ciwONE\x1bw.");
    assert_eq!(b2.as_str(), "ONE ONE gamma");
}

#[test]
fn pluggable_shell_and_custom_text_objects() {
    let alloc = test_alloc();

    // 1. Shell pipeline stage object (`di|`, `ci|`, `da|`).
    let mut buf = OwnedBuf::from_str_in("ls | where size > 1KB | sort-by size -r", alloc);
    let mut vim = VimMachine::new(VimOpts::default(), alloc);
    // Jump into second stage (`where size > 1KB`) and delete inner pipeline stage.
    feed_str(&mut vim, &mut buf, &ShellObjects, "fwci|take 5\x1b");
    assert_eq!(buf.as_str(), "ls | take 5 | sort-by size -r");

    // `da|` removes the stage and its trailing separator cleanly.
    feed_str(&mut vim, &mut buf, &ShellObjects, "da|");
    assert_eq!(buf.as_str(), "ls | sort-by size -r");

    // 2. Path segment object (`ci/`, `di/`).
    let mut pbuf = OwnedBuf::from_str_in("crates/blaze-sh/src/lib.rs", alloc);
    let mut pvim = VimMachine::new(VimOpts::default(), alloc);
    feed_str(&mut pvim, &mut pbuf, &ShellObjects, "f/lci/blaze-vim\x1b");
    assert_eq!(pbuf.as_str(), "crates/blaze-vim/src/lib.rs");

    // 3. Comma argument object (`ci,`).
    let mut abuf = OwnedBuf::from_str_in("foo(alpha, beta, gamma)", alloc);
    let mut avim = VimMachine::new(VimOpts::default(), alloc);
    feed_str(&mut avim, &mut abuf, &ShellObjects, "fbci,BETA\x1b");
    assert_eq!(abuf.as_str(), "foo(alpha, BETA, gamma)");

    // 4. Custom domain resolver composed via `ChainObjects` (e.g. `@` tag object).
    struct AtTag;
    impl TextObjectResolver for AtTag {
        fn resolve(
            &self,
            text: &str,
            cursor: usize,
            scope: ObjectScope,
            key: char,
            _count: usize,
        ) -> Option<(Span, MotionKind)> {
            if key != '@' {
                return None;
            }
            let at = text[..=cursor.min(text.len().saturating_sub(1))].rfind('@')?;
            let mut end = at + 1;
            while end < text.len() && text.as_bytes()[end].is_ascii_alphanumeric() {
                end += 1;
            }
            let sp = match scope {
                ObjectScope::Inner => Span::new(at + 1, end),
                ObjectScope::Around => Span::new(at, end),
            };
            Some((sp, MotionKind::Charwise))
        }
    }
    let custom = ChainObjects { primary: AtTag, fallback: BuiltinObjects };
    let mut tbuf = OwnedBuf::from_str_in("ping @alice today", alloc);
    let mut tvim = VimMachine::new(VimOpts::default(), alloc);
    feed_str(&mut tvim, &mut tbuf, &custom, "f@ci@bob\x1b");
    assert_eq!(tbuf.as_str(), "ping @bob today");
}

#[test]
fn visual_multiline_registers_and_bracket_jump() {
    let alloc = test_alloc();
    let mut buf = OwnedBuf::from_str_in("line one\nline two\nline three", alloc);
    let mut vim = VimMachine::new(VimOpts::default(), alloc);

    // `yy` + `p` duplicates line one below.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "yyp");
    assert_eq!(buf.as_str(), "line one\nline one\nline two\nline three");

    // `gg2dd` deletes the first two lines (`line one` + `line one`), leaving `line two\nline three`.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "gg2dd");
    assert_eq!(buf.as_str(), "line two\nline three");

    // Visual selection `ve` + `U` uppercases word.
    feed_str(&mut vim, &mut buf, &BuiltinObjects, "0veU");
    assert_eq!(buf.as_str(), "LINE two\nline three");

    // `%` bracket jump + `d%`.
    let mut b2 = OwnedBuf::from_str_in("fn f(a, (b, c)) {}", alloc);
    let mut v2 = VimMachine::new(VimOpts::default(), alloc);
    feed_str(&mut v2, &mut b2, &BuiltinObjects, "f(d%");
    assert_eq!(b2.as_str(), "fn f {}");
}

#[test]
fn sim_random_vim_keystreams_preserve_utf8_invariants_and_bounds() {
    const KEYS: &[char] = &[
        'h', 'j', 'k', 'l', 'w', 'b', 'e', 'W', 'B', 'E', '0', '^', '$', '%', '{', '}', 'g', 'G',
        'd', 'c', 'y', 'x', 'X', 's', 'S', 'D', 'C', 'Y', 'p', 'P', 'u', '.', 'v', 'V', 'i', 'a',
        'I', 'A', 'o', 'O', 'r', 'f', 'F', 't', 'T', ';', ',', '~', 'J', '>', '<', '"', '(', '[',
        '{', '|', '/', '1', '2', '3', '\x1b', '\x12',
    ];

    check(Config::cases(160), |tape| {
        let alloc = test_alloc();
        let initial = match tape.below(4, "init") {
            0 => "ls crates/blaze-sh/src | where size > 1KB | sort-by \"size\"",
            1 => "let x = (1 + 2)\nlet y = [3, 4, 5]\nrust { x + y }",
            2 => "héllo “wörld” | café/naïve | ❯ λ",
            _ => "",
        };
        let mut buf = OwnedBuf::from_str_in(initial, alloc);
        let mut vim = VimMachine::new(
            VimOpts { initial_mode: Mode::Normal, max_undo: 32, max_count: 64 },
            alloc,
        );

        let steps = tape.int_in(16..=80usize, "steps");
        for _ in 0..steps {
            let idx = tape.below(KEYS.len() as u64, "key") as usize;
            let ch = KEYS[idx];
            if ch == '\x1b' {
                vim.feed(KeyEvent::key(KeyCode::Escape), &mut buf, &ShellObjects);
            } else if ch == '\x12' {
                vim.feed(KeyEvent::ctrl('r'), &mut buf, &ShellObjects);
            } else if vim.mode() == Mode::Insert {
                let ev = KeyEvent::char(ch);
                vim.feed(ev, &mut buf, &ShellObjects);
                let mut tmp = [0u8; 4];
                let s = ch.encode_utf8(&mut tmp);
                let pos = vim.cursor();
                buf.replace_range(pos, pos, s);
                vim.set_cursor(buf.text(), pos + s.len());
            } else {
                vim.feed(KeyEvent::char(ch), &mut buf, &ShellObjects);
            }

            // Invariants after EVERY keystroke: valid UTF-8 boundary, cursor in range, visual span valid.
            let t = buf.text();
            if vim.cursor() > t.len() {
                return Err(Broken::Oracle("cursor exceeded buffer length"));
            }
            if !t.is_char_boundary(vim.cursor()) {
                return Err(Broken::Oracle("cursor landed off UTF-8 char boundary"));
            }
            if let Some(vis) = vim.visual_selection(t) {
                if vis.start > vis.end || vis.end > t.len() {
                    return Err(Broken::Oracle("visual span out of bounds"));
                }
                if !t.is_char_boundary(vis.start) || !t.is_char_boundary(vis.end) {
                    return Err(Broken::Oracle("visual span off UTF-8 char boundary"));
                }
            }
        }
        Ok(())
    });
}
