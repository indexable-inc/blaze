//! context_signal.zig's tests (OSC 3008).

use super::*;
use std::vec::Vec;

fn ctx(s: &[u8]) -> ContextSignal<'_> {
    match p(s) {
        Some(Command::ContextSignal(c)) => c,
        other => panic!("not a context signal: {other:?}"),
    }
}

/// A command whose only metadata is `meta`, to read one field the way ghostty's enum `parse`
/// functions are tested.
fn meta(m: &[u8]) -> ContextSignal<'_> {
    ContextSignal { action: ContextAction::Start, id: b"x", metadata: m }
}

// context_signal.zig:280 "OSC 3008: basic start command"
#[test]
fn osc_3008_basic_start_command() {
    let c = ctx(b"3008;start=abc123");
    assert_eq!(c.action, ContextAction::Start);
    assert_eq!(c.id, b"abc123");
    assert_eq!(c.metadata, b"");
}

// context_signal.zig:294 "OSC 3008: basic end command"
#[test]
fn osc_3008_basic_end_command() {
    let c = ctx(b"3008;end=abc123");
    assert_eq!(c.action, ContextAction::End);
    assert_eq!(c.id, b"abc123");
    assert_eq!(c.metadata, b"");
}

// context_signal.zig:308 "OSC 3008: start with metadata fields"
#[test]
fn osc_3008_start_with_metadata_fields() {
    let c = ctx(
        b"3008;start=bed86fab93af4328bbed0a1224af6d40;type=container;user=lennart;hostname=zeta",
    );
    assert_eq!(c.action, ContextAction::Start);
    assert_eq!(c.id, b"bed86fab93af4328bbed0a1224af6d40");
    assert_eq!(c.context_type(), Some(ContextType::Container));
    assert_eq!(c.text(ContextText::User), Some(&b"lennart"[..]));
    assert_eq!(c.text(ContextText::Hostname), Some(&b"zeta"[..]));
}

// context_signal.zig:326 "OSC 3008: start with all common fields"
#[test]
fn osc_3008_start_with_all_common_fields() {
    let c = ctx(b"3008;start=myctx;type=shell;user=root;hostname=myhost;machineid=3deb5353d3ba43d08201c136a47ead7b;bootid=d4a3d0fdf2e24fdea6d971ce73f4fbf2;pid=1062862;pidfdid=1063162;comm=bash");
    assert_eq!(c.context_type(), Some(ContextType::Shell));
    assert_eq!(c.text(ContextText::User), Some(&b"root"[..]));
    assert_eq!(c.text(ContextText::Hostname), Some(&b"myhost"[..]));
    assert_eq!(c.text(ContextText::Machineid), Some(&b"3deb5353d3ba43d08201c136a47ead7b"[..]));
    assert_eq!(c.text(ContextText::Bootid), Some(&b"d4a3d0fdf2e24fdea6d971ce73f4fbf2"[..]));
    assert_eq!(c.int(ContextInt::Pid), Some(1062862));
    assert_eq!(c.int(ContextInt::Pidfdid), Some(1063162));
    assert_eq!(c.text(ContextText::Comm), Some(&b"bash"[..]));
}

// context_signal.zig:345 "OSC 3008: end with exit metadata"
#[test]
fn osc_3008_end_with_exit_metadata() {
    let c = ctx(b"3008;end=myctx;exit=success;status=0");
    assert_eq!(c.action, ContextAction::End);
    assert_eq!(c.id, b"myctx");
    assert_eq!(c.exit(), Some(ExitStatus::Success));
    assert_eq!(c.int(ContextInt::Status), Some(0));
}

// context_signal.zig:360 "OSC 3008: end with failure exit"
#[test]
fn osc_3008_end_with_failure_exit() {
    let c = ctx(b"3008;end=myctx;exit=failure;status=1;signal=SIGKILL");
    assert_eq!(c.exit(), Some(ExitStatus::Failure));
    assert_eq!(c.int(ContextInt::Status), Some(1));
    assert_eq!(c.text(ContextText::Signal), Some(&b"SIGKILL"[..]));
}

// context_signal.zig:374 "OSC 3008: unknown fields are ignored"
#[test]
fn osc_3008_unknown_fields_are_ignored() {
    let c = ctx(b"3008;start=myctx;type=shell;unknownfield=value;user=root");
    assert_eq!(c.context_type(), Some(ContextType::Shell));
    assert_eq!(c.text(ContextText::User), Some(&b"root"[..]));
}

// context_signal.zig:387 "OSC 3008: missing field returns null"
#[test]
fn osc_3008_missing_field_returns_null() {
    let c = ctx(b"3008;start=myctx;user=lennart");
    assert_eq!(c.context_type(), None);
    assert_eq!(c.text(ContextText::Hostname), None);
    assert_eq!(c.int(ContextInt::Pid), None);
}

// context_signal.zig:401 "OSC 3008: invalid prefix"
#[test]
fn osc_3008_invalid_prefix() {
    assert_eq!(p(b"3008;bogus=abc123"), None);
}

// context_signal.zig:410 "OSC 3008: empty data"
#[test]
fn osc_3008_empty_data() {
    assert_eq!(p(b"3008;start="), None);
    // Beyond ghostty's test: truly empty data, and no `;` at all.
    assert_eq!(p(b"3008;"), None);
    assert_eq!(p(b"3008"), None);
}

