//! Tests of c_tasks_probe.rs: compiler probes answered from the pinned table.
//!
//! (a) a cc flag probe is answered from its row, `$OUT_DIR` given back as the script's path;
//! (b) the family probe's folder under `OUT_DIR` and its digits are not in its key (a
//!     placeholder with its content digest is) and come back in its answer;
//! (c) a probe with no row is refused with the exact row, and that row, answered and added to
//!     the table, answers it (the refusal is the table's input);
//! (d) a PENDING row is refused, not answered;
//! (e) control for the key: the probe file's content, a clang-read variable and the clang pin
//!     each change the key (no row); a variable clang does not read does not;
//! (f) two rows with one key are refused (rule 21);
//! (g) only probe-shaped compiler spawns are probes; and the table in the tree parses.

use blaze_own::Alloc;

use crate::c_tasks_probe::{Answered, ProbeWhy, Spawn, Table, TableWhy, is_probe};

/// A clang archive's sha256, and the same as the table spells it.
const CLANG_PIN: [u8; 32] = [0x5d; 32];
const CLANG: &str = "5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d";
const OTHER_CLANG: [u8; 32] = [0; 32];
const OUT: &str = "/target/debug/build/zstd-sys-1/out";
const SDK: &str = "/sdk";
const FLAG_SRC: [u8; 32] = [7; 32];
const FAMILY_SRC: [u8; 32] = [9; 32];
const FLAG_SRC_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const FAMILY_SRC_HEX: &str = "0909090909090909090909090909090909090909090909090909090909090909";

fn flag_args(flag: &str) -> [&str; 14] {
    [
        "-O0",
        "-ffunction-sections",
        "-fdata-sections",
        "-fPIC",
        "--target=arm64-apple-macosx",
        "-mmacosx-version-min=11.0",
        "-Wall",
        "-Wextra",
        flag,
        "-Wno-unused-command-line-argument",
        "-o",
        "/target/debug/build/zstd-sys-1/out/flag_check",
        "-c",
        "/target/debug/build/zstd-sys-1/out/flag_check.c",
    ]
}

fn table_text(rows: &[&str]) -> std::string::String {
    let mut text = std::string::String::from("{\"comment\": \"test\", \"form\": 1, \"made_by\": \"test\", \"sources\": {}, \"rows\": [");
    text.push_str(&rows.join(", "));
    text.push_str("]}");
    text
}

fn flag_row(flag: &str, answer: &str) -> std::string::String {
    format!(
        "{{\"clang\": \"{CLANG}\", \"program\": \"cc\", \"args\": [\"-O0\", \"-ffunction-sections\", \"-fdata-sections\", \"-fPIC\", \"--target=arm64-apple-macosx\", \"-mmacosx-version-min=11.0\", \"-Wall\", \"-Wextra\", \"{flag}\", \"-Wno-unused-command-line-argument\", \"-o\", \"$OUT0\", \"-c\", \"$IN0.c\"], \"env\": {{\"MACOSX_DEPLOYMENT_TARGET\": \"11.0\"}}, \"files\": {{\"$IN0.c\": \"{FLAG_SRC_HEX}\"}}, \"answer\": {answer}}}"
    )
}

fn files(name: &str) -> Option<[u8; 32]> {
    match name {
        "flag_check.c" => Some(FLAG_SRC),
        "123456789detect_compiler_family.c" | "out-apple_core_services_check/563577319973579455detect_compiler_family.c" => Some(FAMILY_SRC),
        _ => None,
    }
}

const ENV: &[(&str, &str)] = &[("CARGO_PKG_NAME", "zstd-sys"), ("MACOSX_DEPLOYMENT_TARGET", "11.0")];

fn spawn<'s>(program: &'s str, args: &'s [&'s str], env: &'s [(&'s str, &'s str)]) -> Spawn<'s> {
    Spawn { program, args, env, out_dir: OUT, sdk: SDK, stdin: &[] }
}

fn parse<'a>(text: &str, alloc: Alloc<'a>) -> Table<'a> {
    match Table::parse(text.as_bytes(), alloc) {
        Ok(table) => table,
        Err(e) => panic!("the test table is refused: {e}"),
    }
}

#[test]
fn a_flag_probe_is_answered_from_its_row() {
    let alloc = blaze_own::testing::heap().alloc();
    let row = flag_row("-mno-avx", "{\"code\": 1, \"stdout\": \"\", \"stderr\": \"clang: error: unsupported option '-mno-avx' for target 'arm64-apple-macosx' in $OUT_DIR\\n\"}");
    let table = parse(&table_text(&[&row]), alloc);
    let args = flag_args("-mno-avx");
    let Ok(Answered { code, stdout, stderr }) = table.answer(&CLANG_PIN, &spawn("cc", &args, ENV), files, alloc) else {
        panic!("the row answers the probe");
    };
    assert_eq!(code, 1);
    assert_eq!(stdout.as_str(), "");
    assert_eq!(stderr.as_str(), "clang: error: unsupported option '-mno-avx' for target 'arm64-apple-macosx' in /target/debug/build/zstd-sys-1/out\n");
}

