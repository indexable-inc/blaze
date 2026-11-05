//! Goldens of the renderer for the report a build script that can't find LLVM fails with, as
//! a terminal without colour (wrapped), a pipe and JSON show it; the bytes a words-only report
//! keeps from before reports; the API encoding; the rules, the script directive and the
//! terminal's capabilities.

use crate::terminal::{self, Config, Notifier, Progress};
use crate::{Blocks, Caps, Fix, Report, Roots, Text, json, render, rules, script};

const ROOTS: Roots<'static> = Roots { workspace: Some("/w"), home: Some("/home/u") };

fn example() -> Report {
    Report::error(Text::plain("llvm-sys can't find LLVM"))
        .rule("llvm:pin")
        .at(Text::new()
            .words("build script of ")
            .package("llvm-sys", Some("231.0.1"))
            .words("  (")
            .path("/w/vendor/llvm-sys/build.rs")
            .words(")"))
        .fact(
            "needs",
            Text::new()
                .words("needs LLVM ")
                .version("23.1.4")
                .words(" (")
                .hash("e152d9fa3c71b2d4")
                .words("), pinned in ")
                .path("/w/crates/hir-ty-codegen/llvm.json"),
        )
        .fact("found", Text::new().var("LLVM_SYS_231_PREFIX").words(" is not set"))
        .blocks(Blocks {
            all: vec!["blaze-engine".to_owned(), "blaze-cargo".to_owned()],
            what: "packages".to_owned(),
            count: 247,
            notable: vec!["blaze-engine".to_owned()],
        })
        .fix(Fix::run(&["nix develop"]))
        .fix(Fix::run(&[
            "python3 .github/scripts/llvm.py '{\"command\": \"fetch\", \"target\": \"aarch64-apple-darwin\", \"dir\": \"~/.blaze/llvm\"}'",
            "export LLVM_SYS_231_PREFIX=~/.blaze/llvm",
        ]))
        .log("/home/u/.cache/blaze/workers/67e8ace3/log")
}

#[test]
fn example_piped() {
    let expected = "\
error: llvm-sys can't find LLVM  [llvm: pin]
  ┌ build script of llvm-sys 231.0.1  (vendor/llvm-sys/build.rs)
  │ needs LLVM 23.1.4 (e152d9fa), pinned in crates/hir-ty-codegen/llvm.json
  │ `LLVM_SYS_231_PREFIX` is not set
  └ blocks 247 packages, including blaze-engine

  fix   nix develop
   or   python3 .github/scripts/llvm.py '{\"command\": \"fetch\", \"target\": \"aarch64-apple-darwin\", \"dir\": \"~/.blaze/llvm\"}'
        export LLVM_SYS_231_PREFIX=~/.blaze/llvm

  log   ~/.cache/blaze/workers/67e8ace3/log      more: blaze explain llvm-sys
";
    assert_eq!(render(&example(), Caps::PLAIN, &ROOTS), expected);
}

#[test]
fn example_on_a_narrow_terminal_without_colour() {
    // The box wraps at words, a span whole with what is glued to it; the commands never wrap.
    let expected = "\
error: llvm-sys can't find LLVM  [llvm: pin]
  ┌ build script of llvm-sys 231.0.1
  │ (vendor/llvm-sys/build.rs)
  │ needs LLVM 23.1.4 (e152d9fa), pinned in
  │ crates/hir-ty-codegen/llvm.json
  │ `LLVM_SYS_231_PREFIX` is not set
  └ blocks 247 packages, including blaze-engine

  fix   nix develop
   or   python3 .github/scripts/llvm.py '{\"command\": \"fetch\", \"target\": \"aarch64-apple-darwin\", \"dir\": \"~/.blaze/llvm\"}'
        export LLVM_SYS_231_PREFIX=~/.blaze/llvm

  log   ~/.cache/blaze/workers/67e8ace3/log      more: blaze explain llvm-sys
";
    let caps = Caps { color: false, links: false, width: Some(60) };
    assert_eq!(render(&example(), caps, &ROOTS), expected);
}

#[test]
fn example_as_json() {
    let expected = concat!(
        r#"{"at":[{"Plain":"build script of "},{"Package":{"name":"llvm-sys","version":"231.0.1"}},"#,
        r#"{"Plain":"  ("},{"Path":"/w/vendor/llvm-sys/build.rs"},{"Plain":")"}],"#,
        r#""log":"/home/u/.cache/blaze/workers/67e8ace3/log","rule":"llvm:pin","#,
        r#""facts":[{"key":"needs","text":[{"Plain":"needs LLVM "},{"Version":"23.1.4"},{"Plain":" ("},"#,
        r#"{"Hash":"e152d9fa3c71b2d4"},{"Plain":"), pinned in "},{"Path":"/w/crates/hir-ty-codegen/llvm.json"}]},"#,
        r#"{"key":"found","text":[{"Var":"LLVM_SYS_231_PREFIX"},{"Plain":" is not set"}]}],"#,
        r#""fixes":[{"note":null,"lines":["nix develop"]},{"note":null,"lines":["#,
        r#""python3 .github/scripts/llvm.py '{\"command\": \"fetch\", \"target\": \"aarch64-apple-darwin\", \"dir\": \"~/.blaze/llvm\"}'","#,
        r#""export LLVM_SYS_231_PREFIX=~/.blaze/llvm"]}],"#,
        r#""blocks":{"all":["blaze-engine","blaze-cargo"],"what":"packages","count":247,"notable":["blaze-engine"]},"#,
        r#""causes":[],"headline":[{"Plain":"llvm-sys can't find LLVM"}],"severity":"Error"}"#,
    );
    assert_eq!(json(&example()), expected);
    let back: Report = sonic_rs::from_str(expected).unwrap();
    assert_eq!(back, example());
}

