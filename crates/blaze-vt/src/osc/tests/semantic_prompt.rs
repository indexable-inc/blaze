//! OSC 133 (osc/parsers/semantic_prompt.zig) and the command-line decoders
//! (os/string_encoding.zig), ghostty's tests ported by name.

use super::*;
use PromptAction as A;

/// Parse an OSC 133 that must be a semantic prompt with `action`.
fn sp(input: &[u8], action: PromptAction) -> SemanticPrompt<'_> {
    match p(input) {
        Some(Command::SemanticPrompt(s)) => {
            assert_eq!(s.action, action, "{:?}", core::str::from_utf8(input));
            s
        }
        other => panic!("not a semantic prompt: {other:?}"),
    }
}

/// The options alone, as ghostty's `Option.read` tests pass a raw string.
fn opts(raw: &[u8]) -> SemanticPrompt<'_> {
    SemanticPrompt { action: A::FreshLineNewPrompt, options: raw }
}

fn cmdline(s: &SemanticPrompt<'_>) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    s.write_command_line(&mut |b| out.push(b))?;
    Ok(out)
}

fn printf_q(s: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    printf_q_decode(s, &mut |b| out.push(b))?;
    Ok(out)
}

fn percent(s: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    url_percent_decode(s, &mut |b| out.push(b))?;
    Ok(out)
}

// semantic_prompt.zig:396 "OSC 133: end_input_start_output"
#[test]
fn osc_133_end_input_start_output() {
    let s = sp(b"133;C", A::EndInputStartOutput);
    assert_eq!(s.aid(), None);
    assert_eq!(s.cl(), None);
}

// semantic_prompt.zig:411 "OSC 133: end_input_start_output extra contents"
#[test]
fn osc_133_end_input_start_output_extra_contents() {
    assert_eq!(p(b"133;Cextra"), None);
}

// semantic_prompt.zig:420 "OSC 133: end_input_start_output with options"
#[test]
fn osc_133_end_input_start_output_with_options() {
    let s = sp(b"133;C;aid=foo", A::EndInputStartOutput);
    assert_eq!(s.aid(), Some(&b"foo"[..]));
}

// semantic_prompt.zig:433 "OSC 133: end_input_start_output with cmdline"
#[test]
fn osc_133_end_input_start_output_with_cmdline() {
    let s = sp(b"133;C;cmdline=echo bobr kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa");
}

// semantic_prompt.zig:451 "OSC 133: end_input_start_output with cmdline 3"
#[test]
fn osc_133_end_input_start_output_with_cmdline_3() {
    let s = sp(b"133;C;cmdline=echo bobr\\nkurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr\nkurwa");
}

