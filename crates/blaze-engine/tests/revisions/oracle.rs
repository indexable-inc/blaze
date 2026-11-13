//! The oracle under the real driver (docs/harness.md, "The oracle"): Hegel draws a workspace
//! and a sequence of turns ([`generate`]), a real daemon builds each on the state the turns
//! before left, and after every build the program prints what the model says, the outcome,
//! the output and the diagnostics are a clean build's of the same sources in a fresh home,
//! the functions made again are within the turn's bound, and the daemon caught no panic,
//! contained no unwind and recomputed nothing it has no reason for.
//!
//! A failure is written as a case under `BLAZE_ORACLE_OUT` (the system's temporary directory
//! otherwise) and Hegel shrinks the sequence; the smallest is the last case written, and its
//! directory is checked in as it is (`cases/<name>/`, listed in `real!`).
//!
//! Not in the gate: a case is minutes of real builds. `BLAZE_ORACLE_CASES` is how many
//! sequences (20), `BLAZE_ORACLE_TURNS` the most turns of one (5), `BLAZE_ORACLE_SEED` the
//! seed, `BLAZE_ORACLE_SKIP` the edit kinds, events and parts not drawn, and
//! `BLAZE_ORACLE_CLEAN=when-wrong` builds a revision from nothing only when its incremental
//! build failed a check (the model's `prints` still catches a stale program).

use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::PathBuf,
};

use hegel::{Hegel, Settings, generators as gs};

use super::{
    check,
    generate::{self, Ask, Edit, Event, Parts, Request, Save, Turn},
    real::{self, Clean},
};

fn number(name: &str, otherwise: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value.parse().unwrap_or_else(|_| panic!("{name}={value} is no number")),
        Err(_) => otherwise,
    }
}

fn out() -> PathBuf {
    std::env::var_os("BLAZE_ORACLE_OUT").map_or_else(|| std::env::temp_dir().join("blaze-oracle"), PathBuf::from)
}

/// The case of `turns` over `parts`, run and judged.
fn case(parts: Parts, turns: &[Turn]) {
    let mut test = generate::test(&generate::name(parts, turns), parts, turns, &["real"]);
    // `BLAZE_ORACLE_ENV=NAME=VALUE,..`: variables of every command from the first revision
    // on (the driver clears its own environment, so one exported to this test never arrives).
    if let Ok(all) = std::env::var("BLAZE_ORACLE_ENV") {
        for pair in all.split(',').filter(|it| !it.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or_else(|| panic!("BLAZE_ORACLE_ENV: `{pair}` is not NAME=VALUE"));
            test.revisions[0].env.push((name.to_owned(), value.to_owned()));
        }
    }
    let clean = match std::env::var("BLAZE_ORACLE_CLEAN").as_deref() {
        Ok("when-wrong") => Clean::WhenWrong,
        Ok(other) if other != "always" => panic!("BLAZE_ORACLE_CLEAN={other}: `always` or `when-wrong`"),
        _ => Clean::Always,
    };
    let sequence = format!("the sequence: {parts:?}\n{turns:#?}\n");
    // A step that fails outright (a hang, a second checkout that does not build) panics in
    // the driver: that sequence is written out too.
    let ran = catch_unwind(AssertUnwindSafe(|| real::rows(&test, clean)));
    let wrong = match &ran {
        Ok((rows, _)) => rows.iter().any(|row| !row.failed.is_empty()),
        Err(_) => true,
    };
    let mut note = sequence;
    if wrong {
        generate::write_case(&test, &out());
        note.push_str(&format!("written as the case {}\n", out().join(&test.name).display()));
    }
    match ran {
        Ok((rows, logs)) => check::verdict(&test, "real", &rows, &format!("{note}{logs}")),
        Err(payload) => {
            eprintln!("{note}");
            resume_unwind(payload)
        }
    }
}

