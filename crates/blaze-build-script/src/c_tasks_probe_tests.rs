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

use crate::c_tasks_probe::{
    Answered, CanonicalProbe, MOUNT, ProbeAnswer, ProbeAnswerer, ProbeCache, ProbeKey, ProbeWhy, Spawn, Table, TableWhy,
    canonical_reply_digest, canonicalize, decode_answer, encode_answer, is_probe, resolve,
};

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
    Spawn { program, args, env, out_dir: OUT, sdk: SDK, mount: "", sdk_id: "", stdin: &[] }
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
    assert!(is_probe("c++", &["-E", "/out/1detect_compiler_family.c"]));
    assert!(!is_probe("/usr/bin/c++", &["-E", "/out/1detect_compiler_family.c"]), "path-qualified compiler must stay on normal route");
    assert!(!is_probe("/opt/gcc/bin/gcc", &["-E", "/out/1detect_compiler_family.c"]), "explicit compiler path must not route to pinned clang");
    assert!(!is_probe("/missing/cc", &flag_args("-fno-builtin")), "missing path-qualified compiler must not route to pinned clang");
    assert!(is_probe("cc", &["-?"]));
    assert!(!is_probe("cc", &["-?", "/package/check.c"]), "-? with extra args must stay on normal compiler route");
    assert!(!is_probe("cc", &["-c", "-?", "/out/flag_check.c"]), "-? combined with other flags is not a bare -? probe");
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

#[test]
fn the_pinned_table_answers_a_linux_arm64_family_probe() {
    let alloc = blaze_own::testing::heap().alloc();
    let table = parse(include_str!("../../../pins/answers/clang.json"), alloc);
    let pin = crate::toolchain::clang_pin(blaze_pins::CLANG_JSON, "aarch64-unknown-linux-gnu").unwrap_or_else(|e| panic!("{e}"));
    let unhex = |text: &str| -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).expect("hex");
        }
        out
    };
    let detect = unhex("97ca4b021495611e828becea6187add37414186a16dfedd26c2947cbce6e8b2f");
    let path = format!("{OUT}/123456789detect_compiler_family.c");
    let args = ["-E", path.as_str()];
    let held = |rel: &str| (rel == "123456789detect_compiler_family.c").then_some(detect);
    let probe = Spawn { program: "cc", args: &args, env: &[], out_dir: OUT, sdk: "", mount: "/package", sdk_id: "", stdin: &[] };
    let answered = match table.answer(&pin.sha256, &probe, held, alloc) {
        Ok(answered) => answered,
        Err(refused) => panic!("{refused}"),
    };
    assert_eq!(answered.code, 0);
    assert!(answered.stdout.as_str().contains("\"clang\""), "{answered:?}");
}