#[test]
fn the_family_probe_keeps_its_folder_and_name_out_of_the_key() {
    let alloc = blaze_own::testing::heap().alloc();
    let row = format!(
        "{{\"clang\": \"{CLANG}\", \"program\": \"cc\", \"args\": [\"-E\", \"$IN0.c\"], \"env\": {{\"MACOSX_DEPLOYMENT_TARGET\": \"11.0\"}}, \"files\": {{\"$IN0.c\": \"{FAMILY_SRC_HEX}\"}}, \"answer\": {{\"code\": 0, \"stdout\": \"# 1 \\\"$IN0.c\\\"\\n#pragma message \\\"clang\\\"\\n\", \"stderr\": \"\"}}}}"
    );
    let table = parse(&table_text(&[&row]), alloc);
    // Right under OUT_DIR (zstd-sys), and in a build's own out folder below it with other
    // digits (aws-lc-sys's `out-apple_core_services_check/`): one row answers both.
    for name in ["123456789detect_compiler_family.c", "out-apple_core_services_check/563577319973579455detect_compiler_family.c"] {
        let path = format!("{OUT}/{name}");
        let args = ["-E", path.as_str()];
        let Ok(answered) = table.answer(&CLANG_PIN, &spawn("/usr/bin/cc", &args, ENV), files, alloc) else {
            panic!("the row answers the probe of {name}, whatever its folder and digits");
        };
        assert_eq!(answered.stdout.as_str(), format!("# 1 \"{path}\"\n#pragma message \"clang\"\n"));
    }
    // Control: the file not there (the old fault: `files` empty) is another question, an
    // output, and no row answers it.
    let gone = format!("{OUT}/out-x/1detect_compiler_family.c");
    let args = ["-E", gone.as_str()];
    let Err(refused) = table.answer(&CLANG_PIN, &spawn("cc", &args, ENV), files, alloc) else { panic!("a probe of no file is answered") };
    assert_eq!(refused.why, ProbeWhy::NoRow);
    assert!(refused.row.as_str().contains("\"args\": [\"-E\", \"$OUT0.c\"]") && refused.row.as_str().contains("\"files\": {}"), "{}", refused.row.as_str());
}

#[test]
fn a_refusal_prints_the_row_that_answers_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let empty = parse(&table_text(&[]), alloc);
    let args = flag_args("-fno-builtin");
    let probe = spawn("cc", &args, ENV);
    let Err(refused) = empty.answer(&CLANG_PIN, &probe, files, alloc) else { panic!("an empty table answers nothing") };
    assert_eq!(refused.why, ProbeWhy::NoRow);
    assert_eq!(refused.row.as_str(), flag_row("-fno-builtin", "\"PENDING\""));
    // The row as printed, PENDING: refused as pending, not as missing.
    let pending = parse(&table_text(&[refused.row.as_str()]), alloc);
    let Err(again) = pending.answer(&CLANG_PIN, &probe, files, alloc) else { panic!("a PENDING row answers nothing") };
    assert_eq!(again.why, ProbeWhy::Pending);
    // The row as printed, answered as clang_answers.rs writes it: the probe is answered.
    let answered = refused.row.as_str().replace("\"PENDING\"", "{\"code\": 0, \"stdout\": \"\", \"stderr\": \"\"}");
    let full = parse(&table_text(&[&answered]), alloc);
    let Ok(answer) = full.answer(&CLANG_PIN, &probe, files, alloc) else { panic!("the printed row, answered, answers the probe") };
    assert_eq!((answer.code, answer.stderr.as_str()), (0, ""));
}

