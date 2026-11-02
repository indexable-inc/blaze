use std::ffi::OsString;

use super::*;

fn os(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn cargo(words: &[&str]) -> Invocation {
    match parse(os(words)).unwrap() {
        Asked::Cargo(asked) => *asked,
        other => panic!("not a build: {other:?}"),
    }
}

fn refused(words: &[&str]) -> String {
    refusal(&parse(os(words)).unwrap_err(), false)
}

#[test]
fn check_reads_its_fix_replace_and_baseline_flags_and_keeps_them_from_the_load() {
    let asked = cargo(&["check", "--workspace", "--baselines", "b"]);
    assert_eq!(asked.command, Some(Command::Check));
    assert!(asked.flags.workspace);
    assert_eq!(asked.flags.baselines.as_deref(), Some("b"));
    assert_eq!(asked.args, ["--workspace"], "blaze's own flags stay with the client");
    assert!(refused(&["check", "--no-deps"]).starts_with("error: unexpected argument '--no-deps' found"));
    assert!(refused(&["check", "--allow-dirty"]).starts_with("error: unexpected argument '--allow-dirty' found"));
    assert!(refused(&["check", "--", "-D", "warnings"]).starts_with("error: unexpected argument '-D' found"));
    let unhonored = |words: &[&str]| super::refused(&cargo(words));
    assert_eq!(unhonored(&["check", "--fix"]), None);
    assert!(unhonored(&["check", "--fix", "--baselines", "b"]).is_some(), "a fix changes what the baselines count");
    let fix = cargo(&["check", "--fix", "--lint", "clippy::disallowed_types", "--lint=unused_imports", "-p", "a"]);
    assert!(fix.flags.fix);
    assert_eq!(fix.flags.lints, ["clippy::disallowed_types", "unused_imports"]);
    assert_eq!(fix.args, ["-p", "a"], "`--fix` and its `--lint`s stay with the client");
    assert!(unhonored(&["check", "--lint", "unused_imports"]).is_some(), "`--lint` narrows `--fix` alone");
    let replace = cargo(&["check", "--replace", "std::sync::Mutex=parking_lot::Mutex", "--replace=a::B=c::D"]);
    assert_eq!(replace.flags.replace, ["std::sync::Mutex=parking_lot::Mutex", "a::B=c::D"]);
    assert!(replace.args.is_empty(), "`--replace` stays with the client");
    assert!(unhonored(&["check", "--replace", "a::B=c::D", "--fix"]).is_some(), "a replace checks nothing");
    assert!(unhonored(&["check", "--bless-baselines"]).is_some(), "no directory to bless");
    assert_eq!(unhonored(&["check", "--baselines", "b", "--bless-baselines", "--new-baseline", "clippy::x"]), None);
    let many = cargo(&["check", "--baselines", "b", "--new-baseline", "clippy::a,blaze::b", "--new-baseline=clippy::c"]);
    assert_eq!(many.flags.new_baselines, ["clippy::a", "blaze::b", "clippy::c"]);
}

/// `check` lints and fixes, so `clippy` and `fix` are no commands of blaze's.
#[test]
fn clippy_and_fix_are_no_commands() {
    assert_eq!(parse(os(&["clippy", "--workspace"])), Ok(Asked::Unknown("clippy".to_owned())));
    assert_eq!(parse(os(&["fix", "--replace", "a::B=c::D"])), Ok(Asked::Unknown("fix".to_owned())));
}

#[test]
fn doc_reads_cargo_docs_flags_and_refuses_what_it_does_not_take() {
    let asked = cargo(&["doc", "--open", "--no-deps", "--document-private-items", "-p", "a", "--lib"]);
    assert!(asked.flags.open && asked.flags.no_deps && asked.flags.private && asked.flags.targeted);
    assert_eq!(asked.flags.packages, ["a"]);
    assert_eq!(asked.args, ["--open", "--no-deps", "--document-private-items", "-p", "a", "--lib"], "cargo's to read too");
    assert!(refused(&["doc", "--tests"]).starts_with("error: unexpected argument '--tests' found"));
    assert!(refused(&["doc", "stray"]).starts_with("error: unexpected argument 'stray' found"));
    assert!(refused(&["check", "--open"]).starts_with("error: unexpected argument '--open' found"));
    // What cargo doc takes and blaze doc can't honor yet stops it, naming the flag.
    let unhonored = |words: &[&str]| super::refused(&cargo(words));
    assert_eq!(unhonored(&["doc"]), None);
    assert_eq!(unhonored(&["doc", "--message-format", "short"]), None);
    assert_eq!(unhonored(&["doc", "--unit-graph"]).unwrap(), "`blaze doc` does not write what `--unit-graph` asks for yet");
    assert_eq!(unhonored(&["doc", "--timings"]).unwrap(), "`blaze doc` does not write what `--timings` asks for yet");
    assert_eq!(
        unhonored(&["doc", "--message-format=json-render-diagnostics,json-diagnostic-short"]).unwrap(),
        "`blaze doc` writes no JSON messages for the pages yet (`--message-format json-render-diagnostics`)"
    );
    assert_eq!(super::refused(&cargo(&["check", "--timings"])), None, "not doc's");
}

#[test]
fn commands_and_their_aliases() {
    for (word, command) in [
        ("build", Command::Build),
        ("b", Command::Build),
        ("check", Command::Check),
        ("c", Command::Check),
        ("run", Command::Run),
        ("r", Command::Run),
        ("test", Command::Test),
        ("t", Command::Test),
        ("bench", Command::Bench),
        ("doc", Command::Doc),
        ("d", Command::Doc),
    ] {
        assert_eq!(cargo(&[word]).command, Some(command), "{word}");
    }
    assert_eq!(parse(os(&[])), Ok(Asked::Help(None)));
    assert_eq!(parse(os(&["--help"])), Ok(Asked::Help(None)));
    assert_eq!(parse(os(&["help", "test"])), Ok(Asked::Help(Some(Command::Test))));
    assert_eq!(parse(os(&["build", "--release", "-h"])), Ok(Asked::Help(Some(Command::Build))));
    assert_eq!(parse(os(&["-V"])), Ok(Asked::Version));
    assert_eq!(parse(os(&["stop"])), Ok(Asked::Stop));
    assert_eq!(parse(os(&["status"])), Ok(Asked::Status));
    assert_eq!(parse(os(&["toolchain"])), Ok(Asked::Toolchain));
    assert_eq!(parse(os(&["daemon", "install"])), Ok(Asked::Daemon(DaemonVerb::Install)));
    assert_eq!(parse(os(&["daemon", "uninstall"])), Ok(Asked::Daemon(DaemonVerb::Uninstall)));
    assert_eq!(parse(os(&["daemon", "start"])), Ok(Asked::Unknown("daemon start".to_owned())));
    assert_eq!(parse(os(&["daemon"])), Ok(Asked::Unknown("daemon".to_owned())));
    use crate::cli::inspect::Inspected;
    assert_eq!(
        parse(os(&["codegen-inputs", "--format=json", "-p", "foo"])),
        Ok(Asked::Inspect(Inspected::CodegenInputs, os(&["--format=json", "-p", "foo"])))
    );
    assert_eq!(parse(os(&["trace", "--out", "t"])), Ok(Asked::Inspect(Inspected::Trace, os(&["--out", "t"]))));
    assert_eq!(parse(os(&["tables"])), Ok(Asked::Inspect(Inspected::Tables, Vec::new())));
    assert_eq!(
        parse(os(&["pack", "-p", "app", "--out", "a.pack"])),
        Ok(Asked::Inspect(Inspected::Pack, os(&["-p", "app", "--out", "a.pack"])))
    );
    assert_eq!(parse(os(&["frobnicate"])), Ok(Asked::Unknown("frobnicate".to_owned())));
}

#[test]
fn flags_before_the_command_are_cargos_too() {
    let asked = cargo(&["-q", "--color", "never", "b", "--release", "-vv"]);
    assert_eq!(asked.command(), Command::Build);
    assert_eq!(asked.args, ["-q", "--color", "never", "--release", "-vv"]);
    assert!(asked.flags.quiet && asked.flags.release);
    assert_eq!(asked.flags.color.as_deref(), Some("never"));
    assert_eq!(asked.flags.verbose, 2);
    assert_eq!(asked.profile(), "release");
}

#[test]
fn values_as_clap_takes_them() {
    let asked = cargo(&["build", "-p", "a", "--package=b", "-pc", "-F", "x y,z", "--features=w", "-j8", "--bin", "--target", "aarch64-apple-darwin"]);
    assert_eq!(asked.flags.packages, ["a", "b", "c"]);
    assert_eq!(asked.flags.features, ["x", "y", "z", "w"]);
    assert_eq!(asked.flags.bins, [None::<String>]);
    assert_eq!(asked.flags.targets, ["aarch64-apple-darwin"]);
    // `-p` without a value is cargo's to answer, with the packages it knows.
    let asked = cargo(&["build", "-p", "--release"]);
    assert!(asked.flags.packages.is_empty());
    assert!(asked.flags.release);
    assert_eq!(asked.args, ["-p", "--release"]);
    let asked = cargo(&["build", "--message-format=json,short", "--profile", "dist"]);
    assert_eq!(asked.flags.message_format, ["json,short"]);
    assert_eq!(asked.profile(), "dist");
}

#[test]
fn test_names_its_filter_and_the_harness_args() {
    let asked = cargo(&["test", "parse", "--no-fail-fast", "--", "--exact", "--nocapture"]);
    assert_eq!(asked.filter.as_deref(), Some("parse"));
    assert_eq!(asked.args, ["parse", "--no-fail-fast"]);
    assert_eq!(asked.program, os(&["--exact", "--nocapture"]));
    assert_eq!(asked.test_args(), os(&["parse", "--exact", "--nocapture"]));
    assert!(asked.flags.no_fail_fast);
    assert_eq!(asked.profile(), "test");

    let bench = cargo(&["bench", "fast"]);
    assert_eq!(bench.test_args(), os(&["fast", "--bench"]));
    assert_eq!(bench.profile(), "bench");

    let reached = cargo(&["test", "--reached"]);
    assert!(reached.flags.reached);
    assert!(reached.args.is_empty());
}

#[test]
fn ub_is_blazes_and_its_values_imply_it() {
    let ub = cargo(&["test", "--ub", "parse", "--release"]);
    assert!(ub.flags.ub);
    assert_eq!((ub.flags.ub_seed, ub.flags.ub_schedules), (None, None));
    assert_eq!(ub.args, ["parse", "--release"], "cargo never sees `--ub`");
    let seeded = cargo(&["test", "--ub-seed", "42", "--ub-schedules=8"]);
    assert!(seeded.flags.ub);
    assert_eq!((seeded.flags.ub_seed, seeded.flags.ub_schedules), (Some(42), Some(8)));
    assert!(seeded.args.is_empty());
    assert!(!cargo(&["test"]).flags.ub, "opt-in");
    assert!(parse(os(&["bench", "--ub"])).is_err());
    assert!(parse(os(&["build", "--ub-seed", "1"])).is_err());
}

#[test]
fn ub_values_clap_refuses() {
    assert_eq!(
        refused(&["test", "--ub-seed", "x1"]),
        "error: invalid value 'x1' for '--ub-seed <N>': invalid digit found in string\n\nFor more information, \
         try '--help'.\n"
    );
    assert_eq!(
        refused(&["test", "--ub-schedules", "0"]),
        "error: invalid value '0' for '--ub-schedules <K>': 0 is not in 1..=4294967295\n\nFor more information, \
         try '--help'.\n"
    );
    assert!(parse(os(&["test", "--ub-seed"])).is_err());
}

#[test]
fn refused_is_blazes_a_count_or_all() {
    assert_eq!(cargo(&["build"]).flags.refused, None);
    let all = cargo(&["build", "--refused", "all", "--release"]);
    assert_eq!(all.flags.refused, Some(None));
    assert_eq!(all.args, ["--release"], "cargo never sees `--refused`");
    assert_eq!(cargo(&["test", "--refused=100000"]).flags.refused, Some(Some(100_000)));
    assert_eq!(cargo(&["run", "--refused", "0"]).flags.refused, Some(Some(0)));
    assert!(refused(&["check", "--refused", "every"]).starts_with("error: invalid value 'every' for '--refused <N|all>': "));
    assert!(parse(os(&["doc", "--refused", "all"])).is_err(), "doc compiles no program");
    assert!(parse(os(&["build", "--refused"])).is_err());
}

#[test]
fn run_has_the_rest() {
    let asked = cargo(&["run", "--release", "a", "--verbose", "--", "b"]);
    assert_eq!(asked.args, ["--release"]);
    assert_eq!(asked.program, os(&["a", "--verbose", "b"]));
    assert_eq!(asked.flags.verbose, 0);
    let asked = cargo(&["run", "--bin", "app", "--", "-x", "--", "y"]);
    assert_eq!(asked.flags.bins, [Some("app".to_owned())]);
    assert_eq!(asked.program, os(&["-x", "--", "y"]));
}

#[test]
fn an_unknown_flag() {
    assert_eq!(
        refused(&["build", "--frobnicate"]),
        "error: unexpected argument '--frobnicate' found\n\nUsage: cargo build [OPTIONS]\n\nFor more \
         information, try '--help'.\n"
    );
    // clap adds the flag it suggests to the usage line ("smart usage").
    assert_eq!(
        refused(&["build", "--relese"]),
        "error: unexpected argument '--relese' found\n\n  tip: a similar argument exists: '--release'\n\n\
         Usage: cargo build --release\n\nFor more information, try '--help'.\n"
    );
    // A second positional of `test` is no flag: no `--` tip.
    assert_eq!(
        refused(&["test", "a", "b"]),
        "error: unexpected argument 'b' found\n\nUsage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]\n\nFor \
         more information, try '--help'.\n"
    );
    // `--doc` and `--reached` are `test`'s alone.
    assert!(parse(os(&["build", "--reached"])).is_err());
    assert!(parse(os(&["build", "--", "x"])).is_err());
}

#[test]
fn an_unknown_flag_of_a_command_with_positionals() {
    assert_eq!(
        refused(&["test", "--frobnicate"]),
        "error: unexpected argument '--frobnicate' found\n\n  tip: to pass '--frobnicate' as a value, use '-- \
         --frobnicate'\n\nUsage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["test", "parse", "--relese"]),
        "error: unexpected argument '--relese' found\n\n  tip: a similar argument exists: '--release'\n  tip: to \
         pass '--relese' as a value, use '-- --relese'\n\nUsage: cargo test --release <TESTNAME> [-- [ARGS]...]\n\nFor \
         more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["run", "-y"]),
        "error: unexpected argument '-y' found\n\n  tip: to pass '-y' as a value, use '-- -y'\n\nUsage: cargo run \
         [OPTIONS] [ARGS]...\n\nFor more information, try '--help'.\n"
    );
}

#[test]
fn hidden_flags_that_point_elsewhere() {
    let tip = |words: &[&str], arg: &str, tip: &str, usage: &str| {
        let expected = format!(
            "error: unexpected argument '{arg}' found\n\n  tip: {tip}\n\nUsage: {usage}\n\nFor more information, try '--help'.\n"
        );
        assert_eq!(refused(words), expected, "{words:?}");
    };
    let similar = |to: &str| format!("a similar argument exists: '{to}'");
    tip(&["check", "-x", "a"], "-x", &similar("--exclude"), "cargo check [OPTIONS]");
    tip(&["build", "-s"], "--silent", &similar("--quiet"), "cargo build [OPTIONS]");
    tip(&["run", "--silent"], "--silent", &similar("--quiet"), "cargo run [OPTIONS] [ARGS]...");
    tip(&["build", "-t", "x"], "-t", &similar("--target"), "cargo build [OPTIONS]");
    tip(&["build", "--path", "x"], "--path", &similar("--manifest-path"), "cargo build [OPTIONS]");
    tip(&["build", "--out-dir", "x"], "--out-dir", &similar("--artifact-dir"), "cargo build [OPTIONS]");
    tip(&["build", "-O"], "-O", &similar("--artifact-dir"), "cargo build [OPTIONS]");
    tip(&["build", "-c", "x"], "-c", &similar("--config"), "cargo build [OPTIONS]");
    tip(&["check", "-z", "x"], "-z", &similar("-Z"), "cargo check [OPTIONS]");
    tip(
        &["build", "--debug"],
        "--debug",
        "`--debug` is the default for `cargo build`; instead `--release` is supported",
        "cargo build [OPTIONS]",
    );
    let keep_going = "use `--no-fail-fast` to run as many tests as possible regardless of failure";
    tip(&["test", "--keep-going"], "--keep-going", keep_going, "cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]");
    tip(&["bench", "--keep-going"], "--keep-going", keep_going, "cargo bench [OPTIONS] [BENCHNAME] [-- [ARGS]...]");
    // `test`'s `-q` is its own, and it has no `-s`; `check` has no `--debug`.
    assert!(refused(&["test", "-s"]).starts_with("error: unexpected argument '-s' found\n\n  tip: to pass '-s'"));
    assert!(parse(os(&["check", "--debug"])).is_err());
    assert!(cargo(&["build", "--keep-going"]).args == ["--keep-going"]);
}

#[test]
fn flags_as_cargos_prelude_has_them() {
    let asked = cargo(&["build", "-m", "a/Cargo.toml", "-j", "-1", "--timings", "--compile-time-deps"]);
    assert_eq!(asked.flags.manifest_path.as_deref(), Some("a/Cargo.toml"));
    assert_eq!(asked.args, ["-m", "a/Cargo.toml", "-j", "-1", "--timings", "--compile-time-deps"]);
    assert!(cargo(&["check", "--compile-time-deps"]).args == ["--compile-time-deps"]);
    assert_eq!(
        refused(&["build", "--timings=html"]),
        "error: unexpected value 'html' for '--timings' found; no more were expected\n\nUsage: cargo build \
         [OPTIONS]\n\nFor more information, try '--help'.\n"
    );
    for gone in ["--lockfile-path", "--build-plan"] {
        assert!(refused(&["build", gone]).starts_with(&format!("error: unexpected argument '{gone}' found")), "{gone}");
    }
    // `bench` has no `--release` and no `--future-incompat-report`; `test` has the report.
    assert!(parse(os(&["bench", "--release"])).is_err());
    assert!(parse(os(&["bench", "--future-incompat-report"])).is_err());
    assert!(parse(os(&["test", "--future-incompat-report"])).is_ok());
    assert!(parse(os(&["check", "--artifact-dir", "x"])).is_err());
    assert_eq!(cargo(&["build", "--color", "ALWAYS"]).flags.color.as_deref(), Some("ALWAYS"));
    let help = help(Command::Build);
    assert!(help.contains("Usage: cargo build [OPTIONS]\n") && help.contains("  -m, --manifest-path <PATH>\n"), "{help}");
    assert!(!help.contains("--silent") && !help.contains("compile-time-deps"), "{help}");
}

#[test]
fn a_flag_given_twice() {
    assert_eq!(
        refused(&["build", "--release", "-r"]),
        "error: the argument '--release' cannot be used multiple times\n\nUsage: cargo build [OPTIONS]\n\nFor more \
         information, try '--help'.\n"
    );
    assert!(refused(&["run", "-p", "a", "-p", "b"]).starts_with("error: the argument '--package [<SPEC>]' cannot be used multiple times"));
    assert!(refused(&["build", "--profile", "a", "--profile=b"]).contains("'--profile <PROFILE-NAME>' cannot be used multiple times"));
    // Appended, counted, or once before the command and once after it (clap's parent and child).
    assert_eq!(cargo(&["build", "-p", "a", "-p", "b", "-F", "x", "-F", "y"]).flags.packages, ["a", "b"]);
    assert_eq!(cargo(&["build", "-v", "--verbose", "-v"]).flags.verbose, 3);
    assert!(cargo(&["-q", "build", "-q"]).flags.quiet);
    assert_eq!(cargo(&["--color", "never", "build", "--color", "always"]).flags.color.as_deref(), Some("always"));
}

#[test]
fn release_and_profile_conflict() {
    assert_eq!(
        refused(&["build", "-v", "--release", "--profile", "dev"]),
        "error: the argument '--release' cannot be used with '--profile <PROFILE-NAME>'\n\nUsage: cargo build \
         --verbose... --release\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["test", "--profile", "dev", "parse", "-r"]),
        "error: the argument '--profile <PROFILE-NAME>' cannot be used with '--release'\n\nUsage: cargo test \
         --profile <PROFILE-NAME> <TESTNAME> [-- [ARGS]...]\n\nFor more information, try '--help'.\n"
    );
    // A refusal while reading comes first.
    assert!(refused(&["build", "--release", "--profile", "dev", "--frobnicate"]).contains("'--frobnicate'"));
}

#[test]
fn change_directory_before_the_command() {
    let asked = cargo(&["-C", "sub", "-Zunstable-options", "build", "--release"]);
    assert_eq!(asked.directory, Some(OsString::from("sub")));
    assert!(asked.unstable_options);
    assert_eq!(asked.args, ["-Zunstable-options", "--release"], "`-C` is the client's, not the load's");
    let asked = cargo(&["-Csub", "-Z", "unstable-options", "check"]);
    assert_eq!((asked.directory, asked.unstable_options), (Some(OsString::from("sub")), true));
    let asked = cargo(&["-C=sub", "build", "-Zunstable-options"]);
    assert!(!asked.unstable_options, "cargo looks for it before the command only");
    assert_eq!(
        refusal(&parse(os(&["-C"])).unwrap_err(), false),
        "error: a value is required for '-C <DIRECTORY>' but none was supplied\n\nFor more information, try '--help'.\n"
    );
}

#[test]
fn what_cargos_commands_refuse_before_building() {
    let refused_of = |words: &[&str]| super::refused(&cargo(words));
    assert_eq!(refused_of(&["test", "--doc", "--lib"]).as_deref(), Some("can't mix --doc with other target selecting options"));
    assert_eq!(refused_of(&["test", "--doc", "--test", "x"]).as_deref(), Some("can't mix --doc with other target selecting options"));
    assert_eq!(refused_of(&["test", "--doc", "--no-run"]).as_deref(), Some("can't skip running doc tests with --no-run"));
    assert_eq!(refused_of(&["test", "--doc", "-p", "foo"]), None);
    assert_eq!(
        refused_of(&["run", "-p", "foo*"]).as_deref(),
        Some("`cargo run` does not support glob pattern `foo*` on package selection")
    );
    assert_eq!(
        refused_of(&["run", "--bin", "a?"]).as_deref(),
        Some("`cargo run` does not support glob patterns on target selection")
    );
    assert_eq!(refused_of(&["run", "--bin", "a", "--", "*"]), None);
    assert_eq!(refused_of(&["build", "--bin", "a*"]), None, "cargo's build takes globs");
}

#[test]
fn a_flag_without_its_value() {
    assert_eq!(
        refused(&["build", "--jobs"]),
        "error: a value is required for '--jobs <N>' but none was supplied\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--features", "--release"]),
        "error: a value is required for '--features <FEATURES>' but none was supplied\n\nFor more \
         information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--message-format"]),
        "error: a value is required for '--message-format <FMT>' but none was supplied\n  [possible values: \
         human, short, json, json-diagnostic-short, json-diagnostic-rendered-ansi, json-render-diagnostics]\n\n\
         For more information, try '--help'.\n"
    );
}

