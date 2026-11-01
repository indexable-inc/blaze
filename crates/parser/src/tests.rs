mod prefix_entries;
mod top_entries;

use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use expect_test::expect_file;

use crate::{Edition, LexedStr, TopEntryPoint};

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[rustfmt::skip]
#[path = "../test_data/generated/runner.rs"]
mod runner;

fn infer_edition(file_path: &Path) -> Edition {
    let file_content = std::fs::read_to_string(file_path).unwrap();
    if let Some(edition) = file_content.strip_prefix("//@ edition: ") {
        edition[..4].parse().expect("invalid edition directive")
    } else {
        Edition::CURRENT
    }
}

#[test]
fn lex_ok() {
    for case in TestCase::list("lexer/ok") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let actual = lex(&case.text, infer_edition(&case.rs));
        expect_file![case.rast].assert_eq(&actual)
    }
}

#[test]
fn lex_err() {
    for case in TestCase::list("lexer/err") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let actual = lex(&case.text, infer_edition(&case.rs));
        expect_file![case.rast].assert_eq(&actual)
    }
}

fn lex(text: &str, edition: Edition) -> String {
    let lexed = LexedStr::new(edition, text);

    let mut res = String::new();
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        let text = lexed.text(i);
        let error = lexed.error(i);

        let error = error.map(|err| format!(" error: {err}")).unwrap_or_default();
        writeln!(res, "{kind:?} {text:?}{error}").unwrap();
    }
    res
}

/// The parser's own sources, as real Rust text.
fn sources() -> Vec<String> {
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    let mut out = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(fs::read_to_string(&path).unwrap());
            }
        }
    }
    assert!(out.iter().any(|it| it == include_str!("tests.rs")), "the parser's sources are found");
    out
}

/// A warm lex allocates nothing: its two columns (the tokens and the checkpoints) are the
/// worker's `u32` scratch, back on the shelf since the lex before at the size it grew to, and
/// the byte classes its `u64` scratch. A text with a lexer error is left out, whose message may
/// be formatted; nine in ten real files have none.
#[test]
fn a_warm_lex_allocates_nothing() {
    let texts = sources();
    let mut clean = 0;
    for text in &texts {
        let lexed = LexedStr::new(Edition::CURRENT, text);
        if lexed.errors().next().is_some() {
            continue;
        }
        drop(lexed);
        clean += 1;
        blaze_alloc_count::assert_at_most(0, "a warm lex of a source file", || {
            LexedStr::new(Edition::CURRENT, text)
        });
    }
    assert!(clean * 10 >= texts.len() * 9, "{clean} of {} files lex cleanly", texts.len());
}

/// A lex dropped on another thread warms that thread: its columns go to the shelf of the
/// thread that drops it, the token column on top.
#[test]
fn a_lex_dropped_on_another_thread_gives_it_the_columns() {
    let text = "fn f(a: u32) -> u32 { a + 1 }\n".repeat(1000);
    let lexed = LexedStr::new(Edition::CURRENT, &text);
    let tokens = lexed.len() + 1;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            drop(lexed);
            let first = blaze_mem::scratch::lend_u32s();
            let second = blaze_mem::scratch::lend_u32s();
            assert!(first.capacity() >= tokens, "the token column, on top");
            assert!(second.capacity() >= tokens.div_ceil(16), "the checkpoint column under it");
        });
    });
}

/// A text denser than the columns are sized for grows them by doubling, not per token: a
/// megabyte of `;` is a million tokens in columns sized for a third of that, so two doublings
/// of each, and one growth of the scratch for the classes if the thread's is smaller.
#[test]
fn a_dense_text_grows_the_columns_by_doubling() {
    let text = ";".repeat(1 << 20);
    let lexed = blaze_alloc_count::assert_at_most(7, "lexing a megabyte of `;`", || {
        LexedStr::new(Edition::CURRENT, &text)
    });
    assert_eq!(lexed.len(), 1 << 20);
}