#[test]
fn mount_normalization_shares_probe_key_across_package_and_vendored_workspace() {
    let alloc = blaze_own::testing::heap().alloc();
    let id = blaze_ids::ContentId([7; 32]);
    let pkg_args = flag_args("-ffile-prefix-map=/package=");
    let ws_args = flag_args("-ffile-prefix-map=/workspace/vendored/aws-lc-sys=");
    let pkg_spawn = Spawn { program: "cc", args: &pkg_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let ws_spawn = Spawn { program: "cc", args: &ws_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/workspace/vendored/aws-lc-sys", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let a = canonicalize(id, &CLANG_PIN, &pkg_spawn, files, alloc).expect("pkg canonical");
    let b = canonicalize(id, &CLANG_PIN, &ws_spawn, files, alloc).expect("ws canonical");
    assert_eq!(a.key, b.key, "workspace vendored mount must share probe key with /package");
    assert!(a.row.args.iter().any(|arg| arg.as_str() == format!("-ffile-prefix-map={MOUNT}=")));
    // Control: different sdk_id produces a different ProbeKey.
    let other_sdk = Spawn { sdk_id: "macos-sdk 15.1", ..pkg_spawn };
    let c = canonicalize(id, &CLANG_PIN, &other_sdk, files, alloc).expect("other sdk");
    assert_ne!(a.key, c.key);
}

#[test]
fn header_including_preprocess_is_not_classified_as_probe() {
    assert!(is_probe("cc", &["-E", "/out/123detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "-I/package/include", "/package/src/foo.c"]));
    assert!(!is_probe("cc", &["-E", "-isystem", "/sdk/usr/include", "/package/src/foo.c"]));
    assert!(!is_probe("cc", &["-E", "-include", "config.h", "/package/src/foo.c"]));
}

#[test]
fn binary_probe_answer_round_trips_and_resolves_once_via_cache() {
    use core::cell::RefCell;
    use core::sync::atomic::{AtomicUsize, Ordering};
    let alloc = blaze_own::testing::heap().alloc();
    let ans = ProbeAnswer {
        code: 1,
        stdout: blaze_own::String::from_str_in("", alloc),
        stderr: blaze_own::String::from_str_in("clang: warning in $IN0.c\n", alloc),
    };
    let mut bytes = blaze_own::Vec::new_in(alloc);
    encode_answer(&ans, &mut bytes);
    let decoded = decode_answer(bytes.as_slice(), alloc).expect("decode");
    assert_eq!(decoded, ans);
    assert_eq!(canonical_reply_digest(&decoded), canonical_reply_digest(&ans));

    struct CountingAnswerer(AtomicUsize);
    impl ProbeAnswerer for CountingAnswerer {
        fn id<'a>(&self, _alloc: Alloc<'a>) -> Result<blaze_ids::ContentId, blaze_own::AString<'a>> {
            Ok(blaze_ids::ContentId::of("test c-probe answerer", b"v1"))
        }
        fn answer<'a, 'b>(
            &self,
            _probe: &CanonicalProbe<'_, '_>,
            _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
            alloc: Alloc<'a>,
        ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ProbeAnswer {
                code: 0,
                stdout: blaze_own::String::from_str_in("ok $IN0.c\n", alloc),
                stderr: blaze_own::String::new_in(alloc),
            })
        }
    }
    struct MemCache(RefCell<std::vec::Vec<(ProbeKey, std::vec::Vec<u8>)>>);
    impl ProbeCache for MemCache {
        fn get<'a>(&self, key: &ProbeKey, alloc: Alloc<'a>) -> Result<Option<blaze_own::AVec<'a, u8>>, blaze_own::AString<'a>> {
            let guard = self.0.borrow();
            Ok(guard.iter().find(|(k, _)| k == key).map(|(_, b)| {
                let mut v = blaze_own::Vec::with_capacity_in(b.len(), alloc);
                v.extend_from_slice(b);
                v
            }))
        }
        fn put<'a>(&self, key: &ProbeKey, bytes: &[u8], _alloc: Alloc<'a>) -> Result<(), blaze_own::AString<'a>> {
            self.0.borrow_mut().push((*key, bytes.to_vec()));
            Ok(())
        }
    }

    let answerer = CountingAnswerer(AtomicUsize::new(0));
    let cache = MemCache(RefCell::new(std::vec::Vec::new()));
    let args = flag_args("-Wshadow");
    let sp = Spawn { program: "cc", args: &args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let mut no_bytes = |_: &str| -> Option<&[u8]> { None };
    let first = resolve(&answerer, &cache, &CLANG_PIN, &sp, files, &mut no_bytes, alloc).expect("first");
    let second = resolve(&answerer, &cache, &CLANG_PIN, &sp, files, &mut no_bytes, alloc).expect("second");
    assert_eq!(answerer.0.load(Ordering::SeqCst), 1, "second probe must hit cache");
    assert_eq!(first.key, second.key);
    assert_eq!(first.canonical_digest, second.canonical_digest);
    assert_eq!(first.answered.stdout.as_str(), "ok /target/debug/build/zstd-sys-1/out/flag_check.c\n");
}

