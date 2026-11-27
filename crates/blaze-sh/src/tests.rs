//! End-to-end sans-IO tests and `blaze-sim` property tests for `blaze-sh`.

use std::collections::BTreeMap;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use blaze_own::{AString, AVec, Alloc};
use blaze_sim::{Broken, Config, check};

use crate::{
    Answer, Bytes, DateTime, DirEntry, EngineState, ErrorKind, EvalOutcome, ExternalResult,
    FileKind, FsOp, Machine, Nanos, ProcEntry, Request, RustEvalResult, ShellError, Span, Step,
    StreamChunk, SysSnapshot, Value, highlight, is_incomplete,
};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// Scripted in-memory host for testing every sans-IO request without touching real OS IO.
struct FakeHost {
    files: BTreeMap<StdString, StdVec<u8>>,
    dirs: BTreeMap<StdString, StdVec<(StdString, FileKind, i64)>>,
    now: DateTime,
    streams: StdVec<StdString>,
    rust_calls: StdVec<(StdVec<StdString>, StdString, StdString)>,
    rm_ops: StdVec<(StdString, bool, bool, bool)>,
    fork_count: usize,
}

impl FakeHost {
    fn new() -> Self {
        let mut files = BTreeMap::new();
        files.insert(
            "/work/data.json".into(),
            br#"[{"name":"alpha","n":10},{"name":"beta","n":25},{"name":"gamma","n":5}]"#.to_vec(),
        );
        files.insert(
            "/work/Cargo.toml".into(),
            b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n".to_vec(),
        );
        files.insert(
            "/work/items.csv".into(),
            b"name,qty,price\napple,3,1.5\nbanana,10,0.5\ncherry,2,4.0\n".to_vec(),
        );
        files.insert("/work/hello.txt".into(), b"line1\nline2\nline3\n".to_vec());

        let mut dirs = BTreeMap::new();
        dirs.insert(
            "/work".into(),
            std::vec![
                ("Cargo.toml".into(), FileKind::File, 45),
                ("big.bin".into(), FileKind::File, 2_500_000),
                ("data.json".into(), FileKind::File, 74),
                ("src".into(), FileKind::Dir, 128),
                (".hidden".into(), FileKind::File, 12),
            ],
        );
        dirs.insert(
            "/work/src".into(),
            std::vec![("main.rs".into(), FileKind::File, 4096)],
        );

        Self {
            files,
            dirs,
            now: DateTime { utc_ns: 1_732_500_000_000_000_000, offset_secs: 0 },
            streams: StdVec::new(),
            rust_calls: StdVec::new(),
            rm_ops: StdVec::new(),
            fork_count: 0,
        }
    }