#[test]
#[ignore = "minutes of real builds a case: docs/harness.md, \"The oracle\""]
fn any_turn_sequence_builds_as_clean() {
    let most = number("BLAZE_ORACLE_TURNS", 5);
    let mut settings = Settings::new()
        .test_cases(number("BLAZE_ORACLE_CASES", 20))
        .database(None)
        .suppress_health_check([hegel::HealthCheck::TooSlow]);
    if std::env::var_os("BLAZE_ORACLE_SEED").is_some() {
        settings = settings.seed(Some(number("BLAZE_ORACLE_SEED", 0)));
    }
    // `BLAZE_ORACLE_MINUTES=<n>` with a large `BLAZE_ORACLE_CASES`: a run of that long; the
    // sequences after it are not run.
    let minutes = std::env::var_os("BLAZE_ORACLE_MINUTES").map(|_| number("BLAZE_ORACLE_MINUTES", 0));
    let began = std::time::Instant::now();
    Hegel::new(move |tc: hegel::TestCase| {
        if minutes.is_some_and(|it| began.elapsed().as_secs() >= it * 60) {
            return;
        }
        let mut draw = |n: u64| tc.draw(gs::integers::<u64>().max_value(n - 1));
        let parts = generate::parts(&mut draw);
        let turns = generate::turns(&mut draw, parts, false, most);
        case(parts, &turns);
    })
    .settings(settings)
    .run();
}

/// One of each: every part, every edit kind, every save and every event once, in a fixed
/// order. The oracle's smoke run, and the first thing to read when it fails.
#[test]
#[ignore = "minutes of real builds: docs/harness.md, \"The oracle\""]
fn each_kind_once_builds_as_clean() {
    let parts = Parts { up: true, made: true, mac: std::env::var_os("BLAZE_ORACLE_NO_MAC").is_none(), mid: true };
    let turn = |edits: &[Edit], save, event, event_last, width| Turn { request: Request::RUN, edits: edits.to_vec(), save, event, event_last, width };
    let mut turns = vec![
        turn(&[Edit::Body(1)], Save::Write, Event::Nothing, false, None),
        turn(&[], Save::Write, Event::Nothing, false, None),
        turn(&[Edit::Comment(1), Edit::Lines], Save::Rename, Event::Nothing, false, None),
        turn(&[Edit::MainBody(1)], Save::Truncate, Event::Nothing, false, Some(80)),
        turn(&[Edit::ScriptInput(1)], Save::Write, Event::Stop, false, None),
        turn(&[Edit::Script(2)], Save::Twice, Event::Nothing, false, None),
        turn(&[Edit::Up(1), Edit::Layout], Save::Write, Event::Kill, true, None),
        turn(&[Edit::Signature], Save::Write, Event::Cancel(80), true, None),
        turn(&[Edit::TraitImpl(1), Edit::Override], Save::Write, Event::Beside, false, None),
        turn(&[Edit::Dyn], Save::Write, Event::Build, true, None),
        turn(&[Edit::Item, Edit::Rename], Save::Write, Event::Nothing, false, Some(200)),
        turn(&[Edit::Generated(1), Edit::Whitespace], Save::Write, Event::Nothing, false, None),
        turn(&[Edit::Feature], Save::Write, Event::Nothing, false, None),
        turn(&[Edit::SourceCfg], Save::Write, Event::Nothing, false, None),
        turn(&[Edit::ScriptCfg], Save::Write, Event::Nothing, false, None),
        turn(&[Edit::Body(0)], Save::Write, Event::Nothing, false, None),
    ];
    if parts.mac {
        turns.push(turn(&[Edit::Mac(1)], Save::Write, Event::Nothing, false, None));
    }
    // Below a library (`mid` over `up`): each alone, so a record of `mid` found across it
    // that should not have been shows as that turn's stale number.
    let below = [
        Edit::UpPlain(1),
        Edit::UpLines,
        Edit::UpPad,
        Edit::UpTwist(1),
        Edit::UpSignature,
        Edit::UpAlways(1),
        Edit::UpInherent,
        Edit::UpInherent,
        Edit::UpPlain(0),
    ];
    // After the first turn, before the cfg flips whose splice is refused (row sound-1112-5).
    for (at, edit) in below.into_iter().enumerate() {
        turns.insert(1 + at, turn(&[edit], Save::Write, Event::Nothing, false, None));
    }
    case(parts, &turns);
}