// context_signal.zig:422 "OSC 3008: max length context ID"
#[test]
fn osc_3008_max_length_context_id() {
    let id = [b'a'; 64];
    let mut input = b"3008;start=".to_vec();
    input.extend_from_slice(&id);
    assert_eq!(ctx(&input).id, &id[..]);
}

// context_signal.zig:435 "OSC 3008: over-length context ID"
#[test]
fn osc_3008_over_length_context_id() {
    let mut input: Vec<u8> = b"3008;start=".to_vec();
    input.extend_from_slice(&[b'a'; 65]);
    assert_eq!(p(&input), None);
}

// Beyond ghostty's tests: an id byte outside 0x20–0x7E is invalid.
#[test]
fn osc_3008_id_characters() {
    assert_eq!(p(b"3008;start=a\x7fb"), None);
    assert_eq!(p(b"3008;start=a\xc3\xa9"), None);
    assert_eq!(ctx(b"3008;start= ~").id, b" ~");
}

// context_signal.zig:445 "OSC 3008: context type enum coverage"
#[test]
fn osc_3008_context_type_enum_coverage() {
    use ContextType as T;
    let types: [(&[u8], T); 12] = [
        (b"type=boot", T::Boot),
        (b"type=container", T::Container),
        (b"type=vm", T::Vm),
        (b"type=elevate", T::Elevate),
        (b"type=chpriv", T::Chpriv),
        (b"type=subcontext", T::Subcontext),
        (b"type=remote", T::Remote),
        (b"type=shell", T::Shell),
        (b"type=command", T::Command),
        (b"type=app", T::App),
        (b"type=service", T::Service),
        (b"type=session", T::Session),
    ];
    for (m, want) in types {
        assert_eq!(meta(m).context_type(), Some(want));
    }
    assert_eq!(meta(b"type=invalid").context_type(), None);
}

// context_signal.zig:470 "OSC 3008: exit status enum coverage"
#[test]
fn osc_3008_exit_status_enum_coverage() {
    assert_eq!(meta(b"exit=success").exit(), Some(ExitStatus::Success));
    assert_eq!(meta(b"exit=failure").exit(), Some(ExitStatus::Failure));
    assert_eq!(meta(b"exit=crash").exit(), Some(ExitStatus::Crash));
    assert_eq!(meta(b"exit=interrupt").exit(), Some(ExitStatus::Interrupt));
    assert_eq!(meta(b"exit=invalid").exit(), None);
}

// Beyond ghostty's tests: integer fields take digits only; empty strings are absent; the
// first field with the key wins.
#[test]
fn osc_3008_field_edges() {
    assert_eq!(meta(b"pid=+5").int(ContextInt::Pid), None);
    assert_eq!(meta(b"pid=").int(ContextInt::Pid), None);
    assert_eq!(meta(b"pid=18446744073709551616").int(ContextInt::Pid), None);
    assert_eq!(meta(b"user=").text(ContextText::User), None);
    assert_eq!(meta(b"user=a;user=b").text(ContextText::User), Some(&b"a"[..]));
}

// context_signal.zig:480 "OSC 3008: spec example - container start"
#[test]
fn osc_3008_spec_example_container_start() {
    let c = ctx(b"3008;start=bed86fab93af4328bbed0a1224af6d40;type=container;user=lennart;hostname=zeta;machineid=3deb5353d3ba43d08201c136a47ead7b;bootid=d4a3d0fdf2e24fdea6d971ce73f4fbf2;pid=1062862;pidfdid=1063162;comm=systemd-nspawn;container=foobar");
    assert_eq!(c.action, ContextAction::Start);
    assert_eq!(c.id, b"bed86fab93af4328bbed0a1224af6d40");
    assert_eq!(c.context_type(), Some(ContextType::Container));
    assert_eq!(c.text(ContextText::User), Some(&b"lennart"[..]));
    assert_eq!(c.text(ContextText::Hostname), Some(&b"zeta"[..]));
    assert_eq!(c.text(ContextText::Comm), Some(&b"systemd-nspawn"[..]));
    assert_eq!(c.text(ContextText::Container), Some(&b"foobar"[..]));
    assert_eq!(c.int(ContextInt::Pid), Some(1062862));
}

// context_signal.zig:500 "OSC 3008: spec example - context end"
#[test]
fn osc_3008_spec_example_context_end() {
    let c = ctx(b"3008;end=bed86fab93af4328bbed0a1224af6d40");
    assert_eq!(c.action, ContextAction::End);
    assert_eq!(c.id, b"bed86fab93af4328bbed0a1224af6d40");
}

// context_signal.zig:514 "OSC 3008: cwd and cmdline fields"
#[test]
fn osc_3008_cwd_and_cmdline_fields() {
    let c = ctx(b"3008;start=myctx;type=command;cwd=/home/user;cmdline=ls -la");
    assert_eq!(c.text(ContextText::Cwd), Some(&b"/home/user"[..]));
    assert_eq!(c.text(ContextText::Cmdline), Some(&b"ls -la"[..]));
}

// context_signal.zig:527 "OSC 3008: start command with no fields"
#[test]
fn osc_3008_start_command_with_no_fields() {
    let c = ctx(b"3008;start=simpleid");
    assert_eq!(c.action, ContextAction::Start);
    assert_eq!(c.id, b"simpleid");
    assert_eq!(c.context_type(), None);
    assert_eq!(c.text(ContextText::User), None);
    assert_eq!(c.exit(), None);
}