#[test]
fn parse_ok() {
    for case in TestCase::list("parser/ok") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let (actual, errors) = parse(TopEntryPoint::SourceFile, &case.text, Edition::CURRENT);
        assert!(!errors, "errors in an OK file {}:\n{actual}", case.rs.display());
        expect_file![case.rast].assert_eq(&actual);
    }
}

#[test]
fn parse_err() {
    for case in TestCase::list("parser/err") {
        let _guard = stdx::panic_context::enter(format!("{:?}", case.rs));
        let (actual, errors) = parse(TopEntryPoint::SourceFile, &case.text, Edition::CURRENT);
        assert!(errors, "no errors in an ERR file {}:\n{actual}", case.rs.display());
        expect_file![case.rast].assert_eq(&actual)
    }
}

fn parse(entry: TopEntryPoint, text: &str, edition: Edition) -> (String, bool) {
    let lexed = LexedStr::new(edition, text);
    let input = lexed.to_input(edition);
    let output = entry.parse(&input);

    let mut buf = String::new();
    let mut errors = Vec::new();
    let mut indent = String::new();
    let mut depth = 0;
    let mut len = 0;
    lexed.intersperse_trivia(&output, &mut |step| match step {
        crate::StrStep::Token { kind, text } => {
            assert!(depth > 0);
            len += text.len();
            writeln!(buf, "{indent}{kind:?} {text:?}").unwrap();
        }
        crate::StrStep::Enter { kind } => {
            assert!(depth > 0 || len == 0);
            depth += 1;
            writeln!(buf, "{indent}{kind:?}").unwrap();
            indent.push_str("  ");
        }
        crate::StrStep::Exit => {
            assert!(depth > 0);
            depth -= 1;
            indent.pop();
            indent.pop();
        }
        crate::StrStep::Error { msg, pos } => {
            assert!(depth > 0);
            errors.push(format!("error {pos}: {msg}\n"))
        }
    });
    assert_eq!(
        len,
        text.len(),
        "didn't parse all text.\nParsed:\n{}\n\nAll:\n{}\n",
        &text[..len],
        text
    );

    for (token, msg) in lexed.errors() {
        let pos = lexed.text_start(token);
        errors.push(format!("error {pos}: {msg}\n"));
    }

    let has_errors = !errors.is_empty();
    for e in errors {
        buf.push_str(&e);
    }
    (buf, has_errors)
}

/// The tree's lines without their indentation, and the errors.
fn parse_lines(text: &str, edition: Edition) -> (Vec<String>, Vec<String>) {
    let (out, _) = parse(TopEntryPoint::SourceFile, text, edition);
    let (errors, tree): (Vec<_>, Vec<_>) =
        out.lines().map(|it| it.trim_start().to_owned()).partition(|it| it.starts_with("error "));
    (tree, errors)
}

fn has_run(lines: &[String], run: &[&str]) -> bool {
    lines.windows(run.len()).any(|window| window.iter().zip(run).all(|(a, b)| a == b))
}

const LABELED_LOOP_IN_BREAK: &str =
    "parentheses are required around this expression to avoid confusion with a labeled break expression";

/// rustc's `parse_expr_break`: the labeled loop is the value of an unlabeled `break`, with
/// `LabeledLoopInBreak` where the loop ends.
#[test]
fn a_break_of_a_labeled_loop_takes_it_as_its_value() {
    let text = "fn f() -> u32 { loop { break 'a: loop { break 'a 42; }; } }";
    let (tree, errors) = parse_lines(text, Edition::CURRENT);
    assert!(
        has_run(
            &tree,
            &[
                "BREAK_EXPR",
                "BREAK_KW \"break\"",
                "WHITESPACE \" \"",
                "LOOP_EXPR",
                "LABEL",
                "LIFETIME",
                "LIFETIME_IDENT \"'a\"",
                "COLON \":\"",
            ],
        ),
        "{tree:#?}"
    );
    assert!(
        has_run(&tree, &["BREAK_EXPR", "BREAK_KW \"break\"", "WHITESPACE \" \"", "LIFETIME"]),
        "the inner `break 'a 42` keeps its label: {tree:#?}"
    );
    let end = text.find("42; }").unwrap() + "42; }".len();
    assert_eq!(errors, [format!("error {end}: {LABELED_LOOP_IN_BREAK}")]);
}

