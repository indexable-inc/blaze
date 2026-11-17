//! `blaze agents`' decisions on fixtures: the merge of hooks and the block into agents' files
//! and their removal, the status of what is there, the hook's input and its answers (the
//! selftests of the Python adapters this replaced, ported).

use blaze_api::{ApiError, Span};
use blaze_ids::ContentId;

use super::*;

fn found(level: Level, file: &str, line: u32, message: &str, primary: bool) -> Found {
    let mut id = [0u8; 32];
    id[..message.len().min(32)].copy_from_slice(&message.as_bytes()[..message.len().min(32)]);
    Found {
        id: ContentId(id),
        code: Some("E0308".to_owned()),
        help: vec!["help: try this".to_owned()],
        span: Some(Span {
            file: file.to_owned(),
            line,
            label: None,
            column: 5,
            primary: true,
            byte_end: 0,
            end_line: line,
            byte_start: 0,
            end_column: 6,
            replacement: None,
            applicability: None,
        }),
        fixes: Vec::new(),
        level,
        message: message.to_owned(),
        package: Some("demo".to_owned()),
        primary,
        rendered: message.to_owned(),
    }
}

fn seen(base: Base, new: Vec<Found>, fixed: Vec<Found>, unchanged: Vec<Found>) -> Seen {
    Seen {
        new,
        base,
        root: ContentId([1; 32]),
        build: Some(1),
        ended: true,
        fixed,
        token: Some(ContentId([2; 32])),
        failed: None,
        unchanged,
    }
}

fn demo() -> Owner {
    Owner { manifest: PathBuf::from("/w/demo/Cargo.toml"), package: Some("demo".to_owned()), unparsed: None }
}

fn checked_of(seen: Seen) -> Checked {
    Checked::Seen { owner: demo(), seen, edited: vec!["demo/src/lib.rs".to_owned()], cwd: Some(String::new()) }
}

