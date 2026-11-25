//! osc/parsers/osc9.zig's tests: OSC 9 notifications and the ConEmu `9;n` subcommands.

use super::*;

fn note(body: &[u8]) -> Option<Command<'_>> {
    Some(Command::ShowDesktopNotification { title: b"", body })
}

fn progress(state: ProgressState, progress: Option<u8>) -> Option<Command<'static>> {
    Some(Command::ConemuProgressReport(ProgressReport { state, progress }))
}

fn xterm(keyboard: Option<bool>, output: Option<bool>) -> Option<Command<'static>> {
    Some(Command::ConemuXtermEmulation { keyboard, output })
}

// osc9.zig:288 "OSC 9: show desktop notification"
#[test]
fn osc_9_show_desktop_notification() {
    assert_eq!(p(b"9;Hello world"), note(b"Hello world"));
}

// osc9.zig:302 "OSC 9: show single character desktop notification"
#[test]
fn osc_9_show_single_character_desktop_notification() {
    assert_eq!(p(b"9;H"), note(b"H"));
}

// osc9.zig:316 "OSC 9;1: ConEmu sleep"
#[test]
fn osc_9_1_conemu_sleep() {
    assert_eq!(p(b"9;1;420"), Some(Command::ConemuSleep { duration_ms: 420 }));
}

// osc9.zig:330 "OSC 9;1: ConEmu sleep with no value default to 100ms"
#[test]
fn osc_9_1_conemu_sleep_with_no_value_default_to_100ms() {
    assert_eq!(p(b"9;1;"), Some(Command::ConemuSleep { duration_ms: 100 }));
}

// osc9.zig:344 "OSC 9;1: conemu sleep cannot exceed 10000ms"
#[test]
fn osc_9_1_conemu_sleep_cannot_exceed_10000ms() {
    assert_eq!(p(b"9;1;12345"), Some(Command::ConemuSleep { duration_ms: 10000 }));
}

// osc9.zig:358 "OSC 9;1: conemu sleep invalid input"
#[test]
fn osc_9_1_conemu_sleep_invalid_input() {
    assert_eq!(p(b"9;1;foo"), Some(Command::ConemuSleep { duration_ms: 100 }));
}

// osc9.zig:372 "OSC 9;1: conemu sleep -> desktop notification 1"
#[test]
fn osc_9_1_conemu_sleep_desktop_notification_1() {
    assert_eq!(p(b"9;1"), note(b"1"));
}

// osc9.zig:386 "OSC 9;1: conemu sleep -> desktop notification 2"
#[test]
fn osc_9_1_conemu_sleep_desktop_notification_2() {
    assert_eq!(p(b"9;1a"), note(b"1a"));
}

// osc9.zig:400 "OSC 9;2: ConEmu message box"
#[test]
fn osc_9_2_conemu_message_box() {
    assert_eq!(p(b"9;2;hello world"), Some(Command::ConemuShowMessageBox(b"hello world")));
}

// osc9.zig:413 "OSC 9;2: ConEmu message box invalid input"
#[test]
fn osc_9_2_conemu_message_box_invalid_input() {
    assert_eq!(p(b"9;2"), note(b"2"));
}

// osc9.zig:426 "OSC 9;2: ConEmu message box empty message"
#[test]
fn osc_9_2_conemu_message_box_empty_message() {
    assert_eq!(p(b"9;2;"), Some(Command::ConemuShowMessageBox(b"")));
}

// osc9.zig:439 "OSC 9;2: ConEmu message box spaces only message"
#[test]
fn osc_9_2_conemu_message_box_spaces_only_message() {
    assert_eq!(p(b"9;2;   "), Some(Command::ConemuShowMessageBox(b"   ")));
}

// osc9.zig:452 "OSC 9;2: message box -> desktop notification 1"
#[test]
fn osc_9_2_message_box_desktop_notification_1() {
    assert_eq!(p(b"9;2"), note(b"2"));
}

// osc9.zig:466 "OSC 9;2: message box -> desktop notification 2"
#[test]
fn osc_9_2_message_box_desktop_notification_2() {
    assert_eq!(p(b"9;2a"), note(b"2a"));
}