#[test]
fn a_value_clap_refuses() {
    assert_eq!(
        refused(&["build", "--color", "alwys"]),
        "error: invalid value 'alwys' for '--color <WHEN>'\n  [possible values: auto, always, never]\n\n  tip: \
         a similar value exists: 'always'\n\nFor more information, try '--help'.\n"
    );
    assert_eq!(
        refused(&["build", "--release=yes"]),
        "error: unexpected value 'yes' for '--release' found; no more were expected\n\nUsage: cargo build \
         [OPTIONS]\n\nFor more information, try '--help'.\n"
    );
    assert!(parse(os(&["build", "--message-format", "JSON,Short"])).is_ok());
    assert!(parse(os(&["build", "--message-format", "json,xml"])).is_err());
}

#[test]
fn refusals_in_cargos_colors() {
    let colored = refusal(&parse(os(&["build", "--frobnicate"])).unwrap_err(), true);
    assert!(
        colored.starts_with("\x1b[1m\x1b[91merror:\x1b[0m unexpected argument '\x1b[1m\x1b[93m--frobnicate\x1b[0m' found\n"),
        "{colored:?}"
    );
    assert!(colored.contains("\x1b[1m\x1b[92mUsage:\x1b[0m \x1b[1m\x1b[96mcargo build\x1b[0m \x1b[36m[OPTIONS]\x1b[0m"));
    // `Arg::stylized` and clap's `[-- ...]`: literal brackets and dashes, placeholder values.
    let colored = refusal(&parse(os(&["test", "-y"])).unwrap_err(), true);
    assert!(colored.contains(
        "\x1b[36m[TESTNAME]\x1b[0m \x1b[1m\x1b[96m[--\x1b[0m \x1b[36m[ARGS]...\x1b[0m\x1b[1m\x1b[96m]\x1b[0m"
    ), "{colored:?}");
    assert!(colored.contains("to pass '\x1b[1m\x1b[93m-y\x1b[0m' as a value"), "{colored:?}");
    let colored = refusal(&parse(os(&["build", "-v", "-r", "--profile", "a"])).unwrap_err(), true);
    assert!(colored.contains("\x1b[1m\x1b[96m--verbose\x1b[0m\x1b[36m...\x1b[0m \x1b[1m\x1b[96m--release\x1b[0m"), "{colored:?}");
}

#[test]
fn a_flag_is_painted_only_in_color() {
    let flag = Flag::new("features", None, Takes::Value, "FEATURES");
    assert_eq!(flag.painted(false), "--features <FEATURES>");
    assert_eq!(flag.shown(), flag.painted(false));
    assert_eq!(
        flag.painted(true),
        "\x1b[1m\x1b[96m--features\x1b[0m\x1b[36m \x1b[0m\x1b[36m<FEATURES>\x1b[0m"
    );
}

#[test]
fn jaro_as_strsim() {
    assert!((jaro("martha", "marhta") - 0.944_444).abs() < 1e-5);
    assert_eq!(jaro("", ""), 1.0);
    assert_eq!(jaro("a", ""), 0.0);
}