fn context(answer: &Value) -> &str {
    answer["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Install and remove.

#[test]
fn install_into_nothing_writes_both_hooks_and_installing_again_writes_the_same() {
    let once = merged("s.json", None, &ours(Agent::Claude, false)).unwrap();
    let value: Value = serde_json::from_str(&once).unwrap();
    assert_eq!(value["hooks"]["PostToolUse"][0]["matcher"], "Write|Edit");
    assert_eq!(value["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "blaze agents hook claude v1");
    assert_eq!(value["hooks"]["PostToolUse"][0]["hooks"][0]["timeout"], 30);
    assert_eq!(value["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    let twice = merged("s.json", Some(&once), &ours(Agent::Claude, false)).unwrap();
    assert_eq!(once, twice, "installing is idempotent");
}

#[test]
fn install_keeps_what_is_not_blazes_in_its_order_and_replaces_an_old_hook_in_place() {
    let before = r#"{
  "model": "x",
  "hooks": {
    "PostToolUse": [
      { "matcher": "Write|Edit", "hooks": [
        { "type": "command", "command": "lint-it" },
        { "type": "command", "command": "blaze agents hook claude v0", "timeout": 30 },
        { "type": "command", "command": "after-it" }
      ] }
    ]
  },
  "permissions": {}
}"#;
    let after = merged("s.json", Some(before), &ours(Agent::Claude, true)).unwrap();
    let value: Value = serde_json::from_str(&after).unwrap();
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["model", "hooks", "permissions"], "the order of what was there is kept");
    let list = value["hooks"]["PostToolUse"][0]["hooks"].as_array().unwrap();
    let commands: Vec<&str> = list.iter().map(|it| it["command"].as_str().unwrap()).collect();
    assert_eq!(commands, ["lint-it", "blaze agents hook claude v1", "after-it"]);
    assert_eq!(value["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "blaze agents hook claude v1 --strict");
    // The control: the old one is out of date before, current after.
    assert_eq!(Verdict::of(&hooked("s.json", before).unwrap(), Some(1)), Verdict::Old(0));
    assert_eq!(Verdict::of(&hooked("s.json", &after).unwrap(), Some(1)), Verdict::Current);
}

#[test]
fn install_refuses_a_file_not_of_the_agents_shape_naming_the_file_and_key() {
    let refused = merged("/h/.claude/settings.json", Some(r#"{"hooks": []}"#), &ours(Agent::Claude, false));
    assert_eq!(refused.unwrap_err(), "/h/.claude/settings.json: `hooks` is not an object");
    let refused = merged("f", Some(r#"{"hooks": {"PreToolUse": {}}}"#), &ours(Agent::Codex, false));
    assert!(refused.unwrap_err().contains("`hooks.PreToolUse` is not a list"));
    let refused = merged("f", Some("[1]"), &ours(Agent::Codex, false));
    assert!(refused.unwrap_err().contains("not a JSON object"));
    assert!(merged("f", Some("{"), &ours(Agent::Codex, false)).unwrap_err().contains("does not read as JSON"));
}

#[test]
fn remove_takes_out_only_blazes_and_what_it_left_empty() {
    let theirs = r#"{"hooks": {"PostToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "lint-it"}]}]}}"#;
    let installed = merged("f", Some(theirs), &ours(Agent::Claude, false)).unwrap();
    let removed = unmerged("f", &installed).unwrap().unwrap();
    let value: Value = serde_json::from_str(&removed).unwrap();
    assert_eq!(value, serde_json::from_str::<Value>(theirs).unwrap());
    assert_eq!(unmerged("f", &removed).unwrap(), None, "nothing of blaze's: nothing to remove");
    let alone = merged("f", None, &ours(Agent::Codex, false)).unwrap();
    assert_eq!(unmerged("f", &alone).unwrap().unwrap(), "{}\n");
    // A command that only starts with the words is not blaze's.
    let other = r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"command": "blaze agents hookish"}]}]}}"#;
    assert_eq!(unmerged("f", other).unwrap(), None);
}

#[test]
fn codex_hooks_say_their_status_and_match_apply_patch() {
    let text = merged("h.json", None, &ours(Agent::Codex, false)).unwrap();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["hooks"]["PostToolUse"][0]["matcher"], "apply_patch");
    assert_eq!(value["hooks"]["PostToolUse"][0]["hooks"][0]["statusMessage"], "blaze check");
    assert_eq!(files(Agent::Codex, Scope::User, Path::new("/h")).hooks, Path::new("/h/.codex/hooks.json"));
    assert_eq!(files(Agent::Codex, Scope::Project, Path::new("/w")).instructions, Path::new("/w/AGENTS.md"));
    assert_eq!(files(Agent::Claude, Scope::User, Path::new("/h")).instructions, Path::new("/h/.claude/CLAUDE.md"));
}

#[test]
fn the_block_is_put_in_replaced_in_place_and_taken_out_leaving_the_rest_as_it_was() {
    let theirs = "# Project\n\nOur rules.\n";
    let with = with_block("CLAUDE.md", Some(theirs)).unwrap();
    assert!(with.starts_with(theirs));
    assert_eq!(block_version("CLAUDE.md", &with).unwrap(), Some(VERSION));
    assert_eq!(with_block("CLAUDE.md", Some(&with)).unwrap(), with, "idempotent");
    assert_eq!(without_block("CLAUDE.md", &with).unwrap().unwrap(), theirs);
    // An old block in the middle is replaced where it is.
    let old = format!("top\n{BEGIN}0{BEGIN_END}\nold words\n{BEGIN}0{END_END}\nbottom\n");
    let new = with_block("AGENTS.md", Some(&old)).unwrap();
    assert!(new.starts_with("top\n") && new.ends_with("\nbottom\n") && !new.contains("old words"));
    assert_eq!(without_block("AGENTS.md", &new).unwrap().unwrap(), "top\nbottom\n");
    // A begin with no end is refused, naming the file.
    let broken = format!("x\n{BEGIN}1{BEGIN_END}\nhalf");
    assert!(with_block("AGENTS.md", Some(&broken)).unwrap_err().starts_with("AGENTS.md has blaze's begin marker"));
    assert_eq!(without_block("x", "no block\n").unwrap(), None);
}

#[test]
fn status_says_missing_partial_old_current_and_newer() {
    let after = Hooked { event: "PostToolUse".to_owned(), matcher: None, version: Some(VERSION), strict: false };
    let before = Hooked { event: "PreToolUse".to_owned(), ..after.clone() };
    assert_eq!(Verdict::of(&[], None), Verdict::Missing);
    assert_eq!(Verdict::of(std::slice::from_ref(&after), None), Verdict::Partial);
    assert_eq!(Verdict::of(&[after.clone(), before.clone()], Some(VERSION)), Verdict::Current);
    assert_eq!(Verdict::of(&[after.clone(), before.clone()], Some(0)), Verdict::Old(0));
    assert_eq!(Verdict::of(&[after.clone(), before.clone()], Some(VERSION + 1)), Verdict::Newer(VERSION + 1));
    // The plugin's hook carries no version: it follows the blaze on PATH.
    let plugin = Hooked { version: None, ..after };
    assert_eq!(Verdict::of(&[plugin, before], Some(VERSION)), Verdict::Current);
}

#[test]
fn a_dry_runs_diff_marks_what_is_added_and_taken_out() {
    let diff = diff("f", "a\nb\nc\n", "a\nB\nc\nd\n");
    assert_eq!(diff, "--- f\n+++ f (after)\n@@\n a\n-b\n+B\n c\n+d\n");
    assert_eq!(super::diff("f", "same\n", "same\n"), "--- f\n+++ f (after)\n");
}

// ---------------------------------------------------------------------------------------------
// The hook's input.

#[test]
fn the_hook_input_is_read_leniently_for_each_agent() {
    let claude = serde_json::json!({
        "session_id": "s", "cwd": "/w", "hook_event_name": "PostToolUse", "tool_name": "Write",
        "tool_input": { "file_path": "/w/demo/src/lib.rs", "content": "x" }, "new_field": 1,
    });
    let input = input(Agent::Claude, &claude).unwrap();
    assert_eq!((input.event, input.files), (Event::After, vec!["/w/demo/src/lib.rs".to_owned()]));

    let patch = "*** Begin Patch\n*** Update File: demo/src/lib.rs\n@@\n*** Add File: demo/src/b.rs\n*** Update File: demo/src/lib.rs\n*** Move to: demo/src/c.rs\n*** End Patch";
    let codex = serde_json::json!({
        "session_id": "s", "cwd": "/w", "hook_event_name": "PostToolUse", "tool_name": "apply_patch",
        "tool_input": { "command": patch },
    });
    let read = super::input(Agent::Codex, &codex).unwrap();
    assert_eq!(read.files, ["demo/src/lib.rs", "demo/src/b.rs", "demo/src/c.rs"], "each file once");

    let none = serde_json::json!({ "session_id": "s", "cwd": "/w", "tool_name": "apply_patch", "tool_input": { "command": "nothing" } });
    assert!(super::input(Agent::Codex, &none).unwrap().unread.unwrap().contains("no `*** ... File:` line"));

    let shell = serde_json::json!({ "session_id": "s", "cwd": "/w", "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": { "command": "cargo test -p x" } });
    let read = super::input(Agent::Claude, &shell).unwrap();
    assert_eq!((read.event, read.command.as_deref()), (Event::Before, Some("cargo test -p x")));

    assert!(super::input(Agent::Claude, &serde_json::json!({ "cwd": "/w" })).unwrap_err().contains("session_id"));
}

#[test]
fn cargo_and_rustc_are_found_in_a_shell_command_and_blazes_are_not() {
    assert_eq!(cargo_in("cargo build --release").as_deref(), Some("cargo build"));
    assert_eq!(cargo_in("cd x && RUSTFLAGS=-g cargo +nightly t -p y").as_deref(), Some("cargo t"));
    assert_eq!(cargo_in("env A=1 /usr/bin/rustc main.rs").as_deref(), Some("rustc"));
    assert_eq!(cargo_in("ls | cargo clippy").as_deref(), Some("cargo clippy"));
    assert_eq!(cargo_in("cargo blaze build"), None);
    assert_eq!(cargo_in("cargo fmt && cargo metadata"), None);
    assert_eq!(cargo_in("blaze build"), None);
    assert_eq!(cargo_in("echo cargo build"), None);
}

#[test]
fn the_before_hook_advises_and_under_strict_refuses() {
    let advice = before_answer("cargo test", false);
    assert!(advice["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("`blaze test` with the same flags"));
    assert!(advice["hookSpecificOutput"].get("permissionDecision").is_none());
    let refusal = before_answer("cargo test", true);
    assert_eq!(refusal["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(refusal["hookSpecificOutput"]["hookEventName"], "PreToolUse");
}

#[test]
fn the_owner_is_the_nearest_package_a_virtual_workspace_or_a_manifest_that_does_not_parse() {
    let package = (PathBuf::from("/w/demo/Cargo.toml"), Ok("[package]\nname = \"demo\"\n".to_owned()));
    let workspace = (PathBuf::from("/w/Cargo.toml"), Ok("[workspace]\nmembers = [\"demo\"]\n".to_owned()));
    assert_eq!(owner_of(&[package.clone(), workspace.clone()]), Some(demo()));
    let virtual_only = owner_of(std::slice::from_ref(&workspace)).unwrap();
    assert_eq!(virtual_only.package, None);
    assert!(virtual_only.check().contains(&"--workspace".to_owned()));
    let broken = (PathBuf::from("/w/demo/Cargo.toml"), Ok("[package\n".to_owned()));
    let owner = owner_of(&[broken, workspace]).unwrap();
    assert!(owner.unparsed.unwrap().contains("does not read as TOML"));
    assert_eq!(owner_of(&[]), None);
    assert_eq!(demo().check(), ["check", "--manifest-path", "/w/demo/Cargo.toml", "-p", "demo", "--all-targets"]);
    assert!(checked(Path::new("a/b.rs")) && checked(Path::new("a/Cargo.toml")) && !checked(Path::new("a/README.md")));
}

// ---------------------------------------------------------------------------------------------
// The hook's answers.

#[test]
fn a_first_check_shows_the_edited_files_and_counts_the_rest() {
    let new = vec![
        found(Level::Error, "demo/src/lib.rs", 3, "mismatched types", true),
        found(Level::Warning, "demo/src/other.rs", 9, "unused variable", true),
        found(Level::Error, "demo/src/other.rs", 4, "cannot find value", true),
    ];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::First, new, Vec::new(), Vec::new()))], &[]).unwrap();
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.starts_with("The edit was applied, and blaze check found new errors in it"));
    assert!(reason.contains("blaze check -p demo: 1 error in lib.rs:\ndemo/src/lib.rs:3:5: error[E0308]: mismatched types\n  help: try this"));
    assert!(!reason.contains("cannot find value"));
    assert!(context(&answer).contains("first check this session; elsewhere in demo: 1 error, 1 warning (`blaze diagnostics -p demo` shows them)."));
    assert!(answer.get("systemMessage").is_none());
}

#[test]
fn a_new_error_blocks_and_a_codex_patch_is_said_as_one() {
    let new = vec![found(Level::Error, "demo/src/lib.rs", 3, "mismatched types", true)];
    let result = checked_of(seen(Base::Held, new, Vec::new(), Vec::new()));
    let answer = after_answer(Agent::Codex, &[result], &[]).unwrap();
    assert_eq!(answer["decision"], "block");
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.starts_with("The patch was applied"));
    assert!(reason.contains("blaze check -p demo: 1 new error in demo:"));
    assert!(context(&answer).contains("blaze check -p demo: 1 new, 0 fixed, 0 unchanged."));
}

#[test]
fn a_fixed_warning_is_counted_and_nothing_blocks() {
    let fixed = vec![found(Level::Warning, "demo/src/lib.rs", 2, "unused import", true)];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Held, Vec::new(), fixed, Vec::new()))], &[]).unwrap();
    assert!(answer.get("decision").is_none());
    assert_eq!(context(&answer), "blaze check -p demo: nothing new, 1 fixed, 0 unchanged.");
}