// osc9.zig:480 "OSC 9;3: ConEmu change tab title"
#[test]
fn osc_9_3_conemu_change_tab_title() {
    assert_eq!(p(b"9;3;foo bar"), Some(Command::ConemuChangeTabTitle(TabTitle::Value(b"foo bar"))));
}

// osc9.zig:493 "OSC 9;3: ConEmu change tab title reset"
#[test]
fn osc_9_3_conemu_change_tab_title_reset() {
    assert_eq!(p(b"9;3;"), Some(Command::ConemuChangeTabTitle(TabTitle::Reset)));
}

// osc9.zig:507 "OSC 9;3: ConEmu change tab title spaces only"
#[test]
fn osc_9_3_conemu_change_tab_title_spaces_only() {
    assert_eq!(p(b"9;3;   "), Some(Command::ConemuChangeTabTitle(TabTitle::Value(b"   "))));
}

// osc9.zig:521 "OSC 9;3: change tab title -> desktop notification 1"
#[test]
fn osc_9_3_change_tab_title_desktop_notification_1() {
    assert_eq!(p(b"9;3"), note(b"3"));
}

// osc9.zig:535 "OSC 9;3: message box -> desktop notification 2"
#[test]
fn osc_9_3_message_box_desktop_notification_2() {
    assert_eq!(p(b"9;3a"), note(b"3a"));
}

// osc9.zig:549 "OSC 9;4: ConEmu progress set"
#[test]
fn osc_9_4_conemu_progress_set() {
    assert_eq!(p(b"9;4;1;100"), progress(ProgressState::Set, Some(100)));
}

// osc9.zig:563 "OSC 9;4: ConEmu progress set overflow"
#[test]
fn osc_9_4_conemu_progress_set_overflow() {
    assert_eq!(p(b"9;4;1;900"), progress(ProgressState::Set, Some(100)));
}

// osc9.zig:577 "OSC 9;4: ConEmu progress set single digit"
#[test]
fn osc_9_4_conemu_progress_set_single_digit() {
    assert_eq!(p(b"9;4;1;9"), progress(ProgressState::Set, Some(9)));
}

// osc9.zig:591 "OSC 9;4: ConEmu progress set double digit"
#[test]
fn osc_9_4_conemu_progress_set_double_digit() {
    assert_eq!(p(b"9;4;1;94"), progress(ProgressState::Set, Some(94)));
}

// osc9.zig:605 "OSC 9;4: ConEmu progress set extra semicolon ignored"
#[test]
fn osc_9_4_conemu_progress_set_extra_semicolon_ignored() {
    assert_eq!(p(b"9;4;1;100"), progress(ProgressState::Set, Some(100)));
}

// osc9.zig:619 "OSC 9;4: ConEmu progress remove with no progress"
#[test]
fn osc_9_4_conemu_progress_remove_with_no_progress() {
    assert_eq!(p(b"9;4;0;"), progress(ProgressState::Remove, None));
}

// osc9.zig:633 "OSC 9;4: ConEmu progress remove with double semicolon"
#[test]
fn osc_9_4_conemu_progress_remove_with_double_semicolon() {
    assert_eq!(p(b"9;4;0;;"), progress(ProgressState::Remove, None));
}

// osc9.zig:647 "OSC 9;4: ConEmu progress remove ignores progress"
#[test]
fn osc_9_4_conemu_progress_remove_ignores_progress() {
    assert_eq!(p(b"9;4;0;100"), progress(ProgressState::Remove, None));
}

// osc9.zig:661 "OSC 9;4: ConEmu progress remove extra semicolon"
#[test]
fn osc_9_4_conemu_progress_remove_extra_semicolon() {
    assert_eq!(p(b"9;4;0;100;"), progress(ProgressState::Remove, None));
}

// osc9.zig:674 "OSC 9;4: ConEmu progress error"
#[test]
fn osc_9_4_conemu_progress_error() {
    assert_eq!(p(b"9;4;2"), progress(ProgressState::Error, None));
}

