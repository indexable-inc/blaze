//! The reader, the set and the sandbox's text, against the checked-in file and fake sets.

use std::{
    borrow::ToOwned,
    string::{String as StdString, ToString},
    vec::Vec as StdVec,
};

use blaze_own::Alloc;

use crate::{
    BaseSet, BaseSetError, FromMachine, HostRoot, HostRootError, MachineOs, NotInSet, OsBuild, OsToolGrant, PinsFile, PinsWhy,
    Pinned, Platform, ProfileAllows, SdkId, SourceDateEpoch, StepAt, ToolsPart, WrongMachine, answers_file, os_tool_in_profile,
    profile_lines, step_env,
};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The spec's table (base-tools.md, "The set"), group by group, as written there; nasm and
/// yasm are x86-64's alone, and each platform adds its linker's name (`ld64.lld` on Apple,
/// `ld.lld` on Linux: clang starts the linker by that name).
const C_TOOLCHAIN: [&str; 8] = ["clang", "clang++", "lld", "llvm-ar", "llvm-nm", "llvm-ranlib", "llvm-strip", "llvm-objcopy"];
const SHELL_AND_CORE: [&str; 36] = [
    "sh", "cat", "cp", "mv", "rm", "rmdir", "mkdir", "ln", "ls", "chmod", "touch", "expr", "test", "true", "false", "echo", "printf",
    "basename", "dirname", "sort", "uniq", "tr", "head", "tail", "wc", "cut", "tee", "env", "uname", "hostname", "date", "sleep",
    "find", "xargs", "install", "mktemp",
];
const TEXT: [&str; 5] = ["sed", "grep", "awk", "diff", "cmp"];
const BUILD_DRIVERS: [&str; 4] = ["make", "cmake", "ninja", "pkg-config"];
const SCRIPT_LANGUAGES: [&str; 2] = ["perl", "python3"];
const ARCHIVES: [&str; 5] = ["tar", "gzip", "xz", "zstd", "unzip"];

fn spec_table(platform: Platform) -> StdVec<&'static str> {
    let mut out: StdVec<&'static str> = StdVec::new();
    for group in [&C_TOOLCHAIN[..], &SHELL_AND_CORE[..], &TEXT[..], &BUILD_DRIVERS[..], &SCRIPT_LANGUAGES[..], &ARCHIVES[..]] {
        out.extend_from_slice(group);
    }
    out.push("protoc");
    match platform {
        Platform::Aarch64AppleDarwin => out.push("ld64.lld"),
        Platform::Aarch64UnknownLinuxGnu => out.push("ld.lld"),
        Platform::X86_64UnknownLinuxGnu => out.extend_from_slice(&["ld.lld", "nasm", "yasm"]),
    }
    out.sort_unstable();
    out
}

/// A fake file of one Apple row; each field is spliced in as given.
struct Fake<'x> {
    digest: &'x str,
    /// A JSON value: a number or `"PENDING"`.
    size: &'x str,
    /// The `shell` group's names, as JSON list items.
    shell: &'x str,
    archive: &'x str,
    uname_r: &'x str,
}

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const FAKE_A: Fake<'static> = Fake { digest: DIGEST_A, size: "1000", shell: r#""sh", "cat", "uname""#, archive: "base.tar.gz", uname_r: "20.0.0" };

const FAKE_TEXT: &str = r#"{"comment": "a \"fake\" set", "form": 1, "release": "https://example.invalid/base-tools", "platforms": {"aarch64-apple-darwin": {"archive": "@ARCHIVE@", "digest": "@DIGEST@", "size": @SIZE@, "programs": {"shell": [@SHELL@], "build": ["make", "perl"]}, "answers": {"hostname": "localhost", "uname": {"m": "arm64", "o": "Darwin", "p": "arm", "r": "@R@", "s": "Darwin", "v": "Darwin Kernel Version 20.0.0"}}}}}"#;

fn fake(f: &Fake<'_>) -> StdString {
    FAKE_TEXT
        .replace("@ARCHIVE@", f.archive)
        .replace("@DIGEST@", f.digest)
        .replace("@SIZE@", f.size)
        .replace("@SHELL@", f.shell)
        .replace("@R@", f.uname_r)
}

