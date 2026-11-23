//! THE BODIES PHASE KEYS ONLY WHAT IT OWES (rule 67; engine_bodies.rs `groups`): a phase that
//! owes bodies in F files outlines and keys by chain only those F files' entries
//! (`index_by_chain`, one call a file), and the profile says it beside the plan's line:
//! "bodies: members opened M of T, files listed F of T, entries keyed E (floor: ...)". A file
//! is owed when an entered `BodyCheck` task's body is in it; `Holders::decide` asks no claim of
//! another file. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! Each member is a library with no test harness (`test = false`), so each member is one unit
//! and the file counts are files.

use super::*;
use super::engine_check_tests::{error_lines, profile, said};

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_LIB: &str = "pub mod m;\n\npub fn f() -> u8 {\n    1\n}\n";
const A_M: &str = "pub fn k() -> u8 {\n    2\n}\n";
/// `k`'s body edited to a `bool` where its signature says `u8`: the Reference, "Function
/// items": a body's type is its return type's, so a type error (E0308).
const A_M_WRONG: &str = "pub fn k() -> u8 {\n    true\n}\n";
const B_LIB: &str = "pub fn h() -> u8 {\n    3\n}\n";

/// Two members: `a` of two files (`src/lib.rs`, `src/m.rs`), `b` of one.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", WORKSPACE);
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\ntest = false\n");
    write(io, "a/src/lib.rs", A_LIB);
    write(io, "a/src/m.rs", A_M);
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\ntest = false\n");
    write(io, "b/src/lib.rs", B_LIB);
}

/// The Bodies profile's owed-listing counters of the last request.
#[derive(Debug, PartialEq, Eq)]
struct Counters {
    members_opened: u32,
    members: u32,
    files_listed: u32,
    files: u32,
    entries_keyed: u64,
    members_owed: u32,
    files_owed: u32,
    /// Member listing rows read by key, made from a whole listing, found damaged
    /// (engine_member_rows.rs), one a unit.
    rows_read: u32,
    rows_made: u32,
    rows_damaged: u32,
}

/// The request's lines that are the uncovered note (engine_bodies.rs `Uncovered`), in order.
fn uncovered_note(lines: &[std::string::String]) -> std::vec::Vec<std::string::String> {
    lines.iter().filter(|line| line.contains("{bodies a macro wrote that no entry covers}")).cloned().collect()
}

fn counters(io: &Io) -> Counters {
    let profile = profile(io);
    let line = profile
        .lines()
        .find_map(|line| line.strip_prefix("bodies: members opened "))
        .unwrap_or_else(|| panic!("the Bodies profile says what it opened, listed and keyed:\n{profile}"));
    let words: std::vec::Vec<&str> = line.split_whitespace().map(|w| w.trim_end_matches([',', ';'])).collect();
    let n = |at: usize| -> u64 { words.get(at).and_then(|w| w.parse().ok()).unwrap_or_else(|| panic!("word {at} of the counters line is a count: {line}")) };
    let small = |at: usize| u32::try_from(n(at)).unwrap_or_else(|_| panic!("word {at} of the counters line fits a u32: {line}"));
    Counters { members_opened: small(0), members: small(2), files_listed: small(5), files: small(7), entries_keyed: n(10), members_owed: small(18), files_owed: small(20), rows_read: small(28), rows_made: small(30), rows_damaged: small(32) }
}