// osc9.zig:688 "OSC 9;4: ConEmu progress error with progress"
#[test]
fn osc_9_4_conemu_progress_error_with_progress() {
    assert_eq!(p(b"9;4;2;100"), progress(ProgressState::Error, Some(100)));
}

// osc9.zig:702 "OSC 9;4: progress pause"
#[test]
fn osc_9_4_progress_pause() {
    assert_eq!(p(b"9;4;4"), progress(ProgressState::Pause, None));
}

// osc9.zig:716 "OSC 9;4: ConEmu progress pause with progress"
#[test]
fn osc_9_4_conemu_progress_pause_with_progress() {
    assert_eq!(p(b"9;4;4;100"), progress(ProgressState::Pause, Some(100)));
}

// osc9.zig:730 "OSC 9;4: progress -> desktop notification 1"
#[test]
fn osc_9_4_progress_desktop_notification_1() {
    assert_eq!(p(b"9;4"), note(b"4"));
}

// osc9.zig:744 "OSC 9;4: progress -> desktop notification 2"
#[test]
fn osc_9_4_progress_desktop_notification_2() {
    assert_eq!(p(b"9;4;"), note(b"4;"));
}

// osc9.zig:758 "OSC 9;4: progress -> desktop notification 3"
#[test]
fn osc_9_4_progress_desktop_notification_3() {
    assert_eq!(p(b"9;4;5"), note(b"4;5"));
}

// osc9.zig:772 "OSC 9;4: progress -> desktop notification 4"
#[test]
fn osc_9_4_progress_desktop_notification_4() {
    assert_eq!(p(b"9;4;5a"), note(b"4;5a"));
}

// osc9.zig:786 "OSC 9;5: ConEmu wait input"
#[test]
fn osc_9_5_conemu_wait_input() {
    assert_eq!(p(b"9;5"), Some(Command::ConemuWaitInput));
}

// osc9.zig:798 "OSC 9;5: ConEmu wait ignores trailing characters"
#[test]
fn osc_9_5_conemu_wait_ignores_trailing_characters() {
    assert_eq!(p(b"9;5;foo"), Some(Command::ConemuWaitInput));
}

// osc9.zig:810 "OSC 9;6: ConEmu guimacro 1"
#[test]
fn osc_9_6_conemu_guimacro_1() {
    assert_eq!(p(b"9;6;a"), Some(Command::ConemuGuimacro(b"a")));
}

// osc9.zig:824 "OSC: 9;6: ConEmu guimacro 2"
#[test]
fn osc_9_6_conemu_guimacro_2() {
    assert_eq!(p(b"9;6;ab"), Some(Command::ConemuGuimacro(b"ab")));
}

// osc9.zig:838 "OSC: 9;6: ConEmu guimacro 3 incomplete -> desktop notification"
#[test]
fn osc_9_6_conemu_guimacro_3_incomplete_desktop_notification() {
    assert_eq!(p(b"9;6"), note(b"6"));
}

// osc9.zig:852 "OSC: 9;7: ConEmu run process 1"
#[test]
fn osc_9_7_conemu_run_process_1() {
    assert_eq!(p(b"9;7;ab"), Some(Command::ConemuRunProcess(b"ab")));
}

// osc9.zig:866 "OSC: 9;7: ConEmu run process 2"
#[test]
fn osc_9_7_conemu_run_process_2() {
    assert_eq!(p(b"9;7;"), Some(Command::ConemuRunProcess(b"")));
}

// osc9.zig:880 "OSC: 9;7: ConEmu run process incomplete -> desktop notification"
#[test]
fn osc_9_7_conemu_run_process_incomplete_desktop_notification() {
    assert_eq!(p(b"9;7"), note(b"7"));
}

// osc9.zig:894 "OSC: 9;8: ConEmu output environment variable 1"
#[test]
fn osc_9_8_conemu_output_environment_variable_1() {
    assert_eq!(p(b"9;8;ab"), Some(Command::ConemuOutputEnvironmentVariable(b"ab")));
}