/// Each loop and a block after the label, in every edition; a label before anything else is
/// the label's own error, not this one; a labeled break with a labeled value is no error.
#[test]
fn every_labeled_value_of_a_break_is_parenthesized() {
    for edition in Edition::iter() {
        for value in ["'a: loop {}", "'a: while x {}", "'a: for x in y {}", "'a: {}"] {
            let text = format!("fn f() {{ loop {{ break {value}; }} }}");
            let (tree, errors) = parse_lines(&text, edition);
            let end = text.find(value).unwrap() + value.len();
            assert_eq!(errors, [format!("error {end}: {LABELED_LOOP_IN_BREAK}")], "{value}");
            assert!(!tree.iter().any(|it| it == "ERROR"), "{value}: {tree:#?}");
        }
        let (_, errors) = parse_lines("fn f() { loop { break 'a: 1; } }", edition);
        assert!(errors.iter().any(|it| it.ends_with("expected a loop or block")), "{errors:?}");
        assert!(!errors.iter().any(|it| it.ends_with(LABELED_LOOP_IN_BREAK)), "{errors:?}");
        let (_, errors) =
            parse_lines("fn f() { 'b: loop { break 'b 'a: loop {}; } }", edition);
        assert_eq!(errors, Vec::<String>::new());
    }
}

/// rustc's lexer before 2021 gives a `macro_rules!` matcher `'r`, `#`, `lt` for `'r#lt` and
/// `c`, `"x"` for `c"x"` (tests/ui/lifetimes/raw/three-tokens.rs); the tree has them as tokens.
#[test]
fn a_raw_lifetime_and_a_c_string_are_split_before_2021() {
    let text = "macro_rules! m { ('r # lt) => {}; (c \"x\") => {}; }\nm!('r#lt);\nm!(c\"x\");\n";
    for edition in [Edition::Edition2015, Edition::Edition2018] {
        let (tree, errors) = parse_lines(text, edition);
        assert_eq!(errors, Vec::<String>::new(), "{edition:?}");
        assert!(
            has_run(&tree, &["L_PAREN \"(\"", "LIFETIME_IDENT \"'r\"", "POUND \"#\"", "IDENT \"lt\""]),
            "{edition:?}: {tree:#?}"
        );
        assert!(
            has_run(&tree, &["L_PAREN \"(\"", "IDENT \"c\"", "STRING \"\\\"x\\\"\""]),
            "{edition:?}: {tree:#?}"
        );
    }
    for edition in [Edition::Edition2021, Edition::Edition2024] {
        let (tree, _) = parse_lines(text, edition);
        assert!(tree.iter().any(|it| it == "LIFETIME_IDENT \"'r#lt\""), "{edition:?}");
        assert!(tree.iter().any(|it| it == "C_STRING \"c\\\"x\\\"\""), "{edition:?}");
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct TestCase {
    rs: PathBuf,
    rast: PathBuf,
    text: String,
}

impl TestCase {
    fn list(path: &'static str) -> Vec<TestCase> {
        let crate_root_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let test_data_dir = crate_root_dir.join("test_data");
        let dir = test_data_dir.join(path);

        let mut res = Vec::new();
        let read_dir = fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("can't `read_dir` {}: {err}", dir.display()));
        for file in read_dir {
            let file = file.unwrap();
            let path = file.path();
            if path.extension().unwrap_or_default() == "rs" {
                let rs = path;
                let rast = rs.with_extension("rast");
                let text = fs::read_to_string(&rs).unwrap();
                res.push(TestCase { rs, rast, text });
            }
        }
        res.sort();
        res
    }
}

#[track_caller]
fn run_and_expect_no_errors(path: &str) {
    run_and_expect_no_errors_with_edition(path, Edition::CURRENT)
}

#[track_caller]
fn run_and_expect_errors(path: &str) {
    run_and_expect_errors_with_edition(path, Edition::CURRENT)
}

#[track_caller]
fn run_and_expect_no_errors_with_edition(path: &str, edition: Edition) {
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).unwrap();
    let (actual, errors) = parse(TopEntryPoint::SourceFile, &text, edition);
    assert!(!errors, "errors in an OK file {}:\n{actual}", path.display());
    let mut p = PathBuf::from("..");
    p.push(path);
    p.set_extension("rast");
    expect_file![p].assert_eq(&actual)
}