#[test]
fn a_moved_line_is_unchanged_and_an_error_still_there_is_said_as_such() {
    // The daemon's ids keep a moved diagnostic the same (blaze-api's diagnostics tests); the
    // hook says what the daemon says is unchanged.
    let unchanged = vec![
        found(Level::Error, "demo/src/lib.rs", 30, "mismatched types", true),
        found(Level::Warning, "demo/src/lib.rs", 31, "unused", true),
    ];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Held, Vec::new(), Vec::new(), unchanged))], &[]).unwrap();
    assert!(answer.get("decision").is_none(), "an error said before does not block again");
    assert_eq!(
        context(&answer),
        "blaze check -p demo: nothing new, 0 fixed, 2 unchanged. 1 error said before still there (`blaze diagnostics -p demo` shows all)."
    );
}

#[test]
fn a_dependencys_warning_is_not_said_and_its_error_is() {
    let new = vec![
        found(Level::Warning, "dep/src/lib.rs", 1, "a dependency's warning", false),
        found(Level::Error, "dep/src/lib.rs", 2, "a dependency's error", false),
    ];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Held, new, Vec::new(), Vec::new()))], &[]).unwrap();
    assert!(answer["reason"].as_str().unwrap().contains("a dependency's error"));
    assert!(!answer.to_string().contains("a dependency's warning"));
}