#[test]
fn control_every_input_is_in_the_key() {
    let alloc = blaze_own::testing::heap().alloc();
    let row = flag_row("-fno-builtin", "{\"code\": 0, \"stdout\": \"\", \"stderr\": \"\"}");
    let table = parse(&table_text(&[&row]), alloc);
    let args = flag_args("-fno-builtin");
    assert!(table.answer(&CLANG_PIN, &spawn("cc", &args, ENV), files, alloc).is_ok(), "the row's own probe");
    // A variable clang does not read is not in the key.
    let more: &[(&str, &str)] = &[("CARGO_PKG_NAME", "other"), ("MACOSX_DEPLOYMENT_TARGET", "11.0"), ("PATH", "/bin")];
    assert!(table.answer(&CLANG_PIN, &spawn("cc", &args, more), files, alloc).is_ok(), "PATH and CARGO_* are not clang's");
    let refused = |answer: Result<Answered<'_>, crate::c_tasks_probe::ProbeRefused<'_>>| matches!(answer, Err(refused) if refused.why == ProbeWhy::NoRow);
    let other_source = |name: &str| (name == "flag_check.c").then_some([8; 32]);
    assert!(refused(table.answer(&CLANG_PIN, &spawn("cc", &args, ENV), other_source, alloc)), "the probe file's content");
    let newer: &[(&str, &str)] = &[("MACOSX_DEPLOYMENT_TARGET", "14.0")];
    assert!(refused(table.answer(&CLANG_PIN, &spawn("cc", &args, newer), files, alloc)), "a variable clang reads");
    assert!(refused(table.answer(&OTHER_CLANG, &spawn("cc", &args, ENV), files, alloc)), "the clang pin");
    assert!(refused(table.answer(&CLANG_PIN, &spawn("c++", &args, ENV), files, alloc)), "the program");
    let other = flag_args("-fno-common");
    assert!(refused(table.answer(&CLANG_PIN, &spawn("cc", &other, ENV), files, alloc)), "the flag");
    // Standard input is in no row: refused, never answered as if empty.
    let fed = Spawn { stdin: b"int x;", ..spawn("cc", &args, ENV) };
    assert!(matches!(table.answer(&CLANG_PIN, &fed, files, alloc), Err(refused) if refused.why == ProbeWhy::Stdin));
}

#[test]
fn two_rows_with_one_key_are_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let a = flag_row("-fno-builtin", "{\"code\": 0, \"stdout\": \"\", \"stderr\": \"\"}");
    let b = flag_row("-fno-builtin", "{\"code\": 1, \"stdout\": \"\", \"stderr\": \"x\"}");
    let Err(e) = Table::parse(table_text(&[&a, &b]).as_bytes(), alloc) else { panic!("two answers to one probe parse") };
    assert!(matches!(e.why, TableWhy::TwoRows { row: 1 }), "{e}");
    // Control: two rows of different flags are one table.
    let c = flag_row("-fno-common", "\"PENDING\"");
    assert!(Table::parse(table_text(&[&a, &c]).as_bytes(), alloc).is_ok());
}

#[test]
fn only_probe_shaped_compiler_spawns_are_probes() {
    assert!(is_probe("cc", &flag_args("-fno-builtin")));
    assert!(is_probe("/usr/bin/c++", &["-E", "/out/1detect_compiler_family.c"]));
    assert!(is_probe("cc", &["-?"]));
    assert!(!is_probe("cc", &["-c", "/pkg/src/a.c", "-o", "/out/a.o"]), "an object compile is a C request");
    assert!(!is_probe("ar", &["cq", "/out/libz.a", "/out/a.o"]));
    assert!(!is_probe("cc", &["--version"]), "a fact (facts.rs), not a probe");
}

#[test]
fn the_pinned_table_parses() {
    let alloc = blaze_own::testing::heap().alloc();
    let text = include_str!("../../../pins/answers/clang.json");
    let table = parse(text, alloc);
    assert!(!table.rows().is_empty());
}

/// The table in the tree answers aws-lc-sys 0.47.0's family probe in its build folder
/// `out-apple_core_services_check/` (main.rs `probe_apple_core_services`), the probe that stopped
/// tempo's check, by the content of cc 1.6.0's detect file (the table's own `sources` text).
#[test]
fn the_pinned_table_answers_a_family_probe_in_a_build_folder() {
    let alloc = blaze_own::testing::heap().alloc();
    let table = parse(include_str!("../../../pins/answers/clang.json"), alloc);
    let pin = crate::toolchain::clang_pin(blaze_pins::CLANG_JSON, "aarch64-apple-darwin").unwrap_or_else(|e| panic!("{e}"));
    let unhex = |text: &str| -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).expect("hex");
        }
        out
    };
    let detect = unhex("97ca4b021495611e828becea6187add37414186a16dfedd26c2947cbce6e8b2f");
    let path = format!("{OUT}/out-apple_core_services_check/563577319973579455detect_compiler_family.c");
    let args = ["-E", path.as_str()];
    let held = |rel: &str| (rel == "out-apple_core_services_check/563577319973579455detect_compiler_family.c").then_some(detect);
    let answered = match table.answer(&pin.sha256, &spawn("cc", &args, ENV), held, alloc) {
        Ok(answered) => answered,
        Err(refused) => panic!("{refused}"),
    };
    assert_eq!(answered.code, 0);
    assert!(answered.stdout.as_str().contains("\"clang\"") && answered.stderr.as_str().contains(path.as_str()), "{answered:?}");
}
