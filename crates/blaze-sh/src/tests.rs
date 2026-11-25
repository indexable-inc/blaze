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
                    FsOp::Rm { path, force, .. } => {
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
                    stdout: out,
                    stderr: AVec::new_in(a),
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