#[test]
fn a_check_past_the_deadline_is_said_still_running_to_the_model_only() {
    let mut running = seen(Base::Held, Vec::new(), Vec::new(), Vec::new());
    running.ended = false;
    let answer = after_answer(Agent::Claude, &[checked_of(running)], &[]).unwrap();
    assert_eq!(
        context(&answer),
        "blaze check did not answer in 20 s: it is still running in the daemon; run `blaze diagnostics -p demo` to see it"
    );
    assert!(answer.get("systemMessage").is_none() && answer.get("decision").is_none());
    let late = after_answer(Agent::Claude, &[Checked::Late { owner: demo() }], &[]).unwrap();
    assert!(context(&late).contains("demo was not checked"));
}

#[test]
fn blaze_missing_or_failing_is_said_as_blazes_to_the_model_and_the_person() {
    // Missing: no daemon could be reached or started.
    let missing = Checked::Failed { owner: demo(), what: "could not start blaze serve: No such file".to_owned() };
    let answer = after_answer(Agent::Claude, &[missing], &[]).unwrap();
    assert_eq!(answer["systemMessage"], "blaze itself failed while checking demo, not the code: could not start blaze serve: No such file");
    assert!(answer.get("decision").is_none());
    // Failing: the daemon answered that blaze failed in the check; its last lines only.
    let mut failing = seen(Base::Held, Vec::new(), Vec::new(), Vec::new());
    let long: Vec<String> = (0..40).map(|it| format!("line {it}")).collect();
    failing.failed = Some(ApiError::new(long.join("\n"), None));
    let answer = after_answer(Agent::Claude, &[checked_of(failing)], &[]).unwrap();
    let said = context(&answer);
    assert!(said.contains("line 39") && !said.contains("line 24\n") && said.contains("line 25"));
    assert_eq!(answer["systemMessage"], "blaze itself failed while checking demo, not the code: line 25");
    // The hook's own failure goes to both.
    let own = hook_failed("PostToolUse", "the hook input is not JSON");
    assert_eq!(own["systemMessage"], own["hookSpecificOutput"]["additionalContext"]);
}