#[track_caller]
fn run_and_expect_errors_with_edition(path: &str, edition: Edition) {
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).unwrap();
    let (actual, errors) = parse(TopEntryPoint::SourceFile, &text, edition);
    assert!(errors, "no errors in an ERR file {}:\n{actual}", path.display());
    let mut p = PathBuf::from("..");
    p.push(path);
    p.set_extension("rast");
    expect_file![p].assert_eq(&actual)
}

/// Every entry point of the grammar finishes on every token sequence: windows of the parser's
/// own sources, changed the ways `blaze-fuzz` changes bytes (truncated, a token's kind or
/// jointness changed, tokens inserted, removed, repeated or spliced from another source), and
/// made-up sequences. This is the fuzz target over `TopEntryPoint` and `PrefixEntryPoint` as a
/// property test: `fuzz/Cargo.toml`, where targets are registered, was being edited by another
/// lane, and `blaze-fuzz`'s counting allocator cannot share a test binary with this one's.
mod termination {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use blaze_ids::rng::SplitMix64;

    use crate::{Edition, Input, LexedStr, PrefixEntryPoint, Reparser, SyntaxKind, TopEntryPoint};

    /// How many changed sequences are parsed, unless `BLAZE_FUZZ_RUNS` says.
    const RUNS: usize = 2_000;
    /// The longest window taken of a source.
    const WINDOW: usize = 256;
    /// How long all the runs may take before one is taken to be stuck.
    const DEADLINE: Duration = Duration::from_secs(300);

    const TOP: [TopEntryPoint; 7] = [
        TopEntryPoint::SourceFile,
        TopEntryPoint::MacroStmts,
        TopEntryPoint::MacroItems,
        TopEntryPoint::Pattern,
        TopEntryPoint::Type,
        TopEntryPoint::Expr,
        TopEntryPoint::MetaItem,
    ];
    const PREFIX: [PrefixEntryPoint; 10] = [
        PrefixEntryPoint::Vis,
        PrefixEntryPoint::Block,
        PrefixEntryPoint::Stmt,
        PrefixEntryPoint::Pat,
        PrefixEntryPoint::PatTop,
        PrefixEntryPoint::Ty,
        PrefixEntryPoint::Expr,
        PrefixEntryPoint::Path,
        PrefixEntryPoint::Item,
        PrefixEntryPoint::MetaItem,
    ];

    /// A token as `Input::token` gives it: kind, contextual kind, edition, joint, fragment
    /// start.
    type Token = (SyntaxKind, SyntaxKind, Edition, bool, bool);

    /// What tokens may be: every kind the lexer gives the parser, and for identifiers every
    /// contextual kind.
    struct Alphabet {
        kinds: Vec<SyntaxKind>,
        contextual: Vec<SyntaxKind>,
        editions: Vec<Edition>,
    }

