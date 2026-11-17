use super::*;

const SOURCE: &str = "\
//! A crate.
// fn search() { a comment's }
trait Finds {
    fn search(&self) -> u32;
}

fn search<T: Copy>(of: [T; 2]) -> T { of[0] }

fn main() {
    let found = search([1, 2]);
    println!(\"{found}\");
}
";

fn run(n: u32, build: u64, wall_ms: f64, daemon_ms: f64) -> Run {
    Run {
        run: n,
        build,
        wall_ms,
        daemon_ms,
        client_ms: wall_ms - daemon_ms,
        outcome: "ok".to_owned(),
        tree: Some("t".repeat(64)),
        blaze: Some("b".repeat(64)),
        timeline: Vec::new(),
        counters: counters(&[Counter { query: "infer_body_query".to_owned(), executed: 1 }]),
        problems: Vec::new(),
    }
}

fn edit(dir: &Path, kind: Kind, function: Option<&str>, runs: u32) -> Edit {
    Edit {
        kind,
        file: Some(dir.join("main.rs")),
        function: function.map(|it| (it.to_owned(), None)),
        runs,
    }
}

fn scratch() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), SOURCE).unwrap();
    let state = dir.path().join("state");
    (dir, state)
}

#[test]
fn a_function_is_found_past_a_comment_and_a_declaration_of_its_name() {
    let f = locate(SOURCE, "search", None).unwrap();
    assert_eq!(f.line, 7);
    assert_eq!(&SOURCE[f.brace..f.brace + 9], "{ of[0] }", "past the `;` inside its parameter's type");
    assert_eq!(&SOURCE[f.name_end..f.name_end + 3], "<T:");
    assert_eq!(
        locate(SOURCE, "sear", None),
        Err(EditError::NoFunction { name: "sear".to_owned(), at: None }),
        "a name is whole"
    );
    let twice = format!("{SOURCE}\nmod m {{\n    fn main() {{}}\n}}\n");
    let error = locate(&twice, "main", None).unwrap_err();
    assert_eq!(error, EditError::Ambiguous { name: "main".to_owned(), lines: vec![9, 15] });
    assert!(error.to_string().contains("at lines 9, 15: name one as `--fn main:LINE`"), "{error}");
    assert_eq!(locate(&twice, "main", Some(15)).unwrap().line, 15);
}

#[test]
fn each_kind_of_edit_changes_what_it_says_and_each_run_differs() {
    let at = Some(("search", None));
    let body = apply(SOURCE, Kind::Body, at, 3).unwrap();
    assert!(
        body.contains("-> T { let _ = ::core::hint::black_box(3u64); of[0] }"),
        "a one-line body stays whole: {body}"
    );
    assert_eq!(body.lines().count(), SOURCE.lines().count(), "no line moves");
    assert_ne!(body, apply(SOURCE, Kind::Body, at, 4).unwrap(), "each run is another edit");
    assert_eq!(apply(SOURCE, Kind::DepBody, at, 3).unwrap(), body);

    let comment = apply(SOURCE, Kind::Comment, at, 1).unwrap();
    assert!(comment.contains("-> T { /* blaze debug edits, run 1 */ of[0] }"), "{comment}");
    let trailing = apply(SOURCE, Kind::Comment, None, 1).unwrap();
    assert!(trailing.starts_with(SOURCE) && trailing.ends_with("// blaze debug edits, run 1\n"));

    let generic = apply(SOURCE, Kind::Signature, at, 2).unwrap();
    assert!(generic.contains("fn search<'blaze_edit_2, T: Copy>(of: [T; 2])"), "{generic}");
    let plain = apply(SOURCE, Kind::Signature, Some(("main", None)), 2).unwrap();
    assert!(plain.contains("fn main<'blaze_edit_2>() {"), "{plain}");

    let item = apply(SOURCE, Kind::AddItem, None, 7).unwrap();
    assert!(item.starts_with(SOURCE) && item.contains("fn blaze_debug_edits_item_7() -> u64 {"));

    assert_eq!(apply(SOURCE, Kind::NoOp, None, 1).unwrap(), SOURCE);
    assert_eq!(
        apply(SOURCE, Kind::Body, None, 1),
        Err(EditError::NeedsFunction { kind: Kind::Body })
    );
}

/// The invariant the command is for: whatever the builds do, the file is as it was found, and
/// each build saw an edited file.
#[test]
fn the_file_is_as_it_was_found_after_runs_that_end_well() {
    let (dir, state) = scratch();
    let file = dir.path().join("main.rs");
    let mut seen = Vec::new();
    let (runs, failed, restored) = measure(&edit(dir.path(), Kind::Body, Some("search"), 3), &state, |n| {
        seen.push(std::fs::read_to_string(&file).unwrap());
        assert!(state.join(JOURNAL_PATH).exists(), "the journal is there while the file is edited");
        Ok(Built::Ran(run(n, 10 + u64::from(n), 40.0, 30.0)))
    })
    .unwrap();
    assert_eq!((runs.len(), failed, restored), (3, None, true));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);
    assert!(seen.iter().all(|it| it != SOURCE) && seen[0] != seen[1], "{seen:?}");
    assert!(!state.join(JOURNAL_PATH).exists() && !state.join(JOURNAL_BYTES).exists());
}