#[test]
fn renders_the_same_twice() {
    let caps = Caps { color: true, links: true, width: Some(60) };
    assert_eq!(render(&example(), caps, &ROOTS), render(&example(), caps, &ROOTS));
}

#[test]
fn cbor_is_deterministic_and_in_key_order() {
    let mut writer = blaze_protocol::cbor::Writer::new();
    let mut bytes = Vec::new();
    writer.append(&mut bytes, &example().cause(Report::text("inner", None))).unwrap();
    // Every struct declares its fields in key order: nothing was put in order on the way.
    assert_eq!(writer.permuted(), 0);
    blaze_protocol::cbor::check(&bytes).unwrap();
    let back: Report = blaze_protocol::cbor::from_slice(&bytes).unwrap();
    assert_eq!(back, example().cause(Report::text("inner", None)));
}

#[test]
fn words_only_keep_their_bytes() {
    // What `say::error` printed before reports, byte for byte.
    let text = Report::text("what", None);
    assert!(text.is_text());
    assert_eq!(render(&text, Caps::PLAIN, &Roots::NONE), "error: what\n");
    let color = Caps { color: true, links: false, width: Some(80) };
    assert_eq!(render(&text, color, &Roots::NONE), "\x1b[1m\x1b[91merror\x1b[0m: what\n");
    let warning = crate::Report::warning(Text::plain("what"));
    assert_eq!(render(&warning, color, &Roots::NONE), "\x1b[1m\x1b[93mwarning\x1b[0m: what\n");
    // `at` is a line of the box.
    let at = Report::text("two\nlines", Some("src/lib.rs".to_owned()));
    assert_eq!(render(&at, Caps::PLAIN, &Roots::NONE), "error: two\nlines\n  ─ src/lib.rs\n");
    assert_eq!(at.flat(), "two\nlines");
}

#[test]
fn colour_styles_spans_not_lines() {
    let report =
        Report::error(Text::new().words("run ").code("nix develop").words(" in ").path("/w/a b"));
    let caps = Caps { color: true, links: true, width: None };
    assert_eq!(
        render(&report, caps, &ROOTS),
        "\x1b[1m\x1b[91merror\x1b[0m: run \x1b[38;5;203m\x1b[48;5;236mnix develop\x1b[0m in \
         \x1b]8;;file:///w/a%20b\x1b\\\x1b[2ma b\x1b[0m\x1b]8;;\x1b\\\n"
    );
}

#[test]
fn paths_from_workspace_then_home() {
    let line = |path: &str| render(&Report::error(Text::new().path(path)), Caps::PLAIN, &ROOTS);
    assert_eq!(line("/w/crates/x"), "error: crates/x\n");
    assert_eq!(line("/w"), "error: .\n");
    assert_eq!(line("/wx/y"), "error: /wx/y\n");
    assert_eq!(line("/home/u/.blaze"), "error: ~/.blaze\n");
    assert_eq!(line("/etc/hosts"), "error: /etc/hosts\n");
}

#[test]
fn times() {
    let took = |ns: u64| {
        let mut out = String::new();
        crate::render::took_into(&mut out, ns);
        out
    };
    assert_eq!(took(120_000_000), "0.12s");
    assert_eq!(took(38_000_000_000), "38s");
    assert_eq!(took(72_000_000_000), "1m12s");
    assert_eq!(took(7_500_000_000_000), "2h05m");
}

#[test]
fn marked_text() {
    let text = Text::marked("set `A` or `B");
    assert_eq!(text, Text::new().words("set ").code("A").words(" or ").words("`B"));
}

#[test]
fn rules_are_sorted_and_found() {
    assert!(rules::RULES.windows(2).all(|it| it[0].id < it[1].id));
    assert!(rules::RULES.iter().all(|rule| rules::find(rule.id) == Some(rule)));
    assert_eq!(rules::find("sandbox:nothing"), None);
}