fn os(build: &'static str) -> MachineOs<'static> {
    MachineOs::Apple(OsBuild::new(build).expect("an OS build"))
}

const AT: StepAt<'static> = StepAt { package: "tikv-jemalloc-sys", step: "sh configure" };

/// The checked-in file reads; every platform is there and still PENDING; each set is the
/// spec's table (with its linker's name); the Apple row's answers file is exact. Prints
/// "pins/base-tools.json: 3 platforms PENDING; aarch64-apple-darwin 62 programs".
#[test]
fn the_checked_in_file_is_the_spec_table_and_pending() {
    let alloc = alloc();
    let file = PinsFile::parse(blaze_pins::BASE_TOOLS_JSON, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.release, Pinned::Pending);
    let platforms: StdVec<Platform> = file.rows.iter().map(|row| row.platform).collect();
    assert_eq!(platforms, Platform::ALL);
    for row in file.rows.iter() {
        assert_eq!((row.digest, row.size), (Pinned::Pending, Pinned::Pending), "{}", row.platform);
        let programs: StdVec<&str> = row.programs(alloc).iter().copied().collect();
        assert_eq!(programs, spec_table(row.platform), "{}", row.platform);
        assert_eq!(row.answers.hostname, "localhost");
    }
    let mac = file.row(Platform::Aarch64AppleDarwin).expect("the Apple row");
    let answers = answers_file(&mac.answers, alloc).expect("answers of their form");
    assert_eq!(
        answers.as_str(),
        "hostname=localhost\nuname.m=arm64\nuname.n=localhost\nuname.o=Darwin\nuname.p=arm\nuname.r=20.0.0\nuname.s=Darwin\nuname.v=Darwin Kernel Version 20.0.0\n"
    );
    std::println!("{}: 3 platforms PENDING; aarch64-apple-darwin {} programs", crate::pins::PATH, mac.programs(alloc).len());
}

/// A PENDING digest is the named refusal, for the checked-in file and a fake one; a digest
/// pinned without its size (or the other way round) is refused by the reader. Prints the
/// refusal.
#[test]
fn a_pending_set_is_the_named_refusal() {
    let alloc = alloc();
    let error = BaseSet::checked_in(Platform::Aarch64AppleDarwin, alloc).expect_err("the set is not built");
    assert_eq!(error, BaseSetError::NotBuilt { platform: Platform::Aarch64AppleDarwin });
    assert_eq!(error.to_string(), "the base tool set for aarch64-apple-darwin is not built yet: pins/base-tools.json");
    let linux = BaseSet::checked_in(Platform::Aarch64UnknownLinuxGnu, alloc).expect_err("not built");
    assert_eq!(linux.to_string(), "the base tool set for aarch64-unknown-linux-gnu is not built yet: pins/base-tools.json");

    let pending = fake(&Fake { digest: "PENDING", size: "\"PENDING\"", ..FAKE_A });
    let file = PinsFile::parse(&pending, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(BaseSet::from_file(&file, Platform::Aarch64AppleDarwin, alloc), Err(BaseSetError::NotBuilt { platform: Platform::Aarch64AppleDarwin }));
    // The control: the same file pinned is a set.
    let pinned = fake(&FAKE_A);
    let file = PinsFile::parse(&pinned, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert!(BaseSet::from_file(&file, Platform::Aarch64AppleDarwin, alloc).is_ok());
    // A platform the file has no row for.
    assert_eq!(BaseSet::from_file(&file, Platform::X86_64UnknownLinuxGnu, alloc), Err(BaseSetError::NoRow { platform: Platform::X86_64UnknownLinuxGnu }));

    for half in [Fake { digest: "PENDING", ..FAKE_A }, Fake { size: "\"PENDING\"", ..FAKE_A }] {
        let text = fake(&half);
        assert_eq!(PinsFile::parse(&text, alloc).map(|_| ()).map_err(|e| e.why), Err(PinsWhy::HalfPinned));
    }
    std::println!("{error}");
}

/// The schema round trip: read, written, read again gives the same file, and writing that gives
/// the same bytes; for the checked-in file and for a pinned fake whose comment holds escapes.
#[test]
fn the_schema_round_trips() {
    let alloc = alloc();
    let pinned = fake(&FAKE_A);
    for text in [blaze_pins::BASE_TOOLS_JSON, pinned.as_str()] {
        let first = PinsFile::parse(text, alloc).unwrap_or_else(|e| panic!("{e}"));
        let written = first.write(alloc).expect("a whole file");
        let again = PinsFile::parse(written.as_str(), alloc).unwrap_or_else(|e| panic!("{e}: {}", written.as_str()));
        assert_eq!(first, again);
        let rewritten = again.write(alloc).expect("a whole file");
        assert_eq!(written.as_str(), rewritten.as_str());
    }
    let file = PinsFile::parse(&pinned, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert!(file.comment.has_escapes());
    let row = file.row(Platform::Aarch64AppleDarwin).expect("the row");
    assert_eq!(row.size, Pinned::Is(1000));
    assert_eq!(row.digest, Pinned::Is([0xaa; 32]));
}

/// A program the set does not hold is NotInSet in a machine's words, with the package and the
/// step; one it holds has its guest and host paths; a path is not a name.
#[test]
fn a_program_not_in_the_set_says_command_not_found() {
    let alloc = alloc();
    let text = fake(&FAKE_A);
    let file = PinsFile::parse(&text, alloc).unwrap_or_else(|e| panic!("{e}"));
    let set = BaseSet::from_file(&file, Platform::Aarch64AppleDarwin, alloc).expect("a set");
    let mut tools = set.step(os("26A434")).expect("an Apple machine");
    let missing = tools.program("cmake", AT).expect_err("no cmake in the fake set");
    assert_eq!(missing, NotInSet { name: "cmake", at: AT, platform: Platform::Aarch64AppleDarwin });
    assert_eq!(
        missing.to_string(),
        "sh: cmake: command not found (package `tikv-jemalloc-sys`, step `sh configure`: the base tool set for aarch64-apple-darwin has no `cmake`, pins/base-tools.json; D192)"
    );
    assert!(missing.to_string().starts_with("sh: cmake: command not found"));
    assert!(tools.program("/bin/sh", AT).is_err(), "a path is not a program of the set");
    let sh = tools.program("sh", AT).expect("sh is in the set");
    assert_eq!(sh.name(), "sh");
    assert_eq!(sh.guest_path(alloc).as_str(), "/tools/bin/sh");
    let root = HostRoot::new("/w/shelf/base-tools-aa").expect("a host folder");
    assert_eq!(sh.host_path(&root, alloc).as_str(), "/w/shelf/base-tools-aa/bin/sh");
    assert!(tools.finish().id().is_some());
}

/// The profile lines grant the set's folder and nothing under /bin, /usr/bin or a
/// developer-tools folder; the guard passes them and catches a planted OS path (the control);
/// an SDK under Xcode passes only when it is the allowed SDK; a host folder in the OS's is
/// refused.
#[test]
fn the_profile_grants_the_set_and_no_os_tool() {
    let alloc = alloc();
    let root = HostRoot::new("/w/shelf/base-tools-aa").expect("a host folder");
    let lines = profile_lines(&root, alloc);
    assert_eq!(
        lines.as_str(),
        "(allow process-exec (subpath \"/w/shelf/base-tools-aa\"))\n(allow file-read* (subpath \"/w/shelf/base-tools-aa\"))\n(allow file-read-metadata (literal \"/w\"))\n(allow file-read-metadata (literal \"/w/shelf\"))\n"
    );
    let outside = lines.as_str().replace(root.path(), "");
    for word in ["/bin/", "/usr/bin", "Xcode"] {
        assert!(!outside.contains(word), "{word} in {}", lines.as_str());
    }
    assert_eq!(os_tool_in_profile(lines.as_str(), ProfileAllows { set_root: &root, sdk_root: None }), Ok(()));

    // The control: an OS program planted in the profile is caught, with its line.
    let planted = [lines.as_str(), "(allow process-exec (literal \"/bin/sh\"))\n"].concat();
    assert_eq!(os_tool_in_profile(&planted, ProfileAllows { set_root: &root, sdk_root: None }), Err(OsToolGrant { line: 5, path: "/bin/sh" }));
    let make = "/Applications/Xcode.app/Contents/Developer/usr/bin/make";
    let planted = [lines.as_str(), "(allow process-exec (literal \"", make, "\"))\n"].concat();
    assert_eq!(os_tool_in_profile(&planted, ProfileAllows { set_root: &root, sdk_root: None }), Err(OsToolGrant { line: 5, path: make }));
    // The older one-section-a-line spelling (blaze-build-script native.rs) is caught too.
    let older = "(allow process-exec (literal \"/w/script\") (literal \"/usr/bin/perl\"))\n";
    assert_eq!(os_tool_in_profile(older, ProfileAllows { set_root: &root, sdk_root: None }), Err(OsToolGrant { line: 1, path: "/usr/bin/perl" }));

    // The SDK, which comes from the machine (FromMachine::AppleSdk): its folder and the
    // metadata of the folders above it pass when it is the allowed SDK, and only then.
    let sdk = "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX27.2.sdk";
    let with_sdk = [
        lines.as_str(),
        "(allow file-read* (subpath \"",
        sdk,
        "\"))\n(allow file-read-metadata (literal \"/Applications\"))\n(allow file-read-metadata (literal \"/Applications/Xcode.app\"))\n",
    ]
    .concat();
    assert_eq!(os_tool_in_profile(&with_sdk, ProfileAllows { set_root: &root, sdk_root: Some(sdk) }), Ok(()));
    assert_eq!(os_tool_in_profile(&with_sdk, ProfileAllows { set_root: &root, sdk_root: None }), Err(OsToolGrant { line: 5, path: sdk }));
    // A folder above the SDK granted whole (not its metadata) is a grant of everything in it.
    let wide = [lines.as_str(), "(allow process-exec (subpath \"/Applications\"))\n"].concat();
    assert_eq!(
        os_tool_in_profile(&wide, ProfileAllows { set_root: &root, sdk_root: Some(sdk) }),
        Err(OsToolGrant { line: 5, path: "/Applications" })
    );

    for os_folder in ["/usr/bin", "/bin", "/usr/local/blaze", "/opt/homebrew/base", "/Applications/Xcode.app/Contents/Developer", "/System/x", "/Users/a/CommandLineTools/x"] {
        assert_eq!(HostRoot::new(os_folder), Err(HostRootError::OsFolder { path: os_folder }), "{os_folder}");
    }
    for bad in ["relative", "/", "/a/../b", "/a/./b", "/a//b", "/a/"] {
        assert_eq!(HostRoot::new(bad), Err(HostRootError::NotCanonical { path: bad }), "{bad}");
    }
    assert_eq!(HostRoot::new("/a/\"b"), Err(HostRootError::Unsayable { path: "/a/\"b" }));
    assert_eq!(root.guest_pair(), ("/tools", "/w/shelf/base-tools-aa"));
}

/// `key_part` is the digest: it moves with the digest and with nothing else of the file (size,
/// archive name, program order, answers). A step's part holds it exactly when a program ran,
/// one part for any program of one set; the OS build and the SDK move the part, and a step that
/// ran nothing holds nothing (removing the cause, rule 23).
#[test]
fn the_key_part_moves_with_the_digest_and_nothing_else() {
    let alloc = alloc();
    let text_a = fake(&FAKE_A);
    let text_b = fake(&Fake { digest: DIGEST_B, ..FAKE_A });
    let text_c = fake(&Fake { size: "2000", shell: r#""uname", "sh", "cat", "tr""#, archive: "other.tar.gz", uname_r: "21.0.0", ..FAKE_A });
    let (file_a, file_b, file_c) = (
        PinsFile::parse(&text_a, alloc).unwrap_or_else(|e| panic!("{e}")),
        PinsFile::parse(&text_b, alloc).unwrap_or_else(|e| panic!("{e}")),
        PinsFile::parse(&text_c, alloc).unwrap_or_else(|e| panic!("{e}")),
    );
    let (a, b, c) = (apple_set(&file_a), apple_set(&file_b), apple_set(&file_c));
    assert_eq!(a.key_part(), [0xaa; 32]);
    assert_eq!(b.key_part(), [0xbb; 32]);
    assert_ne!(a.key_part(), b.key_part());
    assert_eq!(a.key_part(), c.key_part());
    assert_ne!(a, c, "the sets differ in all but the digest");

    let none = a.step(os("26A434")).expect("an Apple machine").finish();
    assert_eq!(none, ToolsPart::NOT_GIVEN);
    assert_eq!(none.id(), None);
    let sh = ran(&a, "26A434", "sh");
    assert!(sh.id().is_some());
    assert_eq!(ran(&a, "26A434", "make"), sh, "one part per set, not per program");
    assert_eq!(ran(&c, "26A434", "sh"), sh, "nothing but the digest of the set moves it");
    assert_ne!(ran(&b, "26A434", "sh"), sh, "a new digest moves it");
    assert_ne!(ran(&a, "26B118", "sh"), sh, "the system libraries' OS build moves it");

    // The SDK: held by a step that read it, with or without a program; one SDK a step.
    let sdk = SdkId::new("27.2", "26B118").expect("an SDK id");
    let other = SdkId::new("26.5", "25F70").expect("an SDK id");
    let mut only_sdk = a.step(os("26A434")).expect("an Apple machine");
    only_sdk.read_sdk(sdk).expect("one SDK");
    only_sdk.read_sdk(sdk).expect("the same SDK again");
    assert_eq!(only_sdk.read_sdk(other), Err(WrongMachine::TwoSdks { first: sdk, second: other }));
    let only_sdk = only_sdk.finish();
    assert!(only_sdk.id().is_some());
    assert_ne!(only_sdk, sh);
    let mut both = a.step(os("26A434")).expect("an Apple machine");
    let _sh = both.program("sh", AT).expect("in the set");
    both.read_sdk(sdk).expect("one SDK");
    let both = both.finish();
    assert_ne!(both, sh);
    assert_ne!(both, only_sdk);

    // An Apple set on a Linux machine is refused, never run.
    match a.step(MachineOs::Linux) {
        Ok(tools) => {
            tools.failed();
            panic!("an Apple set was given a Linux machine");
        }
        Err(error) => assert_eq!(error, WrongMachine::Kind { platform: Platform::Aarch64AppleDarwin }),
    }
}

/// The part of a step on OS build `build` that ran `program` of `set`.
fn ran(set: &BaseSet<'_, '_>, build: &'static str, program: &str) -> ToolsPart {
    let mut tools = set.step(os(build)).expect("an Apple machine");
    let _program = tools.program(program, AT).expect("in the set");
    tools.finish()
}

/// The Apple row's set of `file`.
fn apple_set<'t>(file: &PinsFile<'t, '_>) -> BaseSet<'t, 'static> {
    BaseSet::from_file(file, Platform::Aarch64AppleDarwin, alloc()).unwrap_or_else(|e| panic!("{e}"))
}

/// Two files listing one set's programs in different orders give equal sets, sorted.
#[test]
fn two_orders_of_one_set_are_equal() {
    let alloc = alloc();
    let one = fake(&Fake { shell: r#""sh", "cat", "uname""#, ..FAKE_A });
    let two = fake(&Fake { shell: r#""uname", "sh", "cat""#, ..FAKE_A });
    let (one, two) = (PinsFile::parse(&one, alloc).unwrap_or_else(|e| panic!("{e}")), PinsFile::parse(&two, alloc).unwrap_or_else(|e| panic!("{e}")));
    let one = BaseSet::from_file(&one, Platform::Aarch64AppleDarwin, alloc).expect("a set");
    let two = BaseSet::from_file(&two, Platform::Aarch64AppleDarwin, alloc).expect("a set");
    assert_eq!(one, two);
    assert_eq!(one.programs(), ["cat", "make", "perl", "sh", "uname"]);
    // The control: another set is not equal.
    let three = fake(&Fake { shell: r#""sh", "cat""#, ..FAKE_A });
    let three = PinsFile::parse(&three, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(one, BaseSet::from_file(&three, Platform::Aarch64AppleDarwin, alloc).expect("a set"));
}

/// The fixed environment, sorted by name, with `SOURCE_DATE_EPOCH` as passed in.
#[test]
fn the_step_environment_is_fixed() {
    let alloc = alloc();
    let env = step_env(SourceDateEpoch(1_700_000_000), alloc);
    let pairs: StdVec<(&str, &str)> = env.iter().map(|(name, value)| (*name, value.as_str())).collect();
    assert_eq!(pairs, [("LC_ALL", "C"), ("PATH", "/tools/bin"), ("SOURCE_DATE_EPOCH", "1700000000"), ("TZ", "UTC")]);
}

/// What still comes from the machine: each entry's identity bytes are its tag and its fields,
/// length-prefixed, so two entries never give one stream.
#[test]
fn machine_identities_are_framed() {
    let collect = |entry: FromMachine<'_>| {
        let mut out: StdVec<u8> = StdVec::new();
        entry.identity_into(&mut |bytes: &[u8]| out.extend_from_slice(bytes));
        out
    };
    let os = collect(FromMachine::SystemLibraries(OsBuild::new("26A434").expect("a build")));
    let mut want: StdVec<u8> = StdVec::from([2u8]);
    want.extend_from_slice(&6u64.to_le_bytes());
    want.extend_from_slice(b"26A434");
    assert_eq!(os, want);
    let ab = collect(FromMachine::AppleSdk(SdkId::new("27.2", "26B118").expect("an id")));
    let a_b = collect(FromMachine::AppleSdk(SdkId::new("27", "226B118").expect("an id")));
    assert_ne!(ab, a_b);
    assert_eq!(ab[0], 1);
    assert!(OsBuild::new("26 A").is_err());
    assert!(SdkId::new("27.x", "26B118").is_err());
}

/// The reader refuses what is not the schema, naming the line and the thing.
#[test]
fn the_reader_refuses_what_is_not_the_schema() {
    let alloc = alloc();
    let base = fake(&FAKE_A);
    let refused = |text: &str| PinsFile::parse(text, alloc).map(|_| ()).map_err(|e| (e.why, e.found.to_owned()));
    // A program in two groups.
    let twice = fake(&Fake { shell: r#""sh", "make""#, ..FAKE_A });
    assert_eq!(refused(&twice), Err((PinsWhy::TwoPrograms, "make".to_owned())));
    // A platform nobody builds for.
    let riscv = base.replace("aarch64-apple-darwin", "riscv64gc-unknown-linux-gnu");
    assert_eq!(refused(&riscv), Err((PinsWhy::UnknownPlatform, "riscv64gc-unknown-linux-gnu".to_owned())));
    // Another form.
    assert_eq!(refused(&base.replace("\"form\": 1", "\"form\": 2")), Err((PinsWhy::Form, StdString::new())));
    // A key of no schema, and a missing one.
    assert_eq!(refused(&base.replace("\"archive\"", "\"archives\"")), Err((PinsWhy::UnknownKey, "archives".to_owned())));
    let no_answers = base.replace(", \"answers\": {\"hostname\": \"localhost\", \"uname\": {\"m\": \"arm64\", \"o\": \"Darwin\", \"p\": \"arm\", \"r\": \"20.0.0\", \"s\": \"Darwin\", \"v\": \"Darwin Kernel Version 20.0.0\"}}", "");
    assert_eq!(refused(&no_answers), Err((PinsWhy::Missing("answers"), StdString::new())));
    // A digest of the wrong form.
    let short = fake(&Fake { digest: "abc", ..FAKE_A });
    assert_eq!(refused(&short), Err((PinsWhy::Digest, "abc".to_owned())));
    // A program name with a folder in it.
    let slash = fake(&Fake { shell: r#""sh", "bin/cat""#, ..FAKE_A });
    assert_eq!(refused(&slash), Err((PinsWhy::BadName, "bin/cat".to_owned())));
    // The error names the file and the line.
    let error = PinsFile::parse(&twice, alloc).expect_err("refused");
    assert!(error.to_string().starts_with("pins/base-tools.json:1:"), "{error}");
    // The control: the base text reads.
    assert_eq!(refused(&base), Ok(()));
}