    fn answer(&mut self, req: Request<'static>, a: Alloc<'static>) -> Answer<'static> {
        match req {
            Request::ReadDir { path, show_hidden } => {
                let p = path.as_str();
                if let Some(entries) = self.dirs.get(p) {
                    let mut out = AVec::new_in(a);
                    for (name, kind, size) in entries {
                        if !show_hidden && name.starts_with('.') {
                            continue;
                        }
                        out.push(DirEntry {
                            name: AString::from_str_in(name, a),
                            kind: *kind,
                            analysis: None,
                            size: Bytes(*size),
                            modified: self.now,
                            readonly: false,
                        });
                    }
                    Answer::DirEntries(out)
                } else {
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, "directory not found", a))
                }
            }
            Request::Stat { path } => {
                let p = path.as_str();
                if let Some(bytes) = self.files.get(p) {
                    Answer::Stat(DirEntry {
                        name: AString::from_str_in(p, a),
                        kind: FileKind::File,
                        analysis: None,
                        size: Bytes(bytes.len() as i64),
                        modified: self.now,
                        readonly: false,
                    })
                } else if self.dirs.contains_key(p) {
                    Answer::Stat(DirEntry {
                        name: AString::from_str_in(p, a),
                        kind: FileKind::Dir,
                        analysis: None,
                        size: Bytes(0),
                        modified: self.now,
                        readonly: false,
                    })
                } else {
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, "path not found", a))
                }
            }
            Request::ReadFile { path } => {
                if let Some(bytes) = self.files.get(path.as_str()) {
                    let mut out = AVec::with_capacity_in(bytes.len(), a);
                    out.extend_from_slice(bytes);
                    Answer::FileBytes(out)
                } else {
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, "file not found", a))
                }
            }
            Request::WriteFile { path, bytes, append } => {
                let entry = self.files.entry(StdString::from(path.as_str())).or_default();
                if !append {
                    entry.clear();
                }
                entry.extend_from_slice(bytes.as_slice());
                Answer::Ok
            }
            Request::FsOp(op) => {
                match op {
                    FsOp::Mkdir { path, .. } => {
                        self.dirs.entry(StdString::from(path.as_str())).or_default();
                    }
                    FsOp::Touch { path } => {
                        self.files.entry(StdString::from(path.as_str())).or_default();
                    }
                    FsOp::Rm { path, recursive, force, permanent } => {
                        self.rm_ops.push((
                            StdString::from(path.as_str()),
                            recursive,
                            force,
                            permanent,
                        ));
                        let existed = self.files.remove(path.as_str()).is_some()
                            || self.dirs.remove(path.as_str()).is_some();
                        if !existed && !force {
                            return Answer::Err(ShellError::new(
                                ErrorKind::Io,
                                Span::ZERO,
                                "cannot remove missing path",
                                a,
                            ));
                        }
                    }
                    FsOp::Cp { src, dst, .. } => {
                        if let Some(bytes) = self.files.get(src.as_str()).cloned() {
                            self.files.insert(StdString::from(dst.as_str()), bytes);
                        } else {
                            return Answer::Err(ShellError::new(
                                ErrorKind::Io,
                                Span::ZERO,
                                "copy source missing",
                                a,
                            ));
                        }
                    }
                    FsOp::Mv { src, dst } => {
                        if let Some(bytes) = self.files.remove(src.as_str()) {
                            self.files.insert(StdString::from(dst.as_str()), bytes);
                        } else {
                            return Answer::Err(ShellError::new(
                                ErrorKind::Io,
                                Span::ZERO,
                                "move source missing",
                                a,
                            ));
                        }
                    }
                }
                Answer::Ok
            }
            Request::ChangeDir { path } => {
                if self.dirs.contains_key(path.as_str()) {
                    Answer::ChangedDir(path)
                } else {
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, "no such directory", a))
                }
            }
            Request::Which { name } => {
                if name.as_str() == "git" {
                    Answer::FoundPath(Some(AString::from_str_in("/usr/bin/git", a)))
                } else {
                    Answer::FoundPath(None)
                }
            }
            Request::Now => Answer::Now(self.now),
            Request::Sleep { duration } => {
                self.now.utc_ns = self.now.utc_ns.saturating_add(duration.0);
                Answer::Ok
            }
            Request::SysInfo => Answer::Sys(SysSnapshot {
                host: AString::from_str_in("blaze-mac", a),
                os: AString::from_str_in("macos", a),
                arch: AString::from_str_in("aarch64", a),
                cpu_count: 16,
                mem_total: Bytes(64 << 30),
                mem_used: Bytes(16 << 30),
                uptime_ns: 3_600_000_000_000,
            }),
            Request::ProcessList => {
                let mut procs = AVec::new_in(a);
                procs.push(ProcEntry {
                    pid: 100,
                    ppid: 1,
                    name: AString::from_str_in("blaze-shell", a),
                    status: AString::from_str_in("running", a),
                    cpu_pct: 1.5,
                    mem: Bytes(48 << 20),
                });
                Answer::Processes(procs)
            }
            Request::SpawnExternal { program, args, stdin, mode, .. } => {
                let mut out = AVec::new_in(a);
                if program.as_str() == "upper" {
                    if let Some(data) = stdin {
                        for &b in data.as_slice() {
                            out.push(b.to_ascii_uppercase());
                        }
                    }
                } else {
                    out.extend_from_slice(program.as_bytes());
                    for arg in args.iter() {
                        out.push(b' ');
                        out.extend_from_slice(arg.as_bytes());
                    }
                }
                Answer::External(ExternalResult {
                    exit_code: 0,
                    duration_ns: 1_250_000,
                    stdout: out,
                    stderr: AVec::new_in(a),
                    chunks: AVec::new_in(a),
                    interactive: matches!(mode, crate::ExternalMode::Interactive),
                })
            }
            Request::EvalRust { prior_cells, code, input_json, .. } => {
                let priors: StdVec<StdString> =
                    prior_cells.iter().map(|c| StdString::from(c.as_ref())).collect();
                self.rust_calls.push((
                    priors,
                    StdString::from(code.as_str()),
                    StdString::from(input_json.as_str()),
                ));
                let json = if code.as_str().contains("table") {
                    r#"[{"k":"x","v":1},{"k":"y","v":2}]"#
                } else {
                    "42"
                };
                Answer::Rust(RustEvalResult {
                    ok: true,
                    output_json: Some(AString::from_str_in(json, a)),
                    stdout: AString::new_in(a),
                    stderr: AString::new_in(a),
                    compile_ms: 8,
                    run_ms: 1,
                })
            }
            Request::ForkSession { label } => {
                self.fork_count += 1;
                let name = match label {
                    Some(l) => l,
                    None => AString::from_str_in("fork-1", a),
                };
                Answer::Forked(name)
            }
        }
    }

    fn run(&mut self, src: &str, state: EngineState<'static>) -> EvalOutcome<'static> {
        let a = state.alloc();
        let mut m = Machine::new(src, state);
        loop {
            match m.step() {
                Step::Done(out) => return out,
                Step::Need(req) => {
                    let ans = self.answer(req, a);
                    m.answer(ans);
                }
                Step::Stream(chunk) => match chunk {
                    StreamChunk::Text { text, .. } => {
                        self.streams.push(StdString::from(text.as_str()));
                    }
                    StreamChunk::Value(v) => {
                        self.streams.push(StdString::from(v.to_display_string(a).as_str()));
                    }
                },
            }
        }
    }
}

fn base_state() -> EngineState<'static> {
    EngineState::new(
        "/work",
        &[("HOME", "/work"), ("USER", "blaze"), ("PATH", "/usr/bin")],
        alloc(),
    )
}