// osc9.zig:908 "OSC: 9;8: ConEmu output environment variable 2"
#[test]
fn osc_9_8_conemu_output_environment_variable_2() {
    assert_eq!(p(b"9;8;"), Some(Command::ConemuOutputEnvironmentVariable(b"")));
}

// osc9.zig:922 "OSC: 9;8: ConEmu output environment variable incomplete -> desktop notification"
#[test]
fn osc_9_8_conemu_output_environment_variable_incomplete_desktop_notification() {
    assert_eq!(p(b"9;8"), note(b"8"));
}

// osc9.zig:936 "OSC: 9;9: ConEmu set current working directory"
#[test]
fn osc_9_9_conemu_set_current_working_directory() {
    assert_eq!(p(b"9;9;ab"), Some(Command::ReportPwd(b"ab")));
}

// osc9.zig:950 "OSC: 9;9: ConEmu set current working directory incomplete -> desktop notification"
#[test]
fn osc_9_9_conemu_set_current_working_directory_incomplete_desktop_notification() {
    assert_eq!(p(b"9;9"), note(b"9"));
}

// osc9.zig:964 "OSC: 9;10: ConEmu xterm keyboard and output emulation 1"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_1() {
    assert_eq!(p(b"9;10"), xterm(Some(true), Some(true)));
}

// osc9.zig:981 "OSC: 9;10: ConEmu xterm keyboard and output emulation 2"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_2() {
    assert_eq!(p(b"9;10;0"), xterm(Some(false), Some(false)));
}

// osc9.zig:998 "OSC: 9;10: ConEmu xterm keyboard and output emulation 3"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_3() {
    assert_eq!(p(b"9;10;1"), xterm(Some(true), Some(true)));
}

// osc9.zig:1015 "OSC: 9;10: ConEmu xterm keyboard and output emulation 4"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_4() {
    assert_eq!(p(b"9;10;2"), xterm(None, Some(false)));
}

// osc9.zig:1031 "OSC: 9;10: ConEmu xterm keyboard and output emulation 5"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_5() {
    assert_eq!(p(b"9;10;3"), xterm(None, Some(true)));
}

// osc9.zig:1047 "OSC: 9;10: ConEmu xterm keyboard and output emulation 6"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_6() {
    assert_eq!(p(b"9;10;4"), note(b"10;4"));
}

// osc9.zig:1061 "OSC: 9;10: ConEmu xterm keyboard and output emulation 7"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_7() {
    assert_eq!(p(b"9;10;"), note(b"10;"));
}

// osc9.zig:1075 "OSC: 9;10: ConEmu xterm keyboard and output emulation 8"
#[test]
fn osc_9_10_conemu_xterm_keyboard_and_output_emulation_8() {
    assert_eq!(p(b"9;10;abc"), note(b"10;abc"));
}

// osc9.zig:1089 "OSC: 9;11: ConEmu comment"
#[test]
fn osc_9_11_conemu_comment() {
    assert_eq!(p(b"9;11;ab"), Some(Command::ConemuComment(b"ab")));
}

// osc9.zig:1103 "OSC: 9;11: ConEmu comment incomplete -> desktop notification"
#[test]
fn osc_9_11_conemu_comment_incomplete_desktop_notification() {
    assert_eq!(p(b"9;11"), note(b"11"));
}

// osc9.zig:1117 "OSC: 9;12: ConEmu mark prompt start 1"
#[test]
fn osc_9_12_conemu_mark_prompt_start_1() {
    let want = SemanticPrompt { action: PromptAction::FreshLineNewPrompt, options: b"" };
    assert_eq!(p(b"9;12"), Some(Command::SemanticPrompt(want)));
}

// osc9.zig:1130 "OSC: 9;12: ConEmu mark prompt start 2"
#[test]
fn osc_9_12_conemu_mark_prompt_start_2() {
    // Trailing bytes are ignored: the mark carries no options.
    let want = SemanticPrompt { action: PromptAction::FreshLineNewPrompt, options: b"" };
    assert_eq!(p(b"9;12;abc"), Some(Command::SemanticPrompt(want)));
}