#[test]
fn header_search_and_package_paths_are_not_deferred_or_cached_unkeyed() {
    let alloc = blaze_own::testing::heap().alloc();
    // Real preprocessing jobs (`cc -E -I...` AND `cc -E /package/src.c`) are NOT zero-header
    // compiler-family probes and stay on the deferred C-tool route during `CMode::Deferred`.
    let pp_args = ["-E", "-I", "/package/include", "/package/src/check.c"];
    assert!(!is_probe("cc", &pp_args));
    assert!(crate::probe::is_deferred_tool("cc", &pp_args));
    let pp_no_inc = ["-E", "/package/src/check.c"];
    assert!(!is_probe("cc", &pp_no_inc));
    assert!(crate::probe::is_deferred_tool("cc", &pp_no_inc));
    let fam_args = ["-E", "/target/debug/build/zstd-sys-1/out/1detect_compiler_family.c"];
    assert!(is_probe("cc", &fam_args));
    assert!(!crate::probe::is_deferred_tool("cc", &fam_args));

    // `flag_check` referencing package headers or non-prefix-map `$MOUNT` paths is refused cleanly.
    let files = |name: &str| (name == "flag_check.c").then_some(FLAG_SRC);
    let bad_inc = ["-c", "-I/package/include", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
    let sp_inc = Spawn { program: "cc", args: &bad_inc, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
    let err = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_inc, files, alloc).err().expect("must refuse -I in flag_check");
    assert_eq!(err.why, ProbeWhy::PackageInput);

    // Non-prefix-map reference to `$MOUNT` is refused, while `-ffile-prefix-map=<mount>=` is allowed.
    let bad_mount = ["-c", "/workspace/vendored/foo/header.h", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
    let sp_mount = Spawn { program: "cc", args: &bad_mount, env: ENV, out_dir: OUT, sdk: SDK, mount: "/workspace/vendored/foo", sdk_id: "", stdin: &[] };
    let err2 = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_mount, files, alloc).err().expect("must refuse mount path in flag_check");
    assert_eq!(err2.why, ProbeWhy::PackageInput);

    // Relative source path (`probe.c`) and include-steering env (`CPATH`, `C_INCLUDE_PATH`) are refused.
    let rel_args = ["-c", "probe.c", "-o", "/target/debug/build/zstd-sys-1/out/flag_check"];
    let sp_rel = Spawn { program: "cc", args: &rel_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
    assert_eq!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_rel, files, alloc).err().unwrap().why, ProbeWhy::PackageInput);

    let ok_args = flag_args("-Wshadow");
    let bad_env = [("CPATH", "/package/include")];
    let sp_env = Spawn { program: "cc", args: &ok_args, env: &bad_env, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
    assert_eq!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_env, files, alloc).err().unwrap().why, ProbeWhy::PackageInput);

    // OUT_DIR source containing `#include` is refused in `resolve` before caching or running.
    struct PanicAnswerer;
    impl ProbeAnswerer for PanicAnswerer {
        fn id<'a>(&self, _alloc: Alloc<'a>) -> Result<blaze_ids::ContentId, blaze_own::AString<'a>> {
            Ok(blaze_ids::ContentId([9; 32]))
        }
        fn answer<'a, 'b>(&self, _: &CanonicalProbe<'_, '_>, _: &mut dyn FnMut(&str) -> Option<&'b [u8]>, _: Alloc<'a>) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
            panic!("must not invoke answerer for #include probe");
        }
    }
    struct NoCache;
    impl ProbeCache for NoCache {
        fn get<'a>(&self, _: &ProbeKey, _: Alloc<'a>) -> Result<Option<blaze_own::AVec<'a, u8>>, blaze_own::AString<'a>> { Ok(None) }
        fn put<'a>(&self, _: &ProbeKey, _: &[u8], _: Alloc<'a>) -> Result<(), blaze_own::AString<'a>> { Ok(()) }
    }
    let sp_ok = Spawn { program: "cc", args: &ok_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    for bad_src in [
        b"#include \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"#/**/include \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"#\\\ninclude \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"#inc\\\nlude \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"%:include \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"??=include \"local.h\"\nint main(void) { return 0; }\n".as_slice(),
        b"#if __has_include(\"sibling.h\")\nint x;\n#endif\n".as_slice(),
        b"#if __has_include_next(<foo.h>)\nint x;\n#endif\n".as_slice(),
        b"#if __has_embed(\"data.bin\")\nint x;\n#endif\n".as_slice(),
        b"#embed \"sibling.bin\"\n".as_slice(),
        b"#/**/embed \"sibling.bin\"\n".as_slice(),
        b"#pragma GCC dependency \"sibling.h\"\n".as_slice(),
        b"_Pragma(\"GCC dependency \\\"sibling.h\\\"\")\n".as_slice(),
        b"const char *t = __TIMESTAMP__;\n".as_slice(),
    ] {
        let mut inc_bytes = |name: &str| (name == "flag_check.c").then_some(bad_src);
        assert!(resolve(&PanicAnswerer, &NoCache, &CLANG_PIN, &sp_ok, files, &mut inc_bytes, alloc).is_err());
    }

    // Inert tokens inside quoted string literals must NOT be rejected.
    struct OkAnswerer;
    impl ProbeAnswerer for OkAnswerer {
        fn id<'a>(&self, _alloc: Alloc<'a>) -> Result<blaze_ids::ContentId, blaze_own::AString<'a>> {
            Ok(blaze_ids::ContentId([10; 32]))
        }
        fn answer<'a, 'b>(&self, _: &CanonicalProbe<'_, '_>, _: &mut dyn FnMut(&str) -> Option<&'b [u8]>, alloc: Alloc<'a>) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
            Ok(ProbeAnswer { code: 0, stdout: blaze_own::String::new_in(alloc), stderr: blaze_own::String::new_in(alloc) })
        }
    }
    let inert_src = b"const char *a = \"__has_include\";\nconst char *b = \"__TIMESTAMP__\";\nint main(void) { return 0; }\n";
    let mut inert_bytes = |name: &str| (name == "flag_check.c").then_some(inert_src.as_slice());
    assert!(resolve(&OkAnswerer, &NoCache, &CLANG_PIN, &sp_ok, files, &mut inert_bytes, alloc).is_ok());

    // Host-CPU-probing flags and argv `-D` host-dependent macros are refused before caching.
    for native_args in [
        ["-E", "-dM", "-march=native", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-mcpu=native", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-mtune", "native", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-DTS=__TIMESTAMP__", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-D", "TS=__TIMESTAMP__", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
    ] {
        let sp = Spawn { program: "cc", args: native_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp, files, alloc).err().expect("native flag").why,
            ProbeWhy::HostDependent
        );
    }
    let argv_has_inc = ["-c", "-DH=__has_include(\"a.h\")", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
    let sp_argv_inc = Spawn { program: "cc", args: &argv_has_inc, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
    assert_eq!(
        canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_argv_inc, files, alloc).err().expect("argv __has_include").why,
        ProbeWhy::PackageInput
    );

    // Quoted argv macro definitions (e.g. `-DVERSION="__TIMESTAMP__"`) are inert string literals
    // and must pass canonicalization cleanly.
    let quoted_def_args = flag_args("-DVERSION=\"__TIMESTAMP__\"");
    let sp_quoted_def = Spawn { program: "cc", args: &quoted_def_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_quoted_def, files, alloc).is_ok());

    // Unbound `SDKROOT` (non-empty `SDKROOT` env / `sdk` with empty `sdk_id`) is refused before caching.
    let sdk_env = [("SDKROOT", SDK)];
    let sp_unbound_sdk = Spawn { program: "cc", args: &ok_args, env: &sdk_env, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "", stdin: &[] };
    assert_eq!(
        canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_unbound_sdk, files, alloc).err().expect("unbound SDKROOT").why,
        ProbeWhy::PackageInput
    );

    // Joined and delegated header/preprocessor/module-file flags are refused before caching.
    for bad_flag in [
        "-include/target/debug/build/zstd-sys-1/out/foo.h",
        "-imacros/target/debug/build/zstd-sys-1/out/foo.h",
        "-Wp,-include,/target/debug/build/zstd-sys-1/out/foo.h",
        "-Xpreprocessor",
        "-Xclang",
        "--include=/target/debug/build/zstd-sys-1/out/foo.h",
        "-fmodule-file=m=/target/debug/build/zstd-sys-1/out/m.pcm",
        "-fembed-dir=/target/debug/build/zstd-sys-1/out",
        "-fxray-attr-list=/target/debug/build/zstd-sys-1/out/xray.txt",
        "-working-directory=generated",
        "-isysroot/host/sysroot",
        "-ivfsoverlay/target/debug/build/zstd-sys-1/out/vfs.yaml",
    ] {
        let joined_args = ["-c", bad_flag, "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
        let sp = Spawn { program: "cc", args: &joined_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp, files, alloc).err().expect(bad_flag).why,
            ProbeWhy::PackageInput
        );
    }
    let split_isysroot = ["-c", "-isysroot", "/host/sysroot", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
    let sp_isys = Spawn { program: "cc", args: &split_isysroot, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    assert_eq!(
        canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_isys, files, alloc).err().expect("split -isysroot").why,
        ProbeWhy::PackageInput
    );

    // Literal `-fdebug-compilation-dir=$OUT_DIR` and actual `-fdebug-compilation-dir=<OUT_DIR>`
    // have disjoint structural ProbeKeys and keep the literal string distinct from `Segment::OutDir`
    // (likewise for `-fdebug-prefix-map=<OUT_DIR>=.`).
    let lit_args = flag_args("-fdebug-compilation-dir=$OUT_DIR");
    let real_dir_flag = std::format!("-fdebug-compilation-dir={OUT}");
    let real_args = flag_args(&real_dir_flag);
    let pmap_flag = std::format!("-fdebug-prefix-map={OUT}=.");
    let pmap_args = flag_args(&pmap_flag);
    let sp_lit = Spawn { program: "cc", args: &lit_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let sp_real = Spawn { program: "cc", args: &real_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let sp_pmap = Spawn { program: "cc", args: &pmap_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let can_lit = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_lit, files, alloc).expect("lit");
    let can_real = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_real, files, alloc).expect("real");
    let can_pmap = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_pmap, files, alloc).expect("pmap");
    assert_ne!(can_lit.key, can_real.key, "literal $OUT_DIR must not collide with real OUT_DIR");
    assert!(can_lit.exec_args.iter().flatten().all(|s| !matches!(s, crate::c_tasks_probe::Segment::OutDir)));
    assert!(can_real.exec_args.iter().flatten().any(|s| matches!(s, crate::c_tasks_probe::Segment::OutDir)));
    assert!(can_pmap.exec_args.iter().flatten().any(|s| matches!(s, crate::c_tasks_probe::Segment::OutDir)));

    // Inline assembly (`.incbin`) and preprocessor token pasting (`##` / `%:%:`) are refused
    // before caching; deterministic profile-generation flags are allowed while feedback-reading
    // profile-use flags remain refused.
    for bad_src in [
        &b"void f(void) { __asm__(\".incbin \\\"/package/blob\\\"\"); }"[..],
        &b"#define CAT(a, b) a##b\n#if CAT(__has_,include)(\"sibling.h\")\n#endif\n"[..],
        &b"#define CAT(a, b) a%:%:b\nint main(void) { return 0; }\n"[..],
    ] {
        let mut bad_bytes = |rel: &str| (rel == "flag_check.c").then_some(bad_src);
        assert!(resolve(&PanicAnswerer, &NoCache, &CLANG_PIN, &sp_ok, files, &mut bad_bytes, alloc).is_err());
    }
    for ok_prof in ["-fprofile-instr-generate", "-fprofile-generate", "-fprofile-arcs"] {
        let prof_args = flag_args(ok_prof);
        let sp_prof = Spawn { program: "cc", args: &prof_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_prof, files, alloc).is_ok(), "{ok_prof} should be allowed");
    }
    for bad_prof in ["-fprofile-use=/tmp/p.profdata", "-fprofile-instr-use=default.profdata", "-fprofile-sample-use=a.prof"] {
        let prof_args = flag_args(bad_prof);
        let sp_prof = Spawn { program: "cc", args: &prof_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_prof, files, alloc).is_err(), "{bad_prof} must be refused");
    }

    // Command-line token pasting, `_Pragma`, filename macros, assembler inputs/language, nested
    // response files, and timing-report flags are refused before caching.
    for bad_argv in [
        ["-c", "-DCAT(a,b)=a##b", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-D", "CAT(a,b)=a%:%:b", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-DREAD=_Pragma(\"GCC dependency \\\"sibling.h\\\"\")", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-D", "READ=_Pragma(\"GCC dependency\")", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-DF=__FILE_NAME__", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-x", "assembler", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.S"].as_slice(),
        ["-c", "-Xassembler", "@args.rsp", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
    ] {
        let sp = Spawn { program: "cc", args: bad_argv, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp, files, alloc).err().expect("bad argv").why,
            ProbeWhy::PackageInput
        );
    }
    for timing_flag in ["-ftime-report", "-ftime-report-details", "-ftime-trace", "-fproc-stat-report", "-print-stats"] {
        let timing_args = flag_args(timing_flag);
        let sp = Spawn { program: "cc", args: &timing_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp, files, alloc).err().expect("timing flag").why,
            ProbeWhy::HostDependent
        );
    }
    for clock_argv in [
        ["-c", "-DD=__DATE__", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
        ["-c", "-D", "T=__TIME__", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"].as_slice(),
    ] {
        let sp = Spawn { program: "cc", args: clock_argv, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp, files, alloc).err().expect("clock macro flag").why,
            ProbeWhy::HostDependent
        );
    }
    for bad_src in [
        &b".include \"sibling.inc\"\n"[..],
        &b"nop; .incbin \"blob.bin\"\n"[..],
        &b"const char *f = __FILE__;\n"[..],
        &b"const char *f = __BASE_FILE__;\n"[..],
        &b"const char *f = __FILE_NAME__;\n"[..],
        &b"const char *f = __builtin_FILE();\n"[..],
        &b"const char *f = __builtin_FILE_NAME();\n"[..],
        &b"const void *loc = __builtin_source_location();\n"[..],
        &b"const char *d = __DATE__;\n"[..],
        &b"const char *t = __TIME__;\n"[..],
    ] {
        let mut bad_bytes = |rel: &str| (rel == "flag_check.c").then_some(bad_src);
        assert!(resolve(&PanicAnswerer, &NoCache, &CLANG_PIN, &sp_ok, files, &mut bad_bytes, alloc).is_err());
    }

    // A package named `asm` in OUT_DIR must NOT be falsely rejected by macro-token scanning.
    let asm_out = "/target/debug/build/asm-1234/out";
    let asm_args = ["-c", "-Wall", "-o", "/target/debug/build/asm-1234/out/flag_check", "/target/debug/build/asm-1234/out/flag_check.c"];
    let sp_asm_pkg = Spawn { program: "cc", args: &asm_args, env: ENV, out_dir: asm_out, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_asm_pkg, files, alloc).is_ok(), "package named `asm` must be allowed");

    // Only the exact cc-rs flag-check and family-detection probe shapes are routed as probes;
    // duplicate `-o`, other sources, or `-E ... -o <file>` keep their normal compiler route.
    assert!(is_probe("cc", &asm_args));
    assert!(!is_probe("cc", &["-c", "-o", "/out/flag_check", "-o", "/out/other", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-o", "/out/flag_check", "/out/real_code.c"]));
    assert!(!is_probe("cc", &["-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-o", "/out/flag_check", "/out/flag_check.c", "/out/extra.c"]));
    assert!(is_probe("cc", &["-E", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "/out/xdetect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "/out/12345detect_compiler_family.c", "-o", "/out/generated.i"]));
    assert!(!is_probe("cc", &["-E", "-MD", "-MF", "/out/deps.d", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "-MMD", "-MF/out/deps.d", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "-M", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "-MJ", "/out/cdb.json", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "-save-temps", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-E", "--serialize-diagnostics", "/out/diag.dia", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-c", "-MD", "-MF", "/out/deps.d", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-save-temps=obj", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "--coverage", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-Wp,-MD,/out/deps.d", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-E", "-Wp,-MD,/out/deps.d", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-c", "-Xpreprocessor", "-MD", "-Xpreprocessor", "-MF", "-Xpreprocessor", "/out/deps.d", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-E", "-Xpreprocessor", "-MD", "/out/12345detect_compiler_family.c"]));
    assert!(!is_probe("cc", &["-c", "-Xclang", "-dependency-file", "-Xclang", "/out/deps.d", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-g", "-gsplit-dwarf", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-g", "-gsplit-dwarf=split", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-fno-integrated-as", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-no-integrated-as", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-fno-integrated-cc1", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-no-integrated-cc1", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-save-stats", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-save-stats=obj", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-gen-reproducer=always", "-o", "/out/flag_check", "/out/flag_check.c"]));
    assert!(!is_probe("cc", &["-c", "-mllvm", "-stats-file=/out/s", "-o", "/out/flag_check", "/out/flag_check.c"]));

    // Output-metadata compilation-directory flags (`-fdebug-compilation-dir=.`, etc.) do not read
    // host directories and must be accepted by flag-support probes.
    for meta_flag in ["-fdebug-compilation-dir=.", "-fcoverage-compilation-dir=.", "-ffile-compilation-dir=."] {
        let m_args = flag_args(meta_flag);
        let sp_m = Spawn { program: "cc", args: &m_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_m, files, alloc).is_ok(), "{meta_flag} must be allowed");
    }

    // Ambient toolchain discovery flags (`-v`, `-###`, `-print-search-dirs`, `-print-file-name=...`)
    // and Clang/glibc control environment variables (`CC_PRINT_OPTIONS`, `GLIBC_TUNABLES`) are
    // refused as HostDependent before caching.
    for disc in ["-v", "-###", "--verbose", "-print-search-dirs", "-print-file-name=libc.a"] {
        let d_args = flag_args(disc);
        let sp_d = Spawn { program: "cc", args: &d_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_d, files, alloc).err().expect("discovery flag").why,
            ProbeWhy::HostDependent
        );
    }
    for (env_k, env_v) in [
        ("CC_PRINT_OPTIONS", "1"),
        ("CC_PRINT_OPTIONS_FILE", "/out/opts.log"),
        ("CC_LOG_DIAGNOSTICS", "1"),
        ("GLIBC_TUNABLES", "glibc.cpu.hwcaps=-x86-64-v3"),
    ] {
        let bad_env = [("SDKROOT", SDK), (env_k, env_v)];
        let sp_e = Spawn { program: "cc", args: &ok_args, env: &bad_env, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        assert_eq!(
            canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_e, files, alloc).err().expect("control env").why,
            ProbeWhy::HostDependent
        );
    }

    // Separate `-D 'ROOT="/literal"'` preserves path literals just like attached `-DROOT="/literal"`.
    let sep_def = ["-c", "-D", "ROOT=\"/literal\"", "-o", "/target/debug/build/zstd-sys-1/out/flag_check", "/target/debug/build/zstd-sys-1/out/flag_check.c"];
    let sp_sep = Spawn { program: "cc", args: &sep_def, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    assert!(canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_sep, files, alloc).is_ok(), "separate -D path literal must be allowed");

    // `OUT_DIR` paths with `.` and repeated `//` normalize before lookup and key construction.
    let dot_args = ["-c", "-Wall", "-o", "/target/debug/build/zstd-sys-1/out/./flag_check", "/target/debug/build/zstd-sys-1/out/.//flag_check.c"];
    assert!(is_probe("cc", &dot_args));
    let sp_dot = Spawn { program: "cc", args: &dot_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let canon_dot = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_dot, files, alloc).expect("normalized OUT_DIR probe");
    assert_eq!(canon_dot.row.files.len(), 1, "normalized ./flag_check.c must resolve the OUT_DIR input digest");
    assert_eq!(canon_dot.input_rels[0].1.as_str(), "flag_check.c");

    // Root substitution must only occur at genuine path boundaries (start `None`/`=`/`:` and end
    // `/` or EOF, plus `=` only in `-f*-prefix-map=`) and NEVER inside `-D`/`-U` macro defines;
    // embedded roots like `prefix/x/out/file` and punctuation/non-ASCII suffixes stay literal.
    for literal_def in [
        "-DROOT=\"prefix/target/debug/build/zstd-sys-1/out/file\"",
        "-DROOT=/target/debug/build/zstd-sys-1/out",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out/file\"",
        "-fdebug-compilation-dir=prefix/target/debug/build/zstd-sys-1/out/file",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out+suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out:suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out;suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out,suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out=suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out)suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out]suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out}suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out>suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/out suffix\"",
        "-DROOT=\"/target/debug/build/zstd-sys-1/outé\"",
    ] {
        let def_args = flag_args(literal_def);
        let sp_def = Spawn { program: "cc", args: &def_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
        let canon = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_def, files, alloc)
            .expect("literal arg with embedded root or punctuation suffix");
        assert!(
            canon.row.args.iter().any(|a| a.as_str() == literal_def),
            "expected `{literal_def}` to stay literal without $OUT_DIR substitution, got {:?}",
            canon.row.args
        );
        assert!(
            canon.exec_args.iter().flatten().all(|s| !matches!(s, crate::c_tasks_probe::Segment::OutDir)),
            "expected `{literal_def}` exec_args to contain no Segment::OutDir"
        );
    }

    // Ambient host `TMPDIR` values must never pollute content-addressed `ProbeKey`s or `exec_env`.
    let env_tmp_a = [("CARGO_PKG_NAME", "zstd-sys"), ("MACOSX_DEPLOYMENT_TARGET", "11.0"), ("TMPDIR", "/private/var/folders/aa/bb/T")];
    let env_tmp_b = [("CARGO_PKG_NAME", "zstd-sys"), ("MACOSX_DEPLOYMENT_TARGET", "11.0"), ("TMPDIR", "/tmp/other-machine-tmp")];
    let sp_tmp_a = Spawn { program: "cc", args: &ok_args, env: &env_tmp_a, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let sp_tmp_b = Spawn { program: "cc", args: &ok_args, env: &env_tmp_b, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let sp_no_tmp = Spawn { program: "cc", args: &ok_args, env: ENV, out_dir: OUT, sdk: SDK, mount: "/package", sdk_id: "macos-sdk 15.0", stdin: &[] };
    let can_tmp_a = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_tmp_a, files, alloc).expect("tmp a");
    let can_tmp_b = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_tmp_b, files, alloc).expect("tmp b");
    let can_no_tmp = canonicalize(blaze_ids::ContentId([1; 32]), &CLANG_PIN, &sp_no_tmp, files, alloc).expect("no tmp");
    assert_eq!(can_tmp_a.key, can_tmp_b.key, "ambient TMPDIR must not split probe keys");
    assert_eq!(can_tmp_a.key, can_no_tmp.key, "ambient TMPDIR must match absent TMPDIR key");
    assert!(can_tmp_a.exec_env.iter().all(|(k, _)| k.as_str() != "TMPDIR"), "host TMPDIR must not persist into exec_env");

    // Revalidating answerer.id() after a cache hit must reject a mid-lookup runtime-closure switch.
    struct SwitchingAnswerer {
        calls: core::cell::Cell<u32>,
    }
    impl ProbeAnswerer for SwitchingAnswerer {
        fn id<'a>(&self, _alloc: blaze_own::Alloc<'a>) -> Result<blaze_ids::ContentId, blaze_own::AString<'a>> {
            let n = self.calls.get();
            self.calls.set(n + 1);
            let tag = if n == 0 { b"v1" } else { b"v2" };
            Ok(blaze_ids::ContentId::of("switching-answerer-test", tag))
        }
        fn answer<'a, 'b>(
            &self,
            _probe: &CanonicalProbe<'_, '_>,
            _input_bytes: &mut dyn FnMut(&str) -> Option<&'b [u8]>,
            alloc: blaze_own::Alloc<'a>,
        ) -> Result<ProbeAnswer<'a>, blaze_own::AString<'a>> {
            Ok(ProbeAnswer { code: 0, stdout: blaze_own::String::new_in(alloc), stderr: blaze_own::String::new_in(alloc) })
        }
    }
    struct HitCache(std::vec::Vec<u8>);
    impl ProbeCache for HitCache {
        fn get<'a>(&self, _key: &ProbeKey, alloc: blaze_own::Alloc<'a>) -> Result<Option<blaze_own::AVec<'a, u8>>, blaze_own::AString<'a>> {
            let mut out = blaze_own::AVec::with_capacity_in(self.0.len(), alloc);
            out.extend_from_slice(&self.0);
            Ok(Some(out))
        }
        fn put<'a>(&self, _key: &ProbeKey, _bytes: &[u8], _alloc: blaze_own::Alloc<'a>) -> Result<(), blaze_own::AString<'a>> {
            Ok(())
        }
    }
    let mut encoded_hit = blaze_own::AVec::new_in(alloc);
    crate::c_tasks_probe::encode_answer(
        &ProbeAnswer { code: 0, stdout: blaze_own::String::new_in(alloc), stderr: blaze_own::String::new_in(alloc) },
        &mut encoded_hit,
    );
    let switching = SwitchingAnswerer { calls: core::cell::Cell::new(0) };
    let hit_cache = HitCache(encoded_hit.as_slice().to_vec());
    let mut no_bytes = |_: &str| None;
    let err = crate::c_tasks_probe::resolve(&switching, &hit_cache, &CLANG_PIN, &sp_no_tmp, files, &mut no_bytes, alloc)
        .expect_err("mid-lookup runtime-closure switch must reject cache hit");
    assert!(
        err.as_str().contains("runtime closure changed during cache lookup"),
        "expected closure-change error, got: {err}"
    );
}