#[test]
fn arithmetic_precedence_and_units() {
    let mut host = FakeHost::new();
    let st = base_state();

    assert_eq!(host.run("1 + 2 * 3", st.fork()).value, Value::Int(7));
    assert_eq!(host.run("(1 + 2) * 3", st.fork()).value, Value::Int(9));
    assert_eq!(host.run("2 ** 3 ** 2", st.fork()).value, Value::Int(512));
    assert_eq!(host.run("10 // 3", st.fork()).value, Value::Int(3));
    assert_eq!(host.run("10 mod 3", st.fork()).value, Value::Int(1));
    assert_eq!(host.run("1mb + 500kb", st.fork()).value, Value::Size(Bytes(1_500_000)));
    assert_eq!(host.run("2sec + 500ms", st.fork()).value, Value::Duration(Nanos(2_500_000_000)));
    assert_eq!(host.run("1.5mb > 1mb", st.fork()).value, Value::Bool(true));
}

#[test]
fn strings_interpolation_and_raw() {
    let mut host = FakeHost::new();
    let st = base_state();
    let out = host.run(r#"let who = "blaze"; "hello ($who) (1 + 2)!""#, st);
    assert_eq!(out.value, Value::string("hello blaze 3!", alloc()));
    assert_eq!(
        host.run(r#"'no ($interp) here'"#, base_state()).value,
        Value::string("no ($interp) here", alloc())
    );
    assert_eq!(
        host.run(r##"r#'raw "quotes"'#"##, base_state()).value,
        Value::string(r#"raw "quotes""#, alloc())
    );
}

#[test]
fn cow_fork_and_transactional_rollback() {
    let mut host = FakeHost::new();
    let st0 = base_state();
    let r1 = host.run("let x = 10; mut y = 20; $y += 5", st0);
    assert_eq!(r1.exit_code, 0);

    // Fork at r1's state: two independent branches mutate `$y` and `cwd`.
    let branch_a = host.run("$y = 100; cd src; $y", r1.state.fork());
    let branch_b = host.run("$y = 999; $y", r1.state.fork());
    assert_eq!(branch_a.value, Value::Int(100));
    assert_eq!(branch_a.state.cwd.as_ref(), "/work/src");
    assert_eq!(branch_b.value, Value::Int(999));
    assert_eq!(branch_b.state.cwd.as_ref(), "/work");
    // Original snapshot untouched!
    assert_eq!(r1.state.get_var("y").unwrap().value, Value::Int(25));
    assert_eq!(r1.state.cwd.as_ref(), "/work");

    // Immutability check + transactional rollback on error.
    let err = host.run("$y = 777; $x = 5", r1.state.fork());
    assert_ne!(err.exit_code, 0);
    assert!(matches!(err.value, Value::Error(ref e) if e.kind == ErrorKind::Immutable));
    // `$y` rolled back to 25 because the line failed!
    assert_eq!(err.state.get_var("y").unwrap().value, Value::Int(25));
}

#[test]
fn pipelines_tables_ls_where_sort_open() {
    let mut host = FakeHost::new();
    let st = base_state();

    // `ls | where size > 1mb | get name`
    let r = host.run("ls | where size > 1mb | get name", st.fork());
    assert_eq!(r.exit_code, 0, "got {:?}", r.value);
    match r.value {
        Value::List(l) => {
            assert_eq!(l.len(), 1);
            assert_eq!(l.get(0), Some(&Value::string("big.bin", alloc())));
        }
        other => panic!("expected list, got {other:?}"),
    }

    // `open data.json | where n >= 10 | sort-by n -r | get name`
    let r2 = host.run("open data.json | where n >= 10 | sort-by n -r | get name", st.fork());
    assert_eq!(r2.exit_code, 0, "got {:?}", r2.value);
    match r2.value {
        Value::List(l) => {
            assert_eq!(l.len(), 2);
            assert_eq!(l.get(0), Some(&Value::string("beta", alloc())));
            assert_eq!(l.get(1), Some(&Value::string("alpha", alloc())));
        }
        other => panic!("expected list, got {other:?}"),
    }

    // `open Cargo.toml | get package.name`
    let r3 = host.run("open Cargo.toml | get package.name", st.fork());
    assert_eq!(r3.value, Value::string("demo", alloc()));

    // `open items.csv | select name qty | length`
    let r4 = host.run("open items.csv | select name qty | length", st.fork());
    assert_eq!(r4.value, Value::Int(3));
}

#[test]
fn closures_control_flow_defs_and_try_catch() {
    let mut host = FakeHost::new();
    let st = base_state();

    let r = host.run("[1 2 3 4 5] | each {|x| $x * $x } | math sum", st.fork());
    assert_eq!(r.value, Value::Int(55));

    let r2 = host.run("[1 2 3 4] | reduce -f 10 {|it, acc| $acc + $it }", st.fork());
    assert_eq!(r2.value, Value::Int(20));

    let r3 = host.run(
        "def double [n: int, --add(-a): int = 0] { $n * 2 + $add }; double 7 -a 3",
        st.fork(),
    );
    assert_eq!(r3.value, Value::Int(17));

    let r4 = host.run(
        r#"try { open does-not-exist.txt } catch {|e| "caught: ($e.kind)" }"#,
        st.fork(),
    );
    assert_eq!(r4.value, Value::string("caught: io", alloc()));

    let r5 = host.run("match 5 { 1..=3 => 'low', 4..=10 => 'mid', _ => 'high' }", st.fork());
    assert_eq!(r5.value, Value::string("mid", alloc()));

    let r6 = host.run("mut s = 0; for i in 1..=5 { $s += $i }; $s", st.fork());
    assert_eq!(r6.value, Value::Int(15));
}

#[test]
fn strings_parse_formats_and_builtins() {
    let mut host = FakeHost::new();
    let st = base_state();

    assert_eq!(
        host.run(r#""  hello world  " | str trim | str upcase"#, st.fork()).value,
        Value::string("HELLO WORLD", alloc())
    );
    let parsed = host.run(r#""alice 42" | parse "{user} {age}""#, st.fork());
    assert_eq!(parsed.exit_code, 0, "got {:?}", parsed.value);
    assert!(matches!(parsed.value, Value::Table(ref t) if t.nrows() == 1 && t.ncols() == 2));

    let table_lit = host.run("[[a b]; [1 2] [3 4]] | get b | math avg", st.fork());
    assert_eq!(table_lit.value, Value::Float(3.0));

    let roundtrip = host.run("{a: 1, b: [2 3]} | to json | from json | get b.1", st.fork());
    assert_eq!(roundtrip.value, Value::Int(3));

    let help_tbl = host.run("help", st.fork());
    assert!(matches!(help_tbl.value, Value::Table(ref t) if t.nrows() > 20));
}

#[test]
fn rust_cell_and_fork_preserve_session_history() {
    let mut host = FakeHost::new();
    let st = base_state();

    let r1 = host.run("rust { fn helper(x: i64) -> i64 { x + 1 } }", st);
    assert_eq!(r1.exit_code, 0, "got {:?}", r1.value);
    assert_eq!(r1.state.rust_cells.len(), 1);

    // Fork and run a second cell: it sees prior cell #1, while a parallel fork does not see #2.
    let r2 = host.run(":rs table()", r1.state.fork());
    assert_eq!(r2.exit_code, 0, "got {:?}", r2.value);
    assert!(matches!(r2.value, Value::Table(ref t) if t.nrows() == 2));
    assert_eq!(r2.state.rust_cells.len(), 2);
    assert_eq!(r1.state.rust_cells.len(), 1);
    assert_eq!(host.rust_calls.len(), 2);
    assert_eq!(host.rust_calls[1].0.len(), 1);

    let r3 = host.run("fork exp-1", r2.state);
    assert_eq!(r3.value, Value::string("exp-1", alloc()));
    assert_eq!(host.fork_count, 1);
}

#[test]
fn highlight_and_incomplete_detection() {
    let a = alloc();
    let spans = highlight(r#"ls -la | where size > 1mb # comment"#, a);
    assert!(!spans.is_empty());
    assert!(is_incomplete("if true {"));
    assert!(is_incomplete("ls |"));
    assert!(is_incomplete("rust { fn f() {"));
    assert!(!is_incomplete("ls | take 5"));
    assert!(!is_incomplete("rust { 1 + 2 }"));
}

/// Property test (`blaze-sim`): random/hostile UTF-8 snippets never panic in `highlight`,
/// `is_incomplete`, `parse`, or `Machine` (and always stay within fuel).
#[test]
fn hostile_input_never_panics_and_stays_bounded() {
    const TOKENS: &[&str] = &[
        "let ", "mut ", "def ", "if ", "else ", "match ", "for ", "in ", "while ", "try ",
        "catch ", "rust { ", ":rs ", "| ", "; ", "\n", "{", "}", "[", "]", "(", ")", "\"", "'",
        "r#'", "'#", "$x", "$in", ".a", "?.0", "123", "1.5mb", "50ms", "==", "=~", "++", "..<",
        "->", "=>", "# hi\n", "where ", "ls ", "-a ", "--raw ", "??", "^git ",
    ];
    check(Config::cases(256), |tape| {
        let len = tape.int_in(1..=16usize, "pieces");
        let mut src = StdString::new();
        for _ in 0..len {
            let idx = tape.below(TOKENS.len() as u64, "tok") as usize;
            src.push_str(TOKENS[idx]);
        }
        let a = alloc();
        let _ = highlight(&src, a);
        let _ = is_incomplete(&src);
        let mut host = FakeHost::new();
        let st = base_state();
        let mut m = Machine::new_with_fuel(&src, st, 2_000);
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 64 {
                return Err(Broken::Oracle("machine exceeded 64 host steps on small snippet"));
            }
            match m.step() {
                Step::Done(_) => break,
                Step::Need(req) => {
                    let ans = host.answer(req, a);
                    m.answer(ans);
                }
                Step::Stream(_) => {}
            }
        }
        Ok(())
    });
}

#[test]
fn sleep_builtin_and_nushell_duration_units() {
    let mut host = FakeHost::new();
    let st = base_state();
    let before = host.now.utc_ns;
    let out = host.run("sleep 15sec", st.fork());
    assert_eq!(out.value, Value::Nothing);
    assert_eq!(host.now.utc_ns - before, 15_000_000_000);

    let out2 = host.run("sleep 250ms", st.fork());
    assert_eq!(out2.value, Value::Nothing);
    assert_eq!(host.now.utc_ns - before, 15_250_000_000);

    // Format durations cleanly without spaces (`15sec`, `500ms`).
    assert_eq!(
        host.run("15sec", st.fork()).value.to_display_string(alloc()).as_str(),
        "15sec"
    );
    assert_eq!(
        host.run("2m + 30secs", st.fork()).value,
        Value::Duration(Nanos(150_000_000_000))
    );
}

#[test]
fn external_command_returns_structured_timed_output() {
    let mut host = FakeHost::new();
    let st = base_state();
    let res = host.run("(\"hello\" | upper).stdout", st.fork());
    assert_eq!(res.value, Value::string("HELLO", alloc()));
    let code = host.run("(\"hello\" | upper).exit_code", st.fork());
    assert_eq!(code.value, Value::Int(0));
    let events = host.run("(\"hello\" | upper).events", st.fork());
    match events.value {
        Value::Table(t) => {
            assert_eq!(t.nrows(), 1);
            assert_eq!(t.col_index("at"), Some(0));
            assert_eq!(t.col_index("stream"), Some(1));
            assert_eq!(t.col_index("text"), Some(2));
        }
        other => panic!("expected events table, got {other:?}"),
    }
    let piped = host.run("\"hi\" | upper | str downcase", st.fork());
    assert_eq!(piped.value, Value::string("hi", alloc()));
}

#[test]
fn rm_defaults_to_trash_and_parses_permanent_flag() {
    let mut host = FakeHost::new();
    let st = base_state();

    let r1 = host.run("rm hello.txt", st.fork());
    assert_eq!(r1.exit_code, 0, "got {:?}", r1.value);
    assert_eq!(
        host.rm_ops.last(),
        Some(&(StdString::from("/work/hello.txt"), false, false, false))
    );

    let r2 = host.run("rm --permanent data.json", st.fork());
    assert_eq!(r2.exit_code, 0, "got {:?}", r2.value);
    assert_eq!(
        host.rm_ops.last(),
        Some(&(StdString::from("/work/data.json"), false, false, true))
    );

    let r3 = host.run("rm -P Cargo.toml", st.fork());
    assert_eq!(r3.exit_code, 0, "got {:?}", r3.value);
    assert_eq!(
        host.rm_ops.last(),
        Some(&(StdString::from("/work/Cargo.toml"), false, false, true))
    );

    let r4 = host.run("rm -rfP src", st.fork());
    assert_eq!(r4.exit_code, 0, "got {:?}", r4.value);
    assert_eq!(
        host.rm_ops.last(),
        Some(&(StdString::from("/work/src"), true, true, true))
    );

    // Spec documents `-P` and `--permanent`.
    let spec = crate::builtins::find_spec("rm").expect("rm spec");
    assert!(spec.flags.contains(&"-P"));
    assert!(spec.flags.contains(&"--permanent"));
}

#[test]
fn tree_builtin_builds_hierarchical_dataframe() {
    let mut host = FakeHost::new();
    // Extend FakeHost with a nested directory under `/work/src/sub` to test 3 levels and connectors.
    host.dirs.insert(
        "/work/src".into(),
        std::vec![
            ("main.rs".into(), FileKind::File, 4096),
            ("sub".into(), FileKind::Dir, 64),
        ],
    );
    host.dirs.insert(
        "/work/src/sub".into(),
        std::vec![("deep.rs".into(), FileKind::File, 512)],
    );
    let st = base_state();

    // Spec registered with all documented flags.
    let spec = crate::builtins::find_spec("tree").expect("tree spec");
    assert!(spec.flags.contains(&"-L"));
    assert!(spec.flags.contains(&"--depth"));
    assert!(spec.flags.contains(&"-a"));
    assert!(spec.flags.contains(&"--all"));
    assert!(spec.flags.contains(&"-d"));
    assert!(spec.flags.contains(&"--dirs-only"));

    // Default `tree` (depth 3, dotfiles skipped):
    // /work -> src (dir), big.bin, Cargo.toml, data.json
    // /work/src -> sub (dir), main.rs
    // /work/src/sub -> deep.rs
    let res = host.run("tree", st.fork());
    assert_eq!(res.exit_code, 0, "got {:?}", res.value);
    let Value::Table(tbl) = res.value else {
        panic!("expected table from `tree`, got {:?}", res.value);
    };
    assert!(!tbl.default_grid);
    let col_names: StdVec<&str> =
        (0..tbl.ncols()).map(|c| tbl.columns.get(c).unwrap().as_ref()).collect();
    assert_eq!(
        col_names,
        std::vec!["name", "size", "modified", "type", "path", "tree_prefix", "depth"]
    );
    assert_eq!(tbl.col_display(0), crate::ColDisplay::TreeEntry);
    assert_eq!(tbl.col_display(1), crate::ColDisplay::FileSize);
    assert_eq!(tbl.col_display(2), crate::ColDisplay::HumanTime);
    assert!(!tbl.is_col_hidden(0));
    assert!(!tbl.is_col_hidden(1));
    assert!(!tbl.is_col_hidden(2));
    assert!(tbl.is_col_hidden(3)); // type
    assert!(tbl.is_col_hidden(4)); // path
    assert!(tbl.is_col_hidden(5)); // tree_prefix
    assert!(tbl.is_col_hidden(6)); // depth

    let rows: StdVec<(&str, &str, &str, i64)> = (0..tbl.nrows())
        .map(|r| {
            let name = tbl.get_by_name(r, "name").and_then(Value::as_str).unwrap();
            let path = tbl.get_by_name(r, "path").and_then(Value::as_str).unwrap();
            let prefix = tbl.get_by_name(r, "tree_prefix").and_then(Value::as_str).unwrap();
            let depth = match tbl.get_by_name(r, "depth") {
                Some(Value::Int(d)) => *d,
                other => panic!("expected int depth, got {other:?}"),
            };
            (name, path, prefix, depth)
        })
        .collect();
    assert_eq!(
        rows,
        std::vec![
            ("src", "src", "├── ", 0),
            ("sub", "src/sub", "│   ├── ", 1),
            ("deep.rs", "src/sub/deep.rs", "│   │   └── ", 2),
            ("main.rs", "src/main.rs", "│   └── ", 1),
            ("big.bin", "big.bin", "├── ", 0),
            ("Cargo.toml", "Cargo.toml", "├── ", 0),
            ("data.json", "data.json", "└── ", 0),
        ]
    );

    // `tree -L 1` limits traversal to depth 0 entries only.
    let res_l1 = host.run("tree -L 1", st.fork());
    assert_eq!(res_l1.exit_code, 0, "got {:?}", res_l1.value);
    let Value::Table(tbl_l1) = res_l1.value else {
        panic!("expected table from `tree -L 1`, got {:?}", res_l1.value);
    };
    assert_eq!(tbl_l1.nrows(), 4);
    assert_eq!(tbl_l1.get_by_name(0, "name").and_then(Value::as_str), Some("src"));
    assert_eq!(tbl_l1.get_by_name(0, "tree_prefix").and_then(Value::as_str), Some("├── "));
    assert_eq!(tbl_l1.get_by_name(3, "name").and_then(Value::as_str), Some("data.json"));
    assert_eq!(tbl_l1.get_by_name(3, "tree_prefix").and_then(Value::as_str), Some("└── "));

    // `tree -d` returns directories only with proper last-child connectors (`└── `, `    └── `).
    let res_d = host.run("tree -d", st.fork());
    assert_eq!(res_d.exit_code, 0, "got {:?}", res_d.value);
    let Value::Table(tbl_d) = res_d.value else {
        panic!("expected table from `tree -d`, got {:?}", res_d.value);
    };
    assert_eq!(tbl_d.nrows(), 2);
    assert_eq!(tbl_d.get_by_name(0, "name").and_then(Value::as_str), Some("src"));
    assert_eq!(tbl_d.get_by_name(0, "tree_prefix").and_then(Value::as_str), Some("└── "));
    assert_eq!(tbl_d.get_by_name(1, "name").and_then(Value::as_str), Some("sub"));
    assert_eq!(tbl_d.get_by_name(1, "path").and_then(Value::as_str), Some("src/sub"));
    assert_eq!(tbl_d.get_by_name(1, "tree_prefix").and_then(Value::as_str), Some("    └── "));

    // `tree -a -L 1` includes dotfiles (`.hidden`).
    let res_all = host.run("tree -a -L 1", st.fork());
    assert_eq!(res_all.exit_code, 0, "got {:?}", res_all.value);
    let Value::Table(tbl_all) = res_all.value else {
        panic!("expected table from `tree -a -L 1`, got {:?}", res_all.value);
    };
    assert_eq!(tbl_all.nrows(), 5);
    assert_eq!(tbl_all.get_by_name(1, "name").and_then(Value::as_str), Some(".hidden"));

    // Positional path argument (`tree src --depth 1`).
    let res_sub = host.run("tree src --depth 1", st.fork());
    assert_eq!(res_sub.exit_code, 0, "got {:?}", res_sub.value);
    let Value::Table(tbl_sub) = res_sub.value else {
        panic!("expected table from `tree src --depth 1`, got {:?}", res_sub.value);
    };
    assert_eq!(tbl_sub.nrows(), 2);
    assert_eq!(tbl_sub.get_by_name(0, "path").and_then(Value::as_str), Some("sub"));
    assert_eq!(tbl_sub.get_by_name(1, "path").and_then(Value::as_str), Some("main.rs"));

    // Pipeline composability: `tree | where depth == 0` and `tree | sort-by size -r`.
    let pipe_depth = host.run("tree | where depth == 0 | get name", st.fork());
    assert_eq!(pipe_depth.exit_code, 0, "got {:?}", pipe_depth.value);
    match pipe_depth.value {
        Value::List(l) => {
            assert_eq!(l.len(), 4);
            assert_eq!(l.get(0), Some(&Value::string("src", alloc())));
            assert_eq!(l.get(3), Some(&Value::string("data.json", alloc())));
        }
        other => panic!("expected list from `tree | where depth == 0 | get name`, got {other:?}"),
    }

    let pipe_sort = host.run("tree | sort-by size -r | first | get name", st.fork());
    assert_eq!(pipe_sort.exit_code, 0, "got {:?}", pipe_sort.value);
    assert_eq!(pipe_sort.value, Value::string("big.bin", alloc()));
}

#[test]
fn dot_subject_queries_previous_output_pipeline_and_rows() {
    use blaze_own::Share as _;
    let mut host = FakeHost::new();
    let mut st = EngineState::new("/work", &[], alloc());

    // 1. Seed `$_` with a Cargo.toml-like nested record and query `.package.version` directly!
    let r0 = host.run(
        r#"{ package: { name: "hello-world", version: "0.1.0", edition: "blaze-2026.11" } }"#,
        st.fork(),
    );
    assert_eq!(r0.exit_code, 0);
    st.set_var("_", r0.value.share(), false);

    let r1 = host.run(".package.version", st.fork());
    assert_eq!(r1.exit_code, 0, "got {:?}", r1.value);
    assert_eq!(r1.value, Value::string("0.1.0", alloc()));

    let r2 = host.run(".package | keys | length", st.fork());
    assert_eq!(r2.exit_code, 0, "got {:?}", r2.value);
    assert_eq!(r2.value, Value::Int(3));

    let r3 = host.run(".", st.fork());
    assert_eq!(r3.exit_code, 0);
    assert_eq!(r3.value, r0.value);

    // 2. Works inside pipelines (`| .package.name`) and expressions (`.package.name ++ "@" ++ .package.version`).
    let r4 = host.run(r#"{ package: { name: "blaze", version: "2.0.0" } } | .package.name"#, st.fork());
    assert_eq!(r4.exit_code, 0, "got {:?}", r4.value);
    assert_eq!(r4.value, Value::string("blaze", alloc()));

    let r5 = host.run(r#".package.name ++ "@" ++ .package.version"#, st.fork());
    assert_eq!(r5.exit_code, 0, "got {:?}", r5.value);
    assert_eq!(r5.value, Value::string("hello-world@0.1.0", alloc()));

    // 3. Works inside `where` predicates (`.v > 10`) and optional access (`?.missing?`).
    let r6 = host.run("[{v: 5}, {v: 20}, {v: 30}] | where .v > 10 | length", st.fork());
    assert_eq!(r6.exit_code, 0, "got {:?}", r6.value);
    assert_eq!(r6.value, Value::Int(2));

    let r7 = host.run(".package?.missing?", st.fork());
    assert_eq!(r7.exit_code, 0, "got {:?}", r7.value);
    assert_eq!(r7.value, Value::Nothing);
}

#[test]
fn schema_infers_nested_lockfile_types_patterns_and_histograms() {
    use blaze_own::Share as _;
    let mut host = FakeHost::new();
    let mut st = EngineState::new("/work", &[], alloc());

    let lock_expr = r#"{
        version: 4,
        package: [
            { name: "adler2", version: "2.0.1", source: "registry+https://github.com/rust-lang/crates.io-index", size: 10 },
            { name: "ahash", version: "0.8.12", source: "registry+https://github.com/rust-lang/crates.io-index", size: 25 },
            { name: "blaze-core", version: "0.0.0", source: null, size: 100 },
            { name: "blaze-sh", version: "0.0.0", source: null, size: 200 }
        ]
    }"#;
    let seed = host.run(lock_expr, st.fork());
    assert_eq!(seed.exit_code, 0, "got {:?}", seed.value);
    st.set_var("_", seed.value.share(), false);

    // Bare `schema` inspects `$_` automatically!
    let res = host.run("schema", st.fork());
    assert_eq!(res.exit_code, 0, "got {:?}", res.value);
    let Value::Record(rec) = res.value else {
        panic!("expected schema record, got {:?}", res.value);
    };
    assert_eq!(rec.get(crate::schema::SCHEMA_MARKER_KEY), Some(&Value::Bool(true)));
    assert_eq!(rec.get("kind").and_then(Value::as_str), Some("record"));

    let Some(Value::Table(fields)) = rec.get("fields") else {
        panic!("expected fields table");
    };
    let find_row = |path: &str| -> usize {
        (0..fields.nrows())
            .find(|&i| fields.get_by_name(i, "path").and_then(Value::as_str) == Some(path))
            .unwrap_or_else(|| panic!("missing schema path {path}"))
    };
    let v_row = find_row(".package.version");
    assert_eq!(fields.get_by_name(v_row, "pattern").and_then(Value::as_str), Some("x.y.z"));
    let src_row = find_row(".package.source");
    assert_eq!(fields.get_by_name(src_row, "type").and_then(Value::as_str), Some("string?"));
    assert_eq!(fields.get_by_name(src_row, "present"), Some(&Value::Int(2)));
    assert_eq!(fields.get_by_name(src_row, "total"), Some(&Value::Int(4)));
    let sz_row = find_row(".package.size");
    assert!(
        fields
            .get_by_name(sz_row, "hist")
            .and_then(Value::as_str)
            .is_some_and(|h| h.starts_with("num:")),
        "numeric column must emit histogram"
    );

    // `describe` alias works identically via pipe.
    let res2 = host.run("$_ | describe | .kind", st.fork());
    assert_eq!(res2.exit_code, 0, "got {:?}", res2.value);
    assert_eq!(res2.value, Value::string("record", alloc()));
}

#[test]
fn modern_data_shell_syntax_leading_pipe_brackets_projection_files_and_verbs() {
    use blaze_own::Share as _;
    let mut host = FakeHost::new();
    host.files.insert(
        "/work/Cargo.lock".into(),
        b"version = 4\n\n[[package]]\nname = \"adler2\"\nversion = \"2.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n[[package]]\nname = \"blaze-sh\"\nversion = \"0.0.0\"\ndependencies = [\"cfg-if\", \"once_cell\"]\n\n[[package]]\nname = \"blaze-core\"\nversion = \"0.0.0\"\ndependencies = [\"cfg-if\", \"libc\"]\n".to_vec(),
    );
    let mut st = EngineState::new("/work", &[], alloc());

    // 1. Bare structured data file + flatten+skip-nulls `[]` + `uniq` + `sort` + `count`!
    let deps = host.run("Cargo.lock.package.dependencies[] | uniq | sort", st.fork());
    assert_eq!(deps.exit_code, 0, "got {:?}", deps.value);
    match &deps.value {
        Value::List(l) => {
            assert_eq!(l.len(), 3);
            assert_eq!(l.get(0), Some(&Value::string("cfg-if", alloc())));
            assert_eq!(l.get(1), Some(&Value::string("libc", alloc())));
            assert_eq!(l.get(2), Some(&Value::string("once_cell", alloc())));
        }
        other => panic!("expected flattened deduped list, got {other:?}"),
    }

    // 2. Bare standalone data file (`Cargo.lock`, `(Cargo.toml)`) & chained `Cargo.toml.package.version`!
    let bare_lock = host.run("Cargo.lock", st.fork());
    assert_eq!(bare_lock.exit_code, 0, "got {:?}", bare_lock.value);
    assert!(matches!(bare_lock.value, Value::Record(_)), "expected Record, got {:?}", bare_lock.value);

    let bare_paren = host.run("(Cargo.toml).package.version", st.fork());
    assert_eq!(bare_paren.exit_code, 0, "got {:?}", bare_paren.value);
    assert_eq!(bare_paren.value, Value::string("0.1.0", alloc()));

    let ver = host.run("Cargo.toml.package.version", st.fork());
    assert_eq!(ver.exit_code, 0, "got {:?}", ver.value);
    assert_eq!(ver.value, Value::string("0.1.0", alloc()));

    // 3. Inline filter `[source == null]` + projection `.{name, v: .version}` + negative index `[-1]` + slice `[..2]`!
    let proj = host.run("Cargo.lock.package[source == null].{name, v: .version} | sort -name", st.fork());
    assert_eq!(proj.exit_code, 0, "got {:?}", proj.value);
    let Value::Table(tbl) = &proj.value else {
        panic!("expected projected table, got {:?}", proj.value);
    };
    assert_eq!(tbl.nrows(), 2);
    assert_eq!(tbl.get_by_name(0, "name").and_then(Value::as_str), Some("blaze-sh"));
    assert_eq!(tbl.get_by_name(0, "v").and_then(Value::as_str), Some("0.0.0"));
    assert_eq!(tbl.get_by_name(1, "name").and_then(Value::as_str), Some("blaze-core"));

    let last_pkg = host.run("Cargo.lock.package[-1].name", st.fork());
    assert_eq!(last_pkg.exit_code, 0, "got {:?}", last_pkg.value);
    assert_eq!(last_pkg.value, Value::string("blaze-core", alloc()));

    // `..` is inclusive (`0..=2` -> 3 items) and `..<` is exclusive (`0..2` -> 2 items).
    let first_two = host.run("Cargo.lock.package[..<2] | count", st.fork());
    assert_eq!(first_two.exit_code, 0, "got {:?}", first_two.value);
    assert_eq!(first_two.value, Value::Int(2));

    let first_three = host.run("Cargo.lock.package[..2] | count", st.fork());
    assert_eq!(first_three.exit_code, 0, "got {:?}", first_three.value);
    assert_eq!(first_three.value, Value::Int(3));

    // 4. Leading `|` continues from `$_`!
    st.set_var("_", proj.value.share(), false);
    let lead = host.run("| find \"core\" | count", st.fork());
    assert_eq!(lead.exit_code, 0, "got {:?}", lead.value);
    assert_eq!(lead.value, Value::Int(1));

    // 5. `freq`, `top`, and zero-param `map`!
    let freq = host.run("Cargo.lock.package.version | freq", st.fork());
    assert_eq!(freq.exit_code, 0, "got {:?}", freq.value);
    let Value::Table(ftbl) = &freq.value else {
        panic!("expected freq table, got {:?}", freq.value);
    };
    assert_eq!(ftbl.get_by_name(0, "value").and_then(Value::as_str), Some("0.0.0"));
    assert_eq!(ftbl.get_by_name(0, "count"), Some(&Value::Int(2)));

    let top = host.run("data.json | top 1 n | .0.name", st.fork());
    assert_eq!(top.exit_code, 0, "got {:?}", top.value);
    assert_eq!(top.value, Value::string("beta", alloc()));

    let mapped = host.run(r#"Cargo.lock.package | map $"(.name)@(.version)" | .0"#, st.fork());
    assert_eq!(mapped.exit_code, 0, "got {:?}", mapped.value);
    assert_eq!(mapped.value, Value::string("adler2@2.0.1", alloc()));
}