#[test]
fn a_failed_build_ends_the_runs_and_the_file_is_still_restored() {
    let (dir, state) = scratch();
    let file = dir.path().join("main.rs");
    let (runs, failed, restored) = measure(&edit(dir.path(), Kind::Signature, Some("main"), 5), &state, |n| {
        Ok(match n {
            1 => Built::Ran(run(1, 11, 40.0, 30.0)),
            _ => Built::Failed(Failed {
                run: n,
                what: "`blaze build` exited 101".to_owned(),
                code: Some(101),
                stderr: "error[E0308]: mismatched types\n".to_owned(),
            }),
        })
    })
    .unwrap();
    assert_eq!(runs.len(), 1, "nothing is timed past the failure");
    assert_eq!(failed.as_ref().map(|it| (it.run, it.code)), Some((2, Some(101))));
    assert!(restored);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);

    // A harness error (the history could not be read) restores it too.
    let (_, failed, _) =
        measure(&edit(dir.path(), Kind::Body, Some("main"), 2), &state, |_| Err("no history".to_owned()))
            .unwrap();
    assert_eq!(failed.map(|it| it.what), Some("no history".to_owned()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);

    // An edit that can't be made builds nothing.
    let error = measure(&edit(dir.path(), Kind::Body, Some("absent"), 2), &state, |_| {
        panic!("built for an edit that can't be made")
    })
    .unwrap_err();
    assert!(error.contains("no `fn absent` with a body"), "{error}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);
}

#[test]
fn a_panic_in_a_run_restores_the_file() {
    let (dir, state) = scratch();
    let file = dir.path().join("main.rs");
    let asked = edit(dir.path(), Kind::Body, Some("main"), 2);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        _ = measure(&asked, &state, |_| panic!("the build's driver panicked"));
    }));
    assert!(caught.is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);
}

/// A process killed while the file is edited runs no destructor: the journal is what the next
/// run restores from.
#[test]
fn a_killed_runs_edit_is_restored_from_the_journal_by_the_next() {
    let (dir, state) = scratch();
    let file = dir.path().join("main.rs");
    let held = Held::take(&state, &file).unwrap();
    held.edit("fn main() { edited }\n").unwrap();
    // Killed: no drop.
    std::mem::forget(held);
    assert_ne!(std::fs::read_to_string(&file).unwrap(), SOURCE);

    assert_eq!(recover(&state).unwrap(), Some(file.clone()));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), SOURCE);
    assert_eq!(recover(&state).unwrap(), None, "the journal is gone once it is restored");
}

#[test]
fn the_medians_are_of_the_runs_from_the_second_on() {
    let runs = [run(1, 1, 5000.0, 4900.0), run(2, 2, 40.0, 30.0), run(3, 3, 44.0, 34.0), run(4, 4, 42.0, 31.0)];
    let sum = summary(&runs).unwrap();
    assert_eq!((sum.from_run, sum.runs, sum.wall_ms, sum.daemon_ms), (2, 3, 42.0, 31.0));
    let only = summary(&runs[..1]).unwrap();
    assert_eq!((only.from_run, only.runs, only.wall_ms), (1, 1, 5000.0));
    assert_eq!(summary(&[]), None);
    assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
}

fn record_of(blaze: char, tree: char, daemon_ms: f64) -> Record {
    Record {
        version: VERSION,
        workspace: "/w".to_owned(),
        kind: Kind::Body,
        file: Some("src/main.rs".to_owned()),
        function: Some("main".to_owned()),
        blaze: blaze.to_string().repeat(64),
        tree: tree.to_string().repeat(64),
        summary: Summary { from_run: 2, runs: 4, wall_ms: daemon_ms + 8.0, daemon_ms },
        recorded_unix_s: 1,
    }
}

fn report_of(now: &Record, previous: Option<Previous>) -> Report {
    Report {
        version: VERSION,
        workspace: now.workspace.clone(),
        kind: now.kind,
        file: now.file.clone(),
        function: now.function.clone(),
        blaze: now.blaze.clone(),
        tree: now.tree.clone(),
        command: vec!["build".to_owned()],
        runs: vec![run(1, 7, 900.0, 880.0), run(2, 8, 39.0, 31.0)],
        failed: None,
        summary: Some(now.summary),
        previous,
        restored: true,
    }
}