/// The sequence `BLAZE_ORACLE_REPLAY` writes out ([`generate::parse`]): a finding run again,
/// or cut down by hand.
#[test]
#[ignore = "runs the sequence BLAZE_ORACLE_REPLAY names: docs/harness.md, \"The oracle\""]
fn replay() {
    let text = std::env::var("BLAZE_ORACLE_REPLAY").expect("BLAZE_ORACLE_REPLAY is a sequence, as `generate::parse` reads it");
    let (parts, turns) = generate::parse(&text);
    case(parts, &turns);
}

/// The alternations alone, no edit between: build, test, build, test, check, run, release,
/// run, the feature on the command line and off, another width, another message format, each
/// a second time. After its first occurrence a request is a no-op in units and runs few
/// queries, whatever was asked between (the kept front end, the no-op lane's kept answer).
#[test]
#[ignore = "minutes of real builds: docs/harness.md, \"The oracle\""]
fn alternating_requests_of_unchanged_sources_are_no_ops() {
    let parts = Parts { up: true, made: true, mac: false, mid: true };
    let ask = |action, release, loud, short, width| Turn {
        request: Request { action, release, loud, short },
        edits: Vec::new(),
        save: Save::Write,
        event: Event::Nothing,
        event_last: false,
        width,
    };
    let turns = [
        ask(Ask::Build, false, false, false, None),
        ask(Ask::Test, false, false, false, None),
        ask(Ask::Build, false, false, false, None),
        ask(Ask::Test, false, false, false, None),
        ask(Ask::Check, false, false, false, None),
        ask(Ask::Run, false, false, false, None),
        ask(Ask::Check, false, false, false, None),
        ask(Ask::Run, true, false, false, None),
        ask(Ask::Run, false, false, false, None),
        ask(Ask::Run, true, false, false, None),
        ask(Ask::Run, false, true, false, None),
        ask(Ask::Run, false, false, false, None),
        ask(Ask::Run, false, true, false, None),
        ask(Ask::Run, false, false, false, Some(80)),
        ask(Ask::Run, false, false, true, None),
        ask(Ask::Run, false, false, false, None),
        ask(Ask::Run, false, false, true, None),
    ];
    case(parts, &turns);
}

/// The control: a build that answers with the program of before an edit is caught. The
/// planted bug is in the case (the edit's save is taken out of it), so what ran disagrees
/// with the model's `prints` and with a clean build of the sources the edit makes.
#[test]
#[ignore = "minutes of real builds: docs/harness.md, \"The oracle\""]
fn the_oracle_catches_a_build_of_the_sources_before() {
    let parts = Parts { up: false, made: false, mac: false, mid: false };
    let turns = [Turn { request: Request::RUN, edits: vec![Edit::Body(1)], save: Save::Write, event: Event::Nothing, event_last: false, width: None }];
    let mut test = generate::test("control", parts, &turns, &["real"]);
    // The planted bug: the edit is never saved, so the build is of the sources before.
    let lost = std::mem::take(&mut test.revisions[1].changes);
    let (mut rows, _) = real::rows(&test, Clean::WhenWrong);
    assert!(
        rows[1].failed.iter().any(|it| it.check == "prints"),
        "a build of the sources before the edit passed the model's `prints`"
    );
    // And held to a clean build of the sources the edit made: the output differs.
    test.revisions[1].changes = lost;
    let (clean, _) = real::rows(&test, Clean::WhenWrong);
    let failed = check::held(&test.revisions[1], &rows[1].seen, Some(&clean[1].seen), &[]);
    rows[1].failed = failed;
    assert!(
        rows[1].failed.iter().any(|it| it.check == "clean output"),
        "a build of the sources before the edit passed as a clean build of the sources after it"
    );
}