#[test]
fn script_says_its_report() {
    let said = script::read(concat!(
        "cargo:rerun-if-env-changed=X\n",
        "cargo:blaze-report=headline=llvm-sys can't find LLVM\n",
        "cargo:blaze-report=rule=llvm:pin\n",
        "cargo:blaze-report=fact.found=`LLVM_SYS_231_PREFIX` is not set\r\n",
        "cargo:blaze-report=fix=nix develop\n",
        "cargo:blaze-report=fix=python3 fetch\n",
        "cargo:blaze-report=fix+=export A=b\n",
        "cargo:blaze-report=bogus=1\n",
        "cargo::error=blaze links only the LLVM\n",
    ))
    .unwrap();
    assert_eq!(said.headline, Some(Text::plain("llvm-sys can't find LLVM")));
    assert_eq!(said.rule.as_deref(), Some("llvm:pin"));
    assert_eq!(said.facts.len(), 1);
    assert_eq!(said.facts[0].text, Text::new().code("LLVM_SYS_231_PREFIX").words(" is not set"));
    assert_eq!(
        said.fixes,
        vec![Fix::run(&["nix develop"]), Fix::run(&["python3 fetch", "export A=b"])]
    );
    assert_eq!(said.unread, vec!["cargo:blaze-report=bogus=1".to_owned()]);
    assert_eq!(script::read("cargo::error=x\n"), None);
}

/// The variables of a terminal, as `detect` reads them.
fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
}

#[test]
fn terminals() {
    let config = Config::DEFAULT;
    let ghostty = terminal::detect(
        &config,
        env(&[("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "1.2.3")]),
        true,
        Some(100),
    );
    assert_eq!(ghostty.caps, Caps { color: true, links: true, width: Some(100) });
    assert!(ghostty.progress);
    assert_eq!(ghostty.notifier, Some(Notifier::Osc777));

    let iterm = terminal::detect(&config, env(&[("TERM_PROGRAM", "iTerm.app")]), true, None);
    assert_eq!(iterm.notifier, Some(Notifier::Osc9));
    assert!(!iterm.progress);

    let kitty = terminal::detect(&config, env(&[("TERM", "xterm-kitty")]), true, None);
    assert_eq!(kitty.notifier, Some(Notifier::Kitty));

    // A pipe, CI and a dumb terminal get no escape but colour, and colour only when asked.
    for (vars, terminal) in [
        (&[("TERM_PROGRAM", "ghostty")][..], false),
        (&[("TERM_PROGRAM", "ghostty"), ("CI", "true")][..], true),
        (&[("TERM", "dumb")][..], true),
    ] {
        let it = terminal::detect(&config, env(vars), terminal, Some(80));
        assert!(!it.caps.links && !it.progress && it.notifier.is_none(), "{vars:?}");
    }
    assert!(!terminal::detect(&config, env(&[("TERM", "dumb")]), true, None).caps.color);
    assert!(!terminal::detect(&config, env(&[("NO_COLOR", "1")]), true, None).caps.color);

    // Behind tmux: nothing but colour, unless its passthrough is said to be on.
    let tmux = [
        ("TERM_PROGRAM", "ghostty"),
        ("TERM_PROGRAM_VERSION", "1.2.0"),
        ("TMUX", "/tmp/tmux-1/default,1,0"),
    ];
    let behind = terminal::detect(&config, env(&tmux), true, None);
    assert!(!behind.progress && behind.notifier.is_none() && !behind.caps.links);
    let mut through = Config::DEFAULT;
    through.tmux_passthrough = true;
    let behind = terminal::detect(&through, env(&tmux), true, None);
    let mut out = String::new();
    behind.progress_into(&mut out, Progress::Normal(42));
    assert_eq!(out, "\x1bPtmux;\x1b\x1b]9;4;1;42\x07\x1b\\");
}

#[test]
fn escapes() {
    let ghostty = terminal::detect(
        &Config::DEFAULT,
        env(&[("TERM_PROGRAM", "ghostty"), ("TERM_PROGRAM_VERSION", "1.2.0")]),
        true,
        None,
    );
    let mut out = String::new();
    ghostty.progress_into(&mut out, Progress::Error(130));
    ghostty.progress_into(&mut out, Progress::Clear);
    ghostty.notify_into(&mut out, "blaze test; failed", "1 failed\n");
    assert_eq!(
        out,
        "\x1b]9;4;2;100\x07\x1b]9;4;0;0\x07\x1b]777;notify;blaze test  failed;1 failed \x07"
    );
    assert!(ghostty.notifies(true, 0));
    assert!(!ghostty.notifies(false, 3600));
    let mut always = Config::DEFAULT;
    always.notify = terminal::Notify::Always;
    let ghostty = terminal::detect(&always, env(&[("TERM_PROGRAM", "ghostty")]), true, None);
    assert!(ghostty.notifies(false, 10) && !ghostty.notifies(false, 9));
}

#[test]
fn config() {
    assert_eq!(Config::from_toml("[update]\nchannel = \"main\"\n"), Ok(Config::DEFAULT));
    let config = Config::from_toml(
        "[terminal]\ncolor = \"never\"\nnotify = \"off\"\nnotify_after_secs = 30\n",
    )
    .unwrap();
    assert_eq!(config.color, terminal::Color::Never);
    assert_eq!(config.notify, terminal::Notify::Off);
    assert_eq!(config.notify_after_secs, 30);
    assert!(
        Config::from_toml("[terminal]\ncolor = \"blue\"\n").unwrap_err().contains("terminal.color")
    );
    assert!(Config::from_toml("[terminal]\nbeep = true\n").unwrap_err().contains("`beep`"));
}