#[test]
fn many_new_errors_are_capped_and_the_rest_counted() {
    let new: Vec<Found> =
        (0..25).map(|it| found(Level::Error, "demo/src/lib.rs", it + 1, &format!("error number {it:02}"), true)).collect();
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Held, new, Vec::new(), Vec::new()))], &[]).unwrap();
    let reason = answer["reason"].as_str().unwrap();
    assert_eq!(reason.matches(": error[E0308]: ").count(), CAP);
    assert!(reason.contains("blaze check -p demo: 25 new errors in demo:"));
    assert!(reason.ends_with("and 15 more: run `blaze diagnostics -p demo`"));
    // The budget: one huge message is not drawn whole past it.
    let huge = vec![found(Level::Error, "demo/src/lib.rs", 1, &"x".repeat(TEXT_BUDGET + 1), true)];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Held, huge, Vec::new(), Vec::new()))], &[]).unwrap();
    assert!(answer["reason"].as_str().unwrap().len() < TEXT_BUDGET);
}

#[test]
fn names_are_shown_from_the_directory_the_agent_runs_in() {
    let new = vec![found(Level::Error, "demo/src/lib.rs", 3, "mismatched types", true)];
    let result = Checked::Seen {
        owner: demo(),
        seen: seen(Base::Held, new, Vec::new(), Vec::new()),
        edited: vec!["demo/src/lib.rs".to_owned()],
        cwd: Some("demo".to_owned()),
    };
    let answer = after_answer(Agent::Claude, &[result], &[]).unwrap();
    assert!(answer["reason"].as_str().unwrap().contains("\nsrc/lib.rs:3:5: error"));
}

#[test]
fn an_unknown_base_is_said_and_shown_as_a_first_check() {
    let new = vec![found(Level::Warning, "demo/src/lib.rs", 3, "unused", true)];
    let answer = after_answer(Agent::Claude, &[checked_of(seen(Base::Unknown, new, Vec::new(), Vec::new()))], &[]).unwrap();
    assert!(context(&answer).contains("1 warning in lib.rs"));
    assert!(context(&answer).contains("the daemon no longer holds this session's last check of demo"));
}

#[test]
fn nothing_to_say_is_no_answer_and_an_unread_write_is_said() {
    assert_eq!(after_answer(Agent::Claude, &[], &[]), None);
    let answer = after_answer(Agent::Codex, &[], &["checked nothing".to_owned()]).unwrap();
    assert_eq!(context(&answer), "checked nothing");
}