// semantic_prompt.zig:468 "OSC 133: end_input_start_output with cmdline 4"
#[test]
fn osc_133_end_input_start_output_with_cmdline_4() {
    let s = sp(b"133;C;cmdline=$'echo bobr kurwa'", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa");
}

// semantic_prompt.zig:486 "OSC 133: end_input_start_output with cmdline 5"
#[test]
fn osc_133_end_input_start_output_with_cmdline_5() {
    let s = sp(b"133;C;cmdline='echo bobr kurwa'", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa");
}

// semantic_prompt.zig:504 "OSC 133: end_input_start_output with cmdline 6"
#[test]
fn osc_133_end_input_start_output_with_cmdline_6() {
    let s = sp(b"133;C;cmdline='echo bobr kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:520 "OSC 133: end_input_start_output with cmdline 7"
#[test]
fn osc_133_end_input_start_output_with_cmdline_7() {
    let s = sp(b"133;C;cmdline=$'echo bobr kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:537 "OSC 133: end_input_start_output with cmdline 8"
#[test]
fn osc_133_end_input_start_output_with_cmdline_8() {
    let s = sp(b"133;C;cmdline=$'", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:553 "OSC 133: end_input_start_output with cmdline 9"
#[test]
fn osc_133_end_input_start_output_with_cmdline_9() {
    let s = sp(b"133;C;cmdline=", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"");
}

// semantic_prompt.zig:571 "OSC 133: end_input_start_output with cmdline_url 1"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_1() {
    let s = sp(b"133;C;cmdline_url=echo bobr kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa");
}

// semantic_prompt.zig:589 "OSC 133: end_input_start_output with cmdline_url 2"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_2() {
    let s = sp(b"133;C;cmdline_url=echo bobr%20kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa");
}

// semantic_prompt.zig:607 "OSC 133: end_input_start_output with cmdline_url 3"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_3() {
    let s = sp(b"133;C;cmdline_url=echo bobr%3bkurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr;kurwa");
}

// semantic_prompt.zig:625 "OSC 133: end_input_start_output with cmdline_url 4"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_4() {
    let s = sp(b"133;C;cmdline_url=echo bobr%3kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:641 "OSC 133: end_input_start_output with cmdline_url 5"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_5() {
    let s = sp(b"133;C;cmdline_url=echo bobr%kurwa", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:657 "OSC 133: end_input_start_output with cmdline_url 6"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_6() {
    let s = sp(b"133;C;cmdline_url=echo bobr kurwa%20", A::EndInputStartOutput);
    assert_eq!(cmdline(&s).unwrap(), b"echo bobr kurwa ");
}

// semantic_prompt.zig:675 "OSC 133: end_input_start_output with cmdline_url 7"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_7() {
    let s = sp(b"133;C;cmdline_url=echo bobr kurwa%2", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:691 "OSC 133: end_input_start_output with cmdline_url 8"
#[test]
fn osc_133_end_input_start_output_with_cmdline_url_8() {
    let s = sp(b"133;C;cmdline_url=echo bobr kurwa%", A::EndInputStartOutput);
    assert_eq!(cmdline(&s), Err(DecodeError));
}

// semantic_prompt.zig:707 "OSC 133: fresh_line"
#[test]
fn osc_133_fresh_line() {
    sp(b"133;L", A::FreshLine);
}

// semantic_prompt.zig:720 "OSC 133: fresh_line extra contents"
#[test]
fn osc_133_fresh_line_extra_contents() {
    assert_eq!(p(b"133;Lol"), None);
    assert_eq!(p(b"133;L;aid=foo"), None);
}

// semantic_prompt.zig:740 "OSC 133: fresh_line_new_prompt"
#[test]
fn osc_133_fresh_line_new_prompt() {
    let s = sp(b"133;A", A::FreshLineNewPrompt);
    assert_eq!(s.aid(), None);
    assert_eq!(s.cl(), None);
}

// semantic_prompt.zig:755 "OSC 133: fresh_line_new_prompt with aid"
#[test]
fn osc_133_fresh_line_new_prompt_with_aid() {
    assert_eq!(sp(b"133;A;aid=14", A::FreshLineNewPrompt).aid(), Some(&b"14"[..]));
}

// semantic_prompt.zig:769 "OSC 133: fresh_line_new_prompt with '=' in aid"
#[test]
fn osc_133_fresh_line_new_prompt_with_eq_in_aid() {
    assert_eq!(sp(b"133;A;aid=a=b", A::FreshLineNewPrompt).aid(), Some(&b"a=b"[..]));
}

// semantic_prompt.zig:783 "OSC 133: fresh_line_new_prompt with cl=line"
#[test]
fn osc_133_fresh_line_new_prompt_with_cl_line() {
    assert_eq!(sp(b"133;A;cl=line", A::FreshLineNewPrompt).cl(), Some(Click::Line));
}

// semantic_prompt.zig:797 "OSC 133: fresh_line_new_prompt with cl=m"
#[test]
fn osc_133_fresh_line_new_prompt_with_cl_m() {
    assert_eq!(sp(b"133;A;cl=m", A::FreshLineNewPrompt).cl(), Some(Click::Multiple));
}

// semantic_prompt.zig:811 "OSC 133: fresh_line_new_prompt with invalid cl"
#[test]
fn osc_133_fresh_line_new_prompt_with_invalid_cl() {
    assert_eq!(sp(b"133;A;cl=invalid", A::FreshLineNewPrompt).cl(), None);
}

// semantic_prompt.zig:825 "OSC 133: fresh_line_new_prompt with trailing ;"
#[test]
fn osc_133_fresh_line_new_prompt_with_trailing_semicolon() {
    sp(b"133;A;", A::FreshLineNewPrompt);
}

// semantic_prompt.zig:838 "OSC 133: fresh_line_new_prompt with bare key"
#[test]
fn osc_133_fresh_line_new_prompt_with_bare_key() {
    let s = sp(b"133;A;barekey", A::FreshLineNewPrompt);
    assert_eq!(s.aid(), None);
    assert_eq!(s.cl(), None);
}

// semantic_prompt.zig:853 "OSC 133: fresh_line_new_prompt with multiple options"
#[test]
fn osc_133_fresh_line_new_prompt_with_multiple_options() {
    let s = sp(b"133;A;aid=foo;cl=line", A::FreshLineNewPrompt);
    assert_eq!(s.aid(), Some(&b"foo"[..]));
    assert_eq!(s.cl(), Some(Click::Line));
}

// semantic_prompt.zig:868 "OSC 133: fresh_line_new_prompt default redraw"
#[test]
fn osc_133_fresh_line_new_prompt_default_redraw() {
    assert_eq!(sp(b"133;A", A::FreshLineNewPrompt).redraw(), None);
}

// semantic_prompt.zig:882 "OSC 133: fresh_line_new_prompt with redraw=0"
#[test]
fn osc_133_fresh_line_new_prompt_with_redraw_0() {
    assert_eq!(sp(b"133;A;redraw=0", A::FreshLineNewPrompt).redraw(), Some(Redraw::False));
}

// semantic_prompt.zig:896 "OSC 133: fresh_line_new_prompt with redraw=1"
#[test]
fn osc_133_fresh_line_new_prompt_with_redraw_1() {
    assert_eq!(sp(b"133;A;redraw=1", A::FreshLineNewPrompt).redraw(), Some(Redraw::True));
}

// semantic_prompt.zig:910 "OSC 133: fresh_line_new_prompt with invalid redraw"
#[test]
fn osc_133_fresh_line_new_prompt_with_invalid_redraw() {
    assert_eq!(sp(b"133;A;redraw=x", A::FreshLineNewPrompt).redraw(), None);
}

// semantic_prompt.zig:924 "OSC 133: prompt_start"
#[test]
fn osc_133_prompt_start() {
    assert_eq!(sp(b"133;P", A::PromptStart).prompt_kind(), None);
}

// semantic_prompt.zig:938 "OSC 133: prompt_start with k=i"
#[test]
fn osc_133_prompt_start_with_k_i() {
    assert_eq!(sp(b"133;P;k=i", A::PromptStart).prompt_kind(), Some(PromptKind::Initial));
}

// semantic_prompt.zig:952 "OSC 133: prompt_start with k=r"
#[test]
fn osc_133_prompt_start_with_k_r() {
    assert_eq!(sp(b"133;P;k=r", A::PromptStart).prompt_kind(), Some(PromptKind::Right));
}

// semantic_prompt.zig:966 "OSC 133: prompt_start with k=c"
#[test]
fn osc_133_prompt_start_with_k_c() {
    assert_eq!(sp(b"133;P;k=c", A::PromptStart).prompt_kind(), Some(PromptKind::Continuation));
}

// semantic_prompt.zig:980 "OSC 133: prompt_start with k=s"
#[test]
fn osc_133_prompt_start_with_k_s() {
    assert_eq!(sp(b"133;P;k=s", A::PromptStart).prompt_kind(), Some(PromptKind::Secondary));
}

// semantic_prompt.zig:994 "OSC 133: prompt_start with invalid k"
#[test]
fn osc_133_prompt_start_with_invalid_k() {
    assert_eq!(sp(b"133;P;k=x", A::PromptStart).prompt_kind(), None);
}

// semantic_prompt.zig:1008 "OSC 133: prompt_start extra contents"
#[test]
fn osc_133_prompt_start_extra_contents() {
    assert_eq!(p(b"133;Pextra"), None);
}

// semantic_prompt.zig:1017 "OSC 133: new_command"
#[test]
fn osc_133_new_command() {
    let s = sp(b"133;N", A::NewCommand);
    assert_eq!(s.aid(), None);
    assert_eq!(s.cl(), None);
}

// semantic_prompt.zig:1032 "OSC 133: new_command with aid"
#[test]
fn osc_133_new_command_with_aid() {
    assert_eq!(sp(b"133;N;aid=foo", A::NewCommand).aid(), Some(&b"foo"[..]));
}

// semantic_prompt.zig:1046 "OSC 133: new_command with cl=line"
#[test]
fn osc_133_new_command_with_cl_line() {
    assert_eq!(sp(b"133;N;cl=line", A::NewCommand).cl(), Some(Click::Line));
}

// semantic_prompt.zig:1060 "OSC 133: new_command with multiple options"
#[test]
fn osc_133_new_command_with_multiple_options() {
    let s = sp(b"133;N;aid=foo;cl=line", A::NewCommand);
    assert_eq!(s.aid(), Some(&b"foo"[..]));
    assert_eq!(s.cl(), Some(Click::Line));
}

// semantic_prompt.zig:1075 "OSC 133: new_command extra contents"
#[test]
fn osc_133_new_command_extra_contents() {
    assert_eq!(p(b"133;Nextra"), None);
}

// semantic_prompt.zig:1084 "OSC 133: end_prompt_start_input"
#[test]
fn osc_133_end_prompt_start_input() {
    sp(b"133;B", A::EndPromptStartInput);
}

// semantic_prompt.zig:1097 "OSC 133: end_prompt_start_input extra contents"
#[test]
fn osc_133_end_prompt_start_input_extra_contents() {
    assert_eq!(p(b"133;Bextra"), None);
}

// semantic_prompt.zig:1106 "OSC 133: end_prompt_start_input with options"
#[test]
fn osc_133_end_prompt_start_input_with_options() {
    assert_eq!(sp(b"133;B;aid=foo", A::EndPromptStartInput).aid(), Some(&b"foo"[..]));
}

// semantic_prompt.zig:1119 "OSC 133: end_prompt_start_input_terminate_eol"
#[test]
fn osc_133_end_prompt_start_input_terminate_eol() {
    sp(b"133;I", A::EndPromptStartInputTerminateEol);
}

// semantic_prompt.zig:1132 "OSC 133: end_prompt_start_input_terminate_eol extra contents"
#[test]
fn osc_133_end_prompt_start_input_terminate_eol_extra_contents() {
    assert_eq!(p(b"133;Iextra"), None);
}

// semantic_prompt.zig:1141 "OSC 133: end_prompt_start_input_terminate_eol with options"
#[test]
fn osc_133_end_prompt_start_input_terminate_eol_with_options() {
    assert_eq!(sp(b"133;I;aid=foo", A::EndPromptStartInputTerminateEol).aid(), Some(&b"foo"[..]));
}

// semantic_prompt.zig:1154 "OSC 133: end_command"
#[test]
fn osc_133_end_command() {
    let s = sp(b"133;D", A::EndCommand);
    assert_eq!(s.exit_code(), None);
    assert_eq!(s.aid(), None);
    assert_eq!(s.err(), None);
}

// semantic_prompt.zig:1170 "OSC 133: end_command extra contents"
#[test]
fn osc_133_end_command_extra_contents() {
    assert_eq!(p(b"133;Dextra"), None);
}

// semantic_prompt.zig:1179 "OSC 133: end_command with exit code 0"
#[test]
fn osc_133_end_command_with_exit_code_0() {
    assert_eq!(sp(b"133;D;0", A::EndCommand).exit_code(), Some(0));
}

// semantic_prompt.zig:1193 "OSC 133: end_command with exit code and aid"
#[test]
fn osc_133_end_command_with_exit_code_and_aid() {
    let s = sp(b"133;D;12;aid=foo", A::EndCommand);
    assert_eq!(s.aid(), Some(&b"foo"[..]));
    assert_eq!(s.exit_code(), Some(12));
}

// semantic_prompt.zig:1208 "Option.read aid"
#[test]
fn option_read_aid() {
    assert_eq!(opts(b"aid=test123").aid(), Some(&b"test123"[..]));
    assert_eq!(opts(b"cl=line;aid=myaid;k=i").aid(), Some(&b"myaid"[..]));
    assert_eq!(opts(b"cl=line;k=i").aid(), None);
    assert_eq!(opts(b"aid=").aid(), Some(&b""[..]));
    assert_eq!(opts(b"k=i;aid=last").aid(), Some(&b"last"[..]));
    assert_eq!(opts(b"aid=first;k=i").aid(), Some(&b"first"[..]));
    assert_eq!(opts(b"").aid(), None);
    assert_eq!(opts(b"aid").aid(), None);
    assert_eq!(opts(b";;aid=value;;").aid(), Some(&b"value"[..]));
}

// semantic_prompt.zig:1221 "Option.read cl"
#[test]
fn option_read_cl() {
    assert_eq!(opts(b"cl=line").cl(), Some(Click::Line));
    assert_eq!(opts(b"cl=m").cl(), Some(Click::Multiple));
    assert_eq!(opts(b"cl=v").cl(), Some(Click::ConservativeVertical));
    assert_eq!(opts(b"cl=w").cl(), Some(Click::SmartVertical));
    assert_eq!(opts(b"cl=invalid").cl(), None);
    assert_eq!(opts(b"aid=foo").cl(), None);
}

// semantic_prompt.zig:1231 "Option.read prompt_kind"
#[test]
fn option_read_prompt_kind() {
    assert_eq!(opts(b"k=i").prompt_kind(), Some(PromptKind::Initial));
    assert_eq!(opts(b"k=r").prompt_kind(), Some(PromptKind::Right));
    assert_eq!(opts(b"k=c").prompt_kind(), Some(PromptKind::Continuation));
    assert_eq!(opts(b"k=s").prompt_kind(), Some(PromptKind::Secondary));
    assert_eq!(opts(b"k=x").prompt_kind(), None);
    assert_eq!(opts(b"k=ii").prompt_kind(), None);
    assert_eq!(opts(b"k=").prompt_kind(), None);
}

// semantic_prompt.zig:1242 "Option.read err"
#[test]
fn option_read_err() {
    assert_eq!(opts(b"err=some_error").err(), Some(&b"some_error"[..]));
    assert_eq!(opts(b"aid=foo").err(), None);
}

// semantic_prompt.zig:1248 "Option.read redraw"
#[test]
fn option_read_redraw() {
    assert_eq!(opts(b"redraw=1").redraw(), Some(Redraw::True));
    assert_eq!(opts(b"redraw=0").redraw(), Some(Redraw::False));
    assert_eq!(opts(b"redraw=last").redraw(), Some(Redraw::Last));
    assert_eq!(opts(b"redraw=2").redraw(), None);
    assert_eq!(opts(b"redraw=10").redraw(), None);
    assert_eq!(opts(b"redraw=").redraw(), None);
}

// semantic_prompt.zig:1258 "Option.read special_key"
#[test]
fn option_read_special_key() {
    assert_eq!(opts(b"special_key=1").special_key(), Some(true));
    assert_eq!(opts(b"special_key=0").special_key(), Some(false));
    assert_eq!(opts(b"special_key=x").special_key(), None);
}

// semantic_prompt.zig:1265 "Option.read click_events"
#[test]
fn option_read_click_events() {
    assert_eq!(opts(b"click_events=yes").click_events(), None);
    assert_eq!(opts(b"click_events=0").click_events(), None);
    assert_eq!(opts(b"click_events=1").click_events(), Some(ClickEvents::Absolute));
    assert_eq!(opts(b"click_events=2").click_events(), Some(ClickEvents::Relative));
}

// semantic_prompt.zig:1273 "Option.read exit_code"
#[test]
fn option_read_exit_code() {
    assert_eq!(opts(b"42").exit_code(), Some(42));
    assert_eq!(opts(b"0").exit_code(), Some(0));
    assert_eq!(opts(b"-1").exit_code(), Some(-1));
    assert_eq!(opts(b"abc").exit_code(), None);
    assert_eq!(opts(b"127;aid=foo").exit_code(), Some(127));
}

// Beyond ghostty's tests: the first matching key wins even when its value is invalid.
#[test]
fn option_read_first_match_wins() {
    assert_eq!(opts(b"cl=bad;cl=line").cl(), None);
    assert_eq!(opts(b"aid=a;aid=b").aid(), Some(&b"a"[..]));
}

// string_encoding.zig:68 "printf_q 1"
#[test]
fn printf_q_1() {
    assert_eq!(printf_q(b"bobr\\ kurwa").unwrap(), b"bobr kurwa");
}

// string_encoding.zig:78 "printf_q 2"
#[test]
fn printf_q_2() {
    assert_eq!(printf_q(b"bobr\\nkurwa").unwrap(), b"bobr\nkurwa");
}

// string_encoding.zig:88 "printf_q 3"
#[test]
fn printf_q_3() {
    assert_eq!(printf_q(b"bobr\\dkurwa"), Err(DecodeError));
}

// string_encoding.zig:97 "printf_q 4"
#[test]
fn printf_q_4() {
    assert_eq!(printf_q(b"bobr kurwa\\"), Err(DecodeError));
}

// string_encoding.zig:106 "printf_q 5"
#[test]
fn printf_q_5() {
    assert_eq!(printf_q(b"$'bobr kurwa'").unwrap(), b"bobr kurwa");
}

// string_encoding.zig:116 "printf_q 6"
#[test]
fn printf_q_6() {
    assert_eq!(printf_q(b"'bobr kurwa'").unwrap(), b"bobr kurwa");
}

// string_encoding.zig:126 "printf_q 7"
#[test]
fn printf_q_7() {
    assert_eq!(printf_q(b"$'bobr kurwa"), Err(DecodeError));
}

// string_encoding.zig:135 "printf_q 8"
#[test]
fn printf_q_8() {
    assert_eq!(printf_q(b"$'"), Err(DecodeError));
}

// string_encoding.zig:144 "printf_q 9"
#[test]
fn printf_q_9() {
    assert_eq!(printf_q(b"'bobr kurwa"), Err(DecodeError));
}

// string_encoding.zig:153 "printf_q 10"
#[test]
fn printf_q_10() {
    assert_eq!(printf_q(b"'"), Err(DecodeError));
}

// Beyond ghostty's tests: every escape printf_q_decode knows.
#[test]
fn printf_q_escapes() {
    assert_eq!(printf_q(b"\\ \\\\\\\"\\'\\$\\e\\n\\r\\t\\v").unwrap(), b" \\\"'$\x1b\n\r\t\x0b");
}

// string_encoding.zig:202 "singles percent"
#[test]
fn singles_percent() {
    for c in 0..255u8 {
        for s in [std::format!("%{c:02x}"), std::format!("%{c:02X}")] {
            assert_eq!(percent(s.as_bytes()).unwrap(), [c], "{s}");
        }
    }
}

// string_encoding.zig:231 "percent 1"
#[test]
fn percent_1() {
    assert_eq!(percent(b"bobr%20kurwa").unwrap(), b"bobr kurwa");
}

// string_encoding.zig:241 "percent 2"
#[test]
fn percent_2() {
    assert_eq!(percent(b"bobr%2kurwa"), Err(DecodeError));
}

// string_encoding.zig:250 "percent 3"
#[test]
fn percent_3() {
    assert_eq!(percent(b"bobr%kurwa"), Err(DecodeError));
}

// string_encoding.zig:259 "percent 4"
#[test]
fn percent_4() {
    assert_eq!(percent(b"bobr%%kurwa"), Err(DecodeError));
}

// string_encoding.zig:268 "percent 5"
#[test]
fn percent_5() {
    assert_eq!(percent(b"bobr%20kurwa%20").unwrap(), b"bobr kurwa ");
}

// string_encoding.zig:278 "percent 6"
#[test]
fn percent_6() {
    assert_eq!(percent(b"bobr%20kurwa%2"), Err(DecodeError));
}

// string_encoding.zig:287 "percent 7"
#[test]
fn percent_7() {
    assert_eq!(percent(b"bobr%20kurwa%"), Err(DecodeError));
}