    impl Alphabet {
        fn new() -> Alphabet {
            let kinds: Vec<SyntaxKind> = (SyntaxKind::EOF as u16 + 1..=SyntaxKind::SHEBANG as u16)
                .map(SyntaxKind::from)
                .filter(|&kind| !kind.is_trivia())
                .collect();
            // The binder keywords are numbered after the nodes and are contextual only: the
            // lexer hands them to the parser as an identifier's contextual kind, never as a kind.
            let mut contextual: Vec<SyntaxKind> = kinds
                .iter()
                .copied()
                .chain([SyntaxKind::UNWRAP_BINDER_KW, SyntaxKind::WRAP_BINDER_KW])
                .filter(|&kind| Edition::iter().any(|edition| kind.is_contextual_keyword(edition)))
                .collect();
            contextual.push(SyntaxKind::IDENT);
            Alphabet { kinds, contextual, editions: Edition::iter().collect() }
        }

        fn token(&self, random: &mut SplitMix64, edition: Edition) -> Token {
            let kind = self.kinds[random.index(self.kinds.len())];
            let contextual = match kind {
                SyntaxKind::IDENT => self.contextual[random.index(self.contextual.len())],
                _ => SyntaxKind::EOF,
            };
            (kind, contextual, edition, random.index(2) == 0, random.index(16) == 0)
        }

        /// Changes `tokens` in one of the ways a damaged or a made-up input differs from a
        /// written one.
        fn change(
            &self,
            random: &mut SplitMix64,
            tokens: &mut Vec<Token>,
            known: &[Vec<Token>],
            edition: Edition,
        ) {
            let at = random.index(tokens.len());
            match random.index(8) {
                0 => tokens.truncate(at),
                1 if !tokens.is_empty() => tokens[at].3 ^= true,
                2 if !tokens.is_empty() => tokens[at] = self.token(random, edition),
                3 => tokens.insert(at, self.token(random, edition)),
                4 if !tokens.is_empty() => _ = tokens.remove(at),
                // An opening or closing delimiter repeated: deep nesting, or many closers.
                5 => {
                    let delimiter = [
                        SyntaxKind::L_CURLY,
                        SyntaxKind::L_PAREN,
                        SyntaxKind::L_BRACK,
                        SyntaxKind::L_ANGLE,
                        SyntaxKind::R_CURLY,
                        SyntaxKind::R_PAREN,
                        SyntaxKind::R_BRACK,
                        SyntaxKind::R_ANGLE,
                    ][random.index(8)];
                    let token = (delimiter, SyntaxKind::EOF, edition, false, false);
                    let count = 1 + random.index(64);
                    tokens.splice(at..at, std::iter::repeat_n(token, count));
                }
                6 => {
                    let other = &known[random.index(known.len())];
                    let from = random.index(other.len());
                    let to = (from + random.index(WINDOW)).min(other.len());
                    tokens.splice(at.., other[from..to].iter().copied());
                }
                _ => {
                    for _ in 0..random.index(16) {
                        let token = self.token(random, edition);
                        tokens.insert(random.index(tokens.len() + 1), token);
                    }
                }
            }
        }
    }

    fn fill(input: &mut Input, tokens: &[Token]) {
        input.clear();
        for (i, &(kind, contextual, edition, joint, fragment)) in tokens.iter().enumerate() {
            match kind {
                SyntaxKind::IDENT => input.push_ident(contextual, edition),
                _ => input.push(kind, edition),
            }
            if joint {
                input.was_joint();
            }
            if fragment {
                input.mark_fragment_start(i);
            }
        }
    }

    /// Parses `input` with every entry point, full and as an outline.
    fn parse_everywhere(input: &Input) {
        for entry in TOP {
            let _guard = stdx::panic_context::enter(format!("{entry:?}"));
            entry.parse(input);
            entry.parse_outline(input);
        }
        for entry in PREFIX {
            let _guard = stdx::panic_context::enter(format!("{entry:?}"));
            entry.parse(input);
        }
        let _guard = stdx::panic_context::enter("Reparser::top_level_item".to_owned());
        Reparser::top_level_item().parse(input);
    }