#[test]
fn a_previous_run_of_another_binary_is_never_shown_as_the_same_pair() {
    let now = record_of('b', 't', 31.0);
    let other_edit = Record { kind: Kind::Comment, ..record_of('b', 't', 1.0) };
    let records = [record_of('a', 't', 90.0), record_of('b', 't', 35.0), other_edit];

    let same = previous(&records, &now).unwrap();
    assert!(same.same_blaze && same.same_tree, "the last of the same edit, past another edit's");
    assert_eq!(same.daemon_ms_change, -4.0);
    let said = text(&report_of(&now, Some(same)));
    assert!(
        said.contains("against the last recorded run of this binary on this tree: daemon -4.0 ms (was 35.0)"),
        "{said}"
    );

    let other = previous(&records[..1], &now).unwrap();
    assert!(!other.same_blaze && other.same_tree);
    let said = text(&report_of(&now, Some(other)));
    assert!(said.contains("was of ANOTHER BINARY (blaze aaaaaaaaaaaa, this is bbbbbbbbbbbb): not the same pair"), "{said}");
    assert!(!said.contains("ANOTHER TREE"), "{said}");

    assert_eq!(previous(&[], &now), None);
    assert!(text(&report_of(&now, None)).contains("no earlier run of this edit is recorded"));
}

#[test]
fn the_first_line_is_the_answer_or_the_failure() {
    let now = record_of('b', 't', 31.0);
    let report = report_of(&now, None);
    let said = text(&report);
    assert_eq!(
        said.lines().next().unwrap(),
        "body edit of `main` in src/main.rs: daemon 31.0 ms, wall 39.0 ms (median of runs 2..5; run 1 follows \
         whatever came before it and is not counted)"
    );
    assert!(said.contains("blaze bbbbbbbbbbbb (this binary; every run listed was answered by it), tree tttttttttttt"));
    assert!(said.contains("  infer_body_query  mir_body_query"), "the bounded counters head the runs: {said}");

    let failed = Report {
        runs: vec![run(1, 7, 900.0, 880.0)],
        summary: None,
        failed: Some(Failed {
            run: 2,
            what: "`blaze build` exited 101".to_owned(),
            code: Some(101),
            stderr: "error: expected `;`\n".to_owned(),
        }),
        ..report
    };
    let said = text(&failed);
    assert_eq!(
        said.lines().next().unwrap(),
        "FAILED: body edit of `main` in src/main.rs, run 2: `blaze build` exited 101; nothing is timed past it and no \
         median is said"
    );
    assert!(said.contains("  | error: expected `;`"), "{said}");
    assert!(!said.contains("median of runs"), "{said}");
}

#[test]
fn the_json_round_trips_and_names_its_version() {
    let now = record_of('b', 't', 31.0);
    let report = report_of(&now, previous(&[record_of('a', 'u', 90.0)], &now));
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.starts_with(r#"{"version":1,"workspace":"/w","kind":"body","#), "{json}");
    assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report);
    assert_eq!(serde_json::to_string(&Kind::NoOp).unwrap(), r#""no-op""#);
    assert_eq!(serde_json::to_string(&Kind::DepBody).unwrap(), r#""dep-body""#);
}

#[test]
fn the_counters_always_list_the_edit_queries_then_what_ran_most() {
    let counts = [
        Counter { query: "parse".to_owned(), executed: 2 },
        Counter { query: "mir_body_query".to_owned(), executed: 1 },
        Counter { query: "item_tree".to_owned(), executed: 9 },
        Counter { query: "idle".to_owned(), executed: 0 },
    ];
    let got: Vec<(String, u64)> = counters(&counts).into_iter().map(|it| (it.query, it.executed)).collect();
    let want = [
        ("infer_body_query", 0),
        ("mir_body_query", 1),
        ("runtime_mir_body_query", 0),
        ("body_check_clean", 0),
        ("module_check_clean", 0),
        ("item_tree", 9),
        ("parse", 2),
    ];
    assert_eq!(got, want.map(|(query, n)| (query.to_owned(), n)));
}

#[test]
fn the_words_are_read_and_a_wrong_one_is_named() {
    let words = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
    let asked = parse(words("body --file src/main.rs --fn main:12 --runs 3 --json -- -p app --release")).unwrap();
    assert_eq!(
        asked,
        Asked {
            edit: Edit {
                kind: Kind::Body,
                file: Some(PathBuf::from("src/main.rs")),
                function: Some(("main".to_owned(), Some(12))),
                runs: 3,
            },
            json: true,
            build_args: words("-p app --release"),
        }
    );
    assert_eq!(parse(words("no-op")).unwrap().edit, Edit { kind: Kind::NoOp, file: None, function: None, runs: 5 });
    for (given, says) in [
        ("body --file a.rs", "a body edit takes `--fn NAME`"),
        ("signature --fn f", "a signature edit takes `--file PATH`"),
        ("rename --file a.rs", "`rename` is no kind of edit"),
        ("no-op --file a.rs", "a no-op edits nothing"),
        ("comment --file a.rs --runs 0", "`--runs` takes a number of runs, 1 or more"),
        ("--json", "which kind of edit"),
    ] {
        let error = parse(words(given)).unwrap_err();
        assert!(error.starts_with(USAGE) && error.contains(says), "{given}: {error}");
    }
}