/// A fresh check owes every body, so it lists every member and every file (CONTROL: the
/// counters count what is there). An edit of ONE body in `a/src/m.rs` owes that file alone: 1 of
/// 3 files listed and its one entry keyed, the floor 1 member and 1 file. The type error the
/// edit plants IS printed (the check ran on the owed body); the edit taken back prints the
/// fresh answer's lines again, byte for byte. Only the owed file's member is opened: every other
/// member's listing row is read by key (engine_member_rows.rs), and the edited member's row is
/// made again because its file's text is in the key.
#[test]
fn a_one_body_edit_lists_only_its_file() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    workspace(&io);

    let (fresh_ids, fresh_lines, _) = said(&io, true);
    let fresh = counters(&io);
    std::println!("fresh: {fresh:?}");
    // The counters count UNITS and (unit, file) pairs, and a member package is held by more than
    // one unit (observed 2026-11-19: two units a member here, so 4 "members" and 6 pairs for 2
    // packages and 3 files, on the fresh run and on the edit alike). Read them per unit.
    let units = fresh.members / 2;
    assert!(units >= 1 && fresh.members == 2 * units && fresh.files == 3 * units, "each unit set counts the 2 packages and their 3 files: {fresh:?}");
    assert_eq!((fresh.members_owed, fresh.members_opened), (fresh.members, fresh.members), "a fresh check owes and opens both members: {fresh:?}");
    assert_eq!((fresh.files_owed, fresh.files_listed), (fresh.files, fresh.files), "a fresh check owes and lists every file: {fresh:?}");
    assert_eq!(fresh.entries_keyed, u64::from(fresh.files), "each of `f`, `k` and `h` is keyed by its chain once a phase: {fresh:?}");
    assert!(error_lines(&fresh_lines.join("\n")).is_empty(), "the fixture is well typed:\n{}", fresh_lines.join("\n"));
    // CONTROL: a fresh home stores no row, so each unit's row is made from its whole listing.
    assert_eq!((fresh.rows_read, fresh.rows_made, fresh.rows_damaged), (0, fresh.members, 0), "a fresh check makes every member's listing row: {fresh:?}");

    write(&io, "a/src/m.rs", A_M_WRONG);
    let (_, wrong_lines, _) = said(&io, false);
    let edited = counters(&io);
    std::println!("one body edited: {edited:?}");
    assert_eq!(error_lines(&wrong_lines.join("\n")).len(), 1, "the edit's type error is reported, so `k` was checked:\n{}", wrong_lines.join("\n"));
    assert_eq!((edited.members, edited.files), (fresh.members, fresh.files), "the same units and (unit, file) pairs as the fresh run: {edited:?}");
    assert_eq!(edited.files_listed, units, "one body edited in one file: that ONE file listed, once a unit that holds it, of {} pairs: {edited:?}", edited.files);
    assert_eq!((edited.files_owed, edited.members_owed), (units, units), "the floor: the owed entry's one file and one package, once a unit: {edited:?}");
    assert_eq!(edited.entries_keyed, u64::from(units), "only `k`'s entry is keyed by its chain, once a unit: {edited:?}");
    // The floor (D149 I1): only the owed file's member is opened, once a unit; `b` is read from
    // its listing row (engine_bodies.rs `groups`).
    assert_eq!(edited.members_opened, edited.members_owed, "members opened is the floor, the owed file's member once a unit: {edited:?}");
    // Standing members (`StandingMembers` on `Driver::Phases`, #2269): unedited member `b` stands
    // directly from `members_record` without probing its member listing row from the store, so
    // only the owed member `a`'s row is read by key (floor: `edited.members_owed`).
    assert_eq!((edited.rows_read, edited.rows_made, edited.rows_damaged), (edited.members_owed, 0, 0), "a body edit moves no file's head: unedited members stand from members_record, owed members read by key: {edited:?}");
    assert_eq!(uncovered_note(&wrong_lines), uncovered_note(&fresh_lines), "the uncovered note is the same bytes whether a member's part was read or made");

    write(&io, "a/src/m.rs", A_M);
    let (back_ids, back_lines, _) = said(&io, false);
    assert_eq!((back_ids, back_lines.join("\n").into_bytes()), (fresh_ids, fresh_lines.join("\n").into_bytes()), "the edit taken back prints the fresh answer's lines again");
    let back = counters(&io);
    assert_eq!((back.files_listed, back.files_owed), (units, units), "taking the edit back owes the one file again: {back:?}");
    assert_eq!(back.members_opened, back.members_owed, "taking the edit back opens only the owed file's member: {back:?}");
    // Taking the edit back stands `b` from `members_record` and reads `a`'s fresh row by key.
    assert_eq!((back.rows_read, back.rows_made, back.rows_damaged), (back.members_owed, 0, 0), "taking the edit back reads only the owed member's row, the fresh run's: {back:?}");
    assert_eq!(uncovered_note(&back_lines), uncovered_note(&fresh_lines), "the uncovered note after edit and revert is the fresh one, byte for byte");
}