    fn run(done: &AtomicUsize) {
        let alphabet = Alphabet::new();
        let known: Vec<Vec<Token>> = super::sources()
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let edition = alphabet.editions[i % alphabet.editions.len()];
                let input = LexedStr::new(edition, text).to_input(edition);
                (0..input.len()).map(|i| input.token(i)).collect()
            })
            .collect();
        let runs = std::env::var("BLAZE_FUZZ_RUNS").ok().and_then(|it| it.parse().ok());
        let mut random = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        let mut input = Input::with_capacity(4 * WINDOW);
        for run in 0..runs.unwrap_or(RUNS) {
            let edition = alphabet.editions[random.index(alphabet.editions.len())];
            let mut tokens = match run % 4 {
                // Made up: no source under it.
                0 => (0..random.index(64)).map(|_| alphabet.token(&mut random, edition)).collect(),
                _ => {
                    let source = &known[run % known.len()];
                    let from = random.index(source.len());
                    let to = (from + 1 + random.index(WINDOW)).min(source.len());
                    source[from..to].to_vec()
                }
            };
            for _ in 0..=random.index(4) {
                alphabet.change(&mut random, &mut tokens, &known, edition);
            }
            let _guard = stdx::panic_context::enter(format!("run {run}: {tokens:?}"));
            fill(&mut input, &tokens);
            parse_everywhere(&input);
            done.store(run + 1, Ordering::Relaxed);
        }
    }

    #[test]
    fn every_entry_point_finishes_on_every_input() {
        let done = Arc::new(AtomicUsize::new(0));
        let (finished, wait) = crossbeam_channel::unbounded();
        let worker = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                run(&done);
                _ = finished.send(());
            })
        };
        match wait.recv_timeout(DEADLINE) {
            Ok(()) => worker.join().unwrap(),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => match worker.join() {
                Ok(()) => unreachable!("the runs end by sending"),
                Err(panic) => std::panic::resume_unwind(panic),
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => panic!(
                "run {} did not finish within {DEADLINE:?}: the grammar loops without starting \
                 a node, making an error or consuming a token",
                done.load(Ordering::Relaxed)
            ),
        }
    }
}

/// A warm parse takes its event list and its forward-parent list from the thread's scratch
/// (`event::lend_events`, `event::ForwardParents`), so of the general-purpose allocator it asks
/// only for the output's words, which live until the tree is built.
mod warm_parse {
    use crate::{Edition, LexedStr, TopEntryPoint};

    #[test]
    fn a_warm_parse_allocates_nothing() {
        for text in super::sources() {
            let lexed = LexedStr::new(Edition::CURRENT, &text);
            let input = lexed.to_input(Edition::CURRENT);
            drop(TopEntryPoint::SourceFile.parse(&input));
            drop(TopEntryPoint::SourceFile.parse_outline(&input));
            blaze_alloc_count::assert_at_most(0, "a warm parse of one of the parser's sources", || {
                TopEntryPoint::SourceFile.parse(&input)
            });
            blaze_alloc_count::assert_at_most(0, "a warm outline of one of the parser's sources", || {
                TopEntryPoint::SourceFile.parse_outline(&input)
            });
        }
    }

    /// The thread keeps the larger of two event lists: a small parse after a large one leaves
    /// the large one warm.
    #[test]
    fn a_small_parse_keeps_the_large_list_warm() {
        let large = "fn f() { let x = (1, 2, 3); x.0 + x.1 * x.2; }\n".repeat(200);
        let small = "fn g() {}";
        let large = LexedStr::new(Edition::CURRENT, &large).to_input(Edition::CURRENT);
        let small = LexedStr::new(Edition::CURRENT, small).to_input(Edition::CURRENT);
        drop(TopEntryPoint::SourceFile.parse(&large));
        drop(TopEntryPoint::SourceFile.parse(&small));
        blaze_alloc_count::assert_at_most(0, "a large parse after a small one", || {
            TopEntryPoint::SourceFile.parse(&large)
        });
    }
}
